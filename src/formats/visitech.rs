//! Visitech spinning disk reader.
//!
//! A Visitech dataset is one `.html` index (a "Report" file) plus one or more
//! binary `.xys` pixel files (one channel per file). The HTML report provides
//! the dimensions (sizeX/Y from "Image dimensions"), focal planes (sizeZ from
//! "Number of steps"), bit depth, channel count and time points.
//!
//! Each `.xys` file stores its planes after a `[USE SAME FILE]` marker, padded
//! to a fixed stride. This is a faithful (if partial) port of the upstream Java
//! `VisitechReader`.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

const HEADER_MARKER: &[u8] = b"[USE SAME FILE]";

pub struct VisitechReader {
    path: Option<PathBuf>,
    /// One CoreMetadata per stage position (series), mirroring Java's `core`.
    series_meta: Vec<ImageMetadata>,
    series: usize,
    /// Pixel `.xys` files (one per channel, across all series), in order.
    files: Vec<PathBuf>,
    /// Byte offset of the first plane in each pixel file.
    pixel_offsets: Vec<u64>,
    /// Number of channels per series (Java: sizeC / numSeries).
    channels_per_series: u32,
}

impl VisitechReader {
    pub fn new() -> Self {
        VisitechReader {
            path: None,
            series_meta: Vec::new(),
            series: 0,
            files: Vec::new(),
            pixel_offsets: Vec::new(),
            channels_per_series: 1,
        }
    }
}

impl Default for VisitechReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Strip simple HTML tags from a line of text.
fn strip_tags(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut in_tag = false;
    for ch in s.chars() {
        match ch {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(ch),
            _ => {}
        }
    }
    out
}

/// Parsed metadata from the HTML report.
struct VisitechMeta {
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    pixel_type: PixelType,
    /// Number of stage positions (series), derived from "Microscope XY" keys or
    /// the count of "Document created" markers. Mirrors Java `numSeries`.
    num_series: u32,
}

fn parse_html(html: &str) -> Result<VisitechMeta> {
    // Normalize <br> to newlines, like Java does.
    let normalized = html
        .replace("<br>", "\n")
        .replace("<BR>", "\n")
        .replace("<Br>", "\n")
        .replace("<bR>", "\n");

    let mut size_x = 0u32;
    let mut size_y = 0u32;
    let mut size_z = 0u32;
    let mut size_c = 0u32;
    let mut size_t = 0u32;
    let mut pixel_type = PixelType::Uint16;
    let mut saw_bit_depth = false;
    let mut num_series = 0u32;
    // Java tracks an estimated series count / sizeC from "Document created".
    let mut estimated_series_count = 0u32;
    let mut estimated_size_c = 0u32;

    for raw in normalized.split('\n') {
        let token = strip_tags(raw);
        let token = token.trim();
        if token.is_empty() {
            continue;
        }

        // Java counts each "Document created" line toward the series estimate.
        if token.contains("Document created") {
            estimated_series_count += 1;
            estimated_size_c += 1;
        }

        if let Some(ndx) = token.find(':') {
            let key = token[..ndx].trim();
            let value = token[ndx + 1..].trim();

            if key == "Number of steps" {
                size_z = value.parse().unwrap_or(size_z);
            } else if key.starts_with("Microscope XY") {
                num_series += 1;
            } else if key == "Image bit depth" {
                if let Ok(mut bits) = value.parse::<u32>() {
                    saw_bit_depth = true;
                    while bits % 8 != 0 {
                        bits += 1;
                    }
                    let bytes = bits / 8;
                    pixel_type = match bytes {
                        1 => PixelType::Uint8,
                        2 => PixelType::Uint16,
                        4 => PixelType::Float32,
                        _ => {
                            return Err(BioFormatsError::Format(format!(
                                "Visitech: unsupported image bit depth {value}"
                            )));
                        }
                    };
                }
            } else if key == "Image dimensions" {
                // value looks like "(512, 512)"
                if let Some(comma) = value.find(',') {
                    let xs: String = value[..comma]
                        .chars()
                        .filter(|c| c.is_ascii_digit())
                        .collect();
                    let ys: String = value[comma + 1..]
                        .chars()
                        .filter(|c| c.is_ascii_digit())
                        .collect();
                    size_x = xs.parse().unwrap_or(size_x);
                    size_y = ys.parse().unwrap_or(size_y);
                }
            } else if key.starts_with("Channel Selection") {
                size_c += 1;
            }
        }

        // "<n> pixels" indicates a channel with n planes.
        if token.contains("pixels") {
            size_c += 1;
            if let Some(first) = token.split_whitespace().next() {
                if let Ok(_n) = first.parse::<u32>() {
                    // imageCount accumulation is implicit via Z*C*T below.
                }
            }
        } else if token.starts_with("Time Series") {
            if let Some(semi) = token.find(';') {
                let after = &token[semi + 1..];
                let num: String = after
                    .trim_start()
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
                size_t = num.parse().unwrap_or(size_t);
            }
        }
    }

    // Java: if numSeries == 0, fall back to the "Document created" estimate and
    // multiply sizeC by estimatedSizeC.
    if num_series == 0 {
        num_series = estimated_series_count;
        size_c *= estimated_size_c;
    }
    if size_c == 0 {
        size_c = estimated_size_c;
    }

    if size_x == 0 || size_y == 0 {
        return Err(BioFormatsError::Format(
            "Visitech: report is missing positive image dimensions".into(),
        ));
    }
    if size_z == 0 || size_c == 0 || size_t == 0 || num_series == 0 {
        return Err(BioFormatsError::Format(format!(
            "Visitech: report is missing positive counts (Z={size_z}, C={size_c}, T={size_t}, series={num_series})"
        )));
    }
    if !saw_bit_depth {
        return Err(BioFormatsError::Format(
            "Visitech: report is missing image bit depth".into(),
        ));
    }

    Ok(VisitechMeta {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        num_series,
    })
}

/// Locate the HTML report for a given `.xys` or `.html` entry path.
fn find_html(path: &Path) -> Option<PathBuf> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if ext.as_deref() == Some("html") {
        return Some(path.to_path_buf());
    }
    let parent = path.parent()?;
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    // Java: base = name up to last space; report = "<base> Report.html".
    if let Some(space) = name.rfind(' ') {
        let base = &name[..space];
        let report = parent.join(format!("{base} Report.html"));
        if report.exists() {
            return Some(report);
        }
    }
    // Fall back to the first .html file in the directory.
    std::fs::read_dir(parent).ok().and_then(|rd| {
        rd.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("html"))
                .unwrap_or(false)
        })
    })
}

/// Determine the base name used for `.xys` pixel files, e.g. for
/// "scan 1.xys" -> "scan", for "scan Report.html" -> "scan".
fn pixel_base(entry: &Path, html: &Path) -> String {
    let src = if entry
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("xys"))
        .unwrap_or(false)
    {
        entry
    } else {
        html
    };
    let name = src.file_stem().and_then(|s| s.to_str()).unwrap_or_default();
    match name.rfind(' ') {
        Some(i) => name[..i].to_string(),
        None => name.to_string(),
    }
}

/// Find the offset of the first plane in a `.xys` pixel file.
fn find_pixels_offset(
    path: &Path,
    little_endian: bool,
    plane_size: u64,
    plane_count: u64,
) -> Result<u64> {
    let mut bytes = Vec::new();
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    f.read_to_end(&mut bytes).map_err(BioFormatsError::Io)?;

    // Locate the header marker.
    let marker_pos = bytes
        .windows(HEADER_MARKER.len())
        .position(|w| w == HEADER_MARKER)
        .map(|p| (p + HEADER_MARKER.len()) as u64)
        .ok_or_else(|| {
            BioFormatsError::Format("Visitech: header marker not found in .xys".into())
        })?;

    let len = bytes.len() as u64;
    if plane_count == 0 {
        return Ok(marker_pos);
    }
    let payload_bytes = plane_count.checked_mul(plane_size).ok_or_else(|| {
        BioFormatsError::Format("Visitech: declared payload size overflows".into())
    })?;
    if marker_pos
        .checked_add(payload_bytes)
        .map(|end| end > len)
        .unwrap_or(true)
    {
        return Err(BioFormatsError::Format(format!(
            "Visitech: .xys pixel payload is shorter than declared ({payload_bytes} bytes after marker, file length {len})"
        )));
    }
    let skip = (len.saturating_sub(marker_pos).saturating_sub(payload_bytes)) / plane_count;
    let mut fp = marker_pos + skip;
    // PIXELS_MARKER last byte is 0x3f; nudge forward if present.
    if let Some(&b) = bytes.get(fp as usize) {
        let _ = little_endian;
        if b == 0x3f {
            fp += 1;
        }
    }
    Ok(fp)
}

impl FormatReader for VisitechReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("xys") | Some("html"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let html_path = find_html(path);
        let vmeta = match &html_path {
            Some(hp) => {
                let html = std::fs::read_to_string(hp).map_err(BioFormatsError::Io)?;
                parse_html(&html)?
            }
            None => {
                // No report; fall back to scanning the entry for textual dims.
                let raw = std::fs::read(path).map_err(BioFormatsError::Io)?;
                let text = String::from_utf8_lossy(&raw[..raw.len().min(4096)]).to_string();
                parse_html(&text)?
            }
        };

        // Locate the pixel `.xys` files (one per channel) plus the entry file.
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let base = match &html_path {
            Some(hp) => pixel_base(path, hp),
            None => pixel_base(path, path),
        };

        let mut files: Vec<PathBuf> = Vec::new();
        for i in 0..vmeta.size_c {
            let candidate = parent.join(format!("{base} {}.xys", i + 1));
            if candidate.exists() {
                files.push(candidate);
            }
        }
        // Java always appends currentId, but only the entry .xys carries pixels.
        let entry_is_xys = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("xys"))
            .unwrap_or(false);
        if entry_is_xys && !files.iter().any(|f| f == path) {
            files.push(path.to_path_buf());
        }

        // Only keep .xys files that actually contain the header marker (real
        // pixel data); this also rejects the synthetic text-only test file.
        let plane_count = (vmeta.size_z as u64) * (vmeta.size_t as u64);
        let plane_size = (vmeta.size_x as u64)
            * (vmeta.size_y as u64)
            * vmeta.pixel_type.bytes_per_sample() as u64;

        let mut valid_files = Vec::new();
        let mut offsets = Vec::new();
        for f in &files {
            match find_pixels_offset(f, true, plane_size, plane_count) {
                Ok(off) => {
                    valid_files.push(f.clone());
                    offsets.push(off);
                }
                Err(_) => {}
            }
        }

        if valid_files.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "Visitech XYS does not have any companion .xys pixel data".into(),
            ));
        }

        // Total channels across all series == number of valid pixel files.
        let total_c = valid_files.len() as u32;

        // Java splits the dataset into `numSeries` stage positions, each with
        // sizeC = totalC / numSeries channels. Clamp numSeries so it divides the
        // channel count cleanly (and is never zero).
        let mut num_series = vmeta.num_series.max(1);
        if num_series > total_c {
            num_series = total_c.max(1);
        }
        while num_series > 1 && total_c % num_series != 0 {
            num_series -= 1;
        }
        let channels_per_series = (total_c / num_series).max(1);

        let mut series_meta = Vec::with_capacity(num_series as usize);
        for s in 0..num_series {
            let size_c = channels_per_series;
            let image_count = vmeta.size_z * size_c * vmeta.size_t;
            let mut sm: HashMap<String, MetadataValue> = HashMap::new();
            sm.insert(
                "format".into(),
                MetadataValue::String("Visitech XYS".into()),
            );
            // Java: store.setImageName("Position " + (i + 1), i);
            sm.insert(
                "image_name".into(),
                MetadataValue::String(format!("Position {}", s + 1)),
            );
            series_meta.push(ImageMetadata {
                size_x: vmeta.size_x,
                size_y: vmeta.size_y,
                size_z: vmeta.size_z,
                size_c,
                size_t: vmeta.size_t,
                pixel_type: vmeta.pixel_type,
                bits_per_pixel: (vmeta.pixel_type.bytes_per_sample() * 8) as u8,
                image_count,
                // Java sets dimensionOrder = "XYZTC".
                dimension_order: DimensionOrder::XYZTC,
                is_rgb: false,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: true,
                resolution_count: 1,
                thumbnail: false,
                series_metadata: sm,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
        }

        self.series_meta = series_meta;
        self.series = 0;
        self.channels_per_series = channels_per_series;
        self.files = valid_files;
        self.pixel_offsets = offsets;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series_meta.clear();
        self.series = 0;
        self.files.clear();
        self.pixel_offsets.clear();
        self.channels_per_series = 1;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series_meta.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.series_meta.is_empty() {
            Err(BioFormatsError::NotInitialized)
        } else if s >= self.series_meta.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.series = s;
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.series_meta
            .get(self.series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let series = self.series;
        let meta = self
            .series_meta
            .get(series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let plane =
            (meta.size_x as usize) * (meta.size_y as usize) * meta.pixel_type.bytes_per_sample();
        let div = (meta.size_z * meta.size_t).max(1);
        // Java: fileIndex = series * sizeC + no / div; planeIndex = no % div.
        let file_index = (series as u32 * self.channels_per_series + plane_index / div) as usize;
        let plane_in_file = (plane_index % div) as u64;

        if file_index >= self.files.len() || file_index >= self.pixel_offsets.len() {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let path = self.files[file_index].clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        let base = self.pixel_offsets[file_index];

        // padding between planes (Java: (length - fp - div*plane) / (div-1)).
        let padding = if div > 1 {
            (file_len
                .saturating_sub(base)
                .saturating_sub(div as u64 * plane as u64))
                / (div as u64 - 1)
        } else {
            0
        };

        let offset = base + (plane as u64 + padding) * plane_in_file;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane];
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
        let meta = self
            .series_meta
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::lei::crop_region(&full, meta, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series_meta
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
