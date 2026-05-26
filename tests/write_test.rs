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
    let data: Vec<u8> = (0u16..16).flat_map(|v| v.to_le_bytes()).collect();
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

    let planes: Vec<Vec<u8>> = (0u8..3).map(|p| vec![p * 10; 16]).collect();

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
fn pyramid_tiff_reads_reduced_resolution_for_every_plane() {
    use bioformats::tiff::PyramidOmeTiffWriter;
    use bioformats::FormatWriter;

    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 2;
    meta.size_c = 1;
    meta.size_t = 1;
    meta.image_count = 2;

    let full_planes = vec![vec![10; 16], vec![20; 16]];
    let reduced_planes = vec![vec![11, 12, 13, 14], vec![21, 22, 23, 24]];

    let path = temp_path("two_plane_pyramid.tif");
    let mut writer = PyramidOmeTiffWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_id(&path).unwrap();
    for (plane_idx, plane) in full_planes.iter().enumerate() {
        writer.save_bytes(plane_idx as u32, plane).unwrap();
    }
    writer.add_resolution_level(reduced_planes.clone());
    writer.close().unwrap();

    let mut reader = ImageReader::open(&path).expect("read failed");
    assert_eq!(reader.resolution_count(), 2);

    for (plane_idx, expected) in full_planes.iter().enumerate() {
        assert_eq!(
            reader.open_bytes(plane_idx as u32).unwrap(),
            expected.clone()
        );
    }

    reader.set_resolution(1).unwrap();
    for (plane_idx, expected) in reduced_planes.iter().enumerate() {
        assert_eq!(
            reader.open_bytes(plane_idx as u32).unwrap(),
            expected.clone()
        );
    }
}

#[test]
fn pyramid_tiff_rejects_wrong_subresolution_plane_count() {
    use bioformats::tiff::PyramidOmeTiffWriter;
    use bioformats::FormatWriter;

    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 2;
    meta.size_c = 1;
    meta.size_t = 1;
    meta.image_count = 2;

    let path = temp_path("bad_pyramid_plane_count.tif");
    let mut writer = PyramidOmeTiffWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &[1; 16]).unwrap();
    writer.save_bytes(1, &[2; 16]).unwrap();
    writer.add_resolution_level(vec![vec![3; 4]]);

    let err = writer.close().unwrap_err();
    assert!(
        err.to_string()
            .contains("resolution level 1 has 1 planes, expected 2"),
        "unexpected error: {err}"
    );
}

#[test]
fn pyramid_tiff_rejects_wrong_subresolution_plane_size() {
    use bioformats::tiff::PyramidOmeTiffWriter;
    use bioformats::FormatWriter;

    let mut meta = ImageMetadata::default();
    meta.size_x = 5;
    meta.size_y = 3;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 1;
    meta.image_count = 1;

    let path = temp_path("bad_pyramid_plane_size.tif");
    let mut writer = PyramidOmeTiffWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &[1; 15]).unwrap();
    writer.add_resolution_level(vec![vec![2; 5]]);

    let err = writer.close().unwrap_err();
    assert!(
        err.to_string()
            .contains("resolution level 1 plane 0 has 5 bytes, expected 6 for 3x2"),
        "unexpected error: {err}"
    );
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
fn tiff_writer_rejects_wrong_plane_size() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let path = temp_path("wrong_size.tif");
    let err = ImageWriter::save(&path, &meta, &[vec![0; 15]]).unwrap_err();
    assert!(
        err.to_string().contains("expected 16"),
        "unexpected error: {err}"
    );
}

#[test]
fn tiff_writer_rejects_missing_planes_on_close() {
    use bioformats::{FormatWriter, TiffWriter};

    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 2;

    let path = temp_path("missing_plane.tif");
    let mut writer = TiffWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &[0; 16]).unwrap();
    let err = writer.close().unwrap_err();
    assert!(
        err.to_string().contains("wrote 1 planes, expected 2"),
        "unexpected error: {err}"
    );
}

#[test]
fn tiff_writer_does_not_claim_bigtiff_extension() {
    use bioformats::{FormatWriter, TiffWriter};

    let writer = TiffWriter::new();
    assert!(!writer.is_this_type(&temp_path("classic_only.btf")));
    assert!(writer.is_this_type(&temp_path("classic_ok.tif")));
    assert!(writer.is_this_type(&temp_path("classic_ok.tiff")));
}

#[test]
fn ome_tiff_writer_keeps_resolution_offsets_after_description() {
    use bioformats::tiff::ifd::tag;
    use bioformats::tiff::parser::TiffParser;
    use std::fs::File;
    use std::io::BufReader;

    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 2;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let data = vec![1, 2, 3, 4];
    let path = temp_path("ome_resolution_offsets.ome.tif");
    let ome = bioformats::OmeMetadata::from_image_metadata(&meta);
    ImageWriter::save_ome_tiff(&path, &meta, &ome, &[data]).unwrap();

    let file = File::open(&path).unwrap();
    let mut parser = TiffParser::new(BufReader::new(file)).unwrap();
    let ifds = parser.read_ifds().unwrap();
    let ifd = &ifds[0];
    let rational = |value: &bioformats::tiff::ifd::IfdValue| match value {
        bioformats::tiff::ifd::IfdValue::Rational(v) => v[0].0 as f64 / v[0].1 as f64,
        other => panic!("expected rational, got {other:?}"),
    };
    assert_eq!(rational(ifd.get(tag::X_RESOLUTION).unwrap()), 72.0);
    assert_eq!(rational(ifd.get(tag::Y_RESOLUTION).unwrap()), 72.0);
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
