//! Leica LIF (Leica Image Format) reader.
//!
//! LIF is a binary container format. The file begins with a header containing
//! UTF-16 XML metadata that describes all image series, followed by raw image
//! data blocks.
//!
//! Structure (all values little-endian):
//! - Byte 0: magic = 0x70
//! - Bytes 1-3: zero padding
//! - Byte 4: memory indicator = 0x2a
//! - Bytes 5-8: XML character count (int32 LE)
//! - Bytes 9+: UTF-16LE XML (char_count * 2 bytes)
//! - Then: repeating memory blocks (magic=0x70 int32, skip 4, 0x2a byte,
//!         length int32/int64, 0x2a, UTF-16 block ID, raw image data)

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use quick_xml::events::Event;
use quick_xml::Reader as XmlReader;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;

const LIF_MAGIC: u8 = 0x70;
const LIF_MEMORY: u8 = 0x2a;

// ---- memory block -----------------------------------------------------------

#[derive(Debug, Clone)]
struct MemBlock {
    id: String,
    file_offset: u64,
    byte_length: u64,
}

// ---- series info extracted from XML ----------------------------------------

#[derive(Debug, Default, Clone)]
struct LifSeries {
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    bits_per_pixel: u8,
    block_id: String,
    channel_bytes_list: Vec<u64>, // byte sizes per channel/plane
}

// ---- file parsing -----------------------------------------------------------

fn read_u8(f: &mut File) -> std::io::Result<u8> {
    let mut b = [0u8; 1];
    f.read_exact(&mut b)?;
    Ok(b[0])
}

fn read_i32_le(f: &mut File) -> std::io::Result<i32> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b)?;
    Ok(i32::from_le_bytes(b))
}

/// Read a UTF-16LE string prefixed with a u32 character count.
fn read_utf16_string(f: &mut File) -> std::io::Result<String> {
    let char_count = read_i32_le(f)? as usize;
    let byte_count = char_count * 2;
    let mut bytes = vec![0u8; byte_count];
    f.read_exact(&mut bytes)?;
    let chars: Vec<u16> = bytes.chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    Ok(String::from_utf16_lossy(&chars).to_string())
}

fn parse_lif_file(path: &Path) -> Result<(Vec<LifSeries>, Vec<MemBlock>)> {
    let mut f = File::open(path).map_err(BioFormatsError::Io)?;

    // Header
    let magic = read_u8(&mut f).map_err(BioFormatsError::Io)?;
    if magic != LIF_MAGIC {
        return Err(BioFormatsError::Format(format!("LIF: bad magic byte {}", magic)));
    }
    // Skip 3 bytes
    let mut _skip = [0u8; 3];
    f.read_exact(&mut _skip).map_err(BioFormatsError::Io)?;

    let mem_byte = read_u8(&mut f).map_err(BioFormatsError::Io)?;
    if mem_byte != LIF_MEMORY {
        return Err(BioFormatsError::Format("LIF: missing memory byte in header".into()));
    }

    let xml_str = read_utf16_string(&mut f).map_err(BioFormatsError::Io)?;
    let series = parse_xml_metadata(&xml_str)?;

    // Scan memory blocks
    let mut blocks: Vec<MemBlock> = Vec::new();
    loop {
        let magic_val = match read_i32_le(&mut f) {
            Ok(v) => v,
            Err(_) => break,
        };
        if magic_val != LIF_MAGIC as i32 {
            break;
        }
        // Skip 4 bytes
        let mut skip = [0u8; 4];
        if f.read_exact(&mut skip).is_err() { break; }

        let mem = match read_u8(&mut f) {
            Ok(b) => b,
            Err(_) => break,
        };
        if mem != LIF_MEMORY { break; }

        // Read length — may be int32 or int64 depending on next byte
        let len_lo = match read_i32_le(&mut f) {
            Ok(v) => v as u64,
            Err(_) => break,
        };

        // Check if next byte is 0x2a (meaning a second u32 follows for 64-bit length)
        let next_byte = match read_u8(&mut f) {
            Ok(b) => b,
            Err(_) => break,
        };

        let block_bytes = if next_byte == LIF_MEMORY {
            // 64-bit length: len_lo | (next_u32 << 32)
            let len_hi = match read_i32_le(&mut f) {
                Ok(v) => v as u64,
                Err(_) => break,
            };
            len_lo | (len_hi << 32)
        } else {
            // We consumed an extra byte; back up by 1 (only works if seekable)
            if f.seek(SeekFrom::Current(-1)).is_err() { break; }
            len_lo
        };

        // Block ID string
        let id = match read_utf16_string(&mut f) {
            Ok(s) => s,
            Err(_) => break,
        };

        // Image data follows immediately
        let data_offset = match f.stream_position() {
            Ok(p) => p,
            Err(_) => break,
        };

        blocks.push(MemBlock { id, file_offset: data_offset, byte_length: block_bytes });

        // Skip past image data to next block
        if f.seek(SeekFrom::Current(block_bytes as i64)).is_err() { break; }
    }

    Ok((series, blocks))
}

/// Parse XML and extract series metadata.
fn parse_xml_metadata(xml: &str) -> Result<Vec<LifSeries>> {
    let mut series_list: Vec<LifSeries> = Vec::new();
    let mut current: Option<LifSeries> = None;
    let mut in_image = false;
    let mut reader = XmlReader::from_str(xml);
    reader.config_mut().trim_text(true);

    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = std::str::from_utf8(e.name().as_ref()).unwrap_or("").to_ascii_uppercase();
                match name.as_str() {
                    "ELEMENT" => {
                        // <Element Name="..."> is a top-level element (series or folder)
                        let has_name = e.attributes().any(|a| {
                            a.map(|a| a.key.as_ref() == b"Name").unwrap_or(false)
                        });
                        if has_name {
                            in_image = true;
                            current = Some(LifSeries::default());
                        }
                    }
                    "DIMDES" | "DIMENSIONDESCRIPTION" => {
                        // <DimDes DimID="1" NumberOfElements="512" ...>
                        if in_image {
                            let mut dim_id = 0u8;
                            let mut count = 0u32;
                            for attr in e.attributes().flatten() {
                                match attr.key.as_ref() {
                                    b"DimID" => dim_id = std::str::from_utf8(&attr.value).unwrap_or("0").parse().unwrap_or(0),
                                    b"NumberOfElements" => count = std::str::from_utf8(&attr.value).unwrap_or("0").parse().unwrap_or(0),
                                    _ => {}
                                }
                            }
                            if let Some(ref mut s) = current {
                                match dim_id {
                                    1 => s.size_x = count,
                                    2 => s.size_y = count,
                                    3 => s.size_z = count.max(1),
                                    4 => s.size_t = count.max(1),
                                    _ => {}
                                }
                            }
                        }
                    }
                    "CHANNELDESCRIPTION" => {
                        if in_image {
                            if let Some(ref mut s) = current {
                                s.size_c += 1;
                                for attr in e.attributes().flatten() {
                                    if attr.key.as_ref() == b"BytesInc" {
                                        if let Ok(b) = std::str::from_utf8(&attr.value).unwrap_or("0").parse::<u64>() {
                                            s.channel_bytes_list.push(b);
                                        }
                                    }
                                    if attr.key.as_ref() == b"Resolution" {
                                        if let Ok(bpp) = std::str::from_utf8(&attr.value).unwrap_or("8").parse::<u8>() {
                                            s.bits_per_pixel = bpp;
                                        }
                                    }
                                }
                            }
                        }
                    }
                    "MEMORYLAYOUTCOUNT" | "DATACHUNK" => {
                        // Extract block ID
                        if in_image {
                            for attr in e.attributes().flatten() {
                                if attr.key.as_ref() == b"MemoryBlockID" {
                                    if let Some(ref mut s) = current {
                                        s.block_id = String::from_utf8_lossy(&attr.value).into_owned();
                                    }
                                }
                            }
                        }
                    }
                    _ => {}
                }
            }
            Ok(Event::End(e)) => {
                let name = std::str::from_utf8(e.name().as_ref()).unwrap_or("").to_ascii_uppercase();
                if name == "ELEMENT" {
                    if let Some(s) = current.take() {
                        if s.size_x > 0 && s.size_y > 0 {
                            series_list.push(s);
                        }
                    }
                    in_image = false;
                }
            }
            Ok(Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }

    Ok(series_list)
}

// ---- reader -----------------------------------------------------------------

pub struct LifReader {
    path: Option<PathBuf>,
    series_list: Vec<LifSeries>,
    blocks: Vec<MemBlock>,
    current_series: usize,
}

impl LifReader {
    pub fn new() -> Self {
        LifReader { path: None, series_list: Vec::new(), blocks: Vec::new(), current_series: 0 }
    }

    fn find_block(&self, id: &str) -> Option<&MemBlock> {
        self.blocks.iter().find(|b| b.id.contains(id) || id.contains(&b.id))
    }
}

impl Default for LifReader { fn default() -> Self { Self::new() } }

impl FormatReader for LifReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lif"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        !header.is_empty() && header[0] == LIF_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (series, blocks) = parse_lif_file(path)?;
        self.series_list = series;
        self.blocks = blocks;
        self.path = Some(path.to_path_buf());
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None; self.series_list.clear(); self.blocks.clear();
        Ok(())
    }

    fn series_count(&self) -> usize { self.series_list.len().max(1) }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_list.len() { return Err(BioFormatsError::SeriesOutOfRange(s)); }
        self.current_series = s;
        Ok(())
    }

    fn series(&self) -> usize { self.current_series }

    fn metadata(&self) -> &ImageMetadata {
        panic!("LIF metadata is computed on the fly; use open_bytes to access data")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let s = self.series_list.get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;

        let block = self.find_block(&s.block_id).ok_or_else(|| {
            BioFormatsError::Format(format!("LIF: data block '{}' not found", s.block_id))
        })?;

        let bps = match s.bits_per_pixel {
            16 => 2usize,
            _ => 1,
        };
        let spp = s.size_c.max(1) as usize;
        let plane_bytes = s.size_x as usize * s.size_y as usize * spp * bps;
        let offset = block.file_offset + plane_index as u64 * plane_bytes as u64;

        if offset + plane_bytes as u64 > block.file_offset + block.byte_length {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let s = self.series_list.get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let bps = match s.bits_per_pixel { 16 => 2usize, _ => 1 };
        let spp = s.size_c.max(1) as usize;
        let row_bytes = s.size_x as usize * spp * bps;
        let out_row = w as usize * spp * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let st = x as usize * spp * bps;
            out.extend_from_slice(&src[st..st + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (sx, sy) = {
            let s = self.series_list.get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized)?;
            (s.size_x, s.size_y)
        };
        let (tw, th) = (sx.min(256), sy.min(256));
        let (tx, ty) = ((sx - tw) / 2, (sy - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
