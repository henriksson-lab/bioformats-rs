use bioformats::{
    create_lsid, DimensionOrder, ImageMetadata, MetadataValue, ModuloAnnotation, OmeAnnotation,
    OmeChannel, OmeDichroic, OmeFilter, OmeImage, OmeInstrument, OmeLightSource, OmeMetadata,
    PixelType,
};
use std::collections::HashMap;

fn populated_meta() -> ImageMetadata {
    ImageMetadata {
        size_x: 32,
        size_y: 16,
        size_z: 2,
        size_c: 3,
        size_t: 4,
        pixel_type: PixelType::Uint16,
        bits_per_pixel: 16,
        image_count: 24,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: Some(ModuloAnnotation {
            parent_dimension: "Z".into(),
            modulo_type: "phase".into(),
            start: 0.0,
            step: 0.5,
            end: 1.0,
            unit: "rad".into(),
            labels: Vec::new(),
        }),
        modulo_c: None,
        modulo_t: None,
    }
}

#[test]
fn metadata_tools_helpers_populate_and_verify_minimum_pixels() {
    let meta = populated_meta();
    let ome = OmeMetadata::populate_metadata(&meta);

    assert_eq!(ome.images.len(), 1);
    assert_eq!(ome.images[0].channels.len(), 3);
    assert!(ome.images[0]
        .channels
        .iter()
        .all(|channel| channel.samples_per_pixel == 1));
    assert_eq!(
        ome.images[0].modulo_z.as_ref().unwrap().modulo_type,
        "phase"
    );
    ome.verify_minimum_populated(&meta, 0).unwrap();
}

#[test]
fn metadata_tools_rgb_uses_one_channel_with_multiple_samples() {
    let mut meta = populated_meta();
    meta.size_z = 1;
    meta.size_c = 3;
    meta.size_t = 1;
    meta.image_count = 1;
    meta.is_rgb = true;
    meta.is_interleaved = true;

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert_eq!(ome.images[0].channels.len(), 1);
    assert_eq!(ome.images[0].channels[0].samples_per_pixel, 3);
    ome.verify_minimum_populated(&meta, 0).unwrap();
}

#[test]
fn metadata_tools_helpers_create_lsid() {
    assert_eq!(create_lsid("Image", &[0]), "Image:0");
    assert_eq!(create_lsid("Channel", &[2, 5]), "Channel:2:5");
}

#[test]
fn metadata_tools_helpers_reject_incomplete_minimum_pixels() {
    let mut meta = populated_meta();
    meta.image_count = 1;
    let ome = OmeMetadata::from_image_metadata(&meta);

    assert!(ome.verify_minimum_populated(&meta, 0).is_err());
}

#[test]
fn metadata_tools_helpers_store_channel_globals_and_original_metadata() {
    let mut meta = populated_meta();
    meta.series_metadata.insert(
        "AcquisitionMode".into(),
        MetadataValue::String("test".into()),
    );
    meta.series_metadata
        .insert("Gain".into(), MetadataValue::Float(1.5));

    let mut ome = OmeMetadata::convert_metadata(&meta);
    ome.add_channel_global_min_max(0, 2, 10.0, 4095.0).unwrap();
    ome.add_original_metadata_annotations(&meta, 0).unwrap();

    assert_eq!(ome.annotations.len(), 2);
    match &ome.annotations[0] {
        OmeAnnotation::MapAnnotation {
            namespace, values, ..
        } => {
            assert_eq!(
                namespace.as_deref(),
                Some("openmicroscopy.org/bioformats/channel-global-min-max")
            );
            assert!(values.contains(&("Channel".into(), "Channel:0:2".into())));
            assert!(values.contains(&("GlobalMin".into(), "10".into())));
            assert!(values.contains(&("GlobalMax".into(), "4095".into())));
        }
        _ => panic!("expected map annotation"),
    }
    match &ome.annotations[1] {
        OmeAnnotation::MapAnnotation {
            namespace, values, ..
        } => {
            assert_eq!(
                namespace.as_deref(),
                Some("openmicroscopy.org/bioformats/original-metadata")
            );
            assert!(values.contains(&("AcquisitionMode".into(), "test".into())));
            assert!(values.contains(&("Gain".into(), "1.5".into())));
        }
        _ => panic!("expected map annotation"),
    }

    let xml = ome.to_ome_xml(&meta);
    assert!(xml.contains("<StructuredAnnotations>"));
    assert!(xml.contains(r#"Namespace="openmicroscopy.org/bioformats/channel-global-min-max""#));
    assert!(xml.contains(r#"<M K="Channel">Channel:0:2</M>"#));
    assert!(xml.contains(r#"<M K="AcquisitionMode">test</M>"#));
}

#[test]
fn ome_metadata_parser_decodes_entities_and_matches_exact_attributes() {
    let xml = r#"
<ome:OME xmlns:ome="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <ome:Image ObjectiveID="not-an-image-id" ID="Image:0" Name="A&amp;B &lt;image&gt;">
    <ome:Description>first &amp; second</ome:Description>
    <ome:Pixels ID="Pixels:0" PhysicalSizeX="500" PhysicalSizeXUnit="nm" SizeX="1" SizeY="1" SizeZ="1" SizeC="1" SizeT="1">
      <ome:Channel ID="Channel:0:0" Name="DAPI &amp; FITC" SamplesPerPixel="1"/>
    </ome:Pixels>
  </ome:Image>
  <ome:StructuredAnnotations>
    <ome:MapAnnotation ID="Annotation:0" Namespace="ns&amp;value">
      <ome:Value><ome:M K="key&amp;1">value &amp; one</ome:M></ome:Value>
    </ome:MapAnnotation>
  </ome:StructuredAnnotations>
</ome:OME>"#;

    let ome = OmeMetadata::from_ome_xml(xml);

    assert_eq!(ome.images.len(), 1);
    assert_eq!(ome.images[0].name.as_deref(), Some("A&B <image>"));
    assert_eq!(ome.images[0].description.as_deref(), Some("first & second"));
    assert_eq!(ome.images[0].physical_size_x, Some(0.5));
    assert_eq!(
        ome.images[0].channels[0].name.as_deref(),
        Some("DAPI & FITC")
    );
    match &ome.annotations[0] {
        OmeAnnotation::MapAnnotation {
            namespace, values, ..
        } => {
            assert_eq!(namespace.as_deref(), Some("ns&value"));
            assert_eq!(values, &vec![("key&1".into(), "value & one".into())]);
        }
        _ => panic!("expected map annotation"),
    }
}

#[test]
fn ome_metadata_parser_handles_gt_inside_quoted_start_tag_attribute() {
    let xml = r#"
<ome:OME xmlns:ome="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <ome:Image ID="Image:0">
    <ome:Pixels ID="Pixels:0" Name="quoted > delimiter" PhysicalSizeX="2" PhysicalSizeXUnit="µm" SizeX="1" SizeY="1" SizeZ="1" SizeC="1" SizeT="1">
      <ome:Channel ID="Channel:0:0" Name="DAPI" SamplesPerPixel="1"/>
    </ome:Pixels>
  </ome:Image>
</ome:OME>"#;

    let ome = OmeMetadata::from_ome_xml(xml);

    assert_eq!(ome.images[0].physical_size_x, Some(2.0));
    assert_eq!(ome.images[0].channels[0].name.as_deref(), Some("DAPI"));
}

#[test]
fn ome_metadata_serializer_writes_only_image_described_by_single_core_metadata() {
    let meta = populated_meta();
    let ome = OmeMetadata {
        images: vec![
            OmeImage {
                name: Some("first".into()),
                channels: vec![OmeChannel {
                    samples_per_pixel: 1,
                    ..Default::default()
                }],
                ..Default::default()
            },
            OmeImage {
                name: Some("second".into()),
                channels: vec![OmeChannel {
                    samples_per_pixel: 1,
                    ..Default::default()
                }],
                ..Default::default()
            },
        ],
        ..Default::default()
    };

    let xml = ome.to_ome_xml(&meta);

    assert_eq!(xml.matches("<Image ").count(), 1);
    assert!(xml.contains(r#"Name="first""#));
    assert!(!xml.contains(r#"Name="second""#));
}

#[test]
fn ome_metadata_verify_rejects_extra_channels() {
    let mut meta = populated_meta();
    meta.size_c = 1;
    meta.image_count = meta.size_z * meta.size_c * meta.size_t;
    let ome = OmeMetadata {
        images: vec![OmeImage {
            channels: vec![
                OmeChannel {
                    samples_per_pixel: 1,
                    ..Default::default()
                },
                OmeChannel {
                    samples_per_pixel: 1,
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        ..Default::default()
    };

    let err = ome.verify_minimum_populated(&meta, 0).unwrap_err();
    assert!(
        matches!(err, bioformats::BioFormatsError::InvalidData(ref message) if message.contains("metadata SizeC requires 1")),
        "{err:?}"
    );
}

#[test]
fn ome_metadata_verify_rejects_wrong_rgb_samples_per_pixel() {
    let mut meta = populated_meta();
    meta.size_c = 3;
    meta.image_count = meta.size_z * meta.size_t;
    meta.is_rgb = true;
    meta.is_interleaved = true;
    let ome = OmeMetadata {
        images: vec![OmeImage {
            channels: vec![OmeChannel {
                samples_per_pixel: 1,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };

    let err = ome.verify_minimum_populated(&meta, 0).unwrap_err();
    assert!(
        matches!(err, bioformats::BioFormatsError::InvalidData(ref message) if message.contains("SamplesPerPixel=3")),
        "{err:?}"
    );
}

#[test]
fn add_channel_global_min_max_rejects_missing_channel() {
    let mut ome = OmeMetadata {
        images: vec![OmeImage {
            channels: vec![OmeChannel {
                samples_per_pixel: 1,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };

    let err = ome.add_channel_global_min_max(0, 1, 0.0, 1.0).unwrap_err();
    assert!(
        matches!(err, bioformats::BioFormatsError::InvalidData(ref message) if message.contains("missing OME Channel")),
        "{err:?}"
    );
}

#[test]
fn ome_metadata_serializer_generates_unique_instrument_ids() {
    let mut meta = populated_meta();
    meta.size_c = 1;
    meta.image_count = meta.size_z * meta.size_c * meta.size_t;
    let ome = OmeMetadata {
        instruments: vec![OmeInstrument {
            light_sources: vec![OmeLightSource::default(), OmeLightSource::default()],
            filters: vec![OmeFilter::default(), OmeFilter::default()],
            dichroics: vec![OmeDichroic::default(), OmeDichroic::default()],
            ..Default::default()
        }],
        images: vec![OmeImage {
            channels: vec![OmeChannel {
                samples_per_pixel: 1,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };

    let xml = ome.to_ome_xml(&meta);

    assert!(xml.contains(r#"ID="LightSource:0:0""#));
    assert!(xml.contains(r#"ID="LightSource:0:1""#));
    assert!(xml.contains(r#"ID="Filter:0:0""#));
    assert!(xml.contains(r#"ID="Filter:0:1""#));
    assert!(xml.contains(r#"ID="Dichroic:0:0""#));
    assert!(xml.contains(r#"ID="Dichroic:0:1""#));
}

#[test]
fn ome_metadata_serializer_sanitizes_light_source_element_names() {
    let mut meta = populated_meta();
    meta.size_c = 1;
    meta.image_count = meta.size_z * meta.size_c * meta.size_t;
    let ome = OmeMetadata {
        instruments: vec![OmeInstrument {
            light_sources: vec![
                OmeLightSource {
                    id: Some("LightSource:0".into()),
                    light_source_type: Some("Laser Source".into()),
                    ..Default::default()
                },
                OmeLightSource {
                    id: Some("LightSource:1".into()),
                    light_source_type: Some("LED".into()),
                    ..Default::default()
                },
            ],
            ..Default::default()
        }],
        images: vec![OmeImage {
            channels: vec![OmeChannel {
                samples_per_pixel: 1,
                ..Default::default()
            }],
            ..Default::default()
        }],
        ..Default::default()
    };

    let xml = ome.to_ome_xml(&meta);

    assert!(xml.contains(r#"<GenericExcitationSource ID="LightSource:0""#));
    assert!(xml.contains(r#"<LightEmittingDiode ID="LightSource:1""#));
    assert!(!xml.contains("<Laser Source"));
}
