//! Prairie Technologies PrairieView and Leica TCS XML+TIFF series readers.
//!
//! Both formats use an XML metadata file that references companion TIFF files.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

// ── Minimal XML attribute parser ──────────────────────────────────────────────

/// Extract the value of a named attribute from an XML tag string.
/// e.g. extract_attr(`key="pixelsPerLine" value="512"`, "value") → Some("512")
fn extract_attr<'a>(text: &'a str, attr: &str) -> Option<&'a str> {
    let search = format!("{}=\"", attr);
    let start = text.find(search.as_str())? + search.len();
    let end = text[start..].find('"')? + start;
    Some(&text[start..end])
}

fn extract_attr_owned(text: &str, attr: &str) -> Option<String> {
    extract_attr(text, attr).map(|s| s.to_string())
}

// ── Prairie Technologies Reader ───────────────────────────────────────────────

pub struct PrairieReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    tiff_files: Vec<PathBuf>,
}

impl PrairieReader {
    pub fn new() -> Self {
        PrairieReader { path: None, meta: None, tiff_files: Vec::new() }
    }
}

impl Default for PrairieReader {
    fn default() -> Self { Self::new() }
}

fn parse_prairie_xml(path: &Path) -> Result<(ImageMetadata, Vec<PathBuf>)> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));

    let mut width = 512u32;
    let mut height = 512u32;
    let mut bits = 16u32;
    let mut tiff_files: Vec<PathBuf> = Vec::new();

    for line in content.lines() {
        let line = line.trim();

        // Parse PVStateValue elements
        if line.contains("PVStateValue") {
            if let Some(key) = extract_attr(line, "key") {
                if let Some(val) = extract_attr(line, "value") {
                    match key {
                        "pixelsPerLine" => { if let Ok(v) = val.parse::<u32>() { width = v; } }
                        "linesPerFrame" => { if let Ok(v) = val.parse::<u32>() { height = v; } }
                        "bitDepth" => { if let Ok(v) = val.parse::<u32>() { bits = v; } }
                        _ => {}
                    }
                }
            }
        }

        // Collect companion TIFF files from <File> elements
        if line.contains("<File") {
            if let Some(fname) = extract_attr(line, "filename") {
                let tiff_path = dir.join(fname);
                tiff_files.push(tiff_path);
            }
        }
    }

    let pixel_type = match bits {
        8 => PixelType::Uint8,
        16 => PixelType::Uint16,
        32 => PixelType::Float32,
        _ => PixelType::Uint16,
    };
    let image_count = tiff_files.len().max(1) as u32;

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: image_count,
        size_c: 1,
        size_t: 1,
        pixel_type,
        bits_per_pixel: bits as u8,
        image_count,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
    };

    Ok((meta, tiff_files))
}

impl FormatReader for PrairieReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("xml"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        let s = std::str::from_utf8(&header[..header.len().min(256)]).unwrap_or("");
        s.contains("<PVScan")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Check magic first
        let content_prefix = {
            let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
            let mut buf = vec![0u8; 256];
            use std::io::Read;
            let n = f.read(&mut buf).map_err(BioFormatsError::Io)?;
            buf[..n].to_vec()
        };
        if !self.is_this_type_by_bytes(&content_prefix) {
            return Err(BioFormatsError::Format("Not a PrairieView XML file".into()));
        }

        let (meta, tiff_files) = parse_prairie_xml(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.tiff_files = tiff_files;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.tiff_files.clear();
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if self.tiff_files.is_empty() {
            return Ok(vec![0u8; meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample()]);
        }
        let tiff_path = self.tiff_files[plane_index as usize % self.tiff_files.len()].clone();
        let mut tiff = bioformats_tiff::TiffReader::new();
        tiff.set_id(&tiff_path)?;
        tiff.open_bytes(0)
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

// ── Leica TCS Reader ──────────────────────────────────────────────────────────

pub struct LeicaTcsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    tiff_files: Vec<PathBuf>,
}

impl LeicaTcsReader {
    pub fn new() -> Self {
        LeicaTcsReader { path: None, meta: None, tiff_files: Vec::new() }
    }
}

impl Default for LeicaTcsReader {
    fn default() -> Self { Self::new() }
}

fn parse_leica_xml(path: &Path) -> Result<(ImageMetadata, Vec<PathBuf>)> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let dir = path.parent().unwrap_or_else(|| Path::new("."));

    let mut width = 512u32;
    let mut height = 512u32;
    let mut tiff_files: Vec<PathBuf> = Vec::new();

    for line in content.lines() {
        let line = line.trim();

        // Parse <Image Width="N" Height="N"> elements
        if line.contains("<Image") {
            if let Some(w) = extract_attr(line, "Width") {
                if let Ok(v) = w.parse::<u32>() { width = v; }
            }
            if let Some(h) = extract_attr(line, "Height") {
                if let Ok(v) = h.parse::<u32>() { height = v; }
            }
        }

        // Collect attachment files
        if line.contains("<Attachment") || line.contains("FileName") {
            if let Some(fname) = extract_attr_owned(line, "Name")
                .or_else(|| extract_attr_owned(line, "FileName"))
            {
                if fname.to_ascii_lowercase().ends_with(".tif")
                    || fname.to_ascii_lowercase().ends_with(".tiff")
                {
                    tiff_files.push(dir.join(&fname));
                }
            }
        }
    }

    let image_count = tiff_files.len().max(1) as u32;

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: image_count,
        size_c: 1,
        size_t: 1,
        pixel_type: PixelType::Uint16,
        bits_per_pixel: 16,
        image_count,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
    };

    Ok((meta, tiff_files))
}

impl FormatReader for LeicaTcsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("xml"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        let s = std::str::from_utf8(&header[..header.len().min(256)]).unwrap_or("");
        s.contains("<LAS") || s.contains("<LEICA")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let content_prefix = {
            let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
            let mut buf = vec![0u8; 256];
            use std::io::Read;
            let n = f.read(&mut buf).map_err(BioFormatsError::Io)?;
            buf[..n].to_vec()
        };
        if !self.is_this_type_by_bytes(&content_prefix) {
            return Err(BioFormatsError::Format("Not a Leica TCS XML file".into()));
        }

        let (meta, tiff_files) = parse_leica_xml(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.tiff_files = tiff_files;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.tiff_files.clear();
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if self.tiff_files.is_empty() {
            return Ok(vec![0u8; meta.size_x as usize * meta.size_y as usize * 2]);
        }
        let tiff_path = self.tiff_files[plane_index as usize % self.tiff_files.len()].clone();
        let mut tiff = bioformats_tiff::TiffReader::new();
        tiff.set_id(&tiff_path)?;
        tiff.open_bytes(0)
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
