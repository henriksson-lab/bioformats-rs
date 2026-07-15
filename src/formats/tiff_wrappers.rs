//! Thin TIFF-wrapper readers for formats that are TIFF-based but identified
//! only by file extension (no distinct magic bytes beyond TIFF itself).
//!
//! All readers delegate all pixel / metadata work to `crate::tiff::TiffReader`.

use std::path::Path;

use crate::common::compressed::{CompressedExtractionSupport, CompressedTile, CompressedTileMode};
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
    /// Byte offset of the `<` for this start tag.
    start_offset: usize,
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
            start_offset: i,
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
/// Ported from the Java `NDPIReader` (`initStandardMetadata`,
/// `NDPIReader.java:419-607`). NDPI stores its image set as a flat chain of TIFF
/// IFDs that must be regrouped into a logical structure:
///
/// - **sizeZ** is detected by counting trailing IFDs whose width/height match
///   IFD 0 (a focal Z-stack of the full-resolution image).
/// - **pyramid levels** are the differently sized IFDs that are *not* the macro
///   (`SOURCE_LENS == -1`) or map/mask (`SOURCE_LENS == -2`) overview images.
///   When `SOURCE_LENS` (tag 65421) is absent, every differing IFD except the
///   last is assumed to be a pyramid level.
/// - The full-resolution image plus its pyramid levels become **one
///   multi-resolution series** (`resolution_count == pyramidHeight`); the
///   trailing macro / map images become standalone trailing series.
/// - Plane → IFD mapping follows Java `getIFDIndex`: for the pyramid series at
///   resolution `s`, plane `z` lives at IFD `z * pyramidHeight + s`; extra
///   (macro/map) series live after `sizeZ * pyramidHeight`.
///
/// Vendor tags are also surfaced into the first series' metadata
/// (magnification, stage offsets, source lens, serial number, capture mode).
pub struct NdpiReader {
    inner: crate::tiff::TiffReader,
    /// Detected number of focal planes (Z) for the pyramid series.
    size_z: u32,
    /// Number of resolution levels in the pyramid series (>= 1).
    pyramid_height: u32,
    /// True when the file is larger than 4 GB and therefore uses 32-bit TIFF
    /// offsets that wrap; see [`NdpiReader::analyze_large_file_offsets`].
    use_64bit: bool,
    /// Per-flattened-series OME image metadata (name + physical sizes).
    ome_images: Vec<crate::common::ome_metadata::OmeImage>,
}

// NDPI custom TIFF tags (mirrors the constants in NDPIReader.java:66-99).
const NDPI_OFFSET_HIGH_BYTES: u16 = 65324;
const NDPI_BYTE_COUNT_HIGH_BYTES: u16 = 65325;
const NDPI_SOURCE_LENS: u16 = 65421;
const NDPI_X_POSITION: u16 = 65422;
const NDPI_Y_POSITION: u16 = 65423;
const NDPI_Z_POSITION: u16 = 65424;
const NDPI_MARKER_TAG: u16 = 65426;
const NDPI_CAPTURE_MODE: u16 = 65441;
const NDPI_SERIAL_NUMBER: u16 = 65442;
const NDPI_METADATA_TAG: u16 = 65449;

pub(crate) fn ndpi_has_hamamatsu_tags(path: &Path) -> bool {
    let Ok(file) = std::fs::File::open(path) else {
        return false;
    };
    let use_64bit = file
        .metadata()
        .map(|m| m.len() >= (1u64 << 32))
        .unwrap_or(false);
    if use_64bit {
        ndpi_has_hamamatsu_tags_in_large_file(file)
    } else {
        ndpi_has_hamamatsu_tags_in_stream(file)
    }
}

fn ndpi_has_hamamatsu_tags_in_stream<R: std::io::Read + std::io::Seek>(stream: R) -> bool {
    let mut parser = match crate::tiff::parser::TiffParser::new(stream) {
        Ok(parser) => parser,
        Err(_) => return false,
    };
    let offset = parser.first_ifd_offset;
    let ifd = match parser.read_ifd(offset) {
        Ok((ifd, _)) => ifd,
        Err(_) => return false,
    };
    ifd.get(NDPI_MARKER_TAG).is_some() || ifd.get(NDPI_METADATA_TAG).is_some()
}

fn ndpi_has_hamamatsu_tags_in_large_file<R: std::io::Read + std::io::Seek>(stream: R) -> bool {
    let mut parser = match crate::tiff::parser::TiffParser::new(stream) {
        Ok(parser) => parser,
        Err(_) => return false,
    };
    let ifds = match parser.read_ifds_ndpi64() {
        Ok(ifds) => ifds,
        Err(_) => return false,
    };
    ifds.first()
        .map(|ifd| ifd.get(NDPI_MARKER_TAG).is_some() || ifd.get(NDPI_METADATA_TAG).is_some())
        .unwrap_or(false)
}

impl NdpiReader {
    pub fn new() -> Self {
        NdpiReader {
            inner: crate::tiff::TiffReader::new(),
            size_z: 1,
            pyramid_height: 1,
            use_64bit: false,
            ome_images: Vec::new(),
        }
    }

    /// Detected number of focal (Z) planes in the pyramid series.
    pub fn size_z(&self) -> u32 {
        self.size_z
    }

    /// Number of resolution levels in the pyramid series (Java `pyramidHeight`).
    pub fn pyramid_height(&self) -> u32 {
        self.pyramid_height
    }

    /// True when the file is >4 GB and uses wrapping 32-bit TIFF offsets.
    pub fn uses_64bit_offsets(&self) -> bool {
        self.use_64bit
    }

    /// Read `SOURCE_LENS` (tag 65421) from an IFD as a float, if present.
    /// Java stores this as a FLOAT; the special values -1 (macro) and -2
    /// (map/mask) flag the overview images that must not become pyramid levels.
    fn source_lens(&self, ifd_index: usize) -> Option<f32> {
        let ifd = self.inner.ifd(ifd_index)?;
        // Usually FLOAT, but tolerate other numeric encodings.
        if let Some(v) = ifd.get(NDPI_SOURCE_LENS) {
            if let Some(vals) = v.as_vec_f32() {
                return vals.first().copied();
            }
            return v.as_f64().map(|f| f as f32);
        }
        None
    }

    /// Width/height of an IFD (0 if missing).
    fn ifd_dims(&self, ifd_index: usize) -> (u32, u32) {
        match self.inner.ifd(ifd_index) {
            Some(ifd) => (
                ifd.image_width().unwrap_or(0),
                ifd.image_length().unwrap_or(0),
            ),
            None => (0, 0),
        }
    }

    /// Detect `sizeZ` and `pyramidHeight` and regroup the flat IFD chain into a
    /// pyramid series + trailing macro/map series. Mirrors
    /// `NDPIReader.initStandardMetadata` (`NDPIReader.java:524-607`).
    fn build_ndpi_series(&mut self) {
        let ifd_count = self.inner.ifd_count();
        if ifd_count == 0 {
            return;
        }
        let little_endian = self.inner.is_little_endian();

        let (w0, h0) = self.ifd_dims(0);

        // --- detect sizeZ and pyramidHeight (Java 524-548) ---
        let mut size_z: u32 = 1;
        let mut pyramid_height: u32 = 1;
        for i in 1..ifd_count {
            let (w, h) = self.ifd_dims(i);
            if w == w0 && h == h0 {
                size_z += 1;
            } else if size_z == 1 {
                // Differing dimensions: pyramid level vs. macro/map overview.
                let is_pyramid = match self.source_lens(i) {
                    Some(lens) => lens != -1.0 && lens != -2.0,
                    // No SOURCE_LENS: assume the last IFD is the macro image.
                    None => i < ifd_count - 1,
                };
                if is_pyramid {
                    pyramid_height += 1;
                }
            }
        }
        self.size_z = size_z;
        self.pyramid_height = pyramid_height;
        self.apply_capture_mode_overrides();

        // seriesCount = pyramidHeight + (ifds - pyramidHeight*sizeZ)  (Java 552)
        // The first `pyramidHeight` "series" collapse into one multi-resolution
        // series; the remainder are trailing extras (macro/map).
        let pyramid_planes = (pyramid_height as usize) * (size_z as usize);
        let extra_count = ifd_count.saturating_sub(pyramid_planes);

        // Java getIFDIndex: pyramid resolution `s`, plane `z` -> z*pyramidHeight+s
        let pyramid_ifd = |s: usize, z: usize| z * (pyramid_height as usize) + s;
        // Java getIFDIndex: extra series `e` (0-based among extras) -> base + e
        let extra_ifd = |e: usize| pyramid_planes + e;

        // Need a template TiffSeries to obtain instances (struct not re-exported).
        let template = match self.inner.series_list().first() {
            Some(t) => t.clone(),
            None => return,
        };

        let mut new_series = Vec::new();

        // --- pyramid series (level 0 = full res, 1.. = sub-resolutions) ---
        {
            let base_ifd = pyramid_ifd(0, 0);
            let mut meta = self.ndpi_plane_meta(base_ifd, little_endian, size_z);

            // Sub-resolution levels: each level is a single plane per z.
            let mut sub_resolutions: Vec<Vec<usize>> = Vec::new();
            for s in 1..(pyramid_height as usize) {
                let mut level: Vec<usize> = Vec::new();
                for z in 0..(size_z as usize) {
                    let idx = pyramid_ifd(s, z);
                    if idx < ifd_count {
                        level.push(idx);
                    }
                }
                if !level.is_empty() {
                    sub_resolutions.push(level);
                }
            }
            meta.resolution_count = 1 + sub_resolutions.len() as u32;

            // Full-resolution plane list: one IFD per z.
            let mut main_ifds: Vec<usize> = Vec::new();
            for z in 0..(size_z as usize) {
                let idx = pyramid_ifd(0, z);
                if idx < ifd_count {
                    main_ifds.push(idx);
                }
            }
            if main_ifds.is_empty() {
                main_ifds.push(base_ifd.min(ifd_count - 1));
            }

            self.attach_vendor_metadata(0, &mut meta);
            self.apply_capture_mode_to_meta(&mut meta);

            let mut s = template.clone();
            s.ifd_indices = main_ifds;
            s.plane_ifd_indices = Vec::new();
            s.metadata = meta;
            s.sub_resolutions = sub_resolutions;
            new_series.push(s);
        }

        // --- trailing extra series (macro / map / mask), one IFD each ---
        for e in 0..extra_count {
            let idx = extra_ifd(e);
            if idx >= ifd_count {
                break;
            }
            let mut meta = self.ndpi_plane_meta(idx, little_endian, 1);
            meta.resolution_count = 1;
            // Java initMetadataStore names: series 1 = macro, 2 = macro mask.
            let name = match e {
                0 => "macro image",
                1 => "macro mask image",
                _ => "",
            };
            if !name.is_empty() {
                meta.series_metadata.insert(
                    "ndpi.image_type".into(),
                    crate::common::metadata::MetadataValue::String(name.to_string()),
                );
            }
            let mut s = template.clone();
            s.ifd_indices = vec![idx];
            s.plane_ifd_indices = Vec::new();
            s.metadata = meta;
            s.sub_resolutions = Vec::new();
            new_series.push(s);
        }

        if !new_series.is_empty() {
            self.inner.replace_series(new_series);
        }
    }

    /// NDPI `MAX_SIZE` (NDPIReader.java:63): JPEG planes larger than this in
    /// either dimension exceed libjpeg's limits and are decoded by the custom
    /// chunky/interleaved NDPI service instead of the TiffParser.
    const NDPI_MAX_SIZE: u32 = 2048;

    /// Set each (flattened) series' `is_interleaved` flag per
    /// `NDPIReader.useTiffParser`: interleaved only when the series' first IFD is
    /// JPEG-compressed, carries the NDPI marker tag, and is larger than
    /// `MAX_SIZE` in BOTH dimensions. All other series are channel-separated.
    fn set_ndpi_interleaving(&mut self) {
        let mut flags: Vec<bool> = Vec::new();
        for s in self.inner.series_list() {
            let interleaved = s
                .ifd_indices
                .first()
                .and_then(|&idx| self.inner.ifd(idx))
                .map(|ifd| {
                    let w = ifd.image_width().unwrap_or(0);
                    let h = ifd.image_length().unwrap_or(0);
                    let jpeg = matches!(
                        ifd.compression(),
                        crate::tiff::ifd::Compression::Jpeg
                            | crate::tiff::ifd::Compression::JpegNew
                    );
                    let has_marker = ifd.get(NDPI_MARKER_TAG).is_some();
                    // useTiffParser == false  =>  interleaved == true
                    w > Self::NDPI_MAX_SIZE && h > Self::NDPI_MAX_SIZE && jpeg && has_marker
                })
                .unwrap_or(false);
            flags.push(interleaved);
        }
        for (s, &interleaved) in self.inner.series_list_mut().iter_mut().zip(&flags) {
            s.metadata.is_interleaved = interleaved;
        }
    }

    fn ndpi_pixel_bytes_for_metadata(m: &ImageMetadata, buf: Vec<u8>) -> Vec<u8> {
        if !m.is_rgb || m.size_c <= 1 {
            return buf;
        }
        let channels = m.size_c as usize;
        let bps = ((m.bits_per_pixel as usize + 7) / 8).max(1);
        let plane = buf.len() / channels;
        if plane == 0 || plane % bps != 0 || plane * channels != buf.len() {
            return buf;
        }
        let pixels = plane / bps;
        let mut out = vec![0u8; buf.len()];
        for i in 0..pixels {
            for c in 0..channels {
                let src = c * plane + i * bps;
                let dst = (i * channels + c) * bps;
                out[dst..dst + bps].copy_from_slice(&buf[src..src + bps]);
            }
        }
        out
    }

    fn ndpi_pixel_bytes(&self, buf: Vec<u8>, _w: u32, _h: u32) -> Vec<u8> {
        let m = self.inner.metadata();
        let marked_ndpi_ifd = self
            .inner
            .series_list()
            .get(self.inner.series())
            .and_then(|s| s.ifd_indices.first())
            .and_then(|&idx| self.inner.ifd(idx))
            .map(|ifd| ifd.get(NDPI_MARKER_TAG).is_some())
            .unwrap_or(false);
        if !marked_ndpi_ifd {
            return buf;
        }
        Self::ndpi_pixel_bytes_for_metadata(m, buf)
    }

    /// Build OME image metadata for each flattened series: name "Series N" and
    /// PhysicalSizeX/Y derived from the IFD resolution tags
    /// (`10000 / XResolution` for ResolutionUnit == cm), mirroring the
    /// FormatTools.getPhysicalSize path Java uses for NDPI.
    fn build_ndpi_ome(&mut self) {
        use crate::common::ome_metadata::{OmeChannel, OmeImage, OmePlane};
        use crate::tiff::ifd::tag;
        let mut images: Vec<OmeImage> = Vec::new();
        let series: Vec<(usize, ImageMetadata)> = self
            .inner
            .series_list()
            .iter()
            .map(|s| {
                (
                    s.ifd_indices.first().copied().unwrap_or(0),
                    s.metadata.clone(),
                )
            })
            .collect();
        for (i, (ifd_idx, metadata)) in series.into_iter().enumerate() {
            let (px, py) = self
                .inner
                .ifd(ifd_idx)
                .map(|ifd| {
                    let unit = ifd.get_u16(tag::RESOLUTION_UNIT).unwrap_or(2);
                    let scale = match unit {
                        3 => 10_000.0, // centimetre
                        2 => 25_400.0, // inch
                        _ => 0.0,
                    };
                    let conv = |t: u16| {
                        ifd.get(t)
                            .and_then(|v| v.as_vec_f64().first().copied())
                            .filter(|&r| r > 0.0 && scale > 0.0)
                            .map(|r| scale / r)
                    };
                    (conv(tag::X_RESOLUTION), conv(tag::Y_RESOLUTION))
                })
                .unwrap_or((None, None));
            let mut planes = self
                .inner
                .ifd(ifd_idx)
                .and_then(|ifd| {
                    let position_x = ifd
                        .get(NDPI_X_POSITION)
                        .and_then(|v| v.as_vec_f32().and_then(|s| s.first().copied()))
                        .map(|v| v as f64);
                    let position_y = ifd
                        .get(NDPI_Y_POSITION)
                        .and_then(|v| v.as_vec_f32().and_then(|s| s.first().copied()))
                        .map(|v| v as f64);
                    let position_z = ifd.get(NDPI_Z_POSITION).and_then(|v| v.as_f64());
                    if position_x.is_some() || position_y.is_some() || position_z.is_some() {
                        Some(vec![OmePlane {
                            position_x,
                            position_y,
                            position_z,
                            ..Default::default()
                        }])
                    } else {
                        None
                    }
                })
                .unwrap_or_default();
            if planes.is_empty() && i == 0 && metadata.image_count > 0 {
                planes.push(OmePlane::default());
            }
            images.push(OmeImage {
                name: Some(format!("Series {}", i + 1)),
                physical_size_x: px,
                physical_size_y: py,
                channels: vec![OmeChannel {
                    samples_per_pixel: metadata.size_c.max(1),
                    ..Default::default()
                }],
                planes,
                ..Default::default()
            });
        }
        self.ome_images = images;
    }

    /// Build per-series `ImageMetadata` from an IFD, mirroring Java's
    /// per-CoreMetadata population (`NDPIReader.java:582-660`). `size_z` is the
    /// focal-plane count for the pyramid series (1 for extras).
    fn ndpi_plane_meta(&self, ifd_index: usize, little_endian: bool, size_z: u32) -> ImageMetadata {
        let mut meta = ImageMetadata::default();
        if let Some(ifd) = self.inner.ifd(ifd_index) {
            let spp = ifd.samples_per_pixel();
            // Java clamps bits-per-sample up to 8 (NDPIReader.java:558-564).
            let bps = ifd.bits_per_sample().first().copied().unwrap_or(8).max(8);
            let photometric = ifd.photometric();
            let is_rgb = spp > 1 || matches!(photometric, crate::tiff::ifd::Photometric::Rgb);
            meta.size_x = ifd.image_width().unwrap_or(0);
            meta.size_y = ifd.image_length().unwrap_or(0);
            meta.size_c = if is_rgb { spp as u32 } else { 1 };
            meta.is_rgb = is_rgb;
            meta.bits_per_pixel = bps as u8;
            let sample_format = ifd
                .get_u16(crate::tiff::ifd::tag::SAMPLE_FORMAT)
                .unwrap_or(1);
            meta.pixel_type = tiff_pixel_type(bps, sample_format);
            meta.is_indexed = matches!(photometric, crate::tiff::ifd::Photometric::Palette);
        }
        meta.size_z = size_z.max(1);
        meta.size_t = 1;
        meta.is_little_endian = little_endian;
        // RGB planes pack channels into one plane; otherwise one per channel.
        let c_planes = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        meta.image_count = meta.size_z.max(1) * c_planes;
        meta.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;
        meta
    }

    /// Java treats capture modes > 6 as single-channel high-bit-depth data, even
    /// when the TIFF pyramid IFDs advertise 3x8-bit RGB.
    fn capture_mode_bits_for(mode: u16) -> Option<u16> {
        match mode {
            7 => Some(12),
            13 | 14 | 16 => Some(14),
            17 | 18 => Some(16),
            _ if mode > 6 => None,
            _ => None,
        }
    }

    fn capture_mode_bits(&self) -> Option<u16> {
        Self::capture_mode_bits_for(self.inner.ifd(0)?.get_u16(NDPI_CAPTURE_MODE)?)
    }

    fn apply_capture_mode_to_meta(&self, meta: &mut ImageMetadata) {
        if let Some(bits) = self.capture_mode_bits() {
            meta.size_c = 1;
            meta.is_rgb = false;
            meta.pixel_type = crate::common::pixel_type::PixelType::Uint16;
            meta.bits_per_pixel = bits as u8;
            meta.image_count = meta.size_z.max(1) * meta.size_t.max(1);
        }
    }

    fn apply_capture_mode_overrides(&mut self) {
        let Some(bits) = self.capture_mode_bits() else {
            return;
        };
        let pyramid_height = self.pyramid_height as usize;
        let size_z = self.size_z as usize;
        for z in 0..size_z {
            for s in 0..pyramid_height {
                let idx = z * pyramid_height + s;
                if let Some(ifd) = self.inner.ifd_mut(idx) {
                    ifd.entries.insert(
                        crate::tiff::ifd::tag::BITS_PER_SAMPLE,
                        crate::tiff::ifd::IfdValue::Short(vec![bits]),
                    );
                    ifd.entries.insert(
                        crate::tiff::ifd::tag::SAMPLES_PER_PIXEL,
                        crate::tiff::ifd::IfdValue::Short(vec![1]),
                    );
                    ifd.entries.insert(
                        crate::tiff::ifd::tag::PHOTOMETRIC_INTERPRETATION,
                        crate::tiff::ifd::IfdValue::Short(vec![1]),
                    );
                }
            }
        }
    }

    /// Surface NDPI vendor tags from `ifd_index` into `meta.series_metadata`,
    /// plus the `\n`-delimited `METADATA_TAG` (65449) key/value calibration
    /// block (Java `NDPIReader.java:616-689`).
    fn attach_vendor_metadata(&self, ifd_index: usize, meta: &mut ImageMetadata) {
        use crate::common::metadata::MetadataValue;
        let Some(ifd) = self.inner.ifd(ifd_index) else {
            return;
        };

        if let Some(v) = ifd.get(NDPI_SOURCE_LENS) {
            if let Some(mag) = v.as_vec_f32().and_then(|s| s.first().copied()) {
                meta.series_metadata.insert(
                    "ndpi.magnification".into(),
                    MetadataValue::Float(mag as f64),
                );
            }
        }
        if let Some(v) = ifd.get(NDPI_X_POSITION) {
            if let Some(x) = v.as_vec_f32().and_then(|s| s.first().copied()) {
                meta.series_metadata
                    .insert("ndpi.offset.x".into(), MetadataValue::Float(x as f64));
            }
        }
        if let Some(v) = ifd.get(NDPI_Y_POSITION) {
            if let Some(y) = v.as_vec_f32().and_then(|s| s.first().copied()) {
                meta.series_metadata
                    .insert("ndpi.offset.y".into(), MetadataValue::Float(y as f64));
            }
        }
        if let Some(v) = ifd.get(NDPI_Z_POSITION) {
            if let Some(z) = v.as_f64() {
                meta.series_metadata
                    .insert("ndpi.offset.z".into(), MetadataValue::Float(z));
            }
        }
        if let Some(s) = ifd.get(NDPI_SERIAL_NUMBER).and_then(|v| v.as_str()) {
            meta.series_metadata.insert(
                "ndpi.serial_number".into(),
                MetadataValue::String(s.to_string()),
            );
        }
        if let Some(cm) = ifd.get_u16(NDPI_CAPTURE_MODE) {
            meta.series_metadata
                .insert("ndpi.capture_mode".into(), MetadataValue::Int(cm as i64));
        }
        // METADATA_TAG: newline-separated "key=value" calibration entries.
        if let Some(block) = ifd.get(NDPI_METADATA_TAG).and_then(|v| v.as_str()) {
            for entry in block.split('\n') {
                if let Some(eq) = entry.find('=') {
                    let key = entry[..eq].trim();
                    let value = entry[eq + 1..].trim();
                    if key.is_empty() {
                        continue;
                    }
                    meta.series_metadata.insert(
                        format!("ndpi.{key}"),
                        MetadataValue::String(value.to_string()),
                    );
                }
            }
        }
    }

    /// BUG 2 — reconstruct >4 GB offsets for NDPI.
    ///
    /// NDPI files larger than 4 GB keep using classic (32-bit) TIFF offsets that
    /// wrap; Java reconstructs the true 64-bit offsets from the per-IFD high-word
    /// trailer and from the `OFFSET_HIGH_BYTES` (65324) / `BYTE_COUNT_HIGH_BYTES`
    /// (65325) arrays (`NDPIReader.java:439-521`).
    ///
    /// We implement the multi-strip/tile path (Java's `stripOffsets.length > 1`
    /// branch): for each IFD that carries the high-word tags, add `high << 32` to
    /// every strip/tile offset and byte count and rewrite the arrays in place as
    /// 64-bit `Long8` values via `TiffReader::ifd_mut`, so the core pixel-read
    /// path (which reads these arrays as u64 and seeks with a u64 offset) lands
    /// on the correct >4 GB position. The high-word arrays and the base offset
    /// arrays themselves are written near the start of the file (<4 GB), so the
    /// already-parsed values are intact and only need the high words added.
    ///
    /// RESIDUAL LIMITATION: the single-strip case (Java's Mechanism A, where the
    /// 4 high bytes for each IFD *entry* are appended after the IFD body) is not
    /// handled — it would require re-reading the raw IFD bytes, and the IFD file
    /// offsets are not exposed here. NDPI is JPEG-tiled (multi-tile) in practice,
    /// so this affects only the rare single-strip >4 GB layout, flagged in
    /// `ndpi.offset64.limitation` when it occurs.
    fn analyze_large_file_offsets(&mut self, _path: &Path, file_len: u64) {
        use crate::common::metadata::MetadataValue;
        self.use_64bit = file_len >= (1u64 << 32);
        if !self.use_64bit {
            return;
        }

        // The IFD chain and every per-entry value offset were already corrected
        // during 64-bit ("fake BigTIFF") IFD parsing (Mechanism A: the per-IFD
        // high-word trailer). Here we only finish Mechanism B: for multi-strip /
        // multi-tile IFDs the individual element offsets/byte counts carry their
        // high 32 bits in separate OFFSET_HIGH_BYTES (65324) / BYTE_COUNT_HIGH_BYTES
        // (65325) arrays. Add `high << 32` to each element.
        let ifd_count = self.inner.ifd_count();
        let mut any_high_words = false;
        let mut multistrip_corrected = 0usize;

        for i in 0..ifd_count {
            if let Some(ifd) = self.inner.ifd_mut(i) {
                match apply_ndpi_multistrip_offset_correction(ifd) {
                    NdpiOffsetFix::Corrected => {
                        any_high_words = true;
                        multistrip_corrected += 1;
                    }
                    NdpiOffsetFix::SingleStripUnhandled | NdpiOffsetFix::NoHighWords => {}
                }
            }
        }

        if let Some(s) = self.inner.series_list_mut().first_mut() {
            let m = &mut s.metadata.series_metadata;
            m.insert("ndpi.use_64bit_offsets".into(), MetadataValue::Bool(true));
            m.insert(
                "ndpi.offset64.high_word_tags_present".into(),
                MetadataValue::Bool(any_high_words),
            );
            m.insert(
                "ndpi.offset64.multistrip_corrected_ifds".into(),
                MetadataValue::Int(multistrip_corrected as i64),
            );
        }
    }
}

/// Outcome of applying NDPI >4 GB offset correction to one IFD.
enum NdpiOffsetFix {
    /// No high-word tags present in this IFD — nothing to do.
    NoHighWords,
    /// Single-strip layout (Java Mechanism A) — not reconstructed here.
    SingleStripUnhandled,
    /// Multi-strip/tile offsets/byte-counts were rewritten with high words.
    Corrected,
}

/// Apply NDPI's multi-strip/tile >4 GB offset reconstruction to one IFD in
/// place (Java `NDPIReader.java:439-521`, the `stripOffsets.length > 1` branch).
///
/// `OFFSET_HIGH_BYTES` (65324) / `BYTE_COUNT_HIGH_BYTES` (65325) hold the high
/// 32 bits for each strip/tile; the true offset is `low + (high << 32)`. The
/// corrected arrays are written back as 64-bit `Long8` so the core reader, which
/// reads them as u64 and seeks with a u64 offset, lands past 4 GB.
fn apply_ndpi_multistrip_offset_correction(ifd: &mut crate::tiff::ifd::Ifd) -> NdpiOffsetFix {
    use crate::tiff::ifd::{tag, IfdValue};

    let offset_high = ifd.get(NDPI_OFFSET_HIGH_BYTES).map(|v| v.as_vec_u64());
    let count_high = ifd.get(NDPI_BYTE_COUNT_HIGH_BYTES).map(|v| v.as_vec_u64());
    if offset_high.is_none() && count_high.is_none() {
        return NdpiOffsetFix::NoHighWords;
    }

    let (off_tag, offs) = ifd
        .get(tag::STRIP_OFFSETS)
        .map(|v| (tag::STRIP_OFFSETS, v.as_vec_u64()))
        .or_else(|| {
            ifd.get(tag::TILE_OFFSETS)
                .map(|v| (tag::TILE_OFFSETS, v.as_vec_u64()))
        })
        .unwrap_or((0, Vec::new()));
    let (cnt_tag, counts) = ifd
        .get(tag::STRIP_BYTE_COUNTS)
        .map(|v| (tag::STRIP_BYTE_COUNTS, v.as_vec_u64()))
        .or_else(|| {
            ifd.get(tag::TILE_BYTE_COUNTS)
                .map(|v| (tag::TILE_BYTE_COUNTS, v.as_vec_u64()))
        })
        .unwrap_or((0, Vec::new()));

    // Java applies the per-strip high-byte arrays only with >1 strip/tile.
    if offs.len() <= 1 {
        return NdpiOffsetFix::SingleStripUnhandled;
    }

    let mut new_offs = offs;
    if let Some(hi) = &offset_high {
        if hi.len() == new_offs.len() {
            for (o, h) in new_offs.iter_mut().zip(hi) {
                *o = o.wrapping_add(h << 32);
            }
        }
    }
    let mut new_counts = counts;
    if let Some(hi) = &count_high {
        if hi.len() == new_counts.len() {
            for (c, h) in new_counts.iter_mut().zip(hi) {
                *c = c.wrapping_add(h << 32);
            }
        }
    }

    if off_tag != 0 {
        ifd.entries.insert(off_tag, IfdValue::Long8(new_offs));
    }
    if cnt_tag != 0 && !new_counts.is_empty() {
        ifd.entries.insert(cnt_tag, IfdValue::Long8(new_counts));
    }
    NdpiOffsetFix::Corrected
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
        matches!(ext.as_deref(), Some("ndpi")) && ndpi_has_hamamatsu_tags(path)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        ndpi_has_hamamatsu_tags_in_stream(std::io::Cursor::new(_header))
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.close()?;
        self.size_z = 1;
        self.pyramid_height = 1;
        self.use_64bit = false;
        self.ome_images.clear();
        if !ndpi_has_hamamatsu_tags(path) {
            return Err(BioFormatsError::UnsupportedFormat(
                "NDPI TIFF missing Hamamatsu marker/metadata tags".into(),
            ));
        }
        // For files >4 GB, NDPI uses Hamamatsu's "fake BigTIFF" layout: the IFD
        // chain pointers and per-entry value offsets are 64-bit (low 32 bits in
        // the entry, high 32 bits in a per-IFD trailer). A naive 32-bit walk wraps
        // mod 2^32 and lands on garbage, so the inner parser must be told to use
        // the NDPI 64-bit layout BEFORE it reads the IFDs. Mirrors Java
        // NDPIReader.initFile + TiffParser.setUse64BitOffsets(true).
        let file_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        self.use_64bit = file_len >= (1u64 << 32);
        self.inner.set_ndpi_64bit(self.use_64bit);
        self.inner.set_id(path)?;
        // Regroup the flat NDPI IFD chain into a pyramid series (+ macro/map
        // series) and detect sizeZ, mirroring NDPIReader.initStandardMetadata.
        self.build_ndpi_series();
        if self.inner.series_count() == 0 {
            return Err(BioFormatsError::Format(
                "NDPI TIFF contains no readable image series".into(),
            ));
        }
        // Java's default ImageReader flattens the pyramid: each resolution is its
        // own top-level series. Mirror that so seriesCount matches the reference.
        let _ = self.inner.flatten_resolutions_into_series();
        // Per-series interleaving follows NDPIReader.useTiffParser: a JPEG IFD
        // larger than MAX_SIZE in both dimensions is decoded chunky/interleaved
        // by the custom NDPI service; everything else is read channel-separated.
        self.set_ndpi_interleaving();
        self.build_ndpi_ome();
        // BUG 2: per-element multi-strip/tile high-word arrays (Mechanism B).
        // The per-entry/single-strip high words (Mechanism A) and the IFD chain
        // pointers are already corrected during 64-bit IFD parsing above.
        self.analyze_large_file_offsets(path, file_len);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.size_z = 1;
        self.pyramid_height = 1;
        self.use_64bit = false;
        self.ome_images.clear();
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
        let (w, h) = {
            let m = self.inner.metadata();
            (m.size_x, m.size_y)
        };
        let buf = self.inner.open_bytes(p)?;
        Ok(self.ndpi_pixel_bytes(buf, w, h))
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let buf = self.inner.open_bytes_region(p, x, y, w, h)?;
        Ok(self.ndpi_pixel_bytes(buf, w, h))
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn compressed_level_info(
        &self,
        plane_index: u32,
        level: u32,
    ) -> Result<CompressedExtractionSupport> {
        self.inner.compressed_level_info(plane_index, level)
    }
    fn read_compressed_tile(
        &mut self,
        plane_index: u32,
        level: u32,
        col: u64,
        row: u64,
        preferred_modes: &[CompressedTileMode],
    ) -> Result<CompressedTile> {
        self.inner
            .read_compressed_tile(plane_index, level, col, row, preferred_modes)
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.ome_images.is_empty() {
            return None;
        }
        Some(crate::common::ome_metadata::OmeMetadata {
            images: self.ome_images.clone(),
            instruments: vec![crate::common::ome_metadata::OmeInstrument {
                objectives: vec![crate::common::ome_metadata::OmeObjective::default()],
                ..Default::default()
            }],
            ..Default::default()
        })
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
    /// Per-flattened-series OME image metadata (name + physical sizes), built
    /// from the SCN XML before resolution flattening so each (image, resolution)
    /// gets its own name/calibration mirroring Java's LeicaSCNReader.
    ome_images: Vec<crate::common::ome_metadata::OmeImage>,
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
            ome_images: Vec::new(),
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
        let mut current_end: Option<usize> = None;
        for tag in &tags {
            if let Some(end) = current_end {
                if tag.start_offset > end {
                    if let Some(img) = current.take() {
                        images.push(img);
                    }
                    current_end = None;
                }
            }
            match tag.name.as_str() {
                "image" => {
                    if let Some(img) = current.take() {
                        images.push(img);
                    }
                    current_end = xml[tag.body_start..]
                        .find("</image>")
                        .map(|end| tag.body_start + end);
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
                        // Java's SAX handler adds supplemental images directly
                        // to the image map without closing the active <image>.
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
                let is_rgb = spp > 1 || matches!(photometric, crate::tiff::ifd::Photometric::Rgb);
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

        // Java's LeicaSCNReader reports RGB planes channel-separated
        // (isInterleaved == false) for every resolution.
        for s in &mut new_series {
            s.metadata.is_interleaved = false;
        }

        // Build per-(image, resolution) OME metadata BEFORE flattening, so each
        // flattened series gets the right name ("image_NAME (Rk)") and physical
        // size (Leica volume / resolution width), mirroring LeicaSCNReader.
        use crate::common::ome_metadata::{OmeChannel, OmeImage};
        let mut ome_images: Vec<OmeImage> = Vec::new();
        for img in images {
            if img.dims.is_empty() {
                continue;
            }
            let channels = if img
                .lookup(0, 0, 0)
                .or_else(|| img.dims.first())
                .map(|d| d.ifd)
                .and_then(|idx| self.inner.ifd(idx))
                .map(|ifd| ifd.samples_per_pixel() > 1)
                .unwrap_or(true)
            {
                self.inner
                    .ifd(img.lookup(0, 0, 0).map(|d| d.ifd).unwrap_or(0))
                    .map(|ifd| ifd.samples_per_pixel() as u32)
                    .unwrap_or(3)
            } else {
                img.size_c.max(1)
            };
            for r in 0..img.size_r.max(1) {
                let dim = img.lookup(0, 0, r);
                let width = dim
                    .map(|d| d.size_x)
                    .filter(|&w| w > 0)
                    .or_else(|| {
                        dim.and_then(|d| self.inner.ifd(d.ifd))
                            .and_then(|ifd| ifd.image_width())
                    })
                    .unwrap_or(0);
                let height = dim
                    .map(|d| d.size_y)
                    .filter(|&h| h > 0)
                    .or_else(|| {
                        dim.and_then(|d| self.inner.ifd(d.ifd))
                            .and_then(|ifd| ifd.image_length())
                    })
                    .unwrap_or(0);
                let px = if img.v_size_x > 0 && width > 0 {
                    Some((img.v_size_x as f64 / 1000.0) / width as f64)
                } else {
                    None
                };
                let py = if img.v_size_y > 0 && height > 0 {
                    Some((img.v_size_y as f64 / 1000.0) / height as f64)
                } else {
                    None
                };
                let objective_ref = images
                    .iter()
                    .position(|candidate| candidate.name == img.name)
                    .unwrap_or(0)
                    .min(1);
                ome_images.push(OmeImage {
                    name: Some(format!("{} (R{})", img.name, r)),
                    physical_size_x: px,
                    physical_size_y: py,
                    physical_size_z: (img.v_spacing_z > 0)
                        .then_some(img.v_spacing_z as f64 / 1000.0),
                    channels: vec![OmeChannel {
                        samples_per_pixel: channels,
                        ..Default::default()
                    }],
                    planes: vec![crate::common::ome_metadata::OmePlane::default()],
                    instrument_ref: Some(0),
                    objective_ref: Some(objective_ref),
                    ..Default::default()
                });
            }
        }
        self.ome_images = ome_images;

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
        // Java flattens each image's resolution pyramid into top-level series.
        let _ = self.inner.flatten_resolutions_into_series();
        // Flattening copies the parent metadata; re-assert channel-separated.
        for s in self.inner.series_list_mut() {
            s.metadata.is_interleaved = false;
        }
    }

    fn interleave_channels(&self, buf: Vec<u8>, w: u32, h: u32) -> Vec<u8> {
        let _ = (w, h);
        buf
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
        self.ome_images.clear();
        self.inner.close()?;
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.ome_images.clear();
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
        let (w, h) = {
            let m = self.inner.metadata();
            (m.size_x, m.size_y)
        };
        let buf = self.inner.open_bytes(p)?;
        Ok(self.interleave_channels(buf, w, h))
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let buf = self.inner.open_bytes_region(p, x, y, w, h)?;
        Ok(self.interleave_channels(buf, w, h))
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(p)
    }
    fn compressed_level_info(
        &self,
        plane_index: u32,
        level: u32,
    ) -> Result<CompressedExtractionSupport> {
        self.inner.compressed_level_info(plane_index, level)
    }
    fn read_compressed_tile(
        &mut self,
        plane_index: u32,
        level: u32,
        col: u64,
        row: u64,
        preferred_modes: &[CompressedTileMode],
    ) -> Result<CompressedTile> {
        self.inner
            .read_compressed_tile(plane_index, level, col, row, preferred_modes)
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.ome_images.is_empty() {
            return None;
        }
        Some(crate::common::ome_metadata::OmeMetadata {
            images: self.ome_images.clone(),
            instruments: vec![crate::common::ome_metadata::OmeInstrument {
                objectives: vec![
                    crate::common::ome_metadata::OmeObjective::default(),
                    crate::common::ome_metadata::OmeObjective::default(),
                ],
                ..Default::default()
            }],
            ..Default::default()
        })
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
    /// Java rewrites the reported XY size for every resolution after AOI
    /// stitching. Keep those dimensions here because the inner TIFF reader
    /// derives subresolution metadata directly from raw IFD sizes.
    stitched_resolution_sizes: Vec<(u32, u32)>,
    metadata_override: Option<ImageMetadata>,
    ome_images: Vec<crate::common::ome_metadata::OmeImage>,
    pyramid_start_ifd: usize,
    file_name: Option<String>,
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
            stitched_resolution_sizes: Vec::new(),
            metadata_override: None,
            ome_images: Vec::new(),
            pyramid_start_ifd: 0,
            file_name: None,
        }
    }

    fn first_description(&self) -> Option<String> {
        let mut fallback = None;
        let mut iscan_fallback = None;
        for idx in 0..self.inner.ifd_count() {
            let Some(ifd) = self.inner.ifd(idx) else {
                continue;
            };
            let text = Self::ifd_text(ifd, 700).or_else(|| {
                ifd.get_str(crate::tiff::ifd::tag::IMAGE_DESCRIPTION)
                    .map(str::to_string)
            });
            let Some(text) = text else {
                continue;
            };
            if text.contains("SlideStitchInfo") {
                return Some(text);
            }
            if text.contains("<iScan") {
                iscan_fallback.get_or_insert(text);
                continue;
            }
            fallback.get_or_insert(text);
        }
        iscan_fallback.or(fallback)
    }

    fn ifd_text(ifd: &crate::tiff::ifd::Ifd, tag: u16) -> Option<String> {
        match ifd.get(tag)? {
            crate::tiff::ifd::IfdValue::Ascii(s) => Some(s.trim_end_matches('\0').to_string()),
            crate::tiff::ifd::IfdValue::Byte(v) | crate::tiff::ifd::IfdValue::Undefined(v) => {
                let nul = v.iter().position(|&b| b == 0).unwrap_or(v.len());
                String::from_utf8(v[..nul].to_vec()).ok()
            }
            _ => None,
        }
    }

    fn detect_pyramid_start_ifd(&self) -> usize {
        for idx in 0..self.inner.ifd_count() {
            let Some(ifd) = self.inner.ifd(idx) else {
                continue;
            };
            if ifd
                .get_str(crate::tiff::ifd::tag::IMAGE_DESCRIPTION)
                .map(|desc| {
                    desc.split_whitespace()
                        .any(|token| token.starts_with("level="))
                })
                .unwrap_or(false)
            {
                return idx;
            }
        }
        for idx in 0..self.inner.ifd_count() {
            let Some(ifd) = self.inner.ifd(idx) else {
                continue;
            };
            if !ifd
                .get_vec_u64(crate::tiff::ifd::tag::TILE_OFFSETS)
                .is_empty()
                && ifd.image_width().unwrap_or(0) > 0
                && ifd.image_length().unwrap_or(0) > 0
            {
                return idx;
            }
        }
        0
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
        let mut image_info_index = 0i64;
        while i < tags.len() {
            if tags[i].name == "ImageInfo" {
                let info = &tags[i];
                if info.attrs.get("AOIScanned").map(|s| s.as_str()) == Some("0") {
                    image_info_index += 1;
                    i += 1;
                    continue;
                }
                let mut area = VentanaArea {
                    index: info
                        .attrs
                        .get("AOIIndex")
                        .and_then(|s| s.parse().ok())
                        .unwrap_or(image_info_index),
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
                image_info_index += 1;
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

    fn compute_stitched_resolution_sizes(&self) -> Vec<(u32, u32)> {
        let Some(series) = self.inner.series_list().first() else {
            return Vec::new();
        };
        let original_full_x = self
            .inner
            .ifd(self.pyramid_start_ifd)
            .and_then(|ifd| ifd.image_width())
            .unwrap_or(series.metadata.size_x);
        let mut sizes = vec![(self.full_x, self.full_y)];
        for idx in (self.pyramid_start_ifd + 1)..self.inner.ifd_count() {
            let Some((sub_x, _sub_y)) = self.inner.ifd(idx).map(|ifd| {
                (
                    ifd.image_width().unwrap_or(0),
                    ifd.image_length().unwrap_or(0),
                )
            }) else {
                continue;
            };
            if self
                .inner
                .ifd(idx)
                .map(|ifd| {
                    ifd.get_vec_u64(crate::tiff::ifd::tag::TILE_OFFSETS)
                        .is_empty()
                })
                .unwrap_or(true)
            {
                break;
            }
            sizes.push(Self::stitched_size_for_resolution(
                original_full_x,
                self.full_x,
                self.full_y,
                sub_x,
            ));
        }
        sizes
    }

    fn stitched_size_for_resolution(
        original_full_x: u32,
        stitched_full_x: u32,
        stitched_full_y: u32,
        resolution_x: u32,
    ) -> (u32, u32) {
        let scale = if resolution_x > 0 {
            ((original_full_x as f64) / (resolution_x as f64)).round() as u32
        } else {
            1
        }
        .max(1);
        (stitched_full_x / scale, stitched_full_y / scale)
    }

    fn refresh_metadata_override(&mut self) {
        self.metadata_override = None;
        if !self.reassemble {
            return;
        }
        if let Some(&(sx, sy)) = self.stitched_resolution_sizes.get(self.inner.series()) {
            let mut meta = self.inner.metadata().clone();
            meta.size_x = sx;
            meta.size_y = sy;
            self.metadata_override = Some(meta);
        }
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
        let full_w = self
            .inner
            .ifd(self.pyramid_start_ifd)
            .and_then(|ifd| ifd.image_width())
            .or_else(|| series.first().map(|s| s.metadata.size_x))
            .unwrap_or(0);
        // Tile geometry from the full-resolution IFD.
        let main_ifd_idx = self.pyramid_start_ifd;
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
                    tiles[index].real_x =
                        (tiles[index].real_x as f64 - (right_sum * col as f64)) as i64;
                    tiles[index].real_x -= left_col_adjust;
                    if let Some(&adj) = column_x_adjust.get(&col) {
                        left_col_adjust += adj;
                    }
                    tiles[index].real_y =
                        (tiles[index].real_y as f64 - (up_sum * row as f64)) as i64;
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
        let mut valid_areas = Vec::new();
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
            if area_min_x == i64::MAX || area_min_y == i64::MAX {
                continue;
            }
            area.bb_x = area_min_x;
            area.bb_y = area_min_y + max_y_adjust;
            area.bb_w = area_max_x - area_min_x;
            area.bb_h = area_max_y - area_min_y - (3 * max_y_adjust);
            if area.bb_w <= 0 || area.bb_h <= 0 {
                continue;
            }

            min_x = area_min_x.min(min_x);
            max_x = area_max_x.max(max_x);
            min_y = area.bb_y.min(min_y);
            max_y = (area.bb_y + area.bb_h).max(max_y);
            valid_areas.push(area.clone());
        }
        self.areas = valid_areas;
        if self.areas.is_empty() {
            self.tiles = tiles;
            return;
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
        self.pyramid_start_ifd = self.detect_pyramid_start_ifd();
        self.build_tiles();

        // Update full-resolution series dimensions and vendor metadata.
        if self.reassemble {
            self.stitched_resolution_sizes = self.compute_stitched_resolution_sizes();
        }
        let mag = self.magnification;
        let pps = self.physical_pixel_size;
        if self.reassemble {
            self.reorder_bif_series();
        }
        for s in self.inner.series_list_mut() {
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
        self.build_ome_images();
        self.refresh_metadata_override();
    }

    fn reorder_bif_series(&mut self) {
        let series = self.inner.series_list().to_vec();
        if series.len() < 3 {
            return;
        }

        let mut by_ifd = std::collections::HashMap::new();
        for s in series {
            if let Some(&ifd) = s.ifd_indices.first() {
                by_ifd.insert(ifd, s);
            }
        }

        let mut reordered = Vec::new();
        let ifd_count = self.inner.ifd_count();
        for ifd in self.pyramid_start_ifd..ifd_count {
            let Some(mut s) = by_ifd.remove(&ifd) else {
                continue;
            };
            let level = ifd - self.pyramid_start_ifd;
            if let Some(&(sx, sy)) = self.stitched_resolution_sizes.get(level) {
                s.metadata.size_x = sx;
                s.metadata.size_y = sy;
            }
            s.metadata.is_interleaved = false;
            reordered.push(s);
        }
        for ifd in 0..self.pyramid_start_ifd {
            if let Some(mut s) = by_ifd.remove(&ifd) {
                s.metadata.is_interleaved = false;
                reordered.push(s);
            }
        }

        if !reordered.is_empty() {
            self.inner.replace_series(reordered);
        }
    }

    fn build_ome_images(&mut self) {
        use crate::common::ome_metadata::{OmeChannel, OmeImage};
        self.ome_images.clear();
        let physical = self.physical_pixel_size;
        let file_name = self.file_name.as_deref().unwrap_or("Ventana image");
        for (i, s) in self.inner.series_list().iter().enumerate() {
            let channels = s.metadata.size_c.max(1);
            self.ome_images.push(OmeImage {
                name: Some(format!("{file_name} #{}", i + 1)),
                physical_size_x: physical,
                physical_size_y: physical,
                channels: vec![OmeChannel {
                    samples_per_pixel: channels,
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
    }

    /// Scale factor between the full-resolution image and the resolution that is
    /// currently selected on the inner reader. Mirrors Java `getScale`
    /// (`VentanaReader.java:740-743`): `round(fullX / resX)`.
    fn get_scale(&self) -> i64 {
        if self.reassemble {
            let level = self.inner.series();
            if let (Some(&(full_x, _)), Some(&(res_x, _))) = (
                self.stitched_resolution_sizes.first(),
                self.stitched_resolution_sizes.get(level),
            ) {
                if res_x > 0 {
                    return ((full_x as f64) / (res_x as f64)).round().max(1.0) as i64;
                }
            }
        }
        let res_x = self.inner.metadata().size_x as i64;
        if res_x <= 0 {
            return 1;
        }
        let scale = (self.full_x as f64 / res_x as f64).round() as i64;
        scale.max(1)
    }

    /// Scale a full-resolution coordinate to the current resolution. Mirrors
    /// Java `scaleCoordinate` (`VentanaReader.java:750-752`): `ceil(v / scale)`.
    fn scale_coordinate(&self, v: i64, scale: i64) -> i64 {
        if scale <= 1 {
            return v;
        }
        ((v as f64) / (scale as f64)).ceil() as i64
    }

    /// Reassemble a stitched region for the *currently selected resolution* by
    /// copying overlapping tile data. The requested region `(x,y,w,h)` is in the
    /// pixel space of the current resolution. Each tile is first clipped to the
    /// bounding box of the AOI it belongs to (Java `VentanaReader.java:250-262`),
    /// then its placement and dimensions are scaled to the current resolution and
    /// intersected with the requested region (Java `VentanaReader.java:314-340`).
    /// Bytes-per-pixel layout follows the inner TiffReader.
    fn assemble_region(&mut self, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let (bytes_per_sample, samples_per_pixel, planar_rgb) = {
            let m = self.inner.metadata();
            let bytes = (m.bits_per_pixel as usize + 7) / 8;
            let spp = if m.is_rgb { m.size_c as usize } else { 1 };
            (bytes, spp, m.is_rgb && !m.is_interleaved && spp > 1)
        };
        // Full-resolution tile geometry (Java tileWidth/tileHeight).
        let tw = self.tile_width as i64;
        let th = self.tile_height as i64;

        let scale = self.get_scale();
        // Tile dimensions in the current resolution (Java thisTileWidth/Height).
        let this_tw = self.scale_coordinate(tw, scale);
        let this_th = self.scale_coordinate(th, scale);

        let out_row = w as usize * bytes_per_sample;
        let out_plane = out_row * h as usize;
        let mut out = vec![0u8; out_plane * samples_per_pixel];
        let req_x0 = x as i64;
        let req_y0 = y as i64;
        let req_x1 = req_x0 + w as i64;
        let req_y1 = req_y0 + h as i64;

        // Snapshot of tiles + areas to avoid borrow conflicts during inner reads.
        let tiles = self.tiles.clone();
        let areas = self.areas.clone();
        for tile in &tiles {
            // Tile placement rect in full-resolution stitched space.
            let mut box_x = tile.real_x;
            let mut box_y = tile.real_y;
            let mut box_w = tw;
            let mut box_h = th;

            // Clip to the bounding box of the first AOI the tile intersects
            // (Java 253-258).
            for area in &areas {
                let (ax, ay, aw, ah) = (area.bb_x, area.bb_y, area.bb_w, area.bb_h);
                if box_x < ax + aw && ax < box_x + box_w && box_y < ay + ah && ay < box_y + box_h {
                    let nx = box_x.max(ax);
                    let ny = box_y.max(ay);
                    let nx1 = (box_x + box_w).min(ax + aw);
                    let ny1 = (box_y + box_h).min(ay + ah);
                    box_x = nx;
                    box_y = ny;
                    box_w = nx1 - nx;
                    box_h = ny1 - ny;
                    break;
                }
            }

            // Scale the (clipped) tile box to the current resolution (Java 259-262).
            box_x = self.scale_coordinate(box_x, scale);
            box_y = self.scale_coordinate(box_y, scale);
            box_w = self.scale_coordinate(box_w, scale);
            box_h = self.scale_coordinate(box_h, scale);

            // Intersection of the scaled tile box with the requested region
            // (Java 264, 314).
            let ix0 = box_x.max(req_x0);
            let iy0 = box_y.max(req_y0);
            let ix1 = (box_x + box_w).min(req_x1);
            let iy1 = (box_y + box_h).min(req_y1);
            if ix0 >= ix1 || iy0 >= iy1 {
                continue;
            }

            // Java reads a whole TIFF tile from sub-resolutions, aligned to the
            // sub-resolution tile grid, then copies the scaled tile window out
            // of that cache. Reading only this_tw/this_th at the scaled base
            // position gives different JPEG crop pixels for Ventana pyramids.
            let (src_x, src_y, src_w, src_h, src_off_x, src_off_y) = if scale == 1 {
                (tile.base_x, tile.base_y, this_tw, this_th, 0usize, 0usize)
            } else {
                let mut res_x = self.scale_coordinate(tile.base_x, scale);
                let off_x = res_x.rem_euclid(tw);
                res_x -= off_x;
                let mut res_y = self.scale_coordinate(tile.base_y, scale);
                let off_y = res_y.rem_euclid(th);
                res_y -= off_y;
                (res_x, res_y, tw, th, off_x as usize, off_y as usize)
            };
            let src = self.inner.open_bytes_region(
                0,
                src_x as u32,
                src_y as u32,
                src_w as u32,
                src_h as u32,
            )?;
            for row in iy0..iy1 {
                // Source coordinates within the scaled tile (Java realRow / x-x).
                let sy = src_off_y + (row - box_y) as usize;
                let sx = src_off_x + (ix0 - box_x) as usize;
                let copy_pixels = (ix1 - ix0) as usize;
                let copy_len = copy_pixels * bytes_per_sample;
                let dst_y = (row - req_y0) as usize;
                let dst_x = (ix0 - req_x0) as usize;
                if planar_rgb {
                    let src_plane = src_w as usize * src_h as usize * bytes_per_sample;
                    for c in 0..samples_per_pixel {
                        let s_off = c * src_plane
                            + sy * src_w as usize * bytes_per_sample
                            + sx * bytes_per_sample;
                        let d_off = c * out_plane + dst_y * out_row + dst_x * bytes_per_sample;
                        if s_off + copy_len <= src.len() && d_off + copy_len <= out.len() {
                            out[d_off..d_off + copy_len]
                                .copy_from_slice(&src[s_off..s_off + copy_len]);
                        }
                    }
                } else {
                    let chunky_src_row = src_w as usize * samples_per_pixel * bytes_per_sample;
                    let chunky_out_row = w as usize * samples_per_pixel * bytes_per_sample;
                    let s_off = sy * chunky_src_row + sx * samples_per_pixel * bytes_per_sample;
                    let d_off =
                        dst_y * chunky_out_row + dst_x * samples_per_pixel * bytes_per_sample;
                    let chunky_len = copy_pixels * samples_per_pixel * bytes_per_sample;
                    if s_off + chunky_len <= src.len() && d_off + chunky_len <= out.len() {
                        out[d_off..d_off + chunky_len]
                            .copy_from_slice(&src[s_off..s_off + chunky_len]);
                    }
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
        self.file_name = path.file_name().map(|n| n.to_string_lossy().into_owned());
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.tiles.clear();
        self.areas.clear();
        self.reassemble = false;
        self.stitched_resolution_sizes.clear();
        self.metadata_override = None;
        self.ome_images.clear();
        self.file_name = None;
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        self.inner.set_series(s)?;
        self.refresh_metadata_override();
        Ok(())
    }
    fn series(&self) -> usize {
        self.inner.series()
    }
    fn metadata(&self) -> &ImageMetadata {
        if let Some(meta) = &self.metadata_override {
            return meta;
        }
        self.inner.metadata()
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        // Reassemble the stitched image at the current resolution (Java stitches
        // every resolution by scaling AOI/tile coords; VentanaReader.java:240-312).
        if self.reassemble && self.inner.series() < self.stitched_resolution_sizes.len() {
            if p != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(p));
            }
            let (rx, ry) = {
                let m = self.metadata();
                (m.size_x, m.size_y)
            };
            return self.assemble_region(0, 0, rx, ry);
        }
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.reassemble && self.inner.series() < self.stitched_resolution_sizes.len() {
            if p != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(p));
            }
            let (rx, ry) = {
                let m = self.metadata();
                (m.size_x, m.size_y)
            };
            validate_region("Ventana", rx, ry, x, y, w, h)?;
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
        self.inner.set_resolution(level)?;
        self.refresh_metadata_override();
        Ok(())
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.ome_images.is_empty() {
            return None;
        }
        Some(crate::common::ome_metadata::OmeMetadata {
            images: self.ome_images.clone(),
            instruments: vec![crate::common::ome_metadata::OmeInstrument {
                objectives: vec![crate::common::ome_metadata::OmeObjective::default()],
                ..Default::default()
            }],
            ..Default::default()
        })
    }
}

#[cfg(test)]
mod leica_scn_ventana_tests {
    use super::*;
    use crate::common::reader::FormatReader;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str, ext: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bioformats_tiff_wrappers_{name}_{}_{}.{}",
            std::process::id(),
            unique,
            ext
        ))
    }

    fn tiff_entry(tag: u16, typ: u16, count: u32, value: u32) -> [u8; 12] {
        let mut entry = [0u8; 12];
        entry[0..2].copy_from_slice(&tag.to_le_bytes());
        entry[2..4].copy_from_slice(&typ.to_le_bytes());
        entry[4..8].copy_from_slice(&count.to_le_bytes());
        entry[8..12].copy_from_slice(&value.to_le_bytes());
        entry
    }

    fn write_minimal_scn_tiff(path: &Path, description: &str, photometric: u16) {
        let mut desc = description.as_bytes().to_vec();
        desc.push(0);

        let ifd_entry_count = 11u32;
        let ifd_start = 8u32;
        let desc_start = ifd_start + 2 + ifd_entry_count * 12 + 4;
        let pixel_start = desc_start + desc.len() as u32;

        let entries = [
            tiff_entry(256, 4, 1, 1),
            tiff_entry(257, 4, 1, 1),
            tiff_entry(258, 3, 1, 8),
            tiff_entry(259, 3, 1, 1),
            tiff_entry(262, 3, 1, photometric as u32),
            tiff_entry(270, 2, desc.len() as u32, desc_start),
            tiff_entry(273, 4, 1, pixel_start),
            tiff_entry(277, 3, 1, 1),
            tiff_entry(278, 4, 1, 1),
            tiff_entry(279, 4, 1, 1),
            tiff_entry(284, 3, 1, 1),
        ];

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&desc);
        bytes.push(13);

        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn leica_scn_treats_photometric_rgb_as_rgb_even_with_one_sample() {
        let path = temp_path("photometric_rgb", "scn");
        write_minimal_scn_tiff(
            &path,
            concat!(
                r#"<scn><collection name="c"><image name="main"><pixels>"#,
                r#"<dimension z="0" c="0" r="0" sizeX="1" sizeY="1" ifd="0"/>"#,
                r#"</pixels></image></collection></scn>"#,
            ),
            2,
        );

        let mut reader = LeicaScnReader::new();
        reader.set_id(&path).unwrap();
        assert!(reader.metadata().is_rgb);
        assert_eq!(reader.metadata().size_c, 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![13]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn leica_scn_failed_reopen_clears_cached_ome_metadata() {
        let good = temp_path("good_reopen", "scn");
        let bad = temp_path("bad_reopen", "scn");
        write_minimal_scn_tiff(
            &good,
            concat!(
                r#"<scn><collection name="c"><image name="main"><pixels>"#,
                r#"<dimension z="0" c="0" r="0" sizeX="1" sizeY="1" ifd="0"/>"#,
                r#"</pixels></image></collection></scn>"#,
            ),
            1,
        );
        std::fs::write(&bad, b"not a tiff").unwrap();

        let mut reader = LeicaScnReader::new();
        reader.set_id(&good).unwrap();
        assert!(reader.ome_metadata().is_some());

        assert!(reader.set_id(&bad).is_err());
        assert!(reader.ome_metadata().is_none());

        let _ = std::fs::remove_file(good);
        let _ = std::fs::remove_file(bad);
    }

    #[test]
    fn ventana_stitched_subresolution_size_matches_java_integer_scaling() {
        assert_eq!(
            VentanaReader::stitched_size_for_resolution(1000, 901, 777, 250),
            (225, 194)
        );
        assert_eq!(
            VentanaReader::stitched_size_for_resolution(1000, 901, 777, 333),
            (300, 259)
        );
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
    nis_ome: NikonElementsOmeProjection,
    /// Faithful ND2Handler object graph parsed from the embedded Nikon XML.
    nd2_handler: Nd2Handler,
}

#[derive(Debug, Clone, Default)]
struct NikonElementsOmeProjection {
    rois: Vec<crate::common::ome_metadata::OmeROI>,
    stage_position_x: Option<f64>,
    stage_position_y: Option<f64>,
    stage_position_z: Option<f64>,
}

impl NikonElementsTiffReader {
    pub fn new() -> Self {
        NikonElementsTiffReader {
            inner: crate::tiff::TiffReader::new(),
            nis_ome: NikonElementsOmeProjection::default(),
            nd2_handler: Nd2Handler::default(),
        }
    }

    fn enrich_metadata(&mut self) {
        self.nis_ome = NikonElementsOmeProjection::default();
        self.nd2_handler = Nd2Handler::default();
        let desc = {
            let private_xml = self
                .inner
                .ifd(0)
                .and_then(nikon_elements_private_xml_from_ifd);
            let series = self.inner.series_list();
            private_xml.or_else(|| {
                series.first().and_then(|s| {
                    s.metadata
                        .series_metadata
                        .get("ImageDescription")
                        .and_then(|v| {
                            if let crate::common::metadata::MetadataValue::String(s) = v {
                                Some(s.clone())
                            } else {
                                None
                            }
                        })
                })
            })
        };
        let Some(desc) = desc else { return };

        let mut vendor = std::collections::HashMap::new();

        // Nikon NIS-Elements stores bounded acquisition metadata in XML
        // `<variant>` records and channel elements inside ImageDescription.
        let is_nis_xml = desc.contains("<variant")
            || desc.contains("NIS-Elements")
            || desc.contains("Nikon")
            // Java's NikonElementsTiffReader wraps the embedded Nikon XML in
            // `<NIKON>...</NIKON>` before handing it to ND2Handler.
            || desc.contains("<NIKON")
            || desc.contains("NIKON");
        if is_nis_xml {
            let tags = xml_scan_tags(&desc);
            let variant_count = tags
                .iter()
                .filter(|tag| tag.name.eq_ignore_ascii_case("variant"))
                .count();
            if variant_count > 0 {
                vendor.insert(
                    "nikon.variant_count".to_string(),
                    crate::common::metadata::MetadataValue::Int(variant_count as i64),
                );
                nikon_insert_variant_diagnostics(&mut vendor, &tags);
            }

            let channels: Vec<_> = tags
                .iter()
                .filter(|tag| tag.name.eq_ignore_ascii_case("channel"))
                .collect();
            if !channels.is_empty() {
                vendor.insert(
                    "nikon.channel_count".to_string(),
                    crate::common::metadata::MetadataValue::Int(channels.len() as i64),
                );
                for (i, tag) in channels.iter().enumerate() {
                    for attr in [
                        "name",
                        "dyeName",
                        "wavelength",
                        "excitationWavelength",
                        "emissionWavelength",
                        "exposure",
                        "exposureTime",
                        "gain",
                        "modality",
                        "readoutSpeed",
                        "temperature",
                        "power",
                    ] {
                        if let Some(value) = xml_attr_case_insensitive(&tag.attrs, attr) {
                            insert_parsed_metadata_value(
                                &mut vendor,
                                format!("nikon.channel.{i}.{}", nikon_key_suffix(attr)),
                                value,
                            );
                        }
                    }
                }
            }

            let mut recognized = 0usize;
            for tag in &tags {
                if tag.name.eq_ignore_ascii_case("variant") {
                    for attr in [
                        "runtype",
                        "objectiveName",
                        "magnification",
                        "numericAperture",
                        "calibratedMagnification",
                        "cameraName",
                        "binning",
                    ] {
                        if let Some(value) = xml_attr_case_insensitive(&tag.attrs, attr) {
                            recognized += 1;
                            insert_parsed_metadata_value(
                                &mut vendor,
                                format!("nikon.{}", nikon_key_suffix(attr)),
                                value,
                            );
                        }
                    }
                } else {
                    for tag_name in [
                        "runtype",
                        "objectiveName",
                        "magnification",
                        "numericAperture",
                        "calibratedMagnification",
                        "cameraName",
                        "cameraUniqueName",
                        "binning",
                        "exposure",
                        "exposureTime",
                        "gain",
                        "modality",
                        "readoutSpeed",
                        "temperature",
                        "excitationWavelength",
                        "emissionWavelength",
                        "power",
                    ] {
                        if tag.name.eq_ignore_ascii_case(tag_name) {
                            let value = xml_attr_case_insensitive(&tag.attrs, "value")
                                .map(str::to_string)
                                .or_else(|| xml_element_text(&desc, tag));
                            if let Some(text) = value {
                                recognized += 1;
                                insert_parsed_metadata_value(
                                    &mut vendor,
                                    format!("nikon.{}", nikon_key_suffix(tag_name)),
                                    &text,
                                );
                            }
                        }
                    }
                }
            }

            nikon_insert_shallow_object_metadata(&mut vendor, &desc, &tags);
            nikon_insert_hierarchy_scalar_metadata(&mut vendor, &desc, &tags);

            // Faithful ND2Handler translation: parse the embedded Nikon XML the
            // way Java's NikonElementsTiffReader does (qName -> key, value attr
            // -> value, routed through ND2Handler.parseKeyAndValue).
            // nImages for ND2Handler == the number of planes in the backing
            // TIFF (one IFD per plane), gating the Z/Time Loop direct setters.
            let n_images = self.inner.ifd_count() as i32;
            self.nd2_handler = nd2handler_parse_xml(&desc, &tags, n_images);
            nikon_insert_nd2handler_diagnostics(&mut vendor, &self.nd2_handler);

            self.nis_ome = nikon_elements_ome_projection(&tags);
            if !self.nis_ome.rois.is_empty() {
                vendor.insert(
                    "nikon.ome.roi_count".to_string(),
                    crate::common::metadata::MetadataValue::Int(self.nis_ome.rois.len() as i64),
                );
            }
            if let Some(x) = self.nis_ome.stage_position_x {
                vendor.insert(
                    "nikon.ome.stage_position_x".to_string(),
                    crate::common::metadata::MetadataValue::Float(x),
                );
            }
            if let Some(y) = self.nis_ome.stage_position_y {
                vendor.insert(
                    "nikon.ome.stage_position_y".to_string(),
                    crate::common::metadata::MetadataValue::Float(y),
                );
            }
            if let Some(z) = self.nis_ome.stage_position_z {
                vendor.insert(
                    "nikon.ome.stage_position_z".to_string(),
                    crate::common::metadata::MetadataValue::Float(z),
                );
            }

            if variant_count > 0 && recognized == 0 {
                vendor.insert(
                    "nikon.variant.unparsed_diagnostic".to_string(),
                    crate::common::metadata::MetadataValue::String(
                        "NIS-Elements ImageDescription contained <variant> XML but no supported objective/camera/acquisition attributes".into(),
                    ),
                );
            }
        }

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }

        // Apply the ND2Handler-driven CoreMetadataList reshaping (Z/T/C from
        // dimension loops + XY series multiplication) the way ND2Reader copies
        // ND2Handler's reshaped core back onto its own core list.
        self.apply_nd2_core_reshaping();
    }

    /// Reshape the inner TIFF series list to match the dimensions and series
    /// count the embedded Nikon XML expressed through the ND2Handler.
    ///
    /// Mirrors how `ND2Reader` adopts `ND2Handler`'s reshaped `CoreMetadataList`:
    /// the single inner series' Z/T/C are overwritten from the parsed dimension
    /// loops, and when an `XYPosLoop`/`Dimensions XY(n)` requested `n > 1`
    /// positions the series is duplicated `n` times (each position reads the same
    /// embedded TIFF planes, since the NIS wrapper backs all positions with one
    /// physical TIFF).
    fn apply_nd2_core_reshaping(&mut self) {
        let handler = &self.nd2_handler;
        let size_z = handler.core_size_z;
        let size_t = handler.core_size_t;
        let size_c = handler.core_size_c;
        let series_count = handler.core_series_count.max(1);

        let dims_changed = size_z != 0 || size_t != 0 || size_c != 0;
        if !dims_changed && series_count <= 1 {
            return;
        }

        let template = match self.inner.series_list().first() {
            Some(s) => s.clone(),
            None => return,
        };

        let mut reshaped = template;
        if dims_changed {
            // ND2Handler leaves a dimension at its existing value when the loop
            // did not set it (0); clamp to >= 1 like CoreMetadata.imageCount.
            if size_z != 0 {
                reshaped.metadata.size_z = size_z.max(1);
            }
            if size_t != 0 {
                reshaped.metadata.size_t = size_t.max(1);
            }
            if size_c != 0 {
                reshaped.metadata.size_c = size_c.max(1);
            }
            reshaped.metadata.image_count = reshaped.metadata.size_z.max(1)
                * reshaped.metadata.size_c.max(1)
                * reshaped.metadata.size_t.max(1);
        }

        if series_count <= 1 {
            // Only dimensions changed: rewrite the single series in place.
            let series = self.inner.series_list_mut();
            if let Some(s) = series.first_mut() {
                *s = reshaped;
            }
            return;
        }

        // XY series multiplication: replace the core list with `series_count`
        // copies of the (reshaped) template, mirroring `for i in 0..len { core.add(ms0) }`.
        let mut new_series = Vec::with_capacity(series_count);
        for _ in 0..series_count {
            new_series.push(reshaped.clone());
        }
        self.inner.replace_series(new_series);
    }
}

const NIKON_ELEMENTS_XML_TAG: u16 = 65332;
const NIKON_ELEMENTS_XML_TAG_2: u16 = 65333;

fn tiff_header_has_nikon_elements_xml_tag(header: &[u8]) -> bool {
    let cursor = std::io::Cursor::new(header);
    let mut parser = match crate::tiff::parser::TiffParser::new(cursor) {
        Ok(parser) => parser,
        Err(_) => return false,
    };
    let ifd = match parser.read_ifd(parser.first_ifd_offset) {
        Ok((ifd, _)) => ifd,
        Err(_) => return false,
    };
    ifd.get(NIKON_ELEMENTS_XML_TAG).is_some()
}

fn nikon_elements_private_xml_from_ifd(ifd: &crate::tiff::ifd::Ifd) -> Option<String> {
    nikon_elements_ifd_text_value(ifd, NIKON_ELEMENTS_XML_TAG)
        .and_then(|xml| nikon_elements_prepare_private_xml(&xml))
        .or_else(|| {
            nikon_elements_ifd_text_value(ifd, NIKON_ELEMENTS_XML_TAG_2)
                .and_then(|xml| nikon_elements_prepare_private_xml(&xml))
        })
}

fn nikon_elements_ifd_text_value(ifd: &crate::tiff::ifd::Ifd, tag: u16) -> Option<String> {
    match ifd.get(tag)? {
        crate::tiff::ifd::IfdValue::Ascii(s) => Some(s.clone()),
        crate::tiff::ifd::IfdValue::Byte(bytes) | crate::tiff::ifd::IfdValue::Undefined(bytes) => {
            Some(String::from_utf8_lossy(bytes).into_owned())
        }
        _ => None,
    }
}

fn nikon_elements_prepare_private_xml(xml: &str) -> Option<String> {
    let mut xml = xml.trim().trim_matches('\0').trim();
    if xml.is_empty() {
        return None;
    }
    let open = xml.find('<')?;
    xml = &xml[open..];
    Some(format!("<NIKON>{xml}</NIKON>"))
}

fn nikon_insert_hierarchy_scalar_metadata(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    xml: &str,
    tags: &[XmlTag],
) {
    #[derive(Clone)]
    struct StackNode {
        suffix: String,
        end_offset: usize,
        interesting: bool,
    }

    let mut stack: Vec<StackNode> = Vec::new();
    let mut node_count = 0usize;
    let mut scalar_count = 0usize;

    for tag in tags {
        while stack
            .last()
            .is_some_and(|node| tag.start_offset >= node.end_offset)
        {
            stack.pop();
        }

        let suffix = nikon_key_suffix(&tag.name);
        let interesting = nikon_is_hierarchy_object_tag(&suffix);
        let in_interesting_path = interesting || stack.iter().any(|node| node.interesting);

        if in_interesting_path && suffix != "n_i_s__elements" {
            let mut scalars: Vec<(String, String)> = Vec::new();

            let mut attr_names: Vec<_> = tag.attrs.keys().map(String::as_str).collect();
            attr_names.sort_unstable_by_key(|name| name.to_ascii_lowercase());
            for attr in attr_names.into_iter().take(32) {
                if let Some(value) = xml_attr_case_insensitive(&tag.attrs, attr) {
                    scalars.push((nikon_key_suffix(attr), value.to_string()));
                }
            }

            if let Some(text) = xml_element_text(xml, tag) {
                scalars.push(("text".into(), text.chars().take(4096).collect()));
            }

            if !scalars.is_empty() && node_count < 64 {
                let mut path: Vec<&str> = stack
                    .iter()
                    .filter(|node| node.interesting)
                    .map(|node| node.suffix.as_str())
                    .collect();
                path.push(&suffix);

                let node_key = format!("nikon.hierarchy.{node_count}");
                metadata.insert(
                    format!("{node_key}.path"),
                    crate::common::metadata::MetadataValue::String(path.join(".")),
                );
                metadata.insert(
                    format!("{node_key}.type"),
                    crate::common::metadata::MetadataValue::String(suffix.clone()),
                );
                metadata.insert(
                    format!("{node_key}.depth"),
                    crate::common::metadata::MetadataValue::Int(path.len() as i64),
                );

                for (key, value) in scalars {
                    if scalar_count >= 256 {
                        break;
                    }
                    insert_parsed_metadata_value(metadata, format!("{node_key}.{key}"), &value);
                    scalar_count += 1;
                }
                node_count += 1;
            }
        }

        if !tag.self_closing && stack.len() < 8 {
            let end_offset = xml_matching_end_offset(xml, tag).unwrap_or(xml.len());
            stack.push(StackNode {
                suffix,
                end_offset,
                interesting,
            });
        }

        if node_count >= 64 || scalar_count >= 256 {
            break;
        }
    }

    if node_count > 0 {
        metadata.insert(
            "nikon.hierarchy.node_count".into(),
            crate::common::metadata::MetadataValue::Int(node_count as i64),
        );
        metadata.insert(
            "nikon.hierarchy.scalar_count".into(),
            crate::common::metadata::MetadataValue::Int(scalar_count as i64),
        );
    }
}

fn nikon_insert_variant_diagnostics(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    tags: &[XmlTag],
) {
    let mut unsupported_record_count = 0usize;
    let mut unsupported_attribute_count = 0usize;
    let mut unparsed_record_count = 0usize;

    for (variant_index, tag) in tags
        .iter()
        .filter(|tag| tag.name.eq_ignore_ascii_case("variant"))
        .take(64)
        .enumerate()
    {
        let mut recognized_count = 0usize;
        let mut unsupported: Vec<String> = Vec::new();

        let mut attr_names: Vec<_> = tag.attrs.keys().map(String::as_str).collect();
        attr_names.sort_unstable_by_key(|name| name.to_ascii_lowercase());
        for attr in attr_names.into_iter().take(64) {
            if nikon_is_supported_variant_attr(attr) {
                recognized_count += 1;
            } else {
                unsupported.push(nikon_key_suffix(attr));
            }
        }
        unsupported.sort_unstable();
        unsupported.dedup();

        if recognized_count == 0 {
            unparsed_record_count += 1;
            metadata.insert(
                format!("nikon.variant.{variant_index}.unparsed_diagnostic"),
                crate::common::metadata::MetadataValue::String(
                    "NIS-Elements <variant> record contained no supported objective/camera/acquisition attributes".into(),
                ),
            );
        }

        if !unsupported.is_empty() {
            unsupported_record_count += 1;
            unsupported_attribute_count += unsupported.len();
            metadata.insert(
                format!("nikon.variant.{variant_index}.unsupported_attribute_count"),
                crate::common::metadata::MetadataValue::Int(unsupported.len() as i64),
            );
            metadata.insert(
                format!("nikon.variant.{variant_index}.unsupported_attributes"),
                crate::common::metadata::MetadataValue::String(unsupported.join(",")),
            );
        }
    }

    if unsupported_record_count > 0 {
        metadata.insert(
            "nikon.variant.unsupported_record_count".into(),
            crate::common::metadata::MetadataValue::Int(unsupported_record_count as i64),
        );
        metadata.insert(
            "nikon.variant.unsupported_attribute_count".into(),
            crate::common::metadata::MetadataValue::Int(unsupported_attribute_count as i64),
        );
    }
    if unparsed_record_count > 0 {
        metadata.insert(
            "nikon.variant.unparsed_record_count".into(),
            crate::common::metadata::MetadataValue::Int(unparsed_record_count as i64),
        );
    }
}

fn nikon_is_supported_variant_attr(attr: &str) -> bool {
    matches!(
        nikon_key_suffix(attr).as_str(),
        "runtype"
            | "objective_name"
            | "magnification"
            | "numeric_aperture"
            | "calibrated_magnification"
            | "camera_name"
            | "binning"
    )
}

fn nikon_elements_ome_projection(tags: &[XmlTag]) -> NikonElementsOmeProjection {
    let mut projection = NikonElementsOmeProjection::default();

    for tag in tags {
        match nikon_key_suffix(&tag.name).as_str() {
            "roi" => {
                if projection.rois.len() < 64 {
                    if let Some(roi) = nikon_elements_roi_from_tag(tag, projection.rois.len()) {
                        projection.rois.push(roi);
                    }
                }
            }
            "stage" | "xy_stage" => {
                if projection.stage_position_x.is_none() {
                    projection.stage_position_x =
                        xml_attr_f64_any(tag, &["x", "stageX", "positionX"]);
                }
                if projection.stage_position_y.is_none() {
                    projection.stage_position_y =
                        xml_attr_f64_any(tag, &["y", "stageY", "positionY"]);
                }
                if projection.stage_position_z.is_none() {
                    projection.stage_position_z =
                        xml_attr_f64_any(tag, &["z", "stageZ", "positionZ"]);
                }
            }
            _ => {}
        }
    }

    projection
}

fn nikon_elements_roi_from_tag(
    tag: &XmlTag,
    index: usize,
) -> Option<crate::common::ome_metadata::OmeROI> {
    let x = xml_attr_f64_any(tag, &["x", "left", "centerX", "center_x", "x1"])?;
    let y = xml_attr_f64_any(tag, &["y", "top", "centerY", "center_y", "y1"])?;
    let the_z = xml_attr_u32_any(tag, &["theZ", "the_z", "zIndex", "z_index"]);
    let the_c = xml_attr_u32_any(tag, &["theC", "the_c", "cIndex", "c_index"]);
    let the_t = xml_attr_u32_any(tag, &["theT", "the_t", "tIndex", "t_index"]);

    let shape = if let (Some(x2), Some(y2)) = (
        xml_attr_f64_any(tag, &["x2", "endX", "end_x", "right"]),
        xml_attr_f64_any(tag, &["y2", "endY", "end_y", "bottom"]),
    ) {
        let type_hint = xml_attr_case_insensitive(&tag.attrs, "type")
            .or_else(|| xml_attr_case_insensitive(&tag.attrs, "shape"))
            .map(nikon_key_suffix);
        if type_hint
            .as_deref()
            .is_some_and(|hint| hint == "line" || hint == "polyline")
            || xml_attr_case_insensitive(&tag.attrs, "x1").is_some()
            || xml_attr_case_insensitive(&tag.attrs, "y1").is_some()
            || xml_attr_case_insensitive(&tag.attrs, "endX").is_some()
            || xml_attr_case_insensitive(&tag.attrs, "endY").is_some()
            || xml_attr_case_insensitive(&tag.attrs, "end_x").is_some()
            || xml_attr_case_insensitive(&tag.attrs, "end_y").is_some()
        {
            crate::common::ome_metadata::OmeShape::Line {
                x1: x,
                y1: y,
                x2,
                y2,
                the_z,
                the_t,
                the_c,
            }
        } else {
            let width = x2 - x;
            let height = y2 - y;
            if width <= 0.0 || height <= 0.0 {
                return None;
            }
            crate::common::ome_metadata::OmeShape::Rectangle {
                x,
                y,
                width,
                height,
                the_z,
                the_t,
                the_c,
            }
        }
    } else if let (Some(radius_x), Some(radius_y)) = (
        xml_attr_f64_any(tag, &["radiusX", "radius_x", "rx"]),
        xml_attr_f64_any(tag, &["radiusY", "radius_y", "ry"]),
    ) {
        if radius_x < 0.0 || radius_y < 0.0 {
            return None;
        }
        crate::common::ome_metadata::OmeShape::Ellipse {
            x,
            y,
            radius_x,
            radius_y,
            the_z,
            the_t,
            the_c,
        }
    } else if let Some(radius) = xml_attr_f64_any(tag, &["radius", "r"]) {
        if radius < 0.0 {
            return None;
        }
        crate::common::ome_metadata::OmeShape::Ellipse {
            x,
            y,
            radius_x: radius,
            radius_y: radius,
            the_z,
            the_t,
            the_c,
        }
    } else if let (Some(width), Some(height)) = (
        xml_attr_f64_any(tag, &["width", "w"]),
        xml_attr_f64_any(tag, &["height", "h"]),
    ) {
        let type_hint = xml_attr_case_insensitive(&tag.attrs, "type")
            .or_else(|| xml_attr_case_insensitive(&tag.attrs, "shape"))
            .map(nikon_key_suffix);
        if type_hint
            .as_deref()
            .is_some_and(|hint| hint == "ellipse" || hint == "circle" || hint == "oval")
        {
            if width < 0.0 || height < 0.0 {
                return None;
            }
            crate::common::ome_metadata::OmeShape::Ellipse {
                x,
                y,
                radius_x: width / 2.0,
                radius_y: height / 2.0,
                the_z,
                the_t,
                the_c,
            }
        } else {
            crate::common::ome_metadata::OmeShape::Rectangle {
                x,
                y,
                width,
                height,
                the_z,
                the_t,
                the_c,
            }
        }
    } else if let Some(diameter) = xml_attr_f64_any(tag, &["diameter", "d"]) {
        if diameter < 0.0 {
            return None;
        }
        crate::common::ome_metadata::OmeShape::Ellipse {
            x,
            y,
            radius_x: diameter / 2.0,
            radius_y: diameter / 2.0,
            the_z,
            the_t,
            the_c,
        }
    } else {
        crate::common::ome_metadata::OmeShape::Point {
            x,
            y,
            the_z,
            the_t,
            the_c,
        }
    };

    let id = xml_attr_case_insensitive(&tag.attrs, "id")
        .map(str::to_string)
        .or_else(|| Some(crate::common::ome_metadata::create_lsid("ROI", &[index])));
    let name = xml_attr_case_insensitive(&tag.attrs, "name")
        .or_else(|| xml_attr_case_insensitive(&tag.attrs, "label"))
        .map(str::to_string);

    Some(crate::common::ome_metadata::OmeROI {
        id,
        name,
        shapes: vec![shape],
    })
}

fn xml_attr_f64_any(tag: &XmlTag, names: &[&str]) -> Option<f64> {
    names
        .iter()
        .find_map(|name| xml_attr_case_insensitive(&tag.attrs, name))
        .and_then(|value| value.trim().parse::<f64>().ok())
        .filter(|value| value.is_finite())
}

fn xml_attr_u32_any(tag: &XmlTag, names: &[&str]) -> Option<u32> {
    names
        .iter()
        .find_map(|name| xml_attr_case_insensitive(&tag.attrs, name))
        .and_then(|value| value.trim().parse::<u32>().ok())
}

fn nikon_apply_stage_positions_to_ome(
    ome: &mut crate::common::ome_metadata::OmeMetadata,
    meta: &ImageMetadata,
    projection: &NikonElementsOmeProjection,
) {
    if projection.stage_position_x.is_none()
        && projection.stage_position_y.is_none()
        && projection.stage_position_z.is_none()
    {
        return;
    }
    let Some(image) = ome.images.get_mut(0) else {
        return;
    };

    if image.planes.is_empty() {
        let plane_count = meta.image_count.max(1).min(1024);
        for plane in 0..plane_count {
            let (the_z, the_c, the_t) = nikon_plane_to_zct(plane, meta);
            image.planes.push(crate::common::ome_metadata::OmePlane {
                the_z,
                the_c,
                the_t,
                ..Default::default()
            });
        }
    }

    for plane in &mut image.planes {
        if plane.position_x.is_none() {
            plane.position_x = projection.stage_position_x;
        }
        if plane.position_y.is_none() {
            plane.position_y = projection.stage_position_y;
        }
        if plane.position_z.is_none() {
            plane.position_z = projection.stage_position_z;
        }
    }
}

fn nikon_plane_to_zct(plane: u32, meta: &ImageMetadata) -> (u32, u32, u32) {
    let size_z = meta.size_z.max(1);
    let size_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
    let size_t = meta.size_t.max(1);
    for t in 0..size_t {
        for z in 0..size_z {
            for c in 0..size_c {
                let index = match meta.dimension_order {
                    crate::common::metadata::DimensionOrder::XYZCT => {
                        t * size_z * size_c + c * size_z + z
                    }
                    crate::common::metadata::DimensionOrder::XYZTC => {
                        c * size_z * size_t + t * size_z + z
                    }
                    crate::common::metadata::DimensionOrder::XYCZT => {
                        t * size_c * size_z + z * size_c + c
                    }
                    crate::common::metadata::DimensionOrder::XYCTZ => {
                        z * size_c * size_t + t * size_c + c
                    }
                    crate::common::metadata::DimensionOrder::XYTCZ => {
                        z * size_t * size_c + c * size_t + t
                    }
                    crate::common::metadata::DimensionOrder::XYTZC => {
                        c * size_t * size_z + z * size_t + t
                    }
                };
                if index == plane {
                    return (z, c, t);
                }
            }
        }
    }
    (0, 0, plane)
}

// ---------------------------------------------------------------------------
// ND2Handler-faithful translation for Nikon Elements TIFF.
//
// The Java `NikonElementsTiffReader` (extends `BaseTiffReader`) reads the
// Nikon XML stored in TIFF tag 65332/65333, wraps it in `<NIKON>...</NIKON>`,
// and feeds it to `loci.formats.in.ND2Handler`. That SAX handler treats every
// element's `qName` as a metadata *key* and the element's `value` attribute as
// the *value*, routing them through `parseKeyAndValue`. The handler accumulates
// the typed acquisition object graph (channel names, modalities, binnings,
// readout speeds, gains, temperatures, exposures, ex/em wavelengths, powers,
// objective model/NA/mag/immersion/correction, refractive index, camera model,
// lamp voltage, pinhole, stage positions, ROIs) which the reader projects into
// the OME store.
//
// This is a faithful, in-file translation of ND2Handler's local scalar/object
// branches (one Java method -> one Rust function, exact key names, struct
// carrying the same member variables ND2Handler carries). The genuinely
// cross-file ND2 dimension-loop / CoreMetadataList reshaping (uiCount/XYPosLoop
// series multiplication, uiSequenceCount image-count balancing, Dimensions
// reshaping) is NOT reproduced here: it mutates the TIFF reader's core series
// layout, which is owned by the inner `TiffReader` and is out of scope for the
// metadata enrichment performed in this single file.
#[derive(Debug, Clone)]
struct Nd2Handler {
    // Object graph member variables mirrored from ND2Handler.
    pixel_size_x: Option<f64>,
    pixel_size_y: Option<f64>,
    pixel_size_z: Option<f64>,
    pinhole_size: Option<f64>,
    voltage: Option<f64>,
    mag: Option<f64>,
    na: Option<f64>,
    objective_model: Option<String>,
    immersion: Option<String>,
    correction: Option<String>,
    refractive_index: Option<f64>,
    camera_model: Option<String>,
    date: Option<String>,
    channel_names: Vec<String>,
    modality: Vec<String>,
    binning: Vec<String>,
    speed: Vec<f64>,
    gain: Vec<f64>,
    temperature: Vec<f64>,
    exposure_time: Vec<f64>,
    ex_wave: Vec<f64>,
    em_wave: Vec<f64>,
    power: Vec<i64>,
    pos_x: Vec<f64>,
    pos_y: Vec<f64>,
    pos_z: Vec<f64>,
    rois: Vec<std::collections::HashMap<String, String>>,
    // The most recent element name, mirroring ND2Handler.prevElement, used to
    // gate the "Exposure" branch and to thread dPosX/dPosY/dPosZ item lists.
    prev_element: Option<String>,

    // ---- Cross-file dimension-loop / CoreMetadataList reshaping state ----
    // Mirror of ND2Handler's `ms0` core dimensions for the single embedded
    // series, plus the series-multiplication count derived from XYPosLoop
    // (`uiCount`) / `Dimensions` (`XY...`). These are the only ND2Handler core
    // mutations expressible from the embedded Nikon XML (the NIS reader has
    // exactly one inner TIFF series), so we accumulate them here and let the NIS
    // wrapper apply them to the inner TiffReader's series list.
    /// ms0.sizeZ (0 = not yet set, mirroring Java's int default).
    core_size_z: u32,
    /// ms0.sizeT (0 = not yet set).
    core_size_t: u32,
    /// ms0.sizeC (0 = not yet set).
    core_size_c: u32,
    /// Number of series after reshaping (`core.size()`), 1 until multiplied.
    core_series_count: usize,
    /// Mirror of ND2Handler.imageMetadataLVExists (always false for embedded
    /// Nikon XML, which has no ImageMetadataLV stream).
    image_metadata_lv_exists: bool,
    /// ms0.bitsPerPixel set from `uiBpcSignificant` (None = not yet set).
    core_bits_per_pixel: Option<u32>,
    /// Mirror of ND2Handler `dimensionOrder` for the single embedded series.
    /// VirtualComponents / uiCount loops prepend Z/T/C when absent; carried so
    /// those branches reproduce Java's `indexOf('C') == -1` guards exactly.
    core_dimension_order: String,
    /// Mirror of ND2Handler.ts — distinct timepoint stamps from `dTimeMSec`.
    ts: Vec<i64>,
    /// Mirror of ND2Handler.zs — distinct Z positions from `dZPos`.
    zs: Vec<i64>,
    /// Mirror of ND2Handler.nImages (the number of planes in the backing TIFF),
    /// gating the `Z Stack Loop` / `Time Loop` direct dimension setters.
    n_images: i32,
    /// Mirror of ND2Handler.firstTimeLoop.
    first_time_loop: bool,
    /// Mirror of metadata "number of timepoints" — exposed for diagnostics.
    number_of_timepoints: Option<usize>,
}

impl Default for Nd2Handler {
    fn default() -> Self {
        Nd2Handler {
            pixel_size_x: None,
            pixel_size_y: None,
            pixel_size_z: None,
            pinhole_size: None,
            voltage: None,
            mag: None,
            na: None,
            objective_model: None,
            immersion: None,
            correction: None,
            refractive_index: None,
            camera_model: None,
            date: None,
            channel_names: Vec::new(),
            modality: Vec::new(),
            binning: Vec::new(),
            speed: Vec::new(),
            gain: Vec::new(),
            temperature: Vec::new(),
            exposure_time: Vec::new(),
            ex_wave: Vec::new(),
            em_wave: Vec::new(),
            power: Vec::new(),
            pos_x: Vec::new(),
            pos_y: Vec::new(),
            pos_z: Vec::new(),
            rois: Vec::new(),
            prev_element: None,
            // ND2Handler initialises core with a single CoreMetadata whose int
            // dimensions default to 0, and canAdjustDimensions defaults to true.
            core_size_z: 0,
            core_size_t: 0,
            core_size_c: 0,
            core_series_count: 1,
            image_metadata_lv_exists: false,
            core_bits_per_pixel: None,
            // CoreMetadata's default dimensionOrder ("XYCZT" in Bio-Formats).
            core_dimension_order: "XYCZT".to_string(),
            ts: Vec::new(),
            zs: Vec::new(),
            n_images: 0,
            first_time_loop: true,
            number_of_timepoints: None,
        }
    }
}

impl Nd2Handler {
    /// Mirror of `ND2Handler.parseKeyAndValue(String key, String value, String runtype)`.
    /// Local scalar/object-graph branches plus the embedded-XML dimension-loop /
    /// CoreMetadataList reshaping branches (`uiCount` ZStackLoop/TimeLoop/
    /// XYPosLoop, `Dimensions`) that the NIS wrapper can apply to its single
    /// inner TIFF series.
    fn parse_key_and_value(&mut self, key: &str, value: &str, runtype: Option<&str>) {
        let value = value.trim();
        if key.is_empty() || value.is_empty() {
            return;
        }

        if key.ends_with("dCalibration") {
            if let Ok(v) = value.parse::<f64>() {
                self.pixel_size_x = Some(v);
                self.pixel_size_y = Some(v);
            }
        } else if key.ends_with("dZStep") {
            if let Ok(v) = value.parse::<f64>() {
                self.pixel_size_z = Some(v);
            }
        } else if key.ends_with("Gain") {
            if let Ok(v) = value.parse::<f64>() {
                self.gain.push(v);
            }
        } else if key.ends_with("dLampVoltage") {
            self.voltage = value.parse::<f64>().ok();
        } else if key.ends_with("dObjectiveMag") && self.mag.is_none() {
            self.mag = value.parse::<f64>().ok();
        } else if key.ends_with("dObjectiveNA") {
            self.na = value.parse::<f64>().ok();
        } else if key.ends_with("dRefractIndex1") {
            self.refractive_index = value.parse::<f64>().ok();
        } else if key == "sObjective" || key == "wsObjectiveName" || key == "sOptics" {
            // Parse "Plan Apo 60x Oil"-style objective descriptions.
            self.objective_model = Some(value.to_string());
            let tokens: Vec<&str> = value.split(' ').collect();
            let mag_index = tokens.iter().position(|t| t.contains('x'));
            let mut s = String::new();
            for t in tokens.iter().take(mag_index.unwrap_or(0)) {
                s.push_str(t);
            }
            self.correction = Some(s);
            if let Some(mi) = mag_index {
                if let Some(xpos) = tokens[mi].find('x') {
                    self.mag = tokens[mi][..xpos].parse::<f64>().ok();
                }
                if mi + 1 < tokens.len() {
                    self.immersion = Some(tokens[mi + 1].to_string());
                }
            }
        } else if key == "Name" {
            self.channel_names.push(value.to_string());
        } else if key == "Modality" {
            self.modality.push(value.to_string());
        } else if key == "Camera Type" {
            self.camera_model = Some(value.to_string());
        } else if key == "Binning" {
            self.binning.push(value.to_string());
        } else if key == "Readout Speed" {
            let v = match value.rfind(' ') {
                Some(last) => &value[..last],
                None => value,
            };
            if let Ok(parsed) = v.trim().parse::<f64>() {
                self.speed.push(parsed);
            }
        } else if key == "Temperature" {
            // Java strips all non-digit/non-(-.) characters before parsing.
            let temp: String = value
                .chars()
                .filter(|c| c.is_ascii_digit() || *c == '-' || *c == '.')
                .collect();
            if let Ok(v) = temp.parse::<f64>() {
                self.temperature.push(v);
            }
        } else if key == "Exposure"
            && matches!(
                self.prev_element.as_deref(),
                None | Some("no_name") | Some("PropertiesQuality")
            )
        {
            let parts: Vec<&str> = value.split(' ').collect();
            if let Some(first) = parts.first() {
                if let Ok(mut time) = first.parse::<f64>() {
                    if parts.len() > 1 {
                        if parts[1] == "ms" {
                            time /= 1000.0;
                        }
                    } else {
                        time /= 1000.0;
                    }
                    self.exposure_time.push(time);
                }
            }
        } else if key == "{Pinhole Size}" {
            self.pinhole_size = value.parse::<f64>().ok();
        } else if key.eq_ignore_ascii_case("Emission wavelength") {
            if let Some(first) = value.split(' ').next() {
                if let Ok(v) = first.parse::<f64>() {
                    self.em_wave.push(v);
                }
            }
        } else if key.eq_ignore_ascii_case("Excitation wavelength") {
            if let Some(first) = value.split(' ').next() {
                if let Ok(v) = first.parse::<f64>() {
                    self.ex_wave.push(v);
                }
            }
        } else if key == "Power" {
            if let Ok(v) = value.parse::<f64>() {
                self.power.push(v as i64);
            }
        } else if key == "CameraUniqueName" {
            self.camera_model = Some(value.to_string());
        } else if key == "ExposureTime" {
            if let Ok(v) = value.parse::<f64>() {
                self.exposure_time.push(v / 1000.0);
            }
        } else if key == "sDate" {
            self.date = nikon_nd2_format_date(value);
        } else if key.ends_with("dTimeMSec") {
            // ND2Handler: collect distinct timepoint stamps; the count becomes
            // the "number of timepoints" diagnostic. Java parses as a double and
            // truncates to a long.
            if let Ok(d) = value.parse::<f64>() {
                let v = d as i64;
                if !self.ts.contains(&v) {
                    self.ts.push(v);
                    self.number_of_timepoints = Some(self.ts.len());
                }
            }
        } else if key.ends_with("dZPos") {
            // ND2Handler: collect distinct Z positions.
            if let Ok(v) = value.parse::<i64>() {
                if !self.zs.contains(&v) {
                    self.zs.push(v);
                }
            }
        } else if key.ends_with("uiCount") {
            // ND2Handler: runtype-gated dimension loops. Each loop type either
            // sets a single core dimension (Z/T) or multiplies the series count
            // (XYPosLoop), but only while the core still has a single series.
            if let Some(runtype) = runtype {
                if runtype.ends_with("ZStackLoop") && !self.image_metadata_lv_exists {
                    if self.core_size_z == 0 {
                        if let Ok(v) = value.parse::<u32>() {
                            self.core_size_z = v;
                            if !self.core_dimension_order.contains('Z') {
                                self.core_dimension_order =
                                    format!("Z{}", self.core_dimension_order);
                            }
                        }
                    }
                } else if runtype.ends_with("TimeLoop") && !self.image_metadata_lv_exists {
                    if self.core_size_t == 0 {
                        if let Ok(v) = value.parse::<u32>() {
                            self.core_size_t = v;
                            if !self.core_dimension_order.contains('T') {
                                self.core_dimension_order =
                                    format!("T{}", self.core_dimension_order);
                            }
                        }
                    }
                } else if runtype.ends_with("XYPosLoop") && self.core_series_count == 1 {
                    if let Ok(len) = value.parse::<usize>() {
                        // core = new CoreMetadataList(); for i in 0..len { add(ms0) }
                        self.core_series_count = len.max(1);
                    }
                }
            }
        } else if key.ends_with("uiBpcSignificant") {
            // ND2Handler: significant bits per pixel/colour for ms0.
            if let Ok(v) = value.parse::<u32>() {
                self.core_bits_per_pixel = Some(v);
            }
        } else if key == "VirtualComponents" {
            // ND2Handler: virtual channel count. Sets sizeC only when still
            // unset, and mirrors Java's quirky dimensionOrder concatenation
            // (`dimensionOrder += "C" + dimensionOrder`).
            if self.core_size_c == 0 {
                if let Ok(v) = value.parse::<u32>() {
                    self.core_size_c = v;
                    if !self.core_dimension_order.contains('C') {
                        self.core_dimension_order = format!("{0}C{0}", self.core_dimension_order);
                    }
                }
            }
        } else if key.starts_with("TextInfoItem") || key.ends_with("TextInfoItem") {
            // ND2Handler: nested free-text metadata. Normalise CRLF entities,
            // then split into lines and re-route each "k : v" pair (or "Line:..."
            // run) back through parseKeyAndValue.
            let normalized = value
                .replace("&#x000d;", "")
                .replace("#x000d;", "")
                .replace("&#x000a;", "\n")
                .replace("#x000a;", "\n");
            for line in normalized.split('\n') {
                let t = line.trim();
                // Java's String.split(":") drops trailing empty fields.
                let mut v: Vec<&str> = t.split(':').collect();
                while v.len() > 1 && v.last() == Some(&"") {
                    v.pop();
                }
                if v.is_empty() {
                    continue;
                } else if v.len() == 2 {
                    self.parse_key_and_value(v[0].trim(), v[1].trim(), runtype);
                } else if v[0] == "Line" {
                    let rest = match t.find(':') {
                        Some(c) => t[c + 1..].trim(),
                        None => "",
                    };
                    self.parse_key_and_value(v[0], rest, runtype);
                } else if v.len() > 1 {
                    // metadata.put(v[0] sans braces, v[1]); diagnostic-only.
                    let _ = v[0].replace('{', " ").replace('}', " ");
                }
                // (v.len() == 1: metadata.put(key, v[0]); diagnostic-only)
            }
        } else if Self::is_dimensions(key) && !self.image_metadata_lv_exists {
            // "Dimensions" string e.g. "XY(4) x T(10) x Z(3) x \u{3bb}(2)".
            let dims: Vec<&str> = value.split(" x ").collect();

            if self.core_size_z == 0 {
                self.core_size_z = 1;
            }
            if self.core_size_t == 0 {
                self.core_size_t = 1;
            }
            if self.core_size_c == 0 {
                self.core_size_c = 1;
            }

            for dim in dims {
                let dim = dim.trim();
                let digits: String = dim.chars().filter(|c| c.is_ascii_digit()).collect();
                let v = digits.parse::<u32>().unwrap_or(0).max(1);
                if dim.starts_with("XY") {
                    self.core_series_count = (v as usize).max(1);
                } else if dim.starts_with('T') {
                    if self.core_size_t <= 1 || v < self.core_size_t {
                        self.core_size_t = v;
                    }
                } else if dim.starts_with('Z') {
                    if self.core_size_z <= 1 {
                        self.core_size_z = v;
                    }
                } else if self.core_size_c <= 1 {
                    self.core_size_c = v;
                }
            }
        } else if key.starts_with("Number of Picture Planes") {
            // ND2Handler: alternate channel-count key (strip non-digits).
            let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
            if let Ok(v) = digits.parse::<u32>() {
                self.core_size_c = v;
            }
        } else if key == "Line" {
            // ND2Handler: a ';'-separated list of "k: v" pairs, each re-routed.
            for part in value.split(';') {
                if let Some(colon) = part.find(':') {
                    let next_key = part[..colon].trim().to_string();
                    let next_value = part[colon + 1..].trim().to_string();
                    self.parse_key_and_value(&next_key, &next_value, runtype);
                }
            }
        } else if key.starts_with("- Step") {
            // ND2Handler: physical Z step embedded in the key, "- Step <value>".
            if let Some(step) = Self::parse_pixels_size_z_from_key(key) {
                self.pixel_size_z = Some(step);
            }
        } else if key == "Z Stack Loop" {
            // ND2Handler: direct Z setter, gated so it cannot exceed the plane
            // count (unless nImages is unknown).
            if let Ok(v) = value.parse::<i32>() {
                if v <= self.n_images || self.n_images <= 0 {
                    self.core_size_z = v.max(0) as u32;
                }
            }
        } else if key == "Time Loop" {
            // ND2Handler: direct T setter, applied only once (firstTimeLoop).
            if let Ok(v) = value.parse::<i32>() {
                if v <= self.n_images && self.first_time_loop {
                    self.core_size_t = v.max(0) as u32;
                    self.first_time_loop = false;
                }
            }
        }
    }

    /// Mirror of `ND2Handler.parsePixelsSizeZFromKey(String key)`. The key is
    /// expected to be "- Step <value>"; returns the parsed value or None.
    fn parse_pixels_size_z_from_key(key: &str) -> Option<f64> {
        let step_pos = key.find("Step")?;
        let space = key[step_pos + 1..].find(' ').map(|i| step_pos + 1 + i)?;
        let last = key[space + 1..]
            .find(' ')
            .map(|i| space + 1 + i)
            .unwrap_or(key.len());
        key[space..last].trim().parse::<f64>().ok()
    }

    /// Mirror of `ND2Handler.isDimensions(String key)`.
    fn is_dimensions(key: &str) -> bool {
        key.starts_with("Dimensions") || key.starts_with("Abmessungen")
    }
}

/// Port of `DateTools.formatDate(value, "dd/MM/yyyy  HH:mm:ss")` used by
/// Java's `ND2Handler` for Nikon Elements `sDate` values.
fn nikon_nd2_format_date(value: &str) -> Option<String> {
    let tokens: Vec<&str> = value.split_whitespace().collect();
    if tokens.len() != 2 {
        return None;
    }
    let date: Vec<&str> = tokens[0].split('/').collect();
    let time: Vec<&str> = tokens[1].split(':').collect();
    if date.len() != 3 || time.len() != 3 {
        return None;
    }
    let day: u32 = date[0].parse().ok()?;
    let month: u32 = date[1].parse().ok()?;
    let year: u32 = date[2].parse().ok()?;
    let hour: u32 = time[0].parse().ok()?;
    let minute: u32 = time[1].parse().ok()?;
    let second: u32 = time[2].parse().ok()?;
    if !(1..=31).contains(&day)
        || !(1..=12).contains(&month)
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

/// Mirror of the local branches of `ND2Handler.startElement`: walk the XML tags
/// and route each `qName`/`value` pair through `parse_key_and_value`, capturing
/// stage-position item lists, ROIs (`HorizontalLine`/`VerticalLine`/`Text`), and
/// the `dPinholeRadius` element. Returns the populated handler.
fn nd2handler_parse_xml(xml: &str, tags: &[XmlTag], n_images: i32) -> Nd2Handler {
    let mut handler = Nd2Handler::default();
    // Mirror ND2Handler's constructor `nImages`, gating the Z/Time Stack Loop
    // direct dimension setters.
    handler.n_images = n_images;
    // Track the active dPos{X,Y,Z} list element by recording when we enter one.
    let mut pos_list: Option<char> = None;
    let mut pos_list_end: usize = 0;

    for tag in tags {
        // Pop out of a stage-position item list once we pass its closing tag.
        if pos_list.is_some() && tag.start_offset >= pos_list_end {
            pos_list = None;
        }

        let name = tag.name.as_str();
        let value = xml_attr_case_insensitive(&tag.attrs, "value");
        let runtype = xml_attr_case_insensitive(&tag.attrs, "runtype");

        // dPosX/dPosY/dPosZ open an item_N list of stage coordinates.
        if name == "dPosX" || name == "dPosY" || name == "dPosZ" {
            pos_list = Some(match name {
                "dPosX" => 'x',
                "dPosY" => 'y',
                _ => 'z',
            });
            pos_list_end = xml_matching_end_offset(xml, tag).unwrap_or(xml.len());
        } else if name.starts_with("item_") && pos_list.is_some() {
            if let Some(v) = value.and_then(|s| s.trim().parse::<f64>().ok()) {
                match pos_list {
                    Some('x') => handler.pos_x.push(v),
                    Some('y') => handler.pos_y.push(v),
                    Some('z') => handler.pos_z.push(v),
                    _ => {}
                }
            }
        } else if name == "HorizontalLine" || name == "VerticalLine" || name == "Text" {
            let mut roi: std::collections::HashMap<String, String> =
                std::collections::HashMap::new();
            roi.insert("ROIType".to_string(), name.to_string());
            for (k, v) in &tag.attrs {
                roi.insert(k.clone(), v.clone());
            }
            handler.rois.push(roi);
        } else if name == "dPinholeRadius" {
            if let Some(v) = value.and_then(|s| s.trim().parse::<f64>().ok()) {
                handler.pinhole_size = Some(v);
            }
        } else if let Some(value) = value {
            // Catch-all branch: qName is the key, value attr is the value.
            handler.parse_key_and_value(name, value, runtype);
        }

        // Java updates prevElement only for runtype-bearing elements, but for
        // the gating used by the "Exposure" branch the relevant predecessors are
        // ordinary containers (no_name/PropertiesQuality), so record every
        // non-self-closing element name as the previous element context.
        if !tag.self_closing {
            handler.prev_element = Some(name.to_string());
        }
    }

    handler
}

/// Project the ND2Handler object graph into the OME metadata graph, mirroring
/// `NikonElementsTiffReader.initMetadataStore`.
fn nd2handler_apply_to_ome(
    handler: &Nd2Handler,
    ome: &mut crate::common::ome_metadata::OmeMetadata,
    meta: &ImageMetadata,
) {
    if ome.images.is_empty() {
        return;
    }

    // Pixel sizes / acquisition date on the image.
    {
        let image = &mut ome.images[0];
        if let Some(v) = handler.pixel_size_x {
            image.physical_size_x.get_or_insert(v);
        }
        if let Some(v) = handler.pixel_size_y {
            image.physical_size_y.get_or_insert(v);
        }
        if let Some(v) = handler.pixel_size_z {
            image.physical_size_z.get_or_insert(v);
        }
        if let Some(d) = &handler.date {
            image.acquisition_date.get_or_insert_with(|| d.clone());
        }
    }

    // Instrument: objective + detector (camera).
    let has_objective = handler.objective_model.is_some()
        || handler.na.is_some()
        || handler.mag.is_some()
        || handler.immersion.is_some()
        || handler.correction.is_some();
    let has_detector = handler.camera_model.is_some();
    if has_objective || has_detector {
        if ome.instruments.is_empty() {
            ome.instruments
                .push(crate::common::ome_metadata::OmeInstrument {
                    id: Some(crate::common::ome_metadata::create_lsid("Instrument", &[0])),
                    ..Default::default()
                });
        }
        let instrument = &mut ome.instruments[0];
        if has_objective && instrument.objectives.is_empty() {
            instrument
                .objectives
                .push(crate::common::ome_metadata::OmeObjective {
                    id: Some(crate::common::ome_metadata::create_lsid(
                        "Objective",
                        &[0, 0],
                    )),
                    model: handler.objective_model.clone(),
                    calibrated_magnification: handler.mag,
                    lens_na: handler.na,
                    immersion: Some(handler.immersion.clone().unwrap_or_else(|| "Other".into())),
                    correction: Some(
                        handler
                            .correction
                            .clone()
                            .filter(|c| !c.is_empty())
                            .unwrap_or_else(|| "Other".into()),
                    ),
                    ..Default::default()
                });
        }
        if has_detector && instrument.detectors.is_empty() {
            instrument
                .detectors
                .push(crate::common::ome_metadata::OmeDetector {
                    id: Some(crate::common::ome_metadata::create_lsid(
                        "Detector",
                        &[0, 0],
                    )),
                    model: handler.camera_model.clone(),
                    detector_type: Some("Other".into()),
                    ..Default::default()
                });
        }
        ome.images[0].instrument_ref.get_or_insert(0);
        if has_objective {
            ome.images[0].objective_ref.get_or_insert(0);
        }
    }

    // Per-channel fields.
    let effective_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) } as usize;
    let image = &mut ome.images[0];
    while image.channels.len() < effective_c {
        image
            .channels
            .push(crate::common::ome_metadata::OmeChannel {
                samples_per_pixel: 1,
                ..Default::default()
            });
    }
    for (c, channel) in image.channels.iter_mut().enumerate() {
        if let Some(p) = handler.pinhole_size {
            channel.pinhole_size.get_or_insert(p);
        }
        if let Some(name) = handler.channel_names.get(c) {
            channel.name.get_or_insert_with(|| name.clone());
        }
        if let Some(m) = handler.modality.get(c) {
            channel.acquisition_mode.get_or_insert_with(|| m.clone());
        }
        if let Some(em) = handler.em_wave.get(c) {
            channel.emission_wavelength.get_or_insert(*em);
        }
        if let Some(ex) = handler.ex_wave.get(c) {
            channel.excitation_wavelength.get_or_insert(*ex);
        }
        if let Some(b) = handler.binning.get(c) {
            channel
                .detector_settings_binning
                .get_or_insert_with(|| b.clone());
        }
        if let Some(g) = handler.gain.get(c) {
            channel.detector_settings_gain.get_or_insert(*g);
        }
        if c == 0 {
            if let Some(v) = handler.voltage {
                channel.detector_settings_voltage.get_or_insert(v);
            }
        }
    }

    // Per-plane exposure times (indexed by channel) and stage positions.
    let plane_count = meta.image_count.max(1).min(4096);
    if image.planes.is_empty() {
        for plane in 0..plane_count {
            let (the_z, the_c, the_t) = nikon_plane_to_zct(plane, meta);
            image.planes.push(crate::common::ome_metadata::OmePlane {
                the_z,
                the_c,
                the_t,
                ..Default::default()
            });
        }
    }
    for (i, plane) in image.planes.iter_mut().enumerate() {
        let c = plane.the_c as usize;
        if let Some(t) = handler.exposure_time.get(c) {
            plane.exposure_time.get_or_insert(*t);
        }
        if let Some(x) = handler.pos_x.get(i) {
            plane.position_x.get_or_insert(*x);
        }
        if let Some(y) = handler.pos_y.get(i) {
            plane.position_y.get_or_insert(*y);
        }
        if let Some(z) = handler.pos_z.get(i) {
            plane.position_z.get_or_insert(*z);
        }
    }
}

/// Expose the typed ND2Handler object graph as `nikon.nd2.*` metadata so the
/// translated scalars are observable (and round-trip testable) without having
/// to materialise the full OME store. Mirrors the data members ND2Handler
/// carries; uses ND2Handler's own field semantics (per-channel lists, single
/// scalars).
fn nikon_insert_nd2handler_diagnostics(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    handler: &Nd2Handler,
) {
    use crate::common::metadata::MetadataValue;

    let put_f = |m: &mut std::collections::HashMap<String, MetadataValue>, k: &str, v: f64| {
        m.insert(k.to_string(), MetadataValue::Float(v));
    };

    if let Some(v) = handler.pixel_size_x {
        put_f(metadata, "nikon.nd2.pixel_size_x", v);
    }
    if let Some(v) = handler.pixel_size_y {
        put_f(metadata, "nikon.nd2.pixel_size_y", v);
    }
    if let Some(v) = handler.pixel_size_z {
        put_f(metadata, "nikon.nd2.pixel_size_z", v);
    }
    if let Some(v) = handler.pinhole_size {
        put_f(metadata, "nikon.nd2.pinhole_size", v);
    }
    if let Some(v) = handler.voltage {
        put_f(metadata, "nikon.nd2.voltage", v);
    }
    if let Some(v) = handler.mag {
        put_f(metadata, "nikon.nd2.magnification", v);
    }
    if let Some(v) = handler.na {
        put_f(metadata, "nikon.nd2.numerical_aperture", v);
    }
    if let Some(v) = handler.refractive_index {
        put_f(metadata, "nikon.nd2.refractive_index", v);
    }
    if let Some(v) = &handler.objective_model {
        metadata.insert(
            "nikon.nd2.objective_model".into(),
            MetadataValue::String(v.clone()),
        );
    }
    if let Some(v) = &handler.immersion {
        metadata.insert(
            "nikon.nd2.immersion".into(),
            MetadataValue::String(v.clone()),
        );
    }
    if let Some(v) = &handler.correction {
        if !v.is_empty() {
            metadata.insert(
                "nikon.nd2.correction".into(),
                MetadataValue::String(v.clone()),
            );
        }
    }
    if let Some(v) = &handler.camera_model {
        metadata.insert(
            "nikon.nd2.camera_model".into(),
            MetadataValue::String(v.clone()),
        );
    }
    if let Some(v) = &handler.date {
        metadata.insert("nikon.nd2.date".into(), MetadataValue::String(v.clone()));
    }

    for (i, name) in handler.channel_names.iter().enumerate() {
        metadata.insert(
            format!("nikon.nd2.channel.{i}.name"),
            MetadataValue::String(name.clone()),
        );
    }
    for (i, m) in handler.modality.iter().enumerate() {
        metadata.insert(
            format!("nikon.nd2.channel.{i}.modality"),
            MetadataValue::String(m.clone()),
        );
    }
    for (i, b) in handler.binning.iter().enumerate() {
        metadata.insert(
            format!("nikon.nd2.channel.{i}.binning"),
            MetadataValue::String(b.clone()),
        );
    }
    for (i, v) in handler.speed.iter().enumerate() {
        put_f(
            metadata,
            &format!("nikon.nd2.channel.{i}.readout_speed"),
            *v,
        );
    }
    for (i, v) in handler.gain.iter().enumerate() {
        put_f(metadata, &format!("nikon.nd2.channel.{i}.gain"), *v);
    }
    for (i, v) in handler.exposure_time.iter().enumerate() {
        put_f(
            metadata,
            &format!("nikon.nd2.channel.{i}.exposure_time"),
            *v,
        );
    }
    for (i, v) in handler.ex_wave.iter().enumerate() {
        put_f(
            metadata,
            &format!("nikon.nd2.channel.{i}.excitation_wavelength"),
            *v,
        );
    }
    for (i, v) in handler.em_wave.iter().enumerate() {
        put_f(
            metadata,
            &format!("nikon.nd2.channel.{i}.emission_wavelength"),
            *v,
        );
    }
    for (i, v) in handler.power.iter().enumerate() {
        metadata.insert(
            format!("nikon.nd2.channel.{i}.power"),
            MetadataValue::Int(*v),
        );
    }
    for (i, v) in handler.temperature.iter().enumerate() {
        put_f(metadata, &format!("nikon.nd2.temperature.{i}"), *v);
    }
    for (i, v) in handler.pos_x.iter().enumerate() {
        put_f(metadata, &format!("nikon.nd2.position.{i}.x"), *v);
    }
    for (i, v) in handler.pos_y.iter().enumerate() {
        put_f(metadata, &format!("nikon.nd2.position.{i}.y"), *v);
    }
    for (i, v) in handler.pos_z.iter().enumerate() {
        put_f(metadata, &format!("nikon.nd2.position.{i}.z"), *v);
    }
    if !handler.rois.is_empty() {
        metadata.insert(
            "nikon.nd2.roi_count".into(),
            MetadataValue::Int(handler.rois.len() as i64),
        );
    }
    if let Some(v) = handler.core_bits_per_pixel {
        metadata.insert(
            "nikon.nd2.bits_per_pixel".into(),
            MetadataValue::Int(v as i64),
        );
    }
    if let Some(v) = handler.number_of_timepoints {
        metadata.insert(
            "nikon.nd2.number_of_timepoints".into(),
            MetadataValue::Int(v as i64),
        );
    }
    if !handler.zs.is_empty() {
        metadata.insert(
            "nikon.nd2.z_position_count".into(),
            MetadataValue::Int(handler.zs.len() as i64),
        );
    }
}

fn nikon_insert_shallow_object_metadata(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    xml: &str,
    tags: &[XmlTag],
) {
    let mut object_count = 0usize;
    let mut scalar_count = 0usize;
    for tag in tags
        .iter()
        .filter(|tag| nikon_is_shallow_object_tag(&tag.name))
    {
        if object_count >= 64 || scalar_count >= 256 {
            break;
        }
        let object_key = format!("nikon.object.{object_count}");
        metadata.insert(
            format!("{object_key}.type"),
            crate::common::metadata::MetadataValue::String(nikon_key_suffix(&tag.name)),
        );
        object_count += 1;

        let mut attr_names: Vec<_> = tag.attrs.keys().map(String::as_str).collect();
        attr_names.sort_unstable_by_key(|name| name.to_ascii_lowercase());
        for attr in attr_names.into_iter().take(32) {
            if scalar_count >= 256 {
                break;
            }
            let Some(value) = xml_attr_case_insensitive(&tag.attrs, attr) else {
                continue;
            };
            insert_parsed_metadata_value(
                metadata,
                format!("{object_key}.{}", nikon_key_suffix(attr)),
                value,
            );
            scalar_count += 1;
        }

        if scalar_count < 256 {
            if let Some(text) = xml_element_text(xml, tag) {
                let text: String = text.chars().take(4096).collect();
                insert_parsed_metadata_value(metadata, format!("{object_key}.text"), &text);
                scalar_count += 1;
            }
        }
    }

    if object_count > 0 {
        metadata.insert(
            "nikon.object_count".into(),
            crate::common::metadata::MetadataValue::Int(object_count as i64),
        );
        metadata.insert(
            "nikon.object.scalar_count".into(),
            crate::common::metadata::MetadataValue::Int(scalar_count as i64),
        );
    }
}

fn nikon_is_shallow_object_tag(name: &str) -> bool {
    matches!(
        nikon_key_suffix(name).as_str(),
        "acquisition"
            | "camera"
            | "channel"
            | "channel_description"
            | "detector"
            | "device"
            | "experiment"
            | "filter"
            | "filter_cube"
            | "illumination"
            | "lamp"
            | "laser"
            | "light_source"
            | "metadata"
            | "metadata_seq"
            | "microscope"
            | "objective"
            | "plane"
            | "roi"
            | "stage"
            | "xy_stage"
            | "z_drive"
    )
}

fn nikon_is_hierarchy_object_tag(suffix: &str) -> bool {
    matches!(
        suffix,
        "acquisition"
            | "camera"
            | "channel"
            | "channel_description"
            | "detector"
            | "device"
            | "experiment"
            | "filter"
            | "filter_cube"
            | "illumination"
            | "lamp"
            | "laser"
            | "light_source"
            | "metadata"
            | "metadata_seq"
            | "microscope"
            | "objective"
            | "optical_config"
            | "plane"
            | "roi"
            | "stage"
            | "xy_stage"
            | "z_drive"
    )
}

fn xml_matching_end_offset(xml: &str, tag: &XmlTag) -> Option<usize> {
    if tag.self_closing {
        return Some(tag.body_start);
    }

    let bytes = xml.as_bytes();
    let mut i = tag.body_start;
    let mut depth = 1usize;
    while i < bytes.len() {
        if bytes[i] != b'<' {
            i += 1;
            continue;
        }
        if xml[i..].starts_with("<!--") {
            if let Some(end) = xml[i..].find("-->") {
                i += end + 3;
            } else {
                return None;
            }
            continue;
        }
        let end = xml[i..].find('>')?;
        let inner = xml[i + 1..i + end].trim();
        let closing = inner.strip_prefix('/').map(str::trim_start);
        if let Some(closing) = closing {
            let name_end = closing
                .find(|c: char| c.is_whitespace() || c == '>')
                .unwrap_or(closing.len());
            if closing[..name_end].eq_ignore_ascii_case(&tag.name) {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(i + end + 1);
                }
            }
        } else if !inner.starts_with('!') && !inner.starts_with('?') {
            let self_closing = inner.trim_end().ends_with('/');
            let start = inner.trim_end().trim_end_matches('/');
            let name_end = start
                .find(|c: char| c.is_whitespace())
                .unwrap_or(start.len());
            if start[..name_end].eq_ignore_ascii_case(&tag.name) && !self_closing {
                depth += 1;
            }
        }
        i += end + 1;
    }
    None
}

fn xml_attr_case_insensitive<'a>(
    attrs: &'a std::collections::HashMap<String, String>,
    name: &str,
) -> Option<&'a str> {
    attrs
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
        .filter(|v| !v.trim().is_empty())
}

fn nikon_key_suffix(name: &str) -> String {
    let mut suffix = String::new();
    let chars: Vec<char> = name.chars().collect();
    for (i, ch) in chars.iter().copied().enumerate() {
        if ch.is_ascii_uppercase() {
            let prev = i.checked_sub(1).and_then(|idx| chars.get(idx)).copied();
            let next = chars.get(i + 1).copied();
            let starts_new_word = prev
                .is_some_and(|p| p.is_ascii_lowercase() || p.is_ascii_digit())
                || (prev.is_some_and(|p| p.is_ascii_uppercase())
                    && next.is_some_and(|n| n.is_ascii_lowercase()));
            if i > 0 && starts_new_word {
                suffix.push('_');
            }
            suffix.push(ch.to_ascii_lowercase());
        } else if ch == ' ' || ch == '-' {
            suffix.push('_');
        } else {
            suffix.push(ch.to_ascii_lowercase());
        }
    }
    suffix
}

fn insert_parsed_metadata_value(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    key: String,
    value: &str,
) {
    let value = value.trim();
    if value.is_empty() {
        return;
    }
    if let Ok(f) = value.parse::<f64>() {
        metadata.insert(key, crate::common::metadata::MetadataValue::Float(f));
    } else {
        metadata.insert(
            key,
            crate::common::metadata::MetadataValue::String(value.to_string()),
        );
    }
}

impl Default for NikonElementsTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NikonElementsTiffReader {
    fn is_this_type_by_name(&self, _path: &Path) -> bool {
        false
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        tiff_header_has_nikon_elements_xml_tag(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.nis_ome = NikonElementsOmeProjection::default();
        self.nd2_handler = Nd2Handler::default();
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

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let mut ome = self.inner.ome_metadata().unwrap_or_else(|| {
            crate::common::ome_metadata::OmeMetadata::from_image_metadata(self.metadata())
        });
        if !self.nis_ome.rois.is_empty() {
            ome.rois.extend(self.nis_ome.rois.clone());
        }
        nikon_apply_stage_positions_to_ome(&mut ome, self.metadata(), &self.nis_ome);
        // Project the faithful ND2Handler object graph (channels, objective,
        // detector, exposures, positions) into the OME store.
        nd2handler_apply_to_ome(&self.nd2_handler, &mut ome, self.metadata());
        Some(ome)
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
    image_name: Option<String>,
    image_description: Option<String>,
    date: Option<String>,
    user_name: Option<String>,
    microscope_model: Option<String>,
    stage_x: Option<f64>,
    stage_y: Option<f64>,
    stage_z: Option<f64>,
    size_x: Option<f64>,
    size_y: Option<f64>,
    time_increment: Option<f64>,
    detectors: Vec<String>,
    magnification: Option<f64>,
    helios: bool,
}

const FEI_SFEG_TAG: u16 = 34680;
const FEI_HELIOS_TAG: u16 = 34682;
const FEI_TITAN_TAG: u16 = 34683;
const FEI_MAG_MULTIPLIER: f64 = 0.0024388925;

fn fei_ifd_text_value(value: &crate::tiff::ifd::IfdValue) -> Option<String> {
    match value {
        crate::tiff::ifd::IfdValue::Ascii(s) => Some(s.clone()),
        crate::tiff::ifd::IfdValue::Byte(bytes) | crate::tiff::ifd::IfdValue::Undefined(bytes) => {
            Some(
                String::from_utf8_lossy(bytes)
                    .trim_matches('\0')
                    .to_string(),
            )
        }
        _ => None,
    }
}

fn parse_simple_ini(
    text: &str,
) -> std::collections::HashMap<String, std::collections::HashMap<String, String>> {
    let mut out: std::collections::HashMap<String, std::collections::HashMap<String, String>> =
        std::collections::HashMap::new();
    let mut section = String::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('#') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = line[1..line.len() - 1].trim().to_string();
            out.entry(section.clone()).or_default();
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            let key = key.trim();
            if !key.is_empty() {
                out.entry(section.clone())
                    .or_default()
                    .insert(key.to_string(), value.trim().to_string());
            }
        }
    }
    out
}

fn parse_f64_option(value: Option<&String>) -> Option<f64> {
    value.and_then(|v| v.trim().parse::<f64>().ok())
}

fn fei_format_java_date(value: &str) -> Option<String> {
    let mut parts = value.split_whitespace();
    let date = parts.next()?;
    let time = parts.next()?;
    let am_pm = parts.next()?.to_ascii_uppercase();
    if parts.next().is_some() {
        return None;
    }

    let mut date_parts = date.split('/');
    let month = date_parts.next()?.parse::<u32>().ok()?;
    let day = date_parts.next()?.parse::<u32>().ok()?;
    let year = date_parts.next()?.parse::<u32>().ok()?;
    if date_parts.next().is_some() || !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }

    let mut time_parts = time.split(':');
    let mut hour = time_parts.next()?.parse::<u32>().ok()?;
    let minute = time_parts.next()?.parse::<u32>().ok()?;
    let second = time_parts.next()?.parse::<u32>().ok()?;
    if time_parts.next().is_some() || hour > 23 || minute > 59 || second > 59 {
        return None;
    }
    match am_pm.as_str() {
        "AM" if hour == 12 => hour = 0,
        "AM" => {}
        "PM" if hour < 12 => hour += 12,
        "PM" => {}
        _ => return None,
    }

    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
    ))
}

impl FeiTiffReader {
    pub fn new() -> Self {
        FeiTiffReader {
            inner: crate::tiff::TiffReader::new(),
            image_name: None,
            image_description: None,
            date: None,
            user_name: None,
            microscope_model: None,
            stage_x: None,
            stage_y: None,
            stage_z: None,
            size_x: None,
            size_y: None,
            time_increment: None,
            detectors: Vec::new(),
            magnification: None,
            helios: false,
        }
    }

    fn reset_state(&mut self) {
        self.image_name = None;
        self.image_description = None;
        self.date = None;
        self.user_name = None;
        self.microscope_model = None;
        self.stage_x = None;
        self.stage_y = None;
        self.stage_z = None;
        self.size_x = None;
        self.size_y = None;
        self.time_increment = None;
        self.detectors.clear();
        self.magnification = None;
        self.helios = false;
    }

    fn is_this_type_from_bytes(header: &[u8]) -> bool {
        let cursor = std::io::Cursor::new(header);
        let mut parser = match crate::tiff::parser::TiffParser::new(cursor) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let ifd = match parser.read_ifd(parser.first_ifd_offset) {
            Ok((ifd, _)) => ifd,
            Err(_) => return false,
        };
        ifd.entries.contains_key(&FEI_SFEG_TAG)
            || ifd.entries.contains_key(&FEI_HELIOS_TAG)
            || ifd.entries.contains_key(&FEI_TITAN_TAG)
    }

    fn enrich_metadata(&mut self) {
        self.enrich_private_tag_metadata();
        self.enrich_image_description_metadata();
    }

    fn enrich_image_description_metadata(&mut self) {
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

    fn enrich_private_tag_metadata(&mut self) {
        let Some(ifd) = self.inner.ifd(0).cloned() else {
            return;
        };
        let helios = ifd.entries.contains_key(&FEI_HELIOS_TAG);
        let mut titan = ifd.entries.contains_key(&FEI_TITAN_TAG);
        if titan
            && ifd
                .get(FEI_TITAN_TAG)
                .and_then(fei_ifd_text_value)
                .map(|s| s.trim().is_empty())
                .unwrap_or(false)
        {
            titan = false;
        }

        self.helios = helios;
        let software = if titan {
            "Titan"
        } else if helios {
            "Helios NanoLab"
        } else {
            "S-FEG"
        };

        let tag_key = if titan {
            FEI_TITAN_TAG
        } else if helios {
            FEI_HELIOS_TAG
        } else {
            FEI_SFEG_TAG
        };
        let mut vendor = std::collections::HashMap::new();
        vendor.insert(
            "Software".to_string(),
            crate::common::metadata::MetadataValue::String(software.to_string()),
        );
        let Some(tag) = ifd.get(tag_key).and_then(fei_ifd_text_value) else {
            if let Some(s) = self.inner.series_list_mut().first_mut() {
                for (k, v) in vendor {
                    s.metadata.series_metadata.insert(k, v);
                }
            }
            return;
        };
        let tag = tag.trim().to_string();
        if tag.is_empty() {
            if let Some(s) = self.inner.series_list_mut().first_mut() {
                for (k, v) in vendor {
                    s.metadata.series_metadata.insert(k, v);
                }
            }
            return;
        }

        if tag.starts_with('<') {
            self.parse_fei_xmlish(&tag, &mut vendor);
        } else {
            let ini = parse_simple_ini(&tag);
            self.parse_fei_ini(&ini, helios, &mut vendor);
            for (section, values) in &ini {
                for (key, value) in values {
                    vendor.insert(
                        format!("{section} {key}"),
                        crate::common::metadata::MetadataValue::String(value.clone()),
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

    fn parse_fei_ini(
        &mut self,
        ini: &std::collections::HashMap<String, std::collections::HashMap<String, String>>,
        helios: bool,
        vendor: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    ) {
        if helios {
            if let Some(user) = ini.get("User") {
                let date = user.get("Date").cloned().unwrap_or_default();
                let time = user.get("Time").cloned().unwrap_or_default();
                let combined = format!("{date} {time}").trim().to_string();
                if !combined.is_empty() {
                    self.date = Some(combined.clone());
                    vendor.insert(
                        "Acquisition date".to_string(),
                        crate::common::metadata::MetadataValue::String(combined),
                    );
                }
                self.user_name = user.get("User").cloned();
            }
            self.microscope_model = ini
                .get("System")
                .or_else(|| ini.get("SYSTEM"))
                .and_then(|t| t.get("SystemType"))
                .cloned();

            let beam_table = ini.get("Beam").and_then(|beam| {
                beam.get("Beam")
                    .and_then(|name| ini.get(name))
                    .or(Some(beam))
            });
            let stage_table = ini.get("Stage");
            self.stage_x = beam_table
                .and_then(|t| parse_f64_option(t.get("StageX")))
                .or_else(|| stage_table.and_then(|t| parse_f64_option(t.get("StageX"))));
            self.stage_y = beam_table
                .and_then(|t| parse_f64_option(t.get("StageY")))
                .or_else(|| stage_table.and_then(|t| parse_f64_option(t.get("StageY"))));
            self.stage_z = beam_table
                .and_then(|t| parse_f64_option(t.get("StageZ")))
                .or_else(|| stage_table.and_then(|t| parse_f64_option(t.get("StageZ"))));

            if let Some(scan) = ini.get("Scan") {
                self.size_x = parse_f64_option(scan.get("PixelWidth"));
                self.size_y = parse_f64_option(scan.get("PixelHeight"));
                self.time_increment = parse_f64_option(scan.get("FrameTime"));
            }
        } else {
            if let Some(data) = ini.get("DatabarData") {
                self.image_name = data.get("ImageName").cloned();
                self.image_description = data.get("szUserText").cloned();
            }
            if let Some(mag) = ini
                .get("Vector")
                .and_then(|t| parse_f64_option(t.get("Magnification")))
            {
                self.size_x = Some(mag * FEI_MAG_MULTIPLIER);
                self.size_y = Some(mag * FEI_MAG_MULTIPLIER);
            }
            if let Some(scan) = ini.get("Vector.Sysscan") {
                self.stage_x = parse_f64_option(scan.get("PositionX"));
                self.stage_y = parse_f64_option(scan.get("PositionY"));
            }
            if let Some(detectors) = ini.get("Vector.Video.Detectors") {
                let count = detectors
                    .get("NrDetectorsConnected")
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                for i in 0..count {
                    if let Some(name) = detectors.get(&format!("Detector_{i}_Name")) {
                        self.detectors.push(name.clone());
                    }
                }
            }
        }
    }

    fn parse_fei_xmlish(
        &mut self,
        xml: &str,
        vendor: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    ) {
        let mut stack: Vec<String> = Vec::new();
        let mut label: Option<String> = None;
        let mut pos = 0usize;
        while let Some(start_rel) = xml[pos..].find('<') {
            let start = pos + start_rel;
            if xml[start..].starts_with("</") {
                if let Some(end_rel) = xml[start..].find('>') {
                    let name = xml[start + 2..start + end_rel].trim();
                    if stack.last().map(|s| s.as_str()) == Some(name) {
                        stack.pop();
                    }
                    pos = start + end_rel + 1;
                    continue;
                }
                break;
            }
            if xml[start..].starts_with("<!--")
                || xml[start..].starts_with("<?")
                || xml[start..].starts_with("<!")
            {
                if let Some(end_rel) = xml[start..].find('>') {
                    pos = start + end_rel + 1;
                    continue;
                }
                break;
            }
            let Some(end_rel) = xml[start..].find('>') else {
                break;
            };
            let raw_name = xml[start + 1..start + end_rel].trim();
            let self_closing = raw_name.ends_with('/');
            let name = raw_name
                .trim_end_matches('/')
                .split_whitespace()
                .next()
                .unwrap_or("");
            let body_start = start + end_rel + 1;
            if !name.is_empty() && !self_closing {
                stack.push(name.to_string());
            }
            if let Some(close_rel) = xml[body_start..].find(&format!("</{name}>")) {
                let value = xml_unescape(xml[body_start..body_start + close_rel].trim());
                if !value.is_empty() && !value.contains('<') {
                    let parent = stack.iter().rev().nth(1).map(|s| s.as_str()).unwrap_or("");
                    if name == "Label" {
                        label = Some(value);
                    } else {
                        let key = if name == "Value" {
                            label.clone().unwrap_or_else(|| name.to_string())
                        } else if parent.is_empty() {
                            name.to_string()
                        } else {
                            format!("{parent} {name}")
                        };
                        self.apply_fei_key_value(&key, &value);
                        vendor.insert(key, crate::common::metadata::MetadataValue::String(value));
                        if name == "Value" {
                            label = None;
                        }
                    }
                }
            }
            pos = body_start;
        }
    }

    fn apply_fei_key_value(&mut self, key: &str, value: &str) {
        match key {
            "Stage X" | "StagePosition X" => self.stage_x = value.parse().ok(),
            "Stage Y" | "StagePosition Y" => self.stage_y = value.parse().ok(),
            "Stage Z" | "StagePosition Z" => self.stage_z = value.parse().ok(),
            "Microscope" => self.microscope_model = Some(value.to_string()),
            "User" => self.user_name = Some(value.to_string()),
            "Magnification" => self.magnification = value.parse().ok(),
            _ => {
                if (key.ends_with('X') && key.contains("PixelSize")) || key.ends_with(" pixelWidth")
                {
                    self.size_x = value.parse().ok();
                } else if (key.ends_with('Y') && key.contains("PixelSize"))
                    || key.ends_with(" pixelHeight")
                {
                    self.size_y = value.parse().ok();
                }
            }
        }
    }

    fn build_ome(&self) -> crate::common::ome_metadata::OmeMetadata {
        use crate::common::ome_metadata::{
            create_lsid, OmeDetector, OmeExperimenter, OmeImage, OmeInstrument, OmeMetadata,
            OmeObjective, OmePlane,
        };

        let physical_size_x = self
            .size_x
            .map(|v| if self.helios { v * 1_000_000.0 } else { v });
        let physical_size_y = self
            .size_y
            .map(|v| if self.helios { v * 1_000_000.0 } else { v });

        let mut image = OmeImage {
            name: self.image_name.clone(),
            description: self.image_description.clone(),
            acquisition_date: self.date.as_deref().and_then(fei_format_java_date),
            physical_size_x,
            physical_size_y,
            time_increment: self.time_increment,
            ..Default::default()
        };
        image.planes.push(OmePlane {
            position_x: self.stage_x,
            position_y: self.stage_y,
            position_z: self.stage_z,
            ..Default::default()
        });

        let mut instrument = OmeInstrument {
            id: Some(create_lsid("Instrument", &[0])),
            microscope_model: self.microscope_model.clone(),
            ..Default::default()
        };
        if self.magnification.is_some() {
            instrument.objectives.push(OmeObjective {
                id: Some(create_lsid("Objective", &[0, 0])),
                nominal_magnification: self.magnification,
                correction: Some("Other".to_string()),
                immersion: Some("Other".to_string()),
                ..Default::default()
            });
            image.objective_ref = Some(0);
        }
        for (i, detector) in self.detectors.iter().enumerate() {
            instrument.detectors.push(OmeDetector {
                id: Some(create_lsid("Detector", &[0, i])),
                model: Some(detector.clone()),
                detector_type: Some("Other".to_string()),
                ..Default::default()
            });
        }
        let mut ome = OmeMetadata {
            images: vec![image],
            ..Default::default()
        };
        if self.microscope_model.is_some()
            || self.magnification.is_some()
            || !self.detectors.is_empty()
        {
            ome.images[0].instrument_ref = Some(0);
            ome.instruments.push(instrument);
        }
        if let Some(user) = &self.user_name {
            ome.experimenters.push(OmeExperimenter {
                id: Some(create_lsid("Experimenter", &[0])),
                last_name: Some(user.clone()),
                ..Default::default()
            });
        }
        ome
    }
}

impl Default for FeiTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for FeiTiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let _ = path;
        false
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::is_this_type_from_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.reset_state();
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.reset_state();
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
    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.inner.series_list().is_empty() {
            return None;
        }
        Some(self.build_ome())
    }
}

// ---------------------------------------------------------------------------
// 6. Olympus SIS TIFF — enriched reader
// ---------------------------------------------------------------------------
/// Olympus SIS TIFF (`.tif`).
///
/// Parses ImageDescription for pixel calibration and acquisition metadata
/// stored by Olympus SIS software.
pub struct SisReader {
    inner: crate::tiff::TiffReader,
    current_path: Option<std::path::PathBuf>,
    image_name: Option<String>,
    magnification: Option<f64>,
    channel_name: Option<String>,
    camera_name: Option<String>,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    acquisition_date: Option<String>,
}

const SIS_TAG: u16 = 33560;
const SIS_INI_TAG: u16 = 33471;
const SIS_TAG_2: u16 = 34853;
const TIFF_MAKE: u16 = 271;

impl SisReader {
    pub fn new() -> Self {
        SisReader {
            inner: crate::tiff::TiffReader::new(),
            current_path: None,
            image_name: None,
            magnification: None,
            channel_name: None,
            camera_name: None,
            physical_size_x: None,
            physical_size_y: None,
            acquisition_date: None,
        }
    }

    fn reset_state(&mut self) {
        self.image_name = None;
        self.magnification = None;
        self.channel_name = None;
        self.camera_name = None;
        self.physical_size_x = None;
        self.physical_size_y = None;
        self.acquisition_date = None;
        self.current_path = None;
    }

    fn is_this_type_from_bytes(header: &[u8]) -> bool {
        let cursor = std::io::Cursor::new(header);
        let mut parser = match crate::tiff::parser::TiffParser::new(cursor) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let ifd = match parser.read_ifd(parser.first_ifd_offset) {
            Ok((ifd, _)) => ifd,
            Err(_) => return false,
        };
        let software = ifd.get_str(crate::tiff::ifd::tag::SOFTWARE);
        let make = ifd.get_str(TIFF_MAKE);
        (ifd.entries.contains_key(&SIS_TAG)
            && software.map(|s| s.starts_with("analySIS")).unwrap_or(true))
            || (ifd.entries.contains_key(&SIS_TAG_2)
                && make.map(|s| s.starts_with("Olympus")).unwrap_or(false))
    }

    fn enrich_metadata(&mut self) {
        self.enrich_private_metadata();
        self.enrich_image_description_metadata();
    }

    fn enrich_image_description_metadata(&mut self) {
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

    fn enrich_private_metadata(&mut self) {
        let Some(ifd) = self.inner.ifd(0).cloned() else {
            return;
        };
        if let Some(ini_metadata) = ifd.get(SIS_INI_TAG).and_then(fei_ifd_text_value) {
            self.apply_sis_ini_dimensions(&ini_metadata);
        }

        if !ifd.entries.contains_key(&SIS_TAG) {
            if let Some(pointer) = ifd.get_u64(SIS_TAG_2) {
                let _ = self.parse_sis_tag_2(pointer);
            }
            return;
        }

        let Some(metadata_pointer) = ifd.get_u64(SIS_TAG) else {
            return;
        };
        let _ = self.parse_sis_tag(metadata_pointer);

        let mut vendor = std::collections::HashMap::new();
        if let Some(v) = self.physical_size_x {
            vendor.insert(
                "Nanometers per pixel (X)".to_string(),
                crate::common::metadata::MetadataValue::Float(v),
            );
        }
        if let Some(v) = self.physical_size_y {
            vendor.insert(
                "Nanometers per pixel (Y)".to_string(),
                crate::common::metadata::MetadataValue::Float(v),
            );
        }
        if let Some(v) = self.magnification {
            vendor.insert(
                "Magnification".to_string(),
                crate::common::metadata::MetadataValue::Float(v),
            );
        }
        if let Some(v) = &self.channel_name {
            vendor.insert(
                "Channel name".to_string(),
                crate::common::metadata::MetadataValue::String(v.clone()),
            );
        }
        if let Some(v) = &self.camera_name {
            vendor.insert(
                "Camera name".to_string(),
                crate::common::metadata::MetadataValue::String(v.clone()),
            );
        }
        if let Some(v) = &self.image_name {
            vendor.insert(
                "Image name".to_string(),
                crate::common::metadata::MetadataValue::String(v.clone()),
            );
        }
        if let Some(v) = &self.acquisition_date {
            vendor.insert(
                "Acquisition date".to_string(),
                crate::common::metadata::MetadataValue::String(v.clone()),
            );
        }

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            s.metadata.series_metadata.remove("XResolution");
            s.metadata.series_metadata.remove("YResolution");
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }

    fn apply_sis_ini_dimensions(&mut self, ini_metadata: &str) {
        let ini = parse_simple_ini(ini_metadata);
        let Some(dimensions) = ini.get("Dimension") else {
            return;
        };
        let z = dimensions
            .get("Z")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let c = dimensions
            .get("Band")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        let t = dimensions
            .get("Time")
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        if z.saturating_mul(c).saturating_mul(t) != self.inner.ifd_count() as u32 {
            return;
        }
        let image_count = self.inner.ifd_count() as u32;
        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            s.metadata.size_z = z;
            s.metadata.size_t = t;
            s.metadata.size_c = s.metadata.size_c.saturating_mul(c).max(1);
            s.metadata.image_count = image_count;
        }
    }

    fn parse_sis_tag(&mut self, metadata_pointer: u64) -> Result<()> {
        let bytes = std::fs::read(self.inner_path_for_error()?)?;
        let little = self.inner.is_little_endian();
        let base = usize::try_from(metadata_pointer)
            .map_err(|_| BioFormatsError::Format("SIS metadata offset overflows".into()))?;
        if base + 68 > bytes.len() {
            return Ok(());
        }
        let minute = read_i16_at(&bytes, base + 10, little).unwrap_or(0);
        let hour = read_i16_at(&bytes, base + 12, little).unwrap_or(0);
        let day = read_i16_at(&bytes, base + 14, little).unwrap_or(0);
        let month = read_i16_at(&bytes, base + 16, little).unwrap_or(0) + 1;
        let year = 1900 + read_i16_at(&bytes, base + 18, little).unwrap_or(0);
        self.acquisition_date = Some(format!(
            "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:00"
        ));
        self.image_name = Some(read_c_string(&bytes, base + 26).trim().to_string());

        let entry = base + 60;
        let tag_offset = read_u32_at(&bytes, entry + 4, little).unwrap_or(0) as usize;
        if tag_offset >= bytes.len() {
            return Ok(());
        }
        let meta = if tag_offset > 0 {
            tag_offset
        } else {
            entry + 8
        };
        if meta + 112 > bytes.len() {
            return Ok(());
        }
        let unit_exp = read_i16_at(&bytes, meta + 10, little).unwrap_or(0) as f64;
        let mut physical_size_x = read_f64_at(&bytes, meta + 12, little).unwrap_or(0.0);
        let mut physical_size_y = read_f64_at(&bytes, meta + 20, little).unwrap_or(0.0);
        let maybe_y = read_f64_at(&bytes, meta + 28, little).unwrap_or(physical_size_y);
        if (physical_size_x - physical_size_y).abs() > f64::EPSILON {
            physical_size_x = physical_size_y;
            physical_size_y = maybe_y;
        }
        if (-12.0..=12.0).contains(&unit_exp) {
            let unit_multiplier = 10f64.powf(unit_exp) * 10f64.powi(6);
            physical_size_x *= unit_multiplier;
            physical_size_y *= unit_multiplier;
        }
        self.physical_size_x = Some(physical_size_x);
        self.physical_size_y = Some(physical_size_y);
        self.magnification = read_f64_at(&bytes, meta + 36, little);
        let camera_name_length =
            read_i16_at(&bytes, meta + 44, little).unwrap_or(0).max(0) as usize;
        let channel_name = read_c_string(&bytes, meta + 46).trim().to_string();
        if channel_name.len() > 128 {
            self.channel_name = Some(String::new());
        } else {
            if camera_name_length > 0 {
                let end = camera_name_length.min(channel_name.len());
                self.camera_name = Some(channel_name[..end].to_string());
            }
            self.channel_name = Some(channel_name);
        }
        Ok(())
    }

    fn parse_sis_tag_2(&mut self, pointer: u64) -> Result<()> {
        let bytes = std::fs::read(self.inner_path_for_error()?)?;
        let little = self.inner.is_little_endian();
        let mut pos = usize::try_from(pointer)
            .map_err(|_| BioFormatsError::Format("SIS tag 2 offset overflows".into()))?;
        while pos + 2 <= bytes.len() && &bytes[pos..pos + 2] != b"IS" {
            pos += 1;
        }
        if pos + 38 > bytes.len() {
            return Ok(());
        }
        pos += 30;
        let offset = read_u64_at(&bytes, pos, little).unwrap_or(0);
        let meta = match usize::try_from(offset.saturating_sub(84)) {
            Ok(v) => v,
            Err(_) => return Ok(()),
        };
        if meta + 16 <= bytes.len() {
            self.physical_size_x = read_f64_at(&bytes, meta, little).map(|v| v * 1000.0);
            self.physical_size_y = read_f64_at(&bytes, meta + 8, little).map(|v| v * 1000.0);
        }
        Ok(())
    }

    fn inner_path_for_error(&self) -> Result<&Path> {
        self.inner_path().ok_or_else(|| {
            BioFormatsError::Format("SIS metadata parsing requires an initialized TIFF path".into())
        })
    }

    fn inner_path(&self) -> Option<&Path> {
        self.current_path.as_deref()
    }

    fn build_ome(&self) -> crate::common::ome_metadata::OmeMetadata {
        use crate::common::ome_metadata::{
            create_lsid, OmeChannel, OmeDetector, OmeImage, OmeInstrument, OmeMetadata,
            OmeObjective,
        };

        let mut ome = OmeMetadata::from_image_metadata(self.metadata());
        if ome.images.is_empty() {
            ome.images.push(OmeImage::default());
        }
        let image = &mut ome.images[0];
        image.name = self.image_name.clone();
        image.acquisition_date = self.acquisition_date.clone();
        image.physical_size_x = self.physical_size_x;
        image.physical_size_y = self.physical_size_y;
        image.instrument_ref = Some(0);
        image.objective_ref = Some(0);
        if image.channels.is_empty() {
            image.channels.push(OmeChannel {
                samples_per_pixel: 1,
                ..Default::default()
            });
        }
        image.channels[0].name = self.channel_name.clone();
        image.channels[0].detector_ref = Some(create_lsid("Detector", &[0, 0]));
        let instrument = OmeInstrument {
            id: Some(create_lsid("Instrument", &[0])),
            objectives: vec![OmeObjective {
                id: Some(create_lsid("Objective", &[0, 0])),
                nominal_magnification: self.magnification,
                correction: Some("Other".to_string()),
                immersion: Some("Other".to_string()),
                ..Default::default()
            }],
            detectors: vec![OmeDetector {
                id: Some(create_lsid("Detector", &[0, 0])),
                model: self.camera_name.clone(),
                detector_type: Some("Other".to_string()),
                ..Default::default()
            }],
            ..Default::default()
        };
        if ome.instruments.is_empty() {
            ome.instruments.push(instrument);
        } else {
            ome.instruments[0] = instrument;
        }
        ome
    }
}

impl Default for SisReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SisReader {
    fn is_this_type_by_name(&self, _path: &Path) -> bool {
        false
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::is_this_type_from_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.reset_state();
        self.current_path = Some(path.to_path_buf());
        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.reset_state();
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
    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.inner.series_list().is_empty() {
            return None;
        }
        Some(self.build_ome())
    }
}

fn read_i16_at(bytes: &[u8], offset: usize, little: bool) -> Option<i16> {
    let raw: [u8; 2] = bytes.get(offset..offset + 2)?.try_into().ok()?;
    Some(if little {
        i16::from_le_bytes(raw)
    } else {
        i16::from_be_bytes(raw)
    })
}

fn read_u32_at(bytes: &[u8], offset: usize, little: bool) -> Option<u32> {
    let raw: [u8; 4] = bytes.get(offset..offset + 4)?.try_into().ok()?;
    Some(if little {
        u32::from_le_bytes(raw)
    } else {
        u32::from_be_bytes(raw)
    })
}

fn read_u64_at(bytes: &[u8], offset: usize, little: bool) -> Option<u64> {
    let raw: [u8; 8] = bytes.get(offset..offset + 8)?.try_into().ok()?;
    Some(if little {
        u64::from_le_bytes(raw)
    } else {
        u64::from_be_bytes(raw)
    })
}

fn read_f64_at(bytes: &[u8], offset: usize, little: bool) -> Option<f64> {
    let raw: [u8; 8] = bytes.get(offset..offset + 8)?.try_into().ok()?;
    Some(if little {
        f64::from_le_bytes(raw)
    } else {
        f64::from_be_bytes(raw)
    })
}

fn read_c_string(bytes: &[u8], offset: usize) -> String {
    let Some(rest) = bytes.get(offset..) else {
        return String::new();
    };
    let len = rest.iter().position(|&b| b == 0).unwrap_or(rest.len());
    String::from_utf8_lossy(&rest[..len]).to_string()
}

// ---------------------------------------------------------------------------
// 6b. Nikon TIFF (EZ-C1 confocal) — enriched reader
// ---------------------------------------------------------------------------
/// Nikon TIFF (`.tif` / `.tiff`).
///
/// Faithful port of Java `loci.formats.in.NikonTiffReader`
/// (`extends BaseTiffReader`). Pixels come from the crate's TIFF engine; the
/// EZ-C1 confocal acquisition metadata is scraped from the first IFD's
/// ImageDescription comment. Detection is by the TIFF SOFTWARE tag containing
/// the substring `EZ-C1`, so this reader only claims genuine Nikon EZ-C1 TIFFs.
///
/// Note: this is the Nikon EZ-C1 generic-confocal reader, distinct from
/// `NikonElementsTiffReader` (NIS-Elements/ND2 XML), `NikonNisReader`, and the
/// camera-RAW maker-note reader in `crate::tiff::nikon`.
pub struct NikonTiffReader {
    inner: crate::tiff::TiffReader,

    // -- Fields -- (mirror Java NikonTiffReader fields)
    physical_size_x: f64,
    physical_size_y: f64,
    physical_size_z: f64,
    filter_models: Vec<String>,
    dichroic_models: Vec<String>,
    laser_ids: Vec<String>,
    magnification: Option<f64>,
    lens_na: f64,
    working_distance: f64,
    pinhole_size: f64,
    correction: Option<String>,
    immersion: Option<String>,
    gain: Vec<f64>,
    wavelength: Vec<f64>,
    em_wave: Vec<f64>,
    ex_wave: Vec<f64>,
}

/// Mirrors Java `NikonTiffReader.TOP_LEVEL_KEYS`.
const NIKON_TIFF_TOP_LEVEL_KEYS: &[&str] = &[
    "document document",
    "document",
    "history Acquisition",
    "history objective",
    "history history",
    "history laser",
    "history step",
    "history",
    "sensor s_params",
    "sensor",
    "view",
];

impl NikonTiffReader {
    pub fn new() -> Self {
        NikonTiffReader {
            inner: crate::tiff::TiffReader::new(),
            physical_size_x: 0.0,
            physical_size_y: 0.0,
            physical_size_z: 0.0,
            filter_models: Vec::new(),
            dichroic_models: Vec::new(),
            laser_ids: Vec::new(),
            magnification: None,
            lens_na: 0.0,
            working_distance: 0.0,
            pinhole_size: 0.0,
            correction: None,
            immersion: None,
            gain: Vec::new(),
            wavelength: Vec::new(),
            em_wave: Vec::new(),
            ex_wave: Vec::new(),
        }
    }

    fn reset_standard_metadata_fields(&mut self) {
        self.physical_size_x = 0.0;
        self.physical_size_y = 0.0;
        self.physical_size_z = 0.0;
        self.filter_models.clear();
        self.dichroic_models.clear();
        self.laser_ids.clear();
        self.magnification = None;
        self.lens_na = 0.0;
        self.working_distance = 0.0;
        self.pinhole_size = 0.0;
        self.correction = None;
        self.immersion = None;
        self.gain.clear();
        self.wavelength.clear();
        self.em_wave.clear();
        self.ex_wave.clear();
    }

    /// Mirror Java `NikonTiffReader.isThisType(RandomAccessInputStream)`:
    /// parse the first IFD, read its SOFTWARE tag, and require that it contains
    /// the substring `EZ-C1`. Operates on whatever header bytes are available;
    /// if the SOFTWARE value lies beyond the supplied window the parse fails
    /// gracefully and detection returns `false`.
    fn is_this_type_from_bytes(header: &[u8]) -> bool {
        let cursor = std::io::Cursor::new(header);
        let mut parser = match crate::tiff::parser::TiffParser::new(cursor) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let offset = parser.first_ifd_offset;
        let ifd = match parser.read_ifd(offset) {
            Ok((ifd, _)) => ifd,
            Err(_) => return false,
        };
        match ifd
            .get(crate::tiff::ifd::tag::SOFTWARE)
            .and_then(|v| v.as_str())
        {
            Some(software) => software.contains("EZ-C1"),
            None => false,
        }
    }

    /// Mirror Java `NikonTiffReader.initStandardMetadata()`: parse the
    /// tab-separated key/value pairs in the first IFD's comment
    /// (ImageDescription) into the typed acquisition fields and the global
    /// metadata table.
    fn init_standard_metadata(&mut self) {
        self.reset_standard_metadata_fields();
        let comment = {
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
        let Some(comment) = comment else { return };

        let mut vendor: std::collections::HashMap<String, crate::common::metadata::MetadataValue> =
            std::collections::HashMap::new();

        // Java removes the raw "Comment" entry before re-parsing it.
        let lines: Vec<&str> = comment.split('\n').collect();

        let mut dimension_labels: Option<Vec<String>> = None;
        let mut dimension_sizes: Option<Vec<String>> = None;

        for line in lines {
            let tokens: Vec<&str> = line.split('\t').collect();
            // Java `initStandardMetadata`: `line = line.replaceAll("\t", " ")`
            // before the TOP_LEVEL_KEYS startsWith check. EZ-C1 comment lines are
            // tab-delimited between every token, but the keys contain spaces, so
            // the check must run against the space-normalized line.
            let normalized_line = line.replace('\t', " ");

            let mut n_tokens_in_key = 0usize;
            for key in NIKON_TIFF_TOP_LEVEL_KEYS {
                if normalized_line.starts_with(key) {
                    n_tokens_in_key = if key.contains(' ') { 3 } else { 2 };
                    break;
                }
            }

            let mut k = String::new();
            for i in 0..n_tokens_in_key {
                if i >= tokens.len() {
                    break;
                }
                k.push_str(tokens[i]);
                if i < n_tokens_in_key - 1 {
                    k.push(' ');
                }
            }
            let mut v = String::new();
            for i in n_tokens_in_key..tokens.len() {
                v.push_str(tokens[i]);
                if i < tokens.len() - 1 {
                    v.push(' ');
                }
            }
            let key = k;
            let value = v;

            if key == "document label" {
                dimension_labels = Some(
                    value
                        .to_lowercase()
                        .split(' ')
                        .map(|s| s.to_string())
                        .collect(),
                );
            } else if key == "document scale" {
                dimension_sizes = Some(value.split(' ').map(|s| s.to_string()).collect());
            } else if key.starts_with("history Acquisition") && key.contains("Filter") {
                self.filter_models.push(value.clone());
            } else if key.starts_with("history Acquisition") && key.contains("Dichroic") {
                self.dichroic_models.push(value.clone());
            } else if key == "history objective Type" {
                self.correction = Some(value.clone());
            } else if key == "history objective Magnification" {
                self.magnification = nikon_tiff_parse_double(&value);
            } else if key == "history objective NA" {
                if let Some(d) = nikon_tiff_parse_double(&value) {
                    self.lens_na = d;
                }
            } else if key == "history objective WorkingDistance" {
                if let Some(d) = nikon_tiff_parse_double(&value) {
                    self.working_distance = d;
                }
            } else if key == "history objective Immersion" {
                self.immersion = Some(value.clone());
            } else if key.starts_with("history gain") {
                if let Some(d) = nikon_tiff_parse_double(&value) {
                    self.gain.push(d);
                }
            } else if key == "history pinhole" {
                if let Some(idx) = value.find(' ') {
                    if let Some(d) = nikon_tiff_parse_double(&value[..idx]) {
                        self.pinhole_size = d;
                    }
                }
            } else if key.starts_with("history laser") && key.ends_with("wavelength") {
                // Java: parseDouble(value.replaceAll("\\D", "")) — strip non-digits.
                let digits: String = value.chars().filter(|c| c.is_ascii_digit()).collect();
                if let Some(d) = nikon_tiff_parse_double(&digits) {
                    self.wavelength.push(d);
                }
            } else if key.starts_with("history laser") && key.ends_with("name") {
                self.laser_ids.push(value.clone());
            } else if key == "sensor s_params LambdaEx" {
                for i in n_tokens_in_key..tokens.len() {
                    if let Some(d) = nikon_tiff_parse_double(tokens[i]) {
                        self.ex_wave.push(d);
                    }
                }
            } else if key == "sensor s_params LambdaEm" {
                for i in n_tokens_in_key..tokens.len() {
                    if let Some(d) = nikon_tiff_parse_double(tokens[i]) {
                        self.em_wave.push(d);
                    }
                }
            }

            // Java: addGlobalMeta(key, value) for every parsed line.
            if !key.is_empty() {
                vendor.insert(key, crate::common::metadata::MetadataValue::String(value));
            }
        }

        self.parse_dimension_sizes(dimension_labels.as_deref(), dimension_sizes.as_deref());

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            // Java removes "Comment" from the global metadata before re-parsing.
            s.metadata.series_metadata.remove("Comment");
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }

    /// Mirror Java `NikonTiffReader.parseDimensionSizes(String[], String[])`.
    fn parse_dimension_sizes(&mut self, labels: Option<&[String]>, sizes: Option<&[String]>) {
        let (Some(labels), Some(sizes)) = (labels, sizes) else {
            return;
        };
        for (i, label) in labels.iter().enumerate() {
            let Some(size) = sizes.get(i) else { continue };
            if label.starts_with('z') {
                if let Some(d) = nikon_tiff_parse_double(size) {
                    self.physical_size_z = d;
                }
            } else if label == "x" {
                if let Some(d) = nikon_tiff_parse_double(size) {
                    self.physical_size_x = d;
                }
            } else if label == "y" {
                if let Some(d) = nikon_tiff_parse_double(size) {
                    self.physical_size_y = d;
                }
            }
        }
    }

    /// Mirror Java `NikonTiffReader.initMetadataStore()`: project the typed
    /// acquisition fields onto an OME object graph (physical sizes, objective,
    /// lasers, detectors, per-channel pinhole/ex/em, filters, dichroics).
    fn build_ome(&self) -> crate::common::ome_metadata::OmeMetadata {
        use crate::common::ome_metadata::{
            create_lsid, OmeChannel, OmeDetector, OmeDichroic, OmeFilter, OmeImage, OmeInstrument,
            OmeLightSource, OmeMetadata, OmeObjective,
        };

        let meta = self.inner.metadata();
        let effective_size_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) } as usize;

        let mut image = OmeImage {
            description: Some(String::new()),
            ..Default::default()
        };
        if self.physical_size_x > 0.0 {
            image.physical_size_x = Some(self.physical_size_x);
        }
        if self.physical_size_y > 0.0 {
            image.physical_size_y = Some(self.physical_size_y);
        }
        if self.physical_size_z > 0.0 {
            image.physical_size_z = Some(self.physical_size_z);
        }

        // Objective.
        let correction = self
            .correction
            .clone()
            .unwrap_or_else(|| "Other".to_string());
        let immersion = self
            .immersion
            .clone()
            .unwrap_or_else(|| "Other".to_string());
        let objective = OmeObjective {
            id: Some(create_lsid("Objective", &[0, 0])),
            nominal_magnification: self.magnification,
            correction: Some(correction),
            lens_na: Some(self.lens_na),
            working_distance: Some(self.working_distance),
            immersion: Some(immersion),
            ..Default::default()
        };

        // Lasers (light sources).
        let mut light_sources = Vec::new();
        for i in 0..self.wavelength.len() {
            let wave = self.wavelength[i];
            light_sources.push(OmeLightSource {
                id: Some(create_lsid("LightSource", &[0, i])),
                model: self.laser_ids.get(i).cloned(),
                light_source_type: Some("Other".to_string()),
                wavelength: if wave > 0.0 { Some(wave) } else { None },
                ..Default::default()
            });
        }

        // Detectors.
        let mut detectors = Vec::new();
        for i in 0..self.gain.len() {
            detectors.push(OmeDetector {
                id: Some(create_lsid("Detector", &[0, i])),
                gain: Some(self.gain[i]),
                detector_type: Some("Other".to_string()),
                ..Default::default()
            });
        }

        // Filters / dichroics.
        let mut filters = Vec::new();
        for (i, model) in self.filter_models.iter().enumerate() {
            filters.push(OmeFilter {
                id: Some(create_lsid("Filter", &[0, i])),
                model: Some(model.clone()),
                ..Default::default()
            });
        }
        let mut dichroics = Vec::new();
        for (i, model) in self.dichroic_models.iter().enumerate() {
            dichroics.push(OmeDichroic {
                id: Some(create_lsid("Dichroic", &[0, i])),
                model: Some(model.clone()),
                ..Default::default()
            });
        }

        // Per-channel pinhole / excitation / emission.
        for c in 0..effective_size_c {
            let mut channel = OmeChannel {
                samples_per_pixel: 1,
                pinhole_size: Some(self.pinhole_size),
                ..Default::default()
            };
            if let Some(&wave) = self.ex_wave.get(c) {
                if wave > 0.0 {
                    channel.excitation_wavelength = Some(wave);
                }
            }
            if let Some(&wave) = self.em_wave.get(c) {
                if wave > 0.0 {
                    channel.emission_wavelength = Some(wave);
                }
            }
            image.channels.push(channel);
        }

        let instrument = OmeInstrument {
            id: Some(create_lsid("Instrument", &[0])),
            objectives: vec![objective],
            detectors,
            light_sources,
            filters,
            dichroics,
            ..Default::default()
        };
        image.instrument_ref = Some(0);
        image.objective_ref = Some(0);

        OmeMetadata {
            images: vec![image],
            instruments: vec![instrument],
            ..Default::default()
        }
    }
}

/// Mirror Java `DataTools.parseDouble`: trim, then parse, returning `None`
/// (Java `null`) on failure rather than panicking.
fn nikon_tiff_parse_double(value: &str) -> Option<f64> {
    value.trim().parse::<f64>().ok()
}

impl Default for NikonTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NikonTiffReader {
    fn is_this_type_by_name(&self, _path: &Path) -> bool {
        // Java sets suffixSufficient=false; this reader is selected by the
        // SOFTWARE tag signature, not by the broad .tif/.tiff suffix.
        false
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::is_this_type_from_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        self.init_standard_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.reset_standard_metadata_fields();
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
    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.inner.series_list().is_empty() {
            return None;
        }
        Some(self.build_ome())
    }
}

// ---------------------------------------------------------------------------
// 6b. Metamorph TIFF — enriched reader
// ---------------------------------------------------------------------------
/// Faithful port of Java `loci.formats.in.MetamorphTiffReader`
/// (`extends BaseTiffReader`). Pixels come from the crate's TIFF engine; the
/// per-plane acquisition metadata is scraped from each IFD's ImageDescription
/// comment, which Metamorph (version 7.5+) stores as a `<MetaData>...</MetaData>`
/// XML document of `<prop>` and `<custom-prop>` entries (one `<PlaneInfo>`
/// section per plane).
///
/// Distinct from the `.stk`/UIC-tagged `MetamorphReader` (`metamorph.rs`): this
/// reader handles the XML-comment TIFF variant and the HCS/plate flavour.
///
/// Detection mirrors Java `isThisType`: parse the first IFD's comment, trim it,
/// and require it to start with `<MetaData>` and end with `</MetaData>`.
///
/// Scope note: Java's multi-file HCS discovery (`findTIFFs`, integer-named
/// `NEW_SUBFILE_TYPE == 2` master file globbing across siblings) needs
/// filesystem pattern matching across the dataset. This port implements the
/// single-file path faithfully (the common case — `files = {id}`, `wellCount =
/// 1`), including the full `<MetaData>` comment parse, the per-IFD stage-position
/// field-row inference, the axis-size computation, and the OME projection
/// (per-plane Z/C/T, deltaT, exposure, stage X/Y; per-channel name + emission
/// wavelength; physical sizes; imaging-environment temperature). The well/field
/// stage-label helpers are ported verbatim for parity but only exercise the
/// single-well branch.
pub struct MetamorphTiffReader {
    inner: crate::tiff::TiffReader,

    // -- Fields -- (mirror Java MetamorphTiffReader fields)
    well_count: usize,
    field_row_count: usize,
    field_column_count: usize,
    dual_camera: bool,
    /// OME projection computed once at `set_id` from the parsed handler.
    ome_cache: Option<crate::common::ome_metadata::OmeMetadata>,
}

/// Faithful port of `loci.formats.in.MetamorphHandler`: the SAX handler that
/// accumulates per-plane metadata from a Metamorph `<MetaData>` XML comment.
/// Java drives this with one `parseXML` call per IFD comment, all sharing the
/// same handler instance so the vectors accumulate across planes.
#[derive(Default)]
struct MetamorphHandler {
    timestamps: Vec<String>,
    image_name: Option<String>,
    date: Option<String>,
    wavelengths: Vec<i32>,
    z_positions: Vec<f64>,
    pixel_size_x: f64,
    pixel_size_y: f64,
    temperature: f64,
    lens_na: f64,
    lens_ri: f64,
    binning: Option<String>,
    read_out_rate: f64,
    zoom: f64,
    position_x: Option<f64>,
    position_y: Option<f64>,
    exposures: Vec<Option<f64>>,
    channel_name: Option<String>,
    channel_names: Vec<String>,
    stage_label: Option<String>,
    gain: Option<f64>,
    dual_camera: bool,
}

impl MetamorphHandler {
    /// Mirror Java `MetamorphHandler.startElement`: each `<prop id=.. value=..>`
    /// element contributes a key/value pair (the special `Description` element is
    /// itself a packed multi-line block which is split apart here), routed
    /// through `check_key`.
    fn start_element(&mut self, attrs: &std::collections::HashMap<String, String>) {
        let id = attrs.get("id").map(String::as_str);
        let value = attrs.get("value").map(String::as_str);
        // Java: delim defaults to " #13; #10;" and falls back to the entity form.
        let mut delim = " #13; #10;";
        if let Some(v) = value {
            if !v.contains(delim) {
                delim = "&#13;&#10;";
            }
        }
        let (Some(id), Some(value)) = (id, value) else {
            return;
        };

        if id == "Description" {
            let mut k: Option<String> = None;
            let mut freeform = true;

            if value.contains(delim) {
                for line in value.split(delim) {
                    if line.starts_with("Exposure: ") {
                        freeform = false;
                    }
                    if freeform {
                        // Java accumulates a "User Description" free-form block;
                        // mirrored as an accumulation but only the key/value tail
                        // feeds typed fields.
                        continue;
                    }
                    if let Some(colon) = line.find(':') {
                        let key = line[..colon].trim().to_string();
                        let v = line[colon + 1..].trim().to_string();
                        self.check_key(&key, &v);
                        k = Some(key);
                    } else {
                        // prevent non-key/value lines from being lost (Java puts
                        // k -> k); no typed effect.
                        let _ = &k;
                    }
                }
            } else {
                // Java's single-line colon scan.
                let mut value = value.to_string();
                while let Some(colon) = value.find(':') {
                    let key = value[..colon].to_string();
                    // lastIndexOf(" ", indexOf(":", colon+1))
                    let next_colon = value[colon + 1..].find(':').map(|p| colon + 1 + p);
                    let search_end = next_colon.unwrap_or(value.len());
                    let space = value[..search_end].rfind(' ').unwrap_or(value.len());
                    let v = value[colon + 1..space.max(colon + 1)].trim().to_string();
                    self.check_key(&key, &v);
                    value = value[space.min(value.len())..].trim().to_string();
                }
            }
        } else {
            self.check_key(id, value);
        }
    }

    /// Mirror Java `MetamorphHandler.checkKey`: dispatch a single key/value pair
    /// onto the typed accumulator fields.
    fn check_key(&mut self, key: &str, value: &str) {
        let double_value = metamorph_parse_double(value);
        if let Some(dv) = double_value {
            match key {
                "Temperature" => self.temperature = dv,
                "spatial-calibration-x" => self.pixel_size_x = dv,
                "spatial-calibration-y" => self.pixel_size_y = dv,
                "z-position" => self.z_positions.push(dv),
                "_MagNA_" => self.lens_na = dv,
                "_MagRI_" => self.lens_ri = dv,
                "Readout Frequency" => self.read_out_rate = dv,
                "zoom-percent" => self.zoom = dv,
                "stage-position-x" => self.position_x = Some(dv),
                "stage-position-y" => self.position_y = Some(dv),
                _ => {}
            }
        }

        if key == "wavelength" {
            // usually an integer wavelength in nm; sometimes a descriptive
            // string with no usable nm value, in which case Java logs & skips.
            if let Ok(w) = value.parse::<i32>() {
                self.wavelengths.push(w);
            }
        } else if key == "acquisition-time-local" {
            self.date = Some(value.to_string());
            self.timestamps.push(value.to_string());
        } else if key == "image-name" {
            self.image_name = Some(value.to_string());
        } else if key == "Binning" {
            self.binning = Some(value.to_string());
        } else if key == "Speed" {
            let v = match value.find(' ') {
                Some(space) if space > 0 => &value[..space],
                _ => value,
            };
            if let Some(rate) = metamorph_parse_double(v.trim()) {
                self.read_out_rate = rate;
            }
        } else if key == "Exposure" {
            // exposure times are stored in milliseconds; convert to seconds.
            let v = match value.find(' ') {
                Some(space) => &value[..space],
                None => value,
            };
            let exposure = metamorph_parse_double(v).map(|e| e / 1000.0);
            self.exposures.push(exposure);
        } else if key == "_IllumSetting_" {
            if self.channel_name.is_none() {
                self.channel_name = Some(value.to_string());
            }
            self.channel_names.push(value.to_string());
        } else if key == "stage-label" {
            self.stage_label = Some(value.to_string());
        } else if key.ends_with("Gain") && self.gain.is_none() {
            let cleaned: String = value.chars().filter(|c| *c != 'x' && *c != 'X').collect();
            if let Some(v) = metamorph_parse_double(&cleaned) {
                self.gain = Some(v);
            }
        } else if key.starts_with("Dual Camera") {
            // Determine if Metamorph has already split a dual-camera image: it
            // appends the wavelength number to the Description when splitting.
            // Example: "Dual Camera Time Difference: 7 msec 561".
            match value.rfind(' ') {
                None => self.dual_camera = true,
                Some(space) => {
                    // intentional strict parse (Java uses Double.parseDouble so a
                    // non-number throws, meaning "not split").
                    match value[space..].trim().parse::<f64>() {
                        Ok(_) => self.dual_camera = false,
                        Err(_) => self.dual_camera = true,
                    }
                }
            }
        }
    }
}

/// Mirror Java `MetamorphHandler.parseDouble`: strip '+' (scientific notation
/// may be stored as `-1e+006`) then parse, returning `None` on failure.
fn metamorph_parse_double(v: &str) -> Option<f64> {
    v.replace('+', "").trim().parse::<f64>().ok()
}

impl MetamorphTiffReader {
    pub fn new() -> Self {
        MetamorphTiffReader {
            inner: crate::tiff::TiffReader::new(),
            well_count: 1,
            field_row_count: 1,
            field_column_count: 1,
            dual_camera: false,
            ome_cache: None,
        }
    }

    /// Mirror Java `MetamorphTiffReader.isThisType(RandomAccessInputStream)`:
    /// parse the first IFD, read its comment (ImageDescription), trim it, and
    /// require it to start with `<MetaData>` and end with `</MetaData>`.
    fn is_this_type_from_bytes(header: &[u8]) -> bool {
        let cursor = std::io::Cursor::new(header);
        let mut parser = match crate::tiff::parser::TiffParser::new(cursor) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let offset = parser.first_ifd_offset;
        let ifd = match parser.read_ifd(offset) {
            Ok((ifd, _)) => ifd,
            Err(_) => return false,
        };
        let comment = match ifd
            .get(crate::tiff::ifd::tag::IMAGE_DESCRIPTION)
            .and_then(|v| v.as_str())
        {
            Some(c) => c,
            None => return false,
        };
        let comment = comment.trim();
        comment.starts_with("<MetaData>") && comment.ends_with("</MetaData>")
    }

    /// Mirror Java `MetamorphTiffReader.initFile`: parse the `<MetaData>` XML
    /// comment of every IFD into the shared handler, infer field rows from
    /// distinct stage positions, compute the Z/C/T axis sizes, and store the
    /// typed acquisition results for the OME projection.
    ///
    /// Single-file path only (`files = {id}`); see the struct doc comment.
    fn init_file(&mut self) -> MetamorphHandler {
        // Java: files = {id}; wellCount = fieldRowCount = fieldColumnCount = 1;
        // m.sizeC = 0 in the single-file branch.
        self.well_count = 1;
        self.field_row_count = 1;
        self.field_column_count = 1;

        let mut handler = MetamorphHandler::default();

        // parse XML comment — Java iterates over every IFD's comment.
        let mut x_positions: Vec<Option<f64>> = Vec::new();
        let mut y_positions: Vec<Option<f64>> = Vec::new();

        let ifd_comments: Vec<String> = (0..self.inner.ifd_count())
            .filter_map(|i| {
                self.inner
                    .ifd(i)
                    .and_then(|ifd| ifd.get_str(crate::tiff::ifd::tag::IMAGE_DESCRIPTION))
                    .map(str::to_owned)
            })
            .collect();

        for xml in &ifd_comments {
            let tags = xml_scan_tags(xml);
            for tag in &tags {
                // Metamorph stores each datum as a start tag carrying id/value
                // attributes (<prop id=".." value=".."> / <custom-prop ..>).
                if tag.attrs.contains_key("id") {
                    handler.start_element(&tag.attrs);
                }
            }

            let x = handler.position_x;
            let y = handler.position_y;

            if x_positions.is_empty() {
                x_positions.push(x);
                y_positions.push(y);
            } else if let (Some(x), Some(y)) = (x, y) {
                let previous_x = *x_positions.last().unwrap();
                let previous_y = *y_positions.last().unwrap();
                if let (Some(x2), Some(y2)) = (previous_x, previous_y) {
                    if (x - x2).abs() > 0.21 || (y - y2).abs() > 0.21 {
                        x_positions.push(Some(x));
                        y_positions.push(Some(y));
                    }
                } else {
                    x_positions.push(Some(x));
                    y_positions.push(Some(y));
                }
            }
        }

        if x_positions.len() > 1 {
            self.field_row_count = x_positions.len();
        }

        self.dual_camera = handler.dual_camera;

        // calculate axis sizes (Java initFile, single-file branch m.sizeC == 0).
        let mut unique_c: Vec<i32> = Vec::new();
        for &c in &handler.wavelengths {
            if !unique_c.contains(&c) {
                unique_c.push(c);
            }
        }
        let mut effective_c = unique_c.len();
        if effective_c == 0 {
            effective_c = 1;
        }

        let (samples, base_size_x): (u32, u32) = {
            let series = self.inner.series_list();
            let m = series.first().map(|s| &s.metadata);
            let samples = m
                .map(|m| if m.is_rgb { m.size_c.max(1) } else { 1 })
                .unwrap_or(1);
            let sx = m.map(|m| m.size_x).unwrap_or(0);
            (samples, sx)
        };

        // Java: m.sizeC starts at 0 (single-file), then `if getSizeC()==0 sizeC=1`.
        let mut size_c: u32 = 1;
        size_c *= effective_c as u32 * samples;

        let mut unique_z: Vec<f64> = Vec::new();
        for &z in &handler.z_positions {
            if !unique_z.iter().any(|u| *u == z) {
                unique_z.push(z);
            }
        }

        let mut size_z: u32 = 1;
        if unique_z.len() > 1 {
            size_z *= unique_z.len() as u32;
        }

        let mut physical_size_z: Option<f64> = None;
        if size_z > 1 {
            let z_range =
                handler.z_positions[handler.z_positions.len() - 1] - handler.z_positions[0];
            physical_size_z = Some(z_range.abs() / (size_z as f64 - 1.0));
        }

        let total_planes = self.inner.ifd_count().max(1) as u32;
        let effective_c_planes = size_c / samples.max(1);

        // if channel name and Z are unique per plane, prefer unique channels.
        if effective_c_planes * size_z > total_planes
            && ((!unique_c.is_empty()
                && effective_c_planes * (size_z / unique_c.len() as u32) == total_planes)
                || effective_c_planes == total_planes)
        {
            if size_z >= unique_c.len() as u32 {
                if !unique_c.is_empty() {
                    size_z /= unique_c.len() as u32;
                }
            } else {
                size_z = 1;
            }
        }

        let denom = (self.well_count * self.field_row_count * self.field_column_count) as u32
            * size_z.max(1)
            * effective_c_planes.max(1);
        let mut size_t = if denom > 0 { total_planes / denom } else { 1 };
        if size_t == 0 {
            size_t = 1;
        }

        let series_count = self.well_count * self.field_row_count * self.field_column_count;

        if (series_count > 1 && size_z > total_planes / series_count as u32)
            || total_planes > size_z * size_t * effective_c_planes.max(1)
        {
            size_z = 1;
            let d = (series_count as u32) * effective_c_planes.max(1);
            size_t = if d > 0 { total_planes / d } else { 1 };
        }

        let mut image_count = size_z * size_t * effective_c_planes.max(1);

        let mut final_size_x = base_size_x;
        if self.dual_camera {
            final_size_x /= 2;
            size_c *= 2;
            image_count *= 2;
        }

        // Write computed axis sizes back onto the inner series metadata.
        {
            let series = self.inner.series_list_mut();
            if let Some(s) = series.first_mut() {
                s.metadata.size_x = final_size_x;
                s.metadata.size_c = size_c.max(1);
                s.metadata.size_z = size_z.max(1);
                s.metadata.size_t = size_t.max(1);
                s.metadata.image_count = image_count.max(1);
                metamorph_store_physical_z(&mut s.metadata, physical_size_z);
            }
        }

        handler
    }

    /// Mirror Java `MetamorphTiffReader.getField`: parse the leading scan index
    /// from a stage label of the form `"<n>: Scan <Row><Col>"`.
    #[allow(dead_code)]
    fn get_field(stage_label: &str) -> i32 {
        if !stage_label.contains("Scan") {
            return 0;
        }
        match stage_label.find(':') {
            Some(colon) => stage_label[..colon]
                .trim()
                .parse::<i32>()
                .map(|n| n - 1)
                .unwrap_or(0),
            None => 0,
        }
    }

    /// Mirror Java `MetamorphTiffReader.getWellRow`.
    #[allow(dead_code)]
    fn get_well_row(stage_label: &str) -> i32 {
        let Some(scan_index) = stage_label.find("Scan") else {
            return 0;
        };
        let after = &stage_label[scan_index..];
        match after.find(' ') {
            Some(sp) => {
                let pos = scan_index + sp + 1;
                stage_label[pos..]
                    .chars()
                    .next()
                    .map(|c| (c as i32) - ('A' as i32))
                    .unwrap_or(0)
            }
            None => 0,
        }
    }

    /// Mirror Java `MetamorphTiffReader.getWellColumn`.
    #[allow(dead_code)]
    fn get_well_column(stage_label: &str) -> i32 {
        let Some(scan_index) = stage_label.find("Scan") else {
            return 0;
        };
        let after = &stage_label[scan_index..];
        match after.find(' ') {
            Some(sp) => {
                let pos = scan_index + sp + 2;
                stage_label
                    .get(pos..)
                    .and_then(|s| s.trim().parse::<i32>().ok())
                    .map(|n| n - 1)
                    .unwrap_or(0)
            }
            None => 0,
        }
    }

    /// Mirror Java `MetamorphTiffReader.initFile`'s `MetadataStore` population:
    /// project the typed handler fields onto an OME object graph — image name,
    /// acquisition date, physical sizes, imaging-environment temperature,
    /// per-plane Z/C/T + deltaT + exposure + stage X/Y, and per-channel
    /// name + emission wavelength. Single-series (single-file) path.
    fn build_ome(&self, handler: &MetamorphHandler) -> crate::common::ome_metadata::OmeMetadata {
        use crate::common::ome_metadata::{OmeChannel, OmeImage, OmeMetadata, OmePlane};

        let meta = self.inner.metadata();
        let effective_size_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) } as usize;
        let size_t = meta.size_t.max(1) as usize;

        let mut image = OmeImage {
            name: handler.image_name.clone(),
            description: Some(String::new()),
            ..Default::default()
        };

        if let Some(date) = &handler.date {
            // Java formats the Metamorph "yyyyMMdd HH:mm:ss" date to ISO-8601;
            // store the raw value (downstream OME serialisation is lenient).
            image.acquisition_date = Some(date.clone());
        }

        // Per-plane Z/C/T, deltaT, exposure, stage X/Y.
        // start time = first timestamp (Java DateTools.getTime); deltas computed
        // against it in seconds.
        let start_date = handler
            .timestamps
            .first()
            .and_then(|t| metamorph_parse_timestamp(t));

        let mut img_idx = 0usize;
        for c in 0..effective_size_c {
            for t in 0..size_t {
                let mut plane = OmePlane {
                    the_z: 0,
                    the_c: c as u32,
                    the_t: t as u32,
                    ..Default::default()
                };
                if let (Some(stamp), Some(start)) = (handler.timestamps.get(t), start_date) {
                    if let Some(ms) = metamorph_parse_timestamp(stamp) {
                        plane.delta_t = Some((ms - start) as f64 / 1000.0);
                    }
                }
                let mut exposure_index = img_idx;
                if self.dual_camera && effective_size_c > 0 {
                    exposure_index /= effective_size_c;
                }
                if handler.exposures.len() == 1 {
                    exposure_index = 0;
                }
                if let Some(Some(exp)) = handler.exposures.get(exposure_index) {
                    plane.exposure_time = Some(*exp);
                }
                // Single series: stage X/Y from the first parsed position.
                if let Some(x) = handler.position_x {
                    plane.position_x = Some(x);
                }
                if let Some(y) = handler.position_y {
                    plane.position_y = Some(y);
                }
                image.planes.push(plane);
                img_idx += 1;
            }
        }

        if handler.temperature != 0.0 {
            image.imaging_environment_temperature = Some(handler.temperature);
        }

        if handler.pixel_size_x > 0.0 {
            image.physical_size_x = Some(handler.pixel_size_x);
        }
        if handler.pixel_size_y > 0.0 {
            image.physical_size_y = Some(handler.pixel_size_y);
        }
        if let Some(z) = metamorph_load_physical_z(meta) {
            if z > 0.0 {
                image.physical_size_z = Some(z);
            }
        }

        // Per-channel name + emission wavelength.
        for c in 0..effective_size_c {
            let mut channel = OmeChannel {
                samples_per_pixel: 1,
                ..Default::default()
            };
            if handler.channel_names.len() > c {
                channel.name = Some(handler.channel_names[c].clone());
            } else if let Some(name) = &handler.channel_name {
                channel.name = Some(name.clone());
            }
            if let Some(&w) = handler.wavelengths.get(c) {
                channel.emission_wavelength = Some(w as f64);
            }
            image.channels.push(channel);
        }

        OmeMetadata {
            images: vec![image],
            ..Default::default()
        }
    }
}

/// Parse a Metamorph `"yyyyMMdd HH:mm:ss"` timestamp into epoch milliseconds,
/// mirroring Java `DateTools.getTime(.., DATE_FORMAT, ".")`. Returns `None` on
/// malformed input (Java returns a sentinel which propagates as a skipped delta).
fn metamorph_parse_timestamp(stamp: &str) -> Option<i64> {
    // Format: "20100101 12:30:45" possibly with a ".fraction" suffix.
    let stamp = stamp.trim();
    let (date_part, time_part) = stamp.split_once(' ')?;
    if date_part.len() < 8 {
        return None;
    }
    let year: i64 = date_part[0..4].parse().ok()?;
    let month: i64 = date_part[4..6].parse().ok()?;
    let day: i64 = date_part[6..8].parse().ok()?;
    let time_main = time_part.split('.').next().unwrap_or(time_part);
    let mut hms = time_main.split(':');
    let hour: i64 = hms.next()?.parse().ok()?;
    let minute: i64 = hms.next()?.parse().ok()?;
    let second: i64 = hms.next().unwrap_or("0").parse().ok()?;

    // Days since civil epoch (1970-01-01), proleptic Gregorian (Howard Hinnant).
    let y = if month <= 2 { year - 1 } else { year };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let doy = (153 * (if month > 2 { month - 3 } else { month + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146097 + doe - 719468;
    Some(((days * 24 + hour) * 60 + minute) * 60 * 1000 + second * 1000)
}

/// Stash the computed physical Z size on the series metadata so the OME
/// projection can recover it (the inner `ImageMetadata` has no direct field).
fn metamorph_store_physical_z(meta: &mut ImageMetadata, z: Option<f64>) {
    if let Some(z) = z {
        meta.series_metadata.insert(
            "metamorph.physical_size_z".to_string(),
            crate::common::metadata::MetadataValue::Float(z),
        );
    }
}

fn metamorph_load_physical_z(meta: &ImageMetadata) -> Option<f64> {
    match meta.series_metadata.get("metamorph.physical_size_z") {
        Some(crate::common::metadata::MetadataValue::Float(z)) => Some(*z),
        _ => None,
    }
}

impl Default for MetamorphTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MetamorphTiffReader {
    fn is_this_type_by_name(&self, _path: &Path) -> bool {
        // Java sets suffixSufficient=false; this reader is selected from the
        // TIFF ImageDescription signature, not from the broad .tif/.tiff suffix.
        false
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::is_this_type_from_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)?;
        let handler = self.init_file();
        self.ome_cache = Some(self.build_ome(&handler));
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.well_count = 1;
        self.field_row_count = 1;
        self.field_column_count = 1;
        self.dual_camera = false;
        self.ome_cache = None;
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
    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        self.ome_cache.clone()
    }
}

// ---------------------------------------------------------------------------
// 7. Improvision/Volocity annotated TIFF — enriched reader
// ---------------------------------------------------------------------------
fn metadata_value_from_text(value: &str) -> crate::common::metadata::MetadataValue {
    let trimmed = value.trim();
    if let Ok(i) = trimmed.parse::<i64>() {
        crate::common::metadata::MetadataValue::Int(i)
    } else if let Ok(f) = trimmed.parse::<f64>() {
        crate::common::metadata::MetadataValue::Float(f)
    } else {
        crate::common::metadata::MetadataValue::String(trimmed.to_string())
    }
}

/// Improvision/Volocity annotated TIFF (`.tif`).
///
/// Parses ImageDescription for structured metadata stored by
/// Improvision/PerkinElmer Volocity software.
pub struct ImprovisionTiffReader {
    inner: crate::tiff::TiffReader,
    /// Java `files`: sorted same-`SampleUUID` TIFF siblings when
    /// `MultiFileTIFF=yes`, otherwise just the opened file.
    files: Vec<std::path::PathBuf>,
    /// Java `readers`: one TIFF delegate per entry in `files`.
    readers: Vec<crate::tiff::TiffReader>,
    last_file: usize,
    /// Per-plane channel colours parsed from `WhiteColour` comment lines
    /// (mirrors Java `ArrayList<Color> channelColors`). `None` marks a plane
    /// whose `WhiteColour` value had fewer than three components.
    channel_colors: Vec<Option<i32>>,
    /// Per-channel names indexed by `ChannelNo - 1` (mirrors Java `String[] cNames`).
    c_names: Vec<Option<String>>,
    /// Average time per plane in microseconds (mirrors Java `int pixelSizeT`).
    pixel_size_t: i64,
    /// Physical pixel size in X (micrometres) from `XCalibrationMicrons`.
    pixel_size_x: f64,
    /// Physical pixel size in Y (micrometres) from `YCalibrationMicrons`.
    pixel_size_y: f64,
    /// Physical pixel size in Z (micrometres) from `ZCalibrationMicrons`.
    pixel_size_z: f64,
    /// OME image metadata built from the parsed calibration / channel fields.
    ome_images: Vec<crate::common::ome_metadata::OmeImage>,
}

impl ImprovisionTiffReader {
    const IMPROVISION_MAGIC_STRING: &'static str = "Improvision";

    pub fn new() -> Self {
        ImprovisionTiffReader {
            inner: crate::tiff::TiffReader::new(),
            files: Vec::new(),
            readers: Vec::new(),
            last_file: 0,
            channel_colors: Vec::new(),
            c_names: Vec::new(),
            pixel_size_t: 1,
            pixel_size_x: 0.0,
            pixel_size_y: 0.0,
            pixel_size_z: 0.0,
            ome_images: Vec::new(),
        }
    }

    /// Collect every IFD's ImageDescription comment, mirroring Java's
    /// `ifds.get(plane).getComment()` loop.
    fn plane_comments(&self) -> Vec<String> {
        use crate::tiff::ifd::tag;
        (0..self.inner.ifd_count())
            .map(|i| {
                self.inner
                    .ifd(i)
                    .and_then(|ifd| ifd.get_str(tag::IMAGE_DESCRIPTION))
                    .unwrap_or("")
                    .to_string()
            })
            .collect()
    }

    /// Translate Java `initStandardMetadata` field-filling: parse the per-plane
    /// comments to populate `pixel_size_{x,y,z}`, `channel_colors`, `c_names`
    /// and `pixel_size_t`. Returns `size_c` used to size `c_names`.
    fn parse_comments(&mut self, comments: &[String]) -> bool {
        let mut total_z: Option<u32> = None;
        let mut total_c: Option<u32> = None;
        let mut total_t: Option<u32> = None;
        let mut coords: Vec<[i32; 3]> = vec![[-1, -1, -1]; comments.len()];
        let mut raw_metadata: Vec<(String, crate::common::metadata::MetadataValue)> = Vec::new();
        let mut multiple_files = false;

        // First pass: calibration + WhiteColour (mirrors Java lines 170-219).
        for (plane, comment) in comments.iter().enumerate() {
            for line in comment.split('\n') {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                raw_metadata.push((
                    format!("improvision.{}", key.trim()),
                    metadata_value_from_text(value),
                ));
                match key {
                    "TotalZPlanes" => {
                        total_z = value.trim().parse::<u32>().ok();
                    }
                    "TotalChannels" => {
                        total_c = value.trim().parse::<u32>().ok();
                    }
                    "TotalTimepoints" => {
                        total_t = value.trim().parse::<u32>().ok();
                    }
                    "XCalibrationMicrons" => {
                        if let Ok(v) = value.parse::<f64>() {
                            self.pixel_size_x = v;
                        }
                    }
                    "YCalibrationMicrons" => {
                        if let Ok(v) = value.parse::<f64>() {
                            self.pixel_size_y = v;
                        }
                    }
                    "ZCalibrationMicrons" => {
                        if let Ok(v) = value.parse::<f64>() {
                            self.pixel_size_z = v;
                        }
                    }
                    "WhiteColour" => {
                        let rgb: Vec<&str> = value.split(',').collect();
                        if rgb.len() < 3 {
                            self.channel_colors.push(None);
                            continue;
                        }
                        // Java defaults each component to 255 on parse failure.
                        let red = rgb[0].trim().parse::<i32>().unwrap_or(255);
                        let green = rgb[1].trim().parse::<i32>().unwrap_or(255);
                        let blue = rgb[2].trim().parse::<i32>().unwrap_or(255);
                        self.channel_colors
                            .push(Some((red << 24) | (green << 16) | (blue << 8) | 0xff));
                    }
                    "ZPlane" => {
                        coords[plane][0] = value.trim().parse::<i32>().unwrap_or(-1);
                    }
                    "ChannelNo" => {
                        coords[plane][1] = value.trim().parse::<i32>().unwrap_or(-1);
                    }
                    "TimepointName" => {
                        coords[plane][2] = value.trim().parse::<i32>().unwrap_or(-1);
                    }
                    "MultiFileTIFF" => {
                        multiple_files = value.trim().eq_ignore_ascii_case("yes");
                    }
                    _ => {}
                }
            }
        }

        self.apply_improvision_dimensions(total_z, total_c, total_t, comments.len(), &coords);

        if let Some(s) = self.inner.series_list_mut().first_mut() {
            for (key, value) in raw_metadata {
                s.metadata.series_metadata.entry(key).or_insert(value);
            }
        }

        // Determine size_c the way Java does (TotalChannels multiplier, etc.).
        let size_c = self
            .inner
            .series_list()
            .first()
            .map_or(1, |s| s.metadata.size_c.max(1) as usize);

        // Second pass: timestamps + channel names (mirrors Java lines 245-284).
        self.c_names = vec![None; size_c];
        let mut stamps: Vec<i64> = vec![0; comments.len()];
        for (i, comment) in comments.iter().enumerate() {
            let comment = comment.replace("\r\n", "\n").replace('\r', "\n");
            let mut channel_name: Option<String> = None;
            for line in comment.split('\n') {
                let Some((key, value)) = line.split_once('=') else {
                    continue;
                };
                match key {
                    "TimeStampMicroSeconds" => {
                        if let Ok(v) = value.parse::<i64>() {
                            stamps[i] = v;
                        }
                    }
                    "ChannelNo" => {
                        if let Ok(no) = value.parse::<i32>() {
                            let ndx = (no - 1) as usize;
                            if ndx < self.c_names.len() && self.c_names[ndx].is_none() {
                                self.c_names[ndx] = channel_name.clone();
                            }
                        }
                    }
                    "ChannelName" => {
                        channel_name = Some(value.to_string());
                    }
                    _ => {}
                }
            }
        }

        // Average time per plane (mirrors Java lines 328-333).
        let size_t = self
            .inner
            .series_list()
            .first()
            .map_or(1, |s| s.metadata.size_t.max(1)) as i64;
        let mut sum: i64 = 0;
        for i in 1..stamps.len() {
            let diff = stamps[i] - stamps[i - 1];
            if diff > 0 {
                sum += diff;
            }
        }
        if size_t > 0 {
            self.pixel_size_t = sum / size_t;
        }
        multiple_files
    }

    fn apply_improvision_dimensions(
        &mut self,
        total_z: Option<u32>,
        total_c: Option<u32>,
        total_t: Option<u32>,
        ifd_count: usize,
        coords: &[[i32; 3]],
    ) {
        use crate::common::metadata::DimensionOrder;

        let Some(series) = self.inner.series_list_mut().first_mut() else {
            return;
        };
        let m = &mut series.metadata;
        m.size_t = 1;
        if m.size_z == 0 {
            m.size_z = 1;
        }
        if m.size_c == 0 {
            m.size_c = 1;
        }
        if let Some(z) = total_z {
            m.size_z = m.size_z.saturating_mul(z.max(1));
        }
        if let Some(c) = total_c {
            m.size_c = m.size_c.saturating_mul(c.max(1));
        }
        if let Some(t) = total_t {
            m.size_t = m.size_t.saturating_mul(t.max(1));
        }

        let logical = m.size_z.saturating_mul(m.size_c).saturating_mul(m.size_t);
        if logical < m.image_count {
            m.size_c = m.size_c.saturating_mul(m.image_count.max(1));
        } else if let Some(c) = total_c {
            m.image_count = m.size_z.saturating_mul(m.size_t).saturating_mul(c.max(1));
        } else if logical > 0 {
            m.image_count = logical;
        }

        if ifd_count > 0 && (ifd_count as u32).saturating_mul(1) < m.image_count {
            m.series_metadata.insert(
                "improvision.multifile_required".into(),
                crate::common::metadata::MetadataValue::Bool(true),
            );
        }

        let mut order = String::from("XY");
        if m.is_rgb {
            order.push('C');
        }
        for i in 1..coords.len() {
            let z_diff = coords[i][0] - coords[i - 1][0];
            let c_diff = coords[i][1] - coords[i - 1][1];
            let t_diff = coords[i][2] - coords[i - 1][2];
            if z_diff > 0 && !order.contains('Z') {
                order.push('Z');
            }
            if c_diff > 0 && !order.contains('C') {
                order.push('C');
            }
            if t_diff > 0 && !order.contains('T') {
                order.push('T');
            }
            if order.len() == 5 {
                break;
            }
        }
        if !order.contains('Z') {
            order.push('Z');
        }
        if !order.contains('C') {
            order.push('C');
        }
        if !order.contains('T') {
            order.push('T');
        }
        m.dimension_order = match order.as_str() {
            "XYZCT" => DimensionOrder::XYZCT,
            "XYZTC" => DimensionOrder::XYZTC,
            "XYCZT" => DimensionOrder::XYCZT,
            "XYCTZ" => DimensionOrder::XYCTZ,
            "XYTCZ" => DimensionOrder::XYTCZ,
            "XYTZC" => DimensionOrder::XYTZC,
            _ => m.dimension_order,
        };
    }

    /// Translate Java `initMetadataStore`: build OME image/channel metadata from
    /// the parsed calibration / channel fields.
    fn build_ome(&mut self) {
        use crate::common::ome_metadata::{OmeChannel, OmeImage};
        let Some(series) = self.inner.series_list().first() else {
            return;
        };
        let size_c = series.metadata.size_c.max(1);
        let is_rgb = series.metadata.is_rgb;
        // Java getEffectiveSizeC(): C / samplesPerPixel for RGB.
        let effective_c = if is_rgb { 1 } else { size_c };

        let mut channels: Vec<OmeChannel> = Vec::with_capacity(effective_c as usize);
        for i in 0..effective_c as usize {
            let name = self.c_names.get(i).and_then(|n| n.clone());
            // Java color index: getIndex(0, i, 0) into channelColors.
            let color = self.channel_colors.get(i).and_then(|c| *c);
            channels.push(OmeChannel {
                name,
                color,
                samples_per_pixel: if is_rgb { size_c } else { 1 },
                ..Default::default()
            });
        }

        // FormatTools.getPhysicalSize returns null for non-positive values.
        let pos = |v: f64| if v > 0.0 { Some(v) } else { None };
        self.ome_images = vec![OmeImage {
            physical_size_x: pos(self.pixel_size_x),
            physical_size_y: pos(self.pixel_size_y),
            physical_size_z: pos(self.pixel_size_z),
            // pixelSizeT is microseconds; OME TimeIncrement is seconds.
            time_increment: Some(self.pixel_size_t as f64 / 1_000_000.0),
            channels,
            ..Default::default()
        }];
    }

    fn surface_parsed_metadata(&mut self) {
        use crate::common::metadata::MetadataValue;
        let mut vendor: Vec<(String, MetadataValue)> = Vec::new();
        if self.pixel_size_x > 0.0 {
            vendor.push((
                "improvision.XCalibrationMicrons".into(),
                MetadataValue::Float(self.pixel_size_x),
            ));
        }
        if self.pixel_size_y > 0.0 {
            vendor.push((
                "improvision.YCalibrationMicrons".into(),
                MetadataValue::Float(self.pixel_size_y),
            ));
        }
        if self.pixel_size_z > 0.0 {
            vendor.push((
                "improvision.ZCalibrationMicrons".into(),
                MetadataValue::Float(self.pixel_size_z),
            ));
        }
        vendor.push((
            "improvision.pixelSizeT".into(),
            MetadataValue::Int(self.pixel_size_t),
        ));
        for (i, name) in self.c_names.iter().enumerate() {
            if let Some(name) = name {
                vendor.push((
                    format!("improvision.ChannelName.{}", i),
                    MetadataValue::String(name.clone()),
                ));
            }
        }
        for (i, color) in self.channel_colors.iter().enumerate() {
            if let Some(color) = color {
                vendor.push((
                    format!("improvision.WhiteColour.{}", i),
                    MetadataValue::Int(*color as i64),
                ));
            }
        }

        if let Some(s) = self.inner.series_list_mut().first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }

    fn tiff_header_comment_contains_improvision(header: &[u8]) -> bool {
        let cursor = std::io::Cursor::new(header);
        let mut parser = match crate::tiff::parser::TiffParser::new(cursor) {
            Ok(parser) => parser,
            Err(_) => return false,
        };
        let ifd = match parser.read_ifd(parser.first_ifd_offset) {
            Ok((ifd, _)) => ifd,
            Err(_) => return false,
        };
        matches!(
            ifd.get_str(crate::tiff::ifd::tag::IMAGE_DESCRIPTION),
            Some(comment) if comment.contains(Self::IMPROVISION_MAGIC_STRING)
        )
    }

    fn sample_uuid_from_path(path: &Path) -> Result<Option<String>> {
        let mut reader = crate::tiff::TiffReader::new();
        reader.set_id(path)?;
        let comment = reader
            .series_list()
            .first()
            .and_then(|s| s.metadata.series_metadata.get("ImageDescription"))
            .and_then(|v| {
                if let crate::common::metadata::MetadataValue::String(s) = v {
                    Some(s.as_str())
                } else {
                    None
                }
            })
            .unwrap_or("");
        if comment.contains(Self::IMPROVISION_MAGIC_STRING) {
            Ok(Some(improvision_sample_uuid(comment)))
        } else {
            Ok(None)
        }
    }

    fn discover_files(&self, path: &Path, multiple_files: bool) -> Vec<std::path::PathBuf> {
        if !multiple_files {
            return vec![path.to_path_buf()];
        }
        let Ok(Some(current_uuid)) = Self::sample_uuid_from_path(path) else {
            return vec![path.to_path_buf()];
        };
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        let mut entries: Vec<String> = match std::fs::read_dir(parent) {
            Ok(read_dir) => read_dir
                .filter_map(|entry| entry.ok())
                .filter_map(|entry| entry.file_name().into_string().ok())
                .collect(),
            Err(_) => return vec![path.to_path_buf()],
        };
        entries.sort();

        let mut files = Vec::new();
        for name in entries {
            let candidate = parent.join(name);
            let Ok(Some(uuid)) = Self::sample_uuid_from_path(&candidate) else {
                continue;
            };
            if uuid == current_uuid {
                files.push(candidate);
            }
        }
        if files.is_empty() {
            vec![path.to_path_buf()]
        } else {
            files
        }
    }

    fn init_readers(&mut self) -> Result<()> {
        self.readers.clear();
        for file in &self.files {
            let mut reader = crate::tiff::TiffReader::new();
            reader.set_id(file)?;
            self.readers.push(reader);
        }
        Ok(())
    }

    fn apply_java_multifile_fallback(&mut self) {
        let ifd_count = self.inner.ifd_count() as u32;
        let files_len = self.files.len() as u32;
        let Some(series) = self.inner.series_list_mut().first_mut() else {
            return;
        };
        let m = &mut series.metadata;
        if files_len.saturating_mul(ifd_count) < m.image_count {
            self.files.truncate(1);
            m.image_count = ifd_count;
            m.size_z = ifd_count.max(1);
            m.size_t = 1;
            if !m.is_rgb {
                m.size_c = 1;
            }
        }
    }

    fn resolve_multifile_plane(&self, plane_index: u32) -> Result<(usize, u32)> {
        let meta = self.metadata();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let files_len = self.files.len();
        if files_len <= 1 {
            return Ok((0, plane_index));
        }
        let (z, c, t) = improvision_plane_to_zct(plane_index, meta)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let xyzct_index = t
            .checked_mul(meta.size_z)
            .and_then(|v| v.checked_mul(improvision_effective_size_c(meta)))
            .and_then(|v| v.checked_add(c.checked_mul(meta.size_z)?))
            .and_then(|v| v.checked_add(z))
            .ok_or_else(|| BioFormatsError::Format("Improvision plane index overflows".into()))?;
        Ok((
            (xyzct_index as usize) % files_len,
            plane_index / files_len as u32,
        ))
    }
}

fn improvision_sample_uuid(comment: &str) -> String {
    let normalized = comment.replace("\r\n", "\n").replace('\r', "\n");
    for line in normalized.split('\n') {
        let line = line.trim();
        if let Some(value) = line.strip_prefix("SampleUUID=") {
            return value.trim().to_string();
        }
    }
    String::new()
}

fn improvision_effective_size_c(meta: &ImageMetadata) -> u32 {
    if meta.is_rgb {
        (meta.size_c / 3).max(1)
    } else {
        meta.size_c.max(1)
    }
}

fn improvision_plane_to_zct(plane_index: u32, meta: &ImageMetadata) -> Option<(u32, u32, u32)> {
    for t in 0..meta.size_t.max(1) {
        for z in 0..meta.size_z.max(1) {
            for c in 0..improvision_effective_size_c(meta) {
                if improvision_zct_to_plane(z, c, t, meta)? == plane_index {
                    return Some((z, c, t));
                }
            }
        }
    }
    None
}

fn improvision_zct_to_plane(z: u32, c: u32, t: u32, meta: &ImageMetadata) -> Option<u32> {
    let size_z = meta.size_z.max(1);
    let size_c = improvision_effective_size_c(meta);
    let size_t = meta.size_t.max(1);
    if z >= size_z || c >= size_c || t >= size_t {
        return None;
    }
    Some(match meta.dimension_order {
        crate::common::metadata::DimensionOrder::XYZCT => t * size_z * size_c + c * size_z + z,
        crate::common::metadata::DimensionOrder::XYZTC => c * size_z * size_t + t * size_z + z,
        crate::common::metadata::DimensionOrder::XYCZT => t * size_c * size_z + z * size_c + c,
        crate::common::metadata::DimensionOrder::XYCTZ => z * size_c * size_t + t * size_c + c,
        crate::common::metadata::DimensionOrder::XYTCZ => z * size_t * size_c + c * size_t + t,
        crate::common::metadata::DimensionOrder::XYTZC => c * size_t * size_z + z * size_t + t,
    })
}

impl Default for ImprovisionTiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ImprovisionTiffReader {
    fn is_this_type_by_name(&self, _path: &Path) -> bool {
        false
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::tiff_header_comment_contains_improvision(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        self.inner.set_id(path)?;
        let comments = self.plane_comments();
        let multiple_files = self.parse_comments(&comments);
        self.files = self.discover_files(path, multiple_files);
        self.apply_java_multifile_fallback();
        self.init_readers()?;
        self.build_ome();
        self.surface_parsed_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        for reader in &mut self.readers {
            let _ = reader.close();
        }
        self.files.clear();
        self.readers.clear();
        self.last_file = 0;
        self.channel_colors.clear();
        self.c_names.clear();
        self.pixel_size_t = 1;
        self.pixel_size_x = 0.0;
        self.pixel_size_y = 0.0;
        self.pixel_size_z = 0.0;
        self.ome_images.clear();
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
        let (file, plane) = self.resolve_multifile_plane(p)?;
        self.last_file = file;
        if self.readers.is_empty() {
            self.inner.open_bytes(p)
        } else {
            self.readers[file].open_bytes(plane)
        }
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let (file, plane) = self.resolve_multifile_plane(p)?;
        self.last_file = file;
        if self.readers.is_empty() {
            self.inner.open_bytes_region(p, x, y, w, h)
        } else {
            self.readers[file].open_bytes_region(plane, x, y, w, h)
        }
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
    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.ome_images.is_empty() {
            return None;
        }
        Some(crate::common::ome_metadata::OmeMetadata {
            images: self.ome_images.clone(),
            ..Default::default()
        })
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

        Self::parse_zeiss_tiff_tags(&desc, &mut vendor);

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

    fn zeiss_key_name(id: i64) -> Option<&'static str> {
        match id {
            65598 => Some("ApotomeCamCalibrationMode"),
            65599 => Some("ApoTome Grid Position"),
            65600 => Some("ApotomeCamScannerPosition"),
            65601 => Some("ApoTome Full Phase Shift"),
            65602 => Some("ApoTome Grid Name"),
            65603 => Some("ApoTome Staining"),
            65604 => Some("ApoTome Processing Mode"),
            65605 => Some("ApotomeCamLiveCombineMode"),
            65606 => Some("ApoTome Filter Name"),
            65607 => Some("Apotome Filter Strength"),
            65608 => Some("ApotomeCamFilterHarmonics"),
            65609 => Some("ApoTome Grating Period"),
            65610 => Some("ApoTome Auto Shutter Used"),
            65611 => Some("Apotome Cam Status"),
            65612 => Some("ApotomeCamNormalize"),
            65613 => Some("ApotomeCamSettingsManager"),
            65628 => Some("ApotomeCamLiveFocus"),
            65631 => Some("ApotomeCamSliderInGridPosition"),
            65637 => Some("ApoTome Averaging Count"),
            2819 => Some("Image Index Z"),
            2820 => Some("Image Channel Index"),
            2821 => Some("Image Index T"),
            2822 => Some("ImageTile Index"),
            2832 => Some("Image Count Z"),
            2833 => Some("Image Count C"),
            2834 => Some("Image Count T"),
            _ => None,
        }
    }

    fn normalize_zeiss_key(key: &str) -> String {
        key.trim()
            .to_ascii_lowercase()
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
            .collect::<String>()
            .split('_')
            .filter(|part| !part.is_empty())
            .collect::<Vec<_>>()
            .join("_")
    }

    fn parse_zeiss_tiff_tags(
        xml: &str,
        vendor: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    ) {
        let tags = xml_scan_tags(xml);
        let mut keys: std::collections::HashMap<String, i64> = std::collections::HashMap::new();
        let mut values: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        for tag in &tags {
            let Some((prefix, index)) = tag.name.split_at_checked(1) else {
                continue;
            };
            if index.is_empty() || !index.chars().all(|c| c.is_ascii_digit()) {
                continue;
            }
            let Some(text) = xml_element_text(xml, tag) else {
                continue;
            };
            match prefix {
                "I" => {
                    if let Ok(id) = text.trim().parse::<i64>() {
                        keys.insert(index.to_string(), id);
                    }
                }
                "V" => {
                    values.insert(index.to_string(), text);
                }
                _ => {}
            }
        }

        for (index, id) in keys {
            let Some(name) = Self::zeiss_key_name(id) else {
                continue;
            };
            let Some(value) = values.get(&index) else {
                continue;
            };
            vendor.insert(
                format!("zeiss.{}", Self::normalize_zeiss_key(name)),
                metadata_value_from_text(value),
            );
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
        matches!(ext.as_deref(), Some("tif") | Some("tiff"))
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
pub struct FluoviewReader {
    inner: crate::tiff::TiffReader,
}

#[derive(Debug, Clone)]
struct FluoviewMmHeader {
    size_z: u32,
    size_c: u32,
    size_t: u32,
    series_count: u32,
    dimension_order: String,
}

impl FluoviewReader {
    const MMHEADER: u16 = 34361;
    const MMSTAMP: u16 = 34362;
    const TEMPERATURE: u16 = 4869;
    const EXPOSURE_TIME: u16 = 4876;
    const KINETIC_CYCLE_TIME: u16 = 4878;
    const N_ACCUMULATIONS: u16 = 4879;
    const ACQUISITION_CYCLE_TIME: u16 = 4881;
    const READOUT_TIME: u16 = 4882;
    const EM_DAC: u16 = 4885;
    const N_FRAMES: u16 = 4890;
    const HORIZONTAL_FLIP: u16 = 4896;
    const VERTICAL_FLIP: u16 = 4897;
    const CLOCKWISE: u16 = 4898;
    const COUNTER_CLOCKWISE: u16 = 4899;
    const VERTICAL_CLOCK_VOLTAGE: u16 = 4904;
    const VERTICAL_SHIFT_SPEED: u16 = 4905;
    const PRE_AMP_SETTING: u16 = 4907;
    const CAMERA_SERIAL_SETTING: u16 = 4908;
    const ACTUAL_TEMPERATURE: u16 = 4911;
    const BASELINE_CLAMP: u16 = 4912;
    const PRESCANS: u16 = 4913;
    const MODEL: u16 = 4914;
    const CHIP_SIZE_X: u16 = 4915;
    const CHIP_SIZE_Y: u16 = 4916;
    const BASELINE_OFFSET: u16 = 4944;

    pub fn new() -> Self {
        FluoviewReader {
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
        let first_ifd = self.inner.ifd(0).cloned();
        let has_mmheader = first_ifd
            .as_ref()
            .and_then(|ifd| ifd.get(Self::MMHEADER))
            .is_some();
        let has_mmstamp = first_ifd
            .as_ref()
            .and_then(|ifd| ifd.get(Self::MMSTAMP))
            .is_some();
        let is_andor = desc.starts_with("Andor");
        if !desc.contains("[Acquisition Parameters]")
            && !desc.contains("FluoView")
            && !desc.contains("FLUOVIEW")
            && !has_mmheader
            && !has_mmstamp
            && !is_andor
        {
            return;
        }

        let mut vendor = std::collections::HashMap::new();
        if has_mmheader {
            if let Some(ifd) = first_ifd.as_ref() {
                if let Some(mm) = Self::parse_mmheader(ifd, &mut vendor) {
                    Self::apply_mmheader(&mut self.inner, &mm);
                }
            }
        } else if let Some(ifd) = first_ifd.as_ref() {
            Self::parse_andor_tags(ifd, &mut vendor);
        }

        // Parse INI-style key=value pairs
        let mut date: Option<String> = None;
        let mut time: Option<String> = None;
        for line in desc.lines() {
            let line = line.trim();
            if let Some((key, val)) = line.split_once('=') {
                let key = key.trim();
                let val = val.trim();
                if !key.is_empty() && !key.starts_with('[') {
                    vendor.insert(format!("fluoview.{}", key), metadata_value_from_text(val));
                    let normalized = key.to_ascii_lowercase().replace(' ', "_");
                    vendor.insert(
                        format!("fluoview.{}", normalized),
                        metadata_value_from_text(val),
                    );
                    if key == "Date" {
                        date = Some(val.to_string());
                    } else if key == "Time" {
                        time = Some(val.to_string());
                    } else if key == "Magnification" {
                        vendor.insert(
                            "fluoview.objective_magnification".into(),
                            metadata_value_from_text(val),
                        );
                    } else if key == "System Configuration" {
                        vendor.insert(
                            "fluoview.detector_manufacturer".into(),
                            crate::common::metadata::MetadataValue::String(val.to_string()),
                        );
                    } else if key == "Objective Lens" {
                        vendor.insert(
                            "fluoview.objective_manufacturer".into(),
                            crate::common::metadata::MetadataValue::String(val.to_string()),
                        );
                    } else if key.starts_with("Channel ") && key.ends_with("Dye") {
                        vendor.insert(
                            format!(
                                "fluoview.channel_name.{}",
                                key.trim_start_matches("Channel ")
                                    .trim_end_matches("Dye")
                                    .trim()
                            ),
                            crate::common::metadata::MetadataValue::String(val.to_string()),
                        );
                    } else if let Some(index) = key.strip_prefix("Gain Ch") {
                        vendor.insert(
                            format!("fluoview.detector_gain.{}", index.trim()),
                            metadata_value_from_text(val),
                        );
                    } else if let Some(index) = key.strip_prefix("PMT Voltage Ch") {
                        vendor.insert(
                            format!("fluoview.detector_voltage.{}", index.trim()),
                            metadata_value_from_text(val),
                        );
                    } else if let Some(index) = key.strip_prefix("Offset Ch") {
                        vendor.insert(
                            format!("fluoview.detector_offset.{}", index.trim()),
                            metadata_value_from_text(val),
                        );
                    } else if let Some(index) = key.strip_prefix("Confocal Aperture-Ch") {
                        let trimmed = val.trim_end_matches("um").trim();
                        vendor.insert(
                            format!("fluoview.lens_na.{}", index.trim()),
                            metadata_value_from_text(trimmed),
                        );
                    }
                }
            } else if line.starts_with('Z') && line.contains(" um ") && line.contains('-') {
                let z = line[line.find('-').unwrap_or(0) + 1..]
                    .chars()
                    .map(|c| if c.is_ascii_alphabetic() { ' ' } else { c })
                    .collect::<String>();
                let parts: Vec<&str> = z.split_whitespace().collect();
                if parts.len() >= 2 {
                    if let (Ok(size), Ok(n_planes)) =
                        (parts[0].parse::<f64>(), parts[1].parse::<f64>())
                    {
                        if n_planes != 0.0 {
                            vendor.insert(
                                "fluoview.voxel_z".into(),
                                crate::common::metadata::MetadataValue::Float(size / n_planes),
                            );
                        }
                    }
                }
            }
        }
        if let (Some(date), Some(time)) = (date, time) {
            vendor.insert(
                "fluoview.acquisition_date".into(),
                crate::common::metadata::MetadataValue::String(format!("{} {}", date, time)),
            );
        }

        let series = self.inner.series_list_mut();
        if let Some(s) = series.first_mut() {
            for (k, v) in vendor {
                s.metadata.series_metadata.insert(k, v);
            }
        }
    }

    fn ifd_value_to_metadata(
        value: &crate::tiff::ifd::IfdValue,
    ) -> crate::common::metadata::MetadataValue {
        match value {
            crate::tiff::ifd::IfdValue::Ascii(s) => {
                crate::common::metadata::MetadataValue::String(s.clone())
            }
            crate::tiff::ifd::IfdValue::Float(v) if !v.is_empty() => {
                crate::common::metadata::MetadataValue::Float(v[0] as f64)
            }
            crate::tiff::ifd::IfdValue::Double(v) if !v.is_empty() => {
                crate::common::metadata::MetadataValue::Float(v[0])
            }
            _ => {
                let values = value.as_vec_f64();
                if values.len() == 1 {
                    crate::common::metadata::MetadataValue::Float(values[0])
                } else if !values.is_empty() {
                    crate::common::metadata::MetadataValue::String(
                        values
                            .iter()
                            .map(|v| v.to_string())
                            .collect::<Vec<_>>()
                            .join(","),
                    )
                } else {
                    crate::common::metadata::MetadataValue::String(format!("{:?}", value))
                }
            }
        }
    }

    fn parse_andor_tags(
        ifd: &crate::tiff::ifd::Ifd,
        vendor: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    ) {
        for (tag, name) in [
            (Self::TEMPERATURE, "Temperature"),
            (Self::EXPOSURE_TIME, "Exposure time (in seconds)"),
            (Self::KINETIC_CYCLE_TIME, "Kinetic cycle time"),
            (Self::N_ACCUMULATIONS, "Number of accumulations"),
            (Self::ACQUISITION_CYCLE_TIME, "Acquisition cycle time"),
            (Self::READOUT_TIME, "Readout time"),
            (Self::EM_DAC, "EM DAC"),
            (Self::N_FRAMES, "Number of frames"),
            (Self::HORIZONTAL_FLIP, "Horizontal flip"),
            (Self::VERTICAL_FLIP, "Vertical flip"),
            (Self::CLOCKWISE, "Clockwise rotation"),
            (Self::COUNTER_CLOCKWISE, "Counter-clockwise rotation"),
            (Self::VERTICAL_CLOCK_VOLTAGE, "Vertical clock voltage"),
            (Self::VERTICAL_SHIFT_SPEED, "Vertical shift speed"),
            (Self::PRE_AMP_SETTING, "Pre-amp"),
            (Self::CAMERA_SERIAL_SETTING, "Camera serial setting"),
            (Self::ACTUAL_TEMPERATURE, "Actual temperature"),
            (Self::BASELINE_CLAMP, "Baseline clamp"),
            (Self::PRESCANS, "Prescans"),
            (Self::MODEL, "Camera model"),
            (Self::CHIP_SIZE_X, "Chip size X"),
            (Self::CHIP_SIZE_Y, "Chip size Y"),
            (Self::BASELINE_OFFSET, "Baseline offset"),
        ] {
            if let Some(value) = ifd.get(tag) {
                let key = name.to_ascii_lowercase().replace([' ', '(', ')'], "_");
                vendor.insert(
                    format!("fluoview.andor.{}", key),
                    Self::ifd_value_to_metadata(value),
                );
            }
        }
    }

    fn parse_mmheader(
        ifd: &crate::tiff::ifd::Ifd,
        vendor: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    ) -> Option<FluoviewMmHeader> {
        let Some(value) = ifd.get(Self::MMHEADER) else {
            return None;
        };
        let bytes: Vec<u8> = match value {
            crate::tiff::ifd::IfdValue::Short(v) => v.iter().map(|v| *v as u8).collect(),
            crate::tiff::ifd::IfdValue::Byte(v) | crate::tiff::ifd::IfdValue::Undefined(v) => {
                v.clone()
            }
            _ => return None,
        };
        if bytes.len() < 284 + 10 * 96 {
            return None;
        }
        let little = true;
        let read_i32 = |offset: usize| -> i32 {
            let mut b = [0u8; 4];
            b.copy_from_slice(&bytes[offset..offset + 4]);
            if little {
                i32::from_le_bytes(b)
            } else {
                i32::from_be_bytes(b)
            }
        };
        let read_f64 = |offset: usize| -> f64 {
            let mut b = [0u8; 8];
            b.copy_from_slice(&bytes[offset..offset + 8]);
            if little {
                f64::from_le_bytes(b)
            } else {
                f64::from_be_bytes(b)
            }
        };
        let mut order = String::from("XY");
        let mut size_z = 1u32;
        let mut size_c = 1u32;
        let mut size_t = 1u32;
        let mut series_count = 1u32;
        let mut voxel_x = None;
        let mut voxel_y = None;
        let mut voxel_z = None;
        let mut voxel_t = None;
        for i in 0..10usize {
            let offset = 284 + i * 96;
            let raw_name = &bytes[offset..offset + 16];
            let nul = raw_name
                .iter()
                .position(|b| *b == 0)
                .unwrap_or(raw_name.len());
            let name = String::from_utf8_lossy(&raw_name[..nul])
                .trim()
                .to_ascii_lowercase();
            let size = read_i32(offset + 16).max(0) as u32;
            let resolution = read_f64(offset + 28);
            if name.is_empty() || size == 0 {
                continue;
            }
            vendor.insert(
                format!("fluoview.dimension.{}.name", i + 1),
                crate::common::metadata::MetadataValue::String(name.clone()),
            );
            vendor.insert(
                format!("fluoview.dimension.{}.size", i + 1),
                crate::common::metadata::MetadataValue::Int(size as i64),
            );
            vendor.insert(
                format!("fluoview.dimension.{}.resolution", i + 1),
                crate::common::metadata::MetadataValue::Float(resolution),
            );
            match name.as_str() {
                "x" => voxel_x = Some(resolution),
                "y" => voxel_y = Some(resolution),
                "event" | "z" => {
                    size_z = size_z.saturating_mul(size.max(1));
                    if !order.contains('Z') {
                        order.push('Z');
                    }
                    voxel_z = Some(resolution);
                }
                "ch" | "wavelength" => {
                    size_c = size_c.saturating_mul(size.max(1));
                    if !order.contains('C') {
                        order.push('C');
                    }
                }
                "time" | "t" | "animation" => {
                    size_t = size_t.saturating_mul(size.max(1));
                    if !order.contains('T') {
                        order.push('T');
                    }
                    voxel_t = Some(resolution);
                }
                _ => {
                    if !order.contains('S') {
                        order.push('S');
                    }
                    series_count = series_count.saturating_mul(size.max(1));
                }
            }
        }
        for axis in ['Z', 'T', 'C'] {
            if !order.contains(axis) {
                order.push(axis);
            }
        }
        let order_no_s = order.replace('S', "");
        if let Some(v) = voxel_x {
            vendor.insert(
                "fluoview.voxel_x".into(),
                crate::common::metadata::MetadataValue::Float(v),
            );
        }
        if let Some(v) = voxel_y {
            vendor.insert(
                "fluoview.voxel_y".into(),
                crate::common::metadata::MetadataValue::Float(v),
            );
        }
        if let Some(v) = voxel_z {
            vendor.insert(
                "fluoview.voxel_z".into(),
                crate::common::metadata::MetadataValue::Float(v),
            );
        }
        if let Some(v) = voxel_t {
            vendor.insert(
                "fluoview.voxel_t".into(),
                crate::common::metadata::MetadataValue::Float(v),
            );
        }
        Some(FluoviewMmHeader {
            size_z: size_z.max(1),
            size_c: size_c.max(1),
            size_t: size_t.max(1),
            series_count: series_count.max(1),
            dimension_order: order_no_s,
        })
    }

    fn apply_mmheader(inner: &mut crate::tiff::TiffReader, mm: &FluoviewMmHeader) {
        let Some(series) = inner.series_list_mut().first_mut() else {
            return;
        };
        let meta = &mut series.metadata;
        meta.size_z = mm.size_z;
        meta.size_c = mm.size_c;
        meta.size_t = mm.size_t;
        meta.image_count = (meta.image_count / mm.series_count).max(1);
        meta.dimension_order = match mm.dimension_order.as_str() {
            "XYZCT" => crate::common::metadata::DimensionOrder::XYZCT,
            "XYZTC" => crate::common::metadata::DimensionOrder::XYZTC,
            "XYCZT" => crate::common::metadata::DimensionOrder::XYCZT,
            "XYCTZ" => crate::common::metadata::DimensionOrder::XYCTZ,
            "XYTCZ" => crate::common::metadata::DimensionOrder::XYTCZ,
            "XYTZC" => crate::common::metadata::DimensionOrder::XYTZC,
            _ => meta.dimension_order,
        };
    }
}

impl Default for FluoviewReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for FluoviewReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif") | Some("tiff"))
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
    /// Cross-file `.htd` plate state (Java `MetaxpressTiffReader extends
    /// CellWorxReader`). Empty when a plain single `.tif` was opened directly.
    plate: Option<MetaxpressPlate>,
    /// One [`ImageMetadata`] per series (`field_count * well_count`) when a
    /// `.htd` plate was opened; empty otherwise.
    plate_series: Vec<ImageMetadata>,
    /// Currently selected plate series.
    current_series: usize,
    /// Whether `inner` currently holds a loaded companion TIFF (plate mode).
    plate_tiff_loaded: bool,
    /// OME metadata from the first real companion TIFF, used as the Java
    /// MetaXpress plate template for physical sizes and detector metadata.
    plate_ome_template: Option<crate::common::ome_metadata::OmeMetadata>,
}

/// Parsed `.htd` plate grid plus the assembled per-well/field/wavelength TIFF
/// file lists. Faithful subset of `CellWorxReader`'s plate fields needed by the
/// MetaXpress series machinery.
struct MetaxpressPlate {
    /// `well_files[row][col]` = `Some(file list)` for selected wells; ordered
    /// field, channel, timepoint per `MetaxpressTiffReader.getTiffFiles`.
    well_files: Vec<Vec<Option<Vec<std::path::PathBuf>>>>,
    /// Selected wells in row-major order, parallel to series/well indexing.
    selected_wells: Vec<(usize, usize)>,
    field_count: usize,
    channels: usize,
    wavelengths: Vec<Option<String>>,
    z_steps: u32,
    /// Java changes `getFile` indexing after falling back to the
    /// `TimePoint_<t>/ZStep_<z>` directory layout.
    subdirectories: bool,
}

impl MolecularDevicesTiffReader {
    pub fn new() -> Self {
        MolecularDevicesTiffReader {
            inner: crate::tiff::TiffReader::new(),
            plate: None,
            plate_series: Vec::new(),
            current_series: 0,
            plate_tiff_loaded: false,
            plate_ome_template: None,
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
            let tags = xml_scan_tags(&desc);
            moldev_insert_shallow_xml_metadata(&mut vendor, &desc, &tags);
            moldev_insert_hierarchy_scalar_metadata(&mut vendor, &desc, &tags);

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

        insert_simplepci_image_description_metadata(&mut vendor, &desc);

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

    /// Open a MetaXpress `.htd` plate (or a `.tif` whose companion `.htd`
    /// exists), building the cross-file multi-series core. Faithful translation
    /// of `CellWorxReader.initFile` + `MetaxpressTiffReader.findPixelsFiles`/
    /// `getTiffFiles` for the MetaXpress (TIFF) subclass.
    fn set_id_plate(&mut self, htd: &std::path::Path) -> Result<()> {
        let info = metaxpress_parse_htd(htd)?;

        // Field (site) count = number of selected sites in the field map.
        let field_count = info
            .field_map
            .iter()
            .flatten()
            .filter(|&&b| b)
            .count()
            .max(1);

        // Enumerate selected wells row-major and assemble their TIFF file lists.
        let plate = metaxpress_plate_base(htd);
        let channels = info.wavelengths.len();
        let mut well_files: Vec<Vec<Option<Vec<std::path::PathBuf>>>> =
            vec![vec![None; info.x_wells]; info.y_wells];
        let mut selected_wells: Vec<(usize, usize)> = Vec::new();
        let mut subdirectories = false;
        for row in 0..info.y_wells {
            for col in 0..info.x_wells {
                if info
                    .well_selected
                    .get(row)
                    .and_then(|r| r.get(col))
                    .copied()
                    .unwrap_or(false)
                {
                    let resolved = metaxpress_get_tiff_files(
                        htd,
                        &plate,
                        row,
                        col,
                        field_count,
                        channels,
                        info.n_timepoints,
                        info.z_steps,
                        info.do_channels,
                    );
                    subdirectories |= resolved.subdirectories;
                    well_files[row][col] = Some(resolved.files);
                    selected_wells.push((row, col));
                }
            }
        }

        let well_count = selected_wells.len();
        let series_count = field_count * well_count;
        if series_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "MetaXpress HTD declares no selected wells".into(),
            ));
        }

        let plate = MetaxpressPlate {
            well_files,
            selected_wells,
            field_count,
            channels,
            wavelengths: info.wavelengths.clone(),
            z_steps: info.z_steps,
            subdirectories,
        };

        // Find the first companion TIFF that actually exists on disk, mirroring
        // CellWorxReader.populateMetadata's probe loop.
        let planes_per = (info.z_steps as usize) * (info.n_timepoints as usize) * channels;
        let mut series_idx = 0usize;
        let mut plane_idx = 0u32;
        let mut probe: Option<std::path::PathBuf> = None;
        loop {
            if let Some(f) = plate.get_file(series_idx, plane_idx) {
                if f.exists() {
                    probe = Some(f);
                    break;
                }
            }
            if (plane_idx as usize) < planes_per {
                plane_idx += 1;
            } else if series_idx < series_count - 1 {
                plane_idx = 0;
                series_idx += 1;
            } else {
                break;
            }
        }
        let probe = probe.ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(
                "MetaXpress: no companion pixel files found on disk".into(),
            )
        })?;

        self.inner.set_id(&probe)?;
        let plate_ome_template = self.inner.ome_metadata();
        let tm = self.inner.metadata();
        let size_x = tm.size_x;
        let size_y = tm.size_y;
        let pixel_type = tm.pixel_type;
        let bits = tm.bits_per_pixel;
        let little_endian = tm.is_little_endian;
        let _ = self.inner.close();

        // Build one CoreMetadata per series (field x well), mirroring the
        // CellWorxReader.populateMetadata core-construction loop: sizeZ=zSteps,
        // sizeT=nTimepoints, sizeC=wavelengths, dimension order XYCZT.
        let image_count = info.z_steps * channels as u32 * info.n_timepoints;
        let mut plate_series = Vec::with_capacity(series_count);
        for s in 0..series_count {
            let (row, col) = plate.selected_wells[s / field_count];
            let mut md = std::collections::HashMap::new();
            md.insert(
                "format".into(),
                crate::common::metadata::MetadataValue::String("MetaXpress".into()),
            );
            md.insert(
                "Well".into(),
                crate::common::metadata::MetadataValue::String(metaxpress_well_name(row, col)),
            );
            for (i, w) in info.wavelengths.iter().enumerate() {
                if let Some(name) = w {
                    md.insert(
                        format!("Wavelength {}", i + 1),
                        crate::common::metadata::MetadataValue::String(name.clone()),
                    );
                }
            }
            plate_series.push(ImageMetadata {
                size_x,
                size_y,
                size_z: info.z_steps,
                size_c: channels as u32,
                size_t: info.n_timepoints,
                pixel_type,
                bits_per_pixel: bits,
                image_count,
                dimension_order: crate::common::metadata::DimensionOrder::XYCZT,
                is_rgb: false,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: little_endian,
                resolution_count: 1,
                thumbnail: false,
                series_metadata: md,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
        }

        self.plate = Some(plate);
        self.plate_series = plate_series;
        self.current_series = 0;
        self.plate_tiff_loaded = false;
        self.plate_ome_template = plate_ome_template;
        Ok(())
    }

    /// Read one plane of a plate series, delegating pixels to the inner
    /// `TiffReader` on the resolved companion file. Mirrors
    /// `CellWorxReader.openBytes` (with MetaXpress's MetamorphReader delegate).
    fn open_plate_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let (plane_bytes, size_z) = {
            let meta = self
                .plate_series
                .get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized)?;
            if plane_index >= meta.image_count {
                return Err(BioFormatsError::PlaneOutOfRange(plane_index));
            }
            let bps = meta.pixel_type.bytes_per_sample();
            (
                meta.size_x as usize * meta.size_y as usize * bps,
                meta.size_z,
            )
        };

        let plate = self.plate.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let field_count = plate.field_count;
        // Resolve the backing file; a missing companion reads back as zeros.
        let file = match plate.get_file(self.current_series, plane_index) {
            Some(f) if f.exists() => f,
            _ => return Ok(vec![0u8; plane_bytes]),
        };

        if self.plate_tiff_loaded {
            let _ = self.inner.close();
            self.plate_tiff_loaded = false;
        }
        if self.inner.set_id(&file).is_err() {
            return Ok(vec![0u8; plane_bytes]);
        }
        self.plate_tiff_loaded = true;

        let tiff_series = self.inner.series_count();
        let tiff_imgs = self.inner.metadata().image_count;
        let plane = if tiff_series == field_count && field_count > 1 {
            let field = self.current_series % field_count;
            let _ = self.inner.set_series(field);
            plane_index
        } else if tiff_imgs == size_z {
            let meta = &self.plate_series[self.current_series];
            metaxpress_z_coord(meta, plane_index)
        } else {
            0
        };
        self.inner.open_bytes(plane)
    }
}

/// Parsed contents of a MetaXpress / CellWorX `.HTD` plate-index file.
/// Faithful subset of `CellWorxReader.parseHTD`'s state.
struct MetaxpressHtdInfo {
    x_wells: usize,
    y_wells: usize,
    /// `well_selected[row][col]`
    well_selected: Vec<Vec<bool>>,
    /// field acquisition map (sites grid)
    field_map: Vec<Vec<bool>>,
    n_timepoints: u32,
    z_steps: u32,
    do_channels: bool,
    /// One entry per wavelength; `Some(name)` if a `WaveName<i>` was present.
    wavelengths: Vec<Option<String>>,
}

/// `Boolean.parseBoolean` semantics: true only when the token is "true".
fn metaxpress_htd_bool(value: &str) -> bool {
    value.trim().eq_ignore_ascii_case("true")
}

/// Parse a MetaXpress `.HTD` file. Lines are `"key", value[, value...]`; the key
/// is delimited from the value by the literal `",` sequence (matching Java's
/// `CellWorxReader.parseHTD` / `line.indexOf("\",")`).
fn metaxpress_parse_htd(path: &std::path::Path) -> Result<MetaxpressHtdInfo> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;

    let mut x_wells = 0usize;
    let mut y_wells = 0usize;
    let mut well_selected: Vec<Vec<bool>> = Vec::new();
    let mut x_fields = 0usize;
    let mut y_fields = 0usize;
    let mut field_map: Option<Vec<Vec<bool>>> = None;
    let mut n_timepoints = 1u32;
    let mut z_steps = 1u32;
    let mut do_channels = false;
    let mut wavelengths: Vec<Option<String>> = Vec::new();

    for line in content.split('\n') {
        let split = match line.find("\",") {
            Some(s) if s >= 1 => s,
            _ => continue,
        };
        let key = line[1..split].trim();
        let value = line[split + 2..].trim();

        if key == "XWells" {
            x_wells = value.parse().unwrap_or(0);
        } else if key == "YWells" {
            y_wells = value.parse().unwrap_or(0);
            well_selected = vec![vec![false; x_wells]; y_wells];
        } else if let Some(rest) = key.strip_prefix("WellsSelection") {
            if let Ok(row1) = rest.trim().parse::<usize>() {
                if row1 >= 1 && row1 <= well_selected.len() {
                    let row = row1 - 1;
                    let mapping: Vec<&str> = value.split(',').collect();
                    for (col, slot) in well_selected[row].iter_mut().enumerate() {
                        if let Some(tok) = mapping.get(col) {
                            if metaxpress_htd_bool(tok) {
                                *slot = true;
                            }
                        }
                    }
                }
            }
        } else if key == "XSites" {
            x_fields = value.parse().unwrap_or(0);
        } else if key == "YSites" {
            y_fields = value.parse().unwrap_or(0);
            // If field acquisition was off ("Sites" == FALSE), the single-site
            // map is already set; don't overwrite it.
            if field_map.is_none() {
                field_map = Some(vec![vec![false; x_fields]; y_fields]);
            }
        } else if key == "Sites" {
            if value.eq_ignore_ascii_case("false") {
                field_map = Some(vec![vec![true]]);
            }
        } else if key == "TimePoints" {
            n_timepoints = value.parse().unwrap_or(1).max(1);
        } else if key == "ZSteps" {
            z_steps = value.parse().unwrap_or(1).max(1);
        } else if let Some(rest) = key.strip_prefix("SiteSelection") {
            if let (Ok(row1), Some(fm)) = (rest.trim().parse::<usize>(), field_map.as_mut()) {
                if row1 >= 1 && row1 <= fm.len() {
                    let row = row1 - 1;
                    let mapping: Vec<&str> = value.split(',').collect();
                    for (col, slot) in fm[row].iter_mut().enumerate() {
                        if let Some(tok) = mapping.get(col) {
                            *slot = metaxpress_htd_bool(tok);
                        }
                    }
                }
            }
        } else if key == "Waves" {
            do_channels = metaxpress_htd_bool(value);
        } else if key == "NWavelengths" {
            let n = value.parse().unwrap_or(0);
            wavelengths = vec![None; n];
        } else if let Some(rest) = key.strip_prefix("WaveName") {
            if let Ok(idx1) = rest.trim().parse::<usize>() {
                if idx1 >= 1 && idx1 <= wavelengths.len() {
                    wavelengths[idx1 - 1] = Some(value.replace('"', ""));
                }
            }
        }
    }

    let mut field_map = field_map.unwrap_or_else(|| vec![vec![true]]);
    // If the acquisition only contains one site, SiteSelection1 may be absent;
    // assume the field was selected.
    if x_fields == 1 && y_fields == 1 && !field_map.is_empty() && !field_map[0].is_empty() {
        field_map[0][0] = true;
    }
    if wavelengths.is_empty() {
        wavelengths.push(None);
    }

    Ok(MetaxpressHtdInfo {
        x_wells,
        y_wells,
        well_selected,
        field_map,
        n_timepoints,
        z_steps,
        do_channels,
        wavelengths,
    })
}

/// Locate the `.HTD` plate-index file given any member of the dataset, mirroring
/// the top of `CellWorxReader.initFile`.
fn metaxpress_find_htd(path: &std::path::Path) -> Result<std::path::PathBuf> {
    let is_htd = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("htd"))
        .unwrap_or(false);
    if is_htd {
        if path.exists() {
            return Ok(path.to_path_buf());
        }
        return Err(BioFormatsError::UnsupportedFormat(
            "MetaXpress HTD file does not exist".into(),
        ));
    }
    // Derive from a pixel file: strip everything after the last '_'.
    let s = path.to_string_lossy();
    if let Some(us) = s.rfind('_') {
        for ext in ["HTD", "htd"] {
            let cand = std::path::PathBuf::from(format!("{}.{}", &s[..us], ext));
            if cand.exists() {
                return Ok(cand);
            }
        }
    }
    // Fall back to scanning the parent directory for any .htd file.
    if let Some(parent) = path.parent() {
        if let Ok(entries) = std::fs::read_dir(parent) {
            let mut paths: Vec<std::path::PathBuf> = entries.flatten().map(|e| e.path()).collect();
            paths.sort();
            for p in paths {
                if p.extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.eq_ignore_ascii_case("htd"))
                    .unwrap_or(false)
                {
                    return Ok(p);
                }
            }
        }
    }
    Err(BioFormatsError::UnsupportedFormat(
        "MetaXpress: could not locate companion .htd file".into(),
    ))
}

/// Build the plate-name prefix: HTD path with its extension stripped, plus `_`.
/// Mirror of `CellWorxReader.getPlateName`.
fn metaxpress_plate_base(htd: &std::path::Path) -> String {
    let absolute = std::fs::canonicalize(htd).unwrap_or_else(|_| htd.to_path_buf());
    let s = absolute.to_string_lossy();
    let cut = s.rfind('.').unwrap_or(s.len());
    format!("{}_", &s[..cut])
}

/// Well label as used in MetaXpress TIFF names, e.g. row 0 col 0 -> "A01"
/// (`FormatTools.getWellName`).
fn metaxpress_well_name(row: usize, col: usize) -> String {
    let letter = (b'A' + (row as u8 % 26)) as char;
    format!("{}{:02}", letter, col + 1)
}

struct MetaxpressTiffFiles {
    files: Vec<std::path::PathBuf>,
    subdirectories: bool,
}

/// Build the per-well TIFF file list, following
/// `MetaxpressTiffReader.getTiffFiles`. Direct names are ordered field,
/// channel, timepoint. If none exist, mirror Java's sorted flat-directory
/// fallback, then its `TimePoint_<t>/ZStep_<z>` fallback.
fn metaxpress_get_tiff_files(
    htd: &std::path::Path,
    plate: &str,
    row: usize,
    col: usize,
    field_count: usize,
    channels: usize,
    n_timepoints: u32,
    z_steps: u32,
    do_channels: bool,
) -> MetaxpressTiffFiles {
    let base = format!("{}{}", plate, metaxpress_well_name(row, col));
    let mut files: Vec<std::path::PathBuf> =
        Vec::with_capacity(field_count * channels * n_timepoints as usize);
    for field in 0..field_count {
        for channel in 0..channels {
            for _t in 0..n_timepoints {
                let mut name = base.clone();
                if field_count > 1 {
                    name.push_str(&format!("_s{}", field + 1));
                }
                if do_channels || channels > 1 {
                    name.push_str(&format!("_w{}", channel + 1));
                }
                if n_timepoints > 1 {
                    // Matches the upstream quirk: the timepoint *count* is used.
                    name.push_str(&format!("_t{}", n_timepoints));
                }
                let lower = std::path::PathBuf::from(format!("{}.tif", name));
                if lower.exists() {
                    files.push(lower);
                } else {
                    files.push(std::path::PathBuf::from(format!("{}.TIF", name)));
                }
            }
        }
    }
    if files.iter().any(|f| f.exists()) {
        return MetaxpressTiffFiles {
            files,
            subdirectories: false,
        };
    }

    let expected_len = field_count
        .saturating_mul(channels)
        .saturating_mul(n_timepoints as usize)
        .saturating_mul(z_steps as usize);
    let parent = htd.parent().unwrap_or_else(|| std::path::Path::new(""));

    let mut flat = Vec::with_capacity(expected_len);
    for name in metaxpress_list_dir_sorted(parent) {
        let path = parent.join(&name);
        let lower = path.to_string_lossy().to_ascii_lowercase();
        if metaxpress_is_tiff_path(&path)
            && !lower.contains("_thumb")
            && path.to_string_lossy().starts_with(&base)
        {
            flat.push(path);
        }
    }
    if !flat.is_empty() {
        return MetaxpressTiffFiles {
            files: flat,
            subdirectories: false,
        };
    }

    let file_base = std::path::Path::new(&base)
        .file_name()
        .map(|f| f.to_string_lossy().into_owned())
        .unwrap_or(base);
    let mut nested = Vec::with_capacity(expected_len);
    for t in 0..n_timepoints {
        let dir = parent.join(format!("TimePoint_{}", t + 1));
        if !(dir.exists() && dir.is_dir()) {
            continue;
        }
        for z in 0..z_steps {
            let zdir = dir.join(format!("ZStep_{}", z + 1));
            let scan_dir = if zdir.exists() && zdir.is_dir() {
                zdir
            } else if z_steps == 1 {
                dir.clone()
            } else {
                continue;
            };
            for name in metaxpress_list_dir_sorted(&scan_dir) {
                let name_str = name.to_string_lossy();
                let path = scan_dir.join(&name);
                let lower = path.to_string_lossy().to_ascii_lowercase();
                if name_str.starts_with(&file_base)
                    && metaxpress_is_tiff_path(&path)
                    && !lower.contains("_thumb")
                {
                    nested.push(path);
                }
            }
        }
    }

    MetaxpressTiffFiles {
        files: nested,
        subdirectories: true,
    }
}

fn metaxpress_is_tiff_path(path: &std::path::Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
        .unwrap_or(false)
}

fn metaxpress_list_dir_sorted(dir: &std::path::Path) -> Vec<std::ffi::OsString> {
    let mut names: Vec<std::ffi::OsString> = match std::fs::read_dir(dir) {
        Ok(rd) => rd.flatten().map(|e| e.file_name()).collect(),
        Err(_) => Vec::new(),
    };
    names.sort();
    names
}

/// Z coordinate of a plane index under an `XYCZT` dimension order.
fn metaxpress_z_coord(meta: &ImageMetadata, no: u32) -> u32 {
    let sc = meta.size_c.max(1);
    let sz = meta.size_z.max(1);
    (no / sc) % sz
}

impl MetaxpressPlate {
    /// Resolve the companion TIFF for a (series, plane) pair, mirroring
    /// `CellWorxReader.getFile`.
    fn get_file(&self, series: usize, no: u32) -> Option<std::path::PathBuf> {
        if self.field_count == 0 {
            return None;
        }
        let well_index = series / self.field_count;
        let field = series % self.field_count;
        let &(row, col) = self.selected_wells.get(well_index)?;
        let files = self.well_files.get(row)?.get(col)?.as_ref()?;
        if files.is_empty() {
            return None;
        }
        let image_count = files.len() / self.field_count.max(1);
        let idx = field * image_count + no as usize;
        if self.subdirectories && self.channels > 0 && self.z_steps > 0 {
            let no = no as usize;
            let c = no % self.channels;
            let z = (no / self.channels) % self.z_steps as usize;
            let t = no / (self.channels * self.z_steps as usize);
            let plane = c
                + self.channels * field
                + self.channels * self.field_count * z
                + self.channels * self.field_count * self.z_steps as usize * t;
            return files.get(plane).cloned();
        }
        if idx < files.len() {
            files.get(idx).cloned()
        } else if field < files.len() {
            files.get(field).cloned()
        } else if image_count == 0 && files.len() == 1 {
            files.first().cloned()
        } else {
            None
        }
    }
}

fn insert_simplepci_image_description_metadata(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    desc: &str,
) {
    let lower = desc.to_ascii_lowercase();
    if !lower.contains("simplepci") && !lower.contains("simple pci") && !lower.contains("hcimage") {
        return;
    }

    let software = match (
        lower.contains("simplepci") || lower.contains("simple pci"),
        lower.contains("hcimage"),
    ) {
        (true, true) => "SimplePCI HCImage",
        (true, false) => "SimplePCI",
        (false, true) => "HCImage",
        (false, false) => return,
    };
    metadata.insert(
        "simplepci.software".into(),
        crate::common::metadata::MetadataValue::String(software.into()),
    );

    insert_simplepci_ini_typed_metadata(metadata, desc);
    insert_simplepci_xml_metadata(metadata, desc);

    let mut section: Option<String> = None;
    let mut scalar_count = 0usize;
    for line in desc.lines().take(512) {
        if scalar_count >= 256 {
            break;
        }
        let line = line.trim();
        if line.is_empty() || line.starts_with('<') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            let key = simplepci_tiff_metadata_key(&line[1..line.len() - 1]);
            section = (!key.is_empty()).then_some(key);
            continue;
        }

        let Some((key, value)) = line.split_once('=').or_else(|| line.split_once(':')) else {
            continue;
        };
        let key = simplepci_tiff_metadata_key(key);
        let value = value.trim().trim_matches('"');
        if key.is_empty() || value.is_empty() {
            continue;
        }

        let flat_key = format!("simplepci.{key}");
        if !metadata.contains_key(&flat_key) {
            insert_parsed_metadata_value(metadata, flat_key, value);
            scalar_count += 1;
        }

        if let Some(section) = section.as_deref() {
            if scalar_count >= 256 {
                break;
            }
            let scoped_key = format!("simplepci.{section}.{key}");
            if !metadata.contains_key(&scoped_key) {
                insert_parsed_metadata_value(metadata, scoped_key, value);
                scalar_count += 1;
            }
        }
    }

    metadata.insert(
        "simplepci.scalar_count".into(),
        crate::common::metadata::MetadataValue::Int(scalar_count as i64),
    );
}

/// Port of `SimplePCITiffReader.initStandardMetadata()` (formats-gpl).
///
/// SimplePCI stores its acquisition metadata in the TIFF comment as an INI
/// document (the Java reader feeds it to `IniParser` with `;` comments). The
/// upstream reader extracts a fixed set of typed scalars from named sections
/// (` MICROSCOPE `, ` CAPTURE DEVICE `, ` CAPTURE `, ` CALIBRATION `) and also
/// flattens the whole INI into the metadata table. This mirrors that typed
/// extraction so callers see the same magnification/immersion/camera/binning/
/// exposure/calibration values that the Java store records, rather than only
/// the heuristic key=value scalars.
fn insert_simplepci_ini_typed_metadata(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    desc: &str,
) {
    // Java: drop the magic line, then the date line, then strip "ReadFromDoc".
    let mut lines = desc.lines();
    let _magic = lines.next();
    let date = lines.next().map(str::trim).unwrap_or("");
    if !date.is_empty() {
        metadata.insert(
            "simplepci.date".into(),
            crate::common::metadata::MetadataValue::String(date.to_string()),
        );
    }

    // Parse INI sections (";"-prefixed lines are comments).
    let mut section: Option<String> = None;
    let mut sections: std::collections::HashMap<String, Vec<(String, String)>> =
        std::collections::HashMap::new();
    for line in lines {
        let line = line.replace("ReadFromDoc", "");
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') {
            continue;
        }
        if line.starts_with('[') && line.ends_with(']') {
            section = Some(line[1..line.len() - 1].to_string());
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            continue;
        };
        if let Some(section) = section.as_deref() {
            sections
                .entry(section.to_string())
                .or_default()
                .push((key.trim().to_string(), value.trim().to_string()));
        }
    }

    let table_get = |table: &str, key: &str| -> Option<String> {
        sections.get(table).and_then(|entries| {
            entries
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(key))
                .map(|(_, v)| v.clone())
        })
    };

    // " MICROSCOPE " -> magnification + immersion from "Objective".
    if let Some(objective) = table_get(" MICROSCOPE ", "Objective") {
        if let Some(space) = objective.find(' ') {
            // Java parses substring(0, space - 1) then substring(space + 1).
            let mag_end = space.saturating_sub(1);
            if let Ok(mag) = objective[..mag_end].trim().parse::<f64>() {
                metadata.insert(
                    "simplepci.objective_magnification".into(),
                    crate::common::metadata::MetadataValue::Float(mag),
                );
            }
            let immersion = objective[space + 1..].trim();
            if !immersion.is_empty() {
                metadata.insert(
                    "simplepci.immersion".into(),
                    crate::common::metadata::MetadataValue::String(immersion.to_string()),
                );
            }
        }
    }

    // " CAPTURE DEVICE " -> binning, camera type/name, bits per pixel.
    if let Some(binning) = table_get(" CAPTURE DEVICE ", "Binning") {
        metadata.insert(
            "simplepci.binning".into(),
            crate::common::metadata::MetadataValue::String(format!("{binning}x{binning}")),
        );
    }
    if let Some(camera_type) = table_get(" CAPTURE DEVICE ", "Camera Type") {
        metadata.insert(
            "simplepci.camera_type".into(),
            crate::common::metadata::MetadataValue::String(camera_type),
        );
    }
    if let Some(camera_name) = table_get(" CAPTURE DEVICE ", "Camera Name") {
        metadata.insert(
            "simplepci.camera_name".into(),
            crate::common::metadata::MetadataValue::String(camera_name),
        );
    }
    let bits = table_get(" CAPTURE DEVICE ", "Display Depth")
        .and_then(|d| d.trim().parse::<i64>().ok())
        .or_else(|| {
            table_get(" CAPTURE DEVICE ", "Bit Depth").and_then(|d| {
                // Java strips a trailing "-bit" suffix before parsing.
                let d = d.trim();
                let trimmed = d.strip_suffix("-bit").unwrap_or(d);
                trimmed.trim().parse::<i64>().ok()
            })
        });
    if let Some(bits) = bits {
        metadata.insert(
            "simplepci.bits_per_pixel".into(),
            crate::common::metadata::MetadataValue::Int(bits),
        );
    }

    // " CAPTURE " -> per-channel exposure times where a filter is present.
    if let Some(entries) = sections.get(" CAPTURE ") {
        let mut index = 1usize;
        loop {
            let filter_key = format!("c_Filter{index}");
            let has_filter = entries
                .iter()
                .any(|(k, _)| k.eq_ignore_ascii_case(&filter_key));
            if !has_filter {
                break;
            }
            let expos_key = format!("c_Expos{index}");
            if let Some((_, value)) = entries
                .iter()
                .find(|(k, _)| k.eq_ignore_ascii_case(&expos_key))
            {
                if let Ok(exposure) = value.trim().parse::<f64>() {
                    metadata.insert(
                        format!("simplepci.exposure_time_{index}"),
                        crate::common::metadata::MetadataValue::Float(exposure),
                    );
                }
            }
            index += 1;
        }
    }

    // " CALIBRATION " -> physical units + scaling factor.
    if let Some(units) = table_get(" CALIBRATION ", "units") {
        metadata.insert(
            "simplepci.calibration_units".into(),
            crate::common::metadata::MetadataValue::String(units),
        );
    }
    if let Some(factor) = table_get(" CALIBRATION ", "factor") {
        if let Ok(scaling) = factor.trim().parse::<f64>() {
            metadata.insert(
                "simplepci.calibration_factor".into(),
                crate::common::metadata::MetadataValue::Float(scaling),
            );
        }
    }
}

fn insert_simplepci_xml_metadata(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    desc: &str,
) {
    if !desc.contains('<') {
        return;
    }

    let tags = xml_scan_tags(desc);
    insert_simplepci_hierarchy_scalar_metadata(metadata, desc, &tags);

    let mut scalar_count = 0usize;
    for tag in tags.iter().take(128) {
        if scalar_count >= 256 {
            break;
        }

        let tag_key = simplepci_xml_key(&tag.name);
        if tag_key.is_empty() {
            continue;
        }

        let mut attr_names: Vec<_> = tag.attrs.keys().map(String::as_str).collect();
        attr_names.sort_unstable_by_key(|name| name.to_ascii_lowercase());
        for attr in attr_names.into_iter().take(32) {
            if scalar_count >= 256 {
                break;
            }
            let Some(value) = xml_attr_case_insensitive(&tag.attrs, attr) else {
                continue;
            };
            let attr_key = simplepci_xml_key(attr);
            if attr_key.is_empty() {
                continue;
            }
            insert_parsed_metadata_value(
                metadata,
                format!("simplepci.xml.{tag_key}.{attr_key}"),
                value,
            );
            insert_simplepci_xml_alias(metadata, &tag_key, &attr_key, value);
            scalar_count += 1;
        }

        if scalar_count < 256 {
            if let Some(text) = xml_element_text(desc, tag) {
                let text: String = text.chars().take(4096).collect();
                insert_parsed_metadata_value(
                    metadata,
                    format!("simplepci.xml.{tag_key}.text"),
                    &text,
                );
                insert_simplepci_xml_text_alias(metadata, &tag_key, &text);
                scalar_count += 1;
            }
        }
    }

    if scalar_count > 0 {
        metadata.insert(
            "simplepci.xml_scalar_count".into(),
            crate::common::metadata::MetadataValue::Int(scalar_count as i64),
        );
    }
}

fn simplepci_xml_key(key: &str) -> String {
    simplepci_tiff_metadata_key(&nikon_key_suffix(key))
}

fn insert_simplepci_xml_alias(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    tag_key: &str,
    attr_key: &str,
    value: &str,
) {
    let alias = match (tag_key, attr_key) {
        (_, "exposure_time") => Some("exposure_time"),
        (_, "objective_magnification") => Some("objective_magnification"),
        ("objective", "magnification") => Some("objective_magnification"),
        ("channel", "name") | ("wavelength", "channel_name") => Some("channel_name"),
        (_, "channel_name") => Some("channel_name"),
        (_, "wavelength") => Some("wavelength"),
        (_, "well") | ("well", "id") | ("well", "name") => Some("well"),
        (_, "site") | ("site", "id") | ("field", "id") => Some("site"),
        _ => None,
    };
    if let Some(alias) = alias {
        let key = format!("simplepci.{alias}");
        if !metadata.contains_key(&key) {
            insert_parsed_metadata_value(metadata, key, value);
        }
    }
}

fn insert_simplepci_xml_text_alias(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    tag_key: &str,
    value: &str,
) {
    let alias = match tag_key {
        "exposure_time" => Some("exposure_time"),
        "objective_magnification" => Some("objective_magnification"),
        "channel_name" => Some("channel_name"),
        "wavelength" => Some("wavelength"),
        "well" | "well_id" => Some("well"),
        "site" | "site_id" | "field" | "field_id" => Some("site"),
        _ => None,
    };
    if let Some(alias) = alias {
        let key = format!("simplepci.{alias}");
        if !metadata.contains_key(&key) {
            insert_parsed_metadata_value(metadata, key, value);
        }
    }
}

fn insert_simplepci_hierarchy_scalar_metadata(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    xml: &str,
    tags: &[XmlTag],
) {
    #[derive(Clone)]
    struct StackNode {
        suffix: String,
        end_offset: usize,
        interesting: bool,
    }

    let mut stack: Vec<StackNode> = Vec::new();
    let mut node_count = 0usize;
    let mut scalar_count = 0usize;

    for tag in tags {
        while stack
            .last()
            .is_some_and(|node| tag.start_offset >= node.end_offset)
        {
            stack.pop();
        }

        let suffix = simplepci_xml_key(&tag.name);
        if suffix.is_empty() {
            continue;
        }
        let interesting = simplepci_is_hierarchy_object_tag(&suffix);
        let in_interesting_path = interesting || stack.iter().any(|node| node.interesting);

        if in_interesting_path && !simplepci_is_xml_root_tag(&suffix) {
            let mut scalars: Vec<(String, String)> = Vec::new();

            let mut attr_names: Vec<_> = tag.attrs.keys().map(String::as_str).collect();
            attr_names.sort_unstable_by_key(|name| name.to_ascii_lowercase());
            for attr in attr_names.into_iter().take(32) {
                if let Some(value) = xml_attr_case_insensitive(&tag.attrs, attr) {
                    scalars.push((simplepci_xml_key(attr), value.to_string()));
                }
            }

            if let Some(text) = xml_element_text(xml, tag) {
                scalars.push(("text".into(), text.chars().take(4096).collect()));
            }

            if !scalars.is_empty() && node_count < 64 {
                let mut path: Vec<&str> = stack
                    .iter()
                    .filter(|node| node.interesting)
                    .filter(|node| !simplepci_is_xml_root_tag(&node.suffix))
                    .map(|node| node.suffix.as_str())
                    .collect();
                path.push(&suffix);

                let node_key = format!("simplepci.hierarchy.{node_count}");
                metadata.insert(
                    format!("{node_key}.path"),
                    crate::common::metadata::MetadataValue::String(path.join(".")),
                );
                metadata.insert(
                    format!("{node_key}.type"),
                    crate::common::metadata::MetadataValue::String(suffix.clone()),
                );
                metadata.insert(
                    format!("{node_key}.depth"),
                    crate::common::metadata::MetadataValue::Int(path.len() as i64),
                );

                for (key, value) in scalars {
                    if scalar_count >= 256 {
                        break;
                    }
                    if !key.is_empty() {
                        insert_parsed_metadata_value(metadata, format!("{node_key}.{key}"), &value);
                        scalar_count += 1;
                    }
                }
                node_count += 1;
            }
        }

        if !tag.self_closing && stack.len() < 8 {
            let end_offset = xml_matching_end_offset(xml, tag).unwrap_or(xml.len());
            stack.push(StackNode {
                suffix,
                end_offset,
                interesting,
            });
        }

        if node_count >= 64 || scalar_count >= 256 {
            break;
        }
    }

    if node_count > 0 {
        metadata.insert(
            "simplepci.hierarchy.node_count".into(),
            crate::common::metadata::MetadataValue::Int(node_count as i64),
        );
        metadata.insert(
            "simplepci.hierarchy.scalar_count".into(),
            crate::common::metadata::MetadataValue::Int(scalar_count as i64),
        );
    }
}

fn simplepci_is_xml_root_tag(suffix: &str) -> bool {
    matches!(
        suffix,
        "hc_image" | "h_c_image" | "simplepci" | "simple_pci" | "simple_p_c_i"
    )
}

fn simplepci_is_hierarchy_object_tag(suffix: &str) -> bool {
    simplepci_is_xml_root_tag(suffix)
        || matches!(
            suffix,
            "acquisition"
                | "calibration"
                | "camera"
                | "capture"
                | "channel"
                | "channels"
                | "experiment"
                | "field"
                | "filter"
                | "image"
                | "lens"
                | "microscope"
                | "objective"
                | "plane"
                | "sequence"
                | "site"
                | "stage"
                | "time_point"
                | "wavelength"
                | "well"
                | "xy_stage"
                | "z_stage"
        )
}

fn simplepci_tiff_metadata_key(key: &str) -> String {
    key.trim()
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                '_'
            }
        })
        .collect::<String>()
        .split('_')
        .filter(|part| !part.is_empty())
        .collect::<Vec<_>>()
        .join("_")
}

fn moldev_insert_shallow_xml_metadata(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    xml: &str,
    tags: &[XmlTag],
) {
    let mut object_count = 0usize;
    let mut scalar_count = 0usize;
    for tag in tags
        .iter()
        .filter(|tag| moldev_is_shallow_object_tag(&tag.name))
    {
        if object_count >= 64 || scalar_count >= 256 {
            break;
        }

        let object_key = format!("moldev.object.{object_count}");
        metadata.insert(
            format!("{object_key}.type"),
            crate::common::metadata::MetadataValue::String(nikon_key_suffix(&tag.name)),
        );
        object_count += 1;

        let mut attr_names: Vec<_> = tag.attrs.keys().map(String::as_str).collect();
        attr_names.sort_unstable_by_key(|name| name.to_ascii_lowercase());
        for attr in attr_names.into_iter().take(32) {
            if scalar_count >= 256 {
                break;
            }
            let Some(value) = xml_attr_case_insensitive(&tag.attrs, attr) else {
                continue;
            };
            insert_parsed_metadata_value(
                metadata,
                format!("{object_key}.{}", nikon_key_suffix(attr)),
                value,
            );
            insert_moldev_alias(metadata, &tag.name, attr, value);
            scalar_count += 1;
        }

        if scalar_count < 256 {
            if let Some(text) = xml_element_text(xml, tag) {
                let text: String = text.chars().take(4096).collect();
                insert_parsed_metadata_value(metadata, format!("{object_key}.text"), &text);
                insert_moldev_text_alias(metadata, &tag.name, &text);
                scalar_count += 1;
            }
        }
    }

    if object_count > 0 {
        metadata.insert(
            "moldev.object_count".into(),
            crate::common::metadata::MetadataValue::Int(object_count as i64),
        );
        metadata.insert(
            "moldev.object.scalar_count".into(),
            crate::common::metadata::MetadataValue::Int(scalar_count as i64),
        );
    }
}

fn moldev_insert_hierarchy_scalar_metadata(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    xml: &str,
    tags: &[XmlTag],
) {
    #[derive(Clone)]
    struct StackNode {
        suffix: String,
        end_offset: usize,
        interesting: bool,
    }

    let mut stack: Vec<StackNode> = Vec::new();
    let mut node_count = 0usize;
    let mut scalar_count = 0usize;

    for tag in tags {
        while stack
            .last()
            .is_some_and(|node| tag.start_offset >= node.end_offset)
        {
            stack.pop();
        }

        let suffix = nikon_key_suffix(&tag.name);
        let interesting = moldev_is_hierarchy_object_tag(&suffix);
        let in_interesting_path = interesting || stack.iter().any(|node| node.interesting);

        if in_interesting_path && suffix != "meta_xpress" && suffix != "metaxpress" {
            let mut scalars: Vec<(String, String)> = Vec::new();

            let mut attr_names: Vec<_> = tag.attrs.keys().map(String::as_str).collect();
            attr_names.sort_unstable_by_key(|name| name.to_ascii_lowercase());
            for attr in attr_names.into_iter().take(32) {
                if let Some(value) = xml_attr_case_insensitive(&tag.attrs, attr) {
                    scalars.push((nikon_key_suffix(attr), value.to_string()));
                }
            }

            if let Some(text) = xml_element_text(xml, tag) {
                scalars.push(("text".into(), text.chars().take(4096).collect()));
            }

            if !scalars.is_empty() && node_count < 64 {
                let mut path: Vec<&str> = stack
                    .iter()
                    .filter(|node| node.interesting)
                    .filter(|node| node.suffix != "meta_xpress" && node.suffix != "metaxpress")
                    .map(|node| node.suffix.as_str())
                    .collect();
                path.push(&suffix);

                let node_key = format!("moldev.hierarchy.{node_count}");
                metadata.insert(
                    format!("{node_key}.path"),
                    crate::common::metadata::MetadataValue::String(path.join(".")),
                );
                metadata.insert(
                    format!("{node_key}.type"),
                    crate::common::metadata::MetadataValue::String(suffix.clone()),
                );
                metadata.insert(
                    format!("{node_key}.depth"),
                    crate::common::metadata::MetadataValue::Int(path.len() as i64),
                );

                for (key, value) in scalars {
                    if scalar_count >= 256 {
                        break;
                    }
                    insert_parsed_metadata_value(metadata, format!("{node_key}.{key}"), &value);
                    scalar_count += 1;
                }
                node_count += 1;
            }
        }

        if !tag.self_closing && stack.len() < 8 {
            let end_offset = xml_matching_end_offset(xml, tag).unwrap_or(xml.len());
            stack.push(StackNode {
                suffix,
                end_offset,
                interesting,
            });
        }

        if node_count >= 64 || scalar_count >= 256 {
            break;
        }
    }

    if node_count > 0 {
        metadata.insert(
            "moldev.hierarchy.node_count".into(),
            crate::common::metadata::MetadataValue::Int(node_count as i64),
        );
        metadata.insert(
            "moldev.hierarchy.scalar_count".into(),
            crate::common::metadata::MetadataValue::Int(scalar_count as i64),
        );
    }
}

fn moldev_is_shallow_object_tag(name: &str) -> bool {
    matches!(
        nikon_key_suffix(name).as_str(),
        "acquisition"
            | "channel"
            | "field"
            | "image"
            | "meta_xpress"
            | "metaxpress"
            | "objective"
            | "plate"
            | "plate_id"
            | "site"
            | "site_id"
            | "well"
            | "well_id"
            | "wavelength"
    )
}

fn moldev_is_hierarchy_object_tag(suffix: &str) -> bool {
    matches!(
        suffix,
        "acquisition"
            | "acquisition_settings"
            | "channel"
            | "channels"
            | "field"
            | "fields"
            | "image"
            | "image_info"
            | "meta_xpress"
            | "metaxpress"
            | "objective"
            | "plate"
            | "scan_profile"
            | "site"
            | "site_id"
            | "well"
            | "well_id"
            | "wavelength"
    )
}

fn insert_moldev_alias(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    tag_name: &str,
    attr_name: &str,
    value: &str,
) {
    let tag_key = nikon_key_suffix(tag_name);
    let attr_key = nikon_key_suffix(attr_name);
    let alias = match (tag_key.as_str(), attr_key.as_str()) {
        ("plate", "id") | ("plate_id", "value") => Some("plateid"),
        ("well", "id") | ("well_id", "value") => Some("wellid"),
        ("site", "id") | ("site_id", "value") | ("field", "id") => Some("siteid"),
        ("wavelength", "value") | ("channel", "wavelength") => Some("wavelength"),
        ("acquisition", "exposure_time") | ("image", "exposure_time") => Some("exposuretime"),
        ("objective", "magnification") => Some("objectivemagnification"),
        _ => None,
    };
    if let Some(alias) = alias {
        let key = format!("moldev.{alias}");
        if !metadata.contains_key(&key) {
            insert_parsed_metadata_value(metadata, key, value);
        }
    }
}

fn insert_moldev_text_alias(
    metadata: &mut std::collections::HashMap<String, crate::common::metadata::MetadataValue>,
    tag_name: &str,
    value: &str,
) {
    let alias = match nikon_key_suffix(tag_name).as_str() {
        "plate_id" => Some("plateid"),
        "well_id" => Some("wellid"),
        "site_id" => Some("siteid"),
        "wavelength" => Some("wavelength"),
        "exposure_time" => Some("exposuretime"),
        "objective_magnification" => Some("objectivemagnification"),
        _ => None,
    };
    if let Some(alias) = alias {
        let key = format!("moldev.{alias}");
        if !metadata.contains_key(&key) {
            insert_parsed_metadata_value(metadata, key, value);
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
        // Java `MetaxpressTiffReader` accepts `.htd` (plate index) and `.tif`.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif") | Some("htd"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;

        // Cross-file MetaXpress plate: a `.htd` index drives the multi-series
        // (well x field) core. A plain `.tif` opened directly keeps the legacy
        // single-file enrichment path.
        let is_htd = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("htd"))
            .unwrap_or(false);
        if is_htd {
            let htd = metaxpress_find_htd(path)?;
            return self.set_id_plate(&htd);
        }

        self.inner.set_id(path)?;
        self.enrich_metadata();
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.plate = None;
        self.plate_series.clear();
        self.current_series = 0;
        self.plate_tiff_loaded = false;
        self.plate_ome_template = None;
        self.inner.close()
    }
    fn series_count(&self) -> usize {
        if self.plate.is_some() {
            self.plate_series.len()
        } else {
            self.inner.series_count()
        }
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.plate.is_some() {
            if self.plate_series.is_empty() {
                Err(BioFormatsError::NotInitialized)
            } else if s >= self.plate_series.len() {
                Err(BioFormatsError::SeriesOutOfRange(s))
            } else {
                self.current_series = s;
                Ok(())
            }
        } else {
            self.inner.set_series(s)
        }
    }
    fn series(&self) -> usize {
        if self.plate.is_some() {
            self.current_series
        } else {
            self.inner.series()
        }
    }
    fn metadata(&self) -> &ImageMetadata {
        if self.plate.is_some() {
            self.plate_series
                .get(self.current_series)
                .unwrap_or(crate::common::reader::uninitialized_metadata())
        } else {
            self.inner.metadata()
        }
    }
    fn open_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if self.plate.is_some() {
            self.open_plate_bytes(p)
        } else {
            self.inner.open_bytes(p)
        }
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.plate.is_some() {
            let full = self.open_plate_bytes(p)?;
            let meta = self
                .plate_series
                .get(self.current_series)
                .ok_or(BioFormatsError::NotInitialized)?;
            validate_region("MetaXpress", meta.size_x, meta.size_y, x, y, w, h)?;
            return crop_plate_plane(&full, meta, x, y, w, h);
        }
        self.inner.open_bytes_region(p, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, p: u32) -> Result<Vec<u8>> {
        if self.plate.is_some() {
            return self.open_plate_bytes(p);
        }
        self.inner.open_thumb_bytes(p)
    }
    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let Some(plate) = self.plate.as_ref() else {
            return self.inner.ome_metadata();
        };
        if self.plate_series.is_empty() {
            return None;
        }

        let template_image = self
            .plate_ome_template
            .as_ref()
            .and_then(|ome| ome.images.first());
        let mut ome = crate::common::ome_metadata::OmeMetadata::default();

        for (series_index, meta) in self.plate_series.iter().enumerate() {
            let mut image_ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
            let field = series_index % plate.field_count;
            let well_index = series_index / plate.field_count;
            let (row, col) = plate.selected_wells[well_index];
            if let Some(image) = image_ome.images.get_mut(0) {
                image.name = Some(format!(
                    "Well {} Field #{}",
                    metaxpress_well_name(row, col),
                    field + 1
                ));
                if let Some(template) = template_image {
                    image.physical_size_x = template.physical_size_x;
                    image.physical_size_y = template.physical_size_y;
                    image.physical_size_z = template.physical_size_z;
                    image.time_increment = template.time_increment;
                }
                if image.physical_size_x.is_none() {
                    image.physical_size_x = Some(1.6125);
                }
                if image.physical_size_y.is_none() {
                    image.physical_size_y = Some(1.6125);
                }
                for (channel_index, channel) in image.channels.iter_mut().enumerate() {
                    if let Some(Some(name)) = plate.wavelengths.get(channel_index) {
                        channel.name = Some(name.clone());
                    }
                }
                image.instrument_ref = Some(0);
                if image.planes.is_empty() {
                    for t in 0..meta.size_t {
                        for z in 0..meta.size_z {
                            for c in 0..meta.size_c {
                                image.planes.push(crate::common::ome_metadata::OmePlane {
                                    the_z: z,
                                    the_c: c,
                                    the_t: t,
                                    ..Default::default()
                                });
                            }
                        }
                    }
                }
            }
            ome.images.extend(image_ome.images);
        }

        if let Some(template) = &self.plate_ome_template {
            ome.instruments = template.instruments.clone();
        }
        if ome.instruments.is_empty() {
            ome.instruments
                .push(crate::common::ome_metadata::OmeInstrument {
                    id: Some(crate::common::ome_metadata::create_lsid("Instrument", &[0])),
                    detectors: vec![crate::common::ome_metadata::OmeDetector {
                        id: Some(crate::common::ome_metadata::create_lsid(
                            "Detector",
                            &[0, 0],
                        )),
                        ..Default::default()
                    }],
                    ..Default::default()
                });
        }

        let mut wells = Vec::with_capacity(plate.selected_wells.len());
        for (well_index, &(row, col)) in plate.selected_wells.iter().enumerate() {
            let mut well_samples = Vec::with_capacity(plate.field_count);
            for field in 0..plate.field_count {
                let image_index = well_index * plate.field_count + field;
                well_samples.push(crate::common::ome_metadata::OmeWellSample {
                    id: Some(crate::common::ome_metadata::create_lsid(
                        "WellSample",
                        &[0, well_index, field],
                    )),
                    index: field as u32,
                    image_ref: Some(image_index),
                    position_x: None,
                    position_y: None,
                });
            }
            wells.push(crate::common::ome_metadata::OmeWell {
                id: Some(crate::common::ome_metadata::create_lsid(
                    "Well",
                    &[0, well_index],
                )),
                row: row as u32,
                column: col as u32,
                well_samples,
            });
        }
        ome.plates.push(crate::common::ome_metadata::OmePlate {
            id: Some(crate::common::ome_metadata::create_lsid("Plate", &[0])),
            name: None,
            rows: 0,
            columns: 0,
            wells,
        });

        Some(ome)
    }
    fn resolution_count(&self) -> usize {
        if self.plate.is_some() {
            1
        } else {
            self.inner.resolution_count()
        }
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.plate.is_some() {
            if level == 0 {
                Ok(())
            } else {
                Err(BioFormatsError::SeriesOutOfRange(level))
            }
        } else {
            self.inner.set_resolution(level)
        }
    }
    fn resolution(&self) -> usize {
        if self.plate.is_some() {
            0
        } else {
            self.inner.resolution()
        }
    }
}

/// Crop a full decoded plate plane to the requested region (row-major, no
/// RGB interleave — MetaXpress series are single-sample per channel).
fn crop_plate_plane(
    full: &[u8],
    meta: &ImageMetadata,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    let bps = meta.pixel_type.bytes_per_sample();
    let row_stride = meta.size_x as usize * bps;
    let out_stride = w as usize * bps;
    let mut out = vec![0u8; out_stride * h as usize];
    for row in 0..h as usize {
        let src_row = (y as usize + row) * row_stride + x as usize * bps;
        let dst_row = row * out_stride;
        if src_row + out_stride <= full.len() {
            out[dst_row..dst_row + out_stride]
                .copy_from_slice(&full[src_row..src_row + out_stride]);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod nikon_elements_tiff_tests {
    use super::*;
    use crate::common::metadata::MetadataValue;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bioformats_nis_tiff_{name}_{}_{}.tiff",
            std::process::id(),
            unique
        ))
    }

    fn tiff_entry(tag: u16, typ: u16, count: u32, value: u32) -> [u8; 12] {
        let mut entry = [0u8; 12];
        entry[0..2].copy_from_slice(&tag.to_le_bytes());
        entry[2..4].copy_from_slice(&typ.to_le_bytes());
        entry[4..8].copy_from_slice(&count.to_le_bytes());
        entry[8..12].copy_from_slice(&value.to_le_bytes());
        entry
    }

    fn write_minimal_tiff_with_description(path: &Path, description: &str) {
        let mut desc = description.as_bytes().to_vec();
        desc.push(0);

        let ifd_entry_count = 11u32;
        let ifd_start = 8u32;
        let desc_start = ifd_start + 2 + ifd_entry_count * 12 + 4;
        let pixel_start = desc_start + desc.len() as u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());

        let entries = [
            tiff_entry(256, 4, 1, 1),
            tiff_entry(257, 4, 1, 1),
            tiff_entry(258, 3, 1, 8),
            tiff_entry(259, 3, 1, 1),
            tiff_entry(262, 3, 1, 1),
            tiff_entry(270, 2, desc.len() as u32, desc_start),
            tiff_entry(273, 4, 1, pixel_start),
            tiff_entry(277, 3, 1, 1),
            tiff_entry(278, 4, 1, 1),
            tiff_entry(279, 4, 1, 1),
            tiff_entry(284, 3, 1, 1),
        ];

        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&desc);
        bytes.push(7);

        std::fs::write(path, bytes).unwrap();
    }

    fn write_minimal_tiff_with_ascii_tags(path: &Path, tags: &[(u16, &str)]) {
        let mut payloads: Vec<Vec<u8>> = tags
            .iter()
            .map(|(_, value)| {
                let mut bytes = value.as_bytes().to_vec();
                bytes.push(0);
                bytes
            })
            .collect();

        let ifd_entry_count = 10u32 + tags.len() as u32;
        let ifd_start = 8u32;
        let payload_start = ifd_start + 2 + ifd_entry_count * 12 + 4;
        let mut payload_offset = payload_start;
        let pixel_start = payload_start + payloads.iter().map(|p| p.len() as u32).sum::<u32>();

        let mut entries = vec![
            tiff_entry(256, 4, 1, 1),
            tiff_entry(257, 4, 1, 1),
            tiff_entry(258, 3, 1, 8),
            tiff_entry(259, 3, 1, 1),
            tiff_entry(262, 3, 1, 1),
            tiff_entry(273, 4, 1, pixel_start),
            tiff_entry(277, 3, 1, 1),
            tiff_entry(278, 4, 1, 1),
            tiff_entry(279, 4, 1, 1),
            tiff_entry(284, 3, 1, 1),
        ];
        for ((tag, _), payload) in tags.iter().zip(payloads.iter()) {
            entries.push(tiff_entry(*tag, 2, payload.len() as u32, payload_offset));
            payload_offset += payload.len() as u32;
        }

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        for payload in payloads.drain(..) {
            bytes.extend_from_slice(&payload);
        }
        bytes.push(9);

        std::fs::write(path, bytes).unwrap();
    }

    fn write_minimal_sis_tiff_with_binary_metadata(path: &Path, channel_name: &str) {
        let mut software = b"analySIS 5.0".to_vec();
        software.push(0);

        let mut sis_metadata = vec![0u8; 260];
        sis_metadata[10..12].copy_from_slice(&34i16.to_le_bytes());
        sis_metadata[12..14].copy_from_slice(&12i16.to_le_bytes());
        sis_metadata[14..16].copy_from_slice(&2i16.to_le_bytes());
        sis_metadata[16..18].copy_from_slice(&0i16.to_le_bytes());
        sis_metadata[18..20].copy_from_slice(&120i16.to_le_bytes());
        sis_metadata[26..33].copy_from_slice(b"Sample\0");

        let meta = 68usize;
        sis_metadata[meta + 10..meta + 12].copy_from_slice(&(-6i16).to_le_bytes());
        sis_metadata[meta + 12..meta + 20].copy_from_slice(&1.0f64.to_le_bytes());
        sis_metadata[meta + 20..meta + 28].copy_from_slice(&1.0f64.to_le_bytes());
        sis_metadata[meta + 36..meta + 44].copy_from_slice(&20.0f64.to_le_bytes());
        sis_metadata[meta + 44..meta + 46].copy_from_slice(&5i16.to_le_bytes());
        let channel_bytes = channel_name.as_bytes();
        let channel_start = meta + 46;
        sis_metadata[channel_start..channel_start + channel_bytes.len()]
            .copy_from_slice(channel_bytes);

        let ifd_entry_count = 12u32;
        let ifd_start = 8u32;
        let payload_start = ifd_start + 2 + ifd_entry_count * 12 + 4;
        let software_start = payload_start;
        let sis_metadata_start = software_start + software.len() as u32;
        let pixel_start = sis_metadata_start + sis_metadata.len() as u32;

        let entries = [
            tiff_entry(256, 4, 1, 1),
            tiff_entry(257, 4, 1, 1),
            tiff_entry(258, 3, 1, 8),
            tiff_entry(259, 3, 1, 1),
            tiff_entry(262, 3, 1, 1),
            tiff_entry(273, 4, 1, pixel_start),
            tiff_entry(277, 3, 1, 1),
            tiff_entry(278, 4, 1, 1),
            tiff_entry(279, 4, 1, 1),
            tiff_entry(284, 3, 1, 1),
            tiff_entry(SIS_TAG, 4, 1, sis_metadata_start),
            tiff_entry(
                crate::tiff::ifd::tag::SOFTWARE,
                2,
                software.len() as u32,
                software_start,
            ),
        ];

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&software);
        bytes.extend_from_slice(&sis_metadata);
        bytes.push(9);

        std::fs::write(path, bytes).unwrap();
    }

    fn write_minimal_tiff_with_nikon_private_xml(
        path: &Path,
        primary_xml: Option<&str>,
        fallback_xml: Option<&str>,
        image_description: &str,
    ) {
        let mut payloads: Vec<(u16, Vec<u8>)> = Vec::new();
        if let Some(xml) = primary_xml {
            let mut bytes = xml.as_bytes().to_vec();
            bytes.push(0);
            payloads.push((NIKON_ELEMENTS_XML_TAG, bytes));
        }
        if let Some(xml) = fallback_xml {
            let mut bytes = xml.as_bytes().to_vec();
            bytes.push(0);
            payloads.push((NIKON_ELEMENTS_XML_TAG_2, bytes));
        }

        let mut desc = image_description.as_bytes().to_vec();
        desc.push(0);

        let ifd_entry_count = 11u32 + payloads.len() as u32;
        let ifd_start = 8u32;
        let data_start = ifd_start + 2 + ifd_entry_count * 12 + 4;
        let mut next_data = data_start;

        let desc_start = next_data;
        next_data += desc.len() as u32;

        let mut private_entries = Vec::new();
        for (tag, bytes) in &payloads {
            private_entries.push(tiff_entry(*tag, 2, bytes.len() as u32, next_data));
            next_data += bytes.len() as u32;
        }
        let pixel_start = next_data;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());

        let mut entries = vec![
            tiff_entry(256, 4, 1, 1),
            tiff_entry(257, 4, 1, 1),
            tiff_entry(258, 3, 1, 8),
            tiff_entry(259, 3, 1, 1),
            tiff_entry(262, 3, 1, 1),
            tiff_entry(270, 2, desc.len() as u32, desc_start),
            tiff_entry(273, 4, 1, pixel_start),
            tiff_entry(277, 3, 1, 1),
            tiff_entry(278, 4, 1, 1),
            tiff_entry(279, 4, 1, 1),
            tiff_entry(284, 3, 1, 1),
        ];
        entries.extend(private_entries);

        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&desc);
        for (_, payload) in payloads {
            bytes.extend_from_slice(&payload);
        }
        bytes.push(7);

        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn java_tiff_wrapper_suffix_sufficiency_matches() {
        assert!(!NikonElementsTiffReader::new().is_this_type_by_name(Path::new("sample.tif")));
        assert!(!NikonElementsTiffReader::new().is_this_type_by_name(Path::new("sample.tiff")));
        assert!(!FeiTiffReader::new().is_this_type_by_name(Path::new("sample.tif")));
        assert!(!FeiTiffReader::new().is_this_type_by_name(Path::new("sample.tiff")));
        assert!(!SisReader::new().is_this_type_by_name(Path::new("sample.tif")));
        assert!(!SisReader::new().is_this_type_by_name(Path::new("sample.tiff")));
        assert!(!ImprovisionTiffReader::new().is_this_type_by_name(Path::new("sample.tif")));
        assert!(!ImprovisionTiffReader::new().is_this_type_by_name(Path::new("sample.tiff")));
        assert!(ZeissApotomeTiffReader::new().is_this_type_by_name(Path::new("sample.tif")));
        assert!(ZeissApotomeTiffReader::new().is_this_type_by_name(Path::new("sample.tiff")));
        assert!(FluoviewReader::new().is_this_type_by_name(Path::new("sample.tif")));
        assert!(FluoviewReader::new().is_this_type_by_name(Path::new("sample.tiff")));
    }

    #[test]
    fn nikon_elements_detection_requires_java_private_xml_tag() {
        let tagged = temp_path("nikon-elements-tagged");
        write_minimal_tiff_with_nikon_private_xml(
            &tagged,
            Some("<variant>Ti2</variant>"),
            None,
            "",
        );
        let tagged_bytes = std::fs::read(&tagged).unwrap();
        assert!(NikonElementsTiffReader::new().is_this_type_by_bytes(&tagged_bytes));

        let plain = temp_path("nikon-elements-plain");
        write_minimal_tiff_with_description(&plain, "plain TIFF");
        let plain_bytes = std::fs::read(&plain).unwrap();
        assert!(!NikonElementsTiffReader::new().is_this_type_by_bytes(&plain_bytes));

        let _ = std::fs::remove_file(tagged);
        let _ = std::fs::remove_file(plain);
    }

    #[test]
    fn fei_and_sis_detection_matches_java_private_tags() {
        let fei_path = temp_path("fei_detection");
        write_minimal_tiff_with_ascii_tags(
            &fei_path,
            &[(FEI_HELIOS_TAG, "[User]\nDate=01/02/2020")],
        );
        let fei_bytes = std::fs::read(&fei_path).unwrap();
        assert!(FeiTiffReader::new().is_this_type_by_bytes(&fei_bytes));

        let plain_path = temp_path("plain_detection");
        write_minimal_tiff_with_ascii_tags(&plain_path, &[]);
        let plain_bytes = std::fs::read(&plain_path).unwrap();
        assert!(!FeiTiffReader::new().is_this_type_by_bytes(&plain_bytes));
        assert!(!SisReader::new().is_this_type_by_bytes(&plain_bytes));

        let sis_path = temp_path("sis_detection");
        write_minimal_tiff_with_ascii_tags(
            &sis_path,
            &[
                (SIS_TAG, ""),
                (crate::tiff::ifd::tag::SOFTWARE, "analySIS 5.0"),
            ],
        );
        let sis_bytes = std::fs::read(&sis_path).unwrap();
        assert!(SisReader::new().is_this_type_by_bytes(&sis_bytes));

        let sis2_path = temp_path("sis_detection_tag2");
        write_minimal_tiff_with_ascii_tags(&sis2_path, &[(SIS_TAG_2, ""), (TIFF_MAKE, "Olympus")]);
        let sis2_bytes = std::fs::read(&sis2_path).unwrap();
        assert!(SisReader::new().is_this_type_by_bytes(&sis2_bytes));
    }

    #[test]
    fn sis_binary_metadata_preserves_java_empty_channel_for_overlong_name() {
        let path = temp_path("sis_overlong_channel");
        let long_channel = "A".repeat(129);
        write_minimal_sis_tiff_with_binary_metadata(&path, &long_channel);

        let mut reader = SisReader::new();
        reader.set_id(&path).unwrap();

        assert!(matches!(
            reader.metadata().series_metadata.get("Channel name"),
            Some(MetadataValue::String(value)) if value.is_empty()
        ));
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].channels[0].name.as_deref(), Some(""));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn sis_ome_enrichment_preserves_delegated_tiff_channel_scaffold() {
        let path = temp_path("sis_ome_channels");
        write_minimal_sis_tiff_with_binary_metadata(&path, "DAPI");

        let mut reader = SisReader::new();
        reader.set_id(&path).unwrap();
        reader.inner.series_list_mut()[0].metadata.size_c = 3;
        reader.inner.series_list_mut()[0].metadata.is_rgb = false;

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].channels.len(), 3);
        assert_eq!(ome.images[0].channels[0].name.as_deref(), Some("DAPI"));
        assert_eq!(ome.images[0].channels[1].name, None);
        assert_eq!(ome.images[0].channels[2].name, None);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fei_helios_private_tag_projects_java_metadata() {
        let path = temp_path("fei_helios_metadata");
        write_minimal_tiff_with_ascii_tags(
            &path,
            &[(
                FEI_HELIOS_TAG,
                "[User]\nDate=01/02/2020\nTime=03:04:05 PM\nUser=Operator\n\
                 [System]\nSystemType=Helios G4\n\
                 [Beam]\nStageX=1.5\nStageY=2.5\nStageZ=3.5\n\
                 [Scan]\nPixelWidth=0.000001\nPixelHeight=0.000002\nFrameTime=0.25\n",
            )],
        );

        let mut reader = FeiTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            metadata.get("Software"),
            Some(MetadataValue::String(value)) if value == "Helios NanoLab"
        ));
        assert!(matches!(
            metadata.get("User User"),
            Some(MetadataValue::String(value)) if value == "Operator"
        ));

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].physical_size_x, Some(1.0));
        assert_eq!(ome.images[0].physical_size_y, Some(2.0));
        assert_eq!(ome.images[0].time_increment, Some(0.25));
        assert_eq!(
            ome.images[0].acquisition_date.as_deref(),
            Some("2020-01-02T15:04:05")
        );
        assert_eq!(ome.images[0].planes[0].position_x, Some(1.5));
        assert_eq!(ome.images[0].planes[0].position_y, Some(2.5));
        assert_eq!(ome.images[0].planes[0].position_z, Some(3.5));
        assert_eq!(
            ome.instruments[0].microscope_model.as_deref(),
            Some("Helios G4")
        );
        assert_eq!(ome.experimenters[0].last_name.as_deref(), Some("Operator"));
    }

    #[test]
    fn fei_sfeg_ini_projects_java_physical_size_without_objective() {
        let path = temp_path("fei_sfeg_metadata");
        write_minimal_tiff_with_ascii_tags(
            &path,
            &[(
                FEI_SFEG_TAG,
                "[DatabarData]\nImageName=SEM image\nszUserText=notes\n\
                 [Vector]\nMagnification=1000\n\
                 [Vector.Sysscan]\nPositionX=4.5\nPositionY=5.5\n\
                 [Vector.Video.Detectors]\nNrDetectorsConnected=1\nDetector_0_Name=ETD\n",
            )],
        );

        let mut reader = FeiTiffReader::new();
        reader.set_id(&path).unwrap();

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].name.as_deref(), Some("SEM image"));
        assert_eq!(ome.images[0].description.as_deref(), Some("notes"));
        assert_eq!(
            ome.images[0].physical_size_x,
            Some(1000.0 * FEI_MAG_MULTIPLIER)
        );
        assert_eq!(
            ome.images[0].physical_size_y,
            Some(1000.0 * FEI_MAG_MULTIPLIER)
        );
        assert_eq!(ome.images[0].planes[0].position_x, Some(4.5));
        assert_eq!(ome.images[0].planes[0].position_y, Some(5.5));
        assert_eq!(
            ome.instruments[0].detectors[0].model.as_deref(),
            Some("ETD")
        );
        assert!(ome.instruments[0].objectives.is_empty());
        assert!(ome.images[0].objective_ref.is_none());
    }

    #[test]
    fn fei_empty_private_tag_still_records_java_software_metadata() {
        let path = temp_path("fei_empty_titan_metadata");
        write_minimal_tiff_with_ascii_tags(
            &path,
            &[(FEI_HELIOS_TAG, "    "), (FEI_TITAN_TAG, "    ")],
        );

        let mut reader = FeiTiffReader::new();
        reader.set_id(&path).unwrap();

        assert!(matches!(
            reader.metadata().series_metadata.get("Software"),
            Some(MetadataValue::String(value)) if value == "Helios NanoLab"
        ));
    }

    #[test]
    fn nikon_elements_tiff_projects_variant_and_channel_metadata() {
        let path = temp_path("variant_metadata");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIS-Elements>
  <variant runtype="TimeLoop" objectiveName="Plan Apo 20x" magnification="20" numericAperture="0.75" cameraName="DS-Qi2"/>
  <Channel name="DAPI" wavelength="405" exposure="50 ms" gain="1.5"/>
  <Channel dyeName="FITC" wavelength="488" exposureTime="12.5" readoutSpeed="2000000"/>
</NIS-Elements>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.variant_count"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("nikon.channel_count"),
            Some(MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("nikon.runtype"),
            Some(MetadataValue::String(value)) if value == "TimeLoop"
        ));
        assert!(matches!(
            metadata.get("nikon.objective_name"),
            Some(MetadataValue::String(value)) if value == "Plan Apo 20x"
        ));
        assert!(matches!(
            metadata.get("nikon.magnification"),
            Some(MetadataValue::Float(value)) if (*value - 20.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("nikon.numeric_aperture"),
            Some(MetadataValue::Float(value)) if (*value - 0.75).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("nikon.channel.0.name"),
            Some(MetadataValue::String(value)) if value == "DAPI"
        ));
        assert!(matches!(
            metadata.get("nikon.channel.1.dye_name"),
            Some(MetadataValue::String(value)) if value == "FITC"
        ));
        assert!(matches!(
            metadata.get("nikon.channel.0.exposure"),
            Some(MetadataValue::String(value)) if value == "50 ms"
        ));
        assert!(matches!(
            metadata.get("nikon.channel.0.gain"),
            Some(MetadataValue::Float(value)) if (*value - 1.5).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("nikon.channel.1.exposure_time"),
            Some(MetadataValue::Float(value)) if (*value - 12.5).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("nikon.channel.1.readout_speed"),
            Some(MetadataValue::Float(value)) if (*value - 2_000_000.0).abs() < f64::EPSILON
        ));
        assert!(metadata.get("nikon.variant.unparsed_diagnostic").is_none());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_elements_tiff_reads_primary_private_xml_tag() {
        let path = temp_path("private_xml_primary");
        write_minimal_tiff_with_nikon_private_xml(
            &path,
            Some(r#"<dCalibration value="0.42"/><CameraUniqueName value="DS-Qi2"/>"#),
            None,
            "plain description without Nikon XML",
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.nd2.pixel_size_x"),
            Some(MetadataValue::Float(v)) if (v - 0.42).abs() < 1e-9
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.camera_model"),
            Some(MetadataValue::String(v)) if v == "DS-Qi2"
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_elements_tiff_falls_back_to_second_private_xml_tag() {
        let path = temp_path("private_xml_fallback");
        write_minimal_tiff_with_nikon_private_xml(
            &path,
            Some(""),
            Some(r#"<dZStep value="1.25"/><ExposureTime value="50"/>"#),
            "plain description without Nikon XML",
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.nd2.pixel_size_z"),
            Some(MetadataValue::Float(v)) if (v - 1.25).abs() < 1e-9
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.channel.0.exposure_time"),
            Some(MetadataValue::Float(v)) if (v - 0.05).abs() < 1e-9
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_elements_tiff_trims_private_xml_prefix_before_parsing() {
        let path = temp_path("private_xml_prefix");
        write_minimal_tiff_with_nikon_private_xml(
            &path,
            Some("NIKON PRIVATE HEADER <dLampVoltage value=\"7.5\"/>"),
            None,
            "plain description without Nikon XML",
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.nd2.voltage"),
            Some(MetadataValue::Float(v)) if (v - 7.5).abs() < 1e-9
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_elements_tiff_projects_scalar_acquisition_value_tags() {
        let path = temp_path("scalar_acquisition");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIS-Elements>
  <variant runtype="SinglePoint"/>
  <CameraUniqueName value="DS-Fi3"/>
  <ExposureTime value="25"/>
  <Gain>2.25</Gain>
  <EmissionWavelength>525</EmissionWavelength>
</NIS-Elements>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.camera_unique_name"),
            Some(MetadataValue::String(value)) if value == "DS-Fi3"
        ));
        assert!(matches!(
            metadata.get("nikon.exposure_time"),
            Some(MetadataValue::Float(value)) if (*value - 25.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("nikon.gain"),
            Some(MetadataValue::Float(value)) if (*value - 2.25).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("nikon.emission_wavelength"),
            Some(MetadataValue::Float(value)) if (*value - 525.0).abs() < f64::EPSILON
        ));
        assert!(metadata.get("nikon.variant.unparsed_diagnostic").is_none());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_elements_tiff_projects_shallow_object_scalar_metadata() {
        let path = temp_path("object_scalar_metadata");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIS-Elements>
  <Experiment name="Drug screen" operator="Ada"/>
  <Microscope model="Eclipse Ti2" serialNumber="TI2-42"/>
  <Stage x="12.5" y="-3.25" unit="um"/>
  <ROI id="R1">nucleus</ROI>
</NIS-Elements>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.object_count"),
            Some(MetadataValue::Int(4))
        ));
        assert!(matches!(
            metadata.get("nikon.object.scalar_count"),
            Some(MetadataValue::Int(9))
        ));
        assert!(matches!(
            metadata.get("nikon.object.0.type"),
            Some(MetadataValue::String(value)) if value == "experiment"
        ));
        assert!(matches!(
            metadata.get("nikon.object.0.name"),
            Some(MetadataValue::String(value)) if value == "Drug screen"
        ));
        assert!(matches!(
            metadata.get("nikon.object.1.serial_number"),
            Some(MetadataValue::String(value)) if value == "TI2-42"
        ));
        assert!(matches!(
            metadata.get("nikon.object.2.x"),
            Some(MetadataValue::Float(value)) if (*value - 12.5).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("nikon.object.2.y"),
            Some(MetadataValue::Float(value)) if (*value + 3.25).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("nikon.object.3.text"),
            Some(MetadataValue::String(value)) if value == "nucleus"
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_elements_tiff_preserves_bounded_nested_object_scalars() {
        let path = temp_path("nested_object_scalars");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIS-Elements>
  <Experiment name="Deeper run" operator="Ada">
    <OpticalConfig id="OC1">
      <Objective name="Plan Apo">
        <Magnification>40</Magnification>
        <NumericAperture value="0.95"/>
      </Objective>
      <Detector serialNumber="CAM-9">
        <Gain>1.25</Gain>
      </Detector>
    </OpticalConfig>
  </Experiment>
</NIS-Elements>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.hierarchy.node_count"),
            Some(MetadataValue::Int(7))
        ));
        assert!(matches!(
            metadata.get("nikon.hierarchy.scalar_count"),
            Some(MetadataValue::Int(8))
        ));
        assert!(matches!(
            metadata.get("nikon.hierarchy.0.path"),
            Some(MetadataValue::String(value)) if value == "experiment"
        ));
        assert!(matches!(
            metadata.get("nikon.hierarchy.0.name"),
            Some(MetadataValue::String(value)) if value == "Deeper run"
        ));
        assert!(matches!(
            metadata.get("nikon.hierarchy.1.path"),
            Some(MetadataValue::String(value)) if value == "experiment.optical_config"
        ));
        assert!(matches!(
            metadata.get("nikon.hierarchy.1.id"),
            Some(MetadataValue::String(value)) if value == "OC1"
        ));
        assert!(matches!(
            metadata.get("nikon.hierarchy.3.path"),
            Some(MetadataValue::String(value))
                if value == "experiment.optical_config.objective.magnification"
        ));
        assert!(matches!(
            metadata.get("nikon.hierarchy.3.text"),
            Some(MetadataValue::Float(value)) if (*value - 40.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("nikon.hierarchy.4.value"),
            Some(MetadataValue::Float(value)) if (*value - 0.95).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("nikon.hierarchy.5.serial_number"),
            Some(MetadataValue::String(value)) if value == "CAM-9"
        ));
        assert!(matches!(
            metadata.get("nikon.hierarchy.6.text"),
            Some(MetadataValue::Float(value)) if (*value - 1.25).abs() < f64::EPSILON
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_elements_tiff_preserves_channel_object_scalar_aliases() {
        let path = temp_path("channel_object_scalars");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIS-Elements>
  <Channel name="DAPI" color="blue" component="0">
    <LutName>Blue LUT</LutName>
  </Channel>
  <ChannelDescription dyeName="FITC" acquisitionMode="Widefield"/>
</NIS-Elements>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.channel_count"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("nikon.channel.0.name"),
            Some(MetadataValue::String(value)) if value == "DAPI"
        ));
        assert!(matches!(
            metadata.get("nikon.object_count"),
            Some(MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("nikon.object.0.type"),
            Some(MetadataValue::String(value)) if value == "channel"
        ));
        assert!(matches!(
            metadata.get("nikon.object.0.color"),
            Some(MetadataValue::String(value)) if value == "blue"
        ));
        assert!(matches!(
            metadata.get("nikon.object.0.component"),
            Some(MetadataValue::Float(value)) if (*value - 0.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("nikon.object.1.type"),
            Some(MetadataValue::String(value)) if value == "channel_description"
        ));
        assert!(matches!(
            metadata.get("nikon.object.1.acquisition_mode"),
            Some(MetadataValue::String(value)) if value == "Widefield"
        ));
        assert!(matches!(
            metadata.get("nikon.hierarchy.1.path"),
            Some(MetadataValue::String(value)) if value == "channel.lut_name"
        ));
        assert!(matches!(
            metadata.get("nikon.hierarchy.1.text"),
            Some(MetadataValue::String(value)) if value == "Blue LUT"
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_elements_tiff_projects_stage_and_roi_to_ome() {
        let path = temp_path("stage_roi_ome");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIS-Elements>
  <Stage x="12.5" y="-3.25" z="7"/>
  <ROI id="roi-1" name="Cell box" x="10" y="20" width="30" height="40" theC="0"/>
  <ROI label="Centroid" centerX="5.5" centerY="6.5" theT="0"/>
</NIS-Elements>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.ome.roi_count"),
            Some(MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("nikon.ome.stage_position_x"),
            Some(MetadataValue::Float(value)) if (*value - 12.5).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("nikon.ome.stage_position_y"),
            Some(MetadataValue::Float(value)) if (*value + 3.25).abs() < f64::EPSILON
        ));

        let ome = reader.ome_metadata().expect("OME metadata");
        assert_eq!(ome.rois.len(), 2);
        assert_eq!(ome.rois[0].id.as_deref(), Some("roi-1"));
        assert_eq!(ome.rois[0].name.as_deref(), Some("Cell box"));
        assert!(matches!(
            ome.rois[0].shapes.as_slice(),
            [crate::common::ome_metadata::OmeShape::Rectangle {
                x,
                y,
                width,
                height,
                the_c: Some(0),
                ..
            }] if (*x - 10.0).abs() < f64::EPSILON
                && (*y - 20.0).abs() < f64::EPSILON
                && (*width - 30.0).abs() < f64::EPSILON
                && (*height - 40.0).abs() < f64::EPSILON
        ));
        assert_eq!(ome.rois[1].name.as_deref(), Some("Centroid"));
        assert!(matches!(
            ome.rois[1].shapes.as_slice(),
            [crate::common::ome_metadata::OmeShape::Point {
                x,
                y,
                the_t: Some(0),
                ..
            }] if (*x - 5.5).abs() < f64::EPSILON
                && (*y - 6.5).abs() < f64::EPSILON
        ));

        let image = &ome.images[0];
        assert_eq!(image.planes.len(), 1);
        assert_eq!(image.planes[0].position_x, Some(12.5));
        assert_eq!(image.planes[0].position_y, Some(-3.25));
        assert_eq!(image.planes[0].position_z, Some(7.0));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_elements_tiff_projects_line_and_ellipse_roi_to_ome() {
        let path = temp_path("line_ellipse_roi_ome");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIS-Elements>
  <ROI id="roi-line" name="Track" type="Line" x1="1" y1="2" x2="8" y2="9" theZ="1"/>
  <ROI id="roi-ellipse" label="Nucleus" shape="Ellipse" centerX="12" centerY="14" radiusX="6" radiusY="4" theC="2"/>
  <ROI id="roi-circle" label="Spot" shape="Circle" x="20" y="25" diameter="10" theT="3"/>
</NIS-Elements>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.ome.roi_count"),
            Some(MetadataValue::Int(3))
        ));

        let ome = reader.ome_metadata().expect("OME metadata");
        assert_eq!(ome.rois.len(), 3);
        assert_eq!(ome.rois[0].name.as_deref(), Some("Track"));
        assert!(matches!(
            ome.rois[0].shapes.as_slice(),
            [crate::common::ome_metadata::OmeShape::Line {
                x1,
                y1,
                x2,
                y2,
                the_z: Some(1),
                ..
            }] if (*x1 - 1.0).abs() < f64::EPSILON
                && (*y1 - 2.0).abs() < f64::EPSILON
                && (*x2 - 8.0).abs() < f64::EPSILON
                && (*y2 - 9.0).abs() < f64::EPSILON
        ));
        assert_eq!(ome.rois[1].name.as_deref(), Some("Nucleus"));
        assert!(matches!(
            ome.rois[1].shapes.as_slice(),
            [crate::common::ome_metadata::OmeShape::Ellipse {
                x,
                y,
                radius_x,
                radius_y,
                the_c: Some(2),
                ..
            }] if (*x - 12.0).abs() < f64::EPSILON
                && (*y - 14.0).abs() < f64::EPSILON
                && (*radius_x - 6.0).abs() < f64::EPSILON
                && (*radius_y - 4.0).abs() < f64::EPSILON
        ));
        assert_eq!(ome.rois[2].name.as_deref(), Some("Spot"));
        assert!(matches!(
            ome.rois[2].shapes.as_slice(),
            [crate::common::ome_metadata::OmeShape::Ellipse {
                x,
                y,
                radius_x,
                radius_y,
                the_t: Some(3),
                ..
            }] if (*x - 20.0).abs() < f64::EPSILON
                && (*y - 25.0).abs() < f64::EPSILON
                && (*radius_x - 5.0).abs() < f64::EPSILON
                && (*radius_y - 5.0).abs() < f64::EPSILON
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_elements_tiff_reports_variant_attribute_diagnostics() {
        let path = temp_path("variant_attribute_diagnostics");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIS-Elements>
  <variant runtype="TimeLoop" opaqueFlag="yes" unsupportedKey="alpha"/>
  <variant mysteryNumber="42"/>
</NIS-Elements>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.variant.unsupported_record_count"),
            Some(MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("nikon.variant.unsupported_attribute_count"),
            Some(MetadataValue::Int(3))
        ));
        assert!(matches!(
            metadata.get("nikon.variant.unparsed_record_count"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("nikon.variant.0.unsupported_attributes"),
            Some(MetadataValue::String(value)) if value == "opaque_flag,unsupported_key"
        ));
        assert!(matches!(
            metadata.get("nikon.variant.0.unsupported_attribute_count"),
            Some(MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("nikon.variant.1.unsupported_attributes"),
            Some(MetadataValue::String(value)) if value == "mystery_number"
        ));
        assert!(matches!(
            metadata.get("nikon.variant.1.unparsed_diagnostic"),
            Some(MetadataValue::String(value))
                if value.contains("no supported objective/camera/acquisition attributes")
        ));
        assert!(metadata.get("nikon.variant.unparsed_diagnostic").is_none());

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_elements_tiff_reports_unparsed_variant_metadata() {
        let path = temp_path("unparsed_variant");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIS-Elements><variant unsupportedKey="opaque"/></NIS-Elements>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.variant_count"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("nikon.variant.unparsed_diagnostic"),
            Some(MetadataValue::String(value))
                if value.contains("no supported objective/camera/acquisition attributes")
        ));

        let _ = std::fs::remove_file(path);
    }

    // -- ND2Handler-faithful translation tests --------------------------------
    //
    // These exercise the real Nikon/ND2 XML element keys (qName -> key, value
    // attr -> value) that ND2Handler.parseKeyAndValue routes into the typed
    // object graph, mirroring NikonElementsTiffReader.

    #[test]
    fn nikon_nd2handler_translates_typed_object_graph() {
        let path = temp_path("nd2handler_object_graph");
        // qName is the key; the `value` attribute is the value. Mirrors the
        // ND2 XML that NikonElementsTiffReader feeds to ND2Handler.
        write_minimal_tiff_with_description(
            &path,
            r#"<NIKON>
  <dCalibration value="0.32"/>
  <dZStep value="1.5"/>
  <wsObjectiveName value="Plan Apo 60x Oil"/>
  <dObjectiveNA value="1.4"/>
  <dRefractIndex1 value="1.515"/>
  <dLampVoltage value="7.5"/>
  <CameraUniqueName value="DS-Qi2"/>
  <dPinholeRadius value="0.5"/>
  <Name value="DAPI"/>
  <Modality value="Widefield"/>
  <Binning value="2x2"/>
  <CameraGain value="2.0"/>
  <ExposureTime value="50"/>
  <Power value="80"/>
  <Name value="FITC"/>
  <ExposureTime value="100"/>
</NIKON>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        // Scalars
        assert!(matches!(
            metadata.get("nikon.nd2.pixel_size_x"),
            Some(MetadataValue::Float(v)) if (v - 0.32).abs() < 1e-9
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.pixel_size_z"),
            Some(MetadataValue::Float(v)) if (v - 1.5).abs() < 1e-9
        ));
        // "Plan Apo 60x Oil" -> mag 60, immersion Oil, correction "PlanApo"
        assert!(matches!(
            metadata.get("nikon.nd2.magnification"),
            Some(MetadataValue::Float(v)) if (v - 60.0).abs() < 1e-9
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.immersion"),
            Some(MetadataValue::String(v)) if v == "Oil"
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.correction"),
            Some(MetadataValue::String(v)) if v == "PlanApo"
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.numerical_aperture"),
            Some(MetadataValue::Float(v)) if (v - 1.4).abs() < 1e-9
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.refractive_index"),
            Some(MetadataValue::Float(v)) if (v - 1.515).abs() < 1e-9
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.voltage"),
            Some(MetadataValue::Float(v)) if (v - 7.5).abs() < 1e-9
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.camera_model"),
            Some(MetadataValue::String(v)) if v == "DS-Qi2"
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.pinhole_size"),
            Some(MetadataValue::Float(v)) if (v - 0.5).abs() < 1e-9
        ));

        // Per-channel lists
        assert!(matches!(
            metadata.get("nikon.nd2.channel.0.name"),
            Some(MetadataValue::String(v)) if v == "DAPI"
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.channel.1.name"),
            Some(MetadataValue::String(v)) if v == "FITC"
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.channel.0.modality"),
            Some(MetadataValue::String(v)) if v == "Widefield"
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.channel.0.binning"),
            Some(MetadataValue::String(v)) if v == "2x2"
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.channel.0.gain"),
            Some(MetadataValue::Float(v)) if (v - 2.0).abs() < 1e-9
        ));
        // ExposureTime is parsed as ms -> seconds (divide by 1000).
        assert!(matches!(
            metadata.get("nikon.nd2.channel.0.exposure_time"),
            Some(MetadataValue::Float(v)) if (v - 0.05).abs() < 1e-9
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.channel.1.exposure_time"),
            Some(MetadataValue::Float(v)) if (v - 0.1).abs() < 1e-9
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.channel.0.power"),
            Some(MetadataValue::Int(80))
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_nd2handler_formats_sdate_like_java_datetools() {
        let path = temp_path("nd2handler_sdate");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIKON>
  <sDate value="05/01/2015  15:14:00"/>
</NIKON>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.nd2.date"),
            Some(MetadataValue::String(v)) if v == "2015-01-05T15:14:00"
        ));
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(
            ome.images[0].acquisition_date.as_deref(),
            Some("2015-01-05T15:14:00")
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_nd2handler_parses_stage_position_lists_and_rois() {
        let path = temp_path("nd2handler_positions_rois");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIKON>
  <dPosX><item_0 value="100.0"/><item_1 value="200.0"/></dPosX>
  <dPosY><item_0 value="10.0"/><item_1 value="20.0"/></dPosY>
  <dPosZ><item_0 value="1.0"/><item_1 value="2.0"/></dPosZ>
  <HorizontalLine X1="0" Y1="5" X2="50" Y2="5"/>
  <Text X="3" Y="4" eval="hello"/>
</NIKON>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("nikon.nd2.position.0.x"),
            Some(MetadataValue::Float(v)) if (v - 100.0).abs() < 1e-9
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.position.1.x"),
            Some(MetadataValue::Float(v)) if (v - 200.0).abs() < 1e-9
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.position.1.z"),
            Some(MetadataValue::Float(v)) if (v - 2.0).abs() < 1e-9
        ));
        assert!(matches!(
            metadata.get("nikon.nd2.roi_count"),
            Some(MetadataValue::Int(2))
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_nd2handler_projects_objective_and_detector_into_ome() {
        let path = temp_path("nd2handler_ome");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIKON>
  <sObjective value="Plan Fluor 40x Air"/>
  <dObjectiveNA value="0.95"/>
  <CameraUniqueName value="ORCA-Flash"/>
  <Name value="Cy5"/>
  <Modality value="Confocal"/>
  <Power value="50"/>
</NIKON>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();
        let ome = reader.ome_metadata().unwrap();

        assert!(!ome.instruments.is_empty());
        let instrument = &ome.instruments[0];
        assert_eq!(instrument.objectives.len(), 1);
        let obj = &instrument.objectives[0];
        assert!(matches!(obj.lens_na, Some(v) if (v - 0.95).abs() < 1e-9));
        assert!(matches!(obj.calibrated_magnification, Some(v) if (v - 40.0).abs() < 1e-9));
        assert_eq!(obj.immersion.as_deref(), Some("Air"));
        assert_eq!(instrument.detectors.len(), 1);
        assert_eq!(instrument.detectors[0].model.as_deref(), Some("ORCA-Flash"));

        // Channel name + acquisition mode projected onto the first channel.
        let image = &ome.images[0];
        assert_eq!(image.channels[0].name.as_deref(), Some("Cy5"));
        assert_eq!(
            image.channels[0].acquisition_mode.as_deref(),
            Some("Confocal")
        );

        let _ = std::fs::remove_file(path);
    }

    /// Embedded `uiCount` dimension loops (Z/T) reshape the single inner series'
    /// Z/T (ND2Handler ZStackLoop/TimeLoop), without multiplying the series count.
    #[test]
    fn nikon_nd2handler_reshapes_z_and_t_from_dimension_loops() {
        let path = temp_path("zt_loops");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIS-Elements>
  <uiCount runtype="NDSetupMultiZLoop ZStackLoop" value="3"/>
  <uiCount runtype="NDSetupMultiTimeLoop TimeLoop" value="5"/>
</NIS-Elements>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();

        // Still a single series, but Z and T came from the loops.
        assert_eq!(reader.series_count(), 1);
        let m = reader.metadata();
        assert_eq!(m.size_z, 3);
        assert_eq!(m.size_t, 5);
        assert_eq!(m.image_count, 3 * m.size_c.max(1) * 5);

        let _ = std::fs::remove_file(path);
    }

    /// An `XYPosLoop` `uiCount` multiplies the inner TIFF series count (one core
    /// per stage position), mirroring ND2Handler's `core.add(ms0)` loop.
    #[test]
    fn nikon_nd2handler_multiplies_series_from_xypos_loop() {
        let path = temp_path("xypos_loop");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIS-Elements>
  <uiCount runtype="NDSetupMultiXYPosLoop XYPosLoop" value="4"/>
</NIS-Elements>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 4);
        for s in 0..reader.series_count() {
            reader.set_series(s).unwrap();
            // Each position reads back the embedded 1x1 plane.
            assert_eq!(reader.open_bytes(0).unwrap().len(), 1);
        }

        let _ = std::fs::remove_file(path);
    }

    /// A `Dimensions` string with an `XY(n)` term multiplies the series count and
    /// reshapes Z/T/C, mirroring ND2Handler.isDimensions reshaping.
    #[test]
    fn nikon_nd2handler_reshapes_from_dimensions_string() {
        let path = temp_path("dimensions");
        write_minimal_tiff_with_description(
            &path,
            r#"<NIS-Elements>
  <Dimensions value="XY(2) x T(7) x Z(3)"/>
</NIS-Elements>"#,
        );

        let mut reader = NikonElementsTiffReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 2);
        for s in 0..reader.series_count() {
            reader.set_series(s).unwrap();
            let m = reader.metadata();
            assert_eq!(m.size_t, 7);
            assert_eq!(m.size_z, 3);
        }

        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod molecular_devices_tiff_tests {
    use super::*;
    use crate::common::metadata::MetadataValue;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bioformats_moldev_tiff_{name}_{}_{}.tif",
            std::process::id(),
            unique
        ))
    }

    fn tiff_entry(tag: u16, typ: u16, count: u32, value: u32) -> [u8; 12] {
        let mut entry = [0u8; 12];
        entry[0..2].copy_from_slice(&tag.to_le_bytes());
        entry[2..4].copy_from_slice(&typ.to_le_bytes());
        entry[4..8].copy_from_slice(&count.to_le_bytes());
        entry[8..12].copy_from_slice(&value.to_le_bytes());
        entry
    }

    fn write_minimal_tiff_with_description(path: &Path, description: &str) {
        let mut desc = description.as_bytes().to_vec();
        desc.push(0);

        let ifd_entry_count = 11u32;
        let ifd_start = 8u32;
        let desc_start = ifd_start + 2 + ifd_entry_count * 12 + 4;
        let pixel_start = desc_start + desc.len() as u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());

        let entries = [
            tiff_entry(256, 4, 1, 1),
            tiff_entry(257, 4, 1, 1),
            tiff_entry(258, 3, 1, 8),
            tiff_entry(259, 3, 1, 1),
            tiff_entry(262, 3, 1, 1),
            tiff_entry(270, 2, desc.len() as u32, desc_start),
            tiff_entry(273, 4, 1, pixel_start),
            tiff_entry(277, 3, 1, 1),
            tiff_entry(278, 4, 1, 1),
            tiff_entry(279, 4, 1, 1),
            tiff_entry(284, 3, 1, 1),
        ];

        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&desc);
        bytes.push(11);

        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn molecular_devices_tiff_projects_shallow_metaxpress_xml_metadata() {
        let path = temp_path("metaxpress_xml");
        write_minimal_tiff_with_description(
            &path,
            r#"<MetaXpress software="MetaXpress" version="6.7">
  <Plate id="Plate-42"/>
  <Well id="B03" row="B" column="3"/>
  <Site id="5"/>
  <Channel name="DAPI" wavelength="405"/>
  <Acquisition exposureTime="12.5"/>
  <Objective magnification="20"/>
</MetaXpress>"#,
        );

        let mut reader = MolecularDevicesTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("moldev.object_count"),
            Some(MetadataValue::Int(7))
        ));
        assert!(matches!(
            metadata.get("moldev.object.scalar_count"),
            Some(MetadataValue::Int(11))
        ));
        assert!(matches!(
            metadata.get("moldev.object.1.type"),
            Some(MetadataValue::String(value)) if value == "plate"
        ));
        assert!(matches!(
            metadata.get("moldev.object.1.id"),
            Some(MetadataValue::String(value)) if value == "Plate-42"
        ));
        assert!(matches!(
            metadata.get("moldev.object.3.id"),
            Some(MetadataValue::Float(value)) if (*value - 5.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("moldev.plateid"),
            Some(MetadataValue::String(value)) if value == "Plate-42"
        ));
        assert!(matches!(
            metadata.get("moldev.wellid"),
            Some(MetadataValue::String(value)) if value == "B03"
        ));
        assert!(matches!(
            metadata.get("moldev.siteid"),
            Some(MetadataValue::Float(value)) if (*value - 5.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("moldev.wavelength"),
            Some(MetadataValue::Float(value)) if (*value - 405.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("moldev.exposuretime"),
            Some(MetadataValue::Float(value)) if (*value - 12.5).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("moldev.objectivemagnification"),
            Some(MetadataValue::Float(value)) if (*value - 20.0).abs() < f64::EPSILON
        ));

        let pixels = reader.open_bytes(0).unwrap();
        assert_eq!(pixels, vec![11]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn molecular_devices_tiff_preserves_nested_metaxpress_object_scalars() {
        let path = temp_path("metaxpress_nested_xml");
        write_minimal_tiff_with_description(
            &path,
            r#"<MetaXpress software="MetaXpress" version="6.7">
  <Plate id="Plate-42">
    <Well id="B03">
      <Site id="5">
        <Acquisition exposureTime="12.5">
          <Channels>
            <Channel name="DAPI" wavelength="405">
              <Objective magnification="20" numericAperture="0.75"/>
            </Channel>
          </Channels>
        </Acquisition>
      </Site>
    </Well>
  </Plate>
</MetaXpress>"#,
        );

        let mut reader = MolecularDevicesTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("moldev.hierarchy.node_count"),
            Some(MetadataValue::Int(6))
        ));
        assert!(matches!(
            metadata.get("moldev.hierarchy.scalar_count"),
            Some(MetadataValue::Int(8))
        ));
        assert!(matches!(
            metadata.get("moldev.hierarchy.0.path"),
            Some(MetadataValue::String(value)) if value == "plate"
        ));
        assert!(matches!(
            metadata.get("moldev.hierarchy.0.id"),
            Some(MetadataValue::String(value)) if value == "Plate-42"
        ));
        assert!(matches!(
            metadata.get("moldev.hierarchy.3.path"),
            Some(MetadataValue::String(value)) if value == "plate.well.site.acquisition"
        ));
        assert!(matches!(
            metadata.get("moldev.hierarchy.3.exposure_time"),
            Some(MetadataValue::Float(value)) if (*value - 12.5).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("moldev.hierarchy.4.path"),
            Some(MetadataValue::String(value))
                if value == "plate.well.site.acquisition.channels.channel"
        ));
        assert!(matches!(
            metadata.get("moldev.hierarchy.4.name"),
            Some(MetadataValue::String(value)) if value == "DAPI"
        ));
        assert!(matches!(
            metadata.get("moldev.hierarchy.4.wavelength"),
            Some(MetadataValue::Float(value)) if (*value - 405.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("moldev.hierarchy.5.path"),
            Some(MetadataValue::String(value))
                if value == "plate.well.site.acquisition.channels.channel.objective"
        ));
        assert!(matches!(
            metadata.get("moldev.hierarchy.5.magnification"),
            Some(MetadataValue::Float(value)) if (*value - 20.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("moldev.hierarchy.5.numeric_aperture"),
            Some(MetadataValue::Float(value)) if (*value - 0.75).abs() < f64::EPSILON
        ));

        let pixels = reader.open_bytes(0).unwrap();
        assert_eq!(pixels, vec![11]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn molecular_devices_tiff_projects_simplepci_section_metadata() {
        let path = temp_path("simplepci_section_metadata");
        write_minimal_tiff_with_description(
            &path,
            "Created by SimplePCI HCImage\n\
[Acquisition]\n\
Exposure Time=12.5\n\
Objective Magnification: 20\n\
[Channel 1]\n\
Channel Name=DAPI\n\
Wavelength=405\n\
Well=A01\n",
        );

        let mut reader = MolecularDevicesTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("simplepci.software"),
            Some(MetadataValue::String(value)) if value == "SimplePCI HCImage"
        ));
        assert!(matches!(
            metadata.get("simplepci.scalar_count"),
            Some(MetadataValue::Int(10))
        ));
        assert!(matches!(
            metadata.get("simplepci.exposure_time"),
            Some(MetadataValue::Float(value)) if (*value - 12.5).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.acquisition.exposure_time"),
            Some(MetadataValue::Float(value)) if (*value - 12.5).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.acquisition.objective_magnification"),
            Some(MetadataValue::Float(value)) if (*value - 20.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.channel_1.channel_name"),
            Some(MetadataValue::String(value)) if value == "DAPI"
        ));
        assert!(matches!(
            metadata.get("simplepci.channel_1.wavelength"),
            Some(MetadataValue::Float(value)) if (*value - 405.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.channel_1.well"),
            Some(MetadataValue::String(value)) if value == "A01"
        ));

        let pixels = reader.open_bytes(0).unwrap();
        assert_eq!(pixels, vec![11]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn molecular_devices_tiff_projects_simplepci_ini_typed_metadata() {
        // Mirrors SimplePCITiffReader.initStandardMetadata() typed extraction.
        let path = temp_path("simplepci_ini_typed_metadata");
        write_minimal_tiff_with_description(
            &path,
            "Created by SimplePCI\n\
Wed, 12 Jun 2024 10:00:00 GMT\n\
[ MICROSCOPE ]\n\
Objective=60x Oil\n\
[ CAPTURE DEVICE ]\n\
Binning=2\n\
Camera Type=ORCA\n\
Camera Name=C11440\n\
Bit Depth=16-bit\n\
[ CAPTURE ]\n\
c_Filter1=DAPI\n\
c_Expos1=15000000\n\
c_Filter2=FITC\n\
c_Expos2=20000000\n\
[ CALIBRATION ]\n\
units=micron\n\
factor=0.32\n",
        );

        let mut reader = MolecularDevicesTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("simplepci.date"),
            Some(MetadataValue::String(value)) if value == "Wed, 12 Jun 2024 10:00:00 GMT"
        ));
        assert!(matches!(
            metadata.get("simplepci.objective_magnification"),
            Some(MetadataValue::Float(value)) if (*value - 60.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.immersion"),
            Some(MetadataValue::String(value)) if value == "Oil"
        ));
        assert!(matches!(
            metadata.get("simplepci.binning"),
            Some(MetadataValue::String(value)) if value == "2x2"
        ));
        assert!(matches!(
            metadata.get("simplepci.camera_type"),
            Some(MetadataValue::String(value)) if value == "ORCA"
        ));
        assert!(matches!(
            metadata.get("simplepci.camera_name"),
            Some(MetadataValue::String(value)) if value == "C11440"
        ));
        assert!(matches!(
            metadata.get("simplepci.bits_per_pixel"),
            Some(MetadataValue::Int(16))
        ));
        assert!(matches!(
            metadata.get("simplepci.exposure_time_1"),
            Some(MetadataValue::Float(value)) if (*value - 15000000.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.exposure_time_2"),
            Some(MetadataValue::Float(value)) if (*value - 20000000.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.calibration_units"),
            Some(MetadataValue::String(value)) if value == "micron"
        ));
        assert!(matches!(
            metadata.get("simplepci.calibration_factor"),
            Some(MetadataValue::Float(value)) if (*value - 0.32).abs() < f64::EPSILON
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn molecular_devices_tiff_projects_simplepci_ini_display_depth_bits() {
        // Display Depth wins over Bit Depth, matching the Java reader.
        let path = temp_path("simplepci_ini_display_depth");
        write_minimal_tiff_with_description(
            &path,
            "Created by SimplePCI HCImage\n\
Thu, 13 Jun 2024 09:00:00 GMT\n\
[ CAPTURE DEVICE ]\n\
Binning=1\n\
Display Depth=12\n\
Bit Depth=16-bit\n",
        );

        let mut reader = MolecularDevicesTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("simplepci.bits_per_pixel"),
            Some(MetadataValue::Int(12))
        ));
        assert!(matches!(
            metadata.get("simplepci.binning"),
            Some(MetadataValue::String(value)) if value == "1x1"
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn molecular_devices_tiff_projects_simplepci_xml_scalar_metadata() {
        let path = temp_path("simplepci_xml_metadata");
        write_minimal_tiff_with_description(
            &path,
            "Created by HCImage\n\
<HCImage>\n\
  <Acquisition ExposureTime=\"15.25\" ObjectiveMagnification=\"40\"/>\n\
  <Channel Name=\"FITC\" Wavelength=\"488\"/>\n\
  <Well>A02</Well>\n\
  <Site>3</Site>\n\
</HCImage>\n",
        );

        let mut reader = MolecularDevicesTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("simplepci.software"),
            Some(MetadataValue::String(value)) if value == "HCImage"
        ));
        assert!(matches!(
            metadata.get("simplepci.xml_scalar_count"),
            Some(MetadataValue::Int(6))
        ));
        assert!(matches!(
            metadata.get("simplepci.xml.acquisition.exposure_time"),
            Some(MetadataValue::Float(value)) if (*value - 15.25).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.xml.acquisition.objective_magnification"),
            Some(MetadataValue::Float(value)) if (*value - 40.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.xml.channel.name"),
            Some(MetadataValue::String(value)) if value == "FITC"
        ));
        assert!(matches!(
            metadata.get("simplepci.xml.channel.wavelength"),
            Some(MetadataValue::Float(value)) if (*value - 488.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.exposure_time"),
            Some(MetadataValue::Float(value)) if (*value - 15.25).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.objective_magnification"),
            Some(MetadataValue::Float(value)) if (*value - 40.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.channel_name"),
            Some(MetadataValue::String(value)) if value == "FITC"
        ));
        assert!(matches!(
            metadata.get("simplepci.wavelength"),
            Some(MetadataValue::Float(value)) if (*value - 488.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.well"),
            Some(MetadataValue::String(value)) if value == "A02"
        ));
        assert!(matches!(
            metadata.get("simplepci.site"),
            Some(MetadataValue::Float(value)) if (*value - 3.0).abs() < f64::EPSILON
        ));

        let pixels = reader.open_bytes(0).unwrap();
        assert_eq!(pixels, vec![11]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn molecular_devices_tiff_preserves_nested_simplepci_xml_object_scalars() {
        let path = temp_path("simplepci_nested_xml_metadata");
        write_minimal_tiff_with_description(
            &path,
            "Created by HCImage\n\
<HCImage>\n\
  <Acquisition RunName=\"Assay 7\">\n\
    <Channel Name=\"TRITC\" Wavelength=\"561\">\n\
      <Objective Magnification=\"60\" NumericAperture=\"1.4\"/>\n\
    </Channel>\n\
    <Camera SerialNumber=\"CAM-17\">\n\
      <Gain>2.5</Gain>\n\
    </Camera>\n\
  </Acquisition>\n\
</HCImage>\n",
        );

        let mut reader = MolecularDevicesTiffReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;

        assert!(matches!(
            metadata.get("simplepci.hierarchy.node_count"),
            Some(MetadataValue::Int(5))
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.scalar_count"),
            Some(MetadataValue::Int(7))
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.0.path"),
            Some(MetadataValue::String(value)) if value == "acquisition"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.0.run_name"),
            Some(MetadataValue::String(value)) if value == "Assay 7"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.1.path"),
            Some(MetadataValue::String(value)) if value == "acquisition.channel"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.1.name"),
            Some(MetadataValue::String(value)) if value == "TRITC"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.1.wavelength"),
            Some(MetadataValue::Float(value)) if (*value - 561.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.2.path"),
            Some(MetadataValue::String(value)) if value == "acquisition.channel.objective"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.2.magnification"),
            Some(MetadataValue::Float(value)) if (*value - 60.0).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.2.numeric_aperture"),
            Some(MetadataValue::Float(value)) if (*value - 1.4).abs() < f64::EPSILON
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.3.serial_number"),
            Some(MetadataValue::String(value)) if value == "CAM-17"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.4.path"),
            Some(MetadataValue::String(value)) if value == "acquisition.camera.gain"
        ));
        assert!(matches!(
            metadata.get("simplepci.hierarchy.4.text"),
            Some(MetadataValue::Float(value)) if (*value - 2.5).abs() < f64::EPSILON
        ));

        let pixels = reader.open_bytes(0).unwrap();
        assert_eq!(pixels, vec![11]);

        let _ = std::fs::remove_file(path);
    }

    fn plate_dir(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "bioformats_metaxpress_plate_{name}_{}_{}",
            std::process::id(),
            unique
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// Synthetic `.htd` -> per well x field series count, with the MetaXpress
    /// `getTiffFiles` naming (`_s<field>` / `_w<channel>`) wiring real companion
    /// TIFFs so the inner reader can deliver pixels.
    #[test]
    fn metaxpress_htd_builds_well_times_field_series() {
        let dir = plate_dir("grid");
        // 1 row x 2 cols of wells, both selected; 1x2 sites with both selected;
        // 2 wavelengths. => fieldCount = 2, wellCount = 2 => 4 series.
        let htd = dir.join("PLATE.HTD");
        std::fs::write(
            &htd,
            "\"XWells\", 2\n\
             \"YWells\", 1\n\
             \"WellsSelection1\", TRUE, TRUE\n\
             \"XSites\", 2\n\
             \"YSites\", 1\n\
             \"SiteSelection1\", TRUE, TRUE\n\
             \"TimePoints\", 1\n\
             \"ZSteps\", 1\n\
             \"Waves\", TRUE\n\
             \"NWavelengths\", 2\n\
             \"WaveName1\", \"DAPI\"\n\
             \"WaveName2\", \"FITC\"\n",
        )
        .unwrap();

        // Companion TIFFs: PLATE_<well>_s<field>_w<channel>.TIF for both wells.
        let base = dir.join("PLATE_").to_string_lossy().into_owned();
        for well in ["A01", "A02"] {
            for field in 1..=2 {
                for channel in 1..=2 {
                    let p = PathBuf::from(format!("{base}{well}_s{field}_w{channel}.TIF"));
                    write_minimal_tiff_with_description(&p, "MetaXpress site");
                }
            }
        }

        let mut reader = MolecularDevicesTiffReader::new();
        assert!(reader.is_this_type_by_name(&htd));
        reader.set_id(&htd).unwrap();

        // fieldCount(2) * wellCount(2) = 4 series.
        assert_eq!(reader.series_count(), 4);
        // Each series: sizeC == NWavelengths, sizeZ == ZSteps, sizeT == TimePoints.
        for s in 0..reader.series_count() {
            reader.set_series(s).unwrap();
            let m = reader.metadata();
            assert_eq!(m.size_c, 2);
            assert_eq!(m.size_z, 1);
            assert_eq!(m.size_t, 1);
            assert_eq!(m.image_count, 2);
        }

        // Wells map to the right labels (series 0,1 -> A01; series 2,3 -> A02).
        reader.set_series(0).unwrap();
        assert!(matches!(
            reader.metadata().series_metadata.get("Well"),
            Some(MetadataValue::String(w)) if w == "A01"
        ));
        reader.set_series(2).unwrap();
        assert!(matches!(
            reader.metadata().series_metadata.get("Well"),
            Some(MetadataValue::String(w)) if w == "A02"
        ));

        // Pixels delegate to the inner TIFF (1x1 8-bit = 1 byte).
        reader.set_series(0).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    /// A `.htd` with a single selected well and `Sites` == FALSE collapses to a
    /// single-field single series.
    #[test]
    fn metaxpress_htd_single_site_single_series() {
        let dir = plate_dir("single");
        let htd = dir.join("ONE.HTD");
        std::fs::write(
            &htd,
            "\"XWells\", 1\n\
             \"YWells\", 1\n\
             \"WellsSelection1\", TRUE\n\
             \"Sites\", FALSE\n\
             \"XSites\", 1\n\
             \"YSites\", 1\n\
             \"TimePoints\", 1\n\
             \"ZSteps\", 1\n\
             \"NWavelengths\", 1\n\
             \"WaveName1\", \"DAPI\"\n",
        )
        .unwrap();

        // With fieldCount == 1 and channels == 1 and Waves absent, the name has
        // no _s/_w/_t suffixes: PLATE_A01.TIF.
        let f = dir.join("ONE_A01.TIF");
        write_minimal_tiff_with_description(&f, "MetaXpress single");

        let mut reader = MolecularDevicesTiffReader::new();
        reader.set_id(&htd).unwrap();
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_c, 1);
        assert_eq!(reader.open_bytes(0).unwrap().len(), 1);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn metaxpress_htd_uses_sorted_flat_directory_fallback() {
        let dir = plate_dir("flat_fallback");
        let htd = dir.join("FLAT.HTD");
        std::fs::write(
            &htd,
            "\"XWells\", 1\n\
             \"YWells\", 1\n\
             \"WellsSelection1\", TRUE\n\
             \"Sites\", FALSE\n\
             \"XSites\", 1\n\
             \"YSites\", 1\n\
             \"TimePoints\", 2\n\
             \"ZSteps\", 1\n\
             \"NWavelengths\", 1\n\
             \"WaveName1\", \"DAPI\"\n",
        )
        .unwrap();

        // Do not create the direct Java guess (`FLAT_A01_t2.TIF`); force the
        // fallback that scans sorted TIFFs whose absolute path starts with base.
        write_minimal_tiff_with_description(&dir.join("FLAT_A01_extra_1.tif"), "MetaXpress flat");
        write_minimal_tiff_with_description(&dir.join("FLAT_A01_extra_2.TIFF"), "MetaXpress flat");
        write_minimal_tiff_with_description(&dir.join("FLAT_A01_thumb.tif"), "MetaXpress thumb");

        let mut reader = MolecularDevicesTiffReader::new();
        reader.set_id(&htd).unwrap();
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![11]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![11]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn metaxpress_htd_uses_timepoint_zstep_fallback_indexing() {
        let dir = plate_dir("subdir_fallback");
        let htd = dir.join("NESTED.HTD");
        std::fs::write(
            &htd,
            "\"XWells\", 1\n\
             \"YWells\", 1\n\
             \"WellsSelection1\", TRUE\n\
             \"Sites\", FALSE\n\
             \"XSites\", 1\n\
             \"YSites\", 1\n\
             \"TimePoints\", 2\n\
             \"ZSteps\", 2\n\
             \"NWavelengths\", 1\n\
             \"WaveName1\", \"DAPI\"\n",
        )
        .unwrap();

        for t in 1..=2 {
            for z in 1..=2 {
                let zdir = dir.join(format!("TimePoint_{t}/ZStep_{z}"));
                std::fs::create_dir_all(&zdir).unwrap();
                write_minimal_tiff_with_description(
                    &zdir.join(format!("NESTED_A01_t{t}_z{z}.tif")),
                    "MetaXpress nested",
                );
            }
        }

        let mut reader = MolecularDevicesTiffReader::new();
        reader.set_id(&htd).unwrap();
        assert_eq!(reader.series_count(), 1);
        assert_eq!(reader.metadata().size_z, 2);
        assert_eq!(reader.metadata().size_t, 2);
        assert_eq!(reader.metadata().image_count, 4);
        for plane in 0..4 {
            assert_eq!(reader.open_bytes(plane).unwrap(), vec![11]);
        }

        let _ = std::fs::remove_dir_all(dir);
    }
}

#[cfg(test)]
mod ndpi_offset64_tests {
    use super::*;
    use crate::common::pixel_type::PixelType;
    use crate::tiff::ifd::{tag, Ifd, IfdValue};
    use std::collections::HashMap;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn push_u16_le(data: &mut Vec<u8>, value: u16) {
        data.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u32_le(data: &mut Vec<u8>, value: u32) {
        data.extend_from_slice(&value.to_le_bytes());
    }

    fn push_ifd_short(data: &mut Vec<u8>, tag: u16, value: u16) {
        push_u16_le(data, tag);
        push_u16_le(data, 3);
        push_u32_le(data, 1);
        push_u16_le(data, value);
        push_u16_le(data, 0);
    }

    fn push_ifd_long(data: &mut Vec<u8>, tag: u16, value: u32) {
        push_u16_le(data, tag);
        push_u16_le(data, 4);
        push_u32_le(data, 1);
        push_u32_le(data, value);
    }

    fn push_ifd_ascii(data: &mut Vec<u8>, tag: u16, count: u32, offset: u32) {
        push_u16_le(data, tag);
        push_u16_le(data, 2);
        push_u32_le(data, count);
        push_u32_le(data, offset);
    }

    fn push_ifd_float(data: &mut Vec<u8>, tag: u16, value: f32) {
        push_u16_le(data, tag);
        push_u16_le(data, 11);
        push_u32_le(data, 1);
        push_u32_le(data, value.to_bits());
    }

    fn temp_ndpi_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bioformats-rs-ndpi-{name}-{}-{unique}.ndpi",
            std::process::id()
        ))
    }

    fn synthetic_ndpi_capture_mode_tiff(capture_mode: u16) -> Vec<u8> {
        struct Spec {
            width: u32,
            height: u32,
            pixels: Vec<u8>,
            first: bool,
        }

        let specs = [
            Spec {
                width: 4,
                height: 4,
                pixels: vec![0; 4 * 4 * 3],
                first: true,
            },
            Spec {
                width: 2,
                height: 2,
                pixels: vec![0; 2 * 2 * 3],
                first: false,
            },
            Spec {
                width: 1,
                height: 1,
                pixels: vec![0; 3],
                first: false,
            },
        ];

        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        push_u16_le(&mut data, 42);
        push_u32_le(&mut data, 8);

        for (i, spec) in specs.iter().enumerate() {
            let metadata = if spec.first {
                Some("Product=synthetic\nNDP.S/N=12345\n")
            } else {
                None
            };
            let entry_count = 10u16 + u16::from(spec.first) * 5;
            let ifd_start = data.len() as u32;
            let after_ifd = ifd_start + 2 + u32::from(entry_count) * 12 + 4;
            let metadata_len = metadata.map(|s| s.len() as u32 + 1).unwrap_or(0);
            let metadata_offset = after_ifd;
            let pixel_offset = metadata_offset + metadata_len;
            let next_ifd = if i + 1 == specs.len() {
                0
            } else {
                pixel_offset + spec.pixels.len() as u32
            };

            push_u16_le(&mut data, entry_count);
            push_ifd_long(&mut data, tag::IMAGE_WIDTH, spec.width);
            push_ifd_long(&mut data, tag::IMAGE_LENGTH, spec.height);
            push_ifd_short(&mut data, tag::BITS_PER_SAMPLE, 8);
            push_ifd_short(&mut data, tag::COMPRESSION, 1);
            push_ifd_short(&mut data, tag::PHOTOMETRIC_INTERPRETATION, 2);
            push_ifd_long(&mut data, tag::STRIP_OFFSETS, pixel_offset);
            push_ifd_short(&mut data, tag::SAMPLES_PER_PIXEL, 3);
            push_ifd_long(&mut data, tag::ROWS_PER_STRIP, spec.height);
            push_ifd_long(&mut data, tag::STRIP_BYTE_COUNTS, spec.pixels.len() as u32);
            push_ifd_short(&mut data, tag::PLANAR_CONFIGURATION, 1);
            if let Some(metadata) = metadata {
                push_ifd_long(&mut data, NDPI_MARKER_TAG, 1);
                push_ifd_float(&mut data, NDPI_X_POSITION, 12.5);
                push_ifd_float(&mut data, NDPI_Y_POSITION, 25.0);
                push_ifd_short(&mut data, NDPI_CAPTURE_MODE, capture_mode);
                push_ifd_ascii(
                    &mut data,
                    NDPI_METADATA_TAG,
                    metadata.len() as u32 + 1,
                    metadata_offset,
                );
            }
            push_u32_le(&mut data, next_ifd);
            if let Some(metadata) = metadata {
                data.extend_from_slice(metadata.as_bytes());
                data.push(0);
            }
            data.extend_from_slice(&spec.pixels);
        }

        data
    }

    #[test]
    fn ndpi_capture_mode_bit_mapping_matches_java_known_modes() {
        assert_eq!(NdpiReader::capture_mode_bits_for(7), Some(12));
        assert_eq!(NdpiReader::capture_mode_bits_for(13), Some(14));
        assert_eq!(NdpiReader::capture_mode_bits_for(14), Some(14));
        assert_eq!(NdpiReader::capture_mode_bits_for(16), Some(14));
        assert_eq!(NdpiReader::capture_mode_bits_for(17), Some(16));
        assert_eq!(NdpiReader::capture_mode_bits_for(18), Some(16));
        assert_eq!(NdpiReader::capture_mode_bits_for(6), None);
    }

    #[test]
    fn ndpi_capture_mode_overrides_pyramid_rgb_metadata() {
        let path = temp_ndpi_path("capture-mode");
        std::fs::write(&path, synthetic_ndpi_capture_mode_tiff(13)).unwrap();

        let mut reader = NdpiReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 3);
        assert_eq!(reader.pyramid_height(), 2);

        reader.set_series(0).unwrap();
        let full = reader.metadata();
        assert_eq!(full.size_c, 1);
        assert!(!full.is_rgb);
        assert_eq!(full.bits_per_pixel, 14);
        assert_eq!(full.pixel_type, PixelType::Uint16);

        reader.set_series(1).unwrap();
        let subres = reader.metadata();
        assert_eq!(subres.size_c, 1);
        assert!(!subres.is_rgb);
        assert_eq!(subres.bits_per_pixel, 14);
        assert_eq!(subres.pixel_type, PixelType::Uint16);

        reader.set_series(2).unwrap();
        let macro_image = reader.metadata();
        assert_eq!(macro_image.size_c, 3);
        assert!(macro_image.is_rgb);
        assert_eq!(macro_image.bits_per_pixel, 8);

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(
            ome.images
                .iter()
                .map(|image| image.planes.len())
                .sum::<usize>(),
            1
        );
        assert_eq!(ome.images[0].planes[0].position_x, Some(12.5));
        assert_eq!(ome.images[0].planes[0].position_y, Some(25.0));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ndpi_pixel_layout_interleaves_rgb_delegate_bytes_for_band_selection() {
        let meta = ImageMetadata {
            is_rgb: true,
            is_interleaved: false,
            size_c: 3,
            bits_per_pixel: 8,
            ..Default::default()
        };
        let planar = vec![1, 2, 10, 20, 100, 110];
        assert_eq!(
            NdpiReader::ndpi_pixel_bytes_for_metadata(&meta, planar),
            vec![1, 10, 100, 2, 20, 110],
            "NDPI RGB delegate bytes are interleaved for Java-style band selection"
        );
    }

    #[test]
    fn ndpi_close_clears_cached_ome_metadata() {
        let mut reader = NdpiReader::new();
        reader
            .ome_images
            .push(crate::common::ome_metadata::OmeImage::default());
        assert!(reader.ome_metadata().is_some());

        reader.close().unwrap();

        assert!(reader.ome_metadata().is_none());
    }

    #[test]
    fn ndpi_multistrip_offset_correction_applies_high_words() {
        // Two tiles; tile 1's offset/bytecount live past 4 GB via high words.
        let mut entries = HashMap::new();
        entries.insert(tag::TILE_OFFSETS, IfdValue::Long(vec![100, 200]));
        entries.insert(tag::TILE_BYTE_COUNTS, IfdValue::Long(vec![50, 60]));
        entries.insert(NDPI_OFFSET_HIGH_BYTES, IfdValue::Long(vec![0, 1]));
        entries.insert(NDPI_BYTE_COUNT_HIGH_BYTES, IfdValue::Long(vec![0, 2]));
        let mut ifd = Ifd { entries };

        assert!(matches!(
            apply_ndpi_multistrip_offset_correction(&mut ifd),
            NdpiOffsetFix::Corrected
        ));
        // Stored back as 64-bit Long8 with high words added.
        assert!(matches!(
            ifd.get(tag::TILE_OFFSETS),
            Some(IfdValue::Long8(_))
        ));
        assert_eq!(
            ifd.get(tag::TILE_OFFSETS).unwrap().as_vec_u64(),
            vec![100, 200 + (1u64 << 32)]
        );
        assert_eq!(
            ifd.get(tag::TILE_BYTE_COUNTS).unwrap().as_vec_u64(),
            vec![50, 60 + (2u64 << 32)]
        );
    }

    #[test]
    fn ndpi_offset_correction_is_noop_without_high_word_tags() {
        let mut entries = HashMap::new();
        entries.insert(tag::TILE_OFFSETS, IfdValue::Long(vec![100, 200]));
        let mut ifd = Ifd { entries };
        assert!(matches!(
            apply_ndpi_multistrip_offset_correction(&mut ifd),
            NdpiOffsetFix::NoHighWords
        ));
        assert_eq!(
            ifd.get(tag::TILE_OFFSETS).unwrap().as_vec_u64(),
            vec![100, 200]
        );
    }

    #[test]
    fn ndpi_single_strip_high_word_is_flagged_unhandled() {
        // Single strip uses Java's Mechanism A (per-entry trailer), not handled.
        let mut entries = HashMap::new();
        entries.insert(tag::STRIP_OFFSETS, IfdValue::Long(vec![100]));
        entries.insert(NDPI_OFFSET_HIGH_BYTES, IfdValue::Long(vec![1]));
        let mut ifd = Ifd { entries };
        assert!(matches!(
            apply_ndpi_multistrip_offset_correction(&mut ifd),
            NdpiOffsetFix::SingleStripUnhandled
        ));
        // Offset left untouched (low 32 bits only).
        assert_eq!(ifd.get(tag::STRIP_OFFSETS).unwrap().as_vec_u64(), vec![100]);
    }
}

#[cfg(test)]
mod leica_scn_tests {
    use super::*;

    #[test]
    fn scn_supplemental_image_does_not_close_active_image() {
        let xml = r#"
<scn xmlns="http://www.leica-microsystems.com/scn/2010/10/01">
  <collection name="c">
    <image name="main">
      <pixels>
        <dimension r="0" z="0" c="0" sizeX="4" sizeY="3" ifd="0"/>
        <dimension r="1" z="0" c="0" sizeX="2" sizeY="1" ifd="1"/>
      </pixels>
      <objective>20</objective>
    </image>
    <supplementalImage type="barcode" ifd="2"/>
  </collection>
</scn>"#;

        let images = LeicaScnReader::parse_scn_xml(xml);

        assert_eq!(images.len(), 2);
        assert_eq!(images[0].name, "main");
        assert_eq!(images[0].size_r, 2);
        assert_eq!(
            images[0].dims.iter().map(|d| d.ifd).collect::<Vec<_>>(),
            vec![0, 1]
        );
        assert_eq!(images[0].obj_mag.as_deref(), Some("20"));

        assert_eq!(images[1].name, "barcode");
        assert_eq!(images[1].size_r, 1);
        assert_eq!(
            images[1].dims.iter().map(|d| d.ifd).collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn scn_close_clears_cached_ome_metadata() {
        let mut reader = LeicaScnReader::new();
        reader
            .ome_images
            .push(crate::common::ome_metadata::OmeImage::default());
        assert!(reader.ome_metadata().is_some());

        reader.close().unwrap();

        assert!(reader.ome_metadata().is_none());
    }
}

#[cfg(test)]
mod improvision_tests {
    use super::*;

    fn tiff_entry(tag: u16, typ: u16, count: u32, value: u32) -> [u8; 12] {
        let mut entry = [0u8; 12];
        entry[0..2].copy_from_slice(&tag.to_le_bytes());
        entry[2..4].copy_from_slice(&typ.to_le_bytes());
        entry[4..8].copy_from_slice(&count.to_le_bytes());
        entry[8..12].copy_from_slice(&value.to_le_bytes());
        entry
    }

    fn bigtiff_entry(tag: u16, typ: u16, count: u64, value: u64) -> [u8; 20] {
        let mut entry = [0u8; 20];
        entry[0..2].copy_from_slice(&tag.to_le_bytes());
        entry[2..4].copy_from_slice(&typ.to_le_bytes());
        entry[4..12].copy_from_slice(&count.to_le_bytes());
        entry[12..20].copy_from_slice(&value.to_le_bytes());
        entry
    }

    // Comments shaped like Improvision/Volocity per-plane ImageDescription,
    // covering the keys Java's ImprovisionTiffReader parses into data fields.
    fn sample_comments() -> Vec<String> {
        vec![
            "Improvision\n\
             XCalibrationMicrons=0.5\n\
             YCalibrationMicrons=0.25\n\
             ZCalibrationMicrons=2.0\n\
             WhiteColour=255,0,0\n\
             ChannelName=DAPI\n\
             ChannelNo=1\n\
             TimeStampMicroSeconds=0"
                .to_string(),
            "Improvision\n\
             WhiteColour=0,255,0\n\
             ChannelName=GFP\n\
             ChannelNo=2\n\
             TimeStampMicroSeconds=1000000"
                .to_string(),
        ]
    }

    #[test]
    fn parses_calibration_colors_names_and_time() {
        let mut r = ImprovisionTiffReader::new();
        // Empty inner -> size_c defaults to 1; pass an explicit size below by
        // exercising parse_comments directly (calibration + colours are
        // independent of inner state).
        let comments = sample_comments();
        r.parse_comments(&comments);

        // Calibration micrometres -> pixel_size_* fields.
        assert_eq!(r.pixel_size_x, 0.5);
        assert_eq!(r.pixel_size_y, 0.25);
        assert_eq!(r.pixel_size_z, 2.0);

        // WhiteColour -> packed RGBA (Java Color(r,g,b,255)).
        assert_eq!(r.channel_colors.len(), 2);
        assert_eq!(r.channel_colors[0], Some((255i32 << 24) | 0xff));
        assert_eq!(r.channel_colors[1], Some((255i32 << 16) | 0xff));

        // pixel_size_t = sum(positive diffs)/size_t. size_t defaults to 1.
        assert_eq!(r.pixel_size_t, 1_000_000);
    }

    #[test]
    fn white_colour_with_too_few_components_is_none() {
        let mut r = ImprovisionTiffReader::new();
        r.parse_comments(&["WhiteColour=128,128".to_string()]);
        assert_eq!(r.channel_colors, vec![None]);
    }

    #[test]
    fn build_ome_surfaces_physical_sizes_and_time_increment() {
        let mut r = ImprovisionTiffReader::new();
        r.pixel_size_x = 0.5;
        r.pixel_size_y = 0.25;
        r.pixel_size_z = 0.0; // non-positive -> omitted
        r.pixel_size_t = 1_000_000;
        r.build_ome();
        // Empty inner -> no series -> build_ome returns without images.
        // So directly assert the helper is a no-op without a series, matching
        // Java guarding on core.get(0,0).
        assert!(r.ome_images.is_empty());
    }

    #[test]
    fn byte_detection_reads_java_improvision_tiff_comment() {
        let path = std::env::temp_dir().join(format!(
            "bioformats_rs_improvision_probe_{}.tif",
            std::process::id()
        ));
        let mut desc = b"Improvision\nTotalChannels=1".to_vec();
        desc.push(0);

        let ifd_entry_count = 11u32;
        let ifd_start = 8u32;
        let desc_start = ifd_start + 2 + ifd_entry_count * 12 + 4;
        let pixel_start = desc_start + desc.len() as u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());
        let entries = [
            tiff_entry(256, 4, 1, 1),
            tiff_entry(257, 4, 1, 1),
            tiff_entry(258, 3, 1, 8),
            tiff_entry(259, 3, 1, 1),
            tiff_entry(262, 3, 1, 1),
            tiff_entry(270, 2, desc.len() as u32, desc_start),
            tiff_entry(273, 4, 1, pixel_start),
            tiff_entry(277, 3, 1, 1),
            tiff_entry(278, 4, 1, 1),
            tiff_entry(279, 4, 1, 1),
            tiff_entry(284, 3, 1, 1),
        ];
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&desc);
        bytes.push(7);

        std::fs::write(&path, &bytes).unwrap();
        let file_bytes = std::fs::read(&path).unwrap();
        assert!(ImprovisionTiffReader::new().is_this_type_by_bytes(&file_bytes));

        let mut plain = file_bytes.clone();
        let marker = plain
            .windows("Improvision".len())
            .position(|window| window == b"Improvision")
            .unwrap();
        plain[marker..marker + "Improvision".len()].copy_from_slice(b"plain-text!");
        assert!(!ImprovisionTiffReader::new().is_this_type_by_bytes(&plain));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn byte_detection_uses_tiff_parser_for_bigtiff_comments() {
        let mut desc = b"Improvision\nTotalChannels=1".to_vec();
        desc.push(0);

        let ifd_start = 16u64;
        let desc_start = ifd_start + 8 + 20 + 8;
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&43u16.to_le_bytes());
        bytes.extend_from_slice(&8u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());
        bytes.extend_from_slice(&1u64.to_le_bytes());
        bytes.extend_from_slice(&bigtiff_entry(
            crate::tiff::ifd::tag::IMAGE_DESCRIPTION,
            2,
            desc.len() as u64,
            desc_start,
        ));
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&desc);

        assert!(ImprovisionTiffReader::new().is_this_type_by_bytes(&bytes));

        let marker = bytes
            .windows("Improvision".len())
            .position(|window| window == b"Improvision")
            .unwrap();
        bytes[marker..marker + "Improvision".len()].copy_from_slice(b"plain-text!");
        assert!(!ImprovisionTiffReader::new().is_this_type_by_bytes(&bytes));
    }

    fn write_one_pixel_improvision_tiff(path: &std::path::Path, desc: &str, pixel: u8) {
        let mut desc = desc.as_bytes().to_vec();
        desc.push(0);

        let ifd_entry_count = 11u32;
        let ifd_start = 8u32;
        let desc_start = ifd_start + 2 + ifd_entry_count * 12 + 4;
        let pixel_start = desc_start + desc.len() as u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());
        let entries = [
            tiff_entry(256, 4, 1, 1),
            tiff_entry(257, 4, 1, 1),
            tiff_entry(258, 3, 1, 8),
            tiff_entry(259, 3, 1, 1),
            tiff_entry(262, 3, 1, 1),
            tiff_entry(270, 2, desc.len() as u32, desc_start),
            tiff_entry(273, 4, 1, pixel_start),
            tiff_entry(277, 3, 1, 1),
            tiff_entry(278, 4, 1, 1),
            tiff_entry(279, 4, 1, 1),
            tiff_entry(284, 3, 1, 1),
        ];
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&desc);
        bytes.push(pixel);

        std::fs::write(path, bytes).unwrap();
    }

    #[test]
    fn multifile_tiff_groups_by_sample_uuid_and_routes_xyzct_to_siblings() {
        let dir = std::env::temp_dir().join(format!(
            "bioformats_rs_improvision_multifile_{}",
            std::process::id()
        ));
        let _ = std::fs::create_dir(&dir);
        let a = dir.join("a.tif");
        let b = dir.join("b.tif");
        let other = dir.join("c.tif");
        let desc = "Improvision\n\
                    MultiFileTIFF=yes\n\
                    SampleUUID=uuid-1\n\
                    TotalZPlanes=2\n\
                    TotalChannels=1\n\
                    TotalTimepoints=1\n\
                    ZPlane=1\n\
                    ChannelNo=1\n\
                    TimepointName=1";
        write_one_pixel_improvision_tiff(&a, desc, 10);
        write_one_pixel_improvision_tiff(&b, desc, 20);
        write_one_pixel_improvision_tiff(
            &other,
            "Improvision\nMultiFileTIFF=yes\nSampleUUID=other",
            99,
        );

        let mut reader = ImprovisionTiffReader::new();
        reader.set_id(&b).unwrap();

        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![10]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![20]);

        let _ = std::fs::remove_file(a);
        let _ = std::fs::remove_file(b);
        let _ = std::fs::remove_file(other);
        let _ = std::fs::remove_dir(dir);
    }
}

#[cfg(test)]
mod targeted_tiff_wrapper_parity_tests {
    use super::*;
    use crate::common::metadata::MetadataValue;
    use crate::tiff::ifd::{Ifd, IfdValue};
    use std::collections::HashMap;

    #[test]
    fn zeiss_apotome_maps_base_zeiss_tag_ids_from_xml() {
        let xml = "<ROOT><Tags><Count>2</Count>\
                   <I0>65602</I0><V0>Grid A</V0>\
                   <I1>65607</I1><V1>1.5</V1>\
                   </Tags></ROOT>";
        let mut vendor = HashMap::new();
        ZeissApotomeTiffReader::parse_zeiss_tiff_tags(xml, &mut vendor);

        assert!(matches!(
            vendor.get("zeiss.apotome_grid_name"),
            Some(MetadataValue::String(value)) if value == "Grid A"
        ));
        assert!(matches!(
            vendor.get("zeiss.apotome_filter_strength"),
            Some(MetadataValue::Float(value)) if (*value - 1.5).abs() < 1e-9
        ));
    }

    #[test]
    fn fluoview_andor_private_tags_use_java_metadata_names() {
        let mut entries = HashMap::new();
        entries.insert(FluoviewReader::TEMPERATURE, IfdValue::Float(vec![-70.5]));
        entries.insert(FluoviewReader::EXPOSURE_TIME, IfdValue::Float(vec![0.25]));
        entries.insert(FluoviewReader::MODEL, IfdValue::Ascii("iXon Ultra".into()));
        let ifd = Ifd { entries };
        let mut vendor = HashMap::new();

        FluoviewReader::parse_andor_tags(&ifd, &mut vendor);

        assert!(matches!(
            vendor.get("fluoview.andor.temperature"),
            Some(MetadataValue::Float(value)) if (*value + 70.5).abs() < 1e-9
        ));
        assert!(matches!(
            vendor.get("fluoview.andor.exposure_time__in_seconds_"),
            Some(MetadataValue::Float(value)) if (*value - 0.25).abs() < 1e-9
        ));
        assert!(matches!(
            vendor.get("fluoview.andor.camera_model"),
            Some(MetadataValue::String(value)) if value == "iXon Ultra"
        ));
    }

    fn write_fluoview_dim(header: &mut [u8], index: usize, name: &str, size: i32, resolution: f64) {
        let offset = 284 + index * 96;
        for (i, b) in name.as_bytes().iter().take(15).enumerate() {
            header[offset + i] = *b;
        }
        header[offset + 16..offset + 20].copy_from_slice(&size.to_le_bytes());
        header[offset + 28..offset + 36].copy_from_slice(&resolution.to_le_bytes());
    }

    #[test]
    fn fluoview_mmheader_derives_java_axis_sizes_and_voxels() {
        let mut header = vec![0u8; 284 + 10 * 96];
        write_fluoview_dim(&mut header, 0, "X", 64, 0.2);
        write_fluoview_dim(&mut header, 1, "Y", 32, 0.3);
        write_fluoview_dim(&mut header, 2, "Z", 3, 1.25);
        write_fluoview_dim(&mut header, 3, "Ch", 2, 1.0);
        write_fluoview_dim(&mut header, 4, "Time", 4, 0.5);

        let mut entries = HashMap::new();
        entries.insert(
            FluoviewReader::MMHEADER,
            IfdValue::Short(header.iter().map(|b| *b as u16).collect()),
        );
        let ifd = Ifd { entries };
        let mut vendor = HashMap::new();
        let mm = FluoviewReader::parse_mmheader(&ifd, &mut vendor).unwrap();

        assert_eq!(mm.size_z, 3);
        assert_eq!(mm.size_c, 2);
        assert_eq!(mm.size_t, 4);
        assert_eq!(mm.dimension_order, "XYZCT");
        assert!(matches!(
            vendor.get("fluoview.voxel_x"),
            Some(MetadataValue::Float(value)) if (*value - 0.2).abs() < 1e-9
        ));
        assert!(matches!(
            vendor.get("fluoview.voxel_z"),
            Some(MetadataValue::Float(value)) if (*value - 1.25).abs() < 1e-9
        ));
    }
}

#[cfg(test)]
mod nd2handler_key_value_tests {
    use super::*;

    // Exercises the parseKeyAndValue branches ported from ND2Handler that the
    // embedded NIS-Elements Nikon XML drives. nImages is left at the default 0
    // unless a test sets it, mirroring ND2Handler's nImages constructor arg.
    fn handler() -> Nd2Handler {
        Nd2Handler::default()
    }

    #[test]
    fn dtimemsec_collects_distinct_timepoints() {
        let mut h = handler();
        h.parse_key_and_value("dTimeMSec", "10.5", None);
        h.parse_key_and_value("dTimeMSec", "10.5", None); // duplicate ignored
        h.parse_key_and_value("dTimeMSec", "21.0", None);
        assert_eq!(h.ts, vec![10, 21]);
        assert_eq!(h.number_of_timepoints, Some(2));
    }

    #[test]
    fn dzpos_collects_distinct_positions() {
        let mut h = handler();
        h.parse_key_and_value("dZPos", "5", None);
        h.parse_key_and_value("dZPos", "5", None); // duplicate ignored
        h.parse_key_and_value("dZPos", "7", None);
        assert_eq!(h.zs, vec![5, 7]);
    }

    #[test]
    fn uibpcsignificant_sets_bits_per_pixel() {
        let mut h = handler();
        h.parse_key_and_value("uiBpcSignificant", "12", None);
        assert_eq!(h.core_bits_per_pixel, Some(12));
    }

    #[test]
    fn virtual_components_sets_size_c_once() {
        let mut h = handler();
        h.parse_key_and_value("VirtualComponents", "3", None);
        assert_eq!(h.core_size_c, 3);
        // dimensionOrder already contains C, so the quirky concat is skipped.
        assert_eq!(h.core_dimension_order, "XYCZT");
        // Second call is ignored because sizeC is no longer 0.
        h.parse_key_and_value("VirtualComponents", "5", None);
        assert_eq!(h.core_size_c, 3);
    }

    #[test]
    fn number_of_picture_planes_sets_size_c() {
        let mut h = handler();
        h.parse_key_and_value("Number of Picture Planes: 4 planes", "4", None);
        assert_eq!(h.core_size_c, 4);
    }

    #[test]
    fn z_stack_loop_gated_by_n_images() {
        let mut h = handler();
        h.n_images = 10;
        h.parse_key_and_value("Z Stack Loop", "5", None);
        assert_eq!(h.core_size_z, 5);
        // Exceeds nImages -> ignored.
        h.parse_key_and_value("Z Stack Loop", "20", None);
        assert_eq!(h.core_size_z, 5);
        // nImages unknown (<=0) -> always applied.
        let mut h2 = handler();
        h2.n_images = 0;
        h2.parse_key_and_value("Z Stack Loop", "99", None);
        assert_eq!(h2.core_size_z, 99);
    }

    #[test]
    fn time_loop_applied_only_once() {
        let mut h = handler();
        h.n_images = 100;
        h.parse_key_and_value("Time Loop", "8", None);
        assert_eq!(h.core_size_t, 8);
        assert!(!h.first_time_loop);
        // firstTimeLoop now false -> ignored.
        h.parse_key_and_value("Time Loop", "16", None);
        assert_eq!(h.core_size_t, 8);
    }

    #[test]
    fn time_loop_ignored_when_exceeds_n_images() {
        let mut h = handler();
        h.n_images = 4;
        h.parse_key_and_value("Time Loop", "8", None);
        assert_eq!(h.core_size_t, 0);
        assert!(h.first_time_loop);
    }

    #[test]
    fn uicount_zstackloop_sets_z_and_order() {
        let mut h = handler();
        h.parse_key_and_value("uiCount", "6", Some("CLxModeBValue|ZStackLoop"));
        assert_eq!(h.core_size_z, 6);
        // Default order "XYCZT" already contains 'Z', so (faithful to Java's
        // indexOf('Z') == -1 guard) the prepend is skipped.
        assert_eq!(h.core_dimension_order, "XYCZT");
        // Only the first ZStackLoop with sizeZ == 0 takes effect.
        h.parse_key_and_value("uiCount", "99", Some("ZStackLoop"));
        assert_eq!(h.core_size_z, 6);
    }

    #[test]
    fn step_key_sets_pixel_size_z() {
        let mut h = handler();
        h.parse_key_and_value("- Step 1.5 um", "ignored value", None);
        assert_eq!(h.pixel_size_z, Some(1.5));
    }

    #[test]
    fn line_key_routes_subkeys() {
        let mut h = handler();
        // "Line" splits on ';' into "k: v" pairs routed back through the parser.
        h.parse_key_and_value("Line", "Modality: Widefield; Name: DAPI", None);
        assert_eq!(h.modality, vec!["Widefield".to_string()]);
        assert_eq!(h.channel_names, vec!["DAPI".to_string()]);
    }

    #[test]
    fn textinfoitem_routes_nested_pairs() {
        let mut h = handler();
        // Colon-separated pairs across CRLF entity-delimited lines, each routed
        // back through parseKeyAndValue.
        let value = "Camera Type: MyCam&#x000a;Modality: Confocal";
        h.parse_key_and_value("TextInfoItem_0", value, None);
        assert_eq!(h.camera_model, Some("MyCam".to_string()));
        assert_eq!(h.modality, vec!["Confocal".to_string()]);
    }
}

#[cfg(test)]
mod nikon_tiff_tests {
    use super::*;
    use crate::common::metadata::MetadataValue;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bioformats_nikon_tiff_{name}_{}_{}.tiff",
            std::process::id(),
            unique
        ))
    }

    fn tiff_entry(tag: u16, typ: u16, count: u32, value: u32) -> [u8; 12] {
        let mut entry = [0u8; 12];
        entry[0..2].copy_from_slice(&tag.to_le_bytes());
        entry[2..4].copy_from_slice(&typ.to_le_bytes());
        entry[4..8].copy_from_slice(&count.to_le_bytes());
        entry[8..12].copy_from_slice(&value.to_le_bytes());
        entry
    }

    /// Build a tiny synthetic single-IFD TIFF carrying SOFTWARE (tag 305) and
    /// ImageDescription (tag 270) values, both stored out-of-line.
    fn write_minimal_tiff_with_software_and_description(
        path: &Path,
        software: &str,
        description: &str,
    ) {
        let mut soft = software.as_bytes().to_vec();
        soft.push(0);
        let mut desc = description.as_bytes().to_vec();
        desc.push(0);

        let ifd_entry_count = 12u32;
        let ifd_start = 8u32;
        let soft_start = ifd_start + 2 + ifd_entry_count * 12 + 4;
        let desc_start = soft_start + soft.len() as u32;
        let pixel_start = desc_start + desc.len() as u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());

        let entries = [
            tiff_entry(256, 4, 1, 1),                          // ImageWidth
            tiff_entry(257, 4, 1, 1),                          // ImageLength
            tiff_entry(258, 3, 1, 8),                          // BitsPerSample
            tiff_entry(259, 3, 1, 1),                          // Compression
            tiff_entry(262, 3, 1, 1),                          // Photometric
            tiff_entry(270, 2, desc.len() as u32, desc_start), // ImageDescription
            tiff_entry(273, 4, 1, pixel_start),                // StripOffsets
            tiff_entry(277, 3, 1, 1),                          // SamplesPerPixel
            tiff_entry(278, 4, 1, 1),                          // RowsPerStrip
            tiff_entry(279, 4, 1, 1),                          // StripByteCounts
            tiff_entry(284, 3, 1, 1),                          // PlanarConfiguration
            tiff_entry(305, 2, soft.len() as u32, soft_start), // Software
        ];

        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        // TIFF requires IFD entries sorted by tag; tag 305 sorts after 284.
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&soft);
        bytes.extend_from_slice(&desc);
        bytes.push(7);

        std::fs::write(path, bytes).unwrap();
    }

    /// EZ-C1 acquisition comment: tab-separated key/value lines exercising the
    /// top-level-key tokenisation that `init_standard_metadata` mirrors.
    // Real EZ-C1 comments are tab-delimited between every token (the key phrase
    // spans the first 2-3 tab fields). Mirrors the format Java's tokenizer expects.
    const EZC1_DESCRIPTION: &str = concat!(
        "document\tlabel\tx\ty\tz\n",
        "document\tscale\t0.25\t0.25\t1.5\n",
        "history\tobjective\tType\tPlanApo\n",
        "history\tobjective\tMagnification\t60\n",
        "history\tobjective\tNA\t1.4\n",
        "history\tobjective\tWorkingDistance\t0.21\n",
        "history\tobjective\tImmersion\tOil\n",
        "history\tgain\t1.5\n",
        "history\tpinhole\t30 um\n",
        "history\tlaser0\twavelength\t488 nm\n",
        "history\tlaser0\tname\tArgon\n",
        "history\tAcquisition\tFilter\tBA515\n",
        "history\tAcquisition\tDichroic\tDM510\n",
        "sensor\ts_params\tLambdaEx\t488\n",
        "sensor\ts_params\tLambdaEm\t520\n",
    );

    const EZC1_MINIMAL_DESCRIPTION: &str = concat!(
        "document\tlabel\tx\ty\tz\n",
        "document\tscale\t1.0\t1.0\t1.0\n",
    );

    #[test]
    fn nikon_tiff_detects_ezc1_software_tag() {
        let path = temp_path("detect");
        write_minimal_tiff_with_software_and_description(&path, "EZ-C1 3.90", EZC1_DESCRIPTION);

        // Whole-file header so the out-of-line SOFTWARE value is reachable.
        let header = std::fs::read(&path).unwrap();
        let reader = NikonTiffReader::new();
        assert!(!reader.is_this_type_by_name(Path::new("sample.tif")));
        assert!(!reader.is_this_type_by_name(Path::new("sample.tiff")));
        assert!(reader.is_this_type_by_bytes(&header));

        // A non-EZ-C1 SOFTWARE tag must be rejected.
        let path2 = temp_path("reject");
        write_minimal_tiff_with_software_and_description(&path2, "ImageJ 1.53", EZC1_DESCRIPTION);
        let header2 = std::fs::read(&path2).unwrap();
        assert!(!reader.is_this_type_by_bytes(&header2));

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&path2).ok();
    }

    #[test]
    fn nikon_tiff_scrapes_ezc1_metadata() {
        let path = temp_path("metadata");
        write_minimal_tiff_with_software_and_description(&path, "EZ-C1 3.90", EZC1_DESCRIPTION);

        let mut reader = NikonTiffReader::new();
        reader.set_id(&path).unwrap();

        let metadata = &reader.metadata().series_metadata;
        // Global key/value pairs (addGlobalMeta) with top-level-key tokenisation.
        assert!(matches!(
            metadata.get("history objective Type"),
            Some(MetadataValue::String(v)) if v == "PlanApo"
        ));
        assert!(matches!(
            metadata.get("history objective Magnification"),
            Some(MetadataValue::String(v)) if v == "60"
        ));

        // OME projection from the typed acquisition fields.
        let ome = reader.ome_metadata().expect("ome metadata");
        let image = &ome.images[0];
        let inst = &ome.instruments[0];

        // physicalSize{X,Y,Z} from "document scale".
        assert_eq!(image.physical_size_x, Some(0.25));
        assert_eq!(image.physical_size_y, Some(0.25));
        assert_eq!(image.physical_size_z, Some(1.5));

        // Objective.
        let obj = &inst.objectives[0];
        assert_eq!(obj.nominal_magnification, Some(60.0));
        assert_eq!(obj.lens_na, Some(1.4));
        assert_eq!(obj.working_distance, Some(0.21));
        assert_eq!(obj.correction.as_deref(), Some("PlanApo"));
        assert_eq!(obj.immersion.as_deref(), Some("Oil"));

        // Laser light source.
        assert_eq!(inst.light_sources.len(), 1);
        assert_eq!(inst.light_sources[0].wavelength, Some(488.0));
        assert_eq!(inst.light_sources[0].model.as_deref(), Some("Argon"));

        // Detector from gain.
        assert_eq!(inst.detectors.len(), 1);
        assert_eq!(inst.detectors[0].gain, Some(1.5));

        // Filter / dichroic.
        assert_eq!(inst.filters[0].model.as_deref(), Some("BA515"));
        assert_eq!(inst.dichroics[0].model.as_deref(), Some("DM510"));

        // Per-channel pinhole / excitation / emission.
        assert!(!image.channels.is_empty());
        assert_eq!(image.channels[0].pinhole_size, Some(30.0));
        assert_eq!(image.channels[0].excitation_wavelength, Some(488.0));
        assert_eq!(image.channels[0].emission_wavelength, Some(520.0));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn nikon_tiff_reopen_resets_java_metadata_accumulators() {
        let path1 = temp_path("metadata_first");
        let path2 = temp_path("metadata_second");
        write_minimal_tiff_with_software_and_description(&path1, "EZ-C1 3.90", EZC1_DESCRIPTION);
        write_minimal_tiff_with_software_and_description(
            &path2,
            "EZ-C1 3.90",
            EZC1_MINIMAL_DESCRIPTION,
        );

        let mut reader = NikonTiffReader::new();
        reader.set_id(&path1).unwrap();
        assert!(!reader.ome_metadata().unwrap().instruments[0]
            .light_sources
            .is_empty());

        reader.set_id(&path2).unwrap();
        let ome = reader.ome_metadata().unwrap();
        let image = &ome.images[0];
        let instrument = &ome.instruments[0];
        assert_eq!(image.physical_size_x, Some(1.0));
        assert_eq!(image.physical_size_y, Some(1.0));
        assert_eq!(image.physical_size_z, Some(1.0));
        assert!(instrument.light_sources.is_empty());
        assert!(instrument.detectors.is_empty());
        assert!(instrument.filters.is_empty());
        assert!(instrument.dichroics.is_empty());

        std::fs::remove_file(&path1).ok();
        std::fs::remove_file(&path2).ok();
    }
}

#[cfg(test)]
mod metamorph_tiff_tests {
    use super::*;
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> std::path::PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bioformats_metamorph_tiff_{name}_{}_{}.tiff",
            std::process::id(),
            unique
        ))
    }

    fn tiff_entry(tag: u16, typ: u16, count: u32, value: u32) -> [u8; 12] {
        let mut entry = [0u8; 12];
        entry[0..2].copy_from_slice(&tag.to_le_bytes());
        entry[2..4].copy_from_slice(&typ.to_le_bytes());
        entry[4..8].copy_from_slice(&count.to_le_bytes());
        entry[8..12].copy_from_slice(&value.to_le_bytes());
        entry
    }

    /// Single-IFD synthetic TIFF carrying an out-of-line ImageDescription.
    fn write_minimal_tiff_with_description(path: &Path, description: &str) {
        let mut desc = description.as_bytes().to_vec();
        desc.push(0);

        let ifd_entry_count = 11u32;
        let ifd_start = 8u32;
        let desc_start = ifd_start + 2 + ifd_entry_count * 12 + 4;
        let pixel_start = desc_start + desc.len() as u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());

        let entries = [
            tiff_entry(256, 4, 1, 1),                          // ImageWidth
            tiff_entry(257, 4, 1, 1),                          // ImageLength
            tiff_entry(258, 3, 1, 8),                          // BitsPerSample
            tiff_entry(259, 3, 1, 1),                          // Compression
            tiff_entry(262, 3, 1, 1),                          // Photometric
            tiff_entry(270, 2, desc.len() as u32, desc_start), // ImageDescription
            tiff_entry(273, 4, 1, pixel_start),                // StripOffsets
            tiff_entry(277, 3, 1, 1),                          // SamplesPerPixel
            tiff_entry(278, 4, 1, 1),                          // RowsPerStrip
            tiff_entry(279, 4, 1, 1),                          // StripByteCounts
            tiff_entry(284, 3, 1, 1),                          // PlanarConfiguration
        ];

        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&desc);
        bytes.push(7);

        std::fs::write(path, bytes).unwrap();
    }

    fn write_minimal_multi_ifd_tiff_with_descriptions(path: &Path, descriptions: &[&str]) {
        let entry_count = 11u32;
        let ifd_size = 2 + entry_count * 12 + 4;
        let ifd_start = 8u32;

        let mut descs: Vec<Vec<u8>> = descriptions
            .iter()
            .map(|description| {
                let mut desc = description.as_bytes().to_vec();
                desc.push(0);
                desc
            })
            .collect();

        let ifd_offsets: Vec<u32> = (0..descs.len())
            .map(|i| ifd_start + i as u32 * ifd_size)
            .collect();
        let mut next_data_offset = ifd_start + descs.len() as u32 * ifd_size;
        let mut desc_offsets = Vec::with_capacity(descs.len());
        for desc in &descs {
            desc_offsets.push(next_data_offset);
            next_data_offset += desc.len() as u32;
        }
        let pixel_offsets: Vec<u32> = (0..descs.len())
            .map(|i| next_data_offset + i as u32)
            .collect();

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());

        for i in 0..descs.len() {
            let entries = [
                tiff_entry(256, 4, 1, 1),                                   // ImageWidth
                tiff_entry(257, 4, 1, 1),                                   // ImageLength
                tiff_entry(258, 3, 1, 8),                                   // BitsPerSample
                tiff_entry(259, 3, 1, 1),                                   // Compression
                tiff_entry(262, 3, 1, 1),                                   // Photometric
                tiff_entry(270, 2, descs[i].len() as u32, desc_offsets[i]), // ImageDescription
                tiff_entry(273, 4, 1, pixel_offsets[i]),                    // StripOffsets
                tiff_entry(277, 3, 1, 1),                                   // SamplesPerPixel
                tiff_entry(278, 4, 1, 1),                                   // RowsPerStrip
                tiff_entry(279, 4, 1, 1),                                   // StripByteCounts
                tiff_entry(284, 3, 1, 1),                                   // PlanarConfiguration
            ];

            bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
            for entry in entries {
                bytes.extend_from_slice(&entry);
            }
            let next_ifd = ifd_offsets.get(i + 1).copied().unwrap_or(0);
            bytes.extend_from_slice(&next_ifd.to_le_bytes());
        }

        for desc in descs.drain(..) {
            bytes.extend_from_slice(&desc);
        }
        for i in 0..descriptions.len() {
            bytes.push(i as u8);
        }

        std::fs::write(path, bytes).unwrap();
    }

    /// A small Metamorph `<MetaData>` comment exercising the `<prop>` /
    /// `<custom-prop>` id/value parsing path (`MetamorphHandler.startElement`).
    const METADATA_COMMENT: &str = concat!(
        "<MetaData>",
        "<prop id=\"image-name\" type=\"string\" value=\"MyImage\"/>",
        "<prop id=\"acquisition-time-local\" type=\"time\" value=\"20100101 12:30:45\"/>",
        "<prop id=\"spatial-calibration-x\" type=\"float\" value=\"0.32\"/>",
        "<prop id=\"spatial-calibration-y\" type=\"float\" value=\"0.32\"/>",
        "<prop id=\"stage-position-x\" type=\"float\" value=\"100.5\"/>",
        "<prop id=\"stage-position-y\" type=\"float\" value=\"200.25\"/>",
        "<prop id=\"stage-label\" type=\"string\" value=\"Position1\"/>",
        "<custom-prop id=\"Temperature\" type=\"float\" value=\"37.0\"/>",
        "<custom-prop id=\"Exposure\" type=\"string\" value=\"50 ms\"/>",
        "<custom-prop id=\"wavelength\" type=\"int\" value=\"525\"/>",
        "<custom-prop id=\"_IllumSetting_\" type=\"string\" value=\"GFP\"/>",
        "</MetaData>",
    );

    const METADATA_COMMENT_PLANE_2: &str = concat!(
        "<MetaData>",
        "<prop id=\"image-name\" type=\"string\" value=\"MyImage\"/>",
        "<prop id=\"acquisition-time-local\" type=\"time\" value=\"20100101 12:30:46\"/>",
        "<prop id=\"spatial-calibration-x\" type=\"float\" value=\"0.32\"/>",
        "<prop id=\"spatial-calibration-y\" type=\"float\" value=\"0.32\"/>",
        "<prop id=\"stage-position-x\" type=\"float\" value=\"100.5\"/>",
        "<prop id=\"stage-position-y\" type=\"float\" value=\"200.25\"/>",
        "<prop id=\"z-position\" type=\"float\" value=\"1.0\"/>",
        "<custom-prop id=\"Exposure\" type=\"string\" value=\"100 ms\"/>",
        "<custom-prop id=\"wavelength\" type=\"int\" value=\"610\"/>",
        "<custom-prop id=\"_IllumSetting_\" type=\"string\" value=\"RFP\"/>",
        "</MetaData>",
    );

    #[test]
    fn metamorph_tiff_detects_metadata_comment() {
        let path = temp_path("detect");
        write_minimal_tiff_with_description(&path, METADATA_COMMENT);

        let header = std::fs::read(&path).unwrap();
        let reader = MetamorphTiffReader::new();
        assert!(!reader.is_this_type_by_name(Path::new("sample.tif")));
        assert!(!reader.is_this_type_by_name(Path::new("sample.tiff")));
        assert!(reader.is_this_type_by_bytes(&header));

        // A plain (non-Metamorph) comment must be rejected.
        let path2 = temp_path("reject");
        write_minimal_tiff_with_description(&path2, "just a normal description");
        let header2 = std::fs::read(&path2).unwrap();
        assert!(!reader.is_this_type_by_bytes(&header2));

        std::fs::remove_file(&path).ok();
        std::fs::remove_file(&path2).ok();
    }

    #[test]
    fn metamorph_tiff_scrapes_plane_and_channel_metadata() {
        let path = temp_path("metadata");
        write_minimal_tiff_with_description(&path, METADATA_COMMENT);

        let mut reader = MetamorphTiffReader::new();
        reader.set_id(&path).unwrap();

        let ome = reader.ome_metadata().expect("ome metadata");
        let image = &ome.images[0];

        // image-name / acquisition-time-local.
        assert_eq!(image.name.as_deref(), Some("MyImage"));
        assert_eq!(image.acquisition_date.as_deref(), Some("20100101 12:30:45"));

        // spatial-calibration-{x,y} -> physical sizes.
        assert_eq!(image.physical_size_x, Some(0.32));
        assert_eq!(image.physical_size_y, Some(0.32));

        // Temperature -> imaging environment.
        assert_eq!(image.imaging_environment_temperature, Some(37.0));

        // Per-plane stage X/Y + exposure (50 ms -> 0.05 s).
        assert!(!image.planes.is_empty());
        let plane = &image.planes[0];
        assert_eq!(plane.position_x, Some(100.5));
        assert_eq!(plane.position_y, Some(200.25));
        assert_eq!(plane.exposure_time, Some(0.05));
        // First plane deltaT against the start timestamp is 0.
        assert_eq!(plane.delta_t, Some(0.0));

        // Channel name (_IllumSetting_) + emission wavelength.
        assert!(!image.channels.is_empty());
        assert_eq!(image.channels[0].name.as_deref(), Some("GFP"));
        assert_eq!(image.channels[0].emission_wavelength, Some(525.0));

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn metamorph_tiff_accumulates_metadata_from_each_ifd_comment() {
        let path = temp_path("multi_ifd_metadata");
        let first = METADATA_COMMENT.replace(
            "<custom-prop id=\"wavelength\" type=\"int\" value=\"525\"/>",
            "<prop id=\"z-position\" type=\"float\" value=\"0.0\"/><custom-prop id=\"wavelength\" type=\"int\" value=\"525\"/>",
        );
        write_minimal_multi_ifd_tiff_with_descriptions(
            &path,
            &[first.as_str(), METADATA_COMMENT_PLANE_2],
        );

        let mut reader = MetamorphTiffReader::new();
        reader.set_id(&path).unwrap();

        let metadata = reader.metadata();
        assert_eq!(metadata.size_c, 2);
        assert_eq!(metadata.image_count, 2);

        let ome = reader.ome_metadata().expect("ome metadata");
        let image = &ome.images[0];
        assert_eq!(image.physical_size_z, Some(1.0));
        assert_eq!(image.channels.len(), 2);
        assert_eq!(image.channels[0].name.as_deref(), Some("GFP"));
        assert_eq!(image.channels[0].emission_wavelength, Some(525.0));
        assert_eq!(image.channels[1].name.as_deref(), Some("RFP"));
        assert_eq!(image.channels[1].emission_wavelength, Some(610.0));
        assert_eq!(image.planes.len(), 2);
        assert_eq!(image.planes[0].exposure_time, Some(0.05));
        assert_eq!(image.planes[1].exposure_time, Some(0.1));

        std::fs::remove_file(&path).ok();
    }
}
