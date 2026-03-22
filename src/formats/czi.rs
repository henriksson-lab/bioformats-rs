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

// ---- pixel types (from DirectoryEntry) -------------------------------------

fn czi_pixel_type(code: i32) -> (PixelType, u32) {
    // Returns (pixel_type, samples_per_pixel)
    match code {
        0 => (PixelType::Uint8, 1),   // Gray8
        1 => (PixelType::Uint16, 1),  // Gray16
        2 => (PixelType::Float32, 1), // GrayFloat
        3 => (PixelType::Uint8, 3),   // Bgr24
        4 => (PixelType::Uint16, 3),  // Bgr48
        8 => (PixelType::Float32, 3), // BgrFloat
        9 => (PixelType::Uint8, 4),   // Bgra32
        10 => (PixelType::Float32, 2),// Complex (re+im)
        11 => (PixelType::Float32, 2),// ComplexFloat
        12 => (PixelType::Uint32, 1), // Gray32
        13 => (PixelType::Float64, 1),// GrayDouble
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
    i32::from_le_bytes([data[off], data[off+1], data[off+2], data[off+3]])
}
fn read_i64(data: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(data[off..off+8].try_into().unwrap_or([0;8]))
}
fn read_u64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off+8].try_into().unwrap_or([0;8]))
}

// ---- DirectoryEntry (256 bytes) -------------------------------------------

#[derive(Debug, Clone)]
struct DirEntry {
    pixel_type: i32,
    file_position: i64,
    compression: i32,
    // Dimensions from DimensionEntry array
    dims: HashMap<String, (i32, i32)>, // dim_name -> (start, size)
}

fn parse_dir_entry(data: &[u8]) -> DirEntry {
    // schema 0-1 (2 bytes)
    let pixel_type = read_i32(data, 2);
    let file_position = read_i64(data, 6);
    let compression = read_i32(data, 18);
    let dim_count = read_i32(data, 28) as usize;

    let mut dims: HashMap<String, (i32, i32)> = HashMap::new();
    let dim_array_start = 32;
    for i in 0..dim_count {
        let off = dim_array_start + i * 20;
        if off + 20 > data.len() { break; }
        let dim_name = std::str::from_utf8(&data[off..off+4])
            .unwrap_or("")
            .trim_end_matches('\0')
            .trim()
            .to_string();
        let start = read_i32(data, off + 4);
        let size = read_i32(data, off + 8);
        if !dim_name.is_empty() {
            dims.insert(dim_name, (start, size));
        }
    }

    DirEntry { pixel_type, file_position, compression, dims }
}

// ---- file parsing ----------------------------------------------------------

struct CziParsed {
    meta_xml: String,
    entries: Vec<DirEntry>,
    width: u32,
    height: u32,
    z_count: u32,
    c_count: u32,
    t_count: u32,
    pixel_type: PixelType,
    spp: u32,
}

fn parse_czi_file(f: &mut BufReader<File>) -> std::io::Result<CziParsed> {
    // --- Read file header segment ---
    let mut hdr = vec![0u8; SEG_HEADER];
    f.read_exact(&mut hdr)?;
    let seg_type = read_seg_type(&hdr);
    if !seg_type.starts_with("ZISRAWFILE") {
        return Err(std::io::Error::new(std::io::ErrorKind::InvalidData, "Not a CZI file"));
    }

    // FileHeader data starts after the 32-byte segment header
    let mut fh = vec![0u8; 80];
    f.read_exact(&mut fh)?;
    // major = fh[0..4], minor = fh[4..8]
    let dir_position = read_u64(&fh, 36);
    let meta_position = read_u64(&fh, 44);

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
        // Directory body: entry_count (i32), reserved (124), DirectoryEntry[]
        let mut dir_hdr = vec![0u8; 128];
        f.read_exact(&mut dir_hdr)?;
        let entry_count = read_i32(&dir_hdr, 0) as usize;
        for _ in 0..entry_count {
            let mut entry_buf = vec![0u8; 256];
            if f.read_exact(&mut entry_buf).is_err() { break; }
            entries.push(parse_dir_entry(&entry_buf));
        }
    }

    // Compute dimensions from entries
    let mut max_x = 0u32;
    let mut max_y = 0u32;
    let mut max_z = 0i32;
    let mut max_c = 0i32;
    let mut max_t = 0i32;
    let mut pixel_type = 0i32;

    for e in &entries {
        pixel_type = e.pixel_type;
        if let Some(&(_, sz)) = e.dims.get("X") { if sz as u32 > max_x { max_x = sz as u32; } }
        if let Some(&(_, sz)) = e.dims.get("Y") { if sz as u32 > max_y { max_y = sz as u32; } }
        if let Some(&(start, _)) = e.dims.get("Z") { if start > max_z { max_z = start; } }
        if let Some(&(start, _)) = e.dims.get("C") { if start > max_c { max_c = start; } }
        if let Some(&(start, _)) = e.dims.get("T") { if start > max_t { max_t = start; } }
    }

    let (pt, s) = czi_pixel_type(pixel_type);
    let spp = s;

    Ok(CziParsed {
        meta_xml,
        entries,
        width: max_x,
        height: max_y,
        z_count: (max_z + 1) as u32,
        c_count: (max_c + 1) as u32,
        t_count: (max_t + 1) as u32,
        pixel_type: pt,
        spp,
    })
}

// ---- decompression ---------------------------------------------------------

fn decompress_subblock(data: &[u8], compression: i32) -> Result<Vec<u8>> {
    match compression {
        0 => Ok(data.to_vec()), // Uncompressed
        1 => { // JPEG
            let mut dec = jpeg_decoder::Decoder::new(data);
            dec.decode().map_err(|e| BioFormatsError::Codec(e.to_string()))
        }
        2 => { // LZW
            use weezl::{BitOrder, decode::Decoder};
            let mut dec = Decoder::with_tiff_size_switch(BitOrder::Msb, 8);
            dec.decode(data).map_err(|e| BioFormatsError::Codec(e.to_string()))
        }
        4 => { // JPEG-XR — not yet supported
            Err(BioFormatsError::UnsupportedFormat("CZI: JPEG-XR compression not yet supported".into()))
        }
        5 | 6 => { // Zstd
            zstd::decode_all(data).map_err(BioFormatsError::Io)
        }
        _ => Err(BioFormatsError::UnsupportedFormat(format!("CZI: unknown compression {}", compression))),
    }
}

// ---- reader ----------------------------------------------------------------

pub struct CziReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    entries: Vec<DirEntry>,
    meta_xml: String,
}

impl CziReader {
    pub fn new() -> Self {
        CziReader { path: None, meta: None, entries: Vec::new(), meta_xml: String::new() }
    }

    fn find_entry(&self, plane_index: u32) -> Option<&DirEntry> {
        // Map plane_index to (z, c, t) using XYCZT ordering, then find matching entry
        let meta = self.meta.as_ref()?;
        let sz = meta.size_z;
        let sc = meta.size_c;
        let z = plane_index % sz;
        let c = (plane_index / sz) % sc;
        let t = plane_index / (sz * sc);

        self.entries.iter().find(|e| {
            e.dims.get("Z").map(|&(s, _)| s as u32 == z).unwrap_or(z == 0)
            && e.dims.get("C").map(|&(s, _)| s as u32 == c).unwrap_or(c == 0)
            && e.dims.get("T").map(|&(s, _)| s as u32 == t).unwrap_or(t == 0)
        })
    }
}

impl Default for CziReader { fn default() -> Self { Self::new() } }

impl FormatReader for CziReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
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
        series_metadata.insert("czi_subblocks".into(), MetadataValue::Int(parsed.entries.len() as i64));

        self.meta = Some(ImageMetadata {
            size_x: parsed.width,
            size_y: parsed.height,
            size_z: parsed.z_count,
            size_c: parsed.c_count,
            size_t: parsed.t_count,
            pixel_type: parsed.pixel_type,
            bits_per_pixel: bps,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
        });
        self.entries = parsed.entries;
        self.meta_xml = parsed.meta_xml;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None; self.meta = None; self.entries.clear(); self.meta_xml.clear();
        Ok(())
    }

    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let entry = self.find_entry(plane_index)
            .ok_or_else(|| BioFormatsError::PlaneOutOfRange(plane_index))?;
        let file_pos = entry.file_position as u64;
        let compression = entry.compression;

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;

        // Each subblock segment: 32-byte seg header, then SubBlockData header
        f.seek(SeekFrom::Start(file_pos)).map_err(BioFormatsError::Io)?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        f.read_exact(&mut seg_hdr).map_err(BioFormatsError::Io)?;
        let used_size = read_u64(&seg_hdr, 24);

        // SubBlock data header: metadata_size(i32) + attach_size(i32) + data_size(i64) + dir_entry(256)
        let mut sb_hdr = vec![0u8; 16];
        f.read_exact(&mut sb_hdr).map_err(BioFormatsError::Io)?;
        let metadata_size = read_i32(&sb_hdr, 0) as u64;
        let attach_size = read_i32(&sb_hdr, 4) as u64;
        let data_size = read_u64(&sb_hdr, 8);

        // Skip DirectoryEntry (256 bytes) + metadata + attachment
        f.seek(SeekFrom::Current(256 + metadata_size as i64 + attach_size as i64))
            .map_err(BioFormatsError::Io)?;

        let mut compressed = vec![0u8; data_size as usize];
        f.read_exact(&mut compressed).map_err(BioFormatsError::Io)?;

        let raw = decompress_subblock(&compressed, compression)?;

        // Trim/pad to expected plane size
        let bps = meta.pixel_type.bytes_per_sample();
        let expected = meta.size_x as usize * meta.size_y as usize * meta.spp() * bps;
        let _ = (used_size, attach_size);
        let mut out = raw;
        out.truncate(expected);
        while out.len() < expected { out.push(0); }
        Ok(out)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let spp = meta.spp();
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = meta.size_x as usize * spp * bps;
        let out_row = w as usize * spp * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * spp * bps;
            out.extend_from_slice(&src[s..s + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.meta_xml.is_empty() { return None; }
        Some(crate::common::ome_metadata::OmeMetadata::from_czi_xml(&self.meta_xml))
    }
}

// Helper: samples per pixel from ImageMetadata
trait SppExt { fn spp(&self) -> usize; }
impl SppExt for ImageMetadata {
    fn spp(&self) -> usize { if self.is_rgb { self.size_c as usize } else { 1 } }
}
