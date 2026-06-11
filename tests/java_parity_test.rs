//! Java↔Rust parity harness.
//!
//! For each real file in `./testdata`, this runs the Java Bio-Formats reference
//! (`parity/BfParityOracle.java` against `bioformats_package.jar`) and compares
//! its output to our Rust `ImageReader`, across three axes:
//!   1. CORE metadata   — sizeX/Y/Z/C/T, pixelType, bitsPerPixel, imageCount,
//!                        dimensionOrder, rgb/interleaved/indexed/littleEndian.
//!   2. OME metadata    — image name, physical sizes, time increment, and
//!                        per-channel name / samplesPerPixel / emission / excitation.
//!   3. PIXELS          — read identically on both sides and compared three ways:
//!                        a) CRC32 of a bounded top-left 256² region of up to
//!                           MAX_PLANES planes (deep Z/C/T coverage);
//!                        b) for SMALL planes (full plane <= FULL_PLANE_MAX),
//!                           CRC32 of the WHOLE plane (catches corners the crop
//!                           misses);
//!                        c) one NON-ZERO-ORIGIN (centered) 256² region of plane
//!                           0 (catches tiling/stride/offset bugs).
//!
//! Gating (so plain `cargo test` is unaffected):
//!   - Skips unless env `BIOFORMATS_RS_JAVA_PARITY=1`.
//!   - Skips if `bioformats_package.jar`, `java`, or `javac` are absent.
//!   - Skips any file missing from `./testdata`.
//!
//! Run:  BIOFORMATS_RS_JAVA_PARITY=1 cargo test --test java_parity_test -- --nocapture
//!
//! By default the test FAILS only on CORE-metadata divergence (the baseline
//! contract). OME and pixel-CRC parity are printed as a scored report. Set
//! `BIOFORMATS_RS_JAVA_PARITY_STRICT=1` to also fail on OME/pixel divergence.

use bioformats::common::metadata::DimensionOrder;
use bioformats::common::pixel_type::PixelType;
use bioformats::ImageReader;
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

/// Files to compare (relative to ./testdata). Mirrors real_data_test coverage.
const FILES: &[&str] = &[
    "ome-tiff/tubhiswt_C0.ome.tif",
    "lsm/colocsample1b.lsm",
    "nd2/BF007.nd2",
    "czi/Plate1-Blue-A-25.czi",
    "lif/PR2729.lif",
    "dicom/MR-MONO2-12-angio-an1.dcm",
    "dicom/CT-MONO2-16-chest.dcm",
    "fits/WFPC2u5780205r_c0fx.fits",
    "mrc/EMD-2225.map",
    "nifti/zstat1.nii",
    "amira/test.am",
    "ics/benchmark_v1.ics",
    "flex/001001000.flex",
    "ims/Convallaria_3C_1T_confocal.ims",
    "svs/CMU-1-Small-Region.svs",
    "scn/Leica-1.scn",
    "ndpi/CMU-1.ndpi",
    "dv/P-TRE_12_R3D_D3D.dv",
    "gatan/SmallMontage0000.dm4",
    "sdt/FocalCheck.sdt",
    "bdv/HisYFP-SPIM.h5",
    // BioImage Archive set:
    "oib/cry11_colocalization.oib",
    "oif/Source Data Figure S5c-d CTRL.oif",
    "zvi/fig3d_wt_sting_cd31.zvi",
    "avi/cryper2_newborn.avi",
    "psd/sample_rgb.psd",
    "dm3/clem_fig3b.dm3",
    "imagic/12409.stpm.hed",
    "vsi/HN 485 HNSCC APOBEC3A-1.1000.vsi",
    // Formerly untested formats (small public samples):
    "pic/sdub1.pic",
    "nrrd/dt-helix.nrrd",
    "spe/test_000_.spe",
    "stk/C0.stk",
    "sif/image.sif",
    // klb/img.klb omitted — the Rust KLB reader covers bounded single-file layouts,
    // but this fixture still needs a Java parity oracle for its exact grouping/layout.
    "jpg/scifio-test.jpg",
    "png/scifio-test.png",
    "bmp/scribble_P_RGB.bmp",
    // NOTE: testdata/mha/HeadMRVolume.mhd is NOT here — Java Bio-Formats has no
    // MetaImage reader, so there is no oracle to compare against (Rust-only).
];

/// Upper bound on planes compared per series. Raised from 8 so deep Z/C/T
/// stacks are exercised; still bounded so runtime/RAM stay reasonable. When a
/// series has fewer planes than this, all of them are compared.
const MAX_PLANES: u32 = 64;
const REGION: u32 = 256;

fn testdata(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("testdata")
        .join(rel)
}

fn jar_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("bioformats_package.jar")
}

/// Max per-byte absolute difference tolerated when the exact CRC differs.
/// JPEG-compressed tiles decode through a pure-Rust IDCT + YCbCr→RGB path that
/// differs from libjpeg-turbo by a few levels per sample (observed ≤3 on
/// SVS/SCN/NDPI); that is accepted as a "tolerant" match (reported separately)
/// rather than a hard failure. Genuine decode bugs differ by 100s of levels
/// (e.g. the bdv scaleoffset-HDF5 case), so they remain hard failures.
const PIXEL_TOL: u8 = 5;

/// Minimal standard-alphabet base64 decoder (no padding-strictness needed).
fn b64_decode(s: &str) -> Vec<u8> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        let Some(v) = val(c) else { continue };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn pixel_type_to_java(pt: PixelType) -> &'static str {
    match pt {
        PixelType::Int8 => "int8",
        PixelType::Uint8 => "uint8",
        PixelType::Int16 => "int16",
        PixelType::Uint16 => "uint16",
        PixelType::Int32 => "int32",
        PixelType::Uint32 => "uint32",
        PixelType::Float32 => "float",
        PixelType::Float64 => "double",
        PixelType::Bit => "bit",
    }
}

fn dim_order_str(d: DimensionOrder) -> &'static str {
    match d {
        DimensionOrder::XYCTZ => "XYCTZ",
        DimensionOrder::XYCZT => "XYCZT",
        DimensionOrder::XYTCZ => "XYTCZ",
        DimensionOrder::XYTZC => "XYTZC",
        DimensionOrder::XYZCT => "XYZCT",
        DimensionOrder::XYZTC => "XYZTC",
    }
}

/// Compile the Java oracle once; return its class dir, or None if unavailable.
fn oracle_classpath() -> Option<&'static str> {
    static CP: OnceLock<Option<String>> = OnceLock::new();
    CP.get_or_init(|| {
        let jar = jar_path();
        if !jar.exists() {
            eprintln!("SKIP parity: {} not found", jar.display());
            return None;
        }
        if Command::new("java").arg("-version").output().is_err()
            || Command::new("javac").arg("-version").output().is_err()
        {
            eprintln!("SKIP parity: java/javac not available");
            return None;
        }
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("parity/BfParityOracle.java");
        let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("parity/target");
        std::fs::create_dir_all(&out).ok()?;
        let status = Command::new("javac")
            .arg("-cp")
            .arg(&jar)
            .arg(&src)
            .arg("-d")
            .arg(&out)
            .output()
            .ok()?;
        if !status.status.success() {
            eprintln!(
                "SKIP parity: oracle compile failed:\n{}",
                String::from_utf8_lossy(&status.stderr)
            );
            return None;
        }
        Some(format!("{}:{}", jar.display(), out.display()))
    })
    .as_deref()
}

fn run_oracle(cp: &str, path: &Path, max_planes: u32, full_plane: bool) -> Option<Value> {
    let out = Command::new("java")
        .arg("-cp")
        .arg(cp)
        .arg("BfParityOracle")
        .arg(path)
        .arg(max_planes.to_string())
        .arg(REGION.to_string())
        .arg(if full_plane { "1" } else { "0" })
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().find(|l| l.trim_start().starts_with('{'))?;
    serde_json::from_str(line).ok()
}

/// Files where the oracle must NOT do whole-plane `openBytes` reads: those reads
/// hard-crash the old libhdf5 (1.10.5) that Bio-Formats bundles when they hit a
/// full-precision scaleoffset chunk (see JAVA_LIBHDF5_DIVERGENCE / bioformats_bug.txt),
/// producing no oracle output. The bounded crop + offset reads still work, so
/// disabling full-plane reads keeps the file comparable (and ⚠-classified).
fn oracle_no_full_plane(rel: &str) -> bool {
    rel.contains("bdv/")
}

/// Per-file plane cap. bdv has 34 series and our HDF5 reader does an uncached
/// chunk decode per region, so deep coverage takes ~an hour for one file; a tiny
/// cap still exercises core+OME parity and the ⚠ Java-bug planes (s31/s32) while
/// keeping runtime sane. Everything else uses the full MAX_PLANES depth.
fn oracle_max_planes(rel: &str) -> u32 {
    if rel.contains("bdv/") {
        2
    } else {
        MAX_PLANES
    }
}

#[derive(Default)]
struct Score {
    core_ok: u32,
    core_bad: u32,
    ome_ok: u32,
    ome_bad: u32,
    px_exact: u32,    // series whose planes all matched Java bitwise
    px_tol: u32,      // series that passed only within PIXEL_TOL (e.g. JPEG IDCT)
    px_bad: u32,      // series with a real pixel divergence
    px_java_div: u32, // series where Java itself is wrong (see JAVA_LIBHDF5_DIVERGENCE)
}

/// (file substring, series indices) where the divergence is JAVA's fault, not
/// ours, so a pixel mismatch is reported — not failed.
///
/// Bio-Formats reads HDF5 via libhdf5 (JNI). libhdf5 1.14.5 has an off-by-one in
/// H5Zscaleoffset.c (`minbits >= size*8` should be `>`) that rejects/mis-handles
/// full-precision (minbits==16) scaleoffset chunks, which these BDV setup-8
/// pyramid levels contain. Our pure-Rust decode is verified byte-exact against an
/// independent reconstruction (see the hdf5-pure-rust repo's hdf5.txt) — so where
/// we differ from Java here, Java is the wrong side.
const JAVA_LIBHDF5_DIVERGENCE: &[(&str, &[usize])] = &[("bdv/HisYFP-SPIM.h5", &[31, 32])];

fn is_known_java_divergence(rel: &str, series: usize) -> bool {
    JAVA_LIBHDF5_DIVERGENCE
        .iter()
        .any(|(file, idxs)| rel.contains(file) && idxs.contains(&series))
}

fn jf64(v: &Value) -> Option<f64> {
    if v.is_null() {
        None
    } else {
        v.as_f64()
    }
}

fn approx(a: Option<f64>, b: Option<f64>) -> bool {
    match (a, b) {
        (None, None) => true,
        (Some(x), Some(y)) => (x - y).abs() <= 1e-6 + 1e-6 * x.abs().max(y.abs()),
        _ => false,
    }
}

#[test]
fn java_parity() {
    if std::env::var("BIOFORMATS_RS_JAVA_PARITY").as_deref() != Ok("1") {
        eprintln!("SKIP parity: set BIOFORMATS_RS_JAVA_PARITY=1 to run (needs the jar + java).");
        return;
    }
    let strict = std::env::var("BIOFORMATS_RS_JAVA_PARITY_STRICT").as_deref() == Ok("1");
    // Optional comma-separated substring filter, so a worker can verify just its
    // own files quickly: BIOFORMATS_RS_JAVA_PARITY_FILES="lsm/,nd2/"
    let filter = std::env::var("BIOFORMATS_RS_JAVA_PARITY_FILES").unwrap_or_default();
    let filters: Vec<&str> = filter
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    let Some(cp) = oracle_classpath() else { return };

    let mut score = Score::default();
    let mut core_failures: Vec<String> = Vec::new();
    let mut hard_failures: Vec<String> = Vec::new();
    // Pixel divergences caught ONLY by the new deeper checks (deep planes,
    // whole-plane CRC, or offset region) where the old first-8/256² sampling
    // would have reported a clean pass.
    let mut new_findings: Vec<String> = Vec::new();
    let mut checked = 0u32;

    for rel in FILES {
        if !filters.is_empty() && !filters.iter().any(|f| rel.contains(f)) {
            continue;
        }
        let path = testdata(rel);
        if !path.exists() {
            eprintln!("skip (absent): {rel}");
            continue;
        }
        let Some(j) = run_oracle(
            cp,
            &path,
            oracle_max_planes(rel),
            !oracle_no_full_plane(rel),
        ) else {
            eprintln!("skip (oracle no output): {rel}");
            continue;
        };
        if j.get("ok").and_then(Value::as_bool) != Some(true) {
            // Java itself failed to read it — not a parity gap on our side.
            eprintln!(
                "skip ({rel}): Java reader error: {}",
                j.get("error").and_then(Value::as_str).unwrap_or("?")
            );
            continue;
        }
        checked += 1;
        println!("\n══ {rel} ══");

        let mut reader = match ImageReader::open(&path) {
            Ok(r) => r,
            Err(e) => {
                println!("  RUST open FAILED: {e}");
                core_failures.push(format!("{rel}: rust open failed: {e}"));
                hard_failures.push(rel.to_string());
                continue;
            }
        };

        // ---- series count ----
        let jseries = j.get("seriesCount").and_then(Value::as_u64).unwrap_or(0) as usize;
        let rseries = reader.series_count();
        if jseries != rseries {
            println!("  seriesCount: java={jseries} rust={rseries}  ✗");
            core_failures.push(format!("{rel}: seriesCount java={jseries} rust={rseries}"));
            hard_failures.push(rel.to_string());
        } else {
            println!("  seriesCount={rseries} ✓");
        }

        let empty = Vec::new();
        let jseries_arr = j.get("series").and_then(Value::as_array).unwrap_or(&empty);
        for (si, js) in jseries_arr.iter().enumerate() {
            if si >= rseries {
                break;
            }
            if reader.set_series(si).is_err() {
                core_failures.push(format!("{rel} s{si}: rust set_series failed"));
                hard_failures.push(rel.to_string());
                continue;
            }
            let m = reader.metadata().clone();

            // ---- core metadata ----
            let mut core_diffs: Vec<String> = Vec::new();
            let cmp_u = |name: &str, jv: u64, rv: u64, out: &mut Vec<String>| {
                if jv != rv {
                    out.push(format!("{name}: java={jv} rust={rv}"));
                }
            };
            cmp_u(
                "sizeX",
                js["sizeX"].as_u64().unwrap_or(0),
                m.size_x as u64,
                &mut core_diffs,
            );
            cmp_u(
                "sizeY",
                js["sizeY"].as_u64().unwrap_or(0),
                m.size_y as u64,
                &mut core_diffs,
            );
            cmp_u(
                "sizeZ",
                js["sizeZ"].as_u64().unwrap_or(0),
                m.size_z as u64,
                &mut core_diffs,
            );
            cmp_u(
                "sizeC",
                js["sizeC"].as_u64().unwrap_or(0),
                m.size_c as u64,
                &mut core_diffs,
            );
            cmp_u(
                "sizeT",
                js["sizeT"].as_u64().unwrap_or(0),
                m.size_t as u64,
                &mut core_diffs,
            );
            cmp_u(
                "imageCount",
                js["imageCount"].as_u64().unwrap_or(0),
                m.image_count as u64,
                &mut core_diffs,
            );
            if js["pixelType"].as_str() != Some(pixel_type_to_java(m.pixel_type)) {
                core_diffs.push(format!(
                    "pixelType: java={} rust={}",
                    js["pixelType"].as_str().unwrap_or("?"),
                    pixel_type_to_java(m.pixel_type)
                ));
            }
            if js["dimensionOrder"].as_str() != Some(dim_order_str(m.dimension_order)) {
                core_diffs.push(format!(
                    "dimensionOrder: java={} rust={}",
                    js["dimensionOrder"].as_str().unwrap_or("?"),
                    dim_order_str(m.dimension_order)
                ));
            }
            // Bit depth, endianness, rgb/indexed flags (informational-but-core).
            if js["bitsPerPixel"].as_u64().unwrap_or(0) != m.bits_per_pixel as u64 {
                core_diffs.push(format!(
                    "bitsPerPixel: java={} rust={}",
                    js["bitsPerPixel"], m.bits_per_pixel
                ));
            }
            if js["littleEndian"].as_bool() != Some(m.is_little_endian) {
                core_diffs.push(format!(
                    "littleEndian: java={} rust={}",
                    js["littleEndian"], m.is_little_endian
                ));
            }
            if js["rgb"].as_bool() != Some(m.is_rgb) {
                core_diffs.push(format!("rgb: java={} rust={}", js["rgb"], m.is_rgb));
            }

            if core_diffs.is_empty() {
                println!(
                    "  s{si} core ✓  {}x{} z{} c{} t{} {} ic={}",
                    m.size_x,
                    m.size_y,
                    m.size_z,
                    m.size_c,
                    m.size_t,
                    pixel_type_to_java(m.pixel_type),
                    m.image_count
                );
                score.core_ok += 1;
            } else {
                println!("  s{si} core ✗  {}", core_diffs.join("; "));
                score.core_bad += 1;
                core_failures.push(format!("{rel} s{si}: {}", core_diffs.join("; ")));
                hard_failures.push(rel.to_string());
            }

            // ---- pixels: multi-region compare per plane ----
            // For every checked plane we compare, exact-first then tolerant:
            //   a) the top-left 256² crop (deep Z/C/T coverage, up to MAX_PLANES);
            //   b) for SMALL planes (Java emitted fullCrc), the WHOLE plane — so
            //      divergences outside the top-left corner are caught;
            //   c) one centered (non-zero-origin) 256² crop of plane 0 — so
            //      tiling/stride/offset bugs are caught.
            // Exact CRC is primary; on mismatch we fall back to a per-sample
            // tolerance compare against Java's raw bytes (base64) so JPEG IDCT
            // rounding (≤PIXEL_TOL) is a "tolerant" pass, while any larger
            // divergence is a hard fail. All three checks fold into one per-series
            // bucket (bitwise / tolerant / ⚠ Java-bug / ✗).
            //
            // Memory guard: strip-based whole-slide levels decode the entire
            // gigapixel plane even for a 256px crop. Skip the pixel compare for
            // series whose nominal full plane exceeds the budget (core+OME still
            // compared, and smaller pyramid levels — separate series — are
            // compared), so the harness can't exhaust RAM. The whole-plane (b)
            // reads only happen where Java tagged the plane small (<= 4 MiB),
            // so they never blow the budget.
            let plane_bytes = m.size_x as u64
                * m.size_y as u64
                * m.size_c.max(1) as u64
                * m.pixel_type.bytes_per_sample() as u64;
            const PLANE_BUDGET: u64 = 512 << 20; // 512 MiB
            let budget_ok = plane_bytes <= PLANE_BUDGET;
            let planes = if budget_ok {
                js["planeCrc"].as_array().cloned().unwrap_or_default()
            } else {
                println!(
                    "  s{si} pixels — skipped (full plane ~{} MiB > {} MiB budget)",
                    plane_bytes >> 20,
                    PLANE_BUDGET >> 20
                );
                Vec::new()
            };
            let mut px_total = 0usize;
            let mut px_exact = 0usize;
            let mut px_tol = 0usize;
            let mut worst_tol = 0u8;
            let mut full_checks = 0usize; // how many whole-plane (b) checks ran
            let mut off_checks = 0usize; // how many offset-region (c) checks ran
                                         // Track first divergence separately for the OLD sampling envelope
                                         // (top-left 256² crop of planes 0..OLD_MAX_PLANES) vs the NEW deeper
                                         // checks (crop of deep planes, whole-plane CRC, offset region), so
                                         // the report can attribute findings the old harness would have
                                         // missed. `first_px_diff` is the first across both (for the line).
            const OLD_MAX_PLANES: u32 = 8;
            let mut first_px_diff: Option<String> = None;
            let mut first_old_diff: Option<String> = None;
            let mut first_new_diff: Option<String> = None;

            // Outcome of one region compare against Java's CRC/len/(b64).
            enum Out {
                Exact,
                Tol(u8),
                Bad(String),
            }
            // Fold one check's outcome into the per-series tallies. `$new`
            // marks checks outside the OLD sampling envelope.
            macro_rules! record {
                ($out:expr, $new:expr) => {{
                    match $out {
                        Out::Exact => px_exact += 1,
                        Out::Tol(d) => {
                            px_tol += 1;
                            worst_tol = worst_tol.max(d);
                        }
                        Out::Bad(msg) => {
                            if first_px_diff.is_none() {
                                first_px_diff = Some(msg.clone());
                            }
                            if $new {
                                if first_new_diff.is_none() {
                                    first_new_diff = Some(msg);
                                }
                            } else if first_old_diff.is_none() {
                                first_old_diff = Some(msg);
                            }
                        }
                    }
                }};
            }
            let cmp = |rbuf: &[u8], jcrc: u64, jlen: u64, jb64: Option<&str>, label: &str| -> Out {
                let rcrc = crc32_ieee(rbuf) as u64;
                if rcrc == jcrc && rbuf.len() as u64 == jlen {
                    return Out::Exact;
                }
                if let Some(jb) = jb64 {
                    let jbytes = b64_decode(jb);
                    if jbytes.len() == rbuf.len() {
                        let maxd = jbytes
                            .iter()
                            .zip(rbuf)
                            .map(|(a, b)| a.abs_diff(*b))
                            .max()
                            .unwrap_or(0);
                        if maxd <= PIXEL_TOL {
                            return Out::Tol(maxd);
                        }
                        let ndiff = jbytes.iter().zip(rbuf).filter(|(a, b)| a != b).count();
                        return Out::Bad(format!(
                            "{label}: maxdiff={maxd} over {ndiff}/{} bytes",
                            rbuf.len()
                        ));
                    }
                }
                Out::Bad(format!(
                    "{label}: java(len={jlen},crc={jcrc}) rust(len={},crc={rcrc})",
                    rbuf.len()
                ))
            };

            for pj in &planes {
                if pj.get("error").is_some() {
                    continue; // Java couldn't read this plane either
                }
                let p = pj["plane"].as_u64().unwrap_or(0) as u32;
                let w = pj["w"].as_u64().unwrap_or(0) as u32;
                let h = pj["h"].as_u64().unwrap_or(0) as u32;

                // (a) top-left 256² crop. New coverage only for deep planes
                // (>= OLD_MAX_PLANES) the old first-8 sampling never reached.
                px_total += 1;
                let crop_is_new = p >= OLD_MAX_PLANES;
                let out = match reader.open_bytes_region(p, 0, 0, w, h) {
                    Ok(buf) => cmp(
                        &buf,
                        pj["crc"].as_u64().unwrap_or(u64::MAX),
                        pj["len"].as_u64().unwrap_or(0),
                        pj["b64"].as_str(),
                        &format!("plane{p} crop"),
                    ),
                    Err(e) => Out::Bad(format!("plane{p} crop: rust read error: {e}")),
                };
                record!(out, crop_is_new);

                // (b) whole-plane CRC — only where Java emitted it (small
                // planes). Always NEW coverage (the old harness never read
                // beyond the 256² crop).
                if let Some(fcrc) = pj["fullCrc"].as_u64() {
                    px_total += 1;
                    full_checks += 1;
                    let out = match reader.open_bytes(p) {
                        Ok(buf) => cmp(
                            &buf,
                            fcrc,
                            pj["fullLen"].as_u64().unwrap_or(0),
                            pj["fullB64"].as_str(),
                            &format!("plane{p} FULL"),
                        ),
                        Err(e) => Out::Bad(format!("plane{p} FULL: rust read error: {e}")),
                    };
                    record!(out, true);
                }
            }

            // (c) one centered, non-zero-origin 256² crop of plane 0.
            let offset = js.get("offset");
            if budget_ok {
                if let Some(oj) = offset {
                    if oj.get("error").is_none() && oj.get("crc").is_some() {
                        let p = oj["plane"].as_u64().unwrap_or(0) as u32;
                        let ox = oj["ox"].as_u64().unwrap_or(0) as u32;
                        let oy = oj["oy"].as_u64().unwrap_or(0) as u32;
                        let w = oj["w"].as_u64().unwrap_or(0) as u32;
                        let h = oj["h"].as_u64().unwrap_or(0) as u32;
                        px_total += 1;
                        off_checks += 1;
                        let out = match reader.open_bytes_region(p, ox, oy, w, h) {
                            Ok(buf) => cmp(
                                &buf,
                                oj["crc"].as_u64().unwrap_or(u64::MAX),
                                oj["len"].as_u64().unwrap_or(0),
                                oj["b64"].as_str(),
                                &format!("plane{p} offset({ox},{oy})"),
                            ),
                            Err(e) => Out::Bad(format!(
                                "plane{p} offset({ox},{oy}): rust read error: {e}"
                            )),
                        };
                        record!(out, true); // offset region is NEW coverage
                    }
                }
            }

            if px_total > 0 {
                let passed = px_exact + px_tol;
                let coverage =
                    format!("{px_total} checks [crop+{full_checks} full+{off_checks} offset]");
                if passed == px_total && px_tol == 0 {
                    println!("  s{si} pixels ✓  {px_exact}/{coverage} bitwise");
                    score.px_exact += 1;
                } else if passed == px_total {
                    println!(
                        "  s{si} pixels ≈  {px_exact} bitwise + {px_tol} within ±{worst_tol} (JPEG IDCT) / {coverage}"
                    );
                    score.px_tol += 1;
                } else if is_known_java_divergence(rel, si) {
                    // Java (libhdf5) is the wrong side here; our decode is verified.
                    println!(
                        "  s{si} pixels ⚠ Java-divergent ({passed}/{coverage}) — libhdf5 scaleoffset off-by-one; our decode verified correct"
                    );
                    score.px_java_div += 1;
                } else {
                    println!(
                        "  s{si} pixels ✗  {passed}/{coverage} ok — {}",
                        first_px_diff.as_deref().unwrap_or("")
                    );
                    // Attribute the divergence: surfaced ONLY by the new deeper
                    // checks (deep crop / whole-plane / offset) when the old
                    // first-8/256² envelope was clean.
                    if first_old_diff.is_none() {
                        if let Some(nd) = &first_new_diff {
                            println!(
                                "       ↳ NEW-coverage finding (old sampling was clean): {nd}"
                            );
                            new_findings.push(format!("{rel} s{si}: {nd}"));
                        }
                    } else if let Some(nd) = &first_new_diff {
                        // Old sampling also diverged, but record the new one too.
                        println!(
                            "       ↳ (old sampling also diverged: {})",
                            first_old_diff.as_deref().unwrap_or("")
                        );
                        let _ = nd;
                    }
                    score.px_bad += 1;
                    if strict {
                        hard_failures.push(rel.to_string());
                    }
                }
            }
        }

        // ---- OME metadata (compared at image granularity) ----
        let jome = j.get("ome").cloned().unwrap_or(Value::Null);
        if let Some(rome) = reader.ome_metadata() {
            let jimgs = jome
                .get("images")
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let mut ome_diffs: Vec<String> = Vec::new();
            if jimgs.len() != rome.images.len() {
                ome_diffs.push(format!(
                    "image count: java={} rust={}",
                    jimgs.len(),
                    rome.images.len()
                ));
            }
            for (ii, ji) in jimgs.iter().enumerate() {
                let Some(ri) = rome.images.get(ii) else { break };
                if ji["name"].as_str().map(str::to_string) != ri.name {
                    ome_diffs.push(format!(
                        "img{ii} name: java={:?} rust={:?}",
                        ji["name"].as_str(),
                        ri.name
                    ));
                }
                if !approx(jf64(&ji["physicalSizeX"]), ri.physical_size_x) {
                    ome_diffs.push(format!(
                        "img{ii} physX: java={:?} rust={:?}",
                        jf64(&ji["physicalSizeX"]),
                        ri.physical_size_x
                    ));
                }
                if !approx(jf64(&ji["physicalSizeY"]), ri.physical_size_y) {
                    ome_diffs.push(format!(
                        "img{ii} physY: java={:?} rust={:?}",
                        jf64(&ji["physicalSizeY"]),
                        ri.physical_size_y
                    ));
                }
                let jch = ji["channels"].as_array().cloned().unwrap_or_default();
                if jch.len() != ri.channels.len() {
                    ome_diffs.push(format!(
                        "img{ii} channel count: java={} rust={}",
                        jch.len(),
                        ri.channels.len()
                    ));
                }
                for (ci, jc) in jch.iter().enumerate() {
                    let Some(rc) = ri.channels.get(ci) else { break };
                    if jc["name"].as_str().map(str::to_string) != rc.name {
                        ome_diffs.push(format!(
                            "img{ii} ch{ci} name: java={:?} rust={:?}",
                            jc["name"].as_str(),
                            rc.name
                        ));
                    }
                    if !approx(jf64(&jc["emission"]), rc.emission_wavelength) {
                        ome_diffs.push(format!(
                            "img{ii} ch{ci} emission: java={:?} rust={:?}",
                            jf64(&jc["emission"]),
                            rc.emission_wavelength
                        ));
                    }
                    if !approx(jf64(&jc["excitation"]), rc.excitation_wavelength) {
                        ome_diffs.push(format!(
                            "img{ii} ch{ci} excitation: java={:?} rust={:?}",
                            jf64(&jc["excitation"]),
                            rc.excitation_wavelength
                        ));
                    }
                }
            }
            if ome_diffs.is_empty() {
                println!("  OME ✓  {} image(s)", rome.images.len());
                score.ome_ok += 1;
            } else {
                let shown: Vec<_> = ome_diffs.iter().take(6).cloned().collect();
                println!(
                    "  OME ✗  {}{}",
                    shown.join("; "),
                    if ome_diffs.len() > 6 {
                        format!(" (+{} more)", ome_diffs.len() - 6)
                    } else {
                        String::new()
                    }
                );
                score.ome_bad += 1;
                if strict {
                    hard_failures.push(rel.to_string());
                }
            }
        } else if jome
            .get("images")
            .and_then(Value::as_array)
            .map(|a| !a.is_empty())
            == Some(true)
        {
            println!("  OME ✗  java exposed OME images, rust returned None");
            score.ome_bad += 1;
            if strict {
                hard_failures.push(rel.to_string());
            }
        }
    }

    // ---- scoreboard ----
    println!("\n══════════════ PARITY SUMMARY ══════════════");
    println!("files compared : {checked}");
    println!(
        "core metadata  : {} series ✓ / {} series ✗",
        score.core_ok, score.core_bad
    );
    println!(
        "OME metadata   : {} files ✓ / {} files ✗",
        score.ome_ok, score.ome_bad
    );
    println!(
        "pixels         : {} bitwise / {} tolerant(±{PIXEL_TOL} JPEG) / {} ⚠ Java-bug / {} ✗",
        score.px_exact, score.px_tol, score.px_java_div, score.px_bad
    );
    println!(
        "deeper-check findings (new vs old first-8/256² sampling): {}",
        new_findings.len()
    );
    for f in &new_findings {
        println!("   • {f}");
    }
    println!("═════════════════════════════════════════════");

    assert!(checked > 0, "no files were compared — populate ./testdata");

    if strict && !hard_failures.is_empty() {
        hard_failures.sort();
        hard_failures.dedup();
        panic!("STRICT parity divergence in: {}", hard_failures.join(", "));
    }
    if !core_failures.is_empty() {
        panic!(
            "CORE metadata divergence from Java ({} issue(s)):\n  - {}",
            core_failures.len(),
            core_failures.join("\n  - ")
        );
    }
}
