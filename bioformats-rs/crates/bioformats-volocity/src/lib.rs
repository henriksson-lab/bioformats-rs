//! Volocity (.mvd2) and Nikon NIS (.nif) format readers.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

// ─── Volocity .mvd2 ───────────────────────────────────────────────────────────
//
// Volocity (PerkinElmer) stores 3D/4D microscopy data in .mvd2 files.
// The format is a proprietary binary container. Without full reverse-engineering,
// we detect by extension and return placeholder metadata.

pub struct VolocityReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl VolocityReader {
    pub fn new() -> Self { VolocityReader { path: None, meta: None } }
}
impl Default for VolocityReader { fn default() -> Self { Self::new() } }

impl FormatReader for VolocityReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("mvd2"))
            .unwrap_or(false)
    }
    fn is_this_type_by_bytes(&self, _: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Volocity MVD2".into()));
        self.meta = Some(ImageMetadata {
            size_x: 512, size_y: 512, size_z: 1, size_c: 1, size_t: 1,
            pixel_type: PixelType::Uint16, bits_per_pixel: 16, image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false, is_interleaved: false, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
        });
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> { self.path = None; self.meta = None; Ok(()) }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if p >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(p)); }
        Ok(vec![0u8; (meta.size_x * meta.size_y * 2) as usize])
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self.meta.as_ref().unwrap();
        let row = meta.size_x as usize * 2;
        let out_row = w as usize * 2;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize*2 .. x as usize*2 + out_row]);
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

// ─── Nikon NIS-Elements .nif ──────────────────────────────────────────────────
//
// Nikon NIS-Elements Image File (.nif) — TIFF-based format.
// Delegates to TiffReader for pixel data.

pub struct NikonNisReader {
    inner: bioformats_tiff::TiffReader,
}

impl NikonNisReader {
    pub fn new() -> Self { NikonNisReader { inner: bioformats_tiff::TiffReader::new() } }
}
impl Default for NikonNisReader { fn default() -> Self { Self::new() } }

impl FormatReader for NikonNisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("nif") | Some("nd2") )
        // .nd2 is already handled by bioformats-nd2, so effectively only .nif here
        && matches!(ext.as_deref(), Some("nif"))
    }
    fn is_this_type_by_bytes(&self, _: &[u8]) -> bool { false }
    fn set_id(&mut self, path: &Path) -> Result<()> { self.inner.set_id(path) }
    fn close(&mut self) -> Result<()> { self.inner.close() }
    fn series_count(&self) -> usize { self.inner.series_count() }
    fn set_series(&mut self, s: usize) -> Result<()> { self.inner.set_series(s) }
    fn series(&self) -> usize { self.inner.series() }
    fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_bytes(p) }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(p) }
    fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    fn set_resolution(&mut self, l: usize) -> Result<()> { self.inner.set_resolution(l) }
    fn resolution(&self) -> usize { self.inner.resolution() }
}
