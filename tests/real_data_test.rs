//! Integration tests against real downloaded sample files in `./testdata`.
//!
//! Each test is skipped (passes, prints SKIP) when its sample file is absent, so
//! the suite stays green without the data. Populate `./testdata` with:
//!   DEST=./testdata ./scripts/download_test_data.sh          # all small samples
//!   DOWNLOAD_LARGE=1 ./scripts/download_ndpi.sh ./testdata   # the >4 GB NDPI
//!
//! A failing (non-skipped) test means a reader could not handle a real file —
//! that is a genuine finding to investigate, not flaky-test noise.

use bioformats::ImageReader;
use std::path::{Path, PathBuf};

fn testdata(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(rel)
}

/// Open `rel` via the auto-detecting `ImageReader`, assert basic metadata
/// sanity, and read one plane (or a region). Returns `Ok(false)` (skip) if the
/// file is absent, `Ok(true)` on success, `Err(msg)` on a real failure.
fn validate(rel: &str, region: Option<(u32, u32, u32, u32)>) -> Result<bool, String> {
    let path = testdata(rel);
    if !path.exists() {
        return Ok(false);
    }
    let mut reader = ImageReader::open(&path).map_err(|e| format!("open: {e}"))?;
    let (sx, sy, ic, sc) = {
        let m = reader.metadata();
        (m.size_x, m.size_y, m.image_count, m.size_c)
    };
    if sx == 0 || sy == 0 {
        return Err(format!("zero dimensions {sx}x{sy}"));
    }
    if ic == 0 {
        return Err("zero image_count".into());
    }
    // Read a bounded region rather than the full plane: whole-slide formats
    // (SVS/SCN/NDPI) have gigapixel full-resolution planes that would allocate
    // gigabytes. A small top-left region exercises the decode path safely.
    let (x, y, w, h) = region.unwrap_or((0, 0, sx.min(256), sy.min(256)));
    let bytes = reader
        .open_bytes_region(0, x, y, w, h)
        .map_err(|e| format!("open_bytes_region({x},{y},{w},{h}): {e}"))?;
    if bytes.is_empty() {
        return Err("plane read returned no bytes".into());
    }
    eprintln!("OK   {rel}: {sx}x{sy} c={sc} planes={ic} -> {} bytes", bytes.len());
    Ok(true)
}

macro_rules! real_data_test {
    ($name:ident, $rel:expr) => {
        #[test]
        fn $name() {
            match validate($rel, None) {
                Ok(true) => {}
                Ok(false) => eprintln!("SKIP {} (not downloaded)", $rel),
                Err(e) => panic!("{}: {}", $rel, e),
            }
        }
    };
}

// Files are downloaded into per-format subdirectories by scripts/download_test_data.sh.
real_data_test!(real_svs, "svs/CMU-1-Small-Region.svs");
real_data_test!(real_scn, "scn/Leica-1.scn");
real_data_test!(real_czi, "czi/Plate1-Blue-A-25.czi");
real_data_test!(real_nd2, "nd2/BF007.nd2");
real_data_test!(real_lif, "lif/PR2729.lif");
real_data_test!(real_ometiff, "ome-tiff/tubhiswt_C0.ome.tif");
real_data_test!(real_lsm, "lsm/colocsample1b.lsm");
real_data_test!(real_dicom_mr, "dicom/MR-MONO2-12-angio-an1.dcm");
real_data_test!(real_dicom_ct, "dicom/CT-MONO2-16-chest.dcm");
real_data_test!(real_flex, "flex/001001000.flex");
real_data_test!(real_imaris, "ims/Convallaria_3C_1T_confocal.ims");
real_data_test!(real_ics, "ics/benchmark_v1.ics");
real_data_test!(real_fits, "fits/WFPC2u5780205r_c0fx.fits");
real_data_test!(real_mrc, "mrc/EMD-2225.map");
real_data_test!(real_nifti, "nifti/zstat1.nii");
real_data_test!(real_amira, "amira/test.am");
real_data_test!(real_deltavision, "dv/P-TRE_12_R3D_D3D.dv");
real_data_test!(real_gatan_dm4, "gatan/SmallMontage0000.dm4");
real_data_test!(real_sdt, "sdt/FocalCheck.sdt");
real_data_test!(real_bdv, "bdv/HisYFP-SPIM.h5");
real_data_test!(real_ndpi_small, "ndpi/CMU-1.ndpi");

/// The >4 GB Hamamatsu NDPI: exercises the 64-bit offset reconstruction
/// (Mechanism A/B). A region near the bottom-right of full resolution is read;
/// those JPEG tiles are stored past 4 GB in the file, so a successful decode
/// validates the offset correction (a wrong/un-wrapped offset would seek to the
/// low 32 bits and fail to JPEG-decode).
#[test]
fn real_ndpi_large_64bit_offset() {
    let path = testdata("Hamamatsu-1.ndpi");
    // Skip unless the full file is present (the download is large and may still
    // be in progress; a partial file would fail spuriously).
    const NDPI_FULL_SIZE: u64 = 6_901_027_524;
    match std::fs::metadata(&path) {
        Ok(m) if m.len() >= NDPI_FULL_SIZE => {}
        Ok(m) => {
            eprintln!(
                "SKIP Hamamatsu-1.ndpi (incomplete: {} of {} bytes)",
                m.len(),
                NDPI_FULL_SIZE
            );
            return;
        }
        Err(_) => {
            eprintln!("SKIP Hamamatsu-1.ndpi (not downloaded; DOWNLOAD_LARGE=1 scripts/download_ndpi.sh)");
            return;
        }
    }
    let mut reader = ImageReader::open(&path).expect("open >4GB NDPI");
    let (sx, sy) = {
        let m = reader.metadata();
        (m.size_x, m.size_y)
    };
    assert!(
        sx > 4096 && sy > 4096,
        "expected a large whole-slide image, got {sx}x{sy}"
    );
    let w = 256.min(sx);
    let h = 256.min(sy);
    let x = sx - w;
    let y = sy - h;
    let bytes = reader
        .open_bytes_region(0, x, y, w, h)
        .expect("read a >4GB-offset region from full resolution");
    assert!(
        !bytes.is_empty(),
        "the >4GB-offset region decoded to zero bytes"
    );
    eprintln!("OK   Hamamatsu-1.ndpi: {sx}x{sy}, decoded {}-byte region at ({x},{y})", bytes.len());
}
