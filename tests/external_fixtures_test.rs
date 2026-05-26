use bioformats::common::error::BioFormatsError;
use bioformats::common::metadata::MetadataValue;
use bioformats::common::pixel_type::PixelType;
use bioformats::ImageReader;
use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::Command;

fn external_root() -> Option<PathBuf> {
    std::env::var_os("BIOFORMATS_RS_EXTERNAL_FIXTURES").map(PathBuf::from)
}

fn fixture_path(root: &Path, relative: &str) -> PathBuf {
    root.join(relative)
}

fn open_external_image_if_present(root: &Path, relative: &str) -> bool {
    let path = fixture_path(root, relative);
    if !path.exists() {
        eprintln!("skipping missing external fixture {}", path.display());
        return false;
    }

    let mut reader = ImageReader::open(&path)
        .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
    let meta = reader.metadata().clone();
    assert!(meta.size_x > 0, "zero width for {}", path.display());
    assert!(meta.size_y > 0, "zero height for {}", path.display());
    assert!(
        meta.image_count > 0,
        "zero plane count for {}",
        path.display()
    );

    let plane = reader
        .open_bytes(0)
        .unwrap_or_else(|err| panic!("failed to read first plane from {}: {err}", path.display()));
    assert!(
        !plane.is_empty(),
        "empty first plane for {}",
        path.display()
    );
    true
}

fn open_external_metadata_if_present(root: &Path, relative: &str) -> bool {
    let path = fixture_path(root, relative);
    if !path.exists() {
        eprintln!("skipping missing external fixture {}", path.display());
        return false;
    }

    let reader = ImageReader::open(&path)
        .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
    let meta = reader.metadata();
    assert!(meta.size_x > 0, "zero width for {}", path.display());
    assert!(meta.size_y > 0, "zero height for {}", path.display());
    assert!(
        meta.image_count > 0,
        "zero plane count for {}",
        path.display()
    );
    true
}

fn assert_metadata_int(
    meta: &std::collections::HashMap<String, MetadataValue>,
    key: &str,
    value: i64,
) {
    assert!(
        matches!(meta.get(key), Some(MetadataValue::Int(actual)) if *actual == value),
        "wrong metadata value for {key}"
    );
}

fn assert_metadata_float(
    meta: &std::collections::HashMap<String, MetadataValue>,
    key: &str,
    value: f64,
) {
    assert!(
        matches!(meta.get(key), Some(MetadataValue::Float(actual)) if *actual == value),
        "wrong metadata value for {key}"
    );
}

fn assert_metadata_bool(
    meta: &std::collections::HashMap<String, MetadataValue>,
    key: &str,
    value: bool,
) {
    assert!(
        matches!(meta.get(key), Some(MetadataValue::Bool(actual)) if *actual == value),
        "wrong metadata value for {key}"
    );
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash = 0xcbf29ce484222325u64;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    hash
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

fn read_file_row(path: &Path, offset: u64, row_bytes: usize) -> Vec<u8> {
    let mut file = File::open(path).unwrap_or_else(|err| {
        panic!(
            "failed to open {} for raw row comparison: {err}",
            path.display()
        )
    });
    file.seek(SeekFrom::Start(offset)).unwrap_or_else(|err| {
        panic!(
            "failed to seek {} to raw row offset {offset}: {err}",
            path.display()
        )
    });
    let mut row = vec![0u8; row_bytes];
    file.read_exact(&mut row).unwrap_or_else(|err| {
        panic!(
            "failed to read {row_bytes} raw row bytes from {}: {err}",
            path.display()
        )
    });
    row
}

fn read_file_f32(path: &Path, offset: u64) -> f32 {
    let mut file = File::open(path).unwrap_or_else(|err| {
        panic!(
            "failed to open {} for raw sample comparison: {err}",
            path.display()
        )
    });
    file.seek(SeekFrom::Start(offset)).unwrap_or_else(|err| {
        panic!(
            "failed to seek {} to raw sample offset {offset}: {err}",
            path.display()
        )
    });
    let mut sample = [0u8; 4];
    file.read_exact(&mut sample).unwrap_or_else(|err| {
        panic!(
            "failed to read raw f32 sample from {}: {err}",
            path.display()
        )
    });
    f32::from_le_bytes(sample)
}

fn le_i32(bytes: &[u8], offset: usize) -> i32 {
    i32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

fn le_u64(bytes: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes(bytes[offset..offset + 8].try_into().unwrap())
}

fn czi_reduced_stored_xy_entry_count(path: &Path) -> usize {
    let bytes = std::fs::read(path).unwrap_or_else(|err| {
        panic!(
            "failed to read CZI directory from {}: {err}",
            path.display()
        )
    });
    assert!(
        bytes.len() >= 112 && bytes.starts_with(b"ZISRAWFILE"),
        "not a CZI file: {}",
        path.display()
    );

    let file_header = &bytes[32..112];
    let real_dir_position = le_u64(file_header, 52) as usize;
    let legacy_dir_position = le_u64(file_header, 36) as usize;
    let dir_position = [real_dir_position, legacy_dir_position]
        .into_iter()
        .find(|position| *position > 0 && position + 160 <= bytes.len())
        .unwrap_or_else(|| panic!("missing CZI directory in {}", path.display()));

    let dir_header = &bytes[dir_position..dir_position + 32];
    let allocated = le_u64(dir_header, 16) as usize;
    let used = le_u64(dir_header, 24) as usize;
    let entry_count = le_i32(&bytes[dir_position + 32..dir_position + 160], 0).max(0) as usize;
    let body_start = dir_position + 160;
    let body_len = allocated.max(used).saturating_sub(128);
    let body = &bytes[body_start..body_start + body_len.min(bytes.len() - body_start)];
    let fixed_stride = if body.len() >= entry_count.saturating_mul(256) {
        Some(256)
    } else {
        None
    };

    let mut reduced = 0usize;
    let mut offset = 0usize;
    for _ in 0..entry_count {
        if offset + 32 > body.len() {
            break;
        }
        let dim_count = le_i32(body, offset + 28).max(0) as usize;
        let compact_len = 32 + dim_count * 20;
        if offset + compact_len > body.len() {
            break;
        }
        let entry =
            &body[offset..offset + fixed_stride.unwrap_or(compact_len).min(body.len() - offset)];
        let mut has_reduced_xy = false;
        for dim in 0..dim_count {
            let dim_offset = 32 + dim * 20;
            let name = std::str::from_utf8(&entry[dim_offset..dim_offset + 4])
                .unwrap_or("")
                .trim_end_matches('\0')
                .trim();
            if name == "X" || name == "Y" {
                let size = le_i32(entry, dim_offset + 8);
                let stored_size = le_i32(entry, dim_offset + 16);
                has_reduced_xy |= stored_size > 0 && stored_size != size;
            }
        }
        reduced += usize::from(has_reduced_xy);
        offset += fixed_stride.unwrap_or(compact_len);
    }
    reduced
}

fn assert_close_f32(actual: f32, expected: f32, context: &str) {
    assert!(
        (actual - expected).abs() <= 1.0e-6,
        "{context}: expected {expected}, got {actual}"
    );
}

#[test]
fn external_nd2_smoke_set_opens_and_reads_all_planes() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external ND2 fixture tests");
        return;
    };

    let fixtures = [
        (
            "nd2/downloads.openmicroscopy.org/maxime/BF007.nd2",
            164,
            156,
            1,
            16,
            1,
            51_168,
            35,
        ),
        (
            "nd2/downloads.openmicroscopy.org/jonas/jonas_nd2Test/Exception_2.nd2",
            696,
            520,
            1,
            16,
            31,
            723_840,
            41,
        ),
        (
            "nd2/downloads.openmicroscopy.org/aryeh/MeOh_high_fluo_003.nd2",
            800,
            600,
            1,
            16,
            13,
            960_000,
            48,
        ),
        (
            "nd2/downloads.openmicroscopy.org/jonas/header_test2.nd2",
            696,
            520,
            1,
            16,
            20,
            723_840,
            34,
        ),
    ];

    for (relative, size_x, size_y, size_c, bits_per_pixel, image_count, plane_len, chunk_count) in
        fixtures
    {
        let path = fixture_path(&root, relative);
        assert!(path.exists(), "missing external fixture {}", path.display());

        let mut reader = ImageReader::open(&path)
            .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
        let meta = reader.metadata().clone();
        assert_eq!(meta.size_x, size_x, "wrong width for {}", path.display());
        assert_eq!(meta.size_y, size_y, "wrong height for {}", path.display());
        assert_eq!(
            meta.size_c,
            size_c,
            "wrong channel count for {}",
            path.display()
        );
        assert_eq!(
            meta.bits_per_pixel,
            bits_per_pixel,
            "wrong bit depth for {}",
            path.display()
        );
        assert_eq!(
            meta.image_count,
            image_count,
            "wrong plane count for {}",
            path.display()
        );
        assert!(
            matches!(
                meta.series_metadata.get("nd2_chunks"),
                Some(MetadataValue::Int(actual)) if *actual == chunk_count
            ),
            "wrong ND2 chunk count for {}: {:?}",
            path.display(),
            meta.series_metadata.get("nd2_chunks")
        );

        for plane_index in 0..meta.image_count {
            let plane = reader.open_bytes(plane_index).unwrap_or_else(|err| {
                panic!(
                    "failed to read plane {plane_index} from {}: {err}",
                    path.display()
                )
            });
            assert_eq!(
                plane.len(),
                plane_len,
                "wrong plane {plane_index} byte length for {}",
                path.display()
            );
        }
    }
}

#[test]
fn external_old_nd2_jp2_candidates_open_with_nd2_metadata() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external ND2 fixture tests");
        return;
    };

    let fixtures = [
        (
            "nd2/downloads.openmicroscopy.org/aryeh/b16_14_12.nd2",
            1280,
            1024,
            8,
            PixelType::Uint8,
            1,
            2,
            50,
            100,
            1_310_720,
            0xeb4c_020c_f243_2a21,
        ),
        (
            "nd2/downloads.openmicroscopy.org/aryeh/but3_cont200-1.nd2",
            1392,
            1040,
            16,
            PixelType::Uint16,
            5,
            2,
            1,
            2,
            2_895_360,
            0xdd72_ef46_45f1_5e54,
        ),
    ];

    let mut checked = 0;
    for (
        relative,
        size_x,
        size_y,
        bits_per_pixel,
        pixel_type,
        series_count,
        size_c,
        size_t,
        image_count,
        plane_len,
        expected_hash,
    ) in fixtures
    {
        let path = fixture_path(&root, relative);
        if !path.exists() {
            eprintln!("skipping missing external fixture {}", path.display());
            continue;
        }
        checked += 1;

        let mut reader = ImageReader::open(&path)
            .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
        assert_eq!(
            reader.series_count(),
            series_count,
            "wrong series count for {}",
            path.display()
        );
        let meta = reader.metadata().clone();
        assert_eq!(meta.size_x, size_x, "wrong width for {}", path.display());
        assert_eq!(meta.size_y, size_y, "wrong height for {}", path.display());
        assert_eq!(
            meta.bits_per_pixel,
            bits_per_pixel,
            "wrong bit depth for {}",
            path.display()
        );
        assert_eq!(
            meta.pixel_type,
            pixel_type,
            "wrong pixel type for {}",
            path.display()
        );
        assert_eq!(meta.size_c, size_c, "wrong SizeC for {}", path.display());
        assert_eq!(meta.size_t, size_t, "wrong SizeT for {}", path.display());
        assert_eq!(
            meta.image_count,
            image_count,
            "wrong old ND-box JP2-backed ND2 plane count for {}",
            path.display()
        );
        assert!(
            matches!(
                meta.series_metadata.get("nd2_old_jp2"),
                Some(MetadataValue::Bool(true))
            ),
            "old ND2 JP2 metadata flag missing for {}",
            path.display()
        );

        let plane = reader.open_bytes(0).unwrap_or_else(|err| {
            panic!("failed to read first plane from {}: {err}", path.display())
        });
        assert_eq!(
            plane.len(),
            plane_len,
            "wrong decoded plane byte length for {}",
            path.display()
        );
        let actual_hash = fnv1a64(&plane);
        assert_eq!(
            actual_hash,
            expected_hash,
            "wrong decoded plane hash for {}: {actual_hash:#018x}",
            path.display()
        );
    }

    if checked == 0 {
        eprintln!("skipping old ND2 JP2 candidate test; no optional fixtures are present");
    }
}

#[test]
fn external_nd2_zenodo_vpa002_opens_and_reads_first_plane() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external ND2 fixture tests");
        return;
    };

    let path = fixture_path(
        &root,
        "nd2/zenodo.org/records/8161776/files/2D%20500uM%20VPA002.nd2",
    );
    if !path.exists() {
        eprintln!("skipping missing external fixture {}", path.display());
        return;
    }

    let mut reader = ImageReader::open(&path)
        .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
    let meta = reader.metadata().clone();
    assert_eq!(reader.series_count(), 1, "wrong series count");
    assert_eq!(meta.size_x, 5570, "wrong width for {}", path.display());
    assert_eq!(meta.size_y, 5570, "wrong height for {}", path.display());
    assert_eq!(meta.size_c, 1, "wrong channel count for {}", path.display());
    assert_eq!(meta.size_z, 1, "wrong Z count for {}", path.display());
    assert_eq!(meta.size_t, 1, "wrong T count for {}", path.display());
    assert_eq!(
        meta.bits_per_pixel,
        8,
        "wrong bit depth for {}",
        path.display()
    );
    assert_eq!(
        meta.pixel_type,
        PixelType::Uint8,
        "wrong pixel type for {}",
        path.display()
    );
    assert_eq!(
        meta.image_count,
        1,
        "wrong plane count for {}",
        path.display()
    );
    assert!(
        matches!(
            meta.series_metadata.get("nd2_chunks"),
            Some(MetadataValue::Int(actual)) if *actual == 31
        ),
        "wrong ND2 chunk count for {}: {:?}",
        path.display(),
        meta.series_metadata.get("nd2_chunks")
    );

    let plane = reader.open_bytes(0).unwrap_or_else(|err| {
        panic!(
            "failed to read first VPA002 plane from {}: {err}",
            path.display()
        )
    });
    assert_eq!(
        plane.len(),
        31_024_900,
        "wrong first VPA002 plane byte length for {}",
        path.display()
    );
    let actual_hash = fnv1a64(&plane);
    assert_eq!(
        actual_hash,
        0x1988_df00_f399_bb10,
        "wrong first VPA002 plane hash for {}: {actual_hash:#018x}",
        path.display()
    );
}

#[test]
fn external_nd2_modern_uicomp_candidate_reads_sample_planes() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external ND2 fixture tests");
        return;
    };

    let path = fixture_path(
        &root,
        "nd2/downloads.openmicroscopy.org/jonas/100217_OD122_001.nd2",
    );
    if !path.exists() {
        eprintln!("skipping missing external fixture {}", path.display());
        return;
    }

    let mut reader = ImageReader::open(&path)
        .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
    let meta = reader.metadata().clone();
    assert_eq!(reader.series_count(), 1, "wrong series count");
    assert_eq!(meta.size_x, 277, "wrong width for {}", path.display());
    assert_eq!(meta.size_y, 311, "wrong height for {}", path.display());
    assert_eq!(meta.size_c, 2, "wrong channel count for {}", path.display());
    assert_eq!(meta.size_z, 725, "wrong Z count for {}", path.display());
    assert_eq!(meta.size_t, 1, "wrong T count for {}", path.display());
    assert_eq!(
        meta.bits_per_pixel,
        16,
        "wrong bit depth for {}",
        path.display()
    );
    assert_eq!(
        meta.pixel_type,
        PixelType::Uint16,
        "wrong pixel type for {}",
        path.display()
    );
    assert_eq!(
        meta.image_count,
        725,
        "wrong plane count for {}",
        path.display()
    );
    assert!(
        matches!(
            meta.series_metadata.get("nd2_chunks"),
            Some(MetadataValue::Int(actual)) if *actual == 740
        ),
        "wrong ND2 chunk count for {}: {:?}",
        path.display(),
        meta.series_metadata.get("nd2_chunks")
    );

    for (plane_index, expected_hash) in [
        (0, 0x3ce2_526c_db73_5e32),
        (1, 0x74b6_9e64_f7ed_96f6),
        (724, 0x9dea_5ee3_ddfa_194c),
    ] {
        let plane = reader.open_bytes(plane_index).unwrap_or_else(|err| {
            panic!(
                "failed to read plane {plane_index} from {}: {err}",
                path.display()
            )
        });
        assert_eq!(
            plane.len(),
            344_588,
            "wrong plane {plane_index} byte length for {}",
            path.display()
        );
        let actual_hash = fnv1a64(&plane);
        assert_eq!(
            actual_hash,
            expected_hash,
            "wrong plane {plane_index} hash for {}: {actual_hash:#018x}",
            path.display()
        );
    }
}

#[test]
fn external_czi_smoke_set_opens_and_reads_all_planes() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external CZI fixture tests");
        return;
    };

    let fixtures = [
        "czi/downloads.openmicroscopy.org/idr0011/Plate1-Blue-A_TS-Stinger/Plate1-Blue-A-03-Scene-1-P1-D1-01.czi",
        "czi/downloads.openmicroscopy.org/idr0011/Plate1-Blue-A_TS-Stinger/Plate1-Blue-A-03-Scene-2-P3-D1-02.czi",
        "czi/downloads.openmicroscopy.org/idr0011/Plate1-Blue-A_TS-Stinger/Plate1-Blue-A-03-Scene-3-P2-D1-03.czi",
    ];

    for relative in fixtures {
        let path = fixture_path(&root, relative);
        assert!(path.exists(), "missing external fixture {}", path.display());

        let mut reader = ImageReader::open(&path)
            .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
        let meta = reader.metadata().clone();
        assert_eq!(meta.size_x, 672, "wrong width for {}", path.display());
        assert_eq!(meta.size_y, 512, "wrong height for {}", path.display());
        assert_eq!(
            meta.image_count,
            63,
            "wrong plane count for {}",
            path.display()
        );
        assert_eq!(
            meta.resolution_count,
            1,
            "CZI smoke fixture unexpectedly has pyramid levels for {}",
            path.display()
        );
        assert!(
            matches!(
                meta.series_metadata.get("czi_subblocks"),
                Some(MetadataValue::Int(63))
            ),
            "wrong CZI subblock count for {}",
            path.display()
        );

        for plane_index in 0..meta.image_count {
            let plane = reader.open_bytes(plane_index).unwrap_or_else(|err| {
                panic!(
                    "failed to read plane {} from {}: {err}",
                    plane_index,
                    path.display()
                )
            });
            assert_eq!(
                plane.len(),
                688_128,
                "wrong plane {} byte length for {}",
                plane_index,
                path.display()
            );
        }
    }
}

#[test]
fn external_czi_targeted_mosaic_sets_have_expected_structure() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external CZI fixture tests");
        return;
    };

    let fixtures = [
        "czi/openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-Flat.czi",
        "czi/openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-JXR.czi",
        "czi/openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-Cropped.czi",
        "czi/zenodo.org/records/7015307/files/S=2_2x2_CH=1.czi",
        "czi/zenodo.org/records/7015307/files/W96_B2+B4_S=2_T=1=Z=1_C=1_Tile=5x9.czi",
    ];

    let mut checked = 0usize;
    for relative in fixtures {
        let path = fixture_path(&root, relative);
        if !path.exists() {
            eprintln!("skipping missing external fixture {}", path.display());
            continue;
        }
        checked += 1;

        let reader = ImageReader::open(&path)
            .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
        let meta = reader.metadata();
        assert!(meta.size_x > 0, "zero width for {}", path.display());
        assert!(meta.size_y > 0, "zero height for {}", path.display());
        assert!(
            meta.image_count > 0,
            "zero plane count for {}",
            path.display()
        );
        assert!(
            meta.resolution_count >= 1,
            "zero resolution count for {}",
            path.display()
        );
        let subblocks = match meta.series_metadata.get("czi_subblocks") {
            Some(MetadataValue::Int(value)) => *value,
            other => panic!("missing czi_subblocks for {}: {other:?}", path.display()),
        };
        assert!(subblocks > 0, "zero CZI subblocks for {}", path.display());

        assert!(
            subblocks > i64::from(meta.image_count),
            "expected multiple CZI subblocks per logical plane for {}",
            path.display()
        );
    }

    if checked == 0 {
        eprintln!("skipping targeted CZI fixture test; no targeted CZI fixtures are present");
    }
}

#[test]
fn external_czi_real_pyramid_sets_have_expected_directory_structure() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external CZI fixture tests");
        return;
    };

    let fixtures = [
        (
            "czi/openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-JXR.czi",
            96,
        ),
        (
            "czi/openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-Cropped.czi",
            150,
        ),
    ];

    let mut checked = 0usize;
    for (relative, expected_reduced_xy_entries) in fixtures {
        let path = fixture_path(&root, relative);
        if !path.exists() {
            eprintln!("skipping missing external fixture {}", path.display());
            continue;
        }
        checked += 1;

        let reduced_xy_entries = czi_reduced_stored_xy_entry_count(&path);
        assert_eq!(
            reduced_xy_entries,
            expected_reduced_xy_entries,
            "wrong CZI reduced stored X/Y entry count for {}",
            path.display()
        );
    }

    if checked == 0 {
        eprintln!(
            "skipping targeted CZI pyramid test; no targeted CZI pyramid fixtures are present"
        );
    }
}

#[test]
fn external_czi_manifest_tracks_targeted_mosaic_candidates() {
    let manifest = include_str!("../external-fixtures/manifests/fixture_sets.tsv");
    let expected = [
        (
            "czi-openslide-zeiss5-smoke",
            "52281408",
            "https://openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-Flat.czi",
            "external-fixtures/data/czi/openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-Flat.czi",
        ),
        (
            "czi-openslide-zeiss5-feature",
            "68811968",
            "https://openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-JXR.czi",
            "external-fixtures/data/czi/openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-JXR.czi",
        ),
        (
            "czi-openslide-zeiss5-feature",
            "80002752",
            "https://openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-Cropped.czi",
            "external-fixtures/data/czi/openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-Cropped.czi",
        ),
        (
            "czi-openslide-zeiss5-pyramid",
            "68811968",
            "https://openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-JXR.czi",
            "external-fixtures/data/czi/openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-JXR.czi",
        ),
        (
            "czi-openslide-zeiss5-pyramid",
            "80002752",
            "https://openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-Cropped.czi",
            "external-fixtures/data/czi/openslide.cs.cmu.edu/download/openslide-testdata/Zeiss/Zeiss-5-Cropped.czi",
        ),
        (
            "czi-synthetic-tile-smoke",
            "2585792",
            "https://zenodo.org/records/7015307/files/S=2_2x2_CH=1.czi?download=1",
            "external-fixtures/data/czi/zenodo.org/records/7015307/files/S=2_2x2_CH=1.czi",
        ),
        (
            "czi-synthetic-tile-feature",
            "31262496",
            "https://zenodo.org/records/7015307/files/W96_B2+B4_S=2_T=1=Z=1_C=1_Tile=5x9.czi?download=1",
            "external-fixtures/data/czi/zenodo.org/records/7015307/files/W96_B2+B4_S=2_T=1=Z=1_C=1_Tile=5x9.czi",
        ),
        (
            "czi-synthetic-tile-regression-large",
            "737698784",
            "https://zenodo.org/records/7015307/files/W96_B2+B4_S=2_T=2=Z=4_C=3_Tile=5x9.czi?download=1",
            "external-fixtures/data/czi/zenodo.org/records/7015307/files/W96_B2+B4_S=2_T=2=Z=4_C=3_Tile=5x9.czi",
        ),
    ];

    for (set, size, url, path) in expected {
        let found = manifest.lines().any(|line| {
            let fields = line.split('\t').collect::<Vec<_>>();
            fields.len() >= 5
                && fields[0] == set
                && fields[1] == "czi"
                && fields[2] == size
                && fields[3] == url
                && fields[4] == path
        });
        assert!(found, "missing targeted CZI manifest row for {path}");
    }
}

#[test]
fn external_dicom_j2ki_smoke_set_opens_and_reads_first_plane() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external DICOM fixture tests");
        return;
    };

    let fixtures = [
        (
            "dicom/downloads.openmicroscopy.org/nema/WG04/IMAGES/J2KI/CT2_J2KI",
            512,
            512,
            1,
            524_288,
        ),
        (
            "dicom/downloads.openmicroscopy.org/nema/WG04/IMAGES/J2KI/MR1_J2KI",
            512,
            512,
            1,
            524_288,
        ),
        (
            "dicom/downloads.openmicroscopy.org/nema/WG04/IMAGES/J2KI/NM1_J2KI",
            256,
            1024,
            1,
            524_288,
        ),
    ];

    for (relative, size_x, size_y, image_count, plane_len) in fixtures {
        let path = fixture_path(&root, relative);
        assert!(path.exists(), "missing external fixture {}", path.display());

        let mut reader = ImageReader::open(&path)
            .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
        let meta = reader.metadata().clone();
        assert_eq!(meta.size_x, size_x, "wrong width for {}", path.display());
        assert_eq!(meta.size_y, size_y, "wrong height for {}", path.display());
        assert_eq!(
            meta.image_count,
            image_count,
            "wrong plane count for {}",
            path.display()
        );

        let plane = reader.open_bytes(0).unwrap_or_else(|err| {
            panic!("failed to read first plane from {}: {err}", path.display())
        });
        assert_eq!(
            plane.len(),
            plane_len,
            "wrong first plane byte length for {}",
            path.display()
        );
    }
}

#[test]
fn external_sdt_smoke_opens_and_reads_first_plane() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external SDT fixture tests");
        return;
    };

    let path = fixture_path(
        &root,
        "sdt/downloads.openmicroscopy.org/gh-4198/FocalCheck_A1_20x_8xzoom_800nm.sdt",
    );
    if !path.exists() {
        eprintln!("skipping missing external fixture {}", path.display());
        return;
    }

    let mut reader = ImageReader::open(&path)
        .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
    let meta = reader.metadata().clone();
    assert_eq!(meta.size_x, 512, "wrong width for {}", path.display());
    assert_eq!(meta.size_y, 512, "wrong height for {}", path.display());
    assert_eq!(
        meta.image_count,
        8192,
        "wrong plane count for {}",
        path.display()
    );

    let plane = reader
        .open_bytes(0)
        .unwrap_or_else(|err| panic!("failed to read first plane from {}: {err}", path.display()));
    assert_eq!(
        plane.len(),
        524_288,
        "wrong first plane byte length for {}",
        path.display()
    );
}

#[test]
fn external_nrrd_sidecar_smoke_set_opens_and_reads_first_plane() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external NRRD fixture tests");
        return;
    };

    let fixtures = [
        (
            "nrrd/downloads.openmicroscopy.org/gordon/dt-helix.nhdr",
            38,
            39,
            40,
            7,
            41_496,
        ),
        (
            "nrrd/downloads.openmicroscopy.org/glencoe/version4/dt-helix.nhdr",
            38,
            39,
            40,
            7,
            41_496,
        ),
        (
            "nrrd/downloads.openmicroscopy.org/gordon/gk2-rcc-mask.nhdr",
            148,
            190,
            160,
            7,
            787_360,
        ),
    ];

    for (relative, size_x, size_y, image_count, size_c, plane_len) in fixtures {
        let path = fixture_path(&root, relative);
        assert!(path.exists(), "missing external fixture {}", path.display());

        let mut reader = ImageReader::open(&path)
            .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
        let meta = reader.metadata().clone();
        assert_eq!(meta.size_x, size_x, "wrong width for {}", path.display());
        assert_eq!(meta.size_y, size_y, "wrong height for {}", path.display());
        assert_eq!(
            meta.image_count,
            image_count,
            "wrong plane count for {}",
            path.display()
        );
        assert_eq!(
            meta.size_c,
            size_c,
            "wrong channel count for {}",
            path.display()
        );

        let plane = reader.open_bytes(0).unwrap_or_else(|err| {
            panic!("failed to read first plane from {}: {err}", path.display())
        });
        assert_eq!(
            plane.len(),
            plane_len,
            "wrong first plane byte length for {}",
            path.display()
        );
    }
}

#[test]
fn external_multi_format_smoke_set_opens_and_reads_first_plane() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external fixture smoke tests");
        return;
    };

    let fixtures = [
        (
            "amiramesh/downloads.openmicroscopy.org/imagesc-50585/BF_Check.tif",
            100,
            100,
            1,
            10_000,
        ),
        (
            "amiramesh/downloads.openmicroscopy.org/imagesc-50585/BF_CheckMIB.am",
            100,
            100,
            1,
            10_000,
        ),
        (
            "amiramesh/downloads.openmicroscopy.org/imagesc-50585/BF_CheckAmira.am",
            100,
            100,
            1,
            10_000,
        ),
        (
            "ecat7/downloads.openmicroscopy.org/torsten/gradient-512x512x10.v",
            512,
            512,
            10,
            524_288,
        ),
        (
            "cellomics/downloads.openmicroscopy.org/BBBC001/AS_09125_050118150001_A03f00d0.DIB",
            512,
            512,
            1,
            524_288,
        ),
        (
            "mrc/downloads.openmicroscopy.org/EMDB/EMD-2225/EMD-2225.map",
            128,
            128,
            128,
            65_536,
        ),
        (
            "nrrd/downloads.openmicroscopy.org/gordon/dt-helix.nhdr",
            38,
            39,
            40,
            41_496,
        ),
        (
            "nrrd/downloads.openmicroscopy.org/glencoe/version4/dt-helix.nhdr",
            38,
            39,
            40,
            41_496,
        ),
        (
            "nrrd/downloads.openmicroscopy.org/gordon/gk2-rcc-mask.nhdr",
            148,
            190,
            160,
            787_360,
        ),
        (
            "gatan/downloads.openmicroscopy.org/imagesc-36590/SmallMontage0000.dm4",
            1024,
            1024,
            1,
            4_194_304,
        ),
        (
            "gatan/downloads.openmicroscopy.org/imagesc-36590/SmallMontage0001.dm4",
            1024,
            1024,
            1,
            4_194_304,
        ),
        (
            "gatan/downloads.openmicroscopy.org/imagesc-36590/SmallMontage0002.dm4",
            1024,
            1024,
            1,
            4_194_304,
        ),
        (
            "ome-tiff/downloads.openmicroscopy.org/2008-09/single-image.ome.tiff",
            6,
            4,
            1,
            24,
        ),
        (
            "ome-tiff/downloads.openmicroscopy.org/2008-09/multi-pixel-default.ome.tiff",
            6,
            4,
            1,
            24,
        ),
        (
            "ome-tiff/downloads.openmicroscopy.org/2008-09/multi-pixel-aquired.ome.tiff",
            6,
            4,
            1,
            24,
        ),
        (
            "png/downloads.openmicroscopy.org/user-1/dataset-user-1/user-1%2001%20TEST.png",
            285,
            285,
            1,
            324_900,
        ),
        (
            "png/downloads.openmicroscopy.org/user-1/Pdataset-user-1/user-1%2007%20TEST.png",
            285,
            285,
            1,
            324_900,
        ),
        (
            "png/downloads.openmicroscopy.org/user-10/dataset-user-10/user-10%2001%20TEST.png",
            285,
            285,
            1,
            324_900,
        ),
    ];

    for (relative, size_x, size_y, image_count, plane_len) in fixtures {
        let path = fixture_path(&root, relative);
        if !path.exists() {
            eprintln!("skipping missing external fixture {}", path.display());
            continue;
        }

        let mut reader = ImageReader::open(&path)
            .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
        let meta = reader.metadata().clone();
        assert_eq!(meta.size_x, size_x, "wrong width for {}", path.display());
        assert_eq!(meta.size_y, size_y, "wrong height for {}", path.display());
        assert_eq!(
            meta.image_count,
            image_count,
            "wrong plane count for {}",
            path.display()
        );

        let plane = reader.open_bytes(0).unwrap_or_else(|err| {
            panic!("failed to read first plane from {}: {err}", path.display())
        });
        assert_eq!(
            plane.len(),
            plane_len,
            "wrong first plane byte length for {}",
            path.display()
        );
    }
}

#[test]
fn external_targeted_public_fixture_sets_open_supported_images() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run targeted external fixture tests");
        return;
    };

    let readable_fixtures = [
        "dcimg/downloads.openmicroscopy.org/zenodo-14287640/Cell09/Cell09_642_000_000.dcimg",
        "dcimg/downloads.openmicroscopy.org/zenodo-14287640/Cell09/Cell09_642_000_001.dcimg",
        "dcimg/downloads.openmicroscopy.org/zenodo-14287640/Cell09/Cell09_642_000_002.dcimg",
        "cv7000/downloads.openmicroscopy.org/cpg0016/Dest21053D1-15214/Dest210531-152149_A01_T0001F001L01A01Z01C01.tif",
        "perkinelmer-columbus/downloads.openmicroscopy.org/idr0019/22_lines_HC_EGF_200145913/002003-11.tif",
        "perkinelmer-columbus/downloads.openmicroscopy.org/idr0019/22_lines_HC_EGF_200145913/002003-10.tif",
        "perkinelmer-columbus/downloads.openmicroscopy.org/idr0019/22_lines_HC_EGF_200145913/002003-2.tif",
        "incell3000/downloads.openmicroscopy.org/BBBC013/BBBC013_v1_images_bmp/Channel1-01-A-01.BMP",
        "incell3000/downloads.openmicroscopy.org/BBBC013/BBBC013_v1_images_bmp/Channel1-02-A-02.BMP",
        "incell3000/downloads.openmicroscopy.org/BBBC013/BBBC013_v1_images_bmp/Channel1-03-A-03.BMP",
        "scanr/downloads.openmicroscopy.org/idr0009/0307-10--2007-05-30/data/--W00002--P00001--Z00000--T00000--nucleus-dapi.tif",
        "scanr/downloads.openmicroscopy.org/idr0009/0307-10--2007-05-30/data/--W00002--P00001--Z00000--T00000--pm-647.tif",
        "scanr/downloads.openmicroscopy.org/idr0009/0307-10--2007-05-30/data/--W00002--P00001--Z00000--T00000--vsvg-cfp.tif",
    ];
    let metadata_fixtures = [
        "hamamatsu-ndpi/downloads.openmicroscopy.org/manuel/test3-TRITC%202%20%28560%29.ndpi",
        "hamamatsu-ndpi/downloads.openmicroscopy.org/manuel/test3-DAPI%202%20%28387%29%20.ndpi",
        "hamamatsu-ndpi/downloads.openmicroscopy.org/manuel/test3-FITC%202%20%28485%29.ndpi",
    ];

    let mut opened = 0usize;
    for relative in readable_fixtures {
        if open_external_image_if_present(&root, relative) {
            opened += 1;
        }
    }
    for relative in metadata_fixtures {
        if open_external_metadata_if_present(&root, relative) {
            opened += 1;
        }
    }
    if opened == 0 {
        eprintln!("skipping targeted external fixture test; no targeted fixtures are present");
    }
}

#[test]
fn external_mrc_emd_2225_records_orientation_header_metadata() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external MRC fixture tests");
        return;
    };

    let path = fixture_path(
        &root,
        "mrc/downloads.openmicroscopy.org/EMDB/EMD-2225/EMD-2225.map",
    );
    if !path.exists() {
        eprintln!("skipping missing external fixture {}", path.display());
        return;
    }

    let mut reader = ImageReader::open(&path)
        .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
    let meta = reader.metadata().clone();

    assert_eq!(meta.size_x, 128);
    assert_eq!(meta.size_y, 128);
    assert_eq!(meta.size_z, 128);
    assert_eq!(meta.image_count, 128);
    assert_eq!(meta.pixel_type, PixelType::Float32);
    assert!(meta.is_little_endian);

    assert_metadata_int(&meta.series_metadata, "MapColumnAxis", 1);
    assert_metadata_int(&meta.series_metadata, "MapRowAxis", 2);
    assert_metadata_int(&meta.series_metadata, "MapSectionAxis", 3);
    assert_metadata_int(&meta.series_metadata, "ColumnStart", -64);
    assert_metadata_int(&meta.series_metadata, "RowStart", -64);
    assert_metadata_int(&meta.series_metadata, "SectionStart", -64);
    assert_metadata_float(&meta.series_metadata, "OriginY", 0.0);
    assert_metadata_bool(&meta.series_metadata, "FlipY", true);

    let row_bytes = meta.size_x as usize * meta.pixel_type.bytes_per_sample();
    let stored_first_row = read_file_row(&path, 1024, row_bytes);
    let stored_last_row = read_file_row(&path, 1024 + 127 * row_bytes as u64, row_bytes);
    assert_eq!(crc32_ieee(&stored_first_row), 0xb0c6_fc1f);
    assert_eq!(crc32_ieee(&stored_last_row), 0x1ad4_2bb0);
    assert_ne!(
        stored_first_row, stored_last_row,
        "EMD-2225 edge rows must differ for this row-flip regression check"
    );

    let plane = reader.open_bytes(0).unwrap_or_else(|err| {
        panic!(
            "failed to read first MRC plane from {}: {err}",
            path.display()
        )
    });
    assert_eq!(plane.len(), meta.size_x as usize * meta.size_y as usize * 4);
    assert_eq!(
        crc32_ieee(&plane[..row_bytes]),
        0x1ad4_2bb0,
        "decoded first row should be the stored last row when FlipY=true"
    );
    let decoded_last_row = &plane[plane.len() - row_bytes..];
    assert_eq!(
        crc32_ieee(decoded_last_row),
        0xb0c6_fc1f,
        "decoded last row should be the stored first row when FlipY=true"
    );
}

#[test]
fn external_mrc_emdb_small_and_feature_sets_record_axis_metadata() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external MRC fixture tests");
        return;
    };

    let fixtures = [
        (
            "mrc/raw.githubusercontent.com/ccpem/mrcfile/master/tests/test_data/EMD-3197.map",
            20,
            20,
            20,
            1,
            2,
            3,
            Some(-2),
        ),
        (
            "mrc/raw.githubusercontent.com/ccpem/mrcfile/master/tests/test_data/EMD-3001.map",
            73,
            43,
            25,
            3,
            1,
            2,
            None,
        ),
    ];

    let mut checked = 0usize;
    for (relative, size_x, size_y, size_z, mapc, mapr, maps, column_start) in fixtures {
        let path = fixture_path(&root, relative);
        if !path.exists() {
            eprintln!("skipping missing external fixture {}", path.display());
            continue;
        }
        checked += 1;

        let mut reader = ImageReader::open(&path)
            .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
        let meta = reader.metadata().clone();
        assert_eq!(meta.size_x, size_x, "wrong width for {}", path.display());
        assert_eq!(meta.size_y, size_y, "wrong height for {}", path.display());
        assert_eq!(meta.size_z, size_z, "wrong depth for {}", path.display());
        assert_eq!(
            meta.image_count,
            size_z,
            "wrong plane count for {}",
            path.display()
        );
        assert_eq!(
            meta.pixel_type,
            PixelType::Float32,
            "wrong pixel type for {}",
            path.display()
        );
        assert_metadata_int(&meta.series_metadata, "MapColumnAxis", mapc);
        assert_metadata_int(&meta.series_metadata, "MapRowAxis", mapr);
        assert_metadata_int(&meta.series_metadata, "MapSectionAxis", maps);
        if let Some(column_start) = column_start {
            assert_metadata_int(&meta.series_metadata, "ColumnStart", column_start);
        }
        if mapr != 2 {
            assert_metadata_bool(&meta.series_metadata, "FlipY", false);
        }

        let plane = reader.open_bytes(0).unwrap_or_else(|err| {
            panic!(
                "failed to read first MRC plane from {}: {err}",
                path.display()
            )
        });
        assert_eq!(
            plane.len(),
            size_x as usize * size_y as usize * 4,
            "wrong first MRC plane byte length for {}",
            path.display()
        );

        if relative.ends_with("EMD-3197.map") {
            let bytes_per_sample = 4u64;
            let row_bytes = size_x as u64 * bytes_per_sample;
            let plane_bytes = size_x as u64 * size_y as u64 * bytes_per_sample;

            let public_samples = [
                (0u64, 0u64, 0u64, -1.801_309_1f32),
                (9, 6, 13, 4.620_779_0),
                (9, 6, 14, 5.037_393_1),
                (19, 19, 19, 1.307_857_4),
            ];
            for (z, y, x, expected) in public_samples {
                let offset = 1024 + z * plane_bytes + y * row_bytes + x * bytes_per_sample;
                assert_close_f32(
                    read_file_f32(&path, offset),
                    expected,
                    "EMD-3197 upstream mrcfile storage-order sample",
                );
            }

            assert_metadata_bool(&meta.series_metadata, "FlipY", true);
            assert_close_f32(
                f32::from_le_bytes(
                    plane[plane.len() - row_bytes as usize..][0..4]
                        .try_into()
                        .unwrap(),
                ),
                -1.801_309_1,
                "EMD-3197 decoded last row should contain upstream stored first-row sample",
            );
        }
    }

    if checked == 0 {
        eprintln!("skipping targeted MRC fixture test; no targeted MRC fixtures are present");
    }
}

#[test]
fn external_mrc_imod_signed_mode0_records_signed_orientation_evidence() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external MRC fixture tests");
        return;
    };

    let path = fixture_path(
        &root,
        "mrc/bio3d.colorado.edu/imod/nightlyBuilds/ImodTests/fortIOtests/tst0.sbyte",
    );
    if !path.exists() {
        eprintln!("skipping missing external fixture {}", path.display());
        return;
    }

    let mut reader = ImageReader::open(&path)
        .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
    let meta = reader.metadata().clone();

    assert_eq!(meta.size_x, 101);
    assert_eq!(meta.size_y, 128);
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.image_count, 1);
    assert_eq!(meta.pixel_type, PixelType::Int8);
    assert_metadata_bool(&meta.series_metadata, "FlipY", true);

    let row_bytes = meta.size_x as usize;
    let stored_first_row = read_file_row(&path, 1024, row_bytes);
    let stored_last_row = read_file_row(&path, 1024 + 127 * row_bytes as u64, row_bytes);
    assert_eq!(crc32_ieee(&stored_first_row), 0x5183_44f2);
    assert_eq!(crc32_ieee(&stored_last_row), 0xfb1f_abcb);

    let plane = reader.open_bytes(0).unwrap_or_else(|err| {
        panic!(
            "failed to read first IMOD signed-byte MRC plane from {}: {err}",
            path.display()
        )
    });
    assert_eq!(plane.len(), meta.size_x as usize * meta.size_y as usize);
    assert_eq!(crc32_ieee(&plane[..row_bytes]), 0xfb1f_abcb);
    assert_eq!(crc32_ieee(&plane[plane.len() - row_bytes..]), 0x5183_44f2);
}

#[test]
fn external_nikon_raw_manifest_tracks_targeted_no_download_candidates() {
    let manifest = include_str!("../external-fixtures/manifests/fixture_sets.tsv");
    let expected = [
        (
            "nikon-nef-d70-compression-34713-smoke",
            "nikon-nef",
            "https://raw.pixls.us/data/Nikon/D70/20170902_0047.NEF",
            "external-fixtures/data/nikon-nef/raw.pixls.us/data/Nikon/D70/20170902_0047.NEF",
        ),
        (
            "nikon-nef-d40-d50-alternates-smoke",
            "nikon-nef",
            "https://raw.pixls.us/data/Nikon/D40/DSC_1842.NEF",
            "external-fixtures/data/nikon-nef/raw.pixls.us/data/Nikon/D40/DSC_1842.NEF",
        ),
        (
            "nikon-nef-d40-d50-alternates-smoke",
            "nikon-nef",
            "https://raw.pixls.us/data/Nikon/D50/DSC_5155.NEF",
            "external-fixtures/data/nikon-nef/raw.pixls.us/data/Nikon/D50/DSC_5155.NEF",
        ),
        (
            "nikon-nrw-p7000-smoke",
            "nikon-nrw",
            "https://raw.pixls.us/data/Nikon/Coolpix%20P7000/RAW_NIKON_P7000.NRW",
            "external-fixtures/data/nikon-nrw/raw.pixls.us/data/Nikon/Coolpix%20P7000/RAW_NIKON_P7000.NRW",
        ),
    ];

    for (set, format, url, path) in expected {
        let found = manifest.lines().any(|line| {
            let fields = line.split('\t').collect::<Vec<_>>();
            fields.len() >= 5
                && fields[0] == set
                && fields[1] == format
                && fields[3] == url
                && fields[4] == path
        });
        assert!(found, "missing targeted Nikon RAW manifest row for {path}");
    }
}

#[test]
fn external_nikon_nef_34713_reports_explicit_unsupported_decoder_contract() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external Nikon RAW fixture tests");
        return;
    };

    let fixtures = [
        "nikon-nef/raw.pixls.us/data/Nikon/D70/20170902_0047.NEF",
        "nikon-nef/raw.pixls.us/data/Nikon/D40/DSC_1842.NEF",
        "nikon-nef/raw.pixls.us/data/Nikon/D50/DSC_5155.NEF",
    ];

    let mut checked = 0usize;
    for relative in fixtures {
        let path = fixture_path(&root, relative);
        if !path.exists() {
            eprintln!("skipping missing external fixture {}", path.display());
            continue;
        }
        checked += 1;

        let mut reader = ImageReader::open(&path)
            .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
        assert!(
            reader.metadata().size_x > 0,
            "zero width for {}",
            path.display()
        );
        assert!(
            reader.metadata().size_y > 0,
            "zero height for {}",
            path.display()
        );
        assert!(
            reader.metadata().image_count > 0,
            "zero plane count for {}",
            path.display()
        );

        let mut saw_nikon_34713 = false;
        for series in 0..reader.series_count() {
            reader
                .set_series(series)
                .unwrap_or_else(|err| panic!("failed to select series {series}: {err}"));
            for plane_index in 0..reader.metadata().image_count {
                let meta = reader.metadata().clone();
                match reader.open_bytes(plane_index) {
                    Ok(plane) => {
                        assert!(
                            !plane.is_empty(),
                            "empty decoded Nikon plane for {}",
                            path.display()
                        );
                        if meta.bits_per_pixel == 12 && meta.size_x >= 3000 && meta.size_y >= 2000
                        {
                            saw_nikon_34713 = true;
                            if relative.ends_with("D70/20170902_0047.NEF") {
                                assert_eq!(meta.size_x, 3040);
                                assert_eq!(meta.size_y, 2014);
                                assert_eq!(plane.len(), 9_183_840);
                                let mut hash = 1_469_598_103_934_665_603u64;
                                for byte in &plane {
                                    hash ^= u64::from(*byte);
                                    hash = hash.wrapping_mul(1_099_511_628_211);
                                }
                                assert_eq!(hash, 3_264_415_168_095_907_119);
                            }
                        }
                    }
                    Err(BioFormatsError::UnsupportedFormat(message))
                        if message.contains("Nikon NEF compression 34713")
                            && (message.contains(
                                "maker-note IFD tag 150 metadata was parsed",
                            ) || (message.contains("maker-note IFD tag 150")
                                && message.contains("compressed strip byte count/maxBytes"))) =>
                    {
                        saw_nikon_34713 = true;
                    }
                    Err(err) => panic!(
                        "unexpected Nikon NEF decode error for series {series} plane {plane_index} in {}: {err}",
                        path.display()
                    ),
                }
            }
        }
        assert!(
            saw_nikon_34713,
            "no Nikon compression 34713 plane was reached for {}",
            path.display()
        );
    }

    if checked == 0 {
        eprintln!("skipping Nikon NEF fixture test; no Nikon NEF fixtures are present");
    }
}

#[test]
fn external_nikon_d70_34713_matches_java_nikon_codec_packed_output_if_tools_present() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external Nikon RAW fixture tests");
        return;
    };
    let path = fixture_path(
        &root,
        "nikon-nef/raw.pixls.us/data/Nikon/D70/20170902_0047.NEF",
    );
    if !path.exists() {
        eprintln!("skipping missing external fixture {}", path.display());
        return;
    }
    let jar = PathBuf::from("bioformats_package.jar");
    if !jar.exists() {
        eprintln!("skipping Nikon Java parity test; bioformats_package.jar is absent");
        return;
    }
    if Command::new("javac").arg("-version").output().is_err() {
        eprintln!("skipping Nikon Java parity test; javac is not available");
        return;
    }
    if Command::new("java").arg("-version").output().is_err() {
        eprintln!("skipping Nikon Java parity test; java is not available");
        return;
    }

    let mut reader = ImageReader::open(&path)
        .unwrap_or_else(|err| panic!("failed to open {}: {err}", path.display()));
    let mut rust_plane = None;
    for series in 0..reader.series_count() {
        reader
            .set_series(series)
            .unwrap_or_else(|err| panic!("failed to select series {series}: {err}"));
        let meta = reader.metadata().clone();
        if meta.size_x == 3040 && meta.size_y == 2014 && meta.bits_per_pixel == 12 {
            rust_plane = Some(
                reader
                    .open_bytes(0)
                    .unwrap_or_else(|err| panic!("failed to decode Nikon RAW series: {err}")),
            );
            break;
        }
    }
    let rust_plane = rust_plane.expect("D70 Nikon RAW series was not exposed");
    let rust_crc = crc32_ieee(&rust_plane);

    let oracle = java_nikon_codec_oracle(&path, &jar)
        .unwrap_or_else(|err| panic!("failed to run Java NikonCodec oracle: {err}"));
    assert_eq!(oracle.width, 3040);
    assert_eq!(oracle.height, 2014);
    assert_eq!(oracle.bits_per_sample, 12);
    assert_eq!(rust_plane.len(), oracle.bytes);
    assert_eq!(rust_crc, oracle.crc32);
}

#[derive(Debug)]
struct NikonCodecOracle {
    width: u32,
    height: u32,
    bits_per_sample: u16,
    bytes: usize,
    crc32: u32,
}

fn java_nikon_codec_oracle(path: &Path, jar: &Path) -> std::io::Result<NikonCodecOracle> {
    let dir =
        std::env::temp_dir().join(format!("bioformats-rs-nikon-oracle-{}", std::process::id()));
    fs::create_dir_all(&dir)?;
    let source = dir.join("NikonCodecPackedOracle.java");
    fs::write(&source, NIKON_CODEC_PACKED_ORACLE_JAVA)?;

    let compile = Command::new("javac")
        .arg("-cp")
        .arg(jar)
        .arg(&source)
        .output()?;
    if !compile.status.success() {
        let message = String::from_utf8_lossy(&compile.stderr).into_owned();
        let _ = fs::remove_dir_all(&dir);
        return Err(std::io::Error::new(std::io::ErrorKind::Other, message));
    }

    let classpath = format!("{}:{}", jar.display(), dir.display());
    let output = Command::new("java")
        .arg("-cp")
        .arg(classpath)
        .arg("NikonCodecPackedOracle")
        .arg(path)
        .output()?;
    let _ = fs::remove_dir_all(&dir);
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).into_owned();
        return Err(std::io::Error::new(std::io::ErrorKind::Other, message));
    }

    parse_nikon_codec_oracle_output(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::Other, "unparseable oracle output"))
}

fn parse_nikon_codec_oracle_output(output: &str) -> Option<NikonCodecOracle> {
    let line = output
        .lines()
        .find(|line| line.starts_with("nikon-codec "))?;
    let mut width = None;
    let mut height = None;
    let mut bits_per_sample = None;
    let mut bytes = None;
    let mut crc32 = None;
    for field in line.split_whitespace().skip(1) {
        let (key, value) = field.split_once('=')?;
        match key {
            "width" => width = value.parse().ok(),
            "height" => height = value.parse().ok(),
            "bps" => bits_per_sample = value.parse().ok(),
            "bytes" => bytes = value.parse().ok(),
            "crc32" => crc32 = u32::from_str_radix(value, 16).ok(),
            _ => {}
        }
    }
    Some(NikonCodecOracle {
        width: width?,
        height: height?,
        bits_per_sample: bits_per_sample?,
        bytes: bytes?,
        crc32: crc32?,
    })
}

const NIKON_CODEC_PACKED_ORACLE_JAVA: &str = r#"
import java.io.ByteArrayOutputStream;
import java.util.Arrays;
import java.util.zip.CRC32;

import loci.common.ByteArrayHandle;
import loci.common.Constants;
import loci.common.RandomAccessInputStream;
import loci.formats.codec.NikonCodec;
import loci.formats.codec.NikonCodecOptions;
import loci.formats.tiff.IFD;
import loci.formats.tiff.TiffCompression;
import loci.formats.tiff.TiffParser;

public class NikonCodecPackedOracle {
  public static void main(String[] args) throws Exception {
    String path = args[0];
    try (RandomAccessInputStream in = new RandomAccessInputStream(path)) {
      TiffParser parser = new TiffParser(in);
      IFD raw = null;
      for (Object entry : parser.getIFDs()) {
        IFD ifd = (IFD) entry;
        parser.fillInIFD(ifd);
        if (ifd.getCompression() == TiffCompression.NIKON &&
          ifd.getImageWidth() >= 3000 && ifd.getImageLength() >= 2000) {
          raw = ifd;
          break;
        }
      }
      if (raw == null) throw new IllegalStateException("no Nikon raw IFD");

      IFD note = null;
      for (Object entry : parser.getExifIFDs()) {
        IFD exif = (IFD) entry;
        parser.fillInIFD(exif);
        byte[] maker = (byte[]) exif.get(IFD.MAKER_NOTE);
        if (maker == null) continue;
        int extra = new String(maker, 0, Math.min(10, maker.length), Constants.ENCODING)
          .startsWith("Nikon") ? 10 : 0;
        byte[] nested = new byte[maker.length];
        System.arraycopy(maker, extra, nested, 0, maker.length - extra);
        try (RandomAccessInputStream makerNote =
          new RandomAccessInputStream(new ByteArrayHandle(nested))) {
          note = new TiffParser(makerNote).getFirstIFD();
        }
        if (note != null) break;
      }
      if (note == null) throw new IllegalStateException("no Nikon maker note");

      NikonCodecOptions options = optionsFromMakerNote((byte[]) note.get(150), raw);
      int[] basePredictor = Arrays.copyOf(options.vPredictor, options.vPredictor.length);
      ByteArrayOutputStream out = new ByteArrayOutputStream();
      NikonCodec codec = new NikonCodec();
      long[] offsets = raw.getStripOffsets();
      long[] byteCounts = raw.getStripByteCounts();
      for (int i = 0; i < byteCounts.length; i++) {
        byte[] compressed = new byte[(int) byteCounts[i]];
        in.seek(offsets[i]);
        in.read(compressed);
        options.maxBytes = (int) byteCounts[i];
        options.vPredictor = Arrays.copyOf(basePredictor, basePredictor.length);
        out.write(codec.decompress(compressed, options));
      }

      byte[] decoded = out.toByteArray();
      CRC32 crc = new CRC32();
      crc.update(decoded);
      System.out.println("nikon-codec width=" + options.width +
        " height=" + options.height + " bps=" + options.bitsPerSample +
        " bytes=" + decoded.length + " crc32=" + Long.toHexString(crc.getValue()));
    }
  }

  private static NikonCodecOptions optionsFromMakerNote(byte[] tag150, IFD raw)
    throws Exception {
    RandomAccessInputStream s =
      new RandomAccessInputStream(new ByteArrayHandle(tag150));
    byte check1 = s.readByte();
    byte check2 = s.readByte();
    boolean lossyCompression = check1 != 0x46;
    int[] vPredictor = new int[4];
    for (int i = 0; i < vPredictor.length; i++) vPredictor[i] = s.readShort();
    int[] curve = new int[16385];
    int bps = raw.getBitsPerSample()[0];
    int max = 1 << bps & 0x7fff;
    int step = 0;
    int csize = s.readShort();
    int split = -1;
    if (csize > 1) step = max / (csize - 1);
    if (check1 == 0x44 && check2 == 0x20 && step > 0) {
      for (int i = 0; i < csize; i++) curve[i * step] = s.readShort();
      for (int i = 0; i < max; i++) {
        int n = i % step;
        curve[i] = (curve[i - n] * (step - n) + curve[i - n + step] * n) / step;
      }
      s.seek(562);
      split = s.readShort();
    }
    else {
      Arrays.fill(curve, (int) Math.pow(2, bps) - 1);
      int nElements = (int) (s.length() - s.getFilePointer()) / 2;
      if (nElements < 100) {
        for (int i = 0; i < curve.length; i++) curve[i] = (short) i;
      }
      else {
        for (int i = 0; i < nElements; i++) curve[i] = s.readShort();
      }
    }
    s.close();

    NikonCodecOptions options = new NikonCodecOptions();
    options.width = (int) raw.getImageWidth();
    options.height = (int) raw.getImageLength();
    options.bitsPerSample = bps;
    options.curve = curve;
    options.vPredictor = vPredictor;
    options.lossless = !lossyCompression;
    options.split = split;
    return options;
  }
}
"#;

#[test]
fn external_nikon_nrw_candidate_opens_metadata_if_present() {
    let Some(root) = external_root() else {
        eprintln!("set BIOFORMATS_RS_EXTERNAL_FIXTURES to run external Nikon RAW fixture tests");
        return;
    };

    open_external_metadata_if_present(
        &root,
        "nikon-nrw/raw.pixls.us/data/Nikon/Coolpix%20P7000/RAW_NIKON_P7000.NRW",
    );
}
