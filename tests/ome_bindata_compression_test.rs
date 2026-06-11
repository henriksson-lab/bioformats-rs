//! End-to-end tests for the OME-XML reader's inline `<BinData>` decoding across
//! the supported `Compression` values.
//!
//! Production path under test (src/formats/ome.rs):
//!   OmeXmlReader::set_id -> parse_ome_xml_series_with_base -> parse_bindata_blocks
//!   -> decompress_bindata   (match on the `Compression` attribute)
//!
//! The codec functions themselves are unit-tested in src/common/codec.rs; this
//! test exercises the OME-XML BinData plumbing end-to-end by writing a real
//! `.ome.xml` file, opening it through the public `ImageReader` API, and
//! asserting the decoded plane bytes match the original uncompressed bytes.
//!
//! Covered compressions: `none`, `zlib`, `bzip2`.
//! Not covered (lossy / hard to synthesise bit-exactly): `J2K`, `JPEG`.

use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use bioformats::ImageReader;

/// Unique temp path so concurrent test runs don't collide.
fn temp_path(name: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!("bioformats_ome_bindata_{nanos}_{name}"))
}

/// Minimal standard-alphabet base64 encoder with padding (mirrors the private
/// `base64_encode` in src/formats/ome.rs, reimplemented here since it is not
/// public). The reader decodes with the matching private `base64_decode`.
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// zlib-compress (zlib header + adler32) to match `decompress_deflate`'s
/// `flate2::read::ZlibDecoder` (src/common/codec.rs:13).
fn zlib_compress(data: &[u8]) -> Vec<u8> {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// bzip2-compress (full standard "BZh" stream) to match `decompress_bzip2`'s
/// `bzip2::read::BzDecoder` (src/common/codec.rs:34).
fn bzip2_compress(data: &[u8]) -> Vec<u8> {
    use bzip2::write::BzEncoder;
    use bzip2::Compression;
    let mut enc = BzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(data).unwrap();
    enc.finish().unwrap()
}

/// Build a minimal valid OME-XML document for a 4x4 uint16 single-plane image
/// with the given `Compression` attribute and base64 payload.
///
/// SizeZ=SizeC=SizeT=1 -> image_count = 1. uint16 -> 2 bytes/sample. Plane is
/// 4*4*2 = 32 bytes. We pack the pixel bytes little-endian and declare
/// BigEndian="false"; the reader returns the decompressed bytes verbatim (it
/// does not byte-swap), so the round-trip is exact.
fn ome_xml(compression: &str, b64: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">
  <Image ID="Image:0" Name="bindata-test">
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint16" SizeX="4" SizeY="4" SizeZ="1" SizeC="1" SizeT="1" BigEndian="false">
      <Channel ID="Channel:0:0" SamplesPerPixel="1"/>
      <BinData Compression="{compression}" BigEndian="false">{b64}</BinData>
    </Pixels>
  </Image>
</OME>"#
    )
}

/// The original uncompressed plane: 16 uint16 pixels, little-endian.
fn original_plane() -> Vec<u8> {
    let mut bytes = Vec::with_capacity(32);
    for px in 0u16..16u16 {
        let value = px.wrapping_mul(4099); // spread across both bytes
        bytes.extend_from_slice(&value.to_le_bytes());
    }
    bytes
}

/// Write `xml` to a unique `.ome.xml` file, open via `ImageReader`, read plane 0,
/// and assert it equals `expected`. The `.ome.xml` extension makes the registry
/// select `OmeXmlReader` (src/registry.rs:71).
fn assert_roundtrip(label: &str, xml: &str, expected: &[u8]) {
    let path = temp_path(&format!("{label}.ome.xml"));
    std::fs::write(&path, xml).unwrap();

    let mut reader = ImageReader::open(&path).expect("ImageReader::open failed");

    let meta = reader.metadata();
    assert_eq!(meta.size_x, 4, "{label}: size_x");
    assert_eq!(meta.size_y, 4, "{label}: size_y");
    assert_eq!(meta.image_count, 1, "{label}: image_count");
    assert!(meta.is_little_endian, "{label}: expected little-endian");

    let got = reader.open_bytes(0).expect("open_bytes(0) failed");
    assert_eq!(got, expected, "{label}: decoded plane bytes mismatch");

    let _ = std::fs::remove_file(&path);
}

#[test]
fn ome_bindata_compression_none() {
    let plane = original_plane();
    let b64 = base64_encode(&plane);
    let xml = ome_xml("none", &b64);
    assert_roundtrip("none", &xml, &plane);
}

#[test]
fn ome_bindata_compression_zlib() {
    let plane = original_plane();
    let b64 = base64_encode(&zlib_compress(&plane));
    let xml = ome_xml("zlib", &b64);
    assert_roundtrip("zlib", &xml, &plane);
}

#[test]
fn ome_bindata_compression_bzip2() {
    let plane = original_plane();
    let b64 = base64_encode(&bzip2_compress(&plane));
    let xml = ome_xml("bzip2", &b64);
    assert_roundtrip("bzip2", &xml, &plane);
}
