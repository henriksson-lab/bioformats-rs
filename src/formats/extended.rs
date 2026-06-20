//! Extended format readers for Bio-Formats Rust.
//!
//! Group A: TIFF-based wrappers (DNG, QPTIFF, GEL).
//! Group B: Binary readers with structure (Imspector OBF, Hamamatsu VMS, Cellomics).
//! Group C: Extension-only unsupported detectors and small native readers.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{
    DimensionOrder, ImageMetadata, LookupTable, MetadataLevel, MetadataOptions, MetadataValue,
};
use crate::common::ome_metadata::{
    create_lsid, OmeChannel, OmeDetector, OmeImage, OmeInstrument, OmeMetadata, OmeObjective,
    OmePlate, OmeROI, OmeShape, OmeWell, OmeWellSample,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use crate::tiff::jpeg_restart;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Macro: thin TIFF wrapper
// ---------------------------------------------------------------------------
#[allow(unused_macros)]
macro_rules! tiff_wrapper {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
    ) => {
        $(#[$attr])*
        pub struct $name {
            inner: crate::tiff::TiffReader,
        }

        impl $name {
            pub fn new() -> Self {
                $name { inner: crate::tiff::TiffReader::new() }
            }
        }

        impl Default for $name {
            fn default() -> Self { Self::new() }
        }

        impl FormatReader for $name {
            fn is_this_type_by_name(&self, path: &Path) -> bool {
                let ext = path.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase());
                matches!(ext.as_deref(), $(Some($ext))|+)
            }

            fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

            fn set_id(&mut self, path: &Path) -> Result<()> {
                self.inner.set_id(path)
            }

            fn close(&mut self) -> Result<()> {
                self.inner.close()
            }

            fn series_count(&self) -> usize {
                self.inner.series_count()
            }

            fn set_series(&mut self, s: usize) -> Result<()> {
                self.inner.set_series(s)
            }

            fn series(&self) -> usize {
                self.inner.series()
            }

            fn metadata(&self) -> &ImageMetadata {
                self.inner.metadata()
            }

            fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
                self.inner.open_bytes(p)
            }

            fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
                self.inner.open_bytes_region(p, x, y, w, h)
            }

            fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
                self.inner.open_thumb_bytes(p)
            }

            fn resolution_count(&self) -> usize {
                self.inner.resolution_count()
            }

            fn set_resolution(&mut self, level: usize) -> Result<()> {
                self.inner.set_resolution(level)
            }
        }
    };
}

// ---------------------------------------------------------------------------
// Macro: extension-only placeholder reader
// ---------------------------------------------------------------------------
#[allow(unused_macros)]
macro_rules! placeholder_reader {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
    ) => {
        $(#[$attr])*
        pub struct $name {
            path: Option<PathBuf>,
            meta: Option<ImageMetadata>,
        }

        impl $name {
            pub fn new() -> Self {
                $name { path: None, meta: None }
            }
        }

        impl Default for $name {
            fn default() -> Self { Self::new() }
        }

        impl FormatReader for $name {
            fn is_this_type_by_name(&self, path: &Path) -> bool {
                let ext = path.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase());
                matches!(ext.as_deref(), $(Some($ext))|+)
            }

            fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

            fn set_id(&mut self, _path: &Path) -> Result<()> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " native payload decoding is unsupported").to_string()
                ))
            }

            fn close(&mut self) -> Result<()> {
                self.path = None;
                self.meta = None;
                Ok(())
            }

            fn series_count(&self) -> usize { 0 }

            fn set_series(&mut self, s: usize) -> Result<()> {
                Err(BioFormatsError::SeriesOutOfRange(s))
            }

            fn series(&self) -> usize { 0 }

            fn metadata(&self) -> &ImageMetadata {
                self.meta.as_ref().unwrap_or(crate::common::reader::uninitialized_metadata())
            }

            fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " native payload decoding is unsupported").to_string()
                ))
            }

            fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " native payload decoding is unsupported").to_string()
                ))
            }

            fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " native payload decoding is unsupported").to_string()
                ))
            }

            fn resolution_count(&self) -> usize { 1 }

            fn set_resolution(&mut self, level: usize) -> Result<()> {
                if level != 0 { Err(BioFormatsError::Format(format!("resolution {} out of range", level))) }
                else { Ok(()) }
            }
        }
    };
}

// ===========================================================================
// Group A — TIFF-based wrappers
// ===========================================================================

// ---------------------------------------------------------------------------
// 1. Adobe DNG (Digital Negative) RAW
// ---------------------------------------------------------------------------
/// Adobe / Canon DNG (Digital Negative) RAW format — TIFF-based (`.dng`).
///
/// Port of the upstream Java `DNGReader` (which extends `BaseTiffReader`).
///
/// Two code paths, mirroring Java `openBytes`:
/// * **Developed / RGB** — non-CFA images are decoded directly by `TiffReader`.
/// * **Bayer CFA** — when the first IFD has `PhotometricInterpretation`
///   == `CFA_ARRAY` (32803), the strips hold a single-channel bit-packed Bayer
///   mosaic. This reader concatenates the strips, unpacks the
///   `BitsPerSample[0]`-bit samples MSB-first, splits them into a planar
///   [R|G|B] buffer using the `COLOR_MAP` (320) tag (default `{1,0,2,1}`), and
///   runs `ImageTools.interpolate` to produce an interleaved RGB UINT16 plane —
///   exactly as Java does.
///
/// White-balance expansion ports `DNGReader.initStandardMetadata` +
/// `adjustForWhiteBalance`: when the Canon EXIF maker-note
/// (`WHITE_BALANCE_RGB_COEFFS`, via [`TiffReader::dng_white_balance`]) supplies
/// per-channel gains, each demosaiced sample is scaled by its channel gain. If
/// the maker-note is absent, scaling is a no-op (identical to Java's
/// `whiteBalance == null` case).
pub struct DngReader {
    inner: crate::tiff::TiffReader,
    /// CFA decode state, populated only for Bayer-CFA DNGs.
    cfa: Option<DngCfa>,
}

/// State for a Bayer-CFA DNG (raw mosaic that needs demosaicing).
struct DngCfa {
    path: PathBuf,
    meta: ImageMetadata,
    color_map: [i32; 4],
    data_size: u32,
    /// (offset, byte_count) of each strip making up the raw mosaic.
    strips: Vec<(usize, usize)>,
    /// Per-channel white-balance RGB gains from the Canon EXIF maker-note
    /// (`WHITE_BALANCE_RGB_COEFFS`). `None` means white balance is absent and
    /// `adjust_for_white_balance` is a no-op, matching Java.
    white_balance: Option<[f64; 3]>,
    /// Cached demosaiced (or expanded) interleaved RGB plane.
    full_image: Option<Vec<u8>>,
    /// When `true`, take Java's non-demosaic "expand" path
    /// (`DNGReader.openBytes` lines 128-148): expand the raw 8-bit samples to
    /// UINT16 with per-channel white balance and NO Bayer interpolation. This is
    /// selected when `totalStripBytes == planeSize || bitsPerSample.length > 1`,
    /// even for `PhotometricInterpretation == CFA_ARRAY`.
    expand: bool,
}

/// Port of `DNGReader.adjustForWhiteBalance`: scales sample `val` (already
/// masked to 16 bits) by the channel `index` gain when a 3-entry white-balance
/// table is present; otherwise returns `val` unchanged. Java casts the product
/// back to `short`, so we wrap to the low 16 bits identically.
fn adjust_for_white_balance(val: i16, index: usize, wb: &Option<[f64; 3]>) -> i16 {
    match wb {
        Some(w) if index < 3 => ((val as f64 * w[index]) as i64 as u16) as i16,
        _ => val,
    }
}

/// TIFF `PhotometricInterpretation` value for a colour-filter array.
const PHOTO_CFA_ARRAY: u16 = 32803;
/// Java DNGReader.COLOR_MAP: Canon DNG CFA pattern tag, not TIFF palette tag 320.
const DNG_CFA_COLOR_MAP: u16 = 33422;

fn dng_cfa_color_map(ifd: &crate::tiff::ifd::Ifd) -> [i32; 4] {
    // Java default color map {1,0,2,1}; overridden by private COLOR_MAP tag
    // (33422) when all four entries are valid channel indices 0..=2.
    let mut color_map = [1i32, 0, 2, 1];
    let ifd_colors = ifd.get_vec_u16(DNG_CFA_COLOR_MAP);
    if ifd_colors.len() >= 4 {
        let valid = ifd_colors[..4].iter().all(|&c| c <= 2);
        if valid {
            for q in 0..4 {
                color_map[q] = ifd_colors[q] as i32;
            }
        }
    }
    color_map
}

impl DngReader {
    const CANON_TAG: u16 = 34665;
    const TIFF_EPS_STANDARD: u16 = 37398;

    pub fn new() -> Self {
        DngReader {
            inner: crate::tiff::TiffReader::new(),
            cfa: None,
        }
    }

    fn is_canon_dng_tiff(header: &[u8]) -> bool {
        let cursor = std::io::Cursor::new(header);
        let mut parser = match crate::tiff::parser::TiffParser::new(cursor) {
            Ok(parser) => parser,
            Err(_) => return false,
        };
        let ifd = match parser.read_ifd(parser.first_ifd_offset) {
            Ok((ifd, _)) => ifd,
            Err(_) => return false,
        };
        let has_eps_tag =
            ifd.get(Self::TIFF_EPS_STANDARD).is_some() || ifd.get(Self::CANON_TAG).is_some();
        let make = ifd.get_str(271);
        let model = ifd.get_str(272);
        let software = ifd.get_str(crate::tiff::ifd::tag::SOFTWARE);
        matches!(make, Some(make) if make.contains("Canon"))
            && has_eps_tag
            && !matches!(model, Some(model) if model.ends_with("S1 IS"))
            && software.is_none_or(|software| software.contains("Canon"))
    }

    /// Port of the Bayer-CFA branch of `DNGReader.openBytes`. Concatenates the
    /// raw bit-packed strips, splits into a planar [R|G|B] short buffer, and
    /// interpolates into an interleaved RGB UINT16 plane.
    fn decode_cfa(cfa: &DngCfa) -> Result<Vec<u8>> {
        use crate::formats::camera2::cfa as cfahelp;

        let file = std::fs::read(&cfa.path).map_err(BioFormatsError::Io)?;
        let size_x = cfa.meta.size_x as usize;
        let size_y = cfa.meta.size_y as usize;
        let little = cfa.meta.is_little_endian;

        // Java concatenates all strips into one buffer then bit-reads it.
        let mut src: Vec<u8> = Vec::new();
        for &(off, cnt) in &cfa.strips {
            if off < file.len() {
                let end = (off + cnt).min(file.len());
                src.extend_from_slice(&file[off..end]);
            }
        }

        let mut bits = cfahelp::BitReader::new(&src);
        let mut pix = vec![0i16; size_x * size_y * 3];

        for row in 0..size_y {
            for col in 0..size_x {
                let val = (bits.read_bits(cfa.data_size) & 0xffff) as u16 as i16;
                let map_index = (row % 2) * 2 + (col % 2);

                let red_offset = row * size_x + col;
                let green_offset = (size_y + row) * size_x + col;
                let blue_offset = (2 * size_y + row) * size_x + col;

                match cfa.color_map[map_index] {
                    0 => pix[red_offset] = adjust_for_white_balance(val, 0, &cfa.white_balance),
                    1 => pix[green_offset] = adjust_for_white_balance(val, 1, &cfa.white_balance),
                    2 => pix[blue_offset] = adjust_for_white_balance(val, 2, &cfa.white_balance),
                    _ => {}
                }
            }
        }

        let mut full = vec![0u8; size_x * size_y * 3 * 2];
        cfahelp::interpolate(&pix, &mut full, &cfa.color_map, size_x, size_y, little);
        Ok(full)
    }

    /// Port of the non-demosaic "expand" branch of `DNGReader.openBytes`
    /// (lines 128-148): the TIFF stores UINT8 samples, but the output pixel type
    /// is UINT16. Read the raw samples (Java's `tiffParser.getSamples`), then for
    /// each sample expand `b[i] & 0xff` to a 16-bit value scaled by the
    /// per-channel white balance (`adjustForWhiteBalance`) and pack it
    /// little/big-endian. No Bayer interpolation is performed.
    fn decode_expand(&mut self) -> Result<Vec<u8>> {
        let (size_x, size_y, little, interleaved, wb) = {
            let c = self.cfa.as_ref().unwrap();
            (
                c.meta.size_x as usize,
                c.meta.size_y as usize,
                c.meta.is_little_endian,
                c.meta.is_interleaved,
                c.white_balance,
            )
        };

        // Java reads the raw decompressed samples for the plane via
        // tiffParser.getSamples on IFD 0. The inner TiffReader decodes the CFA
        // IFD as a plain UINT8 image, so open_bytes(0) yields the same raw bytes.
        let raw = self.inner.open_bytes(0)?;

        // Java: b has length buf.length / 2 == sizeX * sizeY * 3 (the UINT16 RGB
        // plane has twice as many bytes). Only that many samples are expanded.
        let n = (size_x * size_y * 3).min(raw.len());
        let per_channel = (raw.len() / 3).max(1);
        let mut full = vec![0u8; size_x * size_y * 3 * 2];

        for i in 0..n {
            // c = isInterleaved() ? i % 3 : i / (b.length / 3)
            let c = if interleaved { i % 3 } else { i / per_channel };
            let v = (raw[i] & 0xff) as i16;
            let v = adjust_for_white_balance(v, c, &wb) as u16;
            let out = i * 2;
            if little {
                full[out] = (v & 0xff) as u8;
                full[out + 1] = (v >> 8) as u8;
            } else {
                full[out] = (v >> 8) as u8;
                full[out + 1] = (v & 0xff) as u8;
            }
        }
        Ok(full)
    }
}

#[cfg(test)]
mod dng_wb_tests {
    use super::{adjust_for_white_balance, dng_cfa_color_map, DngReader, DNG_CFA_COLOR_MAP};
    use crate::common::reader::FormatReader;
    use crate::tiff::ifd::{tag, Ifd, IfdValue};

    fn canon_tiff_header(make: &str, model: Option<&str>, software: Option<&str>) -> Vec<u8> {
        struct Entry {
            tag: u16,
            ty: u16,
            count: u32,
            inline: Option<u32>,
            data: Vec<u8>,
        }

        let mut entries = vec![Entry {
            tag: DngReader::TIFF_EPS_STANDARD,
            ty: 3,
            count: 1,
            inline: Some(1),
            data: Vec::new(),
        }];
        for (tag, text) in [
            (271u16, Some(make)),
            (272u16, model),
            (crate::tiff::ifd::tag::SOFTWARE, software),
        ] {
            if let Some(text) = text {
                let mut data = text.as_bytes().to_vec();
                data.push(0);
                entries.push(Entry {
                    tag,
                    ty: 2,
                    count: data.len() as u32,
                    inline: None,
                    data,
                });
            }
        }
        entries.sort_by_key(|entry| entry.tag);

        let ifd_start = 8u32;
        let ifd_size = 2 + entries.len() as u32 * 12 + 4;
        let mut data_offset = ifd_start + ifd_size;
        for entry in &mut entries {
            if entry.inline.is_none() {
                entry.inline = Some(data_offset);
                data_offset += entry.data.len() as u32;
            }
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in &entries {
            bytes.extend_from_slice(&entry.tag.to_le_bytes());
            bytes.extend_from_slice(&entry.ty.to_le_bytes());
            bytes.extend_from_slice(&entry.count.to_le_bytes());
            bytes.extend_from_slice(&entry.inline.unwrap().to_le_bytes());
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        for entry in &entries {
            bytes.extend_from_slice(&entry.data);
        }
        bytes
    }

    #[test]
    fn no_white_balance_is_identity() {
        // Java: whiteBalance == null -> adjustForWhiteBalance returns val.
        let wb = None;
        for v in [0i16, 1, 100, 255, 1000, i16::MAX] {
            assert_eq!(adjust_for_white_balance(v, 0, &wb), v);
            assert_eq!(adjust_for_white_balance(v, 1, &wb), v);
            assert_eq!(adjust_for_white_balance(v, 2, &wb), v);
        }
    }

    #[test]
    fn scales_per_channel_and_truncates_like_java() {
        // Java: (short)(val * whiteBalance[index]); cast truncates toward zero
        // and wraps into 16 bits.
        let wb = Some([2.391381f64, 0.929156, 1.298254]);
        // 100 * 2.391381 = 239.13 -> 239
        assert_eq!(adjust_for_white_balance(100, 0, &wb), 239);
        // 100 * 0.929156 = 92.91 -> 92
        assert_eq!(adjust_for_white_balance(100, 1, &wb), 92);
        // 100 * 1.298254 = 129.82 -> 129
        assert_eq!(adjust_for_white_balance(100, 2, &wb), 129);
    }

    #[test]
    fn out_of_range_channel_is_identity() {
        let wb = Some([2.0f64, 2.0, 2.0]);
        assert_eq!(adjust_for_white_balance(50, 3, &wb), 50);
    }

    #[test]
    fn dng_stream_detection_matches_java_canon_tiff_predicate() {
        let reader = DngReader::new();
        let good = canon_tiff_header("Canon", Some("EOS 5D"), Some("Canon Digital"));
        assert!(reader.is_this_type_by_bytes(&good));

        let s1 = canon_tiff_header("Canon", Some("PowerShot S1 IS"), Some("Canon Digital"));
        assert!(!reader.is_this_type_by_bytes(&s1));

        let non_canon_software = canon_tiff_header("Canon", Some("EOS 5D"), Some("Other"));
        assert!(!reader.is_this_type_by_bytes(&non_canon_software));
    }

    #[test]
    fn dng_cfa_pattern_uses_java_private_color_map_tag() {
        let mut ifd = Ifd::default();
        ifd.entries
            .insert(tag::COLOR_MAP, IfdValue::Short(vec![2, 2, 2, 2]));
        ifd.entries
            .insert(DNG_CFA_COLOR_MAP, IfdValue::Short(vec![0, 1, 1, 2]));

        assert_eq!(dng_cfa_color_map(&ifd), [0, 1, 1, 2]);
    }

    #[test]
    fn dng_cfa_pattern_ignores_tiff_palette_color_map_tag() {
        let mut ifd = Ifd::default();
        ifd.entries
            .insert(tag::COLOR_MAP, IfdValue::Short(vec![0, 1, 1, 2]));

        assert_eq!(dng_cfa_color_map(&ifd), [1, 0, 2, 1]);
    }
}

impl Default for DngReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for DngReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dng"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::is_canon_dng_tiff(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;

        // Inspect the first IFD: a Bayer-CFA DNG has PhotometricInterpretation
        // == CFA_ARRAY (32803). Anything else is returned via the TIFF reader.
        if let Some(ifd) = self.inner.ifd(0) {
            let photo = ifd
                .get_u16(crate::tiff::ifd::tag::PHOTOMETRIC_INTERPRETATION)
                .unwrap_or(1);
            if photo == PHOTO_CFA_ARRAY {
                let bps = ifd.bits_per_sample();
                let data_size = *bps.first().unwrap_or(&16) as u32;
                let bps_len = bps.len();

                let color_map = dng_cfa_color_map(ifd);

                let size_x = ifd.image_width().unwrap_or(0);
                let size_y = ifd.image_length().unwrap_or(0);

                // Strip layout: offsets + byte counts (single strip is common).
                let offsets = ifd.get_vec_u64(crate::tiff::ifd::tag::STRIP_OFFSETS);
                let counts = ifd.get_vec_u64(crate::tiff::ifd::tag::STRIP_BYTE_COUNTS);
                let mut strips: Vec<(usize, usize)> = Vec::new();
                for i in 0..offsets.len() {
                    let cnt = counts.get(i).copied().unwrap_or(0);
                    strips.push((offsets[i] as usize, cnt as usize));
                }

                if size_x == 0 || size_y == 0 || strips.is_empty() {
                    return Err(BioFormatsError::UnsupportedFormat(
                        "DNG CFA: missing dimensions or strip layout".into(),
                    ));
                }

                // Java DNGReader.openBytes:128 — take the non-demosaic "expand"
                // path when the stored data already covers a full UINT16 RGB
                // plane (`totalStripBytes == getPlaneSize()`) or there is more
                // than one sample per pixel (`bps.length > 1`). getPlaneSize()
                // here = sizeX * sizeY * sizeC(=3) * bytesPerPixel(UINT16=2).
                let total_strip_bytes: u64 = counts.iter().copied().sum();
                let plane_size = (size_x as u64) * (size_y as u64) * 3 * 2;
                let expand = total_strip_bytes == plane_size || bps_len > 1;

                // Java: UINT16, RGB, interleaved, sizeC=3, sizeZ=sizeT=1.
                let base = self.inner.metadata();
                let meta = ImageMetadata {
                    size_x,
                    size_y,
                    size_z: 1,
                    size_c: 3,
                    size_t: 1,
                    pixel_type: PixelType::Uint16,
                    bits_per_pixel: 16,
                    image_count: 1,
                    dimension_order: DimensionOrder::XYCZT,
                    is_rgb: true,
                    is_interleaved: true,
                    is_indexed: false,
                    is_little_endian: base.is_little_endian,
                    resolution_count: 1,
                    thumbnail: false,
                    series_metadata: HashMap::new(),
                    lookup_table: None,
                    modulo_z: None,
                    modulo_c: None,
                    modulo_t: None,
                };

                // Canon EXIF maker-note white-balance gains (tag 16385 via the
                // EXIF sub-IFD 34665 -> maker-note IFD). `None` => no-op.
                let white_balance = self.inner.dng_white_balance();

                self.cfa = Some(DngCfa {
                    path: path.to_path_buf(),
                    meta,
                    color_map,
                    data_size,
                    strips,
                    white_balance,
                    full_image: None,
                    expand,
                });
                return Ok(());
            }
        }
        self.cfa = None;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.cfa = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        if self.cfa.is_some() {
            1
        } else {
            self.inner.series_count()
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.cfa.is_some() {
            if s != 0 {
                Err(BioFormatsError::SeriesOutOfRange(s))
            } else {
                Ok(())
            }
        } else {
            self.inner.set_series(s)
        }
    }

    fn series(&self) -> usize {
        if self.cfa.is_some() {
            0
        } else {
            self.inner.series()
        }
    }

    fn metadata(&self) -> &ImageMetadata {
        if let Some(c) = &self.cfa {
            &c.meta
        } else {
            self.inner.metadata()
        }
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if self.cfa.is_some() {
            if p != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(p));
            }
            if self.cfa.as_ref().unwrap().full_image.is_none() {
                let img = if self.cfa.as_ref().unwrap().expand {
                    self.decode_expand()?
                } else {
                    Self::decode_cfa(self.cfa.as_ref().unwrap())?
                };
                self.cfa.as_mut().unwrap().full_image = Some(img);
            }
            return Ok(self.cfa.as_ref().unwrap().full_image.clone().unwrap());
        }
        self.inner.open_bytes(p)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.cfa.is_some() {
            let full = self.open_bytes(p)?;
            let meta = self.metadata().clone();
            return crop_full_plane("DNG", &full, &meta, 3, x, y, w, h);
        }
        self.inner.open_bytes_region(p, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if self.cfa.is_some() {
            return self.open_bytes(p);
        }
        self.inner.open_thumb_bytes(p)
    }

    fn resolution_count(&self) -> usize {
        if self.cfa.is_some() {
            1
        } else {
            self.inner.resolution_count()
        }
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.cfa.is_some() {
            if level != 0 {
                Err(BioFormatsError::Format(format!(
                    "resolution {} out of range",
                    level
                )))
            } else {
                Ok(())
            }
        } else {
            self.inner.set_resolution(level)
        }
    }
}

// ---------------------------------------------------------------------------
// 2. Akoya/PerkinElmer Phenocycler QPTIFF
// ---------------------------------------------------------------------------
/// Akoya/PerkinElmer Phenocycler QPTIFF — TIFF-based (`.qptiff`).
pub struct VectraReader {
    inner: crate::tiff::TiffReader,
    meta: Option<ImageMetadata>,
    current_resolution: usize,
}

impl VectraReader {
    pub fn new() -> Self {
        VectraReader {
            inner: crate::tiff::TiffReader::new(),
            meta: None,
            current_resolution: 0,
        }
    }

    fn current_ifd_indices(&self) -> Vec<usize> {
        let Some(series) = self.inner.series_list().get(self.inner.series()) else {
            return Vec::new();
        };
        if self.current_resolution == 0 {
            series.ifd_indices.clone()
        } else {
            series
                .sub_resolutions
                .get(self.current_resolution - 1)
                .cloned()
                .unwrap_or_default()
        }
    }

    fn refresh_metadata(&mut self) {
        let mut meta = self.inner.metadata().clone();
        qptiff_enrich_metadata(&self.inner, &mut meta, &self.current_ifd_indices());
        self.meta = Some(meta);
    }
}

impl Default for VectraReader {
    fn default() -> Self {
        Self::new()
    }
}

const QPTIFF_SOFTWARE_CHECK: &str = "PerkinElmer-QPI";

fn qptiff_ifd_value_summary(value: &crate::tiff::ifd::IfdValue) -> Option<MetadataValue> {
    use crate::tiff::ifd::IfdValue;
    match value {
        IfdValue::Ascii(text) => {
            let text = text.trim();
            if text.is_empty() {
                None
            } else {
                Some(MetadataValue::String(text.chars().take(8192).collect()))
            }
        }
        IfdValue::Short(v) => Some(MetadataValue::String(
            v.iter().map(u16::to_string).collect::<Vec<_>>().join(","),
        )),
        IfdValue::Long(v) => Some(MetadataValue::String(
            v.iter().map(u32::to_string).collect::<Vec<_>>().join(","),
        )),
        IfdValue::Long8(v) => Some(MetadataValue::String(
            v.iter().map(u64::to_string).collect::<Vec<_>>().join(","),
        )),
        IfdValue::Rational(v) => Some(MetadataValue::String(
            v.iter()
                .map(|(n, d)| format!("{n}/{d}"))
                .collect::<Vec<_>>()
                .join(","),
        )),
        IfdValue::Float(v) => Some(MetadataValue::String(
            v.iter().map(f32::to_string).collect::<Vec<_>>().join(","),
        )),
        IfdValue::Double(v) => Some(MetadataValue::String(
            v.iter().map(f64::to_string).collect::<Vec<_>>().join(","),
        )),
        IfdValue::Byte(v) | IfdValue::Undefined(v) => Some(MetadataValue::Int(v.len() as i64)),
        _ => None,
    }
}

fn qptiff_tag_name(tag: u16) -> Option<&'static str> {
    match tag {
        270 => Some("ImageDescription"),
        305 => Some("Software"),
        306 => Some("DateTime"),
        315 => Some("Artist"),
        330 => Some("SubIFDs"),
        33432 => Some("Copyright"),
        34675 => Some("ICCProfile"),
        50215 => Some("OceScanjobDescription"),
        65000..=65535 => Some("Private"),
        _ => None,
    }
}

fn qptiff_insert_description_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    ifd_index: usize,
    description: &str,
) {
    let description = description.trim_matches(char::from(0)).trim();
    if description.is_empty() {
        return;
    }
    metadata.insert(
        format!("qptiff.ifd.{ifd_index}.ImageDescription"),
        MetadataValue::String(description.chars().take(8192).collect()),
    );

    for (line_index, raw_line) in description
        .split(['\n', '\r', ';', '|'])
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
        .take(256)
    {
        let Some((key, value)) = raw_line.split_once('=') else {
            continue;
        };
        let key = key.trim().trim_matches(char::from(0));
        let value = value.trim().trim_matches(char::from(0));
        if key.is_empty() || value.is_empty() {
            continue;
        }
        let safe_key = key
            .chars()
            .map(|ch| {
                if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
                    ch
                } else {
                    '_'
                }
            })
            .collect::<String>();
        metadata.insert(
            format!("qptiff.ifd.{ifd_index}.description.{line_index}.{safe_key}"),
            MetadataValue::String(value.chars().take(4096).collect()),
        );
    }

    qptiff_insert_vendor_json_metadata(metadata, ifd_index, description);
}

fn qptiff_metadata_key_segment(raw: &str) -> String {
    let key = raw
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' || ch == '.' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if key.is_empty() {
        "_".into()
    } else {
        key
    }
}

fn qptiff_insert_vendor_json_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    ifd_index: usize,
    description: &str,
) {
    let text = description.trim_matches(char::from(0)).trim();
    if text.len() > 65_536 || !(text.starts_with('{') || text.starts_with('[')) {
        return;
    }
    let Ok(value) = serde_json::from_str::<serde_json::Value>(text) else {
        return;
    };
    let graph_nodes = qptiff_insert_vendor_json_graph_metadata(metadata, ifd_index, &value);
    let mut inserted = 0usize;
    qptiff_flatten_vendor_json_value(
        metadata,
        ifd_index,
        &value,
        &mut Vec::new(),
        0,
        &mut inserted,
    );
    if inserted > 0 || graph_nodes > 0 {
        metadata.insert(
            format!("qptiff.ifd.{ifd_index}.vendor_object.format"),
            MetadataValue::String("json".into()),
        );
    }
    if inserted > 0 {
        metadata.insert(
            format!("qptiff.ifd.{ifd_index}.vendor_object.scalar_count"),
            MetadataValue::Int(inserted as i64),
        );
    }
    if graph_nodes > 0 {
        metadata.insert(
            format!("qptiff.ifd.{ifd_index}.vendor_object.graph.node_count"),
            MetadataValue::Int(graph_nodes as i64),
        );
    }
}

fn qptiff_insert_vendor_json_graph_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    ifd_index: usize,
    value: &serde_json::Value,
) -> usize {
    let mut node_count = 0usize;
    qptiff_walk_vendor_json_graph(
        metadata,
        ifd_index,
        value,
        &mut Vec::new(),
        0,
        &mut node_count,
    );
    node_count
}

fn qptiff_walk_vendor_json_graph(
    metadata: &mut HashMap<String, MetadataValue>,
    ifd_index: usize,
    value: &serde_json::Value,
    path: &mut Vec<String>,
    depth: usize,
    node_count: &mut usize,
) {
    if depth > 8 || *node_count >= 128 {
        return;
    }

    let (kind, children): (&str, Vec<(String, &serde_json::Value)>) = match value {
        serde_json::Value::Object(map) => (
            "object",
            map.iter()
                .take(256)
                .map(|(key, child)| (qptiff_metadata_key_segment(key), child))
                .collect(),
        ),
        serde_json::Value::Array(items) => (
            "array",
            items
                .iter()
                .enumerate()
                .take(256)
                .map(|(index, child)| (index.to_string(), child))
                .collect(),
        ),
        _ => return,
    };

    let child_count = children.len();
    let container_child_count = children
        .iter()
        .filter(|(_, child)| child.is_object() || child.is_array())
        .count();
    let scalar_child_count = child_count.saturating_sub(container_child_count);
    let node_index = *node_count;
    *node_count += 1;

    let prefix = format!("qptiff.ifd.{ifd_index}.vendor_object.graph.{node_index}");
    metadata.insert(
        format!("{prefix}.path"),
        MetadataValue::String(qptiff_vendor_json_graph_path(path)),
    );
    metadata.insert(format!("{prefix}.type"), MetadataValue::String(kind.into()));
    metadata.insert(format!("{prefix}.depth"), MetadataValue::Int(depth as i64));
    metadata.insert(
        format!("{prefix}.child_count"),
        MetadataValue::Int(child_count as i64),
    );
    metadata.insert(
        format!("{prefix}.container_child_count"),
        MetadataValue::Int(container_child_count as i64),
    );
    metadata.insert(
        format!("{prefix}.scalar_child_count"),
        MetadataValue::Int(scalar_child_count as i64),
    );

    if matches!(value, serde_json::Value::Object(_)) && !children.is_empty() {
        metadata.insert(
            format!("{prefix}.keys"),
            MetadataValue::String(
                children
                    .iter()
                    .take(64)
                    .map(|(key, _)| key.as_str())
                    .collect::<Vec<_>>()
                    .join("|"),
            ),
        );
    }

    if depth >= 8 {
        return;
    }

    for (segment, child) in children {
        if !(child.is_object() || child.is_array()) {
            continue;
        }
        path.push(segment);
        qptiff_walk_vendor_json_graph(metadata, ifd_index, child, path, depth + 1, node_count);
        path.pop();
        if *node_count >= 128 {
            break;
        }
    }
}

fn qptiff_vendor_json_graph_path(path: &[String]) -> String {
    if path.is_empty() {
        "$".into()
    } else {
        path.join(".")
    }
}

fn qptiff_insert_vendor_json_semantic_scalar(
    metadata: &mut HashMap<String, MetadataValue>,
    ifd_index: usize,
    path: &[String],
    value: &MetadataValue,
) {
    let Some(last) = path.last() else {
        return;
    };
    let last_lower = last.to_ascii_lowercase();
    let channel_index = path.windows(2).find_map(|pair| {
        let container = pair[0].to_ascii_lowercase();
        if matches!(container.as_str(), "channels" | "channel") {
            pair[1].parse::<usize>().ok()
        } else {
            None
        }
    });

    if last_lower == "name" {
        if let Some(channel) = channel_index {
            metadata.insert(
                format!("qptiff.ifd.{ifd_index}.semantic.channel.{channel}.name"),
                value.clone(),
            );
        }
        return;
    }

    let alias = match last_lower.as_str() {
        "channelname" | "channel_name" => Some("channel.0.name"),
        "exposuretime" | "exposure_time" => Some("acquisition.exposure_time"),
        "exposuretimeus" | "exposure_time_us" => Some("acquisition.exposure_time_us"),
        "exposuretimems" | "exposure_time_ms" => Some("acquisition.exposure_time_ms"),
        "excitationwavelength" | "excitation_wavelength" => Some("channel.0.excitation_wavelength"),
        "emissionwavelength" | "emission_wavelength" => Some("channel.0.emission_wavelength"),
        "objective" | "objectivename" | "objective_name" => Some("instrument.objective"),
        "instrument" | "instrumentname" | "instrument_name" => Some("instrument.name"),
        _ => None,
    };
    if let Some(alias) = alias {
        let alias = if let Some(channel) = channel_index {
            alias.replace("channel.0.", &format!("channel.{channel}."))
        } else {
            alias.to_string()
        };
        metadata.insert(
            format!("qptiff.ifd.{ifd_index}.semantic.{alias}"),
            value.clone(),
        );
    }
}

fn qptiff_flatten_vendor_json_value(
    metadata: &mut HashMap<String, MetadataValue>,
    ifd_index: usize,
    value: &serde_json::Value,
    path: &mut Vec<String>,
    depth: usize,
    inserted: &mut usize,
) {
    if depth > 8 || *inserted >= 256 {
        return;
    }
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map.iter().take(256) {
                path.push(qptiff_metadata_key_segment(key));
                qptiff_flatten_vendor_json_value(
                    metadata,
                    ifd_index,
                    child,
                    path,
                    depth + 1,
                    inserted,
                );
                path.pop();
                if *inserted >= 256 {
                    break;
                }
            }
        }
        serde_json::Value::Array(items) => {
            for (index, child) in items.iter().enumerate().take(256) {
                path.push(index.to_string());
                qptiff_flatten_vendor_json_value(
                    metadata,
                    ifd_index,
                    child,
                    path,
                    depth + 1,
                    inserted,
                );
                path.pop();
                if *inserted >= 256 {
                    break;
                }
            }
        }
        serde_json::Value::String(text) if !path.is_empty() => {
            let metadata_value = MetadataValue::String(text.chars().take(4096).collect());
            metadata.insert(
                format!("qptiff.ifd.{ifd_index}.vendor_object.{}", path.join(".")),
                metadata_value.clone(),
            );
            qptiff_insert_vendor_json_semantic_scalar(metadata, ifd_index, path, &metadata_value);
            *inserted += 1;
        }
        serde_json::Value::Number(number) if !path.is_empty() => {
            let metadata_value = if let Some(v) = number.as_i64() {
                MetadataValue::Int(v)
            } else if let Some(v) = number.as_u64() {
                if v <= i64::MAX as u64 {
                    MetadataValue::Int(v as i64)
                } else {
                    MetadataValue::String(v.to_string())
                }
            } else if let Some(v) = number.as_f64() {
                MetadataValue::Float(v)
            } else {
                MetadataValue::String(number.to_string())
            };
            metadata.insert(
                format!("qptiff.ifd.{ifd_index}.vendor_object.{}", path.join(".")),
                metadata_value.clone(),
            );
            qptiff_insert_vendor_json_semantic_scalar(metadata, ifd_index, path, &metadata_value);
            *inserted += 1;
        }
        serde_json::Value::Bool(value) if !path.is_empty() => {
            let metadata_value = MetadataValue::Bool(*value);
            metadata.insert(
                format!("qptiff.ifd.{ifd_index}.vendor_object.{}", path.join(".")),
                metadata_value.clone(),
            );
            qptiff_insert_vendor_json_semantic_scalar(metadata, ifd_index, path, &metadata_value);
            *inserted += 1;
        }
        _ => {}
    }
}

fn qptiff_enrich_metadata(
    inner: &crate::tiff::TiffReader,
    meta: &mut ImageMetadata,
    ifd_indices: &[usize],
) {
    meta.series_metadata.insert(
        "qptiff.ifd_count".into(),
        MetadataValue::Int(inner.ifd_count() as i64),
    );
    if !ifd_indices.is_empty() {
        meta.series_metadata.insert(
            "qptiff.series_ifds".into(),
            MetadataValue::String(
                ifd_indices
                    .iter()
                    .map(usize::to_string)
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        );
    }

    for &ifd_index in ifd_indices.iter().take(512) {
        let Some(ifd) = inner.ifd(ifd_index) else {
            continue;
        };
        for tag in [
            270u16, 305, 306, 315, 330, 33432, 34675, 50215, 65000, 65001, 65002, 65003, 65004,
            65005, 65200, 65201, 65202, 65203, 65204, 65205,
        ] {
            let Some(value) = ifd.get(tag) else {
                continue;
            };
            if tag == crate::tiff::ifd::tag::IMAGE_DESCRIPTION || (65000..=65535).contains(&tag) {
                if let Some(description) = value.as_str() {
                    qptiff_insert_description_metadata(
                        &mut meta.series_metadata,
                        ifd_index,
                        description,
                    );
                }
            }
            if let Some(summary) = qptiff_ifd_value_summary(value) {
                let label = qptiff_tag_name(tag).unwrap_or("Tag");
                meta.series_metadata
                    .insert(format!("qptiff.ifd.{ifd_index}.tag.{tag}.{label}"), summary);
            }
        }
    }
}

impl FormatReader for VectraReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("qptiff"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        let software = self
            .inner
            .ifd(0)
            .and_then(|ifd| ifd.get(crate::tiff::ifd::tag::SOFTWARE))
            .and_then(|value| value.as_str());
        if !software
            .map(|value| value.starts_with(QPTIFF_SOFTWARE_CHECK))
            .unwrap_or(false)
        {
            let _ = self.inner.close();
            self.meta = None;
            return Err(BioFormatsError::UnsupportedFormat(
                "QPTIFF TIFF is missing PerkinElmer-QPI Software tag".into(),
            ));
        }
        self.current_resolution = 0;
        self.refresh_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta = None;
        self.current_resolution = 0;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)?;
        self.current_resolution = 0;
        self.refresh_metadata();
        Ok(())
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)?;
        self.current_resolution = level;
        self.refresh_metadata();
        Ok(())
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = self
            .inner
            .ome_metadata()
            .unwrap_or_else(|| OmeMetadata::from_image_metadata(meta));
        let _ = ome.add_original_metadata_annotations(meta, 0);
        Some(ome)
    }
}

// ===========================================================================
// Group A — Binary readers with structure
// ===========================================================================

// ---------------------------------------------------------------------------
// 3. Molecular Dynamics PhosphorImager GEL
// ---------------------------------------------------------------------------

/// Amersham Biosciences / Molecular Dynamics GEL format (`.gel`).
///
/// Ported from the upstream Java `GelReader`, which extends `BaseTiffReader`:
/// a GEL file is a TIFF carrying private Molecular Dynamics tags. The data
/// format tag (`MD_FILETAG` = 33445) is either LINEAR (128, plain TIFF) or
/// SQUARE_ROOT (2). For SQUARE_ROOT, the stored unsigned-short samples must be
/// squared and multiplied by the `MD_SCALE_PIXEL` (33446) rational scale, and
/// the pixel type becomes 32-bit float. Image count equals the number of IFDs
/// and is reported as the T dimension.
const MD_FILETAG: u16 = 33445;
const MD_SCALE_PIXEL: u16 = 33446;
const GEL_SQUARE_ROOT: u64 = 2;

pub struct GelReader {
    inner: crate::tiff::TiffReader,
    meta: Option<ImageMetadata>,
    /// True when the data format is SQUARE_ROOT (pixels need squaring/scaling).
    square_root: bool,
    /// MD_SCALE_PIXEL rational as f64 (defaults to 1.0).
    scale: f64,
    /// Java GelReader merges paired IFDs when more than one IFD exists. Logical
    /// plane `n` reads physical IFD `n * 2` in that layout.
    plane_ifds: Vec<u32>,
    plane_scales: Vec<f64>,
}

impl GelReader {
    pub fn new() -> Self {
        GelReader {
            inner: crate::tiff::TiffReader::new(),
            meta: None,
            square_root: false,
            scale: 1.0,
            plane_ifds: Vec::new(),
            plane_scales: Vec::new(),
        }
    }
}

fn gel_scale_from_ifd(ifd: &crate::tiff::ifd::Ifd) -> f64 {
    ifd.get(MD_SCALE_PIXEL)
        .and_then(|v| match v {
            crate::tiff::ifd::IfdValue::Rational(r) if !r.is_empty() => {
                let (num, den) = r[0];
                if den == 0 {
                    Some(1.0)
                } else {
                    Some(num as f64 / den as f64)
                }
            }
            _ => None,
        })
        .unwrap_or(1.0)
}

impl Default for GelReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for GelReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("gel"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // A genuine GEL is a TIFF whose first IFD contains MD_FILETAG, which we
        // cannot test from a raw header slice alone, so defer to set_id/by_name.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;

        let ifd_count = self.inner.ifd_count();
        if ifd_count == 0 || ifd_count > 2 {
            let _ = self.inner.close();
            self.meta = None;
            self.square_root = false;
            self.scale = 1.0;
            self.plane_ifds.clear();
            self.plane_scales.clear();
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "GEL TIFF must contain one or two IFDs, found {ifd_count}"
            )));
        }

        // Inspect the first IFD for the private Molecular Dynamics tags.
        let first = self
            .inner
            .ifd(0)
            .ok_or_else(|| BioFormatsError::UnsupportedFormat("GEL TIFF has no IFDs".into()))?;
        if first.get(MD_FILETAG).is_none() {
            // Not a Molecular Dynamics GEL TIFF.
            return Err(BioFormatsError::UnsupportedFormat(
                "GEL TIFF is missing the Molecular Dynamics MD_FILETAG (33445) tag".into(),
            ));
        }
        let fmt = first
            .get(MD_FILETAG)
            .and_then(|v| v.as_u64())
            .unwrap_or(128);
        self.square_root = fmt == GEL_SQUARE_ROOT;
        self.scale = gel_scale_from_ifd(first);

        // imageCount == number of IFDs; reported as the T dimension (Java
        // GelReader.initMetadata sets sizeT = imageCount, sizeZ/sizeC = 1).
        let mut ifds = 0u32;
        while self.inner.ifd(ifds as usize).is_some() {
            ifds += 1;
        }
        self.plane_ifds.clear();
        self.plane_scales.clear();
        if ifds > 1 {
            for logical in 0..(ifds / 2) {
                let physical = logical * 2;
                self.plane_ifds.push(physical);
                let scale = self
                    .inner
                    .ifd(physical as usize)
                    .map(gel_scale_from_ifd)
                    .unwrap_or(1.0);
                self.plane_scales.push(scale);
            }
        } else {
            self.plane_ifds.push(0);
            self.plane_scales.push(self.scale);
        }
        let ifds = self.plane_ifds.len().max(1) as u32;
        let base = self.inner.metadata();
        let mut meta = base.clone();
        meta.size_z = 1;
        meta.size_c = 1;
        meta.size_t = ifds;
        meta.image_count = ifds;
        meta.dimension_order = DimensionOrder::XYZCT;
        if self.square_root {
            meta.pixel_type = PixelType::Float32;
            meta.bits_per_pixel = 32;
        }
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta = None;
        self.square_root = false;
        self.scale = 1.0;
        self.plane_ifds.clear();
        self.plane_scales.clear();
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        1
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let little_endian = meta.is_little_endian;
        let physical_plane = self
            .plane_ifds
            .get(plane_index as usize)
            .copied()
            .unwrap_or(plane_index);

        if !self.square_root {
            // LINEAR: plain TIFF pixels.
            return self.inner.open_bytes(physical_plane);
        }

        // SQUARE_ROOT: the TIFF holds unsigned-short samples that must be
        // squared and multiplied by the scale, then emitted as 32-bit floats.
        // We read the raw shorts directly (the TIFF reports a 16-bit type for
        // these IFDs) rather than letting any float interpretation occur.
        let raw = self.inner.open_bytes(physical_plane)?;
        let n = raw.len() / 2;
        let mut out = vec![0u8; n * 4];
        let scale = self
            .plane_scales
            .get(plane_index as usize)
            .copied()
            .unwrap_or(self.scale);
        for i in 0..n {
            let value = if little_endian {
                u16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]])
            } else {
                u16::from_be_bytes([raw[i * 2], raw[i * 2 + 1]])
            } as u64;
            let pixel = (value * value) as f64 * scale;
            let bits = (pixel as f32).to_bits();
            let bytes = if little_endian {
                bits.to_le_bytes()
            } else {
                bits.to_be_bytes()
            };
            out[i * 4..i * 4 + 4].copy_from_slice(&bytes);
        }
        Ok(out)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("GEL", &full, &meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// 4. Imspector OBF STED microscopy
// ---------------------------------------------------------------------------

const IMSPECTOR_FILE_MAGIC: &[u8; 8] = b"OMAS_BF\n";
const IMSPECTOR_SYNTHETIC_STACK_MAGIC: &[u8; 14] = b"OMAS_BF_STACK\n";
const IMSPECTOR_MSR_MAGIC: &[u8; 10] = b"CDataStack";
const IMSPECTOR_MAGIC_NUMBER: u16 = 0xffff;
const IMSPECTOR_MAX_DIMS: usize = 15;
const IMSPECTOR_MIN_HEADER_LEN: usize = 14;
const IMSPECTOR_STACK_OFFSET_POS: usize = IMSPECTOR_MIN_HEADER_LEN;
const IMSPECTOR_SYNTHETIC_STACK_HEADER_LEN: usize = IMSPECTOR_SYNTHETIC_STACK_MAGIC.len() + 44;

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImspectorHeader {
    version: i32,
}

#[derive(Debug, Clone)]
struct ImspectorStack {
    meta: ImageMetadata,
    payload_offset: usize,
    plane_len: usize,
    decoded_payload: Option<Vec<u8>>,
}

fn parse_imspector_header(bytes: &[u8]) -> Result<ImspectorHeader> {
    if bytes.len() < IMSPECTOR_MIN_HEADER_LEN {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR header is truncated".to_string(),
        ));
    }
    if &bytes[..IMSPECTOR_FILE_MAGIC.len()] != IMSPECTOR_FILE_MAGIC {
        return Err(BioFormatsError::Format(
            "Not an Imspector OBF/MSR file".to_string(),
        ));
    }
    let magic_offset = IMSPECTOR_FILE_MAGIC.len();
    let magic = u16::from_le_bytes([bytes[magic_offset], bytes[magic_offset + 1]]);
    if magic != IMSPECTOR_MAGIC_NUMBER {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR header has invalid magic number".to_string(),
        ));
    }
    let version_offset = magic_offset + 2;
    let version = i32::from_le_bytes([
        bytes[version_offset],
        bytes[version_offset + 1],
        bytes[version_offset + 2],
        bytes[version_offset + 3],
    ]);
    Ok(ImspectorHeader { version })
}

fn imspector_is_java_msr(bytes: &[u8]) -> bool {
    bytes.get(..bytes.len().min(32)).is_some_and(|header| {
        header
            .windows(IMSPECTOR_MSR_MAGIC.len())
            .any(|w| w == IMSPECTOR_MSR_MAGIC)
    })
}

#[allow(dead_code)]
fn imspector_pixel_type(type_code: i32) -> Result<PixelType> {
    match type_code {
        0x01 => Ok(PixelType::Uint8),
        0x02 => Ok(PixelType::Int8),
        0x04 => Ok(PixelType::Uint16),
        0x08 => Ok(PixelType::Int16),
        0x10 => Ok(PixelType::Uint32),
        0x20 => Ok(PixelType::Int32),
        0x40 => Ok(PixelType::Float32),
        0x80 => Ok(PixelType::Float64),
        _ => Err(BioFormatsError::Format(format!(
            "Unsupported Imspector OBF/MSR data type {type_code}"
        ))),
    }
}

#[allow(dead_code)]
fn imspector_bits_per_pixel(type_code: i32) -> Result<u8> {
    Ok(match type_code {
        0x01 | 0x02 => 8,
        0x04 | 0x08 => 16,
        0x10 | 0x20 | 0x40 => 32,
        0x80 => 64,
        _ => {
            return Err(BioFormatsError::Format(format!(
                "Unsupported Imspector OBF/MSR data type {type_code}"
            )));
        }
    })
}

#[allow(dead_code)]
fn imspector_stack_length(length: i64) -> Result<u64> {
    if length >= 0 {
        Ok(length as u64)
    } else {
        Err(BioFormatsError::Format(
            "Negative Imspector OBF/MSR stack length on disk".to_string(),
        ))
    }
}

#[allow(dead_code)]
fn imspector_compression_flag(compression: i32) -> Result<bool> {
    match compression {
        0 => Ok(false),
        1 => Ok(true),
        _ => Err(BioFormatsError::Format(format!(
            "Unsupported Imspector OBF/MSR compression {compression}"
        ))),
    }
}

fn imspector_read_len_string(bytes: &[u8], offset: &mut usize) -> Result<String> {
    if bytes.len().saturating_sub(*offset) < 4 {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR length-prefixed string is truncated".to_string(),
        ));
    }
    let len = i32::from_le_bytes([
        bytes[*offset],
        bytes[*offset + 1],
        bytes[*offset + 2],
        bytes[*offset + 3],
    ]);
    *offset += 4;
    if len <= 0 {
        return Ok(String::new());
    }
    let len = len as usize;
    if bytes.len().saturating_sub(*offset) < len {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR length-prefixed string overruns input".to_string(),
        ));
    }
    let value = std::str::from_utf8(&bytes[*offset..*offset + len])
        .map_err(|e| {
            BioFormatsError::Format(format!("Imspector OBF/MSR string is not UTF-8: {e}"))
        })?
        .to_string();
    *offset += len;
    Ok(value)
}

/// Read a fixed-length string of `len` bytes at `offset` (the OBF stack `Name`
/// and `Description` are stored as plain byte runs whose length is given by a
/// separate header field). Mirrors `RandomAccessInputStream.readString(length)`;
/// decoded lossily because descriptions can carry non-UTF-8 bytes.
fn imspector_read_fixed_string(
    bytes: &[u8],
    offset: usize,
    len: i32,
    field: &str,
) -> Result<String> {
    if len <= 0 {
        return Ok(String::new());
    }
    let len = len as usize;
    if bytes.len().saturating_sub(offset) < len {
        return Err(BioFormatsError::Format(format!(
            "Imspector OBF/MSR {field} overruns input"
        )));
    }
    Ok(String::from_utf8_lossy(&bytes[offset..offset + len]).into_owned())
}

fn imspector_read_i32(bytes: &[u8], offset: &mut usize, field: &str) -> Result<i32> {
    if bytes.len().saturating_sub(*offset) < 4 {
        return Err(BioFormatsError::Format(format!(
            "Imspector OBF/MSR {field} is truncated"
        )));
    }
    let value = i32::from_le_bytes([
        bytes[*offset],
        bytes[*offset + 1],
        bytes[*offset + 2],
        bytes[*offset + 3],
    ]);
    *offset += 4;
    Ok(value)
}

fn imspector_read_i64(bytes: &[u8], offset: &mut usize, field: &str) -> Result<i64> {
    if bytes.len().saturating_sub(*offset) < 8 {
        return Err(BioFormatsError::Format(format!(
            "Imspector OBF/MSR {field} is truncated"
        )));
    }
    let value = i64::from_le_bytes([
        bytes[*offset],
        bytes[*offset + 1],
        bytes[*offset + 2],
        bytes[*offset + 3],
        bytes[*offset + 4],
        bytes[*offset + 5],
        bytes[*offset + 6],
        bytes[*offset + 7],
    ]);
    *offset += 8;
    Ok(value)
}

fn imspector_read_f64(bytes: &[u8], offset: &mut usize, field: &str) -> Result<f64> {
    if bytes.len().saturating_sub(*offset) < 8 {
        return Err(BioFormatsError::Format(format!(
            "Imspector OBF/MSR {field} is truncated"
        )));
    }
    let value = f64::from_le_bytes([
        bytes[*offset],
        bytes[*offset + 1],
        bytes[*offset + 2],
        bytes[*offset + 3],
        bytes[*offset + 4],
        bytes[*offset + 5],
        bytes[*offset + 6],
        bytes[*offset + 7],
    ]);
    *offset += 8;
    Ok(value)
}

fn imspector_positive_dim(value: i32, field: &str) -> Result<u32> {
    u32::try_from(value)
        .map_err(|_| {
            BioFormatsError::Format(format!(
                "Imspector OBF/MSR {field} must be positive, got {value}"
            ))
        })
        .and_then(|value| {
            if value == 0 {
                Err(BioFormatsError::Format(format!(
                    "Imspector OBF/MSR {field} must be positive"
                )))
            } else {
                Ok(value)
            }
        })
}

fn imspector_checked_plane_len(width: u32, height: u32, bytes_per_sample: usize) -> Result<usize> {
    (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(bytes_per_sample))
        .ok_or_else(|| BioFormatsError::Format("Imspector OBF/MSR plane size overflows".into()))
}

fn imspector_stack_offset(bytes: &[u8]) -> Result<Option<usize>> {
    if bytes.len() < IMSPECTOR_STACK_OFFSET_POS + 8 {
        return Ok(None);
    }
    let stack_offset = u64::from_le_bytes([
        bytes[IMSPECTOR_STACK_OFFSET_POS],
        bytes[IMSPECTOR_STACK_OFFSET_POS + 1],
        bytes[IMSPECTOR_STACK_OFFSET_POS + 2],
        bytes[IMSPECTOR_STACK_OFFSET_POS + 3],
        bytes[IMSPECTOR_STACK_OFFSET_POS + 4],
        bytes[IMSPECTOR_STACK_OFFSET_POS + 5],
        bytes[IMSPECTOR_STACK_OFFSET_POS + 6],
        bytes[IMSPECTOR_STACK_OFFSET_POS + 7],
    ]);
    if stack_offset == 0 {
        return Ok(None);
    }
    usize::try_from(stack_offset).map(Some).map_err(|_| {
        BioFormatsError::Format("Imspector OBF/MSR stack offset overflows usize".into())
    })
}

fn imspector_skip_len_bytes(bytes: &[u8], offset: &mut usize, len: i32, field: &str) -> Result<()> {
    let len = len.max(0) as usize;
    if bytes.len().saturating_sub(*offset) < len {
        return Err(BioFormatsError::Format(format!(
            "Imspector OBF/MSR {field} overruns input"
        )));
    }
    *offset += len;
    Ok(())
}

fn imspector_hex_preview(bytes: &[u8], limit: usize) -> String {
    bytes
        .iter()
        .take(limit)
        .map(|byte| format!("{byte:02x}"))
        .collect::<Vec<_>>()
        .join(" ")
}

fn imspector_ascii_preview(bytes: &[u8], limit: usize) -> String {
    bytes
        .iter()
        .take(limit)
        .map(|byte| {
            if byte.is_ascii_graphic() || *byte == b' ' {
                *byte as char
            } else {
                '.'
            }
        })
        .collect()
}

/// Faithful translation of the `Description` XML handling in
/// `OBFReader.initStack`. Java sanitizes the XML, renames the whitespace-laden
/// `<Time Lapse ...>` element to `<TimeLapse ...>`, then walks the DOM: the
/// document element's first child element is the root, each of its child
/// elements is `nodeName`, and each grandchild contributes
/// `addSeriesMeta(nodeName + " " + key, value)` — except `doc`/`hwr`
/// grandchildren, whose own children are added one level deeper.
///
/// Returns the extracted `(key, value)` pairs (empty when the string did not
/// parse as XML, in which case the caller stores the raw `Description`).
fn imspector_parse_description(description: &str) -> Vec<(String, String)> {
    use quick_xml::events::Event;

    // some XML node names contain white space, which prevents parsing
    let sanitized = description
        .replace("<Time Lapse ", "<TimeLapse ")
        .replace("</Time Lapse", "</TimeLapse");

    let mut reader = quick_xml::Reader::from_str(&sanitized);
    reader.config_mut().trim_text(false);

    // Element-depth stack of node names. Depth 1 = document element,
    // depth 2 = the chosen root, depth 3 = `nodeName`, depth 4 = grandchild
    // (`key`), depth 5 = doc/hwr children.
    let mut name_stack: Vec<String> = Vec::new();
    let mut text = String::new();
    let mut pairs: Vec<(String, String)> = Vec::new();
    let mut ok = true;

    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Err(_) => {
                ok = false;
                break;
            }
            Ok(Event::Start(ref e)) => {
                let local = String::from_utf8_lossy(e.local_name().as_ref()).into_owned();
                name_stack.push(local);
                text.clear();
            }
            Ok(Event::Text(t)) => {
                text.push_str(&crate::common::xml::decode_xml_text(&t).unwrap_or_default());
            }
            Ok(Event::GeneralRef(r)) => {
                text.push_str(&crate::common::xml::decode_xml_ref(&r).unwrap_or_default());
            }
            Ok(Event::End(_)) => {
                let depth = name_stack.len();
                // Depth 4: a grandchild whose parent (depth 3) is `nodeName`.
                // Java emits `nodeName + " " + key` unless key is doc/hwr.
                if depth == 4 {
                    let key = &name_stack[3];
                    let node_name = &name_stack[2];
                    if key != "doc" && key != "hwr" {
                        pairs.push((format!("{node_name} {key}"), text.trim().to_string()));
                    }
                } else if depth == 5 {
                    // Depth 5: child of a doc/hwr grandchild. Java keys these as
                    // `nodeName + " " + <this element's name>`.
                    let parent = &name_stack[3];
                    if parent == "doc" || parent == "hwr" {
                        let node_name = &name_stack[2];
                        let key = &name_stack[4];
                        pairs.push((format!("{node_name} {key}"), text.trim().to_string()));
                    }
                }
                text.clear();
                name_stack.pop();
            }
            _ => {}
        }
    }

    if !ok {
        pairs.clear();
    }
    pairs
}

fn imspector_msr_read_u8(bytes: &[u8], offset: &mut usize, field: &str) -> Result<u8> {
    if bytes.len().saturating_sub(*offset) < 1 {
        return Err(BioFormatsError::Format(format!(
            "Imspector MSR {field} is truncated"
        )));
    }
    let value = bytes[*offset];
    *offset += 1;
    Ok(value)
}

fn imspector_msr_read_u16(bytes: &[u8], offset: &mut usize, field: &str) -> Result<u16> {
    if bytes.len().saturating_sub(*offset) < 2 {
        return Err(BioFormatsError::Format(format!(
            "Imspector MSR {field} is truncated"
        )));
    }
    let value = u16::from_le_bytes([bytes[*offset], bytes[*offset + 1]]);
    *offset += 2;
    Ok(value)
}

fn imspector_msr_read_i32(bytes: &[u8], offset: &mut usize, field: &str) -> Result<i32> {
    if bytes.len().saturating_sub(*offset) < 4 {
        return Err(BioFormatsError::Format(format!(
            "Imspector MSR {field} is truncated"
        )));
    }
    let value = i32::from_le_bytes([
        bytes[*offset],
        bytes[*offset + 1],
        bytes[*offset + 2],
        bytes[*offset + 3],
    ]);
    *offset += 4;
    Ok(value)
}

fn imspector_msr_read_string(
    bytes: &[u8],
    offset: &mut usize,
    len: usize,
    field: &str,
) -> Result<String> {
    if bytes.len().saturating_sub(*offset) < len {
        return Err(BioFormatsError::Format(format!(
            "Imspector MSR {field} overruns input"
        )));
    }
    let value = String::from_utf8_lossy(&bytes[*offset..*offset + len]).into_owned();
    *offset += len;
    Ok(value)
}

fn imspector_msr_skip(bytes: &[u8], offset: &mut usize, len: usize, field: &str) -> Result<()> {
    if bytes.len().saturating_sub(*offset) < len {
        return Err(BioFormatsError::Format(format!(
            "Imspector MSR {field} overruns input"
        )));
    }
    *offset += len;
    Ok(())
}

fn imspector_msr_skip_tags(bytes: &[u8], offset: &mut usize, count: i32) -> Result<()> {
    let mut seen = 0i32;
    while seen < count {
        let len = imspector_msr_read_u8(bytes, offset, "tag length")?;
        if len == 0 {
            continue;
        }
        imspector_msr_skip(bytes, offset, len as usize, "tag")?;
        seen += 1;
    }
    Ok(())
}

fn parse_imspector_msr_stack(bytes: &[u8]) -> Result<Option<ImspectorStack>> {
    if !imspector_is_java_msr(bytes) {
        return Ok(None);
    }
    if bytes.len() < 32 {
        return Err(BioFormatsError::Format(
            "Imspector MSR header is truncated".into(),
        ));
    }

    let mut offset = 20usize;
    let length = imspector_msr_read_u16(bytes, &mut offset, "root tag length")? as usize;
    let root_tag = imspector_msr_read_string(bytes, &mut offset, length, "root tag")?;
    let count = imspector_msr_read_i32(bytes, &mut offset, "root tag count")?;
    if count < 0 {
        return Err(BioFormatsError::Format(
            "Imspector MSR root tag count is negative".into(),
        ));
    }
    imspector_msr_skip_tags(bytes, &mut offset, count)?;

    if offset % 2 == 1 {
        imspector_msr_skip(bytes, &mut offset, 1, "alignment byte")?;
    } else if offset < bytes.len() {
        let check = bytes[offset];
        if check == 0xff {
            offset += 1;
        }
    }

    let metadata_len = imspector_msr_read_u16(bytes, &mut offset, "metadata length")? as usize;
    let metadata = imspector_msr_read_string(bytes, &mut offset, metadata_len, "metadata")?;
    if metadata_len % 2 == 0 && offset < bytes.len() && bytes[offset] == 13 {
        offset += 1;
    }

    let mut check = imspector_msr_read_u16(bytes, &mut offset, "PMT marker search")?;
    while check != 3 && check != 2 {
        offset = offset.saturating_sub(1);
        check = imspector_msr_read_u16(bytes, &mut offset, "PMT marker search")?;
    }

    imspector_msr_skip(bytes, &mut offset, 26, "PMT header")?;
    let pmt_len = imspector_msr_read_u8(bytes, &mut offset, "PMT name length")? as usize;
    let pmt = imspector_msr_read_string(bytes, &mut offset, pmt_len, "PMT name")?;
    imspector_msr_skip(bytes, &mut offset, 6, "dimension header")?;
    let size_x = imspector_positive_dim(
        imspector_msr_read_i32(bytes, &mut offset, "size X")?,
        "size X",
    )?;
    let size_y = imspector_positive_dim(
        imspector_msr_read_i32(bytes, &mut offset, "size Y")?,
        "size Y",
    )?;
    let size_z = imspector_positive_dim(
        imspector_msr_read_i32(bytes, &mut offset, "size Z")?,
        "size Z",
    )?;
    let size_t = imspector_positive_dim(
        imspector_msr_read_i32(bytes, &mut offset, "size T")?,
        "size T",
    )?;
    let planes = size_z
        .checked_mul(size_t)
        .ok_or_else(|| BioFormatsError::Format("Imspector MSR image count overflows".into()))?;

    imspector_msr_skip(bytes, &mut offset, 16, "PMT settings header")?;
    let mut pmt_settings = Vec::new();
    for _ in 0..4 {
        let len = imspector_msr_read_u8(bytes, &mut offset, "PMT setting length")? as usize;
        pmt_settings.push(imspector_msr_read_string(
            bytes,
            &mut offset,
            len,
            "PMT setting",
        )?);
    }

    let payload_offset = offset;
    let plane_len =
        imspector_checked_plane_len(size_x, size_y, PixelType::Uint16.bytes_per_sample())?;
    let payload_len = plane_len
        .checked_mul(planes as usize)
        .ok_or_else(|| BioFormatsError::Format("Imspector MSR payload size overflows".into()))?;
    let payload_end = payload_offset
        .checked_add(payload_len)
        .ok_or_else(|| BioFormatsError::Format("Imspector MSR payload end overflows".into()))?;
    if payload_end > bytes.len() {
        return Err(BioFormatsError::Format(
            "Imspector MSR pixel data overruns input".into(),
        ));
    }

    let mut meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c: 1,
        size_t,
        pixel_type: PixelType::Uint16,
        bits_per_pixel: 16,
        image_count: planes,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        ..ImageMetadata::default()
    };
    meta.series_metadata.insert(
        "imspector_version_subset".into(),
        MetadataValue::String("java-msr-cdatastack-first-block".into()),
    );
    meta.series_metadata.insert(
        "imspector_msr_root_tag".into(),
        MetadataValue::String(root_tag),
    );
    if !pmt.is_empty() {
        meta.series_metadata
            .insert("imspector_msr_pmt".into(), MetadataValue::String(pmt));
    }
    for (i, value) in pmt_settings.into_iter().enumerate() {
        if !value.is_empty() {
            meta.series_metadata.insert(
                format!("imspector_msr_pmt_setting_{i}"),
                MetadataValue::String(value),
            );
        }
    }
    let values: Vec<&str> = metadata.split("::").collect();
    for pair in values.chunks_exact(2) {
        meta.series_metadata.insert(
            pair[0].to_string(),
            MetadataValue::String(pair[1].to_string()),
        );
    }

    Ok(Some(ImspectorStack {
        meta,
        payload_offset,
        plane_len,
        decoded_payload: None,
    }))
}

fn parse_imspector_native_stack(
    bytes: &[u8],
    stack_offset: usize,
) -> Result<Option<(ImspectorStack, u64)>> {
    if bytes.len().saturating_sub(stack_offset) < IMSPECTOR_SYNTHETIC_STACK_MAGIC.len() + 6 {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR native stack header is truncated".to_string(),
        ));
    }
    if &bytes[stack_offset..stack_offset + IMSPECTOR_SYNTHETIC_STACK_MAGIC.len()]
        != IMSPECTOR_SYNTHETIC_STACK_MAGIC
    {
        return Ok(None);
    }

    let magic_offset = stack_offset + IMSPECTOR_SYNTHETIC_STACK_MAGIC.len();
    let magic = u16::from_le_bytes([bytes[magic_offset], bytes[magic_offset + 1]]);
    if magic != IMSPECTOR_MAGIC_NUMBER {
        return Ok(None);
    }

    let mut offset = magic_offset + 2;
    let stack_version = imspector_read_i32(bytes, &mut offset, "native stack version")?;
    if !(1..=6).contains(&stack_version) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imspector OBF/MSR native stack version {stack_version} is unsupported by the bounded v1-v6 reader"
        )));
    }
    let num_dims = imspector_read_i32(bytes, &mut offset, "dimension count")?;
    if !(1..=5).contains(&num_dims) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imspector OBF/MSR native stack dimension count {num_dims} is unsupported"
        )));
    }
    let num_dims = num_dims as usize;

    let mut sizes = [1i32; IMSPECTOR_MAX_DIMS];
    let mut sample_count = 1usize;
    for (d, slot) in sizes.iter_mut().enumerate() {
        let size = imspector_read_i32(bytes, &mut offset, "dimension size")?;
        if d < num_dims {
            let positive = imspector_positive_dim(size, "dimension size")? as usize;
            sample_count = sample_count.checked_mul(positive).ok_or_else(|| {
                BioFormatsError::Format("Imspector OBF/MSR sample count overflows".into())
            })?;
            *slot = size;
        }
    }
    // Faithful translation of OBFReader.initStack: read the 15 physical
    // `Lengths` doubles (keeping the first numberOfDimensions) then the 15
    // `Offsets` doubles. Java stores both arrays in the series metadata.
    let mut lengths = Vec::with_capacity(num_dims);
    for d in 0..IMSPECTOR_MAX_DIMS {
        let length = imspector_read_f64(bytes, &mut offset, "dimension length")?;
        if d < num_dims {
            lengths.push(length);
        }
    }
    let mut offsets = Vec::with_capacity(num_dims);
    for d in 0..IMSPECTOR_MAX_DIMS {
        let dim_offset = imspector_read_f64(bytes, &mut offset, "dimension offset")?;
        if d < num_dims {
            offsets.push(dim_offset);
        }
    }
    if offset > bytes.len() {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR native dimension metadata is truncated".to_string(),
        ));
    }

    let type_code = imspector_read_i32(bytes, &mut offset, "data type")?;
    let pixel_type = imspector_pixel_type(type_code)?;
    let bits_per_pixel = imspector_bits_per_pixel(type_code)?;
    let compression = imspector_read_i32(bytes, &mut offset, "compression")?;
    let compressed = imspector_compression_flag(compression)?;
    offset = offset.checked_add(4).ok_or_else(|| {
        BioFormatsError::Format("Imspector OBF/MSR native header offset overflows".into())
    })?;
    let name_len = imspector_read_i32(bytes, &mut offset, "name length")?;
    let description_len = imspector_read_i32(bytes, &mut offset, "description length")?;
    offset = offset.checked_add(8).ok_or_else(|| {
        BioFormatsError::Format("Imspector OBF/MSR native header offset overflows".into())
    })?;
    if offset > bytes.len() {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR native stack header is truncated".to_string(),
        ));
    }
    let payload_len =
        imspector_stack_length(imspector_read_i64(bytes, &mut offset, "payload length")?)?;
    let next_stack = imspector_read_i64(bytes, &mut offset, "next stack offset")?;
    let next_stack_offset = if next_stack < 0 {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR negative next stack offset on disk".into(),
        ));
    } else {
        next_stack as u64
    };
    // OBFReader.initStack reads the stack `Name` and stores it under the
    // "Name" series-metadata key, then parses the `Description` (see below).
    let name_offset = offset;
    let name = imspector_read_fixed_string(bytes, name_offset, name_len, "stack name")?;
    imspector_skip_len_bytes(bytes, &mut offset, name_len, "stack name")?;
    let description_offset = offset;
    let description = imspector_read_fixed_string(
        bytes,
        description_offset,
        description_len,
        "stack description",
    )?;
    imspector_skip_len_bytes(bytes, &mut offset, description_len, "stack description")?;

    let payload_offset = offset;
    let payload_len = usize::try_from(payload_len).map_err(|_| {
        BioFormatsError::Format("Imspector OBF/MSR payload length overflows usize".into())
    })?;
    let payload_end = payload_offset.checked_add(payload_len).ok_or_else(|| {
        BioFormatsError::Format("Imspector OBF/MSR payload end offset overflows".into())
    })?;
    if payload_end > bytes.len() {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR native payload overruns input".to_string(),
        ));
    }

    let expected_len = sample_count
        .checked_mul(pixel_type.bytes_per_sample())
        .ok_or_else(|| {
            BioFormatsError::Format("Imspector OBF/MSR native payload size overflows".into())
        })?;
    let mut footer_offset = payload_end;
    let footer_skip = imspector_read_i32(bytes, &mut footer_offset, "native footer offset")?;
    let mut steps_present = [false; IMSPECTOR_MAX_DIMS];
    for (d, slot) in steps_present.iter_mut().enumerate() {
        let present = imspector_read_i32(bytes, &mut footer_offset, "step presence")? != 0;
        if d < num_dims {
            *slot = present;
        }
    }
    let mut step_labels_present = [false; IMSPECTOR_MAX_DIMS];
    for (d, slot) in step_labels_present.iter_mut().enumerate() {
        let present = imspector_read_i32(bytes, &mut footer_offset, "step-label presence")? != 0;
        if d < num_dims {
            *slot = present;
        }
    }
    let mut obsolete_metadata_length = 0usize;
    let mut num_flush_points = 0usize;
    let mut flush_block_size = 0i64;
    if stack_version >= 3 {
        obsolete_metadata_length = usize::try_from(imspector_stack_length(imspector_read_i32(
            bytes,
            &mut footer_offset,
            "obsolete metadata length",
        )? as i64)?)
        .map_err(|_| {
            BioFormatsError::Format(
                "Imspector OBF/MSR obsolete metadata length overflows usize".into(),
            )
        })?;
        footer_offset = footer_offset
            .checked_add(80 * (IMSPECTOR_MAX_DIMS + 1))
            .ok_or_else(|| {
                BioFormatsError::Format("Imspector OBF/MSR native footer offset overflows".into())
            })?;
        if footer_offset > bytes.len() {
            return Err(BioFormatsError::Format(
                "Imspector OBF/MSR native footer is truncated".to_string(),
            ));
        }
        num_flush_points = usize::try_from(imspector_stack_length(imspector_read_i64(
            bytes,
            &mut footer_offset,
            "flush point count",
        )?)?)
        .map_err(|_| {
            BioFormatsError::Format("Imspector OBF/MSR flush point count overflows usize".into())
        })?;
        flush_block_size = imspector_read_i64(bytes, &mut footer_offset, "flush block size")?;
    }

    let mut tag_dictionary_length = 0usize;
    let mut stack_end_disk = None;
    let mut minimum_format_version = None;
    if stack_version >= 4 {
        tag_dictionary_length = usize::try_from(imspector_stack_length(imspector_read_i64(
            bytes,
            &mut footer_offset,
            "tag dictionary length",
        )?)?)
        .map_err(|_| {
            BioFormatsError::Format(
                "Imspector OBF/MSR tag dictionary length overflows usize".into(),
            )
        })?;
        stack_end_disk = Some(imspector_read_i64(bytes, &mut footer_offset, "stack end")?);
        minimum_format_version = Some(imspector_read_i32(
            bytes,
            &mut footer_offset,
            "minimum format version",
        )?);
    }

    let mut samples_written = sample_count;
    let mut num_chunk_positions = 0usize;
    let mut stack_end_used_disk = None;
    if stack_version >= 6 {
        stack_end_used_disk = Some(imspector_read_i64(
            bytes,
            &mut footer_offset,
            "stack end used",
        )?);
        samples_written = usize::try_from(imspector_stack_length(imspector_read_i64(
            bytes,
            &mut footer_offset,
            "samples written",
        )?)?)
        .map_err(|_| {
            BioFormatsError::Format("Imspector OBF/MSR samples written overflows usize".into())
        })?;
        num_chunk_positions = usize::try_from(imspector_stack_length(imspector_read_i64(
            bytes,
            &mut footer_offset,
            "chunk position count",
        )?)?)
        .map_err(|_| {
            BioFormatsError::Format("Imspector OBF/MSR chunk position count overflows usize".into())
        })?;
    }

    let mut label_offset = payload_end
        .checked_add(footer_skip.max(0) as usize)
        .ok_or_else(|| {
            BioFormatsError::Format("Imspector OBF/MSR native footer offset overflows".into())
        })?;
    let mut dimension_labels = Vec::new();
    for d in 0..num_dims {
        let label = imspector_read_len_string(bytes, &mut label_offset)?;
        if !label.is_empty() {
            dimension_labels.push(format!("{d}:{label}"));
        }
    }
    let mut step_axes = Vec::new();
    let mut step_previews = Vec::new();
    for d in 0..num_dims {
        if steps_present[d] {
            step_axes.push(d.to_string());
            let count = sizes[d] as usize;
            let mut values = Vec::new();
            for i in 0..count {
                let value = imspector_read_f64(bytes, &mut label_offset, "step table value")?;
                if i < 8 {
                    values.push(value.to_string());
                }
            }
            step_previews.push(format!("{d}:{}", values.join(",")));
        }
    }
    let mut step_label_axes = Vec::new();
    let mut step_label_previews = Vec::new();
    for d in 0..num_dims {
        if step_labels_present[d] {
            step_label_axes.push(d.to_string());
            let mut labels = Vec::new();
            for _ in 0..sizes[d] {
                let label = imspector_read_len_string(bytes, &mut label_offset)?;
                if labels.len() < 8 && !label.is_empty() {
                    labels.push(label);
                }
            }
            if !labels.is_empty() {
                step_label_previews.push(format!("{d}:{}", labels.join(",")));
            }
        }
    }
    label_offset = label_offset
        .checked_add(obsolete_metadata_length)
        .ok_or_else(|| {
            BioFormatsError::Format("Imspector OBF/MSR native footer offset overflows".into())
        })?;
    let mut flush_point_previews = Vec::new();
    for i in 0..num_flush_points {
        let point = imspector_read_i64(bytes, &mut label_offset, "flush point")?;
        if i < 8 {
            flush_point_previews.push(point.to_string());
        }
    }
    let tag_dictionary_offset = label_offset;
    let tag_dictionary_end = tag_dictionary_offset
        .checked_add(tag_dictionary_length)
        .ok_or_else(|| {
            BioFormatsError::Format("Imspector OBF/MSR native footer offset overflows".into())
        })?;
    if tag_dictionary_end > bytes.len() {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR native footer is truncated".to_string(),
        ));
    }
    let tag_dictionary_preview = if tag_dictionary_length > 0 {
        Some((
            imspector_ascii_preview(&bytes[tag_dictionary_offset..tag_dictionary_end], 128),
            imspector_hex_preview(&bytes[tag_dictionary_offset..tag_dictionary_end], 32),
        ))
    } else {
        None
    };
    label_offset = label_offset
        .checked_add(tag_dictionary_length)
        .ok_or_else(|| {
            BioFormatsError::Format("Imspector OBF/MSR native footer offset overflows".into())
        })?;
    if label_offset > bytes.len() {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR native footer is truncated".to_string(),
        ));
    }

    let decoded_payload = if num_chunk_positions > 0 {
        if !compressed {
            let expected_written_len = samples_written
                .checked_mul(pixel_type.bytes_per_sample())
                .ok_or_else(|| {
                BioFormatsError::Format("Imspector OBF/MSR native payload size overflows".into())
            })?;
            if expected_written_len != expected_len {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Imspector OBF/MSR chunked native stack declares {samples_written} written samples but {sample_count} logical samples are required"
                )));
            }
        }
        let mut logical_positions = Vec::with_capacity(num_chunk_positions + 1);
        let mut file_positions = Vec::with_capacity(num_chunk_positions + 1);
        logical_positions.push(0usize);
        file_positions.push(0usize);
        for _ in 0..num_chunk_positions {
            logical_positions.push(
                usize::try_from(imspector_stack_length(imspector_read_i64(
                    bytes,
                    &mut label_offset,
                    "chunk logical position",
                )?)?)
                .map_err(|_| {
                    BioFormatsError::Format(
                        "Imspector OBF/MSR chunk logical position overflows usize".into(),
                    )
                })?,
            );
            file_positions.push(
                usize::try_from(imspector_stack_length(imspector_read_i64(
                    bytes,
                    &mut label_offset,
                    "chunk file position",
                )?)?)
                .map_err(|_| {
                    BioFormatsError::Format(
                        "Imspector OBF/MSR chunk file position overflows usize".into(),
                    )
                })?,
            );
        }
        let assembled_len = if compressed {
            let last_logical = logical_positions.last().copied().unwrap_or(0);
            let last_file = file_positions.last().copied().unwrap_or(0);
            let last_file_len = payload_len.checked_sub(last_file).ok_or_else(|| {
                BioFormatsError::Format("Imspector OBF/MSR chunk file positions are invalid".into())
            })?;
            last_logical.checked_add(last_file_len).ok_or_else(|| {
                BioFormatsError::Format("Imspector OBF/MSR compressed chunk size overflows".into())
            })?
        } else {
            expected_len
        };
        let mut assembled = vec![0u8; assembled_len];
        for idx in 0..logical_positions.len() {
            let logical_start = logical_positions[idx];
            let logical_end = logical_positions
                .get(idx + 1)
                .copied()
                .unwrap_or(assembled_len);
            if logical_start > logical_end || logical_end > assembled_len {
                return Err(BioFormatsError::Format(
                    "Imspector OBF/MSR chunk logical positions are invalid".into(),
                ));
            }
            let file_start = payload_offset
                .checked_add(file_positions[idx])
                .ok_or_else(|| {
                    BioFormatsError::Format("Imspector OBF/MSR chunk file offset overflows".into())
                })?;
            let chunk_len = logical_end - logical_start;
            let file_end = file_start.checked_add(chunk_len).ok_or_else(|| {
                BioFormatsError::Format("Imspector OBF/MSR chunk file end overflows".into())
            })?;
            if file_end > payload_end {
                return Err(BioFormatsError::Format(
                    "Imspector OBF/MSR chunk payload overruns native data block".into(),
                ));
            }
            assembled[logical_start..logical_end].copy_from_slice(&bytes[file_start..file_end]);
        }
        if compressed {
            let decoded = crate::common::codec::decompress_deflate(&assembled).map_err(|e| {
                BioFormatsError::Codec(format!(
                    "Imspector OBF/MSR compressed chunked native stack payload could not be decompressed: {e}"
                ))
            })?;
            if decoded.len() != expected_len {
                return Err(BioFormatsError::Format(format!(
                    "Imspector OBF/MSR native chunked decompressed payload length {} does not match declared stack size {expected_len}",
                    decoded.len()
                )));
            }
            Some(decoded)
        } else {
            Some(assembled)
        }
    } else if compressed {
        let decoded = crate::common::codec::decompress_deflate(&bytes[payload_offset..payload_end])
            .map_err(|e| {
                BioFormatsError::Codec(format!(
                    "Imspector OBF/MSR compressed native stack payload could not be decompressed: {e}"
                ))
            })?;
        if decoded.len() != expected_len {
            return Err(BioFormatsError::Format(format!(
                "Imspector OBF/MSR native decompressed payload length {} does not match declared stack size {expected_len}",
                decoded.len()
            )));
        }
        Some(decoded)
    } else if payload_len != expected_len {
        return Err(BioFormatsError::Format(format!(
            "Imspector OBF/MSR native payload length {payload_len} does not match declared stack size {expected_len}"
        )));
    } else {
        None
    };

    let size_x = sizes[0] as u32;
    let size_y = if num_dims > 1 { sizes[1] as u32 } else { 1 };
    let size_z = if num_dims > 2 { sizes[2] as u32 } else { 1 };
    let size_c = if num_dims > 3 { sizes[3] as u32 } else { 1 };
    let size_t = if num_dims > 4 { sizes[4] as u32 } else { 1 };
    let image_count = size_z
        .checked_mul(size_c)
        .and_then(|n| n.checked_mul(size_t))
        .ok_or_else(|| BioFormatsError::Format("Imspector OBF/MSR image count overflows".into()))?;
    let plane_len = imspector_checked_plane_len(size_x, size_y, pixel_type.bytes_per_sample())?;

    let mut meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel,
        image_count,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        ..ImageMetadata::default()
    };
    meta.series_metadata.insert(
        "imspector_version_subset".into(),
        MetadataValue::String(if num_chunk_positions > 0 {
            if compressed {
                format!("native-v{stack_version}-zlib-chunked")
            } else {
                format!("native-v{stack_version}-uncompressed-chunked")
            }
        } else if compressed {
            format!("native-v{stack_version}-zlib-contiguous")
        } else {
            format!("native-v{stack_version}-uncompressed-contiguous")
        }),
    );

    // OBFReader.initStack: addGlobalMeta("Stack version", stackVersion).
    meta.series_metadata.insert(
        "Stack version".into(),
        MetadataValue::Int(stack_version as i64),
    );
    // OBFReader.initStack stores the per-dimension physical `Lengths` and
    // `Offsets` (kept to numberOfDimensions) in the series metadata.
    meta.series_metadata.insert(
        "Lengths".into(),
        MetadataValue::String(
            lengths
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        ),
    );
    meta.series_metadata.insert(
        "Offsets".into(),
        MetadataValue::String(
            offsets
                .iter()
                .map(|v| v.to_string())
                .collect::<Vec<_>>()
                .join(", "),
        ),
    );
    if !name.is_empty() {
        meta.series_metadata
            .insert("Name".into(), MetadataValue::String(name));
    }

    // OBFReader.initFile derives PhysicalSize{X,Y,Z} from the first three
    // Lengths divided by the corresponding dimension size; values below 0.01
    // are treated as metres and scaled to micrometres. We have no dedicated
    // physical-size field, so capture the derived micrometre values as series
    // metadata, faithfully matching Java's scaling.
    let physical_dims: [(usize, u32, &str); 3] = [
        (0, size_x, "PhysicalSizeX"),
        (1, size_y, "PhysicalSizeY"),
        (2, size_z, "PhysicalSizeZ"),
    ];
    for (idx, dim_size, key) in physical_dims {
        if let Some(length) = lengths.get(idx) {
            let mut length = length.abs();
            if length < 0.01 {
                length *= 1_000_000.0;
            }
            if length > 0.0 && dim_size > 0 {
                meta.series_metadata
                    .insert(key.into(), MetadataValue::Float(length / dim_size as f64));
            }
        }
    }

    // OBFReader.initStack parses the stack `Description` as XML, extracting
    // `<Time Lapse>`/`<TimeLapse>` child metadata; non-XML descriptions are
    // kept verbatim under "Description".
    if !description.is_empty() {
        let pairs = imspector_parse_description(&description);
        if pairs.is_empty() {
            meta.series_metadata
                .insert("Description".into(), MetadataValue::String(description));
        } else {
            for (key, value) in pairs {
                meta.series_metadata
                    .insert(key, MetadataValue::String(value));
            }
        }
    }

    if !dimension_labels.is_empty() {
        meta.series_metadata.insert(
            "imspector_dimension_labels".into(),
            MetadataValue::String(dimension_labels.join(";")),
        );
    }
    if !step_axes.is_empty() {
        meta.series_metadata.insert(
            "imspector_step_table_axes".into(),
            MetadataValue::String(step_axes.join(",")),
        );
        meta.series_metadata.insert(
            "imspector_step_table_previews".into(),
            MetadataValue::String(step_previews.join(";")),
        );
    }
    if !step_label_axes.is_empty() {
        meta.series_metadata.insert(
            "imspector_step_label_axes".into(),
            MetadataValue::String(step_label_axes.join(",")),
        );
        if !step_label_previews.is_empty() {
            meta.series_metadata.insert(
                "imspector_step_label_previews".into(),
                MetadataValue::String(step_label_previews.join(";")),
            );
        }
    }
    if stack_version >= 3 {
        meta.series_metadata.insert(
            "imspector_flush_point_count".into(),
            MetadataValue::Int(num_flush_points as i64),
        );
        meta.series_metadata.insert(
            "imspector_flush_block_size".into(),
            MetadataValue::Int(flush_block_size),
        );
        if !flush_point_previews.is_empty() {
            meta.series_metadata.insert(
                "imspector_flush_point_previews".into(),
                MetadataValue::String(flush_point_previews.join(",")),
            );
        }
    }
    if stack_version >= 4 {
        meta.series_metadata.insert(
            "imspector_tag_dictionary_length".into(),
            MetadataValue::Int(tag_dictionary_length as i64),
        );
        if let Some((ascii_preview, hex_preview)) = tag_dictionary_preview {
            meta.series_metadata.insert(
                "imspector_tag_dictionary_offset".into(),
                MetadataValue::Int(tag_dictionary_offset as i64),
            );
            meta.series_metadata.insert(
                "imspector_tag_dictionary_ascii_preview".into(),
                MetadataValue::String(ascii_preview),
            );
            meta.series_metadata.insert(
                "imspector_tag_dictionary_hex_preview".into(),
                MetadataValue::String(hex_preview),
            );
        }
        if let Some(value) = stack_end_disk {
            meta.series_metadata
                .insert("imspector_stack_end".into(), MetadataValue::Int(value));
        }
        if let Some(value) = minimum_format_version {
            meta.series_metadata.insert(
                "imspector_minimum_format_version".into(),
                MetadataValue::Int(value as i64),
            );
        }
    }
    if stack_version >= 6 {
        if let Some(value) = stack_end_used_disk {
            meta.series_metadata
                .insert("imspector_stack_end_used".into(), MetadataValue::Int(value));
        }
        meta.series_metadata.insert(
            "imspector_samples_written".into(),
            MetadataValue::Int(samples_written as i64),
        );
        meta.series_metadata.insert(
            "imspector_chunk_position_count".into(),
            MetadataValue::Int(num_chunk_positions as i64),
        );
    }

    Ok(Some((
        ImspectorStack {
            meta,
            payload_offset: if compressed || num_chunk_positions > 0 {
                0
            } else {
                payload_offset
            },
            plane_len,
            decoded_payload,
        },
        next_stack_offset,
    )))
}

fn parse_imspector_synthetic_stack(bytes: &[u8]) -> Result<Option<ImspectorStack>> {
    let Some(stack_offset) = imspector_stack_offset(bytes)? else {
        return Ok(None);
    };
    if bytes.len().saturating_sub(stack_offset) < IMSPECTOR_SYNTHETIC_STACK_HEADER_LEN {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR stack header is truncated".to_string(),
        ));
    }
    if &bytes[stack_offset..stack_offset + IMSPECTOR_SYNTHETIC_STACK_MAGIC.len()]
        != IMSPECTOR_SYNTHETIC_STACK_MAGIC
    {
        return Ok(None);
    }

    let mut offset = stack_offset + IMSPECTOR_SYNTHETIC_STACK_MAGIC.len();
    let width = imspector_positive_dim(imspector_read_i32(bytes, &mut offset, "width")?, "width")?;
    let height =
        imspector_positive_dim(imspector_read_i32(bytes, &mut offset, "height")?, "height")?;
    let size_z =
        imspector_positive_dim(imspector_read_i32(bytes, &mut offset, "size Z")?, "size Z")?;
    let size_c =
        imspector_positive_dim(imspector_read_i32(bytes, &mut offset, "size C")?, "size C")?;
    let size_t =
        imspector_positive_dim(imspector_read_i32(bytes, &mut offset, "size T")?, "size T")?;
    let type_code = imspector_read_i32(bytes, &mut offset, "data type")?;
    let compression = imspector_read_i32(bytes, &mut offset, "compression")?;
    let compressed = imspector_compression_flag(compression)?;
    let payload_offset =
        imspector_stack_length(imspector_read_i64(bytes, &mut offset, "payload offset")?)?;
    let payload_len =
        imspector_stack_length(imspector_read_i64(bytes, &mut offset, "payload length")?)?;
    let payload_offset = usize::try_from(payload_offset).map_err(|_| {
        BioFormatsError::Format("Imspector OBF/MSR payload offset overflows usize".into())
    })?;
    let payload_len = usize::try_from(payload_len).map_err(|_| {
        BioFormatsError::Format("Imspector OBF/MSR payload length overflows usize".into())
    })?;
    if payload_offset < offset {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR payload overlaps stack header".to_string(),
        ));
    }
    let payload_end = payload_offset.checked_add(payload_len).ok_or_else(|| {
        BioFormatsError::Format("Imspector OBF/MSR payload end offset overflows".into())
    })?;
    if payload_end > bytes.len() {
        return Err(BioFormatsError::Format(
            "Imspector OBF/MSR payload overruns input".to_string(),
        ));
    }

    let pixel_type = imspector_pixel_type(type_code)?;
    let bits_per_pixel = imspector_bits_per_pixel(type_code)?;
    let plane_len = imspector_checked_plane_len(width, height, pixel_type.bytes_per_sample())?;
    let image_count = size_z
        .checked_mul(size_c)
        .and_then(|n| n.checked_mul(size_t))
        .ok_or_else(|| BioFormatsError::Format("Imspector OBF/MSR image count overflows".into()))?;
    let expected_len = plane_len.checked_mul(image_count as usize).ok_or_else(|| {
        BioFormatsError::Format("Imspector OBF/MSR stack payload size overflows".into())
    })?;
    let decoded_payload = if compressed {
        let decoded = crate::common::codec::decompress_deflate(&bytes[payload_offset..payload_end])
            .map_err(|e| {
                BioFormatsError::Codec(format!(
                    "Imspector OBF/MSR compressed synthetic stack payload could not be decompressed: {e}"
                ))
            })?;
        if decoded.len() != expected_len {
            return Err(BioFormatsError::Format(format!(
                "Imspector OBF/MSR decompressed payload length {} does not match declared stack size {expected_len}",
                decoded.len()
            )));
        }
        Some(decoded)
    } else if payload_len != expected_len {
        return Err(BioFormatsError::Format(format!(
            "Imspector OBF/MSR payload length {payload_len} does not match declared stack size {expected_len}"
        )));
    } else {
        None
    };

    let mut meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel,
        image_count,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        ..ImageMetadata::default()
    };
    meta.series_metadata.insert(
        "imspector_version_subset".into(),
        MetadataValue::String(if compressed {
            "synthetic-zlib-raw".into()
        } else {
            "synthetic-uncompressed-raw".into()
        }),
    );

    Ok(Some(ImspectorStack {
        meta,
        payload_offset: if compressed { 0 } else { payload_offset },
        plane_len,
        decoded_payload,
    }))
}

/// Imspector OBF/MSR STED microscopy format (`.obf`, `.msr`).
///
/// Header parsing is translated from Bio-Formats' `OBFReader`. Only a strict,
/// raw subset with an explicit stack marker is decoded; unknown
/// stack layouts are still intentionally rejected instead of guessed.
pub struct ImspectorReader {
    path: Option<PathBuf>,
    bytes: Vec<u8>,
    stacks: Vec<ImspectorStack>,
    current_series: usize,
}

impl ImspectorReader {
    pub fn new() -> Self {
        ImspectorReader {
            path: None,
            bytes: Vec::new(),
            stacks: Vec::new(),
            current_series: 0,
        }
    }
}

impl Default for ImspectorReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ImspectorReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("obf") | Some("msr"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        parse_imspector_header(header).is_ok() || imspector_is_java_msr(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.bytes.clear();
        self.stacks.clear();
        self.current_series = 0;
        let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let header = match parse_imspector_header(&bytes) {
            Ok(header) => Some(header),
            Err(obf_error) => {
                if let Some(stack) = parse_imspector_msr_stack(&bytes)? {
                    self.path = Some(path.to_path_buf());
                    self.bytes = bytes;
                    self.stacks = vec![stack];
                    return Ok(());
                }
                return Err(obf_error);
            }
        };
        // Faithful translation of OBFReader.initFile's do/while stack loop:
        // walk the linked list of native stacks starting at the file-header
        // stack pointer, adding one series per stack until next == 0.
        if let Some(first_offset) = imspector_stack_offset(&bytes)? {
            let mut stack_offset = first_offset;
            let mut stacks = Vec::new();
            loop {
                match parse_imspector_native_stack(&bytes, stack_offset)? {
                    Some((stack, next_stack)) => {
                        stacks.push(stack);
                        if next_stack == 0 {
                            break;
                        }
                        let next = usize::try_from(next_stack).map_err(|_| {
                            BioFormatsError::Format(
                                "Imspector OBF/MSR next stack offset overflows usize".into(),
                            )
                        })?;
                        if next <= stack_offset || next >= bytes.len() {
                            return Err(BioFormatsError::Format(
                                "Imspector OBF/MSR next stack offset is out of range".into(),
                            ));
                        }
                        stack_offset = next;
                    }
                    None => break,
                }
            }
            if !stacks.is_empty() {
                self.path = Some(path.to_path_buf());
                self.bytes = bytes;
                self.stacks = stacks;
                return Ok(());
            }
        }
        if let Some(stack) = parse_imspector_synthetic_stack(&bytes)? {
            self.path = Some(path.to_path_buf());
            self.bytes = bytes;
            self.stacks = vec![stack];
            return Ok(());
        }
        let mut detail = format!(
            "Imspector OBF/MSR native stack decoding is unsupported except v1-v6 single-stack contiguous raw/zlib and uncompressed or zlib-compressed chunked stacks or explicit BFIMSPECTOR_RAW_STACK_V1 data (version {})",
            header.expect("OBF header must exist here").version
        );
        if bytes.len() > IMSPECTOR_MIN_HEADER_LEN + 12 {
            let mut offset = IMSPECTOR_MIN_HEADER_LEN + 8;
            if let Ok(description) = imspector_read_len_string(&bytes, &mut offset) {
                if !description.is_empty() {
                    detail.push_str("; header description parsed");
                }
            }
        }
        Err(BioFormatsError::UnsupportedFormat(detail))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.bytes.clear();
        self.stacks.clear();
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.stacks.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.stacks.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        match self.stacks.get(self.current_series) {
            Some(stack) => &stack.meta,
            None => crate::common::reader::uninitialized_metadata(),
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let stack = self
            .stacks
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= stack.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let rel = stack
            .plane_len
            .checked_mul(plane_index as usize)
            .ok_or_else(|| {
                BioFormatsError::Format("Imspector OBF/MSR plane offset overflows".into())
            })?;
        let start = stack.payload_offset.checked_add(rel).ok_or_else(|| {
            BioFormatsError::Format("Imspector OBF/MSR plane start offset overflows".into())
        })?;
        let end = start.checked_add(stack.plane_len).ok_or_else(|| {
            BioFormatsError::Format("Imspector OBF/MSR plane end offset overflows".into())
        })?;
        let source = stack.decoded_payload.as_deref().unwrap_or(&self.bytes);
        if end > source.len() {
            return Err(BioFormatsError::Format(
                "Imspector OBF/MSR plane overruns pixel data".into(),
            ));
        }
        Ok(source[start..end].to_vec())
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = &self
            .stacks
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?
            .meta;
        crop_full_plane("Imspector OBF/MSR", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod imspector_tests {
    use super::{
        imspector_bits_per_pixel, imspector_compression_flag, imspector_pixel_type,
        imspector_read_len_string, imspector_stack_length, parse_imspector_header, ImspectorReader,
        IMSPECTOR_FILE_MAGIC, IMSPECTOR_MAGIC_NUMBER, IMSPECTOR_MIN_HEADER_LEN,
        IMSPECTOR_MSR_MAGIC, IMSPECTOR_SYNTHETIC_STACK_MAGIC,
    };
    use crate::common::error::BioFormatsError;
    use crate::common::metadata::DimensionOrder;
    use crate::common::pixel_type::PixelType;
    use crate::common::reader::FormatReader;
    use std::path::PathBuf;

    fn imspector_header(version: i32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(IMSPECTOR_FILE_MAGIC);
        bytes.extend_from_slice(&IMSPECTOR_MAGIC_NUMBER.to_le_bytes());
        bytes.extend_from_slice(&version.to_le_bytes());
        bytes
    }

    fn temp_path(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("bioformats_imspector_{name}"))
    }

    fn zlib_compress(bytes: &[u8]) -> Vec<u8> {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(bytes).unwrap();
        encoder.finish().unwrap()
    }

    fn synthetic_stack(width: i32, height: i32, z: i32, c: i32, t: i32, pixels: &[u8]) -> Vec<u8> {
        synthetic_stack_with_compression(width, height, z, c, t, 0, pixels)
    }

    fn synthetic_stack_with_compression(
        width: i32,
        height: i32,
        z: i32,
        c: i32,
        t: i32,
        compression: i32,
        payload: &[u8],
    ) -> Vec<u8> {
        let mut bytes = imspector_header(7);
        let stack_offset = 32u64;
        bytes.extend_from_slice(&stack_offset.to_le_bytes());
        bytes.resize(stack_offset as usize, 0);
        bytes.extend_from_slice(IMSPECTOR_SYNTHETIC_STACK_MAGIC);
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&z.to_le_bytes());
        bytes.extend_from_slice(&c.to_le_bytes());
        bytes.extend_from_slice(&t.to_le_bytes());
        bytes.extend_from_slice(&0x01i32.to_le_bytes());
        bytes.extend_from_slice(&compression.to_le_bytes());
        let payload_offset =
            (stack_offset as usize + IMSPECTOR_SYNTHETIC_STACK_MAGIC.len() + 44) as i64;
        bytes.extend_from_slice(&payload_offset.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as i64).to_le_bytes());
        bytes.extend_from_slice(payload);
        bytes
    }

    fn native_v1_stack_with_compression(
        width: i32,
        height: i32,
        z: i32,
        c: i32,
        t: i32,
        compression: i32,
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut bytes = imspector_header(1);
        let stack_offset = 32u64;
        bytes.extend_from_slice(&stack_offset.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.resize(stack_offset as usize, 0);

        bytes.extend_from_slice(IMSPECTOR_SYNTHETIC_STACK_MAGIC);
        bytes.extend_from_slice(&IMSPECTOR_MAGIC_NUMBER.to_le_bytes());
        bytes.extend_from_slice(&1i32.to_le_bytes());
        bytes.extend_from_slice(&5i32.to_le_bytes());
        for size in [width, height, z, c, t] {
            bytes.extend_from_slice(&size.to_le_bytes());
        }
        for _ in 5..15 {
            bytes.extend_from_slice(&1i32.to_le_bytes());
        }
        for _ in 0..30 {
            bytes.extend_from_slice(&0f64.to_bits().to_le_bytes());
        }
        bytes.extend_from_slice(&0x01i32.to_le_bytes());
        bytes.extend_from_slice(&compression.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&(pixels.len() as i64).to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(pixels);

        bytes.extend_from_slice(&124i32.to_le_bytes());
        for _ in 0..30 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        for _ in 0..5 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        bytes
    }

    fn native_v1_stack(width: i32, height: i32, z: i32, c: i32, t: i32, pixels: &[u8]) -> Vec<u8> {
        native_v1_stack_with_compression(width, height, z, c, t, 0, pixels)
    }

    fn push_len_string(bytes: &mut Vec<u8>, value: &str) {
        bytes.extend_from_slice(&(value.len() as i32).to_le_bytes());
        bytes.extend_from_slice(value.as_bytes());
    }

    fn java_msr_stack(width: i32, height: i32, z: i32, t: i32, pixels: &[u8]) -> Vec<u8> {
        let mut bytes = vec![0u8; 20];
        bytes[4..4 + IMSPECTOR_MSR_MAGIC.len()].copy_from_slice(IMSPECTOR_MSR_MAGIC);
        bytes.extend_from_slice(&(IMSPECTOR_MSR_MAGIC.len() as u16).to_le_bytes());
        bytes.extend_from_slice(IMSPECTOR_MSR_MAGIC);
        bytes.extend_from_slice(&1i32.to_le_bytes());
        bytes.push(4);
        bytes.extend_from_slice(b"Tag1");
        if bytes.len() % 2 == 1 {
            bytes.push(0);
        }
        let metadata = b"Instrument Mode::Frame Imaging::Time Time Resolution::1";
        bytes.extend_from_slice(&(metadata.len() as u16).to_le_bytes());
        bytes.extend_from_slice(metadata);
        if metadata.len() % 2 == 0 {
            bytes.push(13);
        }
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 26]);
        bytes.push(4);
        bytes.extend_from_slice(b"PMT1");
        bytes.extend_from_slice(&[0u8; 6]);
        bytes.extend_from_slice(&width.to_le_bytes());
        bytes.extend_from_slice(&height.to_le_bytes());
        bytes.extend_from_slice(&z.to_le_bytes());
        bytes.extend_from_slice(&t.to_le_bytes());
        bytes.extend_from_slice(&[0u8; 16]);
        for value in [
            b"A".as_slice(),
            b"B".as_slice(),
            b"C".as_slice(),
            b"D".as_slice(),
        ] {
            bytes.push(value.len() as u8);
            bytes.extend_from_slice(value);
        }
        bytes.extend_from_slice(pixels);
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes
    }

    fn native_v1_stack_with_step_tables() -> Vec<u8> {
        let mut bytes = imspector_header(1);
        let stack_offset = 32u64;
        bytes.extend_from_slice(&stack_offset.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.resize(stack_offset as usize, 0);

        bytes.extend_from_slice(IMSPECTOR_SYNTHETIC_STACK_MAGIC);
        bytes.extend_from_slice(&IMSPECTOR_MAGIC_NUMBER.to_le_bytes());
        bytes.extend_from_slice(&1i32.to_le_bytes());
        bytes.extend_from_slice(&5i32.to_le_bytes());
        for size in [2i32, 2, 1, 1, 1] {
            bytes.extend_from_slice(&size.to_le_bytes());
        }
        for _ in 5..15 {
            bytes.extend_from_slice(&1i32.to_le_bytes());
        }
        for _ in 0..30 {
            bytes.extend_from_slice(&0f64.to_bits().to_le_bytes());
        }
        bytes.extend_from_slice(&0x01i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());

        let payload = [9u8, 8, 7, 6];
        bytes.extend_from_slice(&(payload.len() as i64).to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&payload);

        bytes.extend_from_slice(&124i32.to_le_bytes());
        for d in 0..15 {
            bytes.extend_from_slice(&(if d == 0 { 1i32 } else { 0i32 }).to_le_bytes());
        }
        for d in 0..15 {
            bytes.extend_from_slice(&(if d == 1 { 1i32 } else { 0i32 }).to_le_bytes());
        }
        push_len_string(&mut bytes, "X");
        push_len_string(&mut bytes, "Y");
        for _ in 2..5 {
            push_len_string(&mut bytes, "");
        }
        bytes.extend_from_slice(&0.0f64.to_le_bytes());
        bytes.extend_from_slice(&0.25f64.to_le_bytes());
        push_len_string(&mut bytes, "top");
        push_len_string(&mut bytes, "bottom");
        bytes
    }

    fn native_v3_stack_with_flush_points() -> Vec<u8> {
        let mut bytes = imspector_header(1);
        let stack_offset = 32u64;
        bytes.extend_from_slice(&stack_offset.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.resize(stack_offset as usize, 0);

        bytes.extend_from_slice(IMSPECTOR_SYNTHETIC_STACK_MAGIC);
        bytes.extend_from_slice(&IMSPECTOR_MAGIC_NUMBER.to_le_bytes());
        bytes.extend_from_slice(&3i32.to_le_bytes());
        bytes.extend_from_slice(&5i32.to_le_bytes());
        for size in [2i32, 2, 1, 1, 1] {
            bytes.extend_from_slice(&size.to_le_bytes());
        }
        for _ in 5..15 {
            bytes.extend_from_slice(&1i32.to_le_bytes());
        }
        for _ in 0..30 {
            bytes.extend_from_slice(&0f64.to_bits().to_le_bytes());
        }
        bytes.extend_from_slice(&0x01i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());

        let payload = [1u8, 2, 3, 4];
        bytes.extend_from_slice(&(payload.len() as i64).to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&payload);

        bytes.extend_from_slice(&1424i32.to_le_bytes());
        for _ in 0..30 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        bytes.extend_from_slice(&0i32.to_le_bytes());
        for _ in 0..16 {
            bytes.extend_from_slice(&[0u8; 80]);
        }
        bytes.extend_from_slice(&2i64.to_le_bytes());
        bytes.extend_from_slice(&4096i64.to_le_bytes());
        for _ in 0..5 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        bytes.extend_from_slice(&128i64.to_le_bytes());
        bytes.extend_from_slice(&512i64.to_le_bytes());
        bytes
    }

    fn native_v6_chunked_stack() -> Vec<u8> {
        let tag_dictionary = b"laser=STED\n\x01";
        let mut bytes = imspector_header(1);
        let stack_offset = 32u64;
        bytes.extend_from_slice(&stack_offset.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.resize(stack_offset as usize, 0);

        bytes.extend_from_slice(IMSPECTOR_SYNTHETIC_STACK_MAGIC);
        bytes.extend_from_slice(&IMSPECTOR_MAGIC_NUMBER.to_le_bytes());
        bytes.extend_from_slice(&6i32.to_le_bytes());
        bytes.extend_from_slice(&5i32.to_le_bytes());
        for size in [2i32, 2, 2, 1, 1] {
            bytes.extend_from_slice(&size.to_le_bytes());
        }
        for _ in 5..15 {
            bytes.extend_from_slice(&1i32.to_le_bytes());
        }
        for _ in 0..30 {
            bytes.extend_from_slice(&0f64.to_bits().to_le_bytes());
        }
        bytes.extend_from_slice(&0x01i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());

        let payload = [1u8, 2, 3, 4, 0xee, 0xee, 5, 6, 7, 8];
        bytes.extend_from_slice(&(payload.len() as i64).to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&payload);

        bytes.extend_from_slice(&1468i32.to_le_bytes());
        for _ in 0..30 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        bytes.extend_from_slice(&0i32.to_le_bytes());
        for _ in 0..16 {
            bytes.extend_from_slice(&[0u8; 80]);
        }
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&(tag_dictionary.len() as i64).to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&6i32.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as i64).to_le_bytes());
        bytes.extend_from_slice(&8i64.to_le_bytes());
        bytes.extend_from_slice(&1i64.to_le_bytes());
        for _ in 0..5 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        bytes.extend_from_slice(tag_dictionary);
        bytes.extend_from_slice(&4i64.to_le_bytes());
        bytes.extend_from_slice(&6i64.to_le_bytes());
        bytes
    }

    fn native_v6_zlib_chunked_stack() -> Vec<u8> {
        let pixels = [1u8, 2, 3, 4, 5, 6, 7, 8];
        let compressed = zlib_compress(&pixels);
        let split = compressed.len().min(5);
        let gap = [0xee, 0xee, 0xee];
        let mut payload = Vec::new();
        payload.extend_from_slice(&compressed[..split]);
        payload.extend_from_slice(&gap);
        payload.extend_from_slice(&compressed[split..]);

        let mut bytes = imspector_header(1);
        let stack_offset = 32u64;
        bytes.extend_from_slice(&stack_offset.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.resize(stack_offset as usize, 0);

        bytes.extend_from_slice(IMSPECTOR_SYNTHETIC_STACK_MAGIC);
        bytes.extend_from_slice(&IMSPECTOR_MAGIC_NUMBER.to_le_bytes());
        bytes.extend_from_slice(&6i32.to_le_bytes());
        bytes.extend_from_slice(&5i32.to_le_bytes());
        for size in [2i32, 2, 2, 1, 1] {
            bytes.extend_from_slice(&size.to_le_bytes());
        }
        for _ in 5..15 {
            bytes.extend_from_slice(&1i32.to_le_bytes());
        }
        for _ in 0..30 {
            bytes.extend_from_slice(&0f64.to_bits().to_le_bytes());
        }
        bytes.extend_from_slice(&0x01i32.to_le_bytes());
        bytes.extend_from_slice(&1i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());

        bytes.extend_from_slice(&(payload.len() as i64).to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&payload);

        bytes.extend_from_slice(&1468i32.to_le_bytes());
        for _ in 0..30 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        bytes.extend_from_slice(&0i32.to_le_bytes());
        for _ in 0..16 {
            bytes.extend_from_slice(&[0u8; 80]);
        }
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&6i32.to_le_bytes());
        bytes.extend_from_slice(&(payload.len() as i64).to_le_bytes());
        bytes.extend_from_slice(&(compressed.len() as i64).to_le_bytes());
        bytes.extend_from_slice(&1i64.to_le_bytes());
        for _ in 0..5 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        bytes.extend_from_slice(&(split as i64).to_le_bytes());
        bytes.extend_from_slice(&((split + gap.len()) as i64).to_le_bytes());
        bytes
    }

    #[test]
    fn imspector_header_requires_exact_java_magic_and_magic_number() {
        let good = imspector_header(6);
        assert_eq!(parse_imspector_header(&good).unwrap().version, 6);

        let mut wrong_magic = good.clone();
        wrong_magic[7] = b'_';
        assert!(matches!(
            parse_imspector_header(&wrong_magic),
            Err(BioFormatsError::Format(message)) if message.contains("Not an Imspector")
        ));

        let mut wrong_number = good;
        wrong_number[8..10].copy_from_slice(&0x1234u16.to_le_bytes());
        assert!(matches!(
            parse_imspector_header(&wrong_number),
            Err(BioFormatsError::Format(message)) if message.contains("invalid magic number")
        ));
    }

    #[test]
    fn imspector_reader_detects_only_complete_obf_header() {
        let reader = ImspectorReader::new();
        assert!(!reader.is_this_type_by_bytes(b"OMAS_BF_not enough"));
        assert!(reader.is_this_type_by_bytes(&imspector_header(4)));
    }

    #[test]
    fn imspector_reader_detects_java_msr_cdatastack_header() {
        let reader = ImspectorReader::new();
        let mut bytes = vec![0u8; 32];
        bytes[12..12 + IMSPECTOR_MSR_MAGIC.len()].copy_from_slice(IMSPECTOR_MSR_MAGIC);
        assert!(reader.is_this_type_by_bytes(&bytes));

        bytes[12] = b'X';
        assert!(!reader.is_this_type_by_bytes(&bytes));
    }

    #[test]
    fn imspector_helpers_match_bioformats_type_contracts() {
        assert_eq!(imspector_pixel_type(0x01).unwrap(), PixelType::Uint8);
        assert_eq!(imspector_pixel_type(0x08).unwrap(), PixelType::Int16);
        assert_eq!(imspector_pixel_type(0x40).unwrap(), PixelType::Float32);
        assert_eq!(imspector_bits_per_pixel(0x80).unwrap(), 64);
        assert!(matches!(
            imspector_pixel_type(0x03),
            Err(BioFormatsError::Format(message)) if message.contains("Unsupported")
        ));

        assert_eq!(imspector_stack_length(17).unwrap(), 17);
        assert!(matches!(
            imspector_stack_length(-1),
            Err(BioFormatsError::Format(message)) if message.contains("Negative")
        ));
        assert!(!imspector_compression_flag(0).unwrap());
        assert!(imspector_compression_flag(1).unwrap());
        assert!(matches!(
            imspector_compression_flag(2),
            Err(BioFormatsError::Format(message)) if message.contains("Unsupported")
        ));
    }

    #[test]
    fn imspector_length_prefixed_string_tracks_offset_and_bounds() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&5i32.to_le_bytes());
        bytes.extend_from_slice(b"hello");
        bytes.extend_from_slice(&0i32.to_le_bytes());

        let mut offset = 0;
        assert_eq!(
            imspector_read_len_string(&bytes, &mut offset).unwrap(),
            "hello"
        );
        assert_eq!(offset, 9);
        assert_eq!(imspector_read_len_string(&bytes, &mut offset).unwrap(), "");
        assert_eq!(offset, 13);

        let mut truncated_offset = 0;
        assert!(matches!(
            imspector_read_len_string(&[4, 0, 0, 0, b'x'], &mut truncated_offset),
            Err(BioFormatsError::Format(message)) if message.contains("overruns")
        ));
    }

    #[test]
    fn imspector_set_id_parses_header_then_refuses_unported_stack_decoder() {
        let path = temp_path("header_only.obf");
        let mut bytes = imspector_header(7);
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&4i32.to_le_bytes());
        bytes.extend_from_slice(b"desc");
        std::fs::write(&path, bytes).unwrap();

        let mut reader = ImspectorReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("version 7")
                    && message.contains("native stack decoding is unsupported")
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imspector_synthetic_raw_stack_opens_planes_and_regions() {
        let path = temp_path("synthetic_raw.obf");
        let pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
        std::fs::write(&path, synthetic_stack(2, 2, 2, 1, 1, &pixels)).unwrap();

        let mut reader = ImspectorReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.size_c, 1);
        assert_eq!(meta.size_t, 1);
        assert_eq!(meta.image_count, 2);
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        assert_eq!(meta.dimension_order, DimensionOrder::XYZCT);

        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
        assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(), vec![6, 8]);
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imspector_native_v1_uncompressed_stack_opens_planes_and_regions() {
        let path = temp_path("native_v1_raw.obf");
        let pixels = vec![10, 11, 12, 13, 20, 21, 22, 23];
        std::fs::write(&path, native_v1_stack(2, 2, 2, 1, 1, &pixels)).unwrap();

        let mut reader = ImspectorReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.size_c, 1);
        assert_eq!(meta.size_t, 1);
        assert_eq!(meta.image_count, 2);
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        match meta.series_metadata.get("imspector_version_subset") {
            Some(crate::common::metadata::MetadataValue::String(value)) => {
                assert_eq!(value, "native-v1-uncompressed-contiguous")
            }
            other => panic!("unexpected imspector_version_subset metadata: {other:?}"),
        }

        assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 11, 12, 13]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![20, 21, 22, 23]);
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
            vec![11, 13]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imspector_java_msr_first_cdatastack_matches_reference_layout() {
        use crate::common::metadata::MetadataValue;

        let path = temp_path("java_msr_first_stack.msr");
        let pixels = vec![1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0, 8, 0];
        std::fs::write(&path, java_msr_stack(2, 2, 2, 1, &pixels)).unwrap();

        let mut reader = ImspectorReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.size_c, 1);
        assert_eq!(meta.size_t, 1);
        assert_eq!(meta.image_count, 2);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert_eq!(meta.bits_per_pixel, 16);
        assert_eq!(meta.dimension_order, DimensionOrder::XYZCT);
        match meta.series_metadata.get("Instrument Mode") {
            Some(MetadataValue::String(value)) => assert_eq!(value, "Frame Imaging"),
            other => panic!("unexpected Instrument Mode metadata: {other:?}"),
        }
        match meta.series_metadata.get("imspector_msr_pmt") {
            Some(MetadataValue::String(value)) => assert_eq!(value, "PMT1"),
            other => panic!("unexpected PMT metadata: {other:?}"),
        }

        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 0, 2, 0, 3, 0, 4, 0]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 0, 6, 0, 7, 0, 8, 0]);
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(),
            vec![6, 0, 8, 0]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imspector_native_step_tables_are_preserved_as_bounded_metadata() {
        let path = temp_path("native_v1_step_tables.obf");
        std::fs::write(&path, native_v1_stack_with_step_tables()).unwrap();

        let mut reader = ImspectorReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.image_count, 1);
        match meta.series_metadata.get("imspector_dimension_labels") {
            Some(crate::common::metadata::MetadataValue::String(value)) => {
                assert_eq!(value, "0:X;1:Y")
            }
            other => panic!("unexpected imspector_dimension_labels metadata: {other:?}"),
        }
        match meta.series_metadata.get("imspector_step_table_axes") {
            Some(crate::common::metadata::MetadataValue::String(value)) => assert_eq!(value, "0"),
            other => panic!("unexpected imspector_step_table_axes metadata: {other:?}"),
        }
        match meta.series_metadata.get("imspector_step_table_previews") {
            Some(crate::common::metadata::MetadataValue::String(value)) => {
                assert_eq!(value, "0:0,0.25")
            }
            other => panic!("unexpected imspector_step_table_previews metadata: {other:?}"),
        }
        match meta.series_metadata.get("imspector_step_label_axes") {
            Some(crate::common::metadata::MetadataValue::String(value)) => assert_eq!(value, "1"),
            other => panic!("unexpected imspector_step_label_axes metadata: {other:?}"),
        }
        match meta.series_metadata.get("imspector_step_label_previews") {
            Some(crate::common::metadata::MetadataValue::String(value)) => {
                assert_eq!(value, "1:top,bottom")
            }
            other => panic!("unexpected imspector_step_label_previews metadata: {other:?}"),
        }

        assert_eq!(reader.open_bytes(0).unwrap(), vec![9, 8, 7, 6]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imspector_native_v3_flush_points_are_preserved_as_bounded_metadata() {
        let path = temp_path("native_v3_flush_points.obf");
        std::fs::write(&path, native_v3_stack_with_flush_points()).unwrap();

        let mut reader = ImspectorReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.image_count, 1);
        match meta.series_metadata.get("imspector_flush_point_count") {
            Some(crate::common::metadata::MetadataValue::Int(value)) => assert_eq!(*value, 2),
            other => panic!("unexpected imspector_flush_point_count metadata: {other:?}"),
        }
        match meta.series_metadata.get("imspector_flush_block_size") {
            Some(crate::common::metadata::MetadataValue::Int(value)) => assert_eq!(*value, 4096),
            other => panic!("unexpected imspector_flush_block_size metadata: {other:?}"),
        }
        match meta.series_metadata.get("imspector_flush_point_previews") {
            Some(crate::common::metadata::MetadataValue::String(value)) => {
                assert_eq!(value, "128,512")
            }
            other => panic!("unexpected imspector_flush_point_previews metadata: {other:?}"),
        }

        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imspector_native_v6_uncompressed_chunked_stack_opens_planes() {
        let path = temp_path("native_v6_chunked.obf");
        std::fs::write(&path, native_v6_chunked_stack()).unwrap();

        let mut reader = ImspectorReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.image_count, 2);
        match meta.series_metadata.get("imspector_version_subset") {
            Some(crate::common::metadata::MetadataValue::String(value)) => {
                assert_eq!(value, "native-v6-uncompressed-chunked")
            }
            other => panic!("unexpected imspector_version_subset metadata: {other:?}"),
        }
        match meta.series_metadata.get("imspector_tag_dictionary_length") {
            Some(crate::common::metadata::MetadataValue::Int(value)) => assert_eq!(*value, 12),
            other => panic!("unexpected imspector_tag_dictionary_length metadata: {other:?}"),
        }
        match meta
            .series_metadata
            .get("imspector_tag_dictionary_ascii_preview")
        {
            Some(crate::common::metadata::MetadataValue::String(value)) => {
                assert_eq!(value, "laser=STED..")
            }
            other => {
                panic!("unexpected imspector_tag_dictionary_ascii_preview metadata: {other:?}")
            }
        }
        match meta
            .series_metadata
            .get("imspector_tag_dictionary_hex_preview")
        {
            Some(crate::common::metadata::MetadataValue::String(value)) => {
                assert_eq!(value, "6c 61 73 65 72 3d 53 54 45 44 0a 01")
            }
            other => panic!("unexpected imspector_tag_dictionary_hex_preview metadata: {other:?}"),
        }
        match meta.series_metadata.get("imspector_minimum_format_version") {
            Some(crate::common::metadata::MetadataValue::Int(value)) => assert_eq!(*value, 6),
            other => panic!("unexpected imspector_minimum_format_version metadata: {other:?}"),
        }
        match meta.series_metadata.get("imspector_stack_end_used") {
            Some(crate::common::metadata::MetadataValue::Int(value)) => assert_eq!(*value, 10),
            other => panic!("unexpected imspector_stack_end_used metadata: {other:?}"),
        }
        match meta.series_metadata.get("imspector_samples_written") {
            Some(crate::common::metadata::MetadataValue::Int(value)) => assert_eq!(*value, 8),
            other => panic!("unexpected imspector_samples_written metadata: {other:?}"),
        }
        match meta.series_metadata.get("imspector_chunk_position_count") {
            Some(crate::common::metadata::MetadataValue::Int(value)) => assert_eq!(*value, 1),
            other => panic!("unexpected imspector_chunk_position_count metadata: {other:?}"),
        }

        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
        assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(), vec![6, 8]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imspector_native_v6_zlib_chunked_stack_opens_planes() {
        let path = temp_path("native_v6_zlib_chunked.obf");
        std::fs::write(&path, native_v6_zlib_chunked_stack()).unwrap();

        let mut reader = ImspectorReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.image_count, 2);
        match meta.series_metadata.get("imspector_version_subset") {
            Some(crate::common::metadata::MetadataValue::String(value)) => {
                assert_eq!(value, "native-v6-zlib-chunked")
            }
            other => panic!("unexpected imspector_version_subset metadata: {other:?}"),
        }

        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
        assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(), vec![2, 4]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imspector_native_v1_compressed_stack_opens_planes_and_regions() {
        let path = temp_path("native_v1_zlib.obf");
        let pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let compressed = zlib_compress(&pixels);
        std::fs::write(
            &path,
            native_v1_stack_with_compression(2, 2, 2, 1, 1, 1, &compressed),
        )
        .unwrap();

        let mut reader = ImspectorReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.image_count, 2);
        match meta.series_metadata.get("imspector_version_subset") {
            Some(crate::common::metadata::MetadataValue::String(value)) => {
                assert_eq!(value, "native-v1-zlib-contiguous")
            }
            other => panic!("unexpected imspector_version_subset metadata: {other:?}"),
        }

        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
        assert_eq!(reader.open_bytes_region(1, 0, 1, 2, 1).unwrap(), vec![7, 8]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imspector_native_v1_compressed_stack_rejects_wrong_decompressed_size() {
        let path = temp_path("native_v1_zlib_wrong_size.obf");
        let compressed = zlib_compress(&[1, 2, 3]);
        std::fs::write(
            &path,
            native_v1_stack_with_compression(2, 2, 1, 1, 1, 1, &compressed),
        )
        .unwrap();

        let mut reader = ImspectorReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::Format(message)
                if message.contains("native decompressed payload length 3")
                    && message.contains("declared stack size 4")
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imspector_synthetic_compressed_stack_opens_planes_and_regions() {
        let path = temp_path("synthetic_zlib.obf");
        let pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let compressed = zlib_compress(&pixels);
        std::fs::write(
            &path,
            synthetic_stack_with_compression(2, 2, 2, 1, 1, 1, &compressed),
        )
        .unwrap();

        let mut reader = ImspectorReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.image_count, 2);
        match meta.series_metadata.get("imspector_version_subset") {
            Some(crate::common::metadata::MetadataValue::String(value)) => {
                assert_eq!(value, "synthetic-zlib-raw")
            }
            other => panic!("unexpected imspector_version_subset metadata: {other:?}"),
        }

        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
        assert_eq!(reader.open_bytes_region(1, 0, 1, 2, 1).unwrap(), vec![7, 8]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imspector_synthetic_stack_rejects_bad_payload_bounds_and_dimensions() {
        let short_payload = temp_path("synthetic_short_payload.obf");
        let mut bytes = synthetic_stack(2, 2, 1, 1, 1, &[1, 2, 3]);
        std::fs::write(&short_payload, &bytes).unwrap();
        let mut reader = ImspectorReader::new();
        let err = reader.set_id(&short_payload).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::Format(message) if message.contains("does not match declared stack size")
        ));
        let _ = std::fs::remove_file(short_payload);

        let bad_dim = temp_path("synthetic_bad_dim.obf");
        bytes = synthetic_stack(0, 2, 1, 1, 1, &[1, 2]);
        std::fs::write(&bad_dim, &bytes).unwrap();
        let err = reader.set_id(&bad_dim).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::Format(message) if message.contains("width must be positive")
        ));
        let _ = std::fs::remove_file(bad_dim);

        let truncated_stack = temp_path("synthetic_truncated_stack.obf");
        bytes = imspector_header(7);
        bytes.extend_from_slice(&(IMSPECTOR_MIN_HEADER_LEN as u64 + 8).to_le_bytes());
        bytes.extend_from_slice(b"short");
        std::fs::write(&truncated_stack, &bytes).unwrap();
        let err = reader.set_id(&truncated_stack).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::Format(message) if message.contains("stack header is truncated")
        ));
        let _ = std::fs::remove_file(truncated_stack);
    }

    #[test]
    fn imspector_synthetic_compressed_stack_rejects_wrong_decompressed_size() {
        let path = temp_path("synthetic_zlib_wrong_size.obf");
        let compressed = zlib_compress(&[1, 2, 3]);
        std::fs::write(
            &path,
            synthetic_stack_with_compression(2, 2, 1, 1, 1, 1, &compressed),
        )
        .unwrap();

        let mut reader = ImspectorReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::Format(message)
                if message.contains("decompressed payload length 3")
                    && message.contains("declared stack size 4")
        ));

        let _ = std::fs::remove_file(path);
    }

    // Build a single self-contained native v1 stack block (magic .. footer)
    // with no file-level header, so several can be concatenated and linked
    // through their `next` pointers to exercise the multi-stack series loop.
    fn native_v1_stack_block(
        width: i32,
        height: i32,
        z: i32,
        c: i32,
        t: i32,
        next: i64,
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(IMSPECTOR_SYNTHETIC_STACK_MAGIC);
        bytes.extend_from_slice(&IMSPECTOR_MAGIC_NUMBER.to_le_bytes());
        bytes.extend_from_slice(&1i32.to_le_bytes());
        bytes.extend_from_slice(&5i32.to_le_bytes());
        for size in [width, height, z, c, t] {
            bytes.extend_from_slice(&size.to_le_bytes());
        }
        for _ in 5..15 {
            bytes.extend_from_slice(&1i32.to_le_bytes());
        }
        for _ in 0..30 {
            bytes.extend_from_slice(&0f64.to_bits().to_le_bytes());
        }
        bytes.extend_from_slice(&0x01i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&(pixels.len() as i64).to_le_bytes());
        bytes.extend_from_slice(&next.to_le_bytes());
        bytes.extend_from_slice(pixels);

        bytes.extend_from_slice(&124i32.to_le_bytes());
        for _ in 0..30 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        for _ in 0..5 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn imspector_native_multi_stack_exposes_one_series_per_stack() {
        // Two linked native v1 stacks with different dimensions: the file
        // header points at the first stack, whose `next` points at the second.
        let stack1_pixels = vec![10u8, 11, 12, 13, 14, 15, 16, 17, 18];
        let header_len = 32usize;
        // The encoded `next` value does not change the block length, so the
        // second stack starts at header_len + len(first block).
        let block_len = native_v1_stack_block(2, 2, 2, 1, 1, 0, &[1, 2, 3, 4, 5, 6, 7, 8]).len();
        let next_offset = (header_len + block_len) as i64;
        let stack0 = native_v1_stack_block(2, 2, 2, 1, 1, next_offset, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let stack1 = native_v1_stack_block(3, 3, 1, 1, 1, 0, &stack1_pixels);

        let mut bytes = imspector_header(1);
        bytes.extend_from_slice(&(header_len as u64).to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.resize(header_len, 0);
        bytes.extend_from_slice(&stack0);
        bytes.extend_from_slice(&stack1);

        let path = temp_path("native_multi_stack.obf");
        std::fs::write(&path, bytes).unwrap();

        let mut reader = ImspectorReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 2);

        reader.set_series(0).unwrap();
        assert_eq!(reader.series(), 0);
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_z, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);

        reader.set_series(1).unwrap();
        assert_eq!(reader.series(), 1);
        assert_eq!(reader.metadata().size_x, 3);
        assert_eq!(reader.metadata().size_y, 3);
        assert_eq!(reader.metadata().image_count, 1);
        assert_eq!(reader.open_bytes(0).unwrap(), stack1_pixels);

        assert!(matches!(
            reader.set_series(2),
            Err(BioFormatsError::SeriesOutOfRange(2))
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imspector_native_multi_stack_rejects_out_of_range_next_pointer() {
        let stack0 = native_v1_stack_block(2, 2, 1, 1, 1, i64::MAX, &[1, 2, 3, 4]);
        let header_len = 32usize;
        let mut bytes = imspector_header(1);
        bytes.extend_from_slice(&(header_len as u64).to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.resize(header_len, 0);
        bytes.extend_from_slice(&stack0);

        let path = temp_path("native_multi_stack_bad_next.obf");
        std::fs::write(&path, bytes).unwrap();

        let mut reader = ImspectorReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::Format(message)
                if message.contains("next stack offset is out of range")
        ));

        let _ = std::fs::remove_file(path);
    }

    fn native_v1_stack_with_description(
        lengths: [f64; 5],
        offsets: [f64; 5],
        name: &str,
        description: &str,
        pixels: &[u8],
    ) -> Vec<u8> {
        let mut bytes = imspector_header(1);
        let stack_offset = 32u64;
        bytes.extend_from_slice(&stack_offset.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.resize(stack_offset as usize, 0);

        bytes.extend_from_slice(IMSPECTOR_SYNTHETIC_STACK_MAGIC);
        bytes.extend_from_slice(&IMSPECTOR_MAGIC_NUMBER.to_le_bytes());
        bytes.extend_from_slice(&1i32.to_le_bytes());
        bytes.extend_from_slice(&5i32.to_le_bytes());
        for size in [2i32, 2, 1, 1, 1] {
            bytes.extend_from_slice(&size.to_le_bytes());
        }
        for _ in 5..15 {
            bytes.extend_from_slice(&1i32.to_le_bytes());
        }
        // 15 Lengths doubles (first 5 meaningful) then 15 Offsets doubles.
        for d in 0..15 {
            let value = lengths.get(d).copied().unwrap_or(0.0);
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        for d in 0..15 {
            let value = offsets.get(d).copied().unwrap_or(0.0);
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        bytes.extend_from_slice(&0x01i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        bytes.extend_from_slice(&(name.len() as i32).to_le_bytes());
        bytes.extend_from_slice(&(description.len() as i32).to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(&(pixels.len() as i64).to_le_bytes());
        bytes.extend_from_slice(&0i64.to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(description.as_bytes());
        bytes.extend_from_slice(pixels);

        bytes.extend_from_slice(&124i32.to_le_bytes());
        for _ in 0..30 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        for _ in 0..5 {
            bytes.extend_from_slice(&0i32.to_le_bytes());
        }
        bytes
    }

    #[test]
    fn imspector_native_initstack_metadata_is_translated() {
        use crate::common::metadata::MetadataValue;
        // documentElement (doc) -> root -> TimeLapse (nodeName) -> Active/Step
        // (grandchild keys). Java renames `<Time Lapse ` to `<TimeLapse `.
        let description = "<doc><root><Time Lapse mode=\"on\"><Active>1</Active>\
            <Step>0.5</Step></Time Lapse></root></doc>";
        let bytes = native_v1_stack_with_description(
            [2.0, 4.0, 0.0, 0.0, 0.0],
            [1.5, 2.5, 0.0, 0.0, 0.0],
            "stack-name",
            description,
            &[9u8, 8, 7, 6],
        );
        let path = temp_path("native_v1_initstack_meta.obf");
        std::fs::write(&path, bytes).unwrap();

        let mut reader = ImspectorReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);

        // Stack version is recorded faithfully to OBFReader.initStack.
        match meta.series_metadata.get("Stack version") {
            Some(MetadataValue::Int(v)) => assert_eq!(*v, 1),
            other => panic!("unexpected Stack version metadata: {other:?}"),
        }
        // Lengths / Offsets arrays (first numberOfDimensions entries).
        match meta.series_metadata.get("Lengths") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "2, 4, 0, 0, 0"),
            other => panic!("unexpected Lengths metadata: {other:?}"),
        }
        match meta.series_metadata.get("Offsets") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "1.5, 2.5, 0, 0, 0"),
            other => panic!("unexpected Offsets metadata: {other:?}"),
        }
        // Stack Name.
        match meta.series_metadata.get("Name") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "stack-name"),
            other => panic!("unexpected Name metadata: {other:?}"),
        }
        // Time-Lapse XML grandchildren keyed `nodeName + " " + key`.
        match meta.series_metadata.get("TimeLapse Active") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "1"),
            other => panic!("unexpected TimeLapse Active metadata: {other:?}"),
        }
        match meta.series_metadata.get("TimeLapse Step") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "0.5"),
            other => panic!("unexpected TimeLapse Step metadata: {other:?}"),
        }
        // Physical sizes derived from Lengths / dimension size (length<0.01 is
        // treated as metres; here both are plain micrometres).
        match meta.series_metadata.get("PhysicalSizeX") {
            Some(MetadataValue::Float(v)) => assert!((*v - 1.0).abs() < 1e-9),
            other => panic!("unexpected PhysicalSizeX metadata: {other:?}"),
        }

        assert_eq!(reader.open_bytes(0).unwrap(), vec![9, 8, 7, 6]);
        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// 5. Hamamatsu VMS whole-slide
// ---------------------------------------------------------------------------

const HAMAMATSU_VMS_MAX_SIZE: u32 = 2048;

fn hamamatsu_vms_normalize_key(key: &str) -> String {
    key.trim()
        .chars()
        .filter(|c| !c.is_ascii_whitespace())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

fn hamamatsu_vms_parse_index(bytes: &[u8]) -> Result<HashMap<String, String>> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        BioFormatsError::Format("Not a Hamamatsu VMS/VMU text index file".to_string())
    })?;
    let mut saw_assignment = false;
    let mut saw_vms_key = false;
    let mut values = HashMap::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            saw_assignment = true;
            let key = hamamatsu_vms_normalize_key(key);
            if matches!(
                key.as_str(),
                "nolayers"
                    | "no_layers"
                    | "nojpegcolumns"
                    | "nojpegrows"
                    | "imagefile"
                    | "imagefilename"
                    | "imagename"
                    | "imagepath"
                    | "mapfile"
                    | "mapfilename"
                    | "mapimage"
                    | "mapimagefile"
                    | "optimisationfile"
                    | "optimizationfile"
                    | "physicalwidth"
                    | "physicalheight"
                    | "macroimage"
                    | "macroimagefile"
                    | "hamamatsu"
            ) {
                saw_vms_key = true;
            }
            values.insert(key, value.trim().to_string());
        }
    }

    if saw_assignment && saw_vms_key {
        Ok(values)
    } else {
        Err(BioFormatsError::Format(
            "Not a Hamamatsu VMS/VMU text index file".to_string(),
        ))
    }
}

fn hamamatsu_vms_required_u32(values: &HashMap<String, String>, key: &str) -> Result<u32> {
    values
        .get(key)
        .ok_or_else(|| BioFormatsError::UnsupportedFormat(format!("Hamamatsu VMS missing {key}")))?
        .parse::<u32>()
        .map_err(|_| BioFormatsError::UnsupportedFormat(format!("Hamamatsu VMS invalid {key}")))
}

fn hamamatsu_vms_optional_u32(
    values: &HashMap<String, String>,
    keys: &[String],
) -> Result<Option<u32>> {
    for key in keys {
        if let Some(value) = values.get(key) {
            return value.parse::<u32>().map(Some).map_err(|_| {
                BioFormatsError::UnsupportedFormat(format!("Hamamatsu VMS invalid {key}"))
            });
        }
    }
    Ok(None)
}

fn hamamatsu_vms_tile_key(col: u32, row: u32) -> String {
    if col == 0 && row == 0 {
        "imagefile".to_string()
    } else {
        format!("imagefile({col},{row})")
    }
}

fn hamamatsu_vms_push_key(keys: &mut Vec<String>, key: String) {
    if !keys.contains(&key) {
        keys.push(key);
    }
}

fn hamamatsu_vms_clean_path(value: &str) -> &str {
    let trimmed = value.trim();
    trimmed
        .strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .or_else(|| {
            trimmed
                .strip_prefix('\'')
                .and_then(|value| value.strip_suffix('\''))
        })
        .unwrap_or(trimmed)
        .trim()
}

fn hamamatsu_vms_resolve_sidecar_path(parent: &Path, value: &str) -> PathBuf {
    let cleaned = hamamatsu_vms_clean_path(value);
    if cleaned.is_empty() {
        return parent.join(cleaned);
    }

    let path = Path::new(cleaned);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else {
        parent.join(path)
    };
    if joined.is_file() {
        return joined;
    }

    let basename = cleaned
        .rsplit(['/', '\\'])
        .find(|part| !part.is_empty())
        .unwrap_or(cleaned);
    if basename != cleaned {
        let fallback = parent.join(basename);
        if fallback.is_file() {
            return fallback;
        }
    }

    if let Ok(entries) = std::fs::read_dir(parent) {
        let mut matches = entries
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| name.eq_ignore_ascii_case(basename))
            })
            .map(|entry| entry.path());
        if let Some(first) = matches.next() {
            if matches.next().is_none() {
                return first;
            }
        }
    }

    joined
}

fn hamamatsu_vms_tile_key_candidates(
    prefix: &str,
    layer: u32,
    col: u32,
    row: u32,
    include_plain: bool,
) -> Vec<String> {
    let mut keys = Vec::new();
    if include_plain && layer == 0 && col == 0 && row == 0 {
        hamamatsu_vms_push_key(&mut keys, prefix.to_string());
    }
    if layer == 0 {
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}({col},{row})"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}({row},{col})"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}[{col},{row}]"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}[{row},{col}]"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}[{col}][{row}]"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}[{row}][{col}]"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}_{col}_{row}"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}_{row}_{col}"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}column{col}row{row}"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}row{row}column{col}"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}col{col}row{row}"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}row{row}col{col}"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}x{col}y{row}"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}y{row}x{col}"));
    }
    if layer == 0 && row == 0 {
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}{col}"));
    }
    if col == 0 && row == 0 {
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}({layer})"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}[{layer}]"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}_{layer}"));
    }
    for (a, b, c) in [
        (layer, col, row),
        (col, row, layer),
        (layer, row, col),
        (col, layer, row),
        (row, col, layer),
        (row, layer, col),
    ] {
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}({a},{b},{c})"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}[{a},{b},{c}]"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}[{a}][{b}][{c}]"));
        hamamatsu_vms_push_key(&mut keys, format!("{prefix}_{a}_{b}_{c}"));
    }
    keys
}

fn hamamatsu_vms_tile_value<'a>(
    values: &'a HashMap<String, String>,
    layer: u32,
    col: u32,
    row: u32,
) -> Option<&'a String> {
    for prefix in ["imagefile", "imagefilename", "imagename", "imagepath"] {
        let keys = hamamatsu_vms_tile_key_candidates(prefix, layer, col, row, true);
        for key in &keys {
            if let Some(value) = values.get(key) {
                return Some(value);
            }
        }
    }
    None
}

fn hamamatsu_vms_pyramid_key(level: u32, name: &str) -> Vec<String> {
    [
        format!("pyramidlevel{level}{name}"),
        format!("pyramidlevel{level}.{name}"),
        format!("resolution{level}{name}"),
        format!("resolution{level}.{name}"),
        format!("level{level}{name}"),
        format!("level{level}.{name}"),
        format!("opt.pyramidlevel{level}{name}"),
        format!("opt.pyramidlevel{level}.{name}"),
        format!("opt.resolution{level}{name}"),
        format!("opt.resolution{level}.{name}"),
        format!("opt.level{level}{name}"),
        format!("opt.level{level}.{name}"),
    ]
    .into_iter()
    .collect()
}

fn hamamatsu_vms_pyramid_tile_value<'a>(
    values: &'a HashMap<String, String>,
    level: u32,
    layer: u32,
    col: u32,
    row: u32,
) -> Option<&'a String> {
    let mut keys = Vec::new();
    for prefix in hamamatsu_vms_pyramid_key(level, "imagefile") {
        keys.extend(hamamatsu_vms_tile_key_candidates(
            &prefix, layer, col, row, true,
        ));
    }
    for key in &keys {
        if let Some(value) = values.get(key) {
            return Some(value);
        }
    }
    None
}

#[derive(Default)]
struct HamamatsuVmsJpegMarkerMetadata {
    sof_marker: Option<u8>,
    precision: Option<u8>,
    components: Option<u8>,
    restart_interval: Option<u16>,
    jfif: bool,
    exif: bool,
    adobe_transform: Option<u8>,
    icc_declared_chunks: Option<u8>,
    icc_seen_chunks: u8,
    icc_missing_chunks: u8,
    icc_invalid_chunks: u8,
    icc_duplicate_chunks: u8,
    icc_profile: Option<Vec<u8>>,
}

impl HamamatsuVmsJpegMarkerMetadata {
    fn color_model(&self) -> &'static str {
        match (self.components, self.adobe_transform) {
            (Some(1), _) => "grayscale",
            (Some(3), Some(0)) => "rgb",
            (Some(3), _) => "ycbcr",
            (Some(4), Some(2)) => "ycck",
            (Some(4), _) => "cmyk",
            _ => "unknown",
        }
    }

    fn sof_family(&self) -> Option<&'static str> {
        match self.sof_marker? {
            0xc0 => Some("baseline dct"),
            0xc1 => Some("extended sequential dct"),
            0xc2 => Some("progressive dct"),
            0xc3 => Some("lossless sequential"),
            0xc5 => Some("differential sequential dct"),
            0xc6 => Some("differential progressive dct"),
            0xc7 => Some("differential lossless"),
            0xc9 => Some("extended sequential arithmetic"),
            0xca => Some("progressive arithmetic"),
            0xcb => Some("lossless arithmetic"),
            0xcd => Some("differential sequential arithmetic"),
            0xce => Some("differential progressive arithmetic"),
            0xcf => Some("differential lossless arithmetic"),
            _ => Some("unknown"),
        }
    }

    fn is_progressive(&self) -> bool {
        matches!(self.sof_marker, Some(0xc2 | 0xc6 | 0xca | 0xce))
    }

    fn is_lossless(&self) -> bool {
        matches!(self.sof_marker, Some(0xc3 | 0xc7 | 0xcb | 0xcf))
    }

    fn uses_arithmetic_coding(&self) -> bool {
        matches!(
            self.sof_marker,
            Some(0xc9 | 0xca | 0xcb | 0xcd | 0xce | 0xcf)
        )
    }

    fn icc_complete(&self) -> bool {
        match (self.icc_declared_chunks, self.icc_profile.as_ref()) {
            (Some(count), Some(_)) => {
                count == self.icc_seen_chunks
                    && count > 0
                    && self.icc_missing_chunks == 0
                    && self.icc_invalid_chunks == 0
                    && self.icc_duplicate_chunks == 0
            }
            _ => false,
        }
    }

    fn has_icc_markers(&self) -> bool {
        self.icc_declared_chunks.is_some()
            || self.icc_seen_chunks > 0
            || self.icc_invalid_chunks > 0
            || self.icc_duplicate_chunks > 0
    }

    fn color_conversion_note(&self) -> &'static str {
        match self.color_model() {
            "grayscale" => "grayscale expanded to rgb",
            "rgb" => "decoder rgb output",
            "ycbcr" => "decoder ycbcr to rgb",
            "cmyk" => "cmyk formula without icc",
            "ycck" => "decoder ycck/cmyk path without icc",
            _ => "unknown jpeg color encoding",
        }
    }

    fn color_management_note(&self) -> &'static str {
        if self.icc_complete() {
            "icc profile preserved but not applied"
        } else if self.has_icc_markers() {
            "incomplete icc profile markers preserved but not applied"
        } else {
            "no icc profile markers found"
        }
    }
}

fn hamamatsu_vms_read_marker_byte(file: &mut File) -> Result<Option<u8>> {
    let mut byte = [0u8; 1];
    loop {
        match file.read_exact(&mut byte) {
            Ok(()) if byte[0] == 0xff => break,
            Ok(()) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(BioFormatsError::Io(e)),
        }
    }
    loop {
        match file.read_exact(&mut byte) {
            Ok(()) if byte[0] == 0xff => continue,
            Ok(()) if byte[0] == 0x00 => return Ok(None),
            Ok(()) => return Ok(Some(byte[0])),
            Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
            Err(e) => return Err(BioFormatsError::Io(e)),
        }
    }
}

fn hamamatsu_vms_parse_jpeg_marker_metadata(path: &Path) -> Result<HamamatsuVmsJpegMarkerMetadata> {
    let mut file = File::open(path).map_err(BioFormatsError::Io)?;
    let mut soi = [0u8; 2];
    file.read_exact(&mut soi).map_err(BioFormatsError::Io)?;
    if soi != [0xff, 0xd8] {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Hamamatsu VMS JPEG marker metadata expected SOI marker: {}",
            path.display()
        )));
    }

    let mut meta = HamamatsuVmsJpegMarkerMetadata::default();
    let mut icc_chunks: HashMap<u8, Vec<u8>> = HashMap::new();
    while let Some(marker) = hamamatsu_vms_read_marker_byte(&mut file)? {
        if marker == 0xd9 || marker == 0xda {
            break;
        }
        if (0xd0..=0xd7).contains(&marker) || marker == 0x01 {
            continue;
        }

        let mut len_bytes = [0u8; 2];
        file.read_exact(&mut len_bytes)
            .map_err(BioFormatsError::Io)?;
        let segment_len = u16::from_be_bytes(len_bytes) as usize;
        if segment_len < 2 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Hamamatsu VMS JPEG marker 0xff{marker:02x} has invalid length in {}",
                path.display()
            )));
        }
        let payload_len = segment_len - 2;

        match marker {
            0xc0 | 0xc1 | 0xc2 | 0xc3 | 0xc5 | 0xc6 | 0xc7 | 0xc9 | 0xca | 0xcb | 0xcd | 0xce
            | 0xcf => {
                let mut payload = vec![0u8; payload_len];
                file.read_exact(&mut payload).map_err(BioFormatsError::Io)?;
                if payload.len() >= 6 {
                    meta.sof_marker = Some(marker);
                    meta.precision = Some(payload[0]);
                    meta.components = Some(payload[5]);
                }
            }
            0xdd => {
                let mut payload = [0u8; 2];
                if payload_len == 2 {
                    file.read_exact(&mut payload).map_err(BioFormatsError::Io)?;
                    meta.restart_interval = Some(u16::from_be_bytes(payload));
                } else {
                    file.seek(SeekFrom::Current(payload_len as i64))
                        .map_err(BioFormatsError::Io)?;
                }
            }
            0xe0 | 0xe1 | 0xe2 | 0xee => {
                let mut payload = vec![0u8; payload_len];
                file.read_exact(&mut payload).map_err(BioFormatsError::Io)?;
                match marker {
                    0xe0 if payload.starts_with(b"JFIF\0") => meta.jfif = true,
                    0xe1 if payload.starts_with(b"Exif\0\0") => meta.exif = true,
                    0xe2 if payload.starts_with(b"ICC_PROFILE\0") && payload.len() >= 14 => {
                        let seq = payload[12];
                        let count = payload[13];
                        if seq > 0 && count > 0 {
                            meta.icc_declared_chunks = Some(count);
                            if seq > count {
                                meta.icc_invalid_chunks = meta.icc_invalid_chunks.saturating_add(1);
                            } else if icc_chunks.insert(seq, payload[14..].to_vec()).is_some() {
                                meta.icc_duplicate_chunks =
                                    meta.icc_duplicate_chunks.saturating_add(1);
                            }
                        } else {
                            meta.icc_invalid_chunks = meta.icc_invalid_chunks.saturating_add(1);
                        }
                    }
                    0xee if payload.starts_with(b"Adobe") && payload.len() >= 12 => {
                        meta.adobe_transform = Some(payload[11]);
                    }
                    _ => {}
                }
            }
            _ => {
                file.seek(SeekFrom::Current(payload_len as i64))
                    .map_err(BioFormatsError::Io)?;
            }
        }
    }

    if let Some(count) = meta.icc_declared_chunks {
        meta.icc_seen_chunks = icc_chunks.len() as u8;
        meta.icc_missing_chunks = (1..=count)
            .filter(|seq| !icc_chunks.contains_key(seq))
            .count() as u8;
        if meta.icc_seen_chunks == count
            && meta.icc_missing_chunks == 0
            && meta.icc_invalid_chunks == 0
            && meta.icc_duplicate_chunks == 0
        {
            let total_len = (1..=count)
                .filter_map(|seq| icc_chunks.get(&seq))
                .map(Vec::len)
                .sum();
            let mut profile = Vec::with_capacity(total_len);
            for seq in 1..=count {
                if let Some(chunk) = icc_chunks.remove(&seq) {
                    profile.extend_from_slice(&chunk);
                }
            }
            meta.icc_profile = Some(profile);
        }
    }

    Ok(meta)
}

fn hamamatsu_vms_insert_jpeg_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    prefix: &str,
    path: &Path,
) -> Result<()> {
    let markers = hamamatsu_vms_parse_jpeg_marker_metadata(path)?;
    metadata.insert(
        format!("{prefix} JPEG color model"),
        MetadataValue::String(markers.color_model().into()),
    );
    metadata.insert(
        format!("{prefix} JPEG color conversion"),
        MetadataValue::String(markers.color_conversion_note().into()),
    );
    metadata.insert(
        format!("{prefix} JPEG color management"),
        MetadataValue::String(markers.color_management_note().into()),
    );
    metadata.insert(
        format!("{prefix} JPEG progressive"),
        MetadataValue::Bool(markers.is_progressive()),
    );
    metadata.insert(
        format!("{prefix} JPEG lossless"),
        MetadataValue::Bool(markers.is_lossless()),
    );
    metadata.insert(
        format!("{prefix} JPEG arithmetic coding"),
        MetadataValue::Bool(markers.uses_arithmetic_coding()),
    );
    metadata.insert(
        format!("{prefix} JPEG ICC profile applied"),
        MetadataValue::Bool(false),
    );
    metadata.insert(
        format!("{prefix} JPEG ICC profile complete"),
        MetadataValue::Bool(markers.icc_complete()),
    );
    if let Some(marker) = markers.sof_marker {
        metadata.insert(
            format!("{prefix} JPEG SOF marker"),
            MetadataValue::String(format!("0x{marker:02x}")),
        );
    }
    if let Some(family) = markers.sof_family() {
        metadata.insert(
            format!("{prefix} JPEG SOF family"),
            MetadataValue::String(family.into()),
        );
    }
    if let Some(precision) = markers.precision {
        metadata.insert(
            format!("{prefix} JPEG precision"),
            MetadataValue::Int(precision as i64),
        );
    }
    if let Some(components) = markers.components {
        metadata.insert(
            format!("{prefix} JPEG components"),
            MetadataValue::Int(components as i64),
        );
    }
    if let Some(interval) = markers.restart_interval {
        metadata.insert(
            format!("{prefix} JPEG restart interval"),
            MetadataValue::Int(interval as i64),
        );
    }
    if markers.jfif {
        metadata.insert(format!("{prefix} JPEG JFIF"), MetadataValue::Bool(true));
    }
    if markers.exif {
        metadata.insert(format!("{prefix} JPEG Exif"), MetadataValue::Bool(true));
    }
    if let Some(transform) = markers.adobe_transform {
        metadata.insert(
            format!("{prefix} JPEG Adobe transform"),
            MetadataValue::Int(transform as i64),
        );
    }
    if markers.has_icc_markers() {
        metadata.insert(
            format!("{prefix} JPEG ICC markers present"),
            MetadataValue::Bool(true),
        );
    }
    if let Some(count) = markers.icc_declared_chunks {
        metadata.insert(
            format!("{prefix} JPEG ICC declared chunks"),
            MetadataValue::Int(count as i64),
        );
        metadata.insert(
            format!("{prefix} JPEG ICC seen chunks"),
            MetadataValue::Int(markers.icc_seen_chunks as i64),
        );
        metadata.insert(
            format!("{prefix} JPEG ICC missing chunks"),
            MetadataValue::Int(markers.icc_missing_chunks as i64),
        );
        if markers.icc_invalid_chunks > 0 {
            metadata.insert(
                format!("{prefix} JPEG ICC invalid chunks"),
                MetadataValue::Int(markers.icc_invalid_chunks as i64),
            );
        }
        if markers.icc_duplicate_chunks > 0 {
            metadata.insert(
                format!("{prefix} JPEG ICC duplicate chunks"),
                MetadataValue::Int(markers.icc_duplicate_chunks as i64),
            );
        }
    }
    if let Some(profile) = markers.icc_profile {
        metadata.insert(
            format!("{prefix} JPEG ICC profile bytes"),
            MetadataValue::Int(profile.len() as i64),
        );
        metadata.insert(
            format!("{prefix} JPEG ICC profile"),
            MetadataValue::Bytes(profile),
        );
    }
    Ok(())
}

fn hamamatsu_vms_insert_capability_diagnostics(
    metadata: &mut HashMap<String, MetadataValue>,
    pixel_prefix: &str,
) {
    metadata.insert(
        "VMS tile key aliases supported".into(),
        MetadataValue::String(
            "ImageFile, ImageFileName, ImageName, ImagePath with plain, (), [], [][ ], _, label-based row/column or x/y, and single-row zero-based numeric suffix coordinate forms".into(),
        ),
    );
    metadata.insert(
        "VMS associated image key aliases supported".into(),
        MetadataValue::String(
            "MacroImage, MacroImageFile, MacroImageName, MapFile, MapFileName, MapImage, MapImageFile"
                .into(),
        ),
    );
    metadata.insert(
        "VMS unsupported tile key alias handling".into(),
        MetadataValue::String(
            "unrecognized tile aliases are not guessed; missing tiles fail with the unresolved layer/key"
                .into(),
        ),
    );
    metadata.insert(
        format!("{pixel_prefix} JPEG decoded pixel formats"),
        MetadataValue::String("RGB24, L8 expanded to RGB, CMYK32 converted to RGB".into()),
    );
    metadata.insert(
        format!("{pixel_prefix} JPEG unsupported color handling"),
        MetadataValue::String(
            "other decoder pixel formats and ICC/profile transforms are reported but not applied"
                .into(),
        ),
    );
}

fn hamamatsu_vms_parse_optional_index(path: &Path) -> Result<HashMap<String, String>> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(HashMap::new()),
        Err(err) => return Err(BioFormatsError::Io(err)),
    };
    let text = std::str::from_utf8(&bytes).map_err(|_| {
        BioFormatsError::UnsupportedFormat(format!(
            "Hamamatsu VMS optimisation file is not UTF-8 text: {}",
            path.display()
        ))
    })?;
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty()
            || line.starts_with('#')
            || line.starts_with(';')
            || (line.starts_with('[') && line.ends_with(']'))
        {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            values.insert(
                format!("opt.{}", hamamatsu_vms_normalize_key(key)),
                value.trim().to_string(),
            );
        }
    }
    Ok(values)
}

fn hamamatsu_vms_decode_jpeg(path: &Path, scale_denom: u32) -> Result<(u32, u32, Vec<u8>)> {
    let file = File::open(path).map_err(BioFormatsError::Io)?;
    let mut decoder = jpeg_decoder::Decoder::new(file);
    if scale_denom > 1 {
        let (width, height) = hamamatsu_vms_jpeg_dimensions(path)?;
        if width % scale_denom != 0 || height % scale_denom != 0 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Hamamatsu VMS JPEG tile dimensions are not divisible by {scale_denom}: {}",
                path.display()
            )));
        }
        let requested_width = (width / scale_denom) as u16;
        let requested_height = (height / scale_denom) as u16;
        let (scaled_width, scaled_height) = decoder
            .scale(requested_width, requested_height)
            .map_err(|err| {
                BioFormatsError::UnsupportedFormat(format!(
                    "Hamamatsu VMS JPEG tile scaled header decode failed for {}: {err}",
                    path.display()
                ))
            })?;
        if scaled_width != requested_width || scaled_height != requested_height {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Hamamatsu VMS JPEG tile cannot be decoded at exact 1/{scale_denom} scale: {}",
                path.display()
            )));
        }
    }
    let data = decoder.decode().map_err(|err| {
        BioFormatsError::UnsupportedFormat(format!(
            "Hamamatsu VMS JPEG tile decode failed for {}: {err}",
            path.display()
        ))
    })?;
    let info = decoder.info().ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "Hamamatsu VMS JPEG tile has no image info: {}",
            path.display()
        ))
    })?;

    let rgb = match info.pixel_format {
        jpeg_decoder::PixelFormat::RGB24 => data,
        jpeg_decoder::PixelFormat::L8 => {
            let mut out = Vec::with_capacity(data.len() * 3);
            for v in data {
                out.extend_from_slice(&[v, v, v]);
            }
            out
        }
        jpeg_decoder::PixelFormat::CMYK32 => hamamatsu_vms_cmyk_to_rgb(&data),
        other => {
            return Err(hamamatsu_vms_unsupported_jpeg_pixel_format_error(
                other, path,
            ));
        }
    };
    Ok((info.width as u32, info.height as u32, rgb))
}

fn hamamatsu_vms_unsupported_jpeg_pixel_format_error(
    pixel_format: jpeg_decoder::PixelFormat,
    path: &Path,
) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "Hamamatsu VMS JPEG tile pixel format {pixel_format:?} is unsupported for RGB output; \
         supported decoder pixel formats are RGB24, L8 expanded to RGB, and CMYK32 converted to RGB; \
         ICC/profile transforms are preserved as metadata only and are not applied: {}",
        path.display()
    ))
}

struct HamamatsuVmsDecodedJpegBand {
    width: u32,
    height: u32,
    y: u32,
    rows: u32,
    rgb: Vec<u8>,
}

fn hamamatsu_vms_decode_jpeg_rows(
    path: &Path,
    scale_denom: u32,
    y: u32,
    h: u32,
) -> Result<HamamatsuVmsDecodedJpegBand> {
    if scale_denom == 1 && h > 0 {
        let mut file = File::open(path).map_err(BioFormatsError::Io)?;
        let mut data = Vec::new();
        file.read_to_end(&mut data).map_err(BioFormatsError::Io)?;
        if let Some(index) = jpeg_restart::index(&data) {
            if let Some(decoded) = index.decode_rows_default(&data, y, h) {
                let band = decoded?;
                let rgb = hamamatsu_vms_jpeg_pixels_to_rgb(
                    band.pixels,
                    band.band_width,
                    band.band_height,
                    path,
                )?;
                return Ok(HamamatsuVmsDecodedJpegBand {
                    width: band.band_width,
                    height: index.height(),
                    y: band.band_y0,
                    rows: band.band_height,
                    rgb,
                });
            }
        }
    }

    let (width, height, rgb) = hamamatsu_vms_decode_jpeg(path, scale_denom)?;
    Ok(HamamatsuVmsDecodedJpegBand {
        width,
        height,
        y: 0,
        rows: height,
        rgb,
    })
}

fn hamamatsu_vms_jpeg_pixels_to_rgb(
    data: Vec<u8>,
    width: u32,
    height: u32,
    path: &Path,
) -> Result<Vec<u8>> {
    let pixels = (width as usize)
        .checked_mul(height as usize)
        .ok_or_else(|| BioFormatsError::Format("Hamamatsu VMS JPEG band size overflows".into()))?;
    let channels = data
        .len()
        .checked_div(pixels.max(1))
        .filter(|channels| pixels == 0 || channels * pixels == data.len())
        .ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!(
                "Hamamatsu VMS JPEG tile has inconsistent decoded byte count: {}",
                path.display()
            ))
        })?;
    match channels {
        3 => Ok(data),
        1 => {
            let mut out = Vec::with_capacity(data.len() * 3);
            for v in data {
                out.extend_from_slice(&[v, v, v]);
            }
            Ok(out)
        }
        4 => Ok(hamamatsu_vms_cmyk_to_rgb(&data)),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "Hamamatsu VMS JPEG tile decoded to {other} channels: {}",
            path.display()
        ))),
    }
}

fn hamamatsu_vms_cmyk_to_rgb(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() / 4 * 3);
    for pixel in data.chunks_exact(4) {
        let c = 255 - u16::from(pixel[0]);
        let m = 255 - u16::from(pixel[1]);
        let y = 255 - u16::from(pixel[2]);
        let k = 255 - u16::from(pixel[3]);
        out.push(((k * c) / 255) as u8);
        out.push(((k * m) / 255) as u8);
        out.push(((k * y) / 255) as u8);
    }
    out
}

fn hamamatsu_vms_jpeg_dimensions(path: &Path) -> Result<(u32, u32)> {
    let file = File::open(path).map_err(BioFormatsError::Io)?;
    let mut decoder = jpeg_decoder::Decoder::new(file);
    if decoder.read_info().is_err() {
        return hamamatsu_vms_jpeg_dimensions_from_decode(path);
    }
    let info = decoder.info().ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "Hamamatsu VMS JPEG tile has no image info: {}",
            path.display()
        ))
    })?;
    Ok((info.width as u32, info.height as u32))
}

fn hamamatsu_vms_jpeg_dimensions_from_decode(path: &Path) -> Result<(u32, u32)> {
    let file = File::open(path).map_err(BioFormatsError::Io)?;
    let mut decoder = jpeg_decoder::Decoder::new(file);
    decoder.decode().map_err(|err| {
        BioFormatsError::UnsupportedFormat(format!(
            "Hamamatsu VMS JPEG tile header decode failed for {}: {err}",
            path.display()
        ))
    })?;
    let info = decoder.info().ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "Hamamatsu VMS JPEG tile has no image info: {}",
            path.display()
        ))
    })?;
    Ok((info.width as u32, info.height as u32))
}

#[derive(Clone)]
struct HamamatsuVmsTile {
    path: PathBuf,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    scale_denom: u32,
}

enum HamamatsuVmsPixels {
    TilePyramid(Vec<Vec<Vec<HamamatsuVmsTile>>>),
    Jpeg(PathBuf),
}

struct HamamatsuVmsSeries {
    metadata: Vec<ImageMetadata>,
    pixels: HamamatsuVmsPixels,
}

fn hamamatsu_vms_build_tile_layers<F>(
    values: &HashMap<String, String>,
    parent: &Path,
    cols: u32,
    rows: u32,
    layers: u32,
    mut tile_name: F,
) -> Result<(Vec<Vec<HamamatsuVmsTile>>, u32, u32)>
where
    F: FnMut(&HashMap<String, String>, u32, u32, u32) -> Option<&String>,
{
    if cols == 0 || rows == 0 || layers == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Hamamatsu VMS tile grid and layer count must be positive".into(),
        ));
    }
    let tile_count = cols
        .checked_mul(rows)
        .ok_or_else(|| BioFormatsError::Format("Hamamatsu VMS tile count overflows".into()))?;
    let mut layer_tiles = Vec::with_capacity(layers as usize);
    let mut full_size_x = 0u32;
    let mut full_size_y = 0u32;

    for layer in 0..layers {
        let mut grid: Vec<Option<HamamatsuVmsTile>> = vec![None; tile_count as usize];
        let mut col_widths = vec![0u32; cols as usize];
        let mut row_heights = vec![0u32; rows as usize];

        for row in 0..rows {
            for col in 0..cols {
                let name = tile_name(values, layer, col, row).ok_or_else(|| {
                    let key = hamamatsu_vms_tile_key(col, row);
                    BioFormatsError::UnsupportedFormat(format!(
                        "Hamamatsu VMS missing layer {layer} {key}"
                    ))
                })?;
                let tile_path = hamamatsu_vms_resolve_sidecar_path(parent, name);
                let (width, height) = hamamatsu_vms_jpeg_dimensions(&tile_path)?;
                col_widths[col as usize] = col_widths[col as usize].max(width);
                row_heights[row as usize] = row_heights[row as usize].max(height);
                grid[(row * cols + col) as usize] = Some(HamamatsuVmsTile {
                    path: tile_path,
                    x: 0,
                    y: 0,
                    width,
                    height,
                    scale_denom: 1,
                });
            }
        }

        let mut x_offsets = Vec::with_capacity(cols as usize);
        let mut size_x = 0u32;
        for width in &col_widths {
            x_offsets.push(size_x);
            size_x = size_x.checked_add(*width).ok_or_else(|| {
                BioFormatsError::Format("Hamamatsu VMS image width overflows".into())
            })?;
        }
        let mut y_offsets = Vec::with_capacity(rows as usize);
        let mut size_y = 0u32;
        for height in &row_heights {
            y_offsets.push(size_y);
            size_y = size_y.checked_add(*height).ok_or_else(|| {
                BioFormatsError::Format("Hamamatsu VMS image height overflows".into())
            })?;
        }
        if layer == 0 {
            full_size_x = size_x;
            full_size_y = size_y;
        } else if size_x != full_size_x || size_y != full_size_y {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Hamamatsu VMS layer {layer} dimensions {size_x}x{size_y} differ from layer 0 {full_size_x}x{full_size_y}"
            )));
        }

        let mut tiles = Vec::with_capacity(grid.len());
        for row in 0..rows {
            for col in 0..cols {
                let mut tile = grid[(row * cols + col) as usize].take().unwrap();
                tile.x = x_offsets[col as usize];
                tile.y = y_offsets[row as usize];
                tiles.push(tile);
            }
        }
        layer_tiles.push(tiles);
    }

    Ok((layer_tiles, full_size_x, full_size_y))
}

fn hamamatsu_vms_scaled_tile_layers(
    source_layers: &[Vec<HamamatsuVmsTile>],
    scale_denom: u32,
) -> Option<(Vec<Vec<HamamatsuVmsTile>>, u32, u32)> {
    if !matches!(scale_denom, 2 | 4 | 8) || source_layers.is_empty() {
        return None;
    }

    let mut scaled_layers = Vec::with_capacity(source_layers.len());
    let mut scaled_size_x = 0u32;
    let mut scaled_size_y = 0u32;
    for (layer, tiles) in source_layers.iter().enumerate() {
        let mut scaled_tiles = Vec::with_capacity(tiles.len());
        let mut layer_size_x = 0u32;
        let mut layer_size_y = 0u32;
        for tile in tiles {
            if tile.x % scale_denom != 0
                || tile.y % scale_denom != 0
                || tile.width % scale_denom != 0
                || tile.height % scale_denom != 0
                || tile.scale_denom != 1
            {
                return None;
            }
            let scaled_x = tile.x / scale_denom;
            let scaled_y = tile.y / scale_denom;
            let scaled_width = tile.width / scale_denom;
            let scaled_height = tile.height / scale_denom;
            layer_size_x = layer_size_x.max(scaled_x.checked_add(scaled_width)?);
            layer_size_y = layer_size_y.max(scaled_y.checked_add(scaled_height)?);
            scaled_tiles.push(HamamatsuVmsTile {
                path: tile.path.clone(),
                x: scaled_x,
                y: scaled_y,
                width: scaled_width,
                height: scaled_height,
                scale_denom,
            });
        }
        if layer == 0 {
            scaled_size_x = layer_size_x;
            scaled_size_y = layer_size_y;
        } else if layer_size_x != scaled_size_x || layer_size_y != scaled_size_y {
            return None;
        }
        scaled_layers.push(scaled_tiles);
    }

    Some((scaled_layers, scaled_size_x, scaled_size_y))
}

/// Hamamatsu VMS/VMU whole-slide format (`.vms`, `.vmu`).
///
/// The text index names a grid of native JPEG tile files. Pixel dimensions are
/// read from the tile JPEG headers because the index only stores physical sizes.
pub struct HamamatsuVmsReader {
    path: Option<PathBuf>,
    series: Vec<HamamatsuVmsSeries>,
    current_series: usize,
    current_resolution: usize,
    metadata_level: MetadataLevel,
}

impl HamamatsuVmsReader {
    pub fn new() -> Self {
        HamamatsuVmsReader {
            path: None,
            series: Vec::new(),
            current_series: 0,
            current_resolution: 0,
            metadata_level: MetadataLevel::All,
        }
    }
}

impl Default for HamamatsuVmsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for HamamatsuVmsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("vms") | Some("vmu"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let mut values = hamamatsu_vms_parse_index(&bytes)?;
        let cols = hamamatsu_vms_required_u32(&values, "nojpegcolumns")?;
        let rows = hamamatsu_vms_required_u32(&values, "nojpegrows")?;
        let layers = values
            .get("nolayers")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1);
        if cols == 0 || rows == 0 || layers == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Hamamatsu VMS tile grid and layer count must be positive".into(),
            ));
        }

        let parent = path.parent().unwrap_or_else(|| Path::new("."));

        if let Some(name) = values
            .get("optimisationfile")
            .or_else(|| values.get("optimizationfile"))
            .cloned()
        {
            let opt_path = parent.join(name);
            for (key, value) in hamamatsu_vms_parse_optional_index(&opt_path)? {
                values.insert(key, value);
            }
        }

        let base_series_metadata: HashMap<String, MetadataValue> = values
            .iter()
            .map(|(k, v)| (format!("VMS {k}"), MetadataValue::String(v.to_string())))
            .collect();

        let mut series = Vec::new();
        let (full_layer_tiles, full_size_x, full_size_y) =
            hamamatsu_vms_build_tile_layers(&values, parent, cols, rows, layers, |v, l, c, r| {
                hamamatsu_vms_tile_value(v, l, c, r)
            })?;
        let declared_resolution_count = values
            .get("pyramidlevels")
            .or_else(|| values.get("opt.pyramidlevels"))
            .or_else(|| values.get("resolutioncount"))
            .or_else(|| values.get("opt.resolutioncount"))
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(1)
            .max(1);
        let mut pyramid_tiles = vec![full_layer_tiles];
        let mut pyramid_sizes = vec![(full_size_x, full_size_y)];
        for level in 1..declared_resolution_count {
            let scale_denom = 1u32.checked_shl(level).unwrap_or(0);
            let col_keys = hamamatsu_vms_pyramid_key(level, "nojpegcolumns");
            let row_keys = hamamatsu_vms_pyramid_key(level, "nojpegrows");
            let level_cols = hamamatsu_vms_optional_u32(&values, &col_keys)?;
            let level_rows = hamamatsu_vms_optional_u32(&values, &row_keys)?;
            let (tiles, size_x, size_y) =
                if let (Some(level_cols), Some(level_rows)) = (level_cols, level_rows) {
                    hamamatsu_vms_build_tile_layers(
                        &values,
                        parent,
                        level_cols,
                        level_rows,
                        layers,
                        |v, l, c, r| hamamatsu_vms_pyramid_tile_value(v, level, l, c, r),
                    )
                    .map_err(|err| match err {
                        BioFormatsError::UnsupportedFormat(message) => {
                            BioFormatsError::UnsupportedFormat(format!(
                                "Hamamatsu VMS pyramid level {level}: {message}"
                            ))
                        }
                        other => other,
                    })?
                } else if let Some(scaled) =
                    hamamatsu_vms_scaled_tile_layers(&pyramid_tiles[0], scale_denom)
                {
                    scaled
                } else {
                    break;
                };
            if size_x >= full_size_x || size_y >= full_size_y {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Hamamatsu VMS pyramid level {level} is not lower resolution"
                )));
            }
            pyramid_tiles.push(tiles);
            pyramid_sizes.push((size_x, size_y));
        }
        let mut full_metadata = base_series_metadata.clone();
        full_metadata.insert(
            "VMS series kind".into(),
            MetadataValue::String("full resolution".into()),
        );
        // Java initFile reads SourceLens as the objective nominal magnification and
        // (outside MINIMUM) attaches it to an Instrument/Objective on series 0 only.
        if let Some(magnification) = values
            .get("sourcelens")
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v > 0.0)
        {
            full_metadata.insert(
                "VMS source_lens_magnification".into(),
                MetadataValue::Float(magnification),
            );
        }
        if let (Some(physical_width), true) = (
            values
                .get("physicalwidth")
                .and_then(|v| v.parse::<f64>().ok()),
            full_size_x > 0,
        ) {
            full_metadata.insert(
                "VMS physical_size_x".into(),
                MetadataValue::Float(physical_width / full_size_x as f64),
            );
        }
        if let (Some(physical_height), true) = (
            values
                .get("physicalheight")
                .and_then(|v| v.parse::<f64>().ok()),
            full_size_y > 0,
        ) {
            full_metadata.insert(
                "VMS physical_size_y".into(),
                MetadataValue::Float(physical_height / full_size_y as f64),
            );
        }
        if let Some(first_tile_path) = pyramid_tiles
            .first()
            .and_then(|resolution| resolution.first())
            .and_then(|layer| layer.first())
            .map(|tile| tile.path.clone())
        {
            hamamatsu_vms_insert_jpeg_metadata(&mut full_metadata, "VMS tile", &first_tile_path)?;
        }
        hamamatsu_vms_insert_capability_diagnostics(&mut full_metadata, "VMS tile");
        let mut pyramid_metadata = Vec::with_capacity(pyramid_sizes.len());
        for (resolution, (size_x, size_y)) in pyramid_sizes.iter().copied().enumerate() {
            let mut metadata = full_metadata.clone();
            metadata.insert(
                "VMS resolution".into(),
                MetadataValue::Int(resolution as i64),
            );
            pyramid_metadata.push(ImageMetadata {
                size_x: full_size_x,
                size_y: full_size_y,
                size_z: 1,
                size_c: 3,
                size_t: 1,
                pixel_type: PixelType::Uint8,
                bits_per_pixel: 8,
                image_count: layers,
                dimension_order: DimensionOrder::XYCZT,
                is_rgb: true,
                is_interleaved: size_x > HAMAMATSU_VMS_MAX_SIZE && size_y > HAMAMATSU_VMS_MAX_SIZE,
                is_indexed: false,
                is_little_endian: true,
                resolution_count: pyramid_sizes.len() as u32,
                thumbnail: false,
                series_metadata: metadata,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
            let last = pyramid_metadata.last_mut().unwrap();
            last.size_x = size_x;
            last.size_y = size_y;
            if let (Some(physical_width), true) = (
                values
                    .get("physicalwidth")
                    .and_then(|v| v.parse::<f64>().ok()),
                size_x > 0,
            ) {
                last.series_metadata.insert(
                    "VMS physical_size_x".into(),
                    MetadataValue::Float(physical_width / size_x as f64),
                );
            }
            if let (Some(physical_height), true) = (
                values
                    .get("physicalheight")
                    .and_then(|v| v.parse::<f64>().ok()),
                size_y > 0,
            ) {
                last.series_metadata.insert(
                    "VMS physical_size_y".into(),
                    MetadataValue::Float(physical_height / size_y as f64),
                );
            }
        }
        series.push(HamamatsuVmsSeries {
            metadata: pyramid_metadata,
            pixels: HamamatsuVmsPixels::TilePyramid(pyramid_tiles),
        });

        let mut associated_kinds = Vec::new();
        for (kind, key, physical_width_key, physical_height_key) in [
            (
                "macro",
                "macroimage",
                "physicalmacrowidth",
                "physicalmacroheight",
            ),
            (
                "macro",
                "macroimagefile",
                "physicalmacrowidth",
                "physicalmacroheight",
            ),
            (
                "macro",
                "macroimagename",
                "physicalmacrowidth",
                "physicalmacroheight",
            ),
            ("map", "mapfile", "", ""),
            ("map", "mapfilename", "", ""),
            ("map", "mapimage", "", ""),
            ("map", "mapimagefile", "", ""),
        ] {
            if associated_kinds.contains(&kind) {
                continue;
            }
            let Some(name) = values.get(key) else {
                continue;
            };
            let image_path = hamamatsu_vms_resolve_sidecar_path(parent, name);
            let (size_x, size_y) = hamamatsu_vms_jpeg_dimensions(&image_path)?;
            associated_kinds.push(kind);
            let mut series_metadata = base_series_metadata.clone();
            series_metadata.insert("VMS series kind".into(), MetadataValue::String(kind.into()));
            hamamatsu_vms_insert_jpeg_metadata(&mut series_metadata, "VMS image", &image_path)?;
            hamamatsu_vms_insert_capability_diagnostics(&mut series_metadata, "VMS image");
            if let (Some(physical_width), true) = (
                values
                    .get(physical_width_key)
                    .and_then(|v| v.parse::<f64>().ok()),
                size_x > 0,
            ) {
                series_metadata.insert(
                    "VMS physical_size_x".into(),
                    MetadataValue::Float(physical_width / size_x as f64),
                );
            }
            if let (Some(physical_height), true) = (
                values
                    .get(physical_height_key)
                    .and_then(|v| v.parse::<f64>().ok()),
                size_y > 0,
            ) {
                series_metadata.insert(
                    "VMS physical_size_y".into(),
                    MetadataValue::Float(physical_height / size_y as f64),
                );
            }
            series.push(HamamatsuVmsSeries {
                metadata: vec![ImageMetadata {
                    size_x,
                    size_y,
                    size_z: 1,
                    size_c: 3,
                    size_t: 1,
                    pixel_type: PixelType::Uint8,
                    bits_per_pixel: 8,
                    image_count: 1,
                    dimension_order: DimensionOrder::XYCZT,
                    is_rgb: true,
                    is_interleaved: size_x > HAMAMATSU_VMS_MAX_SIZE
                        && size_y > HAMAMATSU_VMS_MAX_SIZE,
                    is_indexed: false,
                    is_little_endian: true,
                    resolution_count: 1,
                    thumbnail: true,
                    series_metadata,
                    lookup_table: None,
                    modulo_z: None,
                    modulo_c: None,
                    modulo_t: None,
                }],
                pixels: HamamatsuVmsPixels::Jpeg(image_path),
            });
        }

        self.path = Some(path.to_path_buf());
        self.series = series;
        self.current_series = 0;
        self.current_resolution = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series.clear();
        self.current_series = 0;
        self.current_resolution = 0;
        Ok(())
    }

    fn set_metadata_options(&mut self, options: MetadataOptions) {
        self.metadata_level = options.level;
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.series.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s >= self.series.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            self.current_resolution = 0;
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current_series)
            .and_then(|s| s.metadata.get(self.current_resolution))
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        let meta = self.metadata();
        if std::ptr::eq(meta, crate::common::reader::uninitialized_metadata()) {
            return None;
        }

        let mut ome = OmeMetadata::from_image_metadata(meta);

        // Java initFile sets the image name from the index file's base name plus a
        // suffix that depends on the series kind ("full resolution"/"macro"/"map").
        let base_name = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned());
        let kind = match meta.series_metadata.get("VMS series kind") {
            Some(MetadataValue::String(kind)) => kind.as_str(),
            _ => "full resolution",
        };
        if let (Some(base_name), Some(image)) = (base_name.as_ref(), ome.images.get_mut(0)) {
            image.name = Some(format!("{base_name} {kind}"));
        }

        // Java guards physical sizes, instrument and objective behind a
        // non-MINIMUM metadata level. Physical sizes apply to the full resolution
        // (series 0) and macro (series 1); the instrument/objective applies to
        // series 0 only.
        if self.metadata_level != MetadataLevel::Minimal {
            if kind == "full resolution" || kind == "macro" {
                if let Some(image) = ome.images.get_mut(0) {
                    if let Some(MetadataValue::Float(size_x)) =
                        meta.series_metadata.get("VMS physical_size_x")
                    {
                        if size_x.is_finite() && *size_x > 0.0 {
                            image.physical_size_x = Some(*size_x);
                        }
                    }
                    if let Some(MetadataValue::Float(size_y)) =
                        meta.series_metadata.get("VMS physical_size_y")
                    {
                        if size_y.is_finite() && *size_y > 0.0 {
                            image.physical_size_y = Some(*size_y);
                        }
                    }
                }
            }
            if kind == "full resolution" {
                let magnification = match meta.series_metadata.get("VMS source_lens_magnification")
                {
                    Some(MetadataValue::Float(value)) if value.is_finite() && *value > 0.0 => {
                        Some(*value)
                    }
                    _ => None,
                };
                ome.instruments.push(OmeInstrument {
                    id: Some(create_lsid("Instrument", &[0])),
                    objectives: vec![OmeObjective {
                        id: Some(create_lsid("Objective", &[0, 0])),
                        nominal_magnification: magnification,
                        ..OmeObjective::default()
                    }],
                    ..OmeInstrument::default()
                });
                if let Some(image) = ome.images.get_mut(0) {
                    image.instrument_ref = Some(0);
                    image.objective_ref = Some(0);
                }
            }
        }

        let _ = ome.add_original_metadata_annotations(meta, 0);
        Some(ome)
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .and_then(|s| s.metadata.get(self.current_resolution))
            .ok_or(BioFormatsError::NotInitialized)?;
        self.open_bytes_region(plane_index, 0, 0, meta.size_x, meta.size_y)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let series = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = series
            .metadata
            .get(self.current_resolution)
            .ok_or_else(|| {
                BioFormatsError::Format("Hamamatsu VMS resolution out of range".into())
            })?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let x2 = x.checked_add(w).ok_or_else(|| {
            BioFormatsError::Format("Hamamatsu VMS region width overflows".into())
        })?;
        let y2 = y.checked_add(h).ok_or_else(|| {
            BioFormatsError::Format("Hamamatsu VMS region height overflows".into())
        })?;
        if x2 > meta.size_x || y2 > meta.size_y {
            return Err(BioFormatsError::Format(
                "Hamamatsu VMS region is outside image bounds".into(),
            ));
        }

        let row_bytes = (w as usize)
            .checked_mul(3)
            .ok_or_else(|| BioFormatsError::Format("Hamamatsu VMS row size overflows".into()))?;
        let out_len = row_bytes.checked_mul(h as usize).ok_or_else(|| {
            BioFormatsError::Format("Hamamatsu VMS output buffer size overflows".into())
        })?;
        let mut out = vec![0u8; out_len];
        match &series.pixels {
            HamamatsuVmsPixels::TilePyramid(pyramid) => {
                let layers = pyramid.get(self.current_resolution).ok_or_else(|| {
                    BioFormatsError::Format("Hamamatsu VMS resolution out of range".into())
                })?;
                let tiles = layers
                    .get(plane_index as usize)
                    .ok_or_else(|| BioFormatsError::PlaneOutOfRange(plane_index))?;
                for tile in tiles {
                    let tx2 = tile.x + tile.width;
                    let ty2 = tile.y + tile.height;
                    if tx2 <= x || tile.x >= x2 || ty2 <= y || tile.y >= y2 {
                        continue;
                    }

                    let ix0 = tile.x.max(x);
                    let iy0 = tile.y.max(y);
                    let ix1 = tx2.min(x2);
                    let iy1 = ty2.min(y2);
                    let band_y = iy0 - tile.y;
                    let band_h = iy1 - iy0;
                    let decoded = hamamatsu_vms_decode_jpeg_rows(
                        &tile.path,
                        tile.scale_denom,
                        band_y,
                        band_h,
                    )?;
                    if decoded.width != tile.width || decoded.height != tile.height {
                        return Err(BioFormatsError::Format(format!(
                            "Hamamatsu VMS tile dimensions changed for {}",
                            tile.path.display()
                        )));
                    }
                    if band_y < decoded.y || band_y + band_h > decoded.y + decoded.rows {
                        return Err(BioFormatsError::Format(format!(
                            "Hamamatsu VMS tile restart band does not cover requested rows for {}",
                            tile.path.display()
                        )));
                    }
                    let copy_w = (ix1 - ix0) as usize;
                    let src_x = (ix0 - tile.x) as usize;
                    let src_y = (band_y - decoded.y) as usize;
                    let dst_x = (ix0 - x) as usize;
                    let dst_y = (iy0 - y) as usize;
                    let src_stride = tile.width as usize * 3;
                    for row in 0..(iy1 - iy0) as usize {
                        let src = (src_y + row) * src_stride + src_x * 3;
                        let dst = (dst_y + row) * row_bytes + dst_x * 3;
                        out[dst..dst + copy_w * 3]
                            .copy_from_slice(&decoded.rgb[src..src + copy_w * 3]);
                    }
                }
            }
            HamamatsuVmsPixels::Jpeg(path) => {
                let decoded = hamamatsu_vms_decode_jpeg_rows(path, 1, y, h)?;
                if decoded.width != meta.size_x || decoded.height != meta.size_y {
                    return Err(BioFormatsError::Format(format!(
                        "Hamamatsu VMS associated image dimensions changed for {}",
                        path.display()
                    )));
                }
                if y < decoded.y || y + h > decoded.y + decoded.rows {
                    return Err(BioFormatsError::Format(format!(
                        "Hamamatsu VMS associated image restart band does not cover requested rows for {}",
                        path.display()
                    )));
                }
                let src_stride = decoded.width as usize * 3;
                let src_x = x as usize;
                let src_y = (y - decoded.y) as usize;
                for row in 0..h as usize {
                    let src = (src_y + row) * src_stride + src_x * 3;
                    let dst = row * row_bytes;
                    out[dst..dst + row_bytes].copy_from_slice(&decoded.rgb[src..src + row_bytes]);
                }
            }
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .and_then(|s| s.metadata.get(self.current_resolution))
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        self.series
            .get(self.current_series)
            .map(|s| s.metadata.len())
            .unwrap_or(0)
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.series.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if level >= self.resolution_count() {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            self.current_resolution = level;
            Ok(())
        }
    }

    fn resolution(&self) -> usize {
        self.current_resolution
    }
}

#[cfg(test)]
mod hamamatsu_vms_tests {
    use super::HamamatsuVmsReader;
    use crate::common::error::BioFormatsError;
    use crate::common::metadata::{MetadataLevel, MetadataOptions, MetadataValue};
    use crate::common::ome_metadata::OmeAnnotation;
    use crate::common::pixel_type::PixelType;
    use crate::common::reader::FormatReader;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("bioformats_hamamatsu_vms_{name}"))
    }

    fn write_rgb_jpeg(path: &std::path::Path, rgb: [u8; 3]) {
        write_rgb_jpeg_pixels(path, 1, 1, &rgb);
    }

    fn write_rgb_jpeg_pixels(path: &std::path::Path, width: u32, height: u32, rgb: &[u8]) {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100)
            .encode(rgb, width, height, image::ColorType::Rgb8.into())
            .unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn write_gray_jpeg_pixels(path: &std::path::Path, width: u32, height: u32, gray: &[u8]) {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100)
            .encode(gray, width, height, image::ColorType::L8.into())
            .unwrap();
        std::fs::write(path, bytes).unwrap();
    }

    fn write_rgb_jpeg_with_marker_segments(path: &std::path::Path, rgb: [u8; 3]) {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100)
            .encode(&rgb, 1, 1, image::ColorType::Rgb8.into())
            .unwrap();
        let mut segments = Vec::new();

        let icc = b"synthetic-icc-profile";
        let app2_len = 2 + 14 + icc.len();
        segments.extend_from_slice(&[0xff, 0xe2]);
        segments.extend_from_slice(&(app2_len as u16).to_be_bytes());
        segments.extend_from_slice(b"ICC_PROFILE\0");
        segments.extend_from_slice(&[1, 1]);
        segments.extend_from_slice(icc);

        segments.extend_from_slice(&[0xff, 0xee, 0x00, 0x0e]);
        segments.extend_from_slice(b"Adobe");
        segments.extend_from_slice(&100u16.to_be_bytes());
        segments.extend_from_slice(&0u16.to_be_bytes());
        segments.extend_from_slice(&0u16.to_be_bytes());
        segments.push(1);

        segments.extend_from_slice(&[0xff, 0xdd, 0x00, 0x04]);
        segments.extend_from_slice(&4u16.to_be_bytes());

        bytes.splice(2..2, segments);
        std::fs::write(path, bytes).unwrap();
    }

    fn write_rgb_jpeg_with_invalid_icc_sequence(path: &std::path::Path, rgb: [u8; 3]) {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100)
            .encode(&rgb, 1, 1, image::ColorType::Rgb8.into())
            .unwrap();
        let icc = b"out-of-range";
        let app2_len = 2 + 14 + icc.len();
        let mut segment = Vec::new();
        segment.extend_from_slice(&[0xff, 0xe2]);
        segment.extend_from_slice(&(app2_len as u16).to_be_bytes());
        segment.extend_from_slice(b"ICC_PROFILE\0");
        segment.extend_from_slice(&[2, 1]);
        segment.extend_from_slice(icc);
        bytes.splice(2..2, segment);
        std::fs::write(path, bytes).unwrap();
    }

    fn write_rgb_jpeg_with_duplicate_icc_sequence(path: &std::path::Path, rgb: [u8; 3]) {
        let mut bytes = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, 100)
            .encode(&rgb, 1, 1, image::ColorType::Rgb8.into())
            .unwrap();
        let mut segments = Vec::new();
        for icc in [b"first".as_slice(), b"second".as_slice()] {
            let app2_len = 2 + 14 + icc.len();
            segments.extend_from_slice(&[0xff, 0xe2]);
            segments.extend_from_slice(&(app2_len as u16).to_be_bytes());
            segments.extend_from_slice(b"ICC_PROFILE\0");
            segments.extend_from_slice(&[1, 1]);
            segments.extend_from_slice(icc);
        }
        bytes.splice(2..2, segments);
        std::fs::write(path, bytes).unwrap();
    }

    fn decode_scaled_jpeg(path: &std::path::Path, width: u16, height: u16) -> Vec<u8> {
        let file = std::fs::File::open(path).unwrap();
        let mut decoder = jpeg_decoder::Decoder::new(file);
        assert_eq!(decoder.scale(width, height).unwrap(), (width, height));
        decoder.decode().unwrap()
    }

    #[test]
    fn hamamatsu_vms_decodes_small_jpeg_tile_grid() {
        let path = temp_path("index.vms");
        let tile0 = temp_path("tile0.jpg");
        let tile1 = temp_path("tile1.jpg");
        write_rgb_jpeg(&tile0, [240, 10, 20]);
        write_rgb_jpeg(&tile1, [30, 220, 40]);
        std::fs::write(
            &path,
            format!(
                "NoLayers=1\nNoJpegColumns=2\nNoJpegRows=1\nImageFile={}\nImageFile(1,0)={}\nPhysicalWidth=2\nPhysicalHeight=1\n",
                tile0.file_name().unwrap().to_string_lossy(),
                tile1.file_name().unwrap().to_string_lossy()
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y, meta.size_c), (2, 1, 3));
        assert!(!meta.is_interleaved);
        assert!(!meta.thumbnail);
        assert_eq!(reader.series_count(), 1);

        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(plane.len(), 6);
        assert!(plane[0] > plane[1] && plane[0] > plane[2], "{plane:?}");
        assert!(plane[4] > plane[3] && plane[4] > plane[5], "{plane:?}");
        let region = reader.open_bytes_region(0, 1, 0, 1, 1).unwrap();
        assert_eq!(region, plane[3..6]);

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile0);
        let _ = std::fs::remove_file(tile1);
    }

    #[test]
    fn hamamatsu_vms_reads_layers_macro_map_and_opt_metadata() {
        let path = temp_path("advanced.vms");
        let opt = temp_path("advanced.opt");
        let tile0 = temp_path("advanced_l0.jpg");
        let tile1 = temp_path("advanced_l1.jpg");
        let macro_image = temp_path("advanced_macro.jpg");
        let map_image = temp_path("advanced_map.jpg");
        write_rgb_jpeg(&tile0, [240, 10, 20]);
        write_rgb_jpeg(&tile1, [20, 30, 240]);
        write_rgb_jpeg_pixels(&macro_image, 2, 1, &[10, 200, 20, 210, 20, 30]);
        write_rgb_jpeg(&map_image, [80, 90, 100]);
        std::fs::write(&opt, b"[Pyramid]\nTileWidth=1024\n").unwrap();
        std::fs::write(
            &path,
            format!(
                concat!(
                    "NoLayers=2\n",
                    "NoJpegColumns=1\n",
                    "NoJpegRows=1\n",
                    "ImageFile={}\n",
                    "ImageFile(1,0,0)={}\n",
                    "MacroImage={}\n",
                    "MapFile={}\n",
                    "OptimisationFile={}\n",
                    "PhysicalWidth=4\n",
                    "PhysicalHeight=2\n",
                    "PhysicalMacroWidth=8\n",
                    "PhysicalMacroHeight=2\n"
                ),
                tile0.file_name().unwrap().to_string_lossy(),
                tile1.file_name().unwrap().to_string_lossy(),
                macro_image.file_name().unwrap().to_string_lossy(),
                map_image.file_name().unwrap().to_string_lossy(),
                opt.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 3);
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y, meta.size_c), (1, 1, 3));
        assert_eq!(meta.image_count, 2);
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        assert!(matches!(
            meta.series_metadata.get("VMS opt.tilewidth"),
            Some(MetadataValue::String(v)) if v == "1024"
        ));
        assert!(matches!(
            meta.series_metadata.get("VMS physical_size_x"),
            Some(MetadataValue::Float(v)) if (*v - 4.0).abs() < 0.0001
        ));

        let layer0 = reader.open_bytes(0).unwrap();
        let layer1 = reader.open_bytes(1).unwrap();
        assert!(layer0[0] > layer0[2], "{layer0:?}");
        assert!(layer1[2] > layer1[0], "{layer1:?}");
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));

        reader.set_series(1).unwrap();
        assert_eq!(reader.series(), 1);
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (2, 1));
        assert_eq!(reader.metadata().image_count, 1);
        assert!(!reader.metadata().is_interleaved);
        assert!(reader.metadata().thumbnail);
        assert!(matches!(
            reader.metadata().series_metadata.get("VMS series kind"),
            Some(MetadataValue::String(v)) if v == "macro"
        ));
        let macro_plane = reader.open_bytes(0).unwrap();
        assert_eq!(macro_plane.len(), 6);

        reader.set_series(2).unwrap();
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (1, 1));
        assert!(!reader.metadata().is_interleaved);
        assert!(reader.metadata().thumbnail);
        assert!(matches!(
            reader.metadata().series_metadata.get("VMS series kind"),
            Some(MetadataValue::String(v)) if v == "map"
        ));
        assert_eq!(reader.open_bytes_region(0, 0, 0, 1, 1).unwrap().len(), 3);

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(opt);
        let _ = std::fs::remove_file(tile0);
        let _ = std::fs::remove_file(tile1);
        let _ = std::fs::remove_file(macro_image);
        let _ = std::fs::remove_file(map_image);
    }

    #[test]
    fn hamamatsu_vms_populates_image_names_physical_sizes_and_objective() {
        let path = temp_path("ome.vms");
        let tile0 = temp_path("ome_tile.jpg");
        let macro_image = temp_path("ome_macro.jpg");
        let map_image = temp_path("ome_map.jpg");
        write_rgb_jpeg_pixels(&tile0, 2, 1, &[240, 10, 20, 30, 220, 40]);
        write_rgb_jpeg_pixels(&macro_image, 2, 1, &[10, 200, 20, 210, 20, 30]);
        write_rgb_jpeg(&map_image, [80, 90, 100]);
        std::fs::write(
            &path,
            format!(
                concat!(
                    "NoLayers=1\n",
                    "NoJpegColumns=2\n",
                    "NoJpegRows=1\n",
                    "ImageFile={}\n",
                    "ImageFile(1,0)={}\n",
                    "MacroImage={}\n",
                    "MapFile={}\n",
                    "PhysicalWidth=4\n",
                    "PhysicalHeight=1\n",
                    "PhysicalMacroWidth=8\n",
                    "PhysicalMacroHeight=2\n",
                    "SourceLens=20\n"
                ),
                tile0.file_name().unwrap().to_string_lossy(),
                tile0.file_name().unwrap().to_string_lossy(),
                macro_image.file_name().unwrap().to_string_lossy(),
                map_image.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let base_name = path.file_name().unwrap().to_string_lossy().into_owned();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();

        // Series 0: full resolution name, physical sizes, and objective magnification.
        let ome0 = reader.ome_metadata().unwrap();
        let image0 = &ome0.images[0];
        assert_eq!(
            image0.name.as_deref(),
            Some(format!("{base_name} full resolution").as_str())
        );
        assert!((image0.physical_size_x.unwrap() - 1.0).abs() < 0.0001);
        assert!((image0.physical_size_y.unwrap() - 1.0).abs() < 0.0001);
        assert_eq!(image0.instrument_ref, Some(0));
        assert_eq!(image0.objective_ref, Some(0));
        assert_eq!(ome0.instruments.len(), 1);
        assert!(
            (ome0.instruments[0].objectives[0]
                .nominal_magnification
                .unwrap()
                - 20.0)
                .abs()
                < 0.0001
        );

        // Series 1: macro name, physical sizes, but no instrument/objective.
        reader.set_series(1).unwrap();
        let ome1 = reader.ome_metadata().unwrap();
        let image1 = &ome1.images[0];
        assert_eq!(
            image1.name.as_deref(),
            Some(format!("{base_name} macro").as_str())
        );
        assert!((image1.physical_size_x.unwrap() - 4.0).abs() < 0.0001);
        assert!((image1.physical_size_y.unwrap() - 2.0).abs() < 0.0001);
        assert!(image1.instrument_ref.is_none());
        assert!(ome1.instruments.is_empty());

        // Series 2: map name, no physical sizes, no instrument.
        reader.set_series(2).unwrap();
        let ome2 = reader.ome_metadata().unwrap();
        let image2 = &ome2.images[0];
        assert_eq!(
            image2.name.as_deref(),
            Some(format!("{base_name} map").as_str())
        );
        assert!(image2.physical_size_x.is_none());
        assert!(ome2.instruments.is_empty());

        // MINIMUM metadata level suppresses physical sizes and the objective,
        // but the image name is still set (matching Java initFile).
        let mut minimal = HamamatsuVmsReader::new();
        minimal.set_metadata_options(MetadataOptions {
            level: MetadataLevel::Minimal,
            original_metadata: true,
        });
        minimal.set_id(&path).unwrap();
        let min_ome = minimal.ome_metadata().unwrap();
        let min_image = &min_ome.images[0];
        assert_eq!(
            min_image.name.as_deref(),
            Some(format!("{base_name} full resolution").as_str())
        );
        assert!(min_image.physical_size_x.is_none());
        assert!(min_image.physical_size_y.is_none());
        assert!(min_image.instrument_ref.is_none());
        assert!(min_ome.instruments.is_empty());

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile0);
        let _ = std::fs::remove_file(macro_image);
        let _ = std::fs::remove_file(map_image);
    }

    #[test]
    fn hamamatsu_vms_reads_layer_key_with_column_layer_row_order() {
        let path = temp_path("layer_key_variants.vms");
        let tile0 = temp_path("layer_key_variants_l0.jpg");
        let tile1 = temp_path("layer_key_variants_l1.jpg");
        write_rgb_jpeg(&tile0, [240, 10, 20]);
        write_rgb_jpeg(&tile1, [20, 30, 240]);
        std::fs::write(
            &path,
            format!(
                concat!(
                    "NoLayers=2\n",
                    "NoJpegColumns=1\n",
                    "NoJpegRows=1\n",
                    "ImageFile={}\n",
                    "ImageFile(0,1,0)={}\n"
                ),
                tile0.file_name().unwrap().to_string_lossy(),
                tile1.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.metadata().image_count, 2);
        let layer0 = reader.open_bytes(0).unwrap();
        let layer1 = reader.open_bytes(1).unwrap();
        assert!(layer0[0] > layer0[2], "{layer0:?}");
        assert!(layer1[2] > layer1[0], "{layer1:?}");

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile0);
        let _ = std::fs::remove_file(tile1);
    }

    #[test]
    fn hamamatsu_vms_ignores_missing_optimisation_file_like_java() {
        let path = temp_path("missing_optimisation_file.vms");
        let tile = temp_path("missing_optimisation_file.jpg");
        write_rgb_jpeg(&tile, [240, 10, 20]);
        std::fs::write(
            &path,
            format!(
                concat!(
                    "NoLayers=1\n",
                    "NoJpegColumns=1\n",
                    "NoJpegRows=1\n",
                    "ImageFile={}\n",
                    "OptimisationFile=does-not-exist.opt\n"
                ),
                tile.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 1);
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (1, 1));
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("VMS optimisationfile"),
            Some(MetadataValue::String(v)) if v == "does-not-exist.opt"
        ));

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile);
    }

    #[test]
    fn hamamatsu_vms_reads_spaced_and_alternate_tile_key_variants() {
        let path = temp_path("alternate_tile_key_variants.vms");
        let tile0 = temp_path("alternate_tile_key_variants_0.jpg");
        let tile1 = temp_path("alternate_tile_key_variants_1.jpg");
        let tile2 = temp_path("alternate_tile_key_variants_2.jpg");
        write_rgb_jpeg(&tile0, [240, 10, 20]);
        write_rgb_jpeg(&tile1, [30, 220, 40]);
        write_rgb_jpeg(&tile2, [20, 30, 240]);
        std::fs::write(
            &path,
            format!(
                concat!(
                    "No Layers=1\n",
                    "No Jpeg Columns=3\n",
                    "No Jpeg Rows=1\n",
                    "Image File={}\n",
                    "ImageFile[1,0]={}\n",
                    "ImageFile_2_0={}\n"
                ),
                tile0.file_name().unwrap().to_string_lossy(),
                tile1.file_name().unwrap().to_string_lossy(),
                tile2.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (3, 1));
        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(plane.len(), 9);
        assert!(plane[0] > plane[1] && plane[0] > plane[2], "{plane:?}");
        assert!(plane[4] > plane[3] && plane[4] > plane[5], "{plane:?}");
        assert!(plane[8] > plane[6] && plane[8] > plane[7], "{plane:?}");

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile0);
        let _ = std::fs::remove_file(tile1);
        let _ = std::fs::remove_file(tile2);
    }

    #[test]
    fn hamamatsu_vms_reads_single_row_numeric_suffix_tile_aliases() {
        let path = temp_path("numeric_suffix_tile_aliases.vms");
        let tile0 = temp_path("numeric_suffix_tile_aliases_0.jpg");
        let tile1 = temp_path("numeric_suffix_tile_aliases_1.jpg");
        write_rgb_jpeg(&tile0, [240, 10, 20]);
        write_rgb_jpeg(&tile1, [20, 30, 240]);
        std::fs::write(
            &path,
            format!(
                concat!(
                    "NoLayers=1\n",
                    "NoJpegColumns=2\n",
                    "NoJpegRows=1\n",
                    "ImageFile0={}\n",
                    "ImageFile1={}\n"
                ),
                tile0.file_name().unwrap().to_string_lossy(),
                tile1.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (2, 1));
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("VMS tile key aliases supported"),
            Some(MetadataValue::String(v))
                if v.contains("single-row zero-based numeric suffix")
        ));
        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(plane.len(), 6);
        assert!(plane[0] > plane[2], "{plane:?}");
        assert!(plane[5] > plane[3], "{plane:?}");

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile0);
        let _ = std::fs::remove_file(tile1);
    }

    #[test]
    fn hamamatsu_vms_reads_image_name_alias_and_windows_sidecar_basename() {
        let path = temp_path("image_name_alias.vms");
        let tile0 = temp_path("image_name_alias_0.jpg");
        let tile1 = temp_path("image_name_alias_1.jpg");
        write_rgb_jpeg(&tile0, [240, 10, 20]);
        write_rgb_jpeg(&tile1, [20, 30, 240]);
        std::fs::write(
            &path,
            format!(
                concat!(
                    "NoLayers=1\n",
                    "NoJpegColumns=2\n",
                    "NoJpegRows=1\n",
                    "ImageName=\"C:\\scan\\{}\"\n",
                    "ImageFileName(1,0)='D:\\other\\{}'\n"
                ),
                tile0.file_name().unwrap().to_string_lossy(),
                tile1.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (2, 1));
        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(plane.len(), 6);
        assert!(plane[0] > plane[2], "{plane:?}");
        assert!(plane[5] > plane[3], "{plane:?}");

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile0);
        let _ = std::fs::remove_file(tile1);
    }

    #[test]
    fn hamamatsu_vms_reads_two_index_row_column_tile_key_variant() {
        let path = temp_path("row_column_tile_key_variant.vms");
        let tile0 = temp_path("row_column_tile_key_variant_0.jpg");
        let tile1 = temp_path("row_column_tile_key_variant_1.jpg");
        write_rgb_jpeg(&tile0, [240, 10, 20]);
        write_rgb_jpeg(&tile1, [20, 30, 240]);
        std::fs::write(
            &path,
            format!(
                concat!(
                    "NoLayers=1\n",
                    "NoJpegColumns=1\n",
                    "NoJpegRows=2\n",
                    "ImageFile={}\n",
                    "ImageFile(1,0)={}\n"
                ),
                tile0.file_name().unwrap().to_string_lossy(),
                tile1.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (1, 2));
        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(plane.len(), 6);
        assert!(plane[0] > plane[2], "{plane:?}");
        assert!(plane[5] > plane[3], "{plane:?}");

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile0);
        let _ = std::fs::remove_file(tile1);
    }

    #[test]
    fn hamamatsu_vms_reads_label_based_row_column_tile_key_variants() {
        let path = temp_path("label_based_tile_key_variant.vms");
        let tile0 = temp_path("label_based_tile_key_variant_0.jpg");
        let tile1 = temp_path("label_based_tile_key_variant_1.jpg");
        write_rgb_jpeg(&tile0, [240, 10, 20]);
        write_rgb_jpeg(&tile1, [20, 30, 240]);
        std::fs::write(
            &path,
            format!(
                concat!(
                    "NoLayers=1\n",
                    "NoJpegColumns=2\n",
                    "NoJpegRows=1\n",
                    "ImageFileRow0Column0={}\n",
                    "ImageFileColumn1Row0={}\n"
                ),
                tile0.file_name().unwrap().to_string_lossy(),
                tile1.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (2, 1));
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("VMS tile key aliases supported"),
            Some(MetadataValue::String(v)) if v.contains("label-based row/column")
        ));
        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(plane.len(), 6);
        assert!(plane[0] > plane[2], "{plane:?}");
        assert!(plane[5] > plane[3], "{plane:?}");

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile0);
        let _ = std::fs::remove_file(tile1);
    }

    #[test]
    fn hamamatsu_vms_reads_macro_and_map_image_aliases_once() {
        let path = temp_path("associated_aliases.vms");
        let tile = temp_path("associated_aliases_tile.jpg");
        let macro_image = temp_path("associated_aliases_macro.jpg");
        let map_image = temp_path("associated_aliases_map.jpg");
        write_rgb_jpeg(&tile, [240, 10, 20]);
        write_rgb_jpeg(&macro_image, [20, 220, 30]);
        write_rgb_jpeg(&map_image, [20, 30, 240]);
        std::fs::write(
            &path,
            format!(
                concat!(
                    "NoLayers=1\n",
                    "NoJpegColumns=1\n",
                    "NoJpegRows=1\n",
                    "ImageFile={}\n",
                    "MacroImageFile={}\n",
                    "MacroImageName={}\n",
                    "MapImageFile={}\n"
                ),
                tile.file_name().unwrap().to_string_lossy(),
                macro_image.file_name().unwrap().to_string_lossy(),
                macro_image.file_name().unwrap().to_string_lossy(),
                map_image.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 3);
        reader.set_series(1).unwrap();
        assert!(matches!(
            reader.metadata().series_metadata.get("VMS series kind"),
            Some(MetadataValue::String(v)) if v == "macro"
        ));
        let macro_plane = reader.open_bytes(0).unwrap();
        assert!(macro_plane[1] > macro_plane[0], "{macro_plane:?}");

        reader.set_series(2).unwrap();
        assert!(matches!(
            reader.metadata().series_metadata.get("VMS series kind"),
            Some(MetadataValue::String(v)) if v == "map"
        ));
        let map_plane = reader.open_bytes(0).unwrap();
        assert!(map_plane[2] > map_plane[0], "{map_plane:?}");

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile);
        let _ = std::fs::remove_file(macro_image);
        let _ = std::fs::remove_file(map_image);
    }

    #[test]
    fn hamamatsu_vms_records_jpeg_marker_metadata_without_applying_icc() {
        let path = temp_path("jpeg_marker_metadata.vms");
        let tile = temp_path("jpeg_marker_metadata.jpg");
        write_rgb_jpeg_with_marker_segments(&tile, [240, 10, 20]);
        std::fs::write(
            &path,
            format!(
                "NoLayers=1\nNoJpegColumns=1\nNoJpegRows=1\nImageFile={}\n",
                tile.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            metadata.get("VMS tile JPEG color model"),
            Some(MetadataValue::String(v)) if v == "ycbcr"
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG color conversion"),
            Some(MetadataValue::String(v)) if v == "decoder ycbcr to rgb"
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG color management"),
            Some(MetadataValue::String(v)) if v == "icc profile preserved but not applied"
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG progressive"),
            Some(MetadataValue::Bool(false))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG lossless"),
            Some(MetadataValue::Bool(false))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG arithmetic coding"),
            Some(MetadataValue::Bool(false))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG restart interval"),
            Some(MetadataValue::Int(4))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG SOF family"),
            Some(MetadataValue::String(v)) if v == "baseline dct"
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG Adobe transform"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC markers present"),
            Some(MetadataValue::Bool(true))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC profile complete"),
            Some(MetadataValue::Bool(true))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC profile bytes"),
            Some(MetadataValue::Int(21))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC profile applied"),
            Some(MetadataValue::Bool(false))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG decoded pixel formats"),
            Some(MetadataValue::String(v))
                if v == "RGB24, L8 expanded to RGB, CMYK32 converted to RGB"
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG unsupported color handling"),
            Some(MetadataValue::String(v))
                if v == "other decoder pixel formats and ICC/profile transforms are reported but not applied"
        ));

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile);
    }

    #[test]
    fn hamamatsu_vms_reports_bounded_tile_alias_capabilities() {
        let path = temp_path("tile_alias_diagnostics.vms");
        let tile = temp_path("tile_alias_diagnostics.jpg");
        write_rgb_jpeg(&tile, [240, 10, 20]);
        std::fs::write(
            &path,
            format!(
                "NoLayers=1\nNoJpegColumns=1\nNoJpegRows=1\nImageFileName={}\n",
                tile.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            metadata.get("VMS tile key aliases supported"),
            Some(MetadataValue::String(v))
                if v.contains("ImageFileName") && v.contains("ImagePath")
        ));
        assert!(matches!(
            metadata.get("VMS associated image key aliases supported"),
            Some(MetadataValue::String(v))
                if v.contains("MacroImageFile") && v.contains("MapImageFile")
        ));
        assert!(matches!(
            metadata.get("VMS unsupported tile key alias handling"),
            Some(MetadataValue::String(v))
                if v.contains("unrecognized tile aliases are not guessed")
        ));

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile);
    }

    #[test]
    fn hamamatsu_vms_projects_original_metadata_to_ome_annotation() {
        let path = temp_path("ome_original_metadata.vms");
        let tile = temp_path("ome_original_metadata.jpg");
        write_rgb_jpeg(&tile, [240, 10, 20]);
        std::fs::write(
            &path,
            format!(
                "NoLayers=1\nNoJpegColumns=1\nNoJpegRows=1\nImageFile={}\nPhysicalWidth=4\nPhysicalHeight=2\n",
                tile.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        let ome = reader.ome_metadata().expect("OME metadata");
        assert_eq!(ome.images.len(), 1);
        let original_metadata = ome
            .annotations
            .iter()
            .find_map(|annotation| match annotation {
                OmeAnnotation::MapAnnotation {
                    namespace, values, ..
                } if namespace.as_deref() == Some("openmicroscopy.org/OriginalMetadata") => {
                    Some(values)
                }
                _ => None,
            })
            .expect("original metadata annotation");

        assert!(original_metadata
            .iter()
            .any(|(key, value)| key == "Image" && value == "Image:0"));
        assert!(original_metadata
            .iter()
            .any(|(key, value)| key == "VMS physicalwidth" && value == "4"));
        assert!(original_metadata
            .iter()
            .any(|(key, value)| { key == "VMS tile JPEG color model" && value == "ycbcr" }));

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile);
    }

    #[test]
    fn hamamatsu_vms_reports_incomplete_icc_sequence_without_applying_profile() {
        let path = temp_path("jpeg_incomplete_icc.vms");
        let tile = temp_path("jpeg_incomplete_icc.jpg");
        write_rgb_jpeg_with_invalid_icc_sequence(&tile, [240, 10, 20]);
        std::fs::write(
            &path,
            format!(
                "NoLayers=1\nNoJpegColumns=1\nNoJpegRows=1\nImageFile={}\n",
                tile.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC profile complete"),
            Some(MetadataValue::Bool(false))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG color management"),
            Some(MetadataValue::String(v))
                if v == "incomplete icc profile markers preserved but not applied"
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC markers present"),
            Some(MetadataValue::Bool(true))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC declared chunks"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC seen chunks"),
            Some(MetadataValue::Int(0))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC missing chunks"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC invalid chunks"),
            Some(MetadataValue::Int(1))
        ));
        assert!(!metadata.contains_key("VMS tile JPEG ICC profile"));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC profile applied"),
            Some(MetadataValue::Bool(false))
        ));

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile);
    }

    #[test]
    fn hamamatsu_vms_reports_duplicate_icc_sequence_without_complete_profile() {
        let path = temp_path("jpeg_duplicate_icc.vms");
        let tile = temp_path("jpeg_duplicate_icc.jpg");
        write_rgb_jpeg_with_duplicate_icc_sequence(&tile, [240, 10, 20]);
        std::fs::write(
            &path,
            format!(
                "NoLayers=1\nNoJpegColumns=1\nNoJpegRows=1\nImageFile={}\n",
                tile.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC profile complete"),
            Some(MetadataValue::Bool(false))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG color management"),
            Some(MetadataValue::String(v))
                if v == "incomplete icc profile markers preserved but not applied"
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC markers present"),
            Some(MetadataValue::Bool(true))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC declared chunks"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC seen chunks"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC missing chunks"),
            Some(MetadataValue::Int(0))
        ));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC duplicate chunks"),
            Some(MetadataValue::Int(1))
        ));
        assert!(!metadata.contains_key("VMS tile JPEG ICC profile"));
        assert!(!metadata.contains_key("VMS tile JPEG ICC profile bytes"));
        assert!(matches!(
            metadata.get("VMS tile JPEG ICC profile applied"),
            Some(MetadataValue::Bool(false))
        ));

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile);
    }

    #[test]
    fn hamamatsu_vms_expands_grayscale_jpeg_tiles_to_rgb() {
        let path = temp_path("gray_tile.vms");
        let tile = temp_path("gray_tile.jpg");
        write_gray_jpeg_pixels(&tile, 2, 1, &[40, 210]);
        std::fs::write(
            &path,
            format!(
                "NoLayers=1\nNoJpegColumns=1\nNoJpegRows=1\nImageFile={}\n",
                tile.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (2, 1));
        assert_eq!(reader.metadata().size_c, 3);
        assert!(reader.metadata().is_rgb);
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("VMS tile JPEG color model"),
            Some(MetadataValue::String(v)) if v == "grayscale"
        ));
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("VMS tile JPEG color conversion"),
            Some(MetadataValue::String(v)) if v == "grayscale expanded to rgb"
        ));
        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(plane.len(), 6);
        assert_eq!(plane[0], plane[1]);
        assert_eq!(plane[1], plane[2]);
        assert_eq!(plane[3], plane[4]);
        assert_eq!(plane[4], plane[5]);
        assert!(plane[3] > plane[0], "{plane:?}");

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile);
    }

    #[test]
    fn hamamatsu_vms_converts_cmyk_jpeg_pixels_to_rgb() {
        let rgb = super::hamamatsu_vms_cmyk_to_rgb(&[
            0, 255, 255, 0, 255, 0, 255, 0, 255, 255, 0, 0, 255, 255, 255, 128,
        ]);
        assert_eq!(
            rgb,
            vec![
                255, 0, 0, // cyan ink absent, magenta/yellow present
                0, 255, 0, // magenta ink absent
                0, 0, 255, // yellow ink absent
                0, 0, 0, // black ink only
            ]
        );
    }

    #[test]
    fn hamamatsu_vms_reports_unsupported_jpeg_pixel_format_policy() {
        let path = temp_path("unsupported_l16_pixel_format.jpg");
        let err = super::hamamatsu_vms_unsupported_jpeg_pixel_format_error(
            jpeg_decoder::PixelFormat::L16,
            &path,
        );
        assert!(
            matches!(
                err,
                BioFormatsError::UnsupportedFormat(ref message)
                    if message.contains("L16")
                        && message.contains("supported decoder pixel formats are RGB24, L8 expanded to RGB, and CMYK32 converted to RGB")
                        && message.contains("ICC/profile transforms are preserved as metadata only and are not applied")
                        && message.contains("unsupported_l16_pixel_format.jpg")
            ),
            "{err:?}"
        );
    }

    #[test]
    fn hamamatsu_vms_decodes_lower_resolution_pyramid_tiles() {
        let path = temp_path("pyramid.vms");
        let opt = temp_path("pyramid.opt");
        let tile0 = temp_path("pyramid_full0.jpg");
        let tile1 = temp_path("pyramid_full1.jpg");
        let low = temp_path("pyramid_low.jpg");
        write_rgb_jpeg_pixels(&tile0, 1, 2, &[240, 10, 20, 230, 20, 10]);
        write_rgb_jpeg_pixels(&tile1, 1, 2, &[30, 220, 40, 20, 210, 30]);
        write_rgb_jpeg(&low, [20, 30, 240]);
        std::fs::write(
            &opt,
            format!(
                concat!(
                    "PyramidLevels=2\n",
                    "PyramidLevel1NoJpegColumns=1\n",
                    "PyramidLevel1NoJpegRows=1\n",
                    "PyramidLevel1ImageFile={}\n"
                ),
                low.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();
        std::fs::write(
            &path,
            format!(
                concat!(
                    "NoLayers=1\n",
                    "NoJpegColumns=2\n",
                    "NoJpegRows=1\n",
                    "ImageFile={}\n",
                    "ImageFile(1,0)={}\n",
                    "OptimisationFile={}\n",
                    "PhysicalWidth=4\n",
                    "PhysicalHeight=2\n"
                ),
                tile0.file_name().unwrap().to_string_lossy(),
                tile1.file_name().unwrap().to_string_lossy(),
                opt.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.resolution_count(), 2);
        assert_eq!(reader.resolution(), 0);
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (2, 2));
        assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 1).unwrap().len(), 3);

        reader.set_resolution(1).unwrap();
        assert_eq!(reader.resolution(), 1);
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (1, 1));
        assert!(matches!(
            reader.metadata().series_metadata.get("VMS resolution"),
            Some(MetadataValue::Int(1))
        ));
        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(plane.len(), 3);
        assert!(plane[2] > plane[0], "{plane:?}");
        assert!(matches!(
            reader.set_resolution(2),
            Err(BioFormatsError::Format(ref message)) if message.contains("resolution 2 out of range")
        ));

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(opt);
        let _ = std::fs::remove_file(tile0);
        let _ = std::fs::remove_file(tile1);
        let _ = std::fs::remove_file(low);
    }

    #[test]
    fn hamamatsu_vms_infers_declared_pyramid_from_scaled_full_tiles() {
        let path = temp_path("pyramid_inferred.vms");
        let opt = temp_path("pyramid_inferred.opt");
        let tile0 = temp_path("pyramid_inferred.jpg");
        write_rgb_jpeg_pixels(
            &tile0,
            2,
            2,
            &[240, 10, 20, 230, 20, 10, 30, 220, 40, 20, 210, 30],
        );
        let expected_low = decode_scaled_jpeg(&tile0, 1, 1);
        std::fs::write(&opt, b"PyramidLevels=2\n").unwrap();
        std::fs::write(
            &path,
            format!(
                concat!(
                    "NoLayers=1\n",
                    "NoJpegColumns=1\n",
                    "NoJpegRows=1\n",
                    "ImageFile={}\n",
                    "OptimisationFile={}\n"
                ),
                tile0.file_name().unwrap().to_string_lossy(),
                opt.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.resolution_count(), 2);
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (2, 2));
        reader.set_resolution(1).unwrap();
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (1, 1));
        assert_eq!(reader.open_bytes(0).unwrap(), expected_low);

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(opt);
        let _ = std::fs::remove_file(tile0);
    }

    #[test]
    fn hamamatsu_vms_does_not_expose_inexact_inferred_pyramid() {
        let path = temp_path("pyramid_inexact.vms");
        let opt = temp_path("pyramid_inexact.opt");
        let tile0 = temp_path("pyramid_inexact.jpg");
        write_rgb_jpeg_pixels(
            &tile0,
            3,
            2,
            &[
                1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
            ],
        );
        std::fs::write(&opt, b"PyramidLevels=2\n").unwrap();
        std::fs::write(
            &path,
            format!(
                concat!(
                    "NoLayers=1\n",
                    "NoJpegColumns=1\n",
                    "NoJpegRows=1\n",
                    "ImageFile={}\n",
                    "OptimisationFile={}\n"
                ),
                tile0.file_name().unwrap().to_string_lossy(),
                opt.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.resolution_count(), 1);
        assert!(matches!(
            reader.set_resolution(1),
            Err(BioFormatsError::Format(ref message)) if message.contains("resolution 1 out of range")
        ));

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(opt);
        let _ = std::fs::remove_file(tile0);
    }

    #[test]
    fn hamamatsu_vms_reports_missing_layer_tiles() {
        let path = temp_path("missing_layer.vms");
        let tile0 = temp_path("missing_layer_l0.jpg");
        write_rgb_jpeg(&tile0, [1, 2, 3]);
        std::fs::write(
            &path,
            format!(
                "NoLayers=2\nNoJpegColumns=1\nNoJpegRows=1\nImageFile={}\n",
                tile0.file_name().unwrap().to_string_lossy(),
            ),
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(
                err,
                BioFormatsError::UnsupportedFormat(ref message)
                    if message.contains("missing layer 1 imagefile")
            ),
            "{err:?}"
        );

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(tile0);
    }

    #[test]
    fn hamamatsu_vms_rejects_fake_text_without_fake_metadata() {
        let path = temp_path("fake.vmu");
        std::fs::write(&path, b"not a real image").unwrap();

        let mut reader = HamamatsuVmsReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("Not a Hamamatsu VMS/VMU")),
            "{err:?}"
        );
        assert_eq!(reader.series_count(), 0);

        let _ = std::fs::remove_file(path);
    }
}

// ---------------------------------------------------------------------------
// 6. Cellomics HCS
// ---------------------------------------------------------------------------

/// Cellomics C01 format (`.c01` / `.dib`).
///
/// Ported from the upstream Java `CellomicsReader`. A `.c01` file is a
/// zlib-compressed payload: the first 4 bytes are the C01 magic, and the
/// remainder is a zlib (Deflate-with-header) stream. The decompressed payload
/// is a DIB-style bitmap: at offset 4 the 32-bit width and height (LE), then
/// 16-bit plane count and bit depth, a 32-bit compression code, and pixel data
/// starting at offset 52. A `.dib` file is the same layout but not compressed.
pub struct CellomicsReader {
    path: Option<PathBuf>,
    metas: Vec<ImageMetadata>,
    current_series: usize,
    pixel_offset: u64,
    /// Decoded (decompressed for .c01, raw for .dib) file bytes.
    data: Vec<u8>,
    series_sources: Vec<Vec<CellomicsDecodedSource>>,
    series_planes: Vec<Vec<CellomicsPlaneSource>>,
}

impl CellomicsReader {
    pub fn new() -> Self {
        CellomicsReader {
            path: None,
            metas: Vec::new(),
            current_series: 0,
            pixel_offset: 52,
            data: Vec::new(),
            series_sources: Vec::new(),
            series_planes: Vec::new(),
        }
    }
}

impl Default for CellomicsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_legacy_cellomics_header(data: &[u8]) -> Result<(u32, u32, u32, PixelType, u8, u64)> {
    let w = u16::from_le_bytes([data[4], data[5]]) as u32;
    let h = u16::from_le_bytes([data[6], data[7]]) as u32;
    let bd = u16::from_le_bytes([data[8], data[9]]);
    if w == 0 || h == 0 || w > 32768 || h > 32768 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Cellomics legacy header has missing or invalid image dimensions {w}x{h}"
        )));
    }
    let (pt, bpp) = match bd {
        8 => (PixelType::Uint8, 8u8),
        16 => (PixelType::Uint16, 16u8),
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Cellomics legacy bits per pixel {bd} is not supported"
            )));
        }
    };
    Ok((w, h, 1, pt, bpp, 52))
}

#[derive(Debug, Clone)]
struct CellomicsParsedHeader {
    width: u32,
    height: u32,
    image_count: u32,
    pixel_type: PixelType,
    bits_per_pixel: u8,
    pixel_offset: u64,
    dib_header_size: Option<u32>,
    dib_planes: u32,
    dib_compression: Option<u32>,
    dib_top_down: Option<bool>,
}

#[derive(Debug, Clone)]
struct CellomicsDecodedSource {
    path: PathBuf,
    data: Vec<u8>,
    pixel_offset: u64,
}

#[derive(Debug, Clone)]
struct CellomicsPlaneSource {
    source_index: usize,
    plane_index: u32,
    channel_index: Option<u32>,
}

#[derive(Debug)]
struct CellomicsPlateSeries {
    sources: Vec<CellomicsDecodedSource>,
    planes: Vec<CellomicsPlaneSource>,
    metadata: HashMap<String, MetadataValue>,
}

#[derive(Debug)]
struct CellomicsPlateAssembly {
    series: Vec<CellomicsPlateSeries>,
}

#[derive(Debug)]
struct CellomicsCandidate {
    path: PathBuf,
    metadata: CellomicsFilenameMetadata,
    data: Vec<u8>,
    header: CellomicsParsedHeader,
    row: u32,
    col: u32,
    field: u32,
    channel: u32,
}

fn decode_cellomics_file(path: &Path) -> Result<Vec<u8>> {
    let raw = std::fs::read(path).map_err(BioFormatsError::Io)?;
    let is_c01 = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("c01"))
        .unwrap_or(false);
    if is_c01 {
        if raw.len() < 4 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Cellomics C01 file is too short to contain a magic number".into(),
            ));
        }
        crate::common::codec::decompress_deflate(&raw[4..]).map_err(|_| {
            BioFormatsError::UnsupportedFormat(
                "Cellomics C01 zlib payload could not be decompressed".into(),
            )
        })
    } else {
        Ok(raw)
    }
}

fn parse_cellomics_decoded_header(data: &[u8]) -> Result<CellomicsParsedHeader> {
    let mut dib_header_size_metadata = None;
    let mut dib_planes_metadata = 1;
    let mut dib_compression_metadata = None;
    let mut dib_top_down_metadata = None;

    let (width, height, image_count, pixel_type, bits_per_pixel, pixel_offset) = if data.len() >= 52
    {
        let dib_header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        if dib_header_size >= 40 {
            dib_header_size_metadata = Some(dib_header_size);
            let raw_w = i32::from_le_bytes([data[4], data[5], data[6], data[7]]);
            let raw_h = i32::from_le_bytes([data[8], data[9], data[10], data[11]]);
            let w = raw_w.unsigned_abs();
            let h = raw_h.unsigned_abs();
            let n_planes = u16::from_le_bytes([data[12], data[13]]) as u32;
            let bd = u16::from_le_bytes([data[14], data[15]]);
            let compression = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
            dib_planes_metadata = n_planes;
            dib_compression_metadata = Some(compression);
            dib_top_down_metadata = Some(raw_h < 0);
            if compression != 0 {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Cellomics DIB compressed pixel data is not supported: compression={compression}"
                    )));
            }
            if w == 0 || h == 0 || w > 32768 || h > 32768 {
                return Err(BioFormatsError::InvalidData(format!(
                    "Cellomics DIB has invalid dimensions {w}x{h}"
                )));
            }
            let (pt, bpp) = match bd {
                8 => (PixelType::Uint8, 8u8),
                16 => (PixelType::Uint16, 16u8),
                _ => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Cellomics DIB bits per pixel {bd} is not supported"
                    )));
                }
            };
            let bytes_per_pixel = (bpp / 8) as u64;
            let image_count = n_planes.max(1);
            let plane_bytes = (w as u64)
                .checked_mul(h as u64)
                .and_then(|n| n.checked_mul(bytes_per_pixel))
                .ok_or_else(|| {
                    BioFormatsError::Format("Cellomics DIB plane size overflows".to_string())
                })?;
            let expected = 52u64
                .checked_add(plane_bytes.checked_mul(image_count as u64).ok_or_else(|| {
                    BioFormatsError::Format("Cellomics DIB total pixel size overflows".to_string())
                })?)
                .ok_or_else(|| {
                    BioFormatsError::Format("Cellomics DIB file size overflows".to_string())
                })?;
            if (data.len() as u64) < expected {
                return Err(BioFormatsError::InvalidData(format!(
                    "Cellomics DIB is too short: got {} bytes, expected at least {expected}",
                    data.len()
                )));
            }
            (w, h, image_count, pt, bpp, 52u64)
        } else {
            parse_legacy_cellomics_header(data)?
        }
    } else if data.len() >= 10 {
        parse_legacy_cellomics_header(data)?
    } else {
        return Err(BioFormatsError::UnsupportedFormat(
            "Cellomics header is too short to determine image dimensions".to_string(),
        ));
    };

    let bytes_per_pixel = (bits_per_pixel / 8) as u64;
    let plane_bytes = (width as u64)
        .checked_mul(height as u64)
        .and_then(|n| n.checked_mul(bytes_per_pixel))
        .ok_or_else(|| BioFormatsError::Format("Cellomics plane size overflows".to_string()))?;
    let expected = pixel_offset
        .checked_add(plane_bytes.checked_mul(image_count as u64).ok_or_else(|| {
            BioFormatsError::Format("Cellomics total pixel size overflows".to_string())
        })?)
        .ok_or_else(|| BioFormatsError::Format("Cellomics file size overflows".to_string()))?;
    if (data.len() as u64) < expected {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Cellomics pixel payload is shorter than declared image: got {} bytes, expected at least {expected}",
            data.len()
        )));
    }

    Ok(CellomicsParsedHeader {
        width,
        height,
        image_count,
        pixel_type,
        bits_per_pixel,
        pixel_offset,
        dib_header_size: dib_header_size_metadata,
        dib_planes: dib_planes_metadata,
        dib_compression: dib_compression_metadata,
        dib_top_down: dib_top_down_metadata,
    })
}

fn cellomics_plate_prefix(path: &Path) -> Option<String> {
    let stem = path.file_stem()?.to_str()?;
    let bytes = stem.as_bytes();
    for i in 0..bytes.len().saturating_sub(3) {
        if bytes[i] == b'_'
            && bytes[i + 1].is_ascii_alphabetic()
            && bytes[i + 2].is_ascii_digit()
            && bytes[i + 3].is_ascii_digit()
        {
            return Some(stem[..i].to_string());
        }
    }
    None
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct CellomicsFilenameMetadata {
    plate: Option<String>,
    well: Option<String>,
    field_index: Option<u32>,
    channel_index: Option<u32>,
    channel_prefix: Option<char>,
}

fn cellomics_filename_metadata(path: &Path) -> CellomicsFilenameMetadata {
    let mut parsed = CellomicsFilenameMetadata::default();
    let Some(stem) = path.file_stem().and_then(|s| s.to_str()) else {
        return parsed;
    };
    parsed.plate = cellomics_plate_prefix(path);

    let bytes = stem.as_bytes();
    for i in 0..bytes.len().saturating_sub(3) {
        if !bytes[i].is_ascii_alphabetic()
            || !bytes[i + 1].is_ascii_digit()
            || !bytes[i + 2].is_ascii_digit()
        {
            continue;
        }
        let well = &stem[i..i + 3];
        let before_ok = i == 0 || !bytes[i - 1].is_ascii_alphanumeric();
        let after_ok = i + 3 == bytes.len()
            || !bytes[i + 3].is_ascii_alphanumeric()
            || bytes[i + 3] == b'f'
            || bytes[i + 3] == b'F'
            || bytes[i + 3] == b'd'
            || bytes[i + 3] == b'D'
            || bytes[i + 3] == b'o'
            || bytes[i + 3] == b'O'
            || bytes[i + 3] == b'c'
            || bytes[i + 3] == b'C';
        if before_ok && after_ok {
            parsed.well = Some(well.to_ascii_uppercase());
            let mut rest = &stem[i + 3..];
            while !rest.is_empty() {
                let Some(tag) = rest.chars().next() else {
                    break;
                };
                if !matches!(tag, 'f' | 'F' | 'd' | 'D' | 'o' | 'O' | 'c' | 'C') {
                    break;
                }
                let digits_start = tag.len_utf8();
                let digits_len = rest[digits_start..]
                    .bytes()
                    .take_while(|b| b.is_ascii_digit())
                    .count();
                if digits_len == 0 {
                    break;
                }
                let number = rest[digits_start..digits_start + digits_len]
                    .parse::<u32>()
                    .ok();
                match tag {
                    'f' | 'F' => parsed.field_index = number,
                    'd' | 'D' | 'o' | 'O' | 'c' | 'C' => {
                        parsed.channel_index = number;
                        parsed.channel_prefix = Some(tag.to_ascii_lowercase());
                    }
                    _ => {}
                }
                rest = &rest[digits_start + digits_len..];
            }
            break;
        }
    }

    parsed
}

fn cellomics_well_row_col(metadata: &CellomicsFilenameMetadata) -> Option<(u32, u32)> {
    let well = metadata.well.as_deref()?;
    if well.len() != 3 {
        return None;
    }
    let bytes = well.as_bytes();
    if !bytes[0].is_ascii_alphabetic() || !bytes[1].is_ascii_digit() || !bytes[2].is_ascii_digit() {
        return None;
    }
    let row = bytes[0].to_ascii_uppercase().checked_sub(b'A')? as u32;
    let col: u32 = well[1..3].parse().ok()?;
    if col == 0 {
        None
    } else {
        Some((row, col - 1))
    }
}

/// Port of `FormatTools.getWellName(row, col)`: a row letter (A, ..., Z, AA, ...)
/// followed by the 1-based column zero-padded to at least two digits.
fn cellomics_well_name(row: i32, col: i32) -> String {
    let mut r = row.max(0);
    let mut letters = String::new();
    loop {
        let rem = (r % 26) as u8;
        letters.insert(0, (b'A' + rem) as char);
        r = r / 26 - 1;
        if r < 0 {
            break;
        }
    }
    format!("{}{:02}", letters, col.max(0) + 1)
}

fn cellomics_headers_match_for_plate_assembly(
    left: &CellomicsParsedHeader,
    right: &CellomicsParsedHeader,
) -> bool {
    left.image_count == 1
        && right.image_count == 1
        && left.width == right.width
        && left.height == right.height
        && left.pixel_type == right.pixel_type
        && left.bits_per_pixel == right.bits_per_pixel
        && left.pixel_offset == right.pixel_offset
        && left.dib_header_size == right.dib_header_size
        && left.dib_compression == right.dib_compression
}

fn cellomics_supported_pixel_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("c01") || e.eq_ignore_ascii_case("dib"))
        .unwrap_or(false)
}

fn cellomics_plate_candidate(
    path: PathBuf,
    metadata: CellomicsFilenameMetadata,
    data: Vec<u8>,
    header: CellomicsParsedHeader,
) -> Option<CellomicsCandidate> {
    let (row, col) = cellomics_well_row_col(&metadata)?;
    Some(CellomicsCandidate {
        path,
        field: metadata.field_index.unwrap_or(0),
        channel: metadata.channel_index?,
        metadata,
        data,
        header,
        row,
        col,
    })
}

fn cellomics_plate_assembly(
    path: &Path,
    current_metadata: &CellomicsFilenameMetadata,
    current_data: Vec<u8>,
    current_header: &CellomicsParsedHeader,
) -> Option<CellomicsPlateAssembly> {
    current_metadata.plate.as_ref()?;
    current_metadata.channel_index?;
    if current_header.image_count != 1 {
        return None;
    }
    let dir = path.parent()?;
    let mut candidates = Vec::new();
    candidates.push(cellomics_plate_candidate(
        path.to_path_buf(),
        current_metadata.clone(),
        current_data,
        current_header.clone(),
    )?);

    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.filter_map(|entry| entry.ok()) {
        let candidate_path = entry.path();
        if candidate_path == path || !cellomics_supported_pixel_extension(&candidate_path) {
            continue;
        }
        let parsed = cellomics_filename_metadata(&candidate_path);
        if parsed.plate != current_metadata.plate || parsed.channel_index.is_none() {
            continue;
        }
        if parsed.channel_prefix != current_metadata.channel_prefix {
            continue;
        }
        let data = match decode_cellomics_file(&candidate_path) {
            Ok(data) => data,
            Err(_) => continue,
        };
        let header = match parse_cellomics_decoded_header(&data) {
            Ok(header) => header,
            Err(_) => continue,
        };
        if !cellomics_headers_match_for_plate_assembly(current_header, &header) {
            continue;
        }
        if let Some(candidate) = cellomics_plate_candidate(candidate_path, parsed, data, header) {
            candidates.push(candidate);
        }
    }

    candidates.sort_by_key(|candidate| {
        (
            candidate.row,
            candidate.col,
            candidate.field,
            candidate.channel,
            candidate.path.clone(),
        )
    });
    candidates.dedup_by(|a, b| {
        a.row == b.row && a.col == b.col && a.field == b.field && a.channel == b.channel
    });

    let mut series = Vec::new();
    let mut current_key = None;
    let mut current_group = Vec::new();
    for candidate in candidates {
        let key = (candidate.row, candidate.col, candidate.field);
        if current_key.is_some() && current_key != Some(key) {
            series.push(cellomics_plate_series_from_group(
                std::mem::take(&mut current_group),
                series.len(),
            ));
        }
        current_key = Some(key);
        current_group.push(candidate);
    }
    if !current_group.is_empty() {
        series.push(cellomics_plate_series_from_group(
            current_group,
            series.len(),
        ));
    }

    if series.is_empty() {
        None
    } else {
        let series_count = series.len() as i64;
        if series_count > 1 {
            for series in &mut series {
                series.metadata.insert(
                    "cellomics.plate_assembly".into(),
                    MetadataValue::String("plate_well_field_filename_series".into()),
                );
                series.metadata.insert(
                    "cellomics.assembled_series_count".into(),
                    MetadataValue::Int(series_count),
                );
            }
        }
        Some(CellomicsPlateAssembly { series })
    }
}

fn cellomics_plate_series_from_group(
    group: Vec<CellomicsCandidate>,
    series_index: usize,
) -> CellomicsPlateSeries {
    let mut sources = Vec::with_capacity(group.len());
    let mut planes = Vec::with_capacity(group.len());
    let mut metadata = HashMap::new();
    let row = group.first().map(|candidate| candidate.row).unwrap_or(0);
    let col = group.first().map(|candidate| candidate.col).unwrap_or(0);
    let field = group.first().map(|candidate| candidate.field).unwrap_or(0);
    if let Some(well) = group
        .first()
        .and_then(|candidate| candidate.metadata.well.clone())
    {
        metadata.insert("cellomics.well".into(), MetadataValue::String(well));
    }
    metadata.insert(
        "cellomics.series_index".into(),
        MetadataValue::Int(series_index as i64),
    );
    metadata.insert("cellomics.well_row".into(), MetadataValue::Int(row as i64));
    metadata.insert(
        "cellomics.well_column".into(),
        MetadataValue::Int(col as i64),
    );
    metadata.insert(
        "cellomics.field_index".into(),
        MetadataValue::Int(field as i64),
    );

    for (source_index, candidate) in group.into_iter().enumerate() {
        let channel = candidate.channel;
        sources.push(CellomicsDecodedSource {
            path: candidate.path,
            data: candidate.data,
            pixel_offset: candidate.header.pixel_offset,
        });
        planes.push(CellomicsPlaneSource {
            source_index,
            plane_index: 0,
            channel_index: Some(channel),
        });
    }

    if !planes.is_empty() {
        metadata.insert(
            "cellomics.assembled_channel_indices".into(),
            MetadataValue::String(
                planes
                    .iter()
                    .filter_map(|plane| plane.channel_index.map(|channel| channel.to_string()))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        );
        metadata.insert(
            "cellomics.assembled_files".into(),
            MetadataValue::String(
                sources
                    .iter()
                    .filter_map(|source| {
                        source
                            .path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .map(|name| name.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        );
    }
    if planes.len() > 1 {
        metadata.insert(
            "cellomics.assembly".into(),
            MetadataValue::String("sibling_filename_channels".into()),
        );
    }

    CellomicsPlateSeries {
        sources,
        planes,
        metadata,
    }
}

fn find_cellomics_mdb(path: &Path) -> Option<PathBuf> {
    let plate_prefix = cellomics_plate_prefix(path)?;
    let dir = path.parent()?;
    std::fs::read_dir(dir)
        .ok()?
        .filter_map(|entry| entry.ok())
        .find_map(|entry| {
            let candidate = entry.path();
            let is_mdb = candidate
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("mdb"))
                .unwrap_or(false);
            let matches_plate = candidate
                .file_stem()
                .and_then(|s| s.to_str())
                .map(|s| s.starts_with(&plate_prefix))
                .unwrap_or(false);
            if is_mdb && matches_plate {
                Some(candidate)
            } else {
                None
            }
        })
}

fn cellomics_channel_metadata_from_table(
    table: &crate::common::mdb::MdbTable,
) -> HashMap<String, MetadataValue> {
    let mut column_index = HashMap::new();
    for (i, column) in table.columns.iter().enumerate() {
        column_index.insert(column.to_ascii_lowercase(), i);
    }

    let mut metadata = HashMap::new();
    for (channel, row) in table.rows.iter().enumerate() {
        let prefix = format!("cellomics.channel.{channel}");
        cellomics_insert_first_string(
            &mut metadata,
            row,
            &column_index,
            &format!("{prefix}.name"),
            &[
                "name",
                "channelname",
                "chname",
                "dye",
                "dyename",
                "fluorophore",
            ],
        );
        cellomics_insert_first_typed(
            &mut metadata,
            row,
            &column_index,
            &format!("{prefix}.exposure_time"),
            &[
                "exposuretime",
                "exposure",
                "exposurems",
                "exposuremilliseconds",
            ],
        );
        cellomics_insert_first_typed(
            &mut metadata,
            row,
            &column_index,
            &format!("{prefix}.composite_color"),
            &["compositecolor", "color", "colour", "rgb"],
        );
        cellomics_insert_first_typed(
            &mut metadata,
            row,
            &column_index,
            &format!("{prefix}.emission_wavelength"),
            &["emissionwavelength", "emission", "emissionnm", "wavelength"],
        );
        cellomics_insert_first_typed(
            &mut metadata,
            row,
            &column_index,
            &format!("{prefix}.excitation_wavelength"),
            &["excitationwavelength", "excitation", "excitationnm"],
        );
        cellomics_insert_first_string(
            &mut metadata,
            row,
            &column_index,
            &format!("{prefix}.filter"),
            &["filter", "filtername", "emissionfilter", "excitationfilter"],
        );
        cellomics_insert_first_string(
            &mut metadata,
            row,
            &column_index,
            &format!("{prefix}.objective"),
            &["objective", "objectivename", "magnification"],
        );
        cellomics_insert_first_typed(
            &mut metadata,
            row,
            &column_index,
            &format!("{prefix}.pixel_size_x"),
            &[
                "pixelsizex",
                "pixelwidth",
                "calibrationx",
                "micronsperpixelx",
            ],
        );
        cellomics_insert_first_typed(
            &mut metadata,
            row,
            &column_index,
            &format!("{prefix}.pixel_size_y"),
            &[
                "pixelsizey",
                "pixelheight",
                "calibrationy",
                "micronsperpixely",
            ],
        );
        cellomics_insert_first_typed(
            &mut metadata,
            row,
            &column_index,
            &format!("{prefix}.pixel_size"),
            &["pixelsize", "micronsperpixel", "calibration"],
        );
        cellomics_insert_first_string(
            &mut metadata,
            row,
            &column_index,
            &format!("{prefix}.binning"),
            &["binning", "bin", "camera binning"],
        );

        for (column_name, value) in table.columns.iter().zip(row.iter()) {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            let normalized = cellomics_normalize_metadata_name(column_name);
            let key = format!("{prefix}.mdb.{normalized}");
            metadata
                .entry(key)
                .or_insert_with(|| MetadataValue::String(value.to_string()));
        }
    }
    metadata
}

fn cellomics_scalar_mdb_metadata_from_tables(
    tables: &[crate::common::mdb::MdbTable],
) -> HashMap<String, MetadataValue> {
    let mut metadata = HashMap::new();
    let mut recognized = Vec::new();

    for table in tables {
        let table_name = table.name.to_ascii_lowercase();
        let Some((scope, aliases)) = cellomics_mdb_scalar_scope(&table_name) else {
            continue;
        };
        recognized.push(scope);

        let mut column_index = HashMap::new();
        for (i, column) in table.columns.iter().enumerate() {
            column_index.insert(column.to_ascii_lowercase(), i);
        }
        let Some(row) = table.rows.first() else {
            metadata.insert(
                format!("cellomics.mdb.{scope}.row_count"),
                MetadataValue::Int(0),
            );
            continue;
        };

        metadata.insert(
            format!("cellomics.mdb.{scope}.row_count"),
            MetadataValue::Int(table.rows.len() as i64),
        );
        for (key, names) in aliases {
            cellomics_insert_first_typed(
                &mut metadata,
                row,
                &column_index,
                &format!("cellomics.{scope}.{key}"),
                names,
            );
        }

        for (column_name, value) in table.columns.iter().zip(row.iter()) {
            let value = value.trim();
            if value.is_empty() {
                continue;
            }
            let normalized = cellomics_normalize_metadata_name(column_name);
            metadata
                .entry(format!("cellomics.mdb.{scope}.{normalized}"))
                .or_insert_with(|| MetadataValue::String(value.to_string()));
        }
    }

    if !recognized.is_empty() {
        recognized.sort_unstable();
        recognized.dedup();
        metadata.insert(
            "cellomics.mdb.scalar_tables".into(),
            MetadataValue::String(recognized.join(",")),
        );
    }

    metadata
}

fn cellomics_mdb_table_diagnostics_from_tables(
    tables: &[crate::common::mdb::MdbTable],
) -> HashMap<String, MetadataValue> {
    let mut metadata = HashMap::new();
    let mut recognized = Vec::new();
    let mut unhandled = Vec::new();
    let mut unhandled_shapes = Vec::new();

    for table in tables {
        let lower_name = table.name.to_ascii_lowercase();
        if lower_name == "asnprotocolchannel" {
            recognized.push(table.name.clone());
        } else if cellomics_mdb_scalar_scope(&lower_name).is_some() {
            recognized.push(table.name.clone());
        } else {
            unhandled.push(table.name.clone());
            unhandled_shapes.push(format!(
                "{}:rows={},columns={}",
                table.name,
                table.rows.len(),
                table.columns.len()
            ));
        }
    }

    if !recognized.is_empty() {
        recognized.sort_unstable();
        recognized.dedup();
        metadata.insert(
            "cellomics.mdb.recognized_tables".into(),
            MetadataValue::String(recognized.join(",")),
        );
    }
    if !unhandled.is_empty() {
        unhandled.sort_unstable();
        unhandled.dedup();
        metadata.insert(
            "cellomics.mdb.unhandled_tables".into(),
            MetadataValue::String(unhandled.join(",")),
        );
    }
    if !unhandled_shapes.is_empty() {
        unhandled_shapes.sort_unstable();
        unhandled_shapes.dedup();
        metadata.insert(
            "cellomics.mdb.unhandled_table_shapes".into(),
            MetadataValue::String(unhandled_shapes.join(";")),
        );
    }

    metadata
}

fn cellomics_mdb_scalar_scope(
    table_name: &str,
) -> Option<(
    &'static str,
    &'static [(&'static str, &'static [&'static str])],
)> {
    const PROTOCOL_ALIASES: &[(&str, &[&str])] = &[
        ("name", &["name", "protocolname", "assayname"]),
        (
            "description",
            &["description", "protocolcomment", "comment"],
        ),
        ("objective", &["objective", "objectivename"]),
        (
            "magnification",
            &["magnification", "objectivemagnification"],
        ),
        ("binning", &["binning", "camerabinning"]),
    ];
    const PLATE_ALIASES: &[(&str, &[&str])] = &[
        ("name", &["name", "platename"]),
        ("id", &["plateid", "barcode", "externalid"]),
        ("description", &["description", "platedescription"]),
        ("rows", &["rows", "platerows"]),
        ("columns", &["columns", "platecolumns", "cols"]),
        ("well_count", &["wellcount", "wells"]),
    ];
    const EXPERIMENT_ALIASES: &[(&str, &[&str])] = &[
        ("name", &["name", "experimentname", "runname"]),
        ("id", &["experimentid", "runid"]),
        ("operator", &["operator", "username", "user"]),
        ("date", &["date", "acquisitiondate", "rundate"]),
        (
            "instrument",
            &["instrument", "instrumentname", "systemname"],
        ),
    ];
    const INSTRUMENT_ALIASES: &[(&str, &[&str])] = &[
        ("name", &["name", "instrumentname", "systemname"]),
        ("model", &["model", "instrumentmodel", "systemmodel"]),
        (
            "manufacturer",
            &["manufacturer", "vendor", "instrumentmanufacturer"],
        ),
        ("serial_number", &["serialnumber", "serial", "systemserial"]),
        ("objective", &["objective", "objectivename"]),
        (
            "magnification",
            &["magnification", "objectivemagnification"],
        ),
        ("camera", &["camera", "cameraname", "cameramodel"]),
    ];

    match table_name {
        "asnprotocol" | "protocol" | "protocolinfo" => Some(("protocol", PROTOCOL_ALIASES)),
        "asnplate" | "plate" | "plateinfo" => Some(("plate", PLATE_ALIASES)),
        "asnexperiment" | "experiment" | "experimentinfo" | "run" | "runinfo" => {
            Some(("experiment", EXPERIMENT_ALIASES))
        }
        "asninstrument" | "instrument" | "instrumentinfo" | "system" | "systeminfo" => {
            Some(("instrument", INSTRUMENT_ALIASES))
        }
        _ => None,
    }
}

fn cellomics_insert_first_string(
    metadata: &mut HashMap<String, MetadataValue>,
    row: &[String],
    column_index: &HashMap<String, usize>,
    key: &str,
    aliases: &[&str],
) {
    if let Some(value) = aliases
        .iter()
        .find_map(|alias| mdb_row_value(row, column_index, alias))
    {
        metadata.insert(key.to_string(), MetadataValue::String(value.to_string()));
    }
}

fn cellomics_insert_first_typed(
    metadata: &mut HashMap<String, MetadataValue>,
    row: &[String],
    column_index: &HashMap<String, usize>,
    key: &str,
    aliases: &[&str],
) {
    if let Some(value) = aliases
        .iter()
        .find_map(|alias| mdb_row_value(row, column_index, alias))
    {
        let value = value.trim();
        if let Ok(int_value) = value.parse::<i64>() {
            metadata.insert(key.to_string(), MetadataValue::Int(int_value));
        } else if let Ok(float_value) = value.parse::<f64>() {
            metadata.insert(key.to_string(), MetadataValue::Float(float_value));
        } else {
            metadata.insert(key.to_string(), MetadataValue::String(value.to_string()));
        }
    }
}

fn cellomics_normalize_metadata_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_sep = false;
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch.to_ascii_lowercase());
            last_sep = false;
        } else if !last_sep {
            out.push('_');
            last_sep = true;
        }
    }
    out.trim_matches('_').to_string()
}

fn mdb_row_value<'a>(
    row: &'a [String],
    column_index: &HashMap<String, usize>,
    name: &str,
) -> Option<&'a str> {
    let value = row.get(*column_index.get(name)?)?.trim();
    if value.is_empty() {
        None
    } else {
        Some(value)
    }
}

fn read_cellomics_mdb_metadata(path: &Path) -> HashMap<String, MetadataValue> {
    let mut metadata = HashMap::new();
    let Some(mdb_path) = find_cellomics_mdb(path) else {
        return metadata;
    };

    metadata.insert(
        "cellomics.mdb_file".into(),
        MetadataValue::String(mdb_path.display().to_string()),
    );
    match crate::common::mdb::parse_database(&mdb_path) {
        Ok(tables) => {
            let table_names: Vec<&str> = tables.iter().map(|table| table.name.as_str()).collect();
            if !table_names.is_empty() {
                metadata.insert(
                    "cellomics.mdb_tables".into(),
                    MetadataValue::String(table_names.join(",")),
                );
            }
            if let Some(table) = tables
                .iter()
                .find(|table| table.name.eq_ignore_ascii_case("asnProtocolChannel"))
            {
                metadata.extend(cellomics_channel_metadata_from_table(table));
            } else {
                metadata.insert(
                    "cellomics.mdb_missing_table".into(),
                    MetadataValue::String("asnProtocolChannel".into()),
                );
            }
            metadata.extend(cellomics_scalar_mdb_metadata_from_tables(&tables));
            metadata.extend(cellomics_mdb_table_diagnostics_from_tables(&tables));
        }
        Err(e) => {
            metadata.insert(
                "cellomics.mdb_error".into(),
                MetadataValue::String(e.to_string()),
            );
        }
    }
    metadata
}

fn insert_cellomics_file_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    path: &Path,
    dib_header_size: Option<u32>,
    dib_planes: u32,
    dib_compression: Option<u32>,
    dib_top_down: Option<bool>,
) {
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        metadata.insert(
            "cellomics.file_name".into(),
            MetadataValue::String(name.to_string()),
        );
    }
    let parsed = cellomics_filename_metadata(path);
    if let Some(value) = parsed.plate {
        metadata.insert("cellomics.plate".into(), MetadataValue::String(value));
    }
    if let Some(value) = parsed.well {
        metadata.insert("cellomics.well".into(), MetadataValue::String(value));
    }
    if let Some(value) = parsed.field_index {
        metadata.insert(
            "cellomics.field_index".into(),
            MetadataValue::Int(value as i64),
        );
    }
    if let Some(value) = parsed.channel_index {
        metadata.insert(
            "cellomics.filename_channel_index".into(),
            MetadataValue::Int(value as i64),
        );
    }
    if let Some(value) = dib_header_size {
        metadata.insert(
            "cellomics.dib.header_size".into(),
            MetadataValue::Int(value as i64),
        );
    }
    metadata.insert(
        "cellomics.dib.planes".into(),
        MetadataValue::Int(dib_planes as i64),
    );
    if let Some(value) = dib_compression {
        metadata.insert(
            "cellomics.dib.compression".into(),
            MetadataValue::Int(value as i64),
        );
    }
    if let Some(value) = dib_top_down {
        metadata.insert(
            "cellomics.dib.top_down".into(),
            MetadataValue::String(value.to_string()),
        );
    }
}

fn cellomics_metadata_f64(metadata: &HashMap<String, MetadataValue>, key: &str) -> Option<f64> {
    match metadata.get(key)? {
        MetadataValue::Float(value) => Some(*value),
        MetadataValue::Int(value) => Some(*value as f64),
        MetadataValue::String(value) => value.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn cellomics_metadata_i64(metadata: &HashMap<String, MetadataValue>, key: &str) -> Option<i64> {
    match metadata.get(key)? {
        MetadataValue::Int(value) => Some(*value),
        MetadataValue::Float(value) if value.fract() == 0.0 => Some(*value as i64),
        MetadataValue::String(value) => value.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn cellomics_metadata_string(
    metadata: &HashMap<String, MetadataValue>,
    key: &str,
) -> Option<String> {
    match metadata.get(key)? {
        MetadataValue::String(value) if !value.trim().is_empty() => Some(value.trim().to_string()),
        MetadataValue::Int(value) => Some(value.to_string()),
        MetadataValue::Float(value) => Some(value.to_string()),
        _ => None,
    }
}

fn cellomics_ome_color(value: i64) -> Option<i32> {
    if (0..=0x00ff_ffff).contains(&value) {
        let rgba = ((value as u32) << 8) | 0xff;
        Some(rgba as i32)
    } else if (i32::MIN as i64..=i32::MAX as i64).contains(&value) {
        Some(value as i32)
    } else {
        None
    }
}

fn cellomics_matching_channel_sources(
    path: &Path,
    current_metadata: &CellomicsFilenameMetadata,
    current_data: Vec<u8>,
    current_header: &CellomicsParsedHeader,
) -> (
    Vec<CellomicsDecodedSource>,
    Vec<CellomicsPlaneSource>,
    HashMap<String, MetadataValue>,
) {
    let mut metadata = HashMap::new();
    let Some(current_channel) = current_metadata.channel_index else {
        return (
            vec![CellomicsDecodedSource {
                path: path.to_path_buf(),
                data: current_data,
                pixel_offset: current_header.pixel_offset,
            }],
            (0..current_header.image_count)
                .map(|plane_index| CellomicsPlaneSource {
                    source_index: 0,
                    plane_index,
                    channel_index: None,
                })
                .collect(),
            metadata,
        );
    };
    if current_header.image_count != 1 {
        return (
            vec![CellomicsDecodedSource {
                path: path.to_path_buf(),
                data: current_data,
                pixel_offset: current_header.pixel_offset,
            }],
            (0..current_header.image_count)
                .map(|plane_index| CellomicsPlaneSource {
                    source_index: 0,
                    plane_index,
                    channel_index: Some(current_channel),
                })
                .collect(),
            metadata,
        );
    }

    let Some(dir) = path.parent() else {
        return (
            vec![CellomicsDecodedSource {
                path: path.to_path_buf(),
                data: current_data,
                pixel_offset: current_header.pixel_offset,
            }],
            vec![CellomicsPlaneSource {
                source_index: 0,
                plane_index: 0,
                channel_index: Some(current_channel),
            }],
            metadata,
        );
    };

    let current_ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let mut candidates = vec![(
        current_channel,
        path.to_path_buf(),
        current_data,
        current_header.pixel_offset,
    )];

    let Ok(entries) = std::fs::read_dir(dir) else {
        return (
            vec![CellomicsDecodedSource {
                path: path.to_path_buf(),
                data: candidates.remove(0).2,
                pixel_offset: current_header.pixel_offset,
            }],
            vec![CellomicsPlaneSource {
                source_index: 0,
                plane_index: 0,
                channel_index: Some(current_channel),
            }],
            metadata,
        );
    };

    for entry in entries.filter_map(|entry| entry.ok()) {
        let candidate_path = entry.path();
        if candidate_path == path {
            continue;
        }
        let candidate_ext = candidate_path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if candidate_ext != current_ext {
            continue;
        }
        let parsed = cellomics_filename_metadata(&candidate_path);
        if parsed.plate != current_metadata.plate
            || parsed.well != current_metadata.well
            || parsed.field_index != current_metadata.field_index
            || parsed.channel_prefix != current_metadata.channel_prefix
        {
            continue;
        }
        let Some(channel) = parsed.channel_index else {
            continue;
        };
        if candidates
            .iter()
            .any(|(known_channel, _, _, _)| *known_channel == channel)
        {
            continue;
        }
        let Ok(data) = decode_cellomics_file(&candidate_path) else {
            continue;
        };
        let Ok(header) = parse_cellomics_decoded_header(&data) else {
            continue;
        };
        if header.image_count != 1
            || header.width != current_header.width
            || header.height != current_header.height
            || header.pixel_type != current_header.pixel_type
            || header.bits_per_pixel != current_header.bits_per_pixel
            || header.pixel_offset != current_header.pixel_offset
            || header.dib_header_size != current_header.dib_header_size
            || header.dib_compression != current_header.dib_compression
        {
            continue;
        }
        candidates.push((channel, candidate_path, data, header.pixel_offset));
    }

    candidates.sort_by_key(|(channel, _, _, _)| *channel);
    let mut sources = Vec::with_capacity(candidates.len());
    let mut planes = Vec::with_capacity(candidates.len());
    for (source_index, (channel, source_path, data, pixel_offset)) in
        candidates.into_iter().enumerate()
    {
        sources.push(CellomicsDecodedSource {
            path: source_path,
            data,
            pixel_offset,
        });
        planes.push(CellomicsPlaneSource {
            source_index,
            plane_index: 0,
            channel_index: Some(channel),
        });
    }

    if planes.len() > 1 {
        metadata.insert(
            "cellomics.assembly".into(),
            MetadataValue::String("sibling_filename_channels".into()),
        );
        metadata.insert(
            "cellomics.assembled_channel_indices".into(),
            MetadataValue::String(
                planes
                    .iter()
                    .filter_map(|plane| plane.channel_index.map(|channel| channel.to_string()))
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        );
        metadata.insert(
            "cellomics.assembled_files".into(),
            MetadataValue::String(
                sources
                    .iter()
                    .filter_map(|source| {
                        source
                            .path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .map(|name| name.to_string())
                    })
                    .collect::<Vec<_>>()
                    .join(","),
            ),
        );
    }

    (sources, planes, metadata)
}

/// Port of the plate/well portion of Java `CellomicsReader.initFile`.
///
/// Java derives a single Plate whose row/column count is snapped to a standard
/// plate size based on the maximum well row/column observed across every series
/// and the total series count:
///   - 1 series                -> 1x1 (the lone well is placed at its own row/col)
///   - <= 8 rows and <= 12 cols -> 96-well (8x12)
///   - otherwise               -> 384-well (16x24)
/// Each series maps to a WellSample (field index) inside its well. We stamp the
/// resulting per-series plate placement into the series metadata so that
/// `ome_metadata` can rebuild the OME Plate/Well/WellSample tree faithfully.
fn cellomics_finalize_plate_metadata(
    metas: &mut [ImageMetadata],
    filename_metadata: &CellomicsFilenameMetadata,
) {
    let series_count = metas.len();
    if series_count == 0 {
        return;
    }

    // wellRows / wellColumns are the maxima over all files (Java tracks them in
    // the file loop). When the plate could not be parsed the metadata simply
    // carries zero, matching Java's defaults.
    let mut well_rows = 0u32;
    let mut well_columns = 0u32;
    for meta in metas.iter() {
        if let Some(row) = cellomics_metadata_i64(&meta.series_metadata, "cellomics.well_row") {
            well_rows = well_rows.max(row.max(0) as u32);
        }
        if let Some(col) = cellomics_metadata_i64(&meta.series_metadata, "cellomics.well_column") {
            well_columns = well_columns.max(col.max(0) as u32);
        }
    }

    let (real_rows, real_cols) = if series_count == 1 {
        (1u32, 1u32)
    } else if well_rows + 1 <= 8 && well_columns + 1 <= 12 {
        (8, 12)
    } else {
        (16, 24)
    };

    let plate_name = filename_metadata
        .plate
        .clone()
        .or_else(|| cellomics_metadata_string(&metas[0].series_metadata, "cellomics.plate"))
        .unwrap_or_default();

    for (series_index, meta) in metas.iter_mut().enumerate() {
        meta.series_metadata.insert(
            "cellomics.plate.real_rows".into(),
            MetadataValue::Int(real_rows as i64),
        );
        meta.series_metadata.insert(
            "cellomics.plate.real_columns".into(),
            MetadataValue::Int(real_cols as i64),
        );
        meta.series_metadata.insert(
            "cellomics.plate.series_count".into(),
            MetadataValue::Int(series_count as i64),
        );
        if !plate_name.is_empty() {
            meta.series_metadata.insert(
                "cellomics.plate.name".into(),
                MetadataValue::String(plate_name.clone()),
            );
        }

        // Java places the lone well of a single-series plate at its own row/col
        // (the 1x1 case sets row=files.get(0).row, col=files.get(0).col), but
        // resets the *image* row/col to 0 when computing the well index.
        let row = cellomics_metadata_i64(&meta.series_metadata, "cellomics.well_row")
            .map(|v| v.max(0) as u32)
            .unwrap_or(0);
        let col = cellomics_metadata_i64(&meta.series_metadata, "cellomics.well_column")
            .map(|v| v.max(0) as u32)
            .unwrap_or(0);
        let (image_row, image_col) = if series_count == 1 {
            (0, 0)
        } else {
            (row, col)
        };

        if image_row < real_rows && image_col < real_cols {
            let well_index = image_row * real_cols + image_col;
            meta.series_metadata.insert(
                "cellomics.plate.well_index".into(),
                MetadataValue::Int(well_index as i64),
            );
            meta.series_metadata.insert(
                "cellomics.plate.well_sample_index".into(),
                MetadataValue::Int(series_index as i64),
            );
        }
    }
}

#[cfg(test)]
mod cellomics_mdb_tests {
    use super::{
        cellomics_channel_metadata_from_table, cellomics_filename_metadata,
        cellomics_mdb_table_diagnostics_from_tables, cellomics_ome_color, cellomics_plate_prefix,
        cellomics_scalar_mdb_metadata_from_tables, CellomicsReader,
    };
    use crate::common::mdb::MdbTable;
    use crate::common::metadata::MetadataValue;
    use crate::common::reader::FormatReader;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn cellomics_plate_prefix_matches_well_token() {
        assert_eq!(
            cellomics_plate_prefix(Path::new("Plate42_A01f00d1.c01")).as_deref(),
            Some("Plate42")
        );
        assert_eq!(
            cellomics_plate_prefix(Path::new("Plate_2024_H12.c01")).as_deref(),
            Some("Plate_2024")
        );
        assert!(cellomics_plate_prefix(Path::new("not_cellomics.c01")).is_none());
    }

    #[test]
    fn cellomics_mdb_channel_table_maps_expected_columns() {
        let table = MdbTable {
            name: "asnProtocolChannel".into(),
            columns: vec![
                "Name".into(),
                "ExposureTime".into(),
                "CompositeColor".into(),
                "EmissionWavelength".into(),
                "ExcitationWavelength".into(),
                "PixelSizeX".into(),
                "PixelSizeY".into(),
                "FilterName".into(),
                "Ignored".into(),
            ],
            rows: vec![
                vec![
                    "DAPI".into(),
                    "35.5".into(),
                    "16711680".into(),
                    "460".into(),
                    "405".into(),
                    "0.65".into(),
                    "0.66".into(),
                    "DAPI cube".into(),
                    "x".into(),
                ],
                vec![
                    "FITC".into(),
                    "n/a".into(),
                    "green".into(),
                    "525".into(),
                    "488".into(),
                    "".into(),
                    "".into(),
                    "FITC cube".into(),
                    "x".into(),
                ],
            ],
        };

        let metadata = cellomics_channel_metadata_from_table(&table);
        assert!(matches!(
            metadata.get("cellomics.channel.0.name"),
            Some(MetadataValue::String(v)) if v == "DAPI"
        ));
        assert!(matches!(
            metadata.get("cellomics.channel.0.exposure_time"),
            Some(MetadataValue::Float(v)) if (*v - 35.5).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("cellomics.channel.0.composite_color"),
            Some(MetadataValue::Int(16711680))
        ));
        assert!(matches!(
            metadata.get("cellomics.channel.0.emission_wavelength"),
            Some(MetadataValue::Int(460))
        ));
        assert!(matches!(
            metadata.get("cellomics.channel.0.excitation_wavelength"),
            Some(MetadataValue::Int(405))
        ));
        assert!(matches!(
            metadata.get("cellomics.channel.0.pixel_size_x"),
            Some(MetadataValue::Float(v)) if (*v - 0.65).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("cellomics.channel.0.pixel_size_y"),
            Some(MetadataValue::Float(v)) if (*v - 0.66).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("cellomics.channel.0.filter"),
            Some(MetadataValue::String(v)) if v == "DAPI cube"
        ));
        assert!(matches!(
            metadata.get("cellomics.channel.0.mdb.emissionwavelength"),
            Some(MetadataValue::String(v)) if v == "460"
        ));
        assert!(matches!(
            metadata.get("cellomics.channel.1.exposure_time"),
            Some(MetadataValue::String(v)) if v == "n/a"
        ));
        assert!(matches!(
            metadata.get("cellomics.channel.1.composite_color"),
            Some(MetadataValue::String(v)) if v == "green"
        ));
    }

    #[test]
    fn cellomics_mdb_scalar_tables_project_protocol_plate_and_experiment() {
        let tables = vec![
            MdbTable {
                name: "asnProtocol".into(),
                columns: vec![
                    "ProtocolName".into(),
                    "ObjectiveMagnification".into(),
                    "CameraBinning".into(),
                ],
                rows: vec![vec!["Drug screen".into(), "20".into(), "2x2".into()]],
            },
            MdbTable {
                name: "asnPlate".into(),
                columns: vec![
                    "PlateID".into(),
                    "PlateRows".into(),
                    "PlateColumns".into(),
                    "PlateDescription".into(),
                ],
                rows: vec![vec![
                    "BAR123".into(),
                    "8".into(),
                    "12".into(),
                    "96 well plate".into(),
                ]],
            },
            MdbTable {
                name: "asnExperiment".into(),
                columns: vec![
                    "ExperimentName".into(),
                    "Operator".into(),
                    "InstrumentName".into(),
                ],
                rows: vec![vec!["Run 7".into(), "Ada".into(), "ArrayScan".into()]],
            },
            MdbTable {
                name: "asnInstrument".into(),
                columns: vec![
                    "InstrumentName".into(),
                    "InstrumentModel".into(),
                    "Manufacturer".into(),
                    "SerialNumber".into(),
                    "ObjectiveMagnification".into(),
                    "CameraModel".into(),
                ],
                rows: vec![vec![
                    "ArrayScan VTI".into(),
                    "VTI 700".into(),
                    "Thermo Fisher".into(),
                    "SN42".into(),
                    "20".into(),
                    "Orca".into(),
                ]],
            },
            MdbTable {
                name: "Unrelated".into(),
                columns: vec!["Name".into()],
                rows: vec![vec!["ignored".into()]],
            },
        ];

        let metadata = cellomics_scalar_mdb_metadata_from_tables(&tables);
        assert!(matches!(
            metadata.get("cellomics.mdb.scalar_tables"),
            Some(MetadataValue::String(v)) if v == "experiment,instrument,plate,protocol"
        ));
        assert!(matches!(
            metadata.get("cellomics.protocol.name"),
            Some(MetadataValue::String(v)) if v == "Drug screen"
        ));
        assert!(matches!(
            metadata.get("cellomics.protocol.magnification"),
            Some(MetadataValue::Int(20))
        ));
        assert!(matches!(
            metadata.get("cellomics.protocol.binning"),
            Some(MetadataValue::String(v)) if v == "2x2"
        ));
        assert!(matches!(
            metadata.get("cellomics.plate.id"),
            Some(MetadataValue::String(v)) if v == "BAR123"
        ));
        assert!(matches!(
            metadata.get("cellomics.plate.rows"),
            Some(MetadataValue::Int(8))
        ));
        assert!(matches!(
            metadata.get("cellomics.plate.columns"),
            Some(MetadataValue::Int(12))
        ));
        assert!(matches!(
            metadata.get("cellomics.experiment.operator"),
            Some(MetadataValue::String(v)) if v == "Ada"
        ));
        assert!(matches!(
            metadata.get("cellomics.instrument.name"),
            Some(MetadataValue::String(v)) if v == "ArrayScan VTI"
        ));
        assert!(matches!(
            metadata.get("cellomics.instrument.model"),
            Some(MetadataValue::String(v)) if v == "VTI 700"
        ));
        assert!(matches!(
            metadata.get("cellomics.instrument.manufacturer"),
            Some(MetadataValue::String(v)) if v == "Thermo Fisher"
        ));
        assert!(matches!(
            metadata.get("cellomics.instrument.serial_number"),
            Some(MetadataValue::String(v)) if v == "SN42"
        ));
        assert!(matches!(
            metadata.get("cellomics.instrument.magnification"),
            Some(MetadataValue::Int(20))
        ));
        assert!(matches!(
            metadata.get("cellomics.instrument.camera"),
            Some(MetadataValue::String(v)) if v == "Orca"
        ));
        assert!(matches!(
            metadata.get("cellomics.mdb.instrument.instrumentmodel"),
            Some(MetadataValue::String(v)) if v == "VTI 700"
        ));
        assert!(matches!(
            metadata.get("cellomics.mdb.protocol.objectivemagnification"),
            Some(MetadataValue::String(v)) if v == "20"
        ));
        assert!(!metadata.contains_key("cellomics.unrelated.name"));
    }

    #[test]
    fn cellomics_mdb_table_diagnostics_report_unhandled_shapes_without_mapping_semantics() {
        let tables = vec![
            MdbTable {
                name: "asnProtocolChannel".into(),
                columns: vec!["Name".into()],
                rows: vec![vec!["DAPI".into()]],
            },
            MdbTable {
                name: "asnPlate".into(),
                columns: vec!["PlateID".into()],
                rows: vec![vec!["BAR123".into()]],
            },
            MdbTable {
                name: "asnWell".into(),
                columns: vec!["Well".into(), "Value".into()],
                rows: vec![
                    vec!["A01".into(), "7".into()],
                    vec!["A02".into(), "8".into()],
                ],
            },
        ];

        let metadata = cellomics_mdb_table_diagnostics_from_tables(&tables);
        assert!(matches!(
            metadata.get("cellomics.mdb.recognized_tables"),
            Some(MetadataValue::String(value)) if value == "asnPlate,asnProtocolChannel"
        ));
        assert!(matches!(
            metadata.get("cellomics.mdb.unhandled_tables"),
            Some(MetadataValue::String(value)) if value == "asnWell"
        ));
        assert!(matches!(
            metadata.get("cellomics.mdb.unhandled_table_shapes"),
            Some(MetadataValue::String(value)) if value == "asnWell:rows=2,columns=2"
        ));
        assert!(!metadata.contains_key("cellomics.well.value"));
    }

    #[test]
    fn cellomics_filename_metadata_extracts_well_field_and_channel() {
        let parsed = cellomics_filename_metadata(Path::new("AS_09125_050118150001_A03f00d1.DIB"));
        assert_eq!(parsed.plate.as_deref(), Some("AS_09125_050118150001"));
        assert_eq!(parsed.well.as_deref(), Some("A03"));
        assert_eq!(parsed.field_index, Some(0));
        assert_eq!(parsed.channel_index, Some(1));
    }

    #[test]
    fn cellomics_filename_metadata_extracts_java_o_channel_variant() {
        let parsed = cellomics_filename_metadata(Path::new("WHICA-VTI1_090915160001_A01f00o2.DIB"));
        assert_eq!(parsed.plate.as_deref(), Some("WHICA-VTI1_090915160001"));
        assert_eq!(parsed.well.as_deref(), Some("A01"));
        assert_eq!(parsed.field_index, Some(0));
        assert_eq!(parsed.channel_index, Some(2));
    }

    #[test]
    fn cellomics_ome_color_packs_rgb_with_opaque_alpha() {
        assert_eq!(cellomics_ome_color(0x336699), Some(0x336699ff));
    }

    #[test]
    fn cellomics_reader_assembles_matching_sibling_channel_files() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bioformats_cellomics_assembly_{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path_d0 = dir.join("AS_09125_050118150001_A03f00d0.DIB");
        let path_d1 = dir.join("AS_09125_050118150001_A03f00d1.DIB");
        let path_other_field = dir.join("AS_09125_050118150001_A03f01d2.DIB");

        for (path, pixels) in [
            (&path_d0, [1u8, 2, 3, 4]),
            (&path_d1, [5u8, 6, 7, 8]),
            (&path_other_field, [9u8, 10, 11, 12]),
        ] {
            let mut data = vec![0u8; 52];
            data[0..4].copy_from_slice(&40u32.to_le_bytes());
            data[4..8].copy_from_slice(&2i32.to_le_bytes());
            data[8..12].copy_from_slice(&2i32.to_le_bytes());
            data[12..14].copy_from_slice(&1u16.to_le_bytes());
            data[14..16].copy_from_slice(&8u16.to_le_bytes());
            data[16..20].copy_from_slice(&0u32.to_le_bytes());
            data.extend_from_slice(&pixels);
            std::fs::write(path, data).unwrap();
        }

        let mut reader = CellomicsReader::new();
        reader.set_id(&path_d1).unwrap();
        assert_eq!(reader.metadata().size_c, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("cellomics.assembly"),
            Some(MetadataValue::String(value)) if value == "sibling_filename_channels"
        ));
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("cellomics.assembled_channel_indices"),
            Some(MetadataValue::String(value)) if value == "0,1"
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn cellomics_reader_assembles_java_o_channel_sibling_files() {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!("bioformats_cellomics_o_assembly_{unique}"));
        std::fs::create_dir_all(&dir).unwrap();
        let path_o1 = dir.join("WHICA-VTI1_090915160001_A01f00o1.DIB");
        let path_o2 = dir.join("WHICA-VTI1_090915160001_A01f00o2.DIB");
        let path_other_field = dir.join("WHICA-VTI1_090915160001_A01f01o3.DIB");

        for (path, pixels) in [
            (&path_o1, [11u8, 12, 13, 14]),
            (&path_o2, [21u8, 22, 23, 24]),
            (&path_other_field, [31u8, 32, 33, 34]),
        ] {
            let mut data = vec![0u8; 52];
            data[0..4].copy_from_slice(&40u32.to_le_bytes());
            data[4..8].copy_from_slice(&2i32.to_le_bytes());
            data[8..12].copy_from_slice(&2i32.to_le_bytes());
            data[12..14].copy_from_slice(&1u16.to_le_bytes());
            data[14..16].copy_from_slice(&8u16.to_le_bytes());
            data[16..20].copy_from_slice(&0u32.to_le_bytes());
            data.extend_from_slice(&pixels);
            std::fs::write(path, data).unwrap();
        }

        let mut reader = CellomicsReader::new();
        reader.set_id(&path_o2).unwrap();
        assert_eq!(reader.metadata().size_c, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![11, 12, 13, 14]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![21, 22, 23, 24]);
        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("cellomics.assembled_channel_indices"),
            Some(MetadataValue::String(value)) if value == "1,2"
        ));

        let _ = std::fs::remove_dir_all(dir);
    }
}

impl FormatReader for CellomicsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("c01") | Some("dib"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        _header.len() >= 4
            && i32::from_be_bytes([_header[0], _header[1], _header[2], _header[3]]) == 16
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = decode_cellomics_file(path)?;
        let header = parse_cellomics_decoded_header(&data)?;
        let filename_metadata = cellomics_filename_metadata(path);
        let mdb_metadata = read_cellomics_mdb_metadata(path);
        let plate_assembly =
            cellomics_plate_assembly(path, &filename_metadata, data.clone(), &header);
        let mut metas = Vec::new();
        let mut series_sources = Vec::new();
        let mut series_planes = Vec::new();

        if let Some(assembly) = plate_assembly {
            for series in assembly.series {
                let representative_path = series
                    .sources
                    .first()
                    .map(|source| source.path.as_path())
                    .unwrap_or(path);
                let mut series_metadata = mdb_metadata.clone();
                series_metadata.extend(series.metadata);
                insert_cellomics_file_metadata(
                    &mut series_metadata,
                    representative_path,
                    header.dib_header_size,
                    header.dib_planes,
                    header.dib_compression,
                    header.dib_top_down,
                );
                let image_count = series.planes.len() as u32;
                metas.push(ImageMetadata {
                    size_x: header.width,
                    size_y: header.height,
                    size_z: 1,
                    size_c: image_count.max(1),
                    size_t: 1,
                    pixel_type: header.pixel_type,
                    bits_per_pixel: header.bits_per_pixel,
                    image_count: image_count.max(1),
                    dimension_order: DimensionOrder::XYCZT,
                    is_rgb: false,
                    is_interleaved: false,
                    is_indexed: false,
                    is_little_endian: true,
                    resolution_count: 1,
                    thumbnail: false,
                    series_metadata,
                    lookup_table: None,
                    modulo_z: None,
                    modulo_c: None,
                    modulo_t: None,
                });
                series_sources.push(series.sources);
                series_planes.push(series.planes);
            }
        } else {
            let (sources, planes, assembly_metadata) =
                cellomics_matching_channel_sources(path, &filename_metadata, data.clone(), &header);
            let image_count = planes.len() as u32;
            let assembled_channels = assembly_metadata.contains_key("cellomics.assembly");
            let mut series_metadata = mdb_metadata;
            series_metadata.extend(assembly_metadata);
            insert_cellomics_file_metadata(
                &mut series_metadata,
                path,
                header.dib_header_size,
                header.dib_planes,
                header.dib_compression,
                header.dib_top_down,
            );
            // Java initFile sets sizeZ = nPlanes (the DIB plane count) and
            // sizeC = uniqueChannels.size(); imageCount = sizeZ*sizeT*sizeC.
            // When sibling channel files were assembled, the per-channel planes
            // become C; otherwise the file's own planes are Z (single channel).
            let (size_z, size_c) = if assembled_channels {
                (1, image_count)
            } else {
                (image_count, 1)
            };
            metas.push(ImageMetadata {
                size_x: header.width,
                size_y: header.height,
                size_z,
                size_c,
                size_t: 1,
                pixel_type: header.pixel_type,
                bits_per_pixel: header.bits_per_pixel,
                image_count,
                dimension_order: DimensionOrder::XYCZT,
                is_rgb: false,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: true,
                resolution_count: 1,
                thumbnail: false,
                series_metadata,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
            series_sources.push(sources);
            series_planes.push(planes);
        }

        cellomics_finalize_plate_metadata(&mut metas, &filename_metadata);

        self.path = Some(path.to_path_buf());
        self.current_series = 0;
        self.pixel_offset = header.pixel_offset;
        self.data = data;
        self.metas = metas;
        self.series_sources = series_sources;
        self.series_planes = series_planes;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.metas.clear();
        self.current_series = 0;
        self.pixel_offset = 52;
        self.data = Vec::new();
        self.series_sources = Vec::new();
        self.series_planes = Vec::new();
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.metas.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.metas.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.metas
            .get(self.current_series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        let meta = self.metas.get(self.current_series)?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        if let Some(image) = ome.images.get_mut(0) {
            // Java initFile names every image "Well %s, Field #%02d" using
            // FormatTools.getWellName(row, col) (zero-padded column) and the
            // field index. We fall back to the plate/well metadata only when the
            // row/column could not be parsed.
            let row = cellomics_metadata_i64(&meta.series_metadata, "cellomics.well_row");
            let col = cellomics_metadata_i64(&meta.series_metadata, "cellomics.well_column");
            let field =
                cellomics_metadata_i64(&meta.series_metadata, "cellomics.field_index").unwrap_or(0);
            if let (Some(row), Some(col)) = (row, col) {
                image.name = Some(format!(
                    "Well {}, Field #{:02}",
                    cellomics_well_name(row.max(0) as i32, col.max(0) as i32),
                    field
                ));
            } else if let Some(plate) =
                cellomics_metadata_string(&meta.series_metadata, "cellomics.plate")
            {
                let well = cellomics_metadata_string(&meta.series_metadata, "cellomics.well")
                    .map(|well| format!(" {well}"))
                    .unwrap_or_default();
                image.name = Some(format!("{plate}{well}"));
            }
            image.physical_size_x =
                cellomics_metadata_f64(&meta.series_metadata, "cellomics.channel.0.pixel_size_x")
                    .or_else(|| {
                        cellomics_metadata_f64(
                            &meta.series_metadata,
                            "cellomics.channel.0.pixel_size",
                        )
                    });
            image.physical_size_y =
                cellomics_metadata_f64(&meta.series_metadata, "cellomics.channel.0.pixel_size_y")
                    .or_else(|| {
                        cellomics_metadata_f64(
                            &meta.series_metadata,
                            "cellomics.channel.0.pixel_size",
                        )
                    });
            for (channel_index, channel) in image.channels.iter_mut().enumerate() {
                let prefix = format!("cellomics.channel.{channel_index}");
                channel.name =
                    cellomics_metadata_string(&meta.series_metadata, &format!("{prefix}.name"));
                channel.emission_wavelength = cellomics_metadata_f64(
                    &meta.series_metadata,
                    &format!("{prefix}.emission_wavelength"),
                );
                channel.excitation_wavelength = cellomics_metadata_f64(
                    &meta.series_metadata,
                    &format!("{prefix}.excitation_wavelength"),
                );
                channel.color = cellomics_metadata_i64(
                    &meta.series_metadata,
                    &format!("{prefix}.composite_color"),
                )
                .and_then(cellomics_ome_color);
            }
        }

        // Port of the OME Plate/Well/WellSample population from Java initFile.
        // The plate is global in Java; here each series exposes the plate frame
        // (id, name, snapped rows/columns) plus the single well/well-sample that
        // this series populates, with the well sample referencing image 0.
        if let (Some(real_rows), Some(real_cols)) = (
            cellomics_metadata_i64(&meta.series_metadata, "cellomics.plate.real_rows"),
            cellomics_metadata_i64(&meta.series_metadata, "cellomics.plate.real_columns"),
        ) {
            let mut plate = OmePlate {
                id: Some(create_lsid("Plate", &[0])),
                name: cellomics_metadata_string(&meta.series_metadata, "cellomics.plate.name"),
                rows: real_rows.max(0) as u32,
                columns: real_cols.max(0) as u32,
                wells: Vec::new(),
            };
            if let (Some(well_index), Some(well_sample_index)) = (
                cellomics_metadata_i64(&meta.series_metadata, "cellomics.plate.well_index"),
                cellomics_metadata_i64(&meta.series_metadata, "cellomics.plate.well_sample_index"),
            ) {
                let cols = plate.columns.max(1);
                let well = well_index.max(0) as u32;
                plate.wells.push(OmeWell {
                    id: Some(create_lsid("Well", &[0, well as usize])),
                    row: well / cols,
                    column: well % cols,
                    well_samples: vec![OmeWellSample {
                        id: Some(create_lsid("WellSample", &[0, well as usize, 0])),
                        index: well_sample_index.max(0) as u32,
                        image_ref: Some(0),
                        position_x: None,
                        position_y: None,
                    }],
                });
            }
            ome.plates.push(plate);
        }

        let _ = ome.add_original_metadata_annotations(meta, 0);
        Some(ome)
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bytes_per_pixel = (meta.bits_per_pixel / 8) as usize;
        let n_bytes = meta.size_x as usize * meta.size_y as usize * bytes_per_pixel;
        let planes = self
            .series_planes
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let sources = self
            .series_sources
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let plane_source = planes
            .get(plane_index as usize)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let source = sources
            .get(plane_source.source_index)
            .ok_or_else(|| BioFormatsError::Format("Cellomics source index is invalid".into()))?;
        // Pixel data lives in the decoded (decompressed for .c01) buffer.
        let plane_offset = source
            .pixel_offset
            .checked_add(
                (plane_source.plane_index as u64)
                    .checked_mul(n_bytes as u64)
                    .ok_or_else(|| {
                        BioFormatsError::Format("Cellomics plane offset overflows".to_string())
                    })?,
            )
            .ok_or_else(|| {
                BioFormatsError::Format("Cellomics plane offset overflows".to_string())
            })? as usize;
        if plane_offset + n_bytes > source.data.len() {
            return Err(BioFormatsError::InvalidData(
                "Cellomics plane extends beyond decoded payload".to_string(),
            ));
        }
        Ok(source.data[plane_offset..plane_offset + n_bytes].to_vec())
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        let meta = self
            .metas
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Cellomics", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Group B — Extension-only placeholder readers
// ===========================================================================

// ---------------------------------------------------------------------------
// 7. Minolta Digital Camera RAW — TIFF delegate
// ---------------------------------------------------------------------------
/// Minolta MRW (Minolta RAW) reader (`.mrw`).
///
/// Ported from the upstream Java `MRWReader`. An MRW file is **not** a TIFF; it
/// is a block-structured binary file. After a 4-byte magic ("\0MRM"), a 32-bit
/// big-endian length gives the offset to the Bayer pixel data (`length + 8`).
/// Between the magic and the data, named 4-character blocks describe the image:
///   - `PRD`: sensor and output dimensions, the per-sample bit depth and the
///     Bayer pattern.
///   - `WBG`: white-balance gains.
///   - `TTW`: an embedded TIFF block of EXIF-style metadata.
///
/// The pixel data is a single-channel Bayer mosaic that the Java reader
/// demosaics (via `ImageTools.interpolate`) into an interleaved RGB UINT16
/// big-endian plane. This port reproduces the Java `openBytes` path exactly:
/// the packed `dataSize`-bit CFA samples are read MSB-first, white-balance
/// gains are applied, the mosaic is split into a planar [R|G|B] buffer using
/// the appropriate color map (`COLOR_MAP_1`/`COLOR_MAP_2`), and
/// `ImageTools.interpolate` fills the missing components into an interleaved
/// big-endian RGB plane.
pub struct MrwReader {
    meta: Option<ImageMetadata>,
    path: Option<PathBuf>,
    sensor_width: u32,
    sensor_height: u32,
    bayer_pattern: u8,
    data_size: u8,
    pixel_offset: u64,
    wbg: [f32; 4],
    /// Cached demosaiced interleaved RGB plane (Java caches `fullImage`).
    full_image: Option<Vec<u8>>,
}

impl MrwReader {
    /// Bayer color maps from `MRWReader.java`.
    const COLOR_MAP_1: [i32; 4] = [0, 1, 1, 2];
    const COLOR_MAP_2: [i32; 4] = [1, 2, 0, 1];

    pub fn new() -> Self {
        MrwReader {
            meta: None,
            path: None,
            sensor_width: 0,
            sensor_height: 0,
            bayer_pattern: 0,
            data_size: 0,
            pixel_offset: 0,
            wbg: [1.0; 4],
            full_image: None,
        }
    }

    /// Port of `MRWReader.openBytes` (the full-plane decode). Returns the
    /// demosaiced interleaved RGB UINT16 big-endian plane.
    fn decode_full_image(&self) -> Result<Vec<u8>> {
        use crate::formats::camera2::cfa;

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;

        let size_x = self.meta.as_ref().unwrap().size_x as usize;
        let size_y = self.meta.as_ref().unwrap().size_y as usize;
        let sensor_w = self.sensor_width as usize;
        let data_size = self.data_size as u32;

        let offset = self.pixel_offset as usize;
        if offset > data.len() {
            return Err(BioFormatsError::UnsupportedFormat(
                "MRW: pixel offset past end of file".into(),
            ));
        }
        // Java seeks to `offset` then reads a continuous bit stream.
        let mut bits = cfa::BitReader::new(&data[offset..]);

        // Planar [R|G|B] short buffer.
        let mut s = vec![0i16; size_x * size_y * 3];

        for row in 0..size_y {
            let even_row = (row % 2) == 0;
            for col in 0..size_x {
                let even_col = (col % 2) == 0;
                let raw = (bits.read_bits(data_size) & 0xffff) as u16 as i16;

                let red_offset = row * size_x + col;
                let green_offset = (size_y + row) * size_x + col;
                let blue_offset = (2 * size_y + row) * size_x + col;

                // Java applies wbg via `(short)(val * wbg[k])` (float -> short).
                let val: i16;
                if even_row {
                    if even_col {
                        val = (raw as f32 * self.wbg[0]) as i16;
                        if self.bayer_pattern == 1 {
                            s[red_offset] = val;
                        } else {
                            s[green_offset] = val;
                        }
                    } else {
                        val = (raw as f32 * self.wbg[1]) as i16;
                        if self.bayer_pattern == 1 {
                            s[green_offset] = val;
                        } else {
                            s[blue_offset] = val;
                        }
                    }
                } else if even_col {
                    val = (raw as f32 * self.wbg[2]) as i16;
                    if self.bayer_pattern == 1 {
                        s[green_offset] = val;
                    } else {
                        s[red_offset] = val;
                    }
                } else {
                    val = (raw as f32 * self.wbg[3]) as i16;
                    if self.bayer_pattern == 1 {
                        s[blue_offset] = val;
                    } else {
                        s[green_offset] = val;
                    }
                }
            }
            // Java: in.skipBits(dataSize * (sensorWidth - getSizeX())).
            bits.skip_bits((data_size as usize) * (sensor_w - size_x));
        }

        let color_map = if self.bayer_pattern == 1 {
            Self::COLOR_MAP_1
        } else {
            Self::COLOR_MAP_2
        };

        // m.littleEndian = false in initFile.
        let mut full = vec![0u8; size_x * size_y * 3 * 2];
        cfa::interpolate(&s, &mut full, &color_map, size_x, size_y, false);
        Ok(full)
    }
}

impl Default for MrwReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MrwReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("mrw"))
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4 && header[..4].ends_with(b"MRM")
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 8 || !data[..4].ends_with(b"MRM") {
            return Err(BioFormatsError::UnsupportedFormat(
                "MRW: missing 'MRM' magic string".into(),
            ));
        }
        // Big-endian throughout. offset = readInt(@4) + 8.
        let be_i32 = |o: usize| -> i64 {
            i32::from_be_bytes([data[o], data[o + 1], data[o + 2], data[o + 3]]) as i64
        };
        let be_i16 = |o: usize| -> i16 { i16::from_be_bytes([data[o], data[o + 1]]) };

        let offset = (be_i32(4) + 8) as u64;
        let mut size_x = 0u32;
        let mut size_y = 0u32;
        let mut sensor_w = 0u32;
        let mut sensor_h = 0u32;
        let mut data_size = 0u8;
        let mut bayer = 0u8;
        let mut wbg = [1.0f32; 4];

        let mut fp = 8usize;
        while (fp as u64) < offset && fp + 8 <= data.len() {
            let block_name = &data[fp..fp + 4];
            let len = i32::from_be_bytes([data[fp + 4], data[fp + 5], data[fp + 6], data[fp + 7]])
                .max(0) as usize;
            let body = fp + 8;
            if block_name.ends_with(b"PRD") {
                // skip 8, sensorHeight(short), sensorWidth(short),
                // sizeY(short), sizeX(short), dataSize(byte), skip 1,
                // storageMethod(byte), skip 4, bayerPattern(byte)
                if body + 17 <= data.len() {
                    sensor_h = be_i16(body + 8) as u16 as u32;
                    sensor_w = be_i16(body + 10) as u16 as u32;
                    size_y = be_i16(body + 12) as u16 as u32;
                    size_x = be_i16(body + 14) as u16 as u32;
                    data_size = data[body + 16];
                    // body+17 skip, body+18 storageMethod, body+19..23 skip,
                    // body+23 bayerPattern
                    if body + 24 <= data.len() {
                        bayer = data[body + 23];
                    }
                }
            } else if block_name.ends_with(b"WBG") {
                // 4-byte scale array, then 4 big-endian shorts: coeff/(64<<scale)
                if body + 12 <= data.len() {
                    let scale = &data[body..body + 4];
                    for i in 0..4 {
                        if scale[i] >= 32 {
                            return Err(BioFormatsError::UnsupportedFormat(
                                "MRW: WBG scale value is too large".into(),
                            ));
                        }
                        let coeff = be_i16(body + 4 + i * 2) as f32;
                        wbg[i] = coeff / ((64u32 << scale[i]) as f32);
                    }
                }
            }
            // TTW block (embedded TIFF metadata) is parsed by Java for global
            // metadata only; not required for pixel layout.
            fp = body + len;
        }

        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "MRW: PRD block did not yield image dimensions".into(),
            ));
        }
        if sensor_w < size_x || sensor_h < size_y {
            return Err(BioFormatsError::UnsupportedFormat(
                "MRW: sensor dimensions are smaller than image dimensions".into(),
            ));
        }
        if data_size == 0 || data_size > 16 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "MRW: unsupported sample bit depth {data_size}"
            )));
        }

        self.sensor_width = sensor_w;
        self.sensor_height = sensor_h;
        self.bayer_pattern = bayer;
        self.data_size = data_size;
        self.pixel_offset = offset;
        self.wbg = wbg;
        self.path = Some(path.to_path_buf());
        self.full_image = None;

        // Java: RGB UINT16, big-endian, interleaved, dimensionOrder XYCZT,
        // sizeC = 3, sizeZ = sizeT = 1, imageCount = 1, bitsPerPixel = dataSize.
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z: 1,
            size_c: 3,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: if data_size > 0 { data_size } else { 16 },
            image_count: 1,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: true,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: false,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        self.meta = None;
        self.path = None;
        self.sensor_width = 0;
        self.sensor_height = 0;
        self.bayer_pattern = 0;
        self.data_size = 0;
        self.pixel_offset = 0;
        self.wbg = [1.0; 4];
        self.full_image = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        1
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }
    fn series(&self) -> usize {
        0
    }
    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if p != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(p));
        }
        if self.full_image.is_none() {
            self.full_image = Some(self.decode_full_image()?);
        }
        Ok(self.full_image.clone().unwrap())
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self.metadata().clone();
        crop_full_plane("MRW", &full, &meta, 3, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.open_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        1
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// 8. Yokogawa CV7000/8000 HCS — XML index + TIFF images
// ---------------------------------------------------------------------------
/// Yokogawa CV7000/8000 HCS reader (`.wpi`).
///
/// Ported from the upstream Java `CV7000Reader`. A CV7000 acquisition is a
/// directory of single-plane TIFFs indexed by three XML files:
///   - `*.wpi`         — the entry point; describes the well plate (rows/cols).
///   - `MeasurementData.mlf`   — one `bts:MeasurementRecord` per acquired image,
///     giving its well row/column, field, Z, channel, timepoint and the TIFF
///     filename.
///   - `MeasurementDetail.mrf` — the acquired channels (pixel sizes etc.).
///
/// Each (well, field) combination becomes a series; planes within a series are
/// addressed in XYCZT order. `open_bytes` delegates to the per-plane TIFF.
pub struct YokogawaReader {
    inner: crate::tiff::TiffReader,
    tiff_loaded: bool,
    series: Vec<ImageMetadata>,
    current_series: usize,
    /// For each series, plane_index -> TIFF file (None for missing planes).
    plane_files: Vec<Vec<Option<PathBuf>>>,
    plate: YokogawaPlate,
    /// For each series: (well_ordinal, field) for OME mapping.
    series_well_field: Vec<(usize, usize)>,
    /// Populated wells in raster order, each (row, col).
    wells: Vec<(u32, u32)>,
    fields: usize,
    /// Physical pixel size (X, Y) in micrometres from the first channel, if any.
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
}

#[derive(Default, Clone)]
struct YokogawaPlate {
    name: Option<String>,
    rows: u32,
    columns: u32,
}

#[derive(Clone)]
struct YokogawaPlane {
    row: u32,
    column: u32,
    field: u32,
    z: i32,
    channel: i32,
    timepoint: i32,
    action_index: i32,
    timeline_index: i32,
    file: Option<PathBuf>,
}

#[derive(Clone, Default)]
struct YokogawaChannel {
    index: i32,
    action_index: i32,
    timeline_index: i32,
    x_size: Option<f64>,
    y_size: Option<f64>,
}

impl YokogawaReader {
    pub fn new() -> Self {
        YokogawaReader {
            inner: crate::tiff::TiffReader::new(),
            tiff_loaded: false,
            series: Vec::new(),
            current_series: 0,
            plane_files: Vec::new(),
            plate: YokogawaPlate::default(),
            series_well_field: Vec::new(),
            wells: Vec::new(),
            fields: 1,
            physical_size_x: None,
            physical_size_y: None,
        }
    }
}

impl Default for YokogawaReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Read every `name="value"` style attribute from the start tag and return the
/// requested one. `bts:` prefixes are preserved by quick_xml.
fn yk_attr(
    e: &quick_xml::events::BytesStart,
    decoder: quick_xml::encoding::Decoder,
    name: &str,
) -> Option<String> {
    for a in e.attributes().flatten() {
        if a.key.as_ref() == name.as_bytes() {
            return crate::common::xml::decode_xml_attr(a, decoder);
        }
    }
    None
}

fn yk_attr_int(
    e: &quick_xml::events::BytesStart,
    decoder: quick_xml::encoding::Decoder,
    name: &str,
) -> Option<i64> {
    yk_attr(e, decoder, name).and_then(|s| s.trim().parse::<i64>().ok())
}

fn yk_attr_positive_i64(
    e: &quick_xml::events::BytesStart,
    decoder: quick_xml::encoding::Decoder,
    name: &str,
) -> Result<i64> {
    let value = yk_attr_int(e, decoder, name).unwrap_or(1);
    if value <= 0 {
        return Err(BioFormatsError::Format(format!(
            "Yokogawa CV7000 attribute {name} must be positive, got {value}"
        )));
    }
    Ok(value)
}

fn yk_attr_f64(
    e: &quick_xml::events::BytesStart,
    decoder: quick_xml::encoding::Decoder,
    name: &str,
) -> Option<f64> {
    yk_attr(e, decoder, name).and_then(|s| s.trim().parse::<f64>().ok())
}

/// Read a file and strip a stray trailing '>' (mirrors readSanitizedXML).
fn yk_read_sanitized(path: &Path) -> Result<String> {
    let mut s = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let trimmed = s.trim_end();
    if trimmed.ends_with(">>") {
        s = trimmed[..trimmed.len() - 1].to_string();
    } else {
        s = trimmed.to_string();
    }
    Ok(s)
}

fn yk_parse_wpi(xml: &str) -> YokogawaPlate {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut plate = YokogawaPlate::default();
    loop {
        match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().as_ref() == b"bts:WellPlate" {
                    plate.name = yk_attr(e, reader.decoder(), "bts:Name");
                    plate.rows = yk_attr_int(e, reader.decoder(), "bts:Rows").unwrap_or(0) as u32;
                    plate.columns =
                        yk_attr_int(e, reader.decoder(), "bts:Columns").unwrap_or(0) as u32;
                }
            }
            _ => {}
        }
    }
    plate
}

fn yk_parse_mlf(xml: &str, parent: &Path) -> Result<Vec<YokogawaPlane>> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut planes: Vec<YokogawaPlane> = Vec::new();
    let mut current_text = String::new();
    let mut in_img_record = false;
    loop {
        match reader.read_event() {
            Ok(Event::Eof) => break,
            Err(e) => {
                return Err(BioFormatsError::Format(format!(
                    "Yokogawa CV7000 MeasurementData.mlf XML parse error: {e}"
                )));
            }
            Ok(Event::Start(ref e)) => {
                if e.name().as_ref() == b"bts:MeasurementRecord" {
                    current_text.clear();
                    let bts_type = yk_attr(e, reader.decoder(), "bts:Type").unwrap_or_default();
                    if bts_type == "IMG" {
                        in_img_record = true;
                        // attributes are 1-based in the file; convert to 0-based.
                        let p = YokogawaPlane {
                            row: (yk_attr_positive_i64(e, reader.decoder(), "bts:Row")? - 1) as u32,
                            column: (yk_attr_positive_i64(e, reader.decoder(), "bts:Column")? - 1)
                                as u32,
                            field: (yk_attr_positive_i64(e, reader.decoder(), "bts:FieldIndex")?
                                - 1) as u32,
                            z: (yk_attr_positive_i64(e, reader.decoder(), "bts:ZIndex")? - 1)
                                as i32,
                            channel: (yk_attr_positive_i64(e, reader.decoder(), "bts:Ch")? - 1)
                                as i32,
                            timepoint: (yk_attr_positive_i64(e, reader.decoder(), "bts:TimePoint")?
                                - 1) as i32,
                            action_index: (yk_attr_positive_i64(
                                e,
                                reader.decoder(),
                                "bts:ActionIndex",
                            )? - 1) as i32,
                            timeline_index: (yk_attr_positive_i64(
                                e,
                                reader.decoder(),
                                "bts:TimelineIndex",
                            )? - 1) as i32,
                            file: None,
                        };
                        planes.push(p);
                    } else {
                        in_img_record = false;
                    }
                }
            }
            Ok(Event::Text(t)) => {
                if in_img_record {
                    current_text
                        .push_str(&crate::common::xml::decode_xml_text(&t).unwrap_or_default());
                }
            }
            Ok(Event::GeneralRef(r)) => {
                if in_img_record {
                    current_text
                        .push_str(&crate::common::xml::decode_xml_ref(&r).unwrap_or_default());
                }
            }
            Ok(Event::End(ref e)) => {
                if e.name().as_ref() == b"bts:MeasurementRecord" && in_img_record {
                    let value = current_text.trim();
                    if !value.is_empty() {
                        let img = parent.join(value);
                        if let Some(last) = planes.last_mut() {
                            if img.exists() {
                                last.file = Some(img);
                            }
                        }
                    }
                    in_img_record = false;
                    current_text.clear();
                }
            }
            _ => {}
        }
    }
    Ok(planes)
}

fn yk_parse_mrf(xml: &str) -> Vec<YokogawaChannel> {
    use quick_xml::events::Event;
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut channels: Vec<YokogawaChannel> = Vec::new();
    loop {
        match reader.read_event() {
            Ok(Event::Eof) | Err(_) => break,
            Ok(Event::Start(ref e)) | Ok(Event::Empty(ref e)) => {
                if e.name().as_ref() == b"bts:MeasurementChannel" {
                    channels.push(YokogawaChannel {
                        index: (yk_attr_int(e, reader.decoder(), "bts:Ch").unwrap_or(1) - 1) as i32,
                        action_index: 0,
                        timeline_index: 0,
                        x_size: yk_attr_f64(e, reader.decoder(), "bts:HorizontalPixelDimension"),
                        y_size: yk_attr_f64(e, reader.decoder(), "bts:VerticalPixelDimension"),
                    });
                }
            }
            _ => {}
        }
    }
    channels
}

impl YokogawaReader {
    fn build(&mut self, wpi_path: &Path) -> Result<()> {
        let parent = wpi_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let plate = yk_parse_wpi(&yk_read_sanitized(wpi_path)?);
        if plate.rows == 0 || plate.columns == 0 {
            return Err(BioFormatsError::Format(
                "Yokogawa CV7000: plate rows and columns must be positive".into(),
            ));
        }

        // MeasurementData.mlf is required.
        let mlf_path = parent.join("MeasurementData.mlf");
        if !mlf_path.exists() {
            return Err(BioFormatsError::UnsupportedFormat(
                "Yokogawa CV7000: missing MeasurementData.mlf index file".into(),
            ));
        }
        let planes = yk_parse_mlf(&yk_read_sanitized(&mlf_path)?, &parent)?;

        // MeasurementDetail.mrf is optional (channels / pixel sizes).
        let mrf_path = parent.join("MeasurementDetail.mrf");
        let channels = if mrf_path.exists() {
            yk_parse_mrf(&yk_read_sanitized(&mrf_path)?)
        } else {
            Vec::new()
        };

        let plate_columns = plate.columns.max(1);

        // Determine acquired wells, fields, channels and per-well Z/T ranges.
        use std::collections::{HashMap, HashSet};
        let mut unique_wells: HashSet<u32> = HashSet::new();
        let mut unique_channels: HashSet<i32> = HashSet::new();
        let mut fields = 0usize;
        // per-well min/max Z and T
        let mut zmin: HashMap<u32, i32> = HashMap::new();
        let mut zmax: HashMap<u32, i32> = HashMap::new();
        let mut tmin: HashMap<u32, i32> = HashMap::new();
        let mut tmax: HashMap<u32, i32> = HashMap::new();
        let mut first_file: Option<PathBuf> = None;
        let mut acquired_wells: HashSet<u32> = HashSet::new();

        for p in &planes {
            if p.file.is_some() {
                acquired_wells.insert(p.row * plate_columns + p.column);
            }
        }

        for p in &planes {
            if p.row >= plate.rows || p.column >= plate.columns {
                return Err(BioFormatsError::Format(format!(
                    "Yokogawa CV7000 plane references well row {}, column {} outside declared plate {}x{}",
                    p.row + 1,
                    p.column + 1,
                    plate.rows,
                    plate.columns
                )));
            }
            let well_number = p.row * plate_columns + p.column;
            if !acquired_wells.contains(&well_number) {
                continue;
            }
            if first_file.is_none() && p.file.is_some() {
                first_file = p.file.clone();
            }
            unique_wells.insert(well_number);
            unique_channels.insert(p.channel);
            if (p.field as usize) + 1 > fields {
                fields = p.field as usize + 1;
            }
            *zmin.entry(well_number).or_insert(i32::MAX) =
                (*zmin.get(&well_number).unwrap_or(&i32::MAX)).min(p.z);
            *zmax.entry(well_number).or_insert(i32::MIN) =
                (*zmax.get(&well_number).unwrap_or(&i32::MIN)).max(p.z);
            *tmin.entry(well_number).or_insert(i32::MAX) =
                (*tmin.get(&well_number).unwrap_or(&i32::MAX)).min(p.timepoint);
            *tmax.entry(well_number).or_insert(i32::MIN) =
                (*tmax.get(&well_number).unwrap_or(&i32::MIN)).max(p.timepoint);
        }
        let fields = fields.max(1);

        let first_file = first_file.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(
                "Yokogawa CV7000: MeasurementData.mlf referenced no existing image files".into(),
            )
        })?;

        // Probe the first TIFF for pixel parameters.
        let mut probe = crate::tiff::TiffReader::new();
        probe.set_id(&first_file)?;
        let pm = probe.metadata().clone();
        let _ = probe.close();
        let tiff_c = pm.size_c.max(1);

        // Sorted unique wells and channels.
        let mut wells: Vec<u32> = unique_wells.into_iter().collect();
        wells.sort_unstable();
        let mut channel_indexes: Vec<i32> = unique_channels.into_iter().collect();
        channel_indexes.sort_unstable();
        let n_channels = channel_indexes.len().max(1) as u32;

        let real_wells = wells.len();
        let series_count = real_wells * fields;

        // Build per-series metadata and plane-file lookup.
        let mut series = Vec::with_capacity(series_count);
        let mut plane_files: Vec<Vec<Option<PathBuf>>> = Vec::with_capacity(series_count);
        let mut series_well_field = Vec::with_capacity(series_count);
        for s in 0..series_count {
            let well_ordinal = s / fields;
            let field = s % fields;
            let well_number = wells[well_ordinal];
            let size_z = (zmax.get(&well_number).copied().unwrap_or(0)
                - zmin.get(&well_number).copied().unwrap_or(0)
                + 1)
            .max(1) as u32;
            let size_t = (tmax.get(&well_number).copied().unwrap_or(0)
                - tmin.get(&well_number).copied().unwrap_or(0)
                + 1)
            .max(1) as u32;
            let size_c = tiff_c * n_channels;
            let planes_per_series = (size_z * size_t * n_channels) as usize;

            let mut meta = pm.clone();
            meta.size_z = size_z;
            meta.size_t = size_t;
            meta.size_c = size_c;
            meta.image_count = size_z * size_t * n_channels;
            meta.dimension_order = DimensionOrder::XYCZT;
            meta.series_metadata.insert(
                "format".into(),
                crate::common::metadata::MetadataValue::String("Yokogawa CV7000".into()),
            );
            series.push(meta);
            plane_files.push(vec![None; planes_per_series]);
            series_well_field.push((well_ordinal, field));
        }

        // Map each plane record into (series, no) and record its file.
        for p in &planes {
            let well_number = p.row * plate_columns + p.column;
            if !acquired_wells.contains(&well_number) {
                continue;
            }
            let Ok(well_ordinal) = wells.binary_search(&well_number) else {
                continue;
            };
            if (p.field as usize) >= fields {
                continue;
            }
            let series_index = well_ordinal * fields + p.field as usize;
            if series_index >= series_count {
                continue;
            }
            // channel index into the unique acquired channels
            let channel_index = channel_indexes
                .binary_search(&yk_channel_index(p, &channels))
                .unwrap_or(0);
            let m = &series[series_index];
            let plane_c = (m.size_c / tiff_c).max(1);
            let plane_z = m.size_z.max(1);
            let z = (p.z - zmin.get(&well_number).copied().unwrap_or(0)).max(0) as u32;
            let t = (p.timepoint - tmin.get(&well_number).copied().unwrap_or(0)).max(0) as u32;
            // positionToRaster([C, Z, T], [channel, z, t]) for XYCZT order:
            // index = channel + plane_c*(z + plane_z*t)
            let no = channel_index as u32 + plane_c * (z + plane_z * t);
            if let Some(slot) = plane_files
                .get_mut(series_index)
                .and_then(|v| v.get_mut(no as usize))
            {
                if slot.is_none() {
                    *slot = p.file.clone();
                }
            }
        }

        for files in &plane_files {
            for file in files.iter().flatten() {
                let mut tr = crate::tiff::TiffReader::new();
                tr.set_id(file).map_err(|e| {
                    BioFormatsError::Format(format!(
                        "Yokogawa CV7000 companion TIFF {} could not be initialized: {e}",
                        file.display()
                    ))
                })?;
                if tr.metadata().image_count == 0 {
                    return Err(BioFormatsError::Format(format!(
                        "Yokogawa CV7000 companion TIFF {} has no image pages",
                        file.display()
                    )));
                }
                let _ = tr.close();
            }
        }

        self.series = series;
        self.plane_files = plane_files;
        self.plate = plate;
        self.series_well_field = series_well_field;
        self.wells = wells
            .iter()
            .map(|&w| (w / plate_columns, w % plate_columns))
            .collect();
        self.fields = fields;
        self.physical_size_x = channels.first().and_then(|c| c.x_size).filter(|&v| v > 0.0);
        self.physical_size_y = channels.first().and_then(|c| c.y_size).filter(|&v| v > 0.0);
        self.current_series = 0;
        Ok(())
    }
}

/// Compute the channel index of a plane within the list of acquired channels,
/// mirroring CV7000Reader.getChannelIndex (simplified: when channel metadata is
/// missing, fall back to the raw channel number).
fn yk_channel_index(p: &YokogawaPlane, channels: &[YokogawaChannel]) -> i32 {
    if channels.is_empty() {
        return p.channel;
    }
    let mut index = -1i32;
    for action in 0..=p.action_index {
        for ch in channels {
            if ch.timeline_index == p.timeline_index && ch.action_index == action {
                index += 1;
                if ch.index == p.channel && ch.action_index == p.action_index {
                    return index;
                }
            }
        }
    }
    if index < 0 {
        p.channel
    } else {
        index
    }
}

impl FormatReader for YokogawaReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("wpi") | Some("mrf"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        self.build(path)?;
        if self.series.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "Yokogawa CV7000: no series could be assembled from the index files".into(),
            ));
        }
        self.tiff_loaded = false;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.series.clear();
        self.plane_files.clear();
        self.series_well_field.clear();
        self.wells.clear();
        self.plate = YokogawaPlate::default();
        self.fields = 1;
        self.physical_size_x = None;
        self.physical_size_y = None;
        self.current_series = 0;
        if self.tiff_loaded {
            let _ = self.inner.close();
            self.tiff_loaded = false;
        }
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.series.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.series.is_empty() {
            Err(BioFormatsError::NotInitialized)
        } else if s >= self.series.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }
    fn series(&self) -> usize {
        self.current_series
    }
    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current_series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if p >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(p));
        }
        let plane_bytes =
            meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample();
        let file = self
            .plane_files
            .get(self.current_series)
            .and_then(|v| v.get(p as usize))
            .cloned()
            .flatten();
        let Some(file) = file else {
            // Java fills the requested buffer, then by default duplicates the
            // first Z/T plane in the same channel for missing planes.
            if p > 0 {
                let plane_c = (meta.image_count / (meta.size_z.max(1) * meta.size_t.max(1))).max(1);
                let duplicate = p % plane_c;
                if duplicate != p {
                    return self.open_bytes(duplicate);
                }
                return self.open_bytes(0);
            }
            return Ok(vec![0u8; plane_bytes]);
        };
        if self.tiff_loaded {
            let _ = self.inner.close();
        }
        self.inner.set_id(&file)?;
        self.tiff_loaded = true;
        self.inner.open_bytes(0)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        crop_full_plane("Yokogawa CV7000", &full, &meta, 1, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(p, tx, ty, tw, th)
    }
    fn resolution_count(&self) -> usize {
        1
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }

    /// Build OME HCS metadata: one Plate with Wells/WellSamples mapping each
    /// (well, field) series to an Image. Mirrors CV7000Reader's MetadataStore.
    fn ome_metadata(&self) -> Option<OmeMetadata> {
        if self.series.is_empty() {
            return None;
        }
        let mut images = Vec::with_capacity(self.series.len());
        for (s, (well_ordinal, field)) in self.series_well_field.iter().enumerate() {
            let (row, col) = self.wells.get(*well_ordinal).copied().unwrap_or((0, 0));
            let name = format!("Well {}{}, Field {}", yk_row_name(row), col + 1, field + 1);
            let _ = s;
            images.push(OmeImage {
                name: Some(name),
                physical_size_x: self.physical_size_x,
                physical_size_y: self.physical_size_y,
                ..Default::default()
            });
        }

        let mut wells = Vec::with_capacity(self.wells.len());
        for (well_ordinal, &(row, col)) in self.wells.iter().enumerate() {
            let mut well_samples = Vec::with_capacity(self.fields);
            for field in 0..self.fields {
                let series = well_ordinal * self.fields + field;
                if series >= self.series.len() {
                    continue;
                }
                well_samples.push(OmeWellSample {
                    id: Some(create_lsid("WellSample", &[0, well_ordinal, field])),
                    index: series as u32,
                    image_ref: Some(series),
                    position_x: None,
                    position_y: None,
                });
            }
            wells.push(OmeWell {
                id: Some(create_lsid("Well", &[0, well_ordinal])),
                row,
                column: col,
                well_samples,
            });
        }

        let plate = OmePlate {
            id: Some(create_lsid("Plate", &[0])),
            name: self.plate.name.clone(),
            rows: self.plate.rows,
            columns: self.plate.columns,
            wells,
        };

        Some(OmeMetadata {
            images,
            plates: vec![plate],
            ..Default::default()
        })
    }
}

/// Well row letter (0 -> "A", 25 -> "Z", 26 -> "AA", ...).
fn yk_row_name(row: u32) -> String {
    let mut n = row as i64;
    let mut s = String::new();
    loop {
        let rem = (n % 26) as u8;
        s.insert(0, (b'A' + rem) as char);
        n = n / 26 - 1;
        if n < 0 {
            break;
        }
    }
    s
}

// ---------------------------------------------------------------------------
// 9. Leica single-image LOF
// ---------------------------------------------------------------------------
/// Leica Object Format (`.lof`) — single-image Leica container.
///
/// Faithful translation of upstream Java `LOFReader` (which extends
/// `LMSFileReader`). A LOF file has three parts:
///   1. a binary header (`0x70` magic, the `LMS_Object_File` type string, major
///      and minor version ints, and a 64-bit memory size),
///   2. the memory block holding the raw pixel data (`memorySize` bytes), and
///   3. an XML section (`0x70` magic again) carrying a UTF-16LE Leica
///      `<ImageDescription>`.
///
/// The header / layout parsing (`checkForLofLayout`) and pixel addressing
/// (`openBytes` / `seekStartOfPlane`) are ported directly. Dimension and pixel
/// type parsing reuses the Leica `<ImageDescription>` schema shared with LIF
/// (`<Dimensions>`/`<DimensionDescription>` with `DimID` 1=X 2=Y 3=Z 4=T,
/// 10=tile, plus `<Channels>`/`<ChannelDescription>`), mirroring
/// `translateImageNodes`. Direct channel attributes (names, wavelengths, LUT
/// names) are projected as conservative metadata. Wider Leica metadata is
/// bounded to safe scalar XML attributes for instruments, detectors, ROI, stage
/// and acquisition fields. RGB channel order is recorded from explicit
/// `ChannelDescription BytesInc` offsets when the simple Leica XML layout is
/// unambiguous; pixel bytes are still returned in stored order.
const LOF_MAGIC_BYTE: u32 = 0x70;
const LOF_MEMORY_BYTE: u8 = 0x2a;
const LOF_TYPE_NAME: &str = "LMS_Object_File";

/// In-memory little/big-endian byte cursor mirroring the subset of
/// `loci.common.RandomAccessInputStream` used by the NAF and LOF readers.
struct ByteCursor<'a> {
    data: &'a [u8],
    pos: usize,
    little: bool,
}

impl<'a> ByteCursor<'a> {
    fn new(data: &'a [u8], little: bool) -> Self {
        ByteCursor {
            data,
            pos: 0,
            little,
        }
    }
    fn len(&self) -> usize {
        self.data.len()
    }
    fn pos(&self) -> usize {
        self.pos
    }
    fn seek(&mut self, p: usize) {
        self.pos = p.min(self.data.len());
    }
    fn skip(&mut self, n: usize) {
        self.pos = self.pos.saturating_add(n).min(self.data.len());
    }
    /// Java `read()`: one unsigned byte, or `None` at EOF.
    fn read_u8_opt(&mut self) -> Option<u8> {
        if self.pos < self.data.len() {
            let b = self.data[self.pos];
            self.pos += 1;
            Some(b)
        } else {
            None
        }
    }
    fn ensure(&self, n: usize, what: &str) -> Result<()> {
        if self.pos + n > self.data.len() {
            Err(BioFormatsError::Format(format!(
                "{what}: unexpected end of file"
            )))
        } else {
            Ok(())
        }
    }
    fn read_i16(&mut self, what: &str) -> Result<i16> {
        self.ensure(2, what)?;
        let p = self.pos;
        let v = if self.little {
            i16::from_le_bytes([self.data[p], self.data[p + 1]])
        } else {
            i16::from_be_bytes([self.data[p], self.data[p + 1]])
        };
        self.pos += 2;
        Ok(v)
    }
    fn read_i32(&mut self, what: &str) -> Result<i32> {
        self.ensure(4, what)?;
        let p = self.pos;
        let v = if self.little {
            i32::from_le_bytes([
                self.data[p],
                self.data[p + 1],
                self.data[p + 2],
                self.data[p + 3],
            ])
        } else {
            i32::from_be_bytes([
                self.data[p],
                self.data[p + 1],
                self.data[p + 2],
                self.data[p + 3],
            ])
        };
        self.pos += 4;
        Ok(v)
    }
    fn read_i64(&mut self, what: &str) -> Result<i64> {
        self.ensure(8, what)?;
        let p = self.pos;
        let mut a = [0u8; 8];
        a.copy_from_slice(&self.data[p..p + 8]);
        self.pos += 8;
        Ok(if self.little {
            i64::from_le_bytes(a)
        } else {
            i64::from_be_bytes(a)
        })
    }
    fn read_f32(&mut self, what: &str) -> Result<f32> {
        self.ensure(4, what)?;
        let p = self.pos;
        let v = if self.little {
            f32::from_le_bytes([
                self.data[p],
                self.data[p + 1],
                self.data[p + 2],
                self.data[p + 3],
            ])
        } else {
            f32::from_be_bytes([
                self.data[p],
                self.data[p + 1],
                self.data[p + 2],
                self.data[p + 3],
            ])
        };
        self.pos += 4;
        Ok(v)
    }
    /// Java `readString(n)`: `n` bytes interpreted as ISO-8859-1.
    fn read_string(&mut self, n: usize, what: &str) -> Result<String> {
        self.ensure(n, what)?;
        let s: String = self.data[self.pos..self.pos + n]
            .iter()
            .map(|&c| c as char)
            .collect();
        self.pos += n;
        Ok(s)
    }
    /// Java `readCString()`: bytes up to and consuming the next NUL terminator.
    fn read_cstring(&mut self) -> String {
        let mut s = String::new();
        while let Some(b) = self.read_u8_opt() {
            if b == 0 {
                break;
            }
            s.push(b as char);
        }
        s
    }
    /// Java loop of `readChar()`: `count` UTF-16 code units (2 bytes each).
    fn read_utf16(&mut self, count: i32, what: &str) -> Result<String> {
        if count < 0 {
            return Err(BioFormatsError::Format(format!("{what}: negative length")));
        }
        let n = count as usize;
        self.ensure(n * 2, what)?;
        let mut units = Vec::with_capacity(n);
        for _ in 0..n {
            let p = self.pos;
            units.push(if self.little {
                u16::from_le_bytes([self.data[p], self.data[p + 1]])
            } else {
                u16::from_be_bytes([self.data[p], self.data[p + 1]])
            });
            self.pos += 2;
        }
        Ok(String::from_utf16_lossy(&units))
    }
}

pub struct LofReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    ome: Option<OmeImage>,
    /// File offset of the first pixel byte (Java `offsets.get(0)`).
    data_offset: u64,
    /// End of pixel data (Java `endPointer`; defaults to the file length).
    end_pointer: u64,
    /// Number of tiles (Java `metaTemp.tileCount[0]`); also the series count.
    tile_count: u32,
    /// Bytes-per-tile increment (Java `metaTemp.tileBytesInc[0]`).
    tile_bytes_inc: u64,
    /// Whether `checkForLofLayout` found a non-empty memory block offset.
    pixel_memory_present: bool,
    current_series: usize,
}

impl LofReader {
    pub fn new() -> Self {
        LofReader {
            path: None,
            meta: None,
            ome: None,
            data_offset: 0,
            end_pointer: 0,
            tile_count: 1,
            tile_bytes_inc: 0,
            pixel_memory_present: false,
            current_series: 0,
        }
    }

    /// Lightweight LOF detection from a header prefix: validates the leading
    /// `0x70` magic, the `0x2A` type marker, and the `LMS_Object_File` type
    /// name. The full Java `isThisType` additionally walks past the memory block
    /// to confirm the XML carries an image node, which is not reachable from a
    /// bounded header slice.
    fn check_magic(header: &[u8]) -> bool {
        let mut c = ByteCursor::new(header, true);
        if c.read_i32("lof magic").ok().map(|v| v as u32) != Some(LOF_MAGIC_BYTE) {
            return false;
        }
        if c.read_i32("lof chunk").is_err() {
            return false;
        }
        if c.read_u8_opt() != Some(LOF_MEMORY_BYTE) {
            return false;
        }
        let nc = match c.read_i32("lof nc") {
            Ok(n) => n,
            Err(_) => return false,
        };
        if nc != LOF_TYPE_NAME.chars().count() as i32 {
            return false;
        }
        matches!(c.read_utf16(nc, "lof type"), Ok(name) if name == LOF_TYPE_NAME)
    }

    /// Java `seekStartOfPlane`: maps a plane index to an absolute file offset,
    /// taking the tile dimension into account.
    fn seek_start_of_plane(&self, no: u32, plane_size: u64) -> u64 {
        let number_of_tiles = self.tile_count.max(1) as u64;
        if number_of_tiles > 1 && plane_size > 0 {
            let bytes_inc_per_tile = self.tile_bytes_inc;
            let frames_per_tile = bytes_inc_per_tile / plane_size;
            if frames_per_tile == 0 {
                return self.data_offset + no as u64 * plane_size;
            }
            let no_outside_tiles = no as u64 / frames_per_tile;
            let no_inside_tiles = no as u64 % frames_per_tile;
            // Single tile group, so the tile within the group is the series.
            let tile = self.current_series as u64;
            self.data_offset
                + no_outside_tiles * bytes_inc_per_tile * number_of_tiles
                + tile * bytes_inc_per_tile
                + no_inside_tiles * plane_size
        } else {
            self.data_offset + no as u64 * plane_size
        }
    }
}

impl Default for LofReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LofReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("lof"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        LofReader::check_magic(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let file_len = bytes.len() as u64;
        let mut c = ByteCursor::new(&bytes, true);

        // ---- Part 1: header (Java LOFReader.checkForLofLayout) ----
        if c.read_i32("LOF header magic")? as u32 != LOF_MAGIC_BYTE {
            return Err(BioFormatsError::Format(
                "Not a valid Leica LOF file (error at header section)".into(),
            ));
        }
        c.read_i32("LOF chunk length")?; // length of the following binary chunk
        if c.read_u8_opt() != Some(LOF_MEMORY_BYTE) {
            return Err(BioFormatsError::Format(
                "Not a valid Leica LOF file (error at header section)".into(),
            ));
        }
        let nc = c.read_i32("LOF type-name length")?;
        let type_name = c.read_utf16(nc, "LOF type name")?;
        if type_name != LOF_TYPE_NAME {
            // Most likely a LIF file, not a single-image LOF.
            return Err(BioFormatsError::Format(format!(
                "Not a valid Leica LOF file (typename={type_name})"
            )));
        }
        // 1.2 major version, 1.3 minor version
        if c.read_u8_opt() != Some(LOF_MEMORY_BYTE) {
            return Err(BioFormatsError::Format(
                "Not a valid Leica LOF file (error at header section)".into(),
            ));
        }
        c.read_i32("LOF major version")?;
        if c.read_u8_opt() != Some(LOF_MEMORY_BYTE) {
            return Err(BioFormatsError::Format(
                "Not a valid Leica LOF file (error at header section)".into(),
            ));
        }
        c.read_i32("LOF minor version")?;
        // 1.4 memory size
        if c.read_u8_opt() != Some(LOF_MEMORY_BYTE) {
            return Err(BioFormatsError::Format(
                "Not a valid Leica LOF file (error at header section)".into(),
            ));
        }
        let memory_size = c.read_i64("LOF memory size")?;
        if memory_size < 0 {
            return Err(BioFormatsError::Format(
                "Not a valid Leica LOF file (negative memory size)".into(),
            ));
        }
        let data_offset = c.pos() as u64;
        let pixel_memory_present = memory_size > 0;

        // ---- Part 2: memory block (raw pixel data) ----
        c.skip(memory_size as usize);
        if c.pos() >= c.len() {
            return Err(BioFormatsError::Format(
                "Not a valid Leica LOF file (xml section not found)".into(),
            ));
        }

        // ---- Part 3: XML ----
        if c.read_i32("LOF xml magic")? as u32 != LOF_MAGIC_BYTE {
            return Err(BioFormatsError::Format(
                "Not a valid Leica LOF file (error at xml section)".into(),
            ));
        }
        c.read_i32("LOF xml chunk length")?;
        if c.read_u8_opt() != Some(LOF_MEMORY_BYTE) {
            return Err(BioFormatsError::Format(
                "Not a valid Leica LOF file (error at xml section)".into(),
            ));
        }
        let xml_length = c.read_i32("LOF xml length")?;
        let lof_xml = c.read_utf16(xml_length, "LOF xml content")?;

        // Translate the Leica <ImageDescription> into core metadata.
        let info = lof_translate_metadata(&lof_xml)?;

        self.path = Some(path.to_path_buf());
        self.meta = Some(info.meta);
        self.ome = Some(info.ome);
        self.data_offset = data_offset;
        self.end_pointer = file_len;
        self.tile_count = info.tile_count.max(1);
        self.tile_bytes_inc = info.tile_bytes_inc;
        self.pixel_memory_present = pixel_memory_present;
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.ome = None;
        self.data_offset = 0;
        self.end_pointer = 0;
        self.tile_count = 1;
        self.tile_bytes_inc = 0;
        self.pixel_memory_present = false;
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        if self.meta.is_some() {
            self.tile_count.max(1) as usize
        } else {
            0
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s >= self.tile_count.max(1) as usize {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let bytes = meta.pixel_type.bytes_per_sample();
        let rgb_channel_count = if meta.is_rgb {
            meta.size_c.max(1) as usize
        } else {
            1
        };
        let bpp = bytes * rgb_channel_count;
        let plane_size = (meta.size_x as u64) * (meta.size_y as u64) * bpp as u64;
        let plane_bytes = plane_size as usize;

        // Java only records an offset when memorySize > 0. Without one,
        // openBytes treats the file as truncated and returns fill-color planes.
        if !self.pixel_memory_present {
            return Ok(vec![0u8; plane_bytes]);
        }

        // Java row-padding (bytesToSkip) calculation.
        let mut bytes_to_skip: i64 = self.end_pointer as i64
            - self.data_offset as i64
            - plane_size as i64 * meta.image_count as i64;
        if plane_size == 0 || bytes_to_skip % plane_size as i64 != 0 {
            bytes_to_skip = 0;
        }
        if meta.size_y > 0 {
            bytes_to_skip /= meta.size_y as i64;
        } else {
            bytes_to_skip = 0;
        }
        if meta.size_x % 4 == 0 {
            bytes_to_skip = 0;
        }
        let bytes_to_skip = bytes_to_skip.max(0) as u64;

        let start = self.seek_start_of_plane(plane_index, plane_size)
            + bytes_to_skip * meta.size_y as u64 * plane_index as u64;

        // Truncated file: imitate LAS AF and return a blank (fill-0) plane.
        if start.saturating_add(plane_size) > self.end_pointer {
            return Ok(vec![0u8; plane_bytes]);
        }

        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(start))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        if bytes_to_skip == 0 {
            f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        } else {
            let row_bytes = meta.size_x as usize * bpp;
            for row in 0..meta.size_y as usize {
                f.read_exact(&mut buf[row * row_bytes..(row + 1) * row_bytes])
                    .map_err(BioFormatsError::Io)?;
                f.seek(SeekFrom::Current(bytes_to_skip as i64))
                    .map_err(BioFormatsError::Io)?;
            }
        }
        if meta.is_rgb && rgb_channel_count == 3 && lof_meta_inverse_rgb(&meta) {
            lof_bgr_to_rgb(&mut buf, bytes);
        }
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let spp = if meta.is_rgb {
            meta.size_c.max(1) as usize
        } else {
            1
        };
        crop_full_plane("Leica LOF", &full, meta, spp, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        if let (Some(img), Some(src)) = (ome.images.get_mut(0), self.ome.as_ref()) {
            img.name = src.name.clone();
            img.physical_size_x = src.physical_size_x;
            img.physical_size_y = src.physical_size_y;
            img.physical_size_z = src.physical_size_z;
            img.instrument_ref = src.instrument_ref;
            if !src.channels.is_empty() {
                img.channels = src.channels.clone();
            }
        }
        ome.instruments = lof_ome_instruments_from_metadata(meta);
        ome.rois = lof_ome_rois_from_metadata(meta);
        Some(ome)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

/// Core metadata derived from a LOF `<ImageDescription>`.
struct LofImageInfo {
    meta: ImageMetadata,
    ome: OmeImage,
    tile_count: u32,
    tile_bytes_inc: u64,
}

/// Minimal Leica `<ImageDescription>` DOM node (tag name + attributes).
struct LofNode {
    name: String,
    attrs: HashMap<String, String>,
}

/// Translate a LOF XML description into core metadata, mirroring the Leica
/// `translateImageNodes` dimension/channel logic shared with the LIF reader.
fn lof_translate_metadata(xml: &str) -> Result<LofImageInfo> {
    use quick_xml::events::Event;

    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(false);

    let mut nodes: Vec<LofNode> = Vec::new();
    let mut has_image = false;
    loop {
        match reader.read_event() {
            Ok(Event::Start(e)) | Ok(Event::Empty(e)) => {
                let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                if name == "Image" {
                    has_image = true;
                }
                let mut attrs = HashMap::new();
                for a in e.attributes().flatten() {
                    let key = String::from_utf8_lossy(a.key.as_ref()).to_string();
                    let val = a
                        .normalized_value(quick_xml::XmlVersion::Implicit1_0)
                        .map(|v| v.to_string())
                        .unwrap_or_default();
                    attrs.insert(key, val);
                }
                nodes.push(LofNode { name, attrs });
            }
            Ok(Event::Eof) => break,
            Ok(_) => {}
            Err(e) => return Err(BioFormatsError::Format(format!("LOF XML parse error: {e}"))),
        }
    }

    if !has_image {
        return Err(BioFormatsError::UnsupportedFormat(
            "Leica LOF XML does not contain image data, it cannot be opened directly".into(),
        ));
    }

    // Channels.
    let channel_nodes: Vec<&LofNode> = nodes
        .iter()
        .filter(|n| n.name == "ChannelDescription")
        .collect();
    let mut size_c = channel_nodes.len().max(1) as u32;

    let attr_u64 = |n: &LofNode, k: &str| -> u64 {
        n.attrs
            .get(k)
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(0)
    };
    let attr_i32 = |n: &LofNode, k: &str| -> i32 {
        n.attrs
            .get(k)
            .and_then(|v| v.trim().parse::<i32>().ok())
            .unwrap_or(0)
    };
    let attr_u32 = |n: &LofNode, k: &str| -> u32 {
        n.attrs
            .get(k)
            .and_then(|v| v.trim().parse::<u32>().ok())
            .unwrap_or(0)
    };

    // Dimensions.
    let dim_nodes: Vec<&LofNode> = nodes
        .iter()
        .filter(|n| n.name == "DimensionDescription")
        .collect();
    if dim_nodes.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "Leica LOF XML has no <DimensionDescription> elements".into(),
        ));
    }

    let mut tile_count: u32 = 1;
    let mut tile_bytes_inc: u64 = 0;
    let mut extras: u64 = 1;
    let mut size_z: u32 = 0;
    let mut size_t: u32 = 0;
    let mut size_x: u32 = 0;
    let mut size_y: u32 = 0;
    let mut is_rgb = false;
    let mut pixel_type = PixelType::Uint8;

    let mut physical_size_x: Option<f64> = None;
    let mut physical_size_y: Option<f64> = None;
    let mut physical_size_z: Option<f64> = None;

    for d in &dim_nodes {
        let id = attr_i32(d, "DimID");
        let len = attr_u32(d, "NumberOfElements");
        let mut n_bytes = attr_u64(d, "BytesInc");
        let phys = lof_physical_size_um(d, len);

        match id {
            1 => {
                size_x = len;
                physical_size_x = phys;
                is_rgb = n_bytes > 0 && n_bytes % 3 == 0;
                if is_rgb {
                    n_bytes /= 3;
                }
                pixel_type = lof_pixel_type_from_bytes(n_bytes);
            }
            2 => {
                if size_y != 0 {
                    if size_z <= 1 {
                        size_z = len;
                        physical_size_z = phys.map(f64::abs);
                    } else if size_t <= 1 {
                        size_t = len;
                    }
                } else {
                    size_y = len;
                    physical_size_y = phys;
                }
            }
            3 => {
                if size_y == 0 {
                    size_y = len;
                    size_z = 1;
                    physical_size_y = phys;
                } else {
                    size_z = len;
                    physical_size_z = phys.map(f64::abs);
                }
            }
            4 => {
                if size_y == 0 {
                    size_y = len;
                    size_t = 1;
                    physical_size_y = phys;
                } else {
                    size_t = len;
                }
            }
            10 => {
                tile_count = tile_count.saturating_mul(len.max(1));
                tile_bytes_inc = n_bytes;
            }
            _ => {
                extras = extras.saturating_mul(len.max(1) as u64);
            }
        }
    }

    if extras > 1 {
        if size_z <= 1 {
            size_z = extras as u32;
        } else if size_t == 0 {
            size_t = extras as u32;
        } else {
            size_t = size_t.saturating_mul(extras as u32);
        }
    }

    if size_c == 0 {
        size_c = 1;
    }
    let size_z = size_z.max(1);
    let size_t = size_t.max(1);
    let size_x = size_x.max(1);
    let size_y = size_y.max(1);

    let rgb_channel_count = if is_rgb { size_c } else { 1 };
    let image_count = size_z * size_t * (size_c / rgb_channel_count.max(1)).max(1);

    let mut series_metadata = HashMap::new();
    for (channel_index, channel_node) in channel_nodes.iter().enumerate() {
        lof_insert_channel_metadata(&mut series_metadata, channel_index, channel_node);
    }
    lof_insert_rgb_channel_order_metadata(
        &mut series_metadata,
        &channel_nodes,
        is_rgb,
        pixel_type.bytes_per_sample() as u64,
    );
    lof_insert_channel_lut_metadata(&mut series_metadata, &channel_nodes);
    let (instruments, _rois) = lof_insert_structured_metadata(&mut series_metadata, &nodes);

    let effective_c = (size_c / rgb_channel_count.max(1)).max(1) as usize;
    let channels = lof_ome_channels(&channel_nodes, effective_c, rgb_channel_count);

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: is_rgb,
        is_indexed: !is_rgb,
        is_little_endian: true,
        resolution_count: 1,
        thumbnail: false,
        series_metadata,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    let ome = OmeImage {
        physical_size_x: physical_size_x.filter(|v| *v > 0.0),
        physical_size_y: physical_size_y.filter(|v| *v > 0.0),
        physical_size_z: physical_size_z.filter(|v| *v > 0.0),
        channels,
        instrument_ref: (!instruments.is_empty()).then_some(0),
        ..OmeImage::default()
    };

    Ok(LofImageInfo {
        meta,
        ome,
        tile_count,
        tile_bytes_inc,
    })
}

fn lof_insert_structured_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    nodes: &[LofNode],
) -> (Vec<OmeInstrument>, Vec<OmeROI>) {
    let mut instruments = Vec::new();
    let mut rois = Vec::new();
    let mut detector_count = 0usize;
    let mut stage_count = 0usize;
    let mut acquisition_count = 0usize;
    let mut lut_count = 0usize;

    for node in nodes {
        let lower = node.name.to_ascii_lowercase();
        if lower.contains("instrument") || lower.contains("microscope") {
            let idx = instruments.len();
            let prefix = format!("lof.instrument.{idx}");
            lof_insert_node_scalar_attrs(metadata, &prefix, node);
            let instrument = OmeInstrument {
                id: Some(create_lsid("Instrument", &[idx])),
                microscope_model: lof_first_attr(node, &["Model", "Name", "SystemName", "Type"]),
                microscope_manufacturer: lof_first_attr(node, &["Manufacturer", "Vendor"]),
                ..OmeInstrument::default()
            };
            if instrument.microscope_model.is_some() || instrument.microscope_manufacturer.is_some()
            {
                instruments.push(instrument);
            }
        } else if lower.contains("detector") || lower.contains("camera") || lower.contains("pmt") {
            let idx = detector_count;
            detector_count += 1;
            let prefix = format!("lof.detector.{idx}");
            lof_insert_node_scalar_attrs(metadata, &prefix, node);
            let detector = OmeDetector {
                id: Some(create_lsid("Detector", &[0, idx])),
                model: lof_first_attr(node, &["Model", "Name", "DetectorName"]),
                manufacturer: lof_first_attr(node, &["Manufacturer", "Vendor"]),
                detector_type: lof_first_attr(node, &["Type", "DetectorType"]),
                gain: lof_first_f64(node, &["Gain", "DetectorGain"]),
                offset: lof_first_f64(node, &["Offset", "DetectorOffset"]),
            };
            if detector.model.is_some()
                || detector.manufacturer.is_some()
                || detector.detector_type.is_some()
                || detector.gain.is_some()
                || detector.offset.is_some()
            {
                if instruments.is_empty() {
                    instruments.push(OmeInstrument {
                        id: Some(create_lsid("Instrument", &[0])),
                        ..OmeInstrument::default()
                    });
                }
                instruments[0].detectors.push(detector);
            }
        } else if lower == "roi" || lower.ends_with("roi") || lower.contains("regionofinterest") {
            let idx = rois.len();
            let prefix = format!("lof.roi.{idx}");
            lof_insert_node_scalar_attrs(metadata, &prefix, node);
            if let Some(roi) = lof_ome_roi(node, idx) {
                rois.push(roi);
            }
        } else if lower.contains("stage") || lower.contains("position") {
            lof_insert_node_scalar_attrs(metadata, &format!("lof.stage.{stage_count}"), node);
            stage_count += 1;
        } else if lower.contains("acquisition") {
            lof_insert_node_scalar_attrs(
                metadata,
                &format!("lof.acquisition.{acquisition_count}"),
                node,
            );
            acquisition_count += 1;
        } else if lower.contains("lut") {
            lof_insert_node_scalar_attrs(metadata, &format!("lof.lut.{lut_count}"), node);
            lut_count += 1;
        }
    }

    (instruments, rois)
}

fn lof_insert_node_scalar_attrs(
    metadata: &mut HashMap<String, MetadataValue>,
    prefix: &str,
    node: &LofNode,
) {
    for (key, value) in &node.attrs {
        let trimmed = value.trim();
        if trimmed.is_empty() {
            continue;
        }
        let key = format!("{prefix}.{}", lof_key_name(key));
        if let Ok(v) = trimmed.parse::<i64>() {
            metadata.insert(key, MetadataValue::Int(v));
        } else if let Ok(v) = trimmed.parse::<f64>() {
            if v.is_finite() {
                metadata.insert(key, MetadataValue::Float(v));
            }
        } else {
            metadata.insert(key, MetadataValue::String(trimmed.to_string()));
        }
    }
}

fn lof_first_attr(node: &LofNode, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| lof_clean_attr(node, key))
}

fn lof_first_f64(node: &LofNode, keys: &[&str]) -> Option<f64> {
    keys.iter().find_map(|key| lof_attr_f64(node, key))
}

fn lof_ome_roi(node: &LofNode, idx: usize) -> Option<OmeROI> {
    let x = lof_first_f64(node, &["X", "Left", "PosX", "StageX"])?;
    let y = lof_first_f64(node, &["Y", "Top", "PosY", "StageY"])?;
    let shape = match (
        lof_first_f64(node, &["Width", "SizeX"]),
        lof_first_f64(node, &["Height", "SizeY"]),
    ) {
        (Some(width), Some(height)) if width >= 0.0 && height >= 0.0 => OmeShape::Rectangle {
            x,
            y,
            width,
            height,
            the_z: None,
            the_t: None,
            the_c: None,
        },
        _ => OmeShape::Point {
            x,
            y,
            the_z: None,
            the_t: None,
            the_c: None,
        },
    };
    Some(OmeROI {
        id: Some(create_lsid("ROI", &[idx])),
        name: lof_first_attr(node, &["Name", "Label"]),
        shapes: vec![shape],
    })
}

fn lof_insert_channel_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    channel_index: usize,
    channel_node: &LofNode,
) {
    let prefix = format!("lof.channel.{channel_index}");
    for key in ["Name", "DyeName", "Dye", "LUTName"] {
        if let Some(value) = lof_clean_attr(channel_node, key) {
            metadata.insert(
                format!("{prefix}.{}", lof_key_name(key)),
                MetadataValue::String(value),
            );
        }
    }
    for key in [
        "ExcitationWavelength",
        "EmissionWavelength",
        "Pinhole",
        "PinholeAiry",
        "PinholeSize",
        "BytesInc",
    ] {
        if let Some(value) = lof_attr_f64(channel_node, key) {
            metadata.insert(
                format!("{prefix}.{}", lof_key_name(key)),
                MetadataValue::Float(value),
            );
        }
    }
    if let Some(bits) = channel_node
        .attrs
        .get("Resolution")
        .and_then(|v| v.trim().parse::<i64>().ok())
    {
        metadata.insert(format!("{prefix}.resolution"), MetadataValue::Int(bits));
    }
}

fn lof_insert_rgb_channel_order_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    channel_nodes: &[&LofNode],
    is_rgb: bool,
    bytes_per_sample: u64,
) {
    if !is_rgb || bytes_per_sample == 0 || channel_nodes.len() < 3 {
        return;
    }

    let mut components = Vec::with_capacity(channel_nodes.len());
    for (index, node) in channel_nodes.iter().enumerate() {
        let Some(bytes_inc) = node
            .attrs
            .get("BytesInc")
            .and_then(|value| value.trim().parse::<u64>().ok())
        else {
            return;
        };
        let Some(label) = lof_channel_component_label(node, index) else {
            return;
        };
        components.push((bytes_inc, label));
    }

    components.sort_by_key(|(bytes_inc, _)| *bytes_inc);
    let mut order = String::with_capacity(components.len());
    let mut offsets = String::new();
    for (idx, (bytes_inc, label)) in components.iter().enumerate() {
        if idx > 0 {
            offsets.push(',');
        }
        if bytes_inc % bytes_per_sample != 0 {
            return;
        }
        offsets.push_str(&bytes_inc.to_string());
        order.push(*label);
    }

    if order.len() >= 3 {
        metadata.insert("lof.rgb.channel_order".into(), MetadataValue::String(order));
        metadata.insert(
            "lof.rgb.channel_order_source".into(),
            MetadataValue::String("ChannelDescription BytesInc".into()),
        );
        metadata.insert(
            "lof.rgb.channel_order_offsets".into(),
            MetadataValue::String(offsets),
        );
    }
}

fn lof_channel_component_label(node: &LofNode, fallback_index: usize) -> Option<char> {
    for key in ["Color", "Colour", "LUTColor", "LutColor", "ColorRGB", "RGB"] {
        if let Some(value) = lof_clean_attr(node, key) {
            if let Some(label) = lof_component_label_from_text(&value) {
                return Some(label);
            }
        }
    }
    lof_channel_name(node)
        .as_deref()
        .and_then(lof_component_label_from_text)
        .or_else(|| {
            ["R", "G", "B", "A"]
                .get(fallback_index)
                .and_then(|s| s.chars().next())
        })
}

fn lof_component_label_from_text(value: &str) -> Option<char> {
    let lower = value.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    if lower.contains("red") || lower == "r" {
        Some('R')
    } else if lower.contains("green") || lower == "g" {
        Some('G')
    } else if lower.contains("blue") || lower == "b" {
        Some('B')
    } else if lower.contains("alpha") || lower == "a" {
        Some('A')
    } else {
        None
    }
}

fn lof_ome_channels(
    channel_nodes: &[&LofNode],
    effective_c: usize,
    samples_per_pixel: u32,
) -> Vec<OmeChannel> {
    (0..effective_c)
        .map(|channel_index| {
            let node = channel_nodes.get(channel_index).copied();
            // Mirror the Leica `translateLuts` positional mapping: channel `i`
            // takes its colour from the `i`-th channel description's `LUTName`.
            // Only RGB-interleaved planes lack per-channel OME colours (the
            // component LUTs are folded into a single packed channel), matching
            // the Java behaviour where `translateLut` runs for every described
            // channel of the (non-RGB) image.
            let color = (samples_per_pixel <= 1)
                .then(|| node.map(|n| lof_translate_lut(lof_lut_name(n))))
                .flatten();
            OmeChannel {
                name: node.and_then(lof_channel_name),
                samples_per_pixel,
                color,
                excitation_wavelength: node.and_then(|n| lof_attr_f64(n, "ExcitationWavelength")),
                emission_wavelength: node.and_then(|n| lof_attr_f64(n, "EmissionWavelength")),
                ..Default::default()
            }
        })
        .collect()
}

/// Java `LMSMetadataExtractor.translateLut`: maps a Leica `LUTName` (a named
/// colour or a `Gradient(b,g,r)` triple) to a packed RGBA colour. Whitespace is
/// stripped first; unknown names fall back to opaque white. The packed value
/// uses the OME convention `(R<<24)|(G<<16)|(B<<8)|A` (alpha 255).
fn lof_translate_lut(lut_name: &str) -> i32 {
    let stripped: String = lut_name.chars().filter(|c| !c.is_whitespace()).collect();
    // Some LUTs are stored as gradients: `Gradient(b,g,r)`. Java reads the three
    // components in reverse (index 2 = red, 1 = green, 0 = blue).
    if let Some(rgb) = lof_parse_gradient_lut(&stripped) {
        let (b, g, r) = rgb;
        return lof_pack_rgba(r, g, b);
    }
    match stripped.to_ascii_lowercase().as_str() {
        "red" => lof_pack_rgba(255, 0, 0),
        "green" => lof_pack_rgba(0, 255, 0),
        "blue" => lof_pack_rgba(0, 0, 255),
        "cyan" => lof_pack_rgba(0, 255, 255),
        "magenta" => lof_pack_rgba(255, 0, 255),
        "yellow" => lof_pack_rgba(255, 255, 0),
        _ => lof_pack_rgba(255, 255, 255),
    }
}

/// Parse a Leica `Gradient(<u8>,<u8>,<u8>)` LUT (case-insensitive). Returns the
/// three numeric components in source order, or `None` for any other text.
fn lof_parse_gradient_lut(stripped: &str) -> Option<(u8, u8, u8)> {
    let lower = stripped.to_ascii_lowercase();
    let inner = lower
        .strip_prefix("gradient(")
        .and_then(|rest| rest.strip_suffix(')'))?;
    let parts: Vec<&str> = inner.split(',').collect();
    if parts.len() != 3 {
        return None;
    }
    let a = parts[0].parse::<i32>().ok()?;
    let b = parts[1].parse::<i32>().ok()?;
    let c = parts[2].parse::<i32>().ok()?;
    if !(0..=255).contains(&a) || !(0..=255).contains(&b) || !(0..=255).contains(&c) {
        return None;
    }
    Some((a as u8, b as u8, c as u8))
}

/// Pack an RGB triple as an OME-style signed RGBA integer with opaque alpha,
/// matching `ome.xml.model.primitives.Color(r, g, b, 255)`.
fn lof_pack_rgba(r: u8, g: u8, b: u8) -> i32 {
    (((r as u32) << 24) | ((g as u32) << 16) | ((b as u32) << 8) | 0xff) as i32
}

/// Java `LMSMetadataExtractor.getChannelPriority`: maps a Leica `LUTName` to the
/// channel-priority index that drives the synthetic 8/16-bit lookup table in
/// `LOFReader.get8BitLookupTable`. Matching is case-sensitive lowercase (as in
/// the Java `switch`), so capitalised names such as `"Red"` fall through to the
/// default `8` (gray/identity).
fn lof_channel_priority(lut_name: &str) -> i32 {
    match lut_name {
        "red" => 0,
        "green" => 1,
        "blue" => 2,
        "cyan" => 3,
        "magenta" => 4,
        "yellow" => 5,
        "black" => 6,
        "gray" => 7,
        _ => 8,
    }
}

/// Java `LMSMetadataExtractor.translateChannelDescriptions` inverse-RGB test:
/// BGR ordering is assumed unless the first three channels are explicitly
/// described as `Red`, `Green`, `Blue` (in that order).
fn lof_inverse_rgb(channel_nodes: &[&LofNode]) -> bool {
    if channel_nodes.len() < 3 {
        return true;
    }
    !(lof_lut_name(channel_nodes[0]) == "Red"
        && lof_lut_name(channel_nodes[1]) == "Green"
        && lof_lut_name(channel_nodes[2]) == "Blue")
}

/// The raw `LUTName` attribute of a channel description (empty string if absent),
/// matching the Java `getAttribute("LUTName")` default.
fn lof_lut_name(node: &LofNode) -> &str {
    node.attrs.get("LUTName").map(String::as_str).unwrap_or("")
}

/// Capture the Leica per-channel LUT object-graph scalars that Java derives in
/// `translateLuts` / `translateChannelDescriptions` but the LOF reader keeps in
/// its transient `metaTemp` buffer (channel colour, channel priority, and the
/// image-level inverse-RGB flag).
fn lof_insert_channel_lut_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    channel_nodes: &[&LofNode],
) {
    for (channel_index, node) in channel_nodes.iter().enumerate() {
        let lut_name = lof_lut_name(node);
        let prefix = format!("lof.channel.{channel_index}");
        metadata.insert(
            format!("{prefix}.lut_color"),
            MetadataValue::Int(lof_translate_lut(lut_name) as i64),
        );
        metadata.insert(
            format!("{prefix}.channel_priority"),
            MetadataValue::Int(lof_channel_priority(lut_name) as i64),
        );
    }
    if !channel_nodes.is_empty() {
        metadata.insert(
            "lof.inverse_rgb".into(),
            MetadataValue::String(lof_inverse_rgb(channel_nodes).to_string()),
        );
    }
}

fn lof_meta_inverse_rgb(meta: &ImageMetadata) -> bool {
    matches!(
        meta.series_metadata.get("lof.inverse_rgb"),
        Some(MetadataValue::String(value)) if value == "true"
    )
}

fn lof_bgr_to_rgb(buf: &mut [u8], bytes_per_sample: usize) {
    if bytes_per_sample == 0 {
        return;
    }
    let pixel_stride = bytes_per_sample * 3;
    for pixel in buf.chunks_exact_mut(pixel_stride) {
        for i in 0..bytes_per_sample {
            pixel.swap(i, 2 * bytes_per_sample + i);
        }
    }
}

fn lof_channel_name(node: &LofNode) -> Option<String> {
    ["Name", "DyeName", "Dye"]
        .into_iter()
        .find_map(|key| lof_clean_attr(node, key))
}

fn lof_clean_attr(node: &LofNode, key: &str) -> Option<String> {
    let trimmed = node.attrs.get(key)?.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn lof_attr_f64(node: &LofNode, key: &str) -> Option<f64> {
    let parsed = node.attrs.get(key)?.trim().parse::<f64>().ok()?;
    parsed.is_finite().then_some(parsed)
}

fn lof_ome_instruments_from_metadata(meta: &ImageMetadata) -> Vec<OmeInstrument> {
    let mut instrument = OmeInstrument::default();
    let mut has_instrument = false;

    if let Some(value) = metadata_string(meta, "lof.instrument.0.model")
        .or_else(|| metadata_string(meta, "lof.instrument.0.name"))
        .or_else(|| metadata_string(meta, "lof.instrument.0.system_name"))
    {
        instrument.microscope_model = Some(value);
        has_instrument = true;
    }
    if let Some(value) = metadata_string(meta, "lof.instrument.0.manufacturer")
        .or_else(|| metadata_string(meta, "lof.instrument.0.vendor"))
    {
        instrument.microscope_manufacturer = Some(value);
        has_instrument = true;
    }

    let mut detector_index = 0usize;
    loop {
        let base = format!("lof.detector.{detector_index}");
        let detector = OmeDetector {
            id: Some(create_lsid("Detector", &[0, detector_index])),
            model: metadata_string(meta, &format!("{base}.model"))
                .or_else(|| metadata_string(meta, &format!("{base}.name")))
                .or_else(|| metadata_string(meta, &format!("{base}.detector_name"))),
            manufacturer: metadata_string(meta, &format!("{base}.manufacturer"))
                .or_else(|| metadata_string(meta, &format!("{base}.vendor"))),
            detector_type: metadata_string(meta, &format!("{base}.type"))
                .or_else(|| metadata_string(meta, &format!("{base}.detector_type"))),
            gain: metadata_float(meta, &format!("{base}.gain"))
                .or_else(|| metadata_float(meta, &format!("{base}.detector_gain"))),
            offset: metadata_float(meta, &format!("{base}.offset"))
                .or_else(|| metadata_float(meta, &format!("{base}.detector_offset"))),
        };
        if detector.model.is_none()
            && detector.manufacturer.is_none()
            && detector.detector_type.is_none()
            && detector.gain.is_none()
            && detector.offset.is_none()
        {
            break;
        }
        has_instrument = true;
        instrument.detectors.push(detector);
        detector_index += 1;
    }

    if has_instrument {
        instrument.id = Some(create_lsid("Instrument", &[0]));
        vec![instrument]
    } else {
        Vec::new()
    }
}

fn lof_ome_rois_from_metadata(meta: &ImageMetadata) -> Vec<OmeROI> {
    let mut rois = Vec::new();
    let mut idx = 0usize;
    loop {
        let base = format!("lof.roi.{idx}");
        let Some(x) = metadata_float(meta, &format!("{base}.x"))
            .or_else(|| metadata_float(meta, &format!("{base}.left")))
            .or_else(|| metadata_float(meta, &format!("{base}.pos_x")))
        else {
            break;
        };
        let Some(y) = metadata_float(meta, &format!("{base}.y"))
            .or_else(|| metadata_float(meta, &format!("{base}.top")))
            .or_else(|| metadata_float(meta, &format!("{base}.pos_y")))
        else {
            break;
        };
        let shape = match (
            metadata_float(meta, &format!("{base}.width"))
                .or_else(|| metadata_float(meta, &format!("{base}.size_x"))),
            metadata_float(meta, &format!("{base}.height"))
                .or_else(|| metadata_float(meta, &format!("{base}.size_y"))),
        ) {
            (Some(width), Some(height)) => OmeShape::Rectangle {
                x,
                y,
                width,
                height,
                the_z: None,
                the_t: None,
                the_c: None,
            },
            _ => OmeShape::Point {
                x,
                y,
                the_z: None,
                the_t: None,
                the_c: None,
            },
        };
        rois.push(OmeROI {
            id: Some(create_lsid("ROI", &[idx])),
            name: metadata_string(meta, &format!("{base}.name"))
                .or_else(|| metadata_string(meta, &format!("{base}.label"))),
            shapes: vec![shape],
        });
        idx += 1;
    }
    rois
}

fn metadata_string(meta: &ImageMetadata, key: &str) -> Option<String> {
    match meta.series_metadata.get(key) {
        Some(MetadataValue::String(value)) => Some(value.clone()),
        _ => None,
    }
}

fn metadata_float(meta: &ImageMetadata, key: &str) -> Option<f64> {
    match meta.series_metadata.get(key) {
        Some(MetadataValue::Float(value)) => Some(*value),
        Some(MetadataValue::Int(value)) => Some(*value as f64),
        _ => None,
    }
}

fn lof_key_name(key: &str) -> String {
    match key {
        "DyeName" => return "dye_name".to_string(),
        "LUTName" => return "lut_name".to_string(),
        "ExcitationWavelength" => return "excitation_wavelength".to_string(),
        "EmissionWavelength" => return "emission_wavelength".to_string(),
        "PinholeAiry" => return "pinhole_airy".to_string(),
        "PinholeSize" => return "pinhole_size".to_string(),
        "BytesInc" => return "bytes_inc".to_string(),
        _ => {}
    }
    let mut out = String::new();
    for (i, ch) in key.chars().enumerate() {
        if ch.is_ascii_uppercase() && i > 0 {
            out.push('_');
        }
        out.push(ch.to_ascii_lowercase());
    }
    out
}

/// Leica calibration: `length / (numElements - 1)`, normalised to µm
/// (`Unit="m"` → ×1e6, `Unit="Ks"` → ÷1000). Returns `None` when there is no
/// usable calibration.
fn lof_physical_size_um(node: &LofNode, num_elements: u32) -> Option<f64> {
    if num_elements <= 1 {
        return None;
    }
    let raw = node.attrs.get("Length").map(|s| s.trim()).unwrap_or("");
    if raw.is_empty() {
        return None;
    }
    let length: f64 = raw.parse().ok()?;
    let mut value = length / (num_elements as f64 - 1.0);
    match node.attrs.get("Unit").map(String::as_str) {
        Some("Ks") => value /= 1000.0,
        Some("m") => value *= 1_000_000.0,
        _ => {}
    }
    if value.is_finite() {
        Some(value)
    } else {
        None
    }
}

/// Java `FormatTools.pixelTypeFromBytes(nBytes, signed=false, fp=...)` as used
/// by the Leica readers: unsigned integer types (8-byte → double).
fn lof_pixel_type_from_bytes(n_bytes: u64) -> PixelType {
    match n_bytes {
        0 | 1 => PixelType::Uint8,
        2 => PixelType::Uint16,
        4 => PixelType::Uint32,
        8 => PixelType::Float64,
        _ => PixelType::Uint8,
    }
}

// ---------------------------------------------------------------------------
// 10. Animated PNG (APNG)
// ---------------------------------------------------------------------------
// Faithful translation of the upstream Java `APNGReader`
// (components/formats-bsd/.../in/APNGReader.java). Each APNG frame becomes a
// timepoint (sizeT == numFrames, dimensionOrder XYCTZ). Frame 0 is the default
// image (the IDAT chunks); each subsequent frame `no` is the fdAT data for that
// `fcTL`, pasted onto a fresh copy of frame 0 at the frame's (x, y) offset,
// mirroring Java's `openBytes` compositing.

/// One parsed PNG/APNG chunk: type + offset/length of its data payload.
#[derive(Clone)]
struct PngBlock {
    offset: u64,
    length: u32,
    type_: [u8; 4],
}

pub struct ApngReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data: Vec<u8>,
    blocks: Vec<PngBlock>,
    /// One [x, y, w, h] per frame (the default image plus each fcTL).
    frame_coordinates: Vec<[u32; 4]>,
    lut: Option<[[u8; 256]; 3]>,
    color_type: u8,
    bit_depth: u8,
    compression: u8,
    interlace: u8,
}

impl ApngReader {
    pub fn new() -> Self {
        ApngReader {
            path: None,
            meta: None,
            data: Vec::new(),
            blocks: Vec::new(),
            frame_coordinates: Vec::new(),
            lut: None,
            color_type: 0,
            bit_depth: 0,
            compression: 0,
            interlace: 0,
        }
    }

    /// Scan a PNG byte stream for an `acTL` chunk (animation control), the
    /// marker that distinguishes an animated PNG from a still PNG. Mirrors
    /// `PngReader::contains_apng_animation_control` but operates on bytes.
    fn has_actl(data: &[u8]) -> bool {
        if !data.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
            return false;
        }
        let mut offset = 8usize;
        while offset + 8 <= data.len() {
            let length = u32::from_be_bytes([
                data[offset],
                data[offset + 1],
                data[offset + 2],
                data[offset + 3],
            ]) as usize;
            let chunk_type = &data[offset + 4..offset + 8];
            if chunk_type == b"acTL" {
                return true;
            }
            // The default image's IDAT always follows acTL when one is present,
            // so an animated PNG is detected before the first IDAT/IEND.
            if chunk_type == b"IEND" {
                return false;
            }
            match offset.checked_add(12).and_then(|v| v.checked_add(length)) {
                Some(next) => offset = next,
                None => return false,
            }
        }
        false
    }

    fn read_be_i32(&self, off: usize) -> Result<i32> {
        self.data
            .get(off..off + 4)
            .map(|b| i32::from_be_bytes([b[0], b[1], b[2], b[3]]))
            .ok_or_else(|| BioFormatsError::Format("APNG: truncated chunk".into()))
    }

    /// `initFile`: parse the PNG signature and every chunk, building `blocks`,
    /// `frame_coordinates`, the palette, and the core metadata.
    fn init_file(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;

        if !data.starts_with(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]) {
            return Err(BioFormatsError::Format("Invalid PNG signature.".into()));
        }
        self.data = data;

        let mut image_count: u32 = 0;
        let mut size_x: u32 = 0;
        let mut size_y: u32 = 0;
        let mut size_c: u32 = 1;
        let mut bits_per_pixel: u8 = 0;
        let mut color_type: u8 = 0;
        let mut compression: u8 = 0;
        let mut interlace: u8 = 0;
        let mut is_indexed = false;
        let mut lut: Option<[[u8; 256]; 3]> = None;

        let mut offset = 8usize;
        let total = self.data.len();
        while offset < total {
            let length = self.read_be_i32(offset)? as i64;
            if length < 0 {
                return Err(BioFormatsError::Format(
                    "APNG: negative chunk length".into(),
                ));
            }
            let length = length as u32;
            let type_bytes = self
                .data
                .get(offset + 4..offset + 8)
                .ok_or_else(|| BioFormatsError::Format("APNG: truncated chunk type".into()))?;
            let mut type_ = [0u8; 4];
            type_.copy_from_slice(type_bytes);
            let data_offset = (offset + 8) as u64;

            self.blocks.push(PngBlock {
                offset: data_offset,
                length,
                type_,
            });

            let d = data_offset as usize;
            match &type_ {
                b"acTL" => {
                    image_count = self.read_be_i32(d)? as u32;
                    // d + 4 is the loop count ("num_plays"); recorded as global
                    // metadata in Java, not needed for pixel access.
                }
                b"fcTL" => {
                    // Skip the 4-byte sequence number, then read w, h, x, y.
                    let w = self.read_be_i32(d + 4)? as u32;
                    let h = self.read_be_i32(d + 8)? as u32;
                    let x = self.read_be_i32(d + 12)? as u32;
                    let y = self.read_be_i32(d + 16)? as u32;
                    self.frame_coordinates.push([x, y, w, h]);
                }
                b"IDAT" => {}
                b"PLTE" => {
                    is_indexed = true;
                    let mut table = [[0u8; 256]; 3];
                    let entries = (length / 3) as usize;
                    for i in 0..entries.min(256) {
                        for (c, row) in table.iter_mut().enumerate() {
                            row[i] = self.data[d + i * 3 + c];
                        }
                    }
                    lut = Some(table);
                }
                b"IHDR" => {
                    size_x = self.read_be_i32(d)? as u32;
                    size_y = self.read_be_i32(d + 4)? as u32;
                    bits_per_pixel = self.data[d + 8];
                    color_type = self.data[d + 9];
                    compression = self.data[d + 10];
                    let filter = self.data[d + 11];
                    interlace = self.data[d + 12];

                    if filter != 0 {
                        return Err(BioFormatsError::Format(format!(
                            "Invalid filter mode: {filter}"
                        )));
                    }

                    size_c = match color_type {
                        0 | 3 => 1, // GRAYSCALE / INDEXED
                        4 => 2,     // GRAY_ALPHA
                        2 => 3,     // TRUE_COLOR
                        6 => 4,     // TRUE_ALPHA
                        other => {
                            return Err(BioFormatsError::Format(format!(
                                "APNG: unsupported color type {other}"
                            )));
                        }
                    };
                }
                b"IEND" => break,
                _ => {}
            }

            // Advance past the data payload and the 4-byte CRC.
            offset = offset
                .checked_add(12)
                .and_then(|v| v.checked_add(length as usize))
                .ok_or_else(|| BioFormatsError::Format("APNG: chunk offset overflow".into()))?;
        }

        if image_count == 0 {
            image_count = 1;
        }

        let pixel_type = if bits_per_pixel <= 8 {
            PixelType::Uint8
        } else {
            PixelType::Uint16
        };
        let is_rgb = size_c > 1;

        self.color_type = color_type;
        self.bit_depth = bits_per_pixel;
        self.compression = compression;
        self.interlace = interlace;
        let lookup_table = lut.as_ref().map(|table| LookupTable {
            red: table[0].iter().map(|&v| v as u16).collect(),
            green: table[1].iter().map(|&v| v as u16).collect(),
            blue: table[2].iter().map(|&v| v as u16).collect(),
        });

        self.lut = lut;

        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z: 1,
            size_c,
            size_t: image_count,
            pixel_type,
            bits_per_pixel,
            image_count,
            // APNGReader.java: dimensionOrder "XYCTZ".
            dimension_order: DimensionOrder::XYCTZ,
            is_rgb,
            // interleaved == isRGB()
            is_interleaved: is_rgb,
            is_indexed,
            // Core metadata defaults to big-endian (littleEndian = false).
            is_little_endian: false,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: HashMap::new(),
            lookup_table,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    /// Rebuild a standalone PNG byte stream from the global IHDR/PLTE plus a
    /// frame's compressed data (IDAT for frame 0, the fdAT chunks otherwise),
    /// then decode it. Mirrors Java's `PNGInputStream` + `decode`: the per-frame
    /// fdAT/IDAT chunks describe a sub-PNG of size (width, height).
    fn decode_frame(&self, frame: usize) -> Result<(Vec<u8>, u32, u32)> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if self.compression != 0 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Compression type {} not supported",
                self.compression
            )));
        }

        let (sub_w, sub_h) = if frame == 0 {
            (meta.size_x, meta.size_y)
        } else {
            let c = self.frame_coordinates[frame];
            (c[2], c[3])
        };

        // Collect the compressed payload for this frame.
        let payload = if frame == 0 {
            // All IDAT chunks (the default image), concatenated.
            let mut p = Vec::new();
            for b in &self.blocks {
                if &b.type_ == b"IDAT" {
                    let s = b.offset as usize;
                    p.extend_from_slice(&self.data[s..s + b.length as usize]);
                }
            }
            p
        } else {
            // The fdAT chunks belonging to the frame-th fcTL. Each fdAT starts
            // with a 4-byte sequence number which is stripped (Java skips 4
            // bytes and shortens blockLength by 4).
            let mut p = Vec::new();
            let mut fctl_count: i32 = -1;
            for b in &self.blocks {
                if &b.type_ == b"fcTL" {
                    fctl_count += 1;
                } else if &b.type_ == b"fdAT" && fctl_count as usize == frame {
                    let s = b.offset as usize + 4;
                    let len = b.length.saturating_sub(4) as usize;
                    p.extend_from_slice(&self.data[s..s + len]);
                }
            }
            p
        };

        let raw = if self.color_type == 3 || (self.color_type == 0 && self.bit_depth < 8) {
            decode_apng_packed_samples(&payload, sub_w, sub_h, self.bit_depth, self.interlace)?
        } else {
            let png = self.build_sub_png(sub_w, sub_h, &payload);
            decode_sub_png(&png, meta, sub_w, sub_h)?
        };
        Ok((raw, sub_w, sub_h))
    }

    /// Assemble a minimal valid PNG: signature, an IHDR matching this frame's
    /// dimensions (reusing the global bit depth / color type / interlace), the
    /// optional PLTE, the frame payload as a single IDAT, and IEND.
    fn build_sub_png(&self, width: u32, height: u32, idat: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(idat.len() + 64);
        out.extend_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);

        // IHDR (13 bytes of data).
        let mut ihdr = Vec::with_capacity(13);
        ihdr.extend_from_slice(&width.to_be_bytes());
        ihdr.extend_from_slice(&height.to_be_bytes());
        ihdr.push(self.bit_depth);
        ihdr.push(self.color_type);
        ihdr.push(0); // compression method
        ihdr.push(0); // filter method
        ihdr.push(self.interlace);
        write_png_chunk(&mut out, b"IHDR", &ihdr);

        // PLTE, if the source was indexed.
        if self.color_type == 3 {
            if let Some(lut) = &self.lut {
                let mut plte = Vec::with_capacity(768);
                for ((&r, &g), &b) in lut[0].iter().zip(lut[1].iter()).zip(lut[2].iter()) {
                    plte.push(r);
                    plte.push(g);
                    plte.push(b);
                }
                write_png_chunk(&mut out, b"PLTE", &plte);
            }
        }

        write_png_chunk(&mut out, b"IDAT", idat);
        write_png_chunk(&mut out, b"IEND", &[]);
        out
    }
}

/// Write one PNG chunk (length + type + data + CRC32 over type+data).
fn write_png_chunk(out: &mut Vec<u8>, type_: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    out.extend_from_slice(type_);
    out.extend_from_slice(data);
    let mut crc = flate2::Crc::new();
    crc.update(type_);
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_be_bytes());
}

/// Decode a complete PNG byte stream into raw, planar/interleaved pixel bytes
/// laid out to match `meta` (interleaved RGB samples, PNG/Java big-endian uint16).
fn decode_sub_png(png: &[u8], meta: &ImageMetadata, w: u32, h: u32) -> Result<Vec<u8>> {
    use image::GenericImageView;
    let img = image::load_from_memory_with_format(png, image::ImageFormat::Png)
        .map_err(|e| BioFormatsError::Format(format!("APNG frame decode failed: {e}")))?;
    let (iw, ih) = img.dimensions();
    if iw != w || ih != h {
        return Err(BioFormatsError::Format(format!(
            "APNG frame decoded to {iw}x{ih}, expected {w}x{h}"
        )));
    }

    let spp = meta.size_c as usize;
    let raw: Vec<u8> = match (meta.pixel_type, spp) {
        (PixelType::Uint8, 1) => img.to_luma8().into_raw(),
        (PixelType::Uint8, 2) => img.to_luma_alpha8().into_raw(),
        (PixelType::Uint8, 3) => img.to_rgb8().into_raw(),
        (PixelType::Uint8, 4) => img.to_rgba8().into_raw(),
        (PixelType::Uint16, 1) => img
            .to_luma16()
            .into_raw()
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect(),
        (PixelType::Uint16, 2) => img
            .to_luma_alpha16()
            .into_raw()
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect(),
        (PixelType::Uint16, 3) => img
            .to_rgb16()
            .into_raw()
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect(),
        (PixelType::Uint16, 4) => img
            .to_rgba16()
            .into_raw()
            .iter()
            .flat_map(|v| v.to_be_bytes())
            .collect(),
        (pt, c) => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "APNG: unsupported pixel layout {pt:?} spp={c}"
            )));
        }
    };
    Ok(raw)
}

fn decode_apng_packed_samples(
    compressed: &[u8],
    width: u32,
    height: u32,
    bit_depth: u8,
    interlace: u8,
) -> Result<Vec<u8>> {
    let mut inflated = Vec::new();
    flate2::read::ZlibDecoder::new(compressed)
        .read_to_end(&mut inflated)
        .map_err(BioFormatsError::Io)?;

    let mut pixels = vec![0u8; width as usize * height as usize];
    if interlace == 0 {
        decode_apng_packed_pass(
            &inflated,
            width as usize,
            height as usize,
            bit_depth,
            |col, row, value| {
                pixels[row * width as usize + col] = value;
            },
        )?;
        return Ok(pixels);
    }

    const ADAM7: [(usize, usize, usize, usize); 7] = [
        (0, 0, 8, 8),
        (4, 0, 8, 8),
        (0, 4, 4, 8),
        (2, 0, 4, 4),
        (0, 2, 2, 4),
        (1, 0, 2, 2),
        (0, 1, 1, 2),
    ];

    let mut offset = 0usize;
    for (x0, y0, x_step, y_step) in ADAM7 {
        if x0 >= width as usize || y0 >= height as usize {
            continue;
        }
        let pass_width = ((width as usize - x0) + x_step - 1) / x_step;
        let pass_height = ((height as usize - y0) + y_step - 1) / y_step;
        let consumed = decode_apng_packed_pass(
            inflated.get(offset..).unwrap_or_default(),
            pass_width,
            pass_height,
            bit_depth,
            |col, row, value| {
                let x = x0 + col * x_step;
                let y = y0 + row * y_step;
                pixels[y * width as usize + x] = value;
            },
        )?;
        offset += consumed;
    }
    Ok(pixels)
}

fn decode_apng_packed_pass<F>(
    inflated: &[u8],
    width: usize,
    height: usize,
    bit_depth: u8,
    mut set_pixel: F,
) -> Result<usize>
where
    F: FnMut(usize, usize, u8),
{
    let row_bits = width * bit_depth as usize;
    let row_bytes = row_bits.div_ceil(8);
    let expected = (row_bytes + 1)
        .checked_mul(height)
        .ok_or_else(|| BioFormatsError::Format("APNG packed payload overflows".into()))?;
    if inflated.len() < expected {
        return Err(BioFormatsError::Format(format!(
            "APNG packed payload ended after {} bytes, expected at least {}",
            inflated.len(),
            expected
        )));
    }

    let mut unfiltered = vec![0u8; row_bytes * height];
    let mut src = 0usize;
    for row in 0..height {
        let filter_type = inflated[src];
        src += 1;
        let row_start = row * row_bytes;
        let prev_start = row.checked_sub(1).map(|prev| prev * row_bytes);
        for col in 0..row_bytes {
            let raw = inflated[src + col];
            let left = if col > 0 {
                unfiltered[row_start + col - 1]
            } else {
                0
            };
            let up = prev_start.map(|base| unfiltered[base + col]).unwrap_or(0);
            let up_left = if col > 0 {
                prev_start
                    .map(|base| unfiltered[base + col - 1])
                    .unwrap_or(0)
            } else {
                0
            };
            unfiltered[row_start + col] = match filter_type {
                0 => raw,
                1 => raw.wrapping_add(left),
                2 => raw.wrapping_add(up),
                3 => raw.wrapping_add(((left as u16 + up as u16) / 2) as u8),
                4 => raw.wrapping_add(apng_paeth_predictor(left, up, up_left)),
                _ => {
                    return Err(BioFormatsError::Format(format!(
                        "APNG invalid filter type {filter_type}"
                    )));
                }
            };
        }
        src += row_bytes;
    }

    for row in 0..height {
        let row_data = &unfiltered[row * row_bytes..(row + 1) * row_bytes];
        for col in 0..width {
            let value = if bit_depth == 8 {
                row_data[col]
            } else {
                let bit = col * bit_depth as usize;
                let byte = row_data[bit / 8];
                let shift = 8 - bit_depth as usize - (bit % 8);
                (byte >> shift) & ((1u16 << bit_depth) - 1) as u8
            };
            set_pixel(col, row, value);
        }
    }

    Ok(expected)
}

fn apng_paeth_predictor(left: u8, up: u8, up_left: u8) -> u8 {
    let a = left as i32;
    let b = up as i32;
    let c = up_left as i32;
    let p = a + b - c;
    let pa = (p - a).abs();
    let pb = (p - b).abs();
    let pc = (p - c).abs();
    if pa <= pb && pa <= pc {
        left
    } else if pb <= pc {
        up
    } else {
        up_left
    }
}

impl Default for ApngReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ApngReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("apng"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Claim only animated PNGs: PNG signature AND an acTL chunk. Still PNGs
        // fall through to PngReader. (For very large files whose acTL lies
        // beyond the peeked header, the extension/`set_id` paths still apply.)
        Self::has_actl(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if !Self::has_actl(&data) {
            return Err(BioFormatsError::UnsupportedFormat(
                "not an animated PNG (no acTL chunk); use PngReader for still PNGs".into(),
            ));
        }
        self.init_file(path)
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data = Vec::new();
        self.blocks = Vec::new();
        self.frame_coordinates = Vec::new();
        self.lut = None;
        self.color_type = 0;
        self.bit_depth = 0;
        self.compression = 0;
        self.interlace = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() || s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }
    fn series(&self) -> usize {
        0
    }
    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if p >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(p));
        }
        let bpp = meta.pixel_type.bytes_per_sample();
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;
        let rgb_channels = if meta.is_rgb { meta.size_c as usize } else { 1 };
        let interleaved = meta.is_interleaved;

        // Frame 0 is the default image (full size, no compositing).
        let (frame0, _, _) = self.decode_frame(0)?;
        if p == 0 {
            return Ok(frame0);
        }

        // Paste frame `p` (a sub-image) onto a fresh copy of frame 0 at its
        // (x, y) offset, mirroring APNGReader.openBytes.
        let coords = self.frame_coordinates[p as usize];
        let (new_image, _, _) = self.decode_frame(p as usize)?;

        let mut last_image = frame0;
        let cx = coords[0] as usize;
        let cy = coords[1] as usize;
        let cw = coords[2] as usize;
        let ch = coords[3] as usize;

        if !interleaved {
            let len = cw * bpp;
            let plane = size_x * size_y * bpp;
            let new_plane = len * ch;
            for c in 0..rgb_channels {
                for row in 0..ch {
                    let src = c * new_plane + row * len;
                    let dst = c * plane + (cy + row) * size_x * bpp + cx * bpp;
                    last_image[dst..dst + len].copy_from_slice(&new_image[src..src + len]);
                }
            }
        } else {
            let len = cw * bpp * rgb_channels;
            for row in 0..ch {
                let src = row * len;
                let dst = (cy + row) * size_x * bpp * rgb_channels + cx * bpp * rgb_channels;
                last_image[dst..dst + len].copy_from_slice(&new_image[src..src + len]);
            }
        }

        Ok(last_image)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let channels = if meta.is_rgb { meta.size_c as usize } else { 1 };
        crop_full_plane("APNG", &full, meta, channels, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(p, tx, ty, tw, th)
    }
    fn resolution_count(&self) -> usize {
        1
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// Animated PNG (APNG) writer
// ---------------------------------------------------------------------------
// Faithful translation of upstream Java `APNGWriter`
// (components/formats-bsd/.../out/APNGWriter.java). Writes the PNG signature +
// IHDR + acTL, then one fcTL + IDAT (first plane) / fdAT (subsequent planes)
// per saved plane, and finally IEND with the frame count patched into acTL.
//
// Our `FormatWriter` trait delivers planes sequentially without a seekable
// output handle, so the file is assembled in memory and flushed on `close`,
// which reproduces the byte layout Java emits with its seek/footer logic.
use crate::common::writer::FormatWriter;

pub struct ApngWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Everything after the acTL data: fcTL/IDAT/fdAT chunks for each frame.
    body: Vec<u8>,
    num_frames: u32,
    next_sequence_number: u32,
    little_endian: bool,
    /// fps / num_plays delay value (Java's `fps` field; default 0).
    fps: u16,
}

impl ApngWriter {
    pub fn new() -> Self {
        ApngWriter {
            path: None,
            meta: None,
            body: Vec::new(),
            num_frames: 0,
            next_sequence_number: 0,
            little_endian: false,
            fps: 0,
        }
    }

    /// `writeFCTL`: frame control chunk (sequence#, w, h, x_off=0, y_off=0,
    /// delay 1/fps, dispose=1, blend=0).
    fn write_fctl(&mut self, width: u32, height: u32) {
        let mut b = Vec::with_capacity(26);
        b.extend_from_slice(&self.next_sequence_number.to_be_bytes());
        self.next_sequence_number += 1;
        b.extend_from_slice(&width.to_be_bytes());
        b.extend_from_slice(&height.to_be_bytes());
        b.extend_from_slice(&0u32.to_be_bytes()); // x_offset
        b.extend_from_slice(&0u32.to_be_bytes()); // y_offset
        b.extend_from_slice(&1u16.to_be_bytes()); // delay_num
        b.extend_from_slice(&self.fps.to_be_bytes()); // delay_den
        b.push(1); // dispose_op
        b.push(0); // blend_op
        write_png_chunk(&mut self.body, b"fcTL", &b);
    }

    /// `writePLTE`: palette chunk (only when an indexed color model is present).
    fn write_plte(&mut self) {
        let Some(meta) = self.meta.as_ref() else {
            return;
        };
        let Some(lut) = meta.lookup_table.as_ref() else {
            return;
        };
        let mut b = Vec::with_capacity(768);
        for i in 0..256 {
            b.push(*lut.red.get(i).unwrap_or(&0) as u8);
            b.push(*lut.green.get(i).unwrap_or(&0) as u8);
            b.push(*lut.blue.get(i).unwrap_or(&0) as u8);
        }
        write_png_chunk(&mut self.body, b"PLTE", &b);
    }

    /// `writePixels`: deflate the plane row-by-row (filter byte 0 per row) into
    /// an IDAT (first frame) or fdAT (with a leading sequence number).
    fn write_pixels(&mut self, chunk: &[u8; 4], stream: &[u8]) -> Result<()> {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;

        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let size_c = if meta.is_rgb { meta.size_c as usize } else { 1 };
        let width = meta.size_x as usize;
        let height = meta.size_y as usize;
        let bytes_per_pixel = meta.pixel_type.bytes_per_sample();
        let signed = matches!(
            meta.pixel_type,
            PixelType::Int8 | PixelType::Int16 | PixelType::Int32
        );
        let interleaved = meta.is_interleaved;
        let little_endian = self.little_endian;

        let plane_size = stream.len() / size_c;
        let row_len = stream.len() / height;

        // The chunk payload begins with the type tag, then (for fdAT) a 4-byte
        // sequence number, then the zlib-compressed scanlines.
        let mut payload: Vec<u8> = Vec::new();
        if chunk == b"fdAT" {
            payload.extend_from_slice(&self.next_sequence_number.to_be_bytes());
            self.next_sequence_number += 1;
        }

        let mut deflater = ZlibEncoder::new(Vec::new(), Compression::default());
        let mut row_buf = vec![0u8; row_len];
        for i in 0..height {
            deflater.write_all(&[0u8]).map_err(BioFormatsError::Io)?; // filter NONE
            if interleaved {
                if little_endian {
                    for col in 0..width * size_c {
                        let offset = (i * size_c * width + col) * bytes_per_pixel;
                        let pixel = bytes_to_int(stream, offset, bytes_per_pixel, true);
                        unpack_bytes_be(
                            pixel,
                            &mut row_buf,
                            col * bytes_per_pixel,
                            bytes_per_pixel,
                        );
                    }
                } else {
                    row_buf.copy_from_slice(&stream[i * row_len..i * row_len + row_len]);
                }
            } else {
                let max = 1i64 << (bytes_per_pixel * 8 - 1);
                for col in 0..width {
                    for c in 0..size_c {
                        let offset = c * plane_size + (i * width + col) * bytes_per_pixel;
                        let mut pixel =
                            bytes_to_int(stream, offset, bytes_per_pixel, little_endian);
                        if signed {
                            if pixel < max {
                                pixel += max;
                            } else {
                                pixel -= max;
                            }
                        }
                        let output = (col * size_c + c) * bytes_per_pixel;
                        unpack_bytes_be(pixel, &mut row_buf, output, bytes_per_pixel);
                    }
                }
            }
            deflater.write_all(&row_buf).map_err(BioFormatsError::Io)?;
        }
        let compressed = deflater.finish().map_err(BioFormatsError::Io)?;
        payload.extend_from_slice(&compressed);

        write_png_chunk(&mut self.body, chunk, &payload);
        Ok(())
    }
}

/// `DataTools.bytesToInt`: read `len` bytes as an integer with the given endian.
fn bytes_to_int(data: &[u8], offset: usize, len: usize, little_endian: bool) -> i64 {
    let mut total: i64 = 0;
    for i in 0..len {
        let shift = if little_endian { i } else { len - 1 - i } * 8;
        total |= ((data[offset + i] as i64) & 0xff) << shift;
    }
    total
}

/// `DataTools.unpackBytes(value, buf, off, len, little=false)`: big-endian.
fn unpack_bytes_be(value: i64, buf: &mut [u8], offset: usize, len: usize) {
    for i in 0..len {
        let shift = (len - 1 - i) * 8;
        buf[offset + i] = ((value >> shift) & 0xff) as u8;
    }
}

impl Default for ApngWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for ApngWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("png") || e.eq_ignore_ascii_case("apng"))
            .unwrap_or(false)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        match meta.pixel_type {
            PixelType::Int8 | PixelType::Uint8 | PixelType::Int16 | PixelType::Uint16 => {}
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "APNG writer supports int8/uint8/int16/uint16, got {other:?}"
                )));
            }
        }
        if meta.is_rgb && !matches!(meta.size_c, 1..=4) {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "APNG writer supports 1-4 channels, got {}",
                meta.size_c
            )));
        }
        self.meta = Some(meta.clone());
        self.body.clear();
        self.num_frames = 0;
        self.next_sequence_number = 0;
        self.little_endian = meta.is_little_endian;
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta
            .as_ref()
            .ok_or_else(|| BioFormatsError::Format("set_metadata first".into()))?;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_next_plane(
            "APNG",
            meta,
            self.num_frames as usize,
            plane_index,
            data.len(),
        )?;
        let width = meta.size_x;
        let height = meta.size_y;

        // `saveBytes`: emit fcTL (and PLTE on the first frame), then the pixel
        // chunk (IDAT for frame 0, fdAT afterwards).
        let first = self.num_frames == 0;
        self.write_fctl(width, height);
        if first {
            self.write_plte();
        }
        let chunk: &[u8; 4] = if first { b"IDAT" } else { b"fdAT" };
        self.write_pixels(chunk, data)?;
        self.num_frames += 1;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        // Only flush if a plane was written and an output path is set.
        if let (Some(path), Some(meta)) = (self.path.clone(), self.meta.clone()) {
            if self.num_frames > 0 {
                crate::formats::stack_writer::validate_complete(
                    "APNG",
                    &meta,
                    self.num_frames as usize,
                )?;
                let bytes_per_pixel = meta.pixel_type.bytes_per_sample();
                let n_channels = if meta.is_rgb { meta.size_c } else { 1 };
                let indexed = meta.is_indexed;

                let mut out: Vec<u8> = Vec::with_capacity(self.body.len() + 64);
                // 8-byte PNG signature.
                out.extend_from_slice(&[0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]);

                // IHDR.
                let mut ihdr = Vec::with_capacity(13);
                ihdr.extend_from_slice(&meta.size_x.to_be_bytes());
                ihdr.extend_from_slice(&meta.size_y.to_be_bytes());
                ihdr.push((bytes_per_pixel * 8) as u8);
                let color_type: u8 = if indexed {
                    3
                } else {
                    match n_channels {
                        1 => 0,
                        2 => 4,
                        3 => 2,
                        4 => 6,
                        _ => 0,
                    }
                };
                ihdr.push(color_type);
                ihdr.push(0); // compression
                ihdr.push(0); // filter
                ihdr.push(0); // interlace
                write_png_chunk(&mut out, b"IHDR", &ihdr);

                // acTL (num_frames, num_plays=0).
                let mut actl = Vec::with_capacity(8);
                actl.extend_from_slice(&self.num_frames.to_be_bytes());
                actl.extend_from_slice(&0u32.to_be_bytes());
                write_png_chunk(&mut out, b"acTL", &actl);

                // fcTL/IDAT/fdAT frame chunks.
                out.extend_from_slice(&self.body);

                // IEND.
                write_png_chunk(&mut out, b"IEND", &[]);

                std::fs::write(&path, &out).map_err(BioFormatsError::Io)?;
            }
        }

        self.path = None;
        self.meta = None;
        self.body.clear();
        self.num_frames = 0;
        self.next_sequence_number = 0;
        self.little_endian = false;
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        true
    }
}

// ---------------------------------------------------------------------------
// 11. POV-Ray density grid (DF3)
// ---------------------------------------------------------------------------
/// POV-Ray density grid reader (`.pov`, `.df3`).
///
/// DF3 format: 6-byte header (3x uint16 BE: x, y, z dimensions) followed
/// by raw uint8 voxel data.
pub struct PovrayReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Option<Vec<u8>>,
}

impl PovrayReader {
    pub fn new() -> Self {
        PovrayReader {
            path: None,
            meta: None,
            pixel_data: None,
        }
    }
}

impl Default for PovrayReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PovrayReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pov") | Some("df3"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 6 {
            return Err(BioFormatsError::Format(
                "DF3 file too short (need at least 6-byte header)".to_string(),
            ));
        }

        let size_x = u16::from_be_bytes([data[0], data[1]]) as u32;
        let size_y = u16::from_be_bytes([data[2], data[3]]) as u32;
        let size_z = u16::from_be_bytes([data[4], data[5]]) as u32;

        if size_x == 0 || size_y == 0 || size_z == 0 {
            return Err(BioFormatsError::Format(
                "DF3 header contains zero dimensions".to_string(),
            ));
        }

        let plane_voxels = (size_x as usize)
            .checked_mul(size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("DF3 plane voxel count overflows".into()))?;
        let total_voxels = plane_voxels
            .checked_mul(size_z as usize)
            .ok_or_else(|| BioFormatsError::Format("DF3 voxel count overflows".into()))?;

        // Java ref (PovrayReader.java:92-95): nBytes = (fileLength - HEADER_SIZE) / (X*Y*Z),
        // pixelType = pixelTypeFromBytes(nBytes, false, false) -> 1=Uint8, 2=Uint16, 4=Uint32.
        let payload = data.len() - 6;
        let n_bytes = payload / total_voxels;
        let (pixel_type, bits_per_pixel) = match n_bytes {
            1 => (PixelType::Uint8, 8),
            2 => (PixelType::Uint16, 16),
            4 => (PixelType::Uint32, 32),
            other => {
                return Err(BioFormatsError::Format(format!(
                    "DF3 unsupported bytes-per-voxel: {} (expected 1, 2, or 4)",
                    other
                )));
            }
        };

        let expected_bytes = total_voxels
            .checked_mul(n_bytes)
            .ok_or_else(|| BioFormatsError::Format("DF3 byte count overflows".into()))?;
        if payload < expected_bytes {
            return Err(BioFormatsError::Format(format!(
                "DF3 pixel payload has {} bytes, expected at least {}",
                payload, expected_bytes
            )));
        }

        let pixel_data = data[6..6 + expected_bytes].to_vec();
        let image_count = size_z.max(1);

        self.path = Some(path.to_path_buf());
        self.pixel_data = Some(pixel_data);
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: false,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_data = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let pixels = self
            .pixel_data
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let plane_bytes =
            meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample();
        let offset = plane_index as usize * plane_bytes;
        let end = offset
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("DF3 plane offset overflows".into()))?;
        Ok(pixels[offset..end].to_vec())
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if x.checked_add(w).is_none_or(|end| end > meta.size_x)
            || y.checked_add(h).is_none_or(|end| end > meta.size_y)
        {
            return Err(BioFormatsError::InvalidData(format!(
                "DF3 region x={x} y={y} width={w} height={h} exceeds image {}x{}",
                meta.size_x, meta.size_y
            )));
        }

        let pixels = self
            .pixel_data
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let bytes_per_pixel = meta.pixel_type.bytes_per_sample();
        let row_bytes = (meta.size_x as usize)
            .checked_mul(bytes_per_pixel)
            .ok_or_else(|| BioFormatsError::Format("DF3 row byte count overflows".into()))?;
        let plane_bytes = row_bytes
            .checked_mul(meta.size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("DF3 plane byte count overflows".into()))?;
        let plane_offset = (plane_index as usize)
            .checked_mul(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("DF3 plane offset overflows".into()))?;
        let out_len = (w as usize)
            .checked_mul(h as usize)
            .and_then(|px| px.checked_mul(bytes_per_pixel))
            .ok_or_else(|| BioFormatsError::Format("DF3 output byte count overflows".into()))?;
        let mut out = Vec::with_capacity(out_len);
        for row in y..y + h {
            let offset = plane_offset + row as usize * row_bytes + x as usize * bytes_per_pixel;
            let width_bytes = w as usize * bytes_per_pixel;
            out.extend_from_slice(&pixels[offset..offset + width_bytes]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// 12. NAF format
// ---------------------------------------------------------------------------
/// Hamamatsu Aquacosmos NAF reader (`.naf`).
///
/// Faithful translation of upstream Java `NAFReader`. NAF stores a 2-byte
/// endian marker (`"II"`/`"MM"`), a series count at offset 98, a description
/// block, then one 256-byte core-metadata record per series (sizeX, sizeY,
/// bit depth, sizeC, sizeZ, sizeT). The pixel-data offset for the first series
/// is located by scanning for Hamamatsu's LUT/marker bytes (`0x03 0x25` runs
/// and the `0xC0 0x2E` sentinel + fixed `LUT_SIZE`/`16063`/`352` deltas);
/// subsequent series follow contiguously. Compressed payloads are unsupported
/// (Java throws `UnsupportedCompressionException`).
pub struct NafReader {
    path: Option<PathBuf>,
    series: Vec<ImageMetadata>,
    offsets: Vec<u64>,
    current_series: usize,
}

impl NafReader {
    pub fn new() -> Self {
        NafReader {
            path: None,
            series: Vec::new(),
            offsets: Vec::new(),
            current_series: 0,
        }
    }
}

impl Default for NafReader {
    fn default() -> Self {
        Self::new()
    }
}

const BURLEIGH_MAGIC: [u8; 4] = [0x66, 0x66, 0x46, 0x40];
/// Java `NAFReader.LUT_SIZE`.
const NAF_LUT_SIZE: u64 = 263168;

/// Java `FormatTools.pixelTypeFromBytes(nBytes, signed=false, fp=(nBytes==8))`
/// as used by NAF.
fn naf_pixel_type(n_bytes: i32) -> Result<(PixelType, u8)> {
    match n_bytes {
        1 => Ok((PixelType::Uint8, 8)),
        2 => Ok((PixelType::Uint16, 16)),
        4 => Ok((PixelType::Uint32, 32)),
        8 => Ok((PixelType::Float64, 64)),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "NAF unsupported bytes-per-pixel {n_bytes}"
        ))),
    }
}

impl FormatReader for NafReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("naf"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Java NAFReader has no isThisType(stream) override: extension-only.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let mut c = ByteCursor::new(&data, true);

        let endian = c.read_string(2, "NAF endian marker")?;
        let little = endian == "II";
        c.little = little;

        c.seek(98);
        let series_count = c.read_i32("NAF series count")?;
        if series_count <= 0 {
            return Err(BioFormatsError::Format(
                "NAF series count must be positive".into(),
            ));
        }
        let series_count = series_count as usize;

        // Description block: skip leading zero bytes, read a C-string, then skip
        // any run of zero ints (Java `while (in.read()==0); readCString();
        // while (in.readInt()==0);`).
        c.seek(192);
        while c.read_u8_opt() == Some(0) {}
        let _description = c.read_cstring();
        loop {
            if c.read_i32("NAF post-description marker")? != 0 {
                break;
            }
        }

        let mut fp = c.pos() as i64;
        if fp % 2 == 0 {
            fp -= 4;
        } else {
            fp -= 1;
        }
        let fp = fp.max(0) as usize;

        let mut offsets = vec![0u64; series_count];
        let mut metas: Vec<ImageMetadata> = Vec::with_capacity(series_count);

        for i in 0..series_count {
            c.seek(fp + i * 256);
            let size_x = c.read_i32("NAF sizeX")?;
            let size_y = c.read_i32("NAF sizeY")?;
            let num_bits = c.read_i32("NAF numBits")?;
            let size_c = c.read_i32("NAF sizeC")?;
            let size_z = c.read_i32("NAF sizeZ")?;
            let size_t = c.read_i32("NAF sizeT")?;
            if size_x <= 0 || size_y <= 0 || size_c <= 0 || size_z <= 0 || size_t <= 0 {
                return Err(BioFormatsError::Format(
                    "NAF core metadata dimensions must be positive".into(),
                ));
            }
            let image_count = (size_z * size_c * size_t) as u32;
            let n_bytes = num_bits / 8;
            let (pixel_type, bits_per_pixel) = naf_pixel_type(n_bytes)?;

            c.skip(4);
            let pointer = c.pos();
            let _name = c.read_cstring();

            if i == 0 {
                // Java: skipBytes(92 - getFilePointer() + pointer) -> pointer+92.
                c.seek(pointer + 92);
                loop {
                    let check = c.read_i32("NAF first-series offset scan")? as i64;
                    if check > c.pos() as i64 {
                        offsets[i] = check as u64 + NAF_LUT_SIZE;
                        break;
                    }
                    c.skip(92);
                    if c.pos() + 4 > c.len() {
                        return Err(BioFormatsError::Format(
                            "NAF first-series pixel offset marker not found".into(),
                        ));
                    }
                }
            } else {
                let mp = &metas[i - 1];
                offsets[i] = offsets[i - 1]
                    + (mp.size_x as u64)
                        * (mp.size_y as u64)
                        * (mp.image_count as u64)
                        * (mp.pixel_type.bytes_per_sample() as u64);
            }

            offsets[i] += 352;
            c.seek(offsets[i] as usize);
            // Skip runs of the `0x03 0x25` + 114-byte block marker.
            while c.pos() + 116 < c.len() {
                if c.read_u8_opt() != Some(3) {
                    break;
                }
                if c.read_u8_opt() != Some(37) {
                    break;
                }
                c.skip(114);
                offsets[i] = c.pos() as u64;
            }

            // Java: seek(getFilePointer() - 1); scan forward for 0xC0 0x2E.
            let start = c.pos().saturating_sub(1);
            let mut found = false;
            let mut q = start;
            while q + 1 < data.len() {
                if data[q] == 192 && data[q + 1] == 46 {
                    offsets[i] = q as u64;
                    found = true;
                    break;
                }
                q += 1;
            }
            if found {
                offsets[i] += 16063;
            }

            // Last-series correction (Java): position so the final plane stack
            // ends exactly at EOF.
            if i == series_count - 1 && i > 0 {
                let needed =
                    (size_x as i64) * (size_y as i64) * (image_count as i64) * (n_bytes as i64);
                offsets[i] = (data.len() as i64 - needed).max(0) as u64;
            }

            metas.push(ImageMetadata {
                size_x: size_x as u32,
                size_y: size_y as u32,
                size_z: size_z as u32,
                size_c: size_c as u32,
                size_t: size_t as u32,
                pixel_type,
                bits_per_pixel,
                image_count,
                dimension_order: DimensionOrder::XYCZT,
                is_rgb: false,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: little,
                resolution_count: 1,
                thumbnail: false,
                series_metadata: HashMap::new(),
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
        }

        self.path = Some(path.to_path_buf());
        self.series = metas;
        self.offsets = offsets;
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series.clear();
        self.offsets.clear();
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current_series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let plane_bytes =
            meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample();
        let base = self.offsets[self.current_series];
        let offset = base + plane_index as u64 * plane_bytes as u64;

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("NAF", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ---------------------------------------------------------------------------
// 13. Burleigh piezo/SPM
// ---------------------------------------------------------------------------
/// Burleigh SPM reader (`.img`).
///
/// Faithful translation of upstream Java `BurleighReader`. The file begins with
/// a little-endian float whose integer part (minus one) gives the version (1 or
/// 2), followed by 16-bit `sizeX`/`sizeY`. Pixel data is a single UINT16 plane
/// at offset 8 (version 1) or 260 (version 2). The trailing acquisition block
/// (scan size, magnification, mode, gain, sample volts, tunnel current) is
/// parsed best-effort and exposed as global metadata and physical pixel sizes.
///
/// Detection follows the Java magic test (`0x66 0x66 {0x46|0x06} 0x40`); the
/// `.img` extension is too generic to be sufficient on its own.
pub struct BurleighReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    ome: Option<OmeImage>,
    pixels_offset: u64,
}

impl BurleighReader {
    pub fn new() -> Self {
        BurleighReader {
            path: None,
            meta: None,
            ome: None,
            pixels_offset: 0,
        }
    }
}

impl Default for BurleighReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for BurleighReader {
    fn is_this_type_by_name(&self, _path: &Path) -> bool {
        // Java sets suffixSufficient=false and suffixNecessary=false; `.img`
        // alone is too generic, so Burleigh is selected by magic bytes.
        false
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Java: magic[0]==0x66 && magic[1]==0x66 && magic[3]==0x40 &&
        //       (magic[2]==0x46 || magic[2]==0x06)
        header.len() >= 4
            && header[0] == BURLEIGH_MAGIC[0]
            && header[1] == BURLEIGH_MAGIC[1]
            && header[3] == BURLEIGH_MAGIC[3]
            && (header[2] == 0x46 || header[2] == 0x06)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let mut c = ByteCursor::new(&data, true);

        let version = c.read_f32("Burleigh version")? as i32 - 1;
        let size_x = c.read_i16("Burleigh sizeX")? as i32;
        let size_y = c.read_i16("Burleigh sizeY")? as i32;
        if size_x <= 0 || size_y <= 0 {
            return Err(BioFormatsError::Format(
                "Burleigh image dimensions must be positive".into(),
            ));
        }

        let pixels_offset = if version == 1 { 8u64 } else { 260u64 };

        // Best-effort acquisition metadata block (Java guards with metadata
        // level != MINIMUM). Parse errors are ignored, leaving sizes unset.
        let mut series_metadata: HashMap<String, MetadataValue> = HashMap::new();
        let (mut x_size, mut y_size, mut z_size) = (0.0f64, 0.0f64, 0.0f64);
        let parsed = (|| -> Result<()> {
            let mut time_per_pixel = 0.0f64;
            let (mut mode, mut gain, mut mag) = (0i32, 0i32, 0i32);
            let (mut sample_volts, mut tunnel_current) = (0.0f64, 0.0f64);
            if version == 1 {
                let len = c.len();
                if len < 40 {
                    return Err(BioFormatsError::Format("Burleigh v1 file too short".into()));
                }
                c.seek(len - 40);
                c.skip(12);
                x_size = c.read_i32("Burleigh xSize")? as f64;
                y_size = c.read_i32("Burleigh ySize")? as f64;
                z_size = c.read_i32("Burleigh zSize")? as f64;
                time_per_pixel = c.read_i16("Burleigh timePerPixel")? as f64 * 50.0;
                mag = c.read_i16("Burleigh mag")? as i32;
                mag = match mag {
                    3 => 10,
                    4 => 50,
                    5 => 250,
                    other => other,
                };
                if mag != 0 {
                    x_size /= mag as f64;
                    y_size /= mag as f64;
                    z_size /= mag as f64;
                }
                mode = c.read_i16("Burleigh mode")? as i32;
                gain = c.read_i16("Burleigh gain")? as i32;
                sample_volts = c.read_f32("Burleigh sampleVolts")? as f64 / 1000.0;
                tunnel_current = c.read_f32("Burleigh tunnelCurrent")? as f64;
            } else if version == 2 {
                c.skip(14);
                x_size = c.read_i32("Burleigh xSize")? as f64;
                y_size = c.read_i32("Burleigh ySize")? as f64;
                z_size = c.read_i32("Burleigh zSize")? as f64;
                mode = c.read_i16("Burleigh mode")? as i32;
                c.skip(4);
                gain = c.read_i16("Burleigh gain")? as i32;
                time_per_pixel = c.read_i16("Burleigh timePerPixel")? as f64 * 50.0;
                c.skip(12);
                sample_volts = c.read_f32("Burleigh sampleVolts")? as f64;
                tunnel_current = c.read_f32("Burleigh tunnelCurrent")? as f64;
                series_metadata.insert(
                    "Force".into(),
                    MetadataValue::Float(c.read_f32("Burleigh force")? as f64),
                );
            }
            series_metadata.insert("Version".into(), MetadataValue::Int(version as i64));
            series_metadata.insert("Image mode".into(), MetadataValue::Int(mode as i64));
            series_metadata.insert("Z gain".into(), MetadataValue::Int(gain as i64));
            series_metadata.insert(
                "Time per pixel (s)".into(),
                MetadataValue::Float(time_per_pixel),
            );
            series_metadata.insert("Sample volts".into(), MetadataValue::Float(sample_volts));
            series_metadata.insert(
                "Tunnel current".into(),
                MetadataValue::Float(tunnel_current),
            );
            series_metadata.insert("Magnification".into(), MetadataValue::Int(mag as i64));
            Ok(())
        })();
        let _ = parsed;

        let meta = ImageMetadata {
            size_x: size_x as u32,
            size_y: size_y as u32,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        // Physical pixel sizes (Java getPhysicalSizeX(xSize / sizeX), etc.).
        let ome = OmeImage {
            physical_size_x: Some(x_size / size_x as f64).filter(|v| *v > 0.0 && v.is_finite()),
            physical_size_y: Some(y_size / size_y as f64).filter(|v| *v > 0.0 && v.is_finite()),
            physical_size_z: Some(z_size).filter(|v| *v > 0.0 && v.is_finite()),
            ..OmeImage::default()
        };

        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.ome = Some(ome);
        self.pixels_offset = pixels_offset;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.ome = None;
        self.pixels_offset = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let plane_bytes =
            meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample();
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.pixels_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Burleigh SPM", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.open_bytes(plane_index)
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        if let (Some(img), Some(src)) = (ome.images.get_mut(0), self.ome.as_ref()) {
            img.physical_size_x = src.physical_size_x;
            img.physical_size_y = src.physical_size_y;
            img.physical_size_z = src.physical_size_z;
        }
        Some(ome)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

#[cfg(test)]
mod mrw_tests {
    use super::MrwReader;
    use crate::common::reader::FormatReader;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// Build a minimal synthetic MRW file with a PRD + WBG block and a 12-bit
    /// packed Bayer mosaic of `sensor_w * sensor_h` samples. Layout follows the
    /// parsing offsets used by `MRWReader.java` / `MrwReader::set_id`.
    fn build_mrw(
        sensor_w: u16,
        sensor_h: u16,
        size_x: u16,
        size_y: u16,
        bayer: u8,
        samples: &[u16],
    ) -> Vec<u8> {
        // PRD block body: 24 bytes minimum.
        let mut prd = vec![0u8; 24];
        prd[8..10].copy_from_slice(&sensor_h.to_be_bytes());
        prd[10..12].copy_from_slice(&sensor_w.to_be_bytes());
        prd[12..14].copy_from_slice(&size_y.to_be_bytes());
        prd[14..16].copy_from_slice(&size_x.to_be_bytes());
        prd[16] = 12; // dataSize
        prd[23] = bayer; // bayerPattern

        // WBG block body: 4 scale bytes + 4 big-endian shorts. Use scale 0 and
        // coeff 64 so wbg[i] = 64 / (64 << 0) = 1.0 (identity gain).
        let mut wbg = vec![0u8; 12];
        for i in 0..4 {
            wbg[4 + i * 2..6 + i * 2].copy_from_slice(&64i16.to_be_bytes());
        }

        // Packed 12-bit samples, MSB-first.
        let mut packed_bits: Vec<u8> = Vec::new();
        let mut acc: u32 = 0;
        let mut nbits = 0u32;
        for &s in samples {
            acc = (acc << 12) | (s as u32 & 0xfff);
            nbits += 12;
            while nbits >= 8 {
                nbits -= 8;
                packed_bits.push(((acc >> nbits) & 0xff) as u8);
            }
        }
        if nbits > 0 {
            packed_bits.push(((acc << (8 - nbits)) & 0xff) as u8);
        }

        // Compose blocks. Each block: 4-char name + be32 length + body.
        let mut blocks: Vec<u8> = Vec::new();
        let push_block = |blocks: &mut Vec<u8>, name: &[u8; 4], body: &[u8]| {
            blocks.extend_from_slice(name);
            blocks.extend_from_slice(&(body.len() as i32).to_be_bytes());
            blocks.extend_from_slice(body);
        };
        push_block(&mut blocks, b"0PRD", &prd);
        push_block(&mut blocks, b"0WBG", &wbg);

        // offset = readInt(@4) + 8 -> points just past the block region (where
        // pixel data starts). The header before blocks is 8 bytes (magic + int).
        let offset = 8 + blocks.len();
        let mut out = Vec::new();
        out.extend_from_slice(b"\0MRM");
        out.extend_from_slice(&((offset - 8) as i32).to_be_bytes());
        out.extend_from_slice(&blocks);
        out.extend_from_slice(&packed_bits);
        out
    }

    fn temp_path(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bioformats_mrw_{name}_{}_{}.mrw",
            std::process::id(),
            unique
        ))
    }

    #[test]
    fn mrw_decodes_interleaved_rgb_plane() {
        // 2x2 image, sensor matches (no row padding). bayer=0 -> COLOR_MAP_2.
        // Distinct sample values so we can confirm channel placement.
        let samples = [100u16, 200, 300, 400];
        let bytes = build_mrw(2, 2, 2, 2, 0, &samples);
        let path = temp_path("decode");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = MrwReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y, meta.size_c), (2, 2, 3));
        assert!(meta.is_rgb && meta.is_interleaved && !meta.is_little_endian);
        assert_eq!(meta.bits_per_pixel, 12);

        let plane = reader.open_bytes(0).unwrap();
        // Interleaved RGB, 2 bytes/sample, big-endian: 2*2 px * 3 * 2 = 24 bytes.
        assert_eq!(plane.len(), 24);

        // Read pixel (row,col) channel c as a big-endian u16.
        let px = |buf: &[u8], row: usize, col: usize, c: usize| -> u16 {
            let base = row * 2 * 6 + col * 6 + c * 2;
            u16::from_be_bytes([buf[base], buf[base + 1]])
        };

        // bayer=0 -> COLOR_MAP_2 = {1,2,0,1}. With identity white balance the
        // present component at each CFA site equals the raw sample:
        //   (0,0) evenRow/evenCol -> green = 100
        //   (0,1) evenRow/oddCol  -> blue  = 200
        //   (1,0) oddRow/evenCol  -> red   = 300
        //   (1,1) oddRow/oddCol   -> green = 400
        assert_eq!(px(&plane, 0, 0, 1), 100);
        assert_eq!(px(&plane, 0, 1, 2), 200);
        assert_eq!(px(&plane, 1, 0, 0), 300);
        assert_eq!(px(&plane, 1, 1, 1), 400);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn mrw_skips_sensor_row_padding() {
        // sensor wider than output: 3-wide sensor, 2-wide output. Each row reads
        // 2 samples then skips dataSize*(3-2)=12 bits. Provide 6 sensor samples
        // (3 per row, 2 rows); the third sample per row must be ignored.
        let samples = [11u16, 22, 999, 33, 44, 888];
        let bytes = build_mrw(3, 2, 2, 2, 0, &samples);
        let path = temp_path("padding");
        std::fs::write(&path, &bytes).unwrap();

        let mut reader = MrwReader::new();
        reader.set_id(&path).unwrap();
        let plane = reader.open_bytes(0).unwrap();
        let px = |buf: &[u8], row: usize, col: usize, c: usize| -> u16 {
            let base = row * 2 * 6 + col * 6 + c * 2;
            u16::from_be_bytes([buf[base], buf[base + 1]])
        };
        // The padding samples 999/888 must not appear as present components.
        assert_eq!(px(&plane, 0, 0, 1), 11); // green (0,0)
        assert_eq!(px(&plane, 0, 1, 2), 22); // blue  (0,1)
        assert_eq!(px(&plane, 1, 0, 0), 33); // red   (1,0)
        assert_eq!(px(&plane, 1, 1, 1), 44); // green (1,1)
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn mrw_accepts_java_magic_suffix_and_rejects_malformed_prd_values() {
        let mut bytes = build_mrw(2, 2, 2, 2, 0, &[1, 2, 3, 4]);
        bytes[0] = b'X';
        let path = temp_path("magic_suffix");
        std::fs::write(&path, &bytes).unwrap();
        let reader = MrwReader::new();
        assert!(reader.is_this_type_by_bytes(&bytes[..4]));
        MrwReader::new().set_id(&path).unwrap();
        std::fs::remove_file(&path).ok();

        let bytes = build_mrw(1, 1, 2, 2, 0, &[1]);
        let path = temp_path("small_sensor");
        std::fs::write(&path, &bytes).unwrap();
        let err = MrwReader::new().set_id(&path).unwrap_err();
        assert!(err.to_string().contains("sensor dimensions"));
        std::fs::remove_file(&path).ok();

        let mut bytes = build_mrw(2, 2, 2, 2, 0, &[1, 2, 3, 4]);
        // WBG block starts after the 8-byte MRW prelude and the 32-byte PRD
        // block. Its first body byte is the first scale value.
        bytes[8 + 32 + 8] = 32;
        let path = temp_path("bad_wbg_scale");
        std::fs::write(&path, &bytes).unwrap();
        let err = MrwReader::new().set_id(&path).unwrap_err();
        assert!(err.to_string().contains("WBG scale"));
        std::fs::remove_file(&path).ok();
    }
}

#[cfg(test)]
mod lof_lut_tests {
    use super::{
        lof_channel_priority, lof_insert_channel_lut_metadata, lof_inverse_rgb, lof_pack_rgba,
        lof_translate_lut, lof_translate_metadata, LofNode,
    };
    use crate::common::metadata::MetadataValue;
    use std::collections::HashMap;

    fn channel_node(lut_name: &str) -> LofNode {
        let mut attrs = HashMap::new();
        if !lut_name.is_empty() {
            attrs.insert("LUTName".to_string(), lut_name.to_string());
        }
        LofNode {
            name: "ChannelDescription".to_string(),
            attrs,
        }
    }

    #[test]
    fn translate_lut_maps_named_colours() {
        // Java translateLut: named LUTs -> packed RGBA (alpha 255).
        assert_eq!(lof_translate_lut("Red"), lof_pack_rgba(255, 0, 0));
        assert_eq!(lof_translate_lut("green"), lof_pack_rgba(0, 255, 0));
        assert_eq!(lof_translate_lut("Blue"), lof_pack_rgba(0, 0, 255));
        assert_eq!(lof_translate_lut("Cyan"), lof_pack_rgba(0, 255, 255));
        assert_eq!(lof_translate_lut("Magenta"), lof_pack_rgba(255, 0, 255));
        assert_eq!(lof_translate_lut("Yellow"), lof_pack_rgba(255, 255, 0));
        // Whitespace is stripped before matching.
        assert_eq!(lof_translate_lut("  Red "), lof_pack_rgba(255, 0, 0));
        // Unknown / empty falls back to opaque white.
        assert_eq!(lof_translate_lut(""), lof_pack_rgba(255, 255, 255));
        assert_eq!(lof_translate_lut("Glow"), lof_pack_rgba(255, 255, 255));
    }

    #[test]
    fn translate_lut_decodes_gradient_in_reverse() {
        // Gradient(b,g,r): Java reads component 2 as red, 1 as green, 0 as blue.
        assert_eq!(
            lof_translate_lut("Gradient(10,20,30)"),
            lof_pack_rgba(30, 20, 10)
        );
        // Case-insensitive name and stripped whitespace.
        assert_eq!(
            lof_translate_lut("gradient( 1 , 2 , 3 )"),
            lof_pack_rgba(3, 2, 1)
        );
        // Out-of-range components are not a gradient -> white fallback.
        assert_eq!(
            lof_translate_lut("Gradient(300,0,0)"),
            lof_pack_rgba(255, 255, 255)
        );
    }

    #[test]
    fn channel_priority_is_case_sensitive_lowercase() {
        // Java getChannelPriority switches on exact lowercase strings.
        assert_eq!(lof_channel_priority("red"), 0);
        assert_eq!(lof_channel_priority("green"), 1);
        assert_eq!(lof_channel_priority("blue"), 2);
        assert_eq!(lof_channel_priority("cyan"), 3);
        assert_eq!(lof_channel_priority("magenta"), 4);
        assert_eq!(lof_channel_priority("yellow"), 5);
        assert_eq!(lof_channel_priority("black"), 6);
        assert_eq!(lof_channel_priority("gray"), 7);
        // Capitalised LUT names fall through to the default (gray/identity).
        assert_eq!(lof_channel_priority("Red"), 8);
        assert_eq!(lof_channel_priority(""), 8);
    }

    #[test]
    fn inverse_rgb_requires_explicit_rgb_order() {
        let rgb = [
            &channel_node("Red"),
            &channel_node("Green"),
            &channel_node("Blue"),
        ];
        assert!(!lof_inverse_rgb(&rgb));

        let bgr = [
            &channel_node("Blue"),
            &channel_node("Green"),
            &channel_node("Red"),
        ];
        assert!(lof_inverse_rgb(&bgr));

        // Fewer than three channels -> BGR assumed.
        let two = [&channel_node("Red"), &channel_node("Green")];
        assert!(lof_inverse_rgb(&two));
    }

    #[test]
    fn channel_lut_metadata_records_colour_priority_and_order() {
        let nodes = [
            &channel_node("red"),
            &channel_node("green"),
            &channel_node("blue"),
        ];
        let mut metadata = HashMap::new();
        lof_insert_channel_lut_metadata(&mut metadata, &nodes);

        let red = lof_pack_rgba(255, 0, 0) as i64;
        assert!(matches!(
            metadata.get("lof.channel.0.lut_color"),
            Some(MetadataValue::Int(v)) if *v == red
        ));
        assert!(matches!(
            metadata.get("lof.channel.0.channel_priority"),
            Some(MetadataValue::Int(0))
        ));
        assert!(matches!(
            metadata.get("lof.channel.2.channel_priority"),
            Some(MetadataValue::Int(2))
        ));
        // Lowercase r/g/b is not the explicit "Red"/"Green"/"Blue" order.
        assert!(matches!(
            metadata.get("lof.inverse_rgb"),
            Some(MetadataValue::String(v)) if v == "true"
        ));
    }

    #[test]
    fn translate_metadata_populates_channel_colours() {
        let xml = r#"<Data><Image>
            <ImageDescription>
              <Channels>
                <ChannelDescription DataType="0" ChannelTag="0" Resolution="8"
                  NameOfMeasurement="" Min="0" Max="255" Unit="" LUTName="Red"
                  IsLUTInverted="0" BytesInc="0" BitInc="0"/>
                <ChannelDescription DataType="0" ChannelTag="0" Resolution="8"
                  NameOfMeasurement="" Min="0" Max="255" Unit="" LUTName="Green"
                  IsLUTInverted="0" BytesInc="1" BitInc="0"/>
              </Channels>
              <Dimensions>
                <DimensionDescription DimID="1" NumberOfElements="4" Length="0"
                  Unit="" BytesInc="1" BitInc="0"/>
                <DimensionDescription DimID="2" NumberOfElements="3" Length="0"
                  Unit="" BytesInc="4" BitInc="0"/>
              </Dimensions>
            </ImageDescription>
          </Image></Data>"#;

        let info = lof_translate_metadata(xml).unwrap();
        assert_eq!(info.ome.channels.len(), 2);
        assert_eq!(info.ome.channels[0].color, Some(lof_pack_rgba(255, 0, 0)));
        assert_eq!(info.ome.channels[1].color, Some(lof_pack_rgba(0, 255, 0)));
        let green = lof_pack_rgba(0, 255, 0) as i64;
        assert!(matches!(
            info.meta.series_metadata.get("lof.channel.1.lut_color"),
            Some(MetadataValue::Int(v)) if *v == green
        ));
    }
}
