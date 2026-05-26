//! Bruker OPUS FTIR spectroscopy and ISS Vista FLIM format readers.

use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;

const OPUS_UNSUPPORTED: &str =
    "Bruker OPUS spectral image decoding is not implemented; refusing guessed header metadata";
const ISS_UNSUPPORTED: &str =
    "ISS Vista FLIM decoding is not implemented; refusing guessed header metadata";

// ─── Bruker OPUS ──────────────────────────────────────────────────────────────
//
// Bruker OPUS is a binary format for FTIR/Raman spectroscopy data.
// The file starts with a block directory. The magic is version-dependent:
//   byte[0] == 0x0A and byte[1] in {0x00, 0x01, 0x02} for versions 5-7.
// Spectral images are stored as 2D or 3D arrays (x, y, wavenumber).

pub struct BrukerOpusReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl BrukerOpusReader {
    pub fn new() -> Self {
        BrukerOpusReader {
            path: None,
            meta: None,
        }
    }
}
impl Default for BrukerOpusReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for BrukerOpusReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // OPUS files have numeric extensions (.0, .1, ...) or .abs, .dpt
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("abs") | Some("dpt") | Some("spa") => true,
            Some(e) => e.chars().all(|c| c.is_ascii_digit()),
            None => false,
        }
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // OPUS magic: first byte 0x0A, second byte in {0,1,2}
        header.len() >= 2 && header[0] == 0x0A && header[1] <= 0x02
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta = None;
        self.path = Some(path.to_path_buf());
        Err(BioFormatsError::UnsupportedFormat(
            OPUS_UNSUPPORTED.to_string(),
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Err(BioFormatsError::UnsupportedFormat(
            OPUS_UNSUPPORTED.to_string(),
        ))
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self.meta.as_ref().unwrap();
        let row = meta.size_x as usize * 4;
        let out_row = w as usize * 4;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * 4..x as usize * 4 + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(p, tx, ty, tw, th)
    }
}

// ─── ISS Vista FLIM ───────────────────────────────────────────────────────────
//
// ISS (formerly ISS Inc.) FLIM data files (.iss).
// Binary format with a header encoding image dimensions.

pub struct IssFlimReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl IssFlimReader {
    pub fn new() -> Self {
        IssFlimReader {
            path: None,
            meta: None,
        }
    }
}
impl Default for IssFlimReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for IssFlimReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("iss"))
            .unwrap_or(false)
    }
    fn is_this_type_by_bytes(&self, _: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta = None;
        self.path = Some(path.to_path_buf());
        Err(BioFormatsError::UnsupportedFormat(
            ISS_UNSUPPORTED.to_string(),
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Err(BioFormatsError::UnsupportedFormat(
            ISS_UNSUPPORTED.to_string(),
        ))
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self.meta.as_ref().unwrap();
        let row = meta.size_x as usize * 4;
        let out_row = w as usize * 4;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * 4..x as usize * 4 + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(p, tx, ty, tw, th)
    }
}
