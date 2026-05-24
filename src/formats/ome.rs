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
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

struct ParsedOmeSeries {
    meta: ImageMetadata,
    planes: Vec<Vec<u8>>,
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
    let pos = tag.find(&needle)?;
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
    let end = xml[pos..]
        .find('>')
        .map(|e| pos + e + 1)
        .unwrap_or(xml.len());
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

fn attr_u32(tag: &str, attr: &str, default: u32) -> u32 {
    xml_attr(tag, attr)
        .or_else(|| xml_attr(tag, &attr.to_ascii_lowercase()))
        .and_then(|s| s.parse::<u32>().ok())
        .unwrap_or(default)
        .max(1)
}

fn dimension_order_from_attr(value: &str) -> DimensionOrder {
    match value.to_ascii_uppercase().as_str() {
        "XYZCT" => DimensionOrder::XYZCT,
        "XYZTC" => DimensionOrder::XYZTC,
        "XYCZT" => DimensionOrder::XYCZT,
        "XYCTZ" => DimensionOrder::XYCTZ,
        "XYTZC" => DimensionOrder::XYTZC,
        "XYTCZ" => DimensionOrder::XYTCZ,
        _ => DimensionOrder::XYZCT,
    }
}

fn pixel_type_from_attr(value: &str) -> (PixelType, u8) {
    match value.to_ascii_lowercase().as_str() {
        "int8" => (PixelType::Int8, 8),
        "uint8" => (PixelType::Uint8, 8),
        "int16" => (PixelType::Int16, 16),
        "uint16" => (PixelType::Uint16, 16),
        "int32" => (PixelType::Int32, 32),
        "uint32" => (PixelType::Uint32, 32),
        "float" | "float32" => (PixelType::Float32, 32),
        "double" | "float64" => (PixelType::Float64, 64),
        _ => (PixelType::Uint8, 8),
    }
}

fn channel_samples_per_pixel(pixels_xml: &str, size_c: u32) -> Vec<u32> {
    let mut samples = Vec::new();
    for pos in tag_positions(pixels_xml, "Channel") {
        let tag = start_tag_at(pixels_xml, pos);
        samples.push(
            xml_attr(tag, "SamplesPerPixel")
                .and_then(|s| s.parse::<u32>().ok())
                .unwrap_or(1)
                .max(1),
        );
    }
    while samples.len() < size_c as usize {
        samples.push(1);
    }
    samples.truncate(size_c as usize);
    samples
}

fn parse_bindata_blocks(pixels_xml: &str) -> (Vec<Vec<u8>>, Option<String>) {
    let mut blocks = Vec::new();
    let mut first_big_endian = None;
    for pos in tag_positions(pixels_xml, "BinData") {
        let tag = start_tag_at(pixels_xml, pos);
        if first_big_endian.is_none() {
            first_big_endian = xml_attr(tag, "BigEndian").or_else(|| xml_attr(tag, "bigendian"));
        }
        let content_start = tag
            .find('>')
            .map(|e| pos + e + 1)
            .unwrap_or(pos + tag.len());
        let content_end = end_tag_start_after(pixels_xml, pos, "BinData").unwrap_or(content_start);
        let b64_text = pixels_xml.get(content_start..content_end).unwrap_or("");
        blocks.push(base64_decode(b64_text));
    }
    (blocks, first_big_endian)
}

fn parse_ome_xml_series(xml: &str) -> Result<Vec<ParsedOmeSeries>> {
    let mut series = Vec::new();
    for image_pos in tag_positions(xml, "Image") {
        let image_end = end_tag_after(xml, image_pos, "Image");
        let image_xml = &xml[image_pos..image_end];
        let pixels_xml = match child_block(image_xml, "Pixels") {
            Some(block) => block,
            None => continue,
        };
        let pixels_tag = start_tag_at(pixels_xml, 0);

        let size_x = attr_u32(pixels_tag, "SizeX", 1);
        let size_y = attr_u32(pixels_tag, "SizeY", 1);
        let size_z = attr_u32(pixels_tag, "SizeZ", 1);
        let logical_c = attr_u32(pixels_tag, "SizeC", 1);
        let size_t = attr_u32(pixels_tag, "SizeT", 1);
        let type_str = xml_attr(pixels_tag, "Type")
            .or_else(|| xml_attr(pixels_tag, "type"))
            .unwrap_or_else(|| "uint8".into());
        let (pixel_type, bpp) = pixel_type_from_attr(&type_str);
        let dim_order_str = xml_attr(pixels_tag, "DimensionOrder")
            .or_else(|| xml_attr(pixels_tag, "dimensionorder"))
            .unwrap_or_else(|| "XYZCT".into());
        let dim_order = dimension_order_from_attr(&dim_order_str);
        let samples = channel_samples_per_pixel(pixels_xml, logical_c);
        let max_spp = samples.iter().copied().max().unwrap_or(1);
        let is_rgb = max_spp > 1;
        let exposed_c = if is_rgb { max_spp } else { logical_c };
        let image_count = size_z
            .checked_mul(logical_c)
            .and_then(|v| v.checked_mul(size_t))
            .ok_or_else(|| BioFormatsError::Format("OME-XML plane count overflow".into()))?;

        let (planes, first_bindata_big_endian) = parse_bindata_blocks(pixels_xml);
        let pixels_big_endian =
            xml_attr(pixels_tag, "BigEndian").or_else(|| xml_attr(pixels_tag, "bigendian"));
        let is_big_endian = pixels_big_endian
            .or(first_bindata_big_endian)
            .map(|s| s.eq_ignore_ascii_case("true"))
            .unwrap_or(false);

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
                series_metadata: meta_map,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            },
            planes,
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
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ome"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        let s = std::str::from_utf8(&header[..header.len().min(128)]).unwrap_or("");
        (s.contains("<?xml") || s.starts_with('<')) && s.contains("OME")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let xml = fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let series = parse_ome_xml_series(&xml)?;
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
        self.series.len().max(1)
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
        &self.series[self.current_series].meta
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
        let full = self.open_bytes(plane_index)?;
        let meta = self.metadata();
        crop_full_plane("OME-XML", &full, meta, 1, x, y, w, h)
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

    fn save_bytes(&mut self, _plane_index: u32, data: &[u8]) -> Result<()> {
        self.planes.push(data.to_vec());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.take().ok_or(BioFormatsError::NotInitialized)?;

        let ome = self.ome.take().unwrap_or_else(|| {
            crate::common::ome_metadata::OmeMetadata::from_image_metadata(&meta)
        });

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

        fs::write(&path, xml.as_bytes()).map_err(BioFormatsError::Io)?;
        self.planes.clear();
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        true
    }
}
