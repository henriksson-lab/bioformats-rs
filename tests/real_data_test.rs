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
    let (sx, sy, ic, sc, bps) = {
        let m = reader.metadata();
        (
            m.size_x,
            m.size_y,
            m.image_count,
            m.size_c,
            m.pixel_type.bytes_per_sample() as u64,
        )
    };
    if sx == 0 || sy == 0 {
        return Err(format!("zero dimensions {sx}x{sy}"));
    }
    if ic == 0 {
        return Err("zero image_count".into());
    }
    // Memory guard: a bounded region exercises the decode path, but strip-based
    // whole-slide levels (e.g. NDPI full resolution) decode the WHOLE strip —
    // i.e. the entire gigapixel plane — even for a 256x256 crop. Skip the pixel
    // read when the nominal full plane exceeds the budget; metadata is already
    // validated, and pyramidal formats expose smaller levels (as other series)
    // that still get read. This keeps the suite from exhausting RAM.
    let plane_bytes = sx as u64 * sy as u64 * sc.max(1) as u64 * bps;
    const PLANE_BUDGET: u64 = 512 << 20; // 512 MiB
    if plane_bytes > PLANE_BUDGET {
        eprintln!(
            "OK   {rel}: {sx}x{sy} c={sc} planes={ic} (pixel read skipped: full plane ~{} MiB > {} MiB budget)",
            plane_bytes >> 20,
            PLANE_BUDGET >> 20
        );
        return Ok(true);
    }
    let (x, y, w, h) = region.unwrap_or((0, 0, sx.min(256), sy.min(256)));
    let bytes = reader
        .open_bytes_region(0, x, y, w, h)
        .map_err(|e| format!("open_bytes_region({x},{y},{w},{h}): {e}"))?;
    if bytes.is_empty() {
        return Err("plane read returned no bytes".into());
    }
    eprintln!(
        "OK   {rel}: {sx}x{sy} c={sc} planes={ic} -> {} bytes",
        bytes.len()
    );
    Ok(true)
}

fn validate_metadata_only(rel: &str) -> Result<bool, String> {
    let path = testdata(rel);
    if !path.exists() {
        return Ok(false);
    }
    let reader = ImageReader::open(&path).map_err(|e| format!("open: {e}"))?;
    let m = reader.metadata();
    if m.size_x == 0 || m.size_y == 0 {
        return Err(format!("zero dimensions {}x{}", m.size_x, m.size_y));
    }
    if m.image_count == 0 {
        return Err("zero image_count".into());
    }
    eprintln!(
        "OK   {rel}: {}x{} c={} planes={} (metadata-only)",
        m.size_x, m.size_y, m.size_c, m.image_count
    );
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
#[test]
fn real_lif() {
    match validate_metadata_only("lif/PR2729.lif") {
        Ok(true) => {}
        Ok(false) => eprintln!("SKIP lif/PR2729.lif (not downloaded)"),
        Err(e) => panic!("lif/PR2729.lif: {e}"),
    }
}
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

// Formats fetched by scripts/download_biostudies_data.sh (EMBL-EBI BioImage
// Archive / EMPIAR). Skipped until downloaded; a failure here is a genuine
// reader finding (some of these readers may still be stubs).
real_data_test!(real_oib, "oib/cry11_colocalization.oib");
real_data_test!(real_oir, "oir/atg8_fig3a_mip.oir");
real_data_test!(real_zvi, "zvi/fig3d_wt_sting_cd31.zvi");
real_data_test!(real_avi, "avi/cryper2_newborn.avi");
real_data_test!(real_psd, "psd/fgf8_pcw5.psd");
real_data_test!(real_dm3, "dm3/clem_fig3b.dm3");
real_data_test!(real_imagic, "imagic/12409.stpm.hed");
real_data_test!(real_vsi, "vsi/HN 485 HNSCC APOBEC3A-1.1000.vsi");

// Formerly untested formats (small public samples via download_test_data.sh).
real_data_test!(real_pic, "pic/sdub1.pic");
real_data_test!(real_nrrd, "nrrd/dt-helix.nrrd");
real_data_test!(real_spe, "spe/test_000_.spe");
real_data_test!(real_stk, "stk/C0.stk");
real_data_test!(real_sif, "sif/image.sif");
// real_klb omitted — the Rust KLB reader covers bounded single-file layouts,
// but this fixture still needs exact grouping/layout validation before enabling.
real_data_test!(real_jpg, "jpg/scifio-test.jpg");
real_data_test!(real_png, "png/scifio-test.png");
real_data_test!(real_bmp, "bmp/scribble_P_RGB.bmp");
real_data_test!(real_metaimage, "mha/HeadMRVolume.mhd");
real_data_test!(real_oif, "oif/Source Data Figure S5c-d CTRL.oif");

/// The >4 GB Hamamatsu NDPI: exercises the 64-bit ("fake BigTIFF") IFD chain and
/// the marker-driven windowed JPEG read. The header's first-IFD pointer and every
/// next-IFD / value offset wrap mod 2^32 past 4 GB; the reader un-wraps them via
/// the NDPI 64-bit layout (8-byte next pointers + per-entry high-word trailer).
/// Full resolution (188160×101376) is stored as ONE ~4.8 GB JPEG strip, so a
/// region is read using the NDPI restart-marker offset array (tags 65426/65432):
/// only the JPEG header and the intervals overlapping the region are read from
/// disk, never the whole strip — keeping the read bounded (~hundreds of MiB).
///
/// Despite "LARGE" this is now memory-bounded, but it still reads a multi-GB file
/// and takes a few seconds, so it is OPT-IN behind `BIOFORMATS_RS_NDPI_LARGE=1`.
#[test]
fn real_ndpi_large_64bit_offset() {
    if std::env::var("BIOFORMATS_RS_NDPI_LARGE").as_deref() != Ok("1") {
        eprintln!("SKIP real_ndpi_large_64bit_offset (set BIOFORMATS_RS_NDPI_LARGE=1)");
        return;
    }
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
            eprintln!(
                "SKIP Hamamatsu-1.ndpi (not downloaded; DOWNLOAD_LARGE=1 scripts/download_ndpi.sh)"
            );
            return;
        }
    }
    let mut reader = ImageReader::open(&path).expect("open >4GB NDPI (64-bit IFD walk)");
    // Java NDPIReader reports an 8-series pyramid with full resolution 188160×101376.
    let n = reader.series_count();
    assert_eq!(n, 8, "expected 8 NDPI series, got {n}");
    reader.set_series(0).unwrap();
    let (sx, sy) = {
        let m = reader.metadata();
        (m.size_x, m.size_y)
    };
    assert_eq!((sx, sy), (188160, 101376), "full-resolution dimensions");

    // Read a 256×256 region near the bottom-right of full resolution — its JPEG
    // restart intervals live past 4 GB in the strip. A wrong offset/byte-count
    // (un-corrected high word) would seek into the low 32 bits and fail to decode.
    let (w, h) = (256u32, 256u32);
    let (x, y) = (sx - w, sy - h);
    let bytes = reader
        .open_bytes_region(0, x, y, w, h)
        .expect("read a >4GB-offset region from full resolution");
    assert_eq!(
        bytes.len(),
        (w * h * 3) as usize,
        "expected RGB region of {w}x{h}"
    );
    assert!(
        bytes.iter().any(|&b| b != 0),
        "the >4GB-offset region decoded to all zeros"
    );
    eprintln!(
        "OK   Hamamatsu-1.ndpi: {sx}x{sy}, decoded {}-byte region at ({x},{y})",
        bytes.len()
    );
}
