//! Amira Mesh (.am / .amiramesh) and Spider EM (.spi / .xmp) format readers.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ─── Amira Mesh ───────────────────────────────────────────────────────────────

/// Parsed Amira Mesh header.
struct AmiraHeader {
    nx: u32,
    ny: u32,
    nz: u32,
    pixel_type: PixelType,
    data_offset: u64,
    little_endian: bool,
    /// True when the data stream is stored as ASCII numbers, not raw binary.
    ascii: bool,
}

/// Parse the Amira Mesh ASCII header (port of AmiraParameters.java).
///
/// Endianness comes from the per-stream encoding token on the first line:
///   `BINARY`               -> big-endian
///   `BINARY-LITTLE-ENDIAN` -> little-endian
///   `ASCII`                -> ASCII-encoded numbers
fn parse_amira_header(path: &Path) -> Result<AmiraHeader> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut reader = BufReader::new(f);

    let mut nx = 0u32;
    let mut ny = 0u32;
    let mut nz = 0u32;
    let mut pixel_type = PixelType::Uint8;
    let mut little_endian = false;
    let mut ascii = false;
    let mut data_section: u32 = 1; // default @1

    let mut line = String::new();
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(BioFormatsError::Io)?;
        if n == 0 {
            break;
        }
        let t = line.trim();

        // First line: encoding token determines endianness / ASCII mode.
        if t.starts_with("# AmiraMesh") || t.starts_with("# Avizo") {
            let up = t.to_ascii_uppercase();
            if up.contains("BINARY-LITTLE-ENDIAN") {
                little_endian = true;
            } else if up.contains("BINARY") {
                // Plain BINARY is big-endian.
                little_endian = false;
            } else if up.contains("ASCII") {
                ascii = true;
                little_endian = true; // immaterial for ASCII
            }
        }

        // "define Lattice NX NY NZ"
        if t.starts_with("define Lattice") {
            let parts: Vec<&str> = t.split_ascii_whitespace().collect();
            if parts.len() >= 5 {
                nx = parts[2].parse().unwrap_or(0);
                ny = parts[3].parse().unwrap_or(0);
                nz = parts[4].parse().unwrap_or(1);
            } else if parts.len() >= 4 {
                nx = parts[2].parse().unwrap_or(0);
                ny = parts[3].parse().unwrap_or(0);
                nz = 1;
            }
        }

        // Lattice data type: "Lattice { byte Data } @1" etc.
        if t.starts_with("Lattice") && t.contains("Data") {
            let lo = t.to_ascii_lowercase();
            // Order matters: check "ushort" before "short", "double" before
            // "float" is fine, and "int" must not match "point"-like tokens.
            pixel_type = if lo.contains("double") {
                PixelType::Float64
            } else if lo.contains("float") {
                PixelType::Float32
            } else if lo.contains("ushort") || lo.contains("unsigned short") {
                PixelType::Uint16
            } else if lo.contains("short") {
                PixelType::Int16
            } else if lo.contains("int") {
                PixelType::Int32
            } else {
                PixelType::Uint8 // "byte"
            };
            // Extract @N section number
            if let Some(at_pos) = t.rfind('@') {
                if let Ok(n) = t[at_pos + 1..].trim().parse::<u32>() {
                    data_section = n;
                }
            }
        }

        // Find @N marker in body — data starts on the next line
        if t == format!("@{}", data_section) {
            let data_offset = reader.stream_position().map_err(BioFormatsError::Io)?;
            return Ok(AmiraHeader {
                nx: nx.max(1),
                ny: ny.max(1),
                nz: nz.max(1),
                pixel_type,
                data_offset,
                little_endian,
                ascii,
            });
        }
    }

    Err(BioFormatsError::Format(
        "Amira Mesh: could not find data section".into(),
    ))
}

pub struct AmiraReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
    ascii: bool,
}

impl AmiraReader {
    pub fn new() -> Self {
        AmiraReader {
            path: None,
            meta: None,
            data_offset: 0,
            ascii: false,
        }
    }

    /// Read one plane's worth of ASCII-encoded numbers and pack into bytes
    /// according to the pixel type. Numbers are whitespace-separated.
    fn read_ascii_plane(&self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let pixel_type = meta.pixel_type;
        let bps = pixel_type.bytes_per_sample();
        let count = (meta.size_x * meta.size_y) as usize;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut reader = BufReader::new(f);
        reader
            .seek(SeekFrom::Start(self.data_offset))
            .map_err(BioFormatsError::Io)?;

        // Read all of the remaining text and tokenize. ASCII Amira streams store
        // values plane-major; skip the planes before the requested one.
        let mut text = String::new();
        reader.read_to_string(&mut text).map_err(BioFormatsError::Io)?;
        let skip = plane_index as usize * count;

        let mut out = vec![0u8; count * bps];
        for (i, tok) in text.split_ascii_whitespace().skip(skip).take(count).enumerate() {
            let dst = &mut out[i * bps..(i + 1) * bps];
            match pixel_type {
                PixelType::Float32 => {
                    let v: f32 = tok.parse().unwrap_or(0.0);
                    dst.copy_from_slice(&v.to_le_bytes());
                }
                PixelType::Float64 => {
                    let v: f64 = tok.parse().unwrap_or(0.0);
                    dst.copy_from_slice(&v.to_le_bytes());
                }
                PixelType::Int32 => {
                    let v: i32 = tok.parse().unwrap_or(0);
                    dst.copy_from_slice(&v.to_le_bytes());
                }
                PixelType::Uint16 => {
                    let v: u16 = tok.parse().unwrap_or(0);
                    dst.copy_from_slice(&v.to_le_bytes());
                }
                PixelType::Int16 => {
                    let v: i16 = tok.parse().unwrap_or(0);
                    dst.copy_from_slice(&v.to_le_bytes());
                }
                _ => {
                    let v: i64 = tok.parse().unwrap_or(0);
                    dst[0] = v as u8;
                }
            }
        }
        Ok(out)
    }
}
impl Default for AmiraReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for AmiraReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("am") | Some("amiramesh"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        let s = std::str::from_utf8(&header[..header.len().min(32)]).unwrap_or("");
        s.starts_with("# AmiraMesh") || s.starts_with("# Avizo")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let hdr = parse_amira_header(path)?;
        let image_count = hdr.nz;
        // ASCII-decoded planes are emitted as little-endian byte buffers.
        let little_endian = if hdr.ascii { true } else { hdr.little_endian };
        self.meta = Some(ImageMetadata {
            size_x: hdr.nx,
            size_y: hdr.ny,
            size_z: hdr.nz,
            size_c: 1,
            size_t: 1,
            pixel_type: hdr.pixel_type,
            bits_per_pixel: (hdr.pixel_type.bytes_per_sample() * 8) as u8,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: little_endian,
            resolution_count: 1,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.data_offset = hdr.data_offset;
        self.ascii = hdr.ascii;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if self.ascii {
            return self.read_ascii_plane(plane_index);
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
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
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
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

// ─── Spider EM ────────────────────────────────────────────────────────────────
//
// Spider files store all data as float32. The header is also float32 values.
// Key word offsets (word N = byte offset (N-1)*4):
//   Word 1 (off  0): NSLICE — number of slices (z-planes)
//   Word 2 (off  4): NROW   — rows (height)
//   Word 5 (off 16): IFORM  — file type: 1=2D, 3=3D, 11=2D sequence
//   Word 12 (off 44): NSAM   — columns (width)
//   Word 13 (off 48): LABREC — records in header
//   Word 22 (off 84): LABBYT — total header bytes

fn r_f32_le_w(b: &[u8], off: usize) -> f32 {
    f32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn parse_spider_header(path: &Path) -> Result<(u32, u32, u32, u64)> {
    let mut f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut hdr = [0u8; 256]; // read first 256 bytes = enough for the key fields
    f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

    let nslice = r_f32_le_w(&hdr, 0).abs() as u32;
    let nrow = r_f32_le_w(&hdr, 4) as u32;
    let iform = r_f32_le_w(&hdr, 16) as i32;
    let nsam = r_f32_le_w(&hdr, 44) as u32;
    let labbyt = r_f32_le_w(&hdr, 84) as u64;

    let width = nsam.max(1);
    let height = nrow.max(1);
    let nz = match iform {
        1 => 1,              // single 2D image
        3 => nslice.max(1),  // 3D volume
        11 => nslice.max(1), // sequence of 2D images
        _ => nslice.max(1),
    };

    let header_size = if labbyt > 0 {
        labbyt
    } else {
        // Estimate: LABREC * NSAM * 4
        let labrec = r_f32_le_w(&hdr, 48) as u64;
        labrec * nsam as u64 * 4
    };

    if width == 0 || height == 0 {
        return Err(BioFormatsError::Format(
            "Spider: invalid image dimensions".into(),
        ));
    }

    Ok((width, height, nz.max(1), header_size))
}

pub struct SpiderReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl SpiderReader {
    pub fn new() -> Self {
        SpiderReader {
            path: None,
            meta: None,
            data_offset: 0,
        }
    }
}
impl Default for SpiderReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SpiderReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("spi") | Some("xmp"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 52 {
            return false;
        }
        // Spider header: check NSLICE (word 1) and NSAM (word 12) are non-zero float32s
        // and IFORM (word 5) is a valid type code
        let iform = r_f32_le_w(header, 16) as i32;
        let nsam = r_f32_le_w(header, 44);
        let nrow = r_f32_le_w(header, 4);
        matches!(iform, 1 | 3 | -1 | -3 | 11 | -11 | -21 | -22) && nsam > 0.0 && nrow > 0.0
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (width, height, nz, data_offset) = parse_spider_header(path)?;
        let image_count = nz;
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: nz,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Float32,
            bits_per_pixel: 32,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: {
                let mut m = HashMap::new();
                m.insert("format".into(), MetadataValue::String("Spider EM".into()));
                m
            },
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.data_offset = data_offset;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let plane_bytes = (meta.size_x * meta.size_y) as usize * 4;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
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
        let meta = self.meta.as_ref().unwrap();
        let bps = 4usize;
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
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
