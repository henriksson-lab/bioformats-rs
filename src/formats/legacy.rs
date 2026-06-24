//! Legacy and obscure format readers.
//!
//! - KodakReader: Kodak thermal camera (.bip)
//! - PictReader: Apple PICT format (.pict, .pct), bounded bitmap/pixmap support

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::codec::decompress_packbits;
use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, LookupTable, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::{crop_full_plane, validate_region};

fn region_crop(full: &[u8], meta: &ImageMetadata, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let bps = meta.pixel_type.bytes_per_sample();
    let row = meta.size_x as usize * bps;
    let out_row = w as usize * bps;
    let mut out = Vec::with_capacity(h as usize * out_row);
    for r in 0..h as usize {
        let src = &full[(y as usize + r) * row..];
        out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
    }
    out
}

// ── KodakReader ────────────────────────────────────────────────────────────

/// Kodak Molecular Imaging `.bip` reader, ported from the Java `KodakReader`.
///
/// The format is big-endian with 32-bit float pixels. Dimensions and the pixel
/// offset are located by scanning for the `GBiH` (dimensions) and `BSfD`
/// (pixels) tag markers; `DTag` is the file magic.
pub struct KodakReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_offset: u64,
}

impl KodakReader {
    pub fn new() -> Self {
        KodakReader {
            path: None,
            meta: None,
            pixel_offset: 0,
        }
    }
}

impl Default for KodakReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Find the byte offset of `marker` within `data`, starting the search at
/// `from`. Mirrors the Java `findString` helper (which leaves the pointer at
/// the start of the marker).
fn kodak_find(data: &[u8], marker: &[u8], from: usize) -> Option<usize> {
    if marker.is_empty() || from >= data.len() {
        return None;
    }
    data[from..]
        .windows(marker.len())
        .position(|w| w == marker)
        .map(|p| from + p)
}

fn kodak_read_cstring(data: &[u8], offset: usize) -> Option<String> {
    if offset >= data.len() {
        return None;
    }
    let end = data[offset..]
        .iter()
        .position(|&b| b == 0)
        .map(|p| offset + p)
        .unwrap_or(data.len());
    Some(String::from_utf8_lossy(&data[offset..end]).into_owned())
}

fn kodak_parse_capture_date(value: &str) -> Option<String> {
    let (time, date) = value.split_once(" on ")?;
    let mut parts = date.split('/');
    let month: u32 = parts.next()?.parse().ok()?;
    let day: u32 = parts.next()?.parse().ok()?;
    let year: i32 = parts.next()?.parse().ok()?;
    if parts.next().is_some() {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}T{time}"))
}

fn kodak_first_number(value: &str) -> Option<f64> {
    value.split_whitespace().next()?.parse::<f64>().ok()
}

fn kodak_add_extra_metadata(data: &[u8], meta: &mut ImageMetadata) {
    if let Some(pos) = kodak_find(data, b"Image Capture Source", 0) {
        if let Some(metadata) = kodak_read_cstring(data, pos) {
            for line in metadata.split('\n') {
                let Some(index) = line.find(':') else {
                    continue;
                };
                if line.starts_with('#') || line.starts_with('-') {
                    continue;
                }
                let key = line[..index].trim();
                let value = line[index + 1..].trim();
                if key.is_empty() {
                    continue;
                }
                meta.series_metadata
                    .insert(key.to_string(), MetadataValue::String(value.to_string()));
            }
        }
    }
}

fn kodak_add_file_info_metadata(data: &[u8], meta: &mut ImageMetadata) {
    const FILEINFO_STRING: &[u8] = b"DLFi";
    let Some(pos) = kodak_find(data, FILEINFO_STRING, 0) else {
        return;
    };
    if data.len().saturating_sub(pos) < FILEINFO_STRING.len() + 20 {
        return;
    }
    let length_offset = pos + FILEINFO_STRING.len() + 16;
    let tag_total = i32::from_be_bytes([
        data[length_offset],
        data[length_offset + 1],
        data[length_offset + 2],
        data[length_offset + 3],
    ]);
    let data_length = tag_total - FILEINFO_STRING.len() as i32 - 20;
    if data_length <= 0 {
        return;
    }
    let data_offset = length_offset + 4;
    let data_length = data_length as usize;
    if data.len().saturating_sub(data_offset) < data_length {
        return;
    }
    let info = String::from_utf8_lossy(&data[data_offset..data_offset + data_length]);
    let collapsed = info
        .split(|ch| ch == '\r' || ch == '\n')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join(" | ");
    let trimmed = collapsed.trim();
    if !trimmed.is_empty() {
        meta.series_metadata.insert(
            "FileInfo".into(),
            MetadataValue::String(trimmed.to_string()),
        );
    }
}

fn parse_kodak_bip(path: &Path) -> Result<(ImageMetadata, u64)> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;

    const DIMENSIONS_STRING: &[u8] = b"GBiH";
    const PIXELS_STRING: &[u8] = b"BSfD";

    // findString(DIMENSIONS_STRING); skipBytes(len + 20); readInt sizeX/sizeY.
    let dim_pos = kodak_find(&data, DIMENSIONS_STRING, 0).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Kodak .bip: dimensions marker not found".into())
    })?;
    let dim_data = dim_pos + DIMENSIONS_STRING.len() + 20;
    if dim_data + 8 > data.len() {
        return Err(BioFormatsError::Format(
            "Kodak .bip: truncated dimensions block".into(),
        ));
    }
    let width = u32::from_be_bytes([
        data[dim_data],
        data[dim_data + 1],
        data[dim_data + 2],
        data[dim_data + 3],
    ]);
    let height = u32::from_be_bytes([
        data[dim_data + 4],
        data[dim_data + 5],
        data[dim_data + 6],
        data[dim_data + 7],
    ]);
    if width == 0 || height == 0 {
        return Err(BioFormatsError::Format(
            "Kodak .bip: missing image dimensions".into(),
        ));
    }

    // findString(PIXELS_STRING); pixelOffset = filePointer + len + 20.
    let pix_pos = kodak_find(&data, PIXELS_STRING, 0).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Kodak .bip: pixel marker not found".into())
    })?;
    let pixel_offset = (pix_pos + PIXELS_STRING.len() + 20) as u64;

    let mut meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type: PixelType::Float32,
        bits_per_pixel: 32,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: false, // Kodak .bip is big-endian
        resolution_count: 1,
        thumbnail: false,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };
    kodak_add_extra_metadata(&data, &mut meta);
    kodak_add_file_info_metadata(&data, &mut meta);
    Ok((meta, pixel_offset))
}

impl FormatReader for KodakReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("bip"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // MAGIC_STRING "DTag" appears within the file header.
        header.windows(4).any(|w| w == b"DTag")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, pixel_offset) = parse_kodak_bip(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.pixel_offset = pixel_offset;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_offset = 0;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.pixel_offset))
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Kodak BIP", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        let Some(img) = ome.images.first_mut() else {
            return Some(ome);
        };

        if let Some(MetadataValue::String(value)) = meta.series_metadata.get("Capture Time/Date") {
            img.acquisition_date = kodak_parse_capture_date(value);
        }
        if let Some(MetadataValue::String(value)) = meta.series_metadata.get("Exposure Time") {
            if let Some(exposure_time) = kodak_first_number(value) {
                if img.planes.is_empty() {
                    img.planes
                        .push(crate::common::ome_metadata::OmePlane::default());
                }
                img.planes[0].exposure_time = Some(exposure_time);
            }
        }
        if let Some(MetadataValue::String(value)) =
            meta.series_metadata.get("Horizontal Resolution")
        {
            img.physical_size_x = kodak_first_number(value)
                .filter(|v| v.is_finite() && *v > 0.0)
                .map(|ppi| 25400.0 / ppi);
        }
        if let Some(MetadataValue::String(value)) = meta.series_metadata.get("Vertical Resolution")
        {
            img.physical_size_y = kodak_first_number(value)
                .filter(|v| v.is_finite() && *v > 0.0)
                .map(|ppi| 25400.0 / ppi);
        }
        if let Some(MetadataValue::String(value)) = meta.series_metadata.get("CCD Temperature") {
            let hex = value.strip_prefix("0x").filter(|digits| {
                digits
                    .chars()
                    .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_lowercase())
            });
            if hex.is_none() {
                img.imaging_environment_temperature = kodak_first_number(value);
            }
        }
        if let Some(MetadataValue::String(value)) = meta.series_metadata.get("Image Capture Source")
        {
            ome.instruments
                .push(crate::common::ome_metadata::OmeInstrument {
                    id: Some("Instrument:0".to_string()),
                    microscope_model: Some(value.clone()),
                    ..Default::default()
                });
            img.instrument_ref = Some(0);
        }
        Some(ome)
    }
}

// ── FujiReader ────────────────────────────────────────────────────────────────

/// Fuji LAS 3000 gel reader, ported from the Java `FujiReader`.
///
/// A dataset is a companion pair: a `.inf` ASCII text header describing the
/// image, and a `.img` file holding a single raw (uncompressed) plane. The
/// header is split on `\r?\n` lines; the fields Java reads by index are:
///   line 1  — image name
///   line 3  — physical width (µm)
///   line 4  — physical height (µm)
///   line 5  — bit depth
///   line 6  — sizeX
///   line 7  — sizeY
///   line 10 — acquisition timestamp (`ddd MMM dd HH:mm:ss yyyy`)
///   line 13 — instrument/microscope model
pub struct FujiReader {
    /// Absolute path to the `.inf` header (the `currentId` Java settles on).
    inf_file: Option<PathBuf>,
    /// Absolute path to the companion `.img` pixels file.
    pixels_file: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    image_name: Option<String>,
    acquisition_date: Option<String>,
    instrument: Option<String>,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
}

impl FujiReader {
    pub fn new() -> Self {
        FujiReader {
            inf_file: None,
            pixels_file: None,
            meta: None,
            image_name: None,
            acquisition_date: None,
            instrument: None,
            physical_size_x: None,
            physical_size_y: None,
        }
    }
}

impl Default for FujiReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Side-channel header values that feed OME metadata (kept off `ImageMetadata`).
struct FujiHeader {
    pixels_file: PathBuf,
    image_name: Option<String>,
    acquisition_date: Option<String>,
    instrument: Option<String>,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
}

/// Replace the extension of `path` (in a confined manner) with `ext`, returning
/// the sibling path. Mirrors Java's `baseName + "." + ext` companion lookups.
fn fuji_sibling(path: &Path, ext: &str) -> Option<PathBuf> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let stem = path.file_stem()?.to_str()?;
    crate::common::path::confined_join(parent, &format!("{stem}.{ext}"))
}

/// Map a sample byte count to an unsigned, non-floating-point pixel type,
/// mirroring Java's `FormatTools.pixelTypeFromBytes(bytes, false, false)`.
fn fuji_pixel_type_from_bytes(bytes: usize) -> Result<PixelType> {
    match bytes {
        1 => Ok(PixelType::Uint8),
        2 => Ok(PixelType::Uint16),
        4 => Ok(PixelType::Uint32),
        8 => Ok(PixelType::Float64),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "Fuji LAS: unsupported pixel size of {bytes} byte(s)"
        ))),
    }
}

/// Split as Java `String.split("\r{0,1}\n")`: normalize CRLF lines and discard
/// trailing empty fields produced by terminal newlines.
fn fuji_split_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text
        .split('\n')
        .map(|l| l.strip_suffix('\r').unwrap_or(l))
        .collect();
    while lines.len() > 1 && lines.last().is_some_and(|line| line.is_empty()) {
        lines.pop();
    }
    lines
}

/// Port of Java `Double.parseDouble` plus `FormatTools.getPhysicalSizeX/Y`:
/// Fuji stores values in micrometres, and OME metadata is populated only for
/// finite positive values.
fn fuji_physical_size(value: &str) -> Result<Option<f64>> {
    let parsed = value
        .trim()
        .parse::<f64>()
        .map_err(|_| BioFormatsError::Format("Fuji LAS: invalid physical size".into()))?;
    Ok(parsed.is_finite().then_some(parsed).filter(|v| *v > 0.0))
}

/// Convert a Fuji `ddd MMM dd HH:mm:ss yyyy` timestamp (e.g.
/// "Wed Jul 25 14:00:00 2007") to ISO-8601, mirroring Java's
/// `DateTools.formatDate(timestamp, DATE_FORMAT)`. Returns `None` if it cannot
/// be parsed (Java's formatDate likewise yields null).
fn fuji_format_date(line: &str) -> Option<String> {
    let tokens: Vec<&str> = line.split_whitespace().collect();
    // Expect: <weekday> <month> <day> <HH:mm:ss> <year>
    if tokens.len() != 5 {
        return None;
    }
    let month = match tokens[1] {
        "Jan" => 1,
        "Feb" => 2,
        "Mar" => 3,
        "Apr" => 4,
        "May" => 5,
        "Jun" => 6,
        "Jul" => 7,
        "Aug" => 8,
        "Sep" => 9,
        "Oct" => 10,
        "Nov" => 11,
        "Dec" => 12,
        _ => return None,
    };
    let day: u32 = tokens[2].parse().ok()?;
    let year: i32 = tokens[4].parse().ok()?;
    let time = tokens[3];
    let time_parts: Vec<&str> = time.split(':').collect();
    if time_parts.len() != 3 || time_parts.iter().any(|p| p.parse::<u32>().is_err()) {
        return None;
    }
    Some(format!("{year:04}-{month:02}-{day:02}T{time}"))
}

/// Port of Java `FujiReader.initFile`: parse the `.inf` header, locate the
/// `.img` pixels file, and build the core metadata plus OME-side fields.
fn parse_fuji(inf_file: &Path) -> Result<(ImageMetadata, FujiHeader)> {
    let pixels_file = fuji_sibling(inf_file, "img").ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Fuji LAS: could not locate companion .img file".into())
    })?;
    if !pixels_file.is_file() {
        return Err(BioFormatsError::UnsupportedFormat(
            "Fuji LAS: could not locate companion .img file".into(),
        ));
    }

    let text = std::fs::read_to_string(inf_file).map_err(BioFormatsError::Io)?;
    let lines = fuji_split_lines(&text);

    // Java indexes lines[5..13] directly; guard the highest index it touches.
    if lines.len() <= 13 {
        return Err(BioFormatsError::Format(
            "Fuji LAS: .inf header has too few lines".into(),
        ));
    }

    let bits: u32 = lines[5].trim().parse().map_err(|_| {
        BioFormatsError::Format("Fuji LAS: invalid bit depth in .inf header".into())
    })?;
    let pixel_type = fuji_pixel_type_from_bytes((bits / 8) as usize)?;

    let size_x: u32 = lines[6]
        .trim()
        .parse()
        .map_err(|_| BioFormatsError::Format("Fuji LAS: invalid sizeX in .inf header".into()))?;
    let size_y: u32 = lines[7]
        .trim()
        .parse()
        .map_err(|_| BioFormatsError::Format("Fuji LAS: invalid sizeY in .inf header".into()))?;

    let mut series_metadata = HashMap::new();
    // addGlobalMetaList("Line", line): bare key first, then "Line #2", "Line #3"...
    for (i, line) in lines.iter().enumerate() {
        let key = if i == 0 {
            "Line".to_string()
        } else {
            format!("Line #{}", i + 1)
        };
        series_metadata.insert(key, MetadataValue::String((*line).to_string()));
    }

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type,
        bits_per_pixel: bits as u8,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        thumbnail: false,
        series_metadata,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    let image_name = Some(lines[1].to_string());
    let acquisition_date = fuji_format_date(lines[10].trim());
    let physical_size_x = fuji_physical_size(lines[3])?;
    let physical_size_y = fuji_physical_size(lines[4])?;
    let instrument = Some(lines[13].to_string());

    let header = FujiHeader {
        pixels_file,
        image_name,
        acquisition_date,
        instrument,
        physical_size_x,
        physical_size_y,
    };
    Ok((meta, header))
}

impl FormatReader for FujiReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // Java accepts both .img and .inf, but only when the companion exists
        // (isThisType(name, open=true)). .img is shared with other readers, so
        // the companion-existence check is what disambiguates.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            Some("inf") => fuji_sibling(path, "img").is_some_and(|p| p.exists()),
            Some("img") => fuji_sibling(path, "inf").is_some_and(|p| p.exists()),
            _ => false,
        }
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // Mirrors Java isThisType(RandomAccessInputStream) returning false.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        // Java initFile redirects to the .inf companion when given the .img.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        let inf_file = if ext.as_deref() == Some("inf") {
            path.to_path_buf()
        } else {
            fuji_sibling(path, "inf").ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "Fuji LAS: could not locate companion .inf header".into(),
                )
            })?
        };

        let (meta, header) = parse_fuji(&inf_file)?;
        self.inf_file = Some(inf_file);
        self.pixels_file = Some(header.pixels_file);
        self.image_name = header.image_name;
        self.acquisition_date = header.acquisition_date;
        self.instrument = header.instrument;
        self.physical_size_x = header.physical_size_x;
        self.physical_size_y = header.physical_size_y;
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inf_file = None;
        self.pixels_file = None;
        self.meta = None;
        self.image_name = None;
        self.acquisition_date = None;
        self.instrument = None;
        self.physical_size_x = None;
        self.physical_size_y = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_some() && s == 0 {
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let path = self
            .pixels_file
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("Fuji LAS", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        if let Some(img) = ome.images.first_mut() {
            img.name = self.image_name.clone();
            img.acquisition_date = self.acquisition_date.clone();
            img.physical_size_x = self.physical_size_x;
            img.physical_size_y = self.physical_size_y;
            // MetadataLevel != MINIMUM: record the microscope model.
            if let Some(instrument) = &self.instrument {
                ome.instruments
                    .push(crate::common::ome_metadata::OmeInstrument {
                        id: Some("Instrument:0".to_string()),
                        microscope_model: Some(instrument.clone()),
                        ..Default::default()
                    });
                img.instrument_ref = Some(0);
            }
        }
        Some(ome)
    }
}

// ── PictReader ────────────────────────────────────────────────────────────────

const PICT_CLIP_RGN: u16 = 0x0001;
const PICT_BITSRECT: u16 = 0x0090;
const PICT_BITSRGN: u16 = 0x0091;
const PICT_PACKBITSRECT: u16 = 0x0098;
const PICT_PACKBITSRGN: u16 = 0x0099;
const PICT_PIXMAP_9A: u16 = 0x009a;
const PICT_END: u16 = 0x00ff;
const PICT_LONGCOMMENT: u16 = 0x00a1;
const PICT_JPEG: u16 = 0x0018;
const PICT_TYPE_1: u16 = 0x0a9f;
const PICT_TYPE_2: u16 = 0x9190;

pub struct PictReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels: Option<Vec<u8>>,
}

impl PictReader {
    pub fn new() -> Self {
        PictReader {
            path: None,
            meta: None,
            pixels: None,
        }
    }
}

impl Default for PictReader {
    fn default() -> Self {
        Self::new()
    }
}

pub(crate) struct PictDecoded {
    pub(crate) meta: ImageMetadata,
    pub(crate) pixels: Vec<u8>,
}

struct PictCursor<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> PictCursor<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }

    fn seek(&mut self, pos: usize) -> Result<()> {
        if pos > self.data.len() {
            return Err(BioFormatsError::Format(
                "PICT: seek past end of file".into(),
            ));
        }
        self.pos = pos;
        Ok(())
    }

    fn position(&self) -> usize {
        self.pos
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.pos)
    }

    fn read_u8(&mut self) -> Result<u8> {
        if self.pos >= self.data.len() {
            return Err(BioFormatsError::Format(
                "PICT: unexpected end of file".into(),
            ));
        }
        let v = self.data[self.pos];
        self.pos += 1;
        Ok(v)
    }

    fn read_u16(&mut self) -> Result<u16> {
        let b = self.read_exact(2)?;
        Ok(u16::from_be_bytes([b[0], b[1]]))
    }

    fn read_i16(&mut self) -> Result<i16> {
        Ok(self.read_u16()? as i16)
    }

    fn read_exact(&mut self, len: usize) -> Result<&'a [u8]> {
        if self.pos + len > self.data.len() {
            return Err(BioFormatsError::Format(
                "PICT: unexpected end of file".into(),
            ));
        }
        let out = &self.data[self.pos..self.pos + len];
        self.pos += len;
        Ok(out)
    }

    fn skip(&mut self, len: usize) -> Result<()> {
        self.read_exact(len).map(|_| ())
    }
}

fn new_pict_meta(
    width: u32,
    height: u32,
    size_c: u32,
    is_rgb: bool,
    is_indexed: bool,
    lookup_table: Option<LookupTable>,
    version_one: bool,
) -> ImageMetadata {
    let mut series_metadata = HashMap::new();
    series_metadata.insert(
        "Version".into(),
        MetadataValue::Int(if version_one { 1 } else { 2 }),
    );
    ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c,
        size_t: 1,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: 8,
        image_count: 1,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: false,
        is_indexed,
        is_little_endian: false,
        resolution_count: 1,
        thumbnail: false,
        series_metadata,
        lookup_table,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    }
}

/// Decode a QuickTime-compressed (JPEG) PICT payload (opcode 0x0018).
///
/// Faithful to the Java `PictReader`: the JPEG data begins two bytes after the
/// opcode (`jpegOffsets[0] = filePointer + 2`). Additional embedded JPEG
/// streams (tiles) are located by scanning for SOI (0xFFD8) markers after the
/// first EOI (0xFFD9). Each stream is concatenated and decoded, then exposed as
/// interleaved 3-channel RGB (`sizeC = 3`, `rgb = true`, `interleaved = true`).
fn decode_pict_jpeg(
    c: &mut PictCursor<'_>,
    fallback_width: u32,
    fallback_height: u32,
    version_one: bool,
) -> Result<PictDecoded> {
    let data = c.data;
    // jpegOffsets[0] = filePointer + 2 (current position is just past opcode).
    let first = c.position() + 2;
    if first >= data.len() {
        return Err(BioFormatsError::Format(
            "PICT: truncated JPEG payload".into(),
        ));
    }

    // Collect the start offsets of each embedded JPEG stream, following the
    // Java scan: skip 16-bit words until the first EOI (0xFFD9), then for each
    // subsequent SOI (0xFFD8) record offset (filePointer - 2).
    let mut offsets = vec![first];
    let mut pos = first;
    // advance to first EOI
    while pos + 1 < data.len() {
        let v = u16::from_be_bytes([data[pos], data[pos + 1]]);
        pos += 2;
        if v == 0xffd9 {
            break;
        }
    }
    while pos + 1 < data.len() {
        let v = u16::from_be_bytes([data[pos], data[pos + 1]]);
        pos += 2;
        if v == 0xffd8 {
            offsets.push(pos - 2);
        }
    }

    // Concatenate each JPEG stream (from its offset up to the next offset, or
    // end of file for the last) and decode. For the common single-stream case
    // this decodes the JPEG from `first` to end of file.
    let mut combined: Vec<u8> = Vec::new();
    for (i, &start) in offsets.iter().enumerate() {
        let end = offsets.get(i + 1).copied().unwrap_or(data.len());
        if start >= end || end > data.len() {
            continue;
        }
        let decoded = crate::common::codec::decompress_jpeg(&data[start..end])?;
        combined.extend_from_slice(&decoded);
    }
    if combined.is_empty() {
        return Err(BioFormatsError::Format(
            "PICT: embedded JPEG produced no pixels".into(),
        ));
    }

    // Derive dimensions from the decoded RGB buffer length when possible.
    let (width, height) = if fallback_width > 0 && fallback_height > 0 {
        (fallback_width, fallback_height)
    } else {
        return Err(BioFormatsError::Format(
            "PICT: JPEG payload without known dimensions".into(),
        ));
    };

    let expected = width as usize * height as usize * 3;
    if combined.len() < expected {
        combined.resize(expected, 0);
    } else if combined.len() > expected {
        combined.truncate(expected);
    }

    let mut meta = new_pict_meta(width, height, 3, true, false, None, version_one);
    meta.is_interleaved = true;
    Ok(PictDecoded {
        meta,
        pixels: combined,
    })
}

fn parse_pict(path: &Path) -> Result<PictDecoded> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    parse_pict_bytes(&data)
}

fn pict_text_masquerade(data: &[u8]) -> bool {
    let prefix_len = data.len().min(128);
    let text = String::from_utf8_lossy(&data[..prefix_len]);
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    let prefix = trimmed
        .chars()
        .take(32)
        .collect::<String>()
        .to_ascii_lowercase();
    prefix.starts_with("<!doctype html")
        || prefix.starts_with("<html")
        || prefix.starts_with("<?xml")
}

fn pict_header_probe(header: &[u8]) -> bool {
    if pict_text_masquerade(header) {
        return false;
    }
    if header.len() < 524 {
        return false;
    }
    let top = i16::from_be_bytes([header[514], header[515]]);
    let left = i16::from_be_bytes([header[516], header[517]]);
    let bottom = i16::from_be_bytes([header[518], header[519]]);
    let right = i16::from_be_bytes([header[520], header[521]]);
    if bottom <= top || right <= left {
        return false;
    }
    match &header[522..524] {
        [0x11, 0x01] => true,
        [0x00, 0x11] => header
            .get(524..526)
            .is_some_and(|version| version == [0x02, 0xff]),
        _ => false,
    }
}

pub(crate) fn parse_pict_bytes(data: &[u8]) -> Result<PictDecoded> {
    if pict_text_masquerade(data) {
        return Err(BioFormatsError::UnsupportedFormat(
            "PICT reader received a text/HTML document, not an Apple PICT image".into(),
        ));
    }

    let mut c = PictCursor::new(data);
    c.seek(518)?;
    let mut height = c.read_i16()?.max(0) as u32;
    let mut width = c.read_i16()?.max(0) as u32;

    let ver_opcode = c.read_u8()?;
    let ver_number = c.read_u8()?;
    let version_one = if ver_opcode == 0x11 && ver_number == 0x01 {
        true
    } else if ver_opcode == 0x00 && ver_number == 0x11 {
        let ver_number2 = c.read_u16()?;
        if ver_number2 != 0x02ff {
            return Err(BioFormatsError::Format(format!(
                "Invalid PICT file: {ver_number2}"
            )));
        }
        c.skip(6)?;
        let _pixels_per_inch_x = c.read_exact(4)?;
        let _pixels_per_inch_y = c.read_exact(4)?;
        c.skip(4)?;
        let y = c.read_i16()?;
        let x = c.read_i16()?;
        if y > 0 {
            height = y as u32;
        }
        if x > 0 {
            width = x as u32;
        }
        c.skip(4)?;
        false
    } else {
        return Err(BioFormatsError::Format("Invalid PICT file".into()));
    };

    let mut decoded = None;
    loop {
        let opcode = if version_one {
            if c.remaining() == 0 {
                break;
            }
            c.read_u8()? as u16
        } else {
            if c.position() & 1 != 0 {
                c.skip(1)?;
            }
            if c.remaining() < 2 {
                break;
            }
            c.read_u16()?
        };

        match opcode {
            PICT_BITSRECT | PICT_BITSRGN | PICT_PACKBITSRECT | PICT_PACKBITSRGN => {
                let row_bytes = c.read_u16()?;
                decoded = Some(parse_pict_image(
                    &mut c,
                    opcode,
                    row_bytes,
                    version_one,
                    width,
                    height,
                )?);
            }
            PICT_PIXMAP_9A => {
                decoded = Some(parse_pict_image(
                    &mut c,
                    opcode,
                    0,
                    version_one,
                    width,
                    height,
                )?);
            }
            PICT_CLIP_RGN => {
                let len = c.read_u16()? as usize;
                if len < 2 {
                    return Err(BioFormatsError::Format("PICT: invalid clip region".into()));
                }
                c.skip(len - 2)?;
            }
            PICT_LONGCOMMENT => {
                c.skip(2)?;
                let len = c.read_u16()? as usize;
                c.skip(len)?;
            }
            PICT_JPEG => {
                decoded = Some(decode_pict_jpeg(&mut c, width, height, version_one)?);
                break;
            }
            PICT_TYPE_1 | PICT_TYPE_2 => {
                let len = c.read_u8()? as usize;
                c.skip(len)?;
            }
            PICT_END => break,
            _ if c.remaining() == 0 => break,
            _ => {}
        }
    }

    decoded.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(
            "PICT: no supported bitmap/pixmap payload found; native vector-only PICT drawing is unsupported"
                .into(),
        )
    })
}

fn parse_pict_image(
    c: &mut PictCursor<'_>,
    opcode: u16,
    row_bytes_raw: u16,
    version_one: bool,
    fallback_width: u32,
    fallback_height: u32,
) -> Result<PictDecoded> {
    let is_bitmap = version_one || (row_bytes_raw & 0x8000) == 0;
    if is_bitmap && opcode != PICT_PIXMAP_9A {
        let row_bytes = (row_bytes_raw & 0x3fff) as usize;
        let (width, height) = read_pict_image_rect(c, opcode, fallback_width, fallback_height)?;
        skip_pict_mask_region(c, opcode)?;
        let rows = read_pict_rows(c, opcode, row_bytes, width as usize, height as usize, 1, 1)?;
        let mut pixels = Vec::with_capacity(width as usize * height as usize);
        for row in rows {
            pixels.extend(expand_pict_bits(1, &row, width as usize)?);
        }
        let meta = new_pict_meta(width, height, 1, false, false, None, version_one);
        return Ok(PictDecoded { meta, pixels });
    }

    let mut row_bytes = if opcode == PICT_PIXMAP_9A {
        0
    } else {
        (row_bytes_raw & 0x3fff) as usize
    };
    let (width, height) = read_pict_image_rect(c, opcode, fallback_width, fallback_height)?;
    let pixel_size = c.read_u16()?;
    let comp_count = c.read_u16()? as usize;
    c.skip(14)?;

    let lookup_table = if opcode == PICT_PIXMAP_9A {
        match pixel_size {
            32 => row_bytes = width as usize * comp_count,
            16 => row_bytes = width as usize * 2,
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(
                    format!(
                        "PICT vector pixmap payloads with {pixel_size}-bit pixels are unsupported; only direct 16/32-bit pixmaps are decoded"
                    ),
                ));
            }
        }
        None
    } else {
        c.skip(4)?;
        c.skip(2)?;
        let count = c.read_u16()? as usize + 1;
        let mut red = Vec::with_capacity(count);
        let mut green = Vec::with_capacity(count);
        let mut blue = Vec::with_capacity(count);
        for _ in 0..count {
            c.skip(2)?;
            red.push((c.read_u8()? as u16) << 8);
            c.skip(1)?;
            green.push((c.read_u8()? as u16) << 8);
            c.skip(1)?;
            blue.push((c.read_u8()? as u16) << 8);
            c.skip(1)?;
        }
        Some(LookupTable { red, green, blue })
    };

    c.skip(18)?;
    skip_pict_mask_region(c, opcode)?;

    let rows = read_pict_rows(
        c,
        opcode,
        row_bytes,
        width as usize,
        height as usize,
        pixel_size,
        comp_count,
    )?;
    rows_to_pict_decoded(
        rows,
        width,
        height,
        pixel_size,
        comp_count,
        lookup_table,
        version_one,
    )
}

fn skip_pict_mask_region(c: &mut PictCursor<'_>, opcode: u16) -> Result<()> {
    if opcode == PICT_BITSRGN || opcode == PICT_PACKBITSRGN {
        let len = c.read_u16()? as usize;
        if len < 2 {
            return Err(BioFormatsError::Format("PICT: invalid mask region".into()));
        }
        c.skip(len - 2)?;
    }
    Ok(())
}

fn read_pict_image_rect(
    c: &mut PictCursor<'_>,
    opcode: u16,
    fallback_width: u32,
    fallback_height: u32,
) -> Result<(u32, u32)> {
    if opcode == PICT_PIXMAP_9A {
        c.skip(6)?;
    }
    let top = c.read_i16()?;
    let left = c.read_i16()?;
    let bottom = c.read_i16()?;
    let right = c.read_i16()?;
    let width = if right > left {
        (right - left) as u32
    } else {
        fallback_width
    };
    let height = if bottom > top {
        (bottom - top) as u32
    } else {
        fallback_height
    };
    c.skip(18)?;
    if width == 0 || height == 0 {
        return Err(BioFormatsError::Format(
            "PICT: missing image dimensions".into(),
        ));
    }
    Ok((width, height))
}

fn read_pict_rows(
    c: &mut PictCursor<'_>,
    _opcode: u16,
    row_bytes: usize,
    width: usize,
    height: usize,
    pixel_size: u16,
    comp_count: usize,
) -> Result<Vec<Vec<u8>>> {
    let compressed = row_bytes >= 8 || pixel_size == 32;
    let mut rows = Vec::with_capacity(height);
    for _ in 0..height {
        let row = if compressed {
            let raw_len = if row_bytes > 250 {
                c.read_u16()? as usize
            } else {
                c.read_u8()? as usize
            };
            let packed = c.read_exact(raw_len)?;
            if pixel_size == 16 {
                unpack_pict_16_packbits(packed, width)?
            } else {
                let out = decompress_packbits(packed)?;
                let min_len = match pixel_size {
                    24 | 32 => width.saturating_mul(comp_count),
                    _ => row_bytes.max(width),
                };
                if out.len() < min_len {
                    return Err(BioFormatsError::InvalidData(format!(
                        "PICT PackBits row decoded to {} bytes, expected at least {min_len}",
                        out.len()
                    )));
                }
                out
            }
        } else {
            c.read_exact(row_bytes)?.to_vec()
        };
        rows.push(row);
    }
    Ok(rows)
}

fn rows_to_pict_decoded(
    rows: Vec<Vec<u8>>,
    width: u32,
    height: u32,
    pixel_size: u16,
    comp_count: usize,
    lookup_table: Option<LookupTable>,
    version_one: bool,
) -> Result<PictDecoded> {
    let plane = width as usize * height as usize;
    match pixel_size {
        1 | 2 | 4 => {
            let mut pixels = Vec::with_capacity(plane);
            for row in rows {
                pixels.extend(expand_pict_bits(pixel_size as u8, &row, width as usize)?);
            }
            let meta = new_pict_meta(
                width,
                height,
                1,
                false,
                lookup_table.is_some(),
                lookup_table,
                version_one,
            );
            Ok(PictDecoded { meta, pixels })
        }
        8 => {
            let mut pixels = Vec::with_capacity(plane);
            for row in rows {
                if row.len() < width as usize {
                    return Err(BioFormatsError::Format("PICT: short 8-bit row".into()));
                }
                pixels.extend_from_slice(&row[..width as usize]);
            }
            let meta = new_pict_meta(
                width,
                height,
                1,
                false,
                lookup_table.is_some(),
                lookup_table,
                version_one,
            );
            Ok(PictDecoded { meta, pixels })
        }
        16 => {
            let mut pixels = vec![0u8; plane * 3];
            for (y, row) in rows.iter().enumerate() {
                for x in 0..width as usize {
                    let off = x * 2;
                    if off + 1 >= row.len() {
                        continue;
                    }
                    let v = u16::from_be_bytes([row[off], row[off + 1]]);
                    let base = y * width as usize + x;
                    pixels[base] = ((v & 0x7c00) >> 10) as u8;
                    pixels[plane + base] = ((v & 0x03e0) >> 5) as u8;
                    pixels[2 * plane + base] = (v & 0x001f) as u8;
                }
            }
            let meta = new_pict_meta(width, height, 3, true, false, None, version_one);
            Ok(PictDecoded { meta, pixels })
        }
        24 | 32 => {
            let channels = comp_count.max(3);
            let mut pixels = vec![0u8; plane * 3];
            for (y, row) in rows.iter().enumerate() {
                for q in 0..3 {
                    let src_channel = if channels > 3 { channels - 3 + q } else { q };
                    let src = src_channel * width as usize;
                    let dst = q * plane + y * width as usize;
                    if src < row.len() {
                        let len = (width as usize).min(row.len() - src);
                        pixels[dst..dst + len].copy_from_slice(&row[src..src + len]);
                    }
                }
            }
            let meta = new_pict_meta(width, height, 3, true, false, None, version_one);
            Ok(PictDecoded { meta, pixels })
        }
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "PICT pixel size {other} is unsupported; supported bitmap/pixmap pixel sizes are 1, 2, 4, 8, 16, 24, and 32"
        ))),
    }
}

fn expand_pict_bits(bit_size: u8, input: &[u8], out_len: usize) -> Result<Vec<u8>> {
    if !matches!(bit_size, 1 | 2 | 4) {
        return Err(BioFormatsError::Format(format!(
            "PICT cannot expand {bit_size}-bit pixels"
        )));
    }
    let mut out = Vec::with_capacity(out_len);
    let count = 8 / bit_size;
    let mask = (1u8 << bit_size) - 1;
    for &byte in input {
        for i in 0..count {
            if out.len() == out_len {
                return Ok(out);
            }
            let shift = 8 - bit_size * (i + 1);
            out.push((byte >> shift) & mask);
            if out.len() == out_len {
                return Ok(out);
            }
        }
    }
    Err(BioFormatsError::InvalidData(format!(
        "PICT bit row expanded to {} pixels, expected {out_len}",
        out.len()
    )))
}

fn unpack_pict_16_packbits(input: &[u8], width: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(width * 2);
    let mut i = 0usize;
    while i < input.len() && out.len() < width * 2 {
        let header = input[i] as i8;
        i += 1;
        if header >= 0 {
            let count = header as usize + 1;
            for _ in 0..count {
                if i + 1 >= input.len() {
                    return Err(BioFormatsError::InvalidData(
                        "PICT PackBits16: literal run overruns input".into(),
                    ));
                }
                out.extend_from_slice(&input[i..i + 2]);
                i += 2;
            }
        } else if header != -128 {
            if i + 1 >= input.len() {
                return Err(BioFormatsError::InvalidData(
                    "PICT PackBits16: repeat run missing sample".into(),
                ));
            }
            let sample = [input[i], input[i + 1]];
            i += 2;
            for _ in 0..((-header as usize) + 1) {
                out.extend_from_slice(&sample);
            }
        }
    }
    if out.len() != width * 2 {
        return Err(BioFormatsError::InvalidData(format!(
            "PICT PackBits16 row decoded to {} bytes, expected {}",
            out.len(),
            width * 2
        )));
    }
    Ok(out)
}

fn pict_region_crop(full: &[u8], meta: &ImageMetadata, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let bps = meta.pixel_type.bytes_per_sample();
    if meta.is_rgb && !meta.is_interleaved {
        let plane = meta.size_x as usize * meta.size_y as usize * bps;
        let out_plane = w as usize * h as usize * bps;
        let mut out = vec![0; out_plane * meta.size_c as usize];
        for c in 0..meta.size_c as usize {
            let src_plane = &full[c * plane..(c + 1) * plane];
            for r in 0..h as usize {
                let src = (y as usize + r) * meta.size_x as usize * bps + x as usize * bps;
                let dst = c * out_plane + r * w as usize * bps;
                out[dst..dst + w as usize * bps]
                    .copy_from_slice(&src_plane[src..src + w as usize * bps]);
            }
        }
        return out;
    }
    if meta.is_rgb && meta.is_interleaved {
        // Interleaved RGB (e.g. JPEG-in-PICT): samples are packed per pixel.
        let spp = meta.size_c as usize;
        let row = meta.size_x as usize * spp * bps;
        let out_row = w as usize * spp * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            let start = x as usize * spp * bps;
            out.extend_from_slice(&src[start..start + out_row]);
        }
        return out;
    }
    region_crop(full, meta, x, y, w, h)
}

impl FormatReader for PictReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pict") | Some("pct"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        pict_header_probe(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let decoded = parse_pict(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(decoded.meta);
        self.pixels = Some(decoded.pixels);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_some() && s == 0 {
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.pixels
            .as_ref()
            .cloned()
            .ok_or(BioFormatsError::NotInitialized)
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
        validate_region("PICT", meta.size_x, meta.size_y, x, y, w, h)?;
        Ok(pict_region_crop(&full, meta, x, y, w, h))
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

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bioformats_legacy_{}_{}.pct",
            name,
            std::process::id()
        ))
    }

    fn push_u16(out: &mut Vec<u8>, v: u16) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    fn push_u32(out: &mut Vec<u8>, v: u32) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    fn kodak_tmp(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "bioformats_legacy_{}_{}.bip",
            name,
            std::process::id()
        ))
    }

    fn build_kodak_bip(width: u32, height: u32, metadata: &[u8], file_info: &[u8]) -> Vec<u8> {
        let mut out = b"DTag".to_vec();
        out.extend_from_slice(metadata);
        out.extend_from_slice(b"GBiH");
        out.extend_from_slice(&[0; 20]);
        push_u32(&mut out, width);
        push_u32(&mut out, height);
        out.extend_from_slice(b"DLFi");
        out.extend_from_slice(&[0; 16]);
        push_u32(&mut out, (b"DLFi".len() + 20 + file_info.len()) as u32);
        out.extend_from_slice(file_info);
        out.extend_from_slice(b"BSfD");
        out.extend_from_slice(&[0; 20]);
        for i in 0..width * height {
            out.extend_from_slice(&(i as f32 + 0.5).to_be_bytes());
        }
        out
    }

    fn pict_v2_prefix(width: u16, height: u16) -> Vec<u8> {
        let mut out = vec![0; 512];
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, 0);
        push_u16(&mut out, height);
        push_u16(&mut out, width);
        out.extend_from_slice(&[0x00, 0x11]);
        push_u16(&mut out, 0x02ff);
        out.extend_from_slice(&[0; 6]);
        out.extend_from_slice(&72u32.to_be_bytes());
        out.extend_from_slice(&72u32.to_be_bytes());
        out.extend_from_slice(&[0; 4]);
        push_u16(&mut out, height);
        push_u16(&mut out, width);
        out.extend_from_slice(&[0; 4]);
        out
    }

    #[test]
    fn kodak_reads_pixels_and_java_metadata_blocks() {
        let path = kodak_tmp("metadata");
        let metadata = b"Image Capture Source: Image Station 4000MM\nCapture Time/Date: 12:34:56 on 03/04/2005\nExposure Time: 1.25 sec\nHorizontal Resolution: 5080 dpi\nVertical Resolution: 2540 dpi\nCCD Temperature: 22 C\n\0";
        let data = build_kodak_bip(2, 1, metadata, b"first line\r\nsecond line");
        std::fs::write(&path, data).unwrap();

        let mut reader = KodakReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (2, 1));
        assert_eq!(meta.pixel_type, PixelType::Float32);
        assert!(!meta.is_little_endian);
        assert_eq!(
            meta.series_metadata
                .get("Image Capture Source")
                .map(|v| v.to_string()),
            Some("Image Station 4000MM".to_string())
        );
        assert_eq!(
            meta.series_metadata.get("FileInfo").map(|v| v.to_string()),
            Some("first line | second line".to_string())
        );
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            [0.5f32.to_be_bytes(), 1.5f32.to_be_bytes()].concat()
        );

        let ome = reader.ome_metadata().unwrap();
        let image = &ome.images[0];
        assert_eq!(
            image.acquisition_date.as_deref(),
            Some("2005-03-04T12:34:56")
        );
        assert_eq!(image.planes[0].exposure_time, Some(1.25));
        assert_eq!(image.physical_size_x, Some(5.0));
        assert_eq!(image.physical_size_y, Some(10.0));
        assert_eq!(image.imaging_environment_temperature, Some(22.0));
        assert_eq!(
            ome.instruments[0].microscope_model.as_deref(),
            Some("Image Station 4000MM")
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn kodak_hex_ccd_temperature_is_metadata_only_like_java() {
        let path = kodak_tmp("hex_temp");
        let metadata = b"Image Capture Source: Image Station\nCCD Temperature: 0xEB\n\0";
        let data = build_kodak_bip(1, 1, metadata, b"");
        std::fs::write(&path, data).unwrap();

        let mut reader = KodakReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(
            reader
                .metadata()
                .series_metadata
                .get("CCD Temperature")
                .map(|v| v.to_string()),
            Some("0xEB".to_string())
        );
        assert_eq!(
            reader.ome_metadata().unwrap().images[0].imaging_environment_temperature,
            None
        );

        std::fs::remove_file(path).ok();
    }

    fn append_pixmap_8(out: &mut Vec<u8>, width: u16, height: u16, rows: &[&[u8]]) {
        if out.len() & 1 != 0 {
            out.push(0);
        }
        push_u16(out, PICT_PACKBITSRECT);
        push_u16(out, 0x8000 | width);
        push_u16(out, 0);
        push_u16(out, 0);
        push_u16(out, height);
        push_u16(out, width);
        out.extend_from_slice(&[0; 18]);
        push_u16(out, 8);
        push_u16(out, 1);
        out.extend_from_slice(&[0; 14]);
        out.extend_from_slice(&[0; 4]);
        push_u16(out, 0);
        push_u16(out, 1);
        for i in 0..2u8 {
            push_u16(out, i as u16);
            out.extend_from_slice(&[i, 0, i, 0, i, 0]);
        }
        out.extend_from_slice(&[0; 18]);
        for row in rows {
            out.push(row.len() as u8);
            out.extend_from_slice(row);
        }
        if out.len() & 1 != 0 {
            out.push(0);
        }
        push_u16(out, PICT_END);
    }

    fn append_vector_pixmap_9a_header(out: &mut Vec<u8>, width: u16, height: u16, pixel_size: u16) {
        if out.len() & 1 != 0 {
            out.push(0);
        }
        push_u16(out, PICT_PIXMAP_9A);
        out.extend_from_slice(&[0; 6]);
        push_u16(out, 0);
        push_u16(out, 0);
        push_u16(out, height);
        push_u16(out, width);
        out.extend_from_slice(&[0; 18]);
        push_u16(out, pixel_size);
        push_u16(out, 1);
        out.extend_from_slice(&[0; 14]);
    }

    fn append_pixmap_with_pixel_size(
        out: &mut Vec<u8>,
        width: u16,
        height: u16,
        pixel_size: u16,
        rows: &[&[u8]],
    ) {
        if out.len() & 1 != 0 {
            out.push(0);
        }
        push_u16(out, PICT_PACKBITSRECT);
        push_u16(out, 0x8000 | 2);
        push_u16(out, 0);
        push_u16(out, 0);
        push_u16(out, height);
        push_u16(out, width);
        out.extend_from_slice(&[0; 18]);
        push_u16(out, pixel_size);
        push_u16(out, 1);
        out.extend_from_slice(&[0; 14]);
        out.extend_from_slice(&[0; 4]);
        push_u16(out, 0);
        push_u16(out, 0);
        push_u16(out, 0);
        out.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
        out.extend_from_slice(&[0; 18]);
        for row in rows {
            out.extend_from_slice(row);
        }
        if out.len() & 1 != 0 {
            out.push(0);
        }
        push_u16(out, PICT_END);
    }

    #[test]
    fn pict_v2_packbits_indexed_rows_decode_and_crop() {
        let path = tmp("packbits");
        let mut data = pict_v2_prefix(8, 2);
        append_pixmap_8(
            &mut data,
            8,
            2,
            &[
                &[7, 1, 2, 3, 4, 5, 6, 7, 8],
                &[7, 9, 10, 11, 12, 13, 14, 15, 16],
            ],
        );
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (8, 2));
        assert!(meta.is_indexed);
        assert_eq!(meta.image_count, 1);
        assert_eq!(meta.dimension_order, DimensionOrder::XYCZT);
        assert!(!meta.is_rgb);
        assert!(!meta.is_interleaved);
        assert!(!meta.is_little_endian);
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16]
        );
        assert_eq!(
            reader.open_bytes_region(0, 2, 0, 3, 2).unwrap(),
            vec![3, 4, 5, 11, 12, 13]
        );
        let err = reader.open_bytes_region(0, 0, 0, 0, 1).unwrap_err();
        assert!(
            err.to_string()
                .contains("width and height must be non-zero"),
            "unexpected error: {err}"
        );
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images.len(), 1);
        assert_eq!(ome.images[0].channels.len(), 1);
        assert_eq!(ome.images[0].channels[0].samples_per_pixel, 1);

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_packbits_region_skips_full_mask_region() {
        let path = tmp("packbits_region");
        let mut data = pict_v2_prefix(8, 1);
        if data.len() & 1 != 0 {
            data.push(0);
        }
        push_u16(&mut data, PICT_PACKBITSRGN);
        push_u16(&mut data, 0x8000 | 8);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 1);
        push_u16(&mut data, 8);
        data.extend_from_slice(&[0; 18]);
        push_u16(&mut data, 8);
        push_u16(&mut data, 1);
        data.extend_from_slice(&[0; 14]);
        data.extend_from_slice(&[0; 4]);
        push_u16(&mut data, 0);
        push_u16(&mut data, 1);
        for i in 0..2u8 {
            push_u16(&mut data, i as u16);
            data.extend_from_slice(&[i, 0, i, 0, i, 0]);
        }
        data.extend_from_slice(&[0; 18]);
        push_u16(&mut data, 10);
        data.extend_from_slice(&[0, 0, 0, 1, 0, 8, 0, 0]);
        data.push(9);
        data.extend_from_slice(&[7, 1, 2, 3, 4, 5, 6, 7, 8]);
        if data.len() & 1 != 0 {
            data.push(0);
        }
        push_u16(&mut data, PICT_END);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6, 7, 8]);

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_bitmap_bits_region_skips_full_mask_region() {
        let path = tmp("bitmap_bits_region");
        let mut data = pict_v2_prefix(8, 1);
        if data.len() & 1 != 0 {
            data.push(0);
        }
        push_u16(&mut data, PICT_BITSRGN);
        push_u16(&mut data, 1);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 1);
        push_u16(&mut data, 8);
        data.extend_from_slice(&[0; 18]);
        push_u16(&mut data, 10);
        data.extend_from_slice(&[0, 0, 0, 1, 0, 8, 0, 0]);
        data.push(0b1010_0101);
        if data.len() & 1 != 0 {
            data.push(0);
        }
        push_u16(&mut data, PICT_END);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 0, 1, 0, 0, 1, 0, 1]);

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_v2_packbits_short_row_is_rejected() {
        let path = tmp("packbits_short");
        let mut data = pict_v2_prefix(8, 1);
        append_pixmap_8(&mut data, 8, 1, &[&[3, 1, 2, 3, 4]]);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(err.to_string().contains("PackBits row decoded"));

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_16_bit_direct_color_matches_java_5_bit_channels() {
        let path = tmp("pixmap16");
        let mut data = pict_v2_prefix(2, 1);
        if data.len() & 1 != 0 {
            data.push(0);
        }
        push_u16(&mut data, PICT_BITSRECT);
        push_u16(&mut data, 0x8004);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 1);
        push_u16(&mut data, 2);
        data.extend_from_slice(&[0; 18]);
        push_u16(&mut data, 16);
        push_u16(&mut data, 1);
        data.extend_from_slice(&[0; 14]);
        data.extend_from_slice(&[0; 4]);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        data.extend_from_slice(&[0; 6]);
        data.extend_from_slice(&[0; 18]);
        push_u16(&mut data, 0x7fff);
        push_u16(&mut data, 0x001f);
        if data.len() & 1 != 0 {
            data.push(0);
        }
        push_u16(&mut data, PICT_END);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.metadata().size_c, 3);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![31, 0, 31, 0, 31, 31]);

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_32_bit_direct_color_skips_alpha_plane() {
        let path = tmp("pixmap32");
        let mut data = pict_v2_prefix(2, 1);
        if data.len() & 1 != 0 {
            data.push(0);
        }
        push_u16(&mut data, PICT_BITSRECT);
        push_u16(&mut data, 0x8008);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 1);
        push_u16(&mut data, 2);
        data.extend_from_slice(&[0; 18]);
        push_u16(&mut data, 32);
        push_u16(&mut data, 4);
        data.extend_from_slice(&[0; 14]);
        data.extend_from_slice(&[0; 4]);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        data.extend_from_slice(&[0; 6]);
        data.extend_from_slice(&[0; 18]);
        data.push(9);
        data.extend_from_slice(&[7, 0xaa, 0xbb, 1, 2, 3, 4, 5, 6]);
        if data.len() & 1 != 0 {
            data.push(0);
        }
        push_u16(&mut data, PICT_END);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y, meta.size_c), (2, 1, 3));
        assert!(meta.is_rgb);
        assert!(!meta.is_interleaved);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 1).unwrap(),
            vec![2, 4, 6]
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_jpeg_payload_reads_interleaved_rgb_and_crops() {
        let path = tmp("jpeg_payload");
        let mut jpeg = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 100)
            .encode(&[255, 0, 0, 0, 255, 0], 2, 1, image::ColorType::Rgb8.into())
            .unwrap();

        let mut data = pict_v2_prefix(2, 1);
        if data.len() & 1 != 0 {
            data.push(0);
        }
        push_u16(&mut data, PICT_JPEG);
        data.extend_from_slice(&[0; 2]);
        data.extend_from_slice(&jpeg);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y, meta.size_c), (2, 1, 3));
        assert!(meta.is_rgb);
        assert!(meta.is_interleaved);
        assert_eq!(meta.image_count, 1);
        assert_eq!(meta.dimension_order, DimensionOrder::XYCZT);
        assert!(!meta.is_little_endian);
        let pixels = reader.open_bytes(0).unwrap();
        assert_eq!(pixels.len(), 6);
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 1).unwrap(),
            pixels[3..6]
        );
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images.len(), 1);
        assert_eq!(ome.images[0].channels.len(), 1);
        assert_eq!(ome.images[0].channels[0].samples_per_pixel, 3);

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_v2_vector_pixmap_9a_boundary_is_explicit_unsupported() {
        let path = tmp("vector_pixmap_9a");
        let mut data = pict_v2_prefix(2, 1);
        append_vector_pixmap_9a_header(&mut data, 2, 1, 8);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("vector pixmap payloads with 8-bit pixels are unsupported")),
            "unexpected error: {err}"
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_v2_unsupported_pixmap_pixel_size_is_explicit() {
        let path = tmp("unsupported_pixel_size");
        let mut data = pict_v2_prefix(2, 1);
        append_pixmap_with_pixel_size(&mut data, 2, 1, 12, &[&[0, 0]]);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("PICT pixel size 12 is unsupported")),
            "unexpected error: {err}"
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_v1_bitmap_bits_expand() {
        let path = tmp("bitmap");
        let mut data = vec![0; 512];
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 2);
        push_u16(&mut data, 8);
        data.extend_from_slice(&[0x11, 0x01, PICT_BITSRECT as u8]);
        push_u16(&mut data, 1);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 2);
        push_u16(&mut data, 8);
        data.extend_from_slice(&[0; 18]);
        data.extend_from_slice(&[0b1010_0101, 0b0101_1010, PICT_END as u8]);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            vec![1, 0, 1, 0, 0, 1, 0, 1, 0, 1, 0, 1, 1, 0, 1, 0]
        );

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_byte_detection_requires_bounding_rect_and_v2_marker() {
        let reader = PictReader::new();
        let mut masquerade = vec![b' '; 526];
        masquerade[..15].copy_from_slice(b"<!doctype html>");
        masquerade[522] = 0x00;
        masquerade[523] = 0x11;
        assert!(!reader.is_this_type_by_bytes(&masquerade));

        let mut valid_looking_html = vec![b' '; 526];
        valid_looking_html[..12].copy_from_slice(b" <HTML>\xffbody");
        valid_looking_html[514..522].copy_from_slice(&[0, 0, 0, 0, 0, 1, 0, 1]);
        valid_looking_html[522..526].copy_from_slice(&[0x00, 0x11, 0x02, 0xff]);
        assert!(!reader.is_this_type_by_bytes(&valid_looking_html));

        let mut valid = pict_v2_prefix(1, 1);
        valid.resize(526, 0);
        assert!(reader.is_this_type_by_bytes(&valid));

        valid[525] = 0xfe;
        assert!(!reader.is_this_type_by_bytes(&valid));
    }

    #[test]
    fn pict_text_masquerade_and_truncated_files_fail_cleanly() {
        let html = tmp("html_masquerade");
        std::fs::write(&html, b"\xef\xbb\xbf  <HTML><body></body>").unwrap();
        let xml = tmp("xml_masquerade");
        std::fs::write(&xml, b"  <?XML version=\"1.0\"?><root/>").unwrap();

        let mut reader = PictReader::new();
        let err = reader.set_id(&html).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("text/HTML document")),
            "unexpected error: {err}"
        );
        assert_eq!(reader.series_count(), 0);

        let err = reader.set_id(&xml).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("text/HTML document")),
            "unexpected error: {err}"
        );
        assert_eq!(reader.series_count(), 0);

        let unicode_html = tmp("unicode_html_masquerade");
        std::fs::write(
            &unicode_html,
            "<html>                         \u{e9}</html>",
        )
        .unwrap();
        let err = reader.set_id(&unicode_html).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("text/HTML document")),
            "unexpected error: {err}"
        );
        assert_eq!(reader.series_count(), 0);

        let invalid_utf8_html = tmp("invalid_utf8_html_masquerade");
        std::fs::write(&invalid_utf8_html, b" <html><body>\xff</body>").unwrap();
        let err = reader.set_id(&invalid_utf8_html).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("text/HTML document")),
            "unexpected error: {err}"
        );
        assert_eq!(reader.series_count(), 0);

        let truncated = tmp("truncated");
        let mut data = pict_v2_prefix(1, 1);
        data.push(0);
        push_u16(&mut data, PICT_PACKBITSRECT);
        std::fs::write(&truncated, data).unwrap();

        let err = reader.set_id(&truncated).unwrap_err();
        assert!(
            err.to_string().contains("unexpected end")
                || err.to_string().contains("no supported bitmap"),
            "unexpected error: {err}"
        );
        assert_eq!(reader.series_count(), 0);

        std::fs::remove_file(html).ok();
        std::fs::remove_file(xml).ok();
        std::fs::remove_file(unicode_html).ok();
        std::fs::remove_file(invalid_utf8_html).ok();
        std::fs::remove_file(truncated).ok();
    }

    #[test]
    fn pict_direct_rgb_nonzero_crop_is_planar_and_bounded() {
        let path = tmp("rgb_crop");
        let mut data = pict_v2_prefix(3, 2);
        if data.len() & 1 != 0 {
            data.push(0);
        }
        push_u16(&mut data, PICT_BITSRECT);
        push_u16(&mut data, 0x8006);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 2);
        push_u16(&mut data, 3);
        data.extend_from_slice(&[0; 18]);
        push_u16(&mut data, 16);
        push_u16(&mut data, 1);
        data.extend_from_slice(&[0; 14]);
        data.extend_from_slice(&[0; 4]);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        data.extend_from_slice(&[0; 6]);
        data.extend_from_slice(&[0; 18]);
        for sample in [0x7c00u16, 0x03e0, 0x001f, 0x7fff, 0x03ff, 0x7fe0] {
            push_u16(&mut data, sample);
        }
        if data.len() & 1 != 0 {
            data.push(0);
        }
        push_u16(&mut data, PICT_END);
        std::fs::write(&path, data).unwrap();

        let mut reader = PictReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(
            reader.open_bytes_region(0, 1, 1, 2, 1).unwrap(),
            vec![0, 31, 31, 31, 31, 0]
        );
        let err = reader.open_bytes_region(0, 2, 1, 2, 1).unwrap_err();
        assert!(err.to_string().contains("outside image bounds"));

        std::fs::remove_file(path).ok();
    }

    #[test]
    fn pict_failed_second_set_id_clears_previous_pixels() {
        let good = tmp("good_then_bad");
        let bad = tmp("bad_after_good");
        let mut data = vec![0; 512];
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 1);
        push_u16(&mut data, 1);
        data.extend_from_slice(&[0x11, 0x01, PICT_BITSRECT as u8]);
        push_u16(&mut data, 1);
        push_u16(&mut data, 0);
        push_u16(&mut data, 0);
        push_u16(&mut data, 1);
        push_u16(&mut data, 1);
        data.extend_from_slice(&[0; 18]);
        data.extend_from_slice(&[0x80, PICT_END as u8]);
        std::fs::write(&good, data).unwrap();
        std::fs::write(&bad, b"not a pict").unwrap();

        let mut reader = PictReader::new();
        reader.set_id(&good).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1]);

        assert!(reader.set_id(&bad).is_err());
        assert_eq!(reader.series_count(), 0);
        assert!(reader.set_series(0).is_err());
        assert!(matches!(
            reader.open_bytes(0),
            Err(BioFormatsError::NotInitialized)
        ));

        std::fs::remove_file(good).ok();
        std::fs::remove_file(bad).ok();
    }

    fn fuji_tmp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bioformats_fuji_{}_{}", name, std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Build a minimal but Java-shaped 14-line `.inf` header. Index meanings:
    /// 1=name, 3=physW, 4=physH, 5=bits, 6=sizeX, 7=sizeY, 10=date, 13=model.
    fn fuji_inf(name: &str, phys_w: &str, phys_h: &str, bits: u32, x: u32, y: u32) -> String {
        let mut lines = vec![String::new(); 14];
        lines[1] = name.to_string();
        lines[3] = phys_w.to_string();
        lines[4] = phys_h.to_string();
        lines[5] = bits.to_string();
        lines[6] = x.to_string();
        lines[7] = y.to_string();
        lines[10] = "Wed Jul 25 14:00:00 2007".to_string();
        lines[13] = "LAS-3000".to_string();
        // Use CRLF to exercise the \r? line splitting.
        lines.join("\r\n")
    }

    #[test]
    fn fuji_set_id_parses_header_and_reads_plane() {
        let dir = fuji_tmp_dir("roundtrip");
        let inf = dir.join("gel.inf");
        let img = dir.join("gel.img");

        // 16-bit, 3x2 = 6 samples => 12 bytes of little-endian pixel data.
        std::fs::write(&inf, fuji_inf("my gel", "10.5", "12.25", 16, 3, 2)).unwrap();
        let pixels: Vec<u8> = (0u8..12).collect();
        std::fs::write(&img, &pixels).unwrap();

        let mut reader = FujiReader::new();
        // Detection must be companion-driven: .img + .inf both present.
        assert!(reader.is_this_type_by_name(&img));
        assert!(reader.is_this_type_by_name(&inf));
        assert!(!reader.is_this_type_by_bytes(&[0u8; 16]));

        // set_id given the .img redirects to the .inf header.
        reader.set_id(&img).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (3, 2));
        assert_eq!(meta.size_z, 1);
        assert_eq!(meta.size_c, 1);
        assert_eq!(meta.size_t, 1);
        assert_eq!(meta.image_count, 1);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert_eq!(meta.bits_per_pixel, 16);
        assert!(meta.is_little_endian);
        // addGlobalMetaList("Line", ...) numbering: bare key then "Line #N".
        // MetadataValue has no PartialEq, so compare via its Display form.
        assert_eq!(
            meta.series_metadata.get("Line").map(|v| v.to_string()),
            Some(String::new())
        );
        assert_eq!(
            meta.series_metadata.get("Line #2").map(|v| v.to_string()),
            Some("my gel".to_string())
        );

        // Whole plane reads back verbatim.
        assert_eq!(reader.open_bytes(0).unwrap(), pixels);
        // Region crop: x=1,y=0,w=2,h=1 => samples 1,2 => bytes [2,3,4,5].
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 2, 1).unwrap(),
            vec![2, 3, 4, 5]
        );
        assert!(matches!(
            reader.open_bytes(1),
            Err(BioFormatsError::PlaneOutOfRange(1))
        ));

        // OME metadata side-channel fields.
        let ome = reader.ome_metadata().unwrap();
        let image = &ome.images[0];
        assert_eq!(image.name.as_deref(), Some("my gel"));
        assert_eq!(
            image.acquisition_date.as_deref(),
            Some("2007-07-25T14:00:00")
        );
        assert_eq!(image.physical_size_x, Some(10.5));
        assert_eq!(image.physical_size_y, Some(12.25));
        assert_eq!(image.instrument_ref, Some(0));
        assert_eq!(
            ome.instruments[0].microscope_model.as_deref(),
            Some("LAS-3000")
        );

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuji_detection_requires_companion() {
        let dir = fuji_tmp_dir("nocompanion");
        let img = dir.join("lonely.img");
        std::fs::write(&img, [0u8; 8]).unwrap();

        // No .inf companion => not a Fuji dataset (the .img extension is shared).
        let reader = FujiReader::new();
        assert!(!reader.is_this_type_by_name(&img));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuji_set_id_rejects_header_without_img_companion() {
        let dir = fuji_tmp_dir("missing_img");
        let inf = dir.join("gel.inf");
        std::fs::write(&inf, fuji_inf("missing img", "1.0", "1.0", 8, 1, 1)).unwrap();

        let mut reader = FujiReader::new();
        let err = reader.set_id(&inf).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message)
                if message.contains("companion .img")),
            "unexpected error: {err}"
        );
        assert_eq!(reader.series_count(), 0);
        assert!(matches!(
            reader.open_bytes(0),
            Err(BioFormatsError::NotInitialized)
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuji_maps_64_bit_samples_like_java_pixel_type_from_bytes() {
        let dir = fuji_tmp_dir("float64");
        let inf = dir.join("gel.inf");
        let img = dir.join("gel.img");

        std::fs::write(&inf, fuji_inf("float gel", "1.0", "1.0", 64, 1, 1)).unwrap();
        std::fs::write(&img, 42.0f64.to_le_bytes()).unwrap();

        let mut reader = FujiReader::new();
        reader.set_id(&inf).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.pixel_type, PixelType::Float64);
        assert_eq!(meta.bits_per_pixel, 64);
        assert_eq!(reader.open_bytes(0).unwrap(), 42.0f64.to_le_bytes());

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuji_filters_non_positive_physical_sizes_like_format_tools() {
        let dir = fuji_tmp_dir("badphys");
        let inf = dir.join("gel.inf");
        let img = dir.join("gel.img");

        let mut header = fuji_inf("bad phys", "0", "-2.5", 8, 1, 1);
        header.push_str("\r\n");
        std::fs::write(&inf, header).unwrap();
        std::fs::write(&img, [7u8]).unwrap();

        let mut reader = FujiReader::new();
        reader.set_id(&inf).unwrap();
        assert_eq!(reader.metadata().series_metadata.len(), 14);
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].physical_size_x, None);
        assert_eq!(ome.images[0].physical_size_y, None);

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuji_rejects_invalid_physical_size_like_double_parse() {
        let dir = fuji_tmp_dir("invalidphys");
        let inf = dir.join("gel.inf");
        let img = dir.join("gel.img");

        std::fs::write(
            &inf,
            fuji_inf("invalid phys", "not-a-number", "1.0", 8, 1, 1),
        )
        .unwrap();
        std::fs::write(&img, [7u8]).unwrap();

        let mut reader = FujiReader::new();
        assert!(matches!(
            reader.set_id(&inf),
            Err(BioFormatsError::Format(message)) if message.contains("invalid physical size")
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuji_failed_second_set_id_clears_previous_dataset() {
        let dir = fuji_tmp_dir("failed_second");
        let inf = dir.join("gel.inf");
        let img = dir.join("gel.img");
        let bad = dir.join("bad.inf");

        std::fs::write(&inf, fuji_inf("my gel", "1.0", "1.0", 8, 1, 1)).unwrap();
        std::fs::write(&img, [7u8]).unwrap();
        std::fs::write(&bad, "not enough lines").unwrap();

        let mut reader = FujiReader::new();
        reader.set_id(&inf).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![7]);

        assert!(reader.set_id(&bad).is_err());
        assert_eq!(reader.series_count(), 0);
        assert!(reader.set_series(0).is_err());
        assert!(matches!(
            reader.open_bytes(0),
            Err(BioFormatsError::NotInitialized)
        ));

        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn fuji_direct_inf_path_reads_nonzero_region() {
        let dir = fuji_tmp_dir("direct_inf_region");
        let inf = dir.join("gel.inf");
        let img = dir.join("gel.img");

        std::fs::write(&inf, fuji_inf("direct inf", "2.0", "3.0", 8, 3, 2)).unwrap();
        std::fs::write(&img, [1u8, 2, 3, 4, 5, 6]).unwrap();

        let mut reader = FujiReader::new();
        reader.set_id(&inf).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(reader.open_bytes_region(0, 1, 1, 2, 1).unwrap(), vec![5, 6]);
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].physical_size_x, Some(2.0));
        assert_eq!(ome.images[0].physical_size_y, Some(3.0));

        std::fs::remove_dir_all(&dir).ok();
    }
}
