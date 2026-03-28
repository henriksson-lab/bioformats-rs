//! NRRD (Nearly Raw Raster Data) reader and writer.
//!
//! Specification: http://teem.sourceforge.net/nrrd/format.html
//! Supports inline (`.nrrd`) and detached (`.nhdr` + data file) formats.
//! Encoding: raw, gzip. (bzip2 omitted to avoid C deps.)

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::writer::FormatWriter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding { Raw, Gzip, Ascii }

#[derive(Debug)]
struct NrrdHeader {
    pixel_type: PixelType,
    dimension: usize,
    sizes: Vec<u32>,
    endian: bool,   // true = little-endian
    encoding: Encoding,
    data_file: Option<PathBuf>,
    data_offset: u64,
    extra: HashMap<String, String>,
}

fn nrrd_pixel_type(t: &str) -> PixelType {
    match t {
        "int8" | "signed char" => PixelType::Int8,
        "uint8" | "uchar" | "unsigned char" => PixelType::Uint8,
        "int16" | "short" | "signed short" | "short int" | "signed short int" => PixelType::Int16,
        "uint16" | "ushort" | "unsigned short" | "unsigned short int" => PixelType::Uint16,
        "int32" | "int" | "signed int" => PixelType::Int32,
        "uint32" | "uint" | "unsigned int" => PixelType::Uint32,
        "float" => PixelType::Float32,
        "double" => PixelType::Float64,
        _ => PixelType::Uint8,
    }
}

fn parse_nrrd_header(path: &Path) -> Result<NrrdHeader> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut reader = BufReader::new(f);

    // First line must be "NRRD00XX"
    let mut first_line = String::new();
    reader.read_line(&mut first_line).map_err(BioFormatsError::Io)?;
    if !first_line.trim_start().starts_with("NRRD") {
        return Err(BioFormatsError::Format("Not a NRRD file".into()));
    }

    let mut pixel_type = PixelType::Uint8;
    let mut dimension = 0usize;
    let mut sizes: Vec<u32> = Vec::new();
    let mut little_endian = true;
    let mut encoding = Encoding::Raw;
    let mut data_file: Option<PathBuf> = None;
    let mut data_offset = 0u64;
    let mut extra: HashMap<String, String> = HashMap::new();

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(BioFormatsError::Io)?;
        if n == 0 { break; }

        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');

        // Blank line = start of inline data
        if trimmed.is_empty() {
            data_offset = reader.stream_position().map_err(BioFormatsError::Io)?;
            break;
        }

        // Skip comments
        if trimmed.starts_with('#') { continue; }

        // Parse "key: value" or "key:=value"
        let sep_pos = trimmed.find(':');
        if let Some(sep) = sep_pos {
            let key = trimmed[..sep].trim().to_ascii_lowercase();
            let val = trimmed[sep + 1..].trim_start_matches(|c| c == '=' || c == ' ');
            let val = val.trim();

            match key.as_str() {
                "type" => pixel_type = nrrd_pixel_type(val),
                "dimension" => dimension = val.parse().unwrap_or(0),
                "sizes" => {
                    sizes = val.split_ascii_whitespace()
                        .filter_map(|s| s.parse().ok())
                        .collect();
                }
                "endian" => {
                    little_endian = val.eq_ignore_ascii_case("little");
                }
                "encoding" => {
                    encoding = match val.to_ascii_lowercase().as_str() {
                        "gzip" | "gz" => Encoding::Gzip,
                        "ascii" | "text" | "txt" => Encoding::Ascii,
                        _ => Encoding::Raw,
                    };
                }
                "data file" | "datafile" => {
                    if !val.eq_ignore_ascii_case("LIST") {
                        // Resolve relative to the .nhdr file
                        let parent = path.parent().unwrap_or(Path::new("."));
                        data_file = Some(parent.join(val));
                    }
                }
                _ => { extra.insert(key, val.to_string()); }
            }
        }
    }

    Ok(NrrdHeader { pixel_type, dimension, sizes, endian: little_endian, encoding, data_file, data_offset, extra })
}

// ---- reader -----------------------------------------------------------------

pub struct NrrdReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    header: Option<NrrdHeader>,
}

impl NrrdReader {
    pub fn new() -> Self { NrrdReader { path: None, meta: None, header: None } }

    fn read_plane_data(&self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let hdr = self.header.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let ics_path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * meta.size_c as usize * bps;
        let plane_offset = plane_index as u64 * plane_bytes as u64;

        let data_path = hdr.data_file.as_ref().map(|p| p.clone()).unwrap_or_else(|| ics_path.clone());
        let data_start = hdr.data_offset;

        let mut f = File::open(&data_path).map_err(BioFormatsError::Io)?;

        match hdr.encoding {
            Encoding::Raw => {
                f.seek(SeekFrom::Start(data_start + plane_offset)).map_err(BioFormatsError::Io)?;
                let mut buf = vec![0u8; plane_bytes];
                f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
                // Byte-swap if needed
                if !hdr.endian && bps > 1 {
                    for chunk in buf.chunks_exact_mut(bps) { chunk.reverse(); }
                }
                Ok(buf)
            }
            Encoding::Gzip => {
                f.seek(SeekFrom::Start(data_start)).map_err(BioFormatsError::Io)?;
                let mut dec = flate2::read::GzDecoder::new(f);
                let mut all = Vec::new();
                dec.read_to_end(&mut all).map_err(BioFormatsError::Io)?;
                let start = plane_offset as usize;
                let end = start + plane_bytes;
                if end > all.len() {
                    return Err(BioFormatsError::InvalidData("NRRD: plane out of range".into()));
                }
                let mut buf = all[start..end].to_vec();
                if !hdr.endian && bps > 1 {
                    for chunk in buf.chunks_exact_mut(bps) { chunk.reverse(); }
                }
                Ok(buf)
            }
            Encoding::Ascii => {
                // Parse whitespace-separated numbers
                f.seek(SeekFrom::Start(data_start)).map_err(BioFormatsError::Io)?;
                let mut text = String::new();
                f.read_to_string(&mut text).map_err(BioFormatsError::Io)?;
                let total_samples = meta.size_x as usize * meta.size_y as usize
                    * meta.size_c as usize * meta.image_count as usize;
                let offset_samples = plane_index as usize
                    * meta.size_x as usize * meta.size_y as usize * meta.size_c as usize;
                let plane_samples = plane_bytes / bps.max(1);
                let mut buf = vec![0u8; plane_bytes];
                for (i, token) in text.split_ascii_whitespace()
                    .skip(offset_samples)
                    .take(plane_samples)
                    .enumerate()
                {
                    let dst = i * bps;
                    match meta.pixel_type {
                        PixelType::Uint8 | PixelType::Int8 => {
                            if let Ok(v) = token.parse::<u8>() { buf[dst] = v; }
                        }
                        PixelType::Uint16 | PixelType::Int16 => {
                            if let Ok(v) = token.parse::<u16>() {
                                buf[dst..dst+2].copy_from_slice(&v.to_le_bytes());
                            }
                        }
                        PixelType::Float32 => {
                            if let Ok(v) = token.parse::<f32>() {
                                buf[dst..dst+4].copy_from_slice(&v.to_le_bytes());
                            }
                        }
                        _ => {}
                    }
                }
                let _ = total_samples;
                Ok(buf)
            }
        }
    }
}

impl Default for NrrdReader { fn default() -> Self { Self::new() } }

impl FormatReader for NrrdReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "nrrd" | "nhdr"))
            .unwrap_or(false)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"NRRD")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let hdr = parse_nrrd_header(path)?;

        let (size_x, size_y, size_z, size_c) = match hdr.sizes.as_slice() {
            [x] => (*x, 1, 1, 1),
            [x, y] => (*x, *y, 1, 1),
            [x, y, z] => (*x, *y, *z, 1),
            [x, y, z, c, ..] => (*x, *y, *z, *c),
            [] => (1, 1, 1, 1),
        };
        let image_count = size_z;

        let mut series_metadata: HashMap<String, MetadataValue> = hdr.extra.iter()
            .map(|(k, v)| (k.clone(), MetadataValue::String(v.clone())))
            .collect();
        series_metadata.insert("nrrd_dimension".into(), MetadataValue::Int(hdr.dimension as i64));

        let bps = (hdr.pixel_type.bytes_per_sample() * 8) as u8;
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t: 1,
            pixel_type: hdr.pixel_type,
            bits_per_pixel: bps,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: size_c == 3,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: hdr.endian,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.header = Some(hdr);
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> { self.path = None; self.meta = None; self.header = None; Ok(()) }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        self.read_plane_data(plane_index)
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

// ---- writer -----------------------------------------------------------------

pub struct NrrdWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
}

impl NrrdWriter {
    pub fn new() -> Self { NrrdWriter { path: None, meta: None, planes: Vec::new() } }
}

impl Default for NrrdWriter { fn default() -> Self { Self::new() } }

fn nrrd_type_str(pt: PixelType) -> &'static str {
    match pt {
        PixelType::Int8 => "int8",
        PixelType::Uint8 | PixelType::Bit => "uint8",
        PixelType::Int16 => "int16",
        PixelType::Uint16 => "uint16",
        PixelType::Int32 => "int32",
        PixelType::Uint32 => "uint32",
        PixelType::Float32 => "float",
        PixelType::Float64 => "double",
    }
}

impl FormatWriter for NrrdWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "nrrd" | "nhdr"))
            .unwrap_or(false)
    }
    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        self.meta = Some(meta.clone()); Ok(())
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta.as_ref().ok_or_else(|| BioFormatsError::Format("set_metadata first".into()))?;
        self.path = Some(path.to_path_buf());
        self.planes.clear();
        Ok(())
    }
    fn save_bytes(&mut self, _: u32, data: &[u8]) -> Result<()> {
        self.planes.push(data.to_vec()); Ok(())
    }
    fn close(&mut self) -> Result<()> {
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.take().ok_or(BioFormatsError::NotInitialized)?;
        let f = File::create(&path).map_err(BioFormatsError::Io)?;
        let mut w = std::io::BufWriter::new(f);

        let nz = self.planes.len();
        let bps = meta.pixel_type.bytes_per_sample();

        writeln!(w, "NRRD0004").map_err(BioFormatsError::Io)?;
        writeln!(w, "type: {}", nrrd_type_str(meta.pixel_type)).map_err(BioFormatsError::Io)?;
        let dim = if nz > 1 { 3 } else { 2 };
        writeln!(w, "dimension: {}", dim).map_err(BioFormatsError::Io)?;
        if nz > 1 {
            writeln!(w, "sizes: {} {} {}", meta.size_x, meta.size_y, nz).map_err(BioFormatsError::Io)?;
        } else {
            writeln!(w, "sizes: {} {}", meta.size_x, meta.size_y).map_err(BioFormatsError::Io)?;
        }
        if bps > 1 {
            writeln!(w, "endian: little").map_err(BioFormatsError::Io)?;
        }
        writeln!(w, "encoding: raw").map_err(BioFormatsError::Io)?;
        writeln!(w).map_err(BioFormatsError::Io)?; // blank line → inline data

        for plane in &self.planes {
            w.write_all(plane).map_err(BioFormatsError::Io)?;
        }
        w.flush().map_err(BioFormatsError::Io)?;
        self.planes.clear();
        Ok(())
    }
    fn can_do_stacks(&self) -> bool { true }
}
