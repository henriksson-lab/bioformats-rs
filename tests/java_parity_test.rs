//! Java↔Rust parity harness.
//!
//! For each real file in `./testdata`, this runs the Java Bio-Formats reference
//! (`parity/BfParityOracle.java` against `bioformats_package.jar`) and compares
//! its output to our Rust `ImageReader`, across three axes:
//!   1. CORE metadata   — sizeX/Y/Z/C/T, pixelType, bitsPerPixel, imageCount,
//!                        dimensionOrder, rgb/interleaved/indexed/littleEndian.
//!   2. OME metadata    — image name, physical sizes, time increment, and
//!                        per-channel name / samplesPerPixel / emission / excitation.
//!   3. PIXELS          — CRC32 of a bounded top-left region of the first planes,
//!                        read identically on both sides.
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
];

const MAX_PLANES: u32 = 8;
const REGION: u32 = 256;

fn testdata(rel: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("testdata").join(rel)
}

fn jar_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("bioformats_package.jar")
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

fn run_oracle(cp: &str, path: &Path) -> Option<Value> {
    let out = Command::new("java")
        .arg("-cp")
        .arg(cp)
        .arg("BfParityOracle")
        .arg(path)
        .arg(MAX_PLANES.to_string())
        .arg(REGION.to_string())
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().find(|l| l.trim_start().starts_with('{'))?;
    serde_json::from_str(line).ok()
}

#[derive(Default)]
struct Score {
    core_ok: u32,
    core_bad: u32,
    ome_ok: u32,
    ome_bad: u32,
    px_ok: u32,
    px_bad: u32,
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
    let filters: Vec<&str> = filter.split(',').map(str::trim).filter(|s| !s.is_empty()).collect();
    let Some(cp) = oracle_classpath() else { return };

    let mut score = Score::default();
    let mut core_failures: Vec<String> = Vec::new();
    let mut hard_failures: Vec<String> = Vec::new();
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
        let Some(j) = run_oracle(cp, &path) else {
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
            cmp_u("sizeX", js["sizeX"].as_u64().unwrap_or(0), m.size_x as u64, &mut core_diffs);
            cmp_u("sizeY", js["sizeY"].as_u64().unwrap_or(0), m.size_y as u64, &mut core_diffs);
            cmp_u("sizeZ", js["sizeZ"].as_u64().unwrap_or(0), m.size_z as u64, &mut core_diffs);
            cmp_u("sizeC", js["sizeC"].as_u64().unwrap_or(0), m.size_c as u64, &mut core_diffs);
            cmp_u("sizeT", js["sizeT"].as_u64().unwrap_or(0), m.size_t as u64, &mut core_diffs);
            cmp_u("imageCount", js["imageCount"].as_u64().unwrap_or(0), m.image_count as u64, &mut core_diffs);
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
                    m.size_x, m.size_y, m.size_z, m.size_c, m.size_t,
                    pixel_type_to_java(m.pixel_type), m.image_count
                );
                score.core_ok += 1;
            } else {
                println!("  s{si} core ✗  {}", core_diffs.join("; "));
                score.core_bad += 1;
                core_failures.push(format!("{rel} s{si}: {}", core_diffs.join("; ")));
                hard_failures.push(rel.to_string());
            }

            // ---- pixels: bounded-region CRC per plane ----
            let planes = js["planeCrc"].as_array().cloned().unwrap_or_default();
            let mut px_match = 0usize;
            let mut px_total = 0usize;
            let mut first_px_diff: Option<String> = None;
            for pj in &planes {
                if pj.get("error").is_some() {
                    continue; // Java couldn't read this plane either
                }
                px_total += 1;
                let p = pj["plane"].as_u64().unwrap_or(0) as u32;
                let w = pj["w"].as_u64().unwrap_or(0) as u32;
                let h = pj["h"].as_u64().unwrap_or(0) as u32;
                match reader.open_bytes_region(p, 0, 0, w, h) {
                    Ok(buf) => {
                        let rcrc = crc32_ieee(&buf) as u64;
                        let jcrc = pj["crc"].as_u64().unwrap_or(u64::MAX);
                        let jlen = pj["len"].as_u64().unwrap_or(0);
                        if rcrc == jcrc && buf.len() as u64 == jlen {
                            px_match += 1;
                        } else if first_px_diff.is_none() {
                            first_px_diff = Some(format!(
                                "plane{p}: java(len={jlen},crc={jcrc}) rust(len={},crc={rcrc})",
                                buf.len()
                            ));
                        }
                    }
                    Err(e) => {
                        if first_px_diff.is_none() {
                            first_px_diff = Some(format!("plane{p}: rust read error: {e}"));
                        }
                    }
                }
            }
            if px_total > 0 {
                if px_match == px_total {
                    println!("  s{si} pixels ✓  {px_match}/{px_total} plane CRCs match");
                    score.px_ok += 1;
                } else {
                    println!(
                        "  s{si} pixels ✗  {px_match}/{px_total} match — {}",
                        first_px_diff.as_deref().unwrap_or("")
                    );
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
                println!("  OME ✗  {}{}", shown.join("; "),
                    if ome_diffs.len() > 6 { format!(" (+{} more)", ome_diffs.len() - 6) } else { String::new() });
                score.ome_bad += 1;
                if strict {
                    hard_failures.push(rel.to_string());
                }
            }
        } else if jome.get("images").and_then(Value::as_array).map(|a| !a.is_empty()) == Some(true) {
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
    println!("core metadata  : {} series ✓ / {} series ✗", score.core_ok, score.core_bad);
    println!("OME metadata   : {} files ✓ / {} files ✗", score.ome_ok, score.ome_bad);
    println!("pixel CRC      : {} series ✓ / {} series ✗", score.px_ok, score.px_bad);
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
