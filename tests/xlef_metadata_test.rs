use bioformats::common::metadata::MetadataValue;
use bioformats::common::pixel_type::PixelType;
use bioformats::formats::flim2::XlefReader;
use bioformats::formats::leica_lms::{image_metadata_from_xlif, XlifDocument};
use bioformats::{FormatReader, OmeAnnotation, OmeShape};
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
fn xlef_lms_four_byte_x_stride_maps_to_float_like_java() {
    let lms = temp_path("float32_image.xlif");
    std::fs::write(
        &lms,
        r#"<XLIF><Element Name="Float scan"><Data><Image Name="Float Image">
<ImageDescription>
<Channels><ChannelDescription BytesInc="0"/></Channels>
<Dimensions>
<DimensionDescription DimID="1" NumberOfElements="2" BytesInc="4"/>
<DimensionDescription DimID="2" NumberOfElements="2" BytesInc="8"/>
</Dimensions>
</ImageDescription>
</Image></Data></Element></XLIF>"#,
    )
    .unwrap();

    let xlif = XlifDocument::new(&lms).unwrap();
    let meta = image_metadata_from_xlif(&xlif).unwrap();
    assert_eq!(meta.pixel_type, PixelType::Float32);
    assert_eq!(meta.bits_per_pixel, 32);

    let _ = std::fs::remove_file(lms);
}

#[test]
fn xlef_lms_metadata_only_series_projects_safe_scalars_to_ome() {
    let xlef = temp_path("project.xlef");
    let lms = xlef.with_extension("lms");
    std::fs::write(
        &lms,
        r##"<XLIF><Element Name="Experiment 42"><Data><Image Name="Scan A" ID="img-1" Description="Bounded LMS metadata">
<ImageDescription>
<Channels>
<ChannelDescription Name="DAPI" Resolution="16" ExcitationWavelength="405" EmissionWavelength="460" Pinhole="1.2" Color="#336699"/>
<ChannelDescription DyeName="FITC" Resolution="16" ExcitationWavelength="488" EmissionWavelength="525" ColorRGB="1,2,3"/>
</Channels>
<Dimensions>
<DimensionDescription DimID="1" NumberOfElements="5" Length="8" Unit="um"/>
<DimensionDescription DimID="2" NumberOfElements="3" Length="4" Unit="um"/>
<DimensionDescription DimID="3" NumberOfElements="2" Length="3" Unit="um"/>
</Dimensions>
<Instrument>
<Microscope Name="SP8" Manufacturer="Leica Microsystems"/>
<ObjectiveDescription Name="HC PL APO 63x" Magnification="63" CalibratedMagnification="62.8" NumericalAperture="1.4" Immersion="Oil" WorkingDistance="140"/>
<DetectorDescription Name="HyD S" Type="HyD" Gain="120" Offset="4.5"/>
<LaserDescription Name="White Light Laser" Wavelength="488" Power="12.5" Manufacturer="Leica Microsystems"/>
<FilterDescription Name="FITC emission" FilterType="BandPass" CutIn="500" CutOut="550"/>
<DichroicDescription Name="488 main dichroic" Manufacturer="Leica Microsystems"/>
</Instrument>
<ROIs><ROI ID="roi-1" Name="Cell boundary" X="1.5" Y="2.5" Width="10" Height="11"/></ROIs>
</ImageDescription>
</Image></Data></Element></XLIF>"##,
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
    assert_string(&meta.series_metadata, "xlef.lms.channel.0.color", "#336699");
    assert_int(
        &meta.series_metadata,
        "xlef.lms.channel.0.ome_color",
        862362111,
    );
    assert_string(&meta.series_metadata, "xlef.lms.channel.1.color", "1,2,3");
    assert_int(
        &meta.series_metadata,
        "xlef.lms.channel.1.ome_color",
        16909311,
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
    assert_int(&meta.series_metadata, "xlef.lms.graph.microscope_count", 1);
    assert_int(&meta.series_metadata, "xlef.lms.graph.filter_count", 1);
    assert_int(&meta.series_metadata, "xlef.lms.graph.dichroic_count", 1);
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
    assert_float(
        &meta.series_metadata,
        "xlef.lms.objective.0.working_distance",
        140.0,
    );
    assert_string(&meta.series_metadata, "xlef.lms.detector.0.type", "HyD");
    assert_float(&meta.series_metadata, "xlef.lms.detector.0.gain", 120.0);
    assert_float(&meta.series_metadata, "xlef.lms.detector.0.offset", 4.5);
    assert_float(&meta.series_metadata, "xlef.lms.laser.0.wavelength", 488.0);
    assert_float(&meta.series_metadata, "xlef.lms.laser.0.power", 12.5);
    assert_string(&meta.series_metadata, "xlef.lms.microscope.0.name", "SP8");
    assert_string(
        &meta.series_metadata,
        "xlef.lms.filter.0.filter_type",
        "BandPass",
    );
    assert_float(&meta.series_metadata, "xlef.lms.filter.0.cut_in", 500.0);
    assert_string(
        &meta.series_metadata,
        "xlef.lms.dichroic.0.name",
        "488 main dichroic",
    );
    assert_string(&meta.series_metadata, "xlef.lms.roi.0.id", "roi-1");
    assert_float(&meta.series_metadata, "xlef.lms.roi.0.x", 1.5);
    assert_float(&meta.series_metadata, "xlef.lms.roi.0.width", 10.0);

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
    assert_eq!(image.channels[0].color, Some(862362111));
    assert_eq!(image.channels[1].name.as_deref(), Some("FITC"));
    assert_eq!(image.channels[1].excitation_wavelength, Some(488.0));
    assert_eq!(image.channels[1].emission_wavelength, Some(525.0));
    assert_eq!(image.channels[1].color, Some(16909311));
    assert_eq!(image.instrument_ref, Some(0));
    assert_eq!(image.objective_ref, Some(0));
    assert_eq!(ome.instruments.len(), 1);
    let instrument = &ome.instruments[0];
    assert_eq!(instrument.microscope_model.as_deref(), Some("SP8"));
    assert_eq!(
        instrument.microscope_manufacturer.as_deref(),
        Some("Leica Microsystems")
    );
    assert_eq!(instrument.objectives.len(), 1);
    assert_eq!(
        instrument.objectives[0].model.as_deref(),
        Some("HC PL APO 63x")
    );
    assert_eq!(instrument.objectives[0].nominal_magnification, Some(63.0));
    assert_eq!(
        instrument.objectives[0].calibrated_magnification,
        Some(62.8)
    );
    assert_eq!(instrument.objectives[0].lens_na, Some(1.4));
    assert_eq!(instrument.objectives[0].immersion.as_deref(), Some("Oil"));
    assert_eq!(instrument.objectives[0].working_distance, Some(140.0));
    assert_eq!(instrument.detectors.len(), 1);
    assert_eq!(instrument.detectors[0].model.as_deref(), Some("HyD S"));
    assert_eq!(
        instrument.detectors[0].detector_type.as_deref(),
        Some("HyD")
    );
    assert_eq!(instrument.detectors[0].gain, Some(120.0));
    assert_eq!(instrument.detectors[0].offset, Some(4.5));
    assert_eq!(instrument.light_sources.len(), 1);
    assert_eq!(
        instrument.light_sources[0].model.as_deref(),
        Some("White Light Laser")
    );
    assert_eq!(
        instrument.light_sources[0].manufacturer.as_deref(),
        Some("Leica Microsystems")
    );
    assert_eq!(instrument.light_sources[0].power, Some(12.5));
    assert_eq!(instrument.filters.len(), 1);
    assert_eq!(
        instrument.filters[0].model.as_deref(),
        Some("FITC emission")
    );
    assert_eq!(
        instrument.filters[0].filter_type.as_deref(),
        Some("BandPass")
    );
    assert_eq!(instrument.filters[0].cut_in, Some(500.0));
    assert_eq!(instrument.filters[0].cut_out, Some(550.0));
    assert_eq!(instrument.dichroics.len(), 1);
    assert_eq!(
        instrument.dichroics[0].model.as_deref(),
        Some("488 main dichroic")
    );
    assert_eq!(ome.rois.len(), 1);
    assert_eq!(ome.rois[0].id.as_deref(), Some("roi-1"));
    assert_eq!(ome.rois[0].name.as_deref(), Some("Cell boundary"));
    assert!(matches!(
        ome.rois[0].shapes.first(),
        Some(OmeShape::Rectangle {
            x,
            y,
            width,
            height,
            ..
        }) if (*x, *y, *width, *height) == (1.5, 2.5, 10.0, 11.0)
    ));
    let annotation = ome
        .annotations
        .iter()
        .find_map(|annotation| match annotation {
            OmeAnnotation::MapAnnotation {
                id,
                namespace,
                values,
            } if id.as_deref() == Some("Annotation:OriginalMetadata:0")
                && namespace.as_deref() == Some("openmicroscopy.org/OriginalMetadata") =>
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
        .any(|(key, value)| key == "xlef.lms.channel.0.ome_color" && value == "862362111"));
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
        .any(|(key, value)| key == "xlef.lms.filter.0.cut_out" && value == "550"));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "xlef.lms.roi.0.width" && value == "10"));
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
fn xlef_lms_metadata_only_series_reports_unsupported_pixel_layout_diagnostics() {
    let xlef = temp_path("pixel_layout_project.xlef");
    let lms = xlef.with_extension("lms");
    std::fs::write(
        &lms,
        r#"<LMSDataContainerHeader><Element Name="pixel layout"><Memory MemoryBlockID="Mem1" Compression="zlib"/><Data><Image Name="layout scan">
<ImageDescription>
<Channels><ChannelDescription Name="DAPI" Resolution="8" BytesInc="0"/></Channels>
<Dimensions>
<DimensionDescription DimID="1" NumberOfElements="4" BytesInc="1"/>
<DimensionDescription DimID="2" NumberOfElements="3" BytesInc="4"/>
</Dimensions>
<Storage FileName="pixels.bin"/>
</ImageDescription>
</Image></Data></Element></LMSDataContainerHeader>"#,
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
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 4);
    assert_eq!(meta.size_y, 3);
    assert_float(&meta.series_metadata, "xlef.lms.channel.0.bytes_inc", 0.0);
    assert_int(&meta.series_metadata, "xlef.lms.dimension.1.bytes_inc", 1);
    assert_int(&meta.series_metadata, "xlef.lms.dimension.2.bytes_inc", 4);
    assert_string(
        &meta.series_metadata,
        "xlef.lms.pixel_payload",
        "declared_unsupported",
    );
    assert_string(
        &meta.series_metadata,
        "xlef.lms.pixel_layout.status",
        "declared_unsupported",
    );
    assert_int(
        &meta.series_metadata,
        "xlef.lms.pixel_layout.channel_bytes_inc_count",
        1,
    );
    assert_int(
        &meta.series_metadata,
        "xlef.lms.pixel_layout.dimension_bytes_inc_count",
        2,
    );
    assert_int(
        &meta.series_metadata,
        "xlef.lms.pixel_layout.memory_count",
        1,
    );
    assert_int(
        &meta.series_metadata,
        "xlef.lms.pixel_layout.storage_count",
        1,
    );
    assert_string(
        &meta.series_metadata,
        "xlef.lms.pixel_layout.compression",
        "zlib",
    );
    assert_string(
        &meta.series_metadata,
        "xlef.lms.pixel_layout.memory_block_id",
        "Mem1",
    );
    assert_string(
        &meta.series_metadata,
        "xlef.lms.pixel_layout.storage_reference",
        "pixels.bin",
    );

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, bioformats::BioFormatsError::UnsupportedFormat(ref message)
            if message.contains("LMS metadata series has no pixel delegate")
                && message.contains("unsupported LMS pixel layout declared by")
                && message.contains("ChannelDescription BytesInc")
                && message.contains("DimensionDescription BytesInc")
                && message.contains("memory nodes")
                && message.contains("storage nodes")),
        "unexpected error: {err}"
    );

    let _ = std::fs::remove_file(xlef);
    let _ = std::fs::remove_file(lms);
}

#[test]
fn xlef_lms_raw_storage_leaf_reads_tightly_packed_xy_pixels() {
    let xlef = temp_path("raw_storage_project.xlef");
    let lms = xlef.with_extension("lms");
    let raw = xlef.with_file_name("pixel data.raw");
    let pixels = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    std::fs::write(&raw, &pixels).unwrap();
    std::fs::write(
        &lms,
        r#"<LMSDataContainerHeader><Element Name="raw storage"><Data><Image Name="raw scan">
<ImageDescription>
<Channels><ChannelDescription Name="DAPI" Resolution="8" BytesInc="0"/></Channels>
<Dimensions>
<DimensionDescription DimID="1" NumberOfElements="4" BytesInc="1"/>
<DimensionDescription DimID="2" NumberOfElements="3" BytesInc="4"/>
</Dimensions>
<Storage FileName="pixel%20data.raw"/>
</ImageDescription>
</Image></Data></Element></LMSDataContainerHeader>"#,
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
    let meta = reader.metadata();
    assert_eq!((meta.size_x, meta.size_y, meta.size_c), (4, 3, 1));
    assert_eq!(meta.pixel_type, PixelType::Uint8);
    assert_string(
        &meta.series_metadata,
        "xlef.lms.pixel_payload",
        "raw_storage",
    );
    assert_string(
        &meta.series_metadata,
        "xlef.lms.pixel_layout.status",
        "supported_raw_storage",
    );
    assert_string(
        &meta.series_metadata,
        "xlef.project.source_kind",
        "lms_pixel",
    );
    assert!(matches!(
        meta.series_metadata.get("xlef.lms.pixel_layout.storage_path"),
        Some(MetadataValue::String(path)) if path.ends_with("pixel data.raw")
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), pixels);
    assert_eq!(
        reader.open_bytes_region(0, 1, 1, 2, 2).unwrap(),
        vec![6, 7, 10, 11]
    );

    let _ = std::fs::remove_file(xlef);
    let _ = std::fs::remove_file(lms);
    let _ = std::fs::remove_file(raw);
}

#[test]
fn xlef_lms_raw_storage_reads_padded_rows_from_declared_strides() {
    let xlef = temp_path("raw_storage_padded_project.xlef");
    let lms = xlef.with_extension("lms");
    let raw = xlef.with_file_name("padded.raw");
    std::fs::write(
        &raw,
        [
            1u8, 2, 3, 4, 200, 201, 202, 203, 5, 6, 7, 8, 204, 205, 206, 207, 9, 10, 11, 12, 208,
            209, 210, 211,
        ],
    )
    .unwrap();
    std::fs::write(
        &lms,
        r#"<LMSDataContainerHeader><Element Name="padded raw storage"><Data><Image Name="raw scan">
<ImageDescription>
<Channels><ChannelDescription Resolution="8" BytesInc="0"/></Channels>
<Dimensions>
<DimensionDescription DimID="1" NumberOfElements="4" BytesInc="1"/>
<DimensionDescription DimID="2" NumberOfElements="3" BytesInc="8"/>
</Dimensions>
<Storage FileName="padded.raw"/>
</ImageDescription>
</Image></Data></Element></LMSDataContainerHeader>"#,
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
    let meta = reader.metadata();
    assert_string(
        &meta.series_metadata,
        "xlef.lms.pixel_payload",
        "raw_storage",
    );
    assert_string(
        &meta.series_metadata,
        "xlef.project.source_kind",
        "lms_pixel",
    );
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
    );
    assert_eq!(
        reader.open_bytes_region(0, 1, 1, 2, 2).unwrap(),
        vec![6, 7, 10, 11]
    );

    let _ = std::fs::remove_file(xlef);
    let _ = std::fs::remove_file(lms);
    let _ = std::fs::remove_file(raw);
}

#[test]
fn xlef_lms_raw_storage_reads_multiplane_declared_strides() {
    let xlef = temp_path("raw_storage_multiplane_project.xlef");
    let lms = xlef.with_extension("lms");
    let raw = xlef.with_file_name("multiplane.raw");
    let mut pixels = vec![0u8; 32];
    for (offset, values) in [
        (0usize, [1u8, 2, 3, 4]),
        (8usize, [11u8, 12, 13, 14]),
        (16usize, [21u8, 22, 23, 24]),
        (24usize, [31u8, 32, 33, 34]),
    ] {
        pixels[offset..offset + 2].copy_from_slice(&values[0..2]);
        pixels[offset + 4..offset + 6].copy_from_slice(&values[2..4]);
    }
    std::fs::write(&raw, &pixels).unwrap();
    std::fs::write(
        &lms,
        r#"<LMSDataContainerHeader><Element Name="multiplane raw storage"><Data><Image Name="raw scan">
<ImageDescription>
<Channels>
<ChannelDescription Name="C0" Resolution="8" BytesInc="0"/>
<ChannelDescription Name="C1" Resolution="8" BytesInc="16"/>
</Channels>
<Dimensions>
<DimensionDescription DimID="1" NumberOfElements="2" BytesInc="1"/>
<DimensionDescription DimID="2" NumberOfElements="2" BytesInc="4"/>
<DimensionDescription DimID="3" NumberOfElements="2" BytesInc="8"/>
<DimensionDescription DimID="5" NumberOfElements="2" BytesInc="16"/>
</Dimensions>
<Storage FileName="multiplane.raw"/>
</ImageDescription>
</Image></Data></Element></LMSDataContainerHeader>"#,
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
    let meta = reader.metadata();
    assert_eq!(
        (meta.size_x, meta.size_y, meta.size_z, meta.size_c),
        (2, 2, 2, 2)
    );
    assert_eq!(meta.image_count, 4);
    assert_string(
        &meta.series_metadata,
        "xlef.lms.pixel_payload",
        "raw_storage",
    );
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![11, 12, 13, 14]);
    assert_eq!(reader.open_bytes(2).unwrap(), vec![21, 22, 23, 24]);
    assert_eq!(reader.open_bytes(3).unwrap(), vec![31, 32, 33, 34]);
    assert_eq!(
        reader.open_bytes_region(3, 1, 0, 1, 2).unwrap(),
        vec![32, 34]
    );

    let _ = std::fs::remove_file(xlef);
    let _ = std::fs::remove_file(lms);
    let _ = std::fs::remove_file(raw);
}

#[test]
fn xlef_lms_roi_shape_aliases_project_to_ome_shapes() {
    let xlef = temp_path("roi_shapes_project.xlef");
    let lms = xlef.with_extension("lms");
    std::fs::write(
        &lms,
        r#"<XLIF><Element Name="roi shapes"><Data><Image Name="LMS ROI scan">
<ImageDescription>
<Dimensions>
<DimensionDescription DimID="1" NumberOfElements="8"/>
<DimensionDescription DimID="2" NumberOfElements="6"/>
</Dimensions>
<ROIs>
<ROI ID="line-1" Name="Track" Shape="Line" X1="1" Y1="2" X2="7" Y2="5" TheZ="1"/>
<ROI ID="ellipse-1" Name="Nucleus" Shape="Ellipse" CenterX="4" CenterY="3" RadiusX="2" RadiusY="1.5" TheC="0" TheT="2"/>
<ROI ID="point-1" Name="Spot" X="6.5" Y="2.5" IndexC="1"/>
</ROIs>
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
    let meta = reader.metadata();
    assert_int(&meta.series_metadata, "xlef.lms.graph.roi_count", 3);
    assert_float(&meta.series_metadata, "xlef.lms.roi.0.x1", 1.0);
    assert_float(&meta.series_metadata, "xlef.lms.roi.1.radius_x", 2.0);
    assert_int(&meta.series_metadata, "xlef.lms.roi.2.index_c", 1);

    let ome = reader.ome_metadata().expect("OME metadata");
    assert_eq!(ome.rois.len(), 3);
    assert!(matches!(
        ome.rois[0].shapes.first(),
        Some(OmeShape::Line {
            x1,
            y1,
            x2,
            y2,
            the_z,
            ..
        }) if (*x1, *y1, *x2, *y2, *the_z) == (1.0, 2.0, 7.0, 5.0, Some(1))
    ));
    assert!(matches!(
        ome.rois[1].shapes.first(),
        Some(OmeShape::Ellipse {
            x,
            y,
            radius_x,
            radius_y,
            the_c,
            the_t,
            ..
        }) if (*x, *y, *radius_x, *radius_y, *the_c, *the_t)
            == (4.0, 3.0, 2.0, 1.5, Some(0), Some(2))
    ));
    assert!(matches!(
        ome.rois[2].shapes.first(),
        Some(OmeShape::Point { x, y, the_c, .. })
            if (*x, *y, *the_c) == (6.5, 2.5, Some(1))
    ));

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
fn xlef_xlif_multiple_frame_files_are_one_java_style_series() {
    let xlef = temp_path("multi_frame_project.xlef");
    let xlif = xlef.with_file_name("stack.xlif");
    let bmp_a = xlef.with_file_name("frame_a.bmp");
    let bmp_b = xlef.with_file_name("frame_b.bmp");
    write_one_pixel_bmp(&bmp_a, 11, 12, 13);
    write_one_pixel_bmp(&bmp_b, 21, 22, 23);
    std::fs::write(
        &xlif,
        r#"<XLIF><Element Name="Z stack"><Data><Image Name="Stack">
<ImageDescription>
<Channels><ChannelDescription Resolution="8"/></Channels>
<Dimensions>
<DimensionDescription DimID="1" NumberOfElements="1" BytesInc="1"/>
<DimensionDescription DimID="2" NumberOfElements="1" BytesInc="1"/>
<DimensionDescription DimID="3" NumberOfElements="2" BytesInc="1"/>
</Dimensions>
<Frame File="frame_a.bmp"/>
<Frame File="frame_b.bmp"/>
</ImageDescription>
</Image></Data></Element></XLIF>"#,
    )
    .unwrap();
    std::fs::write(&xlef, r#"<XLEF><Reference File="stack.xlif"/></XLEF>"#).unwrap();

    let mut reader = XlefReader::new();
    reader.set_id(&xlef).unwrap();
    assert_eq!(reader.series_count(), 1);
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 2);
    assert_eq!(meta.image_count, 2);
    reader.set_resolution(0).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![11, 12, 13]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![21, 22, 23]);
    assert_eq!(reader.open_thumb_bytes(1).unwrap(), vec![21, 22, 23]);
    assert!(matches!(
        reader.open_bytes(2),
        Err(bioformats::BioFormatsError::PlaneOutOfRange(2))
    ));

    let _ = std::fs::remove_file(xlef);
    let _ = std::fs::remove_file(xlif);
    let _ = std::fs::remove_file(bmp_a);
    let _ = std::fs::remove_file(bmp_b);
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
