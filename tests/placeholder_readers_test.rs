use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats::common::error::BioFormatsError;
use bioformats::formats::misc4::FilePatternReader;
use bioformats::FormatReader;
use bioformats::MetadataValue;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn tmp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!("bioformats_filepattern_{tag}_{nanos}_{n}"))
}

// NOTE: OirReader, VolocityClippingReader, CellWorxReader, ObfReader, I2iReader,
// JdceReader, and PciReader were formerly stubs but are now real readers.
// BrukerOpusReader, IssFlimReader, LambertFlimReader, VolocityLibraryReader, and
// SedatReader/WoolzReader were fabricated readers for formats Bio-Formats has no
// reader for (or duplicates of real readers) and have been DELETED. FilePattern
// now delegates to FileStitcher, so there are no hand-written placeholders left.

#[test]
fn unsupported_hand_written_placeholders_stay_uninitialized() {
    // Kept as a regression marker: when adding a new explicit unsupported
    // detector, test its honest failure in that reader's focused tests instead
    // of reintroducing uninitialized placeholder metadata here.
}

#[test]
fn filepattern_reader_delegates_to_stitcher_for_pattern_files() {
    let dir = tmp_dir("fake");
    std::fs::create_dir_all(&dir).unwrap();
    let f0 = dir.join("fp_z00&sizeX=2&sizeY=1.fake");
    let f1 = dir.join("fp_z01&sizeX=2&sizeY=1.fake");
    std::fs::write(&f0, b"").unwrap();
    std::fs::write(&f1, b"").unwrap();
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, f0.file_name().unwrap().to_str().unwrap()).unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_z, 2);
    assert_eq!(meta.image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![0, 1]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![0, 1]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_explicit_zct_grid() {
    let dir = tmp_dir("zct");
    std::fs::create_dir_all(&dir).unwrap();
    for z in 0..2 {
        for c in 0..2 {
            for t in 0..2 {
                std::fs::write(dir.join(format!("fp_z{z}_c{c}_t{t}.fake")), b"").unwrap();
            }
        }
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "fp_z<0-1>_c<0-1>_t<0-1>.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 2);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 8);
    assert_eq!(reader.open_bytes_region(7, 0, 0, 1, 1).unwrap(), vec![0]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_comma_lists_and_steps() {
    let dir = tmp_dir("lists");
    std::fs::create_dir_all(&dir).unwrap();
    for t in [0, 2, 4] {
        std::fs::write(dir.join(format!("fp_t{t:02}.fake")), b"").unwrap();
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "fp_t<00,02-04:2>.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.size_t, 3);
    assert_eq!(meta.image_count, 3);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_groups_string_channel_labels_and_time() {
    let dir = tmp_dir("labels");
    std::fs::create_dir_all(&dir).unwrap();
    for channel in ["DAPI", "FITC"] {
        for t in 0..2 {
            std::fs::write(dir.join(format!("img_{channel}_t{t}.fake")), b"").unwrap();
        }
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "img_<DAPI,FITC>_t<0-1>.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 4);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern channel 0 name")
            .unwrap()
            .to_string(),
        "DAPI"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern channel 1 name")
            .unwrap()
            .to_string(),
        "FITC"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern Channel 1 Name")
            .unwrap()
            .to_string(),
        "FITC"
    );
    assert_eq!(
        meta.series_metadata
            .get("Channel 1 Name")
            .unwrap()
            .to_string(),
        "FITC"
    );
    assert_eq!(
        meta.series_metadata
            .get("Channel:1:Name")
            .unwrap()
            .to_string(),
        "FITC"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern Channel 1 Label")
            .unwrap()
            .to_string(),
        "FITC"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern axes")
            .unwrap()
            .to_string(),
        "C,T"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern Axes")
            .unwrap()
            .to_string(),
        "C,T"
    );

    let ome = reader.ome_metadata().unwrap();
    assert_eq!(ome.images[0].channels[0].name.as_deref(), Some("DAPI"));
    assert_eq!(ome.images[0].channels[1].name.as_deref(), Some("FITC"));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_preserves_explicit_pattern_metadata_slice() {
    let dir = tmp_dir("explicit_metadata");
    std::fs::create_dir_all(&dir).unwrap();
    for channel in ["DAPI", "FITC"] {
        for t in 0..2 {
            std::fs::write(
                dir.join(format!("img_ch{channel}_t{t}&sizeX=2&sizeY=1.fake")),
                b"",
            )
            .unwrap();
        }
    }
    let pattern = dir.join("stack.pattern");
    let pattern_text = "img_ch<DAPI,FITC>_t[0-1]&sizeX=2&sizeY=1.fake";
    std::fs::write(&pattern, pattern_text).unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 2);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern pattern")
            .unwrap()
            .to_string(),
        dir.join(pattern_text).display().to_string()
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern Pattern")
            .unwrap()
            .to_string(),
        dir.join(pattern_text).display().to_string()
    );
    assert_eq!(
        meta.series_metadata
            .get("File pattern")
            .unwrap()
            .to_string(),
        dir.join(pattern_text).display().to_string()
    );
    assert_eq!(
        meta.series_metadata.get("FilePattern").unwrap().to_string(),
        dir.join(pattern_text).display().to_string()
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern root")
            .unwrap()
            .to_string(),
        dir.display().to_string()
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern Root")
            .unwrap()
            .to_string(),
        dir.display().to_string()
    );
    assert!(matches!(
        meta.series_metadata.get("FilePattern block count"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata.get("FilePattern Block Count"),
        Some(MetadataValue::Int(2))
    ));
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 token")
            .unwrap()
            .to_string(),
        "<DAPI,FITC>"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern Block 0 Token")
            .unwrap()
            .to_string(),
        "<DAPI,FITC>"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern Axis 0 Token")
            .unwrap()
            .to_string(),
        "<DAPI,FITC>"
    );
    assert_eq!(
        meta.series_metadata
            .get("Axis 0 Token")
            .unwrap()
            .to_string(),
        "<DAPI,FITC>"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern Axis 0 Type")
            .unwrap()
            .to_string(),
        "C"
    );
    assert_eq!(
        meta.series_metadata.get("Axis 0 Type").unwrap().to_string(),
        "C"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern Axis 0 Values")
            .unwrap()
            .to_string(),
        "DAPI,FITC"
    );
    assert_eq!(
        meta.series_metadata
            .get("Axis 0 Values")
            .unwrap()
            .to_string(),
        "DAPI,FITC"
    );
    assert!(matches!(
        meta.series_metadata.get("FilePattern Axis 0 Size"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata.get("Axis 0 Size"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata.get("FilePattern block 0 count"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata.get("FilePattern Block 0 Count"),
        Some(MetadataValue::Int(2))
    ));
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 1 token")
            .unwrap()
            .to_string(),
        "[0-1]"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern Block 1 Token")
            .unwrap()
            .to_string(),
        "[0-1]"
    );
    assert!(matches!(
        meta.series_metadata.get("FilePattern block 1 count"),
        Some(MetadataValue::Int(2))
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_names_size_aware_numeric_channel_axis() {
    let dir = tmp_dir("size_aware_channel");
    std::fs::create_dir_all(&dir).unwrap();
    for z in 0..2 {
        for c in 0..2 {
            std::fs::write(
                dir.join(format!("img_z{z}_{c}&sizeX=2&sizeY=1&sizeZ=2.fake")),
                b"",
            )
            .unwrap();
        }
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "img_z<0-1>_<0-1>&sizeX=2&sizeY=1&sizeZ=2.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 4);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 1);
    assert_eq!(meta.image_count, 8);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern axes")
            .unwrap()
            .to_string(),
        "Z,C"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern channel 1 name")
            .unwrap()
            .to_string(),
        "1"
    );
    assert_eq!(
        meta.series_metadata
            .get("Channel 1 Name")
            .unwrap()
            .to_string(),
        "1"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_alphabetic_channel_ranges() {
    let dir = tmp_dir("alpha");
    std::fs::create_dir_all(&dir).unwrap();
    for channel in ["A", "B", "C"] {
        std::fs::write(dir.join(format!("img_c{channel}.fake")), b"").unwrap();
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "img_c<A-C>.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_c, 3);
    assert_eq!(meta.image_count, 3);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 values")
            .unwrap()
            .to_string(),
        "A,B,C"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_descending_ranges() {
    let dir = tmp_dir("descending");
    std::fs::create_dir_all(&dir).unwrap();
    for t in 0..3 {
        std::fs::write(dir.join(format!("img_t{t}.fake")), b"").unwrap();
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "img_t<2-0>.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_t, 3);
    assert_eq!(meta.image_count, 3);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 values")
            .unwrap()
            .to_string(),
        "2,1,0"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_reports_missing_expanded_files() {
    let dir = tmp_dir("missing");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("fp_z0.fake"), b"").unwrap();
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "fp_z<0-1>.fake").unwrap();

    let mut reader = FilePatternReader::new();
    let err = reader.set_id(&pattern).unwrap_err();
    assert!(matches!(err, BioFormatsError::Format(message) if message.contains("missing files")));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_simple_star_glob() {
    let dir = tmp_dir("star_glob");
    std::fs::create_dir_all(&dir).unwrap();
    for t in 0..3 {
        std::fs::write(dir.join(format!("img_t{t:02}&sizeX=2&sizeY=1.fake")), b"").unwrap();
    }
    std::fs::write(dir.join("notes.txt"), b"not an image").unwrap();
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "img_t*.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.size_t, 3);
    assert_eq!(meta.image_count, 3);
    assert_eq!(reader.open_bytes_region(2, 1, 0, 1, 1).unwrap(), vec![1]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_question_glob_and_keeps_axis_metadata() {
    let dir = tmp_dir("question_glob");
    std::fs::create_dir_all(&dir).unwrap();
    for c in 0..2 {
        for t in 0..2 {
            std::fs::write(dir.join(format!("img_c{c}_t{t}.fake")), b"").unwrap();
        }
    }
    std::fs::write(dir.join("img_c10_t0.fake"), b"").unwrap();
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "img_c?_t?.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 4);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern axes")
            .unwrap()
            .to_string(),
        "C,T"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_bracket_numeric_ranges_with_metadata() {
    let dir = tmp_dir("bracket");
    std::fs::create_dir_all(&dir).unwrap();
    for t in 0..3 {
        std::fs::write(dir.join(format!("fp_t{t}.fake")), b"").unwrap();
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "fp_[0-9].fake").unwrap();

    let mut reader = FilePatternReader::new();
    let err = reader.set_id(&pattern).unwrap_err();
    assert!(matches!(err, BioFormatsError::Format(message) if message.contains("missing files")));

    std::fs::write(&pattern, "fp_t[0-2].fake").unwrap();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_t, 3);
    assert_eq!(meta.image_count, 3);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 values")
            .unwrap()
            .to_string(),
        "0,1,2"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_brace_channel_alternation() {
    let dir = tmp_dir("brace");
    std::fs::create_dir_all(&dir).unwrap();
    for channel in ["DAPI", "FITC"] {
        std::fs::write(dir.join(format!("img_{channel}.fake")), b"").unwrap();
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "img_{DAPI,FITC}.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.image_count, 2);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern axes")
            .unwrap()
            .to_string(),
        "C"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern channel 0 name")
            .unwrap()
            .to_string(),
        "DAPI"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern channel 1 name")
            .unwrap()
            .to_string(),
        "FITC"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_nested_brace_and_class_channel_alternation() {
    let dir = tmp_dir("nested_blocks");
    std::fs::create_dir_all(&dir).unwrap();
    for channel in ["DAPI", "FITC0", "FITC1"] {
        std::fs::write(dir.join(format!("img_{channel}.fake")), b"").unwrap();
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "img_{DAPI,FITC[0-1]}.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_c, 3);
    assert_eq!(meta.image_count, 3);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 values")
            .unwrap()
            .to_string(),
        "DAPI,FITC0,FITC1"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern channel 2 name")
            .unwrap()
            .to_string(),
        "FITC1"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_deeper_nested_brace_and_class_alternation() {
    let dir = tmp_dir("deeper_nested_blocks");
    std::fs::create_dir_all(&dir).unwrap();
    for channel in ["DAPI", "FITC0", "FITC1", "TRITC0", "TRITC1"] {
        std::fs::write(dir.join(format!("img_{channel}.fake")), b"").unwrap();
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "img_{DAPI,{FITC,TRITC}[0-1]}.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_c, 5);
    assert_eq!(meta.image_count, 5);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 values")
            .unwrap()
            .to_string(),
        "DAPI,FITC0,FITC1,TRITC0,TRITC1"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern Block 0 Values")
            .unwrap()
            .to_string(),
        "DAPI,FITC0,FITC1,TRITC0,TRITC1"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern Channel 4 Name")
            .unwrap()
            .to_string(),
        "TRITC1"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_matches_overlapping_explicit_channel_labels() {
    let dir = tmp_dir("overlap_labels");
    std::fs::create_dir_all(&dir).unwrap();
    for channel in ["A", "AB"] {
        std::fs::write(dir.join(format!("img_{channel}.fake")), b"").unwrap();
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "img_{A,AB}.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.image_count, 2);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 values")
            .unwrap()
            .to_string(),
        "A,AB"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern channel 1 name")
            .unwrap()
            .to_string(),
        "AB"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_shell_bracket_glob_classes() {
    let dir = tmp_dir("shell_bracket_glob");
    std::fs::create_dir_all(&dir).unwrap();
    for channel in ["A", "B"] {
        for t in 0..2 {
            std::fs::write(dir.join(format!("img_c{channel}_t{t}.fake")), b"").unwrap();
        }
    }
    std::fs::write(dir.join("img_cC_t0.fake"), b"").unwrap();
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "img_c[AB]_t?.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.image_count, 4);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 token")
            .unwrap()
            .to_string(),
        "[AB]"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 values")
            .unwrap()
            .to_string(),
        "A,B"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern channel 1 name")
            .unwrap()
            .to_string(),
        "B"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_negated_shell_bracket_glob_classes() {
    let dir = tmp_dir("negated_shell_bracket_glob");
    std::fs::create_dir_all(&dir).unwrap();
    for channel in ["A", "B", "C"] {
        for t in 0..2 {
            std::fs::write(dir.join(format!("img_c{channel}_t{t}.fake")), b"").unwrap();
        }
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "img_c[!C]_t?.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.image_count, 4);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 token")
            .unwrap()
            .to_string(),
        "[!C]"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 values")
            .unwrap()
            .to_string(),
        "A,B"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern channel 1 name")
            .unwrap()
            .to_string(),
        "B"
    );
    assert_eq!(reader.open_bytes_region(3, 1, 0, 1, 1).unwrap(), vec![1]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_uses_directory_blocks_as_axes() {
    let dir = tmp_dir("dir_axes");
    for channel in ["DAPI", "FITC"] {
        let channel_dir = dir.join(format!("well_{channel}"));
        std::fs::create_dir_all(&channel_dir).unwrap();
        for t in 0..2 {
            std::fs::write(
                channel_dir.join(format!("img_t{t}&sizeX=2&sizeY=1.fake")),
                b"",
            )
            .unwrap();
        }
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "well_<DAPI,FITC>/img_t<0-1>&sizeX=2&sizeY=1.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 4);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern axes")
            .unwrap()
            .to_string(),
        "C,T"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 values")
            .unwrap()
            .to_string(),
        "DAPI,FITC"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern channel 1 name")
            .unwrap()
            .to_string(),
        "FITC"
    );
    assert_eq!(reader.open_bytes_region(3, 1, 0, 1, 1).unwrap(), vec![1]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_recursive_directory_globs() {
    let dir = tmp_dir("recursive_glob");
    for well in ["A", "B"] {
        let image_dir = dir.join(format!("plate/well_{well}/site_0"));
        std::fs::create_dir_all(&image_dir).unwrap();
        for t in 0..2 {
            std::fs::write(
                image_dir.join(format!("img_t{t}&sizeX=2&sizeY=1.fake")),
                b"",
            )
            .unwrap();
        }
    }
    std::fs::write(dir.join("plate").join("notes.txt"), b"not an image").unwrap();
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "plate/**/img_t?&sizeX=2&sizeY=1.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 4);
    assert_eq!(reader.open_bytes_region(3, 1, 0, 1, 1).unwrap(), vec![1]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_recursive_glob_matches_zero_or_more_directories() {
    let dir = tmp_dir("recursive_zero_or_more");
    let plate = dir.join("plate");
    let nested = plate.join("well_A");
    std::fs::create_dir_all(&nested).unwrap();
    for t in 0..2 {
        std::fs::write(plate.join(format!("img_t{t}&sizeX=2&sizeY=1.fake")), b"").unwrap();
        std::fs::write(nested.join(format!("img_t{t}&sizeX=2&sizeY=1.fake")), b"").unwrap();
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "plate/**/img_t?&sizeX=2&sizeY=1.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 4);
    assert_eq!(reader.open_bytes_region(3, 1, 0, 1, 1).unwrap(), vec![1]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_collapses_adjacent_recursive_globs() {
    let dir = tmp_dir("recursive_adjacent_globs");
    let plate = dir.join("plate");
    let nested = plate.join("well_A").join("site_0");
    std::fs::create_dir_all(&nested).unwrap();
    for t in 0..2 {
        std::fs::write(plate.join(format!("img_t{t}&sizeX=2&sizeY=1.fake")), b"").unwrap();
        std::fs::write(nested.join(format!("img_t{t}&sizeX=2&sizeY=1.fake")), b"").unwrap();
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "plate/**/**/img_t?&sizeX=2&sizeY=1.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.image_count, 4);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern file count")
            .unwrap()
            .to_string(),
        "4"
    );
    assert_eq!(reader.open_bytes_region(3, 1, 0, 1, 1).unwrap(), vec![1]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_terminal_recursive_glob_ignores_unreadable_sidecars() {
    let dir = tmp_dir("recursive_terminal_sidecars");
    let plate = dir.join("plate");
    let nested = plate.join("well_A");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(plate.join("img_t0&sizeX=2&sizeY=1.fake"), b"").unwrap();
    std::fs::write(nested.join("img_t1&sizeX=2&sizeY=1.fake"), b"").unwrap();
    std::fs::write(plate.join("notes.sidecar"), b"not an image").unwrap();
    std::fs::write(nested.join("acquisition.unreadable"), b"not an image").unwrap();
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "plate/**").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.image_count, 2);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern file count")
            .unwrap()
            .to_string(),
        "2"
    );
    assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(), vec![1]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_terminal_recursive_glob_reports_unsupported_sidecar_only_tree() {
    let dir = tmp_dir("recursive_terminal_sidecar_only");
    let plate = dir.join("plate");
    let nested = plate.join("well_A");
    std::fs::create_dir_all(&nested).unwrap();
    std::fs::write(plate.join("notes.sidecar"), b"not an image").unwrap();
    std::fs::write(nested.join("acquisition.unreadable"), b"not an image").unwrap();
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "plate/**").unwrap();

    let mut reader = FilePatternReader::new();
    let err = reader.set_id(&pattern).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(message) if message.contains("recursive ** glob matched no supported reader files"))
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_directory_globs() {
    let dir = tmp_dir("dir_glob");
    for well in ["A", "B"] {
        let well_dir = dir.join(format!("well_{well}"));
        std::fs::create_dir_all(&well_dir).unwrap();
        for t in 0..2 {
            std::fs::write(well_dir.join(format!("img_t{t}&sizeX=2&sizeY=1.fake")), b"").unwrap();
        }
    }
    std::fs::create_dir_all(dir.join("notes_A")).unwrap();
    std::fs::write(dir.join("well_A").join("notes.txt"), b"not an image").unwrap();
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "well_?/img_t?&sizeX=2&sizeY=1.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 4);
    assert_eq!(reader.open_bytes_region(3, 1, 0, 1, 1).unwrap(), vec![1]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_expands_directory_globs_mixed_with_pattern_blocks() {
    let dir = tmp_dir("dir_glob_mixed");
    for well in ["A", "B"] {
        let well_dir = dir.join(format!("well_{well}"));
        std::fs::create_dir_all(&well_dir).unwrap();
        for t in 0..2 {
            std::fs::write(well_dir.join(format!("img_t{t}&sizeX=2&sizeY=1.fake")), b"").unwrap();
        }
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "well_?/img_t<0-1>&sizeX=2&sizeY=1.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 4);
    assert_eq!(
        meta.series_metadata
            .get("FilePattern axes")
            .unwrap()
            .to_string(),
        "C,T"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 token")
            .unwrap()
            .to_string(),
        "?"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 0 values")
            .unwrap()
            .to_string(),
        "A,B"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern block 1 token")
            .unwrap()
            .to_string(),
        "*"
    );
    assert_eq!(
        meta.series_metadata
            .get("FilePattern root")
            .unwrap()
            .to_string(),
        dir.display().to_string()
    );
    assert_eq!(reader.open_bytes_region(3, 1, 0, 1, 1).unwrap(), vec![1]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_supports_confined_parent_traversal_after_directory_glob() {
    let dir = tmp_dir("dir_glob_parent");
    for well in ["A", "B"] {
        std::fs::create_dir_all(dir.join(format!("well_{well}"))).unwrap();
    }
    for t in 0..2 {
        std::fs::write(dir.join(format!("img_t{t}&sizeX=2&sizeY=1.fake")), b"").unwrap();
    }
    let pattern = dir.join("stack.pattern");
    std::fs::write(&pattern, "well_?/../img_t<0-1>&sizeX=2&sizeY=1.fake").unwrap();

    let mut reader = FilePatternReader::new();
    reader.set_id(&pattern).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 2);
    assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(), vec![1]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn filepattern_reader_rejects_escaping_parent_traversal_after_directory_glob() {
    let dir = tmp_dir("dir_glob_parent_escape");
    std::fs::create_dir_all(dir.join("well_A")).unwrap();
    let outside = dir.parent().unwrap().join(format!(
        "{}_outside.fake",
        dir.file_name().unwrap().to_string_lossy()
    ));
    std::fs::write(&outside, b"").unwrap();
    let pattern = dir.join("stack.pattern");
    let outside_name = outside.file_name().unwrap().to_string_lossy();
    std::fs::write(&pattern, format!("well_?/../../{outside_name}")).unwrap();

    let mut reader = FilePatternReader::new();
    let err = reader.set_id(&pattern).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(message) if message.contains("escapes the pattern root"))
    );

    let _ = std::fs::remove_file(outside);
    let _ = std::fs::remove_dir_all(dir);
}
