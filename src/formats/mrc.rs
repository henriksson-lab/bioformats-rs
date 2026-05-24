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
fn read_f32_le(data: &[u8], off: usize) -> f32 {
    f32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}
fn read_i32_be(data: &[u8], off: usize) -> i32 {
    i32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}
fn read_f32_be(data: &[u8], off: usize) -> f32 {
    f32::from_be_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
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
    little_endian: bool,
}

fn parse_header(buf: &[u8]) -> Result<MrcHeader> {
    if buf.len() < HEADER_SIZE as usize {
        return Err(BioFormatsError::Format("MRC header too short".into()));
    }

    // Endianness: byte 212 — 'D' (0x44) = little-endian, 'A' (0x11=17) = big-endian
    // Alternatively check the magic stamp at 52-55 (MRC2014: "MAP ")
    let endian_byte = buf[212];
    let little_endian = endian_byte != 17; // 'A'=17 means big-endian; anything else = LE

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
        buf[152..156].iter().fold(0u32, |a, &b| (a << 8) | b as u32)
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
        little_endian,
    })
}

fn valid_axis_permutation(mapc: i32, mapr: i32, maps: i32) -> bool {
    matches!(
        (mapc, mapr, maps),
        (1, 2, 3) | (1, 3, 2) | (2, 1, 3) | (2, 3, 1) | (3, 1, 2) | (3, 2, 1)
    )
}

fn has_explicit_origin(hdr: &MrcHeader) -> bool {
    hdr.origin_x != 0.0 || hdr.origin_y != 0.0 || hdr.origin_z != 0.0
}

fn should_flip_y(hdr: &MrcHeader) -> bool {
    if valid_axis_permutation(hdr.mapc, hdr.mapr, hdr.maps) && hdr.mapr != 2 {
        return false;
    }

    let top_y = (hdr.ny - 1).max(0);
    if has_explicit_origin(hdr) && hdr.origin_y.round() as i32 >= top_y {
        return false;
    }
    if hdr.nystart >= top_y && top_y > 0 {
        return false;
    }

    true
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
        let pixel_type = pixel_type_from_mode(hdr.mode, hdr.imod_stamp, hdr.imod_flags);
        let is_rgb = hdr.mode == 16;
        let spp = if is_rgb { 3u32 } else { 1u32 };

        let nx = hdr.nx.max(0) as u32;
        let ny = hdr.ny.max(0) as u32;
        let nz = hdr.nz.max(0) as u32;

        let data_offset = HEADER_SIZE + hdr.extended_header_size.max(0) as u64;
        let flip_y = should_flip_y(&hdr);

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
            size_z: nz.max(1),
            size_c: spp,
            size_t: 1,
            pixel_type,
            bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
            image_count: nz.max(1),
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
        self.meta.as_ref().expect("set_id not called")
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
        let meta = self.meta.as_ref().unwrap();
        let spp = meta.size_c as usize;
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = meta.size_x as usize * spp * bps;
        let out_row = w as usize * spp * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * spp * bps;
            out.extend_from_slice(&src[s..s + out_row]);
        }
        Ok(out)
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

    fn save_bytes(&mut self, _idx: u32, data: &[u8]) -> Result<()> {
        self.planes.push(data.to_vec());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
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
        let row_bytes =
            meta.size_x as usize * meta.size_c as usize * meta.pixel_type.bytes_per_sample();
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

    #[test]
    fn mrc_reader_does_not_flip_when_row_axis_is_not_y() {
        let path = write_mrc_fixture("row_axis_not_y", 2, 1, 3, 0, 0.0);
        let (plane, flip_y) = read_fixture(&path);
        fs::remove_file(path).ok();

        assert!(!flip_y);
        assert_eq!(plane, vec![1, 2, 3, 4]);
    }

    #[test]
    fn mrc_reader_does_not_flip_when_y_origin_is_top_edge() {
        let path = write_mrc_fixture("origin_top", 1, 2, 3, 0, 1.0);
        let (plane, flip_y) = read_fixture(&path);
        fs::remove_file(path).ok();

        assert!(!flip_y);
        assert_eq!(plane, vec![1, 2, 3, 4]);
    }

    #[test]
    fn mrc_reader_does_not_flip_when_y_start_is_top_edge() {
        let path = write_mrc_fixture("start_top", 1, 2, 3, 1, 0.0);
        let (plane, flip_y) = read_fixture(&path);
        fs::remove_file(path).ok();

        assert!(!flip_y);
        assert_eq!(plane, vec![1, 2, 3, 4]);
    }
}
