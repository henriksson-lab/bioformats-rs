//! EPS/PostScript format reader.
//!
//! This reader supports the Java Bio-Formats raster subset: EPS files with
//! inline `image`/`colorimage` pixel payloads. Vector-only PostScript still
//! requires a full interpreter and remains unsupported.

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

pub struct EpsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Vec<u8>,
}

impl EpsReader {
    pub fn new() -> Self {
        EpsReader {
            path: None,
            meta: None,
            pixels: Vec::new(),
        }
    }

    /// Attempt to read the embedded TIFF preview of a DOS EPS file. Returns
    /// `Ok(None)` if no valid TIFF preview is present.
    fn try_tiff_preview(&self, data: &[u8]) -> Result<Option<(ImageMetadata, Vec<u8>)>> {
        // DOS EPS magic: C5 D0 D3 C6. The header (little-endian) stores the
        // TIFF preview offset at byte 20 and length at byte 24.
        if data.len() < 30 {
            return Ok(None);
        }
        let offset = u32::from_le_bytes([data[20], data[21], data[22], data[23]]) as usize;
        let len = u32::from_le_bytes([data[24], data[25], data[26], data[27]]) as usize;
        if offset == 0 || len == 0 || offset + len > data.len() {
            return Ok(None);
        }
        let tiff_bytes = &data[offset..offset + len];
        // Validate a TIFF byte-order/magic signature.
        let is_tiff = tiff_bytes.len() >= 4
            && ((tiff_bytes[0] == b'I' && tiff_bytes[1] == b'I' && tiff_bytes[2] == 42)
                || (tiff_bytes[0] == b'M' && tiff_bytes[1] == b'M' && tiff_bytes[3] == 42));
        if !is_tiff {
            return Ok(None);
        }

        // Delegate to the project TiffReader via a temporary file, since it
        // reads from a path.
        let mut tmp = std::env::temp_dir();
        tmp.push(format!("bf_eps_preview_{}.tif", std::process::id()));
        std::fs::write(&tmp, tiff_bytes).map_err(BioFormatsError::Io)?;

        let result = (|| -> Result<(ImageMetadata, Vec<u8>)> {
            let mut reader = crate::tiff::TiffReader::new();
            reader.set_id(&tmp)?;
            let mut meta = reader.metadata().clone();
            meta.dimension_order = DimensionOrder::XYCZT;
            meta.image_count = 1;
            meta.size_z = 1;
            meta.size_t = 1;
            let pixels = reader.open_bytes(0)?;
            Ok((meta, pixels))
        })();

        let _ = std::fs::remove_file(&tmp);
        result.map(Some)
    }
}

impl Default for EpsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn line_end(bytes: &[u8], mut pos: usize) -> usize {
    while pos < bytes.len() && bytes[pos] != b'\n' && bytes[pos] != b'\r' {
        pos += 1;
    }
    pos
}

fn next_line_start(bytes: &[u8], mut pos: usize) -> usize {
    if pos < bytes.len() && bytes[pos] == b'\r' {
        pos += 1;
        if pos < bytes.len() && bytes[pos] == b'\n' {
            pos += 1;
        }
    } else if pos < bytes.len() && bytes[pos] == b'\n' {
        pos += 1;
    }
    pos
}

fn line_text(bytes: &[u8], start: usize, end: usize) -> String {
    String::from_utf8_lossy(&bytes[start..end])
        .trim()
        .to_string()
}

fn parse_eps_int(value: &str) -> Option<i32> {
    value.parse::<i32>().ok()
}

fn parse_positive_eps_u32(value: &str, label: &str) -> Result<u32> {
    let parsed = value
        .parse::<i64>()
        .map_err(|_| BioFormatsError::UnsupportedFormat(format!("EPS invalid {label}")))?;
    u32::try_from(parsed)
        .ok()
        .filter(|&v| v > 0)
        .ok_or_else(|| BioFormatsError::UnsupportedFormat(format!("EPS invalid {label}")))
}

fn parse_hex_payload(bytes: &[u8], offset: usize, expected: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(expected);
    let mut high: Option<u8> = None;
    for &byte in &bytes[offset..] {
        let Some(nibble) = (byte as char).to_digit(16).map(|v| v as u8) else {
            continue;
        };
        if let Some(h) = high.take() {
            out.push((h << 4) | nibble);
            if out.len() == expected {
                return Ok(out);
            }
        } else {
            high = Some(nibble);
        }
    }
    Err(BioFormatsError::InvalidData(format!(
        "EPS raster payload ended after {} bytes, expected {}",
        out.len(),
        expected
    )))
}

impl FormatReader for EpsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("eps") | Some("epsi") | Some("ps"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 4 {
            return false;
        }
        // DOS EPS binary header with embedded TIFF preview: C5 D0 D3 C6.
        if header[0..4] == [0xC5, 0xD0, 0xD3, 0xC6] {
            return true;
        }
        // Must start with "%!" and contain "PS" in first 32 bytes.
        let starts = header.starts_with(b"%!");
        let window = &header[..header.len().min(32)];
        let has_ps = window.windows(2).any(|w| w == b"PS");
        starts && has_ps
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = fs::read(path).map_err(BioFormatsError::Io)?;

        // If the file does not start with "%!PS" it is a DOS EPS binary with an
        // embedded TIFF preview (per the Java EPSReader). The DOS EPS header
        // stores the TIFF offset/length as little-endian ints at byte 20/24.
        let starts_with_ps = data
            .iter()
            .take_while(|&&b| b == b' ' || b == b'\t')
            .count();
        let is_ps = data[starts_with_ps..]
            .iter()
            .take(4)
            .copied()
            .eq(b"%!PS".iter().copied());
        if !is_ps {
            if let Some((meta, pixels)) = self.try_tiff_preview(&data)? {
                self.meta = Some(meta);
                self.pixels = pixels;
                self.path = Some(path.to_path_buf());
                return Ok(());
            }
            return Err(BioFormatsError::UnsupportedFormat(
                "EPS: not a PostScript file and no embedded TIFF preview found".into(),
            ));
        }

        let mut width = 0u32;
        let mut height = 0u32;
        let mut channels = 1u32;
        let mut binary = false;
        let mut data_offset = None;
        let mut metadata = HashMap::new();
        metadata.insert(
            "format".into(),
            MetadataValue::String("Encapsulated PostScript".into()),
        );

        let mut pos = 0usize;
        while pos < data.len() {
            let end = line_end(&data, pos);
            let line = line_text(&data, pos, end);
            let trimmed = line.trim();
            let fields: Vec<&str> = trimmed.split_whitespace().collect();

            if let Some(rest) = trimmed.strip_prefix("%%BoundingBox:") {
                let bb: Vec<&str> = rest.split_whitespace().collect();
                if bb.len() >= 4 {
                    let x0 = parse_eps_int(bb[0]).ok_or_else(|| {
                        BioFormatsError::UnsupportedFormat(
                            "EPS BoundingBox is not integer-valued".into(),
                        )
                    })?;
                    let y0 = parse_eps_int(bb[1]).ok_or_else(|| {
                        BioFormatsError::UnsupportedFormat(
                            "EPS BoundingBox is not integer-valued".into(),
                        )
                    })?;
                    let x1 = parse_eps_int(bb[2]).ok_or_else(|| {
                        BioFormatsError::UnsupportedFormat(
                            "EPS BoundingBox is not integer-valued".into(),
                        )
                    })?;
                    let y1 = parse_eps_int(bb[3]).ok_or_else(|| {
                        BioFormatsError::UnsupportedFormat(
                            "EPS BoundingBox is not integer-valued".into(),
                        )
                    })?;
                    if x1 <= x0 || y1 <= y0 {
                        return Err(BioFormatsError::UnsupportedFormat(
                            "EPS BoundingBox has non-positive dimensions".into(),
                        ));
                    }
                    width = u32::try_from(x1 - x0).map_err(|_| {
                        BioFormatsError::UnsupportedFormat("EPS BoundingBox is too large".into())
                    })?;
                    height = u32::try_from(y1 - y0).map_err(|_| {
                        BioFormatsError::UnsupportedFormat("EPS BoundingBox is too large".into())
                    })?;
                    metadata.insert(
                        "X-coordinate of origin".into(),
                        MetadataValue::Int(x0 as i64),
                    );
                    metadata.insert(
                        "Y-coordinate of origin".into(),
                        MetadataValue::Int(y0 as i64),
                    );
                }
            } else if let Some(rest) = trimmed.strip_prefix("%ImageData:") {
                let parts: Vec<&str> = rest.split_whitespace().collect();
                if parts.len() >= 4 {
                    width = parse_positive_eps_u32(parts[0], "ImageData width")?;
                    height = parse_positive_eps_u32(parts[1], "ImageData height")?;
                    channels = parse_positive_eps_u32(parts[3], "ImageData channel count")?;
                }
            } else if trimmed.starts_with("%%BeginBinary") {
                binary = true;
            } else if trimmed.ends_with("colorimage") {
                channels = 3;
                data_offset = Some(next_line_start(&data, end));
                break;
            } else if trimmed == "image" || trimmed.ends_with(" image") {
                if fields.len() >= 3 {
                    if let (Ok(x), Ok(y), Ok(bits)) = (
                        fields[0].parse::<u32>(),
                        fields[1].parse::<u32>(),
                        fields[2].parse::<u32>(),
                    ) {
                        if bits >= 8 {
                            if x == 0 || y == 0 {
                                return Err(BioFormatsError::UnsupportedFormat(
                                    "EPS image operator has non-positive dimensions".into(),
                                ));
                            }
                            width = x;
                            height = y;
                        }
                    }
                }
                data_offset = Some(next_line_start(&data, end));
                break;
            } else if trimmed.starts_with("%%") {
                if let Some((key, value)) = trimmed.split_once(':') {
                    metadata.insert(
                        key.trim_start_matches('%').to_string(),
                        MetadataValue::String(value.trim().to_string()),
                    );
                }
            }
            pos = next_line_start(&data, end);
        }

        let Some(offset) = data_offset else {
            return Err(BioFormatsError::UnsupportedFormat(
                "EPS vector data without inline image pixels is not supported".into(),
            ));
        };
        if width == 0 || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "EPS raster dimensions were not found".into(),
            ));
        }
        if channels != 1 && channels != 3 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "EPS reader supports 1 or 3 channels, got {}",
                channels
            )));
        }

        let expected = width as usize * height as usize * channels as usize;
        let pixels = if binary {
            let end = offset.checked_add(expected).ok_or_else(|| {
                BioFormatsError::InvalidData("EPS binary payload size overflow".into())
            })?;
            if end > data.len() {
                return Err(BioFormatsError::InvalidData(format!(
                    "EPS binary payload ended after {} bytes, expected {}",
                    data.len().saturating_sub(offset),
                    expected
                )));
            }
            data[offset..end].to_vec()
        } else {
            parse_hex_payload(&data, offset, expected)?
        };

        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: channels,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: channels == 3,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.pixels = pixels;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels.clear();
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
        self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Ok(self.pixels.clone())
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
        let meta = self.meta.as_ref().unwrap();
        if x.checked_add(w).is_none_or(|end| end > meta.size_x)
            || y.checked_add(h).is_none_or(|end| end > meta.size_y)
        {
            return Err(BioFormatsError::InvalidData(
                "EPS requested region is outside image bounds".into(),
            ));
        }
        let spp = meta.size_c as usize;
        let row = meta.size_x as usize * spp;
        let out_row = w as usize * spp;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            let start = x as usize * spp;
            out.extend_from_slice(&src[start..start + out_row]);
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
}

// ---------------------------------------------------------------------------
// EPS Writer
// ---------------------------------------------------------------------------

use crate::common::writer::FormatWriter;
use std::io::Write;

pub struct EpsWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
}

impl EpsWriter {
    pub fn new() -> Self {
        EpsWriter {
            path: None,
            meta: None,
            planes: Vec::new(),
        }
    }
}

impl Default for EpsWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for EpsWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("eps") | Some("epsi") | Some("ps"))
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        let logical_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        let required_planes = meta
            .size_z
            .max(1)
            .checked_mul(logical_c)
            .and_then(|v| v.checked_mul(meta.size_t.max(1)))
            .ok_or_else(|| BioFormatsError::Format("EPS writer plane count overflow".into()))?;
        if required_planes > 1 || meta.image_count > 1 {
            return Err(BioFormatsError::UnsupportedFormat(
                "EPS writer supports only one plane".into(),
            ));
        }
        if meta.pixel_type != PixelType::Uint8 {
            return Err(BioFormatsError::UnsupportedFormat(
                "EPS writer supports only 8-bit pixel data".into(),
            ));
        }
        if meta.size_c != 1 && !(meta.is_rgb && meta.size_c == 3) {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "EPS writer supports grayscale (1) or RGB (3), got spp={}",
                meta.size_c
            )));
        }
        self.meta = Some(meta.clone());
        self.planes.clear();
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta.as_ref().ok_or_else(|| {
            BioFormatsError::Format("set_metadata must be called before set_id".into())
        })?;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        if self.planes.is_empty() {
            return Err(BioFormatsError::Format("no planes written".into()));
        }

        let width = meta.size_x;
        let height = meta.size_y;
        let spp = meta.size_c as usize;

        // Only 8-bit grayscale or RGB supported
        if meta.pixel_type != PixelType::Uint8 {
            return Err(BioFormatsError::UnsupportedFormat(
                "EPS writer supports only 8-bit pixel data".into(),
            ));
        }
        if spp != 1 && spp != 3 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "EPS writer supports grayscale (1) or RGB (3), got spp={}",
                spp
            )));
        }

        let row_bytes = width as usize * spp;
        let bits = 8u32;
        let data = &self.planes[0];

        let mut f = std::fs::File::create(path).map_err(BioFormatsError::Io)?;

        writeln!(f, "%!PS-Adobe-3.0 EPSF-3.0").map_err(BioFormatsError::Io)?;
        writeln!(f, "%%BoundingBox: 0 0 {} {}", width, height).map_err(BioFormatsError::Io)?;
        writeln!(f, "%%EndComments").map_err(BioFormatsError::Io)?;

        if spp == 1 {
            // Grayscale: use `image` operator
            writeln!(
                f,
                "{} {} {} [{} 0 0 -{} 0 {}]",
                width, height, bits, width, height, height
            )
            .map_err(BioFormatsError::Io)?;
            writeln!(
                f,
                "{{currentfile {} string readhexstring pop}}",
                row_bytes * 2
            )
            .map_err(BioFormatsError::Io)?;
            writeln!(f, "image").map_err(BioFormatsError::Io)?;
        } else {
            // RGB: use `colorimage` operator
            writeln!(
                f,
                "{} {} {} [{} 0 0 -{} 0 {}]",
                width, height, bits, width, height, height
            )
            .map_err(BioFormatsError::Io)?;
            writeln!(
                f,
                "{{currentfile {} string readhexstring pop}}",
                row_bytes * 2
            )
            .map_err(BioFormatsError::Io)?;
            writeln!(f, "false 3 colorimage").map_err(BioFormatsError::Io)?;
        }

        // Hex-encode pixel data
        for byte in data.iter() {
            write!(f, "{:02X}", byte).map_err(BioFormatsError::Io)?;
        }
        writeln!(f).map_err(BioFormatsError::Io)?;

        writeln!(f, "showpage").map_err(BioFormatsError::Io)?;
        writeln!(f, "%%EOF").map_err(BioFormatsError::Io)?;

        self.path = None;
        self.meta = None;
        self.planes.clear();
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if !self.planes.is_empty() {
            return Err(BioFormatsError::Format(
                "EPS writer supports only one plane".into(),
            ));
        }
        let expected = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|px| px.checked_mul(meta.size_c as usize))
            .and_then(|samples| samples.checked_mul(meta.pixel_type.bytes_per_sample()))
            .ok_or_else(|| BioFormatsError::Format("EPS image plane is too large".into()))?;
        if data.len() != expected {
            return Err(BioFormatsError::Format(format!(
                "EPS writer: plane 0 has {} bytes, expected {}",
                data.len(),
                expected
            )));
        }
        self.planes.push(data.to_vec());
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        false
    }
}
