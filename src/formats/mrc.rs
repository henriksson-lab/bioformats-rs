//! MRC/CCP4 format reader and writer (used in electron microscopy / cryo-EM).
//!
//! Specification: MRC2014 — https://www.ccpem.ac.uk/mrc_format/mrc2014.php
//! Header is exactly 1024 bytes.

use std::fs::File;
use std::io::{BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use crate::common::writer::FormatWriter;

// ---- header -----------------------------------------------------------------

const HEADER_SIZE: u64 = 1024;
const IMOD_STAMP: u32 = 1146047817; // 'IMOD' in ASCII little-endian

fn read_i32_le(data: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}
fn read_u32_le(data: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}
fn read_u32_be(data: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}
fn read_f32_le(data: &[u8], off: usize) -> f32 {
    f32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}
fn read_i32_be(data: &[u8], off: usize) -> i32 {
    i32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}
fn read_f32_be(data: &[u8], off: usize) -> f32 {
    f32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}

fn plausible_mode(mode: i32) -> bool {
    matches!(mode, 0 | 1 | 2 | 3 | 4 | 6 | 16)
}

fn plausible_dimension(value: i32) -> bool {
    value > 0 && value <= 1_000_000
}

fn endian_score(buf: &[u8], little_endian: bool) -> i32 {
    let read_i32 = if little_endian {
        read_i32_le
    } else {
        read_i32_be
    };
    let nx = read_i32(buf, 0);
    let ny = read_i32(buf, 4);
    let nz = read_i32(buf, 8);
    let mode = read_i32(buf, 12);
    let mapc = read_i32(buf, 64);
    let mapr = read_i32(buf, 68);
    let maps = read_i32(buf, 72);

    let mut score = 0;
    if plausible_dimension(nx) {
        score += 2;
    }
    if plausible_dimension(ny) {
        score += 2;
    }
    if plausible_dimension(nz) {
        score += 2;
    }
    if plausible_mode(mode) {
        score += 3;
    }
    if valid_axis_permutation(mapc, mapr, maps) {
        score += 2;
    }
    score
}

fn detect_little_endian(buf: &[u8]) -> bool {
    let le_score = endian_score(buf, true);
    let be_score = endian_score(buf, false);
    if le_score != be_score {
        return le_score > be_score;
    }

    // MRC2014 machine stamp, when present and not contradicted by header
    // plausibility, uses 0x44 for little-endian and 0x11 for big-endian.
    match buf[212] {
        0x11 => false,
        0x44 => true,
        _ => true,
    }
}

struct MrcHeader {
    nx: i32,
    ny: i32,
    nz: i32,
    mode: i32,
    nxstart: i32,
    nystart: i32,
    nzstart: i32,
    xlen: f32,
    ylen: f32,
    zlen: f32,
    mx: i32,
    my: i32,
    mz: i32,
    mapc: i32,
    mapr: i32,
    maps: i32,
    origin_x: f32,
    origin_y: f32,
    origin_z: f32,
    imod_stamp: u32,
    imod_flags: i32,
    extended_header_size: i32,
    min_value: f32,
    max_value: f32,
    little_endian: bool,
}

fn parse_header(buf: &[u8]) -> Result<MrcHeader> {
    if buf.len() < HEADER_SIZE as usize {
        return Err(BioFormatsError::Format("MRC header too short".into()));
    }

    // Some legacy MRC/IMOD files either omit the modern machine stamp or carry
    // a misleading first stamp byte. Prefer the endian interpretation that
    // yields plausible dimensions, mode, and axis metadata.
    let little_endian = detect_little_endian(buf);

    let (nx, ny, nz, mode) = if little_endian {
        (
            read_i32_le(buf, 0),
            read_i32_le(buf, 4),
            read_i32_le(buf, 8),
            read_i32_le(buf, 12),
        )
    } else {
        (
            read_i32_be(buf, 0),
            read_i32_be(buf, 4),
            read_i32_be(buf, 8),
            read_i32_be(buf, 12),
        )
    };

    // Cell dimensions (angstroms)
    let (xlen, ylen, zlen) = if little_endian {
        (
            read_f32_le(buf, 40),
            read_f32_le(buf, 44),
            read_f32_le(buf, 48),
        )
    } else {
        (
            read_f32_be(buf, 40),
            read_f32_be(buf, 44),
            read_f32_be(buf, 48),
        )
    };

    let (mx, my, mz) = if little_endian {
        (
            read_i32_le(buf, 28),
            read_i32_le(buf, 32),
            read_i32_le(buf, 36),
        )
    } else {
        (
            read_i32_be(buf, 28),
            read_i32_be(buf, 32),
            read_i32_be(buf, 36),
        )
    };

    let (nxstart, nystart, nzstart, mapc, mapr, maps) = if little_endian {
        (
            read_i32_le(buf, 16),
            read_i32_le(buf, 20),
            read_i32_le(buf, 24),
            read_i32_le(buf, 64),
            read_i32_le(buf, 68),
            read_i32_le(buf, 72),
        )
    } else {
        (
            read_i32_be(buf, 16),
            read_i32_be(buf, 20),
            read_i32_be(buf, 24),
            read_i32_be(buf, 64),
            read_i32_be(buf, 68),
            read_i32_be(buf, 72),
        )
    };

    let (origin_x, origin_y, origin_z) = if little_endian {
        (
            read_f32_le(buf, 196),
            read_f32_le(buf, 200),
            read_f32_le(buf, 204),
        )
    } else {
        (
            read_f32_be(buf, 196),
            read_f32_be(buf, 200),
            read_f32_be(buf, 204),
        )
    };

    let imod_stamp = if little_endian {
        read_u32_le(buf, 152)
    } else {
        read_u32_be(buf, 152)
    };
    let imod_flags = if little_endian {
        read_i32_le(buf, 156)
    } else {
        read_i32_be(buf, 156)
    };

    // Extended header size (bytes): at offset 92 in MRC2014
    let extended_header_size = if little_endian {
        read_i32_le(buf, 92)
    } else {
        read_i32_be(buf, 92)
    };

    // Min/max pixel values (DMIN/DMAX). Per MRCReader.java these are read
    // immediately after the 12-byte MAPC/MAPR/MAPS block (offset 76 = DMIN,
    // offset 80 = DMAX, offset 84 = DMEAN).
    let (min_value, max_value) = if little_endian {
        (read_f32_le(buf, 76), read_f32_le(buf, 80))
    } else {
        (read_f32_be(buf, 76), read_f32_be(buf, 80))
    };

    Ok(MrcHeader {
        nx,
        ny,
        nz,
        mode,
        nxstart,
        nystart,
        nzstart,
        xlen,
        ylen,
        zlen,
        mx,
        my,
        mz,
        mapc,
        mapr,
        maps,
        origin_x,
        origin_y,
        origin_z,
        imod_stamp,
        imod_flags,
        extended_header_size,
        min_value,
        max_value,
        little_endian,
    })
}

/// Whether the pixel type is signed (used for the EMAN2 min/max correction).
fn pixel_type_is_signed(pt: PixelType) -> bool {
    matches!(pt, PixelType::Int8 | PixelType::Int16 | PixelType::Int32)
}

/// Apply the EMAN2 unsigned-data correction from MRCReader.java: if the stored
/// data min/max fall outside the signed pixel-type range, promote INT16→UINT16
/// and INT32→UINT32.
fn correct_eman2_pixel_type(pt: PixelType, min_value: f64, max_value: f64) -> PixelType {
    let bytes = pt.bytes_per_sample() as f64;
    let range = 2f64.powf(bytes * 8.0) - 1.0;
    let signed = pixel_type_is_signed(pt);
    let pixel_type_min = if signed { -(range / 2.0) } else { 0.0 };
    let pixel_type_max = pixel_type_min + range;

    // Java: if (pixelTypeMax < maxValue || pixelTypeMin > minValue && signed)
    if pixel_type_max < max_value || (pixel_type_min > min_value && signed) {
        match pt {
            PixelType::Int16 => PixelType::Uint16,
            PixelType::Int32 => PixelType::Uint32,
            other => other,
        }
    } else {
        pt
    }
}

fn valid_axis_permutation(mapc: i32, mapr: i32, maps: i32) -> bool {
    matches!(
        (mapc, mapr, maps),
        (1, 2, 3) | (1, 3, 2) | (2, 1, 3) | (2, 3, 1) | (3, 1, 2) | (3, 2, 1)
    )
}

fn pixel_type_from_mode(mode: i32, imod_stamp: u32, imod_flags: i32) -> PixelType {
    match mode {
        0 => {
            // In IMOD, bit 0 of IMODFLAGS indicates signed
            if imod_stamp == IMOD_STAMP && (imod_flags & 1) != 0 {
                PixelType::Int8
            } else {
                PixelType::Uint8
            }
        }
        1 => PixelType::Int16,
        2 => PixelType::Float32,
        3 => PixelType::Uint32, // complex16 → treated as uint32 here
        4 => PixelType::Float64,
        6 => PixelType::Uint16,
        16 => PixelType::Uint8, // RGB uint8 (3-channel)
        _ => PixelType::Float32,
    }
}

fn mode_from_pixel_type(pt: PixelType, is_rgb: bool) -> i32 {
    if is_rgb {
        return 16;
    }
    match pt {
        PixelType::Uint8 | PixelType::Int8 => 0,
        PixelType::Int16 => 1,
        PixelType::Float32 => 2,
        PixelType::Float64 => 4,
        PixelType::Uint16 => 6,
        _ => 2,
    }
}

// ---- reader -----------------------------------------------------------------

pub struct MrcReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
    flip_y: bool,
}

impl MrcReader {
    pub fn new() -> Self {
        MrcReader {
            path: None,
            meta: None,
            data_offset: 0,
            flip_y: true,
        }
    }
}

impl Default for MrcReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MrcReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "mrc" | "mrcs" | "ccp4" | "map" | "rec"
                )
            })
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // MRC2014: bytes 208-211 = "MAP " (with space)
        if header.len() >= 212 && &header[208..212] == b"MAP " {
            return true;
        }
        // Older MRC / IMOD: check for reasonable NX/NY/NZ in first 12 bytes
        if header.len() >= 12 {
            if header.len() >= 16 {
                let dib_header_size =
                    u32::from_le_bytes([header[0], header[1], header[2], header[3]]);
                let dib_planes = u16::from_le_bytes([header[12], header[13]]);
                let dib_bpp = u16::from_le_bytes([header[14], header[15]]);
                if matches!(dib_header_size, 40 | 108 | 124)
                    && dib_planes == 1
                    && matches!(dib_bpp, 1 | 4 | 8 | 16 | 24 | 32)
                {
                    return false;
                }
            }
            let nx = i32::from_le_bytes([header[0], header[1], header[2], header[3]]);
            let ny = i32::from_le_bytes([header[4], header[5], header[6], header[7]]);
            let nz = i32::from_le_bytes([header[8], header[9], header[10], header[11]]);
            if nx > 0 && nx < 65536 && ny > 0 && ny < 65536 && nz > 0 && nz < 65536 {
                return true;
            }
        }
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; HEADER_SIZE as usize];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;

        let hdr = parse_header(&buf)?;
        let mut pixel_type = pixel_type_from_mode(hdr.mode, hdr.imod_stamp, hdr.imod_flags);
        // EMAN2 unsigned-data correction (MRCReader.java).
        pixel_type =
            correct_eman2_pixel_type(pixel_type, hdr.min_value as f64, hdr.max_value as f64);
        let is_rgb = hdr.mode == 16;
        let spp = if is_rgb { 3u32 } else { 1u32 };

        if hdr.nx <= 0 || hdr.ny <= 0 || hdr.nz <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "MRC header dimensions must be positive: nx={}, ny={}, nz={}",
                hdr.nx, hdr.ny, hdr.nz
            )));
        }
        let nx = hdr.nx as u32;
        let ny = hdr.ny as u32;
        let nz = hdr.nz as u32;

        let data_offset = HEADER_SIZE + hdr.extended_header_size.max(0) as u64;
        let plane_bytes = (nx as u64)
            .checked_mul(ny as u64)
            .and_then(|v| v.checked_mul(spp as u64))
            .and_then(|v| v.checked_mul(pixel_type.bytes_per_sample() as u64))
            .ok_or_else(|| BioFormatsError::Format("MRC plane byte count overflows".into()))?;
        let pixel_bytes = plane_bytes
            .checked_mul(nz as u64)
            .ok_or_else(|| BioFormatsError::Format("MRC pixel byte count overflows".into()))?;
        let required_len = data_offset
            .checked_add(pixel_bytes)
            .ok_or_else(|| BioFormatsError::Format("MRC pixel payload offset overflows".into()))?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        if file_len < required_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "MRC pixel payload is shorter than declared: need {required_len} bytes, found {file_len}"
            )));
        }
        // Per MRCReader.java the rows are always flipped (lower-left origin).
        let flip_y = true;

        // Physical pixel size (if available)
        let mut series_metadata = std::collections::HashMap::new();
        series_metadata.insert("MapColumnAxis".into(), MetadataValue::Int(hdr.mapc as i64));
        series_metadata.insert("MapRowAxis".into(), MetadataValue::Int(hdr.mapr as i64));
        series_metadata.insert("MapSectionAxis".into(), MetadataValue::Int(hdr.maps as i64));
        series_metadata.insert("ColumnStart".into(), MetadataValue::Int(hdr.nxstart as i64));
        series_metadata.insert("RowStart".into(), MetadataValue::Int(hdr.nystart as i64));
        series_metadata.insert(
            "SectionStart".into(),
            MetadataValue::Int(hdr.nzstart as i64),
        );
        series_metadata.insert("OriginX".into(), MetadataValue::Float(hdr.origin_x as f64));
        series_metadata.insert("OriginY".into(), MetadataValue::Float(hdr.origin_y as f64));
        series_metadata.insert("OriginZ".into(), MetadataValue::Float(hdr.origin_z as f64));
        series_metadata.insert("FlipY".into(), MetadataValue::Bool(flip_y));
        if hdr.mx > 0 && hdr.xlen > 0.0 {
            let px_a = hdr.xlen / hdr.mx as f32;
            series_metadata.insert(
                "PhysicalSizeXAngstrom".into(),
                MetadataValue::Float(px_a as f64),
            );
        }
        if hdr.my > 0 && hdr.ylen > 0.0 {
            let py_a = hdr.ylen / hdr.my as f32;
            series_metadata.insert(
                "PhysicalSizeYAngstrom".into(),
                MetadataValue::Float(py_a as f64),
            );
        }
        if hdr.mz > 0 && hdr.zlen > 0.0 && nz > 1 {
            let pz_a = hdr.zlen / hdr.mz as f32;
            series_metadata.insert(
                "PhysicalSizeZAngstrom".into(),
                MetadataValue::Float(pz_a as f64),
            );
        }

        self.meta = Some(ImageMetadata {
            size_x: nx,
            size_y: ny,
            size_z: nz,
            size_c: spp,
            size_t: 1,
            pixel_type,
            bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
            image_count: nz,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: hdr.little_endian,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.data_offset = data_offset;
        self.flip_y = flip_y;
        self.path = Some(path.to_path_buf());
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
        let spp = meta.size_c as usize;
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * spp * bps;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;

        if !self.flip_y {
            return Ok(buf);
        }

        // Legacy MRC files commonly store planes with a lower-left origin.
        let row_bytes = meta.size_x as usize * spp * bps;
        let mut flipped = vec![0u8; plane_bytes];
        for row in 0..meta.size_y as usize {
            let src = &buf[row * row_bytes..(row + 1) * row_bytes];
            let dst_row = meta.size_y as usize - 1 - row;
            flipped[dst_row * row_bytes..(dst_row + 1) * row_bytes].copy_from_slice(src);
        }
        Ok(flipped)
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
        crop_full_plane("MRC", &full, meta, meta.size_c as usize, x, y, w, h)
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
        // Stored in Ångströms → convert to µm (÷10)
        img.physical_size_x = get_f("PhysicalSizeXAngstrom").map(|v| v / 10.0);
        img.physical_size_y = get_f("PhysicalSizeYAngstrom").map(|v| v / 10.0);
        img.physical_size_z = get_f("PhysicalSizeZAngstrom").map(|v| v / 10.0);
        Some(ome)
    }
}

// ---- writer -----------------------------------------------------------------

pub struct MrcWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
}

impl MrcWriter {
    pub fn new() -> Self {
        MrcWriter {
            path: None,
            meta: None,
            planes: Vec::new(),
        }
    }
}

impl Default for MrcWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for MrcWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                matches!(
                    e.to_ascii_lowercase().as_str(),
                    "mrc" | "mrcs" | "map" | "ccp4"
                )
            })
            .unwrap_or(false)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        if meta.size_t.max(1) > 1 || (meta.size_c.max(1) > 1 && !meta.is_rgb) {
            return Err(BioFormatsError::UnsupportedFormat(
                "MRC writer preserves only Z stacks and RGB samples, not non-RGB C/T axes".into(),
            ));
        }
        self.meta = Some(meta.clone());
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta
            .as_ref()
            .ok_or_else(|| BioFormatsError::Format("set_metadata first".into()))?;
        self.path = Some(path.to_path_buf());
        self.planes.clear();
        Ok(())
    }

    fn save_bytes(&mut self, idx: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_next_plane(
            "MRC",
            meta,
            self.planes.len(),
            idx,
            data.len(),
        )?;
        self.planes.push(data.to_vec());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let _path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_complete("MRC", meta, self.planes.len())?;
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.take().ok_or(BioFormatsError::NotInitialized)?;

        let f = File::create(&path).map_err(BioFormatsError::Io)?;
        let mut w = BufWriter::new(f);

        let nx = meta.size_x as i32;
        let ny = meta.size_y as i32;
        let nz = self.planes.len() as i32;
        let mode = mode_from_pixel_type(meta.pixel_type, meta.is_rgb);

        // Build 1024-byte header (little-endian, MRC2014)
        let mut hdr = vec![0u8; 1024];
        hdr[0..4].copy_from_slice(&nx.to_le_bytes());
        hdr[4..8].copy_from_slice(&ny.to_le_bytes());
        hdr[8..12].copy_from_slice(&nz.to_le_bytes());
        hdr[12..16].copy_from_slice(&mode.to_le_bytes());
        // MX, MY, MZ (grid sampling = image size)
        hdr[28..32].copy_from_slice(&nx.to_le_bytes());
        hdr[32..36].copy_from_slice(&ny.to_le_bytes());
        hdr[36..40].copy_from_slice(&nz.to_le_bytes());
        // CELLA (cell = image dims in Å; default 1 Å/pixel)
        let xl = (nx as f32).to_le_bytes();
        let yl = (ny as f32).to_le_bytes();
        let zl = (nz as f32).to_le_bytes();
        hdr[40..44].copy_from_slice(&xl);
        hdr[44..48].copy_from_slice(&yl);
        hdr[48..52].copy_from_slice(&zl);
        // Cell angles (90, 90, 90)
        let ninety = 90.0f32.to_le_bytes();
        hdr[52..56].copy_from_slice(&ninety);
        hdr[56..60].copy_from_slice(&ninety);
        hdr[60..64].copy_from_slice(&ninety);
        // MAPC, MAPR, MAPS = 1, 2, 3
        hdr[64..68].copy_from_slice(&1i32.to_le_bytes());
        hdr[68..72].copy_from_slice(&2i32.to_le_bytes());
        hdr[72..76].copy_from_slice(&3i32.to_le_bytes());
        // MAP identifier (MRC2014)
        hdr[208..212].copy_from_slice(b"MAP ");
        // Endian stamp: little-endian = 0x44 0x44 0x00 0x00
        hdr[212] = 0x44;
        hdr[213] = 0x44;
        // NVERSION = 20140
        hdr[220..224].copy_from_slice(&20140i32.to_le_bytes());

        w.write_all(&hdr).map_err(BioFormatsError::Io)?;

        // Write planes (flip rows — MRC is bottom-up)
        let samples = if meta.is_rgb {
            meta.size_c.max(1) as usize
        } else {
            1
        };
        let row_bytes = meta.size_x as usize * samples * meta.pixel_type.bytes_per_sample();
        for plane in &self.planes {
            for row in (0..meta.size_y as usize).rev() {
                w.write_all(&plane[row * row_bytes..(row + 1) * row_bytes])
                    .map_err(BioFormatsError::Io)?;
            }
        }
        w.flush().map_err(BioFormatsError::Io)?;
        self.planes.clear();
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_mrc_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_rs_{name}_{nonce}.mrc"))
    }

    fn write_mrc_fixture(
        name: &str,
        mapc: i32,
        mapr: i32,
        maps: i32,
        nystart: i32,
        origin_y: f32,
    ) -> PathBuf {
        let path = temp_mrc_path(name);
        let mut bytes = vec![0u8; HEADER_SIZE as usize];
        bytes[0..4].copy_from_slice(&2i32.to_le_bytes());
        bytes[4..8].copy_from_slice(&2i32.to_le_bytes());
        bytes[8..12].copy_from_slice(&1i32.to_le_bytes());
        bytes[12..16].copy_from_slice(&0i32.to_le_bytes());
        bytes[20..24].copy_from_slice(&nystart.to_le_bytes());
        bytes[28..32].copy_from_slice(&2i32.to_le_bytes());
        bytes[32..36].copy_from_slice(&2i32.to_le_bytes());
        bytes[36..40].copy_from_slice(&1i32.to_le_bytes());
        bytes[40..44].copy_from_slice(&2.0f32.to_le_bytes());
        bytes[44..48].copy_from_slice(&2.0f32.to_le_bytes());
        bytes[48..52].copy_from_slice(&1.0f32.to_le_bytes());
        bytes[64..68].copy_from_slice(&mapc.to_le_bytes());
        bytes[68..72].copy_from_slice(&mapr.to_le_bytes());
        bytes[72..76].copy_from_slice(&maps.to_le_bytes());
        bytes[200..204].copy_from_slice(&origin_y.to_le_bytes());
        bytes[208..212].copy_from_slice(b"MAP ");
        bytes[212] = 0x44;
        bytes[213] = 0x44;
        bytes[220..224].copy_from_slice(&20140i32.to_le_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn write_imod_signed_mode0_fixture() -> PathBuf {
        let path = write_mrc_fixture("imod_signed_mode0", 1, 2, 3, 0, 0.0);
        let mut bytes = fs::read(&path).unwrap();
        bytes[152..156].copy_from_slice(&IMOD_STAMP.to_le_bytes());
        bytes[156..160].copy_from_slice(&1i32.to_le_bytes());
        bytes[HEADER_SIZE as usize..HEADER_SIZE as usize + 4]
            .copy_from_slice(&[0x80, 0xff, 0x00, 0x7f]);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn write_old_big_endian_mode0_fixture() -> PathBuf {
        let path = temp_mrc_path("old_big_endian_mode0");
        let mut bytes = vec![0u8; HEADER_SIZE as usize];
        bytes[0..4].copy_from_slice(&2i32.to_be_bytes());
        bytes[4..8].copy_from_slice(&2i32.to_be_bytes());
        bytes[8..12].copy_from_slice(&1i32.to_be_bytes());
        bytes[12..16].copy_from_slice(&0i32.to_be_bytes());
        bytes[28..32].copy_from_slice(&2i32.to_be_bytes());
        bytes[32..36].copy_from_slice(&2i32.to_be_bytes());
        bytes[36..40].copy_from_slice(&1i32.to_be_bytes());
        bytes[40..44].copy_from_slice(&2.0f32.to_be_bytes());
        bytes[44..48].copy_from_slice(&2.0f32.to_be_bytes());
        bytes[48..52].copy_from_slice(&1.0f32.to_be_bytes());
        bytes[64..68].copy_from_slice(&1i32.to_be_bytes());
        bytes[68..72].copy_from_slice(&2i32.to_be_bytes());
        bytes[72..76].copy_from_slice(&3i32.to_be_bytes());
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        fs::write(&path, bytes).unwrap();
        path
    }

    fn write_old_little_endian_mode0_fixture_with_ambiguous_stamp() -> PathBuf {
        let path = write_mrc_fixture("old_little_endian_ambiguous_stamp", 1, 2, 3, 0, 0.0);
        let mut bytes = fs::read(&path).unwrap();
        bytes[208..212].copy_from_slice(b"MAP ");
        bytes[212] = 0x11;
        bytes[213] = 0;
        bytes[214] = 0;
        bytes[215] = 0;
        fs::write(&path, bytes).unwrap();
        path
    }

    fn read_fixture(path: &Path) -> (Vec<u8>, bool) {
        let mut reader = MrcReader::new();
        reader.set_id(path).unwrap();
        let flip_y = match reader.metadata().series_metadata.get("FlipY") {
            Some(MetadataValue::Bool(v)) => *v,
            other => panic!("unexpected FlipY metadata: {other:?}"),
        };
        let plane = reader.open_bytes(0).unwrap();
        (plane, flip_y)
    }

    #[test]
    fn mrc_reader_keeps_legacy_lower_left_origin_flip() {
        let path = write_mrc_fixture("legacy_flip", 1, 2, 3, 0, 0.0);
        let (plane, flip_y) = read_fixture(&path);
        fs::remove_file(path).ok();

        assert!(flip_y);
        assert_eq!(plane, vec![3, 4, 1, 2]);
    }

    // MRCReader.java always reverses the rows regardless of axis permutation,
    // Y-origin, or Y-start: planes are always stored with a lower-left origin.
    #[test]
    fn mrc_reader_flips_even_when_row_axis_is_not_y() {
        let path = write_mrc_fixture("row_axis_not_y", 2, 1, 3, 0, 0.0);
        let (plane, flip_y) = read_fixture(&path);
        fs::remove_file(path).ok();

        assert!(flip_y);
        assert_eq!(plane, vec![3, 4, 1, 2]);
    }

    #[test]
    fn mrc_reader_flips_even_when_y_origin_is_top_edge() {
        let path = write_mrc_fixture("origin_top", 1, 2, 3, 0, 1.0);
        let (plane, flip_y) = read_fixture(&path);
        fs::remove_file(path).ok();

        assert!(flip_y);
        assert_eq!(plane, vec![3, 4, 1, 2]);
    }

    #[test]
    fn mrc_reader_flips_even_when_y_start_is_top_edge() {
        let path = write_mrc_fixture("start_top", 1, 2, 3, 1, 0.0);
        let (plane, flip_y) = read_fixture(&path);
        fs::remove_file(path).ok();

        assert!(flip_y);
        assert_eq!(plane, vec![3, 4, 1, 2]);
    }

    #[test]
    fn mrc_reader_promotes_eman2_unsigned_int16() {
        // mode 1 → INT16, but stored DMAX exceeds the signed-16 range, so the
        // EMAN2 correction promotes the type to UINT16 (MRCReader.java).
        let path = temp_mrc_path("eman2_uint16");
        let mut bytes = vec![0u8; HEADER_SIZE as usize];
        bytes[0..4].copy_from_slice(&1i32.to_le_bytes()); // nx
        bytes[4..8].copy_from_slice(&1i32.to_le_bytes()); // ny
        bytes[8..12].copy_from_slice(&1i32.to_le_bytes()); // nz
        bytes[12..16].copy_from_slice(&1i32.to_le_bytes()); // mode 1 = INT16
        bytes[64..68].copy_from_slice(&1i32.to_le_bytes()); // mapc
        bytes[68..72].copy_from_slice(&2i32.to_le_bytes()); // mapr
        bytes[72..76].copy_from_slice(&3i32.to_le_bytes()); // maps
        bytes[76..80].copy_from_slice(&0.0f32.to_le_bytes()); // DMIN
        bytes[80..84].copy_from_slice(&60000.0f32.to_le_bytes()); // DMAX > 32767
        bytes[208..212].copy_from_slice(b"MAP ");
        bytes[212] = 0x44;
        bytes[213] = 0x44;
        bytes.extend_from_slice(&[0x00, 0x00]); // 1 sample
        fs::write(&path, &bytes).unwrap();

        let mut reader = MrcReader::new();
        reader.set_id(&path).unwrap();
        let pt = reader.metadata().pixel_type;
        fs::remove_file(path).ok();
        assert_eq!(pt, PixelType::Uint16);
    }

    #[test]
    fn mrc_reader_uses_imod_signed_mode0_pixel_type() {
        let path = write_imod_signed_mode0_fixture();
        let mut reader = MrcReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata().clone();
        let plane = reader.open_bytes(0).unwrap();
        fs::remove_file(path).ok();

        assert_eq!(meta.pixel_type, PixelType::Int8);
        assert_eq!(meta.bits_per_pixel, 8);
        assert_eq!(plane, vec![0x00, 0x7f, 0x80, 0xff]);
    }

    #[test]
    fn mrc_reader_detects_legacy_big_endian_without_machine_stamp() {
        let path = write_old_big_endian_mode0_fixture();
        let mut reader = MrcReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata().clone();
        let plane = reader.open_bytes(0).unwrap();
        fs::remove_file(path).ok();

        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 1);
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        assert!(!meta.is_little_endian);
        assert_eq!(plane, vec![3, 4, 1, 2]);
    }

    #[test]
    fn mrc_reader_prefers_plausible_little_endian_over_ambiguous_stamp() {
        let path = write_old_little_endian_mode0_fixture_with_ambiguous_stamp();
        let mut reader = MrcReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata().clone();
        let plane = reader.open_bytes(0).unwrap();
        fs::remove_file(path).ok();

        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 1);
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        assert!(meta.is_little_endian);
        assert_eq!(plane, vec![3, 4, 1, 2]);
    }
}
