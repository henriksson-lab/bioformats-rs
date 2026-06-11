//! Zeiss LSM format reader (confocal laser scanning microscopy).
//!
//! LSM files are TIFF-based with a proprietary CZ_LSMInfo block (tag 34412).
//! The CZ_LSMInfo block provides the true Z/C/T dimensions.
//! Every other IFD is a thumbnail; only even-indexed IFDs contain full-res data.

use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::tiff::ifd::{tag, Compression, IfdValue};
use crate::tiff::parser::TiffParser;
use crate::tiff::TiffReader;

// ── Tag IDs ───────────────────────────────────────────────────────────────────
const CZ_LSM_INFO: u16 = 34412;

// ── CZ_LSMInfo block (partial) ────────────────────────────────────────────────
// Only the fields we actually need:
//   offset 0:  MagicNumber (int32) = 0x00300494
//   offset 4:  StructureSize (int32)
//   offset 8:  DimensionX (int32)
//   offset 12: DimensionY (int32)
//   offset 16: DimensionZ (int32)
//   offset 20: DimensionChannels (int32)
//   offset 24: DimensionTime (int32)
//   offset 28: DataType (int32) -> 1=uint8, 2=uint12, 5=float32
//   offset 32: ThumbnailX (int32)
//   offset 36: ThumbnailY (int32)
//   offset 40: VoxelSizeX (float64)
//   offset 48: VoxelSizeY (float64)
//   offset 56: VoxelSizeZ (float64)
// Known CZ_LSMInfo magic numbers. ZeissLSMReader.java does not gate on the
// magic value at all (it only records it as metadata), so we accept both
// documented variants and do not hard-fail on others.
const LSM_MAGIC: u32 = 0x0030_0494;
const LSM_MAGIC_ALT: u32 = 0x0040_0494;

#[derive(Debug, Default)]
struct LsmInfo {
    dim_z: u32,
    dim_c: u32,
    dim_t: u32,
    data_type: i32,
    /// CZ-LSMINFO ScanType (short at offset 88); selects the dimension order.
    scan_type: i16,
    voxel_x: f64,
    voxel_y: f64,
    voxel_z: f64,
    /// CZ-LSMINFO OffsetChannelColors (int at offset 108): absolute file offset
    /// of the channel-colours/-names sub-block, or 0 when absent.
    channel_colors_offset: u32,
    /// CZ-LSMINFO TimeInterval (double at offset 112); seconds between frames.
    time_interval: f64,
    /// Per-channel names parsed from the channel-colours sub-block (Java
    /// ZeissLSMReader.java:1162-1181).
    channel_names: Vec<String>,
}

fn checked_plane_count(size_z: u32, size_c: u32, size_t: u32) -> Result<u32> {
    size_z
        .checked_mul(size_c)
        .and_then(|v| v.checked_mul(size_t))
        .ok_or_else(|| BioFormatsError::Format("LSM: plane count overflow".into()))
}

fn resolve_lsm_plane_index(
    plane_index: u32,
    logical_count: u32,
    physical_count: u32,
) -> Result<u32> {
    if plane_index >= logical_count || plane_index >= physical_count {
        return Err(BioFormatsError::PlaneOutOfRange(plane_index));
    }
    Ok(plane_index)
}

fn read_i32_lsm(buf: &[u8], off: usize, le: bool) -> i32 {
    let b = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
    if le {
        i32::from_le_bytes(b)
    } else {
        i32::from_be_bytes(b)
    }
}
fn read_i16_lsm(buf: &[u8], off: usize, le: bool) -> i16 {
    let b = [buf[off], buf[off + 1]];
    if le {
        i16::from_le_bytes(b)
    } else {
        i16::from_be_bytes(b)
    }
}
fn read_f64_lsm(buf: &[u8], off: usize, le: bool) -> f64 {
    let b: [u8; 8] = buf[off..off + 8].try_into().unwrap_or([0u8; 8]);
    if le {
        f64::from_le_bytes(b)
    } else {
        f64::from_be_bytes(b)
    }
}

fn parse_lsm_info(bytes: &[u8], le: bool) -> Result<LsmInfo> {
    if bytes.len() < 64 {
        return Err(BioFormatsError::Format(
            "LSM: CZ_LSMInfo block is shorter than 64 bytes".into(),
        ));
    }
    // ZeissLSMReader.java never rejects based on the magic number; it only
    // records it. We mirror that: accept the documented magics (0x00300494 and
    // 0x00400494) and, for any other value, only emit a debug-level note rather
    // than failing to parse the block.
    let magic = read_i32_lsm(bytes, 0, le) as u32;
    if magic != LSM_MAGIC && magic != LSM_MAGIC_ALT {
        // Not a hard error: continue parsing dimensions like Java does.
    }

    let dim_z = read_i32_lsm(bytes, 16, le);
    // ZeissLSMReader.java:773-777 reads sizeZ (offset 16), SKIPS the channel
    // field (offset 20), then reads sizeT (offset 24). sizeC is taken from the
    // TIFF, not from this struct, so the offset-20 channel count is read here
    // only for the validity check (Java does not use it for sizeC).
    let dim_c = read_i32_lsm(bytes, 20, le);
    let dim_t = read_i32_lsm(bytes, 24, le);
    if dim_z <= 0 || dim_c <= 0 || dim_t <= 0 {
        return Err(BioFormatsError::Format(format!(
            "LSM: invalid non-positive dimensions Z={dim_z} C={dim_c} T={dim_t}"
        )));
    }

    Ok(LsmInfo {
        dim_z: dim_z as u32,
        dim_c: dim_c as u32,
        dim_t: dim_t as u32,
        data_type: read_i32_lsm(bytes, 28, le),
        // ZeissLSMReader.java:822-824 seeks to offset 88 and reads a short for
        // ScanType. Missing/short blocks fall back to 0 (-> XYZCT), matching the
        // Java default case.
        scan_type: if bytes.len() >= 90 {
            read_i16_lsm(bytes, 88, le)
        } else {
            0
        },
        voxel_x: if bytes.len() >= 48 {
            read_f64_lsm(bytes, 40, le)
        } else {
            0.0
        },
        voxel_y: if bytes.len() >= 56 {
            read_f64_lsm(bytes, 48, le)
        } else {
            0.0
        },
        voxel_z: if bytes.len() >= 64 {
            read_f64_lsm(bytes, 56, le)
        } else {
            0.0
        },
        // ZeissLSMReader.java:952 reads OffsetChannelColors and java:954 reads
        // TimeInterval. After seek(88) the field order is: scanType(2),
        // spectralScan(2), type(4), overlay[0..2](12) -> offset 108 holds
        // channelColorsOffset, offset 112 holds TimeInterval(double).
        channel_colors_offset: if bytes.len() >= 112 {
            read_i32_lsm(bytes, 108, le) as u32
        } else {
            0
        },
        time_interval: if bytes.len() >= 120 {
            read_f64_lsm(bytes, 112, le)
        } else {
            0.0
        },
        channel_names: Vec::new(),
    })
}

/// Parses the per-channel names from the channel-colours sub-block, mirroring
/// ZeissLSMReader.java:1112-1182. `colors_offset`/`names_offset` are relative to
/// `channel_colors_offset`; the name table is a sequence of (int length, bytes)
/// records with trailing NULs stripped.
fn parse_channel_names(
    file_bytes: &[u8],
    channel_colors_offset: u32,
    size_c: u32,
    le: bool,
) -> Vec<String> {
    let mut names = Vec::new();
    if channel_colors_offset == 0 {
        return names;
    }
    let base = channel_colors_offset as usize;
    // Need at least the two offset ints at base+12 and base+16.
    if base + 20 > file_bytes.len() {
        return names;
    }
    let names_offset = read_i32_lsm(file_bytes, base + 16, le);
    if names_offset <= 0 {
        return names;
    }
    let mut p = base + names_offset as usize;
    for _ in 0..size_c {
        if p + 4 > file_bytes.len() {
            break;
        }
        let length = read_i32_lsm(file_bytes, p, le);
        p += 4;
        if length < 0 {
            break;
        }
        let length = length as usize;
        if p + length > file_bytes.len() {
            break;
        }
        let raw = &file_bytes[p..p + length];
        p += length;
        let trimmed = raw.split(|&b| b == 0).next().unwrap_or(&[]);
        names.push(String::from_utf8_lossy(trimmed).into_owned());
    }
    names
}

/// Maps the CZ-LSMINFO ScanType to a dimension order, mirroring
/// ZeissLSMReader.java:824-885.
///
/// Base switch (java:825-873):
///   3 / 5 / 9 -> XYTCZ   (time series x-y / Mean of ROIs / time series spline x-z)
///   4 / 6     -> XYZTC   (time series x-z / time series x-y-z)
///   7         -> XYCTZ   (spline scan)
///   8         -> XYCZT   (spline scan x-z)
///   0,1,2,10,default -> XYZCT
///
/// When the image is RGB (java:881-885), C is shuffled to the front: "C" is
/// removed from the order then re-inserted right after "XY", i.e. the result is
/// always "XYC" + the remaining two axes.
fn lsm_dimension_order(scan_type: i16, is_rgb: bool) -> DimensionOrder {
    let base = match scan_type {
        3 | 5 | 9 => DimensionOrder::XYTCZ,
        4 | 6 => DimensionOrder::XYZTC,
        7 => DimensionOrder::XYCTZ,
        8 => DimensionOrder::XYCZT,
        // 0, 1, 2, 10 and any other value -> XYZCT
        _ => DimensionOrder::XYZCT,
    };
    if !is_rgb {
        return base;
    }
    // Shuffle C to the front (after XY), preserving the relative order of the
    // remaining Z/T axes. base never already has C right after XY here.
    match base {
        // XYTCZ -> XYTZ -> XYCTZ
        DimensionOrder::XYTCZ => DimensionOrder::XYCTZ,
        // XYZTC -> XYZT -> XYCZT
        DimensionOrder::XYZTC => DimensionOrder::XYCZT,
        // XYCTZ -> XYTZ -> XYCTZ (unchanged)
        DimensionOrder::XYCTZ => DimensionOrder::XYCTZ,
        // XYCZT -> XYZT -> XYCZT (unchanged)
        DimensionOrder::XYCZT => DimensionOrder::XYCZT,
        // XYZCT -> XYZT -> XYCZT
        DimensionOrder::XYZCT => DimensionOrder::XYCZT,
        DimensionOrder::XYTZC => DimensionOrder::XYCTZ,
    }
}

fn lsm_pixel_type(data_type: i32, tiff_bps: u16) -> Result<PixelType> {
    // data_type follows ZeissLSMReader: 1=uint8, 2=12-bit stored as uint16,
    // 5=32-bit float.
    match data_type {
        1 => Ok(PixelType::Uint8),
        2 => Ok(PixelType::Uint16),
        5 => Ok(PixelType::Float32),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "LSM: unsupported CZ_LSMInfo DataType {other} (TIFF bits/sample {tiff_bps})"
        ))),
    }
}

// ── Minimal TIFF IFD reader for fetching CZ_LSMInfo bytes ────────────────────
fn read_lsm_info_from_file(path: &Path) -> Result<(LsmInfo, bool)> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let buf = BufReader::new(f);
    let mut parser = TiffParser::new(buf)?;
    let le = parser.little_endian;
    let (ifd, _) = parser.read_ifd(parser.first_ifd_offset)?;

    // Find CZ_LSMInfo tag
    let lsm_bytes = match ifd.get(CZ_LSM_INFO) {
        Some(IfdValue::Byte(b)) => b.clone(),
        Some(IfdValue::Undefined(b)) => b.clone(),
        _ => {
            return Err(BioFormatsError::Format(
                "LSM: CZ_LSMInfo tag (34412) not found in first IFD".into(),
            ))
        }
    };

    let mut info = parse_lsm_info(&lsm_bytes, le)?;

    // Channel names live in the channel-colours sub-block, addressed by an
    // absolute file offset stored in the CZ-LSMINFO struct. Read the whole file
    // once to resolve it (these files are small) and parse the name table.
    if info.channel_colors_offset != 0 {
        if let Ok(file_bytes) = std::fs::read(path) {
            info.channel_names =
                parse_channel_names(&file_bytes, info.channel_colors_offset, info.dim_c, le);
        }
    }

    Ok((info, le))
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct LsmReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Inner TIFF reader handles pixel I/O; we select the correct series.
    inner: TiffReader,
    /// When true, one physical IFD packs all `size_c` channels in planar order
    /// and a logical plane maps to (ifd = plane / sizeC, channel = plane % sizeC),
    /// with the channel sliced out (Java splitPlanes path).
    split_planes: bool,
    /// Per-channel names parsed from the CZ-LSMINFO channel-colours sub-block.
    channel_names: Vec<String>,
    /// OME image name (file stem), mirroring Java's getLSMFileFromSeries name.
    image_name: Option<String>,
}

impl LsmReader {
    pub fn new() -> Self {
        LsmReader {
            path: None,
            meta: None,
            inner: TiffReader::new(),
            split_planes: false,
            channel_names: Vec::new(),
            image_name: None,
        }
    }

    fn collect_full_resolution_ifds(&self, best_series: usize) -> Vec<usize> {
        let series = self.inner.series_list();
        let Some(target) = series.get(best_series).map(|s| &s.metadata) else {
            return Vec::new();
        };

        series
            .iter()
            .filter(|s| {
                let meta = &s.metadata;
                meta.size_x == target.size_x
                    && meta.size_y == target.size_y
                    && meta.size_c == target.size_c
                    && meta.bits_per_pixel == target.bits_per_pixel
                    && meta.pixel_type == target.pixel_type
                    && meta.is_rgb == target.is_rgb
                    && meta.is_interleaved == target.is_interleaved
            })
            .flat_map(|s| s.ifd_indices.iter().copied())
            .collect()
    }

    fn configure_full_resolution_series(&mut self, best_series: usize) -> u32 {
        let full_res_ifds = self.collect_full_resolution_ifds(best_series);
        let full_res_ifd_count = full_res_ifds.len() as u32;
        if !full_res_ifds.is_empty() {
            if let Some(series) = self.inner.series_list_mut().get_mut(best_series) {
                series.ifd_indices = full_res_ifds;
                series.plane_ifd_indices.clear();
                series.metadata.image_count = full_res_ifd_count;
                series.metadata.size_z = full_res_ifd_count;
            }
        }
        full_res_ifd_count
    }
}

impl Default for LsmReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LsmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lsm"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // LSM files are TIFF; we rely on extension detection since the TIFF
        // reader also matches magic bytes.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        // First, read the CZ_LSMInfo block to get true dimensions
        let (lsm_info, le) = read_lsm_info_from_file(path)?;

        // Open with inner TIFF reader to get pixel dimensions and read pixel data
        self.inner.set_id(path)?;

        // ZeissLSMReader.java:544-548 — many .lsm files carry a stray
        // PREDICTOR=2 tag on IFDs whose compression is NOT LZW; the predictor
        // must only be honoured for LZW data. Force PREDICTOR=1 on every IFD
        // that is not LZW-compressed, after the inner reader has parsed the
        // IFDs and before any pixel read (read_plane_bytes re-derives the
        // predictor from the live IFD, so this mutation takes effect).
        let ifd_count = self.inner.ifd_count();
        for i in 0..ifd_count {
            if let Some(ifd) = self.inner.ifd_mut(i) {
                if ifd.compression() != Compression::Lzw {
                    ifd.entries.insert(tag::PREDICTOR, IfdValue::Short(vec![1]));
                }
            }
        }

        // The TIFF reader may have multiple series (full-res + thumbnails).
        // Select the series with the largest images.
        let n_series = self.inner.series_count();
        let mut best_series = 0usize;
        let mut best_pixels = 0u64;
        for s in 0..n_series {
            let _ = self.inner.set_series(s);
            let m = self.inner.metadata();
            let px = m.size_x as u64 * m.size_y as u64;
            if px > best_pixels {
                best_pixels = px;
                best_series = s;
            }
        }
        let _ = self.inner.set_series(best_series);
        // Capture the first full-resolution IFD index *before* the series is
        // reconfigured, so we can inspect its SamplesPerPixel.
        let first_full_res_ifd = self
            .collect_full_resolution_ifds(best_series)
            .first()
            .copied();
        let full_res_ifd_count = self.configure_full_resolution_series(best_series);
        let tiff_meta = self.inner.metadata().clone();

        // ZeissLSMReader.java:720,725 — sizeC/rgb derive from the full-res IFD's
        // SamplesPerPixel. When a single IFD carries more than one sample (planar
        // multi-channel, e.g. SamplesPerPixel=2, PlanarConfiguration=2), every
        // physical IFD holds *all* channels and Java splits them into separate
        // planes (splitPlanes path, java:410-428, 988-992). Otherwise the file
        // stores one channel per IFD.
        let samples_per_ifd = first_full_res_ifd
            .and_then(|i| self.inner.ifd(i))
            .map(|ifd| ifd.samples_per_pixel())
            .unwrap_or(1);

        // Build corrected metadata using LSM dimensions.
        //
        // sizeC comes from the CZ-LSMINFO channel field (offset 20). There are
        // two physical layouts (see `samples_per_ifd` above):
        //
        //   * packed (samples_per_ifd > 1): one IFD per Z/T plane carries all C
        //     channels in planar order. full_res_ifd_count == Z*T. Java splits
        //     these into C logical planes (splitPlanes), so imageCount = Z*C*T.
        //     We slice the requested channel out of the planar IFD in
        //     open_bytes_region.
        //   * separate (samples_per_ifd == 1): one IFD per channel. We expose
        //     each IFD as a logical plane directly. imageCount = ifd count.
        let dim_z = lsm_info.dim_z;
        let dim_c = lsm_info.dim_c;
        let dim_t = lsm_info.dim_t;

        // A planar/packed multichannel LSM: SamplesPerPixel>1 on the full-res
        // IFD and exactly one IFD per Z/T plane (java:410 condition
        // `ifds.size() == sizeZ * sizeT`).
        let split_planes = samples_per_ifd > 1
            && dim_c > 1
            && checked_plane_count(dim_z, 1, dim_t).ok() == Some(full_res_ifd_count);

        let image_count = if split_planes {
            checked_plane_count(dim_z, dim_c, dim_t)?
        } else {
            full_res_ifd_count
        };

        let pixel_type = lsm_pixel_type(lsm_info.data_type, tiff_meta.bits_per_pixel as u16)?;
        // ZeissLSMReader sets rgb=samples>1 to drive the dimension-order shuffle,
        // but always flattens rgb back to false once channels are split / the
        // image is indexed (java:877, 990). We never expose LSM as packed RGB.
        let rgb_for_order = samples_per_ifd > 1;
        let is_rgb = false;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert(
            "voxel_size_x_um".into(),
            MetadataValue::Float(lsm_info.voxel_x * 1e6),
        );
        meta_map.insert(
            "voxel_size_y_um".into(),
            MetadataValue::Float(lsm_info.voxel_y * 1e6),
        );
        meta_map.insert(
            "voxel_size_z_um".into(),
            MetadataValue::Float(lsm_info.voxel_z * 1e6),
        );
        // ZeissLSMReader.java:954 records TimeInterval; surfaced as the OME
        // TimeIncrement (seconds).
        if lsm_info.time_interval != 0.0 {
            meta_map.insert(
                "time_increment_s".into(),
                MetadataValue::Float(lsm_info.time_interval),
            );
        }

        // OME image name: Java uses the LSM file path; ImageReader/OME later
        // reduce it to the file's base name. Use the file stem to match.
        let image_name = path
            .file_stem()
            .and_then(|s| s.to_str())
            .map(|s| s.to_string());

        // Java sets indexed=true whenever a channel-colours sub-block (LUT) is
        // present (java:1122). It carries one per channel here.
        let is_indexed = lsm_info.channel_colors_offset != 0;

        let meta = ImageMetadata {
            size_x: tiff_meta.size_x,
            size_y: tiff_meta.size_y,
            size_z: dim_z,
            size_c: dim_c,
            size_t: dim_t,
            pixel_type,
            bits_per_pixel: tiff_meta.bits_per_pixel,
            image_count,
            dimension_order: lsm_dimension_order(lsm_info.scan_type, rgb_for_order),
            is_rgb,
            is_interleaved: tiff_meta.is_interleaved,
            is_indexed,
            is_little_endian: le,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        self.split_planes = split_planes;
        self.channel_names = lsm_info.channel_names;
        self.image_name = image_name;
        self.meta = Some(meta);
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.split_planes = false;
        self.channel_names = Vec::new();
        self.image_name = None;
        let _ = self.inner.close();
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() || s != 0 {
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
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (w, h) = (meta.size_x, meta.size_y);
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
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let inner_count = self.inner.metadata().image_count;

        if self.split_planes {
            // One physical IFD packs all channels in planar order. Map the
            // logical plane to (ifd, channel) and slice the channel out, mirroring
            // ZeissLSMReader.java:410-428 (getSamples + ImageTools.splitChannels,
            // non-interleaved). dimensionOrder is XY C..., so C is the fastest
            // axis: ifd = no / sizeC, channel = no % sizeC.
            let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            let size_c = meta.size_c.max(1);
            let bpp = meta.pixel_type.bytes_per_sample();
            let physical = plane_index / size_c;
            let channel = (plane_index % size_c) as usize;
            if physical >= inner_count {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index));
            }
            let packed = self.inner.open_bytes_region(physical, x, y, w, h)?;
            let chan_len = (w as usize) * (h as usize) * bpp;
            let start = chan_len * channel;
            let end = start + chan_len;
            if end > packed.len() {
                return Err(BioFormatsError::Format(format!(
                    "LSM: split-channel slice {start}..{end} exceeds plane length {}",
                    packed.len()
                )));
            }
            return Ok(packed[start..end].to_vec());
        }

        let inner_idx = resolve_lsm_plane_index(plane_index, count, inner_count)?;
        self.inner.open_bytes_region(inner_idx, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::metadata::MetadataValue;
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = &mut ome.images[0];
        let get_f = |k: &str| -> Option<f64> {
            if let Some(MetadataValue::Float(v)) = meta.series_metadata.get(k) {
                Some(*v)
            } else {
                None
            }
        };
        // Already stored in µm
        img.physical_size_x = get_f("voxel_size_x_um");
        img.physical_size_y = get_f("voxel_size_y_um");
        img.physical_size_z = get_f("voxel_size_z_um");
        img.time_increment = get_f("time_increment_s");
        img.name = self.image_name.clone();
        // Channel names from the CZ-LSMINFO channel-colours sub-block
        // (ZeissLSMReader.java:1351 store.setChannelName).
        for (ci, name) in self.channel_names.iter().enumerate() {
            if let Some(ch) = img.channels.get_mut(ci) {
                if !name.is_empty() {
                    ch.name = Some(name.clone());
                }
            }
        }
        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lsm_plane_mapping_rejects_logical_planes_without_physical_ifds() {
        assert_eq!(resolve_lsm_plane_index(0, 3, 2).unwrap(), 0);
        assert_eq!(resolve_lsm_plane_index(1, 3, 2).unwrap(), 1);
        assert!(matches!(
            resolve_lsm_plane_index(2, 3, 2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));
    }

    #[test]
    fn lsm_plane_mapping_rejects_planes_past_logical_count() {
        assert!(matches!(
            resolve_lsm_plane_index(2, 2, 4),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));
    }

    #[test]
    fn lsm_split_plane_image_count_multiplies_by_channels() {
        // Packed multichannel: one IFD per Z/T plane carries all channels, so
        // the logical plane count is Z*C*T.
        assert_eq!(checked_plane_count(33, 2, 1).unwrap(), 66);
        assert_eq!(checked_plane_count(2, 3, 4).unwrap(), 24);
    }

    #[test]
    fn lsm_dimension_order_shuffles_c_when_packed() {
        // scanType 0 -> XYZCT, RGB-style shuffle moves C to front -> XYCZT.
        assert_eq!(lsm_dimension_order(0, false), DimensionOrder::XYZCT);
        assert_eq!(lsm_dimension_order(0, true), DimensionOrder::XYCZT);
    }

    #[test]
    fn lsm_parse_channel_names_reads_length_prefixed_table() {
        let le = true;
        // channel-colours sub-block at file offset 4. Layout:
        //   +12 colorsOffset (int), +16 namesOffset (int)
        // names table at offset 4 + namesOffset.
        let names_offset: i32 = 24;
        let mut buf = vec![0u8; 4]; // 0..4 header padding (base = 4)
        buf.resize(4 + names_offset as usize, 0); // fill up to the names table
                                                  // +12 colorsOffset, +16 namesOffset relative to base=4
        buf[4 + 12..4 + 16].copy_from_slice(&0i32.to_le_bytes());
        buf[4 + 16..4 + 20].copy_from_slice(&names_offset.to_le_bytes());
        // names table at base + names_offset = index 28
        for name in ["Ch2-T1\0", "Ch1-T2\0"] {
            buf.extend_from_slice(&(name.len() as i32).to_le_bytes());
            buf.extend_from_slice(name.as_bytes());
        }
        let names = parse_channel_names(&buf, 4, 2, le);
        assert_eq!(names, vec!["Ch2-T1".to_string(), "Ch1-T2".to_string()]);
    }
}
