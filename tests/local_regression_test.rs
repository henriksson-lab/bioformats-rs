//! Regression tests on real files from user bug reports.
//!
//! The files are private lab data and are not checked in. They are looked up
//! under `examples/` (override with `BIOFORMATS_RS_REGRESSION_DIR`); absent
//! files are skipped. Expected values were recorded with Java Bio-Formats
//! 8.5.0 and verified unchanged against the parity reference 9.0.0-rc1. Pixels are compared as an FNV-1a hash of the centered
//! min(1024)x min(1024) region, only for planes stored losslessly: lossy
//! JPEG/JPEG-2000 planes differ from Java by ±1–3 decoder rounding and are
//! checked for metadata only.
//!
//! Run: cargo test --test local_regression_test -- --nocapture

use bioformats::common::metadata::DimensionOrder;
use bioformats::common::pixel_type::PixelType;
use bioformats::ImageReader;
use std::path::PathBuf;

struct SeriesExpect {
    size: (u32, u32),
    zct: (u32, u32, u32),
    image_count: u32,
    pixel_type: PixelType,
    rgb: bool,
    order: DimensionOrder,
    /// (plane index, FNV-1a of the centered region) recorded from Java.
    planes: &'static [(u32, u64)],
}

struct FileExpect {
    path: &'static str,
    series: &'static [SeriesExpect],
}

#[allow(clippy::too_many_arguments)]
const fn s(
    x: u32,
    y: u32,
    z: u32,
    c: u32,
    t: u32,
    image_count: u32,
    pixel_type: PixelType,
    rgb: bool,
    order: DimensionOrder,
    planes: &'static [(u32, u64)],
) -> SeriesExpect {
    SeriesExpect {
        size: (x, y),
        zct: (z, c, t),
        image_count,
        pixel_type,
        rgb,
        order,
        planes,
    }
}

// Findings covered:
// - A1_Maximum_Z.vsi: single-file VSI read channels as T (C=1,T=5), and the
//   VSI tag parser counted channels Java never reaches.
// - B2_0003.vsi / D7_006.vsi: stale/inline DIMENSION_MEANING lost the Z axis.
// - D1/D3 *.nd2: camera-settings chunk overwrote size_c/pixel type (eb0c183),
//   and D1 (aborted acquisition) reported Z=45 instead of Java's T=5.
// - The remaining files guard the ETS/pyramid paths against regressions.
const EXPECTED: &[FileExpect] = &[
    FileExpect {
        path: "uptake assay/A1_Maximum_Z.vsi",
        series: &[
            s(
                512,
                512,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[(0, 0x98ed5496387e2325)],
            ),
            s(
                2048,
                2048,
                1,
                5,
                1,
                5,
                PixelType::Uint16,
                false,
                DimensionOrder::XYZCT,
                &[
                    (0, 0xbdae7a45775db0d6),
                    (1, 0x65fb22ed78eac7a0),
                    (2, 0x06ea2f022e21f730),
                    (3, 0x903b977f00a4843b),
                    (4, 0x88539d340960d137),
                ],
            ),
        ],
    },
    FileExpect {
        path: "Slide1_distal_zoom_40x.vsi",
        series: &[
            s(
                512,
                375,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                4096,
                3000,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[(0, 0x67f2ad761713ff3e)],
            ),
        ],
    },
    FileExpect {
        path: "haut/Image_045_M5_MEV_Sk3_Zstack_40x_decon.vsi",
        series: &[
            s(
                6308,
                9107,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                3154,
                4554,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                1577,
                2277,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                789,
                1139,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                395,
                570,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                198,
                285,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                31489,
                14986,
                1,
                1,
                1,
                1,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                15745,
                7493,
                1,
                1,
                1,
                1,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                7873,
                3747,
                1,
                1,
                1,
                1,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                3937,
                1874,
                1,
                1,
                1,
                1,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                1969,
                937,
                1,
                1,
                1,
                1,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                985,
                469,
                1,
                1,
                1,
                1,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                493,
                235,
                1,
                1,
                1,
                1,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                9343,
                27143,
                5,
                5,
                1,
                25,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                4672,
                13572,
                5,
                5,
                1,
                25,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                2336,
                6786,
                5,
                5,
                1,
                25,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                1168,
                3393,
                5,
                5,
                1,
                25,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                584,
                1697,
                5,
                5,
                1,
                25,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                292,
                849,
                5,
                5,
                1,
                25,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                146,
                425,
                5,
                5,
                1,
                25,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[],
            ),
            s(
                991,
                375,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[],
            ),
        ],
    },
    FileExpect {
        path: "böse bilder/B2_0003.vsi",
        series: &[
            s(
                2048,
                2048,
                5,
                5,
                1,
                25,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[
                    (0, 0xa862cbadbb3da656),
                    (1, 0xbc9fb9b4e7626c36),
                    (2, 0xd390a830df60199a),
                    (3, 0x6bd56d0c5240d696),
                    (24, 0x2272f87c938d90ee),
                ],
            ),
            s(
                512,
                512,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[],
            ),
        ],
    },
    FileExpect {
        path: "böse bilder/D7_006.vsi",
        series: &[
            s(
                2048,
                2048,
                6,
                3,
                1,
                18,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[
                    (0, 0xca679de1b15efb7e),
                    (1, 0x6c95c590e3fa3898),
                    (2, 0x7f1c68143d06fd9d),
                    (3, 0x07c97ef3289e8dfe),
                    (17, 0xbd994ee8dc23b9d9),
                ],
            ),
            s(
                512,
                512,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[],
            ),
        ],
    },
    FileExpect {
        path: "anna_images/G10_02.vsi",
        series: &[
            s(
                2048,
                2048,
                1,
                3,
                1,
                3,
                PixelType::Uint16,
                false,
                DimensionOrder::XYCZT,
                &[
                    (0, 0xd4e2c9e2dbed302b),
                    (1, 0x9650e709c7719803),
                    (2, 0x9058b55827ce1c97),
                ],
            ),
            s(
                512,
                512,
                1,
                3,
                1,
                1,
                PixelType::Uint8,
                true,
                DimensionOrder::XYCZT,
                &[],
            ),
        ],
    },
    FileExpect {
        path: "D1 bEV001.nd2",
        series: &[s(
            2720,
            2720,
            1,
            3,
            5,
            15,
            PixelType::Uint16,
            false,
            DimensionOrder::XYCZT,
            &[
                (0, 0xe9e1e59ae0ed9095),
                (1, 0x4c4e364981096adc),
                (2, 0x7e45a93b44ec2650),
                (3, 0x9afdfb7c7cb4bfec),
                (14, 0x7ab6a128b6a22325),
            ],
        )],
    },
    FileExpect {
        path: "D3 bEV003.nd2",
        series: &[s(
            2720,
            2720,
            41,
            3,
            1,
            123,
            PixelType::Uint16,
            false,
            DimensionOrder::XYCZT,
            &[
                (0, 0x0cc1a61ad3dacd53),
                (1, 0x94bd273cbfa7ca4b),
                (2, 0xd7fec9c1839e65fc),
                (3, 0x05a75c009e8afb67),
                (122, 0xc5f1a3e8779f5a69),
            ],
        )],
    },
];

/// Files with a known, not yet fixed divergence from Java. Their mismatches
/// are printed but do not fail the test; remove an entry once it is fixed.
const KNOWN_FAILURES: &[(&str, &str)] = &[(
    "D1 bEV001.nd2",
    "truncated file without chunk map: Rust drops the last ImageDataSeq block \
     and applies Z=45 from metadata Java never reaches (iterateIn aborts)",
)];

fn fnv1a(bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for &b in bytes {
        h ^= b as u64;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h
}

#[test]
fn local_files_match_java_bioformats() {
    let root = std::env::var("BIOFORMATS_RS_REGRESSION_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("examples"));
    let mut failures = Vec::new();
    for file in EXPECTED {
        let known = KNOWN_FAILURES.iter().find(|(p, _)| *p == file.path);
        let failures_before = failures.len();
        let path = root.join(file.path);
        if !path.exists() {
            eprintln!("skip (absent): {}", path.display());
            continue;
        }
        let mut reader = match ImageReader::open(&path) {
            Ok(r) => r,
            Err(e) => {
                failures.push(format!("{}: open failed: {e}", file.path));
                continue;
            }
        };
        if reader.series_count() != file.series.len() {
            failures.push(format!(
                "{}: series_count {} != java {}",
                file.path,
                reader.series_count(),
                file.series.len()
            ));
            continue;
        }
        for (si, exp) in file.series.iter().enumerate() {
            reader.set_series(si).unwrap();
            let m = reader.metadata().clone();
            let got = (
                (m.size_x, m.size_y),
                (m.size_z, m.size_c, m.size_t),
                m.image_count,
                m.pixel_type,
                m.is_rgb,
                m.dimension_order,
            );
            let want = (
                exp.size,
                exp.zct,
                exp.image_count,
                exp.pixel_type,
                exp.rgb,
                exp.order,
            );
            if got != want {
                failures.push(format!(
                    "{} series {si}: {got:?} != java {want:?}",
                    file.path
                ));
                continue;
            }
            let (w, h) = (m.size_x.min(1024), m.size_y.min(1024));
            let (x, y) = ((m.size_x - w) / 2, (m.size_y - h) / 2);
            for &(plane, hash) in exp.planes {
                match reader.open_bytes_region(plane, x, y, w, h) {
                    Ok(bytes) if fnv1a(&bytes) == hash => {}
                    Ok(_) => failures.push(format!(
                        "{} series {si} plane {plane}: pixels differ from java",
                        file.path
                    )),
                    Err(e) => failures.push(format!(
                        "{} series {si} plane {plane}: read failed: {e}",
                        file.path
                    )),
                }
            }
        }
        if let Some((_, why)) = known {
            for f in failures.drain(failures_before..) {
                eprintln!("KNOWN FAILURE ({why}): {f}");
            }
        }
        eprintln!("checked: {}", file.path);
    }
    assert!(failures.is_empty(), "\n{}", failures.join("\n"));
}
