use bioformats::{BioFormatsError, FormatReader, ImageReader, MetadataValue, PixelType};
use std::path::{Path, PathBuf};

fn isolated_tmp_dir(name: &str) -> PathBuf {
    let mut path = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    path.push(format!("bioformats_tillvision_{name}_{nanos}"));
    std::fs::create_dir_all(&path).unwrap();
    path
}

fn tillvision_native_cimage_contents() -> Vec<u8> {
    let mut contents = vec![0u8; 125];
    contents[0..4].copy_from_slice(b"\xf0\x3f\xff\x00");
    contents[12..16].copy_from_slice(b"\x00\x00\xff\x00");
    contents[22..26].copy_from_slice(b"\x08\x00\x04\x00");
    contents[26] = 11;
    contents[27..38].copy_from_slice(b"NativeImage");
    contents[48..50].copy_from_slice(b"sB");
    let dims = 70;
    contents[dims..dims + 4].copy_from_slice(&2u32.to_le_bytes());
    contents[dims + 4..dims + 8].copy_from_slice(&2u32.to_le_bytes());
    contents[dims + 8..dims + 12].copy_from_slice(&1u32.to_le_bytes());
    contents[dims + 12..dims + 16].copy_from_slice(&2u32.to_le_bytes());
    contents[dims + 16..dims + 20].copy_from_slice(&1u32.to_le_bytes());
    contents[dims + 20..dims + 24].copy_from_slice(&2u32.to_le_bytes());
    contents
}

fn tillvision_native_cimage_contents_with_implicit_fragments(
    fragments: &[(usize, &[u8])],
) -> Vec<u8> {
    let mut contents = tillvision_native_cimage_contents();
    contents.resize(133, 0xaa);
    for &(offset, payload) in fragments {
        assert!(offset >= contents.len());
        contents.resize(offset, 0xaa);
        contents.extend_from_slice(payload);
    }
    contents
}

fn tillvision_native_cimage_contents_with_payload_fragments(
    fragments: &[(usize, &[u8])],
    description: &[u8],
) -> Vec<u8> {
    let mut contents = tillvision_native_cimage_contents();
    for &(offset, payload) in fragments {
        assert!(offset >= contents.len());
        contents.resize(offset, 0xaa);
        contents.extend_from_slice(payload);
    }
    contents.extend_from_slice(b"\0\0\0\0\0\xff");
    contents.extend_from_slice(&(description.len() as u16).to_le_bytes());
    contents.extend_from_slice(description);
    contents
}

fn write_tillvision_vws_with_contents(path: &Path, contents: &[u8]) {
    use std::io::Write;

    let mut comp = cfb::create(path).unwrap();
    comp.create_stream("/Contents")
        .unwrap()
        .write_all(contents)
        .unwrap();
}

#[test]
fn tillvision_vws_reads_fragment_table_declared_with_size_alias() {
    let dir = isolated_tmp_dir("fragment_size_alias");
    let vws = dir.join("fragment_size_alias.vws");
    let description = b"Fragment offsets: 160, 180\r\nFragment sizes: 3, 5\r\n";
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_payload_fragments(
            &[(160, &[1, 2, 3]), (180, &[4, 5, 6, 7, 8])],
            description,
        ),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
    assert!(matches!(
        reader.metadata().series_metadata.get("Info Fragment sizes"),
        Some(MetadataValue::String(value)) if value == "3, 5"
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_infers_fragmented_native_cimage_payload_without_description() {
    let dir = isolated_tmp_dir("implicit_fragments");
    let vws = dir.join("implicit_fragments.vws");
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_implicit_fragments(&[
            (160, &[1, 2, 3]),
            (180, &[4, 5, 6, 7, 8]),
        ]),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("Info inferred_payload_fragments"),
        Some(MetadataValue::String(value)) if value == "160:3, 180:5"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_rejects_ambiguous_implicit_fragment_payload_without_description() {
    let dir = isolated_tmp_dir("ambiguous_implicit_fragments");
    let vws = dir.join("ambiguous_implicit_fragments.vws");
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_implicit_fragments(&[
            (160, &[1, 2]),
            (180, &[3, 4, 5, 6, 7]),
        ]),
    );

    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&vws).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message)
            if message.contains("without description metadata")
                && message.contains("assemble to 7 bytes, expected 8")),
        "{err:?}"
    );

    let _ = std::fs::remove_dir_all(dir);
}
