//! Reader for whole-slide image formats via openslide-pure-rs.
//!
//! Supports MRXS (3DHISTECH), VMS (Hamamatsu), BIF (Ventana),
//! and other formats that OpenSlide handles.
//!
//! Requires the `openslide` cargo feature.

#[cfg(feature = "openslide")]
mod inner {
    use std::path::{Path, PathBuf};

    use openslide_pure_rs::compressed as os_compressed;
    use openslide_pure_rs::OpenSlide;

    use crate::common::compressed::{
        CompressedBytes, CompressedExtractionConstraint, CompressedExtractionSupport,
        CompressedFileRange, CompressedLevelInfo, CompressedTile, CompressedTileMode,
        Jpeg2000Container, JpegColorSpace, JpegSubsampling, LossyCodec,
    };
    use crate::common::error::{BioFormatsError, Result};
    use crate::common::metadata::{DimensionOrder, ImageMetadata};
    use crate::common::pixel_type::PixelType;
    use crate::common::reader::FormatReader;

    /// Extensions delegated to OpenSlide because bioformats-rs has no more
    /// complete native reader for them. Do not add broad OpenSlide formats
    /// here (for example BIF, CZI, DICOM, NDPI, SCN, SVS, TIFF, VMS); those are
    /// handled by native readers with fuller Bio-Formats metadata semantics.
    const OPENSLIDE_EXTENSIONS: &[&str] = &[
        "mrxs", // 3DHISTECH Pannoramic
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
            self.slide.as_ref().ok_or(BioFormatsError::NotInitialized)
        }
    }

    fn to_os_mode(mode: CompressedTileMode) -> os_compressed::CompressedTileMode {
        match mode {
            CompressedTileMode::OriginalBytes => os_compressed::CompressedTileMode::OriginalBytes,
            CompressedTileMode::DerivedLosslessJpeg => {
                os_compressed::CompressedTileMode::DerivedLosslessJpeg
            }
        }
    }

    fn from_os_mode(mode: os_compressed::CompressedTileMode) -> CompressedTileMode {
        match mode {
            os_compressed::CompressedTileMode::OriginalBytes => CompressedTileMode::OriginalBytes,
            os_compressed::CompressedTileMode::DerivedLosslessJpeg => {
                CompressedTileMode::DerivedLosslessJpeg
            }
        }
    }

    fn from_os_codec(codec: os_compressed::LossyCodec) -> LossyCodec {
        match codec {
            os_compressed::LossyCodec::Jpeg {
                color_space,
                subsampling,
            } => LossyCodec::Jpeg {
                color_space: match color_space {
                    os_compressed::JpegColorSpace::Rgb => JpegColorSpace::Rgb,
                    os_compressed::JpegColorSpace::YCbCr => JpegColorSpace::YCbCr,
                    os_compressed::JpegColorSpace::Gray => JpegColorSpace::Gray,
                    os_compressed::JpegColorSpace::Unknown => JpegColorSpace::Unknown,
                },
                subsampling: subsampling.map(|s| match s {
                    os_compressed::JpegSubsampling::Cs444 => JpegSubsampling::Cs444,
                    os_compressed::JpegSubsampling::Cs422 => JpegSubsampling::Cs422,
                    os_compressed::JpegSubsampling::Cs420 => JpegSubsampling::Cs420,
                    os_compressed::JpegSubsampling::Other {
                        horizontal,
                        vertical,
                    } => JpegSubsampling::Other {
                        horizontal,
                        vertical,
                    },
                }),
            },
            os_compressed::LossyCodec::Jpeg2000 { container } => LossyCodec::Jpeg2000 {
                container: match container {
                    os_compressed::Jpeg2000Container::Codestream => Jpeg2000Container::Codestream,
                    os_compressed::Jpeg2000Container::Jp2 => Jpeg2000Container::Jp2,
                    os_compressed::Jpeg2000Container::Unknown => Jpeg2000Container::Unknown,
                },
            },
            os_compressed::LossyCodec::JpegXr => LossyCodec::JpegXr,
        }
    }

    fn from_os_constraint(
        constraint: os_compressed::CompressedExtractionConstraint,
    ) -> CompressedExtractionConstraint {
        match constraint {
            os_compressed::CompressedExtractionConstraint::RequiresCustomZarrCodec => {
                CompressedExtractionConstraint::RequiresCustomZarrCodec
            }
            os_compressed::CompressedExtractionConstraint::EdgeTilesMayBePartial => {
                CompressedExtractionConstraint::EdgeTilesMayBePartial
            }
            os_compressed::CompressedExtractionConstraint::FragmentedSource => {
                CompressedExtractionConstraint::FragmentedSource
            }
        }
    }

    fn from_os_bytes(bytes: os_compressed::CompressedBytes) -> CompressedBytes {
        match bytes {
            os_compressed::CompressedBytes::Owned(data) => CompressedBytes::Owned(data),
            os_compressed::CompressedBytes::FileRange {
                path,
                offset,
                length,
            } => CompressedBytes::FileRange {
                path,
                offset,
                length,
            },
            os_compressed::CompressedBytes::FileRanges { ranges } => CompressedBytes::FileRanges {
                ranges: ranges
                    .into_iter()
                    .map(|range| CompressedFileRange {
                        path: range.path,
                        offset: range.offset,
                        length: range.length,
                    })
                    .collect(),
            },
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
                    BioFormatsError::Format(format!(
                        "OpenSlide dims level {}: not available",
                        level
                    ))
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
            self.meta
                .as_ref()
                .unwrap_or_else(|| crate::common::reader::uninitialized_metadata())
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

        fn compressed_level_info(
            &self,
            plane_index: u32,
            level: u32,
        ) -> Result<CompressedExtractionSupport> {
            if plane_index != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index));
            }
            let slide = self.slide()?;
            match slide.compressed_level_info(level).map_err(|e| {
                BioFormatsError::Format(format!("OpenSlide compressed_level_info: {}", e))
            })? {
                os_compressed::CompressedExtractionSupport::Supported(info) => Ok(
                    CompressedExtractionSupport::Supported(CompressedLevelInfo {
                        plane_index,
                        level: info.level,
                        width: info.width,
                        height: info.height,
                        tile_width: info.tile_width,
                        tile_height: info.tile_height,
                        tiles_across: info.tiles_across,
                        tiles_down: info.tiles_down,
                        codec: from_os_codec(info.codec),
                        modes: info.modes.into_iter().map(from_os_mode).collect(),
                        constraints: info
                            .constraints
                            .into_iter()
                            .map(from_os_constraint)
                            .collect(),
                    }),
                ),
                os_compressed::CompressedExtractionSupport::NotSupported { reason } => {
                    Ok(CompressedExtractionSupport::NotSupported { reason })
                }
            }
        }

        fn read_compressed_tile(
            &mut self,
            plane_index: u32,
            level: u32,
            col: u64,
            row: u64,
            preferred_modes: &[CompressedTileMode],
        ) -> Result<CompressedTile> {
            if plane_index != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index));
            }
            let slide = self.slide()?;
            let os_preferred_modes: Vec<_> =
                preferred_modes.iter().copied().map(to_os_mode).collect();
            let tile = slide
                .read_compressed_tile(level, col, row, &os_preferred_modes)
                .map_err(|e| {
                    BioFormatsError::Format(format!("OpenSlide read_compressed_tile: {}", e))
                })?;
            Ok(CompressedTile {
                plane_index,
                level: tile.level,
                col: tile.col,
                row: tile.row,
                origin_x: tile.origin_x,
                origin_y: tile.origin_y,
                width: tile.width,
                height: tile.height,
                nominal_tile_width: tile.nominal_tile_width,
                nominal_tile_height: tile.nominal_tile_height,
                codec: from_os_codec(tile.codec),
                mode: from_os_mode(tile.mode),
                bytes: from_os_bytes(tile.bytes),
            })
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

    #[cfg(test)]
    mod tests {
        use super::OpenSlideReader;
        use crate::common::reader::FormatReader;

        #[test]
        fn metadata_before_set_id_returns_uninitialized_fallback() {
            let reader = OpenSlideReader::new();

            assert_eq!(reader.metadata().size_x, 0);
            assert_eq!(reader.metadata().size_y, 0);
        }

        #[test]
        fn detection_is_limited_to_formats_without_native_readers() {
            let reader = OpenSlideReader::new();

            assert!(reader.is_this_type_by_name(std::path::Path::new("slide.mrxs")));
            assert!(!reader.is_this_type_by_name(std::path::Path::new("slide.bif")));
            assert!(!reader.is_this_type_by_name(std::path::Path::new("slide.vms")));
            assert!(!reader.is_this_type_by_name(std::path::Path::new("slide.scn")));
            assert!(!reader.is_this_type_by_name(std::path::Path::new("slide.svs")));
            assert!(!reader.is_this_type_by_name(std::path::Path::new("slide.czi")));
            assert!(!reader.is_this_type_by_name(std::path::Path::new("slide.dcm")));
            assert!(!reader.is_this_type_by_name(std::path::Path::new("slide.tif")));
        }

        #[test]
        fn compressed_extraction_requires_initialized_slide() {
            let mut reader = OpenSlideReader::new();

            assert!(reader.compressed_level_info(0, 0).is_err());
            assert!(reader.read_compressed_tile(0, 0, 0, 0, &[]).is_err());
        }

        #[test]
        fn compressed_extraction_rejects_nonzero_plane() {
            let mut reader = OpenSlideReader::new();

            assert!(reader.compressed_level_info(1, 0).is_err());
            assert!(reader.read_compressed_tile(1, 0, 0, 0, &[]).is_err());
        }
    }
}

// Re-export only when feature is enabled
#[cfg(feature = "openslide")]
pub use inner::OpenSlideReader;
