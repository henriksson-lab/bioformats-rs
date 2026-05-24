//! FITS (Flexible Image Transport System) reader and writer.
//!
//! Supports primary image HDUs and the first IMAGE extension after an empty
//! primary HDU. No tile compression yet.

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

fn parse_float_value(s: &str) -> Option<f64> {
    let s = s
        .split('/')
        .next()
        .unwrap_or(s)
        .trim()
        .trim_matches('\'')
        .trim()
        .replace('D', "E");
    s.parse().ok()
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

fn scaled_pixel_type(bitpix: i64, bzero: f64, bscale: f64) -> PixelType {
    if (bscale - 1.0).abs() < f64::EPSILON && bzero.abs() < f64::EPSILON {
        return pixel_type_from_bitpix(bitpix);
    }

    if bitpix == 16 && (bscale - 1.0).abs() < f64::EPSILON && (bzero - 32768.0).abs() < f64::EPSILON
    {
        PixelType::Uint16
    } else {
        PixelType::Float32
    }
}

#[derive(Debug)]
struct FitsHdu {
    bitpix: i64,
    dims: Vec<u32>,
    bzero: f64,
    bscale: f64,
    xtension: Option<String>,
    data_offset: u64,
    series_metadata: HashMap<String, MetadataValue>,
}

fn hdu_data_len(bitpix: i64, dims: &[u32]) -> u64 {
    if dims.is_empty() {
        return 0;
    }
    let bytes_per_sample = (bitpix.unsigned_abs() / 8).max(1);
    dims.iter()
        .fold(bytes_per_sample, |acc, &dim| acc.saturating_mul(dim as u64))
}

fn padded_len(len: u64) -> u64 {
    let block = BLOCK as u64;
    ((len + block - 1) / block) * block
}

fn raw_sample_as_f64(bytes: &[u8], bitpix: i64) -> f64 {
    match bitpix {
        8 => bytes[0] as f64,
        16 => i16::from_be_bytes([bytes[0], bytes[1]]) as f64,
        32 => i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64,
        64 => i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]) as f64,
        -32 => f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64,
        -64 => f64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ]),
        _ => 0.0,
    }
}

fn convert_fits_endian(buf: &[u8], bitpix: i64, pixel_type: PixelType) -> Vec<u8> {
    match bitpix {
        8 => buf.to_vec(),
        16 | -16 => buf
            .chunks_exact(2)
            .flat_map(|chunk| [chunk[1], chunk[0]])
            .collect(),
        32 | -32 => buf
            .chunks_exact(4)
            .flat_map(|chunk| [chunk[3], chunk[2], chunk[1], chunk[0]])
            .collect(),
        64 if pixel_type == PixelType::Float64 => buf
            .chunks_exact(8)
            .flat_map(|chunk| raw_sample_as_f64(chunk, bitpix).to_le_bytes().into_iter())
            .collect(),
        64 | -64 => buf
            .chunks_exact(8)
            .flat_map(|chunk| {
                [
                    chunk[7], chunk[6], chunk[5], chunk[4], chunk[3], chunk[2], chunk[1], chunk[0],
                ]
            })
            .collect(),
        _ => buf.to_vec(),
    }
}

fn apply_fits_scaling(
    buf: &[u8],
    bitpix: i64,
    pixel_type: PixelType,
    bzero: f64,
    bscale: f64,
) -> Vec<u8> {
    let raw_bps = (bitpix.unsigned_abs() as usize / 8).max(1);
    match pixel_type {
        PixelType::Uint16 => buf
            .chunks_exact(raw_bps)
            .flat_map(|chunk| {
                let value = raw_sample_as_f64(chunk, bitpix) * bscale + bzero;
                (value.round().clamp(0.0, u16::MAX as f64) as u16).to_le_bytes()
            })
            .collect(),
        PixelType::Float32 => buf
            .chunks_exact(raw_bps)
            .flat_map(|chunk| {
                let value = (raw_sample_as_f64(chunk, bitpix) * bscale + bzero) as f32;
                value.to_le_bytes()
            })
            .collect(),
        PixelType::Float64 => buf
            .chunks_exact(raw_bps)
            .flat_map(|chunk| {
                let value = raw_sample_as_f64(chunk, bitpix) * bscale + bzero;
                value.to_le_bytes()
            })
            .collect(),
        _ => convert_fits_endian(buf, bitpix, pixel_type),
    }
}

fn read_hdu(f: &mut File) -> Result<Option<FitsHdu>> {
    let mut bitpix: i64 = 8;
    let mut dims: Vec<u32> = Vec::new();
    let mut bzero = 0.0;
    let mut bscale = 1.0;
    let mut xtension = None;
    let mut series_metadata: HashMap<String, MetadataValue> = HashMap::new();
    let mut found_end = false;
    let mut block = vec![0u8; BLOCK];

    loop {
        let n = f.read(&mut block).map_err(BioFormatsError::Io)?;
        if n == 0 {
            if series_metadata.is_empty() && dims.is_empty() {
                return Ok(None);
            }
            break;
        }

        for rec_start in (0..n).step_by(RECORD) {
            let rec = &block[rec_start..(rec_start + RECORD).min(n)];
            if rec.is_empty() {
                continue;
            }
            let (key, val) = read_keyword(rec);
            match key {
                "END" => {
                    found_end = true;
                    break;
                }
                "BITPIX" => {
                    if let Some(v) = val.and_then(parse_int_value) {
                        bitpix = v;
                    }
                }
                "NAXIS" => {
                    if let Some(v) = val.and_then(parse_int_value) {
                        if v == 0 {
                            dims.clear();
                        }
                    }
                }
                k if k.starts_with("NAXIS") => {
                    if let Some(v) = val.and_then(parse_int_value) {
                        let axis: usize = k[5..].parse().unwrap_or(0);
                        if axis > 0 {
                            if dims.len() < axis {
                                dims.resize(axis, 1);
                            }
                            dims[axis - 1] = v as u32;
                        }
                    }
                }
                "BZERO" => {
                    if let Some(v) = val.and_then(parse_float_value) {
                        bzero = v;
                    }
                }
                "BSCALE" => {
                    if let Some(v) = val.and_then(parse_float_value) {
                        bscale = v;
                    }
                }
                "XTENSION" => {
                    if let Some(v) = val {
                        xtension = Some(v.to_string());
                        series_metadata
                            .insert(key.to_string(), MetadataValue::String(v.to_string()));
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

        if found_end {
            break;
        }
    }

    let data_offset = f.stream_position().map_err(BioFormatsError::Io)?;
    Ok(Some(FitsHdu {
        bitpix,
        dims,
        bzero,
        bscale,
        xtension,
        data_offset,
        series_metadata,
    }))
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

fn fits_series_from_hdu(hdu: FitsHdu) -> FitsSeries {
    let pixel_type = scaled_pixel_type(hdu.bitpix, hdu.bzero, hdu.bscale);
    let (size_x, size_y, size_z) = match hdu.dims.as_slice() {
        [x] => (*x, 1, 1),
        [x, y] => (*x, *y, 1),
        [x, y, z, ..] => (*x, *y, *z),
        [] => (1, 1, 1),
    };

    FitsSeries {
        metadata: ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
            image_count: size_z,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: hdu.series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        },
        data_offset: hdu.data_offset,
        raw_bitpix: hdu.bitpix,
        bzero: hdu.bzero,
        bscale: hdu.bscale,
    }
}

// ---- reader -----------------------------------------------------------------

pub struct FitsReader {
    path: Option<PathBuf>,
    series: Vec<FitsSeries>,
    current_series: usize,
}

#[derive(Debug)]
struct FitsSeries {
    metadata: ImageMetadata,
    data_offset: u64,
    raw_bitpix: i64,
    bzero: f64,
    bscale: f64,
}

impl FitsReader {
    pub fn new() -> Self {
        FitsReader {
            path: None,
            series: Vec::new(),
            current_series: 0,
        }
    }
}

impl Default for FitsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for FitsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "fits" | "fit" | "fts"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"SIMPLE  =") || header.starts_with(b"SIMPLE  ")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut series = Vec::new();

        while let Some(hdu) = read_hdu(&mut f)? {
            let data_len = hdu_data_len(hdu.bitpix, &hdu.dims);
            let is_image_hdu = hdu
                .xtension
                .as_deref()
                .map(|xtension| xtension.contains("IMAGE"))
                .unwrap_or(true);
            if !hdu.dims.is_empty() && is_image_hdu {
                series.push(fits_series_from_hdu(hdu));
            }
            f.seek(SeekFrom::Current(padded_len(data_len) as i64))
                .map_err(BioFormatsError::Io)?;
        }

        if series.is_empty() {
            return Err(BioFormatsError::Format(
                "FITS file contains no image HDU".into(),
            ));
        }
        self.series = series;
        self.current_series = 0;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series.clear();
        self.current_series = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.series.len().max(1)
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_count() || self.series.is_empty() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }
    fn series(&self) -> usize {
        self.current_series
    }
    fn metadata(&self) -> &ImageMetadata {
        &self.series[self.current_series].metadata
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let series = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = &series.metadata;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let raw_bps = (series.raw_bitpix.unsigned_abs() as usize / 8).max(1);
        let samples = meta.size_x as usize * meta.size_y as usize;
        let raw_plane_bytes = samples * raw_bps;
        let offset = series.data_offset + plane_index as u64 * raw_plane_bytes as u64;

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; raw_plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;

        if (series.bscale - 1.0).abs() < f64::EPSILON && series.bzero.abs() < f64::EPSILON {
            return Ok(convert_fits_endian(
                &buf,
                series.raw_bitpix,
                meta.pixel_type,
            ));
        }
        Ok(apply_fits_scaling(
            &buf,
            series.raw_bitpix,
            meta.pixel_type,
            series.bzero,
            series.bscale,
        ))
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
        let meta = &self.series[self.current_series].metadata;
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
        let meta = &self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?
            .metadata;
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
    pub fn new() -> Self {
        FitsWriter {
            path: None,
            meta: None,
            planes: Vec::new(),
        }
    }
}

impl Default for FitsWriter {
    fn default() -> Self {
        Self::new()
    }
}

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
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "fits" | "fit" | "fts"))
            .unwrap_or(false)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        self.meta = Some(meta.clone());
        Ok(())
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta
            .as_ref()
            .ok_or_else(|| BioFormatsError::Format("set_metadata first".into()))?;
        self.path = Some(path.to_path_buf());
        self.planes.clear();
        Ok(())
    }
    fn save_bytes(&mut self, _: u32, data: &[u8]) -> Result<()> {
        self.planes.push(data.to_vec());
        Ok(())
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

    fn can_do_stacks(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_fits_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bioformats_fits_{}_{}.fits",
            name,
            std::process::id()
        ))
    }

    fn padded_block(mut bytes: Vec<u8>) -> Vec<u8> {
        let pad = (BLOCK - (bytes.len() % BLOCK)) % BLOCK;
        bytes.extend(std::iter::repeat(0).take(pad));
        bytes
    }

    fn header(mut records: Vec<[u8; RECORD]>) -> Vec<u8> {
        records.push(fits_comment("END"));
        let mut bytes = Vec::new();
        for record in records {
            bytes.extend_from_slice(&record);
        }
        padded_block(bytes)
    }

    fn empty_primary_hdu() -> Vec<u8> {
        header(vec![
            fits_record("SIMPLE", "                   T"),
            fits_record("BITPIX", "                   8"),
            fits_record("NAXIS", "                   0"),
        ])
    }

    fn image_extension_hdu(
        bitpix: i64,
        dims: &[u32],
        extra: &[(&str, &str)],
        data: &[u8],
    ) -> Vec<u8> {
        let mut records = vec![
            fits_record("XTENSION", "'IMAGE   '"),
            fits_record("BITPIX", &format!("{:20}", bitpix)),
            fits_record("NAXIS", &format!("{:20}", dims.len())),
        ];
        for (i, dim) in dims.iter().enumerate() {
            records.push(fits_record(
                &format!("NAXIS{}", i + 1),
                &format!("{:20}", dim),
            ));
        }
        for (key, value) in extra {
            records.push(fits_record(key, value));
        }

        let mut bytes = header(records);
        bytes.extend_from_slice(&padded_block(data.to_vec()));
        bytes
    }

    #[test]
    fn fits_image_extensions_are_exposed_as_series() {
        let path = temp_fits_path("extension_series");
        let mut bytes = empty_primary_hdu();
        bytes.extend_from_slice(&image_extension_hdu(8, &[2, 2], &[], &[1, 2, 3, 4]));
        bytes.extend_from_slice(&image_extension_hdu(
            16,
            &[2, 1, 2],
            &[
                ("BZERO", "             32768.0"),
                ("BSCALE", "                 1.0"),
            ],
            &[
                0x80, 0x00, 0xff, 0xff, // plane 0: -32768, -1
                0x00, 0x00, 0x7f, 0xff, // plane 1: 0, 32767
            ],
        ));
        std::fs::write(&path, bytes).expect("write synthetic FITS");

        let mut reader = FitsReader::new();
        reader.set_id(&path).expect("open FITS");
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().image_count, 1);
        assert_eq!(
            reader.open_bytes(0).expect("series 0 plane"),
            vec![1, 2, 3, 4]
        );

        reader.set_series(1).expect("select extension series");
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_z, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
        assert_eq!(
            reader.open_bytes(0).expect("series 1 plane 0"),
            vec![0, 0, 255, 127]
        );
        assert_eq!(
            reader.open_bytes(1).expect("series 1 plane 1"),
            vec![0, 128, 255, 255]
        );
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fits_primary_image_and_extension_are_both_series() {
        let path = temp_fits_path("primary_and_extension");
        let mut bytes = header(vec![
            fits_record("SIMPLE", "                   T"),
            fits_record("BITPIX", "                   8"),
            fits_record("NAXIS", "                   2"),
            fits_record("NAXIS1", "                   1"),
            fits_record("NAXIS2", "                   2"),
        ]);
        bytes.extend_from_slice(&padded_block(vec![9, 10]));
        bytes.extend_from_slice(&image_extension_hdu(8, &[1, 1], &[], &[42]));
        std::fs::write(&path, bytes).expect("write synthetic FITS");

        let mut reader = FitsReader::new();
        reader.set_id(&path).expect("open FITS");
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.open_bytes(0).expect("primary plane"), vec![9, 10]);
        reader.set_series(1).expect("select extension");
        assert_eq!(reader.open_bytes(0).expect("extension plane"), vec![42]);

        let _ = std::fs::remove_file(path);
    }
}
