//! Zeiss CZI (ZISRAWFILE) format reader.
//!
//! Segments use a 32-byte header:
//!   bytes  0-15: segment type (ASCII, zero-padded) e.g. "ZISRAWFILE"
//!   bytes 16-23: allocated size (int64 LE)
//!   bytes 24-31: used size (int64 LE)
//!
//! Supported compressions: Uncompressed, JPEG (new-style), LZW, Zstd.
//! JPEG-XR is detected but not decoded (needs a JXRC decoder).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ---- pixel types (from DirectoryEntry) -------------------------------------

fn czi_pixel_type(code: i32) -> (PixelType, u32) {
    // Returns (pixel_type, samples_per_pixel)
    match code {
        0 => (PixelType::Uint8, 1),    // Gray8
        1 => (PixelType::Uint16, 1),   // Gray16
        2 => (PixelType::Float32, 1),  // GrayFloat
        3 => (PixelType::Uint8, 3),    // Bgr24
        4 => (PixelType::Uint16, 3),   // Bgr48
        8 => (PixelType::Float32, 3),  // BgrFloat
        9 => (PixelType::Uint8, 4),    // Bgra32
        10 => (PixelType::Float32, 2), // Complex (re+im)
        11 => (PixelType::Float32, 2), // ComplexFloat
        12 => (PixelType::Uint32, 1),  // Gray32
        13 => (PixelType::Float64, 1), // GrayDouble
        _ => (PixelType::Uint8, 1),
    }
}

// ---- segment header --------------------------------------------------------

const SEG_HEADER: usize = 32;

fn read_seg_type(data: &[u8]) -> String {
    let end = data[..16].iter().position(|&b| b == 0).unwrap_or(16);
    String::from_utf8_lossy(&data[..end]).into_owned()
}

fn read_i32(data: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}
fn read_i64(data: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(data[off..off + 8].try_into().unwrap_or([0; 8]))
}
fn read_u64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap_or([0; 8]))
}

fn read_seg_sizes(data: &[u8]) -> (u64, u64) {
    let allocated = read_u64(data, 16);
    let mut used = read_u64(data, 24);
    if used == 0 {
        used = allocated;
    }
    (allocated, used)
}

fn valid_segment_position(pos: u64, file_len: u64) -> bool {
    pos > 0 && pos.saturating_add(SEG_HEADER as u64) <= file_len
}

// ---- DirectoryEntry (256 bytes) -------------------------------------------

#[derive(Debug, Clone)]
struct DirEntry {
    pixel_type: i32,
    file_position: i64,
    compression: i32,
    // Dimensions from DimensionEntry array
    dims: HashMap<String, (i32, i32)>, // dim_name -> (start, size)
    // storedSize per dimension (physical/decoded extent of the tile, which may
    // differ from `size` for downsampled or compressed subblocks).
    stored: HashMap<String, i32>, // dim_name -> storedSize
}

impl DirEntry {
    fn dim_start(&self, name: &str) -> i32 {
        self.dims.get(name).map(|&(start, _)| start).unwrap_or(0)
    }

    fn dim_size(&self, name: &str) -> i32 {
        self.dims.get(name).map(|&(_, size)| size).unwrap_or(1)
    }

    /// Stored (physical) size of a dimension, falling back to the logical size.
    fn dim_stored_size(&self, name: &str) -> i32 {
        match self.stored.get(name) {
            Some(&s) if s > 0 => s,
            _ => self.dim_size(name),
        }
    }

    fn matches_plane(&self, z: u32, c: u32, t: u32) -> bool {
        self.dims
            .get("Z")
            .map(|&(s, _)| s as u32 == z)
            .unwrap_or(z == 0)
            && self
                .dims
                .get("C")
                .map(|&(s, _)| s as u32 == c)
                .unwrap_or(c == 0)
            && self
                .dims
                .get("T")
                .map(|&(s, _)| s as u32 == t)
                .unwrap_or(t == 0)
    }
}

#[derive(Debug, Clone)]
struct CziResolution {
    r: i32,
    width: u32,
    height: u32,
}

/// One series corresponds to one scene/position (the CZI "S" dimension), as in
/// ZeissCZIReader where `seriesCount = positions` (mosaics/acquisitions/angles
/// aside). Each series carries its own pyramid resolution list.
#[derive(Debug, Clone)]
struct CziSeries {
    /// Absolute value of the "S" dimension start that selects this scene.
    scene: i32,
    resolutions: Vec<CziResolution>,
}

fn parse_dir_entry(data: &[u8]) -> DirEntry {
    // schema 0-1 (2 bytes)
    let pixel_type = read_i32(data, 2);
    let file_position = read_i64(data, 6);
    let compression = read_i32(data, 18);
    let dim_count = read_i32(data, 28) as usize;

    let mut dims: HashMap<String, (i32, i32)> = HashMap::new();
    let mut stored: HashMap<String, i32> = HashMap::new();
    let dim_array_start = 32;
    for i in 0..dim_count {
        let off = dim_array_start + i * 20;
        if off + 20 > data.len() {
            break;
        }
        // DimensionEntry layout (20 bytes):
        //   0  dimension (4 chars)
        //   4  start (int)
        //   8  size (int)
        //   12 startCoordinate (float)
        //   16 storedSize (int)
        let dim_name = std::str::from_utf8(&data[off..off + 4])
            .unwrap_or("")
            .trim_end_matches('\0')
            .trim()
            .to_string();
        let start = read_i32(data, off + 4);
        let size = read_i32(data, off + 8);
        let stored_size = read_i32(data, off + 16);
        if !dim_name.is_empty() {
            dims.insert(dim_name.clone(), (start, size));
            stored.insert(dim_name, stored_size);
        }
    }

    DirEntry {
        pixel_type,
        file_position,
        compression,
        dims,
        stored,
    }
}

fn parse_directory_entries(data: &[u8], entry_count: usize) -> Vec<DirEntry> {
    let mut entries = Vec::with_capacity(entry_count);

    if entry_count == 0 {
        return entries;
    }

    // Synthetic fixtures in this crate historically wrote each directory entry
    // into the 256-byte subblock slot. Real CZI directory segments store compact
    // entries: 32 bytes plus 20 bytes per DimensionEntry.
    let fixed_stride = if data.len() >= entry_count * 256 {
        Some(256)
    } else {
        None
    };

    let mut off = 0usize;
    for _ in 0..entry_count {
        if off + 32 > data.len() {
            break;
        }
        let dim_count = read_i32(data, off + 28).max(0) as usize;
        let compact_len = 32 + dim_count * 20;
        let entry_len = fixed_stride.unwrap_or(compact_len);
        if off + compact_len > data.len() {
            break;
        }

        let parse_len = entry_len.min(data.len() - off);
        entries.push(parse_dir_entry(&data[off..off + parse_len]));
        off = off.saturating_add(entry_len);
    }

    entries
}

// ---- file parsing ----------------------------------------------------------

struct CziParsed {
    meta_xml: String,
    entries: Vec<DirEntry>,
    z_count: u32,
    c_count: u32,
    t_count: u32,
    pixel_type: PixelType,
    spp: u32,
    /// One entry per scene/position (the "S" dimension). Always non-empty.
    series: Vec<CziSeries>,
}

fn parse_czi_file(f: &mut BufReader<File>) -> std::io::Result<CziParsed> {
    let file_len = f.get_ref().metadata()?.len();

    // --- Read file header segment ---
    let mut hdr = vec![0u8; SEG_HEADER];
    f.read_exact(&mut hdr)?;
    let seg_type = read_seg_type(&hdr);
    if !seg_type.starts_with("ZISRAWFILE") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Not a CZI file",
        ));
    }

    // FileHeader data starts after the 32-byte segment header.
    // Layout (matching ZeissCZIReader.FileHeader.fillInData):
    //   0  majorVersion (int)
    //   4  minorVersion (int)
    //   8  reserved1 (int)
    //   12 reserved2 (int)
    //   16 primaryFileGUID (long)
    //   24 fileGUID (long)
    //   32 filePart (int)
    //   36 directoryPosition (long)
    //   44 metadataPosition (long)
    let mut fh = vec![0u8; 80];
    f.read_exact(&mut fh)?;
    let dir_position = read_u64(&fh, 36);
    let meta_position = read_u64(&fh, 44);
    let dir_position = if valid_segment_position(dir_position, file_len) {
        dir_position
    } else {
        0
    };
    let meta_position = if valid_segment_position(meta_position, file_len) {
        meta_position
    } else {
        0
    };

    // --- Read metadata segment ---
    let mut meta_xml = String::new();
    if meta_position > 0 {
        f.seek(SeekFrom::Start(meta_position))?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        f.read_exact(&mut seg_hdr)?;
        // Metadata segment body: xml_size (i32), attach_size (i32), reserved (248), xml data
        let mut meta_body_hdr = vec![0u8; 256];
        f.read_exact(&mut meta_body_hdr)?;
        let xml_size = read_i32(&meta_body_hdr, 0) as usize;
        if xml_size > 0 {
            let mut xml_bytes = vec![0u8; xml_size];
            f.read_exact(&mut xml_bytes)?;
            meta_xml = String::from_utf8_lossy(&xml_bytes).into_owned();
        }
    }

    // --- Read directory segment ---
    let mut entries: Vec<DirEntry> = Vec::new();
    if dir_position > 0 {
        f.seek(SeekFrom::Start(dir_position))?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        f.read_exact(&mut seg_hdr)?;
        let (allocated_size, used_size) = read_seg_sizes(&seg_hdr);
        // Directory body: entry_count (i32), reserved (124), DirectoryEntry[]
        let mut dir_hdr = vec![0u8; 128];
        f.read_exact(&mut dir_hdr)?;
        let entry_count = read_i32(&dir_hdr, 0) as usize;
        let body_size = used_size.max(allocated_size).saturating_sub(128);
        let remaining = file_len.saturating_sub(f.stream_position()?);
        let body_size = body_size.min(remaining);
        if body_size > 0 {
            let mut entry_bytes = vec![0u8; body_size as usize];
            f.read_exact(&mut entry_bytes)?;
            entries = parse_directory_entries(&entry_bytes, entry_count);
        }
    }

    // Compute dimensions from entries.
    //
    // ZeissCZIReader.calculateDimensions: the "S" dimension yields the scene
    // count (`positions = maxS - minS + 1`); each scene becomes a series. The
    // "R" dimension yields pyramid resolution levels. X/Y extents are tracked
    // per (scene, R) bucket so every series/resolution gets its own size.
    let mut max_z = 0i32;
    let mut max_c = 0i32;
    let mut max_t = 0i32;
    let mut pixel_type = 0i32;

    let mut min_scene = i32::MAX;
    let mut max_scene = i32::MIN;
    let mut has_scene = false;
    // (scene, R) -> (width, height)
    let mut extents: HashMap<(i32, i32), (u32, u32)> = HashMap::new();

    for e in &entries {
        pixel_type = e.pixel_type;
        let scene = e.dim_start("S");
        if e.dims.contains_key("S") {
            has_scene = true;
            min_scene = min_scene.min(scene);
            max_scene = max_scene.max(scene);
        }
        let x_start = e.dim_start("X").max(0) as u32;
        let y_start = e.dim_start("Y").max(0) as u32;
        let x_size = e.dim_size("X").max(0) as u32;
        let y_size = e.dim_size("Y").max(0) as u32;
        let extent = extents.entry((scene, e.dim_start("R"))).or_insert((0, 0));
        extent.0 = extent.0.max(x_start.saturating_add(x_size));
        extent.1 = extent.1.max(y_start.saturating_add(y_size));
        if let Some(&(start, _)) = e.dims.get("Z") {
            if start > max_z {
                max_z = start;
            }
        }
        if let Some(&(start, _)) = e.dims.get("C") {
            if start > max_c {
                max_c = start;
            }
        }
        if let Some(&(start, _)) = e.dims.get("T") {
            if start > max_t {
                max_t = start;
            }
        }
    }

    // positions = maxS - minS + 1 (BaseZeissReader semantics); with no "S"
    // dimension a single scene with start 0.
    let (min_scene, positions) = if has_scene {
        (min_scene, (max_scene - min_scene + 1).max(1))
    } else {
        (0, 1)
    };

    // Build one CziSeries per scene, each with its sorted resolution list.
    let mut series: Vec<CziSeries> = Vec::with_capacity(positions as usize);
    for s in 0..positions {
        let scene = min_scene + s;
        let mut resolutions: Vec<CziResolution> = extents
            .iter()
            .filter(|((sc, _), _)| *sc == scene)
            .map(|((_, r), (width, height))| CziResolution {
                r: *r,
                width: *width,
                height: *height,
            })
            .collect();
        resolutions.sort_by_key(|res| res.r);
        if resolutions.is_empty() {
            resolutions.push(CziResolution {
                r: 0,
                width: 0,
                height: 0,
            });
        }
        series.push(CziSeries { scene, resolutions });
    }
    if series.is_empty() {
        series.push(CziSeries {
            scene: 0,
            resolutions: vec![CziResolution {
                r: 0,
                width: 0,
                height: 0,
            }],
        });
    }

    let (pt, s) = czi_pixel_type(pixel_type);
    let spp = s;

    Ok(CziParsed {
        meta_xml,
        entries,
        z_count: (max_z + 1) as u32,
        c_count: (max_c + 1) as u32,
        t_count: (max_t + 1) as u32,
        pixel_type: pt,
        spp,
        series,
    })
}

// ---- decompression ---------------------------------------------------------

fn decompress_subblock(
    data: &[u8],
    compression: i32,
    tile_width: usize,
    tile_height: usize,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    match compression {
        0 => Ok(data.to_vec()), // Uncompressed
        1 => {
            // JPEG
            let mut dec = jpeg_decoder::Decoder::new(data);
            dec.decode()
                .map_err(|e| BioFormatsError::Codec(e.to_string()))
        }
        2 => {
            // LZW
            use weezl::{decode::Decoder, BitOrder};
            let mut dec = Decoder::with_tiff_size_switch(BitOrder::Msb, 8);
            dec.decode(data)
                .map_err(|e| BioFormatsError::Codec(e.to_string()))
        }
        4 => {
            // JPEG-XR
            crate::common::codec::decompress_jpegxr(data)
        }
        5 => {
            // Zstd
            zstd::decode_all(data).map_err(BioFormatsError::Io)
        }
        6 => decompress_zstd_1(data),
        104 => {
            // Camera-specific 12-bit packed pixels, with column reversal.
            // (matches ZeissCZIReader case 104)
            let mut decoded = decode_12bit_camera(data, max_bytes)?;
            reverse_columns_16bit(&mut decoded, tile_width, tile_height);
            Ok(decoded)
        }
        504 => {
            // Camera-specific 12-bit packed pixels without column reversal.
            decode_12bit_camera(data, max_bytes)
        }
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "CZI: unknown compression {}",
            compression
        ))),
    }
}

/// Decode 12-bit camera-packed pixel data into 16-bit samples.
///
/// Port of ZeissCZIReader.decode12BitCamera: unpacks the input into 4-bit
/// nibbles (3 nibbles per 2 output bytes), performs an in-place nibble reorder,
/// then reassembles 16-bit values.
fn decode_12bit_camera(data: &[u8], max_bytes: usize) -> Result<Vec<u8>> {
    let mut decoded = vec![0u8; max_bytes];

    let four_bits_len = (max_bytes / 2) * 3;
    let mut four_bits = vec![0u8; four_bits_len];

    // Read 4-bit groups MSB-first from the packed input.
    let mut bit_pos = 0usize;
    for nibble in four_bits.iter_mut() {
        let byte_index = bit_pos / 8;
        if byte_index >= data.len() {
            break;
        }
        let in_byte_shift = 4 - (bit_pos % 8);
        *nibble = ((data[byte_index] >> in_byte_shift) & 0x0f) as u8;
        bit_pos += 4;
    }

    // In-place nibble reordering (matches the Java reference loop).
    if four_bits_len > 1 {
        for index in 1..four_bits_len - 1 {
            if (index as isize - 3) % 6 == 0 {
                let middle = four_bits[index];
                let last = four_bits[index + 1];
                let first = four_bits[index - 1];
                four_bits[index + 1] = middle;
                four_bits[index] = first;
                four_bits[index - 1] = last;
            }
        }
    }

    // Reassemble 16-bit values from the nibble stream.
    let mut current_byte = 0usize;
    let mut index = 0usize;
    while index < four_bits_len && current_byte < decoded.len() {
        if index % 3 == 0 {
            decoded[current_byte] = four_bits[index];
            current_byte += 1;
            index += 1;
        } else {
            let hi = four_bits[index];
            index += 1;
            let lo = if index < four_bits_len {
                four_bits[index]
            } else {
                0
            };
            index += 1;
            decoded[current_byte] = (hi << 4) | lo;
            current_byte += 1;
        }
    }

    Ok(decoded)
}

/// Reverse the column order of 16-bit pixels, row by row.
/// Port of the column-reversal loop in ZeissCZIReader case 104.
fn reverse_columns_16bit(data: &mut [u8], width: usize, height: usize) {
    if width == 0 {
        return;
    }
    for row in 0..height {
        for col in 0..width / 2 {
            let left = row * width * 2 + col * 2;
            let right = row * width * 2 + (width - col - 1) * 2;
            if right + 1 >= data.len() {
                continue;
            }
            data.swap(left, right);
            data.swap(left + 1, right + 1);
        }
    }
}

fn read_czi_varint(data: &[u8], offset: &mut usize) -> Result<usize> {
    if *offset >= data.len() {
        return Err(BioFormatsError::InvalidData(
            "CZI ZSTD_1 truncated varint".into(),
        ));
    }
    let a = data[*offset];
    *offset += 1;
    if a & 0x80 == 0 {
        return Ok(a as usize);
    }

    if *offset >= data.len() {
        return Err(BioFormatsError::InvalidData(
            "CZI ZSTD_1 truncated varint".into(),
        ));
    }
    let b = data[*offset];
    *offset += 1;
    if b & 0x80 == 0 {
        return Ok(((b as usize) << 7) | ((a & 0x7f) as usize));
    }

    if *offset >= data.len() {
        return Err(BioFormatsError::InvalidData(
            "CZI ZSTD_1 truncated varint".into(),
        ));
    }
    let c = data[*offset];
    *offset += 1;
    Ok(((c as usize) << 14) | (((b & 0x7f) as usize) << 7) | ((a & 0x7f) as usize))
}

fn decompress_zstd_1(data: &[u8]) -> Result<Vec<u8>> {
    let mut offset = 0usize;
    let header_end = read_czi_varint(data, &mut offset)?;
    if header_end > data.len() || header_end < offset {
        return Err(BioFormatsError::InvalidData(
            "CZI ZSTD_1 invalid header size".into(),
        ));
    }

    let mut high_low_unpacking = false;
    while offset < header_end {
        let chunk_id = read_czi_varint(data, &mut offset)?;
        match chunk_id {
            1 => {
                if offset >= header_end {
                    return Err(BioFormatsError::InvalidData(
                        "CZI ZSTD_1 missing chunk payload".into(),
                    ));
                }
                high_low_unpacking = (data[offset] & 1) == 1;
                offset += 1;
            }
            _ => {
                return Err(BioFormatsError::InvalidData(format!(
                    "CZI ZSTD_1 invalid chunk ID {chunk_id}"
                )));
            }
        }
    }

    let decoded = zstd::decode_all(&data[header_end..]).map_err(BioFormatsError::Io)?;
    if !high_low_unpacking {
        return Ok(decoded);
    }
    if decoded.len() % 2 != 0 {
        return Err(BioFormatsError::InvalidData(
            "CZI ZSTD_1 high/low decoded byte count is odd".into(),
        ));
    }

    let second_half = decoded.len() / 2;
    let mut out = vec![0; decoded.len()];
    for i in 0..decoded.len() {
        let half_offset = i / 2;
        out[i] = if i % 2 == 0 {
            decoded[half_offset]
        } else {
            decoded[second_half + half_offset]
        };
    }
    Ok(out)
}

// ---- reader ----------------------------------------------------------------

pub struct CziReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    entries: Vec<DirEntry>,
    meta_xml: String,
    packed_spp: u32,
    /// One series per scene (CZI "S" dimension); each has its own resolutions.
    /// `series[i].scene` is the absolute "S" start that selects scene `i`.
    series: Vec<CziSeries>,
    current_series: usize,
    current_resolution: usize,
}

impl CziReader {
    pub fn new() -> Self {
        CziReader {
            path: None,
            meta: None,
            entries: Vec::new(),
            meta_xml: String::new(),
            packed_spp: 1,
            series: Vec::new(),
            current_series: 0,
            current_resolution: 0,
        }
    }

    fn plane_zct(&self, plane_index: u32) -> Option<(u32, u32, u32)> {
        let meta = self.meta.as_ref()?;
        let sz = meta.size_z;
        let sc = meta.size_c;
        let z = (plane_index / sc) % sz;
        let c = plane_index % sc;
        let t = plane_index / (sc * sz);
        Some((z, c, t))
    }

    /// Resolution list for the active series.
    fn current_resolutions(&self) -> &[CziResolution] {
        self.series
            .get(self.current_series)
            .map(|s| s.resolutions.as_slice())
            .unwrap_or(&[])
    }

    fn matching_entries(&self, plane_index: u32) -> Option<Vec<DirEntry>> {
        let (z, c, t) = self.plane_zct(plane_index)?;
        let scene = self.series.get(self.current_series)?.scene;
        let has_scene = self.series.len() > 1;
        let r = self.current_resolutions().get(self.current_resolution)?.r;
        let entries: Vec<DirEntry> = self
            .entries
            .iter()
            .filter(|e| {
                // Match the active scene only when scenes exist; a file with a
                // single (or absent) "S" dimension exposes every plane.
                (!has_scene || e.dim_start("S") == scene)
                    && e.dim_start("R") == r
                    && e.matches_plane(z, c, t)
            })
            .cloned()
            .collect();
        (!entries.is_empty()).then_some(entries)
    }

    /// Apply the active series/resolution's X/Y size to the cached metadata.
    fn refresh_meta_dimensions(&mut self) {
        let (width, height, res_count) = {
            let resolutions = self.current_resolutions();
            let res_count = resolutions.len().max(1) as u32;
            let res = resolutions.get(self.current_resolution);
            (
                res.map(|r| r.width).unwrap_or(0),
                res.map(|r| r.height).unwrap_or(0),
                res_count,
            )
        };
        if let Some(meta) = self.meta.as_mut() {
            meta.size_x = width;
            meta.size_y = height;
            meta.resolution_count = res_count;
        }
    }

    fn read_subblock(path: &Path, entry: &DirEntry, pixel_bytes: usize) -> Result<Vec<u8>> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(entry.file_position as u64))
            .map_err(BioFormatsError::Io)?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        f.read_exact(&mut seg_hdr).map_err(BioFormatsError::Io)?;

        // SubBlock body (matching ZeissCZIReader.SubBlock.fillInData):
        //   body_start = file_position + HEADER_SIZE
        //   metadataSize (int), attachmentSize (int), dataSize (long) -> 16 bytes
        //   DirectoryEntry, then skip so the fixed part of the body is 256 bytes
        //   total (measured from body_start), then metadata of metadataSize bytes.
        // Pixel data therefore starts at body_start + 256 + metadataSize.
        let mut sb_hdr = vec![0u8; 16];
        f.read_exact(&mut sb_hdr).map_err(BioFormatsError::Io)?;
        let metadata_size = read_i32(&sb_hdr, 0) as u64;
        let data_size = read_u64(&sb_hdr, 8);

        // We have already consumed the 16-byte size header out of the 256-byte
        // fixed body, so skip the remaining (256 - 16) bytes plus the metadata.
        f.seek(SeekFrom::Current((256 - 16) + metadata_size as i64))
            .map_err(BioFormatsError::Io)?;

        let mut compressed = vec![0u8; data_size as usize];
        f.read_exact(&mut compressed).map_err(BioFormatsError::Io)?;

        // For compressed/downsampled tiles Java uses the stored (physical) X/Y
        // sizes to size the decoded buffer.
        let tile_w = entry.dim_stored_size("X").max(0) as usize;
        let tile_h = entry.dim_stored_size("Y").max(0) as usize;
        let max_bytes = tile_w * tile_h * pixel_bytes;
        decompress_subblock(&compressed, entry.compression, tile_w, tile_h, max_bytes)
    }

    fn assemble_entry(
        out: &mut [u8],
        out_width: u32,
        out_height: u32,
        tile: &[u8],
        entry: &DirEntry,
        pixel_bytes: usize,
    ) {
        let tile_x = entry.dim_start("X").max(0) as u32;
        let tile_y = entry.dim_start("Y").max(0) as u32;
        let tile_w = entry.dim_stored_size("X").max(0) as u32;
        let tile_h = entry.dim_stored_size("Y").max(0) as u32;
        let copy_w = tile_w.min(out_width.saturating_sub(tile_x));
        let copy_h = tile_h.min(out_height.saturating_sub(tile_y));
        let src_row_bytes = tile_w as usize * pixel_bytes;
        let dst_row_bytes = out_width as usize * pixel_bytes;
        let copy_bytes = copy_w as usize * pixel_bytes;

        for row in 0..copy_h as usize {
            let src_off = row * src_row_bytes;
            let dst_off = ((tile_y as usize + row) * dst_row_bytes) + tile_x as usize * pixel_bytes;
            if src_off + copy_bytes <= tile.len() && dst_off + copy_bytes <= out.len() {
                out[dst_off..dst_off + copy_bytes]
                    .copy_from_slice(&tile[src_off..src_off + copy_bytes]);
            }
        }
    }
}

impl Default for CziReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for CziReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("czi"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"ZISRAWFILE")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut reader = BufReader::new(f);
        let parsed = parse_czi_file(&mut reader).map_err(BioFormatsError::Io)?;

        let image_count = parsed.z_count * parsed.c_count * parsed.t_count;
        let bps = (parsed.pixel_type.bytes_per_sample() * 8) as u8;
        let is_rgb = parsed.spp >= 3;

        let mut series_metadata: HashMap<String, MetadataValue> = HashMap::new();
        series_metadata.insert(
            "czi_subblocks".into(),
            MetadataValue::Int(parsed.entries.len() as i64),
        );

        let first = parsed.series.first();
        let (init_w, init_h, init_res_count) = first
            .and_then(|s| s.resolutions.first().map(|r| (r.width, r.height, s.resolutions.len())))
            .unwrap_or((0, 0, 1));

        self.meta = Some(ImageMetadata {
            size_x: init_w,
            size_y: init_h,
            size_z: parsed.z_count,
            size_c: parsed.c_count,
            size_t: parsed.t_count,
            pixel_type: parsed.pixel_type,
            bits_per_pixel: bps,
            image_count,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: init_res_count as u32,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.packed_spp = parsed.spp.max(1);
        self.entries = parsed.entries;
        self.series = parsed.series;
        self.current_series = 0;
        self.current_resolution = 0;
        self.meta_xml = parsed.meta_xml;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.entries.clear();
        self.meta_xml.clear();
        self.packed_spp = 1;
        self.series.clear();
        self.current_series = 0;
        self.current_resolution = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len().max(1)
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_count() {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.current_series = s;
        // Switching scenes resets the active resolution to full-res (level 0),
        // matching how setSeries resets the core/resolution index in Java.
        self.current_resolution = 0;
        self.refresh_meta_dimensions();
        Ok(())
    }
    fn series(&self) -> usize {
        self.current_series
    }
    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let entries = self
            .matching_entries(plane_index)
            .ok_or_else(|| BioFormatsError::PlaneOutOfRange(plane_index))?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let bps = meta.pixel_type.bytes_per_sample();
        let expected = meta.size_x as usize * meta.size_y as usize * self.packed_spp as usize * bps;
        let pixel_bytes = self.packed_spp as usize * bps;
        let mut out = vec![0; expected];

        for entry in entries {
            let tile_w = entry.dim_stored_size("X").max(0) as usize;
            let tile_h = entry.dim_stored_size("Y").max(0) as usize;
            let tile_expected = tile_w * tile_h * pixel_bytes;
            let mut tile = Self::read_subblock(path, &entry, pixel_bytes)?;
            tile.truncate(tile_expected);
            tile.resize(tile_expected, 0);
            Self::assemble_entry(
                &mut out,
                meta.size_x,
                meta.size_y,
                &tile,
                &entry,
                pixel_bytes,
            );
        }
        if meta.is_rgb && self.packed_spp >= 3 {
            swap_bgr_to_rgb(&mut out, bps, self.packed_spp as usize);
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
        let meta = self.meta.as_ref().unwrap();
        crop_full_plane("CZI", &full, meta, self.packed_spp as usize, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        self.current_resolutions().len().max(1)
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        let count = self.current_resolutions().len();
        if level >= count {
            return Err(BioFormatsError::Format(format!(
                "CZI resolution level {} out of range (max {})",
                level,
                count.saturating_sub(1)
            )));
        }
        self.current_resolution = level;
        self.refresh_meta_dimensions();
        Ok(())
    }

    fn resolution(&self) -> usize {
        self.current_resolution
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.meta_xml.is_empty() {
            return None;
        }
        Some(crate::common::ome_metadata::OmeMetadata::from_czi_xml(
            &self.meta_xml,
        ))
    }
}

fn swap_bgr_to_rgb(buf: &mut [u8], bytes_per_sample: usize, samples_per_pixel: usize) {
    if samples_per_pixel < 3 || bytes_per_sample == 0 {
        return;
    }

    let pixel_bytes = bytes_per_sample * samples_per_pixel;
    for pixel in buf.chunks_exact_mut(pixel_bytes) {
        for i in 0..bytes_per_sample {
            pixel.swap(i, 2 * bytes_per_sample + i);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    fn put_i32(buf: &mut [u8], off: usize, value: i32) {
        buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_i64(buf: &mut [u8], off: usize, value: i64) {
        buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u64(buf: &mut [u8], off: usize, value: u64) {
        buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn segment_header(name: &str, used_size: u64) -> Vec<u8> {
        let mut header = vec![0; SEG_HEADER];
        header[..name.len()].copy_from_slice(name.as_bytes());
        put_u64(&mut header, 16, used_size);
        put_u64(&mut header, 24, used_size);
        header
    }

    fn dimension_entry(name: &str, start: i32, size: i32) -> [u8; 20] {
        let mut dim = [0; 20];
        dim[..name.len()].copy_from_slice(name.as_bytes());
        put_i32(&mut dim, 4, start);
        put_i32(&mut dim, 8, size);
        dim
    }

    fn directory_entry(pixel_type: i32, file_position: i64, c: i32, x: i32, y: i32) -> Vec<u8> {
        directory_entry_dims(pixel_type, file_position, c, 0, 0, x, y, 0)
    }

    fn directory_entry_dims(
        pixel_type: i32,
        file_position: i64,
        c: i32,
        x_start: i32,
        y_start: i32,
        x_size: i32,
        y_size: i32,
        r: i32,
    ) -> Vec<u8> {
        let mut entry = vec![0; 256];
        put_i32(&mut entry, 2, pixel_type);
        put_i64(&mut entry, 6, file_position);
        put_i32(&mut entry, 18, 0);
        put_i32(&mut entry, 28, 4);
        entry[32..52].copy_from_slice(&dimension_entry("X", x_start, x_size));
        entry[52..72].copy_from_slice(&dimension_entry("Y", y_start, y_size));
        entry[72..92].copy_from_slice(&dimension_entry("C", c, 1));
        entry[92..112].copy_from_slice(&dimension_entry("R", r, 1));
        entry
    }

    /// Directory entry carrying an explicit scene ("S") dimension, used to test
    /// the multi-series scene split (one series per S position).
    fn directory_entry_scene(
        pixel_type: i32,
        file_position: i64,
        scene: i32,
        x_size: i32,
        y_size: i32,
    ) -> Vec<u8> {
        let mut entry = vec![0; 256];
        put_i32(&mut entry, 2, pixel_type);
        put_i64(&mut entry, 6, file_position);
        put_i32(&mut entry, 18, 0);
        put_i32(&mut entry, 28, 5);
        entry[32..52].copy_from_slice(&dimension_entry("X", 0, x_size));
        entry[52..72].copy_from_slice(&dimension_entry("Y", 0, y_size));
        entry[72..92].copy_from_slice(&dimension_entry("C", 0, 1));
        entry[92..112].copy_from_slice(&dimension_entry("R", 0, 1));
        entry[112..132].copy_from_slice(&dimension_entry("S", scene, 1));
        entry
    }

    fn directory_entry_zc_dims(
        pixel_type: i32,
        file_position: i64,
        z: i32,
        c: i32,
        x_size: i32,
        y_size: i32,
    ) -> Vec<u8> {
        let mut entry = vec![0; 256];
        put_i32(&mut entry, 2, pixel_type);
        put_i64(&mut entry, 6, file_position);
        put_i32(&mut entry, 18, 0);
        put_i32(&mut entry, 28, 5);
        entry[32..52].copy_from_slice(&dimension_entry("X", 0, x_size));
        entry[52..72].copy_from_slice(&dimension_entry("Y", 0, y_size));
        entry[72..92].copy_from_slice(&dimension_entry("Z", z, 1));
        entry[92..112].copy_from_slice(&dimension_entry("C", c, 1));
        entry[112..132].copy_from_slice(&dimension_entry("R", 0, 1));
        entry
    }

    fn write_synthetic_bgr_czi(name: &str, pixel_type: i32, planes: &[Vec<u8>]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bioformats_czi_{name}_{}_{}.czi",
            std::process::id(),
            planes.len()
        ));
        let width = 2;
        let height = 1;
        let file_header_size = SEG_HEADER + 80;
        let dir_size = SEG_HEADER + 128 + planes.len() * 256;
        // Java-correct subblock layout: the fixed body (from body_start) is 256
        // bytes total, which includes the 16-byte size header. Pixel data follows.
        let subblock_size = |plane: &Vec<u8>| SEG_HEADER + 256 + plane.len();
        let dir_pos = file_header_size as u64;
        let mut subblock_pos = (file_header_size + dir_size) as u64;

        let mut data = Vec::new();
        data.extend_from_slice(&segment_header("ZISRAWFILE", file_header_size as u64));
        let mut file_header = vec![0; 80];
        put_u64(&mut file_header, 36, dir_pos);
        data.extend_from_slice(&file_header);

        data.extend_from_slice(&segment_header("ZISRAWDIRECTORY", dir_size as u64));
        let mut dir_header = vec![0; 128];
        put_i32(&mut dir_header, 0, planes.len() as i32);
        data.extend_from_slice(&dir_header);
        let mut entries = Vec::new();
        for (c, plane) in planes.iter().enumerate() {
            entries.push(directory_entry(
                pixel_type,
                subblock_pos as i64,
                c as i32,
                width,
                height,
            ));
            subblock_pos += subblock_size(plane) as u64;
        }
        for entry in &entries {
            data.extend_from_slice(entry);
        }

        for (_entry, plane) in entries.iter().zip(planes) {
            let used_size = (SEG_HEADER + 256 + plane.len()) as u64;
            data.extend_from_slice(&segment_header("ZISRAWSUBBLOCK", used_size));
            // 256-byte fixed body: 16-byte size header followed by 240 reserved bytes.
            let mut subblock_body = vec![0; 256];
            put_u64(&mut subblock_body, 8, plane.len() as u64);
            data.extend_from_slice(&subblock_body);
            data.extend_from_slice(plane);
        }

        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&data).unwrap();
        path
    }

    fn write_synthetic_czi_entries(
        name: &str,
        entries_and_pixels: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bioformats_czi_{name}_{}_{}.czi",
            std::process::id(),
            entries_and_pixels.len()
        ));
        let file_header_size = SEG_HEADER + 80;
        let dir_size = SEG_HEADER + 128 + entries_and_pixels.len() * 256;
        let dir_pos = file_header_size as u64;
        let mut subblock_pos = (file_header_size + dir_size) as u64;
        let mut entries = Vec::new();

        for (mut entry, pixels) in entries_and_pixels {
            put_i64(&mut entry, 6, subblock_pos as i64);
            subblock_pos += (SEG_HEADER + 256 + pixels.len()) as u64;
            entries.push((entry, pixels));
        }

        let mut data = Vec::new();
        data.extend_from_slice(&segment_header("ZISRAWFILE", file_header_size as u64));
        let mut file_header = vec![0; 80];
        put_u64(&mut file_header, 36, dir_pos);
        data.extend_from_slice(&file_header);

        data.extend_from_slice(&segment_header("ZISRAWDIRECTORY", dir_size as u64));
        let mut dir_header = vec![0; 128];
        put_i32(&mut dir_header, 0, entries.len() as i32);
        data.extend_from_slice(&dir_header);
        for (entry, _) in &entries {
            data.extend_from_slice(entry);
        }

        for (_entry, pixels) in &entries {
            let used_size = (SEG_HEADER + 256 + pixels.len()) as u64;
            data.extend_from_slice(&segment_header("ZISRAWSUBBLOCK", used_size));
            // 256-byte fixed body: 16-byte size header followed by 240 reserved bytes.
            let mut subblock_body = vec![0; 256];
            put_u64(&mut subblock_body, 8, pixels.len() as u64);
            data.extend_from_slice(&subblock_body);
            data.extend_from_slice(pixels);
        }

        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&data).unwrap();
        path
    }

    #[test]
    fn czi_varint_matches_java_encoding() {
        let mut offset = 0;
        assert_eq!(read_czi_varint(&[0x7f], &mut offset).unwrap(), 0x7f);
        assert_eq!(offset, 1);

        let mut offset = 0;
        assert_eq!(read_czi_varint(&[0x80, 0x01], &mut offset).unwrap(), 0x80);
        assert_eq!(offset, 2);

        let mut offset = 0;
        assert_eq!(
            read_czi_varint(&[0x80, 0x80, 0x01], &mut offset).unwrap(),
            0x4000
        );
        assert_eq!(offset, 3);
    }

    #[test]
    fn czi_zstd_1_plain_payload() {
        let payload = zstd::encode_all(&b"\x11\x22\x33\x44"[..], 0).unwrap();
        let mut wrapped = vec![3, 1, 0];
        wrapped.extend_from_slice(&payload);
        assert_eq!(
            decompress_zstd_1(&wrapped).unwrap(),
            vec![0x11, 0x22, 0x33, 0x44]
        );
    }

    #[test]
    fn czi_zstd_1_high_low_unpacking() {
        let payload = zstd::encode_all(&b"\x11\x33\x22\x44"[..], 0).unwrap();
        let mut wrapped = vec![3, 1, 1];
        wrapped.extend_from_slice(&payload);
        assert_eq!(
            decompress_zstd_1(&wrapped).unwrap(),
            vec![0x11, 0x22, 0x33, 0x44]
        );
    }

    #[test]
    fn czi_bgr24_keeps_logical_channels_separate_from_packed_samples() {
        let planes = vec![vec![1, 2, 3, 4, 5, 6], vec![7, 8, 9, 10, 11, 12]];
        let path = write_synthetic_bgr_czi("bgr24_logical_c", 3, &planes);
        let mut reader = CziReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_c, 2);
        assert_eq!(meta.image_count, 2);
        assert!(meta.is_rgb);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![3, 2, 1, 6, 5, 4]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![9, 8, 7, 12, 11, 10]);
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(),
            vec![12, 11, 10]
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_bgr48_keeps_logical_channels_separate_from_packed_samples() {
        let planes = vec![
            vec![1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0],
            vec![7, 0, 8, 0, 9, 0, 10, 0, 11, 0, 12, 0],
        ];
        let path = write_synthetic_bgr_czi("bgr48_logical_c", 4, &planes);
        let mut reader = CziReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_c, 2);
        assert_eq!(meta.image_count, 2);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert!(meta.is_rgb);
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            vec![3, 0, 2, 0, 1, 0, 6, 0, 5, 0, 4, 0]
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            vec![9, 0, 8, 0, 7, 0, 12, 0, 11, 0, 10, 0]
        );
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(),
            vec![12, 0, 11, 0, 10, 0]
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_assembles_mosaic_tiles_into_single_plane() {
        let entries = vec![
            (directory_entry_dims(0, 0, 0, 0, 0, 2, 1, 0), vec![1, 2]),
            (directory_entry_dims(0, 0, 0, 2, 0, 2, 1, 0), vec![3, 4]),
            (directory_entry_dims(0, 0, 0, 0, 1, 2, 1, 0), vec![5, 6]),
            (directory_entry_dims(0, 0, 0, 2, 1, 2, 1, 0), vec![7, 8]),
        ];
        let path = write_synthetic_czi_entries("mosaic_tiles", entries);
        let mut reader = CziReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (4, 2));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
            vec![2, 3, 6, 7]
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_uses_java_xyczt_plane_order() {
        let entries = vec![
            (directory_entry_zc_dims(0, 0, 0, 0, 1, 1), vec![10]),
            (directory_entry_zc_dims(0, 0, 0, 1, 1, 1), vec![11]),
            (directory_entry_zc_dims(0, 0, 1, 0, 1, 1), vec![12]),
            (directory_entry_zc_dims(0, 0, 1, 1, 1, 1), vec![13]),
        ];
        let path = write_synthetic_czi_entries("xyczt_order", entries);
        let mut reader = CziReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.dimension_order, DimensionOrder::XYCZT);
        assert_eq!((meta.size_z, meta.size_c, meta.size_t), (2, 2, 1));
        assert_eq!(meta.image_count, 4);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![10]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![11]);
        assert_eq!(reader.open_bytes(2).unwrap(), vec![12]);
        assert_eq!(reader.open_bytes(3).unwrap(), vec![13]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_selects_pyramid_resolution_level() {
        let entries = vec![
            (
                directory_entry_dims(0, 0, 0, 0, 0, 4, 2, 0),
                vec![1, 2, 3, 4, 5, 6, 7, 8],
            ),
            (directory_entry_dims(0, 0, 0, 0, 0, 2, 1, 1), vec![9, 10]),
        ];
        let path = write_synthetic_czi_entries("pyramid_levels", entries);
        let mut reader = CziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.resolution_count(), 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6, 7, 8]);

        reader.set_resolution(1).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (2, 1));
        assert_eq!(reader.resolution(), 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![9, 10]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_splits_scenes_into_separate_series() {
        // Two scenes (S=0, S=1), each a single 2x1 plane. ZeissCZIReader treats
        // each "S" position as its own series (positions = maxS - minS + 1).
        let entries = vec![
            (directory_entry_scene(0, 0, 0, 2, 1), vec![1, 2]),
            (directory_entry_scene(0, 0, 1, 2, 1), vec![3, 4]),
        ];
        let path = write_synthetic_czi_entries("scene_series", entries);
        let mut reader = CziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 2);

        // Series 0 -> scene S=0.
        assert_eq!(reader.series(), 0);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);

        // Series 1 -> scene S=1.
        reader.set_series(1).unwrap();
        assert_eq!(reader.series(), 1);
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (2, 1));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![3, 4]);

        // Switch back to series 0.
        reader.set_series(0).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);

        assert!(reader.set_series(2).is_err());

        fs::remove_file(path).unwrap();
    }
}
