use bioformats::{FormatReader, ImageReader, PixelType};
use std::path::Path;

fn assert_region_error(reader: &mut ImageReader, x: u32, y: u32, w: u32, h: u32) {
    assert!(
        reader.open_bytes_region(0, x, y, w, h).is_err(),
        "expected region x={x}, y={y}, w={w}, h={h} to fail"
    );
}

fn assert_non_tiff_region_bounds(reader: &mut ImageReader) {
    assert_region_error(reader, 3, 0, 1, 1);
    assert_region_error(reader, 2, 0, 2, 1);
    assert_region_error(reader, 0, 2, 1, 1);
    assert_region_error(reader, 0, 1, 1, 2);
    assert_region_error(reader, 0, 0, 0, 1);
    assert_region_error(reader, 0, 0, 1, 0);
}

#[test]
fn test_tiff_8x8_gray8() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_8x8_gray8.tif");
    let mut reader = ImageReader::open(&path).expect("open failed");

    assert_eq!(reader.series_count(), 1);

    let meta = reader.metadata();
    assert_eq!(meta.size_x, 8);
    assert_eq!(meta.size_y, 8);
    assert_eq!(meta.image_count, 1);
    assert_eq!(meta.pixel_type, bioformats::PixelType::Uint8);

    let plane = reader.open_bytes(0).expect("open_bytes failed");
    assert_eq!(plane.len(), 64);

    // Data should be ascending ramp 0..63
    for (i, &byte) in plane.iter().enumerate() {
        assert_eq!(byte, i as u8, "pixel {i} mismatch");
    }
}

#[test]
fn test_unknown_file_returns_error() {
    let path = Path::new("/tmp/nonexistent_bioformats_test.xyz");
    assert!(ImageReader::open(path).is_err());
}

#[test]
fn test_png_region_bounds_are_validated() {
    let path = std::env::temp_dir().join("bioformats_region_bounds.png");
    let img = image::RgbImage::from_raw(
        3,
        2,
        vec![
            1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
        ],
    )
    .expect("image construction failed");
    img.save(&path).expect("fixture write failed");

    let mut reader = ImageReader::open(&path).expect("open failed");
    assert_eq!(
        reader
            .open_bytes_region(0, 1, 0, 2, 2)
            .expect("region failed"),
        vec![4, 5, 6, 7, 8, 9, 13, 14, 15, 16, 17, 18]
    );
    assert_non_tiff_region_bounds(&mut reader);
}

#[test]
fn test_jpeg_region_bounds_are_validated() {
    let path = std::env::temp_dir().join("bioformats_region_bounds.jpg");
    let img = image::RgbImage::from_raw(3, 2, vec![10; 18]).expect("image construction failed");
    img.save(&path).expect("fixture write failed");

    let mut reader = ImageReader::open(&path).expect("open failed");
    assert_eq!(
        reader
            .open_bytes_region(0, 1, 0, 2, 2)
            .expect("region failed")
            .len(),
        12
    );
    assert_non_tiff_region_bounds(&mut reader);
}

#[test]
fn test_ome_xml_region_bounds_are_validated() {
    let path = std::env::temp_dir().join("bioformats_region_bounds.ome");
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME>
  <Image ID="Image:0">
    <Pixels DimensionOrder="XYCZT" Type="uint8" SizeX="3" SizeY="2" SizeZ="1" SizeC="1" SizeT="1">
      <BinData Length="6" BigEndian="false">AQIDBAUG</BinData>
    </Pixels>
  </Image>
</OME>"#;
    std::fs::write(&path, xml).expect("fixture write failed");

    let mut reader = ImageReader::open(&path).expect("open failed");
    assert_eq!(
        reader
            .open_bytes_region(0, 1, 0, 2, 2)
            .expect("region failed"),
        vec![2, 3, 5, 6]
    );
    assert_non_tiff_region_bounds(&mut reader);
}

#[test]
fn test_generic_raster_region_bounds_are_validated() {
    let path = std::env::temp_dir().join("bioformats_region_bounds.ppm");
    std::fs::write(
        &path,
        b"P6\n3 2\n255\n\x01\x02\x03\x04\x05\x06\x07\x08\x09\x0a\x0b\x0c\x0d\x0e\x0f\x10\x11\x12",
    )
    .expect("fixture write failed");

    let mut reader = ImageReader::open(&path).expect("open failed");
    assert_eq!(
        reader
            .open_bytes_region(0, 1, 0, 2, 2)
            .expect("region failed"),
        vec![4, 5, 6, 7, 8, 9, 13, 14, 15, 16, 17, 18]
    );
    assert_non_tiff_region_bounds(&mut reader);
}

#[test]
fn test_tiff_region() {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_8x8_gray8.tif");
    let mut reader = ImageReader::open(&path).expect("open failed");

    // Read a 4x4 sub-region starting at (2, 2)
    let region = reader
        .open_bytes_region(0, 2, 2, 4, 4)
        .expect("region failed");
    assert_eq!(region.len(), 16); // 4*4*1 byte

    // Row 2 of original starts at offset 16, pixels 16..23 = [16,17,18,19,20,21,22,23]
    // Starting at x=2 → bytes 18,19,20,21 for first row of region
    assert_eq!(region[0], 2 + 2 * 8); // row 2, col 2 = 18

    assert_region_error(&mut reader, 8, 0, 1, 1);
    assert_region_error(&mut reader, 7, 0, 2, 1);
    assert_region_error(&mut reader, 0, 7, 1, 2);
    assert_region_error(&mut reader, 0, 0, 0, 1);
    assert_region_error(&mut reader, 0, 0, 1, 0);
}

#[test]
fn test_big_endian_tiff_inline_short_tags() {
    let path = std::env::temp_dir().join("bioformats_be_inline_short.tif");
    let data_offset = 8 + 2 + 10 * 12 + 4;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"MM");
    bytes.extend_from_slice(&42u16.to_be_bytes());
    bytes.extend_from_slice(&8u32.to_be_bytes());
    bytes.extend_from_slice(&10u16.to_be_bytes());

    fn short_entry(bytes: &mut Vec<u8>, tag: u16, value: u16) {
        bytes.extend_from_slice(&tag.to_be_bytes());
        bytes.extend_from_slice(&3u16.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&value.to_be_bytes());
        bytes.extend_from_slice(&0u16.to_be_bytes());
    }

    fn long_entry(bytes: &mut Vec<u8>, tag: u16, value: u32) {
        bytes.extend_from_slice(&tag.to_be_bytes());
        bytes.extend_from_slice(&4u16.to_be_bytes());
        bytes.extend_from_slice(&1u32.to_be_bytes());
        bytes.extend_from_slice(&value.to_be_bytes());
    }

    short_entry(&mut bytes, 256, 2); // ImageWidth
    short_entry(&mut bytes, 257, 2); // ImageLength
    short_entry(&mut bytes, 258, 8); // BitsPerSample
    short_entry(&mut bytes, 259, 1); // Compression
    short_entry(&mut bytes, 262, 1); // PhotometricInterpretation
    long_entry(&mut bytes, 273, data_offset as u32); // StripOffsets
    short_entry(&mut bytes, 277, 1); // SamplesPerPixel
    short_entry(&mut bytes, 278, 2); // RowsPerStrip
    long_entry(&mut bytes, 279, 4); // StripByteCounts
    short_entry(&mut bytes, 284, 1); // PlanarConfiguration
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&[1, 2, 3, 4]);

    std::fs::write(&path, bytes).expect("fixture write failed");
    let mut reader = ImageReader::open(&path).expect("open failed");

    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.pixel_type, PixelType::Uint8);
    assert!(!meta.is_little_endian);
    assert_eq!(
        reader.open_bytes(0).expect("open_bytes failed"),
        vec![1, 2, 3, 4]
    );
}

#[test]
fn test_tiff_horizontal_predictor_resets_each_row() {
    use flate2::{write::ZlibEncoder, Compression};
    use std::io::Write;

    let path = std::env::temp_dir().join("bioformats_predictor_rows.tif");
    let differenced = [1u8, 1, 1, 10, 10, 10];
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&differenced).unwrap();
    let compressed = encoder.finish().unwrap();

    let ifd_offset = 8 + compressed.len() as u32;
    let data_offset = 8u32;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&ifd_offset.to_le_bytes());
    bytes.extend_from_slice(&compressed);
    bytes.extend_from_slice(&11u16.to_le_bytes());

    fn short_entry(bytes: &mut Vec<u8>, tag: u16, value: u16) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }

    fn long_entry(bytes: &mut Vec<u8>, tag: u16, value: u32) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    long_entry(&mut bytes, 256, 3); // ImageWidth
    long_entry(&mut bytes, 257, 2); // ImageLength
    short_entry(&mut bytes, 258, 8); // BitsPerSample
    short_entry(&mut bytes, 259, 8); // Deflate
    short_entry(&mut bytes, 262, 1); // PhotometricInterpretation
    long_entry(&mut bytes, 273, data_offset); // StripOffsets
    short_entry(&mut bytes, 277, 1); // SamplesPerPixel
    long_entry(&mut bytes, 278, 2); // RowsPerStrip
    long_entry(&mut bytes, 279, compressed.len() as u32); // StripByteCounts
    short_entry(&mut bytes, 284, 1); // PlanarConfiguration
    short_entry(&mut bytes, 317, 2); // Predictor
    bytes.extend_from_slice(&0u32.to_le_bytes());

    std::fs::write(&path, bytes).expect("fixture write failed");
    let mut reader = ImageReader::open(&path).expect("open failed");
    assert_eq!(
        reader.open_bytes(0).expect("open_bytes failed"),
        vec![1, 2, 3, 10, 20, 30]
    );
}

#[test]
fn test_tiff_white_is_zero_inverts_grayscale() {
    let path = std::env::temp_dir().join("bioformats_white_is_zero.tif");
    let data_offset = 8 + 2 + 10 * 12 + 4;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&10u16.to_le_bytes());

    fn short_entry(bytes: &mut Vec<u8>, tag: u16, value: u16) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }

    fn long_entry(bytes: &mut Vec<u8>, tag: u16, value: u32) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    short_entry(&mut bytes, 256, 2);
    short_entry(&mut bytes, 257, 1);
    short_entry(&mut bytes, 258, 8);
    short_entry(&mut bytes, 259, 1);
    short_entry(&mut bytes, 262, 0); // WhiteIsZero
    long_entry(&mut bytes, 273, data_offset as u32);
    short_entry(&mut bytes, 277, 1);
    short_entry(&mut bytes, 278, 1);
    long_entry(&mut bytes, 279, 2);
    short_entry(&mut bytes, 284, 1);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&[0, 255]);

    std::fs::write(&path, bytes).expect("fixture write failed");
    let mut reader = ImageReader::open(&path).expect("open failed");
    assert_eq!(
        reader.open_bytes(0).expect("open_bytes failed"),
        vec![255, 0]
    );
}

#[test]
fn test_tiff_packed_samples_unpack_to_bytes() {
    let path = std::env::temp_dir().join("bioformats_packed4.tif");
    let data_offset = 8 + 2 + 10 * 12 + 4;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&10u16.to_le_bytes());

    fn short_entry(bytes: &mut Vec<u8>, tag: u16, value: u16) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    fn long_entry(bytes: &mut Vec<u8>, tag: u16, value: u32) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    short_entry(&mut bytes, 256, 4);
    short_entry(&mut bytes, 257, 1);
    short_entry(&mut bytes, 258, 4);
    short_entry(&mut bytes, 259, 1);
    short_entry(&mut bytes, 262, 1);
    long_entry(&mut bytes, 273, data_offset as u32);
    short_entry(&mut bytes, 277, 1);
    short_entry(&mut bytes, 278, 1);
    long_entry(&mut bytes, 279, 2);
    short_entry(&mut bytes, 284, 1);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&[0x12, 0xA0]);

    std::fs::write(&path, bytes).expect("fixture write failed");
    let mut reader = ImageReader::open(&path).expect("open failed");
    assert_eq!(
        reader.open_bytes(0).expect("open_bytes failed"),
        vec![1, 2, 10, 0]
    );
}

#[test]
fn test_tiff_packed_white_is_zero_unpacks_and_inverts() {
    let path = std::env::temp_dir().join("bioformats_packed1_white.tif");
    let data_offset = 8 + 2 + 10 * 12 + 4;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&10u16.to_le_bytes());

    fn short_entry(bytes: &mut Vec<u8>, tag: u16, value: u16) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    fn long_entry(bytes: &mut Vec<u8>, tag: u16, value: u32) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    short_entry(&mut bytes, 256, 8);
    short_entry(&mut bytes, 257, 1);
    short_entry(&mut bytes, 258, 1);
    short_entry(&mut bytes, 259, 1);
    short_entry(&mut bytes, 262, 0);
    long_entry(&mut bytes, 273, data_offset as u32);
    short_entry(&mut bytes, 277, 1);
    short_entry(&mut bytes, 278, 1);
    long_entry(&mut bytes, 279, 1);
    short_entry(&mut bytes, 284, 1);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.push(0b1010_0000);

    std::fs::write(&path, bytes).expect("fixture write failed");
    let mut reader = ImageReader::open(&path).expect("open failed");
    assert_eq!(
        reader.open_bytes(0).expect("open_bytes failed"),
        vec![0, 1, 0, 1, 1, 1, 1, 1]
    );
}

#[test]
fn test_tiff_planar_rgb_strips_are_returned_planar() {
    let path = std::env::temp_dir().join("bioformats_planar_rgb.tif");
    let bits_offset = 8 + 2 + 11 * 12 + 4;
    let strip_offsets_offset = bits_offset + 6;
    let strip_counts_offset = strip_offsets_offset + 12;
    let data_offset = strip_counts_offset + 12;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&11u16.to_le_bytes());

    fn short_entry(bytes: &mut Vec<u8>, tag: u16, value: u16) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    fn offset_entry(bytes: &mut Vec<u8>, tag: u16, typ: u16, count: u32, offset: u32) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&typ.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
    }

    short_entry(&mut bytes, 256, 2);
    short_entry(&mut bytes, 257, 2);
    offset_entry(&mut bytes, 258, 3, 3, bits_offset);
    short_entry(&mut bytes, 259, 1);
    short_entry(&mut bytes, 262, 2);
    offset_entry(&mut bytes, 273, 4, 3, strip_offsets_offset);
    short_entry(&mut bytes, 277, 3);
    short_entry(&mut bytes, 278, 2);
    offset_entry(&mut bytes, 279, 4, 3, strip_counts_offset);
    short_entry(&mut bytes, 284, 2);
    short_entry(&mut bytes, 339, 1);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    for _ in 0..3 {
        bytes.extend_from_slice(&8u16.to_le_bytes());
    }
    for off in [data_offset, data_offset + 4, data_offset + 8] {
        bytes.extend_from_slice(&off.to_le_bytes());
    }
    for count in [4u32, 4, 4] {
        bytes.extend_from_slice(&count.to_le_bytes());
    }
    bytes.extend_from_slice(&[1, 2, 3, 4, 10, 20, 30, 40, 100, 110, 120, 130]);

    std::fs::write(&path, bytes).expect("fixture write failed");
    let mut reader = ImageReader::open(&path).expect("open failed");
    assert_eq!(reader.metadata().size_c, 3);
    assert!(reader.metadata().is_rgb);
    assert!(!reader.metadata().is_interleaved);
    assert_eq!(
        reader.open_bytes(0).expect("open_bytes failed"),
        vec![1, 2, 3, 4, 10, 20, 30, 40, 100, 110, 120, 130]
    );
    assert_eq!(
        reader
            .open_bytes_region(0, 1, 0, 1, 2)
            .expect("region failed"),
        vec![2, 4, 20, 40, 110, 130]
    );
    assert_region_error(&mut reader, 2, 0, 1, 1);
    assert_region_error(&mut reader, 1, 0, 2, 1);
    assert_region_error(&mut reader, 0, 1, 1, 2);
    assert_region_error(&mut reader, 0, 0, 0, 1);
    assert_region_error(&mut reader, 0, 0, 1, 0);
}

#[test]
fn test_tiff_planar_rgb_tiles_are_returned_planar() {
    let path = std::env::temp_dir().join("bioformats_planar_rgb_tiles.tif");
    let bits_offset = 8 + 2 + 11 * 12 + 4;
    let tile_offsets_offset = bits_offset + 6;
    let tile_counts_offset = tile_offsets_offset + 12;
    let data_offset = tile_counts_offset + 12;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&11u16.to_le_bytes());

    fn short_entry(bytes: &mut Vec<u8>, tag: u16, value: u16) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    fn offset_entry(bytes: &mut Vec<u8>, tag: u16, typ: u16, count: u32, offset: u32) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&typ.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
    }

    short_entry(&mut bytes, 256, 2);
    short_entry(&mut bytes, 257, 2);
    offset_entry(&mut bytes, 258, 3, 3, bits_offset);
    short_entry(&mut bytes, 259, 1);
    short_entry(&mut bytes, 262, 2);
    short_entry(&mut bytes, 322, 2);
    short_entry(&mut bytes, 323, 2);
    offset_entry(&mut bytes, 324, 4, 3, tile_offsets_offset);
    short_entry(&mut bytes, 277, 3);
    offset_entry(&mut bytes, 325, 4, 3, tile_counts_offset);
    short_entry(&mut bytes, 284, 2);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    for _ in 0..3 {
        bytes.extend_from_slice(&8u16.to_le_bytes());
    }
    for off in [data_offset, data_offset + 4, data_offset + 8] {
        bytes.extend_from_slice(&off.to_le_bytes());
    }
    for count in [4u32, 4, 4] {
        bytes.extend_from_slice(&count.to_le_bytes());
    }
    bytes.extend_from_slice(&[1, 2, 3, 4, 10, 20, 30, 40, 100, 110, 120, 130]);

    std::fs::write(&path, bytes).expect("fixture write failed");
    let mut reader = ImageReader::open(&path).expect("open failed");
    assert_eq!(reader.metadata().size_c, 3);
    assert!(reader.metadata().is_rgb);
    assert!(!reader.metadata().is_interleaved);
    assert_eq!(
        reader.open_bytes(0).expect("open_bytes failed"),
        vec![1, 2, 3, 4, 10, 20, 30, 40, 100, 110, 120, 130]
    );
    assert_eq!(
        reader
            .open_bytes_region(0, 0, 1, 2, 1)
            .expect("region failed"),
        vec![3, 4, 30, 40, 120, 130]
    );
    assert_region_error(&mut reader, 2, 0, 1, 1);
    assert_region_error(&mut reader, 1, 0, 2, 1);
    assert_region_error(&mut reader, 0, 1, 1, 2);
    assert_region_error(&mut reader, 0, 0, 0, 1);
    assert_region_error(&mut reader, 0, 0, 1, 0);
}

#[test]
fn test_tiff_cmyk_preserves_four_channels_and_inverts() {
    let path = std::env::temp_dir().join("bioformats_cmyk.tif");
    let bits_offset = 8 + 2 + 10 * 12 + 4;
    let data_offset = bits_offset + 8;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&10u16.to_le_bytes());

    fn short_entry(bytes: &mut Vec<u8>, tag: u16, value: u16) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    fn long_entry(bytes: &mut Vec<u8>, tag: u16, value: u32) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    fn offset_entry(bytes: &mut Vec<u8>, tag: u16, typ: u16, count: u32, offset: u32) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&typ.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
    }

    short_entry(&mut bytes, 256, 1);
    short_entry(&mut bytes, 257, 1);
    offset_entry(&mut bytes, 258, 3, 4, bits_offset);
    short_entry(&mut bytes, 259, 1);
    short_entry(&mut bytes, 262, 5);
    long_entry(&mut bytes, 273, data_offset);
    short_entry(&mut bytes, 277, 4);
    short_entry(&mut bytes, 278, 1);
    long_entry(&mut bytes, 279, 4);
    short_entry(&mut bytes, 284, 1);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    for _ in 0..4 {
        bytes.extend_from_slice(&8u16.to_le_bytes());
    }
    bytes.extend_from_slice(&[0, 10, 20, 30]);

    std::fs::write(&path, bytes).expect("fixture write failed");
    let mut reader = ImageReader::open(&path).expect("open failed");
    assert_eq!(reader.metadata().size_c, 4);
    assert!(!reader.metadata().is_rgb);
    assert!(reader.metadata().is_interleaved);
    assert_eq!(
        reader.open_bytes(0).expect("open_bytes failed"),
        vec![255, 245, 235, 225]
    );
}

#[test]
fn test_ome_tiff_prefixed_tiffdata_maps_logical_planes_to_ifds() {
    let path = std::env::temp_dir().join("bioformats_ome_tiffdata_mapping.tif");
    let xml = r#"<ome:OME xmlns:ome="http://www.openmicroscopy.org/Schemas/OME/2016-06"><ome:Image ID="Image:0"><ome:Pixels ID="Pixels:0" Name="quoted > delimiter" DimensionOrder="XYCZT" Type="uint8" SizeX="1" SizeY="1" SizeZ="1" SizeC="3" SizeT="1"><ome:Channel ID="Channel:0:0" SamplesPerPixel="1"/><ome:Channel ID="Channel:0:1" SamplesPerPixel="1"/><ome:Channel ID="Channel:0:2" SamplesPerPixel="1"/><ome:TiffData IFD="2" FirstC="0" PlaneCount="1"/><ome:TiffData IFD="0" FirstC="1" PlaneCount="1"/><ome:TiffData IFD="1" FirstC="2" PlaneCount="1"/></ome:Pixels></ome:Image></ome:OME>"#;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&8u32.to_le_bytes());

    fn short_entry(bytes: &mut Vec<u8>, tag: u16, value: u16) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    fn long_entry(bytes: &mut Vec<u8>, tag: u16, value: u32) -> usize {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        let pos = bytes.len();
        bytes.extend_from_slice(&value.to_le_bytes());
        pos
    }
    fn ascii_entry(bytes: &mut Vec<u8>, tag: u16, count: u32, value: u32) -> usize {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        let pos = bytes.len();
        bytes.extend_from_slice(&value.to_le_bytes());
        pos
    }

    let mut strip_offset_positions = Vec::new();
    let mut image_description_position = None;
    for ifd in 0..3 {
        let ifd_start = bytes.len() as u32;
        let entry_count = if ifd == 0 { 11u16 } else { 10u16 };
        let next_ifd = if ifd == 2 {
            0
        } else {
            ifd_start + 2 + entry_count as u32 * 12 + 4
        };
        bytes.extend_from_slice(&entry_count.to_le_bytes());
        short_entry(&mut bytes, 256, 1);
        short_entry(&mut bytes, 257, 1);
        short_entry(&mut bytes, 258, 8);
        short_entry(&mut bytes, 259, 1);
        short_entry(&mut bytes, 262, 1);
        strip_offset_positions.push(long_entry(&mut bytes, 273, 0));
        short_entry(&mut bytes, 277, 1);
        short_entry(&mut bytes, 278, 1);
        long_entry(&mut bytes, 279, 1);
        short_entry(&mut bytes, 284, 1);
        if ifd == 0 {
            image_description_position =
                Some(ascii_entry(&mut bytes, 270, xml.len() as u32 + 1, 0));
        }
        bytes.extend_from_slice(&next_ifd.to_le_bytes());
    }

    let desc_offset = bytes.len() as u32;
    let desc_pos = image_description_position.expect("description entry");
    bytes[desc_pos..desc_pos + 4].copy_from_slice(&desc_offset.to_le_bytes());
    bytes.extend_from_slice(xml.as_bytes());
    bytes.push(0);

    for (value, pos) in [10u8, 20, 30].into_iter().zip(strip_offset_positions) {
        let data_offset = bytes.len() as u32;
        bytes[pos..pos + 4].copy_from_slice(&data_offset.to_le_bytes());
        bytes.push(value);
    }

    std::fs::write(&path, bytes).expect("fixture write failed");
    let mut reader = ImageReader::open(&path).expect("open failed");
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 3);
    assert_eq!(meta.size_t, 1);
    assert_eq!(meta.image_count, 3);
    assert_eq!(reader.open_bytes(0).expect("plane 0"), vec![30]);
    assert_eq!(reader.open_bytes(1).expect("plane 1"), vec![10]);
    assert_eq!(reader.open_bytes(2).expect("plane 2"), vec![20]);
}

#[test]
fn test_tiff_ycbcr_subsampled_decodes_to_planar_rgb() {
    let path = std::env::temp_dir().join("bioformats_ycbcr_subsampled.tif");
    let data_offset = 8 + 2 + 11 * 12 + 4;
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&11u16.to_le_bytes());

    fn short_entry(bytes: &mut Vec<u8>, tag: u16, value: u16) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }
    fn short_pair_entry(bytes: &mut Vec<u8>, tag: u16, a: u16, b: u16) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&a.to_le_bytes());
        bytes.extend_from_slice(&b.to_le_bytes());
    }
    fn long_entry(bytes: &mut Vec<u8>, tag: u16, value: u32) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&4u16.to_le_bytes());
        bytes.extend_from_slice(&1u32.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
    }

    short_entry(&mut bytes, 256, 2);
    short_entry(&mut bytes, 257, 2);
    short_entry(&mut bytes, 258, 8);
    short_entry(&mut bytes, 259, 1);
    short_entry(&mut bytes, 262, 6);
    long_entry(&mut bytes, 273, data_offset as u32);
    short_entry(&mut bytes, 277, 3);
    short_entry(&mut bytes, 278, 2);
    long_entry(&mut bytes, 279, 6);
    short_entry(&mut bytes, 284, 1);
    short_pair_entry(&mut bytes, 530, 2, 2);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&[10, 20, 30, 40, 128, 128]);

    std::fs::write(&path, bytes).expect("fixture write failed");
    let mut reader = ImageReader::open(&path).expect("open failed");
    assert_eq!(reader.metadata().size_c, 3);
    assert!(reader.metadata().is_rgb);
    assert_eq!(
        reader.open_bytes(0).expect("open_bytes failed"),
        vec![10, 20, 30, 40, 10, 20, 30, 40, 10, 20, 30, 40]
    );
}

#[test]
fn test_tiff_missing_strip_byte_counts_returns_format_error() {
    let path = write_malformed_tiff(
        "bioformats_missing_strip_byte_counts.tif",
        &[
            TiffEntry::Short(256, 2),
            TiffEntry::Short(257, 2),
            TiffEntry::Short(258, 8),
            TiffEntry::Short(259, 1),
            TiffEntry::Short(262, 1),
            TiffEntry::Long(273, 128),
            TiffEntry::Short(277, 1),
            TiffEntry::Short(278, 2),
            TiffEntry::Short(284, 1),
        ],
    );

    assert_tiff_open_error_contains(&path, "StripByteCounts");
}

#[test]
fn test_tiff_missing_strip_offsets_returns_format_error() {
    let path = write_malformed_tiff(
        "bioformats_missing_strip_offsets.tif",
        &[
            TiffEntry::Short(256, 2),
            TiffEntry::Short(257, 2),
            TiffEntry::Short(258, 8),
            TiffEntry::Short(259, 1),
            TiffEntry::Short(262, 1),
            TiffEntry::Short(277, 1),
            TiffEntry::Short(278, 2),
            TiffEntry::Long(279, 4),
            TiffEntry::Short(284, 1),
        ],
    );

    assert_tiff_open_error_contains(&path, "StripOffsets");
}

#[test]
fn test_tiff_zero_rows_per_strip_returns_format_error() {
    let path = write_malformed_tiff(
        "bioformats_zero_rows_per_strip.tif",
        &[
            TiffEntry::Short(256, 2),
            TiffEntry::Short(257, 2),
            TiffEntry::Short(258, 8),
            TiffEntry::Short(259, 1),
            TiffEntry::Short(262, 1),
            TiffEntry::Long(273, 128),
            TiffEntry::Short(277, 1),
            TiffEntry::Short(278, 0),
            TiffEntry::Long(279, 4),
            TiffEntry::Short(284, 1),
        ],
    );

    assert_tiff_open_error_contains(&path, "RowsPerStrip");
}

#[test]
fn test_tiff_planar_strip_count_mismatch_returns_format_error() {
    let path = write_malformed_tiff(
        "bioformats_planar_strip_count_mismatch.tif",
        &[
            TiffEntry::Short(256, 2),
            TiffEntry::Short(257, 2),
            TiffEntry::LongArray(258, &[8, 8, 8]),
            TiffEntry::Short(259, 1),
            TiffEntry::Short(262, 2),
            TiffEntry::LongArray(273, &[128, 132]),
            TiffEntry::Short(277, 3),
            TiffEntry::Short(278, 2),
            TiffEntry::LongArray(279, &[4, 4]),
            TiffEntry::Short(284, 2),
        ],
    );

    assert_tiff_open_error_contains(&path, "expected strip count 3");
}

#[test]
fn test_tiff_missing_tile_width_returns_format_error() {
    let path = write_malformed_tiff(
        "bioformats_missing_tile_width.tif",
        &[
            TiffEntry::Short(256, 2),
            TiffEntry::Short(257, 2),
            TiffEntry::Short(258, 8),
            TiffEntry::Short(259, 1),
            TiffEntry::Short(262, 1),
            TiffEntry::Short(323, 2),
            TiffEntry::Long(324, 128),
            TiffEntry::Long(325, 4),
            TiffEntry::Short(277, 1),
            TiffEntry::Short(284, 1),
        ],
    );

    assert_tiff_open_error_contains(&path, "TileWidth");
}

#[test]
fn test_tiff_missing_tile_byte_counts_returns_format_error() {
    let path = write_malformed_tiff(
        "bioformats_missing_tile_byte_counts.tif",
        &[
            TiffEntry::Short(256, 2),
            TiffEntry::Short(257, 2),
            TiffEntry::Short(258, 8),
            TiffEntry::Short(259, 1),
            TiffEntry::Short(262, 1),
            TiffEntry::Short(322, 2),
            TiffEntry::Short(323, 2),
            TiffEntry::Long(324, 128),
            TiffEntry::Short(277, 1),
            TiffEntry::Short(284, 1),
        ],
    );

    assert_tiff_open_error_contains(&path, "TileByteCounts");
}

#[test]
fn test_tiff_tile_offsets_counts_mismatch_returns_format_error() {
    let path = write_malformed_tiff(
        "bioformats_tile_offsets_counts_mismatch.tif",
        &[
            TiffEntry::Short(256, 4),
            TiffEntry::Short(257, 4),
            TiffEntry::Short(258, 8),
            TiffEntry::Short(259, 1),
            TiffEntry::Short(262, 1),
            TiffEntry::Short(322, 2),
            TiffEntry::Short(323, 2),
            TiffEntry::LongArray(324, &[128, 132, 136, 140]),
            TiffEntry::LongArray(325, &[4, 4, 4]),
            TiffEntry::Short(277, 1),
            TiffEntry::Short(284, 1),
        ],
    );

    assert_tiff_open_error_contains(
        &path,
        "TileOffsets count 4 does not match TileByteCounts count 3",
    );
}

enum TiffEntry<'a> {
    Short(u16, u16),
    Long(u16, u32),
    LongArray(u16, &'a [u32]),
}

fn write_malformed_tiff(name: &str, entries: &[TiffEntry<'_>]) -> std::path::PathBuf {
    let path = std::env::temp_dir().join(name);
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&8u32.to_le_bytes());
    bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());

    let mut array_data = Vec::new();
    let mut array_offset = 8 + 2 + entries.len() as u32 * 12 + 4;
    for entry in entries {
        match *entry {
            TiffEntry::Short(tag, value) => {
                bytes.extend_from_slice(&tag.to_le_bytes());
                bytes.extend_from_slice(&3u16.to_le_bytes());
                bytes.extend_from_slice(&1u32.to_le_bytes());
                bytes.extend_from_slice(&value.to_le_bytes());
                bytes.extend_from_slice(&0u16.to_le_bytes());
            }
            TiffEntry::Long(tag, value) => {
                bytes.extend_from_slice(&tag.to_le_bytes());
                bytes.extend_from_slice(&4u16.to_le_bytes());
                bytes.extend_from_slice(&1u32.to_le_bytes());
                bytes.extend_from_slice(&value.to_le_bytes());
            }
            TiffEntry::LongArray(tag, values) => {
                bytes.extend_from_slice(&tag.to_le_bytes());
                bytes.extend_from_slice(&4u16.to_le_bytes());
                bytes.extend_from_slice(&(values.len() as u32).to_le_bytes());
                bytes.extend_from_slice(&array_offset.to_le_bytes());
                for value in values {
                    array_data.extend_from_slice(&value.to_le_bytes());
                }
                array_offset += values.len() as u32 * 4;
            }
        }
    }
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&array_data);

    std::fs::write(&path, bytes).expect("fixture write failed");
    path
}

fn assert_tiff_open_error_contains(path: &std::path::Path, needle: &str) {
    let mut reader = bioformats::tiff::TiffReader::new();
    let err = match reader.set_id(path) {
        Ok(_) => panic!("malformed TIFF should fail"),
        Err(err) => err,
    };
    let message = err.to_string();
    assert!(
        message.contains(needle),
        "expected error containing {needle:?}, got {message:?}"
    );
}
