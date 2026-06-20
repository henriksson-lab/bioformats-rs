use bioformats::{
    create_lsid, DimensionOrder, ImageMetadata, MetadataValue, ModuloAnnotation, OmeAnnotation,
    OmeChannel, OmeDichroic, OmeFilter, OmeImage, OmeInstrument, OmeLightSource, OmeMetadata,
    OmeShape, PixelType,
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
        thumbnail: false,
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
fn metadata_tools_to_ome_xml_writes_pixels_big_endian_like_java() {
    let mut meta = populated_meta();
    let ome = OmeMetadata::populate_metadata(&meta);

    let little_xml = ome.to_ome_xml(&meta);
    assert!(little_xml.contains(r#"BigEndian="false""#));

    meta.is_little_endian = false;
    let big_xml = ome.to_ome_xml(&meta);
    assert!(big_xml.contains(r#"BigEndian="true""#));
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
fn metadata_tools_project_objective_magnification_to_ome() {
    let mut meta = populated_meta();
    meta.series_metadata
        .insert("objective.magnification".into(), MetadataValue::Float(40.0));

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert_eq!(ome.images[0].instrument_ref, Some(0));
    assert_eq!(ome.images[0].objective_ref, Some(0));
    assert_eq!(ome.instruments.len(), 1);
    assert_eq!(ome.instruments[0].id.as_deref(), Some("Instrument:0"));
    assert_eq!(ome.instruments[0].objectives.len(), 1);
    assert_eq!(
        ome.instruments[0].objectives[0].id.as_deref(),
        Some("Objective:0:0")
    );
    assert_eq!(
        ome.instruments[0].objectives[0].nominal_magnification,
        Some(40.0)
    );

    let xml = ome.to_ome_xml(&meta);
    assert!(xml.contains(r#"<Instrument ID="Instrument:0">"#));
    assert!(xml.contains(r#"<Objective ID="Objective:0:0" NominalMagnification="40"/>"#));
    assert!(xml.contains(r#"<InstrumentRef ID="Instrument:0"/>"#));
    assert!(xml.contains(r#"<ObjectiveSettings ID="Objective:0:0"/>"#));
}

#[test]
fn metadata_tools_project_generic_prefixed_metadata_to_ome() {
    let mut meta = populated_meta();
    meta.series_metadata.insert(
        "slidebook7.record.objective.name".into(),
        MetadataValue::String("Plan Apo 60x".into()),
    );
    meta.series_metadata.insert(
        "slidebook7.record.objective.numerical_aperture".into(),
        MetadataValue::Float(1.4),
    );
    meta.series_metadata.insert(
        "slidebook7.record.objective.immersion".into(),
        MetadataValue::String("Oil".into()),
    );
    meta.series_metadata.insert(
        "nis.detector.0.name".into(),
        MetadataValue::String("Prime BSI".into()),
    );
    meta.series_metadata.insert(
        "nis.detector.0.type".into(),
        MetadataValue::String("sCMOS".into()),
    );
    meta.series_metadata
        .insert("nis.detector.0.gain".into(), MetadataValue::Float(2.5));
    meta.series_metadata.insert(
        "xlef.lms.channel.0.name".into(),
        MetadataValue::String("DAPI".into()),
    );
    meta.series_metadata.insert(
        "xlef.lms.channel.0.excitation_wavelength".into(),
        MetadataValue::Float(405.0),
    );
    meta.series_metadata.insert(
        "xlef.lms.channel.0.emission_wavelength".into(),
        MetadataValue::Float(460.0),
    );
    meta.series_metadata.insert(
        "xlef.lms.channel.1.name".into(),
        MetadataValue::String("FITC".into()),
    );
    meta.series_metadata.insert(
        "xlef.lms.channel.1.emission_wavelength".into(),
        MetadataValue::Float(525.0),
    );

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert_eq!(ome.images[0].instrument_ref, Some(0));
    assert_eq!(ome.images[0].objective_ref, Some(0));
    assert_eq!(ome.images[0].channels[0].name.as_deref(), Some("DAPI"));
    assert_eq!(ome.images[0].channels[0].excitation_wavelength, Some(405.0));
    assert_eq!(ome.images[0].channels[0].emission_wavelength, Some(460.0));
    assert_eq!(ome.images[0].channels[1].name.as_deref(), Some("FITC"));
    assert_eq!(ome.images[0].channels[1].emission_wavelength, Some(525.0));

    let instrument = &ome.instruments[0];
    assert_eq!(instrument.objectives.len(), 1);
    assert_eq!(
        instrument.objectives[0].model.as_deref(),
        Some("Plan Apo 60x")
    );
    assert_eq!(instrument.objectives[0].lens_na, Some(1.4));
    assert_eq!(instrument.objectives[0].immersion.as_deref(), Some("Oil"));
    assert_eq!(instrument.detectors.len(), 1);
    assert_eq!(instrument.detectors[0].model.as_deref(), Some("Prime BSI"));
    assert_eq!(
        instrument.detectors[0].detector_type.as_deref(),
        Some("sCMOS")
    );
    assert_eq!(instrument.detectors[0].gain, Some(2.5));

    let xml = ome.to_ome_xml(&meta);
    assert!(xml.contains(
        r#"<Objective ID="Objective:0:0" Model="Plan Apo 60x" LensNA="1.4" Immersion="Oil"/>"#
    ));
    assert!(
        xml.contains(r#"<Detector ID="Detector:0:0" Model="Prime BSI" Type="sCMOS" Gain="2.5"/>"#)
    );
    assert!(xml.contains(r#"<Channel ID="Channel:0:0" SamplesPerPixel="1" Name="DAPI" EmissionWavelength="460" ExcitationWavelength="405"/>"#));
}

#[test]
fn metadata_tools_project_generic_light_source_metadata_to_ome() {
    let mut meta = populated_meta();
    meta.series_metadata.insert(
        "nis.illumination.0.name".into(),
        MetadataValue::String("Spectra X".into()),
    );
    meta.series_metadata.insert(
        "nis.illumination.0.manufacturer".into(),
        MetadataValue::String("Lumencor".into()),
    );
    meta.series_metadata.insert(
        "nis.illumination.0.type".into(),
        MetadataValue::String("LED".into()),
    );
    meta.series_metadata.insert(
        "nis.illumination.0.power".into(),
        MetadataValue::Float(35.0),
    );

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert_eq!(ome.images[0].instrument_ref, Some(0));
    assert_eq!(ome.instruments.len(), 1);
    assert!(ome.instruments[0].objectives.is_empty());
    assert!(ome.instruments[0].detectors.is_empty());
    assert_eq!(ome.instruments[0].light_sources.len(), 1);
    let light_source = &ome.instruments[0].light_sources[0];
    assert_eq!(light_source.id.as_deref(), Some("LightSource:0:0"));
    assert_eq!(light_source.model.as_deref(), Some("Spectra X"));
    assert_eq!(light_source.manufacturer.as_deref(), Some("Lumencor"));
    assert_eq!(light_source.light_source_type.as_deref(), Some("LED"));
    assert_eq!(light_source.power, Some(35.0));

    let xml = ome.to_ome_xml(&meta);
    assert!(xml.contains(
        r#"<LightEmittingDiode ID="LightSource:0:0" Model="Spectra X" Manufacturer="Lumencor" Power="35"/>"#
    ));
}

#[test]
fn metadata_tools_project_generic_filter_and_dichroic_metadata_to_ome() {
    let mut meta = populated_meta();
    meta.series_metadata.insert(
        "lof.filter.0.name".into(),
        MetadataValue::String("DAPI emission".into()),
    );
    meta.series_metadata.insert(
        "lof.filter.0.manufacturer".into(),
        MetadataValue::String("Chroma".into()),
    );
    meta.series_metadata.insert(
        "lof.filter.0.type".into(),
        MetadataValue::String("Emission".into()),
    );
    meta.series_metadata
        .insert("lof.filter.0.cut_in".into(), MetadataValue::Float(430.0));
    meta.series_metadata
        .insert("lof.filter.0.cut_out".into(), MetadataValue::Float(480.0));
    meta.series_metadata.insert(
        "lof.dichroic.0.name".into(),
        MetadataValue::String("405/488/561".into()),
    );
    meta.series_metadata.insert(
        "lof.dichroic.0.manufacturer".into(),
        MetadataValue::String("Leica".into()),
    );
    meta.series_metadata.insert(
        "lof.detector.0.manufacturer".into(),
        MetadataValue::String("Hamamatsu".into()),
    );
    meta.series_metadata
        .insert("lof.detector.0.offset".into(), MetadataValue::Float(-12.5));

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert_eq!(ome.images[0].instrument_ref, Some(0));
    assert_eq!(ome.instruments.len(), 1);
    let instrument = &ome.instruments[0];
    assert_eq!(instrument.filters.len(), 1);
    assert_eq!(instrument.filters[0].id.as_deref(), Some("Filter:0:0"));
    assert_eq!(
        instrument.filters[0].model.as_deref(),
        Some("DAPI emission")
    );
    assert_eq!(
        instrument.filters[0].manufacturer.as_deref(),
        Some("Chroma")
    );
    assert_eq!(
        instrument.filters[0].filter_type.as_deref(),
        Some("Emission")
    );
    assert_eq!(instrument.filters[0].cut_in, Some(430.0));
    assert_eq!(instrument.filters[0].cut_out, Some(480.0));
    assert_eq!(instrument.dichroics.len(), 1);
    assert_eq!(instrument.dichroics[0].id.as_deref(), Some("Dichroic:0:0"));
    assert_eq!(
        instrument.dichroics[0].model.as_deref(),
        Some("405/488/561")
    );
    assert_eq!(
        instrument.dichroics[0].manufacturer.as_deref(),
        Some("Leica")
    );
    assert_eq!(instrument.detectors.len(), 1);
    assert_eq!(
        instrument.detectors[0].manufacturer.as_deref(),
        Some("Hamamatsu")
    );
    assert_eq!(instrument.detectors[0].offset, Some(-12.5));

    let xml = ome.to_ome_xml(&meta);
    assert!(
        xml.contains(r#"<Detector ID="Detector:0:0" Manufacturer="Hamamatsu" Offset="-12.5"/>"#)
    );
    assert!(xml.contains(
        r#"<Filter ID="Filter:0:0" Model="DAPI emission" Manufacturer="Chroma" Type="Emission" CutIn="430" CutOut="480"/>"#
    ));
    assert!(
        xml.contains(r#"<Dichroic ID="Dichroic:0:0" Model="405/488/561" Manufacturer="Leica"/>"#)
    );
}

#[test]
fn metadata_tools_project_generic_channel_light_paths_to_ome() {
    let mut meta = populated_meta();
    meta.series_metadata.insert(
        "reader.channel.1.excitation_filter_id".into(),
        MetadataValue::String("0,Filter:0:2".into()),
    );
    meta.series_metadata.insert(
        "reader.channel.1.dichroic_id".into(),
        MetadataValue::String("0".into()),
    );
    meta.series_metadata.insert(
        "reader.channel.1.emission_filter_id".into(),
        MetadataValue::String("EmissionFilter:custom".into()),
    );

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert_eq!(ome.images[0].light_paths.len(), 2);
    assert!(ome.images[0].light_paths[0]
        .excitation_filter_ids
        .is_empty());
    let path = &ome.images[0].light_paths[1];
    assert_eq!(
        path.excitation_filter_ids,
        vec!["Filter:0:0".to_string(), "Filter:0:2".to_string()]
    );
    assert_eq!(path.dichroic_id.as_deref(), Some("Dichroic:0:0"));
    assert_eq!(
        path.emission_filter_ids,
        vec!["EmissionFilter:custom".to_string()]
    );

    let xml = ome.to_ome_xml(&meta);
    assert!(xml.contains(r#"<Channel ID="Channel:0:1" SamplesPerPixel="1"><LightPath><ExcitationFilterRef ID="Filter:0:0"/><ExcitationFilterRef ID="Filter:0:2"/><DichroicRef ID="Dichroic:0:0"/><EmissionFilterRef ID="EmissionFilter:custom"/></LightPath></Channel>"#));
}

#[test]
fn metadata_tools_project_generic_experimenter_metadata_to_ome() {
    let mut meta = populated_meta();
    meta.series_metadata.insert(
        "cellSens.experimenter.first_name".into(),
        MetadataValue::String("Ada".into()),
    );
    meta.series_metadata.insert(
        "cellSens.experimenter.last_name".into(),
        MetadataValue::String("Lovelace".into()),
    );
    meta.series_metadata.insert(
        "cellSens.experimenter.email".into(),
        MetadataValue::String("ada@example.org".into()),
    );
    meta.series_metadata.insert(
        "cellSens.experimenter.institution".into(),
        MetadataValue::String("Analytical Engine Lab".into()),
    );

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert_eq!(ome.experimenters.len(), 1);
    let experimenter = &ome.experimenters[0];
    assert_eq!(experimenter.id.as_deref(), Some("Experimenter:0"));
    assert_eq!(experimenter.first_name.as_deref(), Some("Ada"));
    assert_eq!(experimenter.last_name.as_deref(), Some("Lovelace"));
    assert_eq!(experimenter.email.as_deref(), Some("ada@example.org"));
    assert_eq!(
        experimenter.institution.as_deref(),
        Some("Analytical Engine Lab")
    );

    let xml = ome.to_ome_xml(&meta);
    assert!(xml.contains(
        r#"<Experimenter ID="Experimenter:0" FirstName="Ada" LastName="Lovelace" Email="ada@example.org" Institution="Analytical Engine Lab"/>"#
    ));
}

#[test]
fn metadata_tools_project_generic_acquisition_date_to_ome() {
    let mut meta = populated_meta();
    meta.series_metadata.insert(
        "tillvision.acquisition_datetime_iso8601".into(),
        MetadataValue::String("2026-05-26T09:10:11".into()),
    );

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert_eq!(
        ome.images[0].acquisition_date.as_deref(),
        Some("2026-05-26T09:10:11")
    );
    let xml = ome.to_ome_xml(&meta);
    assert!(xml.contains("<AcquisitionDate>2026-05-26T09:10:11</AcquisitionDate>"));

    let parsed = OmeMetadata::from_ome_xml(&xml);
    assert_eq!(
        parsed.images[0].acquisition_date.as_deref(),
        Some("2026-05-26T09:10:11")
    );
}

#[test]
fn metadata_tools_project_generic_image_identity_to_ome() {
    let mut meta = populated_meta();
    meta.series_metadata.insert(
        "reader.series_name".into(),
        MetadataValue::String("Embryo position 3".into()),
    );
    meta.series_metadata.insert(
        "reader.image_description".into(),
        MetadataValue::String("first & second".into()),
    );

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert_eq!(ome.images[0].name.as_deref(), Some("Embryo position 3"));
    assert_eq!(ome.images[0].description.as_deref(), Some("first & second"));
    let xml = ome.to_ome_xml(&meta);
    assert!(xml.contains(r#"<Image ID="Image:0" Name="Embryo position 3">"#));
    assert!(xml.contains("<Description>first &amp; second</Description>"));
}

#[test]
fn metadata_tools_project_generic_plane_metadata_to_ome() {
    let mut meta = populated_meta();
    meta.series_metadata
        .insert("nis.plane.1.delta_t".into(), MetadataValue::Float(3.5));
    meta.series_metadata.insert(
        "nis.plane.1.exposure_time".into(),
        MetadataValue::Float(0.125),
    );
    meta.series_metadata
        .insert("nis.plane.1.stage_x".into(), MetadataValue::Float(12.0));
    meta.series_metadata
        .insert("nis.plane.1.stage_y".into(), MetadataValue::Float(-4.0));
    meta.series_metadata
        .insert("nis.plane.1.stage_z".into(), MetadataValue::Float(7.25));

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert_eq!(ome.images[0].planes.len(), 1);
    let plane = &ome.images[0].planes[0];
    assert_eq!(plane.the_z, 1);
    assert_eq!(plane.the_c, 0);
    assert_eq!(plane.the_t, 0);
    assert_eq!(plane.delta_t, Some(3.5));
    assert_eq!(plane.exposure_time, Some(0.125));
    assert_eq!(plane.position_x, Some(12.0));
    assert_eq!(plane.position_y, Some(-4.0));
    assert_eq!(plane.position_z, Some(7.25));

    let xml = ome.to_ome_xml(&meta);
    assert!(xml.contains(
        r#"<Plane TheZ="1" TheC="0" TheT="0" DeltaT="3.5" ExposureTime="0.125" PositionX="12" PositionY="-4" PositionZ="7.25"/>"#
    ));
}

#[test]
fn metadata_tools_project_generic_roi_metadata_to_ome() {
    let mut meta = populated_meta();
    meta.series_metadata.insert(
        "reader.roi.0.name".into(),
        MetadataValue::String("Cell box".into()),
    );
    meta.series_metadata
        .insert("reader.roi.0.x".into(), MetadataValue::Float(10.0));
    meta.series_metadata
        .insert("reader.roi.0.y".into(), MetadataValue::Float(20.0));
    meta.series_metadata
        .insert("reader.roi.0.width".into(), MetadataValue::Float(30.0));
    meta.series_metadata
        .insert("reader.roi.0.height".into(), MetadataValue::Float(40.0));
    meta.series_metadata
        .insert("reader.roi.0.the_c".into(), MetadataValue::Int(1));
    meta.series_metadata.insert(
        "reader.roi.2.label".into(),
        MetadataValue::String("Centroid".into()),
    );
    meta.series_metadata
        .insert("reader.roi.2.center_x".into(), MetadataValue::Float(5.5));
    meta.series_metadata
        .insert("reader.roi.2.center_y".into(), MetadataValue::Float(6.5));
    meta.series_metadata.insert(
        "reader.roi.2.the_t".into(),
        MetadataValue::String("3".into()),
    );
    meta.series_metadata.insert(
        "reader.roi.3.name".into(),
        MetadataValue::String("Nucleus".into()),
    );
    meta.series_metadata
        .insert("reader.roi.3.center_x".into(), MetadataValue::Float(12.0));
    meta.series_metadata
        .insert("reader.roi.3.center_y".into(), MetadataValue::Float(14.0));
    meta.series_metadata
        .insert("reader.roi.3.radius_x".into(), MetadataValue::Float(6.0));
    meta.series_metadata
        .insert("reader.roi.3.radius_y".into(), MetadataValue::Float(4.0));
    meta.series_metadata
        .insert("reader.roi.3.the_z".into(), MetadataValue::Int(2));
    meta.series_metadata.insert(
        "reader.roi.4.label".into(),
        MetadataValue::String("Track".into()),
    );
    meta.series_metadata
        .insert("reader.roi.4.x1".into(), MetadataValue::Float(1.0));
    meta.series_metadata
        .insert("reader.roi.4.y1".into(), MetadataValue::Float(2.0));
    meta.series_metadata
        .insert("reader.roi.4.x2".into(), MetadataValue::Float(8.0));
    meta.series_metadata
        .insert("reader.roi.4.y2".into(), MetadataValue::Float(9.0));
    meta.series_metadata
        .insert("reader.roi.4.the_c".into(), MetadataValue::Int(2));

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert_eq!(ome.rois.len(), 4);
    assert_eq!(ome.rois[0].id.as_deref(), Some("ROI:0"));
    assert_eq!(ome.rois[0].name.as_deref(), Some("Cell box"));
    match &ome.rois[0].shapes[0] {
        OmeShape::Rectangle {
            x,
            y,
            width,
            height,
            the_c,
            ..
        } => {
            assert_eq!((*x, *y, *width, *height), (10.0, 20.0, 30.0, 40.0));
            assert_eq!(*the_c, Some(1));
        }
        other => panic!("expected rectangle, got {other:?}"),
    }
    assert_eq!(ome.rois[1].id.as_deref(), Some("ROI:2"));
    assert_eq!(ome.rois[1].name.as_deref(), Some("Centroid"));
    match &ome.rois[1].shapes[0] {
        OmeShape::Point { x, y, the_t, .. } => {
            assert_eq!((*x, *y), (5.5, 6.5));
            assert_eq!(*the_t, Some(3));
        }
        other => panic!("expected point, got {other:?}"),
    }
    assert_eq!(ome.rois[2].id.as_deref(), Some("ROI:3"));
    assert_eq!(ome.rois[2].name.as_deref(), Some("Nucleus"));
    match &ome.rois[2].shapes[0] {
        OmeShape::Ellipse {
            x,
            y,
            radius_x,
            radius_y,
            the_z,
            ..
        } => {
            assert_eq!((*x, *y, *radius_x, *radius_y), (12.0, 14.0, 6.0, 4.0));
            assert_eq!(*the_z, Some(2));
        }
        other => panic!("expected ellipse, got {other:?}"),
    }
    assert_eq!(ome.rois[3].id.as_deref(), Some("ROI:4"));
    assert_eq!(ome.rois[3].name.as_deref(), Some("Track"));
    match &ome.rois[3].shapes[0] {
        OmeShape::Line {
            x1,
            y1,
            x2,
            y2,
            the_c,
            ..
        } => {
            assert_eq!((*x1, *y1, *x2, *y2), (1.0, 2.0, 8.0, 9.0));
            assert_eq!(*the_c, Some(2));
        }
        other => panic!("expected line, got {other:?}"),
    }

    let xml = ome.to_ome_xml(&meta);
    assert!(xml.contains(r#"<ROI ID="ROI:0" Name="Cell box"><Union><Rectangle X="10" Y="20" Width="30" Height="40" TheC="1"/></Union></ROI>"#));
    assert!(xml.contains(
        r#"<ROI ID="ROI:2" Name="Centroid"><Union><Point X="5.5" Y="6.5" TheT="3"/></Union></ROI>"#
    ));
    assert!(xml.contains(
        r#"<ROI ID="ROI:3" Name="Nucleus"><Union><Ellipse X="12" Y="14" RadiusX="6" RadiusY="4" TheZ="2"/></Union></ROI>"#
    ));
    assert!(xml.contains(
        r#"<ROI ID="ROI:4" Name="Track"><Union><Line X1="1" Y1="2" X2="8" Y2="9" TheC="2"/></Union></ROI>"#
    ));
}

#[test]
fn metadata_tools_ignore_invalid_objective_magnification() {
    let mut meta = populated_meta();
    meta.series_metadata
        .insert("objective.magnification".into(), MetadataValue::Float(0.0));

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert!(ome.instruments.is_empty());
    assert_eq!(ome.images[0].instrument_ref, None);
    assert_eq!(ome.images[0].objective_ref, None);
}

#[test]
fn metadata_tools_ignore_invalid_generic_numeric_metadata() {
    let mut meta = populated_meta();
    meta.series_metadata.insert(
        "reader.objective.numerical_aperture".into(),
        MetadataValue::Float(0.0),
    );
    meta.series_metadata.insert(
        "reader.channel.0.excitation_wavelength".into(),
        MetadataValue::Float(f64::NAN),
    );

    let ome = OmeMetadata::from_image_metadata(&meta);

    assert!(ome.instruments.is_empty());
    assert_eq!(ome.images[0].channels[0].excitation_wavelength, None);
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
                Some("openmicroscopy.org/OriginalMetadata")
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
