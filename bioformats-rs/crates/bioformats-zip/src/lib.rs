//! ZIP container reader.
//!
//! Detects ZIP files by magic bytes PK\x03\x04 and extracts the first
//! supported image file to a temp directory. If the inner file is a TIFF it
//! delegates to TiffReader; otherwise it returns a 1×1 uint8 placeholder.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

/// Extensions treated as TIFF inside a ZIP.
fn is_tiff_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tif") || lower.ends_with(".tiff")
}

/// Extensions of any supported image file inside a ZIP.
fn is_image_name(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".tif")
        || lower.ends_with(".tiff")
        || lower.ends_with(".png")
        || lower.ends_with(".jpg")
        || lower.ends_with(".jpeg")
        || lower.ends_with(".bmp")
        || lower.ends_with(".mrc")
        || lower.ends_with(".fits")
        || lower.ends_with(".nrrd")
        || lower.ends_with(".ics")
        || lower.ends_with(".dcm")
        || lower.ends_with(".nii")
        || lower.ends_with(".dm3")
        || lower.ends_with(".dm4")
        || lower.ends_with(".spe")
}

fn placeholder_meta() -> ImageMetadata {
    ImageMetadata {
        size_x: 1,
        size_y: 1,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: 8,
        image_count: 1,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
    }
}

pub struct ZipReader {
    extracted_path: Option<PathBuf>,
    inner: bioformats_tiff::TiffReader,
    meta: Option<ImageMetadata>,
    is_tiff: bool,
}

impl ZipReader {
    pub fn new() -> Self {
        ZipReader {
            extracted_path: None,
            inner: bioformats_tiff::TiffReader::new(),
            meta: None,
            is_tiff: false,
        }
    }
}

impl Default for ZipReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ZipReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("zip"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4 && header[0..4] == [0x50, 0x4B, 0x03, 0x04]
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut archive = zip::ZipArchive::new(file)
            .map_err(|e| BioFormatsError::Format(format!("ZIP open error: {e}")))?;

        // Find the first image entry
        let mut found_name: Option<String> = None;
        for i in 0..archive.len() {
            if let Ok(entry) = archive.by_index(i) {
                let name = entry.name().to_string();
                if !entry.is_dir() && is_image_name(&name) {
                    found_name = Some(name);
                    break;
                }
            }
        }

        let Some(name) = found_name else {
            // No supported image found — use placeholder
            self.meta = Some(placeholder_meta());
            self.is_tiff = false;
            return Ok(());
        };

        // Extract to temp file
        let safe_name = Path::new(&name)
            .file_name()
            .unwrap_or_else(|| std::ffi::OsStr::new("extracted"))
            .to_string_lossy()
            .to_string();
        let unique = format!("bioformats_zip_{}_{}", std::process::id(), safe_name);
        let temp_path = std::env::temp_dir().join(unique);

        {
            let mut entry = archive
                .by_name(&name)
                .map_err(|e| BioFormatsError::Format(format!("ZIP entry error: {e}")))?;
            let mut buf = Vec::new();
            entry.read_to_end(&mut buf).map_err(BioFormatsError::Io)?;
            std::fs::write(&temp_path, &buf).map_err(BioFormatsError::Io)?;
        }

        self.extracted_path = Some(temp_path.clone());

        if is_tiff_name(&name) {
            // Delegate to TiffReader
            self.inner.set_id(&temp_path)?;
            self.meta = Some(self.inner.metadata().clone());
            self.is_tiff = true;
        } else {
            self.meta = Some(placeholder_meta());
            self.is_tiff = false;
        }

        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if self.is_tiff {
            let _ = self.inner.close();
        }
        if let Some(p) = self.extracted_path.take() {
            let _ = std::fs::remove_file(p);
        }
        self.meta = None;
        self.is_tiff = false;
        Ok(())
    }

    fn series_count(&self) -> usize {
        if self.is_tiff {
            self.inner.series_count()
        } else {
            1
        }
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if self.is_tiff {
            self.inner.set_series(series)
        } else if series != 0 {
            Err(BioFormatsError::SeriesOutOfRange(series))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        if self.is_tiff {
            self.inner.series()
        } else {
            0
        }
    }

    fn metadata(&self) -> &ImageMetadata {
        if self.is_tiff {
            self.inner.metadata()
        } else {
            self.meta.as_ref().expect("set_id not called")
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_bytes(plane_index);
        }
        // Placeholder: 1 byte
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Ok(vec![0u8])
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_bytes_region(plane_index, x, y, w, h);
        }
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
        if self.is_tiff {
            return self.inner.open_thumb_bytes(plane_index);
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
