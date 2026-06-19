use super::ifd::Compression;
use super::nikon::{decompress_nikon as decompress_nikon_tiff, NikonCompressionOptions};
use crate::common::codec::*;
use crate::common::error::{BioFormatsError, Result};

/// How a JPEG-compressed TIFF strip/tile's stored components map to output bytes.
///
/// Mirrors Java's `codecOptions.ycbcr` / `ImageIO` behaviour: an Aperio (and
/// some Leica/Hamamatsu) TIFF that declares `PhotometricInterpretation == RGB`
/// (2) but stores a baseline JPEG with component IDs 1/2/3 must NOT have the
/// YCbCr→RGB transform applied — the stored components already ARE the channel
/// values the file intends. libjpeg / `jpeg_decoder`'s default assumes YCbCr and
/// converts, which corrupts the pixels relative to Java. `Rgb` forces the
/// "components are RGB, emit as-is" path (`ColorTransform::RGB`).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum JpegColor {
    /// Let the JPEG markers / default heuristics decide (YCbCr→RGB for typical
    /// 3-component baseline JPEGs). Used for `PhotometricInterpretation == YCbCr`.
    Default,
    /// Treat the stored components as RGB and emit them without conversion. Used
    /// for `PhotometricInterpretation == RGB` JPEG strips/tiles.
    Rgb,
}

/// Decode a JPEG payload, honouring `color` for the YCbCr↔RGB transform.
pub(crate) fn decompress_jpeg_color(data: &[u8], color: JpegColor) -> Result<Vec<u8>> {
    match color {
        JpegColor::Default => decompress_jpeg(data),
        JpegColor::Rgb => {
            let mut decoder = jpeg_decoder::Decoder::new(crate::common::codec::jpeg_payload(data));
            decoder.set_color_transform(jpeg_decoder::ColorTransform::RGB);
            decoder
                .decode()
                .map_err(|e| BioFormatsError::Codec(e.to_string()))
        }
    }
}

/// Decompress one strip or tile using the specified TIFF compression scheme.
/// `jpeg_tables` may contain JFIF tables from tag 347 for old-style JPEG tiles.
pub fn decompress(
    data: &[u8],
    compression: Compression,
    expected_len: usize,
    predictor: u16,
    samples_per_pixel: u16,
    bits_per_sample: u16,
    row_width: u32,
    block_height: u32,
    little_endian: bool,
    jpeg_tables: Option<&[u8]>,
    nikon_options: Option<&NikonCompressionOptions>,
    jpeg_color: JpegColor,
) -> Result<Vec<u8>> {
    let bits_per_pixel = samples_per_pixel as u32 * bits_per_sample as u32;
    let mut out = match compression {
        Compression::None => data.to_vec(),
        Compression::Lzw => decompress_lzw(data)?,
        Compression::Deflate => decompress_deflate(data)?,
        Compression::DeflateOld => {
            decompress_deflate_raw(data).or_else(|_| decompress_deflate(data))?
        }
        Compression::PackBits => decompress_packbits(data)?,
        Compression::Jpeg | Compression::JpegNew => {
            // New-style (technote, code 7) and old-style (code 6) JPEG both store
            // the abbreviated quantization/Huffman tables in the JPEGTables tag
            // (347), spliced ahead of each tile/strip's entropy-coded scan. Mirror
            // Java TiffParser.getTile: tile = jpegTable[..len-2] + scan[2..].
            if let Some(tables) = jpeg_tables {
                let combined = merge_jpeg_tables(tables, data);
                decompress_jpeg_color(&combined, jpeg_color)?
            } else {
                decompress_jpeg_color(data, jpeg_color)?
            }
        }
        Compression::Zstd => decompress_zstd(data)?,
        Compression::Jpeg2000 => decompress_jpeg2000_with_endianness(data, little_endian)?,
        Compression::JpegXR => decompress_jpegxr(data)?,
        Compression::Ccitt => {
            if bits_per_pixel != 1 {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "CCITT 1D compression requires 1 bpp, got {bits_per_pixel} bpp"
                )));
            }
            return decompress_ccitt_group3(data, row_width, block_height);
        }
        Compression::Group3Fax => {
            return decompress_ccitt_group3(data, row_width, block_height);
        }
        Compression::Group4Fax => {
            return decompress_ccitt_group4(data, row_width, block_height);
        }
        Compression::Thunderscan => {
            return decompress_thunderscan(
                data,
                row_width,
                block_height,
                samples_per_pixel,
                bits_per_sample,
            );
        }
        Compression::Nikon => {
            return decompress_nikon_tiff_strip(
                data,
                row_width,
                block_height,
                bits_per_pixel,
                nikon_options,
            );
        }
        Compression::Unknown(c) => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Unknown TIFF compression code {}",
                c
            )))
        }
    };

    match predictor {
        1 => {}
        2 => undo_tiff_horizontal_differencing(
            &mut out,
            row_width as usize,
            samples_per_pixel as usize,
            bits_per_sample,
            little_endian,
        )?,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "TIFF predictor {} not supported",
                other
            )));
        }
    }

    // Clamp to expected output length (strips may be padded)
    if out.len() > expected_len {
        out.truncate(expected_len);
    }

    Ok(out)
}

pub(crate) fn merge_jpeg_tables(tables: &[u8], data: &[u8]) -> Vec<u8> {
    if tables.is_empty() {
        return data.to_vec();
    }

    let table_payload = if starts_with_soi(tables) {
        strip_optional_eoi(&tables[2..])
    } else {
        strip_optional_eoi(tables)
    };
    let scan_payload = if starts_with_soi(data) {
        &data[2..]
    } else {
        data
    };
    let mut combined = Vec::with_capacity(2 + table_payload.len() + scan_payload.len());
    combined.extend_from_slice(&[0xff, 0xd8]);
    combined.extend_from_slice(table_payload);
    combined.extend_from_slice(scan_payload);
    combined
}

fn starts_with_soi(data: &[u8]) -> bool {
    data.len() >= 2 && data[0] == 0xff && data[1] == 0xd8
}

fn strip_optional_eoi(data: &[u8]) -> &[u8] {
    if data.len() >= 2 && data[data.len() - 2] == 0xff && data[data.len() - 1] == 0xd9 {
        &data[..data.len() - 2]
    } else {
        data
    }
}

fn decompress_thunderscan(
    data: &[u8],
    width: u32,
    height: u32,
    samples_per_pixel: u16,
    bits_per_sample: u16,
) -> Result<Vec<u8>> {
    if bits_per_sample != 4 || samples_per_pixel != 1 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Thunderscan compression requires one 4-bit sample per pixel, got \
             samples_per_pixel={samples_per_pixel}, bits_per_sample={bits_per_sample}"
        )));
    }

    let width = width as usize;
    let height = height as usize;
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }

    let row_bytes = width.div_ceil(2);
    let out_len = row_bytes.checked_mul(height).ok_or_else(|| {
        BioFormatsError::InvalidData("Thunderscan output byte count overflows".into())
    })?;
    let mut out = vec![0u8; out_len];
    let mut i = 0usize;

    for row in 0..height {
        let row_start = row * row_bytes;
        let mut pixels = 0usize;
        let mut last_pixel = 0u8;

        while pixels < width {
            if i >= data.len() {
                return Err(BioFormatsError::InvalidData(format!(
                    "Thunderscan data ended before row {row} was complete"
                )));
            }
            let packet = data[i];
            i += 1;

            match packet & 0xc0 {
                0x00 => {
                    let count = (packet & 0x3f) as usize;
                    for _ in 0..count {
                        thunderscan_put_pixel(
                            &mut out[row_start..row_start + row_bytes],
                            width,
                            &mut pixels,
                            last_pixel,
                        )?;
                    }
                }
                0x40 => {
                    for shift in [4, 2, 0] {
                        let delta_code = (packet >> shift) & 0x03;
                        if delta_code != 2 {
                            last_pixel = thunderscan_add_delta(
                                last_pixel,
                                THUNDERSCAN_2BIT_DELTAS[delta_code as usize],
                            );
                            thunderscan_put_pixel(
                                &mut out[row_start..row_start + row_bytes],
                                width,
                                &mut pixels,
                                last_pixel,
                            )?;
                        }
                    }
                }
                0x80 => {
                    for shift in [3, 0] {
                        let delta_code = (packet >> shift) & 0x07;
                        if delta_code != 4 {
                            last_pixel = thunderscan_add_delta(
                                last_pixel,
                                THUNDERSCAN_3BIT_DELTAS[delta_code as usize],
                            );
                            thunderscan_put_pixel(
                                &mut out[row_start..row_start + row_bytes],
                                width,
                                &mut pixels,
                                last_pixel,
                            )?;
                        }
                    }
                }
                0xc0 => {
                    last_pixel = packet & 0x0f;
                    thunderscan_put_pixel(
                        &mut out[row_start..row_start + row_bytes],
                        width,
                        &mut pixels,
                        last_pixel,
                    )?;
                }
                _ => unreachable!(),
            }
        }
    }

    Ok(out)
}

const THUNDERSCAN_2BIT_DELTAS: [i8; 4] = [0, 1, 0, -1];
const THUNDERSCAN_3BIT_DELTAS: [i8; 8] = [0, 1, 2, 3, 0, -3, -2, -1];

fn thunderscan_add_delta(pixel: u8, delta: i8) -> u8 {
    ((pixel as i16 + delta as i16) & 0x0f) as u8
}

fn thunderscan_put_pixel(
    row: &mut [u8],
    width: usize,
    pixels: &mut usize,
    pixel: u8,
) -> Result<()> {
    if *pixels >= width {
        return Ok(());
    }
    let pixel = pixel & 0x0f;
    let byte = *pixels / 2;
    if *pixels & 1 == 0 {
        row[byte] = pixel << 4;
    } else {
        row[byte] |= pixel;
    }
    *pixels += 1;
    Ok(())
}

fn decompress_nikon_tiff_strip(
    data: &[u8],
    width: u32,
    height: u32,
    bpp: u32,
    options: Option<&NikonCompressionOptions>,
) -> Result<Vec<u8>> {
    let Some(options) = options else {
        return decompress_nikon(data, width, height, bpp);
    };

    if bpp != 12 && bpp != 14 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Nikon NEF compression 34713 decoder is only defined for 12-bit or 14-bit RAW \
             samples; got {bpp} bpp for {width}x{height}"
        )));
    }

    let bits_per_sample = u16::try_from(bpp).map_err(|_| {
        BioFormatsError::UnsupportedFormat(format!(
            "Nikon NEF compression 34713 got unsupported {bpp} bits per pixel"
        ))
    })?;
    decompress_nikon_tiff(data, width, height, bits_per_sample, options)
}

fn undo_tiff_horizontal_differencing(
    data: &mut [u8],
    row_width: usize,
    samples_per_pixel: usize,
    bits_per_sample: u16,
    little_endian: bool,
) -> Result<()> {
    if row_width == 0 || samples_per_pixel == 0 {
        return Ok(());
    }

    // Horizontal differencing (predictor 2) operates on whole-byte samples.
    // Mirrors Java TiffCompression.undifference: each sample is treated as an
    // integer of `bytes_per_sample` bytes (respecting endianness); the previous
    // same-channel sample (one pixel, i.e. `samples_per_pixel` samples, to the
    // left) is added with wrapping overflow and repacked in place.
    let bytes_per_sample = ((bits_per_sample as usize) + 7) / 8;
    if bytes_per_sample == 0 {
        return Ok(());
    }
    if bytes_per_sample > 8 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "TIFF horizontal predictor for {bits_per_sample}-bit samples not supported"
        )));
    }

    // Stride, in samples, between a sample and its left neighbour of the same
    // channel. This is the per-channel horizontal differencing stride.
    let channel_stride = samples_per_pixel;
    let row_stride_bytes = row_width * samples_per_pixel * bytes_per_sample;
    if row_stride_bytes == 0 {
        return Ok(());
    }

    for row in data.chunks_mut(row_stride_bytes) {
        let sample_count = row.len() / bytes_per_sample;
        // Process only whole pixels so the channel stride never reads past the row.
        let usable = sample_count / samples_per_pixel * samples_per_pixel;
        for i in channel_stride..usable {
            let cur = i * bytes_per_sample;
            let prev = (i - channel_stride) * bytes_per_sample;
            add_sample(row, cur, prev, bytes_per_sample, little_endian);
        }
    }
    Ok(())
}

/// Reads the `bytes_per_sample`-wide integers at `cur` and `prev`, adds them
/// with wrapping overflow, and writes the result back at `cur`. Endianness
/// matches the byte order declared by the IFD.
fn add_sample(
    row: &mut [u8],
    cur: usize,
    prev: usize,
    bytes_per_sample: usize,
    little_endian: bool,
) {
    let read = |off: usize| -> u64 {
        let mut buf = [0u8; 8];
        if little_endian {
            buf[..bytes_per_sample].copy_from_slice(&row[off..off + bytes_per_sample]);
            u64::from_le_bytes(buf)
        } else {
            buf[8 - bytes_per_sample..].copy_from_slice(&row[off..off + bytes_per_sample]);
            u64::from_be_bytes(buf)
        }
    };

    let value = read(cur).wrapping_add(read(prev));
    let bytes = if little_endian {
        value.to_le_bytes()
    } else {
        value.to_be_bytes()
    };
    if little_endian {
        row[cur..cur + bytes_per_sample].copy_from_slice(&bytes[..bytes_per_sample]);
    } else {
        row[cur..cur + bytes_per_sample].copy_from_slice(&bytes[8 - bytes_per_sample..]);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::error::BioFormatsError;

    fn assert_unsupported(result: Result<Vec<u8>>, expected: &str) {
        match result {
            Err(BioFormatsError::UnsupportedFormat(message)) => {
                assert!(
                    message.contains(expected),
                    "expected unsupported message containing {expected:?}, got {message:?}"
                );
            }
            other => panic!("expected unsupported error, got {other:?}"),
        }
    }

    fn assert_invalid_data(result: Result<Vec<u8>>, expected: &str) {
        match result {
            Err(BioFormatsError::InvalidData(message)) => {
                assert!(
                    message.contains(expected),
                    "expected invalid-data message containing {expected:?}, got {message:?}"
                );
            }
            other => panic!("expected invalid-data error, got {other:?}"),
        }
    }

    fn bits_to_bytes(bits: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut byte = 0u8;
        let mut used = 0usize;
        for bit in bits.bytes().filter(|b| *b == b'0' || *b == b'1') {
            byte = (byte << 1) | (bit - b'0');
            used += 1;
            if used == 8 {
                out.push(byte);
                byte = 0;
                used = 0;
            }
        }
        if used != 0 {
            out.push(byte << (8 - used));
        }
        out
    }

    fn lzw_encode(data: &[u8]) -> Vec<u8> {
        use weezl::{encode::Encoder, BitOrder};

        let mut out = Vec::new();
        let mut encoder = Encoder::with_tiff_size_switch(BitOrder::Msb, 8);
        encoder
            .into_stream(&mut out)
            .encode_all(data)
            .status
            .expect("LZW fixture encode failed");
        out
    }

    #[test]
    fn tiff_horizontal_predictor_16_bit_respects_little_endian_samples() {
        let mut differenced = Vec::new();
        for sample in [0x0100u16, 0x0002, 0x0003] {
            differenced.extend_from_slice(&sample.to_le_bytes());
        }

        let out = decompress(
            &differenced,
            Compression::None,
            differenced.len(),
            2,
            1,
            16,
            3,
            1,
            true,
            None,
            None,
            JpegColor::Default,
        )
        .expect("predictor decode failed");

        let expected: Vec<u8> = [0x0100u16, 0x0102, 0x0105]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn tiff_horizontal_predictor_16_bit_respects_big_endian_samples() {
        let mut differenced = Vec::new();
        for sample in [0x0100u16, 0x0002, 0x0003] {
            differenced.extend_from_slice(&sample.to_be_bytes());
        }

        let out = decompress(
            &differenced,
            Compression::None,
            differenced.len(),
            2,
            1,
            16,
            3,
            1,
            false,
            None,
            None,
            JpegColor::Default,
        )
        .expect("predictor decode failed");

        let expected: Vec<u8> = [0x0100u16, 0x0102, 0x0105]
            .into_iter()
            .flat_map(u16::to_be_bytes)
            .collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn tiff_horizontal_predictor_accepts_ceil_byte_sample_widths() {
        let differenced = [1u8, 0, 2, 0, 3, 0];

        let out = decompress(
            &differenced,
            Compression::None,
            differenced.len(),
            2,
            1,
            12,
            3,
            1,
            true,
            None,
            None,
            JpegColor::Default,
        )
        .expect("12-bit predictor decode failed");

        let expected: Vec<u8> = [1u16, 3, 6]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(out, expected);
    }

    #[test]
    fn lzw_decompression_applies_horizontal_predictor_per_row() {
        let differenced = [5u8, 2, 2, 10, 10, 10];
        let compressed = lzw_encode(&differenced);

        let out = decompress(
            &compressed,
            Compression::Lzw,
            differenced.len(),
            2,
            1,
            8,
            3,
            2,
            true,
            None,
            None,
            JpegColor::Default,
        )
        .expect("LZW predictor decode failed");

        assert_eq!(out, vec![5, 7, 9, 10, 20, 30]);
    }

    #[test]
    fn packbits_handles_nop_literal_and_repeat_runs() {
        let out = decompress(
            &[0x80, 0x02, 1, 2, 3, 0xfe, 9],
            Compression::PackBits,
            6,
            1,
            1,
            8,
            6,
            1,
            true,
            None,
            None,
            JpegColor::Default,
        )
        .expect("PackBits decode failed");

        assert_eq!(out, vec![1, 2, 3, 9, 9, 9]);
    }

    #[test]
    fn packbits_reports_truncated_literal_and_repeat_runs() {
        assert_invalid_data(
            decompress(
                &[0x02, 1, 2],
                Compression::PackBits,
                3,
                1,
                1,
                8,
                3,
                1,
                true,
                None,
                None,
                JpegColor::Default,
            ),
            "literal run overruns input",
        );
        assert_invalid_data(
            decompress(
                &[0xfe],
                Compression::PackBits,
                3,
                1,
                1,
                8,
                3,
                1,
                true,
                None,
                None,
                JpegColor::Default,
            ),
            "repeat run missing byte",
        );
    }

    #[test]
    fn old_style_jpeg_tables_are_accepted_before_tile_stream() {
        use image::{codecs::jpeg::JpegEncoder, ColorType};

        let mut jpeg = Vec::new();
        JpegEncoder::new(&mut jpeg)
            .encode(&[12, 34, 56], 1, 1, ColorType::Rgb8.into())
            .expect("JPEG fixture encode failed");
        let comment_table = [0xff, 0xd8, 0xff, 0xfe, 0x00, 0x02, 0xff, 0xd9];

        let out = decompress(
            &jpeg,
            Compression::Jpeg,
            3,
            1,
            3,
            8,
            1,
            1,
            true,
            Some(&comment_table),
            None,
            JpegColor::Default,
        )
        .expect("old-style JPEG tables decode failed");

        assert_eq!(out.len(), 3);
    }

    #[test]
    fn jpeg_tables_without_soi_are_still_spliced_java_style() {
        let tables = [0xff, 0xdb, 0x00, 0x04, 0x11, 0x22, 0xff, 0xd9];
        let scan = [0xff, 0xd8, 0xff, 0xda, 0x00, 0x02, 0x33, 0xff, 0xd9];

        let merged = merge_jpeg_tables(&tables, &scan);

        assert_eq!(
            merged,
            vec![
                0xff, 0xd8, // synthetic SOI
                0xff, 0xdb, 0x00, 0x04, 0x11, 0x22, // table payload, EOI stripped
                0xff, 0xda, 0x00, 0x02, 0x33, 0xff, 0xd9, // scan payload, SOI stripped
            ]
        );
    }

    #[test]
    fn decompression_allows_short_output_for_caller_validation() {
        let out = decompress(
            &[1, 2],
            Compression::None,
            4,
            1,
            1,
            8,
            4,
            1,
            true,
            None,
            None,
            JpegColor::Default,
        )
        .expect("uncompressed decode failed");

        assert_eq!(out, vec![1, 2]);
    }

    #[test]
    fn old_deflate_accepts_raw_deflate_streams() {
        use flate2::write::DeflateEncoder;
        use std::io::Write;

        let pixels = b"raw-deflate-tiff-strip";
        let mut encoder = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
        encoder.write_all(pixels).unwrap();
        let compressed = encoder.finish().unwrap();

        let out = decompress(
            &compressed,
            Compression::DeflateOld,
            pixels.len(),
            1,
            1,
            8,
            pixels.len() as u32,
            1,
            true,
            None,
            None,
            JpegColor::Default,
        )
        .expect("old deflate raw stream should decode");

        assert_eq!(out, pixels);
    }

    #[test]
    fn group3_fax_dispatch_decodes_one_dimensional_ccitt() {
        let data = bits_to_bytes(
            "000000000001\
             0111\
             0011\
             1000",
        );

        let out = decompress(
            &data,
            Compression::Group3Fax,
            2,
            1,
            1,
            1,
            10,
            1,
            true,
            None,
            None,
            JpegColor::Default,
        )
        .expect("Group 3 fax decode failed");

        assert_eq!(out, vec![0b0011_1110, 0x00]);
    }

    #[test]
    fn ccitt_dispatch_decodes_one_dimensional_modified_huffman() {
        let data = bits_to_bytes(
            "000000000001\
             0111\
             0011\
             1000",
        );

        let out = decompress(
            &data,
            Compression::Ccitt,
            2,
            1,
            1,
            1,
            10,
            1,
            true,
            None,
            None,
            JpegColor::Default,
        )
        .expect("CCITT 1D decode failed");

        assert_eq!(out, vec![0b0011_1110, 0x00]);
    }

    #[test]
    fn group4_fax_dispatch_decodes_two_dimensional_ccitt() {
        let data = bits_to_bytes(
            "001\
             0111\
             011\
             1\
             111",
        );

        let out = decompress(
            &data,
            Compression::Group4Fax,
            4,
            1,
            1,
            1,
            10,
            2,
            true,
            None,
            None,
            JpegColor::Default,
        )
        .expect("Group 4 fax decode failed");

        assert_eq!(out, vec![0b0011_1100, 0x00, 0b0011_1100, 0x00]);
    }

    #[test]
    fn floating_point_predictor_3_is_reported_as_unsupported() {
        assert_unsupported(
            decompress(
                &[0, 0, 0, 0],
                Compression::None,
                4,
                3,
                1,
                32,
                1,
                1,
                true,
                None,
                None,
                JpegColor::Default,
            ),
            "TIFF predictor 3 not supported",
        );
    }

    #[test]
    fn unsupported_tiff_compressions_return_clear_errors() {
        assert_unsupported(
            decompress(
                &[],
                Compression::Ccitt,
                0,
                1,
                3,
                8,
                17,
                23,
                true,
                None,
                None,
                JpegColor::Default,
            ),
            "CCITT 1D compression requires 1 bpp, got 24 bpp",
        );
        assert_unsupported(
            decompress(
                &[],
                Compression::Nikon,
                0,
                1,
                3,
                12,
                17,
                23,
                true,
                None,
                None,
                JpegColor::Default,
            ),
            "Nikon NEF compression 34713 requires Nikon maker-note IFD tag 150 metadata",
        );
        assert_unsupported(
            decompress(
                &[1, 2, 3, 4, 5],
                Compression::Nikon,
                0,
                1,
                3,
                12,
                17,
                23,
                true,
                None,
                None,
                JpegColor::Default,
            ),
            "compressed strip byte count/maxBytes",
        );
        assert_unsupported(
            decompress(
                &[1, 2, 3, 4, 5],
                Compression::Nikon,
                0,
                1,
                3,
                12,
                17,
                23,
                true,
                None,
                None,
                JpegColor::Default,
            ),
            "5 compressed bytes",
        );
    }

    #[test]
    fn thunderscan_decodes_raw_delta_and_run_packets() {
        let out = decompress(
            &[0xc5, 0x54, 0x99, 0x03],
            Compression::Thunderscan,
            4,
            1,
            1,
            4,
            8,
            1,
            true,
            None,
            None,
            JpegColor::Default,
        )
        .expect("Thunderscan decode failed");

        // ThunderScan (libtiff tif_thunder.c) decode of [0xc5, 0x54, 0x99, 0x03]
        // for an 8-pixel, 1-row strip, lastpixel resetting to 0 at row start:
        //   0xc5 -> RAW (code 0xc0): lastpixel = 5                       => [5]
        //   0x54 -> 2-bit deltas (0x40), shifts 4/2/0, table {0,1,0,-1}:
        //           +1 -> 6, +1 -> 7, +0 -> 7                            => [5,6,7,7]
        //   0x99 -> 3-bit deltas (0x80), shifts 3/0, table {0,1,2,3,0,-3,-2,-1}:
        //           +3 -> 10, +1 -> 11                                   => [5,6,7,7,10,11]
        //   0x03 -> RUN (0x00) of 3 x lastpixel(11); clamped to width 8  => [...,11,11]
        // Packed nibbles (high nibble first): 0x56, 0x77, 0xAB, 0xBB.
        assert_eq!(out, vec![0x56, 0x77, 0xAB, 0xBB]);
    }

    #[test]
    fn thunderscan_resets_predictor_each_row() {
        let out = decompress(
            &[0xc5, 0x01, 0x02],
            Compression::Thunderscan,
            2,
            1,
            1,
            4,
            2,
            2,
            true,
            None,
            None,
            JpegColor::Default,
        )
        .expect("Thunderscan decode failed");

        assert_eq!(out, vec![0x55, 0x00]);
    }

    #[test]
    fn thunderscan_rejects_non_four_bit_samples() {
        assert_unsupported(
            decompress(
                &[],
                Compression::Thunderscan,
                0,
                1,
                1,
                8,
                17,
                23,
                true,
                None,
                None,
                JpegColor::Default,
            ),
            "requires one 4-bit sample per pixel",
        );
    }

    #[test]
    fn nikon_dispatch_with_parsed_options_decodes_with_java_eof_padding() {
        let options = NikonCompressionOptions {
            lossless: true,
            v_predictor: [1, 2, 3, 4],
            curve: vec![0, 1, 2, 3],
            split: -1,
        };

        let out = decompress(
            &[1, 2, 3],
            Compression::Nikon,
            0,
            1,
            1,
            12,
            17,
            23,
            true,
            None,
            Some(&options),
            JpegColor::Default,
        )
        .expect("Nikon decoder should mirror Bio-Formats EOF bit padding");
        assert_eq!(out.len(), (17 * 23 * 12usize).div_ceil(8));
    }

    #[test]
    fn nikon_dispatch_rejects_non_raw_sample_depth_before_decoder_boundary() {
        let options = NikonCompressionOptions {
            lossless: true,
            v_predictor: [1, 2, 3, 4],
            curve: vec![0, 1, 2, 3],
            split: -1,
        };

        assert_unsupported(
            decompress(
                &[1, 2, 3],
                Compression::Nikon,
                0,
                1,
                1,
                16,
                17,
                23,
                true,
                None,
                Some(&options),
                JpegColor::Default,
            ),
            "only defined for 12-bit or 14-bit RAW samples",
        );
    }
}
