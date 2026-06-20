//! Camera and RAW format readers — PCO, Bio-Rad GEL, Li-Cor L2D, and more.
//!
//! Includes three binary readers with partial metadata parsing (PcoRawReader,
//! BioRadGelReader, L2dReader) and several extension-only placeholder readers.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------
fn placeholder_meta_u16() -> ImageMetadata {
    ImageMetadata {
        size_x: 512,
        size_y: 512,
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
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    }
}

// ---------------------------------------------------------------------------
// Shared RAW / Bayer-CFA pixel helpers
//
// Faithful Rust port of the pixel path used by the Java RAW-camera readers:
//   * `loci.common.DataTools.unpackBytes(long, byte[], int, int, boolean)`
//   * `loci.formats.ImageTools.interpolate(short[], byte[], int[], int, int, boolean)`
//   * the MSB-first bit reader used by `RandomAccessInputStream.readBits`
//
// These are used by `CanonRawReader` (this file) and by the MRW and DNG CFA
// paths in `extended.rs`, which re-export them via `pub(crate)`.
// ---------------------------------------------------------------------------
pub(crate) mod cfa {
    /// Port of `loci.common.DataTools.unpackBytes`.
    ///
    /// Writes the low `nbytes` bytes of `value` into `buf` starting at `ndx`,
    /// little- or big-endian. Matches the Java implementation byte-for-byte:
    /// little-endian stores byte `i` as `(value >> (8*i)) & 0xff`; big-endian
    /// stores byte `i` as `(value >> (8*(nbytes-i-1))) & 0xff`.
    pub fn unpack_bytes(value: i64, buf: &mut [u8], ndx: usize, nbytes: usize, little: bool) {
        if little {
            for i in 0..nbytes {
                buf[ndx + i] = ((value >> (8 * i)) & 0xff) as u8;
            }
        } else {
            for i in 0..nbytes {
                buf[ndx + i] = ((value >> (8 * (nbytes - i - 1))) & 0xff) as u8;
            }
        }
    }

    /// MSB-first bit reader matching `RandomAccessInputStream.readBits` /
    /// `skipBits` used by the Java RAW readers (bits are consumed from the most
    /// significant bit of each successive byte).
    pub struct BitReader<'a> {
        data: &'a [u8],
        /// Absolute bit position into `data`.
        bit_pos: usize,
    }

    impl<'a> BitReader<'a> {
        pub fn new(data: &'a [u8]) -> Self {
            BitReader { data, bit_pos: 0 }
        }

        /// Read `n` bits MSB-first as an unsigned value. Reads past the end of
        /// the buffer yield zero bits, mirroring Java's behaviour of returning
        /// -1/EOF bits as 0 once exhausted (callers size buffers to the data).
        pub fn read_bits(&mut self, n: u32) -> u32 {
            let mut value: u32 = 0;
            for _ in 0..n {
                let byte_index = self.bit_pos >> 3;
                let bit_index = 7 - (self.bit_pos & 7);
                let bit = if byte_index < self.data.len() {
                    (self.data[byte_index] >> bit_index) & 1
                } else {
                    0
                };
                value = (value << 1) | bit as u32;
                self.bit_pos += 1;
            }
            value
        }

        /// Skip `n` bits (port of `skipBits`).
        pub fn skip_bits(&mut self, n: usize) {
            self.bit_pos += n;
        }
    }

    /// Port of `loci.formats.ImageTools.interpolate`.
    ///
    /// `s` is a planar short buffer of length `width*height*3` laid out as three
    /// stacked planes [R | G | B]. `buf` receives interleaved RGB samples
    /// (R,G,B per pixel, 2 bytes each, in the given byte order). `bayer_pattern`
    /// is a 4-element color map indexed by `(row%2)*2 + (col%2)` where the value
    /// names the channel present at that CFA position (0=R, 1=G, 2=B).
    ///
    /// This is NOT a fancy demosaic: it is exactly Java's nearest-neighbour
    /// average per missing component (the same algorithm Java DNG/MRW/Canon use).
    pub fn interpolate(
        s: &[i16],
        buf: &mut [u8],
        bayer_pattern: &[i32; 4],
        width: usize,
        height: usize,
        little_endian: bool,
    ) {
        if width == 1 && height == 1 {
            for b in buf.iter_mut() {
                *b = s[0] as u8;
            }
            return;
        }

        let plane = width * height;

        for row in 0..height {
            for col in 0..width {
                let even_col = (col % 2) == 0;

                let index = (row % 2) * 2 + (col % 2);
                let need_green = bayer_pattern[index] != 1;
                let need_red = bayer_pattern[index] != 0;
                let need_blue = bayer_pattern[index] != 2;

                // --- Green channel (buf offset +2) ---
                if need_green {
                    let mut sum: i32 = 0;
                    let mut ncomps = 0i32;
                    if row > 0 {
                        sum += s[plane + (row - 1) * width + col] as i32;
                        ncomps += 1;
                    }
                    if row < height - 1 {
                        sum += s[plane + (row + 1) * width + col] as i32;
                        ncomps += 1;
                    }
                    if col > 0 {
                        sum += s[plane + row * width + col - 1] as i32;
                        ncomps += 1;
                    }
                    if col < width - 1 {
                        sum += s[plane + row * width + col + 1] as i32;
                        ncomps += 1;
                    }
                    let v = (sum / ncomps) as i16;
                    unpack_bytes(
                        v as i64,
                        buf,
                        row * width * 6 + col * 6 + 2,
                        2,
                        little_endian,
                    );
                } else {
                    unpack_bytes(
                        s[plane + row * width + col] as i64,
                        buf,
                        row * width * 6 + col * 6 + 2,
                        2,
                        little_endian,
                    );
                }

                // --- Red channel (buf offset +0) ---
                if need_red {
                    let mut sum: i32 = 0;
                    let mut ncomps = 0i32;
                    if !need_blue {
                        // four corners
                        if row > 0 {
                            if col > 0 {
                                sum += s[(row - 1) * width + col - 1] as i32;
                                ncomps += 1;
                            }
                            if col < width - 1 {
                                sum += s[(row - 1) * width + col + 1] as i32;
                                ncomps += 1;
                            }
                        }
                        if row < height - 1 {
                            if col > 0 {
                                sum += s[(row + 1) * width + col - 1] as i32;
                                ncomps += 1;
                            }
                            if col < width - 1 {
                                sum += s[(row + 1) * width + col + 1] as i32;
                                ncomps += 1;
                            }
                        }
                    } else if (even_col && bayer_pattern[index + 1] == 0)
                        || (!even_col && bayer_pattern[index - 1] == 0)
                    {
                        // horizontal
                        if col > 0 {
                            sum += s[row * width + col - 1] as i32;
                            ncomps += 1;
                        }
                        if col < width - 1 {
                            sum += s[row * width + col + 1] as i32;
                            ncomps += 1;
                        }
                    } else {
                        // vertical
                        if row > 0 {
                            sum += s[(row - 1) * width + col] as i32;
                            ncomps += 1;
                        }
                        if row < height - 1 {
                            sum += s[(row + 1) * width + col] as i32;
                            ncomps += 1;
                        }
                    }
                    let v = (sum / ncomps) as i16;
                    unpack_bytes(v as i64, buf, row * width * 6 + col * 6, 2, little_endian);
                } else {
                    unpack_bytes(
                        s[row * width + col] as i64,
                        buf,
                        row * width * 6 + col * 6,
                        2,
                        little_endian,
                    );
                }

                // --- Blue channel (buf offset +4) ---
                if need_blue {
                    let mut sum: i32 = 0;
                    let mut ncomps = 0i32;
                    if !need_red {
                        // four corners
                        if row > 0 {
                            if col > 0 {
                                sum += s[(2 * height + row - 1) * width + col - 1] as i32;
                                ncomps += 1;
                            }
                            if col < width - 1 {
                                sum += s[(2 * height + row - 1) * width + col + 1] as i32;
                                ncomps += 1;
                            }
                        }
                        if row < height - 1 {
                            if col > 0 {
                                sum += s[(2 * height + row + 1) * width + col - 1] as i32;
                                ncomps += 1;
                            }
                            if col < width - 1 {
                                sum += s[(2 * height + row + 1) * width + col + 1] as i32;
                                ncomps += 1;
                            }
                        }
                    } else if (even_col && bayer_pattern[index + 1] == 2)
                        || (!even_col && bayer_pattern[index - 1] == 2)
                    {
                        // horizontal
                        if col > 0 {
                            sum += s[(2 * height + row) * width + col - 1] as i32;
                            ncomps += 1;
                        }
                        if col < width - 1 {
                            sum += s[(2 * height + row) * width + col + 1] as i32;
                            ncomps += 1;
                        }
                    } else {
                        // vertical
                        if row > 0 {
                            sum += s[(2 * height + row - 1) * width + col] as i32;
                            ncomps += 1;
                        }
                        if row < height - 1 {
                            sum += s[(2 * height + row + 1) * width + col] as i32;
                            ncomps += 1;
                        }
                    }
                    let v = (sum / ncomps) as i16;
                    unpack_bytes(
                        v as i64,
                        buf,
                        row * width * 6 + col * 6 + 4,
                        2,
                        little_endian,
                    );
                } else {
                    unpack_bytes(
                        s[2 * plane + row * width + col] as i64,
                        buf,
                        row * width * 6 + col * 6 + 4,
                        2,
                        little_endian,
                    );
                }
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Macro for TIFF wrapper readers
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
// 1. PCO-RAW camera file
// ---------------------------------------------------------------------------
/// PCO-RAW camera format (`.pcoraw`) with optional `.rec` companion metadata.
///
/// Java `PCORAWReader` delegates pixel I/O to `TiffReader`; the `.pcoraw`
/// image file is TIFF-encoded, and a similarly named `.rec` file contributes
/// native key/value metadata.
pub struct PcoRawReader {
    inner: crate::tiff::TiffReader,
    image_file: Option<PathBuf>,
    param_file: Option<PathBuf>,
}

impl PcoRawReader {
    pub fn new() -> Self {
        PcoRawReader {
            inner: crate::tiff::TiffReader::new(),
            image_file: None,
            param_file: None,
        }
    }

    fn companion_path(path: &Path, extension: &str) -> PathBuf {
        path.with_extension(extension)
    }

    fn is_rec(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("rec"))
    }

    fn is_pcoraw(path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("pcoraw"))
    }

    fn parse_rec_metadata(path: &Path) -> Result<HashMap<String, MetadataValue>> {
        let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let mut values = HashMap::new();
        for line in text.lines() {
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            values.insert(
                key.trim().to_owned(),
                MetadataValue::String(value.trim().to_owned()),
            );
        }
        Ok(values)
    }
}

impl Default for PcoRawReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PcoRawReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        Self::is_pcoraw(path) || Self::is_rec(path)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.inner.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;

        let (image_file, param_file) = if Self::is_rec(path) {
            let image = Self::companion_path(path, "pcoraw");
            if !image.exists() {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Could not find PCO-RAW image file.".into(),
                ));
            }
            (image, Some(path.to_path_buf()))
        } else {
            let param = Self::companion_path(path, "rec");
            let param = if param.exists() { Some(param) } else { None };
            (path.to_path_buf(), param)
        };

        self.inner.set_id(&image_file)?;

        if let Some(param) = &param_file {
            let metadata = Self::parse_rec_metadata(param)?;
            for series in self.inner.series_list_mut() {
                for (key, value) in &metadata {
                    series
                        .metadata
                        .series_metadata
                        .insert(key.clone(), value.clone());
                }
            }
        }

        self.image_file = Some(image_file);
        self.param_file = param_file;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()?;
        self.image_file = None;
        self.param_file = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.inner.series_count()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.image_file.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(plane_index)
    }

    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }

    fn resolution(&self) -> usize {
        self.inner.resolution()
    }

    fn lookup_table(
        &mut self,
        plane_index: u32,
    ) -> Result<Option<crate::common::metadata::LookupTable>> {
        self.inner.lookup_table(plane_index)
    }

    fn set_metadata_options(&mut self, options: crate::common::metadata::MetadataOptions) {
        self.inner.set_metadata_options(options);
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        self.inner.ome_metadata()
    }
}

// ---------------------------------------------------------------------------
// Legacy PCO B16 support
// ---------------------------------------------------------------------------
/// PCO camera raw B16 binary format (`.b16`).
///
/// Header is 216 bytes; width at offset 4 (u16 LE), height at offset 6 (u16 LE).
/// Pixel data starts at offset 216 as 16-bit little-endian grayscale values.
pub struct PcoB16Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl PcoB16Reader {
    pub fn new() -> Self {
        PcoB16Reader {
            path: None,
            meta: None,
        }
    }
}

impl Default for PcoB16Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PcoB16Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("b16")
        )
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let file_size = f.metadata().map_err(BioFormatsError::Io)?.len();
        let mut header = [0u8; 216];
        let n = f.read(&mut header).map_err(BioFormatsError::Io)?;
        let (w, h) = if n >= 8 {
            let w = u16::from_le_bytes([header[4], header[5]]) as u32;
            let h = u16::from_le_bytes([header[6], header[7]]) as u32;
            if w == 0 || h == 0 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "PCO B16 header contains zero image dimensions".into(),
                ));
            } else {
                (w, h)
            }
        } else {
            return Err(BioFormatsError::UnsupportedFormat(
                "PCO B16 header is too short to contain dimensions".into(),
            ));
        };
        let expected = (w as u64)
            .checked_mul(h as u64)
            .and_then(|pixels| pixels.checked_mul(2))
            .and_then(|bytes| bytes.checked_add(216))
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat("PCO B16 declared dimensions overflow".into())
            })?;
        if file_size < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "PCO B16 file is too short for declared dimensions {w}x{h}"
            )));
        }
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: w,
            size_y: h,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            is_little_endian: true,
            ..placeholder_meta_u16()
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
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
        let n_bytes = meta.size_x as usize * meta.size_y as usize * 2;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(|e| BioFormatsError::Io(e))?;
        f.seek(SeekFrom::Start(216))
            .map_err(|e| BioFormatsError::Io(e))?;
        let mut buf = vec![0u8; n_bytes];
        f.read_exact(&mut buf).map_err(|e| BioFormatsError::Io(e))?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("PCO B16", &full, meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 2. Bio-Rad GEL phosphor imager (.1sc)
// ---------------------------------------------------------------------------
/// Bio-Rad GEL phosphor imager format (`.1sc`).
///
/// Port of BioRadGelReader.java: magic 0xafaf, chunk-walks from offsets
/// START_OFFSET (160) / BASE_OFFSET (352), reads bpp (2 or 4 bytes), and a
/// dynamic pixel offset relative to PIXEL_OFFSET (59654).
pub struct BioRadGelReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Whether the on-disk values are little-endian ("Intel Format").
    little_endian: bool,
    /// Java `diff = BASE_OFFSET - baseFP`, used to pick the pixel offset.
    diff: i64,
}

const BRG_MAGIC: u16 = 0xafaf;
const BRG_PIXEL_OFFSET: u64 = 59654;
const BRG_START_OFFSET: u64 = 160;
const BRG_BASE_OFFSET: i64 = 352;

impl BioRadGelReader {
    pub fn new() -> Self {
        BioRadGelReader {
            path: None,
            meta: None,
            little_endian: false,
            diff: 0,
        }
    }

    /// Compute the seek position for the pixel data, mirroring openBytes() in
    /// BioRadGelReader.java. Returns None when no special offset applies and the
    /// caller should fall back to (file_len - plane_size).
    fn pixel_seek(&self, f: &mut std::fs::File, plane_size: u64, file_len: u64) -> Result<u64> {
        if BRG_PIXEL_OFFSET + plane_size < file_len {
            if self.diff < 0 {
                let mut pos = 0x379d1u64;
                if pos + plane_size > file_len {
                    pos = BRG_PIXEL_OFFSET + 62;
                }
                Ok(pos)
            } else if self.diff == 0 {
                Ok(BRG_PIXEL_OFFSET)
            } else if file_len - plane_size > 61000 {
                // Scan backwards for the "scn0x" marker starting near
                // PIXEL_OFFSET - 196, then skip a variable metadata block.
                let mut pos = BRG_PIXEL_OFFSET - 196;
                loop {
                    f.seek(SeekFrom::Start(pos)).map_err(BioFormatsError::Io)?;
                    let mut s = [0u8; 5];
                    f.read_exact(&mut s).map_err(BioFormatsError::Io)?;
                    if &s == b"scn0x" {
                        break;
                    }
                    // back up 4 from the post-read position (== pos + 5 - 4)
                    pos = (pos + 5) - 4;
                }
                let mut p = pos + 5; // after reading "scn0x"
                p += 69;
                f.seek(SeekFrom::Start(p)).map_err(BioFormatsError::Io)?;
                let mut check = [0u8; 1];
                f.read_exact(&mut check).map_err(BioFormatsError::Io)?;
                p += 1;
                p += 19;
                f.seek(SeekFrom::Start(p)).map_err(BioFormatsError::Io)?;
                if check[0] != 0 {
                    let extra = read_i16(f, self.little_endian)? as i64 - 2;
                    p += 2;
                    p += extra.max(0) as u64;
                    f.seek(SeekFrom::Start(p)).map_err(BioFormatsError::Io)?;
                }
                let len = read_i16(f, self.little_endian)? as i64;
                p += 2;
                p += len.max(0) as u64;
                p += 32;
                Ok(p)
            } else {
                Ok(file_len - plane_size)
            }
        } else {
            Ok(file_len - plane_size)
        }
    }
}

fn read_i16(f: &mut std::fs::File, little_endian: bool) -> Result<i16> {
    let mut b = [0u8; 2];
    f.read_exact(&mut b).map_err(BioFormatsError::Io)?;
    Ok(if little_endian {
        i16::from_le_bytes(b)
    } else {
        i16::from_be_bytes(b)
    })
}

fn read_i32(f: &mut std::fs::File, little_endian: bool) -> Result<i32> {
    let mut b = [0u8; 4];
    f.read_exact(&mut b).map_err(BioFormatsError::Io)?;
    Ok(if little_endian {
        i32::from_le_bytes(b)
    } else {
        i32::from_be_bytes(b)
    })
}

fn read_c_string(f: &mut std::fs::File, max_len: usize) -> Result<String> {
    let mut bytes = Vec::new();
    for _ in 0..max_len {
        let mut b = [0u8; 1];
        let n = f.read(&mut b).map_err(BioFormatsError::Io)?;
        if n == 0 || b[0] == 0 {
            break;
        }
        bytes.push(b[0]);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

impl Default for BioRadGelReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for BioRadGelReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("1sc")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Magic: first big-endian short == 0xafaf.
        header.len() >= 2 && u16::from_be_bytes([header[0], header[1]]) == BRG_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let file_size = f.metadata().map_err(BioFormatsError::Io)?.len();

        // Reject files too small to hold the 48-byte header and the metadata
        // chunk table that begins at START_OFFSET, instead of leaking an Io EOF.
        if file_size < BRG_START_OFFSET + 4 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Bio-Rad GEL file is too short".into(),
            ));
        }

        // Header begins with a 48-byte string; "Intel Format" => little-endian.
        let mut head48 = [0u8; 48];
        f.read_exact(&mut head48).map_err(BioFormatsError::Io)?;
        let check = String::from_utf8_lossy(&head48);
        let mut little_endian = check.contains("Intel Format");

        // Walk metadata chunks from START_OFFSET until code 0x81 is found.
        f.seek(SeekFrom::Start(BRG_START_OFFSET))
            .map_err(BioFormatsError::Io)?;
        let mut code_found = false;
        let mut skip: i64 = 0;
        let mut base_fp: i64 = 0;
        // Guard against runaway loops on malformed input.
        let mut iterations = 0u32;
        while !code_found {
            iterations += 1;
            if iterations > 100_000 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Bio-Rad GEL: chunk walk did not find code 0x81".into(),
                ));
            }
            let code = read_i16(&mut f, little_endian)?;
            if code == 0x81 {
                code_found = true;
            }
            let length = read_i16(&mut f, little_endian)?;

            f.seek(SeekFrom::Current(2 + 2 * length as i64))
                .map_err(BioFormatsError::Io)?;
            if code_found {
                let fp = f.stream_position().map_err(BioFormatsError::Io)? as i64;
                base_fp = fp + 2;
                if length > 1 {
                    f.seek(SeekFrom::Current(-2)).map_err(BioFormatsError::Io)?;
                }
                skip = read_i32(&mut f, little_endian)? as i64 - 32;
            } else if length == 1 {
                f.seek(SeekFrom::Current(12)).map_err(BioFormatsError::Io)?;
            } else if length == 2 {
                f.seek(SeekFrom::Current(10)).map_err(BioFormatsError::Io)?;
            }
        }

        self.diff = BRG_BASE_OFFSET - base_fp;
        skip += self.diff;

        let metadata_anchor = base_fp + skip;
        let mut series_metadata = HashMap::new();
        if metadata_anchor >= 298 && (metadata_anchor - 298) as u64 + 90 < file_size {
            f.seek(SeekFrom::Start((metadata_anchor - 298) as u64))
                .map_err(BioFormatsError::Io)?;
            let mut date = [0u8; 17];
            f.read_exact(&mut date).map_err(BioFormatsError::Io)?;
            let date = String::from_utf8_lossy(&date)
                .trim_matches('\0')
                .trim()
                .to_owned();
            if !date.is_empty() {
                series_metadata.insert("Acquisition date".into(), MetadataValue::String(date));
            }
            f.seek(SeekFrom::Current(73)).map_err(BioFormatsError::Io)?;
            let scanner_name = read_c_string(&mut f, 4096)?;
            series_metadata.insert("Scanner name".into(), MetadataValue::String(scanner_name));
        }

        // Seek to baseFP + skip and read dimensions + bpp.
        let dims_pos = metadata_anchor.max(0) as u64;
        f.seek(SeekFrom::Start(dims_pos))
            .map_err(BioFormatsError::Io)?;

        let mut size_x = (read_i16(&mut f, little_endian)? as u16) as u32;
        let mut size_y = (read_i16(&mut f, little_endian)? as u16) as u32;
        if (size_x as u64) * (size_y as u64) > file_size {
            // Retry as little-endian, re-reading the two shorts.
            little_endian = true;
            f.seek(SeekFrom::Current(-4)).map_err(BioFormatsError::Io)?;
            size_x = read_i16(&mut f, little_endian)? as u32;
            size_y = read_i16(&mut f, little_endian)? as u32;
        }
        f.seek(SeekFrom::Current(2)).map_err(BioFormatsError::Io)?; // skip 2

        let bpp = read_i16(&mut f, little_endian)?;
        // pixelTypeFromBytes(bpp, signed=false, fp=false): 2 -> Uint16, 4 -> Uint32.
        // Java uses fp=false here; 4-byte support is FLOAT per the GEL spec, but
        // the reader declares an integer type. Follow Java: unsigned integer.
        let (pixel_type, bits) = match bpp {
            2 => (PixelType::Uint16, 16u8),
            4 => (PixelType::Uint32, 32u8),
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Bio-Rad GEL: unsupported bytes per pixel {bpp}"
                )))
            }
        };

        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Bio-Rad GEL: invalid image dimensions".into(),
            ));
        }
        self.little_endian = little_endian;
        let plane_size = (size_x as u64)
            .checked_mul(size_y as u64)
            .and_then(|pixels| pixels.checked_mul(pixel_type.bytes_per_sample() as u64))
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "Bio-Rad GEL: declared image is too large".into(),
                )
            })?;
        let pixel_offset = self.pixel_seek(&mut f, plane_size, file_size)?;
        if pixel_offset
            .checked_add(plane_size)
            .is_none_or(|end| end > file_size)
        {
            return Err(BioFormatsError::UnsupportedFormat(
                "Bio-Rad GEL: file is too short for declared pixel payload".into(),
            ));
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            pixel_type,
            bits_per_pixel: bits,
            dimension_order: DimensionOrder::XYCZT,
            is_little_endian: little_endian,
            series_metadata,
            ..placeholder_meta_u16()
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
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
        let bpp = meta.pixel_type.bytes_per_sample();
        let pixel = bpp * meta.size_c as usize;
        let w = meta.size_x as usize;
        let h = meta.size_y as usize;
        let plane_size = (pixel * w * h) as u64;

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();

        let seek_pos = self.pixel_seek(&mut f, plane_size, file_len)?;
        f.seek(SeekFrom::Start(seek_pos))
            .map_err(BioFormatsError::Io)?;

        // Java reads rows bottom-to-top into the destination buffer, which flips
        // the image vertically relative to disk order.
        let row_bytes = w * pixel;
        let mut buf = vec![0u8; h * row_bytes];
        for row in (0..h).rev() {
            f.read_exact(&mut buf[row * row_bytes..(row + 1) * row_bytes])
                .map_err(BioFormatsError::Io)?;
        }
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let spp = meta.size_c as usize;
        crop_full_plane("Bio-Rad GEL", &full, meta, spp, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 3. Li-Cor L2D companion-file reader
// ---------------------------------------------------------------------------
/// Li-Cor L2D format (`.l2d`).
///
/// Java Bio-Formats stores L2D pixels in companion TIFF files listed by the
/// `.l2d` scan manifest and each scan's `.scn` metadata file.
pub struct L2dReader {
    current_id: Option<PathBuf>,
    tiffs: Vec<Vec<PathBuf>>,
    metadata: Vec<ImageMetadata>,
    current_series: usize,
    reader: crate::tiff::TiffReader,
}

impl L2dReader {
    const LICOR_MAGIC: &'static str = "LI-COR LI2D";

    pub fn new() -> Self {
        L2dReader {
            current_id: None,
            tiffs: Vec::new(),
            metadata: Vec::new(),
            current_series: 0,
            reader: crate::tiff::TiffReader::new(),
        }
    }

    fn parse_key_value_lines(text: &str) -> HashMap<String, String> {
        text.lines()
            .filter_map(|line| {
                let line = line.trim();
                if line.is_empty() || line.starts_with('#') {
                    return None;
                }
                let (key, value) = line.split_once('=')?;
                Some((key.trim().to_string(), value.trim().to_string()))
            })
            .collect()
    }

    fn split_list(value: &str) -> Vec<String> {
        value
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect()
    }

    fn find_group_manifest(path: &Path) -> Result<PathBuf> {
        if path
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("l2d"))
        {
            return Ok(path.to_path_buf());
        }

        let scan_dir = path
            .parent()
            .ok_or_else(|| BioFormatsError::Format("Li-Cor L2D path has no parent".into()))?;
        let root = scan_dir.parent().ok_or_else(|| {
            BioFormatsError::Format("Li-Cor L2D companion path has no dataset root".into())
        })?;
        for entry in std::fs::read_dir(root).map_err(BioFormatsError::Io)? {
            let entry = entry.map_err(BioFormatsError::Io)?;
            let candidate = entry.path();
            if candidate
                .extension()
                .and_then(|e| e.to_str())
                .is_some_and(|e| e.eq_ignore_ascii_case("l2d"))
            {
                return Ok(candidate);
            }
        }
        Err(BioFormatsError::Format("Could not find .l2d file".into()))
    }

    fn set_l2d_id(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        if !text.contains(Self::LICOR_MAGIC) {
            return Err(BioFormatsError::UnsupportedFormat(
                "Li-Cor L2D file is missing LI-COR LI2D marker".into(),
            ));
        }

        let l2d = Self::parse_key_value_lines(&text);
        let scans = l2d
            .get("ScanNames")
            .map(|v| Self::split_list(v))
            .ok_or_else(|| BioFormatsError::Format("Li-Cor L2D missing ScanNames".into()))?;
        if scans.is_empty() {
            return Err(BioFormatsError::Format(
                "Li-Cor L2D ScanNames list is empty".into(),
            ));
        }

        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut tiffs = Vec::new();
        let mut metadata = Vec::new();

        for scan in scans {
            let scan_dir = parent.join(&scan);
            if !scan_dir.is_dir() {
                continue;
            }
            let scan_path = scan_dir.join(format!("{scan}.scn"));
            let scan_text = std::fs::read_to_string(&scan_path).map_err(BioFormatsError::Io)?;
            let scan_meta = Self::parse_key_value_lines(&scan_text);
            let image_names = scan_meta
                .get("ImageNames")
                .map(|v| Self::split_list(v))
                .ok_or_else(|| {
                    BioFormatsError::Format(format!("Li-Cor L2D scan {scan} missing ImageNames"))
                })?;
            if image_names.is_empty() {
                return Err(BioFormatsError::Format(format!(
                    "Li-Cor L2D scan {scan} ImageNames list is empty"
                )));
            }

            let scan_tiffs: Vec<PathBuf> = image_names
                .into_iter()
                .map(|name| scan_dir.join(name))
                .collect();
            for tiff in &scan_tiffs {
                if !tiff.is_file() {
                    return Err(BioFormatsError::Format(format!(
                        "Li-Cor L2D companion TIFF is missing: {}",
                        tiff.display()
                    )));
                }
            }

            self.reader.set_id(&scan_tiffs[0])?;
            let first = self.reader.metadata().clone();
            self.reader.close()?;

            let mut series_meta = first;
            series_meta.image_count = scan_tiffs.len() as u32;
            series_meta.size_z = 1;
            series_meta.size_t = 1;
            series_meta.size_c =
                (scan_tiffs.len() as u32).saturating_mul(series_meta.size_c.max(1));
            series_meta.dimension_order = DimensionOrder::XYCZT;
            series_meta.series_metadata = scan_meta
                .into_iter()
                .map(|(k, v)| (k, crate::common::metadata::MetadataValue::String(v)))
                .collect();
            tiffs.push(scan_tiffs);
            metadata.push(series_meta);
        }

        if tiffs.is_empty() {
            return Err(BioFormatsError::Format(
                "Li-Cor L2D did not reference any existing scan directories".into(),
            ));
        }

        self.current_id = Some(path.to_path_buf());
        self.tiffs = tiffs;
        self.metadata = metadata;
        self.current_series = 0;
        Ok(())
    }
}

impl Default for L2dReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for L2dReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("l2d") | Some("scn")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        std::str::from_utf8(&header[..header.len().min(512)])
            .map(|s| s.contains(Self::LICOR_MAGIC))
            .unwrap_or(false)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let l2d_path = Self::find_group_manifest(path)?;
        self.set_l2d_id(&l2d_path)
    }

    fn close(&mut self) -> Result<()> {
        self.current_id = None;
        self.tiffs.clear();
        self.metadata.clear();
        self.current_series = 0;
        self.reader.close()?;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.metadata.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.metadata.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s >= self.metadata.len() {
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
        self.metadata
            .get(self.current_series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metadata
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let tiff = self
            .tiffs
            .get(self.current_series)
            .and_then(|series| series.get(plane_index as usize))
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?
            .clone();
        self.reader.set_id(&tiff)?;
        let bytes = self.reader.open_bytes(0);
        self.reader.close()?;
        bytes
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
            .metadata
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let tiff = self
            .tiffs
            .get(self.current_series)
            .and_then(|series| series.get(plane_index as usize))
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?
            .clone();
        self.reader.set_id(&tiff)?;
        let bytes = self.reader.open_bytes_region(0, x, y, w, h);
        self.reader.close()?;
        bytes
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metadata
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 4. Canon RAW (CR2 / CRW / CR3) — TIFF wrapper
// ---------------------------------------------------------------------------
/// Canon RAW format reader (`.cr2`, `.crw`, `.cr3`).
///
/// Two code paths, mirroring Java Bio-Formats:
///
/// * **Legacy CRW** — `CanonRawReader.java` recognises raw Canon 300D `.crw`
///   files solely by a fixed file length of 18 653 760 bytes. Those have no
///   TIFF structure: bytes are byte-swapped in pairs, 12-bit samples are
///   unpacked, and the Bayer mosaic is split into an interleaved RGB plane
///   (`COLOR_MAP = {1,0,2,1}`, sizeX=4080, sizeY=3048, UINT16, 12 bpp). This
///   reader reproduces that unpacking exactly (the `ImageTools.interpolate`
///   demosaic in Java is a simple channel split, not full demosaicing).
/// * **TIFF-based** — modern CR2 files are valid TIFFs; delegate to
///   `TiffReader`.
pub struct CanonRawReader {
    inner: crate::tiff::TiffReader,
    /// Set when the file matched the legacy fixed-length CRW layout.
    legacy: Option<LegacyCrw>,
}

/// State for a legacy fixed-length Canon `.crw` file.
struct LegacyCrw {
    path: PathBuf,
    meta: ImageMetadata,
    /// Decoded interleaved RGB plane (UINT16 LE, 3 samples/pixel), cached.
    plane: Option<Vec<u8>>,
}

impl CanonRawReader {
    /// Fixed file length used by `CanonRawReader.java` to detect legacy CRW.
    const FILE_LENGTH: u64 = 18_653_760;
    const SIZE_X: usize = 4080;
    const SIZE_Y: usize = 3048;
    /// Bayer color map: index = (row%2)*2 + (col%2) -> 0=R, 1=G, 2=B.
    const COLOR_MAP: [u8; 4] = [1, 0, 2, 1];

    pub fn new() -> Self {
        CanonRawReader {
            inner: crate::tiff::TiffReader::new(),
            legacy: None,
        }
    }

    /// Decode the legacy CRW interleaved RGB plane (port of initFile + the
    /// channel split in openBytes from `CanonRawReader.java`).
    fn decode_legacy_plane(path: &Path) -> Result<Vec<u8>> {
        let mut buf = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if buf.len() < Self::FILE_LENGTH as usize {
            return Err(BioFormatsError::UnsupportedFormat(
                "Canon CRW: file shorter than expected fixed length".into(),
            ));
        }
        buf.truncate(Self::FILE_LENGTH as usize);

        // Reverse bytes in pairs.
        let mut i = 0;
        while i + 1 < buf.len() {
            buf.swap(i, i + 1);
            i += 2;
        }

        let w = Self::SIZE_X;
        let h = Self::SIZE_Y;
        let plane = w * h;
        // pix layout: 3 planar channels [R | G | B], each w*h shorts.
        let mut pix = vec![0i16; plane * 3];

        let mut next_byte = 0usize;
        let mut even = true;
        for row in 0..h {
            let row_offset = row * w;
            for col in 0..w {
                let v: u32 = if even {
                    let a = buf[next_byte] as u32;
                    next_byte += 1;
                    let b = buf[next_byte] as u32;
                    (a << 4) | ((b & 0xf0) >> 4)
                } else {
                    let a = buf[next_byte] as u32;
                    next_byte += 1;
                    let b = buf[next_byte] as u32;
                    next_byte += 1;
                    ((a & 0xf) << 8) | b
                };
                let val = (v & 0xffff) as u16 as i16;
                even = !even;

                let map_index = (row % 2) * 2 + (col % 2);
                match Self::COLOR_MAP[map_index] {
                    0 => pix[row_offset + col] = val,
                    1 => pix[plane + row_offset + col] = val,
                    2 => pix[2 * plane + row_offset + col] = val,
                    _ => {}
                }
            }
        }

        // Java: ImageTools.interpolate(pix, plane, COLOR_MAP, ...) fills in the
        // missing CFA components, then readPlane delivers interleaved RGB.
        // littleEndian = true (m.littleEndian set in initFile).
        let color_map = Self::COLOR_MAP.map(|c| c as i32);
        let mut out = vec![0u8; plane * 3 * 2];
        cfa::interpolate(&pix, &mut out, &color_map, w, h, true);
        Ok(out)
    }
}

impl Default for CanonRawReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for CanonRawReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(
            ext.as_deref(),
            Some("cr2") | Some("crw") | Some("jpg") | Some("thm") | Some("wav") | Some("cr3")
        )
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        // Legacy detection: exact fixed file length (CanonRawReader.java).
        let len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        if len == Self::FILE_LENGTH {
            let mut meta = placeholder_meta_u16();
            meta.size_x = Self::SIZE_X as u32;
            meta.size_y = Self::SIZE_Y as u32;
            meta.size_c = 3;
            meta.pixel_type = PixelType::Uint16;
            meta.bits_per_pixel = 12;
            meta.image_count = 1;
            meta.is_rgb = true;
            meta.is_interleaved = true;
            meta.dimension_order = DimensionOrder::XYCZT;
            self.legacy = Some(LegacyCrw {
                path: path.to_path_buf(),
                meta,
                plane: None,
            });
            return Ok(());
        }
        self.legacy = None;
        self.inner.set_id(path)
    }

    fn close(&mut self) -> Result<()> {
        self.legacy = None;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        if self.legacy.is_some() {
            1
        } else {
            self.inner.series_count()
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.legacy.is_some() {
            if s != 0 {
                return Err(BioFormatsError::SeriesOutOfRange(s));
            }
            Ok(())
        } else if self.inner.series_count() == 0 {
            Err(BioFormatsError::NotInitialized)
        } else {
            self.inner.set_series(s)
        }
    }

    fn series(&self) -> usize {
        if self.legacy.is_some() {
            0
        } else {
            self.inner.series()
        }
    }

    fn metadata(&self) -> &ImageMetadata {
        if let Some(l) = &self.legacy {
            &l.meta
        } else {
            self.inner.metadata()
        }
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if let Some(l) = &mut self.legacy {
            if p != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(p));
            }
            if l.plane.is_none() {
                l.plane = Some(Self::decode_legacy_plane(&l.path)?);
            }
            return Ok(l.plane.clone().unwrap());
        }
        self.inner.open_bytes(p)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.legacy.is_some() {
            let full = self.open_bytes(p)?;
            let meta = self.metadata().clone();
            return crop_full_plane("Canon CRW", &full, &meta, 3, x, y, w, h);
        }
        self.inner.open_bytes_region(p, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if self.legacy.is_some() {
            let meta = self.metadata().clone();
            let tw = meta.size_x.min(256);
            let th = meta.size_y.min(256);
            let tx = (meta.size_x - tw) / 2;
            let ty = (meta.size_y - th) / 2;
            return self.open_bytes_region(p, tx, ty, tw, th);
        }
        self.inner.open_thumb_bytes(p)
    }

    fn resolution_count(&self) -> usize {
        if self.legacy.is_some() {
            1
        } else {
            self.inner.resolution_count()
        }
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.legacy.is_some() {
            if level != 0 {
                return Err(BioFormatsError::Format(format!(
                    "resolution {} out of range",
                    level
                )));
            }
            Ok(())
        } else {
            self.inner.set_resolution(level)
        }
    }
}

// ---------------------------------------------------------------------------
// 5. Hasselblad Imacon — TIFF with private tags
// ---------------------------------------------------------------------------
/// Hasselblad Imacon format reader (`.fff`).
///
/// Ported from `ImaconReader.java` (extends `BaseTiffReader`). Imacon `.fff`
/// files are TIFFs identified by private tag 50457 (`XML_TAG`); each main IFD
/// is a separate series. The CREATOR tag (34377) carries experimenter/name/date
/// lines. Pixel reading is delegated to `TiffReader`; this reader adds the
/// tag-based detection and metadata parsing.
pub struct ImaconReader {
    inner: crate::tiff::TiffReader,
    meta: Vec<ImageMetadata>,
}

impl ImaconReader {
    const XML_TAG: u16 = 50457;
    const CREATOR_TAG: u16 = 34377;

    pub fn new() -> Self {
        ImaconReader {
            inner: crate::tiff::TiffReader::new(),
            meta: Vec::new(),
        }
    }
}

fn imacon_add_xml_metadata(xml_text: &str, series_metadata: &mut HashMap<String, MetadataValue>) {
    let Some(xml_start) = xml_text.find('<') else {
        return;
    };
    let xml = xml_text[xml_start..].trim();
    let mut reader = quick_xml::Reader::from_str(xml);
    reader.config_mut().trim_text(true);
    let mut current_element = String::new();
    let mut key: Option<String> = None;

    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(e)) => {
                current_element = String::from_utf8_lossy(e.name().as_ref()).into_owned();
            }
            Ok(quick_xml::events::Event::Text(t)) => {
                let Some(value) = crate::common::xml::decode_xml_text(&t) else {
                    continue;
                };
                if current_element == "key" {
                    key = Some(value);
                } else if let Some(k) = key.take() {
                    series_metadata.insert(k, MetadataValue::String(value));
                }
            }
            Ok(quick_xml::events::Event::GeneralRef(r)) => {
                let Some(value) = crate::common::xml::decode_xml_ref(&r) else {
                    continue;
                };
                if current_element == "key" {
                    key = Some(value);
                } else if let Some(k) = key.take() {
                    series_metadata.insert(k, MetadataValue::String(value));
                }
            }
            Ok(quick_xml::events::Event::End(_)) => {
                current_element.clear();
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Ok(_) => {}
            Err(_) => break,
        }
    }
}

impl Default for ImaconReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ImaconReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("fff")
        )
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Java requires the XML_TAG in the first IFD; bytes alone insufficient.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        self.inner.set_id(path)?;

        let first = self
            .inner
            .ifd(0)
            .ok_or_else(|| BioFormatsError::UnsupportedFormat("Imacon: no IFD".into()))?;
        if first.get(Self::XML_TAG).is_none() {
            let _ = self.inner.close();
            return Err(BioFormatsError::UnsupportedFormat(
                "Imacon: TIFF is missing the XML tag (50457)".into(),
            ));
        }

        for i in 0..self.inner.ifd_count() {
            if let Some(ifd) = self.inner.ifd_mut(i) {
                ifd.entries.remove(&46275);
            }
        }
        self.inner.split_ifds_into_single_ifd_series_xyczt()?;

        let first = self
            .inner
            .ifd(0)
            .ok_or_else(|| BioFormatsError::UnsupportedFormat("Imacon: no IFD".into()))?;

        // CREATOR_TAG: newline-delimited; Java reads experimenter (line 4),
        // image name (line 6), creation date (lines 8 + 10).
        let mut experimenter_name = None;
        let mut image_name = None;
        let mut creation_date = None;
        if let Some(creator) = first.get_str(Self::CREATOR_TAG) {
            let lines: Vec<&str> = creator.split('\n').collect();
            if lines.len() > 4 {
                experimenter_name = Some(lines[4].trim().to_string());
            }
            if lines.len() > 6 {
                image_name = Some(lines[6].trim().to_string());
            }
            if lines.len() > 8 {
                let mut date = lines[8].trim().to_string();
                if lines.len() > 10 {
                    date.push(' ');
                    date.push_str(lines[10].trim());
                }
                creation_date = Some(date);
            }
        }

        let xml = first.get_str(Self::XML_TAG).map(str::to_owned);
        self.meta = self
            .inner
            .series_list()
            .iter()
            .enumerate()
            .map(|(i, series)| {
                let mut meta = series.metadata.clone();
                meta.series_metadata
                    .insert("format".into(), MetadataValue::String("Imacon".into()));
                if let Some(xml) = xml.as_deref() {
                    imacon_add_xml_metadata(xml, &mut meta.series_metadata);
                }
                if let Some(name) = experimenter_name.as_deref() {
                    meta.series_metadata.insert(
                        "Experimenter".into(),
                        MetadataValue::String(name.to_string()),
                    );
                    let mut parts = name.splitn(2, ' ');
                    let first = parts.next().unwrap_or("");
                    let last = parts.next();
                    meta.series_metadata.insert(
                        "ExperimenterFirstName".into(),
                        MetadataValue::String(last.map(|_| first).unwrap_or("").to_string()),
                    );
                    meta.series_metadata.insert(
                        "ExperimenterLastName".into(),
                        MetadataValue::String(last.unwrap_or(first).to_string()),
                    );
                }
                if let Some(base_name) = image_name.as_deref() {
                    let name = if base_name.is_empty() {
                        format!("#{}", i + 1)
                    } else {
                        format!("{base_name} #{}", i + 1)
                    };
                    meta.series_metadata
                        .insert("ImageName".into(), MetadataValue::String(name));
                }
                if let Some(date) = creation_date.as_deref() {
                    meta.series_metadata.insert(
                        "CreationDate".into(),
                        MetadataValue::String(date.to_string()),
                    );
                }
                meta
            })
            .collect();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta.clear();
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        if !self.meta.is_empty() {
            self.inner.series_count()
        } else {
            0
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        self.inner.set_series(s)
    }

    fn series(&self) -> usize {
        self.inner.series()
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .get(self.inner.series())
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
        self.inner.set_resolution(level)
    }
}

// ---------------------------------------------------------------------------
// 6. Santa Barbara Instrument Group — FITS wrapper
// ---------------------------------------------------------------------------
/// Santa Barbara Instrument Group reader (`.fts`).
///
/// SBIG camera format (`ST-7 Compressed Image` header).
pub struct SbigReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    compressed: bool,
}

impl SbigReader {
    const HEADER_SIZE: u64 = 2048;
    const MAGIC: &'static str = "ST-7 Compressed Image";

    pub fn new() -> Self {
        SbigReader {
            path: None,
            meta: None,
            compressed: false,
        }
    }
}

impl Default for SbigReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SbigReader {
    fn is_this_type_by_name(&self, _path: &Path) -> bool {
        false
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < Self::HEADER_SIZE as usize {
            return false;
        }
        let n = header.len().min(32);
        std::str::from_utf8(&header[..n])
            .map(|s| s.contains(Self::MAGIC))
            .unwrap_or(false)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        if file_len < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "SBIG file is too short".into(),
            ));
        }
        let mut header = vec![0u8; Self::HEADER_SIZE as usize];
        f.read_exact(&mut header).map_err(BioFormatsError::Io)?;
        if !self.is_this_type_by_bytes(&header) {
            return Err(BioFormatsError::UnsupportedFormat(
                "SBIG header magic not found".into(),
            ));
        }

        let text = String::from_utf8_lossy(&header);
        let mut size_x = None;
        let mut size_y = None;
        let mut compressed = false;
        let mut description = None;
        let mut date = None::<String>;
        let mut physical_size_x = None;
        let mut physical_size_y = None;
        let mut series_metadata = HashMap::new();
        for line in text.lines() {
            let line = line.trim();
            if line == "End" {
                break;
            }
            if line.contains("Compressed") {
                compressed = true;
            }
            if let Some((key, value)) = line.split_once('=') {
                let key = key.trim();
                let value = value.trim();
                series_metadata.insert(key.to_string(), MetadataValue::String(value.to_string()));
                match key {
                    "Width" => size_x = value.parse::<u32>().ok(),
                    "Height" => size_y = value.parse::<u32>().ok(),
                    "Note" => description = Some(value.to_string()),
                    "X_pixel_size" => {
                        physical_size_x = value.parse::<f64>().ok().map(|v| v * 1000.0)
                    }
                    "Y_pixel_size" => {
                        physical_size_y = value.parse::<f64>().ok().map(|v| v * 1000.0)
                    }
                    "Date" => date = Some(value.to_string()),
                    "Time" => {
                        if let Some(date) = &mut date {
                            date.push(' ');
                            date.push_str(value);
                        } else {
                            date = Some(format!("null {value}"));
                        }
                    }
                    _ => {}
                }
            }
        }

        let size_x = size_x
            .filter(|&v| v > 0)
            .ok_or_else(|| BioFormatsError::Format("SBIG: missing Width".into()))?;
        let size_y = size_y
            .filter(|&v| v > 0)
            .ok_or_else(|| BioFormatsError::Format("SBIG: missing Height".into()))?;
        if !compressed {
            let expected = Self::HEADER_SIZE
                .checked_add(size_x as u64 * size_y as u64 * 2)
                .ok_or_else(|| BioFormatsError::Format("SBIG plane size overflow".into()))?;
            if file_len < expected {
                return Err(BioFormatsError::Format(
                    "SBIG file is too short for declared dimensions".into(),
                ));
            }
        }

        self.path = Some(path.to_path_buf());
        self.compressed = compressed;
        if let Some(description) = description {
            series_metadata.insert("Description".into(), MetadataValue::String(description));
        }
        if let Some(date) = date {
            series_metadata.insert("AcquisitionDate".into(), MetadataValue::String(date));
        }
        if let Some(size) = physical_size_x {
            series_metadata.insert("PhysicalSizeX".into(), MetadataValue::Float(size));
        }
        if let Some(size) = physical_size_y {
            series_metadata.insert("PhysicalSizeY".into(), MetadataValue::Float(size));
        }
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
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
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.compressed = false;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::HEADER_SIZE))
            .map_err(BioFormatsError::Io)?;
        let width_bytes = meta.size_x as usize * 2;
        let mut buf = vec![0u8; width_bytes * meta.size_y as usize];
        if self.compressed {
            for row in 0..meta.size_y as usize {
                let row_len = read_u16_from(&mut f, true)? as usize;
                let row_start = row * width_bytes;
                if row_len == width_bytes {
                    f.read_exact(&mut buf[row_start..row_start + row_len])
                        .map_err(BioFormatsError::Io)?;
                } else {
                    if width_bytes < 2 {
                        continue;
                    }
                    f.read_exact(&mut buf[row_start..row_start + 2])
                        .map_err(BioFormatsError::Io)?;
                    let mut offset = row_start + 2;
                    while offset - row_start < width_bytes {
                        let mut check = [0u8; 1];
                        f.read_exact(&mut check).map_err(BioFormatsError::Io)?;
                        if check[0] == 0x80 {
                            f.read_exact(&mut buf[offset..offset + 2])
                                .map_err(BioFormatsError::Io)?;
                        } else {
                            let prev = i16::from_le_bytes([buf[offset - 2], buf[offset - 1]]);
                            let value = prev.wrapping_add(check[0] as i8 as i16);
                            buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
                        }
                        offset += 2;
                    }
                }
            }
        } else {
            f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        }
        Ok(buf)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("SBIG", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(p, tx, ty, tw, th)
    }
}

fn read_u16_from(f: &mut std::fs::File, little_endian: bool) -> Result<u16> {
    let mut b = [0u8; 2];
    f.read_exact(&mut b).map_err(BioFormatsError::Io)?;
    Ok(if little_endian {
        u16::from_le_bytes(b)
    } else {
        u16::from_be_bytes(b)
    })
}

// ---------------------------------------------------------------------------
// 7. Image-Pro Workspace — OLE2 compound document with embedded TIFFs
// ---------------------------------------------------------------------------
/// Image-Pro Workspace format reader (`.ipw`).
///
/// Ported from `IPWReader.java`. An IPW file is an OLE2/Compound Document
/// (magic `0xd0cf11e0`), NOT a plain TIFF. Each image plane is stored as an
/// embedded `ImageTIFF` stream; an `ImageInfo` stream carries a text
/// description with `channels`/`slices`/`frames` counts. This reader uses the
/// `cfb` crate to enumerate streams, parses dimensions from the first
/// embedded TIFF, and reads each plane by extracting its `ImageTIFF` stream
/// to a temporary file and delegating to `TiffReader` (the in-tree TIFF
/// reader is path-based).
pub struct IpwReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Embedded TIFF stream paths, ordered by plane index.
    image_streams: Vec<String>,
}

impl IpwReader {
    pub fn new() -> Self {
        IpwReader {
            path: None,
            meta: None,
            image_streams: Vec::new(),
        }
    }

    /// Extract an embedded stream to a temp file and run a `TiffReader` op.
    fn read_embedded_tiff(
        &self,
        stream_path: &str,
        op: impl FnOnce(&mut crate::tiff::TiffReader) -> Result<Vec<u8>>,
    ) -> Result<Vec<u8>> {
        let (mut reader, tmp) = self.open_embedded_tiff(stream_path)?;
        let result = op(&mut reader);
        reader.close().ok();
        std::fs::remove_file(&tmp).ok();
        result
    }

    /// Extract an embedded stream to a temp file, returning an initialised
    /// `TiffReader` plus the temp path to clean up.
    fn open_embedded_tiff(&self, stream_path: &str) -> Result<(crate::tiff::TiffReader, PathBuf)> {
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut comp =
            cfb::open(path).map_err(|e| BioFormatsError::Format(format!("IPW CFB open: {e}")))?;
        let mut stream = comp
            .open_stream(stream_path)
            .map_err(|e| BioFormatsError::Format(format!("IPW stream {stream_path}: {e}")))?;
        let mut data = Vec::new();
        stream.read_to_end(&mut data).map_err(BioFormatsError::Io)?;
        drop(stream);
        drop(comp);

        let tmp = std::env::temp_dir().join(format!(
            "bioformats_ipw_{}_{}.tif",
            std::process::id(),
            stream_path.replace(['/', '\\', ' '], "_")
        ));
        std::fs::write(&tmp, &data).map_err(BioFormatsError::Io)?;
        let mut reader = crate::tiff::TiffReader::new();
        match reader.set_id(&tmp) {
            Ok(()) => Ok((reader, tmp)),
            Err(e) => {
                std::fs::remove_file(&tmp).ok();
                Err(e)
            }
        }
    }
}

impl Default for IpwReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Parse the IPW `ImageInfo` description into (sizeC, sizeZ, sizeT).
fn parse_ipw_image_info(text: &str) -> Result<(Option<u32>, Option<u32>, Option<u32>)> {
    let (mut c, mut z, mut t) = (None, None, None);
    for line in text.split('\n') {
        if let Some((label, data)) = line.split_once('=') {
            let label = label.trim();
            match label.trim() {
                "channels" | "slices" | "frames" => {
                    let value = data.trim().parse::<u32>().map_err(|_| {
                        BioFormatsError::Format(format!("IPW: invalid {label} value"))
                    })?;
                    match label {
                        "channels" => c = Some(value),
                        "slices" => z = Some(value),
                        "frames" => t = Some(value),
                        _ => {}
                    }
                }
                _ => {}
            }
        }
    }
    Ok((c, z, t))
}

impl FormatReader for IpwReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("ipw")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4
            && u32::from_be_bytes([header[0], header[1], header[2], header[3]]) == 0xd0cf_11e0
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let mut comp =
            cfb::open(path).map_err(|e| BioFormatsError::Format(format!("IPW CFB open: {e}")))?;

        // Enumerate streams. ImageTIFF streams hold pixels; the numeric
        // storage just above the stream is the plane index (Java parses it
        // from the path, defaulting to 0 directly under Root Entry).
        let entries: Vec<(String, bool)> = comp
            .walk()
            .map(|e| (e.path().to_string_lossy().to_string(), e.is_stream()))
            .collect();

        let mut image_streams: Vec<(u32, String)> = Vec::new();
        let mut info_stream: Option<String> = None;
        for (raw_path, is_stream) in &entries {
            if !is_stream {
                continue;
            }
            let norm = raw_path.replace('\\', "/");
            let base = norm.rsplit('/').next().unwrap_or("");
            if base == "ImageTIFF" {
                let parts: Vec<&str> = norm.trim_matches('/').split('/').collect();
                let idx = if parts.len() >= 2 {
                    parts[parts.len() - 2]
                        .chars()
                        .filter(|c| c.is_ascii_digit())
                        .collect::<String>()
                        .parse::<u32>()
                        .unwrap_or(0)
                } else {
                    0
                };
                image_streams.push((idx, raw_path.clone()));
            } else if base == "ImageInfo" {
                info_stream = Some(raw_path.clone());
            }
        }

        if image_streams.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "IPW: no embedded ImageTIFF streams found".into(),
            ));
        }
        image_streams.sort_by_key(|(idx, _)| *idx);
        let image_count = image_streams.len() as u32;
        let ordered: Vec<String> = image_streams.into_iter().map(|(_, p)| p).collect();

        // Parse ImageInfo for axis sizes.
        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "format".into(),
            MetadataValue::String("Image-Pro Workspace".into()),
        );
        let (mut size_c, mut size_z, mut size_t) = (None, None, None);
        if let Some(info_path) = &info_stream {
            if let Ok(mut s) = comp.open_stream(info_path) {
                let mut buf = Vec::new();
                if s.read_to_end(&mut buf).is_ok() {
                    let text = String::from_utf8_lossy(&buf);
                    series_metadata.insert(
                        "Image Description".into(),
                        MetadataValue::String(text.trim().to_string()),
                    );
                    let (c, z, t) = parse_ipw_image_info(&text)?;
                    size_c = c;
                    size_z = z;
                    size_t = t;
                }
            }
        }
        drop(comp);

        self.path = Some(path.to_path_buf());
        self.image_streams = ordered;

        // Read first embedded TIFF for X/Y/pixel type.
        let first_stream = self.image_streams[0].clone();
        let (mut tiff, tmp) = self.open_embedded_tiff(&first_stream)?;
        let first_meta = tiff.metadata().clone();
        tiff.close().ok();
        std::fs::remove_file(&tmp).ok();

        let mut size_z = size_z.unwrap_or(1);
        let mut size_c = size_c.unwrap_or(1).max(1);
        let size_t = size_t.unwrap_or(1).max(1);
        if size_z == 0 {
            size_z = 1;
        }
        // Java: if axis product == 1 but multiple planes exist, treat as Z.
        if size_z * size_c * size_t == 1 && image_count != 1 {
            size_z = image_count;
        }
        if first_meta.is_rgb {
            size_c = size_c.saturating_mul(first_meta.size_c.max(1));
        }

        let meta = ImageMetadata {
            size_x: first_meta.size_x,
            size_y: first_meta.size_y,
            size_z,
            size_c,
            size_t,
            pixel_type: first_meta.pixel_type,
            bits_per_pixel: first_meta.bits_per_pixel,
            image_count,
            dimension_order: if first_meta.is_rgb {
                DimensionOrder::XYCZT
            } else {
                DimensionOrder::XYZCT
            },
            is_rgb: first_meta.is_rgb,
            is_interleaved: first_meta.is_interleaved,
            is_indexed: first_meta.is_indexed,
            is_little_endian: first_meta.is_little_endian,
            resolution_count: 1,
            thumbnail: false,
            series_metadata,
            lookup_table: first_meta.lookup_table.clone(),
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.image_streams.clear();
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
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
        let stream = self.image_streams[plane_index as usize].clone();
        self.read_embedded_tiff(&stream, |r| r.open_bytes(0))
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
        let stream = self.image_streams[plane_index as usize].clone();
        self.read_embedded_tiff(&stream, move |r| r.open_bytes_region(0, x, y, w, h))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 8. Photoshop-annotated TIFF — TIFF wrapper
// ---------------------------------------------------------------------------

/// IFD tag carrying the Photoshop `IMAGE_SOURCE_DATA` layer payload.
///
/// Mirrors `PhotoshopTiffReader.IMAGE_SOURCE_DATA` (37724) in the Java reader.
const PHOTOSHOP_IMAGE_SOURCE_DATA: u16 = 37724;

/// Endianness-aware byte cursor over the `IMAGE_SOURCE_DATA` payload.
///
/// Mirrors the `tag` `RandomAccessInputStream` of the Java reader, whose byte
/// order is taken from the host TIFF (`tag.order(isLittleEndian())`). Reads that
/// run past the end clamp to zero / empty, never panic.
struct PsTag<'a> {
    d: &'a [u8],
    p: usize,
    little_endian: bool,
}

impl<'a> PsTag<'a> {
    fn new(d: &'a [u8], little_endian: bool) -> Self {
        PsTag {
            d,
            p: 0,
            little_endian,
        }
    }
    fn fp(&self) -> usize {
        self.p
    }
    fn len(&self) -> usize {
        self.d.len()
    }
    fn seek(&mut self, p: usize) {
        self.p = p.min(self.d.len());
    }
    fn skip_bytes(&mut self, n: usize) {
        self.p = self.p.saturating_add(n).min(self.d.len());
    }
    /// Read one byte (Java `read()`), returning 0 past the end.
    fn read(&mut self) -> u8 {
        let v = self.d.get(self.p).copied().unwrap_or(0);
        if self.p < self.d.len() {
            self.p += 1;
        }
        v
    }
    fn read_short(&mut self) -> i16 {
        let v = if self.p + 2 <= self.d.len() {
            let b = [self.d[self.p], self.d[self.p + 1]];
            if self.little_endian {
                i16::from_le_bytes(b)
            } else {
                i16::from_be_bytes(b)
            }
        } else {
            0
        };
        self.skip_bytes(2);
        v
    }
    fn read_int(&mut self) -> i32 {
        let v = if self.p + 4 <= self.d.len() {
            let b = [
                self.d[self.p],
                self.d[self.p + 1],
                self.d[self.p + 2],
                self.d[self.p + 3],
            ];
            if self.little_endian {
                i32::from_le_bytes(b)
            } else {
                i32::from_be_bytes(b)
            }
        } else {
            0
        };
        self.skip_bytes(4);
        v
    }
    /// Read `n` raw bytes (Java `readString(n)` body), clamping at the end.
    fn read_string(&mut self, n: usize) -> &'a [u8] {
        let end = self.p.saturating_add(n).min(self.d.len());
        let s = &self.d[self.p..end];
        self.p = end;
        s
    }
    /// Read a NUL-terminated ASCII string (Java `readCString()`).
    fn read_cstring(&mut self) {
        while self.p < self.d.len() && self.d[self.p] != 0 {
            self.p += 1;
        }
        if self.p < self.d.len() {
            self.p += 1; // consume the terminator
        }
    }
}

/// Strip non-ASCII bytes and trim, mirroring Java's
/// `replaceAll("[^\\p{ASCII}]", "").trim()` on decoded layer names.
fn photoshop_clean_layer_name(bytes: &[u8]) -> String {
    let ascii: String = bytes
        .iter()
        .filter(|&&b| b.is_ascii())
        .map(|&b| b as char)
        .collect();
    // Java String.trim() removes any leading/trailing char <= ' ' (0x20),
    // which includes the NUL padding bytes appended to layer names.
    ascii.trim_matches(|c: char| (c as u32) <= 0x20).to_string()
}

/// Adobe Photoshop TIFF reader.
///
/// Port of `loci.formats.in.PhotoshopTiffReader`. Pixel data and the merged
/// (series 0) dimensions are served by the inner [`crate::tiff::TiffReader`];
/// the `IMAGE_SOURCE_DATA` tag (37724) is additionally parsed for per-layer
/// metadata — layer names recorded as `"Layer name"` global-metadata entries
/// and the layer count, mirroring the Java reader's `initFile` layer loop.
pub struct PhotoshopTiffReader {
    inner: crate::tiff::TiffReader,
    metas: Vec<ImageMetadata>,
    current_series: usize,
    /// Decoded, ASCII-cleaned layer names (Java `layerNames`, filtered).
    layer_names: Vec<String>,
}

impl PhotoshopTiffReader {
    pub fn new() -> Self {
        PhotoshopTiffReader {
            inner: crate::tiff::TiffReader::new(),
            metas: Vec::new(),
            current_series: 0,
            layer_names: Vec::new(),
        }
    }

    /// Mirror of Java `openPixelTag()`: fetch the raw `IMAGE_SOURCE_DATA` bytes.
    ///
    /// Returns `None` when the tag is absent (a plain TIFF), exactly as the Java
    /// reader leaves `tag` null.
    fn open_pixel_tag(&self) -> Option<Vec<u8>> {
        let ifd = self.inner.ifd(0)?;
        match ifd.get(PHOTOSHOP_IMAGE_SOURCE_DATA) {
            Some(crate::tiff::ifd::IfdValue::Undefined(b))
            | Some(crate::tiff::ifd::IfdValue::Byte(b)) => Some(b.clone()),
            _ => None,
        }
    }

    /// Mirror of Java `initFile()`'s `IMAGE_SOURCE_DATA` layer loop.
    ///
    /// Walks the signature/type/length blocks; for the `"ryaL"` (`Layr`
    /// reversed) block it decodes each layer's bounds, channel table, and name,
    /// applying Java's name-acceptance filter. Accepted names become `layer_names`
    /// and `"Layer name"` global-metadata list entries.
    fn init_file(&mut self, source_data: &[u8]) {
        let little_endian = self.inner.is_little_endian();
        let mut tag = PsTag::new(source_data, little_endian);

        // Java: String checkString = tag.readCString();
        tag.read_cstring();

        // Series 0 ("Merged") is the inner TIFF; further series are layers.
        let mut series_count: usize = 1;
        let mut layer_metas: Vec<ImageMetadata> = Vec::new();

        while tag.fp() < tag.len().saturating_sub(12) && tag.fp() > 0 {
            let _signature = tag.read_string(4);
            let block_type = tag.read_string(4).to_vec();
            let length = tag.read_int();
            let mut skip = (length as i64).rem_euclid(4);
            if skip != 0 {
                skip = 4 - skip;
            }

            if block_type == b"ryaL" {
                let n_layers = (tag.read_short() as i32).unsigned_abs() as usize;

                for layer in 0..n_layers {
                    let top = tag.read_int();
                    let left = tag.read_int();
                    let bottom = tag.read_int();
                    let right = tag.read_int();

                    let layer_size_x = right.wrapping_sub(left);
                    let layer_size_y = bottom.wrapping_sub(top);
                    let layer_size_c = tag.read_short() as i32;

                    // Java: if sizeX==0 || sizeY==0 || (sizeC>1 && !RGB) -> reset
                    // to a single series and break. The merged image is not RGB
                    // in this port's metadata, so multi-channel layers abort.
                    let is_rgb = self.inner.metadata().is_rgb;
                    if layer_size_x == 0 || layer_size_y == 0 || (layer_size_c > 1 && !is_rgb) {
                        series_count = 1;
                        self.layer_names.clear();
                        layer_metas.clear();
                        break;
                    }

                    let channel_count = layer_size_c.max(0) as usize;
                    for _c in 0..channel_count {
                        let _channel_id = tag.read_short();
                        let _data_size = tag.read_int();
                    }

                    tag.skip_bytes(12);

                    let len = tag.read_int();
                    let fp = tag.fp();

                    let mask = tag.read_int();
                    if mask != 0 {
                        tag.skip_bytes(mask.max(0) as usize);
                    }
                    let blending = tag.read_int();
                    tag.skip_bytes(blending.max(0) as usize);

                    let name_length = tag.read() as usize;
                    let mut pad = name_length % 4;
                    if pad != 0 {
                        pad = 4 - pad;
                    }
                    let raw_name = tag.read_string(name_length + pad);
                    let layer_name = photoshop_clean_layer_name(raw_name);

                    // Java tests the cleaned String length after
                    // replaceAll(...).trim(), not the number of bytes read.
                    let synthetic = format!("Layer {layer}M");
                    if layer_name.len() == name_length + pad
                        && !layer_name.eq_ignore_ascii_case(&synthetic)
                    {
                        let mut layer_meta = self.inner.metadata().clone();
                        layer_meta.size_x = layer_size_x as u32;
                        layer_meta.size_y = layer_size_y as u32;
                        layer_meta.size_c = layer_size_c.max(1) as u32;
                        layer_meta.size_z = 1;
                        layer_meta.size_t = 1;
                        layer_meta.image_count = 1;
                        layer_meta.is_rgb = is_rgb;
                        layer_meta.is_interleaved = self.inner.metadata().is_interleaved;
                        layer_meta.dimension_order = self.inner.metadata().dimension_order;
                        self.layer_names.push(layer_name);
                        layer_metas.push(layer_meta);
                        series_count += 1;
                    }

                    // Java: tag.skipBytes(fp + len - tag.getFilePointer());
                    let target = fp.saturating_add(len.max(0) as usize);
                    if target > tag.fp() {
                        tag.skip_bytes(target - tag.fp());
                    } else {
                        tag.seek(target);
                    }
                }
            } else {
                // Java: tag.skipBytes((long) length + skip);
                let advance = (length.max(0) as usize).saturating_add(skip as usize);
                tag.skip_bytes(advance);
            }
        }

        // Java: store.setImageName("Merged", 0), then add one CoreMetadata per
        // accepted layer. Pixel offsets are not decoded here, but the exposed
        // series metadata/dimensions match the Java layer discovery path.
        let mut merged = self.inner.metadata().clone();
        let mut metas = vec![merged.clone()];
        for (i, name) in self.layer_names.iter().enumerate() {
            merged.series_metadata.insert(
                format!("Layer name #{}", i + 1),
                MetadataValue::String(name.clone()),
            );
            let mut layer_meta = layer_metas
                .get(i)
                .cloned()
                .unwrap_or_else(|| self.inner.metadata().clone());
            layer_meta
                .series_metadata
                .insert("ImageName".into(), MetadataValue::String(name.clone()));
            metas.push(layer_meta);
        }
        merged.series_metadata.insert(
            "Photoshop layer count".to_string(),
            MetadataValue::Int(self.layer_names.len() as i64),
        );
        metas[0] = merged;
        let _ = series_count;
        self.metas = metas;
        self.current_series = 0;
    }
}

impl Default for PhotoshopTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PhotoshopTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("tif") | Some("tiff")
        )
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Java isThisType requires the first IFD to contain IMAGE_SOURCE_DATA,
        // which lives past the header window; detection happens in set_id.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        self.inner.set_id(path)?;
        self.layer_names.clear();
        // Mirror Java openPixelTag()/initFile(): parse the layer payload when
        // the IMAGE_SOURCE_DATA tag is present, else fall back to plain TIFF.
        match self.open_pixel_tag() {
            Some(source_data) => self.init_file(&source_data),
            None => self.metas = vec![self.inner.metadata().clone()],
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.layer_names.clear();
        self.metas.clear();
        self.current_series = 0;
        self.inner.close()
    }

    fn series_count(&self) -> usize {
        self.metas.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.metas.len() {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.current_series = s;
        if s == 0 {
            self.inner.set_series(0)?;
        }
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.metas
            .get(self.current_series)
            .unwrap_or_else(|| self.inner.metadata())
    }

    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if self.current_series != 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Photoshop TIFF layer pixel decoding is not supported".into(),
            ));
        }
        self.inner.open_bytes(p)
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.current_series != 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Photoshop TIFF layer pixel decoding is not supported".into(),
            ));
        }
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

// ---------------------------------------------------------------------------
// 9. Nikon NEF — TIFF-based camera RAW (Nikon maker-note compression 34713)
// ---------------------------------------------------------------------------
/// Nikon NEF reader (`.nef`).
///
/// Faithful port of `NikonReader.java` (extends `BaseTiffReader`). NEF files are
/// TIFF-based camera RAW images. The reader delegates IFD parsing and metadata
/// to [`crate::tiff::TiffReader`], then mirrors Java's `initStandardMetadata`,
/// `initFile`, and `openBytes`:
///
///  * detection: extension `.nef`, or a TIFF whose first IFD carries the
///    `TIFF_EPS_STANDARD` tag (37398) or a `Make` tag containing "Nikon";
///  * pixel decode: each strip is read raw, decompressed with the **existing**
///    Nikon codec (`crate::tiff::nikon::decompress_nikon`, fed by options from
///    `crate::tiff::nikon::extract_compression_options`) when the strip uses
///    Nikon compression 34713, then assembled with the per-pixel Bayer color
///    map, optional white-balance scaling, and `cfa::interpolate` into an
///    interleaved RGB plane (matching `ImageTools.interpolate`).
///
/// The Nikon decompression codec and maker-note option parser are reused
/// verbatim from `src/tiff/nikon.rs`; no codec is reimplemented here.
pub struct NikonReader {
    inner: crate::tiff::TiffReader,
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Maker-note compression options (tag 150), if the file is compressed.
    compression_options: Option<crate::tiff::nikon::NikonCompressionOptions>,
    /// White-balance RGB coefficients (maker-note tag 12), if present.
    white_balance: Option<[f64; 3]>,
    /// Cached decoded interleaved-RGB plane (`lastPlane` in Java).
    last_plane: Option<Vec<u8>>,
}

impl NikonReader {
    /// Tag that gives a good indication of whether this is an NEF file.
    const TIFF_EPS_STANDARD: u16 = 37398;
    /// CFA color map tag carried in the data IFD.
    const COLOR_MAP: u16 = 33422;
    /// TIFF `Make` tag.
    const MAKE: u16 = 271;
    /// Maker-note tag holding RGB white-balance coefficients.
    const WHITE_BALANCE_RGB_COEFFS: u16 = 12;
    /// Default Bayer color map (index = (row%2)*2 + (col%2) -> 0=R, 1=G, 2=B).
    const DEFAULT_COLOR_MAP: [i32; 4] = [1, 0, 2, 1];

    pub fn new() -> Self {
        NikonReader {
            inner: crate::tiff::TiffReader::new(),
            path: None,
            meta: None,
            compression_options: None,
            white_balance: None,
            last_plane: None,
        }
    }

    /// Port of `isThisType(RandomAccessInputStream)`: parse the first IFD and
    /// check for the EPS-standard tag or a `Make` value containing "Nikon".
    fn is_this_type_stream(header: &[u8]) -> bool {
        let cursor = std::io::Cursor::new(header);
        let mut parser = match crate::tiff::parser::TiffParser::new(cursor) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let ifd = match parser.read_ifd(parser.first_ifd_offset) {
            Ok((ifd, _)) => ifd,
            Err(_) => return false,
        };
        if ifd.get(Self::TIFF_EPS_STANDARD).is_some() {
            return true;
        }
        matches!(ifd.get_str(Self::MAKE), Some(make) if make.contains("Nikon"))
    }

    /// Extract the maker-note white-balance coefficients (tag 12), if present.
    ///
    /// Mirrors the `WHITE_BALANCE_RGB_COEFFS` branch of Java's
    /// `initStandardMetadata`; reuses the same EXIF/maker-note traversal as the
    /// compression-option extractor.
    fn read_white_balance(path: &Path) -> Result<Option<[f64; 3]>> {
        use crate::tiff::ifd::IfdValue;
        let file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut parser = match crate::tiff::parser::TiffParser::new(std::io::BufReader::new(file)) {
            Ok(p) => p,
            Err(_) => return Ok(None),
        };
        let little = parser.little_endian;
        let main_ifds = parser.read_ifds()?;
        for ifd in &main_ifds {
            let Some(exif_offset) = ifd.get_u64(crate::tiff::nikon::EXIF_IFD_TAG) else {
                continue;
            };
            if exif_offset == 0 {
                continue;
            }
            let (exif_ifd, _) = parser.read_ifd(exif_offset)?;
            let maker = match exif_ifd.get(crate::tiff::nikon::EXIF_MAKER_NOTE_TAG) {
                Some(IfdValue::Byte(b)) | Some(IfdValue::Undefined(b)) => b.clone(),
                _ => continue,
            };
            let Some(note) = Self::parse_maker_note_ifd(&maker, little)? else {
                continue;
            };
            if note.is_rational(Self::WHITE_BALANCE_RGB_COEFFS) {
                let coeffs = note
                    .get(Self::WHITE_BALANCE_RGB_COEFFS)
                    .map(|v| v.as_vec_f64())
                    .unwrap_or_default();
                if coeffs.len() >= 3 {
                    return Ok(Some([coeffs[0], coeffs[1], coeffs[2]]));
                }
            }
        }
        Ok(None)
    }

    /// Parse the nested Nikon maker-note IFD from raw EXIF MakerNote bytes,
    /// skipping the 10-byte `Nikon...` prefix. Mirrors the (private) helper of
    /// the same name in `src/tiff/nikon.rs`; kept here as a thin public-API
    /// wrapper so this reader needs no changes to that module.
    fn parse_maker_note_ifd(
        data: &[u8],
        little_endian: bool,
    ) -> Result<Option<crate::tiff::ifd::Ifd>> {
        let nested = if data.len() >= 10 && data.starts_with(b"Nikon") {
            &data[10..]
        } else {
            data
        };
        let mut parser = match crate::tiff::parser::TiffParser::new(std::io::Cursor::new(nested)) {
            Ok(parser) => parser,
            Err(_) => return Ok(None),
        };
        if parser.little_endian != little_endian {
            return Ok(None);
        }
        parser
            .read_ifd(parser.first_ifd_offset)
            .map(|(ifd, _)| Some(ifd))
    }

    /// Port of `adjustForWhiteBalance`: scale a sample by the per-channel
    /// white-balance coefficient when three coefficients are available.
    fn adjust_for_white_balance(&self, val: i16, index: usize) -> i16 {
        match self.white_balance {
            Some(wb) => (val as f64 * wb[index]) as i16,
            None => val,
        }
    }

    /// Port of `openBytes`: decode plane `no` into an interleaved RGB plane.
    fn decode_plane(&mut self, no: usize) -> Result<Vec<u8>> {
        use crate::tiff::ifd::tag;

        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;
        let little = meta.is_little_endian;

        let ifd = self
            .inner
            .ifd(no)
            .ok_or_else(|| BioFormatsError::PlaneOutOfRange(no as u32))?;
        let bps = ifd.bits_per_sample();
        let mut data_size = *bps.first().unwrap_or(&16) as u16;
        let byte_counts = ifd.get_vec_u64(tag::STRIP_BYTE_COUNTS);
        let offsets = ifd.get_vec_u64(tag::STRIP_OFFSETS);
        let compression = ifd.compression();
        let ifd_colors = ifd.get_vec_u16(Self::COLOR_MAP);

        let total_bytes: u64 = byte_counts.iter().copied().sum();
        let plane_size = size_x
            .saturating_mul(size_y)
            .saturating_mul(meta.pixel_type.bytes_per_sample())
            .saturating_mul(meta.size_c.max(1) as usize);

        // If the data is already uncompressed full-size (or multi-sample),
        // Java defers to BaseTiffReader.openBytes (plain strip decode).
        if total_bytes as usize == plane_size || bps.len() > 1 {
            return self.inner.open_bytes(no as u32);
        }

        let maybe_compressed = compression == crate::tiff::ifd::Compression::Nikon;
        let compressed = self.compression_options.is_some() && maybe_compressed;

        if !maybe_compressed && data_size == 14 {
            data_size = 16;
        }

        // Read and (optionally) decompress every strip into one buffer.
        let mut file = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        let mut src: Vec<u8> = Vec::new();
        for (i, &count) in byte_counts.iter().enumerate() {
            let mut t = vec![0u8; count as usize];
            std::io::Seek::seek(&mut file, std::io::SeekFrom::Start(offsets[i]))
                .map_err(BioFormatsError::Io)?;
            std::io::Read::read_exact(&mut file, &mut t).map_err(BioFormatsError::Io)?;
            if compressed {
                let options = self.compression_options.as_ref().unwrap();
                t = crate::tiff::nikon::decompress_nikon(
                    &t,
                    size_x as u32,
                    size_y as u32,
                    data_size,
                    options,
                )?;
            }
            src.extend_from_slice(&t);
        }

        // Build the effective color map (default unless the IFD overrides it).
        let mut color_map = Self::DEFAULT_COLOR_MAP;
        if ifd_colors.len() >= color_map.len() {
            let colors_valid = ifd_colors[..color_map.len()].iter().all(|&c| c <= 2);
            if colors_valid {
                for q in 0..color_map.len() {
                    color_map[q] = ifd_colors[q] as i32;
                }
            }
        }

        let interleave_rows = offsets.len() == 1 && !maybe_compressed && color_map[0] != 0;

        // Planar [R | G | B] short buffer assembled from the CFA samples.
        let mut pix = vec![0i16; size_x * size_y * 3];
        let mut bb = cfa::BitReader::new(&src);
        for row in 0..size_y {
            let real_row = if interleave_rows {
                if row < size_y / 2 {
                    row * 2
                } else {
                    (row - size_y / 2) * 2 + 1
                }
            } else {
                row
            };
            for col in 0..size_x {
                let val = (bb.read_bits(data_size as u32) & 0xffff) as u16 as i16;
                let map_index = (real_row % 2) * 2 + (col % 2);

                let red_offset = real_row * size_x + col;
                let green_offset = (size_y + real_row) * size_x + col;
                let blue_offset = (2 * size_y + real_row) * size_x + col;

                match color_map[map_index] {
                    0 => pix[red_offset] = self.adjust_for_white_balance(val, 0),
                    1 => pix[green_offset] = self.adjust_for_white_balance(val, 1),
                    2 => pix[blue_offset] = self.adjust_for_white_balance(val, 2),
                    _ => {}
                }

                if maybe_compressed && !compressed {
                    let mut to_skip = 0usize;
                    if col % 10 == 9 {
                        to_skip = 1;
                    }
                    if col == size_x - 1 {
                        to_skip = 10;
                    }
                    bb.skip_bits(to_skip * 8);
                }
            }
        }

        let mut out = vec![0u8; size_x * size_y * 3 * 2];
        cfa::interpolate(&pix, &mut out, &color_map, size_x, size_y, little);
        Ok(out)
    }
}

impl Default for NikonReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NikonReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // extension is sufficient as long as it is NEF (Java NEF_SUFFIX).
        matches!(
            path.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.to_ascii_lowercase())
                .as_deref(),
            Some("nef")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::is_this_type_stream(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        self.inner.set_id(path)?;

        let first = self
            .inner
            .ifd(0)
            .ok_or_else(|| BioFormatsError::UnsupportedFormat("Nikon NEF: no IFD".into()))?;
        let bits = first.bits_per_sample();
        let bits_per_sample = bits.first().copied().unwrap_or(16);

        // initStandardMetadata: reset dimensions from the first data IFD.
        let samples = first.samples_per_pixel();
        let photo = first.get_u16(crate::tiff::ifd::tag::PHOTOMETRIC_INTERPRETATION);
        // PhotoInterp.CFA_ARRAY == 32803 (not in our Photometric enum).
        let is_cfa = photo == Some(32803);
        let is_rgb = samples > 1 || photo == Some(2) || is_cfa;
        let size_x = first.image_width().unwrap_or(0);
        let size_y = first.image_length().unwrap_or(0);
        let samples = if is_cfa { 3 } else { samples };

        let mut meta = self.inner.metadata().clone();
        meta.size_x = size_x;
        meta.size_y = size_y;
        meta.size_z = 1;
        meta.size_c = if is_rgb { samples as u32 } else { 1 };
        meta.is_rgb = is_rgb;
        meta.is_indexed = false;
        // initFile: imageCount = 1, sizeT = 1; interleaved when single-sample.
        meta.size_t = 1;
        meta.image_count = 1;
        if first.samples_per_pixel() == 1 {
            meta.is_interleaved = true;
        }
        meta.series_metadata
            .insert("format".into(), MetadataValue::String("Nikon NEF".into()));

        // Extract the Nikon maker-note compression options (tag 150) using the
        // existing crate codec helper. Build a parser over the file for it.
        let file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut parser = crate::tiff::parser::TiffParser::new(std::io::BufReader::new(file))?;
        let main_ifds = parser.read_ifds()?;
        self.compression_options = crate::tiff::nikon::extract_compression_options(
            &mut parser,
            &main_ifds,
            bits_per_sample,
        )?;
        self.white_balance = Self::read_white_balance(path)?;

        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.last_plane = None;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.compression_options = None;
        self.white_balance = None;
        self.last_plane = None;
        self.inner.close()
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
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        Ok(())
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
        if self.last_plane.is_none() {
            let plane = self.decode_plane(p as usize)?;
            self.last_plane = Some(plane);
        }
        Ok(self.last_plane.clone().unwrap())
    }

    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(p)?;
        let meta = self.metadata().clone();
        crop_full_plane("Nikon NEF", &full, &meta, 3, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        let meta = self.metadata().clone();
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
            return Err(BioFormatsError::Format(format!(
                "resolution {level} out of range"
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::metadata::MetadataValue;
    use crate::common::writer::FormatWriter;
    use crate::tiff::TiffWriter;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "bioformats_camera2_{name}_{}_{}",
            std::process::id(),
            unique
        ));
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn write_u8_tiff(path: &Path, pixels: &[u8], width: u32, height: u32) {
        let mut meta = ImageMetadata::default();
        meta.size_x = width;
        meta.size_y = height;
        meta.pixel_type = PixelType::Uint8;
        meta.bits_per_pixel = 8;
        meta.image_count = 1;

        let mut writer = TiffWriter::new();
        writer.set_metadata(&meta).unwrap();
        writer.set_id(path).unwrap();
        writer.save_bytes(0, pixels).unwrap();
        writer.close().unwrap();
    }

    fn write_rgb_tiff(path: &Path, pixels: &[u8], width: u32, height: u32) {
        let mut meta = ImageMetadata::default();
        meta.size_x = width;
        meta.size_y = height;
        meta.size_c = 3;
        meta.pixel_type = PixelType::Uint8;
        meta.bits_per_pixel = 8;
        meta.image_count = 1;
        meta.is_rgb = true;
        meta.is_interleaved = true;

        let mut writer = TiffWriter::new();
        writer.set_metadata(&meta).unwrap();
        writer.set_id(path).unwrap();
        writer.save_bytes(0, pixels).unwrap();
        writer.close().unwrap();
    }

    fn write_l2d_dataset(root: &Path) -> PathBuf {
        let scan_dir = root.join("ScanA");
        fs::create_dir_all(&scan_dir).unwrap();
        write_u8_tiff(&scan_dir.join("ch1.tif"), &[1, 2, 3, 4, 5, 6], 3, 2);
        write_u8_tiff(&scan_dir.join("ch2.tif"), &[7, 8, 9, 10, 11, 12], 3, 2);
        fs::write(
            scan_dir.join("ScanA.scn"),
            "ImageNames=ch1.tif, ch2.tif\nComments=synthetic\nScanChannels=700,800\n",
        )
        .unwrap();
        let l2d = root.join("sample.l2d");
        fs::write(&l2d, "FileType=LI-COR LI2D\nScanNames=ScanA\n").unwrap();
        l2d
    }

    #[test]
    fn l2d_delegates_planes_to_companion_tiffs() {
        let root = temp_dir("l2d_planes");
        let l2d = write_l2d_dataset(&root);
        let mut reader = L2dReader::new();
        reader.set_id(&l2d).unwrap();

        let meta = reader.metadata();
        assert_eq!(
            (meta.size_x, meta.size_y, meta.size_c, meta.image_count),
            (3, 2, 2, 2)
        );
        assert_eq!(meta.dimension_order, DimensionOrder::XYCZT);
        match meta.series_metadata.get("Comments") {
            Some(MetadataValue::String(value)) => assert_eq!(value, "synthetic"),
            other => panic!("unexpected Comments metadata: {other:?}"),
        }
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![7, 8, 9, 10, 11, 12]);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn l2d_delegates_regions_to_companion_tiffs() {
        let root = temp_dir("l2d_region");
        let l2d = write_l2d_dataset(&root);
        let mut reader = L2dReader::new();
        reader.set_id(&l2d).unwrap();

        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 2, 2).unwrap(),
            vec![8, 9, 11, 12]
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn l2d_can_open_from_grouped_scn_or_tiff_companion() {
        let root = temp_dir("l2d_grouped_open");
        let l2d = write_l2d_dataset(&root);
        let scn = root.join("ScanA").join("ScanA.scn");
        let tiff = root.join("ScanA").join("ch1.tif");

        let mut from_scn = L2dReader::new();
        from_scn.set_id(&scn).unwrap();
        assert_eq!(from_scn.metadata().image_count, 2);
        assert_eq!(from_scn.open_bytes(1).unwrap(), vec![7, 8, 9, 10, 11, 12]);

        let mut from_tiff = L2dReader::new();
        from_tiff.set_id(&tiff).unwrap();
        assert_eq!(from_tiff.metadata().image_count, 2);
        assert_eq!(from_tiff.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);

        assert!(l2d.is_file());
        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn l2d_multiplies_logical_channels_by_rgb_samples() {
        let root = temp_dir("l2d_rgb_channels");
        let scan_dir = root.join("ScanA");
        fs::create_dir_all(&scan_dir).unwrap();
        write_rgb_tiff(&scan_dir.join("rgb1.tif"), &[1, 2, 3, 4, 5, 6], 2, 1);
        write_rgb_tiff(&scan_dir.join("rgb2.tif"), &[7, 8, 9, 10, 11, 12], 2, 1);
        fs::write(
            scan_dir.join("ScanA.scn"),
            "ImageNames=rgb1.tif, rgb2.tif\n",
        )
        .unwrap();
        let l2d = root.join("sample.l2d");
        fs::write(&l2d, "FileType=LI-COR LI2D\nScanNames=ScanA\n").unwrap();

        let mut reader = L2dReader::new();
        reader.set_id(&l2d).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_c, meta.image_count), (6, 2));
        assert!(meta.is_rgb);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn l2d_rejects_manifest_without_magic() {
        let root = temp_dir("l2d_magic");
        let l2d = root.join("bad.l2d");
        fs::write(&l2d, "ScanNames=ScanA\n").unwrap();
        let err = L2dReader::new().set_id(&l2d).unwrap_err();
        assert!(
            err.to_string().contains("LI-COR LI2D"),
            "unexpected error: {err}"
        );

        fs::remove_dir_all(root).unwrap();
    }

    fn push_tiff_entry(data: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: u32) {
        data.extend_from_slice(&tag.to_le_bytes());
        data.extend_from_slice(&typ.to_le_bytes());
        data.extend_from_slice(&count.to_le_bytes());
        data.extend_from_slice(&value.to_le_bytes());
    }

    fn push_imacon_ifd(
        data: &mut Vec<u8>,
        next_ifd: u32,
        pixel_offset: u32,
        creator_offset: u32,
        xml_offset: u32,
    ) {
        let creator_len =
            b"0\n1\n2\n3\nAda Lovelace\n5\nScan\n7\n20240102\n9\n030405+0000\0".len() as u32;
        let xml_len =
            b"prefix <root><key>Camera</key><value>Imacon 949</value></root>\0".len() as u32;
        data.extend_from_slice(&11u16.to_le_bytes());
        push_tiff_entry(data, 256, 4, 1, 1);
        push_tiff_entry(data, 257, 4, 1, 1);
        push_tiff_entry(data, 258, 3, 1, 8);
        push_tiff_entry(data, 259, 3, 1, 1);
        push_tiff_entry(data, 262, 3, 1, 1);
        push_tiff_entry(data, 273, 4, 1, pixel_offset);
        push_tiff_entry(data, 277, 3, 1, 1);
        push_tiff_entry(data, 278, 4, 1, 1);
        push_tiff_entry(data, 279, 4, 1, 1);
        push_tiff_entry(
            data,
            ImaconReader::CREATOR_TAG,
            2,
            creator_len,
            creator_offset,
        );
        push_tiff_entry(data, ImaconReader::XML_TAG, 2, xml_len, xml_offset);
        data.extend_from_slice(&next_ifd.to_le_bytes());
    }

    fn write_two_ifd_imacon(path: &Path) {
        let ifd_len = 2 + 11 * 12 + 4;
        let ifd1_offset = 8u32;
        let ifd2_offset = ifd1_offset + ifd_len;
        let data_offset = ifd2_offset + ifd_len;
        let creator = b"0\n1\n2\n3\nAda Lovelace\n5\nScan\n7\n20240102\n9\n030405+0000\0";
        let xml = b"prefix <root><key>Camera</key><value>Imacon 949</value></root>\0";
        let creator_offset = data_offset;
        let xml_offset = creator_offset + creator.len() as u32;
        let pixel1_offset = xml_offset + xml.len() as u32;
        let pixel2_offset = pixel1_offset + 1;

        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        data.extend_from_slice(&42u16.to_le_bytes());
        data.extend_from_slice(&ifd1_offset.to_le_bytes());
        push_imacon_ifd(
            &mut data,
            ifd2_offset,
            pixel1_offset,
            creator_offset,
            xml_offset,
        );
        push_imacon_ifd(&mut data, 0, pixel2_offset, creator_offset, xml_offset);
        data.extend_from_slice(creator);
        data.extend_from_slice(xml);
        data.push(11);
        data.push(22);
        fs::write(path, data).unwrap();
    }

    #[test]
    fn imacon_uses_each_ifd_as_a_series_and_applies_first_ifd_metadata() {
        let root = temp_dir("imacon_series");
        let path = root.join("sample.fff");
        write_two_ifd_imacon(&path);

        let mut reader = ImaconReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.metadata().image_count, 1);
        assert_eq!(reader.metadata().dimension_order, DimensionOrder::XYCZT);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![11]);

        let md0 = &reader.metadata().series_metadata;
        assert!(matches!(md0.get("ImageName"), Some(MetadataValue::String(v)) if v == "Scan #1"));
        assert!(
            matches!(md0.get("ExperimenterFirstName"), Some(MetadataValue::String(v)) if v == "Ada")
        );
        assert!(
            matches!(md0.get("ExperimenterLastName"), Some(MetadataValue::String(v)) if v == "Lovelace")
        );
        assert!(
            matches!(md0.get("CreationDate"), Some(MetadataValue::String(v)) if v == "20240102 030405+0000")
        );
        assert!(matches!(md0.get("Camera"), Some(MetadataValue::String(v)) if v == "Imacon 949"));

        reader.set_series(1).unwrap();
        assert_eq!(reader.metadata().image_count, 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![22]);
        assert!(matches!(
            reader.metadata().series_metadata.get("ImageName"),
            Some(MetadataValue::String(v)) if v == "Scan #2"
        ));

        fs::remove_dir_all(root).unwrap();
    }

    fn write_sbig(path: &Path, compressed: bool) {
        let mut bytes = vec![0u8; SbigReader::HEADER_SIZE as usize];
        bytes[..SbigReader::MAGIC.len()].copy_from_slice(SbigReader::MAGIC.as_bytes());
        let header = b"\nWidth = 3\nHeight = 1\nNote = synthetic\nX_pixel_size = 0.001\nY_pixel_size = 0.002\nDate = 06/19/26\nTime = 12:34:56\nEnd\n";
        let start = SbigReader::MAGIC.len();
        bytes[start..start + header.len()].copy_from_slice(header);
        if compressed {
            bytes.extend_from_slice(&4u16.to_le_bytes());
            bytes.extend_from_slice(&100i16.to_le_bytes());
            bytes.push(2u8);
            bytes.push(0x80);
            bytes.extend_from_slice(&50i16.to_le_bytes());
        } else {
            bytes.extend_from_slice(&[1, 0, 2, 0, 3, 0]);
        }
        fs::write(path, bytes).unwrap();
    }

    #[test]
    fn sbig_requires_full_header_and_preserves_metadata() {
        let root = temp_dir("sbig_header");
        let path = root.join("sample.sbig");
        write_sbig(&path, false);

        let reader = SbigReader::new();
        assert!(!reader.is_this_type_by_bytes(SbigReader::MAGIC.as_bytes()));
        let header = fs::read(&path).unwrap();
        assert!(reader.is_this_type_by_bytes(&header[..SbigReader::HEADER_SIZE as usize]));

        let mut reader = SbigReader::new();
        reader.set_id(&path).unwrap();
        let md = &reader.metadata().series_metadata;
        assert!(
            matches!(md.get("Description"), Some(MetadataValue::String(v)) if v == "synthetic")
        );
        assert!(
            matches!(md.get("AcquisitionDate"), Some(MetadataValue::String(v)) if v == "06/19/26 12:34:56")
        );
        assert!(
            matches!(md.get("PhysicalSizeX"), Some(MetadataValue::Float(v)) if (*v - 1.0).abs() < 1e-12)
        );
        assert!(
            matches!(md.get("PhysicalSizeY"), Some(MetadataValue::Float(v)) if (*v - 2.0).abs() < 1e-12)
        );

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn sbig_decompresses_delta_and_literal_pixels() {
        let root = temp_dir("sbig_compressed");
        let path = root.join("compressed.sbig");
        write_sbig(&path, true);

        let mut reader = SbigReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [100i16, 102, 50]
                .into_iter()
                .flat_map(i16::to_le_bytes)
                .collect::<Vec<_>>()
        );

        fs::remove_dir_all(root).unwrap();
    }

    // -----------------------------------------------------------------------
    // CFA / RAW helper tests (cfa::unpack_bytes, cfa::BitReader, cfa::interpolate)
    // -----------------------------------------------------------------------

    #[test]
    fn unpack_bytes_little_and_big_endian() {
        // Port-of-Java check: DataTools.unpackBytes(0x1234, buf, 0, 2, le).
        let mut le = [0u8; 2];
        cfa::unpack_bytes(0x1234, &mut le, 0, 2, true);
        assert_eq!(le, [0x34, 0x12]);

        let mut be = [0u8; 2];
        cfa::unpack_bytes(0x1234, &mut be, 0, 2, false);
        assert_eq!(be, [0x12, 0x34]);

        // Offset and a 4-byte write, big-endian.
        let mut buf = [0u8; 6];
        cfa::unpack_bytes(0x0A0B0C0D, &mut buf, 2, 4, false);
        assert_eq!(buf, [0, 0, 0x0A, 0x0B, 0x0C, 0x0D]);

        // Negative short truncated to its low 2 bytes (Java casts to short).
        let mut neg = [0u8; 2];
        cfa::unpack_bytes((-1i16) as i64, &mut neg, 0, 2, true);
        assert_eq!(neg, [0xff, 0xff]);
    }

    #[test]
    fn bit_reader_msb_first_and_skip() {
        // 0b1010_0110, 0b1100_1111
        let data = [0xA6u8, 0xCF];
        let mut r = cfa::BitReader::new(&data);
        assert_eq!(r.read_bits(4), 0b1010);
        assert_eq!(r.read_bits(4), 0b0110);
        assert_eq!(r.read_bits(8), 0b1100_1111);
        // Past end -> zero bits.
        assert_eq!(r.read_bits(4), 0);

        // 12-bit reads crossing byte boundaries, matching readBits(12).
        let data = [0xFF, 0xF0, 0x00];
        let mut r = cfa::BitReader::new(&data);
        assert_eq!(r.read_bits(12), 0xFFF);
        assert_eq!(r.read_bits(12), 0x000);

        // skip_bits advances the cursor.
        let data = [0b1111_0000, 0b1010_1010];
        let mut r = cfa::BitReader::new(&data);
        r.skip_bits(4);
        assert_eq!(r.read_bits(4), 0);
        assert_eq!(r.read_bits(8), 0b1010_1010);
    }

    #[test]
    fn interpolate_single_pixel_special_case() {
        // width == 1 && height == 1: every output byte = (byte) s[0].
        let s = [0x42i16, 0, 0];
        let mut buf = [0u8; 6];
        cfa::interpolate(&s, &mut buf, &[1, 0, 2, 1], 1, 1, true);
        assert_eq!(buf, [0x42; 6]);
    }

    #[test]
    fn interpolate_fills_missing_components() {
        // 2x2 image, color map {1,0,2,1} (G R / B G), the Canon/DNG default.
        // Planar source [R|G|B] of 4 shorts each; only the present channel is
        // set at each CFA site, exactly as the Java readers populate `pix`.
        let w = 2usize;
        let h = 2usize;
        let plane = w * h;
        let color_map = [1i32, 0, 2, 1];

        // CFA layout by index (row%2)*2 + (col%2):
        //   (0,0)->1=G  (0,1)->0=R
        //   (1,0)->2=B  (1,1)->1=G
        let mut s = vec![0i16; plane * 3];
        // Greens at (0,0) and (1,1).
        s[plane] = 10; // G(0,0): channel 1, row 0, col 0
        s[plane + 1 * w + 1] = 40; // G(1,1)
                                   // Red at (0,1).
        s[1] = 20; // R(0,1): channel 0, row 0, col 1
                   // Blue at (1,0).
        s[2 * plane + 1 * w + 0] = 30; // B(1,0)

        let mut buf = vec![0u8; plane * 3 * 2];
        cfa::interpolate(&s, &mut buf, &color_map, w, h, true);

        // Helper to read an interleaved RGB sample (little-endian u16 -> i16).
        let px = |buf: &[u8], row: usize, col: usize, c: usize| -> i16 {
            let base = row * w * 6 + col * 6 + c * 2;
            i16::from_le_bytes([buf[base], buf[base + 1]])
        };

        // Present components are passed through unchanged.
        assert_eq!(px(&buf, 0, 0, 1), 10); // G present at (0,0)
        assert_eq!(px(&buf, 0, 1, 0), 20); // R present at (0,1)
        assert_eq!(px(&buf, 1, 0, 2), 30); // B present at (1,0)
        assert_eq!(px(&buf, 1, 1, 1), 40); // G present at (1,1)

        // Missing green at (0,1): neighbours in green plane are (0,0)=10 and
        // (1,1)=40 -> (10+40)/2 = 25.
        assert_eq!(px(&buf, 0, 1, 1), 25);
        // Missing red at (0,0): index 0, need_red && need_blue so !need_blue is
        // false. even_col && bayer_pattern[index+1]==0 (pattern[1]==0) -> the
        // horizontal branch: col==0 so only the right neighbour R(0,1)=20 is
        // summed (ncomps==1) -> 20.
        assert_eq!(px(&buf, 0, 0, 0), 20);
    }

    // -----------------------------------------------------------------------
    // Photoshop IMAGE_SOURCE_DATA layer-block parsing
    // -----------------------------------------------------------------------

    #[test]
    fn photoshop_clean_layer_name_strips_non_ascii_and_trims() {
        // Mirrors Java replaceAll("[^\\p{ASCII}]", "").trim().
        assert_eq!(photoshop_clean_layer_name(b"  Layer 1  "), "Layer 1");
        let mixed = [b'A', 0xC3, 0xA9, b'B', b' '];
        assert_eq!(photoshop_clean_layer_name(&mixed), "AB");
        assert_eq!(photoshop_clean_layer_name(b""), "");
    }

    #[test]
    fn ps_tag_reads_respect_endianness_and_clamp() {
        let data = [0x01, 0x02, 0x03, 0x04, b'A', b'B'];
        let mut le = PsTag::new(&data, true);
        assert_eq!(le.read_int(), 0x04030201);
        assert_eq!(le.read_string(2), b"AB");
        // Past the end yields zero / empty, never panics.
        assert_eq!(le.read_short(), 0);
        assert_eq!(le.read_string(4), b"");

        let mut be = PsTag::new(&data, false);
        assert_eq!(be.read_int(), 0x01020304);
        assert_eq!(be.read(), b'A');
        be.skip_bytes(100);
        assert_eq!(be.fp(), be.len());
    }

    #[test]
    fn ps_tag_read_cstring_consumes_terminator() {
        let data = [b'P', b'h', b'o', b't', 0, b'X', b'Y'];
        let mut tag = PsTag::new(&data, false);
        tag.read_cstring();
        // Positioned just past the NUL terminator.
        assert_eq!(tag.read_string(2), b"XY");
    }

    #[test]
    fn photoshop_layer_block_yields_named_layer_metadata() {
        // Hand-build a little-endian IMAGE_SOURCE_DATA payload (matching an
        // uninitialised TiffReader's default endianness) with one Layr block
        // carrying a single-channel layer named "Backgrnd". The name length is
        // a multiple of 4 (pad == 0) so it survives Java's acceptance check
        // `cleanedLength == nameLength + pad` (trim removes NUL padding, so a
        // padded name would fail that length comparison and be skipped).
        let name = b"Backgrnd";
        let name_len = name.len(); // 8
        let pad = (4 - (name_len % 4)) % 4; // 0

        let mut layer = Vec::new();
        // bounds: top, left, bottom, right -> 4 x 4
        layer.extend_from_slice(&0i32.to_le_bytes());
        layer.extend_from_slice(&0i32.to_le_bytes());
        layer.extend_from_slice(&4i32.to_le_bytes());
        layer.extend_from_slice(&4i32.to_le_bytes());
        // sizeC = 1 (single channel passes the !RGB guard)
        layer.extend_from_slice(&1i16.to_le_bytes());
        // one channel: channelID + dataSize
        layer.extend_from_slice(&0i16.to_le_bytes());
        layer.extend_from_slice(&16i32.to_le_bytes());
        // skip 12 (blend mode signature + key + opacity/clipping/flags/filler)
        layer.extend_from_slice(&[0u8; 12]);
        // extra-data: build the name section first to size `len`.
        let mut extra = Vec::new();
        extra.extend_from_slice(&0i32.to_le_bytes()); // mask == 0
        extra.extend_from_slice(&0i32.to_le_bytes()); // blending == 0
        extra.push(name_len as u8);
        extra.extend_from_slice(name);
        extra.extend_from_slice(&vec![0u8; pad]);
        layer.extend_from_slice(&(extra.len() as i32).to_le_bytes());
        layer.extend_from_slice(&extra);

        let mut block = Vec::new();
        block.extend_from_slice(b"8BIM"); // signature
        block.extend_from_slice(b"ryaL"); // type ("Layr" reversed)
        let body = {
            let mut body = Vec::new();
            body.extend_from_slice(&1i16.to_le_bytes()); // nLayers = 1
            body.extend_from_slice(&layer);
            body
        };
        block.extend_from_slice(&(body.len() as i32).to_le_bytes());
        block.extend_from_slice(&body);

        let mut payload = Vec::new();
        payload.extend_from_slice(b"8BPS\0"); // leading C-string
        payload.extend_from_slice(&block);
        // trailing slack so the while-loop terminates cleanly past the block.
        payload.extend_from_slice(&[0u8; 16]);

        let mut reader = PhotoshopTiffReader::new();
        // Drive init_file directly against the synthetic payload (little-endian).
        reader.init_file(&payload);

        assert_eq!(reader.layer_names, vec!["Backgrnd".to_string()]);
        assert_eq!(reader.series_count(), 2);
        let meta = reader.metadata();
        match meta.series_metadata.get("Layer name #1") {
            Some(MetadataValue::String(value)) => assert_eq!(value, "Backgrnd"),
            other => panic!("unexpected Layer name #1 metadata: {other:?}"),
        }
        match meta.series_metadata.get("Photoshop layer count") {
            Some(MetadataValue::Int(value)) => assert_eq!(*value, 1),
            other => panic!("unexpected layer-count metadata: {other:?}"),
        }
        reader.set_series(1).unwrap();
        assert_eq!(
            (
                reader.metadata().size_x,
                reader.metadata().size_y,
                reader.metadata().size_c,
                reader.metadata().image_count,
            ),
            (4, 4, 1, 1)
        );
        assert!(matches!(
            reader.metadata().series_metadata.get("ImageName"),
            Some(MetadataValue::String(value)) if value == "Backgrnd"
        ));
    }

    #[test]
    fn photoshop_layer_block_rejects_padded_name_like_java() {
        // Java compares layerNames[layer].length() after ASCII stripping and
        // trim() to nameLength + pad. A padded Pascal string is therefore not
        // accepted because trim() removes the NUL padding.
        let name = b"Layer"; // name_len 5 => pad 3
        let name_len = name.len();
        let pad = (4 - (name_len % 4)) % 4;

        let mut layer = Vec::new();
        layer.extend_from_slice(&0i32.to_le_bytes()); // top
        layer.extend_from_slice(&0i32.to_le_bytes()); // left
        layer.extend_from_slice(&4i32.to_le_bytes()); // bottom
        layer.extend_from_slice(&4i32.to_le_bytes()); // right
        layer.extend_from_slice(&1i16.to_le_bytes()); // sizeC
        layer.extend_from_slice(&0i16.to_le_bytes()); // channelID
        layer.extend_from_slice(&16i32.to_le_bytes()); // dataSize
        layer.extend_from_slice(&[0u8; 12]);

        let mut extra = Vec::new();
        extra.extend_from_slice(&0i32.to_le_bytes()); // mask == 0
        extra.extend_from_slice(&0i32.to_le_bytes()); // blending == 0
        extra.push(name_len as u8);
        extra.extend_from_slice(name);
        extra.extend_from_slice(&vec![0u8; pad]);
        layer.extend_from_slice(&(extra.len() as i32).to_le_bytes());
        layer.extend_from_slice(&extra);

        let mut body = Vec::new();
        body.extend_from_slice(&1i16.to_le_bytes());
        body.extend_from_slice(&layer);

        let mut payload = Vec::new();
        payload.extend_from_slice(b"8BPS\0");
        payload.extend_from_slice(b"8BIM");
        payload.extend_from_slice(b"ryaL");
        payload.extend_from_slice(&(body.len() as i32).to_le_bytes());
        payload.extend_from_slice(&body);
        payload.extend_from_slice(&[0u8; 16]);

        let mut reader = PhotoshopTiffReader::new();
        reader.init_file(&payload);

        assert!(reader.layer_names.is_empty());
        match reader
            .metadata()
            .series_metadata
            .get("Photoshop layer count")
        {
            Some(MetadataValue::Int(value)) => assert_eq!(*value, 0),
            other => panic!("unexpected layer-count metadata: {other:?}"),
        }
        assert!(!reader
            .metadata()
            .series_metadata
            .contains_key("Layer name #1"));
    }

    #[test]
    fn interpolate_matches_planar_passthrough_when_fully_sampled() {
        // If every CFA site already carries all three channels (degenerate but
        // exercises the "else" passthrough branches), interpolate must copy the
        // present component for each channel verbatim.
        let w = 2usize;
        let h = 2usize;
        let plane = w * h;
        let color_map = [1i32, 0, 2, 1];
        let mut s = vec![0i16; plane * 3];
        for p in 0..plane {
            s[p] = (p as i16) + 1; // R plane
            s[plane + p] = (p as i16) + 11; // G plane
            s[2 * plane + p] = (p as i16) + 21; // B plane
        }
        let mut buf = vec![0u8; plane * 3 * 2];
        cfa::interpolate(&s, &mut buf, &color_map, w, h, true);
        let px = |buf: &[u8], idx: usize, c: usize| -> i16 {
            let base = idx * 6 + c * 2;
            i16::from_le_bytes([buf[base], buf[base + 1]])
        };
        for (row, col, idx) in [(0usize, 0usize, 0usize), (0, 1, 1), (1, 0, 2), (1, 1, 3)] {
            let cfa_index = (row % 2) * 2 + (col % 2);
            // The channel present at this site is copied straight through.
            let present = color_map[cfa_index] as usize;
            let expected_plane_val = s[present * plane + idx];
            assert_eq!(px(&buf, idx, present), expected_plane_val);
        }
    }

    // -----------------------------------------------------------------------
    // Nikon NEF reader tests
    // -----------------------------------------------------------------------

    fn push_u16_le(data: &mut Vec<u8>, value: u16) {
        data.extend_from_slice(&value.to_le_bytes());
    }
    fn push_u32_le(data: &mut Vec<u8>, value: u32) {
        data.extend_from_slice(&value.to_le_bytes());
    }
    fn push_ifd_entry(data: &mut Vec<u8>, tag: u16, type_code: u16, count: u32, value: u32) {
        push_u16_le(data, tag);
        push_u16_le(data, type_code);
        push_u32_le(data, count);
        push_u32_le(data, value);
    }

    /// Build a minimal single-IFD classic TIFF describing an 8-bit RGB strip,
    /// optionally tagging it as a Nikon NEF via `Make` or the EPS-standard tag.
    /// `pixels` is the raw RGB strip (interleaved, 3 bytes/pixel).
    fn synthetic_nef(
        width: u32,
        height: u32,
        pixels: &[u8],
        make_nikon: bool,
        eps: bool,
    ) -> Vec<u8> {
        use crate::tiff::ifd::tag;
        // Layout: header(8) | IFD | "Nikon\0" make string | strip data.
        let mut entries: Vec<(u16, u16, u32, u32)> = Vec::new();
        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        push_u16_le(&mut data, 42);

        let make_str: &[u8] = b"Nikon\0"; // 6 bytes
                                          // 8 fixed entries + optional Make + optional EPS-standard.
        let entry_count: u16 = 8 + make_nikon as u16 + eps as u16;
        let ifd_offset = 8u32;
        let ifd_bytes = 2 + entry_count as u32 * 12 + 4;
        let make_offset = ifd_offset + ifd_bytes;
        // The Make string only occupies file space when present.
        let make_bytes = if make_nikon { make_str.len() as u32 } else { 0 };
        let strip_offset = make_offset + make_bytes;

        entries.push((tag::IMAGE_WIDTH, 4, 1, width));
        entries.push((tag::IMAGE_LENGTH, 4, 1, height));
        entries.push((tag::BITS_PER_SAMPLE, 3, 1, 8));
        entries.push((tag::COMPRESSION, 3, 1, 1)); // uncompressed
        entries.push((tag::PHOTOMETRIC_INTERPRETATION, 3, 1, 2)); // RGB
        entries.push((tag::STRIP_OFFSETS, 4, 1, strip_offset));
        entries.push((tag::SAMPLES_PER_PIXEL, 3, 1, 3));
        entries.push((tag::STRIP_BYTE_COUNTS, 4, 1, pixels.len() as u32));
        if make_nikon {
            entries.push((NikonReader::MAKE, 2, make_str.len() as u32, make_offset));
        }
        if eps {
            entries.push((NikonReader::TIFF_EPS_STANDARD, 3, 1, 1));
        }
        entries.sort_by_key(|e| e.0); // TIFF requires ascending tag order
        let entry_count = entries.len() as u16;

        push_u32_le(&mut data, ifd_offset);
        push_u16_le(&mut data, entry_count);
        for (t, ty, c, v) in &entries {
            push_ifd_entry(&mut data, *t, *ty, *c, *v);
        }
        push_u32_le(&mut data, 0); // next IFD = 0
        if make_nikon {
            data.extend_from_slice(make_str);
        }
        data.extend_from_slice(pixels);
        data
    }

    #[test]
    fn nikon_detects_by_extension_and_eps_and_make_tag() {
        let reader = NikonReader::new();
        assert!(reader.is_this_type_by_name(Path::new("photo.NEF")));
        assert!(reader.is_this_type_by_name(Path::new("photo.nef")));
        assert!(!reader.is_this_type_by_name(Path::new("photo.tif")));

        let eps_tiff = synthetic_nef(2, 1, &[1, 2, 3, 4, 5, 6], false, true);
        assert!(reader.is_this_type_by_bytes(&eps_tiff));

        let make_tiff = synthetic_nef(2, 1, &[1, 2, 3, 4, 5, 6], true, false);
        assert!(reader.is_this_type_by_bytes(&make_tiff));

        // Plain TIFF with neither marker is rejected.
        let plain = synthetic_nef(2, 1, &[1, 2, 3, 4, 5, 6], false, false);
        assert!(!reader.is_this_type_by_bytes(&plain));
    }

    #[test]
    fn nikon_parses_metadata_and_reads_uncompressed_rgb_plane() {
        let root = temp_dir("nikon_rgb");
        let w = 2u32;
        let h = 2u32;
        // 2x2 interleaved RGB, distinct values per channel/pixel.
        let pixels: Vec<u8> = (0..(w * h * 3) as u8).collect();
        let nef = synthetic_nef(w, h, &pixels, true, false);
        let path = root.join("frame.nef");
        fs::write(&path, &nef).unwrap();

        let mut reader = NikonReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (w, h));
        assert_eq!(meta.size_z, 1);
        assert_eq!(meta.size_t, 1);
        assert_eq!(meta.image_count, 1);
        assert!(meta.is_rgb);
        assert_eq!(meta.size_c, 3);
        assert_eq!(reader.series_count(), 1);
        match meta.series_metadata.get("format") {
            Some(MetadataValue::String(v)) => assert_eq!(v, "Nikon NEF"),
            other => panic!("unexpected format metadata: {other:?}"),
        }

        // total strip bytes == plane size, so Java/our path defers to the plain
        // TIFF strip decode. Bio-Formats exposes chunky RGB as channel-planar.
        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(plane, vec![0, 3, 6, 9, 1, 4, 7, 10, 2, 5, 8, 11]);

        fs::remove_dir_all(root).unwrap();
    }
}
