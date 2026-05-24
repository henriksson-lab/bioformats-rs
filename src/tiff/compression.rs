use super::ifd::Compression;
use crate::common::codec::*;
use crate::common::error::{BioFormatsError, Result};

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
) -> Result<Vec<u8>> {
    let bits_per_pixel = samples_per_pixel as u32 * bits_per_sample as u32;
    let mut out = match compression {
        Compression::None => data.to_vec(),
        Compression::Lzw => decompress_lzw(data)?,
        Compression::Deflate | Compression::DeflateOld => decompress_deflate(data)?,
        Compression::PackBits => decompress_packbits(data)?,
        Compression::JpegNew => decompress_jpeg(data)?,
        Compression::Jpeg => {
            // Old-style JPEG: prepend tables from tag 347 if present
            if let Some(tables) = jpeg_tables {
                let mut combined = Vec::with_capacity(tables.len() + data.len());
                // tables is a JFIF stream; merge into the tile stream at byte 2
                // Simple approach: create a fresh JFIF with the tables bytes inserted
                if tables.len() > 2 && tables[0] == 0xFF && tables[1] == 0xD8 {
                    // Prefix: SOI from tables then tables content (skip SOI of data)
                    combined.extend_from_slice(tables);
                    // Append data after its SOI marker
                    if data.len() > 2 {
                        combined.extend_from_slice(&data[2..]);
                    }
                } else {
                    combined.extend_from_slice(data);
                }
                decompress_jpeg(&combined)?
            } else {
                decompress_jpeg(data)?
            }
        }
        Compression::Zstd => decompress_zstd(data)?,
        Compression::Jpeg2000 => decompress_jpeg2000(data)?,
        Compression::JpegXR => decompress_jpegxr(data)?,
        Compression::Ccitt => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "CCITT compression not yet supported for {row_width}x{block_height} at {bits_per_pixel} bpp"
            )))
        }
        Compression::Group3Fax => {
            return decompress_ccitt_group3(data, row_width, block_height);
        }
        Compression::Group4Fax => {
            return decompress_ccitt_group4(data, row_width, block_height);
        }
        Compression::Thunderscan => {
            return Err(BioFormatsError::UnsupportedFormat(
                "Thunderscan compression not yet supported".into(),
            ))
        }
        Compression::Nikon => {
            return decompress_nikon(data, row_width, block_height, bits_per_pixel);
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

    match bits_per_sample {
        8 => {
            let row_stride = row_width * samples_per_pixel;
            if row_stride == 0 {
                return Ok(());
            }
            for row in data.chunks_mut(row_stride) {
                let usable = row.len() / samples_per_pixel * samples_per_pixel;
                for i in samples_per_pixel..usable {
                    row[i] = row[i].wrapping_add(row[i - samples_per_pixel]);
                }
            }
            Ok(())
        }
        16 => {
            let row_stride = row_width * samples_per_pixel * 2;
            if row_stride == 0 {
                return Ok(());
            }
            for row in data.chunks_mut(row_stride) {
                let sample_count = row.len() / 2;
                let usable = sample_count / samples_per_pixel * samples_per_pixel;
                for i in samples_per_pixel..usable {
                    let cur = i * 2;
                    let prev = (i - samples_per_pixel) * 2;
                    let value = if little_endian {
                        u16::from_le_bytes([row[cur], row[cur + 1]])
                            .wrapping_add(u16::from_le_bytes([row[prev], row[prev + 1]]))
                    } else {
                        u16::from_be_bytes([row[cur], row[cur + 1]])
                            .wrapping_add(u16::from_be_bytes([row[prev], row[prev + 1]]))
                    };
                    let bytes = if little_endian {
                        value.to_le_bytes()
                    } else {
                        value.to_be_bytes()
                    };
                    row[cur..cur + 2].copy_from_slice(&bytes);
                }
            }
            Ok(())
        }
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "TIFF horizontal predictor for {}-bit samples not supported",
            bits_per_sample
        ))),
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
        )
        .expect("predictor decode failed");

        let expected: Vec<u8> = [0x0100u16, 0x0102, 0x0105]
            .into_iter()
            .flat_map(u16::to_be_bytes)
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
            ),
            "literal run overruns input",
        );
        assert_invalid_data(
            decompress(&[0xfe], Compression::PackBits, 3, 1, 1, 8, 3, 1, true, None),
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
        let comment_table = [0xff, 0xd8, 0xff, 0xfe, 0x00, 0x02];

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
        )
        .expect("old-style JPEG tables decode failed");

        assert_eq!(out.len(), 3);
    }

    #[test]
    fn decompression_allows_short_output_for_caller_validation() {
        let out = decompress(&[1, 2], Compression::None, 4, 1, 1, 8, 4, 1, true, None)
            .expect("uncompressed decode failed");

        assert_eq!(out, vec![1, 2]);
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
            ),
            "TIFF predictor 3 not supported",
        );
    }

    #[test]
    fn unsupported_tiff_compressions_return_clear_errors() {
        assert_unsupported(
            decompress(&[], Compression::Ccitt, 0, 1, 1, 1, 17, 23, true, None),
            "CCITT compression not yet supported for 17x23 at 1 bpp",
        );
        assert_unsupported(
            decompress(&[], Compression::Group3Fax, 0, 1, 1, 1, 17, 23, true, None),
            "CCITT Group 3 fax decompression not yet implemented for 17x23",
        );
        assert_unsupported(
            decompress(&[], Compression::Group4Fax, 0, 1, 1, 1, 17, 23, true, None),
            "CCITT Group 4 fax decompression not yet implemented for 17x23",
        );
        assert_unsupported(
            decompress(&[], Compression::Nikon, 0, 1, 3, 12, 17, 23, true, None),
            "Nikon NEF codec not yet implemented for 17x23 at 36 bpp",
        );
        assert_unsupported(
            decompress(&[], Compression::Thunderscan, 0, 1, 1, 1, 0, 0, true, None),
            "Thunderscan compression not yet supported",
        );
    }
}
