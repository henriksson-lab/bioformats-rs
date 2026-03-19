//! Khoros VIFF (Visualization Image File Format) reader.
//!
//! Magic: first byte == 0xAB.
//! Extensions: .xv, .viff
//! 1024-byte big-endian header followed by raw pixel data.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

const HEADER_SIZE: u64 = 1024;

fn read_u32_be(buf: &[u8], offset: usize) -> u32 {
    u32::from_be_bytes([buf[offset], buf[offset+1], buf[offset+2], buf[offset+3]])
}

fn parse_viff_header(header: &[u8]) -> Result<ImageMetadata> {
    if header.len() < HEADER_SIZE as usize {
        return Err(BioFormatsError::Format("VIFF header too short".into()));
    }

    let height = read_u32_be(header, 40); // row_size
    let width  = read_u32_be(header, 44); // col_size
    let channels = read_u32_be(header, 76); // num_data_bands
    let storage_type = read_u32_be(header, 80);

    let pixel_type = match storage_type {
        0 => PixelType::Bit,
        1 => PixelType::Uint8,
        2 => PixelType::Uint16,
        4 => PixelType::Uint32,
        5 => PixelType::Float32,
        9 => PixelType::Float64,
        _ => PixelType::Uint8,
    };

    let bps = pixel_type.bytes_per_sample();
    let size_c = channels.max(1);

    let meta = ImageMetadata {
        size_x: width.max(1),
        size_y: height.max(1),
        size_z: 1,
        size_c,
        size_t: 1,
        pixel_type,
        bits_per_pixel: (bps * 8) as u8,
        image_count: 1,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: size_c == 3,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: false, // VIFF is big-endian
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
    };

    Ok(meta)
}

pub struct ViffReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl ViffReader {
    pub fn new() -> Self {
        ViffReader { path: None, meta: None, data_offset: HEADER_SIZE }
    }
}

impl Default for ViffReader {
    fn default() -> Self {
        Self::new()
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
        !header.is_empty() && header[0] == 0xAB
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let meta = parse_viff_header(&data)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.data_offset = HEADER_SIZE;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = HEADER_SIZE;
        Ok(())
    }

    fn series_count(&self) -> usize { 1 }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }

    fn series(&self) -> usize { 0 }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * meta.size_c as usize * bps;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
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
