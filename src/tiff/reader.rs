use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::Path;

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::read_bytes_at;

use super::ifd::{tag, Ifd, Photometric};
use super::parser::TiffParser;
use super::compression::decompress;

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
        let width = ifd.image_width().ok_or_else(|| {
            BioFormatsError::Format("IFD missing ImageWidth".into())
        })?;
        let height = ifd.image_length().ok_or_else(|| {
            BioFormatsError::Format("IFD missing ImageLength".into())
        })?;

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
            ifd.get_u32(tag::ROWS_PER_STRIP).unwrap_or(height)
        };

        let strip_offsets = ifd.get_vec_u64(tag::STRIP_OFFSETS);
        let strip_byte_counts = ifd.get_vec_u64(tag::STRIP_BYTE_COUNTS);
        let tile_offsets = ifd.get_vec_u64(tag::TILE_OFFSETS);
        let tile_byte_counts = ifd.get_vec_u64(tag::TILE_BYTE_COUNTS);

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
            .filter_map(|(i, ifd)| Self::ifd_info(ifd, little_endian).ok().map(|info| (i, info)))
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

                let is_rgb = matches!(
                    info.photometric,
                    Photometric::Rgb | Photometric::YCbCr
                ) && info.samples_per_pixel >= 3;
                let is_indexed = info.photometric == Photometric::Palette;

                let lookup_table = info.color_map.as_ref().map(|(r, g, b)| {
                    crate::common::metadata::LookupTable {
                        red: r.clone(),
                        green: g.clone(),
                        blue: b.clone(),
                    }
                });

                let image_count = ifd_indices.len() as u32;
                let mut meta = ImageMetadata {
                    size_x: info.width,
                    size_y: info.height,
                    size_z: image_count,
                    size_c: if is_rgb { 3 } else { 1 },
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
                };

                // Store image description in metadata
                if let Some(desc) = &info.image_description {
                    meta.series_metadata.insert(
                        "ImageDescription".into(),
                        crate::common::metadata::MetadataValue::String(desc.clone()),
                    );
                }

                TiffSeries { ifd_indices, metadata: meta, sub_resolutions: Vec::new() }
            })
            .collect()
    }

    /// Parse SubIFD chains for pyramid support.
    /// For each series, check the first IFD for a SUB_IFD tag (330).
    /// If present, read sub-IFDs and store them as sub-resolution levels.
    fn parse_sub_ifds(&mut self) -> Result<()> {
        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;

        for series in &mut self.series {
            if series.ifd_indices.is_empty() {
                continue;
            }
            // Check the first IFD of this series for SubIFD offsets
            let first_ifd_idx = series.ifd_indices[0];
            let sub_ifd_offsets: Vec<u64> = {
                let ifd = &file.ifds[first_ifd_idx];
                let offsets = ifd.get_vec_u64(tag::SUB_IFD);
                if offsets.is_empty() {
                    continue;
                }
                offsets
            };

            // Read sub-IFDs and add them to the ifds array
            let mut sub_res_levels: Vec<Vec<usize>> = Vec::new();
            for offset in &sub_ifd_offsets {
                let (sub_ifd, _next) = file.parser.read_ifd(*offset)?;
                // Verify the sub-IFD is a valid image (has width/height)
                if sub_ifd.image_width().is_none() {
                    continue;
                }
                let sub_idx = file.ifds.len();
                file.ifds.push(sub_ifd);
                sub_res_levels.push(vec![sub_idx]);
            }

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
            s.ifd_indices.get(plane_index as usize)
                .copied()
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))
        } else {
            // Sub-resolution level (1-based index into sub_resolutions)
            let level = self.current_resolution - 1;
            let sub = s.sub_resolutions.get(level).ok_or_else(|| {
                BioFormatsError::Format(format!("resolution level {} out of range", self.current_resolution))
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
        let ifd = file.ifds.get(ifd_index).ok_or_else(|| {
            BioFormatsError::PlaneOutOfRange(ifd_index as u32)
        })?;
        let little_endian = file.parser.little_endian;
        let info = Self::ifd_info(ifd, little_endian)?;

        let bytes_per_sample = (info.bits_per_sample as u32 + 7) / 8;
        let effective_spp = if info.planar_config == 2 { 1 } else { info.samples_per_pixel as u32 };
        let plane_byte_len = (w * h * effective_spp * bytes_per_sample) as usize;

        if info.is_tiled {
            self.read_tiled_plane(&info, x, y, w, h, plane_byte_len)
        } else {
            self.read_stripped_plane(&info, x, y, w, h, plane_byte_len)
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
        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let bytes_per_sample = (info.bits_per_sample as u32 + 7) / 8;
        let effective_spp = if info.planar_config == 2 {
            1u32
        } else {
            info.samples_per_pixel as u32
        };
        let row_bytes = info.width * effective_spp * bytes_per_sample;

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
            let expected = (strip_rows * row_bytes) as usize;

            let mut strip_data = decompress(
                &compressed,
                info.compression,
                expected,
                info.predictor,
                info.samples_per_pixel,
                info.bits_per_sample,
                info.jpeg_tables.as_deref(),
            )?;
            strip_data.truncate(expected);

            // Crop rows within this strip to the requested y range
            let row_start = y.saturating_sub(strip_start_row) as usize;
            let row_end = (y + h - strip_start_row).min(strip_rows) as usize;

            for row in row_start..row_end {
                let rs = row * row_bytes as usize;
                let re = rs + row_bytes as usize;
                if re <= strip_data.len() {
                    plane_rows.extend_from_slice(&strip_data[rs..re]);
                }
            }
        }

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

    fn read_tiled_plane(
        &mut self,
        info: &IfdInfo,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
        _plane_byte_len: usize,
    ) -> Result<Vec<u8>> {
        let file = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;
        let bytes_per_sample = (info.bits_per_sample as u32 + 7) / 8;
        let effective_spp = if info.planar_config == 2 {
            1u32
        } else {
            info.samples_per_pixel as u32
        };
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
                    info.samples_per_pixel,
                    info.bits_per_sample,
                    info.jpeg_tables.as_deref(),
                )?;
                tile_data.resize(tile_data_bytes, 0);

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
                    if src_off + copy_bytes <= tile_data.len()
                        && dst_off + copy_bytes <= out.len()
                    {
                        out[dst_off..dst_off + copy_bytes]
                            .copy_from_slice(&tile_data[src_off..src_off + copy_bytes]);
                    }
                }
            }
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

// ---- FormatReader impl ----

impl crate::common::reader::FormatReader for TiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(
            ext.as_deref(),
            Some("tif") | Some("tiff") | Some("ome.tif") | Some("ome.tiff") | Some("btf") | Some("tf8")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 4 {
            return false;
        }
        // II 42 00 or MM 00 42 — classic TIFF
        // II 43 00 or MM 00 43 — BigTIFF
        (header[0..2] == [0x49, 0x49] || header[0..2] == [0x4D, 0x4D])
            && (header[2..4] == [42, 0] || header[2..4] == [0, 42]
                || header[2..4] == [43, 0] || header[2..4] == [0, 43])
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
        self.series = Self::build_series(&tf.ifds, little_endian);
        // Detect OME-TIFF: OME-XML is stored in the first IFD's ImageDescription.
        self.ome_xml = self.series.first()
            .and_then(|s| s.metadata.series_metadata.get("ImageDescription"))
            .and_then(|v| if let crate::common::metadata::MetadataValue::String(s) = v {
                Some(s.as_str())
            } else {
                None
            })
            .filter(|desc| {
                let t = desc.trim_start();
                t.starts_with("<?xml") || t.starts_with("<OME") || t.contains("<OME ")
            })
            .map(str::to_owned);
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
                "resolution level {} out of range (max {})", level, max - 1
            )));
        }
        self.current_resolution = level;
        Ok(())
    }

    fn resolution(&self) -> usize {
        self.current_resolution
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        self.ome_xml.as_deref().map(crate::common::ome_metadata::OmeMetadata::from_ome_xml)
    }
}
