//! Synthetic tests for the IMOD binary model reader (`src/formats/sem.rs`).
//!
//! IMOD model files are big-endian and start with the magic `IMODV1.2`. There
//! is no sample file in the tree, so these tests build minimal-but-faithful
//! model headers in memory and exercise detection, header parsing, and the
//! (blank) plane production that mirrors Java Bio-Formats' `IMODReader`.

use bioformats::common::error::BioFormatsError;
use bioformats::common::pixel_type::PixelType;
use bioformats::common::reader::FormatReader;
use bioformats::formats::sem::ImodReader;
use std::path::PathBuf;

fn tmp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("bioformats_imod_{name}"));
    p
}

fn put_i32(out: &mut Vec<u8>, v: i32) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn put_f32(out: &mut Vec<u8>, v: f32) {
    out.extend_from_slice(&v.to_be_bytes());
}

/// Builds a minimal IMOD model header. `objects` is raw bytes appended after
/// the model header (already encoding the object chunks).
fn imod_header(size_x: i32, size_y: i32, size_z: i32, n_objects: i32, objects: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"IMODV1.2");
    out.extend_from_slice(&[0u8; 128]); // filename
    put_i32(&mut out, size_x);
    put_i32(&mut out, size_y);
    put_i32(&mut out, size_z);
    put_i32(&mut out, n_objects);
    // flags, drawMode, mouseMode, blackLevel, whiteLevel
    for _ in 0..5 {
        put_i32(&mut out, 0);
    }
    // xOffset, yOffset, zOffset, xScale, yScale, zScale
    for _ in 0..3 {
        put_f32(&mut out, 0.0);
    }
    for _ in 0..3 {
        put_f32(&mut out, 1.0);
    }
    // currentObject, currentContour, currentPoint
    for _ in 0..3 {
        put_i32(&mut out, 0);
    }
    // res, thresh
    put_i32(&mut out, 0);
    put_i32(&mut out, 0);
    // pixSize, pixSizeUnits, checksum
    put_f32(&mut out, 1.0);
    put_i32(&mut out, -6); // micrometer
    put_i32(&mut out, 0);
    // alpha, beta, gamma
    for _ in 0..3 {
        put_f32(&mut out, 0.0);
    }
    out.extend_from_slice(objects);
    out
}

/// Encodes one OBJT chunk with no contours and no meshes.
fn empty_object() -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"OBJT");
    out.extend_from_slice(&[0u8; 64]); // object name
    out.extend_from_slice(&[0u8; 64]); // unused
    put_i32(&mut out, 0); // nContours
    put_i32(&mut out, 0); // objFlags
    put_i32(&mut out, 0); // axis
    put_i32(&mut out, 0); // objDrawMode
    put_f32(&mut out, 1.0); // red
    put_f32(&mut out, 0.0); // green
    put_f32(&mut out, 0.0); // blue
    put_i32(&mut out, 0); // pixelRadius
    out.extend_from_slice(&[0u8; 8]); // 8 single-byte fields
    put_i32(&mut out, 0); // nMeshes
    put_i32(&mut out, 0); // nSurfaces
    out
}

#[test]
fn imod_detects_magic_and_extension() {
    let reader = ImodReader::new();
    let data = imod_header(4, 3, 2, 0, &[]);
    assert!(reader.is_this_type_by_bytes(&data));
    assert!(!reader.is_this_type_by_bytes(b"NOTIMOD!"));
    assert!(reader.is_this_type_by_name(std::path::Path::new("model.mod")));
    assert!(!reader.is_this_type_by_name(std::path::Path::new("model.tif")));
}

#[test]
fn imod_parses_header_dimensions_and_blank_planes() {
    let data = imod_header(4, 3, 2, 0, &[]);
    let path = tmp("empty.mod");
    std::fs::write(&path, &data).unwrap();

    let mut reader = ImodReader::new();
    reader.set_id(&path).unwrap();

    assert_eq!(reader.series_count(), 1);
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 4);
    assert_eq!(meta.size_y, 3);
    assert_eq!(meta.size_z, 2);
    assert_eq!(meta.size_c, 3);
    assert_eq!(meta.size_t, 1);
    assert_eq!(meta.image_count, 2);
    assert_eq!(meta.pixel_type, PixelType::Uint8);
    assert!(meta.is_rgb);
    assert!(meta.is_interleaved);
    assert!(!meta.is_little_endian);

    // Rasterization is disabled (as in Java), so planes are blank RGB.
    let plane = reader.open_bytes(0).unwrap();
    assert_eq!(plane.len(), 4 * 3 * 3);
    assert!(plane.iter().all(|&b| b == 0));
    assert!(reader.open_bytes(2).is_err());

    // Region crop keeps the 3-sample RGB layout.
    let region = reader.open_bytes_region(0, 1, 0, 2, 2).unwrap();
    assert_eq!(region.len(), 2 * 2 * 3);
}

#[test]
fn imod_walks_object_chunk() {
    let object = empty_object();
    let data = imod_header(8, 8, 1, 1, &object);
    let path = tmp("oneobj.mod");
    std::fs::write(&path, &data).unwrap();

    let mut reader = ImodReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 8);
    assert_eq!(reader.metadata().image_count, 1);
}

#[test]
fn imod_rejects_bad_magic() {
    let mut data = imod_header(4, 4, 1, 0, &[]);
    data[0..8].copy_from_slice(b"BADMAGIC");
    let path = tmp("bad.mod");
    std::fs::write(&path, &data).unwrap();

    let mut reader = ImodReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(matches!(err, BioFormatsError::Format(_)), "{err:?}");
}

#[test]
fn imod_rejects_truncated_header() {
    let path = tmp("trunc.mod");
    std::fs::write(&path, b"IMODV1.2").unwrap();

    let mut reader = ImodReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(matches!(err, BioFormatsError::Format(_)), "{err:?}");
}
