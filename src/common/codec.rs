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
pub fn decompress_jpeg(data: &[u8]) -> Result<Vec<u8>> {
    let mut decoder = jpeg_decoder::Decoder::new(data);
    decoder
        .decode()
        .map_err(|e| BioFormatsError::Codec(e.to_string()))
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
    let bps = if prec <= 8 {
        1
    } else if prec <= 16 {
        2
    } else {
        4
    };

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
    let mut decoder = Lzo1xDecoder::new(data);
    decoder.decode()
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
                    if self.ip == self.input.len() {
                        return Ok(std::mem::take(&mut self.output));
                    }
                    return Err(BioFormatsError::InvalidData(
                        "LZO1X: trailing data after end marker".into(),
                    ));
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
}
