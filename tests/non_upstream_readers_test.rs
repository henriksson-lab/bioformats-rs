use bioformats::{
    BioFormatsError, FormatReader, ImageMetadata, ImageReader, ImageWriter, MetadataValue,
    PixelType,
};

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bioformats_non_upstream_{}_{}_{}",
        std::process::id(),
        nanos,
        name
    ))
}

fn round_trip(path: &std::path::Path, meta: &ImageMetadata, plane: Vec<u8>) -> Vec<u8> {
    ImageWriter::save(path, meta, &[plane]).expect("write failed");
    let mut reader = ImageReader::open(path).expect("read failed");
    reader.open_bytes(0).expect("open_bytes failed")
}

#[test]
fn metaimage_mha_and_mhd_round_trip() {
    let mut meta = ImageMetadata {
        size_x: 8,
        size_y: 8,
        pixel_type: PixelType::Uint8,
        image_count: 1,
        ..ImageMetadata::default()
    };
    meta.size_c = 1;

    let data: Vec<u8> = (0..64).collect();
    assert_eq!(round_trip(&tmp("roundtrip.mha"), &meta, data.clone()), data);
    assert_eq!(round_trip(&tmp("roundtrip.mhd"), &meta, data.clone()), data);
}

#[test]
fn metaimage_reads_interleaved_element_channels() {
    let path = tmp("rgb_channels.mha");
    let mut bytes = b"ObjectType = Image
NDims = 2
DimSize = 2 1
ElementType = MET_UCHAR
ElementNumberOfChannels = 3
ElementDataFile = LOCAL
"
    .to_vec();
    bytes.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_c, 3);
    assert!(reader.metadata().is_rgb);
    assert!(reader.metadata().is_interleaved);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 1, 1).unwrap(),
        vec![4, 5, 6]
    );
}

#[test]
fn simfcs_requires_whole_frames_and_crops_real_pixels() {
    let short = tmp("short_frame.b64");
    std::fs::write(&short, [1, 2, 3]).unwrap();
    let mut reader = bioformats::formats::simfcs::SimfcsReader::new();
    let err = reader.set_id(&short).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("whole number of 256x256 frames")),
        "{err:?}"
    );

    let path = tmp("one_frame.b64");
    let mut data: Vec<u8> = (0..=255).cycle().take(256 * 256).collect();
    data[257] = 99;
    std::fs::write(&path, &data).unwrap();

    let mut reader = bioformats::formats::simfcs::SimfcsReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert!(matches!(
        reader.metadata().series_metadata.get("simfcs.extension"),
        Some(MetadataValue::String(value)) if value == "b64"
    ));
    assert_eq!(reader.open_bytes_region(0, 1, 1, 1, 1).unwrap(), vec![99]);
}

fn norpix_seq_header(
    frames: u32,
    width: u32,
    height: u32,
    desc_fmt: u32,
    true_size: u32,
) -> Vec<u8> {
    let mut data = vec![0u8; 1024];
    data[..10].copy_from_slice(b"Norpix seq");
    data[548..552].copy_from_slice(&frames.to_le_bytes());
    data[572..576].copy_from_slice(&true_size.to_le_bytes());
    data[592..596].copy_from_slice(&desc_fmt.to_le_bytes());
    data[596..600].copy_from_slice(&width.to_le_bytes());
    data[600..604].copy_from_slice(&height.to_le_bytes());
    data
}

#[test]
fn norpix_seq_preserves_header_metadata_timestamps_and_pixels() {
    let path = tmp("metadata.seq");
    let mut data = norpix_seq_header(2, 2, 1, 0, 10);
    data[24..32].copy_from_slice(&3i64.to_le_bytes());
    data[32..36].copy_from_slice(&1024i32.to_le_bytes());
    data.extend_from_slice(&[1, 2]);
    data.extend_from_slice(&1000u32.to_le_bytes());
    data.extend_from_slice(&250u16.to_le_bytes());
    data.extend_from_slice(&500u16.to_le_bytes());
    data.extend_from_slice(&[3, 4]);
    data.extend_from_slice(&1002u32.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::norpix::NorpixReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_z, 2);
    assert!(matches!(
        meta.series_metadata.get("norpix.version"),
        Some(MetadataValue::Int(3))
    ));
    assert_eq!(reader.open_bytes(1).unwrap(), vec![3, 4]);

    let ome = reader.ome_metadata().expect("Norpix OME metadata");
    assert_eq!(ome.images[0].planes.len(), 2);
    assert_eq!(ome.images[0].planes[0].delta_t, Some(0.0));
    assert!((ome.images[0].planes[1].delta_t.unwrap() - 1.7495).abs() < 1.0e-12);
}

#[test]
fn pco_b16_reads_declared_dimensions_and_pixels() {
    let path = tmp("frame.b16");
    let mut data = vec![0u8; 216];
    data[4..6].copy_from_slice(&2u16.to_le_bytes());
    data[6..8].copy_from_slice(&2u16.to_le_bytes());
    for pixel in [1u16, 2, 3, 4] {
        data.extend_from_slice(&pixel.to_le_bytes());
    }
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::camera2::PcoB16Reader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        [1u16, 2, 3, 4]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    );
}
