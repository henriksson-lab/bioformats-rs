//! EPS/PostScript format reader.
//!
//! PostScript cannot be rendered to pixels without a full interpreter.

use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

pub struct EpsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl EpsReader {
    pub fn new() -> Self {
        EpsReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for EpsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for EpsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("eps") | Some("epsi") | Some("ps"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 4 {
            return false;
        }
        // Must start with "%!" and contain "PS" in first 32 bytes
        let starts = header.starts_with(b"%!");
        let window = &header[..header.len().min(32)];
        let has_ps = window.windows(2).any(|w| w == b"PS");
        starts && has_ps
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "EPS/PostScript rasterization requires a PostScript interpreter and is not implemented"
                .into(),
        ))
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
        self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Err(BioFormatsError::UnsupportedFormat(
            "EPS/PostScript rasterization requires a PostScript interpreter and is not implemented"
                .into(),
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
        let _ = (plane_index, x, y, w, h);
        Err(BioFormatsError::UnsupportedFormat(
            "EPS/PostScript rasterization requires a PostScript interpreter and is not implemented"
                .into(),
        ))
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

use crate::common::writer::FormatWriter;
use std::io::Write;

pub struct EpsWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
}

impl EpsWriter {
    pub fn new() -> Self {
        EpsWriter {
            path: None,
            meta: None,
            planes: Vec::new(),
        }
    }
}

impl Default for EpsWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for EpsWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
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
                "EPS writer supports grayscale (1) or RGB (3), got spp={}",
                spp
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
            writeln!(
                f,
                "{{currentfile {} string readhexstring pop}}",
                row_bytes * 2
            )
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
            writeln!(
                f,
                "{{currentfile {} string readhexstring pop}}",
                row_bytes * 2
            )
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

    fn can_do_stacks(&self) -> bool {
        false
    }
}
