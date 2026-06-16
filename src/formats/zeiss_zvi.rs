//! Zeiss ZVI format reader (OLE2/CFB container).
//!
//! ZVI is the Zeiss AxioVision proprietary microscopy format.
//! It uses OLE2 Compound File Binary (CFB) as its container — the same
//! format as old Microsoft Office .doc/.xls files.
//!
//! Key streams:
//!   /Image/CONTENTS            — global metadata (width, height, pixel type)
//!   /Image/Item(N)/CONTENTS    — per-plane pixel data (N is 1-based)
//!   /Image/Item(N)/Tags/CONTENTS — per-plane z/c/t indices

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

pub struct ZeissZviReader {
    path: Option<PathBuf>,
    comp: Option<cfb::CompoundFile<File>>,
    meta: Option<ImageMetadata>,
    planes: Vec<ZviPlane>,
    bytes_per_pixel: usize,
    is_rgb: bool,
    /// Number of tiles; each tile is exposed as a separate series, matching
    /// ZeissZVIReader where `totalTiles = offsets.length / getImageCount()` and
    /// `coordinates[i][3]` (the tile index) selects the series.
    tile_count: usize,
    current_series: usize,
    /// OME enrichment harvested from the per-item Tags streams.
    ome_info: ZviOmeInfo,
}

/// OME metadata harvested from ZVI tag streams, mirroring the subset of
/// BaseZeissReader.parseMainTags that the parity harness checks: physical pixel
/// sizes and per-channel name / emission / excitation, keyed by channel index.
#[derive(Default, Clone)]
struct ZviOmeInfo {
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    physical_size_z: Option<f64>,
    /// channel index -> name
    channel_names: HashMap<u32, String>,
    /// channel index -> emission wavelength (nm)
    emission: HashMap<u32, f64>,
    /// channel index -> excitation wavelength (nm)
    excitation: HashMap<u32, f64>,
}

struct ZviPlane {
    /// Stream path inside the CFB, e.g. "/Image/Item(1)/CONTENTS"
    stream_path: String,
    z: u32,
    c: u32,
    t: u32,
    /// Tile (mosaic) index — maps to the Bio-Formats series.
    tile: u32,
    /// Byte offset of pixel data within the item stream.
    data_offset: usize,
    is_zlib: bool,
    is_jpeg: bool,
}

/// The immediate parent directory component of a "…/<dir>/Contents" path.
///
/// Java derives `dirName` this way (the directory directly containing the
/// CONTENTS stream) and dispatches on it, so the image-item test must look only
/// at the parent dir, not the whole path — otherwise unrelated nested "Item(n)"
/// directories (Layers, RootFolder Locations, …) would also match.
fn parent_dir(p: &str) -> &str {
    let trimmed = p.strip_suffix('/').unwrap_or(p);
    let last_slash = match trimmed.rfind('/') {
        Some(i) => i,
        None => return "",
    };
    let dir_path = &trimmed[..last_slash];
    match dir_path.rfind('/') {
        Some(i) => &dir_path[i + 1..],
        None => dir_path,
    }
}

/// Port of `MetadataTools.makeSaneDimensionOrder`: keep only XYZCT characters,
/// then append any missing axis in the fixed precedence X, Y, C, Z, T and drop
/// duplicate occurrences. Maps the resulting 5-char string to a [`DimensionOrder`].
fn make_sane_dimension_order(input: &str) -> DimensionOrder {
    let mut order = String::new();
    for ch in input.to_ascii_uppercase().chars() {
        if matches!(ch, 'X' | 'Y' | 'Z' | 'C' | 'T') && !order.contains(ch) {
            order.push(ch);
        }
    }
    for axis in ['X', 'Y', 'C', 'Z', 'T'] {
        if !order.contains(axis) {
            order.push(axis);
        }
    }
    match order.as_str() {
        "XYCTZ" => DimensionOrder::XYCTZ,
        "XYCZT" => DimensionOrder::XYCZT,
        "XYTCZ" => DimensionOrder::XYTCZ,
        "XYTZC" => DimensionOrder::XYTZC,
        "XYZCT" => DimensionOrder::XYZCT,
        "XYZTC" => DimensionOrder::XYZTC,
        // makeSaneDimensionOrder can only yield the six XY-prefixed permutations
        // above; anything else means a logic error, so fall back to XYCZT.
        _ => DimensionOrder::XYCZT,
    }
}

/// The Z/C/T axis characters of a [`DimensionOrder`], in order (X, Y omitted).
fn dimension_order_axes(order: DimensionOrder) -> Vec<char> {
    let s = match order {
        DimensionOrder::XYCTZ => "CTZ",
        DimensionOrder::XYCZT => "CZT",
        DimensionOrder::XYTCZ => "TCZ",
        DimensionOrder::XYTZC => "TZC",
        DimensionOrder::XYZCT => "ZCT",
        DimensionOrder::XYZTC => "ZTC",
    };
    s.chars().collect()
}

fn zvi_tag_name(tag_id: u32) -> &'static str {
    match tag_id {
        515 => "ImageWidth",
        516 => "ImageHeight",
        518 => "PixelType",
        769 => "Scale Factor for X",
        770 => "Scale Unit for X",
        772 => "Scale Factor for Y",
        773 => "Scale Unit for Y",
        1025 | 1047 => "Camera Acquisition Time",
        1284 => "Channel Name",
        1537 => "Title",
        1538 => "Author",
        1540 => "Comments",
        1553 => "Filename",
        1793 => "Acquisition Date",
        1801 => "User Name",
        _ => "Unknown",
    }
}

/// Port of `DataTools.stripString`: drop NUL characters (ZVI strings are stored
/// UTF-16LE, so bytes decode to interleaved NULs) and trim surrounding
/// whitespace. This turns e.g. "C\0y\05" back into "Cy5".
fn strip_string(raw: &[u8]) -> String {
    String::from_utf8_lossy(raw)
        .chars()
        .filter(|&c| c != '\0')
        .collect::<String>()
        .trim()
        .to_string()
}

fn read_zvi_variant(data: &[u8], offset: &mut usize) -> Option<String> {
    let ty = u16::from_le_bytes(data.get(*offset..*offset + 2)?.try_into().ok()?);
    *offset += 2;
    let value = match ty {
        0 | 1 => String::new(),
        2 => {
            let v = i16::from_le_bytes(data.get(*offset..*offset + 2)?.try_into().ok()?);
            *offset += 2;
            v.to_string()
        }
        3 | 22 => {
            let v = i32::from_le_bytes(data.get(*offset..*offset + 4)?.try_into().ok()?);
            *offset += 4;
            v.to_string()
        }
        4 => {
            let v = f32::from_le_bytes(data.get(*offset..*offset + 4)?.try_into().ok()?);
            *offset += 4;
            v.to_string()
        }
        5 | 7 => {
            let v = f64::from_le_bytes(data.get(*offset..*offset + 8)?.try_into().ok()?);
            *offset += 8;
            v.to_string()
        }
        8 | 69 => {
            let len = u32::from_le_bytes(data.get(*offset..*offset + 4)?.try_into().ok()?) as usize;
            *offset += 4;
            let raw = data.get(*offset..*offset + len)?;
            *offset += len;
            strip_string(raw)
        }
        11 => {
            let v = u16::from_le_bytes(data.get(*offset..*offset + 2)?.try_into().ok()?) != 0;
            *offset += 2;
            v.to_string()
        }
        19 | 23 => {
            let v = u32::from_le_bytes(data.get(*offset..*offset + 4)?.try_into().ok()?);
            *offset += 4;
            v.to_string()
        }
        20 | 21 => {
            let v = u64::from_le_bytes(data.get(*offset..*offset + 8)?.try_into().ok()?);
            *offset += 8;
            v.to_string()
        }
        66 => {
            let len = u16::from_le_bytes(data.get(*offset..*offset + 2)?.try_into().ok()?) as usize;
            *offset += 2;
            let raw = data.get(*offset..*offset + len)?;
            *offset += len;
            strip_string(raw)
        }
        _ => return None,
    };
    Some(value)
}

/// Harvest OME-relevant tags from one item's Tags stream, mirroring
/// BaseZeissReader.parseMainTags. Reads each (value, tagID) record in stream
/// order, tracking the current channel index (tag 2820 "Image Channel Index")
/// so that channel name (1284), emission (16777489), and excitation (16777488)
/// are attributed to the right channel. Physical sizes come from tags 769/772/775
/// ("Scale Factor for X/Y/Z"); Java keeps the first value seen for each.
///
/// `c_index` is threaded across all item streams to match Java's stateful field.
fn harvest_zvi_ome_tags(data: &[u8], info: &mut ZviOmeInfo, c_index: &mut i32) {
    const TAG_SCALE_X: u32 = 769;
    const TAG_SCALE_Y: u32 = 772;
    const TAG_SCALE_Z: u32 = 775;
    const TAG_CHANNEL_NAME: u32 = 1284;
    const TAG_CHANNEL_INDEX: u32 = 2820;
    const TAG_EXCITATION: u32 = 16_777_488;
    const TAG_EMISSION: u32 = 16_777_489;

    if data.len() < 12 {
        return;
    }
    let count = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let mut offset = 12;
    for _ in 0..count {
        if offset + 2 >= data.len() {
            break;
        }
        let Some(value) = read_zvi_variant(data, &mut offset) else {
            break;
        };
        if offset + 12 > data.len() {
            break;
        }
        offset += 2;
        let tag_id = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        offset += 10;

        match tag_id {
            TAG_CHANNEL_INDEX => {
                if let Ok(v) = value.trim().parse::<i32>() {
                    *c_index = v;
                }
            }
            TAG_SCALE_X => {
                if info.physical_size_x.is_none() {
                    if let Ok(v) = value.trim().parse::<f64>() {
                        info.physical_size_x = Some(v);
                    }
                }
            }
            TAG_SCALE_Y => {
                if info.physical_size_y.is_none() {
                    if let Ok(v) = value.trim().parse::<f64>() {
                        info.physical_size_y = Some(v);
                    }
                }
            }
            TAG_SCALE_Z => {
                if info.physical_size_z.is_none() {
                    if let Ok(v) = value.trim().parse::<f64>() {
                        info.physical_size_z = Some(v);
                    }
                }
            }
            TAG_CHANNEL_NAME => {
                if *c_index != -1 {
                    info.channel_names
                        .insert(*c_index as u32, value.trim().to_string());
                }
            }
            TAG_EMISSION => {
                if *c_index != -1 {
                    if let Ok(v) = value.trim().parse::<f64>() {
                        if v > 0.0 {
                            info.emission.insert(*c_index as u32, v);
                        }
                    }
                }
            }
            TAG_EXCITATION => {
                if *c_index != -1 {
                    if let Ok(v) = value.trim().parse::<f64>() {
                        if v > 0.0 {
                            info.excitation.insert(*c_index as u32, v);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

fn parse_zvi_tag_stream(data: &[u8], image_num: usize) -> HashMap<String, MetadataValue> {
    let mut map = HashMap::new();
    if data.len() < 12 {
        return map;
    }
    let count = u32::from_le_bytes([data[8], data[9], data[10], data[11]]) as usize;
    let mut offset = 12;
    for i in 0..count {
        let Some(value) = read_zvi_variant(data, &mut offset) else {
            break;
        };
        if offset + 12 > data.len() {
            break;
        }
        offset += 2;
        let tag_id = u32::from_le_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]);
        offset += 10;
        map.insert(
            format!("zvi.image.{image_num}.tag.{tag_id}"),
            MetadataValue::String(value.clone()),
        );
        let name = zvi_tag_name(tag_id);
        if name != "Unknown" {
            map.insert(
                format!("zvi.image.{image_num}.{name}"),
                MetadataValue::String(value),
            );
        }
        map.insert(
            format!("zvi.image.{image_num}.tag.{i}.id"),
            MetadataValue::Int(tag_id as i64),
        );
    }
    map
}

impl ZeissZviReader {
    pub fn new() -> Self {
        ZeissZviReader {
            path: None,
            comp: None,
            meta: None,
            planes: Vec::new(),
            bytes_per_pixel: 1,
            is_rgb: false,
            tile_count: 1,
            current_series: 0,
            ome_info: ZviOmeInfo::default(),
        }
    }
}

impl Default for ZeissZviReader {
    fn default() -> Self {
        Self::new()
    }
}

/// A simple little-endian byte cursor over an in-memory stream, mirroring the
/// subset of RandomAccessInputStream behaviour used by ZeissZVIReader.
struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Cursor { data, pos: 0 }
    }

    fn len(&self) -> usize {
        self.data.len()
    }

    fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n);
    }

    fn read_i16(&mut self) -> Option<i16> {
        let b = self.data.get(self.pos..self.pos + 2)?;
        self.pos += 2;
        Some(i16::from_le_bytes([b[0], b[1]]))
    }

    fn read_i32(&mut self) -> Option<i32> {
        let b = self.data.get(self.pos..self.pos + 4)?;
        self.pos += 4;
        Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }

    fn read_string(&mut self, len: usize) {
        // We only need to advance past the string for layout purposes.
        self.pos = self.pos.saturating_add(len);
    }
}

/// Port of ZeissZVIReader.getNextTag — advances the cursor past one VARIANT-typed
/// tag value. We only need the side effect on the cursor position, not the value.
fn skip_next_tag(s: &mut Cursor) {
    let ty = match s.read_i16() {
        Some(t) => t,
        None => return,
    };
    match ty {
        0 | 1 => {} // VT_EMPTY / VT_NULL
        2 | 11 => {
            s.skip(2);
        } // VT_I2 / VT_BOOL (readShort)
        3 | 22 | 19 | 23 | 4 => {
            s.skip(4);
        } // VT_I4/INT/UI4/UINT/R4
        5 | 7 | 20 | 21 => {
            s.skip(8);
        } // VT_R8/DATE/I8/UI8
        8 | 69 => {
            // VT_BSTR / VT_STORED_OBJECT: int length then string
            let len = s.read_i32().unwrap_or(0).max(0) as usize;
            s.read_string(len);
        }
        9 | 13 => {
            s.skip(16);
        } // VT_DISPATCH / VT_UNKNOWN
        63 | 65 => {
            // VT_BLOB: int length then skip
            let len = s.read_i32().unwrap_or(0).max(0) as usize;
            s.skip(len);
        }
        66 => {
            // VT_STREAM: short length then string
            let len = s.read_i16().unwrap_or(0).max(0) as usize;
            s.read_string(len);
        }
        _ => {
            // Unknown: scan forward until a short value of 3 (VT_I4) is found.
            let old = s.pos;
            while s.len() >= s.pos + 2 {
                if s.read_i16() == Some(3) {
                    break;
                }
            }
            let fp = s.pos.saturating_sub(2);
            s.pos = old.saturating_sub(2);
            s.read_string(fp.saturating_sub(old).saturating_add(2));
        }
    }
}

/// Result of parsing a single ZVI item (image) stream.
struct ParsedItem {
    z: u32,
    c: u32,
    t: u32,
    tile: u32,
    size_x: u32,
    size_y: u32,
    bpp: u32,
    data_offset: usize,
    is_zlib: bool,
    is_jpeg: bool,
}

/// Parse one ZVI item ("/Image/Item(N)/CONTENTS") stream.
///
/// Port of the per-image parsing in ZeissZVIReader.fillMetadataPass1.
fn parse_zvi_item(data: &[u8], stream_len: usize) -> Result<Option<ParsedItem>> {
    // Image streams smaller than this are metadata-only and skipped by Java.
    if stream_len <= 1024 {
        return Ok(None);
    }

    let mut s = Cursor::new(data);

    // 11 leading tags.
    for _ in 0..11 {
        skip_next_tag(&mut s);
    }

    s.skip(2);
    let Some(len_raw) = s.read_i32() else {
        return Ok(None);
    };
    let len = len_raw - 20;
    s.skip(8);

    let Some(zidx) = s.read_i32() else {
        return Ok(None);
    };
    let Some(cidx) = s.read_i32() else {
        return Ok(None);
    };
    let Some(tidx) = s.read_i32() else {
        return Ok(None);
    };
    s.skip(4);
    let Some(tile_index) = s.read_i32() else {
        return Ok(None);
    };

    // skipBytes(len - 8)
    let skip_len = (len - 8).max(0) as usize;
    s.skip(skip_len);

    // 5 more tags.
    for _ in 0..5 {
        skip_next_tag(&mut s);
    }

    s.skip(4);
    let Some(size_x) = s.read_i32() else {
        return Ok(None);
    };
    let Some(size_y) = s.read_i32() else {
        return Ok(None);
    };
    s.skip(4);
    let Some(bpp) = s.read_i32() else {
        return Ok(None);
    };
    if size_x <= 0 || size_y <= 0 {
        return Err(BioFormatsError::Format(format!(
            "ZVI: invalid non-positive image dimensions {size_x}x{size_y}"
        )));
    }
    if !matches!(bpp, 1 | 2 | 3 | 6) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "ZVI: unsupported bytes-per-pixel value {bpp}"
        )));
    }
    // Java skips exactly one 4-byte field here (ZeissZVIReader.java:311) before
    // reading `valid`. Our pixel-data offset = filePointer - 4 depends on this
    // being a single skip; a second skip would push the offset 4 bytes too far.
    s.skip(4);

    let Some(valid) = s.read_i32() else {
        return Ok(None);
    };
    let check_bytes = data.get(s.pos..s.pos + 4).unwrap_or(&[]);
    let check = String::from_utf8_lossy(check_bytes).trim().to_string();
    s.skip(4);

    let is_zlib = (valid == 0 || valid == 1) && check == "WZL";
    let is_jpeg = (valid == 0 || valid == 1) && !is_zlib;

    // Pixel data offset = filePointer - 4 (+8 for zlib).
    let mut data_offset = s.pos.saturating_sub(4);
    if is_zlib {
        data_offset += 8;
    }

    if !is_zlib && !is_jpeg {
        // Validate the offset is in range, but tolerate a plane that is a few
        // bytes short of the declared size: ZeissZVIReader.openBytes reads into
        // a pre-zeroed buffer via readPlane, so a stream that ends a few bytes
        // early (seen in real Zeiss exports) simply leaves the tail zero rather
        // than failing. open_bytes mirrors this by zero-padding.
        let plane_bytes = (size_x as usize)
            .checked_mul(size_y as usize)
            .and_then(|px| px.checked_mul(bpp as usize))
            .ok_or_else(|| BioFormatsError::Format("ZVI plane size overflows".into()))?;
        if data_offset > stream_len {
            return Err(BioFormatsError::InvalidData(
                "ZVI: pixel data offset is past end of stream".into(),
            ));
        }
        let _ = plane_bytes;
    }

    Ok(Some(ParsedItem {
        z: zidx.max(0) as u32,
        c: cidx.max(0) as u32,
        t: tidx.max(0) as u32,
        tile: tile_index.max(0) as u32,
        size_x: size_x.max(0) as u32,
        size_y: size_y.max(0) as u32,
        bpp: bpp.max(0) as u32,
        data_offset,
        is_zlib,
        is_jpeg,
    }))
}

fn parse_zvi_item_stream<R: Read + Seek>(stream: &mut R) -> Result<Option<ParsedItem>> {
    let stream_len = stream.seek(SeekFrom::End(0)).map_err(BioFormatsError::Io)? as usize;
    stream
        .seek(SeekFrom::Start(0))
        .map_err(BioFormatsError::Io)?;

    let initial_len = stream_len.min(64 * 1024);
    let mut data = vec![0u8; initial_len];
    stream.read_exact(&mut data).map_err(BioFormatsError::Io)?;
    match parse_zvi_item(&data, stream_len) {
        Ok(Some(item)) => Ok(Some(item)),
        Ok(None) if initial_len < stream_len => {
            stream
                .seek(SeekFrom::Start(0))
                .map_err(BioFormatsError::Io)?;
            data.resize(stream_len, 0);
            stream.read_exact(&mut data).map_err(BioFormatsError::Io)?;
            parse_zvi_item(&data, stream_len)
        }
        other => other,
    }
}

fn parse_zvi(
    path: &Path,
) -> Result<(
    ImageMetadata,
    Vec<ZviPlane>,
    usize,
    bool,
    usize,
    ZviOmeInfo,
    cfb::CompoundFile<File>,
)> {
    let mut comp =
        cfb::open(path).map_err(|e| BioFormatsError::Format(format!("ZVI CFB open error: {e}")))?;

    // ── Enumerate image item streams ─────────────────────────────────────────
    // ZeissZVIReader matches stream names case-insensitively: it uppercases the
    // path and keeps those ending in "CONTENTS" that live under an "ITEM(n)"
    // directory (ZeissZVIReader.java:393-404). The cfb container preserves the
    // original on-disk casing (e.g. ".../Item(0)/Contents"), so we must match
    // without regard to case.
    let item_num = |s: &str| -> u32 {
        // Extract the index from the parent "Item(n)" directory, mirroring
        // getImageNumber (case-insensitive "ITEM").
        let dir = parent_dir(s);
        let upper = dir.to_ascii_uppercase();
        if upper.contains("ITEM") {
            if let Some(open) = dir.find('(') {
                let after = &dir[open + 1..];
                return after
                    .split(')')
                    .next()
                    .and_then(|n| n.trim().parse().ok())
                    .unwrap_or(0);
            }
        }
        0
    };
    let is_item_contents = |p: &str| -> bool {
        // relPath must be exactly CONTENTS (case-insensitive) and the immediate
        // parent dir must be "Image" or contain "ITEM" (ZeissZVIReader:240-266).
        let upper = p.to_ascii_uppercase();
        if !upper.ends_with("/CONTENTS") {
            return false;
        }
        let dir = parent_dir(p).to_ascii_uppercase();
        dir == "IMAGE" || dir.contains("ITEM")
    };
    let mut item_paths: Vec<String> = comp
        .walk()
        .filter_map(|entry| {
            if !entry.is_stream() {
                return None;
            }
            let p = entry.path().to_string_lossy().to_string();
            if is_item_contents(&p) {
                Some(p)
            } else {
                None
            }
        })
        .collect();

    // Numeric sort by item index.
    item_paths.sort_by_key(|p| item_num(p));

    let mut planes: Vec<ZviPlane> = Vec::with_capacity(item_paths.len());
    let mut series_metadata = HashMap::new();
    let mut ome_info = ZviOmeInfo::default();
    let mut c_index: i32 = -1;
    let mut bpp: u32 = 0;
    let mut size_x: u32 = 0;
    let mut size_y: u32 = 0;
    let mut is_jpeg_global = false;

    for stream_path in item_paths {
        let mut stream = match comp.open_stream(&stream_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let item = match parse_zvi_item_stream(&mut stream) {
            Ok(Some(item)) => item,
            Ok(None) => continue,
            Err(_) => {
                let mut stream = match comp.open_stream(&stream_path) {
                    Ok(s) => s,
                    Err(_) => continue,
                };
                let mut data = Vec::new();
                if stream.read_to_end(&mut data).is_err() {
                    continue;
                }
                match parse_zvi_item(&data, data.len())? {
                    Some(item) => item,
                    None => continue,
                }
            }
        };
        if item.data_offset > 64 * 1024 {
            // Extremely large item headers are rare; reopen and parse the whole
            // stream so the result is still derived from the same bytes as the
            // original full-read implementation.
            let mut stream = match comp.open_stream(&stream_path) {
                Ok(s) => s,
                Err(_) => continue,
            };
            let mut data = Vec::new();
            if stream.read_to_end(&mut data).is_err() {
                continue;
            }
            let Some(item) = parse_zvi_item(&data, data.len())? else {
                continue;
            };
            planes.push(ZviPlane {
                stream_path,
                z: item.z,
                c: item.c,
                t: item.t,
                tile: item.tile,
                data_offset: item.data_offset,
                is_zlib: item.is_zlib,
                is_jpeg: item.is_jpeg,
            });
            continue;
        }

        // bpp / sizeX / sizeY are taken from the first valid image stream.
        if bpp == 0 {
            bpp = item.bpp;
        }
        if size_x == 0 {
            size_x = item.size_x;
        }
        if size_y == 0 {
            size_y = item.size_y;
        }
        if item.is_jpeg {
            is_jpeg_global = true;
        }

        // Keep every image stream, including tiles. ZeissZVIReader records the
        // tile index in coordinates[i][3] and exposes each tile as a series
        // rather than stitching them into a single plane.
        planes.push(ZviPlane {
            stream_path,
            z: item.z,
            c: item.c,
            t: item.t,
            tile: item.tile,
            data_offset: item.data_offset,
            is_zlib: item.is_zlib,
            is_jpeg: item.is_jpeg,
        });
    }

    for plane in &planes {
        let image_num = item_num(&plane.stream_path) as usize;
        // Derive the sibling Tags stream from the item's own "…/Contents" path,
        // preserving on-disk casing (e.g. ".../Item(0)/Tags/Contents").
        let sp = &plane.stream_path;
        let tag_path = match sp.rfind('/') {
            Some(slash) => {
                let (dir, contents) = sp.split_at(slash);
                format!("{dir}/Tags{contents}")
            }
            None => continue,
        };
        if let Ok(mut stream) = comp.open_stream(&tag_path) {
            let mut data = Vec::new();
            if stream.read_to_end(&mut data).is_ok() {
                series_metadata.extend(parse_zvi_tag_stream(&data, image_num));
                harvest_zvi_ome_tags(&data, &mut ome_info, &mut c_index);
            }
        }
    }

    // Physical pixel sizes live in the top-level "/Image/Tags/Contents" stream
    // (dirName "Tags"), not the per-item Tags. BaseZeissReader parses that stream
    // too; harvest it for the Scale Factor tags. Use a throwaway channel index so
    // any "Image Channel Index" tag here cannot disturb the per-channel mapping.
    if let Ok(mut stream) = comp.open_stream("/Image/Tags/Contents") {
        let mut data = Vec::new();
        if stream.read_to_end(&mut data).is_ok() {
            let mut scratch_index: i32 = -1;
            harvest_zvi_ome_tags(&data, &mut ome_info, &mut scratch_index);
        }
    }

    if planes.is_empty() {
        return Err(BioFormatsError::Format("ZVI: no image planes found".into()));
    }

    // ── Pixel type from bpp (BaseZeissReader.fillMetadataPass6) ───────────────
    //   bpp 1|3 -> UINT8, bpp 2|6 -> UINT16; isJPEG forces UINT8.
    //   RGB when bpp % 3 == 0.
    let is_rgb = bpp != 0 && bpp % 3 == 0;
    let pixel_type = if is_jpeg_global {
        PixelType::Uint8
    } else if bpp == 1 || bpp == 3 {
        PixelType::Uint8
    } else if bpp == 2 || bpp == 6 {
        PixelType::Uint16
    } else {
        PixelType::Uint8
    };
    let bytes_per_sample = pixel_type.bytes_per_sample();
    // Stored bytes per pixel including RGB channels (matches Java `bpp`).
    let bytes_per_pixel = if is_rgb {
        bytes_per_sample * 3
    } else {
        bytes_per_sample
    };

    // ── Derive dimension sizes from distinct indices ──────────────────────────
    // BaseZeissReader.fillMetadataPass2: sizeZ/sizeT/sizeC = the number of
    // distinct z/t/channel index values (collected across all tiles, since the
    // per-tile coordinate sets are identical).
    let distinct = |sel: &dyn Fn(&ZviPlane) -> u32| -> u32 {
        let mut v: Vec<u32> = planes.iter().map(sel).collect();
        v.sort_unstable();
        v.dedup();
        v.len() as u32
    };
    let size_z = distinct(&|p| p.z);
    let logical_c = distinct(&|p| p.c);
    let size_t = distinct(&|p| p.t);
    let mut size_c = logical_c;
    if is_rgb {
        size_c *= 3;
    }

    // Number of tiles = total planes / per-tile plane count, with each tile a
    // separate series (ZeissZVIReader: totalTiles = offsets.length/imageCount).
    let image_count = size_z * logical_c * size_t;
    let tile_count = if image_count > 0 {
        (planes.len() as u32 / image_count).max(1) as usize
    } else {
        1
    };

    // ── Dimension order (BaseZeissReader.fillMetadataPass4:236-255) ───────────
    // Java builds the order from the per-plane coordinate deltas, walked in the
    // original (item-number) stream order: start with "XY", prepend 'C' for RGB,
    // then append Z/C/T the first time consecutive planes increase along that
    // axis, and finally run makeSaneDimensionOrder to fill any missing axes.
    // `planes` is currently in item-number order, matching `coordinates`.
    let mut order = String::from("XY");
    if is_rgb {
        order.push('C');
    }
    for w in planes.windows(2) {
        let (a, b) = (&w[0], &w[1]);
        if b.z > a.z && !order.contains('Z') {
            order.push('Z');
        }
        if b.c > a.c && !order.contains('C') {
            order.push('C');
        }
        if b.t > a.t && !order.contains('T') {
            order.push('T');
        }
    }
    let dimension_order = make_sane_dimension_order(&order);

    // Sort planes so each tile's planes form a contiguous block ordered by the
    // derived dimension order (fastest-varying axis last in the sort key), so
    // `plane_index` maps to the same plane Java resolves via getZCTCoords.
    let axis_key = |p: &ZviPlane, axis: char| -> u32 {
        match axis {
            'Z' => p.z,
            'C' => p.c,
            'T' => p.t,
            _ => 0,
        }
    };
    // Build the (outer..inner) axis list from the order string (skip X, Y).
    let axes: Vec<char> = dimension_order_axes(dimension_order);
    planes.sort_by(|a, b| {
        let mut ord = a.tile.cmp(&b.tile);
        // outer-most axis first; the last axis in `axes` varies fastest, so
        // compare from outermost (axes.last) to innermost (axes.first) by
        // iterating the reversed slice as major→minor.
        for &axis in axes.iter().rev() {
            ord = ord.then_with(|| axis_key(a, axis).cmp(&axis_key(b, axis)));
        }
        ord
    });

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (bytes_per_sample * 8) as u8,
        image_count,
        dimension_order,
        is_rgb,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        thumbnail: false,
        series_metadata,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok((
        meta,
        planes,
        bytes_per_pixel,
        is_rgb,
        tile_count,
        ome_info,
        comp,
    ))
}

/// Decode pixel data from a ZVI plane stream starting at `data_offset`.
///
/// Port of ZeissZVIReader.openBytes pixel-decode dispatch: the pixel data offset
/// is the precomputed `offsets[index]` (already advanced past the zlib WZL
/// sub-header when `is_zlib`), and the compression flags select the codec.
fn decode_plane_data(data: &[u8], plane: &ZviPlane) -> Result<Vec<u8>> {
    let payload = data.get(plane.data_offset..).ok_or_else(|| {
        BioFormatsError::Format("ZVI: pixel data offset is past end of stream".into())
    })?;

    if plane.is_jpeg {
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(payload));
        let pixels = decoder
            .decode()
            .map_err(|e| BioFormatsError::Format(format!("ZVI JPEG decode: {e}")))?;
        return Ok(pixels);
    }

    if plane.is_zlib {
        let mut decoder = flate2::read::ZlibDecoder::new(payload);
        let mut out = Vec::new();
        decoder
            .read_to_end(&mut out)
            .map_err(|e| BioFormatsError::Format(format!("ZVI zlib decode: {e}")))?;
        return Ok(out);
    }

    // Raw uncompressed.
    Ok(payload.to_vec())
}

impl FormatReader for ZeissZviReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("zvi"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // OLE2 CFB magic — shared with other OLE2 files, so also require the context
        // that the caller will have already checked the extension separately.
        // For the magic-byte pass we require both magic + a deferred extension check
        // is not possible here (no path), so we return false to force extension path.
        // Actually we CAN check: bytes 0-3 must match AND the call site checks extension
        // too via is_this_type_by_name. But the registry tries magic first; to avoid
        // false-matching .doc/.xls/.oib etc. we intentionally return false here
        // and let the extension fallback handle ZVI.
        //
        // Returning false from magic means the registry will try is_this_type_by_name
        // next, which checks the .zvi extension.
        let _ = header;
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let (meta, planes, bpp, is_rgb, tile_count, ome_info, comp) = parse_zvi(path)?;
        self.meta = Some(meta);
        self.planes = planes;
        self.comp = Some(comp);
        self.path = Some(path.to_path_buf());
        self.bytes_per_pixel = bpp;
        self.is_rgb = is_rgb;
        self.tile_count = tile_count.max(1);
        self.current_series = 0;
        self.ome_info = ome_info;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.comp = None;
        self.meta = None;
        self.planes.clear();
        self.tile_count = 1;
        self.current_series = 0;
        self.ome_info = ZviOmeInfo::default();
        Ok(())
    }

    fn series_count(&self) -> usize {
        if self.meta.is_some() {
            self.tile_count.max(1)
        } else {
            0
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_count() {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.current_series = s;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "ZVI: resolution {level} out of range"
            )))
        } else {
            Ok(())
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        // Planes are stored contiguously per tile (series), so the active
        // series offsets into the global plane list. This mirrors how
        // ZeissZVIReader resolves the plane by matching coordinates[i][3]
        // (the tile index) against getSeries().
        let image_count = meta.image_count;
        let global_index = (self.current_series as u32)
            .checked_mul(image_count)
            .and_then(|base| base.checked_add(plane_index))
            .ok_or_else(|| BioFormatsError::PlaneOutOfRange(plane_index))?;

        let plane = self
            .planes
            .get(global_index as usize)
            .ok_or_else(|| BioFormatsError::PlaneOutOfRange(plane_index))?;
        let stream_path = plane.stream_path.clone();
        let plane = ZviPlane {
            stream_path: stream_path.clone(),
            z: plane.z,
            c: plane.c,
            t: plane.t,
            tile: plane.tile,
            data_offset: plane.data_offset,
            is_zlib: plane.is_zlib,
            is_jpeg: plane.is_jpeg,
        };

        let comp = self.comp.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let mut stream = comp
            .open_stream(&stream_path)
            .map_err(|e| BioFormatsError::Format(format!("ZVI stream {stream_path}: {e}")))?;
        let mut data = Vec::new();
        stream
            .read_to_end(&mut data)
            .map_err(|e| BioFormatsError::Io(e))?;

        let mut pixels = decode_plane_data(&data, &plane)?;

        // Normalise to exactly one plane's worth of bytes. Java reads
        // sizeX * sizeY * pixel bytes into a pre-zeroed buffer via readPlane, so
        // a stream that ends a few bytes early leaves the tail zero rather than
        // failing; mirror that by zero-padding a short decode.
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * self.bytes_per_pixel;
        if pixels.len() > plane_bytes {
            pixels.truncate(plane_bytes);
        } else if pixels.len() < plane_bytes {
            pixels.resize(plane_bytes, 0);
        }

        // BGR storage: reverse channel bytes in groups for RGB images (but not
        // for JPEG, which the codec already returns in RGB order). Matches
        // ZeissZVIReader.openBytes: swap the first sample with the third per
        // pixel, where each sample is `bytes` wide and the pixel stride is bpp.
        if self.is_rgb && !plane.is_jpeg && self.bytes_per_pixel >= 3 {
            let bpp = self.bytes_per_pixel;
            let bytes = bpp / 3;
            let mut i = 0;
            while i + bpp <= pixels.len() {
                for k in 0..bytes {
                    pixels.swap(i + k, i + 2 * bytes + k);
                }
                i += bpp;
            }
        }

        Ok(pixels)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let samples_per_pixel = self
            .bytes_per_pixel
            .checked_div(bps)
            .filter(|samples| {
                *samples > 0 && samples.checked_mul(bps) == Some(self.bytes_per_pixel)
            })
            .ok_or_else(|| BioFormatsError::Format("ZVI pixel size is inconsistent".into()))?;
        let x2 = x
            .checked_add(w)
            .ok_or_else(|| BioFormatsError::Format("ZVI region width overflows".into()))?;
        let y2 = y
            .checked_add(h)
            .ok_or_else(|| BioFormatsError::Format("ZVI region height overflows".into()))?;
        if w == 0 || h == 0 || x2 > meta.size_x || y2 > meta.size_y {
            return Err(BioFormatsError::Format(
                "ZVI region is outside image bounds".into(),
            ));
        }

        let image_count = meta.image_count;
        let global_index = (self.current_series as u32)
            .checked_mul(image_count)
            .and_then(|base| base.checked_add(plane_index))
            .ok_or_else(|| BioFormatsError::PlaneOutOfRange(plane_index))?;
        let plane = self
            .planes
            .get(global_index as usize)
            .ok_or_else(|| BioFormatsError::PlaneOutOfRange(plane_index))?;

        if !plane.is_jpeg && !plane.is_zlib {
            let comp = self.comp.as_mut().ok_or(BioFormatsError::NotInitialized)?;
            let mut stream = comp.open_stream(&plane.stream_path).map_err(|e| {
                BioFormatsError::Format(format!("ZVI stream {}: {e}", plane.stream_path))
            })?;

            let src_row_bytes = meta.size_x as usize * self.bytes_per_pixel;
            let dst_row_bytes = w as usize * self.bytes_per_pixel;
            let plane_bytes = src_row_bytes
                .checked_mul(meta.size_y as usize)
                .ok_or_else(|| BioFormatsError::Format("ZVI plane byte count overflows".into()))?;
            let mut out = vec![0u8; dst_row_bytes * h as usize];

            for row in 0..h as usize {
                let src = plane
                    .data_offset
                    .checked_add((y as usize + row) * src_row_bytes)
                    .and_then(|off| off.checked_add(x as usize * self.bytes_per_pixel))
                    .ok_or_else(|| BioFormatsError::Format("ZVI region offset overflows".into()))?;
                if src < plane.data_offset + plane_bytes {
                    let remaining = plane.data_offset + plane_bytes - src;
                    let to_read = dst_row_bytes.min(remaining);
                    stream
                        .seek(SeekFrom::Start(src as u64))
                        .map_err(BioFormatsError::Io)?;
                    let dst = row * dst_row_bytes;
                    stream
                        .read_exact(&mut out[dst..dst + to_read])
                        .map_err(BioFormatsError::Io)?;
                }
            }

            if self.is_rgb && self.bytes_per_pixel >= 3 {
                let bpp = self.bytes_per_pixel;
                let bytes = bpp / 3;
                let mut i = 0;
                while i + bpp <= out.len() {
                    for k in 0..bytes {
                        out.swap(i + k, i + 2 * bytes + k);
                    }
                    i += bpp;
                }
            }
            return Ok(out);
        }

        let full = self.open_bytes(plane_index)?;
        crop_full_plane("ZVI", &full, &meta, samples_per_pixel, x, y, w, h)
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
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let info = &self.ome_info;
        let img = ome.images.get_mut(0)?;

        // Image name: BaseZeissReader only sets an explicit name ("Tile #N") for
        // multi-series files; for a single series Java falls back to the file's
        // base name (with extension), e.g. "fig3d_wt_sting_cd31.zvi".
        if self.tile_count > 1 {
            img.name = Some(format!("Tile #{}", self.current_series + 1));
        } else if let Some(path) = &self.path {
            img.name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(str::to_string);
        }

        img.physical_size_x = info.physical_size_x;
        img.physical_size_y = info.physical_size_y;
        img.physical_size_z = info.physical_size_z;

        // Per-channel name / emission / excitation. The raw channel-index values
        // ("Image Channel Index" tags) need not be 0..N-1 (this file uses 0,2,3),
        // so — like BaseZeissReader — OME channel i takes the i-th value when the
        // recorded channel-name keys are sorted ascending (channelKeys[i]).
        let mut channel_keys: Vec<u32> = info.channel_names.keys().copied().collect();
        channel_keys.sort_unstable();
        for (ci, ch) in img.channels.iter_mut().enumerate() {
            let Some(&key) = channel_keys.get(ci) else {
                break;
            };
            if let Some(name) = info.channel_names.get(&key) {
                ch.name = Some(name.clone());
            }
            ch.emission_wavelength = info.emission.get(&key).copied();
            ch.excitation_wavelength = info.excitation.get(&key).copied();
        }

        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_zvi_{nanos}_{name}.zvi"))
    }

    /// Build one ZVI item ("/Image/Item(N)/CONTENTS") stream carrying the given
    /// z/c/t/tile indices and a single uncompressed 1x1 UINT8 pixel value. The
    /// byte layout matches `parse_zvi_item` (and the Java reference).
    fn build_item(z: i32, c: i32, t: i32, tile: i32, pixel: u8) -> Vec<u8> {
        let mut item: Vec<u8> = Vec::new();
        // 11 leading VT_EMPTY tags (type 0, 2 bytes each).
        item.extend_from_slice(&[0u8; 22]);
        // skip(2)
        item.extend_from_slice(&[0u8; 2]);
        // len = readInt() - 20; pad skip(len-8) past the 1024-byte cutoff.
        let pad: i32 = 1100;
        let len_raw: i32 = pad + 28;
        item.extend_from_slice(&len_raw.to_le_bytes());
        // skip(8)
        item.extend_from_slice(&[0u8; 8]);
        item.extend_from_slice(&z.to_le_bytes());
        item.extend_from_slice(&c.to_le_bytes());
        item.extend_from_slice(&t.to_le_bytes());
        item.extend_from_slice(&[0u8; 4]); // skip(4)
        item.extend_from_slice(&tile.to_le_bytes());
        item.extend_from_slice(&vec![0u8; pad as usize]); // skip(len - 8)
                                                          // 5 more VT_EMPTY tags.
        item.extend_from_slice(&[0u8; 10]);
        // skip(4)
        item.extend_from_slice(&[0u8; 4]);
        item.extend_from_slice(&1i32.to_le_bytes()); // sizeX
        item.extend_from_slice(&1i32.to_le_bytes()); // sizeY
        item.extend_from_slice(&[0u8; 4]); // skip(4)
        item.extend_from_slice(&1i32.to_le_bytes()); // bpp -> UINT8
        item.extend_from_slice(&[0u8; 4]); // skip(4) before valid
        item.extend_from_slice(&2i32.to_le_bytes()); // valid=2 -> uncompressed
        item.extend_from_slice(&[pixel, 0, 0, 0]); // check / first-pixel region
        item
    }

    #[test]
    fn zvi_exposes_each_tile_as_a_separate_series() {
        // Two tiles, each a single (z=c=t=0) 1x1 plane. ZeissZVIReader records
        // the tile index per plane and treats each tile as its own series
        // (totalTiles = offsets.length / getImageCount()).
        let path = temp_path("two_tiles");
        {
            let mut comp = cfb::create(&path).unwrap();
            comp.create_storage_all("/Image/Item(1)").unwrap();
            comp.create_storage_all("/Image/Item(2)").unwrap();
            comp.create_stream("/Image/Item(1)/CONTENTS")
                .unwrap()
                .write_all(&build_item(0, 0, 0, 0, 11))
                .unwrap();
            comp.create_stream("/Image/Item(2)/CONTENTS")
                .unwrap()
                .write_all(&build_item(0, 0, 0, 1, 22))
                .unwrap();
        }

        let mut reader = ZeissZviReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 2);
        let meta = reader.metadata();
        assert_eq!(meta.image_count, 1);
        assert_eq!((meta.size_x, meta.size_y), (1, 1));

        // Series 0 -> tile 0.
        assert_eq!(reader.series(), 0);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![11]);

        // Series 1 -> tile 1.
        reader.set_series(1).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![22]);
        assert!(reader.open_bytes_region(0, 1, 0, 1, 1).is_err());

        assert!(reader.set_series(2).is_err());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zvi_single_tile_is_one_series() {
        let path = temp_path("one_tile");
        {
            let mut comp = cfb::create(&path).unwrap();
            comp.create_storage_all("/Image/Item(1)").unwrap();
            comp.create_stream("/Image/Item(1)/CONTENTS")
                .unwrap()
                .write_all(&build_item(0, 0, 0, 0, 99))
                .unwrap();
        }

        let mut reader = ZeissZviReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![99]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zvi_zero_pads_a_short_raw_plane() {
        // ZeissZVIReader.openBytes reads into a pre-zeroed buffer, so a stream
        // that ends a few bytes short of a full plane leaves the tail zero
        // rather than failing. Here the single uncompressed pixel byte (99) is
        // truncated away, so the decoded plane must be a single zero byte.
        let path = temp_path("short_plane");
        let mut item = build_item(0, 0, 0, 0, 99);
        item.truncate(item.len() - 4);
        {
            let mut comp = cfb::create(&path).unwrap();
            comp.create_storage_all("/Image/Item(1)").unwrap();
            comp.create_stream("/Image/Item(1)/CONTENTS")
                .unwrap()
                .write_all(&item)
                .unwrap();
        }

        let mut reader = ZeissZviReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![0]);

        let _ = std::fs::remove_file(path);
    }
}
