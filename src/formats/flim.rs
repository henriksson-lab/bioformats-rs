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

/// Parsed Becker & Hickl SDT header (a partial port of SDTInfo.java covering
/// the fields the reader needs: dimensions, channels, time bins, timepoints,
/// MCS-TA points, count increment, and per-data-block offsets/lengths).
struct SdtInfo {
    width: u32,
    height: u32,
    time_bins: u32,
    channels: u32,
    timepoints: u32,
    mcsta_points: u32,
    incr: u16,
    block_offsets: Vec<u64>,
    block_lengths: Vec<u64>,
}

/// Read the SDT header and measurement-descriptor blocks (SDTInfo.java).
fn parse_sdt_info(f: &mut File, file_len: u64) -> Result<SdtInfo> {
    // bhfileHeader (little-endian).
    let mut hdr = [0u8; 42];
    f.seek(SeekFrom::Start(0)).map_err(BioFormatsError::Io)?;
    f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

    let info_offs = r_u32_le(&hdr, 2) as u64;
    let setup_offs = r_u32_le(&hdr, 8) as u64;
    let setup_length = r_u16_le(&hdr, 12) as usize;
    let data_block_offs = r_u32_le(&hdr, 14) as u64;
    let no_of_data_blocks = r_i16_le(&hdr, 18);
    let meas_desc_block_offs = r_u32_le(&hdr, 20) as u64;
    let no_of_meas_desc_blocks = r_i16_le(&hdr, 24);
    let meas_desc_block_length = r_i16_le(&hdr, 26).max(0) as usize;
    let reserved1 = r_u32_le(&hdr, 34) as usize;
    let _ = info_offs;

    let block_count = if no_of_data_blocks == 0x7fff {
        reserved1
    } else {
        no_of_data_blocks.max(0) as usize
    };

    // Setup text block: parse for X/Y/ADC_RE/SCAN_RX.
    let (mut width, mut height, mut time_bins, mut channels) =
        read_sdt_setup_block(f, setup_offs, setup_length, file_len)?.unwrap_or((1, 1, 256, 1));

    let mut timepoints: u32 = 0;
    let mcsta_points: u32 = 0; // MCS-TA parsing is in the binary setup extension; left 0.
    let mut incr: u16 = 1;

    // Measurement-descriptor block (MeasureInfo) carries authoritative dims.
    if no_of_meas_desc_blocks > 0 && meas_desc_block_length >= 211 && meas_desc_block_offs < file_len
    {
        f.seek(SeekFrom::Start(meas_desc_block_offs))
            .map_err(BioFormatsError::Io)?;
        let mut mb = vec![0u8; 211.min((file_len - meas_desc_block_offs) as usize)];
        f.read_exact(&mut mb).map_err(BioFormatsError::Io)?;
        // Field offsets within MeasureInfo (see SDTInfo.java order):
        //   9 (time) + 11 (date) + 16 (modSerNo) = 36; measMode short at 36.
        let meas_mode = r_i16_le(&mb, 36);
        // adcRE short at offset 36+2 + 6*float(4)=24 + short(2)+float(4)
        //   + float(4)+short(2)+float(4)+float(4)+float(4) ... compute directly:
        // Build cumulative offset from the documented field sequence:
        // measMode(2), cfdLL..cfdHF(4 floats=16), synZC(4), synFD(2), synHF(4),
        // tacR(4), tacG(2), tacOF(4), tacLL(4), tacLH(4), adcRE(2)...
        let mut off = 36usize;
        off += 2; // measMode
        off += 16; // cfdLL,cfdLH,cfdZC,cfdHF
        off += 4; // synZC
        off += 2; // synFD
        off += 4; // synHF
        off += 4; // tacR
        off += 2; // tacG
        off += 4; // tacOF
        off += 4; // tacLL
        off += 4; // tacLH
        let adc_re = r_i16_le(&mb, off);
        off += 2; // adcRE
        off += 2; // ealDE
        off += 2; // ncx
        off += 2; // ncy
        off += 2; // page (ushort)
        off += 4; // colT
        off += 4; // repT
        let stopt = r_i16_le(&mb, off);
        off += 2; // stopt
        off += 1; // overfl (ubyte)
        off += 2; // useMotor
        off += 2; // steps (ushort)
        off += 4; // offset
        off += 2; // dither
        incr = r_u16_le(&mb, off);
        off += 2; // incr
        off += 2; // memBank
        off += 16; // modType
        off += 4; // synTH
        off += 2; // deadTimeComp
        off += 2; // polarityL
        off += 2; // polarityF
        off += 2; // polarityP
        off += 2; // linediv
        off += 2; // accumulate
        off += 4; // flbckY
        off += 4; // flbckX
        off += 4; // bordU
        off += 4; // bordL
        off += 4; // pixTime
        off += 2; // pixClk
        off += 2; // trigger
        let scan_x = r_i32_le(&mb, off);
        off += 4; // scanX
        let scan_y = r_i32_le(&mb, off);
        off += 4; // scanY
        let scan_rx = r_i32_le(&mb, off);
        // (remaining MeasureInfo fields are not needed)

        timepoints = stopt.max(0) as u32;

        if scan_x > 0 {
            width = scan_x as u32;
        }
        if scan_y > 0 {
            height = scan_y as u32;
        }
        if adc_re > 0 {
            time_bins = adc_re as u32;
        }
        if scan_rx > 0 {
            channels = scan_rx as u32;
        }
        if meas_mode == 0 || meas_mode == 1 {
            width = 1;
            height = 1;
        }
        if meas_mode == 13 {
            channels = no_of_meas_desc_blocks.max(1) as u32;
        }
    }

    // Walk the data-block headers to collect offsets and lengths.
    let mut block_offsets = Vec::new();
    let mut block_lengths = Vec::new();
    let mut next = data_block_offs;
    for _ in 0..block_count {
        if next == 0 || next + 20 > file_len {
            break;
        }
        f.seek(SeekFrom::Start(next)).map_err(BioFormatsError::Io)?;
        let mut bh = [0u8; 20];
        f.read_exact(&mut bh).map_err(BioFormatsError::Io)?;
        // blockNo(2), dataOffs(4), nextBlockOffs(4), blockType(2),
        // measDescBlockNo(2), lblockNo(4), blockLength(4)
        let next_block_offs = r_u32_le(&bh, 6) as u64;
        let block_length = r_u32_le(&bh, 16) as u64;
        let block_data_offset = next + 20; // file pointer after header
        if block_data_offset >= file_len {
            break;
        }
        block_offsets.push(block_data_offset);
        block_lengths.push(block_length);

        if next_block_offs == 0 || next_block_offs <= next {
            break;
        }
        next = next_block_offs;
    }

    Ok(SdtInfo {
        width: width.max(1),
        height: height.max(1),
        time_bins: time_bins.max(1),
        channels: channels.max(1),
        timepoints,
        mcsta_points,
        incr,
        block_offsets,
        block_lengths,
    })
}

/// One SDT series corresponds to a single Becker & Hickl data block.
#[derive(Clone)]
struct SdtSeries {
    block: SdtBlock,
    n_time: u32,
    meta: ImageMetadata,
}

pub struct SdtReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    n_time: u32,
    blocks: Vec<SdtBlock>,
    series: Vec<SdtSeries>,
    current_series: usize,
}

impl SdtReader {
    pub fn new() -> Self {
        SdtReader {
            path: None,
            meta: None,
            n_time: 256,
            blocks: Vec::new(),
            series: Vec::new(),
            current_series: 0,
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
            let block = SdtBlock {
                data_offset,
                next_block_offset: 0,
            };
            self.blocks = vec![block.clone()];
            self.series = vec![SdtSeries {
                block,
                n_time: adc_re,
                meta: self.meta.clone().unwrap(),
            }];
            self.current_series = 0;
            self.path = Some(path.to_path_buf());
            return Ok(());
        }

        // Modern Becker & Hickl SDT: parse the full SDTInfo header.
        let info = parse_sdt_info(&mut f, file_len)?;
        if info.block_offsets.is_empty() {
            return Err(BioFormatsError::Format(
                "SDT file does not contain readable data blocks".into(),
            ));
        }

        // Per SDTReader.java: sizeT = timeBins * timepoints, sizeC = channels.
        let timepoints = info.timepoints.max(1);
        let size_t = info.time_bins.saturating_mul(timepoints).max(1);
        let plane_pixels = info.width as u64 * info.height as u64 * 2;
        let base_image_count = size_t.saturating_mul(info.channels);

        // Each data block becomes its own series.
        let mut series = Vec::with_capacity(info.block_offsets.len());
        for (i, &offset) in info.block_offsets.iter().enumerate() {
            let block_len = info.block_lengths.get(i).copied().unwrap_or(0);

            let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
            meta_map.insert(
                "format".into(),
                MetadataValue::String("Becker & Hickl SDT".into()),
            );
            meta_map.insert(
                "time_channels".into(),
                MetadataValue::Int(info.time_bins as i64),
            );
            meta_map.insert("channels".into(), MetadataValue::Int(info.channels as i64));
            meta_map.insert("incr".into(), MetadataValue::Int(info.incr as i64));

            // SDTReader.java: if the block length matches mcstaPoints * planeSize,
            // sizeT becomes mcstaPoints for that series.
            let mut series_t = size_t;
            let mut image_count = base_image_count;
            let expected = plane_pixels.saturating_mul(base_image_count as u64);
            if info.mcsta_points > 0 && block_len != expected {
                if (info.mcsta_points as u64).saturating_mul(plane_pixels) == block_len {
                    series_t = info.mcsta_points;
                    image_count = series_t.saturating_mul(info.channels);
                }
            }

            let meta = ImageMetadata {
                size_x: info.width,
                size_y: info.height,
                size_z: 1,
                size_c: info.channels,
                size_t: series_t,
                pixel_type: PixelType::Uint16,
                bits_per_pixel: 16,
                image_count,
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
            };

            let next_block_offset = info
                .block_offsets
                .get(i + 1)
                .map(|o| o.saturating_sub(20))
                .unwrap_or(0);

            series.push(SdtSeries {
                block: SdtBlock {
                    data_offset: offset,
                    next_block_offset,
                },
                n_time: info.time_bins,
                meta,
            });
        }

        self.blocks = series.iter().map(|s| s.block.clone()).collect();
        self.n_time = series[0].n_time;
        self.meta = Some(series[0].meta.clone());
        self.series = series;
        self.current_series = 0;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.blocks.clear();
        self.series.clear();
        self.current_series = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.series.len().max(1)
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series.len().max(1) {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.current_series = s;
        if let Some(series) = self.series.get(s) {
            self.meta = Some(series.meta.clone());
            self.n_time = series.n_time;
        }
        Ok(())
    }
    fn series(&self) -> usize {
        self.current_series
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
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;
        let plane_bytes = size_x * size_y * 2;
        // Within a series (one data block), planes are laid out channel-major:
        //   no = channel * times + timeBin   (SDTReader.java)
        let times = self.n_time as usize;
        let time_bin = plane_index as usize % times;
        let channel = plane_index as usize / times;

        let block = self
            .series
            .get(self.current_series)
            .map(|s| s.block.clone())
            .or_else(|| self.blocks.first().cloned())
            .ok_or_else(|| BioFormatsError::Format("SDT plane has no data block".into()))?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;

        f.seek(SeekFrom::Start(block.data_offset))
            .map_err(BioFormatsError::Io)?;
        let mut signature = [0u8; 2];
        f.read_exact(&mut signature).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(block.data_offset))
            .map_err(BioFormatsError::Io)?;

        // planeSize for one channel = sizeX * sizeY * times * bpp.
        let channel_plane_size = (size_x * size_y * times * 2) as u64;

        if &signature == b"PK" {
            // For ZIP blocks we cannot random-seek; decode and skip channels by
            // reading from the start of the decompressed stream.
            read_sdt_zip_plane(
                &mut f,
                &block,
                size_x,
                size_y,
                times,
                time_bin,
            )
        } else {
            // Skip to the requested channel within the block.
            f.seek(SeekFrom::Current(channel as i64 * channel_plane_size as i64))
                .map_err(BioFormatsError::Io)?;
            read_sdt_raw_plane(
                &mut f,
                size_x,
                size_y,
                times,
                time_bin,
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
