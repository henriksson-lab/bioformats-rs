//! Reader for whole-slide image formats via openslide-pure-rs.
//!
//! Supports MRXS (3DHISTECH), VMS (Hamamatsu), BIF (Ventana),
//! and other formats that OpenSlide handles.
//!
//! Requires the `openslide` cargo feature.

#[cfg(feature = "openslide")]
mod inner {
    use std::path::{Path, PathBuf};

    use openslide_pure_rs::OpenSlide;

    use crate::common::error::{BioFormatsError, Result};
    use crate::common::metadata::{DimensionOrder, ImageMetadata};
    use crate::common::pixel_type::PixelType;
    use crate::common::reader::FormatReader;

    /// Extensions supported by OpenSlide that bioformats-rs doesn't already cover well.
    const OPENSLIDE_EXTENSIONS: &[&str] = &[
        "mrxs", // 3DHISTECH Pannoramic
        "vms",  // Hamamatsu VMS
        "bif",  // Ventana BIF
    ];

    pub struct OpenSlideReader {
        path: Option<PathBuf>,
        slide: Option<OpenSlide>,
        meta: Option<ImageMetadata>,
        current_resolution: usize,
        resolution_dims: Vec<(u32, u32)>,
    }

    impl OpenSlideReader {
        pub fn new() -> Self {
            OpenSlideReader {
                path: None,
                slide: None,
                meta: None,
                current_resolution: 0,
                resolution_dims: Vec::new(),
            }
        }

        fn slide(&self) -> Result<&OpenSlide> {
            self.slide
                .as_ref()
                .ok_or(BioFormatsError::NotInitialized)
        }
    }

    impl Default for OpenSlideReader {
        fn default() -> Self {
            Self::new()
        }
    }

    impl FormatReader for OpenSlideReader {
        fn is_this_type_by_name(&self, path: &Path) -> bool {
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| {
                    let lower = e.to_ascii_lowercase();
                    OPENSLIDE_EXTENSIONS.iter().any(|ext| *ext == lower)
                })
                .unwrap_or(false)
        }

        fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
            false
        }

        fn set_id(&mut self, path: &Path) -> Result<()> {
            let slide = OpenSlide::open(path)
                .map_err(|e| BioFormatsError::Format(format!("OpenSlide: {}", e)))?;

            let level_count = slide.level_count();

            // Collect dimensions for all resolution levels
            let mut resolution_dims = Vec::with_capacity(level_count as usize);
            for level in 0..level_count {
                let (w, h) = slide.level_dimensions(level).ok_or_else(|| {
                    BioFormatsError::Format(format!("OpenSlide dims level {}: not available", level))
                })?;
                resolution_dims.push((w as u32, h as u32));
            }

            let (w0, h0) = resolution_dims[0];

            let channel_count = slide.channel_count();

            let meta = ImageMetadata {
                size_x: w0,
                size_y: h0,
                size_z: 1,
                size_c: channel_count,
                size_t: 1,
                pixel_type: PixelType::Uint8,
                bits_per_pixel: 8,
                image_count: 1,
                dimension_order: DimensionOrder::XYCZT,
                is_rgb: true,
                is_interleaved: true,
                is_indexed: false,
                is_little_endian: true,
                resolution_count: level_count,
                ..Default::default()
            };

            self.path = Some(path.to_path_buf());
            self.slide = Some(slide);
            self.meta = Some(meta);
            self.current_resolution = 0;
            self.resolution_dims = resolution_dims;

            Ok(())
        }

        fn close(&mut self) -> Result<()> {
            self.path = None;
            self.slide = None;
            self.meta = None;
            self.current_resolution = 0;
            self.resolution_dims.clear();
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
            if plane_index != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index));
            }
            let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            let w = meta.size_x;
            let h = meta.size_y;
            self.open_bytes_region(plane_index, 0, 0, w, h)
        }

        fn open_bytes_region(
            &mut self,
            plane_index: u32,
            x: u32,
            y: u32,
            w: u32,
            h: u32,
        ) -> Result<Vec<u8>> {
            if plane_index != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index));
            }
            let slide = self.slide()?;
            let level = self.current_resolution as u32;

            // openslide-pure-rs read_region coordinates are in level-0 pixel space.
            // If we're at a lower resolution, scale the coordinates up.
            let downsample = slide.level_downsample(level).unwrap_or(1.0);
            let x0 = (x as f64 * downsample) as i64;
            let y0 = (y as f64 * downsample) as i64;

            let channel_count = slide.channel_count();

            // Read all channels and interleave into RGB(A) bytes
            let size = w as usize * h as usize;
            let mut out = vec![0u8; size * channel_count as usize];

            for ch in 0..channel_count {
                let gray = slide.read_region(ch, x0, y0, level, w, h).map_err(|e| {
                    BioFormatsError::Format(format!("OpenSlide read_region: {}", e))
                })?;
                for i in 0..size {
                    if i < gray.data.len() {
                        out[i * channel_count as usize + ch as usize] = gray.data[i];
                    }
                }
            }

            Ok(out)
        }

        fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
            if plane_index != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index));
            }
            let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            let tw = meta.size_x.min(256);
            let th = meta.size_y.min(256);
            let tx = (meta.size_x - tw) / 2;
            let ty = (meta.size_y - th) / 2;
            self.open_bytes_region(plane_index, tx, ty, tw, th)
        }

        fn resolution_count(&self) -> usize {
            self.resolution_dims.len()
        }

        fn set_resolution(&mut self, level: usize) -> Result<()> {
            if level >= self.resolution_dims.len() {
                return Err(BioFormatsError::Format(format!(
                    "Resolution level {} out of range ({})",
                    level,
                    self.resolution_dims.len()
                )));
            }
            self.current_resolution = level;
            // Update metadata dimensions for the new resolution level
            if let Some(meta) = self.meta.as_mut() {
                let (w, h) = self.resolution_dims[level];
                meta.size_x = w;
                meta.size_y = h;
            }
            Ok(())
        }

        fn resolution(&self) -> usize {
            self.current_resolution
        }
    }
}

// Re-export only when feature is enabled
#[cfg(feature = "openslide")]
pub use inner::OpenSlideReader;
