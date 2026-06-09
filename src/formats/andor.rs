//! Andor SIF format reader.
//!
//! SIF is a text-header + binary-data format used by Andor cameras.
//! The header is ASCII text; the pixel data follows after a specific marker.
//! Header format contains image dimensions on lines beginning with "32 ".

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

const MAGIC_STRING: &str = "Andor Technology";
const FOOTER_SIZE: u64 = 8;

#[derive(Debug, Clone)]
struct SifHeader {
    width: u32,
    height: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    image_count: u32,
    data_offset: u64,
    /// Per-plane acquisition timestamps (seconds), one per image, in the order
    /// read from the header lines preceding the coordinate line (SIFReader.java).
    timestamps: Vec<f64>,
}

fn parse_u32_token(token: Option<&&str>, label: &str) -> Result<u32> {
    token
        .ok_or_else(|| BioFormatsError::Format(format!("Andor SIF: missing {label}")))?
        .parse::<u32>()
        .map_err(|_| BioFormatsError::Format(format!("Andor SIF: invalid {label}")))
}

fn parse_i64_token(token: Option<&&str>, label: &str) -> Result<i64> {
    token
        .ok_or_else(|| BioFormatsError::Format(format!("Andor SIF: missing {label}")))?
        .parse::<i64>()
        .map_err(|_| BioFormatsError::Format(format!("Andor SIF: invalid {label}")))
}

fn checked_footer_offset(path: &Path, width: u32, height: u32, image_count: u32) -> Result<u64> {
    let file_len = std::fs::metadata(path).map_err(BioFormatsError::Io)?.len();
    let pixel_bytes = width as u64 * height as u64 * image_count as u64 * 4;
    file_len
        .checked_sub(FOOTER_SIZE)
        .and_then(|len_without_footer| len_without_footer.checked_sub(pixel_bytes))
        .ok_or_else(|| {
            BioFormatsError::Format("Andor SIF: file is shorter than declared pixel payload".into())
        })
}

fn checked_inline_payload(
    path: &Path,
    data_offset: u64,
    width: u32,
    height: u32,
    image_count: u32,
    declared_byte_count: Option<u64>,
) -> Result<()> {
    let pixel_bytes = width as u64 * height as u64 * image_count as u64 * 4;
    if let Some(byte_count) = declared_byte_count {
        if byte_count < pixel_bytes {
            return Err(BioFormatsError::Format(format!(
                "Andor SIF: declared data block has {byte_count} bytes, expected {pixel_bytes}"
            )));
        }
    }
    let file_len = std::fs::metadata(path).map_err(BioFormatsError::Io)?.len();
    if data_offset
        .checked_add(pixel_bytes)
        .is_none_or(|end| end > file_len)
    {
        return Err(BioFormatsError::Format(
            "Andor SIF: file is shorter than declared pixel payload".into(),
        ));
    }
    Ok(())
}

/// Parse the SIF text header using Java Bio-Formats' Pixel number layout when
/// present, with the legacy "32 " acquisition-line parser retained as fallback.
fn parse_sif_header(path: &Path) -> Result<SifHeader> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut reader = BufReader::new(f);

    let mut line = String::new();

    reader.read_line(&mut line).map_err(BioFormatsError::Io)?;
    if !line.starts_with(MAGIC_STRING) {
        return Err(BioFormatsError::Format("Not an Andor SIF file".into()));
    }

    let mut width = 0u32;
    let mut height = 0u32;
    let mut num_frames = 1u32;

    // Scan header lines looking for the image-dimension line.
    // In SIF v4+, a critical line starts with a large integer (like 65538 or similar)
    // and later lines starting with "32 " contain acquisition region info.
    // Pattern: "32 accum_cycles x_start x_end y_start y_end 1 exposure ..."
    // The actual image size is: width = (x_end - x_start + 1) / xbinning
    // We search for the "Ydet " / "Xdet " lines OR the "32 " data lines.
    loop {
        line.clear();
        let n = reader.read_line(&mut line).map_err(BioFormatsError::Io)?;
        if n == 0 {
            break;
        }
        let trimmed = line.trim();

        // Java Bio-Formats SIFReader reads:
        //   Pixel number <C> <X> <Y> <Z> <T>
        // and then a six-coordinate line used to compute the stored plane size.
        if trimmed.starts_with("Pixel number") {
            let parts: Vec<&str> = trimmed.split_ascii_whitespace().collect();
            // SIFReader.java only treats a "Pixel number" line as the dimension
            // line when `tokens.length > 2`. Older Andor SOLIS files contain a
            // decoy "Pixel number6" line (2 tokens) earlier in the header, plus
            // a real "Pixel number65538 1 512 512 1 1 1 ..." line (no space after
            // "number", so token[2] is SizeC). Skip the decoy and keep scanning.
            if parts.len() <= 2 {
                continue;
            }
            let size_c = parse_u32_token(parts.get(2), "SizeC")?;
            let declared_x = parse_u32_token(parts.get(3), "SizeX")?;
            let declared_y = parse_u32_token(parts.get(4), "SizeY")?;
            let size_z = parse_u32_token(parts.get(5), "SizeZ")?;
            let size_t = parse_u32_token(parts.get(6), "SizeT")?;
            let image_count = size_c
                .checked_mul(size_z)
                .and_then(|v| v.checked_mul(size_t))
                .ok_or_else(|| BioFormatsError::Format("Andor SIF: plane count overflow".into()))?;

            line.clear();
            if reader.read_line(&mut line).map_err(BioFormatsError::Io)? == 0 {
                return Err(BioFormatsError::Format(
                    "Andor SIF: missing coordinate line after Pixel number".into(),
                ));
            }
            let coords: Vec<&str> = line.split_ascii_whitespace().collect();
            let x1 = parse_i64_token(coords.get(1), "x1")?;
            let y1 = parse_i64_token(coords.get(2), "y1")?;
            let x2 = parse_i64_token(coords.get(3), "x2")?;
            let y2 = parse_i64_token(coords.get(4), "y2")?;
            let x3 = parse_i64_token(coords.get(5), "x3")?;
            let y3 = parse_i64_token(coords.get(6), "y3")?;
            let computed_width = (x1 - x2).abs() + x3;
            let computed_height = (y1 - y2).abs() + y3;
            let width = u32::try_from(computed_width)
                .ok()
                .filter(|&v| v > 0)
                .ok_or_else(|| {
                    BioFormatsError::Format("Andor SIF: invalid computed width".into())
                })?;
            let height = u32::try_from(computed_height)
                .ok()
                .filter(|&v| v > 0)
                .ok_or_else(|| {
                    BioFormatsError::Format("Andor SIF: invalid computed height".into())
                })?;
            if declared_x == 0 || declared_y == 0 || size_c == 0 || size_z == 0 || size_t == 0 {
                return Err(BioFormatsError::Format(
                    "Andor SIF: Pixel number contains non-positive dimensions".into(),
                ));
            }
            let data_offset = checked_footer_offset(path, width, height, image_count)?;

            // SIFReader.java allocates one timestamp per plane. Its index math
            // (lineNumber - (endLine - imageCount) - 1) only fires for lines
            // strictly before endLine, but endLine equals the "Pixel number"
            // line number, so the timestamp branch is never reached and the
            // array stays all-zero. We replicate that faithfully.
            let timestamps = vec![0.0f64; image_count as usize];

            return Ok(SifHeader {
                width,
                height,
                size_z,
                size_c,
                size_t,
                image_count,
                data_offset,
                timestamps,
            });
        }

        // Look for "Ydet " (height) and "Xdet " (width) labels from older SIF versions
        if trimmed.starts_with("Ydet ") {
            let parts: Vec<&str> = trimmed.split_ascii_whitespace().collect();
            if let Some(v) = parts.get(1).and_then(|s| s.parse::<u32>().ok()) {
                height = v;
            }
        } else if trimmed.starts_with("Xdet ") {
            let parts: Vec<&str> = trimmed.split_ascii_whitespace().collect();
            if let Some(v) = parts.get(1).and_then(|s| s.parse::<u32>().ok()) {
                width = v;
            }
        }

        // The line starting with "32 " contains: 32 <mode> <ncycles> <1> <x1> <x2> <y1> <y2> <xbin> <ybin> <w> <h> <nframes>
        // where w and h are the actual pixel dimensions of the acquired image.
        if trimmed.starts_with("32 ") {
            let parts: Vec<&str> = trimmed.split_ascii_whitespace().collect();
            // Various SIF versions have slightly different layouts, try multiple
            if parts.len() >= 12 {
                if let (Some(w), Some(h)) = (
                    parts.get(10).and_then(|s| s.parse::<u32>().ok()),
                    parts.get(11).and_then(|s| s.parse::<u32>().ok()),
                ) {
                    if w > 0 && h > 0 {
                        width = w;
                        height = h;
                    }
                }
                if let Some(n) = parts.get(12).and_then(|s| s.parse::<u32>().ok()) {
                    if n > 0 {
                        num_frames = n;
                    }
                }
            }
        }

        // The binary data section starts after a line that is just a single integer
        // (the byte count of the data), or after we've seen the header end marker.
        // A common marker: a line that parses as a large integer on its own.
        if trimmed.chars().all(|c| c.is_ascii_digit()) {
            if let Ok(byte_count) = trimmed.parse::<u64>() {
                if byte_count > 0 && width > 0 && height > 0 {
                    let data_offset = reader.stream_position().map_err(BioFormatsError::Io)?;
                    let image_count = num_frames.max(1);
                    checked_inline_payload(
                        path,
                        data_offset,
                        width,
                        height,
                        image_count,
                        Some(byte_count),
                    )?;
                    return Ok(SifHeader {
                        width,
                        height,
                        size_z: image_count,
                        size_c: 1,
                        size_t: 1,
                        image_count,
                        data_offset,
                        timestamps: vec![0.0f64; image_count as usize],
                    });
                }
            }
        }
    }

    if width == 0 || height == 0 {
        return Err(BioFormatsError::Format(
            "Andor SIF: could not determine image dimensions from header".into(),
        ));
    }
    // Fallback data offset: end of file? Try 0 with a warning
    let data_offset = reader.stream_position().map_err(BioFormatsError::Io)?;
    let image_count = num_frames.max(1);
    checked_inline_payload(path, data_offset, width, height, image_count, None)?;
    Ok(SifHeader {
        width,
        height,
        size_z: image_count,
        size_c: 1,
        size_t: 1,
        image_count,
        data_offset,
        timestamps: vec![0.0f64; image_count as usize],
    })
}

pub struct AndorSifReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
    timestamps: Vec<f64>,
}

impl AndorSifReader {
    pub fn new() -> Self {
        AndorSifReader {
            path: None,
            meta: None,
            data_offset: 0,
            timestamps: Vec::new(),
        }
    }
}

impl Default for AndorSifReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for AndorSifReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("sif"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(MAGIC_STRING.as_bytes())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let header = parse_sif_header(path)?;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Andor SIF".into()));
        meta_map.insert(
            "sif.pixel_offset".into(),
            MetadataValue::Int(header.data_offset as i64),
        );

        // SIF stores float32 pixel data
        self.meta = Some(ImageMetadata {
            size_x: header.width,
            size_y: header.height,
            size_z: header.size_z,
            size_c: header.size_c,
            size_t: header.size_t,
            pixel_type: PixelType::Float32,
            bits_per_pixel: 32,
            image_count: header.image_count,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.data_offset = header.data_offset;
        self.timestamps = header.timestamps;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
        self.timestamps.clear();
        Ok(())
    }
    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s == 0 && self.meta.is_some() {
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = 4usize; // float32
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
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
        let meta = self.meta.as_ref().unwrap();
        let bps = 4usize;
        if x.checked_add(w).is_none_or(|end| end > meta.size_x)
            || y.checked_add(h).is_none_or(|end| end > meta.size_y)
        {
            return Err(BioFormatsError::InvalidData(
                "Andor SIF: requested region is outside the image".into(),
            ));
        }
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{OmeMetadata, OmePlane};
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        // Java's MetadataTools.populatePixels names the image after the file.
        if let Some(name) = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
        {
            ome.images[0].name = Some(name.to_string());
        }
        // SIFReader.java calls store.setPlaneDeltaT(timestamp[i], 0, i) for each
        // plane (XYCZT order). Mirror that with the parsed (zero-filled) values.
        if !self.timestamps.is_empty() {
            let img = &mut ome.images[0];
            img.planes = (0..meta.image_count)
                .map(|i| {
                    let c = i % meta.size_c;
                    let z = (i / meta.size_c) % meta.size_z;
                    let t = i / (meta.size_c * meta.size_z);
                    OmePlane {
                        the_z: z,
                        the_c: c,
                        the_t: t,
                        delta_t: self.timestamps.get(i as usize).copied(),
                        ..Default::default()
                    }
                })
                .collect();
        }
        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!("bioformats_andor_{}_{}", std::process::id(), name))
    }

    #[test]
    fn java_pixel_number_header_uses_footer_relative_pixel_offset() {
        let path = tmp("pixel_number.sif");
        let mut data = Vec::new();
        data.extend_from_slice(b"Andor Technology Multi-Channel File\n");
        data.extend_from_slice(b"Some original metadata\n");
        data.extend_from_slice(b"Pixel number 2 4 9 1 1\n");
        data.extend_from_slice(b"0 1 1 2 4 1 1\n");
        data.extend_from_slice(b"padding before pixels ignored by footer math\n");

        let mut plane0 = Vec::new();
        let mut plane1 = Vec::new();
        for value in 1u32..=8 {
            plane0.extend_from_slice(&(value as f32).to_le_bytes());
        }
        for value in 101u32..=108 {
            plane1.extend_from_slice(&(value as f32).to_le_bytes());
        }
        data.extend_from_slice(&plane0);
        data.extend_from_slice(&plane1);
        data.extend_from_slice(&[0u8; FOOTER_SIZE as usize]);
        std::fs::write(&path, data).unwrap();

        let mut reader = AndorSifReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 4);
        assert_eq!(meta.size_c, 2);
        assert_eq!(meta.size_z, 1);
        assert_eq!(meta.size_t, 1);
        assert_eq!(meta.image_count, 2);
        assert_eq!(meta.dimension_order, DimensionOrder::XYCZT);
        assert_eq!(reader.open_bytes(0).unwrap(), plane0);
        assert_eq!(reader.open_bytes(1).unwrap(), plane1);
    }

    #[test]
    fn sif_detection_matches_java_magic_at_start() {
        let reader = AndorSifReader::new();
        assert!(reader.is_this_type_by_bytes(b"Andor Technology"));
        assert!(!reader.is_this_type_by_bytes(b"prefix Andor Technology"));
    }
}
