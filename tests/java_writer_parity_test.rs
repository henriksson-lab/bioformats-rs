//! Writer↔Java parity harness.
//!
//! This is the mirror image of `tests/java_parity_test.rs` (which checks our
//! *readers* against Java). Here we exercise our *writers*: we synthesise a
//! small image in memory with a known shape and a deterministic pixel pattern,
//! write it to a temp file with one of our writers, then run the Java
//! Bio-Formats reference oracle (`parity/BfParityOracle.java` against
//! `bioformats_package.jar`) to read the file back and confirm:
//!
//!   1. CORE metadata  — sizeX/Y, pixelType and imageCount always; plus
//!                       sizeZ/C/T and dimensionOrder for formats that carry
//!                       explicit dimension metadata (OME-TIFF / OME-XML).
//!   2. PIXELS         — CRC32 of the bounded top-left region of each plane.
//!                       For lossless formats we recompute the CRC of the same
//!                       region from our known source pattern *in Java's
//!                       reported layout* (interleave + endianness) and require
//!                       a bitwise match. For JPEG (lossy) we compare Java's raw
//!                       bytes against our source within a documented tolerance.
//!
//! Proving Java reads OUR files with the SAME metadata + pixels demonstrates the
//! writers emit correct, interoperable files. A genuine divergence is a real
//! writer bug and fails the test (rather than being papered over) — unless the
//! case is annotated with `known_bug`, in which case the divergence is reported
//! loudly in the summary but kept non-fatal so the regression stays documented
//! and visible until the writer is fixed (see the ICS case).
//!
//! Gating (so plain `cargo test` is unaffected on machines without a JVM):
//!   - Skips if `bioformats_package.jar`, `java`, or `javac` are absent.
//!   - Honours `BIOFORMATS_RS_JAVA_PARITY=0` as an explicit opt-out.
//!
//! Run:  cargo test --test java_writer_parity_test -- --nocapture
//!
//! (Self-contained: parses the oracle's JSON with a tiny embedded parser so the
//! test needs no extra crate dependencies.)

use bioformats::common::metadata::DimensionOrder;
use bioformats::common::pixel_type::PixelType;
use bioformats::{ImageMetadata, ImageWriter, OmeMetadata};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::OnceLock;

const MAX_PLANES: u32 = 8;
const REGION: u32 = 256;

// Synthetic image dimensions (kept small; both below REGION so the oracle's
// bounded region covers the whole plane).
const W: u32 = 64;
const H: u32 = 48;

/// JPEG (lossy) tolerance: mean absolute per-sample error over the plane. The
/// JPEG path goes RGB→YCbCr→DCT→quantise and back; with our smooth gradient
/// source this stays small. Reported alongside the observed max diff.
const JPEG_MEAN_TOL: f64 = 8.0;

// ===========================================================================
// Minimal JSON value + parser (objects, arrays, strings, numbers, bool, null).
// Sufficient for the single-line output of BfParityOracle.
// ===========================================================================

#[derive(Debug, Clone)]
enum Json {
    Null,
    Bool(bool),
    Num(f64),
    Str(String),
    Arr(Vec<Json>),
    Obj(Vec<(String, Json)>),
}

static JSON_NULL: Json = Json::Null;

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Obj(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }
    fn as_u64(&self) -> Option<u64> {
        match self {
            Json::Num(n) => Some(n.round() as u64),
            _ => None,
        }
    }
    fn as_bool(&self) -> Option<bool> {
        match self {
            Json::Bool(b) => Some(*b),
            _ => None,
        }
    }
    fn as_str(&self) -> Option<&str> {
        match self {
            Json::Str(s) => Some(s),
            _ => None,
        }
    }
    fn as_array(&self) -> Option<&Vec<Json>> {
        match self {
            Json::Arr(a) => Some(a),
            _ => None,
        }
    }
    /// Convenience numeric accessor that returns u64::MAX when absent/non-numeric.
    fn u(&self, key: &str) -> u64 {
        self.get(key).and_then(Json::as_u64).unwrap_or(u64::MAX)
    }
}

impl std::ops::Index<&str> for Json {
    type Output = Json;
    fn index(&self, key: &str) -> &Json {
        self.get(key).unwrap_or(&JSON_NULL)
    }
}

struct JsonParser<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> JsonParser<'a> {
    fn new(s: &'a str) -> Self {
        JsonParser {
            b: s.as_bytes(),
            i: 0,
        }
    }
    fn ws(&mut self) {
        while self.i < self.b.len() && self.b[self.i].is_ascii_whitespace() {
            self.i += 1;
        }
    }
    fn parse(&mut self) -> Option<Json> {
        self.ws();
        match self.b.get(self.i)? {
            b'{' => self.obj(),
            b'[' => self.arr(),
            b'"' => self.string().map(Json::Str),
            b't' | b'f' => self.boolean(),
            b'n' => self.null(),
            _ => self.number(),
        }
    }
    fn obj(&mut self) -> Option<Json> {
        self.i += 1; // {
        let mut out = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b'}') {
            self.i += 1;
            return Some(Json::Obj(out));
        }
        loop {
            self.ws();
            let key = self.string()?;
            self.ws();
            if self.b.get(self.i) != Some(&b':') {
                return None;
            }
            self.i += 1;
            let val = self.parse()?;
            out.push((key, val));
            self.ws();
            match self.b.get(self.i) {
                Some(&b',') => self.i += 1,
                Some(&b'}') => {
                    self.i += 1;
                    return Some(Json::Obj(out));
                }
                _ => return None,
            }
        }
    }
    fn arr(&mut self) -> Option<Json> {
        self.i += 1; // [
        let mut out = Vec::new();
        self.ws();
        if self.b.get(self.i) == Some(&b']') {
            self.i += 1;
            return Some(Json::Arr(out));
        }
        loop {
            let val = self.parse()?;
            out.push(val);
            self.ws();
            match self.b.get(self.i) {
                Some(&b',') => self.i += 1,
                Some(&b']') => {
                    self.i += 1;
                    return Some(Json::Arr(out));
                }
                _ => return None,
            }
        }
    }
    fn string(&mut self) -> Option<String> {
        if self.b.get(self.i) != Some(&b'"') {
            return None;
        }
        self.i += 1;
        let mut s = String::new();
        while let Some(&c) = self.b.get(self.i) {
            self.i += 1;
            match c {
                b'"' => return Some(s),
                b'\\' => {
                    let e = *self.b.get(self.i)?;
                    self.i += 1;
                    match e {
                        b'"' => s.push('"'),
                        b'\\' => s.push('\\'),
                        b'/' => s.push('/'),
                        b'n' => s.push('\n'),
                        b'r' => s.push('\r'),
                        b't' => s.push('\t'),
                        b'b' => s.push('\u{0008}'),
                        b'f' => s.push('\u{000C}'),
                        b'u' => {
                            let hex = std::str::from_utf8(self.b.get(self.i..self.i + 4)?).ok()?;
                            let cp = u32::from_str_radix(hex, 16).ok()?;
                            self.i += 4;
                            s.push(char::from_u32(cp).unwrap_or('\u{FFFD}'));
                        }
                        _ => return None,
                    }
                }
                _ => s.push(c as char),
            }
        }
        None
    }
    fn boolean(&mut self) -> Option<Json> {
        if self.b[self.i..].starts_with(b"true") {
            self.i += 4;
            Some(Json::Bool(true))
        } else if self.b[self.i..].starts_with(b"false") {
            self.i += 5;
            Some(Json::Bool(false))
        } else {
            None
        }
    }
    fn null(&mut self) -> Option<Json> {
        if self.b[self.i..].starts_with(b"null") {
            self.i += 4;
            Some(Json::Null)
        } else {
            None
        }
    }
    fn number(&mut self) -> Option<Json> {
        let start = self.i;
        while let Some(&c) = self.b.get(self.i) {
            if c == b'-' || c == b'+' || c == b'.' || c == b'e' || c == b'E' || c.is_ascii_digit() {
                self.i += 1;
            } else {
                break;
            }
        }
        let s = std::str::from_utf8(&self.b[start..self.i]).ok()?;
        s.parse::<f64>().ok().map(Json::Num)
    }
}

fn parse_json(s: &str) -> Option<Json> {
    JsonParser::new(s).parse()
}

// ===========================================================================
// Oracle harness
// ===========================================================================

fn jar_path() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("bioformats_package.jar")
}

fn temp_path(name: &str) -> PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    std::env::temp_dir().join(format!(
        "bf_writer_parity_{}_{}_{}",
        std::process::id(),
        nanos,
        name
    ))
}

fn crc32_ieee(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for &b in bytes {
        crc ^= u32::from(b);
        for _ in 0..8 {
            let mask = 0u32.wrapping_sub(crc & 1);
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// Minimal standard-alphabet base64 decoder.
fn b64_decode(s: &str) -> Vec<u8> {
    fn val(c: u8) -> Option<u8> {
        match c {
            b'A'..=b'Z' => Some(c - b'A'),
            b'a'..=b'z' => Some(c - b'a' + 26),
            b'0'..=b'9' => Some(c - b'0' + 52),
            b'+' => Some(62),
            b'/' => Some(63),
            _ => None,
        }
    }
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    let mut acc = 0u32;
    let mut bits = 0u32;
    for &c in s.as_bytes() {
        let Some(v) = val(c) else { continue };
        acc = (acc << 6) | u32::from(v);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    out
}

fn pixel_type_to_java(pt: PixelType) -> &'static str {
    match pt {
        PixelType::Int8 => "int8",
        PixelType::Uint8 => "uint8",
        PixelType::Int16 => "int16",
        PixelType::Uint16 => "uint16",
        PixelType::Int32 => "int32",
        PixelType::Uint32 => "uint32",
        PixelType::Float32 => "float",
        PixelType::Float64 => "double",
        PixelType::Bit => "bit",
    }
}

fn dim_order_str(d: DimensionOrder) -> &'static str {
    match d {
        DimensionOrder::XYCTZ => "XYCTZ",
        DimensionOrder::XYCZT => "XYCZT",
        DimensionOrder::XYTCZ => "XYTCZ",
        DimensionOrder::XYTZC => "XYTZC",
        DimensionOrder::XYZCT => "XYZCT",
        DimensionOrder::XYZTC => "XYZTC",
    }
}

/// Compile the Java oracle once; return its classpath, or None if unavailable.
fn oracle_classpath() -> Option<&'static str> {
    static CP: OnceLock<Option<String>> = OnceLock::new();
    CP.get_or_init(|| {
        let jar = jar_path();
        if !jar.exists() {
            eprintln!("SKIP writer-parity: {} not found", jar.display());
            return None;
        }
        if Command::new("java").arg("-version").output().is_err()
            || Command::new("javac").arg("-version").output().is_err()
        {
            eprintln!("SKIP writer-parity: java/javac not available");
            return None;
        }
        let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("parity/BfParityOracle.java");
        let out = Path::new(env!("CARGO_MANIFEST_DIR")).join("parity/target");
        std::fs::create_dir_all(&out).ok()?;
        let status = Command::new("javac")
            .arg("-cp")
            .arg(&jar)
            .arg(&src)
            .arg("-d")
            .arg(&out)
            .output()
            .ok()?;
        if !status.status.success() {
            eprintln!(
                "SKIP writer-parity: oracle compile failed:\n{}",
                String::from_utf8_lossy(&status.stderr)
            );
            return None;
        }
        Some(format!("{}:{}", jar.display(), out.display()))
    })
    .as_deref()
}

fn run_oracle(cp: &str, path: &Path) -> Option<Json> {
    let out = Command::new("java")
        .arg("-cp")
        .arg(cp)
        .arg("BfParityOracle")
        .arg(path)
        .arg(MAX_PLANES.to_string())
        .arg(REGION.to_string())
        .output()
        .ok()?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let line = stdout.lines().find(|l| l.trim_start().starts_with('{'))?;
    parse_json(line)
}

// ---------------------------------------------------------------------------
// Deterministic synthetic pixel model.
//
// One scalar VALUE per (x, y, plane, sample). Lossless writers must preserve it
// exactly. The same model is used (a) to build the bytes we feed the writer in
// OUR layout, and (b) to rebuild the expected bytes in Java's reported layout,
// so a layout/endianness difference is never mistaken for a pixel bug.
// ---------------------------------------------------------------------------

/// The pixel value model. With `smooth=false` it is a high-entropy ramp that
/// wraps the type range (good stress for lossless writers). With `smooth=true`
/// it is a low-frequency gradient with no wrap-around, used for the lossy JPEG
/// case so quantisation error stays small and meaningful.
fn model_value(smooth: bool, x: u32, y: u32, plane: u32, s: u32) -> i64 {
    if smooth {
        let base = (x * 200) / W.max(1) + (y * 40) / H.max(1) + s * 12 + plane * 7;
        base.min(255) as i64
    } else {
        (x as i64) * 7 + (y as i64) * 13 + (plane as i64) * 101 + (s as i64) * 53
    }
}

fn encode_sample(pt: PixelType, val: i64, little_endian: bool) -> Vec<u8> {
    match pt {
        PixelType::Uint8 | PixelType::Int8 => vec![(val & 0xff) as u8],
        PixelType::Uint16 | PixelType::Int16 => {
            let v = (val & 0xffff) as u16;
            if little_endian {
                v.to_le_bytes().to_vec()
            } else {
                v.to_be_bytes().to_vec()
            }
        }
        other => panic!("test pattern does not support pixel type {other:?}"),
    }
}

/// Build the bytes for one plane in OUR writer's expected input layout
/// (grayscale row-major, or interleaved RGB when `meta.is_rgb`).
fn build_input_plane(meta: &ImageMetadata, plane: u32, smooth: bool) -> Vec<u8> {
    let le = meta.is_little_endian;
    let samples = if meta.is_rgb { meta.size_c } else { 1 };
    let mut buf = Vec::new();
    if meta.is_rgb {
        // We always set is_interleaved = true for RGB writers under test.
        for y in 0..meta.size_y {
            for x in 0..meta.size_x {
                for s in 0..samples {
                    buf.extend_from_slice(&encode_sample(
                        meta.pixel_type,
                        model_value(smooth, x, y, plane, s),
                        le,
                    ));
                }
            }
        }
    } else {
        for y in 0..meta.size_y {
            for x in 0..meta.size_x {
                buf.extend_from_slice(&encode_sample(
                    meta.pixel_type,
                    model_value(smooth, x, y, plane, 0),
                    le,
                ));
            }
        }
    }
    buf
}

/// Rebuild the expected region bytes for `plane` exactly as Java's
/// `openBytes(plane, 0, 0, w, h)` would return them, given the layout Java
/// reports (rgbChannelCount, interleaved, littleEndian).
fn build_expected_region(js: &Json, pt: PixelType, plane: u32, smooth: bool) -> Vec<u8> {
    let size_x = js["sizeX"].as_u64().unwrap_or(0) as u32;
    let size_y = js["sizeY"].as_u64().unwrap_or(0) as u32;
    let w = size_x.min(REGION);
    let h = size_y.min(REGION);
    let channels = js["rgbChannelCount"].as_u64().unwrap_or(1) as u32;
    let interleaved = js["interleaved"].as_bool().unwrap_or(false);
    let le = js["littleEndian"].as_bool().unwrap_or(true);

    let mut buf = Vec::new();
    if interleaved {
        for y in 0..h {
            for x in 0..w {
                for s in 0..channels {
                    buf.extend_from_slice(&encode_sample(
                        pt,
                        model_value(smooth, x, y, plane, s),
                        le,
                    ));
                }
            }
        }
    } else {
        for s in 0..channels {
            for y in 0..h {
                for x in 0..w {
                    buf.extend_from_slice(&encode_sample(
                        pt,
                        model_value(smooth, x, y, plane, s),
                        le,
                    ));
                }
            }
        }
    }
    buf
}

/// A single writer-under-test case.
struct Case {
    label: &'static str,
    file: &'static str,
    meta: ImageMetadata,
    /// How to write the planes to disk.
    write: WriteKind,
    /// Lossless => require bitwise pixel match. Lossy => tolerant. Lossy cases
    /// use the smooth gradient pattern so quantisation error stays small.
    lossy: bool,
    /// Also hard-assert sizeZ/C/T + dimensionOrder (formats with explicit dims).
    strict_dims: bool,
    /// If set, this case exercises a KNOWN, documented writer bug: any
    /// divergence is reported loudly (not papered over) but does not fail the
    /// suite, so the regression stays visible without blocking CI. Clear this
    /// once the underlying writer is fixed.
    known_bug: Option<&'static str>,
}

impl Case {
    fn smooth(&self) -> bool {
        self.lossy
    }
}

enum WriteKind {
    /// Generic extension-routed writer.
    Image,
    /// OME-TIFF with embedded OME-XML.
    OmeTiff,
}

fn base_meta(pt: PixelType) -> ImageMetadata {
    let mut m = ImageMetadata::default();
    m.size_x = W;
    m.size_y = H;
    m.pixel_type = pt;
    m.bits_per_pixel = (pt.bytes_per_sample() * 8) as u8;
    m.size_z = 1;
    m.size_c = 1;
    m.size_t = 1;
    m.image_count = 1;
    m
}

fn gray_single(pt: PixelType) -> ImageMetadata {
    base_meta(pt)
}

fn gray_zstack(pt: PixelType, z: u32) -> ImageMetadata {
    let mut m = base_meta(pt);
    m.size_z = z;
    m.image_count = z;
    m
}

fn cstack(pt: PixelType, c: u32) -> ImageMetadata {
    let mut m = base_meta(pt);
    m.size_c = c;
    m.image_count = c;
    m
}

fn rgb8() -> ImageMetadata {
    let mut m = base_meta(PixelType::Uint8);
    m.size_c = 3;
    m.is_rgb = true;
    m.is_interleaved = true;
    m.image_count = 1;
    m
}

fn cases() -> Vec<Case> {
    vec![
        // ---- plain TIFF (single plane; TIFF carries no Z/C/T axis tags) ----
        Case {
            label: "TIFF uint8 gray",
            file: "tiff_u8.tif",
            meta: gray_single(PixelType::Uint8),
            write: WriteKind::Image,
            lossy: false,
            strict_dims: false,
            known_bug: None,
        },
        Case {
            label: "TIFF uint16 gray",
            file: "tiff_u16.tif",
            meta: gray_single(PixelType::Uint16),
            write: WriteKind::Image,
            lossy: false,
            strict_dims: false,
            known_bug: None,
        },
        Case {
            label: "TIFF rgb8",
            file: "tiff_rgb8.tif",
            meta: rgb8(),
            write: WriteKind::Image,
            lossy: false,
            strict_dims: false,
            known_bug: None,
        },
        // ---- OME-TIFF (explicit dimension metadata) ----
        Case {
            label: "OME-TIFF uint8 Z=3",
            file: "ome_u8_z3.ome.tif",
            meta: gray_zstack(PixelType::Uint8, 3),
            write: WriteKind::OmeTiff,
            lossy: false,
            strict_dims: true,
            known_bug: None,
        },
        Case {
            label: "OME-TIFF uint16 C=2",
            file: "ome_u16_c2.ome.tif",
            meta: cstack(PixelType::Uint16, 2),
            write: WriteKind::OmeTiff,
            lossy: false,
            strict_dims: true,
            known_bug: None,
        },
        // ---- OME-XML standalone (inline BinData; explicit dims) ----
        Case {
            label: "OME-XML uint8 Z=2",
            file: "ome_u8_z2.ome",
            meta: gray_zstack(PixelType::Uint8, 2),
            write: WriteKind::Image,
            lossy: false,
            strict_dims: true,
            known_bug: None,
        },
        // ---- DICOM (multiframe; axis assignment is format-dependent) ----
        Case {
            label: "DICOM uint8 single",
            file: "dicom_u8.dcm",
            meta: gray_single(PixelType::Uint8),
            write: WriteKind::Image,
            lossy: false,
            strict_dims: false,
            known_bug: None,
        },
        Case {
            label: "DICOM uint16 single",
            file: "dicom_u16.dcm",
            meta: gray_single(PixelType::Uint16),
            write: WriteKind::Image,
            lossy: false,
            strict_dims: false,
            known_bug: None,
        },
        Case {
            label: "DICOM uint8 Z=3",
            file: "dicom_u8_z3.dcm",
            meta: gray_zstack(PixelType::Uint8, 3),
            write: WriteKind::Image,
            lossy: false,
            strict_dims: false,
            known_bug: None,
        },
        // ---- PNG ----
        Case {
            label: "PNG uint8 gray",
            file: "png_u8.png",
            meta: gray_single(PixelType::Uint8),
            write: WriteKind::Image,
            lossy: false,
            strict_dims: false,
            known_bug: None,
        },
        Case {
            label: "PNG uint16 gray",
            file: "png_u16.png",
            meta: gray_single(PixelType::Uint16),
            write: WriteKind::Image,
            lossy: false,
            strict_dims: false,
            known_bug: None,
        },
        Case {
            label: "PNG rgb8",
            file: "png_rgb8.png",
            meta: rgb8(),
            write: WriteKind::Image,
            lossy: false,
            strict_dims: false,
            known_bug: None,
        },
        // ---- BMP (writer is RGB-Uint8 only; grayscale is rejected up front,
        //      a validated limitation — not exercised here) ----
        Case {
            label: "BMP rgb8",
            file: "bmp_rgb8.bmp",
            meta: rgb8(),
            write: WriteKind::Image,
            lossy: false,
            strict_dims: false,
            known_bug: None,
        },
        // ---- ICS (raw/stack writer) ----
        // The IcsWriter header has two distinct line-ending requirements that took
        // two fixes to satisfy. (1) The leading `ics_version\t2.0` line must be
        // CRLF-terminated so it is exactly 17 bytes — Java's v2 probe does
        // `readString(17).trim() == "ics_version\t2.0"`, and a bare LF (16 bytes)
        // makes it read one byte into `filename`, fall back to v1, and demand a
        // nonexistent `.ids`. (2) The trailing `end` line must use a bare LF, NOT
        // CRLF: Java locates the pixel offset with `in.readString(NL)` (NL =
        // "\r\n"), which stops at the FIRST terminator char and consumes only it,
        // so `end\r\n` leaves the `\n` inside the pixel stream and shifts every
        // plane by one byte. Both are now handled; Java reads our pixels bitwise.
        Case {
            label: "ICS uint8 Z=3",
            file: "ics_u8_z3.ics",
            meta: gray_zstack(PixelType::Uint8, 3),
            write: WriteKind::Image,
            lossy: false,
            strict_dims: false,
            known_bug: None,
        },
        // ---- JPEG (lossy; tolerant check on a smooth gradient) ----
        Case {
            label: "JPEG rgb8 (lossy)",
            file: "jpeg_rgb8.jpg",
            meta: rgb8(),
            write: WriteKind::Image,
            lossy: true,
            strict_dims: false,
            known_bug: None,
        },
    ]
}

fn write_case(case: &Case, path: &Path) -> Result<(), String> {
    let planes: Vec<Vec<u8>> = (0..case.meta.image_count)
        .map(|p| build_input_plane(&case.meta, p, case.smooth()))
        .collect();
    match case.write {
        WriteKind::Image => ImageWriter::save(path, &case.meta, &planes).map_err(|e| e.to_string()),
        WriteKind::OmeTiff => {
            let ome = OmeMetadata::from_image_metadata(&case.meta);
            ImageWriter::save_ome_tiff(path, &case.meta, &ome, &planes).map_err(|e| e.to_string())
        }
    }
}

/// Assemble the case result, demoting any failures to reported "known bug"
/// lines (so a documented, still-unfixed writer bug stays loudly visible
/// without blocking the suite). Returns (hard_failures, info, known_bugs).
fn finish(
    case: &Case,
    fails: Vec<String>,
    info: Vec<String>,
) -> (Vec<String>, Vec<String>, Vec<String>) {
    if let (Some(note), false) = (case.known_bug, fails.is_empty()) {
        let known: Vec<String> = fails
            .into_iter()
            .map(|f| format!("{f}  [KNOWN BUG: {note}]"))
            .collect();
        return (Vec::new(), info, known);
    }
    (fails, info, Vec::new())
}

/// Returns (hard_failures, info_lines, known_bug_lines) for one case.
fn verify_case(cp: &str, case: &Case) -> (Vec<String>, Vec<String>, Vec<String>) {
    let mut fails = Vec::new();
    let mut info = Vec::new();
    let path = temp_path(case.file);

    if let Err(e) = write_case(case, &path) {
        fails.push(format!("{}: WRITE failed: {e}", case.label));
        return finish(case, fails, info);
    }

    let Some(j) = run_oracle(cp, &path) else {
        fails.push(format!("{}: oracle produced no output", case.label));
        return finish(case, fails, info);
    };
    if j.get("ok").and_then(Json::as_bool) != Some(true) {
        fails.push(format!(
            "{}: Java FAILED to read our file: {}",
            case.label,
            j.get("error").and_then(Json::as_str).unwrap_or("?")
        ));
        let _ = std::fs::remove_file(&path);
        return finish(case, fails, info);
    }

    let js = match j
        .get("series")
        .and_then(Json::as_array)
        .and_then(|a| a.first())
    {
        Some(s) => s,
        None => {
            fails.push(format!("{}: oracle returned no series", case.label));
            let _ = std::fs::remove_file(&path);
            return finish(case, fails, info);
        }
    };

    let m = &case.meta;

    // --- always-hard core checks ---
    if js.u("sizeX") != m.size_x as u64 {
        fails.push(format!(
            "{}: sizeX java={} rust={}",
            case.label,
            js.u("sizeX"),
            m.size_x
        ));
    }
    if js.u("sizeY") != m.size_y as u64 {
        fails.push(format!(
            "{}: sizeY java={} rust={}",
            case.label,
            js.u("sizeY"),
            m.size_y
        ));
    }
    if js.u("imageCount") != m.image_count as u64 {
        fails.push(format!(
            "{}: imageCount java={} rust={}",
            case.label,
            js.u("imageCount"),
            m.image_count
        ));
    }
    let jpt = js["pixelType"].as_str().unwrap_or("?");
    let pt_ok = jpt == pixel_type_to_java(m.pixel_type);
    if !pt_ok {
        fails.push(format!(
            "{}: pixelType java={} rust={}",
            case.label,
            jpt,
            pixel_type_to_java(m.pixel_type)
        ));
    }

    // --- dimension axes: hard for OME formats, informational otherwise ---
    let mut dim_diffs: Vec<String> = [
        ("sizeZ", js.u("sizeZ"), m.size_z as u64),
        ("sizeC", js.u("sizeC"), m.size_c as u64),
        ("sizeT", js.u("sizeT"), m.size_t as u64),
    ]
    .into_iter()
    .filter(|(_, jv, rv)| jv != rv)
    .map(|(n, jv, rv)| format!("{n} java={jv} rust={rv}"))
    .collect();
    let jdo = js["dimensionOrder"].as_str().unwrap_or("?");
    if jdo != dim_order_str(m.dimension_order) {
        dim_diffs.push(format!(
            "dimensionOrder java={jdo} rust={}",
            dim_order_str(m.dimension_order)
        ));
    }
    if !dim_diffs.is_empty() {
        if case.strict_dims {
            fails.push(format!("{}: {}", case.label, dim_diffs.join("; ")));
        } else {
            info.push(format!(
                "{}: axis remap (format has no explicit Z/C/T): {}",
                case.label,
                dim_diffs.join("; ")
            ));
        }
    }

    // --- pixels ---
    if pt_ok {
        let empty = Vec::new();
        let planes = js["planeCrc"].as_array().unwrap_or(&empty);
        let mut px_ok = 0usize;
        let mut px_total = 0usize;
        let mut worst: Option<String> = None;
        for pj in planes {
            if pj.get("error").is_some() {
                fails.push(format!(
                    "{}: Java could not read plane {}: {}",
                    case.label,
                    pj.u("plane"),
                    pj.get("error").and_then(Json::as_str).unwrap_or("?")
                ));
                continue;
            }
            let p = pj.u("plane") as u32;
            px_total += 1;
            let expected = build_expected_region(js, m.pixel_type, p, case.smooth());
            let jcrc = pj.u("crc");
            let jlen = pj.u("len");

            if !case.lossy {
                let ecrc = crc32_ieee(&expected) as u64;
                if ecrc == jcrc && expected.len() as u64 == jlen {
                    px_ok += 1;
                } else if worst.is_none() {
                    // characterise the divergence against Java's raw bytes
                    let detail = if let Some(b64) = pj["b64"].as_str() {
                        let jb = b64_decode(b64);
                        if jb.len() == expected.len() {
                            let (maxd, ndiff) = jb
                                .iter()
                                .zip(&expected)
                                .fold((0u8, 0usize), |(mx, n), (a, b)| {
                                    (mx.max(a.abs_diff(*b)), n + (a != b) as usize)
                                });
                            format!("maxdiff={maxd} over {ndiff}/{} bytes", expected.len())
                        } else {
                            format!("len java={} ours={}", jb.len(), expected.len())
                        }
                    } else {
                        format!("crc java={jcrc} ours={ecrc}")
                    };
                    worst = Some(format!("plane{p}: {detail}"));
                }
            } else {
                // lossy: tolerant mean-abs-error compare against Java's bytes
                let Some(b64) = pj["b64"].as_str() else {
                    worst = Some(format!("plane{p}: no base64 from oracle"));
                    continue;
                };
                let jb = b64_decode(b64);
                if jb.len() != expected.len() {
                    worst = Some(format!(
                        "plane{p}: len java={} ours={}",
                        jb.len(),
                        expected.len()
                    ));
                    continue;
                }
                let (sum, maxd) = jb
                    .iter()
                    .zip(&expected)
                    .fold((0u64, 0u8), |(s, mx), (a, b)| {
                        let d = a.abs_diff(*b);
                        (s + d as u64, mx.max(d))
                    });
                let mean = sum as f64 / expected.len().max(1) as f64;
                if mean <= JPEG_MEAN_TOL {
                    px_ok += 1;
                    info.push(format!(
                        "{} plane{p}: JPEG tolerant (mean={mean:.2}, max={maxd})",
                        case.label
                    ));
                } else {
                    worst = Some(format!("plane{p}: JPEG mean={mean:.2} > {JPEG_MEAN_TOL}"));
                }
            }
        }
        if px_total > 0 && px_ok == px_total {
            info.push(format!(
                "{}: pixels {} OK ({})",
                case.label,
                px_ok,
                if case.lossy { "tolerant" } else { "bitwise" }
            ));
        } else if let Some(w) = worst {
            fails.push(format!(
                "{}: pixels {}/{} ok — {}",
                case.label, px_ok, px_total, w
            ));
        }
    }

    let _ = std::fs::remove_file(&path);
    finish(case, fails, info)
}

#[test]
fn java_writer_parity() {
    if std::env::var("BIOFORMATS_RS_JAVA_PARITY").as_deref() == Ok("0") {
        eprintln!("SKIP writer-parity: BIOFORMATS_RS_JAVA_PARITY=0");
        return;
    }
    let Some(cp) = oracle_classpath() else { return };

    let mut all_fails: Vec<String> = Vec::new();
    let mut all_info: Vec<String> = Vec::new();
    let mut all_known: Vec<String> = Vec::new();

    for case in cases() {
        let (fails, info, known) = verify_case(cp, &case);
        let status = if !fails.is_empty() {
            "BAD"
        } else if !known.is_empty() {
            "BUG"
        } else {
            "ok "
        };
        println!("== {} {} ==", status, case.label);
        for line in &info {
            println!("   . {line}");
        }
        for line in &known {
            println!("   ! {line}");
        }
        for line in &fails {
            println!("   X {line}");
        }
        all_info.extend(info);
        all_known.extend(known);
        all_fails.extend(fails);
    }

    println!("\n========== WRITER PARITY SUMMARY ==========");
    println!("info lines        : {}", all_info.len());
    println!("known writer bugs : {}", all_known.len());
    println!("hard failures     : {}", all_fails.len());
    if !all_known.is_empty() {
        println!("-- known writer bugs (reported, not fatal) --");
        for line in &all_known {
            println!("  ! {line}");
        }
    }
    println!("===========================================");

    if !all_fails.is_empty() {
        panic!(
            "Writer<->Java parity divergence ({} issue(s)):\n  - {}",
            all_fails.len(),
            all_fails.join("\n  - ")
        );
    }
}
