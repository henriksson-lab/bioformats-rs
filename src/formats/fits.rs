//! FITS (Flexible Image Transport System) reader and writer.
//!
//! Supports the primary HDU (Header Data Unit) with N-dimensional integer
//! and floating-point image data. No extensions or tile compression yet.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::writer::FormatWriter;

const BLOCK: usize = 2880;
const RECORD: usize = 80;

fn read_keyword(record: &[u8]) -> (&str, Option<&str>) {
    let key = std::str::from_utf8(&record[..8]).unwrap_or("").trim_end();
    if record.len() > 9 && record[8] == b'=' {
        let val = std::str::from_utf8(&record[10..]).unwrap_or("").trim();
        (key, Some(val))
    } else {
        (key, None)
    }
}

fn parse_int_value(s: &str) -> Option<i64> {
    let s = s.split('/').next().unwrap_or(s).trim();
    s.trim_matches('\'').trim().parse().ok()
}

fn pixel_type_from_bitpix(bitpix: i64) -> PixelType {
    match bitpix {
        8 => PixelType::Uint8,
        16 => PixelType::Int16,
        -16 => PixelType::Uint16, // IEEE 16-bit float treated as uint16
        32 => PixelType::Int32,
        -32 => PixelType::Float32,
        64 => PixelType::Float64, // int64 treated as float64 for compatibility
        -64 => PixelType::Float64,
        _ => PixelType::Float32,
    }
}

fn bitpix_from_pixel_type(pt: PixelType) -> i64 {
    match pt {
        PixelType::Uint8 => 8,
        PixelType::Int16 | PixelType::Uint16 => 16,
        PixelType::Int32 | PixelType::Uint32 => 32,
        PixelType::Float32 => -32,
        PixelType::Float64 => -64,
        PixelType::Int8 => 8,
        PixelType::Bit => 8,
    }
}

// ---- reader -----------------------------------------------------------------

pub struct FitsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl FitsReader {
    pub fn new() -> Self { FitsReader { path: None, meta: None, data_offset: 0 } }
}

impl Default for FitsReader { fn default() -> Self { Self::new() } }

impl FormatReader for FitsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "fits" | "fit" | "fts"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"SIMPLE  =") || header.starts_with(b"SIMPLE  ")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut bitpix: i64 = 8;
        let mut dims: Vec<u32> = Vec::new();
        let mut series_metadata: HashMap<String, MetadataValue> = HashMap::new();
        let mut found_end = false;

        let mut block = vec![0u8; BLOCK];
        let mut header_blocks = 0u64;

        while !found_end {
            let n = f.read(&mut block).map_err(BioFormatsError::Io)?;
            if n == 0 { break; }
            header_blocks += 1;
            for rec_start in (0..n).step_by(RECORD) {
                let rec = &block[rec_start..(rec_start + RECORD).min(n)];
                if rec.is_empty() { continue; }
                let (key, val) = read_keyword(rec);
                match key {
                    "END" => { found_end = true; break; }
                    "BITPIX" => {
                        if let Some(v) = val.and_then(parse_int_value) { bitpix = v; }
                    }
                    "NAXIS" => {
                        if let Some(v) = val.and_then(parse_int_value) { let _ = v; }
                    }
                    k if k.starts_with("NAXIS") => {
                        if let Some(v) = val.and_then(parse_int_value) {
                            let axis: usize = k[5..].parse().unwrap_or(0);
                            if axis > 0 {
                                if dims.len() < axis { dims.resize(axis, 1); }
                                dims[axis - 1] = v as u32;
                            }
                        }
                    }
                    k if !k.is_empty() => {
                        if let Some(v) = val {
                            series_metadata.insert(k.to_string(), MetadataValue::String(v.to_string()));
                        }
                    }
                    _ => {}
                }
            }
        }

        let data_offset = header_blocks * BLOCK as u64;
        let pixel_type = pixel_type_from_bitpix(bitpix);

        let (size_x, size_y, size_z) = match dims.as_slice() {
            [x] => (*x, 1, 1),
            [x, y] => (*x, *y, 1),
            [x, y, z, ..] => (*x, *y, *z),
            [] => (1, 1, 1),
        };

        // FITS data is big-endian
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bitpix.unsigned_abs() as u8,
            image_count: size_z,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: false, // FITS is big-endian
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
        });
        self.data_offset = data_offset;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> { self.path = None; self.meta = None; Ok(()) }
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
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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

// ---- writer -----------------------------------------------------------------

pub struct FitsWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
}

impl FitsWriter {
    pub fn new() -> Self { FitsWriter { path: None, meta: None, planes: Vec::new() } }
}

impl Default for FitsWriter { fn default() -> Self { Self::new() } }

fn fits_record(key: &str, value: &str) -> [u8; 80] {
    let mut rec = [b' '; 80];
    let k = key.as_bytes();
    let klen = k.len().min(8);
    rec[..klen].copy_from_slice(&k[..klen]);
    rec[8] = b'=';
    let vbytes = value.as_bytes();
    let vlen = vbytes.len().min(70);
    rec[10..10 + vlen].copy_from_slice(&vbytes[..vlen]);
    rec
}

fn fits_comment(text: &str) -> [u8; 80] {
    let mut rec = [b' '; 80];
    let t = text.as_bytes();
    let tlen = t.len().min(80);
    rec[..tlen].copy_from_slice(&t[..tlen]);
    rec
}

impl FormatWriter for FitsWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "fits" | "fit" | "fts"))
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
        let mut w = BufWriter::new(f);

        let bitpix = bitpix_from_pixel_type(meta.pixel_type);
        let nz = self.planes.len() as i64;
        let naxis = if nz > 1 { 3 } else { 2 };

        let mut records: Vec<[u8; 80]> = Vec::new();
        records.push(fits_record("SIMPLE", "                   T"));
        records.push(fits_record("BITPIX", &format!("{:20}", bitpix)));
        records.push(fits_record("NAXIS", &format!("{:20}", naxis)));
        records.push(fits_record("NAXIS1", &format!("{:20}", meta.size_x)));
        records.push(fits_record("NAXIS2", &format!("{:20}", meta.size_y)));
        if nz > 1 {
            records.push(fits_record("NAXIS3", &format!("{:20}", nz)));
        }
        records.push(fits_comment("END"));

        // Pad header to multiple of 2880 bytes
        while records.len() % 36 != 0 {
            records.push([b' '; 80]);
        }

        for rec in &records {
            w.write_all(rec).map_err(BioFormatsError::Io)?;
        }

        // Write pixel data; FITS is big-endian
        let bps = meta.pixel_type.bytes_per_sample();
        for plane in &self.planes {
            if bps == 1 {
                w.write_all(plane).map_err(BioFormatsError::Io)?;
            } else {
                // Byte-swap to big-endian
                for chunk in plane.chunks_exact(bps) {
                    let mut c = chunk.to_vec();
                    c.reverse();
                    w.write_all(&c).map_err(BioFormatsError::Io)?;
                }
            }
        }

        // Pad data to 2880-byte boundary
        let data_bytes = self.planes.iter().map(|p| p.len()).sum::<usize>();
        let pad = (BLOCK - (data_bytes % BLOCK)) % BLOCK;
        w.write_all(&vec![0u8; pad]).map_err(BioFormatsError::Io)?;
        w.flush().map_err(BioFormatsError::Io)?;
        self.planes.clear();
        Ok(())
    }

    fn can_do_stacks(&self) -> bool { true }
}
