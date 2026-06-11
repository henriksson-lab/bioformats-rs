use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats::{BioFormatsError, FormatReader, MetadataValue};

const PTU_TAG_INT8: u32 = 0x1000_0008;
const PTU_TAG_EMPTY8: u32 = 0xffff_0008;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp(name: &str) -> std::path::PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("bioformats_picoquant_ptu_{name}_{nanos}_{n}"))
}

fn append_ptu_tag(out: &mut Vec<u8>, ident: &str, index: i32, tag_type: u32, value: i64) {
    let mut name = [0u8; 32];
    let bytes = ident.as_bytes();
    let len = bytes.len().min(name.len());
    name[..len].copy_from_slice(&bytes[..len]);
    out.extend_from_slice(&name);
    out.extend_from_slice(&index.to_le_bytes());
    out.extend_from_slice(&tag_type.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}

fn append_ptu_int_tag(out: &mut Vec<u8>, ident: &str, value: i64) {
    append_ptu_tag(out, ident, -1, PTU_TAG_INT8, value);
}

fn minimal_ptu(record_type: i64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PQTTTR\0\0");
    out.extend_from_slice(b"1.0\0\0\0\0\0");
    append_ptu_int_tag(&mut out, "ImgHdr_PixX", 3);
    append_ptu_int_tag(&mut out, "ImgHdr_PixY", 2);
    append_ptu_int_tag(&mut out, "ImgHdr_Frame", 1);
    append_ptu_int_tag(&mut out, "TTResult_NumberOfRecords", 0);
    append_ptu_int_tag(&mut out, "TTResultFormat_TTTRRecType", record_type);
    append_ptu_tag(&mut out, "Header_End", -1, PTU_TAG_EMPTY8, 0);
    out
}

fn minimal_ptu_without_record_type() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PQTTTR\0\0");
    out.extend_from_slice(b"1.0\0\0\0\0\0");
    append_ptu_int_tag(&mut out, "ImgHdr_PixX", 3);
    append_ptu_int_tag(&mut out, "ImgHdr_PixY", 2);
    append_ptu_int_tag(&mut out, "ImgHdr_Frame", 1);
    append_ptu_int_tag(&mut out, "TTResult_NumberOfRecords", 0);
    append_ptu_tag(&mut out, "Header_End", -1, PTU_TAG_EMPTY8, 0);
    out
}

fn minimal_histogram_ptu() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PQTTTR\0\0");
    out.extend_from_slice(b"1.0\0\0\0\0\0");
    append_ptu_int_tag(&mut out, "HistResDscr_HistogramBins", 4);
    append_ptu_int_tag(&mut out, "HistResDscr_CurveIndex", 0);
    append_ptu_tag(&mut out, "Header_End", -1, PTU_TAG_EMPTY8, 0);
    out
}

fn minimal_histo_result_ptu(bins: i64, curves: i64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PQTTTR\0\0");
    out.extend_from_slice(b"1.0\0\0\0\0\0");
    append_ptu_int_tag(&mut out, "HistoResult_NumberOfBins", bins);
    append_ptu_int_tag(&mut out, "HistoResult_NumberOfCurves", curves);
    append_ptu_tag(&mut out, "Header_End", -1, PTU_TAG_EMPTY8, 0);
    out
}

#[test]
fn picoquant_ptu_recognizes_non_hydraharp_tttr_record_metadata_before_marker_reconstruction() {
    for (code, label, family, mode) in [
        (0x0001_0203, "PicoHarp T2", "PicoHarp", "tttr_t2"),
        (0x0001_0303, "PicoHarp T3", "PicoHarp", "tttr_t3"),
        (0x0001_0205, "TimeHarp 260N T2", "TimeHarp 260N", "tttr_t2"),
        (0x0001_0305, "TimeHarp 260N T3", "TimeHarp 260N", "tttr_t3"),
        (0x0001_0206, "TimeHarp 260P T2", "TimeHarp 260P", "tttr_t2"),
        (0x0001_0306, "TimeHarp 260P T3", "TimeHarp 260P", "tttr_t3"),
        (0x0001_0207, "MultiHarp T2", "MultiHarp", "tttr_t2"),
        (0x0001_0307, "MultiHarp T3", "MultiHarp", "tttr_t3"),
    ] {
        let path = tmp(label);
        std::fs::write(&path, minimal_ptu(code)).unwrap();

        let mut reader = bioformats::formats::spm::PicoQuantReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 3);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.image_count, 1);
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_record_type"),
            Some(MetadataValue::String(value)) if value == label
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_record_family"),
            Some(MetadataValue::String(value)) if value == family
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.acquisition_mode"),
            Some(MetadataValue::String(value)) if value == mode
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.acquisition_mode_ambiguous"),
            Some(MetadataValue::Bool(false))
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.acquisition_mode_source"),
            Some(MetadataValue::String(value)) if value == "TTResultFormat_TTTRRecType"
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_hydraharp_layout"),
            Some(MetadataValue::Bool(false))
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_marker_raster_layout"),
            Some(MetadataValue::Bool(value)) if *value == (family != "PicoHarp")
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_record_type_code_hex"),
            Some(MetadataValue::String(value)) if value == &format!("0x{code:08x}")
        ));

        if family == "PicoHarp" {
            assert!(matches!(
                meta.series_metadata.get("ptu.reconstruction_unsupported"),
                Some(MetadataValue::String(value))
                    if value.contains(label)
                        && value.contains("bit packing and marker encoding")
                        && value.contains("fixture/spec-blocked")
            ));
        } else {
            assert!(matches!(
                meta.series_metadata.get("ptu.reconstruction_unsupported"),
                Some(MetadataValue::String(value))
                    if value.contains(label) && value.contains("missing line-start marker")
            ));
        }

        let err = reader.open_bytes(0).unwrap_err();
        let expected_message = if family == "PicoHarp" {
            "fixture/spec-blocked"
        } else {
            "missing line-start marker"
        };
        assert!(
            matches!(
                err,
                BioFormatsError::UnsupportedFormat(ref message)
                    if message.contains(label) && message.contains(expected_message)
            ),
            "{err:?}"
        );

        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn picoquant_ptu_marks_unknown_tttr_record_mode_as_ambiguous() {
    let path = tmp("unknown_tttr_record.ptu");
    std::fs::write(&path, minimal_ptu(0x0001_0999)).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 3);
    assert_eq!(meta.size_y, 2);
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_type_code_hex"),
        Some(MetadataValue::String(value)) if value == "0x00010999"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.acquisition_mode"),
        Some(MetadataValue::String(value)) if value == "tttr_unknown_record"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.acquisition_mode_ambiguous"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.acquisition_mode_source"),
        Some(MetadataValue::String(value)) if value == "unrecognized TTResultFormat_TTTRRecType"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_type"),
        Some(MetadataValue::String(value)) if value == "Unknown TTTR record type"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_family"),
        Some(MetadataValue::String(value)) if value == "Unknown"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_hydraharp_layout"),
        Some(MetadataValue::Bool(false))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value)) if value.contains("unsupported TTTR record type 0x00010999")
    ));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(
            err,
            BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("unsupported TTTR record type 0x00010999")
        ),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_infers_t2_t3_mode_from_unknown_tttr_record_mode_byte() {
    for (code, mode, label) in [
        (0x0001_0299, "tttr_t2", "Unknown T2 TTTR record type"),
        (0x0001_0399, "tttr_t3", "Unknown T3 TTTR record type"),
    ] {
        let path = tmp(mode);
        std::fs::write(&path, minimal_ptu(code)).unwrap();

        let mut reader = bioformats::formats::spm::PicoQuantReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert!(matches!(
            meta.series_metadata.get("ptu.acquisition_mode"),
            Some(MetadataValue::String(value)) if value == mode
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.acquisition_mode_ambiguous"),
            Some(MetadataValue::Bool(true))
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.acquisition_mode_source"),
            Some(MetadataValue::String(value))
                if value == "inferred from unrecognized TTResultFormat_TTTRRecType mode byte"
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_record_type"),
            Some(MetadataValue::String(value)) if value == label
        ));

        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(
                err,
                BioFormatsError::UnsupportedFormat(ref message)
                    if message.contains(&format!("unsupported TTTR record type 0x{code:08x}"))
            ),
            "{err:?}"
        );

        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn picoquant_ptu_marks_missing_tttr_record_mode_as_unspecified() {
    let path = tmp("missing_tttr_record_type.ptu");
    std::fs::write(&path, minimal_ptu_without_record_type()).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 3);
    assert_eq!(meta.size_y, 2);
    assert!(matches!(
        meta.series_metadata.get("ptu.acquisition_mode"),
        Some(MetadataValue::String(value)) if value == "unspecified"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.acquisition_mode_ambiguous"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.acquisition_mode_source"),
        Some(MetadataValue::String(value)) if value == "missing TTResultFormat_TTTRRecType"
    ));
    assert!(!meta
        .series_metadata
        .contains_key("ptu.tttr_record_type_code_hex"));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value)) if value.contains("missing TTResultFormat_TTTRRecType tag")
    ));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(
            err,
            BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("missing TTResultFormat_TTTRRecType tag")
        ),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_marks_histogram_mode_as_unambiguous() {
    let path = tmp("histogram_mode.ptu");
    std::fs::write(&path, minimal_histogram_ptu()).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 4);
    assert_eq!(meta.size_y, 1);
    assert!(matches!(
        meta.series_metadata.get("ptu.acquisition_mode"),
        Some(MetadataValue::String(value)) if value == "histogram"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.acquisition_mode_ambiguous"),
        Some(MetadataValue::Bool(false))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.acquisition_mode_source"),
        Some(MetadataValue::String(value)) if value == "HistResDscr metadata"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_bins"),
        Some(MetadataValue::Int(4))
    ));

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_recognizes_histo_result_histogram_metadata_variant() {
    let path = tmp("histo_result_metadata.ptu");
    std::fs::write(&path, minimal_histo_result_ptu(5, 2)).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 5);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.image_count, 1);
    assert!(matches!(
        meta.series_metadata.get("ptu.acquisition_mode"),
        Some(MetadataValue::String(value)) if value == "histogram"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_bins"),
        Some(MetadataValue::Int(5))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_curves"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_expected_bytes"),
        Some(MetadataValue::Int(40))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_actual_bytes"),
        Some(MetadataValue::Int(0))
    ));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("0 payload bytes found")),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_decodes_histo_result_histogram_payload_with_explicit_curve_count() {
    let path = tmp("histo_result_payload.ptu");
    let mut data = minimal_histo_result_ptu(3, 2);
    for value in [1u16, 2, 3, 4, 5, 6] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 3);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.image_count, 2);
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_layout"),
        Some(MetadataValue::String(value)) if value == "little-endian uint16 bins"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_expected_bytes"),
        Some(MetadataValue::Int(12))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_sample_bytes"),
        Some(MetadataValue::Int(2))
    ));

    let curve_0: Vec<u16> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(2)
        .map(|px| u16::from_le_bytes(px.try_into().unwrap()))
        .collect();
    let curve_1: Vec<u16> = reader
        .open_bytes(1)
        .unwrap()
        .chunks_exact(2)
        .map(|px| u16::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(curve_0, vec![1, 2, 3]);
    assert_eq!(curve_1, vec![4, 5, 6]);

    let _ = std::fs::remove_file(path);
}
