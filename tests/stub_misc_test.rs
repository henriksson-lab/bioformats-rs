//! Synthetic-fixture tests for the faithfully ported `misc.rs` readers:
//! MNG, Improvision Openlab LIFF and 3i SlideBook.
//!
//! These build minimal, self-contained binary fixtures (no external crates) and
//! exercise detection + metadata + pixel reads. The fixtures mirror the byte
//! layouts the Rust ports parse, which are translated directly from the Java
//! reference readers.

use bioformats::formats::misc::{MngReader, OpenlabReader, SlidebookReader};
use bioformats::{FormatReader, MetadataValue, OmeAnnotation, PixelType};
use std::io::Write;

// --- tiny PNG / zlib / crc helpers (self-contained) -----------------------

fn crc32(bytes: &[u8]) -> u32 {
    let mut crc: u32 = 0xffff_ffff;
    for &b in bytes {
        crc ^= b as u32;
        for _ in 0..8 {
            if crc & 1 != 0 {
                crc = (crc >> 1) ^ 0xEDB8_8320;
            } else {
                crc >>= 1;
            }
        }
    }
    crc ^ 0xffff_ffff
}

fn adler32(bytes: &[u8]) -> u32 {
    let mut a: u32 = 1;
    let mut b: u32 = 0;
    for &x in bytes {
        a = (a + x as u32) % 65521;
        b = (b + a) % 65521;
    }
    (b << 16) | a
}

/// Build a PNG chunk: len(4 BE) + type + data + crc32(type+data) (4 BE).
fn png_chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let mut crc_input = kind.to_vec();
    crc_input.extend_from_slice(data);
    out.extend_from_slice(&crc32(&crc_input).to_be_bytes());
    out
}

/// Wrap raw bytes in a zlib stream using a single uncompressed (stored) deflate
/// block. Valid for payloads under 65535 bytes.
fn zlib_stored(raw: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // zlib header (deflate, default)
    out.push(0x01); // BFINAL=1, BTYPE=00 (stored)
    let len = raw.len() as u16;
    out.extend_from_slice(&len.to_le_bytes());
    out.extend_from_slice(&(!len).to_le_bytes());
    out.extend_from_slice(raw);
    out.extend_from_slice(&adler32(raw).to_be_bytes());
    out
}

/// Build a complete grayscale-8 PNG with the given pixels (row-major).
fn build_gray_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    assert_eq!(pixels.len(), (width * height) as usize);
    let mut png = vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a];
    // IHDR: width, height, bitdepth=8, colortype=0 (gray), compression, filter, interlace
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 0, 0, 0, 0]);
    png.extend_from_slice(&png_chunk(b"IHDR", &ihdr));
    // IDAT: filtered scanlines (filter byte 0 per row).
    let mut filtered = Vec::new();
    for y in 0..height as usize {
        filtered.push(0u8);
        let start = y * width as usize;
        filtered.extend_from_slice(&pixels[start..start + width as usize]);
    }
    png.extend_from_slice(&png_chunk(b"IDAT", &zlib_stored(&filtered)));
    png.extend_from_slice(&png_chunk(b"IEND", &[]));
    png
}

/// Wrap a PNG datastream as a one-frame MNG file.
fn build_mng(png: &[u8]) -> Vec<u8> {
    let mut mng = vec![0x8a, 0x4d, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]; // MNG signature
                                                                        // MHDR chunk: 28 bytes of (here zeroed) data. The reader skips 12 then reads
                                                                        // "MHDR", then skips 32 (28 data + 4 CRC).
    mng.extend_from_slice(&png_chunk(b"MHDR", &[0u8; 28]));
    // Embed the PNG chunks (everything after the 8-byte PNG signature).
    mng.extend_from_slice(&png[8..]);
    mng
}

fn write_temp(name: &str, bytes: &[u8]) -> std::path::PathBuf {
    let mut path = std::env::temp_dir();
    path.push(format!("bf_stub_misc_{}_{}", std::process::id(), name));
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(bytes).unwrap();
    path
}

// --- MNG -------------------------------------------------------------------

#[test]
fn mng_detects_and_reads_embedded_png_frame() {
    let png = build_gray_png(2, 2, &[10, 20, 30, 40]);
    let mng = build_mng(&png);
    let path = write_temp("frame.mng", &mng);

    let mut reader = MngReader::new();
    // Magic-byte detection.
    assert!(reader.is_this_type_by_bytes(&mng));
    assert!(reader.is_this_type_by_name(std::path::Path::new("x.mng")));

    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 1);

    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.pixel_type, PixelType::Uint8);
    assert_eq!(meta.image_count, 1);
    assert!(!meta.is_little_endian); // Java MNGReader: littleEndian = false

    let plane = reader.open_bytes(0).unwrap();
    assert_eq!(plane, vec![10, 20, 30, 40]);

    // Region crop: bottom-right 1x1 pixel.
    let region = reader.open_bytes_region(0, 1, 1, 1, 1).unwrap();
    assert_eq!(region, vec![40]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn mng_rejects_non_mng_bytes() {
    let reader = MngReader::new();
    assert!(!reader.is_this_type_by_bytes(b"not a mng file at all"));
}

// --- Openlab LIFF (version 2, MAC_16_GREYS -> UINT16) ----------------------

fn build_openlab_v2() -> Vec<u8> {
    let mut data = vec![0u8; 360];
    // Magic: readLong == 0x0000ffff696d7072; bytes 4..8 == "impr".
    data[0..8].copy_from_slice(&[0x00, 0x00, 0xff, 0xff, 0x69, 0x6d, 0x70, 0x72]);
    // version = 2 (i32 BE) at 8..12
    data[8..12].copy_from_slice(&2i32.to_be_bytes());
    // planeCount = 1 (i16 BE) at 12..14
    data[12..14].copy_from_slice(&1i16.to_be_bytes());
    // id seed 14..16 (zero)
    // offset to first plane = 24 (i32 BE) at 16..20
    data[16..20].copy_from_slice(&24i32.to_be_bytes());

    // Tag header at offset 24 (big-endian, version-2 layout).
    data[24..26].copy_from_slice(&67i16.to_be_bytes()); // tag = IMAGE_TYPE_1
    data[26..28].copy_from_slice(&0i16.to_be_bytes()); // sub_tag
    data[28..32].copy_from_slice(&100_000i32.to_be_bytes()); // next_tag (past EOF -> stop)
    data[32..36].copy_from_slice(b"RAW "); // fmt (not "pict")
                                           // 36..40 skipped (4 bytes), then 24 bytes skipped -> pos 64.
                                           // volume_type = 3 (MAC_16_GREYS) at 64..66
    data[64..66].copy_from_slice(&3i16.to_be_bytes());
    // 66..82 skipped (16 bytes). name terminator at 82.
    data[82] = 0x00; // empty name -> new series
                     // skip(256 - 83 + 82) = 255 -> plane_offset = 338.
                     // version-2 dimensions: skip(2) then top,left,bottom,right at 340..348.
    data[338] = 0xAA;
    data[339] = 0xBB;
    // top=0 (340..342), left=0 (342..344)
    data[344..346].copy_from_slice(&2i16.to_be_bytes()); // bottom = 2
    data[346..348].copy_from_slice(&2i16.to_be_bytes()); // right = 2
    data
}

fn write_openlab_v2_plane_tag(
    data: &mut [u8],
    tag_offset: usize,
    next_tag: i32,
    volume_type: i16,
    name: &str,
    width: i16,
    height: i16,
) {
    data[tag_offset..tag_offset + 2].copy_from_slice(&67i16.to_be_bytes());
    data[tag_offset + 2..tag_offset + 4].copy_from_slice(&0i16.to_be_bytes());
    data[tag_offset + 4..tag_offset + 8].copy_from_slice(&next_tag.to_be_bytes());
    data[tag_offset + 8..tag_offset + 12].copy_from_slice(b"RAW ");
    data[tag_offset + 40..tag_offset + 42].copy_from_slice(&volume_type.to_be_bytes());
    let name_offset = tag_offset + 58;
    data[name_offset..name_offset + name.len()].copy_from_slice(name.as_bytes());
    data[name_offset + name.len()] = 0;
    let plane_offset = name_offset + 256;
    data[plane_offset + 2..plane_offset + 4].copy_from_slice(&0i16.to_be_bytes());
    data[plane_offset + 4..plane_offset + 6].copy_from_slice(&0i16.to_be_bytes());
    data[plane_offset + 6..plane_offset + 8].copy_from_slice(&height.to_be_bytes());
    data[plane_offset + 8..plane_offset + 10].copy_from_slice(&width.to_be_bytes());
}

fn build_openlab_v2_named_planes(names: &[&str]) -> Vec<u8> {
    let tag_stride = 360usize;
    let mut data = vec![0u8; 24 + names.len() * tag_stride + 128];
    data[0..8].copy_from_slice(&[0x00, 0x00, 0xff, 0xff, 0x69, 0x6d, 0x70, 0x72]);
    data[8..12].copy_from_slice(&2i32.to_be_bytes());
    data[12..14].copy_from_slice(&(names.len() as i16).to_be_bytes());
    data[16..20].copy_from_slice(&24i32.to_be_bytes());

    for (i, name) in names.iter().enumerate() {
        let tag_offset = 24 + i * tag_stride;
        let next_tag = if i + 1 == names.len() {
            100_000
        } else {
            (tag_offset + tag_stride) as i32
        };
        write_openlab_v2_plane_tag(&mut data, tag_offset, next_tag, 3, name, 1, 1);
    }
    data
}

fn build_openlab_v2_with_calibration(name: &str, xcal: f32, ycal: f32) -> Vec<u8> {
    let calibration_offset = 24usize;
    let image_offset = 384usize;
    let mut data = vec![0u8; image_offset + 360 + 128];
    data[0..8].copy_from_slice(&[0x00, 0x00, 0xff, 0xff, 0x69, 0x6d, 0x70, 0x72]);
    data[8..12].copy_from_slice(&2i32.to_be_bytes());
    data[12..14].copy_from_slice(&1i16.to_be_bytes());
    data[16..20].copy_from_slice(&(calibration_offset as i32).to_be_bytes());

    data[calibration_offset..calibration_offset + 2].copy_from_slice(&69i16.to_be_bytes());
    data[calibration_offset + 2..calibration_offset + 4].copy_from_slice(&0i16.to_be_bytes());
    data[calibration_offset + 4..calibration_offset + 8]
        .copy_from_slice(&(image_offset as i32).to_be_bytes());
    data[calibration_offset + 8..calibration_offset + 12].copy_from_slice(b"CAL ");
    data[calibration_offset + 20..calibration_offset + 22].copy_from_slice(&3i16.to_be_bytes());
    data[calibration_offset + 34..calibration_offset + 38].copy_from_slice(&xcal.to_be_bytes());
    data[calibration_offset + 38..calibration_offset + 42].copy_from_slice(&ycal.to_be_bytes());

    write_openlab_v2_plane_tag(&mut data, image_offset, 100_000, 3, name, 2, 2);
    data
}

fn push_be_u16(out: &mut Vec<u8>, v: u16) {
    out.extend_from_slice(&v.to_be_bytes());
}

fn build_pict_v2_prefix(width: u16, height: u16) -> Vec<u8> {
    let mut out = vec![0; 512];
    push_be_u16(&mut out, 0);
    push_be_u16(&mut out, 0);
    push_be_u16(&mut out, 0);
    push_be_u16(&mut out, height);
    push_be_u16(&mut out, width);
    out.extend_from_slice(&[0x00, 0x11]);
    push_be_u16(&mut out, 0x02ff);
    out.extend_from_slice(&[0; 18]);
    push_be_u16(&mut out, height);
    push_be_u16(&mut out, width);
    out.extend_from_slice(&[0; 4]);
    out
}

fn append_pict_packbits_8(out: &mut Vec<u8>, width: u16, height: u16, rows: &[&[u8]]) {
    if out.len() & 1 != 0 {
        out.push(0);
    }
    push_be_u16(out, 0x0098);
    push_be_u16(out, 0x8000 | width);
    push_be_u16(out, 0);
    push_be_u16(out, 0);
    push_be_u16(out, height);
    push_be_u16(out, width);
    out.extend_from_slice(&[0; 18]);
    push_be_u16(out, 8);
    push_be_u16(out, 1);
    out.extend_from_slice(&[0; 14]);
    out.extend_from_slice(&[0; 4]);
    push_be_u16(out, 0);
    push_be_u16(out, 1);
    for i in 0..2u8 {
        push_be_u16(out, i as u16);
        out.extend_from_slice(&[i, 0, i, 0, i, 0]);
    }
    out.extend_from_slice(&[0; 18]);
    for row in rows {
        out.push(row.len() as u8);
        out.extend_from_slice(row);
    }
    if out.len() & 1 != 0 {
        out.push(0);
    }
    push_be_u16(out, 0x00ff);
}

fn append_pict_vector_pixmap_9a_header(out: &mut Vec<u8>, width: u16, height: u16) {
    if out.len() & 1 != 0 {
        out.push(0);
    }
    push_be_u16(out, 0x009a);
    out.extend_from_slice(&[0; 6]);
    push_be_u16(out, 0);
    push_be_u16(out, 0);
    push_be_u16(out, height);
    push_be_u16(out, width);
    out.extend_from_slice(&[0; 18]);
    push_be_u16(out, 8);
    push_be_u16(out, 1);
    out.extend_from_slice(&[0; 14]);
}

fn build_openlab_v2_pict(width: i16, height: i16, pict: &[u8], name: &str) -> Vec<u8> {
    let tag_offset = 24usize;
    let name_offset = tag_offset + 58;
    let plane_offset = name_offset + 256;
    let payload_offset = plane_offset + 10;
    let mut data = vec![0u8; payload_offset + pict.len()];

    data[0..8].copy_from_slice(&[0x00, 0x00, 0xff, 0xff, 0x69, 0x6d, 0x70, 0x72]);
    data[8..12].copy_from_slice(&2i32.to_be_bytes());
    data[12..14].copy_from_slice(&1i16.to_be_bytes());
    data[16..20].copy_from_slice(&(tag_offset as i32).to_be_bytes());

    data[tag_offset..tag_offset + 2].copy_from_slice(&67i16.to_be_bytes());
    data[tag_offset + 2..tag_offset + 4].copy_from_slice(&0i16.to_be_bytes());
    data[tag_offset + 4..tag_offset + 8].copy_from_slice(&100_000i32.to_be_bytes());
    data[tag_offset + 8..tag_offset + 12].copy_from_slice(b"PICT");
    data[tag_offset + 40..tag_offset + 42].copy_from_slice(&2i16.to_be_bytes());
    data[name_offset..name_offset + name.len()].copy_from_slice(name.as_bytes());
    data[name_offset + name.len()] = 0;
    data[plane_offset + 2..plane_offset + 4].copy_from_slice(&0i16.to_be_bytes());
    data[plane_offset + 4..plane_offset + 6].copy_from_slice(&0i16.to_be_bytes());
    data[plane_offset + 6..plane_offset + 8].copy_from_slice(&height.to_be_bytes());
    data[plane_offset + 8..plane_offset + 10].copy_from_slice(&width.to_be_bytes());
    data[payload_offset..payload_offset + pict.len()].copy_from_slice(pict);
    data
}

#[test]
fn openlab_v2_detects_and_reads_uint16_plane() {
    let data = build_openlab_v2();
    let path = write_temp("plane.liff", &data);

    let mut reader = OpenlabReader::new();
    assert!(reader.is_this_type_by_bytes(&data));
    assert!(reader.is_this_type_by_name(std::path::Path::new("x.liff")));

    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 1);

    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.pixel_type, PixelType::Uint16);
    assert_eq!(meta.image_count, 1);
    assert!(!meta.is_little_endian);

    // Plane = raw bytes at plane_offset (338) for w*h*2 = 8 bytes.
    let plane = reader.open_bytes(0).unwrap();
    assert_eq!(plane, vec![0xAA, 0xBB, 0, 0, 0, 0, 0, 2]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn openlab_v2_delegates_embedded_pict_bitmap_payload() {
    let mut pict = build_pict_v2_prefix(8, 2);
    append_pict_packbits_8(
        &mut pict,
        8,
        2,
        &[
            &[7, 1, 2, 3, 4, 5, 6, 7, 8],
            &[7, 9, 10, 11, 12, 13, 14, 15, 16],
        ],
    );
    let data = build_openlab_v2_pict(8, 2, &pict, "PICT bitmap");
    let path = write_temp("pict_bitmap.liff", &data);

    let mut reader = OpenlabReader::new();
    reader.set_id(&path).unwrap();

    let meta = reader.metadata();
    assert_eq!((meta.size_x, meta.size_y), (8, 2));
    assert_eq!(meta.pixel_type, PixelType::Uint8);
    assert!(meta.is_indexed);
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
    );
    assert_eq!(
        reader.open_bytes_region(0, 2, 0, 3, 2).unwrap(),
        vec![3, 4, 5, 11, 12, 13]
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn openlab_v2_reports_pict_decoder_boundary() {
    let mut pict = build_pict_v2_prefix(2, 1);
    append_pict_vector_pixmap_9a_header(&mut pict, 2, 1);
    let data = build_openlab_v2_pict(2, 1, &pict, "PICT vector");
    let path = write_temp("pict_vector.liff", &data);

    let mut reader = OpenlabReader::new();
    reader.set_id(&path).unwrap();
    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        err.to_string()
            .contains("vector pixmap payloads with 8-bit pixels are unsupported"),
        "unexpected error: {err}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn openlab_v2_infers_zct_and_ome_names_from_plane_names() {
    let data = build_openlab_v2_named_planes(&[
        "Plate42 WellA01 Z1 C1 T1",
        "Plate42 WellA01 Z1 C2 T1",
        "Plate42 WellA01 Z1 C1 T2",
        "Plate42 WellA01 Z1 C2 T2",
    ]);
    let path = write_temp("Plate42.liff", &data);

    let mut reader = OpenlabReader::new();
    reader.set_id(&path).unwrap();

    let meta = reader.metadata();
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 4);
    assert_eq!(
        meta.series_metadata
            .get("openlab.image_name")
            .map(ToString::to_string)
            .as_deref(),
        Some("Plate42 WellA01")
    );
    assert_eq!(
        meta.series_metadata
            .get("openlab.plane.0.name")
            .map(ToString::to_string)
            .as_deref(),
        Some("Plate42 WellA01 Z1 C1 T1")
    );
    assert_eq!(
        meta.series_metadata
            .get("openlab.plane.1.the_c")
            .map(ToString::to_string)
            .as_deref(),
        Some("1")
    );
    assert_eq!(
        meta.series_metadata
            .get("openlab.plane.2.the_t")
            .map(ToString::to_string)
            .as_deref(),
        Some("1")
    );

    let ome = reader.ome_metadata().expect("Openlab OME metadata");
    assert_eq!(ome.images[0].name.as_deref(), Some("Plate42 WellA01"));
    assert_eq!(ome.plates[0].name.as_deref(), Some("Plate42"));
    let zct: Vec<_> = ome.images[0]
        .planes
        .iter()
        .map(|p| (p.the_z, p.the_c, p.the_t))
        .collect();
    assert_eq!(zct, vec![(0, 0, 0), (0, 1, 0), (0, 0, 1), (0, 1, 1)]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn openlab_v2_ome_annotations_include_native_series_metadata() {
    let data = build_openlab_v2_with_calibration("Plate7 FieldA Z1 C1 T1", 0.5, 0.75);
    let path = write_temp("Plate7.liff", &data);

    let mut reader = OpenlabReader::new();
    reader.set_id(&path).unwrap();

    let meta = reader.metadata();
    match meta.series_metadata.get("openlab.version") {
        Some(MetadataValue::Int(2)) => {}
        other => panic!("unexpected openlab.version: {other:?}"),
    }
    match meta.series_metadata.get("openlab.volume_type") {
        Some(MetadataValue::Int(3)) => {}
        other => panic!("unexpected openlab.volume_type: {other:?}"),
    }
    match meta.series_metadata.get("openlab.volume_type_name") {
        Some(MetadataValue::String(value)) if value == "MAC_16_GREYS" => {}
        other => panic!("unexpected openlab.volume_type_name: {other:?}"),
    }
    match meta.series_metadata.get("openlab.pixel_payload") {
        Some(MetadataValue::String(value)) if value == "raw" => {}
        other => panic!("unexpected openlab.pixel_payload: {other:?}"),
    }
    match meta
        .series_metadata
        .get("openlab.ome.stage_detector_projection")
    {
        Some(MetadataValue::String(value)) if value == "not_projected_no_safe_liff_fields" => {}
        other => panic!("unexpected openlab.ome.stage_detector_projection: {other:?}"),
    }
    match meta
        .series_metadata
        .get("openlab.ome.stage_detector_projection.source_fields")
    {
        Some(MetadataValue::String(value))
            if value.contains("plane tag/sub_tag/format/name/offset")
                && value.contains("calibration physical_size_x/y") => {}
        other => {
            panic!("unexpected openlab.ome.stage_detector_projection.source_fields: {other:?}")
        }
    }
    match meta.series_metadata.get("openlab.ome.stage_projection") {
        Some(MetadataValue::String(value))
            if value == "not_projected_no_explicit_stage_coordinates" => {}
        other => panic!("unexpected openlab.ome.stage_projection: {other:?}"),
    }
    match meta
        .series_metadata
        .get("openlab.ome.stage_projection.inspected_fields")
    {
        Some(MetadataValue::String(value))
            if value.contains("plane names may encode image/Z/C/T labels")
                && value.contains("calibration stores physical pixel size only") => {}
        other => panic!("unexpected openlab.ome.stage_projection.inspected_fields: {other:?}"),
    }
    match meta
        .series_metadata
        .get("openlab.ome.stage_projection.reason")
    {
        Some(MetadataValue::String(value))
            if value.contains("no parsed LIFF field contains explicit stage X/Y/Z coordinates")
                && value.contains("plane names are only used for image/Z/C/T indexing")
                && value.contains("calibration values are pixel sizes") => {}
        other => panic!("unexpected openlab.ome.stage_projection.reason: {other:?}"),
    }
    match meta.series_metadata.get("openlab.ome.detector_projection") {
        Some(MetadataValue::String(value))
            if value == "not_projected_no_explicit_detector_fields" => {}
        other => panic!("unexpected openlab.ome.detector_projection: {other:?}"),
    }
    match meta
        .series_metadata
        .get("openlab.ome.detector_projection.inspected_fields")
    {
        Some(MetadataValue::String(value))
            if value.contains("volume_type")
                && value.contains("pixel_payload")
                && value.contains("not detector identity") => {}
        other => panic!("unexpected openlab.ome.detector_projection.inspected_fields: {other:?}"),
    }
    match meta
        .series_metadata
        .get("openlab.ome.detector_projection.reason")
    {
        Some(MetadataValue::String(value))
            if value.contains("no parsed LIFF field contains detector model")
                && value.contains("gain, offset")
                && value.contains("storage descriptors") => {}
        other => panic!("unexpected openlab.ome.detector_projection.reason: {other:?}"),
    }
    match meta.series_metadata.get("openlab.plane.0.tag") {
        Some(MetadataValue::Int(67)) => {}
        other => panic!("unexpected openlab.plane.0.tag: {other:?}"),
    }
    match meta.series_metadata.get("openlab.plane.0.tag_name") {
        Some(MetadataValue::String(value)) if value == "IMAGE_TYPE_1" => {}
        other => panic!("unexpected openlab.plane.0.tag_name: {other:?}"),
    }
    match meta.series_metadata.get("openlab.plane.0.sub_tag") {
        Some(MetadataValue::Int(0)) => {}
        other => panic!("unexpected openlab.plane.0.sub_tag: {other:?}"),
    }
    match meta.series_metadata.get("openlab.plane.0.format") {
        Some(MetadataValue::String(value)) if value == "RAW" => {}
        other => panic!("unexpected openlab.plane.0.format: {other:?}"),
    }
    let physical_size_x = match meta.series_metadata.get("openlab.physical_size_x") {
        Some(MetadataValue::Float(value)) => *value,
        other => panic!("unexpected openlab.physical_size_x: {other:?}"),
    };
    let physical_size_y = match meta.series_metadata.get("openlab.physical_size_y") {
        Some(MetadataValue::Float(value)) => *value,
        other => panic!("unexpected openlab.physical_size_y: {other:?}"),
    };
    assert!((physical_size_x - 0.0005).abs() < 1e-9);
    assert!((physical_size_y - 0.00075).abs() < 1e-9);

    let ome = reader.ome_metadata().expect("Openlab OME metadata");
    assert_eq!(ome.images[0].physical_size_x, Some(physical_size_x));
    assert_eq!(ome.images[0].physical_size_y, Some(physical_size_y));
    let annotation = ome
        .annotations
        .iter()
        .find_map(|annotation| match annotation {
            OmeAnnotation::MapAnnotation {
                id,
                namespace,
                values,
            } if id.as_deref() == Some("Annotation:OriginalMetadata:0")
                && namespace.as_deref()
                    == Some("openmicroscopy.org/bioformats/original-metadata") =>
            {
                Some(values)
            }
            _ => None,
        })
        .expect("Openlab original metadata annotation");
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.image_name" && value == "Plate7 FieldA"));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.version" && value == "2"));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.volume_type" && value == "3"));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.volume_type_name" && value == "MAC_16_GREYS"));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.pixel_payload" && value == "raw"));
    assert!(annotation.iter().any(
        |(key, value)| key == "openlab.ome.stage_detector_projection"
            && value == "not_projected_no_safe_liff_fields"
    ));
    assert!(annotation.iter().any(|(key, value)| key
        == "openlab.ome.stage_detector_projection.source_fields"
        && value.contains("plane tag/sub_tag/format/name/offset")
        && value.contains("calibration physical_size_x/y")));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.ome.stage_projection"
            && value == "not_projected_no_explicit_stage_coordinates"));
    assert!(annotation.iter().any(|(key, value)| key
        == "openlab.ome.stage_projection.inspected_fields"
        && value.contains("plane names may encode image/Z/C/T labels")
        && value.contains("calibration stores physical pixel size only")));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.ome.stage_projection.reason"
            && value.contains("no parsed LIFF field contains explicit stage X/Y/Z coordinates")
            && value.contains("plane names are only used for image/Z/C/T indexing")
            && value.contains("calibration values are pixel sizes")));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.ome.detector_projection"
            && value == "not_projected_no_explicit_detector_fields"));
    assert!(annotation.iter().any(|(key, value)| key
        == "openlab.ome.detector_projection.inspected_fields"
        && value.contains("volume_type")
        && value.contains("pixel_payload")
        && value.contains("not detector identity")));
    assert!(annotation.iter().any(
        |(key, value)| key == "openlab.ome.detector_projection.reason"
            && value.contains("no parsed LIFF field contains detector model")
            && value.contains("gain, offset")
            && value.contains("storage descriptors")
    ));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.plane.0.tag" && value == "67"));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.plane.0.tag_name" && value == "IMAGE_TYPE_1"));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.plane.0.sub_tag" && value == "0"));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.plane.0.format" && value == "RAW"));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.physical_size_x"
            && value == &physical_size_x.to_string()));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "openlab.physical_size_y"
            && value == &physical_size_y.to_string()));

    let _ = std::fs::remove_file(path);
}

#[test]
fn openlab_v2_keeps_stack_when_name_tokens_are_incomplete() {
    let data = build_openlab_v2_named_planes(&["field Z1 C1", "field Z2 C1"]);
    let path = write_temp("field.liff", &data);

    let mut reader = OpenlabReader::new();
    reader.set_id(&path).unwrap();

    let meta = reader.metadata();
    assert_eq!(meta.size_z, 2);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.size_t, 1);
    assert!(!meta
        .series_metadata
        .contains_key("openlab.image_name_zct_inference"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn openlab_rejects_bad_magic() {
    let reader = OpenlabReader::new();
    assert!(!reader.is_this_type_by_bytes(b"\x00\x00\x00\x00impr12"));
}

// --- 3i SlideBook ----------------------------------------------------------

#[test]
fn slidebook_detection_by_magic_shorts() {
    let reader = SlidebookReader::new();
    let mut header = vec![0u8; 16];
    header[4..6].copy_from_slice(b"II"); // little-endian
    header[6..8].copy_from_slice(&0x006cu16.to_le_bytes()); // magic1 = SLD_MAGIC_BYTES_1_0
    header[8..10].copy_from_slice(&0x0100u16.to_le_bytes()); // magic2 high byte = 0x0100
    assert!(reader.is_this_type_by_bytes(&header));

    // Not a SlideBook header.
    assert!(!reader.is_this_type_by_bytes(b"random bytes...."));
}

#[test]
fn slidebook_reports_honest_error_when_no_pixel_blocks() {
    // A detectable but empty file has no pixel blocks; the reader must report an
    // honest error rather than fabricate a series.
    let mut data = vec![0u8; 64];
    data[4..6].copy_from_slice(b"II");
    data[6..8].copy_from_slice(&0x006cu16.to_le_bytes());
    data[8..10].copy_from_slice(&0x0100u16.to_le_bytes());
    let path = write_temp("empty.sld", &data);

    let mut reader = SlidebookReader::new();
    let result = reader.set_id(&path);
    assert!(
        result.is_err(),
        "expected an error for a file with no pixel data"
    );

    let _ = std::fs::remove_file(path);
}
