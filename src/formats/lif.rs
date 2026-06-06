//! Leica LIF (Leica Image Format) reader.
//!
//! LIF is a binary Leica container: a sequence of memory blocks, the first of
//! which carries a UTF-16LE XML description (`<LMSDataContainerHeader>` with an
//! `<Element>` tree). Each `<Element>` that holds an `<Image>` describes one
//! series: channels (`<ChannelDescription>`) and dimensions
//! (`<DimensionDescription>` with `DimID` 1=X 2=Y 3=Z 4=T, 10=tile). Subsequent
//! binary blocks carry the pixel payloads, matched to the XML `<Memory>`
//! entries by their memory-block IDs.
//!
//! This port mirrors the upstream Java `LIFReader`: it parses the block layout,
//! enumerates series from the XML, derives per-series dimensions / pixel type,
//! and maps memory-block IDs to pixel data offsets. Tiled acquisitions are
//! expanded into one series per tile, matching the Java behaviour.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use quick_xml::events::Event;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

const LIF_MAGIC_BYTE: u8 = 0x70;
const LIF_MEMORY_BYTE: u8 = 0x2a;

/// One pixel-data memory block: a byte offset into the file plus its ID.
#[derive(Debug, Clone)]
struct MemoryBlock {
    file_offset: u64,
    id: String,
}

/// Per-series core metadata derived from one `<Image>` element.
#[derive(Debug, Clone)]
struct SeriesInfo {
    meta: ImageMetadata,
    /// Number of tiles this image was split into (>=1).
    tile_count: u32,
    /// Bytes-per-tile increment from the tile dimension (DimID 10).
    tile_bytes_inc: u64,
}

pub struct LifReader {
    path: Option<PathBuf>,
    /// One entry per (expanded) series; tiled images contribute `tile_count`
    /// identical entries.
    series: Vec<SeriesInfo>,
    /// One memory block per *tile group* (i.e. per original `<Image>` element).
    memory_blocks: Vec<MemoryBlock>,
    /// File offset where pixel data ends (next block or EOF).
    end_pointer: u64,
    current_series: usize,
    file_len: u64,
}

impl LifReader {
    pub fn new() -> Self {
        LifReader {
            path: None,
            series: Vec::new(),
            memory_blocks: Vec::new(),
            end_pointer: 0,
            current_series: 0,
            file_len: 0,
        }
    }

    fn cur(&self) -> Result<&SeriesInfo> {
        self.series
            .get(self.current_series)
            .ok_or(BioFormatsError::SeriesOutOfRange(self.current_series))
    }

    /// Map an (expanded) series index to its tile-group index, i.e. the index
    /// into `memory_blocks`. Mirrors Java `getTileIndex`.
    fn tile_index(&self, series: usize) -> usize {
        let mut count = 0usize;
        for (group, info) in self.tile_groups().iter().enumerate() {
            if series < count + info.tile_count.max(1) as usize {
                return group;
            }
            count += info.tile_count.max(1) as usize;
        }
        0
    }

    /// Returns one `SeriesInfo` per original `<Image>` element (deduplicated
    /// from the expanded per-tile list).
    fn tile_groups(&self) -> Vec<&SeriesInfo> {
        let mut groups: Vec<&SeriesInfo> = Vec::new();
        let mut i = 0usize;
        while i < self.series.len() {
            groups.push(&self.series[i]);
            i += self.series[i].tile_count.max(1) as usize;
        }
        groups
    }

    /// The tile number of `series` within its tile group.
    fn tile_within_group(&self, series: usize) -> usize {
        let group = self.tile_index(series);
        let mut count = 0usize;
        for info in self.tile_groups().iter().take(group) {
            count += info.tile_count.max(1) as usize;
        }
        series - count
    }

    fn parse(&mut self, data: &[u8]) -> Result<()> {
        if data.len() < 13 {
            return Err(BioFormatsError::Format("LIF file too short".into()));
        }
        self.file_len = data.len() as u64;

        // -- header --
        // byte 0: magic; skip 2; byte 3: magic; skip 4; then 0x2A + XML
        let check_one = data[0];
        let check_two = data[3];
        if check_one != LIF_MAGIC_BYTE && check_two != LIF_MAGIC_BYTE {
            return Err(BioFormatsError::Format("Not a valid Leica LIF file".into()));
        }
        let mut off: usize = 8;
        if data[off] != LIF_MEMORY_BYTE {
            return Err(BioFormatsError::Format("Invalid LIF XML description".into()));
        }
        off += 1;
        let nc = read_i32(data, off)? as i64;
        off += 4;
        let xml_bytes = nc
            .checked_mul(2)
            .filter(|n| *n >= 0)
            .ok_or_else(|| BioFormatsError::Format("Invalid LIF XML length".into()))?
            as usize;
        if off + xml_bytes > data.len() {
            return Err(BioFormatsError::Format("LIF XML extends past EOF".into()));
        }
        let xml = decode_utf16le(&data[off..off + xml_bytes]);
        off += xml_bytes;

        // -- memory blocks --
        let mut raw_blocks: Vec<MemoryBlock> = Vec::new();
        let mut end_pointer: u64 = 0;
        while off < data.len() {
            if off + 4 > data.len() {
                break;
            }
            let check = read_i32(data, off)?;
            off += 4;
            if check != LIF_MAGIC_BYTE as i32 {
                if check == 0 && !raw_blocks.is_empty() {
                    // newer LIF: trailing zeros after the last block
                    end_pointer = off as u64;
                    break;
                }
                return Err(BioFormatsError::Format(format!(
                    "Invalid LIF memory block: magic {check}"
                )));
            }
            off += 4; // skip the per-block size word
            if off >= data.len() || data[off] != LIF_MEMORY_BYTE {
                return Err(BioFormatsError::Format(
                    "Invalid LIF memory description".into(),
                ));
            }
            off += 1;
            let mut block_length: u64 = read_i32(data, off)? as u32 as u64;
            off += 4;
            let test = data.get(off).copied();
            off += 1;
            if test != Some(LIF_MEMORY_BYTE) {
                // BigTIFF-style 64-bit length: rewind to before the int32.
                off -= 5;
                block_length = read_i64(data, off)? as u64;
                off += 8;
                if data.get(off).copied() != Some(LIF_MEMORY_BYTE) {
                    return Err(BioFormatsError::Format(
                        "Invalid LIF memory description (64-bit)".into(),
                    ));
                }
                off += 1;
            }
            let descr_len = (read_i32(data, off)? as usize)
                .checked_mul(2)
                .ok_or_else(|| BioFormatsError::Format("Invalid LIF block ID length".into()))?;
            off += 4;
            if off + descr_len > data.len() {
                return Err(BioFormatsError::Format("LIF block ID past EOF".into()));
            }
            let mem_id = decode_utf16le(&data[off..off + descr_len]);
            off += descr_len;
            if block_length > 0 {
                raw_blocks.push(MemoryBlock {
                    file_offset: off as u64,
                    id: mem_id,
                });
            }
            off = off.saturating_add(block_length as usize);
        }
        if end_pointer == 0 {
            end_pointer = data.len() as u64;
        }
        self.end_pointer = end_pointer;

        // -- XML metadata --
        let (series, ordered_ids) = parse_xml(&xml)?;
        if series.is_empty() {
            return Err(BioFormatsError::Format("No images found in LIF".into()));
        }
        self.series = series;

        // Match memory blocks to image elements by ID, preserving the XML
        // order. Fall back to file order if IDs do not match.
        let mut matched: Vec<MemoryBlock> = Vec::new();
        for id in &ordered_ids {
            if let Some(b) = raw_blocks.iter().find(|b| &b.id == id) {
                matched.push(b.clone());
            }
        }
        self.memory_blocks = if matched.len() == ordered_ids.len() && !matched.is_empty() {
            matched
        } else {
            raw_blocks
        };

        Ok(())
    }
}

impl Default for LifReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LifReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lif"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // magic; skip 7; 0x2A; int32 nc; string == "LMS_Object_File" => LOF, not LIF
        if header.len() < 13 {
            return false;
        }
        if header[0] != LIF_MAGIC_BYTE {
            return false;
        }
        if header[8] != LIF_MEMORY_BYTE {
            return false;
        }
        let nc = match read_i32(header, 9) {
            Ok(v) => v as i64,
            Err(_) => return false,
        };
        if nc < 0 {
            return false;
        }
        let want = (nc * 2) as usize;
        let avail = header.len() - 13;
        let take = want.min(avail);
        let desc = decode_utf16le(&header[13..13 + take]);
        desc != "LMS_Object_File"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = Some(path.to_path_buf());
        self.current_series = 0;
        self.series.clear();
        self.memory_blocks.clear();

        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        self.parse(&data)?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series.clear();
        self.memory_blocks.clear();
        self.end_pointer = 0;
        self.current_series = 0;
        self.file_len = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if series >= self.series.len() {
            return Err(BioFormatsError::SeriesOutOfRange(series));
        }
        self.current_series = series;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        match self.series.get(self.current_series) {
            Some(info) => &info.meta,
            None => crate::common::reader::uninitialized_metadata(),
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (w, h) = {
            let m = &self.cur()?.meta;
            (m.size_x, m.size_y)
        };
        self.open_bytes_region(plane_index, 0, 0, w, h)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let path = self
            .path
            .clone()
            .ok_or_else(|| BioFormatsError::Format("LIF reader not initialized".into()))?;

        let series = self.current_series;
        let info = self.cur()?.clone();
        let m = &info.meta;
        if plane_index >= m.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if x + w > m.size_x || y + h > m.size_y {
            return Err(BioFormatsError::Format("LIF region out of bounds".into()));
        }

        let bytes_per_pixel = m.pixel_type.bytes_per_sample();
        let rgb_channels = if m.is_rgb { m.size_c as usize } else { 1 };
        let bpp = bytes_per_pixel * rgb_channels;
        let plane_size = m.size_x as u64 * m.size_y as u64 * bpp as u64;

        let group = self.tile_index(series);
        let block = self
            .memory_blocks
            .get(group)
            .ok_or(BioFormatsError::SeriesOutOfRange(series))?;
        let data_offset = block.file_offset;

        // bytesToSkip handles row padding for widths not divisible by 4.
        let next_offset = self
            .memory_blocks
            .get(group + 1)
            .map(|b| b.file_offset)
            .unwrap_or(self.end_pointer);
        let mut bytes_to_skip: i64 = next_offset as i64
            - data_offset as i64
            - (plane_size as i64) * (m.image_count as i64);
        if m.size_y > 0 {
            bytes_to_skip /= m.size_y as i64;
        }
        if m.size_x % 4 == 0 || bytes_to_skip < 0 {
            bytes_to_skip = 0;
        }
        let bytes_to_skip = bytes_to_skip as u64;

        // seekStartOfPlane: account for tiles.
        let pos_in_file = self.seek_start_of_plane(series, plane_index, data_offset, plane_size);

        let mut file = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        use std::io::{Read, Seek, SeekFrom};

        let row_bytes = w as usize * bpp;
        let mut out = vec![0u8; row_bytes * h as usize];

        if bytes_to_skip == 0 {
            // Contiguous plane: read the requested region row by row.
            let base = pos_in_file;
            for row in 0..h as usize {
                let row_off = base
                    + ((y as u64 + row as u64) * m.size_x as u64 + x as u64) * bpp as u64;
                file.seek(SeekFrom::Start(row_off)).map_err(BioFormatsError::Io)?;
                file.read_exact(&mut out[row * row_bytes..(row + 1) * row_bytes])
                    .map_err(BioFormatsError::Io)?;
            }
        } else {
            // Padded rows.
            let mut cursor = pos_in_file;
            cursor += bytes_to_skip * (m.size_y as u64) * (plane_index as u64);
            cursor += (y as u64) * (m.size_x as u64 * bpp as u64 + bytes_to_skip);
            file.seek(SeekFrom::Start(cursor)).map_err(BioFormatsError::Io)?;
            for row in 0..h as usize {
                file.seek(SeekFrom::Current((x as i64) * bpp as i64))
                    .map_err(BioFormatsError::Io)?;
                file.read_exact(&mut out[row * row_bytes..(row + 1) * row_bytes])
                    .map_err(BioFormatsError::Io)?;
                let skip = bpp as i64 * (m.size_x as i64 - w as i64 - x as i64)
                    + bytes_to_skip as i64;
                file.seek(SeekFrom::Current(skip)).map_err(BioFormatsError::Io)?;
            }
        }

        // RGB (interleaved) planes are stored BGR; swap to RGB.
        if rgb_channels == 3 && m.is_interleaved {
            for px in out.chunks_mut(3 * bytes_per_pixel) {
                if px.len() == 3 * bytes_per_pixel {
                    for b in 0..bytes_per_pixel {
                        px.swap(b, 2 * bytes_per_pixel + b);
                    }
                }
            }
        }

        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }
}

impl LifReader {
    fn seek_start_of_plane(
        &self,
        series: usize,
        no: u32,
        data_offset: u64,
        plane_size: u64,
    ) -> u64 {
        let group = self.tile_index(series);
        let info = &self.tile_groups()[group];
        let number_of_tiles = info.tile_count.max(1);
        if number_of_tiles > 1 && plane_size > 0 {
            let bytes_inc_per_tile = info.tile_bytes_inc;
            let frames_per_tile = (bytes_inc_per_tile / plane_size).max(1);
            let no_outside = no as u64 / frames_per_tile;
            let no_inside = no as u64 % frames_per_tile;
            let tile = self.tile_within_group(series) as u64;
            let mut pos = data_offset;
            pos += no_outside * bytes_inc_per_tile * number_of_tiles as u64;
            pos += tile * bytes_inc_per_tile;
            pos += no_inside * plane_size;
            pos
        } else {
            data_offset + no as u64 * plane_size
        }
    }
}

// -- byte helpers --

fn read_i32(data: &[u8], off: usize) -> Result<i32> {
    if off + 4 > data.len() {
        return Err(BioFormatsError::Format("LIF: read past EOF (i32)".into()));
    }
    Ok(i32::from_le_bytes([
        data[off],
        data[off + 1],
        data[off + 2],
        data[off + 3],
    ]))
}

fn read_i64(data: &[u8], off: usize) -> Result<i64> {
    if off + 8 > data.len() {
        return Err(BioFormatsError::Format("LIF: read past EOF (i64)".into()));
    }
    let mut b = [0u8; 8];
    b.copy_from_slice(&data[off..off + 8]);
    Ok(i64::from_le_bytes(b))
}

/// Decode UTF-16LE, stripping trailing/leading NULs (Java `stripString`).
fn decode_utf16le(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    String::from_utf16_lossy(&units)
        .trim_matches(|c: char| c == '\u{0}')
        .to_string()
}

// -- XML parsing --

/// A minimal DOM node built from the LIF XML so we can walk parent/child
/// relationships the way the Java reader does.
struct Node {
    name: String,
    attrs: BTreeMap<String, String>,
    children: Vec<usize>,
    parent: Option<usize>,
}

struct Dom {
    nodes: Vec<Node>,
}

impl Dom {
    fn parse(xml: &str) -> Result<Dom> {
        let mut reader = quick_xml::Reader::from_str(xml);
        reader.config_mut().trim_text(false);
        let mut nodes: Vec<Node> = Vec::new();
        let mut stack: Vec<usize> = Vec::new();

        let push_node =
            |nodes: &mut Vec<Node>, stack: &[usize], e: &quick_xml::events::BytesStart| -> usize {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                let mut attrs = BTreeMap::new();
                for a in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(a.key.as_ref()).to_string();
                    let val = a.unescape_value().map(|v| v.to_string()).unwrap_or_default();
                    attrs.insert(key, val);
                }
                let parent = stack.last().copied();
                let idx = nodes.len();
                nodes.push(Node {
                    name,
                    attrs,
                    children: Vec::new(),
                    parent,
                });
                if let Some(p) = parent {
                    nodes[p].children.push(idx);
                }
                idx
            };

        loop {
            match reader.read_event() {
                Ok(Event::Start(e)) => {
                    let idx = push_node(&mut nodes, &stack, &e);
                    stack.push(idx);
                }
                Ok(Event::Empty(e)) => {
                    push_node(&mut nodes, &stack, &e);
                }
                Ok(Event::End(_)) => {
                    stack.pop();
                }
                Ok(Event::Eof) => break,
                Ok(_) => {}
                Err(e) => {
                    return Err(BioFormatsError::Format(format!("LIF XML parse error: {e}")))
                }
            }
        }
        Ok(Dom { nodes })
    }

    fn children_named<'a>(&'a self, idx: usize, name: &'a str) -> impl Iterator<Item = usize> + 'a {
        self.nodes[idx]
            .children
            .iter()
            .copied()
            .filter(move |c| self.nodes[*c].name == name)
    }

    /// First descendant (any depth) with the given tag name.
    fn first_descendant(&self, idx: usize, name: &str) -> Option<usize> {
        for &c in &self.nodes[idx].children {
            if self.nodes[c].name == name {
                return Some(c);
            }
            if let Some(found) = self.first_descendant(c, name) {
                return Some(found);
            }
        }
        None
    }

    /// All descendants (any depth) with the given tag name.
    fn descendants(&self, idx: usize, name: &str, out: &mut Vec<usize>) {
        for &c in &self.nodes[idx].children {
            if self.nodes[c].name == name {
                out.push(c);
            }
            self.descendants(c, name, out);
        }
    }
}

/// Parse the LIF XML, returning (expanded series list, ordered memory-block IDs
/// one per original image element).
fn parse_xml(xml: &str) -> Result<(Vec<SeriesInfo>, Vec<String>)> {
    let dom = Dom::parse(xml)?;
    if dom.nodes.is_empty() {
        return Err(BioFormatsError::Format("Empty LIF XML".into()));
    }
    let root = 0usize;

    let mut image_nodes: Vec<usize> = Vec::new();
    dom.descendants(root, "Image", &mut image_nodes);

    let mut series: Vec<SeriesInfo> = Vec::new();
    let mut ordered_ids: Vec<String> = Vec::new();

    for &img in &image_nodes {
        // Java: grandparent = image.parent.parent (the owning <Element>).
        let parent = match dom.nodes[img].parent {
            Some(p) => p,
            None => continue,
        };
        let grandparent = match dom.nodes[parent].parent {
            Some(g) => g,
            None => continue,
        };
        // Skip event-list references (grandparent named ProcessingHistory).
        if dom.nodes[grandparent].name == "ProcessingHistory" {
            continue;
        }
        // Find the Memory child of the grandparent.
        let mem_id = dom
            .children_named(grandparent, "Memory")
            .next()
            .and_then(|m| dom.nodes[m].attrs.get("MemoryBlockID").cloned());

        let info = translate_image(&dom, img)?;
        let tiles = info.tile_count.max(1);
        for _ in 0..tiles {
            series.push(info.clone());
        }
        ordered_ids.push(mem_id.unwrap_or_default());
    }

    Ok((series, ordered_ids))
}

/// Mirror of Java `translateImageNodes`: derive dimensions/pixel type from the
/// `<ImageDescription>` of one `<Image>` element.
fn translate_image(dom: &Dom, img: usize) -> Result<SeriesInfo> {
    let mut m = ImageMetadata {
        is_little_endian: true,
        ..ImageMetadata::default()
    };

    let image_desc = dom
        .first_descendant(img, "ImageDescription")
        .ok_or_else(|| BioFormatsError::Format("LIF image has no ImageDescription".into()))?;

    // Channels.
    let channels_node = dom.first_descendant(image_desc, "Channels");
    let mut channel_nodes: Vec<usize> = Vec::new();
    if let Some(cn) = channels_node {
        dom.descendants(cn, "ChannelDescription", &mut channel_nodes);
    }
    m.size_c = channel_nodes.len().max(1) as u32;

    // bytesPerAxis: sorted map nBytes -> axis, used to derive dimension order.
    let mut bytes_per_axis: BTreeMap<u64, char> = BTreeMap::new();
    for &ch in &channel_nodes {
        if let Some(bi) = dom.nodes[ch].attrs.get("BytesInc") {
            if let Ok(b) = bi.trim().parse::<u64>() {
                if b > 0 {
                    bytes_per_axis.insert(b, 'C');
                }
            }
        }
    }

    // Dimensions.
    let dims_node = dom.first_descendant(image_desc, "Dimensions");
    let mut dim_nodes: Vec<usize> = Vec::new();
    if let Some(dn) = dims_node {
        dom.descendants(dn, "DimensionDescription", &mut dim_nodes);
    }

    let mut tile_count: u32 = 1;
    let mut tile_bytes_inc: u64 = 0;
    let mut extras: u64 = 1;
    let mut size_z: u32 = 0;
    let mut size_t: u32 = 0;
    let mut size_x: u32 = 0;
    let mut size_y: u32 = 0;
    let mut is_rgb = false;
    let mut pixel_type = PixelType::Uint8;

    for &d in &dim_nodes {
        let attrs = &dom.nodes[d].attrs;
        let id: i32 = attrs
            .get("DimID")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let len: u32 = attrs
            .get("NumberOfElements")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);
        let mut n_bytes: u64 = attrs
            .get("BytesInc")
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(0);

        match id {
            1 => {
                size_x = len;
                is_rgb = n_bytes > 0 && n_bytes % 3 == 0;
                if is_rgb {
                    n_bytes /= 3;
                }
                pixel_type = pixel_type_from_bytes(n_bytes);
            }
            2 => {
                if size_y != 0 {
                    if size_z <= 1 {
                        size_z = len;
                        bytes_per_axis.insert(n_bytes, 'Z');
                    } else if size_t <= 1 {
                        size_t = len;
                        bytes_per_axis.insert(n_bytes, 'T');
                    }
                } else {
                    size_y = len;
                }
            }
            3 => {
                if size_y == 0 {
                    // XZ scan: swap Y and Z
                    size_y = len;
                    size_z = 1;
                    bytes_per_axis.insert(n_bytes, 'Y');
                } else {
                    size_z = len;
                    bytes_per_axis.insert(n_bytes, 'Z');
                }
            }
            4 => {
                if size_y == 0 {
                    // XT scan: swap Y and T
                    size_y = len;
                    size_t = 1;
                    bytes_per_axis.insert(n_bytes, 'Y');
                } else {
                    size_t = len;
                    bytes_per_axis.insert(n_bytes, 'T');
                }
            }
            10 => {
                tile_count = tile_count.saturating_mul(len.max(1));
                tile_bytes_inc = n_bytes;
            }
            _ => {
                extras = extras.saturating_mul(len.max(1) as u64);
            }
        }
    }

    if extras > 1 {
        if size_z <= 1 {
            size_z = extras as u32;
        } else if size_t == 0 {
            size_t = extras as u32;
        } else {
            size_t = size_t.saturating_mul(extras as u32);
        }
    }

    if m.size_c == 0 {
        m.size_c = 1;
    }
    let size_z = size_z.max(1);
    let size_t = size_t.max(1);
    let size_x = size_x.max(1);
    let size_y = size_y.max(1);

    m.size_x = size_x;
    m.size_y = size_y;
    m.size_z = size_z;
    m.size_t = size_t;
    m.pixel_type = pixel_type;
    m.bits_per_pixel = (pixel_type.bytes_per_sample() * 8) as u8;
    m.is_rgb = is_rgb;
    m.is_interleaved = is_rgb;
    m.is_indexed = !is_rgb;

    let rgb_channel_count = if is_rgb { m.size_c } else { 1 };
    m.image_count = size_z * size_t * (m.size_c / rgb_channel_count.max(1));

    Ok(SeriesInfo {
        meta: m,
        tile_count,
        tile_bytes_inc,
    })
}

/// Java `FormatTools.pixelTypeFromBytes(nBytes, signed=false, fp=true)`:
/// LIF channels are unsigned integer.
fn pixel_type_from_bytes(n_bytes: u64) -> PixelType {
    match n_bytes {
        0 | 1 => PixelType::Uint8,
        2 => PixelType::Uint16,
        4 => PixelType::Uint32,
        8 => PixelType::Float64,
        _ => PixelType::Uint8,
    }
}

#[cfg(test)]
mod tests {
    use super::LifReader;
    use crate::common::reader::FormatReader;

    #[test]
    fn rejects_non_lif_bytes() {
        let reader = LifReader::new();
        assert!(!reader.is_this_type_by_bytes(b"not a real lif file at all!!"));
    }

    #[test]
    fn detects_lif_extension() {
        let reader = LifReader::new();
        assert!(reader.is_this_type_by_name(std::path::Path::new("foo.lif")));
        assert!(reader.is_this_type_by_name(std::path::Path::new("FOO.LIF")));
        assert!(!reader.is_this_type_by_name(std::path::Path::new("foo.tif")));
    }

    #[test]
    fn set_id_fails_cleanly_on_garbage() {
        let path = std::env::temp_dir().join("bioformats_lif_garbage.lif");
        std::fs::write(&path, b"not a real lif").unwrap();
        let mut reader = LifReader::new();
        assert!(reader.set_id(&path).is_err());
        let _ = std::fs::remove_file(path);
    }
}
