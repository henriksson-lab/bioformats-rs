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

#[cfg(feature = "gpl")]
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
#[cfg(feature = "gpl")]
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

    let mut reader = bioformats::formats::gpl::norpix::NorpixReader::new();
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
#[cfg(feature = "gpl")]
fn pco_b16_reads_declared_dimensions_and_pixels() {
    let path = tmp("frame.b16");
    let mut data = vec![0u8; 216];
    data[4..6].copy_from_slice(&2u16.to_le_bytes());
    data[6..8].copy_from_slice(&2u16.to_le_bytes());
    for pixel in [1u16, 2, 3, 4] {
        data.extend_from_slice(&pixel.to_le_bytes());
    }
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::gpl::camera2::PcoB16Reader::new();
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

    // Also verify default ImageReader::open registration for .b16
    let mut auto = ImageReader::open(&path).unwrap();
    assert_eq!(auto.metadata().size_x, 2);
    assert_eq!(auto.metadata().size_y, 2);
    assert_eq!(auto.open_bytes(0).unwrap().len(), 8);
}

#[test]
fn metaimage_mha_local_non_utf8_and_zlib_compressed() {
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    let raw_path = tmp("local_non_utf8.mha");
    let mut raw_bytes = b"ObjectType = Image\nNDims = 2\nDimSize = 2 2\nElementType = MET_UCHAR\nElementDataFile = LOCAL\n".to_vec();
    raw_bytes.extend_from_slice(&[0xff, 0xfe, 0x80, 0x81]);
    std::fs::write(&raw_path, raw_bytes).unwrap();

    let mut reader = ImageReader::open(&raw_path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![0xff, 0xfe, 0x80, 0x81]);

    let z_path = tmp("local_zlib.mha");
    let mut voxels = Vec::new();
    for v in [0x1234u16, 0xabcd, 0xffff, 0x8001, 0x0042, 0x7fff, 0x9000, 0x0102] {
        voxels.extend_from_slice(&v.to_le_bytes());
    }
    let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&voxels).unwrap();
    let compressed = enc.finish().unwrap();

    let mut z_bytes = b"ObjectType = Image\nNDims = 3\nDimSize = 2 2 2\nElementType = MET_USHORT\nCompressedData = True\nElementDataFile = LOCAL\n".to_vec();
    z_bytes.extend_from_slice(&compressed);
    std::fs::write(&z_path, z_bytes).unwrap();

    let mut z_reader = ImageReader::open(&z_path).unwrap();
    assert_eq!(z_reader.metadata().size_z, 2);
    assert_eq!(z_reader.open_bytes(0).unwrap(), voxels[..8]);
    assert_eq!(z_reader.open_bytes(1).unwrap(), voxels[8..]);
}

#[test]
fn simfcs_supports_b64_header_and_zlib_r64_i64() {
    use flate2::write::ZlibEncoder;
    use std::io::Write;

    // .b64 with 4-byte LE dimension header (4x4 Int16, 2 frames)
    let b64_path = tmp("headered.b64");
    let mut b64 = Vec::new();
    b64.extend_from_slice(&4i32.to_le_bytes());
    for i in 0..(4 * 4 * 2) {
        b64.extend_from_slice(&(i as i16 - 5).to_le_bytes());
    }
    std::fs::write(&b64_path, &b64).unwrap();
    let mut r = ImageReader::open(&b64_path).unwrap();
    assert_eq!(r.metadata().size_x, 4);
    assert_eq!(r.metadata().size_y, 4);
    assert_eq!(r.metadata().image_count, 2);
    assert_eq!(r.metadata().pixel_type, PixelType::Int16);
    assert_eq!(r.open_bytes(0).unwrap(), &b64[4..4 + 32]);
    assert_eq!(r.open_bytes(1).unwrap(), &b64[4 + 32..]);

    // .r64 zlib-compressed with 4-byte LE header (2x2 Float32, 1 frame)
    let r64_path = tmp("compressed.r64");
    let mut r64_raw = Vec::new();
    r64_raw.extend_from_slice(&2i32.to_le_bytes());
    for v in [1.5f32, -2.5, 3.25, 4.0] {
        r64_raw.extend_from_slice(&v.to_le_bytes());
    }
    let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&r64_raw).unwrap();
    std::fs::write(&r64_path, enc.finish().unwrap()).unwrap();
    let mut r64_reader = ImageReader::open(&r64_path).unwrap();
    assert_eq!(r64_reader.metadata().size_x, 2);
    assert_eq!(r64_reader.metadata().pixel_type, PixelType::Float32);
    assert_eq!(r64_reader.open_bytes(0).unwrap(), &r64_raw[4..]);

    // .i64 zlib-compressed headerless (256x256 Int32, 1 frame)
    let i64_path = tmp("compressed.i64");
    let mut i64_raw = vec![0u8; 256 * 256 * 4];
    i64_raw[0..4].copy_from_slice(&(-42i32).to_le_bytes());
    let mut enc = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    enc.write_all(&i64_raw).unwrap();
    std::fs::write(&i64_path, enc.finish().unwrap()).unwrap();
    let mut i64_reader = ImageReader::open(&i64_path).unwrap();
    assert_eq!(i64_reader.metadata().size_x, 256);
    assert_eq!(i64_reader.metadata().pixel_type, PixelType::Int32);
    assert_eq!(&i64_reader.open_bytes(0).unwrap()[0..4], &(-42i32).to_le_bytes());
}

#[test]
#[cfg(feature = "gpl")]
fn registry_opens_norpix_seq_and_leica_lof_and_vff_ignores_binary_siblings() {
    // 1. Norpix .seq via ImageReader::open
    let seq_path = tmp("auto_open.seq");
    let mut seq = norpix_seq_header(1, 2, 2, 0, 4);
    seq.extend_from_slice(&[10, 20, 30, 40]);
    std::fs::write(&seq_path, seq).unwrap();
    let mut seq_reader = ImageReader::open(&seq_path).unwrap();
    assert_eq!(seq_reader.open_bytes(0).unwrap(), vec![10, 20, 30, 40]);

    // 2. Leica .lof must not fall into MrcReader (whose heuristic accepts small nx/ny/nz)
    let lof_path = tmp("auto_open.lof");
    let mut lof = Vec::new();
    lof.extend_from_slice(&0x70i32.to_le_bytes());
    lof.extend_from_slice(&128i32.to_le_bytes()); // chunk len < 65536 (looks like MRC ny)
    lof.push(0x2a); // LOF memory byte -> combined with nc=15 gives nz = 3882 < 65536!
    let type_name = "LMS_Object_File";
    lof.extend_from_slice(&(type_name.len() as i32).to_le_bytes());
    for u in type_name.encode_utf16() {
        lof.extend_from_slice(&u.to_le_bytes());
    }
    std::fs::write(&lof_path, &lof).unwrap();
    // Must fail as LOF (not succeed as bogus MRC!)
    let err = match ImageReader::open(&lof_path) {
        Ok(_) => panic!("truncated LOF should not open as MRC"),
        Err(e) => e,
    };
    let msg = err.to_string();
    assert!(
        msg.contains("LOF") || msg.contains("Leica"),
        "expected LOF error rather than MRC hijack, got: {msg}"
    );

    // 3. MicroCtVffReader ignores non-text/binary siblings in the same directory
    let vff_dir = tmp("vff_mixed_dir");
    let _ = std::fs::remove_dir_all(&vff_dir);
    std::fs::create_dir_all(&vff_dir).unwrap();
    let vff_path = vff_dir.join("scan.vff");
    let mut vff_bytes = b"ncaa\nrank=2;\nsize=2 2;\nbits=8;\n\x0c\n".to_vec();
    vff_bytes.extend_from_slice(&[1, 2, 3, 4]);
    std::fs::write(&vff_path, vff_bytes).unwrap();
    // Place binary/NUL-containing sibling file in the same directory
    std::fs::write(vff_dir.join("unrelated.bin"), &[0x00, 0xff, 0xfe, 0x89, 0x50]).unwrap();
    let mut vff_reader = ImageReader::open(&vff_path).unwrap();
    assert_eq!(vff_reader.open_bytes(0).unwrap(), vec![3, 4, 1, 2]);
    let _ = std::fs::remove_dir_all(&vff_dir);
}
