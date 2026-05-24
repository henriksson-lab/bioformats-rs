use bioformats::{
    create_lsid, DimensionOrder, ImageMetadata, MetadataValue, ModuloAnnotation, OmeAnnotation,
    OmeMetadata, PixelType,
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
