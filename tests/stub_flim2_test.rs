//! Integration tests for the formerly-stubbed `flim2` readers:
//! Olympus OIR (`OirReader`) and Volocity Library Clipping
//! (`VolocityClippingReader`).

use std::path::{Path, PathBuf};

use bioformats::common::pixel_type::PixelType;
use bioformats::formats::flim2::{OirReader, VolocityClippingReader};
use bioformats::FormatReader;

// ---------------------------------------------------------------------------
// Olympus OIR
// ---------------------------------------------------------------------------

const OIR_SAMPLE: &str = "testdata/oir/atg8_fig3a_mip.oir";

#[test]
fn oir_detects_extension() {
    let r = OirReader::new();
    assert!(r.is_this_type_by_name(Path::new("foo.oir")));
    assert!(r.is_this_type_by_name(Path::new("FOO.OIR")));
    assert!(!r.is_this_type_by_name(Path::new("foo.tif")));
}

#[test]
fn oir_detects_native_magic_only() {
    let r = OirReader::new();
    // Native OLYMPUSRAWFORMAT magic is claimed by bytes...
    assert!(r.is_this_type_by_bytes(b"OLYMPUSRAWFORMAT...."));
    // ...but a bare TIFF header is NOT (handled via extension fallback so we
    // do not hijack generic TIFF detection in the registry).
    assert!(!r.is_this_type_by_bytes(&[0x49, 0x49, 42, 0]));
    assert!(!r.is_this_type_by_bytes(&[0x4d, 0x4d, 0, 42]));
}

#[test]
fn oir_opens_sample_with_correct_dimensions_and_reads_plane0() {
    let path = Path::new(OIR_SAMPLE);
    if !path.exists() {
        eprintln!("skipping: {OIR_SAMPLE} not present");
        return;
    }

    let mut reader = OirReader::new();
    reader.set_id(path).expect("OIR sample should open");

    assert_eq!(reader.series_count(), 1);
    reader.set_series(0).unwrap();

    let meta = reader.metadata().clone();
    // The provided "OIR" is actually a big-endian ImageJ TIFF MIP export
    // (1024x1024, 3 channels, 16-bit). Verified directly from the TIFF/ImageJ
    // structure since the Java OIRReader crashes on this (non-OLYMPUSRAWFORMAT)
    // file.
    assert_eq!(meta.size_x, 1024, "size_x");
    assert_eq!(meta.size_y, 1024, "size_y");
    assert_eq!(meta.size_c, 3, "size_c (ImageJ channels=3)");
    assert_eq!(meta.image_count, 3, "image_count");
    assert_eq!(meta.pixel_type, PixelType::Uint16, "pixel_type");
    assert!(!meta.is_little_endian, "big-endian TIFF");

    // Plane 0 should read the full 16-bit grayscale plane.
    let plane = reader.open_bytes(0).expect("plane 0 should read");
    assert_eq!(plane.len(), 1024 * 1024 * 2, "plane byte count");
    assert!(plane.iter().any(|&b| b != 0), "plane should contain data");

    // All declared planes should be readable.
    for p in 0..meta.image_count {
        let bytes = reader.open_bytes(p).unwrap_or_else(|e| panic!("plane {p}: {e:?}"));
        assert_eq!(bytes.len(), 1024 * 1024 * 2);
    }

    // A small region read should succeed and have the expected size.
    let region = reader.open_bytes_region(0, 10, 10, 32, 16).unwrap();
    assert_eq!(region.len(), 32 * 16 * 2);
}

#[test]
fn oir_rejects_non_oir_non_tiff() {
    let dir = std::env::temp_dir();
    let path = dir.join("bioformats_rs_stub_flim2_bad.oir");
    std::fs::write(&path, b"this is not an oir or tiff file at all").unwrap();
    let mut reader = OirReader::new();
    let err = reader.set_id(&path);
    assert!(err.is_err(), "non-OIR/non-TIFF should be rejected");
    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// Volocity Library Clipping
// ---------------------------------------------------------------------------

fn temp_path(name: &str) -> PathBuf {
    std::env::temp_dir().join(format!("bioformats_rs_stub_flim2_{name}"))
}

/// Build a minimal little-endian, uncompressed Volocity clipping fixture
/// matching the layout parsed by `VolocityClippingReader::set_id`.
fn write_volocity_fixture(path: &Path, w: u32, h: u32, z: u32, pixels: &[u8]) {
    // Keep the file large enough that the LZO auto-detection branch is skipped
    // (Java only probes when sizeX*sizeY*100 >= fileLength).
    let plane_pixels = (w * h) as usize;
    let min_len = plane_pixels * 100 + 16;
    let pixel_offset = 25 + 65; // fp after dims (25) + 65
    let mut data = vec![0u8; min_len.max(pixel_offset + pixels.len() + 8)];

    data[0] = b'I'; // little-endian
    data[5..9].copy_from_slice(b"FFCA");
    data[9..13].copy_from_slice(&0x208u32.to_le_bytes()); // geometry marker
    data[13..17].copy_from_slice(&w.to_le_bytes());
    data[17..21].copy_from_slice(&h.to_le_bytes());
    data[21..25].copy_from_slice(&z.to_le_bytes());
    data[pixel_offset..pixel_offset + pixels.len()].copy_from_slice(pixels);

    std::fs::write(path, &data).unwrap();
}

#[test]
fn volocity_clipping_detection() {
    let r = VolocityClippingReader::new();
    assert!(r.is_this_type_by_name(Path::new("clip.acff")));
    assert!(!r.is_this_type_by_name(Path::new("clip.tif")));
    // Java isThisType(stream) always returns false.
    assert!(!r.is_this_type_by_bytes(b"FFCA....................."));
}

#[test]
fn volocity_clipping_reads_raw_synthetic_fixture() {
    let path = temp_path("raw.acff");
    let pixels: Vec<u8> = (0..12).collect(); // 4x3, uint8
    write_volocity_fixture(&path, 4, 3, 1, &pixels);

    let mut reader = VolocityClippingReader::new();
    reader.set_id(&path).expect("fixture should open");

    let meta = reader.metadata().clone();
    assert_eq!(meta.size_x, 4);
    assert_eq!(meta.size_y, 3);
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.size_t, 1);
    assert_eq!(meta.image_count, 1);
    assert_eq!(meta.pixel_type, PixelType::Uint8);
    assert!(meta.is_little_endian);

    let plane = reader.open_bytes(0).unwrap();
    assert_eq!(plane, pixels);

    // Region: row 0, columns 1..3 -> [1, 2].
    let region = reader.open_bytes_region(0, 1, 0, 2, 1).unwrap();
    assert_eq!(region, vec![1u8, 2u8]);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn volocity_clipping_rejects_bad_magic() {
    let path = temp_path("badmagic.acff");
    let mut data = vec![0u8; 2000];
    data[0] = b'I';
    data[5..9].copy_from_slice(b"XXXX");
    std::fs::write(&path, &data).unwrap();

    let mut reader = VolocityClippingReader::new();
    let err = reader.set_id(&path);
    assert!(err.is_err(), "invalid magic should be rejected");
    let _ = std::fs::remove_file(&path);
}
