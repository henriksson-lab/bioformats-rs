//! Camera and RAW format readers — PCO, Bio-Rad GEL, Li-Cor L2D, and more.
//!
//! Includes three binary readers with partial metadata parsing (PcoRawReader,
//! BioRadGelReader, L2dReader) and several extension-only placeholder readers.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------
fn placeholder_meta_u16() -> ImageMetadata {
    ImageMetadata {
        size_x: 512,
        size_y: 512,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type: PixelType::Uint16,
        bits_per_pixel: 16,
        image_count: 1,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    }
}

// ---------------------------------------------------------------------------
// Macro for TIFF wrapper readers
// ---------------------------------------------------------------------------
macro_rules! tiff_wrapper {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
    ) => {
        $(#[$attr])*
        pub struct $name {
            inner: crate::tiff::TiffReader,
        }

        impl $name {
            pub fn new() -> Self {
                $name { inner: crate::tiff::TiffReader::new() }
            }
        }

        impl Default for $name {
            fn default() -> Self { Self::new() }
        }

        impl FormatReader for $name {
            fn is_this_type_by_name(&self, path: &Path) -> bool {
                let ext = path.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase());
                matches!(ext.as_deref(), $(Some($ext))|+)
            }

            fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

            fn set_id(&mut self, path: &Path) -> Result<()> {
                self.inner.set_id(path)
            }

            fn close(&mut self) -> Result<()> {
                self.inner.close()
            }

            fn series_count(&self) -> usize {
                self.inner.series_count()
            }

            fn set_series(&mut self, s: usize) -> Result<()> {
                self.inner.set_series(s)
            }

            fn series(&self) -> usize {
                self.inner.series()
            }

            fn metadata(&self) -> &ImageMetadata {
                self.inner.metadata()
            }

            fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
                self.inner.open_bytes(p)
            }

            fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
                self.inner.open_bytes_region(p, x, y, w, h)
            }

            fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
                self.inner.open_thumb_bytes(p)
            }

            fn resolution_count(&self) -> usize {
                self.inner.resolution_count()
            }

            fn set_resolution(&mut self, level: usize) -> Result<()> {
                self.inner.set_resolution(level)
            }
        }
    };
}

// ---------------------------------------------------------------------------
// 1. PCO B16 raw camera file
// ---------------------------------------------------------------------------
/// PCO camera raw B16 binary format (`.b16`).
///
/// Header is 216 bytes; width at offset 4 (u16 LE), height at offset 6 (u16 LE).
/// Pixel data starts at offset 216 as 16-bit little-endian grayscale values.
pub struct PcoRawReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl PcoRawReader {
    pub fn new() -> Self {
        PcoRawReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for PcoRawReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PcoRawReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("b16")
        )
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = std::fs::File::open(path).map_err(|e| BioFormatsError::Io(e))?;
        let file_size = f.metadata().map_err(BioFormatsError::Io)?.len();
        let mut header = [0u8; 216];
        let n = f.read(&mut header).map_err(|e| BioFormatsError::Io(e))?;
        let (w, h) = if n >= 8 {
            let w = u16::from_le_bytes([header[4], header[5]]) as u32;
            let h = u16::from_le_bytes([header[6], header[7]]) as u32;
            if w == 0 || h == 0 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "PCO B16 header contains zero image dimensions".into(),
                ));
            } else {
                (w, h)
            }
        } else {
            return Err(BioFormatsError::UnsupportedFormat(
                "PCO B16 header is too short to contain dimensions".into(),
            ));
        };
        let expected = 216u64 + w as u64 * h as u64 * 2;
        if file_size < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "PCO B16 file is too short for declared dimensions {w}x{h}"
            )));
        }
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            is_little_endian: true,
            ..placeholder_meta_u16()
        });
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
        let n_bytes = meta.size_x as usize * meta.size_y as usize * 2;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(|e| BioFormatsError::Io(e))?;
        f.seek(SeekFrom::Start(216))
            .map_err(|e| BioFormatsError::Io(e))?;
        let mut buf = vec![0u8; n_bytes];
        f.read_exact(&mut buf).map_err(|e| BioFormatsError::Io(e))?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("PCO B16", &full, meta, 1, _x, _y, w, h)
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

// ---------------------------------------------------------------------------
// 2. Bio-Rad GEL phosphor imager (.1sc)
// ---------------------------------------------------------------------------
/// Bio-Rad GEL phosphor imager format (`.1sc`).
///
/// Port of BioRadGelReader.java: magic 0xafaf, chunk-walks from offsets
/// START_OFFSET (160) / BASE_OFFSET (352), reads bpp (2 or 4 bytes, the latter
/// being FLOAT), and a dynamic pixel offset relative to PIXEL_OFFSET (59654).
pub struct BioRadGelReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Whether the on-disk values are little-endian ("Intel Format").
    little_endian: bool,
    /// Java `diff = BASE_OFFSET - baseFP`, used to pick the pixel offset.
    diff: i64,
}

const BRG_MAGIC: u16 = 0xafaf;
const BRG_PIXEL_OFFSET: u64 = 59654;
const BRG_START_OFFSET: u64 = 160;
const BRG_BASE_OFFSET: i64 = 352;

impl BioRadGelReader {
    pub fn new() -> Self {
        BioRadGelReader {
            path: None,
            meta: None,
            little_endian: false,
            diff: 0,
        }
    }

    /// Compute the seek position for the pixel data, mirroring openBytes() in
    /// BioRadGelReader.java. Returns None when no special offset applies and the
    /// caller should fall back to (file_len - plane_size).
    fn pixel_seek(&self, f: &mut std::fs::File, plane_size: u64, file_len: u64) -> Result<u64> {
        if BRG_PIXEL_OFFSET + plane_size < file_len {
            if self.diff < 0 {
                let mut pos = 0x379d1u64;
                if pos + plane_size > file_len {
                    pos = BRG_PIXEL_OFFSET + 62;
                }
                Ok(pos)
            } else if self.diff == 0 {
                Ok(BRG_PIXEL_OFFSET)
            } else if file_len - plane_size > 61000 {
                // Scan backwards for the "scn0x" marker starting near
                // PIXEL_OFFSET - 196, then skip a variable metadata block.
                let mut pos = BRG_PIXEL_OFFSET - 196;
                loop {
                    f.seek(SeekFrom::Start(pos)).map_err(BioFormatsError::Io)?;
                    let mut s = [0u8; 5];
                    f.read_exact(&mut s).map_err(BioFormatsError::Io)?;
                    if &s == b"scn0x" {
                        break;
                    }
                    // back up 4 from the post-read position (== pos + 5 - 4)
                    pos = (pos + 5) - 4;
                }
                let mut p = pos + 5; // after reading "scn0x"
                p += 69;
                f.seek(SeekFrom::Start(p)).map_err(BioFormatsError::Io)?;
                let mut check = [0u8; 1];
                f.read_exact(&mut check).map_err(BioFormatsError::Io)?;
                p += 1;
                p += 19;
                f.seek(SeekFrom::Start(p)).map_err(BioFormatsError::Io)?;
                if check[0] != 0 {
                    let extra = read_i16(f, self.little_endian)? as i64 - 2;
                    p += 2;
                    p += extra.max(0) as u64;
                    f.seek(SeekFrom::Start(p)).map_err(BioFormatsError::Io)?;
                }
                let len = read_i16(f, self.little_endian)? as i64;
                p += 2;
                p += len.max(0) as u64;
                p += 32;
                Ok(p)
            } else {
                Ok(file_len - plane_size)
            }
        } else {
            Ok(file_len - plane_size)
        }
    }
}

fn read_i16(f: &mut std::fs::File, little_endian: bool) -> Result<i16> {
    let mut b = [0u8; 2];
    f.read_exact(&mut b).map_err(BioFormatsError::Io)?;
    Ok(if little_endian {
        i16::from_le_bytes(b)
    } else {
        i16::from_be_bytes(b)
    })
}

fn read_i32(f: &mut std::fs::File, little_endian: bool) -> Result<i32> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b).map_err(BioFormatsError::Io)?;
    Ok(if little_endian {
        i32::from_le_bytes(b)
    } else {
        i32::from_be_bytes(b)
    })
}

impl Default for BioRadGelReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for BioRadGelReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("1sc")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Magic: first big-endian short == 0xafaf.
        header.len() >= 2 && u16::from_be_bytes([header[0], header[1]]) == BRG_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let file_size = f.metadata().map_err(BioFormatsError::Io)?.len();

        // Reject files too small to hold the 48-byte header and the metadata
        // chunk table that begins at START_OFFSET, instead of leaking an Io EOF.
        if file_size < BRG_START_OFFSET + 4 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Bio-Rad GEL file is too short".into(),
            ));
        }

        // Header begins with a 48-byte string; "Intel Format" => little-endian.
        let mut head48 = [0u8; 48];
        f.read_exact(&mut head48).map_err(BioFormatsError::Io)?;
        let check = String::from_utf8_lossy(&head48);
        let mut little_endian = check.contains("Intel Format");

        // Walk metadata chunks from START_OFFSET until code 0x81 is found.
        f.seek(SeekFrom::Start(BRG_START_OFFSET))
            .map_err(BioFormatsError::Io)?;
        let mut code_found = false;
        let mut skip: i64 = 0;
        let mut base_fp: i64 = 0;
        // Guard against runaway loops on malformed input.
        let mut iterations = 0u32;
        while !code_found {
            iterations += 1;
            if iterations > 100_000 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Bio-Rad GEL: chunk walk did not find code 0x81".into(),
                ));
            }
            let code = read_i16(&mut f, little_endian)?;
            if code == 0x81 {
                code_found = true;
            }
            let length = read_i16(&mut f, little_endian)?;

            f.seek(SeekFrom::Current(2 + 2 * length as i64))
                .map_err(BioFormatsError::Io)?;
            if code_found {
                let fp = f.stream_position().map_err(BioFormatsError::Io)? as i64;
                base_fp = fp + 2;
                if length > 1 {
                    f.seek(SeekFrom::Current(-2)).map_err(BioFormatsError::Io)?;
                }
                skip = read_i32(&mut f, little_endian)? as i64 - 32;
            } else if length == 1 {
                f.seek(SeekFrom::Current(12)).map_err(BioFormatsError::Io)?;
            } else if length == 2 {
                f.seek(SeekFrom::Current(10)).map_err(BioFormatsError::Io)?;
            }
        }

        self.diff = BRG_BASE_OFFSET - base_fp;
        skip += self.diff;

        // Seek to baseFP + skip and read dimensions + bpp.
        let dims_pos = (base_fp + skip).max(0) as u64;
        f.seek(SeekFrom::Start(dims_pos)).map_err(BioFormatsError::Io)?;

        let mut size_x = (read_i16(&mut f, little_endian)? as u16) as u32;
        let mut size_y = (read_i16(&mut f, little_endian)? as u16) as u32;
        if (size_x as u64) * (size_y as u64) > file_size {
            // Retry as little-endian, re-reading the two shorts.
            little_endian = true;
            f.seek(SeekFrom::Current(-4)).map_err(BioFormatsError::Io)?;
            size_x = read_i16(&mut f, little_endian)? as u32;
            size_y = read_i16(&mut f, little_endian)? as u32;
        }
        f.seek(SeekFrom::Current(2)).map_err(BioFormatsError::Io)?; // skip 2

        let bpp = read_i16(&mut f, little_endian)?;
        // pixelTypeFromBytes(bpp, signed=false, fp=false): 2 -> Uint16, 4 -> Uint32.
        // Java uses fp=false here; 4-byte support is FLOAT per the GEL spec, but
        // the reader declares an integer type. Follow Java: unsigned integer.
        let (pixel_type, bits) = match bpp {
            4 => (PixelType::Uint32, 32u8),
            _ => (PixelType::Uint16, 16u8),
        };

        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Bio-Rad GEL: invalid image dimensions".into(),
            ));
        }

        self.little_endian = little_endian;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            pixel_type,
            bits_per_pixel: bits,
            is_little_endian: little_endian,
            ..placeholder_meta_u16()
        });
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
        let bpp = meta.pixel_type.bytes_per_sample();
        let pixel = bpp * meta.size_c as usize;
        let w = meta.size_x as usize;
        let h = meta.size_y as usize;
        let plane_size = (pixel * w * h) as u64;

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();

        let seek_pos = self.pixel_seek(&mut f, plane_size, file_len)?;
        f.seek(SeekFrom::Start(seek_pos))
            .map_err(BioFormatsError::Io)?;

        // Java reads rows bottom-to-top into the destination buffer, which flips
        // the image vertically relative to disk order.
        let row_bytes = w * pixel;
        let mut buf = vec![0u8; h * row_bytes];
        for row in (0..h).rev() {
            f.read_exact(&mut buf[row * row_bytes..(row + 1) * row_bytes])
                .map_err(BioFormatsError::Io)?;
        }
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let spp = meta.size_c as usize;
        crop_full_plane("Bio-Rad GEL", &full, meta, spp, _x, _y, w, h)
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

// ---------------------------------------------------------------------------
// 3. Li-Cor L2D companion-file reader
// ---------------------------------------------------------------------------
/// Li-Cor L2D format (`.l2d`).
///
/// Java Bio-Formats stores L2D pixels in companion TIFF files listed by the
/// `.l2d` scan manifest and each scan's `.scn` metadata file.
pub struct L2dReader {
    current_id: Option<PathBuf>,
    tiffs: Vec<Vec<PathBuf>>,
    metadata: Vec<ImageMetadata>,
    current_series: usize,
    reader: crate::tiff::TiffReader,
}

impl L2dReader {
    const LICOR_MAGIC: &'static str = "LI-COR LI2D";

    pub fn new() -> Self {
        L2dReader {
            current_id: None,
            tiffs: Vec::new(),
            metadata: Vec::new(),
            current_series: 0,
            reader: crate::tiff::TiffReader::new(),
        }
    }

    fn parse_key_value_lines(text: &str) -> HashMap<String, String> {
        text.lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    return None;
                }
                let (key, value) = line.split_once('=')?;
                Some((key.trim().to_string(), value.trim().to_string()))
            })
            .collect()
    }

    fn split_list(value: &str) -> Vec<String> {
        value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn set_l2d_id(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        if !text.contains(Self::LICOR_MAGIC) {
            return Err(BioFormatsError::UnsupportedFormat(
                "Li-Cor L2D file is missing LI-COR LI2D marker".into(),
            ));
        }

        let l2d = Self::parse_key_value_lines(&text);
        let scans = l2d
            .get("ScanNames")
            .map(|v| Self::split_list(v))
            .ok_or_else(|| BioFormatsError::Format("Li-Cor L2D missing ScanNames".into()))?;
        if scans.is_empty() {
            return Err(BioFormatsError::Format(
                "Li-Cor L2D ScanNames list is empty".into(),
            ));
        }

        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tiffs = Vec::new();
        let mut metadata = Vec::new();

        for scan in scans {
            let scan_dir = parent.join(&scan);
            if !scan_dir.is_dir() {
                continue;
            }
            let scan_path = scan_dir.join(format!("{scan}.scn"));
            let scan_text = std::fs::read_to_string(&scan_path).map_err(BioFormatsError::Io)?;
            let scan_meta = Self::parse_key_value_lines(&scan_text);
            let image_names = scan_meta
                .get("ImageNames")
                .map(|v| Self::split_list(v))
                .ok_or_else(|| {
                    BioFormatsError::Format(format!("Li-Cor L2D scan {scan} missing ImageNames"))
                })?;
            if image_names.is_empty() {
                return Err(BioFormatsError::Format(format!(
                    "Li-Cor L2D scan {scan} ImageNames list is empty"
                )));
            }

            let scan_tiffs: Vec<PathBuf> = image_names
                .into_iter()
                .map(|name| scan_dir.join(name))
                .collect();
            for tiff in &scan_tiffs {
                if !tiff.is_file() {
                    return Err(BioFormatsError::Format(format!(
                        "Li-Cor L2D companion TIFF is missing: {}",
                        tiff.display()
                    )));
                }
            }

            self.reader.set_id(&scan_tiffs[0])?;
            let first = self.reader.metadata().clone();
            self.reader.close()?;

            let mut series_meta = first;
            series_meta.image_count = scan_tiffs.len() as u32;
            series_meta.size_z = 1;
            series_meta.size_t = 1;
            series_meta.size_c = scan_tiffs.len() as u32;
            series_meta.dimension_order = DimensionOrder::XYCZT;
            series_meta.series_metadata = scan_meta
                .into_iter()
                .map(|(k, v)| (k, crate::common::metadata::MetadataValue::String(v)))
                .collect();
            tiffs.push(scan_tiffs);
            metadata.push(series_meta);
        }

        if tiffs.is_empty() {
            return Err(BioFormatsError::Format(
                "Li-Cor L2D did not reference any existing scan directories".into(),
            ));
        }

        self.current_id = Some(path.to_path_buf());
        self.tiffs = tiffs;
        self.metadata = metadata;
        self.current_series = 0;
        Ok(())
    }
}

impl Default for L2dReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for L2dReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("l2d") | Some("scn")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        std::str::from_utf8(&header[..header.len().min(512)])
            .map(|s| s.contains(Self::LICOR_MAGIC))
            .unwrap_or(false)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let l2d_path = if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("l2d"))
            .unwrap_or(false)
        {
            path.to_path_buf()
        } else {
            return Err(BioFormatsError::UnsupportedFormat(
                "Li-Cor L2D grouped reads must be opened from the .l2d manifest".into(),
            ));
        };
        self.set_l2d_id(&l2d_path)
    }

    fn close(&mut self) -> Result<()> {
        self.current_id = None;
        self.tiffs.clear();
        self.metadata.clear();
        self.current_series = 0;
        self.reader.close()?;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.metadata.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.metadata.len() {
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
        self.metadata
            .get(self.current_series)
            .expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metadata
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let tiff = self
            .tiffs
            .get(self.current_series)
            .and_then(|series| series.get(plane_index as usize))
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?
            .clone();
        self.reader.set_id(&tiff)?;
        let bytes = self.reader.open_bytes(0);
        self.reader.close()?;
        bytes
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .metadata
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let tiff = self
            .tiffs
            .get(self.current_series)
            .and_then(|series| series.get(plane_index as usize))
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?
            .clone();
        self.reader.set_id(&tiff)?;
        let bytes = self.reader.open_bytes_region(0, x, y, w, h);
        self.reader.close()?;
        bytes
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metadata
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 4. Canon RAW (CR2 / CRW / CR3) — TIFF wrapper
// ---------------------------------------------------------------------------
/// Canon RAW format reader (`.cr2`, `.crw`, `.cr3`).
///
/// Two code paths, mirroring Java Bio-Formats:
///
/// * **Legacy CRW** — `CanonRawReader.java` recognises raw Canon 300D `.crw`
///   files solely by a fixed file length of 18 653 760 bytes. Those have no
///   TIFF structure: bytes are byte-swapped in pairs, 12-bit samples are
///   unpacked, and the Bayer mosaic is split into an interleaved RGB plane
///   (`COLOR_MAP = {1,0,2,1}`, sizeX=4080, sizeY=3048, UINT16, 12 bpp). This
///   reader reproduces that unpacking exactly (the `ImageTools.interpolate`
///   demosaic in Java is a simple channel split, not full demosaicing).
/// * **TIFF-based** — modern CR2 files are valid TIFFs; delegate to
///   `TiffReader`.
pub struct CanonRawReader {
    inner: crate::tiff::TiffReader,
    /// Set when the file matched the legacy fixed-length CRW layout.
    legacy: Option<LegacyCrw>,
}

/// State for a legacy fixed-length Canon `.crw` file.
struct LegacyCrw {
    path: PathBuf,
    meta: ImageMetadata,
    /// Decoded interleaved RGB plane (UINT16 LE, 3 samples/pixel), cached.
    plane: Option<Vec<u8>>,
}

impl CanonRawReader {
    /// Fixed file length used by `CanonRawReader.java` to detect legacy CRW.
    const FILE_LENGTH: u64 = 18_653_760;
    const SIZE_X: usize = 4080;
    const SIZE_Y: usize = 3048;
    /// Bayer color map: index = (row%2)*2 + (col%2) -> 0=R, 1=G, 2=B.
    const COLOR_MAP: [u8; 4] = [1, 0, 2, 1];

    pub fn new() -> Self {
        CanonRawReader {
            inner: crate::tiff::TiffReader::new(),
            legacy: None,
        }
    }

    /// Decode the legacy CRW interleaved RGB plane (port of initFile + the
    /// channel split in openBytes from `CanonRawReader.java`).
    fn decode_legacy_plane(path: &Path) -> Result<Vec<u8>> {
        let mut buf = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if buf.len() < Self::FILE_LENGTH as usize {
            return Err(BioFormatsError::UnsupportedFormat(
                "Canon CRW: file shorter than expected fixed length".into(),
            ));
        }
        buf.truncate(Self::FILE_LENGTH as usize);

        // Reverse bytes in pairs.
        let mut i = 0;
        while i + 1 < buf.len() {
            buf.swap(i, i + 1);
            i += 2;
        }

        let w = Self::SIZE_X;
        let h = Self::SIZE_Y;
        let plane = w * h;
        // pix layout: 3 planar channels [R | G | B], each w*h shorts.
        let mut pix = vec![0u16; plane * 3];

        let mut next_byte = 0usize;
        let mut even = true;
        for row in 0..h {
            let row_offset = row * w;
            for col in 0..w {
                let v: u32 = if even {
                    let a = buf[next_byte] as u32;
                    next_byte += 1;
                    let b = buf[next_byte] as u32;
                    (a << 4) | ((b & 0xf0) >> 4)
                } else {
                    let a = buf[next_byte] as u32;
                    next_byte += 1;
                    let b = buf[next_byte] as u32;
                    next_byte += 1;
                    ((a & 0xf) << 8) | b
                };
                let val = (v & 0xffff) as u16;
                even = !even;

                let map_index = (row % 2) * 2 + (col % 2);
                match Self::COLOR_MAP[map_index] {
                    0 => pix[row_offset + col] = val,
                    1 => pix[plane + row_offset + col] = val,
                    2 => pix[2 * plane + row_offset + col] = val,
                    _ => {}
                }
            }
        }

        // Java: ImageTools.interpolate(pix, plane, COLOR_MAP, ...) then
        // readPlane delivers interleaved RGB. We emit interleaved RGB
        // (is_interleaved=true) UINT16 LE: for each pixel R,G,B.
        let mut out = vec![0u8; plane * 3 * 2];
        for p in 0..plane {
            let r = pix[p];
            let g = pix[plane + p];
            let b = pix[2 * plane + p];
            let base = p * 6;
            out[base..base + 2].copy_from_slice(&r.to_le_bytes());
            out[base + 2..base + 4].copy_from_slice(&g.to_le_bytes());
            out[base + 4..base + 6].copy_from_slice(&b.to_le_bytes());
        }
        Ok(out)
    }
}

impl Default for CanonRawReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for CanonRawReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("cr2") | Some("crw") | Some("cr3"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Legacy detection: exact fixed file length (CanonRawReader.java).
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if len == Self::FILE_LENGTH {
            let mut meta = placeholder_meta_u16();
            meta.size_x = Self::SIZE_X as u32;
            meta.size_y = Self::SIZE_Y as u32;
            meta.size_c = 3;
            meta.pixel_type = PixelType::Uint16;
            meta.bits_per_pixel = 12;
            meta.image_count = 1;
            meta.is_rgb = true;
            meta.is_interleaved = true;
            meta.dimension_order = DimensionOrder::XYCZT;
            self.legacy = Some(LegacyCrw {
                path: path.to_path_buf(),
                meta,
                plane: None,
            });
            return Ok(());
        }
        self.legacy = None;
        self.inner.set_id(path)
    }

    fn close(&mut self) -> Result<()> {
        self.legacy = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        if self.legacy.is_some() {
            1
        } else {
            self.inner.series_count()
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.legacy.is_some() {
            if s != 0 {
                return Err(BioFormatsError::SeriesOutOfRange(s));
            }
            Ok(())
        } else {
            self.inner.set_series(s)
        }
    }

    fn series(&self) -> usize {
        if self.legacy.is_some() {
            0
        } else {
            self.inner.series()
        }
    }

    fn metadata(&self) -> &ImageMetadata {
        if let Some(l) = &self.legacy {
            &l.meta
        } else {
            self.inner.metadata()
        }
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if let Some(l) = &mut self.legacy {
            if p != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(p));
            }
            if l.plane.is_none() {
                l.plane = Some(Self::decode_legacy_plane(&l.path)?);
            }
            return Ok(l.plane.clone().unwrap());
        }
        self.inner.open_bytes(p)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.legacy.is_some() {
            let full = self.open_bytes(p)?;
            let meta = self.metadata().clone();
            return crop_full_plane("Canon CRW", &full, &meta, 3, x, y, w, h);
        }
        self.inner.open_bytes_region(p, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if self.legacy.is_some() {
            let meta = self.metadata().clone();
            let tw = meta.size_x.min(256);
            let th = meta.size_y.min(256);
            let tx = (meta.size_x - tw) / 2;
            let ty = (meta.size_y - th) / 2;
            return self.open_bytes_region(p, tx, ty, tw, th);
        }
        self.inner.open_thumb_bytes(p)
    }

    fn resolution_count(&self) -> usize {
        if self.legacy.is_some() {
            1
        } else {
            self.inner.resolution_count()
        }
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.legacy.is_some() {
            if level != 0 {
                return Err(BioFormatsError::Format(format!(
                    "resolution {} out of range",
                    level
                )));
            }
            Ok(())
        } else {
            self.inner.set_resolution(level)
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Hasselblad Imacon — TIFF with private tags
// ---------------------------------------------------------------------------
/// Hasselblad Imacon format reader (`.fff`).
///
/// Ported from `ImaconReader.java` (extends `BaseTiffReader`). Imacon `.fff`
/// files are TIFFs identified by private tag 50457 (`XML_TAG`); each main IFD
/// is a separate series. The CREATOR tag (34377) carries experimenter/name/date
/// lines. Pixel reading is delegated to `TiffReader`; this reader adds the
/// tag-based detection and metadata parsing.
pub struct ImaconReader {
    inner: crate::tiff::TiffReader,
    meta: Option<ImageMetadata>,
}

impl ImaconReader {
    const XML_TAG: u16 = 50457;
    const CREATOR_TAG: u16 = 34377;

    pub fn new() -> Self {
        ImaconReader {
            inner: crate::tiff::TiffReader::new(),
            meta: None,
        }
    }
}

impl Default for ImaconReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ImaconReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("fff")
        )
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Java requires the XML_TAG in the first IFD; bytes alone insufficient.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;

        let first = self
            .inner
            .ifd(0)
            .ok_or_else(|| BioFormatsError::UnsupportedFormat("Imacon: no IFD".into()))?;
        if first.get(Self::XML_TAG).is_none() {
            return Err(BioFormatsError::UnsupportedFormat(
                "Imacon: TIFF is missing the XML tag (50457)".into(),
            ));
        }

        let mut meta = self.inner.metadata().clone();
        meta.series_metadata
            .insert("format".into(), MetadataValue::String("Imacon".into()));

        // CREATOR_TAG: newline-delimited; Java reads experimenter (line 4),
        // image name (line 6), creation date (lines 8 + 10).
        if let Some(creator) = first.get_str(Self::CREATOR_TAG) {
            let lines: Vec<&str> = creator.split('\n').collect();
            if lines.len() > 4 {
                meta.series_metadata.insert(
                    "Experimenter".into(),
                    MetadataValue::String(lines[4].trim().to_string()),
                );
            }
            if lines.len() > 6 {
                meta.series_metadata.insert(
                    "ImageName".into(),
                    MetadataValue::String(lines[6].trim().to_string()),
                );
            }
            if lines.len() > 8 {
                let mut date = lines[8].trim().to_string();
                if lines.len() > 10 {
                    date.push(' ');
                    date.push_str(lines[10].trim());
                }
                meta.series_metadata
                    .insert("CreationDate".into(), MetadataValue::String(date));
            }
        }

        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
}

// ---------------------------------------------------------------------------
// 6. Santa Barbara Instrument Group — FITS wrapper
// ---------------------------------------------------------------------------
/// Santa Barbara Instrument Group reader (`.fts`).
///
/// SBIG .fts files use the FITS format; this reader delegates to `FitsReader`.
pub struct SbigReader {
    inner: crate::formats::fits::FitsReader,
}

impl SbigReader {
    pub fn new() -> Self {
        SbigReader {
            inner: crate::formats::fits::FitsReader::new(),
        }
    }
}

impl Default for SbigReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SbigReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("fts"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
}

// ---------------------------------------------------------------------------
// 7. Image-Pro Workspace — OLE2 compound document with embedded TIFFs
// ---------------------------------------------------------------------------
/// Image-Pro Workspace format reader (`.ipw`).
///
/// Ported from `IPWReader.java`. An IPW file is an OLE2/Compound Document
/// (magic `0xd0cf11e0`), NOT a plain TIFF. Each image plane is stored as an
/// embedded `ImageTIFF` stream; an `ImageInfo` stream carries a text
/// description with `channels`/`slices`/`frames` counts. This reader uses the
/// `cfb` crate to enumerate streams, parses dimensions from the first
/// embedded TIFF, and reads each plane by extracting its `ImageTIFF` stream
/// to a temporary file and delegating to `TiffReader` (the in-tree TIFF
/// reader is path-based).
pub struct IpwReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Embedded TIFF stream paths, ordered by plane index.
    image_streams: Vec<String>,
}

impl IpwReader {
    /// OLE2/CFB magic bytes (D0 CF 11 E0).
    const MAGIC: [u8; 4] = [0xd0, 0xcf, 0x11, 0xe0];

    pub fn new() -> Self {
        IpwReader {
            path: None,
            meta: None,
            image_streams: Vec::new(),
        }
    }

    /// Extract an embedded stream to a temp file and run a `TiffReader` op.
    fn read_embedded_tiff(
        &self,
        stream_path: &str,
        op: impl FnOnce(&mut crate::tiff::TiffReader) -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let (mut reader, tmp) = self.open_embedded_tiff(stream_path)?;
        let result = op(&mut reader);
        reader.close().ok();
        std::fs::remove_file(&tmp).ok();
        result
    }

    /// Extract an embedded stream to a temp file, returning an initialised
    /// `TiffReader` plus the temp path to clean up.
    fn open_embedded_tiff(
        &self,
        stream_path: &str,
    ) -> Result<(crate::tiff::TiffReader, PathBuf)> {
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut comp =
            cfb::open(path).map_err(|e| BioFormatsError::Format(format!("IPW CFB open: {e}")))?;
        let mut stream = comp
            .open_stream(stream_path)
            .map_err(|e| BioFormatsError::Format(format!("IPW stream {stream_path}: {e}")))?;
        let mut data = Vec::new();
        stream.read_to_end(&mut data).map_err(BioFormatsError::Io)?;
        drop(stream);
        drop(comp);

        let tmp = std::env::temp_dir().join(format!(
            "bioformats_ipw_{}_{}.tif",
            std::process::id(),
            stream_path.replace(['/', '\\', ' '], "_")
        ));
        std::fs::write(&tmp, &data).map_err(BioFormatsError::Io)?;
        let mut reader = crate::tiff::TiffReader::new();
        match reader.set_id(&tmp) {
            Ok(()) => Ok((reader, tmp)),
            Err(e) => {
                std::fs::remove_file(&tmp).ok();
                Err(e)
            }
        }
    }
}

impl Default for IpwReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse the IPW `ImageInfo` description into (sizeC, sizeZ, sizeT).
fn parse_ipw_image_info(text: &str) -> (Option<u32>, Option<u32>, Option<u32>) {
    let (mut c, mut z, mut t) = (None, None, None);
    for line in text.split('\n') {
        if let Some((label, data)) = line.split_once('=') {
            match label.trim() {
                "channels" => c = data.trim().parse().ok(),
                "slices" => z = data.trim().parse().ok(),
                "frames" => t = data.trim().parse().ok(),
                _ => {}
            }
        }
    }
    (c, z, t)
}

impl FormatReader for IpwReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("ipw")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4 && header[..4] == Self::MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut comp =
            cfb::open(path).map_err(|e| BioFormatsError::Format(format!("IPW CFB open: {e}")))?;

        // Enumerate streams. ImageTIFF streams hold pixels; the numeric
        // storage just above the stream is the plane index (Java parses it
        // from the path, defaulting to 0 directly under Root Entry).
        let entries: Vec<(String, bool)> = comp
            .walk()
            .map(|e| (e.path().to_string_lossy().to_string(), e.is_stream()))
            .collect();

        let mut image_streams: Vec<(u32, String)> = Vec::new();
        let mut info_stream: Option<String> = None;
        for (raw_path, is_stream) in &entries {
            if !is_stream {
                continue;
            }
            let norm = raw_path.replace('\\', "/");
            let base = norm.rsplit('/').next().unwrap_or("");
            if base == "ImageTIFF" {
                let parts: Vec<&str> = norm.trim_matches('/').split('/').collect();
                let idx = if parts.len() >= 2 {
                    parts[parts.len() - 2]
                        .chars()
                        .filter(|c| c.is_ascii_digit())
                        .collect::<String>()
                        .parse::<u32>()
                        .unwrap_or(0)
                } else {
                    0
                };
                image_streams.push((idx, raw_path.clone()));
            } else if base == "ImageInfo" {
                info_stream = Some(raw_path.clone());
            }
        }

        if image_streams.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "IPW: no embedded ImageTIFF streams found".into(),
            ));
        }
        image_streams.sort_by_key(|(idx, _)| *idx);
        let image_count = image_streams.len() as u32;
        let ordered: Vec<String> = image_streams.into_iter().map(|(_, p)| p).collect();

        // Parse ImageInfo for axis sizes.
        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "format".into(),
            MetadataValue::String("Image-Pro Workspace".into()),
        );
        let (mut size_c, mut size_z, mut size_t) = (None, None, None);
        if let Some(info_path) = &info_stream {
            if let Ok(mut s) = comp.open_stream(info_path) {
                let mut buf = Vec::new();
                if s.read_to_end(&mut buf).is_ok() {
                    let text = String::from_utf8_lossy(&buf);
                    series_metadata.insert(
                        "Image Description".into(),
                        MetadataValue::String(text.trim().to_string()),
                    );
                    let (c, z, t) = parse_ipw_image_info(&text);
                    size_c = c;
                    size_z = z;
                    size_t = t;
                }
            }
        }
        drop(comp);

        self.path = Some(path.to_path_buf());
        self.image_streams = ordered;

        // Read first embedded TIFF for X/Y/pixel type.
        let first_stream = self.image_streams[0].clone();
        let (mut tiff, tmp) = self.open_embedded_tiff(&first_stream)?;
        let first_meta = tiff.metadata().clone();
        tiff.close().ok();
        std::fs::remove_file(&tmp).ok();

        let mut size_z = size_z.unwrap_or(1).max(1);
        let size_c = size_c.unwrap_or(1).max(1);
        let size_t = size_t.unwrap_or(1).max(1);
        // Java: if axis product == 1 but multiple planes exist, treat as Z.
        if size_z * size_c * size_t == 1 && image_count != 1 {
            size_z = image_count;
        }

        let meta = ImageMetadata {
            size_x: first_meta.size_x,
            size_y: first_meta.size_y,
            size_z,
            size_c,
            size_t,
            pixel_type: first_meta.pixel_type,
            bits_per_pixel: first_meta.bits_per_pixel,
            image_count,
            dimension_order: if first_meta.is_rgb {
                DimensionOrder::XYCZT
            } else {
                DimensionOrder::XYZCT
            },
            is_rgb: first_meta.is_rgb,
            is_interleaved: first_meta.is_interleaved,
            is_indexed: first_meta.is_indexed,
            is_little_endian: first_meta.is_little_endian,
            resolution_count: 1,
            series_metadata,
            lookup_table: first_meta.lookup_table.clone(),
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.image_streams.clear();
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
        let stream = self.image_streams[plane_index as usize].clone();
        self.read_embedded_tiff(&stream, |r| r.open_bytes(0))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let stream = self.image_streams[plane_index as usize].clone();
        self.read_embedded_tiff(&stream, move |r| r.open_bytes_region(0, x, y, w, h))
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

// ---------------------------------------------------------------------------
// 8. Photoshop-annotated TIFF — TIFF wrapper
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Photoshop-annotated TIFF format (`.tif`).
    pub struct PhotoshopTiffReader;
    extensions: ["tif"];
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::metadata::MetadataValue;
    use crate::common::writer::FormatWriter;
    use crate::tiff::TiffWriter;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "bioformats_camera2_{name}_{}_{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_u8_tiff(path: &Path, pixels: &[u8], width: u32, height: u32) {
        let mut meta = ImageMetadata::default();
        meta.size_x = width;
        meta.size_y = height;
        meta.pixel_type = PixelType::Uint8;
        meta.bits_per_pixel = 8;
        meta.image_count = 1;

        let mut writer = TiffWriter::new();
        writer.set_metadata(&meta).unwrap();
        writer.set_id(path).unwrap();
        writer.save_bytes(0, pixels).unwrap();
        writer.close().unwrap();
    }

    fn write_l2d_dataset(root: &Path) -> PathBuf {
        let scan_dir = root.join("ScanA");
        fs::create_dir_all(&scan_dir).unwrap();
        write_u8_tiff(&scan_dir.join("ch1.tif"), &[1, 2, 3, 4, 5, 6], 3, 2);
        write_u8_tiff(&scan_dir.join("ch2.tif"), &[7, 8, 9, 10, 11, 12], 3, 2);
        fs::write(
            scan_dir.join("ScanA.scn"),
            "ImageNames=ch1.tif, ch2.tif\nComments=synthetic\nScanChannels=700,800\n",
        )
        .unwrap();
        let l2d = root.join("sample.l2d");
        fs::write(&l2d, "FileType=LI-COR LI2D\nScanNames=ScanA\n").unwrap();
        l2d
    }

    #[test]
    fn l2d_delegates_planes_to_companion_tiffs() {
        let root = temp_dir("l2d_planes");
        let l2d = write_l2d_dataset(&root);
        let mut reader = L2dReader::new();
        reader.set_id(&l2d).unwrap();

        let meta = reader.metadata();
        assert_eq!(
            (meta.size_x, meta.size_y, meta.size_c, meta.image_count),
            (3, 2, 2, 2)
        );
        assert_eq!(meta.dimension_order, DimensionOrder::XYCZT);
        match meta.series_metadata.get("Comments") {
            Some(MetadataValue::String(value)) => assert_eq!(value, "synthetic"),
            other => panic!("unexpected Comments metadata: {other:?}"),
        }
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![7, 8, 9, 10, 11, 12]);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn l2d_delegates_regions_to_companion_tiffs() {
        let root = temp_dir("l2d_region");
        let l2d = write_l2d_dataset(&root);
        let mut reader = L2dReader::new();
        reader.set_id(&l2d).unwrap();

        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 2, 2).unwrap(),
            vec![8, 9, 11, 12]
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn l2d_rejects_manifest_without_magic() {
        let root = temp_dir("l2d_magic");
        let l2d = root.join("bad.l2d");
        fs::write(&l2d, "ScanNames=ScanA\n").unwrap();
        let err = L2dReader::new().set_id(&l2d).unwrap_err();
        assert!(
            err.to_string().contains("LI-COR LI2D"),
            "unexpected error: {err}"
        );

        fs::remove_dir_all(root).unwrap();
    }
}
