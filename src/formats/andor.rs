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

/// Parse the SIF text header. Returns (width, height, num_frames, data_offset_bytes).
fn parse_sif_header(path: &Path) -> Result<(u32, u32, u32, u64)> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut reader = BufReader::new(f);

    let mut line = String::new();

    // First line must start with "Andor Technology"
    reader.read_line(&mut line).map_err(BioFormatsError::Io)?;
    if !line.trim_start().contains("Andor Technology") {
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
        if n == 0 { break; }
        let trimmed = line.trim();

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
                    if w > 0 && h > 0 { width = w; height = h; }
                }
                if let Some(n) = parts.get(12).and_then(|s| s.parse::<u32>().ok()) {
                    if n > 0 { num_frames = n; }
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
                    return Ok((width, height, num_frames, data_offset));
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
    Ok((width, height, num_frames.max(1), data_offset))
}

pub struct AndorSifReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl AndorSifReader {
    pub fn new() -> Self { AndorSifReader { path: None, meta: None, data_offset: 0 } }
}

impl Default for AndorSifReader { fn default() -> Self { Self::new() } }

impl FormatReader for AndorSifReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("sif"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Check for "Andor Technology" in first 64 bytes
        let s = std::str::from_utf8(&header[..header.len().min(64)]).unwrap_or("");
        s.contains("Andor Technology")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (width, height, num_frames, data_offset) = parse_sif_header(path)?;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Andor SIF".into()));

        // SIF stores float32 pixel data
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: num_frames,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Float32,
            bits_per_pixel: 32,
            image_count: num_frames,
            dimension_order: DimensionOrder::XYZCT,
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
        self.data_offset = data_offset;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> { self.path = None; self.meta = None; Ok(()) }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        let bps = 4usize; // float32
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = 4usize;
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize*bps .. x as usize*bps + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
