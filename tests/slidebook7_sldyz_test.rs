use std::io::Write;
use std::path::{Path, PathBuf};

use bioformats::common::pixel_type::PixelType;
use bioformats::formats::flim2::SlideBook7Reader;
use bioformats::FormatReader;

fn temp_path(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bioformats_slidebook7_sldyz_{}_{}_{}",
        std::process::id(),
        nanos,
        name
    ))
}

fn build_npy(descr: &str, shape: &[u32], payload: &[u8]) -> Vec<u8> {
    let shape_text = if shape.len() == 1 {
        format!("({},)", shape[0])
    } else {
        shape
            .iter()
            .map(u32::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut header =
        format!("{{'descr': '{descr}', 'fortran_order': False, 'shape': ({shape_text}), }}");
    let preamble_len = 10usize;
    let padding = (16 - ((preamble_len + header.len() + 1) % 16)) % 16;
    header.extend(std::iter::repeat_n(' ', padding));
    header.push('\n');

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"\x93NUMPY");
    bytes.push(1);
    bytes.push(0);
    bytes.extend_from_slice(&(header.len() as u16).to_le_bytes());
    bytes.extend_from_slice(header.as_bytes());
    bytes.extend_from_slice(payload);
    bytes
}

fn write_entry<W: Write + std::io::Seek>(zip: &mut zip::ZipWriter<W>, name: &str, bytes: &[u8]) {
    zip.start_file(name, zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(bytes).unwrap();
}

fn write_sldyz(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    write_entry(
        &mut zip,
        "native.dir/Capture.imgdir/ImageRecord.yaml",
        b"mWidth: 2\nmHeight: 2\nmNumPlanes: 1\nmNumChannels: 1\nmNumTimepoints: 2\n",
    );
    let payload = [10u16, 11, 12, 13, 20, 21, 22, 23]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    write_entry(
        &mut zip,
        "native.dir/Capture.imgdir/ImageData_Ch0_TP0000000.npy",
        &build_npy("<u2", &[2, 2, 2], &payload),
    );
    zip.finish().unwrap();
}

fn write_nested_sldyz(path: &Path) {
    let file = std::fs::File::create(path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    write_entry(
        &mut zip,
        "export/session/native.dir/Capture.imgdir/ImageRecord.yaml",
        b"mWidth: 2\nmHeight: 1\nmNumPlanes: 1\nmNumChannels: 1\nmNumTimepoints: 1\n",
    );
    let payload = [41u16, 42]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();
    write_entry(
        &mut zip,
        "export/session/native.dir/Capture.imgdir/ImageData_Ch0_TP0000000.npy",
        &build_npy("<u2", &[1, 2], &payload),
    );
    zip.finish().unwrap();
}

#[test]
fn slidebook7_reads_sldyz_archive_with_supported_native_payloads() {
    let path = temp_path("archive.sldyz");
    write_sldyz(&path);

    let mut reader = SlideBook7Reader::new();
    assert!(reader.is_this_type_by_name(&path));
    reader.set_id(&path).expect("sldyz archive should open");

    let meta = reader.metadata().clone();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 2);
    assert_eq!(meta.pixel_type, PixelType::Uint16);
    assert!(meta.is_little_endian);
    assert_eq!(
        reader.open_bytes(1).unwrap(),
        [20u16, 21, 22, 23]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    );
    assert_eq!(
        reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(),
        [21u16, 23]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    );

    reader.close().unwrap();
    let _ = std::fs::remove_file(path);
}

#[test]
fn slidebook7_reads_sldyz_archive_with_nested_native_root() {
    let path = temp_path("nested-archive.sldyz");
    write_nested_sldyz(&path);

    let mut reader = SlideBook7Reader::new();
    reader
        .set_id(&path)
        .expect("nested sldyz archive should open");

    let meta = reader.metadata().clone();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.image_count, 1);
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        [41u16, 42]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect::<Vec<_>>()
    );

    reader.close().unwrap();
    let _ = std::fs::remove_file(path);
}
