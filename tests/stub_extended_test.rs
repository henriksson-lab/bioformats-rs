//! Tests for the three extended.rs readers that were faithfully translated from
//! their Java references (Hamamatsu Aquacosmos NAF, Burleigh SPM, Leica LOF).
//!
//! No real sample files exist in-tree for these formats, so these tests use
//! small synthetic fixtures built to match the on-disk layout that each Java
//! reader parses.

use bioformats::formats::extended::{BurleighReader, ImspectorReader, LeicaLofReader, NafReader};
use bioformats::{FormatReader, MetadataValue};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("bioformats_stub_{name}_{nonce}"))
}

fn utf16le(s: &str) -> Vec<u8> {
    s.encode_utf16().flat_map(|u| u.to_le_bytes()).collect()
}

// ---------------------------------------------------------------------------
// Imspector OBF/MSR (Java OBFReader)
// ---------------------------------------------------------------------------

const IMSPECTOR_FILE_MAGIC: &[u8; 8] = b"OMAS_BF\n";
const IMSPECTOR_MAGIC_NUMBER: u16 = 0xffff;
const IMSPECTOR_STACK_MAGIC: &[u8; 14] = b"OMAS_BF_STACK\n";

fn imspector_header(version: i32) -> Vec<u8> {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(IMSPECTOR_FILE_MAGIC);
    bytes.extend_from_slice(&IMSPECTOR_MAGIC_NUMBER.to_le_bytes());
    bytes.extend_from_slice(&version.to_le_bytes());
    bytes
}

fn zlib_compress(bytes: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn native_imspector_v1_stack(
    width: i32,
    height: i32,
    z: i32,
    c: i32,
    t: i32,
    compression: i32,
    payload: &[u8],
) -> Vec<u8> {
    let mut bytes = imspector_header(1);
    let stack_offset = 32u64;
    bytes.extend_from_slice(&stack_offset.to_le_bytes());
    bytes.extend_from_slice(&0i32.to_le_bytes());
    bytes.resize(stack_offset as usize, 0);

    bytes.extend_from_slice(IMSPECTOR_STACK_MAGIC);
    bytes.extend_from_slice(&IMSPECTOR_MAGIC_NUMBER.to_le_bytes());
    bytes.extend_from_slice(&1i32.to_le_bytes());
    bytes.extend_from_slice(&5i32.to_le_bytes());
    for size in [width, height, z, c, t] {
        bytes.extend_from_slice(&size.to_le_bytes());
    }
    for _ in 5..15 {
        bytes.extend_from_slice(&1i32.to_le_bytes());
    }
    for _ in 0..30 {
        bytes.extend_from_slice(&0f64.to_bits().to_le_bytes());
    }
    bytes.extend_from_slice(&0x01i32.to_le_bytes());
    bytes.extend_from_slice(&compression.to_le_bytes());
    bytes.extend_from_slice(&0i32.to_le_bytes());
    bytes.extend_from_slice(&0i32.to_le_bytes());
    bytes.extend_from_slice(&0i32.to_le_bytes());
    bytes.extend_from_slice(&0i64.to_le_bytes());
    bytes.extend_from_slice(&(payload.len() as i64).to_le_bytes());
    bytes.extend_from_slice(&0i64.to_le_bytes());
    bytes.extend_from_slice(payload);

    bytes.extend_from_slice(&124i32.to_le_bytes());
    for _ in 0..30 {
        bytes.extend_from_slice(&0i32.to_le_bytes());
    }
    for _ in 0..5 {
        bytes.extend_from_slice(&0i32.to_le_bytes());
    }
    bytes
}

#[test]
fn imspector_reads_native_v1_zlib_contiguous_stack() {
    let pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
    let compressed = zlib_compress(&pixels);
    let path = temp_path("native_v1_zlib.obf");
    std::fs::write(
        &path,
        native_imspector_v1_stack(2, 2, 2, 1, 1, 1, &compressed),
    )
    .unwrap();

    let mut reader = ImspectorReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_z, 2);
    assert_eq!(meta.image_count, 2);
    assert!(matches!(
        meta.series_metadata.get("imspector_version_subset"),
        Some(MetadataValue::String(value)) if value == "native-v1-zlib-contiguous"
    ));

    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
    assert_eq!(reader.open_bytes_region(1, 0, 1, 2, 1).unwrap(), vec![7, 8]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn imspector_rejects_native_v1_zlib_wrong_decompressed_size() {
    let compressed = zlib_compress(&[1, 2, 3]);
    let path = temp_path("native_v1_zlib_wrong_size.obf");
    std::fs::write(
        &path,
        native_imspector_v1_stack(2, 2, 1, 1, 1, 1, &compressed),
    )
    .unwrap();

    let mut reader = ImspectorReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(format!("{err}").contains("native decompressed payload length 3"));
    assert!(format!("{err}").contains("declared stack size 4"));

    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// Burleigh SPM (Java BurleighReader)
// ---------------------------------------------------------------------------

#[test]
fn burleigh_detects_magic() {
    let reader = BurleighReader::new();
    // Version 1 magic (0x06) and version 2 magic (0x46), both ending in 0x40.
    assert!(reader.is_this_type_by_bytes(&[0x66, 0x66, 0x06, 0x40]));
    assert!(reader.is_this_type_by_bytes(&[0x66, 0x66, 0x46, 0x40]));
    // Wrong sentinel bytes are rejected.
    assert!(!reader.is_this_type_by_bytes(&[0x66, 0x66, 0x00, 0x40]));
    assert!(!reader.is_this_type_by_bytes(&[0x00, 0x66, 0x46, 0x40]));
    assert!(!reader.is_this_type_by_bytes(b"PK"));
}

#[test]
fn burleigh_reads_version1_uint16_plane() {
    // Version float 0x40066666 (little-endian) ~= 2.1 -> (int)2.1 - 1 = 1.
    let mut bytes = vec![0x66, 0x66, 0x06, 0x40];
    bytes.extend_from_slice(&2i16.to_le_bytes()); // sizeX
    bytes.extend_from_slice(&2i16.to_le_bytes()); // sizeY
                                                  // 2x2 UINT16 plane at offset 8 (version 1).
    let plane: Vec<u8> = vec![1, 0, 2, 0, 3, 0, 4, 0];
    bytes.extend_from_slice(&plane);

    let path = temp_path("burleigh_v1.img");
    std::fs::write(&path, &bytes).unwrap();

    let mut reader = BurleighReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.image_count, 1);
    assert_eq!(meta.pixel_type, bioformats::PixelType::Uint16);

    assert_eq!(reader.open_bytes(0).unwrap(), plane);
    // Column x=1 of the 2x2 plane (two UINT16 values: 2 and 4).
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
        vec![2, 0, 4, 0]
    );
    assert!(reader.open_bytes(1).is_err());

    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// Leica LOF (Java LOFReader)
// ---------------------------------------------------------------------------

fn build_lof(width: u32, height: u32, channel_bytes_inc: u32, pixels: &[u8]) -> Vec<u8> {
    // Leica <ImageDescription>: DimID 1 = X, 2 = Y, plus one channel.
    let xml = format!(
        "<Image><ImageDescription>\
<Channels><ChannelDescription Resolution=\"8\" BytesInc=\"{channel_bytes_inc}\"/></Channels>\
<Dimensions>\
<DimensionDescription DimID=\"1\" NumberOfElements=\"{width}\" BytesInc=\"1\"/>\
<DimensionDescription DimID=\"2\" NumberOfElements=\"{height}\" BytesInc=\"{width}\"/>\
</Dimensions>\
</ImageDescription></Image>"
    );
    build_lof_with_xml(&xml, pixels)
}

fn build_lof_with_xml(xml: &str, pixels: &[u8]) -> Vec<u8> {
    let xml_units = xml.encode_utf16().count() as i32;

    let mut b = Vec::new();
    // Part 1: header.
    b.extend_from_slice(&0x70i32.to_le_bytes()); // magic
    b.extend_from_slice(&0i32.to_le_bytes()); // chunk length
    b.push(0x2a); // memory marker
    b.extend_from_slice(&15i32.to_le_bytes()); // type-name length
    b.extend_from_slice(&utf16le("LMS_Object_File"));
    b.push(0x2a); // major version marker
    b.extend_from_slice(&2i32.to_le_bytes());
    b.push(0x2a); // minor version marker
    b.extend_from_slice(&0i32.to_le_bytes());
    b.push(0x2a); // memory-size marker
    b.extend_from_slice(&(pixels.len() as i64).to_le_bytes());
    // Part 2: memory block (raw pixels).
    b.extend_from_slice(pixels);
    // Part 3: XML.
    b.extend_from_slice(&0x70i32.to_le_bytes()); // magic
    b.extend_from_slice(&0i32.to_le_bytes()); // chunk length
    b.push(0x2a); // marker
    b.extend_from_slice(&xml_units.to_le_bytes());
    b.extend_from_slice(&utf16le(&xml));
    b
}

#[test]
fn lof_detects_magic() {
    // width multiple of 4 keeps the row-padding logic disabled.
    let pixels = vec![10u8, 20, 30, 40];
    let bytes = build_lof(4, 1, 1, &pixels);

    let reader = LeicaLofReader::new();
    assert!(reader.is_this_type_by_bytes(&bytes));
    // A LIF-style file (different type name) is rejected.
    let mut wrong = bytes.clone();
    // Corrupt the type name's first UTF-16 unit.
    wrong[13] = b'X';
    assert!(!reader.is_this_type_by_bytes(&wrong));
    assert!(reader.is_this_type_by_name(std::path::Path::new("x.lof")));
    assert!(!reader.is_this_type_by_name(std::path::Path::new("x.tif")));
}

#[test]
fn lof_reads_single_image() {
    let pixels = vec![10u8, 20, 30, 40];
    let bytes = build_lof(4, 1, 1, &pixels);
    let path = temp_path("single.lof");
    std::fs::write(&path, &bytes).unwrap();

    let mut reader = LeicaLofReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 1);
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 4);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.image_count, 1);
    assert_eq!(meta.pixel_type, bioformats::PixelType::Uint8);

    assert_eq!(reader.open_bytes(0).unwrap(), pixels);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 1).unwrap(),
        vec![20, 30]
    );
    assert!(reader.open_bytes(1).is_err());

    let _ = std::fs::remove_file(path);
}

#[test]
fn lof_projects_channel_names_lut_and_wavelength_metadata() {
    let pixels = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
    let xml = "\
<Image><ImageDescription>\
<Channels>\
<ChannelDescription Name=\"DAPI\" Resolution=\"8\" BytesInc=\"1\" ExcitationWavelength=\"405\" EmissionWavelength=\"460\" LUTName=\"Blue\"/>\
<ChannelDescription DyeName=\"FITC\" Resolution=\"8\" BytesInc=\"1\" ExcitationWavelength=\"488\" EmissionWavelength=\"525\" LUTName=\"Green\"/>\
</Channels>\
<Dimensions>\
<DimensionDescription DimID=\"1\" NumberOfElements=\"4\" BytesInc=\"1\"/>\
<DimensionDescription DimID=\"2\" NumberOfElements=\"1\" BytesInc=\"4\"/>\
</Dimensions>\
</ImageDescription></Image>";
    let bytes = build_lof_with_xml(xml, &pixels);
    let path = temp_path("channel_metadata.lof");
    std::fs::write(&path, &bytes).unwrap();

    let mut reader = LeicaLofReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.image_count, 2);
    assert!(matches!(
        meta.series_metadata.get("lof.channel.0.name"),
        Some(MetadataValue::String(value)) if value == "DAPI"
    ));
    assert!(matches!(
        meta.series_metadata.get("lof.channel.1.dye_name"),
        Some(MetadataValue::String(value)) if value == "FITC"
    ));
    assert!(matches!(
        meta.series_metadata.get("lof.channel.1.lut_name"),
        Some(MetadataValue::String(value)) if value == "Green"
    ));
    assert!(matches!(
        meta.series_metadata.get("lof.channel.0.excitation_wavelength"),
        Some(MetadataValue::Float(value)) if (*value - 405.0).abs() < f64::EPSILON
    ));

    let ome = reader.ome_metadata().unwrap();
    assert_eq!(ome.images[0].channels.len(), 2);
    assert_eq!(ome.images[0].channels[0].name.as_deref(), Some("DAPI"));
    assert_eq!(ome.images[0].channels[1].name.as_deref(), Some("FITC"));
    assert_eq!(ome.images[0].channels[0].excitation_wavelength, Some(405.0));
    assert_eq!(ome.images[0].channels[1].emission_wavelength, Some(525.0));

    let _ = std::fs::remove_file(path);
}

#[test]
fn lof_records_bgr_channel_order_from_channel_offsets() {
    let pixels = vec![10u8, 20, 30, 40, 50, 60];
    let xml = "\
<Image><ImageDescription>\
<Channels>\
<ChannelDescription Name=\"Red\" Resolution=\"8\" BytesInc=\"2\"/>\
<ChannelDescription Name=\"Green\" Resolution=\"8\" BytesInc=\"1\"/>\
<ChannelDescription Name=\"Blue\" Resolution=\"8\" BytesInc=\"0\"/>\
</Channels>\
<Dimensions>\
<DimensionDescription DimID=\"1\" NumberOfElements=\"2\" BytesInc=\"3\"/>\
<DimensionDescription DimID=\"2\" NumberOfElements=\"1\" BytesInc=\"6\"/>\
</Dimensions>\
</ImageDescription></Image>";
    let bytes = build_lof_with_xml(xml, &pixels);
    let path = temp_path("bgr_channel_order.lof");
    std::fs::write(&path, &bytes).unwrap();

    let mut reader = LeicaLofReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert!(meta.is_rgb);
    assert_eq!(meta.size_c, 3);
    assert!(matches!(
        meta.series_metadata.get("lof.rgb.channel_order"),
        Some(MetadataValue::String(value)) if value == "BGR"
    ));
    assert!(matches!(
        meta.series_metadata.get("lof.rgb.channel_order_source"),
        Some(MetadataValue::String(value)) if value == "ChannelDescription BytesInc"
    ));
    assert!(matches!(
        meta.series_metadata.get("lof.rgb.channel_order_offsets"),
        Some(MetadataValue::String(value)) if value == "0,1,2"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), pixels);

    let _ = std::fs::remove_file(path);
}

#[test]
fn lof_projects_structured_instrument_detector_roi_and_stage_metadata() {
    let pixels = vec![1u8, 2, 3, 4];
    let xml = "\
<Image><ImageDescription>\
<InstrumentDescription Manufacturer=\"Leica\" Model=\"SP8\"/>\
<DetectorDescription Name=\"HyD 1\" Type=\"HyD\" Gain=\"1.25\" Offset=\"2\"/>\
<StagePosition X=\"12.5\" Y=\"-3.25\" Z=\"1.5\"/>\
<AcquisitionDescription Mode=\"Sequential\" LaserPower=\"7.5\"/>\
<ROI Name=\"Cell 1\" X=\"10\" Y=\"20\" Width=\"30\" Height=\"40\"/>\
<Channels><ChannelDescription Name=\"DAPI\" Resolution=\"8\" BytesInc=\"1\"/></Channels>\
<Dimensions>\
<DimensionDescription DimID=\"1\" NumberOfElements=\"4\" BytesInc=\"1\"/>\
<DimensionDescription DimID=\"2\" NumberOfElements=\"1\" BytesInc=\"4\"/>\
</Dimensions>\
</ImageDescription></Image>";
    let bytes = build_lof_with_xml(xml, &pixels);
    let path = temp_path("structured_metadata.lof");
    std::fs::write(&path, &bytes).unwrap();

    let mut reader = LeicaLofReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata.get("lof.instrument.0.manufacturer"),
        Some(MetadataValue::String(value)) if value == "Leica"
    ));
    assert!(matches!(
        meta.series_metadata.get("lof.detector.0.type"),
        Some(MetadataValue::String(value)) if value == "HyD"
    ));
    assert!(matches!(
        meta.series_metadata.get("lof.stage.0.x"),
        Some(MetadataValue::Float(value)) if (*value - 12.5).abs() < f64::EPSILON
    ));
    assert!(matches!(
        meta.series_metadata.get("lof.acquisition.0.mode"),
        Some(MetadataValue::String(value)) if value == "Sequential"
    ));
    assert!(matches!(
        meta.series_metadata.get("lof.roi.0.name"),
        Some(MetadataValue::String(value)) if value == "Cell 1"
    ));

    let ome = reader.ome_metadata().unwrap();
    assert_eq!(ome.images[0].instrument_ref, Some(0));
    assert_eq!(ome.instruments.len(), 1);
    assert_eq!(
        ome.instruments[0].microscope_manufacturer.as_deref(),
        Some("Leica")
    );
    assert_eq!(ome.instruments[0].microscope_model.as_deref(), Some("SP8"));
    assert_eq!(ome.instruments[0].detectors.len(), 1);
    assert_eq!(
        ome.instruments[0].detectors[0].model.as_deref(),
        Some("HyD 1")
    );
    assert_eq!(
        ome.instruments[0].detectors[0].detector_type.as_deref(),
        Some("HyD")
    );
    assert_eq!(ome.rois.len(), 1);
    assert_eq!(ome.rois[0].name.as_deref(), Some("Cell 1"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn lof_rejects_non_lof() {
    let path = temp_path("garbage.lof");
    std::fs::write(&path, b"not a leica lof file at all").unwrap();
    let mut reader = LeicaLofReader::new();
    assert!(reader.set_id(&path).is_err());
    assert_eq!(reader.series_count(), 0);
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// Hamamatsu Aquacosmos NAF (Java NAFReader)
// ---------------------------------------------------------------------------

#[test]
fn naf_detection_is_extension_only() {
    let reader = NafReader::new();
    assert!(reader.is_this_type_by_name(std::path::Path::new("image.naf")));
    assert!(reader.is_this_type_by_name(std::path::Path::new("IMAGE.NAF")));
    assert!(!reader.is_this_type_by_name(std::path::Path::new("image.tif")));
    // Java NAFReader has no byte-based detection.
    assert!(!reader.is_this_type_by_bytes(b"II\x00\x00anything"));
}

#[test]
fn naf_rejects_garbage_without_panicking() {
    let path = temp_path("garbage.naf");
    std::fs::write(&path, b"II not really a naf file, just some bytes here").unwrap();
    let mut reader = NafReader::new();
    // The offset-scanning parser must fail cleanly (no panic) on junk input.
    assert!(reader.set_id(&path).is_err());
    let _ = std::fs::remove_file(path);
}
