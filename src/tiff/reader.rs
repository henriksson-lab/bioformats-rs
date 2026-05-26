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
use super::nikon::NikonCompressionOptions;
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
    fill_order: u16,
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
    nikon_compression_options: Option<NikonCompressionOptions>,
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

    /// Replace the entire series list (used by vendor wrappers such as SVS that
    /// need to regroup the IFD chain into resolution levels of a single series).
    /// Resets the current series/resolution to 0.
    pub fn replace_series(&mut self, series: Vec<TiffSeries>) {
        self.series = series;
        self.current_series = 0;
        self.current_resolution = 0;
    }

    /// Number of parsed IFDs in the open file.
    pub fn ifd_count(&self) -> usize {
        self.file.as_ref().map(|f| f.ifds.len()).unwrap_or(0)
    }

    /// Whether the file was parsed as little-endian.
    pub fn is_little_endian(&self) -> bool {
        self.file.as_ref().map(|f| f.parser.little_endian).unwrap_or(true)
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
        // FillOrder (tag 266); default 1 (MSB-to-LSB). 2 means bits within each
        // byte are stored LSB-to-MSB and must be reversed before decompression.
        let fill_order = ifd.get_u16(tag::FILL_ORDER).unwrap_or(1);

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
            fill_order,
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
            nikon_compression_options: None,
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

    fn add_nikon_raw_sub_ifd_series(&mut self) -> Result<()> {
        let file = self.file.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let little_endian = file.parser.little_endian;
        let mut assigned_ifds: Vec<usize> = Vec::new();
        for series in &self.series {
            assigned_ifds.extend(series.ifd_indices.iter().copied());
            assigned_ifds.extend(series.plane_ifd_indices.iter().filter_map(|&idx| idx));
        }

        let mut raw_series = Vec::new();
        for (ifd_index, ifd) in file.ifds.iter().enumerate() {
            if assigned_ifds.contains(&ifd_index) || ifd.compression() != Compression::Nikon {
                continue;
            }
            let info = Self::ifd_info(ifd, little_endian)?;
            raw_series.push(Self::single_ifd_series(ifd_index, &info, little_endian));
            assigned_ifds.push(ifd_index);
        }

        self.series.extend(raw_series);
        Ok(())
    }

    /// Re-group the main IFD chain into Aperio SVS series, mirroring
    /// `SVSReader.java`. The largest IFDs become the resolution levels of a
    /// single pyramid series; label/macro images become trailing extra series.
    ///
    /// Classification follows SVSReader: an IFD with no ImageDescription comment
    /// is assigned to label, then macro. An IFD whose comment contains "label"
    /// or "macro" is tagged accordingly. Otherwise, if NewSubfileType != 0 and
    /// neither label nor macro has been found yet, it is treated as label/macro.
    /// Resolution levels with a pixel type differing from the full-resolution
    /// image are dropped (as in SVSReader).
    pub fn regroup_as_svs_pyramid(&mut self) -> Result<()> {
        let little_endian = self.is_little_endian();

        // Collect the main IFD chain in order. Each pre-existing series maps to
        // one top-level IFD (SVS stores its pyramid as the main IFD chain).
        let main_ifds: Vec<usize> = self
            .series
            .iter()
            .filter_map(|s| s.ifd_indices.first().copied())
            .collect();
        if main_ifds.len() <= 1 {
            return Ok(());
        }

        let file = self.file.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let mut label_index: Option<usize> = None;
        let mut macro_index: Option<usize> = None;

        for (i, &ifd_idx) in main_ifds.iter().enumerate() {
            let ifd = &file.ifds[ifd_idx];
            let comment = ifd.get_str(tag::IMAGE_DESCRIPTION);
            let subfile_type = ifd.get_u32(tag::NEW_SUBFILE_TYPE).unwrap_or(0);

            match comment {
                None => {
                    if label_index.is_none() {
                        label_index = Some(i);
                    } else if macro_index.is_none() {
                        macro_index = Some(i);
                    }
                    continue;
                }
                Some(comment) => {
                    let mut found_label = false;
                    let mut found_macro = false;
                    // Java only checks tokens without '=' for label/macro names.
                    for line in comment.split('\n') {
                        for tok in line.split('|') {
                            if tok.contains('=') {
                                continue;
                            }
                            let t = tok.to_ascii_lowercase();
                            if t.contains("label") {
                                label_index = Some(i);
                                found_label = true;
                            } else if t.contains("macro") {
                                macro_index = Some(i);
                                found_macro = true;
                            }
                        }
                    }
                    if !found_label && !found_macro && subfile_type != 0 {
                        if label_index.is_none() {
                            label_index = Some(i);
                        } else if macro_index.is_none() {
                            macro_index = Some(i);
                        }
                    }
                }
            }
        }

        // Resolution images are the main IFDs that are NOT label/macro, in order.
        let extra: Vec<usize> = [label_index, macro_index].into_iter().flatten().collect();
        let resolution_positions: Vec<usize> = (0..main_ifds.len())
            .filter(|i| !extra.contains(i))
            .collect();
        if resolution_positions.is_empty() {
            return Ok(());
        }

        // Full-resolution IFD = first resolution image.
        let full_ifd_idx = main_ifds[resolution_positions[0]];
        let full_info = Self::ifd_info(&file.ifds[full_ifd_idx], little_endian)?;

        // Drop pyramid levels whose pixel type differs from the full resolution.
        let mut kept_levels: Vec<usize> = Vec::new();
        for &pos in &resolution_positions {
            let ifd_idx = main_ifds[pos];
            let info = Self::ifd_info(&file.ifds[ifd_idx], little_endian)?;
            if ifd_idx == full_ifd_idx || info.pixel_type == full_info.pixel_type {
                kept_levels.push(ifd_idx);
            }
        }
        if kept_levels.is_empty() {
            return Ok(());
        }

        // Build the single pyramid series: level 0 is the main resolution,
        // remaining levels are sub-resolutions.
        let mut pyramid = Self::single_ifd_series(kept_levels[0], &full_info, little_endian);
        let sub_resolutions: Vec<Vec<usize>> =
            kept_levels[1..].iter().map(|&idx| vec![idx]).collect();
        pyramid.metadata.resolution_count = 1 + sub_resolutions.len() as u32;
        pyramid.sub_resolutions = sub_resolutions;
        if let Some(desc) = file.ifds[kept_levels[0]].get_str(tag::IMAGE_DESCRIPTION) {
            pyramid.metadata.series_metadata.insert(
                "ImageDescription".into(),
                crate::common::metadata::MetadataValue::String(desc.to_string()),
            );
        }

        let mut new_series = vec![pyramid];

        // Append label/macro as standalone series, label first then macro.
        for &pos in &extra {
            let ifd_idx = main_ifds[pos];
            let info = Self::ifd_info(&file.ifds[ifd_idx], little_endian)?;
            let mut s = Self::single_ifd_series(ifd_idx, &info, little_endian);
            let name = if Some(pos) == label_index {
                "label"
            } else {
                "macro"
            };
            s.metadata.series_metadata.insert(
                "svs.image_type".into(),
                crate::common::metadata::MetadataValue::String(name.to_string()),
            );
            new_series.push(s);
        }

        self.replace_series(new_series);
        Ok(())
    }

    fn single_ifd_series(ifd_index: usize, info: &IfdInfo, little_endian: bool) -> TiffSeries {
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

        let mut metadata = crate::common::metadata::ImageMetadata {
            size_x: info.width,
            size_y: info.height,
            size_z: 1,
            size_c,
            size_t: 1,
            pixel_type: info.pixel_type,
            bits_per_pixel: info.bits_per_sample as u8,
            image_count: 1,
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
        metadata.series_metadata.insert(
            "NikonRawSubIFD".into(),
            crate::common::metadata::MetadataValue::Bool(true),
        );

        TiffSeries {
            ifd_indices: vec![ifd_index],
            plane_ifd_indices: Vec::new(),
            metadata,
            sub_resolutions: Vec::new(),
        }
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
        let mut info = Self::ifd_info(ifd, little_endian)?;
        if info.compression == Compression::Nikon {
            info.nikon_compression_options = super::nikon::extract_compression_options(
                &mut file.parser,
                &file.ifds,
                info.bits_per_sample,
            )?;
        }
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
        let packed_row_layout = info.bits_per_sample % 8 != 0;
        let subbyte_samples = info.bits_per_sample < 8;
        let ycbcr = info.photometric == Photometric::YCbCr;
        let row_bytes = if ycbcr {
            ycbcr_row_bytes(info.width, 1, info.ycbcr_subsampling)
        } else if packed_row_layout {
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
        // For subsampled YCbCr, RGB is accumulated per-plane (R then G then B) as
        // strips are decoded, so partial-row reads (a strip overlapping only part of
        // the requested y range) work like Java's per-block unpacking.
        let mut ycbcr_r: Vec<u8> = Vec::new();
        let mut ycbcr_g: Vec<u8> = Vec::new();
        let mut ycbcr_b: Vec<u8> = Vec::new();

        for strip_idx in 0..info.strip_offsets.len() {
            let strip_start_row = strip_idx as u32 * rows_per_strip;
            let strip_end_row = (strip_start_row + rows_per_strip).min(info.height);

            // Skip strips entirely above or below the requested region
            if strip_end_row <= y || strip_start_row >= y + h {
                continue;
            }

            let offset = info.strip_offsets[strip_idx];
            let byte_count = info.strip_byte_counts[strip_idx] as usize;

            let mut compressed = read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
            apply_fill_order(&mut compressed, info.fill_order, info.compression);
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
                info.nikon_compression_options.as_ref(),
            )?;
            strip_data.truncate(expected);

            // Crop rows within this strip to the requested y range
            let row_start = y.saturating_sub(strip_start_row) as usize;
            let row_end = (y + h - strip_start_row).min(strip_rows) as usize;

            if ycbcr {
                // Decode this strip's subsampled YCbCr to planar RGB covering the
                // strip's full height, then keep only the requested rows. This
                // supports partial-row reads of strips spanning subsampling blocks.
                let rgb = decode_ycbcr_chunky(
                    &strip_data,
                    info.width,
                    strip_rows,
                    info.ycbcr_subsampling,
                    info.ycbcr_coefficients,
                )?;
                let plane_len = (info.width * strip_rows) as usize;
                let row_w = info.width as usize;
                let r_plane = &rgb[0..plane_len];
                let g_plane = &rgb[plane_len..2 * plane_len];
                let b_plane = &rgb[2 * plane_len..3 * plane_len];
                ycbcr_r.extend_from_slice(&r_plane[row_start * row_w..row_end * row_w]);
                ycbcr_g.extend_from_slice(&g_plane[row_start * row_w..row_end * row_w]);
                ycbcr_b.extend_from_slice(&b_plane[row_start * row_w..row_end * row_w]);
                continue;
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
            if x == 0 && w == info.width {
                let mut out =
                    Vec::with_capacity(ycbcr_r.len() + ycbcr_g.len() + ycbcr_b.len());
                out.extend_from_slice(&ycbcr_r);
                out.extend_from_slice(&ycbcr_g);
                out.extend_from_slice(&ycbcr_b);
                return Ok(out);
            }
            // Crop each plane (single sample per pixel) and emit planar R, G, B.
            let r = crop_unpacked_rows(&ycbcr_r, info.width, 1, x, w, h);
            let g = crop_unpacked_rows(&ycbcr_g, info.width, 1, x, w, h);
            let b = crop_unpacked_rows(&ycbcr_b, info.width, 1, x, w, h);
            let mut out = Vec::with_capacity(r.len() + g.len() + b.len());
            out.extend_from_slice(&r);
            out.extend_from_slice(&g);
            out.extend_from_slice(&b);
            return Ok(out);
        }

        if subbyte_samples {
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

        if packed_row_layout {
            // bits_per_sample is >= 8 but not a multiple of 8 (e.g. 12-bit). Java's
            // TiffParser.unpackBytes reads each sample MSB-first via readBits and
            // writes it as bytes_per_sample little/big-endian bytes, with per-row
            // skipBits padding. Unpack to byte-aligned samples so we can crop columns.
            let unpacked = unpack_packed_samples(
                &plane_rows,
                info.width,
                h,
                effective_spp,
                info.bits_per_sample,
                file.parser.little_endian,
            );
            let mut out =
                crop_unpacked_rows(&unpacked, info.width, effective_spp * bytes_per_sample, x, w, h);
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
        let subbyte = info.bits_per_sample < 8;
        // For sub-byte planar samples, each channel's row is stored as packed bits
        // (one sample per pixel) padded to a byte boundary, matching Java's per-row
        // skipBits handling in TiffParser.unpackBytes.
        let channel_row_bytes = if subbyte {
            packed_row_bytes(info.width, 1, info.bits_per_sample) as u32
        } else {
            info.width * bytes_per_sample
        };
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
                let mut compressed = read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
                apply_fill_order(&mut compressed, info.fill_order, info.compression);
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
                    info.nikon_compression_options.as_ref(),
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

            if subbyte {
                // Unpack the packed bits of this channel into one byte per sample,
                // then apply photometric inversion and crop, matching Java's
                // per-sample unpacking in TiffParser.unpackBytes.
                let mut unpacked =
                    unpack_subbyte_samples(&channel_rows, info.width, h, 1, info.bits_per_sample);
                apply_photometric(
                    &mut unpacked,
                    info.photometric,
                    info.bits_per_sample,
                    file.parser.little_endian,
                );
                let cropped = crop_unpacked_rows(&unpacked, info.width, 1, x, w, h);
                out.extend_from_slice(&cropped);
                continue;
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
            return self.read_planar_tiled_plane(info, x, y, w, h);
        }

        if info.photometric == Photometric::YCbCr {
            if is_unsupported_ycbcr(info) {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Only 8-bit chunky non-JPEG TIFF YCbCr is supported".into(),
                ));
            }
            return self.read_tiled_ycbcr_plane(info, x, y, w, h);
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
                let mut compressed = read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
                apply_fill_order(&mut compressed, info.fill_order, info.compression);
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
                    info.nikon_compression_options.as_ref(),
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

    /// Read a tiled, subsampled YCbCr plane (chunky, 8-bit, non-JPEG), decoding
    /// each tile to planar RGB and assembling planar R, G, B output. Mirrors the
    /// YCbCr handling in Java's TiffParser.getTile + unpackBytes.
    fn read_tiled_ycbcr_plane(
        &mut self,
        info: &IfdInfo,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let tiles_across = (info.width + info.tile_width - 1) / info.tile_width;
        // Each YCbCr tile stores subsampled blocks: (subX*subY) luma + 2 chroma.
        let tile_data_bytes =
            ycbcr_strip_bytes(info.tile_width, info.tile_height, info.ycbcr_subsampling);

        let tx_start = x / info.tile_width;
        let tx_end = (x + w + info.tile_width - 1) / info.tile_width;
        let ty_start = y / info.tile_height;
        let ty_end = (y + h + info.tile_height - 1) / info.tile_height;

        // Planar RGB output for the requested region.
        let plane_size = (w * h) as usize;
        let mut r_out = vec![0u8; plane_size];
        let mut g_out = vec![0u8; plane_size];
        let mut b_out = vec![0u8; plane_size];

        for ty in ty_start..ty_end {
            for tx in tx_start..tx_end {
                let tile_idx = (ty * tiles_across + tx) as usize;
                if tile_idx >= info.tile_offsets.len() || tile_idx >= info.tile_byte_counts.len() {
                    continue;
                }
                let offset = info.tile_offsets[tile_idx];
                let byte_count = info.tile_byte_counts[tile_idx] as usize;
                let mut compressed = read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
                apply_fill_order(&mut compressed, info.fill_order, info.compression);
                let mut tile_data = decompress(
                    &compressed,
                    info.compression,
                    tile_data_bytes,
                    info.predictor,
                    info.samples_per_pixel,
                    info.bits_per_sample,
                    info.tile_width,
                    info.tile_height,
                    file.parser.little_endian,
                    info.jpeg_tables.as_deref(),
                    info.nikon_compression_options.as_ref(),
                )?;
                tile_data.resize(tile_data_bytes, 0);

                // Decode the tile's YCbCr blocks to planar RGB at tile resolution.
                let rgb = decode_ycbcr_chunky(
                    &tile_data,
                    info.tile_width,
                    info.tile_height,
                    info.ycbcr_subsampling,
                    info.ycbcr_coefficients,
                )?;
                let tile_plane = (info.tile_width * info.tile_height) as usize;
                let r_plane = &rgb[0..tile_plane];
                let g_plane = &rgb[tile_plane..2 * tile_plane];
                let b_plane = &rgb[2 * tile_plane..3 * tile_plane];

                let tile_x0 = tx * info.tile_width;
                let tile_y0 = ty * info.tile_height;
                let src_x = x.saturating_sub(tile_x0) as usize;
                let src_y = y.saturating_sub(tile_y0) as usize;
                let dst_x = tile_x0.saturating_sub(x) as usize;
                let dst_y = tile_y0.saturating_sub(y) as usize;
                let copy_w = ((info.tile_width - src_x as u32).min(w - dst_x as u32)) as usize;
                let copy_h = ((info.tile_height - src_y as u32).min(h - dst_y as u32)) as usize;
                let tw = info.tile_width as usize;
                let ow = w as usize;

                for row in 0..copy_h {
                    let src_off = (src_y + row) * tw + src_x;
                    let dst_off = (dst_y + row) * ow + dst_x;
                    r_out[dst_off..dst_off + copy_w]
                        .copy_from_slice(&r_plane[src_off..src_off + copy_w]);
                    g_out[dst_off..dst_off + copy_w]
                        .copy_from_slice(&g_plane[src_off..src_off + copy_w]);
                    b_out[dst_off..dst_off + copy_w]
                        .copy_from_slice(&b_plane[src_off..src_off + copy_w]);
                }
            }
        }

        let mut out = Vec::with_capacity(plane_size * 3);
        out.extend_from_slice(&r_out);
        out.extend_from_slice(&g_out);
        out.extend_from_slice(&b_out);
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
        let subbyte = info.bits_per_sample < 8;
        // For sub-byte planar samples, each tile stores packed bits (one sample per
        // pixel) with byte-aligned rows; we unpack to one byte per sample below.
        let tile_data_bytes = if subbyte {
            packed_row_bytes(info.tile_width, 1, info.bits_per_sample) * info.tile_height as usize
        } else {
            (info.tile_width * bytes_per_sample) as usize * info.tile_height as usize
        };
        let tile_row_bytes = (info.tile_width * bytes_per_sample) as usize;
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
                    let mut compressed = read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
                    apply_fill_order(&mut compressed, info.fill_order, info.compression);
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
                        info.nikon_compression_options.as_ref(),
                    )?;
                    tile_data.resize(tile_data_bytes, 0);
                    if subbyte {
                        // Unpack the packed bits of this tile to one byte per sample.
                        tile_data = unpack_subbyte_samples(
                            &tile_data,
                            info.tile_width,
                            info.tile_height,
                            1,
                            info.bits_per_sample,
                        );
                    }
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

/// Reverse the bit order within each byte if `FillOrder == 2` for a compression
/// scheme that Java's `TiffParser` bit-reverses (CCITT/fax and Deflate streams).
/// Mirrors `tile[i] = (byte) (Integer.reverse(tile[i]) >> 24)`.
fn apply_fill_order(data: &mut [u8], fill_order: u16, compression: Compression) {
    if fill_order == 2 && compression.reverses_bits_on_fill_order_2() {
        for b in data.iter_mut() {
            *b = b.reverse_bits();
        }
    }
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

/// Unpack packed samples whose bit depth is not a multiple of 8 (e.g. 12-bit),
/// reading MSB-first within each byte-aligned row, and emitting each sample as
/// `ceil(bits_per_sample/8)` little/big-endian bytes. Mirrors the `noDiv8` path
/// of Java's TiffParser.unpackBytes, including the per-row skipBits padding (rows
/// are byte-aligned because each row occupies `packed_row_bytes` bytes).
fn unpack_packed_samples(
    data: &[u8],
    width: u32,
    height: u32,
    samples_per_pixel: u32,
    bits_per_sample: u16,
    little_endian: bool,
) -> Vec<u8> {
    let row_samples = width as usize * samples_per_pixel as usize;
    let row_bytes = packed_row_bytes(width, samples_per_pixel, bits_per_sample);
    let bytes_per_sample = ((bits_per_sample as usize) + 7) / 8;
    let bps = bits_per_sample as usize;
    let mut out = vec![0u8; row_samples * bytes_per_sample * height as usize];

    for row in 0..height as usize {
        let row_start = row * row_bytes;
        let row_data = data.get(row_start..row_start + row_bytes).unwrap_or(&[]);
        for sample in 0..row_samples {
            let mut value: u64 = 0;
            let bit_base = sample * bps;
            for k in 0..bps {
                let bit = bit_base + k;
                let byte = row_data.get(bit / 8).copied().unwrap_or(0);
                let bit_val = (byte >> (7 - (bit % 8))) & 1;
                value = (value << 1) | bit_val as u64;
            }
            let out_base = (row * row_samples + sample) * bytes_per_sample;
            for b in 0..bytes_per_sample {
                let shift = if little_endian {
                    8 * b
                } else {
                    8 * (bytes_per_sample - 1 - b)
                };
                out[out_base + b] = ((value >> shift) & 0xff) as u8;
            }
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
        self.add_nikon_raw_sub_ifd_series()?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::reader::FormatReader;
    use std::fs;

    fn push_u16_le(data: &mut Vec<u8>, value: u16) {
        data.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32_le(data: &mut Vec<u8>, value: u32) {
        data.extend_from_slice(&value.to_le_bytes());
    }

    fn push_ifd_short(data: &mut Vec<u8>, tag: u16, value: u16) {
        push_u16_le(data, tag);
        push_u16_le(data, 3);
        push_u32_le(data, 1);
        push_u16_le(data, value);
        push_u16_le(data, 0);
    }

    fn push_ifd_long(data: &mut Vec<u8>, tag: u16, value: u32) {
        push_u16_le(data, tag);
        push_u16_le(data, 4);
        push_u32_le(data, 1);
        push_u32_le(data, value);
    }

    fn push_ifd_undefined(data: &mut Vec<u8>, tag: u16, value: &[u8], offset: u32) {
        push_u16_le(data, tag);
        push_u16_le(data, 7);
        push_u32_le(data, value.len() as u32);
        push_u32_le(data, offset);
    }

    fn classic_tiff_with_one_undefined_ifd(tag: u16, value: &[u8]) -> Vec<u8> {
        let value_offset = 26u32;
        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        push_u16_le(&mut data, 42);
        push_u32_le(&mut data, 8);
        push_u16_le(&mut data, 1);
        push_ifd_undefined(&mut data, tag, value, value_offset);
        push_u32_le(&mut data, 0);
        data.extend_from_slice(value);
        data
    }

    fn synthetic_nikon_maker_note() -> Vec<u8> {
        let mut tag_150 = vec![0x46, 0x00];
        for predictor in [11, 22, 33, 44] {
            push_u16_le(&mut tag_150, predictor);
        }
        push_u16_le(&mut tag_150, 0);

        let nested = classic_tiff_with_one_undefined_ifd(
            super::super::nikon::MAKER_NOTE_COMPRESSION_TAG,
            &tag_150,
        );
        let mut maker_note = b"Nikon\0\x02\0\0\0".to_vec();
        maker_note.extend_from_slice(&nested);
        maker_note
    }

    fn synthetic_nikon_compressed_tiff() -> Vec<u8> {
        let maker_note = synthetic_nikon_maker_note();
        let main_ifd_offset = 8u32;
        let main_entry_count = 10u16;
        let main_ifd_bytes = 2 + main_entry_count as u32 * 12 + 4;
        let exif_ifd_offset = main_ifd_offset + main_ifd_bytes;
        let exif_ifd_bytes = 2 + 12 + 4;
        let maker_note_offset = exif_ifd_offset + exif_ifd_bytes;
        let compressed = [1u8, 2, 3];
        let strip_offset = maker_note_offset + maker_note.len() as u32;

        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        push_u16_le(&mut data, 42);
        push_u32_le(&mut data, main_ifd_offset);

        push_u16_le(&mut data, main_entry_count);
        push_ifd_long(&mut data, tag::IMAGE_WIDTH, 17);
        push_ifd_long(&mut data, tag::IMAGE_LENGTH, 23);
        push_ifd_short(&mut data, tag::BITS_PER_SAMPLE, 12);
        push_ifd_short(&mut data, tag::COMPRESSION, 34713);
        push_ifd_short(&mut data, tag::PHOTOMETRIC_INTERPRETATION, 1);
        push_ifd_long(&mut data, tag::STRIP_OFFSETS, strip_offset);
        push_ifd_short(&mut data, tag::SAMPLES_PER_PIXEL, 1);
        push_ifd_long(&mut data, tag::ROWS_PER_STRIP, 23);
        push_ifd_long(&mut data, tag::STRIP_BYTE_COUNTS, compressed.len() as u32);
        push_ifd_long(
            &mut data,
            super::super::nikon::EXIF_IFD_TAG,
            exif_ifd_offset,
        );
        push_u32_le(&mut data, 0);

        push_u16_le(&mut data, 1);
        push_ifd_undefined(
            &mut data,
            super::super::nikon::EXIF_MAKER_NOTE_TAG,
            &maker_note,
            maker_note_offset,
        );
        push_u32_le(&mut data, 0);

        data.extend_from_slice(&maker_note);
        data.extend_from_slice(&compressed);
        data
    }

    fn synthetic_nikon_compressed_sub_ifd_tiff() -> Vec<u8> {
        let maker_note = synthetic_nikon_maker_note();
        let main_ifd_offset = 8u32;
        let main_entry_count = 11u16;
        let main_ifd_bytes = 2 + main_entry_count as u32 * 12 + 4;
        let exif_ifd_offset = main_ifd_offset + main_ifd_bytes;
        let exif_ifd_bytes = 2 + 12 + 4;
        let maker_note_offset = exif_ifd_offset + exif_ifd_bytes;
        let sub_ifd_offset = maker_note_offset + maker_note.len() as u32;
        let sub_entry_count = 9u16;
        let sub_ifd_bytes = 2 + sub_entry_count as u32 * 12 + 4;
        let compressed = [1u8, 2, 3];
        let compressed_offset = sub_ifd_offset + sub_ifd_bytes;
        let preview = [9u8, 8, 7, 6];
        let preview_offset = compressed_offset + compressed.len() as u32;

        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        push_u16_le(&mut data, 42);
        push_u32_le(&mut data, main_ifd_offset);

        push_u16_le(&mut data, main_entry_count);
        push_ifd_long(&mut data, tag::IMAGE_WIDTH, 2);
        push_ifd_long(&mut data, tag::IMAGE_LENGTH, 2);
        push_ifd_short(&mut data, tag::BITS_PER_SAMPLE, 8);
        push_ifd_short(&mut data, tag::COMPRESSION, 1);
        push_ifd_short(&mut data, tag::PHOTOMETRIC_INTERPRETATION, 1);
        push_ifd_long(&mut data, tag::STRIP_OFFSETS, preview_offset);
        push_ifd_short(&mut data, tag::SAMPLES_PER_PIXEL, 1);
        push_ifd_long(&mut data, tag::ROWS_PER_STRIP, 2);
        push_ifd_long(&mut data, tag::STRIP_BYTE_COUNTS, preview.len() as u32);
        push_ifd_long(
            &mut data,
            super::super::nikon::EXIF_IFD_TAG,
            exif_ifd_offset,
        );
        push_ifd_long(&mut data, tag::SUB_IFD, sub_ifd_offset);
        push_u32_le(&mut data, 0);

        push_u16_le(&mut data, 1);
        push_ifd_undefined(
            &mut data,
            super::super::nikon::EXIF_MAKER_NOTE_TAG,
            &maker_note,
            maker_note_offset,
        );
        push_u32_le(&mut data, 0);

        data.extend_from_slice(&maker_note);

        push_u16_le(&mut data, sub_entry_count);
        push_ifd_long(&mut data, tag::IMAGE_WIDTH, 17);
        push_ifd_long(&mut data, tag::IMAGE_LENGTH, 23);
        push_ifd_short(&mut data, tag::BITS_PER_SAMPLE, 12);
        push_ifd_short(&mut data, tag::COMPRESSION, 34713);
        push_ifd_short(&mut data, tag::PHOTOMETRIC_INTERPRETATION, 1);
        push_ifd_long(&mut data, tag::STRIP_OFFSETS, compressed_offset);
        push_ifd_short(&mut data, tag::SAMPLES_PER_PIXEL, 1);
        push_ifd_long(&mut data, tag::ROWS_PER_STRIP, 23);
        push_ifd_long(&mut data, tag::STRIP_BYTE_COUNTS, compressed.len() as u32);
        push_u32_le(&mut data, 0);

        data.extend_from_slice(&compressed);
        data.extend_from_slice(&preview);
        data
    }

    #[test]
    fn nikon_34713_reader_routes_parsed_maker_note_options_to_decoder() {
        let path = std::env::temp_dir().join(format!(
            "bioformats-rs-nikon-34713-route-{}.tif",
            std::process::id()
        ));
        fs::write(&path, synthetic_nikon_compressed_tiff()).unwrap();

        let mut reader = TiffReader::new();
        reader.set_id(&path).unwrap();
        let bytes = reader
            .open_bytes(0)
            .expect("Nikon decoder should mirror Bio-Formats EOF bit padding");

        let _ = fs::remove_file(&path);

        assert!(!bytes.is_empty());
    }

    #[test]
    fn nikon_34713_sub_ifd_is_exposed_as_raw_series_before_decoder() {
        let path = std::env::temp_dir().join(format!(
            "bioformats-rs-nikon-34713-sub-ifd-route-{}.tif",
            std::process::id()
        ));
        fs::write(&path, synthetic_nikon_compressed_sub_ifd_tiff()).unwrap();

        let mut reader = TiffReader::new();
        reader.set_id(&path).unwrap();

        let mut raw_series = None;
        for series in 0..reader.series_count() {
            reader.set_series(series).unwrap();
            let meta = reader.metadata();
            if meta.size_x == 17 && meta.size_y == 23 && meta.bits_per_pixel == 12 {
                raw_series = Some(series);
                break;
            }
        }
        let series = raw_series.expect("Nikon compression 34713 SubIFD should be a RAW series");
        reader.set_series(series).unwrap();
        let bytes = reader
            .open_bytes(0)
            .expect("Nikon decoder should mirror Bio-Formats EOF bit padding");

        let _ = fs::remove_file(&path);

        assert!(!bytes.is_empty());
    }
}
