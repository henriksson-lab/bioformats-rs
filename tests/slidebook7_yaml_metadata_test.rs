use std::path::{Path, PathBuf};

use bioformats::common::metadata::MetadataValue;
use bioformats::formats::flim2::SlideBook7Reader;
use bioformats::FormatReader;

fn temp_path(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bioformats_slidebook7_yaml_metadata_{}_{}_{}",
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

fn write_npy(path: &Path, values: &[u16]) {
    let payload = values
        .iter()
        .copied()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    std::fs::write(path, build_npy("<u2", &[1, 2, 2], &payload)).unwrap();
}

// This exercises a port-specific "flatten arbitrary nested YAML into
// slidebook7.record.* original metadata" behaviour that upstream Bio-Formats
// does NOT implement: the Java SlideBook7Reader decodes typed record fields via
// snakeyaml + ClassDecoder and discards unrecognised attributes
// (DecodeUnknownString has an empty body). The faithful path is the typed
// CImageRecord70 decoder (see slidebook7_typed_* tests in
// tests/slidebook7_sldyz_test.rs and src/formats/flim2.rs). Kept ignored as a
// record of the deferred non-upstream flatten feature rather than deleted.
#[test]
#[ignore = "non-upstream flatten-to-metadata YAML behaviour; upstream uses typed ClassDecoder (see slidebook7_typed_* tests)"]
fn slidebook7_preserves_safe_scalar_yaml_metadata() {
    let path = temp_path("native.sldy");
    std::fs::write(&path, b"SlideBook 7 native placeholder").unwrap();
    let root = path.with_extension("dir");
    let group = root.join("Capture.imgdir");
    std::fs::create_dir_all(&group).unwrap();
    std::fs::write(
        group.join("ImageRecord.yaml"),
        "\
mWidth: 2
mHeight: 2
mNumPlanes: 1
mNumChannels: 2
mNumTimepoints: 1
mElapsedTime: 9.5
mStageX: 1.25
AcquisitionName: Test capture
IsMontage: false
CameraGain: 3
StagePosition: {X: 1.25, Y: -2.5, Z: 3.75}
Objective: {Magnification: 60, Immersion: Oil}
ExcitationWavelengths: [405, 488]
Units:
  X: um
  Time: s
Timestamps:
  Start: 2024-01-02T03:04:05Z
Masks:
  - Name: Nucleus
    Area: 42.5
    Units:
      Area: um2
Annotations:
  Experiment:
    Operator: Ada
    Approved: true
Tags:
  - control
  - replicate
Regions:
  - {Name: ROI-1, Area: 5.5}
  - {Name: ROI-2, Area: 6.5}
Objects: [{Name: Cell-1, Center: {X: 1.5, Y: 2.5}, Measurements: [7, 8]}, {Name: Cell-2, Center: {X: 3.5, Y: 4.5}}]
TaggedObjective: !<SlideBook.Objective>
  Name: Plan Apo
  Magnification: !<SlideBook.Quantity> 63
TaggedChannels:
  - !<SlideBook.Channel>
    Name: Cy5
    Enabled: !<SlideBook.Boolean> true
TaggedObjects: !<SlideBook.ObjectList> [{Name: Cell-3, Center: !<SlideBook.Point> {X: 5.5, Y: 6.5}}]
mChannels:
  - mName: DAPI
    mElapsedTime: 1.5
    mPositionX: 10.25
    mPositionY: -2.5
    mPositionZ: 3.75
    StagePosition: {X: 10.25, Y: -2.5, Z: 3.75}
  - mName: FITC
    mElapsedTime: 2.75
    StagePosition: {X: 11.25, Y: -3.5, Z: 4.75}
",
    )
    .unwrap();
    write_npy(&group.join("ImageData_Ch0_TP0000000.npy"), &[0, 1, 2, 3]);
    write_npy(
        &group.join("ImageData_Ch1_TP0000000.npy"),
        &[10, 11, 12, 13],
    );

    let mut reader = SlideBook7Reader::new();
    reader.set_id(&path).expect("native SlideBook 7 fixture");
    let md = &reader.metadata().series_metadata;

    assert!(matches!(
        md.get("slidebook7.channel.0.name"),
        Some(MetadataValue::String(value)) if value == "DAPI"
    ));
    assert!(matches!(
        md.get("slidebook7.channel.1.name"),
        Some(MetadataValue::String(value)) if value == "FITC"
    ));
    assert!(matches!(
        md.get("slidebook7.channel.0.elapsed_time"),
        Some(MetadataValue::Float(value)) if (*value - 1.5).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.channel.1.elapsed_time"),
        Some(MetadataValue::Float(value)) if (*value - 2.75).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.channel.0.position_x"),
        Some(MetadataValue::Float(value)) if (*value - 10.25).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.channel.0.position_y"),
        Some(MetadataValue::Float(value)) if (*value + 2.5).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.channel.0.position_z"),
        Some(MetadataValue::Float(value)) if (*value - 3.75).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.channel.1.position_x"),
        Some(MetadataValue::Float(value)) if (*value - 11.25).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.elapsed_time"),
        Some(MetadataValue::Float(value)) if (*value - 9.5).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.position_x"),
        Some(MetadataValue::Float(value)) if (*value - 1.25).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.position_y"),
        Some(MetadataValue::Float(value)) if (*value + 2.5).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.record.acquisitionname"),
        Some(MetadataValue::String(value)) if value == "Test capture"
    ));
    assert!(matches!(
        md.get("slidebook7.record.ismontage"),
        Some(MetadataValue::Bool(false))
    ));
    assert!(matches!(
        md.get("slidebook7.record.cameragain"),
        Some(MetadataValue::Int(3))
    ));
    assert!(matches!(
        md.get("slidebook7.record.stageposition.x"),
        Some(MetadataValue::Float(value)) if (*value - 1.25).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.record.objective.magnification"),
        Some(MetadataValue::Int(60))
    ));
    assert!(matches!(
        md.get("slidebook7.record.objective.immersion"),
        Some(MetadataValue::String(value)) if value == "Oil"
    ));
    assert!(matches!(
        md.get("slidebook7.record.excitationwavelengths.0"),
        Some(MetadataValue::Int(405))
    ));
    assert!(matches!(
        md.get("slidebook7.record.excitationwavelengths.1"),
        Some(MetadataValue::Int(488))
    ));
    assert!(matches!(
        md.get("slidebook7.record.units.x"),
        Some(MetadataValue::String(value)) if value == "um"
    ));
    assert!(matches!(
        md.get("slidebook7.record.units.time"),
        Some(MetadataValue::String(value)) if value == "s"
    ));
    assert!(matches!(
        md.get("slidebook7.record.timestamps.start"),
        Some(MetadataValue::String(value)) if value == "2024-01-02T03:04:05Z"
    ));
    assert!(matches!(
        md.get("slidebook7.record.masks.0.name"),
        Some(MetadataValue::String(value)) if value == "Nucleus"
    ));
    assert!(matches!(
        md.get("slidebook7.record.masks.0.area"),
        Some(MetadataValue::Float(value)) if (*value - 42.5).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.record.masks.0.units.area"),
        Some(MetadataValue::String(value)) if value == "um2"
    ));
    assert!(matches!(
        md.get("slidebook7.record.annotations.experiment.operator"),
        Some(MetadataValue::String(value)) if value == "Ada"
    ));
    assert!(matches!(
        md.get("slidebook7.record.annotations.experiment.approved"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        md.get("slidebook7.record.tags.0"),
        Some(MetadataValue::String(value)) if value == "control"
    ));
    assert!(matches!(
        md.get("slidebook7.record.tags.1"),
        Some(MetadataValue::String(value)) if value == "replicate"
    ));
    assert!(matches!(
        md.get("slidebook7.record.regions.0.name"),
        Some(MetadataValue::String(value)) if value == "ROI-1"
    ));
    assert!(matches!(
        md.get("slidebook7.record.regions.1.area"),
        Some(MetadataValue::Float(value)) if (*value - 6.5).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.record.objects.0.name"),
        Some(MetadataValue::String(value)) if value == "Cell-1"
    ));
    assert!(matches!(
        md.get("slidebook7.record.objects.0.center.x"),
        Some(MetadataValue::Float(value)) if (*value - 1.5).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.record.objects.0.measurements.1"),
        Some(MetadataValue::Int(8))
    ));
    assert!(matches!(
        md.get("slidebook7.record.objects.1.center.y"),
        Some(MetadataValue::Float(value)) if (*value - 4.5).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.record.taggedobjective.name"),
        Some(MetadataValue::String(value)) if value == "Plan Apo"
    ));
    assert!(matches!(
        md.get("slidebook7.record.taggedobjective.magnification"),
        Some(MetadataValue::Int(63))
    ));
    assert!(matches!(
        md.get("slidebook7.record.taggedchannels.0.name"),
        Some(MetadataValue::String(value)) if value == "Cy5"
    ));
    assert!(matches!(
        md.get("slidebook7.record.taggedchannels.0.enabled"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        md.get("slidebook7.record.taggedobjects.0.center.y"),
        Some(MetadataValue::Float(value)) if (*value - 6.5).abs() < 1e-12
    ));
    assert!(matches!(
        md.get("slidebook7.record.mchannels.0.mname"),
        Some(MetadataValue::String(value)) if value == "DAPI"
    ));
    assert_eq!(
        reader.open_bytes(1).unwrap(),
        [10u16, 11, 12, 13]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    );

    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_file(path);
}
