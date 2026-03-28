//! MetaImage MHA/MHD reader and writer (ITK/VTK format).
//!
//! `.mha` = inline (header + data in same file)
//! `.mhd` = detached header; data in a separate `.raw` (or `.zraw`) file

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::writer::FormatWriter;

fn meta_pixel_type(s: &str) -> PixelType {
    match s {
        "MET_CHAR" => PixelType::Int8,
        "MET_UCHAR" => PixelType::Uint8,
        "MET_SHORT" => PixelType::Int16,
        "MET_USHORT" => PixelType::Uint16,
        "MET_INT" => PixelType::Int32,
        "MET_UINT" => PixelType::Uint32,
        "MET_FLOAT" => PixelType::Float32,
        "MET_DOUBLE" => PixelType::Float64,
        _ => PixelType::Uint8,
    }
}

fn meta_type_str(pt: PixelType) -> &'static str {
    match pt {
        PixelType::Int8 => "MET_CHAR",
        PixelType::Uint8 | PixelType::Bit => "MET_UCHAR",
        PixelType::Int16 => "MET_SHORT",
        PixelType::Uint16 => "MET_USHORT",
        PixelType::Int32 => "MET_INT",
        PixelType::Uint32 => "MET_UINT",
        PixelType::Float32 => "MET_FLOAT",
        PixelType::Float64 => "MET_DOUBLE",
    }
}

struct MhdHeader {
    ndims: usize,
    sizes: Vec<u32>,
    pixel_type: PixelType,
    little_endian: bool,
    compressed: bool,
    data_file: Option<String>, // "LOCAL" or path
    data_offset: u64,
    extra: HashMap<String, String>,
}

fn parse_mhd(path: &Path) -> Result<MhdHeader> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut reader = BufReader::new(f);

    let mut ndims = 3usize;
    let mut sizes: Vec<u32> = Vec::new();
    let mut pixel_type = PixelType::Uint8;
    let mut little_endian = true;
    let mut compressed = false;
    let mut data_file: Option<String> = None;
    let mut data_offset = 0u64;
    let mut extra: HashMap<String, String> = HashMap::new();

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(BioFormatsError::Io)?;
        if n == 0 { break; }

        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if trimmed.is_empty() { continue; }

        if let Some(eq) = trimmed.find('=') {
            let key = trimmed[..eq].trim().to_ascii_uppercase();
            let val = trimmed[eq + 1..].trim();

            match key.as_str() {
                "NDIMS" | "NIMS" | "OBJECTTYPE" => {
                    if key == "NDIMS" {
                        ndims = val.parse().unwrap_or(3);
                    }
                }
                "DIMSIZE" | "DIM_SIZE" => {
                    sizes = val.split_ascii_whitespace()
                        .filter_map(|s| s.parse().ok())
                        .collect();
                }
                "ELEMENTTYPE" => pixel_type = meta_pixel_type(val),
                "ELEMENTBYTEORDERMSB" => little_endian = !val.eq_ignore_ascii_case("true"),
                "BINARYDATA" if val.eq_ignore_ascii_case("false") => {}
                "BINARYDATABYTEORDERMSB" => little_endian = !val.eq_ignore_ascii_case("true"),
                "COMPRESSEDDATA" => compressed = val.eq_ignore_ascii_case("true"),
                "ELEMENTDATAFILE" => {
                    data_file = Some(val.to_string());
                    if val.eq_ignore_ascii_case("LOCAL") {
                        data_offset = reader.stream_position().map_err(BioFormatsError::Io)?;
                    }
                }
                _ => { extra.insert(key, val.to_string()); }
            }
        }
    }

    Ok(MhdHeader { ndims, sizes, pixel_type, little_endian, compressed, data_file, data_offset, extra })
}

// ---- reader -----------------------------------------------------------------

pub struct MetaImageReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    header: Option<MhdHeader>,
}

impl MetaImageReader {
    pub fn new() -> Self { MetaImageReader { path: None, meta: None, header: None } }

    fn read_data(&self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let hdr = self.header.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mhd_path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * meta.size_c as usize * bps;
        let plane_offset = plane_index as u64 * plane_bytes as u64;

        let data_path = match &hdr.data_file {
            Some(s) if s.eq_ignore_ascii_case("LOCAL") => mhd_path.clone(),
            Some(s) => {
                let parent = mhd_path.parent().unwrap_or(Path::new("."));
                parent.join(s)
            }
            None => {
                // Try replacing .mhd extension with .raw
                mhd_path.with_extension("raw")
            }
        };

        let mut f = File::open(&data_path).map_err(BioFormatsError::Io)?;

        let buf = if hdr.compressed {
            f.seek(SeekFrom::Start(hdr.data_offset)).map_err(BioFormatsError::Io)?;
            let mut dec = flate2::read::ZlibDecoder::new(f);
            let mut all = Vec::new();
            dec.read_to_end(&mut all).map_err(BioFormatsError::Io)?;
            let start = plane_offset as usize;
            let end = start + plane_bytes;
            if end > all.len() {
                return Err(BioFormatsError::InvalidData("MetaImage: plane out of range".into()));
            }
            all[start..end].to_vec()
        } else {
            let offset = hdr.data_offset + plane_offset;
            f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
            let mut buf = vec![0u8; plane_bytes];
            f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
            buf
        };

        // Byte-swap if big-endian
        let mut buf = buf;
        if !hdr.little_endian && bps > 1 {
            for chunk in buf.chunks_exact_mut(bps) { chunk.reverse(); }
        }
        Ok(buf)
    }
}

impl Default for MetaImageReader { fn default() -> Self { Self::new() } }

impl FormatReader for MetaImageReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "mha" | "mhd"))
            .unwrap_or(false)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // MetaImage header always starts with "ObjectType"
        let s = std::str::from_utf8(&header[..header.len().min(32)]).unwrap_or("");
        s.trim_start().starts_with("ObjectType") || s.trim_start().starts_with("NDims")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let hdr = parse_mhd(path)?;

        let (size_x, size_y, size_z) = match hdr.sizes.as_slice() {
            [x] => (*x, 1, 1),
            [x, y] => (*x, *y, 1),
            [x, y, z, ..] => (*x, *y, *z),
            [] => (1, 1, 1),
        };
        let bps = (hdr.pixel_type.bytes_per_sample() * 8) as u8;
        let mut series_metadata: HashMap<String, MetadataValue> = hdr.extra.iter()
            .map(|(k, v)| (k.clone(), MetadataValue::String(v.clone())))
            .collect();
        series_metadata.insert("ndims".into(), MetadataValue::Int(hdr.ndims as i64));

        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c: 1,
            size_t: 1,
            pixel_type: hdr.pixel_type,
            bits_per_pixel: bps,
            image_count: size_z,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: hdr.little_endian,
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
        self.read_data(plane_index)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
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

// ---- writer (MHA = inline) --------------------------------------------------

pub struct MetaImageWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
}

impl MetaImageWriter {
    pub fn new() -> Self { MetaImageWriter { path: None, meta: None, planes: Vec::new() } }
}

impl Default for MetaImageWriter { fn default() -> Self { Self::new() } }

impl FormatWriter for MetaImageWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "mha" | "mhd"))
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

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("mha");
        let is_mhd = ext.eq_ignore_ascii_case("mhd");

        let nz = self.planes.len();
        let f = File::create(&path).map_err(BioFormatsError::Io)?;
        let mut w = BufWriter::new(f);

        writeln!(w, "ObjectType = Image").map_err(BioFormatsError::Io)?;
        writeln!(w, "NDims = {}", if nz > 1 { 3 } else { 2 }).map_err(BioFormatsError::Io)?;
        if nz > 1 {
            writeln!(w, "DimSize = {} {} {}", meta.size_x, meta.size_y, nz).map_err(BioFormatsError::Io)?;
        } else {
            writeln!(w, "DimSize = {} {}", meta.size_x, meta.size_y).map_err(BioFormatsError::Io)?;
        }
        writeln!(w, "ElementType = {}", meta_type_str(meta.pixel_type)).map_err(BioFormatsError::Io)?;
        writeln!(w, "BinaryData = True").map_err(BioFormatsError::Io)?;
        writeln!(w, "BinaryDataByteOrderMSB = False").map_err(BioFormatsError::Io)?;
        writeln!(w, "CompressedData = False").map_err(BioFormatsError::Io)?;

        if is_mhd {
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            writeln!(w, "ElementDataFile = {}.raw", stem).map_err(BioFormatsError::Io)?;
            w.flush().map_err(BioFormatsError::Io)?;
            drop(w);
            // Write raw data file
            let raw_path = path.with_extension("raw");
            let rf = File::create(&raw_path).map_err(BioFormatsError::Io)?;
            let mut rw = BufWriter::new(rf);
            for plane in &self.planes {
                rw.write_all(plane).map_err(BioFormatsError::Io)?;
            }
            rw.flush().map_err(BioFormatsError::Io)?;
        } else {
            writeln!(w, "ElementDataFile = LOCAL").map_err(BioFormatsError::Io)?;
            for plane in &self.planes {
                w.write_all(plane).map_err(BioFormatsError::Io)?;
            }
            w.flush().map_err(BioFormatsError::Io)?;
        }
        self.planes.clear();
        Ok(())
    }
    fn can_do_stacks(&self) -> bool { true }
}
