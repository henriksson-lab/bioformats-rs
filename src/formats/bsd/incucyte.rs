// SPDX-License-Identifier: BSD-2-Clause
//! Incucyte S3 raw TIFFs and `.icarch` archive markers.
//!
//! Independently implemented from the supplied files (no Java reader exists).
//! Vendor XML lives in tag 60020 of a metadata IFD referenced by tag 60000.
//! Its 1x1 payload is not an image. Main-chain reduced images are resolutions.
//! Archive discovery groups native fluorescence, phase and auxiliary Seed
//! streams separately so bit depths and stored pyramids remain unchanged.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs::{self, File};
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use quick_xml::{events::Event, Reader};

use crate::common::endian::{read_u16, read_u32, read_u64};
use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ome_metadata::{
    create_lsid, OmeChannel, OmeDetector, OmeImage, OmeInstrument, OmeMetadata, OmeObjective,
    OmePixels, OmePlane, OmePlate, OmeWell, OmeWellSample,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::{uninitialized_metadata, FormatReader};
use crate::tiff::ifd::{tag, Ifd};
use crate::tiff::parser::{TiffParser, TiffVariant};
use crate::tiff::TiffReader;

const POINTER: u16 = 60000;
const XML: u16 = 60020;
const MAX_XML: u64 = 4 << 20;
const MAX_IFDS: usize = 1024;
const MAX_FILES: usize = 1_000_000;

fn invalid(message: impl Into<String>) -> BioFormatsError {
    BioFormatsError::Format(format!("Incucyte: {}", message.into()))
}

/// Read just the vendor ASCII tag without generic TIFF's lossy UTF-8 decoding.
/// Offsets/counts are bounded before allocation; the rest of TIFF ASCII parsing
/// intentionally keeps its existing behavior.
fn raw_xml<R: Read + Seek>(parser: &mut TiffParser<R>, offset: u64) -> Result<Option<Vec<u8>>> {
    let big = parser.variant == TiffVariant::Big;
    let le = parser.little_endian;
    let len = parser.reader.seek(SeekFrom::End(0))?;
    parser.reader.seek(SeekFrom::Start(offset))?;
    let count = if big {
        read_u64(&mut parser.reader, le)?
    } else {
        read_u16(&mut parser.reader, le)? as u64
    };
    let size = if big { 20 } else { 12 };
    let end = offset
        .checked_add(if big { 8 } else { 2 })
        .and_then(|v| count.checked_mul(size).and_then(|n| v.checked_add(n)))
        .and_then(|v| v.checked_add(if big { 8 } else { 4 }))
        .ok_or_else(|| invalid("IFD table overflow"))?;
    if end > len {
        return Err(invalid("IFD table outside file"));
    }
    for _ in 0..count {
        let code = read_u16(&mut parser.reader, le)?;
        let typ = read_u16(&mut parser.reader, le)?;
        let n = if big {
            read_u64(&mut parser.reader, le)?
        } else {
            read_u32(&mut parser.reader, le)? as u64
        };
        let inline = parser.reader.stream_position()?;
        let value = if big {
            read_u64(&mut parser.reader, le)?
        } else {
            read_u32(&mut parser.reader, le)? as u64
        };
        if code != XML {
            continue;
        }
        if typ != 2 || n == 0 || n > MAX_XML {
            return Err(invalid("invalid or oversized XML tag"));
        }
        let start = if n <= if big { 8 } else { 4 } {
            inline
        } else {
            value
        };
        if start.checked_add(n).filter(|&v| v <= len).is_none() {
            return Err(invalid("XML outside file"));
        }
        parser.reader.seek(SeekFrom::Start(start))?;
        let mut data = vec![0; n as usize];
        parser.reader.read_exact(&mut data)?;
        while data.last() == Some(&0) {
            data.pop();
        }
        return Ok(Some(data));
    }
    Ok(None)
}

fn decode_xml(bytes: &[u8]) -> String {
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_owned(),
        Err(_) => encoding_rs::WINDOWS_1252
            .decode_without_bom_handling(bytes)
            .0
            .into_owned(),
    }
}

fn xml_fields(xml: &str) -> Result<BTreeMap<String, String>> {
    let mut reader = Reader::from_str(xml);
    let mut stack = Vec::<String>::new();
    let mut fields = BTreeMap::<String, String>::new();
    let mut roots = 0;
    loop {
        match reader
            .read_event()
            .map_err(|e| invalid(format!("invalid XML: {e}")))?
        {
            Event::Start(e) => {
                if stack.is_empty() {
                    roots += 1;
                    if e.name().as_ref() != b"ImageInfo" || roots != 1 {
                        return Err(invalid("expected ImageInfo XML root"));
                    }
                }
                if stack.len() >= 64 {
                    return Err(invalid("XML nesting limit"));
                }
                stack.push(String::from_utf8_lossy(e.name().as_ref()).into_owned());
            }
            Event::Text(e) => {
                let text = e.decode().map_err(|e| invalid(e.to_string()))?;
                let text =
                    quick_xml::escape::unescape(&text).map_err(|e| invalid(e.to_string()))?;
                if !text.trim().is_empty() {
                    fields.entry(stack.join("/")).or_default().push_str(&text);
                }
            }
            Event::CData(e) => {
                fields
                    .entry(stack.join("/"))
                    .or_default()
                    .push_str(&e.decode().map_err(|e| invalid(e.to_string()))?);
            }
            Event::GeneralRef(e) => {
                let name = e.decode().map_err(|e| invalid(e.to_string()))?;
                let reference = format!("&{name};");
                let value =
                    quick_xml::escape::unescape(&reference).map_err(|e| invalid(e.to_string()))?;
                fields.entry(stack.join("/")).or_default().push_str(&value);
            }
            Event::End(_) => {
                stack.pop();
            }
            Event::DocType(_) => return Err(invalid("XML document types are unsupported")),
            Event::Eof => break,
            _ => {}
        }
    }
    if roots != 1 || !stack.is_empty() || !fields.contains_key("ImageInfo/IncuCyteMetadataVersion")
    {
        return Err(invalid("missing IncuCyteMetadataVersion"));
    }
    for value in fields.values_mut() {
        *value = value.trim().to_owned();
    }
    Ok(fields)
}

fn number(s: &str) -> Option<f64> {
    let n: f64 = s.split_whitespace().next()?.parse().ok()?;
    n.is_finite().then_some(n)
}

fn length(s: &str) -> Option<f64> {
    let mut parts = s.split_whitespace();
    let n = number(parts.next()?)?;
    let value = match parts.next()? {
        "µm" | "μm" | "um" => n,
        "mm" => n * 1000.0,
        "nm" => n / 1000.0,
        _ => return None,
    };
    (parts.next().is_none() && value.is_finite()).then_some(value)
}

fn validate_archive_marker(text: &str) -> Result<()> {
    let mut reader = Reader::from_str(text.trim_start_matches('\u{feff}'));
    let mut depth = 0usize;
    let mut roots = 0usize;
    loop {
        match reader
            .read_event()
            .map_err(|e| invalid(format!("invalid archive marker: {e}")))?
        {
            Event::Start(e) => {
                if depth != 0 || e.name().as_ref() != b"incuCyteArchiveHeaderFile" {
                    return Err(invalid("invalid archive marker"));
                }
                roots += 1;
                depth = 1;
            }
            Event::Empty(e) => {
                if depth != 0 || e.name().as_ref() != b"incuCyteArchiveHeaderFile" {
                    return Err(invalid("invalid archive marker"));
                }
                roots += 1;
            }
            Event::End(e) => {
                if depth != 1 || e.name().as_ref() != b"incuCyteArchiveHeaderFile" {
                    return Err(invalid("invalid archive marker"));
                }
                depth = 0;
            }
            Event::DocType(_) => return Err(invalid("invalid archive marker document type")),
            Event::Text(e)
                if !e
                    .decode()
                    .map_err(|e| invalid(e.to_string()))?
                    .trim()
                    .is_empty() =>
            {
                return Err(invalid("invalid archive marker text"))
            }
            Event::CData(_) | Event::GeneralRef(_) => {
                return Err(invalid("invalid archive marker content"))
            }
            Event::Eof => break,
            _ => {}
        }
    }
    if roots != 1 || depth != 0 {
        return Err(invalid("invalid archive marker"));
    }
    Ok(())
}

fn position(s: &str) -> Option<(f64, f64)> {
    let (xy, unit) = s.trim().strip_prefix('(')?.split_once(')')?;
    let (x, y) = xy.split_once(',')?;
    Some((
        length(&format!("{} {}", x.trim(), unit.trim()))?,
        length(&format!("{} {}", y.trim(), unit.trim()))?,
    ))
}

/// Parse local US timestamps without assigning the host timezone or inventing Z.
/// The Gregorian day number is only used to subtract local acquisition times.
fn timestamp(s: &str) -> Option<(i64, String)> {
    let mut words = s.split_whitespace();
    let date: Vec<i64> = words
        .next()?
        .split('/')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    let time: Vec<i64> = words
        .next()?
        .split(':')
        .map(str::parse)
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    if date.len() != 3 || !(2..=3).contains(&time.len()) {
        return None;
    }
    let (month, day, year) = (date[0], date[1], date[2]);
    let (mut hour, minute, second) = (time[0], time[1], *time.get(2).unwrap_or(&0));
    if let Some(ampm) = words.next() {
        if !(1..=12).contains(&hour) {
            return None;
        }
        hour %= 12;
        match ampm {
            "PM" => hour += 12,
            "AM" => {}
            _ => return None,
        }
    }
    if words.next().is_some()
        || !(1..=9999).contains(&year)
        || !(1..=12).contains(&month)
        || !(0..24).contains(&hour)
        || !(0..60).contains(&minute)
        || !(0..60).contains(&second)
    {
        return None;
    }
    let leap = year % 4 == 0 && (year % 100 != 0 || year % 400 == 0);
    let days = [
        31,
        if leap { 29 } else { 28 },
        31,
        30,
        31,
        30,
        31,
        31,
        30,
        31,
        30,
        31,
    ];
    if day < 1 || day > days[(month - 1) as usize] {
        return None;
    }
    let y = year - i64::from(month <= 2);
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let days = era * 146097 + yoe * 365 + yoe / 4 - yoe / 100 + doy;
    Some((
        days * 86400 + hour * 3600 + minute * 60 + second,
        format!("{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"),
    ))
}

fn well(s: &str) -> Option<(u32, u32)> {
    let n = s.bytes().take_while(u8::is_ascii_alphabetic).count();
    if n == 0 {
        return None;
    }
    let mut row = 0u32;
    for b in s[..n].bytes() {
        row = row
            .checked_mul(26)?
            .checked_add((b.to_ascii_uppercase() - b'A' + 1) as u32)?;
    }
    let col = s[n..].parse::<u32>().ok()?.checked_sub(1)?;
    Some((row - 1, col))
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Level {
    ifd: usize,
    width: u32,
    height: u32,
}

struct Source {
    path: PathBuf,
    fields: BTreeMap<String, String>,
    xml: String,
    source_path: Option<String>,
    levels: Vec<Level>,
    pixel_type: PixelType,
    bits: u16,
    little_endian: bool,
}

impl Source {
    fn get(&self, path: &str) -> Option<&str> {
        self.fields
            .get(&format!("ImageInfo/{path}"))
            .map(String::as_str)
    }
    fn required(&self, path: &str) -> Result<&str> {
        self.get(path)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| invalid(format!("{} missing {path}", self.path.display())))
    }
    fn spacing(&self) -> Option<f64> {
        self.get("AcqInfo/FullResPixelSpacing")
            .and_then(length)
            .filter(|&n| n > 0.0)
    }
    fn instrument(&self) -> &str {
        self.get("DeviceInfo/IncuCyteID")
            .or_else(|| self.get("DeviceInfo/ControllerSerialNumber"))
            .unwrap_or("unknown")
    }
    fn family(&self) -> String {
        match self.get("AcqInfo/ImageType").unwrap_or("Unknown") {
            s if s.starts_with("FLR (Color ") => "Fluorescence".into(),
            "Phase" => "Phase".into(),
            "Cell-by-Cell Seed" => "Seed".into(),
            s => s.into(),
        }
    }
    fn channel(&self) -> Result<String> {
        Ok(self.required("AcqInfo/ImageType")?.to_owned())
    }
    fn scan(&self) -> Result<i64> {
        let value = self
            .get("ScanInfo/ScanDateTime")
            .or_else(|| self.get("AcqInfo/TimeStamp"))
            .ok_or_else(|| invalid("missing scan timestamp"))?;
        timestamp(value)
            .map(|t| t.0)
            .ok_or_else(|| invalid(format!("invalid scan timestamp {value}")))
    }
    fn stage(&self) -> Option<(f64, f64)> {
        self.get("AcqInfo/Location/GantryPosition")
            .and_then(position)
    }
}

fn inspect(path: &Path) -> Result<Source> {
    let mut parser = TiffParser::new(BufReader::new(File::open(path)?))?;
    let le = parser.little_endian;
    let mut offset = parser.first_ifd_offset;
    let mut dirs = Vec::<(u64, Ifd)>::new();
    let mut visited = HashSet::new();
    while offset != 0 {
        if dirs.len() >= MAX_IFDS || !visited.insert(offset) {
            return Err(invalid("too many or cyclic TIFF directories"));
        }
        let (ifd, next) = parser.read_ifd(offset)?;
        dirs.push((offset, ifd));
        offset = next;
    }
    let first = dirs.first().ok_or_else(|| invalid("empty TIFF"))?;
    let metadata_offset = first
        .1
        .get_u64(POINTER)
        .filter(|&p| p != 0)
        .or_else(|| {
            dirs.iter()
                .find(|(_, d)| d.get(XML).is_some())
                .map(|(o, _)| *o)
        })
        .ok_or_else(|| invalid("missing metadata pointer/XML"))?;
    let (metadata_ifd, _) = parser.read_ifd(metadata_offset)?;
    let bytes = raw_xml(&mut parser, metadata_offset)?
        .ok_or_else(|| invalid("metadata pointer has no XML"))?;
    let xml = decode_xml(&bytes);
    let fields = xml_fields(&xml)?;
    let source_path = metadata_ifd.get_str(60011).map(str::to_owned);
    let bits = first.1.get_u16(tag::BITS_PER_SAMPLE).unwrap_or(8);
    let sample_format = first.1.get_u16(tag::SAMPLE_FORMAT).unwrap_or(1);
    let pixel_type = match (sample_format, bits) {
        (1, 8) => PixelType::Uint8,
        (1, 16) => PixelType::Uint16,
        (3, 32) => PixelType::Float32,
        _ => return Err(invalid("unsupported sample format/bit depth")),
    };
    let mut levels: Vec<Level> = Vec::new();
    for (i, (o, d)) in dirs.iter().enumerate() {
        if *o == metadata_offset || d.get(XML).is_some() {
            continue;
        }
        let width = d
            .image_width()
            .filter(|&n| n > 0)
            .ok_or_else(|| invalid("invalid width"))?;
        let height = d
            .image_length()
            .filter(|&n| n > 0)
            .ok_or_else(|| invalid("invalid height"))?;
        if d.get_u16(tag::SAMPLES_PER_PIXEL).unwrap_or(1) != 1
            || d.get_u16(tag::BITS_PER_SAMPLE).unwrap_or(8) != bits
            || d.get_u16(tag::SAMPLE_FORMAT).unwrap_or(1) != sample_format
            || d.get_u16(tag::PHOTOMETRIC_INTERPRETATION).unwrap_or(1) != 1
            || d.get_u16(274).unwrap_or(1) != 1
        {
            return Err(invalid(
                "inconsistent or unsupported image samples/orientation",
            ));
        }
        if let Some(prev) = levels.last() {
            if width != prev.width.div_ceil(2)
                || height != prev.height.div_ceil(2)
                || (width == prev.width && height == prev.height)
            {
                return Err(invalid(
                    "unexpected main-chain image; not a half-size pyramid",
                ));
            }
        }
        levels.push(Level {
            ifd: i,
            width,
            height,
        });
    }
    if levels.is_empty() {
        return Err(invalid("no image directories"));
    }
    Ok(Source {
        path: path.to_owned(),
        fields,
        xml,
        source_path,
        levels,
        pixel_type,
        bits,
        little_endian: le,
    })
}

/// A bounded TIFF metadata probe. It does not depend on filenames or on an
/// Incucyte Software prefix (the supplied files say SnTiffDll.dll).
pub(crate) fn is_incucyte_tiff(path: &Path) -> bool {
    let probe = || -> Result<bool> {
        let mut parser = TiffParser::new(BufReader::new(File::open(path)?))?;
        let mut offset = parser.first_ifd_offset;
        let (first, _) = parser.read_ifd(offset)?;
        if let Some(pointer) = first.get_u64(POINTER).filter(|&p| p != 0) {
            // Keep recognized damaged Incucyte data from silently falling back
            // to generic TIFF with false DPI calibration.
            if first
                .get_str(tag::SOFTWARE)
                .is_some_and(|s| s.starts_with("SnTiffDll"))
            {
                return Ok(true);
            }
            return Ok(raw_xml(&mut parser, pointer)?
                .is_some_and(|b| decode_xml(&b).contains("<IncuCyteMetadataVersion>")));
        }
        if first.get(XML).is_some() {
            return Ok(raw_xml(&mut parser, offset)?
                .is_some_and(|b| decode_xml(&b).contains("<IncuCyteMetadataVersion>")));
        }
        // The pointer identifies raw files even if Software is absent. Only
        // walk a pointer-less chain for a known vendor Software signature;
        // probing an ordinary multipage/whole-slide TIFF must not scan its
        // entire directory chain before the generic reader can open it.
        if !first.get_str(tag::SOFTWARE).is_some_and(|s| {
            s.starts_with("SnTiffDll") || s.starts_with("IncuCyte") || s.starts_with("Incucyte")
        }) {
            return Ok(false);
        }
        let mut seen = HashSet::new();
        for _ in 0..MAX_IFDS {
            if offset == 0 || !seen.insert(offset) {
                break;
            }
            let (d, next) = parser.read_ifd(offset)?;
            if d.get(XML).is_some() {
                return Ok(raw_xml(&mut parser, offset)?
                    .is_some_and(|b| decode_xml(&b).contains("<IncuCyteMetadataVersion>")));
            }
            offset = next;
        }
        Ok(false)
    };
    probe().unwrap_or(false)
}

fn is_tiff(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("tif") || s.eq_ignore_ascii_case("tiff"))
}
fn is_archive(path: &Path) -> bool {
    path.extension()
        .and_then(|s| s.to_str())
        .is_some_and(|s| s.eq_ignore_ascii_case("icarch"))
}

fn discover(root: &Path, recursive: bool, depth: usize, paths: &mut Vec<PathBuf>) -> Result<()> {
    if depth > 8 {
        return Err(invalid("archive directory depth limit"));
    }
    for entry in fs::read_dir(root)? {
        let entry = entry?;
        let ty = entry.file_type()?;
        // Never follow source-path metadata or directory/file symlinks outside
        // the selected root. A directly selected symlink is resolved at entry.
        if ty.is_file() && is_tiff(&entry.path()) {
            if paths.len() >= MAX_FILES {
                return Err(invalid("archive file count limit"));
            }
            paths.push(entry.path());
        } else if recursive && ty.is_dir() {
            discover(&entry.path(), true, depth + 1, paths)?;
        }
    }
    Ok(())
}

struct Stream {
    files: Vec<usize>, // t-major, then channel (XYCZT, Z=1)
    channels: Vec<String>,
    times: Vec<i64>,
    levels: Vec<Level>,
    metadata: ImageMetadata,
    vessel: (String, String),
    well: String,
    site: u32,
    family: String,
}

/// Raw Incucyte reader. The default preserves native stream families:
/// fluorescence channels together, phase and auxiliary Seed separate.
/// Each stream is an OME Image/WellSample; `incucyte.site` retains physical
/// site identity when multiple modality streams share a stage position.
pub struct IncucyteReader {
    sources: Vec<Source>,
    streams: Vec<Stream>,
    views: Vec<(usize, usize)>, // public series -> (stream, initial resolution)
    current_series: usize,
    current_resolution: usize,
    current_metadata: Option<ImageMetadata>,
    flattened: bool,
    group_files: bool,
    cached: Option<(PathBuf, TiffReader)>,
}

impl Default for IncucyteReader {
    fn default() -> Self {
        Self::new()
    }
}

impl IncucyteReader {
    pub fn new() -> Self {
        Self {
            sources: Vec::new(),
            streams: Vec::new(),
            views: Vec::new(),
            current_series: 0,
            current_resolution: 0,
            current_metadata: None,
            flattened: true,
            group_files: true,
            cached: None,
        }
    }

    /// Disable companion discovery for a selected TIFF. Archive marker entry
    /// points always discover their images. Configure before `set_id`.
    pub fn set_group_files(&mut self, enabled: bool) -> Result<()> {
        if !self.views.is_empty() {
            return Err(invalid("set grouping before set_id"));
        }
        self.group_files = enabled;
        Ok(())
    }

    fn current(&self) -> Result<(&Stream, usize)> {
        let &(s, l) = self
            .views
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        Ok((
            &self.streams[s],
            if self.flattened {
                l
            } else {
                self.current_resolution
            },
        ))
    }

    fn metadata_at(&self, stream: usize, level: usize) -> ImageMetadata {
        let s = &self.streams[stream];
        let mut m = s.metadata.clone();
        m.size_x = s.levels[level].width;
        m.size_y = s.levels[level].height;
        m.thumbnail = level > 0;
        m.resolution_count = if self.flattened {
            1
        } else {
            s.levels.len() as u32
        };
        m
    }

    fn refresh(&mut self) {
        if let Some(&(s, l)) = self.views.get(self.current_series) {
            self.current_metadata = Some(self.metadata_at(
                s,
                if self.flattened {
                    l
                } else {
                    self.current_resolution
                },
            ));
        }
    }

    fn build_streams(&mut self, root: &Path) -> Result<()> {
        type Key = (String, String, (u32, u32), String, u32, String);
        let mut groups = BTreeMap::<Key, Vec<usize>>::new();
        for (i, source) in self.sources.iter().enumerate() {
            let well_name = source.required("AcqInfo/Location/WellorSector")?;
            let coords = well(well_name)
                .ok_or_else(|| invalid(format!("unsupported well/sector {well_name}")))?;
            let site = source
                .required("AcqInfo/Location/UnityBasedImageIndex")?
                .parse::<u32>()
                .ok()
                .filter(|&n| n > 0)
                .ok_or_else(|| invalid("invalid one-based site"))?;
            groups
                .entry((
                    source.instrument().into(),
                    source.required("ScanInfo/VesselID")?.into(),
                    coords,
                    well_name.to_uppercase(),
                    site,
                    source.family(),
                ))
                .or_default()
                .push(i);
        }
        for ((instrument, vessel, _, well_name, site, family), indices) in groups {
            let first = &self.sources[indices[0]];
            let mut channels: Vec<String> = indices
                .iter()
                .map(|&i| self.sources[i].channel())
                .collect::<Result<BTreeSet<_>>>()?
                .into_iter()
                .collect();
            channels.sort_by_key(|kind| {
                let index = kind
                    .strip_prefix("FLR (Color ")
                    .and_then(|s| s.strip_suffix(')'))
                    .and_then(|s| s.parse::<u32>().ok());
                (index.is_none(), index.unwrap_or(0), kind.clone())
            });
            let times: Vec<i64> = indices
                .iter()
                .map(|&i| self.sources[i].scan())
                .collect::<Result<BTreeSet<_>>>()?
                .into_iter()
                .collect();
            let mut plane_map = BTreeMap::new();
            for &i in &indices {
                let source = &self.sources[i];
                if source.pixel_type != first.pixel_type
                    || source.bits != first.bits
                    || source.little_endian != first.little_endian
                    || source.spacing() != first.spacing()
                    || source.get("AcqInfo/ObjectiveMagnification")
                        != first.get("AcqInfo/ObjectiveMagnification")
                    || source
                        .levels
                        .iter()
                        .map(|l| (l.width, l.height))
                        .collect::<Vec<_>>()
                        != first
                            .levels
                            .iter()
                            .map(|l| (l.width, l.height))
                            .collect::<Vec<_>>()
                {
                    return Err(invalid(format!("inconsistent dtype/geometry/calibration in vessel {vessel} {well_name} site {site} {family}")));
                }
                if plane_map
                    .insert((source.scan()?, source.channel()?), i)
                    .is_some()
                {
                    return Err(invalid("duplicate scan/well/site/channel plane"));
                }
            }
            let mut files = Vec::new();
            for &t in &times {
                for c in &channels {
                    files.push(*plane_map.get(&(t, c.clone())).ok_or_else(|| {
                        invalid(format!(
                            "missing plane in {vessel} {well_name} site {site} {family}"
                        ))
                    })?);
                }
            }
            let size_c = u32::try_from(channels.len()).map_err(|_| invalid("too many channels"))?;
            let size_t = u32::try_from(times.len()).map_err(|_| invalid("too many timepoints"))?;
            let mut metadata = ImageMetadata {
                size_x: first.levels[0].width,
                size_y: first.levels[0].height,
                size_z: 1,
                size_c,
                size_t,
                image_count: size_c
                    .checked_mul(size_t)
                    .ok_or_else(|| invalid("plane count overflow"))?,
                pixel_type: first.pixel_type,
                bits_per_pixel: first.bits,
                dimension_order: DimensionOrder::XYCZT,
                is_little_endian: first.little_endian,
                ..Default::default()
            };
            let map = &mut metadata.series_metadata;
            for (k, v) in &first.fields {
                map.insert(format!("incucyte.{k}"), MetadataValue::String(v.clone()));
            }
            for (k, v) in [
                ("reader", "IncucyteReader".into()),
                ("incucyte.discovery_root", root.display().to_string()),
                ("incucyte.vessel", vessel.clone()),
                ("incucyte.well", well_name.clone()),
                ("incucyte.site", site.to_string()),
                ("incucyte.family", family.clone()),
                (
                    "incucyte.stage_convention",
                    "Native gantry position; camera axes and centre/corner registration unknown"
                        .into(),
                ),
                ("incucyte.timezone", "Unspecified local time".into()),
                (
                    "incucyte.intensity",
                    "Stored values; conversion scale/bias not applied".into(),
                ),
            ] {
                map.insert(k.into(), MetadataValue::String(v));
            }
            for (p, &i) in files.iter().enumerate() {
                let source = &self.sources[i];
                map.insert(
                    format!("incucyte.plane.{p}.file"),
                    MetadataValue::String(source.path.display().to_string()),
                );
                map.insert(
                    format!("incucyte.plane.{p}.xml"),
                    MetadataValue::String(source.xml.clone()),
                );
                if let Some(path) = &source.source_path {
                    map.insert(
                        format!("incucyte.plane.{p}.instrument_path"),
                        MetadataValue::String(path.clone()),
                    );
                }
            }
            self.streams.push(Stream {
                files,
                channels,
                times,
                levels: first.levels.clone(),
                metadata,
                vessel: (instrument, vessel),
                well: well_name,
                site,
                family,
            });
        }
        for (s, stream) in self.streams.iter().enumerate() {
            for l in 0..if self.flattened {
                stream.levels.len()
            } else {
                1
            } {
                self.views.push((s, l));
            }
        }
        self.refresh();
        Ok(())
    }
}

impl FormatReader for IncucyteReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        is_archive(path) || (is_tiff(path) && is_incucyte_tiff(path))
    }
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        std::str::from_utf8(header).is_ok_and(|s| {
            s.trim_start_matches('\u{feff}')
                .contains("<incuCyteArchiveHeaderFile")
        })
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let archive = is_archive(path);
        let path = path.canonicalize()?;
        let root;
        let mut paths = Vec::new();
        if archive {
            if fs::metadata(&path)?.len() > MAX_XML {
                return Err(invalid("oversized archive marker"));
            }
            let text = fs::read_to_string(&path)?;
            validate_archive_marker(&text)?;
            root = path
                .parent()
                .ok_or_else(|| invalid("archive has no parent"))?
                .join("EssenFiles/ScanData");
            discover(&root, true, 0, &mut paths)?;
        } else {
            // Parsing the selected file is mandatory, not a best-effort probe.
            inspect(&path)?;
            if let Some(scan_root) = path.ancestors().take(7).find(|p| {
                self.group_files
                    && p.file_name().is_some_and(|s| s == "ScanData")
                    && p.parent()
                        .and_then(Path::file_name)
                        .is_some_and(|s| s == "EssenFiles")
            }) {
                root = scan_root.to_owned();
                discover(&root, true, 0, &mut paths)?;
            } else {
                // A moved TIFF is standalone: do not merge unrelated siblings.
                root = path.parent().unwrap_or(Path::new(".")).to_owned();
                paths.push(path.clone());
            }
        }
        paths.sort();
        if paths.is_empty() {
            return Err(invalid("no TIFF images in archive"));
        }
        let selected = if archive {
            None
        } else {
            let s = inspect(&path)?;
            Some((
                s.instrument().to_owned(),
                s.required("ScanInfo/VesselID")?.to_owned(),
            ))
        };
        let mut sources = Vec::new();
        for p in paths {
            let source = match inspect(&p) {
                Ok(s) => s,
                Err(e) if p == path || is_incucyte_tiff(&p) => {
                    return Err(invalid(format!("{}: {e}", p.display())))
                }
                Err(_) => continue, // unrelated generic TIFF in the archive tree
            };
            if selected.as_ref().is_none_or(|(instrument, vessel)| {
                source.instrument() == instrument
                    && source.get("ScanInfo/VesselID") == Some(vessel.as_str())
            }) {
                sources.push(source);
            }
        }
        if sources.is_empty() {
            return Err(invalid("no matching vessel images"));
        }
        self.sources = sources;
        if let Err(e) = self.build_streams(&root) {
            self.close()?;
            return Err(e);
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.sources.clear();
        self.streams.clear();
        self.views.clear();
        self.cached = None;
        self.current_metadata = None;
        self.current_series = 0;
        self.current_resolution = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.views.len()
    }
    fn series(&self) -> usize {
        self.current_series
    }
    fn set_series(&mut self, series: usize) -> Result<()> {
        if series >= self.views.len() {
            return Err(BioFormatsError::SeriesOutOfRange(series));
        }
        self.current_series = series;
        self.current_resolution = 0;
        self.refresh();
        Ok(())
    }
    fn metadata(&self) -> &ImageMetadata {
        self.current_metadata
            .as_ref()
            .unwrap_or_else(|| uninitialized_metadata())
    }
    fn has_flattened_resolutions(&self) -> bool {
        self.flattened
    }
    fn set_flattened_resolutions(&mut self, flattened: bool) -> Result<()> {
        if !self.views.is_empty() {
            return Err(invalid("set flattening before set_id"));
        }
        self.flattened = flattened;
        Ok(())
    }
    fn resolution_count(&self) -> usize {
        self.current()
            .map(|(s, _)| if self.flattened { 1 } else { s.levels.len() })
            .unwrap_or(0)
    }
    fn resolution(&self) -> usize {
        self.current_resolution
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level >= self.resolution_count() {
            return Err(invalid(format!("resolution {level} out of range")));
        }
        self.current_resolution = level;
        self.refresh();
        Ok(())
    }
    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let m = self.metadata();
        self.open_bytes_region(plane_index, 0, 0, m.size_x, m.size_y)
    }
    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let (stream, level) = self.current()?;
        let &i = stream
            .files
            .get(plane_index as usize)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let source = &self.sources[i];
        let l = &source.levels[level];
        if x > l.width || y > l.height || w > l.width - x || h > l.height - y {
            return Err(invalid("region outside image"));
        }
        let ifd = l.ifd;
        let path = source.path.clone();
        if self.cached.as_ref().is_none_or(|(p, _)| p != &path) {
            let mut reader = TiffReader::new();
            reader.set_id(&path)?;
            self.cached = Some((path, reader));
        }
        self.cached
            .as_mut()
            .unwrap()
            .1
            .read_physical_ifd_region(ifd, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        // Native smallest level, consistently capped with a nearest-neighbour
        // thumbnail when a standalone Seed stream has no stored reduction.
        let old = self.current_resolution;
        if !self.flattened {
            self.set_resolution(self.resolution_count().saturating_sub(1))?;
        }
        let result = (|| {
            let m = self.metadata().clone();
            let bytes = self.open_bytes(plane_index)?;
            let tw = m.size_x.min(256);
            let th = m.size_y.min(256);
            let bpp = m.pixel_type.bytes_per_sample();
            let mut out = Vec::with_capacity(tw as usize * th as usize * bpp);
            for y in 0..th {
                for x in 0..tw {
                    let at = ((u64::from(y) * u64::from(m.size_y) / u64::from(th))
                        * u64::from(m.size_x)
                        + u64::from(x) * u64::from(m.size_x) / u64::from(tw))
                        as usize
                        * bpp;
                    out.extend_from_slice(&bytes[at..at + bpp]);
                }
            }
            Ok(out)
        })();
        self.current_resolution = old;
        self.refresh();
        result
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        if self.views.is_empty() {
            return None;
        }
        let mut ome = OmeMetadata::default();
        let mut origins = BTreeMap::<(String, String), i64>::new();
        for source in &self.sources {
            if let (Some(vessel), Some((time, _))) = (
                source.get("ScanInfo/VesselID"),
                source.get("AcqInfo/TimeStamp").and_then(timestamp),
            ) {
                origins
                    .entry((source.instrument().into(), vessel.into()))
                    .and_modify(|t| *t = (*t).min(time))
                    .or_insert(time);
            }
        }
        let mut instruments = BTreeMap::<String, usize>::new();
        let mut plates = BTreeMap::<(String, String), usize>::new();
        for (image_index, &(s, level)) in self.views.iter().enumerate() {
            let stream = &self.streams[s];
            let origin = origins.get(&stream.vessel).copied();
            let first = &self.sources[stream.files[0]];
            let instrument_index = if let Some(&i) = instruments.get(first.instrument()) {
                i
            } else {
                let i = ome.instruments.len();
                instruments.insert(first.instrument().into(), i);
                ome.instruments.push(OmeInstrument {
                    id: Some(create_lsid("Instrument", &[i])),
                    microscope_model: first.get("DeviceInfo/HardwareName").map(str::to_owned),
                    microscope_manufacturer: Some("Sartorius / Essen BioScience".into()),
                    microscope_serial_number: first
                        .get("DeviceInfo/ControllerSerialNumber")
                        .map(str::to_owned),
                    objectives: vec![OmeObjective {
                        id: Some(create_lsid("Objective", &[i, 0])),
                        nominal_magnification: first
                            .get("AcqInfo/ObjectiveMagnification")
                            .and_then(number),
                        ..Default::default()
                    }],
                    detectors: vec![OmeDetector {
                        id: Some(create_lsid("Detector", &[i, 0])),
                        model: first.get("DeviceInfo/Camera/Model").map(str::to_owned),
                        ..Default::default()
                    }],
                    ..Default::default()
                });
                i
            };
            let magnification = first.get("AcqInfo/ObjectiveMagnification").and_then(number);
            let objectives = &mut ome.instruments[instrument_index].objectives;
            let objective_index = objectives
                .iter()
                .position(|o| o.nominal_magnification == magnification)
                .unwrap_or_else(|| {
                    let i = objectives.len();
                    objectives.push(OmeObjective {
                        id: Some(create_lsid("Objective", &[instrument_index, i])),
                        nominal_magnification: magnification,
                        ..Default::default()
                    });
                    i
                });
            let mut image = OmeImage {
                pixels:Some(OmePixels::from(&self.metadata_at(s,level))),
                name:Some(format!("Vessel {} {} Site {} {}{}", stream.vessel.1, stream.well, stream.site, stream.family, if level == 0 { String::new() } else { format!(" resolution {level}") })),
                description:Some("Native gantry coordinates; image-to-stage registration and acquisition timezone unspecified. Stored values retained without applying vendor conversion coefficients. Seed is auxiliary image data.".into()),
                physical_size_x:first.spacing().map(|n| n * first.levels[0].width as f64 / stream.levels[level].width as f64),
                physical_size_y:first.spacing().map(|n| n * first.levels[0].height as f64 / stream.levels[level].height as f64),
                instrument_ref:Some(instrument_index), objective_ref:Some(objective_index), ..Default::default()
            };
            // No synthetic UTC offset. Local timestamps remain in original XML.
            if stream.times.len() > 1 {
                let dt = stream.times[1] - stream.times[0];
                if stream.times.windows(2).all(|w| w[1] - w[0] == dt) {
                    image.time_increment = Some(dt as f64);
                }
            }
            for (c, kind) in stream.channels.iter().enumerate() {
                let source = &self.sources[stream.files[c]];
                let index = kind
                    .strip_prefix("FLR (Color ")
                    .and_then(|v| v.strip_suffix(')'));
                let prefix = index.map(|n| format!("DeviceInfo/OpticalModule/ColorChannel{n}"));
                let channel_name = prefix
                    .as_ref()
                    .and_then(|p| source.get(&format!("{p}/Name")))
                    .unwrap_or(kind);
                image.channels.push(OmeChannel {
                    name: Some(channel_name.into()),
                    samples_per_pixel: 1,
                    color: match channel_name {
                        "Green" => Some(0x00ff00ff),
                        "Red" => Some(0xff0000ffu32 as i32),
                        _ => None,
                    },
                    emission_wavelength: prefix
                        .as_ref()
                        .and_then(|p| source.get(&format!("{p}/EmissionWavelength")))
                        .and_then(number),
                    excitation_wavelength: prefix
                        .as_ref()
                        .and_then(|p| source.get(&format!("{p}/ExcitationWavelength")))
                        .and_then(number),
                    detector_settings_gain: source
                        .get("AcqInfo/CameraSettings/ExpGain")
                        .and_then(number),
                    detector_ref: Some(create_lsid("Detector", &[instrument_index, 0])),
                    ..Default::default()
                });
            }
            for (p, &i) in stream.files.iter().enumerate() {
                let source = &self.sources[i];
                let stage = source.stage();
                let exp = source.get("AcqInfo/CameraSettings/ExpTime").and_then(|s| {
                    s.ends_with("msec")
                        .then(|| number(s).map(|n| n / 1000.0))
                        .flatten()
                }); // FLR AcqTime is a combined acquisition budget, not one exposure.
                image.planes.push(OmePlane {
                    the_z: 0,
                    the_c: (p % stream.channels.len()) as u32,
                    the_t: (p / stream.channels.len()) as u32,
                    delta_t: source
                        .get("AcqInfo/TimeStamp")
                        .and_then(timestamp)
                        .and_then(|t| origin.map(|o| (t.0 - o) as f64)),
                    exposure_time: exp,
                    position_x: stage.map(|p| p.0),
                    position_y: stage.map(|p| p.1),
                    position_z: source
                        .get("AcqInfo/Location/FocusPosition")
                        .and_then(length),
                    position_x_unit: stage.map(|_| "µm".into()),
                    position_y_unit: stage.map(|_| "µm".into()),
                    position_z_unit: source
                        .get("AcqInfo/Location/FocusPosition")
                        .and_then(length)
                        .map(|_| "µm".into()),
                });
            }
            ome.images.push(image);
            // Original per-plane XML is attached once per native stream rather
            // than duplicated for every flattened resolution.
            if level == 0 {
                let _ = ome.add_original_metadata_annotations(&stream.metadata, image_index);
            }
            if level != 0 {
                continue;
            }
            let pi = if let Some(&i) = plates.get(&stream.vessel) {
                i
            } else {
                let i = ome.plates.len();
                plates.insert(stream.vessel.clone(), i);
                let labware = first.get("ScanInfo/Labware/Name").unwrap_or("");
                let (rows, columns) = if labware.to_lowercase().contains("24-well") {
                    (4, 6)
                } else {
                    (0, 0)
                };
                ome.plates.push(OmePlate {
                    id: Some(create_lsid("Plate", &[i])),
                    name: Some(format!("Incucyte vessel {}", stream.vessel.1)),
                    rows,
                    columns,
                    wells: Vec::new(),
                });
                i
            };
            let (row, col) = well(&stream.well)?;
            let plate = &mut ome.plates[pi];
            plate.rows = plate.rows.max(row + 1);
            plate.columns = plate.columns.max(col + 1);
            let wi = plate
                .wells
                .iter()
                .position(|w| w.row == row && w.column == col)
                .unwrap_or_else(|| {
                    let i = plate.wells.len();
                    plate.wells.push(OmeWell {
                        id: Some(create_lsid("Well", &[pi, i])),
                        row,
                        column: col,
                        well_samples: Vec::new(),
                    });
                    i
                });
            let stage = first.stage();
            let sample = plate.wells[wi].well_samples.len();
            plate.wells[wi].well_samples.push(OmeWellSample {
                id: Some(create_lsid("WellSample", &[pi, wi, sample])),
                index: image_index as u32,
                image_ref: Some(image_index),
                position_x: stage.map(|s| s.0),
                position_y: stage.map(|s| s.1),
            });
        }
        Some(ome)
    }
}
