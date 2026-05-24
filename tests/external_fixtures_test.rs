use bioformats::ImageReader;
use std::path::{Path, PathBuf};

fn external_root() -> Option<PathBuf> {
    std::env::var_os("BIOFORMATS_RS_EXTERNAL_FIXTURES").map(PathBuf::from)
}

fn fixture_path(root: &Path, relative: &str) -> PathBuf {
    root.join(relative)
}

#[test]
fn external_nd2_smoke_set_opens_and_reads_first_plane() {
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
            51_168,
        ),
        (
            "nd2/downloads.openmicroscopy.org/jonas/jonas_nd2Test/Exception_2.nd2",
            696,
            520,
            31,
            723_840,
        ),
        (
            "nd2/downloads.openmicroscopy.org/aryeh/MeOh_high_fluo_003.nd2",
            800,
            600,
            13,
            960_000,
        ),
        (
            "nd2/downloads.openmicroscopy.org/jonas/header_test2.nd2",
            696,
            520,
            20,
            723_840,
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
