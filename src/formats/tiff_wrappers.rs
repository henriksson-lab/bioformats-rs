//! Thin TIFF-wrapper readers for formats that are TIFF-based but identified
//! only by file extension (no distinct magic bytes beyond TIFF itself).
//!
//! All readers delegate all pixel / metadata work to `crate::tiff::TiffReader`.

use std::path::Path;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::reader::FormatReader;
use crate::common::region::validate_region;

// ---------------------------------------------------------------------------
// Minimal XML helpers (shared by the Leica SCN, Ventana, XLEF ports)
// ---------------------------------------------------------------------------
/// A very small XML start-tag scanner sufficient for the attribute-driven
/// descriptors used by these microscopy formats. It is NOT a general XML
/// parser: it only locates start tags and extracts their attributes. CDATA
/// between tags is captured separately by `xml_element_text`.
#[derive(Debug, Clone)]
struct XmlTag {
    name: String,
    attrs: std::collections::HashMap<String, String>,
    /// Byte offset just after the `>` of this start tag.
    body_start: usize,
    /// True if this was a self-closing `<foo/>` tag.
    self_closing: bool,
}

/// Iterate over all start tags (including self-closing) in document order.
fn xml_scan_tags(xml: &str) -> Vec<XmlTag> {
    let bytes = xml.as_bytes();
    let mut tags = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        // Skip comments, declarations, closing tags, processing instructions.
        if xml[i..].starts_with("<!--") {
            if let Some(end) = xml[i..].find("-->") {
                i += end + 3;
            } else {
                break;
            }
            continue;
        }
        if bytes.get(i + 1) == Some(&b'/')
            || bytes.get(i + 1) == Some(&b'?')
            || bytes.get(i + 1) == Some(&b'!')
        {
            if let Some(end) = xml[i..].find('>') {
                i += end + 1;
            } else {
                break;
            }
            continue;
        }
        // Find end of this start tag, respecting quotes.
        let mut j = i + 1;
        let mut in_quote = 0u8;
        while j < bytes.len() {
            let c = bytes[j];
            if in_quote != 0 {
                if c == in_quote {
                    in_quote = 0;
                }
            } else if c == b'"' || c == b'\'' {
                in_quote = c;
            } else if c == b'>' {
                break;
            }
            j += 1;
        }
        if j >= bytes.len() {
            break;
        }
        let inner = &xml[i + 1..j];
        let self_closing = inner.trim_end().ends_with('/');
        let inner_trim = inner.trim_end().trim_end_matches('/');
        // Tag name is up to first whitespace.
        let name_end = inner_trim
            .find(|c: char| c.is_whitespace())
            .unwrap_or(inner_trim.len());
        let name = inner_trim[..name_end].to_string();
        let attrs = xml_parse_attrs(&inner_trim[name_end..]);
        tags.push(XmlTag {
            name,
            attrs,
            body_start: j + 1,
            self_closing,
        });
        i = j + 1;
    }
    tags
}

/// Parse `key="value"` / `key='value'` attribute pairs from a fragment.
fn xml_parse_attrs(s: &str) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let bytes = s.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let key_start = i;
        while i < bytes.len() && bytes[i] != b'=' && !(bytes[i] as char).is_whitespace() {
            i += 1;
        }
        let key = s[key_start..i].trim().to_string();
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() || bytes[i] != b'=' {
            if key.is_empty() {
                break;
            }
            continue;
        }
        i += 1; // skip '='
        while i < bytes.len() && (bytes[i] as char).is_whitespace() {
            i += 1;
        }
        if i >= bytes.len() {
            break;
        }
        let quote = bytes[i];
        if quote == b'"' || quote == b'\'' {
            i += 1;
            let val_start = i;
            while i < bytes.len() && bytes[i] != quote {
                i += 1;
            }
            let val = xml_unescape(&s[val_start..i]);
            if !key.is_empty() {
                map.insert(key, val);
            }
            i += 1;
        } else {
            let val_start = i;
            while i < bytes.len() && !(bytes[i] as char).is_whitespace() {
                i += 1;
            }
            if !key.is_empty() {
                map.insert(key, xml_unescape(&s[val_start..i]));
            }
        }
    }
    map
}

/// Map TIFF BitsPerSample + SampleFormat to a `PixelType` (mirrors the private
/// helper in `tiff::reader`).
fn tiff_pixel_type(bps: u16, sample_format: u16) -> crate::common::pixel_type::PixelType {
    use crate::common::pixel_type::PixelType;
    match (bps, sample_format) {
        (1, _) => PixelType::Bit,
        (8, 2) => PixelType::Int8,
        (8, _) => PixelType::Uint8,
        (16, 2) => PixelType::Int16,
        (16, _) => PixelType::Uint16,
        (32, 2) => PixelType::Int32,
        (32, 3) => PixelType::Float32,
        (32, _) => PixelType::Uint32,
        (64, 3) => PixelType::Float64,
        _ => PixelType::Uint8,
    }
}

fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

/// Return the text content immediately following a tag's start (up to the next
/// `<`). Used for simple `<creationDate>...</creationDate>` style elements.
fn xml_element_text(xml: &str, tag: &XmlTag) -> Option<String> {
    if tag.self_closing {
        return None;
    }
    let rest = &xml[tag.body_start..];
    let end = rest.find('<')?;
    let text = xml_unescape(rest[..end].trim());
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

// ---------------------------------------------------------------------------
// 1. Hamamatsu NDPI whole-slide — enriched reader
// ---------------------------------------------------------------------------
/// Hamamatsu NDPI whole-slide image (TIFF-based, `.ndpi`).
///
/// Enriches metadata with NDPI-specific vendor tags:
/// - Tag 65421: magnification (float)
/// - Tag 65422: x-offset (float)
/// - Tag 65423: y-offset (float)
/// - Tag 65441: z-offset (float)
/// - Tag 65442: source lens (ASCII)
/// - Tag 65449: NDPI JPEG quality (long)
pub struct NdpiReader {
    inner: crate::tiff::TiffReader,
}

impl NdpiReader {
    pub fn new() -> Self {
        NdpiReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    fn enrich_metadata(&mut self) {
        // Read vendor tags from the first IFD
        let vendor = {
            let ifd = match self.inner.ifd(0) {
                Some(ifd) => ifd,
                None => return,
            };
            let mut meta = std::collections::HashMap::new();
            // Tag 65421 = magnification (stored as FLOAT)
            if let Some(v) = ifd.get(65421) {
                if let Some(vals) = v.as_vec_f32() {
                    if let Some(&mag) = vals.first() {
                        meta.insert(
                            "ndpi.magnification".to_string(),
                            crate::common::metadata::MetadataValue::Float(mag as f64),
                        );
                    }
                }
            }
            // Tag 65422 = x offset (FLOAT)
            if let Some(v) = ifd.get(65422) {
                if let Some(vals) = v.as_vec_f32() {
                    if let Some(&x) = vals.first() {
                        meta.insert(
                            "ndpi.offset.x".to_string(),
                            crate::common::metadata::MetadataValue::Float(x as f64),
                        );
                    }
                }
            }
            // Tag 65423 = y offset (FLOAT)
            if let Some(v) = ifd.get(65423) {
                if let Some(vals) = v.as_vec_f32() {
                    if let Some(&y) = vals.first() {
                        meta.insert(
                            "ndpi.offset.y".to_string(),
                            crate::common::metadata::MetadataValue::Float(y as f64),
                        );
                    }
                }
            }
            // Tag 65442 = source lens (ASCII)
            if let Some(v) = ifd.get(65442) {
                if let Some(s) = v.as_str() {
                    meta.insert(
                        "ndpi.source_lens".to_string(),
                        crate::common::metadata::MetadataValue::String(s.to_string()),
                    );
                }
            }
            meta
        };

        if let Some(s) = self.inner.series_list_mut().first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }
}

impl Default for NdpiReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NdpiReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ndpi"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}

// ---------------------------------------------------------------------------
// 2. Leica SCN whole-slide — enriched reader
// ---------------------------------------------------------------------------
/// Leica SCN whole-slide image (TIFF-based, `.scn`).
///
/// Ported from the Java `LeicaSCNReader`. The `<scn>` XML stored in the first
/// IFD's ImageDescription describes a `<collection>` of `<image>` elements.
/// Each `<image>` carries a `<pixels>` block whose `<dimension>` children map a
/// (z, c, r) coordinate — where `r` is the sub-resolution level — to a TIFF IFD
/// index. Each image (and each `<supplementalImage>` such as a barcode/label)
/// becomes its own series; the dimensions with r>0 become pyramid resolutions.
pub struct LeicaScnReader {
    inner: crate::tiff::TiffReader,
}

/// One `<dimension>` element: a plane for a given z/c/r mapped to a TIFF IFD.
#[derive(Debug, Clone, Default)]
struct ScnDimension {
    z: u32,
    c: u32,
    r: u32,
    size_x: u32,
    size_y: u32,
    ifd: usize,
}

/// One `<image>` (or `<supplementalImage>`) parsed from the SCN XML.
#[derive(Debug, Clone, Default)]
struct ScnImage {
    name: String,
    creation_date: Option<String>,
    dev_model: Option<String>,
    dev_version: Option<String>,
    obj_mag: Option<String>,
    illum_na: Option<String>,
    illum_source: Option<String>,
    v_size_x: i64,
    v_size_y: i64,
    v_spacing_z: i64,
    dims: Vec<ScnDimension>,
    size_z: u32,
    size_c: u32,
    size_r: u32,
}

impl ScnImage {
    fn lookup(&self, z: u32, c: u32, r: u32) -> Option<&ScnDimension> {
        self.dims.iter().find(|d| d.z == z && d.c == c && d.r == r)
    }
}

impl LeicaScnReader {
    pub fn new() -> Self {
        LeicaScnReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    fn scn_xml(&self) -> Option<String> {
        let series = self.inner.series_list();
        let first = series.first()?;
        let v = first.metadata.series_metadata.get("ImageDescription")?;
        if let crate::common::metadata::MetadataValue::String(s) = v {
            if s.contains("<scn") || s.contains("<SCN") {
                return Some(s.clone());
            }
        }
        None
    }

    /// Parse the `<scn>` XML into a list of images (mirrors LeicaSCNHandler).
    /// `<image>` elements become regular (possibly multi-resolution) series;
    /// `<supplementalImage>` elements become single-IFD series.
    fn parse_scn_xml(xml: &str) -> Vec<ScnImage> {
        let tags = xml_scan_tags(xml);
        let mut images: Vec<ScnImage> = Vec::new();
        let mut current: Option<ScnImage> = None;
        for tag in &tags {
            match tag.name.as_str() {
                "image" => {
                    if let Some(img) = current.take() {
                        images.push(img);
                    }
                    current = Some(ScnImage {
                        name: tag.attrs.get("name").cloned().unwrap_or_default(),
                        ..ScnImage::default()
                    });
                }
                "device" => {
                    if let Some(img) = current.as_mut() {
                        img.dev_model = tag.attrs.get("model").cloned();
                        img.dev_version = tag.attrs.get("version").cloned();
                    }
                }
                "view" => {
                    if let Some(img) = current.as_mut() {
                        if let Some(v) = tag.attrs.get("sizeX").and_then(|s| s.parse().ok()) {
                            img.v_size_x = v;
                        }
                        if let Some(v) = tag.attrs.get("sizeY").and_then(|s| s.parse().ok()) {
                            img.v_size_y = v;
                        }
                        if let Some(v) = tag.attrs.get("spacingZ").and_then(|s| s.parse().ok()) {
                            img.v_spacing_z = v;
                        }
                    }
                }
                "dimension" => {
                    if let Some(img) = current.as_mut() {
                        let a = &tag.attrs;
                        img.dims.push(ScnDimension {
                            z: a.get("z").and_then(|s| s.parse().ok()).unwrap_or(0),
                            c: a.get("c").and_then(|s| s.parse().ok()).unwrap_or(0),
                            r: a.get("r").and_then(|s| s.parse().ok()).unwrap_or(0),
                            size_x: a.get("sizeX").and_then(|s| s.parse().ok()).unwrap_or(0),
                            size_y: a.get("sizeY").and_then(|s| s.parse().ok()).unwrap_or(0),
                            ifd: a.get("ifd").and_then(|s| s.parse().ok()).unwrap_or(0),
                        });
                    }
                }
                "objective" => {
                    if let Some(img) = current.as_mut() {
                        img.obj_mag = xml_element_text(xml, tag);
                    }
                }
                "numericalAperture" => {
                    if let Some(img) = current.as_mut() {
                        img.illum_na = xml_element_text(xml, tag);
                    }
                }
                "illuminationSource" => {
                    if let Some(img) = current.as_mut() {
                        img.illum_source = xml_element_text(xml, tag);
                    }
                }
                "creationDate" => {
                    if let Some(img) = current.as_mut() {
                        img.creation_date = xml_element_text(xml, tag);
                    }
                }
                "supplementalImage" => {
                    if let Some(ifd) = tag.attrs.get("ifd").and_then(|s| s.parse::<usize>().ok()) {
                        let mut img = ScnImage {
                            name: tag.attrs.get("type").cloned().unwrap_or_default(),
                            size_z: 1,
                            size_c: 1,
                            size_r: 1,
                            ..ScnImage::default()
                        };
                        img.dims.push(ScnDimension {
                            ifd,
                            ..Default::default()
                        });
                        // A supplemental image interrupts the current image.
                        if let Some(cur) = current.take() {
                            images.push(cur);
                        }
                        images.push(img);
                    }
                }
                _ => {}
            }
        }
        if let Some(img) = current.take() {
            images.push(img);
        }
        // Compute sizeZ/sizeC/sizeR per image (max index + 1), as in the Java
        // <pixels> end-element handler. Supplemental images already set theirs.
        for img in &mut images {
            if img.dims.is_empty() {
                continue;
            }
            if img.size_r == 0 {
                let mut sc = 0;
                let mut sr = 0;
                let mut sz = 0;
                for d in &img.dims {
                    sc = sc.max(d.c);
                    sr = sr.max(d.r);
                    sz = sz.max(d.z);
                }
                img.size_c = sc + 1;
                img.size_r = sr + 1;
                img.size_z = sz + 1;
            }
        }
        images
    }

    /// Build the TiffReader series list from the parsed SCN images.
    fn build_scn_series(&mut self, images: &[ScnImage]) {
        use crate::common::metadata::MetadataValue;
        let ifd_count = self.inner.ifd_count();
        let little_endian = self.inner.is_little_endian();
        // `TiffSeries` is not re-exported; clone a template to obtain instances.
        let template = match self.inner.series_list().first() {
            Some(t) => t.clone(),
            None => return,
        };
        let mut new_series = Vec::new();

        for img in images {
            if img.dims.is_empty() {
                continue;
            }
            // Main resolution dimensions (r == 0), ordered C then Z.
            let main = match img.lookup(0, 0, 0).or_else(|| img.dims.first()) {
                Some(d) => d.clone(),
                None => continue,
            };
            // Plane order XYCZT: for each c, each z, at r=0.
            let mut main_ifds: Vec<usize> = Vec::new();
            for c in 0..img.size_c.max(1) {
                for z in 0..img.size_z.max(1) {
                    if let Some(d) = img.lookup(z, c, 0) {
                        if d.ifd < ifd_count {
                            main_ifds.push(d.ifd);
                        }
                    }
                }
            }
            if main_ifds.is_empty() {
                main_ifds.push(main.ifd);
            }

            // Sub-resolutions (r >= 1).
            let mut sub_resolutions: Vec<Vec<usize>> = Vec::new();
            for r in 1..img.size_r.max(1) {
                let mut level: Vec<usize> = Vec::new();
                for c in 0..img.size_c.max(1) {
                    for z in 0..img.size_z.max(1) {
                        if let Some(d) = img.lookup(z, c, r) {
                            if d.ifd < ifd_count {
                                level.push(d.ifd);
                            }
                        }
                    }
                }
                if !level.is_empty() {
                    sub_resolutions.push(level);
                }
            }

            // Determine pixel metadata from the main IFD.
            let main_ifd_idx = main_ifds[0];
            let mut meta = ImageMetadata::default();
            if let Some(ifd) = self.inner.ifd(main_ifd_idx) {
                let spp = ifd.samples_per_pixel();
                let bps = ifd.bits_per_sample().first().copied().unwrap_or(8);
                let photometric = ifd.photometric();
                let is_rgb = spp > 1;
                meta.size_x = if main.size_x > 0 {
                    main.size_x
                } else {
                    ifd.image_width().unwrap_or(0)
                };
                meta.size_y = if main.size_y > 0 {
                    main.size_y
                } else {
                    ifd.image_length().unwrap_or(0)
                };
                meta.size_z = img.size_z.max(1);
                meta.size_t = 1;
                meta.size_c = if is_rgb {
                    spp as u32
                } else {
                    img.size_c.max(1)
                };
                meta.is_rgb = is_rgb;
                meta.bits_per_pixel = bps as u8;
                let sample_format = ifd
                    .get_u16(crate::tiff::ifd::tag::SAMPLE_FORMAT)
                    .unwrap_or(1);
                meta.pixel_type = tiff_pixel_type(bps, sample_format);
                meta.is_indexed = matches!(photometric, crate::tiff::ifd::Photometric::Palette);
            }
            meta.is_little_endian = little_endian;
            let c_planes = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
            meta.image_count = meta.size_z.max(1) * c_planes;
            meta.resolution_count = 1 + sub_resolutions.len() as u32;
            meta.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;

            // Vendor metadata.
            if !img.name.is_empty() {
                meta.series_metadata.insert(
                    "leica.image.name".into(),
                    MetadataValue::String(img.name.clone()),
                );
            }
            if let Some(mag) = &img.obj_mag {
                if let Ok(m) = mag.parse::<f64>() {
                    meta.series_metadata.insert(
                        "leica.objective_magnification".into(),
                        MetadataValue::Float(m),
                    );
                }
            }
            if let Some(na) = &img.illum_na {
                if let Ok(v) = na.parse::<f64>() {
                    meta.series_metadata
                        .insert("leica.numerical_aperture".into(), MetadataValue::Float(v));
                }
            }
            if let Some(src) = &img.illum_source {
                meta.series_metadata.insert(
                    "leica.illumination_source".into(),
                    MetadataValue::String(src.clone()),
                );
            }
            if let Some(model) = &img.dev_model {
                meta.series_metadata.insert(
                    "leica.device.model".into(),
                    MetadataValue::String(model.clone()),
                );
            }
            if let Some(ver) = &img.dev_version {
                meta.series_metadata.insert(
                    "leica.device.version".into(),
                    MetadataValue::String(ver.clone()),
                );
            }
            if let Some(date) = &img.creation_date {
                meta.series_metadata.insert(
                    "leica.creation_date".into(),
                    MetadataValue::String(date.clone()),
                );
            }
            // Leica units are nanometres; physical size in micrometres.
            if img.v_size_x > 0 && meta.size_x > 0 {
                let px = (img.v_size_x as f64 / 1000.0) / meta.size_x as f64;
                meta.series_metadata
                    .insert("leica.physical_size_x".into(), MetadataValue::Float(px));
            }
            if img.v_size_y > 0 && meta.size_y > 0 {
                let py = (img.v_size_y as f64 / 1000.0) / meta.size_y as f64;
                meta.series_metadata
                    .insert("leica.physical_size_y".into(), MetadataValue::Float(py));
            }
            if img.v_spacing_z > 0 {
                meta.series_metadata.insert(
                    "leica.physical_size_z".into(),
                    MetadataValue::Float(img.v_spacing_z as f64 / 1000.0),
                );
            }

            let mut s = template.clone();
            s.ifd_indices = main_ifds;
            s.plane_ifd_indices = Vec::new();
            s.metadata = meta;
            s.sub_resolutions = sub_resolutions;
            new_series.push(s);
        }

        if !new_series.is_empty() {
            self.inner.replace_series(new_series);
        }
    }

    fn enrich_metadata(&mut self) {
        let Some(xml) = self.scn_xml() else { return };
        let images = Self::parse_scn_xml(&xml);
        if images.is_empty() {
            return;
        }
        self.build_scn_series(&images);
    }
}

impl Default for LeicaScnReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LeicaScnReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("scn"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}

// ---------------------------------------------------------------------------
// 3. Ventana/Roche BIF whole-slide — enriched reader
// ---------------------------------------------------------------------------
/// Ventana/Roche BIF whole-slide image (TIFF-based, `.bif`).
///
/// Ported from the Java `VentanaReader`. The full-resolution image is stored as
/// a single tiled TIFF IFD where tiles are laid out in snake (boustrophedon)
/// order. The XMP/XML in tag 700 (`<iScan>`/`<SlideStitchInfo>`/`<AoiOrigin>`)
/// provides the per-area tile grid and inter-tile overlaps used to compute each
/// tile's real position. `open_bytes`/`open_bytes_region` reassemble the tiles
/// into the stitched full-resolution image. Sub-resolution and label/overview
/// images are read directly via the inner `TiffReader`.
pub struct VentanaReader {
    inner: crate::tiff::TiffReader,
    magnification: Option<f64>,
    physical_pixel_size: Option<f64>,
    tile_width: u32,
    tile_height: u32,
    tiles: Vec<VentanaTile>,
    areas: Vec<VentanaArea>,
    /// Stitched full-resolution dimensions, once computed.
    full_x: u32,
    full_y: u32,
    /// True when the XML provided usable AOIs and we should reassemble tiles.
    reassemble: bool,
}

#[derive(Debug, Clone, Default)]
struct VentanaTile {
    base_x: i64,
    base_y: i64,
    real_x: i64,
    real_y: i64,
}

#[derive(Debug, Clone, Default)]
struct VentanaOverlap {
    a: i64,
    x: i64,
    y: i64,
    confidence: i64,
    direction: String,
}

#[derive(Debug, Clone, Default)]
struct VentanaArea {
    x_origin: i64,
    y_origin: i64,
    index: i64,
    tile_rows: i64,
    tile_columns: i64,
    overlaps: Vec<VentanaOverlap>,
    // bounding box (x, y, w, h) in full-res pixels
    bb_x: i64,
    bb_y: i64,
    bb_w: i64,
    bb_h: i64,
}

impl VentanaReader {
    pub fn new() -> Self {
        VentanaReader {
            inner: crate::tiff::TiffReader::new(),
            magnification: None,
            physical_pixel_size: None,
            tile_width: 0,
            tile_height: 0,
            tiles: Vec::new(),
            areas: Vec::new(),
            full_x: 0,
            full_y: 0,
            reassemble: false,
        }
    }

    fn first_description(&self) -> Option<String> {
        let series = self.inner.series_list();
        let v = series
            .first()?
            .metadata
            .series_metadata
            .get("ImageDescription")?;
        if let crate::common::metadata::MetadataValue::String(s) = v {
            Some(s.clone())
        } else {
            None
        }
    }

    /// Parse the iScan XMP. Mirrors `VentanaReader.parseXML`.
    fn parse_xml(&mut self, xml: &str) {
        let tags = xml_scan_tags(xml);

        // iScan ScanRes -> physical pixel size; also magnification if present.
        for tag in &tags {
            if tag.name == "iScan" {
                if let Some(sr) = tag.attrs.get("ScanRes").and_then(|s| s.parse::<f64>().ok()) {
                    self.physical_pixel_size = Some(sr);
                }
                if self.magnification.is_none() {
                    if let Some(m) = tag.attrs.get("Magnification").and_then(|s| s.parse().ok()) {
                        self.magnification = Some(m);
                    }
                }
            }
        }

        // SlideStitchInfo -> ImageInfo (areas) with TileJointInfo (overlaps).
        // We track nesting by index ranges between ImageInfo start tags.
        let mut areas: Vec<VentanaArea> = Vec::new();
        let mut i = 0usize;
        while i < tags.len() {
            if tags[i].name == "ImageInfo" {
                let info = &tags[i];
                if info.attrs.get("AOIScanned").map(|s| s.as_str()) == Some("0") {
                    i += 1;
                    continue;
                }
                let mut area = VentanaArea {
                    index: info
                        .attrs
                        .get("AOIIndex")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(areas.len() as i64),
                    tile_rows: info
                        .attrs
                        .get("NumRows")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    tile_columns: info
                        .attrs
                        .get("NumCols")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(0),
                    ..VentanaArea::default()
                };
                // Collect TileJointInfo until next ImageInfo or end.
                let mut j = i + 1;
                while j < tags.len() && tags[j].name != "ImageInfo" {
                    if tags[j].name == "TileJointInfo"
                        && tags[j].attrs.get("FlagJoined").map(|s| s.as_str()) == Some("1")
                    {
                        let a = &tags[j].attrs;
                        let overlap = VentanaOverlap {
                            a: a.get("Tile1")
                                .and_then(|s| s.parse::<i64>().ok())
                                .unwrap_or(1)
                                - 1,
                            x: a.get("OverlapX")
                                .and_then(|s| s.parse::<f64>().ok())
                                .map(|f| f as i64)
                                .unwrap_or(0),
                            y: a.get("OverlapY")
                                .and_then(|s| s.parse::<f64>().ok())
                                .map(|f| f as i64)
                                .unwrap_or(0),
                            confidence: a
                                .get("Confidence")
                                .and_then(|s| s.parse().ok())
                                .unwrap_or(0),
                            direction: a.get("Direction").cloned().unwrap_or_default(),
                        };
                        area.overlaps.push(overlap);
                    }
                    j += 1;
                }
                areas.push(area);
                i = j;
            } else {
                i += 1;
            }
        }

        // AoiOrigin children: <AOI0 OriginX=.. OriginY=..>, etc.
        for tag in &tags {
            if let Some(rest) = tag.name.strip_prefix("AOI") {
                if let Ok(this_index) = rest.parse::<i64>() {
                    let ox = tag.attrs.get("OriginX").and_then(|s| s.parse().ok());
                    let oy = tag.attrs.get("OriginY").and_then(|s| s.parse().ok());
                    if let (Some(ox), Some(oy)) = (ox, oy) {
                        for a in &mut areas {
                            if a.index == this_index {
                                a.x_origin = ox;
                                a.y_origin = oy;
                            }
                        }
                    }
                }
            }
        }

        self.areas = areas;
    }

    fn get_tile_column(index: i64, _rows: i64, cols: i64) -> i64 {
        if cols == 0 {
            return 0;
        }
        let row = index / cols;
        let col = index - row * cols;
        if row % 2 == 1 {
            cols - col - 1
        } else {
            col
        }
    }

    /// Build the tile grid (snake order) and compute real tile positions from
    /// overlaps. Mirrors the body of `initStandardMetadata` after the IFD scan.
    fn build_tiles(&mut self) {
        let series = self.inner.series_list();
        let Some(s0) = series.first() else { return };
        let full_w = s0.metadata.size_x;
        // Tile geometry from the full-resolution IFD.
        let main_ifd_idx = match s0.ifd_indices.first() {
            Some(&i) => i,
            None => return,
        };
        let (tw, th, offset_count) = match self.inner.ifd(main_ifd_idx) {
            Some(ifd) => (
                ifd.tile_width().unwrap_or(0),
                ifd.tile_length().unwrap_or(0),
                ifd.get_vec_u64(crate::tiff::ifd::tag::TILE_OFFSETS).len(),
            ),
            None => return,
        };
        if tw == 0 || th == 0 || offset_count == 0 {
            return;
        }
        self.tile_width = tw;
        self.tile_height = th;

        // base positions, snake/row-major as in Java (x increments, wraps at width)
        let mut tiles = vec![VentanaTile::default(); offset_count];
        let (mut x, mut y) = (0i64, 0i64);
        for t in tiles.iter_mut() {
            t.real_x = -(tw as i64);
            t.real_y = -(th as i64);
            t.base_x = tw as i64 * x;
            t.base_y = th as i64 * y;
            x += 1;
            if x * tw as i64 >= full_w as i64 {
                y += 1;
                x = 0;
            }
        }
        if self.areas.is_empty() {
            self.tiles = tiles;
            return;
        }

        let tile_cols = (full_w / tw) as i64;
        let mut max_y_adjust = i64::MIN;
        for area in &self.areas {
            let tile_row = area.y_origin / th as i64;
            let tile_col = area.x_origin / tw as i64;
            for row in 0..area.tile_rows {
                for col in 0..area.tile_columns {
                    let index = ((tile_row + row) * tile_cols + (tile_col + col)) as usize;
                    if index < tiles.len() {
                        tiles[index].real_x = tiles[index].base_x;
                        tiles[index].real_y = tiles[index].base_y;
                    }
                }
            }

            let mut column_y_adjust: std::collections::HashMap<i64, i64> =
                std::collections::HashMap::new();
            let mut column_x_adjust: std::collections::HashMap<i64, i64> =
                std::collections::HashMap::new();
            let mut right_sum = 0.0f64;
            let mut up_sum = 0.0f64;
            let mut right_count = 0;
            let mut up_count = 0;
            for overlap in &area.overlaps {
                if overlap.confidence < 98 {
                    continue;
                }
                match overlap.direction.as_str() {
                    "RIGHT" => {
                        right_sum += overlap.x as f64;
                        right_count += 1;
                        if overlap.y > 0 {
                            column_y_adjust.insert(
                                Self::get_tile_column(overlap.a, area.tile_rows, area.tile_columns),
                                overlap.y,
                            );
                        }
                    }
                    "UP" => {
                        up_sum += overlap.y as f64;
                        up_count += 1;
                    }
                    "LEFT" => {
                        let tc =
                            Self::get_tile_column(overlap.a, area.tile_rows, area.tile_columns);
                        column_x_adjust.insert(tc, overlap.x);
                        if overlap.y <= 0 {
                            column_y_adjust.insert(tc, overlap.y);
                        }
                    }
                    _ => {}
                }
            }
            if right_count > 0 {
                right_sum /= right_count as f64;
            }
            if up_count > 0 {
                up_sum /= up_count as f64;
            }

            // fill missing column Y adjustments (all-even / all-odd heuristic)
            let mut all_even = true;
            let mut all_odd = true;
            let mut first_value = None;
            for (&column, &val) in &column_y_adjust {
                first_value = Some(val);
                if column % 2 == 0 {
                    all_odd = false;
                } else {
                    all_even = false;
                }
            }
            if let Some(first_value) = first_value.filter(|_| all_odd || all_even) {
                for i in 0..area.tile_columns {
                    if (i % 2 == 0 && all_odd) || (i % 2 == 1 && all_even) {
                        continue;
                    }
                    column_y_adjust.entry(i).or_insert(first_value);
                }
            }
            for &adjust in column_y_adjust.values() {
                if adjust > max_y_adjust {
                    max_y_adjust = adjust;
                }
            }

            for row in 0..area.tile_rows {
                let mut left_col_adjust = 0i64;
                for col in 0..area.tile_columns {
                    let index = ((tile_row + row) * tile_cols + (tile_col + col)) as usize;
                    if index >= tiles.len() {
                        continue;
                    }
                    tiles[index].real_x -= (right_sum * col as f64) as i64;
                    tiles[index].real_x -= left_col_adjust;
                    if let Some(&adj) = column_x_adjust.get(&col) {
                        left_col_adjust += adj;
                    }
                    tiles[index].real_y -= (up_sum * row as f64) as i64;
                    if let Some(&adj) = column_y_adjust.get(&col) {
                        tiles[index].real_y += adj;
                    }
                }
            }
        }
        if max_y_adjust == i64::MIN {
            max_y_adjust = 0;
        }

        // compute minimal bounding box of all AOIs
        let mut min_x = i64::MAX;
        let mut min_y = i64::MAX;
        let mut max_x = 0i64;
        let mut max_y = 0i64;
        for area in &mut self.areas {
            let tile_row = area.y_origin / th as i64;
            let tile_col = area.x_origin / tw as i64;
            let mut area_min_x = i64::MAX;
            let mut area_min_y = i64::MAX;
            let mut area_max_x = 0i64;
            let mut area_max_y = 0i64;
            for row in 0..area.tile_rows {
                for col in 0..area.tile_columns {
                    let index = ((tile_row + row) * tile_cols + (tile_col + col)) as usize;
                    if index < tiles.len() && tiles[index].real_x >= 0 && tiles[index].real_y >= 0 {
                        area_min_x = area_min_x.min(tiles[index].real_x);
                        area_max_x = area_max_x.max(tiles[index].real_x + tw as i64);
                        area_min_y = area_min_y.min(tiles[index].real_y);
                        area_max_y = area_max_y.max(tiles[index].real_y + th as i64);
                    }
                }
            }
            area.bb_x = area_min_x;
            area.bb_y = area_min_y + max_y_adjust;
            area.bb_w = area_max_x - area_min_x;
            area.bb_h = area_max_y - area_min_y - (3 * max_y_adjust);

            min_x = area_min_x.min(min_x);
            max_x = area_max_x.max(max_x);
            min_y = area.bb_y.min(min_y);
            max_y = (area.bb_y + area.bb_h).max(max_y);
        }
        for area in &mut self.areas {
            let tile_row = area.y_origin / th as i64;
            let tile_col = area.x_origin / tw as i64;
            for row in 0..area.tile_rows {
                for col in 0..area.tile_columns {
                    let index = ((tile_row + row) * tile_cols + (tile_col + col)) as usize;
                    if index < tiles.len() {
                        tiles[index].real_x -= min_x;
                        tiles[index].real_y -= min_y;
                    }
                }
            }
            area.bb_x -= min_x;
            area.bb_y -= min_y;
        }

        self.tiles = tiles;
        if !self.areas.is_empty() && max_x > min_x && max_y > min_y {
            self.full_x = (max_x - min_x) as u32;
            self.full_y = (max_y - min_y) as u32;
            self.reassemble = true;
        }
    }

    fn enrich_metadata(&mut self) {
        let Some(desc) = self.first_description() else {
            return;
        };
        if !desc.contains("iScan") {
            return;
        }
        self.parse_xml(&desc);
        self.build_tiles();

        // Update full-resolution series dimensions and vendor metadata.
        if self.reassemble {
            let (fx, fy) = (self.full_x, self.full_y);
            if let Some(s) = self.inner.series_list_mut().first_mut() {
                s.metadata.size_x = fx;
                s.metadata.size_y = fy;
            }
        }
        let mag = self.magnification;
        let pps = self.physical_pixel_size;
        if let Some(s) = self.inner.series_list_mut().first_mut() {
            if let Some(m) = mag {
                s.metadata.series_metadata.insert(
                    "ventana.magnification".into(),
                    crate::common::metadata::MetadataValue::Float(m),
                );
            }
            if let Some(p) = pps {
                s.metadata.series_metadata.insert(
                    "ventana.physical_pixel_size".into(),
                    crate::common::metadata::MetadataValue::Float(p),
                );
            }
        }
    }

    /// Reassemble the full-resolution stitched image for a requested region by
    /// copying overlapping tile data. Bytes-per-pixel layout follows the inner
    /// TiffReader (chunky/planar handled by the underlying region reads).
    fn assemble_region(&mut self, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let bpp_pixel = {
            let m = self.inner.metadata();
            let bytes = (m.bits_per_pixel as usize + 7) / 8;
            let spp = if m.is_rgb { m.size_c as usize } else { 1 };
            bytes * spp
        };
        let tw = self.tile_width;
        let th = self.tile_height;
        let out_row = w as usize * bpp_pixel;
        let mut out = vec![0u8; out_row * h as usize];
        let req_x0 = x as i64;
        let req_y0 = y as i64;
        let req_x1 = req_x0 + w as i64;
        let req_y1 = req_y0 + h as i64;

        // Collect tile snapshot to avoid borrow conflicts during inner reads.
        let tiles = self.tiles.clone();
        for tile in &tiles {
            if tile.real_x < 0 || tile.real_y < 0 {
                continue;
            }
            let tx0 = tile.real_x;
            let ty0 = tile.real_y;
            let tx1 = tx0 + tw as i64;
            let ty1 = ty0 + th as i64;
            // Intersection of tile with requested region in stitched space.
            let ix0 = tx0.max(req_x0);
            let iy0 = ty0.max(req_y0);
            let ix1 = tx1.min(req_x1);
            let iy1 = ty1.min(req_y1);
            if ix0 >= ix1 || iy0 >= iy1 {
                continue;
            }
            // Read the source tile from the base TIFF layout.
            let src =
                self.inner
                    .open_bytes_region(0, tile.base_x as u32, tile.base_y as u32, tw, th)?;
            let src_row = tw as usize * bpp_pixel;
            for row in iy0..iy1 {
                let src_y = (row - ty0) as usize;
                let src_x = (ix0 - tx0) as usize;
                let copy_len = (ix1 - ix0) as usize * bpp_pixel;
                let s_off = src_y * src_row + src_x * bpp_pixel;
                let dst_y = (row - req_y0) as usize;
                let dst_x = (ix0 - req_x0) as usize;
                let d_off = dst_y * out_row + dst_x * bpp_pixel;
                if s_off + copy_len <= src.len() && d_off + copy_len <= out.len() {
                    out[d_off..d_off + copy_len].copy_from_slice(&src[s_off..s_off + copy_len]);
                }
            }
        }
        Ok(out)
    }
}

impl Default for VentanaReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for VentanaReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("bif"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.tiles.clear();
        self.areas.clear();
        self.reassemble = false;
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        // Reassemble only the full-resolution image (series 0, resolution 0).
        if self.reassemble && self.inner.series() == 0 && self.inner.resolution() == 0 {
            if p != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(p));
            }
            let (fx, fy) = (self.full_x, self.full_y);
            return self.assemble_region(0, 0, fx, fy);
        }
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.reassemble && self.inner.series() == 0 && self.inner.resolution() == 0 {
            if p != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(p));
            }
            validate_region("Ventana", self.full_x, self.full_y, x, y, w, h)?;
            return self.assemble_region(x, y, w, h);
        }
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}

// ---------------------------------------------------------------------------
// 4. Nikon NIS-Elements TIFF — enriched reader
// ---------------------------------------------------------------------------
/// Nikon NIS-Elements annotated TIFF (`.tiff`).
///
/// Parses XML metadata from ImageDescription looking for `<variant>` elements
/// to extract channel info and acquisition parameters.
pub struct NikonElementsTiffReader {
    inner: crate::tiff::TiffReader,
}

impl NikonElementsTiffReader {
    pub fn new() -> Self {
        NikonElementsTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    fn enrich_metadata(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            if series.is_empty() {
                return;
            }
            series[0]
                .metadata
                .series_metadata
                .get("ImageDescription")
                .and_then(|v| {
                    if let crate::common::metadata::MetadataValue::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
        };
        let Some(desc) = desc else { return };

        let mut vendor = std::collections::HashMap::new();

        // Nikon NIS-Elements XML uses <variant> elements
        if desc.contains("<variant") || desc.contains("NIS-Elements") || desc.contains("Nikon") {
            // Count channel references
            let channel_count = desc
                .matches("<Channel")
                .count()
                .max(desc.matches("<channel").count());
            if channel_count > 0 {
                vendor.insert(
                    "nikon.channel_count".to_string(),
                    crate::common::metadata::MetadataValue::Int(channel_count as i64),
                );
            }

            // Extract runtype or variant name attributes: name="value"
            // Look for key attributes in <variant> tags
            let lower = desc.to_ascii_lowercase();
            for tag_name in &[
                "runtype",
                "objectivename",
                "magnification",
                "numericaperture",
            ] {
                if let Some(pos) = lower.find(*tag_name) {
                    let rest = &desc[pos..];
                    if let Some(eq) = rest.find('=') {
                        let val_start = &rest[eq + 1..];
                        let val = val_start.trim_start_matches(|c: char| {
                            c == '"' || c == '\'' || c.is_whitespace()
                        });
                        let end = val
                            .find(|c: char| {
                                c == '"' || c == '\'' || c == '<' || c == '/' || c.is_whitespace()
                            })
                            .unwrap_or(val.len());
                        if !val[..end].is_empty() {
                            let key = format!("nikon.{}", tag_name);
                            if let Ok(f) = val[..end].parse::<f64>() {
                                vendor
                                    .insert(key, crate::common::metadata::MetadataValue::Float(f));
                            } else {
                                vendor.insert(
                                    key,
                                    crate::common::metadata::MetadataValue::String(
                                        val[..end].to_string(),
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }
}

impl Default for NikonElementsTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NikonElementsTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tiff"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}

// ---------------------------------------------------------------------------
// 5. FEI-annotated TIFF — enriched reader
// ---------------------------------------------------------------------------
/// FEI/ThermoFisher annotated TIFF (`.tiff`).
///
/// Parses ImageDescription for key=value pairs commonly found in FEI
/// electron microscope images (e.g. HV, beam current, pixel size).
pub struct FeiTiffReader {
    inner: crate::tiff::TiffReader,
}

impl FeiTiffReader {
    pub fn new() -> Self {
        FeiTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    fn enrich_metadata(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            if series.is_empty() {
                return;
            }
            series[0]
                .metadata
                .series_metadata
                .get("ImageDescription")
                .and_then(|v| {
                    if let crate::common::metadata::MetadataValue::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
        };
        let Some(desc) = desc else { return };

        let mut vendor = std::collections::HashMap::new();

        // FEI images use key=value lines, often with section headers like [User], [Beam], [Scan]
        for line in desc.lines() {
            let line = line.trim();
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim();
                if !key.is_empty() && !key.starts_with('[') && !val.is_empty() {
                    let sanitized_key = key.to_ascii_lowercase().replace(' ', "_");
                    if let Ok(f) = val.parse::<f64>() {
                        vendor.insert(
                            format!("fei.{}", sanitized_key),
                            crate::common::metadata::MetadataValue::Float(f),
                        );
                    } else {
                        vendor.insert(
                            format!("fei.{}", sanitized_key),
                            crate::common::metadata::MetadataValue::String(val.to_string()),
                        );
                    }
                }
            }
        }

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }
}

impl Default for FeiTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for FeiTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tiff"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}

// ---------------------------------------------------------------------------
// 6. Olympus SIS TIFF — enriched reader
// ---------------------------------------------------------------------------
/// Olympus SIS TIFF (`.tif`).
///
/// Parses ImageDescription for pixel calibration and acquisition metadata
/// stored by Olympus SIS software.
pub struct OlympusSisTiffReader {
    inner: crate::tiff::TiffReader,
}

impl OlympusSisTiffReader {
    pub fn new() -> Self {
        OlympusSisTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    fn enrich_metadata(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            if series.is_empty() {
                return;
            }
            series[0]
                .metadata
                .series_metadata
                .get("ImageDescription")
                .and_then(|v| {
                    if let crate::common::metadata::MetadataValue::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
        };
        let Some(desc) = desc else { return };

        let mut vendor = std::collections::HashMap::new();

        // Olympus SIS uses key=value or key:value lines for calibration
        for line in desc.lines() {
            let line = line.trim();
            // Try key=value first, then key: value
            let pair = line.split_once('=').or_else(|| line.split_once(':'));
            if let Some((key, val)) = pair {
                let key = key.trim();
                let val = val.trim();
                if key.is_empty() || val.is_empty() || key.starts_with('[') || key.starts_with('<')
                {
                    continue;
                }
                let sanitized_key = key.to_ascii_lowercase().replace(' ', "_");
                if let Ok(f) = val.parse::<f64>() {
                    vendor.insert(
                        format!("olympus_sis.{}", sanitized_key),
                        crate::common::metadata::MetadataValue::Float(f),
                    );
                } else {
                    vendor.insert(
                        format!("olympus_sis.{}", sanitized_key),
                        crate::common::metadata::MetadataValue::String(val.to_string()),
                    );
                }
            }
        }

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }
}

impl Default for OlympusSisTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for OlympusSisTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}

// ---------------------------------------------------------------------------
// 7. Improvision/Volocity annotated TIFF — enriched reader
// ---------------------------------------------------------------------------
/// Improvision/Volocity annotated TIFF (`.tif`).
///
/// Parses ImageDescription for structured metadata stored by
/// Improvision/PerkinElmer Volocity software.
pub struct ImprovisionTiffReader {
    inner: crate::tiff::TiffReader,
}

impl ImprovisionTiffReader {
    pub fn new() -> Self {
        ImprovisionTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    fn enrich_metadata(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            if series.is_empty() {
                return;
            }
            series[0]
                .metadata
                .series_metadata
                .get("ImageDescription")
                .and_then(|v| {
                    if let crate::common::metadata::MetadataValue::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
        };
        let Some(desc) = desc else { return };

        let mut vendor = std::collections::HashMap::new();

        // Improvision/Volocity uses key=value or key: value lines
        for line in desc.lines() {
            let line = line.trim();
            let pair = line.split_once('=').or_else(|| line.split_once(':'));
            if let Some((key, val)) = pair {
                let key = key.trim();
                let val = val.trim();
                if key.is_empty() || val.is_empty() || key.starts_with('[') || key.starts_with('<')
                {
                    continue;
                }
                let sanitized_key = key.to_ascii_lowercase().replace(' ', "_");
                if let Ok(f) = val.parse::<f64>() {
                    vendor.insert(
                        format!("improvision.{}", sanitized_key),
                        crate::common::metadata::MetadataValue::Float(f),
                    );
                } else {
                    vendor.insert(
                        format!("improvision.{}", sanitized_key),
                        crate::common::metadata::MetadataValue::String(val.to_string()),
                    );
                }
            }
        }

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }
}

impl Default for ImprovisionTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ImprovisionTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}

// ---------------------------------------------------------------------------
// 8. Zeiss ApoTome TIFF — enriched reader
// ---------------------------------------------------------------------------
/// Zeiss ApoTome TIFF (`.tif`).
///
/// Parses XML metadata from ImageDescription looking for `<Zeiss>` or
/// ApoTome acquisition parameters.
pub struct ZeissApotomeTiffReader {
    inner: crate::tiff::TiffReader,
}

impl ZeissApotomeTiffReader {
    pub fn new() -> Self {
        ZeissApotomeTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    fn enrich_metadata(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            if series.is_empty() {
                return;
            }
            series[0]
                .metadata
                .series_metadata
                .get("ImageDescription")
                .and_then(|v| {
                    if let crate::common::metadata::MetadataValue::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
        };
        let Some(desc) = desc else { return };

        let mut vendor = std::collections::HashMap::new();

        // Zeiss ApoTome may store XML with <Zeiss> or <ApoTome> elements
        if desc.contains("<Zeiss")
            || desc.contains("<zeiss")
            || desc.contains("<ApoTome")
            || desc.contains("AxioVision")
        {
            let lower = desc.to_ascii_lowercase();
            // Extract common Zeiss attributes
            for tag_name in &[
                "objectivemagnification",
                "objectivename",
                "exposuretime",
                "numericalaperture",
                "scalex",
                "scaley",
            ] {
                if let Some(pos) = lower.find(*tag_name) {
                    let rest = &desc[pos..];
                    // Try attribute form: key="value" or element <key>value</key>
                    if let Some(eq) = rest.find('=') {
                        let val_start = &rest[eq + 1..];
                        let val = val_start.trim_start_matches(|c: char| {
                            c == '"' || c == '\'' || c.is_whitespace()
                        });
                        let end = val
                            .find(|c: char| {
                                c == '"' || c == '\'' || c == '<' || c == '/' || c.is_whitespace()
                            })
                            .unwrap_or(val.len());
                        if !val[..end].is_empty() {
                            let key = format!("zeiss.{}", tag_name);
                            if let Ok(f) = val[..end].parse::<f64>() {
                                vendor
                                    .insert(key, crate::common::metadata::MetadataValue::Float(f));
                            } else {
                                vendor.insert(
                                    key,
                                    crate::common::metadata::MetadataValue::String(
                                        val[..end].to_string(),
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }

        // Also parse key=value lines for non-XML descriptions
        for line in desc.lines() {
            let line = line.trim();
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim().trim_matches('"');
                if !key.is_empty()
                    && !val.is_empty()
                    && !key.starts_with('[')
                    && !key.starts_with('<')
                {
                    let sanitized_key = key.to_ascii_lowercase().replace(' ', "_");
                    if !vendor.contains_key(&format!("zeiss.{}", sanitized_key)) {
                        if let Ok(f) = val.parse::<f64>() {
                            vendor.insert(
                                format!("zeiss.{}", sanitized_key),
                                crate::common::metadata::MetadataValue::Float(f),
                            );
                        } else {
                            vendor.insert(
                                format!("zeiss.{}", sanitized_key),
                                crate::common::metadata::MetadataValue::String(val.to_string()),
                            );
                        }
                    }
                }
            }
        }

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }
}

impl Default for ZeissApotomeTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ZeissApotomeTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}

// ---------------------------------------------------------------------------
// 9. Olympus Fluoview FV300 (`.tif`) — enriched reader
// ---------------------------------------------------------------------------
/// Olympus Fluoview FV300 TIFF (`.tif`).
///
/// Enriches metadata from the ImageDescription tag which may contain
/// Fluoview-specific key=value pairs like `[Acquisition Parameters]`.
pub struct FluoviewTiffReader {
    inner: crate::tiff::TiffReader,
}

impl FluoviewTiffReader {
    pub fn new() -> Self {
        FluoviewTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    fn enrich_metadata(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            if series.is_empty() {
                return;
            }
            series[0]
                .metadata
                .series_metadata
                .get("ImageDescription")
                .and_then(|v| {
                    if let crate::common::metadata::MetadataValue::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
        };
        let Some(desc) = desc else { return };
        if !desc.contains("[Acquisition Parameters]") && !desc.contains("FluoView") {
            return;
        }

        let mut vendor = std::collections::HashMap::new();
        // Parse INI-style key=value pairs
        for line in desc.lines() {
            let line = line.trim();
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim();
                if !key.is_empty() && !key.starts_with('[') {
                    vendor.insert(
                        format!("fluoview.{}", key),
                        crate::common::metadata::MetadataValue::String(val.to_string()),
                    );
                }
            }
        }

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }
}

impl Default for FluoviewTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for FluoviewTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}

// ---------------------------------------------------------------------------
// 10. Molecular Devices plate TIFF — enriched reader
// ---------------------------------------------------------------------------
/// Molecular Devices MetaXpress plate TIFF (`.tif`).
///
/// Parses ImageDescription for plate/well info and acquisition parameters
/// stored by Molecular Devices MetaXpress software.
pub struct MolecularDevicesTiffReader {
    inner: crate::tiff::TiffReader,
}

impl MolecularDevicesTiffReader {
    pub fn new() -> Self {
        MolecularDevicesTiffReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    fn enrich_metadata(&mut self) {
        let desc = {
            let series = self.inner.series_list();
            if series.is_empty() {
                return;
            }
            series[0]
                .metadata
                .series_metadata
                .get("ImageDescription")
                .and_then(|v| {
                    if let crate::common::metadata::MetadataValue::String(s) = v {
                        Some(s.clone())
                    } else {
                        None
                    }
                })
        };
        let Some(desc) = desc else { return };

        let mut vendor = std::collections::HashMap::new();

        // Molecular Devices may use XML or key=value pairs
        // Look for plate/well identifiers and acquisition parameters
        if desc.contains("<MetaXpress")
            || desc.contains("Molecular Devices")
            || desc.contains("<PlateID")
        {
            let lower = desc.to_ascii_lowercase();
            for tag_name in &[
                "plateid",
                "wellid",
                "siteid",
                "wavelength",
                "exposuretime",
                "objectivemagnification",
            ] {
                if let Some(pos) = lower.find(*tag_name) {
                    let rest = &desc[pos..];
                    if let Some(eq) = rest.find('=') {
                        let val_start = &rest[eq + 1..];
                        let val = val_start.trim_start_matches(|c: char| {
                            c == '"' || c == '\'' || c.is_whitespace()
                        });
                        let end = val
                            .find(|c: char| {
                                c == '"' || c == '\'' || c == '<' || c == '/' || c.is_whitespace()
                            })
                            .unwrap_or(val.len());
                        if !val[..end].is_empty() {
                            let key = format!("moldev.{}", tag_name);
                            if let Ok(f) = val[..end].parse::<f64>() {
                                vendor
                                    .insert(key, crate::common::metadata::MetadataValue::Float(f));
                            } else {
                                vendor.insert(
                                    key,
                                    crate::common::metadata::MetadataValue::String(
                                        val[..end].to_string(),
                                    ),
                                );
                            }
                        }
                    }
                }
            }
        }

        // Also parse generic key=value lines
        for line in desc.lines() {
            let line = line.trim();
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim().trim_matches('"');
                if !key.is_empty()
                    && !val.is_empty()
                    && !key.starts_with('[')
                    && !key.starts_with('<')
                {
                    let sanitized_key = key.to_ascii_lowercase().replace(' ', "_");
                    if !vendor.contains_key(&format!("moldev.{}", sanitized_key)) {
                        if let Ok(f) = val.parse::<f64>() {
                            vendor.insert(
                                format!("moldev.{}", sanitized_key),
                                crate::common::metadata::MetadataValue::Float(f),
                            );
                        } else {
                            vendor.insert(
                                format!("moldev.{}", sanitized_key),
                                crate::common::metadata::MetadataValue::String(val.to_string()),
                            );
                        }
                    }
                }
            }
        }

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }
}

impl Default for MolecularDevicesTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MolecularDevicesTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}
