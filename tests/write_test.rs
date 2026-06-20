use bioformats::{
    FormatReader, FormatWriter, ImageMetadata, ImageReader, ImageWriter, MetadataValue, PixelType,
};

fn temp_path(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bioformats_test_{}_{}_{}",
        std::process::id(),
        nanos,
        name
    ))
}

fn dicom_vr_has_long_length(vr: &[u8; 2]) -> bool {
    matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
    )
}

fn dicom_element(path: &std::path::Path, group: u16, elem: u16) -> ([u8; 2], Vec<u8>) {
    let bytes = std::fs::read(path).expect("read DICOM file");
    let mut offset = 132;
    while offset + 8 <= bytes.len() {
        let current_group = u16::from_le_bytes([bytes[offset], bytes[offset + 1]]);
        let current_elem = u16::from_le_bytes([bytes[offset + 2], bytes[offset + 3]]);
        let vr = [bytes[offset + 4], bytes[offset + 5]];
        let (value_offset, value_len) = if dicom_vr_has_long_length(&vr) {
            let len = u32::from_le_bytes([
                bytes[offset + 8],
                bytes[offset + 9],
                bytes[offset + 10],
                bytes[offset + 11],
            ]) as usize;
            (offset + 12, len)
        } else {
            let len = u16::from_le_bytes([bytes[offset + 6], bytes[offset + 7]]) as usize;
            (offset + 8, len)
        };
        let value_end = value_offset + value_len;
        assert!(value_end <= bytes.len(), "DICOM element exceeds file");
        if current_group == group && current_elem == elem {
            return (vr, bytes[value_offset..value_end].to_vec());
        }
        offset = value_end;
    }
    panic!("missing DICOM element ({group:04X},{elem:04X})");
}

fn dicom_u16(path: &std::path::Path, group: u16, elem: u16) -> u16 {
    let (vr, value) = dicom_element(path, group, elem);
    assert_eq!(vr, *b"US");
    assert_eq!(value.len(), 2);
    u16::from_le_bytes([value[0], value[1]])
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
fn dicom_writer_derives_16_bit_depth_from_pixel_type_when_default_bits_per_pixel() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint16;
    meta.image_count = 1;
    meta.size_c = 1;

    let data: Vec<u8> = [1u16, 2].into_iter().flat_map(u16::to_le_bytes).collect();
    let path = temp_path("dicom_uint16_default_bpp.dcm");
    ImageWriter::save(&path, &meta, &[data.clone()]).expect("DICOM write failed");

    assert_eq!(dicom_u16(&path, 0x0028, 0x0100), 16);
    assert_eq!(dicom_u16(&path, 0x0028, 0x0101), 16);
    assert_eq!(dicom_u16(&path, 0x0028, 0x0102), 15);
    let (vr, pixel_data) = dicom_element(&path, 0x7FE0, 0x0010);
    assert_eq!(vr, *b"OW");
    assert_eq!(pixel_data, data);
}

#[test]
fn dicom_writer_uses_pixel_type_when_bits_per_pixel_is_inconsistent() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 16;
    meta.image_count = 1;
    meta.size_c = 1;

    let path = temp_path("dicom_uint8_inconsistent_bpp.dcm");
    ImageWriter::save(&path, &meta, &[vec![3, 4]]).expect("DICOM write failed");

    assert_eq!(dicom_u16(&path, 0x0028, 0x0100), 8);
    assert_eq!(dicom_u16(&path, 0x0028, 0x0101), 8);
    assert_eq!(dicom_u16(&path, 0x0028, 0x0102), 7);
    let (vr, pixel_data) = dicom_element(&path, 0x7FE0, 0x0010);
    assert_eq!(vr, *b"OB");
    assert_eq!(pixel_data, vec![3, 4]);
}

#[test]
fn dicom_writer_writes_rgb_planar_configuration_and_pads_odd_ob_pixel_data() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 1;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;
    meta.size_c = 3;
    meta.is_rgb = true;
    meta.is_interleaved = true;

    let path = temp_path("dicom_rgb_odd_ob.dcm");
    ImageWriter::save(&path, &meta, &[vec![10, 20, 30]]).expect("DICOM write failed");

    assert_eq!(dicom_u16(&path, 0x0028, 0x0002), 3);
    assert_eq!(dicom_u16(&path, 0x0028, 0x0006), 0);
    let (vr, pixel_data) = dicom_element(&path, 0x7FE0, 0x0010);
    assert_eq!(vr, *b"OB");
    assert_eq!(pixel_data, vec![10, 20, 30, 0]);
}

#[test]
fn dicom_writer_rejects_dimensions_that_exceed_rows_columns_limit() {
    let mut meta = ImageMetadata::default();
    meta.size_x = u16::MAX as u32 + 1;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;
    meta.size_c = 1;

    let path = temp_path("dicom_too_wide.dcm");
    let err = ImageWriter::save(&path, &meta, &[vec![0; meta.size_x as usize]]).unwrap_err();

    assert!(
        err.to_string().contains("Rows/Columns limit"),
        "unexpected error: {err}"
    );
}

#[test]
fn dicom_writer_rejects_bit_pixel_type() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Bit;
    meta.image_count = 1;

    let path = temp_path("dicom_bit_rejected.dcm");
    let err = ImageWriter::save(&path, &meta, &[vec![0; 8]]).unwrap_err();

    assert!(
        err.to_string().contains("does not support PixelType::Bit"),
        "unexpected error: {err}"
    );
    assert!(
        !path.exists(),
        "DICOM writer created output before rejecting bit pixels"
    );
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
    let expected: Vec<u8> = (0..3)
        .flat_map(|channel| data.chunks_exact(3).map(move |pixel| pixel[channel]))
        .collect();
    assert_eq!(readback, expected);
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
    assert_eq!(reader.metadata().size_x, 4);
    assert_eq!(reader.metadata().size_y, 4);

    for (plane_idx, expected) in full_planes.iter().enumerate() {
        assert_eq!(
            reader.open_bytes(plane_idx as u32).unwrap(),
            expected.clone()
        );
    }

    reader.set_resolution(1).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().image_count, 2);
    for (plane_idx, expected) in reduced_planes.iter().enumerate() {
        assert_eq!(
            reader.open_bytes(plane_idx as u32).unwrap(),
            expected.clone()
        );
    }

    reader.set_series(0).unwrap();
    assert_eq!(reader.metadata().size_x, 4);
    assert_eq!(reader.metadata().size_y, 4);
    assert_eq!(reader.open_bytes(0).unwrap(), full_planes[0]);
}

#[test]
fn pyramid_tiff_rejects_wrong_subresolution_plane_count() {
    use bioformats::tiff::PyramidOmeTiffWriter;

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
fn pyramid_tiff_validation_error_does_not_create_output() {
    use bioformats::tiff::PyramidOmeTiffWriter;

    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let path = temp_path("bad_empty_pyramid.tif");
    let mut writer = PyramidOmeTiffWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_id(&path).unwrap();

    let err = writer.close().unwrap_err();
    assert!(
        err.to_string().contains("No resolution levels provided"),
        "unexpected error: {err}"
    );
    assert!(
        !path.exists(),
        "validation failure created {}",
        path.display()
    );
}

#[test]
fn pyramid_tiff_rejects_wrong_subresolution_plane_size() {
    use bioformats::tiff::PyramidOmeTiffWriter;

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
    assert!(
        !path.exists(),
        "wrong plane size should be rejected before creating output"
    );
}

#[test]
fn direct_tiff_writer_accepts_larger_plane_buffer_like_java() {
    use bioformats::TiffWriter;

    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 2;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let path = temp_path("direct_larger_plane_buffer.tif");
    let mut writer = TiffWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &[1, 2, 3, 4, 99, 100]).unwrap();
    writer.close().unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
}

#[test]
fn tiff_writer_suffixes_match_java_big_tiff_suffixes() {
    use bioformats::tiff::PyramidOmeTiffWriter;
    use bioformats::TiffWriter;

    let tiff = TiffWriter::new();
    for name in [
        "plain.tif",
        "plain.tiff",
        "plain.tf2",
        "plain.tf8",
        "plain.btf",
    ] {
        assert!(tiff.is_this_type(std::path::Path::new(name)), "{name}");
    }

    let pyramid = PyramidOmeTiffWriter::new();
    for name in [
        "pyramid.ome.tif",
        "pyramid.ome.tiff",
        "pyramid.ome.tf2",
        "pyramid.ome.tf8",
        "pyramid.ome.btf",
    ] {
        assert!(pyramid.is_this_type(std::path::Path::new(name)), "{name}");
    }
    for name in [
        "plain.tif",
        "plain.tiff",
        "plain.tf2",
        "plain.tf8",
        "plain.btf",
    ] {
        assert!(!pyramid.is_this_type(std::path::Path::new(name)), "{name}");
    }
}

#[test]
fn image_writer_ome_tiff_dispatch_matches_java_pyramid_first_order() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 2;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 1;
    meta.image_count = 1;

    let path = temp_path("generic_dispatch.ome.tif");
    ImageWriter::save(&path, &meta, &[vec![1, 2, 3, 4]]).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert!(
        bytes.windows(b"<OME".len()).any(|window| window == b"<OME"),
        "generic .ome.tif dispatch should write OME metadata like Java's OME-TIFF writers"
    );
    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
}

#[test]
fn tiff_writer_rejects_missing_planes_on_close() {
    use bioformats::TiffWriter;

    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 2;
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
fn tiff_writer_close_before_set_id_keeps_metadata_for_retry() {
    use bioformats::TiffWriter;

    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let path = temp_path("retry_after_uninitialized_close.tif");
    let mut writer = TiffWriter::new();
    writer.set_metadata(&meta).unwrap();
    let err = writer.close().unwrap_err();
    assert!(
        err.to_string().contains("wrote 0 planes, expected 1"),
        "unexpected error: {err}"
    );

    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &[0; 16]).unwrap();
    writer.close().unwrap();
}

#[test]
fn direct_tiff_writer_derives_plane_count_from_dimensions() {
    use bioformats::TiffWriter;

    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 3;
    meta.image_count = 1;

    let path = temp_path("direct_tiff_dimension_plane_count.tif");
    let mut writer = TiffWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &[0; 16]).unwrap();

    let err = writer.close().unwrap_err();

    assert!(
        err.to_string().contains("wrote 1 planes, expected 3"),
        "unexpected error: {err}"
    );
}

#[test]
fn image_writer_save_rejects_wrong_plane_count() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 2;
    meta.image_count = 2;

    let path = temp_path("wrong_plane_count.tif");
    let err = ImageWriter::save(&path, &meta, &[vec![0; 16]]).unwrap_err();

    assert!(
        err.to_string().contains("received 1 planes, expected 2"),
        "unexpected error: {err}"
    );
}

#[test]
fn image_writer_rejects_image_count_above_dimensions() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 1;
    meta.size_c = 1;
    meta.size_t = 1;
    meta.image_count = 2;

    let path = temp_path("inconsistent_plane_count.tif");
    let err = ImageWriter::save(&path, &meta, &[vec![0; 16], vec![1; 16]]).unwrap_err();

    assert!(
        err.to_string()
            .contains("image_count 2 exceeds dimensional plane count 1"),
        "unexpected error: {err}"
    );
}

#[test]
fn image_writer_reports_native_vendor_writers_as_explicitly_untranslated() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 1;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 1;
    meta.image_count = 1;

    for ext in ["lif", "nd2", "czi"] {
        let path = temp_path(&format!("native_vendor_writer.{ext}"));
        let err = ImageWriter::save(&path, &meta, &[vec![0]]).unwrap_err();
        let message = err.to_string();
        assert!(
            message.contains(&format!("native .{ext} writing is not registered")),
            "unexpected {ext} error: {err}"
        );
        assert!(
            message.contains("no LIF/ND2/CZI writer to translate"),
            "missing Java parity rationale for {ext}: {err}"
        );
        assert!(
            !path.exists(),
            "unsupported native writer created {}",
            path.display()
        );

        let stream_path = temp_path(&format!("native_vendor_stream_writer.{ext}"));
        let stream_err = match ImageWriter::open(&stream_path, &meta) {
            Ok(_) => panic!("streaming writer unexpectedly opened {ext}"),
            Err(err) => err,
        };
        assert!(
            stream_err
                .to_string()
                .contains("no LIF/ND2/CZI writer to translate"),
            "missing streaming Java parity rationale for {ext}: {stream_err}"
        );
    }
}

#[test]
fn image_writer_save_accepts_larger_plane_buffer_like_java() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 2;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 1;
    meta.image_count = 1;

    let path = temp_path("larger_plane_buffer.tif");
    ImageWriter::save(&path, &meta, &[vec![1, 2, 3, 4, 99, 100]]).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
}

#[test]
fn image_writer_streaming_accepts_larger_plane_buffer_like_java() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 2;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 1;
    meta.image_count = 1;

    let path = temp_path("stream_larger_plane_buffer.tif");
    let mut writer = ImageWriter::open(&path, &meta).unwrap();
    writer.save_bytes(0, &[5, 6, 7, 8, 101, 102]).unwrap();
    writer.close().unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![5, 6, 7, 8]);
}

#[test]
fn image_writer_derives_missing_plane_count_from_dimensions() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 2;
    meta.size_c = 2;
    meta.size_t = 1;
    meta.image_count = 1;

    let path = temp_path("dimension_plane_count.tif");
    let err = ImageWriter::save(&path, &meta, &[vec![0; 16]]).unwrap_err();

    assert!(
        err.to_string().contains("received 1 planes, expected 4"),
        "unexpected error: {err}"
    );
}

#[test]
fn image_writer_treats_rgb_channels_as_samples_not_planes() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 3;
    meta.image_count = 1;
    meta.is_rgb = true;
    meta.is_interleaved = true;

    let path = temp_path("rgb_channels_one_plane.tif");
    ImageWriter::save(&path, &meta, &[vec![0; 48]]).expect("RGB plane should write as one plane");
}

#[test]
fn image_writer_open_rejects_stack_for_single_plane_format() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 2;
    meta.image_count = 2;

    let path = temp_path("stack.jpg");
    let err = match ImageWriter::open(&path, &meta) {
        Ok(_) => panic!("JPEG stack unexpectedly opened for writing"),
        Err(err) => err,
    };

    assert!(
        err.to_string().contains("does not support stacks"),
        "unexpected error: {err}"
    );
}

#[test]
fn generic_png_writer_uses_apng_stack_writer_like_java() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 1;
    meta.size_z = 2;
    meta.size_c = 1;
    meta.size_t = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 2;

    let path = temp_path("java_style_stack.png");
    ImageWriter::save(&path, &meta, &[vec![10, 20], vec![30, 40]]).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    assert!(
        bytes.windows(4).any(|chunk| chunk == b"acTL"),
        "generic .png writer should emit APNG control chunk like Java APNGWriter"
    );

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 20]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![30, 40]);
}

#[test]
fn image_writer_streaming_rejects_out_of_range_plane() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let path = temp_path("out_of_range_stream.tif");
    let mut writer = ImageWriter::open(&path, &meta).unwrap();
    let err = writer.save_bytes(1, &[0; 16]).unwrap_err();

    assert!(
        err.to_string().contains("Plane index 1 out of range"),
        "unexpected error: {err}"
    );
}

#[test]
fn image_writer_allows_retry_after_incomplete_close() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 2;
    meta.image_count = 2;

    let path = temp_path("closed_after_missing_plane.tif");
    let mut writer = ImageWriter::open(&path, &meta).unwrap();
    writer.save_bytes(0, &[0; 16]).unwrap();
    let first = writer.close().unwrap_err();
    assert!(
        first.to_string().contains("wrote 1 planes, expected 2"),
        "unexpected error: {first}"
    );

    let second = writer.close().unwrap_err();
    assert!(
        second.to_string().contains("wrote 1 planes, expected 2"),
        "unexpected error: {second}"
    );

    writer.save_bytes(1, &[1; 16]).unwrap();
    writer.close().unwrap();
    let already_closed = writer.close().unwrap_err();
    assert!(
        already_closed.to_string().contains("writer already closed"),
        "unexpected error: {already_closed}"
    );
}

fn direct_stack_writer_cases() -> Vec<(
    &'static str,
    &'static str,
    Box<dyn bioformats::FormatWriter>,
)> {
    vec![
        (
            "ICS",
            "ics",
            Box::new(bioformats::formats::ics::IcsWriter::new()),
        ),
        (
            "MRC",
            "mrc",
            Box::new(bioformats::formats::mrc::MrcWriter::new()),
        ),
        (
            "FITS",
            "fits",
            Box::new(bioformats::formats::fits::FitsWriter::new()),
        ),
        (
            "NRRD",
            "nrrd",
            Box::new(bioformats::formats::nrrd::NrrdWriter::new()),
        ),
        (
            "MetaImage",
            "mha",
            Box::new(bioformats::formats::metaimage::MetaImageWriter::new()),
        ),
        (
            "OME-XML",
            "ome",
            Box::new(bioformats::formats::ome_xml::OmeXmlWriter::new()),
        ),
        (
            "AVI",
            "avi",
            Box::new(bioformats::formats::avi::AviWriter::new()),
        ),
        (
            "DICOM",
            "dcm",
            Box::new(bioformats::formats::dicom::DicomWriter::new()),
        ),
    ]
}

fn stack_writer_meta() -> ImageMetadata {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 2;
    meta.size_c = 1;
    meta.size_t = 1;
    meta.image_count = 2;
    meta
}

#[test]
fn axis_flattening_writers_reject_unsupported_c_t_metadata() {
    let mut c_meta = ImageMetadata::default();
    c_meta.size_x = 2;
    c_meta.size_y = 2;
    c_meta.pixel_type = PixelType::Uint8;
    c_meta.size_z = 1;
    c_meta.size_c = 2;
    c_meta.size_t = 1;
    c_meta.image_count = 2;

    let mut t_meta = ImageMetadata::default();
    t_meta.size_x = 2;
    t_meta.size_y = 2;
    t_meta.pixel_type = PixelType::Uint8;
    t_meta.size_z = 1;
    t_meta.size_c = 1;
    t_meta.size_t = 2;
    t_meta.image_count = 2;

    let cases: Vec<(&str, &str, ImageMetadata, Box<dyn bioformats::FormatWriter>)> = vec![
        (
            "FITS",
            "fits",
            c_meta.clone(),
            Box::new(bioformats::formats::fits::FitsWriter::new()),
        ),
        (
            "MetaImage",
            "mha",
            t_meta.clone(),
            Box::new(bioformats::formats::metaimage::MetaImageWriter::new()),
        ),
        (
            "MRC",
            "mrc",
            c_meta.clone(),
            Box::new(bioformats::formats::mrc::MrcWriter::new()),
        ),
        (
            "NRRD",
            "nrrd",
            c_meta,
            Box::new(bioformats::formats::nrrd::NrrdWriter::new()),
        ),
    ];

    for (name, ext, meta, mut writer) in cases {
        let err = writer.set_metadata(&meta).unwrap_err();
        assert!(
            err.to_string().contains("preserve") || err.to_string().contains("cannot safely"),
            "{name}: unexpected error: {err}"
        );

        let path = temp_path(&format!("axis_flatten_{name}.{ext}"));
        let err = ImageWriter::save(&path, &meta, &[vec![0; 4], vec![1; 4]]).unwrap_err();
        assert!(
            err.to_string().contains("preserve") || err.to_string().contains("cannot safely"),
            "{name}: unexpected ImageWriter error: {err}"
        );
    }
}

#[test]
fn nrrd_writer_preserves_grayscale_time_axis() {
    let mut meta = ImageMetadata::default();
    // The NRRD reader follows Bio-Formats' positional heuristic: a leading
    // dimension in 2..=16 is treated as channels. Use an unambiguous X size so
    // the written fourth axis is recovered as T.
    meta.size_x = 17;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 1;
    meta.size_c = 1;
    meta.size_t = 2;
    meta.image_count = 2;

    let planes = vec![vec![1; 17], vec![2; 17]];
    let path = temp_path("nrrd_gray_time.nrrd");
    ImageWriter::save(&path, &meta, &planes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_z, 1);
    assert_eq!(reader.metadata().size_t, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), planes[0]);
    assert_eq!(reader.open_bytes(1).unwrap(), planes[1]);
}

#[test]
fn nrrd_writer_preserves_rgb_time_axis() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 1;
    meta.size_c = 3;
    meta.size_t = 2;
    meta.image_count = 2;
    meta.is_rgb = true;
    meta.is_interleaved = true;

    let planes = vec![vec![1, 2, 3, 4, 5, 6], vec![7, 8, 9, 10, 11, 12]];
    let path = temp_path("nrrd_rgb_time.nrrd");
    ImageWriter::save(&path, &meta, &planes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_c, 3);
    assert_eq!(reader.metadata().size_z, 1);
    assert_eq!(reader.metadata().size_t, 2);
    assert!(reader.metadata().is_rgb);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), planes[0]);
    assert_eq!(reader.open_bytes(1).unwrap(), planes[1]);
}

#[test]
fn ics_writer_describes_rgb_as_interleaved_channel_axis() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 1;
    meta.size_c = 3;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 8;
    meta.image_count = 1;
    meta.is_rgb = true;
    meta.is_interleaved = true;

    let path = temp_path("ics_rgb_interleaved.ics");
    ImageWriter::save(&path, &meta, &[vec![1, 2, 3, 4, 5, 6]]).unwrap();

    let mut reader = bioformats::formats::ics::IcsReader::new();
    reader.set_id(&path).unwrap();
    assert!(reader.metadata().is_rgb);
    assert!(reader.metadata().is_interleaved);
    assert_eq!(reader.metadata().size_c, 3);
    assert_eq!(reader.metadata().image_count, 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
}

#[test]
fn ics_writer_reorders_non_rgb_planes_to_declared_xyztc_layout() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 1;
    meta.size_y = 1;
    meta.size_z = 1;
    meta.size_c = 2;
    meta.size_t = 2;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 8;
    meta.image_count = 4;
    meta.dimension_order = bioformats::common::metadata::DimensionOrder::XYCZT;

    let path = temp_path("ics_ct_reordered.ics");
    ImageWriter::save(&path, &meta, &[vec![10], vec![20], vec![30], vec![40]]).unwrap();

    let mut reader = bioformats::formats::ics::IcsReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(
        reader.metadata().dimension_order,
        bioformats::common::metadata::DimensionOrder::XYTCZ
    );
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![30]);
    assert_eq!(reader.open_bytes(2).unwrap(), vec![20]);
    assert_eq!(reader.open_bytes(3).unwrap(), vec![40]);
}

#[test]
fn ics_writer_accepts_ids_suffix_like_java_ics1_pair() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 1;
    meta.size_c = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 8;
    meta.image_count = 1;

    let ids_path = temp_path("writer_pair.ids");
    let ics_path = ids_path.with_extension("ics");
    ImageWriter::save(&ids_path, &meta, &[vec![7, 9]]).unwrap();

    assert!(ids_path.exists(), "ICS writer did not create .ids pixels");
    assert!(ics_path.exists(), "ICS writer did not create .ics metadata");

    let header = std::fs::read_to_string(&ics_path).unwrap();
    assert!(header.contains("ics_version\t1.0"));
    assert!(header.contains("filename\t"));
    assert!(header.contains("layout\torder\tbits x y"));
    assert_eq!(std::fs::read(&ids_path).unwrap(), vec![7, 9]);

    let mut from_ics = bioformats::formats::ics::IcsReader::new();
    from_ics.set_id(&ics_path).unwrap();
    assert_eq!(from_ics.open_bytes(0).unwrap(), vec![7, 9]);

    let mut from_ids = bioformats::formats::ics::IcsReader::new();
    from_ids.set_id(&ids_path).unwrap();
    assert_eq!(from_ids.open_bytes(0).unwrap(), vec![7, 9]);
}

#[test]
fn scientific_writers_emit_bytes_matching_declared_endianness() {
    let mut big_meta = ImageMetadata::default();
    big_meta.size_x = 1;
    big_meta.size_y = 1;
    big_meta.pixel_type = PixelType::Uint16;
    big_meta.bits_per_pixel = 16;
    big_meta.image_count = 1;
    big_meta.size_c = 1;
    big_meta.is_little_endian = false;

    let fits = temp_path("writer_big_input.fits");
    ImageWriter::save(&fits, &big_meta, &[vec![0x12, 0x34]]).unwrap();
    let mut fits_reader = bioformats::formats::fits::FitsReader::new();
    fits_reader.set_id(&fits).unwrap();
    assert!(!fits_reader.metadata().is_little_endian);
    assert_eq!(fits_reader.open_bytes(0).unwrap(), vec![0x12, 0x34]);

    let cases: Vec<(&str, &str, Box<dyn Fn(&std::path::Path) -> Vec<u8>>)> = vec![
        (
            "ICS",
            "writer_big_input.ics",
            Box::new(|p| {
                let mut r = bioformats::formats::ics::IcsReader::new();
                r.set_id(p).unwrap();
                r.open_bytes(0).unwrap()
            }),
        ),
        (
            "MRC",
            "writer_big_input.mrc",
            Box::new(|p| {
                let mut r = bioformats::formats::mrc::MrcReader::new();
                r.set_id(p).unwrap();
                r.open_bytes(0).unwrap()
            }),
        ),
        (
            "NRRD",
            "writer_big_input.nrrd",
            Box::new(|p| {
                let mut r = bioformats::formats::nrrd::NrrdReader::new();
                r.set_id(p).unwrap();
                r.open_bytes(0).unwrap()
            }),
        ),
        (
            "MetaImage",
            "writer_big_input.mha",
            Box::new(|p| {
                let mut r = bioformats::formats::metaimage::MetaImageReader::new();
                r.set_id(p).unwrap();
                r.open_bytes(0).unwrap()
            }),
        ),
    ];

    for (name, file, open) in cases {
        let path = temp_path(file);
        ImageWriter::save(&path, &big_meta, &[vec![0x12, 0x34]]).unwrap();
        assert_eq!(open(&path), vec![0x34, 0x12], "{name}");
    }
}

#[test]
fn direct_non_tiff_stack_writers_reject_wrong_plane_size() {
    for (name, ext, mut writer) in direct_stack_writer_cases() {
        let meta = stack_writer_meta();
        let path = temp_path(&format!("direct_wrong_size_{name}.{ext}"));
        writer.set_metadata(&meta).unwrap();
        writer.set_id(&path).unwrap();

        let err = writer.save_bytes(0, &[0; 15]).unwrap_err();

        assert!(
            err.to_string()
                .contains(&format!("{name} writer: plane 0 has 15 bytes, expected 16")),
            "{name}: unexpected error: {err}"
        );
    }
}

#[test]
fn image_writer_save_rejects_zero_sized_images_before_creating_file() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 0;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let path = temp_path("zero_sized_metaimage.mha");
    let err = ImageWriter::save(&path, &meta, &[Vec::new()]).unwrap_err();

    assert!(
        err.to_string()
            .contains("writer image dimensions must be positive"),
        "unexpected error: {err}"
    );
    assert!(!path.exists(), "writer created output before validation");
}

#[test]
fn direct_non_tiff_stack_writers_reject_zero_sized_images() {
    for (name, ext, mut writer) in direct_stack_writer_cases() {
        let mut meta = stack_writer_meta();
        meta.size_x = 0;
        let path = temp_path(&format!("direct_zero_size_{name}.{ext}"));
        writer.set_metadata(&meta).unwrap();
        writer.set_id(&path).unwrap();

        let err = writer.save_bytes(0, &[]).unwrap_err();

        assert!(
            err.to_string()
                .contains(&format!("{name} writer: image dimensions must be positive")),
            "{name}: unexpected error: {err}"
        );
    }
}

#[test]
fn direct_non_tiff_stack_writers_reject_duplicate_and_out_of_order_planes() {
    for (name, ext, mut writer) in direct_stack_writer_cases() {
        let meta = stack_writer_meta();
        let path = temp_path(&format!("direct_duplicate_{name}.{ext}"));
        writer.set_metadata(&meta).unwrap();
        writer.set_id(&path).unwrap();
        writer.save_bytes(0, &[0; 16]).unwrap();

        let err = writer.save_bytes(0, &[1; 16]).unwrap_err();

        assert!(
            err.to_string().contains(&format!(
                "{name} writer: planes must be written in order; expected 1, got 0"
            )),
            "{name}: unexpected error: {err}"
        );
    }

    for (name, ext, mut writer) in direct_stack_writer_cases() {
        let meta = stack_writer_meta();
        let path = temp_path(&format!("direct_out_of_order_{name}.{ext}"));
        writer.set_metadata(&meta).unwrap();
        writer.set_id(&path).unwrap();

        let err = writer.save_bytes(1, &[1; 16]).unwrap_err();

        assert!(
            err.to_string().contains(&format!(
                "{name} writer: planes must be written in order; expected 0, got 1"
            )),
            "{name}: unexpected error: {err}"
        );
    }
}

#[test]
fn direct_non_tiff_stack_writers_reject_out_of_range_plane() {
    for (name, ext, mut writer) in direct_stack_writer_cases() {
        let meta = stack_writer_meta();
        let path = temp_path(&format!("direct_out_of_range_{name}.{ext}"));
        writer.set_metadata(&meta).unwrap();
        writer.set_id(&path).unwrap();
        writer.save_bytes(0, &[0; 16]).unwrap();
        writer.save_bytes(1, &[1; 16]).unwrap();

        let err = writer.save_bytes(2, &[2; 16]).unwrap_err();

        assert!(
            err.to_string().contains("Plane index 2 out of range"),
            "{name}: unexpected error: {err}"
        );
    }
}

#[test]
fn direct_non_tiff_stack_writers_reject_missing_planes_on_close() {
    for (name, ext, mut writer) in direct_stack_writer_cases() {
        let meta = stack_writer_meta();
        let path = temp_path(&format!("direct_missing_{name}.{ext}"));
        writer.set_metadata(&meta).unwrap();
        writer.set_id(&path).unwrap();
        writer.save_bytes(0, &[0; 16]).unwrap();

        let err = writer.close().unwrap_err();

        assert!(
            err.to_string()
                .contains(&format!("{name} writer: wrote 1 planes, expected 2")),
            "{name}: unexpected error: {err}"
        );
    }
}

#[test]
fn direct_stateful_stack_writers_allow_retry_after_incomplete_close() {
    let cases: Vec<(
        &'static str,
        &'static str,
        Box<dyn bioformats::FormatWriter>,
    )> = vec![
        (
            "ICS",
            "ics",
            Box::new(bioformats::formats::ics::IcsWriter::new()),
        ),
        (
            "MRC",
            "mrc",
            Box::new(bioformats::formats::mrc::MrcWriter::new()),
        ),
        (
            "FITS",
            "fits",
            Box::new(bioformats::formats::fits::FitsWriter::new()),
        ),
        (
            "NRRD",
            "nrrd",
            Box::new(bioformats::formats::nrrd::NrrdWriter::new()),
        ),
        (
            "MetaImage",
            "mha",
            Box::new(bioformats::formats::metaimage::MetaImageWriter::new()),
        ),
        (
            "DICOM",
            "dcm",
            Box::new(bioformats::formats::dicom::DicomWriter::new()),
        ),
        (
            "OME-XML",
            "ome",
            Box::new(bioformats::formats::ome_xml::OmeXmlWriter::new()),
        ),
        (
            "AVI",
            "avi",
            Box::new(bioformats::formats::avi::AviWriter::new()),
        ),
    ];

    for (name, ext, mut writer) in cases {
        let meta = stack_writer_meta();
        let path = temp_path(&format!("direct_retry_missing_{name}.{ext}"));
        writer.set_metadata(&meta).unwrap();
        writer.set_id(&path).unwrap();
        writer.save_bytes(0, &[0; 16]).unwrap();

        let err = writer.close().unwrap_err();
        assert!(
            err.to_string()
                .contains(&format!("{name} writer: wrote 1 planes, expected 2")),
            "{name}: unexpected error: {err}"
        );

        writer.save_bytes(1, &[1; 16]).unwrap();
        writer.close().unwrap();
    }
}

#[test]
fn mrc_writer_rejects_non_rgb_channels_instead_of_flattening_to_z() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 2;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 1;
    meta.size_c = 2;
    meta.size_t = 1;
    meta.image_count = 2;
    meta.is_rgb = false;

    let path = temp_path("mrc_non_rgb_channels.mrc");
    let mut writer = bioformats::formats::mrc::MrcWriter::new();
    let err = writer.set_metadata(&meta).unwrap_err();
    assert!(
        err.to_string().contains("not non-RGB C/T axes"),
        "unexpected error: {err}"
    );

    let err = ImageWriter::save(&path, &meta, &[vec![1, 2, 3, 4], vec![5, 6, 7, 8]]).unwrap_err();
    assert!(
        err.to_string().contains("not non-RGB C/T axes"),
        "unexpected ImageWriter error: {err}"
    );
}

#[test]
fn direct_single_plane_writers_reject_malformed_planes() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 1;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint16;
    meta.image_count = 1;
    meta.size_c = 1;

    let mut png = bioformats::formats::png::PngWriter::new();
    png.set_metadata(&meta).unwrap();
    png.set_id(&temp_path("direct_odd_uint16.png")).unwrap();
    let err = png.save_bytes(0, &[1, 0, 2]).unwrap_err();
    assert!(
        err.to_string()
            .contains("PNG writer: plane 0 has 3 bytes, expected 2"),
        "unexpected error: {err}"
    );

    let mut eps_meta = ImageMetadata::default();
    eps_meta.size_x = 1;
    eps_meta.size_y = 1;
    eps_meta.pixel_type = PixelType::Uint8;
    eps_meta.image_count = 1;
    eps_meta.size_c = 1;

    let mut eps = bioformats::formats::eps::EpsWriter::new();
    eps.set_metadata(&eps_meta).unwrap();
    eps.set_id(&temp_path("direct_duplicate.eps")).unwrap();
    eps.save_bytes(0, &[1]).unwrap();
    let err = eps.save_bytes(0, &[2]).unwrap_err();
    assert!(
        err.to_string().contains("supports only one plane"),
        "unexpected error: {err}"
    );

    let mut tga = bioformats::formats::raster::TgaWriter::new();
    tga.set_metadata(&eps_meta).unwrap();
    tga.set_id(&temp_path("direct_bad_len.tga")).unwrap();
    let err = tga.save_bytes(0, &[1, 2]).unwrap_err();
    assert!(
        err.to_string()
            .contains("writer plane has 2 bytes, expected 1"),
        "unexpected error: {err}"
    );

    let mut png_missing = bioformats::formats::png::PngWriter::new();
    png_missing.set_metadata(&eps_meta).unwrap();
    png_missing
        .set_id(&temp_path("direct_missing.png"))
        .unwrap();
    let err = png_missing.close().unwrap_err();
    assert!(
        err.to_string()
            .contains("PNG writer closed before plane 0 was written"),
        "unexpected error: {err}"
    );

    let mut png_duplicate = bioformats::formats::png::PngWriter::new();
    png_duplicate.set_metadata(&eps_meta).unwrap();
    png_duplicate
        .set_id(&temp_path("direct_duplicate.png"))
        .unwrap();
    png_duplicate.save_bytes(0, &[1]).unwrap();
    let err = png_duplicate.save_bytes(0, &[2]).unwrap_err();
    assert!(
        err.to_string().contains("PNG writer already wrote plane 0"),
        "unexpected error: {err}"
    );

    let mut stack_meta = eps_meta.clone();
    stack_meta.size_z = 2;
    stack_meta.image_count = 2;
    let mut jpeg = bioformats::formats::jpeg::JpegWriter::new();
    let err = jpeg.set_metadata(&stack_meta).unwrap_err();
    assert!(
        err.to_string()
            .contains("JPEG writer supports only one plane"),
        "unexpected error: {err}"
    );
}

#[test]
fn direct_tga_and_eps_writers_reject_stack_metadata() {
    let mut stack_meta = ImageMetadata::default();
    stack_meta.size_x = 1;
    stack_meta.size_y = 1;
    stack_meta.pixel_type = PixelType::Uint8;
    stack_meta.size_z = 2;
    stack_meta.size_c = 1;
    stack_meta.size_t = 1;
    stack_meta.image_count = 2;

    let mut tga = bioformats::formats::raster::TgaWriter::new();
    let err = tga.set_metadata(&stack_meta).unwrap_err();
    assert!(
        err.to_string()
            .contains("TGA writer supports only one plane"),
        "unexpected TGA error: {err}"
    );

    let mut eps = bioformats::formats::eps::EpsWriter::new();
    let err = eps.set_metadata(&stack_meta).unwrap_err();
    assert!(
        err.to_string()
            .contains("EPS writer supports only one plane"),
        "unexpected EPS error: {err}"
    );
}

#[test]
fn direct_png_writer_preserves_big_endian_uint16_samples() {
    let path = temp_path("direct_big_endian_u16.png");
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint16;
    meta.size_c = 1;
    meta.image_count = 1;
    meta.is_little_endian = false;

    let data = [0x12, 0x34, 0xab, 0xcd];
    let mut writer = bioformats::formats::png::PngWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &data).unwrap();
    writer.close().unwrap();

    let mut reader = bioformats::formats::png::PngReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), data);
}

#[test]
fn direct_tga_and_pnm_writers_interleave_planar_rgb_input() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 3;
    meta.image_count = 1;
    meta.is_rgb = true;
    meta.is_interleaved = false;

    let planar = [1, 2, 10, 20, 100, 200];
    let interleaved = [1, 10, 100, 2, 20, 200];

    let tga_path = temp_path("direct_planar_rgb.tga");
    let mut tga = bioformats::formats::raster::TgaWriter::new();
    tga.set_metadata(&meta).unwrap();
    tga.set_id(&tga_path).unwrap();
    tga.save_bytes(0, &planar).unwrap();
    tga.close().unwrap();
    let mut tga_reader = ImageReader::open(&tga_path).unwrap();
    assert_eq!(tga_reader.open_bytes(0).unwrap(), interleaved);

    let pnm_path = temp_path("direct_planar_rgb.ppm");
    let mut pnm = bioformats::formats::raster::PnmWriter::new();
    pnm.set_metadata(&meta).unwrap();
    pnm.set_id(&pnm_path).unwrap();
    pnm.save_bytes(0, &planar).unwrap();
    pnm.close().unwrap();
    let mut pnm_reader = ImageReader::open(&pnm_path).unwrap();
    assert_eq!(pnm_reader.open_bytes(0).unwrap(), interleaved);
}

#[test]
fn avi_writer_rejects_metadata_it_cannot_encode() {
    let mut uint16_meta = ImageMetadata::default();
    uint16_meta.size_x = 1;
    uint16_meta.size_y = 1;
    uint16_meta.pixel_type = PixelType::Uint16;
    uint16_meta.size_c = 1;
    uint16_meta.image_count = 1;
    let mut writer = bioformats::formats::avi::AviWriter::new();
    let err = writer.set_metadata(&uint16_meta).unwrap_err();
    assert!(
        err.to_string().contains("only 8-bit pixel data"),
        "unexpected Uint16 error: {err}"
    );

    let mut channel_meta = ImageMetadata::default();
    channel_meta.size_x = 1;
    channel_meta.size_y = 1;
    channel_meta.pixel_type = PixelType::Uint8;
    channel_meta.size_c = 2;
    channel_meta.image_count = 2;
    let mut writer = bioformats::formats::avi::AviWriter::new();
    let err = writer.set_metadata(&channel_meta).unwrap_err();
    assert!(
        err.to_string().contains("got 2 non-RGB channels"),
        "unexpected channel error: {err}"
    );

    let mut rgba_meta = ImageMetadata::default();
    rgba_meta.size_x = 1;
    rgba_meta.size_y = 1;
    rgba_meta.pixel_type = PixelType::Uint8;
    rgba_meta.size_c = 4;
    rgba_meta.image_count = 1;
    rgba_meta.is_rgb = true;
    rgba_meta.is_interleaved = true;
    let mut writer = bioformats::formats::avi::AviWriter::new();
    let err = writer.set_metadata(&rgba_meta).unwrap_err();
    assert!(
        err.to_string().contains("RGB Uint8 data with 3 channels"),
        "unexpected RGBA error: {err}"
    );
}

#[test]
fn tiff_writer_claims_java_bigtiff_suffixes() {
    use bioformats::TiffWriter;

    let writer = TiffWriter::new();
    assert!(writer.is_this_type(&temp_path("classic_ok.btf")));
    assert!(writer.is_this_type(&temp_path("classic_ok.tf2")));
    assert!(writer.is_this_type(&temp_path("classic_ok.tf8")));
    assert!(writer.is_this_type(&temp_path("classic_ok.tif")));
    assert!(writer.is_this_type(&temp_path("classic_ok.tiff")));
}

#[test]
fn tiff_writer_accepts_planar_rgb_like_java() {
    use bioformats::tiff::ifd::tag;
    use bioformats::tiff::parser::TiffParser;
    use bioformats::TiffWriter;
    use std::fs::File;
    use std::io::BufReader;

    let mut planar_rgb = ImageMetadata::default();
    planar_rgb.size_x = 2;
    planar_rgb.size_y = 1;
    planar_rgb.pixel_type = PixelType::Uint8;
    planar_rgb.size_c = 3;
    planar_rgb.image_count = 1;
    planar_rgb.is_rgb = true;
    planar_rgb.is_interleaved = false;

    let data = vec![10, 40, 20, 50, 30, 60];
    let path = temp_path("planar_rgb.tif");
    let mut writer = TiffWriter::new();
    writer.set_metadata(&planar_rgb).unwrap();
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &data).unwrap();
    writer.close().unwrap();

    let file = File::open(&path).unwrap();
    let mut parser = TiffParser::new(BufReader::new(file)).unwrap();
    let ifds = parser.read_ifds().unwrap();
    assert_eq!(ifds[0].get_u16(tag::PLANAR_CONFIGURATION), Some(2));
    assert_eq!(ifds[0].get_vec_u32(tag::STRIP_OFFSETS).len(), 3);
    assert_eq!(ifds[0].get_vec_u32(tag::STRIP_BYTE_COUNTS), vec![2, 2, 2]);

    let mut reader = ImageReader::open(&path).unwrap();
    assert!(reader.metadata().is_rgb);
    assert!(!reader.metadata().is_interleaved);
    assert_eq!(reader.open_bytes(0).unwrap(), data);
}

#[test]
fn tiff_writer_rejects_bit_metadata() {
    use bioformats::TiffWriter;

    let mut bit_meta = ImageMetadata::default();
    bit_meta.size_x = 8;
    bit_meta.size_y = 1;
    bit_meta.pixel_type = PixelType::Bit;
    bit_meta.size_c = 1;
    bit_meta.image_count = 1;

    let mut writer = TiffWriter::new();
    let err = writer.set_metadata(&bit_meta).unwrap_err();
    assert!(
        err.to_string().contains("does not support PixelType::Bit"),
        "unexpected bit pixel error: {err}"
    );
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
    assert_eq!(rational(ifd.get(tag::X_RESOLUTION).unwrap()), 0.0);
    assert_eq!(rational(ifd.get(tag::Y_RESOLUTION).unwrap()), 0.0);
    assert_eq!(ifd.get_u16(tag::RESOLUTION_UNIT), Some(3));
}

#[test]
fn tiff_writer_writes_java_imagej_description_and_physical_resolution() {
    use bioformats::tiff::ifd::tag;
    use bioformats::tiff::parser::TiffParser;
    use bioformats::TiffWriter;
    use std::fs::File;
    use std::io::BufReader;

    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 2;
    meta.size_z = 2;
    meta.size_c = 3;
    meta.size_t = 2;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 12;
    meta.series_metadata
        .insert("PhysicalSizeX".into(), MetadataValue::Float(0.5));
    meta.series_metadata
        .insert("PhysicalSizeY".into(), MetadataValue::Float(0.25));

    let path = temp_path("java_imagej_description_resolution.tif");
    let mut writer = TiffWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_id(&path).unwrap();
    for plane in 0..12 {
        writer.save_bytes(plane, &[plane as u8; 4]).unwrap();
    }
    writer.close().unwrap();

    let file = File::open(&path).unwrap();
    let mut parser = TiffParser::new(BufReader::new(file)).unwrap();
    let ifds = parser.read_ifds().unwrap();
    assert_eq!(ifds.len(), 12);
    let first = &ifds[0];
    let description = first.get_str(tag::IMAGE_DESCRIPTION).unwrap();
    assert!(description.starts_with("ImageJ=\nhyperstack=true\n"));
    assert!(description.contains("images=12"));
    assert!(description.contains("channels=3"));
    assert!(description.contains("slices=2"));
    assert!(description.contains("frames=2"));
    assert_eq!(first.get_vec_f64(tag::X_RESOLUTION), vec![20000.0]);
    assert_eq!(first.get_vec_f64(tag::Y_RESOLUTION), vec![40000.0]);
    assert_eq!(first.get_u16(tag::RESOLUTION_UNIT), Some(3));
    assert_eq!(
        ifds[1].get_str(tag::IMAGE_DESCRIPTION).unwrap(),
        description
    );
}

#[test]
fn pyramid_ome_tiff_direct_writer_auto_embeds_ome_xml() {
    use bioformats::tiff::ifd::tag;
    use bioformats::tiff::parser::TiffParser;
    use bioformats::tiff::PyramidOmeTiffWriter;
    use std::fs::File;
    use std::io::BufReader;

    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let path = temp_path("direct_auto_ome_pyramid.ome.tif");
    let mut writer = PyramidOmeTiffWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &[1; 16]).unwrap();
    writer.add_resolution_level(vec![vec![2; 4]]);
    writer.close().unwrap();

    let file = File::open(&path).unwrap();
    let mut parser = TiffParser::new(BufReader::new(file)).unwrap();
    let ifds = parser.read_ifds().unwrap();
    let description = ifds[0].get_str(tag::IMAGE_DESCRIPTION).unwrap();
    assert!(description.contains("<OME"));
    assert!(description.contains("<Pixels"));
    assert_eq!(ifds[0].get_u16(tag::RESOLUTION_UNIT), Some(3));
}

#[test]
fn ome_tiff_save_rejects_wrong_plane_size_before_creating_file() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 2;
    meta.size_z = 1;
    meta.size_c = 1;
    meta.size_t = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let path = temp_path("wrong_ome_tiff_plane_size.ome.tif");
    let ome = bioformats::OmeMetadata::from_image_metadata(&meta);
    let err = ImageWriter::save_ome_tiff(&path, &meta, &ome, &[vec![1, 2, 3]]).unwrap_err();

    assert!(
        err.to_string().contains("expected 4"),
        "unexpected error: {err}"
    );
    assert!(
        !path.exists(),
        "wrong plane size should be rejected before creating output"
    );
}

#[test]
fn direct_tiff_set_ome_metadata_populates_required_channels() {
    use bioformats::tiff::TiffWriter;
    use bioformats::OmeMetadata;

    let mut meta = ImageMetadata::default();
    meta.size_x = 1;
    meta.size_y = 1;
    meta.size_z = 1;
    meta.size_c = 2;
    meta.size_t = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 2;

    let path = temp_path("direct_empty_store.ome.tif");
    let mut writer = TiffWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_ome_metadata(&OmeMetadata::default()).unwrap();
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &[1]).unwrap();
    writer.save_bytes(1, &[2]).unwrap();
    writer.close().unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(text.contains(r#"<Image ID="Image:0""#));
    assert!(text.contains(r#"<Channel ID="Channel:0:0" SamplesPerPixel="1""#));
    assert!(text.contains(r#"<Channel ID="Channel:0:1" SamplesPerPixel="1""#));
}

#[test]
fn direct_ome_xml_writer_populates_required_channels_from_empty_store() {
    use bioformats::formats::ome_xml::OmeXmlWriter;
    use bioformats::OmeMetadata;

    let mut meta = ImageMetadata::default();
    meta.size_x = 1;
    meta.size_y = 1;
    meta.size_z = 1;
    meta.size_c = 2;
    meta.size_t = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 2;

    let path = temp_path("direct_empty_store.ome.xml");
    let mut writer = OmeXmlWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_ome_metadata(OmeMetadata::default());
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &[1]).unwrap();
    writer.save_bytes(1, &[2]).unwrap();
    writer.close().unwrap();

    let xml = std::fs::read_to_string(&path).unwrap();
    assert!(xml.contains(r#"<Image ID="Image:0""#));
    assert!(xml.contains(r#"<Channel ID="Channel:0:0" SamplesPerPixel="1""#));
    assert!(xml.contains(r#"<Channel ID="Channel:0:1" SamplesPerPixel="1""#));
}

#[test]
fn generic_ome_tiff_suffix_writes_embedded_ome_xml_like_java_writer_selection() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 1;
    meta.size_y = 1;
    meta.size_z = 1;
    meta.size_c = 2;
    meta.size_t = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 2;

    let path = temp_path("generic_writer_selection.ome.tif");
    ImageWriter::save(&path, &meta, &[vec![7], vec![9]]).unwrap();

    let bytes = std::fs::read(&path).unwrap();
    let text = String::from_utf8_lossy(&bytes);
    assert!(
        text.contains(r#"<OME "#),
        "OME-XML missing from TIFF comment"
    );
    assert!(text.contains(r#"SizeC="2""#));
}

#[test]
fn jpeg_writer_accepts_jpe_suffix_like_java() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 1;
    meta.size_c = 3;
    meta.is_rgb = true;
    meta.is_interleaved = true;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 8;
    meta.image_count = 1;

    let path = temp_path("suffix_java.jpe");
    ImageWriter::save(&path, &meta, &[vec![255, 0, 0, 0, 255, 0]]).unwrap();
    assert!(path.exists());
}

#[test]
fn ome_xml_writer_splits_rgb_bindata_per_channel_like_java() {
    use bioformats::formats::ome_xml::{OmeXmlReader, OmeXmlWriter};

    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 1;
    meta.size_c = 3;
    meta.is_rgb = true;
    meta.is_interleaved = true;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 8;
    meta.image_count = 1;

    let path = temp_path("rgb_split.ome.xml");
    let mut writer = OmeXmlWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &[1, 2, 3, 4, 5, 6]).unwrap();
    writer.close().unwrap();

    let xml = std::fs::read_to_string(&path).unwrap();
    assert_eq!(xml.matches("<BinData ").count(), 3);
    assert_eq!(xml.matches(r#"Length="2""#).count(), 3);
    assert!(xml.contains(">AQQ=</BinData>"));
    assert!(xml.contains(">AgU=</BinData>"));
    assert!(xml.contains(">AwY=</BinData>"));

    let mut reader = OmeXmlReader::new();
    reader.set_id(&path).unwrap();
    assert!(reader.metadata().is_rgb);
    assert!(reader.metadata().is_interleaved);
    assert_eq!(reader.metadata().image_count, 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
}

#[test]
fn planar_rgb_writer_inputs_are_interleaved_like_java() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 1;
    meta.size_c = 3;
    meta.is_rgb = true;
    meta.is_interleaved = false;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 8;
    meta.image_count = 1;

    // R plane [10, 40], G plane [20, 50], B plane [30, 60].
    let planar = vec![10, 40, 20, 50, 30, 60];

    let png_path = temp_path("planar_rgb.png");
    ImageWriter::save(&png_path, &meta, &[planar.clone()]).unwrap();
    let mut png_reader = ImageReader::open(&png_path).unwrap();
    assert_eq!(
        png_reader.open_bytes(0).unwrap(),
        vec![10, 20, 30, 40, 50, 60]
    );

    let bmp_path = temp_path("planar_rgb.bmp");
    ImageWriter::save(&bmp_path, &meta, &[planar.clone()]).unwrap();
    let mut bmp_reader = ImageReader::open(&bmp_path).unwrap();
    assert_eq!(
        bmp_reader.open_bytes(0).unwrap(),
        vec![10, 20, 30, 40, 50, 60]
    );

    let eps_path = temp_path("planar_rgb.eps");
    ImageWriter::save(&eps_path, &meta, &[planar.clone()]).unwrap();
    let eps = std::fs::read_to_string(&eps_path).unwrap();
    assert!(eps.contains("0A141E28323C"));

    let avi_path = temp_path("planar_rgb.avi");
    ImageWriter::save(&avi_path, &meta, &[planar.clone()]).unwrap();
    let mut avi_reader = ImageReader::open(&avi_path).unwrap();
    assert_eq!(
        avi_reader.open_bytes(0).unwrap(),
        vec![10, 20, 30, 40, 50, 60]
    );

    let mov_path = temp_path("planar_rgb.mov");
    ImageWriter::save(&mov_path, &meta, &[planar]).unwrap();
    let mut mov_reader = ImageReader::open(&mov_path).unwrap();
    assert_eq!(
        mov_reader.open_bytes(0).unwrap(),
        vec![10, 20, 30, 40, 50, 60]
    );
}

#[test]
fn png_round_trip() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 8;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 3;
    meta.is_rgb = true;
    meta.is_interleaved = true;
    meta.image_count = 1;

    let data: Vec<u8> = (0u8..192).collect(); // 8×8×3
    let readback = round_trip("test.png", &meta, &data);
    assert_eq!(readback, data);
}

#[test]
fn pnm_round_trip_gray8() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 1;
    meta.image_count = 1;

    let data: Vec<u8> = (0u8..16).collect();
    let readback = round_trip("test.pgm", &meta, &data);
    assert_eq!(readback, data);
}

#[test]
fn pnm_writer_emits_raw_p5_p6_readable_by_pnm_reader() {
    let mut gray_meta = ImageMetadata::default();
    gray_meta.size_x = 2;
    gray_meta.size_y = 1;
    gray_meta.pixel_type = PixelType::Uint16;
    gray_meta.size_c = 1;
    gray_meta.image_count = 1;

    let gray = [0x34, 0x12, 0xff, 0xff];
    let gray_path = temp_path("raw_p5_uint16.pgm");
    ImageWriter::save(&gray_path, &gray_meta, &[gray.to_vec()]).unwrap();
    let gray_file = std::fs::read(&gray_path).unwrap();
    assert!(gray_file.starts_with(b"P5\n2 1\n65535\n"));
    let mut gray_reader = bioformats::formats::raster::pnm_reader();
    gray_reader.set_id(&gray_path).unwrap();
    assert_eq!(gray_reader.metadata().pixel_type, PixelType::Uint16);
    assert_eq!(gray_reader.open_bytes(0).unwrap(), gray);

    let mut rgb_meta = ImageMetadata::default();
    rgb_meta.size_x = 2;
    rgb_meta.size_y = 1;
    rgb_meta.pixel_type = PixelType::Uint8;
    rgb_meta.size_c = 3;
    rgb_meta.is_rgb = true;
    rgb_meta.is_interleaved = true;
    rgb_meta.image_count = 1;

    let rgb = [1, 2, 3, 4, 5, 6];
    let rgb_path = temp_path("raw_p6_rgb.ppm");
    ImageWriter::save(&rgb_path, &rgb_meta, &[rgb.to_vec()]).unwrap();
    let rgb_file = std::fs::read(&rgb_path).unwrap();
    assert!(rgb_file.starts_with(b"P6\n2 1\n255\n"));
    let mut rgb_reader = bioformats::formats::raster::pnm_reader();
    rgb_reader.set_id(&rgb_path).unwrap();
    assert_eq!(rgb_reader.metadata().size_c, 3);
    assert!(rgb_reader.metadata().is_rgb);
    assert_eq!(rgb_reader.open_bytes(0).unwrap(), rgb);
}
