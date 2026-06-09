//! Windowed JPEG decoding for single-strip whole-slide TIFF/NDPI.
//!
//! Hamamatsu NDPI (and some other whole-slide formats) store an entire pyramid
//! level as ONE baseline JPEG strip (`RowsPerStrip >= ImageLength`). Decoding the
//! whole strip to crop a small region forces `jpeg-decoder` to materialise the
//! full plane in RAM — tens of GB for a gigapixel level.
//!
//! A JPEG written with a restart interval (`DRI` marker + periodic `RSTn`
//! markers) splits its entropy-coded scan into independently decodable runs of
//! `restart_interval` MCUs. When the restart interval is a whole number of MCU
//! rows (the usual NDPI case), we can decode only the band of MCU rows covering
//! the requested `[y, y+h)` by reconstructing a small synthetic JPEG containing
//! just those intervals and decoding that with the existing `jpeg-decoder` path.
//! This is the technique OpenSlide uses; no extra crate is required.

use std::io::{Read, Seek, SeekFrom};

use super::compression::{decompress_jpeg_color, merge_jpeg_tables, JpegColor};
use crate::common::error::{BioFormatsError, Result};

/// A decoded sub-rectangle of a JPEG, chunky (interleaved) pixels.
pub(crate) struct DecodedBand {
    /// `band_width * band_height * channels` chunky bytes.
    pub pixels: Vec<u8>,
    /// First image column covered (a whole MCU / restart-interval boundary).
    /// Always `0` for the full-width `decode_rows` path.
    pub band_x0: u32,
    /// Number of image columns covered (the decoded row stride in pixels).
    pub band_width: u32,
    /// First image row covered by the band (always a whole MCU-row boundary).
    pub band_y0: u32,
    /// Number of image rows covered by the band.
    pub band_height: u32,
}

/// Restart-marker index for one baseline JPEG, enabling windowed decode.
pub(crate) struct JpegRestartIndex {
    /// Owned copy of the header bytes `jpeg[0..scan_start]`, patched per call.
    header: Vec<u8>,
    /// Offset within `header` of the 2-byte SOF height field (big-endian).
    sof_height_offset: usize,
    /// Image width in pixels (kept for completeness / debugging).
    #[allow(dead_code)]
    width: u32,
    height: u32,
    /// MCU width in pixels (`8 * max H sampling factor`). Kept for completeness.
    #[allow(dead_code)]
    mcu_w: u32,
    /// MCU height in pixels (`8 * max V sampling factor`).
    mcu_h: u32,
    mcus_per_row: u32,
    mcu_rows: u32,
    /// Restart interval in MCUs (from the DRI marker). Non-zero.
    restart_interval: u32,
    /// Byte offset (into the original JPEG) of the start of each restart
    /// interval. `restart_offsets[0]` is the scan start; `restart_offsets[k]`
    /// for `k >= 1` is the offset just after the k-th `RSTn` marker.
    restart_offsets: Vec<usize>,
    /// Byte offset of the trailing EOI (or end of data).
    scan_end: usize,
}

#[inline]
fn be16(data: &[u8], off: usize) -> u16 {
    ((data[off] as u16) << 8) | data[off + 1] as u16
}

/// Header-only JPEG parse result (no entropy-stream scan), for NDPI windowing.
struct JpegHeader {
    /// Header bytes `jpeg[0..scan_start]` (SOI … end of SOS).
    header: Vec<u8>,
    sof_height_offset: usize,
    sof_width_offset: usize,
    max_h: u8,
    max_v: u8,
    restart_interval: u32,
    /// Offset within the JPEG where the entropy-coded scan begins.
    scan_start: usize,
}

/// Parse only the JPEG marker segments up to and including SOS (no entropy
/// scan). Returns the header geometry needed to build a windowed sub-JPEG.
fn parse_header_only(jpeg: &[u8]) -> Option<JpegHeader> {
    if jpeg.len() < 2 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return None;
    }
    let mut pos = 2usize;
    let mut max_h = 0u8;
    let mut max_v = 0u8;
    let mut restart_interval = 0u32;
    let mut sof_height_offset = 0usize;
    let mut sof_width_offset = 0usize;
    let scan_start;
    loop {
        if pos >= jpeg.len() || jpeg[pos] != 0xFF {
            return None;
        }
        let mut mp = pos + 1;
        while mp < jpeg.len() && jpeg[mp] == 0xFF {
            mp += 1;
        }
        if mp >= jpeg.len() {
            return None;
        }
        let marker = jpeg[mp];
        pos = mp + 1;
        match marker {
            0xC0 | 0xC1 => {
                if pos + 2 > jpeg.len() {
                    return None;
                }
                let len = be16(jpeg, pos) as usize;
                let seg = pos + 2;
                if seg + 6 > jpeg.len() {
                    return None;
                }
                sof_height_offset = seg + 1;
                sof_width_offset = seg + 3;
                let ncomp = jpeg[seg + 5] as usize;
                let mut cp = seg + 6;
                for _ in 0..ncomp {
                    if cp + 3 > jpeg.len() {
                        return None;
                    }
                    let samp = jpeg[cp + 1];
                    max_h = max_h.max(samp >> 4);
                    max_v = max_v.max(samp & 0x0F);
                    cp += 3;
                }
                pos += len;
            }
            0xDD => {
                if pos + 4 > jpeg.len() {
                    return None;
                }
                let len = be16(jpeg, pos) as usize;
                restart_interval = be16(jpeg, pos + 2) as u32;
                pos += len;
            }
            0xDA => {
                if pos + 2 > jpeg.len() {
                    return None;
                }
                let len = be16(jpeg, pos) as usize;
                scan_start = pos + len;
                break;
            }
            0x01 | 0xD0..=0xD7 => {}
            _ => {
                if pos + 2 > jpeg.len() {
                    return None;
                }
                let len = be16(jpeg, pos) as usize;
                pos += len;
            }
        }
    }
    if restart_interval == 0 || max_h == 0 || max_v == 0 || scan_start > jpeg.len() {
        return None;
    }
    Some(JpegHeader {
        header: jpeg[0..scan_start].to_vec(),
        sof_height_offset,
        sof_width_offset,
        max_h,
        max_v,
        restart_interval,
        scan_start,
    })
}

/// Windowed decode of one giant single-strip Hamamatsu NDPI JPEG level, driven
/// by the NDPI restart-marker offset array (tag 65426/65432) instead of scanning
/// the strip. Reads only the JPEG header and the band of restart intervals that
/// cover `[y, y+h)` from `reader`, so a >4 GB level is never materialised whole.
///
/// `markers[k]` is the byte offset (relative to the strip start) just after the
/// k-th `RSTn` marker; `markers[0]` is the scan start. `width`/`height` are the
/// true level dimensions from the TIFF tags (the embedded JPEG's SOF is often
/// 0×0, so both fields are patched into the synthetic sub-JPEG).
///
/// Returns `None` when windowing is not possible (no markers, unparseable
/// header, unaligned restart intervals); the caller falls back to the generic
/// path. `Some(Err(_))` means the synthetic JPEG failed to decode.
#[allow(clippy::too_many_arguments)]
pub(crate) fn decode_rows_ndpi<R: Read + Seek>(
    reader: &mut R,
    strip_offset: u64,
    strip_byte_count: u64,
    markers: &[u64],
    jpeg_tables: Option<&[u8]>,
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    color: JpegColor,
) -> Option<Result<DecodedBand>> {
    if w == 0 || h == 0 || width == 0 || height == 0 || markers.len() < 2 {
        return None;
    }
    let y_end = y.checked_add(h)?;
    if y_end > height {
        return None;
    }

    // Read just the JPEG header (SOI..SOS) from the start of the strip. A header
    // is at most a few KB; cap the read at the marker[1] offset (the first RSTn)
    // or 1 MiB, whichever is smaller, so we never read the multi-GB scan here.
    const HEADER_CAP: u64 = 1 << 20;
    let header_len = markers.get(1).copied().unwrap_or(HEADER_CAP).min(HEADER_CAP);
    let header_len = header_len.min(strip_byte_count).max(2) as usize;
    let mut head_buf = read_at(reader, strip_offset, header_len).ok()?;
    // Abbreviated streams keep the tables in JPEGTables (tag 347); splice them in.
    if let Some(tables) = jpeg_tables {
        head_buf = merge_jpeg_tables(tables, &head_buf);
    }
    let hdr = parse_header_only(&head_buf)?;

    let mcu_w = 8 * hdr.max_h as u32;
    let mcu_h = 8 * hdr.max_v as u32;
    let mcus_per_row = width.div_ceil(mcu_w);
    let mcu_rows = height.div_ceil(mcu_h);
    let ri = hdr.restart_interval;
    if mcus_per_row == 0 || mcu_rows == 0 || ri == 0 {
        return None;
    }

    // NDPI levels store the image as one JPEG with a 0×0 SOF whose true width can
    // exceed JPEG's 16-bit SOF limit (e.g. 188160 px). The restart interval is a
    // whole number of MCUs *within* a single MCU row (`mcus_per_row % ri == 0`),
    // so each interval is an independently-decodable horizontal block of
    // `ri * mcu_w` px × `mcu_h` px. We therefore window in BOTH axes: select the
    // interval-columns overlapping [x, x+w) and the MCU-rows overlapping
    // [y, y+h), and assemble a synthetic JPEG whose width is just those columns
    // (≤ 65535). Layouts where a restart interval is not a sub-row (the rare
    // multi-row case) are left to the generic path.
    if mcus_per_row % ri != 0 {
        return None;
    }
    let intervals_per_row = mcus_per_row / ri; // restart intervals per MCU row
    let iv_w = ri * mcu_w; // pixel width of one restart interval

    let x_end = x.checked_add(w)?;
    if x_end > width {
        return None;
    }
    let col0 = x / iv_w;
    let col1 = x_end.div_ceil(iv_w).min(intervals_per_row);
    let mr0 = y / mcu_h;
    let mr1 = y_end.div_ceil(mcu_h).min(mcu_rows);
    if col1 <= col0 || mr1 <= mr0 {
        return None;
    }
    let cols = (col1 - col0) as usize;

    let band_x0 = col0 * iv_w;
    let band_y0 = mr0 * mcu_h;
    let band_height = ((mr1 - mr0) * mcu_h).min(height - band_y0);
    // Synthetic JPEG width must be a whole number of MCUs and fit in 16 bits.
    let synth_width = (col1 - col0) * iv_w;
    if synth_width == 0 || synth_width > 65535 || band_height == 0 || band_height > 65535 {
        return None;
    }

    // Gather the selected intervals (row-major), inserting a fresh RSTn between
    // consecutive intervals. Each interval (r, c) has linear index
    // `r * intervals_per_row + c`; its bytes are `[markers[idx], markers[idx+1])`
    // minus the trailing 2-byte RST marker that begins the next interval.
    let mut scan: Vec<u8> = Vec::new();
    let mut rst = 0u8;
    let total = (mr1 - mr0) as usize * cols;
    let mut emitted = 0usize;
    for r in mr0..mr1 {
        for c in col0..col1 {
            let idx = (r * intervals_per_row + c) as usize;
            if idx + 1 > markers.len() {
                return None;
            }
            let from = markers[idx];
            // End of this interval's data: just before the next RST marker, or the
            // strip end for the very last interval in the file.
            let to = if idx + 1 < markers.len() {
                markers[idx + 1].checked_sub(2)?
            } else {
                strip_byte_count
            };
            if from < hdr.scan_start as u64 || to <= from || to > strip_byte_count {
                return None;
            }
            let len = usize::try_from(to - from).ok()?;
            let mut data = read_at(reader, strip_offset + from, len).ok()?;
            if data.len() >= 2 && data[data.len() - 2] == 0xFF && data[data.len() - 1] == 0xD9 {
                data.truncate(data.len() - 2);
            }
            // Renumber any RSTn already inside the interval data (there are none in
            // a well-formed single-interval slice, but stay safe) — actually copy
            // raw, then append a separating RST unless this is the last interval.
            scan.extend_from_slice(&data);
            emitted += 1;
            if emitted < total {
                scan.push(0xFF);
                scan.push(0xD0 | (rst & 0x07));
                rst = rst.wrapping_add(1) & 0x07;
            }
        }
    }

    // Build the synthetic JPEG: patched header (width = synth_width, height =
    // band_height) + assembled scan + EOI.
    let mut synthetic = hdr.header.clone();
    let wb = (synth_width as u16).to_be_bytes();
    let hb = (band_height as u16).to_be_bytes();
    synthetic[hdr.sof_width_offset] = wb[0];
    synthetic[hdr.sof_width_offset + 1] = wb[1];
    synthetic[hdr.sof_height_offset] = hb[0];
    synthetic[hdr.sof_height_offset + 1] = hb[1];
    synthetic.reserve(scan.len() + 2);
    synthetic.extend_from_slice(&scan);
    synthetic.push(0xFF);
    synthetic.push(0xD9);

    Some(decompress_jpeg_color(&synthetic, color).map(|pixels| DecodedBand {
        pixels,
        band_x0,
        band_width: synth_width,
        band_y0,
        band_height,
    }))
}

/// Read exactly `len` bytes at absolute `offset` from `reader`.
fn read_at<R: Read + Seek>(reader: &mut R, offset: u64, len: usize) -> Result<Vec<u8>> {
    reader.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
    Ok(buf)
}

/// Parse the JPEG markers and index its restart structure. Returns `None` when
/// the JPEG cannot be windowed (no restart interval, progressive, malformed,
/// etc.) — callers then fall back to a full decode.
pub(crate) fn index(jpeg: &[u8]) -> Option<JpegRestartIndex> {
    if jpeg.len() < 2 || jpeg[0] != 0xFF || jpeg[1] != 0xD8 {
        return None;
    }

    let mut pos = 2usize;
    let mut width = 0u32;
    let mut height = 0u32;
    let mut max_h = 0u8;
    let mut max_v = 0u8;
    let mut restart_interval = 0u32;
    let mut sof_height_offset = 0usize;
    let scan_start;

    loop {
        // Locate the next marker, skipping any 0xFF fill bytes.
        if pos >= jpeg.len() || jpeg[pos] != 0xFF {
            return None;
        }
        let mut mp = pos + 1;
        while mp < jpeg.len() && jpeg[mp] == 0xFF {
            mp += 1;
        }
        if mp >= jpeg.len() {
            return None;
        }
        let marker = jpeg[mp];
        pos = mp + 1;

        match marker {
            // SOF0 (baseline) / SOF1 (extended sequential): geometry + sampling.
            0xC0 | 0xC1 => {
                if pos + 2 > jpeg.len() {
                    return None;
                }
                let len = be16(jpeg, pos) as usize;
                let seg = pos + 2;
                // precision(1) height(2) width(2) num_components(1) then 3 each.
                if seg + 6 > jpeg.len() {
                    return None;
                }
                height = be16(jpeg, seg + 1) as u32;
                width = be16(jpeg, seg + 3) as u32;
                sof_height_offset = seg + 1;
                let ncomp = jpeg[seg + 5] as usize;
                let mut cp = seg + 6;
                for _ in 0..ncomp {
                    if cp + 3 > jpeg.len() {
                        return None;
                    }
                    let samp = jpeg[cp + 1];
                    let h = samp >> 4;
                    let v = samp & 0x0F;
                    if h > max_h {
                        max_h = h;
                    }
                    if v > max_v {
                        max_v = v;
                    }
                    cp += 3;
                }
                pos += len;
            }
            // DRI: restart interval (in MCUs).
            0xDD => {
                if pos + 4 > jpeg.len() {
                    return None;
                }
                let len = be16(jpeg, pos) as usize;
                restart_interval = be16(jpeg, pos + 2) as u32;
                pos += len;
            }
            // SOS: scan header; entropy data begins right after it.
            0xDA => {
                if pos + 2 > jpeg.len() {
                    return None;
                }
                let len = be16(jpeg, pos) as usize;
                scan_start = pos + len;
                break;
            }
            // Markers without a length payload.
            0x01 | 0xD0..=0xD7 => {}
            // Everything else (APPn, COM, DQT, DHT, DAC, other SOFn): skip by length.
            _ => {
                if pos + 2 > jpeg.len() {
                    return None;
                }
                let len = be16(jpeg, pos) as usize;
                pos += len;
            }
        }
    }

    if restart_interval == 0
        || width == 0
        || height == 0
        || max_h == 0
        || max_v == 0
        || scan_start > jpeg.len()
    {
        return None;
    }

    let mcu_w = 8 * max_h as u32;
    let mcu_h = 8 * max_v as u32;
    let mcus_per_row = width.div_ceil(mcu_w);
    let mcu_rows = height.div_ceil(mcu_h);

    // Walk the entropy-coded scan recording the offset after each RSTn marker.
    let mut restart_offsets = vec![scan_start];
    let mut scan_end = jpeg.len();
    let mut i = scan_start;
    while i < jpeg.len() {
        if jpeg[i] == 0xFF {
            if i + 1 >= jpeg.len() {
                scan_end = jpeg.len();
                break;
            }
            let m = jpeg[i + 1];
            if m == 0x00 {
                // Stuffed 0xFF byte — part of the entropy stream.
                i += 2;
                continue;
            }
            if m == 0xFF {
                // Fill byte before a marker.
                i += 1;
                continue;
            }
            if (0xD0..=0xD7).contains(&m) {
                restart_offsets.push(i + 2);
                i += 2;
                continue;
            }
            // EOI or any other marker terminates the scan.
            scan_end = i;
            break;
        }
        i += 1;
    }

    Some(JpegRestartIndex {
        header: jpeg[0..scan_start].to_vec(),
        sof_height_offset,
        width,
        height,
        mcu_w,
        mcu_h,
        mcus_per_row,
        mcu_rows,
        restart_interval,
        restart_offsets,
        scan_end,
    })
}

impl JpegRestartIndex {
    /// Decode only the band of MCU rows covering `[y, y+h)`.
    ///
    /// Returns:
    /// * `None` — windowing is not possible (the restart intervals do not align
    ///   to MCU-row boundaries); the caller should fall back to full decode.
    /// * `Some(Ok(band))` — the decoded band (chunky, full image width).
    /// * `Some(Err(_))` — the synthetic JPEG failed to decode.
    pub(crate) fn decode_rows(
        &self,
        jpeg: &[u8],
        y: u32,
        h: u32,
        color: JpegColor,
    ) -> Option<Result<DecodedBand>> {
        if h == 0 {
            return None;
        }
        let y_end = y.checked_add(h)?;
        if y_end > self.height {
            return None;
        }

        if self.mcus_per_row == 0 || self.restart_interval == 0 {
            return None;
        }

        let mr0 = y / self.mcu_h;
        let mr1 = y_end.div_ceil(self.mcu_h).min(self.mcu_rows);
        if mr1 <= mr0 {
            return None;
        }

        // Windowing only works when MCU-row boundaries coincide with restart
        // interval boundaries. Two aligned layouts cover the cases seen in the
        // wild; anything else falls back to a full decode.
        //
        //  (a) restart_interval >= one MCU row (restart_interval % mcus_per_row
        //      == 0): each interval spans `rows_per_interval` whole MCU rows. We
        //      can only cut on interval boundaries, so the band snaps out to the
        //      nearest enclosing intervals.
        //  (b) restart_interval < one MCU row (mcus_per_row % restart_interval
        //      == 0, the usual Hamamatsu/Aperio NDPI layout): each MCU row is an
        //      exact whole number of intervals, so any MCU-row boundary is also
        //      an interval boundary and the band is exactly [mr0, mr1).
        let (band_mr0, band_mr1, i0, i1) =
            if self.restart_interval % self.mcus_per_row == 0 {
                let rows_per_interval = self.restart_interval / self.mcus_per_row;
                if rows_per_interval == 0 {
                    return None;
                }
                let i0 = mr0 / rows_per_interval;
                let i1 = mr1.div_ceil(rows_per_interval);
                let band_mr0 = i0 * rows_per_interval;
                let band_mr1 = (i1 * rows_per_interval).min(self.mcu_rows);
                (band_mr0, band_mr1, i0 as usize, i1 as usize)
            } else if self.mcus_per_row % self.restart_interval == 0 {
                let intervals_per_row = self.mcus_per_row / self.restart_interval;
                let i0 = mr0 * intervals_per_row;
                let i1 = mr1 * intervals_per_row;
                (mr0, mr1, i0 as usize, i1 as usize)
            } else {
                return None;
            };

        let i1 = i1.min(self.restart_offsets.len());
        if i0 >= self.restart_offsets.len() || i1 <= i0 {
            return None;
        }

        let band_y0 = band_mr0 * self.mcu_h;
        let band_end = (band_mr1 * self.mcu_h).min(self.height);
        if band_end <= band_y0 {
            return None;
        }
        let band_height = band_end - band_y0;

        // Build the synthetic JPEG: patched header + selected intervals + EOI.
        let scan_from = self.restart_offsets[i0];
        let scan_to = if i1 < self.restart_offsets.len() {
            // Stop just before the RSTn marker that begins interval i1.
            self.restart_offsets[i1].checked_sub(2)?
        } else {
            self.scan_end
        };
        if scan_from > scan_to || scan_to > jpeg.len() {
            return None;
        }
        let scan = &jpeg[scan_from..scan_to];

        let mut synthetic = self.header.clone();
        let hb = (band_height as u16).to_be_bytes();
        synthetic[self.sof_height_offset] = hb[0];
        synthetic[self.sof_height_offset + 1] = hb[1];

        synthetic.reserve(scan.len() + 2);
        // Renumber RSTn markers so the cycle restarts RST0,RST1,…,RST7 from the
        // synthetic start (the decoder counts restart markers from zero).
        let mut rst = 0u8;
        let mut j = 0usize;
        while j < scan.len() {
            let b = scan[j];
            if b == 0xFF && j + 1 < scan.len() {
                let m = scan[j + 1];
                if m == 0x00 {
                    synthetic.push(0xFF);
                    synthetic.push(0x00);
                    j += 2;
                    continue;
                }
                if (0xD0..=0xD7).contains(&m) {
                    synthetic.push(0xFF);
                    synthetic.push(0xD0 | (rst & 0x07));
                    rst = rst.wrapping_add(1) & 0x07;
                    j += 2;
                    continue;
                }
            }
            synthetic.push(b);
            j += 1;
        }
        synthetic.push(0xFF);
        synthetic.push(0xD9);

        Some(decompress_jpeg_color(&synthetic, color).map(|pixels| DecodedBand {
            pixels,
            band_x0: 0,
            band_width: self.width,
            band_y0,
            band_height,
        }))
    }
}
