//! MicroManager format reader (open-source microscopy platform).
//!
//! MicroManager saves data as:
//!   - `_metadata.txt` (or `metadata.txt`) — JSON with image dimensions
//!   - TIFF files (`MMStack_*.tif`, `img_*.tif`, etc.) — the actual pixel data
//!
//! Detection: file named `*_metadata.txt` or `metadata.txt`.
//! The JSON Summary block contains Width, Height, Channels, Slices, Frames, PixelType.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;
use bioformats_tiff::TiffReader;

// ── Minimal JSON key extractor ────────────────────────────────────────────────
/// Extract the integer value of a JSON key at the top level of the `Summary` block.
/// Handles patterns like `"Width": 512` or `"Width":512`.
fn json_int(json: &str, key: &str) -> Option<i64> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    // Skip whitespace and ':'
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').map(str::trim_start).unwrap_or(rest);
    // Parse integer (may be negative)
    let end = rest.find(|c: char| !c.is_ascii_digit() && c != '-').unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_str(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').map(str::trim_start).unwrap_or(rest);
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn pixel_type_from_str(s: &str) -> PixelType {
    match s.to_uppercase().as_str() {
        "GRAY8" | "RGB8" => PixelType::Uint8,
        "GRAY16" | "RGB16" => PixelType::Uint16,
        "GRAY32" | "RGB32" => PixelType::Float32,
        _ => PixelType::Uint16,
    }
}

fn parse_mm_metadata(json: &str) -> Result<ImageMetadata> {
    // Find the "Summary" block
    let summary_start = json.find("\"Summary\"")
        .ok_or_else(|| BioFormatsError::Format("MicroManager: no Summary key in metadata".into()))?;
    let summary = &json[summary_start..];

    let width = json_int(summary, "Width")
        .ok_or_else(|| BioFormatsError::Format("MicroManager: missing Width".into()))? as u32;
    let height = json_int(summary, "Height")
        .ok_or_else(|| BioFormatsError::Format("MicroManager: missing Height".into()))? as u32;
    let channels = json_int(summary, "Channels").unwrap_or(1).max(1) as u32;
    let slices = json_int(summary, "Slices").unwrap_or(1).max(1) as u32;
    let frames = json_int(summary, "Frames").unwrap_or(1).max(1) as u32;
    let pixel_type_str = json_str(summary, "PixelType").unwrap_or_else(|| "GRAY16".into());
    let pixel_type = pixel_type_from_str(&pixel_type_str);
    let bits = json_int(summary, "BitDepth").unwrap_or(16) as u8;

    let image_count = channels * slices * frames;
    let is_rgb = pixel_type_str.starts_with("RGB");

    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    meta_map.insert("format".into(), MetadataValue::String("MicroManager".into()));
    meta_map.insert("pixel_type_str".into(), MetadataValue::String(pixel_type_str));

    Ok(ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: slices,
        size_c: channels,
        size_t: frames,
        pixel_type,
        bits_per_pixel: bits,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: meta_map,
        lookup_table: None,
    })
}

/// Find the first TIFF file in the same directory as the metadata file.
fn find_first_tiff(meta_path: &Path) -> Option<PathBuf> {
    let dir = meta_path.parent()?;
    let entries = std::fs::read_dir(dir).ok()?;
    for entry in entries.flatten() {
        let p = entry.path();
        if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
            if ext.eq_ignore_ascii_case("tif") || ext.eq_ignore_ascii_case("tiff") {
                return Some(p);
            }
        }
    }
    None
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct MicromanagerReader {
    meta_path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    tiff_reader: Option<TiffReader>,
}

impl MicromanagerReader {
    pub fn new() -> Self {
        MicromanagerReader {
            meta_path: None,
            meta: None,
            tiff_reader: None,
        }
    }
}

impl Default for MicromanagerReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for MicromanagerReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let name = path.file_name().and_then(|n| n.to_str())
            .map(|n| n.to_ascii_lowercase())
            .unwrap_or_default();
        name == "metadata.txt"
            || name.ends_with("_metadata.txt")
            || name == "metadata.json"
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Read and parse JSON metadata
        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut json = String::new();
        BufReader::new(f).read_to_string(&mut json).map_err(BioFormatsError::Io)?;

        let meta = parse_mm_metadata(&json)?;

        // Try to open the companion TIFF for pixel data
        let tiff_reader = find_first_tiff(path).and_then(|tiff_path| {
            let mut r = TiffReader::new();
            r.set_id(&tiff_path).ok()?;
            Some(r)
        });

        self.meta = Some(meta);
        self.tiff_reader = tiff_reader;
        self.meta_path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta_path = None;
        self.meta = None;
        if let Some(mut r) = self.tiff_reader.take() {
            let _ = r.close();
        }
        Ok(())
    }

    fn series_count(&self) -> usize { 1 }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }

    fn series(&self) -> usize { 0 }

    fn metadata(&self) -> &ImageMetadata {
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if let Some(ref mut r) = self.tiff_reader {
            let inner_count = r.metadata().image_count;
            let inner_idx = if inner_count > 0 { plane_index.min(inner_count - 1) } else { 0 };
            r.open_bytes(inner_idx)
        } else {
            Err(BioFormatsError::Format(
                "MicroManager: no companion TIFF file found".into(),
            ))
        }
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if let Some(ref mut r) = self.tiff_reader {
            let inner_count = r.metadata().image_count;
            let inner_idx = if inner_count > 0 { plane_index.min(inner_count - 1) } else { 0 };
            r.open_bytes_region(inner_idx, x, y, w, h)
        } else {
            Err(BioFormatsError::Format(
                "MicroManager: no companion TIFF file found".into(),
            ))
        }
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
