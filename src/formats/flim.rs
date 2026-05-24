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
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

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
