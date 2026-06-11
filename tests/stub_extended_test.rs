//! Tests for the three extended.rs readers that were faithfully translated from
//! their Java references (Hamamatsu Aquacosmos NAF, Burleigh SPM, Leica LOF).
//!
//! No real sample files exist in-tree for these formats, so these tests use
//! small synthetic fixtures built to match the on-disk layout that each Java
//! reader parses.

use bioformats::formats::extended::{BurleighReader, LeicaLofReader, NafReader};
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
