use bioformats::{ImageMetadata, ImageReader, ImageWriter, PixelType};

fn temp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("bioformats_test_{}", name))
}

/// Round-trip helper: write `data` as a single-plane image, read it back.
fn round_trip(filename: &str, meta: &ImageMetadata, data: &[u8]) -> Vec<u8> {
    let path = temp_path(filename);
    ImageWriter::save(&path, meta, &[data.to_vec()]).expect("write failed");
    let mut reader = ImageReader::open(&path).expect("read back failed");
    reader.open_bytes(0).expect("open_bytes failed")
}

#[test]
fn tiff_round_trip_gray8() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 8;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;
    meta.size_c = 1;

    let data: Vec<u8> = (0u8..64).collect();
    let readback = round_trip("gray8.tif", &meta, &data);
    assert_eq!(readback, data);
}

#[test]
fn tiff_round_trip_gray16() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint16;
    meta.bits_per_pixel = 16;
    meta.image_count = 1;
    meta.size_c = 1;

    // 16 pixels × 2 bytes, values 0..=15 in little-endian
    let data: Vec<u8> = (0u16..16)
        .flat_map(|v| v.to_le_bytes())
        .collect();
    let readback = round_trip("gray16.tif", &meta, &data);
    assert_eq!(readback, data);
}

#[test]
fn tiff_round_trip_rgb8() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;
    meta.size_c = 3;
    meta.is_rgb = true;
    meta.is_interleaved = true;

    let data: Vec<u8> = (0u8..48).collect(); // 4×4×3
    let readback = round_trip("rgb8.tif", &meta, &data);
    assert_eq!(readback, data);
}

#[test]
fn tiff_multi_plane_stack() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 3;
    meta.size_c = 1;
    meta.size_t = 1;
    meta.image_count = 3;

    let planes: Vec<Vec<u8>> = (0u8..3)
        .map(|p| vec![p * 10; 16])
        .collect();

    let path = temp_path("stack.tif");
    ImageWriter::save(&path, &meta, &planes).expect("write failed");

    let mut reader = ImageReader::open(&path).expect("read failed");
    let rmeta = reader.metadata();
    assert_eq!(rmeta.image_count, 3);
    for p in 0u8..3 {
        let plane = reader.open_bytes(p as u32).expect("plane failed");
        assert_eq!(plane.len(), 16);
        assert!(plane.iter().all(|&b| b == p * 10));
    }
}

#[test]
fn tiff_deflate_round_trip() {
    use bioformats::FormatWriter;
    use bioformats::{TiffWriter, WriteCompression};

    let mut meta = ImageMetadata::default();
    meta.size_x = 16;
    meta.size_y = 16;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let data: Vec<u8> = (0u8..=255).cycle().take(256).collect();
    let path = temp_path("deflate.tif");

    let mut writer = TiffWriter::new().with_compression(WriteCompression::Deflate);
    writer.set_metadata(&meta).unwrap();
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &data).unwrap();
    writer.close().unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let readback = reader.open_bytes(0).unwrap();
    assert_eq!(readback, data);
}

#[test]
fn png_round_trip() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 8;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 3;
    meta.is_rgb = true;
    meta.image_count = 1;

    let data: Vec<u8> = (0u8..192).collect(); // 8×8×3
    let readback = round_trip("test.png", &meta, &data);
    assert_eq!(readback, data);
}
