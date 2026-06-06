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

    /// De-interleave a chunky RGB plane into channel-separated layout when the
    /// current series is RGB and flagged non-interleaved (mirrors the SVS path).
    fn separate_channels(&self, buf: Vec<u8>, w: u32, h: u32) -> Vec<u8> {
        let m = self.inner.metadata();
        if !m.is_rgb || m.is_interleaved {
            return buf;
        }
        let channels = m.size_c as usize;
        if channels < 2 {
            return buf;
        }
        let bps = ((m.bits_per_pixel as usize + 7) / 8).max(1);
        let pixels = w as usize * h as usize;
        let expected = pixels * channels * bps;
        if pixels == 0 || buf.len() != expected {
            return buf;
        }
        let mut out = vec![0u8; expected];
        let plane = pixels * bps;
        for i in 0..pixels {
            for c in 0..channels {
                let src = (i * channels + c) * bps;
                let dst = c * plane + i * bps;
                out[dst..dst + bps].copy_from_slice(&buf[src..src + bps]);
            }
        }
        out
    }

    /// Build OME image metadata for each flattened series: name "Series N" and
    /// PhysicalSizeX/Y derived from the IFD resolution tags
    /// (`10000 / XResolution` for ResolutionUnit == cm), mirroring the
    /// FormatTools.getPhysicalSize path Java uses for NDPI.
    fn build_ndpi_ome(&mut self) {
        use crate::common::ome_metadata::{OmeChannel, OmeImage};
        use crate::tiff::ifd::tag;
        let mut images: Vec<OmeImage> = Vec::new();
        let series: Vec<(usize, u32)> = self
            .inner
            .series_list()
            .iter()
            .map(|s| (s.ifd_indices.first().copied().unwrap_or(0), s.metadata.size_c.max(1)))
            .collect();
        for (i, (ifd_idx, channels)) in series.into_iter().enumerate() {
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
            images.push(OmeImage {
                name: Some(format!("Series {}", i + 1)),
                physical_size_x: px,
                physical_size_y: py,
                channels: vec![OmeChannel {
                    samples_per_pixel: channels,
                    ..Default::default()
                }],
                ..Default::default()
            });
        }
        self.ome_images = images;
    }

    /// Build per-series `ImageMetadata` from an IFD, mirroring Java's
    /// per-CoreMetadata population (`NDPIReader.java:582-660`). `size_z` is the
    /// focal-plane count for the pyramid series (1 for extras).
    fn ndpi_plane_meta(
        &self,
        ifd_index: usize,
        little_endian: bool,
        size_z: u32,
    ) -> ImageMetadata {
        let mut meta = ImageMetadata::default();
        if let Some(ifd) = self.inner.ifd(ifd_index) {
            let spp = ifd.samples_per_pixel();
            // Java clamps bits-per-sample up to 8 (NDPIReader.java:558-564).
            let bps = ifd.bits_per_sample().first().copied().unwrap_or(8).max(8);
            let photometric = ifd.photometric();
            let is_rgb =
                spp > 1 || matches!(photometric, crate::tiff::ifd::Photometric::Rgb);
            meta.size_x = ifd.image_width().unwrap_or(0);
            meta.size_y = ifd.image_length().unwrap_or(0);
            meta.size_c = if is_rgb { spp as u32 } else { 1 };
            meta.is_rgb = is_rgb;
            meta.bits_per_pixel = bps as u8;
            let sample_format = ifd
                .get_u16(crate::tiff::ifd::tag::SAMPLE_FORMAT)
                .unwrap_or(1);
            meta.pixel_type = tiff_pixel_type(bps, sample_format);
            meta.is_indexed =
                matches!(photometric, crate::tiff::ifd::Photometric::Palette);
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
                meta.series_metadata
                    .insert("ndpi.magnification".into(), MetadataValue::Float(mag as f64));
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
    fn analyze_large_file_offsets(&mut self, path: &Path, file_len: u64) {
        use crate::common::metadata::MetadataValue;
        self.use_64bit = file_len >= (1u64 << 32);
        if !self.use_64bit {
            return;
        }

        let le = self.inner.is_little_endian();
        // Chain offsets for the raw IFD trailers used by Mechanism A.
        let ifd_offsets = ndpi_ifd_offsets(path, le).unwrap_or_default();

        let ifd_count = self.inner.ifd_count();
        let mut any_high_words = false;
        let mut multistrip_corrected = 0usize;
        let mut single_strip_corrected = 0usize;
        let mut out_of_line_unhandled = false;

        for i in 0..ifd_count {
            // Mechanism A (NDPIReader.java:444-490): single-strip/tile files store
            // the offset inline; its true 64-bit value comes from the per-entry
            // high-word trailer appended after the IFD body. Needs the raw IFD.
            if let Some(&off) = ifd_offsets.get(i) {
                if let Some(ifd) = self.inner.ifd_mut(i) {
                    match apply_ndpi_single_strip_correction(path, off, le, ifd) {
                        Ok(NdpiTrailerFix::Corrected) => {
                            single_strip_corrected += 1;
                            any_high_words = true;
                        }
                        Ok(NdpiTrailerFix::OutOfLineUnhandled) => {
                            out_of_line_unhandled = true;
                            any_high_words = true;
                        }
                        Ok(NdpiTrailerFix::None) | Err(_) => {}
                    }
                }
            }
            // Mechanism B: multi-strip/tile per-element high-word arrays
            // (OFFSET_HIGH_BYTES / BYTE_COUNT_HIGH_BYTES).
            if let Some(ifd) = self.inner.ifd_mut(i) {
                match apply_ndpi_multistrip_offset_correction(ifd) {
                    NdpiOffsetFix::Corrected => {
                        any_high_words = true;
                        multistrip_corrected += 1;
                    }
                    // Single-strip is now handled by Mechanism A above.
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
            m.insert(
                "ndpi.offset64.single_strip_corrected_ifds".into(),
                MetadataValue::Int(single_strip_corrected as i64),
            );
            if out_of_line_unhandled {
                m.insert(
                    "ndpi.offset64.limitation".into(),
                    MetadataValue::String(
                        "An IFD stores its strip/tile offset array out-of-line past \
                         4GB (the array storage itself wraps); re-reading it from the \
                         corrected location is not implemented, so such planes may \
                         read incorrectly. Inline single-strip and in-range multi-tile \
                         offsets are corrected."
                            .into(),
                    ),
                );
            }
        }
    }
}

/// TIFF IFD-type → bytes-per-element (subset needed for offset/bytecount tags).
fn tiff_type_size(type_code: u16) -> u64 {
    match type_code {
        1 | 2 | 6 | 7 => 1, // BYTE, ASCII, SBYTE, UNDEFINED
        3 | 8 => 2,         // SHORT, SSHORT
        4 | 9 | 11 | 13 => 4, // LONG, SLONG, FLOAT, IFD
        5 | 10 | 12 => 8,   // RATIONAL, SRATIONAL, DOUBLE
        16 | 17 | 18 => 8,  // LONG8, SLONG8, IFD8
        _ => 0,
    }
}

/// Walk the classic little-endian TIFF IFD chain and return each IFD's file
/// offset in chain order. NDPI is always classic `II`/42 TIFF, and its IFDs (and
/// the next-IFD pointers) live below 4 GB, so a 32-bit-offset walk is safe even
/// for >4 GB files. The per-IFD high-word trailer is extra data after the body
/// and is ignored by this walk (the next-IFD pointer sits right after the entry
/// table, as in standard TIFF).
fn ndpi_ifd_offsets(path: &Path, le: bool) -> std::io::Result<Vec<u64>> {
    use std::io::{Read, Seek, SeekFrom};
    let rd16 = |b: [u8; 2]| if le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) };
    let rd32 = |b: [u8; 4]| if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) };

    let mut f = std::fs::File::open(path)?;
    let mut hdr = [0u8; 8];
    f.read_exact(&mut hdr)?;
    let mut offset = rd32([hdr[4], hdr[5], hdr[6], hdr[7]]) as u64;

    let mut offsets = Vec::new();
    let mut seen = std::collections::HashSet::new();
    while offset != 0 && seen.insert(offset) {
        offsets.push(offset);
        f.seek(SeekFrom::Start(offset))?;
        let mut cb = [0u8; 2];
        f.read_exact(&mut cb)?;
        let count = rd16(cb) as u64;
        f.seek(SeekFrom::Start(offset + 2 + count * 12))?;
        let mut nb = [0u8; 4];
        f.read_exact(&mut nb)?;
        offset = rd32(nb) as u64;
        if offsets.len() > 100_000 {
            break; // runaway guard
        }
    }
    Ok(offsets)
}

/// Outcome of NDPI Mechanism A (per-IFD high-word trailer) for one IFD.
enum NdpiTrailerFix {
    /// No high-order word applied to a strip/tile offset/bytecount tag.
    None,
    /// An inline single-strip/tile offset/bytecount was corrected.
    Corrected,
    /// A strip/tile offset/bytecount array is stored out-of-line past 4 GB
    /// (its storage offset wraps); re-reading it is not implemented.
    OutOfLineUnhandled,
}

/// NDPI Mechanism A (`NDPIReader.java:444-490`): for >4 GB files each IFD carries
/// a trailer of per-entry high-order 32-bit words after the entry table (+ an
/// 8-byte gap). For single-strip/tile files the STRIP/TILE offset (and byte
/// count) is stored inline, and its true 64-bit value is `inline + (high << 32)`.
/// Re-read the raw IFD trailer and correct those inline values in place, writing
/// them back as 64-bit `Long8` so the core reader seeks past 4 GB correctly.
fn apply_ndpi_single_strip_correction(
    path: &Path,
    ifd_offset: u64,
    le: bool,
    ifd: &mut crate::tiff::ifd::Ifd,
) -> std::io::Result<NdpiTrailerFix> {
    use crate::tiff::ifd::{tag, IfdValue};
    use std::io::{Read, Seek, SeekFrom};

    let rd16 = |b: [u8; 2]| if le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) };
    let rd32 = |b: [u8; 4]| if le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) };

    let mut f = std::fs::File::open(path)?;
    f.seek(SeekFrom::Start(ifd_offset))?;
    let mut b2 = [0u8; 2];
    let mut b4 = [0u8; 4];
    f.read_exact(&mut b2)?;
    let count = rd16(b2) as usize;

    // (tag, is_out_of_line_offset, inline_value)
    let mut entries: Vec<(u16, bool, u32)> = Vec::with_capacity(count);
    for _ in 0..count {
        f.read_exact(&mut b2)?;
        let tag_id = rd16(b2);
        f.read_exact(&mut b2)?;
        let typ = rd16(b2);
        f.read_exact(&mut b4)?;
        let vcount = rd32(b4) as u64;
        f.read_exact(&mut b4)?;
        let val = rd32(b4);
        let n_value_bytes = vcount.saturating_mul(tiff_type_size(typ));
        entries.push((tag_id, n_value_bytes > 4, val));
    }

    // Skip the 8-byte gap (next-IFD pointer + padding), then read one high-order
    // 32-bit word per entry, in tag order.
    f.seek(SeekFrom::Current(8))?;
    let mut highs = Vec::with_capacity(count);
    for _ in 0..count {
        f.read_exact(&mut b4)?;
        highs.push(rd32(b4));
    }

    let mut result = NdpiTrailerFix::None;
    for (idx, &(tag_id, is_offset, val)) in entries.iter().enumerate() {
        let high = highs[idx];
        if high == 0 {
            continue;
        }
        let is_strip_tag = matches!(
            tag_id,
            tag::STRIP_OFFSETS | tag::TILE_OFFSETS | tag::STRIP_BYTE_COUNTS | tag::TILE_BYTE_COUNTS
        );
        if !is_strip_tag {
            continue;
        }
        if is_offset {
            // The offset/bytecount ARRAY is stored out-of-line past 4 GB; the
            // core parser already read it from the wrapped (low) location, so we
            // can't fix it by a simple value rewrite. Flag rather than mis-handle.
            result = NdpiTrailerFix::OutOfLineUnhandled;
            continue;
        }
        let corrected = (val as u64).wrapping_add((high as u64) << 32);
        ifd.entries.insert(tag_id, IfdValue::Long8(vec![corrected]));
        if !matches!(result, NdpiTrailerFix::OutOfLineUnhandled) {
            result = NdpiTrailerFix::Corrected;
        }
    }
    Ok(result)
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
        matches!(ext.as_deref(), Some("ndpi"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.close()?;
        self.inner.set_id(path)?;
        // Regroup the flat NDPI IFD chain into a pyramid series (+ macro/map
        // series) and detect sizeZ, mirroring NDPIReader.initStandardMetadata.
        self.build_ndpi_series();
        // Java's default ImageReader flattens the pyramid: each resolution is its
        // own top-level series. Mirror that so seriesCount matches the reference.
        let _ = self.inner.flatten_resolutions_into_series();
        // Per-series interleaving follows NDPIReader.useTiffParser: a JPEG IFD
        // larger than MAX_SIZE in both dimensions is decoded chunky/interleaved
        // by the custom NDPI service; everything else is read channel-separated.
        self.set_ndpi_interleaving();
        self.build_ndpi_ome();
        // BUG 2: detect / flag the >4 GB offset-reconstruction situation.
        let file_len = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
        self.analyze_large_file_offsets(path, file_len);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.size_z = 1;
        self.pyramid_height = 1;
        self.use_64bit = false;
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
        Ok(self.separate_channels(buf, w, h))
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let buf = self.inner.open_bytes_region(p, x, y, w, h)?;
        Ok(self.separate_channels(buf, w, h))
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
                ome_images.push(OmeImage {
                    name: Some(format!("{} (R{})", img.name, r)),
                    physical_size_x: px,
                    physical_size_y: py,
                    channels: vec![OmeChannel {
                        samples_per_pixel: channels,
                        ..Default::default()
                    }],
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

    /// De-interleave a chunky RGB plane into channel-separated layout when the
    /// current series is RGB and flagged non-interleaved (mirrors the SVS path).
    fn separate_channels(&self, buf: Vec<u8>, w: u32, h: u32) -> Vec<u8> {
        let m = self.inner.metadata();
        if !m.is_rgb || m.is_interleaved {
            return buf;
        }
        let channels = m.size_c as usize;
        if channels < 2 {
            return buf;
        }
        let bps = ((m.bits_per_pixel as usize + 7) / 8).max(1);
        let pixels = w as usize * h as usize;
        let expected = pixels * channels * bps;
        if pixels == 0 || buf.len() != expected {
            return buf;
        }
        let mut out = vec![0u8; expected];
        let plane = pixels * bps;
        for i in 0..pixels {
            for c in 0..channels {
                let src = (i * channels + c) * bps;
                let dst = c * plane + i * bps;
                out[dst..dst + bps].copy_from_slice(&buf[src..src + bps]);
            }
        }
        out
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
        let (w, h) = {
            let m = self.inner.metadata();
            (m.size_x, m.size_y)
        };
        let buf = self.inner.open_bytes(p)?;
        Ok(self.separate_channels(buf, w, h))
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let buf = self.inner.open_bytes_region(p, x, y, w, h)?;
        Ok(self.separate_channels(buf, w, h))
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

    /// Scale factor between the full-resolution image and the resolution that is
    /// currently selected on the inner reader. Mirrors Java `getScale`
    /// (`VentanaReader.java:740-743`): `round(fullX / resX)`.
    fn get_scale(&self) -> i64 {
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
        let bpp_pixel = {
            let m = self.inner.metadata();
            let bytes = (m.bits_per_pixel as usize + 7) / 8;
            let spp = if m.is_rgb { m.size_c as usize } else { 1 };
            bytes * spp
        };
        // Full-resolution tile geometry (Java tileWidth/tileHeight).
        let tw = self.tile_width as i64;
        let th = self.tile_height as i64;

        let scale = self.get_scale();
        // Tile dimensions in the current resolution (Java thisTileWidth/Height).
        let this_tw = self.scale_coordinate(tw, scale);
        let this_th = self.scale_coordinate(th, scale);

        let out_row = w as usize * bpp_pixel;
        let mut out = vec![0u8; out_row * h as usize];
        let req_x0 = x as i64;
        let req_y0 = y as i64;
        let req_x1 = req_x0 + w as i64;
        let req_y1 = req_y0 + h as i64;

        // Snapshot of tiles + areas to avoid borrow conflicts during inner reads.
        let tiles = self.tiles.clone();
        let areas = self.areas.clone();
        for tile in &tiles {
            if tile.real_x < 0 || tile.real_y < 0 {
                continue;
            }
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

            // Read the source tile from the current resolution's TIFF layout. The
            // base tile origin is scaled into the current resolution so the inner
            // reader pulls the matching sub-resolution pixels (Java getSamples,
            // scale==1 vs sub-resolution branches collapse to one region read).
            let src_x = self.scale_coordinate(tile.base_x, scale);
            let src_y = self.scale_coordinate(tile.base_y, scale);
            let src = self.inner.open_bytes_region(
                0,
                src_x as u32,
                src_y as u32,
                this_tw as u32,
                this_th as u32,
            )?;
            let src_row = this_tw as usize * bpp_pixel;
            for row in iy0..iy1 {
                // Source coordinates within the scaled tile (Java realRow / x-x).
                let sy = (row - box_y) as usize;
                let sx = (ix0 - box_x) as usize;
                let copy_len = (ix1 - ix0) as usize * bpp_pixel;
                let s_off = sy * src_row + sx * bpp_pixel;
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
        // Reassemble the stitched image at the current resolution (Java stitches
        // every resolution by scaling AOI/tile coords; VentanaReader.java:240-312).
        if self.reassemble && self.inner.series() == 0 {
            if p != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(p));
            }
            let (rx, ry) = {
                let m = self.inner.metadata();
                (m.size_x, m.size_y)
            };
            return self.assemble_region(0, 0, rx, ry);
        }
        self.inner.open_bytes(p)
    }
    fn open_bytes_region(&mut self, p: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        if self.reassemble && self.inner.series() == 0 {
            if p != 0 {
                return Err(BioFormatsError::PlaneOutOfRange(p));
            }
            let (rx, ry) = {
                let m = self.inner.metadata();
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

#[cfg(test)]
mod ndpi_offset64_tests {
    use super::*;
    use crate::tiff::ifd::{tag, Ifd, IfdValue};
    use std::collections::HashMap;

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

    // ── Mechanism A (single-strip per-IFD high-word trailer) ────────────────
    fn push_u16(v: &mut Vec<u8>, x: u16) {
        v.extend_from_slice(&x.to_le_bytes());
    }
    fn push_u32(v: &mut Vec<u8>, x: u32) {
        v.extend_from_slice(&x.to_le_bytes());
    }

    /// Build a minimal classic little-endian TIFF whose single IFD (at offset 8)
    /// has one inline LONG `STRIP_OFFSETS` entry, followed by NDPI's per-entry
    /// high-word trailer (8-byte gap + one high word).
    fn write_single_strip_ndpi_fixture(
        stem: &str,
        inline_offset: u32,
        high_word: u32,
    ) -> std::path::PathBuf {
        let mut b: Vec<u8> = Vec::new();
        b.extend_from_slice(b"II"); // little-endian
        push_u16(&mut b, 42); // classic TIFF magic
        push_u32(&mut b, 8); // first IFD at offset 8
        assert_eq!(b.len(), 8);
        push_u16(&mut b, 1); // entry count
        push_u16(&mut b, tag::STRIP_OFFSETS); // tag 273
        push_u16(&mut b, 4); // type LONG
        push_u32(&mut b, 1); // value count
        push_u32(&mut b, inline_offset); // inline value (low 32 bits)
        // 8-byte gap (next-IFD pointer = 0 + 4 padding), then the high word.
        push_u32(&mut b, 0);
        push_u32(&mut b, 0);
        push_u32(&mut b, high_word);

        let path = std::env::temp_dir().join(stem);
        std::fs::write(&path, b).unwrap();
        path
    }

    #[test]
    fn ndpi_walks_classic_ifd_chain_offsets() {
        let path = write_single_strip_ndpi_fixture("ndpi_chain.tif", 1000, 0);
        let offsets = ndpi_ifd_offsets(&path, true).unwrap();
        assert_eq!(offsets, vec![8]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ndpi_single_strip_inline_offset_is_corrected_from_trailer() {
        // Inline strip offset 1000 with high word 2 -> 1000 + (2 << 32).
        let path = write_single_strip_ndpi_fixture("ndpi_mech_a.tif", 1000, 2);
        let mut ifd = Ifd {
            entries: {
                let mut m = HashMap::new();
                m.insert(tag::STRIP_OFFSETS, IfdValue::Long(vec![1000]));
                m
            },
        };
        let fix = apply_ndpi_single_strip_correction(&path, 8, true, &mut ifd).unwrap();
        assert!(matches!(fix, NdpiTrailerFix::Corrected));
        assert!(matches!(ifd.get(tag::STRIP_OFFSETS), Some(IfdValue::Long8(_))));
        assert_eq!(
            ifd.get(tag::STRIP_OFFSETS).unwrap().as_vec_u64(),
            vec![1000 + (2u64 << 32)]
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ndpi_single_strip_zero_high_word_is_noop() {
        let path = write_single_strip_ndpi_fixture("ndpi_mech_a_zero.tif", 1000, 0);
        let mut ifd = Ifd {
            entries: HashMap::new(),
        };
        let fix = apply_ndpi_single_strip_correction(&path, 8, true, &mut ifd).unwrap();
        assert!(matches!(fix, NdpiTrailerFix::None));
        let _ = std::fs::remove_file(path);
    }
}
