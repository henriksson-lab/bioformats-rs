use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::read_bytes_at;
use crate::common::metadata::DimensionOrder;
use crate::common::pixel_type::PixelType;

use super::compression::decompress;
use super::ifd::{tag, Compression, Ifd, Photometric};
use super::parser::TiffParser;

// Re-export LookupTable from bioformats facade via bioformats_common — but here we define a
// local one and later translate it.
//
// Actually the bioformats crate owns ImageMetadata. We build it from our data.

/// Internal per-IFD derived image info.
#[derive(Debug, Clone)]
struct IfdInfo {
    width: u32,
    height: u32,
    samples_per_pixel: u16,
    bits_per_sample: u16,
    pixel_type: crate::common::pixel_type::PixelType,
    compression: super::ifd::Compression,
    photometric: Photometric,
    planar_config: u16,
    predictor: u16,
    is_tiled: bool,
    tile_width: u32,
    tile_height: u32,
    rows_per_strip: u32,
    strip_offsets: Vec<u64>,
    strip_byte_counts: Vec<u64>,
    tile_offsets: Vec<u64>,
    tile_byte_counts: Vec<u64>,
    color_map: Option<(Vec<u16>, Vec<u16>, Vec<u16>)>,
    jpeg_tables: Option<Vec<u8>>,
    image_description: Option<String>,
    ycbcr_subsampling: (u16, u16),
    ycbcr_coefficients: (f32, f32, f32),
}

#[derive(Debug, Clone)]
struct OmeTiffImage {
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    effective_c: u32,
    size_t: u32,
    pixel_type: PixelType,
    bits_per_pixel: u8,
    dimension_order: DimensionOrder,
    tiff_data: Vec<OmeTiffData>,
}

#[derive(Debug, Clone)]
struct OmeTiffData {
    ifd: usize,
    plane_count: Option<usize>,
    first_z: u32,
    first_c: u32,
    first_t: u32,
}

/// Open TIFF file handle.
struct TiffFile {
    parser: TiffParser<BufReader<File>>,
    ifds: Vec<Ifd>,
}

/// A TIFF series groups IFDs that belong together (e.g., Z-stack stored as multiple IFDs).
#[derive(Debug, Clone)]
pub struct TiffSeries {
    /// IFD indices belonging to this series (into `TiffFile::ifds`).
    pub ifd_indices: Vec<usize>,
    /// Optional logical plane to IFD mapping for OME-TIFF TiffData.
    pub plane_ifd_indices: Vec<Option<usize>>,
    pub metadata: crate::common::metadata::ImageMetadata,
    /// Sub-resolution pyramid levels. Each entry is a list of IFD indices for
    /// one resolution level (smaller than the main). Level 0 = main (ifd_indices).
    pub sub_resolutions: Vec<Vec<usize>>,
}

pub struct TiffReader {
    file: Option<TiffFile>,
    series: Vec<TiffSeries>,
    current_series: usize,
    current_resolution: usize,
    /// OME-XML embedded in the first IFD's ImageDescription, if present.
    ome_xml: Option<String>,
}

impl TiffReader {
    pub fn new() -> Self {
        TiffReader {
            file: None,
            series: Vec::new(),
            current_series: 0,
            current_resolution: 0,
            ome_xml: None,
        }
    }

    /// Access the series list for vendor-specific wrappers.
    pub fn series_list(&self) -> &[TiffSeries] {
        &self.series
    }

    /// Access the series list mutably (for enriching metadata in wrappers).
    pub fn series_list_mut(&mut self) -> &mut [TiffSeries] {
        &mut self.series
    }

    /// Access a raw IFD by index (for extracting vendor-specific tags).
    pub fn ifd(&self, index: usize) -> Option<&Ifd> {
        self.file.as_ref().and_then(|f| f.ifds.get(index))
    }

    /// Get the embedded OME-XML string, if any.
    pub fn ome_xml_str(&self) -> Option<&str> {
        self.ome_xml.as_deref()
    }

    /// Extract `IfdInfo` from a raw `Ifd`.
    fn ifd_info(ifd: &Ifd, _little_endian: bool) -> Result<IfdInfo> {
        let width = ifd
            .image_width()
            .ok_or_else(|| BioFormatsError::Format("IFD missing ImageWidth".into()))?;
        let height = ifd
            .image_length()
            .ok_or_else(|| BioFormatsError::Format("IFD missing ImageLength".into()))?;

        let samples_per_pixel = ifd.samples_per_pixel();
        let bps_vec = ifd.bits_per_sample();
        let bits_per_sample = bps_vec.first().copied().unwrap_or(8);

        let sample_format = ifd.get_u16(tag::SAMPLE_FORMAT).unwrap_or(1);
        let pixel_type = pixel_type_from_bps_format(bits_per_sample, sample_format);

        let photometric = ifd.photometric();
        let compression = ifd.compression();
        let planar_config = ifd.planar_configuration();
        let predictor = ifd.predictor();

        let is_tiled = ifd.is_tiled();

        let (tile_width, tile_height) = if is_tiled {
            (
                ifd.tile_width().unwrap_or(0),
                ifd.tile_length().unwrap_or(0),
            )
        } else {
            (0, 0)
        };

        let rows_per_strip = if is_tiled {
            0
        } else {
            ifd.get_u32(tag::ROWS_PER_STRIP).unwrap_or(0)
        };

        let strip_offsets = ifd.get_vec_u64(tag::STRIP_OFFSETS);
        let strip_byte_counts = ifd.get_vec_u64(tag::STRIP_BYTE_COUNTS);
        let tile_offsets = ifd.get_vec_u64(tag::TILE_OFFSETS);
        let tile_byte_counts = ifd.get_vec_u64(tag::TILE_BYTE_COUNTS);
        validate_tiff_storage(
            width,
            height,
            samples_per_pixel,
            planar_config,
            is_tiled,
            tile_width,
            tile_height,
            rows_per_strip,
            &strip_offsets,
            &strip_byte_counts,
            &tile_offsets,
            &tile_byte_counts,
        )?;

        let color_map = if photometric == Photometric::Palette {
            if let Some(v) = ifd.get(tag::COLOR_MAP) {
                let data = v.as_vec_u16();
                let n = data.len() / 3;
                Some((
                    data[..n].to_vec(),
                    data[n..2 * n].to_vec(),
                    data[2 * n..].to_vec(),
                ))
            } else {
                None
            }
        } else {
            None
        };

        // JPEG tables (tag 347)
        let jpeg_tables = ifd.get(tag::JPEG_TABLES).and_then(|v| match v {
            super::ifd::IfdValue::Undefined(b) => Some(b.clone()),
            _ => None,
        });

        let image_description = ifd.get_str(tag::IMAGE_DESCRIPTION).map(str::to_owned);
        let subsampling = ifd.get_vec_u16(tag::YCBCR_SUBSAMPLING);
        let ycbcr_subsampling = (
            subsampling.first().copied().unwrap_or(2).max(1),
            subsampling.get(1).copied().unwrap_or(2).max(1),
        );
        let coefficients = ifd
            .get(tag::YCBCR_COEFFICIENTS)
            .and_then(|v| v.as_vec_f32())
            .filter(|v| v.len() >= 3)
            .map(|v| (v[0], v[1], v[2]))
            .unwrap_or((0.299, 0.587, 0.114));

        Ok(IfdInfo {
            width,
            height,
            samples_per_pixel,
            bits_per_sample,
            pixel_type,
            compression,
            photometric,
            planar_config,
            predictor,
            is_tiled,
            tile_width,
            tile_height,
            rows_per_strip,
            strip_offsets,
            strip_byte_counts,
            tile_offsets,
            tile_byte_counts,
            color_map,
            jpeg_tables,
            image_description,
            ycbcr_subsampling,
            ycbcr_coefficients: coefficients,
        })
    }

    /// Build `TiffSeries` list from parsed IFDs.
    /// Heuristic: IFDs with the same (width, height, spp, bps) form one series.
    fn build_series(ifds: &[Ifd], little_endian: bool) -> Vec<TiffSeries> {
        use crate::common::metadata::ImageMetadata;

        // Parse infos for all IFDs (skip ones that fail)
        let infos: Vec<(usize, IfdInfo)> = ifds
            .iter()
            .enumerate()
            .filter_map(|(i, ifd)| {
                Self::ifd_info(ifd, little_endian)
                    .ok()
                    .map(|info| (i, info))
            })
            .collect();

        if infos.is_empty() {
            return vec![];
        }

        // Group consecutive IFDs with matching dimensions
        let mut groups: Vec<Vec<(usize, &IfdInfo)>> = Vec::new();
        for (idx, info) in &infos {
            if let Some(last) = groups.last_mut() {
                let prev = last.last().unwrap().1;
                if prev.width == info.width
                    && prev.height == info.height
                    && prev.samples_per_pixel == info.samples_per_pixel
                    && prev.bits_per_sample == info.bits_per_sample
                {
                    last.push((*idx, info));
                    continue;
                }
            }
            groups.push(vec![(*idx, info)]);
        }

        groups
            .into_iter()
            .map(|group| {
                let ifd_indices: Vec<usize> = group.iter().map(|(i, _)| *i).collect();
                let info = group[0].1;

                let is_rgb = matches!(info.photometric, Photometric::Rgb | Photometric::YCbCr)
                    && info.samples_per_pixel >= 3;
                let is_indexed = info.photometric == Photometric::Palette;
                let size_c = if is_rgb || info.photometric == Photometric::Cmyk {
                    info.samples_per_pixel as u32
                } else {
                    1
                };

                let lookup_table =
                    info.color_map
                        .as_ref()
                        .map(|(r, g, b)| crate::common::metadata::LookupTable {
                            red: r.clone(),
                            green: g.clone(),
                            blue: b.clone(),
                        });

                let image_count = ifd_indices.len() as u32;
                let mut meta = ImageMetadata {
                    size_x: info.width,
                    size_y: info.height,
                    size_z: image_count,
                    size_c,
                    size_t: 1,
                    pixel_type: info.pixel_type,
                    bits_per_pixel: info.bits_per_sample as u8,
                    image_count,
                    dimension_order: crate::common::metadata::DimensionOrder::XYZTC,
                    is_rgb,
                    is_interleaved: info.planar_config == 1,
                    is_indexed,
                    is_little_endian: little_endian,
                    resolution_count: 1,
                    series_metadata: HashMap::new(),
                    lookup_table,
                    modulo_z: None,
                    modulo_c: None,
                    modulo_t: None,
                };

                // Store image description in metadata
                if let Some(desc) = &info.image_description {
                    meta.series_metadata.insert(
                        "ImageDescription".into(),
                        crate::common::metadata::MetadataValue::String(desc.clone()),
                    );
                }

                TiffSeries {
                    ifd_indices,
                    plane_ifd_indices: Vec::new(),
                    metadata: meta,
                    sub_resolutions: Vec::new(),
                }
            })
            .collect()
    }

    fn build_ome_series(ifds: &[Ifd], xml: &str, little_endian: bool) -> Option<Vec<TiffSeries>> {
        let images = parse_ome_tiff_images(xml);
        if images.is_empty() {
            return None;
        }

        let mut series = Vec::new();
        for image in images {
            let image_count = image
                .size_z
                .saturating_mul(image.effective_c)
                .saturating_mul(image.size_t);
            if image_count == 0 {
                continue;
            }

            let mut plane_map = build_ome_plane_map(&image, ifds.len());
            if plane_map.iter().all(Option::is_none) {
                for (i, slot) in plane_map.iter_mut().enumerate() {
                    if i < ifds.len() {
                        *slot = Some(i);
                    }
                }
            }

            let ifd_indices: Vec<usize> = plane_map.iter().filter_map(|&idx| idx).collect();
            let first_info = ifd_indices
                .first()
                .and_then(|&idx| ifds.get(idx))
                .and_then(|ifd| Self::ifd_info(ifd, little_endian).ok())
                .or_else(|| {
                    ifds.first()
                        .and_then(|ifd| Self::ifd_info(ifd, little_endian).ok())
                })?;

            let mut meta = crate::common::metadata::ImageMetadata {
                size_x: image.size_x,
                size_y: image.size_y,
                size_z: image.size_z,
                size_c: image.size_c,
                size_t: image.size_t,
                pixel_type: image.pixel_type,
                bits_per_pixel: image.bits_per_pixel,
                image_count,
                dimension_order: image.dimension_order,
                is_rgb: first_info.samples_per_pixel >= 3
                    && matches!(
                        first_info.photometric,
                        Photometric::Rgb | Photometric::YCbCr
                    ),
                is_interleaved: first_info.planar_config == 1,
                is_indexed: first_info.photometric == Photometric::Palette,
                is_little_endian: little_endian,
                resolution_count: 1,
                series_metadata: HashMap::new(),
                lookup_table: first_info.color_map.as_ref().map(|(r, g, b)| {
                    crate::common::metadata::LookupTable {
                        red: r.clone(),
                        green: g.clone(),
                        blue: b.clone(),
                    }
                }),
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            };
            meta.series_metadata.insert(
                "ImageDescription".into(),
                crate::common::metadata::MetadataValue::String(xml.to_string()),
            );

            series.push(TiffSeries {
                ifd_indices,
                plane_ifd_indices: plane_map,
                metadata: meta,
                sub_resolutions: Vec::new(),
            });
        }

        if series.is_empty() {
            None
        } else {
            Some(series)
        }
    }

    /// Parse SubIFD chains for pyramid support.
    /// For each series, collect each main plane's SUB_IFD tag (330) and
    /// transpose those per-plane offsets into per-resolution plane lists.
    fn parse_sub_ifds(&mut self) -> Result<()> {
        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;

        for series in &mut self.series {
            if series.ifd_indices.is_empty() {
                continue;
            }

            let main_planes: Vec<Option<usize>> = if !series.plane_ifd_indices.is_empty() {
                series.plane_ifd_indices.clone()
            } else {
                series.ifd_indices.iter().copied().map(Some).collect()
            };
            let plane_count = main_planes.len();
            let mut sub_res_slots: Vec<Vec<Option<usize>>> = Vec::new();

            for (plane_idx, main_ifd_idx) in main_planes.into_iter().enumerate() {
                let Some(main_ifd_idx) = main_ifd_idx else {
                    continue;
                };
                let sub_ifd_offsets = file.ifds[main_ifd_idx].get_vec_u64(tag::SUB_IFD);
                for (level_idx, offset) in sub_ifd_offsets.into_iter().enumerate() {
                    let (sub_ifd, _next) = file.parser.read_ifd(offset)?;
                    // Verify the sub-IFD is a valid image (has width/height).
                    if sub_ifd.image_width().is_none() {
                        continue;
                    }
                    let sub_idx = file.ifds.len();
                    file.ifds.push(sub_ifd);

                    while sub_res_slots.len() <= level_idx {
                        sub_res_slots.push(vec![None; plane_count]);
                    }
                    sub_res_slots[level_idx][plane_idx] = Some(sub_idx);
                }
            }

            let sub_res_levels: Vec<Vec<usize>> = sub_res_slots
                .into_iter()
                .filter_map(|level| level.into_iter().collect::<Option<Vec<_>>>())
                .collect();

            if !sub_res_levels.is_empty() {
                series.sub_resolutions = sub_res_levels;
                series.metadata.resolution_count = 1 + series.sub_resolutions.len() as u32;
            }
        }
        Ok(())
    }

    /// Resolve the IFD index for a given plane, taking current resolution into account.
    fn resolve_ifd_index(&self, plane_index: u32) -> Result<usize> {
        let s = &self.series[self.current_series];
        if self.current_resolution == 0 {
            // Main resolution
            if !s.plane_ifd_indices.is_empty() {
                return s
                    .plane_ifd_indices
                    .get(plane_index as usize)
                    .and_then(|&idx| idx)
                    .ok_or(BioFormatsError::PlaneOutOfRange(plane_index));
            }
            s.ifd_indices
                .get(plane_index as usize)
                .copied()
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))
        } else {
            // Sub-resolution level (1-based index into sub_resolutions)
            let level = self.current_resolution - 1;
            let sub = s.sub_resolutions.get(level).ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "resolution level {} out of range",
                    self.current_resolution
                ))
            })?;
            sub.get(plane_index as usize)
                .copied()
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))
        }
    }

    /// Read raw bytes for one plane from the file.
    fn read_plane_bytes(
        &mut self,
        ifd_index: usize,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let ifd = file
            .ifds
            .get(ifd_index)
            .ok_or_else(|| BioFormatsError::PlaneOutOfRange(ifd_index as u32))?;
        let little_endian = file.parser.little_endian;
        let info = Self::ifd_info(ifd, little_endian)?;
        validate_region(&info, x, y, w, h)?;

        if info.is_tiled {
            self.read_tiled_plane(&info, x, y, w, h, 0)
        } else {
            self.read_stripped_plane(&info, x, y, w, h, 0)
        }
    }

    fn read_stripped_plane(
        &mut self,
        info: &IfdInfo,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        _plane_byte_len: usize,
    ) -> Result<Vec<u8>> {
        if info.planar_config == 2 && info.samples_per_pixel > 1 {
            if info.bits_per_sample < 8 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Packed planar TIFF samples are not yet supported".into(),
                ));
            }
            return self.read_planar_stripped_plane(info, x, y, w, h);
        }

        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        if info.photometric == Photometric::YCbCr && is_unsupported_ycbcr(info) {
            return Err(BioFormatsError::UnsupportedFormat(
                "Only 8-bit chunky non-JPEG TIFF YCbCr is supported".into(),
            ));
        }

        let bytes_per_sample = (info.bits_per_sample as u32 + 7) / 8;
        let effective_spp = info.samples_per_pixel as u32;
        let packed_samples = info.bits_per_sample < 8;
        let ycbcr = info.photometric == Photometric::YCbCr;
        let row_bytes = if ycbcr {
            ycbcr_row_bytes(info.width, 1, info.ycbcr_subsampling)
        } else if packed_samples {
            packed_row_bytes(info.width, effective_spp, info.bits_per_sample) as u32
        } else {
            info.width * effective_spp * bytes_per_sample
        };

        let rows_per_strip = if info.rows_per_strip == 0 || info.rows_per_strip >= info.height {
            info.height
        } else {
            info.rows_per_strip
        };

        // We assemble the full plane row-by-row, then crop to [x, y, w, h].
        let mut plane_rows: Vec<u8> = Vec::with_capacity((h * row_bytes) as usize);

        for strip_idx in 0..info.strip_offsets.len() {
            let strip_start_row = strip_idx as u32 * rows_per_strip;
            let strip_end_row = (strip_start_row + rows_per_strip).min(info.height);

            // Skip strips entirely above or below the requested region
            if strip_end_row <= y || strip_start_row >= y + h {
                continue;
            }

            let offset = info.strip_offsets[strip_idx];
            let byte_count = info.strip_byte_counts[strip_idx] as usize;

            let compressed = read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
            let strip_rows = strip_end_row - strip_start_row;
            let expected = if ycbcr {
                ycbcr_strip_bytes(info.width, strip_rows, info.ycbcr_subsampling)
            } else {
                (strip_rows * row_bytes) as usize
            };

            let mut strip_data = decompress(
                &compressed,
                info.compression,
                expected,
                info.predictor,
                effective_spp as u16,
                info.bits_per_sample,
                info.width,
                strip_rows,
                file.parser.little_endian,
                info.jpeg_tables.as_deref(),
            )?;
            strip_data.truncate(expected);

            // Crop rows within this strip to the requested y range
            let row_start = y.saturating_sub(strip_start_row) as usize;
            let row_end = (y + h - strip_start_row).min(strip_rows) as usize;

            if ycbcr {
                if row_start == 0 && row_end as u32 == strip_rows {
                    plane_rows.extend_from_slice(&strip_data);
                    continue;
                }
                return Err(BioFormatsError::UnsupportedFormat(
                    "Partial-row reads for subsampled TIFF YCbCr are not yet supported".into(),
                ));
            }

            for row in row_start..row_end {
                let rs = row * row_bytes as usize;
                let re = rs + row_bytes as usize;
                if re <= strip_data.len() {
                    plane_rows.extend_from_slice(&strip_data[rs..re]);
                }
            }
        }

        if ycbcr {
            let rgb = decode_ycbcr_chunky(
                &plane_rows,
                info.width,
                h,
                info.ycbcr_subsampling,
                info.ycbcr_coefficients,
            )?;
            if x == 0 && w == info.width {
                return Ok(rgb);
            }
            return Ok(crop_unpacked_rows(&rgb, info.width, 3, x, w, h));
        }

        if packed_samples {
            let unpacked = unpack_subbyte_samples(
                &plane_rows,
                info.width,
                h,
                effective_spp,
                info.bits_per_sample,
            );
            let mut out = crop_unpacked_rows(&unpacked, info.width, effective_spp, x, w, h);
            apply_photometric(
                &mut out,
                info.photometric,
                info.bits_per_sample,
                file.parser.little_endian,
            );
            return Ok(out);
        }

        apply_photometric(
            &mut plane_rows,
            info.photometric,
            info.bits_per_sample,
            file.parser.little_endian,
        );

        // Crop columns
        if x == 0 && w == info.width {
            return Ok(plane_rows);
        }

        let x_start = (x * effective_spp * bytes_per_sample) as usize;
        let x_len = (w * effective_spp * bytes_per_sample) as usize;
        let full_row = row_bytes as usize;
        let mut out = Vec::with_capacity(h as usize * x_len);
        for row in 0..h as usize {
            let src = &plane_rows[row * full_row..];
            out.extend_from_slice(&src[x_start..x_start + x_len]);
        }
        Ok(out)
    }

    fn read_planar_stripped_plane(
        &mut self,
        info: &IfdInfo,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let bytes_per_sample = (info.bits_per_sample as u32 + 7) / 8;
        let channel_row_bytes = info.width * bytes_per_sample;
        let rows_per_strip = if info.rows_per_strip == 0 || info.rows_per_strip >= info.height {
            info.height
        } else {
            info.rows_per_strip
        };
        let strips_per_channel = (info.height + rows_per_strip - 1) / rows_per_strip;
        let x_start = (x * bytes_per_sample) as usize;
        let x_len = (w * bytes_per_sample) as usize;
        let mut out = Vec::with_capacity(h as usize * x_len * info.samples_per_pixel as usize);

        for channel in 0..info.samples_per_pixel as usize {
            let mut channel_rows = Vec::with_capacity((h * channel_row_bytes) as usize);
            for strip in 0..strips_per_channel as usize {
                let strip_start_row = strip as u32 * rows_per_strip;
                let strip_end_row = (strip_start_row + rows_per_strip).min(info.height);
                if strip_end_row <= y || strip_start_row >= y + h {
                    continue;
                }

                let strip_idx = channel * strips_per_channel as usize + strip;
                if strip_idx >= info.strip_offsets.len()
                    || strip_idx >= info.strip_byte_counts.len()
                {
                    continue;
                }
                let offset = info.strip_offsets[strip_idx];
                let byte_count = info.strip_byte_counts[strip_idx] as usize;
                let compressed = read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
                let strip_rows = strip_end_row - strip_start_row;
                let expected = (strip_rows * channel_row_bytes) as usize;
                let mut strip_data = decompress(
                    &compressed,
                    info.compression,
                    expected,
                    info.predictor,
                    1,
                    info.bits_per_sample,
                    info.width,
                    strip_rows,
                    file.parser.little_endian,
                    info.jpeg_tables.as_deref(),
                )?;
                strip_data.truncate(expected);

                let row_start = y.saturating_sub(strip_start_row) as usize;
                let row_end = (y + h - strip_start_row).min(strip_rows) as usize;
                for row in row_start..row_end {
                    let rs = row * channel_row_bytes as usize;
                    let re = rs + channel_row_bytes as usize;
                    if re <= strip_data.len() {
                        channel_rows.extend_from_slice(&strip_data[rs..re]);
                    }
                }
            }

            apply_photometric(
                &mut channel_rows,
                info.photometric,
                info.bits_per_sample,
                file.parser.little_endian,
            );

            if x == 0 && w == info.width {
                out.extend_from_slice(&channel_rows);
            } else {
                let full_row = channel_row_bytes as usize;
                for row in 0..h as usize {
                    let src = &channel_rows[row * full_row..];
                    out.extend_from_slice(&src[x_start..x_start + x_len]);
                }
            }
        }

        Ok(out)
    }

    fn read_tiled_plane(
        &mut self,
        info: &IfdInfo,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        _plane_byte_len: usize,
    ) -> Result<Vec<u8>> {
        if info.planar_config == 2 && info.samples_per_pixel > 1 {
            if info.bits_per_sample < 8 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Packed planar TIFF samples are not yet supported".into(),
                ));
            }
            return self.read_planar_tiled_plane(info, x, y, w, h);
        }

        if info.photometric == Photometric::YCbCr {
            return Err(BioFormatsError::UnsupportedFormat(
                "Tiled TIFF YCbCr is not yet supported".into(),
            ));
        }

        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let bytes_per_sample = (info.bits_per_sample as u32 + 7) / 8;
        let effective_spp = info.samples_per_pixel as u32;
        let tile_row_bytes = (info.tile_width * effective_spp * bytes_per_sample) as usize;
        let tile_data_bytes = tile_row_bytes * info.tile_height as usize;
        let tiles_across = (info.width + info.tile_width - 1) / info.tile_width;

        let tx_start = x / info.tile_width;
        let tx_end = (x + w + info.tile_width - 1) / info.tile_width;
        let ty_start = y / info.tile_height;
        let ty_end = (y + h + info.tile_height - 1) / info.tile_height;

        let out_row_bytes = (w * effective_spp * bytes_per_sample) as usize;
        let mut out = vec![0u8; h as usize * out_row_bytes];

        for ty in ty_start..ty_end {
            for tx in tx_start..tx_end {
                let tile_idx = (ty * tiles_across + tx) as usize;
                if tile_idx >= info.tile_offsets.len() {
                    continue;
                }
                let offset = info.tile_offsets[tile_idx];
                let byte_count = info.tile_byte_counts[tile_idx] as usize;
                let compressed = read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
                let mut tile_data = decompress(
                    &compressed,
                    info.compression,
                    tile_data_bytes,
                    info.predictor,
                    effective_spp as u16,
                    info.bits_per_sample,
                    info.tile_width,
                    info.tile_height,
                    file.parser.little_endian,
                    info.jpeg_tables.as_deref(),
                )?;
                tile_data.resize(tile_data_bytes, 0);
                apply_photometric(
                    &mut tile_data,
                    info.photometric,
                    info.bits_per_sample,
                    file.parser.little_endian,
                );

                // Determine overlap between tile and requested region
                let tile_x0 = tx * info.tile_width;
                let tile_y0 = ty * info.tile_height;

                let src_x = x.saturating_sub(tile_x0) as usize;
                let src_y = y.saturating_sub(tile_y0) as usize;
                let dst_x = tile_x0.saturating_sub(x) as usize;
                let dst_y = tile_y0.saturating_sub(y) as usize;

                let copy_w = ((info.tile_width - src_x as u32).min(w - dst_x as u32)) as usize;
                let copy_h = ((info.tile_height - src_y as u32).min(h - dst_y as u32)) as usize;
                let copy_bytes = copy_w * effective_spp as usize * bytes_per_sample as usize;

                for row in 0..copy_h {
                    let src_off = ((src_y + row) * tile_row_bytes)
                        + src_x * effective_spp as usize * bytes_per_sample as usize;
                    let dst_off = ((dst_y + row) * out_row_bytes)
                        + dst_x * effective_spp as usize * bytes_per_sample as usize;
                    if src_off + copy_bytes <= tile_data.len() && dst_off + copy_bytes <= out.len()
                    {
                        out[dst_off..dst_off + copy_bytes]
                            .copy_from_slice(&tile_data[src_off..src_off + copy_bytes]);
                    }
                }
            }
        }

        Ok(out)
    }

    fn read_planar_tiled_plane(
        &mut self,
        info: &IfdInfo,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let bytes_per_sample = (info.bits_per_sample as u32 + 7) / 8;
        let tile_row_bytes = (info.tile_width * bytes_per_sample) as usize;
        let tile_data_bytes = tile_row_bytes * info.tile_height as usize;
        let tiles_across = (info.width + info.tile_width - 1) / info.tile_width;
        let tiles_down = (info.height + info.tile_height - 1) / info.tile_height;
        let tiles_per_channel = (tiles_across * tiles_down) as usize;

        let tx_start = x / info.tile_width;
        let tx_end = (x + w + info.tile_width - 1) / info.tile_width;
        let ty_start = y / info.tile_height;
        let ty_end = (y + h + info.tile_height - 1) / info.tile_height;

        let out_row_bytes = (w * bytes_per_sample) as usize;
        let mut out =
            Vec::with_capacity(h as usize * out_row_bytes * info.samples_per_pixel as usize);

        for channel in 0..info.samples_per_pixel as usize {
            let mut channel_out = vec![0u8; h as usize * out_row_bytes];
            for ty in ty_start..ty_end {
                for tx in tx_start..tx_end {
                    let spatial_tile_idx = (ty * tiles_across + tx) as usize;
                    let tile_idx = channel * tiles_per_channel + spatial_tile_idx;
                    if tile_idx >= info.tile_offsets.len()
                        || tile_idx >= info.tile_byte_counts.len()
                    {
                        continue;
                    }
                    let offset = info.tile_offsets[tile_idx];
                    let byte_count = info.tile_byte_counts[tile_idx] as usize;
                    let compressed = read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
                    let mut tile_data = decompress(
                        &compressed,
                        info.compression,
                        tile_data_bytes,
                        info.predictor,
                        1,
                        info.bits_per_sample,
                        info.tile_width,
                        info.tile_height,
                        file.parser.little_endian,
                        info.jpeg_tables.as_deref(),
                    )?;
                    tile_data.resize(tile_data_bytes, 0);
                    apply_photometric(
                        &mut tile_data,
                        info.photometric,
                        info.bits_per_sample,
                        file.parser.little_endian,
                    );

                    let tile_x0 = tx * info.tile_width;
                    let tile_y0 = ty * info.tile_height;
                    let src_x = x.saturating_sub(tile_x0) as usize;
                    let src_y = y.saturating_sub(tile_y0) as usize;
                    let dst_x = tile_x0.saturating_sub(x) as usize;
                    let dst_y = tile_y0.saturating_sub(y) as usize;
                    let copy_w = ((info.tile_width - src_x as u32).min(w - dst_x as u32)) as usize;
                    let copy_h = ((info.tile_height - src_y as u32).min(h - dst_y as u32)) as usize;
                    let copy_bytes = copy_w * bytes_per_sample as usize;

                    for row in 0..copy_h {
                        let src_off =
                            ((src_y + row) * tile_row_bytes) + src_x * bytes_per_sample as usize;
                        let dst_off =
                            ((dst_y + row) * out_row_bytes) + dst_x * bytes_per_sample as usize;
                        if src_off + copy_bytes <= tile_data.len()
                            && dst_off + copy_bytes <= channel_out.len()
                        {
                            channel_out[dst_off..dst_off + copy_bytes]
                                .copy_from_slice(&tile_data[src_off..src_off + copy_bytes]);
                        }
                    }
                }
            }
            out.extend_from_slice(&channel_out);
        }

        Ok(out)
    }
}

fn pixel_type_from_bps_format(
    bps: u16,
    sample_format: u16,
) -> crate::common::pixel_type::PixelType {
    use crate::common::pixel_type::PixelType;
    match (bps, sample_format) {
        (1, _) => PixelType::Bit,
        (8, 2) => PixelType::Int8,
        (8, _) => PixelType::Uint8,
        (16, 2) => PixelType::Int16,
        (16, _) => PixelType::Uint16,
        (32, 2) => PixelType::Int32,
        (32, 3) => PixelType::Float32,
        (32, _) => PixelType::Uint32,
        (64, 3) => PixelType::Float64,
        _ => PixelType::Uint8,
    }
}

fn validate_region(info: &IfdInfo, x: u32, y: u32, w: u32, h: u32) -> Result<()> {
    if w == 0 || h == 0 {
        return Err(BioFormatsError::Format(
            "TIFF region width and height must be non-zero".into(),
        ));
    }

    let x_end = x
        .checked_add(w)
        .ok_or_else(|| BioFormatsError::Format("TIFF region x range overflows".into()))?;
    let y_end = y
        .checked_add(h)
        .ok_or_else(|| BioFormatsError::Format("TIFF region y range overflows".into()))?;

    if x >= info.width || x_end > info.width || y >= info.height || y_end > info.height {
        return Err(BioFormatsError::Format(format!(
            "TIFF region x={x}, y={y}, w={w}, h={h} is outside image bounds {}x{}",
            info.width, info.height
        )));
    }

    Ok(())
}

fn validate_tiff_storage(
    width: u32,
    height: u32,
    samples_per_pixel: u16,
    planar_config: u16,
    is_tiled: bool,
    tile_width: u32,
    tile_height: u32,
    rows_per_strip: u32,
    strip_offsets: &[u64],
    strip_byte_counts: &[u64],
    tile_offsets: &[u64],
    tile_byte_counts: &[u64],
) -> Result<()> {
    if width == 0 || height == 0 {
        return Err(BioFormatsError::Format(
            "TIFF ImageWidth and ImageLength must be non-zero".into(),
        ));
    }
    if samples_per_pixel == 0 {
        return Err(BioFormatsError::Format(
            "TIFF SamplesPerPixel must be non-zero".into(),
        ));
    }

    let planes = if planar_config == 2 {
        samples_per_pixel as usize
    } else {
        1
    };

    if is_tiled {
        if tile_width == 0 {
            return Err(BioFormatsError::Format(
                "TIFF TileWidth is required and must be non-zero".into(),
            ));
        }
        if tile_height == 0 {
            return Err(BioFormatsError::Format(
                "TIFF TileLength is required and must be non-zero".into(),
            ));
        }
        if tile_offsets.is_empty() {
            return Err(BioFormatsError::Format("TIFF missing TileOffsets".into()));
        }
        if tile_byte_counts.is_empty() {
            return Err(BioFormatsError::Format(
                "TIFF missing TileByteCounts".into(),
            ));
        }
        if tile_offsets.len() != tile_byte_counts.len() {
            return Err(BioFormatsError::Format(format!(
                "TIFF TileOffsets count {} does not match TileByteCounts count {}",
                tile_offsets.len(),
                tile_byte_counts.len()
            )));
        }

        let tiles_across = div_ceil_u32(width, tile_width) as usize;
        let tiles_down = div_ceil_u32(height, tile_height) as usize;
        let expected = tiles_across
            .checked_mul(tiles_down)
            .and_then(|v| v.checked_mul(planes))
            .ok_or_else(|| BioFormatsError::Format("TIFF tile count overflows usize".into()))?;
        if tile_offsets.len() != expected {
            return Err(BioFormatsError::Format(format!(
                "TIFF TileOffsets/TileByteCounts count {} does not match expected tile count {}",
                tile_offsets.len(),
                expected
            )));
        }
    } else {
        if rows_per_strip == 0 {
            return Err(BioFormatsError::Format(
                "TIFF RowsPerStrip is required and must be non-zero".into(),
            ));
        }
        if strip_offsets.is_empty() {
            return Err(BioFormatsError::Format("TIFF missing StripOffsets".into()));
        }
        if strip_byte_counts.is_empty() {
            return Err(BioFormatsError::Format(
                "TIFF missing StripByteCounts".into(),
            ));
        }
        if strip_offsets.len() != strip_byte_counts.len() {
            return Err(BioFormatsError::Format(format!(
                "TIFF StripOffsets count {} does not match StripByteCounts count {}",
                strip_offsets.len(),
                strip_byte_counts.len()
            )));
        }

        let strips_per_plane = div_ceil_u32(height, rows_per_strip) as usize;
        let expected = strips_per_plane
            .checked_mul(planes)
            .ok_or_else(|| BioFormatsError::Format("TIFF strip count overflows usize".into()))?;
        if strip_offsets.len() != expected {
            return Err(BioFormatsError::Format(format!(
                "TIFF StripOffsets/StripByteCounts count {} does not match expected strip count {}",
                strip_offsets.len(),
                expected
            )));
        }
    }

    Ok(())
}

fn div_ceil_u32(value: u32, divisor: u32) -> u32 {
    value / divisor + u32::from(value % divisor != 0)
}

fn parse_ome_tiff_images(xml: &str) -> Vec<OmeTiffImage> {
    let mut images = Vec::new();
    let image_positions = start_tag_positions(xml, "Image");

    for (image_idx, &image_pos) in image_positions.iter().enumerate() {
        let image_end = image_positions
            .get(image_idx + 1)
            .copied()
            .unwrap_or(xml.len());
        let image_xml = &xml[image_pos..image_end];
        let Some(pixels_rel) = start_tag_positions(image_xml, "Pixels").first().copied() else {
            continue;
        };
        let pixels_tag = start_tag_at(image_xml, pixels_rel);

        let size_x = parse_u32_attr(pixels_tag, "SizeX").unwrap_or(0);
        let size_y = parse_u32_attr(pixels_tag, "SizeY").unwrap_or(0);
        let size_z = parse_u32_attr(pixels_tag, "SizeZ").unwrap_or(1);
        let size_c = parse_u32_attr(pixels_tag, "SizeC").unwrap_or(1);
        let size_t = parse_u32_attr(pixels_tag, "SizeT").unwrap_or(1);
        if size_x == 0 || size_y == 0 {
            continue;
        }

        let dimension_order = xml_attr(pixels_tag, "DimensionOrder")
            .as_deref()
            .map(parse_dimension_order)
            .unwrap_or_default();
        let (pixel_type, bits_per_pixel) = xml_attr(pixels_tag, "Type")
            .as_deref()
            .map(parse_ome_pixel_type)
            .unwrap_or((PixelType::Uint8, 8));

        let pixels_end =
            matching_end_tag_start(image_xml, pixels_rel, "Pixels").unwrap_or(image_xml.len());
        let pixels_xml = &image_xml[pixels_rel..pixels_end];
        let samples_per_pixel = start_tag_positions(pixels_xml, "Channel")
            .into_iter()
            .filter_map(|pos| parse_u32_attr(start_tag_at(pixels_xml, pos), "SamplesPerPixel"))
            .next()
            .unwrap_or(1)
            .max(1);
        let effective_c = if samples_per_pixel > 1 && size_c % samples_per_pixel == 0 {
            size_c / samples_per_pixel
        } else {
            size_c
        }
        .max(1);

        let tiff_data = start_tag_positions(pixels_xml, "TiffData")
            .into_iter()
            .map(|pos| {
                let tag = start_tag_at(pixels_xml, pos);
                OmeTiffData {
                    ifd: parse_u32_attr(tag, "IFD").unwrap_or(0) as usize,
                    plane_count: parse_u32_attr(tag, "PlaneCount").map(|v| v as usize),
                    first_z: parse_u32_attr(tag, "FirstZ").unwrap_or(0),
                    first_c: parse_u32_attr(tag, "FirstC").unwrap_or(0),
                    first_t: parse_u32_attr(tag, "FirstT").unwrap_or(0),
                }
            })
            .collect();

        images.push(OmeTiffImage {
            size_x,
            size_y,
            size_z,
            size_c,
            effective_c,
            size_t,
            pixel_type,
            bits_per_pixel,
            dimension_order,
            tiff_data,
        });
    }

    images
}

fn build_ome_plane_map(image: &OmeTiffImage, physical_ifd_count: usize) -> Vec<Option<usize>> {
    let plane_count = image
        .size_z
        .saturating_mul(image.effective_c)
        .saturating_mul(image.size_t) as usize;
    let mut map = vec![None; plane_count];
    if plane_count == 0 {
        return map;
    }

    if image.tiff_data.is_empty() {
        for (plane, slot) in map.iter_mut().enumerate() {
            if plane < physical_ifd_count {
                *slot = Some(plane);
            }
        }
        return map;
    }

    let mut explicit_starts = vec![false; plane_count];
    for td in &image.tiff_data {
        if let Some(logical) = ome_plane_index(
            td.first_z,
            td.first_c,
            td.first_t,
            image.size_z,
            image.effective_c,
            image.size_t,
            image.dimension_order,
        ) {
            explicit_starts[logical] = true;
            if td.ifd < physical_ifd_count {
                map[logical] = Some(td.ifd);
            }
        }
    }

    for td in &image.tiff_data {
        let Some(start_logical) = ome_plane_index(
            td.first_z,
            td.first_c,
            td.first_t,
            image.size_z,
            image.effective_c,
            image.size_t,
            image.dimension_order,
        ) else {
            continue;
        };
        let limit = td
            .plane_count
            .unwrap_or_else(|| plane_count.saturating_sub(start_logical));
        let mut z = td.first_z;
        let mut c = td.first_c;
        let mut t = td.first_t;
        for offset in 0..limit {
            if offset > 0 && !advance_ome_plane(&mut z, &mut c, &mut t, image) {
                break;
            }
            let Some(logical) = ome_plane_index(
                z,
                c,
                t,
                image.size_z,
                image.effective_c,
                image.size_t,
                image.dimension_order,
            ) else {
                break;
            };
            if offset > 0 && td.plane_count.is_none() && explicit_starts[logical] {
                break;
            }
            let ifd_index = td.ifd + offset;
            if ifd_index >= physical_ifd_count {
                break;
            }
            map[logical] = Some(ifd_index);
        }
    }

    map
}

fn xml_attr(tag_text: &str, attr: &str) -> Option<String> {
    let bytes = tag_text.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        while i < bytes.len() && !is_xml_name_start(bytes[i]) {
            i += 1;
        }
        let name_start = i;
        while i < bytes.len() && is_xml_name_char(bytes[i]) {
            i += 1;
        }
        if name_start == i {
            break;
        }
        let name = &tag_text[name_start..i];
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            continue;
        }
        i += 1;
        while i < bytes.len() && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || (bytes[i] != b'"' && bytes[i] != b'\'') {
            continue;
        }
        let quote = bytes[i];
        let value_start = i + 1;
        i = value_start;
        while i < bytes.len() && bytes[i] != quote {
            i += 1;
        }
        if local_xml_name(name).eq_ignore_ascii_case(attr) {
            return Some(tag_text[value_start..i].to_string());
        }
        i += usize::from(i < bytes.len());
    }
    None
}

fn parse_u32_attr(tag_text: &str, attr: &str) -> Option<u32> {
    xml_attr(tag_text, attr).and_then(|s| s.parse().ok())
}

fn start_tag_at(xml: &str, pos: usize) -> &str {
    let tail = &xml[pos..];
    let end = xml_tag_end(tail, 0).unwrap_or(tail.len());
    &tail[..end]
}

#[derive(Debug, Clone, Copy)]
struct XmlTag<'a> {
    start: usize,
    end: usize,
    name: &'a str,
    closing: bool,
    self_closing: bool,
}

fn start_tag_positions(xml: &str, local_name: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut search_from = 0;
    while let Some(tag) = next_xml_tag(xml, search_from) {
        if !tag.closing && local_xml_name(tag.name).eq_ignore_ascii_case(local_name) {
            out.push(tag.start);
        }
        search_from = tag.end;
    }
    out
}

fn is_ome_xml_description(xml: &str) -> bool {
    start_tag_positions(xml.trim_start(), "OME")
        .first()
        .is_some()
}

fn matching_end_tag_start(xml: &str, open_pos: usize, local_name: &str) -> Option<usize> {
    let open = parse_xml_tag_at(xml, open_pos)?;
    if open.self_closing {
        return Some(open.end);
    }

    let mut depth = 1usize;
    let mut search_from = open.end;
    while let Some(tag) = next_xml_tag(xml, search_from) {
        if local_xml_name(tag.name).eq_ignore_ascii_case(local_name) {
            if tag.closing {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(tag.start);
                }
            } else if !tag.self_closing {
                depth += 1;
            }
        }
        search_from = tag.end;
    }
    None
}

fn next_xml_tag<'a>(xml: &'a str, from: usize) -> Option<XmlTag<'a>> {
    let mut search_from = from;
    while let Some(rel) = xml.get(search_from..)?.find('<') {
        let pos = search_from + rel;
        if let Some(tag) = parse_xml_tag_at(xml, pos) {
            return Some(tag);
        }
        search_from = pos + 1;
    }
    None
}

fn parse_xml_tag_at<'a>(xml: &'a str, pos: usize) -> Option<XmlTag<'a>> {
    let bytes = xml.as_bytes();
    if bytes.get(pos).copied()? != b'<' {
        return None;
    }

    let mut i = pos + 1;
    let closing = bytes.get(i).copied() == Some(b'/');
    if closing {
        i += 1;
    }
    if matches!(bytes.get(i).copied(), Some(b'!' | b'?')) {
        return None;
    }
    if !bytes.get(i).copied().is_some_and(is_xml_name_start) {
        return None;
    }

    let name_start = i;
    while bytes.get(i).copied().is_some_and(is_xml_name_char) {
        i += 1;
    }
    let end = xml_tag_end(xml, pos)?;
    let before_close = xml[pos..end - 1].trim_end();
    Some(XmlTag {
        start: pos,
        end,
        name: &xml[name_start..i],
        closing,
        self_closing: !closing && before_close.ends_with('/'),
    })
}

fn xml_tag_end(xml: &str, pos: usize) -> Option<usize> {
    let mut quote = None;
    for (rel, ch) in xml.get(pos..)?.char_indices().skip(1) {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(ch),
            (None, '>') => return Some(pos + rel + ch.len_utf8()),
            _ => {}
        }
    }
    None
}

fn local_xml_name(name: &str) -> &str {
    name.rsplit_once(':').map_or(name, |(_, local)| local)
}

fn is_xml_name_start(byte: u8) -> bool {
    byte.is_ascii_alphabetic() || byte == b'_' || byte == b':'
}

fn is_xml_name_char(byte: u8) -> bool {
    is_xml_name_start(byte) || byte.is_ascii_digit() || byte == b'-' || byte == b'.'
}

fn parse_dimension_order(value: &str) -> DimensionOrder {
    match value {
        "XYZCT" => DimensionOrder::XYZCT,
        "XYZTC" => DimensionOrder::XYZTC,
        "XYCTZ" => DimensionOrder::XYCTZ,
        "XYTCZ" => DimensionOrder::XYTCZ,
        "XYTZC" => DimensionOrder::XYTZC,
        _ => DimensionOrder::XYCZT,
    }
}

fn parse_ome_pixel_type(value: &str) -> (PixelType, u8) {
    match value.to_ascii_lowercase().as_str() {
        "bit" => (PixelType::Bit, 1),
        "int8" => (PixelType::Int8, 8),
        "uint8" => (PixelType::Uint8, 8),
        "int16" => (PixelType::Int16, 16),
        "uint16" => (PixelType::Uint16, 16),
        "int32" => (PixelType::Int32, 32),
        "uint32" => (PixelType::Uint32, 32),
        "float" | "float32" => (PixelType::Float32, 32),
        "double" | "float64" => (PixelType::Float64, 64),
        _ => (PixelType::Uint8, 8),
    }
}

fn ome_plane_index(
    z: u32,
    c: u32,
    t: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    order: DimensionOrder,
) -> Option<usize> {
    if z >= size_z || c >= size_c || t >= size_t {
        return None;
    }
    Some(match order {
        DimensionOrder::XYZCT => t * size_z * size_c + c * size_z + z,
        DimensionOrder::XYZTC => c * size_z * size_t + t * size_z + z,
        DimensionOrder::XYCZT => t * size_c * size_z + z * size_c + c,
        DimensionOrder::XYCTZ => z * size_c * size_t + t * size_c + c,
        DimensionOrder::XYTCZ => z * size_t * size_c + c * size_t + t,
        DimensionOrder::XYTZC => c * size_t * size_z + z * size_t + t,
    } as usize)
}

fn advance_ome_plane(z: &mut u32, c: &mut u32, t: &mut u32, image: &OmeTiffImage) -> bool {
    fn advance_axis(value: &mut u32, limit: u32) -> bool {
        *value += 1;
        if *value < limit {
            true
        } else {
            *value = 0;
            false
        }
    }

    match image.dimension_order {
        DimensionOrder::XYZCT => {
            advance_axis(z, image.size_z)
                || advance_axis(c, image.effective_c)
                || advance_axis(t, image.size_t)
        }
        DimensionOrder::XYZTC => {
            advance_axis(z, image.size_z)
                || advance_axis(t, image.size_t)
                || advance_axis(c, image.effective_c)
        }
        DimensionOrder::XYCZT => {
            advance_axis(c, image.effective_c)
                || advance_axis(z, image.size_z)
                || advance_axis(t, image.size_t)
        }
        DimensionOrder::XYCTZ => {
            advance_axis(c, image.effective_c)
                || advance_axis(t, image.size_t)
                || advance_axis(z, image.size_z)
        }
        DimensionOrder::XYTCZ => {
            advance_axis(t, image.size_t)
                || advance_axis(c, image.effective_c)
                || advance_axis(z, image.size_z)
        }
        DimensionOrder::XYTZC => {
            advance_axis(t, image.size_t)
                || advance_axis(z, image.size_z)
                || advance_axis(c, image.effective_c)
        }
    }
}

fn packed_row_bytes(width: u32, samples_per_pixel: u32, bits_per_sample: u16) -> usize {
    (width as usize * samples_per_pixel as usize * bits_per_sample as usize + 7) / 8
}

fn unpack_subbyte_samples(
    data: &[u8],
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    bits_per_sample: u16,
) -> Vec<u8> {
    let row_samples = width as usize * samples_per_pixel as usize;
    let row_bytes = packed_row_bytes(width, samples_per_pixel, bits_per_sample);
    let mask = ((1u16 << bits_per_sample) - 1) as u8;
    let mut out = Vec::with_capacity(row_samples * height as usize);

    for row in 0..height as usize {
        let row_start = row * row_bytes;
        let row_data = data.get(row_start..row_start + row_bytes).unwrap_or(&[]);
        for sample in 0..row_samples {
            let bit = sample * bits_per_sample as usize;
            let byte = row_data.get(bit / 8).copied().unwrap_or(0);
            let shift = 8 - bits_per_sample as usize - (bit % 8);
            out.push((byte >> shift) & mask);
        }
    }

    out
}

fn crop_unpacked_rows(
    data: &[u8],
    full_width: u32,
    samples_per_pixel: u32,
    x: u32,
    w: u32,
    h: u32,
) -> Vec<u8> {
    if x == 0 && w == full_width {
        return data.to_vec();
    }

    let full_row = full_width as usize * samples_per_pixel as usize;
    let x_start = x as usize * samples_per_pixel as usize;
    let x_len = w as usize * samples_per_pixel as usize;
    let mut out = Vec::with_capacity(h as usize * x_len);
    for row in 0..h as usize {
        let start = row * full_row + x_start;
        let end = start + x_len;
        if end <= data.len() {
            out.extend_from_slice(&data[start..end]);
        }
    }
    out
}

fn apply_photometric(
    data: &mut [u8],
    photometric: Photometric,
    bits_per_sample: u16,
    little_endian: bool,
) {
    if photometric != Photometric::MinIsWhite && photometric != Photometric::Cmyk {
        return;
    }

    match bits_per_sample {
        1 | 2 | 4 => {
            let max = ((1u16 << bits_per_sample) - 1) as u8;
            for b in data {
                *b = max.saturating_sub(*b);
            }
        }
        8 => {
            for b in data {
                *b = 255u8.wrapping_sub(*b);
            }
        }
        16 => {
            for px in data.chunks_exact_mut(2) {
                let value = if little_endian {
                    u16::from_le_bytes([px[0], px[1]])
                } else {
                    u16::from_be_bytes([px[0], px[1]])
                };
                let inverted = u16::MAX.wrapping_sub(value);
                if little_endian {
                    px.copy_from_slice(&inverted.to_le_bytes());
                } else {
                    px.copy_from_slice(&inverted.to_be_bytes());
                }
            }
        }
        _ => {}
    }
}

fn is_unsupported_ycbcr(info: &IfdInfo) -> bool {
    info.bits_per_sample != 8
        || info.planar_config != 1
        || info.samples_per_pixel < 3
        || matches!(
            info.compression,
            Compression::Jpeg | Compression::JpegNew | Compression::JpegXR
        )
}

fn ycbcr_row_bytes(width: u32, rows: u32, subsampling: (u16, u16)) -> u32 {
    ycbcr_strip_bytes(width, rows, subsampling) as u32
}

fn ycbcr_strip_bytes(width: u32, rows: u32, subsampling: (u16, u16)) -> usize {
    let h = subsampling.0.max(1) as u32;
    let v = subsampling.1.max(1) as u32;
    let blocks_x = (width + h - 1) / h;
    let blocks_y = (rows + v - 1) / v;
    (blocks_x * blocks_y * (h * v + 2)) as usize
}

fn decode_ycbcr_chunky(
    data: &[u8],
    width: u32,
    height: u32,
    subsampling: (u16, u16),
    coefficients: (f32, f32, f32),
) -> Result<Vec<u8>> {
    let hsub = subsampling.0.max(1) as u32;
    let vsub = subsampling.1.max(1) as u32;
    let mut r = vec![0u8; (width * height) as usize];
    let mut g = vec![0u8; (width * height) as usize];
    let mut b = vec![0u8; (width * height) as usize];
    let mut offset = 0usize;

    for block_y in (0..height).step_by(vsub as usize) {
        for block_x in (0..width).step_by(hsub as usize) {
            let y_count = (hsub * vsub) as usize;
            if offset + y_count + 2 > data.len() {
                return Err(BioFormatsError::Format(
                    "TIFF YCbCr block is shorter than expected".into(),
                ));
            }
            let y_values = &data[offset..offset + y_count];
            let cb = data[offset + y_count] as f32;
            let cr = data[offset + y_count + 1] as f32;
            offset += y_count + 2;

            for yy in 0..vsub {
                for xx in 0..hsub {
                    let x = block_x + xx;
                    let y = block_y + yy;
                    if x >= width || y >= height {
                        continue;
                    }
                    let y_sample = y_values[(yy * hsub + xx) as usize] as f32;
                    let (rr, gg, bb) = ycbcr_to_rgb(y_sample, cb, cr, coefficients);
                    let idx = (y * width + x) as usize;
                    r[idx] = rr;
                    g[idx] = gg;
                    b[idx] = bb;
                }
            }
        }
    }

    let mut out = Vec::with_capacity((width * height * 3) as usize);
    out.extend_from_slice(&r);
    out.extend_from_slice(&g);
    out.extend_from_slice(&b);
    Ok(out)
}

fn ycbcr_to_rgb(y: f32, cb: f32, cr: f32, coefficients: (f32, f32, f32)) -> (u8, u8, u8) {
    let (luma_red, luma_green, luma_blue) = coefficients;
    let cr = cr - 128.0;
    let cb = cb - 128.0;
    let red = y + cr * (2.0 - 2.0 * luma_red);
    let blue = y + cb * (2.0 - 2.0 * luma_blue);
    let green = (y - luma_blue * blue - luma_red * red) / luma_green;
    (clamp_u8(red), clamp_u8(green), clamp_u8(blue))
}

fn clamp_u8(value: f32) -> u8 {
    value.round().clamp(0.0, 255.0) as u8
}

// ---- FormatReader impl ----

impl crate::common::reader::FormatReader for TiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(
            ext.as_deref(),
            Some("tif")
                | Some("tiff")
                | Some("ome.tif")
                | Some("ome.tiff")
                | Some("btf")
                | Some("tf8")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 4 {
            return false;
        }
        // II 42 00 or MM 00 42 — classic TIFF
        // II 43 00 or MM 00 43 — BigTIFF
        (header[0..2] == [0x49, 0x49] || header[0..2] == [0x4D, 0x4D])
            && (header[2..4] == [42, 0]
                || header[2..4] == [0, 42]
                || header[2..4] == [43, 0]
                || header[2..4] == [0, 43])
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let buf = BufReader::new(f);
        let parser = TiffParser::new(buf)?;
        let little_endian = parser.little_endian;

        // We need to read IFDs. Move parser into a temporary to call read_ifds.
        let mut tf = TiffFile {
            parser,
            ifds: Vec::new(),
        };
        tf.ifds = tf.parser.read_ifds()?;
        for ifd in &tf.ifds {
            Self::ifd_info(ifd, little_endian)?;
        }
        self.series = Self::build_series(&tf.ifds, little_endian);
        // Detect OME-TIFF: OME-XML is stored in the first IFD's ImageDescription.
        self.ome_xml = self
            .series
            .first()
            .and_then(|s| s.metadata.series_metadata.get("ImageDescription"))
            .and_then(|v| {
                if let crate::common::metadata::MetadataValue::String(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .filter(|desc| is_ome_xml_description(desc))
            .map(str::to_owned);
        if let Some(xml) = self.ome_xml.as_deref() {
            if let Some(ome_series) = Self::build_ome_series(&tf.ifds, xml, little_endian) {
                self.series = ome_series;
            }
        }
        self.file = Some(tf);
        self.current_series = 0;
        self.current_resolution = 0;
        // Parse SubIFD chains for pyramid support
        self.parse_sub_ifds()?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.file = None;
        self.series.clear();
        self.ome_xml = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }

    fn set_series(&mut self, series: usize) -> Result<()> {
        if series >= self.series.len() {
            return Err(BioFormatsError::SeriesOutOfRange(series));
        }
        self.current_series = series;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &crate::common::metadata::ImageMetadata {
        &self.series[self.current_series].metadata
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let ifd_index = self.resolve_ifd_index(plane_index)?;
        let file = self.file.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let ifd = &file.ifds[ifd_index];
        let w = ifd.image_width().unwrap_or(0);
        let h = ifd.image_length().unwrap_or(0);
        self.read_plane_bytes(ifd_index, 0, 0, w, h)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let ifd_index = self.resolve_ifd_index(plane_index)?;
        self.read_plane_bytes(ifd_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        // Return a small center crop (max 256x256) as a thumbnail.
        let ifd_index = self.resolve_ifd_index(plane_index)?;
        let file = self.file.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let ifd = &file.ifds[ifd_index];
        let w = ifd.image_width().unwrap_or(0);
        let h = ifd.image_length().unwrap_or(0);
        let tw = w.min(256);
        let th = h.min(256);
        let tx = (w - tw) / 2;
        let ty = (h - th) / 2;
        self.read_plane_bytes(ifd_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        let s = &self.series[self.current_series];
        1 + s.sub_resolutions.len()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        let s = &self.series[self.current_series];
        let max = 1 + s.sub_resolutions.len();
        if level >= max {
            return Err(BioFormatsError::Format(format!(
                "resolution level {} out of range (max {})",
                level,
                max - 1
            )));
        }
        self.current_resolution = level;
        Ok(())
    }

    fn resolution(&self) -> usize {
        self.current_resolution
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        self.ome_xml
            .as_deref()
            .map(crate::common::ome_metadata::OmeMetadata::from_ome_xml)
    }
}
