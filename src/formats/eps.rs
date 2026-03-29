//! EPS/PostScript metadata reader.
//!
//! Reads BoundingBox from EPS/EPSI/PS files and returns placeholder pixel data.
//! PostScript cannot be rendered to pixels without a full interpreter.

use std::collections::HashMap;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

pub struct EpsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl EpsReader {
    pub fn new() -> Self {
        EpsReader { path: None, meta: None }
    }
}

impl Default for EpsReader {
    fn default() -> Self { Self::new() }
}

fn load_eps(path: &Path) -> Result<ImageMetadata> {
    let f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    let reader = BufReader::new(f);

    let mut width = 640u32;
    let mut height = 480u32;

    for line in reader.lines() {
        let line = line.map_err(BioFormatsError::Io)?;
        let line = line.trim();

        // Parse %%BoundingBox: llx lly urx ury
        if let Some(bbox) = line.strip_prefix("%%BoundingBox:") {
            let parts: Vec<&str> = bbox.split_whitespace().collect();
            if parts.len() >= 4 {
                if let (Ok(llx), Ok(lly), Ok(urx), Ok(ury)) = (
                    parts[0].parse::<i32>(),
                    parts[1].parse::<i32>(),
                    parts[2].parse::<i32>(),
                    parts[3].parse::<i32>(),
                ) {
                    let w = (urx - llx).max(1) as u32;
                    let h = (ury - lly).max(1) as u32;
                    width = w;
                    height = h;
                    break;
                }
            }
        }

        // Stop searching after the header section
        if !line.starts_with('%') && !line.is_empty() && !line.starts_with("%%") {
            break;
        }
    }

    Ok(ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: 8,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
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
    })
}

impl FormatReader for EpsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("eps") | Some("epsi") | Some("ps"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 4 { return false; }
        // Must start with "%!" and contain "PS" in first 32 bytes
        let starts = header.starts_with(b"%!");
        let window = &header[..header.len().min(32)];
        let has_ps = window.windows(2).any(|w| w == b"PS");
        starts && has_ps
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = load_eps(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
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
        if plane_index != 0 { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        // Placeholder: zero-filled
        Ok(vec![0u8; meta.size_x as usize * meta.size_y as usize])
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let row_bytes = meta.size_x as usize;
        let mut out = Vec::with_capacity(h as usize * w as usize);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            out.extend_from_slice(&src[x as usize..x as usize + w as usize]);
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

// ---------------------------------------------------------------------------
// EPS Writer
// ---------------------------------------------------------------------------

use std::io::Write;
use crate::common::writer::FormatWriter;

pub struct EpsWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
}

impl EpsWriter {
    pub fn new() -> Self {
        EpsWriter { path: None, meta: None, planes: Vec::new() }
    }
}

impl Default for EpsWriter {
    fn default() -> Self { Self::new() }
}

impl FormatWriter for EpsWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("eps") | Some("epsi") | Some("ps"))
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        self.meta = Some(meta.clone());
        self.planes.clear();
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta.as_ref().ok_or_else(|| {
            BioFormatsError::Format("set_metadata must be called before set_id".into())
        })?;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        if self.planes.is_empty() {
            return Err(BioFormatsError::Format("no planes written".into()));
        }

        let width = meta.size_x;
        let height = meta.size_y;
        let spp = meta.size_c as usize;

        // Only 8-bit grayscale or RGB supported
        if meta.pixel_type != PixelType::Uint8 {
            return Err(BioFormatsError::UnsupportedFormat(
                "EPS writer supports only 8-bit pixel data".into(),
            ));
        }
        if spp != 1 && spp != 3 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "EPS writer supports grayscale (1) or RGB (3), got spp={}", spp
            )));
        }

        let row_bytes = width as usize * spp;
        let bits = 8u32;
        let data = &self.planes[0];

        let mut f = std::fs::File::create(path).map_err(BioFormatsError::Io)?;

        writeln!(f, "%!PS-Adobe-3.0 EPSF-3.0").map_err(BioFormatsError::Io)?;
        writeln!(f, "%%BoundingBox: 0 0 {} {}", width, height).map_err(BioFormatsError::Io)?;
        writeln!(f, "%%EndComments").map_err(BioFormatsError::Io)?;

        if spp == 1 {
            // Grayscale: use `image` operator
            writeln!(
                f,
                "{} {} {} [{} 0 0 -{} 0 {}]",
                width, height, bits, width, height, height
            )
            .map_err(BioFormatsError::Io)?;
            writeln!(f, "{{currentfile {} string readhexstring pop}}", row_bytes * 2)
                .map_err(BioFormatsError::Io)?;
            writeln!(f, "image").map_err(BioFormatsError::Io)?;
        } else {
            // RGB: use `colorimage` operator
            writeln!(
                f,
                "{} {} {} [{} 0 0 -{} 0 {}]",
                width, height, bits, width, height, height
            )
            .map_err(BioFormatsError::Io)?;
            writeln!(f, "{{currentfile {} string readhexstring pop}}", row_bytes * 2)
                .map_err(BioFormatsError::Io)?;
            writeln!(f, "false 3 colorimage").map_err(BioFormatsError::Io)?;
        }

        // Hex-encode pixel data
        for byte in data.iter() {
            write!(f, "{:02X}", byte).map_err(BioFormatsError::Io)?;
        }
        writeln!(f).map_err(BioFormatsError::Io)?;

        writeln!(f, "showpage").map_err(BioFormatsError::Io)?;
        writeln!(f, "%%EOF").map_err(BioFormatsError::Io)?;

        self.path = None;
        self.meta = None;
        self.planes.clear();
        Ok(())
    }

    fn save_bytes(&mut self, _plane_index: u32, data: &[u8]) -> Result<()> {
        self.planes.push(data.to_vec());
        Ok(())
    }

    fn can_do_stacks(&self) -> bool { false }
}
