use super::error::{BioFormatsError, Result};

/// Decompress LZW-encoded data (TIFF variant — horizontal differencing applied separately).
pub fn decompress_lzw(data: &[u8]) -> Result<Vec<u8>> {
    use weezl::{BitOrder, decode::Decoder};
    let mut decoder = Decoder::with_tiff_size_switch(BitOrder::Msb, 8);
    decoder.decode(data).map_err(|e| BioFormatsError::Codec(e.to_string()))
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

/// Decompress JPEG data.
pub fn decompress_jpeg(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    decoder.decode().map_err(|e| BioFormatsError::Codec(e.to_string()))
}

/// Decompress Zstd data.
pub fn decompress_zstd(data: &[u8]) -> Result<Vec<u8>> {
    zstd::decode_all(data).map_err(BioFormatsError::Io)
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

    // Determine bytes per sample from the first component's precision
    let prec = components[0].precision() as usize;
    let bps = if prec <= 8 { 1 } else if prec <= 16 { 2 } else { 4 };

    let mut out = Vec::with_capacity(width * height * n_components * bps);
    // Interleave components pixel by pixel (RGBRGB...)
    for y in 0..height {
        for x in 0..width {
            for c in 0..n_components {
                let val = components[c].data()[y * width + x];
                match bps {
                    1 => out.push(val as u8),
                    2 => out.extend_from_slice(&(val as u16).to_le_bytes()),
                    _ => out.extend_from_slice(&val.to_le_bytes()),
                }
            }
        }
    }
    Ok(out)
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
    let (width, height) = decoder.get_size()
        .map_err(|e| BioFormatsError::Codec(format!("JPEG-XR size: {e}")))?;
    let format = decoder.get_pixel_format()
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
    decoder.copy_all(&mut buf, stride)
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
        "JPEG-XR support requires the 'jpegxr' feature: cargo build --features jpegxr".into()
    ))
}

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
