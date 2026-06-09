//! Tests for the four misc4 readers ported from Java Bio-Formats:
//! OBF (real sample), I2I (synthetic), and smoke tests for JDCE/PCI detection.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats::common::pixel_type::PixelType;
use bioformats::common::reader::FormatReader;
use bioformats::formats::misc4::{I2iReader, JdceReader, ObfReader, PciReader};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_path(tag: &str, ext: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("bioformats_misc4_{tag}_{nanos}_{n}.{ext}"))
}

// ---------------------------------------------------------------------------
// OBF (real sample, gated on file existence)
// ---------------------------------------------------------------------------
#[test]
fn obf_real_sample_v6() {
    let sample = Path::new("testdata/obf/test-v6-short-write.obf");
    if !sample.exists() {
        eprintln!("skipping: {} not present", sample.display());
        return;
    }

    let mut r = ObfReader::new();
    assert!(r.is_this_type_by_name(sample));

    // Magic-byte detection: "OMAS_BF\n" + 0xFFFF + non-negative version.
    let mut magic = b"OMAS_BF\n".to_vec();
    magic.extend_from_slice(&[0xFF, 0xFF, 0x06, 0x00, 0x00, 0x00]);
    assert!(r.is_this_type_by_bytes(&magic));
    assert!(!r.is_this_type_by_bytes(b"not an obf file at all"));

    r.set_id(sample).expect("OBF set_id should succeed");

    // The sample holds 7 stacks (= series), each 381 x 339, sizeZ=2,
    // sizeC=18, INT16, zlib-compressed and stored in chunks.
    assert_eq!(r.series_count(), 7);
    let meta = r.metadata();
    assert_eq!(meta.size_x, 381);
    assert_eq!(meta.size_y, 339);
    assert_eq!(meta.size_z, 2);
    assert_eq!(meta.size_c, 18);
    assert_eq!(meta.size_t, 1);
    assert_eq!(meta.pixel_type, PixelType::Int16);
    assert_eq!(meta.image_count, 2 * 18);
    assert!(meta.is_little_endian);

    // Plane 0 is fully written and zlib+chunk compressed; it must decode to a
    // full INT16 plane of the right length and be non-trivially populated.
    let plane = r.open_bytes(0).expect("OBF open_bytes(0) should succeed");
    assert_eq!(plane.len(), 381 * 339 * 2);
    assert!(plane.iter().any(|&b| b != 0), "decoded plane is all zeros");

    // A region read must be a strict crop of the full plane.
    let region = r
        .open_bytes_region(0, 0, 0, 10, 1)
        .expect("OBF region read should succeed");
    assert_eq!(region.len(), 10 * 2);
    assert_eq!(&region[..], &plane[..20]);
}

// ---------------------------------------------------------------------------
// I2I (synthetic, mirrors the Java header layout)
// ---------------------------------------------------------------------------

/// Build a minimal 1024-byte I2I header followed by `image_count` planes of
/// raw little-endian INT16 pixels.
fn write_i2i(path: &Path, size_x: u32, size_y: u32, total_z: u32, n: i16, planes: &[i16]) {
    let mut header = vec![0u8; 1024];
    header[0] = b'I'; // INT16 pixel type
    header[1] = b' ';
    let put_dim = |buf: &mut [u8], off: usize, v: u32| {
        let s = format!("{:>6}", v); // 6-char right-justified ASCII
        buf[off..off + 6].copy_from_slice(s.as_bytes());
    };
    put_dim(&mut header, 2, size_x);
    put_dim(&mut header, 8, size_y);
    put_dim(&mut header, 14, total_z);
    header[20] = b'L'; // little-endian (anything but 'B')
    // shorts at offset 21: min, max, x, y, then n at offset 29.
    header[29..31].copy_from_slice(&n.to_le_bytes());

    let mut data = header;
    for s in planes {
        data.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, data).unwrap();
}

#[test]
fn i2i_synthetic_roundtrip() {
    let path = unique_path("i2i", "i2i");
    // 2x2 planes; total stored Z = 6, n = 2 -> sizeZ = 3, sizeT = 2, count = 6.
    let size_x = 2u32;
    let size_y = 2u32;
    let n = 2i16;
    let total_z = 6u32;
    let plane_count = 6usize;
    let mut planes: Vec<i16> = Vec::new();
    for p in 0..plane_count {
        let base = (p as i16) * 10;
        planes.extend_from_slice(&[base, base + 1, base + 2, base + 3]);
    }
    write_i2i(&path, size_x, size_y, total_z, n, &planes);

    let mut r = I2iReader::new();
    assert!(r.is_this_type_by_name(&path));

    r.set_id(&path).expect("I2I set_id should succeed");
    let meta = r.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_z, 3); // 6 / n(2)
    assert_eq!(meta.size_t, 2); // n
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.image_count, 6);
    assert_eq!(meta.pixel_type, PixelType::Int16);
    assert!(meta.is_little_endian);

    // Plane 0 and plane 3 must read back exactly.
    let p0 = r.open_bytes(0).unwrap();
    let expect0: Vec<u8> = [0i16, 1, 2, 3].iter().flat_map(|s| s.to_le_bytes()).collect();
    assert_eq!(p0, expect0);

    let p3 = r.open_bytes(3).unwrap();
    let expect3: Vec<u8> = [30i16, 31, 32, 33]
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
    assert_eq!(p3, expect3);

    // Region crop: bottom-right pixel of plane 0.
    let region = r.open_bytes_region(0, 1, 1, 1, 1).unwrap();
    assert_eq!(region, 3i16.to_le_bytes());

    // Out-of-range plane rejected.
    assert!(r.open_bytes(6).is_err());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn i2i_detection_rejects_bad_pixel_type() {
    let r = I2iReader::new();
    // First byte not in {I,R,C} -> not an I2I file.
    let mut header = vec![0u8; 64];
    header[0] = b'Z';
    header[1] = b' ';
    assert!(!r.is_this_type_by_bytes(&header));

    // Valid type + space + a positive pixel count -> accepted.
    let mut good = vec![0u8; 64];
    good[0] = b'R';
    good[1] = b' ';
    good[2..8].copy_from_slice(b"     4");
    good[8..14].copy_from_slice(b"     4");
    good[14..20].copy_from_slice(b"     1");
    assert!(r.is_this_type_by_bytes(&good));
}

// ---------------------------------------------------------------------------
// PCI / JDCE detection smoke tests (no real samples available)
// ---------------------------------------------------------------------------
#[test]
fn pci_detection() {
    let r = PciReader::new();
    assert!(r.is_this_type_by_name(Path::new("foo.cxd")));
    assert!(r.is_this_type_by_name(Path::new("foo.pci")));
    // OLE2 compound-document magic.
    assert!(r.is_this_type_by_bytes(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1]));
    assert!(!r.is_this_type_by_bytes(b"plain text not ole2"));
}

#[test]
fn jdce_detection_and_bad_json() {
    let r = JdceReader::new();
    assert!(r.is_this_type_by_name(Path::new("plate.jdce")));
    assert!(!r.is_this_type_by_name(Path::new("plate.tif")));

    // A .jdce that is not valid JSON must fail cleanly (no panic).
    let path = unique_path("jdce", "jdce");
    std::fs::write(&path, b"this is not json").unwrap();
    let mut r = JdceReader::new();
    assert!(r.set_id(&path).is_err());
    let _ = std::fs::remove_file(&path);
}
