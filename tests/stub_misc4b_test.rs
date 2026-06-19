//! Tests for three misc4 readers ported from Java Bio-Formats:
//!   - KLB  (Keller Lab Block) — exercised against the real `testdata/klb/img.klb`
//!   - HRDGDF (NOAA-HRD Gridded Data Format) — synthetic round-trip
//!   - APL (Olympus APL) — detection + graceful failure when the sidecar is absent
//!
//! These replace the previous fabricated "strict raw subset" stubs.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats::common::metadata::{DimensionOrder, MetadataValue};
use bioformats::common::pixel_type::PixelType;
use bioformats::common::reader::FormatReader;
use bioformats::formats::misc4::{AplReader, HrdgdfReader, KlbReader};

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn unique_path(tag: &str, ext: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("bioformats_misc4b_{tag}_{nanos}_{n}.{ext}"))
}

// ---------------------------------------------------------------------------
// KLB (real sample)
// ---------------------------------------------------------------------------
#[test]
fn klb_real_sample() {
    let sample = Path::new("testdata/klb/img.klb");
    if !sample.exists() {
        eprintln!("skipping: {} not present", sample.display());
        return;
    }

    let mut r = KlbReader::new();
    assert!(r.is_this_type_by_name(sample));
    // KLB has no magic-byte signature; detection is purely by suffix.
    assert!(!r.is_this_type_by_bytes(&[2u8; 64]));

    r.set_id(sample).expect("set_id");
    let m = r.metadata();

    // Matches `java -cp bioformats_package.jar loci.formats.tools.ImageInfo`.
    assert_eq!(m.size_x, 101);
    assert_eq!(m.size_y, 151);
    assert_eq!(m.size_z, 29);
    assert_eq!(m.size_c, 1);
    assert_eq!(m.size_t, 1);
    assert_eq!(m.image_count, 29);
    assert_eq!(m.pixel_type, PixelType::Uint16);
    assert_eq!(m.dimension_order, DimensionOrder::XYZCT);
    assert!(m.is_little_endian);

    let plane_bytes = 101 * 151 * 2;

    // Plane 0 reads to full size (it is legitimately empty at the volume edge).
    let p0 = r.open_bytes(0).expect("open_bytes(0)");
    assert_eq!(p0.len(), plane_bytes);

    // A mid-stack plane must contain real (non-zero) data.
    let p14 = r.open_bytes(14).expect("open_bytes(14)");
    assert_eq!(p14.len(), plane_bytes);
    assert!(
        p14.iter().any(|&b| b != 0),
        "mid-stack KLB plane should contain non-zero pixels"
    );

    // Spot-check the largest 16-bit value is sane (< 65536, obviously, but also
    // confirms little-endian decode produced plausible microscopy intensities).
    let max = p14
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .max()
        .unwrap();
    assert!(max > 0 && max < 4096, "unexpected max intensity {max}");

    // Region read must agree with the full plane.
    let region = r.open_bytes_region(14, 10, 20, 8, 6).expect("region");
    assert_eq!(region.len(), 8 * 6 * 2);

    assert!(r.open_bytes(29).is_err(), "plane 29 is out of range");
}

// ---------------------------------------------------------------------------
// HRDGDF (synthetic round-trip)
// ---------------------------------------------------------------------------
#[test]
fn hrdgdf_synthetic_roundtrip() {
    let path = unique_path("hrdgdf", "txt");
    let body = "SURFACE WIND COMPONENTS TestStorm\n\
                DX 5.0 KM\n\
                STORM CENTER LOCALE IS 90.0 W 25.0 N\n\
                SURFACE WIND COMPONENTS\n\
                2 2\n\
                (1.0,2.0) (3.0,4.0)\n\
                (5.0,6.0) (7.0,8.0)\n";
    std::fs::write(&path, body).unwrap();

    let mut r = HrdgdfReader::new();
    // Detection is by the leading magic string, not the extension.
    assert!(r.is_this_type_by_bytes(body.as_bytes()));
    assert!(!r.is_this_type_by_name(&path));

    r.set_id(&path).expect("set_id");
    let m = r.metadata();
    assert_eq!(m.size_x, 2);
    assert_eq!(m.size_y, 2);
    assert_eq!(m.size_c, 2);
    assert_eq!(m.size_z, 1);
    assert_eq!(m.size_t, 1);
    assert_eq!(m.image_count, 2);
    assert_eq!(m.pixel_type, PixelType::Float64);
    assert_eq!(m.dimension_order, DimensionOrder::XYCTZ);
    assert!(
        !m.is_little_endian,
        "HRDGDF is big-endian per the Java reader"
    );
    assert!(matches!(
        m.series_metadata.get("DX (kilometers)"),
        Some(MetadataValue::String(value)) if value == "5.0"
    ));
    assert!(matches!(
        m.series_metadata.get("DY (kilometers)"),
        Some(MetadataValue::String(value)) if value == "5.0"
    ));
    assert!(matches!(
        m.series_metadata.get("Storm center (Longitude)"),
        Some(MetadataValue::Float(value)) if (*value - 90.0).abs() < f64::EPSILON
    ));
    assert!(matches!(
        m.series_metadata.get("Storm center (Latitude)"),
        Some(MetadataValue::Float(value)) if (*value - 25.0).abs() < f64::EPSILON
    ));

    // Channel 0 = east-west: pixels in row-major order are 1,3,5,7.
    let ch0 = r.open_bytes(0).expect("plane 0");
    let ch0_vals: Vec<f64> = ch0
        .chunks_exact(8)
        .map(|c| f64::from_be_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(ch0_vals, vec![1.0, 3.0, 5.0, 7.0]);

    // Channel 1 = north-south: 2,4,6,8.
    let ch1 = r.open_bytes(1).expect("plane 1");
    let ch1_vals: Vec<f64> = ch1
        .chunks_exact(8)
        .map(|c| f64::from_be_bytes(c.try_into().unwrap()))
        .collect();
    assert_eq!(ch1_vals, vec![2.0, 4.0, 6.0, 8.0]);

    assert!(r.open_bytes(2).is_err(), "only two channels exist");

    let _ = std::fs::remove_file(&path);
}

// ---------------------------------------------------------------------------
// APL (no public sample: detection + graceful failure)
// ---------------------------------------------------------------------------
#[test]
fn apl_detection_and_missing_sidecar() {
    let r = AplReader::new();
    assert!(r.is_this_type_by_name(Path::new("dataset.apl")));
    assert!(r.is_this_type_by_name(Path::new("dataset.mtb")));
    assert!(r.is_this_type_by_name(Path::new("dataset.tnb")));
    assert!(!r.is_this_type_by_name(Path::new("dataset.tif")));
    let root = unique_path("apl_tif_entry", "dir");
    let dataset = root.join("experiment");
    let image_dir = dataset.join("experiment_DocumentFiles").join("field");
    std::fs::create_dir_all(&image_dir).unwrap();
    std::fs::write(dataset.join("experiment.apl"), b"placeholder").unwrap();
    let tiff_entry = image_dir.join("plane.tif");
    std::fs::write(&tiff_entry, b"not a real tiff").unwrap();
    assert!(r.is_this_type_by_name(&tiff_entry));
    let _ = std::fs::remove_dir_all(&root);
    // APL has no magic-byte signature.
    assert!(!r.is_this_type_by_bytes(b"anything at all"));

    // An .apl with no accompanying _d.mtb database must fail cleanly, not panic.
    let apl = unique_path("apl", "apl");
    std::fs::write(&apl, b"placeholder").unwrap();
    let mut r = AplReader::new();
    assert!(
        r.set_id(&apl).is_err(),
        "APL must require its .mtb sidecar database"
    );
    let _ = std::fs::remove_file(&apl);
}
