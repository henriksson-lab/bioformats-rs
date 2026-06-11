use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats::common::metadata::MetadataValue;
use bioformats::common::pixel_type::PixelType;
use bioformats::common::reader::FormatReader;
use bioformats::formats::flim2::IvisionReader;

fn temp_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("bioformats_ivision_xml_{nanos}_{name}"))
}

fn write_native_ivision(path: &Path, payload: &[u8], tail: &[u8]) {
    let mut bytes = vec![0u8; 72];
    bytes[..4].copy_from_slice(b"1.0A");
    bytes[4] = 1;
    bytes[5] = 6;
    bytes[6..10].copy_from_slice(&2u32.to_be_bytes());
    bytes[10..14].copy_from_slice(&2u32.to_be_bytes());
    bytes[20..22].copy_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&vec![0u8; 2048]);
    bytes.extend_from_slice(payload);
    bytes.extend_from_slice(tail);
    std::fs::write(path, bytes).unwrap();
}

fn assert_float(md: &std::collections::HashMap<String, MetadataValue>, key: &str, expected: f64) {
    match md.get(key) {
        Some(MetadataValue::Float(value)) => assert!(
            (*value - expected).abs() < 1e-12,
            "{key}: expected {expected}, got {value}"
        ),
        other => panic!("{key}: expected float, got {other:?}"),
    }
}

#[test]
fn ivision_preserves_embedded_ome_xml_scalars_without_changing_pixels() {
    let path = temp_path("embedded.ipm");
    let payload = [0x0102u16, 0x0304, 0x0506, 0x0708]
        .into_iter()
        .flat_map(u16::to_be_bytes)
        .collect::<Vec<_>>();
    let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<OME>
  <Image ID="Image:0" Name="iVision XML image">
    <Pixels DimensionOrder="XYZCT" Type="uint16" SizeX="2" SizeY="2" SizeZ="1" SizeC="1" SizeT="1"
      PhysicalSizeX="0.25" PhysicalSizeXUnit="um"
      PhysicalSizeY="0.5" PhysicalSizeYUnit="um"
      PhysicalSizeZ="1.5" PhysicalSizeZUnit="um"
      TimeIncrement="2.0" TimeIncrementUnit="s">
      <Channel ID="Channel:0:0" Name="DAPI" SamplesPerPixel="1"
        ExcitationWavelength="405" EmissionWavelength="460" Color="-16776961"/>
    </Pixels>
  </Image>
</OME>"#;
    write_native_ivision(&path, &payload, xml);

    let mut reader = IvisionReader::new();
    reader
        .set_id(&path)
        .expect("iVision fixture with embedded XML");
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
    assert_eq!(reader.open_bytes(0).unwrap(), payload);

    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("iVision XML Metadata"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        md.get("iVision XML Source"),
        Some(MetadataValue::String(value)) if value == "embedded_tail"
    ));
    assert!(matches!(
        md.get("iVision XML Image Name"),
        Some(MetadataValue::String(value)) if value == "iVision XML image"
    ));
    assert_float(md, "iVision XML PhysicalSizeX", 0.25);
    assert_float(md, "iVision XML PhysicalSizeY", 0.5);
    assert_float(md, "iVision XML PhysicalSizeZ", 1.5);
    assert_float(md, "iVision XML TimeIncrement", 2.0);
    assert!(matches!(
        md.get("iVision XML Channel 0 Name"),
        Some(MetadataValue::String(value)) if value == "DAPI"
    ));
    assert_float(md, "iVision XML Channel 0 ExcitationWavelength", 405.0);
    assert_float(md, "iVision XML Channel 0 EmissionWavelength", 460.0);
    assert!(matches!(
        md.get("iVision XML Channel 0 Color"),
        Some(MetadataValue::Int(-16776961))
    ));

    let ome = reader.ome_metadata().expect("OME metadata");
    assert_eq!(ome.images[0].name.as_deref(), Some("iVision XML image"));
    assert_eq!(ome.images[0].channels[0].name.as_deref(), Some("DAPI"));
    assert_eq!(ome.images[0].physical_size_x, Some(0.25));
    assert_eq!(ome.images[0].physical_size_y, Some(0.5));
    assert_eq!(ome.images[0].physical_size_z, Some(1.5));
    assert_eq!(ome.images[0].time_increment, Some(2.0));

    let _ = std::fs::remove_file(path);
}

#[test]
fn ivision_reads_adjacent_ome_xml_sidecar_when_tail_has_no_xml() {
    let path = temp_path("sidecar.ipm");
    let sidecar = path.with_extension("xml");
    let payload = [0x0102u16, 0x0304, 0x0506, 0x0708]
        .into_iter()
        .flat_map(u16::to_be_bytes)
        .collect::<Vec<_>>();
    write_native_ivision(&path, &payload, b"");
    std::fs::write(
        &sidecar,
        br#"<OME><Image ID="Image:0" Name="sidecar image"><Pixels SizeX="2" SizeY="2" SizeZ="1" SizeC="1" SizeT="1"><Channel ID="Channel:0:0" Name="FITC" SamplesPerPixel="1"/></Pixels></Image></OME>"#,
    )
    .unwrap();

    let mut reader = IvisionReader::new();
    reader
        .set_id(&path)
        .expect("iVision fixture with sidecar XML");
    assert_eq!(reader.open_bytes(0).unwrap(), payload);
    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("iVision XML Source"),
        Some(MetadataValue::String(value)) if value == "sidecar"
    ));
    assert!(matches!(
        md.get("iVision XML Channel 0 Name"),
        Some(MetadataValue::String(value)) if value == "FITC"
    ));

    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(sidecar);
}

#[test]
fn ivision_flattens_proprietary_xml_tail_without_ome_image() {
    let path = temp_path("proprietary.ipm");
    let payload = [0x0102u16, 0x0304, 0x0506, 0x0708]
        .into_iter()
        .flat_map(u16::to_be_bytes)
        .collect::<Vec<_>>();
    let xml = br#"<?xml version="1.0" encoding="UTF-8"?>
<iVisionMetadata Version="2">
  <Acquisition ExposureMs="12.5" Enabled="true">
    <Operator>Ada &amp; Co</Operator>
  </Acquisition>
</iVisionMetadata>"#;
    write_native_ivision(&path, &payload, xml);

    let mut reader = IvisionReader::new();
    reader
        .set_id(&path)
        .expect("iVision fixture with proprietary XML");
    assert_eq!(reader.open_bytes(0).unwrap(), payload);

    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("iVision XML Metadata"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        md.get("iVision XML Source"),
        Some(MetadataValue::String(value)) if value == "embedded_tail"
    ));
    assert!(matches!(
        md.get("iVision XML iVisionMetadata Version"),
        Some(MetadataValue::Int(2))
    ));
    assert_float(
        md,
        "iVision XML iVisionMetadata Acquisition ExposureMs",
        12.5,
    );
    assert!(matches!(
        md.get("iVision XML iVisionMetadata Acquisition Enabled"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        md.get("iVision XML iVisionMetadata Acquisition Operator"),
        Some(MetadataValue::String(value)) if value == "Ada & Co"
    ));
    assert!(matches!(
        md.get("iVision XML Flattened Fields"),
        Some(MetadataValue::Int(value)) if *value >= 4
    ));

    let _ = std::fs::remove_file(path);
}
