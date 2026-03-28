//! ICS (Image Cytometry Standard) reader and writer.
//!
//! Supports ICS version 1.0 (`.ics` + `.ids` pair) and 2.0 (single `.ics` file).
//! Handles gzip-compressed data and all standard pixel types.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::writer::FormatWriter;

// ---- header parsing ---------------------------------------------------------

#[derive(Debug, Default)]
struct IcsHeader {
    version: f32,
    filename: Option<PathBuf>,
    /// Axis names (e.g. ["bits","x","y","z","t"])
    order: Vec<String>,
    /// Axis sizes in the same order as `order`
    sizes: Vec<u32>,
    significant_bits: u8,
    format: String,      // "real" or "integer"
    sign: String,        // "signed" or "unsigned"
    byte_order: Vec<u8>, // e.g. [1,2,3,4]
    gzip_compressed: bool,
    /// Byte offset of pixel data in the data file
    data_offset: u64,
    extra: HashMap<String, String>,
}

impl IcsHeader {
    fn parse(path: &Path) -> Result<IcsHeader> {
        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut reader = BufReader::new(f);
        let mut hdr = IcsHeader::default();

        let mut data_offset = 0u64;

        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).map_err(BioFormatsError::Io)?;
            if n == 0 { break; }

            let line = line.trim_end_matches(|c| c == '\r' || c == '\n');
            if line.eq_ignore_ascii_case("end") {
                // For ICS2, data immediately follows
                data_offset = reader.stream_position().map_err(BioFormatsError::Io)?;
                break;
            }

            let tokens: Vec<&str> = line.split_ascii_whitespace().collect();
            if tokens.is_empty() { continue; }

            match tokens[0].to_ascii_lowercase().as_str() {
                "ics_version" if tokens.len() >= 2 => {
                    hdr.version = tokens[1].parse().unwrap_or(1.0);
                }
                "filename" if tokens.len() >= 2 => {
                    hdr.filename = Some(PathBuf::from(tokens[1]));
                }
                "layout" if tokens.len() >= 3 => match tokens[1].to_ascii_lowercase().as_str() {
                    "order" => {
                        hdr.order = tokens[2..].iter().map(|s| s.to_ascii_lowercase()).collect();
                    }
                    "sizes" => {
                        hdr.sizes = tokens[2..].iter().filter_map(|s| s.parse().ok()).collect();
                    }
                    "significant_bits" | "significant bits" if tokens.len() >= 3 => {
                        hdr.significant_bits = tokens[2].parse().unwrap_or(8);
                    }
                    _ => {}
                },
                "representation" if tokens.len() >= 3 => {
                    match tokens[1].to_ascii_lowercase().as_str() {
                        "format" => hdr.format = tokens[2].to_ascii_lowercase(),
                        "sign" => hdr.sign = tokens[2].to_ascii_lowercase(),
                        "byte_order" | "byteorder" => {
                            hdr.byte_order = tokens[2..].iter()
                                .filter_map(|s| s.parse().ok())
                                .collect();
                        }
                        "compression" if tokens.len() >= 3 => {
                            hdr.gzip_compressed = tokens[2].contains("gzip")
                                || tokens[2].contains("gz");
                        }
                        _ => {}
                    }
                }
                _ => {
                    // Store all other metadata as key-value
                    if tokens.len() >= 3 {
                        let key = format!("{}\t{}", tokens[0], tokens[1]);
                        let val = tokens[2..].join(" ");
                        hdr.extra.insert(key, val);
                    }
                }
            }
        }

        hdr.data_offset = data_offset;
        Ok(hdr)
    }
}

fn pixel_type_from_ics(significant_bits: u8, format: &str, sign: &str) -> PixelType {
    match (significant_bits, format, sign) {
        (1, _, _) => PixelType::Bit,
        (8, _, "signed") => PixelType::Int8,
        (8, _, _) => PixelType::Uint8,
        (16, _, "signed") => PixelType::Int16,
        (16, _, _) => PixelType::Uint16,
        (32, "real", _) => PixelType::Float32,
        (32, _, "signed") => PixelType::Int32,
        (32, _, _) => PixelType::Uint32,
        (64, "real", _) => PixelType::Float64,
        _ => PixelType::Uint8,
    }
}

fn build_metadata(hdr: &IcsHeader) -> Result<ImageMetadata> {
    // ICS axis order: the first axis is usually "bits" (samples per pixel).
    // The remaining axes are spatial/temporal dimensions.
    let axes = &hdr.order;
    let sizes = &hdr.sizes;
    if axes.len() != sizes.len() {
        return Err(BioFormatsError::Format(
            "ICS: order and sizes length mismatch".into(),
        ));
    }

    let mut size_x = 1u32;
    let mut size_y = 1u32;
    let mut size_z = 1u32;
    let mut size_c = 1u32;
    let mut size_t = 1u32;

    for (axis, &sz) in axes.iter().zip(sizes.iter()) {
        match axis.as_str() {
            "x" | "width" => size_x = sz,
            "y" | "height" => size_y = sz,
            "z" | "depth" => size_z = sz,
            "c" | "ch" | "channel" | "channels" => size_c = sz,
            "t" | "time" | "phase" => size_t = sz,
            "bits" => {} // handled separately
            _ => {}
        }
    }

    let sig = if hdr.significant_bits == 0 {
        // Infer from sizes[0] if axis[0] == "bits"
        if axes.first().map(|a| a == "bits").unwrap_or(false) {
            sizes[0] as u8
        } else {
            8
        }
    } else {
        hdr.significant_bits
    };

    let pixel_type = pixel_type_from_ics(sig, &hdr.format, &hdr.sign);

    let image_count = size_z * size_t * if size_c == 1 { 1 } else { size_c };
    let is_rgb = size_c == 3;

    let mut series_metadata: HashMap<String, MetadataValue> = hdr.extra.iter()
        .map(|(k, v)| (k.clone(), MetadataValue::String(v.clone())))
        .collect();
    series_metadata.insert("ics_version".into(), MetadataValue::Float(hdr.version as f64));

    Ok(ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: sig,
        image_count,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true, // ICS is usually LE; TODO: check byte_order field
        resolution_count: 1,
        series_metadata,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    })
}

// ---- reader -----------------------------------------------------------------

pub struct IcsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    header: Option<IcsHeader>,
}

impl IcsReader {
    pub fn new() -> Self { IcsReader { path: None, meta: None, header: None } }

    fn data_path(ics_path: &Path, hdr: &IcsHeader) -> PathBuf {
        if hdr.version < 2.0 {
            // ICS1: companion .ids file
            let stem = ics_path.file_stem().unwrap_or_default();
            ics_path.with_file_name(format!("{}.ids", stem.to_string_lossy()))
        } else {
            ics_path.to_path_buf()
        }
    }

    fn load_raw_data(&self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let hdr = self.header.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let ics_path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let bytes_per_sample = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y * meta.size_c) as usize * bytes_per_sample;
        let plane_offset = plane_index as u64 * plane_bytes as u64;

        let data_path = Self::data_path(ics_path, hdr);
        let data_offset = hdr.data_offset + plane_offset;

        let mut f = File::open(&data_path).map_err(BioFormatsError::Io)?;

        if hdr.gzip_compressed {
            // Decompress all then seek; gzip doesn't support random access
            f.seek(SeekFrom::Start(hdr.data_offset)).map_err(BioFormatsError::Io)?;
            let mut dec = flate2::read::GzDecoder::new(f);
            let mut all = Vec::new();
            dec.read_to_end(&mut all).map_err(BioFormatsError::Io)?;
            let start = plane_offset as usize;
            let end = start + plane_bytes;
            if end > all.len() {
                return Err(BioFormatsError::InvalidData("plane out of range in ICS data".into()));
            }
            Ok(all[start..end].to_vec())
        } else {
            f.seek(SeekFrom::Start(data_offset)).map_err(BioFormatsError::Io)?;
            let mut buf = vec![0u8; plane_bytes];
            f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
            Ok(buf)
        }
    }
}

impl Default for IcsReader { fn default() -> Self { Self::new() } }

impl FormatReader for IcsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ics"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // ICS header starts with "ics_version" or whitespace-then-ics_version
        let s = std::str::from_utf8(&header[..header.len().min(64)]).unwrap_or("");
        s.trim_start().starts_with("ics_version")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let hdr = IcsHeader::parse(path)?;
        let meta = build_metadata(&hdr)?;
        self.path = Some(path.to_path_buf());
        self.header = Some(hdr);
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None; self.meta = None; self.header = None;
        Ok(())
    }

    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.load_raw_data(plane_index)
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

pub struct IcsWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
}

impl IcsWriter {
    pub fn new() -> Self { IcsWriter { path: None, meta: None, planes: Vec::new() } }
}

impl Default for IcsWriter { fn default() -> Self { Self::new() } }

impl FormatWriter for IcsWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ics"))
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

    fn save_bytes(&mut self, _idx: u32, data: &[u8]) -> Result<()> {
        self.planes.push(data.to_vec());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.take().ok_or(BioFormatsError::NotInitialized)?;

        // Write ICS2 format: header + "end\r\n" + raw binary (all in one .ics file)
        let mut f = File::create(&path).map_err(BioFormatsError::Io)?;

        let bps = meta.pixel_type.bytes_per_sample() * 8;
        let (format_str, sign_str) = match meta.pixel_type {
            PixelType::Float32 | PixelType::Float64 => ("real", "signed"),
            PixelType::Int8 | PixelType::Int16 | PixelType::Int32 => ("integer", "signed"),
            _ => ("integer", "unsigned"),
        };

        writeln!(f, "ics_version\t2.0").map_err(BioFormatsError::Io)?;
        writeln!(f, "filename\t{}", path.file_stem().unwrap_or_default().to_string_lossy()).map_err(BioFormatsError::Io)?;
        writeln!(f, "layout\tparameters\t{}", 4 + if meta.size_z > 1 { 1 } else { 0 } + if meta.size_t > 1 { 1 } else { 0 }).map_err(BioFormatsError::Io)?;

        let mut order_parts = vec!["bits", "x", "y"];
        let mut size_parts = vec![
            bps.to_string(),
            meta.size_x.to_string(),
            meta.size_y.to_string(),
        ];
        if meta.size_z > 1 {
            order_parts.push("z");
            size_parts.push(meta.size_z.to_string());
        }
        if meta.size_t > 1 {
            order_parts.push("t");
            size_parts.push(meta.size_t.to_string());
        }
        if meta.size_c > 1 {
            order_parts.push("ch");
            size_parts.push(meta.size_c.to_string());
        }

        writeln!(f, "layout\torder\t{}", order_parts.join(" ")).map_err(BioFormatsError::Io)?;
        writeln!(f, "layout\tsizes\t{}", size_parts.join(" ")).map_err(BioFormatsError::Io)?;
        writeln!(f, "layout\tsignificant_bits\t{}", bps).map_err(BioFormatsError::Io)?;
        writeln!(f, "representation\tformat\t{}", format_str).map_err(BioFormatsError::Io)?;
        writeln!(f, "representation\tsign\t{}", sign_str).map_err(BioFormatsError::Io)?;
        writeln!(f, "representation\tbyte_order\t1 2 3 4").map_err(BioFormatsError::Io)?;
        writeln!(f, "representation\tcompression\tuncompressed").map_err(BioFormatsError::Io)?;
        writeln!(f, "end\r").map_err(BioFormatsError::Io)?;

        for plane in &self.planes {
            f.write_all(plane).map_err(BioFormatsError::Io)?;
        }
        self.planes.clear();
        Ok(())
    }

    fn can_do_stacks(&self) -> bool { true }
}
