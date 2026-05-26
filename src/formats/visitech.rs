//! Visitech spinning disk reader.
//!
//! Handles .xys (binary coordinate file) and .html (index file) extensions.
//! Width/Height are scanned from the first 4096 bytes of .xys files looking
//! for `Width=N` / `Height=N` text patterns.  Falls back to 512×512.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

pub struct VisitechReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    image_files: Vec<PathBuf>,
}

impl VisitechReader {
    pub fn new() -> Self {
        VisitechReader {
            path: None,
            meta: None,
            image_files: Vec::new(),
        }
    }
}

impl Default for VisitechReader {
    fn default() -> Self {
        Self::new()
    }
}

fn scan_width_height(data: &[u8]) -> (u32, u32) {
    let text = std::str::from_utf8(&data[..data.len().min(4096)]).unwrap_or("");
    let mut width = 512u32;
    let mut height = 512u32;
    for token in text.split(|c: char| !c.is_alphanumeric() && c != '=' && c != '-') {
        if let Some(val) = token.strip_prefix("Width=") {
            if let Ok(v) = val.parse() {
                width = v;
            }
        } else if let Some(val) = token.strip_prefix("Height=") {
            if let Ok(v) = val.parse() {
                height = v;
            }
        }
    }
    // Also try line-by-line
    for line in text.lines() {
        let line = line.trim();
        if let Some(rest) = line.strip_prefix("Width=") {
            if let Ok(v) = rest.trim().parse() {
                width = v;
            }
        } else if let Some(rest) = line.strip_prefix("Height=") {
            if let Ok(v) = rest.trim().parse() {
                height = v;
            }
        }
    }
    (width, height)
}

fn collect_companion_tiffs(path: &Path) -> Vec<PathBuf> {
    let dir = path.parent().unwrap_or(Path::new("."));
    let mut files: Vec<PathBuf> = std::fs::read_dir(dir)
        .ok()
        .into_iter()
        .flat_map(|rd| rd.filter_map(|e| e.ok()))
        .map(|e| e.path())
        .filter(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
                .unwrap_or(false)
        })
        .collect();
    files.sort();
    files
}

fn parse_visitech(path: &Path) -> Result<(ImageMetadata, Vec<PathBuf>)> {
    let data = std::fs::read(path).unwrap_or_default();
    let (width, height) = scan_width_height(&data);

    let image_files = collect_companion_tiffs(path);
    if image_files.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "Visitech XYS does not have any companion TIFF image files".into(),
        ));
    }
    let image_count = image_files.len() as u32;

    Ok((
        ImageMetadata {
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
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        },
        image_files,
    ))
}

impl FormatReader for VisitechReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("xys"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, image_files) = parse_visitech(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.image_files = image_files;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.image_files.clear();
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
        let tiff_path = self.image_files[plane_index as usize % self.image_files.len()].clone();
        let mut tiff = crate::tiff::TiffReader::new();
        tiff.set_id(&tiff_path)?;
        tiff.open_bytes(0)
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
