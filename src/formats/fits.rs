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
use crate::common::region::crop_full_plane;
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

fn pixel_type_from_bitpix(bitpix: i64) -> Result<PixelType> {
    match bitpix {
        8 => Ok(PixelType::Uint8),
        16 | -16 => Ok(PixelType::Int16),
        32 => Ok(PixelType::Int32),
        -32 => Ok(PixelType::Float32),
        64 | -64 => Ok(PixelType::Float64),
        _ => Err(BioFormatsError::Format(format!(
            "Unsupported byte depth: {}",
            bitpix.unsigned_abs() / 8
        ))),
    }
}

#[derive(Debug)]
struct FitsHdu {
    bitpix: i64,
    dims: Vec<u32>,
    data_offset: u64,
    series_metadata: HashMap<String, MetadataValue>,
}

fn read_hdu(f: &mut File) -> Result<Option<FitsHdu>> {
    let mut bitpix: i64 = 8;
    let mut dims: Vec<u32> = Vec::new();
    let mut series_metadata: HashMap<String, MetadataValue> = HashMap::new();
    let mut found_image_end = false;
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
                    // FitsReader.java keeps scanning if the first header has
                    // no populated image dimensions; the next END after NAXIS1
                    // marks the image header whose pixels will be read.
                    if dims.first().copied().unwrap_or(0) > 0 {
                        found_image_end = true;
                        break;
                    }
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
                        let axis: usize = k[5..].parse().map_err(|_| {
                            BioFormatsError::Format(format!("FITS: invalid NAXIS keyword {k:?}"))
                        })?;
                        if axis > 0 {
                            if dims.len() < axis {
                                dims.resize(axis, 1);
                            }
                            dims[axis - 1] = v as u32;
                        }
                    }
                }
                "BZERO" | "BSCALE" => {
                    // Recorded as metadata only; Java does not rescale samples.
                    if let Some(v) = val {
                        series_metadata
                            .insert(key.to_string(), MetadataValue::String(v.to_string()));
                    }
                }
                "XTENSION" => {
                    if let Some(v) = val {
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

        if found_image_end {
            break;
        }
    }

    let data_offset = f.stream_position().map_err(BioFormatsError::Io)?;
    Ok(Some(FitsHdu {
        bitpix,
        dims,
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

fn fits_series_from_hdu(hdu: FitsHdu) -> Result<FitsSeries> {
    // Java FitsReader keeps the raw on-disk pixel type (no BZERO/BSCALE
    // rescaling) and treats the data as big-endian.
    let pixel_type = pixel_type_from_bitpix(hdu.bitpix)?;
    let (size_x, size_y, size_z) = match hdu.dims.as_slice() {
        [x] => (*x, 1, 1),
        [x, y] => (*x, *y, 1),
        [x, y, z, ..] => (*x, *y, *z),
        [] => (1, 1, 1),
    };

    Ok(FitsSeries {
        metadata: ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
            image_count: size_z,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: false, // FITS is big-endian per spec
            resolution_count: 1,
            thumbnail: false,
            series_metadata: hdu.series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        },
        data_offset: hdu.data_offset,
        raw_bitpix: hdu.bitpix,
    })
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
        let mut first_record = [0u8; RECORD];
        f.read_exact(&mut first_record)
            .map_err(BioFormatsError::Io)?;
        if !first_record.starts_with(b"SIMPLE") {
            return Err(BioFormatsError::Format("Unsupported FITS file.".into()));
        }
        f.seek(SeekFrom::Start(0)).map_err(BioFormatsError::Io)?;
        let mut series = Vec::new();

        // Java FitsReader reads only the primary HDU (the standard header that
        // populates the image dimensions). Image extensions are ignored.
        if let Some(hdu) = read_hdu(&mut f)? {
            if hdu.dims.is_empty() {
                return Err(BioFormatsError::Format(
                    "FITS primary HDU contains no image".into(),
                ));
            }
            let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
            let mut s = fits_series_from_hdu(hdu)?;
            // Correct for truncated files: Java FitsReader clamps sizeZ to
            // (fileLen - pixelOffset) / planeSize when the declared stack would
            // run past the end of the file.
            let meta = &mut s.metadata;
            let raw_bps = (s.raw_bitpix.unsigned_abs() as u64 / 8).max(1);
            let plane_size = (meta.size_x as u64) * (meta.size_y as u64) * raw_bps;
            if plane_size > 0 {
                let avail = file_len.saturating_sub(s.data_offset);
                let declared = plane_size.saturating_mul(meta.size_z as u64);
                if declared > avail {
                    let clamped = (avail / plane_size) as u32;
                    meta.size_z = clamped;
                    meta.image_count = clamped;
                }
            }
            series.push(s);
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
        self.series
            .get(self.current_series)
            .map(|series| &series.metadata)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
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

        // Java FitsReader returns the raw big-endian samples unchanged: no
        // BZERO/BSCALE rescaling and no byte-swap.
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
        let meta = &self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?
            .metadata;
        crop_full_plane("FITS", &full, meta, 1, x, y, w, h)
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let meta = self.series.get(self.current_series).map(|s| &s.metadata)?;
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        // FitsReader does not call setImageName, so Java falls back to the
        // current file's basename (with extension).
        if let (Some(path), Some(img)) = (self.path.as_ref(), ome.images.get_mut(0)) {
            img.name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        }
        Some(ome)
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

fn bytes_as_big_endian(meta: &ImageMetadata, data: &[u8]) -> Vec<u8> {
    let bps = meta.pixel_type.bytes_per_sample();
    if !meta.is_little_endian || bps <= 1 {
        return data.to_vec();
    }
    let mut out = data.to_vec();
    for chunk in out.chunks_exact_mut(bps) {
        chunk.reverse();
    }
    out
}

impl FormatWriter for FitsWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "fits" | "fit" | "fts"))
            .unwrap_or(false)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        if meta.size_c.max(1) > 1 || meta.size_t.max(1) > 1 {
            return Err(BioFormatsError::UnsupportedFormat(
                "FITS writer does not preserve C/T axes; write Z stacks only".into(),
            ));
        }
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
    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_next_plane(
            "FITS",
            meta,
            self.planes.len(),
            plane_index,
            data.len(),
        )?;
        self.planes.push(bytes_as_big_endian(meta, data));
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let _path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_complete("FITS", meta, self.planes.len())?;
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

        // Write pixel data; FITS is big-endian.
        for plane in &self.planes {
            w.write_all(plane).map_err(BioFormatsError::Io)?;
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
        // Java FitsReader reads ONLY the primary HDU. A 16-bit, big-endian,
        // 2-plane primary image must be exposed as a single series whose raw
        // big-endian bytes are returned unmodified (no BZERO/BSCALE rescale,
        // no byte-swap).
        let mut bytes = header(vec![
            fits_record("SIMPLE", "                   T"),
            fits_record("BITPIX", "                  16"),
            fits_record("NAXIS", "                   3"),
            fits_record("NAXIS1", "                   2"),
            fits_record("NAXIS2", "                   1"),
            fits_record("NAXIS3", "                   2"),
            fits_record("BZERO", "             32768.0"),
            fits_record("BSCALE", "                 1.0"),
        ]);
        bytes.extend_from_slice(&padded_block(vec![
            0x80, 0x00, 0xff, 0xff, // plane 0: big-endian -32768, -1
            0x00, 0x00, 0x7f, 0xff, // plane 1: big-endian 0, 32767
        ]));
        std::fs::write(&path, bytes).expect("write synthetic FITS");

        let mut reader = FitsReader::new();
        reader.set_id(&path).expect("open FITS");
        // Only the primary HDU is exposed.
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_z, 2);
        assert_eq!(reader.metadata().image_count, 2);
        // BITPIX 16 → signed Int16, big-endian, raw.
        assert_eq!(reader.metadata().pixel_type, PixelType::Int16);
        assert!(!reader.metadata().is_little_endian);
        // Raw big-endian bytes are returned unchanged.
        assert_eq!(
            reader.open_bytes(0).expect("plane 0"),
            vec![0x80, 0x00, 0xff, 0xff]
        );
        assert_eq!(
            reader.open_bytes(1).expect("plane 1"),
            vec![0x00, 0x00, 0x7f, 0xff]
        );
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fits_only_primary_hdu_is_read() {
        // Java reads only the primary HDU; any image extension is ignored.
        let path = temp_fits_path("primary_only");
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
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.open_bytes(0).expect("primary plane"), vec![9, 10]);
        assert!(reader.set_series(1).is_err());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fits_empty_primary_reads_first_image_extension_like_java() {
        let path = temp_fits_path("empty_primary_extension");
        let mut bytes = header(vec![
            fits_record("SIMPLE", "                   T"),
            fits_record("BITPIX", "                   8"),
            fits_record("NAXIS", "                   0"),
        ]);
        bytes.extend_from_slice(&image_extension_hdu(8, &[2, 1], &[], &[11, 12]));
        std::fs::write(&path, bytes).expect("write synthetic FITS");

        let mut reader = FitsReader::new();
        reader.set_id(&path).expect("open FITS extension image");
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_z, 1);
        assert_eq!(reader.open_bytes(0).expect("extension plane"), vec![11, 12]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fits_rejects_malformed_naxis_keyword() {
        let path = temp_fits_path("bad_naxis_keyword");
        let mut bytes = header(vec![
            fits_record("SIMPLE", "                   T"),
            fits_record("BITPIX", "                   8"),
            fits_record("NAXIS", "                   2"),
            fits_record("NAXISX", "                   1"),
            fits_record("NAXIS2", "                   1"),
        ]);
        bytes.extend_from_slice(&padded_block(vec![42]));
        std::fs::write(&path, bytes).expect("write synthetic FITS");

        let mut reader = FitsReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            err.to_string().contains("invalid NAXIS keyword"),
            "unexpected FITS error: {err}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fits_rejects_missing_simple_primary_record_like_java() {
        let path = temp_fits_path("missing_simple");
        let mut bytes = header(vec![
            fits_record("XTENSION", "'IMAGE   '"),
            fits_record("BITPIX", "                   8"),
            fits_record("NAXIS", "                   2"),
            fits_record("NAXIS1", "                   1"),
            fits_record("NAXIS2", "                   1"),
        ]);
        bytes.extend_from_slice(&padded_block(vec![42]));
        std::fs::write(&path, bytes).expect("write synthetic FITS");

        let mut reader = FitsReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            err.to_string().contains("Unsupported FITS file"),
            "unexpected FITS error: {err}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fits_negative_sixteen_bitpix_maps_to_int16_like_java() {
        let path = temp_fits_path("negative_16_bitpix");
        let mut bytes = header(vec![
            fits_record("SIMPLE", "                   T"),
            fits_record("BITPIX", "                 -16"),
            fits_record("NAXIS", "                   2"),
            fits_record("NAXIS1", "                   1"),
            fits_record("NAXIS2", "                   1"),
        ]);
        bytes.extend_from_slice(&padded_block(vec![0x12, 0x34]));
        std::fs::write(&path, bytes).expect("write synthetic FITS");

        let mut reader = FitsReader::new();
        reader.set_id(&path).expect("open FITS");
        assert_eq!(reader.metadata().pixel_type, PixelType::Int16);
        assert_eq!(reader.open_bytes(0).expect("plane"), vec![0x12, 0x34]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fits_rejects_unsupported_bitpix_like_java() {
        let path = temp_fits_path("unsupported_bitpix");
        let mut bytes = header(vec![
            fits_record("SIMPLE", "                   T"),
            fits_record("BITPIX", "                  24"),
            fits_record("NAXIS", "                   2"),
            fits_record("NAXIS1", "                   1"),
            fits_record("NAXIS2", "                   1"),
        ]);
        bytes.extend_from_slice(&padded_block(vec![1, 2, 3]));
        std::fs::write(&path, bytes).expect("write synthetic FITS");

        let mut reader = FitsReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            err.to_string().contains("Unsupported byte depth: 3"),
            "unexpected FITS error: {err}"
        );

        let _ = std::fs::remove_file(path);
    }
}
