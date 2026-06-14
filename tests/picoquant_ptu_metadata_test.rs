use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats::{BioFormatsError, FormatReader, MetadataValue};
use flate2::write::{DeflateEncoder, GzEncoder, ZlibEncoder};
use std::io::Write;

const PTU_TAG_INT8: u32 = 0x1000_0008;
const PTU_TAG_EMPTY8: u32 = 0xffff_0008;
const PTU_TAG_ANSI_STRING: u32 = 0x4001_ffff;

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

fn append_ptu_ansi_tag(out: &mut Vec<u8>, ident: &str, value: &str) {
    let mut payload = value.as_bytes().to_vec();
    payload.push(0);
    append_ptu_tag(out, ident, -1, PTU_TAG_ANSI_STRING, payload.len() as i64);
    out.extend_from_slice(&payload);
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

fn picoharp_marker_raster_candidate(record_type: i64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PQTTTR\0\0");
    out.extend_from_slice(b"1.0\0\0\0\0\0");
    append_ptu_int_tag(&mut out, "ImgHdr_PixX", 2);
    append_ptu_int_tag(&mut out, "ImgHdr_PixY", 1);
    append_ptu_int_tag(&mut out, "ImgHdr_Frame", 1);
    append_ptu_int_tag(&mut out, "TTResult_NumberOfRecords", 3);
    append_ptu_int_tag(&mut out, "TTResultFormat_TTTRRecType", record_type);
    append_ptu_int_tag(&mut out, "ImgHdr_LineStart", 1);
    append_ptu_int_tag(&mut out, "ImgHdr_LineStop", 2);
    append_ptu_tag(&mut out, "Header_End", -1, PTU_TAG_EMPTY8, 0);

    for record in [
        0x8000_0000u32 | (1 << 25),
        1,
        0x8000_0000u32 | (2 << 25) | 2,
    ] {
        out.extend_from_slice(&record.to_le_bytes());
    }
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

fn headered_histo_result_ptu(bins: i64, curves: i64, payload_offset: i64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PQTTTR\0\0");
    out.extend_from_slice(b"1.0\0\0\0\0\0");
    append_ptu_int_tag(&mut out, "HistoResult_NumberOfBins", bins);
    append_ptu_int_tag(&mut out, "HistoResult_NumberOfCurves", curves);
    append_ptu_int_tag(&mut out, "HistoResult_DataOffset", payload_offset);
    append_ptu_tag(&mut out, "Header_End", -1, PTU_TAG_EMPTY8, 0);
    out
}

fn compressed_histo_result_ptu(bins: i64, curves: i64, compression: &str) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PQTTTR\0\0");
    out.extend_from_slice(b"1.0\0\0\0\0\0");
    append_ptu_int_tag(&mut out, "HistoResult_NumberOfBins", bins);
    append_ptu_int_tag(&mut out, "HistoResult_NumberOfCurves", curves);
    append_ptu_ansi_tag(&mut out, "HistoResultFormat_Compression", compression);
    append_ptu_tag(&mut out, "Header_End", -1, PTU_TAG_EMPTY8, 0);
    out
}

fn compressed_histo_result_flag_ptu(bins: i64, curves: i64) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PQTTTR\0\0");
    out.extend_from_slice(b"1.0\0\0\0\0\0");
    append_ptu_int_tag(&mut out, "HistoResult_NumberOfBins", bins);
    append_ptu_int_tag(&mut out, "HistoResult_NumberOfCurves", curves);
    append_ptu_int_tag(&mut out, "HistoResult_Compressed", 1);
    append_ptu_tag(&mut out, "Header_End", -1, PTU_TAG_EMPTY8, 0);
    out
}

fn zlib_encode(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = ZlibEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn gzip_encode(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn deflate_encode(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = DeflateEncoder::new(Vec::new(), flate2::Compression::default());
    encoder.write_all(bytes).unwrap();
    encoder.finish().unwrap()
}

fn non_contiguous_indexed_histogram_ptu() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PQTTTR\0\0");
    out.extend_from_slice(b"1.0\0\0\0\0\0");
    append_ptu_int_tag(&mut out, "HistResDscr_HistogramBins", 1);
    append_ptu_tag(&mut out, "HistResDscr_CurveIndex", 0, PTU_TAG_INT8, 0);
    append_ptu_tag(&mut out, "HistResDscr_CurveIndex", 2, PTU_TAG_INT8, 2);
    append_ptu_tag(&mut out, "Header_End", -1, PTU_TAG_EMPTY8, 0);
    out
}

fn mixed_indexed_histogram_bin_count_ptu() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PQTTTR\0\0");
    out.extend_from_slice(b"1.0\0\0\0\0\0");
    append_ptu_tag(&mut out, "HistResDscr_HistogramBins", 0, PTU_TAG_INT8, 2);
    append_ptu_tag(&mut out, "HistResDscr_CurveIndex", 0, PTU_TAG_INT8, 0);
    append_ptu_tag(&mut out, "HistResDscr_HistogramBins", 1, PTU_TAG_INT8, 3);
    append_ptu_tag(&mut out, "HistResDscr_CurveIndex", 1, PTU_TAG_INT8, 1);
    append_ptu_tag(&mut out, "Header_End", -1, PTU_TAG_EMPTY8, 0);
    out
}

fn equal_width_indexed_histogram_ptu() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PQTTTR\0\0");
    out.extend_from_slice(b"1.0\0\0\0\0\0");
    append_ptu_tag(&mut out, "HistResDscr_HistogramBins", 0, PTU_TAG_INT8, 3);
    append_ptu_tag(&mut out, "HistResDscr_CurveIndex", 0, PTU_TAG_INT8, 0);
    append_ptu_tag(&mut out, "HistResDscr_HistogramBins", 1, PTU_TAG_INT8, 3);
    append_ptu_tag(&mut out, "HistResDscr_CurveIndex", 1, PTU_TAG_INT8, 1);
    append_ptu_tag(&mut out, "Header_End", -1, PTU_TAG_EMPTY8, 0);
    out
}

fn indexed_offset_histogram_ptu() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"PQTTTR\0\0");
    out.extend_from_slice(b"1.0\0\0\0\0\0");
    append_ptu_tag(&mut out, "HistResDscr_HistogramBins", 0, PTU_TAG_INT8, 3);
    append_ptu_tag(&mut out, "HistResDscr_BitsPerBin", 0, PTU_TAG_INT8, 16);
    append_ptu_tag(&mut out, "HistResDscr_CurveIndex", 0, PTU_TAG_INT8, 0);
    append_ptu_tag(&mut out, "HistResDscr_DataOffset", 0, PTU_TAG_INT8, 4);
    append_ptu_tag(&mut out, "HistResDscr_HistogramBins", 1, PTU_TAG_INT8, 3);
    append_ptu_tag(&mut out, "HistResDscr_BitsPerBin", 1, PTU_TAG_INT8, 16);
    append_ptu_tag(&mut out, "HistResDscr_CurveIndex", 1, PTU_TAG_INT8, 1);
    append_ptu_tag(&mut out, "HistResDscr_DataOffset", 1, PTU_TAG_INT8, 12);
    append_ptu_tag(&mut out, "Header_End", -1, PTU_TAG_EMPTY8, 0);
    out
}

#[test]
fn picoquant_ptu_rejects_picoharp_marker_raster_candidates_without_hydraharp_inference() {
    for (code, label) in [(0x0001_0203, "PicoHarp T2"), (0x0001_0303, "PicoHarp T3")] {
        let path = tmp(label);
        std::fs::write(&path, picoharp_marker_raster_candidate(code)).unwrap();

        let mut reader = bioformats::formats::spm::PicoQuantReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_record_type"),
            Some(MetadataValue::String(value)) if value == label
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_record_layout"),
            Some(MetadataValue::String(value)) if value == "picoharp"
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_marker_raster_layout"),
            Some(MetadataValue::Bool(false))
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.reconstruction_unsupported"),
            Some(MetadataValue::String(value))
                if value.contains(label)
                    && value.contains("not present in the local Bio-Formats Java reader set")
                    && value.contains("not inferred from HydraHarp-compatible bit packing")
        ));

        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(
                err,
                BioFormatsError::UnsupportedFormat(ref message)
                    if message.contains(label)
                        && message.contains("not inferred from HydraHarp-compatible bit packing")
            ),
            "{err:?}"
        );

        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn picoquant_ptu_recognizes_non_hydraharp_tttr_record_metadata_before_marker_reconstruction() {
    for (code, label, family, mode, layout) in [
        (
            0x0001_0203,
            "PicoHarp T2",
            "PicoHarp",
            "tttr_t2",
            "picoharp",
        ),
        (
            0x0001_0303,
            "PicoHarp T3",
            "PicoHarp",
            "tttr_t3",
            "picoharp",
        ),
        (
            0x0001_0205,
            "TimeHarp 260N T2",
            "TimeHarp 260N",
            "tttr_t2",
            "hydraharp-compatible",
        ),
        (
            0x0001_0305,
            "TimeHarp 260N T3",
            "TimeHarp 260N",
            "tttr_t3",
            "hydraharp-compatible",
        ),
        (
            0x0001_0206,
            "TimeHarp 260P T2",
            "TimeHarp 260P",
            "tttr_t2",
            "hydraharp-compatible",
        ),
        (
            0x0001_0306,
            "TimeHarp 260P T3",
            "TimeHarp 260P",
            "tttr_t3",
            "hydraharp-compatible",
        ),
        (
            0x0001_0207,
            "MultiHarp T2",
            "MultiHarp",
            "tttr_t2",
            "hydraharp-compatible",
        ),
        (
            0x0001_0307,
            "MultiHarp T3",
            "MultiHarp",
            "tttr_t3",
            "hydraharp-compatible",
        ),
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
            meta.series_metadata.get("ptu.tttr_record_layout"),
            Some(MetadataValue::String(value)) if value == layout
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_record_layout_source"),
            Some(MetadataValue::String(value)) if value == "TTResultFormat_TTTRRecType"
        ));
        let expected_layout_provenance = if family == "PicoHarp" {
            "PicoHarp marker-raster pixel reconstruction is disabled"
        } else {
            "supported local HydraHarp-compatible record path"
        };
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_record_layout_provenance"),
            Some(MetadataValue::String(value)) if value.contains(expected_layout_provenance)
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
                        && value.contains("recognized for metadata only")
                        && value.contains("not present in the local Bio-Formats Java reader set")
                        && value.contains("not inferred from HydraHarp-compatible bit packing")
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
            "recognized for metadata only"
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
        meta.series_metadata.get("ptu.tttr_record_layout"),
        Some(MetadataValue::String(value)) if value == "unknown"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_layout_source"),
        Some(MetadataValue::String(value)) if value == "unrecognized TTResultFormat_TTTRRecType"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_layout_provenance"),
        Some(MetadataValue::String(value))
            if value.contains("no T2/T3 mode byte could be inferred")
                && value.contains("record bit layout is not decoded")
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
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_record_layout"),
            Some(MetadataValue::String(value)) if value == "unknown"
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_record_layout_source"),
            Some(MetadataValue::String(value)) if value == "unrecognized TTResultFormat_TTTRRecType"
        ));
        assert!(matches!(
            meta.series_metadata.get("ptu.tttr_record_layout_provenance"),
            Some(MetadataValue::String(value))
                if value.contains("mode byte only")
                    && value.contains("record bit layout is not decoded")
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
        meta.series_metadata.get("ptu.tttr_record_layout"),
        Some(MetadataValue::String(value)) if value == "unspecified"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_layout_source"),
        Some(MetadataValue::String(value)) if value == "missing TTResultFormat_TTTRRecType"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_layout_provenance"),
        Some(MetadataValue::String(value))
            if value.contains("no TTTR record type tag is present")
                && value.contains("record bit layout is not decoded")
    ));
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
            .get("ptu.histogram_supported_payload_bytes"),
        Some(MetadataValue::String(value)) if value == "10, 20, 40"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_actual_bytes"),
        Some(MetadataValue::Int(0))
    ));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(
            err,
            BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("uint8, uint16, or uint32")
                    && message.contains("10, 20, 40 bytes supported")
                    && message.contains("0 payload bytes found")
        ),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_reports_supported_histogram_payload_sizes_for_unmatched_payload() {
    let path = tmp("histogram_payload_size_diagnostic.ptu");
    let mut data = minimal_histogram_ptu();
    data.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_supported_payload_bytes"),
        Some(MetadataValue::String(value)) if value == "4, 8, 16"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_actual_bytes"),
        Some(MetadataValue::Int(6))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value))
            if value.contains("exact contiguous uint8, uint16, or uint32")
                && value.contains("4, 8, 16 bytes supported")
                && value.contains("6 payload bytes found")
    ));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(
            err,
            BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("4, 8, 16 bytes supported")
                    && message.contains("6 payload bytes found")
        ),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_decodes_declared_zlib_histogram_payload() {
    let path = tmp("histogram_declared_zlib_payload.ptu");
    let mut data = compressed_histo_result_ptu(4, 1, "zlib");
    let mut raw = Vec::new();
    for value in [4u16, 3, 2, 1] {
        raw.extend_from_slice(&value.to_le_bytes());
    }
    data.extend_from_slice(&zlib_encode(&raw));
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 4);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.bits_per_pixel, 16);
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_compressed"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_compression"),
        Some(MetadataValue::String(value)) if value == "HistoResultFormat_Compression=zlib"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_signature"),
        Some(MetadataValue::String(value)) if value == "zlib stream"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_compression_codec"),
        Some(MetadataValue::String(value)) if value == "zlib"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_decompressed_payload_bytes"),
        Some(MetadataValue::Int(8))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_layout"),
        Some(MetadataValue::String(value)) if value == "little-endian uint16 bins"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction"),
        Some(MetadataValue::String(value))
            if value.contains("histogram payload decoded")
    ));

    let counts: Vec<u16> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(2)
        .map(|px| u16::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(counts, vec![4, 3, 2, 1]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_decodes_declared_gzip_histogram_payload() {
    let path = tmp("histogram_declared_gzip_payload.ptu");
    let mut data = compressed_histo_result_ptu(3, 1, "gzip");
    let raw = [9u8, 8, 7];
    data.extend_from_slice(&gzip_encode(&raw));
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 3);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.bits_per_pixel, 8);
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_signature"),
        Some(MetadataValue::String(value)) if value == "gzip stream"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_compression_codec"),
        Some(MetadataValue::String(value)) if value == "gzip"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_decompressed_payload_bytes"),
        Some(MetadataValue::Int(3))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_layout"),
        Some(MetadataValue::String(value)) if value == "uint8 bins"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), raw);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_decodes_declared_raw_deflate_histogram_payload() {
    let path = tmp("histogram_declared_raw_deflate_payload.ptu");
    let mut data = compressed_histo_result_ptu(2, 1, "raw deflate");
    let mut raw = Vec::new();
    for value in [31u32, 32] {
        raw.extend_from_slice(&value.to_le_bytes());
    }
    data.extend_from_slice(&deflate_encode(&raw));
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.bits_per_pixel, 32);
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_signature"),
        Some(MetadataValue::String(value)) if value == "unknown payload"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_compression_codec"),
        Some(MetadataValue::String(value)) if value == "deflate"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_decompressed_payload_bytes"),
        Some(MetadataValue::Int(8))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_layout"),
        Some(MetadataValue::String(value)) if value == "little-endian uint32 bins"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), raw);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_reports_integer_compressed_histogram_flag_and_payload_signature() {
    let path = tmp("histogram_integer_compressed_flag.ptu");
    let mut data = compressed_histo_result_flag_ptu(2, 1);
    data.extend_from_slice(&[0x1f, 0x8b, 0x08, 0x00, 0xff]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_compression"),
        Some(MetadataValue::String(value)) if value == "HistoResult_Compressed=1"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_signature"),
        Some(MetadataValue::String(value)) if value == "gzip stream"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_first_bytes"),
        Some(MetadataValue::String(value)) if value == "1f 8b 08 00 ff"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value))
            if value.contains("HistoResult_Compressed=1")
                && value.contains("payload signature gzip stream")
                && value.contains("5 payload bytes found")
    ));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(
            err,
            BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("compressed histogram payload decoding is unsupported")
                    && message.contains("HistoResult_Compressed=1")
        ),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_preserves_non_contiguous_histogram_descriptor_metadata_without_decoding() {
    let path = tmp("histogram_non_contiguous_descriptors.ptu");
    let mut data = non_contiguous_indexed_histogram_ptu();
    for value in [1u32, 2, 3] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 1);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 1);
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_curves"),
        Some(MetadataValue::Int(3))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_indexed_descriptor_indices"),
        Some(MetadataValue::String(value)) if value == "0,2"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_indexed_descriptors_contiguous"),
        Some(MetadataValue::Bool(false))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_descriptor_layout"),
        Some(MetadataValue::String(value)) if value == "non-contiguous indexed descriptors"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_expected_bytes"),
        Some(MetadataValue::Int(12))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_actual_bytes"),
        Some(MetadataValue::Int(12))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value))
            if value.contains("non-contiguous indexed histogram descriptors")
                && value.contains("structured payload interpretation")
                && value.contains("12 payload bytes found")
    ));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(
            err,
            BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("non-contiguous indexed histogram descriptors")
                    && message.contains("structured payload interpretation")
        ),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_preserves_mixed_indexed_histogram_bin_counts_without_decoding() {
    let path = tmp("histogram_mixed_indexed_bins.ptu");
    let mut data = mixed_indexed_histogram_bin_count_ptu();
    for value in [1u32, 2, 3, 4, 5] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 1);
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_curves"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_indexed_descriptor_indices"),
        Some(MetadataValue::String(value)) if value == "0,1"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_indexed_descriptors_contiguous"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_indexed_bin_counts"),
        Some(MetadataValue::String(value)) if value == "2,3"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_indexed_bin_counts_consistent"),
        Some(MetadataValue::Bool(false))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_descriptor_layout"),
        Some(MetadataValue::String(value)) if value == "mixed indexed bin counts"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_actual_bytes"),
        Some(MetadataValue::Int(20))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value))
            if value.contains("mixed indexed histogram bin counts")
                && value.contains("structured payload interpretation")
                && value.contains("20 payload bytes found")
    ));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(
            err,
            BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("mixed indexed histogram bin counts")
                    && message.contains("structured payload interpretation")
        ),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_decodes_equal_width_indexed_histogram_descriptors() {
    let path = tmp("histogram_equal_width_indexed_bins.ptu");
    let mut data = equal_width_indexed_histogram_ptu();
    for value in [11u16, 12, 13, 21, 22, 23] {
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
    assert_eq!(meta.bits_per_pixel, 16);
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_descriptor_layout"),
        Some(MetadataValue::String(value))
            if value == "contiguous equal-width indexed descriptors"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_indexed_bin_counts"),
        Some(MetadataValue::String(value)) if value == "3,3"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_indexed_bin_counts_consistent"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_layout"),
        Some(MetadataValue::String(value)) if value == "little-endian uint16 bins"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_expected_bytes"),
        Some(MetadataValue::Int(12))
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
    assert_eq!(curve_0, vec![11, 12, 13]);
    assert_eq!(curve_1, vec![21, 22, 23]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_decodes_indexed_offset_histogram_payload_with_padding() {
    let path = tmp("histogram_indexed_offsets_with_padding.ptu");
    let mut data = indexed_offset_histogram_ptu();
    data.extend_from_slice(&[0xaa, 0xbb, 0xcc, 0xdd]);
    for value in [101u16, 102, 103] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    data.extend_from_slice(&[0xee, 0xff]);
    for value in [201u16, 202, 203] {
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
    assert_eq!(meta.bits_per_pixel, 16);
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_indexed_payload_offset_indices"),
        Some(MetadataValue::String(value)) if value == "0,1"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_indexed_payload_offsets"),
        Some(MetadataValue::String(value)) if value == "4,12"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_indexed_payload_offsets_contiguous"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_layout"),
        Some(MetadataValue::String(value))
            if value == "indexed-offset little-endian uint16 bins"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_expected_bytes"),
        Some(MetadataValue::Int(12))
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
    assert_eq!(curve_0, vec![101, 102, 103]);
    assert_eq!(curve_1, vec![201, 202, 203]);

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

#[test]
fn picoquant_ptu_decodes_headered_histo_result_payload_from_declared_offset() {
    let path = tmp("histo_result_headered_payload.ptu");
    let mut data = headered_histo_result_ptu(3, 1, 4);
    data.extend_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    for value in [7u16, 8, 9] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 3);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.bits_per_pixel, 16);
    assert!(matches!(
        meta.series_metadata.get("ptu.HistoResult_DataOffset"),
        Some(MetadataValue::Int(4))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_actual_bytes"),
        Some(MetadataValue::Int(10))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_header_bytes"),
        Some(MetadataValue::Int(4))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_sample_actual_bytes"),
        Some(MetadataValue::Int(6))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_supported_payload_bytes"),
        Some(MetadataValue::String(value)) if value == "3, 6, 12"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_expected_bytes"),
        Some(MetadataValue::Int(6))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_layout"),
        Some(MetadataValue::String(value)) if value == "little-endian uint16 bins"
    ));

    let counts: Vec<u16> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(2)
        .map(|px| u16::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(counts, vec![7, 8, 9]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_rejects_histogram_payload_offset_past_payload() {
    let path = tmp("histo_result_bad_payload_offset.ptu");
    let mut data = headered_histo_result_ptu(3, 1, 8);
    data.extend_from_slice(&[1, 2, 3, 4]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(
            err,
            BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("payload offset 8 exceeds payload size 4")
        ),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}
