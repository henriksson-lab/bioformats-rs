//! Extended format readers for Bio-Formats Rust.
//!
//! Group A: TIFF-based wrappers (DNG, QPTIFF, GEL).
//! Group B: Binary readers with structure (Imspector OBF, Hamamatsu VMS, Cellomics).
//! Group C: Extension-only placeholder readers (MRW, Yokogawa, etc.).

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ome_metadata::{
    create_lsid, OmeImage, OmeMetadata, OmePlate, OmeWell, OmeWellSample,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

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
    /// Cached demosaiced interleaved RGB plane.
    full_image: Option<Vec<u8>>,
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
        if let Some(c) = &mut self.cfa {
            if p != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(p));
            }
            if c.full_image.is_none() {
                c.full_image = Some(Self::decode_cfa(c)?);
            }
            return Ok(c.full_image.clone().unwrap());
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
    if imspector_compression_flag(compression)? {
        return Err(BioFormatsError::UnsupportedFormat(
            "Imspector OBF/MSR compressed synthetic stack payload is unsupported; explicit uncompressed BFIMSPECTOR_RAW_STACK_V1 payloads are supported".to_string(),
        ));
    }
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
    if payload_len != expected_len {
        return Err(BioFormatsError::Format(format!(
            "Imspector OBF/MSR payload length {payload_len} does not match declared stack size {expected_len}"
        )));
    }

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
        MetadataValue::String("synthetic-uncompressed-raw".into()),
    );

    Ok(Some(ImspectorStack {
        meta,
        payload_offset,
        plane_len,
    }))
}

/// Imspector OBF/MSR STED microscopy format (`.obf`, `.msr`).
///
/// Header parsing is translated from Bio-Formats' `OBFReader`. Only a strict,
/// uncompressed raw subset with an explicit stack marker is decoded; unknown
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
        Ok(self.bytes[start..end].to_vec())
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

    fn synthetic_stack(width: i32, height: i32, z: i32, c: i32, t: i32, pixels: &[u8]) -> Vec<u8> {
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
        bytes.extend_from_slice(&0i32.to_le_bytes());
        let payload_offset =
            (stack_offset as usize + IMSPECTOR_SYNTHETIC_STACK_MAGIC.len() + 44) as i64;
        bytes.extend_from_slice(&payload_offset.to_le_bytes());
        bytes.extend_from_slice(&(pixels.len() as i64).to_le_bytes());
        bytes.extend_from_slice(pixels);
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
}

// ---------------------------------------------------------------------------
// 5. Hamamatsu VMS whole-slide
// ---------------------------------------------------------------------------

const HAMAMATSU_VMS_UNSUPPORTED: &str =
    "Hamamatsu VMS/VMU native JPEG tile payload decoding is unsupported";

fn hamamatsu_vms_looks_like_index(bytes: &[u8]) -> Result<()> {
    let text = std::str::from_utf8(bytes).map_err(|_| {
        BioFormatsError::Format("Not a Hamamatsu VMS/VMU text index file".to_string())
    })?;
    let mut saw_assignment = false;
    let mut saw_vms_key = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some((key, _value)) = line.split_once('=') {
            saw_assignment = true;
            let key = key.trim().to_ascii_lowercase();
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
        }
    }

    if saw_assignment && saw_vms_key {
        Ok(())
    } else {
        Err(BioFormatsError::Format(
            "Not a Hamamatsu VMS/VMU text index file".to_string(),
        ))
    }
}

fn hamamatsu_vms_unsupported() -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(HAMAMATSU_VMS_UNSUPPORTED.to_string())
}

/// Hamamatsu VMS/VMU whole-slide format stub (`.vms`, `.vmu`).
///
/// Native tile metadata and JPEG payload decoding are unsupported.
pub struct HamamatsuVmsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl HamamatsuVmsReader {
    pub fn new() -> Self {
        HamamatsuVmsReader {
            path: None,
            meta: None,
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
        self.path = None;
        self.meta = None;
        let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
        hamamatsu_vms_looks_like_index(&bytes)?;
        Err(hamamatsu_vms_unsupported())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
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
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
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
        let _ = plane_index;
        Err(hamamatsu_vms_unsupported())
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let _ = (plane_index, w, h);
        Err(hamamatsu_vms_unsupported())
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let _ = plane_index;
        Err(hamamatsu_vms_unsupported())
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
mod hamamatsu_vms_tests {
    use super::HamamatsuVmsReader;
    use crate::common::error::BioFormatsError;
    use crate::common::reader::FormatReader;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("bioformats_hamamatsu_vms_{name}"))
    }

    #[test]
    fn hamamatsu_vms_validates_text_index_before_unsupported_payload() {
        let path = temp_path("index.vms");
        std::fs::write(
            &path,
            b"NoLayers=1\nImageFile=tile.jpg\nPhysicalWidth=2\nPhysicalHeight=2\n",
        )
        .unwrap();

        let mut reader = HamamatsuVmsReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("native JPEG tile payload decoding is unsupported")),
            "{err:?}"
        );
        assert_eq!(reader.series_count(), 0);
        assert!(matches!(
            reader.set_series(0),
            Err(BioFormatsError::NotInitialized)
        ));

        let _ = std::fs::remove_file(path);
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
/// Leica single-image LOF reader (`.lof`).
///
/// Leica LOF is a proprietary binary format used by Leica Application Suite.
/// The internal structure is vendor-specific and undocumented.
const LEICA_LOF_MARKER: &[u8] = b"LMS_Object_File";
const LEICA_LOF_STRICT_RAW_MAGIC: &[u8; 16] = b"BFLEICALOFRAW001";
const LEICA_LOF_UNSUPPORTED: &str = "Leica LOF native payload decoding is unsupported";

fn leica_lof_validate_header(bytes: &[u8]) -> Result<()> {
    if bytes
        .windows(LEICA_LOF_MARKER.len())
        .any(|window| window == LEICA_LOF_MARKER)
    {
        Ok(())
    } else {
        Err(BioFormatsError::Format(
            "Not a Leica LOF file: missing LMS_Object_File marker".to_string(),
        ))
    }
}

fn leica_lof_unsupported() -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(LEICA_LOF_UNSUPPORTED.to_string())
}

pub struct LeicaLofReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Vec<u8>,
    plane_bytes: usize,
}

impl LeicaLofReader {
    pub fn new() -> Self {
        LeicaLofReader {
            path: None,
            meta: None,
            pixel_data: Vec::new(),
            plane_bytes: 0,
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
        header.starts_with(LEICA_LOF_STRICT_RAW_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let bytes = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if bytes.starts_with(LEICA_LOF_STRICT_RAW_MAGIC) {
            let parsed =
                parse_extended_strict_raw(&bytes, LEICA_LOF_STRICT_RAW_MAGIC, "Leica LOF")?;
            self.path = Some(path.to_path_buf());
            self.meta = Some(parsed.meta);
            self.pixel_data = parsed.payload;
            self.plane_bytes = parsed.plane_bytes;
            return Ok(());
        }
        leica_lof_validate_header(&bytes)?;
        Err(leica_lof_unsupported())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_data.clear();
        self.plane_bytes = 0;
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
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
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
        let start = self
            .plane_bytes
            .checked_mul(plane_index as usize)
            .ok_or_else(|| {
                BioFormatsError::Format("Leica LOF strict raw plane offset overflows".into())
            })?;
        let end = start.checked_add(self.plane_bytes).ok_or_else(|| {
            BioFormatsError::Format("Leica LOF strict raw plane end overflows".into())
        })?;
        Ok(self.pixel_data[start..end].to_vec())
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
        crop_full_plane("Leica LOF", &full, meta, 1, x, y, w, h)
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
mod leica_lof_tests {
    use super::{LeicaLofReader, LEICA_LOF_STRICT_RAW_MAGIC};
    use crate::common::error::BioFormatsError;
    use crate::common::pixel_type::PixelType;
    use crate::common::reader::FormatReader;

    fn temp_path(name: &str) -> std::path::PathBuf {
        std::env::temp_dir().join(format!("bioformats_leica_lof_{name}"))
    }

    #[test]
    fn leica_lof_requires_marker_before_unsupported_payload() {
        let path = temp_path("marker.lof");
        std::fs::write(&path, b"\0\0LMS_Object_File\0payload").unwrap();

        let mut reader = LeicaLofReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Leica LOF native payload decoding is unsupported")),
            "{err:?}"
        );
        assert_eq!(reader.series_count(), 0);
        assert!(matches!(
            reader.set_series(0),
            Err(BioFormatsError::NotInitialized)
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn leica_lof_strict_raw_opens_planes_and_regions() {
        let path = temp_path("strict.lof");
        let plane0 = vec![1u8, 2, 3, 4, 5, 6];
        let plane1 = vec![11u8, 12, 13, 14, 15, 16];
        let mut payload = plane0.clone();
        payload.extend_from_slice(&plane1);
        let mut bytes = Vec::new();
        bytes.extend_from_slice(LEICA_LOF_STRICT_RAW_MAGIC);
        bytes.extend_from_slice(&3u32.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&2u32.to_le_bytes());
        bytes.extend_from_slice(&1u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&40u64.to_le_bytes());
        bytes.extend_from_slice(&payload);
        std::fs::write(&path, bytes).unwrap();

        let mut reader = LeicaLofReader::new();
        assert!(reader.is_this_type_by_bytes(LEICA_LOF_STRICT_RAW_MAGIC));
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_x, 3);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().size_t, 2);
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
        assert_eq!(reader.open_bytes(0).unwrap(), plane0);
        assert_eq!(reader.open_bytes(1).unwrap(), plane1);
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 2, 2).unwrap(),
            vec![12, 13, 15, 16]
        );
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn leica_lof_rejects_fake_payload_without_fake_metadata() {
        let path = temp_path("fake.lof");
        std::fs::write(&path, b"not a real image").unwrap();

        let mut reader = LeicaLofReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("missing LMS_Object_File marker")),
            "{err:?}"
        );
        assert_eq!(reader.series_count(), 0);

        let _ = std::fs::remove_file(path);
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
/// NAF format reader (`.naf`).
///
/// NAF is a proprietary format with undocumented structure. This reader only
/// accepts a strict synthetic raw subset with an explicit Bio-Formats Rust
/// marker; native proprietary payloads are rejected.
pub struct NafReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Vec<u8>,
    plane_bytes: usize,
}

impl NafReader {
    pub fn new() -> Self {
        NafReader {
            path: None,
            meta: None,
            pixel_data: Vec::new(),
            plane_bytes: 0,
        }
    }
}

impl Default for NafReader {
    fn default() -> Self {
        Self::new()
    }
}

const NAF_STRICT_RAW_MAGIC: &[u8; 16] = b"BFNAFSTRICTRAW01";
const BURLEIGH_STRICT_RAW_MAGIC: &[u8; 16] = b"BFBURLEIGHRAW001";
const EXTENDED_STRICT_RAW_HEADER_LEN: usize = 40;

#[derive(Clone)]
struct ExtendedStrictRaw {
    meta: ImageMetadata,
    payload: Vec<u8>,
    plane_bytes: usize,
}

fn extended_strict_raw_pixel_type(code: u16, label: &str) -> Result<(PixelType, u8)> {
    match code {
        1 => Ok((PixelType::Uint8, 8)),
        2 => Ok((PixelType::Uint16, 16)),
        3 => Ok((PixelType::Int16, 16)),
        4 => Ok((PixelType::Float32, 32)),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "{label} strict raw unsupported pixel type code {code}"
        ))),
    }
}

fn parse_extended_strict_raw(
    data: &[u8],
    magic: &[u8; 16],
    label: &str,
) -> Result<ExtendedStrictRaw> {
    if !data.starts_with(magic) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "{label} native payload decoding is unsupported"
        )));
    }
    if data.len() < EXTENDED_STRICT_RAW_HEADER_LEN {
        return Err(BioFormatsError::Format(format!(
            "{label} strict raw header is truncated"
        )));
    }

    let width = u32::from_le_bytes([data[16], data[17], data[18], data[19]]);
    let height = u32::from_le_bytes([data[20], data[21], data[22], data[23]]);
    let planes = u32::from_le_bytes([data[24], data[25], data[26], data[27]]);
    let pixel_type_code = u16::from_le_bytes([data[28], data[29]]);
    let reserved = u16::from_le_bytes([data[30], data[31]]);
    let data_offset = u64::from_le_bytes([
        data[32], data[33], data[34], data[35], data[36], data[37], data[38], data[39],
    ]);

    if width == 0 || height == 0 || planes == 0 {
        return Err(BioFormatsError::Format(format!(
            "{label} strict raw dimensions must be non-zero"
        )));
    }
    if reserved != 0 {
        return Err(BioFormatsError::Format(format!(
            "{label} strict raw reserved header bytes must be zero"
        )));
    }
    let data_offset = usize::try_from(data_offset).map_err(|_| {
        BioFormatsError::Format(format!("{label} strict raw data offset overflows"))
    })?;
    if data_offset < EXTENDED_STRICT_RAW_HEADER_LEN {
        return Err(BioFormatsError::Format(format!(
            "{label} strict raw data offset points into header"
        )));
    }
    if data_offset > data.len() {
        return Err(BioFormatsError::Format(format!(
            "{label} strict raw data offset points past end of file"
        )));
    }

    let (pixel_type, bits_per_pixel) = extended_strict_raw_pixel_type(pixel_type_code, label)?;
    let plane_bytes = (width as usize)
        .checked_mul(height as usize)
        .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample()))
        .ok_or_else(|| {
            BioFormatsError::Format(format!("{label} strict raw plane size overflows"))
        })?;
    let expected = plane_bytes.checked_mul(planes as usize).ok_or_else(|| {
        BioFormatsError::Format(format!("{label} strict raw payload size overflows"))
    })?;
    let available = data.len() - data_offset;
    if available != expected {
        return Err(BioFormatsError::Format(format!(
            "{label} strict raw payload length mismatch: got {available} bytes, expected {expected}"
        )));
    }

    let mut series_metadata = HashMap::new();
    series_metadata.insert(
        "format_subset".to_string(),
        MetadataValue::String("strict-synthetic-raw".to_string()),
    );

    Ok(ExtendedStrictRaw {
        meta: ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: planes,
            pixel_type,
            bits_per_pixel,
            image_count: planes,
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
        },
        payload: data[data_offset..].to_vec(),
        plane_bytes,
    })
}

impl FormatReader for NafReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("naf"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(NAF_STRICT_RAW_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let parsed = parse_extended_strict_raw(&data, NAF_STRICT_RAW_MAGIC, "NAF")?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(parsed.meta);
        self.pixel_data = parsed.payload;
        self.plane_bytes = parsed.plane_bytes;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_data.clear();
        self.plane_bytes = 0;
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
        let start = self
            .plane_bytes
            .checked_mul(plane_index as usize)
            .ok_or_else(|| {
                BioFormatsError::Format("NAF strict raw plane offset overflows".into())
            })?;
        let end = start
            .checked_add(self.plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("NAF strict raw plane end overflows".into()))?;
        Ok(self.pixel_data[start..end].to_vec())
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
/// Burleigh piezo/SPM reader (`.img`).
///
/// NOTE: `.img` is a very generic extension shared by many formats.
/// Burleigh SPM images have an undocumented proprietary structure.
/// This reader is a last-resort extension fallback.
pub struct BurleighReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Vec<u8>,
    plane_bytes: usize,
}

impl BurleighReader {
    pub fn new() -> Self {
        BurleighReader {
            path: None,
            meta: None,
            pixel_data: Vec::new(),
            plane_bytes: 0,
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
        header.starts_with(BURLEIGH_STRICT_RAW_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let parsed = parse_extended_strict_raw(&data, BURLEIGH_STRICT_RAW_MAGIC, "Burleigh SPM")?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(parsed.meta);
        self.pixel_data = parsed.payload;
        self.plane_bytes = parsed.plane_bytes;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_data.clear();
        self.plane_bytes = 0;
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
        let start = self
            .plane_bytes
            .checked_mul(plane_index as usize)
            .ok_or_else(|| {
                BioFormatsError::Format("Burleigh SPM strict raw plane offset overflows".into())
            })?;
        let end = start.checked_add(self.plane_bytes).ok_or_else(|| {
            BioFormatsError::Format("Burleigh SPM strict raw plane end overflows".into())
        })?;
        Ok(self.pixel_data[start..end].to_vec())
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
