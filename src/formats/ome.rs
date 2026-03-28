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
    let input: Vec<u8> = input.bytes().filter(|&b| !b.is_ascii_whitespace()).collect();
    let n = input.len();
    if n == 0 { return vec![]; }
    let mut out = Vec::with_capacity((n / 4) * 3 + 3);
    let mut i = 0;
    while i + 3 < n {
        let a = B64_TABLE[input[i]   as usize];
        let b = B64_TABLE[input[i+1] as usize];
        let c = B64_TABLE[input[i+2] as usize];
        let d = B64_TABLE[input[i+3] as usize];
        if a == 255 || b == 255 { break; }
        out.push((a << 2) | (b >> 4));
        if input[i+2] != b'=' && c != 255 {
            out.push((b << 4) | (c >> 2));
        }
        if input[i+3] != b'=' && d != 255 {
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
        let end = rest.find(|c: char| c.is_whitespace() || c == '>').unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

fn parse_ome_xml(xml: &str) -> Result<(u32, u32, u32, u32, u32, PixelType, u8, bool, DimensionOrder, Vec<Vec<u8>>)> {
    // Find the <Pixels ...> tag
    let lower = xml.to_ascii_lowercase();
    let pixels_start = lower.find("<pixels").ok_or_else(||
        BioFormatsError::Format("OME-XML: no <Pixels> element".into()))?;
    let tag_end = xml[pixels_start..].find('>').unwrap_or(xml.len() - pixels_start);
    let pixels_tag = &xml[pixels_start..pixels_start + tag_end + 1];

    let size_x = xml_attr(pixels_tag, "SizeX").or_else(|| xml_attr(pixels_tag, "sizex"))
        .and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).max(1);
    let size_y = xml_attr(pixels_tag, "SizeY").or_else(|| xml_attr(pixels_tag, "sizey"))
        .and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).max(1);
    let size_z = xml_attr(pixels_tag, "SizeZ").or_else(|| xml_attr(pixels_tag, "sizez"))
        .and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).max(1);
    let size_c = xml_attr(pixels_tag, "SizeC").or_else(|| xml_attr(pixels_tag, "sizec"))
        .and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).max(1);
    let size_t = xml_attr(pixels_tag, "SizeT").or_else(|| xml_attr(pixels_tag, "sizet"))
        .and_then(|s| s.parse::<u32>().ok()).unwrap_or(1).max(1);

    let type_str = xml_attr(pixels_tag, "Type").or_else(|| xml_attr(pixels_tag, "type"))
        .unwrap_or_else(|| "uint8".into());
    let big_endian_str = xml_attr(pixels_tag, "BigEndian").or_else(|| xml_attr(pixels_tag, "bigendian"))
        .unwrap_or_else(|| "false".into());
    let is_big_endian = big_endian_str.eq_ignore_ascii_case("true");

    let dim_order_str = xml_attr(pixels_tag, "DimensionOrder")
        .or_else(|| xml_attr(pixels_tag, "dimensionorder"))
        .unwrap_or_else(|| "XYZCT".into());
    let dim_order = match dim_order_str.to_ascii_uppercase().as_str() {
        "XYZCT" => DimensionOrder::XYZCT,
        "XYZTC" => DimensionOrder::XYZTC,
        "XYCZT" => DimensionOrder::XYCZT,
        "XYCTZ" => DimensionOrder::XYCTZ,
        "XYTZC" => DimensionOrder::XYTZC,
        "XYTCZ" => DimensionOrder::XYTCZ,
        _       => DimensionOrder::XYZCT,
    };

    let (pixel_type, bpp): (PixelType, u8) = match type_str.to_ascii_lowercase().as_str() {
        "int8"                => (PixelType::Uint8,   8),
        "uint8"               => (PixelType::Uint8,   8),
        "int16"               => (PixelType::Int16,  16),
        "uint16"              => (PixelType::Uint16, 16),
        "int32"               => (PixelType::Int32,  32),
        "uint32"              => (PixelType::Uint32, 32),
        "float"  | "float32"  => (PixelType::Float32, 32),
        "double" | "float64"  => (PixelType::Float64, 64),
        _                     => (PixelType::Uint8,   8),
    };

    // Collect all <BinData> blocks
    let mut planes: Vec<Vec<u8>> = Vec::new();
    let mut search_start = pixels_start;
    loop {
        let lo_tail = &lower[search_start..];
        let bd_rel = match lo_tail.find("<bindata") {
            Some(p) => p,
            None => break,
        };
        let bd_abs = search_start + bd_rel;

        // Find where the tag ends (could be <BinData ...>DATA</BinData>)
        let tag_end_rel = xml[bd_abs..].find('>').unwrap_or(0);
        let content_start = bd_abs + tag_end_rel + 1;

        // Find </BinData>
        let close_rel = lower[content_start..].find("</bindata>").unwrap_or(0);
        let b64_text = &xml[content_start..content_start + close_rel];
        planes.push(base64_decode(b64_text));

        search_start = content_start + close_rel + 10; // skip past </BinData>
        if search_start >= xml.len() { break; }

        // Stop at </Pixels>
        if lower[search_start..].find("</pixels>").map(|p| p < lower[search_start..].find("<bindata").unwrap_or(usize::MAX)).unwrap_or(false) {
            break;
        }
    }

    Ok((size_x, size_y, size_z, size_c, size_t, pixel_type, bpp, !is_big_endian, dim_order, planes))
}

pub struct OmeXmlReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
    raw_xml: Option<String>,
}

impl OmeXmlReader {
    pub fn new() -> Self { OmeXmlReader { path: None, meta: None, planes: Vec::new(), raw_xml: None } }
}
impl Default for OmeXmlReader { fn default() -> Self { Self::new() } }

impl FormatReader for OmeXmlReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ome"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        let s = std::str::from_utf8(&header[..header.len().min(128)]).unwrap_or("");
        (s.contains("<?xml") || s.starts_with('<')) && s.contains("OME")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let xml = fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let (size_x, size_y, size_z, size_c, size_t, pixel_type, bpp, little_endian, dim_order, planes)
            = parse_ome_xml(&xml)?;
        self.raw_xml = Some(xml.clone());

        let image_count = size_z * size_c * size_t;
        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("OME-XML".into()));

        self.meta = Some(ImageMetadata {
            size_x, size_y, size_z, size_c, size_t,
            pixel_type, bits_per_pixel: bpp,
            image_count,
            dimension_order: dim_order,
            is_rgb: false, is_interleaved: false, is_indexed: false,
            is_little_endian: little_endian,
            resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
        });
        self.planes = planes;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> { self.path = None; self.meta = None; self.planes.clear(); self.raw_xml = None; Ok(()) }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        if let Some(plane) = self.planes.get(plane_index as usize) {
            return Ok(plane.clone());
        }
        // If single BinData block contains all planes, slice it
        if !self.planes.is_empty() {
            let bps = meta.pixel_type.bytes_per_sample();
            let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
            let offset = plane_index as usize * plane_bytes;
            let src = &self.planes[0];
            if offset + plane_bytes <= src.len() {
                return Ok(src[offset..offset + plane_bytes].to_vec());
            }
        }
        Err(BioFormatsError::PlaneOutOfRange(plane_index))
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize*bps .. x as usize*bps + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        self.raw_xml.as_deref().map(crate::common::ome_metadata::OmeMetadata::from_ome_xml)
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

/// OME-XML standalone writer (`.ome` files with Base64-encoded pixel data).
pub struct OmeXmlWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
    ome: Option<crate::common::ome_metadata::OmeMetadata>,
}

impl OmeXmlWriter {
    pub fn new() -> Self {
        OmeXmlWriter { path: None, meta: None, planes: Vec::new(), ome: None }
    }

    /// Set optional OME metadata (channels, physical sizes, etc.).
    pub fn set_ome_metadata(&mut self, ome: crate::common::ome_metadata::OmeMetadata) {
        self.ome = Some(ome);
    }
}

impl Default for OmeXmlWriter {
    fn default() -> Self { Self::new() }
}

impl crate::common::writer::FormatWriter for OmeXmlWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        let name = path.file_name().and_then(|n| n.to_str())
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
        let _ = write!(xml, r#"<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06">"#);

        let pt_str = match meta.pixel_type {
            PixelType::Bit => "bit", PixelType::Int8 => "int8", PixelType::Uint8 => "uint8",
            PixelType::Int16 => "int16", PixelType::Uint16 => "uint16",
            PixelType::Int32 => "int32", PixelType::Uint32 => "uint32",
            PixelType::Float32 => "float", PixelType::Float64 => "double",
        };
        let dim_order = format!("{:?}", meta.dimension_order);

        // Image element
        let img_name = ome.images.first()
            .and_then(|i| i.name.as_deref())
            .unwrap_or("Image 0");
        let _ = write!(xml, r#"<Image ID="Image:0" Name="{img_name}">"#);
        let _ = write!(xml,
            r#"<Pixels ID="Pixels:0" DimensionOrder="{dim_order}" Type="{pt_str}" SizeX="{}" SizeY="{}" SizeZ="{}" SizeC="{}" SizeT="{}" BigEndian="{}">"#,
            meta.size_x, meta.size_y, meta.size_z, meta.size_c, meta.size_t,
            !meta.is_little_endian);

        // Channels
        if let Some(img) = ome.images.first() {
            for (ci, ch) in img.channels.iter().enumerate() {
                let _ = write!(xml, r#"<Channel ID="Channel:0:{ci}" SamplesPerPixel="{}"/>"#,
                    ch.samples_per_pixel);
            }
        }

        // BinData for each plane
        for plane in &self.planes {
            let b64 = base64_encode(plane);
            let _ = write!(xml,
                r#"<BinData xmlns="http://www.openmicroscopy.org/Schemas/BinaryFile/2016-06" Length="{}" BigEndian="{}">{}</BinData>"#,
                plane.len(), !meta.is_little_endian, b64);
        }

        xml.push_str("</Pixels></Image></OME>");

        fs::write(&path, xml.as_bytes()).map_err(BioFormatsError::Io)?;
        self.planes.clear();
        Ok(())
    }

    fn can_do_stacks(&self) -> bool { true }
}
