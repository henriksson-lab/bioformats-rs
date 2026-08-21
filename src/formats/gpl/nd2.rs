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

use crate::common::compressed::{
    mode_allowed, CompressedBytes, CompressedExtractionSupport, CompressedLevelInfo,
    CompressedTile, CompressedTileMode, Jpeg2000Container, LossyCodec,
};
use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::{crop_full_plane, validate_region};

/// ND2 file magic bytes.
pub const ND2_MAGIC: [u8; 4] = [0xDA, 0xCE, 0xBE, 0x0A];
const ND2_EAGER_PLANE_METADATA_LIMIT: usize = 128;

#[derive(Debug, Clone)]
struct Nd2Chunk {
    name: String,
    block_offset: u64,
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
            block_offset: chunk_start,
            data_offset,
            data_length: data_len,
        });

        // Advance past data
        f.seek(SeekFrom::Start(data_end))?;
    }
    Ok(chunks)
}

fn scan_chunks_before(f: &mut BufReader<File>, stop: u64) -> std::io::Result<Vec<Nd2Chunk>> {
    let mut chunks = Vec::new();
    let file_len = f.get_ref().metadata()?.len();
    let stop = stop.min(file_len);
    let mut search_pos = 0u64;
    let mut buf = vec![0u8; 1024 * 1024];

    while search_pos + 16 <= stop {
        f.seek(SeekFrom::Start(search_pos))?;
        let to_read = ((stop - search_pos).min(buf.len() as u64)) as usize;
        if to_read < ND2_MAGIC.len() {
            break;
        }
        let n = f.read(&mut buf[..to_read])?;
        if n < ND2_MAGIC.len() {
            break;
        }

        let Some(found) = buf[..n]
            .windows(ND2_MAGIC.len())
            .position(|window| window == ND2_MAGIC)
        else {
            search_pos += (n - (ND2_MAGIC.len() - 1)) as u64;
            continue;
        };

        let chunk_start = search_pos + found as u64;
        if chunk_start + 16 > stop {
            break;
        }
        f.seek(SeekFrom::Start(chunk_start))?;

        let mut magic = [0u8; 4];
        if f.read_exact(&mut magic).is_err() {
            break;
        }
        if magic != ND2_MAGIC {
            search_pos = chunk_start + 1;
            continue;
        }

        let mut name_len_bytes = [0u8; 4];
        f.read_exact(&mut name_len_bytes)?;
        let name_len = u32::from_le_bytes(name_len_bytes) as usize;
        if name_len == 0 || name_len > 4096 {
            search_pos = chunk_start + 1;
            continue;
        }

        let mut data_len_bytes = [0u8; 8];
        f.read_exact(&mut data_len_bytes)?;
        let data_len = u64::from_le_bytes(data_len_bytes);
        let data_offset = chunk_start + 16 + name_len as u64;
        let Some(data_end) = data_offset.checked_add(data_len) else {
            search_pos = chunk_start + 1;
            continue;
        };
        if data_end > file_len || data_offset > stop {
            break;
        }

        let mut name_bytes = vec![0u8; name_len];
        f.read_exact(&mut name_bytes)?;
        let name = String::from_utf8_lossy(&name_bytes)
            .trim_end_matches('\0')
            .to_string();
        if !name.ends_with('!') {
            search_pos = chunk_start + 1;
            continue;
        }

        chunks.push(Nd2Chunk {
            name,
            block_offset: chunk_start,
            data_offset,
            data_length: data_len,
        });

        search_pos = data_end;
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
    let mut raw_entries = Vec::new();
    let mut pos = entries_offset;

    while pos + 1 + 16 <= entries_end {
        let mut name_bytes = Vec::new();
        loop {
            if pos >= entries_end {
                return Ok(None);
            }
            let mut b = [0u8; 1];
            f.read_exact(&mut b)?;
            pos += 1;
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
        pos += 16;
        let position = u64::from_le_bytes(position_bytes);
        let length = u64::from_le_bytes(length_bytes);
        if position + length > file_len || position + 16 > file_len {
            return Ok(None);
        }
        raw_entries.push((name, position));
    }

    let mut chunks = Vec::with_capacity(raw_entries.len());
    let mut image_count = 0usize;
    let mut max_image_index: Option<usize> = None;

    for (name, position) in raw_entries {
        let image_index = image_data_index(&name);
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

        if let Some(index) = image_index {
            image_count += 1;
            max_image_index = Some(max_image_index.map_or(index, |m| m.max(index)));
        }
        chunks.push(Nd2Chunk {
            name: format!("{name}!"),
            block_offset: position,
            data_offset,
            data_length: actual_data_len,
        });
    }

    if let Some(max_index) = max_image_index {
        if image_count != max_index + 1 {
            return Ok(None);
        }
    }

    if let Some(first_image_offset) = chunks
        .iter()
        .filter(|c| c.name.starts_with("ImageDataSeq"))
        .map(|c| c.block_offset)
        .min()
    {
        let prefix_scan_stop = first_image_offset.min(4 * 1024 * 1024);
        for prefix_chunk in scan_chunks_before(f, prefix_scan_stop)? {
            if !prefix_chunk.name.starts_with("ImageDataSeq")
                && !chunks
                    .iter()
                    .any(|chunk| chunk.block_offset == prefix_chunk.block_offset)
            {
                chunks.push(prefix_chunk);
            }
        }
    }

    let check_every = file_len / 10;
    let mut next_check = 0;
    if check_every > 0 {
        for chunk in chunks.iter().filter(|c| c.name.starts_with("ImageDataSeq")) {
            if chunk.block_offset <= next_check {
                continue;
            }
            if chunk.block_offset + 4 > file_len {
                return Ok(None);
            }
            f.seek(SeekFrom::Start(chunk.block_offset))?;
            let mut magic = [0u8; 4];
            f.read_exact(&mut magic)?;
            if magic != ND2_MAGIC {
                return Ok(None);
            }
            next_check = chunk.block_offset + check_every;
        }
    }

    chunks.sort_by_key(|c| c.data_offset);
    Ok(Some(chunks))
}

/// Read the `CustomData|AcqTimesCache` per-plane acquisition timestamps.
///
/// Mirrors `ND2Reader.initFile` (java:1105-1108, 1789-1812): the first
/// `CustomData|AcqTimesCache` block carries one `double` per image plane as an
/// undelimited stream of milliseconds at the *end* of the block, which are
/// divided by 1000 to obtain seconds and stored in `tsT`. Java seeks to
/// `fp + (len - imageCount*8)` where `fp = helper + 24` (after the 12-byte name
/// length / data length header plus the 12-byte block-type peek) and
/// `len = nameLength + dataLength`. Re-expressed with the fields available here
/// (`data_offset = chunkStart + 16 + nameLength`), the tail begins at
/// `data_offset + data_length + 8 - imageCount*8`; the trailing `+8` faithfully
/// reproduces Java's `helper+24` versus block-end (`helper+12+len`) offset quirk.
fn read_acq_times_cache(
    f: &mut BufReader<File>,
    chunk: &Nd2Chunk,
    image_count: usize,
) -> std::io::Result<Vec<f64>> {
    if image_count == 0 {
        return Ok(Vec::new());
    }
    let timestamp_bytes = (image_count as u64) * 8;
    let tail_start = chunk
        .data_offset
        .saturating_add(chunk.data_length)
        .saturating_add(8)
        .saturating_sub(timestamp_bytes);
    let file_len = f.get_ref().metadata()?.len();
    if tail_start >= file_len || tail_start + timestamp_bytes > file_len {
        return Ok(Vec::new());
    }
    f.seek(SeekFrom::Start(tail_start))?;
    let mut buf = vec![0u8; timestamp_bytes as usize];
    f.read_exact(&mut buf)?;
    let mut out = Vec::with_capacity(image_count);
    for i in 0..image_count {
        let bytes: [u8; 8] = buf[i * 8..i * 8 + 8].try_into().unwrap();
        // timestamps are stored in ms; we want them in seconds (java:1804-1805).
        out.push(f64::from_le_bytes(bytes) / 1000.0);
    }
    Ok(out)
}

/// Read `count` little-endian f64 values starting at `offset` (ND2Reader.initFile
/// java:1555-1597, the binary posX/posY/posZ fallback reads at xOffset/yOffset/
/// zOffset). Returns an empty vec if the requested range is out of bounds.
fn read_doubles_at(
    f: &mut BufReader<File>,
    offset: u64,
    count: usize,
) -> std::io::Result<Vec<f64>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let need = (count as u64) * 8;
    let file_len = f.get_ref().metadata()?.len();
    if offset >= file_len || offset + need > file_len {
        return Ok(Vec::new());
    }
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; need as usize];
    f.read_exact(&mut buf)?;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let bytes: [u8; 8] = buf[i * 8..i * 8 + 8].try_into().unwrap();
        out.push(f64::from_le_bytes(bytes));
    }
    Ok(out)
}

/// Read `count` little-endian i32 values starting at `offset` (ND2Reader.initFile
/// java:1599-1610, the PFS Offset / PFS Status global-metadata lists). Returns an
/// empty vec if the requested range is out of bounds.
fn read_ints_at(f: &mut BufReader<File>, offset: u64, count: usize) -> std::io::Result<Vec<i32>> {
    if count == 0 {
        return Ok(Vec::new());
    }
    let need = (count as u64) * 4;
    let file_len = f.get_ref().metadata()?.len();
    if offset >= file_len || offset + need > file_len {
        return Ok(Vec::new());
    }
    f.seek(SeekFrom::Start(offset))?;
    let mut buf = vec![0u8; need as usize];
    f.read_exact(&mut buf)?;
    let mut out = Vec::with_capacity(count);
    for i in 0..count {
        let bytes: [u8; 4] = buf[i * 4..i * 4 + 4].try_into().unwrap();
        out.push(i32::from_le_bytes(bytes));
    }
    Ok(out)
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
    /// Flat binary ImageAttributes values parsed in ND2Reader.initFile before
    /// dimension fallbacks.
    attr_size_x: Option<u32>,
    attr_size_y: Option<u32>,
    attr_size_c: Option<u32>,
    attr_bpc_in_memory: Option<u16>,
    attr_bpc_significant: Option<u16>,
    /// ImageAttributesLV fields when the attributes are stored as a normal
    /// Nikon LV tree rather than the flat scan Java also supports.
    lv_size_x: Option<u32>,
    lv_size_y: Option<u32>,
    lv_size_c: Option<u32>,
    lv_bpc_in_memory: Option<u16>,
    lv_bpc_significant: Option<u16>,
    /// dZHigh/dZLow, combined with dZStep to infer sizeZ like Java iterateIn.
    z_high: Option<f64>,
    z_low: Option<f64>,
    /// Dimensions recovered from line-based TextInfo metadata
    /// (`Dimensions`, `Time Loop`, `Z Stack Loop` in ND2Handler.parseKeyAndValue).
    text_size_z: Option<u32>,
    text_size_t: Option<u32>,
    text_series_count: Option<u32>,
}

#[derive(Debug, Clone)]
struct Nd2LoopDescriptor {
    kind: &'static str,
    count: Option<u32>,
}

#[derive(Debug, Clone)]
struct Nd2XmlChannelMetadata {
    name: String,
    emission_wavelength: Option<f64>,
    excitation_wavelength: Option<f64>,
    color: Option<i32>,
}

/// Parse the Nikon LV binary metadata tree starting at the root of a chunk.
///
/// Entry layout: `[type:u8][nameLen:u8][name: nameLen × UTF-16LE]` followed by a
/// type-specific value. Type 11 is a nested level: `[count:i32][absOffset:i64]`,
/// where children live until `absOffset` (relative to the chunk start) and a
/// trailing `count × 8` byte index table is skipped afterwards.
fn parse_nd2_lv(data: &[u8], out: &mut Nd2LvValues) {
    let initial_text_channel_name_count = out.text_channel_names.len();

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
    fn nd2_lv_direct_child_u32(data: &[u8], mut p: usize, end: usize, wanted: &str) -> Option<u32> {
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
                1 => p += 1,
                2 | 3 => {
                    if name == wanted {
                        return read_i32(data, p)
                            .filter(|&value| value > 0)
                            .map(|value| value as u32);
                    }
                    p += 4;
                }
                4 | 5 | 6 | 7 => p += 8,
                8 => {
                    while p + 2 <= end {
                        let u = read_u16(data, p).unwrap_or(0);
                        p += 2;
                        if u == 0 {
                            break;
                        }
                    }
                }
                9 => {
                    let len = read_i64(data, p)?.max(0) as usize;
                    p = (p + 8 + len).min(end);
                }
                11 => {
                    let count = read_i32(data, p)?;
                    let off = read_i64(data, p + 4)?;
                    let child_end = entry_start
                        .saturating_add(off.max(0) as usize)
                        .clamp(p + 12, data.len());
                    p = child_end
                        .saturating_add((count.max(0) as usize) * 8)
                        .min(end);
                }
                _ => break,
            }
        }
        None
    }

    // Recursive walk. `end` is an exclusive byte bound for the current level.
    // `current_color` carries the most recent uiColor within this level, so the
    // next sDescription can be paired with it (ND2Reader.iterateIn).
    fn walk(
        data: &[u8],
        mut p: usize,
        end: usize,
        depth: u32,
        out: &mut Nd2LvValues,
        channel_count_context: Option<u32>,
        level_name: Option<&str>,
    ) -> usize {
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
                    if name == "uiColor" || name == "Color" {
                        current_color = read_i32(data, p);
                    } else if let Some(value) = read_i32(data, p).filter(|&v| v > 0) {
                        match name.as_str() {
                            "uiWidth" => {
                                out.lv_size_x = Some(value as u32);
                            }
                            "uiHeight" => {
                                out.lv_size_y = Some(value as u32);
                            }
                            "uiComp" | "uiVirtualComponents" if out.lv_size_c.is_none() => {
                                out.lv_size_c = Some(value as u32);
                            }
                            "uiBpcInMemory" if out.lv_bpc_in_memory.is_none() => {
                                out.lv_bpc_in_memory = Some(value as u16);
                            }
                            "uiBpcSignificant" if out.lv_bpc_significant.is_none() => {
                                out.lv_bpc_significant = Some(value as u16);
                            }
                            "ChannelCount" => {}
                            _ => {}
                        }
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
                            "dZHigh" => {
                                if v.is_finite() && out.z_high.is_none() {
                                    out.z_high = Some(v);
                                }
                            }
                            "dZLow" => {
                                if v.is_finite() && out.z_low.is_none() {
                                    out.z_low = Some(v);
                                }
                            }
                            "EmWavelength" => {
                                if !out.emission_wavelengths.contains(&v) {
                                    out.emission_wavelengths.push(v);
                                }
                            }
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
                        // Pair the channel name with the pending uiColor, mirroring
                        // ND2Reader.iterateIn:2197-2202 (only when a color was seen).
                        if let Some(color) = current_color {
                            out.text_channel_names.push(s.clone());
                            out.channel_colors.insert(s, color);
                        }
                    } else if name == "sObjective" && !s.is_empty() && out.objective_model.is_none()
                    {
                        out.objective_model = Some(s);
                    } else if (name.starts_with("TextInfo")
                        || s.contains("<variant")
                        || s.contains("<NDControl"))
                        && !s.is_empty()
                    {
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
                        walk(
                            data,
                            p,
                            child_end,
                            depth + 1,
                            out,
                            channel_count_context,
                            level_name,
                        );
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
                        let child_channel_count =
                            nd2_lv_direct_child_u32(data, p, child_end, "ChannelCount")
                                .filter(|&count| count > 0)
                                .or(channel_count_context);
                        walk(
                            data,
                            p,
                            child_end.min(end),
                            depth + 1,
                            out,
                            child_channel_count,
                            Some(&name),
                        );
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

    walk(data, 0, data.len(), 0, out, None, None);

    // Some ND2 LV blocks carry channel descriptors in deeply nested list
    // variants whose offsets are not always represented by the simple recursive
    // walk above. Java's iterateIn still sees the scalar entry stream and pairs
    // the most recent uiColor with the following sDescription. Recover that
    // exact pair from the raw LV entry layout as a fallback.
    let mut current_color: Option<i32> = None;
    let mut p = 0usize;
    while p + 2 <= data.len() {
        let ty = data[p];
        let name_len = data[p + 1] as usize;
        let name_start = p + 2;
        let name_end = name_start + name_len * 2;
        if name_end > data.len() {
            p += 1;
            continue;
        }
        let name_units: Vec<u16> = (0..name_len)
            .filter_map(|i| read_u16(data, name_start + i * 2))
            .collect();
        let name = String::from_utf16_lossy(&name_units)
            .trim_end_matches('\0')
            .to_string();
        if (ty == 2 || ty == 3) && (name == "uiColor" || name == "Color") {
            current_color = read_i32(data, name_end).filter(|&value| value != 0);
        } else if ty == 8 && name == "sDescription" {
            if let Some(color) = current_color {
                let mut units = Vec::new();
                let mut q = name_end;
                while q + 2 <= data.len() {
                    let u = read_u16(data, q).unwrap_or(0);
                    q += 2;
                    if u == 0 {
                        break;
                    }
                    units.push(u);
                }
                let value = String::from_utf16_lossy(&units);
                if !value.is_empty()
                    && !out.text_channel_names[initial_text_channel_name_count..].contains(&value)
                {
                    out.text_channel_names.push(value.clone());
                    out.channel_colors.entry(value).or_insert(color);
                }
            }
        }
        p += 1;
    }
}

/// Parse the flat binary `ImageAttributes*` attribute list handled directly in
/// `ND2Reader.initFile`: core dimensions/bit depth plus compression flags.
fn parse_nd2_binary_image_attributes(data: &[u8], out: &mut Nd2LvValues) {
    fn read_i32(d: &[u8], p: usize) -> Option<i32> {
        d.get(p..p + 4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    if data.len() <= 7 {
        return;
    }

    // Java skips 6 bytes, then consumes zero padding and one non-zero byte
    // before the repeated [nameLen][UTF-16LE name][i32 value] records.
    let mut p = 6usize;
    while p < data.len() && data[p] == 0 {
        p += 1;
    }
    if p < data.len() {
        p += 1;
    }

    let mut saw_lossless_param = false;
    let mut lossless_param = false;
    let mut can_be_lossless = true;

    while p < data.len() {
        let name_len = data[p] as usize;
        p += 1;
        if name_len == 0 {
            continue;
        }
        let name_bytes = match name_len.checked_mul(2) {
            Some(v) => v,
            None => break,
        };
        if p + name_bytes + 4 > data.len() {
            break;
        }
        let units: Vec<u16> = (0..name_len)
            .map(|i| u16::from_le_bytes([data[p + i * 2], data[p + i * 2 + 1]]))
            .take_while(|&u| u != 0)
            .collect();
        let name = String::from_utf16_lossy(&units);
        p += name_bytes;
        let Some(value) = read_i32(data, p) else {
            break;
        };
        p += 4;

        match name.as_str() {
            "uiWidth" if value > 0 => out.attr_size_x = Some(value as u32),
            "uiHeight" if value > 0 => out.attr_size_y = Some(value as u32),
            "uiComp" if value > 0 => out.attr_size_c = Some(value as u32),
            "uiBpcInMemory" if value > 0 => out.attr_bpc_in_memory = Some(value as u16),
            "uiBpcSignificant" if value > 0 => out.attr_bpc_significant = Some(value as u16),
            // Java binary ImageAttributes path: isLossless = valueOrLength >= 0.
            "dCompressionParam" => {
                saw_lossless_param = true;
                lossless_param = value >= 0;
            }
            // Java: canBeLossless = valueOrLength <= 0, then
            // isLossless = isLossless && canBeLossless after the block.
            "eCompression" => can_be_lossless = value <= 0,
            _ => {}
        }
    }

    if saw_lossless_param {
        out.is_lossless = lossless_param && can_be_lossless;
    }
}

/// Result of the binary `ImageMetadataLV` eType/uiCount walk
/// (ND2Reader.initFile java:967-1062, 1135-1141).
///
/// Faithfully reproduces the flat byte scan over the binary image-metadata block
/// that builds `imageMetadataLVOrder` and the per-axis counts (M/T/Z) directly,
/// rather than inferring dimensions from the XML loop heuristic.
#[derive(Default, Clone)]
struct ImageMetadataLv {
    /// Concatenated axis order, e.g. "MTZ" / "TZ" (Java: imageMetadataLVOrder).
    order: String,
    /// XY (multi-point / series) count (Java: XYCount).
    xy_count: i32,
    /// Time count (Java: timeCount).
    time_count: i32,
    /// Z count (Java: zCount).
    z_count: i32,
    /// Whether the walk actually set a count (Java: currentCountSetted).
    current_count_set: bool,
    /// Whether the LV block was processed (Java: imageMetadataLVProcessed).
    processed: bool,
}

/// Port of the `blockType.startsWith("ImageMetadat")` binary walk in
/// `ND2Reader.initFile` (java:967-1062). `data` starts where Java's stream is
/// positioned immediately after `blockType = in.readString(12)`: the unconsumed
/// suffix of the chunk name is still before the chunk payload. Java then does
/// `skipBytes(6)` and a `while (in.read() == 0)` zero-skip before the attribute
/// scan; we mirror that on the byte buffer.
///
/// Returns `None` if the block does not look like a parseable LV experiment.
fn parse_image_metadata_lv(data: &[u8]) -> Option<ImageMetadataLv> {
    // strip_string equivalent: drop trailing NUL units. Java's
    // DataTools.stripString trims the string at the first embedded null.
    fn strip_string(units: &[u16]) -> String {
        let trimmed: Vec<u16> = units.iter().copied().take_while(|&u| u != 0).collect();
        String::from_utf16_lossy(&trimmed)
    }
    fn read_i32(d: &[u8], p: usize) -> Option<i32> {
        d.get(p..p + 4)
            .map(|b| i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    let mut state = ImageMetadataLv {
        order: String::new(),
        xy_count: 1,
        time_count: 1,
        z_count: 1,
        current_count_set: false,
        processed: false,
    };

    // Java: in.skipBytes(6); then while (in.read() == 0); — the failed zero
    // comparison has already consumed the first non-zero sentinel byte.
    let start_file_pointer = 0usize;
    let mut p = 6usize;
    while p < data.len() && data[p] == 0 {
        p += 1;
    }
    if p < data.len() {
        p += 1;
    }
    let end_fp = data.len(); // endFP = fp + len - 18; the chunk data is already that bounded slice
    let mut current_file_pointer = p;

    let mut e_type: i32 = 0;
    let mut next_experiment = true;

    loop {
        // in.seek(currentFilePointer)
        let mut q = current_file_pointer;
        if q >= data.len() {
            break;
        }
        // int nameLen = in.read();
        let name_len = data[q] as i32;
        q += 1;
        if name_len == 0 {
            current_file_pointer += 1;
            if current_file_pointer > end_fp {
                break;
            }
            continue;
        }

        // String attributeName = stripString(in.readString(nameLen * 2));
        let read_bytes = (name_len as usize) * 2;
        if q + read_bytes > data.len() {
            // Java's readString would hit EOF; treat as end of block.
            break;
        }
        let units: Vec<u16> = (0..name_len as usize)
            .map(|i| u16::from_le_bytes([data[q + i * 2], data[q + i * 2 + 1]]))
            .collect();
        let attribute_name = strip_string(&units);
        let after_name = q + read_bytes;

        // if (attributeName.length() != nameLen - 1) { currentFilePointer++; continue; }
        if attribute_name.chars().count() as i32 != name_len - 1 {
            current_file_pointer += 1;
            if current_file_pointer > end_fp {
                break;
            }
            continue;
        }

        // `in` is now positioned at `after_name` (the readString advanced it).
        let mut file_pointer = after_name;

        if attribute_name == "SLxExperiment" {
            current_file_pointer += (name_len as usize) * 2;
            state.processed = true;
            state.order = String::new();
        }

        if attribute_name == "eType" {
            current_file_pointer += (name_len as usize) * 2;
            if next_experiment {
                if let Some(v) = read_i32(data, file_pointer) {
                    e_type = v;
                }
                file_pointer += 4;
            }
            next_experiment = false;
        } else if attribute_name == "uiCount" {
            current_file_pointer += (name_len as usize) * 2;
            if !state.current_count_set {
                if e_type == 2 {
                    state.order = format!("M{}", state.order);
                    if let Some(v) = read_i32(data, file_pointer) {
                        state.xy_count = v;
                    }
                    file_pointer += 4;
                } else if e_type == 1 {
                    state.order = format!("T{}", state.order);
                    if let Some(v) = read_i32(data, file_pointer) {
                        state.time_count = v;
                    }
                    file_pointer += 4;
                }
                if e_type == 4 {
                    state.order = format!("Z{}", state.order);
                    if let Some(v) = read_i32(data, file_pointer) {
                        state.z_count = v;
                    }
                    file_pointer += 4;
                }
                state.current_count_set = true;
            }
        } else if attribute_name == "bKeepObject" {
            current_file_pointer += (name_len as usize) * 2;
        } else if attribute_name == "uiRepeatCount" {
            current_file_pointer += (name_len as usize) * 2;
        } else if attribute_name == "vectStimulationConfigurationsSize" {
            current_file_pointer += (name_len as usize) * 2;
        } else if attribute_name == "uiNextLevelCount" {
            current_file_pointer += (name_len as usize) * 2;
            let ui_next_level_count = read_i32(data, file_pointer).unwrap_or(0);
            file_pointer += 4;
            if ui_next_level_count == 0 {
                break;
            }
            state.current_count_set = false;
            next_experiment = true;
        }

        // if (in.getFilePointer() > endFP) { in.seek(startFilePointer); break; }
        if file_pointer > end_fp {
            let _ = start_file_pointer;
            break;
        }

        current_file_pointer += 1;
        if current_file_pointer > end_fp {
            break;
        }
    }

    Some(state)
}

fn image_metadata_lv_scan_bytes(chunk_name: &str, payload: &[u8]) -> Vec<u8> {
    let name_bytes = chunk_name.as_bytes();
    let mut data = Vec::with_capacity(name_bytes.len().saturating_sub(12) + payload.len());
    if name_bytes.len() > 12 {
        data.extend_from_slice(&name_bytes[12..]);
    }
    data.extend_from_slice(payload);
    data
}

/// FormatTools.rasterToPosition: block 0 varies fastest (Java
/// loci.formats.FormatTools.rasterToPosition).
fn raster_to_position(lengths: &[i32], mut raster: i32) -> Vec<i32> {
    let mut pos = vec![0i32; lengths.len()];
    let mut offset = 1i32;
    for i in 0..lengths.len() {
        let offset1 = offset.saturating_mul(lengths[i]);
        let q = if i < lengths.len() - 1 {
            if offset1 != 0 {
                raster % offset1
            } else {
                0
            }
        } else {
            raster
        };
        pos[i] = if offset != 0 { q / offset } else { 0 };
        raster -= q;
        offset = offset1;
    }
    pos
}

/// FormatTools.positionToRaster: inverse of rasterToPosition.
fn position_to_raster(lengths: &[i32], pos: &[i32]) -> i32 {
    let mut offset = 1i32;
    let mut raster = 0i32;
    for i in 0..lengths.len() {
        raster += offset.saturating_mul(pos[i]);
        offset = offset.saturating_mul(lengths[i]);
    }
    raster
}

/// Build the plane → (series, plane) mapping from the binary ImageMetadataLV
/// order (ND2Reader.initFile java:1624-1718, the `imageMetadataLVProcessed`
/// branch). `image_sequence_indices` carries each ImageDataSeq frame's parsed
/// index (the `ndx` in Java's image-name loop), in the same order as
/// `image_chunks`.
///
/// Returns `(series_count, source_planes)` where `source_planes[series]` lists
/// the global image-chunk positions that belong to that series, ordered by the
/// computed in-series plane index. Returns `None` if the LV order yields no
/// usable M (series) axis or the layout is degenerate.
struct Nd2RasterMapping {
    series_count: usize,
    source_planes: Vec<Vec<usize>>,
    field_index: usize,
    /// Fixed in-series plane count (the collapsed zctLengths product). Equals
    /// `offsets[i].length` in Java (the per-series offsets array length, before
    /// the invalid-slot count is subtracted).
    in_series_planes: usize,
    /// Per-series flag: was plane slot 0 filled? Mirrors Java's
    /// `offsets[i][0] > 0` test used by the tmpOffsets compaction (java:1708-1713).
    first_slot_filled: Vec<bool>,
}

fn nd2_raster_mapping(
    lv: &ImageMetadataLv,
    size_z: u32,
    size_t: u32,
    series_count: usize,
    image_sequence_indices: &[usize],
) -> Option<Nd2RasterMapping> {
    // Java builds lengths[4] from imageMetadataLVOrder (java:1638-1668).
    let mut lengths = [1i32; 4];
    let mut field_index: usize = 3;
    let mut curr_pos: i32 = 1;
    for c in lv.order.chars() {
        let idx = curr_pos.clamp(0, 3) as usize;
        match c {
            'Z' => lengths[idx] = size_z as i32,
            'M' => {
                field_index = idx;
                lengths[idx] = series_count as i32;
            }
            'T' => lengths[idx] = size_t as i32,
            _ => {
                curr_pos -= 1;
            }
        }
        curr_pos += 1;
    }
    if !lv.order.contains('M') {
        field_index = 3;
    }
    if field_index >= 4 {
        return None;
    }

    // zctLengths = lengths with the field (series) axis collapsed to 1.
    let mut zct_lengths = lengths;
    zct_lengths[field_index] = 1;

    let in_series_planes: usize = zct_lengths.iter().map(|&l| l.max(1) as usize).product();
    let n_series = lengths[field_index].max(1) as usize;

    let mut source_planes: Vec<Vec<usize>> = vec![Vec::new(); n_series];
    // Track plane index per series so we can place chunks in raster order.
    let mut placed: Vec<Vec<Option<usize>>> = vec![vec![None; in_series_planes]; n_series];

    // oneIndexed detection (java:1690-1695): if the first frame's parsed index is
    // 1, all indices are decremented.
    let mut one_indexed = false;
    for (i, &ndx_raw) in image_sequence_indices.iter().enumerate() {
        let mut ndx = ndx_raw as i32;
        if ndx == 1 && i == 0 {
            one_indexed = true;
        }
        if one_indexed {
            ndx -= 1;
        }
        if ndx < 0 {
            continue;
        }
        let mut pos = raster_to_position(&lengths, ndx);
        let series_index = pos[field_index];
        pos[field_index] = 0;
        let plane = position_to_raster(&zct_lengths, &pos);
        if series_index >= 0
            && (series_index as usize) < n_series
            && plane >= 0
            && (plane as usize) < in_series_planes
        {
            placed[series_index as usize][plane as usize] = Some(i);
        }
    }

    // Flatten each series' placement table into a dense, raster-ordered plane list,
    // skipping unfilled slots. Track whether slot 0 was filled (Java's
    // `offsets[i][0] > 0`) for the tmpOffsets compaction in the caller.
    let mut first_slot_filled = vec![false; n_series];
    for (s, table) in placed.into_iter().enumerate() {
        first_slot_filled[s] = table.first().map(|slot| slot.is_some()).unwrap_or(false);
        for slot in table {
            if let Some(global_plane) = slot {
                source_planes[s].push(global_plane);
            }
        }
    }

    Some(Nd2RasterMapping {
        series_count: n_series,
        source_planes,
        field_index,
        in_series_planes,
        first_slot_filled,
    })
}

/// Very lightweight XML value extractor — just grab the first occurrence of a tag.
fn xml_value(xml: &str, tag: &str) -> Option<String> {
    let (pos, gt) = xml_find_start_tag(xml, tag, 0)?;
    let after_open = &xml[pos..];
    let attrs = &after_open[..gt];
    if let Some(value) = xml_attr(attrs, "value") {
        return Some(value);
    }

    let content_start = &after_open[gt + 1..];
    let close = format!("</{}>", tag);
    let end = content_start.find(&close)?;
    Some(content_start[..end].trim().to_string())
}

fn xml_find_start_tag(xml: &str, tag: &str, mut cursor: usize) -> Option<(usize, usize)> {
    let open = format!("<{tag}");
    while let Some(relative_pos) = xml[cursor..].find(&open) {
        let pos = cursor + relative_pos;
        let name_end = pos + open.len();
        let valid_boundary = xml[name_end..]
            .chars()
            .next()
            .is_some_and(|c| c == '>' || c == '/' || c.is_whitespace());
        if valid_boundary {
            let gt = xml[pos..].find('>')?;
            return Some((pos, gt));
        }
        cursor = name_end;
    }
    None
}

fn xml_attr(tag_text: &str, attr: &str) -> Option<String> {
    let mut cursor = 0;
    while cursor < tag_text.len() {
        let tail = &tag_text[cursor..];
        let Some(eq_rel) = tail.find('=') else {
            break;
        };
        let eq = cursor + eq_rel;
        let name_start = tag_text[..eq]
            .char_indices()
            .rev()
            .find(|&(_, c)| c.is_whitespace() || c == '<')
            .map_or(0, |(pos, c)| pos + c.len_utf8());
        let name = tag_text[name_start..eq].trim();
        let rest = tag_text[eq + 1..].trim_start();
        let mut chars = rest.chars();
        let quote = chars.next()?;
        if quote != '"' && quote != '\'' {
            cursor = eq + 1;
            continue;
        }
        let value_start = eq + 1 + (tag_text[eq + 1..].len() - rest.len()) + quote.len_utf8();
        let value_end = tag_text[value_start..].find(quote)?;
        if name == attr {
            return Some(tag_text[value_start..value_start + value_end].to_string());
        }
        cursor = value_start + value_end + quote.len_utf8();
    }
    None
}

fn xml_values(xml: &str, tag: &str) -> Vec<String> {
    let mut values = Vec::new();
    let close = format!("</{}>", tag);
    let mut cursor = 0;

    while let Some((pos, gt)) = xml_find_start_tag(xml, tag, cursor) {
        let after_open = &xml[pos..];
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

fn xml_element_blocks<'a>(xml: &'a str, tag: &str) -> Vec<&'a str> {
    let mut blocks = Vec::new();
    let close = format!("</{tag}>");
    let mut cursor = 0;

    while let Some((pos, gt)) = xml_find_start_tag(xml, tag, cursor) {
        let after_open = &xml[pos..];
        let end = if after_open[..gt].trim_end().ends_with('/') {
            pos + gt + 1
        } else if let Some(end_rel) = xml[pos + gt + 1..].find(&close) {
            pos + gt + 1 + end_rel + close.len()
        } else {
            break;
        };
        blocks.push(&xml[pos..end]);
        cursor = end;
    }

    blocks
}

/// Collect the `<item_N>` numeric children of the first `<tag>…</tag>` element,
/// mirroring ND2Handler's `dPosX`/`dPosY`/`dPosZ` position-list parsing.
fn nd2_xml_item_list_f64(xml: &str, tag: &str) -> Vec<f64> {
    let close = format!("</{tag}>");
    let Some((pos, gt)) = xml_find_start_tag(xml, tag, 0) else {
        return Vec::new();
    };
    let after_open = &xml[pos..];
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
        if let Some(v) = value
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite())
        {
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

fn nd2_xml_signed_f64_value(xml: &str, tag: &str) -> Option<f64> {
    xml_value(xml, tag)?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite())
}

fn nd2_z_count_from_range(
    z_high: Option<f64>,
    z_low: Option<f64>,
    z_step: Option<f64>,
) -> Option<u32> {
    let high = z_high?;
    let low = z_low?;
    let step = z_step.filter(|v| *v > 0.0)?;
    let count = ((high - low).abs() / step).ceil() as u32 + 1;
    (count > 1).then_some(count)
}

fn nd2_xml_metadata_channels(xml: &str) -> Vec<Nd2XmlChannelMetadata> {
    let mut channels = Vec::new();
    let mut cursor = 0;

    while let Some(relative_pos) = xml[cursor..].find("<Channel_") {
        let pos = cursor + relative_pos;
        let after_open = &xml[pos..];
        let Some(gt) = after_open.find('>') else {
            break;
        };
        let tag_text = &after_open[..gt];
        let tag_name = tag_text
            .trim_start_matches('<')
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_end_matches('/');
        if tag_name.is_empty() {
            cursor = pos + gt + 1;
            continue;
        }
        let close = format!("</{tag_name}>");
        let end = if tag_text.trim_end().ends_with('/') {
            pos + gt + 1
        } else if let Some(end_rel) = xml[pos + gt + 1..].find(&close) {
            pos + gt + 1 + end_rel + close.len()
        } else {
            break;
        };
        let block = &xml[pos..end];
        if let Some(name) = xml_value(block, "Name").filter(|name| !name.is_empty()) {
            channels.push(Nd2XmlChannelMetadata {
                name,
                emission_wavelength: xml_value(block, "EmWavelength")
                    .and_then(|value| value.parse::<f64>().ok())
                    .filter(|value| value.is_finite() && *value > 0.0),
                excitation_wavelength: None,
                color: xml_value(block, "Color")
                    .and_then(|value| value.parse::<i32>().ok())
                    .filter(|&value| value != 0),
            });
        }
        cursor = end;
    }

    channels
}

fn nd2_xml_old_jp2_valid_position_names(xml: &str) -> Vec<String> {
    let mut best: Vec<String> = Vec::new();
    let mut cursor = 0;
    while let Some((pos_name_pos, pos_name_gt)) = xml_find_start_tag(xml, "pPosName", cursor) {
        let Some(pos_name_end_rel) = xml[pos_name_pos + pos_name_gt + 1..].find("</pPosName>")
        else {
            break;
        };
        let pos_name_end = pos_name_pos + pos_name_gt + 1 + pos_name_end_rel;
        let names_block = &xml[pos_name_pos + pos_name_gt + 1..pos_name_end];

        let mut names = Vec::new();
        let mut item_cursor = 0;
        while let Some(relative_pos) = names_block[item_cursor..].find("<item_") {
            let pos = item_cursor + relative_pos;
            let after_open = &names_block[pos..];
            let Some(gt) = after_open.find('>') else {
                break;
            };
            if let Some(value) = xml_attr(&after_open[..gt], "value") {
                names.push(value);
            }
            item_cursor = pos + gt + 1;
        }

        let mut valid = Vec::new();
        if let Some((valid_pos, valid_gt)) = xml_find_start_tag(xml, "pItemValid", pos_name_end) {
            let search_end = (pos_name_end + 32768).min(xml.len());
            if valid_pos < search_end {
                if let Some(valid_end_rel) = xml[valid_pos + valid_gt + 1..].find("</pItemValid>") {
                    let valid_end = valid_pos + valid_gt + 1 + valid_end_rel;
                    let valid_block = &xml[valid_pos + valid_gt + 1..valid_end];
                    let mut valid_cursor = 0;
                    while let Some(relative_pos) = valid_block[valid_cursor..].find("<_") {
                        let pos = valid_cursor + relative_pos;
                        let after_open = &valid_block[pos..];
                        let Some(gt) = after_open.find('>') else {
                            break;
                        };
                        let tag = &after_open[..gt];
                        if let Some(value) = xml_attr(tag, "value") {
                            valid.push(value == "true");
                        }
                        valid_cursor = pos + gt + 1;
                    }
                }
            }
        }

        if !valid.is_empty() {
            names = names
                .into_iter()
                .enumerate()
                .filter_map(|(index, name)| {
                    valid.get(index).copied().unwrap_or(true).then_some(name)
                })
                .collect();
        }

        if best.is_empty()
            || (!best.iter().any(|name| !name.is_empty())
                && names.iter().any(|name| !name.is_empty()))
        {
            best = names;
        }
        if best.iter().any(|name| !name.is_empty()) {
            break;
        }
        cursor = pos_name_end + "</pPosName>".len();
    }
    best
}

fn nd2_xml_xy_position_count_with_valid_flags(xml: &str) -> Option<u32> {
    let xy_count = nd2_xml_first_loop_count_near_runtype(xml, "XYPosLoop")?;
    let xy_pos = xml.find("XYPosLoop")?;
    let search_end = (xy_pos + 32768).min(xml.len());
    if let Some((valid_pos, valid_gt)) = xml_find_start_tag(xml, "pItemValid", xy_pos) {
        if valid_pos < search_end {
            if let Some(valid_end_rel) = xml[valid_pos + valid_gt + 1..].find("</pItemValid>") {
                let valid_end = valid_pos + valid_gt + 1 + valid_end_rel;
                let valid_block = &xml[valid_pos + valid_gt + 1..valid_end];
                let true_count = valid_block.matches("value=\"true\"").count() as u32;
                let false_count = valid_block.matches("value=\"false\"").count() as u32;
                if true_count + false_count > 0 {
                    return Some(true_count.max(1));
                }
            }
        }
    }
    Some(xy_count)
}

fn nd2_replace_position_names_if_more_informative(
    current: &mut Vec<String>,
    candidate: Vec<String>,
) {
    if candidate.is_empty() {
        return;
    }
    let current_has_names = current.iter().any(|name| !name.is_empty());
    let candidate_has_names = candidate.iter().any(|name| !name.is_empty());
    if current.is_empty() || (!current_has_names && candidate_has_names) {
        *current = candidate;
    }
}

fn nd2_xml_first_loop_count_near_runtype(xml: &str, runtype_suffix: &str) -> Option<u32> {
    let mut cursor = 0;
    while let Some(relative_pos) = xml[cursor..].find(runtype_suffix) {
        let pos = cursor + relative_pos;
        let tag_start = xml[..pos].rfind('<').unwrap_or(pos);
        let tag_end = xml[pos..].find('>').map(|gt| pos + gt)?;
        let tag = &xml[tag_start..tag_end];
        if xml_attr(tag, "runtype")
            .as_deref()
            .is_some_and(|runtype| runtype.ends_with(runtype_suffix))
        {
            if let Some((count_pos, count_gt)) = xml_find_start_tag(xml, "uiCount", tag_end + 1) {
                if count_pos <= tag_end + 8192 {
                    let count_tag = &xml[count_pos..count_pos + count_gt];
                    if let Some(count) = xml_attr(count_tag, "value")
                        .and_then(|value| value.parse::<u32>().ok())
                        .filter(|&count| count > 0)
                    {
                        return Some(count);
                    }
                }
            }
        }
        cursor = pos + runtype_suffix.len();
    }
    None
}

fn nd2_xml_metadata_channel_groups(xml: &str) -> Vec<Vec<Nd2XmlChannelMetadata>> {
    let mut groups = Vec::new();
    for metadata_block in xml_element_blocks(xml, "Metadata") {
        let channels = nd2_xml_metadata_channels(metadata_block);
        if !channels.is_empty() {
            groups.push(channels);
        }
    }
    groups
}

fn nd2_apply_metadata_channels(xml: &str, out: &mut Nd2LvValues) {
    let channels = nd2_xml_metadata_channels(xml);
    if channels.is_empty() {
        return;
    }

    if out.channel_names.len() < channels.len() {
        out.channel_names = channels
            .iter()
            .map(|channel| channel.name.clone())
            .collect();
    }
    for channel in channels {
        if let Some(color) = channel.color {
            out.channel_colors.entry(channel.name).or_insert(color);
        }
        if let Some(value) = channel.emission_wavelength {
            out.emission_wavelengths.push(value);
        }
        if let Some(value) = channel.excitation_wavelength {
            out.excitation_wavelengths.push(value);
        }
    }
}

fn parse_nd2_xml_metadata(xml: &str, out: &mut Nd2LvValues) {
    parse_nd2_text_info_elements(xml, out);
    nd2_apply_metadata_channels(xml, out);

    if out.calibration.is_none() {
        out.calibration = nd2_xml_f64_value(xml, "dCalibration");
    }
    if out.z_step.is_none() {
        out.z_step = nd2_xml_f64_value(xml, "dZStep");
    }
    if out.z_high.is_none() {
        out.z_high = nd2_xml_signed_f64_value(xml, "dZHigh");
    }
    if out.z_low.is_none() {
        out.z_low = nd2_xml_signed_f64_value(xml, "dZLow");
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
    fn positive_digits(value: &str) -> Option<u32> {
        let digits = value
            .chars()
            .filter(|c| c.is_ascii_digit())
            .collect::<String>();
        digits.parse::<u32>().ok().filter(|&v| v > 0)
    }

    fn apply_text_dimension_token(token: &str, out: &mut Nd2LvValues) {
        let token = token.trim();
        let Some(value) = positive_digits(token).map(|v| v.max(1)) else {
            return;
        };
        if token.starts_with("XY") {
            if value > 1 {
                out.text_series_count = Some(value);
            }
        } else if token.starts_with('T') {
            if out
                .text_size_t
                .is_none_or(|current| current <= 1 || value < current)
            {
                out.text_size_t = Some(value);
            }
        } else if token.starts_with('Z') {
            if out.text_size_z.is_none_or(|current| current <= 1) {
                out.text_size_z = Some(value);
            }
        }
    }

    fn apply_text_key_value(key: &str, value: &str, out: &mut Nd2LvValues) {
        if value.is_empty() {
            return;
        }
        if key == "Name" {
            // ND2Handler:830-831 / 908-909 — channel name.
            if !out.channel_names.contains(&value.to_string()) {
                out.channel_names.push(value.to_string());
            }
        } else if key.starts_with("Dimensions") || key.starts_with("Abmessungen") {
            for dim in value.split(" x ") {
                apply_text_dimension_token(dim, out);
            }
        } else if key == "Line" {
            // ND2Handler.parseKeyAndValue:878-886 recursively parses semicolon
            // sub-fields like `Excitation wavelength:488`.
            for item in value.split(';') {
                let Some(sep) = item.find(':') else {
                    continue;
                };
                apply_text_key_value(item[..sep].trim(), item[sep + 1..].trim(), out);
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
        } else if key == "Z Stack Loop" {
            if let Some(v) = value.parse::<u32>().ok().filter(|&v| v > 0) {
                out.text_size_z = Some(v);
            }
        } else if key == "Time Loop" {
            if let Some(v) = value.parse::<u32>().ok().filter(|&v| v > 0) {
                if out.text_size_t.is_none() {
                    out.text_size_t = Some(v);
                }
            }
        }
    }

    let trimmed = nd2_text_xml_fragment(text).unwrap_or_else(|| text.trim().to_string());
    // XML case: reuse the same parser ND2Handler uses for metadata XML.
    if trimmed.contains('<') && trimmed.contains('>') {
        parse_nd2_xml_metadata(&trimmed, out);
    }

    // Line-based fallback (ND2Handler.parseKeyAndValue). This runs regardless,
    // matching how the text key/value pairs supply channel names and emission /
    // excitation wavelengths that the XML form may not carry.
    for line in text.split('\n') {
        let Some(sep) = line.find(':') else { continue };
        let key = line[..sep].trim();
        let value = line[sep + 1..].trim();
        apply_text_key_value(key, value, out);
    }
}

fn nd2_text_xml_fragment(text: &str) -> Option<String> {
    let start = text.find('<')?;
    let end = text.rfind('>')?;
    (end >= start).then(|| text[start..=end].to_string())
}

fn nd2_unescape_text_attr(text: &str) -> String {
    text.replace("&#x000d;", "\r")
        .replace("&#x000D;", "\r")
        .replace("&#x000a;", "\n")
        .replace("&#x000A;", "\n")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&amp;", "&")
}

fn parse_old_nd2_text_info_items(xml: &str, out: &mut Nd2LvValues) {
    let mut cursor = 0;
    while let Some((pos, gt)) = xml_find_start_tag(xml, "TextInfoItem", cursor) {
        let after_open = &xml[pos..];
        if let Some(text) = xml_attr(&after_open[..gt], "Text") {
            parse_text(&nd2_unescape_text_attr(&text), out);
        }
        cursor = pos + gt + 1;
    }
}

fn parse_nd2_text_info_elements(xml: &str, out: &mut Nd2LvValues) {
    let mut cursor = 0;
    while let Some(relative_pos) = xml[cursor..].find("<TextInfo") {
        let pos = cursor + relative_pos;
        let after_open = &xml[pos..];
        let Some(gt) = after_open.find('>') else {
            break;
        };
        let tag_text = &after_open[..gt];
        let value = xml_attr(tag_text, "Text").or_else(|| xml_attr(tag_text, "value"));
        if let Some(text) = value {
            parse_text(&nd2_unescape_text_attr(&text), out);
        }
        cursor = pos + gt + 1;
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
    let mut prev_runtype: Option<String> = None;

    while let Some(relative_pos) = xml[cursor..].find('<') {
        let pos = cursor + relative_pos;
        let after_open = &xml[pos..];
        let Some(gt) = after_open.find('>') else {
            break;
        };
        let tag_text = &after_open[..gt];
        let tag_name = tag_text
            .trim_start_matches('<')
            .trim_start_matches('/')
            .split_whitespace()
            .next()
            .unwrap_or_default()
            .trim_end_matches('/');
        if tag_name == "uiCount"
            && prev_runtype
                .clone()
                .or_else(|| xml_attr(tag_text, "runtype"))
                .as_deref()
                .is_some_and(|runtype| runtype.ends_with(runtype_suffix))
        {
            let value = xml_attr(tag_text, "value").or_else(|| {
                if tag_text.trim_end().ends_with('/') {
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
        prev_runtype = xml_attr(tag_text, "runtype");
        cursor = pos + gt + 1;
    }
    None
}

fn nd2_xml_child_ui_count(xml: &str, start: usize, end: usize) -> Option<u32> {
    let (pos, gt) = xml_find_start_tag(&xml[start..end], "uiCount", 0)?;
    let pos = start + pos;
    let after_open = &xml[pos..];
    let attrs = &after_open[..gt];
    let value = xml_attr(attrs, "value").or_else(|| {
        if attrs.trim_end().ends_with('/') {
            None
        } else {
            let content_start = pos + gt + 1;
            xml[content_start..end].find("</uiCount>").map(|end_rel| {
                xml[content_start..content_start + end_rel]
                    .trim()
                    .to_string()
            })
        }
    })?;
    value.parse::<u32>().ok().filter(|&count| count > 0)
}

fn nd2_xml_loop_container_counts(xml: &str, runtype_suffix: &str) -> Vec<u32> {
    let mut counts = Vec::new();
    let mut cursor = 0;

    while let Some(relative_pos) = xml[cursor..].find('<') {
        let pos = cursor + relative_pos;
        let after_open = &xml[pos..];
        let Some(gt) = after_open.find('>') else {
            break;
        };
        let tag_text = &after_open[..gt];
        if xml_attr(tag_text, "runtype")
            .as_deref()
            .is_some_and(|runtype| runtype.ends_with(runtype_suffix))
        {
            let local_end = (pos + gt + 1 + 8192).min(xml.len());
            if let Some(count) = nd2_xml_child_ui_count(xml, pos + gt + 1, local_end) {
                counts.push(count);
            }
        }
        cursor = pos + gt + 1;
    }

    counts
}

fn nd2_xml_nearby_ui_count_after_runtype(xml: &str, runtype_suffix: &str) -> Option<u32> {
    let mut cursor = 0;
    while let Some(rel) = xml[cursor..].find(runtype_suffix) {
        let pos = cursor + rel;
        let start = xml[..pos].rfind('<').unwrap_or(pos);
        let tag_end = xml[pos..].find('>').map(|gt| pos + gt).unwrap_or(pos);
        let tag = &xml[start..=tag_end.min(xml.len().saturating_sub(1))];
        if xml_attr(tag, "runtype")
            .as_deref()
            .is_some_and(|runtype| runtype.ends_with(runtype_suffix))
        {
            let search_end = (tag_end + 2048).min(xml.len());
            if let Some((count_pos, count_gt)) = xml_find_start_tag(xml, "uiCount", tag_end) {
                if count_pos < search_end {
                    let attrs = &xml[count_pos..count_pos + count_gt];
                    if let Some(count) = xml_attr(attrs, "value")
                        .and_then(|value| value.parse::<u32>().ok())
                        .filter(|&count| count > 0)
                    {
                        return Some(count);
                    }
                }
            }
        }
        cursor = pos + runtype_suffix.len();
    }
    None
}

fn nd2_xml_first_two_no_name_values(xml: &str) -> (Option<u32>, Option<u32>) {
    let mut first = None;
    let mut second = None;
    let mut cursor = 0;

    while let Some((pos, gt)) = xml_find_start_tag(xml, "no_name", cursor) {
        let after_open = &xml[pos..];
        let attrs = &after_open[..gt];
        if let Some(value) = xml_attr(attrs, "value").and_then(|value| value.parse::<u32>().ok()) {
            if value == 0 {
                cursor = pos + gt + 1;
                continue;
            }
            if first.is_none() {
                first = Some(value);
            } else {
                second = Some(value);
                break;
            }
        }
        cursor = pos + gt + 1;
    }

    (first, second)
}

fn nd2_xml_ndcontrol_loop_dimensions(xml: &str) -> Option<(Option<u32>, Option<u32>)> {
    let mut cursor = 0;
    while let Some((pos, gt)) = xml_find_start_tag(xml, "LoopSize", cursor) {
        let after_open = &xml[pos..];
        let end = if after_open[..gt].trim_end().ends_with('/') {
            pos + gt + 1
        } else if let Some(end_rel) = xml[pos + gt + 1..].find("</LoopSize>") {
            pos + gt + 1 + end_rel + "</LoopSize>".len()
        } else {
            break;
        };
        let (size_t, size_z) = nd2_xml_first_two_no_name_values(&xml[pos..end]);
        let size_z = size_z.filter(|&value| value > 1);
        if size_t.is_some() || size_z.is_some() {
            return Some((size_z, size_t));
        }
        cursor = end;
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
    if let Some((z, t)) = nd2_xml_ndcontrol_loop_dimensions(xml) {
        if z.is_some() && loop_size_z.is_none_or(|current| current <= 1) {
            *loop_size_z = z;
        }
        if t.is_some() && loop_size_t.is_none_or(|current| current <= 1) {
            *loop_size_t = t;
        }
    }
    if loop_size_z.is_none() {
        *loop_size_z = nd2_xml_ui_count_for_runtype(xml, "ZStackLoop")
            .or_else(|| {
                nd2_xml_loop_container_counts(xml, "ZStackLoop")
                    .into_iter()
                    .next()
            })
            .or_else(|| nd2_xml_nearby_ui_count_after_runtype(xml, "ZStackLoop"));
    }
    if loop_size_t.is_none() {
        *loop_size_t = nd2_xml_ui_count_for_runtype(xml, "TimeLoop")
            .or_else(|| {
                nd2_xml_loop_container_counts(xml, "TimeLoop")
                    .into_iter()
                    .next()
            })
            .or_else(|| nd2_xml_nearby_ui_count_after_runtype(xml, "TimeLoop"));
    }
    if loop_series_count.is_none_or(|count| count <= 1) {
        *loop_series_count = nd2_xml_loop_container_counts(xml, "XYPosLoop")
            .into_iter()
            .find(|&count| count > 1)
            .or_else(|| nd2_xml_nearby_ui_count_after_runtype(xml, "XYPosLoop"))
            .or_else(|| {
                let count = nd2_xml_item_list_f64(xml, "dPosX").len() as u32;
                (count > 1).then_some(count)
            })
            .or_else(|| nd2_xml_ui_count_for_runtype(xml, "XYPosLoop"));
    }
}

fn nd2_u32_value(xml: &str, tag: &str) -> Option<u32> {
    let value = xml_value(xml, tag)?.parse::<u32>().ok()?;
    (value != u32::MAX).then_some(value)
}

fn nd2_bpp_value(xml: &str) -> Option<u16> {
    xml_value(xml, "uiBpcSignificant")
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|&b| b > 0)
}

fn nd2_storage_bpp_value(xml: &str) -> Option<u16> {
    xml_value(xml, "uiBpcInMemory")
        .or_else(|| xml_value(xml, "uiBpc"))
        .and_then(|s| s.parse::<u16>().ok())
        .filter(|&b| b > 0)
}

fn rect_sensor_extent(xml: &str) -> Option<(u32, u32)> {
    let (pos, gt) = xml_find_start_tag(xml, "rectSensorUser", 0)?;
    let after_open = &xml[pos..];
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

fn parse_nd2_attributes(xml: &str) -> (u32, u32, u32, u32, u16) {
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
    let bpp = nd2_bpp_value(xml).unwrap_or(0u16);
    // Java ND2Handler treats uiSequenceCount as an image-count consistency
    // hint, not as a Z size. Z/T dimensions come from loop metadata or later
    // fallback normalization.
    let z_count = 1u32;
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

fn old_nd2_metadata_text(
    f: &mut BufReader<File>,
    last_codestream_offset: u64,
) -> std::io::Result<String> {
    let file_len = f.get_ref().metadata()?.len();
    f.seek(SeekFrom::Start(last_codestream_offset))?;

    let mut found = false;
    let mut metadata_offset = 0u64;
    let mut buf = vec![0u8; 8192];
    while !found && f.stream_position()? < file_len {
        let read = if f.stream_position()? == last_codestream_offset {
            f.read(&mut buf)?
        } else {
            let overlap_start = buf.len() - 10;
            buf.copy_within(overlap_start.., 0);
            10 + f.read(&mut buf[10..])?
        };
        if read == 0 {
            break;
        }
        let scan_len = if read == buf.len() {
            read.saturating_sub(10)
        } else {
            read
        };
        for i in 0..scan_len.saturating_add(9).min(buf.len().saturating_sub(1)) {
            if buf[i] == 0xff && buf[i + 1] == 0xd9 {
                found = true;
                metadata_offset = f.stream_position()? - (scan_len as u64 + 10) + i as u64;
                break;
            }
        }
    }
    if !found || metadata_offset == 0 || metadata_offset >= file_len.saturating_sub(5) {
        return Ok(String::new());
    }

    f.seek(SeekFrom::Start(metadata_offset + 4))?;
    let mut out = String::from("<?xml version=\"1.0\" encoding=\"ISO-8859-1\"?><NIKON>");
    while f.stream_position()? < file_len {
        let mut len_bytes = [0u8; 2];
        if f.read_exact(&mut len_bytes).is_err() {
            break;
        }
        let mut block_len = i16::from_be_bytes(len_bytes) as i64;
        if block_len < 2 {
            break;
        }
        block_len -= 2;
        let remaining = file_len.saturating_sub(f.stream_position()?) as i64;
        if block_len > remaining {
            block_len = remaining;
        }
        if block_len <= 0 {
            break;
        }
        let mut block = vec![0u8; block_len as usize];
        f.read_exact(&mut block)?;
        let mut s = String::from_utf8_lossy(&block).into_owned();
        while let Some(start) = s.find("<!--") {
            let Some(end) = s[start + 4..].find("-->") else {
                break;
            };
            s.replace_range(start..start + 4 + end + 3, "");
        }
        let Some(open_bracket) = s.find('<') else {
            continue;
        };
        let Some(closed_bracket) = s.rfind('>').map(|pos| pos + 1) else {
            continue;
        };
        if closed_bracket < open_bracket {
            continue;
        }
        let s = s[open_bracket..closed_bracket].trim();
        if !s.contains("CalibrationSeq") && !s.contains("VCAL") && !s.contains("jp2cLUNK") {
            out.push_str(s);
        }
    }
    out.push_str("</NIKON>");

    let mut chars: Vec<char> = out.chars().collect();
    let mut offset = 0usize;
    for (i, ch) in chars.iter_mut().enumerate() {
        if offset == 0 && *ch == '!' {
            offset = i + 1;
        }
        if ch.is_control() {
            *ch = ' ';
        }
    }
    if chars.len().saturating_sub(offset) < offset {
        offset = 0;
    }
    Ok(chars[offset..].iter().collect())
}

fn old_nd2_metadata_tail_text(f: &mut BufReader<File>) -> std::io::Result<String> {
    const OLD_ND2_METADATA_TAIL_LIMIT: u64 = 64 * 1024 * 1024;

    let file_len = f.get_ref().metadata()?.len();
    let start = file_len.saturating_sub(OLD_ND2_METADATA_TAIL_LIMIT);
    f.seek(SeekFrom::Start(start))?;
    let mut data = Vec::new();
    f.read_to_end(&mut data)?;
    Ok(String::from_utf8_lossy(&data).into_owned())
}

fn old_nd2_metadata_indexes(text: &str) -> Vec<u32> {
    let mut indexes = Vec::new();
    let mut cursor = 0;
    while let Some((pos, gt)) = xml_find_start_tag(text, "MetadataSeq", cursor) {
        let after_open = &text[pos..];
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

fn old_nd2_plane_metadata(
    text: &str,
    image_count: usize,
    size_c: u32,
) -> (Vec<Option<f64>>, Vec<Option<f64>>) {
    let effective_c = size_c.max(1) as usize;
    let frame_count = (image_count / effective_c).max(1);
    let times = xml_values(text, "dTimeMSec")
        .into_iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .take(frame_count)
        .collect::<Vec<_>>();
    let z_positions = xml_values(text, "dZPos")
        .into_iter()
        .filter_map(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite())
        .take(frame_count)
        .collect::<Vec<_>>();

    let mut plane_delta_t = vec![None; image_count];
    let mut plane_position_z = vec![None; image_count];
    for plane in 0..image_count {
        let frame = plane / effective_c;
        if let Some(time_ms) = times.get(frame).copied() {
            plane_delta_t[plane] = Some(time_ms / 1000.0);
        }
        if let Some(z) = z_positions.get(frame).copied() {
            plane_position_z[plane] = Some(z);
        }
    }
    (plane_delta_t, plane_position_z)
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
    let mut out = Vec::with_capacity(expected);
    dec.read_to_end(&mut out).map_err(BioFormatsError::Io)?;
    require_exact_frame(out, expected, "zlib")
}

fn decompress_nd2_zlib_chunk(data: &[u8], remaining: usize) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read as _;

    let mut dec = ZlibDecoder::new(data);
    let mut out = Vec::with_capacity(remaining);
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

fn nd2_jpeg2000_container(data: &[u8]) -> Jpeg2000Container {
    if data.starts_with(&[0xff, 0x4f, 0xff, 0x51]) {
        Jpeg2000Container::Codestream
    } else if data.starts_with(&[0x00, 0x00, 0x00, 0x0c, b'j', b'P', b' ', b' ']) {
        Jpeg2000Container::Jp2
    } else {
        Jpeg2000Container::Unknown
    }
}

fn nd2_compressed_jpeg2000_payload(data: &[u8]) -> Option<(Vec<u8>, Jpeg2000Container)> {
    const FRAME_PREFIX_LEN: usize = 8;
    const NIKON_PAYLOAD_OFFSET: usize = 4096;

    for prefix_len in [0usize, FRAME_PREFIX_LEN, NIKON_PAYLOAD_OFFSET] {
        let Some(payload) = data.get(prefix_len..) else {
            continue;
        };
        if looks_like_jpeg2000(payload) {
            return Some((payload.to_vec(), nd2_jpeg2000_container(payload)));
        }
    }

    let table = nd2_frame_chunk_table_any_payload(data, data.len())?;
    if nd2_chunk_table_per_chunk_compression_label(data, &table).is_some() {
        return None;
    }
    let payload = assemble_nd2_frame_chunks(data, &table);
    looks_like_jpeg2000(&payload).then(|| {
        let container = nd2_jpeg2000_container(&payload);
        (payload, container)
    })
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

fn nd2_frame_payload_layout(
    prefix: &[u8],
    total_len: usize,
    expected: usize,
) -> (&'static str, usize) {
    const LEGACY_FRAME_PREFIX_LEN: usize = 7;
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

    for prefix_len in [
        0usize,
        LEGACY_FRAME_PREFIX_LEN,
        FRAME_PREFIX_LEN,
        NIKON_PAYLOAD_OFFSET,
    ] {
        let Some(payload) = prefix.get(prefix_len..) else {
            continue;
        };
        let prefix = match prefix_len {
            0 => "",
            LEGACY_FRAME_PREFIX_LEN => "_after_7_byte_prefix",
            FRAME_PREFIX_LEN => "_after_8_byte_prefix",
            NIKON_PAYLOAD_OFFSET => "_after_4096_byte_prefix",
            _ => "",
        };

        if looks_like_zlib(payload) {
            return match prefix {
                "" => ("zlib", prefix_len),
                "_after_7_byte_prefix" => ("zlib_after_7_byte_prefix", prefix_len),
                "_after_8_byte_prefix" => ("zlib_after_8_byte_prefix", prefix_len),
                "_after_4096_byte_prefix" => ("zlib_after_4096_byte_prefix", prefix_len),
                _ => ("zlib", prefix_len),
            };
        }

        if looks_like_jpeg2000(payload) {
            return match prefix {
                "" => ("jpeg2000", prefix_len),
                "_after_7_byte_prefix" => ("jpeg2000_after_7_byte_prefix", prefix_len),
                "_after_8_byte_prefix" => ("jpeg2000_after_8_byte_prefix", prefix_len),
                "_after_4096_byte_prefix" => ("jpeg2000_after_4096_byte_prefix", prefix_len),
                _ => ("jpeg2000", prefix_len),
            };
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

fn nd2_split_interleaved_channel(
    pixels: &[u8],
    size_x: usize,
    size_y: usize,
    size_c: usize,
    bps: usize,
    channel: usize,
) -> Result<Vec<u8>> {
    if size_c == 0 || channel >= size_c {
        return Err(BioFormatsError::PlaneOutOfRange(channel as u32));
    }
    let row_pixels = size_x
        .checked_mul(size_c)
        .and_then(|samples| samples.checked_mul(bps))
        .ok_or_else(|| BioFormatsError::InvalidData("ND2 row byte size overflow".into()))?;
    let expected = row_pixels
        .checked_mul(size_y)
        .ok_or_else(|| BioFormatsError::InvalidData("ND2 frame byte size overflow".into()))?;
    if pixels.len() < expected {
        return Err(BioFormatsError::InvalidData(format!(
            "ND2 split frame is too short: need {expected} bytes, found {}",
            pixels.len()
        )));
    }
    let mut out = vec![0u8; size_x * size_y * bps];
    let src_step = size_c * bps;
    let channel_offset = channel * bps;
    let mut dst = 0usize;
    for y in 0..size_y {
        let mut src = y * row_pixels + channel_offset;
        for _ in 0..size_x {
            match bps {
                1 => out[dst] = pixels[src],
                2 => {
                    out[dst] = pixels[src];
                    out[dst + 1] = pixels[src + 1];
                }
                4 => {
                    out[dst] = pixels[src];
                    out[dst + 1] = pixels[src + 1];
                    out[dst + 2] = pixels[src + 2];
                    out[dst + 3] = pixels[src + 3];
                }
                _ => out[dst..dst + bps].copy_from_slice(&pixels[src..src + bps]),
            }
            dst += bps;
            src += src_step;
        }
    }
    Ok(out)
}

fn decode_nd2_frame_payload(data: &[u8], expected: usize) -> Result<Vec<u8>> {
    const LEGACY_FRAME_PREFIX_LEN: usize = 7;
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

    for prefix_len in [
        0usize,
        LEGACY_FRAME_PREFIX_LEN,
        FRAME_PREFIX_LEN,
        NIKON_PAYLOAD_OFFSET,
    ] {
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

fn nd2_is_indexed_from_channel_colors(channel_colors: &HashMap<String, i32>) -> bool {
    channel_colors
        .values()
        .any(|&color| color != 0 && color != 0x00ff_ffff)
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
    /// Java `ND2Reader.split`: normal ND2 frames store all channels together,
    /// while logical planes are exposed per channel when sizeC > 1.
    split_channels: bool,
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
    /// Per-plane acquisition timestamps (seconds) from CustomData|AcqTimesCache
    /// (Java: tsT). One entry per global image plane, in ImageDataSeq order.
    ts_t: Vec<f64>,
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
    /// Position suffixes from pPosName (Java: handler.posNames).
    position_names: Vec<String>,
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
            ts_t: Vec::new(),
            exposure_time: Vec::new(),
            channel_colors: HashMap::new(),
            text_channel_names: Vec::new(),
            colors: Vec::new(),
            pos_x: Vec::new(),
            pos_y: Vec::new(),
            pos_z: Vec::new(),
            position_names: Vec::new(),
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
            split_channels: false,
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

        let last_codestream_offset = planes
            .last()
            .map(|plane| plane.data_offset)
            .unwrap_or_default();
        let mut metadata_text = old_nd2_metadata_text(&mut reader, last_codestream_offset)
            .map_err(BioFormatsError::Io)?;
        if old_nd2_component_count(&metadata_text, jp2_bands) <= jp2_bands as u32
            && nd2_xml_ui_count_for_runtype(&metadata_text, "XYPosLoop").unwrap_or(1) <= 1
            && planes.len() > 1
        {
            let tail_text = old_nd2_metadata_tail_text(&mut reader).map_err(BioFormatsError::Io)?;
            if old_nd2_component_count(&tail_text, jp2_bands)
                > old_nd2_component_count(&metadata_text, jp2_bands)
                || nd2_xml_ui_count_for_runtype(&tail_text, "XYPosLoop").unwrap_or(1)
                    > nd2_xml_ui_count_for_runtype(&metadata_text, "XYPosLoop").unwrap_or(1)
            {
                metadata_text = tail_text;
            }
        }
        let metadata_indexes = old_nd2_metadata_indexes(&metadata_text);
        let size_c = old_nd2_component_count(&metadata_text, jp2_bands);
        let is_rgb = jp2_bands > 1;
        let effective_size_c = if is_rgb { 1 } else { size_c.max(1) };
        let mut lv = Nd2LvValues::default();
        parse_nd2_xml_metadata(&metadata_text, &mut lv);
        let old_jp2_position_names = nd2_xml_old_jp2_valid_position_names(&metadata_text);
        let old_jp2_channel_groups = nd2_xml_metadata_channel_groups(&metadata_text);
        // Java's old-JP2 path feeds the trailing XML through ND2Handler only.
        // ND2Handler does not promote sDescription or optical-filter spectrum
        // XML elements to channel names/wavelengths there; those are populated
        // only from text "Name"/"Emission wavelength"/"Excitation wavelength"
        // keys or binary LV text-channel handling in the newer path.
        lv.channel_names.clear();
        lv.emission_wavelengths.clear();
        lv.excitation_wavelengths.clear();
        lv.channel_colors.clear();
        lv.text_channel_names.clear();
        parse_old_nd2_text_info_items(&metadata_text, &mut lv);
        if !old_jp2_channel_groups.is_empty() {
            let best_channels = old_jp2_channel_groups
                .iter()
                .max_by_key(|channels| channels.len())
                .unwrap();
            if lv.channel_names.len() < best_channels.len() {
                lv.channel_names = best_channels
                    .iter()
                    .map(|channel| channel.name.clone())
                    .collect();
            }
            if lv.emission_wavelengths.len() < best_channels.len() {
                lv.emission_wavelengths.clear();
                for channel in best_channels {
                    if let Some(value) = channel.emission_wavelength {
                        lv.emission_wavelengths.push(value);
                    }
                }
            }
            if lv.excitation_wavelengths.len() < best_channels.len() {
                lv.excitation_wavelengths.clear();
                for channel in best_channels {
                    if let Some(value) = channel.excitation_wavelength {
                        lv.excitation_wavelengths.push(value);
                    }
                }
            }
            for channel in best_channels {
                if let Some(color) = channel.color {
                    lv.channel_colors
                        .entry(channel.name.clone())
                        .or_insert(color);
                }
            }
        }
        let mut usable_plane_count = planes.len();
        if !is_rgb && size_c > 1 && usable_plane_count % size_c as usize == 1 {
            usable_plane_count -= 1;
        }
        usable_plane_count -= usable_plane_count % effective_size_c as usize;
        if usable_plane_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "ND2: old JP2-backed file has no complete component planes".into(),
            ));
        }

        let metadata_count = metadata_indexes.len();
        let xml_series = nd2_xml_ui_count_for_runtype(&metadata_text, "XYPosLoop").unwrap_or(1);
        let xml_size_z = nd2_xml_ui_count_for_runtype(&metadata_text, "ZStackLoop").unwrap_or(1);
        let xml_size_t = nd2_xml_ui_count_for_runtype(&metadata_text, "TimeLoop").unwrap_or(1);
        let complete_frame_count = usable_plane_count / effective_size_c as usize;
        let timestamp_frame_count = xml_values(&metadata_text, "dTimeMSec")
            .into_iter()
            .filter_map(|value| value.parse::<f64>().ok())
            .filter(|value| value.is_finite())
            .take(complete_frame_count)
            .count();

        // Old JP2 mirrors ND2Handler's SAX loop counts first, then ND2Reader's
        // final series cap. If those loop counts cannot consume the available
        // planes, keep Java's parsed XY count and derive T from dTimeMSec entries.
        let mut used_xml_loop_dimensions = false;
        let (mut series_count, size_z, size_t) = {
            let mut series_count = xml_series as usize;
            let size_z = xml_size_z;
            let size_t = xml_size_t;
            let nplanes = (size_z as usize).saturating_mul(effective_size_c as usize);
            let expected = series_count
                .saturating_mul(nplanes)
                .saturating_mul(size_t as usize);
            if (xml_series > 1 || xml_size_z > 1 || xml_size_t > 1)
                && !(xml_series <= 1 && expected != usable_plane_count)
                && !(expected > usable_plane_count && size_t <= 1)
            {
                if expected <= usable_plane_count {
                    used_xml_loop_dimensions = true;
                } else if nplanes > 0 && size_t > 0 {
                    let capped_series = usable_plane_count / (nplanes * size_t as usize);
                    used_xml_loop_dimensions = capped_series > 0
                        && capped_series
                            .saturating_mul(nplanes)
                            .saturating_mul(size_t as usize)
                            == usable_plane_count;
                }
            }

            if used_xml_loop_dimensions {
                (series_count, size_z, size_t)
            } else {
                series_count = (xml_series > 1)
                    .then_some(xml_series as usize)
                    .filter(|&count| {
                        timestamp_frame_count > 0
                            && timestamp_frame_count % count == 0
                            && timestamp_frame_count / count > 0
                    })
                    .unwrap_or_else(|| {
                        if metadata_count > 1
                            && usable_plane_count == metadata_count * effective_size_c as usize
                        {
                            metadata_count
                        } else {
                            1
                        }
                    });
                let size_t = if timestamp_frame_count > 0
                    && timestamp_frame_count % series_count == 0
                {
                    (timestamp_frame_count / series_count).max(1) as u32
                } else {
                    (usable_plane_count / series_count / effective_size_c as usize).max(1) as u32
                };
                (series_count, 1, size_t)
            }
        };
        let nplanes = size_z as usize * effective_size_c as usize;
        if nplanes > 0 && size_t > 0 {
            let java_used = series_count
                .saturating_mul(nplanes)
                .saturating_mul(size_t as usize);
            if java_used > usable_plane_count {
                series_count = usable_plane_count / (nplanes * size_t as usize);
            }
            usable_plane_count = series_count
                .saturating_mul(nplanes)
                .saturating_mul(size_t as usize);
        }
        let image_count = size_z * size_t * effective_size_c;
        let (plane_delta_t, plane_position_z) =
            old_nd2_plane_metadata(&metadata_text, usable_plane_count, effective_size_c);
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
        let mut source_series = vec![Vec::with_capacity(image_count as usize); series_count];
        for t in 0..size_t as usize {
            for series in 0..series_count {
                for q in 0..nplanes {
                    let source = (t * series_count + series) * nplanes + q;
                    if source < usable_plane_count {
                        plane_series[series].push(planes[source].clone());
                        source_series[series].push(source);
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
            if is_rgb {
                series_metadata.insert(
                    "nd2_rgb_channel_count".into(),
                    MetadataValue::Int(size_c as i64),
                );
            }

            metas.push(ImageMetadata {
                size_x,
                size_y,
                size_z,
                size_c,
                size_t,
                pixel_type,
                bits_per_pixel,
                image_count,
                dimension_order,
                is_rgb,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: false,
                resolution_count: 1,
                thumbnail: false,
                series_metadata,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
        }

        self.meta = metas;
        self.current_series = 0;
        // Old JP2 XML can carry non-pixel-size dCalibration values; Java's
        // old SAX path does not project the 50.0 value in but3_cont200-1.nd2
        // into OME PhysicalSizeX/Y. Keep only microscope-scale XY pixel sizes
        // on this legacy path; the modern binary trueSizeX/Y path is unchanged.
        self.physical_size = lv
            .calibration
            .filter(|v| *v > 0.0 && (*v < 10.0 || (size_c == 1 && (*v - 100.0).abs() < 1.0e-9)));
        self.physical_size_z = lv.z_step;
        self.channel_names = lv.channel_names;
        self.emission_wavelengths = lv.emission_wavelengths;
        self.excitation_wavelengths = lv.excitation_wavelengths;
        self.backup_channel_names.clear();
        self.backup_emission_wavelengths.clear();
        self.backup_excitation_wavelengths.clear();
        self.exposure_time = lv.exposure_time;
        self.channel_colors = lv.channel_colors;
        self.text_channel_names = lv.text_channel_names;
        self.colors = (0..size_c as usize)
            .map(|c| {
                self.channel_names
                    .get(c)
                    .and_then(|name| self.channel_colors.get(name))
                    .copied()
                    .unwrap_or(0)
            })
            .collect();
        self.pos_x = lv.pos_x;
        self.pos_y = lv.pos_y;
        self.pos_z = lv.pos_z;
        self.position_names = old_jp2_position_names;
        self.position_count = lv.position_count;
        self.lens_na = lv.lens_na;
        self.objective_mag = lv.objective_mag;
        self.objective_model = lv.objective_model;
        self.refractive_index = lv.refractive_index;
        self.is_lossless = lv.is_lossless;
        self.n_x_fields = if lv.n_x_fields > 6 { 0 } else { lv.n_x_fields };
        self.old_jp2_planes = plane_series;
        self.split_channels = false;
        self.image_chunks.clear();
        self.series_image_chunks.clear();
        self.series_plane_offsets.clear();
        self.series_source_planes = source_series;
        self.chunks.clear();
        self.plane_delta_t = plane_delta_t;
        self.plane_position_z = plane_position_z;
        self.ts_t.clear();
        self.pfs_offset = 0;
        self.pfs_state_offset = 0;
        reader
            .seek(SeekFrom::Start(0))
            .map_err(BioFormatsError::Io)?;
        self.file = Some(reader);
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn current_meta_checked(&self, plane_index: u32) -> Result<&ImageMetadata> {
        let meta = self
            .meta
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Ok(meta)
    }

    fn normal_frame_chunk_for_plane(&self, plane_index: u32) -> Result<&Nd2Chunk> {
        let meta = self.current_meta_checked(plane_index)?;
        let series_chunks = self
            .series_image_chunks
            .get(self.current_series)
            .unwrap_or(&self.image_chunks);
        let stored_plane_index = if self.split_channels && meta.size_c > 1 {
            plane_index / meta.size_c
        } else {
            plane_index
        };
        let chunk_idx = series_chunks
            .get(stored_plane_index as usize)
            .copied()
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        self.chunks
            .get(chunk_idx)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))
    }

    fn read_normal_frame_data(&self, chunk: &Nd2Chunk) -> Result<Vec<u8>> {
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut reader = BufReader::new(File::open(path).map_err(BioFormatsError::Io)?);
        read_chunk_data(&mut reader, chunk).map_err(BioFormatsError::Io)
    }

    fn nd2_compressed_payload_for_plane(
        &self,
        plane_index: u32,
    ) -> Result<(Vec<u8>, Jpeg2000Container)> {
        self.current_meta_checked(plane_index)?;
        if !self.old_jp2_planes.is_empty() {
            return self
                .old_jp2_planes
                .get(self.current_series)
                .and_then(|planes| planes.get(plane_index as usize))
                .map(|_| (Vec::new(), Jpeg2000Container::Codestream))
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let chunk = self.normal_frame_chunk_for_plane(plane_index)?;
        let data = self.read_normal_frame_data(chunk)?;
        nd2_compressed_jpeg2000_payload(&data).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(
                "ND2 frame is not a clean whole-frame JPEG2000 payload".into(),
            )
        })
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
        let old_jp2_signature = read >= 4 && header[..4] == [0xff, 0x4f, 0xff, 0x51]
            || read >= 8
                && u32::from_be_bytes(header[..4].try_into().unwrap_or([0; 4])) == 12
                && &header[4..8] == b"jP  ";
        if old_jp2_signature {
            return self.set_old_jp2_id(reader, path);
        }
        let chunks = match read_chunk_map(&mut reader).map_err(BioFormatsError::Io)? {
            Some(chunks) => chunks,
            None => scan_chunks(&mut reader).map_err(BioFormatsError::Io)?,
        };

        let (mut size_x, mut size_y, mut size_c, mut size_z, mut bpp) =
            (0u32, 0u32, 1u32, 1u32, 0u16);
        let mut storage_bpp: Option<u16> = None;
        let mut loop_size_z: Option<u32> = None;
        let mut loop_size_t: Option<u32> = None;
        let mut loop_series_count: Option<u32> = None;
        let mut loop_descriptors = Vec::new();
        let mut position_names = Vec::new();
        let mut has_spectral_loop = false;
        let mut has_declared_ndcontrol_loop_dimensions = false;
        let mut has_ndcontrol_dimension_order = false;

        for ac in chunks
            .iter()
            .filter(|c| c.name.starts_with("ImageAttributes"))
        {
            let data = read_chunk_data(&mut reader, ac).map_err(BioFormatsError::Io)?;
            // Data may be a raw binary struct OR XML wrapped. Java handles the
            // flat binary attributes in this early block before falling back to
            // inferred dimensions, so try both representations here.
            let xml = String::from_utf8_lossy(&data);
            let (w, h, c, z, b) = parse_nd2_attributes(&xml);
            if b > 0 {
                bpp = b;
            }
            if w > 0 && h > 0 {
                size_x = w;
                size_y = h;
                if c > 0 {
                    size_c = c;
                }
                if z > 0 {
                    size_z = z;
                }
                storage_bpp = nd2_storage_bpp_value(&xml);
                nd2_update_loop_counts_from_xml(
                    &xml,
                    &mut loop_size_z,
                    &mut loop_size_t,
                    &mut loop_series_count,
                );
                if let Some(count) = nd2_xml_xy_position_count_with_valid_flags(&xml) {
                    if count > 1 {
                        loop_series_count = Some(count);
                    }
                }
                let has_ndcontrol_loop = nd2_xml_ndcontrol_loop_dimensions(&xml).is_some();
                has_declared_ndcontrol_loop_dimensions |= has_ndcontrol_loop;
                has_ndcontrol_dimension_order |= has_ndcontrol_loop;
                nd2_update_loop_descriptors_from_xml(&xml, &mut loop_descriptors);
                nd2_replace_position_names_if_more_informative(
                    &mut position_names,
                    nd2_xml_old_jp2_valid_position_names(&xml),
                );
                has_spectral_loop |= !nd2_xml_loop_container_counts(&xml, "SpectLoop").is_empty();
                break;
            }
            let mut attrs = Nd2LvValues::default();
            parse_nd2_lv(&data, &mut attrs);
            parse_nd2_binary_image_attributes(&data, &mut attrs);
            let attr_size_x = attrs.lv_size_x.or(attrs.attr_size_x);
            let attr_size_y = attrs.lv_size_y.or(attrs.attr_size_y);
            let attr_size_c = attrs.lv_size_c.or(attrs.attr_size_c);
            let attr_storage_bpp = attrs.lv_bpc_in_memory.or(attrs.attr_bpc_in_memory);
            let attr_bpp = attrs
                .lv_bpc_significant
                .or_else(|| {
                    attrs
                        .attr_bpc_significant
                        .filter(|_| attr_storage_bpp.is_none())
                })
                .or(attr_storage_bpp);
            if let (Some(w), Some(h)) = (attr_size_x, attr_size_y) {
                size_x = w;
                size_y = h;
                if let Some(c) = attr_size_c.filter(|&c| c > 0) {
                    size_c = c;
                }
                if let Some(b) = attr_bpp.filter(|&b| b > 0) {
                    bpp = b;
                }
                storage_bpp = attr_storage_bpp.filter(|&b| b > 0);
                break;
            }
        }

        for mc in chunks.iter().filter(|c| {
            c.name.starts_with("ImageMetadata")
                || c.name.contains("GrabberCameraSettings")
                || c.name.starts_with("CustomDataVar|NDControl")
        }) {
            let data = read_chunk_data(&mut reader, mc).map_err(BioFormatsError::Io)?;
            let xml = String::from_utf8_lossy(&data);
            nd2_update_loop_counts_from_xml(
                &xml,
                &mut loop_size_z,
                &mut loop_size_t,
                &mut loop_series_count,
            );
            if let Some(count) = nd2_xml_xy_position_count_with_valid_flags(&xml) {
                if count > 1 {
                    loop_series_count = Some(count);
                }
            }
            let has_ndcontrol_loop = nd2_xml_ndcontrol_loop_dimensions(&xml).is_some();
            has_declared_ndcontrol_loop_dimensions |= has_ndcontrol_loop;
            has_ndcontrol_dimension_order |= has_ndcontrol_loop;
            nd2_update_loop_descriptors_from_xml(&xml, &mut loop_descriptors);
            nd2_replace_position_names_if_more_informative(
                &mut position_names,
                nd2_xml_old_jp2_valid_position_names(&xml),
            );
            has_spectral_loop |= !nd2_xml_loop_container_counts(&xml, "SpectLoop").is_empty();
            // Only a fallback for files where ImageAttributesLV didn't already
            // establish dimensions: Java's ND2Reader never reads
            // GrabberCameraSettings/CustomDataVar chunks for uiWidth/uiHeight/
            // uiComp/pixel-type at all, and this chunk's camera-hardware
            // `rectSensorUser`/`uiComp`/bpp fields describe the physical sensor
            // readout, not the logical multi-channel experiment structure, so
            // they must never override values already found from
            // ImageAttributesLV (matches the size_x==0/size_y==0/size_c==1
            // guards on the sibling fallbacks below).
            if size_x == 0 {
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
                    storage_bpp = nd2_storage_bpp_value(&xml);
                    break;
                }
            }
            if size_x == 0 {
                if let Some(w) = nd2_u32_value(&xml, "uiCamPxlCountX").filter(|&w| w > 0) {
                    size_x = w;
                }
            }
            if size_y == 0 {
                if let Some(h) = nd2_u32_value(&xml, "uiCamPxlCountY").filter(|&h| h > 0) {
                    size_y = h;
                }
            }
            if size_c == 1 {
                if let Some(c) = nd2_u32_value(&xml, "uiComp").filter(|&c| c > 0) {
                    size_c = c;
                }
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
        let has_image_metadata_lv_chunk =
            chunks.iter().any(|c| c.name.starts_with("ImageMetadataLV"));

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

        let storage_bits_for_pixel_type = storage_bpp
            .filter(|&bits| bits == 8 || bits == 16 || bits == 32)
            .or_else(|| (bpp == 8 || bpp == 16 || bpp == 32).then_some(bpp))
            .unwrap_or(8);
        let pixel_type = match storage_bits_for_pixel_type {
            8 => PixelType::Uint8,
            16 => PixelType::Uint16,
            32 => PixelType::Float32,
            _ => PixelType::Uint16,
        };
        if bpp == 0 {
            bpp = (pixel_type.bytes_per_sample() * 8) as u16;
        }

        // Parse the Nikon LV binary metadata tree (ImageMetadataSeqLV /
        // ImageCalibrationLV) for OME attributes: physical pixel size, channel
        // names, emission wavelengths. Matches ND2Reader.iterateIn in Java.
        //
        // Binary ImageMetadataLV eType/uiCount walk (ND2Reader.initFile
        // java:967-1062). Builds imageMetadataLVOrder (M/T/Z) and the T/Z/M
        // counts directly from the binary metadata. Java guards this with
        // `!imageMetadataLVProcessed`, so only the FIRST block whose name starts
        // with "ImageMetadat" is walked. Keep this in the same metadata pass so
        // we do not read/scan the same metadata block again.
        let mut lv = Nd2LvValues::default();
        let mut image_metadata_lv = ImageMetadataLv::default();
        let mut image_metadata_z_count_candidate: Option<u32> = None;
        let mut image_metadata_lv_found = false;
        for ac in chunks
            .iter()
            .filter(|c| c.name.starts_with("ImageAttributes"))
        {
            if let Ok(data) = read_chunk_data(&mut reader, ac) {
                parse_nd2_lv(&data, &mut lv);
                parse_nd2_binary_image_attributes(&data, &mut lv);
            }
        }
        for mc in chunks.iter().filter(|c| {
            c.name.starts_with("ImageMetadataSeq")
                || c.name.starts_with("ImageMetadata")
                || c.name.starts_with("ImageCalibration")
                || c.name.starts_with("ImageText")
                || c.name.starts_with("CustomDataVar|NDControl")
                || c.name.contains("GrabberCameraSettings")
        }) {
            if let Ok(data) = read_chunk_data(&mut reader, mc) {
                parse_nd2_lv(&data, &mut lv);
                let xml = String::from_utf8_lossy(&data);
                parse_nd2_xml_metadata(&xml, &mut lv);
                if mc.name.starts_with("ImageMetadat") {
                    let scan_data = image_metadata_lv_scan_bytes(&mc.name, &data);
                    if let Some(result) = parse_image_metadata_lv(&scan_data) {
                        if !scan_data.starts_with(b"<?xml")
                            && (result.processed
                                || result.current_count_set
                                || !result.order.is_empty())
                        {
                            image_metadata_lv_found = true;
                        }
                        if result.current_count_set && result.z_count > 1 {
                            image_metadata_z_count_candidate = Some(
                                image_metadata_z_count_candidate
                                    .unwrap_or(1)
                                    .max(result.z_count as u32),
                            );
                        }
                        if result.processed && !image_metadata_lv.processed {
                            image_metadata_lv = result;
                        }
                    }
                }
                nd2_update_loop_counts_from_xml(
                    &xml,
                    &mut loop_size_z,
                    &mut loop_size_t,
                    &mut loop_series_count,
                );
                if let Some(count) = nd2_xml_xy_position_count_with_valid_flags(&xml) {
                    if count > 1 {
                        loop_series_count = Some(count);
                    }
                }
                let has_ndcontrol_loop = nd2_xml_ndcontrol_loop_dimensions(&xml).is_some();
                has_declared_ndcontrol_loop_dimensions |= has_ndcontrol_loop;
                has_ndcontrol_dimension_order |= has_ndcontrol_loop;
                nd2_update_loop_descriptors_from_xml(&xml, &mut loop_descriptors);
                nd2_replace_position_names_if_more_informative(
                    &mut position_names,
                    nd2_xml_old_jp2_valid_position_names(&xml),
                );
                has_spectral_loop |= !nd2_xml_loop_container_counts(&xml, "SpectLoop").is_empty();
            }
        }
        for text in lv.text_infos.clone() {
            if let Some(xml) = nd2_text_xml_fragment(&text) {
                parse_nd2_xml_metadata(&xml, &mut lv);
                nd2_update_loop_counts_from_xml(
                    &xml,
                    &mut loop_size_z,
                    &mut loop_size_t,
                    &mut loop_series_count,
                );
                if let Some(count) = nd2_xml_xy_position_count_with_valid_flags(&xml) {
                    if count > 1 {
                        loop_series_count = Some(count);
                    }
                }
                let has_ndcontrol_loop = nd2_xml_ndcontrol_loop_dimensions(&xml).is_some();
                has_declared_ndcontrol_loop_dimensions |= has_ndcontrol_loop;
                has_ndcontrol_dimension_order |= has_ndcontrol_loop;
                nd2_update_loop_descriptors_from_xml(&xml, &mut loop_descriptors);
                nd2_replace_position_names_if_more_informative(
                    &mut position_names,
                    nd2_xml_old_jp2_valid_position_names(&xml),
                );
                has_spectral_loop |= !nd2_xml_loop_container_counts(&xml, "SpectLoop").is_empty();
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
            if let Some(z) = candidate.text_size_z.filter(|&z| z > 0) {
                if loop_size_z.is_none_or(|current| current <= 1 || z < current) {
                    loop_size_z = Some(z);
                }
            }
            if let Some(t) = candidate.text_size_t.filter(|&t| t > 0) {
                if loop_size_t.is_none_or(|current| current <= 1 || t < current) {
                    loop_size_t = Some(t);
                }
            }
            if let Some(count) = candidate.text_series_count.filter(|&count| count > 1) {
                if loop_series_count.is_none_or(|current| current <= 1) {
                    loop_series_count = Some(count);
                }
            }
            if backup.channel_names.is_empty() {
                backup = candidate;
            }
        }
        self.backup_channel_names = backup.channel_names;
        self.backup_emission_wavelengths = backup.emission_wavelengths;
        self.backup_excitation_wavelengths = backup.excitation_wavelengths;
        if let Some(z) = lv.text_size_z.filter(|&z| z > 0) {
            if loop_size_z.is_none_or(|current| current <= 1) {
                loop_size_z = Some(z);
            }
        }
        if let Some(t) = lv.text_size_t.filter(|&t| t > 0) {
            if loop_size_t.is_none_or(|current| current <= 1 || t < current) {
                loop_size_t = Some(t);
            }
        }
        if let Some(count) = lv.text_series_count.filter(|&count| count > 1) {
            if loop_series_count.is_none_or(|current| current <= 1) {
                loop_series_count = Some(count);
            }
        }
        if let Some(w) = lv.lv_size_x.filter(|&w| w > 0) {
            size_x = w;
        }
        if let Some(h) = lv.lv_size_y.filter(|&h| h > 0) {
            size_y = h;
        }
        let z_count_from_range = nd2_z_count_from_range(lv.z_high, lv.z_low, lv.z_step);

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
        self.position_names = position_names;
        self.position_count = lv.position_count;
        self.lens_na = lv.lens_na;
        self.objective_mag = lv.objective_mag;
        self.objective_model = lv.objective_model;
        self.refractive_index = lv.refractive_index;
        self.is_lossless = lv.is_lossless;
        // ND2Reader caps an implausible field count to zero (>6 ⇒ 0).
        self.n_x_fields = if lv.n_x_fields > 6 { 0 } else { lv.n_x_fields };

        // CustomData|X/Y/Z/P offsets (ND2Reader.initFile java:1109-1128). Java
        // points these at the *last* imageOffsets.size() values within each
        // block's payload: doubleOffset = fp + 8*(len/8 - imageOffsets.size())
        // for X/Y/Z (doubles), intOffset = fp + 4*(len/4 - imageOffsets.size())
        // for the PFS P-blocks (ints). `fp` is the payload start (chunk.data_offset)
        // and `len` is the payload length (chunk.data_length).
        let n_image_offsets = image_chunks.len() as u64;
        let double_offset = |chunk: &Nd2Chunk| -> u64 {
            let n_doubles = chunk.data_length / 8;
            chunk.data_offset + 8 * n_doubles.saturating_sub(n_image_offsets)
        };
        let int_offset = |chunk: &Nd2Chunk| -> u64 {
            let n_ints = chunk.data_length / 4;
            chunk.data_offset + 4 * n_ints.saturating_sub(n_image_offsets)
        };

        // zOffset takes the first CustomData|Z block (java:1109-1112).
        let mut x_offset = 0u64;
        let mut y_offset = 0u64;
        let mut z_offset = 0u64;
        self.pfs_offset = 0;
        self.pfs_state_offset = 0;
        for chunk in &chunks {
            if chunk.name.starts_with("CustomData|Z") {
                if z_offset == 0 {
                    z_offset = double_offset(chunk);
                }
            } else if chunk.name.starts_with("CustomData|X") {
                x_offset = double_offset(chunk);
            } else if chunk.name.starts_with("CustomData|Y") {
                y_offset = double_offset(chunk);
            } else if chunk.name.starts_with("CustomData|P") {
                if self.pfs_offset == 0 {
                    self.pfs_offset = int_offset(chunk);
                } else if self.pfs_state_offset == 0 {
                    self.pfs_state_offset = int_offset(chunk);
                }
            }
        }

        // Binary posX/posY/posZ fallback (ND2Reader.initFile java:1554-1598). When
        // the XML handler yielded no stage positions but a CustomData|X/Y/Z block
        // exists, read imageOffsets.size() doubles (µm) from the computed offset.
        // The uniqueX/uniqueY/uniqueZ counters Java derives here feed only the
        // positionCount heuristic at java:1258 (already settled upstream), so we
        // simply populate the position lists.
        let n_offsets = image_chunks.len();
        if self.pos_x.is_empty() && x_offset != 0 {
            self.pos_x =
                read_doubles_at(&mut reader, x_offset, n_offsets).map_err(BioFormatsError::Io)?;
        }
        if self.pos_y.is_empty() && y_offset != 0 {
            self.pos_y =
                read_doubles_at(&mut reader, y_offset, n_offsets).map_err(BioFormatsError::Io)?;
        }
        if self.pos_z.is_empty() && z_offset != 0 {
            self.pos_z =
                read_doubles_at(&mut reader, z_offset, n_offsets).map_err(BioFormatsError::Io)?;
        }

        // PFS Offset / PFS Status global-metadata lists (ND2Reader.initFile
        // java:1599-1610): imageOffsets.size() ints read from pfsOffset/
        // pfsStateOffset. Stored as comma-joined nd2_pfs_offsets / nd2_pfs_status.
        let pfs_offsets = if self.pfs_offset != 0 {
            read_ints_at(&mut reader, self.pfs_offset, n_offsets).map_err(BioFormatsError::Io)?
        } else {
            Vec::new()
        };
        let pfs_status = if self.pfs_state_offset != 0 {
            read_ints_at(&mut reader, self.pfs_state_offset, n_offsets)
                .map_err(BioFormatsError::Io)?
        } else {
            Vec::new()
        };

        // Per-plane acquisition timestamps from the first CustomData|AcqTimesCache
        // block (ND2Reader.initFile:1105-1108, 1789-1812 → tsT). The stream holds
        // one millisecond double per global ImageDataSeq plane.
        self.ts_t = chunks
            .iter()
            .find(|c| c.name.starts_with("CustomData|AcqTimesCache"))
            .map(|chunk| read_acq_times_cache(&mut reader, chunk, image_chunks.len()))
            .transpose()
            .map_err(BioFormatsError::Io)?
            .unwrap_or_default();
        let has_float_comp_range = chunks
            .iter()
            .any(|c| c.name.starts_with("CustomData|FloatCompRange"));
        let has_nikon_sim_acq_data = chunks
            .iter()
            .any(|c| c.name.starts_with("CustomData|NikonSimAcqData"));

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

        // Dimension order: Java ND2Reader builds "XY" + the handler's seed
        // order, then appends any of Z/C/T not already present. Text loop counts
        // and XML loop descriptors do not by themselves make the seed C-first;
        // ImageMetadataLV-present Z/T `uiCount` handlers are explicitly skipped
        // in ND2Handler. The NDControl `LoopSize` branch is different: it sets
        // `dimensionOrder = "CZT"` unconditionally.
        let mut dimension_order = if size_c > 1
            || (has_spectral_loop && loop_size_t.is_some())
            || has_ndcontrol_dimension_order
            || loop_series_count.is_some_and(|count| count > 1)
        {
            DimensionOrder::XYCZT
        } else {
            DimensionOrder::XYZCT
        };

        let has_time_loop_descriptor = loop_descriptors
            .iter()
            .any(|descriptor| descriptor.kind == "TimeLoop");
        let has_z_loop_descriptor = loop_descriptors
            .iter()
            .any(|descriptor| descriptor.kind == "ZStackLoop");
        let mut image_count = image_chunks.len() as u32;
        let mut position_count = loop_series_count.filter(|&count| count > 1).unwrap_or(1);
        if self.position_names.len() > 1 {
            position_count = self.position_names.len() as u32;
        }
        if !has_z_loop_descriptor
            && loop_size_z.is_some_and(|z| z == position_count || Some(z) == loop_series_count)
        {
            loop_size_z = None;
            if size_z == position_count {
                size_z = 1;
            }
        }
        if !has_time_loop_descriptor
            && loop_size_t.is_some_and(|t| t == position_count || Some(t) == loop_series_count)
        {
            loop_size_t = None;
        }
        let mut size_t = 1u32;

        // Validate the binary ImageMetadataLV result (ND2Reader.initFile
        // java:1135-1141): apply it only when a count was actually set, an order
        // was produced, and either there are no image offsets or
        // timeCount * zCount * XYCount equals the offset count. Otherwise Java
        // clears imageMetadataLVProcessed and falls back to the XML/heuristic path.
        if image_metadata_lv.current_count_set
            && !image_metadata_lv.order.is_empty()
            && (image_count == 0
                || (image_metadata_lv.time_count.max(0) as u64)
                    .saturating_mul(image_metadata_lv.z_count.max(0) as u64)
                    .saturating_mul(image_metadata_lv.xy_count.max(0) as u64)
                    == image_count as u64)
        {
            // setDimensions(timeCount, zCount, XYCount): sizeT=numT, sizeZ=numZ,
            // and when numSeries>1 the file is split into numSeries series
            // (java:2810-2839).
            size_t = (image_metadata_lv.time_count.max(0) as u32).max(1);
            size_z = (image_metadata_lv.z_count.max(0) as u32).max(1);
            position_count = (image_metadata_lv.xy_count.max(0) as u32).max(1);
        } else {
            image_metadata_lv.processed = false;
            if let Some(t) = loop_size_t {
                size_t = t.max(1);
            }
            if position_count > 1 && size_z <= 1 {
                let complete_t = image_chunks.len() as u32 / position_count.max(1);
                if complete_t > 0 && complete_t < size_t {
                    size_t = complete_t;
                }
            }
            let z_candidate_matches_image_count =
                image_metadata_z_count_candidate.is_some_and(|z| {
                    image_count > 0
                        && image_count
                            % position_count
                                .max(1)
                                .saturating_mul(size_t.max(1))
                                .saturating_mul(z.max(1))
                            == 0
                });
            if z_candidate_matches_image_count {
                size_z = image_metadata_z_count_candidate.unwrap().max(1);
            } else if let Some(z) = loop_size_z {
                size_z = z.max(1);
            } else if let Some(z) = image_metadata_z_count_candidate {
                size_z = z.max(1);
            } else if let Some(z) = z_count_from_range {
                size_z = z.max(1);
            }
            if size_c > 1 && position_count > 1 {
                let denom = position_count
                    .max(1)
                    .saturating_mul(size_t.max(1))
                    .saturating_mul(size_c.max(1));
                if denom > 0 && image_count > 0 && image_count % denom == 0 {
                    let inferred_z = image_count / denom;
                    if inferred_z > size_z {
                        size_z = inferred_z;
                    }
                }
            }
            if size_c > 1 && position_count > 1 && size_z > 1 {
                let denom = position_count
                    .max(1)
                    .saturating_mul(size_z.max(1))
                    .saturating_mul(size_c.max(1));
                if denom > 0 && image_count > 0 && image_count % denom == 0 {
                    size_t = (image_count / denom).max(1);
                }
            }
            if image_count > 0 && image_count % position_count.max(1) == 0 {
                let per_series_planes = image_count / position_count;
                let count = if size_c > 1 {
                    per_series_planes
                } else if per_series_planes >= size_c.max(1) {
                    per_series_planes / size_c.max(1)
                } else {
                    per_series_planes
                };
                let zt = size_z.saturating_mul(size_t);
                if count > zt && count - zt == size_z {
                    size_t = size_t.saturating_add(1);
                }
            }
            let expected_planes = size_z
                .saturating_mul(size_t)
                .saturating_mul(position_count.max(1));
            let has_explicit_plane_count_evidence = loop_size_z.is_some()
                || loop_size_t.is_some()
                || image_metadata_z_count_candidate.is_some()
                || z_count_from_range.is_some();
            if size_c > 1
                && position_count <= 1
                && expected_planes > 0
                && expected_planes <= image_count
                && has_explicit_plane_count_evidence
            {
                image_count = expected_planes;
            }
            if has_declared_ndcontrol_loop_dimensions
                && expected_planes > 0
                && expected_planes < image_count
            {
                image_count = expected_planes;
            }
            if image_count > 0
                && expected_planes != image_count
                && !has_declared_ndcontrol_loop_dimensions
            {
                let positioned_z_planes = position_count.max(1).saturating_mul(size_z.max(1));
                if position_count > 1
                    && size_z > 1
                    && positioned_z_planes > 0
                    && image_count % positioned_z_planes == 0
                {
                    size_t = (image_count / positioned_z_planes).max(1);
                } else if size_t > 1 && image_count % size_t == 0 {
                    size_z = (image_count / size_t).max(1);
                } else if size_z > 1 && image_count % size_z == 0 {
                    size_t = (image_count / size_z).max(1);
                } else if loop_size_z.is_some() || loop_size_t.is_some() {
                    size_z = 1;
                    size_t = image_count.max(1);
                }
            }
        }
        if !image_metadata_lv.processed
            && loop_size_z.is_none()
            && loop_size_t.is_none()
            && position_count <= 1
            && image_count > 1
            && size_z == 1
            && size_t == 1
        {
            // Java fallback when no Z/T metadata was established: sizeZ is set
            // to 1 and sizeT becomes imageOffsets.size()/seriesCount.
            size_t = image_count;
        }
        if size_c > 1
            && position_count <= 1
            && loop_size_z.is_none()
            && loop_size_t == Some(size_t)
            && image_count > 0
            && image_count == size_z
        {
            size_t = 1;
        }
        if size_c == 1
            && !image_metadata_lv_found
            && !has_image_metadata_lv_chunk
            && !has_ndcontrol_dimension_order
            && !(loop_size_z.is_some() && loop_size_t.is_some())
            && !loop_descriptors.is_empty()
        {
            let z_index = loop_descriptors
                .iter()
                .position(|descriptor| descriptor.kind == "ZStackLoop");
            let t_index = loop_descriptors
                .iter()
                .position(|descriptor| descriptor.kind == "TimeLoop");
            if let (Some(z_index), Some(t_index)) = (z_index, t_index) {
                dimension_order = if t_index < z_index {
                    DimensionOrder::XYZTC
                } else {
                    DimensionOrder::XYTZC
                };
            }
        }
        if size_c == 1
            && size_t > 1
            && !has_ndcontrol_dimension_order
            && loop_size_t.is_some_and(|declared_t| declared_t != size_t)
            && loop_descriptors
                .first()
                .is_some_and(|descriptor| descriptor.kind == "TimeLoop")
        {
            dimension_order = DimensionOrder::XYZTC;
        }
        let mut too_small_row_corrected = false;
        if image_chunks.len() > 1 && size_x > 0 && size_y > 0 && size_c > 0 {
            let first = &chunks[image_chunks[0]];
            let second = &chunks[image_chunks[1]];
            // Java computes availableBytes from the distance between consecutive
            // image offsets. In this map-reader representation the comparable
            // offset is block_offset + 16, i.e. after magic/nameLen/dataLen.
            let available_bytes = second
                .block_offset
                .saturating_add(16)
                .saturating_sub(first.block_offset.saturating_add(16));
            let row_size = u64::from(size_x)
                .saturating_mul(u64::from(size_c.max(1)))
                .saturating_mul(pixel_type.bytes_per_sample() as u64);
            let first_encoding = read_chunk_prefix(&mut reader, first, 8192)
                .ok()
                .map(|prefix| {
                    let bps = pixel_type.bytes_per_sample();
                    let stored_expected = if size_c > 1 {
                        let scanline_pad = if size_x % 2 != 0 && size_c % 2 != 0 {
                            1usize
                        } else {
                            0usize
                        };
                        (size_x as usize + scanline_pad)
                            .saturating_mul(size_y as usize)
                            .saturating_mul(size_c as usize)
                            .saturating_mul(bps)
                    } else {
                        let scanline_pad = ((bps * size_x as usize) % 4) / bps;
                        (size_x as usize + scanline_pad)
                            .saturating_mul(size_y as usize)
                            .saturating_mul(bps)
                    };
                    nd2_frame_payload_layout(&prefix, first.data_length as usize, stored_expected)
                        .0
                        .to_string()
                })
                .unwrap_or_default();
            let too_small_row_candidate = first_encoding == "too_small" && size_y > 1000;
            let java_style_row_correction =
                first_encoding.starts_with("raw") || too_small_row_candidate;
            if java_style_row_correction && row_size > 0 {
                let corrected_size_y = (available_bytes / row_size) as u32;
                if corrected_size_y > 0 && corrected_size_y < size_y {
                    size_y = corrected_size_y;
                    too_small_row_corrected = too_small_row_candidate;
                }
            }
        }
        if too_small_row_corrected {
            self.channel_colors.clear();
            self.colors.clear();
            self.text_channel_names.clear();
        }
        let mut series_metadata: HashMap<String, MetadataValue> = HashMap::new();
        series_metadata.insert("nd2_chunks".into(), MetadataValue::Int(chunks.len() as i64));
        series_metadata.insert(
            "nd2_image_data_chunks".into(),
            MetadataValue::Int(image_chunks.len() as i64),
        );
        let mut plane_delta_t = vec![None; image_count as usize];
        let mut plane_position_z = vec![None; image_count as usize];
        let eager_plane_metadata =
            image_chunks.len() <= ND2_EAGER_PLANE_METADATA_LIMIT || position_count > 1;
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
            let mut image_data_encodings = Vec::new();
            let mut image_data_payload_offsets = Vec::new();
            let mut image_data_chunk_tables = Vec::new();
            let mut image_data_chunk_table_ranges = Vec::new();
            let mut image_data_timestamps = Vec::new();
            let bps = pixel_type.bytes_per_sample();
            let stored_expected = if size_c > 1 {
                let scanline_pad = if size_x % 2 != 0 && size_c % 2 != 0 {
                    1usize
                } else {
                    0usize
                };
                (size_x as usize + scanline_pad)
                    .saturating_mul(size_y as usize)
                    .saturating_mul(size_c as usize)
                    .saturating_mul(bps)
            } else {
                let scanline_pad = ((bps * size_x as usize) % 4) / bps;
                (size_x as usize + scanline_pad)
                    .saturating_mul(size_y as usize)
                    .saturating_mul(bps)
            };
            if eager_plane_metadata {
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
                image_data_encodings.reserve(image_chunks.len());
                image_data_payload_offsets.reserve(image_chunks.len());
                for (plane, &chunk_index) in image_chunks.iter().enumerate() {
                    let chunk = &chunks[chunk_index];
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
                        if let Some(timestamp) =
                            nd2_prefix_timestamp_seconds(&prefix, payload_offset)
                        {
                            image_data_timestamps.push(timestamp.to_string());
                            if let Some(slot) = plane_delta_t.get_mut(plane) {
                                *slot = Some(timestamp);
                            }
                        }
                    }
                }
            } else if let Some(&first_chunk_index) = image_chunks.first() {
                let chunk = &chunks[first_chunk_index];
                if let Ok(prefix) = read_chunk_prefix(&mut reader, chunk, 8192) {
                    let (encoding, payload_offset) = nd2_frame_payload_layout(
                        &prefix,
                        chunk.data_length as usize,
                        stored_expected,
                    );
                    image_data_encodings.push(encoding.to_string());
                    image_data_payload_offsets.push(payload_offset.to_string());
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

            if let Some(first_encoding) = image_data_encodings.first() {
                series_metadata.insert(
                    "nd2_first_image_data_encoding".into(),
                    MetadataValue::String(first_encoding.clone()),
                );
            }
        }
        if !metadata_sequence_indices.is_empty() {
            series_metadata.insert(
                "nd2_image_metadata_seq_chunks".into(),
                MetadataValue::Int(metadata_chunks.len() as i64),
            );
            if eager_plane_metadata {
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
            }
            series_metadata.insert(
                "nd2_image_metadata_seq_matches_images".into(),
                MetadataValue::Bool(metadata_sequence_indices == image_sequence_indices),
            );
            let mut metadata_timestamps = Vec::with_capacity(metadata_chunks.len());
            if eager_plane_metadata {
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
        // Per-series overrides for size_z / size_t / image_count. When empty, the
        // scalar series_size_z / series_size_t / series_image_count are broadcast
        // to every series. Populated only by the ND2Reader.initFile (java:1720-1763)
        // `offsets.length != getSeriesCount()` rebuild, which derives sizeT per
        // series from each series' valid (non-zero) offset count.
        let mut series_size_z_overrides: Vec<u32> = Vec::new();
        let mut series_size_t_overrides: Vec<u32> = Vec::new();
        let mut series_image_count_overrides: Vec<u32> = Vec::new();

        if has_declared_ndcontrol_loop_dimensions
            && !has_time_loop_descriptor
            && !has_z_loop_descriptor
            && size_z.saturating_mul(size_t) != image_count
            && size_c > 1
            && position_count <= 1
            && image_count > 0
            && image_count <= image_chunks.len() as u32
        {
            size_z = image_count.max(1);
            size_t = 1;
            series_size_z = size_z;
            series_size_t = size_t;
            series_image_count = size_z.saturating_mul(size_c.max(1));
        }
        if size_c > 1 && self.position_count > 1 && size_z > 1 {
            let position_count = self.position_count.max(1);
            let denom = position_count.saturating_mul(size_z.max(1));
            if denom > 0 && (image_chunks.len() as u32) % denom == 0 {
                let inferred_t = (image_chunks.len() as u32 / denom).max(1);
                if inferred_t > size_t {
                    size_t = inferred_t;
                    series_size_t = size_t;
                    series_image_count =
                        size_z.saturating_mul(size_t).saturating_mul(size_c.max(1));
                }
            }
        }

        // When the binary ImageMetadataLV was processed, drive series/plane mapping
        // from the faithful FormatTools.rasterToPosition layout (ND2Reader.initFile
        // java:1624-1718) rather than the XML loop-order heuristic. `lv_mapping`
        // carries the result so the heuristic block below is skipped.
        let lv_mapping = if image_metadata_lv.processed {
            nd2_raster_mapping(
                &image_metadata_lv,
                size_z,
                size_t,
                position_count as usize,
                &image_sequence_indices,
            )
        } else {
            None
        };

        // Apply the tmpOffsets compaction (ND2Reader.initFile java:1708-1718) up
        // front: keep only series whose offsets array is non-empty and whose slot 0
        // was filled (offsets[i][0] > 0). We require the compacted result to retain
        // a genuine multi-series split before taking this path, preserving the
        // prior `series_count > 1` invariant (a degenerate 0/1-series compaction
        // falls through to the single-series defaults / heuristic path).
        let lv_mapping = lv_mapping.and_then(|mapping| {
            if mapping.series_count <= 1 {
                return None;
            }
            let kept: Vec<Vec<usize>> = mapping
                .source_planes
                .iter()
                .enumerate()
                .filter(|(s, _)| {
                    mapping.in_series_planes > 0
                        && mapping.first_slot_filled.get(*s).copied().unwrap_or(false)
                })
                .map(|(_, planes)| planes.clone())
                .collect();
            if kept.len() > 1 {
                Some((mapping, kept))
            } else {
                None
            }
        });

        if let Some((mapping, kept_source_planes)) = lv_mapping {
            // The series count derived from the LV M-axis == getSeriesCount() in
            // Java at this point (setDimensions split the file into numSeries).
            let java_series_count = mapping.series_count;

            series_count = kept_source_planes.len();
            series_source_planes = kept_source_planes;
            series_image_chunks = series_source_planes
                .iter()
                .map(|planes| planes.iter().map(|&plane| image_chunks[plane]).collect())
                .collect();
            series_plane_offsets = series_source_planes
                .iter()
                .map(|planes| planes.first().copied().unwrap_or(0))
                .collect();
            // Per-series plane count = sizeZ*sizeT (the collapsed zctLengths
            // product); size_z / size_t already reflect setDimensions.
            series_image_count = series_source_planes
                .iter()
                .map(|p| p.len())
                .max()
                .unwrap_or(0) as u32;
            series_size_z = size_z;
            series_size_t = size_t;

            // Post-mapping rebuild (ND2Reader.initFile java:1720-1763). If the
            // compacted series count no longer matches getSeriesCount(), Java
            // rebuilds CoreMetadata per series: sizeZ forced to 1, imageCount set
            // to the count of valid (non-zero) offsets, and sizeT derived as
            // imageCount / (rgb ? 1 : sizeC) (min 1). Otherwise every series keeps
            // the uniform getSizeZ/getSizeT/getImageCount (the scalar broadcast).
            if series_count != java_series_count {
                let is_rgb = size_c == 3;
                let denom = if is_rgb { 1 } else { size_c.max(1) };
                series_size_z_overrides = vec![1u32; series_count];
                series_image_count_overrides = Vec::with_capacity(series_count);
                series_size_t_overrides = Vec::with_capacity(series_count);
                for planes in &series_source_planes {
                    // offsets[i].length - invalid == number of filled slots; our
                    // source_planes already holds only the filled slots.
                    let image_count_series = planes.len() as u32;
                    let mut size_t_series = image_count_series / denom;
                    if size_t_series == 0 {
                        size_t_series = 1;
                    }
                    series_image_count_overrides.push(image_count_series);
                    series_size_t_overrides.push(size_t_series);
                }
                series_handling = "image_metadata_lv_raster_mapping_rebuilt_series";
            } else {
                series_handling = "image_metadata_lv_raster_mapping";
            }
            series_metadata.insert(
                "nd2_image_metadata_lv_order".into(),
                MetadataValue::String(image_metadata_lv.order.clone()),
            );
            series_metadata.insert(
                "nd2_image_metadata_lv_field_index".into(),
                MetadataValue::Int(mapping.field_index as i64),
            );
        } else if (has_float_comp_range || has_nikon_sim_acq_data)
            && size_c == 1
            && size_z == 1
            && image_chunks.len() > 1
            && loop_size_t.is_some()
        {
            // Nikon SIM ND2s can carry one stored image per SIM angle/phase.
            // Java's ND2Handler sees initialized Z/T core dimensions and takes
            // the `qName == "no_name" && v > 1` branch for NDControl LoopSize,
            // expanding the core list instead of treating the value as a time
            // count. Raw SIM files may have LoopSize one less than the stored
            // offset count, so the series count follows imageOffsets.
            series_count = image_chunks.len();
            series_image_count = 1;
            series_size_z = 1;
            series_size_t = 1;
            series_image_chunks = image_chunks.iter().map(|&chunk| vec![chunk]).collect();
            series_plane_offsets = (0..series_count).collect();
            series_source_planes = (0..series_count).map(|plane| vec![plane]).collect();
            series_handling = "ndcontrol_nikon_sim_loop_size_rebuilt_series";
        } else {
            if let Some(position_count) = loop_series_count.filter(|&count| count > 1) {
                let position_count = position_count as usize;
                let global_plane_count = image_chunks.len();
                let logical_planes_per_series = size_z as usize
                    * size_t as usize
                    * if size_c > 1 { size_c as usize } else { 1 };
                let compacted_series = (logical_planes_per_series > 0
                    && global_plane_count % logical_planes_per_series == 0)
                    .then_some(global_plane_count / logical_planes_per_series)
                    .filter(|&count| count > 1 && count <= position_count);
                let java_non_lv_mapping = if global_plane_count > 0 {
                    let mut lengths = [1i32; 4];
                    let field_index = 2usize;
                    let mut axes = match dimension_order {
                        DimensionOrder::XYCTZ => ['C', 'T', 'Z'].into_iter(),
                        DimensionOrder::XYCZT => ['C', 'Z', 'T'].into_iter(),
                        DimensionOrder::XYTCZ => ['T', 'C', 'Z'].into_iter(),
                        DimensionOrder::XYTZC => ['T', 'Z', 'C'].into_iter(),
                        DimensionOrder::XYZCT => ['Z', 'C', 'T'].into_iter(),
                        DimensionOrder::XYZTC => ['Z', 'T', 'C'].into_iter(),
                    };
                    for (i, length) in lengths.iter_mut().enumerate() {
                        if i == field_index {
                            *length = position_count as i32;
                        } else if let Some(axis) = axes.next() {
                            *length = match axis {
                                'Z' => size_z.max(1) as i32,
                                'T' => size_t.max(1) as i32,
                                // Java uses length 1 for C in this mapping; split
                                // channel selection happens later in openBytes.
                                'C' => 1,
                                _ => 1,
                            };
                        }
                    }
                    let mut zct_lengths = lengths;
                    zct_lengths[field_index] = 1;
                    let in_series_planes = zct_lengths
                        .iter()
                        .map(|&length| length.max(1) as usize)
                        .product::<usize>();
                    let mut placed =
                        vec![vec![None; in_series_planes]; lengths[field_index].max(1) as usize];
                    let mut one_indexed = false;
                    for compact in 0..global_plane_count {
                        let mut ndx = image_sequence_indices
                            .get(compact)
                            .copied()
                            .unwrap_or(compact) as i32;
                        if ndx == 1 && compact == 0 {
                            one_indexed = true;
                        }
                        if one_indexed {
                            ndx -= 1;
                        }
                        if ndx < 0 {
                            continue;
                        }
                        let mut pos = raster_to_position(&lengths, ndx);
                        let series_index = pos[field_index];
                        pos[field_index] = 0;
                        let plane = position_to_raster(&zct_lengths, &pos);
                        if series_index >= 0
                            && (series_index as usize) < placed.len()
                            && plane >= 0
                            && (plane as usize) < in_series_planes
                        {
                            placed[series_index as usize][plane as usize] = Some(compact);
                        }
                    }
                    let source_planes = placed
                        .into_iter()
                        .filter(|planes| planes.first().is_some_and(Option::is_some))
                        .map(|planes| planes.into_iter().flatten().collect::<Vec<_>>())
                        .filter(|planes| !planes.is_empty())
                        .collect::<Vec<_>>();
                    let max_source_planes = source_planes.iter().map(Vec::len).max().unwrap_or(0);
                    let expected_source_planes = compacted_series
                        .map(|count| global_plane_count / count)
                        .unwrap_or_else(|| size_z.max(1) as usize * size_t.max(1) as usize);
                    (source_planes.len() > 1 && max_source_planes == expected_source_planes)
                        .then_some(source_planes)
                } else {
                    None
                };
                if let Some(source_planes) = java_non_lv_mapping {
                    series_count = source_planes.len();
                    let stored_planes_per_series =
                        source_planes.iter().map(Vec::len).max().unwrap_or(0);
                    let inferred_t =
                        if size_z > 0 && stored_planes_per_series % size_z as usize == 0 {
                            (stored_planes_per_series / size_z as usize) as u32
                        } else {
                            size_t
                        };
                    series_image_count = size_z
                        .max(1)
                        .saturating_mul(inferred_t.max(1))
                        .saturating_mul(size_c.max(1));
                    series_size_z = size_z;
                    series_size_t = inferred_t.max(1);
                    series_source_planes = source_planes;
                    series_image_chunks = series_source_planes
                        .iter()
                        .map(|planes| planes.iter().map(|&plane| image_chunks[plane]).collect())
                        .collect();
                    series_plane_offsets = series_source_planes
                        .iter()
                        .map(|planes| planes.first().copied().unwrap_or(0))
                        .collect();
                    series_metadata.insert(
                        "nd2_loop_series_layout_source".into(),
                        MetadataValue::String("java_non_lv_raster_mapping".into()),
                    );
                    series_handling = "split_xy_positions_non_lv_raster_mapping";
                } else if let Some(compacted_series) = compacted_series {
                    let stored_planes_per_series = global_plane_count / compacted_series;
                    let inferred_t =
                        if size_z > 0 && stored_planes_per_series % size_z as usize == 0 {
                            (stored_planes_per_series / size_z as usize) as u32
                        } else {
                            size_t
                        };
                    series_count = compacted_series;
                    series_image_count = size_z
                        .max(1)
                        .saturating_mul(inferred_t.max(1))
                        .saturating_mul(size_c.max(1));
                    series_size_z = size_z;
                    series_size_t = inferred_t.max(1);
                    let mut lengths = [1i32; 4];
                    let field_index = 2usize;
                    let mut axes = match dimension_order {
                        DimensionOrder::XYCTZ => ['C', 'T', 'Z'].into_iter(),
                        DimensionOrder::XYCZT => ['C', 'Z', 'T'].into_iter(),
                        DimensionOrder::XYTCZ => ['T', 'C', 'Z'].into_iter(),
                        DimensionOrder::XYTZC => ['T', 'Z', 'C'].into_iter(),
                        DimensionOrder::XYZCT => ['Z', 'C', 'T'].into_iter(),
                        DimensionOrder::XYZTC => ['Z', 'T', 'C'].into_iter(),
                    };
                    for (i, length) in lengths.iter_mut().enumerate() {
                        if i == field_index {
                            *length = compacted_series as i32;
                        } else if let Some(axis) = axes.next() {
                            *length = match axis {
                                'Z' => size_z.max(1) as i32,
                                'T' => inferred_t.max(1) as i32,
                                'C' => 1,
                                _ => 1,
                            };
                        }
                    }
                    let mut zct_lengths = lengths;
                    zct_lengths[field_index] = 1;
                    let mut placed = vec![vec![None; stored_planes_per_series]; compacted_series];
                    let mut one_indexed = false;
                    for compact in 0..global_plane_count {
                        let mut ndx = image_sequence_indices
                            .get(compact)
                            .copied()
                            .unwrap_or(compact) as i32;
                        if ndx == 1 && compact == 0 {
                            one_indexed = true;
                        }
                        if one_indexed {
                            ndx -= 1;
                        }
                        if ndx < 0 {
                            continue;
                        }
                        let mut pos = raster_to_position(&lengths, ndx);
                        let series_index = pos[field_index];
                        pos[field_index] = 0;
                        let plane = position_to_raster(&zct_lengths, &pos);
                        if series_index >= 0
                            && (series_index as usize) < placed.len()
                            && plane >= 0
                            && (plane as usize) < stored_planes_per_series
                        {
                            placed[series_index as usize][plane as usize] = Some(compact);
                        }
                    }
                    series_source_planes = placed
                        .into_iter()
                        .map(|planes| planes.into_iter().flatten().collect::<Vec<_>>())
                        .collect();
                    series_image_chunks = series_source_planes
                        .iter()
                        .map(|planes| planes.iter().map(|&plane| image_chunks[plane]).collect())
                        .collect();
                    series_plane_offsets = series_source_planes
                        .iter()
                        .map(|planes| planes.first().copied().unwrap_or(0))
                        .collect();
                    series_metadata.insert(
                        "nd2_loop_series_layout_source".into(),
                        MetadataValue::String("java_offset_compaction_raster_mapping".into()),
                    );
                    series_handling = "split_xy_positions_offset_compaction";
                } else if global_plane_count == position_count {
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
                } else {
                    let expected_planes_per_position = size_z as usize * size_t as usize;
                    if global_plane_count % position_count == 0 {
                        let planes_per_position = global_plane_count / position_count;
                        if expected_planes_per_position == planes_per_position {
                            let (layout, source_planes, layout_source) =
                                nd2_choose_xy_position_layout(
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
                    } else if expected_planes_per_position > 0 {
                        let expected_total =
                            position_count.saturating_mul(expected_planes_per_position);
                        let missing_planes = expected_total.saturating_sub(global_plane_count);
                        let trailing_planes = global_plane_count.saturating_sub(expected_total);
                        let use_sequence_indices = global_plane_count <= expected_total;
                        let sequence_to_compact: HashMap<usize, usize> = image_sequence_indices
                            .iter()
                            .copied()
                            .enumerate()
                            .map(|(compact, sequence)| (sequence, compact))
                            .collect();

                        if (global_plane_count <= expected_total
                            && missing_planes <= position_count)
                            || (global_plane_count >= expected_total
                                && trailing_planes <= position_count)
                        {
                            let mut source_planes = (0..position_count)
                                .map(|_| Vec::with_capacity(expected_planes_per_position))
                                .collect::<Vec<_>>();
                            for frame in 0..expected_planes_per_position {
                                for (series, planes) in source_planes.iter_mut().enumerate() {
                                    let sequence = frame * position_count + series;
                                    let source = if !use_sequence_indices
                                        || sequence_to_compact.is_empty()
                                    {
                                        (sequence < image_chunks.len()).then_some(sequence)
                                    } else {
                                        sequence_to_compact.get(&sequence).copied()
                                    };
                                    if let Some(source) = source {
                                        planes.push(source);
                                    }
                                }
                            }
                            if source_planes
                                .iter()
                                .filter(|planes| !planes.is_empty())
                                .count()
                                > 1
                            {
                                series_count = position_count;
                                series_image_count = expected_planes_per_position as u32;
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
                                    MetadataValue::String("interleaved_sparse".into()),
                                );
                                series_metadata.insert(
                                    "nd2_loop_series_assumed_layout".into(),
                                    MetadataValue::String("interleaved".into()),
                                );
                                series_metadata.insert(
                                    "nd2_loop_series_layout_source".into(),
                                    MetadataValue::String("sparse_sequence_indices".into()),
                                );
                                series_handling =
                                    "split_xy_positions_interleaved_sparse_full_series";
                            } else {
                                series_handling = "unsupported_multi_position_layout_kept_flat";
                            }
                        } else {
                            series_handling = "unsupported_multi_position_layout_kept_flat";
                        }
                    } else if image_count > 0 {
                        series_handling = "unsupported_multi_position_layout_kept_flat";
                    }
                }
            } else if size_c > 1 && self.position_count > 1 {
                let stored_planes_per_series = size_z.max(1).saturating_mul(size_t.max(1)) as usize;
                if stored_planes_per_series > 0
                    && image_chunks.len() > stored_planes_per_series
                    && image_chunks.len() % stored_planes_per_series == 0
                {
                    let inferred_series = image_chunks.len() / stored_planes_per_series;
                    if inferred_series > 1 && inferred_series <= self.position_count as usize {
                        series_count = inferred_series;
                        series_image_count = stored_planes_per_series as u32;
                        series_size_z = size_z;
                        series_size_t = size_t;
                        let mut lengths = [1i32; 4];
                        let field_index = 2usize;
                        let mut axes = match dimension_order {
                            DimensionOrder::XYCTZ => ['C', 'T', 'Z'].into_iter(),
                            DimensionOrder::XYCZT => ['C', 'Z', 'T'].into_iter(),
                            DimensionOrder::XYTCZ => ['T', 'C', 'Z'].into_iter(),
                            DimensionOrder::XYTZC => ['T', 'Z', 'C'].into_iter(),
                            DimensionOrder::XYZCT => ['Z', 'C', 'T'].into_iter(),
                            DimensionOrder::XYZTC => ['Z', 'T', 'C'].into_iter(),
                        };
                        for (i, length) in lengths.iter_mut().enumerate() {
                            if i == field_index {
                                *length = inferred_series as i32;
                            } else if let Some(axis) = axes.next() {
                                *length = match axis {
                                    'Z' => size_z.max(1) as i32,
                                    'T' => size_t.max(1) as i32,
                                    'C' => 1,
                                    _ => 1,
                                };
                            }
                        }
                        let mut zct_lengths = lengths;
                        zct_lengths[field_index] = 1;
                        let mut placed =
                            vec![vec![None; stored_planes_per_series]; inferred_series];
                        let mut one_indexed = false;
                        for compact in 0..image_chunks.len() {
                            let mut ndx = image_sequence_indices
                                .get(compact)
                                .copied()
                                .unwrap_or(compact)
                                as i32;
                            if ndx == 1 && compact == 0 {
                                one_indexed = true;
                            }
                            if one_indexed {
                                ndx -= 1;
                            }
                            if ndx < 0 {
                                continue;
                            }
                            let mut pos = raster_to_position(&lengths, ndx);
                            let series_index = pos[field_index];
                            pos[field_index] = 0;
                            let plane = position_to_raster(&zct_lengths, &pos);
                            if series_index >= 0
                                && (series_index as usize) < placed.len()
                                && plane >= 0
                                && (plane as usize) < stored_planes_per_series
                            {
                                placed[series_index as usize][plane as usize] = Some(compact);
                            }
                        }
                        series_source_planes = placed
                            .into_iter()
                            .map(|planes| planes.into_iter().flatten().collect::<Vec<_>>())
                            .collect();
                        series_image_chunks = series_source_planes
                            .iter()
                            .map(|planes| planes.iter().map(|&plane| image_chunks[plane]).collect())
                            .collect();
                        series_plane_offsets = series_source_planes
                            .iter()
                            .map(|planes| planes.first().copied().unwrap_or(0))
                            .collect();
                        series_metadata.insert(
                            "nd2_loop_series_layout_source".into(),
                            MetadataValue::String("java_position_count_raster_mapping".into()),
                        );
                        series_handling = "position_count_non_lv_raster_mapping";
                    }
                }
            }
        }

        series_metadata.insert(
            "nd2_loop_series_handling".into(),
            MetadataValue::String(series_handling.to_string()),
        );
        let split_channels = size_c > 1;
        series_metadata.insert(
            "nd2_split_channels".into(),
            MetadataValue::Bool(split_channels),
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
        if !pfs_offsets.is_empty() {
            series_metadata.insert(
                "nd2_pfs_offsets".into(),
                MetadataValue::String(
                    pfs_offsets
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );
        }
        if !pfs_status.is_empty() {
            series_metadata.insert(
                "nd2_pfs_status".into(),
                MetadataValue::String(
                    pfs_status
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );
        }
        if !self.pos_x.is_empty() {
            series_metadata.insert(
                "nd2_pos_x".into(),
                MetadataValue::String(
                    self.pos_x
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );
        }
        if !self.pos_y.is_empty() {
            series_metadata.insert(
                "nd2_pos_y".into(),
                MetadataValue::String(
                    self.pos_y
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );
        }
        if !self.pos_z.is_empty() {
            series_metadata.insert(
                "nd2_pos_z".into(),
                MetadataValue::String(
                    self.pos_z
                        .iter()
                        .map(|v| v.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );
        }
        if let Some(ri) = self.refractive_index {
            series_metadata.insert("nd2_refractive_index".into(), MetadataValue::Float(ri));
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
        if !self.ts_t.is_empty() {
            series_metadata.insert(
                "nd2_acq_times".into(),
                MetadataValue::String(
                    self.ts_t
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
            // Per-series sizes when the java:1720-1763 rebuild produced overrides,
            // otherwise the uniform scalar values broadcast to every series.
            let this_size_z = series_size_z_overrides
                .get(series_index)
                .copied()
                .unwrap_or(series_size_z);
            let this_size_t = series_size_t_overrides
                .get(series_index)
                .copied()
                .unwrap_or(series_size_t);
            let mut this_image_count = series_image_count_overrides
                .get(series_index)
                .copied()
                .unwrap_or(series_image_count);
            if split_channels {
                this_image_count = this_size_z
                    .max(1)
                    .saturating_mul(this_size_t.max(1))
                    .saturating_mul(size_c.max(1));
            }
            let this_bits_per_pixel = if ((size_c == 1 && series_count > 1)
                || (split_channels && series_count > 1 && bpp == 14))
                && bpp < storage_bits_for_pixel_type
            {
                storage_bits_for_pixel_type
            } else {
                bpp
            };
            metas.push(ImageMetadata {
                size_x,
                size_y,
                size_z: this_size_z,
                size_c,
                size_t: this_size_t,
                pixel_type,
                bits_per_pixel: this_bits_per_pixel,
                image_count: this_image_count,
                dimension_order,
                is_rgb: false,
                is_interleaved: false,
                is_indexed: if size_c == 3 && pixel_type == PixelType::Uint8 {
                    false
                } else {
                    nd2_is_indexed_from_channel_colors(&self.channel_colors)
                },
                is_little_endian: true,
                resolution_count: 1,
                thumbnail: false,
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
        self.split_channels = false;
        self.plane_delta_t = plane_delta_t;
        self.plane_position_z = plane_position_z;
        self.series_image_chunks = series_image_chunks;
        self.series_plane_offsets = series_plane_offsets;
        self.series_source_planes = series_source_planes;
        self.image_chunks = image_chunks;
        self.chunks = chunks;
        self.split_channels = split_channels;
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
        self.split_channels = false;
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
        self.ts_t.clear();
        self.exposure_time.clear();
        self.channel_colors.clear();
        self.text_channel_names.clear();
        self.colors.clear();
        self.pos_x.clear();
        self.pos_y.clear();
        self.pos_z.clear();
        self.position_names.clear();
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
            let samples = if meta.is_rgb {
                meta.size_c.max(1) as usize
            } else {
                1
            };
            let expected = meta.size_x as usize
                * meta.size_y as usize
                * samples
                * meta.pixel_type.bytes_per_sample();
            let mut decoded =
                crate::common::codec::decompress_jpeg2000_with_endianness(&data, false)?;
            if meta.is_rgb && samples > 1 {
                let plane_pixels = meta.size_x as usize * meta.size_y as usize;
                let bps = meta.pixel_type.bytes_per_sample();
                if decoded.len() >= plane_pixels * samples * bps {
                    let mut planar = vec![0u8; plane_pixels * samples * bps];
                    for pixel in 0..plane_pixels {
                        for sample in 0..samples {
                            let src = (pixel * samples + sample) * bps;
                            let dst = (sample * plane_pixels + pixel) * bps;
                            planar[dst..dst + bps].copy_from_slice(&decoded[src..src + bps]);
                        }
                    }
                    decoded = planar;
                }
            }
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
        let stored_plane_index = if self.split_channels && meta.size_c > 1 {
            plane_index / meta.size_c
        } else {
            plane_index
        };
        let split_channel = (self.split_channels && meta.size_c > 1)
            .then_some((plane_index % meta.size_c) as usize);
        let chunk_idx = series_chunks
            .get(stored_plane_index as usize)
            .copied()
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let chunk = &self.chunks[chunk_idx];

        let f = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let data = read_chunk_data(f, chunk).map_err(BioFormatsError::Io)?;

        let bps = meta.pixel_type.bytes_per_sample();
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;
        let size_c = meta.size_c as usize;

        let split = self.split_channels && meta.size_c > 1;
        let scanline_pad = if meta.size_x % 2 != 0 && meta.size_c % 2 != 0 {
            1usize
        } else {
            0usize
        };
        let split_java_buffer_branch =
            split && (meta.size_c <= 4 || scanline_pad == 0) && self.n_x_fields == 1;
        let stored_row = if split {
            if split_java_buffer_branch {
                (size_x + scanline_pad) * size_c * bps
            } else {
                let row_length = size_x * size_c * bps;
                let row_mod = row_length % 4;
                row_length + if row_mod == 0 { 0 } else { 4 - row_mod }
            }
        } else {
            // Java ND2Reader.openBytes resets scanlinePad for uncompressed
            // non-split planes to 4-byte boundaries before calling readPlane.
            let raw_scanline_pad = ((bps * size_x) % 4) / bps;
            (size_x + raw_scanline_pad) * bps
        };
        let stored_expected = stored_row * size_y;

        let chunk_context = format!(
            "plane {plane_index}: {} at offset {} length {}",
            chunk.name, chunk.data_offset, chunk.data_length
        );
        let decoded = match decode_nd2_frame_payload(&data, stored_expected) {
            Ok(decoded) => decoded,
            Err(BioFormatsError::Format(msg))
                if msg.starts_with("frame data too small")
                    && !looks_like_zlib(&data)
                    && !looks_like_jpeg2000(&data)
                    && data.get(7..).is_none_or(|payload| {
                        !looks_like_zlib(payload) && !looks_like_jpeg2000(payload)
                    })
                    && data.get(8..).is_none_or(|payload| {
                        !looks_like_zlib(payload) && !looks_like_jpeg2000(payload)
                    }) =>
            {
                let payload_offset = if nd2_prefix_timestamp_seconds(&data, 8).is_some() {
                    8
                } else {
                    0
                };
                f.seek(SeekFrom::Start(chunk.data_offset + payload_offset))
                    .map_err(BioFormatsError::Io)?;
                let mut decoded = vec![0u8; stored_expected];
                let mut filled = 0usize;
                while filled < decoded.len() {
                    let n = f
                        .read(&mut decoded[filled..])
                        .map_err(BioFormatsError::Io)?;
                    if n == 0 {
                        break;
                    }
                    filled += n;
                }
                decoded
            }
            Err(e) => {
                return Err(match e {
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
                });
            }
        };

        if let Some(channel) = split_channel {
            if split_java_buffer_branch {
                let split_width = size_x + scanline_pad;
                let split = nd2_split_interleaved_channel(
                    &decoded,
                    split_width,
                    size_y,
                    size_c,
                    bps,
                    channel,
                )?;
                if scanline_pad == 0 {
                    return Ok(split);
                }
                let in_row = split_width * bps;
                let out_row = size_x * bps;
                let mut out = vec![0u8; out_row * size_y];
                for row in 0..size_y {
                    let src = row * in_row;
                    let dst = row * out_row;
                    if src >= split.len() {
                        break;
                    }
                    let available = (split.len() - src).min(out_row);
                    out[dst..dst + available].copy_from_slice(&split[src..src + available]);
                }
                return Ok(out);
            }

            let row_bytes = size_x * size_c * bps;
            let out_row = size_x * bps;
            let mut out = vec![0u8; out_row * size_y];
            for row in 0..size_y {
                let src = row * stored_row;
                if src >= decoded.len() {
                    break;
                }
                let available = (decoded.len() - src).min(row_bytes);
                let split_row = nd2_split_interleaved_channel(
                    &decoded[src..src + available],
                    available / (size_c * bps),
                    1,
                    size_c,
                    bps,
                    channel,
                )?;
                let dst = row * out_row;
                let copy_len = split_row.len().min(out_row);
                out[dst..dst + copy_len].copy_from_slice(&split_row[..copy_len]);
            }
            return Ok(out);
        }

        let raw_scanline_pad = ((bps * size_x) % 4) / bps;
        if raw_scanline_pad == 0 {
            return Ok(decoded);
        }
        let in_row = (size_x + raw_scanline_pad) * bps;
        let out_row = size_x * bps;
        let mut out = vec![0u8; out_row * size_y];
        for row in 0..size_y {
            let src = row * in_row;
            let dst = row * out_row;
            if src >= decoded.len() {
                break;
            }
            let available = (decoded.len() - src).min(out_row);
            out[dst..dst + available].copy_from_slice(&decoded[src..src + available]);
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
        {
            let meta = self
                .meta
                .get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized)?;
            if plane_index >= meta.image_count {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index));
            }
            validate_region("ND2", meta.size_x, meta.size_y, x, y, w, h)?;

            let split = self.split_channels && meta.size_c > 1;
            let supports_raw_window = (!self.split_channels && meta.size_c == 1) || split;
            if self.old_jp2_planes.is_empty() && !self.is_lossless && supports_raw_window {
                let series_chunks = self
                    .series_image_chunks
                    .get(self.current_series)
                    .unwrap_or(&self.image_chunks);
                let stored_plane_index = if split {
                    plane_index / meta.size_c
                } else {
                    plane_index
                };
                let chunk_idx = series_chunks
                    .get(stored_plane_index as usize)
                    .copied()
                    .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
                let chunk = &self.chunks[chunk_idx];
                let chunk_data_offset = chunk.data_offset;
                let chunk_data_length = chunk.data_length;

                let bps = meta.pixel_type.bytes_per_sample();
                let size_x = meta.size_x as usize;
                let size_c = meta.size_c.max(1) as usize;
                let scanline_pad = if meta.size_x % 2 != 0 && meta.size_c % 2 != 0 {
                    1usize
                } else {
                    0usize
                };
                let split_java_buffer_branch =
                    split && (meta.size_c <= 4 || scanline_pad == 0) && self.n_x_fields == 1;
                let stored_row = if split {
                    if split_java_buffer_branch {
                        (size_x + scanline_pad) * size_c * bps
                    } else {
                        let row_length = size_x * size_c * bps;
                        let row_mod = row_length % 4;
                        row_length + if row_mod == 0 { 0 } else { 4 - row_mod }
                    }
                } else {
                    let raw_scanline_pad = ((bps * size_x) % 4) / bps;
                    (size_x + raw_scanline_pad) * bps
                };
                let stored_expected = stored_row * meta.size_y as usize;
                let expected = stored_expected as u64;

                let f = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
                let probe_len = chunk_data_length.min(4096 + 16) as usize;
                f.seek(SeekFrom::Start(chunk_data_offset))
                    .map_err(BioFormatsError::Io)?;
                let mut probe = vec![0u8; probe_len];
                f.read_exact(&mut probe).map_err(BioFormatsError::Io)?;

                let raw_payload_offset = if chunk_data_length == expected {
                    (!looks_like_zlib(&probe) && !looks_like_jpeg2000(&probe)).then_some(0usize)
                } else if chunk_data_length == expected + 8 {
                    probe.get(8..).and_then(|payload| {
                        (!looks_like_zlib(payload) && !looks_like_jpeg2000(payload))
                            .then_some(8usize)
                    })
                } else if chunk_data_length > expected + 8
                    && chunk_data_length - expected - 8 <= 4096
                    && nd2_prefix_timestamp_seconds(&probe, 8).is_some()
                {
                    probe.get(8..).and_then(|payload| {
                        (!looks_like_zlib(payload) && !looks_like_jpeg2000(payload))
                            .then_some(8usize)
                    })
                } else if chunk_data_length > expected + 4096
                    && chunk_data_length - expected - 4096 <= 4096
                {
                    probe.get(4096..).and_then(|payload| {
                        (!looks_like_zlib(payload) && !looks_like_jpeg2000(payload))
                            .then_some(4096usize)
                    })
                } else if chunk_data_length > expected && chunk_data_length - expected <= 4096 {
                    (!looks_like_zlib(&probe) && !looks_like_jpeg2000(&probe)).then_some(0usize)
                } else {
                    None
                };

                if let Some(payload_offset) = raw_payload_offset {
                    let out_row = w as usize * bps;
                    let mut out = vec![0u8; out_row * h as usize];
                    let base = chunk_data_offset + payload_offset as u64;
                    let channel = if split {
                        (plane_index % meta.size_c) as usize
                    } else {
                        0
                    };
                    let row_span = if split {
                        w as usize * size_c * bps
                    } else {
                        out_row
                    };
                    let start_x_bytes = if split {
                        x as usize * size_c * bps
                    } else {
                        x as usize * bps
                    };
                    let mut row_buf = vec![0u8; row_span];
                    for row in 0..h as usize {
                        let src = base + ((y as usize + row) * stored_row + start_x_bytes) as u64;
                        let dst = row * out_row;
                        f.seek(SeekFrom::Start(src)).map_err(BioFormatsError::Io)?;
                        row_buf.fill(0);
                        let mut filled = 0usize;
                        while filled < row_span {
                            let n = f
                                .read(&mut row_buf[filled..row_span])
                                .map_err(BioFormatsError::Io)?;
                            if n == 0 {
                                break;
                            }
                            filled += n;
                        }
                        if split {
                            for px in 0..w as usize {
                                let src = px * size_c * bps + channel * bps;
                                let target = dst + px * bps;
                                out[target..target + bps].copy_from_slice(&row_buf[src..src + bps]);
                            }
                        } else {
                            out[dst..dst + out_row].copy_from_slice(&row_buf[..out_row]);
                        }
                    }
                    return Ok(out);
                }
            }
        }

        let full = self.open_bytes(plane_index)?;
        let meta = self.metadata();
        if !self.old_jp2_planes.is_empty() && meta.is_rgb && !meta.is_interleaved {
            validate_region("ND2", meta.size_x, meta.size_y, x, y, w, h)?;
            let bps = meta.pixel_type.bytes_per_sample();
            let channel_count = meta.size_c.max(1) as usize;
            let row_bytes = meta.size_x as usize * bps;
            let channel_bytes = row_bytes * meta.size_y as usize;
            let out_row = w as usize * bps;
            let mut out = vec![0u8; channel_count * h as usize * out_row];
            for channel in 0..channel_count {
                let channel_base = channel * channel_bytes;
                for row in 0..h as usize {
                    let src = channel_base + (y as usize + row) * row_bytes + x as usize * bps;
                    let dst = channel * h as usize * out_row + row * out_row;
                    out[dst..dst + out_row].copy_from_slice(&full[src..src + out_row]);
                }
            }
            return Ok(out);
        }
        let spp = if self.split_channels {
            1
        } else if !self.old_jp2_planes.is_empty() && meta.is_rgb {
            meta.size_c as usize
        } else if self.old_jp2_planes.is_empty() {
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

    fn compressed_level_info(
        &self,
        plane_index: u32,
        level: u32,
    ) -> Result<CompressedExtractionSupport> {
        let meta = self.current_meta_checked(plane_index)?;
        if level != 0 {
            return Ok(CompressedExtractionSupport::NotSupported {
                reason: "ND2 compressed extraction only supports resolution level 0".into(),
            });
        }
        if self.split_channels && meta.size_c > 1 {
            return Ok(CompressedExtractionSupport::NotSupported {
                reason: "ND2 split-channel logical planes require channel extraction".into(),
            });
        }
        let container = match self.nd2_compressed_payload_for_plane(plane_index) {
            Ok((_, container)) => container,
            Err(BioFormatsError::UnsupportedFormat(reason)) => {
                return Ok(CompressedExtractionSupport::NotSupported { reason });
            }
            Err(err) => return Err(err),
        };
        Ok(CompressedExtractionSupport::Supported(
            CompressedLevelInfo {
                plane_index,
                level,
                width: u64::from(meta.size_x),
                height: u64::from(meta.size_y),
                tile_width: meta.size_x,
                tile_height: meta.size_y,
                tiles_across: 1,
                tiles_down: 1,
                codec: LossyCodec::Jpeg2000 { container },
                modes: vec![CompressedTileMode::OriginalBytes],
                constraints: Vec::new(),
            },
        ))
    }

    fn read_compressed_tile(
        &mut self,
        plane_index: u32,
        level: u32,
        col: u64,
        row: u64,
        preferred_modes: &[CompressedTileMode],
    ) -> Result<CompressedTile> {
        if !mode_allowed(preferred_modes, CompressedTileMode::OriginalBytes) {
            return Err(BioFormatsError::UnsupportedFormat(
                "requested compressed tile modes are not available for ND2 frames".into(),
            ));
        }
        let meta = self.current_meta_checked(plane_index)?;
        if level != 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "ND2 compressed extraction only supports resolution level 0".into(),
            ));
        }
        if self.split_channels && meta.size_c > 1 {
            return Err(BioFormatsError::UnsupportedFormat(
                "ND2 split-channel logical planes require channel extraction".into(),
            ));
        }
        if col != 0 || row != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let codec;
        let bytes = if !self.old_jp2_planes.is_empty() {
            let plane = self
                .old_jp2_planes
                .get(self.current_series)
                .and_then(|planes| planes.get(plane_index as usize))
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
            let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            codec = LossyCodec::Jpeg2000 {
                container: Jpeg2000Container::Codestream,
            };
            CompressedBytes::FileRange {
                path: path.clone(),
                offset: plane.data_offset,
                length: plane.data_length,
            }
        } else {
            let (payload, container) = self.nd2_compressed_payload_for_plane(plane_index)?;
            codec = LossyCodec::Jpeg2000 { container };
            CompressedBytes::Owned(payload)
        };
        Ok(CompressedTile {
            plane_index,
            level,
            col,
            row,
            origin_x: 0,
            origin_y: 0,
            width: meta.size_x,
            height: meta.size_y,
            nominal_tile_width: meta.size_x,
            nominal_tile_height: meta.size_y,
            codec,
            mode: CompressedTileMode::OriginalBytes,
            bytes,
        })
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{
            create_lsid, OmeDetector, OmeInstrument, OmeMetadata, OmeObjective, OmePlane,
        };
        if self.meta.is_empty() {
            return None;
        }
        let mut ome = OmeMetadata::default();
        for (series, meta) in self.meta.iter().enumerate() {
            let mut series_ome = OmeMetadata::from_image_metadata(meta);
            let mut img = series_ome.images.pop().unwrap_or_default();

            // Image name: "<filename> (series <n>)" per ND2Reader (~2263).
            if let Some(path) = &self.path {
                if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
                    let width = self.meta.len().to_string().len();
                    let series_suffix = format!("(series {:0width$})", series + 1, width = width);
                    let suffix = self
                        .position_names
                        .get(series)
                        .filter(|name| !name.is_empty())
                        .map(String::as_str)
                        .unwrap_or(&series_suffix);
                    img.name = Some(format!("{} {}", fname, suffix).trim().to_string());
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

            img.instrument_ref = Some(0);
            img.objective_ref = Some(0);

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
                channel.detector_ref = Some(create_lsid("Detector", &[0, 0]));
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

            // The CustomData|AcqTimesCache stream is the authoritative per-plane
            // DeltaT when it covers every global plane (Java: tsT, used directly as
            // stampIndex = n when tsT.size() == getImageCount()).
            let ts_t_global = (self.ts_t.len() == self.image_chunks.len() && !self.ts_t.is_empty())
                .then_some(self.ts_t.as_slice());

            if self.plane_delta_t.iter().any(Option::is_some)
                || self.plane_position_z.iter().any(Option::is_some)
                || ts_t_global.is_some()
                || plane_pos_x.is_some()
                || plane_pos_y.is_some()
                || plane_pos_z_value.is_some()
                || !self.exposure_time.is_empty()
            {
                // Java split mode exposes one logical plane per channel while one
                // ImageDataSeq chunk stores the interleaved channels for a Z/T plane.
                let effective_c = if self.split_channels {
                    meta.size_c.max(1)
                } else {
                    1
                };
                let plane_offset = self.series_plane_offsets.get(series).copied().unwrap_or(0);
                let source_planes = self.series_source_planes.get(series);
                img.planes = (0..meta.image_count)
                    .map(|i| {
                        let c = i % effective_c;
                        let z = (i / effective_c) % meta.size_z.max(1);
                        let t = i / (effective_c * meta.size_z.max(1));
                        let source_plane = source_planes
                            .and_then(|planes| {
                                planes
                                    .get(if self.split_channels {
                                        (i / effective_c) as usize
                                    } else {
                                        i as usize
                                    })
                                    .copied()
                            })
                            .unwrap_or_else(|| {
                                plane_offset
                                    + if self.split_channels {
                                        (i / effective_c) as usize
                                    } else {
                                        i as usize
                                    }
                            });
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
                            delta_t: ts_t_global
                                .and_then(|ts| ts.get(source_plane).copied())
                                .or_else(|| {
                                    self.plane_delta_t.get(source_plane).copied().flatten()
                                }),
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

            ome.images.push(img);
        }

        // Java ND2Reader always creates Detector:0:0 with type Other, then links
        // every channel's DetectorSettings to it.
        let objective = OmeObjective {
            calibrated_magnification: self.objective_mag,
            lens_na: self.lens_na,
            model: self.objective_model.clone(),
            ..Default::default()
        };
        let instrument = OmeInstrument {
            detectors: vec![OmeDetector {
                id: Some(create_lsid("Detector", &[0, 0])),
                detector_type: Some("Other".to_string()),
                ..Default::default()
            }],
            objectives: vec![objective],
            ..Default::default()
        };
        ome.instruments.push(instrument);

        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_nd2_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_nd2_{nanos}_{name}"))
    }

    fn nd2_chunk(name: &str, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&ND2_MAGIC);
        out.extend_from_slice(&(name.len() as u32).to_le_bytes());
        out.extend_from_slice(&(payload.len() as u64).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.extend_from_slice(payload);
        out
    }

    fn nd2_attr(name: &str, value: i32) -> Vec<u8> {
        let mut out = vec![name.chars().count() as u8];
        for u in name.encode_utf16() {
            out.extend_from_slice(&u.to_le_bytes());
        }
        out.extend_from_slice(&value.to_le_bytes());
        out
    }

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
    fn nd2_zero_timestamp_raw_frame_skips_eight_bytes_like_java() {
        let mut frame = vec![0; 8];
        frame.extend_from_slice(&[17, 23]);

        let (encoding, payload_offset) = nd2_frame_payload_layout(&frame, frame.len(), 2);
        assert_eq!(encoding, "raw_with_8_byte_prefix");
        assert_eq!(payload_offset, 8);
        assert_eq!(decode_nd2_frame_payload(&frame, 2).unwrap(), vec![17, 23]);
    }

    #[test]
    fn nd2_chunk_map_uses_each_image_data_name_length() {
        let path = temp_nd2_path("chunk_map_variable_image_name_lengths.nd2");
        let mut bytes = Vec::new();
        let mut entries = Vec::new();

        for i in 0..=10 {
            let name = format!("ImageDataSeq|{i}!");
            let position = bytes.len() as u64;
            let payload = [i as u8; 10];
            bytes.extend_from_slice(&nd2_chunk(&name, &payload));
            entries.extend_from_slice(name.as_bytes());
            entries.extend_from_slice(&position.to_le_bytes());
            let block_length = 16 + name.len() as u64 + payload.len() as u64;
            entries.extend_from_slice(&block_length.to_le_bytes());
        }

        let map_offset = bytes.len() as u64;
        bytes.extend_from_slice(&nd2_chunk("ImageMetadataSeqLV|0!", &entries));
        bytes.extend_from_slice(b"ND2 CHUNK MAP SIGNATURE 0000001");
        bytes.push(0);
        bytes.extend_from_slice(&map_offset.to_le_bytes());

        std::fs::write(&path, bytes).unwrap();
        let mut reader = BufReader::new(File::open(&path).unwrap());
        let chunks = read_chunk_map(&mut reader).unwrap().unwrap();
        let image_chunks: Vec<_> = chunks
            .iter()
            .filter(|chunk| chunk.name.starts_with("ImageDataSeq"))
            .collect();
        assert_eq!(image_chunks.len(), 11);

        let tenth = image_chunks[10];
        assert_eq!(tenth.name, "ImageDataSeq|10!");
        assert_eq!(
            tenth.data_offset,
            tenth.block_offset + 16 + "ImageDataSeq|10!".len() as u64
        );
        assert_eq!(tenth.data_length, 10);
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
    fn nd2_compressed_payload_returns_direct_jpeg2000_frame() {
        let jp2 = vec![0xff, 0x4f, 0xff, 0x51, 1, 2, 3, 4];
        let (payload, container) = nd2_compressed_jpeg2000_payload(&jp2).unwrap();
        assert_eq!(payload, jp2);
        assert_eq!(container, Jpeg2000Container::Codestream);
    }

    #[test]
    fn nd2_compressed_payload_assembles_split_jpeg2000_frame() {
        let jp2 = [0xff, 0x4f, 0xff, 0x51, 1, 2, 3, 4];
        let frame = chunk_table_frame(&[(20, &jp2[..4]), (28, &jp2[4..])]);

        let (payload, container) = nd2_compressed_jpeg2000_payload(&frame).unwrap();
        assert_eq!(payload, jp2);
        assert_eq!(container, Jpeg2000Container::Codestream);
    }

    #[test]
    fn nd2_compressed_payload_rejects_per_chunk_jpeg2000() {
        let first = [0xff, 0x4f, 0xff, 0x51, 1];
        let second = [0xff, 0x4f, 0xff, 0x51, 2];
        let frame = chunk_table_frame(&[(20, &first), (32, &second)]);

        assert!(nd2_compressed_jpeg2000_payload(&frame).is_none());
    }

    #[test]
    fn nd2_indexed_flag_ignores_black_and_white_like_java() {
        let mut colors = HashMap::new();
        assert!(!nd2_is_indexed_from_channel_colors(&colors));

        colors.insert("black".to_string(), 0);
        colors.insert("white".to_string(), 0x00ff_ffff);
        assert!(!nd2_is_indexed_from_channel_colors(&colors));

        colors.insert("dapi".to_string(), 0x0000_00ff);
        assert!(nd2_is_indexed_from_channel_colors(&colors));
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
    fn nd2_z_count_from_high_low_step_matches_java() {
        assert_eq!(
            nd2_z_count_from_range(Some(14.0), Some(-14.0), Some(1.0)),
            Some(29)
        );
        assert_eq!(
            nd2_z_count_from_range(Some(0.0), Some(0.0), Some(1.0)),
            None
        );
    }

    #[test]
    fn nd2_xml_ignores_filter_spectrum_wavelengths_like_java() {
        let xml = r#"<root>
          <Channel_0>
            <Name value="pdt-405"/>
            <EmWavelength value="460"/>
            <ExWavelength value="400"/>
          </Channel_0>
          <m_ExcitationSpectrum>
            <dWavelength value="488"/>
          </m_ExcitationSpectrum>
          <m_EmissionSpectrum>
            <dWavelength value="520"/>
          </m_EmissionSpectrum>
          <m_MirrorSpectrum>
            <dWavelength value="999"/>
          </m_MirrorSpectrum>
        </root>"#;
        let mut lv = Nd2LvValues::default();
        parse_nd2_xml_metadata(xml, &mut lv);
        assert!(lv.excitation_wavelengths.is_empty());
        assert_eq!(lv.emission_wavelengths, vec![460.0]);
    }

    #[test]
    fn nd2_xml_ui_count_inherits_previous_runtype_like_sax_handler() {
        let xml = r#"<root>
          <no_name runtype="RLxExperiment.RLxExpXYPosLoop">
            <uiCount runtype="lx_uint32" value="4"/>
          </no_name>
        </root>"#;

        assert_eq!(nd2_xml_ui_count_for_runtype(xml, "XYPosLoop"), Some(4));
    }

    #[test]
    fn nd2_xml_position_names_follow_java_pos_name_list() {
        let xml = r##"<root>
          <no_name runtype="RLxExperiment.RLxExpXYPosLoop">
            <uiCount runtype="lx_uint32" value="14"/>
            <pPosName runtype="CLxListVariant">
              <item_00000 runtype="CLxStringW" value="#3"/>
              <item_00001 runtype="CLxStringW" value="#4"/>
              <item_00002 runtype="CLxStringW" value="#5"/>
              <item_00003 runtype="CLxStringW" value="#6"/>
              <item_00004 runtype="CLxStringW" value="#7"/>
              <item_00005 runtype="CLxStringW" value="#8"/>
              <item_00006 runtype="CLxStringW" value="#9"/>
              <item_00007 runtype="CLxStringW" value="#10"/>
              <item_00008 runtype="CLxStringW" value=""/>
              <item_00009 runtype="CLxStringW" value=""/>
              <item_00010 runtype="CLxStringW" value=""/>
              <item_00011 runtype="CLxStringW" value=""/>
              <item_00012 runtype="CLxStringW" value=""/>
              <item_00013 runtype="CLxStringW" value=""/>
            </pPosName>
          </no_name>
          <pItemValid runtype="CLxListVariant">
            <_00 runtype="bool" value="true"/>
            <_01 runtype="bool" value="true"/>
            <_02 runtype="bool" value="true"/>
            <_03 runtype="bool" value="true"/>
            <_04 runtype="bool" value="true"/>
            <_05 runtype="bool" value="true"/>
            <_06 runtype="bool" value="true"/>
            <_07 runtype="bool" value="true"/>
            <_08 runtype="bool" value="false"/>
            <_09 runtype="bool" value="false"/>
            <_10 runtype="bool" value="true"/>
            <_11 runtype="bool" value="true"/>
            <_12 runtype="bool" value="true"/>
            <_13 runtype="bool" value="true"/>
          </pItemValid>
        </root>"##;

        let names = nd2_xml_old_jp2_valid_position_names(xml);
        assert_eq!(
            &names[..8],
            ["#3", "#4", "#5", "#6", "#7", "#8", "#9", "#10"]
        );
        assert_eq!(names.len(), 12);
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
    fn nd2_xml_accepts_single_quoted_attributes_like_sax() {
        let xml = r#"<root>
          <uiCount runtype='CLxTimeLoop' value='3'/>
          <dCalibration value='0.25'/>
          <dPosX><item_0 value='100.0'/><item_1 value='200.0'/></dPosX>
        </root>"#;
        let mut lv = Nd2LvValues::default();
        parse_nd2_xml_metadata(xml, &mut lv);

        assert_eq!(nd2_xml_ui_count_for_runtype(xml, "TimeLoop"), Some(3));
        assert_eq!(lv.calibration, Some(0.25));
        assert_eq!(lv.pos_x, vec![100.0, 200.0]);
    }

    #[test]
    fn nd2_xml_ndcontrol_loop_size_uses_java_loop_state_rules() {
        let xml = r#"<NDControl>
          <LoopState>
            <no_name value="1052433"/><no_name value="529"/>
            <no_name value="3856"/><no_name value="529"/>
          </LoopState>
          <LoopSize>
            <no_name value="1"/><no_name value="0"/>
            <no_name value="21"/><no_name value="0"/>
          </LoopSize>
        </NDControl>"#;

        assert_eq!(
            nd2_xml_ndcontrol_loop_dimensions(xml),
            Some((Some(21), Some(1)))
        );
    }

    #[test]
    fn nd2_xml_metadata_channels_supply_color_camera_names() {
        let xml = r#"<Metadata>
          <Channels>
            <Channel_0><Color value="16711680"/><Name value="Blue"/></Channel_0>
            <Channel_1><Color value="65280"/><Name value="Green"/></Channel_1>
            <Channel_2><Color value="255"/><Name value="Red"/></Channel_2>
          </Channels>
        </Metadata>"#;
        let mut lv = Nd2LvValues::default();
        parse_nd2_xml_metadata(xml, &mut lv);

        assert_eq!(lv.channel_names, vec!["Blue", "Green", "Red"]);
        assert_eq!(lv.channel_colors.get("Blue"), Some(&16711680));
        assert_eq!(lv.channel_colors.get("Green"), Some(&65280));
        assert_eq!(lv.channel_colors.get("Red"), Some(&255));
    }

    #[test]
    fn nd2_xml_attribute_names_match_exactly_like_sax() {
        let xml = r#"<root>
          <uiCount other_runtype='CLxTimeLoop' value='3'/>
          <uiCount runtype='CLxTimeLoop' other_value='9'>4</uiCount>
          <dCalibration other_value='0.5'>0.25</dCalibration>
          <dPosX><item_0 other_value='100.0'>200.0</item_0></dPosX>
        </root>"#;
        let mut lv = Nd2LvValues::default();
        parse_nd2_xml_metadata(xml, &mut lv);

        assert_eq!(nd2_xml_ui_count_for_runtype(xml, "TimeLoop"), Some(4));
        assert_eq!(lv.calibration, Some(0.25));
        assert_eq!(lv.pos_x, vec![200.0]);
    }

    #[test]
    fn nd2_xml_tag_names_match_exactly_like_sax() {
        let xml = r#"<root>
          <uiCountExtra runtype="CLxTimeLoop" value="3"/>
          <uiCount runtype="CLxTimeLoop" value="4"/>
          <dCalibrationExtra value="0.5"/>
          <dCalibration value="0.25"/>
          <dPosXExtra><item_0 value="100.0"/></dPosXExtra>
          <dPosX><item_0 value="200.0"/></dPosX>
        </root>"#;
        let mut lv = Nd2LvValues::default();
        parse_nd2_xml_metadata(xml, &mut lv);

        assert_eq!(nd2_xml_ui_count_for_runtype(xml, "TimeLoop"), Some(4));
        assert_eq!(lv.calibration, Some(0.25));
        assert_eq!(lv.pos_x, vec![200.0]);
    }

    #[test]
    fn nd2_xml_dimension_tags_do_not_match_prefix_names() {
        let xml = r#"<root>
          <uiWidthExtra value="99"/>
          <uiHeightExtra value="88"/>
          <uiCompExtra value="7"/>
          <uiBpcExtra value="16"/>
        </root>"#;

        assert_eq!(parse_nd2_attributes(xml), (0, 0, 1, 1, 0));
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
        assert!(lv.channel_names.is_empty());
        assert_eq!(lv.text_channel_names, vec!["DAPI".to_string()]);
        assert_eq!(lv.channel_colors.get("DAPI"), Some(&0x0000FF));
        assert_eq!(lv.exposure_time, vec![0.025]);
        assert_eq!(lv.position_count, 1);
    }

    #[test]
    fn nd2_binary_lv_extracts_image_attributes() {
        fn entry(ty: u8, name: &str, value: &[u8]) -> Vec<u8> {
            let mut e = vec![ty, name.chars().count() as u8];
            for u in name.encode_utf16() {
                e.extend_from_slice(&u.to_le_bytes());
            }
            e.extend_from_slice(value);
            e
        }

        let mut data = Vec::new();
        data.extend_from_slice(&entry(3, "uiWidth", &2424u32.to_le_bytes()));
        data.extend_from_slice(&entry(3, "uiHeight", &1800u32.to_le_bytes()));
        data.extend_from_slice(&entry(3, "uiComp", &3u32.to_le_bytes()));
        data.extend_from_slice(&entry(3, "uiBpcInMemory", &16u32.to_le_bytes()));
        data.extend_from_slice(&entry(3, "uiBpcSignificant", &14u32.to_le_bytes()));

        let mut lv = Nd2LvValues::default();
        parse_nd2_lv(&data, &mut lv);

        assert_eq!(lv.lv_size_x, Some(2424));
        assert_eq!(lv.lv_size_y, Some(1800));
        assert_eq!(lv.lv_size_c, Some(3));
        assert_eq!(lv.lv_bpc_in_memory, Some(16));
        assert_eq!(lv.lv_bpc_significant, Some(14));
    }

    #[test]
    fn nd2_binary_image_attributes_lossless_matches_java_flags() {
        fn attr(name: &str, value: i32) -> Vec<u8> {
            let mut out = vec![name.chars().count() as u8];
            for u in name.encode_utf16() {
                out.extend_from_slice(&u.to_le_bytes());
            }
            out.extend_from_slice(&value.to_le_bytes());
            out
        }

        // Java skips 6 bytes, consumes zero padding and one non-zero byte, then
        // reads flat attribute records. dCompressionParam >= 0 is lossless only
        // while eCompression <= 0 leaves canBeLossless true.
        let mut data = vec![0; 6];
        data.extend_from_slice(&[0, 0, 1]);
        data.extend_from_slice(&attr("dCompressionParam", 0));
        data.extend_from_slice(&attr("eCompression", 0));
        let mut lv = Nd2LvValues::default();
        parse_nd2_binary_image_attributes(&data, &mut lv);
        assert!(lv.is_lossless);
        assert_eq!(lv.attr_size_x, None);

        let mut data = vec![0; 6];
        data.extend_from_slice(&[0, 1]);
        data.extend_from_slice(&attr("uiWidth", 2424));
        data.extend_from_slice(&attr("uiHeight", 1800));
        data.extend_from_slice(&attr("uiComp", 3));
        data.extend_from_slice(&attr("uiBpcInMemory", 16));
        data.extend_from_slice(&attr("uiBpcSignificant", 14));
        data.extend_from_slice(&attr("dCompressionParam", 5));
        data.extend_from_slice(&attr("eCompression", 1));
        let mut lv = Nd2LvValues::default();
        parse_nd2_binary_image_attributes(&data, &mut lv);
        assert!(!lv.is_lossless);
        assert_eq!(lv.attr_size_x, Some(2424));
        assert_eq!(lv.attr_size_y, Some(1800));
        assert_eq!(lv.attr_size_c, Some(3));
        assert_eq!(lv.attr_bpc_in_memory, Some(16));
        assert_eq!(lv.attr_bpc_significant, Some(14));
    }

    #[test]
    fn nd2_image_metadata_lv_scan_starts_after_first_twelve_name_bytes_like_java() {
        fn attr_name(name: &str) -> Vec<u8> {
            let mut out = vec![name.chars().count() as u8 + 1];
            for u in name.encode_utf16() {
                out.extend_from_slice(&u.to_le_bytes());
            }
            out.extend_from_slice(&0u16.to_le_bytes());
            out
        }
        fn attr_i32(name: &str, value: i32) -> Vec<u8> {
            let mut out = attr_name(name);
            out.extend_from_slice(&value.to_le_bytes());
            out
        }

        let mut payload = Vec::new();
        // The unconsumed name suffix is four bytes for "ImageMetadataLV!".
        // Java's skipBytes(6) therefore consumes those four bytes plus these
        // two payload bytes before the zero/sentinel scan.
        payload.extend_from_slice(&[0, 0]);
        payload.extend_from_slice(&[0, 0, 1]);
        payload.extend_from_slice(&attr_name("SLxExperiment"));
        payload.extend_from_slice(&attr_i32("eType", 4));
        payload.extend_from_slice(&attr_i32("uiCount", 3));
        payload.extend_from_slice(&attr_i32("uiNextLevelCount", 1));
        payload.extend_from_slice(&attr_i32("eType", 1));
        payload.extend_from_slice(&attr_i32("uiCount", 2));
        payload.extend_from_slice(&attr_i32("uiNextLevelCount", 0));

        let scan = image_metadata_lv_scan_bytes("ImageMetadataLV!", &payload);
        let lv = parse_image_metadata_lv(&scan).expect("parse image metadata LV");

        assert!(lv.processed);
        assert_eq!(lv.order, "TZ");
        assert_eq!(lv.z_count, 3);
        assert_eq!(lv.time_count, 2);
    }

    #[test]
    fn nd2_split_channel_planes_map_logical_plane_to_stored_frame_like_java() {
        let path = temp_nd2_path("split_channels.nd2");
        fn lv_entry(ty: u8, name: &str, value: &[u8]) -> Vec<u8> {
            let mut e = vec![ty, name.chars().count() as u8];
            for u in name.encode_utf16() {
                e.extend_from_slice(&u.to_le_bytes());
            }
            e.extend_from_slice(value);
            e
        }

        let mut attrs = vec![0; 6];
        attrs.extend_from_slice(&[0, 1]);
        attrs.extend_from_slice(&nd2_attr("uiWidth", 2));
        attrs.extend_from_slice(&nd2_attr("uiHeight", 1));
        attrs.extend_from_slice(&nd2_attr("uiComp", 2));
        attrs.extend_from_slice(&nd2_attr("uiBpcInMemory", 8));
        attrs.extend_from_slice(&nd2_attr("uiBpcSignificant", 8));

        let mut bytes = Vec::new();
        bytes.extend_from_slice(&nd2_chunk("ImageAttributesLV!", &attrs));
        bytes.extend_from_slice(&nd2_chunk(
            "ImageMetadataLV!",
            &lv_entry(6, "dExposureTime", &10.0f64.to_bits().to_le_bytes()),
        ));
        // Stored interleaved samples: pixel0 C0/C1, pixel1 C0/C1.
        bytes.extend_from_slice(&nd2_chunk("ImageDataSeq|0!", &[1, 11, 2, 12]));
        std::fs::write(&path, bytes).unwrap();

        let mut reader = Nd2Reader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 1);
        assert_eq!(meta.size_c, 2);
        assert_eq!(meta.size_t, 1);
        assert_eq!(meta.image_count, 2);
        assert_eq!(
            meta.series_metadata
                .get("nd2_split_channels")
                .map(|v| v.to_string()),
            Some("true".to_string())
        );

        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![11, 12]);
        assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(), vec![12]);

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].planes.len(), 2);
        assert_eq!(ome.images[0].planes[0].the_c, 0);
        assert_eq!(ome.images[0].planes[1].the_c, 1);

        let _ = std::fs::remove_file(path);
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
    fn nd2_parse_text_recovers_dimension_annotations_like_java() {
        let text = "Metadata:\nDimensions: T'(41) x XY(4) x Z(7)\nTime Loop: 41\nZ Stack Loop: 7";
        let mut backup = Nd2LvValues::default();
        parse_text(text, &mut backup);

        assert_eq!(backup.text_size_t, Some(41));
        assert_eq!(backup.text_series_count, Some(4));
        assert_eq!(backup.text_size_z, Some(7));
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
