use crate::common::codec::*;
use crate::common::error::{BioFormatsError, Result};
use super::ifd::Compression;

/// Decompress one strip or tile using the specified TIFF compression scheme.
/// `jpeg_tables` may contain JFIF tables from tag 347 for old-style JPEG tiles.
pub fn decompress(
    data: &[u8],
    compression: Compression,
    expected_len: usize,
    predictor: u16,
    samples_per_pixel: u16,
    bits_per_sample: u16,
    jpeg_tables: Option<&[u8]>,
) -> Result<Vec<u8>> {
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
            return Err(BioFormatsError::UnsupportedFormat(
                "CCITT compression not yet supported".into(),
            ))
        }
        Compression::Group3Fax => {
            return decompress_ccitt_group3(data, 0, 0);
        }
        Compression::Group4Fax => {
            return decompress_ccitt_group4(data, 0, 0);
        }
        Compression::Thunderscan => {
            return Err(BioFormatsError::UnsupportedFormat(
                "Thunderscan compression not yet supported".into(),
            ))
        }
        Compression::Nikon => {
            return decompress_nikon(data, 0, 0, 0);
        }
        Compression::Unknown(c) => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Unknown TIFF compression code {}",
                c
            )))
        }
    };

    // Apply predictor (horizontal differencing)
    if predictor == 2 {
        // 8-bit
        if bits_per_sample == 8 {
            undo_horizontal_differencing(&mut out, samples_per_pixel as usize);
        } else if bits_per_sample == 16 {
            // Reinterpret as u16 slice, apply, reinterpret back
            if out.len() % 2 == 0 {
                let mut u16_data: Vec<u16> = out
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect();
                undo_horizontal_differencing_u16(&mut u16_data, samples_per_pixel as usize);
                out = u16_data
                    .iter()
                    .flat_map(|&v| v.to_le_bytes())
                    .collect();
            }
        }
    }

    // Clamp to expected output length (strips may be padded)
    if out.len() > expected_len {
        out.truncate(expected_len);
    }

    Ok(out)
}
