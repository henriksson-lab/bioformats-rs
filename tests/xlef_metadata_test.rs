use bioformats::common::metadata::MetadataValue;
use bioformats::formats::flim2::XlefReader;
use bioformats::{FormatReader, OmeAnnotation};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_path(name: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("bioformats_xlef_metadata_{name}_{nonce}"))
}

fn assert_float(meta: &std::collections::HashMap<String, MetadataValue>, key: &str, value: f64) {
    match meta.get(key) {
        Some(MetadataValue::Float(actual)) => assert!(
            (actual - value).abs() < 1e-9,
            "{key}: expected {value}, got {actual}"
        ),
        other => panic!("{key}: expected float {value}, got {other:?}"),
    }
}

fn assert_int(meta: &std::collections::HashMap<String, MetadataValue>, key: &str, value: i64) {
    match meta.get(key) {
        Some(MetadataValue::Int(actual)) => assert_eq!(*actual, value, "{key}"),
        other => panic!("{key}: expected int {value}, got {other:?}"),
    }
}

fn assert_string(
    meta: &std::collections::HashMap<String, MetadataValue>,
    key: &str,
    expected: &str,
) {
    match meta.get(key) {
        Some(MetadataValue::String(actual)) => assert_eq!(actual, expected, "{key}"),
        other => panic!("{key}: expected string {expected:?}, got {other:?}"),
    }
}

fn write_one_pixel_bmp(path: &std::path::Path, red: u8, green: u8, blue: u8) {
    let mut data = Vec::new();
    data.extend_from_slice(b"BM");
    data.extend_from_slice(&58u32.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&54u32.to_le_bytes());
    data.extend_from_slice(&40u32.to_le_bytes());
    data.extend_from_slice(&1i32.to_le_bytes());
    data.extend_from_slice(&1i32.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&24u16.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&4u32.to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&0i32.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&[blue, green, red, 0]);
    std::fs::write(path, data).unwrap();
}

#[test]
fn xlef_lms_metadata_only_series_projects_safe_scalars_to_ome() {
    let xlef = temp_path("project.xlef");
    let lms = xlef.with_extension("lms");
    std::fs::write(
        &lms,
        r#"<XLIF><Element Name="Experiment 42"><Data><Image Name="Scan A" ID="img-1" Description="Bounded LMS metadata">
<ImageDescription>
<Channels>
<ChannelDescription Name="DAPI" Resolution="16" ExcitationWavelength="405" EmissionWavelength="460" Pinhole="1.2"/>
<ChannelDescription DyeName="FITC" Resolution="16" ExcitationWavelength="488" EmissionWavelength="525"/>
</Channels>
<Dimensions>
<DimensionDescription DimID="1" NumberOfElements="5" Length="8" Unit="um"/>
<DimensionDescription DimID="2" NumberOfElements="3" Length="4" Unit="um"/>
<DimensionDescription DimID="3" NumberOfElements="2" Length="3" Unit="um"/>
</Dimensions>
<Instrument>
<ObjectiveDescription Name="HC PL APO 63x" Magnification="63" NumericalAperture="1.4" Immersion="Oil"/>
<DetectorDescription Name="HyD S" Type="HyD" Gain="120"/>
<LaserDescription Name="White Light Laser" Wavelength="488" Power="12.5"/>
</Instrument>
<ROIs><ROI ID="roi-1" Name="Cell boundary" X="1.5" Y="2.5"/></ROIs>
</ImageDescription>
</Image></Data></Element></XLIF>"#,
    )
    .unwrap();
    std::fs::write(
        &xlef,
        format!(
            r#"<XLEF><Image File="{}"/></XLEF>"#,
            lms.file_name().unwrap().to_string_lossy()
        ),
    )
    .unwrap();

    let mut reader = XlefReader::new();
    reader.set_id(&xlef).unwrap();
    assert_eq!(reader.series_count(), 1);

    let meta = reader.metadata();
    assert_eq!(meta.size_x, 5);
    assert_eq!(meta.size_y, 3);
    assert_eq!(meta.size_z, 2);
    assert_eq!(meta.size_c, 2);
    assert_float(&meta.series_metadata, "xlef.lms.physical_size_x", 2.0);
    assert_float(&meta.series_metadata, "xlef.lms.physical_size_y", 2.0);
    assert_float(&meta.series_metadata, "xlef.lms.physical_size_z", 3.0);
    assert_float(
        &meta.series_metadata,
        "xlef.lms.channel.0.excitation_wavelength",
        405.0,
    );
    assert_float(
        &meta.series_metadata,
        "xlef.lms.channel.1.emission_wavelength",
        525.0,
    );
    assert!(matches!(
        meta.series_metadata.get("xlef.lms.channel.0.name"),
        Some(MetadataValue::String(name)) if name == "DAPI"
    ));
    assert!(matches!(
        meta.series_metadata.get("xlef.lms.channel.1.dye_name"),
        Some(MetadataValue::String(name)) if name == "FITC"
    ));
    assert_string(
        &meta.series_metadata,
        "xlef.lms.description",
        "Bounded LMS metadata",
    );
    assert_int(&meta.series_metadata, "xlef.lms.graph.objective_count", 1);
    assert_int(&meta.series_metadata, "xlef.lms.graph.detector_count", 1);
    assert_int(&meta.series_metadata, "xlef.lms.graph.laser_count", 1);
    assert_int(&meta.series_metadata, "xlef.lms.graph.roi_count", 1);
    assert_string(
        &meta.series_metadata,
        "xlef.lms.objective.0.name",
        "HC PL APO 63x",
    );
    assert_float(
        &meta.series_metadata,
        "xlef.lms.objective.0.numerical_aperture",
        1.4,
    );
    assert_string(&meta.series_metadata, "xlef.lms.detector.0.type", "HyD");
    assert_float(&meta.series_metadata, "xlef.lms.laser.0.wavelength", 488.0);
    assert_string(&meta.series_metadata, "xlef.lms.roi.0.id", "roi-1");
    assert_float(&meta.series_metadata, "xlef.lms.roi.0.x", 1.5);

    let ome = reader.ome_metadata().expect("OME metadata");
    let image = ome.images.first().expect("OME image");
    assert_eq!(image.name.as_deref(), Some("Scan A"));
    assert_eq!(image.description.as_deref(), Some("Bounded LMS metadata"));
    assert_eq!(image.physical_size_x, Some(2.0));
    assert_eq!(image.physical_size_y, Some(2.0));
    assert_eq!(image.physical_size_z, Some(3.0));
    assert_eq!(image.channels.len(), 2);
    assert_eq!(image.channels[0].name.as_deref(), Some("DAPI"));
    assert_eq!(image.channels[0].excitation_wavelength, Some(405.0));
    assert_eq!(image.channels[0].emission_wavelength, Some(460.0));
    assert_eq!(image.channels[1].name.as_deref(), Some("FITC"));
    assert_eq!(image.channels[1].excitation_wavelength, Some(488.0));
    assert_eq!(image.channels[1].emission_wavelength, Some(525.0));
    let annotation = ome
        .annotations
        .iter()
        .find_map(|annotation| match annotation {
            OmeAnnotation::MapAnnotation {
                id,
                namespace,
                values,
            } if id.as_deref() == Some("Annotation:OriginalMetadata:0")
                && namespace.as_deref()
                    == Some("openmicroscopy.org/bioformats/original-metadata") =>
            {
                Some(values)
            }
            _ => None,
        })
        .expect("LMS original metadata annotation");
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "xlef.lms.description" && value == "Bounded LMS metadata"));
    assert!(annotation.iter().any(|(key, value)| {
        key == "xlef.lms.channel.0.excitation_wavelength" && value == "405"
    }));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "xlef.lms.path" && value.ends_with(".lms")));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "xlef.lms.objective.0.name" && value == "HC PL APO 63x"));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "xlef.lms.detector.0.type" && value == "HyD"));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "xlef.lms.graph.roi_count" && value == "1"));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, bioformats::BioFormatsError::UnsupportedFormat(message) if message.contains("LMS metadata series has no pixel delegate"))
    );

    let _ = std::fs::remove_file(xlef);
    let _ = std::fs::remove_file(lms);
}

#[test]
fn xlef_mixed_project_adds_project_grouping_metadata_to_each_series() {
    let xlef = temp_path("mixed_project.xlef");
    let bmp = xlef.with_extension("bmp");
    let lms = xlef.with_extension("lms");
    write_one_pixel_bmp(&bmp, 10, 20, 30);
    std::fs::write(
        &lms,
        r#"<XLIF><Element Name="metadata only"><Data><Image Name="LMS scan">
<ImageDescription><Dimensions>
<DimensionDescription DimID="1" NumberOfElements="2"/>
<DimensionDescription DimID="2" NumberOfElements="3"/>
</Dimensions></ImageDescription>
</Image></Data></Element></XLIF>"#,
    )
    .unwrap();
    std::fs::write(
        &xlef,
        format!(
            r#"<XLEF><Image File="{}"/><Image File="{}"/></XLEF>"#,
            bmp.file_name().unwrap().to_string_lossy(),
            lms.file_name().unwrap().to_string_lossy()
        ),
    )
    .unwrap();

    let mut reader = XlefReader::new();
    reader.set_id(&xlef).unwrap();
    assert_eq!(reader.series_count(), 2);

    let meta = reader.metadata();
    assert_int(&meta.series_metadata, "xlef.project.series_index", 0);
    assert_int(&meta.series_metadata, "xlef.project.series_count", 2);
    assert_string(
        &meta.series_metadata,
        "xlef.project.source_kind",
        "pixel_delegate",
    );
    assert!(matches!(
        meta.series_metadata.get("xlef.project.source_path"),
        Some(MetadataValue::String(path)) if path.ends_with(bmp.file_name().unwrap().to_string_lossy().as_ref())
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 20, 30]);

    reader.set_series(1).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 3);
    assert_int(&meta.series_metadata, "xlef.project.series_index", 1);
    assert_int(&meta.series_metadata, "xlef.project.series_count", 2);
    assert_string(
        &meta.series_metadata,
        "xlef.project.source_kind",
        "lms_metadata",
    );
    assert_string(
        &meta.series_metadata,
        "xlef.lms.element.name",
        "metadata only",
    );

    let _ = std::fs::remove_file(xlef);
    let _ = std::fs::remove_file(bmp);
    let _ = std::fs::remove_file(lms);
}

#[test]
fn xlef_mixed_project_rejects_unsupported_attribute_leaf_before_partial_open() {
    let xlef = temp_path("unsupported_mixed.xlef");
    let bmp = xlef.with_extension("bmp");
    write_one_pixel_bmp(&bmp, 1, 2, 3);
    std::fs::write(
        &xlef,
        format!(
            r#"<XLEF><Image File="{}"/><Image File="unsupported.dat"/></XLEF>"#,
            bmp.file_name().unwrap().to_string_lossy()
        ),
    )
    .unwrap();

    let err = XlefReader::new().set_id(&xlef).unwrap_err();
    assert!(
        matches!(err, bioformats::BioFormatsError::UnsupportedFormat(ref message)
            if message.contains("mixes supported leaves with unsupported files")
                && message.contains("unsupported.dat")),
        "unexpected error: {err}"
    );

    let _ = std::fs::remove_file(xlef);
    let _ = std::fs::remove_file(bmp);
}
