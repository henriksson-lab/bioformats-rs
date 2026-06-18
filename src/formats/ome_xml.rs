//! OME-XML format reader (.ome files with inline Base64 pixel data).
//!
//! OME-XML is an open format where pixel metadata is encoded in an XML header
//! and pixel data is Base64-encoded inline in `<BinData>` elements.
//!
//! The XML structure looks like:
//! ```xml
//! <OME>
//!   <Image>
//!     <Pixels SizeX="512" SizeY="512" SizeZ="10" SizeC="3" SizeT="1"
//!             Type="uint8" DimensionOrder="XYZCT">
//!       <BinData Length="..." BigEndian="false">BASE64DATA...</BinData>
//!     </Pixels>
//!   </Image>
//! </OME>
//! ```

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::path::confined_join;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::{crop_full_plane, validate_region};

struct ParsedOmeSeries {
    meta: ImageMetadata,
    /// Inline Base64 `<BinData>` planes (empty when pixels are external).
    planes: Vec<Vec<u8>>,
    /// External plane mapping for `<TiffData>` references, one entry per logical
    /// plane. `None` means the plane has no associated pixel data (black plane).
    /// `Some((file, ifd))` resolves to IFD `ifd` of companion TIFF `file`.
    external_planes: Vec<Option<ExternalPlane>>,
}

/// A logical plane resolved to a companion TIFF file and IFD index, mirroring
/// the `OMETiffPlane` structure in Java's `OMETiffReader`.
#[derive(Debug, Clone)]
struct ExternalPlane {
    /// Absolute path to the companion TIFF.
    path: PathBuf,
    /// IFD index within that TIFF.
    ifd: usize,
}

// ─── Minimal Base64 decoder ───────────────────────────────────────────────────

const B64_TABLE: [u8; 256] = {
    let mut t = [255u8; 256];
    let mut i = 0usize;
    let chars = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    while i < 64 {
        t[chars[i] as usize] = i as u8;
        i += 1;
    }
    t
};

fn base64_decode(input: &str) -> Vec<u8> {
    let input: Vec<u8> = input
        .bytes()
        .filter(|&b| !b.is_ascii_whitespace())
        .collect();
    let n = input.len();
    if n == 0 {
        return vec![];
    }
    let mut out = Vec::with_capacity((n / 4) * 3 + 3);
    let mut i = 0;
    while i + 3 < n {
        let a = B64_TABLE[input[i] as usize];
        let b = B64_TABLE[input[i + 1] as usize];
        let c = B64_TABLE[input[i + 2] as usize];
        let d = B64_TABLE[input[i + 3] as usize];
        if a == 255 || b == 255 {
            break;
        }
        out.push((a << 2) | (b >> 4));
        if input[i + 2] != b'=' && c != 255 {
            out.push((b << 4) | (c >> 2));
        }
        if input[i + 3] != b'=' && d != 255 {
            out.push((c << 6) | d);
        }
        i += 4;
    }
    out
}

// ─── Minimal XML attribute extractor ─────────────────────────────────────────

/// Extract the value of `attr` from an XML element start tag.
fn xml_attr(tag: &str, attr: &str) -> Option<String> {
    let needle = format!("{}=", attr);
    let pos = tag.match_indices(&needle).find_map(|(pos, _)| {
        tag[..pos]
            .chars()
            .next_back()
            .is_none_or(|c| c.is_ascii_whitespace() || c == '<')
            .then_some(pos)
    })?;
    let rest = &tag[pos + needle.len()..];
    // Value may be quoted with " or '
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let inner = &rest[1..];
        let end = inner.find(quote)?;
        Some(inner[..end].to_string())
    } else {
        // Unquoted: read until space or >
        let end = rest
            .find(|c: char| c.is_whitespace() || c == '>')
            .unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

fn tag_local_name(tag: &str) -> &str {
    tag.rsplit_once(':').map(|(_, local)| local).unwrap_or(tag)
}

fn start_tag_at(xml: &str, pos: usize) -> &str {
    let mut quote = None;
    let mut end = xml.len();
    for (rel, ch) in xml[pos..].char_indices() {
        match quote {
            Some(q) if ch == q => quote = None,
            Some(_) => {}
            None if ch == '"' || ch == '\'' => quote = Some(ch),
            None if ch == '>' => {
                end = pos + rel + ch.len_utf8();
                break;
            }
            None => {}
        }
    }
    &xml[pos..end]
}

fn start_tag_name(tag: &str) -> Option<&str> {
    let s = tag.strip_prefix('<')?;
    let s = s.strip_prefix('/').unwrap_or(s);
    let s = s.trim_start();
    let name_end = s
        .find(|c: char| c.is_whitespace() || c == '>' || c == '/')
        .unwrap_or(s.len());
    Some(&s[..name_end])
}

fn tag_name_at(xml: &str, pos: usize) -> Option<&str> {
    start_tag_name(start_tag_at(xml, pos))
}

fn tag_positions(xml: &str, local_name: &str) -> Vec<usize> {
    let mut out = Vec::new();
    let mut search = 0usize;
    while let Some(rel) = xml[search..].find('<') {
        let pos = search + rel;
        if xml[pos + 1..].starts_with('/') || xml[pos + 1..].starts_with('!') {
            search = pos + 1;
            continue;
        }
        if let Some(name) = tag_name_at(xml, pos) {
            if tag_local_name(name).eq_ignore_ascii_case(local_name) {
                out.push(pos);
            }
        }
        search = pos + 1;
    }
    out
}

fn end_tag_after(xml: &str, start: usize, local_name: &str) -> usize {
    let Some(pos) = end_tag_start_after(xml, start, local_name) else {
        return xml.len();
    };
    xml[pos..]
        .find('>')
        .map(|e| pos + e + 1)
        .unwrap_or(xml.len())
}

fn end_tag_start_after(xml: &str, start: usize, local_name: &str) -> Option<usize> {
    let mut search = start;
    while let Some(rel) = xml[search..].find("</") {
        let pos = search + rel;
        if let Some(name) = tag_name_at(xml, pos) {
            if tag_local_name(name).eq_ignore_ascii_case(local_name) {
                return Some(pos);
            }
        }
        search = pos + 2;
    }
    None
}

fn child_block<'a>(xml: &'a str, local_name: &str) -> Option<&'a str> {
    let pos = tag_positions(xml, local_name).into_iter().next()?;
    let end = end_tag_after(xml, pos, local_name);
    Some(&xml[pos..end])
}

fn attr_required_nonzero_u32(tag: &str, attr: &str) -> Result<u32> {
    let value = xml_attr(tag, attr)
        .or_else(|| xml_attr(tag, &attr.to_ascii_lowercase()))
        .ok_or_else(|| BioFormatsError::Format(format!("OME-XML missing {attr}")))?;
    let parsed = value
        .parse::<u32>()
        .map_err(|_| BioFormatsError::Format(format!("OME-XML invalid {attr}: {value}")))?;
    if parsed == 0 {
        return Err(BioFormatsError::Format(format!(
            "OME-XML {attr} must be positive"
        )));
    }
    Ok(parsed)
}

fn dimension_order_from_attr(value: &str) -> Result<DimensionOrder> {
    Ok(match value.to_ascii_uppercase().as_str() {
        "XYZCT" => DimensionOrder::XYZCT,
        "XYZTC" => DimensionOrder::XYZTC,
        "XYCZT" => DimensionOrder::XYCZT,
        "XYCTZ" => DimensionOrder::XYCTZ,
        "XYTZC" => DimensionOrder::XYTZC,
        "XYTCZ" => DimensionOrder::XYTCZ,
        _ => {
            return Err(BioFormatsError::Format(format!(
                "OME-XML unsupported DimensionOrder {value}"
            )));
        }
    })
}

fn pixel_type_from_attr(value: &str) -> Result<(PixelType, u8)> {
    Ok(match value.to_ascii_lowercase().as_str() {
        "int8" => (PixelType::Int8, 8),
        "uint8" => (PixelType::Uint8, 8),
        "int16" => (PixelType::Int16, 16),
        "uint16" => (PixelType::Uint16, 16),
        "int32" => (PixelType::Int32, 32),
        "uint32" => (PixelType::Uint32, 32),
        "float" | "float32" => (PixelType::Float32, 32),
        "double" | "float64" => (PixelType::Float64, 64),
        _ => {
            return Err(BioFormatsError::Format(format!(
                "OME-XML unsupported Type {value}"
            )))
        }
    })
}

fn channel_samples_per_pixel(pixels_xml: &str, size_c: u32) -> Result<Vec<u32>> {
    let mut samples = Vec::new();
    for pos in tag_positions(pixels_xml, "Channel") {
        let tag = start_tag_at(pixels_xml, pos);
        let spp = match xml_attr(tag, "SamplesPerPixel") {
            Some(value) => value.parse::<u32>().map_err(|_| {
                BioFormatsError::Format(format!("OME-XML invalid SamplesPerPixel: {value}"))
            })?,
            None => 1,
        };
        if spp == 0 {
            return Err(BioFormatsError::Format(
                "OME-XML SamplesPerPixel must be positive".into(),
            ));
        }
        samples.push(spp);
    }
    while samples.len() < size_c as usize {
        samples.push(1);
    }
    samples.truncate(size_c as usize);
    Ok(samples)
}

fn parse_bindata_blocks(pixels_xml: &str) -> Result<(Vec<Vec<u8>>, Option<String>)> {
    let mut blocks = Vec::new();
    let mut first_big_endian = None;
    for pos in tag_positions(pixels_xml, "BinData") {
        let tag = start_tag_at(pixels_xml, pos);
        if first_big_endian.is_none() {
            first_big_endian = xml_attr(tag, "BigEndian").or_else(|| xml_attr(tag, "bigendian"));
        }
        let compression = xml_attr(tag, "Compression")
            .or_else(|| xml_attr(tag, "compression"))
            .unwrap_or_else(|| "none".to_string());
        let content_start = pos + tag.len();
        let content_end = end_tag_start_after(pixels_xml, pos, "BinData").unwrap_or(content_start);
        let b64_text = pixels_xml.get(content_start..content_end).unwrap_or("");
        let raw = base64_decode(b64_text);
        blocks.push(decompress_bindata(raw, &compression)?);
    }
    Ok((blocks, first_big_endian))
}

/// Decompress an inline `<BinData>` payload according to its `Compression`
/// attribute, mirroring `OMEXMLReader.openBytes`.
///
/// - `none` (or empty/absent) → raw bytes
/// - `zlib` → Deflate/Zlib via `codec::decompress_deflate`
/// - `J2K` → JPEG 2000 via `codec::decompress_jpeg2000`
/// - `JPEG` → JPEG via `codec::decompress_jpeg`
/// - `bzip2` → bzip2 via `codec::decompress_bzip2`
fn decompress_bindata(data: Vec<u8>, compression: &str) -> Result<Vec<u8>> {
    if data.is_empty() {
        return Ok(data);
    }
    match compression {
        "none" | "" => Ok(data),
        "zlib" => crate::common::codec::decompress_deflate(&data),
        "J2K" => crate::common::codec::decompress_jpeg2000(&data),
        "JPEG" => crate::common::codec::decompress_jpeg(&data),
        "bzip2" => crate::common::codec::decompress_bzip2(&data),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "OME-XML BinData unknown compression: {other}"
        ))),
    }
}

// ─── External TiffData resolution (ported from OMETiffReader.java) ───────────

/// A single `<TiffData>` element with its (optional) `<UUID FileName=...>`.
#[derive(Debug, Clone)]
struct ParsedTiffData {
    ifd: u32,
    plane_count: Option<u32>,
    first_z: u32,
    first_c: u32,
    first_t: u32,
    /// FileName from a nested `<UUID FileName="...">`, if any.
    filename: Option<String>,
}

/// Parse the `<TiffData>` children of a `<Pixels>` block, including any nested
/// `<UUID FileName="...">` element.
fn parse_tiff_data(pixels_xml: &str) -> Vec<ParsedTiffData> {
    let mut out = Vec::new();
    for pos in tag_positions(pixels_xml, "TiffData") {
        let tag = start_tag_at(pixels_xml, pos);
        // Extract the body of the TiffData element to look for a nested <UUID>.
        let body_start = pos + tag.len();
        // A self-closing <TiffData/> has no body.
        let self_closing = tag.trim_end().ends_with("/>");
        let body_end = if self_closing {
            body_start
        } else {
            end_tag_start_after(pixels_xml, pos, "TiffData").unwrap_or(body_start)
        };
        let body = pixels_xml.get(body_start..body_end).unwrap_or("");

        let filename = tag_positions(body, "UUID")
            .into_iter()
            .next()
            .and_then(|up| xml_attr(start_tag_at(body, up), "FileName"));

        out.push(ParsedTiffData {
            ifd: xml_attr(tag, "IFD")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            plane_count: xml_attr(tag, "PlaneCount").and_then(|s| s.parse().ok()),
            first_z: xml_attr(tag, "FirstZ")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            first_c: xml_attr(tag, "FirstC")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            first_t: xml_attr(tag, "FirstT")
                .and_then(|s| s.parse().ok())
                .unwrap_or(0),
            filename,
        });
    }
    out
}

/// Linearised plane index for given Z/C/T coordinates, mirroring
/// `FormatTools.getIndex` (see `ome_plane_index` in `tiff/reader.rs`).
fn ome_plane_index(
    z: u32,
    c: u32,
    t: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    order: DimensionOrder,
) -> Option<usize> {
    if z >= size_z || c >= size_c || t >= size_t {
        return None;
    }
    Some(match order {
        DimensionOrder::XYZCT => t * size_z * size_c + c * size_z + z,
        DimensionOrder::XYZTC => c * size_z * size_t + t * size_z + z,
        DimensionOrder::XYCZT => t * size_c * size_z + z * size_c + c,
        DimensionOrder::XYCTZ => z * size_c * size_t + t * size_c + c,
        DimensionOrder::XYTCZ => z * size_t * size_c + c * size_t + t,
        DimensionOrder::XYTZC => c * size_t * size_z + z * size_t + t,
    } as usize)
}

/// Advance (z, c, t) by one plane in the given dimension order. Returns false
/// when the coordinate space wraps (no more planes), mirroring
/// `advance_ome_plane` in `tiff/reader.rs`.
fn advance_ome_plane(
    z: &mut u32,
    c: &mut u32,
    t: &mut u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    order: DimensionOrder,
) -> bool {
    fn advance_axis(value: &mut u32, limit: u32) -> bool {
        *value += 1;
        if *value < limit {
            true
        } else {
            *value = 0;
            false
        }
    }
    match order {
        DimensionOrder::XYZCT => {
            advance_axis(z, size_z) || advance_axis(c, size_c) || advance_axis(t, size_t)
        }
        DimensionOrder::XYZTC => {
            advance_axis(z, size_z) || advance_axis(t, size_t) || advance_axis(c, size_c)
        }
        DimensionOrder::XYCZT => {
            advance_axis(c, size_c) || advance_axis(z, size_z) || advance_axis(t, size_t)
        }
        DimensionOrder::XYCTZ => {
            advance_axis(c, size_c) || advance_axis(t, size_t) || advance_axis(z, size_z)
        }
        DimensionOrder::XYTCZ => {
            advance_axis(t, size_t) || advance_axis(c, size_c) || advance_axis(z, size_z)
        }
        DimensionOrder::XYTZC => {
            advance_axis(t, size_t) || advance_axis(z, size_z) || advance_axis(c, size_c)
        }
    }
}

/// Resolve a `<UUID FileName="...">` reference to an absolute path relative to
/// the directory containing the `.ome` file. Returns `None` if the file does
/// not exist (mirrors Java `normalizeFilename` + existence check).
fn resolve_companion(base_dir: Option<&Path>, filename: &str) -> Option<PathBuf> {
    let trimmed = filename.trim();
    let filename_path = Path::new(trimmed);
    let (candidate, allow_basename_retry) = match base_dir {
        Some(_) if filename_path.is_absolute() => (None, true),
        Some(dir) => (confined_join(dir, trimmed), false),
        None => {
            let path = PathBuf::from(trimmed);
            if path.is_absolute()
                || filename_path
                    .components()
                    .any(|component| matches!(component, std::path::Component::ParentDir))
            {
                return None;
            }
            (Some(path), false)
        }
    };
    if let Some(candidate) = candidate {
        if candidate.exists() {
            return Some(candidate);
        }
    }
    // Old writers stored absolute paths in UUID.FileName; retry with just the
    // basename relative to the metadata directory (Java OMETiffReader).
    if allow_basename_retry {
        let (Some(dir), Some(base)) = (base_dir, filename_path.file_name()) else {
            return None;
        };
        let retry = dir.join(base);
        if retry.exists() {
            return Some(retry);
        }
    }
    None
}

/// Cheaply determine the byte order of a companion TIFF by opening it.
fn companion_is_little_endian(path: &Path) -> Result<bool> {
    let mut reader = crate::tiff::TiffReader::new();
    reader.set_id(path)?;
    Ok(reader.is_little_endian())
}

/// Read one external plane: open the companion TIFF with the crate's
/// `TiffReader` and read the IFD recorded in `plane`. The companion is a plain
/// (non-OME) TIFF, so its IFDs map sequentially onto series planes; we locate
/// the series whose `ifd_indices` contains the target IFD and read the matching
/// plane index from it.
fn read_external_plane(plane: &ExternalPlane, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
    let mut reader = crate::tiff::TiffReader::new();
    reader.set_id(&plane.path)?;

    // Find (series, plane-within-series) for the requested global IFD index.
    let mut target: Option<(usize, usize)> = None;
    for (si, s) in reader.series_list().iter().enumerate() {
        if let Some(pos) = s.ifd_indices.iter().position(|&idx| idx == plane.ifd) {
            target = Some((si, pos));
            break;
        }
    }
    // Fallback: assume IFD index == plane index in series 0 (typical companion).
    let (series_idx, plane_idx) = target.unwrap_or((0, plane.ifd));

    reader.set_series(series_idx)?;
    if x == 0 && y == 0 {
        let meta = reader.metadata();
        if w == meta.size_x && h == meta.size_y {
            return reader.open_bytes(plane_idx as u32);
        }
    }
    reader.open_bytes_region(plane_idx as u32, x, y, w, h)
}

/// Build the logical-plane -> (file, IFD) mapping for a Pixels block that
/// stores its pixels in external companion TIFFs via `<TiffData>` elements.
///
/// Faithful port of the TiffData-untangling loop in `OMETiffReader.initFile`
/// (the single-file/grouped-files branch): it honours `IFD`, `PlaneCount`,
/// `FirstZ/C/T`, the 1-indexed-coordinate workaround, and the "fill down when
/// PlaneCount is unspecified" behaviour.
fn build_external_plane_map(
    tiff_data: &[ParsedTiffData],
    base_dir: Option<&Path>,
    size_z: u32,
    eff_c: u32,
    size_t: u32,
    order: DimensionOrder,
) -> Vec<Option<ExternalPlane>> {
    let num = (size_z * eff_c * size_t) as usize;
    let mut planes: Vec<Option<ExternalPlane>> = vec![None; num];
    if num == 0 {
        return planes;
    }

    // Pre-scan: detect whether FirstZ/C/T are 1-indexed (some writers do this).
    let mut z_one_indexed: Option<bool> = None;
    let mut c_one_indexed: Option<bool> = None;
    let mut t_one_indexed: Option<bool> = None;
    for td in tiff_data {
        let (z, c, t) = (td.first_z, td.first_c, td.first_t);
        if c >= eff_c && c_one_indexed.is_none() {
            c_one_indexed = Some(true);
        } else if c == 0 {
            c_one_indexed = Some(false);
        }
        if z >= size_z && z_one_indexed.is_none() {
            z_one_indexed = Some(true);
        } else if z == 0 {
            z_one_indexed = Some(false);
        }
        if t >= size_t && t_one_indexed.is_none() {
            t_one_indexed = Some(true);
        } else if t == 0 {
            t_one_indexed = Some(false);
        }
        if c == 0 && z == 0 && t == 0 {
            break;
        }
    }

    // Track "certain" planes so that fill-down stops at explicitly-started planes.
    let mut certain = vec![false; num];
    for td in tiff_data {
        let mut z = td.first_z;
        let mut c = td.first_c;
        let mut t = td.first_t;
        if c_one_indexed == Some(true) && c > 0 {
            c -= 1;
        }
        if z_one_indexed == Some(true) && z > 0 {
            z -= 1;
        }
        if t_one_indexed == Some(true) && t > 0 {
            t -= 1;
        }
        if z >= size_z || c >= eff_c || t >= size_t {
            continue;
        }
        if let Some(index) = ome_plane_index(z, c, t, size_z, eff_c, size_t, order) {
            certain[index] = true;
        }
    }

    for td in tiff_data {
        let mut z = td.first_z;
        let mut c = td.first_c;
        let mut t = td.first_t;
        if c_one_indexed == Some(true) && c > 0 {
            c -= 1;
        }
        if z_one_indexed == Some(true) && z > 0 {
            z -= 1;
        }
        if t_one_indexed == Some(true) && t > 0 {
            t -= 1;
        }

        if z >= size_z || c >= eff_c || t >= size_t {
            // Invalid TiffData (Java logs a warning and breaks out of the loop).
            break;
        }

        let Some(index) = ome_plane_index(z, c, t, size_z, eff_c, size_t, order) else {
            break;
        };

        // Resolve the companion file for this TiffData. A missing/non-existent
        // file leaves the planes as black (None), matching the non-fail path.
        let resolved = td
            .filename
            .as_deref()
            .and_then(|f| resolve_companion(base_dir, f));

        let count = td.plane_count.unwrap_or(1);
        if count == 0 {
            // PlaneCount=0 invalidates the whole series in Java; we drop refs.
            return vec![None; num];
        }

        for q in 0..count as usize {
            let no = index + q;
            if no >= num {
                break;
            }
            certain[no] = true;
            planes[no] = resolved.as_ref().map(|p| ExternalPlane {
                path: p.clone(),
                ifd: td.ifd as usize + q,
            });
        }

        // Unknown plane count: fill down sequential IFDs until the next
        // explicitly-started ("certain") plane.
        if td.plane_count.is_none() {
            let mut prev_ifd = td.ifd as usize;
            let mut no = index + 1;
            while no < num {
                if certain[no] {
                    break;
                }
                certain[no] = true;
                prev_ifd += 1;
                planes[no] = resolved.as_ref().map(|p| ExternalPlane {
                    path: p.clone(),
                    ifd: prev_ifd,
                });
                no += 1;
            }
            // advance helper keeps z/c/t consistent for any future extension; not
            // strictly needed for IFD numbering but mirrors the Java traversal.
            let _ = advance_ome_plane(&mut z, &mut c, &mut t, size_z, eff_c, size_t, order);
        }
    }

    planes
}

fn parse_ome_xml_series_with_base(
    xml: &str,
    base_dir: Option<&Path>,
) -> Result<Vec<ParsedOmeSeries>> {
    let mut series = Vec::new();
    for image_pos in tag_positions(xml, "Image") {
        let image_end = end_tag_after(xml, image_pos, "Image");
        let image_xml = &xml[image_pos..image_end];
        let pixels_xml = match child_block(image_xml, "Pixels") {
            Some(block) => block,
            None => continue,
        };
        let pixels_tag = start_tag_at(pixels_xml, 0);

        let size_x = attr_required_nonzero_u32(pixels_tag, "SizeX")?;
        let size_y = attr_required_nonzero_u32(pixels_tag, "SizeY")?;
        let size_z = attr_required_nonzero_u32(pixels_tag, "SizeZ")?;
        let logical_c = attr_required_nonzero_u32(pixels_tag, "SizeC")?;
        let size_t = attr_required_nonzero_u32(pixels_tag, "SizeT")?;
        let type_str = xml_attr(pixels_tag, "Type")
            .or_else(|| xml_attr(pixels_tag, "type"))
            .ok_or_else(|| BioFormatsError::Format("OME-XML missing Type".into()))?;
        let (pixel_type, bpp) = pixel_type_from_attr(&type_str)?;
        let dim_order_str = xml_attr(pixels_tag, "DimensionOrder")
            .or_else(|| xml_attr(pixels_tag, "dimensionorder"))
            .ok_or_else(|| BioFormatsError::Format("OME-XML missing DimensionOrder".into()))?;
        let dim_order = dimension_order_from_attr(&dim_order_str)?;
        let samples = channel_samples_per_pixel(pixels_xml, logical_c)?;
        let max_spp = samples.iter().copied().max().unwrap_or(1);
        let is_rgb = max_spp > 1;
        let exposed_c = if is_rgb { max_spp } else { logical_c };
        // For RGB channels each plane bundles `SamplesPerPixel` samples, so the
        // number of effective (separately-addressable) channels is reduced:
        //   effectiveSizeC = SizeC / SamplesPerPixel
        // imageCount = SizeZ * SizeT * effectiveSizeC.
        let effective_c = if max_spp > 1 {
            (logical_c / max_spp).max(1)
        } else {
            logical_c
        };
        if max_spp > 1 && effective_c > 1 {
            return Err(BioFormatsError::UnsupportedFormat(
                "OME-XML: multiple logical RGB channels are not representable by ImageMetadata"
                    .into(),
            ));
        }
        let image_count = size_z
            .checked_mul(effective_c)
            .and_then(|v| v.checked_mul(size_t))
            .ok_or_else(|| BioFormatsError::Format("OME-XML plane count overflow".into()))?;

        let (planes, first_bindata_big_endian) = parse_bindata_blocks(pixels_xml)?;
        if !planes.is_empty() {
            let samples_per_plane = if is_rgb { exposed_c as usize } else { 1 };
            let plane_bytes = (size_x as usize)
                .checked_mul(size_y as usize)
                .and_then(|v| v.checked_mul(pixel_type.bytes_per_sample()))
                .and_then(|v| v.checked_mul(samples_per_plane))
                .ok_or_else(|| {
                    BioFormatsError::Format("OME-XML plane byte count overflow".into())
                })?;
            let expected_total =
                plane_bytes
                    .checked_mul(image_count as usize)
                    .ok_or_else(|| {
                        BioFormatsError::Format("OME-XML pixel byte count overflow".into())
                    })?;
            if planes.len() == 1 {
                if planes[0].len() < expected_total {
                    return Err(BioFormatsError::Format(format!(
                        "OME-XML BinData pixel payload is shorter than expected: {} < {expected_total}",
                        planes[0].len()
                    )));
                }
            } else {
                if planes.len() < image_count as usize {
                    return Err(BioFormatsError::Format(format!(
                        "OME-XML has {} BinData planes but expected {image_count}",
                        planes.len()
                    )));
                }
                for (index, plane) in planes.iter().take(image_count as usize).enumerate() {
                    if plane.len() < plane_bytes {
                        return Err(BioFormatsError::Format(format!(
                            "OME-XML BinData plane {index} is shorter than expected: {} < {plane_bytes}",
                            plane.len()
                        )));
                    }
                }
            }
        }
        let pixels_big_endian =
            xml_attr(pixels_tag, "BigEndian").or_else(|| xml_attr(pixels_tag, "bigendian"));
        let mut is_big_endian = pixels_big_endian
            .or(first_bindata_big_endian)
            .map(|s| s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        // When there is no inline pixel data, attempt to resolve external
        // pixels referenced via <TiffData>/<UUID FileName="..."> (the OME-TIFF
        // companion case handled by Java's OMETiffReader). A <MetadataOnly>
        // element with no resolvable TiffData yields black/absent planes.
        let mut external_planes: Vec<Option<ExternalPlane>> = Vec::new();
        let inline_pixels_present = planes.iter().any(|p| !p.is_empty());
        if !inline_pixels_present {
            let tiff_data = parse_tiff_data(pixels_xml);
            if !tiff_data.is_empty() {
                for td in &tiff_data {
                    if let Some(filename) = td.filename.as_deref() {
                        if resolve_companion(base_dir, filename).is_none() {
                            return Err(BioFormatsError::Format(format!(
                                "OME-XML companion TIFF not found: {filename}"
                            )));
                        }
                    }
                }
                external_planes = build_external_plane_map(
                    &tiff_data,
                    base_dir,
                    size_z,
                    effective_c,
                    size_t,
                    dim_order,
                );
                // Mirror OMETiffReader: pixel endianness follows the first
                // resolvable companion TIFF's byte order, not the OME-XML
                // BigEndian attribute (which OME-TIFF forces true for BinData).
                if let Some(first) = external_planes.iter().flatten().next() {
                    if let Ok(le) = companion_is_little_endian(&first.path) {
                        is_big_endian = !le;
                    }
                }
            } else if child_block(pixels_xml, "MetadataOnly").is_some() {
                // <MetadataOnly/> declares planes that exist but carry no pixel
                // source; Java returns blank planes for these. Mark the whole
                // image as external-with-no-files so open_bytes yields zeros.
                external_planes = vec![None; image_count as usize];
            }
        }

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("OME-XML".into()));
        series.push(ParsedOmeSeries {
            meta: ImageMetadata {
                size_x,
                size_y,
                size_z,
                size_c: exposed_c,
                size_t,
                pixel_type,
                bits_per_pixel: bpp,
                image_count,
                dimension_order: dim_order,
                is_rgb,
                is_interleaved: is_rgb,
                is_indexed: false,
                is_little_endian: !is_big_endian,
                resolution_count: 1,
                thumbnail: false,
                series_metadata: meta_map,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            },
            planes,
            external_planes,
        });
    }

    if series.is_empty() {
        return Err(BioFormatsError::Format(
            "OME-XML: no <Pixels> element".into(),
        ));
    }
    Ok(series)
}

pub struct OmeXmlReader {
    path: Option<PathBuf>,
    series: Vec<ParsedOmeSeries>,
    current_series: usize,
    raw_xml: Option<String>,
}

impl OmeXmlReader {
    pub fn new() -> Self {
        OmeXmlReader {
            path: None,
            series: Vec::new(),
            current_series: 0,
            raw_xml: None,
        }
    }
}
impl Default for OmeXmlReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for OmeXmlReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_ascii_lowercase());
        matches!(name.as_deref(), Some(n) if n.ends_with(".ome") || n.ends_with(".ome.xml"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        let s = std::str::from_utf8(&header[..header.len().min(128)]).unwrap_or("");
        (s.contains("<?xml") || s.starts_with('<')) && s.contains("OME")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let xml = fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        // Companion TIFFs referenced from <UUID FileName="..."> are resolved
        // relative to the directory containing the .ome file.
        let base_dir = path.parent();
        let series = parse_ome_xml_series_with_base(&xml, base_dir)?;
        self.raw_xml = Some(xml.clone());
        self.series = series;
        self.current_series = 0;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series.clear();
        self.current_series = 0;
        self.raw_xml = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.series.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_count() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }
    fn series(&self) -> usize {
        self.current_series
    }
    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current_series)
            .map(|series| &series.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let series = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = &series.meta;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let samples = if meta.is_rgb { meta.size_c as usize } else { 1 };
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps * samples;

        // External pixels referenced via <TiffData> companion TIFFs.
        if !series.external_planes.is_empty() {
            match series.external_planes.get(plane_index as usize) {
                Some(Some(ext)) => return read_external_plane(ext, 0, 0, meta.size_x, meta.size_y),
                // Missing/unresolved plane: return a black plane (Java fills the
                // buffer with the fill color for non-existent planes).
                _ => return Ok(vec![0u8; plane_bytes]),
            }
        }

        if let Some(plane) = series.planes.get(plane_index as usize) {
            if series.planes.len() > 1 || plane.len() == plane_bytes {
                return Ok(plane.clone());
            }
        }
        // If single BinData block contains all planes, slice it
        if !series.planes.is_empty() {
            let offset = plane_index as usize * plane_bytes;
            let src = &series.planes[0];
            if offset + plane_bytes <= src.len() {
                return Ok(src[offset..offset + plane_bytes].to_vec());
            }
        }
        Err(BioFormatsError::PlaneOutOfRange(plane_index))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        // For external companion-TIFF planes, read the cropped region directly
        // from the TIFF so we don't materialise the whole plane first.
        {
            let series = self
                .series
                .get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized)?;
            if !series.external_planes.is_empty() {
                let meta = &series.meta;
                if plane_index >= meta.image_count {
                    return Err(BioFormatsError::PlaneOutOfRange(plane_index));
                }
                match series.external_planes.get(plane_index as usize) {
                    Some(Some(ext)) => {
                        let ext = ext.clone();
                        return read_external_plane(&ext, x, y, w, h);
                    }
                    _ => {
                        validate_region("OME-XML", meta.size_x, meta.size_y, x, y, w, h)?;
                        let bps = meta.pixel_type.bytes_per_sample();
                        let samples = if meta.is_rgb { meta.size_c as usize } else { 1 };
                        return Ok(vec![0u8; w as usize * h as usize * bps * samples]);
                    }
                }
            }
        }
        let full = self.open_bytes(plane_index)?;
        let meta = self.metadata();
        let samples = if meta.is_rgb { meta.size_c as usize } else { 1 };
        crop_full_plane("OME-XML", &full, meta, samples, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.metadata();
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        self.raw_xml
            .as_deref()
            .map(crate::common::ome_metadata::OmeMetadata::from_ome_xml)
    }
}

// ═══════════════════════════════════════════════════════════════════════════════
// OME-XML Writer
// ═══════════════════════════════════════════════════════════════════════════════

/// Base64 encoder (standard alphabet, with padding).
fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((triple >> 18) & 0x3F) as usize] as char);
        out.push(CHARS[((triple >> 12) & 0x3F) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((triple >> 6) & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(triple & 0x3F) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn xml_escape_attr(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
}

/// OME-XML standalone writer (`.ome` files with Base64-encoded pixel data).
pub struct OmeXmlWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
    ome: Option<crate::common::ome_metadata::OmeMetadata>,
}

impl OmeXmlWriter {
    pub fn new() -> Self {
        OmeXmlWriter {
            path: None,
            meta: None,
            planes: Vec::new(),
            ome: None,
        }
    }

    /// Set optional OME metadata (channels, physical sizes, etc.).
    pub fn set_ome_metadata(&mut self, ome: crate::common::ome_metadata::OmeMetadata) {
        self.ome = Some(ome);
    }
}

impl Default for OmeXmlWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl crate::common::writer::FormatWriter for OmeXmlWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_ascii_lowercase())
            .unwrap_or_default();
        name.ends_with(".ome") || name.ends_with(".ome.xml")
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        self.meta = Some(meta.clone());
        self.planes.clear();
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_next_plane(
            "OME-XML",
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
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_complete("OME-XML", &meta, self.planes.len())?;

        let mut ome = self
            .ome
            .clone()
            .unwrap_or_else(|| crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta));
        ome.populate_pixels(meta, 0)?;
        ome.verify_minimum_populated(meta, 0)?;

        // Build OME-XML with inline BinData
        use std::fmt::Write;
        let mut xml = String::new();
        let _ = write!(xml, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        let _ = write!(
            xml,
            r#"<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">"#
        );

        let pt_str = match meta.pixel_type {
            PixelType::Bit => "bit",
            PixelType::Int8 => "int8",
            PixelType::Uint8 => "uint8",
            PixelType::Int16 => "int16",
            PixelType::Uint16 => "uint16",
            PixelType::Int32 => "int32",
            PixelType::Uint32 => "uint32",
            PixelType::Float32 => "float",
            PixelType::Float64 => "double",
        };
        let dim_order = format!("{:?}", meta.dimension_order);

        // Image element
        let img_name = ome
            .images
            .first()
            .and_then(|i| i.name.as_deref())
            .unwrap_or("Image 0");
        let _ = write!(
            xml,
            r#"<Image ID="Image:0" Name="{}">"#,
            xml_escape_attr(img_name)
        );
        let _ = write!(
            xml,
            r#"<Pixels ID="Pixels:0" DimensionOrder="{dim_order}" Type="{pt_str}" SizeX="{}" SizeY="{}" SizeZ="{}" SizeC="{}" SizeT="{}" BigEndian="{}">"#,
            meta.size_x, meta.size_y, meta.size_z, meta.size_c, meta.size_t, !meta.is_little_endian
        );

        // Channels
        if let Some(img) = ome.images.first() {
            for (ci, ch) in img.channels.iter().enumerate() {
                let _ = write!(
                    xml,
                    r#"<Channel ID="Channel:0:{ci}" SamplesPerPixel="{}""#,
                    ch.samples_per_pixel
                );
                if let Some(name) = &ch.name {
                    let _ = write!(xml, r#" Name="{}""#, xml_escape_attr(name));
                }
                xml.push_str("/>");
            }
        }

        // BinData for each plane
        for plane in &self.planes {
            let b64 = base64_encode(plane);
            let _ = write!(
                xml,
                r#"<BinData xmlns="http://www.openmicroscopy.org/Schemas/BinaryFile/2016-06" Length="{}" BigEndian="{}">{}</BinData>"#,
                plane.len(),
                !meta.is_little_endian,
                b64
            );
        }

        xml.push_str("</Pixels></Image></OME>");

        fs::write(path, xml.as_bytes()).map_err(BioFormatsError::Io)?;
        self.meta = None;
        self.path = None;
        self.ome = None;
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
        std::env::temp_dir().join(format!("bioformats_ome_{nanos}_{name}"))
    }

    #[test]
    fn parses_tiffdata_with_uuid_filename() {
        let pixels = r#"<Pixels SizeX="4" SizeY="4" SizeZ="2" SizeC="1" SizeT="1" Type="uint8" DimensionOrder="XYZCT">
            <TiffData IFD="0" PlaneCount="1" FirstZ="0" FirstC="0" FirstT="0">
              <UUID FileName="a.tiff">urn:uuid:1111</UUID>
            </TiffData>
            <TiffData IFD="3" PlaneCount="1" FirstZ="1" FirstC="0" FirstT="0">
              <UUID FileName="b.tiff">urn:uuid:2222</UUID>
            </TiffData>
        </Pixels>"#;
        let td = parse_tiff_data(pixels);
        assert_eq!(td.len(), 2);
        assert_eq!(td[0].ifd, 0);
        assert_eq!(td[0].plane_count, Some(1));
        assert_eq!(td[0].filename.as_deref(), Some("a.tiff"));
        assert_eq!(td[1].ifd, 3);
        assert_eq!(td[1].first_z, 1);
        assert_eq!(td[1].filename.as_deref(), Some("b.tiff"));
    }

    #[test]
    fn external_plane_map_resolves_missing_files_to_black() {
        // Files don't exist on disk -> entries resolve to None (black planes),
        // but plane slots and IFD numbering still follow the TiffData layout.
        let td = vec![
            ParsedTiffData {
                ifd: 0,
                plane_count: Some(1),
                first_z: 0,
                first_c: 0,
                first_t: 0,
                filename: Some("does_not_exist_a.tiff".into()),
            },
            ParsedTiffData {
                ifd: 0,
                plane_count: Some(1),
                first_z: 1,
                first_c: 0,
                first_t: 0,
                filename: Some("does_not_exist_b.tiff".into()),
            },
        ];
        let map = build_external_plane_map(&td, None, 2, 1, 1, DimensionOrder::XYZCT);
        assert_eq!(map.len(), 2);
        // Both unresolved (files absent) but the mapping was attempted for both.
        assert!(map[0].is_none());
        assert!(map[1].is_none());
    }

    #[test]
    fn external_plane_map_does_not_basename_retry_relative_escape() {
        let dir = temp_path("escape_companions");
        std::fs::create_dir_all(&dir).unwrap();
        let companion = dir.join("plane.tif");
        std::fs::write(&companion, b"not used by map test").unwrap();

        let td = vec![ParsedTiffData {
            ifd: 0,
            plane_count: Some(1),
            first_z: 0,
            first_c: 0,
            first_t: 0,
            filename: Some("../plane.tif".into()),
        }];

        let map = build_external_plane_map(&td, Some(&dir), 1, 1, 1, DimensionOrder::XYZCT);

        assert_eq!(map.len(), 1);
        assert!(map[0].is_none());
        let _ = std::fs::remove_file(companion);
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn external_plane_map_basename_retries_absolute_legacy_path() {
        let dir = temp_path("absolute_companions");
        std::fs::create_dir_all(&dir).unwrap();
        let companion = dir.join("plane.tif");
        std::fs::write(&companion, b"not used by map test").unwrap();

        let td = vec![ParsedTiffData {
            ifd: 0,
            plane_count: Some(1),
            first_z: 0,
            first_c: 0,
            first_t: 0,
            filename: Some("/old/location/plane.tif".into()),
        }];

        let map = build_external_plane_map(&td, Some(&dir), 1, 1, 1, DimensionOrder::XYZCT);

        assert_eq!(map.len(), 1);
        assert_eq!(
            map[0].as_ref().map(|p| p.path.as_path()),
            Some(companion.as_path())
        );
        let _ = std::fs::remove_file(companion);
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn external_plane_map_fills_down_when_planecount_absent() {
        // Single TiffData with no PlaneCount should fill IFDs 0..N sequentially.
        let td = vec![ParsedTiffData {
            ifd: 0,
            plane_count: None,
            first_z: 0,
            first_c: 0,
            first_t: 0,
            filename: None,
        }];
        // filename is None -> resolved is None, so plane entries are None, but
        // the fill-down still marks all planes (as black). Verify length only.
        let map = build_external_plane_map(&td, None, 3, 1, 1, DimensionOrder::XYZCT);
        assert_eq!(map.len(), 3);
    }

    #[test]
    fn external_plane_map_fill_down_stops_at_later_explicit_start() {
        let dir = temp_path("companions");
        std::fs::create_dir_all(&dir).unwrap();
        let companion = dir.join("plane.tif");
        std::fs::write(&companion, b"not used by map test").unwrap();

        let td = vec![
            ParsedTiffData {
                ifd: 0,
                plane_count: None,
                first_z: 0,
                first_c: 0,
                first_t: 0,
                filename: Some("plane.tif".into()),
            },
            ParsedTiffData {
                ifd: 10,
                plane_count: Some(1),
                first_z: 0,
                first_c: 2,
                first_t: 0,
                filename: Some("plane.tif".into()),
            },
        ];

        let map = build_external_plane_map(&td, Some(&dir), 1, 4, 1, DimensionOrder::XYZCT);

        assert_eq!(map[0].as_ref().map(|p| p.ifd), Some(0));
        assert_eq!(map[1].as_ref().map(|p| p.ifd), Some(1));
        assert_eq!(map[2].as_ref().map(|p| p.ifd), Some(10));
        assert!(map[3].is_none());
        let _ = std::fs::remove_file(companion);
        let _ = std::fs::remove_dir(dir);
    }

    #[test]
    fn inline_rgb_region_uses_rgb_stride() {
        let path = temp_path("rgb.ome");
        let xml = r#"<OME><Image ID="Image:0"><Pixels ID="Pixels:0" Name="quoted > delimiter" DimensionOrder="XYZCT" Type="uint8" SizeX="2" SizeY="2" SizeZ="1" SizeC="3" SizeT="1"><Channel ID="Channel:0:0" SamplesPerPixel="3"/><BinData BigEndian="false">AQIDBAUGBwgJCgsM</BinData></Pixels></Image></OME>"#;
        std::fs::write(&path, xml).unwrap();

        let mut reader = OmeXmlReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
            vec![4, 5, 6, 10, 11, 12]
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn missing_external_companion_is_rejected_before_metadata() {
        let path = temp_path("missing_external.ome");
        let xml = r#"<OME><Image ID="Image:0"><Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint8" SizeX="2" SizeY="2" SizeZ="1" SizeC="1" SizeT="1"><TiffData IFD="0" PlaneCount="1"><UUID FileName="missing.tif">urn:uuid:missing</UUID></TiffData></Pixels></Image></OME>"#;
        std::fs::write(&path, xml).unwrap();

        let mut reader = OmeXmlReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            err.to_string().contains("companion TIFF not found"),
            "{err:?}"
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ome_plane_index_matches_dimension_order() {
        // XYZCT: index = t*Z*C + c*Z + z
        assert_eq!(
            ome_plane_index(1, 0, 0, 3, 2, 4, DimensionOrder::XYZCT),
            Some(1)
        );
        assert_eq!(
            ome_plane_index(0, 1, 0, 3, 2, 4, DimensionOrder::XYZCT),
            Some(3)
        );
        // Out of range -> None
        assert_eq!(
            ome_plane_index(3, 0, 0, 3, 2, 4, DimensionOrder::XYZCT),
            None
        );
    }
}
