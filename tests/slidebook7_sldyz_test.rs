use std::io::Write;
use std::path::{Path, PathBuf};

use bioformats::common::error::BioFormatsError;
use bioformats::common::metadata::MetadataValue;
use bioformats::common::pixel_type::PixelType;
use bioformats::formats::flim2::SlideBook7Reader;
use bioformats::FormatReader;

fn temp_path(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bioformats_slidebook7_sldyz_{}_{}_{}",
        std::process::id(),
        nanos,
        name
    ))
}

fn build_npy(descr: &str, shape: &[u32], payload: &[u8]) -> Vec<u8> {
    let shape_text = if shape.len() == 1 {
        format!("({},)", shape[0])
    } else {
        shape
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut header =
        format!("{{'descr': '{descr}', 'fortran_order': False, 'shape': ({shape_text}), }}");
    let preamble_len = 10usize;
    let padding = (16 - ((preamble_len + header.len() + 1) % 16)) % 16;
    header.extend(std::iter::repeat_n(' ', padding));
    header.push('\n');

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x93NUMPY");
    bytes.push(1);
    bytes.push(0);
    bytes.extend_from_slice(&(header.len() as u16).to_le_bytes());
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn write_entry<W: Write + std::io::Seek>(zip: &mut zip::ZipWriter<W>, name: &str, bytes: &[u8]) {
    zip.start_file(name, zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(bytes).unwrap();
}

fn gzip_npy(descr: &str, shape: &[u32], payload: &[u8]) -> Vec<u8> {
    let npy = build_npy(descr, shape, payload);
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(&npy).unwrap();
    encoder.finish().unwrap()
}

fn write_sldyz(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    write_entry(
        &mut zip,
        "native.dir/Capture.imgdir/ImageRecord.yaml",
        b"mWidth: 2\nmHeight: 2\nmNumPlanes: 1\nmNumChannels: 1\nmNumTimepoints: 2\n",
    );
    let payload = [10u16, 11, 12, 13, 20, 21, 22, 23]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    write_entry(
        &mut zip,
        "native.dir/Capture.imgdir/ImageData_Ch0_TP0000000.npy",
        &build_npy("<u2", &[2, 2, 2], &payload),
    );
    zip.finish().unwrap();
}

fn write_sldy_with_compression_dictionary(path: &Path, compression: &str) {
    std::fs::write(path, b"SlideBook 7 native placeholder").unwrap();
    let root = path.with_extension("dir");
    let group = root.join("Capture.imgdir");
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(
        group.join("ImageRecord.yaml"),
        b"mWidth: 2\nmHeight: 2\nmNumPlanes: 1\nmNumChannels: 1\nmNumTimepoints: 1\n",
    )
    .unwrap();
    let payload = [10u16, 11, 12, 13]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    std::fs::write(
        group.join("ImageData_Ch0_TP0000000.npyz"),
        gzip_npy("<u2", &[1, 2, 2], &payload),
    )
    .unwrap();
    std::fs::write(
        group.join("CompressionDictionary.yaml"),
        format!("ImageData_Ch0_TP0000000.npyz: {compression}\n"),
    )
    .unwrap();
}

fn write_sldy_with_npyz_bytes(path: &Path, bytes: &[u8]) {
    std::fs::write(path, b"SlideBook 7 native placeholder").unwrap();
    let root = path.with_extension("dir");
    let group = root.join("Capture.imgdir");
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(
        group.join("ImageRecord.yaml"),
        b"mWidth: 2\nmHeight: 2\nmNumPlanes: 1\nmNumChannels: 1\nmNumTimepoints: 1\n",
    )
    .unwrap();
    std::fs::write(group.join("ImageData_Ch0_TP0000000.npyz"), bytes).unwrap();
}

fn write_sldy_with_npy_descriptors(path: &Path, first_descr: &str, second_descr: &str) {
    std::fs::write(path, b"SlideBook 7 native placeholder").unwrap();
    let root = path.with_extension("dir");
    let group = root.join("Capture.imgdir");
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(
        group.join("ImageRecord.yaml"),
        b"mWidth: 1\nmHeight: 1\nmNumPlanes: 1\nmNumChannels: 2\nmNumTimepoints: 1\n",
    )
    .unwrap();
    std::fs::write(
        group.join("ImageData_Ch0_TP0000000.npy"),
        build_npy(first_descr, &[1, 1], &[]),
    )
    .unwrap();
    std::fs::write(
        group.join("ImageData_Ch1_TP0000000.npy"),
        build_npy(second_descr, &[1, 1], &[]),
    )
    .unwrap();
}

fn write_nested_sldyz(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    write_entry(
        &mut zip,
        "export/session/native.dir/Capture.imgdir/ImageRecord.yaml",
        b"mWidth: 2\nmHeight: 1\nmNumPlanes: 1\nmNumChannels: 1\nmNumTimepoints: 1\n",
    );
    let payload = [41u16, 42]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    write_entry(
        &mut zip,
        "export/session/native.dir/Capture.imgdir/ImageData_Ch0_TP0000000.npy",
        &build_npy("<u2", &[1, 2], &payload),
    );
    zip.finish().unwrap();
}

#[test]
fn slidebook7_reads_sldyz_archive_with_supported_native_payloads() {
    let path = temp_path("archive.sldyz");
    write_sldyz(&path);

    let mut reader = SlideBook7Reader::new();
    assert!(reader.is_this_type_by_name(&path));
    reader.set_id(&path).expect("sldyz archive should open");

    let meta = reader.metadata().clone();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 2);
    assert_eq!(meta.pixel_type, PixelType::Uint16);
    assert!(meta.is_little_endian);
    assert_eq!(
        reader.open_bytes(1).unwrap(),
        [20u16, 21, 22, 23]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(),
        [21u16, 23]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    );

    reader.close().unwrap();
    let _ = std::fs::remove_file(path);
}

#[test]
fn slidebook7_reads_sldyz_archive_with_nested_native_root() {
    let path = temp_path("nested-archive.sldyz");
    write_nested_sldyz(&path);

    let mut reader = SlideBook7Reader::new();
    reader
        .set_id(&path)
        .expect("nested sldyz archive should open");

    let meta = reader.metadata().clone();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.image_count, 1);
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        [41u16, 42]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    );

    reader.close().unwrap();
    let _ = std::fs::remove_file(path);
}

#[test]
fn slidebook7_validates_native_compression_dictionary_entries() {
    let path = temp_path("dictionary.sldy");
    write_sldy_with_compression_dictionary(&path, "gzip");

    let mut reader = SlideBook7Reader::new();
    reader
        .set_id(&path)
        .expect("compression dictionary-backed native SlideBook 7");

    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("slidebook7.compression_dictionary.entries"),
        Some(MetadataValue::Int(1))
    ));
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        [10u16, 11, 12, 13]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(path.with_extension("dir"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn slidebook7_rejects_inconsistent_compression_dictionary_entries() {
    let path = temp_path("bad-dictionary.sldy");
    write_sldy_with_compression_dictionary(&path, "uncompressed");

    let err = SlideBook7Reader::new().set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message)
            if message.contains("compression dictionary")
                && message.contains("ImageData_Ch0_TP0000000.npyz")
                && message.contains("uncompressed")),
        "unexpected compression dictionary error: {err:?}"
    );

    let _ = std::fs::remove_dir_all(path.with_extension("dir"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn slidebook7_reports_unknown_npyz_container_signature() {
    let path = temp_path("zip-npyz.sldy");
    write_sldy_with_npyz_bytes(&path, b"PK\x03\x04not-a-supported-npyz-container");

    let err = SlideBook7Reader::new().set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message)
            if message.contains("NPYZ image data")
                && message.contains("ZIP container")
                && message.contains("first bytes [50 4b 03 04")
                && message.contains("probes gzip:")
                && message.contains("zlib:")
                && message.contains("deflate:")),
        "unexpected NPYZ diagnostic: {err:?}"
    );

    let _ = std::fs::remove_dir_all(path.with_extension("dir"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn slidebook7_reports_mixed_npy_pixel_type_details() {
    let path = temp_path("mixed-type.sldy");
    write_sldy_with_npy_descriptors(&path, "<u2", "<u1");

    let err = SlideBook7Reader::new().set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message)
            if message.contains("mixed NPY pixel types")
                && message.contains("ImageData_Ch0_TP0000000.npy")
                && message.contains("descriptor \"<u2\"")
                && message.contains("Uint16")
                && message.contains("ImageData_Ch1_TP0000000.npy")
                && message.contains("descriptor \"<u1\"")
                && message.contains("Uint8")),
        "unexpected mixed type diagnostic: {err:?}"
    );

    let _ = std::fs::remove_dir_all(path.with_extension("dir"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn slidebook7_reports_mixed_npy_byte_order_details() {
    let path = temp_path("mixed-endian.sldy");
    write_sldy_with_npy_descriptors(&path, "<u2", ">u2");

    let err = SlideBook7Reader::new().set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message)
            if message.contains("mixed NPY byte orders")
                && message.contains("ImageData_Ch0_TP0000000.npy")
                && message.contains("descriptor \"<u2\"")
                && message.contains("little-endian")
                && message.contains("ImageData_Ch1_TP0000000.npy")
                && message.contains("descriptor \">u2\"")
                && message.contains("big-endian")),
        "unexpected mixed byte-order diagnostic: {err:?}"
    );

    let _ = std::fs::remove_dir_all(path.with_extension("dir"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn slidebook7_typed_image_record_drives_reader() {
    // Real SlideBook 7 StartClass/EndClass record layout: dimensions and
    // metadata must come from the typed CImageRecord70 decoder, not the
    // bounded line-scan fallback.
    let path = temp_path("typed.sldy");
    std::fs::write(&path, b"SlideBook 7 native placeholder").unwrap();
    let root = path.with_extension("dir");
    let group = root.join("Capture.imgdir");
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(
        group.join("ImageRecord.yaml"),
        "\
StartClass:
  ClassName: CImageRecord70
  mWidth: 3
  mHeight: 2
  mNumPlanes: 1
  mNumChannels: 1
  mNumTimepoints: 1
  mName: Typed capture
EndClass: 0
StartClass:
  ClassName: CLensDef70
  mName: 60x Oil
  mNA: 1.4
EndClass: 0
",
    )
    .unwrap();
    std::fs::write(
        group.join("ChannelRecord.yaml"),
        "\
StartClass:
  ClassName: CChannelRecord70
  mNumPlanes: 1
EndClass: 0
StartClass:
  ClassName: CExposureRecord70
  mExposureTime: 75
EndClass: 0
StartClass:
  ClassName: CChannelDef70
  mName: GFP
EndClass: 0
StartClass:
  ClassName: CFluorDef70
  mExcitationLambda: 488
  mLambda: 509
EndClass: 0
",
    )
    .unwrap();
    std::fs::write(
        group.join("MaskRecord.yaml"),
        "\
theNumMasks: 1
StartClass:
  ClassName: CMaskRecord70
  mName: Nucleus
EndClass: 0
",
    )
    .unwrap();
    std::fs::write(
        group.join("AnnotationRecord.yaml"),
        "\
StartClass:
  ClassName: CDataTableHeaderRecord70
  mChannelIndex: 0
EndClass: 0
theTimepointIndex: 0
theCubeAnnotation70ListSize: 0
theAnnotation70ListSize: 1
StartClass:
  ClassName: CAnnotation70
  mText: Region A
EndClass: 0
theFRAPRegionAnnotation70ListSize: 0
theUnknownAnnotation70ListSize: 0
",
    )
    .unwrap();
    std::fs::write(
        group.join("ElapsedTimes.yaml"),
        "theElapsedTimes: [2, 0, 250]\n",
    )
    .unwrap();
    std::fs::write(
        group.join("StagePositionData.yaml"),
        "StructArraySize: 1\nStructArrayValues: [1.5, 2.5, 3.5]\n",
    )
    .unwrap();
    std::fs::write(
        group.join("AuxData.yaml"),
        "\
theAuxFloatDataTablesSize: 1
StartClass:
  ClassName: CDataTableHeaderRecord70
  mChannelIndex: 0
EndClass: 0
theXMLDescriptor: aux-float
theAuxData: [2, 1.0, 2.0]
theAuxDoubleDataTablesSize: 0
theAuxSInt32DataTablesSize: 0
theAuxSInt64DataTablesSize: 0
theAuxSerializedDataTablesSize: 0
",
    )
    .unwrap();
    let payload = [1u16, 2, 3, 4, 5, 6]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    std::fs::write(
        group.join("ImageData_Ch0_TP0000000.npy"),
        build_npy("<u2", &[2, 3], &payload),
    )
    .unwrap();

    let mut reader = SlideBook7Reader::new();
    reader.set_id(&path).expect("typed SlideBook 7 record");
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 3);
    assert_eq!(meta.size_y, 2);
    assert!(matches!(
        meta.series_metadata.get("slidebook7.image_record.name"),
        Some(MetadataValue::String(value)) if value == "Typed capture"
    ));
    assert!(matches!(
        meta.series_metadata.get("slidebook7.image_record.lens.name"),
        Some(MetadataValue::String(value)) if value == "60x Oil"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("slidebook7.image_record.lens.numerical_aperture"),
        Some(MetadataValue::Float(value)) if (*value - 1.4).abs() < 1e-9
    ));
    // Channel metadata from the typed ChannelRecord.yaml decoder.
    assert!(matches!(
        meta.series_metadata.get("slidebook7.channel.0.name"),
        Some(MetadataValue::String(value)) if value == "GFP"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("slidebook7.channel.0.exposure_time"),
        Some(MetadataValue::Int(75))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("slidebook7.channel.0.emission_wavelength"),
        Some(MetadataValue::Float(value)) if (*value - 509.0).abs() < 1e-9
    ));
    // Mask + annotation metadata from the typed MaskRecord/AnnotationRecord decoders.
    assert!(matches!(
        meta.series_metadata.get("slidebook7.mask.count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata.get("slidebook7.mask.0.name"),
        Some(MetadataValue::String(value)) if value == "Nucleus"
    ));
    assert!(matches!(
        meta.series_metadata.get("slidebook7.annotation.base_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata.get("slidebook7.annotation.0.text"),
        Some(MetadataValue::String(value)) if value == "Region A"
    ));
    // Elapsed-times / stage-position / aux-data typed loaders.
    assert!(matches!(
        meta.series_metadata.get("slidebook7.elapsed_times.count"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata.get("slidebook7.stage_positions.0.x"),
        Some(MetadataValue::Float(value)) if (*value - 1.5).abs() < 1e-9
    ));
    assert!(matches!(
        meta.series_metadata.get("slidebook7.aux.float_tables"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata.get("slidebook7.aux.float.0.descriptor"),
        Some(MetadataValue::String(value)) if value == "aux-float"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), payload);

    // OME plane timing/position promoted from the typed records: DeltaT from the
    // elapsed-times array, ExposureTime from the channel exposure (ms -> s), and
    // PositionX/Y/Z from the stage point.
    let ome = reader.ome_metadata().expect("OME metadata");
    let image = ome.images.first().expect("OME image");
    assert_eq!(image.planes.len(), 1);
    let plane = &image.planes[0];
    assert_eq!(plane.delta_t, Some(0.0));
    assert_eq!(plane.exposure_time, Some(0.075));
    assert_eq!(plane.position_x, Some(1.5));
    assert_eq!(plane.position_y, Some(2.5));
    assert_eq!(plane.position_z, Some(3.5));

    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_file(path);
}

#[test]
fn slidebook7_typed_ome_planes_vary_by_z_and_t() {
    // 2 timepoints x 2 Z planes x 1 channel: DeltaT must vary per timepoint and
    // PositionZ per Z plane (stage Z + interplane spacing * z).
    let path = temp_path("planes.sldy");
    std::fs::write(&path, b"SlideBook 7 native placeholder").unwrap();
    let root = path.with_extension("dir");
    let group = root.join("Capture.imgdir");
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(
        group.join("ImageRecord.yaml"),
        "\
StartClass:
  ClassName: CImageRecord70
  mWidth: 3
  mHeight: 2
  mNumPlanes: 2
  mNumChannels: 1
  mNumTimepoints: 2
EndClass: 0
",
    )
    .unwrap();
    std::fs::write(
        group.join("ChannelRecord.yaml"),
        "\
StartClass:
  ClassName: CChannelRecord70
  mNumPlanes: 2
EndClass: 0
StartClass:
  ClassName: CExposureRecord70
  mExposureTime: 50
  mInterplaneSpacing: 0.5
EndClass: 0
StartClass:
  ClassName: CChannelDef70
  mName: Ch0
EndClass: 0
StartClass:
  ClassName: CFluorDef70
  mLambda: 500
EndClass: 0
",
    )
    .unwrap();
    std::fs::write(
        group.join("ElapsedTimes.yaml"),
        "theElapsedTimes: [2, 0, 1000]\n",
    )
    .unwrap();
    std::fs::write(
        group.join("StagePositionData.yaml"),
        "StructArraySize: 1\nStructArrayValues: [10.0, 20.0, 30.0]\n",
    )
    .unwrap();
    let plane_payload: Vec<u8> = (0u16..12).flat_map(u16::to_le_bytes).collect();
    std::fs::write(
        group.join("ImageData_Ch0_TP0000000.npy"),
        build_npy("<u2", &[2, 2, 3], &plane_payload),
    )
    .unwrap();
    std::fs::write(
        group.join("ImageData_Ch0_TP0000001.npy"),
        build_npy("<u2", &[2, 2, 3], &plane_payload),
    )
    .unwrap();

    let mut reader = SlideBook7Reader::new();
    reader.set_id(&path).expect("typed multi-plane SlideBook 7");
    assert_eq!(reader.metadata().image_count, 4);

    let ome = reader.ome_metadata().expect("OME metadata");
    let planes = &ome.images.first().expect("OME image").planes;
    assert_eq!(planes.len(), 4);
    // Reader plane order: z fastest, then channel, then timepoint.
    // p0: z0 t0, p1: z1 t0, p2: z0 t1, p3: z1 t1.
    assert_eq!((planes[0].the_z, planes[0].the_t), (0, 0));
    assert_eq!((planes[1].the_z, planes[1].the_t), (1, 0));
    assert_eq!((planes[2].the_z, planes[2].the_t), (0, 1));
    assert_eq!((planes[3].the_z, planes[3].the_t), (1, 1));
    assert_eq!(planes[0].delta_t, Some(0.0));
    assert_eq!(planes[2].delta_t, Some(1.0));
    assert_eq!(planes[0].position_z, Some(30.0));
    assert_eq!(planes[1].position_z, Some(30.5));
    assert_eq!(planes[3].position_z, Some(30.5));
    for plane in planes {
        assert_eq!(plane.exposure_time, Some(0.05));
        assert_eq!(plane.position_x, Some(10.0));
        assert_eq!(plane.position_y, Some(20.0));
    }

    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_file(path);
}
