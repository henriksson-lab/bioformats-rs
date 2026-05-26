//! Camera and RAW format readers — PCO, Bio-Rad GEL, Li-Cor L2D, and more.
//!
//! Includes three binary readers with partial metadata parsing (PcoRawReader,
//! BioRadGelReader, L2dReader) and several extension-only placeholder readers.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
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
/// 76-byte header; width at offset 10 (u16 BE), height at offset 12 (u16 BE).
/// Pixel data at offset 76 as 16-bit big-endian values.
pub struct BioRadGelReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl BioRadGelReader {
    pub fn new() -> Self {
        BioRadGelReader {
            path: None,
            meta: None,
        }
    }
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

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = std::fs::File::open(path).map_err(|e| BioFormatsError::Io(e))?;
        let file_size = f.metadata().map_err(BioFormatsError::Io)?.len();
        let mut header = [0u8; 76];
        let n = f.read(&mut header).map_err(|e| BioFormatsError::Io(e))?;
        let (w, h) = if n >= 14 {
            let w = u16::from_be_bytes([header[10], header[11]]) as u32;
            let h = u16::from_be_bytes([header[12], header[13]]) as u32;
            if w == 0 || h == 0 || w > 32768 || h > 32768 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Bio-Rad GEL header contains invalid image dimensions".into(),
                ));
            } else {
                (w, h)
            }
        } else {
            return Err(BioFormatsError::UnsupportedFormat(
                "Bio-Rad GEL header is too short to contain dimensions".into(),
            ));
        };
        let expected = 76u64 + w as u64 * h as u64 * 2;
        if file_size < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Bio-Rad GEL file is too short for declared dimensions {w}x{h}"
            )));
        }
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            is_little_endian: false,
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
        f.seek(SeekFrom::Start(76))
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
        crop_full_plane("Bio-Rad GEL", &full, meta, 1, _x, _y, w, h)
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
/// CR2 files are valid TIFF files; this reader delegates to `TiffReader`.
pub struct CanonRawReader {
    inner: crate::tiff::TiffReader,
}

impl CanonRawReader {
    pub fn new() -> Self {
        CanonRawReader {
            inner: crate::tiff::TiffReader::new(),
        }
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

// ---------------------------------------------------------------------------
// 5. Hasselblad Imacon — TIFF wrapper
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Hasselblad Imacon format reader (`.fff`).
    ///
    /// Imacon files are TIFF-based; delegates to `TiffReader`.
    pub struct ImaconReader;
    extensions: ["fff"];
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
// 7. Image Pro Workspace — TIFF wrapper
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Image Pro Workspace format reader (`.ipw`).
    ///
    /// IPW files are TIFF-based; delegates to `TiffReader`.
    pub struct IpwReader;
    extensions: ["ipw"];
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
