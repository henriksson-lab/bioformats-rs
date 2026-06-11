//! MetaImage MHA/MHD reader and writer (ITK/VTK format).
//!
//! `.mha` = inline (header + data in same file)
//! `.mhd` = detached header; data in a separate `.raw` (or `.zraw`) file

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::path::confined_join;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use crate::common::writer::FormatWriter;

fn meta_pixel_type(s: &str) -> PixelType {
    match s {
        "MET_CHAR" => PixelType::Int8,
        "MET_UCHAR" => PixelType::Uint8,
        "MET_SHORT" => PixelType::Int16,
        "MET_USHORT" => PixelType::Uint16,
        "MET_INT" => PixelType::Int32,
        "MET_UINT" => PixelType::Uint32,
        "MET_FLOAT" => PixelType::Float32,
        "MET_DOUBLE" => PixelType::Float64,
        _ => PixelType::Uint8,
    }
}

fn meta_type_str(pt: PixelType) -> &'static str {
    match pt {
        PixelType::Int8 => "MET_CHAR",
        PixelType::Uint8 | PixelType::Bit => "MET_UCHAR",
        PixelType::Int16 => "MET_SHORT",
        PixelType::Uint16 => "MET_USHORT",
        PixelType::Int32 => "MET_INT",
        PixelType::Uint32 => "MET_UINT",
        PixelType::Float32 => "MET_FLOAT",
        PixelType::Float64 => "MET_DOUBLE",
    }
}

fn parse_meta_scalar<T>(value: &str, field: &str) -> Result<T>
where
    T: std::str::FromStr,
{
    value.parse().map_err(|_| {
        BioFormatsError::Format(format!(
            "MetaImage: invalid numeric value for {field}: {value}"
        ))
    })
}

fn parse_meta_sizes(value: &str) -> Result<Vec<u32>> {
    value
        .split_ascii_whitespace()
        .map(|s| parse_meta_scalar(s, "DimSize"))
        .collect()
}

/// How the pixel data is stored across files (MetaIO `ElementDataFile`).
#[derive(Clone)]
enum DataLayout {
    /// `LOCAL`: data follows the header in the same file.
    Local,
    /// A single detached file (one path, all planes packed sequentially).
    Single(String),
    /// `LIST` or a printf pattern: one file per slice.
    PerSlice(Vec<String>),
}

struct MhdHeader {
    ndims: usize,
    sizes: Vec<u32>,
    pixel_type: PixelType,
    little_endian: bool,
    compressed: bool,
    layout: Option<DataLayout>,
    data_offset: u64,
    /// MetaIO `HeaderSize`: bytes to skip in the detached data file before the
    /// pixels. `None` if unspecified, `Some(-1)` for auto (skip = fileLen -
    /// dataLen), `Some(n>=0)` for an explicit byte count.
    header_size: Option<i64>,
    extra: HashMap<String, String>,
}

fn parse_mhd(path: &Path) -> Result<MhdHeader> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let mut reader = BufReader::new(f);

    let mut ndims = 3usize;
    let mut sizes: Vec<u32> = Vec::new();
    let mut pixel_type = PixelType::Uint8;
    let mut little_endian = true;
    let mut compressed = false;
    let mut layout: Option<DataLayout> = None;
    let mut data_offset = 0u64;
    let mut header_size: Option<i64> = None;
    let mut extra: HashMap<String, String> = HashMap::new();

    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).map_err(BioFormatsError::Io)?;
        if n == 0 {
            break;
        }

        let trimmed = line.trim_end_matches(|c| c == '\r' || c == '\n');
        if trimmed.is_empty() {
            continue;
        }

        if let Some(eq) = trimmed.find('=') {
            let key = trimmed[..eq].trim().to_ascii_uppercase();
            let val = trimmed[eq + 1..].trim();

            match key.as_str() {
                "NDIMS" | "NIMS" | "OBJECTTYPE" => {
                    if key == "NDIMS" {
                        ndims = parse_meta_scalar(val, "NDims")?;
                    }
                }
                "DIMSIZE" | "DIM_SIZE" => {
                    sizes = parse_meta_sizes(val)?;
                }
                "ELEMENTTYPE" => pixel_type = meta_pixel_type(val),
                "ELEMENTBYTEORDERMSB" => little_endian = !val.eq_ignore_ascii_case("true"),
                "BINARYDATA" if val.eq_ignore_ascii_case("false") => {}
                "BINARYDATABYTEORDERMSB" => little_endian = !val.eq_ignore_ascii_case("true"),
                "COMPRESSEDDATA" => compressed = val.eq_ignore_ascii_case("true"),
                "HEADERSIZE" => {
                    header_size = Some(parse_meta_scalar::<i64>(val, "HeaderSize")?);
                }
                "ELEMENTDATAFILE" => {
                    // MetaIO ElementDataFile may be:
                    //   LOCAL            data follows the header in this file
                    //   <path>           a single detached data file
                    //   LIST [n]         one filename per following line
                    //   <printf-pattern> <start> <stop> <step>  generated names
                    let upper = val.to_ascii_uppercase();
                    if upper == "LOCAL" {
                        data_offset = reader.stream_position().map_err(BioFormatsError::Io)?;
                        layout = Some(DataLayout::Local);
                    } else if upper == "LIST" || upper.starts_with("LIST ") {
                        // Remaining non-empty lines each name one slice file.
                        let mut files = Vec::new();
                        loop {
                            let mut l = String::new();
                            let m = reader.read_line(&mut l).map_err(BioFormatsError::Io)?;
                            if m == 0 {
                                break;
                            }
                            let t = l.trim();
                            if !t.is_empty() {
                                files.push(t.to_string());
                            }
                        }
                        layout = Some(DataLayout::PerSlice(files));
                    } else if val.contains('%') {
                        // printf pattern, optionally followed by start stop step.
                        let parts: Vec<&str> = val.split_ascii_whitespace().collect();
                        let pattern = parts[0];
                        let (start, stop, step) = match parts.len() {
                            n if n >= 4 => (
                                parse_meta_scalar::<i64>(parts[1], "DataFile start")?,
                                parse_meta_scalar::<i64>(parts[2], "DataFile stop")?,
                                parse_meta_scalar::<i64>(parts[3], "DataFile step")?,
                            ),
                            _ => (1, 1, 1),
                        };
                        let files = expand_printf_pattern(pattern, start, stop, step)?;
                        layout = Some(DataLayout::PerSlice(files));
                    } else {
                        layout = Some(DataLayout::Single(val.to_string()));
                    }
                }
                _ => {
                    extra.insert(key, val.to_string());
                }
            }
        }
    }

    if ndims == 0 || ndims > 3 {
        return Err(BioFormatsError::Format(format!(
            "MetaImage: unsupported NDims value {ndims}"
        )));
    }
    if sizes.len() != ndims {
        return Err(BioFormatsError::Format(format!(
            "MetaImage: DimSize has {} value(s), expected {ndims}",
            sizes.len()
        )));
    }
    if sizes.iter().any(|&size| size == 0) {
        return Err(BioFormatsError::Format(
            "MetaImage: DimSize values must be positive".into(),
        ));
    }

    Ok(MhdHeader {
        ndims,
        sizes,
        pixel_type,
        little_endian,
        compressed,
        layout,
        data_offset,
        header_size,
        extra,
    })
}

/// Expand a MetaIO printf-style `ElementDataFile` pattern (e.g. `slice%03d.raw`)
/// into one filename per slice for the inclusive range `start..=stop` stepping
/// by `step`. Only the integer `%[0][width]d` conversion is supported.
fn expand_printf_pattern(pattern: &str, start: i64, stop: i64, step: i64) -> Result<Vec<String>> {
    if step == 0 {
        return Err(BioFormatsError::Format(
            "MetaImage: ElementDataFile printf step must be non-zero".into(),
        ));
    }
    // Locate the single %d-style conversion.
    let pct = pattern.find('%').ok_or_else(|| {
        BioFormatsError::Format("MetaImage: ElementDataFile pattern has no '%' conversion".into())
    })?;
    let d = pattern[pct..].find('d').ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "MetaImage: unsupported ElementDataFile conversion in {pattern:?} (only %d)"
        ))
    })?;
    let spec = &pattern[pct + 1..pct + d]; // e.g. "03" or ""
    let zero_pad = spec.starts_with('0');
    let width: usize = spec.trim_start_matches('0').parse().unwrap_or(0);
    let prefix = &pattern[..pct];
    let suffix = &pattern[pct + d + 1..];

    let mut files = Vec::new();
    let mut i = start;
    while (step > 0 && i <= stop) || (step < 0 && i >= stop) {
        let num = if zero_pad {
            format!("{i:0width$}")
        } else {
            format!("{i:width$}")
        };
        files.push(format!("{prefix}{num}{suffix}"));
        i += step;
    }
    Ok(files)
}

// ---- reader -----------------------------------------------------------------

pub struct MetaImageReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    header: Option<MhdHeader>,
}

impl MetaImageReader {
    pub fn new() -> Self {
        MetaImageReader {
            path: None,
            meta: None,
            header: None,
        }
    }

    fn read_data(&self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let hdr = self
            .header
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mhd_path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|v| v.checked_mul(meta.size_c as usize))
            .and_then(|v| v.checked_mul(bps))
            .ok_or_else(|| BioFormatsError::InvalidData("MetaImage: plane size overflow".into()))?;
        let parent = mhd_path.parent().unwrap_or(Path::new("."));
        let resolve = |s: &str| -> Result<PathBuf> {
            confined_join(parent, s).ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "MetaImage: ElementDataFile escapes image directory: {s}"
                ))
            })
        };

        // Resolve the data file for this plane, how many planes that file holds,
        // and the index of the requested plane within that file. For a single
        // packed file all planes share one file; for per-slice layouts each
        // file holds exactly one plane.
        let (data_path, base_offset, planes_in_file, plane_in_file) = match &hdr.layout {
            Some(DataLayout::Local) => (
                mhd_path.clone(),
                hdr.data_offset,
                meta.image_count,
                plane_index,
            ),
            Some(DataLayout::Single(s)) => (resolve(s)?, 0, meta.image_count, plane_index),
            None => {
                // Default: sibling .raw file, all planes packed.
                (
                    mhd_path.with_extension("raw"),
                    0,
                    meta.image_count,
                    plane_index,
                )
            }
            Some(DataLayout::PerSlice(files)) => {
                let f = files.get(plane_index as usize).ok_or_else(|| {
                    BioFormatsError::InvalidData(format!(
                        "MetaImage: ElementDataFile list has no entry for plane {plane_index}"
                    ))
                })?;
                (resolve(f)?, 0, 1u32, 0u32)
            }
        };

        // HeaderSize: bytes to skip in the data file before the pixels. -1 means
        // auto (skip = fileLen - dataLen for the bytes this file should hold).
        let header_skip: u64 = match hdr.header_size {
            None | Some(0) => 0,
            Some(n) if n > 0 => n as u64,
            Some(_) => {
                // Auto: file length minus the data this file is meant to contain.
                let file_len = std::fs::metadata(&data_path)
                    .map_err(BioFormatsError::Io)?
                    .len();
                let data_len = (planes_in_file as u64)
                    .checked_mul(plane_bytes as u64)
                    .ok_or_else(|| {
                        BioFormatsError::InvalidData("MetaImage: data size overflow".into())
                    })?;
                file_len.saturating_sub(data_len)
            }
        };

        let plane_offset = (plane_in_file as u64)
            .checked_mul(plane_bytes as u64)
            .ok_or_else(|| {
                BioFormatsError::InvalidData("MetaImage: plane offset overflow".into())
            })?;

        let mut f = File::open(&data_path).map_err(BioFormatsError::Io)?;

        let buf = if hdr.compressed {
            f.seek(SeekFrom::Start(base_offset + header_skip))
                .map_err(BioFormatsError::Io)?;
            let mut dec = flate2::read::ZlibDecoder::new(f);
            let mut all = Vec::new();
            dec.read_to_end(&mut all).map_err(BioFormatsError::Io)?;
            let start = usize::try_from(plane_offset).map_err(|_| {
                BioFormatsError::InvalidData("MetaImage: plane offset overflow".into())
            })?;
            let end = start.checked_add(plane_bytes).ok_or_else(|| {
                BioFormatsError::InvalidData("MetaImage: plane range overflow".into())
            })?;
            if end > all.len() {
                return Err(BioFormatsError::InvalidData(
                    "MetaImage: plane out of range".into(),
                ));
            }
            all[start..end].to_vec()
        } else {
            let offset = base_offset
                .checked_add(header_skip)
                .and_then(|o| o.checked_add(plane_offset))
                .ok_or_else(|| {
                    BioFormatsError::InvalidData("MetaImage: plane offset overflow".into())
                })?;
            f.seek(SeekFrom::Start(offset))
                .map_err(BioFormatsError::Io)?;
            let mut buf = vec![0u8; plane_bytes];
            f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
            buf
        };

        // Byte-swap if big-endian
        let mut buf = buf;
        if !hdr.little_endian && bps > 1 {
            for chunk in buf.chunks_exact_mut(bps) {
                chunk.reverse();
            }
        }
        Ok(buf)
    }
}

impl Default for MetaImageReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MetaImageReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "mha" | "mhd"))
            .unwrap_or(false)
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // MetaImage header always starts with "ObjectType"
        let s = std::str::from_utf8(&header[..header.len().min(32)]).unwrap_or("");
        s.trim_start().starts_with("ObjectType") || s.trim_start().starts_with("NDims")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let hdr = parse_mhd(path)?;

        let (size_x, size_y, size_z) = match hdr.sizes.as_slice() {
            [x] => (*x, 1, 1),
            [x, y] => (*x, *y, 1),
            [x, y, z, ..] => (*x, *y, *z),
            [] => (1, 1, 1),
        };
        let bps = (hdr.pixel_type.bytes_per_sample() * 8) as u8;
        let mut series_metadata: HashMap<String, MetadataValue> = hdr
            .extra
            .iter()
            .map(|(k, v)| (k.clone(), MetadataValue::String(v.clone())))
            .collect();
        series_metadata.insert("ndims".into(), MetadataValue::Int(hdr.ndims as i64));

        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c: 1,
            size_t: 1,
            pixel_type: hdr.pixel_type,
            bits_per_pixel: bps,
            image_count: size_z,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: hdr.little_endian,
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
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.read_data(plane_index)
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
        crop_full_plane("MetaImage", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---- writer (MHA = inline) --------------------------------------------------

pub struct MetaImageWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
}

impl MetaImageWriter {
    pub fn new() -> Self {
        MetaImageWriter {
            path: None,
            meta: None,
            planes: Vec::new(),
        }
    }
}

impl Default for MetaImageWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for MetaImageWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| matches!(e.to_ascii_lowercase().as_str(), "mha" | "mhd"))
            .unwrap_or(false)
    }
    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        if meta.size_c.max(1) > 1 || meta.size_t.max(1) > 1 {
            return Err(BioFormatsError::UnsupportedFormat(
                "MetaImage writer does not preserve C/T axes; write Z stacks only".into(),
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
            "MetaImage",
            meta,
            self.planes.len(),
            plane_index,
            data.len(),
        )?;
        self.planes.push(data.to_vec());
        Ok(())
    }
    fn close(&mut self) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let _path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_complete("MetaImage", meta, self.planes.len())?;
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.take().ok_or(BioFormatsError::NotInitialized)?;

        let ext = path.extension().and_then(|e| e.to_str()).unwrap_or("mha");
        let is_mhd = ext.eq_ignore_ascii_case("mhd");

        let nz = self.planes.len();
        let f = File::create(&path).map_err(BioFormatsError::Io)?;
        let mut w = BufWriter::new(f);

        writeln!(w, "ObjectType = Image").map_err(BioFormatsError::Io)?;
        writeln!(w, "NDims = {}", if nz > 1 { 3 } else { 2 }).map_err(BioFormatsError::Io)?;
        if nz > 1 {
            writeln!(w, "DimSize = {} {} {}", meta.size_x, meta.size_y, nz)
                .map_err(BioFormatsError::Io)?;
        } else {
            writeln!(w, "DimSize = {} {}", meta.size_x, meta.size_y)
                .map_err(BioFormatsError::Io)?;
        }
        writeln!(w, "ElementType = {}", meta_type_str(meta.pixel_type))
            .map_err(BioFormatsError::Io)?;
        writeln!(w, "BinaryData = True").map_err(BioFormatsError::Io)?;
        writeln!(w, "BinaryDataByteOrderMSB = False").map_err(BioFormatsError::Io)?;
        writeln!(w, "CompressedData = False").map_err(BioFormatsError::Io)?;

        if is_mhd {
            let stem = path.file_stem().unwrap_or_default().to_string_lossy();
            writeln!(w, "ElementDataFile = {}.raw", stem).map_err(BioFormatsError::Io)?;
            w.flush().map_err(BioFormatsError::Io)?;
            drop(w);
            // Write raw data file
            let raw_path = path.with_extension("raw");
            let rf = File::create(&raw_path).map_err(BioFormatsError::Io)?;
            let mut rw = BufWriter::new(rf);
            for plane in &self.planes {
                rw.write_all(plane).map_err(BioFormatsError::Io)?;
            }
            rw.flush().map_err(BioFormatsError::Io)?;
        } else {
            writeln!(w, "ElementDataFile = LOCAL").map_err(BioFormatsError::Io)?;
            for plane in &self.planes {
                w.write_all(plane).map_err(BioFormatsError::Io)?;
            }
            w.flush().map_err(BioFormatsError::Io)?;
        }
        self.planes.clear();
        Ok(())
    }
    fn can_do_stacks(&self) -> bool {
        true
    }
}
