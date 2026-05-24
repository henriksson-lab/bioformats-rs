//! NRRD (Nearly Raw Raster Data) reader and writer.
//!
//! Specification: http://teem.sourceforge.net/nrrd/format.html
//! Supports inline (`.nrrd`) and detached (`.nhdr` + data file) formats.
//! Encoding: raw, gzip. (bzip2 omitted to avoid C deps.)

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::writer::FormatWriter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Raw,
    Gzip,
    Ascii,
}

#[derive(Debug)]
struct NrrdHeader {
    pixel_type: PixelType,
    dimension: usize,
    sizes: Vec<u32>,
    kinds: Vec<String>,
    space_directions: Vec<bool>,
    endian: bool, // true = little-endian
    encoding: Encoding,
    data_file: Option<PathBuf>,
    data_files: Vec<PathBuf>,
    data_offset: u64,
    byte_skip: i64,
    line_skip: usize,
    extra: HashMap<String, String>,
}

#[derive(Debug, Clone)]
struct NrrdAxes {
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    axis_x: Option<usize>,
    axis_y: Option<usize>,
    axis_z: Option<usize>,
    axis_c: Option<usize>,
    axis_t: Option<usize>,
}

impl NrrdAxes {
    fn image_count(&self) -> u32 {
        self.size_z.max(1) * self.size_t.max(1)
    }
}

fn nrrd_pixel_type(t: &str) -> PixelType {
    match t {
        "int8" | "signed char" => PixelType::Int8,
        "uint8" | "uchar" | "unsigned char" => PixelType::Uint8,
        "int16" | "short" | "signed short" | "short int" | "signed short int" => PixelType::Int16,
        "uint16" | "ushort" | "unsigned short" | "unsigned short int" => PixelType::Uint16,
        "int32" | "int" | "signed int" => PixelType::Int32,
        "uint32" | "uint" | "unsigned int" => PixelType::Uint32,
        "float" => PixelType::Float32,
        "double" => PixelType::Float64,
        _ => PixelType::Uint8,
    }
}

fn parse_nrrd_header(path: &Path) -> Result<NrrdHeader> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut reader = BufReader::new(f);

    // First line must be "NRRD00XX"
    let mut first_line = String::new();
    reader
        .read_line(&mut first_line)
        .map_err(BioFormatsError::Io)?;
    if !first_line.trim_start().starts_with("NRRD") {
        return Err(BioFormatsError::Format("Not a NRRD file".into()));
    }

    let mut pixel_type = PixelType::Uint8;
    let mut dimension = 0usize;
    let mut sizes: Vec<u32> = Vec::new();
    let mut little_endian = true;
    let mut encoding = Encoding::Raw;
    let mut data_file: Option<PathBuf> = None;
    let mut data_files: Vec<PathBuf> = Vec::new();
    let mut data_offset = 0u64;
    let mut byte_skip = 0i64;
    let mut line_skip = 0usize;
    let mut kinds: Vec<String> = Vec::new();
    let mut space_directions: Vec<bool> = Vec::new();
    let mut extra: HashMap<String, String> = HashMap::new();
    let mut data_file_list = false;
    let parent = path.parent().unwrap_or(Path::new(".")).to_path_buf();

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(BioFormatsError::Io)?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');

        // Blank line = start of inline data
        if trimmed.is_empty() {
            data_offset = reader.stream_position().map_err(BioFormatsError::Io)?;
            break;
        }

        if data_file_list {
            data_files.push(parent.join(trimmed));
            continue;
        }

        // Skip comments
        if trimmed.starts_with('#') {
            continue;
        }

        // Parse "key: value" or "key:=value"
        let sep_pos = trimmed.find(':');
        if let Some(sep) = sep_pos {
            let key = trimmed[..sep].trim().to_ascii_lowercase();
            let val = trimmed[sep + 1..].trim_start_matches(|c| c == '=' || c == ' ');
            let val = val.trim();

            match key.as_str() {
                "type" => pixel_type = nrrd_pixel_type(val),
                "dimension" => dimension = val.parse().unwrap_or(0),
                "sizes" => {
                    sizes = val
                        .split_ascii_whitespace()
                        .filter_map(|s| s.parse().ok())
                        .collect();
                }
                "kinds" => {
                    kinds = val
                        .split_ascii_whitespace()
                        .map(|s| s.to_ascii_lowercase())
                        .collect();
                }
                "space directions" | "spacedirections" => {
                    space_directions = val
                        .split_ascii_whitespace()
                        .map(|s| !s.eq_ignore_ascii_case("none"))
                        .collect();
                }
                "endian" => {
                    little_endian = val.eq_ignore_ascii_case("little");
                }
                "encoding" => {
                    encoding = match val.to_ascii_lowercase().as_str() {
                        "gzip" | "gz" => Encoding::Gzip,
                        "ascii" | "text" | "txt" => Encoding::Ascii,
                        _ => Encoding::Raw,
                    };
                }
                "data file" | "datafile" => {
                    if val.eq_ignore_ascii_case("LIST") {
                        data_file_list = true;
                    } else {
                        // Resolve relative to the .nhdr file
                        data_file = Some(parent.join(val));
                    }
                }
                "byte skip" | "byteskip" => {
                    byte_skip = val.parse().unwrap_or(0);
                }
                "line skip" | "lineskip" => {
                    line_skip = val.parse().unwrap_or(0);
                }
                _ => {
                    extra.insert(key, val.to_string());
                }
            }
        }
    }

    Ok(NrrdHeader {
        pixel_type,
        dimension,
        sizes,
        kinds,
        space_directions,
        endian: little_endian,
        encoding,
        data_file,
        data_files,
        data_offset,
        byte_skip,
        line_skip,
        extra,
    })
}

fn is_channel_kind(kind: &str) -> bool {
    let k = kind.to_ascii_lowercase();
    k.contains("color") || k.contains("vector") || k == "rgb" || k == "rgba"
}

fn is_time_kind(kind: &str) -> bool {
    kind.to_ascii_lowercase().contains("time")
}

fn derive_axes(hdr: &NrrdHeader) -> NrrdAxes {
    if hdr.kinds.is_empty() && hdr.space_directions.is_empty() {
        return match hdr.sizes.as_slice() {
            [x] => NrrdAxes {
                size_x: *x,
                size_y: 1,
                size_z: 1,
                size_c: 1,
                size_t: 1,
                axis_x: Some(0),
                axis_y: None,
                axis_z: None,
                axis_c: None,
                axis_t: None,
            },
            [x, y] => NrrdAxes {
                size_x: *x,
                size_y: *y,
                size_z: 1,
                size_c: 1,
                size_t: 1,
                axis_x: Some(0),
                axis_y: Some(1),
                axis_z: None,
                axis_c: None,
                axis_t: None,
            },
            [x, y, z] => NrrdAxes {
                size_x: *x,
                size_y: *y,
                size_z: *z,
                size_c: 1,
                size_t: 1,
                axis_x: Some(0),
                axis_y: Some(1),
                axis_z: Some(2),
                axis_c: None,
                axis_t: None,
            },
            [x, y, z, c, ..] => NrrdAxes {
                size_x: *x,
                size_y: *y,
                size_z: *z,
                size_c: *c,
                size_t: 1,
                axis_x: Some(0),
                axis_y: Some(1),
                axis_z: Some(2),
                axis_c: Some(3),
                axis_t: None,
            },
            [] => NrrdAxes {
                size_x: 1,
                size_y: 1,
                size_z: 1,
                size_c: 1,
                size_t: 1,
                axis_x: None,
                axis_y: None,
                axis_z: None,
                axis_c: None,
                axis_t: None,
            },
        };
    }

    let mut axis_c = None;
    let mut axis_t = None;
    let mut spatial = Vec::new();

    for (axis, size) in hdr.sizes.iter().enumerate() {
        let kind = hdr.kinds.get(axis).map(String::as_str).unwrap_or("");
        if is_channel_kind(kind) {
            axis_c = Some(axis);
        } else if is_time_kind(kind) {
            axis_t = Some(axis);
        } else if hdr
            .space_directions
            .get(axis)
            .is_some_and(|has_direction| !*has_direction)
            && *size <= 4
        {
            axis_c = Some(axis);
        } else if hdr.space_directions.get(axis).copied().unwrap_or(false) {
            spatial.push(axis);
        } else if *size > 0 {
            spatial.push(axis);
        }
    }

    let axis_x = spatial.first().copied();
    let axis_y = spatial.get(1).copied();
    let axis_z = spatial.get(2).copied();

    NrrdAxes {
        size_x: axis_x.map(|a| hdr.sizes[a]).unwrap_or(1),
        size_y: axis_y.map(|a| hdr.sizes[a]).unwrap_or(1),
        size_z: axis_z.map(|a| hdr.sizes[a]).unwrap_or(1),
        size_c: axis_c.map(|a| hdr.sizes[a]).unwrap_or(1),
        size_t: axis_t.map(|a| hdr.sizes[a]).unwrap_or(1),
        axis_x,
        axis_y,
        axis_z,
        axis_c,
        axis_t,
    }
}

fn total_sample_count(sizes: &[u32]) -> usize {
    sizes
        .iter()
        .fold(1usize, |acc, size| acc.saturating_mul(*size as usize))
}

fn data_start_offset(path: &Path, base_offset: u64, hdr: &NrrdHeader) -> Result<u64> {
    if hdr.byte_skip < 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "NRRD byte skip -1 is not supported".into(),
        ));
    }

    let mut offset = base_offset;
    if hdr.line_skip > 0 {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut seen = 0usize;
        let mut byte = [0u8; 1];
        while seen < hdr.line_skip {
            if f.read(&mut byte).map_err(BioFormatsError::Io)? == 0 {
                return Err(BioFormatsError::InvalidData(
                    "NRRD line skip exceeds data length".into(),
                ));
            }
            offset += 1;
            if byte[0] == b'\n' {
                seen += 1;
            }
        }
    }

    Ok(offset + hdr.byte_skip as u64)
}

// ---- reader -----------------------------------------------------------------

pub struct NrrdReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    header: Option<NrrdHeader>,
}

impl NrrdReader {
    pub fn new() -> Self {
        NrrdReader {
            path: None,
            meta: None,
            header: None,
        }
    }

    fn read_plane_data(&self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let hdr = self
            .header
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let ics_path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * meta.size_c as usize * bps;
        let plane_offset = plane_index as u64 * plane_bytes as u64;
        let axes = derive_axes(hdr);

        if hdr.data_files.len() == meta.image_count as usize {
            let data_path = &hdr.data_files[plane_index as usize];
            let raw = self.read_nrrd_payload(data_path, 0, hdr, plane_bytes)?;
            let mut buf = raw[..plane_bytes.min(raw.len())].to_vec();
            if buf.len() != plane_bytes {
                return Err(BioFormatsError::InvalidData(
                    "NRRD: detached LIST plane is shorter than expected".into(),
                ));
            }
            if !hdr.endian && bps > 1 {
                for chunk in buf.chunks_exact_mut(bps) {
                    chunk.reverse();
                }
            }
            return Ok(buf);
        }

        let data_sources: Vec<(PathBuf, u64)> = if hdr.data_files.is_empty() {
            vec![(
                hdr.data_file
                    .as_ref()
                    .map(|p| p.clone())
                    .unwrap_or_else(|| ics_path.clone()),
                if hdr.data_file.is_some() {
                    0
                } else {
                    hdr.data_offset
                },
            )]
        } else {
            hdr.data_files.iter().map(|p| (p.clone(), 0)).collect()
        };

        let expected_bytes = total_sample_count(&hdr.sizes) * bps;
        let mut all = Vec::with_capacity(expected_bytes);
        for (data_path, base_offset) in &data_sources {
            let remaining = expected_bytes.saturating_sub(all.len());
            if remaining == 0 {
                break;
            }
            let mut chunk = self.read_nrrd_payload(data_path, *base_offset, hdr, remaining)?;
            all.append(&mut chunk);
        }
        if all.len() < expected_bytes {
            return Err(BioFormatsError::InvalidData(
                "NRRD: data is shorter than expected".into(),
            ));
        }

        let can_slice = axes.axis_x == Some(0)
            && (axes.axis_y == Some(1) || axes.axis_y.is_none())
            && axes.axis_c.is_none()
            && axes.axis_t.map_or(true, |a| a > axes.axis_z.unwrap_or(1))
            && axes.axis_z.map_or(true, |a| a >= 2);
        if can_slice {
            let start = plane_offset as usize;
            let end = start + plane_bytes;
            if end > all.len() {
                return Err(BioFormatsError::InvalidData(
                    "NRRD: plane out of range".into(),
                ));
            }
            let mut buf = all[start..end].to_vec();
            if !hdr.endian && bps > 1 {
                for chunk in buf.chunks_exact_mut(bps) {
                    chunk.reverse();
                }
            }
            return Ok(buf);
        }

        let mut strides = vec![1usize; hdr.sizes.len()];
        for axis in 1..hdr.sizes.len() {
            strides[axis] = strides[axis - 1] * hdr.sizes[axis - 1] as usize;
        }

        let z = plane_index % axes.size_z.max(1);
        let t = plane_index / axes.size_z.max(1);
        let mut buf = vec![0u8; plane_bytes];
        for y in 0..axes.size_y {
            for x in 0..axes.size_x {
                for c in 0..axes.size_c {
                    let mut coords = vec![0u32; hdr.sizes.len()];
                    if let Some(axis) = axes.axis_x {
                        coords[axis] = x;
                    }
                    if let Some(axis) = axes.axis_y {
                        coords[axis] = y;
                    }
                    if let Some(axis) = axes.axis_z {
                        coords[axis] = z;
                    }
                    if let Some(axis) = axes.axis_c {
                        coords[axis] = c;
                    }
                    if let Some(axis) = axes.axis_t {
                        coords[axis] = t;
                    }
                    let sample_index = coords
                        .iter()
                        .zip(strides.iter())
                        .map(|(coord, stride)| *coord as usize * *stride)
                        .sum::<usize>();
                    let src = sample_index * bps;
                    let dst = ((y as usize * axes.size_x as usize + x as usize)
                        * axes.size_c as usize
                        + c as usize)
                        * bps;
                    buf[dst..dst + bps].copy_from_slice(&all[src..src + bps]);
                }
            }
        }
        if !hdr.endian && bps > 1 {
            for chunk in buf.chunks_exact_mut(bps) {
                chunk.reverse();
            }
        }
        Ok(buf)
    }

    fn read_nrrd_payload(
        &self,
        data_path: &Path,
        base_offset: u64,
        hdr: &NrrdHeader,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let mut f = File::open(data_path).map_err(BioFormatsError::Io)?;
        let data_start = data_start_offset(data_path, base_offset, hdr)?;

        let data = match hdr.encoding {
            Encoding::Raw => {
                f.seek(SeekFrom::Start(data_start))
                    .map_err(BioFormatsError::Io)?;
                let mut buf = Vec::new();
                f.read_to_end(&mut buf).map_err(BioFormatsError::Io)?;
                buf.truncate(max_bytes);
                buf
            }
            Encoding::Gzip => {
                f.seek(SeekFrom::Start(data_start))
                    .map_err(BioFormatsError::Io)?;
                let mut dec = flate2::read::GzDecoder::new(f);
                let mut all = Vec::new();
                dec.read_to_end(&mut all).map_err(BioFormatsError::Io)?;
                all.truncate(max_bytes);
                all
            }
            Encoding::Ascii => {
                // Parse whitespace-separated numbers
                f.seek(SeekFrom::Start(data_start))
                    .map_err(BioFormatsError::Io)?;
                let mut text = String::new();
                f.read_to_string(&mut text).map_err(BioFormatsError::Io)?;
                let bps = self
                    .meta
                    .as_ref()
                    .ok_or(BioFormatsError::NotInitialized)?
                    .pixel_type
                    .bytes_per_sample();
                let pixel_type = self
                    .meta
                    .as_ref()
                    .ok_or(BioFormatsError::NotInitialized)?
                    .pixel_type;
                let samples = max_bytes / bps.max(1);
                let mut buf = vec![0u8; max_bytes];
                for (i, token) in text.split_ascii_whitespace().take(samples).enumerate() {
                    let dst = i * bps;
                    match pixel_type {
                        PixelType::Uint8 | PixelType::Int8 => {
                            if let Ok(v) = token.parse::<u8>() {
                                buf[dst] = v;
                            }
                        }
                        PixelType::Uint16 | PixelType::Int16 => {
                            if let Ok(v) = token.parse::<u16>() {
                                buf[dst..dst + 2].copy_from_slice(&v.to_le_bytes());
                            }
                        }
                        PixelType::Float32 => {
                            if let Ok(v) = token.parse::<f32>() {
                                buf[dst..dst + 4].copy_from_slice(&v.to_le_bytes());
                            }
                        }
                        _ => {}
                    }
                }
                buf
            }
        };
        Ok(data)
    }
}

impl Default for NrrdReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NrrdReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "nrrd" | "nhdr"))
            .unwrap_or(false)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"NRRD")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let hdr = parse_nrrd_header(path)?;

        let axes = derive_axes(&hdr);
        let image_count = axes.image_count();

        let mut series_metadata: HashMap<String, MetadataValue> = hdr
            .extra
            .iter()
            .map(|(k, v)| (k.clone(), MetadataValue::String(v.clone())))
            .collect();
        series_metadata.insert(
            "nrrd_dimension".into(),
            MetadataValue::Int(hdr.dimension as i64),
        );
        if !hdr.kinds.is_empty() {
            series_metadata.insert(
                "nrrd_kinds".into(),
                MetadataValue::String(hdr.kinds.join(" ")),
            );
        }
        if !hdr.space_directions.is_empty() {
            series_metadata.insert(
                "nrrd_space_directions".into(),
                MetadataValue::String(
                    hdr.space_directions
                        .iter()
                        .map(|has_direction| if *has_direction { "space" } else { "none" })
                        .collect::<Vec<_>>()
                        .join(" "),
                ),
            );
        }
        if hdr.byte_skip != 0 {
            series_metadata.insert("nrrd_byte_skip".into(), MetadataValue::Int(hdr.byte_skip));
        }
        if hdr.line_skip != 0 {
            series_metadata.insert(
                "nrrd_line_skip".into(),
                MetadataValue::Int(hdr.line_skip as i64),
            );
        }

        let bps = (hdr.pixel_type.bytes_per_sample() * 8) as u8;
        self.meta = Some(ImageMetadata {
            size_x: axes.size_x,
            size_y: axes.size_y,
            size_z: axes.size_z,
            size_c: axes.size_c,
            size_t: axes.size_t,
            pixel_type: hdr.pixel_type,
            bits_per_pixel: bps,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: axes.size_c == 3 || axes.size_c == 4,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: hdr.endian,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.header = Some(hdr);
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.header = None;
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
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.read_plane_data(plane_index)
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
        let spp = meta.size_c as usize;
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = meta.size_x as usize * spp * bps;
        let out_row = w as usize * spp * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * spp * bps;
            out.extend_from_slice(&src[s..s + out_row]);
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

// ---- writer -----------------------------------------------------------------

pub struct NrrdWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
}

impl NrrdWriter {
    pub fn new() -> Self {
        NrrdWriter {
            path: None,
            meta: None,
            planes: Vec::new(),
        }
    }
}

impl Default for NrrdWriter {
    fn default() -> Self {
        Self::new()
    }
}

fn nrrd_type_str(pt: PixelType) -> &'static str {
    match pt {
        PixelType::Int8 => "int8",
        PixelType::Uint8 | PixelType::Bit => "uint8",
        PixelType::Int16 => "int16",
        PixelType::Uint16 => "uint16",
        PixelType::Int32 => "int32",
        PixelType::Uint32 => "uint32",
        PixelType::Float32 => "float",
        PixelType::Float64 => "double",
    }
}

impl FormatWriter for NrrdWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "nrrd" | "nhdr"))
            .unwrap_or(false)
    }
    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        self.meta = Some(meta.clone());
        Ok(())
    }
    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta
            .as_ref()
            .ok_or_else(|| BioFormatsError::Format("set_metadata first".into()))?;
        self.path = Some(path.to_path_buf());
        self.planes.clear();
        Ok(())
    }
    fn save_bytes(&mut self, _: u32, data: &[u8]) -> Result<()> {
        self.planes.push(data.to_vec());
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.take().ok_or(BioFormatsError::NotInitialized)?;
        let f = File::create(&path).map_err(BioFormatsError::Io)?;
        let mut w = std::io::BufWriter::new(f);

        let nz = self.planes.len();
        let bps = meta.pixel_type.bytes_per_sample();

        writeln!(w, "NRRD0004").map_err(BioFormatsError::Io)?;
        writeln!(w, "type: {}", nrrd_type_str(meta.pixel_type)).map_err(BioFormatsError::Io)?;
        let dim = if nz > 1 { 3 } else { 2 };
        writeln!(w, "dimension: {}", dim).map_err(BioFormatsError::Io)?;
        if nz > 1 {
            writeln!(w, "sizes: {} {} {}", meta.size_x, meta.size_y, nz)
                .map_err(BioFormatsError::Io)?;
        } else {
            writeln!(w, "sizes: {} {}", meta.size_x, meta.size_y).map_err(BioFormatsError::Io)?;
        }
        if bps > 1 {
            writeln!(w, "endian: little").map_err(BioFormatsError::Io)?;
        }
        writeln!(w, "encoding: raw").map_err(BioFormatsError::Io)?;
        writeln!(w).map_err(BioFormatsError::Io)?; // blank line → inline data

        for plane in &self.planes {
            w.write_all(plane).map_err(BioFormatsError::Io)?;
        }
        w.flush().map_err(BioFormatsError::Io)?;
        self.planes.clear();
        Ok(())
    }
    fn can_do_stacks(&self) -> bool {
        true
    }
}
