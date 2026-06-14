use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::read_bytes_at;
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::path::confined_join;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader as _;

use super::compression::{decompress, merge_jpeg_tables, JpegColor};
use super::ifd::{tag, Compression, Ifd, Photometric};
use super::jpeg_restart;
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
    /// Whether the RowsPerStrip tag was actually present in the IFD. When absent,
    /// `rows_per_strip` is synthesized as `height`. Java's getStripByteCounts
    /// (IFD.java:870-877) doubles LZW strip byte counts when the tag is absent, so
    /// the raw presence must be tracked separately from the synthesized value.
    rows_per_strip_present: bool,
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
    /// Hamamatsu NDPI restart-marker offsets (relative to the strip start) for a
    /// single-strip JPEG level. Built from tag 65426 (low 32 bits) + 65432 (high
    /// 32 bits). When present, a giant single-strip JPEG (whole-slide level 0,
    /// stored as one >4 GB strip) can be windowed without scanning the strip:
    /// `markers[k]` is the byte offset just after the k-th `RSTn` marker, and
    /// `markers[0]` is the scan start. See `jpeg_restart::decode_rows_ndpi`.
    ndpi_restart_markers: Option<Vec<u64>>,
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
    /// SamplesPerPixel from the first `<Channel>`, used by Java
    /// OMETiffReader.initFile:1212 to compute `rgb`.
    samples_per_pixel: u32,
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
    /// FileName from a nested `<UUID FileName="...">`, if any. When present the
    /// plane's pixels live in a companion TIFF rather than the current file.
    filename: Option<String>,
}

/// A logical plane whose pixels live in a companion TIFF file, mirroring the
/// `planes[no].id` / `planes[no].reader` mapping in Java's `OMETiffReader`.
#[derive(Debug, Clone)]
struct ExternalPlane {
    /// Absolute path to the companion TIFF.
    path: PathBuf,
    /// IFD index within that companion TIFF.
    ifd: usize,
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
    /// For multi-file OME-TIFF: logical plane -> companion-file pixel source.
    /// Empty when all planes live in the current file. A `Some(_)` entry means
    /// the matching plane's pixels must be read from another file; `None` at a
    /// populated index falls back to `plane_ifd_indices`/`ifd_indices`.
    external_planes: Vec<Option<ExternalPlane>>,
}

pub struct TiffReader {
    file: Option<TiffFile>,
    series: Vec<TiffSeries>,
    current_series: usize,
    current_resolution: usize,
    current_resolution_metadata: Option<ImageMetadata>,
    /// OME-XML embedded in the first IFD's ImageDescription, if present.
    ome_xml: Option<String>,
    /// Path of the file passed to `set_id`, used to resolve OME-TIFF companion
    /// files referenced via `<UUID FileName="...">` relative to its directory.
    path: Option<PathBuf>,
    /// Cache of opened companion TIFF readers, keyed by absolute path, so that
    /// reading many external planes from the same companion reuses one reader.
    companion_readers: HashMap<PathBuf, TiffReader>,
    /// When set, parse the IFD chain using the Hamamatsu NDPI 64-bit ("fake
    /// BigTIFF") layout instead of standard 32-bit pointers. Enabled by
    /// `NdpiReader` for files >4 GB before `set_id`.
    ndpi_64bit: bool,
}

impl TiffReader {
    pub fn new() -> Self {
        TiffReader {
            file: None,
            series: Vec::new(),
            current_series: 0,
            current_resolution: 0,
            current_resolution_metadata: None,
            ome_xml: None,
            path: None,
            companion_readers: HashMap::new(),
            ndpi_64bit: false,
        }
    }

    /// Enable Hamamatsu NDPI 64-bit ("fake BigTIFF") IFD parsing. Must be called
    /// before `set_id`. See [`TiffParser::read_ifds_ndpi64`].
    pub fn set_ndpi_64bit(&mut self, on: bool) {
        self.ndpi_64bit = on;
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
        self.current_resolution_metadata = None;
    }

    /// Flatten each series' resolution pyramid into independent top-level series,
    /// mirroring Java's default `ImageReader` behaviour (flattenedResolutions =
    /// true): every (series, resolution) pair becomes its own series with
    /// `resolutionCount == 1`. The per-resolution `sizeX`/`sizeY`/`imageCount`
    /// are taken from each level's first IFD. Whole-slide vendor wrappers (SVS,
    /// SCN, NDPI) call this so their flattened series count and per-series
    /// dimensions match Java's reference output.
    pub fn flatten_resolutions_into_series(&mut self) -> Result<()> {
        let little_endian = self.is_little_endian();
        let old = std::mem::take(&mut self.series);
        let mut flat: Vec<TiffSeries> = Vec::with_capacity(old.len());
        for s in old {
            // Level 0 (main resolution): keep ifd_indices / plane map / externals.
            let mut base = TiffSeries {
                ifd_indices: s.ifd_indices.clone(),
                plane_ifd_indices: s.plane_ifd_indices.clone(),
                metadata: s.metadata.clone(),
                sub_resolutions: Vec::new(),
                external_planes: s.external_planes.clone(),
            };
            base.metadata.resolution_count = 1;
            flat.push(base);

            // Each sub-resolution level becomes its own series.
            for level in &s.sub_resolutions {
                let mut meta = s.metadata.clone();
                meta.resolution_count = 1;
                if let Some(&first_idx) = level.first() {
                    if let Some(file) = self.file.as_ref() {
                        if let Some(ifd) = file.ifds.get(first_idx) {
                            if let Ok(info) = Self::ifd_info(ifd, little_endian) {
                                meta.size_x = info.width;
                                meta.size_y = info.height;
                            }
                        }
                    }
                }
                // One plane per Z (sub-resolutions carry no separate C/T split).
                meta.image_count = level.len() as u32;
                meta.size_z = level.len() as u32;
                flat.push(TiffSeries {
                    ifd_indices: level.clone(),
                    plane_ifd_indices: Vec::new(),
                    metadata: meta,
                    sub_resolutions: Vec::new(),
                    external_planes: Vec::new(),
                });
            }
        }
        self.series = flat;
        self.current_series = 0;
        self.current_resolution = 0;
        self.current_resolution_metadata = None;
        Ok(())
    }

    /// Number of parsed IFDs in the open file.
    pub fn ifd_count(&self) -> usize {
        self.file.as_ref().map(|f| f.ifds.len()).unwrap_or(0)
    }

    /// Whether the file was parsed as little-endian.
    pub fn is_little_endian(&self) -> bool {
        self.file
            .as_ref()
            .map(|f| f.parser.little_endian)
            .unwrap_or(true)
    }

    /// Access a raw IFD by index (for extracting vendor-specific tags).
    pub fn ifd(&self, index: usize) -> Option<&Ifd> {
        self.file.as_ref().and_then(|f| f.ifds.get(index))
    }

    /// Mutable access to a parsed IFD. Used by vendor wrappers (e.g. NDPI) that
    /// must rewrite strip/tile offset arrays after parsing — for instance to
    /// apply NDPI's >4 GB high-word offset corrections before pixel reads.
    pub fn ifd_mut(&mut self, index: usize) -> Option<&mut Ifd> {
        self.file.as_mut().and_then(|f| f.ifds.get_mut(index))
    }

    /// Get the embedded OME-XML string, if any.
    pub fn ome_xml_str(&self) -> Option<&str> {
        self.ome_xml.as_deref()
    }

    /// Read the Canon DNG white-balance RGB coefficients from the EXIF
    /// maker-note, returning `Some([r, g, b])` when present.
    ///
    /// This is an additive helper for [`crate::formats::extended::DngReader`];
    /// it does not affect ordinary TIFF parsing. It ports the EXIF/maker-note
    /// traversal in `DNGReader.initStandardMetadata` (Java ~274-336):
    ///
    /// 1. follow the EXIF sub-IFD pointer (tag 34665) on the first main IFD,
    /// 2. read the `MAKER_NOTE` (tag 37500) bytes from that EXIF IFD,
    /// 3. apply Canon's offset rewrite — the last 4 bytes give an `offset`; the
    ///    trailing 8 bytes (a mini-TIFF header) move to position 0 and the body
    ///    is relocated to `offset` — then parse the resulting buffer as a TIFF,
    /// 4. read tag 16385 (`WHITE_BALANCE_RGB_COEFFS`) from the maker-note IFD.
    ///
    /// As in Java, a present-but-non-rational coefficient entry falls back to
    /// the hard-coded `{2.391381, 0.929156, 1.298254}` table; a fully absent
    /// maker-note / EXIF yields `None` (white balance is then a no-op).
    pub fn dng_white_balance(&mut self) -> Option<[f64; 3]> {
        const WHITE_BALANCE_RGB_COEFFS: u16 = 16385;
        const DEFAULT_WB: [f64; 3] = [2.391381, 0.929156, 1.298254];

        let file = self.file.as_mut()?;
        let little = file.parser.little_endian;

        // EXIF sub-IFD pointer lives on the first main IFD.
        let exif_offset = file.ifds.first()?.get_u64(super::nikon::EXIF_IFD_TAG)?;
        if exif_offset == 0 {
            return None;
        }
        let (exif_ifd, _) = file.parser.read_ifd(exif_offset).ok()?;

        // MAKER_NOTE bytes (Canon stores an offset-relative TIFF blob here).
        let maker = match exif_ifd.get(super::nikon::EXIF_MAKER_NOTE_TAG)? {
            super::ifd::IfdValue::Byte(b) | super::ifd::IfdValue::Undefined(b) => b.clone(),
            _ => return None,
        };
        let note = parse_canon_maker_note(&maker, little)?;

        let value = note.get(WHITE_BALANCE_RGB_COEFFS)?;
        if note.is_rational(WHITE_BALANCE_RGB_COEFFS) {
            let coeffs = value.as_vec_f64();
            if coeffs.len() >= 3 {
                return Some([coeffs[0], coeffs[1], coeffs[2]]);
            }
            // Java only treats a TiffRational[] as valid white balance; a short
            // rational array is not expected, so fall through to the default.
            return Some(DEFAULT_WB);
        }
        // Present but non-rational: Java uses the hard-coded default table.
        Some(DEFAULT_WB)
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

        let rows_per_strip_present = !is_tiled && ifd.get(tag::ROWS_PER_STRIP).is_some();
        let rows_per_strip = if is_tiled {
            0
        } else {
            if rows_per_strip_present {
                ifd.get_u32(tag::ROWS_PER_STRIP).unwrap_or(0)
            } else {
                height
            }
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

        // Hamamatsu NDPI restart-marker offsets (tag 65426 low, 65432 high). Used
        // to window a giant single-strip JPEG level without reading the whole
        // strip. Mirrors NDPIReader.getMarkers (NDPIReader.java:891-921).
        const NDPI_MARKER_TAG: u16 = 65426;
        const NDPI_MARKER_HIGH_BYTES: u16 = 65432;
        let ndpi_restart_markers = ifd.get(NDPI_MARKER_TAG).map(|v| {
            let low = v.as_vec_u64();
            match ifd.get(NDPI_MARKER_HIGH_BYTES).map(|h| h.as_vec_u64()) {
                Some(high) => low
                    .iter()
                    .enumerate()
                    .map(|(i, &lo)| (lo & 0xffff_ffff) + (high.get(i).copied().unwrap_or(0) << 32))
                    .collect::<Vec<u64>>(),
                None => {
                    // High words absent (can happen in sub-resolution IFDs): add
                    // 4 GB whenever the offset sequence decreases (overflow).
                    let mut out = low.clone();
                    for i in 1..out.len() {
                        if out[i] < out[i - 1] {
                            out[i] += 1u64 << 32;
                        }
                    }
                    out
                }
            }
        });

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
            rows_per_strip_present,
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
            ndpi_restart_markers,
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
                    is_interleaved: is_interleaved_rgb(info),
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

                // ImageJ-style TIFF comment parsing (Java TiffReader.parseCommentImageJ
                // + populateMetadataStoreImageJ). Java checks both the first and last
                // IFD's comment; mirror that here. Strictly metadata-only: this does
                // not alter dimensions, ordering, or pixel decoding.
                let comment = info
                    .image_description
                    .as_deref()
                    .filter(|c| check_comment_imagej(c))
                    .or_else(|| {
                        group
                            .last()
                            .and_then(|(_, last)| last.image_description.as_deref())
                            .filter(|c| check_comment_imagej(c))
                    });
                if let Some(comment) = comment {
                    parse_comment_imagej(comment, &mut meta.series_metadata);
                }

                TiffSeries {
                    ifd_indices,
                    plane_ifd_indices: Vec::new(),
                    metadata: meta,
                    sub_resolutions: Vec::new(),
                    external_planes: Vec::new(),
                }
            })
            .collect()
    }

    fn build_ome_series(
        ifds: &[Ifd],
        xml: &str,
        little_endian: bool,
        base_dir: Option<&Path>,
        current_path: Option<&Path>,
    ) -> Option<Vec<TiffSeries>> {
        let images = parse_ome_tiff_images(xml);
        if images.is_empty() {
            return None;
        }

        // Parse the structured OME metadata once so we can attach per-image
        // Modulo annotations. NOTE: the Modulo parser lives in
        // `common/ome_metadata.rs` (currently reads ModuloAlong{Z,C,T}; another
        // agent is extending it to also read StructuredAnnotations). We rely only
        // on its stable public surface: `OmeMetadata::from_ome_xml(xml)` and the
        // per-image `modulo_z/c/t` fields. If those move, update this call site.
        let ome_meta = crate::common::ome_metadata::OmeMetadata::from_ome_xml(xml);

        let mut series = Vec::new();
        for (image_idx, image) in images.into_iter().enumerate() {
            let image_count = image
                .size_z
                .saturating_mul(image.effective_c)
                .saturating_mul(image.size_t);
            if image_count == 0 {
                continue;
            }

            let companions = resolve_tiff_data_companions(&image, base_dir, current_path);
            let (mut plane_map, external_planes) =
                build_ome_plane_maps(&image, ifds.len(), &companions);
            // Fall back to sequential mapping only when neither a local IFD nor a
            // companion was resolved for any plane.
            if plane_map.iter().all(Option::is_none) && external_planes.iter().all(Option::is_none)
            {
                for (i, slot) in plane_map.iter_mut().enumerate() {
                    if i < ifds.len() {
                        *slot = Some(i);
                    }
                }
            }

            let ifd_indices: Vec<usize> = plane_map.iter().filter_map(|&idx| idx).collect();
            // Prefer a local IFD for sample-layout hints; if every plane lives in
            // a companion file (multi-file / binary-only OME-TIFF) read the first
            // companion's first IFD instead. Both may be absent for a degenerate
            // file, in which case we fall back to OME-XML attributes only.
            let first_info = ifd_indices
                .first()
                .and_then(|&idx| ifds.get(idx))
                .and_then(|ifd| Self::ifd_info(ifd, little_endian).ok())
                .or_else(|| {
                    ifds.first()
                        .and_then(|ifd| Self::ifd_info(ifd, little_endian).ok())
                })
                .or_else(|| {
                    external_planes
                        .iter()
                        .find_map(|p| p.as_ref())
                        .and_then(external_plane_ifd_info)
                });

            // Layout hints: from a resolved IFD if available, else inferred from
            // the OME-XML pixel type / channel layout.
            // Java OMETiffReader.initFile:1212 sets `m.rgb = samples > 1 ||
            // photo == RGB`, where `samples` is the OME-XML Channel SamplesPerPixel
            // and `photo` is the first IFD's photometric interpretation. This is
            // broader than the generic TIFF rule (spp >= 3 && photo in {RGB,YCbCr}).
            let (is_rgb, is_interleaved, is_indexed, lookup_table) = match &first_info {
                Some(info) => (
                    image.samples_per_pixel > 1 || info.photometric == Photometric::Rgb,
                    is_interleaved_rgb(info),
                    info.photometric == Photometric::Palette,
                    info.color_map
                        .as_ref()
                        .map(|(r, g, b)| crate::common::metadata::LookupTable {
                            red: r.clone(),
                            green: g.clone(),
                            blue: b.clone(),
                        }),
                ),
                None => (
                    image.samples_per_pixel > 1 || image.effective_c < image.size_c,
                    false,
                    false,
                    None,
                ),
            };

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
                is_rgb,
                is_interleaved,
                is_indexed,
                is_little_endian: little_endian,
                resolution_count: 1,
                series_metadata: HashMap::new(),
                lookup_table,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            };
            // Propagate Modulo annotations parsed from the OME-XML (Java
            // OMETiffReader.initFile sets m.moduloZ/C/T per image).
            if let Some(ome_img) = ome_meta.images.get(image_idx) {
                meta.modulo_z = ome_img.modulo_z.clone();
                meta.modulo_c = ome_img.modulo_c.clone();
                meta.modulo_t = ome_img.modulo_t.clone();
            }
            meta.series_metadata.insert(
                "ImageDescription".into(),
                crate::common::metadata::MetadataValue::String(xml.to_string()),
            );

            // Only keep the external-plane vector if it actually references a
            // companion file; otherwise leave it empty (pure single-file case).
            let external_planes = if external_planes.iter().any(Option::is_some) {
                external_planes
            } else {
                Vec::new()
            };

            series.push(TiffSeries {
                ifd_indices,
                plane_ifd_indices: plane_map,
                metadata: meta,
                sub_resolutions: Vec::new(),
                external_planes,
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

        // Drop a "thumbnail" smallest resolution: SVSReader.removeThumbnail()
        // (SVSReader.java:612-630) removes the last pyramid level when it is
        // stored with strips (StripByteCounts present) rather than tiles, per
        // https://github.com/ome/bioformats/issues/3757. Only applies when more
        // than one resolution remains.
        if kept_levels.len() > 1 {
            if let Some(&last_idx) = kept_levels.last() {
                let last_ifd = &file.ifds[last_idx];
                let stripped = last_ifd.get(tag::STRIP_BYTE_COUNTS).is_some()
                    && last_ifd.get(tag::TILE_BYTE_COUNTS).is_none();
                if stripped {
                    kept_levels.pop();
                }
            }
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

        // Java SVSReader reports RGB whole-slide planes as channel-separated
        // (interleaved=false) with dimension order XYCZT (SVSReader.java:516-538).
        for s in &mut new_series {
            s.metadata.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;
            s.metadata.is_interleaved = false;
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
            is_interleaved: is_interleaved_rgb(info),
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
            external_planes: Vec::new(),
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

    /// Return the companion-file pixel source for `plane_index`, if the current
    /// series at the main resolution maps that plane to an external TIFF (multi-
    /// file / binary-only OME-TIFF). Sub-resolutions never have external planes.
    fn external_plane_for(&self, plane_index: u32) -> Option<ExternalPlane> {
        if self.current_resolution != 0 {
            return None;
        }
        self.series
            .get(self.current_series)?
            .external_planes
            .get(plane_index as usize)
            .and_then(|p| p.clone())
    }

    /// Read one plane region from a companion TIFF, opening (and caching) a
    /// `TiffReader` for that file. Mirrors Java OMETiffReader delegating to the
    /// per-plane `reader` for files other than the current one. The companion is
    /// a plain (non-OME) TIFF whose IFDs map sequentially onto series planes.
    fn read_external_plane(
        &mut self,
        plane: &ExternalPlane,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if !self.companion_readers.contains_key(&plane.path) {
            let mut reader = TiffReader::new();
            reader.set_id(&plane.path)?;
            self.companion_readers.insert(plane.path.clone(), reader);
        }
        let reader = self
            .companion_readers
            .get_mut(&plane.path)
            .expect("companion reader just inserted");

        // The OME-XML `<TiffData IFD=N>` references a PHYSICAL IFD in the
        // companion file, not a logical plane. Read that IFD directly. Going
        // through the companion's own series/logical-plane mapping is wrong when
        // the companion is itself a multi-file OME-TIFF: e.g. for the tubhiswt
        // pair, C1's own OME-XML maps its logical plane 0 back to C0 (external),
        // so `open_bytes(0)` on C1 would return C0's pixels. The physical IFD 0
        // of C1 is the C1 data we want. (Java's OMETiffReader likewise indexes
        // the companion by IFD.)
        reader.read_physical_ifd_region(plane.ifd, x, y, w, h)
    }

    /// Read a region from a PHYSICAL IFD index, bypassing all series / logical-
    /// plane / OME companion mapping. `(x, y, w, h)` clamp to the IFD's bounds;
    /// passing the full IFD size reads the whole plane. Used for companion-file
    /// reads where the OME-XML references a raw IFD number.
    pub fn read_physical_ifd_region(
        &mut self,
        ifd_index: usize,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.read_plane_bytes(ifd_index, x, y, w, h)
    }

    fn resolution_metadata(&self, level: usize) -> Result<Option<ImageMetadata>> {
        if level == 0 {
            return Ok(None);
        }

        let file = self.file.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let series = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::SeriesOutOfRange(self.current_series))?;
        let sub_ifds = series.sub_resolutions.get(level - 1).ok_or_else(|| {
            BioFormatsError::Format(format!("resolution level {level} out of range"))
        })?;
        let first_ifd = sub_ifds
            .first()
            .and_then(|&idx| file.ifds.get(idx))
            .ok_or_else(|| {
                BioFormatsError::Format(format!("resolution level {level} has no planes"))
            })?;

        let mut meta = series.metadata.clone();
        meta.size_x = first_ifd.image_width().unwrap_or(meta.size_x);
        meta.size_y = first_ifd.image_length().unwrap_or(meta.size_y);
        meta.image_count = sub_ifds.len() as u32;
        Ok(Some(meta))
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
        // JPEG-compressed YCbCr decodes to RGB inside the JPEG codec, so it takes
        // the generic chunky path below (ycbcr = false). Only the manual TIFF
        // YCbCr conversion path has the 8-bit/chunky restriction.
        let jpeg_ycbcr = ycbcr_decoded_by_jpeg(info);
        if info.photometric == Photometric::YCbCr && !jpeg_ycbcr && is_unsupported_ycbcr(info) {
            return Err(BioFormatsError::UnsupportedFormat(
                "Only 8-bit chunky non-JPEG TIFF YCbCr is supported".into(),
            ));
        }

        let bytes_per_sample = (info.bits_per_sample as u32 + 7) / 8;
        let effective_spp = info.samples_per_pixel as u32;
        let packed_row_layout = info.bits_per_sample % 8 != 0;
        let subbyte_samples = info.bits_per_sample < 8;
        let ycbcr = info.photometric == Photometric::YCbCr && !jpeg_ycbcr;
        let row_bytes = if ycbcr {
            checked_ycbcr_strip_bytes(info.width, 1, info.ycbcr_subsampling)?
        } else if packed_row_layout {
            checked_packed_row_bytes(info.width, effective_spp, info.bits_per_sample)?
        } else {
            checked_row_bytes(info.width, effective_spp, bytes_per_sample)?
        };

        let rows_per_strip = if info.rows_per_strip == 0 || info.rows_per_strip >= info.height {
            info.height
        } else {
            info.rows_per_strip
        };

        // Java IFD.getStripByteCounts (IFD.java:870-877) doubles the stored LZW
        // strip byte counts when RowsPerStrip is absent OR imageLength is not an
        // exact multiple of RowsPerStrip[0] (the clamped value). The doubled count
        // is the number of compressed bytes read per strip; decompression still
        // stops at `expected` output bytes. Gated strictly on LZW so the common
        // path is untouched.
        let lzw_double_byte_counts = info.compression == Compression::Lzw
            && (!info.rows_per_strip_present
                || rows_per_strip == 0
                || info.height % rows_per_strip != 0);

        // We assemble the full plane row-by-row, then crop to [x, y, w, h].
        let requested_rows_bytes = checked_mul_usize(h as usize, row_bytes, "TIFF crop row bytes")?;
        let mut plane_rows: Vec<u8> =
            checked_vec_with_capacity(requested_rows_bytes, "TIFF crop rows")?;
        // For subsampled YCbCr, RGB is accumulated per-plane (R then G then B) as
        // strips are decoded, so partial-row reads (a strip overlapping only part of
        // the requested y range) work like Java's per-block unpacking.
        let mut ycbcr_r: Vec<u8> = Vec::new();
        let mut ycbcr_g: Vec<u8> = Vec::new();
        let mut ycbcr_b: Vec<u8> = Vec::new();
        let y_end = y
            .checked_add(h)
            .ok_or_else(|| BioFormatsError::Format("TIFF region y range overflows".into()))?;

        // Windowed single-strip JPEG fast path (NDPI / whole-slide). When a level
        // is stored as ONE baseline JPEG strip covering the whole image, decode
        // only the band of MCU rows overlapping [y, y_end) via restart markers
        // instead of materialising the entire plane. Falls back to the generic
        // strip loop below when the JPEG lacks restart markers / is unaligned.
        let mut used_window = false;

        // NDPI marker-driven windowing: a Hamamatsu level is one JPEG strip that
        // can exceed 4 GB (full resolution). Its restart-marker offsets live in a
        // dedicated TIFF tag, so we can window WITHOUT reading the whole strip —
        // only the JPEG header and the requested band's intervals are read from
        // disk. This is the only path that keeps the >4 GB level bounded.
        if !used_window
            && matches!(info.compression, Compression::Jpeg | Compression::JpegNew)
            && !ycbcr
            && info.strip_offsets.len() == 1
            && info.strip_byte_counts.len() == 1
            && info.rows_per_strip >= info.height
        {
            if let Some(markers) = info.ndpi_restart_markers.as_deref() {
                let decoded = jpeg_restart::decode_rows_ndpi(
                    &mut file.parser.reader,
                    info.strip_offsets[0],
                    info.strip_byte_counts[0],
                    markers,
                    info.jpeg_tables.as_deref(),
                    info.width,
                    info.height,
                    x,
                    y,
                    w,
                    h,
                    jpeg_color_for(info),
                );
                if let Some(result) = decoded {
                    let band = result?;
                    let channels = band
                        .pixels
                        .len()
                        .checked_div(band.band_width as usize * band.band_height.max(1) as usize)
                        .filter(|&c| c > 0)
                        .unwrap_or(effective_spp as usize);
                    if info.bits_per_sample == 8
                        && !packed_row_layout
                        && !subbyte_samples
                        && !matches!(
                            info.photometric,
                            Photometric::MinIsWhite | Photometric::Cmyk
                        )
                        && x >= band.band_x0
                        && x.checked_add(w)
                            .is_some_and(|end| end <= band.band_x0 + band.band_width)
                        && y >= band.band_y0
                        && y_end <= band.band_y0 + band.band_height
                    {
                        let src_x = (x - band.band_x0) as usize;
                        let src_y = (y - band.band_y0) as usize;
                        let band_stride = band.band_width as usize * channels;
                        let row_len = w as usize * channels;
                        let mut out = checked_vec_with_capacity(
                            h as usize * row_len,
                            "TIFF NDPI cropped band",
                        )?;
                        for row in 0..h as usize {
                            let src = (src_y + row) * band_stride + src_x * channels;
                            let end = src.checked_add(row_len).ok_or_else(|| {
                                BioFormatsError::Format(
                                    "TIFF NDPI cropped band row overflows".into(),
                                )
                            })?;
                            out.extend_from_slice(band.pixels.get(src..end).ok_or_else(|| {
                                BioFormatsError::InvalidData(
                                    "TIFF NDPI cropped band row is outside decoded data".into(),
                                )
                            })?);
                        }
                        return Ok(out);
                    }
                    // The band is a sub-rectangle [band_x0, band_x0+band_width) ×
                    // [band_y0, band_y0+band_height) that fully contains the
                    // requested [x,x+w) columns. Emit `h` FULL-WIDTH rows (with the
                    // band's columns placed at `band_x0`, the rest left zero) so the
                    // shared apply_photometric + column-crop below produce exactly
                    // the same output as the generic path. Only the [x,x+w) columns
                    // — which lie inside the band — survive the crop.
                    let band_stride = band.band_width as usize * channels;
                    let dst_off = band.band_x0 as usize * channels;
                    let copy_len = band_stride.min(row_bytes.saturating_sub(dst_off));
                    for out_row in 0..h as usize {
                        let img_row = y as usize + out_row;
                        let band_row = img_row.saturating_sub(band.band_y0 as usize);
                        let mut full = vec![0u8; row_bytes];
                        let rs = checked_mul_usize(band_row, band_stride, "TIFF NDPI band row")?;
                        if let Some(src) = band.pixels.get(rs..rs + copy_len) {
                            full[dst_off..dst_off + copy_len].copy_from_slice(src);
                        }
                        plane_rows.extend_from_slice(&full);
                    }
                    used_window = true;
                }
            }
        }
        // Cap the compressed-strip read used for windowing/indexing. A real
        // whole-slide JPEG level compresses to at most a few hundred MB; a strip
        // byte-count of multiple GB means a garbage >4 GB-NDPI offset table (it
        // clamps to "rest of file"). Don't read it — let the safety valve below
        // refuse the level cleanly instead of allocating gigabytes here.
        const MAX_STRIP_READ: u64 = 2 << 30; // 2 GiB
        if matches!(info.compression, Compression::Jpeg | Compression::JpegNew)
            && !ycbcr
            && info.strip_offsets.len() == 1
            && info.strip_byte_counts.len() == 1
            && info.rows_per_strip >= info.height
            && (info.strip_byte_counts[0] as u64) <= MAX_STRIP_READ
        {
            let offset = info.strip_offsets[0];
            let byte_count = info.strip_byte_counts[0] as usize;
            let mut compressed = read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
            apply_fill_order(&mut compressed, info.fill_order, info.compression);
            let merged = match info.jpeg_tables.as_deref() {
                Some(tables) => merge_jpeg_tables(tables, &compressed),
                None => compressed,
            };
            let indexed = jpeg_restart::index(&merged);
            if std::env::var("BF_JR_DEBUG").is_ok() {
                eprintln!(
                    "[jpeg_restart] single-strip JPEG {}x{}: index={}",
                    info.width,
                    info.height,
                    if indexed.is_some() {
                        "Some(has restart markers)"
                    } else {
                        "None (no DRI/restart markers)"
                    }
                );
            }
            if let Some(idx) = indexed {
                let decoded = idx.decode_rows(&merged, y, h, jpeg_color_for(info));
                if decoded.is_none() && std::env::var("BF_JR_DEBUG").is_ok() {
                    eprintln!(
                        "[jpeg_restart] decode_rows=None (restart intervals not row-aligned)"
                    );
                }
                if let Some(result) = decoded {
                    let band = result?;
                    let channels = band
                        .pixels
                        .len()
                        .checked_div(band.band_width as usize * band.band_height.max(1) as usize)
                        .filter(|&c| c > 0)
                        .unwrap_or(effective_spp as usize);
                    if info.bits_per_sample == 8
                        && !packed_row_layout
                        && !subbyte_samples
                        && !matches!(
                            info.photometric,
                            Photometric::MinIsWhite | Photometric::Cmyk
                        )
                        && x.checked_add(w).is_some_and(|end| end <= band.band_width)
                        && y >= band.band_y0
                        && y_end <= band.band_y0 + band.band_height
                    {
                        let src_y = (y - band.band_y0) as usize;
                        let src_x = x as usize;
                        let band_stride = band.band_width as usize * channels;
                        let row_len = w as usize * channels;
                        let mut out = checked_vec_with_capacity(
                            h as usize * row_len,
                            "TIFF JPEG cropped band",
                        )?;
                        for row in 0..h as usize {
                            let src = (src_y + row) * band_stride + src_x * channels;
                            let end = src.checked_add(row_len).ok_or_else(|| {
                                BioFormatsError::Format(
                                    "TIFF JPEG cropped band row overflows".into(),
                                )
                            })?;
                            out.extend_from_slice(band.pixels.get(src..end).ok_or_else(|| {
                                BioFormatsError::InvalidData(
                                    "TIFF JPEG cropped band row is outside decoded data".into(),
                                )
                            })?);
                        }
                        return Ok(out);
                    }
                    // Crop [y, y_end) rows from the band, exactly like the generic
                    // path crops within a strip (band_y0 plays strip_start_row).
                    let row_start = y.saturating_sub(band.band_y0) as usize;
                    let row_end = y_end.saturating_sub(band.band_y0).min(band.band_height) as usize;
                    for row in row_start..row_end {
                        let rs = checked_mul_usize(row, row_bytes, "TIFF JPEG band row offset")?;
                        let re = rs.checked_add(row_bytes).ok_or_else(|| {
                            BioFormatsError::Format("TIFF JPEG band row range overflows".into())
                        })?;
                        let row_data = band.pixels.get(rs..re).ok_or_else(|| {
                            BioFormatsError::InvalidData(
                                "TIFF JPEG band row is outside decoded data".into(),
                            )
                        })?;
                        plane_rows.extend_from_slice(row_data);
                    }
                    used_window = true;
                }
            }
        }

        // Memory safety valve: a single strip covering the whole plane that we
        // could NOT window (no JPEG restart markers, unaligned intervals, or a
        // garbage >4 GB-NDPI strip) would otherwise be decoded in full here,
        // materialising the entire gigapixel plane (observed: 144 GiB on the
        // 6.5 GB Hamamatsu slide). Refuse it instead of OOMing. Real small
        // single-strip planes (< cap) and multi-strip/tiled reads are unaffected.
        if !used_window && info.strip_offsets.len() == 1 && info.rows_per_strip >= info.height {
            let bps = ((info.bits_per_sample as u64) + 7) / 8;
            let plane_bytes = (info.width as u64)
                .saturating_mul(info.height as u64)
                .saturating_mul(info.samples_per_pixel as u64)
                .saturating_mul(bps);
            const HARD_CAP: u64 = 1 << 30; // 1 GiB
            if plane_bytes > HARD_CAP {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "single-strip plane is ~{} MiB and could not be windowed (no JPEG restart markers / unaligned intervals); refusing full-plane decode to avoid exhausting memory",
                    plane_bytes >> 20
                )));
            }
        }

        for strip_idx in (0..info.strip_offsets.len()).take_while(|_| !used_window) {
            let strip_start_row = checked_strip_start_row(strip_idx, rows_per_strip)?;
            let strip_end_row = strip_start_row
                .checked_add(rows_per_strip)
                .unwrap_or(u32::MAX)
                .min(info.height);

            // Skip strips entirely above or below the requested region
            if strip_end_row <= y || strip_start_row >= y_end {
                continue;
            }

            let offset = info.strip_offsets[strip_idx];
            let mut byte_count = info.strip_byte_counts[strip_idx] as usize;
            if lzw_double_byte_counts {
                // Read up to double the stored count (Java IFD.java:870-877), but
                // never past the end of the file. The LZW decoder stops at the EOI
                // marker, so trailing bytes from the next strip / file tail are
                // harmless; this only guards the read length so read_exact (which
                // requires exactly N bytes) does not fail near EOF.
                let doubled = byte_count.saturating_mul(2);
                let remaining = file
                    .parser
                    .reader
                    .seek(SeekFrom::End(0))
                    .ok()
                    .map(|end| end.saturating_sub(offset) as usize)
                    .unwrap_or(doubled);
                byte_count = doubled.min(remaining);
            }

            let mut compressed = read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
            apply_fill_order(&mut compressed, info.fill_order, info.compression);
            let strip_rows = strip_end_row - strip_start_row;
            let expected = if ycbcr {
                checked_ycbcr_strip_bytes(info.width, strip_rows, info.ycbcr_subsampling)?
            } else {
                checked_mul_usize(strip_rows as usize, row_bytes, "TIFF strip byte count")?
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
                jpeg_color_for(info),
            )?;
            if info.compression != Compression::Nikon {
                require_decompressed_len("strip", strip_idx, strip_data.len(), expected)?;
            }
            strip_data.truncate(expected);

            // Crop rows within this strip to the requested y range
            let row_start = y.saturating_sub(strip_start_row) as usize;
            let row_end = y_end.saturating_sub(strip_start_row).min(strip_rows) as usize;

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
                let plane_len = checked_mul_usize(
                    info.width as usize,
                    strip_rows as usize,
                    "TIFF YCbCr plane length",
                )?;
                let row_w = info.width as usize;
                let two_plane_len = checked_mul_usize(2, plane_len, "TIFF YCbCr plane length")?;
                let three_plane_len = checked_mul_usize(3, plane_len, "TIFF YCbCr plane length")?;
                let r_plane = rgb.get(0..plane_len).ok_or_else(|| {
                    BioFormatsError::Format("TIFF YCbCr decoded R plane is truncated".into())
                })?;
                let g_plane = rgb.get(plane_len..two_plane_len).ok_or_else(|| {
                    BioFormatsError::Format("TIFF YCbCr decoded G plane is truncated".into())
                })?;
                let b_plane = rgb.get(two_plane_len..three_plane_len).ok_or_else(|| {
                    BioFormatsError::Format("TIFF YCbCr decoded B plane is truncated".into())
                })?;
                let rows_start = checked_mul_usize(row_start, row_w, "TIFF YCbCr crop offset")?;
                let rows_end = checked_mul_usize(row_end, row_w, "TIFF YCbCr crop offset")?;
                ycbcr_r.extend_from_slice(r_plane.get(rows_start..rows_end).ok_or_else(|| {
                    BioFormatsError::Format(
                        "TIFF YCbCr R crop range is outside decoded data".into(),
                    )
                })?);
                ycbcr_g.extend_from_slice(g_plane.get(rows_start..rows_end).ok_or_else(|| {
                    BioFormatsError::Format(
                        "TIFF YCbCr G crop range is outside decoded data".into(),
                    )
                })?);
                ycbcr_b.extend_from_slice(b_plane.get(rows_start..rows_end).ok_or_else(|| {
                    BioFormatsError::Format(
                        "TIFF YCbCr B crop range is outside decoded data".into(),
                    )
                })?);
                continue;
            }

            for row in row_start..row_end {
                let rs = checked_mul_usize(row, row_bytes, "TIFF strip row offset")?;
                let re = rs.checked_add(row_bytes).ok_or_else(|| {
                    BioFormatsError::Format("TIFF strip row range overflows".into())
                })?;
                if let Some(row_data) = strip_data.get(rs..re) {
                    plane_rows.extend_from_slice(row_data);
                } else if info.compression != Compression::Nikon {
                    return Err(BioFormatsError::InvalidData(format!(
                        "TIFF strip {strip_idx} row {row} is outside decoded data"
                    )));
                }
            }
        }

        if ycbcr {
            if x == 0 && w == info.width {
                let ycbcr_len = ycbcr_r
                    .len()
                    .checked_add(ycbcr_g.len())
                    .and_then(|v| v.checked_add(ycbcr_b.len()))
                    .ok_or_else(|| {
                        BioFormatsError::Format("TIFF YCbCr output length overflows".into())
                    })?;
                let mut out = checked_vec_with_capacity(ycbcr_len, "TIFF YCbCr output")?;
                out.extend_from_slice(&ycbcr_r);
                out.extend_from_slice(&ycbcr_g);
                out.extend_from_slice(&ycbcr_b);
                return Ok(out);
            }
            // Crop each plane (single sample per pixel) and emit planar R, G, B.
            let r = crop_unpacked_rows(&ycbcr_r, info.width, 1, x, w, h);
            let g = crop_unpacked_rows(&ycbcr_g, info.width, 1, x, w, h);
            let b = crop_unpacked_rows(&ycbcr_b, info.width, 1, x, w, h);
            let ycbcr_len = r
                .len()
                .checked_add(g.len())
                .and_then(|v| v.checked_add(b.len()))
                .ok_or_else(|| {
                    BioFormatsError::Format("TIFF YCbCr output length overflows".into())
                })?;
            let mut out = checked_vec_with_capacity(ycbcr_len, "TIFF YCbCr output")?;
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
            let mut out = crop_unpacked_rows(
                &unpacked,
                info.width,
                effective_spp * bytes_per_sample,
                x,
                w,
                h,
            );
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

        let x_start = checked_row_bytes(x, effective_spp, bytes_per_sample)?;
        let x_len = checked_row_bytes(w, effective_spp, bytes_per_sample)?;
        let full_row = row_bytes;
        let out_len = checked_mul_usize(h as usize, x_len, "TIFF cropped output length")?;
        let mut out = checked_vec_with_capacity(out_len, "TIFF cropped output")?;
        for row in 0..h as usize {
            let row_start = checked_mul_usize(row, full_row, "TIFF plane row offset")?;
            let row_end = row_start
                .checked_add(full_row)
                .ok_or_else(|| BioFormatsError::Format("TIFF plane row range overflows".into()))?;
            let src = plane_rows.get(row_start..row_end).ok_or_else(|| {
                BioFormatsError::InvalidData(format!("TIFF decoded plane row {row} is missing"))
            })?;
            let x_end = x_start
                .checked_add(x_len)
                .ok_or_else(|| BioFormatsError::Format("TIFF crop x range overflows".into()))?;
            out.extend_from_slice(src.get(x_start..x_end).ok_or_else(|| {
                BioFormatsError::Format("TIFF crop range is outside decoded row".into())
            })?);
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
            checked_packed_row_bytes(info.width, 1, info.bits_per_sample)?
        } else {
            checked_row_bytes(info.width, 1, bytes_per_sample)?
        };
        let rows_per_strip = if info.rows_per_strip == 0 || info.rows_per_strip >= info.height {
            info.height
        } else {
            info.rows_per_strip
        };
        let strips_per_channel = div_ceil_u32(info.height, rows_per_strip);
        // Mirror the LZW StripByteCount doubling in the chunky path / Java
        // IFD.getStripByteCounts (IFD.java:870-877): gated strictly on LZW with
        // RowsPerStrip absent or not evenly dividing the image height.
        let lzw_double_byte_counts = info.compression == Compression::Lzw
            && (!info.rows_per_strip_present
                || rows_per_strip == 0
                || info.height % rows_per_strip != 0);
        let x_start = checked_row_bytes(x, 1, bytes_per_sample)?;
        let x_len = checked_row_bytes(w, 1, bytes_per_sample)?;
        let out_len =
            checked_mul_usize(h as usize, x_len, "TIFF cropped output length").and_then(|v| {
                checked_mul_usize(
                    v,
                    info.samples_per_pixel as usize,
                    "TIFF cropped output length",
                )
            })?;
        let mut out = checked_vec_with_capacity(out_len, "TIFF cropped output")?;
        let y_end = y
            .checked_add(h)
            .ok_or_else(|| BioFormatsError::Format("TIFF region y range overflows".into()))?;

        for channel in 0..info.samples_per_pixel as usize {
            let channel_capacity =
                checked_mul_usize(h as usize, channel_row_bytes, "TIFF channel row bytes")?;
            let mut channel_rows =
                checked_vec_with_capacity(channel_capacity, "TIFF channel rows")?;
            for strip in 0..strips_per_channel as usize {
                let strip_start_row = checked_strip_start_row(strip, rows_per_strip)?;
                let strip_end_row = strip_start_row
                    .checked_add(rows_per_strip)
                    .unwrap_or(u32::MAX)
                    .min(info.height);
                if strip_end_row <= y || strip_start_row >= y_end {
                    continue;
                }

                let strip_idx = channel
                    .checked_mul(strips_per_channel as usize)
                    .and_then(|v| v.checked_add(strip))
                    .ok_or_else(|| BioFormatsError::Format("TIFF strip index overflows".into()))?;
                if strip_idx >= info.strip_offsets.len()
                    || strip_idx >= info.strip_byte_counts.len()
                {
                    continue;
                }
                let offset = info.strip_offsets[strip_idx];
                let mut byte_count = info.strip_byte_counts[strip_idx] as usize;
                if lzw_double_byte_counts {
                    let doubled = byte_count.saturating_mul(2);
                    let remaining = file
                        .parser
                        .reader
                        .seek(SeekFrom::End(0))
                        .ok()
                        .map(|end| end.saturating_sub(offset) as usize)
                        .unwrap_or(doubled);
                    byte_count = doubled.min(remaining);
                }
                let mut compressed = read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
                apply_fill_order(&mut compressed, info.fill_order, info.compression);
                let strip_rows = strip_end_row - strip_start_row;
                let expected = checked_mul_usize(
                    strip_rows as usize,
                    channel_row_bytes,
                    "TIFF strip byte count",
                )?;
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
                    jpeg_color_for(info),
                )?;
                if info.compression != Compression::Nikon {
                    require_decompressed_len("strip", strip_idx, strip_data.len(), expected)?;
                }
                strip_data.truncate(expected);

                let row_start = y.saturating_sub(strip_start_row) as usize;
                let row_end = y_end.saturating_sub(strip_start_row).min(strip_rows) as usize;
                for row in row_start..row_end {
                    let rs = checked_mul_usize(row, channel_row_bytes, "TIFF strip row offset")?;
                    let re = rs.checked_add(channel_row_bytes).ok_or_else(|| {
                        BioFormatsError::Format("TIFF strip row range overflows".into())
                    })?;
                    if let Some(row_data) = strip_data.get(rs..re) {
                        channel_rows.extend_from_slice(row_data);
                    } else if info.compression != Compression::Nikon {
                        return Err(BioFormatsError::InvalidData(format!(
                            "TIFF strip {strip_idx} row {row} is outside decoded data"
                        )));
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
                let full_row = channel_row_bytes;
                let x_end = x_start
                    .checked_add(x_len)
                    .ok_or_else(|| BioFormatsError::Format("TIFF crop x range overflows".into()))?;
                for row in 0..h as usize {
                    let row_start = checked_mul_usize(row, full_row, "TIFF channel row offset")?;
                    let row_end = row_start.checked_add(full_row).ok_or_else(|| {
                        BioFormatsError::Format("TIFF channel row range overflows".into())
                    })?;
                    let src = channel_rows.get(row_start..row_end).ok_or_else(|| {
                        BioFormatsError::InvalidData(format!(
                            "TIFF decoded channel {channel} row {row} is missing"
                        ))
                    })?;
                    out.extend_from_slice(src.get(x_start..x_end).ok_or_else(|| {
                        BioFormatsError::Format("TIFF crop range is outside decoded row".into())
                    })?);
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

        // JPEG-compressed YCbCr tiles decode to chunky RGB inside the JPEG codec;
        // fall through to the generic chunky-RGB tile path. Only non-JPEG YCbCr
        // uses the manual TIFF YCbCr (tag 529/532) conversion.
        if info.photometric == Photometric::YCbCr && !ycbcr_decoded_by_jpeg(info) {
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
        let subbyte = info.bits_per_sample < 8;
        let packed_tile_row_bytes =
            packed_row_bytes(info.tile_width, effective_spp, info.bits_per_sample);
        let unpacked_tile_row_bytes = (info.tile_width * effective_spp * bytes_per_sample) as usize;
        let tile_row_bytes = if subbyte {
            packed_tile_row_bytes
        } else {
            unpacked_tile_row_bytes
        };
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
                    jpeg_color_for(info),
                )?;
                require_decompressed_len("tile", tile_idx, tile_data.len(), tile_data_bytes)?;
                tile_data.truncate(tile_data_bytes);
                if subbyte {
                    tile_data = unpack_subbyte_samples(
                        &tile_data,
                        info.tile_width,
                        info.tile_height,
                        effective_spp,
                        info.bits_per_sample,
                    );
                }
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
                let src_row_bytes = if subbyte {
                    unpacked_tile_row_bytes
                } else {
                    tile_row_bytes
                };

                for row in 0..copy_h {
                    let src_off = ((src_y + row) * src_row_bytes)
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
                    jpeg_color_for(info),
                )?;
                require_decompressed_len("tile", tile_idx, tile_data.len(), tile_data_bytes)?;
                tile_data.truncate(tile_data_bytes);

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
                    let mut compressed =
                        read_bytes_at(&mut file.parser.reader, offset, byte_count)?;
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
                        jpeg_color_for(info),
                    )?;
                    require_decompressed_len("tile", tile_idx, tile_data.len(), tile_data_bytes)?;
                    tile_data.truncate(tile_data_bytes);
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

    /// Initialise from an external OME-XML document (a `companion.ome` file).
    /// The document itself contains no pixel data — every plane lives in a
    /// companion TIFF referenced via `<TiffData>/<UUID FileName>`. We open the
    /// first companion as the backing `TiffFile` so resolution/SubIFD machinery
    /// has something to read, and rely on `external_planes` for pixel access.
    fn init_from_external_ome_xml(&mut self, xml: &str, base_dir: Option<&Path>) -> Result<()> {
        if !is_ome_xml_description(xml) {
            return Err(BioFormatsError::Format(
                "companion.ome file does not contain OME-XML".into(),
            ));
        }
        // Locate the first companion TIFF so we can open a real TiffFile.
        let images = parse_ome_tiff_images(xml);
        let first_companion = images
            .iter()
            .flat_map(|img| img.tiff_data.iter())
            .find_map(|td| td.filename.as_deref())
            .and_then(|name| resolve_companion_path(base_dir, name))
            .ok_or_else(|| {
                BioFormatsError::Format(
                    "companion.ome references no resolvable companion TIFF".into(),
                )
            })?;

        let f = File::open(&first_companion).map_err(BioFormatsError::Io)?;
        let parser = TiffParser::new(BufReader::new(f))?;
        let little_endian = parser.little_endian;
        let mut tf = TiffFile {
            parser,
            ifds: Vec::new(),
        };
        tf.ifds = tf.parser.read_ifds()?;

        self.ome_xml = Some(xml.to_string());
        // Pass an empty IFD slice so every plane is resolved as external; the
        // backing `tf` only needs to exist for the reader to stay initialised.
        let series = Self::build_ome_series(&[], xml, little_endian, base_dir, None)
            .ok_or_else(|| BioFormatsError::Format("companion.ome has no usable images".into()))?;
        self.series = series;
        self.file = Some(tf);
        self.current_series = 0;
        self.current_resolution = 0;
        self.current_resolution_metadata = None;
        Ok(())
    }
}

fn is_interleaved_rgb(info: &IfdInfo) -> bool {
    info.planar_config == 1 && info.photometric != Photometric::YCbCr
}

fn pixel_type_from_bps_format(
    bps: u16,
    sample_format: u16,
) -> crate::common::pixel_type::PixelType {
    use crate::common::pixel_type::PixelType;
    // 1-bit bilevel pixels are exposed as a packed Bit plane (handled specially
    // by the sub-byte unpacking path), so keep that case before rounding.
    if bps == 1 {
        return PixelType::Bit;
    }
    // Mirror Java IFD.getPixelType: round the bit depth UP to the next multiple
    // of 8 before mapping, so 9-15 bit samples (unpacked at ceil(bits/8) bytes)
    // report a PixelType consistent with that byte stride. 24-bit non-float data
    // is promoted to 32-bit as Java does; 24-bit float maps to FLOAT.
    let mut rounded = bps;
    while rounded % 8 != 0 {
        rounded += 1;
    }
    if rounded == 24 && sample_format != 3 {
        rounded = 32;
    }
    match (rounded, sample_format) {
        (8, 2) => PixelType::Int8,
        (8, _) => PixelType::Uint8,
        // 16-bit is exposed as a 2-byte integer type to stay consistent with the
        // ceil(bits/8)=2 byte stride; there is no Float16 type in this crate.
        (16, 2) => PixelType::Int16,
        (16, _) => PixelType::Uint16,
        // 24-bit float (e.g. some FITS/scientific encodings): Java maps to FLOAT.
        // NOTE: the unpacking stride for 24-bit is 3 bytes (ceil(24/8)) whereas
        // Float32 is 4 bytes; 24-bit samples are vanishingly rare in practice.
        (24, _) => PixelType::Float32,
        (32, 2) => PixelType::Int32,
        (32, 3) => PixelType::Float32,
        (32, _) => PixelType::Uint32,
        (64, 3) => PixelType::Float64,
        // 64-bit integer data is unsupported (no Int64/Uint64 type); Java throws
        // for this case. Keep the prior fallback rather than mis-report a stride.
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

fn require_decompressed_len(
    block_kind: &str,
    block_index: usize,
    actual: usize,
    expected: usize,
) -> Result<()> {
    if actual < expected {
        return Err(BioFormatsError::InvalidData(format!(
            "TIFF {block_kind} {block_index} decompressed to {actual} bytes, expected {expected}"
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
        // Java (IFD.getTileOffsets/getTileByteCounts) errors only when there are
        // *too few* entries; some writers pad with extra trailing offsets, which
        // the tile read loop simply never indexes. Tolerate the extras.
        if tile_offsets.len() < expected {
            return Err(BioFormatsError::Format(format!(
                "TIFF TileOffsets/TileByteCounts count {} is fewer than expected tile count {}",
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
        // Java (IFD.getStripOffsets/getStripByteCounts) errors only when there are
        // *too few* entries; extra trailing strip offsets (writer padding) are
        // skipped by the strip read loop's row-range bounds check. Tolerate them.
        if strip_offsets.len() < expected {
            return Err(BioFormatsError::Format(format!(
                "TIFF StripOffsets/StripByteCounts count {} is fewer than expected strip count {}",
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
        let (pixel_type, type_bits) = xml_attr(pixels_tag, "Type")
            .as_deref()
            .map(parse_ome_pixel_type)
            .unwrap_or((PixelType::Uint8, 8));
        // Java OMETiffReader.initFile:1250-1252 overrides m.bitsPerPixel with the
        // Pixels SignificantBits attribute when present, falling back to the
        // pixel-type default otherwise.
        let bits_per_pixel = parse_u32_attr(pixels_tag, "SignificantBits")
            .filter(|&b| b > 0 && b <= u8::MAX as u32)
            .map(|b| b as u8)
            .unwrap_or(type_bits);

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
                // Look for a nested <UUID FileName="..."> inside this TiffData;
                // when present the plane's pixels live in a companion TIFF.
                let body_start = pos + tag.len();
                let self_closing = tag.trim_end().ends_with("/>");
                let filename = if self_closing {
                    None
                } else {
                    let body_end = matching_end_tag_start(pixels_xml, pos, "TiffData")
                        .unwrap_or(pixels_xml.len());
                    let body = pixels_xml.get(body_start..body_end).unwrap_or("");
                    start_tag_positions(body, "UUID")
                        .first()
                        .map(|&up| start_tag_at(body, up))
                        .and_then(|t| xml_attr(t, "FileName"))
                        .filter(|s| !s.trim().is_empty())
                };
                OmeTiffData {
                    ifd: parse_u32_attr(tag, "IFD").unwrap_or(0) as usize,
                    plane_count: parse_u32_attr(tag, "PlaneCount").map(|v| v as usize),
                    first_z: parse_u32_attr(tag, "FirstZ").unwrap_or(0),
                    first_c: parse_u32_attr(tag, "FirstC").unwrap_or(0),
                    first_t: parse_u32_attr(tag, "FirstT").unwrap_or(0),
                    filename,
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
            samples_per_pixel,
            dimension_order,
            tiff_data,
        });
    }

    images
}

/// Open a companion TIFF and read the `IfdInfo` for the IFD a given external
/// plane points at. Used to derive sample-layout metadata (RGB/indexed/LUT) when
/// every plane of an OME image lives in companion files (binary-only OME-TIFF).
fn external_plane_ifd_info(plane: &ExternalPlane) -> Option<IfdInfo> {
    let f = File::open(&plane.path).ok()?;
    let parser = TiffParser::new(BufReader::new(f)).ok()?;
    let little_endian = parser.little_endian;
    let mut tf = TiffFile {
        parser,
        ifds: Vec::new(),
    };
    tf.ifds = tf.parser.read_ifds().ok()?;
    let ifd = tf.ifds.get(plane.ifd).or_else(|| tf.ifds.first())?;
    TiffReader::ifd_info(ifd, little_endian).ok()
}

/// Resolve, for each `<TiffData>`, the companion file its pixels live in (if any).
/// Returns `None` for a TiffData whose pixels are in the current file (no
/// `<UUID FileName>`, or a FileName equal to the current file). Returns
/// `Some(path)` for a companion file that exists. A FileName that does not
/// resolve to an existing file yields `Some(None)` so the planes are left blank
/// rather than mis-mapped onto local IFDs (mirrors Java's "missing file" path,
/// which warns and leaves the plane unset).
fn resolve_tiff_data_companions(
    image: &OmeTiffImage,
    base_dir: Option<&Path>,
    current_path: Option<&Path>,
) -> Vec<Option<Option<PathBuf>>> {
    image
        .tiff_data
        .iter()
        .map(|td| {
            let Some(name) = td.filename.as_deref() else {
                return None; // pixels in current file
            };
            let resolved = resolve_companion_path(base_dir, name);
            // If the FileName points back at the current file, treat as local.
            if let (Some(resolved), Some(cur)) = (resolved.as_ref(), current_path) {
                if paths_equal(resolved, cur) {
                    return None;
                }
            }
            Some(resolved)
        })
        .collect()
}

/// Compare two paths by canonicalised form when possible, falling back to a
/// component comparison.
fn paths_equal(a: &Path, b: &Path) -> bool {
    match (a.canonicalize(), b.canonicalize()) {
        (Ok(ca), Ok(cb)) => ca == cb,
        _ => a == b,
    }
}

/// Resolve a `<UUID FileName>` relative to `base_dir`, returning the path only if
/// it exists (mirrors Java `normalizeFilename` + existence check, with the
/// basename retry used for old absolute-path writers).
fn resolve_companion_path(base_dir: Option<&Path>, filename: &str) -> Option<PathBuf> {
    let trimmed = filename.trim();
    if trimmed.is_empty() {
        return None;
    }
    let filename_path = Path::new(trimmed);
    if filename_path.is_absolute() {
        if filename_path.exists() {
            return Some(filename_path.to_path_buf());
        }
        // Old writers stored absolute paths; retry with basename in base_dir.
        if let (Some(dir), Some(base)) = (base_dir, filename_path.file_name()) {
            let retry = dir.join(base);
            if retry.exists() {
                return Some(retry);
            }
        }
        return None;
    }
    match base_dir {
        Some(dir) => {
            let candidate = confined_join(dir, trimmed)?;
            if candidate.exists() {
                Some(candidate)
            } else {
                None
            }
        }
        None => {
            // No directory context: only accept a bare filename in the cwd.
            if filename_path
                .components()
                .any(|c| matches!(c, std::path::Component::ParentDir))
            {
                return None;
            }
            if filename_path.exists() {
                Some(filename_path.to_path_buf())
            } else {
                None
            }
        }
    }
}

/// Build both the local logical-plane -> IFD map and the logical-plane ->
/// companion-file map for one OME image. `companions[i]` describes where
/// `image.tiff_data[i]`'s pixels live (see `resolve_tiff_data_companions`).
fn build_ome_plane_maps(
    image: &OmeTiffImage,
    physical_ifd_count: usize,
    companions: &[Option<Option<PathBuf>>],
) -> (Vec<Option<usize>>, Vec<Option<ExternalPlane>>) {
    let plane_count = image
        .size_z
        .saturating_mul(image.effective_c)
        .saturating_mul(image.size_t) as usize;
    let mut map = vec![None; plane_count];
    let mut external: Vec<Option<ExternalPlane>> = vec![None; plane_count];
    if plane_count == 0 {
        return (map, external);
    }

    if image.tiff_data.is_empty() {
        for (plane, slot) in map.iter_mut().enumerate() {
            if plane < physical_ifd_count {
                *slot = Some(plane);
            }
        }
        return (map, external);
    }

    let mut z_one_indexed: Option<bool> = None;
    let mut c_one_indexed: Option<bool> = None;
    let mut t_one_indexed: Option<bool> = None;
    for td in &image.tiff_data {
        if td.first_c >= image.effective_c && c_one_indexed.is_none() {
            c_one_indexed = Some(true);
        } else if td.first_c == 0 {
            c_one_indexed = Some(false);
        }
        if td.first_z >= image.size_z && z_one_indexed.is_none() {
            z_one_indexed = Some(true);
        } else if td.first_z == 0 {
            z_one_indexed = Some(false);
        }
        if td.first_t >= image.size_t && t_one_indexed.is_none() {
            t_one_indexed = Some(true);
        } else if td.first_t == 0 {
            t_one_indexed = Some(false);
        }
        if td.first_z == 0 && td.first_c == 0 && td.first_t == 0 {
            break;
        }
    }

    // For each TiffData index, where do its pixels live? The closure returns
    // `None` for a local TiffData (pixels in the current file) and `Some(opt)`
    // for a companion TiffData (`opt` = resolved path, or `None` if missing).
    let companion_for =
        |i: usize| -> Option<&Option<PathBuf>> { companions.get(i).and_then(|c| c.as_ref()) };

    let mut explicit_starts = vec![false; plane_count];
    for (i, td) in image.tiff_data.iter().enumerate() {
        let (first_z, first_c, first_t) =
            normalize_ome_tiff_coordinates(td, z_one_indexed, c_one_indexed, t_one_indexed);
        if let Some(logical) = ome_plane_index(
            first_z,
            first_c,
            first_t,
            image.size_z,
            image.effective_c,
            image.size_t,
            image.dimension_order,
        ) {
            explicit_starts[logical] = true;
            match companion_for(i) {
                // Pixels in a companion file: record (or leave blank if missing).
                Some(comp) => {
                    if let Some(path) = comp {
                        external[logical] = Some(ExternalPlane {
                            path: path.clone(),
                            ifd: td.ifd,
                        });
                    }
                }
                // Pixels in the current file.
                None => {
                    if td.ifd < physical_ifd_count {
                        map[logical] = Some(td.ifd);
                    }
                }
            }
        }
    }

    for (i, td) in image.tiff_data.iter().enumerate() {
        let (first_z, first_c, first_t) =
            normalize_ome_tiff_coordinates(td, z_one_indexed, c_one_indexed, t_one_indexed);
        let Some(start_logical) = ome_plane_index(
            first_z,
            first_c,
            first_t,
            image.size_z,
            image.effective_c,
            image.size_t,
            image.dimension_order,
        ) else {
            continue;
        };
        let companion = companion_for(i);
        let limit = td
            .plane_count
            .unwrap_or_else(|| plane_count.saturating_sub(start_logical));
        let mut z = first_z;
        let mut c = first_c;
        let mut t = first_t;
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
            match companion {
                Some(comp) => {
                    if let Some(path) = comp {
                        external[logical] = Some(ExternalPlane {
                            path: path.clone(),
                            ifd: ifd_index,
                        });
                    }
                }
                None => {
                    if ifd_index >= physical_ifd_count {
                        break;
                    }
                    map[logical] = Some(ifd_index);
                }
            }
        }
    }

    (map, external)
}

fn normalize_ome_tiff_coordinates(
    td: &OmeTiffData,
    z_one_indexed: Option<bool>,
    c_one_indexed: Option<bool>,
    t_one_indexed: Option<bool>,
) -> (u32, u32, u32) {
    let mut z = td.first_z;
    let mut c = td.first_c;
    let mut t = td.first_t;
    if z_one_indexed == Some(true) && z > 0 {
        z -= 1;
    }
    if c_one_indexed == Some(true) && c > 0 {
        c -= 1;
    }
    if t_one_indexed == Some(true) && t > 0 {
        t -= 1;
    }
    (z, c, t)
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

/// Java TiffReader.checkCommentImageJ: an ImageDescription comment is ImageJ-style
/// if it begins with `ImageJ=`.
fn check_comment_imagej(comment: &str) -> bool {
    comment.starts_with("ImageJ=")
}

/// Mirror of Java TiffReader.parseCommentImageJ + populateMetadataStoreImageJ,
/// restricted to the *metadata* it produces (no dimension/order changes). Parses
/// the ImageJ newline-delimited key=value comment and records the standard fields
/// Java's BaseTiffReader/TiffReader surface:
///   - `description`        (the comment, newlines joined with "; " as in
///     initMetadataStore -> setImageDescription)
///   - `Unit`               (calibrationUnit, from `unit=`)
///   - `Spacing` / PhysicalSizeZ      (from `spacing=`)
///   - `Frame Interval` / TimeIncrement (seconds, from `finterval=`)
///   - `X Origin` / `Y Origin`        (plane stage origin, from `xorigin=`/`yorigin=`)
///   - `Color mode`         (from `mode=`)
/// plus any other `key=value` token as an original-metadata entry.
///
/// Java keys are preserved verbatim so downstream consumers match the reference.
/// Dimension sizes (channels/slices/frames/images) are intentionally NOT applied:
/// this port keeps the existing TIFF dimension/series logic unchanged.
fn parse_comment_imagej(
    comment: &str,
    out: &mut HashMap<String, crate::common::metadata::MetadataValue>,
) {
    use crate::common::metadata::MetadataValue;

    // put("ImageJ", first line after the "ImageJ=" prefix)
    let after_prefix = &comment[7..];
    let imagej_value = match after_prefix.find('\n') {
        Some(nl) => &after_prefix[..nl],
        None => after_prefix,
    };
    out.insert(
        "ImageJ".into(),
        MetadataValue::String(imagej_value.to_string()),
    );

    let mut physical_size_z: Option<f64> = None;
    let mut time_increment: Option<f64> = None;

    for token in comment.split('\n') {
        let eq = token.find('=');
        let value = eq.map(|i| &token[i + 1..]);

        if let Some(value) = token.strip_prefix("mode=") {
            out.insert("Color mode".into(), MetadataValue::String(value.to_string()));
        } else if let Some(value) = token.strip_prefix("unit=") {
            out.insert("Unit".into(), MetadataValue::String(value.to_string()));
        } else if let Some(value) = token.strip_prefix("finterval=") {
            if let Ok(v) = value.trim().parse::<f64>() {
                time_increment = Some(v);
                // Java stores a Time(seconds) object; surface the numeric value.
                out.insert("Frame Interval".into(), MetadataValue::Float(v));
            }
        } else if let Some(value) = token.strip_prefix("spacing=") {
            if let Ok(v) = value.trim().parse::<f64>() {
                physical_size_z = Some(v);
                out.insert("Spacing".into(), MetadataValue::Float(v));
            }
        } else if let Some(value) = token.strip_prefix("xorigin=") {
            if let Ok(v) = value.trim().parse::<i64>() {
                out.insert("X Origin".into(), MetadataValue::Int(v));
            }
        } else if let Some(value) = token.strip_prefix("yorigin=") {
            if let Ok(v) = value.trim().parse::<i64>() {
                out.insert("Y Origin".into(), MetadataValue::Int(v));
            }
        } else if let (Some(eq), Some(value)) = (eq, value) {
            if eq > 0 {
                let key = token[..eq].trim();
                if !key.is_empty() {
                    out.insert(key.to_string(), MetadataValue::String(value.to_string()));
                }
            }
        }
    }

    // populateMetadataStoreImageJ: PhysicalSizeZ uses the absolute value; Java
    // stores it on Pixels. We surface it under the OME-style key as well so it is
    // discoverable alongside the original "Spacing" entry.
    if let Some(z) = physical_size_z {
        let z = z.abs();
        out.insert("PhysicalSizeZ".into(), MetadataValue::Float(z));
    }
    if let Some(t) = time_increment {
        out.insert("TimeIncrement".into(), MetadataValue::Float(t));
    }

    // initMetadataStore: description = comment with newlines replaced by "; ".
    let description = comment.replace('\n', "; ");
    out.insert("description".into(), MetadataValue::String(description));
}

/// True if `path` ends with the OME companion-metadata suffix `companion.ome`
/// (Java OMETiffReader.checkSuffix). Case-insensitive.
fn has_companion_ome_suffix(path: &Path) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase().ends_with("companion.ome"))
        .unwrap_or(false)
}

/// Extract the `MetadataFile` reference of a binary-only OME-TIFF. The OME-XML of
/// a binary-only file is just `<OME><BinaryOnly UUID="..." MetadataFile="..."/></OME>`
/// (Java BinaryOnly / meta.getBinaryOnlyMetadataFile). Returns the referenced
/// filename when present.
fn binary_only_metadata_file(xml: &str) -> Option<String> {
    start_tag_positions(xml, "BinaryOnly")
        .first()
        .map(|&pos| start_tag_at(xml, pos))
        .and_then(|tag| xml_attr(tag, "MetadataFile"))
        .filter(|s| !s.trim().is_empty())
}

/// Read the OME-XML from a companion metadata file. Mirrors Java
/// `OMETiffReader.readMetadataFile`: when the metadata file is itself an OME-TIFF
/// the XML lives in the first IFD's ImageDescription; a plain `.ome` (XML) file
/// is read directly.
fn read_metadata_companion(path: &Path) -> Result<String> {
    let lower = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase())
        .unwrap_or_default();
    let is_tiff = lower.ends_with("ome.tiff")
        || lower.ends_with("ome.tif")
        || lower.ends_with("ome.tf2")
        || lower.ends_with("ome.tf8")
        || lower.ends_with("ome.btf");
    if is_tiff {
        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut parser = TiffParser::new(BufReader::new(f))?;
        let ifds = parser.read_ifds()?;
        let first = ifds
            .first()
            .ok_or_else(|| BioFormatsError::Format("metadata OME-TIFF has no IFDs".into()))?;
        first
            .get_str(tag::IMAGE_DESCRIPTION)
            .map(str::to_owned)
            .ok_or_else(|| {
                BioFormatsError::Format("metadata OME-TIFF has no ImageDescription".into())
            })
    } else {
        std::fs::read_to_string(path).map_err(BioFormatsError::Io)
    }
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

fn checked_mul_usize(a: usize, b: usize, context: &str) -> Result<usize> {
    a.checked_mul(b)
        .ok_or_else(|| BioFormatsError::Format(format!("{context} overflows usize")))
}

fn checked_vec_with_capacity<T>(capacity: usize, context: &str) -> Result<Vec<T>> {
    const MAX_TIFF_BUFFER_BYTES: usize = 1 << 31;
    if capacity > MAX_TIFF_BUFFER_BYTES {
        return Err(BioFormatsError::Format(format!(
            "{context} is too large to allocate"
        )));
    }
    let mut out = Vec::new();
    out.try_reserve_exact(capacity)
        .map_err(|_| BioFormatsError::Format(format!("{context} allocation failed")))?;
    Ok(out)
}

fn checked_row_bytes(width: u32, samples_per_pixel: u32, bytes_per_sample: u32) -> Result<usize> {
    checked_mul_usize(
        width as usize,
        samples_per_pixel as usize,
        "TIFF row samples",
    )
    .and_then(|v| checked_mul_usize(v, bytes_per_sample as usize, "TIFF row bytes"))
}

fn checked_packed_row_bytes(
    width: u32,
    samples_per_pixel: u32,
    bits_per_sample: u16,
) -> Result<usize> {
    let bits = checked_mul_usize(
        width as usize,
        samples_per_pixel as usize,
        "TIFF row samples",
    )
    .and_then(|v| checked_mul_usize(v, bits_per_sample as usize, "TIFF packed row bits"))?;
    bits.checked_add(7)
        .map(|v| v / 8)
        .ok_or_else(|| BioFormatsError::Format("TIFF packed row bytes overflow".into()))
}

fn checked_strip_start_row(strip_idx: usize, rows_per_strip: u32) -> Result<u32> {
    let row = (strip_idx as u64)
        .checked_mul(rows_per_strip as u64)
        .ok_or_else(|| BioFormatsError::Format("TIFF strip row offset overflows".into()))?;
    u32::try_from(row)
        .map_err(|_| BioFormatsError::Format("TIFF strip row offset overflows u32".into()))
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

    // Sub-byte depths (1/2/4 bits) arrive as one unpacked sample per byte; invert
    // against the sub-byte maximum directly.
    if bits_per_sample < 8 {
        let max = ((1u16 << bits_per_sample) - 1) as u8;
        for b in data {
            *b = max.saturating_sub(*b);
        }
        return;
    }

    // Byte-aligned and packed (>=8 bit) samples are stored as `bytes_per_sample`
    // little/big-endian bytes by the unpacking code. Mirror Java TiffParser's
    // `value = maxValue - value` with `maxValue = 2^bps - 1` over that stride, so
    // 12-bit, 32-bit, etc. WhiteIsZero/CMYK images are inverted instead of being
    // silently passed through.
    let bytes_per_sample = ((bits_per_sample as usize) + 7) / 8;
    if bytes_per_sample == 0 {
        return;
    }
    // 2^bps - 1 (saturates at u64::MAX for bps >= 64, which never inverts below).
    let max: u64 = if bits_per_sample >= 64 {
        u64::MAX
    } else {
        (1u64 << bits_per_sample) - 1
    };
    for px in data.chunks_exact_mut(bytes_per_sample) {
        let mut value: u64 = 0;
        for b in 0..bytes_per_sample {
            let byte = px[b] as u64;
            if little_endian {
                value |= byte << (8 * b);
            } else {
                value |= byte << (8 * (bytes_per_sample - 1 - b));
            }
        }
        let inverted = max.wrapping_sub(value) & max;
        for b in 0..bytes_per_sample {
            let shift = if little_endian {
                8 * b
            } else {
                8 * (bytes_per_sample - 1 - b)
            };
            px[b] = ((inverted >> shift) & 0xff) as u8;
        }
    }
}

/// True when a YCbCr IFD is JPEG-compressed (old- or new-style). The JPEG
/// decoder performs the YCbCr->RGB conversion internally and emits chunky RGB,
/// so these IFDs are decoded through the generic RGB path rather than the manual
/// TIFF YCbCr (tag 529/532) conversion. Mirrors Java, where the JPEG codec
/// returns RGB and TiffParser does not re-apply YCbCr math to JPEG output.
fn ycbcr_decoded_by_jpeg(info: &IfdInfo) -> bool {
    info.photometric == Photometric::YCbCr
        && matches!(info.compression, Compression::Jpeg | Compression::JpegNew)
}

/// Decide how a JPEG-compressed strip/tile's components map to output channels.
///
/// Java reads Aperio/Leica TIFF JPEG tiles whose `PhotometricInterpretation` is
/// RGB (2) WITHOUT applying the YCbCr→RGB transform (ImageIO emits the stored
/// components as-is); libjpeg/`jpeg_decoder` would otherwise assume YCbCr and
/// corrupt the pixels. YCbCr-photometric JPEGs keep the default conversion.
fn jpeg_color_for(info: &IfdInfo) -> JpegColor {
    if matches!(info.compression, Compression::Jpeg | Compression::JpegNew)
        && info.photometric == Photometric::Rgb
    {
        JpegColor::Rgb
    } else {
        JpegColor::Default
    }
}

fn is_unsupported_ycbcr(info: &IfdInfo) -> bool {
    info.bits_per_sample != 8
        || info.planar_config != 1
        || info.samples_per_pixel < 3
        || matches!(info.compression, Compression::JpegXR)
}

fn ycbcr_strip_bytes(width: u32, rows: u32, subsampling: (u16, u16)) -> usize {
    let h = subsampling.0.max(1) as u32;
    let v = subsampling.1.max(1) as u32;
    let blocks_x = (width + h - 1) / h;
    let blocks_y = (rows + v - 1) / v;
    (blocks_x * blocks_y * (h * v + 2)) as usize
}

fn checked_ycbcr_strip_bytes(width: u32, rows: u32, subsampling: (u16, u16)) -> Result<usize> {
    let h = subsampling.0.max(1) as u64;
    let v = subsampling.1.max(1) as u64;
    let blocks_x = (width as u64)
        .checked_add(h - 1)
        .ok_or_else(|| BioFormatsError::Format("TIFF YCbCr block width overflows".into()))?
        / h;
    let blocks_y = (rows as u64)
        .checked_add(v - 1)
        .ok_or_else(|| BioFormatsError::Format("TIFF YCbCr block height overflows".into()))?
        / v;
    let samples_per_block = h
        .checked_mul(v)
        .and_then(|value| value.checked_add(2))
        .ok_or_else(|| BioFormatsError::Format("TIFF YCbCr block size overflows".into()))?;
    let bytes = blocks_x
        .checked_mul(blocks_y)
        .and_then(|value| value.checked_mul(samples_per_block))
        .ok_or_else(|| BioFormatsError::Format("TIFF YCbCr strip bytes overflow".into()))?;
    usize::try_from(bytes)
        .map_err(|_| BioFormatsError::Format("TIFF YCbCr strip bytes overflow usize".into()))
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
    let plane_len = checked_mul_usize(width as usize, height as usize, "TIFF YCbCr plane length")?;
    let mut r = vec![0u8; plane_len];
    let mut g = vec![0u8; plane_len];
    let mut b = vec![0u8; plane_len];
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

/// Parse a Canon DNG EXIF maker-note blob into a TIFF IFD.
///
/// Canon stores the maker-note as a TIFF fragment whose value offsets are
/// relative to a base recorded in the blob's trailing bytes. Bio-Formats
/// (`DNGReader.initStandardMetadata`) reconstructs a self-contained buffer:
/// the last 4 bytes give `offset`; a new buffer of length `len + offset - 8`
/// is built, the trailing 8 bytes (a mini-TIFF header: byte-order, magic,
/// first-IFD pointer) are copied to position 0, and the leading `len - 8`
/// body bytes are copied to position `offset`. The result is then parsed as a
/// standalone TIFF and its first IFD returned. Returns `None` on any malformed
/// input (matching Java, which logs and continues).
fn parse_canon_maker_note(data: &[u8], little_endian: bool) -> Option<Ifd> {
    if data.len() < 8 {
        return None;
    }
    let n = data.len();
    let off_bytes = &data[n - 4..n];
    let offset = if little_endian {
        u32::from_le_bytes([off_bytes[0], off_bytes[1], off_bytes[2], off_bytes[3]]) as usize
    } else {
        u32::from_be_bytes([off_bytes[0], off_bytes[1], off_bytes[2], off_bytes[3]]) as usize
    };

    // Java: new byte[b.length + offset - 8]. Guard against pathological offsets.
    let new_len = (n + offset).checked_sub(8)?;
    if offset < 8 || new_len < n {
        return None;
    }
    let mut buf = vec![0u8; new_len];
    // Trailing 8 bytes (mini-TIFF header) -> start.
    buf[0..8].copy_from_slice(&data[n - 8..n]);
    // Leading body -> position `offset`.
    if offset + (n - 8) > buf.len() {
        return None;
    }
    buf[offset..offset + (n - 8)].copy_from_slice(&data[0..n - 8]);

    let mut parser = TiffParser::new(std::io::Cursor::new(buf)).ok()?;
    let _ = little_endian; // header in `buf` dictates endianness, as in Java.
    parser
        .read_ifd(parser.first_ifd_offset)
        .ok()
        .map(|(ifd, _)| ifd)
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
            Some("tif") | Some("tiff") | Some("btf") | Some("tf8")
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
        self.path = Some(path.to_path_buf());
        let base_dir = path.parent();

        // ── companion.ome: the file IS an OME-XML metadata document whose pixels
        // live in companion TIFFs (Java OMETiffReader.initFile ~:511-530). It is
        // not itself a TIFF, so open the first referenced companion TIFF to get
        // the IFD chain and read external planes from there.
        if has_companion_ome_suffix(path) {
            let xml = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
            return self.init_from_external_ome_xml(&xml, base_dir);
        }

        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let buf = BufReader::new(f);
        let parser = TiffParser::new(buf)?;
        let little_endian = parser.little_endian;

        // We need to read IFDs. Move parser into a temporary to call read_ifds.
        let mut tf = TiffFile {
            parser,
            ifds: Vec::new(),
        };
        tf.ifds = if self.ndpi_64bit {
            tf.parser.read_ifds_ndpi64()?
        } else {
            tf.parser.read_ifds()?
        };
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

        // ── Binary-only OME-TIFF: the inline OME-XML is just a pointer to an
        // external companion OME-XML that holds the real metadata (Java
        // OMETiffReader.initFile ~:537-569, <BinaryOnly MetadataFile="...">).
        if let Some(xml) = self.ome_xml.clone() {
            if let Some(meta_file) = binary_only_metadata_file(&xml) {
                if let Some(meta_path) = resolve_companion_path(base_dir, &meta_file) {
                    if let Ok(full_xml) = read_metadata_companion(&meta_path) {
                        // Companions are resolved relative to the metadata file's
                        // directory (Java sets dir = path.getParentFile()).
                        let meta_dir = meta_path.parent().map(Path::to_path_buf);
                        self.ome_xml = Some(full_xml.clone());
                        if let Some(ome_series) = Self::build_ome_series(
                            &tf.ifds,
                            &full_xml,
                            little_endian,
                            meta_dir.as_deref(),
                            Some(path),
                        ) {
                            self.series = ome_series;
                        }
                        self.file = Some(tf);
                        self.current_series = 0;
                        self.current_resolution = 0;
                        self.current_resolution_metadata = None;
                        self.parse_sub_ifds()?;
                        self.add_nikon_raw_sub_ifd_series()?;
                        return Ok(());
                    }
                }
            }
        }

        if let Some(xml) = self.ome_xml.as_deref() {
            if let Some(ome_series) =
                Self::build_ome_series(&tf.ifds, xml, little_endian, base_dir, Some(path))
            {
                self.series = ome_series;
            }
        }
        self.file = Some(tf);
        self.current_series = 0;
        self.current_resolution = 0;
        self.current_resolution_metadata = None;
        // Parse SubIFD chains for pyramid support
        self.parse_sub_ifds()?;
        self.add_nikon_raw_sub_ifd_series()?;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.file = None;
        self.series.clear();
        self.current_resolution_metadata = None;
        self.ome_xml = None;
        self.path = None;
        self.companion_readers.clear();
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
        self.current_resolution = 0;
        self.current_resolution_metadata = None;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &crate::common::metadata::ImageMetadata {
        if let Some(meta) = &self.current_resolution_metadata {
            return meta;
        }
        self.series
            .get(self.current_series)
            .map(|series| &series.metadata)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if let Some(ext) = self.external_plane_for(plane_index) {
            let (w, h) = {
                let m = self.metadata();
                (m.size_x, m.size_y)
            };
            return self.read_external_plane(&ext, 0, 0, w, h);
        }
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
        if let Some(ext) = self.external_plane_for(plane_index) {
            return self.read_external_plane(&ext, x, y, w, h);
        }
        let ifd_index = self.resolve_ifd_index(plane_index)?;
        self.read_plane_bytes(ifd_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        // Return a small center crop (max 256x256) as a thumbnail.
        if let Some(ext) = self.external_plane_for(plane_index) {
            let (w, h) = {
                let m = self.metadata();
                (m.size_x, m.size_y)
            };
            let tw = w.min(256);
            let th = h.min(256);
            let tx = (w - tw) / 2;
            let ty = (h - th) / 2;
            return self.read_external_plane(&ext, tx, ty, tw, th);
        }
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
        self.current_resolution_metadata = self.resolution_metadata(level)?;
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

    #[test]
    fn ome_tiff_plane_map_accepts_one_indexed_tiffdata_coordinates() {
        let image = OmeTiffImage {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 2,
            effective_c: 2,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            samples_per_pixel: 1,
            dimension_order: DimensionOrder::XYZCT,
            tiff_data: vec![
                OmeTiffData {
                    ifd: 0,
                    plane_count: Some(1),
                    first_z: 1,
                    first_c: 1,
                    first_t: 1,
                    filename: None,
                },
                OmeTiffData {
                    ifd: 1,
                    plane_count: Some(1),
                    first_z: 1,
                    first_c: 2,
                    first_t: 1,
                    filename: None,
                },
            ],
        };

        let companions = vec![None, None];
        let (map, external) = build_ome_plane_maps(&image, 2, &companions);
        assert_eq!(map, vec![Some(0), Some(1)]);
        assert!(external.iter().all(Option::is_none));
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

    fn synthetic_huge_chunky_stripped_tiff() -> Vec<u8> {
        let main_ifd_offset = 8u32;
        let entry_count = 9u16;
        let ifd_bytes = 2 + entry_count as u32 * 12 + 4;
        let pixel_offset = main_ifd_offset + ifd_bytes;
        let pixels = [1u8, 2, 3, 4];

        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        push_u16_le(&mut data, 42);
        push_u32_le(&mut data, main_ifd_offset);

        push_u16_le(&mut data, entry_count);
        push_ifd_long(&mut data, tag::IMAGE_WIDTH, u32::MAX);
        push_ifd_long(&mut data, tag::IMAGE_LENGTH, 1);
        push_ifd_short(&mut data, tag::BITS_PER_SAMPLE, 8);
        push_ifd_short(&mut data, tag::COMPRESSION, 1);
        push_ifd_short(&mut data, tag::PHOTOMETRIC_INTERPRETATION, 2);
        push_ifd_long(&mut data, tag::STRIP_OFFSETS, pixel_offset);
        push_ifd_short(&mut data, tag::SAMPLES_PER_PIXEL, 4);
        push_ifd_long(&mut data, tag::ROWS_PER_STRIP, 1);
        push_ifd_long(&mut data, tag::STRIP_BYTE_COUNTS, pixels.len() as u32);
        push_u32_le(&mut data, 0);
        data.extend_from_slice(&pixels);
        data
    }

    #[test]
    fn stripped_reader_rejects_huge_row_without_wrapping_or_allocating() {
        let path = std::env::temp_dir().join(format!(
            "bioformats-rs-huge-stripped-row-{}.tif",
            std::process::id()
        ));
        fs::write(&path, synthetic_huge_chunky_stripped_tiff()).unwrap();

        let mut reader = TiffReader::new();
        reader.set_id(&path).unwrap();
        let err = reader.open_bytes_region(0, 0, 0, 1, 1).unwrap_err();

        let _ = fs::remove_file(&path);

        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("too large") || message.contains("overflows")),
            "unexpected error: {err:?}"
        );
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

    fn push_ifd_rational(data: &mut Vec<u8>, tag: u16, count: u32, value_offset: u32) {
        push_u16_le(data, tag);
        push_u16_le(data, 5); // RATIONAL
        push_u32_le(data, count);
        push_u32_le(data, value_offset);
    }

    /// Build a Canon-style EXIF maker-note blob carrying a rational
    /// `WHITE_BALANCE_RGB_COEFFS` (tag 16385) with the given r/g/b values.
    ///
    /// We first construct the *reconstructed* self-contained little-endian TIFF
    /// `buf` exactly as `parse_canon_maker_note` expects (header at offset 0,
    /// first IFD at offset 8), choosing the Canon relocation `offset == 8`. With
    /// `offset == 8`, the inverse transform is: blob = buf[8..] ++ buf[0..8],
    /// and the last 4 bytes of buf[0..8] already encode the IFD offset 8.
    fn synthetic_canon_maker_note(r: (u32, u32), g: (u32, u32), b: (u32, u32)) -> Vec<u8> {
        const WB: u16 = 16385;
        // Reconstructed TIFF buffer `buf`.
        let mut buf = Vec::new();
        buf.extend_from_slice(b"II");
        push_u16_le(&mut buf, 42);
        push_u32_le(&mut buf, 8); // first IFD at offset 8
                                  // IFD at offset 8: one entry, rational[3] stored out-of-line.
        push_u16_le(&mut buf, 1); // entry count
                                  // Entry table is 2 + 12 + 4 = 18 bytes -> rational data starts at 8+18=26.
        let rational_offset = 26u32;
        push_ifd_rational(&mut buf, WB, 3, rational_offset);
        push_u32_le(&mut buf, 0); // next IFD
                                  // Rational data (3 x (num, den)).
        for (n, d) in [r, g, b] {
            push_u32_le(&mut buf, n);
            push_u32_le(&mut buf, d);
        }

        // Inverse of Java's relocation with offset == 8: header(8) goes to the
        // blob tail, body (buf[8..]) goes to the blob head.
        let mut blob = Vec::new();
        blob.extend_from_slice(&buf[8..]);
        blob.extend_from_slice(&buf[0..8]);
        blob
    }

    #[test]
    fn parse_canon_maker_note_reads_white_balance_rational() {
        let blob = synthetic_canon_maker_note((2u32, 1), (3u32, 4), (5u32, 2));
        let note = parse_canon_maker_note(&blob, true).expect("maker-note should parse");
        let coeffs = note.get_vec_f64(16385);
        assert_eq!(coeffs.len(), 3);
        assert!((coeffs[0] - 2.0).abs() < 1e-9);
        assert!((coeffs[1] - 0.75).abs() < 1e-9);
        assert!((coeffs[2] - 2.5).abs() < 1e-9);
        assert!(note.is_rational(16385));
    }

    #[test]
    fn dng_white_balance_extracts_coeffs_from_exif_maker_note() {
        // Full synthetic DNG: main IFD -> EXIF sub-IFD (34665) -> MAKER_NOTE
        // (37500) -> Canon white-balance rational.
        let maker_note = synthetic_canon_maker_note((2u32, 1), (3u32, 4), (5u32, 2));

        let main_ifd_offset = 8u32;
        // Main IFD carries a valid (tiny) image plus the EXIF pointer so that
        // set_id succeeds; the white-balance read does not depend on pixels.
        let main_entry_count = 9u16;
        let main_ifd_bytes = 2 + main_entry_count as u32 * 12 + 4;
        let exif_ifd_offset = main_ifd_offset + main_ifd_bytes;
        let exif_entry_count = 1u16;
        let exif_ifd_bytes = 2 + exif_entry_count as u32 * 12 + 4;
        let maker_note_offset = exif_ifd_offset + exif_ifd_bytes;
        let strip_offset = maker_note_offset + maker_note.len() as u32;
        let pixels = [0u8, 1, 2, 3]; // 2x2 grayscale UINT8

        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        push_u16_le(&mut data, 42);
        push_u32_le(&mut data, main_ifd_offset);

        // Main IFD: minimal image + EXIF pointer.
        push_u16_le(&mut data, main_entry_count);
        push_ifd_long(&mut data, tag::IMAGE_WIDTH, 2);
        push_ifd_long(&mut data, tag::IMAGE_LENGTH, 2);
        push_ifd_short(&mut data, tag::BITS_PER_SAMPLE, 8);
        push_ifd_short(&mut data, tag::COMPRESSION, 1);
        push_ifd_short(&mut data, tag::PHOTOMETRIC_INTERPRETATION, 1);
        push_ifd_long(&mut data, tag::STRIP_OFFSETS, strip_offset);
        push_ifd_long(&mut data, tag::ROWS_PER_STRIP, 2);
        push_ifd_long(&mut data, tag::STRIP_BYTE_COUNTS, pixels.len() as u32);
        push_ifd_long(
            &mut data,
            super::super::nikon::EXIF_IFD_TAG,
            exif_ifd_offset,
        );
        push_u32_le(&mut data, 0);

        // EXIF IFD: just the MAKER_NOTE (undefined).
        push_u16_le(&mut data, exif_entry_count);
        push_ifd_undefined(
            &mut data,
            super::super::nikon::EXIF_MAKER_NOTE_TAG,
            &maker_note,
            maker_note_offset,
        );
        push_u32_le(&mut data, 0);

        data.extend_from_slice(&maker_note);
        data.extend_from_slice(&pixels);

        let path =
            std::env::temp_dir().join(format!("bioformats-rs-dng-wb-{}.tif", std::process::id()));
        fs::write(&path, &data).unwrap();

        let mut reader = TiffReader::new();
        reader.set_id(&path).unwrap();
        let wb = reader.dng_white_balance();
        let _ = fs::remove_file(&path);

        let wb = wb.expect("white balance should be present");
        assert!((wb[0] - 2.0).abs() < 1e-9);
        assert!((wb[1] - 0.75).abs() < 1e-9);
        assert!((wb[2] - 2.5).abs() < 1e-9);
    }

    #[test]
    fn dng_white_balance_absent_returns_none() {
        // A plain TIFF with no EXIF pointer yields no white balance (no-op path).
        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        push_u16_le(&mut data, 42);
        push_u32_le(&mut data, 8);
        push_u16_le(&mut data, 4);
        push_ifd_long(&mut data, tag::IMAGE_WIDTH, 2);
        push_ifd_long(&mut data, tag::IMAGE_LENGTH, 2);
        push_ifd_short(&mut data, tag::BITS_PER_SAMPLE, 8);
        push_ifd_short(&mut data, tag::PHOTOMETRIC_INTERPRETATION, 1);
        push_u32_le(&mut data, 0);

        let path = std::env::temp_dir().join(format!(
            "bioformats-rs-dng-no-wb-{}.tif",
            std::process::id()
        ));
        fs::write(&path, &data).unwrap();
        let mut reader = TiffReader::new();
        let _ = reader.set_id(&path);
        let wb = reader.dng_white_balance();
        let _ = fs::remove_file(&path);
        assert!(wb.is_none());
    }

    #[test]
    fn imagej_comment_populates_standard_metadata() {
        use crate::common::metadata::MetadataValue;

        let comment = "ImageJ=1.53c\nimages=10\nslices=5\nframes=2\nunit=micron\n\
                       spacing=-0.5\nfinterval=1.25\nxorigin=12\nyorigin=34\nmode=color";
        let mut out: HashMap<String, MetadataValue> = HashMap::new();

        assert!(check_comment_imagej(comment));
        parse_comment_imagej(comment, &mut out);

        let str_val = |out: &HashMap<String, MetadataValue>, k: &str| match out.get(k) {
            Some(MetadataValue::String(s)) => s.clone(),
            other => panic!("expected String for {k}, got {other:?}"),
        };
        let float_val = |out: &HashMap<String, MetadataValue>, k: &str| match out.get(k) {
            Some(MetadataValue::Float(v)) => *v,
            other => panic!("expected Float for {k}, got {other:?}"),
        };
        let int_val = |out: &HashMap<String, MetadataValue>, k: &str| match out.get(k) {
            Some(MetadataValue::Int(v)) => *v,
            other => panic!("expected Int for {k}, got {other:?}"),
        };

        assert_eq!(str_val(&out, "ImageJ"), "1.53c");
        assert_eq!(str_val(&out, "Unit"), "micron");
        assert_eq!(float_val(&out, "Spacing"), -0.5);
        // PhysicalSizeZ uses the absolute value (Java populateMetadataStoreImageJ).
        assert_eq!(float_val(&out, "PhysicalSizeZ"), 0.5);
        assert_eq!(float_val(&out, "Frame Interval"), 1.25);
        assert_eq!(float_val(&out, "TimeIncrement"), 1.25);
        assert_eq!(int_val(&out, "X Origin"), 12);
        assert_eq!(int_val(&out, "Y Origin"), 34);
        assert_eq!(str_val(&out, "Color mode"), "color");
        // Generic key=value tokens are preserved as original metadata.
        assert_eq!(str_val(&out, "images"), "10");
        // description joins the comment lines with "; " (Java initMetadataStore).
        let desc = str_val(&out, "description");
        assert!(desc.starts_with("ImageJ=1.53c; "));
        assert!(!desc.contains('\n'));
    }

    #[test]
    fn non_imagej_comment_is_not_treated_as_imagej() {
        assert!(!check_comment_imagej("MetaMorph foo"));
        assert!(!check_comment_imagej("<OME ...>"));
    }
}
