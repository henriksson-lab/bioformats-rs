//! InCell GE Healthcare HCS reader.
//!
//! Detects .xdce files or .xml files that contain "<InCell" in the first 512 bytes.
//! Parses the XML for image dimensions and companion TIFF paths.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

pub struct InCellReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    image_files: Vec<PathBuf>,
    current_plane: u32,
    tiff_reader: bioformats_tiff::TiffReader,
    tiff_loaded: bool,
}

impl InCellReader {
    pub fn new() -> Self {
        InCellReader {
            path: None,
            meta: None,
            image_files: Vec::new(),
            current_plane: 0,
            tiff_reader: bioformats_tiff::TiffReader::new(),
            tiff_loaded: false,
        }
    }
}

impl Default for InCellReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_incell_xml(path: &Path) -> Result<(ImageMetadata, Vec<PathBuf>)> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let dir = path.parent().unwrap_or(Path::new("."));

    let mut width = 512u32;
    let mut height = 512u32;
    let mut image_files: Vec<PathBuf> = Vec::new();

    // Simple text scanning for Width/Height attributes
    for line in content.lines() {
        if line.contains("Width=") && line.contains("Height=") {
            // Try to extract Width="N"
            if let Some(w) = extract_attr(line, "Width") {
                if let Ok(v) = w.parse() { width = v; }
            }
            if let Some(h) = extract_attr(line, "Height") {
                if let Ok(v) = h.parse() { height = v; }
            }
        }
        // Collect referenced image files
        for attr in &["filename", "URL", "FileName"] {
            if let Some(fname) = extract_attr(line, attr) {
                let p = dir.join(fname);
                if p.extension().and_then(|e| e.to_str())
                    .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
                    .unwrap_or(false)
                {
                    if !image_files.contains(&p) {
                        image_files.push(p);
                    }
                }
            }
        }
    }

    let image_count = (image_files.len() as u32).max(1);

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: 1,
        size_t: image_count,
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

    Ok((meta, image_files))
}

fn extract_attr<'a>(line: &'a str, attr: &str) -> Option<&'a str> {
    // Match attr="value" or attr='value'
    let search = format!("{}=\"", attr);
    if let Some(start) = line.find(&search) {
        let rest = &line[start + search.len()..];
        if let Some(end) = rest.find('"') {
            return Some(&rest[..end]);
        }
    }
    let search2 = format!("{}='", attr);
    if let Some(start) = line.find(&search2) {
        let rest = &line[start + search2.len()..];
        if let Some(end) = rest.find('\'') {
            return Some(&rest[..end]);
        }
    }
    None
}

impl FormatReader for InCellReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if matches!(ext.as_deref(), Some("xdce")) {
            return true;
        }
        // For .xml, check content
        if matches!(ext.as_deref(), Some("xml")) {
            if let Ok(data) = std::fs::read(path) {
                let snippet = std::str::from_utf8(&data[..data.len().min(512)]).unwrap_or("");
                return snippet.contains("<InCell") || snippet.contains("xdce");
            }
        }
        false
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        let snippet = std::str::from_utf8(&header[..header.len().min(512)]).unwrap_or("");
        snippet.contains("<InCell") || snippet.contains("xdce")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, image_files) = parse_incell_xml(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.image_files = image_files;
        self.tiff_loaded = false;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.image_files.clear();
        if self.tiff_loaded {
            let _ = self.tiff_reader.close();
            self.tiff_loaded = false;
        }
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

        if let Some(tiff_path) = self.image_files.get(plane_index as usize) {
            let tiff_path = tiff_path.clone();
            // Load the TIFF for this plane
            if self.tiff_loaded {
                let _ = self.tiff_reader.close();
            }
            self.tiff_reader.set_id(&tiff_path)?;
            self.tiff_loaded = true;
            self.current_plane = plane_index;
            return self.tiff_reader.open_bytes(0);
        }

        // No companion file — return blank plane
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        Ok(vec![0u8; meta.size_x as usize * meta.size_y as usize * bps])
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
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
