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

fn metadata_seq_index(name: &str) -> Option<usize> {
    let suffix = name
        .strip_prefix("ImageMetadataSeqLV|")
        .or_else(|| name.strip_prefix("ImageMetadataSeq|"))?
        .trim_end_matches('!');
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

fn read_chunk_prefix(
    f: &mut BufReader<File>,
    chunk: &Nd2Chunk,
    max_len: usize,
) -> std::io::Result<Vec<u8>> {
    let len = chunk.data_length.min(max_len as u64) as usize;
    f.seek(SeekFrom::Start(chunk.data_offset))?;
    let mut buf = vec![0u8; len];
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
    /// Excitation wavelengths (Java ND2Handler: exWave). Populated only from the
    /// text-annotation "Excitation wavelength" key; the LV/XML metadata block
    /// does not carry these, mirroring upstream behaviour.
    excitation_wavelengths: Vec<f64>,
    /// `TextInfoItem*` annotation strings collected during the LV walk
    /// (ND2Reader.iterateIn:2130-2133 → textInfos), later fed to parse_text.
    text_infos: Vec<String>,
    /// dExposureTime per channel, converted from ms to seconds (Java: /1000).
    exposure_time: Vec<f64>,
    /// uiColor → sDescription channel name → packed BGR color, mirroring
    /// ND2Reader.iterateIn (channelColors map + textChannelNames list).
    channel_colors: HashMap<String, i32>,
    text_channel_names: Vec<String>,
    /// Number of dPosX entries seen (Java: positionCount++ on dPosX).
    position_count: u32,
    /// dObjectiveMag → objectiveMag (must be > 0).
    objective_mag: Option<f64>,
    /// sObjective → objectiveModel.
    objective_model: Option<String>,
    /// dObjectiveNA → lensNA (also from text "Numerical Aperture").
    lens_na: Option<f64>,
    /// dRefractIndex1 / "Refractive Index" → refractiveIndex.
    refractive_index: Option<f64>,
    /// Stage positions per acquired position (µm). Populated from the XML
    /// `<dPosX>/<item_N>` lists (ND2Handler.startElement:513-527).
    pos_x: Vec<f64>,
    pos_y: Vec<f64>,
    pos_z: Vec<f64>,
    /// Sum of `<iXFields>` values (ND2Handler: nXFields).
    n_x_fields: u32,
    /// `dCompressionParam > 0` ⇒ lossless (ND2Handler:548-550).
    is_lossless: bool,
}

#[derive(Debug, Clone)]
struct Nd2LoopDescriptor {
    kind: &'static str,
    count: Option<u32>,
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
    // `current_color` carries the most recent uiColor within this level, so the
    // next sDescription can be paired with it (ND2Reader.iterateIn).
    fn walk(data: &[u8], mut p: usize, end: usize, depth: u32, out: &mut Nd2LvValues) -> usize {
        if depth > 64 {
            return end;
        }
        let mut current_color: Option<i32> = None;
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
                1 => p += 1, // bool
                2 | 3 => {
                    // int32 / uint32. uiColor sets the pending channel color
                    // (Java: currentColor = (Integer) value).
                    if name == "uiColor" {
                        current_color = read_i32(data, p);
                    }
                    p += 4;
                }
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
                            // dExposureTime is milliseconds; Java stores /1000 s
                            // and only when value > 0 (ND2Reader.iterateIn:2206).
                            "dExposureTime" => {
                                if v > 0.0 {
                                    out.exposure_time.push(v / 1000.0);
                                }
                            }
                            // Each dPosX marks one acquired position (positionCount++).
                            "dPosX" => out.position_count += 1,
                            // dObjectiveMag → objectiveMag (only when > 0).
                            "dObjectiveMag" => {
                                if v > 0.0 && out.objective_mag.is_none() {
                                    out.objective_mag = Some(v);
                                }
                            }
                            // dObjectiveNA → lensNA (handler.parseKeyAndValue).
                            "dObjectiveNA" => {
                                if v > 0.0 && out.lens_na.is_none() {
                                    out.lens_na = Some(v);
                                }
                            }
                            // dRefractIndex1 → refractiveIndex (handler).
                            "dRefractIndex1" => {
                                if v > 0.0 && out.refractive_index.is_none() {
                                    out.refractive_index = Some(v);
                                }
                            }
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
                        out.channel_names.push(s.clone());
                        // Pair the channel name with the pending uiColor, mirroring
                        // ND2Reader.iterateIn:2197-2202 (only when a color was seen).
                        if let Some(color) = current_color {
                            out.text_channel_names.push(s.clone());
                            out.channel_colors.insert(s, color);
                        }
                    } else if name == "sObjective" && !s.is_empty() && out.objective_model.is_none()
                    {
                        out.objective_model = Some(s);
                    } else if name.starts_with("TextInfoItem") && !s.is_empty() {
                        // Collect text-annotation blobs for the backup handler
                        // (ND2Reader.iterateIn:2130-2133 → textInfos).
                        out.text_infos.push(s);
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
                    let Some(count) = read_i32(data, p) else {
                        break;
                    };
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

/// Collect the `<item_N>` numeric children of the first `<tag>…</tag>` element,
/// mirroring ND2Handler's `dPosX`/`dPosY`/`dPosZ` position-list parsing.
fn nd2_xml_item_list_f64(xml: &str, tag: &str) -> Vec<f64> {
    let open = format!("<{tag}");
    let close = format!("</{tag}>");
    let Some(pos) = xml.find(&open) else {
        return Vec::new();
    };
    let after_open = &xml[pos..];
    let Some(gt) = after_open.find('>') else {
        return Vec::new();
    };
    if after_open[..gt].trim_end().ends_with('/') {
        return Vec::new();
    }
    let content_start = pos + gt + 1;
    let Some(end) = xml[content_start..].find(&close) else {
        return Vec::new();
    };
    let body = &xml[content_start..content_start + end];

    let mut items = Vec::new();
    let mut cursor = 0;
    while let Some(rel) = body[cursor..].find("<item_") {
        let item_pos = cursor + rel;
        let after = &body[item_pos..];
        let Some(item_gt) = after.find('>') else {
            break;
        };
        let item_tag = &after[..item_gt];
        let value = xml_attr(item_tag, "value").or_else(|| {
            if item_tag.trim_end().ends_with('/') {
                None
            } else {
                let item_content = item_pos + item_gt + 1;
                body[item_content..]
                    .find("</item_")
                    .map(|e| body[item_content..item_content + e].trim().to_string())
            }
        });
        if let Some(v) = value.and_then(|v| v.parse::<f64>().ok()).filter(|v| v.is_finite()) {
            items.push(v);
        }
        cursor = item_pos + item_gt + 1;
    }
    items
}

fn nd2_xml_f64_value(xml: &str, tag: &str) -> Option<f64> {
    xml_value(xml, tag)?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v > 0.0)
}

fn parse_nd2_xml_metadata(xml: &str, out: &mut Nd2LvValues) {
    if out.calibration.is_none() {
        out.calibration = nd2_xml_f64_value(xml, "dCalibration");
    }
    if out.z_step.is_none() {
        out.z_step = nd2_xml_f64_value(xml, "dZStep");
    }

    for name in xml_values(xml, "sDescription") {
        if !name.is_empty() && !out.channel_names.contains(&name) {
            out.channel_names.push(name);
        }
    }
    for wavelength in xml_values(xml, "EmWavelength")
        .into_iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
    {
        if !out.emission_wavelengths.contains(&wavelength) {
            out.emission_wavelengths.push(wavelength);
        }
    }

    // Objective NA / magnification / model and refractive index
    // (ND2Handler.parseKeyAndValue:663-694, 669).
    if out.objective_mag.is_none() {
        out.objective_mag = nd2_xml_f64_value(xml, "dObjectiveMag");
    }
    if out.lens_na.is_none() {
        out.lens_na = nd2_xml_f64_value(xml, "dObjectiveNA");
    }
    if out.refractive_index.is_none() {
        out.refractive_index = nd2_xml_f64_value(xml, "dRefractIndex1");
    }
    if out.objective_model.is_none() {
        out.objective_model = xml_value(xml, "sObjective")
            .or_else(|| xml_value(xml, "wsObjectiveName"))
            .filter(|s| !s.is_empty());
    }

    // dExposureTime (ms → s, value > 0), matching ND2Reader.iterateIn:2206-2209.
    for exposure in xml_values(xml, "dExposureTime")
        .into_iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value > 0.0)
    {
        out.exposure_time.push(exposure / 1000.0);
    }

    // Stage position lists (µm), one item per acquired position.
    if out.pos_x.is_empty() {
        out.pos_x = nd2_xml_item_list_f64(xml, "dPosX");
    }
    if out.pos_y.is_empty() {
        out.pos_y = nd2_xml_item_list_f64(xml, "dPosY");
    }
    if out.pos_z.is_empty() {
        out.pos_z = nd2_xml_item_list_f64(xml, "dPosZ");
    }
    if out.position_count == 0 {
        out.position_count = out.pos_x.len() as u32;
    }

    // Number of X fields (ND2Handler.iXFields summed, capped >6 ⇒ 0 by reader).
    for fields in xml_values(xml, "iXFields")
        .into_iter()
        .filter_map(|value| value.parse::<u32>().ok())
    {
        out.n_x_fields = out.n_x_fields.saturating_add(fields);
    }

    // dCompressionParam > 0 ⇒ lossless (ND2Handler:548-550).
    if let Some(param) = nd2_xml_f64_value(xml, "dCompressionParam") {
        out.is_lossless = param > 0.0;
    }
}

/// Parse one text-annotation block into `out`, mirroring `ND2Reader.parseText`.
///
/// Java first tries to parse the string as XML through an `ND2Handler`
/// (`XMLTools.parseXML`); on failure it falls back to a line-based
/// `key: value` scan handed to `ND2Handler.parseKeyAndValue`. We reuse the
/// existing XML metadata path (`parse_nd2_xml_metadata`) for the XML case and
/// implement the `Name` / `Emission wavelength` / `Excitation wavelength`
/// key handling for the line-based case (ND2Handler.parseKeyAndValue:830-894).
/// The resulting `out` is the equivalent of Java's `backupHandler`.
fn parse_text(text: &str, out: &mut Nd2LvValues) {
    let trimmed = text.trim();
    // XML case: reuse the same parser ND2Handler uses for metadata XML.
    if trimmed.contains('<') && trimmed.contains('>') {
        parse_nd2_xml_metadata(trimmed, out);
    }

    // Line-based fallback (ND2Handler.parseKeyAndValue). This runs regardless,
    // matching how the text key/value pairs supply channel names and emission /
    // excitation wavelengths that the XML form may not carry.
    for line in text.split('\n') {
        let Some(sep) = line.find(':') else { continue };
        let key = line[..sep].trim();
        let value = line[sep + 1..].trim();
        if value.is_empty() {
            continue;
        }
        if key == "Name" {
            // ND2Handler:830-831 / 908-909 — channel name.
            if !out.channel_names.contains(&value.to_string()) {
                out.channel_names.push(value.to_string());
            }
        } else if key.eq_ignore_ascii_case("Emission wavelength") {
            // ND2Handler:888-890 — first whitespace-delimited token as f64.
            if let Some(v) = value
                .split_whitespace()
                .next()
                .and_then(|t| t.parse::<f64>().ok())
            {
                out.emission_wavelengths.push(v);
            }
        } else if key.eq_ignore_ascii_case("Excitation wavelength") {
            // ND2Handler:892-894 — first whitespace-delimited token as f64.
            if let Some(v) = value
                .split_whitespace()
                .next()
                .and_then(|t| t.parse::<f64>().ok())
            {
                out.excitation_wavelengths.push(v);
            }
        }
    }
}

fn nd2_xml_plane_timestamp_seconds(xml: &str) -> Option<f64> {
    [
        "dTimeMSec",
        "dTimeMs",
        "dTime",
        "dRelativeTime",
        "TimeStamp",
    ]
    .into_iter()
    .find_map(|tag| {
        let value = xml_value(xml, tag)?
            .parse::<f64>()
            .ok()
            .filter(|value| value.is_finite() && *value >= 0.0)?;
        Some(if tag.contains("MS") || tag.contains("Ms") {
            value / 1000.0
        } else {
            value
        })
    })
}

fn nd2_xml_plane_z_position(xml: &str) -> Option<f64> {
    xml_value(xml, "dZPos")
        .or_else(|| xml_value(xml, "dZPosition"))
        .or_else(|| xml_value(xml, "ZPosition"))
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn nd2_xml_ui_count_for_runtype(xml: &str, runtype_suffix: &str) -> Option<u32> {
    let mut cursor = 0;
    while let Some(relative_pos) = xml[cursor..].find("<uiCount") {
        let pos = cursor + relative_pos;
        let after_open = &xml[pos..];
        let Some(gt) = after_open.find('>') else {
            break;
        };
        let attrs = &after_open[..gt];
        if xml_attr(attrs, "runtype")
            .as_deref()
            .is_some_and(|runtype| runtype.ends_with(runtype_suffix))
        {
            let value = xml_attr(attrs, "value").or_else(|| {
                if attrs.trim_end().ends_with('/') {
                    None
                } else {
                    let content_start = pos + gt + 1;
                    xml[content_start..]
                        .find("</uiCount>")
                        .map(|end| xml[content_start..content_start + end].trim().to_string())
                }
            });
            if let Some(count) = value
                .and_then(|value| value.parse::<u32>().ok())
                .filter(|&count| count > 0)
            {
                return Some(count);
            }
        }
        cursor = pos + gt + 1;
    }
    None
}

fn nd2_loop_kind_from_runtype(runtype: &str) -> Option<&'static str> {
    [
        ("XYPosLoop", "XYPosLoop"),
        ("ZStackLoop", "ZStackLoop"),
        ("TimeLoop", "TimeLoop"),
    ]
    .into_iter()
    .find_map(|(suffix, kind)| runtype.ends_with(suffix).then_some(kind))
}

fn nd2_xml_loop_descriptors(xml: &str) -> Vec<Nd2LoopDescriptor> {
    let mut loops = Vec::new();
    let mut cursor = 0;
    while let Some(relative_pos) = xml[cursor..].find('<') {
        let pos = cursor + relative_pos;
        let after_open = &xml[pos..];
        let Some(gt) = after_open.find('>') else {
            break;
        };
        let tag_text = &after_open[..gt];
        if let Some(runtype) = xml_attr(tag_text, "runtype") {
            if let Some(kind) = nd2_loop_kind_from_runtype(&runtype) {
                let count = xml_attr(tag_text, "value").and_then(|value| {
                    value
                        .parse::<u32>()
                        .ok()
                        .filter(|&count| count > 0 && count != u32::MAX)
                });
                loops.push(Nd2LoopDescriptor { kind, count });
            }
        }
        cursor = pos + gt + 1;
    }
    loops
}

fn nd2_update_loop_descriptors_from_xml(xml: &str, out: &mut Vec<Nd2LoopDescriptor>) {
    for descriptor in nd2_xml_loop_descriptors(xml) {
        if let Some(existing) = out
            .iter_mut()
            .find(|existing| existing.kind == descriptor.kind)
        {
            if existing.count.is_none() {
                existing.count = descriptor.count;
            }
        } else {
            out.push(descriptor);
        }
    }
}

fn nd2_update_loop_counts_from_xml(
    xml: &str,
    loop_size_z: &mut Option<u32>,
    loop_size_t: &mut Option<u32>,
    loop_series_count: &mut Option<u32>,
) {
    if loop_size_z.is_none() {
        *loop_size_z = nd2_xml_ui_count_for_runtype(xml, "ZStackLoop");
    }
    if loop_size_t.is_none() {
        *loop_size_t = nd2_xml_ui_count_for_runtype(xml, "TimeLoop");
    }
    if loop_series_count.is_none() {
        *loop_series_count = nd2_xml_ui_count_for_runtype(xml, "XYPosLoop");
    }
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

fn decompress_nd2_zlib_chunk(data: &[u8], remaining: usize) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read as _;

    let mut dec = ZlibDecoder::new(data);
    let mut out = Vec::new();
    dec.by_ref()
        .take(remaining.saturating_add(1) as u64)
        .read_to_end(&mut out)
        .map_err(BioFormatsError::Io)?;
    if out.len() > remaining {
        Err(BioFormatsError::Format(format!(
            "per-chunk zlib frame has trailing decoded data ({} > {remaining})",
            out.len()
        )))
    } else {
        Ok(out)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Nd2FrameChunkTable {
    table_offset: usize,
    chunk_count: usize,
    entry_width: usize,
    total_payload_len: usize,
    first_payload_offset: usize,
    ranges: Vec<(usize, usize)>,
}

fn read_le_u32(bytes: &[u8], offset: usize) -> Option<u32> {
    Some(u32::from_le_bytes(
        bytes.get(offset..offset + 4)?.try_into().ok()?,
    ))
}

fn read_le_u64_usize(bytes: &[u8], offset: usize) -> Option<usize> {
    usize::try_from(u64::from_le_bytes(
        bytes.get(offset..offset + 8)?.try_into().ok()?,
    ))
    .ok()
}

fn nd2_frame_chunk_table(
    prefix: &[u8],
    total_len: usize,
    expected: usize,
) -> Option<Nd2FrameChunkTable> {
    nd2_frame_chunk_table_inner(prefix, total_len, Some(expected), 4)
        .or_else(|| nd2_frame_chunk_table_inner(prefix, total_len, Some(expected), 8))
}

fn nd2_frame_chunk_table_inner(
    prefix: &[u8],
    total_len: usize,
    expected: Option<usize>,
    entry_width: usize,
) -> Option<Nd2FrameChunkTable> {
    const FRAME_PREFIX_LEN: usize = 8;
    const MAX_CHUNK_TABLE_ENTRIES: usize = 1024;
    if entry_width != 4 && entry_width != 8 {
        return None;
    }

    for table_offset in [0usize, FRAME_PREFIX_LEN, 4096] {
        let Some(chunk_count) = read_le_u32(prefix, table_offset).map(|count| count as usize)
        else {
            continue;
        };
        if chunk_count == 0 || chunk_count > MAX_CHUNK_TABLE_ENTRIES {
            continue;
        }
        let table_len = 4usize.checked_add(chunk_count.checked_mul(entry_width * 2)?)?;
        let table_end = table_offset.checked_add(table_len)?;
        if table_end > prefix.len() {
            continue;
        }

        let mut ranges = Vec::with_capacity(chunk_count);
        let mut total_payload_len = 0usize;
        for i in 0..chunk_count {
            let entry = table_offset + 4 + i * entry_width * 2;
            let (offset, length) = if entry_width == 4 {
                (
                    read_le_u32(prefix, entry)? as usize,
                    read_le_u32(prefix, entry + 4)? as usize,
                )
            } else {
                (
                    read_le_u64_usize(prefix, entry)?,
                    read_le_u64_usize(prefix, entry + 8)?,
                )
            };
            let end = offset.checked_add(length)?;
            if length == 0 || offset < table_end || end > total_len {
                ranges.clear();
                break;
            }
            total_payload_len = total_payload_len.checked_add(length)?;
            ranges.push((offset, end));
        }
        if ranges.len() != chunk_count
            || expected.is_some_and(|expected| total_payload_len != expected)
        {
            continue;
        }

        ranges.sort_unstable();
        if ranges.windows(2).any(|pair| pair[0].1 > pair[1].0) {
            continue;
        }

        return Some(Nd2FrameChunkTable {
            table_offset,
            chunk_count,
            entry_width,
            total_payload_len,
            first_payload_offset: ranges[0].0,
            ranges,
        });
    }

    None
}

fn nd2_frame_chunk_table_any_payload(
    prefix: &[u8],
    total_len: usize,
) -> Option<Nd2FrameChunkTable> {
    nd2_frame_chunk_table_inner(prefix, total_len, None, 4)
        .or_else(|| nd2_frame_chunk_table_inner(prefix, total_len, None, 8))
}

fn assemble_nd2_frame_chunks(data: &[u8], table: &Nd2FrameChunkTable) -> Vec<u8> {
    let mut out = Vec::with_capacity(table.total_payload_len);
    for &(start, end) in &table.ranges {
        out.extend_from_slice(&data[start..end]);
    }
    out
}

fn nd2_chunk_table_label(table: &Nd2FrameChunkTable, suffix: &str) -> Option<&'static str> {
    match (table.entry_width, suffix) {
        (4, "") => Some("chunk_table_le32"),
        (8, "") => Some("chunk_table_le64"),
        (4, "_zlib") => Some("chunk_table_le32_zlib"),
        (8, "_zlib") => Some("chunk_table_le64_zlib"),
        (4, "_jpeg2000") => Some("chunk_table_le32_jpeg2000"),
        (8, "_jpeg2000") => Some("chunk_table_le64_jpeg2000"),
        (4, "_per_chunk_zlib") => Some("chunk_table_le32_per_chunk_zlib"),
        (8, "_per_chunk_zlib") => Some("chunk_table_le64_per_chunk_zlib"),
        (4, "_per_chunk_zlib_unsupported") => Some("chunk_table_le32_per_chunk_zlib_unsupported"),
        (8, "_per_chunk_zlib_unsupported") => Some("chunk_table_le64_per_chunk_zlib_unsupported"),
        (4, "_per_chunk_jpeg2000_unsupported") => {
            Some("chunk_table_le32_per_chunk_jpeg2000_unsupported")
        }
        (8, "_per_chunk_jpeg2000_unsupported") => {
            Some("chunk_table_le64_per_chunk_jpeg2000_unsupported")
        }
        (4, "_mixed_per_chunk_compression_unsupported") => {
            Some("chunk_table_le32_mixed_per_chunk_compression_unsupported")
        }
        (8, "_mixed_per_chunk_compression_unsupported") => {
            Some("chunk_table_le64_mixed_per_chunk_compression_unsupported")
        }
        _ => None,
    }
}

fn nd2_chunk_table_per_chunk_compression_label(
    data: &[u8],
    table: &Nd2FrameChunkTable,
) -> Option<&'static str> {
    if table.chunk_count < 2 {
        return None;
    }

    let mut zlib_chunks = 0usize;
    let mut jpeg2000_chunks = 0usize;
    for &(start, end) in &table.ranges {
        let payload = data.get(start..end)?;
        if looks_like_zlib(payload) {
            zlib_chunks += 1;
        } else if looks_like_jpeg2000(payload) {
            jpeg2000_chunks += 1;
        }
    }

    if zlib_chunks == table.chunk_count {
        nd2_chunk_table_label(table, "_per_chunk_zlib")
    } else if jpeg2000_chunks == table.chunk_count {
        nd2_chunk_table_label(table, "_per_chunk_jpeg2000_unsupported")
    } else if zlib_chunks + jpeg2000_chunks == table.chunk_count
        && zlib_chunks > 0
        && jpeg2000_chunks > 0
    {
        nd2_chunk_table_label(table, "_mixed_per_chunk_compression_unsupported")
    } else {
        None
    }
}

fn nd2_chunk_table_is_per_chunk_zlib(data: &[u8], table: &Nd2FrameChunkTable) -> bool {
    table.chunk_count >= 2
        && table
            .ranges
            .iter()
            .all(|&(start, end)| data.get(start..end).is_some_and(looks_like_zlib))
}

fn nd2_chunk_table_summary(table: &Nd2FrameChunkTable) -> String {
    format!(
        "offset={}, entry_width={}, count={}, first_payload={}, payload_bytes={}",
        table.table_offset,
        table.entry_width,
        table.chunk_count,
        table.first_payload_offset,
        table.total_payload_len
    )
}

fn nd2_chunk_table_payload_encoding(
    prefix: &[u8],
    total_len: usize,
    expected: usize,
) -> Option<(&'static str, Nd2FrameChunkTable)> {
    if let Some(table) = nd2_frame_chunk_table(prefix, total_len, expected) {
        let encoding = nd2_chunk_table_label(&table, "")?;
        return Some((encoding, table));
    }

    let table = nd2_frame_chunk_table_any_payload(prefix, total_len)?;
    if let Some(encoding) = nd2_chunk_table_per_chunk_compression_label(prefix, &table) {
        return Some((encoding, table));
    }

    let first_payload = prefix.get(table.first_payload_offset..)?;
    if looks_like_zlib(first_payload) {
        Some((nd2_chunk_table_label(&table, "_zlib")?, table))
    } else if looks_like_jpeg2000(first_payload) {
        Some((nd2_chunk_table_label(&table, "_jpeg2000")?, table))
    } else {
        None
    }
}

fn nd2_frame_payload_hint(data: &[u8], expected: usize) -> &'static str {
    nd2_frame_payload_layout(data, data.len(), expected).0
}

fn nd2_frame_payload_layout(
    prefix: &[u8],
    total_len: usize,
    expected: usize,
) -> (&'static str, usize) {
    const FRAME_PREFIX_LEN: usize = 8;
    const NIKON_PAYLOAD_OFFSET: usize = 4096;
    const MAX_RAW_TRAILER_LEN: usize = 4096;

    if total_len == expected {
        return ("raw", 0);
    }

    if total_len == expected + FRAME_PREFIX_LEN {
        if let Some(payload) = prefix.get(FRAME_PREFIX_LEN..) {
            if !looks_like_zlib(payload) && !looks_like_jpeg2000(payload) {
                return ("raw_with_8_byte_prefix", FRAME_PREFIX_LEN);
            }
        }
    }

    if let Some((encoding, table)) = nd2_chunk_table_payload_encoding(prefix, total_len, expected) {
        return (encoding, table.table_offset);
    }

    if total_len > expected + FRAME_PREFIX_LEN
        && total_len - expected - FRAME_PREFIX_LEN <= MAX_RAW_TRAILER_LEN
    {
        if let Some(payload) = prefix.get(FRAME_PREFIX_LEN..) {
            if nd2_prefix_timestamp_seconds(prefix, FRAME_PREFIX_LEN).is_some()
                && !looks_like_zlib(payload)
                && !looks_like_jpeg2000(payload)
            {
                return ("raw_with_8_byte_prefix_and_trailer", FRAME_PREFIX_LEN);
            }
        }
    }

    if total_len == expected + NIKON_PAYLOAD_OFFSET {
        if let Some(payload) = prefix.get(NIKON_PAYLOAD_OFFSET..) {
            if !looks_like_zlib(payload) && !looks_like_jpeg2000(payload) {
                return ("raw_after_4096_byte_prefix", NIKON_PAYLOAD_OFFSET);
            }
        }
    }

    if total_len > expected + NIKON_PAYLOAD_OFFSET
        && total_len - expected - NIKON_PAYLOAD_OFFSET <= MAX_RAW_TRAILER_LEN
    {
        if let Some(payload) = prefix.get(NIKON_PAYLOAD_OFFSET..) {
            if !looks_like_zlib(payload) && !looks_like_jpeg2000(payload) {
                return (
                    "raw_after_4096_byte_prefix_and_trailer",
                    NIKON_PAYLOAD_OFFSET,
                );
            }
        }
    }

    for prefix_len in [0usize, FRAME_PREFIX_LEN, NIKON_PAYLOAD_OFFSET] {
        let Some(payload) = prefix.get(prefix_len..) else {
            continue;
        };
        let prefix = match prefix_len {
            0 => "",
            FRAME_PREFIX_LEN => "_after_8_byte_prefix",
            NIKON_PAYLOAD_OFFSET => "_after_4096_byte_prefix",
            _ => "",
        };

        if looks_like_zlib(payload) {
            return match prefix {
                "" => ("zlib", prefix_len),
                "_after_8_byte_prefix" => ("zlib_after_8_byte_prefix", prefix_len),
                "_after_4096_byte_prefix" => ("zlib_after_4096_byte_prefix", prefix_len),
                _ => ("zlib", prefix_len),
            };
        }

        if looks_like_jpeg2000(payload) {
            return match prefix {
                "" => ("jpeg2000", prefix_len),
                "_after_8_byte_prefix" => ("jpeg2000_after_8_byte_prefix", prefix_len),
                "_after_4096_byte_prefix" => ("jpeg2000_after_4096_byte_prefix", prefix_len),
                _ => ("jpeg2000", prefix_len),
            };
        }
    }

    if let Some((encoding, _)) = nd2_chunk_table_payload_encoding(prefix, total_len, expected) {
        return (encoding, 0);
    }

    if total_len > expected
        && expected >= 1024
        && total_len - expected <= MAX_RAW_TRAILER_LEN
        && !looks_like_zlib(prefix)
        && !looks_like_jpeg2000(prefix)
    {
        return ("raw_with_trailer", 0);
    }

    if total_len > expected {
        ("unknown_oversized", 0)
    } else {
        ("too_small", 0)
    }
}

fn nd2_prefix_timestamp_seconds(prefix: &[u8], payload_prefix_len: usize) -> Option<f64> {
    if payload_prefix_len != 8 {
        return None;
    }
    let bytes: [u8; 8] = prefix.get(..8)?.try_into().ok()?;
    let value = f64::from_le_bytes(bytes);
    // Real ND2 frame timestamps are elapsed seconds. Treat zero and tiny
    // denormal-looking values as pixel data, so old raw-with-trailer payloads
    // whose first eight pixels happen to be finite doubles are not shifted.
    (value.is_finite() && (1.0e-9..1.0e12).contains(&value)).then_some(value)
}

fn stored_expected_for_nd2_frame(
    size_x: u32,
    size_y: u32,
    size_c: u32,
    pixel_type: PixelType,
) -> usize {
    let scanline_pad = if size_x % 2 != 0 && size_c % 2 != 0 {
        1usize
    } else {
        0usize
    };
    ((size_x as usize * size_c as usize + scanline_pad) * pixel_type.bytes_per_sample())
        * size_y as usize
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

    if let Some(decoded) = decode_nd2_frame_chunk_table(data, expected, Some(FRAME_PREFIX_LEN)) {
        return decoded;
    }

    if data.len() > expected + FRAME_PREFIX_LEN
        && data.len() - expected - FRAME_PREFIX_LEN <= MAX_RAW_TRAILER_LEN
    {
        let payload = &data[FRAME_PREFIX_LEN..];
        if nd2_prefix_timestamp_seconds(data, FRAME_PREFIX_LEN).is_some()
            && !looks_like_zlib(payload)
            && !looks_like_jpeg2000(payload)
        {
            return Ok(payload[..expected].to_vec());
        }
    }

    if let Some(decoded) = decode_nd2_frame_chunk_table(data, expected, Some(NIKON_PAYLOAD_OFFSET))
    {
        return decoded;
    }

    if data.len() > expected + NIKON_PAYLOAD_OFFSET
        && data.len() - expected - NIKON_PAYLOAD_OFFSET <= MAX_RAW_TRAILER_LEN
    {
        let payload = &data[NIKON_PAYLOAD_OFFSET..];
        if !looks_like_zlib(payload) && !looks_like_jpeg2000(payload) {
            return Ok(payload[..expected].to_vec());
        }
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

    if let Some(decoded) = decode_nd2_frame_chunk_table(data, expected, None) {
        return decoded;
    }

    if data.len() > expected
        && expected >= 1024
        && data.len() - expected <= MAX_RAW_TRAILER_LEN
        && !looks_like_zlib(data)
        && !looks_like_jpeg2000(data)
    {
        return Ok(data[..expected].to_vec());
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

fn decode_nd2_frame_chunk_table(
    data: &[u8],
    expected: usize,
    required_table_offset: Option<usize>,
) -> Option<Result<Vec<u8>>> {
    let table_matches = |table: &Nd2FrameChunkTable| {
        required_table_offset.is_none_or(|required| table.table_offset == required)
    };

    if let Some(table) = nd2_frame_chunk_table(data, data.len(), expected).filter(table_matches) {
        if nd2_chunk_table_is_per_chunk_zlib(data, &table) {
            let mut out = Vec::with_capacity(expected);
            for &(start, end) in &table.ranges {
                let remaining = expected.saturating_sub(out.len());
                match decompress_nd2_zlib_chunk(&data[start..end], remaining) {
                    Ok(decoded) => out.extend_from_slice(&decoded),
                    Err(err) => return Some(Err(err)),
                }
            }
            return Some(require_exact_frame(out, expected, "per-chunk zlib"));
        }
        return Some(Ok(assemble_nd2_frame_chunks(data, &table)));
    }

    let table = nd2_frame_chunk_table_any_payload(data, data.len()).filter(table_matches)?;
    if nd2_chunk_table_is_per_chunk_zlib(data, &table) {
        let mut out = Vec::with_capacity(expected);
        for &(start, end) in &table.ranges {
            let remaining = expected.saturating_sub(out.len());
            match decompress_nd2_zlib_chunk(&data[start..end], remaining) {
                Ok(decoded) => out.extend_from_slice(&decoded),
                Err(err) => return Some(Err(err)),
            }
        }
        return Some(require_exact_frame(out, expected, "per-chunk zlib"));
    }

    if let Some(encoding) = nd2_chunk_table_per_chunk_compression_label(data, &table) {
        return Some(Err(BioFormatsError::UnsupportedFormat(format!(
            "unsupported chunk-table compression layout {encoding} ({}, expected={expected})",
            nd2_chunk_table_summary(&table)
        ))));
    }

    let payload = assemble_nd2_frame_chunks(data, &table);
    if looks_like_zlib(&payload) {
        return Some(decompress_nd2_zlib(&payload, expected));
    }
    if looks_like_jpeg2000(&payload) {
        let decoded = crate::common::codec::decompress_jpeg2000(&payload)
            .and_then(|decoded| require_exact_frame(decoded, expected, "JPEG2000 chunk-table"));
        return Some(decoded);
    }
    if table.total_payload_len != expected {
        return Some(Err(BioFormatsError::UnsupportedFormat(format!(
            "unsupported chunk-table frame encoding ({} payload bytes for {expected}-byte plane)",
            table.total_payload_len
        ))));
    }

    None
}

fn nd2_interleaved_position_planes(
    position_count: usize,
    planes_per_position: usize,
) -> Vec<Vec<usize>> {
    (0..position_count)
        .map(|series| {
            (0..planes_per_position)
                .map(|plane| plane * position_count + series)
                .collect::<Vec<_>>()
        })
        .collect()
}

fn nd2_contiguous_position_planes(
    position_count: usize,
    planes_per_position: usize,
) -> Vec<Vec<usize>> {
    (0..position_count)
        .map(|series| {
            let start = series * planes_per_position;
            (start..start + planes_per_position).collect::<Vec<_>>()
        })
        .collect()
}

fn nd2_z_variation_score(source_planes: &[Vec<usize>], plane_position_z: &[Option<f64>]) -> usize {
    source_planes
        .iter()
        .filter(|planes| {
            let mut values = planes
                .iter()
                .filter_map(|&plane| plane_position_z.get(plane).copied().flatten());
            let Some(first) = values.next() else {
                return false;
            };
            values.any(|value| (value - first).abs() > 1.0e-9)
        })
        .count()
}

fn nd2_choose_xy_position_layout(
    position_count: usize,
    planes_per_position: usize,
    size_z: u32,
    plane_position_z: &[Option<f64>],
    loop_descriptors: &[Nd2LoopDescriptor],
) -> (&'static str, Vec<Vec<usize>>, &'static str) {
    let interleaved = nd2_interleaved_position_planes(position_count, planes_per_position);
    let contiguous = nd2_contiguous_position_planes(position_count, planes_per_position);

    if size_z > 1 && plane_position_z.iter().all(Option::is_some) {
        let interleaved_score = nd2_z_variation_score(&interleaved, plane_position_z);
        let contiguous_score = nd2_z_variation_score(&contiguous, plane_position_z);
        if contiguous_score > interleaved_score {
            return ("contiguous", contiguous, "z_position_metadata");
        }
    }

    if plane_position_z.iter().all(Option::is_none) {
        if let Some(layout) = nd2_xy_position_layout_from_loop_order(
            loop_descriptors,
            position_count,
            planes_per_position,
        ) {
            return if layout == "contiguous" {
                ("contiguous", contiguous, "xml_loop_order_outer_to_inner")
            } else {
                ("interleaved", interleaved, "xml_loop_order_outer_to_inner")
            };
        }
    }

    ("interleaved", interleaved, "default")
}

fn nd2_xy_position_layout_from_loop_order(
    loop_descriptors: &[Nd2LoopDescriptor],
    position_count: usize,
    planes_per_position: usize,
) -> Option<&'static str> {
    let xy_indices = loop_descriptors
        .iter()
        .enumerate()
        .filter(|(_, descriptor)| descriptor.kind == "XYPosLoop")
        .collect::<Vec<_>>();
    if xy_indices.len() != 1 {
        return None;
    }
    let (xy_index, xy_descriptor) = xy_indices[0];
    if xy_descriptor.count? as usize != position_count {
        return None;
    }

    let mut non_xy_product = 1usize;
    for descriptor in loop_descriptors
        .iter()
        .filter(|descriptor| descriptor.kind != "XYPosLoop")
    {
        let count = descriptor.count? as usize;
        if count == 0 {
            return None;
        }
        non_xy_product = non_xy_product.checked_mul(count)?;
    }
    if non_xy_product != planes_per_position {
        return None;
    }

    if xy_index == 0 {
        Some("contiguous")
    } else if xy_index + 1 == loop_descriptors.len() {
        Some("interleaved")
    } else {
        None
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
    series_image_chunks: Vec<Vec<usize>>,
    series_plane_offsets: Vec<usize>,
    series_source_planes: Vec<Vec<usize>>,
    old_jp2_planes: Vec<Vec<OldJp2Plane>>,
    // OME-parity metadata harvested from the LV binary metadata tree.
    physical_size: Option<f64>,
    physical_size_z: Option<f64>,
    channel_names: Vec<String>,
    emission_wavelengths: Vec<f64>,
    /// Excitation wavelengths from the primary metadata (Java handler.exWave).
    excitation_wavelengths: Vec<f64>,
    /// Backup-handler channel names / wavelengths recovered from the text
    /// annotation block (Java: backupHandler). Used only as a fallback when the
    /// primary metadata yields incomplete channel names or no wavelengths
    /// (ND2Reader.populateMetadataStore:2276-2277, 2493-2498).
    backup_channel_names: Vec<String>,
    backup_emission_wavelengths: Vec<f64>,
    backup_excitation_wavelengths: Vec<f64>,
    plane_delta_t: Vec<Option<f64>>,
    plane_position_z: Vec<Option<f64>>,
    // Data members mirroring the Java ND2Reader (see ND2Reader.java fields).
    /// dExposureTime per channel, seconds (Java: exposureTime).
    exposure_time: Vec<f64>,
    /// Channel name → packed BGR color (Java: channelColors).
    channel_colors: HashMap<String, i32>,
    /// Channel names harvested with a color (Java: textChannelNames).
    text_channel_names: Vec<String>,
    /// Per-effective-channel colors (Java: colors[]).
    colors: Vec<i32>,
    /// Stage positions per position, µm (Java: posX/posY/posZ).
    pos_x: Vec<f64>,
    pos_y: Vec<f64>,
    pos_z: Vec<f64>,
    /// Number of acquired XY positions (Java: positionCount).
    position_count: u32,
    /// Number of X fields (Java: nXFields).
    n_x_fields: u32,
    /// Objective numerical aperture / magnification / model (Java: lensNA,
    /// objectiveMag, objectiveModel).
    lens_na: Option<f64>,
    objective_mag: Option<f64>,
    objective_model: Option<String>,
    /// Objective-settings refractive index (Java: refractiveIndex).
    refractive_index: Option<f64>,
    /// Whether pixel data is losslessly compressed (Java: isLossless).
    is_lossless: bool,
    /// PFS focus / state offsets within the file (Java: pfsOffset/pfsStateOffset).
    pfs_offset: u64,
    pfs_state_offset: u64,
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
            excitation_wavelengths: Vec::new(),
            backup_channel_names: Vec::new(),
            backup_emission_wavelengths: Vec::new(),
            backup_excitation_wavelengths: Vec::new(),
            plane_delta_t: Vec::new(),
            plane_position_z: Vec::new(),
            exposure_time: Vec::new(),
            channel_colors: HashMap::new(),
            text_channel_names: Vec::new(),
            colors: Vec::new(),
            pos_x: Vec::new(),
            pos_y: Vec::new(),
            pos_z: Vec::new(),
            position_count: 0,
            n_x_fields: 0,
            lens_na: None,
            objective_mag: None,
            objective_model: None,
            refractive_index: None,
            is_lossless: false,
            pfs_offset: 0,
            pfs_state_offset: 0,
            image_chunks: Vec::new(),
            series_image_chunks: Vec::new(),
            series_plane_offsets: Vec::new(),
            series_source_planes: Vec::new(),
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
        self.series_image_chunks.clear();
        self.series_plane_offsets.clear();
        self.series_source_planes.clear();
        self.chunks.clear();
        self.plane_delta_t.clear();
        self.plane_position_z.clear();
        self.exposure_time.clear();
        self.channel_colors.clear();
        self.text_channel_names.clear();
        self.colors.clear();
        self.pos_x.clear();
        self.pos_y.clear();
        self.pos_z.clear();
        self.position_count = 0;
        self.n_x_fields = 0;
        self.lens_na = None;
        self.objective_mag = None;
        self.objective_model = None;
        self.refractive_index = None;
        self.is_lossless = false;
        self.pfs_offset = 0;
        self.pfs_state_offset = 0;
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
        let mut loop_size_z: Option<u32> = None;
        let mut loop_size_t: Option<u32> = None;
        let mut loop_series_count: Option<u32> = None;
        let mut loop_descriptors = Vec::new();

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
                nd2_update_loop_counts_from_xml(
                    &xml,
                    &mut loop_size_z,
                    &mut loop_size_t,
                    &mut loop_series_count,
                );
                nd2_update_loop_descriptors_from_xml(&xml, &mut loop_descriptors);
                break;
            }
        }

        for mc in chunks.iter().filter(|c| {
            c.name.starts_with("ImageMetadata") || c.name.contains("GrabberCameraSettings")
        }) {
            let data = read_chunk_data(&mut reader, mc).map_err(BioFormatsError::Io)?;
            let xml = String::from_utf8_lossy(&data);
            nd2_update_loop_counts_from_xml(
                &xml,
                &mut loop_size_z,
                &mut loop_size_t,
                &mut loop_series_count,
            );
            nd2_update_loop_descriptors_from_xml(&xml, &mut loop_descriptors);
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
        let mut indexed_image_chunks: Vec<(usize, usize)> = chunks
            .iter()
            .enumerate()
            .filter_map(|(i, c)| image_data_index(&c.name).map(|image_index| (image_index, i)))
            .collect();
        indexed_image_chunks.sort_by_key(|&(image_index, _)| image_index);
        let image_sequence_indices: Vec<usize> = indexed_image_chunks
            .iter()
            .map(|&(image_index, _)| image_index)
            .collect();
        let image_chunks: Vec<usize> = indexed_image_chunks
            .into_iter()
            .map(|(_, chunk_index)| chunk_index)
            .collect();

        let mut indexed_metadata_chunks: Vec<(usize, usize)> = chunks
            .iter()
            .enumerate()
            .filter_map(|(i, c)| {
                metadata_seq_index(&c.name).map(|metadata_index| (metadata_index, i))
            })
            .collect();
        indexed_metadata_chunks.sort_by_key(|&(metadata_index, _)| metadata_index);
        let metadata_sequence_indices: Vec<usize> = indexed_metadata_chunks
            .iter()
            .map(|&(metadata_index, _)| metadata_index)
            .collect();
        let metadata_chunks: Vec<usize> = indexed_metadata_chunks
            .into_iter()
            .map(|(_, chunk_index)| chunk_index)
            .collect();

        // Infer size_z from number of image chunks only when no loop metadata is
        // available. Common modern ND2 XML stores loop counts separately in
        // uiCount nodes with TimeLoop/ZStackLoop runtype attributes.
        if size_z == 1 && loop_size_z.is_none() && loop_size_t.is_none() && !image_chunks.is_empty()
        {
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
                let xml = String::from_utf8_lossy(&data);
                parse_nd2_xml_metadata(&xml, &mut lv);
                nd2_update_loop_counts_from_xml(
                    &xml,
                    &mut loop_size_z,
                    &mut loop_size_t,
                    &mut loop_series_count,
                );
                nd2_update_loop_descriptors_from_xml(&xml, &mut loop_descriptors);
            }
        }
        // Build the backup handler from the text-annotation blocks, mirroring
        // ND2Reader.parseText feeding `backupHandler` (java:2656-2674). Each
        // TextInfoItem string is parsed independently into a fresh value bag,
        // the equivalent of a separate ND2Handler. `backupHandler` is replaced
        // only while it is still unset or has zero channel names
        // (java:2670-2674), so the first text block with channel names wins.
        let mut backup = Nd2LvValues::default();
        for text in &lv.text_infos {
            let mut candidate = Nd2LvValues::default();
            parse_text(text, &mut candidate);
            if backup.channel_names.is_empty() {
                backup = candidate;
            }
        }
        self.backup_channel_names = backup.channel_names;
        self.backup_emission_wavelengths = backup.emission_wavelengths;
        self.backup_excitation_wavelengths = backup.excitation_wavelengths;

        self.physical_size = lv.calibration;
        self.physical_size_z = lv.z_step;
        self.channel_names = lv.channel_names;
        self.emission_wavelengths = lv.emission_wavelengths;
        self.excitation_wavelengths = lv.excitation_wavelengths;
        self.exposure_time = lv.exposure_time;
        self.channel_colors = lv.channel_colors;
        self.text_channel_names = lv.text_channel_names;
        self.pos_x = lv.pos_x;
        self.pos_y = lv.pos_y;
        self.pos_z = lv.pos_z;
        self.position_count = lv.position_count;
        self.lens_na = lv.lens_na;
        self.objective_mag = lv.objective_mag;
        self.objective_model = lv.objective_model;
        self.refractive_index = lv.refractive_index;
        self.is_lossless = lv.is_lossless;
        // ND2Reader caps an implausible field count to zero (>6 ⇒ 0).
        self.n_x_fields = if lv.n_x_fields > 6 { 0 } else { lv.n_x_fields };

        // PFS focus/state offsets come from the first two CustomData|P chunks
        // (ND2Reader:1121-1128). Use each chunk's payload start as the offset.
        self.pfs_offset = 0;
        self.pfs_state_offset = 0;
        for chunk in chunks.iter().filter(|c| c.name.starts_with("CustomData|P")) {
            if self.pfs_offset == 0 {
                self.pfs_offset = chunk.data_offset;
            } else if self.pfs_state_offset == 0 {
                self.pfs_state_offset = chunk.data_offset;
                break;
            }
        }

        // Per-effective-channel colors: look each channel name up in the
        // channelColors map (ND2Reader.populateMetadataStore:2271-2288). Names
        // come from sDescription, falling back to the backup handler and then
        // textChannelNames, matching the channelNames fallback chain there.
        let color_names: &[String] = if self.channel_names.len() < size_c as usize
            && !self.backup_channel_names.is_empty()
        {
            &self.backup_channel_names
        } else {
            &self.channel_names
        };
        let color_names: &[String] = if color_names.len() < size_c as usize {
            &self.text_channel_names
        } else {
            color_names
        };
        self.colors = (0..size_c as usize)
            .map(|c| {
                color_names
                    .get(c)
                    .and_then(|name| self.channel_colors.get(name))
                    .copied()
                    .unwrap_or(0)
            })
            .collect();

        // Dimension order: Java ND2Reader builds "XY" + order, then appends any
        // of Z/C/T not already present. With no acquisition-loop order and a
        // single channel this yields XYZCT (see ND2Reader ~1530, ~2014).
        let dimension_order = if size_c > 1 {
            DimensionOrder::XYCZT
        } else {
            DimensionOrder::XYZCT
        };

        let image_count = image_chunks.len() as u32;
        let position_count = loop_series_count.filter(|&count| count > 1).unwrap_or(1);
        let mut size_t = 1u32;
        if let Some(z) = loop_size_z {
            size_z = z.max(1);
        }
        if let Some(t) = loop_size_t {
            size_t = t.max(1);
        }
        let expected_planes = size_z
            .saturating_mul(size_t)
            .saturating_mul(position_count.max(1));
        if image_count > 0 && expected_planes != image_count {
            if size_t > 1 && image_count % size_t == 0 {
                size_z = (image_count / size_t).max(1);
            } else if size_z > 1 && image_count % size_z == 0 {
                size_t = (image_count / size_z).max(1);
            } else if loop_size_z.is_some() || loop_size_t.is_some() {
                size_z = 1;
                size_t = image_count.max(1);
            }
        }
        let mut series_metadata: HashMap<String, MetadataValue> = HashMap::new();
        series_metadata.insert("nd2_chunks".into(), MetadataValue::Int(chunks.len() as i64));
        series_metadata.insert(
            "nd2_image_data_chunks".into(),
            MetadataValue::Int(image_chunks.len() as i64),
        );
        let mut plane_delta_t = vec![None; image_count as usize];
        let mut plane_position_z = vec![None; image_count as usize];
        if let Some(z) = loop_size_z {
            series_metadata.insert("nd2_loop_size_z".into(), MetadataValue::Int(z as i64));
        }
        if let Some(t) = loop_size_t {
            series_metadata.insert("nd2_loop_size_t".into(), MetadataValue::Int(t as i64));
        }
        if let Some(series_count) = loop_series_count {
            series_metadata.insert(
                "nd2_loop_series_count".into(),
                MetadataValue::Int(series_count as i64),
            );
        }
        if !loop_descriptors.is_empty() {
            series_metadata.insert(
                "nd2_loop_order".into(),
                MetadataValue::String(
                    loop_descriptors
                        .iter()
                        .map(|descriptor| descriptor.kind)
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );
            let counts = loop_descriptors
                .iter()
                .filter_map(|descriptor| {
                    descriptor
                        .count
                        .map(|count| format!("{}={}", descriptor.kind, count))
                })
                .collect::<Vec<_>>();
            if !counts.is_empty() {
                series_metadata.insert(
                    "nd2_loop_count_evidence".into(),
                    MetadataValue::String(counts.join(",")),
                );
            }
        }
        if !image_sequence_indices.is_empty() {
            series_metadata.insert(
                "nd2_image_data_sequence_indices".into(),
                MetadataValue::String(
                    image_sequence_indices
                        .iter()
                        .map(|index| index.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );
            series_metadata.insert(
                "nd2_image_data_chunk_lengths".into(),
                MetadataValue::String(
                    image_chunks
                        .iter()
                        .map(|&chunk_index| chunks[chunk_index].data_length.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );

            let mut image_data_encodings = Vec::with_capacity(image_chunks.len());
            let mut image_data_payload_offsets = Vec::with_capacity(image_chunks.len());
            let mut image_data_chunk_tables = Vec::new();
            let mut image_data_chunk_table_ranges = Vec::new();
            let mut image_data_timestamps = Vec::new();
            for (plane, &chunk_index) in image_chunks.iter().enumerate() {
                let chunk = &chunks[chunk_index];
                let stored_expected =
                    stored_expected_for_nd2_frame(size_x, size_y, size_c, pixel_type);
                if let Ok(prefix) = read_chunk_prefix(&mut reader, chunk, 8192) {
                    let (encoding, payload_offset) = nd2_frame_payload_layout(
                        &prefix,
                        chunk.data_length as usize,
                        stored_expected,
                    );
                    image_data_encodings.push(encoding.to_string());
                    image_data_payload_offsets.push(payload_offset.to_string());
                    if let Some((_, table)) = nd2_chunk_table_payload_encoding(
                        &prefix,
                        chunk.data_length as usize,
                        stored_expected,
                    ) {
                        image_data_chunk_tables.push(format!(
                            "plane={plane}:offset={},entry_width={},count={},first_payload={},payload_bytes={}",
                            table.table_offset,
                            table.entry_width,
                            table.chunk_count,
                            table.first_payload_offset,
                            table.total_payload_len
                        ));
                        image_data_chunk_table_ranges.push(format!(
                            "plane={plane}:{}",
                            table
                                .ranges
                                .iter()
                                .map(|&(start, end)| format!("{start}..{end}"))
                                .collect::<Vec<_>>()
                                .join(",")
                        ));
                    }
                    if let Some(timestamp) = nd2_prefix_timestamp_seconds(&prefix, payload_offset) {
                        image_data_timestamps.push(timestamp.to_string());
                        if let Some(slot) = plane_delta_t.get_mut(plane) {
                            *slot = Some(timestamp);
                        }
                    }
                }
            }
            if !image_data_encodings.is_empty() {
                series_metadata.insert(
                    "nd2_image_data_encodings".into(),
                    MetadataValue::String(image_data_encodings.join(",")),
                );
                series_metadata.insert(
                    "nd2_image_data_payload_offsets".into(),
                    MetadataValue::String(image_data_payload_offsets.join(",")),
                );
                if image_data_timestamps.len() == image_data_encodings.len() {
                    series_metadata.insert(
                        "nd2_image_data_timestamps".into(),
                        MetadataValue::String(image_data_timestamps.join(",")),
                    );
                }
                if !image_data_chunk_tables.is_empty() {
                    series_metadata.insert(
                        "nd2_image_data_chunk_tables".into(),
                        MetadataValue::String(image_data_chunk_tables.join(";")),
                    );
                }
                if !image_data_chunk_table_ranges.is_empty() {
                    series_metadata.insert(
                        "nd2_image_data_chunk_table_ranges".into(),
                        MetadataValue::String(image_data_chunk_table_ranges.join(";")),
                    );
                }
            }

            if let Some(&first_chunk_index) = image_chunks.first() {
                let first_chunk = &chunks[first_chunk_index];
                let stored_expected =
                    stored_expected_for_nd2_frame(size_x, size_y, size_c, pixel_type);
                if let Ok(data) = read_chunk_data(&mut reader, first_chunk) {
                    series_metadata.insert(
                        "nd2_first_image_data_encoding".into(),
                        MetadataValue::String(
                            nd2_frame_payload_hint(&data, stored_expected).to_string(),
                        ),
                    );
                }
            }
        }
        if !metadata_sequence_indices.is_empty() {
            series_metadata.insert(
                "nd2_image_metadata_seq_chunks".into(),
                MetadataValue::Int(metadata_chunks.len() as i64),
            );
            series_metadata.insert(
                "nd2_image_metadata_seq_indices".into(),
                MetadataValue::String(
                    metadata_sequence_indices
                        .iter()
                        .map(|index| index.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );
            series_metadata.insert(
                "nd2_image_metadata_seq_chunk_lengths".into(),
                MetadataValue::String(
                    metadata_chunks
                        .iter()
                        .map(|&chunk_index| chunks[chunk_index].data_length.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );
            series_metadata.insert(
                "nd2_image_metadata_seq_matches_images".into(),
                MetadataValue::Bool(metadata_sequence_indices == image_sequence_indices),
            );
            let mut metadata_timestamps = Vec::with_capacity(metadata_chunks.len());
            for (ordinal, &chunk_index) in metadata_chunks.iter().enumerate() {
                let chunk = &chunks[chunk_index];
                if let Ok(data) = read_chunk_data(&mut reader, chunk) {
                    let xml = String::from_utf8_lossy(&data);
                    let plane = metadata_sequence_indices
                        .get(ordinal)
                        .copied()
                        .unwrap_or(ordinal);
                    if let Some(timestamp) = nd2_xml_plane_timestamp_seconds(&xml) {
                        metadata_timestamps.push(timestamp.to_string());
                        if let Some(slot) = plane_delta_t.get_mut(plane) {
                            if slot.is_none() {
                                *slot = Some(timestamp);
                            }
                        }
                    }
                    if let Some(z) = nd2_xml_plane_z_position(&xml) {
                        if let Some(slot) = plane_position_z.get_mut(plane) {
                            *slot = Some(z);
                        }
                    }
                }
            }
            if metadata_timestamps.len() == metadata_chunks.len() {
                series_metadata.insert(
                    "nd2_image_metadata_seq_timestamps".into(),
                    MetadataValue::String(metadata_timestamps.join(",")),
                );
            }
        }
        let mut series_image_chunks = vec![image_chunks.clone()];
        let mut series_plane_offsets = vec![0usize];
        let mut series_source_planes = vec![(0..image_chunks.len()).collect::<Vec<_>>()];
        let mut series_count = 1usize;
        let mut series_image_count = image_count.max(1);
        let mut series_size_z = size_z;
        let mut series_size_t = size_t;
        let mut series_handling = "single_series";

        if let Some(position_count) = loop_series_count.filter(|&count| count > 1) {
            let position_count = position_count as usize;
            if image_count as usize == position_count {
                // Java exposes simple XY-position loops as separate series. The
                // general ImageDataSeq mapping is index/dimension-order based;
                // only split the unambiguous one-frame-per-position case here.
                series_count = position_count;
                series_image_count = 1;
                series_size_z = 1;
                series_size_t = 1;
                series_image_chunks = image_chunks.iter().map(|&chunk| vec![chunk]).collect();
                series_plane_offsets = (0..position_count).collect();
                series_source_planes = (0..position_count).map(|plane| vec![plane]).collect();
                series_handling = "split_xy_positions_one_plane_each";
            } else if image_count as usize % position_count == 0 {
                let planes_per_position = image_count as usize / position_count;
                let expected_planes_per_position = size_z as usize * size_t as usize;
                if expected_planes_per_position == planes_per_position {
                    let (layout, source_planes, layout_source) = nd2_choose_xy_position_layout(
                        position_count,
                        planes_per_position,
                        size_z,
                        &plane_position_z,
                        &loop_descriptors,
                    );
                    series_count = position_count;
                    series_image_count = planes_per_position as u32;
                    series_source_planes = source_planes;
                    series_image_chunks = (0..position_count)
                        .map(|series| {
                            series_source_planes[series]
                                .iter()
                                .map(|&plane| image_chunks[plane])
                                .collect::<Vec<_>>()
                        })
                        .collect();
                    series_plane_offsets = series_source_planes
                        .iter()
                        .map(|planes| planes.first().copied().unwrap_or(0))
                        .collect();
                    series_metadata.insert(
                        "nd2_loop_series_candidate_layouts".into(),
                        MetadataValue::String("interleaved,contiguous".into()),
                    );
                    series_metadata.insert(
                        "nd2_loop_series_assumed_layout".into(),
                        MetadataValue::String(layout.into()),
                    );
                    series_metadata.insert(
                        "nd2_loop_series_layout_source".into(),
                        MetadataValue::String(layout_source.into()),
                    );
                    series_handling = if layout == "contiguous" {
                        "split_xy_positions_contiguous_full_series"
                    } else {
                        "split_xy_positions_interleaved_full_series"
                    };
                } else {
                    series_handling = "unsupported_multi_position_layout_kept_flat";
                }
            } else if image_count > 0 {
                series_handling = "unsupported_multi_position_layout_kept_flat";
            }
        }

        series_metadata.insert(
            "nd2_loop_series_handling".into(),
            MetadataValue::String(series_handling.to_string()),
        );

        // Surface the newly captured Java data members. These mirror the values
        // ND2Reader stores in its global/series metadata table.
        series_metadata.insert(
            "nd2_is_lossless".into(),
            MetadataValue::Bool(self.is_lossless),
        );
        series_metadata.insert(
            "nd2_position_count".into(),
            MetadataValue::Int(self.position_count as i64),
        );
        series_metadata.insert(
            "nd2_x_fields".into(),
            MetadataValue::Int(self.n_x_fields as i64),
        );
        if self.pfs_offset != 0 {
            series_metadata.insert(
                "nd2_pfs_offset".into(),
                MetadataValue::Int(self.pfs_offset as i64),
            );
        }
        if self.pfs_state_offset != 0 {
            series_metadata.insert(
                "nd2_pfs_state_offset".into(),
                MetadataValue::Int(self.pfs_state_offset as i64),
            );
        }
        if let Some(ri) = self.refractive_index {
            series_metadata.insert(
                "nd2_refractive_index".into(),
                MetadataValue::Float(ri),
            );
        }
        if let Some(na) = self.lens_na {
            series_metadata.insert("nd2_objective_na".into(), MetadataValue::Float(na));
        }
        if let Some(mag) = self.objective_mag {
            series_metadata.insert(
                "nd2_objective_magnification".into(),
                MetadataValue::Float(mag),
            );
        }
        if let Some(model) = &self.objective_model {
            series_metadata.insert(
                "nd2_objective_model".into(),
                MetadataValue::String(model.clone()),
            );
        }
        if !self.exposure_time.is_empty() {
            series_metadata.insert(
                "nd2_exposure_times".into(),
                MetadataValue::String(
                    self.exposure_time
                        .iter()
                        .map(|t| t.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );
        }
        if !self.colors.is_empty() {
            series_metadata.insert(
                "nd2_channel_colors".into(),
                MetadataValue::String(
                    self.colors
                        .iter()
                        .map(|c| c.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );
        }

        let mut metas = Vec::with_capacity(series_count);
        for series_index in 0..series_count {
            let mut md = series_metadata.clone();
            if series_count > 1 {
                md.insert(
                    "nd2_series_index".into(),
                    MetadataValue::Int(series_index as i64),
                );
                if let Some(source_planes) = series_source_planes.get(series_index) {
                    md.insert(
                        "nd2_series_source_planes".into(),
                        MetadataValue::String(
                            source_planes
                                .iter()
                                .map(|plane| plane.to_string())
                                .collect::<Vec<_>>()
                                .join(","),
                        ),
                    );
                }
            }
            metas.push(ImageMetadata {
                size_x,
                size_y,
                size_z: series_size_z,
                size_c,
                size_t: series_size_t,
                pixel_type,
                bits_per_pixel: bpp,
                image_count: series_image_count,
                dimension_order,
                is_rgb: size_c == 3,
                is_interleaved: true,
                is_indexed: false,
                is_little_endian: true,
                resolution_count: 1,
                series_metadata: md,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
        }

        self.meta = metas;
        self.current_series = 0;
        self.old_jp2_planes.clear();
        self.plane_delta_t = plane_delta_t;
        self.plane_position_z = plane_position_z;
        self.series_image_chunks = series_image_chunks;
        self.series_plane_offsets = series_plane_offsets;
        self.series_source_planes = series_source_planes;
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
        self.series_image_chunks.clear();
        self.series_plane_offsets.clear();
        self.series_source_planes.clear();
        self.old_jp2_planes.clear();
        self.physical_size = None;
        self.physical_size_z = None;
        self.channel_names.clear();
        self.emission_wavelengths.clear();
        self.excitation_wavelengths.clear();
        self.backup_channel_names.clear();
        self.backup_emission_wavelengths.clear();
        self.backup_excitation_wavelengths.clear();
        self.plane_delta_t.clear();
        self.plane_position_z.clear();
        self.exposure_time.clear();
        self.channel_colors.clear();
        self.text_channel_names.clear();
        self.colors.clear();
        self.pos_x.clear();
        self.pos_y.clear();
        self.pos_z.clear();
        self.position_count = 0;
        self.n_x_fields = 0;
        self.lens_na = None;
        self.objective_mag = None;
        self.objective_model = None;
        self.refractive_index = None;
        self.is_lossless = false;
        self.pfs_offset = 0;
        self.pfs_state_offset = 0;
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

        let series_chunks = self
            .series_image_chunks
            .get(self.current_series)
            .unwrap_or(&self.image_chunks);
        let chunk_idx = series_chunks
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

        let chunk_context = format!(
            "plane {plane_index}: {} at offset {} length {}",
            chunk.name, chunk.data_offset, chunk.data_length
        );
        let decoded = decode_nd2_frame_payload(&data, stored_expected).map_err(|e| match e {
            BioFormatsError::Format(msg) => {
                BioFormatsError::Format(format!("ND2: {chunk_context}: {msg}"))
            }
            BioFormatsError::UnsupportedFormat(msg) => {
                BioFormatsError::UnsupportedFormat(format!("ND2: {chunk_context}: {msg}"))
            }
            BioFormatsError::Codec(msg) => {
                BioFormatsError::Codec(format!("ND2: {chunk_context}: {msg}"))
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
        use crate::common::ome_metadata::{OmeInstrument, OmeObjective, OmeMetadata, OmePlane};
        let meta = self.meta.get(self.current_series)?;
        let mut ome = OmeMetadata::from_image_metadata(meta);

        // Objective (lensNA / objectiveMag / objectiveModel) → OME Objective,
        // mirroring ND2Reader.populateMetadataStore:2569-2585.
        if self.lens_na.is_some() || self.objective_mag.is_some() || self.objective_model.is_some() {
            let instrument = OmeInstrument {
                objectives: vec![OmeObjective {
                    calibrated_magnification: self.objective_mag,
                    lens_na: self.lens_na,
                    model: self.objective_model.clone(),
                    ..Default::default()
                }],
                ..Default::default()
            };
            ome.instruments.push(instrument);
            if let Some(img) = ome.images.get_mut(0) {
                img.instrument_ref = Some(0);
                img.objective_ref = Some(0);
            }
        }

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

        // Channel names, emission wavelengths and colors. The effective channel
        // count is the per-series channel count.
        let effective_size_c = img.channels.len();

        // Channel-name fallback chain (ND2Reader.populateMetadataStore:2275-2281):
        // primary channel names; if fewer than effectiveSizeC and a backup
        // handler exists, use the backup's; if still short, use textChannelNames.
        let channel_names: &[String] = if self.channel_names.len() < effective_size_c
            && !self.backup_channel_names.is_empty()
        {
            &self.backup_channel_names
        } else {
            &self.channel_names
        };
        let channel_names: &[String] = if channel_names.len() < effective_size_c {
            &self.text_channel_names
        } else {
            channel_names
        };

        // Wavelength fallback (ND2Reader.populateMetadataStore:2493-2499): use the
        // backup handler only when the primary list is empty.
        let emission_wavelengths: &[f64] = if self.emission_wavelengths.is_empty() {
            &self.backup_emission_wavelengths
        } else {
            &self.emission_wavelengths
        };
        let excitation_wavelengths: &[f64] = if self.excitation_wavelengths.is_empty() {
            &self.backup_excitation_wavelengths
        } else {
            &self.excitation_wavelengths
        };

        for (c, channel) in img.channels.iter_mut().enumerate() {
            if let Some(name) = channel_names.get(c) {
                channel.name = Some(name.clone());
            }
            if let Some(em) = emission_wavelengths.get(c).filter(|v| **v > 0.0) {
                channel.emission_wavelength = Some(*em);
            }
            if let Some(ex) = excitation_wavelengths.get(c).filter(|v| **v > 0.0) {
                channel.excitation_wavelength = Some(*ex);
            }
            // Java sets the channel color only when the recorded BGR color is
            // non-black (populateMetadataStore:2303-2313), packing it as RGBA.
            if let Some(&packed) = self.colors.get(c).filter(|&&c| c != 0) {
                let red = packed & 0xff;
                let green = (packed >> 8) & 0xff;
                let blue = (packed >> 16) & 0xff;
                channel.color = Some((red << 24) | (green << 16) | (blue << 8) | 0xff);
            }
        }

        // Per-position stage coordinates for this series. Java indexes posX/Y/Z
        // by acquisition position; here each split series is one XY position, so
        // the series index selects the position (falling back to index 0 when a
        // single list applies to all planes).
        let series = self.current_series;
        let series_count = self.meta.len().max(1);
        let pos_index = |list: &[f64]| -> Option<f64> {
            if list.is_empty() {
                None
            } else if list.len() == series_count {
                list.get(series).copied()
            } else {
                list.first().copied()
            }
        };
        let plane_pos_x = pos_index(&self.pos_x);
        let plane_pos_y = pos_index(&self.pos_y);
        let plane_pos_z_value = pos_index(&self.pos_z);
        // A single shared exposure time applies to every plane (Java: index 0
        // when exposureTime.size() == 1, populateMetadataStore:2423-2426).
        let shared_exposure = (self.exposure_time.len() == 1)
            .then(|| self.exposure_time[0])
            .filter(|t| *t > 0.0);

        if self.plane_delta_t.iter().any(Option::is_some)
            || self.plane_position_z.iter().any(Option::is_some)
            || plane_pos_x.is_some()
            || plane_pos_y.is_some()
            || plane_pos_z_value.is_some()
            || !self.exposure_time.is_empty()
        {
            // This reader treats uiComp samples as interleaved within each
            // ImageDataSeq frame, so one chunk maps to one Z/T plane.
            let effective_c = 1;
            let plane_offset = self
                .series_plane_offsets
                .get(self.current_series)
                .copied()
                .unwrap_or(0);
            let source_planes = self.series_source_planes.get(self.current_series);
            img.planes = (0..meta.image_count)
                .map(|i| {
                    let c = i % effective_c;
                    let z = (i / effective_c) % meta.size_z.max(1);
                    let t = i / (effective_c * meta.size_z.max(1));
                    let source_plane = source_planes
                        .and_then(|planes| planes.get(i as usize).copied())
                        .unwrap_or(plane_offset + i as usize);
                    // Per-channel exposure when the list matches sizeC, else the
                    // shared single value (ND2Reader:2419-2430).
                    let exposure_time = if self.exposure_time.len() == meta.size_c as usize {
                        self.exposure_time
                            .get((i % meta.size_c.max(1)) as usize)
                            .copied()
                            .filter(|t| *t > 0.0)
                    } else {
                        shared_exposure
                    };
                    OmePlane {
                        the_z: z,
                        the_c: c,
                        the_t: t,
                        delta_t: self.plane_delta_t.get(source_plane).copied().flatten(),
                        position_x: plane_pos_x,
                        position_y: plane_pos_y,
                        position_z: self
                            .plane_position_z
                            .get(source_plane)
                            .copied()
                            .flatten()
                            .or(plane_pos_z_value),
                        exposure_time,
                    }
                })
                .collect();
        }

        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk_table_frame(ranges: &[(u32, &[u8])]) -> Vec<u8> {
        let mut frame = Vec::new();
        frame.extend_from_slice(&(ranges.len() as u32).to_le_bytes());
        for &(offset, payload) in ranges {
            frame.extend_from_slice(&offset.to_le_bytes());
            frame.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        }

        for &(offset, payload) in ranges {
            let offset = offset as usize;
            if frame.len() < offset {
                frame.resize(offset, 0);
            }
            frame.extend_from_slice(payload);
        }
        frame
    }

    #[test]
    fn nd2_decodes_zlib_stream_split_by_chunk_table() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&[17, 23, 31, 47]).unwrap();
        let compressed = encoder.finish().unwrap();
        let split = compressed.len() / 2;
        let second_offset = 20 + split as u32 + 4;
        let frame = chunk_table_frame(&[
            (20, &compressed[..split]),
            (second_offset, &compressed[split..]),
        ]);

        let (encoding, payload_offset) = nd2_frame_payload_layout(&frame, frame.len(), 4);
        assert_eq!(encoding, "chunk_table_le32_zlib");
        assert_eq!(payload_offset, 0);
        assert_eq!(
            decode_nd2_frame_payload(&frame, 4).unwrap(),
            vec![17, 23, 31, 47]
        );
    }

    #[test]
    fn nd2_records_jpeg2000_stream_split_by_chunk_table() {
        let jp2 = [0xff, 0x4f, 0xff, 0x51, 0, 0, 0, 0];
        let frame = chunk_table_frame(&[(20, &jp2[..4]), (28, &jp2[4..])]);

        let (encoding, payload_offset) = nd2_frame_payload_layout(&frame, frame.len(), 4);
        assert_eq!(encoding, "chunk_table_le32_jpeg2000");
        assert_eq!(payload_offset, 0);
    }

    #[test]
    fn nd2_xml_captures_objective_refractive_and_lossless() {
        let xml = r#"<root>
          <dObjectiveMag>40</dObjectiveMag>
          <dObjectiveNA>0.95</dObjectiveNA>
          <dRefractIndex1>1.515</dRefractIndex1>
          <sObjective value="Plan Apo 40x"/>
          <dCompressionParam>3</dCompressionParam>
          <iXFields>2</iXFields>
        </root>"#;
        let mut lv = Nd2LvValues::default();
        parse_nd2_xml_metadata(xml, &mut lv);
        assert_eq!(lv.objective_mag, Some(40.0));
        assert_eq!(lv.lens_na, Some(0.95));
        assert_eq!(lv.refractive_index, Some(1.515));
        assert_eq!(lv.objective_model.as_deref(), Some("Plan Apo 40x"));
        assert!(lv.is_lossless);
        assert_eq!(lv.n_x_fields, 2);
    }

    #[test]
    fn nd2_xml_captures_exposure_and_position_lists() {
        let xml = r#"<root>
          <dExposureTime>50</dExposureTime>
          <dExposureTime>100</dExposureTime>
          <dPosX><item_0 value="100.0"/><item_1 value="200.0"/></dPosX>
          <dPosY><item_0 value="10.0"/><item_1 value="20.0"/></dPosY>
          <dPosZ><item_0>1.0</item_0><item_1>2.0</item_1></dPosZ>
        </root>"#;
        let mut lv = Nd2LvValues::default();
        parse_nd2_xml_metadata(xml, &mut lv);
        // ms → s conversion.
        assert_eq!(lv.exposure_time, vec![0.05, 0.1]);
        assert_eq!(lv.pos_x, vec![100.0, 200.0]);
        assert_eq!(lv.pos_y, vec![10.0, 20.0]);
        assert_eq!(lv.pos_z, vec![1.0, 2.0]);
        assert_eq!(lv.position_count, 2);
    }

    #[test]
    fn nd2_binary_lv_pairs_color_with_channel_and_collects_exposure() {
        // Build a minimal LV stream: uiColor (uint32) then sDescription (string),
        // then dExposureTime (double). Layout per parse_nd2_lv:
        //   [type:u8][nameLen:u8][name UTF-16LE][value].
        fn entry(ty: u8, name: &str, value: &[u8]) -> Vec<u8> {
            let mut e = vec![ty, name.chars().count() as u8];
            for u in name.encode_utf16() {
                e.extend_from_slice(&u.to_le_bytes());
            }
            e.extend_from_slice(value);
            e
        }
        let mut data = Vec::new();
        // uiColor = 0x0000FF (red in BGR) as uint32.
        data.extend_from_slice(&entry(3, "uiColor", &0x0000FFu32.to_le_bytes()));
        // sDescription = "DAPI" (null-terminated UTF-16LE).
        let mut desc = Vec::new();
        for u in "DAPI".encode_utf16() {
            desc.extend_from_slice(&u.to_le_bytes());
        }
        desc.extend_from_slice(&0u16.to_le_bytes());
        data.extend_from_slice(&entry(8, "sDescription", &desc));
        // dExposureTime = 25.0 ms.
        data.extend_from_slice(&entry(6, "dExposureTime", &25.0f64.to_bits().to_le_bytes()));
        // dPosX = 1.0 (double) → positionCount++.
        data.extend_from_slice(&entry(6, "dPosX", &1.0f64.to_bits().to_le_bytes()));

        let mut lv = Nd2LvValues::default();
        parse_nd2_lv(&data, &mut lv);
        assert_eq!(lv.channel_names, vec!["DAPI".to_string()]);
        assert_eq!(lv.text_channel_names, vec!["DAPI".to_string()]);
        assert_eq!(lv.channel_colors.get("DAPI"), Some(&0x0000FF));
        assert_eq!(lv.exposure_time, vec![0.025]);
        assert_eq!(lv.position_count, 1);
    }

    #[test]
    fn nd2_parse_text_recovers_channel_names_and_wavelengths() {
        // Line-based text annotation (ND2Reader.parseText catch fallback →
        // ND2Handler.parseKeyAndValue). "Name" supplies channel names; the
        // emission/excitation keys supply wavelengths (first token parsed).
        let text = "Metadata:\n\
                    Name: DAPI\n\
                    Emission wavelength: 461 nm\n\
                    Excitation wavelength: 358 nm\n\
                    Name: FITC\n\
                    Emission wavelength: 519 nm\n\
                    Excitation wavelength: 495 nm\n";
        let mut backup = Nd2LvValues::default();
        parse_text(text, &mut backup);
        assert_eq!(
            backup.channel_names,
            vec!["DAPI".to_string(), "FITC".to_string()]
        );
        assert_eq!(backup.emission_wavelengths, vec![461.0, 519.0]);
        assert_eq!(backup.excitation_wavelengths, vec![358.0, 495.0]);
    }

    #[test]
    fn nd2_lv_collects_textinfo_for_backup_handler() {
        // A TextInfoItem* string in the LV tree must be captured into text_infos
        // so it can later seed the backup handler (ND2Reader.iterateIn:2130-2133).
        fn entry(ty: u8, name: &str, value: &[u8]) -> Vec<u8> {
            let mut e = vec![ty, name.chars().count() as u8];
            for u in name.encode_utf16() {
                e.extend_from_slice(&u.to_le_bytes());
            }
            e.extend_from_slice(value);
            e
        }
        let mut info = Vec::new();
        for u in "Name: TexasRed".encode_utf16() {
            info.extend_from_slice(&u.to_le_bytes());
        }
        info.extend_from_slice(&0u16.to_le_bytes());

        let data = entry(8, "TextInfoItem_5", &info);
        let mut lv = Nd2LvValues::default();
        parse_nd2_lv(&data, &mut lv);
        assert_eq!(lv.text_infos, vec!["Name: TexasRed".to_string()]);
        // The primary LV channel names stay empty (no sDescription/uiColor pair).
        assert!(lv.channel_names.is_empty());

        // Feeding the collected text through parse_text recovers the channel name.
        let mut backup = Nd2LvValues::default();
        for t in &lv.text_infos {
            parse_text(t, &mut backup);
        }
        assert_eq!(backup.channel_names, vec!["TexasRed".to_string()]);
    }

    #[test]
    fn nd2_decodes_per_chunk_zlib_chunk_table_layout() {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        let compress = |value: u8| {
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            encoder.write_all(&[value]).unwrap();
            encoder.finish().unwrap()
        };
        let first = compress(17);
        let second = compress(23);
        let second_offset = 20 + first.len() as u32 + 4;
        let frame = chunk_table_frame(&[(20, &first), (second_offset, &second)]);

        let (encoding, payload_offset) = nd2_frame_payload_layout(&frame, frame.len(), 2);
        assert_eq!(encoding, "chunk_table_le32_per_chunk_zlib");
        assert_eq!(payload_offset, 0);
        assert_eq!(decode_nd2_frame_payload(&frame, 2).unwrap(), vec![17, 23]);
    }
}
