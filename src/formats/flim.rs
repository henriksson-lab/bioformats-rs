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
use std::io::{Cursor, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue, ModuloAnnotation};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

fn r_i16_le(b: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([b[off], b[off + 1]])
}
fn r_i32_le(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn r_u16_le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn r_u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[derive(Clone, Debug)]
struct SdtBlock {
    data_offset: u64,
    next_block_offset: u64,
}

/// Parse setup text block for image dimensions.
/// Returns (n_x, n_y, adc_re, channels) extracted from the SPC setup keys.
fn parse_sdt_setup(text: &str) -> (u32, u32, u32, u32) {
    let mut nx: u32 = 0;
    let mut ny: u32 = 0;
    let mut adc_re: u32 = 256;
    let mut channels: u32 = 1;
    for line in text.lines() {
        let t = line.trim();
        // Format: "  #SP [SP_FLIM_X,I,128]" or "sp_img_x:128" or "IMG_X 128"
        let low = t.to_ascii_lowercase();
        if low.contains("sp_img_x") || low.contains("img_x") || low.contains("flim_x") {
            if let Some(v) = extract_int(t) {
                if v > 0 {
                    nx = v;
                }
            }
        } else if low.contains("sp_img_y") || low.contains("img_y") || low.contains("flim_y") {
            if let Some(v) = extract_int(t) {
                if v > 0 {
                    ny = v;
                }
            }
        } else if low.contains("sp_adc_re") || low.contains("adc_re") {
            if let Some(v) = extract_int(t) {
                if v > 0 {
                    adc_re = v;
                }
            }
        } else if low.contains("sp_scan_rx") || low.contains("sp_img_rx") {
            if let Some(v) = extract_int(t) {
                if v > 0 {
                    channels = channels.max(v);
                }
            }
        }
    }
    (nx.max(1), ny.max(1), adc_re.max(1), channels.max(1))
}

fn extract_int(s: &str) -> Option<u32> {
    // Find the last sequence of digits in the string
    let mut last: Option<u32> = None;
    let mut acc = String::new();
    for c in s.chars() {
        if c.is_ascii_digit() {
            acc.push(c);
        } else if !acc.is_empty() {
            if let Ok(v) = acc.parse::<u32>() {
                last = Some(v);
            }
            acc.clear();
        }
    }
    if !acc.is_empty() {
        if let Ok(v) = acc.parse::<u32>() {
            last = Some(v);
        }
    }
    last
}

fn read_sdt_raw_plane(
    f: &mut File,
    size_x: usize,
    size_y: usize,
    time_bins: usize,
    time_bin: usize,
    plane_bytes: usize,
) -> Result<Vec<u8>> {
    let row_len = size_x
        .checked_mul(time_bins)
        .and_then(|v| v.checked_mul(2))
        .ok_or_else(|| BioFormatsError::Format("SDT row size overflow".into()))?;
    let mut row = vec![0u8; row_len];
    let mut out = vec![0u8; plane_bytes];
    let sample_offset = time_bin
        .checked_mul(2)
        .ok_or_else(|| BioFormatsError::Format("SDT time-bin offset overflow".into()))?;

    for y in 0..size_y {
        f.read_exact(&mut row).map_err(BioFormatsError::Io)?;
        copy_time_bin_row(
            &row,
            &mut out[y * size_x * 2..(y + 1) * size_x * 2],
            time_bins,
            sample_offset,
        );
    }
    Ok(out)
}

fn read_sdt_zip_plane(
    f: &mut File,
    block: &SdtBlock,
    size_x: usize,
    size_y: usize,
    time_bins: usize,
    time_bin: usize,
) -> Result<Vec<u8>> {
    let compressed_len = compressed_block_len(f, block)?;
    let mut compressed = vec![0u8; compressed_len];
    f.read_exact(&mut compressed).map_err(BioFormatsError::Io)?;
    let payload = zip_deflate_payload(&compressed)?;
    let mut decoder = flate2::read::DeflateDecoder::new(Cursor::new(payload));

    let plane_bytes = size_x
        .checked_mul(size_y)
        .and_then(|v| v.checked_mul(2))
        .ok_or_else(|| BioFormatsError::Format("SDT plane size overflow".into()))?;
    let row_len = size_x
        .checked_mul(time_bins)
        .and_then(|v| v.checked_mul(2))
        .ok_or_else(|| BioFormatsError::Format("SDT row size overflow".into()))?;
    let sample_offset = time_bin
        .checked_mul(2)
        .ok_or_else(|| BioFormatsError::Format("SDT time-bin offset overflow".into()))?;

    let mut row = vec![0u8; row_len];
    let mut out = vec![0u8; plane_bytes];
    for y in 0..size_y {
        decoder
            .read_exact(&mut row)
            .map_err(|e| BioFormatsError::Codec(format!("SDT ZIP decode failed: {e}")))?;
        copy_time_bin_row(
            &row,
            &mut out[y * size_x * 2..(y + 1) * size_x * 2],
            time_bins,
            sample_offset,
        );
    }
    Ok(out)
}

fn copy_time_bin_row(row: &[u8], out: &mut [u8], time_bins: usize, sample_offset: usize) {
    for x in 0..out.len() / 2 {
        let input = (x * time_bins * 2) + sample_offset;
        out[x * 2..x * 2 + 2].copy_from_slice(&row[input..input + 2]);
    }
}

fn compressed_block_len(f: &File, block: &SdtBlock) -> Result<usize> {
    let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
    let end = if block.next_block_offset > block.data_offset {
        block.next_block_offset
    } else {
        file_len
    };
    let len = end
        .checked_sub(block.data_offset)
        .ok_or_else(|| BioFormatsError::Format("SDT compressed block range is invalid".into()))?;
    usize::try_from(len)
        .map_err(|_| BioFormatsError::Format("SDT compressed block is too large".into()))
}

fn zip_deflate_payload(block: &[u8]) -> Result<&[u8]> {
    if block.len() < 30 || &block[..4] != b"PK\x03\x04" {
        return Err(BioFormatsError::Codec(
            "SDT compressed block is not a ZIP local file header".into(),
        ));
    }
    let method = r_u16_le(block, 8);
    if method != 8 {
        return Err(BioFormatsError::Codec(format!(
            "unsupported SDT ZIP compression method {method}"
        )));
    }
    let name_len = r_u16_le(block, 26) as usize;
    let extra_len = r_u16_le(block, 28) as usize;
    let payload_offset = 30usize
        .checked_add(name_len)
        .and_then(|v| v.checked_add(extra_len))
        .ok_or_else(|| BioFormatsError::Format("SDT ZIP header size overflow".into()))?;
    if payload_offset > block.len() {
        return Err(BioFormatsError::Codec(
            "SDT ZIP local header extends beyond data block".into(),
        ));
    }
    Ok(&block[payload_offset..])
}

fn read_sdt_setup_block(
    f: &mut File,
    setup_offs: u64,
    setup_length: usize,
    file_len: u64,
) -> Result<Option<(u32, u32, u32, u32)>> {
    if setup_offs == 0 || setup_length == 0 {
        return Ok(None);
    }
    if setup_offs >= file_len {
        return Err(BioFormatsError::Format(
            "SDT setup offset is beyond end of file".into(),
        ));
    }
    f.seek(SeekFrom::Start(setup_offs))
        .map_err(BioFormatsError::Io)?;
    let mut setup_buf = vec![0u8; setup_length.min(65536)];
    let n = f.read(&mut setup_buf).map_err(BioFormatsError::Io)?;
    setup_buf.truncate(n);
    let text = String::from_utf8_lossy(&setup_buf).into_owned();
    Ok(Some(parse_sdt_setup(&text)))
}

pub struct SdtReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    n_time: u32,
    blocks: Vec<SdtBlock>,
}

impl SdtReader {
    pub fn new() -> Self {
        SdtReader {
            path: None,
            meta: None,
            n_time: 256,
            blocks: Vec::new(),
        }
    }

    fn set_metadata(
        &mut self,
        nx: u32,
        ny: u32,
        adc_re: u32,
        channels: u32,
        mut meta_map: HashMap<String, MetadataValue>,
    ) {
        meta_map
            .entry("format".into())
            .or_insert_with(|| MetadataValue::String("Becker & Hickl SDT".into()));
        meta_map
            .entry("time_channels".into())
            .or_insert_with(|| MetadataValue::Int(adc_re as i64));

        // FLIM image: size_x = nx, size_y = ny, size_z = 1 (single time-point),
        // size_c = spectral/routing channels, size_t = lifetime bins.
        // Pixel data: uint16 histogram values.
        self.meta = Some(ImageMetadata {
            size_x: nx,
            size_y: ny,
            size_z: 1,
            size_c: channels,
            size_t: adc_re,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: adc_re.saturating_mul(channels),
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.n_time = adc_re;
    }
}

impl Default for SdtReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SdtReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sdt") | Some("spc"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() >= 4 && &header[..4] == b"SPC-" {
            return true;
        }
        if header.len() < 42 {
            return false;
        }

        let info_offs = r_u32_le(header, 2);
        let setup_offs = r_u32_le(header, 8);
        let data_block_offs = r_u32_le(header, 14);
        let header_valid = r_u16_le(header, 32);
        matches!(header_valid, 0x1111 | 0x5555)
            && info_offs >= 42
            && setup_offs >= info_offs
            && data_block_offs > setup_offs
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();

        let mut hdr = [0u8; 42];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        if &hdr[..4] == b"SPC-" {
            let setup_offs = r_i16_le(&hdr, 22).max(0) as u64;
            let setup_length = r_i32_le(&hdr, 24).max(0) as usize;
            let data_offs = r_i16_le(&hdr, 28) as i32;
            let (nx, ny, adc_re, channels) =
                read_sdt_setup_block(&mut f, setup_offs, setup_length, file_len)?
                    .unwrap_or((1, 1, 256, 1));
            let data_offset = if data_offs > 0 {
                data_offs as u64
            } else {
                setup_offs + setup_length as u64
            };
            if data_offset >= file_len {
                return Err(BioFormatsError::Format(
                    "SDT data offset is beyond end of file".into(),
                ));
            }

            self.set_metadata(nx, ny, adc_re, channels, HashMap::new());
            self.blocks = vec![SdtBlock {
                data_offset,
                next_block_offset: 0,
            }];
            self.path = Some(path.to_path_buf());
            return Ok(());
        }

        // BH file header layout used by modern Becker & Hickl SDT files:
        // revision i16, info_offs i32, info_length i16, setup_offs i32,
        // setup_length u16, data_block_offs i32, no_of_data_blocks i16,
        // data_block_length i32, meas_desc_block_offs i32, ...
        let info_offs = r_u32_le(&hdr, 2) as u64;
        let info_length = r_u16_le(&hdr, 6) as usize;
        let setup_offs = r_u32_le(&hdr, 8) as u64;
        let setup_length = r_u16_le(&hdr, 12) as usize;
        let data_block_offs = r_u32_le(&hdr, 14) as u64;
        let no_of_data_blocks = r_i16_le(&hdr, 18).max(0) as usize;
        let reserved1 = r_u32_le(&hdr, 34) as usize;
        let block_count = if no_of_data_blocks == 0x7fff {
            reserved1
        } else {
            no_of_data_blocks
        };

        if setup_offs >= file_len || data_block_offs >= file_len {
            return Err(BioFormatsError::Format(
                "SDT header contains offsets beyond end of file".into(),
            ));
        }

        // Read setup text block
        let (nx, ny, adc_re, mut channels) =
            read_sdt_setup_block(&mut f, setup_offs, setup_length, file_len)?
                .unwrap_or((1, 1, 256, 1));

        let mut blocks = Vec::new();
        let mut block_header_offset = data_block_offs;
        for _ in 0..block_count {
            if block_header_offset == 0 || block_header_offset + 22 > file_len {
                break;
            }
            f.seek(SeekFrom::Start(block_header_offset))
                .map_err(BioFormatsError::Io)?;
            let mut block_hdr = [0u8; 22];
            f.read_exact(&mut block_hdr).map_err(BioFormatsError::Io)?;

            let data_offset = r_u32_le(&block_hdr, 2) as u64;
            let next_block_offset = r_u32_le(&block_hdr, 6) as u64;
            if data_offset == 0 || data_offset >= file_len {
                return Err(BioFormatsError::Format(
                    "SDT data block offset is invalid".into(),
                ));
            }
            blocks.push(SdtBlock {
                data_offset,
                next_block_offset,
            });

            if next_block_offset == 0 || next_block_offset <= block_header_offset {
                break;
            }
            block_header_offset = next_block_offset;
        }

        if blocks.is_empty() {
            return Err(BioFormatsError::Format(
                "SDT file does not contain readable data blocks".into(),
            ));
        }
        channels = channels.max(blocks.len() as u32);

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert(
            "format".into(),
            MetadataValue::String("Becker & Hickl SDT".into()),
        );
        meta_map.insert("time_channels".into(), MetadataValue::Int(adc_re as i64));
        meta_map.insert("info_offset".into(), MetadataValue::Int(info_offs as i64));
        meta_map.insert("info_length".into(), MetadataValue::Int(info_length as i64));

        self.set_metadata(nx, ny, adc_re, channels, meta_map);
        self.blocks = blocks;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.blocks.clear();
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
        // Each plane is one lifetime-bin slice: size_x × size_y × uint16.
        let plane_bytes = (meta.size_x * meta.size_y) as usize * 2;
        let time_bin = plane_index % self.n_time;
        let channel = (plane_index / self.n_time) as usize;
        let block_index = channel.min(self.blocks.len().saturating_sub(1));
        let block = self
            .blocks
            .get(block_index)
            .ok_or_else(|| BioFormatsError::Format("SDT plane has no data block".into()))?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;

        f.seek(SeekFrom::Start(block.data_offset))
            .map_err(BioFormatsError::Io)?;
        let mut signature = [0u8; 2];
        f.read_exact(&mut signature).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(block.data_offset))
            .map_err(BioFormatsError::Io)?;

        if &signature == b"PK" {
            read_sdt_zip_plane(
                &mut f,
                block,
                meta.size_x as usize,
                meta.size_y as usize,
                self.n_time as usize,
                time_bin as usize,
            )
        } else {
            read_sdt_raw_plane(
                &mut f,
                meta.size_x as usize,
                meta.size_y as usize,
                self.n_time as usize,
                time_bin as usize,
                plane_bytes,
            )
        }
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
        let bps = 2usize;
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
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

#[derive(Clone, Debug)]
struct LiFlimLayout {
    compression: String,
    datatype: String,
    packing: String,
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    phases: u32,
    frequencies: u32,
    pixel_type: PixelType,
    bits_per_pixel: u8,
    uint12_packed: bool,
}

/// Reader for Lambert Instruments LI-FLIM `.fli` files.
///
/// This ports the bounded Java `LiFlimReader` header contract and raw pixel
/// paths: INI header terminated by `{END}`, version 1.0/2.0 dimensional keys,
/// gzip flag, and UINT12 packed-pixel expansion.
pub struct LiFlimReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
    layout: Option<LiFlimLayout>,
}

impl LiFlimReader {
    pub fn new() -> Self {
        Self {
            path: None,
            meta: None,
            data_offset: 0,
            layout: None,
        }
    }
}

impl Default for LiFlimReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LiFlimReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("fli"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.windows(b"{END}".len()).any(|w| w == b"{END}")
            && std::str::from_utf8(header)
                .map(|s| s.contains("FLIMIMAGE") || s.contains("pixelFormat"))
                .unwrap_or(false)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut file = File::open(path).map_err(BioFormatsError::Io)?;
        let (header, data_offset) = read_liflim_header(&mut file)?;
        let ini = parse_liflim_ini(&header);
        let layout = parse_liflim_layout(&ini)?;

        let mut series_metadata = HashMap::new();
        series_metadata.insert("format".into(), MetadataValue::String("LI-FLIM".into()));
        series_metadata.insert(
            "compression".into(),
            MetadataValue::String(layout.compression.clone()),
        );
        series_metadata.insert(
            "datatype".into(),
            MetadataValue::String(layout.datatype.clone()),
        );
        series_metadata.insert(
            "packing".into(),
            MetadataValue::String(layout.packing.clone()),
        );

        self.meta = Some(ImageMetadata {
            size_x: layout.size_x,
            size_y: layout.size_y,
            size_z: layout.size_z,
            size_c: layout.size_c,
            size_t: layout.size_t,
            pixel_type: layout.pixel_type,
            bits_per_pixel: layout.bits_per_pixel,
            image_count: layout.size_z.saturating_mul(layout.size_t),
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: layout.size_c > 1,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: Some(ModuloAnnotation {
                parent_dimension: "Z".into(),
                modulo_type: "frequency".into(),
                start: 0.0,
                step: layout.size_z as f64 / layout.frequencies.max(1) as f64,
                end: layout.size_z.saturating_sub(1) as f64,
                unit: String::new(),
                labels: Vec::new(),
            }),
            modulo_c: None,
            modulo_t: Some(ModuloAnnotation {
                parent_dimension: "T".into(),
                modulo_type: "phase".into(),
                start: 0.0,
                step: layout.size_t as f64 / layout.phases.max(1) as f64,
                end: layout.size_t.saturating_sub(1) as f64,
                unit: String::new(),
                labels: Vec::new(),
            }),
        });
        self.data_offset = data_offset;
        self.layout = Some(layout);
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
        self.layout = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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
        let layout = self
            .layout
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let plane_bytes = liflim_plane_bytes(meta)?;
        let stored_plane_bytes = if layout.uint12_packed {
            plane_bytes
                .checked_mul(3)
                .and_then(|n| n.checked_div(4))
                .ok_or_else(|| {
                    BioFormatsError::Format("LI-FLIM UINT12 plane size overflow".into())
                })?
        } else {
            plane_bytes
        };
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let mut payload = Vec::new();
        File::open(path)
            .map_err(BioFormatsError::Io)?
            .read_to_end(&mut payload)
            .map_err(BioFormatsError::Io)?;
        let data = payload
            .get(self.data_offset as usize..)
            .ok_or_else(|| BioFormatsError::Format("LI-FLIM data offset is beyond EOF".into()))?;
        let decoded = match layout.compression.as_str() {
            "0" => data.to_vec(),
            "1" => {
                let mut decoder = flate2::read::GzDecoder::new(Cursor::new(data));
                let mut out = Vec::new();
                decoder.read_to_end(&mut out).map_err(|e| {
                    BioFormatsError::Codec(format!("LI-FLIM gzip decode failed: {e}"))
                })?;
                out
            }
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "LI-FLIM unknown compression type: {other}"
                )));
            }
        };

        let offset = (plane_index as usize)
            .checked_mul(stored_plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("LI-FLIM plane offset overflow".into()))?;
        let end = offset
            .checked_add(stored_plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("LI-FLIM plane end overflow".into()))?;
        let stored = decoded.get(offset..end).ok_or_else(|| {
            BioFormatsError::InvalidData(format!(
                "LI-FLIM payload shorter than declared plane {plane_index}"
            ))
        })?;

        if layout.uint12_packed {
            let unpacked = if layout.packing.eq_ignore_ascii_case("msb") {
                liflim_convert12_to_16_msb(stored)
            } else {
                liflim_convert12_to_16_lsb(stored)
            };
            if unpacked.len() != plane_bytes {
                return Err(BioFormatsError::InvalidData(
                    "LI-FLIM UINT12 payload is not a whole number of packed samples".into(),
                ));
            }
            Ok(unpacked)
        } else {
            Ok(stored.to_vec())
        }
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
        crop_full_plane("LI-FLIM", &full, meta, meta.size_c as usize, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

fn read_liflim_header(file: &mut File) -> Result<(String, u64)> {
    let mut bytes = Vec::new();
    file.read_to_end(&mut bytes).map_err(BioFormatsError::Io)?;
    let end = bytes
        .windows(b"{END}".len())
        .position(|w| w == b"{END}")
        .ok_or_else(|| BioFormatsError::Format("LI-FLIM header missing {END}".into()))?;
    let data_offset = end + b"{END}".len();
    let header = String::from_utf8_lossy(&bytes[..end]).into_owned();
    Ok((header, data_offset as u64))
}

fn parse_liflim_ini(text: &str) -> HashMap<String, HashMap<String, String>> {
    let mut tables: HashMap<String, HashMap<String, String>> = HashMap::new();
    let mut current = String::new();
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if let Some(section) = line.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            current = section.trim().to_string();
            tables.entry(current.clone()).or_default();
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        tables
            .entry(current.clone())
            .or_default()
            .insert(key.trim().to_string(), value.trim().to_string());
    }
    tables
}

fn parse_liflim_layout(ini: &HashMap<String, HashMap<String, String>>) -> Result<LiFlimLayout> {
    let version = liflim_find_key(ini, "version").unwrap_or_else(|| "1.0".into());
    let (
        datatype,
        packing,
        channels,
        x_len,
        y_len,
        z_len,
        phases,
        frequencies,
        timestamps,
        compression,
    ) = if version == "2.0" {
        let base = ini.get("").ok_or_else(|| {
            BioFormatsError::Format("LI-FLIM 2.0 header missing default table".into())
        })?;
        let datatype = required_liflim_key(base, "pixelFormat")?;
        (
            datatype.clone(),
            liflim_pixel_format_packing(&datatype),
            "1".into(),
            required_liflim_key(base, "x")?,
            required_liflim_key(base, "y")?,
            required_liflim_key(base, "z")?,
            "1".into(),
            "1".into(),
            required_liflim_key(base, "numberOfFrames")?,
            "0".into(),
        )
    } else {
        let layout = ini.get("FLIMIMAGE: LAYOUT").ok_or_else(|| {
            BioFormatsError::Format("LI-FLIM header missing FLIMIMAGE: LAYOUT table".into())
        })?;
        let info = ini.get("FLIMIMAGE: INFO").ok_or_else(|| {
            BioFormatsError::Format("LI-FLIM header missing FLIMIMAGE: INFO table".into())
        })?;
        (
            required_liflim_key(layout, "datatype")?,
            layout.get("packing").cloned().unwrap_or_default(),
            required_liflim_key(layout, "channels")?,
            required_liflim_key(layout, "x")?,
            required_liflim_key(layout, "y")?,
            required_liflim_key(layout, "z")?,
            required_liflim_key(layout, "phases")?,
            required_liflim_key(layout, "frequencies")?,
            required_liflim_key(layout, "timestamps")?,
            info.get("compression")
                .cloned()
                .unwrap_or_else(|| "0".into()),
        )
    };

    let size_x = parse_liflim_u32("x", &x_len)?;
    let size_y = parse_liflim_u32("y", &y_len)?;
    let channels = parse_liflim_u32("channels", &channels)?;
    let z = parse_liflim_u32("z", &z_len)?;
    let phases = parse_liflim_u32("phases", &phases)?;
    let frequencies = parse_liflim_u32("frequencies", &frequencies)?;
    let timestamps = parse_liflim_u32("timestamps", &timestamps)?;
    let (pixel_type, bits_per_pixel, uint12_packed) =
        liflim_pixel_type(&datatype, packing.as_str())?;

    Ok(LiFlimLayout {
        compression,
        datatype,
        packing,
        size_x,
        size_y,
        size_z: z.saturating_mul(frequencies),
        size_c: channels,
        size_t: timestamps.saturating_mul(phases),
        phases,
        frequencies,
        pixel_type,
        bits_per_pixel,
        uint12_packed,
    })
}

fn required_liflim_key(table: &HashMap<String, String>, key: &str) -> Result<String> {
    table
        .get(key)
        .cloned()
        .ok_or_else(|| BioFormatsError::Format(format!("LI-FLIM header missing {key}")))
}

fn liflim_find_key(ini: &HashMap<String, HashMap<String, String>>, key: &str) -> Option<String> {
    ini.values().find_map(|table| table.get(key).cloned())
}

fn parse_liflim_u32(key: &str, value: &str) -> Result<u32> {
    let parsed = value
        .parse::<u32>()
        .map_err(|_| BioFormatsError::Format(format!("LI-FLIM invalid {key}: {value}")))?;
    if parsed == 0 {
        Err(BioFormatsError::Format(format!(
            "LI-FLIM {key} must be non-zero"
        )))
    } else {
        Ok(parsed)
    }
}

fn liflim_pixel_format_packing(datatype: &str) -> String {
    let lower = datatype.to_ascii_lowercase();
    if lower.contains("msb") {
        "msb".into()
    } else if lower.contains("lsb") {
        "lsb".into()
    } else {
        String::new()
    }
}

fn liflim_pixel_type(datatype: &str, packing: &str) -> Result<(PixelType, u8, bool)> {
    let upper = datatype.to_ascii_uppercase();
    if upper == "UINT8" || liflim_pixel_format_bits(&upper) == Some(8) {
        Ok((PixelType::Uint8, 8, false))
    } else if upper == "INT8" {
        Ok((PixelType::Int8, 8, false))
    } else if upper == "UINT16" || matches!(liflim_pixel_format_bits(&upper), Some(10 | 14 | 16)) {
        Ok((PixelType::Uint16, 16, false))
    } else if upper == "INT16" {
        Ok((PixelType::Int16, 16, false))
    } else if upper == "UINT32" {
        Ok((PixelType::Uint32, 32, false))
    } else if upper == "INT32" {
        Ok((PixelType::Int32, 32, false))
    } else if upper == "REAL32" {
        Ok((PixelType::Float32, 32, false))
    } else if upper == "REAL64" {
        Ok((PixelType::Float64, 64, false))
    } else if upper == "UINT12" || liflim_pixel_format_bits(&upper) == Some(12) {
        Ok((PixelType::Uint16, 12, !packing.is_empty()))
    } else {
        Err(BioFormatsError::Format(format!(
            "LI-FLIM unknown data type: {datatype}"
        )))
    }
}

fn liflim_pixel_format_bits(datatype: &str) -> Option<u8> {
    let digits: String = datatype.chars().filter(|c| c.is_ascii_digit()).collect();
    digits.parse().ok()
}

fn liflim_plane_bytes(meta: &ImageMetadata) -> Result<usize> {
    meta.size_x
        .checked_mul(meta.size_y)
        .and_then(|n| n.checked_mul(meta.size_c))
        .and_then(|n| (n as usize).checked_mul(meta.pixel_type.bytes_per_sample()))
        .ok_or_else(|| BioFormatsError::Format("LI-FLIM plane size overflow".into()))
}

fn liflim_convert12_to_16_lsb(image: &[u8]) -> Vec<u8> {
    let mut image16 = vec![0; image.len() * 4 / 3];
    if image16.len() / 4 != image.len() / 3 {
        return Vec::new();
    }
    for (chunk, out) in image.chunks_exact(3).zip(image16.chunks_exact_mut(4)) {
        out[0] = chunk[0];
        out[1] = chunk[1] & 0x0f;
        out[2] = ((chunk[1] & 0xf0) >> 4) | ((chunk[2] & 0x0f) << 4);
        out[3] = (chunk[2] & 0xf0) >> 4;
    }
    image16
}

fn liflim_convert12_to_16_msb(image: &[u8]) -> Vec<u8> {
    let mut image16 = vec![0; image.len() * 4 / 3];
    if image16.len() / 4 != image.len() / 3 {
        return Vec::new();
    }
    for (chunk, out) in image.chunks_exact(3).zip(image16.chunks_exact_mut(4)) {
        out[0] = ((chunk[0] & 0x0f) << 4) | ((chunk[1] & 0xf0) >> 4);
        out[1] = (chunk[0] & 0xf0) >> 4;
        out[2] = chunk[2];
        out[3] = chunk[1] & 0x0f;
    }
    image16
}

#[cfg(test)]
mod liflim_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_liflim_{nanos}_{name}"))
    }

    fn write_liflim(path: &Path, header: &str, payload: &[u8]) {
        let mut bytes = header.as_bytes().to_vec();
        bytes.extend_from_slice(b"{END}");
        bytes.extend_from_slice(payload);
        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn liflim_reads_uncompressed_version1_planes_and_regions() {
        let path = temp_path("raw.fli");
        let header = "\
[FLIMIMAGE: INFO]
version=1.0
compression=0
[FLIMIMAGE: LAYOUT]
datatype=UINT16
packing=lsb
channels=1
x=3
y=2
z=1
phases=1
frequencies=1
timestamps=2
";
        let mut payload = Vec::new();
        for value in 1u16..=12 {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        write_liflim(&path, header, &payload);

        let mut reader = LiFlimReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(
            (
                meta.size_x,
                meta.size_y,
                meta.size_z,
                meta.size_c,
                meta.size_t
            ),
            (3, 2, 1, 1, 2)
        );
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert_eq!(meta.image_count, 2);

        assert_eq!(reader.open_bytes(1).unwrap(), payload[12..24].to_vec());
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 2, 1).unwrap(),
            vec![8, 0, 9, 0]
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn liflim_expands_uint12_lsb_payloads_like_java() {
        let path = temp_path("uint12.fli");
        let header = "\
[FLIMIMAGE: INFO]
compression=0
[FLIMIMAGE: LAYOUT]
datatype=UINT12
packing=lsb
channels=1
x=2
y=1
z=1
phases=1
frequencies=1
timestamps=1
";
        write_liflim(&path, header, &[0xbc, 0x3a, 0x12]);

        let mut reader = LiFlimReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.metadata().bits_per_pixel, 12);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![0xbc, 0x0a, 0x23, 0x01]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn liflim_uint12_msb_helper_matches_java_byte_order() {
        assert_eq!(
            liflim_convert12_to_16_msb(&[0xab, 0xc1, 0x23]),
            vec![0xbc, 0x0a, 0x23, 0x01]
        );
        assert_eq!(liflim_convert12_to_16_lsb(&[1, 2]), vec![0, 0]);
    }
}
