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

fn tillvision_native_cimage_contents_with_binary_fragment_table(
    fragments: &[(usize, &[u8])],
) -> Vec<u8> {
    let mut contents = tillvision_native_cimage_contents();
    let table_len = 4 + fragments.len() * 8;
    contents.resize(125 + table_len, 0);
    contents[125..129].copy_from_slice(&(fragments.len() as u32).to_le_bytes());
    for (index, &(offset, payload)) in fragments.iter().enumerate() {
        let pair = 129 + index * 8;
        contents[pair..pair + 4].copy_from_slice(&(offset as u32).to_le_bytes());
        contents[pair + 4..pair + 8].copy_from_slice(&(payload.len() as u32).to_le_bytes());
        assert!(offset >= contents.len());
        contents.resize(offset, 0xaa);
        contents.extend_from_slice(payload);
    }
    contents
}

fn tillvision_native_cimage_contents_with_binary_fragment_table_u64(
    fragments: &[(usize, &[u8])],
) -> Vec<u8> {
    let mut contents = tillvision_native_cimage_contents();
    let table_len = 4 + fragments.len() * 16;
    contents.resize(125 + table_len, 0);
    contents[125..129].copy_from_slice(&(fragments.len() as u32).to_le_bytes());
    for (index, &(offset, payload)) in fragments.iter().enumerate() {
        let pair = 129 + index * 16;
        contents[pair..pair + 8].copy_from_slice(&(offset as u64).to_le_bytes());
        contents[pair + 8..pair + 16].copy_from_slice(&(payload.len() as u64).to_le_bytes());
        assert!(offset >= contents.len());
        contents.resize(offset, 0xaa);
        contents.extend_from_slice(payload);
    }
    contents
}

fn tillvision_native_cimage_contents_with_binary_fragment_end_table(
    fragments: &[(usize, &[u8])],
) -> Vec<u8> {
    let mut contents = tillvision_native_cimage_contents();
    let table_len = 4 + fragments.len() * 8;
    contents.resize(125 + table_len, 0);
    contents[125..129].copy_from_slice(&(fragments.len() as u32).to_le_bytes());
    for (index, &(offset, payload)) in fragments.iter().enumerate() {
        let pair = 129 + index * 8;
        let end = offset + payload.len();
        contents[pair..pair + 4].copy_from_slice(&(offset as u32).to_le_bytes());
        contents[pair + 4..pair + 8].copy_from_slice(&(end as u32).to_le_bytes());
        assert!(offset >= contents.len());
        contents.resize(offset, 0xaa);
        contents.extend_from_slice(payload);
    }
    contents
}

fn tillvision_native_cimage_class_name_fixed_offset_contents(payload: &[u8]) -> Vec<u8> {
    let mut contents = vec![0u8; 1351];
    contents[13..15].copy_from_slice(&6u16.to_le_bytes());
    contents[15..21].copy_from_slice(b"CImage");
    contents[27] = 12;
    contents[28..40].copy_from_slice(b"SpecialImage");

    let dims = 1280 + 20;
    contents[dims..dims + 4].copy_from_slice(&2u32.to_le_bytes());
    contents[dims + 4..dims + 8].copy_from_slice(&2u32.to_le_bytes());
    contents[dims + 8..dims + 12].copy_from_slice(&1u32.to_le_bytes());
    contents[dims + 12..dims + 16].copy_from_slice(&2u32.to_le_bytes());
    contents[dims + 16..dims + 20].copy_from_slice(&1u32.to_le_bytes());
    contents[dims + 20..dims + 24].copy_from_slice(&2u32.to_le_bytes());
    contents.extend_from_slice(payload);
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
fn tillvision_vws_reads_class_name_fixed_offset_cimage_layout() {
    let dir = isolated_tmp_dir("class_name_fixed_offset");
    let vws = dir.join("class_name_fixed_offset.vws");
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_class_name_fixed_offset_contents(&[1, 2, 3, 4, 5, 6, 7, 8]),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 2);
    assert!(matches!(
        reader.metadata().series_metadata.get("Info image_name"),
        Some(MetadataValue::String(value)) if value == "SpecialImage"
    ));
    assert!(matches!(
        reader.metadata().series_metadata.get("Info cimage_layout"),
        Some(MetadataValue::String(value)) if value == "class-name-fixed-offset"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reports_truncated_class_name_fixed_offset_cimage_layout() {
    let dir = isolated_tmp_dir("class_name_fixed_offset_truncated");
    let vws = dir.join("class_name_fixed_offset_truncated.vws");
    let mut contents = vec![0u8; 64];
    contents[13..15].copy_from_slice(&6u16.to_le_bytes());
    contents[15..21].copy_from_slice(b"CImage");
    contents[27] = 12;
    contents[28..40].copy_from_slice(b"SpecialImage");
    write_tillvision_vws_with_contents(&vws, &contents);

    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&vws).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message)
            if message.contains("class-name CImage layout dimensions are truncated")),
        "{err:?}"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_binary_fragment_table_without_description() {
    let dir = isolated_tmp_dir("binary_fragment_table");
    let vws = dir.join("binary_fragment_table.vws");
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_binary_fragment_table(&[
            (160, &[1, 2, 3]),
            (180, &[4, 5, 6, 7, 8]),
        ]),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("Info inferred_payload_fragments"),
        Some(MetadataValue::String(value)) if value == "160:3, 180:5"
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_u64_binary_fragment_table_without_description() {
    let dir = isolated_tmp_dir("binary_fragment_table_u64");
    let vws = dir.join("binary_fragment_table_u64.vws");
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_binary_fragment_table_u64(&[
            (176, &[1, 2, 3]),
            (192, &[4, 5, 6, 7, 8]),
        ]),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("Info inferred_payload_fragments"),
        Some(MetadataValue::String(value)) if value == "176:3, 192:5"
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_binary_fragment_end_table_without_description() {
    let dir = isolated_tmp_dir("binary_fragment_end_table");
    let vws = dir.join("binary_fragment_end_table.vws");
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_binary_fragment_end_table(&[
            (160, &[1, 2, 3]),
            (180, &[4, 5, 6, 7, 8]),
        ]),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("Info inferred_payload_fragments"),
        Some(MetadataValue::String(value)) if value == "160:3, 180:5"
    ));

    let _ = std::fs::remove_dir_all(dir);
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
fn tillvision_vws_reads_fragment_table_declared_as_start_and_byte_counts() {
    let dir = isolated_tmp_dir("fragment_start_byte_count_alias");
    let vws = dir.join("fragment_start_byte_count_alias.vws");
    let description = b"Chunk starts: 0xa0, 0xb4\r\nChunk byte counts: 3, 5\r\n";
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
        reader.metadata().series_metadata.get("Info Chunk byte counts"),
        Some(MetadataValue::String(value)) if value == "3, 5"
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_fragment_table_declared_as_offsets_and_ends() {
    let dir = isolated_tmp_dir("fragment_offset_end_alias");
    let vws = dir.join("fragment_offset_end_alias.vws");
    let description = b"Fragment offsets: 160, 180\r\nFragment ends: 163, 185\r\n";
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
        reader.metadata().series_metadata.get("Info Fragment ends"),
        Some(MetadataValue::String(value)) if value == "163, 185"
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_rejects_fragment_end_before_offset() {
    let dir = isolated_tmp_dir("fragment_end_before_offset");
    let vws = dir.join("fragment_end_before_offset.vws");
    let description = b"Fragment offsets: 160, 180\r\nFragment ends: 159, 185\r\n";
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_payload_fragments(
            &[(160, &[1, 2, 3]), (180, &[4, 5, 6, 7, 8])],
            description,
        ),
    );

    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&vws).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message)
            if message.contains("fragment end 159 before offset 160")),
        "{err:?}"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_fragment_table_declared_as_flat_pairs() {
    let dir = isolated_tmp_dir("fragment_flat_pairs");
    let vws = dir.join("fragment_flat_pairs.vws");
    let description = b"Fragment table: [160, 3] [180, 5]\r\n";
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

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_zip_alias_for_zlib_compressed_cimage() {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let dir = isolated_tmp_dir("zip_alias");
    let vws = dir.join("zip_alias.vws");
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    let compressed = encoder.finish().unwrap();
    let description = format!(
        "Compression: zlib-deflate\r\nPayload fragments: 160:{}\r\n",
        compressed.len()
    );
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_payload_fragments(
            &[(160, &compressed)],
            description.as_bytes(),
        ),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_rfc1951_alias_for_raw_deflate_cimage() {
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;

    let dir = isolated_tmp_dir("rfc1951_alias");
    let vws = dir.join("rfc1951_alias.vws");
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    let compressed = encoder.finish().unwrap();
    let description = format!(
        "Compression: raw deflate\r\nPayload fragments: 160:{}\r\n",
        compressed.len()
    );
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_payload_fragments(
            &[(160, &compressed)],
            description.as_bytes(),
        ),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_explicit_native_compression_aliases() {
    use flate2::write::{DeflateEncoder, ZlibEncoder};
    use flate2::Compression;
    use std::io::Write;

    let dir = isolated_tmp_dir("explicit_compression_aliases");

    let zlib = dir.join("rfc1950_alias.vws");
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    let compressed = encoder.finish().unwrap();
    let description = format!(
        "Compression: RFC 1950\r\nPayload fragments: 160:{}\r\n",
        compressed.len()
    );
    write_tillvision_vws_with_contents(
        &zlib,
        &tillvision_native_cimage_contents_with_payload_fragments(
            &[(160, &compressed)],
            description.as_bytes(),
        ),
    );
    let mut reader = ImageReader::open(&zlib).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);

    let raw = dir.join("nowrap_deflate_alias.vws");
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    let compressed = encoder.finish().unwrap();
    let description = format!(
        "Compression: deflate-no-wrap\r\nPayload fragments: 160:{}\r\n",
        compressed.len()
    );
    write_tillvision_vws_with_contents(
        &raw,
        &tillvision_native_cimage_contents_with_payload_fragments(
            &[(160, &compressed)],
            description.as_bytes(),
        ),
    );
    let mut reader = ImageReader::open(&raw).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_gzip_compressed_native_cimage_payload() {
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    let dir = isolated_tmp_dir("gzip_alias");
    let vws = dir.join("gzip_alias.vws");
    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    let compressed = encoder.finish().unwrap();
    let description = format!(
        "Compression: RFC 1952\r\nPayload fragments: 160:{}\r\n",
        compressed.len()
    );
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_payload_fragments(
            &[(160, &compressed)],
            description.as_bytes(),
        ),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
    assert!(matches!(
        reader.metadata().series_metadata.get("Info Compression"),
        Some(MetadataValue::String(value)) if value == "RFC 1952"
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_rejects_native_compressed_flag_without_algorithm() {
    let dir = isolated_tmp_dir("compressed_flag_without_algorithm");
    let vws = dir.join("compressed_flag_without_algorithm.vws");
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_payload_fragments(
            &[(125, &[1, 2, 3, 4, 5, 6, 7, 8])],
            b"Compressed: yes\r\n",
        ),
    );

    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&vws).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message)
            if message.contains("compressed payload without a supported algorithm Compressed: yes")),
        "{err:?}"
    );

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
