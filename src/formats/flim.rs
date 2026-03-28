//! Becker & Hickl SPC / SDT FLIM format reader.
//!
//! The SDT (Single Photon Counting Data) file format is used by Becker & Hickl
//! TCSPC modules for fluorescence lifetime imaging (FLIM).
//!
//! File structure:
//!   - 18-byte ASCII ident: "SPC-130 Data File " (or SPC-140, SPC-630, etc.)
//!   - SPCFileHeader (binary fields): info_offs, info_length, setup_offs,
//!     setup_length, data_block_offs, no_of_data_blocks, data_block_length,
//!     meas_desc_block_offs, no_of_meas_desc_blocks, meas_desc_block_length
//!   - Info text block (ASCII)
//!   - Setup text block (ASCII, contains parameter lines like "sp_img_x:512")
//!   - Measurement descriptor blocks (binary)
//!   - Data blocks (16-bit photon counts: [n_t × n_x × n_y])
//!
//! The setup block contains keys of the form:  sp_img_x, sp_img_y, sp_ADC_RE
//! (ADC resolution = number of time channels).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

fn r_i16_le(b: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([b[off], b[off+1]])
}
fn r_i32_le(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off+1], b[off+2], b[off+3]])
}

/// Parse setup text block for image dimensions.
/// Returns (n_x, n_y, adc_re) extracted from "sp_img_x", "sp_img_y", "sp_ADC_RE".
fn parse_sdt_setup(text: &str) -> (u32, u32, u32) {
    let mut nx: u32 = 0;
    let mut ny: u32 = 0;
    let mut adc_re: u32 = 256;
    for line in text.lines() {
        let t = line.trim();
        // Format: "  #SP [SP_FLIM_X,I,128]" or "sp_img_x:128" or "IMG_X 128"
        let low = t.to_ascii_lowercase();
        if low.contains("sp_img_x") || low.contains("img_x") || low.contains("flim_x") {
            if let Some(v) = extract_int(t) { if v > 0 { nx = v; } }
        } else if low.contains("sp_img_y") || low.contains("img_y") || low.contains("flim_y") {
            if let Some(v) = extract_int(t) { if v > 0 { ny = v; } }
        } else if low.contains("sp_adc_re") || low.contains("adc_re") {
            if let Some(v) = extract_int(t) { if v > 0 { adc_re = v; } }
        }
    }
    (nx.max(1), ny.max(1), adc_re.max(1))
}

fn extract_int(s: &str) -> Option<u32> {
    // Find the last sequence of digits in the string
    let mut last: Option<u32> = None;
    let mut acc = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            acc.push(c);
        } else if !acc.is_empty() {
            if let Ok(v) = acc.parse::<u32>() { last = Some(v); }
            acc.clear();
        }
    }
    if !acc.is_empty() {
        if let Ok(v) = acc.parse::<u32>() { last = Some(v); }
    }
    last
}

pub struct SdtReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
    n_time: u32,
}

impl SdtReader {
    pub fn new() -> Self {
        SdtReader { path: None, meta: None, data_offset: 0, n_time: 256 }
    }
}

impl Default for SdtReader { fn default() -> Self { Self::new() } }

impl FormatReader for SdtReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sdt") | Some("spc"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Ident[18] starts with "SPC-"
        header.len() >= 4 && &header[..4] == b"SPC-"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;

        // Read full 22-byte SPCFileHeader
        let mut hdr = [0u8; 22];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        // Skip ident[18] (already read), then:
        // offset 18: info_offs (i16)
        // offset 20: info_length (i16)
        // We need more fields — re-read a larger block
        let _ = f.seek(SeekFrom::Start(0)).map_err(BioFormatsError::Io)?;
        let mut hdr = [0u8; 48];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        // SPCFileHeader layout:
        //  0: Ident[18]
        // 18: info_offs (i16)
        // 20: info_length (i16)
        // 22: setup_offs (i16)
        // 24: setup_length (i32)
        // 28: data_block_offs (i16)   -- truncated if >32767
        // 30: no_of_data_blocks (i16)
        // 32: data_block_length (i32)
        // 36: meas_desc_block_offs (i16)
        // 38: no_of_meas_desc_blocks (i16)
        // 40: meas_desc_block_length (i16)
        // 42: reserved[2×2]
        let setup_offs   = r_i16_le(&hdr, 22) as u64;
        let setup_length = r_i32_le(&hdr, 24) as usize;
        let data_offs    = r_i16_le(&hdr, 28) as i32;

        // Read setup text block
        let (nx, ny, adc_re) = if setup_offs > 0 && setup_length > 0 {
            f.seek(SeekFrom::Start(setup_offs)).map_err(BioFormatsError::Io)?;
            let mut setup_buf = vec![0u8; setup_length.min(65536)];
            let _ = f.read(&mut setup_buf).map_err(BioFormatsError::Io)?;
            let text = String::from_utf8_lossy(&setup_buf).into_owned();
            parse_sdt_setup(&text)
        } else {
            (1, 1, 256)
        };

        let data_offset: u64 = if data_offs > 0 { data_offs as u64 } else {
            setup_offs + setup_length as u64
        };

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Becker & Hickl SDT".into()));
        meta_map.insert("time_channels".into(), MetadataValue::Int(adc_re as i64));

        // FLIM image: size_x = nx, size_y = ny, size_z = 1 (single time-point),
        // size_c = adc_re (time channels stored as channels), size_t = 1.
        // Pixel data: uint16 histogram values.
        // For simplicity we report adc_re channels each containing one 2D image.
        self.meta = Some(ImageMetadata {
            size_x: nx, size_y: ny, size_z: 1, size_c: adc_re, size_t: 1,
            pixel_type: PixelType::Uint16, bits_per_pixel: 16,
            image_count: adc_re,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false, is_interleaved: false, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.data_offset = data_offset;
        self.n_time = adc_re;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> { self.path = None; self.meta = None; Ok(()) }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        // Each plane is one time-channel slice: size_x × size_y × uint16
        let plane_bytes = (meta.size_x * meta.size_y) as usize * 2;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = 2usize;
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize*bps .. x as usize*bps + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

