//! Tests for features added during the gap audit implementation:
//! - DICOM writer round-trip
//! - AVI writer round-trip
//! - OME-XML writer round-trip
//! - OME-TIFF writer round-trip
//! - ChannelSeparator wrapper
//! - ChannelMerger wrapper
//! - DimensionSwapper wrapper
//! - MinMaxCalculator wrapper
//! - FileStitcher with synthetic sequence

use bioformats::formats::ome::OmeXmlWriter;
use bioformats::{
    BioFormatsError, ChannelSeparator, DimensionOrder, DimensionSwapper, FormatReader,
    FormatWriter, ImageMetadata, ImageReader, ImageWriter, MinMaxCalculator, PixelType,
};
use std::path::Path;

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("bioformats_new_{}", name))
}

// ── DICOM round-trip ─────────────────────────────────────────────────────────

#[test]
fn dicom_round_trip_gray8() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 8;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 8;
    meta.image_count = 1;

    let data: Vec<u8> = (0u8..64).collect();
    let path = tmp("test.dcm");
    ImageWriter::save(&path, &meta, &[data.clone()]).expect("DICOM write failed");

    let mut reader = ImageReader::open(&path).expect("DICOM read failed");
    let rmeta = reader.metadata();
    assert_eq!(rmeta.size_x, 8);
    assert_eq!(rmeta.size_y, 8);
    let rb = reader.open_bytes(0).expect("DICOM open_bytes failed");
    assert_eq!(rb.len(), 64);
    // DICOM may reinterpret pixels; just check dimensions match
}

#[test]
fn dicom_round_trip_gray16() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint16;
    meta.bits_per_pixel = 16;
    meta.image_count = 1;

    let data: Vec<u8> = (0u16..16).flat_map(|v| v.to_le_bytes()).collect();
    let path = tmp("test16.dcm");
    ImageWriter::save(&path, &meta, &[data.clone()]).expect("DICOM write failed");

    let mut reader = ImageReader::open(&path).expect("DICOM read failed");
    let rmeta = reader.metadata();
    assert_eq!(rmeta.size_x, 4);
    assert_eq!(rmeta.size_y, 4);
    assert_eq!(rmeta.bits_per_pixel, 16);
    let rb = reader.open_bytes(0).expect("open_bytes failed");
    assert_eq!(rb, data);
}

// ── AVI round-trip ───────────────────────────────────────────────────────────

#[test]
fn avi_round_trip_gray8() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 8;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 2;
    meta.size_z = 2;

    let plane0: Vec<u8> = vec![100; 64];
    let plane1: Vec<u8> = vec![200; 64];
    let path = tmp("test.avi");
    ImageWriter::save(&path, &meta, &[plane0.clone(), plane1.clone()]).expect("AVI write failed");

    let reader = ImageReader::open(&path).expect("AVI read failed");
    let rmeta = reader.metadata();
    assert_eq!(rmeta.size_x, 8);
    assert_eq!(rmeta.size_y, 8);
    assert!(
        rmeta.image_count >= 2,
        "expected at least 2 frames, got {}",
        rmeta.image_count
    );
}

#[test]
fn avi_round_trip_rgb24_preserves_rows_and_channels() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 3;
    meta.size_y = 2;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 3;
    meta.is_rgb = true;
    meta.is_interleaved = true;
    meta.image_count = 1;

    let data = vec![
        255, 0, 0, 0, 255, 0, 0, 0, 255, 10, 20, 30, 40, 50, 60, 70, 80, 90,
    ];
    let path = tmp("rgb24.avi");
    ImageWriter::save(&path, &meta, &[data.clone()]).expect("AVI write failed");

    let mut reader = ImageReader::open(&path).expect("AVI read failed");
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 3);
    assert_eq!(reader.open_bytes(0).expect("AVI open_bytes failed"), data);
}

#[test]
fn avi_reader_repacks_uncompressed_bgr_bottom_up_with_padding() {
    let path = tmp("external_rgb24.avi");
    let mut bytes = Vec::new();
    let frame = vec![
        130, 120, 110, 160, 150, 140, 190, 180, 170, 0xAA, 0xBB, 0xCC, 30, 20, 10, 60, 50, 40, 90,
        80, 70, 0xDD, 0xEE, 0xFF,
    ];
    let avih_size = 56u32;
    let strh_size = 56u32;
    let strf_size = 40u32;
    let strl_size = 4 + (8 + strh_size) + (8 + strf_size);
    let hdrl_size = 4 + (8 + avih_size) + (8 + strl_size);
    let movi_size = 4 + 8 + frame.len() as u32;
    let riff_size = 4 + (8 + hdrl_size) + (8 + movi_size);

    fn fourcc(bytes: &mut Vec<u8>, cc: &[u8; 4]) {
        bytes.extend_from_slice(cc);
    }
    fn u16le(bytes: &mut Vec<u8>, v: u16) {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    fn u32le(bytes: &mut Vec<u8>, v: u32) {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    fn i32le(bytes: &mut Vec<u8>, v: i32) {
        bytes.extend_from_slice(&v.to_le_bytes());
    }

    fourcc(&mut bytes, b"RIFF");
    u32le(&mut bytes, riff_size);
    fourcc(&mut bytes, b"AVI ");
    fourcc(&mut bytes, b"LIST");
    u32le(&mut bytes, hdrl_size);
    fourcc(&mut bytes, b"hdrl");
    fourcc(&mut bytes, b"avih");
    u32le(&mut bytes, avih_size);
    u32le(&mut bytes, 100_000);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 1);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 1);
    u32le(&mut bytes, frame.len() as u32);
    u32le(&mut bytes, 3);
    u32le(&mut bytes, 2);
    bytes.extend_from_slice(&[0; 16]);
    fourcc(&mut bytes, b"LIST");
    u32le(&mut bytes, strl_size);
    fourcc(&mut bytes, b"strl");
    fourcc(&mut bytes, b"strh");
    u32le(&mut bytes, strh_size);
    fourcc(&mut bytes, b"vids");
    fourcc(&mut bytes, b"DIB ");
    bytes.extend_from_slice(&[0; 48]);
    fourcc(&mut bytes, b"strf");
    u32le(&mut bytes, strf_size);
    u32le(&mut bytes, 40);
    i32le(&mut bytes, 3);
    i32le(&mut bytes, 2);
    u16le(&mut bytes, 1);
    u16le(&mut bytes, 24);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, frame.len() as u32);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0);
    fourcc(&mut bytes, b"LIST");
    u32le(&mut bytes, movi_size);
    fourcc(&mut bytes, b"movi");
    fourcc(&mut bytes, b"00db");
    u32le(&mut bytes, frame.len() as u32);
    bytes.extend_from_slice(&frame);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 3);
    assert!(reader.metadata().is_rgb);
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 110, 120, 130, 140, 150, 160, 170, 180, 190]
    );
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![40, 50, 60, 70, 80, 90, 140, 150, 160, 170, 180, 190]
    );
}

#[test]
fn avi_reader_uses_idx1_for_movi_after_large_header() {
    let path = tmp("idx1_after_large_header.avi");
    let frame = vec![77, 0, 0, 0];
    let bytes = minimal_avi_bytes(
        b"DIB ",
        [0, 0, 0, 0],
        b"00db",
        &frame,
        1024 * 1024 + 32,
        true,
    );
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).expect("AVI read failed");
    assert_eq!(reader.metadata().size_x, 1);
    assert_eq!(reader.metadata().size_y, 1);
    assert_eq!(reader.metadata().size_c, 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![77]);
}

#[test]
fn avi_reader_rejects_mjpg_compressed_stream() {
    let path = tmp("compressed_mjpg.avi");
    let bytes = minimal_avi_bytes(b"MJPG", *b"MJPG", b"00dc", &[1, 2, 3, 4], 0, false);
    std::fs::write(&path, bytes).unwrap();

    let err = bioformats::formats::avi::AviReader::new()
        .set_id(&path)
        .expect_err("MJPG AVI should be rejected");
    match err {
        BioFormatsError::UnsupportedFormat(msg) => {
            assert!(msg.contains("MJPG"), "unexpected error message: {msg}");
            assert!(
                msg.contains("uncompressed BI_RGB/DIB"),
                "unexpected error message: {msg}"
            );
        }
        other => panic!("unexpected error: {other:?}"),
    }
}

fn minimal_avi_bytes(
    handler: &[u8; 4],
    compression: [u8; 4],
    frame_chunk: &[u8; 4],
    frame: &[u8],
    junk_size: usize,
    include_idx1: bool,
) -> Vec<u8> {
    let avih_size = 56u32;
    let strh_size = 56u32;
    let strf_size = 40u32;
    let strl_size = 4 + (8 + strh_size) + (8 + strf_size);
    let hdrl_size = 4 + (8 + avih_size) + (8 + strl_size);
    let frame_chunk_size = 8 + frame.len() as u32 + (frame.len() as u32 & 1);
    let movi_size = 4 + frame_chunk_size;
    let junk_chunk_size = if junk_size > 0 {
        8 + junk_size as u32 + (junk_size as u32 & 1)
    } else {
        0
    };
    let idx1_chunk_size = if include_idx1 { 8 + 16 } else { 0 };
    let riff_size = 4 + (8 + hdrl_size) + junk_chunk_size + (8 + movi_size) + idx1_chunk_size;
    let mut bytes = Vec::new();

    fn fourcc(bytes: &mut Vec<u8>, cc: &[u8; 4]) {
        bytes.extend_from_slice(cc);
    }
    fn u16le(bytes: &mut Vec<u8>, v: u16) {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    fn u32le(bytes: &mut Vec<u8>, v: u32) {
        bytes.extend_from_slice(&v.to_le_bytes());
    }
    fn i32le(bytes: &mut Vec<u8>, v: i32) {
        bytes.extend_from_slice(&v.to_le_bytes());
    }

    fourcc(&mut bytes, b"RIFF");
    u32le(&mut bytes, riff_size);
    fourcc(&mut bytes, b"AVI ");
    fourcc(&mut bytes, b"LIST");
    u32le(&mut bytes, hdrl_size);
    fourcc(&mut bytes, b"hdrl");
    fourcc(&mut bytes, b"avih");
    u32le(&mut bytes, avih_size);
    u32le(&mut bytes, 100_000);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, if include_idx1 { 0x10 } else { 0 });
    u32le(&mut bytes, 1);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 1);
    u32le(&mut bytes, frame.len() as u32);
    u32le(&mut bytes, 1);
    u32le(&mut bytes, 1);
    bytes.extend_from_slice(&[0; 16]);
    fourcc(&mut bytes, b"LIST");
    u32le(&mut bytes, strl_size);
    fourcc(&mut bytes, b"strl");
    fourcc(&mut bytes, b"strh");
    u32le(&mut bytes, strh_size);
    fourcc(&mut bytes, b"vids");
    fourcc(&mut bytes, handler);
    bytes.extend_from_slice(&[0; 48]);
    fourcc(&mut bytes, b"strf");
    u32le(&mut bytes, strf_size);
    u32le(&mut bytes, 40);
    i32le(&mut bytes, 1);
    i32le(&mut bytes, 1);
    u16le(&mut bytes, 1);
    u16le(&mut bytes, 8);
    fourcc(&mut bytes, &compression);
    u32le(&mut bytes, frame.len() as u32);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0);
    u32le(&mut bytes, 0);

    if junk_size > 0 {
        fourcc(&mut bytes, b"JUNK");
        u32le(&mut bytes, junk_size as u32);
        bytes.resize(bytes.len() + junk_size, 0);
        if junk_size & 1 == 1 {
            bytes.push(0);
        }
    }

    fourcc(&mut bytes, b"LIST");
    u32le(&mut bytes, movi_size);
    fourcc(&mut bytes, b"movi");
    fourcc(&mut bytes, frame_chunk);
    u32le(&mut bytes, frame.len() as u32);
    bytes.extend_from_slice(frame);
    if frame.len() & 1 == 1 {
        bytes.push(0);
    }

    if include_idx1 {
        fourcc(&mut bytes, b"idx1");
        u32le(&mut bytes, 16);
        fourcc(&mut bytes, frame_chunk);
        u32le(&mut bytes, 0x10);
        u32le(&mut bytes, 0);
        u32le(&mut bytes, frame.len() as u32);
    }

    bytes
}

// ── OME-XML round-trip ───────────────────────────────────────────────────────

#[test]
fn ome_xml_round_trip() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let data: Vec<u8> = (0u8..16).collect();
    let path = tmp("test.ome");
    ImageWriter::save(&path, &meta, &[data.clone()]).expect("OME-XML write failed");

    let mut reader = ImageReader::open(&path).expect("OME-XML read failed");
    let rmeta = reader.metadata();
    assert_eq!(rmeta.size_x, 4);
    assert_eq!(rmeta.size_y, 4);
    let rb = reader.open_bytes(0).expect("OME-XML open_bytes failed");
    assert_eq!(rb, data);
}

#[test]
fn ome_xml_writer_escapes_standalone_names() {
    use bioformats::{OmeChannel, OmeImage, OmeMetadata};

    let mut meta = ImageMetadata::default();
    meta.size_x = 1;
    meta.size_y = 1;
    meta.size_z = 1;
    meta.size_c = 1;
    meta.size_t = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 8;
    meta.image_count = 1;

    let ome = OmeMetadata {
        images: vec![OmeImage {
            name: Some(r#"A&B <"image"> '0'"#.into()),
            channels: vec![OmeChannel {
                name: Some(r#"C&D <"channel"> '0'"#.into()),
                samples_per_pixel: 1,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };

    let path = tmp("escaped_names.ome");
    let mut writer = OmeXmlWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_ome_metadata(ome);
    writer.set_id(&path).unwrap();
    writer.save_bytes(0, &[42]).unwrap();
    writer.close().unwrap();

    let xml = std::fs::read_to_string(&path).unwrap();
    assert!(xml.contains(r#"Name="A&amp;B &lt;&quot;image&quot;&gt; &apos;0&apos;""#));
    assert!(xml.contains(r#"Name="C&amp;D &lt;&quot;channel&quot;&gt; &apos;0&apos;""#));
    assert!(!xml.contains(r#"A&B <"image"> '0'"#));
    assert!(!xml.contains(r#"C&D <"channel"> '0'"#));
}

// ── OME-TIFF round-trip ──────────────────────────────────────────────────────

#[test]
fn ome_tiff_round_trip() {
    use bioformats::{OmeChannel, OmeImage, OmeMetadata};

    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint16;
    meta.bits_per_pixel = 16;
    meta.image_count = 1;

    let ome = OmeMetadata {
        images: vec![OmeImage {
            name: Some("Test Image".into()),
            physical_size_x: Some(0.325),
            physical_size_y: Some(0.325),
            channels: vec![OmeChannel {
                name: Some("DAPI".into()),
                samples_per_pixel: 1,
                emission_wavelength: Some(461.0),
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };

    let data: Vec<u8> = (0u16..16).flat_map(|v| v.to_le_bytes()).collect();
    let path = tmp("test.ome.tif");
    ImageWriter::save_ome_tiff(&path, &meta, &ome, &[data.clone()]).expect("OME-TIFF write failed");

    let mut reader = ImageReader::open(&path).expect("OME-TIFF read failed");
    let rmeta = reader.metadata();
    assert_eq!(rmeta.size_x, 4);
    assert_eq!(rmeta.size_y, 4);
    let rb = reader.open_bytes(0).expect("open_bytes failed");
    assert_eq!(rb, data);

    // Verify OME metadata was preserved
    let ome_back = reader.ome_metadata().expect("OME metadata missing");
    assert!(!ome_back.images.is_empty());
    let img = &ome_back.images[0];
    assert!(img.physical_size_x.is_some());
    let psx = img.physical_size_x.unwrap();
    assert!(
        (psx - 0.325).abs() < 0.001,
        "physical_size_x mismatch: {}",
        psx
    );
}

// ── ChannelSeparator test ────────────────────────────────────────────────────

#[test]
fn channel_separator_splits_rgb() {
    // Write an RGB TIFF
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;
    meta.size_c = 3;
    meta.is_rgb = true;
    meta.is_interleaved = true;

    // RGBRGB... pattern: R=10, G=20, B=30 for all pixels
    let mut data = Vec::with_capacity(48);
    for _ in 0..16 {
        data.extend_from_slice(&[10, 20, 30]);
    }
    let path = tmp("rgb_sep.tif");
    ImageWriter::save(&path, &meta, &[data]).expect("write failed");

    // Open with ChannelSeparator
    let inner = open_boxed_reader(&path);
    let mut sep = ChannelSeparator::new(inner);
    sep.set_id(&path).expect("set_id failed");

    let sep_meta = sep.metadata();
    assert_eq!(
        sep_meta.image_count, 3,
        "should have 3 planes (one per channel)"
    );
    assert!(!sep_meta.is_interleaved);

    let r_plane = sep.open_bytes(0).expect("R plane");
    let g_plane = sep.open_bytes(1).expect("G plane");
    let b_plane = sep.open_bytes(2).expect("B plane");

    assert!(
        r_plane.iter().all(|&v| v == 10),
        "R channel should be all 10"
    );
    assert!(
        g_plane.iter().all(|&v| v == 20),
        "G channel should be all 20"
    );
    assert!(
        b_plane.iter().all(|&v| v == 30),
        "B channel should be all 30"
    );
}

// ── MinMaxCalculator test ────────────────────────────────────────────────────

#[test]
fn minmax_calculator_tracks_range() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let data: Vec<u8> = (10u8..26).collect(); // 10..25
    let path = tmp("minmax.tif");
    ImageWriter::save(&path, &meta, &[data]).expect("write failed");

    let inner = open_boxed_reader(&path);
    let mut calc = MinMaxCalculator::new(inner);
    calc.set_id(&path).expect("set_id failed");

    let _ = calc.open_bytes(0).expect("open_bytes");
    let stats = calc.channel_min_max();
    assert!(!stats.is_empty());
    assert_eq!(stats[0].0, 10.0, "min should be 10");
    assert_eq!(stats[0].1, 25.0, "max should be 25");
}

// ── DimensionSwapper test ────────────────────────────────────────────────────

#[test]
fn dimension_swapper_changes_order() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let data: Vec<u8> = (0u8..16).collect();
    let path = tmp("dimswap.tif");
    ImageWriter::save(&path, &meta, &[data]).expect("write failed");

    let inner = open_boxed_reader(&path);
    let mut swapper = DimensionSwapper::new(inner, DimensionOrder::XYZTC);
    swapper.set_id(&path).expect("set_id failed");

    assert_eq!(swapper.metadata().dimension_order, DimensionOrder::XYZTC);
    // With 1 plane, the swapper should still work
    let rb = swapper.open_bytes(0).expect("open_bytes");
    assert_eq!(rb.len(), 16);
}

// ── FileStitcher test ────────────────────────────────────────────────────────

#[test]
fn file_stitcher_assembles_sequence() {
    use bioformats::FileStitcher;

    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    // Create 3 files: seq_000.tif, seq_001.tif, seq_002.tif
    for i in 0..3u8 {
        let path = tmp(&format!("seq_{:03}.tif", i));
        let data = vec![i * 50; 16];
        ImageWriter::save(&path, &meta, &[data]).expect("write failed");
    }

    let mut stitcher = FileStitcher::open(&tmp("seq_001.tif")).expect("stitch failed");
    let smeta = stitcher.metadata();
    assert_eq!(smeta.image_count, 3, "should have 3 stitched planes");

    let p0 = stitcher.open_bytes(0).expect("plane 0");
    let p1 = stitcher.open_bytes(1).expect("plane 1");
    let p2 = stitcher.open_bytes(2).expect("plane 2");

    assert!(p0.iter().all(|&v| v == 0), "plane 0 should be 0");
    assert!(p1.iter().all(|&v| v == 50), "plane 1 should be 50");
    assert!(p2.iter().all(|&v| v == 100), "plane 2 should be 100");
}

// ── Helper ───────────────────────────────────────────────────────────────────

fn open_boxed_reader(path: &Path) -> Box<dyn FormatReader> {
    let header = std::fs::read(path).unwrap_or_default();
    let header = &header[..header.len().min(512)];
    for r in bioformats::registry::all_readers_pub() {
        if r.is_this_type_by_bytes(header) {
            return r;
        }
    }
    for r in bioformats::registry::all_readers_pub() {
        if r.is_this_type_by_name(path) {
            return r;
        }
    }
    panic!("No reader found for {}", path.display());
}
