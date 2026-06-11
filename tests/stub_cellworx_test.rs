//! Integration test for the CellWorX / MetaXpress (.HTD + TIFF) reader.
//!
//! Uses a minimal real plate set under `testdata/metaxpress/`. Only the A01/w1
//! TIFF is present on disk; the reader must expose the full well/channel grid
//! parsed from the .HTD and read the one plane that exists. The test is skipped
//! when the sample data is absent.

use std::path::Path;

use bioformats::common::reader::FormatReader;
use bioformats::formats::mias::CellWorxReader;

const HTD: &str = "testdata/metaxpress/BSF018292-1A.HTD";
const A01_W1: &str = "testdata/metaxpress/BSF018292-1A_A01_w1.TIF";

#[test]
fn cellworx_parses_htd_and_reads_present_plane() {
    if !Path::new(HTD).exists() || !Path::new(A01_W1).exists() {
        eprintln!("skipping: {HTD} sample not present");
        return;
    }

    let mut reader = CellWorxReader::new();
    reader.set_id(Path::new(HTD)).expect("set_id on .HTD");

    // HTD: 24 columns x 16 rows, all selected; Sites=FALSE -> 1 field;
    // NWavelengths=2 -> sizeC=2; one series per well x field.
    let expected_series = 24 * 16;
    assert_eq!(
        reader.series_count(),
        expected_series,
        "expected one series per selected well"
    );

    // Per-well dimensions come from the companion TIFF (2048x2048, 16-bit).
    {
        let m = reader.metadata();
        assert_eq!(m.size_x, 2048);
        assert_eq!(m.size_y, 2048);
        assert_eq!(m.size_c, 2, "two wavelengths -> SizeC=2");
        assert_eq!(m.size_z, 1);
        assert_eq!(m.size_t, 1);
        assert_eq!(m.image_count, 2);
        assert_eq!(
            m.pixel_type,
            bioformats::common::pixel_type::PixelType::Uint16
        );
        assert!(m.is_little_endian);
    }

    // Series 0 = well A01. Plane 0 = wavelength 1 (the TIFF that exists).
    reader.set_series(0).expect("set_series(0)");
    let plane = reader.open_bytes(0).expect("read A01 w1 plane");
    let expected_bytes = 2048usize * 2048 * 2; // uint16
    assert_eq!(plane.len(), expected_bytes, "full 16-bit plane size");

    // The pixels should not be all-zero (that is the fallback for missing files).
    assert!(
        plane.iter().any(|&b| b != 0),
        "A01 w1 TIFF exists, so the plane must contain real data"
    );

    // Plane 1 = wavelength 2 (A01_w2 absent) -> graceful zero-fill, same size.
    let plane2 = reader.open_bytes(1).expect("read A01 w2 (missing) plane");
    assert_eq!(plane2.len(), expected_bytes);
    assert!(
        plane2.iter().all(|&b| b == 0),
        "missing companion TIFF must read back as zeros"
    );
}
