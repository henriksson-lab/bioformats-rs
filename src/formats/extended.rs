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
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ome_metadata::{
    create_lsid, OmeChannel, OmeImage, OmeMetadata, OmePlate, OmeWell, OmeWellSample,
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

impl DngReader {
    pub fn new() -> Self {
        DngReader {
            inner: crate::tiff::TiffReader::new(),
            cfa: None,
        }
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
    use super::adjust_for_white_balance;

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

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
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

                // Java default color map {1,0,2,1}; overridden by COLOR_MAP tag
                // (320) when all four entries are valid channel indices 0..=2.
                let mut color_map = [1i32, 0, 2, 1];
                let ifd_colors = ifd.get_vec_u16(crate::tiff::ifd::tag::COLOR_MAP);
                if ifd_colors.len() >= 4 {
                    let valid = ifd_colors[..4].iter().all(|&c| c <= 2);
                    if valid {
                        for q in 0..4 {
                            color_map[q] = ifd_colors[q] as i32;
                        }
                    }
                }

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
tiff_wrapper! {
    /// Akoya/PerkinElmer Phenocycler QPTIFF — TIFF-based (`.qptiff`).
    pub struct QptiffReader;
    extensions: ["qptiff"];
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
}

impl GelReader {
    pub fn new() -> Self {
        GelReader {
            inner: crate::tiff::TiffReader::new(),
            meta: None,
            square_root: false,
            scale: 1.0,
        }
    }
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
        self.scale = first
            .get(MD_SCALE_PIXEL)
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
            .unwrap_or(1.0);

        // imageCount == number of IFDs; reported as the T dimension (Java
        // GelReader.initMetadata sets sizeT = imageCount, sizeZ/sizeC = 1).
        let mut ifds = 0u32;
        while self.inner.ifd(ifds as usize).is_some() {
            ifds += 1;
        }
        let ifds = ifds.max(1);
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

        if !self.square_root {
            // LINEAR: plain TIFF pixels.
            return self.inner.open_bytes(plane_index);
        }

        // SQUARE_ROOT: the TIFF holds unsigned-short samples that must be
        // squared and multiplied by the scale, then emitted as 32-bit floats.
        // We read the raw shorts directly (the TIFF reports a 16-bit type for
        // these IFDs) rather than letting any float interpretation occur.
        let raw = self.inner.open_bytes(plane_index)?;
        let n = raw.len() / 2;
        let mut out = vec![0u8; n * 4];
        for i in 0..n {
            let value = if little_endian {
                u16::from_le_bytes([raw[i * 2], raw[i * 2 + 1]])
            } else {
                u16::from_be_bytes([raw[i * 2], raw[i * 2 + 1]])
            } as u64;
            let pixel = (value * value) as f64 * self.scale;
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
const IMSPECTOR_MAGIC_NUMBER: u16 = 0xffff;
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

fn parse_imspector_synthetic_stack(bytes: &[u8]) -> Result<Option<ImspectorStack>> {
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
    let stack_offset = usize::try_from(stack_offset).map_err(|_| {
        BioFormatsError::Format("Imspector OBF/MSR stack offset overflows usize".into())
    })?;
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
    meta: Option<ImageMetadata>,
    bytes: Vec<u8>,
    stack: Option<ImspectorStack>,
}

impl ImspectorReader {
    pub fn new() -> Self {
        ImspectorReader {
            path: None,
            meta: None,
            bytes: Vec::new(),
            stack: None,
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
        parse_imspector_header(header).is_ok()
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.bytes.clear();
        self.stack = None;
        let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let header = parse_imspector_header(&bytes)?;
        if let Some(stack) = parse_imspector_synthetic_stack(&bytes)? {
            self.path = Some(path.to_path_buf());
            self.meta = Some(stack.meta.clone());
            self.bytes = bytes;
            self.stack = Some(stack);
            return Ok(());
        }
        let mut detail = format!(
            "Imspector OBF/MSR native stack decoding is unsupported unless explicit BFIMSPECTOR_RAW_STACK_V1 data is present (version {})",
            header.version
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
        self.meta = None;
        self.bytes.clear();
        self.stack = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        if self.meta.is_some() {
            1
        } else {
            0
        }
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
        let stack = self.stack.as_ref().ok_or(BioFormatsError::NotInitialized)?;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
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
        IMSPECTOR_SYNTHETIC_STACK_MAGIC,
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
}

// ---------------------------------------------------------------------------
// 5. Hamamatsu VMS whole-slide
// ---------------------------------------------------------------------------

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
                    | "imagefile"
                    | "mapfile"
                    | "optimisationfile"
                    | "optimizationfile"
                    | "physicalwidth"
                    | "physicalheight"
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
    let keys = hamamatsu_vms_tile_key_candidates("imagefile", layer, col, row, true);
    for key in &keys {
        if let Some(value) = values.get(key) {
            return Some(value);
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
            (Some(count), Some(_)) => count == self.icc_seen_chunks && count > 0,
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
        if meta.icc_seen_chunks == count && meta.icc_missing_chunks == 0 {
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

fn hamamatsu_vms_parse_optional_index(path: &Path) -> Result<HashMap<String, String>> {
    let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
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
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Hamamatsu VMS JPEG tile pixel format {other:?} is unsupported"
            )));
        }
    };
    Ok((info.width as u32, info.height as u32, rgb))
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
                let tile_path = parent.join(name);
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
}

impl HamamatsuVmsReader {
    pub fn new() -> Self {
        HamamatsuVmsReader {
            path: None,
            series: Vec::new(),
            current_series: 0,
            current_resolution: 0,
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
                is_interleaved: true,
                is_indexed: false,
                is_little_endian: true,
                resolution_count: pyramid_sizes.len() as u32,
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

        for (kind, key, physical_width_key, physical_height_key) in [
            (
                "macro",
                "macroimage",
                "physicalmacrowidth",
                "physicalmacroheight",
            ),
            ("map", "mapfile", "", ""),
        ] {
            let Some(name) = values.get(key) else {
                continue;
            };
            let image_path = parent.join(name);
            let (size_x, size_y) = hamamatsu_vms_jpeg_dimensions(&image_path)?;
            let mut series_metadata = base_series_metadata.clone();
            series_metadata.insert("VMS series kind".into(), MetadataValue::String(kind.into()));
            hamamatsu_vms_insert_jpeg_metadata(&mut series_metadata, "VMS image", &image_path)?;
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
                    is_interleaved: true,
                    is_indexed: false,
                    is_little_endian: true,
                    resolution_count: 1,
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
    use crate::common::metadata::MetadataValue;
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
        assert!(matches!(
            reader.metadata().series_metadata.get("VMS series kind"),
            Some(MetadataValue::String(v)) if v == "macro"
        ));
        let macro_plane = reader.open_bytes(0).unwrap();
        assert_eq!(macro_plane.len(), 6);

        reader.set_series(2).unwrap();
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (1, 1));
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
                } if namespace.as_deref()
                    == Some("openmicroscopy.org/bioformats/original-metadata") =>
                {
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
    meta: Option<ImageMetadata>,
    pixel_offset: u64,
    /// Decoded (decompressed for .c01, raw for .dib) file bytes.
    data: Vec<u8>,
}

impl CellomicsReader {
    pub fn new() -> Self {
        CellomicsReader {
            path: None,
            meta: None,
            pixel_offset: 52,
            data: Vec::new(),
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
        if let Some(value) = mdb_row_value(row, &column_index, "name") {
            metadata.insert(
                format!("{prefix}.name"),
                MetadataValue::String(value.to_string()),
            );
        }
        if let Some(value) = mdb_row_value(row, &column_index, "exposuretime") {
            let value = value.trim();
            if let Ok(exposure) = value.parse::<f64>() {
                metadata.insert(
                    format!("{prefix}.exposure_time"),
                    MetadataValue::Float(exposure),
                );
            } else {
                metadata.insert(
                    format!("{prefix}.exposure_time"),
                    MetadataValue::String(value.to_string()),
                );
            }
        }
        if let Some(value) = mdb_row_value(row, &column_index, "compositecolor") {
            let value = value.trim();
            if let Ok(color) = value.parse::<i64>() {
                metadata.insert(
                    format!("{prefix}.composite_color"),
                    MetadataValue::Int(color),
                );
            } else {
                metadata.insert(
                    format!("{prefix}.composite_color"),
                    MetadataValue::String(value.to_string()),
                );
            }
        }
    }
    metadata
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
    match crate::common::mdb::parse_table(&mdb_path, "asnProtocolChannel") {
        Ok(Some(table)) => metadata.extend(cellomics_channel_metadata_from_table(&table)),
        Ok(None) => {
            metadata.insert(
                "cellomics.mdb_missing_table".into(),
                MetadataValue::String("asnProtocolChannel".into()),
            );
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

#[cfg(test)]
mod cellomics_mdb_tests {
    use super::{cellomics_channel_metadata_from_table, cellomics_plate_prefix};
    use crate::common::mdb::MdbTable;
    use crate::common::metadata::MetadataValue;
    use std::path::Path;

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
                "Ignored".into(),
            ],
            rows: vec![
                vec!["DAPI".into(), "35.5".into(), "16711680".into(), "x".into()],
                vec!["FITC".into(), "n/a".into(), "green".into(), "x".into()],
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
            metadata.get("cellomics.channel.1.exposure_time"),
            Some(MetadataValue::String(v)) if v == "n/a"
        ));
        assert!(matches!(
            metadata.get("cellomics.channel.1.composite_color"),
            Some(MetadataValue::String(v)) if v == "green"
        ));
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
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let raw = std::fs::read(path).map_err(BioFormatsError::Io)?;
        // .c01 files are zlib-compressed after a 4-byte magic; .dib are raw.
        let is_c01 = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("c01"))
            .unwrap_or(false);
        let data = if is_c01 {
            if raw.len() < 4 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Cellomics C01 file is too short to contain a magic number".into(),
                ));
            }
            crate::common::codec::decompress_deflate(&raw[4..]).map_err(|_| {
                BioFormatsError::UnsupportedFormat(
                    "Cellomics C01 zlib payload could not be decompressed".into(),
                )
            })?
        } else {
            raw
        };

        let (w, h, image_count, pixel_type, bpp, pixel_offset) = if data.len() >= 52 {
            let dib_header_size = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
            if dib_header_size >= 40 {
                let w = i32::from_le_bytes([data[4], data[5], data[6], data[7]]).unsigned_abs();
                let h = i32::from_le_bytes([data[8], data[9], data[10], data[11]]).unsigned_abs();
                let n_planes = u16::from_le_bytes([data[12], data[13]]) as u32;
                let bd = u16::from_le_bytes([data[14], data[15]]);
                let compression = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
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
                        BioFormatsError::Format(
                            "Cellomics DIB total pixel size overflows".to_string(),
                        )
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
                parse_legacy_cellomics_header(&data)?
            }
        } else if data.len() >= 10 {
            parse_legacy_cellomics_header(&data)?
        } else {
            return Err(BioFormatsError::UnsupportedFormat(
                "Cellomics header is too short to determine image dimensions".to_string(),
            ));
        };

        let bytes_per_pixel = (bpp / 8) as u64;
        let plane_bytes = (w as u64)
            .checked_mul(h as u64)
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

        self.path = Some(path.to_path_buf());
        self.pixel_offset = pixel_offset;
        self.data = data;
        let series_metadata = read_cellomics_mdb_metadata(path);

        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
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
        self.pixel_offset = 52;
        self.data = Vec::new();
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
        let bytes_per_pixel = (meta.bits_per_pixel / 8) as usize;
        let n_bytes = meta.size_x as usize * meta.size_y as usize * bytes_per_pixel;
        // Pixel data lives in the decoded (decompressed for .c01) buffer.
        let plane_offset = self
            .pixel_offset
            .checked_add(
                (plane_index as u64)
                    .checked_mul(n_bytes as u64)
                    .ok_or_else(|| {
                        BioFormatsError::Format("Cellomics plane offset overflows".to_string())
                    })?,
            )
            .ok_or_else(|| {
                BioFormatsError::Format("Cellomics plane offset overflows".to_string())
            })? as usize;
        if plane_offset + n_bytes > self.data.len() {
            return Err(BioFormatsError::InvalidData(
                "Cellomics plane extends beyond decoded payload".to_string(),
            ));
        }
        Ok(self.data[plane_offset..plane_offset + n_bytes].to_vec())
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
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Cellomics", &full, meta, 1, x, y, w, h)
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
        header.len() >= 4 && header[..4] == *b"\0MRM"
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 8 || data[..4] != *b"\0MRM" {
            return Err(BioFormatsError::UnsupportedFormat(
                "MRW: missing '\\0MRM' magic string".into(),
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
fn yk_attr(e: &quick_xml::events::BytesStart, name: &str) -> Option<String> {
    for a in e.attributes().flatten() {
        if a.key.as_ref() == name.as_bytes() {
            return Some(String::from_utf8_lossy(&a.value).to_string());
        }
    }
    None
}

fn yk_attr_int(e: &quick_xml::events::BytesStart, name: &str) -> Option<i64> {
    yk_attr(e, name).and_then(|s| s.trim().parse::<i64>().ok())
}

fn yk_attr_positive_i64(e: &quick_xml::events::BytesStart, name: &str) -> Result<i64> {
    let value = yk_attr_int(e, name).unwrap_or(1);
    if value <= 0 {
        return Err(BioFormatsError::Format(format!(
            "Yokogawa CV7000 attribute {name} must be positive, got {value}"
        )));
    }
    Ok(value)
}

fn yk_attr_f64(e: &quick_xml::events::BytesStart, name: &str) -> Option<f64> {
    yk_attr(e, name).and_then(|s| s.trim().parse::<f64>().ok())
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
                    plate.name = yk_attr(e, "bts:Name");
                    plate.rows = yk_attr_int(e, "bts:Rows").unwrap_or(0) as u32;
                    plate.columns = yk_attr_int(e, "bts:Columns").unwrap_or(0) as u32;
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
                    let bts_type = yk_attr(e, "bts:Type").unwrap_or_default();
                    if bts_type == "IMG" {
                        in_img_record = true;
                        // attributes are 1-based in the file; convert to 0-based.
                        let p = YokogawaPlane {
                            row: (yk_attr_positive_i64(e, "bts:Row")? - 1) as u32,
                            column: (yk_attr_positive_i64(e, "bts:Column")? - 1) as u32,
                            field: (yk_attr_positive_i64(e, "bts:FieldIndex")? - 1) as u32,
                            z: (yk_attr_positive_i64(e, "bts:ZIndex")? - 1) as i32,
                            channel: (yk_attr_positive_i64(e, "bts:Ch")? - 1) as i32,
                            timepoint: (yk_attr_positive_i64(e, "bts:TimePoint")? - 1) as i32,
                            action_index: (yk_attr_positive_i64(e, "bts:ActionIndex")? - 1) as i32,
                            timeline_index: (yk_attr_positive_i64(e, "bts:TimelineIndex")? - 1)
                                as i32,
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
                    current_text.push_str(&t.unescape().unwrap_or_default());
                }
            }
            Ok(Event::End(ref e)) => {
                if e.name().as_ref() == b"bts:MeasurementRecord" && in_img_record {
                    let value = current_text.trim();
                    if !value.is_empty() {
                        let img = parent.join(value);
                        if !img.exists() {
                            return Err(BioFormatsError::UnsupportedFormat(format!(
                                "Yokogawa CV7000 MeasurementData.mlf references missing image file {}",
                                img.display()
                            )));
                        }
                        if let Some(last) = planes.last_mut() {
                            last.file = Some(img);
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
                        index: (yk_attr_int(e, "bts:Ch").unwrap_or(1) - 1) as i32,
                        action_index: 0,
                        timeline_index: 0,
                        x_size: yk_attr_f64(e, "bts:HorizontalPixelDimension"),
                        y_size: yk_attr_f64(e, "bts:VerticalPixelDimension"),
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

        for p in &planes {
            if p.file.is_none() {
                continue;
            }
            if p.row >= plate.rows || p.column >= plate.columns {
                return Err(BioFormatsError::Format(format!(
                    "Yokogawa CV7000 plane references well row {}, column {} outside declared plate {}x{}",
                    p.row + 1,
                    p.column + 1,
                    plate.rows,
                    plate.columns
                )));
            }
            if first_file.is_none() {
                first_file = p.file.clone();
            }
            let well_number = p.row * plate_columns + p.column;
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
            let Some(_) = p.file.as_ref() else { continue };
            let well_number = p.row * plate_columns + p.column;
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

        for (series_index, files) in plane_files.iter().enumerate() {
            for (plane_index, file) in files.iter().enumerate() {
                let Some(file) = file else {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Yokogawa CV7000: series {series_index} plane {plane_index} has no companion TIFF payload"
                    )));
                };
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
            // Missing plane: return zero-filled buffer (Java fills with 0).
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
/// names) are projected as conservative metadata, but the wider Leica
/// LeicaMicrosystemsMetadata translation (instrument / detector / ROI / BGR
/// channel ordering) is intentionally not ported and is left as an honest gap.
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

pub struct LeicaLofReader {
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
    current_series: usize,
}

impl LeicaLofReader {
    pub fn new() -> Self {
        LeicaLofReader {
            path: None,
            meta: None,
            ome: None,
            data_offset: 0,
            end_pointer: 0,
            tile_count: 1,
            tile_bytes_inc: 0,
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

impl Default for LeicaLofReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LeicaLofReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("lof"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        LeicaLofReader::check_magic(header)
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
        if memory_size <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Leica LOF contains no image data, it cannot be opened directly".into(),
            ));
        }
        let data_offset = c.pos() as u64;

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
            if !src.channels.is_empty() {
                img.channels = src.channels.clone();
            }
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
                        .unescape_value()
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
        ..OmeImage::default()
    };

    Ok(LofImageInfo {
        meta,
        ome,
        tile_count,
        tile_bytes_inc,
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

fn lof_ome_channels(
    channel_nodes: &[&LofNode],
    effective_c: usize,
    samples_per_pixel: u32,
) -> Vec<OmeChannel> {
    (0..effective_c)
        .map(|channel_index| {
            let node = channel_nodes.get(channel_index).copied();
            OmeChannel {
                name: node.and_then(lof_channel_name),
                samples_per_pixel,
                excitation_wavelength: node.and_then(|n| lof_attr_f64(n, "ExcitationWavelength")),
                emission_wavelength: node.and_then(|n| lof_attr_f64(n, "EmissionWavelength")),
                ..OmeChannel::default()
            }
        })
        .collect()
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
// 10. Animated PNG — delegates to PngReader
// ---------------------------------------------------------------------------
/// Animated PNG reader (`.apng`).
///
/// Tries to open the file as a regular PNG via `PngReader` (reads the first
/// frame). Full APNG animation decoding is not supported.
pub struct ApngReader {
    inner: crate::formats::png::PngReader,
}

impl ApngReader {
    pub fn new() -> Self {
        ApngReader {
            inner: crate::formats::png::PngReader::new(),
        }
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
        // PNG magic: 89 50 4E 47 0D 0A 1A 0A
        header.len() >= 8 && header[..8] == [0x89, 0x50, 0x4E, 0x47, 0x0D, 0x0A, 0x1A, 0x0A]
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path).map_err(|err| match err {
            BioFormatsError::UnsupportedFormat(_) => err,
            _ => BioFormatsError::UnsupportedFormat(
                "APNG file could not be opened as PNG (animated PNG may require dedicated parser)"
                    .to_string(),
            ),
        })
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
// 11. POV-Ray density grid (DF3)
// ---------------------------------------------------------------------------
/// POV-Ray density grid reader (`.pov`, `.df3`).
///
/// DF3 format: 6-byte header (3x uint16 BE: x, y, z dimensions) followed
/// by raw uint8 voxel data.
pub struct PovRayReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Option<Vec<u8>>,
}

impl PovRayReader {
    pub fn new() -> Self {
        PovRayReader {
            path: None,
            meta: None,
            pixel_data: None,
        }
    }
}

impl Default for PovRayReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PovRayReader {
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
        if payload != expected_bytes {
            return Err(BioFormatsError::Format(format!(
                "DF3 pixel payload has {} bytes, expected {}",
                payload, expected_bytes
            )));
        }

        let pixel_data = data[6..].to_vec();
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
        let plane_bytes = meta.size_x as usize * meta.size_y as usize;
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
        let row_bytes = meta.size_x as usize;
        let plane_bytes = row_bytes
            .checked_mul(meta.size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("DF3 plane byte count overflows".into()))?;
        let plane_offset = (plane_index as usize)
            .checked_mul(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("DF3 plane offset overflows".into()))?;
        let mut out = Vec::with_capacity(w as usize * h as usize);
        for row in y..y + h {
            let offset = plane_offset + row as usize * row_bytes + x as usize;
            out.extend_from_slice(&pixels[offset..offset + w as usize]);
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
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("img"))
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
    fn mrw_rejects_loose_magic_and_malformed_prd_values() {
        let mut bytes = build_mrw(2, 2, 2, 2, 0, &[1, 2, 3, 4]);
        bytes[0] = b'X';
        let path = temp_path("bad_magic");
        std::fs::write(&path, &bytes).unwrap();
        let reader = MrwReader::new();
        assert!(!reader.is_this_type_by_bytes(&bytes[..4]));
        let err = MrwReader::new().set_id(&path).unwrap_err();
        assert!(err.to_string().contains("\\0MRM"));
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
