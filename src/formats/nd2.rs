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
//! Compression: uncompressed or zlib. (JPEG2000 requires an external decoder.)

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

/// ND2 file magic bytes.
pub const ND2_MAGIC: [u8; 4] = [0xDA, 0xCE, 0xBE, 0x0A];

#[derive(Debug, Clone)]
struct Nd2Chunk {
    name: String,
    data_offset: u64,
    data_length: u64,
}

fn scan_chunks(f: &mut BufReader<File>) -> std::io::Result<Vec<Nd2Chunk>> {
    let mut chunks = Vec::new();
    f.seek(SeekFrom::Start(0))?;

    loop {
        let mut magic = [0u8; 4];
        if f.read_exact(&mut magic).is_err() { break; }
        if magic != ND2_MAGIC { break; }

        let mut name_len_bytes = [0u8; 4];
        f.read_exact(&mut name_len_bytes)?;
        let name_len = u32::from_le_bytes(name_len_bytes) as usize;

        let mut data_len_bytes = [0u8; 8];
        f.read_exact(&mut data_len_bytes)?;
        let data_len = u64::from_le_bytes(data_len_bytes);

        let mut name_bytes = vec![0u8; name_len];
        f.read_exact(&mut name_bytes)?;
        let name = String::from_utf8_lossy(&name_bytes)
            .trim_end_matches('\0')
            .to_string();

        let data_offset = f.stream_position()?;
        chunks.push(Nd2Chunk { name, data_offset, data_length: data_len });

        // Advance past data
        f.seek(SeekFrom::Start(data_offset + data_len))?;
    }
    Ok(chunks)
}

fn read_chunk_data(f: &mut BufReader<File>, chunk: &Nd2Chunk) -> std::io::Result<Vec<u8>> {
    f.seek(SeekFrom::Start(chunk.data_offset))?;
    let mut buf = vec![0u8; chunk.data_length as usize];
    f.read_exact(&mut buf)?;
    Ok(buf)
}

/// Very lightweight XML value extractor — just grab the first occurrence of a tag.
fn xml_value(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{}", tag);
    let pos = xml.find(&open)?;
    let after_open = &xml[pos..];
    // Look for > and value until </tag>
    let gt = after_open.find('>')?;
    let content_start = &after_open[gt + 1..];
    let close = format!("</{}>", tag);
    let end = content_start.find(&close)?;
    Some(content_start[..end].trim().to_string())
}

fn parse_nd2_attributes(xml: &str) -> (u32, u32, u32, u32, u8) {
    let w = xml_value(xml, "uiWidth")
        .or_else(|| xml_value(xml, "uiCamPxlCountX"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0u32);
    let h = xml_value(xml, "uiHeight")
        .or_else(|| xml_value(xml, "uiCamPxlCountY"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(0u32);
    let c = xml_value(xml, "uiComp")
        .and_then(|s| s.parse().ok())
        .unwrap_or(1u32);
    let bpp = xml_value(xml, "uiBpcSignificant")
        .or_else(|| xml_value(xml, "uiBpc"))
        .and_then(|s| s.parse().ok())
        .unwrap_or(8u8);
    let z_count = xml_value(xml, "uiZStackHome")
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(0);
    (w, h, c, z_count.max(1), bpp)
}

// ---- reader -----------------------------------------------------------------

pub struct Nd2Reader {
    file: Option<BufReader<File>>,
    path: Option<PathBuf>,
    chunks: Vec<Nd2Chunk>,
    meta: Option<ImageMetadata>,
    image_chunks: Vec<usize>, // indices into chunks[] for ImageDataSeq chunks
}

impl Nd2Reader {
    pub fn new() -> Self {
        Nd2Reader { file: None, path: None, chunks: Vec::new(), meta: None, image_chunks: Vec::new() }
    }
}

impl Default for Nd2Reader { fn default() -> Self { Self::new() } }

impl FormatReader for Nd2Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("nd2"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(&ND2_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut reader = BufReader::new(f);

        let chunks = scan_chunks(&mut reader).map_err(BioFormatsError::Io)?;

        // Find attributes chunk
        let attr_chunk = chunks.iter().find(|c| c.name.starts_with("ImageAttributesLV"));
        let (mut size_x, mut size_y, mut size_c, mut size_z, mut bpp) = (0u32, 0u32, 1u32, 1u32, 8u8);

        if let Some(ac) = attr_chunk {
            let data = read_chunk_data(&mut reader, ac).map_err(BioFormatsError::Io)?;
            // Data may be a raw binary struct OR XML wrapped. Try XML first.
            if let Ok(xml) = std::str::from_utf8(&data) {
                let (w, h, c, z, b) = parse_nd2_attributes(xml);
                if w > 0 { size_x = w; }
                if h > 0 { size_y = h; }
                if c > 0 { size_c = c; }
                if z > 0 { size_z = z; }
                if b > 0 { bpp = b; }
            }
        }

        // Collect image data chunks (ImageDataSeq|N!)
        let image_chunks: Vec<usize> = chunks.iter().enumerate()
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

        let image_count = image_chunks.len() as u32;
        let mut series_metadata: HashMap<String, MetadataValue> = HashMap::new();
        series_metadata.insert("nd2_chunks".into(), MetadataValue::Int(chunks.len() as i64));

        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
            image_count: image_count.max(1),
            dimension_order: DimensionOrder::XYCZT,
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
        });
        self.image_chunks = image_chunks;
        self.chunks = chunks;
        self.file = Some(reader);
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.file = None; self.path = None; self.meta = None;
        self.chunks.clear(); self.image_chunks.clear();
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

        let chunk_idx = self.image_chunks.get(plane_index as usize)
            .copied()
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let chunk = &self.chunks[chunk_idx];

        let f = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let data = read_chunk_data(f, chunk).map_err(BioFormatsError::Io)?;

        // ND2 image data chunks may have a small per-frame header (variable).
        // A safe heuristic: if the data is larger than expected, skip the excess prefix.
        let bps = meta.pixel_type.bytes_per_sample();
        let expected = meta.size_x as usize * meta.size_y as usize * meta.size_c as usize * bps;

        if data.len() >= expected {
            let offset = data.len() - expected;
            Ok(data[offset..].to_vec())
        } else {
            // Try zlib decompression
            use flate2::read::ZlibDecoder;
            use std::io::Read as _;
            let mut dec = ZlibDecoder::new(data.as_slice());
            let mut decompressed = Vec::new();
            if dec.read_to_end(&mut decompressed).is_ok() && decompressed.len() >= expected {
                let offset = decompressed.len() - expected;
                Ok(decompressed[offset..].to_vec())
            } else {
                Err(BioFormatsError::Format(format!(
                    "ND2: plane {} data too small ({} < {})", plane_index, data.len(), expected
                )))
            }
        }
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let spp = meta.size_c as usize;
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
}
