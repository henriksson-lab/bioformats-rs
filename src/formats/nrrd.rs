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
use crate::common::path::confined_join;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use crate::common::writer::FormatWriter;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Encoding {
    Raw,
    Gzip,
    Ascii,
    /// bzip2-compressed payload. The Java NRRDReader does *not* support this
    /// encoding (it only handles "raw" and "gzip" and otherwise throws
    /// UnsupportedCompressionException); we recognise the keyword so we can
    /// emit a precise error. Decoding it would require a `bzip2` decoder crate,
    /// which is not a direct dependency of this crate.
    Bzip2,
    /// Any other encoding keyword the file declares. Java throws
    /// UnsupportedCompressionException for these.
    Unsupported,
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

fn resolve_nrrd_data_path(parent: &Path, value: &str, java_detached_file: bool) -> Result<PathBuf> {
    let value = if java_detached_file {
        match value.find(std::path::MAIN_SEPARATOR) {
            Some(i) => &value[i + 1..],
            None => value,
        }
    } else {
        value
    };
    confined_join(parent, value).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(
            "NRRD detached data path must stay within the header directory".into(),
        )
    })
}

impl NrrdAxes {
    fn image_count(&self) -> u32 {
        self.size_z.max(1) * self.size_t.max(1)
    }
}

fn nrrd_pixel_type(t: &str) -> Result<PixelType> {
    // Mirror NRRDReader.java: any type containing "char" or "8" maps to UINT8,
    // any containing "short" or "16" maps to UINT16, the int/uint family maps
    // to UINT32. NRRD/MetaImage treats these as unsigned regardless of the
    // declared signedness (e.g. "int8"/"signed char" → UINT8, "short" → UINT16,
    // "int32" → UINT32).
    let v = t.to_ascii_lowercase();
    if v.contains("char") || v.contains('8') {
        Ok(PixelType::Uint8)
    } else if v.contains("short") || v.contains("16") {
        Ok(PixelType::Uint16)
    } else if matches!(
        v.as_str(),
        "int"
            | "signed int"
            | "int32"
            | "int32_t"
            | "uint"
            | "unsigned int"
            | "uint32"
            | "uint32_t"
    ) {
        Ok(PixelType::Uint32)
    } else if v == "float" {
        Ok(PixelType::Float32)
    } else if v == "double" {
        Ok(PixelType::Float64)
    } else {
        Err(BioFormatsError::Format(format!(
            "Unsupported data type: {t}"
        )))
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
            data_files.push(resolve_nrrd_data_path(&parent, trimmed, false)?);
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
                "type" => pixel_type = nrrd_pixel_type(val)?,
                "dimension" => {
                    dimension = val.parse().map_err(|_| {
                        BioFormatsError::Format(format!("NRRD: invalid dimension value {val:?}"))
                    })?;
                }
                "sizes" => {
                    sizes.clear();
                    for token in val.split_ascii_whitespace() {
                        sizes.push(token.parse().map_err(|_| {
                            BioFormatsError::Format(format!("NRRD: invalid size value {token:?}"))
                        })?);
                    }
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
                    // Java NRRDReader only recognises "raw" and "gzip"; every
                    // other encoding throws UnsupportedCompressionException at
                    // openBytes time. We additionally accept "ascii"/"text"
                    // (a faithful-to-spec extension already implemented here)
                    // and classify "bzip2"/"bz2" so we can report the missing
                    // decoder precisely.
                    encoding = match val.to_ascii_lowercase().as_str() {
                        "raw" => Encoding::Raw,
                        "gzip" | "gz" => Encoding::Gzip,
                        "ascii" | "text" | "txt" => Encoding::Ascii,
                        "bzip2" | "bz2" => Encoding::Bzip2,
                        _ => Encoding::Unsupported,
                    };
                }
                "data file" | "datafile" => {
                    if val.eq_ignore_ascii_case("LIST") {
                        data_file_list = true;
                    } else {
                        data_file = Some(resolve_nrrd_data_path(&parent, val, true)?);
                    }
                }
                "byte skip" | "byteskip" => {
                    byte_skip = val.parse().map_err(|_| {
                        BioFormatsError::Format(format!("NRRD: invalid byte skip value {val:?}"))
                    })?;
                }
                "line skip" | "lineskip" => {
                    line_skip = val.parse().map_err(|_| {
                        BioFormatsError::Format(format!("NRRD: invalid line skip value {val:?}"))
                    })?;
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

/// Derive the X/Y/Z/C/T axis sizes from the NRRD header.
///
/// Mirrors `NRRDReader.java` (initFile, lines ~308-328) exactly. Java applies a
/// single positional rule per axis index `i` (where `numDimensions` is the
/// declared `dimension:` field, falling back to the number of `sizes`):
///
/// ```text
/// if numDimensions >= 3 && i == 0 && size > 1 && size <= 16 -> sizeC = size
/// else if i == 0 || (sizeC > 1 && i == 1) -> sizeX = size
/// else if i == 1 || (sizeC > 1 && i == 2) -> sizeY = size
/// else if i == 2 || (sizeC > 1 && i == 3) -> sizeZ = size
/// else if i == 3 || (sizeC > 1 && i == 4) -> sizeT = size
/// ```
///
/// The dimension order is always `XYCZT` (NRRDReader.java line 277). There is no
/// `kinds`/`space directions` based axis detection in Java, so this function
/// does not use those fields for axis assignment.
fn derive_axes(hdr: &NrrdHeader) -> NrrdAxes {
    // Java uses `numDimensions` (the declared `dimension:` value); when absent
    // or inconsistent, fall back to the number of parsed sizes.
    let num_dimensions = if hdr.dimension > 0 && hdr.dimension <= hdr.sizes.len() {
        hdr.dimension
    } else {
        hdr.sizes.len()
    };

    let mut size_x = 1u32;
    let mut size_y = 1u32;
    let mut size_z = 1u32;
    let mut size_c = 1u32;
    let mut size_t = 1u32;

    let mut axis_x = None;
    let mut axis_y = None;
    let mut axis_z = None;
    let mut axis_c = None;
    let mut axis_t = None;

    for i in 0..num_dimensions {
        let size = hdr.sizes[i];

        if num_dimensions >= 3 && i == 0 && size > 1 && size <= 16 {
            size_c = size;
            axis_c = Some(i);
        } else if i == 0 || (size_c > 1 && i == 1) {
            size_x = size;
            axis_x = Some(i);
        } else if i == 1 || (size_c > 1 && i == 2) {
            size_y = size;
            axis_y = Some(i);
        } else if i == 2 || (size_c > 1 && i == 3) {
            size_z = size;
            axis_z = Some(i);
        } else if i == 3 || (size_c > 1 && i == 4) {
            size_t = size;
            axis_t = Some(i);
        }
    }

    NrrdAxes {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        axis_x,
        axis_y,
        axis_z,
        axis_c,
        axis_t,
    }
}

fn total_sample_count(sizes: &[u32]) -> Result<usize> {
    sizes.iter().try_fold(1usize, |acc, size| {
        acc.checked_mul(*size as usize)
            .ok_or_else(|| BioFormatsError::InvalidData("NRRD: total sample count overflow".into()))
    })
}

fn data_start_offset(
    path: &Path,
    base_offset: u64,
    hdr: &NrrdHeader,
    has_external_data: bool,
) -> Result<u64> {
    if hdr.byte_skip < 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "NRRD byte skip -1 is not supported".into(),
        ));
    }

    // Per NRRDReader.java (raw encoding):
    //   - external data file: offset = byteSkip (absolute), and "line skip" is
    //     NOT applied — the reader simply seeks to `offset + no * planeSize`.
    //   - inline data: offset = post-header file pointer; byte skip is
    //     overwritten/ignored.
    // "byte skip" is therefore an absolute offset, never additive to the
    // header end, and "line skip" is not honoured for raw data.
    if has_external_data {
        return Ok(hdr.byte_skip as u64);
    }

    let mut offset = base_offset;

    // Retained for legacy line-skip handling on inline data; Java does not
    // apply line skip for raw encoding, but keep it to support text/ascii-like
    // inline payloads where a leading line count is meaningful.
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

    Ok(offset)
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
        let plane_bytes = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|px| px.checked_mul(meta.size_c as usize))
            .and_then(|samples| samples.checked_mul(bps))
            .ok_or_else(|| BioFormatsError::InvalidData("NRRD: plane size overflow".into()))?;
        let plane_offset = (plane_index as usize)
            .checked_mul(plane_bytes)
            .ok_or_else(|| BioFormatsError::InvalidData("NRRD: plane offset overflow".into()))?;
        let axes = derive_axes(hdr);

        if hdr.data_files.len() == meta.image_count as usize {
            let data_path = &hdr.data_files[plane_index as usize];
            // Detached LIST files are external data sources.
            let raw = self.read_nrrd_payload(data_path, 0, hdr, plane_bytes, true)?;
            let buf = raw[..plane_bytes.min(raw.len())].to_vec();
            if buf.len() != plane_bytes {
                return Err(BioFormatsError::InvalidData(
                    "NRRD: detached LIST plane is shorter than expected".into(),
                ));
            }
            // Java's openBytes returns the raw on-disk bytes without swapping;
            // the byte order is reported via is_little_endian instead.
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

        let expected_bytes = total_sample_count(&hdr.sizes)?
            .checked_mul(bps)
            .ok_or_else(|| BioFormatsError::InvalidData("NRRD: byte count overflow".into()))?;
        // External data exists when the pixels live in a separate file
        // (.nhdr "data file" / detached LIST); inline NRRD data does not.
        let has_external_data = hdr.data_file.is_some() || !hdr.data_files.is_empty();
        let mut all = Vec::with_capacity(expected_bytes);
        for (data_path, base_offset) in &data_sources {
            let remaining = expected_bytes.saturating_sub(all.len());
            if remaining == 0 {
                break;
            }
            let mut chunk =
                self.read_nrrd_payload(data_path, *base_offset, hdr, remaining, has_external_data)?;
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
            let start = plane_offset;
            let end = start.checked_add(plane_bytes).ok_or_else(|| {
                BioFormatsError::InvalidData("NRRD: plane offset overflow".into())
            })?;
            if end > all.len() {
                return Err(BioFormatsError::InvalidData(
                    "NRRD: plane out of range".into(),
                ));
            }
            let buf = all[start..end].to_vec();
            // Raw on-disk byte order is preserved; see is_little_endian.
            return Ok(buf);
        }

        let mut strides = vec![1usize; hdr.sizes.len()];
        for axis in 1..hdr.sizes.len() {
            strides[axis] = strides[axis - 1]
                .checked_mul(hdr.sizes[axis - 1] as usize)
                .ok_or_else(|| BioFormatsError::InvalidData("NRRD: stride overflow".into()))?;
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
                        .try_fold(0usize, |acc, (coord, stride)| {
                            (*coord as usize)
                                .checked_mul(*stride)
                                .and_then(|v| acc.checked_add(v))
                        })
                        .ok_or_else(|| {
                            BioFormatsError::InvalidData("NRRD: sample offset overflow".into())
                        })?;
                    let src = sample_index.checked_mul(bps).ok_or_else(|| {
                        BioFormatsError::InvalidData("NRRD: byte offset overflow".into())
                    })?;
                    let src_end = src.checked_add(bps).ok_or_else(|| {
                        BioFormatsError::InvalidData("NRRD: byte offset overflow".into())
                    })?;
                    if src_end > all.len() {
                        return Err(BioFormatsError::InvalidData(
                            "NRRD: plane out of range".into(),
                        ));
                    }
                    let dst = (y as usize)
                        .checked_mul(axes.size_x as usize)
                        .and_then(|row| row.checked_add(x as usize))
                        .and_then(|px| px.checked_mul(axes.size_c as usize))
                        .and_then(|base| base.checked_add(c as usize))
                        .and_then(|sample| sample.checked_mul(bps))
                        .ok_or_else(|| {
                            BioFormatsError::InvalidData("NRRD: output offset overflow".into())
                        })?;
                    let dst_end = dst.checked_add(bps).ok_or_else(|| {
                        BioFormatsError::InvalidData("NRRD: output offset overflow".into())
                    })?;
                    if dst_end > buf.len() {
                        return Err(BioFormatsError::InvalidData(
                            "NRRD: output plane offset is out of range".into(),
                        ));
                    }
                    buf[dst..dst_end].copy_from_slice(&all[src..src_end]);
                }
            }
        }
        // Raw on-disk byte order is preserved; see is_little_endian.
        Ok(buf)
    }

    fn read_nrrd_payload(
        &self,
        data_path: &Path,
        base_offset: u64,
        hdr: &NrrdHeader,
        max_bytes: usize,
        has_external_data: bool,
    ) -> Result<Vec<u8>> {
        let mut f = File::open(data_path).map_err(BioFormatsError::Io)?;
        let data_start = data_start_offset(data_path, base_offset, hdr, has_external_data)?;

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
                let mut buf = Vec::with_capacity(max_bytes);
                let mut tokens = text.split_ascii_whitespace();
                for i in 0..samples {
                    let token = tokens.next().ok_or_else(|| {
                        BioFormatsError::InvalidData(
                            "NRRD: ASCII data is shorter than expected".into(),
                        )
                    })?;
                    let dst = i * bps;
                    match pixel_type {
                        PixelType::Uint8 | PixelType::Int8 => {
                            let v = token.parse::<u8>().map_err(|_| {
                                BioFormatsError::InvalidData("NRRD: malformed ASCII sample".into())
                            })?;
                            buf.push(v);
                        }
                        PixelType::Uint16 | PixelType::Int16 => {
                            let v = token.parse::<u16>().map_err(|_| {
                                BioFormatsError::InvalidData("NRRD: malformed ASCII sample".into())
                            })?;
                            buf.extend_from_slice(&v.to_le_bytes());
                        }
                        PixelType::Uint32 | PixelType::Int32 => {
                            let v = token.parse::<u32>().map_err(|_| {
                                BioFormatsError::InvalidData("NRRD: malformed ASCII sample".into())
                            })?;
                            buf.extend_from_slice(&v.to_le_bytes());
                        }
                        PixelType::Float32 => {
                            let v = token.parse::<f32>().map_err(|_| {
                                BioFormatsError::InvalidData("NRRD: malformed ASCII sample".into())
                            })?;
                            buf.extend_from_slice(&v.to_le_bytes());
                        }
                        PixelType::Float64 => {
                            let v = token.parse::<f64>().map_err(|_| {
                                BioFormatsError::InvalidData("NRRD: malformed ASCII sample".into())
                            })?;
                            buf.extend_from_slice(&v.to_le_bytes());
                        }
                        PixelType::Bit => {
                            let v = token.parse::<u8>().map_err(|_| {
                                BioFormatsError::InvalidData("NRRD: malformed ASCII sample".into())
                            })?;
                            buf.push(v);
                        }
                    }
                    debug_assert_eq!(buf.len(), dst + bps);
                }
                if buf.len() != max_bytes {
                    return Err(BioFormatsError::InvalidData(
                        "NRRD: ASCII data is shorter than expected".into(),
                    ));
                }
                buf
            }
            Encoding::Bzip2 => {
                // The Java NRRDReader throws UnsupportedCompressionException for
                // any encoding other than "raw"/"gzip"; bzip2 decoding would
                // need a `bzip2` decoder crate that is not a direct dependency.
                return Err(BioFormatsError::UnsupportedFormat(
                    "NRRD bzip2 encoding is not supported (requires a bzip2 decoder crate)".into(),
                ));
            }
            Encoding::Unsupported => {
                return Err(BioFormatsError::UnsupportedFormat(
                    "NRRD: unsupported encoding".into(),
                ));
            }
        };
        Ok(data)
    }
}

#[cfg(test)]
mod sidecar_path_tests {
    use super::*;

    fn tmp_path(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_nrrd_{name}_{nanos}.nrrd"))
    }

    #[test]
    fn ascii_payload_rejects_truncated_samples() {
        let path = tmp_path("truncated_ascii");
        std::fs::write(
            &path,
            b"NRRD0004\ntype: uint8\ndimension: 2\nsizes: 3 1\nencoding: ascii\n\n1 2",
        )
        .unwrap();

        let mut reader = NrrdReader::new();
        reader.set_id(&path).unwrap();
        let err = reader
            .open_bytes(0)
            .expect_err("truncated ASCII payload should be rejected");
        std::fs::remove_file(&path).ok();
        assert!(
            matches!(err, BioFormatsError::InvalidData(message) if message.contains("shorter"))
        );
    }

    #[test]
    fn ascii_payload_rejects_malformed_samples() {
        let path = tmp_path("malformed_ascii");
        std::fs::write(
            &path,
            b"NRRD0004\ntype: uint8\ndimension: 2\nsizes: 3 1\nencoding: ascii\n\n1 nope 3",
        )
        .unwrap();

        let mut reader = NrrdReader::new();
        reader.set_id(&path).unwrap();
        let err = reader
            .open_bytes(0)
            .expect_err("malformed ASCII payload should be rejected");
        std::fs::remove_file(&path).ok();
        assert!(
            matches!(err, BioFormatsError::InvalidData(message) if message.contains("malformed"))
        );
    }

    #[test]
    fn unsupported_type_rejects_like_java() {
        let path = tmp_path("unsupported_type");
        std::fs::write(
            &path,
            b"NRRD0004\ntype: block\ndimension: 2\nsizes: 1 1\nencoding: raw\n\n\0",
        )
        .unwrap();

        let mut reader = NrrdReader::new();
        let err = reader
            .set_id(&path)
            .expect_err("unsupported NRRD type should be rejected");
        std::fs::remove_file(&path).ok();
        assert!(
            matches!(err, BioFormatsError::Format(message) if message.contains("Unsupported data type"))
        );
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
            // NRRDReader.java fixes dimensionOrder = "XYCZT" (initFile line 277).
            dimension_order: DimensionOrder::XYCZT,
            // NRRDReader.java: m.rgb = getSizeC() > 1 (initFile line 368). Any
            // multi-component leading axis is an interleaved RGB-like channel.
            is_rgb: axes.size_c > 1,
            is_interleaved: true,
            is_indexed: false,
            // Honor the NRRD `endian:` header field exactly as Java does
            // (m.littleEndian = v.equals("little")). We return the raw bytes in
            // their on-disk order without swapping, so this flag is the byte
            // order callers actually receive. Default is little-endian when the
            // header omits `endian:` (single-byte types never declare it).
            is_little_endian: hdr.endian,
            resolution_count: 1,
            thumbnail: false,
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("NRRD", &full, meta, meta.size_c as usize, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        // Java's MetadataTools.populatePixels sets the OME Image name to the
        // dataset file name (e.g. "dt-helix.nrrd").
        if let Some(name) = self
            .path
            .as_ref()
            .and_then(|p| p.file_name())
            .and_then(|n| n.to_str())
        {
            if let Some(img) = ome.images.get_mut(0) {
                img.name = Some(name.to_string());
            }
        }
        Some(ome)
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

fn bytes_as_little_endian(meta: &ImageMetadata, data: &[u8]) -> Vec<u8> {
    let bps = meta.pixel_type.bytes_per_sample();
    if meta.is_little_endian || bps <= 1 {
        return data.to_vec();
    }
    let mut out = data.to_vec();
    for chunk in out.chunks_exact_mut(bps) {
        chunk.reverse();
    }
    out
}

impl FormatWriter for NrrdWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "nrrd" | "nhdr"))
            .unwrap_or(false)
    }
    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        if meta.size_c.max(1) > 1 && !meta.is_rgb {
            return Err(BioFormatsError::UnsupportedFormat(
                "NRRD writer cannot safely preserve non-RGB C planes with the current plane API"
                    .into(),
            ));
        }
        if !meta.is_rgb && meta.size_t.max(1) > 1 && (2..=16).contains(&meta.size_x) {
            return Err(BioFormatsError::UnsupportedFormat(
                "NRRD writer cannot safely preserve T when the leading X axis would be read as C"
                    .into(),
            ));
        }
        if meta.is_rgb && !matches!(meta.size_c, 3 | 4) {
            return Err(BioFormatsError::UnsupportedFormat(
                "NRRD writer supports RGB planes only when size_c is 3 or 4".into(),
            ));
        }
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
    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_next_plane(
            "NRRD",
            meta,
            self.planes.len(),
            plane_index,
            data.len(),
        )?;
        self.planes.push(bytes_as_little_endian(meta, data));
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let _path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_complete("NRRD", meta, self.planes.len())?;
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.take().ok_or(BioFormatsError::NotInitialized)?;
        let f = File::create(&path).map_err(BioFormatsError::Io)?;
        let mut w = std::io::BufWriter::new(f);

        let size_z = meta.size_z.max(1);
        let size_t = meta.size_t.max(1);
        let bps = meta.pixel_type.bytes_per_sample();

        writeln!(w, "NRRD0004").map_err(BioFormatsError::Io)?;
        writeln!(w, "type: {}", nrrd_type_str(meta.pixel_type)).map_err(BioFormatsError::Io)?;
        if meta.is_rgb {
            let mut sizes = vec![meta.size_c.max(1), meta.size_x, meta.size_y];
            if size_z > 1 || size_t > 1 {
                sizes.push(size_z);
            }
            if size_t > 1 {
                sizes.push(size_t);
            }
            writeln!(w, "dimension: {}", sizes.len()).map_err(BioFormatsError::Io)?;
            writeln!(
                w,
                "sizes: {}",
                sizes
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            )
            .map_err(BioFormatsError::Io)?;
        } else {
            let mut sizes = vec![meta.size_x, meta.size_y];
            if size_z > 1 || size_t > 1 {
                sizes.push(size_z);
            }
            if size_t > 1 {
                sizes.push(size_t);
            }
            writeln!(w, "dimension: {}", sizes.len()).map_err(BioFormatsError::Io)?;
            writeln!(
                w,
                "sizes: {}",
                sizes
                    .iter()
                    .map(u32::to_string)
                    .collect::<Vec<_>>()
                    .join(" ")
            )
            .map_err(BioFormatsError::Io)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_nrrd_{nanos}_{name}"))
    }

    #[test]
    fn detached_data_file_strips_leading_directory_like_java() {
        let dir = temp_path("java_data_file_dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        let path = dir.join("image.nhdr");
        std::fs::write(
            &path,
            b"NRRD0004\ntype: uint8\ndimension: 2\nsizes: 2 1\nencoding: raw\ndata file: sub/pixels.raw\n",
        )
        .unwrap();
        std::fs::write(dir.join("sub").join("pixels.raw"), [1, 2]).unwrap();
        std::fs::write(dir.join("pixels.raw"), [9, 8]).unwrap();

        let mut reader = NrrdReader::new();
        reader.set_id(&path).unwrap();
        let plane = reader.open_bytes(0).unwrap();

        let _ = std::fs::remove_dir_all(dir);
        assert_eq!(plane, vec![9, 8]);
    }

    #[test]
    fn detached_list_rejects_parent_escape() {
        let dir = temp_path("escape_list");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("image.nhdr");
        std::fs::write(
            &path,
            b"NRRD0004\ntype: uint8\ndimension: 3\nsizes: 1 1 2\nencoding: raw\ndata file: LIST\nplane0.raw\n../plane1.raw\n",
        )
        .unwrap();

        let err = parse_nrrd_header(&path).unwrap_err();

        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("must stay within"))
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn header_rejects_malformed_dimension_sizes_and_skips() {
        let cases: &[(&str, &[u8], &str)] = &[
            (
                "bad_dimension",
                b"NRRD0004\ntype: uint8\ndimension: nope\nsizes: 1 1\nencoding: raw\n\n",
                "invalid dimension",
            ),
            (
                "bad_size",
                b"NRRD0004\ntype: uint8\ndimension: 2\nsizes: 1 nope\nencoding: raw\n\n",
                "invalid size",
            ),
            (
                "bad_byte_skip",
                b"NRRD0004\ntype: uint8\ndimension: 2\nsizes: 1 1\nencoding: raw\nbyte skip: nope\n\n",
                "invalid byte skip",
            ),
            (
                "bad_line_skip",
                b"NRRD0004\ntype: uint8\ndimension: 2\nsizes: 1 1\nencoding: raw\nline skip: nope\n\n",
                "invalid line skip",
            ),
        ];

        for (name, bytes, expected) in cases {
            let path = temp_path(name);
            std::fs::write(&path, bytes).unwrap();
            let err = parse_nrrd_header(&path).unwrap_err();
            assert!(
                err.to_string().contains(expected),
                "{name}: unexpected error: {err}"
            );
            let _ = std::fs::remove_file(path);
        }
    }
}
