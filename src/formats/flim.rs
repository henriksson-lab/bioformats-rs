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

const SDT_MAX_CURVE_PAYLOAD_BYTES: u64 = 128 * 1024 * 1024;
const SDT_MAX_ZIP_HEADER_PREVIEW_BYTES: usize = 256;
const SDT_MAX_ZIP_FILE_NAME_PREVIEW_BYTES: usize = 64;

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
fn r_f32_le(b: &[u8], off: usize) -> f32 {
    f32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

#[derive(Clone, Debug)]
struct SdtBlock {
    data_offset: u64,
    next_block_offset: u64,
}

/// Dimensions extracted from the SPC ASCII setup block (SDTInfo.java:534-565).
#[derive(Clone, Copy, Debug)]
struct SdtSetup {
    /// SP_SCAN_X — scanning width.
    scan_x: u32,
    /// SP_SCAN_Y — scanning height.
    scan_y: u32,
    /// SP_ADC_RE — number of time (lifetime) bins.
    adc_re: u32,
    /// SP_SCAN_RX — number of routing/spectral channels.
    scan_rx: u32,
    /// SP_IMG_X — image width (used for measMode 13).
    img_x: u32,
    /// SP_IMG_Y — image height (used for measMode 13).
    img_y: u32,
}

#[derive(Clone, Copy, Debug)]
struct SdtSetupInfo {
    setup: SdtSetup,
    mcsta_points: u32,
}

/// Parse setup text block for image dimensions, mirroring SDTInfo.java's
/// exact key matching (`#SP [SP_SCAN_X,I,...]` etc.).
fn parse_sdt_setup(text: &str) -> SdtSetup {
    let mut s = SdtSetup {
        scan_x: 0,
        scan_y: 0,
        adc_re: 0,
        scan_rx: 0,
        img_x: 0,
        img_y: 0,
    };
    for line in text.lines() {
        let t = line.trim();
        let low = t.to_ascii_lowercase();
        // Match the most specific keys first; the order matters because some
        // keys are substrings of others would-be but the SDT keys are distinct.
        if low.contains("sp_scan_rx") {
            if let Some(v) = extract_int(t) {
                s.scan_rx = v;
            }
        } else if low.contains("sp_scan_x") {
            if let Some(v) = extract_int(t) {
                s.scan_x = v;
            }
        } else if low.contains("sp_scan_y") {
            if let Some(v) = extract_int(t) {
                s.scan_y = v;
            }
        } else if low.contains("sp_adc_re") || low.contains("adc_re") {
            if let Some(v) = extract_int(t) {
                s.adc_re = v;
            }
        } else if low.contains("sp_img_x") {
            if let Some(v) = extract_int(t) {
                s.img_x = v;
            }
        } else if low.contains("sp_img_y") {
            if let Some(v) = extract_int(t) {
                s.img_y = v;
            }
        }
    }
    s
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

#[allow(clippy::too_many_arguments)]
fn read_sdt_raw_plane(
    f: &mut File,
    size_x: usize,
    size_y: usize,
    time_bins: usize,
    time_bin: usize,
    plane_bytes: usize,
    padded_width: usize,
) -> Result<Vec<u8>> {
    // SDTReader.java:176 — rows are stored at a width padded up to a multiple
    // of 4 pixels. Each disk row holds `paddedWidth` pixels worth of decays,
    // but the output plane only contains the unpadded `size_x` columns
    // (SDTReader.java:185-190 drops the padding columns). The caller computes
    // the effective `padded_width`, which may collapse to `size_x` when the
    // block length indicates the data is not actually padded.
    let row_len = padded_width
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

/// Width padded up to a multiple of 4 pixels, per SDTReader.java:176:
/// `paddedWidth = sizeX + ((4 - (sizeX % 4)) % 4)`.
fn padded_width(size_x: usize) -> usize {
    size_x + ((4 - (size_x % 4)) % 4)
}

/// Effective on-disk row width, replicating the padding-removal heuristic in
/// SDTReader.java:176-190. Rows are normally stored at a width padded up to a
/// multiple of 4 pixels, but if the padded plane would overrun the block while
/// the unpadded plane fits, the padding is dropped and `size_x` is used.
fn effective_padded_width(
    size_x: usize,
    size_y: usize,
    times: usize,
    size_c: usize,
    block_size: u64,
) -> usize {
    const BPP: usize = 2;
    let padded = padded_width(size_x);
    let plane_size = (padded * size_y * times * BPP) as u64;
    if padded > size_x
        && plane_size.saturating_mul(size_c as u64) > block_size
        && ((plane_size / padded as u64) * size_x as u64 * size_c as u64) <= block_size
    {
        size_x
    } else {
        padded
    }
}

/// Apply the Becker & Hickl count-increment division (SDTReader.java:278-295).
/// When `incr > 1`, each 16-bit little-endian sample is divided by `incr`;
/// negative values (sign bit set) are treated as unsigned before dividing.
fn apply_sdt_incr(buf: &mut [u8], incr: u16) {
    if incr <= 1 {
        return;
    }
    let incr = incr as i32;
    for chunk in buf.chunks_exact_mut(2) {
        let s = i16::from_le_bytes([chunk[0], chunk[1]]);
        let result: i16 = if s > 0 {
            (s as i32 / incr) as i16
        } else {
            let ii = (s as u16) as i32; // s & 0xffff
            (ii / incr) as i16
        };
        chunk.copy_from_slice(&result.to_le_bytes());
    }
}

#[allow(clippy::too_many_arguments)]
fn read_sdt_zip_plane(
    f: &mut File,
    block: &SdtBlock,
    size_x: usize,
    size_y: usize,
    time_bins: usize,
    time_bin: usize,
    channel: usize,
    padded_width: usize,
) -> Result<Vec<u8>> {
    let compressed_len = compressed_block_len(f, block)?;
    let mut compressed = vec![0u8; compressed_len];
    f.read_exact(&mut compressed).map_err(BioFormatsError::Io)?;
    let payload = zip_deflate_payload(&compressed)?;
    let mut decoder = flate2::read::DeflateDecoder::new(Cursor::new(payload));

    // Output plane is unpadded (size_x columns); disk rows are padded_width
    // wide (SDTReader.java:176,185-190). The effective padded_width is supplied
    // by the caller.
    let plane_bytes = size_x
        .checked_mul(size_y)
        .and_then(|v| v.checked_mul(2))
        .ok_or_else(|| BioFormatsError::Format("SDT plane size overflow".into()))?;
    let row_len = padded_width
        .checked_mul(time_bins)
        .and_then(|v| v.checked_mul(2))
        .ok_or_else(|| BioFormatsError::Format("SDT row size overflow".into()))?;
    let sample_offset = time_bin
        .checked_mul(2)
        .ok_or_else(|| BioFormatsError::Format("SDT time-bin offset overflow".into()))?;

    // Skip preceding channels in the decompressed stream
    // (SDTReader.java:221: codec.skip(channel * planeSize)).
    let channel_plane_size = padded_width
        .checked_mul(size_y)
        .and_then(|v| v.checked_mul(time_bins))
        .and_then(|v| v.checked_mul(2))
        .ok_or_else(|| BioFormatsError::Format("SDT channel plane size overflow".into()))?;
    let mut to_skip = channel
        .checked_mul(channel_plane_size)
        .ok_or_else(|| BioFormatsError::Format("SDT channel skip overflow".into()))?;
    let mut skip_buf = [0u8; 65536];
    while to_skip > 0 {
        let n = to_skip.min(skip_buf.len());
        decoder
            .read_exact(&mut skip_buf[..n])
            .map_err(|e| BioFormatsError::Codec(format!("SDT ZIP channel skip failed: {e}")))?;
        to_skip -= n;
    }

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

fn read_sdt_zip_payload_bounded(
    f: &mut File,
    block: &SdtBlock,
    max_decoded_bytes: u64,
    context: &str,
) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(block.data_offset))
        .map_err(BioFormatsError::Io)?;
    let compressed_len = compressed_block_len(f, block)?;
    let mut compressed = vec![0u8; compressed_len];
    f.read_exact(&mut compressed).map_err(BioFormatsError::Io)?;
    let payload = zip_deflate_payload(&compressed)?;
    let decoder = flate2::read::DeflateDecoder::new(Cursor::new(payload));
    let mut limited = decoder.take(max_decoded_bytes + 1);
    let mut decoded = Vec::new();
    limited
        .read_to_end(&mut decoded)
        .map_err(|e| BioFormatsError::Codec(format!("SDT ZIP {context} decode failed: {e}")))?;
    if decoded.len() as u64 > max_decoded_bytes {
        return Err(BioFormatsError::Codec(format!(
            "SDT ZIP {context} decoded payload exceeds bounded limit of {max_decoded_bytes} bytes"
        )));
    }
    Ok(decoded)
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

fn read_sdt_block_prefix(f: &mut File, block: &SdtBlock, max_len: usize) -> Result<Vec<u8>> {
    f.seek(SeekFrom::Start(block.data_offset))
        .map_err(BioFormatsError::Io)?;
    let block_len = compressed_block_len(f, block)?;
    let mut prefix = vec![0u8; block_len.min(max_len)];
    f.read_exact(&mut prefix).map_err(BioFormatsError::Io)?;
    Ok(prefix)
}

fn insert_sdt_curve_zip_header_metadata(
    meta_map: &mut HashMap<String, MetadataValue>,
    prefix: &[u8],
    block_len: u64,
) {
    meta_map.insert(
        "sdt_curve_compressed_block_length".into(),
        MetadataValue::Int(block_len as i64),
    );
    if !prefix.is_empty() {
        meta_map.insert(
            "sdt_curve_zip_leading_bytes".into(),
            MetadataValue::Bytes(prefix[..prefix.len().min(16)].to_vec()),
        );
    }
    if prefix.len() < 30 || &prefix[..4] != b"PK\x03\x04" {
        return;
    }

    let flags = r_u16_le(prefix, 6);
    let method = r_u16_le(prefix, 8);
    let compressed_size = r_u32_le(prefix, 18);
    let uncompressed_size = r_u32_le(prefix, 22);
    let name_len = r_u16_le(prefix, 26) as usize;
    let extra_len = r_u16_le(prefix, 28) as usize;
    let payload_offset = 30usize
        .checked_add(name_len)
        .and_then(|v| v.checked_add(extra_len));

    meta_map.insert(
        "sdt_curve_zip_method".into(),
        MetadataValue::Int(method as i64),
    );
    meta_map.insert(
        "sdt_curve_zip_flags".into(),
        MetadataValue::Int(flags as i64),
    );
    meta_map.insert(
        "sdt_curve_zip_declared_compressed_size".into(),
        MetadataValue::Int(compressed_size as i64),
    );
    meta_map.insert(
        "sdt_curve_zip_declared_uncompressed_size".into(),
        MetadataValue::Int(uncompressed_size as i64),
    );
    meta_map.insert(
        "sdt_curve_zip_file_name_length".into(),
        MetadataValue::Int(name_len as i64),
    );
    meta_map.insert(
        "sdt_curve_zip_extra_length".into(),
        MetadataValue::Int(extra_len as i64),
    );
    if let Some(payload_offset) = payload_offset {
        meta_map.insert(
            "sdt_curve_zip_payload_offset".into(),
            MetadataValue::Int(payload_offset as i64),
        );
    }

    let name_start = 30;
    let name_end = name_start + name_len.min(SDT_MAX_ZIP_FILE_NAME_PREVIEW_BYTES);
    if name_start < prefix.len() {
        let available_end = name_end.min(prefix.len());
        let preview = String::from_utf8_lossy(&prefix[name_start..available_end]).into_owned();
        meta_map.insert(
            "sdt_curve_zip_file_name_preview".into(),
            MetadataValue::String(preview),
        );
    }
    if name_len > SDT_MAX_ZIP_FILE_NAME_PREVIEW_BYTES || 30 + name_len > prefix.len() {
        meta_map.insert(
            "sdt_curve_zip_file_name_preview_truncated".into(),
            MetadataValue::Bool(true),
        );
    }
}

fn read_sdt_setup_block(
    f: &mut File,
    setup_offs: u64,
    setup_length: usize,
    file_len: u64,
) -> Result<Option<SdtSetup>> {
    Ok(read_sdt_setup_info(f, setup_offs, setup_length, file_len)?.map(|s| s.setup))
}

fn read_sdt_setup_info(
    f: &mut File,
    setup_offs: u64,
    setup_length: usize,
    file_len: u64,
) -> Result<Option<SdtSetupInfo>> {
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
    let mut setup_buf = vec![0u8; setup_length.min(1 << 20)];
    let n = f.read(&mut setup_buf).map_err(BioFormatsError::Io)?;
    setup_buf.truncate(n);
    let text_end = binary_setup_marker(&setup_buf).unwrap_or(setup_buf.len());
    let text = String::from_utf8_lossy(&setup_buf[..text_end]).into_owned();
    Ok(Some(SdtSetupInfo {
        setup: parse_sdt_setup(&text),
        mcsta_points: parse_sdt_mcsta_points(&setup_buf),
    }))
}

fn binary_setup_marker(setup: &[u8]) -> Option<usize> {
    const BINARY_SETUP: &[u8] = b"BIN_PARA_BEGIN:\0";
    setup
        .windows(BINARY_SETUP.len())
        .position(|w| w == BINARY_SETUP)
        .filter(|&pos| pos > 0)
}

fn parse_sdt_mcsta_points(setup: &[u8]) -> u32 {
    // SDTInfo.java looks for BIN_PARA_BEGIN:\0, skips the next four bytes,
    // then treats the following position as the base for BH/SPC binary setup
    // offsets. MCS_TA.points is stored in the MCS image block at offset +8.
    const BINARY_SETUP_LEN: usize = b"BIN_PARA_BEGIN:\0".len();
    let Some(marker) = binary_setup_marker(setup) else {
        return 0;
    };
    let Some(base) = marker
        .checked_add(BINARY_SETUP_LEN)
        .and_then(|v| v.checked_add(4))
    else {
        return 0;
    };

    let Some(binhdrext_offset) = read_u32_at(setup, base + 84).map(|v| v as usize) else {
        return 0;
    };
    if binhdrext_offset == 0 {
        return 0;
    }
    let Some(binhdrext) = base.checked_add(binhdrext_offset) else {
        return 0;
    };
    let Some(mcs_img_offset) = read_u32_at(setup, binhdrext).map(|v| v as usize) else {
        return 0;
    };
    if mcs_img_offset == 0 {
        return 0;
    }
    let Some(mcsta_points_offset) = base
        .checked_add(mcs_img_offset)
        .and_then(|v| v.checked_add(8))
    else {
        return 0;
    };
    read_u16_at(setup, mcsta_points_offset)
        .map(u32::from)
        .unwrap_or(0)
}

fn read_u16_at(buf: &[u8], off: usize) -> Option<u16> {
    let bytes = buf.get(off..off + 2)?;
    Some(u16::from_le_bytes([bytes[0], bytes[1]]))
}

fn read_u32_at(buf: &[u8], off: usize) -> Option<u32> {
    let bytes = buf.get(off..off + 4)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

/// Parsed Becker & Hickl SDT header (a partial port of SDTInfo.java covering
/// the fields the reader needs: dimensions, channels, time bins, timepoints,
/// MCS-TA points, count increment, and per-data-block offsets/lengths).
#[derive(Clone, Copy, Debug)]
struct SdtMeasureInfo {
    meas_mode: i16,
    adc_re: i16,
    stopt: i16,
    incr: u16,
    scan_x: i32,
    scan_y: i32,
    scan_rx: i32,
}

/// MeasStopInfo descriptor sub-block (SDTInfo.java:798-836), present when
/// measDescBlockLength >= 211 + 60. Information collected when the measurement
/// is finished.
#[derive(Clone, Copy, Debug, Default)]
struct SdtMeasStopInfo {
    status: u16,
    flags: u16,
    stop_time: f32,
    cur_step: i32,
    cur_cycle: i32,
    cur_page: i32,
    min_sync_rate: f32,
    min_cfd_rate: f32,
    min_tac_rate: f32,
    min_adc_rate: f32,
    max_sync_rate: f32,
    max_cfd_rate: f32,
    max_tac_rate: f32,
    max_adc_rate: f32,
    reserved1: i32,
    reserved2: f32,
}

/// MeasFCSInfo descriptor sub-block (SDTInfo.java:839-871), present when
/// measDescBlockLength >= 211 + 60 + 38. Information collected when a FIFO
/// measurement is finished; describes the FCS / cross-FCS curve payloads.
#[derive(Clone, Copy, Debug, Default)]
struct SdtMeasFcsInfo {
    chan: u16,
    fcs_decay_calc: u16,
    mt_resol: u32,
    cortime: f32,
    calc_photons: u32,
    fcs_points: i32,
    end_time: f32,
    overruns: u16,
    fcs_type: u16,
    cross_chan: u16,
    mod_: u16,
    cross_mod: u16,
    cross_mt_resol: u32,
}

/// Extended MeasureInfo descriptor sub-block (SDTInfo.java:874-898), present
/// when measDescBlockLength >= 211 + 60 + 38 + 26. Valid for Camera mode or
/// FIFO_IMAGE mode.
#[derive(Clone, Copy, Debug, Default)]
struct SdtExtendedMeasureInfo {
    image_x: i32,
    image_y: i32,
    image_rx: i32,
    image_ry: i32,
    xy_gain: i16,
    master_clock: i16,
    adc_de: i16,
    det_type: i16,
    x_axis: i16,
}

/// MeasHISTInfo descriptor sub-block (SDTInfo.java:900-920), present when
/// measDescBlockLength >= 211 + 60 + 38 + 26 + 24. Extension of MeasFCSInfo for
/// the FIDA, FILDA and MCS histogram curve payloads.
#[derive(Clone, Copy, Debug, Default)]
struct SdtMeasHistInfo {
    fida_time: f32,
    filda_time: f32,
    fida_points: i32,
    filda_points: i32,
    mcs_time: f32,
    mcs_points: i32,
}

struct SdtInfo {
    width: u32,
    height: u32,
    time_bins: u32,
    channels: u32,
    timepoints: u32,
    mcsta_points: u32,
    incr: u16,
    meas_infos: Vec<SdtMeasureInfo>,
    meas_stop_info: Option<SdtMeasStopInfo>,
    meas_fcs_info: Option<SdtMeasFcsInfo>,
    extended_measure_info: Option<SdtExtendedMeasureInfo>,
    meas_hist_info: Option<SdtMeasHistInfo>,
    block_offsets: Vec<u64>,
    block_lengths: Vec<u64>,
    block_types: Vec<u16>,
    block_measure_descs: Vec<i16>,
}

fn parse_sdt_measure_info(mb: &[u8]) -> SdtMeasureInfo {
    // Field offsets within MeasureInfo (see SDTInfo.java order):
    //   9 (time) + 11 (date) + 16 (modSerNo) = 36; measMode short at 36.
    let meas_mode = r_i16_le(mb, 36);
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
    let adc_re = r_i16_le(mb, off);
    off += 2; // adcRE
    off += 2; // ealDE
    off += 2; // ncx
    off += 2; // ncy
    off += 2; // page (ushort)
    off += 4; // colT
    off += 4; // repT
    let stopt = r_i16_le(mb, off);
    off += 2; // stopt
    off += 1; // overfl (ubyte)
    off += 2; // useMotor
    off += 2; // steps (ushort)
    off += 4; // offset
    off += 2; // dither
    let incr = r_u16_le(mb, off);
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
    let scan_x = r_i32_le(mb, off);
    off += 4; // scanX
    let scan_y = r_i32_le(mb, off);
    off += 4; // scanY
    let scan_rx = r_i32_le(mb, off);

    SdtMeasureInfo {
        meas_mode,
        adc_re,
        stopt,
        incr,
        scan_x,
        scan_y,
        scan_rx,
    }
}

/// Parse the MeasStopInfo descriptor sub-block (SDTInfo.java:798-836). `base`
/// is the offset of the sub-block within the descriptor buffer (211).
fn parse_sdt_meas_stop_info(mb: &[u8], base: usize) -> SdtMeasStopInfo {
    let mut off = base;
    let status = r_u16_le(mb, off);
    off += 2;
    let flags = r_u16_le(mb, off);
    off += 2;
    let stop_time = r_f32_le(mb, off);
    off += 4;
    let cur_step = r_i32_le(mb, off);
    off += 4;
    let cur_cycle = r_i32_le(mb, off);
    off += 4;
    let cur_page = r_i32_le(mb, off);
    off += 4;
    let min_sync_rate = r_f32_le(mb, off);
    off += 4;
    let min_cfd_rate = r_f32_le(mb, off);
    off += 4;
    let min_tac_rate = r_f32_le(mb, off);
    off += 4;
    let min_adc_rate = r_f32_le(mb, off);
    off += 4;
    let max_sync_rate = r_f32_le(mb, off);
    off += 4;
    let max_cfd_rate = r_f32_le(mb, off);
    off += 4;
    let max_tac_rate = r_f32_le(mb, off);
    off += 4;
    let max_adc_rate = r_f32_le(mb, off);
    off += 4;
    let reserved1 = r_i32_le(mb, off);
    off += 4;
    let reserved2 = r_f32_le(mb, off);
    SdtMeasStopInfo {
        status,
        flags,
        stop_time,
        cur_step,
        cur_cycle,
        cur_page,
        min_sync_rate,
        min_cfd_rate,
        min_tac_rate,
        min_adc_rate,
        max_sync_rate,
        max_cfd_rate,
        max_tac_rate,
        max_adc_rate,
        reserved1,
        reserved2,
    }
}

/// Parse the MeasFCSInfo descriptor sub-block (SDTInfo.java:839-871). `base`
/// is the offset of the sub-block within the descriptor buffer (271).
fn parse_sdt_meas_fcs_info(mb: &[u8], base: usize) -> SdtMeasFcsInfo {
    let mut off = base;
    let chan = r_u16_le(mb, off);
    off += 2;
    let fcs_decay_calc = r_u16_le(mb, off);
    off += 2;
    let mt_resol = r_u32_le(mb, off);
    off += 4;
    let cortime = r_f32_le(mb, off);
    off += 4;
    let calc_photons = r_u32_le(mb, off);
    off += 4;
    let fcs_points = r_i32_le(mb, off);
    off += 4;
    let end_time = r_f32_le(mb, off);
    off += 4;
    let overruns = r_u16_le(mb, off);
    off += 2;
    let fcs_type = r_u16_le(mb, off);
    off += 2;
    let cross_chan = r_u16_le(mb, off);
    off += 2;
    let mod_ = r_u16_le(mb, off);
    off += 2;
    let cross_mod = r_u16_le(mb, off);
    off += 2;
    let cross_mt_resol = r_u32_le(mb, off);
    SdtMeasFcsInfo {
        chan,
        fcs_decay_calc,
        mt_resol,
        cortime,
        calc_photons,
        fcs_points,
        end_time,
        overruns,
        fcs_type,
        cross_chan,
        mod_,
        cross_mod,
        cross_mt_resol,
    }
}

/// Parse the extended MeasureInfo descriptor sub-block (SDTInfo.java:874-898).
/// `base` is the offset of the sub-block within the descriptor buffer (309).
fn parse_sdt_extended_measure_info(mb: &[u8], base: usize) -> SdtExtendedMeasureInfo {
    let mut off = base;
    let image_x = r_i32_le(mb, off);
    off += 4;
    let image_y = r_i32_le(mb, off);
    off += 4;
    let image_rx = r_i32_le(mb, off);
    off += 4;
    let image_ry = r_i32_le(mb, off);
    off += 4;
    let xy_gain = r_i16_le(mb, off);
    off += 2;
    let master_clock = r_i16_le(mb, off);
    off += 2;
    let adc_de = r_i16_le(mb, off);
    off += 2;
    let det_type = r_i16_le(mb, off);
    off += 2;
    let x_axis = r_i16_le(mb, off);
    SdtExtendedMeasureInfo {
        image_x,
        image_y,
        image_rx,
        image_ry,
        xy_gain,
        master_clock,
        adc_de,
        det_type,
        x_axis,
    }
}

/// Parse the MeasHISTInfo descriptor sub-block (SDTInfo.java:900-920). `base`
/// is the offset of the sub-block within the descriptor buffer (335).
fn parse_sdt_meas_hist_info(mb: &[u8], base: usize) -> SdtMeasHistInfo {
    let mut off = base;
    let fida_time = r_f32_le(mb, off);
    off += 4;
    let filda_time = r_f32_le(mb, off);
    off += 4;
    let fida_points = r_i32_le(mb, off);
    off += 4;
    let filda_points = r_i32_le(mb, off);
    off += 4;
    let mcs_time = r_f32_le(mb, off);
    off += 4;
    let mcs_points = r_i32_le(mb, off);
    SdtMeasHistInfo {
        fida_time,
        filda_time,
        fida_points,
        filda_points,
        mcs_time,
        mcs_points,
    }
}

fn sdt_curve_variant(meas_mode: i16) -> Option<&'static str> {
    match meas_mode {
        3 => Some("FCS"),
        4 => Some("FIDA"),
        5 => Some("FILDA"),
        0 | 1 | 2 | 6 => Some("MCS/non-image curve"),
        _ => None,
    }
}

fn insert_sdt_measurement_metadata(
    meta_map: &mut HashMap<String, MetadataValue>,
    meas: &SdtMeasureInfo,
) {
    meta_map.insert(
        "sdt_measurement_mode".into(),
        MetadataValue::Int(meas.meas_mode as i64),
    );
    meta_map.insert(
        "sdt_measurement_adc_re".into(),
        MetadataValue::Int(meas.adc_re as i64),
    );
    meta_map.insert(
        "sdt_measurement_stopt".into(),
        MetadataValue::Int(meas.stopt as i64),
    );
    meta_map.insert(
        "sdt_measurement_incr".into(),
        MetadataValue::Int(meas.incr as i64),
    );
    meta_map.insert(
        "sdt_measurement_scan_x".into(),
        MetadataValue::Int(meas.scan_x as i64),
    );
    meta_map.insert(
        "sdt_measurement_scan_y".into(),
        MetadataValue::Int(meas.scan_y as i64),
    );
    meta_map.insert(
        "sdt_measurement_scan_rx".into(),
        MetadataValue::Int(meas.scan_rx as i64),
    );
}

/// Emit MeasStopInfo.* metadata with the exact Java key names
/// (SDTInfo.java:818-835).
fn insert_sdt_meas_stop_info_metadata(
    meta_map: &mut HashMap<String, MetadataValue>,
    s: &SdtMeasStopInfo,
) {
    let p = "MeasStopInfo.";
    meta_map.insert(format!("{p}status"), MetadataValue::Int(s.status as i64));
    meta_map.insert(format!("{p}flags"), MetadataValue::Int(s.flags as i64));
    meta_map.insert(
        format!("{p}stopTime"),
        MetadataValue::Float(s.stop_time as f64),
    );
    meta_map.insert(format!("{p}curStep"), MetadataValue::Int(s.cur_step as i64));
    meta_map.insert(
        format!("{p}curCycle"),
        MetadataValue::Int(s.cur_cycle as i64),
    );
    meta_map.insert(format!("{p}curPage"), MetadataValue::Int(s.cur_page as i64));
    meta_map.insert(
        format!("{p}minSyncRate"),
        MetadataValue::Float(s.min_sync_rate as f64),
    );
    meta_map.insert(
        format!("{p}minCfdRate"),
        MetadataValue::Float(s.min_cfd_rate as f64),
    );
    meta_map.insert(
        format!("{p}minTacRate"),
        MetadataValue::Float(s.min_tac_rate as f64),
    );
    meta_map.insert(
        format!("{p}minAdcRate"),
        MetadataValue::Float(s.min_adc_rate as f64),
    );
    meta_map.insert(
        format!("{p}maxSyncRate"),
        MetadataValue::Float(s.max_sync_rate as f64),
    );
    meta_map.insert(
        format!("{p}maxCfdRate"),
        MetadataValue::Float(s.max_cfd_rate as f64),
    );
    meta_map.insert(
        format!("{p}maxTacRate"),
        MetadataValue::Float(s.max_tac_rate as f64),
    );
    meta_map.insert(
        format!("{p}maxAdcRate"),
        MetadataValue::Float(s.max_adc_rate as f64),
    );
    meta_map.insert(
        format!("{p}reserved1"),
        MetadataValue::Int(s.reserved1 as i64),
    );
    meta_map.insert(
        format!("{p}reserved2"),
        MetadataValue::Float(s.reserved2 as f64),
    );
}

/// Emit MeasFCSInfo.* metadata with the exact Java key names
/// (SDTInfo.java:856-870).
fn insert_sdt_meas_fcs_info_metadata(
    meta_map: &mut HashMap<String, MetadataValue>,
    f: &SdtMeasFcsInfo,
) {
    let p = "MeasFCSInfo.";
    meta_map.insert(format!("{p}chan"), MetadataValue::Int(f.chan as i64));
    meta_map.insert(
        format!("{p}fcsDecayCalc"),
        MetadataValue::Int(f.fcs_decay_calc as i64),
    );
    meta_map.insert(format!("{p}mtResol"), MetadataValue::Int(f.mt_resol as i64));
    meta_map.insert(
        format!("{p}cortime"),
        MetadataValue::Float(f.cortime as f64),
    );
    meta_map.insert(
        format!("{p}calcPhotons"),
        MetadataValue::Int(f.calc_photons as i64),
    );
    meta_map.insert(
        format!("{p}fcsPoints"),
        MetadataValue::Int(f.fcs_points as i64),
    );
    meta_map.insert(
        format!("{p}endTime"),
        MetadataValue::Float(f.end_time as f64),
    );
    meta_map.insert(format!("{p}overruns"), MetadataValue::Int(f.overruns as i64));
    meta_map.insert(format!("{p}fcsType"), MetadataValue::Int(f.fcs_type as i64));
    meta_map.insert(
        format!("{p}crossChan"),
        MetadataValue::Int(f.cross_chan as i64),
    );
    meta_map.insert(format!("{p}mod"), MetadataValue::Int(f.mod_ as i64));
    meta_map.insert(
        format!("{p}crossMod"),
        MetadataValue::Int(f.cross_mod as i64),
    );
    // SDTInfo.java stores crossMtResol via Float.valueOf despite the field being
    // an unsigned long; mirror the resulting numeric value.
    meta_map.insert(
        format!("{p}crossMtResol"),
        MetadataValue::Float(f.cross_mt_resol as f64),
    );
}

/// Emit the extended MeasureInfo.* metadata with the exact Java key names
/// (SDTInfo.java:887-896).
fn insert_sdt_extended_measure_info_metadata(
    meta_map: &mut HashMap<String, MetadataValue>,
    e: &SdtExtendedMeasureInfo,
) {
    let p = "MeasureInfo.";
    meta_map.insert(format!("{p}imageX"), MetadataValue::Int(e.image_x as i64));
    meta_map.insert(format!("{p}imageY"), MetadataValue::Int(e.image_y as i64));
    meta_map.insert(format!("{p}imageRX"), MetadataValue::Int(e.image_rx as i64));
    meta_map.insert(format!("{p}imageRY"), MetadataValue::Int(e.image_ry as i64));
    meta_map.insert(format!("{p}xyGain"), MetadataValue::Int(e.xy_gain as i64));
    meta_map.insert(
        format!("{p}masterClock"),
        MetadataValue::Int(e.master_clock as i64),
    );
    meta_map.insert(format!("{p}adcDE"), MetadataValue::Int(e.adc_de as i64));
    meta_map.insert(format!("{p}detType"), MetadataValue::Int(e.det_type as i64));
    meta_map.insert(format!("{p}xAxis"), MetadataValue::Int(e.x_axis as i64));
}

/// Emit MeasHISTInfo.* metadata with the exact Java key names
/// (SDTInfo.java:911-918).
fn insert_sdt_meas_hist_info_metadata(
    meta_map: &mut HashMap<String, MetadataValue>,
    h: &SdtMeasHistInfo,
) {
    let p = "MeasHISTInfo.";
    meta_map.insert(
        format!("{p}fidaTime"),
        MetadataValue::Float(h.fida_time as f64),
    );
    meta_map.insert(
        format!("{p}fildaTime"),
        MetadataValue::Float(h.filda_time as f64),
    );
    meta_map.insert(
        format!("{p}fidaPoints"),
        MetadataValue::Int(h.fida_points as i64),
    );
    meta_map.insert(
        format!("{p}fildaPoints"),
        MetadataValue::Int(h.filda_points as i64),
    );
    meta_map.insert(
        format!("{p}mcsTime"),
        MetadataValue::Float(h.mcs_time as f64),
    );
    meta_map.insert(
        format!("{p}mcsPoints"),
        MetadataValue::Int(h.mcs_points as i64),
    );
}

/// Read the SDT header and measurement-descriptor blocks (SDTInfo.java).
///
/// bhfileHeader layout (little-endian, SDTInfo.java:441-456):
///   revision        i16 @0
///   infoOffs        i32 @2
///   infoLength      i16 @6
///   setupOffs       i32 @8
///   setupLength     u16 @12
///   dataBlockOffs   i32 @14
///   noOfDataBlocks  i16 @18
///   dataBlockLength i32 @20
///   measDescBlockOffs i32 @24
///   noOfMeasDescBlocks i16 @28
///   measDescBlockLength i16 @30
///   headerValid     u16 @32
///   reserved1       u32 @34
fn parse_sdt_info(f: &mut File, file_len: u64) -> Result<SdtInfo> {
    let mut hdr = [0u8; 42];
    f.seek(SeekFrom::Start(0)).map_err(BioFormatsError::Io)?;
    f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

    let setup_offs = r_u32_le(&hdr, 8) as u64;
    let setup_length = r_u16_le(&hdr, 12) as usize;
    let data_block_offs = r_u32_le(&hdr, 14) as u64;
    let no_of_data_blocks = r_i16_le(&hdr, 18);
    let meas_desc_block_offs = r_u32_le(&hdr, 24) as u64;
    let no_of_meas_desc_blocks = r_i16_le(&hdr, 28);
    let meas_desc_block_length = r_i16_le(&hdr, 30).max(0) as usize;
    let reserved1 = r_u32_le(&hdr, 34) as usize;

    let block_count = if no_of_data_blocks == 0x7fff {
        reserved1
    } else {
        no_of_data_blocks.max(0) as usize
    };

    // Setup text block: parse for SCAN_X/Y, ADC_RE, SCAN_RX, IMG_X/Y.
    let setup_info =
        read_sdt_setup_info(f, setup_offs, setup_length, file_len)?.unwrap_or(SdtSetupInfo {
            setup: SdtSetup {
                scan_x: 0,
                scan_y: 0,
                adc_re: 256,
                scan_rx: 0,
                img_x: 0,
                img_y: 0,
            },
            mcsta_points: 0,
        });
    let setup = setup_info.setup;
    let mut width: u32 = setup.scan_x.max(1);
    let mut height: u32 = setup.scan_y.max(1);
    let mut time_bins: u32 = setup.adc_re.max(1);
    let mut channels: u32 = setup.scan_rx.max(1);

    let mut timepoints: u32 = 0;
    let mcsta_points: u32 = setup_info.mcsta_points;
    let mut incr: u16 = 1;
    let mut meas_infos = Vec::new();
    let mut meas_stop_info: Option<SdtMeasStopInfo> = None;
    let mut meas_fcs_info: Option<SdtMeasFcsInfo> = None;
    let mut extended_measure_info: Option<SdtExtendedMeasureInfo> = None;
    let mut meas_hist_info: Option<SdtMeasHistInfo> = None;

    // Descriptor sub-block presence flags, keyed off measDescBlockLength
    // (SDTInfo.java:643-647). Each successive sub-block follows MeasureInfo (211
    // bytes), then MeasStopInfo (+60), MeasFCSInfo (+38), extended MeasureInfo
    // (+26) and MeasHISTInfo (+24).
    let has_meas_stop_info = meas_desc_block_length >= 211 + 60;
    let has_meas_fcs_info = meas_desc_block_length >= 211 + 60 + 38;
    let has_extended_measure_info = meas_desc_block_length >= 211 + 60 + 38 + 26;
    let has_meas_hist_info = meas_desc_block_length >= 211 + 60 + 38 + 26 + 24;

    // Measurement-descriptor block (MeasureInfo) carries authoritative dims.
    if no_of_meas_desc_blocks > 0
        && meas_desc_block_length >= 211
        && meas_desc_block_offs < file_len
    {
        for i in 0..no_of_meas_desc_blocks.max(0) as usize {
            let Some(desc_offs) =
                meas_desc_block_offs.checked_add((i * meas_desc_block_length) as u64)
            else {
                break;
            };
            if desc_offs + 211 > file_len {
                break;
            }
            // Read the full descriptor so the FCS / HIST curve sub-blocks of the
            // first descriptor can be decoded; bound the read to the file end.
            let want = meas_desc_block_length.max(211);
            let avail = (file_len - desc_offs) as usize;
            let read_len = want.min(avail);
            f.seek(SeekFrom::Start(desc_offs))
                .map_err(BioFormatsError::Io)?;
            let mut mb = vec![0u8; read_len];
            f.read_exact(&mut mb).map_err(BioFormatsError::Io)?;
            meas_infos.push(parse_sdt_measure_info(&mb));

            // SDTInfo.java reads the MeasStopInfo / MeasFCSInfo / extended
            // MeasureInfo / MeasHISTInfo sub-blocks once, immediately after the
            // first MeasureInfo. Mirror that by parsing them from the first
            // descriptor buffer when present and fully available.
            if i == 0 {
                if has_meas_stop_info && mb.len() >= 211 + 60 {
                    meas_stop_info = Some(parse_sdt_meas_stop_info(&mb, 211));
                }
                if has_meas_fcs_info && mb.len() >= 211 + 60 + 38 {
                    meas_fcs_info = Some(parse_sdt_meas_fcs_info(&mb, 211 + 60));
                }
                if has_extended_measure_info && mb.len() >= 211 + 60 + 38 + 26 {
                    extended_measure_info =
                        Some(parse_sdt_extended_measure_info(&mb, 211 + 60 + 38));
                }
                if has_meas_hist_info && mb.len() >= 211 + 60 + 38 + 26 + 24 {
                    meas_hist_info = Some(parse_sdt_meas_hist_info(&mb, 211 + 60 + 38 + 26));
                }
            }
        }
    }

    if let Some(first) = meas_infos.first() {
        timepoints = first.stopt.max(0) as u32;
        incr = first.incr;

        if first.scan_x > 0 {
            width = first.scan_x as u32;
        }
        if first.scan_y > 0 {
            height = first.scan_y as u32;
        }
        if first.adc_re > 0 {
            time_bins = first.adc_re as u32;
        }
        if first.scan_rx > 0 {
            channels = first.scan_rx as u32;
        }
        if first.meas_mode == 0 || first.meas_mode == 1 {
            width = 1;
            height = 1;
        }
        // measMode 13 (FLIM imaging): width/height come from SP_IMG_X/Y in the
        // ASCII setup, and each measurement-descriptor block is a channel
        // (SDTInfo.java:790-793).
        if first.meas_mode == 13 {
            width = setup.img_x.max(1);
            height = setup.img_y.max(1);
            channels = no_of_meas_desc_blocks.max(1) as u32;
        }
    }

    // Walk the data-block headers to collect offsets and lengths.
    // BHFileBlockHeader is 22 bytes (SDTInfo.java:930-940):
    //   blockNo(2), dataOffs(4), nextBlockOffs(4), blockType(2),
    //   measDescBlockNo(2), lblockNo(4), blockLength(4).
    // The pixel data for each block starts immediately after its 22-byte
    // header; the next header is located via nextBlockOffs.
    let mut block_offsets = Vec::new();
    let mut block_lengths = Vec::new();
    let mut block_types = Vec::new();
    let mut block_measure_descs = Vec::new();
    let mut next = data_block_offs;
    for _ in 0..block_count {
        if next == 0 || next + 22 > file_len {
            break;
        }
        f.seek(SeekFrom::Start(next)).map_err(BioFormatsError::Io)?;
        let mut bh = [0u8; 22];
        f.read_exact(&mut bh).map_err(BioFormatsError::Io)?;
        let next_block_offs = r_u32_le(&bh, 6) as u64;
        let block_type = r_u16_le(&bh, 10);
        let meas_desc_block_no = r_i16_le(&bh, 12);
        let block_length = r_u32_le(&bh, 18) as u64;
        let block_data_offset = next + 22; // file pointer after header
        if block_data_offset > file_len || (block_data_offset == file_len && block_length > 0) {
            break;
        }
        block_offsets.push(block_data_offset);
        block_lengths.push(block_length);
        block_types.push(block_type);
        block_measure_descs.push(meas_desc_block_no);

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
        meas_infos,
        meas_stop_info,
        meas_fcs_info,
        extended_measure_info,
        meas_hist_info,
        block_offsets,
        block_lengths,
        block_types,
        block_measure_descs,
    })
}

/// One SDT series corresponds to a single Becker & Hickl data block.
#[derive(Clone)]
struct SdtSeries {
    block: SdtBlock,
    n_time: u32,
    meta: ImageMetadata,
    unsupported_layout_reason: Option<String>,
    raw_curve_payload_len: Option<usize>,
    compressed_curve_payload_len: Option<usize>,
    /// Becker & Hickl block length (`info.allBlockLengths[series]`), needed by
    /// openBytes to replicate the SDTReader.java:185-190 padding heuristic.
    block_length: u64,
    /// `info.incr` count increment; values > 1 are divided out (SDTReader.java:278-295).
    incr: u16,
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
            let setup = read_sdt_setup_block(&mut f, setup_offs, setup_length, file_len)?
                .unwrap_or(SdtSetup {
                    scan_x: 1,
                    scan_y: 1,
                    adc_re: 256,
                    scan_rx: 1,
                    img_x: 0,
                    img_y: 0,
                });
            let nx = setup.scan_x.max(1);
            let ny = setup.scan_y.max(1);
            let adc_re = setup.adc_re.max(1);
            let channels = setup.scan_rx.max(1);
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
                unsupported_layout_reason: None,
                raw_curve_payload_len: None,
                compressed_curve_payload_len: None,
                block_length: 0,
                incr: 1,
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
        let disk_plane_bytes = padded_width(info.width as usize) as u64 * info.height as u64 * 2;
        let base_image_count = size_t.saturating_mul(info.channels);

        // Each data block becomes its own series.
        let mut series = Vec::with_capacity(info.block_offsets.len());
        for (i, &offset) in info.block_offsets.iter().enumerate() {
            let block_len = info.block_lengths.get(i).copied().unwrap_or(0);
            let block_type = info.block_types.get(i).copied().unwrap_or(0);
            let meas_desc_no = info.block_measure_descs.get(i).copied().unwrap_or(-1);
            let meas_info = usize::try_from(meas_desc_no)
                .ok()
                .and_then(|idx| info.meas_infos.get(idx))
                .or_else(|| info.meas_infos.first());

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
            meta_map.insert(
                "sdt_block_type".into(),
                MetadataValue::Int(block_type as i64),
            );
            if meas_desc_no >= 0 {
                meta_map.insert(
                    "sdt_measurement_descriptor_index".into(),
                    MetadataValue::Int(meas_desc_no as i64),
                );
            }
            if let Some(meas) = meas_info {
                insert_sdt_measurement_metadata(&mut meta_map, meas);
                if let Some(variant) = sdt_curve_variant(meas.meas_mode) {
                    meta_map.insert(
                        "sdt_curve_variant".into(),
                        MetadataValue::String(variant.into()),
                    );
                }
            }
            // File-level descriptor sub-blocks (MeasStopInfo / MeasFCSInfo /
            // extended MeasureInfo / MeasHISTInfo) describe the non-image
            // FCS/FIDA/FILDA/MCS curve payloads; expose them on every series
            // with the exact Java key names.
            if let Some(stop) = info.meas_stop_info.as_ref() {
                insert_sdt_meas_stop_info_metadata(&mut meta_map, stop);
            }
            if let Some(fcs) = info.meas_fcs_info.as_ref() {
                insert_sdt_meas_fcs_info_metadata(&mut meta_map, fcs);
            }
            if let Some(ext) = info.extended_measure_info.as_ref() {
                insert_sdt_extended_measure_info_metadata(&mut meta_map, ext);
            }
            if let Some(hist) = info.meas_hist_info.as_ref() {
                insert_sdt_meas_hist_info_metadata(&mut meta_map, hist);
            }

            let mut sig = [0u8; 2];
            let is_zip_block = f
                .seek(SeekFrom::Start(offset))
                .and_then(|_| f.read_exact(&mut sig))
                .map(|_| &sig == b"PK")
                .unwrap_or(false);

            // SDTReader.java: if the block length matches mcstaPoints * planeSize,
            // sizeT becomes mcstaPoints for that series. planeSize is the
            // padded on-disk stride, while exposed planes are cropped back to
            // sizeX columns when reading.
            let mut series_t = size_t;
            let mut image_count = base_image_count;
            let mut layout = "image";
            let mut unsupported_layout_reason = None;
            let mut raw_curve_payload_len = None;
            let mut compressed_curve_payload_len = None;
            let mut series_size_x = info.width;
            let mut series_size_y = info.height;
            let mut series_size_c = info.channels;
            let mut series_pixel_type = PixelType::Uint16;
            let mut series_bits_per_pixel = 16;
            let expected = disk_plane_bytes.saturating_mul(base_image_count as u64);
            let next_block_offset = info
                .block_offsets
                .get(i + 1)
                .map(|o| o.saturating_sub(22))
                .unwrap_or(0);
            let probe_block = SdtBlock {
                data_offset: offset,
                next_block_offset,
            };
            if info.mcsta_points > 0 && block_len != expected {
                if (info.mcsta_points as u64).saturating_mul(disk_plane_bytes) == block_len {
                    series_t = info.mcsta_points;
                    image_count = series_t.saturating_mul(info.channels);
                    layout = "mcs_ta";
                }
            }
            if layout == "image" {
                if let Some(meas) = meas_info {
                    if let Some(variant) = sdt_curve_variant(meas.meas_mode) {
                        if is_zip_block {
                            if let Ok(prefix) = read_sdt_block_prefix(
                                &mut f,
                                &probe_block,
                                SDT_MAX_ZIP_HEADER_PREVIEW_BYTES,
                            ) {
                                insert_sdt_curve_zip_header_metadata(
                                    &mut meta_map,
                                    &prefix,
                                    block_len,
                                );
                            }
                            match read_sdt_zip_payload_bounded(
                                &mut f,
                                &probe_block,
                                SDT_MAX_CURVE_PAYLOAD_BYTES,
                                "curve payload",
                            ) {
                                Ok(decoded) if decoded.is_empty() => {
                                    series_size_x = 0;
                                    series_size_y = 1;
                                    series_size_c = 1;
                                    series_t = 0;
                                    image_count = 0;
                                    compressed_curve_payload_len = Some(0);
                                    layout = "empty_curve";
                                    meta_map.insert(
                                        "sdt_curve_payload_encoding".into(),
                                        MetadataValue::String("zip_deflate_raw_u16_le".into()),
                                    );
                                    meta_map.insert(
                                        "sdt_curve_sample_count".into(),
                                        MetadataValue::Int(0),
                                    );
                                    meta_map.insert(
                                        "sdt_curve_compressed_block_length".into(),
                                        MetadataValue::Int(block_len as i64),
                                    );
                                }
                                Ok(decoded)
                                    if decoded.len() % 2 == 0
                                        && decoded.len() / 2 <= u32::MAX as usize =>
                                {
                                    let sample_count = (decoded.len() / 2) as u32;
                                    series_size_x = sample_count;
                                    series_size_y = 1;
                                    series_size_c = 1;
                                    series_t = 1;
                                    image_count = 1;
                                    compressed_curve_payload_len = Some(decoded.len());
                                    layout = "zip_u16_curve";
                                    meta_map.insert(
                                        "sdt_curve_payload_encoding".into(),
                                        MetadataValue::String("zip_deflate_raw_u16_le".into()),
                                    );
                                    meta_map.insert(
                                        "sdt_curve_sample_count".into(),
                                        MetadataValue::Int(sample_count as i64),
                                    );
                                    meta_map.insert(
                                        "sdt_curve_compressed_block_length".into(),
                                        MetadataValue::Int(block_len as i64),
                                    );
                                }
                                Ok(decoded) => {
                                    let byte_count = decoded.len();
                                    if byte_count <= u32::MAX as usize {
                                        series_size_x = byte_count as u32;
                                        series_size_y = 1;
                                        series_size_c = 1;
                                        series_t = 1;
                                        image_count = 1;
                                        series_pixel_type = PixelType::Uint8;
                                        series_bits_per_pixel = 8;
                                        compressed_curve_payload_len = Some(byte_count);
                                        layout = "zip_odd_byte_curve";
                                        meta_map.insert(
                                            "sdt_curve_payload_encoding".into(),
                                            MetadataValue::String(
                                                "zip_deflate_raw_bytes_odd_length".into(),
                                            ),
                                        );
                                        meta_map.insert(
                                            "sdt_curve_byte_count".into(),
                                            MetadataValue::Int(byte_count as i64),
                                        );
                                        meta_map.insert(
                                            "sdt_curve_payload_diagnostic".into(),
                                            MetadataValue::String(format!(
                                                "non-image Becker & Hickl SDT/SPC {variant} curve measurement mode {} decoded to odd byte count {}; preserving raw bytes as uint8 curve payload",
                                                meas.meas_mode, byte_count
                                            )),
                                        );
                                        meta_map.insert(
                                            "sdt_curve_compressed_block_length".into(),
                                            MetadataValue::Int(block_len as i64),
                                        );
                                    } else {
                                        let reason = format!(
                                            "non-image Becker & Hickl SDT/SPC {variant} curve measurement mode {} has unsupported ZIP-deflated curve payload length {} after decompression",
                                            meas.meas_mode, byte_count
                                        );
                                        meta_map.insert(
                                            "sdt_unsupported_layout_reason".into(),
                                            MetadataValue::String(reason.clone()),
                                        );
                                        unsupported_layout_reason = Some(reason);
                                        layout = "non_image_curve";
                                    }
                                }
                                Err(err) => {
                                    let reason = format!(
                                        "non-image Becker & Hickl SDT/SPC {variant} curve measurement mode {} has unsupported ZIP-deflated curve payload: {err}",
                                        meas.meas_mode
                                    );
                                    meta_map.insert(
                                        "sdt_unsupported_layout_reason".into(),
                                        MetadataValue::String(reason.clone()),
                                    );
                                    unsupported_layout_reason = Some(reason);
                                    layout = "non_image_curve";
                                }
                            }
                        } else if block_len == 0 {
                            series_size_x = 0;
                            series_size_y = 1;
                            series_size_c = 1;
                            series_t = 0;
                            image_count = 0;
                            raw_curve_payload_len = Some(0);
                            layout = "empty_curve";
                            meta_map.insert(
                                "sdt_curve_payload_encoding".into(),
                                MetadataValue::String("raw_u16_le".into()),
                            );
                            meta_map.insert("sdt_curve_sample_count".into(), MetadataValue::Int(0));
                        } else if block_len % 2 == 0 && block_len / 2 <= u32::MAX as u64 {
                            let sample_count = (block_len / 2) as u32;
                            series_size_x = sample_count;
                            series_size_y = 1;
                            series_size_c = 1;
                            series_t = 1;
                            image_count = 1;
                            raw_curve_payload_len = Some(block_len as usize);
                            layout = "raw_u16_curve";
                            meta_map.insert(
                                "sdt_curve_payload_encoding".into(),
                                MetadataValue::String("raw_u16_le".into()),
                            );
                            meta_map.insert(
                                "sdt_curve_sample_count".into(),
                                MetadataValue::Int(sample_count as i64),
                            );
                        } else if block_len <= u32::MAX as u64 {
                            series_size_x = block_len as u32;
                            series_size_y = 1;
                            series_size_c = 1;
                            series_t = 1;
                            image_count = 1;
                            series_pixel_type = PixelType::Uint8;
                            series_bits_per_pixel = 8;
                            raw_curve_payload_len = Some(block_len as usize);
                            layout = "raw_odd_byte_curve";
                            meta_map.insert(
                                "sdt_curve_payload_encoding".into(),
                                MetadataValue::String("raw_bytes_odd_length".into()),
                            );
                            meta_map.insert(
                                "sdt_curve_byte_count".into(),
                                MetadataValue::Int(block_len as i64),
                            );
                            meta_map.insert(
                                "sdt_curve_payload_diagnostic".into(),
                                MetadataValue::String(format!(
                                    "non-image Becker & Hickl SDT/SPC {variant} curve measurement mode {} has odd byte count {}; preserving raw bytes as uint8 curve payload",
                                    meas.meas_mode, block_len
                                )),
                            );
                        } else {
                            let reason = format!(
                                "non-image Becker & Hickl SDT/SPC {variant} curve measurement mode {} has no supported raw u16 curve payload",
                                meas.meas_mode
                            );
                            meta_map.insert(
                                "sdt_unsupported_layout_reason".into(),
                                MetadataValue::String(reason.clone()),
                            );
                            unsupported_layout_reason = Some(reason);
                            layout = "non_image_curve";
                        }
                    }
                }
            }
            if block_len > 0 && block_len != expected && layout == "image" && !is_zip_block {
                let reason = format!(
                    "raw SDT data block length {block_len} does not match image layout {expected} bytes or supported MCS-TA layout"
                );
                meta_map.insert(
                    "sdt_unsupported_layout_reason".into(),
                    MetadataValue::String(reason.clone()),
                );
                unsupported_layout_reason = Some(reason);
                layout = "unsupported";
            }
            meta_map.insert(
                "sdt_data_block_layout".into(),
                MetadataValue::String(layout.into()),
            );
            if block_len > 0 {
                meta_map.insert(
                    "sdt_data_block_length".into(),
                    MetadataValue::Int(block_len as i64),
                );
            }

            let meta = ImageMetadata {
                size_x: series_size_x,
                size_y: series_size_y,
                size_z: 1,
                size_c: series_size_c,
                size_t: series_t,
                pixel_type: series_pixel_type,
                bits_per_pixel: series_bits_per_pixel,
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

            series.push(SdtSeries {
                block: probe_block,
                n_time: if info.mcsta_points > 0 && series_t == info.mcsta_points {
                    series_t
                } else {
                    info.time_bins
                },
                meta,
                unsupported_layout_reason,
                raw_curve_payload_len,
                compressed_curve_payload_len,
                block_length: block_len,
                incr: info.incr,
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
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
        // Becker & Hickl block length and count-increment for this series
        // (SDTReader.java:174,278). Used by the padding heuristic and incr
        // division below.
        let series_block_length = self
            .series
            .get(self.current_series)
            .map(|s| s.block_length)
            .unwrap_or(0);
        let series_incr = self
            .series
            .get(self.current_series)
            .map(|s| s.incr)
            .unwrap_or(1);
        let series_size_c = meta.size_c as usize;
        if let Some(curve_len) = self
            .series
            .get(self.current_series)
            .and_then(|s| s.raw_curve_payload_len)
        {
            let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            let mut f = File::open(path).map_err(BioFormatsError::Io)?;
            f.seek(SeekFrom::Start(block.data_offset))
                .map_err(BioFormatsError::Io)?;
            let mut out = vec![0u8; curve_len];
            f.read_exact(&mut out).map_err(BioFormatsError::Io)?;
            return Ok(out);
        }
        if let Some(curve_len) = self
            .series
            .get(self.current_series)
            .and_then(|s| s.compressed_curve_payload_len)
        {
            let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            let mut f = File::open(path).map_err(BioFormatsError::Io)?;
            let out =
                read_sdt_zip_payload_bounded(&mut f, &block, curve_len as u64, "curve payload")?;
            if out.len() != curve_len {
                return Err(BioFormatsError::Codec(format!(
                    "SDT ZIP curve payload decoded to {} bytes, expected {curve_len}",
                    out.len()
                )));
            }
            return Ok(out);
        }
        if let Some(reason) = self
            .series
            .get(self.current_series)
            .and_then(|s| s.unsupported_layout_reason.as_ref())
        {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "unsupported Becker & Hickl SDT/SPC data block layout: {reason}"
            )));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;

        f.seek(SeekFrom::Start(block.data_offset))
            .map_err(BioFormatsError::Io)?;
        let mut signature = [0u8; 2];
        f.read_exact(&mut signature).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(block.data_offset))
            .map_err(BioFormatsError::Io)?;

        // planeSize for one channel = paddedWidth * sizeY * times * bpp
        // (SDTReader.java:181). Rows on disk are padded to a multiple of 4
        // pixels in width, but the padding may be dropped when the block length
        // shows the data is unpadded (SDTReader.java:185-190).
        let padded_width =
            effective_padded_width(size_x, size_y, times, series_size_c, series_block_length);
        let channel_plane_size = (padded_width * size_y * times * 2) as u64;

        let mut plane = if &signature == b"PK" {
            // For ZIP blocks we cannot random-seek; decode and skip preceding
            // channels by reading from the start of the decompressed stream.
            read_sdt_zip_plane(
                &mut f,
                &block,
                size_x,
                size_y,
                times,
                time_bin,
                channel,
                padded_width,
            )?
        } else {
            // Skip to the requested channel within the block.
            f.seek(SeekFrom::Current(
                channel as i64 * channel_plane_size as i64,
            ))
            .map_err(BioFormatsError::Io)?;
            read_sdt_raw_plane(
                &mut f,
                size_x,
                size_y,
                times,
                time_bin,
                plane_bytes,
                padded_width,
            )?
        };
        // Divide out the count increment if > 1 (SDTReader.java:278-295).
        apply_sdt_incr(&mut plane, series_incr);
        Ok(plane)
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
        let bps = ((meta.bits_per_pixel as usize).max(1) + 7) / 8;
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{OmeChannel, OmeImage, OmeMetadata};
        if self.series.is_empty() {
            return None;
        }
        // Java names each SDT series "<filename> #<n>" (1-based) and exposes one
        // OME channel per spectral/routing channel (SDTReader populates pixels
        // with sizeC channels per image).
        let file_name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
            .map(str::to_string);
        let mut ome = OmeMetadata::default();
        for (i, series) in self.series.iter().enumerate() {
            let channels = (0..series.meta.size_c.max(1))
                .map(|_| OmeChannel {
                    samples_per_pixel: 1,
                    ..Default::default()
                })
                .collect();
            let name = file_name.as_ref().map(|f| format!("{f} #{}", i + 1));
            ome.images.push(OmeImage {
                name,
                channels,
                modulo_t: series.meta.modulo_t.clone(),
                ..Default::default()
            });
        }
        Some(ome)
    }
}

#[cfg(test)]
mod sdt_tests {
    use super::*;
    use std::fs;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_sdt_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_sdt_{nanos}_{name}"))
    }

    fn put_u16(buf: &mut [u8], off: usize, value: u16) {
        buf[off..off + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_i16(buf: &mut [u8], off: usize, value: i16) {
        buf[off..off + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(buf: &mut [u8], off: usize, value: u32) {
        buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_i32(buf: &mut [u8], off: usize, value: i32) {
        buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn synthetic_setup_with_mcsta_points(points: u16) -> Vec<u8> {
        let mut setup = b"#SP [SP_SCAN_X,I,4]\n#SP [SP_SCAN_Y,I,1]\n#SP [SP_ADC_RE,I,4]\n#SP [SP_SCAN_RX,I,1]\n".to_vec();
        setup.extend_from_slice(b"BIN_PARA_BEGIN:\0");
        setup.extend_from_slice(&[0; 4]);
        let base = setup.len();
        setup.resize(base + 160, 0);
        put_u32(&mut setup, base + 84, 96);
        put_u32(&mut setup, base + 96, 128);
        put_u16(&mut setup, base + 128 + 8, points);
        setup
    }

    fn synthetic_setup(width: u16, height: u16, times: u16) -> Vec<u8> {
        format!(
            "#SP [SP_SCAN_X,I,{width}]\n#SP [SP_SCAN_Y,I,{height}]\n#SP [SP_ADC_RE,I,{times}]\n#SP [SP_SCAN_RX,I,1]\n"
        )
        .into_bytes()
    }

    fn decay_payload(width: u16, height: u16, times: u16, base_value: u16) -> Vec<u8> {
        let mut payload = Vec::new();
        for _y in 0..height {
            for x in 0..width {
                for t in 0..times {
                    payload.extend_from_slice(&(base_value + x * 10 + t).to_le_bytes());
                }
            }
        }
        payload
    }

    fn padded_decay_payload(width: u16, height: u16, times: u16, base_value: u16) -> Vec<u8> {
        let padded = padded_width(width as usize) as u16;
        let mut payload = Vec::new();
        for y in 0..height {
            for x in 0..padded {
                for t in 0..times {
                    let value = if x < width {
                        base_value + y * 100 + x * 10 + t
                    } else {
                        9000 + y * 100 + x * 10 + t
                    };
                    payload.extend_from_slice(&value.to_le_bytes());
                }
            }
        }
        payload
    }

    fn zip_deflate_local_file(payload: &[u8]) -> Vec<u8> {
        let mut encoder =
            flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(payload).unwrap();
        let compressed = encoder.finish().unwrap();

        let name = b"curve.bin";
        let mut zip = Vec::new();
        zip.extend_from_slice(b"PK\x03\x04");
        zip.extend_from_slice(&20u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&8u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u32.to_le_bytes());
        zip.extend_from_slice(&(compressed.len() as u32).to_le_bytes());
        zip.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(name);
        zip.extend_from_slice(&compressed);
        zip
    }

    fn zip_local_file_with_method(method: u16, name: &[u8], payload: &[u8]) -> Vec<u8> {
        let mut zip = Vec::new();
        zip.extend_from_slice(b"PK\x03\x04");
        zip.extend_from_slice(&20u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&method.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(&0u32.to_le_bytes());
        zip.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        zip.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        zip.extend_from_slice(&(name.len() as u16).to_le_bytes());
        zip.extend_from_slice(&0u16.to_le_bytes());
        zip.extend_from_slice(name);
        zip.extend_from_slice(payload);
        zip
    }

    fn write_single_block_sdt(path: &Path, setup: &[u8], payload: &[u8]) {
        let setup_offs = 42u32;
        let data_block_offs = setup_offs as usize + setup.len();

        let mut file = vec![0; 42];
        put_i16(&mut file, 0, 1);
        put_i32(&mut file, 8, setup_offs as i32);
        put_u16(&mut file, 12, setup.len() as u16);
        put_i32(&mut file, 14, data_block_offs as i32);
        put_i16(&mut file, 18, 1);
        put_i32(&mut file, 20, 0);
        put_i32(&mut file, 24, 0);
        put_i16(&mut file, 28, 0);
        put_i16(&mut file, 30, 0);
        put_u16(&mut file, 32, 0x5555);
        file.extend_from_slice(setup);

        file.resize(data_block_offs + 22, 0);
        put_i32(
            &mut file,
            data_block_offs + 2,
            (data_block_offs + 22) as i32,
        );
        put_i32(&mut file, data_block_offs + 6, 0);
        put_u32(&mut file, data_block_offs + 18, payload.len() as u32);
        file.extend_from_slice(payload);
        fs::write(path, file).unwrap();
    }

    fn synthetic_measure_info(
        meas_mode: i16,
        width: i32,
        height: i32,
        times: i16,
        channels: i32,
    ) -> Vec<u8> {
        let mut mb = vec![0u8; 211];
        put_i16(&mut mb, 36, meas_mode);
        put_i16(&mut mb, 82, times);
        put_i16(&mut mb, 100, 0);
        put_u16(&mut mb, 113, 1);
        put_i32(&mut mb, 173, width);
        put_i32(&mut mb, 177, height);
        put_i32(&mut mb, 181, channels);
        mb
    }

    fn write_single_block_sdt_with_measure_info(
        path: &Path,
        setup: &[u8],
        measure_info: &[u8],
        payload: &[u8],
    ) {
        let setup_offs = 42u32;
        let meas_desc_offs = setup_offs as usize + setup.len();
        let data_block_offs = meas_desc_offs + measure_info.len();

        let mut file = vec![0; 42];
        put_i16(&mut file, 0, 1);
        put_i32(&mut file, 8, setup_offs as i32);
        put_u16(&mut file, 12, setup.len() as u16);
        put_i32(&mut file, 14, data_block_offs as i32);
        put_i16(&mut file, 18, 1);
        put_i32(&mut file, 20, 0);
        put_i32(&mut file, 24, meas_desc_offs as i32);
        put_i16(&mut file, 28, 1);
        put_i16(&mut file, 30, measure_info.len() as i16);
        put_u16(&mut file, 32, 0x5555);
        file.extend_from_slice(setup);
        file.extend_from_slice(measure_info);

        file.resize(data_block_offs + 22, 0);
        put_i32(
            &mut file,
            data_block_offs + 2,
            (data_block_offs + 22) as i32,
        );
        put_i32(&mut file, data_block_offs + 6, 0);
        put_i16(&mut file, data_block_offs + 12, 0);
        put_u32(&mut file, data_block_offs + 18, payload.len() as u32);
        file.extend_from_slice(payload);
        fs::write(path, file).unwrap();
    }

    #[test]
    fn sdt_raw_odd_width_uses_padded_rows_but_returns_cropped_plane() {
        let path = temp_sdt_path("padded_rows.sdt");
        let setup = synthetic_setup(3, 2, 2);
        let payload = padded_decay_payload(3, 2, 2, 10);
        write_single_block_sdt(&path, &setup, &payload);

        let mut reader = SdtReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.metadata().size_x, 3);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().size_t, 2);
        match reader
            .metadata()
            .series_metadata
            .get("sdt_data_block_layout")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "image"),
            other => panic!("unexpected SDT layout metadata: {other:?}"),
        }

        let plane = reader.open_bytes(1).unwrap();
        let values: Vec<u16> = plane
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(values, vec![11, 21, 31, 111, 121, 131]);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn sdt_raw_unmatched_block_length_reports_unsupported_layout() {
        let path = temp_sdt_path("unsupported_curve.sdt");
        let setup = synthetic_setup(4, 1, 4);
        let payload = vec![1, 2, 3, 4, 5, 6];
        write_single_block_sdt(&path, &setup, &payload);

        let mut reader = SdtReader::new();
        reader.set_id(&path).unwrap();
        match reader
            .metadata()
            .series_metadata
            .get("sdt_data_block_layout")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "unsupported"),
            other => panic!("unexpected SDT layout metadata: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_unsupported_layout_reason")
        {
            Some(MetadataValue::String(v)) => {
                assert!(v.contains("raw SDT data block length 6"));
                assert!(v.contains("image layout 32 bytes"));
            }
            other => panic!("unexpected SDT unsupported metadata: {other:?}"),
        }

        let err = reader.open_bytes(0).unwrap_err();
        match err {
            BioFormatsError::UnsupportedFormat(msg) => {
                assert!(msg.contains("unsupported Becker & Hickl SDT/SPC data block layout"));
                assert!(msg.contains("raw SDT data block length 6"));
            }
            other => panic!("unexpected SDT error: {other:?}"),
        }

        let _ = fs::remove_file(path);
    }

    #[test]
    fn sdt_fcs_measurement_mode_decodes_raw_u16_curve_payload() {
        let path = temp_sdt_path("fcs_curve.sdt");
        let setup = synthetic_setup(4, 1, 4);
        let measure_info = synthetic_measure_info(3, 4, 1, 4, 1);
        let mut payload = Vec::new();
        for value in [7u16, 11, 13] {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        write_single_block_sdt_with_measure_info(&path, &setup, &measure_info, &payload);

        let mut reader = SdtReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.metadata().size_x, 3);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_c, 1);
        assert_eq!(reader.metadata().size_t, 1);
        assert_eq!(reader.metadata().image_count, 1);
        match reader
            .metadata()
            .series_metadata
            .get("sdt_data_block_layout")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "raw_u16_curve"),
            other => panic!("unexpected SDT layout metadata: {other:?}"),
        }
        match reader.metadata().series_metadata.get("sdt_curve_variant") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "FCS"),
            other => panic!("unexpected SDT curve metadata: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_payload_encoding")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "raw_u16_le"),
            other => panic!("unexpected SDT curve payload encoding: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_sample_count")
        {
            Some(MetadataValue::Int(v)) => assert_eq!(*v, 3),
            other => panic!("unexpected SDT curve sample count: {other:?}"),
        }

        let plane = reader.open_bytes(0).unwrap();
        let values: Vec<u16> = plane
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(values, vec![7, 11, 13]);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn sdt_fcs_measurement_mode_decodes_zip_deflated_u16_curve_payload() {
        let path = temp_sdt_path("fcs_zip_curve.sdt");
        let setup = synthetic_setup(4, 1, 4);
        let measure_info = synthetic_measure_info(3, 4, 1, 4, 1);
        let mut raw_curve = Vec::new();
        for value in [17u16, 19, 23, 29] {
            raw_curve.extend_from_slice(&value.to_le_bytes());
        }
        let payload = zip_deflate_local_file(&raw_curve);
        write_single_block_sdt_with_measure_info(&path, &setup, &measure_info, &payload);

        let mut reader = SdtReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.metadata().size_x, 4);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_c, 1);
        assert_eq!(reader.metadata().size_t, 1);
        assert_eq!(reader.metadata().image_count, 1);
        match reader
            .metadata()
            .series_metadata
            .get("sdt_data_block_layout")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "zip_u16_curve"),
            other => panic!("unexpected SDT layout metadata: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_payload_encoding")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "zip_deflate_raw_u16_le"),
            other => panic!("unexpected SDT curve payload encoding: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_sample_count")
        {
            Some(MetadataValue::Int(v)) => assert_eq!(*v, 4),
            other => panic!("unexpected SDT curve sample count: {other:?}"),
        }

        let plane = reader.open_bytes(0).unwrap();
        let values: Vec<u16> = plane
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(values, vec![17, 19, 23, 29]);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn sdt_fcs_measurement_mode_exposes_empty_zip_deflated_curve_as_metadata_only() {
        let path = temp_sdt_path("fcs_empty_zip_curve.sdt");
        let setup = synthetic_setup(4, 1, 4);
        let measure_info = synthetic_measure_info(3, 4, 1, 4, 1);
        let payload = zip_deflate_local_file(&[]);
        write_single_block_sdt_with_measure_info(&path, &setup, &measure_info, &payload);

        let mut reader = SdtReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.metadata().size_x, 0);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_c, 1);
        assert_eq!(reader.metadata().size_t, 0);
        assert_eq!(reader.metadata().image_count, 0);
        match reader
            .metadata()
            .series_metadata
            .get("sdt_data_block_layout")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "empty_curve"),
            other => panic!("unexpected SDT layout metadata: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_payload_encoding")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "zip_deflate_raw_u16_le"),
            other => panic!("unexpected SDT curve payload encoding: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_sample_count")
        {
            Some(MetadataValue::Int(v)) => assert_eq!(*v, 0),
            other => panic!("unexpected SDT curve sample count: {other:?}"),
        }
        assert!(matches!(
            reader.open_bytes(0),
            Err(BioFormatsError::PlaneOutOfRange(0))
        ));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn sdt_fcs_measurement_mode_exposes_empty_raw_curve_as_metadata_only() {
        let path = temp_sdt_path("fcs_empty_raw_curve.sdt");
        let setup = synthetic_setup(4, 1, 4);
        let measure_info = synthetic_measure_info(3, 4, 1, 4, 1);
        write_single_block_sdt_with_measure_info(&path, &setup, &measure_info, &[]);

        let mut reader = SdtReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.metadata().size_x, 0);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_c, 1);
        assert_eq!(reader.metadata().size_t, 0);
        assert_eq!(reader.metadata().image_count, 0);
        match reader
            .metadata()
            .series_metadata
            .get("sdt_data_block_layout")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "empty_curve"),
            other => panic!("unexpected SDT layout metadata: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_payload_encoding")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "raw_u16_le"),
            other => panic!("unexpected SDT curve payload encoding: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_sample_count")
        {
            Some(MetadataValue::Int(v)) => assert_eq!(*v, 0),
            other => panic!("unexpected SDT curve sample count: {other:?}"),
        }
        assert!(matches!(
            reader.open_bytes(0),
            Err(BioFormatsError::PlaneOutOfRange(0))
        ));

        let _ = fs::remove_file(path);
    }

    #[test]
    fn sdt_fcs_measurement_mode_preserves_odd_sized_raw_curve_payload_as_bytes() {
        let path = temp_sdt_path("fcs_odd_curve.sdt");
        let setup = synthetic_setup(4, 1, 4);
        let measure_info = synthetic_measure_info(3, 4, 1, 4, 1);
        let payload = vec![1, 2, 3, 4, 5];
        write_single_block_sdt_with_measure_info(&path, &setup, &measure_info, &payload);

        let mut reader = SdtReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.metadata().size_x, 5);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_c, 1);
        assert_eq!(reader.metadata().size_t, 1);
        assert_eq!(reader.metadata().image_count, 1);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
        assert_eq!(reader.metadata().bits_per_pixel, 8);
        match reader
            .metadata()
            .series_metadata
            .get("sdt_data_block_layout")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "raw_odd_byte_curve"),
            other => panic!("unexpected SDT layout metadata: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_payload_encoding")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "raw_bytes_odd_length"),
            other => panic!("unexpected SDT curve payload encoding: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_byte_count")
        {
            Some(MetadataValue::Int(v)) => assert_eq!(*v, 5),
            other => panic!("unexpected SDT curve byte count: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_payload_diagnostic")
        {
            Some(MetadataValue::String(v)) => {
                assert!(v.contains("odd byte count 5"));
                assert!(v.contains("preserving raw bytes"));
            }
            other => panic!("unexpected SDT curve diagnostic: {other:?}"),
        }
        assert_eq!(reader.open_bytes(0).unwrap(), payload);
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 3, 1).unwrap(),
            vec![2, 3, 4]
        );

        let _ = fs::remove_file(path);
    }

    #[test]
    fn sdt_fcs_measurement_mode_preserves_odd_sized_zip_curve_payload_as_bytes() {
        let path = temp_sdt_path("fcs_odd_zip_curve.sdt");
        let setup = synthetic_setup(4, 1, 4);
        let measure_info = synthetic_measure_info(3, 4, 1, 4, 1);
        let raw_curve = vec![9, 8, 7];
        let payload = zip_deflate_local_file(&raw_curve);
        write_single_block_sdt_with_measure_info(&path, &setup, &measure_info, &payload);

        let mut reader = SdtReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.metadata().size_x, 3);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.metadata().size_c, 1);
        assert_eq!(reader.metadata().size_t, 1);
        assert_eq!(reader.metadata().image_count, 1);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
        assert_eq!(reader.metadata().bits_per_pixel, 8);
        match reader
            .metadata()
            .series_metadata
            .get("sdt_data_block_layout")
        {
            Some(MetadataValue::String(v)) => assert_eq!(v, "zip_odd_byte_curve"),
            other => panic!("unexpected SDT layout metadata: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_payload_encoding")
        {
            Some(MetadataValue::String(v)) => {
                assert_eq!(v, "zip_deflate_raw_bytes_odd_length")
            }
            other => panic!("unexpected SDT curve payload encoding: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_byte_count")
        {
            Some(MetadataValue::Int(v)) => assert_eq!(*v, 3),
            other => panic!("unexpected SDT curve byte count: {other:?}"),
        }
        match reader
            .metadata()
            .series_metadata
            .get("sdt_curve_payload_diagnostic")
        {
            Some(MetadataValue::String(v)) => {
                assert!(v.contains("decoded to odd byte count 3"));
                assert!(v.contains("preserving raw bytes"));
            }
            other => panic!("unexpected SDT curve diagnostic: {other:?}"),
        }
        assert_eq!(reader.open_bytes(0).unwrap(), raw_curve);

        let _ = fs::remove_file(path);
    }

    #[test]
    fn sdt_fcs_zip_curve_with_unsupported_method_preserves_header_diagnostics() {
        let path = temp_sdt_path("fcs_zip_method_diagnostic.sdt");
        let setup = synthetic_setup(4, 1, 4);
        let measure_info = synthetic_measure_info(3, 4, 1, 4, 1);
        let payload = zip_local_file_with_method(0, b"curve-raw.bin", &[1, 2, 3, 4]);
        write_single_block_sdt_with_measure_info(&path, &setup, &measure_info, &payload);

        let mut reader = SdtReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;
        match metadata.get("sdt_data_block_layout") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "non_image_curve"),
            other => panic!("unexpected SDT layout metadata: {other:?}"),
        }
        match metadata.get("sdt_unsupported_layout_reason") {
            Some(MetadataValue::String(v)) => {
                assert!(v.contains("unsupported ZIP-deflated curve payload"));
                assert!(v.contains("unsupported SDT ZIP compression method 0"));
            }
            other => panic!("unexpected SDT unsupported metadata: {other:?}"),
        }
        match metadata.get("sdt_curve_zip_method") {
            Some(MetadataValue::Int(v)) => assert_eq!(*v, 0),
            other => panic!("unexpected SDT ZIP method metadata: {other:?}"),
        }
        match metadata.get("sdt_curve_zip_declared_uncompressed_size") {
            Some(MetadataValue::Int(v)) => assert_eq!(*v, 4),
            other => panic!("unexpected SDT ZIP size metadata: {other:?}"),
        }
        match metadata.get("sdt_curve_zip_file_name_preview") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "curve-raw.bin"),
            other => panic!("unexpected SDT ZIP name metadata: {other:?}"),
        }
        match metadata.get("sdt_curve_zip_leading_bytes") {
            Some(MetadataValue::Bytes(v)) => assert_eq!(&v[..4], b"PK\x03\x04"),
            other => panic!("unexpected SDT ZIP leading-byte metadata: {other:?}"),
        }

        let err = reader.open_bytes(0).unwrap_err();
        match err {
            BioFormatsError::UnsupportedFormat(msg) => {
                assert!(msg.contains("unsupported Becker & Hickl SDT/SPC data block layout"));
                assert!(msg.contains("unsupported SDT ZIP compression method 0"));
            }
            other => panic!("unexpected SDT error: {other:?}"),
        }

        let _ = fs::remove_file(path);
    }

    #[test]
    fn sdt_preserves_measurement_descriptor_scalars_as_metadata() {
        let path = temp_sdt_path("descriptor_metadata.sdt");
        let setup = synthetic_setup(3, 2, 5);
        let measure_info = synthetic_measure_info(13, 3, 2, 5, 2);
        let payload = padded_decay_payload(3, 2, 5, 20);
        write_single_block_sdt_with_measure_info(&path, &setup, &measure_info, &payload);

        let mut reader = SdtReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;
        for (key, expected) in [
            ("sdt_measurement_mode", 13),
            ("sdt_measurement_adc_re", 5),
            ("sdt_measurement_stopt", 0),
            ("sdt_measurement_incr", 1),
            ("sdt_measurement_scan_x", 3),
            ("sdt_measurement_scan_y", 2),
            ("sdt_measurement_scan_rx", 2),
        ] {
            match metadata.get(key) {
                Some(MetadataValue::Int(v)) => assert_eq!(*v, expected, "{key}"),
                other => panic!("unexpected {key} metadata: {other:?}"),
            }
        }

        let _ = fs::remove_file(path);
    }

    fn put_f32(buf: &mut [u8], off: usize, value: f32) {
        buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    #[test]
    fn sdt_parses_fcs_and_hist_descriptor_subblocks_as_metadata() {
        // Build a full-length measurement descriptor with all five sub-blocks
        // present (MeasureInfo 211, MeasStopInfo 60, MeasFCSInfo 38, extended
        // MeasureInfo 26, MeasHISTInfo 24 = 359 bytes) so the FCS/FIDA/FILDA/MCS
        // curve descriptors are decoded.
        let path = temp_sdt_path("fcs_hist_descriptor.sdt");
        let setup = synthetic_setup(4, 1, 4);
        // FCS measurement mode (3), single-line, 4 time bins, 1 channel.
        let mut measure_info = synthetic_measure_info(3, 4, 1, 4, 1);
        measure_info.resize(211 + 60 + 38 + 26 + 24, 0);

        // MeasStopInfo at offset 211: status (u16) and stopTime (f32).
        put_u16(&mut measure_info, 211, 7);
        put_f32(&mut measure_info, 211 + 4, 1.5);

        // MeasFCSInfo at offset 271: fcsDecayCalc bitmask (decay|fcs|FIDA|MCS)
        // at +2, fcsPoints (i32) at +16 (chan, fcsDecayCalc, mtResol, cortime,
        // calcPhotons all precede it).
        put_u16(&mut measure_info, 271 + 2, 0b10111);
        put_i32(&mut measure_info, 271 + 16, 1024);

        // Extended MeasureInfo at offset 309: imageX (i32) and detType (i16).
        put_i32(&mut measure_info, 309, 256);
        put_i16(&mut measure_info, 309 + 22, 9);

        // MeasHISTInfo at offset 335: fidaPoints (+8), fildaPoints (+12),
        // mcsPoints (+20).
        put_i32(&mut measure_info, 335 + 8, 64);
        put_i32(&mut measure_info, 335 + 12, 32);
        put_i32(&mut measure_info, 335 + 20, 256);

        // Provide a raw curve payload (mode 3 is a non-image FCS curve).
        let payload = vec![0u8; 8];
        write_single_block_sdt_with_measure_info(&path, &setup, &measure_info, &payload);

        let mut reader = SdtReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        for (key, expected) in [
            ("MeasStopInfo.status", 7),
            ("MeasFCSInfo.fcsDecayCalc", 0b10111),
            ("MeasFCSInfo.fcsPoints", 1024),
            ("MeasureInfo.imageX", 256),
            ("MeasureInfo.detType", 9),
            ("MeasHISTInfo.fidaPoints", 64),
            ("MeasHISTInfo.fildaPoints", 32),
            ("MeasHISTInfo.mcsPoints", 256),
        ] {
            match metadata.get(key) {
                Some(MetadataValue::Int(v)) => assert_eq!(*v, expected, "{key}"),
                other => panic!("unexpected {key} metadata: {other:?}"),
            }
        }
        match metadata.get("MeasStopInfo.stopTime") {
            Some(MetadataValue::Float(v)) => assert!((*v - 1.5).abs() < 1e-6),
            other => panic!("unexpected MeasStopInfo.stopTime metadata: {other:?}"),
        }

        let _ = fs::remove_file(path);
    }

    #[test]
    fn sdt_binary_setup_mcsta_points_sizes_mcs_ta_series() {
        let path = temp_sdt_path("mcsta.sdt");
        let setup = synthetic_setup_with_mcsta_points(3);
        let setup_offs = 42u32;
        let data_block_offs = setup_offs as usize + setup.len();

        let block0 = decay_payload(4, 1, 4, 10);
        let block1 = decay_payload(4, 1, 3, 100);
        let block0_header = data_block_offs;
        let block1_header = block0_header + 22 + block0.len();

        let mut file = vec![0; 42];
        put_i16(&mut file, 0, 1);
        put_i32(&mut file, 8, setup_offs as i32);
        put_u16(&mut file, 12, setup.len() as u16);
        put_i32(&mut file, 14, data_block_offs as i32);
        put_i16(&mut file, 18, 2);
        put_i32(&mut file, 20, 0);
        put_i32(&mut file, 24, 0);
        put_i16(&mut file, 28, 0);
        put_i16(&mut file, 30, 0);
        put_u16(&mut file, 32, 0x5555);
        file.extend_from_slice(&setup);

        file.resize(block0_header + 22, 0);
        put_i16(&mut file, block0_header, 0);
        put_i32(&mut file, block0_header + 2, (block0_header + 22) as i32);
        put_i32(&mut file, block0_header + 6, block1_header as i32);
        put_u32(&mut file, block0_header + 18, block0.len() as u32);
        file.extend_from_slice(&block0);

        file.resize(block1_header + 22, 0);
        put_i16(&mut file, block1_header, 1);
        put_i32(&mut file, block1_header + 2, (block1_header + 22) as i32);
        put_i32(&mut file, block1_header + 6, 0);
        put_u32(&mut file, block1_header + 18, block1.len() as u32);
        file.extend_from_slice(&block1);

        fs::write(&path, file).unwrap();

        let mut reader = SdtReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.metadata().size_t, 4);
        assert_eq!(reader.metadata().image_count, 4);

        reader.set_series(1).unwrap();
        assert_eq!(reader.metadata().size_t, 3);
        assert_eq!(reader.metadata().image_count, 3);
        match reader.metadata().series_metadata.get("time_channels") {
            Some(MetadataValue::Int(v)) => assert_eq!(*v, 4),
            other => panic!("unexpected time_channels metadata: {other:?}"),
        }

        let plane = reader.open_bytes(2).unwrap();
        let values: Vec<u16> = plane
            .chunks_exact(2)
            .map(|b| u16::from_le_bytes([b[0], b[1]]))
            .collect();
        assert_eq!(values, vec![102, 112, 122, 132]);

        let _ = fs::remove_file(path);
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
        // Mirror Java initOriginalMetadata(): surface every INI key/value pair as
        // global metadata under the "<table header> - <key>" key. This is how
        // Java exposes the BACKGROUND_TABLE (background.*), datatype, channel
        // count and hasDarkImage keys as original metadata.
        liflim_add_global_meta(&ini, &mut series_metadata);

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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
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

/// Mirrors the generic INI-walk in Java `LiFlimReader.initOriginalMetadata()`
/// (the `for (IniTable table : ini)` loop), which calls
/// `addGlobalMeta(name + " - " + key, value)` for every key in every table.
///
/// `name` is the table header (`IniTable.HEADER_KEY`); for the version 2.0
/// default table our parser stores the section as the empty string, which Java
/// represents with `IniTable.DEFAULT_HEADER` ("MainTable").
fn liflim_add_global_meta(
    ini: &HashMap<String, HashMap<String, String>>,
    series_metadata: &mut HashMap<String, MetadataValue>,
) {
    const DEFAULT_HEADER: &str = "MainTable";
    for (section, table) in ini {
        let name = if section.is_empty() {
            DEFAULT_HEADER
        } else {
            section.as_str()
        };
        for (key, value) in table {
            let meta_key = format!("{name} - {key}");
            series_metadata.insert(meta_key, MetadataValue::String(value.clone()));
        }
    }
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
    fn liflim_surfaces_ini_keys_including_background_table() {
        let path = temp_path("background.fli");
        let header = "\
[FLIMIMAGE: INFO]
version=1.0
compression=0
[FLIMIMAGE: LAYOUT]
datatype=UINT16
packing=lsb
channels=1
x=2
y=1
z=1
phases=1
frequencies=1
timestamps=1
hasDarkImage=0
[FLIMIMAGE: BACKGROUND]
datatype=UINT16
channels=1
x=2
y=1
z=1
phases=1
frequencies=1
timestamps=1
";
        let mut payload = Vec::new();
        for value in 1u16..=2 {
            payload.extend_from_slice(&value.to_le_bytes());
        }
        write_liflim(&path, header, &payload);

        let mut reader = LiFlimReader::new();
        reader.set_id(&path).unwrap();
        let md = &reader.metadata().series_metadata;

        // Background-table keys are surfaced under Java's "<header> - <key>" form.
        match md.get("FLIMIMAGE: BACKGROUND - datatype") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "UINT16"),
            other => panic!("missing background datatype metadata: {other:?}"),
        }
        match md.get("FLIMIMAGE: BACKGROUND - channels") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "1"),
            other => panic!("missing background channels metadata: {other:?}"),
        }
        // The layout/info keys are surfaced too (e.g. hasDarkImage, version).
        match md.get("FLIMIMAGE: LAYOUT - hasDarkImage") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "0"),
            other => panic!("missing hasDarkImage metadata: {other:?}"),
        }
        match md.get("FLIMIMAGE: INFO - version") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "1.0"),
            other => panic!("missing version metadata: {other:?}"),
        }
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
