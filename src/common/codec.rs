use super::error::{BioFormatsError, Result};

/// Decompress LZW-encoded data (TIFF variant — horizontal differencing applied separately).
pub fn decompress_lzw(data: &[u8]) -> Result<Vec<u8>> {
    use weezl::{decode::Decoder, BitOrder};
    let mut decoder = Decoder::with_tiff_size_switch(BitOrder::Msb, 8);
    decoder
        .decode(data)
        .map_err(|e| BioFormatsError::Codec(e.to_string()))
}

/// Decompress Deflate/Zlib data (TIFF compression 8 = Deflate, 32946 = deflate without header).
pub fn decompress_deflate(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::ZlibDecoder;
    use std::io::Read;
    let mut decoder = ZlibDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).map_err(BioFormatsError::Io)?;
    Ok(out)
}

/// Decompress a bzip2 stream.
///
/// The input is a complete, standard bzip2 stream including the leading "BZh"
/// magic. Note: Java's `OMEXMLReader` strips the first two bytes before feeding
/// the data to Apache Ant's `CBZip2InputStream` (a quirk of that constructor);
/// a standard decoder like this one must NOT strip them.
///
/// Uses the `bzip2` crate with its default pure-Rust `libbz2-rs-sys` backend,
/// which correctly decodes multi-block streams (real pixel planes span many
/// blocks). The pure-Rust `bzip2-rs` crate was tried first but only ever
/// decodes the first block, failing with "huffman bitstream truncated" on
/// anything larger than one block (~900 KB).
pub fn decompress_bzip2(data: &[u8]) -> Result<Vec<u8>> {
    use bzip2::read::BzDecoder;
    use std::io::Read;
    let mut decoder = BzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).map_err(BioFormatsError::Io)?;
    Ok(out)
}

/// Decompress raw Deflate (no zlib header).
pub fn decompress_deflate_raw(data: &[u8]) -> Result<Vec<u8>> {
    use flate2::read::DeflateDecoder;
    use std::io::Read;
    let mut decoder = DeflateDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out).map_err(BioFormatsError::Io)?;
    Ok(out)
}

/// Decompress PackBits run-length encoding (TIFF compression 32773).
pub fn decompress_packbits(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < data.len() {
        let header = data[i] as i8;
        i += 1;
        if header >= 0 {
            // Copy (header+1) literal bytes
            let count = (header as usize) + 1;
            if i + count > data.len() {
                return Err(BioFormatsError::InvalidData(
                    "PackBits: literal run overruns input".into(),
                ));
            }
            out.extend_from_slice(&data[i..i + count]);
            i += count;
        } else if header != -128 {
            // Repeat next byte (-header+1) times
            let count = (-header as usize) + 1;
            if i >= data.len() {
                return Err(BioFormatsError::InvalidData(
                    "PackBits: repeat run missing byte".into(),
                ));
            }
            let byte = data[i];
            i += 1;
            for _ in 0..count {
                out.push(byte);
            }
        }
        // header == -128: NOP
    }
    Ok(out)
}

/// Decompress JPEG data (both lossy and lossless/SOF3 variants).
///
/// Standard baseline/progressive streams are decoded with `zune-jpeg` (faster,
/// and numerically closer to libjpeg-turbo). Anything `zune-jpeg` cannot handle
/// — lossless SOF3, CMYK/YCCK, Adobe-RGB, 16-bit, or any decode error — falls
/// back to `jpeg-decoder`, which is the authoritative path for those cases. The
/// output byte layout is identical to the previous `jpeg-decoder`-only path:
/// interleaved RGB for YCbCr 3-component input, single-byte grayscale for
/// 1-component input.
pub fn decompress_jpeg(data: &[u8]) -> Result<Vec<u8>> {
    if let Some(out) = try_decompress_jpeg_zune(data) {
        return Ok(out);
    }
    decompress_jpeg_fallback(data)
}

/// Decode with `jpeg-decoder` (default color transform). The authoritative path
/// for everything `zune-jpeg` does not handle.
pub(crate) fn decompress_jpeg_fallback(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    decoder
        .decode()
        .map_err(|e| BioFormatsError::Codec(e.to_string()))
}

/// Attempt to decode a standard baseline/progressive JPEG with `zune-jpeg`.
///
/// Returns `Some(bytes)` only for the cases whose output layout provably matches
/// `jpeg-decoder`'s default: 1-component (Luma → 1 byte/px) and 3-component
/// YCbCr (→ interleaved RGB). For any other input colorspace (RGB/CMYK/YCCK),
/// lossless SOF3 (which `zune-jpeg` does not support), or any decode error,
/// returns `None` so the caller falls back to `jpeg-decoder`.
pub(crate) fn try_decompress_jpeg_zune(data: &[u8]) -> Option<Vec<u8>> {
    use zune_core::bytestream::ZCursor;
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;

    let mut decoder = zune_jpeg::JpegDecoder::new(ZCursor::new(data));
    if decoder.decode_headers().is_err() {
        return None;
    }
    let info = decoder.info()?;
    let out_colorspace = match (decoder.input_colorspace()?, info.components) {
        (ColorSpace::Luma, 1) => ColorSpace::Luma,
        (ColorSpace::YCbCr, 3) => ColorSpace::RGB,
        // RGB/CMYK/YCCK/other: defer to jpeg-decoder to preserve exact bytes.
        _ => return None,
    };
    decoder.set_options(DecoderOptions::default().jpeg_set_out_colorspace(out_colorspace));
    decoder.decode().ok()
}

/// Decompress Zstd data.
pub fn decompress_zstd(data: &[u8]) -> Result<Vec<u8>> {
    zstd_decode_all(data)
}

/// Decompress a Zstd frame using the pure-Rust `zstd-pure-rs` backend.
///
/// Mirrors `zstd::decode_all` for the single-frame payloads produced by the
/// formats we support (TIFF Zstd tiles, CZI Zstd/Zstd_1).
pub(crate) fn zstd_decode_all(data: &[u8]) -> Result<Vec<u8>> {
    use zstd_pure_rs::prelude::*;
    if data.is_empty() {
        return Ok(Vec::new());
    }
    let bound = ZSTD_decompressBound(data);
    if bound == ZSTD_CONTENTSIZE_ERROR || bound == ZSTD_CONTENTSIZE_UNKNOWN {
        return Err(BioFormatsError::Codec(
            "zstd: unable to determine decompressed size".into(),
        ));
    }
    let mut out = vec![0u8; bound as usize];
    let n = ZSTD_decompress(&mut out, data);
    if ERR_isError(n) {
        return Err(BioFormatsError::Codec(format!(
            "zstd decompress failed: {}",
            ZSTD_getErrorName(n)
        )));
    }
    out.truncate(n);
    Ok(out)
}

/// Compress data with Zstd using the pure-Rust `zstd-pure-rs` backend.
#[cfg(test)]
pub(crate) fn zstd_encode_all(data: &[u8], level: i32) -> Result<Vec<u8>> {
    use zstd_pure_rs::prelude::*;
    let mut dst = vec![0u8; ZSTD_compressBound(data.len())];
    let n = ZSTD_compress(&mut dst, data, level);
    if ERR_isError(n) {
        return Err(BioFormatsError::Codec(format!(
            "zstd compress failed: {}",
            ZSTD_getErrorName(n)
        )));
    }
    dst.truncate(n);
    Ok(dst)
}

/// Decode an in-memory PNG payload to interleaved raw pixel bytes.
///
/// Mirrors the Java cellSens `APNGReader` path used for ETS PNG tiles
/// (CellSensReader.java:1198-1210). Pixel bytes are returned little-endian and
/// channel-interleaved (e.g. RGBRGB...), matching what the ETS tile-stitcher
/// expects. 16-bit channels are emitted as 2 little-endian bytes per sample.
pub fn decompress_png(data: &[u8]) -> Result<Vec<u8>> {
    decode_image_memory(data, image::ImageFormat::Png)
}

/// Decode an in-memory BMP payload to interleaved raw pixel bytes.
///
/// Mirrors the Java cellSens `BMPReader` path used for ETS BMP tiles
/// (CellSensReader.java:1201-1210). See [`decompress_png`] for the output byte
/// layout.
pub fn decompress_bmp(data: &[u8]) -> Result<Vec<u8>> {
    decode_image_memory(data, image::ImageFormat::Bmp)
}

/// Decode an in-memory image payload of a known format to interleaved raw pixel
/// bytes (little-endian). Shared backend for [`decompress_png`]/[`decompress_bmp`].
fn decode_image_memory(data: &[u8], format: image::ImageFormat) -> Result<Vec<u8>> {
    let img = image::load_from_memory_with_format(data, format)
        .map_err(|e| BioFormatsError::Codec(e.to_string()))?;
    Ok(match img {
        image::DynamicImage::ImageLuma8(b) => b.into_raw(),
        image::DynamicImage::ImageLumaA8(b) => b.into_raw(),
        image::DynamicImage::ImageRgb8(b) => b.into_raw(),
        image::DynamicImage::ImageRgba8(b) => b.into_raw(),
        image::DynamicImage::ImageLuma16(b) => {
            b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect()
        }
        image::DynamicImage::ImageLumaA16(b) => {
            b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect()
        }
        image::DynamicImage::ImageRgb16(b) => {
            b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect()
        }
        image::DynamicImage::ImageRgba16(b) => {
            b.into_raw().iter().flat_map(|v| v.to_le_bytes()).collect()
        }
        other => other.to_rgb8().into_raw(),
    })
}

/// Decompress JPEG 2000 data (JP2 or J2K codestream).
pub fn decompress_jpeg2000(data: &[u8]) -> Result<Vec<u8>> {
    use jpeg2k::Image as J2kImage;
    let image = J2kImage::from_bytes(data)
        .map_err(|e| BioFormatsError::Codec(format!("JPEG 2000: {e}")))?;
    let components = image.components();
    if components.is_empty() {
        return Err(BioFormatsError::Codec("JPEG 2000: no components".into()));
    }
    let width = components[0].width() as usize;
    let height = components[0].height() as usize;
    let n_components = components.len();
    let first_precision = components[0].precision();
    let component_pixels = width.checked_mul(height).ok_or_else(|| {
        BioFormatsError::Codec("JPEG 2000: component dimensions are too large".into())
    })?;

    for (idx, component) in components.iter().enumerate() {
        if component.width() as usize != width || component.height() as usize != height {
            return Err(BioFormatsError::Codec(format!(
                "JPEG 2000: unsupported component geometry mismatch at component {idx}"
            )));
        }
        if component.precision() != first_precision {
            return Err(BioFormatsError::Codec(format!(
                "JPEG 2000: unsupported component precision mismatch at component {idx}"
            )));
        }
        if component.data().len() < component_pixels {
            return Err(BioFormatsError::Codec(format!(
                "JPEG 2000: component {idx} data is shorter than its geometry"
            )));
        }
    }

    // Determine bytes per sample from the first component's precision.
    // jpeg2k component samples are i32; narrow by taking the low `bps` bytes of
    // the two's-complement representation (preserving sign bits for signed data),
    // rather than casting i32 -> u16 which would silently drop bits for any
    // precision > 16 that happened to fall into the 2-byte path.
    let prec = first_precision as usize;
    let bps = if prec <= 8 {
        1
    } else if prec <= 16 {
        2
    } else {
        4
    };

    let out_len = component_pixels
        .checked_mul(n_components)
        .and_then(|v| v.checked_mul(bps))
        .ok_or_else(|| BioFormatsError::Codec("JPEG 2000: decoded image is too large".into()))?;
    let mut out = Vec::with_capacity(out_len);
    // Interleave components pixel by pixel (RGBRGB...)
    for y in 0..height {
        for x in 0..width {
            for c in 0..n_components {
                let val = components[c].data()[y * width + x];
                // Take the low `bps` bytes of the little-endian i32 encoding.
                let bytes = val.to_le_bytes();
                out.extend_from_slice(&bytes[..bps]);
            }
        }
    }
    Ok(out)
}

/// Compress an interleaved pixel plane to a lossless JPEG 2000 (`.jp2`) file.
///
/// Requires the `jpeg2000-write` feature (default-on). Uses the pure-Rust
/// `openjp2` encoder (OpenJPEG port). Mirrors the lossless output semantics of
/// Java `JPEG2000Writer` (`irreversible = 0`, single quality layer, rate 0).
///
/// `pixels` is component-interleaved (e.g. `RGBRGB…` for 3 components), little-
/// endian, with `precision` bits per sample stored in `(precision+7)/8` bytes.
/// The result is written directly to `path` (the openjp2 file stream).
#[cfg(feature = "jpeg2000-write")]
pub fn compress_jpeg2000(
    pixels: &[u8],
    width: u32,
    height: u32,
    components: u32,
    precision: u32,
    signed: bool,
    path: &std::path::Path,
) -> Result<()> {
    use openjp2::image::opj_image_cmptparm_t;
    use openjp2::openjpeg::*;
    use std::ffi::CString;

    if width == 0 || height == 0 || components == 0 {
        return Err(BioFormatsError::Codec(
            "JPEG 2000 encode: zero-sized image".into(),
        ));
    }
    if components != 1 && components != 3 {
        return Err(BioFormatsError::Codec(format!(
            "JPEG 2000 encode: only 1 (gray) or 3 (RGB) components supported, got {components}"
        )));
    }
    if precision == 0 || precision > 32 {
        return Err(BioFormatsError::Codec(format!(
            "JPEG 2000 encode: unsupported precision {precision}"
        )));
    }

    let w = width as usize;
    let h = height as usize;
    let nc = components as usize;
    let bps = ((precision + 7) / 8) as usize;
    let npix = w
        .checked_mul(h)
        .ok_or_else(|| BioFormatsError::Codec("JPEG 2000 encode: image too large".into()))?;
    let expected = npix
        .checked_mul(nc)
        .and_then(|v| v.checked_mul(bps))
        .ok_or_else(|| BioFormatsError::Codec("JPEG 2000 encode: image too large".into()))?;
    if pixels.len() < expected {
        return Err(BioFormatsError::Codec(format!(
            "JPEG 2000 encode: expected {expected} bytes, got {}",
            pixels.len()
        )));
    }

    // De-interleave the source plane into one i32 sample buffer per component,
    // sign-extending when the data is signed.
    let mut planes: Vec<Vec<i32>> = vec![vec![0i32; npix]; nc];
    for p in 0..npix {
        for c in 0..nc {
            let off = (p * nc + c) * bps;
            let mut raw: u32 = 0;
            for b in 0..bps {
                raw |= (pixels[off + b] as u32) << (8 * b);
            }
            let val = if signed && precision < 32 {
                let sign_bit = 1u32 << (precision - 1);
                if raw & sign_bit != 0 {
                    (raw | !((1u32 << precision) - 1)) as i32
                } else {
                    raw as i32
                }
            } else {
                raw as i32
            };
            planes[c][p] = val;
        }
    }

    let color_space = if nc == 1 {
        OPJ_CLRSPC_GRAY
    } else {
        OPJ_CLRSPC_SRGB
    };
    let sgnd = if signed { 1u32 } else { 0u32 };

    // SAFETY: every successfully created openjpeg resource is destroyed before
    // returning on every path (success or error) below.
    unsafe {
        let mut cmptparms = vec![
            opj_image_cmptparm_t {
                dx: 1,
                dy: 1,
                w: width,
                h: height,
                x0: 0,
                y0: 0,
                prec: precision,
                bpp: precision,
                sgnd,
            };
            nc
        ];

        let image = opj_image_create(components, cmptparms.as_mut_ptr(), color_space);
        if image.is_null() {
            return Err(BioFormatsError::Codec(
                "JPEG 2000 encode: opj_image_create failed".into(),
            ));
        }

        let cleanup_image = |image: *mut opj_image_t| opj_image_destroy(image);

        (*image).x0 = 0;
        (*image).y0 = 0;
        (*image).x1 = width;
        (*image).y1 = height;

        // Copy the de-interleaved samples into each component's data buffer.
        let comps = std::slice::from_raw_parts_mut((*image).comps, nc);
        for c in 0..nc {
            let dst = comps[c].data;
            if dst.is_null() {
                cleanup_image(image);
                return Err(BioFormatsError::Codec(
                    "JPEG 2000 encode: component data buffer is null".into(),
                ));
            }
            std::ptr::copy_nonoverlapping(planes[c].as_ptr(), dst, npix);
        }

        let mut params: opj_cparameters_t = std::mem::zeroed();
        opj_set_default_encoder_parameters(&mut params);
        // Lossless: rate-distortion allocation with a single layer at rate 0.
        params.tcp_numlayers = 1;
        params.tcp_rates[0] = 0.0;
        params.cp_disto_alloc = 1;
        params.irreversible = 0;
        params.cod_format = 1; // JP2
        // Number of resolution levels must satisfy 2^(numres-1) <= min(w, h).
        let min_dim = w.min(h) as i32;
        let mut numres = params.numresolution;
        while numres > 1 && (1i32 << (numres - 1)) > min_dim {
            numres -= 1;
        }
        params.numresolution = numres;

        let codec = opj_create_compress(OPJ_CODEC_JP2);
        if codec.is_null() {
            cleanup_image(image);
            return Err(BioFormatsError::Codec(
                "JPEG 2000 encode: opj_create_compress failed".into(),
            ));
        }

        if opj_setup_encoder(codec, &mut params, image) == 0 {
            opj_destroy_codec(codec);
            cleanup_image(image);
            return Err(BioFormatsError::Codec(
                "JPEG 2000 encode: opj_setup_encoder failed".into(),
            ));
        }

        let path_str = path.to_str().ok_or_else(|| {
            opj_destroy_codec(codec);
            cleanup_image(image);
            BioFormatsError::Codec("JPEG 2000 encode: non-UTF-8 output path".into())
        })?;
        let c_path = CString::new(path_str).map_err(|_| {
            opj_destroy_codec(codec);
            cleanup_image(image);
            BioFormatsError::Codec("JPEG 2000 encode: path contains NUL".into())
        })?;

        let stream = opj_stream_create_default_file_stream(c_path.as_ptr(), 0);
        if stream.is_null() {
            opj_destroy_codec(codec);
            cleanup_image(image);
            return Err(BioFormatsError::Codec(
                "JPEG 2000 encode: could not open output stream".into(),
            ));
        }

        let mut ok = opj_start_compress(codec, image, stream) != 0;
        ok = ok && opj_encode(codec, stream) != 0;
        ok = ok && opj_end_compress(codec, stream) != 0;

        opj_stream_destroy(stream);
        opj_destroy_codec(codec);
        cleanup_image(image);

        if !ok {
            return Err(BioFormatsError::Codec(
                "JPEG 2000 encode: openjp2 compression failed".into(),
            ));
        }
    }

    Ok(())
}

/// Decompress JPEG-XR data.
///
/// Requires the `jpegxr` feature flag: `cargo build --features jpegxr`
#[cfg(feature = "jpegxr")]
pub fn decompress_jpegxr(data: &[u8]) -> Result<Vec<u8>> {
    use std::io::Cursor;
    let cursor = Cursor::new(data);
    let mut decoder = jpegxr::ImageDecode::with_reader(cursor)
        .map_err(|e| BioFormatsError::Codec(format!("JPEG-XR: {e}")))?;
    let (width, height) = decoder
        .get_size()
        .map_err(|e| BioFormatsError::Codec(format!("JPEG-XR size: {e}")))?;
    let format = decoder
        .get_pixel_format()
        .map_err(|e| BioFormatsError::Codec(format!("JPEG-XR format: {e}")))?;

    // Determine bytes per pixel from the pixel format
    let bpp: usize = match format {
        jpegxr::PixelFormat::PixelFormat8bppGray => 1,
        jpegxr::PixelFormat::PixelFormat16bppGray => 2,
        jpegxr::PixelFormat::PixelFormat32bppGrayFloat => 4,
        jpegxr::PixelFormat::PixelFormat24bppRGB => 3,
        jpegxr::PixelFormat::PixelFormat24bppBGR => 3,
        jpegxr::PixelFormat::PixelFormat32bppBGRA => 4,
        jpegxr::PixelFormat::PixelFormat32bppRGBA => 4,
        jpegxr::PixelFormat::PixelFormat48bppRGB => 6,
        jpegxr::PixelFormat::PixelFormat64bppRGBA => 8,
        _ => 3, // fallback: assume 3 bytes per pixel
    };
    let row_bytes = width as usize * bpp;
    let stride = (row_bytes + 3) & !3; // 4-byte aligned
    let mut buf = vec![0u8; stride * height as usize];
    decoder
        .copy_all(&mut buf, stride)
        .map_err(|e| BioFormatsError::Codec(format!("JPEG-XR decode: {e}")))?;

    // Remove stride padding if needed
    if stride != row_bytes {
        let mut out = Vec::with_capacity(row_bytes * height as usize);
        for y in 0..height as usize {
            out.extend_from_slice(&buf[y * stride..y * stride + row_bytes]);
        }
        Ok(out)
    } else {
        Ok(buf)
    }
}

/// Placeholder for JPEG-XR when the feature is not enabled.
#[cfg(not(feature = "jpegxr"))]
pub fn decompress_jpegxr(_data: &[u8]) -> Result<Vec<u8>> {
    Err(BioFormatsError::UnsupportedFormat(
        "JPEG-XR support requires the 'jpegxr' feature: cargo build --features jpegxr".into(),
    ))
}

// ---- CCITT fax compression ----

/// Decompress CCITT Group 3 (T.4) 1-bit fax compression.
///
/// This implements the 1-dimensional Modified Huffman mode used by baseline
/// TIFF Group 3 strips. Bits are read most-significant-bit first (TIFF
/// FillOrder 1), and output pixels are packed one bit per pixel with white as 0
/// and black as 1.
pub fn decompress_ccitt_group3(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }

    let row_bytes = width.div_ceil(8);
    let mut out = vec![0u8; row_bytes * height];
    let mut bits = MsbBitReader::new(data);

    for row in 0..height {
        bits.skip_ccitt_eols();
        let mut x = 0usize;
        let mut black = false;

        while x < width {
            let mut run = 0usize;
            loop {
                match decode_ccitt_run(&mut bits, black)? {
                    CcittCode::Run(len) => {
                        run += len as usize;
                        if len < 64 {
                            break;
                        }
                    }
                    CcittCode::Eol => {
                        if x == 0 {
                            continue;
                        }
                        return Err(BioFormatsError::InvalidData(
                            "CCITT Group 3: EOL before row is complete".into(),
                        ));
                    }
                }
            }

            if x + run > width {
                return Err(BioFormatsError::InvalidData(
                    "CCITT Group 3: run exceeds row width".into(),
                ));
            }
            if black {
                for px in x..x + run {
                    out[row * row_bytes + px / 8] |= 0x80 >> (px % 8);
                }
            }
            x += run;
            black = !black;
        }
    }

    Ok(out)
}

/// Decompress CCITT Group 4 (T.6) 1-bit fax compression.
///
/// This implements the two-dimensional T.6 mode used by TIFF Group 4 strips.
/// Bits are read most-significant-bit first (TIFF FillOrder 1), and output
/// pixels are packed one bit per pixel with white as 0 and black as 1.
pub fn decompress_ccitt_group4(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }

    let row_bytes = width.div_ceil(8);
    let mut out = vec![0u8; row_bytes * height];
    let mut bits = MsbBitReader::new(data);
    let mut reference = vec![false; width];

    for row in 0..height {
        let mut coding = vec![false; width];
        let mut x = 0usize;
        let mut black = false;

        while x < width {
            match decode_group4_mode(&mut bits)? {
                Group4Mode::Pass => {
                    let (_, b2) = group4_reference_changing_elements(&reference, x, black);
                    if b2 < x {
                        return Err(BioFormatsError::InvalidData(
                            "CCITT Group 4: pass mode moved backwards".into(),
                        ));
                    }
                    if black {
                        coding[x..b2].fill(true);
                    }
                    x = b2;
                }
                Group4Mode::Horizontal => {
                    let run1 = decode_ccitt_run_length(&mut bits, black, "CCITT Group 4")?;
                    let mid = x.checked_add(run1).ok_or_else(|| {
                        BioFormatsError::InvalidData(
                            "CCITT Group 4: horizontal run overflows".into(),
                        )
                    })?;
                    if mid > width {
                        return Err(BioFormatsError::InvalidData(
                            "CCITT Group 4: horizontal run exceeds row width".into(),
                        ));
                    }
                    if black {
                        coding[x..mid].fill(true);
                    }

                    let run2 = decode_ccitt_run_length(&mut bits, !black, "CCITT Group 4")?;
                    let next = mid.checked_add(run2).ok_or_else(|| {
                        BioFormatsError::InvalidData(
                            "CCITT Group 4: horizontal run overflows".into(),
                        )
                    })?;
                    if next > width {
                        return Err(BioFormatsError::InvalidData(
                            "CCITT Group 4: horizontal run exceeds row width".into(),
                        ));
                    }
                    if !black {
                        coding[mid..next].fill(true);
                    }
                    if next == x {
                        return Err(BioFormatsError::InvalidData(
                            "CCITT Group 4: horizontal mode made no progress".into(),
                        ));
                    }
                    x = next;
                }
                Group4Mode::Vertical(offset) => {
                    let (b1, _) = group4_reference_changing_elements(&reference, x, black);
                    let a1 = b1 as isize + offset as isize;
                    if a1 < x as isize || a1 > width as isize {
                        return Err(BioFormatsError::InvalidData(
                            "CCITT Group 4: vertical run exceeds row width".into(),
                        ));
                    }
                    let next = a1 as usize;
                    if black {
                        coding[x..next].fill(true);
                    }
                    x = next;
                    black = !black;
                }
            }
        }

        for (px, &is_black) in coding.iter().enumerate() {
            if is_black {
                out[row * row_bytes + px / 8] |= 0x80 >> (px % 8);
            }
        }
        reference = coding;
    }

    Ok(out)
}

// ---- Video codec stubs ----

const MAX_VIDEO_DECODE_BYTES: usize = 512 * 1024 * 1024;

fn checked_video_output_len(
    codec: &str,
    width: usize,
    height: usize,
    channels: usize,
) -> Result<usize> {
    let len = width
        .checked_mul(height)
        .and_then(|n| n.checked_mul(channels))
        .ok_or_else(|| {
            BioFormatsError::InvalidData(format!("{codec}: output byte count overflows"))
        })?;
    if len > MAX_VIDEO_DECODE_BYTES {
        return Err(BioFormatsError::InvalidData(format!(
            "{codec}: decoded frame is too large"
        )));
    }
    Ok(len)
}

/// Microsoft RLE8 video codec for indexed 8-bit AVI/BMP frames.
pub fn decompress_msrle(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    if width == 0 || height == 0 {
        return Err(BioFormatsError::InvalidData(
            "MSRLE: width and height must be non-zero".into(),
        ));
    }

    let output_len = checked_video_output_len("MSRLE", width, height, 1)?;
    let mut out = vec![0u8; output_len];
    let mut x = 0usize;
    let mut y = height - 1;
    let mut i = 0usize;

    while i + 1 < data.len() {
        let count = data[i] as usize;
        let value = data[i + 1];
        i += 2;

        if count != 0 {
            if x + count > width {
                return Err(BioFormatsError::InvalidData(
                    "MSRLE: encoded run exceeds row width".into(),
                ));
            }
            let row = y * width;
            for px in &mut out[row + x..row + x + count] {
                *px = value;
            }
            x += count;
            continue;
        }

        match value {
            0 => {
                x = 0;
                if y == 0 {
                    break;
                }
                y -= 1;
            }
            1 => break,
            2 => {
                if i + 1 >= data.len() {
                    return Err(BioFormatsError::InvalidData(
                        "MSRLE: delta command missing offsets".into(),
                    ));
                }
                let dx = data[i] as usize;
                let dy = data[i + 1] as usize;
                i += 2;
                x = x.checked_add(dx).ok_or_else(|| {
                    BioFormatsError::InvalidData("MSRLE: delta x overflows".into())
                })?;
                y = y.checked_sub(dy).ok_or_else(|| {
                    BioFormatsError::InvalidData("MSRLE: delta y moves before first row".into())
                })?;
                if x > width {
                    return Err(BioFormatsError::InvalidData(
                        "MSRLE: delta moves past row width".into(),
                    ));
                }
            }
            n => {
                let n = n as usize;
                if i + n > data.len() {
                    return Err(BioFormatsError::InvalidData(
                        "MSRLE: absolute run overruns input".into(),
                    ));
                }
                if x + n > width {
                    return Err(BioFormatsError::InvalidData(
                        "MSRLE: absolute run exceeds row width".into(),
                    ));
                }
                let row = y * width;
                out[row + x..row + x + n].copy_from_slice(&data[i..i + n]);
                x += n;
                i += n;
                if n & 1 == 1 {
                    if i >= data.len() {
                        return Err(BioFormatsError::InvalidData(
                            "MSRLE: absolute run missing pad byte".into(),
                        ));
                    }
                    i += 1;
                }
            }
        }
    }

    Ok(out)
}

/// Microsoft Video 1 (MSVC / CRAM) codec.
///
/// Ported from the Microsoft Video 1 algorithm described at
/// <http://wiki.multimedia.cx/index.php?title=Microsoft_Video_1>, which is the
/// reference the Java `MSVideoCodec` (via `ome.codecs.MSVideoCodec`) follows.
///
/// The image is divided into 4x4 blocks scanned bottom-to-top (block rows are
/// laid out from the bottom of the image upward). Each block is one of:
/// skip (copy from the previous frame — emitted as 0 here since this stateless
/// helper has no prior frame), 1-color (solid), 2-color, or 8-color (each 2x2
/// quadrant has its own 2-color pair).
///
/// Two variants share the same control flow:
/// * 8-bit (`bpp == 1`): each color is a single palette index; output is a
///   width*height index plane (one byte per pixel).
/// * 16-bit (`bpp == 2`): each color is an RGB555 value; output is
///   width*height*3 interleaved RGB bytes.
pub fn decompress_msvideo(data: &[u8], width: u32, height: u32, bpp: u32) -> Result<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    if width == 0 || height == 0 {
        return Err(BioFormatsError::InvalidData(
            "MSVideo: width and height must be non-zero".into(),
        ));
    }
    let out_channels = match bpp {
        1 => 1usize,
        2 => 3usize,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "MSVideo: unsupported bit depth {} (only 8-bit and 16-bit are supported)",
                other * 8
            )))
        }
    };
    let sixteen_bit = bpp == 2;

    let output_len = checked_video_output_len("MSVideo", width, height, out_channels)?;
    let mut out = vec![0u8; output_len];

    // MS Video 1 requires dimensions divisible by 4; round the block grid up so
    // odd sizes still decode (extra pixels are clipped on write).
    let blocks_wide = width.div_ceil(4);
    let blocks_high = height.div_ceil(4);
    let total_blocks = blocks_wide
        .checked_mul(blocks_high)
        .ok_or_else(|| BioFormatsError::InvalidData("MSVideo: block count overflows".into()))?;

    // Writes one color into output pixel (px, py); for 8-bit the color is a
    // palette index, for 16-bit it is an RGB555 value expanded to RGB888.
    // Out-of-bounds pixels (odd dimensions) are clipped.
    let put = |out: &mut [u8], px: usize, py: usize, color: u16| {
        if px >= width || py >= height {
            return;
        }
        if sixteen_bit {
            // RGB555: red bits 14-10, green 9-5, blue 4-0 (5 bits each, scaled
            // to 8 bits). Output order is R, G, B.
            let r = (((color >> 10) & 0x1f) as u32 * 255 / 31) as u8;
            let g = (((color >> 5) & 0x1f) as u32 * 255 / 31) as u8;
            let b = ((color & 0x1f) as u32 * 255 / 31) as u8;
            let off = (py * width + px) * 3;
            out[off] = r;
            out[off + 1] = g;
            out[off + 2] = b;
        } else {
            out[py * width + px] = color as u8;
        }
    };

    // Writes a 4x4 block where each of the 16 pixels selects between two colors
    // according to `flags`. Bit 0 corresponds to the bottom-left pixel; bits run
    // left-to-right then bottom-to-top:
    //   bit indices by position:
    //     12 13 14 15   (top row)
    //      8  9 10 11
    //      4  5  6  7
    //      0  1  2  3   (bottom row)
    // A set bit selects color_a (`ca`); a clear bit selects color_b (`cb`).
    let put_2color =
        |out: &mut [u8], base_x: usize, base_y: usize, flags: u16, ca: u16, cb: u16| {
            for bit in 0..16usize {
                let col = bit % 4;
                let row_from_bottom = bit / 4;
                let px = base_x + col;
                // base_y is the top of the block; bit row 0 is the bottom row.
                let py = base_y + (3 - row_from_bottom);
                let color = if (flags >> bit) & 1 == 1 { ca } else { cb };
                put(out, px, py, color);
            }
        };

    let mut i = 0usize;
    let mut block_index = 0usize; // in encoder (bottom-up) order

    while block_index < total_blocks {
        if i + 2 > data.len() {
            // Out of data: remaining blocks keep the (zeroed) previous frame.
            break;
        }
        let byte_a = data[i];
        let byte_b = data[i + 1];
        let flags = u16::from_le_bytes([byte_a, byte_b]);
        i += 2;

        // Block position: encoder order lays out block rows bottom-up, so block
        // row 0 is the bottom-most strip of the image.
        let block_row_from_bottom = block_index / blocks_wide;
        let block_col = block_index % blocks_wide;
        let block_row = blocks_high - 1 - block_row_from_bottom;
        let base_x = block_col * 4;
        let base_y = block_row * 4;

        // End-of-stream marker.
        if byte_a == 0 && byte_b == 0 {
            break;
        }

        // Skip run: 0x84 <= byte_b <= 0x87, skipping up to 1023 blocks.
        if (0x84..=0x87).contains(&byte_b) {
            let skip = (byte_b as usize - 0x84) * 256 + byte_a as usize;
            block_index += skip.max(1);
            continue;
        }

        if sixteen_bit {
            if byte_b < 0x80 {
                // 2-color or 8-color, determined by the MSB of the first color.
                if i + 2 > data.len() {
                    return Err(BioFormatsError::InvalidData(
                        "MSVideo: 16-bit block missing color".into(),
                    ));
                }
                let first = u16::from_le_bytes([data[i], data[i + 1]]);
                if first & 0x8000 != 0 {
                    // 8-color block: 4 quadrants, each with its own 2-color pair.
                    if i + 16 > data.len() {
                        return Err(BioFormatsError::InvalidData(
                            "MSVideo: 16-bit 8-color block overruns input".into(),
                        ));
                    }
                    let mut colors = [0u16; 8];
                    for c in &mut colors {
                        *c = u16::from_le_bytes([data[i], data[i + 1]]) & 0x7fff;
                        i += 2;
                    }
                    put_8color(&mut out, base_x, base_y, flags, &colors, &put);
                } else {
                    // 2-color block.
                    if i + 4 > data.len() {
                        return Err(BioFormatsError::InvalidData(
                            "MSVideo: 16-bit 2-color block overruns input".into(),
                        ));
                    }
                    let ca = u16::from_le_bytes([data[i], data[i + 1]]);
                    let cb = u16::from_le_bytes([data[i + 2], data[i + 3]]);
                    i += 4;
                    put_2color(&mut out, base_x, base_y, flags, ca, cb);
                }
            } else {
                // 1-color (solid) block: byte_a/byte_b form the 16-bit color.
                let color = flags & 0x7fff;
                for bit in 0..16usize {
                    put(&mut out, base_x + bit % 4, base_y + (3 - bit / 4), color);
                }
            }
            block_index += 1;
            continue;
        }

        // 8-bit mode.
        if byte_b < 0x80 {
            // 2-color block: read color_a, color_b (palette indices).
            if i + 2 > data.len() {
                return Err(BioFormatsError::InvalidData(
                    "MSVideo: 8-bit 2-color block overruns input".into(),
                ));
            }
            let ca = data[i] as u16;
            let cb = data[i + 1] as u16;
            i += 2;
            put_2color(&mut out, base_x, base_y, flags, ca, cb);
        } else if byte_b >= 0x90 {
            // 8-color block: 4 quadrants, each with a 2-color pair (8 bytes).
            if i + 8 > data.len() {
                return Err(BioFormatsError::InvalidData(
                    "MSVideo: 8-bit 8-color block overruns input".into(),
                ));
            }
            let mut colors = [0u16; 8];
            for c in &mut colors {
                *c = data[i] as u16;
                i += 1;
            }
            put_8color(&mut out, base_x, base_y, flags, &colors, &put);
        } else {
            // 1-color (solid) block: byte_a is the palette index for all pixels.
            let color = byte_a as u16;
            for bit in 0..16usize {
                put(&mut out, base_x + bit % 4, base_y + (3 - bit / 4), color);
            }
        }
        block_index += 1;
    }

    Ok(out)
}

/// Writes a Microsoft Video 1 8-color 4x4 block: each 2x2 quadrant uses its own
/// color pair (`colors[2*q]`, `colors[2*q+1]`). `flags` provides the per-pixel
/// selector bits using the same bottom-up bit layout as 2-color blocks.
///
/// Quadrant numbering by position:
///   quad2 | quad3   (top half)
///   quad0 | quad1   (bottom half)
fn put_8color<F: Fn(&mut [u8], usize, usize, u16)>(
    out: &mut [u8],
    base_x: usize,
    base_y: usize,
    flags: u16,
    colors: &[u16; 8],
    put: &F,
) {
    for bit in 0..16usize {
        let col = bit % 4;
        let row_from_bottom = bit / 4;
        // Quadrant: bottom-left=0, bottom-right=1, top-left=2, top-right=3.
        let quad = (if row_from_bottom < 2 { 0 } else { 2 }) + usize::from(col >= 2);
        let pair = quad * 2;
        let color = if (flags >> bit) & 1 == 1 {
            colors[pair + 1]
        } else {
            colors[pair]
        };
        let px = base_x + col;
        let py = base_y + (3 - row_from_bottom);
        put(out, px, py, color);
    }
}

/// Motion JPEG-B codec.
pub fn decompress_mjpb(_data: &[u8]) -> Result<Vec<u8>> {
    Err(BioFormatsError::UnsupportedFormat(
        "Motion JPEG-B codec not implemented: this checkout has no MJPB bitstream parser, ome-codecs source, or known-output fixture".into(),
    ))
}

/// QuickTime Animation/RLE codec.
///
/// This implements the byte-oriented packet form used by 8-bit indexed and
/// 24-bit RGB QuickTime Animation frames: a big-endian chunk header, optional
/// changed-line window, then per-line skip/literal/repeat opcodes. Interframe
/// unchanged pixels are returned as zero because this stateless helper has no
/// previous frame buffer.
pub fn decompress_qtrle(data: &[u8], width: u32, height: u32, bpp: u32) -> Result<Vec<u8>> {
    let bytes_per_pixel = match bpp {
        8 => 1usize,
        24 => 3usize,
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "QuickTime RLE: unsupported {bpp} bpp; only 8 and 24 bpp are implemented"
            )));
        }
    };

    let width = width as usize;
    let height = height as usize;
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }

    let row_bytes = width.checked_mul(bytes_per_pixel).ok_or_else(|| {
        BioFormatsError::InvalidData("QuickTime RLE: row byte count overflows".into())
    })?;
    let out_len = row_bytes.checked_mul(height).ok_or_else(|| {
        BioFormatsError::InvalidData("QuickTime RLE: output byte count overflows".into())
    })?;

    if data.len() < 6 {
        return Err(BioFormatsError::InvalidData(
            "QuickTime RLE: chunk header is truncated".into(),
        ));
    }

    let chunk_size = u32::from_be_bytes([data[0], data[1], data[2], data[3]]) as usize;
    if chunk_size < 6 || chunk_size > data.len() {
        return Err(BioFormatsError::InvalidData(
            "QuickTime RLE: invalid chunk size".into(),
        ));
    }

    let mut i = 4usize;
    let header = read_qtrle_be_u16(data, &mut i)?;
    if header & !0x0008 != 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "QuickTime RLE: unsupported header flags 0x{header:04x}"
        )));
    }

    let (start_line, changed_lines) = if header & 0x0008 != 0 {
        let start_line = read_qtrle_be_u16(data, &mut i)? as usize;
        i = i.checked_add(2).ok_or_else(|| {
            BioFormatsError::InvalidData("QuickTime RLE: header offset overflows".into())
        })?;
        if i > chunk_size {
            return Err(BioFormatsError::InvalidData(
                "QuickTime RLE: changed-line header is truncated".into(),
            ));
        }
        let changed_lines = read_qtrle_be_u16(data, &mut i)? as usize;
        i = i.checked_add(2).ok_or_else(|| {
            BioFormatsError::InvalidData("QuickTime RLE: header offset overflows".into())
        })?;
        if i > chunk_size {
            return Err(BioFormatsError::InvalidData(
                "QuickTime RLE: changed-line header is truncated".into(),
            ));
        }
        (start_line, changed_lines)
    } else {
        (0, height)
    };

    let end_line = start_line.checked_add(changed_lines).ok_or_else(|| {
        BioFormatsError::InvalidData("QuickTime RLE: changed-line range overflows".into())
    })?;
    if end_line > height {
        return Err(BioFormatsError::InvalidData(
            "QuickTime RLE: changed-line range exceeds image height".into(),
        ));
    }

    let mut out = vec![0u8; out_len];
    for y in start_line..end_line {
        let initial_skip = read_qtrle_u8(data, &mut i, chunk_size)? as usize;
        if initial_skip == 0 {
            return Err(BioFormatsError::InvalidData(
                "QuickTime RLE: line skip underflows".into(),
            ));
        }
        let mut x = initial_skip - 1;

        loop {
            let opcode = read_qtrle_i8(data, &mut i, chunk_size)?;
            match opcode {
                -1 => break,
                0 => {
                    let skip = read_qtrle_u8(data, &mut i, chunk_size)? as usize;
                    if skip == 0 {
                        return Err(BioFormatsError::InvalidData(
                            "QuickTime RLE: skip opcode underflows".into(),
                        ));
                    }
                    x = x.checked_add(skip - 1).ok_or_else(|| {
                        BioFormatsError::InvalidData("QuickTime RLE: skip overflows".into())
                    })?;
                    if x > width {
                        return Err(BioFormatsError::InvalidData(
                            "QuickTime RLE: skip exceeds row width".into(),
                        ));
                    }
                }
                n if n < 0 => {
                    let count = (-n) as usize;
                    if i + bytes_per_pixel > chunk_size {
                        return Err(BioFormatsError::InvalidData(
                            "QuickTime RLE: repeat pixel overruns input".into(),
                        ));
                    }
                    write_qtrle_pixels(
                        &mut out,
                        y,
                        &mut x,
                        width,
                        row_bytes,
                        bytes_per_pixel,
                        count,
                        &data[i..i + bytes_per_pixel],
                    )?;
                    i += bytes_per_pixel;
                }
                n => {
                    let count = n as usize;
                    let byte_count = count.checked_mul(bytes_per_pixel).ok_or_else(|| {
                        BioFormatsError::InvalidData(
                            "QuickTime RLE: literal byte count overflows".into(),
                        )
                    })?;
                    if i + byte_count > chunk_size {
                        return Err(BioFormatsError::InvalidData(
                            "QuickTime RLE: literal run overruns input".into(),
                        ));
                    }
                    if x + count > width {
                        return Err(BioFormatsError::InvalidData(
                            "QuickTime RLE: literal run exceeds row width".into(),
                        ));
                    }
                    let dst = y * row_bytes + x * bytes_per_pixel;
                    out[dst..dst + byte_count].copy_from_slice(&data[i..i + byte_count]);
                    i += byte_count;
                    x += count;
                }
            }
        }
    }

    Ok(out)
}

/// Apple RPZA video codec.
///
/// RPZA stores RGB555 pixels in 4x4 blocks. QuickTime sample payloads begin
/// with a one-byte chunk marker followed by a 24-bit chunk length.
pub fn decompress_rpza(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }
    if data.len() < 4 {
        return Err(BioFormatsError::InvalidData(
            "RPZA: chunk header is truncated".into(),
        ));
    }

    let chunk_len = ((data[1] as usize) << 16) | ((data[2] as usize) << 8) | data[3] as usize;
    if chunk_len > data.len() {
        return Err(BioFormatsError::InvalidData(
            "RPZA: chunk length exceeds input".into(),
        ));
    }

    let output_len = checked_video_output_len("RPZA", width, height, 3)?;
    let mut out = vec![0u8; output_len];
    let blocks_wide = width.div_ceil(4);
    let blocks_high = height.div_ceil(4);
    let total_blocks = blocks_wide
        .checked_mul(blocks_high)
        .ok_or_else(|| BioFormatsError::InvalidData("RPZA: block count overflows".into()))?;
    let mut block = 0usize;
    let mut i = 4usize;
    let end = chunk_len.max(4);

    while block < total_blocks && i < end {
        let opcode = data[i];
        i += 1;

        if opcode < 0x80 {
            i -= 1;
            if i + 32 > end {
                return Err(BioFormatsError::InvalidData(
                    "RPZA: literal block overruns input".into(),
                ));
            }
            let mut colors = [0u16; 16];
            for color in &mut colors {
                *color = read_rpza_be_u16(data, &mut i, end)?;
            }
            rpza_write_literal_block(&mut out, width, height, blocks_wide, block, &colors);
            block += 1;
            continue;
        }

        let count = ((opcode & 0x1f) as usize) + 1;
        if block + count > total_blocks {
            return Err(BioFormatsError::InvalidData(
                "RPZA: block run exceeds frame".into(),
            ));
        }

        match opcode & 0xe0 {
            0x80 => {
                block += count;
            }
            0xa0 => {
                let color = read_rpza_be_u16(data, &mut i, end)?;
                for _ in 0..count {
                    rpza_write_solid_block(&mut out, width, height, blocks_wide, block, color);
                    block += 1;
                }
            }
            0xc0 => {
                let color_a = read_rpza_be_u16(data, &mut i, end)?;
                let color_b = read_rpza_be_u16(data, &mut i, end)?;
                let colors = rpza_four_color_table(color_a, color_b);
                for _ in 0..count {
                    if i + 4 > end {
                        return Err(BioFormatsError::InvalidData(
                            "RPZA: four-color block overruns input".into(),
                        ));
                    }
                    let indices = [data[i], data[i + 1], data[i + 2], data[i + 3]];
                    i += 4;
                    rpza_write_indexed_block(
                        &mut out,
                        width,
                        height,
                        blocks_wide,
                        block,
                        &colors,
                        &indices,
                    );
                    block += 1;
                }
            }
            _ => {
                return Err(BioFormatsError::InvalidData(format!(
                    "RPZA: unsupported opcode 0x{opcode:02x}"
                )));
            }
        }
    }

    Ok(out)
}

// ---- Niche codec stubs ----

/// Nikon NEF compression.
pub fn decompress_nikon(data: &[u8], width: u32, height: u32, bpp: u32) -> Result<Vec<u8>> {
    Err(BioFormatsError::UnsupportedFormat(format!(
        "Nikon NEF compression 34713 requires Nikon maker-note IFD tag 150 metadata \
         (vPredictor, curve, split, lossless flag, and compressed strip byte count/maxBytes) \
         plus a dedicated decoder; generic TIFF strip geometry is insufficient for \
         {width}x{height} at {bpp} bpp with {} compressed bytes",
        data.len()
    )))
}

/// LZO1X decompression.
///
/// This decodes the raw LZO1X block format used inside container codecs. It
/// does not parse lzop file headers.
pub fn decompress_lzo(data: &[u8]) -> Result<Vec<u8>> {
    decompress_lzo_with_consumed(data).map(|(out, _)| out)
}

/// LZO1X decompression that also reports how many input bytes were consumed.
///
/// Returns the decoded bytes plus the number of bytes read from `data` up to
/// and including the LZO1X end marker. This is needed by container formats
/// (e.g. Volocity clipping planes) that pack multiple raw LZO1X blocks
/// back-to-back and must advance the input cursor past each decoded block.
pub fn decompress_lzo_with_consumed(data: &[u8]) -> Result<(Vec<u8>, usize)> {
    let mut decoder = Lzo1xDecoder::new(data);
    let out = decoder.decode()?;
    Ok((out, decoder.ip))
}

/// Standard Base64 decoding.
pub fn codec_base64_decode(data: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(data.len() * 3 / 4);
    let mut buf: u32 = 0;
    let mut bits: u32 = 0;
    for &b in data {
        let val = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' | b'\n' | b'\r' | b' ' => continue,
            _ => continue,
        };
        buf = (buf << 6) | val as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((buf >> bits) as u8);
            buf &= (1 << bits) - 1;
        }
    }
    Ok(out)
}

/// Standalone Huffman codec.
pub fn decompress_huffman(_data: &[u8]) -> Result<Vec<u8>> {
    Err(BioFormatsError::UnsupportedFormat(
        "Standalone Huffman codec not yet implemented".into(),
    ))
}

fn read_qtrle_be_u16(data: &[u8], i: &mut usize) -> Result<u16> {
    if *i + 2 > data.len() {
        return Err(BioFormatsError::InvalidData(
            "QuickTime RLE: truncated 16-bit field".into(),
        ));
    }
    let value = u16::from_be_bytes([data[*i], data[*i + 1]]);
    *i += 2;
    Ok(value)
}

fn read_qtrle_u8(data: &[u8], i: &mut usize, limit: usize) -> Result<u8> {
    if *i >= limit {
        return Err(BioFormatsError::InvalidData(
            "QuickTime RLE: packet overruns chunk".into(),
        ));
    }
    let value = data[*i];
    *i += 1;
    Ok(value)
}

fn read_qtrle_i8(data: &[u8], i: &mut usize, limit: usize) -> Result<i8> {
    Ok(read_qtrle_u8(data, i, limit)? as i8)
}

struct Lzo1xDecoder<'a> {
    input: &'a [u8],
    ip: usize,
    output: Vec<u8>,
}

impl<'a> Lzo1xDecoder<'a> {
    fn new(input: &'a [u8]) -> Self {
        Self {
            input,
            ip: 0,
            output: Vec::new(),
        }
    }

    fn decode(&mut self) -> Result<Vec<u8>> {
        if self.input.is_empty() {
            return Ok(Vec::new());
        }

        let mut token = self.read_u8()? as usize;
        if token > 17 {
            let literal_len = token - 17;
            self.copy_literals(literal_len)?;
            token = self.read_u8()? as usize;
            if token < 16 {
                return Err(BioFormatsError::InvalidData(
                    "LZO1X: invalid token after initial literal run".into(),
                ));
            }
        }

        loop {
            if token < 16 {
                let literal_len = if token == 0 {
                    self.extended_len(15)?
                } else {
                    token
                } + 3;
                self.copy_literals(literal_len)?;
                token = self.read_u8()? as usize;
                if token < 16 {
                    let offset = 0x0801 + (token >> 2) + ((self.read_u8()? as usize) << 2);
                    self.copy_match(offset, 3)?;
                    token = self.copy_trailing_literals(token & 0x03)?;
                    continue;
                }
            }

            let trailing = if token >= 64 {
                let len = (token >> 5) + 1;
                let offset = 1 + ((token >> 2) & 0x07) + ((self.read_u8()? as usize) << 3);
                self.copy_match(offset, len)?;
                token & 0x03
            } else if token >= 32 {
                let len = token & 0x1f;
                let len = if len == 0 {
                    self.extended_len(31)?
                } else {
                    len
                } + 2;
                let pair = self.read_le_u16()? as usize;
                let offset = 1 + (pair >> 2);
                self.copy_match(offset, len)?;
                pair & 0x03
            } else {
                let len = token & 0x07;
                let len = if len == 0 { self.extended_len(7)? } else { len } + 2;
                let pair = self.read_le_u16()? as usize;
                let offset = 0x4000 + ((token & 0x08) << 11) + (pair >> 2);
                if offset == 0x4000 {
                    // End-of-stream marker (0x11 0x00 0x00). The stream is
                    // complete; stop here and leave `ip` positioned just past
                    // the marker regardless of any trailing bytes, so callers
                    // that pack blocks back-to-back (e.g. Volocity clipping
                    // planes, which skip a 4-byte trailer between blocks) can
                    // resume decoding from `decoder.ip`. Java's LZOCodec also
                    // terminates at the marker and ignores trailing input.
                    return Ok(std::mem::take(&mut self.output));
                }
                self.copy_match(offset + 1, len)?;
                pair & 0x03
            };

            token = self.copy_trailing_literals(trailing)?;
        }
    }

    fn read_u8(&mut self) -> Result<u8> {
        if self.ip >= self.input.len() {
            return Err(BioFormatsError::InvalidData(
                "LZO1X: truncated input".into(),
            ));
        }
        let value = self.input[self.ip];
        self.ip += 1;
        Ok(value)
    }

    fn read_le_u16(&mut self) -> Result<u16> {
        if self.ip + 2 > self.input.len() {
            return Err(BioFormatsError::InvalidData(
                "LZO1X: truncated 16-bit match offset".into(),
            ));
        }
        let value = u16::from_le_bytes([self.input[self.ip], self.input[self.ip + 1]]);
        self.ip += 2;
        Ok(value)
    }

    fn extended_len(&mut self, base: usize) -> Result<usize> {
        let mut len = base;
        while self.ip < self.input.len() && self.input[self.ip] == 0 {
            len = len.checked_add(255).ok_or_else(|| {
                BioFormatsError::InvalidData("LZO1X: length overflows usize".into())
            })?;
            self.ip += 1;
        }
        let extra = self.read_u8()? as usize;
        len.checked_add(extra)
            .ok_or_else(|| BioFormatsError::InvalidData("LZO1X: length overflows usize".into()))
    }

    fn copy_literals(&mut self, len: usize) -> Result<()> {
        if self.ip + len > self.input.len() {
            return Err(BioFormatsError::InvalidData(
                "LZO1X: literal run overruns input".into(),
            ));
        }
        self.output
            .extend_from_slice(&self.input[self.ip..self.ip + len]);
        self.ip += len;
        Ok(())
    }

    fn copy_match(&mut self, offset: usize, len: usize) -> Result<()> {
        if offset == 0 || offset > self.output.len() {
            return Err(BioFormatsError::InvalidData(
                "LZO1X: invalid match back-reference".into(),
            ));
        }
        let start = self.output.len() - offset;
        for i in 0..len {
            let value = self.output[start + i];
            self.output.push(value);
        }
        Ok(())
    }

    fn copy_trailing_literals(&mut self, len: usize) -> Result<usize> {
        self.copy_literals(len)?;
        Ok(self.read_u8()? as usize)
    }
}

fn write_qtrle_pixels(
    out: &mut [u8],
    y: usize,
    x: &mut usize,
    width: usize,
    row_bytes: usize,
    bytes_per_pixel: usize,
    count: usize,
    pixel: &[u8],
) -> Result<()> {
    if *x + count > width {
        return Err(BioFormatsError::InvalidData(
            "QuickTime RLE: repeat run exceeds row width".into(),
        ));
    }

    let mut dst = y * row_bytes + *x * bytes_per_pixel;
    for _ in 0..count {
        out[dst..dst + bytes_per_pixel].copy_from_slice(pixel);
        dst += bytes_per_pixel;
    }
    *x += count;
    Ok(())
}

fn read_rpza_be_u16(data: &[u8], i: &mut usize, limit: usize) -> Result<u16> {
    if *i + 2 > limit {
        return Err(BioFormatsError::InvalidData(
            "RPZA: truncated 16-bit color".into(),
        ));
    }
    let value = u16::from_be_bytes([data[*i], data[*i + 1]]);
    *i += 2;
    Ok(value)
}

fn rpza_rgb555_to_rgb24(color: u16) -> [u8; 3] {
    let r = ((color >> 10) & 0x1f) as u8;
    let g = ((color >> 5) & 0x1f) as u8;
    let b = (color & 0x1f) as u8;
    [
        (r << 3) | (r >> 2),
        (g << 3) | (g >> 2),
        (b << 3) | (b >> 2),
    ]
}

fn rpza_four_color_table(color_a: u16, color_b: u16) -> [[u8; 3]; 4] {
    let mut colors = [[0u8; 3]; 4];
    colors[0] = rpza_rgb555_to_rgb24(color_a);
    colors[3] = rpza_rgb555_to_rgb24(color_b);

    for component in 0..3 {
        let shift = 10 - component * 5;
        let a = ((color_a >> shift) & 0x1f) as u32;
        let b = ((color_b >> shift) & 0x1f) as u32;
        let c1 = ((11 * a + 21 * b) >> 5) as u8;
        let c2 = ((21 * a + 11 * b) >> 5) as u8;
        colors[1][component] = (c1 << 3) | (c1 >> 2);
        colors[2][component] = (c2 << 3) | (c2 >> 2);
    }

    colors
}

fn rpza_write_rgb(
    out: &mut [u8],
    width: usize,
    height: usize,
    block_x: usize,
    block_y: usize,
    px: usize,
    py: usize,
    rgb: [u8; 3],
) {
    let x = block_x * 4 + px;
    let y = block_y * 4 + py;
    if x >= width || y >= height {
        return;
    }
    let offset = (y * width + x) * 3;
    out[offset..offset + 3].copy_from_slice(&rgb);
}

fn rpza_write_solid_block(
    out: &mut [u8],
    width: usize,
    height: usize,
    blocks_wide: usize,
    block: usize,
    color: u16,
) {
    let block_x = block % blocks_wide;
    let block_y = block / blocks_wide;
    let rgb = rpza_rgb555_to_rgb24(color);
    for py in 0..4 {
        for px in 0..4 {
            rpza_write_rgb(out, width, height, block_x, block_y, px, py, rgb);
        }
    }
}

fn rpza_write_literal_block(
    out: &mut [u8],
    width: usize,
    height: usize,
    blocks_wide: usize,
    block: usize,
    colors: &[u16; 16],
) {
    let block_x = block % blocks_wide;
    let block_y = block / blocks_wide;
    for py in 0..4 {
        for px in 0..4 {
            let rgb = rpza_rgb555_to_rgb24(colors[py * 4 + px]);
            rpza_write_rgb(out, width, height, block_x, block_y, px, py, rgb);
        }
    }
}

fn rpza_write_indexed_block(
    out: &mut [u8],
    width: usize,
    height: usize,
    blocks_wide: usize,
    block: usize,
    colors: &[[u8; 3]; 4],
    indices: &[u8; 4],
) {
    let block_x = block % blocks_wide;
    let block_y = block / blocks_wide;
    for (py, &row) in indices.iter().enumerate() {
        for px in 0..4 {
            let index = ((row >> (6 - px * 2)) & 0x03) as usize;
            rpza_write_rgb(out, width, height, block_x, block_y, px, py, colors[index]);
        }
    }
}

#[derive(Clone)]
struct MsbBitReader<'a> {
    data: &'a [u8],
    bit_pos: usize,
}

impl<'a> MsbBitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, bit_pos: 0 }
    }

    fn read_bit(&mut self) -> Result<u16> {
        if self.bit_pos >= self.data.len() * 8 {
            return Err(BioFormatsError::InvalidData(
                "CCITT Group 3: truncated bitstream".into(),
            ));
        }
        let byte = self.data[self.bit_pos / 8];
        let bit = (byte >> (7 - (self.bit_pos % 8))) & 1;
        self.bit_pos += 1;
        Ok(bit as u16)
    }

    fn peek_bits(&self, len: usize) -> Option<u16> {
        if self.bit_pos + len > self.data.len() * 8 {
            return None;
        }
        let mut code = 0u16;
        for bit in 0..len {
            let pos = self.bit_pos + bit;
            let byte = self.data[pos / 8];
            code = (code << 1) | ((byte >> (7 - (pos % 8))) & 1) as u16;
        }
        Some(code)
    }

    fn skip_bits(&mut self, len: usize) {
        self.bit_pos += len;
    }

    fn skip_ccitt_eols(&mut self) {
        while self.peek_bits(12) == Some(0b0000_0000_0001) {
            self.skip_bits(12);
        }
    }
}

enum CcittCode {
    Run(u16),
    Eol,
}

enum Group4Mode {
    Pass,
    Horizontal,
    Vertical(i8),
}

fn decode_group4_mode(bits: &mut MsbBitReader<'_>) -> Result<Group4Mode> {
    if bits.peek_bits(1) == Some(0b1) {
        bits.skip_bits(1);
        return Ok(Group4Mode::Vertical(0));
    }
    if bits.peek_bits(3) == Some(0b011) {
        bits.skip_bits(3);
        return Ok(Group4Mode::Vertical(1));
    }
    if bits.peek_bits(3) == Some(0b010) {
        bits.skip_bits(3);
        return Ok(Group4Mode::Vertical(-1));
    }
    if bits.peek_bits(3) == Some(0b001) {
        bits.skip_bits(3);
        return Ok(Group4Mode::Horizontal);
    }
    if bits.peek_bits(4) == Some(0b0001) {
        bits.skip_bits(4);
        return Ok(Group4Mode::Pass);
    }
    if bits.peek_bits(6) == Some(0b000011) {
        bits.skip_bits(6);
        return Ok(Group4Mode::Vertical(2));
    }
    if bits.peek_bits(6) == Some(0b000010) {
        bits.skip_bits(6);
        return Ok(Group4Mode::Vertical(-2));
    }
    if bits.peek_bits(7) == Some(0b0000011) {
        bits.skip_bits(7);
        return Ok(Group4Mode::Vertical(3));
    }
    if bits.peek_bits(7) == Some(0b0000010) {
        bits.skip_bits(7);
        return Ok(Group4Mode::Vertical(-3));
    }

    Err(BioFormatsError::InvalidData(
        "CCITT Group 4: invalid two-dimensional mode code".into(),
    ))
}

fn group4_reference_changing_elements(
    reference: &[bool],
    x: usize,
    current_black: bool,
) -> (usize, usize) {
    let b1 = group4_next_transition_to(reference, x, !current_black);
    let b2 = if b1 >= reference.len() {
        reference.len()
    } else {
        group4_next_transition_to(reference, b1 + 1, current_black)
    };
    (b1, b2)
}

fn group4_next_transition_to(reference: &[bool], start: usize, color: bool) -> usize {
    if start >= reference.len() {
        return reference.len();
    }

    let mut previous = if start == 0 {
        false
    } else {
        reference[start - 1]
    };
    for (offset, &pixel) in reference[start..].iter().enumerate() {
        if pixel != previous && pixel == color {
            return start + offset;
        }
        previous = pixel;
    }
    reference.len()
}

fn decode_ccitt_run_length(
    bits: &mut MsbBitReader<'_>,
    black: bool,
    context: &str,
) -> Result<usize> {
    let mut run = 0usize;
    loop {
        match decode_ccitt_run(bits, black)? {
            CcittCode::Run(len) => {
                run += len as usize;
                if len < 64 {
                    return Ok(run);
                }
            }
            CcittCode::Eol => {
                return Err(BioFormatsError::InvalidData(format!(
                    "{context}: unexpected EOL code"
                )));
            }
        }
    }
}

fn decode_ccitt_run(bits: &mut MsbBitReader<'_>, black: bool) -> Result<CcittCode> {
    let table = if black {
        CCITT_BLACK_CODES
    } else {
        CCITT_WHITE_CODES
    };
    let mut code = 0u16;
    for len in 1..=13 {
        code = (code << 1) | bits.read_bit()?;
        if len == 12 && code == 0b0000_0000_0001 {
            return Ok(CcittCode::Eol);
        }
        if let Some((_, _, run)) = table
            .iter()
            .find(|(code_len, table_code, _)| *code_len == len && *table_code == code)
        {
            return Ok(CcittCode::Run(*run));
        }
    }

    Err(BioFormatsError::InvalidData(
        "CCITT Group 3: invalid Huffman code".into(),
    ))
}

const CCITT_WHITE_CODES: &[(u8, u16, u16)] = &[
    (8, 0b00110101, 0),
    (6, 0b000111, 1),
    (4, 0b0111, 2),
    (4, 0b1000, 3),
    (4, 0b1011, 4),
    (4, 0b1100, 5),
    (4, 0b1110, 6),
    (4, 0b1111, 7),
    (5, 0b10011, 8),
    (5, 0b10100, 9),
    (5, 0b00111, 10),
    (5, 0b01000, 11),
    (6, 0b001000, 12),
    (6, 0b000011, 13),
    (6, 0b110100, 14),
    (6, 0b110101, 15),
    (6, 0b101010, 16),
    (6, 0b101011, 17),
    (7, 0b0100111, 18),
    (7, 0b0001100, 19),
    (7, 0b0001000, 20),
    (7, 0b0010111, 21),
    (7, 0b0000011, 22),
    (7, 0b0000100, 23),
    (7, 0b0101000, 24),
    (7, 0b0101011, 25),
    (7, 0b0010011, 26),
    (7, 0b0100100, 27),
    (7, 0b0011000, 28),
    (8, 0b00000010, 29),
    (8, 0b00000011, 30),
    (8, 0b00011010, 31),
    (8, 0b00011011, 32),
    (8, 0b00010010, 33),
    (8, 0b00010011, 34),
    (8, 0b00010100, 35),
    (8, 0b00010101, 36),
    (8, 0b00010110, 37),
    (8, 0b00010111, 38),
    (8, 0b00101000, 39),
    (8, 0b00101001, 40),
    (8, 0b00101010, 41),
    (8, 0b00101011, 42),
    (8, 0b00101100, 43),
    (8, 0b00101101, 44),
    (8, 0b00000100, 45),
    (8, 0b00000101, 46),
    (8, 0b00001010, 47),
    (8, 0b00001011, 48),
    (8, 0b01010010, 49),
    (8, 0b01010011, 50),
    (8, 0b01010100, 51),
    (8, 0b01010101, 52),
    (8, 0b00100100, 53),
    (8, 0b00100101, 54),
    (8, 0b01011000, 55),
    (8, 0b01011001, 56),
    (8, 0b01011010, 57),
    (8, 0b01011011, 58),
    (8, 0b01001010, 59),
    (8, 0b01001011, 60),
    (8, 0b00110010, 61),
    (8, 0b00110011, 62),
    (8, 0b00110100, 63),
    (5, 0b11011, 64),
    (5, 0b10010, 128),
    (6, 0b010111, 192),
    (7, 0b0110111, 256),
    (8, 0b00110110, 320),
    (8, 0b00110111, 384),
    (8, 0b01100100, 448),
    (8, 0b01100101, 512),
    (8, 0b01101000, 576),
    (8, 0b01100111, 640),
    (9, 0b011001100, 704),
    (9, 0b011001101, 768),
    (9, 0b011010010, 832),
    (9, 0b011010011, 896),
    (9, 0b011010100, 960),
    (9, 0b011010101, 1024),
    (9, 0b011010110, 1088),
    (9, 0b011010111, 1152),
    (9, 0b011011000, 1216),
    (9, 0b011011001, 1280),
    (9, 0b011011010, 1344),
    (9, 0b011011011, 1408),
    (9, 0b010011000, 1472),
    (9, 0b010011001, 1536),
    (9, 0b010011010, 1600),
    (6, 0b011000, 1664),
    (9, 0b010011011, 1728),
];

const CCITT_BLACK_CODES: &[(u8, u16, u16)] = &[
    (10, 0b0000110111, 0),
    (3, 0b010, 1),
    (2, 0b11, 2),
    (2, 0b10, 3),
    (3, 0b011, 4),
    (4, 0b0011, 5),
    (4, 0b0010, 6),
    (5, 0b00011, 7),
    (6, 0b000101, 8),
    (6, 0b000100, 9),
    (7, 0b0000100, 10),
    (7, 0b0000101, 11),
    (7, 0b0000111, 12),
    (8, 0b00000100, 13),
    (8, 0b00000111, 14),
    (9, 0b000011000, 15),
    (10, 0b0000010111, 16),
    (10, 0b0000011000, 17),
    (10, 0b0000001000, 18),
    (11, 0b00001100111, 19),
    (11, 0b00001101000, 20),
    (11, 0b00001101100, 21),
    (11, 0b00000110111, 22),
    (11, 0b00000101000, 23),
    (11, 0b00000010111, 24),
    (11, 0b00000011000, 25),
    (12, 0b000011001010, 26),
    (12, 0b000011001011, 27),
    (12, 0b000011001100, 28),
    (12, 0b000011001101, 29),
    (12, 0b000001101000, 30),
    (12, 0b000001101001, 31),
    (12, 0b000001101010, 32),
    (12, 0b000001101011, 33),
    (12, 0b000011010010, 34),
    (12, 0b000011010011, 35),
    (12, 0b000011010100, 36),
    (12, 0b000011010101, 37),
    (12, 0b000011010110, 38),
    (12, 0b000011010111, 39),
    (12, 0b000001101100, 40),
    (12, 0b000001101101, 41),
    (12, 0b000011011010, 42),
    (12, 0b000011011011, 43),
    (12, 0b000001010100, 44),
    (12, 0b000001010101, 45),
    (12, 0b000001010110, 46),
    (12, 0b000001010111, 47),
    (12, 0b000001100100, 48),
    (12, 0b000001100101, 49),
    (12, 0b000001010010, 50),
    (12, 0b000001010011, 51),
    (12, 0b000000100100, 52),
    (12, 0b000000110111, 53),
    (12, 0b000000111000, 54),
    (12, 0b000000100111, 55),
    (12, 0b000000101000, 56),
    (12, 0b000001011000, 57),
    (12, 0b000001011001, 58),
    (12, 0b000000101011, 59),
    (12, 0b000000101100, 60),
    (12, 0b000001011010, 61),
    (12, 0b000001100110, 62),
    (12, 0b000001100111, 63),
    (10, 0b0000001111, 64),
    (12, 0b000011001000, 128),
    (12, 0b000011001001, 192),
    (12, 0b000001011011, 256),
    (12, 0b000000110011, 320),
    (12, 0b000000110100, 384),
    (12, 0b000000110101, 448),
    (13, 0b0000001101100, 512),
    (13, 0b0000001101101, 576),
    (13, 0b0000001001010, 640),
    (13, 0b0000001001011, 704),
    (13, 0b0000001001100, 768),
    (13, 0b0000001001101, 832),
    (13, 0b0000001110010, 896),
    (13, 0b0000001110011, 960),
    (13, 0b0000001110100, 1024),
    (13, 0b0000001110101, 1088),
    (13, 0b0000001110110, 1152),
    (13, 0b0000001110111, 1216),
    (13, 0b0000001010010, 1280),
    (13, 0b0000001010011, 1344),
    (13, 0b0000001010100, 1408),
    (13, 0b0000001010101, 1472),
    (13, 0b0000001011010, 1536),
    (13, 0b0000001011011, 1600),
    (13, 0b0000001100100, 1664),
    (13, 0b0000001100101, 1728),
];

/// Apply TIFF horizontal differencing predictor (predictor = 2).
/// Modifies `data` in-place. `samples_per_pixel` is the number of components.
pub fn undo_horizontal_differencing(data: &mut [u8], samples_per_pixel: usize) {
    if samples_per_pixel == 0 || data.len() < samples_per_pixel * 2 {
        return;
    }
    for i in samples_per_pixel..data.len() {
        data[i] = data[i].wrapping_add(data[i - samples_per_pixel]);
    }
}

/// Apply TIFF horizontal differencing predictor for 16-bit samples.
pub fn undo_horizontal_differencing_u16(data: &mut [u16], samples_per_pixel: usize) {
    if samples_per_pixel == 0 || data.len() < samples_per_pixel * 2 {
        return;
    }
    for i in samples_per_pixel..data.len() {
        data[i] = data[i].wrapping_add(data[i - samples_per_pixel]);
    }
}

// ───────────────────────────────────────────────────────────────────────────
// Cinepak ("cvid") video codec
// ───────────────────────────────────────────────────────────────────────────

/// One Cinepak codebook (256 entries). Each entry holds a decoded 4x4 block's
/// four corner colors as RGB (V4) or the single replicated color (V1).
#[derive(Clone)]
struct CinepakCodebook {
    /// Per-entry RGB for the four sub-quadrants: [tl, tr, bl, br], each [r,g,b].
    entries: Vec<[[u8; 3]; 4]>,
}

impl CinepakCodebook {
    fn new() -> Self {
        CinepakCodebook {
            entries: vec![[[0u8; 3]; 4]; 256],
        }
    }
}

#[inline]
fn cinepak_yuv_to_rgb(y: u8, u: i8, v: i8) -> [u8; 3] {
    // YUV -> RGB per the FFmpeg/Bio-Formats Cinepak convention.
    let y = y as i32;
    let u = u as i32;
    let v = v as i32;
    let r = (y + (v * 2)).clamp(0, 255) as u8;
    let g = (y - (u / 2) - v).clamp(0, 255) as u8;
    let b = (y + (u * 2)).clamp(0, 255) as u8;
    [r, g, b]
}

#[inline]
fn rd_be_u16(data: &[u8], i: usize) -> Result<u16> {
    data.get(i..i + 2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .ok_or_else(|| BioFormatsError::Codec("Cinepak: truncated stream".into()))
}

/// Decode a codebook chunk. `is_v4` selects 6-byte (luma+chroma) entries; when
/// `grayscale` only the 4 luma bytes are present (V1 uses 6 or 4 likewise).
/// `detail` (0x2200/0x2300) chunks update only entries flagged in a bit vector.
fn cinepak_read_codebook(
    book: &mut CinepakCodebook,
    data: &[u8],
    mut pos: usize,
    end: usize,
    grayscale: bool,
    detail: bool,
) -> Result<()> {
    let entry_size = if grayscale { 4 } else { 6 };
    let mut index = 0usize;
    let mut flag = 0u32;
    let mut flag_bits = 0u32;
    while index < 256 && pos < end {
        let update = if detail {
            if flag_bits == 0 {
                if pos + 4 > end {
                    break;
                }
                flag = u32::from_be_bytes([data[pos], data[pos + 1], data[pos + 2], data[pos + 3]]);
                pos += 4;
                flag_bits = 32;
            }
            flag_bits -= 1;
            (flag >> flag_bits) & 1 == 1
        } else {
            true
        };

        if update {
            if pos + entry_size > end {
                break;
            }
            let y = [data[pos], data[pos + 1], data[pos + 2], data[pos + 3]];
            let (u, v) = if grayscale {
                (0i8, 0i8)
            } else {
                (data[pos + 4] as i8, data[pos + 5] as i8)
            };
            pos += entry_size;
            book.entries[index] = [
                cinepak_yuv_to_rgb(y[0], u, v),
                cinepak_yuv_to_rgb(y[1], u, v),
                cinepak_yuv_to_rgb(y[2], u, v),
                cinepak_yuv_to_rgb(y[3], u, v),
            ];
        }
        index += 1;
    }
    Ok(())
}

/// Write a single 4x4 macroblock from a V1 codebook entry (one color upscaled
/// to the four 2x2 quadrants). `mb_x`,`mb_y` are pixel coordinates of the top-
/// left corner.
#[allow(clippy::too_many_arguments)]
fn cinepak_put_v1(
    out: &mut [u8],
    width: usize,
    height: usize,
    channels: usize,
    mb_x: usize,
    mb_y: usize,
    e: &[[u8; 3]; 4],
) {
    // V1: each of the four codebook colors fills a 2x2 quadrant.
    for q in 0..4 {
        let (qx, qy) = ((q & 1) * 2, (q >> 1) * 2);
        let rgb = e[q];
        for dy in 0..2usize {
            for dx in 0..2usize {
                cinepak_set_pixel(
                    out,
                    width,
                    height,
                    channels,
                    mb_x + qx + dx,
                    mb_y + qy + dy,
                    rgb,
                );
            }
        }
    }
}

/// Write a single 4x4 macroblock from a V4 codebook entry (each color maps to
/// one 2x2 quadrant, no upscaling).
#[allow(clippy::too_many_arguments)]
fn cinepak_put_v4(
    out: &mut [u8],
    width: usize,
    height: usize,
    channels: usize,
    mb_x: usize,
    mb_y: usize,
    e0: &[[u8; 3]; 4],
    e1: &[[u8; 3]; 4],
    e2: &[[u8; 3]; 4],
    e3: &[[u8; 3]; 4],
) {
    // V4: four codebook entries, one per 2x2 quadrant; within each quadrant the
    // four corner colors map to the four pixels.
    let books = [e0, e1, e2, e3];
    for q in 0..4 {
        let (qx, qy) = ((q & 1) * 2, (q >> 1) * 2);
        let e = books[q];
        cinepak_set_pixel(out, width, height, channels, mb_x + qx, mb_y + qy, e[0]);
        cinepak_set_pixel(out, width, height, channels, mb_x + qx + 1, mb_y + qy, e[1]);
        cinepak_set_pixel(out, width, height, channels, mb_x + qx, mb_y + qy + 1, e[2]);
        cinepak_set_pixel(
            out,
            width,
            height,
            channels,
            mb_x + qx + 1,
            mb_y + qy + 1,
            e[3],
        );
    }
}

#[inline]
fn cinepak_set_pixel(
    out: &mut [u8],
    width: usize,
    height: usize,
    channels: usize,
    x: usize,
    y: usize,
    rgb: [u8; 3],
) {
    if x >= width || y >= height {
        return;
    }
    let off = (y * width + x) * channels;
    if channels == 1 {
        if off < out.len() {
            out[off] = rgb[0];
        }
    } else if off + 2 < out.len() {
        out[off] = rgb[0];
        out[off + 1] = rgb[1];
        out[off + 2] = rgb[2];
    }
}

/// Decode a Cinepak ("cvid") compressed frame.
///
/// Ported to match the Bio-Formats `CinepakCodec` algorithm (strip / macroblock
/// structure with V1 and V4 codebooks of 4x4 blocks). `bpp` is the source bits
/// per pixel (8 -> single-channel grayscale output, otherwise 24-bit RGB).
/// `prev` is the previously decoded frame (same layout) used for inter-coded
/// strips; pass an empty slice for keyframes.
pub fn decompress_cinepak(
    data: &[u8],
    width: u32,
    height: u32,
    bpp: u32,
    prev: &[u8],
) -> Result<Vec<u8>> {
    let width = width as usize;
    let height = height as usize;
    if width == 0 || height == 0 {
        return Err(BioFormatsError::InvalidData(
            "Cinepak: width and height must be non-zero".into(),
        ));
    }
    let grayscale = bpp == 8;
    let channels = if grayscale { 1 } else { 3 };
    let output_len = checked_video_output_len("Cinepak", width, height, channels)?;

    let mut out = vec![0u8; output_len];
    if prev.len() == output_len {
        out.copy_from_slice(prev);
    }

    if data.len() < 10 {
        return Err(BioFormatsError::Codec(
            "Cinepak: frame header too short".into(),
        ));
    }
    // Frame header: flags(1), length(3), width(2 BE), height(2 BE), strips(2 BE).
    let num_strips = rd_be_u16(data, 8)? as usize;
    let mut pos = 10usize;

    // Per-strip codebooks persist across strips within a frame.
    let mut v1 = CinepakCodebook::new();
    let mut v4 = CinepakCodebook::new();

    let mut strip_y0 = 0usize;
    for _ in 0..num_strips {
        if pos + 12 > data.len() {
            break;
        }
        let _strip_id = rd_be_u16(data, pos)?;
        let strip_size = rd_be_u16(data, pos + 2)? as usize;
        let top = rd_be_u16(data, pos + 4)? as usize;
        let _left = rd_be_u16(data, pos + 6)? as usize;
        let bottom = rd_be_u16(data, pos + 8)? as usize;
        let _right = rd_be_u16(data, pos + 10)? as usize;
        let strip_data_start = pos + 12;
        let strip_end = (pos + strip_size).min(data.len()).max(strip_data_start);
        pos = strip_data_start;

        // Strip vertical bounds: some encoders store absolute, others relative.
        let strip_top = if bottom > top { top } else { strip_y0 };
        let strip_bottom = if bottom > top {
            bottom
        } else {
            (strip_y0 + (bottom)).max(strip_y0)
        };
        let _ = strip_bottom;

        // Decode the chunks inside this strip.
        cinepak_decode_strip(
            &mut out, width, height, channels, prev, grayscale, &mut v1, &mut v4, data, pos,
            strip_end, strip_top,
        )?;

        strip_y0 = strip_top + {
            // advance by the strip height (in macroblocks * 4)
            let h = bottom.saturating_sub(top);
            if h > 0 {
                h
            } else {
                0
            }
        };
        pos = strip_end;
    }

    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn cinepak_decode_strip(
    out: &mut [u8],
    width: usize,
    height: usize,
    channels: usize,
    prev: &[u8],
    grayscale: bool,
    v1: &mut CinepakCodebook,
    v4: &mut CinepakCodebook,
    data: &[u8],
    mut pos: usize,
    strip_end: usize,
    strip_top: usize,
) -> Result<()> {
    let mb_per_row = width.div_ceil(4);
    while pos + 4 <= strip_end {
        let chunk_id = rd_be_u16(data, pos)?;
        let chunk_size = rd_be_u16(data, pos + 2)? as usize;
        let chunk_data = pos + 4;
        let chunk_end = (pos + chunk_size).min(strip_end).max(chunk_data);
        pos = chunk_end;

        match chunk_id {
            // V4 codebook
            0x2000 => cinepak_read_codebook(v4, data, chunk_data, chunk_end, grayscale, false)?,
            0x2200 => cinepak_read_codebook(v4, data, chunk_data, chunk_end, grayscale, true)?,
            // V1 codebook
            0x2100 => cinepak_read_codebook(v1, data, chunk_data, chunk_end, grayscale, false)?,
            0x2300 => cinepak_read_codebook(v1, data, chunk_data, chunk_end, grayscale, true)?,
            // Intra-coded vectors (0x3000) and inter-coded (0x3100).
            0x3000 | 0x3100 => {
                let inter = chunk_id == 0x3100;
                cinepak_decode_vectors(
                    out, width, height, channels, prev, v1, v4, data, chunk_data, chunk_end,
                    strip_top, mb_per_row, inter,
                )?;
            }
            // V1-only vectors (no selector flags).
            0x3200 => {
                cinepak_decode_v1_only(
                    out, width, height, channels, v1, data, chunk_data, chunk_end, strip_top,
                    mb_per_row,
                )?;
            }
            _ => {
                // Unknown chunk: skip.
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cinepak_decode_vectors(
    out: &mut [u8],
    width: usize,
    height: usize,
    channels: usize,
    prev: &[u8],
    v1: &CinepakCodebook,
    v4: &CinepakCodebook,
    data: &[u8],
    mut pos: usize,
    end: usize,
    strip_top: usize,
    mb_per_row: usize,
    inter: bool,
) -> Result<()> {
    // Macroblock rows remaining from the strip top to the bottom of the image.
    let strip_mb_rows = height.saturating_sub(strip_top).div_ceil(4).max(1);
    let mut mb_index = 0usize;
    let total_mb = mb_per_row * strip_mb_rows;

    let mut flag = 0u32;
    let mut flag_bits = 0u32;
    let mut next_flag = |pos: &mut usize| -> Option<bool> {
        if flag_bits == 0 {
            if *pos + 4 > end {
                return None;
            }
            flag = u32::from_be_bytes([data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]]);
            *pos += 4;
            flag_bits = 32;
        }
        flag_bits -= 1;
        Some((flag >> flag_bits) & 1 == 1)
    };

    while pos < end && mb_index < total_mb {
        let mb_x = (mb_index % mb_per_row) * 4;
        let mb_y = strip_top + (mb_index / mb_per_row) * 4;
        if mb_y >= height {
            break;
        }

        // For inter frames, a top-level flag says whether this MB is coded.
        let coded = if inter {
            match next_flag(&mut pos) {
                Some(b) => b,
                None => break,
            }
        } else {
            true
        };

        if !coded {
            // copy from previous frame (already pre-filled in `out`)
            let _ = prev;
            mb_index += 1;
            continue;
        }

        // V1 vs V4 selector flag.
        let is_v4 = match next_flag(&mut pos) {
            Some(b) => b,
            None => break,
        };

        if is_v4 {
            if pos + 4 > end {
                break;
            }
            let i0 = data[pos] as usize;
            let i1 = data[pos + 1] as usize;
            let i2 = data[pos + 2] as usize;
            let i3 = data[pos + 3] as usize;
            pos += 4;
            cinepak_put_v4(
                out,
                width,
                height,
                channels,
                mb_x,
                mb_y,
                &v4.entries[i0],
                &v4.entries[i1],
                &v4.entries[i2],
                &v4.entries[i3],
            );
        } else {
            if pos >= end {
                break;
            }
            let i = data[pos] as usize;
            pos += 1;
            cinepak_put_v1(out, width, height, channels, mb_x, mb_y, &v1.entries[i]);
        }
        mb_index += 1;
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn cinepak_decode_v1_only(
    out: &mut [u8],
    width: usize,
    height: usize,
    channels: usize,
    v1: &CinepakCodebook,
    data: &[u8],
    mut pos: usize,
    end: usize,
    strip_top: usize,
    mb_per_row: usize,
) -> Result<()> {
    let mut mb_index = 0usize;
    while pos < end {
        let mb_x = (mb_index % mb_per_row) * 4;
        let mb_y = strip_top + (mb_index / mb_per_row) * 4;
        if mb_y >= height {
            break;
        }
        let i = data[pos] as usize;
        pos += 1;
        cinepak_put_v1(out, width, height, channels, mb_x, mb_y, &v1.entries[i]);
        mb_index += 1;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bits_to_bytes(bits: &str) -> Vec<u8> {
        let mut out = Vec::new();
        let mut byte = 0u8;
        let mut used = 0usize;
        for bit in bits.bytes().filter(|b| *b == b'0' || *b == b'1') {
            byte = (byte << 1) | (bit - b'0');
            used += 1;
            if used == 8 {
                out.push(byte);
                byte = 0;
                used = 0;
            }
        }
        if used != 0 {
            out.push(byte << (8 - used));
        }
        out
    }

    #[test]
    fn bzip2_decompresses_standard_stream() {
        // `printf 'hello bzip2 world, hello bzip2 world!' | bzip2 -c`
        // A complete standard stream starting with the "BZh" magic.
        let data: &[u8] = &[
            0x42, 0x5a, 0x68, 0x39, 0x31, 0x41, 0x59, 0x26, 0x53, 0x59, 0x73, 0x4d, 0xd1, 0x53,
            0x00, 0x00, 0x08, 0x19, 0x80, 0x60, 0x04, 0x10, 0x00, 0x16, 0x64, 0xd0, 0x90, 0x20,
            0x00, 0x20, 0xaa, 0xa6, 0x8d, 0x3d, 0x35, 0x32, 0x6d, 0x0a, 0x60, 0x00, 0x32, 0xf2,
            0xe8, 0x69, 0xeb, 0x4c, 0xbb, 0x9a, 0x6d, 0xfa, 0x0a, 0x53, 0x69, 0x53, 0x82, 0xee,
            0x48, 0xa7, 0x0a, 0x12, 0x0e, 0x69, 0xba, 0x2a, 0x60,
        ];

        let out = decompress_bzip2(data).expect("bzip2 decode");

        assert_eq!(out, b"hello bzip2 world, hello bzip2 world!");
    }

    #[test]
    fn ccitt_group3_decodes_all_white_row_with_eol() {
        let data = bits_to_bytes(
            "000000000001\
             10011",
        );

        let out = decompress_ccitt_group3(&data, 8, 1).expect("CCITT Group 3 decode");

        assert_eq!(out, vec![0x00]);
    }

    #[test]
    fn ccitt_group3_decodes_mixed_runs() {
        let data = bits_to_bytes(
            "000000000001\
             0111\
             0011\
             1000",
        );

        let out = decompress_ccitt_group3(&data, 10, 1).expect("CCITT Group 3 decode");

        assert_eq!(out, vec![0b0011_1110, 0x00]);
    }

    #[test]
    fn ccitt_group3_rejects_overlong_run() {
        let data = bits_to_bytes("10100");
        let err = decompress_ccitt_group3(&data, 8, 1).expect_err("overlong run must fail");

        assert!(matches!(
            err,
            BioFormatsError::InvalidData(message) if message.contains("run exceeds row width")
        ));
    }

    #[test]
    fn ccitt_group4_decodes_all_white_rows_with_vertical_mode() {
        let data = bits_to_bytes("11");

        let out = decompress_ccitt_group4(&data, 8, 2).expect("CCITT Group 4 decode");

        assert_eq!(out, vec![0x00, 0x00]);
    }

    #[test]
    fn ccitt_group4_decodes_horizontal_then_vertical_mixed_rows() {
        let data = bits_to_bytes(
            "001\
             0111\
             011\
             1\
             111",
        );

        let out = decompress_ccitt_group4(&data, 10, 2).expect("CCITT Group 4 decode");

        assert_eq!(out, vec![0b0011_1100, 0x00, 0b0011_1100, 0x00]);
    }

    #[test]
    fn ccitt_group4_decodes_pass_mode() {
        let data = bits_to_bytes(
            "001\
             0111\
             011\
             1\
             0001\
             1",
        );

        let out = decompress_ccitt_group4(&data, 10, 2).expect("CCITT Group 4 decode");

        assert_eq!(out, vec![0b0011_1100, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn msrle_decodes_encoded_absolute_delta_and_bottom_up_rows() {
        let data = [
            3, 7, // bottom row: 7 7 7
            0, 0, // EOL
            0, 3, 1, 2, 3, 0, // top row absolute: 1 2 3 plus word pad
            0, 1, // EOB
        ];

        let out = decompress_msrle(&data, 3, 2).expect("MSRLE decode");

        assert_eq!(out, vec![1, 2, 3, 7, 7, 7]);
    }

    #[test]
    fn msrle_decodes_delta_offsets() {
        let data = [
            1, 9, // bottom row x=0
            0, 2, 1, 1, // move to top row x=2
            1, 5, // top row x=2
            0, 1, // EOB
        ];

        let out = decompress_msrle(&data, 3, 2).expect("MSRLE decode");

        assert_eq!(out, vec![0, 0, 5, 9, 0, 0]);
    }

    #[test]
    fn msvideo_8bit_decodes_two_color_block() {
        // Single 4x4 block. flags = 0x000F: bits 0..3 set select color_a, which
        // in the bottom-up bit layout is the bottom display row.
        let data = [
            0x0f, 0x00, // flags (byte_b < 0x80 => 2-color)
            100,  // color_a (selected by set bits)
            200,  // color_b (selected by clear bits)
        ];
        let out = decompress_msvideo(&data, 4, 4, 1).expect("MSVideo 8-bit decode");
        // Rows 0..2 (top three display rows) are color_b; row 3 (bottom) is color_a.
        let mut expected = vec![200u8; 16];
        for px in expected.iter_mut().skip(12) {
            *px = 100;
        }
        assert_eq!(out, expected);
    }

    #[test]
    fn msvideo_8bit_decodes_solid_block() {
        // 1-color block: 0x80 <= byte_b < 0x90 and byte_b < 0x90 => solid fill of byte_a.
        let data = [42u8, 0x80];
        let out = decompress_msvideo(&data, 4, 4, 1).expect("MSVideo 8-bit solid");
        assert_eq!(out, vec![42u8; 16]);
    }

    #[test]
    fn msvideo_8bit_skip_leaves_previous_frame_zero() {
        // Skip run covering the whole 4x4 frame (one block). 0x84 <= byte_b <= 0x87.
        // skip = (0x84 - 0x84) * 256 + 1 = 1 block.
        let data = [0x01u8, 0x84];
        let out = decompress_msvideo(&data, 4, 4, 1).expect("MSVideo 8-bit skip");
        assert_eq!(out, vec![0u8; 16]);
    }

    #[test]
    fn msvideo_16bit_decodes_solid_rgb555() {
        // 1-color 16-bit block: byte_b >= 0x80 (here 0xFF) => solid fill.
        // color = 0x7FFF & flags. Use pure white: R=G=B=31 => 0x7FFF.
        let data = [0xff, 0xff];
        let out = decompress_msvideo(&data, 4, 4, 2).expect("MSVideo 16-bit solid");
        assert_eq!(out.len(), 16 * 3);
        // RGB555 0x7FFF expands to (255,255,255).
        assert!(out.iter().all(|&b| b == 255));
    }

    #[test]
    fn msvideo_rejects_unsupported_depth() {
        let err = decompress_msvideo(&[0, 0], 4, 4, 3).expect_err("unsupported depth must fail");
        assert!(matches!(err, BioFormatsError::UnsupportedFormat(_)));
    }

    #[test]
    fn msrle_rejects_overlong_runs() {
        let err = decompress_msrle(&[4, 1], 3, 1).expect_err("overlong run must fail");

        assert!(matches!(
            err,
            BioFormatsError::InvalidData(message) if message.contains("exceeds row width")
        ));
    }

    #[test]
    fn msrle_rejects_oversized_dimensions_before_allocation() {
        let err = decompress_msrle(&[0, 1], u32::MAX, u32::MAX)
            .expect_err("oversized frame must fail before allocation");

        assert!(matches!(
            err,
            BioFormatsError::InvalidData(message)
                if message.contains("output byte count overflows")
                    || message.contains("decoded frame is too large")
        ));
    }

    #[test]
    fn mjpb_reports_missing_local_implementation_sources_and_fixture() {
        let err = decompress_mjpb(&[]).expect_err("MJPB remains unsupported without fixtures");

        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("MJPB bitstream parser")
                    && message.contains("ome-codecs source")
                    && message.contains("known-output fixture")
        ));
    }

    #[test]
    fn standalone_huffman_reports_missing_decoder_contract() {
        let err = decompress_huffman(&[]).expect_err("standalone Huffman remains unsupported");

        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("Standalone Huffman codec not yet implemented")
        ));
    }

    #[test]
    fn nikon_reports_missing_decoder_contract() {
        let err = decompress_nikon(&[], 17, 23, 36).expect_err("Nikon remains unsupported");

        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("Nikon NEF compression 34713")
                    && message.contains("maker-note IFD tag 150")
                    && message.contains("vPredictor")
                    && message.contains("curve")
                    && message.contains("split")
                    && message.contains("lossless flag")
                    && message.contains("maxBytes")
                    && message.contains("17x23")
                    && message.contains("36 bpp")
                    && message.contains("0 compressed bytes")
        ));
    }

    #[test]
    fn lzo1x_decodes_initial_literal_run() {
        let data = [
            22, b'h', b'e', b'l', b'l', b'o', // five initial literals
            0x11, 0x00, 0x00, // end marker
        ];

        let out = decompress_lzo(&data).expect("LZO1X decode");

        assert_eq!(out, b"hello");
    }

    #[test]
    fn lzo1x_with_consumed_reports_block_length() {
        // A single "hello" literal block is 9 bytes (5 initial literals + the
        // 3-byte end marker, preceded by the token byte).
        let block = [
            22, b'h', b'e', b'l', b'l', b'o', // five initial literals
            0x11, 0x00, 0x00, // end marker
        ];

        let (out, consumed) =
            decompress_lzo_with_consumed(&block).expect("LZO1X decode with consumed");
        assert_eq!(out, b"hello");
        assert_eq!(consumed, block.len());
    }

    #[test]
    fn lzo1x_with_consumed_decodes_back_to_back_blocks() {
        // Two raw LZO1X blocks packed back-to-back, each separated from the
        // next by a 4-byte trailer (as Volocity clipping planes store them).
        let block_a = [22, b'h', b'e', b'l', b'l', b'o', 0x11, 0x00, 0x00];
        let block_b = [21, b'w', b'o', b'r', b'l', 0x11, 0x00, 0x00];
        let trailer = [0xde, 0xad, 0xbe, 0xef];

        let mut data = Vec::new();
        data.extend_from_slice(&block_a);
        data.extend_from_slice(&trailer);
        data.extend_from_slice(&block_b);

        let mut pos = 0usize;
        let mut decoded = Vec::new();
        let (out_a, consumed_a) =
            decompress_lzo_with_consumed(&data[pos..]).expect("first block");
        decoded.extend_from_slice(&out_a);
        assert_eq!(consumed_a, block_a.len());
        pos += consumed_a + trailer.len(); // mirror Java's skipBytes(4)

        let (out_b, consumed_b) =
            decompress_lzo_with_consumed(&data[pos..]).expect("second block");
        decoded.extend_from_slice(&out_b);
        assert_eq!(consumed_b, block_b.len());

        assert_eq!(decoded, b"helloworl");
    }

    #[test]
    fn lzo1x_decodes_near_match() {
        let data = [
            20, b'a', b'b', b'c', // three initial literals
            0x48, 0x00, // length 3, offset 3
            0x11, 0x00, 0x00, // end marker
        ];

        let out = decompress_lzo(&data).expect("LZO1X decode");

        assert_eq!(out, b"abcabc");
    }

    #[test]
    fn lzo1x_decodes_extended_literal_run() {
        let mut data = vec![0x00, 0x06];
        data.extend_from_slice(&[0x5a; 24]);
        data.extend_from_slice(&[0x11, 0x00, 0x00]);

        let out = decompress_lzo(&data).expect("LZO1X decode");

        assert_eq!(out, vec![0x5a; 24]);
    }

    #[test]
    fn lzo1x_rejects_invalid_back_reference() {
        let data = [
            20, b'a', b'b', b'c', // three initial literals
            0x40, 0x00, // length 3, offset 1, followed by one trailing literal
            b'd', 0x10, 0x00, 0x00, // invalid far match offset before any end marker
        ];

        let err = decompress_lzo(&data).expect_err("invalid back-reference must fail");

        assert!(matches!(
            err,
            BioFormatsError::InvalidData(message)
                if message.contains("invalid match back-reference")
        ));
    }

    #[test]
    fn lzo1x_rejects_truncated_literal_run() {
        let err = decompress_lzo(&[22, b'h']).expect_err("truncated literal run must fail");

        assert!(matches!(
            err,
            BioFormatsError::InvalidData(message) if message.contains("literal run overruns input")
        ));
    }

    #[test]
    fn qtrle_decodes_8bit_literal_and_repeat_rows() {
        let data = [
            0x00, 0x00, 0x00, 0x12, // chunk size
            0x00, 0x00, // full-frame update
            1, 5, 1, 2, 3, 4, 5, 0xff, // row 0: five literal pixels
            1, 0xfb, 9, 0xff, // row 1: repeat pixel 9 five times
        ];

        let out = decompress_qtrle(&data, 5, 2, 8).expect("QTRLE decode");

        assert_eq!(out, vec![1, 2, 3, 4, 5, 9, 9, 9, 9, 9]);
    }

    #[test]
    fn qtrle_decodes_24bit_changed_line_window() {
        let data = [
            0x00, 0x00, 0x00, 0x17, // chunk size
            0x00, 0x08, // changed-line header follows
            0x00, 0x01, // start line
            0x00, 0x00, // reserved
            0x00, 0x01, // one changed line
            0x00, 0x00, // reserved
            2,    // skip pixel 0
            2,    // two literal RGB pixels
            255, 0, 0, 0, 255, 0, 0xff,
        ];

        let out = decompress_qtrle(&data, 4, 3, 24).expect("QTRLE decode");

        assert_eq!(
            out,
            vec![
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // unchanged row 0
                0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 0, // changed row 1
                0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, // unchanged row 2
            ]
        );
    }

    #[test]
    fn qtrle_rejects_unsupported_depth() {
        let err = decompress_qtrle(&[0, 0, 0, 6, 0, 0], 1, 1, 16)
            .expect_err("16 bpp is outside the implemented subset");

        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message)
                if message.contains("only 8 and 24 bpp are implemented")
        ));
    }

    #[test]
    fn rpza_decodes_solid_color_run() {
        let data = [
            0xe1, 0x00, 0x00, 0x07, // chunk marker and length
            0xa0, 0x7c, 0x00, // one RGB555 red block
        ];

        let out = decompress_rpza(&data, 4, 4).expect("RPZA decode");

        assert_eq!(out, vec![255, 0, 0].repeat(16));
    }

    #[test]
    fn rpza_decodes_literal_block_and_clips_edges() {
        let data = [
            0xe1, 0x00, 0x00, 0x24, // chunk marker and length
            0x7c, 0x00, 0x03, 0xe0, 0x00, 0x1f, 0x7f, 0xff, // row 0
            0x00, 0x00, 0x7c, 0x00, 0x03, 0xe0, 0x00, 0x1f, // row 1
            0x7f, 0xff, 0x00, 0x00, 0x7c, 0x00, 0x03, 0xe0, // row 2
            0x00, 0x1f, 0x7f, 0xff, 0x00, 0x00, 0x7c, 0x00, // row 3
        ];

        let out = decompress_rpza(&data, 2, 2).expect("RPZA decode");

        assert_eq!(
            out,
            vec![
                255, 0, 0, 0, 255, 0, // clipped row 0
                0, 0, 0, 255, 0, 0, // clipped row 1
            ]
        );
    }

    #[test]
    fn rpza_decodes_four_color_block() {
        let data = [
            0xe1, 0x00, 0x00, 0x0d, // chunk marker and length
            0xc0, 0x7c, 0x00, 0x00, 0x00, // red and black endpoints
            0x1b, 0x1b, 0x1b, 0x1b, // indices 0, 1, 2, 3 on every row
        ];

        let out = decompress_rpza(&data, 4, 4).expect("RPZA decode");

        assert_eq!(out, vec![255, 0, 0, 82, 0, 0, 165, 0, 0, 0, 0, 0].repeat(4));
    }

    #[test]
    fn rpza_decodes_skipped_blocks_as_black() {
        let data = [
            0xe1, 0x00, 0x00, 0x08, // chunk marker and length
            0xa0, 0x7c, 0x00, // one RGB555 red block
            0x80, // skip the next block
        ];

        let out = decompress_rpza(&data, 8, 4).expect("RPZA decode");
        let mut expected = Vec::new();
        for _ in 0..4 {
            expected.extend_from_slice(&[255, 0, 0].repeat(4));
            expected.extend_from_slice(&[0, 0, 0].repeat(4));
        }

        assert_eq!(out, expected);
    }

    #[test]
    fn rpza_rejects_oversized_dimensions_before_allocation() {
        let data = [0xe1, 0x00, 0x00, 0x04];
        let err = decompress_rpza(&data, u32::MAX, u32::MAX)
            .expect_err("oversized frame must fail before allocation");

        assert!(matches!(
            err,
            BioFormatsError::InvalidData(message)
                if message.contains("output byte count overflows")
                    || message.contains("decoded frame is too large")
        ));
    }

    // ── Cinepak ──────────────────────────────────────────────────────────────

    fn be16(v: u16) -> [u8; 2] {
        v.to_be_bytes()
    }

    /// Build a single-strip Cinepak frame with the given chunk bodies.
    fn cinepak_frame(width: u16, height: u16, chunks: &[(u16, Vec<u8>)]) -> Vec<u8> {
        let mut strip_body = Vec::new();
        for (id, body) in chunks {
            strip_body.extend_from_slice(&be16(*id));
            strip_body.extend_from_slice(&be16((body.len() + 4) as u16));
            strip_body.extend_from_slice(body);
        }
        let strip_size = strip_body.len() + 12;
        let mut frame = Vec::new();
        frame.push(0x00); // flags (intra)
        frame.extend_from_slice(&[0, 0, 0]); // length (unused by decoder)
        frame.extend_from_slice(&be16(width));
        frame.extend_from_slice(&be16(height));
        frame.extend_from_slice(&be16(1)); // num strips
                                           // strip header
        frame.extend_from_slice(&be16(0x1000)); // intra strip id
        frame.extend_from_slice(&be16(strip_size as u16));
        frame.extend_from_slice(&be16(0)); // y0
        frame.extend_from_slice(&be16(0)); // x0
        frame.extend_from_slice(&be16(height)); // y1
        frame.extend_from_slice(&be16(width)); // x1
        frame.extend_from_slice(&strip_body);
        frame
    }

    #[test]
    fn cinepak_v1_grayscale_block_upscales_quadrants() {
        // One 4x4 grayscale block from V1 codebook entry 0 (Y = 10,20,30,40).
        let v1 = vec![10u8, 20, 30, 40]; // grayscale entry (4 bytes)
        let vectors = vec![0u8]; // one block, index 0
        let frame = cinepak_frame(4, 4, &[(0x2100, v1), (0x3200, vectors)]);

        let out = decompress_cinepak(&frame, 4, 4, 8, &[]).expect("decode");
        assert_eq!(out.len(), 16);
        // Expected: TL quadrant=10, TR=20, BL=30, BR=40, each 2x2.
        let px = |x: usize, y: usize| out[y * 4 + x];
        for y in 0..2 {
            for x in 0..2 {
                assert_eq!(px(x, y), 10, "TL");
                assert_eq!(px(x + 2, y), 20, "TR");
                assert_eq!(px(x, y + 2), 30, "BL");
                assert_eq!(px(x + 2, y + 2), 40, "BR");
            }
        }
    }

    #[test]
    fn cinepak_v4_rgb_block_maps_four_codebook_entries() {
        // V4 codebook entries 0..4, each a flat luma so RGB is grayscale-ish.
        // entry i: Y all = (i+1)*30, U=V=0 -> R=G=B=(i+1)*30.
        let mut v4 = Vec::new();
        for i in 0..4u8 {
            let y = (i + 1) * 30;
            v4.extend_from_slice(&[y, y, y, y, 0, 0]); // 6-byte color entry
        }
        // intra vectors chunk (0x3000): selector flag word + one MB using V4.
        // flag word: first bit (MSB) = 1 -> V4 for the single macroblock.
        let mut vectors = Vec::new();
        vectors.extend_from_slice(&[0x80, 0x00, 0x00, 0x00]); // 32-bit flags, top bit set
        vectors.extend_from_slice(&[0, 1, 2, 3]); // four V4 indices
        let frame = cinepak_frame(4, 4, &[(0x2000, v4), (0x3000, vectors)]);

        let out = decompress_cinepak(&frame, 4, 4, 24, &[]).expect("decode");
        assert_eq!(out.len(), 4 * 4 * 3);
        // Quadrant q uses entry q -> color (q+1)*30 across its 2x2 area.
        let px = |x: usize, y: usize| out[(y * 4 + x) * 3];
        assert_eq!(px(0, 0), 30); // entry 0 quadrant TL
        assert_eq!(px(2, 0), 60); // entry 1 quadrant TR
        assert_eq!(px(0, 2), 90); // entry 2 quadrant BL
        assert_eq!(px(2, 2), 120); // entry 3 quadrant BR
    }

    #[test]
    fn cinepak_rejects_zero_dimensions() {
        let err = decompress_cinepak(&[0u8; 10], 0, 4, 24, &[]).expect_err("zero width must fail");
        assert!(matches!(err, BioFormatsError::InvalidData(_)));
    }

    #[test]
    fn cinepak_inter_frame_copies_previous_when_uncoded() {
        // Build a previous frame (solid 50 grayscale 4x4).
        let prev = vec![50u8; 16];
        // Inter strip with a 0x3100 chunk whose single MB is flagged uncoded.
        // flags: top-level coded bit = 0 for the one MB -> copy from prev.
        let mut vectors = Vec::new();
        vectors.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]); // all flags 0
        let mut frame = cinepak_frame(4, 4, &[(0x3100, vectors)]);
        // mark the strip as inter-coded (0x1100)
        frame[10] = 0x11;

        let out = decompress_cinepak(&frame, 4, 4, 8, &prev).expect("decode");
        assert_eq!(out, prev, "uncoded inter MB should copy previous frame");
    }

    /// Encode an RGB8 image to the given format in memory.
    fn encode_rgb(pixels: &[u8], w: u32, h: u32, format: image::ImageFormat) -> Vec<u8> {
        let img = image::RgbImage::from_raw(w, h, pixels.to_vec()).expect("rgb image");
        let mut buf = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgb8(img)
            .write_to(&mut buf, format)
            .expect("encode");
        buf.into_inner()
    }

    #[test]
    fn png_tile_decodes_to_interleaved_rgb() {
        // 2x2 RGB image with distinct pixels.
        let pixels = vec![
            10, 20, 30, // (0,0)
            40, 50, 60, // (1,0)
            70, 80, 90, // (0,1)
            100, 110, 120, // (1,1)
        ];
        let encoded = encode_rgb(&pixels, 2, 2, image::ImageFormat::Png);
        let out = decompress_png(&encoded).expect("PNG decode");
        assert_eq!(out, pixels, "PNG tile must decode to interleaved RGB bytes");
    }

    #[test]
    fn bmp_tile_decodes_to_interleaved_rgb() {
        let pixels = vec![10, 20, 30, 40, 50, 60, 70, 80, 90, 100, 110, 120];
        let encoded = encode_rgb(&pixels, 2, 2, image::ImageFormat::Bmp);
        let out = decompress_bmp(&encoded).expect("BMP decode");
        assert_eq!(out, pixels, "BMP tile must decode to interleaved RGB bytes");
    }

    #[test]
    fn png_decode_rejects_garbage() {
        let err = decompress_png(&[0u8; 16]).expect_err("garbage must fail");
        assert!(matches!(err, BioFormatsError::Codec(_)));
    }

    #[cfg(feature = "jpeg2000-write")]
    #[test]
    fn jpeg2000_roundtrip_gray8_is_lossless() {
        let w = 16u32;
        let h = 12u32;
        // Ramp pattern, one 8-bit gray component.
        let pixels: Vec<u8> = (0..(w * h)).map(|i| (i % 251) as u8).collect();
        let dir = std::env::temp_dir();
        let path = dir.join(format!("bf_jp2_gray_{}.jp2", std::process::id()));
        compress_jpeg2000(&pixels, w, h, 1, 8, false, &path).expect("encode gray JP2");
        let bytes = std::fs::read(&path).expect("read JP2 back");
        let decoded = decompress_jpeg2000(&bytes).expect("decode JP2");
        let _ = std::fs::remove_file(&path);
        assert_eq!(decoded, pixels, "lossless grayscale JP2 must round-trip");
    }

    #[cfg(feature = "jpeg2000-write")]
    #[test]
    fn jpeg2000_roundtrip_rgb8_is_lossless() {
        let w = 10u32;
        let h = 8u32;
        // Interleaved RGB ramp.
        let mut pixels: Vec<u8> = Vec::with_capacity((w * h * 3) as usize);
        for i in 0..(w * h) {
            pixels.push((i % 256) as u8);
            pixels.push(((i * 3) % 256) as u8);
            pixels.push(((i * 7) % 256) as u8);
        }
        let dir = std::env::temp_dir();
        let path = dir.join(format!("bf_jp2_rgb_{}.jp2", std::process::id()));
        compress_jpeg2000(&pixels, w, h, 3, 8, false, &path).expect("encode RGB JP2");
        let bytes = std::fs::read(&path).expect("read JP2 back");
        let decoded = decompress_jpeg2000(&bytes).expect("decode JP2");
        let _ = std::fs::remove_file(&path);
        assert_eq!(decoded, pixels, "lossless RGB JP2 must round-trip");
    }

    #[cfg(feature = "jpeg2000-write")]
    #[test]
    fn jpeg2000_roundtrip_gray16_is_lossless() {
        let w = 8u32;
        let h = 8u32;
        let mut pixels: Vec<u8> = Vec::with_capacity((w * h * 2) as usize);
        for i in 0..(w * h) {
            let v = ((i * 257) % 65536) as u16;
            pixels.extend_from_slice(&v.to_le_bytes());
        }
        let dir = std::env::temp_dir();
        let path = dir.join(format!("bf_jp2_gray16_{}.jp2", std::process::id()));
        compress_jpeg2000(&pixels, w, h, 1, 16, false, &path).expect("encode gray16 JP2");
        let bytes = std::fs::read(&path).expect("read JP2 back");
        let decoded = decompress_jpeg2000(&bytes).expect("decode JP2");
        let _ = std::fs::remove_file(&path);
        assert_eq!(decoded, pixels, "lossless 16-bit grayscale JP2 must round-trip");
    }
}
