//! Nikon ND2 format reader.
//!
//! ND2 is a chunk-based binary format. Each chunk has a 16-byte header:
//!   - 4 bytes magic: 0xDA 0xCE 0xBE 0x0A
//!   - 4 bytes name length
//!   - 8 bytes data length
//! Followed by the name string and then the data payload.
//!
//! Key chunk names: "ImageAttributesLV!", "ImageMetadataLV!",
//!                  "ImageDataSeq|0!", "ImageDataSeq|1!", ...
//!
//! Compression: uncompressed, zlib, or JPEG2000.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

/// ND2 file magic bytes.
pub const ND2_MAGIC: [u8; 4] = [0xDA, 0xCE, 0xBE, 0x0A];

#[derive(Debug, Clone)]
struct Nd2Chunk {
    name: String,
    data_offset: u64,
    data_length: u64,
}

#[derive(Debug, Clone)]
struct OldJp2Plane {
    data_offset: u64,
    data_length: u64,
}

fn scan_chunks(f: &mut BufReader<File>) -> std::io::Result<Vec<Nd2Chunk>> {
    let mut chunks = Vec::new();
    let file_len = f.get_ref().metadata()?.len();
    f.seek(SeekFrom::Start(0))?;

    loop {
        let chunk_start = f.stream_position()?;
        if chunk_start + 16 > file_len {
            break;
        }

        let mut magic = [0u8; 4];
        if f.read_exact(&mut magic).is_err() {
            break;
        }
        if magic != ND2_MAGIC {
            f.seek(SeekFrom::Start(chunk_start + 1))?;
            continue;
        }

        let mut name_len_bytes = [0u8; 4];
        f.read_exact(&mut name_len_bytes)?;
        let name_len = u32::from_le_bytes(name_len_bytes) as usize;
        if name_len == 0 || name_len > 4096 {
            f.seek(SeekFrom::Start(chunk_start + 1))?;
            continue;
        }

        let mut data_len_bytes = [0u8; 8];
        f.read_exact(&mut data_len_bytes)?;
        let data_len = u64::from_le_bytes(data_len_bytes);
        let data_offset = chunk_start + 16 + name_len as u64;
        let Some(data_end) = data_offset.checked_add(data_len) else {
            f.seek(SeekFrom::Start(chunk_start + 1))?;
            continue;
        };
        if data_end > file_len {
            f.seek(SeekFrom::Start(chunk_start + 1))?;
            continue;
        }

        let mut name_bytes = vec![0u8; name_len];
        f.read_exact(&mut name_bytes)?;
        let name = String::from_utf8_lossy(&name_bytes)
            .trim_end_matches('\0')
            .to_string();
        if !name.ends_with('!') {
            f.seek(SeekFrom::Start(chunk_start + 1))?;
            continue;
        }

        chunks.push(Nd2Chunk {
            name,
            data_offset,
            data_length: data_len,
        });

        // Advance past data
        f.seek(SeekFrom::Start(data_end))?;
    }
    Ok(chunks)
}

fn image_data_index(name: &str) -> Option<usize> {
    let suffix = name.strip_prefix("ImageDataSeq|")?.trim_end_matches('!');
    suffix.parse().ok()
}

fn read_chunk_map(f: &mut BufReader<File>) -> std::io::Result<Option<Vec<Nd2Chunk>>> {
    const CHUNK_MAP_SIGNATURE: &[u8] = b"ND2 CHUNK MAP SIGNATURE 0000001";

    let file_len = f.get_ref().metadata()?.len();
    if file_len < 40 {
        return Ok(None);
    }

    f.seek(SeekFrom::Start(file_len - 40))?;
    let mut sig = vec![0u8; CHUNK_MAP_SIGNATURE.len()];
    f.read_exact(&mut sig)?;
    if sig != CHUNK_MAP_SIGNATURE {
        return Ok(None);
    }

    let mut skip = [0u8; 1];
    f.read_exact(&mut skip)?;
    let mut off = [0u8; 8];
    f.read_exact(&mut off)?;
    let map_offset = u64::from_le_bytes(off);
    if map_offset + 16 > file_len {
        return Ok(None);
    }

    f.seek(SeekFrom::Start(map_offset))?;
    let mut magic = [0u8; 4];
    f.read_exact(&mut magic)?;
    if magic != ND2_MAGIC {
        return Ok(None);
    }

    let mut name_len_bytes = [0u8; 4];
    f.read_exact(&mut name_len_bytes)?;
    let name_len = u32::from_le_bytes(name_len_bytes) as u64;
    let mut data_len_bytes = [0u8; 8];
    f.read_exact(&mut data_len_bytes)?;
    let map_len = u64::from_le_bytes(data_len_bytes);
    let entries_offset = map_offset + 16 + name_len;
    let entries_end = entries_offset.checked_add(map_len).unwrap_or(u64::MAX);
    if entries_offset > file_len || entries_end > file_len {
        return Ok(None);
    }

    f.seek(SeekFrom::Start(entries_offset))?;
    let mut chunks = Vec::new();
    let mut image_count = 0usize;
    let mut max_image_index: Option<usize> = None;

    while f.stream_position()? + 1 + 16 <= entries_end {
        let mut name_bytes = Vec::new();
        loop {
            if f.stream_position()? >= entries_end {
                return Ok(None);
            }
            let mut b = [0u8; 1];
            f.read_exact(&mut b)?;
            if b[0] == b'!' {
                break;
            }
            name_bytes.push(b[0]);
        }

        let name = String::from_utf8_lossy(&name_bytes).to_string();
        if name.as_bytes() == CHUNK_MAP_SIGNATURE {
            break;
        }
        let mut position_bytes = [0u8; 8];
        let mut length_bytes = [0u8; 8];
        f.read_exact(&mut position_bytes)?;
        f.read_exact(&mut length_bytes)?;
        let position = u64::from_le_bytes(position_bytes);
        let _length = u64::from_le_bytes(length_bytes);
        let map_entry_offset = f.stream_position()?;

        if position + 16 > file_len {
            return Ok(None);
        }

        f.seek(SeekFrom::Start(position))?;
        let mut chunk_magic = [0u8; 4];
        f.read_exact(&mut chunk_magic)?;
        if chunk_magic != ND2_MAGIC {
            return Ok(None);
        }
        let mut actual_name_len_bytes = [0u8; 4];
        let mut actual_data_len_bytes = [0u8; 8];
        f.read_exact(&mut actual_name_len_bytes)?;
        f.read_exact(&mut actual_data_len_bytes)?;
        let actual_name_len = u32::from_le_bytes(actual_name_len_bytes) as u64;
        let actual_data_len = u64::from_le_bytes(actual_data_len_bytes);
        let data_offset = position + 16 + actual_name_len;
        if data_offset > file_len || data_offset + actual_data_len > file_len {
            return Ok(None);
        }
        f.seek(SeekFrom::Start(map_entry_offset))?;

        if let Some(index) = image_data_index(&name) {
            image_count += 1;
            max_image_index = Some(max_image_index.map_or(index, |m| m.max(index)));
        }
        chunks.push(Nd2Chunk {
            name: format!("{name}!"),
            data_offset,
            data_length: actual_data_len,
        });
    }

    if let Some(max_index) = max_image_index {
        if image_count != max_index + 1 {
            return Ok(None);
        }
    }

    for chunk in chunks.iter().filter(|c| c.name.starts_with("ImageDataSeq")) {
        let block_offset = chunk
            .data_offset
            .saturating_sub(16 + chunk.name.len() as u64);
        if block_offset + 4 > file_len {
            return Ok(None);
        }
        f.seek(SeekFrom::Start(block_offset))?;
        let mut magic = [0u8; 4];
        f.read_exact(&mut magic)?;
        if magic != ND2_MAGIC {
            return Ok(None);
        }
    }

    chunks.sort_by_key(|c| c.data_offset);
    Ok(Some(chunks))
}

fn read_chunk_data(f: &mut BufReader<File>, chunk: &Nd2Chunk) -> std::io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(chunk.data_offset))?;
    let mut buf = vec![0u8; chunk.data_length as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Values harvested from the Nikon LV (LIM) binary metadata tree.
///
/// Mirrors `ND2Reader.iterateIn` in Java Bio-Formats: a recursive, length-typed
/// key/value structure. We only collect the handful of attributes needed for
/// OME parity (physical pixel size, channel names, emission wavelengths).
#[derive(Default)]
struct Nd2LvValues {
    calibration: Option<f64>,
    z_step: Option<f64>,
    channel_names: Vec<String>,
    emission_wavelengths: Vec<f64>,
}

/// Parse the Nikon LV binary metadata tree starting at the root of a chunk.
///
/// Entry layout: `[type:u8][nameLen:u8][name: nameLen × UTF-16LE]` followed by a
/// type-specific value. Type 11 is a nested level: `[count:i32][absOffset:i64]`,
/// where children live until `absOffset` (relative to the chunk start) and a
/// trailing `count × 8` byte index table is skipped afterwards.
fn parse_nd2_lv(data: &[u8], out: &mut Nd2LvValues) {
    fn read_u16(d: &[u8], p: usize) -> Option<u16> {
        d.get(p..p + 2).map(|b| u16::from_le_bytes([b[0], b[1]]))
    }
    fn read_i32(d: &[u8], p: usize) -> Option<i32> {
        d.get(p..p + 4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn read_i64(d: &[u8], p: usize) -> Option<i64> {
        d.get(p..p + 8)
            .map(|b| i64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]]))
    }
    fn read_f64(d: &[u8], p: usize) -> Option<f64> {
        read_i64(d, p).map(|v| f64::from_bits(v as u64))
    }

    // Recursive walk. `end` is an exclusive byte bound for the current level.
    fn walk(data: &[u8], mut p: usize, end: usize, depth: u32, out: &mut Nd2LvValues) -> usize {
        if depth > 64 {
            return end;
        }
        while p + 2 <= end {
            let entry_start = p;
            let ty = data[p];
            let name_len = data[p + 1] as usize;
            let name_start = p + 2;
            let name_end = name_start + name_len * 2;
            if name_end > end {
                break;
            }
            let name_units: Vec<u16> = (0..name_len)
                .filter_map(|i| read_u16(data, name_start + i * 2))
                .collect();
            let name = String::from_utf16_lossy(&name_units)
                .trim_end_matches('\0')
                .to_string();
            p = name_end;

            match ty {
                1 => p += 1,         // bool
                2 | 3 => p += 4,     // int32 / uint32
                4 | 5 | 7 => p += 8, // int64 / uint64 / void*
                6 => {
                    // double
                    if let Some(v) = read_f64(data, p) {
                        match name.as_str() {
                            "dCalibration" => {
                                if v > 0.0 && out.calibration.is_none() {
                                    out.calibration = Some(v);
                                }
                            }
                            "dZStep" => {
                                if v > 0.0 && out.z_step.is_none() {
                                    out.z_step = Some(v);
                                }
                            }
                            "EmWavelength" => out.emission_wavelengths.push(v),
                            _ => {}
                        }
                    }
                    p += 8;
                }
                8 => {
                    // Null-terminated UTF-16LE string.
                    let mut units = Vec::new();
                    let mut q = p;
                    while q + 2 <= end {
                        let u = read_u16(data, q).unwrap_or(0);
                        q += 2;
                        if u == 0 {
                            break;
                        }
                        units.push(u);
                    }
                    let s = String::from_utf16_lossy(&units);
                    if name == "sDescription" && !s.is_empty() {
                        out.channel_names.push(s);
                    }
                    p = q;
                }
                9 => {
                    // ByteArray: i64 length then nested LV when length > 2.
                    let Some(len) = read_i64(data, p) else { break };
                    p += 8;
                    let len = len.max(0) as usize;
                    if len > 2 {
                        let child_end = (p + len).min(end);
                        walk(data, p, child_end, depth + 1, out);
                    }
                    p = (p + len).min(end);
                }
                11 => {
                    // Level: count (i32), then an end offset (i64) measured from
                    // this entry's own start (Java: endOffset = off + startOffset).
                    // Children occupy [p, child_end); a count*8 index table follows.
                    let Some(count) = read_i32(data, p) else { break };
                    let Some(off) = read_i64(data, p + 4) else {
                        break;
                    };
                    p += 12;
                    let child_end = entry_start
                        .saturating_add(off.max(0) as usize)
                        .clamp(p, data.len());
                    if child_end > p {
                        walk(data, p, child_end.min(end), depth + 1, out);
                    }
                    // Skip children plus the trailing count*8 index table.
                    let after = child_end.saturating_add((count.max(0) as usize) * 8);
                    p = after.min(end);
                }
                _ => break, // Unknown type: bail out of this level.
            }
        }
        p
    }

    walk(data, 0, data.len(), 0, out);
}

/// Very lightweight XML value extractor — just grab the first occurrence of a tag.
fn xml_value(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let pos = xml.find(&open)?;
    let after_open = &xml[pos..];
    let gt = after_open.find('>')?;
    let attrs = &after_open[..gt];
    if let Some(value) = xml_attr(attrs, "value") {
        return Some(value);
    }

    let content_start = &after_open[gt + 1..];
    let close = format!("</{}>", tag);
    let end = content_start.find(&close)?;
    Some(content_start[..end].trim().to_string())
}

fn xml_attr(tag_text: &str, attr: &str) -> Option<String> {
    let pattern = format!("{attr}=\"");
    let pos = tag_text.find(&pattern)?;
    let value_start = pos + pattern.len();
    let rest = &tag_text[value_start..];
    let value_end = rest.find('"')?;
    Some(rest[..value_end].to_string())
}

fn xml_values(xml: &str, tag: &str) -> Vec<String> {
    let mut values = Vec::new();
    let open = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let mut cursor = 0;

    while let Some(relative_pos) = xml[cursor..].find(&open) {
        let pos = cursor + relative_pos;
        let after_open = &xml[pos..];
        let Some(gt) = after_open.find('>') else {
            break;
        };
        let attrs = &after_open[..gt];
        if let Some(value) = xml_attr(attrs, "value") {
            values.push(value);
        } else if !attrs.trim_end().ends_with('/') {
            let content_start = pos + gt + 1;
            if let Some(end) = xml[content_start..].find(&close) {
                values.push(xml[content_start..content_start + end].trim().to_string());
            }
        }
        cursor = pos + gt + 1;
    }

    values
}

fn nd2_u32_value(xml: &str, tag: &str) -> Option<u32> {
    let value = xml_value(xml, tag)?.parse::<u32>().ok()?;
    (value != u32::MAX).then_some(value)
}

fn nd2_bpp_value(xml: &str) -> Option<u8> {
    xml_value(xml, "uiBpcInMemory")
        .or_else(|| xml_value(xml, "uiBpc"))
        .or_else(|| xml_value(xml, "uiBpcSignificant"))
        .and_then(|s| s.parse::<u8>().ok())
        .filter(|&b| b > 0)
}

fn rect_sensor_extent(xml: &str) -> Option<(u32, u32)> {
    let pos = xml.find("<rectSensorUser")?;
    let after_open = &xml[pos..];
    let gt = after_open.find('>')?;
    let content_start = &after_open[gt + 1..];
    let end = content_start.find("</rectSensorUser>")?;
    let rect = &content_start[..end];

    let left = nd2_u32_value(rect, "left")?;
    let top = nd2_u32_value(rect, "top")?;
    let right = nd2_u32_value(rect, "right")?;
    let bottom = nd2_u32_value(rect, "bottom")?;

    if right > left && bottom > top {
        Some((right - left, bottom - top))
    } else {
        None
    }
}

fn parse_nd2_attributes(xml: &str) -> (u32, u32, u32, u32, u8) {
    let (rect_w, rect_h) = rect_sensor_extent(xml).unwrap_or((0, 0));
    let w = if rect_w > 0 {
        rect_w
    } else {
        nd2_u32_value(xml, "uiWidth")
            .or_else(|| nd2_u32_value(xml, "uiCamPxlCountX"))
            .unwrap_or(0)
    };
    let h = if rect_h > 0 {
        rect_h
    } else {
        nd2_u32_value(xml, "uiHeight")
            .or_else(|| nd2_u32_value(xml, "uiCamPxlCountY"))
            .unwrap_or(0)
    };
    let c = nd2_u32_value(xml, "uiComp").unwrap_or(1u32);
    let bpp = nd2_bpp_value(xml).unwrap_or(8u8);
    let z_count = nd2_u32_value(xml, "uiZStackHome")
        .or_else(|| nd2_u32_value(xml, "uiSequenceCount"))
        .unwrap_or(1u32);
    (w, h, c, z_count.max(1), bpp)
}

fn looks_like_zlib(data: &[u8]) -> bool {
    if data.len() < 2 {
        return false;
    }
    let cmf = data[0];
    let flg = data[1];
    (cmf & 0x0f) == 8 && u16::from_be_bytes([cmf, flg]) % 31 == 0
}

fn looks_like_jpeg2000(data: &[u8]) -> bool {
    data.starts_with(&[0xff, 0x4f, 0xff, 0x51])
        || data.starts_with(&[0x00, 0x00, 0x00, 0x0c, b'j', b'P', b' ', b' '])
}

fn has_old_nd_box_footer(f: &mut BufReader<File>) -> std::io::Result<bool> {
    const OLD_ND_BOX_MARKER: &[u8] = b"LABORATORY IMAGING ND BOX MAP 00";

    let file_len = f.get_ref().metadata()?.len();
    let start = file_len.saturating_sub(4096);
    f.seek(SeekFrom::Start(start))?;
    let mut tail = Vec::with_capacity((file_len - start) as usize);
    f.read_to_end(&mut tail)?;
    Ok(tail
        .windows(OLD_ND_BOX_MARKER.len())
        .any(|window| window == OLD_ND_BOX_MARKER))
}

fn read_be_u16(bytes: &[u8]) -> Option<u16> {
    Some(u16::from_be_bytes(bytes.get(..2)?.try_into().ok()?))
}

fn read_be_u32(bytes: &[u8]) -> Option<u32> {
    Some(u32::from_be_bytes(bytes.get(..4)?.try_into().ok()?))
}

fn scan_old_jp2_boxes(
    f: &mut BufReader<File>,
) -> std::io::Result<(Vec<OldJp2Plane>, u32, u32, u16, u32)> {
    let file_len = f.get_ref().metadata()?.len();
    let mut planes = Vec::new();
    let (mut size_x, mut size_y, mut bands, mut pixel_type_code) = (0u32, 0u32, 1u16, 0u32);
    let mut pos = 0u64;

    while pos + 8 <= file_len {
        f.seek(SeekFrom::Start(pos))?;
        let mut header = [0u8; 8];
        f.read_exact(&mut header)?;
        let length = read_be_u32(&header[..4]).unwrap_or(0) as u64;
        let box_type = &header[4..8];
        let next_pos = pos.saturating_add(length);
        if length < 8 || next_pos > file_len {
            break;
        }

        if box_type == b"jp2c" {
            planes.push(OldJp2Plane {
                data_offset: pos + 8,
                data_length: length - 8,
            });
        } else if box_type == b"jp2h" {
            let mut sub_pos = pos + 8;
            while sub_pos + 8 <= next_pos {
                f.seek(SeekFrom::Start(sub_pos))?;
                let mut sub_header = [0u8; 8];
                f.read_exact(&mut sub_header)?;
                let sub_length = read_be_u32(&sub_header[..4]).unwrap_or(0) as u64;
                let sub_type = &sub_header[4..8];
                let sub_next = sub_pos.saturating_add(sub_length);
                if sub_length < 8 || sub_next > next_pos {
                    break;
                }
                if sub_type == b"ihdr" && sub_length >= 22 {
                    let mut ihdr = [0u8; 14];
                    f.read_exact(&mut ihdr)?;
                    size_y = read_be_u32(&ihdr[0..4]).unwrap_or(0);
                    size_x = read_be_u32(&ihdr[4..8]).unwrap_or(0);
                    bands = read_be_u16(&ihdr[8..10]).unwrap_or(1);
                    pixel_type_code = read_be_u32(&ihdr[10..14]).unwrap_or(0);
                }
                sub_pos = sub_next;
            }
        }

        pos = next_pos;
    }

    Ok((planes, size_x, size_y, bands, pixel_type_code))
}

fn old_nd2_metadata_text(f: &mut BufReader<File>) -> std::io::Result<String> {
    f.seek(SeekFrom::Start(0))?;
    let mut data = Vec::new();
    f.read_to_end(&mut data)?;
    Ok(String::from_utf8_lossy(&data).into_owned())
}

fn old_nd2_metadata_indexes(text: &str) -> Vec<u32> {
    let mut indexes = Vec::new();
    let mut cursor = 0;
    while let Some(relative_pos) = text[cursor..].find("<MetadataSeq") {
        let pos = cursor + relative_pos;
        let after_open = &text[pos..];
        let Some(gt) = after_open.find('>') else {
            break;
        };
        if let Some(value) = xml_attr(&after_open[..gt], "_SEQUENCE_INDEX") {
            if let Ok(index) = value.parse::<u32>() {
                if !indexes.contains(&index) {
                    indexes.push(index);
                }
            }
        }
        cursor = pos + gt + 1;
    }
    indexes.sort_unstable();
    indexes
}

fn old_nd2_component_count(text: &str, jp2_bands: u16) -> u32 {
    xml_values(text, "uiCompCount")
        .into_iter()
        .filter_map(|value| value.parse::<u32>().ok())
        .filter(|&value| value > 0 && value != u32::MAX)
        .max()
        .unwrap_or(jp2_bands as u32)
        .max(1)
}

fn require_exact_frame(data: Vec<u8>, expected: usize, kind: &str) -> Result<Vec<u8>> {
    if data.len() == expected {
        Ok(data)
    } else if data.len() > expected {
        Err(BioFormatsError::Format(format!(
            "{kind} frame has trailing data ({} > {expected})",
            data.len()
        )))
    } else {
        Err(BioFormatsError::Format(format!(
            "{kind} frame too small ({} < {expected})",
            data.len()
        )))
    }
}

fn decompress_nd2_zlib(data: &[u8], expected: usize) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read as _;

    let mut dec = ZlibDecoder::new(data);
    let mut out = Vec::new();
    dec.read_to_end(&mut out).map_err(BioFormatsError::Io)?;
    require_exact_frame(out, expected, "zlib")
}

fn decode_nd2_frame_payload(data: &[u8], expected: usize) -> Result<Vec<u8>> {
    const FRAME_PREFIX_LEN: usize = 8;
    const NIKON_PAYLOAD_OFFSET: usize = 4096;
    const MAX_RAW_TRAILER_LEN: usize = 4096;

    if data.len() == expected {
        return Ok(data.to_vec());
    }

    // Each ImageDataSeq block is [8-byte frame timestamp/double][pixel data].
    // Java always skips the leading 8 bytes before reading the plane
    // (ND2Reader.java:1704 `offsets[...] = offset + p[0] + 8`, then :249 readPlane).
    // Prefer interpreting the leading 8 bytes as the frame-timestamp prefix
    // (yielding exactly `expected` pixel bytes) over truncating a trailer, which
    // would otherwise keep the timestamp bytes as the first pixels and drop the
    // last 8 real bytes. Skip this when the payload looks compressed so the
    // zlib/JPEG2000 paths below remain unaffected.
    if data.len() == expected + FRAME_PREFIX_LEN {
        let payload = &data[FRAME_PREFIX_LEN..];
        if !looks_like_zlib(payload) && !looks_like_jpeg2000(payload) {
            return Ok(payload.to_vec());
        }
    }

    if data.len() > expected
        && expected >= 1024
        && data.len() - expected <= MAX_RAW_TRAILER_LEN
        && !looks_like_zlib(data)
        && !looks_like_jpeg2000(data)
    {
        return Ok(data[..expected].to_vec());
    }

    for prefix_len in [0usize, FRAME_PREFIX_LEN, NIKON_PAYLOAD_OFFSET] {
        let Some(payload) = data.get(prefix_len..) else {
            continue;
        };

        if prefix_len > 0 && payload.len() == expected {
            return Ok(payload.to_vec());
        }

        if looks_like_zlib(payload) {
            return decompress_nd2_zlib(payload, expected);
        }

        if looks_like_jpeg2000(payload) {
            let decoded = crate::common::codec::decompress_jpeg2000(payload)?;
            return require_exact_frame(decoded, expected, "JPEG2000");
        }
    }

    if data.len() > expected {
        Err(BioFormatsError::UnsupportedFormat(format!(
            "unsupported structured frame encoding ({} bytes for {expected}-byte plane)",
            data.len()
        )))
    } else {
        Err(BioFormatsError::Format(format!(
            "frame data too small ({} < {expected})",
            data.len()
        )))
    }
}

// ---- reader -----------------------------------------------------------------

pub struct Nd2Reader {
    file: Option<BufReader<File>>,
    path: Option<PathBuf>,
    chunks: Vec<Nd2Chunk>,
    meta: Vec<ImageMetadata>,
    current_series: usize,
    image_chunks: Vec<usize>, // indices into chunks[] for ImageDataSeq chunks
    old_jp2_planes: Vec<Vec<OldJp2Plane>>,
    // OME-parity metadata harvested from the LV binary metadata tree.
    physical_size: Option<f64>,
    physical_size_z: Option<f64>,
    channel_names: Vec<String>,
    emission_wavelengths: Vec<f64>,
}

impl Nd2Reader {
    pub fn new() -> Self {
        Nd2Reader {
            file: None,
            path: None,
            chunks: Vec::new(),
            meta: Vec::new(),
            current_series: 0,
            physical_size: None,
            physical_size_z: None,
            channel_names: Vec::new(),
            emission_wavelengths: Vec::new(),
            image_chunks: Vec::new(),
            old_jp2_planes: Vec::new(),
        }
    }

    fn set_old_jp2_id(&mut self, mut reader: BufReader<File>, path: &Path) -> Result<()> {
        if !has_old_nd_box_footer(&mut reader).map_err(BioFormatsError::Io)? {
            return Err(BioFormatsError::UnsupportedFormat(
                "ND2: JP2-backed file is missing old ND box footer".into(),
            ));
        }

        let (planes, size_x, size_y, jp2_bands, pixel_type_code) =
            scan_old_jp2_boxes(&mut reader).map_err(BioFormatsError::Io)?;
        if planes.is_empty() || size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "ND2: old JP2-backed file has no usable JP2 codestreams".into(),
            ));
        }

        let metadata_text = old_nd2_metadata_text(&mut reader).map_err(BioFormatsError::Io)?;
        let metadata_indexes = old_nd2_metadata_indexes(&metadata_text);
        let size_c = old_nd2_component_count(&metadata_text, jp2_bands);
        let mut usable_plane_count = planes.len();
        if size_c > 1 && usable_plane_count % size_c as usize == 1 {
            usable_plane_count -= 1;
        }
        usable_plane_count -= usable_plane_count % size_c as usize;
        if usable_plane_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "ND2: old JP2-backed file has no complete component planes".into(),
            ));
        }

        let metadata_count = metadata_indexes.len();
        let series_count =
            if metadata_count > 1 && usable_plane_count == metadata_count * size_c as usize {
                metadata_count
            } else {
                1
            };
        let size_t = (usable_plane_count / series_count / size_c as usize).max(1) as u32;
        let image_count = size_t * size_c;
        let bits_per_pixel = if pixel_type_code == 0x0f07_0100 || pixel_type_code == 0x0f07_0000 {
            16
        } else {
            8
        };
        let pixel_type = if bits_per_pixel == 16 {
            PixelType::Uint16
        } else {
            PixelType::Uint8
        };
        let dimension_order = if series_count > 1 {
            DimensionOrder::XYCZT
        } else {
            DimensionOrder::XYCTZ
        };

        let mut plane_series = vec![Vec::with_capacity(image_count as usize); series_count];
        for t in 0..size_t as usize {
            for series in 0..series_count {
                for c in 0..size_c as usize {
                    let source = (t * series_count + series) * size_c as usize + c;
                    if source < usable_plane_count {
                        plane_series[series].push(planes[source].clone());
                    }
                }
            }
        }

        let mut metas = Vec::with_capacity(series_count);
        for _ in 0..series_count {
            let mut series_metadata = HashMap::new();
            series_metadata.insert("nd2_old_jp2".into(), MetadataValue::Bool(true));
            series_metadata.insert(
                "nd2_old_jp2_codestreams".into(),
                MetadataValue::Int(planes.len() as i64),
            );
            series_metadata.insert(
                "nd2_old_jp2_used_codestreams".into(),
                MetadataValue::Int(usable_plane_count as i64),
            );
            series_metadata.insert(
                "nd2_metadata_seq_count".into(),
                MetadataValue::Int(metadata_count as i64),
            );

            metas.push(ImageMetadata {
                size_x,
                size_y,
                size_z: 1,
                size_c,
                size_t,
                pixel_type,
                bits_per_pixel,
                image_count,
                dimension_order,
                is_rgb: false,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: false,
                resolution_count: 1,
                series_metadata,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
        }

        self.meta = metas;
        self.current_series = 0;
        self.old_jp2_planes = plane_series;
        self.image_chunks.clear();
        self.chunks.clear();
        reader
            .seek(SeekFrom::Start(0))
            .map_err(BioFormatsError::Io)?;
        self.file = Some(reader);
        self.path = Some(path.to_path_buf());
        Ok(())
    }
}

impl Default for Nd2Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for Nd2Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("nd2"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(&ND2_MAGIC) || looks_like_jpeg2000(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut reader = BufReader::new(f);

        let mut header = [0u8; 8];
        let read = reader.read(&mut header).map_err(BioFormatsError::Io)?;
        reader
            .seek(SeekFrom::Start(0))
            .map_err(BioFormatsError::Io)?;
        if read >= 8 && looks_like_jpeg2000(&header) {
            return self.set_old_jp2_id(reader, path);
        }

        let chunks = match read_chunk_map(&mut reader).map_err(BioFormatsError::Io)? {
            Some(chunks) => chunks,
            None => scan_chunks(&mut reader).map_err(BioFormatsError::Io)?,
        };

        let (mut size_x, mut size_y, mut size_c, mut size_z, mut bpp) =
            (0u32, 0u32, 1u32, 1u32, 8u8);

        for ac in chunks
            .iter()
            .filter(|c| c.name.starts_with("ImageAttributes"))
        {
            let data = read_chunk_data(&mut reader, ac).map_err(BioFormatsError::Io)?;
            // Data may be a raw binary struct OR XML wrapped. Try XML first.
            let xml = String::from_utf8_lossy(&data);
            let (w, h, c, z, b) = parse_nd2_attributes(&xml);
            if w > 0 && h > 0 {
                size_x = w;
                size_y = h;
                if c > 0 {
                    size_c = c;
                }
                if z > 0 {
                    size_z = z;
                }
                if b > 0 {
                    bpp = b;
                }
                break;
            }
        }

        for mc in chunks.iter().filter(|c| {
            c.name.starts_with("ImageMetadata") || c.name.contains("GrabberCameraSettings")
        }) {
            let data = read_chunk_data(&mut reader, mc).map_err(BioFormatsError::Io)?;
            let xml = String::from_utf8_lossy(&data);
            if let Some((w, h)) = rect_sensor_extent(&xml) {
                size_x = w;
                size_y = h;
                let c = nd2_u32_value(&xml, "uiComp").unwrap_or(0);
                if c > 0 {
                    size_c = c;
                }
                if let Some(b) = nd2_bpp_value(&xml) {
                    bpp = b;
                }
                break;
            }
        }

        // Collect image data chunks (ImageDataSeq|N!)
        let image_chunks: Vec<usize> = chunks
            .iter()
            .enumerate()
            .filter(|(_, c)| c.name.starts_with("ImageDataSeq"))
            .map(|(i, _)| i)
            .collect();

        // Infer size_z from number of image chunks if not found in attributes
        if size_z == 1 && !image_chunks.is_empty() {
            size_z = image_chunks.len() as u32;
        }

        // If we still don't know dimensions, try to infer from first image chunk size
        if size_x == 0 {
            if let Some(&idx) = image_chunks.first() {
                let chunk = &chunks[idx];
                if chunk.data_length > 0 {
                    // Assume square with bpp/8 bytes per pixel
                    let bytes_per_px = ((bpp as u64 + 7) / 8).max(1);
                    let total_px = chunk.data_length / bytes_per_px / size_c as u64;
                    let side = (total_px as f64).sqrt() as u32;
                    if side > 0 {
                        size_x = side;
                        size_y = side;
                    }
                }
            }
        }

        let pixel_type = match bpp {
            8 => PixelType::Uint8,
            16 => PixelType::Uint16,
            _ => PixelType::Uint16,
        };

        // Parse the Nikon LV binary metadata tree (ImageMetadataSeqLV /
        // ImageCalibrationLV) for OME attributes: physical pixel size, channel
        // names, emission wavelengths. Matches ND2Reader.iterateIn in Java.
        let mut lv = Nd2LvValues::default();
        for mc in chunks.iter().filter(|c| {
            c.name.starts_with("ImageMetadataSeq")
                || c.name.starts_with("ImageMetadata")
                || c.name.starts_with("ImageCalibration")
        }) {
            if let Ok(data) = read_chunk_data(&mut reader, mc) {
                parse_nd2_lv(&data, &mut lv);
            }
        }
        self.physical_size = lv.calibration;
        self.physical_size_z = lv.z_step;
        self.channel_names = lv.channel_names;
        self.emission_wavelengths = lv.emission_wavelengths;

        // Dimension order: Java ND2Reader builds "XY" + order, then appends any
        // of Z/C/T not already present. With no acquisition-loop order and a
        // single channel this yields XYZCT (see ND2Reader ~1530, ~2014).
        let dimension_order = if size_c > 1 {
            DimensionOrder::XYCZT
        } else {
            DimensionOrder::XYZCT
        };

        let image_count = image_chunks.len() as u32;
        let mut series_metadata: HashMap<String, MetadataValue> = HashMap::new();
        series_metadata.insert("nd2_chunks".into(), MetadataValue::Int(chunks.len() as i64));

        self.meta = vec![ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
            image_count: image_count.max(1),
            dimension_order,
            is_rgb: size_c == 3,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        }];
        self.current_series = 0;
        self.old_jp2_planes.clear();
        self.image_chunks = image_chunks;
        self.chunks = chunks;
        self.file = Some(reader);
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.file = None;
        self.path = None;
        self.meta.clear();
        self.current_series = 0;
        self.chunks.clear();
        self.image_chunks.clear();
        self.old_jp2_planes.clear();
        self.physical_size = None;
        self.physical_size_z = None;
        self.channel_names.clear();
        self.emission_wavelengths.clear();
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.meta.len().max(1)
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_count() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }
    fn series(&self) -> usize {
        self.current_series
    }
    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .get(self.current_series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        if !self.old_jp2_planes.is_empty() {
            let plane = self
                .old_jp2_planes
                .get(self.current_series)
                .and_then(|planes| planes.get(plane_index as usize))
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
            let f = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
            f.seek(SeekFrom::Start(plane.data_offset))
                .map_err(BioFormatsError::Io)?;
            let mut data = vec![0u8; plane.data_length as usize];
            f.read_exact(&mut data).map_err(BioFormatsError::Io)?;
            let expected =
                meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample();
            let decoded = crate::common::codec::decompress_jpeg2000(&data)?;
            return require_exact_frame(decoded, expected, "old ND2 JPEG2000").map_err(
                |e| match e {
                    BioFormatsError::Format(msg) => {
                        BioFormatsError::Format(format!("ND2: plane {plane_index}: {msg}"))
                    }
                    BioFormatsError::Codec(msg) => {
                        BioFormatsError::Codec(format!("ND2: plane {plane_index}: {msg}"))
                    }
                    other => other,
                },
            );
        }

        let chunk_idx = self
            .image_chunks
            .get(plane_index as usize)
            .copied()
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let chunk = &self.chunks[chunk_idx];

        let f = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let data = read_chunk_data(f, chunk).map_err(BioFormatsError::Io)?;

        let bps = meta.pixel_type.bytes_per_sample();
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;
        let size_c = meta.size_c as usize;

        // Java ND2Reader.getScanlinePad() (~2650-2654): one padding sample per
        // row total (not per channel) when BOTH sizeX and sizeC are odd. The
        // stored plane is therefore (sizeX + scanlinePad) * sizeY * sizeC * bpp
        // bytes (openBytes ~277,308), while the output buffer is unpadded.
        let scanline_pad = if meta.size_x % 2 != 0 && meta.size_c % 2 != 0 {
            1
        } else {
            0
        };

        // Stored row length in bytes: sizeX*sizeC samples plus one pad sample.
        let stored_row = (size_x * size_c + scanline_pad) * bps;
        let stored_expected = stored_row * size_y;

        let decoded = decode_nd2_frame_payload(&data, stored_expected).map_err(|e| match e {
            BioFormatsError::Format(msg) => {
                BioFormatsError::Format(format!("ND2: plane {plane_index}: {msg}"))
            }
            BioFormatsError::UnsupportedFormat(msg) => {
                BioFormatsError::UnsupportedFormat(format!("ND2: plane {plane_index}: {msg}"))
            }
            BioFormatsError::Codec(msg) => {
                BioFormatsError::Codec(format!("ND2: plane {plane_index}: {msg}"))
            }
            other => other,
        })?;

        if scanline_pad == 0 {
            return Ok(decoded);
        }

        // De-pad: strip the trailing pad sample from each row so the returned
        // buffer is the unpadded sizeX*sizeY*sizeC*bpp plane (Java openBytes
        // copies rowLength bytes then skips scanlinePad*bpp per row, ~280-289).
        let out_row = size_x * size_c * bps;
        let mut out = Vec::with_capacity(out_row * size_y);
        for row in 0..size_y {
            let start = row * stored_row;
            out.extend_from_slice(&decoded[start..start + out_row]);
        }
        Ok(out)
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
        let meta = self.metadata();
        let spp = if self.old_jp2_planes.is_empty() {
            meta.size_c as usize
        } else {
            1
        };
        crop_full_plane("ND2", &full, meta, spp, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.get(self.current_series)?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = ome.images.get_mut(0)?;

        // Image name: "<filename> (series <n>)" per ND2Reader (~2263).
        if let Some(path) = &self.path {
            if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                img.name = Some(format!("{} (series {})", fname, self.current_series + 1));
            }
        }

        // Physical pixel size: dCalibration applies to X and Y (µm/px).
        if let Some(cal) = self.physical_size.filter(|v| *v > 0.0) {
            img.physical_size_x = Some(cal);
            img.physical_size_y = Some(cal);
        }
        if let Some(z) = self.physical_size_z.filter(|v| *v > 0.0) {
            img.physical_size_z = Some(z);
        }

        // Channel names and emission wavelengths.
        for (c, channel) in img.channels.iter_mut().enumerate() {
            if let Some(name) = self.channel_names.get(c) {
                channel.name = Some(name.clone());
            }
            if let Some(em) = self.emission_wavelengths.get(c).filter(|v| **v > 0.0) {
                channel.emission_wavelength = Some(*em);
            }
        }

        Some(ome)
    }
}
