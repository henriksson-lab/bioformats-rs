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
        // Must start with "%!" and contain "PS" in first 32 bytes
        let starts = header.starts_with(b"%!");
        let window = &header[..header.len().min(32)];
        let has_ps = window.windows(2).any(|w| w == b"PS");
        starts && has_ps
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 4 || !self.is_this_type_by_bytes(&data[..data.len().min(64)]) {
            return Err(BioFormatsError::UnsupportedFormat(
                "not an EPS/PostScript raster file".into(),
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
                    width = x1.saturating_sub(x0).max(1) as u32;
                    height = y1.saturating_sub(y0).max(1) as u32;
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
                    width = parts[0].parse::<u32>().unwrap_or(width).max(1);
                    height = parts[1].parse::<u32>().unwrap_or(height).max(1);
                    channels = parts[3].parse::<u32>().unwrap_or(channels).max(1);
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
                            width = x.max(1);
                            height = y.max(1);
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
        self.meta.as_ref().expect("set_id not called")
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
        if x > meta.size_x || y > meta.size_y || x + w > meta.size_x || y + h > meta.size_y {
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

    fn save_bytes(&mut self, _plane_index: u32, data: &[u8]) -> Result<()> {
        self.planes.push(data.to_vec());
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        false
    }
}
