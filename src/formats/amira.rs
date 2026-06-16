//! Amira Mesh (.am / .amiramesh) and Spider EM (.spi / .xmp) format readers.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::{crop_full_plane, validate_region};

// ─── Amira Mesh ───────────────────────────────────────────────────────────────

/// Per-stream compression of an AmiraMesh lattice (port of AmiraReader's
/// `compression` token from the `@N(...)` stream descriptor).
#[derive(Clone, Copy, PartialEq, Eq)]
enum AmiraCompression {
    /// No compression — raw binary stream.
    None,
    /// `HxZip,<size>`: a single zlib/deflate stream for the whole stack.
    HxZip,
    /// `HxByteRLE,<size>`: byte run-length encoding for the whole stack.
    HxByteRLE,
}

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
    /// Per-stream compression of the `@N` data stream.
    compression: AmiraCompression,
    /// Bounding box (x0 x1 y0 y1 z0 z1), used to derive physical pixel sizes.
    bounding_box: Option<[f64; 6]>,
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
    let mut compression = AmiraCompression::None;
    let mut bounding_box: Option<[f64; 6]> = None;

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
                nx = parts[2].parse().map_err(|_| {
                    BioFormatsError::Format(format!(
                        "Amira Mesh: invalid lattice width {:?}",
                        parts[2]
                    ))
                })?;
                ny = parts[3].parse().map_err(|_| {
                    BioFormatsError::Format(format!(
                        "Amira Mesh: invalid lattice height {:?}",
                        parts[3]
                    ))
                })?;
                nz = parts[4].parse().map_err(|_| {
                    BioFormatsError::Format(format!(
                        "Amira Mesh: invalid lattice depth {:?}",
                        parts[4]
                    ))
                })?;
            } else if parts.len() >= 4 {
                nx = parts[2].parse().map_err(|_| {
                    BioFormatsError::Format(format!(
                        "Amira Mesh: invalid lattice width {:?}",
                        parts[2]
                    ))
                })?;
                ny = parts[3].parse().map_err(|_| {
                    BioFormatsError::Format(format!(
                        "Amira Mesh: invalid lattice height {:?}",
                        parts[3]
                    ))
                })?;
                nz = 1;
            }
        }

        // "BoundingBox x0 x1 y0 y1 z0 z1" gives the voxel-centre extents.
        if t.starts_with("BoundingBox") {
            let vals: Vec<f64> = t
                .split_ascii_whitespace()
                .skip(1)
                .filter_map(|s| s.parse::<f64>().ok())
                .collect();
            if vals.len() >= 6 {
                bounding_box = Some([vals[0], vals[1], vals[2], vals[3], vals[4], vals[5]]);
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
            } else if lo.contains("byte") {
                PixelType::Uint8
            } else {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Amira Mesh: unsupported lattice data type in {t:?}"
                )));
            };
            // Extract @N section number and optional compression token. The
            // stream descriptor looks like "... @1" or "... @1(HxByteRLE,12345)"
            // (the parenthesised string is the AmiraReader `compression` token).
            if let Some(at_pos) = t.rfind('@') {
                let rest = t[at_pos + 1..].trim();
                // Split off any "(...)" compression descriptor.
                let (num_part, comp_part) = match rest.find('(') {
                    Some(p) => (rest[..p].trim(), Some(&rest[p + 1..])),
                    None => (rest, None),
                };
                if let Ok(n) = num_part.parse::<u32>() {
                    data_section = n;
                }
                if let Some(comp) = comp_part {
                    // Strip the trailing ')' if present.
                    let comp = comp.trim_end_matches(')').trim();
                    if comp.starts_with("HxZip,") {
                        compression = AmiraCompression::HxZip;
                    } else if comp.starts_with("HxByteRLE,") {
                        compression = AmiraCompression::HxByteRLE;
                    } else if !comp.is_empty() {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "Amira Mesh: unsupported stream compression {comp:?}"
                        )));
                    }
                }
            }
        }

        // Find @N marker in body — data starts on the next line
        if t == format!("@{}", data_section) {
            let data_offset = reader.stream_position().map_err(BioFormatsError::Io)?;
            validate_positive_dims("Amira Mesh", nx, ny, nz)?;
            return Ok(AmiraHeader {
                nx,
                ny,
                nz,
                pixel_type,
                data_offset,
                little_endian,
                ascii,
                compression,
                bounding_box,
            });
        }
    }

    Err(BioFormatsError::Format(
        "Amira Mesh: could not find data section".into(),
    ))
}

fn validate_positive_dims(format: &str, width: u32, height: u32, depth: u32) -> Result<()> {
    if width == 0 || height == 0 || depth == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format}: invalid non-positive dimensions {width}x{height}x{depth}"
        )));
    }
    Ok(())
}

fn checked_plane_bytes(format: &str, meta: &ImageMetadata) -> Result<u64> {
    (meta.size_x as u64)
        .checked_mul(meta.size_y as u64)
        .and_then(|pixels| pixels.checked_mul(meta.pixel_type.bytes_per_sample() as u64))
        .ok_or_else(|| BioFormatsError::Format(format!("{format}: plane size overflows")))
}

fn validate_payload_len(
    format: &str,
    path: &Path,
    data_offset: u64,
    meta: &ImageMetadata,
) -> Result<()> {
    let file_len = std::fs::metadata(path).map_err(BioFormatsError::Io)?.len();
    let required_len = data_offset
        .checked_add(
            checked_plane_bytes(format, meta)?
                .checked_mul(meta.image_count as u64)
                .ok_or_else(|| {
                    BioFormatsError::Format(format!("{format}: payload size overflows"))
                })?,
        )
        .ok_or_else(|| BioFormatsError::Format(format!("{format}: payload size overflows")))?;
    if file_len < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{format}: pixel payload is shorter than declared ({file_len} < {required_len})"
        )));
    }
    Ok(())
}

pub struct AmiraReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
    ascii: bool,
    compression: AmiraCompression,
    /// Bounding box (x0 x1 y0 y1 z0 z1) for deriving physical pixel sizes.
    bounding_box: Option<[f64; 6]>,
    /// Lazily decoded full stack for compressed streams (HxZip/HxByteRLE).
    /// The whole stack is one compressed stream, so we decode once and slice.
    decoded_stack: Option<Vec<u8>>,
}

impl AmiraReader {
    pub fn new() -> Self {
        AmiraReader {
            path: None,
            meta: None,
            data_offset: 0,
            ascii: false,
            compression: AmiraCompression::None,
            bounding_box: None,
            decoded_stack: None,
        }
    }

    /// Decode an HxByteRLE stream. Port of `AmiraReader.HxRLE.read`.
    ///
    /// A control byte `insn` is read: when its high bit is set (negative as a
    /// signed byte), `insn & 0x7f` literal bytes follow and are copied verbatim;
    /// otherwise `insn` is a run length and the single following byte is
    /// repeated that many times. Decoding stops once `expected` bytes have been
    /// produced (the whole stack is one stream).
    fn decode_byte_rle(data: &[u8], expected: usize) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(expected);
        let mut i = 0;
        while out.len() < expected && i < data.len() {
            let insn = data[i] as i8;
            i += 1;
            if insn < 0 {
                // Literal run of (insn & 0x7f) bytes.
                let count = (insn as u8 & 0x7f) as usize;
                if i + count > data.len() {
                    return Err(BioFormatsError::InvalidData(
                        "Amira HxByteRLE: literal run overruns input".into(),
                    ));
                }
                out.extend_from_slice(&data[i..i + count]);
                i += count;
            } else {
                // Fill run of `insn` copies of the next byte.
                let count = insn as usize;
                if i >= data.len() {
                    return Err(BioFormatsError::InvalidData(
                        "Amira HxByteRLE: fill run missing byte".into(),
                    ));
                }
                let byte = data[i];
                i += 1;
                out.resize(out.len() + count, byte);
            }
        }
        if out.len() < expected {
            return Err(BioFormatsError::InvalidData(format!(
                "Amira HxByteRLE: decoded {} bytes, expected {expected}",
                out.len()
            )));
        }
        out.truncate(expected);
        Ok(out)
    }

    /// Decode the whole compressed stack into raw little/big-endian bytes.
    fn decode_stack(&self) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let expected = (checked_plane_bytes("Amira Mesh", meta)?
            .checked_mul(meta.image_count as u64)
            .ok_or_else(|| BioFormatsError::Format("Amira Mesh: payload size overflows".into()))?)
            as usize;

        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset))
            .map_err(BioFormatsError::Io)?;
        let mut compressed = Vec::new();
        f.read_to_end(&mut compressed)
            .map_err(BioFormatsError::Io)?;

        let decoded = match self.compression {
            AmiraCompression::HxZip => crate::common::codec::decompress_deflate(&compressed)?,
            AmiraCompression::HxByteRLE => Self::decode_byte_rle(&compressed, expected)?,
            AmiraCompression::None => compressed,
        };
        if decoded.len() < expected {
            return Err(BioFormatsError::InvalidData(format!(
                "Amira Mesh: decompressed stack is shorter than declared ({} < {expected})",
                decoded.len()
            )));
        }
        Ok(decoded)
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
        reader
            .read_to_string(&mut text)
            .map_err(BioFormatsError::Io)?;
        let skip = plane_index as usize * count;

        let tokens: Vec<&str> = text
            .split_ascii_whitespace()
            .skip(skip)
            .take(count)
            .collect();
        if tokens.len() != count {
            return Err(BioFormatsError::InvalidData(format!(
                "Amira ASCII plane {plane_index} has {} samples, expected {count}",
                tokens.len()
            )));
        }

        let mut out = vec![0u8; count * bps];
        for (i, tok) in tokens.into_iter().enumerate() {
            let dst = &mut out[i * bps..(i + 1) * bps];
            match pixel_type {
                PixelType::Float32 => {
                    let v: f32 = tok.parse().map_err(|_| {
                        BioFormatsError::InvalidData(format!(
                            "Amira ASCII plane {plane_index} contains non-Float32 sample {tok:?}"
                        ))
                    })?;
                    dst.copy_from_slice(&v.to_le_bytes());
                }
                PixelType::Float64 => {
                    let v: f64 = tok.parse().map_err(|_| {
                        BioFormatsError::InvalidData(format!(
                            "Amira ASCII plane {plane_index} contains non-Float64 sample {tok:?}"
                        ))
                    })?;
                    dst.copy_from_slice(&v.to_le_bytes());
                }
                PixelType::Int32 => {
                    let v: i32 = tok.parse().map_err(|_| {
                        BioFormatsError::InvalidData(format!(
                            "Amira ASCII plane {plane_index} contains non-Int32 sample {tok:?}"
                        ))
                    })?;
                    dst.copy_from_slice(&v.to_le_bytes());
                }
                PixelType::Uint16 => {
                    let v: u16 = tok.parse().map_err(|_| {
                        BioFormatsError::InvalidData(format!(
                            "Amira ASCII plane {plane_index} contains non-Uint16 sample {tok:?}"
                        ))
                    })?;
                    dst.copy_from_slice(&v.to_le_bytes());
                }
                PixelType::Int16 => {
                    let v: i16 = tok.parse().map_err(|_| {
                        BioFormatsError::InvalidData(format!(
                            "Amira ASCII plane {plane_index} contains non-Int16 sample {tok:?}"
                        ))
                    })?;
                    dst.copy_from_slice(&v.to_le_bytes());
                }
                _ => {
                    let v: i64 = tok.parse().map_err(|_| {
                        BioFormatsError::InvalidData(format!(
                            "Amira ASCII plane {plane_index} contains non-integer sample {tok:?}"
                        ))
                    })?;
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
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
        self.ascii = false;
        self.compression = AmiraCompression::None;
        self.bounding_box = None;
        self.decoded_stack = None;
        let hdr = parse_amira_header(path)?;
        let image_count = hdr.nz;
        // ASCII-decoded planes are emitted as little-endian byte buffers.
        let little_endian = if hdr.ascii { true } else { hdr.little_endian };
        let meta = ImageMetadata {
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
            thumbnail: false,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };
        // For raw binary streams we can validate the on-disk payload length.
        // Compressed (HxZip/HxByteRLE) streams are shorter than the decoded
        // payload, so length validation happens after decompression instead.
        if !hdr.ascii && hdr.compression == AmiraCompression::None {
            validate_payload_len("Amira Mesh", path, hdr.data_offset, &meta)?;
        }
        self.meta = Some(meta);
        self.data_offset = hdr.data_offset;
        self.ascii = hdr.ascii;
        self.compression = hdr.compression;
        self.bounding_box = hdr.bounding_box;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.decoded_stack = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() || s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }
    fn series(&self) -> usize {
        0
    }
    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if self.ascii {
            return self.read_ascii_plane(plane_index);
        }
        let plane_bytes = checked_plane_bytes("Amira Mesh", meta)? as usize;

        if self.compression != AmiraCompression::None {
            // The whole stack is one compressed stream: decode once, then slice
            // out the requested plane.
            if self.decoded_stack.is_none() {
                self.decoded_stack = Some(self.decode_stack()?);
            }
            let stack = self.decoded_stack.as_ref().unwrap();
            let start = plane_index as usize * plane_bytes;
            let end = start + plane_bytes;
            if end > stack.len() {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index));
            }
            return Ok(stack[start..end].to_vec());
        }

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
        {
            let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            validate_region("Amira", meta.size_x, meta.size_y, x, y, w, h)?;
        }
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Amira", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        let img = ome.images.get_mut(0)?;

        // AmiraReader does not call setImageName, so Java falls back to the
        // current file's basename (with extension).
        if let Some(path) = self.path.as_ref() {
            img.name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        }

        // Physical pixel size from the bounding box (voxel-centre extents):
        //   pixelSize = (high - low) / (count - 1)   (Java AmiraReader).
        if let Some(bb) = self.bounding_box {
            let span = |hi: f64, lo: f64, n: u32| {
                if n > 1 {
                    Some((hi - lo) / (n as f64 - 1.0))
                } else {
                    None
                }
            };
            img.physical_size_x = span(bb[1], bb[0], meta.size_x);
            img.physical_size_y = span(bb[3], bb[2], meta.size_y);
            img.physical_size_z = span(bb[5], bb[4], meta.size_z);
        }

        Some(ome)
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

// Mirrors the addGlobalMeta(...) block of Java SpiderReader.initFile: reads the
// named header words (4-byte little-endian floats) plus the trailing date/time/
// title strings and stores them under Java's exact key names. Header layout in
// words (word N = byte offset (N-1)*4); some words Java skips are omitted here.
fn read_spider_metadata(path: &Path) -> Result<HashMap<String, MetadataValue>> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    // Read up to 1024 bytes (through the 160-byte title at byte 864), zero-
    // padding short files so payload validation — not this read — reports any
    // truncation. read_to_end may exceed 1024; truncate/pad to a fixed size.
    let mut hdr = Vec::new();
    f.take(1024).read_to_end(&mut hdr).map_err(BioFormatsError::Io)?;
    hdr.resize(1024, 0);

    let int_w = |off: usize| MetadataValue::Int(r_f32_le_w(&hdr, off) as i32 as i64);
    let float_w = |off: usize| MetadataValue::Float(r_f32_le_w(&hdr, off) as f64);
    let str_at = |off: usize, len: usize| {
        MetadataValue::String(
            String::from_utf8_lossy(&hdr[off..off + len])
                .trim_end_matches('\0')
                .trim()
                .to_string(),
        )
    };

    let mut m = HashMap::new();
    m.insert("NSLICE".into(), int_w(0));
    m.insert("NROW".into(), int_w(4));
    m.insert("IREC".into(), int_w(8));
    m.insert("IFORM".into(), int_w(16));
    m.insert("IMAMI".into(), int_w(20));
    m.insert("FMAX".into(), float_w(24));
    m.insert("FMIN".into(), float_w(28));
    m.insert("AV".into(), float_w(32));
    m.insert("SIG".into(), float_w(36));
    m.insert("NSAM".into(), int_w(44));
    m.insert("LABREC".into(), int_w(48));
    m.insert("IANGLE".into(), int_w(52));
    m.insert("PHI".into(), float_w(56));
    m.insert("THETA".into(), float_w(60));
    m.insert("GAMMA".into(), float_w(64));
    m.insert("XOFF".into(), float_w(68));
    m.insert("YOFF".into(), float_w(72));
    m.insert("ZOFF".into(), float_w(76));
    m.insert("SCALE".into(), float_w(80));
    m.insert("LABBYT".into(), int_w(84));
    m.insert("LENBYT".into(), int_w(88));
    m.insert("ISTACK/MAXINDX".into(), int_w(92));
    m.insert("MAXIM".into(), float_w(100));
    m.insert("IMGNUM".into(), float_w(104));
    m.insert("LASTINDX".into(), float_w(108));
    m.insert("KANGLE".into(), float_w(120));
    m.insert("PHI1".into(), float_w(124));
    m.insert("THETA1".into(), float_w(128));
    m.insert("PSI1".into(), float_w(132));
    m.insert("PHI2".into(), float_w(136));
    m.insert("THETA2".into(), float_w(140));
    m.insert("PSI2".into(), float_w(144));
    m.insert("PIXSIZ".into(), float_w(148));
    m.insert("EV".into(), float_w(152));
    m.insert("PHI3".into(), float_w(408));
    m.insert("THETA3".into(), float_w(404));
    m.insert("PSI3".into(), float_w(400));
    m.insert("LANGLE".into(), float_w(412));
    m.insert("CDAT".into(), str_at(844, 12));
    m.insert("CTIM".into(), str_at(856, 8));
    m.insert("CTIT".into(), str_at(864, 160));
    Ok(m)
}

fn parse_spider_header(path: &Path) -> Result<(u32, u32, u32, u64)> {
    let mut f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut hdr = [0u8; 256]; // read first 256 bytes = enough for the key fields
    f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

    let nslice = spider_positive_u32(r_f32_le_w(&hdr, 0), "NSLICE")?;
    let nrow = spider_positive_u32(r_f32_le_w(&hdr, 4), "NROW")?;
    let iform = r_f32_le_w(&hdr, 16) as i32;
    let nsam = spider_positive_u32(r_f32_le_w(&hdr, 44), "NSAM")?;
    let labbyt = r_f32_le_w(&hdr, 84) as u64;

    let width = nsam;
    let height = nrow;
    let nz = match iform {
        1 | -1 => 1,                    // single 2D image
        3 | -3 => nslice,               // 3D volume
        11 | -11 | -21 | -22 => nslice, // sequence / known Spider variants
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Spider: unsupported IFORM {iform}"
            )))
        }
    };

    let header_size = if labbyt > 0 {
        labbyt
    } else {
        // Estimate: LABREC * NSAM * 4
        let labrec = r_f32_le_w(&hdr, 48) as u64;
        labrec * nsam as u64 * 4
    };

    Ok((width, height, nz, header_size))
}

fn spider_positive_u32(value: f32, label: &str) -> Result<u32> {
    if !value.is_finite() || value <= 0.0 || value.fract() != 0.0 || value > u32::MAX as f32 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Spider: invalid {label} dimension {value}"
        )));
    }
    Ok(value as u32)
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
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
        let (width, height, nz, data_offset) = parse_spider_header(path)?;
        let image_count = nz;
        let meta = ImageMetadata {
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
            thumbnail: false,
            series_metadata: {
                let mut m = read_spider_metadata(path)?;
                m.insert("format".into(), MetadataValue::String("Spider EM".into()));
                m
            },
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };
        validate_payload_len("Spider", path, data_offset, &meta)?;
        self.meta = Some(meta);
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
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() || s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }
    fn series(&self) -> usize {
        0
    }
    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let plane_bytes = checked_plane_bytes("Spider", meta)? as usize;
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
        {
            let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            validate_region("Spider", meta.size_x, meta.size_y, x, y, w, h)?;
        }
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Spider", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

#[cfg(test)]
mod spider_tests {
    use super::*;
    use std::io::Write;

    fn put_f32(buf: &mut [u8], word: usize, v: f32) {
        let off = word * 4;
        buf[off..off + 4].copy_from_slice(&v.to_le_bytes());
    }

    #[test]
    fn spider_header_metadata_keys() {
        // Build a minimal 1024-byte little-endian Spider header (a single 2x2
        // 2D image, IFORM=1) plus the float32 pixel payload.
        let mut hdr = vec![0u8; 1024];
        put_f32(&mut hdr, 0, 1.0); // NSLICE (word 1)
        put_f32(&mut hdr, 1, 2.0); // NROW
        put_f32(&mut hdr, 4, 1.0); // IFORM (word 5)
        put_f32(&mut hdr, 6, 9.5); // FMAX (word 7)
        put_f32(&mut hdr, 7, -3.25); // FMIN (word 8)
        put_f32(&mut hdr, 11, 2.0); // NSAM (word 12)
        put_f32(&mut hdr, 13, 0.0); // LABREC (word 14) -> labbyt drives header size
        put_f32(&mut hdr, 21, 1024.0); // LABBYT (word 22) header size in bytes

        let path = std::env::temp_dir().join(format!(
            "spider_meta_{}_{}.spi",
            std::process::id(),
            line!()
        ));
        {
            let mut f = File::create(&path).unwrap();
            f.write_all(&hdr).unwrap();
            f.write_all(&[0u8; 2 * 2 * 4]).unwrap(); // 2x2 float32 plane
            f.flush().unwrap();
        }

        let mut r = SpiderReader::new();
        r.set_id(&path).unwrap();
        let md = &r.metadata().series_metadata;

        let int_of = |k: &str| match md.get(k) {
            Some(MetadataValue::Int(v)) => *v,
            other => panic!("{k} not Int: {other:?}"),
        };
        let float_of = |k: &str| match md.get(k) {
            Some(MetadataValue::Float(v)) => *v,
            other => panic!("{k} not Float: {other:?}"),
        };

        assert_eq!(int_of("IFORM"), 1);
        assert_eq!(int_of("NSLICE"), 1);
        assert_eq!(float_of("FMAX"), 9.5);
        assert_eq!(float_of("FMIN"), -3.25);
        assert_eq!(int_of("NSAM"), 2);
        assert!(matches!(md.get("CTIT"), Some(MetadataValue::String(_))));

        std::fs::remove_file(&path).ok();
    }
}
