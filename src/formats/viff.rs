//! Khoros VIFF / XV (Visualization Image File Format) reader.
//!
//! Ported from the Java `KhorosReader` ("Khoros XV"). Magic: 16-bit value
//! `0xAB01` (first byte `0xAB`). Extensions: `.xv`, `.viff`.
//!
//! The 1024-byte header is parsed per `KhorosReader.initFile`: a `dependency`
//! word selects byte order, the comment block is skipped, dimensions and the
//! `imageCount`/`sizeC`/pixel-type fields are read, and an optional colour
//! lookup table is parsed. Multiple bands are exposed as Z planes
//! (`sizeZ = imageCount`).

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, LookupTable, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

fn read_i32(buf: &[u8], offset: usize, little: bool) -> i32 {
    let b = [buf[offset], buf[offset + 1], buf[offset + 2], buf[offset + 3]];
    if little {
        i32::from_le_bytes(b)
    } else {
        i32::from_be_bytes(b)
    }
}

struct ViffParsed {
    meta: ImageMetadata,
    offset: u64,
}

fn parse_khoros(data: &[u8]) -> Result<ViffParsed> {
    // Need at least the fixed 1024-byte header.
    if data.len() < 584 {
        return Err(BioFormatsError::Format("VIFF/Khoros header too short".into()));
    }

    // skipBytes(4); order(true); dependency = readInt() [big-endian].
    let dependency = read_i32(data, 4, false);

    // Comment block: readString(512) at pos 8..520.
    let comment = String::from_utf8_lossy(&data[8..520]).trim_end_matches('\0').to_string();

    // Remaining reads use little-endian iff dependency is 4 or 8.
    let little = dependency == 4 || dependency == 8;

    let size_x = read_i32(data, 520, little).max(0) as u32;
    let size_y = read_i32(data, 524, little).max(0) as u32;
    // skipBytes(28) -> pos 556
    let mut image_count = read_i32(data, 556, little);
    if image_count == 0 {
        image_count = 1;
    }
    let image_count = image_count.max(1) as u32;
    let mut size_c = read_i32(data, 560, little).max(1) as u32;

    let type_code = read_i32(data, 564, little);
    let pixel_type = match type_code {
        0 => PixelType::Int8,
        1 => PixelType::Uint8,
        2 => PixelType::Uint16,
        4 => PixelType::Int32,
        5 => PixelType::Float32,
        9 => PixelType::Float64,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Khoros/VIFF unsupported pixel type: {other}"
            )))
        }
    };

    // read lookup table: skipBytes(12) -> pos 580; c = readInt() at 580.
    let lut_c = read_i32(data, 580, little);
    let mut lookup_table = None;
    let offset: u64;

    if lut_c > 1 {
        size_c = lut_c as u32;
        // n = readInt() at 584.
        if data.len() < 588 {
            return Err(BioFormatsError::Format("VIFF/Khoros header too short for LUT".into()));
        }
        let n = read_i32(data, 584, little).max(0) as usize;
        // skipBytes(436): pos 588 -> 1024. LUT bytes start at 1024.
        let lut_start = 1024usize;
        let lut_bytes = (lut_c as usize).checked_mul(n).ok_or_else(|| {
            BioFormatsError::Format("VIFF/Khoros LUT size overflow".into())
        })?;
        if lut_start + lut_bytes > data.len() {
            return Err(BioFormatsError::Format(
                "VIFF/Khoros LUT extends past end of file".into(),
            ));
        }
        // lut[c][n]: build an RGB LookupTable from the first three bands when
        // available (n entries per band). Java exposes it as a c x n table; we
        // map bands 0/1/2 to R/G/B for the common 3-band palette case.
        if lut_c >= 3 && n > 0 {
            let mut red = vec![0u16; n];
            let mut green = vec![0u16; n];
            let mut blue = vec![0u16; n];
            for j in 0..n {
                red[j] = data[lut_start + j] as u16;
                green[j] = data[lut_start + n + j] as u16;
                blue[j] = data[lut_start + 2 * n + j] as u16;
            }
            lookup_table = Some(LookupTable { red, green, blue });
        } else if n > 0 {
            // Single-band palette: replicate across RGB.
            let mut chan = vec![0u16; n];
            for j in 0..n {
                chan[j] = data[lut_start + j] as u16;
            }
            lookup_table = Some(LookupTable {
                red: chan.clone(),
                green: chan.clone(),
                blue: chan,
            });
        }
        offset = (lut_start + lut_bytes) as u64;
    } else {
        // skipBytes(440): pos 584 -> 1024.
        offset = 1024;
    }

    if size_x == 0 || size_y == 0 {
        return Err(BioFormatsError::Format(
            "VIFF/Khoros header is missing image dimensions".into(),
        ));
    }

    let is_indexed = lookup_table.is_some();
    let mut final_size_c = size_c;
    let mut is_rgb = size_c > 1;
    if is_indexed {
        final_size_c = 1;
        is_rgb = false;
    }

    let mut series_metadata = HashMap::new();
    if !comment.is_empty() {
        series_metadata.insert("Comment".into(), MetadataValue::String(comment));
    }

    let bps = pixel_type.bytes_per_sample();
    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z: image_count,
        size_c: final_size_c,
        size_t: 1,
        pixel_type,
        bits_per_pixel: (bps * 8) as u8,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: false,
        is_indexed,
        is_little_endian: little,
        resolution_count: 1,
        series_metadata,
        lookup_table,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok(ViffParsed { meta, offset })
}

pub struct ViffReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl ViffReader {
    pub fn new() -> Self {
        ViffReader {
            path: None,
            meta: None,
            data_offset: 1024,
        }
    }
}

impl Default for ViffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl ViffReader {
    /// Bytes for a single Z plane (sizeX * sizeY * sizeC * bytesPerSample).
    fn plane_bytes(meta: &ImageMetadata) -> usize {
        let bps = meta.pixel_type.bytes_per_sample();
        meta.size_x as usize * meta.size_y as usize * meta.size_c as usize * bps
    }
}

impl FormatReader for ViffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("xv") | Some("viff"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // KHOROS_MAGIC_BYTES = 0xab01 (big-endian short).
        header.len() >= 2 && header[0] == 0xAB && header[1] == 0x01
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let parsed = parse_khoros(&data)?;
        self.path = Some(path.to_path_buf());
        self.data_offset = parsed.offset;
        self.meta = Some(parsed.meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 1024;
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
        let plane_bytes = Self::plane_bytes(meta);
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(
            self.data_offset + plane_index as u64 * plane_bytes as u64,
        ))
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
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        // Planar (non-interleaved) layout: each channel is a separate plane.
        let channels = meta.size_c as usize;
        let plane = meta.size_x as usize * meta.size_y as usize * bps;
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let out_plane = w as usize * h as usize * bps;
        let mut out = vec![0u8; out_plane * channels];
        for c in 0..channels {
            let src_plane = &full[c * plane..(c + 1) * plane];
            for r in 0..h as usize {
                let src = (y as usize + r) * row + x as usize * bps;
                let dst = c * out_plane + r * out_row;
                out[dst..dst + out_row].copy_from_slice(&src_plane[src..src + out_row]);
            }
        }
        Ok(out)
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
