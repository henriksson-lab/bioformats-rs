//! AFM/STM format readers.
//!
//! - TopoMetrix AFM (.tfr, .ffr, .zfr, .zfp, .2fl): BINARY header + binary
//!   UINT16 pixel data. The header layout is ported from the Java
//!   `TopometrixReader.initFile`: a fixed-offset binary structure where the
//!   version and pixel offset are stored as 4-byte ASCII numeric fields, the
//!   acquisition date is a newline-terminated line, followed by a 240-byte
//!   comment region (measured from the post-date file pointer) and a
//!   fixed-offset block holding the image dimensions.
//! - Unisoku STM/AFM (.hdr + .dat): text header with companion binary

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ── TopoMetrix Reader ─────────────────────────────────────────────────────────

pub struct TopometrixReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
    /// OME `Image/Description` — the header comment (Java L203).
    comment: Option<String>,
    /// OME `Image/AcquisitionDate`, ISO-8601, or `None` when the raw header
    /// date did not match either expected pattern (Java L185-189).
    acquisition_date: Option<String>,
    /// OME `PhysicalSizeX` in micrometres: `xSize / sizeX` (Java L192-199).
    physical_size_x: Option<f64>,
    /// OME `PhysicalSizeY` in micrometres: `ySize / sizeY` (Java L194-201).
    physical_size_y: Option<f64>,
}

impl TopometrixReader {
    pub fn new() -> Self {
        TopometrixReader {
            path: None,
            meta: None,
            data_offset: 0,
            comment: None,
            acquisition_date: None,
            physical_size_x: None,
            physical_size_y: None,
        }
    }
}

impl Default for TopometrixReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Mirror of `RandomAccessInputStream.readLine()`: read bytes from `data`
/// starting at `*pos` up to and including the first `\n` (0x0A). The returned
/// slice excludes the terminating `\n` (but may still include a trailing
/// `\r`); `*pos` is advanced past the `\n`. If no `\n` is found, reads to EOF.
fn read_line<'a>(data: &'a [u8], pos: &mut usize) -> &'a [u8] {
    let start = *pos;
    let mut end = start;
    while end < data.len() && data[end] != b'\n' {
        end += 1;
    }
    let line = &data[start..end];
    // Advance past the newline if present.
    *pos = if end < data.len() { end + 1 } else { end };
    line
}

/// Trim ASCII whitespace (matches Java `String.trim()`, which strips any
/// char <= 0x20 from both ends — including the trailing `\r` and embedded NUL
/// padding common in these binary headers).
fn java_trim(bytes: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = bytes.len();
    while start < end && bytes[start] <= b' ' {
        start += 1;
    }
    while end > start && bytes[end - 1] <= b' ' {
        end -= 1;
    }
    &bytes[start..end]
}

/// Read a little-endian `f32` from `data` at `*pos`, advancing `*pos` by 4.
/// Mirrors `RandomAccessInputStream.readFloat()`; errors past EOF where Java
/// would raise an `IOException`.
fn read_le_float(data: &[u8], pos: &mut usize) -> Result<f32> {
    let end = *pos + 4;
    if end > data.len() {
        return Err(BioFormatsError::UnsupportedFormat(
            "TopoMetrix header truncated reading a float field".into(),
        ));
    }
    let bytes = [data[*pos], data[*pos + 1], data[*pos + 2], data[*pos + 3]];
    *pos = end;
    Ok(f32::from_le_bytes(bytes))
}

/// Read a little-endian `f64` from `data` at `*pos`, advancing `*pos` by 8.
/// Mirrors `RandomAccessInputStream.readDouble()`; errors past EOF where Java
/// would raise an `IOException`.
fn read_le_double(data: &[u8], pos: &mut usize) -> Result<f64> {
    let end = *pos + 8;
    if end > data.len() {
        return Err(BioFormatsError::UnsupportedFormat(
            "TopoMetrix header truncated reading a double field".into(),
        ));
    }
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&data[*pos..end]);
    *pos = end;
    Ok(f64::from_le_bytes(bytes))
}

/// Port of `FormatTools.getPhysicalSize(value, MICROMETER)` for the common
/// case: a value of `0` (or non-finite) yields `None`; otherwise the value is
/// kept as micrometres (Java L192-195 wrap this around `xSize / sizeX`).
fn get_physical_size(value: f64) -> Option<f64> {
    if value != 0.0 && value.is_finite() {
        Some(value)
    } else {
        None
    }
}

/// Port of `DateTools.formatDate(date, ["MM/dd/yy HH:mm:ss",
/// "MM/dd/yyyy HH:mm:ss"])` (Java L185-186): produce an ISO-8601 timestamp, or
/// `None` when the raw string matches neither pattern.
fn format_topometrix_date(date: &str) -> Option<String> {
    let date = date.trim();
    let (d, t) = date.split_once(' ')?;
    let date_parts: Vec<&str> = d.split('/').collect();
    if date_parts.len() != 3 {
        return None;
    }
    let month: u32 = date_parts[0].parse().ok()?;
    let day: u32 = date_parts[1].parse().ok()?;
    // Accept both 2-digit ("MM/dd/yy") and 4-digit ("MM/dd/yyyy") years.
    let year_raw = date_parts[2];
    let year: i32 = match year_raw.len() {
        // Java SimpleDateFormat "yy" maps into the 100-year window beginning
        // 80 years before formatter creation. DateTools constructs the
        // formatter at parse time, so mirror that rolling window.
        2 => java_simple_date_format_two_digit_year(year_raw.parse::<i32>().ok()?),
        4 => year_raw.parse::<i32>().ok()?,
        _ => return None,
    };
    let time_parts: Vec<&str> = t.split(':').collect();
    if time_parts.len() != 3 {
        return None;
    }
    let hour: u32 = time_parts[0].parse().ok()?;
    let minute: u32 = time_parts[1].parse().ok()?;
    let second: u32 = time_parts[2].parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || hour > 23
        || minute > 59
        || second > 59
    {
        return None;
    }
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
    ))
}

fn java_simple_date_format_two_digit_year(two_digit_year: i32) -> i32 {
    let start = current_utc_year().unwrap_or(2026) - 80;
    let century = (start / 100) * 100;
    let mut year = century + two_digit_year;
    if year < start {
        year += 100;
    }
    year
}

fn current_utc_year() -> Option<i32> {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs();
    Some(gregorian_year_from_days((secs / 86_400) as i64))
}

fn gregorian_year_from_days(days_since_unix_epoch: i64) -> i32 {
    let z = days_since_unix_epoch + 719_468;
    let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let month = mp + if mp < 10 { 3 } else { -9 };
    (y + i64::from(month <= 2)) as i32
}

struct TopoMetrixHeader {
    meta: ImageMetadata,
    pixel_offset: u64,
    comment: Option<String>,
    acquisition_date: Option<String>,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
}

/// Parse the binary TopoMetrix header, faithfully ported from the Java
/// `TopometrixReader.initFile` (java-bioformats .../in/TopometrixReader.java,
/// lines 100-205).
///
/// Byte layout (all multi-byte integers little-endian):
///   [0..2)    skipped (2 bytes)                              (Java L109)
///   [2..6)    `version` as 4-byte ASCII, parsed as double    (Java L110)
///   [6..8)    skipped (2 bytes)                              (Java L111)
///   [8..12)   `pixelOffset` as 4-byte ASCII, parsed as long  (Java L112)
///   [12..14)  skipped (2 bytes)                              (Java L113)
///   fp = 14   (saved file pointer)                           (Java L115)
///   [14..)    `date`: a newline-terminated line              (Java L116)
///   comment:  `240 - fp_after_date + 14` bytes               (Java L117-118)
///             (so the comment region always ends at offset 14 + 240 = 254)
///   if version == 5: seek to absolute offset 452             (Java L120-122)
///   skipBytes(152)                                           (Java L124)
///     -> dims block starts at 254+152 = 406 (non-5),
///        or 452+152 = 604 (version 5)
///   sizeX = readShort (2 bytes)                              (Java L126)
///   skip 2 bytes                                             (Java L127)
///   sizeY = readShort (2 bytes)                              (Java L129)
///   metadata block: scaling fields, version-dependent layout (Java L130-173)
///   pixelType = UINT16, pixels read from `pixelOffset`.      (Java L175, L84)
fn parse_topometrix(path: &Path) -> Result<TopoMetrixHeader> {
    let content = std::fs::read(path).map_err(BioFormatsError::Io)?;

    // Need at least through the version/pixelOffset ASCII fields.
    if content.len() < 14 {
        return Err(BioFormatsError::UnsupportedFormat(
            "TopoMetrix file too short for header".into(),
        ));
    }

    // [2..6): version as 4-byte ASCII (Java L110).
    let version_str = std::str::from_utf8(&content[2..6])
        .map(|s| s.trim())
        .map_err(|_| {
            BioFormatsError::UnsupportedFormat("TopoMetrix version field not ASCII".into())
        })?;
    let version: i32 = version_str.parse::<f64>().map(|v| v as i32).map_err(|_| {
        BioFormatsError::UnsupportedFormat(format!(
            "TopoMetrix invalid version field {version_str:?}"
        ))
    })?;

    // [8..12): pixelOffset as 4-byte ASCII parsed as long (Java L112).
    let pixel_offset_str = std::str::from_utf8(&content[8..12])
        .map(|s| s.trim())
        .map_err(|_| {
            BioFormatsError::UnsupportedFormat("TopoMetrix pixelOffset field not ASCII".into())
        })?;
    let pixel_offset: i64 = pixel_offset_str.parse::<i64>().map_err(|_| {
        BioFormatsError::UnsupportedFormat(format!(
            "TopoMetrix invalid pixelOffset field {pixel_offset_str:?}"
        ))
    })?;
    if pixel_offset < 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "TopoMetrix negative pixelOffset".into(),
        ));
    }
    let pixel_offset = pixel_offset as u64;

    // fp = 14 after the three skipped 2-byte gaps and two 4-byte ASCII fields.
    let saved_fp: usize = 14;
    let mut pos = saved_fp;

    // date = readLine().trim() (Java L116).
    let date_line = read_line(&content, &mut pos);
    let date = String::from_utf8_lossy(java_trim(date_line)).into_owned();

    // commentLength = 240 - getFilePointer() + fp (Java L117).
    // `pos` is the file pointer after readLine (past the consumed '\n').
    let comment_length = 240i64 - pos as i64 + saved_fp as i64;
    if comment_length < 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "TopoMetrix date line longer than 240-byte comment region".into(),
        ));
    }
    let comment_length = comment_length as usize;
    let comment_start = pos;
    let comment_end = comment_start + comment_length;
    if comment_end > content.len() {
        return Err(BioFormatsError::UnsupportedFormat(
            "TopoMetrix header truncated within comment region".into(),
        ));
    }
    let comment =
        String::from_utf8_lossy(java_trim(&content[comment_start..comment_end])).into_owned();
    // After reading the comment, fp == 14 + 240 == 254.
    pos = comment_end;

    // version == 5 => seek(452) (Java L120-122).
    if version == 5 {
        pos = 452;
    }

    // skipBytes(152) (Java L124).
    pos += 152;

    // Dimensions block: sizeX (short), skip 2, sizeY (short) (Java L126-129).
    if pos + 6 > content.len() {
        return Err(BioFormatsError::UnsupportedFormat(
            "TopoMetrix header truncated before dimensions".into(),
        ));
    }
    let size_x = i16::from_le_bytes([content[pos], content[pos + 1]]);
    let size_y = i16::from_le_bytes([content[pos + 4], content[pos + 5]]);
    pos += 6; // sizeX (2) + skip (2) + sizeY (2), matching the Java reads.

    // Java does not validate the dimensions, but a non-positive size cannot
    // describe a real plane; reject it rather than producing a zero/garbage
    // image.
    if size_x <= 0 || size_y <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "TopoMetrix invalid dimensions {size_x}x{size_y}"
        )));
    }
    let width = size_x as u32;
    let height = size_y as u32;

    // Metadata-level block (Java L130-204). We always run it (the Rust readers
    // do not expose a MINIMUM metadata level); this reads the scaling fields
    // with a version-dependent layout and populates the global metadata table.
    // Java pre-initialises these to 0 (L130-131); every branch below overwrites
    // them, so we bind them per branch instead.
    let x_size: f64;
    let y_size: f64;
    let adc: f64;
    let dac_to_world_zero: f64;
    let mut metadata_block: Vec<(String, MetadataValue)> = Vec::new();

    // skipBytes(10) (Java L134).
    pos += 10;
    if version == 5 {
        // skipBytes(4) (Java L136).
        pos += 4;
        x_size = read_le_double(&content, &mut pos)?;
        // skipBytes(8) (Java L138).
        pos += 8;
        y_size = read_le_double(&content, &mut pos)?;
        adc = read_le_double(&content, &mut pos)?;
        dac_to_world_zero = read_le_double(&content, &mut pos)?;

        // skipBytes(1176) (Java L143).
        pos += 1176;

        let sample_volts = read_le_double(&content, &mut pos)?;
        let tunnel_current = read_le_double(&content, &mut pos)?;
        // skipBytes(16) (Java L147).
        pos += 16;
        let time_per_pixel = read_le_double(&content, &mut pos)?;
        // skipBytes(40) (Java L149).
        pos += 40;
        let scan_angle = read_le_double(&content, &mut pos)?;

        metadata_block.push(("Sample volts".into(), MetadataValue::Float(sample_volts)));
        metadata_block.push((
            "Tunnel current".into(),
            MetadataValue::Float(tunnel_current),
        ));
        metadata_block.push(("Scan rate".into(), MetadataValue::Float(time_per_pixel)));
        metadata_block.push(("Scan angle".into(), MetadataValue::Float(scan_angle)));
    } else {
        x_size = read_le_float(&content, &mut pos)? as f64;
        // skipBytes(4) (Java L159).
        pos += 4;
        y_size = read_le_float(&content, &mut pos)? as f64;
        adc = read_le_float(&content, &mut pos)? as f64;
        // skipBytes(764) (Java L162).
        pos += 764;
        dac_to_world_zero = read_le_float(&content, &mut pos)? as f64;
    }

    metadata_block.push(("Version".into(), MetadataValue::Int(version as i64)));
    metadata_block.push(("X size (in um)".into(), MetadataValue::Float(x_size)));
    metadata_block.push(("Y size (in um)".into(), MetadataValue::Float(y_size)));
    metadata_block.push(("ADC".into(), MetadataValue::Float(adc)));
    metadata_block.push((
        "DAC to world zero".into(),
        MetadataValue::Float(dac_to_world_zero),
    ));
    metadata_block.push(("Comment".into(), MetadataValue::String(comment.clone())));
    metadata_block.push((
        "Acquisition date".into(),
        MetadataValue::String(date.clone()),
    ));

    // Physical sizes: xSize/sizeX and ySize/sizeY in micrometres (Java L192-201),
    // filtered through FormatTools.getPhysicalSize (rejects value == 0).
    let physical_size_x = get_physical_size(x_size / width as f64);
    let physical_size_y = get_physical_size(y_size / height as f64);

    // Acquisition date parsed via DateTools.formatDate (Java L185-189).
    let acquisition_date = format_topometrix_date(&date);
    let description = if comment.is_empty() {
        None
    } else {
        Some(comment.clone())
    };

    // pixelType = UINT16 (Java L175); pixels read from pixelOffset (Java L84).
    let pixel_type = PixelType::Uint16;
    let bps = pixel_type.bytes_per_sample();
    let plane_bytes = (width as u64)
        .checked_mul(height as u64)
        .and_then(|v| v.checked_mul(bps as u64))
        .ok_or_else(|| BioFormatsError::Format("TopoMetrix plane size overflows".into()))?;
    if pixel_offset
        .checked_add(plane_bytes)
        .is_none_or(|end| end > content.len() as u64)
    {
        return Err(BioFormatsError::UnsupportedFormat(
            "TopoMetrix pixel payload is shorter than declared dimensions".into(),
        ));
    }

    // Global metadata table, mirroring the Java `addGlobalMeta` calls
    // (Java L152-172). Later inserts win, matching Java's LinkedHashMap
    // overwrite semantics for duplicate keys.
    let mut series_metadata = HashMap::new();
    for (k, v) in metadata_block {
        series_metadata.insert(k, v);
    }

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type,
        bits_per_pixel: (bps * 8) as u8,
        image_count: 1,
        dimension_order: DimensionOrder::XYZCT,
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

    Ok(TopoMetrixHeader {
        meta,
        pixel_offset,
        comment: description,
        acquisition_date,
        physical_size_x,
        physical_size_y,
    })
}

fn kv_value<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    // Accept "key=value" or "key = value"
    let stripped = line.strip_prefix(key)?;
    let stripped = stripped.trim_start();
    let val = stripped.strip_prefix('=')?.trim_start();
    Some(val)
}

impl FormatReader for TopometrixReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        // Java constructor registers: tfr, ffr, zfr, zfp, 2fl.
        matches!(
            ext.as_deref(),
            Some("tfr") | Some("ffr") | Some("zfr") | Some("zfp") | Some("2fl")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Mirror Java `isThisType`: require at least 6 bytes and a "#R" prefix
        // (Java reads a 6-char string and checks `startsWith("#R")`).
        //
        // NOTE: the Java method has a subtle bug — every code path falls
        // through to `return false`, so the JVM reader's magic check never
        // actually succeeds and detection relies on the extension. We diverge
        // intentionally and honor the documented intent (the "#R" magic),
        // which is what makes content-based detection useful here.
        header.len() >= 6 && header.starts_with(b"#R")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let header = parse_topometrix(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(header.meta);
        self.data_offset = header.pixel_offset;
        self.comment = header.comment;
        self.acquisition_date = header.acquisition_date;
        self.physical_size_x = header.physical_size_x;
        self.physical_size_y = header.physical_size_y;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
        self.comment = None;
        self.acquisition_date = None;
        self.physical_size_x = None;
        self.physical_size_y = None;
        Ok(())
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = ome.images.get_mut(0)?;
        // Java sets ImageDescription = comment (L203), AcquisitionDate (L185-189)
        // and PhysicalSizeX/Y in micrometres (L197-202).
        if let Some(comment) = &self.comment {
            img.description = Some(comment.clone());
        }
        if let Some(date) = &self.acquisition_date {
            img.acquisition_date = Some(date.clone());
        }
        img.physical_size_x = self.physical_size_x;
        img.physical_size_y = self.physical_size_y;
        Some(ome)
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
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.data_offset))
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
        crop_full_plane("TopoMetrix", &full, meta, 1, x, y, w, h)
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

// ── Unisoku Reader ─────────────────────────────────────────────────────────────

pub struct UnisokuReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    dat_path: Option<PathBuf>,
    image_name: Option<String>,
    description: Option<String>,
    acquisition_date: Option<String>,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
}

impl UnisokuReader {
    pub fn new() -> Self {
        UnisokuReader {
            path: None,
            meta: None,
            dat_path: None,
            image_name: None,
            description: None,
            acquisition_date: None,
            physical_size_x: None,
            physical_size_y: None,
        }
    }
}

impl Default for UnisokuReader {
    fn default() -> Self {
        Self::new()
    }
}

fn resolve_unisoku_header_path(path: &Path) -> PathBuf {
    let is_dat = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("dat"))
        .unwrap_or(false);
    if !is_dat {
        return path.to_path_buf();
    }

    let upper = path.with_extension("HDR");
    if upper.exists() {
        return upper;
    }
    let lower = path.with_extension("hdr");
    if lower.exists() {
        return lower;
    }
    upper
}

fn resolve_unisoku_dat_path(header: &Path) -> PathBuf {
    let upper = header.with_extension("DAT");
    if upper.exists() {
        return upper;
    }
    let lower = header.with_extension("dat");
    if lower.exists() {
        return lower;
    }
    upper
}

fn unisoku_pixel_type_from_ascii_data_type(data_type: i32) -> Option<PixelType> {
    let signed = data_type % 2 == 1;
    let bytes = data_type / 2;
    match (bytes, signed) {
        (1, false) => Some(PixelType::Uint8),
        (1, true) => Some(PixelType::Int8),
        (2, false) => Some(PixelType::Uint16),
        (2, true) => Some(PixelType::Int16),
        (4, _) => Some(PixelType::Float32),
        _ => None,
    }
}

struct UnisokuHeader {
    meta: ImageMetadata,
    dat_path: PathBuf,
    image_name: Option<String>,
    description: Option<String>,
    acquisition_date: Option<String>,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
}

fn unisoku_physical_size(value: f64, unit: &str) -> Option<f64> {
    if !value.is_finite() || value <= 0.0 {
        return None;
    }

    // `OmeMetadata` stores physical sizes in micrometres. Java preserves the
    // unit object; converting here keeps the same physical length.
    let scale_to_um = match unit.trim().to_ascii_lowercase().as_str() {
        "um" | "µm" | "micron" | "microns" | "micrometer" | "micrometers" | "micrometre"
        | "micrometres" => 1.0,
        "nm" | "nanometer" | "nanometers" | "nanometre" | "nanometres" => 0.001,
        "mm" | "millimeter" | "millimeters" | "millimetre" | "millimetres" => 1000.0,
        "m" | "meter" | "meters" | "metre" | "metres" => 1_000_000.0,
        _ => return None,
    };
    Some(value * scale_to_um)
}

fn unisoku_axis_physical_size(tokens: &[&str], size: u32) -> Option<f64> {
    if tokens.len() < 3 || size == 0 {
        return None;
    }
    let start = tokens[1].parse::<f64>().ok()?;
    let end = tokens[2].parse::<f64>().ok()?;
    unisoku_physical_size((end - start) / size as f64, tokens[0])
}

fn parse_unisoku_hdr(path: &Path) -> Result<UnisokuHeader> {
    let header_path = resolve_unisoku_header_path(path);
    let content = std::fs::read_to_string(&header_path).map_err(BioFormatsError::Io)?;

    let mut width: Option<u32> = None;
    let mut height: Option<u32> = None;
    let mut bits: Option<u32> = None;
    let mut pixel_type: Option<PixelType> = None;
    let mut image_name: Option<String> = None;
    let mut description: Option<String> = None;
    let mut acquisition_date: Option<String> = None;
    let mut x_axis_tokens: Option<Vec<String>> = None;
    let mut y_axis_tokens: Option<Vec<String>> = None;
    let mut series_metadata = HashMap::new();

    if content.contains(":STM data") {
        let lines: Vec<&str> = content.split('\r').collect();
        let mut i = 0usize;
        while i < lines.len() {
            let key = lines[i].trim();
            i += 1;
            if !key.starts_with(':') {
                continue;
            }

            let mut values = Vec::new();
            while i < lines.len() {
                let value = lines[i].trim();
                if value.starts_with(':') {
                    break;
                }
                if !value.is_empty() {
                    values.push(value);
                }
                i += 1;
            }

            let value = values.join(" ");
            series_metadata.insert(key.to_string(), MetadataValue::String(value.clone()));
            let tokens: Vec<&str> = value.split_whitespace().collect();

            if key == ":data volume(x*y)" && tokens.len() >= 2 {
                width = tokens[0].parse::<u32>().ok();
                height = tokens[1].parse::<u32>().ok();
            } else if key == ":date; time" {
                acquisition_date = format_topometrix_date(&value);
            } else if key.starts_with(":ascii flag; data type") {
                let type_token = tokens
                    .last()
                    .ok_or_else(|| {
                        BioFormatsError::UnsupportedFormat(
                            "Unisoku header missing ASCII data type".into(),
                        )
                    })?
                    .parse::<i32>()
                    .map_err(|_| {
                        BioFormatsError::UnsupportedFormat(
                            "Unisoku header has invalid ASCII data type".into(),
                        )
                    })?;
                pixel_type = unisoku_pixel_type_from_ascii_data_type(type_token);
                if pixel_type.is_none() {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Unisoku unsupported ASCII data type {type_token}"
                    )));
                }
            } else if key == ":sample name" {
                if !value.is_empty() {
                    image_name = Some(value);
                }
            } else if key == ":remark" {
                if !value.is_empty() {
                    description = Some(value);
                }
            } else if key.starts_with(":x_data ->") {
                x_axis_tokens = Some(tokens.iter().map(|s| (*s).to_string()).collect());
            } else if key.starts_with(":y_data ->") {
                y_axis_tokens = Some(tokens.iter().map(|s| (*s).to_string()).collect());
            }
        }
    } else {
        for line in content.lines() {
            let line = line.trim();
            if let Some(val) = kv_value(line, "XSIZE") {
                if let Ok(v) = val.parse::<u32>() {
                    width = Some(v);
                }
            } else if let Some(val) = kv_value(line, "YSIZE") {
                if let Ok(v) = val.parse::<u32>() {
                    height = Some(v);
                }
            } else if let Some(val) = kv_value(line, "BIT") {
                if let Ok(v) = val.parse::<u32>() {
                    bits = Some(v);
                }
            }
        }
    }

    let width = width
        .filter(|&v| v > 0)
        .ok_or_else(|| BioFormatsError::UnsupportedFormat("Unisoku header missing XSIZE".into()))?;
    let height = height
        .filter(|&v| v > 0)
        .ok_or_else(|| BioFormatsError::UnsupportedFormat("Unisoku header missing YSIZE".into()))?;
    let pixel_type = match pixel_type {
        Some(pixel_type) => pixel_type,
        None => {
            let bits = bits.ok_or_else(|| {
                BioFormatsError::UnsupportedFormat("Unisoku header missing BIT depth".into())
            })?;
            if bits == 0 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "Unisoku header has invalid BIT depth".into(),
                ));
            } else if bits <= 16 {
                PixelType::Int16
            } else {
                PixelType::Int32
            }
        }
    };
    let bps = pixel_type.bytes_per_sample();
    let physical_size_x = x_axis_tokens.as_deref().and_then(|tokens| {
        let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
        unisoku_axis_physical_size(&refs, width)
    });
    let physical_size_y = y_axis_tokens.as_deref().and_then(|tokens| {
        let refs: Vec<&str> = tokens.iter().map(String::as_str).collect();
        unisoku_axis_physical_size(&refs, height)
    });

    let dat_path = resolve_unisoku_dat_path(&header_path);
    let plane_bytes = (width as u64)
        .checked_mul(height as u64)
        .and_then(|v| v.checked_mul(bps as u64))
        .ok_or_else(|| BioFormatsError::Format("Unisoku plane size overflows".into()))?;
    let dat_len = std::fs::metadata(&dat_path)
        .map_err(BioFormatsError::Io)?
        .len();
    if dat_len < plane_bytes {
        return Err(BioFormatsError::UnsupportedFormat(
            "Unisoku .dat payload is shorter than declared dimensions".into(),
        ));
    }

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type,
        bits_per_pixel: (bps * 8) as u8,
        image_count: 1,
        dimension_order: DimensionOrder::XYZCT,
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

    Ok(UnisokuHeader {
        meta,
        dat_path,
        image_name,
        description,
        acquisition_date,
        physical_size_x,
        physical_size_y,
    })
}

impl FormatReader for UnisokuReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let Some(ext) = path.extension().and_then(|e| e.to_str()) else {
            return false;
        };
        let header_path = if ext.eq_ignore_ascii_case("hdr") {
            path.to_path_buf()
        } else if ext.eq_ignore_ascii_case("dat") {
            resolve_unisoku_header_path(path)
        } else {
            return false;
        };
        std::fs::read(&header_path)
            .map(|header| self.is_this_type_by_bytes(&header))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= b":STM data".len() && &header[..b":STM data".len()] == b":STM data"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let header_path = resolve_unisoku_header_path(path);
        let header = parse_unisoku_hdr(path)?;
        self.path = Some(header_path);
        self.meta = Some(header.meta);
        self.dat_path = Some(header.dat_path);
        self.image_name = header.image_name;
        self.description = header.description;
        self.acquisition_date = header.acquisition_date;
        self.physical_size_x = header.physical_size_x;
        self.physical_size_y = header.physical_size_y;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.dat_path = None;
        self.image_name = None;
        self.description = None;
        self.acquisition_date = None;
        self.physical_size_x = None;
        self.physical_size_y = None;
        Ok(())
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = ome.images.get_mut(0)?;

        img.name = self.image_name.clone();
        img.description = self.description.clone();
        img.acquisition_date = self.acquisition_date.clone();
        img.physical_size_x = self.physical_size_x;
        img.physical_size_y = self.physical_size_y;
        Some(ome)
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
        let dat = self
            .dat_path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(dat).map_err(BioFormatsError::Io)?;
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
        crop_full_plane("Unisoku", &full, meta, 1, x, y, w, h)
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

// ── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// Build a minimal BINARY TopoMetrix fixture (version != 5) with the exact
    /// byte layout mirrored from `parse_topometrix`.
    ///
    /// Layout (offsets in bytes, all integers little-endian):
    ///   [0..2)    skipped padding
    ///   [2..6)    version ASCII "1.0 "          -> version == 1
    ///   [6..8)    skipped padding
    ///   [8..12)   pixelOffset ASCII             -> where pixels start
    ///   [12..14)  skipped padding
    ///   [14..)    date line, newline-terminated
    ///   comment region: ends at offset 254
    ///   skip 152                                -> dims block at 406
    ///   [406..408)  sizeX (i16 LE)
    ///   [408..410)  skipped
    ///   [410..412)  sizeY (i16 LE)
    ///   metadata block (skip 10, xSize/skip/ySize/adc floats, skip 764, dac)
    ///   [pixel_offset..)  width*height UINT16 LE pixels
    fn build_fixture(size_x: i16, size_y: i16, pixel_offset: u32, pixels: &[u16]) -> Vec<u8> {
        // dims at 406; metadata block runs:
        //   406+6 = 412 (after sizeX/skip/sizeY)
        //   +10 skip            -> 422
        //   +4 xSize float      -> 426
        //   +4 skip             -> 430
        //   +4 ySize float      -> 434
        //   +4 adc float        -> 438
        //   +764 skip           -> 1202
        //   +4 dacToWorldZero   -> 1206
        let header_len = 1206usize;
        let plane_bytes = (size_x as usize) * (size_y as usize) * 2;
        let total = (pixel_offset as usize + plane_bytes).max(header_len);
        let mut buf = vec![0u8; total];

        // version "1.0 " at [2..6)
        buf[2..6].copy_from_slice(b"1.0 ");
        // pixelOffset ASCII at [8..12); pad to 4 chars with trailing spaces.
        let off_str = format!("{:<4}", pixel_offset);
        assert_eq!(off_str.len(), 4, "pixelOffset must fit in 4 ASCII chars");
        buf[8..12].copy_from_slice(off_str.as_bytes());

        // date line at offset 14, terminated by '\n'.
        let date = b"05/29/26 12:00:00\n";
        buf[14..14 + date.len()].copy_from_slice(date);
        // Comment region left as NUL padding (trims to empty).

        // sizeX / sizeY in the dims block.
        buf[406..408].copy_from_slice(&size_x.to_le_bytes());
        buf[410..412].copy_from_slice(&size_y.to_le_bytes());

        // metadata block: xSize at 422, ySize at 430, adc at 434.
        buf[422..426].copy_from_slice(&10.0f32.to_le_bytes());
        buf[430..434].copy_from_slice(&20.0f32.to_le_bytes());

        // pixel data
        let mut p = pixel_offset as usize;
        for &px in pixels {
            buf[p..p + 2].copy_from_slice(&px.to_le_bytes());
            p += 2;
        }
        buf
    }

    fn write_temp(name: &str, bytes: &[u8]) -> PathBuf {
        let mut path = std::env::temp_dir();
        path.push(format!("topometrix_{}_{}.tfr", std::process::id(), name));
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(bytes).unwrap();
        f.flush().unwrap();
        path
    }

    #[test]
    fn topometrix_reads_binary_fixture() {
        // 2x2 plane = 4 UINT16 pixels, placed after the metadata block.
        let pixels: [u16; 4] = [0x0102, 0x0304, 0x0506, 0x0708];
        let bytes = build_fixture(2, 2, 1206, &pixels);
        let path = write_temp("ok", &bytes);

        let mut reader = TopometrixReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert_eq!(meta.size_z, 1);
        assert_eq!(meta.size_c, 1);
        assert_eq!(meta.size_t, 1);
        assert_eq!(meta.image_count, 1);
        assert!(meta.is_little_endian);

        // Metadata table fields ported from Java's addGlobalMeta calls.
        // MetadataValue has no PartialEq, so match the variant payload directly.
        assert!(matches!(
            meta.series_metadata.get("Version"),
            Some(MetadataValue::Int(1))
        ));
        assert!(
            matches!(meta.series_metadata.get("X size (in um)"), Some(MetadataValue::Float(v)) if *v == 10.0)
        );
        assert!(
            matches!(meta.series_metadata.get("Y size (in um)"), Some(MetadataValue::Float(v)) if *v == 20.0)
        );
        assert!(meta.series_metadata.contains_key("ADC"));
        assert!(meta.series_metadata.contains_key("DAC to world zero"));

        // OME metadata: physical sizes (xSize/sizeX, ySize/sizeY) + date.
        let ome = reader.ome_metadata().unwrap();
        let img = &ome.images[0];
        assert_eq!(img.physical_size_x, Some(5.0)); // 10 / 2
        assert_eq!(img.physical_size_y, Some(10.0)); // 20 / 2
        assert_eq!(img.acquisition_date.as_deref(), Some("2026-05-29T12:00:00"));

        let plane = reader.open_bytes(0).unwrap();
        let expected: Vec<u8> = pixels.iter().flat_map(|p| p.to_le_bytes()).collect();
        assert_eq!(plane, expected);

        // Sanity-check the recorded pixel offset.
        assert_eq!(reader.data_offset, 1206);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn topometrix_rejects_truncated_pixels() {
        // Declare a 4x4 plane (32 bytes) but only provide pixels for part of it.
        let mut bytes = build_fixture(2, 2, 1206, &[1, 2, 3, 4]);
        // Rewrite the declared dimensions to 4x4 without enlarging the payload.
        bytes[406..408].copy_from_slice(&4i16.to_le_bytes());
        bytes[410..412].copy_from_slice(&4i16.to_le_bytes());
        let path = write_temp("trunc", &bytes);

        let mut reader = TopometrixReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(matches!(err, BioFormatsError::UnsupportedFormat(_)));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn topometrix_rejects_short_file() {
        let path = write_temp("short", &[0u8; 8]);
        let mut reader = TopometrixReader::new();
        assert!(reader.set_id(&path).is_err());
        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn topometrix_date_parsing() {
        assert_eq!(
            format_topometrix_date("05/29/26 12:00:00"),
            Some("2026-05-29T12:00:00".to_string())
        );
        assert_eq!(
            format_topometrix_date("12/31/99 08:09:10"),
            Some("1999-12-31T08:09:10".to_string())
        );
        assert_eq!(
            format_topometrix_date("01/01/45 08:09:10"),
            Some("2045-01-01T08:09:10".to_string())
        );
        assert_eq!(
            format_topometrix_date("12/31/2015 08:09:10"),
            Some("2015-12-31T08:09:10".to_string())
        );
        assert_eq!(format_topometrix_date("garbage"), None);
        assert_eq!(format_topometrix_date(""), None);
    }

    #[test]
    fn is_this_type_by_bytes_matches_magic() {
        let reader = TopometrixReader::new();
        assert!(reader.is_this_type_by_bytes(b"#R1.0 stuff"));
        assert!(!reader.is_this_type_by_bytes(b"#X1.0 "));
        assert!(!reader.is_this_type_by_bytes(b"#R")); // too short (<6)
        assert!(!reader.is_this_type_by_bytes(b""));
    }

    fn write_unisoku_pair(name: &str, hdr: &str, dat: &[u8]) -> (PathBuf, PathBuf) {
        let mut hdr_path = std::env::temp_dir();
        hdr_path.push(format!("unisoku_{}_{}.HDR", std::process::id(), name));
        let dat_path = hdr_path.with_extension("DAT");
        std::fs::write(&hdr_path, hdr.as_bytes()).unwrap();
        std::fs::write(&dat_path, dat).unwrap();
        (hdr_path, dat_path)
    }

    #[test]
    fn unisoku_projects_java_header_metadata_to_ome() {
        let hdr = concat!(
            ":STM data\r",
            ":data volume(x*y)\r",
            "2 2\r",
            ":ascii flag; data type\r",
            "0 4\r",
            ":sample name\r",
            "Calibration sample\r",
            ":remark\r",
            "fine scan\r",
            ":date; time\r",
            "05/29/26 12:00:00\r",
            ":x_data -> range\r",
            "nm 0 200\r",
            ":y_data -> range\r",
            "um 10 14\r",
        );
        let pixels = [1u16, 2, 3, 4];
        let dat: Vec<u8> = pixels.iter().flat_map(|p| p.to_le_bytes()).collect();
        let (hdr_path, dat_path) = write_unisoku_pair("metadata", hdr, &dat);

        let mut reader = UnisokuReader::new();
        reader.set_id(&hdr_path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_x, 2);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert!(matches!(
            meta.series_metadata.get(":sample name"),
            Some(MetadataValue::String(v)) if v == "Calibration sample"
        ));

        let ome = reader.ome_metadata().unwrap();
        let img = &ome.images[0];
        assert_eq!(img.name.as_deref(), Some("Calibration sample"));
        assert_eq!(img.description.as_deref(), Some("fine scan"));
        assert_eq!(img.acquisition_date.as_deref(), Some("2026-05-29T12:00:00"));
        assert_eq!(img.physical_size_x, Some(0.1)); // (200 nm / 2) -> 0.1 um
        assert_eq!(img.physical_size_y, Some(2.0)); // (14 - 10) um / 2
        assert_eq!(reader.open_bytes(0).unwrap(), dat);

        std::fs::remove_file(&hdr_path).ok();
        std::fs::remove_file(&dat_path).ok();
    }

    #[test]
    fn unisoku_magic_check_matches_first_nine_bytes() {
        let reader = UnisokuReader::new();
        assert!(reader.is_this_type_by_bytes(b":STM data"));
        assert!(reader.is_this_type_by_bytes(b":STM data trailing"));
        assert!(!reader.is_this_type_by_bytes(b"xx:STM data"));
        assert!(!reader.is_this_type_by_bytes(b":STM dat"));
    }
}
