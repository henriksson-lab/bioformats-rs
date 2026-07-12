use std::fs::File;
use std::io::{copy, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use bioformats::{
    CompressedBytes, CompressedExtractionSupport, CompressedTileMode, ImageReader, LossyCodec,
};

fn usage(program: &str) -> ! {
    eprintln!(
        "usage: {program} <input> <output.jpg> [plane=0] [level=0] [col=0] [row=0]\n\
         Extracts one source JPEG-compressed tile/strip without pixel-domain decode/recompress.\n\
         The crop is aligned to the file's compressed block grid."
    );
    std::process::exit(2);
}

fn parse_arg<T: std::str::FromStr>(args: &[String], index: usize, default: T, name: &str) -> T {
    match args.get(index) {
        Some(value) => value.parse().unwrap_or_else(|_| {
            eprintln!("invalid {name}: {value}");
            std::process::exit(2);
        }),
        None => default,
    }
}

fn write_compressed_bytes(bytes: CompressedBytes, output: &Path) -> bioformats::error::Result<u64> {
    let mut out = File::create(output).map_err(bioformats::error::BioFormatsError::Io)?;
    let mut written = 0u64;
    match bytes {
        CompressedBytes::Owned(data) => {
            out.write_all(&data)
                .map_err(bioformats::error::BioFormatsError::Io)?;
            written = data.len() as u64;
        }
        CompressedBytes::FileRange {
            path,
            offset,
            length,
        } => {
            written += copy_range(&path, offset, length, &mut out)?;
        }
        CompressedBytes::FileRanges { ranges } => {
            for range in ranges {
                written += copy_range(&range.path, range.offset, range.length, &mut out)?;
            }
        }
    }
    Ok(written)
}

fn copy_range(
    path: &Path,
    offset: u64,
    length: u64,
    out: &mut File,
) -> bioformats::error::Result<u64> {
    let mut input = File::open(path).map_err(bioformats::error::BioFormatsError::Io)?;
    input
        .seek(SeekFrom::Start(offset))
        .map_err(bioformats::error::BioFormatsError::Io)?;
    let mut limited = input.take(length);
    copy(&mut limited, out).map_err(bioformats::error::BioFormatsError::Io)
}

fn main() -> bioformats::error::Result<()> {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 3 {
        usage(
            args.first()
                .map(String::as_str)
                .unwrap_or("lossless_jpeg_crop"),
        );
    }

    let input = PathBuf::from(&args[1]);
    let output = PathBuf::from(&args[2]);
    let plane = parse_arg(&args, 3, 0u32, "plane");
    let level = parse_arg(&args, 4, 0u32, "level");
    let col = parse_arg(&args, 5, 0u64, "col");
    let row = parse_arg(&args, 6, 0u64, "row");

    let mut reader = ImageReader::open(&input)?;
    let support = reader.compressed_level_info(plane, level)?;
    let info = match support {
        CompressedExtractionSupport::Supported(info) => info,
        CompressedExtractionSupport::NotSupported { reason } => {
            eprintln!("compressed extraction is not supported: {reason}");
            std::process::exit(1);
        }
    };

    if !matches!(info.codec, LossyCodec::Jpeg { .. }) {
        eprintln!("level uses {:?}, not JPEG", info.codec);
        std::process::exit(1);
    }

    let tile = reader.read_compressed_tile(
        plane,
        level,
        col,
        row,
        &[
            CompressedTileMode::OriginalBytes,
            CompressedTileMode::DerivedLosslessJpeg,
        ],
    )?;
    let bytes = write_compressed_bytes(tile.bytes, &output)?;

    println!(
        "wrote {bytes} bytes to {}\n\
         source block: plane={} level={} col={} row={} origin=({}, {}) size={}x{} nominal_tile={}x{} mode={:?}",
        output.display(),
        tile.plane_index,
        tile.level,
        tile.col,
        tile.row,
        tile.origin_x,
        tile.origin_y,
        tile.width,
        tile.height,
        tile.nominal_tile_width,
        tile.nominal_tile_height,
        tile.mode
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::codecs::jpeg::JpegEncoder;
    use image::ColorType;

    fn push_u16_le(data: &mut Vec<u8>, value: u16) {
        data.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32_le(data: &mut Vec<u8>, value: u32) {
        data.extend_from_slice(&value.to_le_bytes());
    }

    fn push_ifd_short(data: &mut Vec<u8>, tag: u16, value: u16) {
        push_u16_le(data, tag);
        push_u16_le(data, 3);
        push_u32_le(data, 1);
        push_u16_le(data, value);
        push_u16_le(data, 0);
    }

    fn push_ifd_long(data: &mut Vec<u8>, tag: u16, value: u32) {
        push_u16_le(data, tag);
        push_u16_le(data, 4);
        push_u32_le(data, 1);
        push_u32_le(data, value);
    }

    fn jpeg_payload() -> Vec<u8> {
        let pixels = [
            255, 0, 0, 0, 255, 0, //
            0, 0, 255, 255, 255, 255,
        ];
        let mut jpeg = Vec::new();
        JpegEncoder::new_with_quality(&mut jpeg, 90)
            .encode(&pixels, 2, 2, ColorType::Rgb8.into())
            .unwrap();
        jpeg
    }

    fn jpeg_compressed_tiff() -> (Vec<u8>, Vec<u8>, u64) {
        let compressed = jpeg_payload();
        let entry_count = 10u16;
        let pixel_offset = 8 + 2 + u32::from(entry_count) * 12 + 4;
        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        push_u16_le(&mut data, 42);
        push_u32_le(&mut data, 8);
        push_u16_le(&mut data, entry_count);
        push_ifd_long(&mut data, 256, 2); // ImageWidth
        push_ifd_long(&mut data, 257, 2); // ImageLength
        push_ifd_short(&mut data, 258, 8); // BitsPerSample
        push_ifd_short(&mut data, 259, 7); // Compression = JPEG
        push_ifd_short(&mut data, 262, 2); // PhotometricInterpretation = RGB
        push_ifd_long(&mut data, 273, pixel_offset); // StripOffsets
        push_ifd_short(&mut data, 277, 3); // SamplesPerPixel
        push_ifd_long(&mut data, 278, 2); // RowsPerStrip
        push_ifd_long(&mut data, 279, compressed.len() as u32); // StripByteCounts
        push_ifd_short(&mut data, 284, 1); // PlanarConfiguration
        push_u32_le(&mut data, 0);
        data.extend_from_slice(&compressed);
        (data, compressed, u64::from(pixel_offset))
    }

    #[test]
    fn example_extracts_decodable_jpeg_without_reencoding() {
        let dir = std::env::temp_dir();
        let input = dir.join(format!(
            "bioformats-lossless-jpeg-crop-example-{}.tif",
            std::process::id()
        ));
        let output = dir.join(format!(
            "bioformats-lossless-jpeg-crop-example-{}.jpg",
            std::process::id()
        ));
        let (tiff, compressed, offset) = jpeg_compressed_tiff();
        std::fs::write(&input, tiff).unwrap();

        let mut reader = ImageReader::open(&input).unwrap();
        let tile = reader
            .read_compressed_tile(0, 0, 0, 0, &[CompressedTileMode::OriginalBytes])
            .unwrap();
        let written = write_compressed_bytes(tile.bytes, &output).unwrap();
        assert_eq!(written, compressed.len() as u64);

        let extracted = std::fs::read(&output).unwrap();
        assert_eq!(extracted, compressed, "JPEG bytes must be copied unchanged");
        assert_eq!(offset, 134);

        let mut decoder = jpeg_decoder::Decoder::new(extracted.as_slice());
        let decoded = decoder.decode().unwrap();
        let info = decoder.info().unwrap();
        assert_eq!((info.width, info.height), (2, 2));
        assert!(!decoded.is_empty());

        let _ = std::fs::remove_file(input);
        let _ = std::fs::remove_file(output);
    }
}
