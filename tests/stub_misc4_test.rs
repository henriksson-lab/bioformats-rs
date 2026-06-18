//! Tests for misc4 readers ported from Java Bio-Formats:
//! OBF (real sample), I2I/KLB (synthetic), and smoke tests for JDCE/PCI detection.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats::common::metadata::{DimensionOrder, MetadataValue};
use bioformats::common::pixel_type::PixelType;
use bioformats::common::reader::FormatReader;
use bioformats::formats::misc4::{I2iReader, JdceReader, KlbReader, ObfReader, PciReader};

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
    // Little-endian (anything but 'B').
    header[20] = b'L';
    // Shorts at offset 21: min, max, x, y, then n at offset 29.
    header[21..23].copy_from_slice(&(-10i16).to_le_bytes());
    header[23..25].copy_from_slice(&123i16.to_le_bytes());
    header[25..27].copy_from_slice(&7i16.to_le_bytes());
    header[27..29].copy_from_slice(&9i16.to_le_bytes());
    header[29..31].copy_from_slice(&n.to_le_bytes());
    let history = b"created by Rust parity test";
    header[64..64 + history.len()].copy_from_slice(history);

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
    assert!(matches!(
        meta.series_metadata.get("Minimum intensity value"),
        Some(MetadataValue::Int(-10))
    ));
    assert!(matches!(
        meta.series_metadata.get("Maximum intensity value"),
        Some(MetadataValue::Int(123))
    ));
    assert!(matches!(
        meta.series_metadata.get("Image position X"),
        Some(MetadataValue::Int(7))
    ));
    assert!(matches!(
        meta.series_metadata.get("Image position Y"),
        Some(MetadataValue::Int(9))
    ));
    assert!(matches!(
        meta.series_metadata.get("Image history"),
        Some(MetadataValue::String(s)) if s == "created by Rust parity test"
    ));
    assert!(matches!(
        meta.series_metadata.get("Image history #15"),
        Some(MetadataValue::String(s)) if s.is_empty()
    ));

    // Plane 0 and plane 3 must read back exactly.
    let p0 = r.open_bytes(0).unwrap();
    let expect0: Vec<u8> = [0i16, 1, 2, 3]
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
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
    let mut header = vec![0u8; 1024];
    header[0] = b'Z';
    header[1] = b' ';
    assert!(!r.is_this_type_by_bytes(&header));

    // Valid type + space + a positive pixel count -> accepted.
    let mut short_good = vec![0u8; 64];
    short_good[0] = b'R';
    short_good[1] = b' ';
    short_good[2..8].copy_from_slice(b"     4");
    short_good[8..14].copy_from_slice(b"     4");
    short_good[14..20].copy_from_slice(b"     1");
    assert!(!r.is_this_type_by_bytes(&short_good));

    // Valid type + space + a positive pixel count + full Java header -> accepted.
    let mut good = vec![0u8; 1024];
    good[0] = b'R';
    good[1] = b' ';
    good[2..8].copy_from_slice(b"     4");
    good[8..14].copy_from_slice(b"     4");
    good[14..20].copy_from_slice(b"     1");
    assert!(r.is_this_type_by_bytes(&good));
}

// ---------------------------------------------------------------------------
// KLB (synthetic single-file raw blocks)
// ---------------------------------------------------------------------------

fn le_u16(samples: &[u16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_le_bytes()).collect()
}

fn klb_fixture_bytes(compression_type: u8, offset_override: Option<Vec<u64>>) -> Vec<u8> {
    let dims = [3u32, 2, 2, 1, 1];
    let block_size = [2u32, 1, 1, 1, 1];
    let blocks_per_row = 2usize;
    let blocks_per_col = 2usize;
    let blocks_per_z = blocks_per_row * blocks_per_col;
    let mut payload = Vec::new();
    let mut offsets = Vec::new();

    for z in 0..dims[2] as usize {
        for by in 0..blocks_per_col {
            for bx in 0..blocks_per_row {
                let x0 = bx * block_size[0] as usize;
                let y = by;
                let bw = (block_size[0] as usize).min(dims[0] as usize - x0);
                let mut block_samples = Vec::with_capacity(bw);
                for x in x0..x0 + bw {
                    block_samples.push((z as u16) * 100 + (y as u16) * 10 + x as u16);
                }
                payload.extend_from_slice(&le_u16(&block_samples));
                offsets.push(payload.len() as u64);
            }
        }
    }
    assert_eq!(offsets.len(), blocks_per_z * dims[2] as usize);
    if let Some(custom_offsets) = offset_override {
        offsets = custom_offsets;
    }

    let mut bytes = Vec::new();
    bytes.push(1); // header version
    for dim in dims {
        bytes.extend_from_slice(&dim.to_le_bytes());
    }
    for _ in 0..5 {
        bytes.extend_from_slice(&1.0f32.to_le_bytes());
    }
    bytes.push(1); // UINT16
    bytes.push(compression_type);
    bytes.extend(std::iter::repeat(0u8).take(256));
    for block in block_size {
        bytes.extend_from_slice(&block.to_le_bytes());
    }
    for offset in offsets {
        bytes.extend_from_slice(&offset.to_le_bytes());
    }
    bytes.extend_from_slice(&payload);
    bytes
}

#[test]
fn klb_synthetic_raw_single_file_roundtrip() {
    let path = unique_path("klb_raw", "klb");
    std::fs::write(&path, klb_fixture_bytes(0, None)).unwrap();

    let mut r = KlbReader::new();
    assert!(r.is_this_type_by_name(&path));
    assert!(!r.is_this_type_by_bytes(&klb_fixture_bytes(0, None)[..64]));

    r.set_id(&path).expect("KLB set_id should succeed");
    let meta = r.metadata();
    assert_eq!(meta.size_x, 3);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_z, 2);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.size_t, 1);
    assert_eq!(meta.image_count, 2);
    assert_eq!(meta.pixel_type, PixelType::Uint16);
    assert_eq!(meta.dimension_order, DimensionOrder::XYZCT);
    assert!(meta.is_little_endian);

    assert_eq!(r.open_bytes(0).unwrap(), le_u16(&[0, 1, 2, 10, 11, 12]));
    assert_eq!(
        r.open_bytes(1).unwrap(),
        le_u16(&[100, 101, 102, 110, 111, 112])
    );
    assert_eq!(
        r.open_bytes_region(1, 1, 0, 2, 2).unwrap(),
        le_u16(&[101, 102, 111, 112])
    );
    assert!(r.open_bytes(2).is_err());

    let _ = std::fs::remove_file(&path);
}

#[test]
fn klb_rejects_bad_metadata_before_pixel_read() {
    let unsupported_compression = unique_path("klb_bad_compression", "klb");
    std::fs::write(&unsupported_compression, klb_fixture_bytes(99, None)).unwrap();
    let mut r = KlbReader::new();
    assert!(
        r.set_id(&unsupported_compression).is_err(),
        "unsupported compression should be rejected during set_id"
    );

    let non_monotonic_offsets = unique_path("klb_bad_offsets", "klb");
    std::fs::write(
        &non_monotonic_offsets,
        klb_fixture_bytes(0, Some(vec![4, 2, 6, 8, 10, 12, 14, 16])),
    )
    .unwrap();
    let mut r = KlbReader::new();
    assert!(
        r.set_id(&non_monotonic_offsets).is_err(),
        "non-monotonic block offsets should be rejected during set_id"
    );

    let past_eof_offsets = unique_path("klb_past_eof_offsets", "klb");
    std::fs::write(
        &past_eof_offsets,
        klb_fixture_bytes(0, Some(vec![4, 6, 8, 10, 12, 14, 16, 1_000_000])),
    )
    .unwrap();
    let mut r = KlbReader::new();
    assert!(
        r.set_id(&past_eof_offsets).is_err(),
        "offsets past EOF should be rejected during set_id"
    );

    let _ = std::fs::remove_file(&unsupported_compression);
    let _ = std::fs::remove_file(&non_monotonic_offsets);
    let _ = std::fs::remove_file(&past_eof_offsets);
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
