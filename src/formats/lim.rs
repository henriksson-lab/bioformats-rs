//! LIM (Laboratory Imaging) and TillVision format readers.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ── LIM Reader ────────────────────────────────────────────────────────────────

pub struct LimReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl LimReader {
    pub fn new() -> Self {
        LimReader {
            path: None,
            meta: None,
            data_offset: 0,
        }
    }
}

impl Default for LimReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Fixed pixel-data offset used by LIMReader.java.
const LIM_PIXELS_OFFSET: u64 = 0x94b;

fn load_lim_header(path: &Path) -> Result<(ImageMetadata, u64)> {
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    let mut header = [0u8; 8];
    f.read_exact(&mut header).map_err(BioFormatsError::Io)?;

    // Header layout (matching LIMReader.initFile, little-endian):
    //   0  sizeX = readShort() & 0x7fff
    //   2  sizeY = readShort()
    //   4  bits  = readShort()
    //   6  isCompressed = readShort() != 0
    let size_x = (i16::from_le_bytes([header[0], header[1]]) as i32 & 0x7fff) as u32;
    let size_y = i16::from_le_bytes([header[2], header[3]]) as i32 as u32;
    let mut bits = i16::from_le_bytes([header[4], header[5]]) as i32;
    let is_compressed = i16::from_le_bytes([header[6], header[7]]) != 0;

    if size_x == 0 || size_y == 0 || bits == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "LIM header is missing required dimensions".to_string(),
        ));
    }

    // Round bits up to the next multiple of 8.
    while bits % 8 != 0 {
        bits += 1;
    }

    // RGB images store 3 channels packed; bits is divided across them.
    let mut size_c: u32 = 1;
    if bits % 3 == 0 {
        size_c = 3;
        bits /= 3;
    }

    // FormatTools.pixelTypeFromBytes(bits/8, false, false) -> unsigned integer.
    let pixel_type = match bits / 8 {
        1 => PixelType::Uint8,
        2 => PixelType::Uint16,
        4 => PixelType::Uint32,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "LIM byte depth {other} is not supported"
            )));
        }
    };

    // LIMReader.java itself rejects compressed planes with
    // UnsupportedCompressionException("Compressed LIM files not supported."),
    // i.e. the Java reference does NOT decompress LIM data. Being faithful to
    // the reference, we reject compressed files here as well rather than
    // inventing an undocumented decompression scheme.
    if is_compressed {
        return Err(BioFormatsError::UnsupportedFormat(
            "Compressed LIM files not supported.".to_string(),
        ));
    }

    let is_rgb = size_c > 1;
    let bps = pixel_type.bytes_per_sample();
    let plane_bytes = (size_x as u64)
        .checked_mul(size_y as u64)
        .and_then(|px| px.checked_mul(size_c as u64))
        .and_then(|samples| samples.checked_mul(bps as u64))
        .ok_or_else(|| BioFormatsError::Format("LIM plane size overflows".to_string()))?;
    let required_len = LIM_PIXELS_OFFSET
        .checked_add(plane_bytes)
        .ok_or_else(|| BioFormatsError::Format("LIM file size overflows".to_string()))?;
    let actual_len = f.metadata().map_err(BioFormatsError::Io)?.len();
    if actual_len < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "LIM pixel payload is shorter than declared ({actual_len} < {required_len})"
        )));
    }

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z: 1,
        size_c,
        size_t: 1,
        pixel_type,
        bits_per_pixel: (bps * 8) as u8,
        image_count: 1,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok((meta, LIM_PIXELS_OFFSET))
}

impl FormatReader for LimReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lim"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, data_offset) = load_lim_header(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.data_offset = data_offset;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let is_rgb = meta.is_rgb;
        let size_c = meta.size_c as usize;
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * size_c * bps;
        // LIM always reads from the fixed PIXELS_OFFSET (single image plane).
        let file_offset = self.data_offset;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(file_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;

        // Swap red and blue channels for RGB images (BGR storage), matching
        // LIMReader.openBytes. The swap is per-channel byte-wise (3 channels).
        if is_rgb {
            let i = 0..buf.len() / 3;
            for px in i {
                buf.swap(px * 3, px * 3 + 2);
            }
        }
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
        validate_region(meta, x, y, w, h)?;
        let bps = meta.pixel_type.bytes_per_sample() * meta.size_c as usize;
        let row_bytes = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * bps;
            out.extend_from_slice(&src[s..s + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

fn validate_region(meta: &ImageMetadata, x: u32, y: u32, w: u32, h: u32) -> Result<()> {
    let x2 = x
        .checked_add(w)
        .ok_or_else(|| BioFormatsError::Format("LIM region width overflows".to_string()))?;
    let y2 = y
        .checked_add(h)
        .ok_or_else(|| BioFormatsError::Format("LIM region height overflows".to_string()))?;
    if x2 > meta.size_x || y2 > meta.size_y {
        return Err(BioFormatsError::Format(
            "LIM region is outside image bounds".to_string(),
        ));
    }
    Ok(())
}

// ── TillVision Reader ─────────────────────────────────────────────────────────

pub struct TillVisionReader {
    series: Vec<TillVisionSeries>,
    current_series: usize,
}

#[derive(Clone)]
struct TillVisionSeries {
    pixel_path: PathBuf,
    meta: ImageMetadata,
}

impl TillVisionReader {
    pub fn new() -> Self {
        TillVisionReader {
            series: Vec::new(),
            current_series: 0,
        }
    }

    fn unsupported() -> BioFormatsError {
        BioFormatsError::UnsupportedFormat(
            "TillVision embedded VWS payload decoding is not implemented".to_string(),
        )
    }
}

impl Default for TillVisionReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for TillVisionReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("vws") || e.eq_ignore_ascii_case("pst"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let series = load_tillvision_series(path)?;
        if series.is_empty() {
            return Err(Self::unsupported());
        }
        self.series = series;
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.series.clear();
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len().max(1)
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series.len() {
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
            .map(|series| &series.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let series = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = &series.meta;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let plane_bytes = tillvision_plane_bytes(meta)?;
        let offset = plane_index as u64 * plane_bytes as u64;
        let mut f = std::fs::File::open(&series.pixel_path).map_err(BioFormatsError::Io)?;
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
        let meta = self.metadata();
        validate_region(meta, x, y, w, h)?;
        let bps = meta.pixel_type.bytes_per_sample() * meta.size_c as usize;
        let row_bytes = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * bps;
            out.extend_from_slice(&src[s..s + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.metadata();
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

fn load_tillvision_series(path: &Path) -> Result<Vec<TillVisionSeries>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut pixel_files = Vec::new();

    if ext == "pst" && path.is_file() {
        pixel_files.push(path.to_path_buf());
    } else if ext == "vws" {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        for entry in std::fs::read_dir(parent).map_err(BioFormatsError::Io)? {
            let entry = entry.map_err(BioFormatsError::Io)?;
            let entry_path = entry.path();
            let entry_name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if entry_path.is_file() && entry_name.ends_with(".pst") {
                pixel_files.push(entry_path);
            } else if entry_path.is_dir()
                && entry_name.ends_with(".pst")
                && (stem.is_empty() || entry_name.starts_with(&stem))
            {
                for sub in std::fs::read_dir(&entry_path).map_err(BioFormatsError::Io)? {
                    let sub = sub.map_err(BioFormatsError::Io)?;
                    let sub_path = sub.path();
                    if sub_path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("pst"))
                        .unwrap_or(false)
                    {
                        pixel_files.push(sub_path);
                    }
                }
            }
        }
    }

    pixel_files.sort();
    let mut series = Vec::new();
    for pixel_path in pixel_files {
        let inf_path = pixel_path.with_extension("inf");
        let meta = load_tillvision_inf(&inf_path)?;
        let plane_bytes = tillvision_plane_bytes(&meta)?;
        let expected = plane_bytes
            .checked_mul(meta.image_count as usize)
            .ok_or_else(|| BioFormatsError::Format("TillVision pixel size overflows".into()))?;
        let actual = std::fs::metadata(&pixel_path)
            .map_err(BioFormatsError::Io)?
            .len() as usize;
        if actual < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "TillVision PST pixel payload is shorter than declared ({actual} < {expected})"
            )));
        }
        series.push(TillVisionSeries { pixel_path, meta });
    }
    Ok(series)
}

fn load_tillvision_inf(path: &Path) -> Result<ImageMetadata> {
    let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('[') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            values.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let int_value = |key: &str| -> Result<u32> {
        values
            .get(&key.to_ascii_lowercase())
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!("TillVision INF missing {key}"))
            })?
            .parse::<u32>()
            .map_err(|_| {
                BioFormatsError::UnsupportedFormat(format!("TillVision INF invalid {key}"))
            })
    };

    let size_x = int_value("Width")?;
    let size_y = int_value("Height")?;
    let size_c = int_value("Bands")?.max(1);
    let size_z = int_value("Slices")?.max(1);
    let size_t = int_value("Frames")?.max(1);
    let datatype = int_value("Datatype")?;
    let pixel_type = tillvision_pixel_type(datatype)?;
    let image_count = size_z
        .checked_mul(size_t)
        .ok_or_else(|| BioFormatsError::Format("TillVision image count overflows".into()))?;

    Ok(ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb: false,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: values
            .into_iter()
            .map(|(k, v)| (format!("Info {k}"), MetadataValue::String(v)))
            .collect(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    })
}

fn tillvision_pixel_type(datatype: u32) -> Result<PixelType> {
    let signed = datatype % 2 == 1;
    let bytes = datatype / 2 + u32::from(signed);
    match (bytes, signed) {
        (1, false) => Ok(PixelType::Uint8),
        (1, true) => Ok(PixelType::Int8),
        (2, false) => Ok(PixelType::Uint16),
        (2, true) => Ok(PixelType::Int16),
        (4, false) => Ok(PixelType::Uint32),
        (4, true) => Ok(PixelType::Int32),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "TillVision datatype {datatype} is not supported"
        ))),
    }
}

fn tillvision_plane_bytes(meta: &ImageMetadata) -> Result<usize> {
    meta.size_x
        .checked_mul(meta.size_y)
        .and_then(|px| px.checked_mul(meta.size_c))
        .and_then(|samples| samples.checked_mul(meta.pixel_type.bytes_per_sample() as u32))
        .map(|n| n as usize)
        .ok_or_else(|| BioFormatsError::Format("TillVision plane size overflows".into()))
}
