//! Synthetic-fixture tests for the faithfully ported `misc.rs` readers:
//! MNG, Improvision Openlab LIFF and 3i SlideBook.
//!
//! These build minimal, self-contained binary fixtures (no external crates) and
//! exercise detection + metadata + pixel reads. The fixtures mirror the byte
//! layouts the Rust ports parse, which are translated directly from the Java
//! reference readers.

use bioformats::formats::misc::{MngReader, OpenlabLiffReader, SlideBookReader};
use bioformats::{FormatReader, PixelType};
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

#[test]
fn openlab_v2_detects_and_reads_uint16_plane() {
    let data = build_openlab_v2();
    let path = write_temp("plane.liff", &data);

    let mut reader = OpenlabLiffReader::new();
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
fn openlab_rejects_bad_magic() {
    let reader = OpenlabLiffReader::new();
    assert!(!reader.is_this_type_by_bytes(b"\x00\x00\x00\x00impr12"));
}

// --- 3i SlideBook ----------------------------------------------------------

#[test]
fn slidebook_detection_by_magic_shorts() {
    let reader = SlideBookReader::new();
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

    let mut reader = SlideBookReader::new();
    let result = reader.set_id(&path);
    assert!(result.is_err(), "expected an error for a file with no pixel data");

    let _ = std::fs::remove_file(path);
}
