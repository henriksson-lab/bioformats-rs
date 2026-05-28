//! bioformats-mias — format readers:
//!
//! - CellWorxReader: CellWorX HCS (.htd / .pnl)
//! - Al3dReader: 3D image format (.al3d) with "AL3D" magic
//! - OxfordInstrumentsReader: Oxford Instruments SEM/AFM (.top)
//! - FeiSerReader: FEI SER electron-microscopy series (.ser)

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ── Helpers ───────────────────────────────────────────────────────────────────

fn simple_meta(w: u32, h: u32, z: u32, pt: PixelType) -> ImageMetadata {
    let bps = pt.bytes_per_sample();
    ImageMetadata {
        size_x: w,
        size_y: h,
        size_z: z,
        size_c: 1,
        size_t: 1,
        pixel_type: pt,
        bits_per_pixel: (bps * 8) as u8,
        image_count: z,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    }
}

fn checked_payload_len(meta: &ImageMetadata) -> Result<u64> {
    let bps = meta.pixel_type.bytes_per_sample() as u64;
    (meta.size_x as u64)
        .checked_mul(meta.size_y as u64)
        .and_then(|px| px.checked_mul(bps))
        .and_then(|plane| plane.checked_mul(meta.image_count as u64))
        .ok_or_else(|| BioFormatsError::Format("declared image payload size overflows".into()))
}

// ── CellWorxReader ────────────────────────────────────────────────────────────

pub struct CellWorxReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl CellWorxReader {
    pub fn new() -> Self {
        CellWorxReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for CellWorxReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_htd(path: &Path) -> Result<ImageMetadata> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let mut x_sites = 1u32;
    let mut y_sites = 1u32;
    let mut timepoints = 1u32;
    let mut z_steps = 1u32;
    let mut wavelengths = 1u32;

    for line in content.lines() {
        let line = line.trim();
        if let Some(v) = htd_kv(line, "XSites") {
            if let Ok(n) = v.parse() {
                x_sites = n;
            }
        } else if let Some(v) = htd_kv(line, "YSites") {
            if let Ok(n) = v.parse() {
                y_sites = n;
            }
        } else if let Some(v) = htd_kv(line, "TimePoints") {
            if let Ok(n) = v.parse() {
                timepoints = n;
            }
        } else if let Some(v) = htd_kv(line, "ZSteps") {
            if let Ok(n) = v.parse() {
                z_steps = n;
            }
        } else if let Some(v) = htd_kv(line, "Wavelengths") {
            if let Ok(n) = v.parse() {
                wavelengths = n;
            }
        }
    }

    let image_count = x_sites * y_sites * timepoints * z_steps * wavelengths;
    let image_count = image_count.max(1);
    Ok(simple_meta(512, 512, image_count, PixelType::Uint16))
}

fn htd_kv<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let stripped = line.strip_prefix(key)?.trim_start();
    Some(stripped.strip_prefix(',')?.trim_start())
}

impl FormatReader for CellWorxReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("htd") | Some("pnl"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // If .pnl, look for companion .htd
        let cfg_path = if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pnl"))
            .unwrap_or(false)
        {
            path.with_extension("htd")
        } else {
            path.to_path_buf()
        };

        if cfg_path.exists() {
            let _ = parse_htd(&cfg_path)?;
        }
        Err(BioFormatsError::UnsupportedFormat(
            "CellWorX HTD/PNL companion image decoding is not implemented".to_string(),
        ))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        0
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        Err(BioFormatsError::SeriesOutOfRange(s))
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Err(BioFormatsError::UnsupportedFormat(
            "CellWorX HTD/PNL companion image decoding is not implemented".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }
}

// ── Al3dReader ────────────────────────────────────────────────────────────────

const AL3D_MAGIC: &[u8] = b"AL3D";
const AL3D_DATA_OFFSET: u64 = 512;

pub struct Al3dReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl Al3dReader {
    pub fn new() -> Self {
        Al3dReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for Al3dReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_al3d(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < AL3D_DATA_OFFSET as usize {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file too short for declared header offset".into(),
        ));
    }
    if &data[..4] != AL3D_MAGIC {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file is missing AL3D magic".into(),
        ));
    }
    // Offset 8: width (u32 LE), 12: height (u32 LE), 16: depth (u32 LE)
    let width = u32::from_le_bytes([data[8], data[9], data[10], data[11]]);
    let height = u32::from_le_bytes([data[12], data[13], data[14], data[15]]);
    let depth = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
    if width == 0 || height == 0 || depth == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "AL3D file has zero image dimensions".into(),
        ));
    }
    // Offset 20: data_type (u16 LE)
    let data_type = u16::from_le_bytes([data[20], data[21]]);
    let pixel_type = match data_type {
        0 => PixelType::Uint8,
        1 => PixelType::Uint16,
        2 => PixelType::Float32,
        _ => PixelType::Uint16,
    };
    let meta = simple_meta(width, height, depth, pixel_type);
    let required_len = AL3D_DATA_OFFSET
        .checked_add(checked_payload_len(&meta)?)
        .ok_or_else(|| BioFormatsError::Format("AL3D file size overflows".into()))?;
    if (data.len() as u64) < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "AL3D pixel payload is shorter than declared ({} < {required_len})",
            data.len()
        )));
    }
    Ok(meta)
}

impl FormatReader for Al3dReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("al3d"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4 && header[0..4] == *AL3D_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_al3d(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let plane_offset = AL3D_DATA_OFFSET + plane_index as u64 * plane_bytes as u64;
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(plane_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("AL3D", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ── FeiSerReader ──────────────────────────────────────────────────────────────

/// FEI SER format: electron-microscopy image series from TEM/STEM systems.
/// Magic: bytes 0-1 == 0x97 0x01 (series file signature).
pub struct FeiSerReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl FeiSerReader {
    pub fn new() -> Self {
        FeiSerReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for FeiSerReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_ser(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < 32 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FEI SER header is too short for safe image decoding".to_string(),
        ));
    }
    // Bytes 4-5: data type id (LE u16). 1=u8,2=u16,3=u32,4=i8,5=i16,6=i32,7=f32,8=f64
    let dtype = u16::from_le_bytes([data[4], data[5]]);
    // Bytes 8-11: total element count (LE u32) — number of frames
    let n_frames = u32::from_le_bytes([data[8], data[9], data[10], data[11]]).max(1);
    // Bytes 24-27: width, 28-31: height (LE u32 at those positions in the tag)
    let width = u32::from_le_bytes([data[24], data[25], data[26], data[27]]).max(1);
    let height = if data.len() >= 32 {
        u32::from_le_bytes([data[28], data[29], data[30], data[31]]).max(1)
    } else {
        512
    };
    let pixel_type = match dtype {
        1 => PixelType::Uint8,
        2 => PixelType::Uint16,
        3 | 6 => PixelType::Int32,
        7 => PixelType::Float32,
        8 => PixelType::Float64,
        _ => PixelType::Uint16,
    };
    let width = if width > 65535 { 512 } else { width };
    let height = if height > 65535 { 512 } else { height };
    Ok(simple_meta(width, height, n_frames, pixel_type))
}

impl FormatReader for FeiSerReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ser"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 2 && header[0] == 0x97 && header[1] == 0x01
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let _ = parse_ser(path)?;
        Err(BioFormatsError::UnsupportedFormat(
            "FEI SER payload decoding is not implemented".to_string(),
        ))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        0
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        Err(BioFormatsError::SeriesOutOfRange(s))
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Err(BioFormatsError::UnsupportedFormat(
            "FEI SER payload decoding is not implemented".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }
}

// ── OxfordInstrumentsReader ───────────────────────────────────────────────────

const OXFORD_DATA_OFFSET: u64 = 128;

pub struct OxfordInstrumentsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl OxfordInstrumentsReader {
    pub fn new() -> Self {
        OxfordInstrumentsReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for OxfordInstrumentsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_oxford(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < 12 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Oxford TOP header is too short for safe image decoding".to_string(),
        ));
    }
    // Offset 4: width (u16 LE), 6: height (u16 LE), 8: data_type (u16 LE)
    let width = u16::from_le_bytes([data[4], data[5]]) as u32;
    let height = u16::from_le_bytes([data[6], data[7]]) as u32;
    let dtype = u16::from_le_bytes([data[8], data[9]]);
    let pixel_type = match dtype {
        0 => PixelType::Uint8,
        1 => PixelType::Uint16,
        2 => PixelType::Float32,
        _ => PixelType::Uint16,
    };
    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Oxford TOP header is missing image dimensions".to_string(),
        ));
    }
    let meta = simple_meta(width, height, 1, pixel_type);
    let required_len = OXFORD_DATA_OFFSET
        .checked_add(checked_payload_len(&meta)?)
        .ok_or_else(|| BioFormatsError::Format("Oxford TOP file size overflows".into()))?;
    if (data.len() as u64) < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Oxford TOP pixel payload is shorter than declared ({} < {required_len})",
            data.len()
        )));
    }
    Ok(meta)
}

impl FormatReader for OxfordInstrumentsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("top"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_oxford(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        Ok(())
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(OXFORD_DATA_OFFSET))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Oxford Instruments", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ── MIASReader ────────────────────────────────────────────────────────────────
//
// MIAS (Maia Scientific) HCS reader, ported from the upstream Java MIASReader.
// A dataset is a directory hierarchy:
//
//   <experiment>/<plate>/Well<xxxx>/mode<c>_z<zzz>_t<ttt>_im<r>_<col>.tif
//
// Each TIFF contains a single grayscale plane.  The "mode" block is the
// channel, "z"/"t" are the Z section and timepoint, and "im<r>_<col>" gives the
// tile coordinates within a mosaic.  One series is produced per well.
//
// This implementation handles the common (non-tiled, single-plane-per-file)
// case faithfully; tiled mosaics fall back to reading the first tile.

/// Per-well TIFF planes plus the parsed dimension structure.
struct MiasWell {
    /// Sorted TIFF file paths (one plane each).
    tiffs: Vec<PathBuf>,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    well_number: i64,
}

pub struct MiasReader {
    wells: Vec<MiasWell>,
    series: Vec<ImageMetadata>,
    current_series: usize,
    tile_rows: u32,
    tile_cols: u32,
    tiff_reader: crate::tiff::TiffReader,
    tiff_loaded: bool,
}

impl MiasReader {
    pub fn new() -> Self {
        MiasReader {
            wells: Vec::new(),
            series: Vec::new(),
            current_series: 0,
            tile_rows: 1,
            tile_cols: 1,
            tiff_reader: crate::tiff::TiffReader::new(),
            tiff_loaded: false,
        }
    }
}

impl Default for MiasReader {
    fn default() -> Self {
        Self::new()
    }
}

fn is_mias_tiff(name: &str) -> bool {
    let l = name.to_ascii_lowercase();
    l.ends_with(".tif") || l.ends_with(".tiff")
}

/// Extract the integer following a `<prefix>` block in a MIAS filename, e.g.
/// `mode2_z003_t001_...` -> for prefix "z" returns Some(3).
fn mias_block(name: &str, prefix: &str) -> Option<i64> {
    let lname = name.to_ascii_lowercase();
    for part in lname.split('_') {
        if let Some(rest) = part.strip_prefix(prefix) {
            if !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()) {
                return rest.parse::<i64>().ok();
            }
        }
    }
    None
}

/// Extract the trailing tile-column index from a MIAS tile filename, e.g.
/// `mode2_z003_t001_im0_2.tif` -> the bare `2` block after `im<r>_` -> Some(2).
/// In the MIAS convention the last underscore-separated block before the
/// extension is the tile column (a bare integer with no alphabetic prefix).
fn mias_trailing_col(name: &str) -> Option<i64> {
    // Strip extension.
    let stem = name.rsplit_once('.').map(|(s, _)| s).unwrap_or(name);
    let last = stem.rsplit('_').next()?;
    if !last.is_empty() && last.chars().all(|c| c.is_ascii_digit()) {
        last.parse::<i64>().ok()
    } else {
        None
    }
}

/// Identify whether a directory name is a MIAS well directory.
fn is_well_dir_name(name: &str) -> bool {
    if name.starts_with("Well") {
        return true;
    }
    // Four-digit well directory in the alternate layout.
    name.len() == 4 && name.chars().all(|c| c.is_ascii_digit())
}

fn well_number_from_name(name: &str) -> i64 {
    let stripped = name.trim_start_matches("Well");
    stripped.trim().parse::<i64>().map(|v| v - 1).unwrap_or(0)
}

impl MiasReader {
    /// Locate the plate directory and enumerate well directories given a TIFF
    /// (or well directory) path inside a MIAS hierarchy.
    fn build(&mut self, id: &Path) -> Result<()> {
        let base = id.canonicalize().unwrap_or_else(|_| id.to_path_buf());

        // The well directory is the parent of a TIFF, or `id` itself when a
        // directory is given.  The plate directory is the parent of the well.
        let well_dir = if base.is_dir() {
            base.clone()
        } else {
            base.parent()
                .map(|p| p.to_path_buf())
                .unwrap_or(base.clone())
        };
        let plate_dir = well_dir
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or(well_dir.clone());

        // Enumerate well directories under the plate.
        let mut well_dirs: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&plate_dir) {
            let mut names: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            names.sort();
            for p in names {
                if p.is_dir() {
                    let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
                    if is_well_dir_name(name) && dir_has_tiff_or_subdir(&p) {
                        well_dirs.push(p);
                    }
                }
            }
        }
        // Fallback: treat the single given well directory as the only well.
        if well_dirs.is_empty() {
            well_dirs.push(well_dir.clone());
        }

        let mut wells = Vec::new();
        for wd in &well_dirs {
            let mut tiffs = collect_well_tiffs(wd);
            tiffs.sort();
            if tiffs.is_empty() {
                continue;
            }

            // Determine the dimension counts from distinct block values.
            let mut z_vals: Vec<i64> = Vec::new();
            let mut t_vals: Vec<i64> = Vec::new();
            let mut c_vals: Vec<i64> = Vec::new();
            let mut im_rows: Vec<i64> = Vec::new();
            let mut im_cols: Vec<i64> = Vec::new();
            for t in &tiffs {
                let name = t.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if let Some(z) = mias_block(name, "z") {
                    if !z_vals.contains(&z) {
                        z_vals.push(z);
                    }
                }
                if let Some(tt) = mias_block(name, "t") {
                    if !t_vals.contains(&tt) {
                        t_vals.push(tt);
                    }
                }
                if let Some(c) = mias_block(name, "mode") {
                    if !c_vals.contains(&c) {
                        c_vals.push(c);
                    }
                }
                if let Some(im) = mias_block(name, "im") {
                    if !im_rows.contains(&im) {
                        im_rows.push(im);
                    }
                    // The tile column is the trailing bare-integer block; it is
                    // only meaningful for tiled mosaics (those with an "im" row
                    // block), per MIASReader's FilePattern handling.
                    if let Some(col) = mias_trailing_col(name) {
                        if !im_cols.contains(&col) {
                            im_cols.push(col);
                        }
                    }
                }
            }
            let size_z = (z_vals.len() as u32).max(1);
            let size_t = (t_vals.len() as u32).max(1);
            let size_c = (c_vals.len() as u32).max(1);
            if im_rows.len() as u32 > self.tile_rows {
                self.tile_rows = im_rows.len() as u32;
            }
            if im_cols.len() as u32 > self.tile_cols {
                self.tile_cols = im_cols.len() as u32;
            }

            let name = wd.file_name().and_then(|n| n.to_str()).unwrap_or("");
            wells.push(MiasWell {
                tiffs,
                size_z,
                size_c,
                size_t,
                well_number: well_number_from_name(name),
            });
        }

        if wells.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "MIAS: no TIFF files found in any well directory".into(),
            ));
        }

        if self.tile_cols == 0 {
            self.tile_cols = 1;
        }
        if self.tile_rows == 0 {
            self.tile_rows = 1;
        }

        // Probe the first TIFF for pixel parameters (assume uniform).
        self.tiff_reader.set_id(&wells[0].tiffs[0])?;
        let tm = self.tiff_reader.metadata();
        let tile_w = tm.size_x;
        let tile_h = tm.size_y;
        let pixel_type = tm.pixel_type;
        let bits = tm.bits_per_pixel;
        let little_endian = tm.is_little_endian;
        let tiff_c = tm.size_c.max(1);
        let is_rgb = tm.is_rgb;
        let _ = self.tiff_reader.close();

        let mut series = Vec::with_capacity(wells.len());
        for w in &wells {
            let size_c = w.size_c * tiff_c;
            let mut meta_map = HashMap::new();
            meta_map.insert(
                "format".to_string(),
                crate::common::metadata::MetadataValue::String("MIAS".into()),
            );
            meta_map.insert(
                "well_number".to_string(),
                crate::common::metadata::MetadataValue::Int(w.well_number),
            );
            let image_count = (w.size_z * w.size_t * w.size_c).max(1);
            series.push(ImageMetadata {
                size_x: tile_w * self.tile_cols,
                size_y: tile_h * self.tile_rows,
                size_z: w.size_z,
                size_c,
                size_t: w.size_t,
                pixel_type,
                bits_per_pixel: bits,
                image_count,
                dimension_order: DimensionOrder::XYZCT,
                is_rgb,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: little_endian,
                resolution_count: 1,
                series_metadata: meta_map,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
        }

        self.wells = wells;
        self.series = series;
        self.current_series = 0;
        Ok(())
    }
}

fn dir_has_tiff_or_subdir(dir: &Path) -> bool {
    std::fs::read_dir(dir)
        .map(|entries| {
            entries.flatten().any(|e| {
                let p = e.path();
                p.is_dir()
                    || p.file_name()
                        .and_then(|n| n.to_str())
                        .map(is_mias_tiff)
                        .unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

/// Collect TIFFs from a well directory; if none are present, descend into
/// single-character channel subdirectories (the alternate MIAS layout).
fn collect_well_tiffs(well_dir: &Path) -> Vec<PathBuf> {
    let mut tiffs = Vec::new();
    if let Ok(entries) = std::fs::read_dir(well_dir) {
        let mut paths: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
        paths.sort();
        for p in &paths {
            if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                if is_mias_tiff(name) {
                    tiffs.push(p.clone());
                }
            }
        }
        if tiffs.is_empty() {
            for p in &paths {
                if p.is_dir() {
                    if let Ok(sub) = std::fs::read_dir(p) {
                        let mut subpaths: Vec<PathBuf> = sub.flatten().map(|e| e.path()).collect();
                        subpaths.sort();
                        for sp in subpaths {
                            if let Some(name) = sp.file_name().and_then(|n| n.to_str()) {
                                if is_mias_tiff(name) {
                                    tiffs.push(sp);
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    tiffs
}

impl FormatReader for MiasReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // A MIAS TIFF lives in a Well<xxxx> directory and uses the
        // mode/z/t naming convention.
        if !path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
            .unwrap_or(false)
        {
            return false;
        }
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let in_well_dir = path
            .parent()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(is_well_dir_name)
            .unwrap_or(false);
        in_well_dir && (mias_block(name, "mode").is_some() || mias_block(name, "z").is_some())
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Robustly reject any .tif/.tiff that is not a genuine MIAS dataset so
        // that plain TIFFs fall through to the generic TiffReader. A real MIAS
        // file lives in a Well<xxxx> directory and uses the mode/z/t naming
        // convention (the same guard the registry uses before the TIFF magic
        // pass). Directory inputs (a well/plate dir) are allowed through.
        if !path.is_dir() && !self.is_this_type_by_name(path) {
            return Err(BioFormatsError::UnsupportedFormat(
                "MIAS: file is not a Well<xxxx>/mode<c>_z<zzz>_t<ttt> TIFF dataset".into(),
            ));
        }
        self.tile_rows = 1;
        self.tile_cols = 1;
        self.build(path)?;
        self.tiff_loaded = false;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.wells.clear();
        self.series.clear();
        self.current_series = 0;
        self.tile_rows = 1;
        self.tile_cols = 1;
        if self.tiff_loaded {
            let _ = self.tiff_reader.close();
            self.tiff_loaded = false;
        }
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len().max(1)
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_count() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current_series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let tile_rows = self.tile_rows.max(1);
        let tile_cols = self.tile_cols.max(1);

        // Non-tiled case: plane index maps directly to tiffs[series][no].
        if tile_rows == 1 && tile_cols == 1 {
            let well = self
                .wells
                .get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized)?;
            let tiff_path = well
                .tiffs
                .get(plane_index as usize)
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?
                .clone();
            if self.tiff_loaded {
                let _ = self.tiff_reader.close();
            }
            self.tiff_reader.set_id(&tiff_path)?;
            self.tiff_loaded = true;
            return self.tiff_reader.open_bytes(0);
        }

        // Tiled mosaic: assemble all tiles of this plane into the full plane.
        // Tile (row, col) is the TIFF at index (no*tileRows + row)*tileCols + col
        // and is placed at output position (col*tileWidth, row*tileHeight),
        // matching MIASReader.openBytes / getTile.
        let full_w = meta.size_x as usize;
        let full_h = meta.size_y as usize;
        let bps = meta.pixel_type.bytes_per_sample();
        let rgb = meta.is_rgb;
        let samples = if rgb { meta.size_c.max(1) as usize } else { 1 };
        // bytes per output (full) row across all samples for the non-interleaved
        // layout used by the underlying TIFF reader is handled per-tile below.
        let mut out = vec![0u8; full_w * full_h * bps * samples];
        let out_row_len = full_w * bps * samples;

        for row in 0..tile_rows {
            for col in 0..tile_cols {
                let tile_index = ((plane_index * tile_rows + row) * tile_cols + col) as usize;
                let tiff_path = {
                    let well = self
                        .wells
                        .get(self.current_series)
                        .ok_or(BioFormatsError::NotInitialized)?;
                    match well.tiffs.get(tile_index) {
                        Some(p) => p.clone(),
                        None => continue, // missing tile -> leave zero-filled
                    }
                };
                if self.tiff_loaded {
                    let _ = self.tiff_reader.close();
                }
                self.tiff_reader.set_id(&tiff_path)?;
                self.tiff_loaded = true;
                let tile = self.tiff_reader.open_bytes(0)?;

                let tm = self.tiff_reader.metadata();
                let tile_w = tm.size_x as usize;
                let tile_h = tm.size_y as usize;
                let tile_row_len = tile_w * bps * samples;

                let x_off = col as usize * tile_w * bps * samples;
                let y_off = row as usize * tile_h;
                // Copy each tile row into the output, clipping at the edges.
                for trow in 0..tile_h {
                    let out_y = y_off + trow;
                    if out_y >= full_h {
                        break;
                    }
                    let src = &tile[trow * tile_row_len..(trow + 1) * tile_row_len];
                    let dst_start = out_y * out_row_len + x_off;
                    let copy_len = tile_row_len.min(out_row_len.saturating_sub(x_off));
                    out[dst_start..dst_start + copy_len].copy_from_slice(&src[..copy_len]);
                }
            }
        }
        Ok(out)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("MIAS", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
