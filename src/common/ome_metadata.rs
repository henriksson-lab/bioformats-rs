//! Structured OME metadata — the Rust equivalent of Java Bio-Formats `IMetadata`.
//!
//! Populated by format readers that carry OME-XML or equivalent rich metadata
//! (CZI, OME-TIFF, OME-XML). Readers without this information return `None`
//! from [`crate::reader::FormatReader::ome_metadata`].

// ─── Public types ────────────────────────────────────────────────────────────

/// Top-level metadata store — one [`OmeImage`] per image series.
#[derive(Debug, Clone, Default)]
pub struct OmeMetadata {
    pub images: Vec<OmeImage>,
    pub instruments: Vec<OmeInstrument>,
    pub experimenters: Vec<OmeExperimenter>,
    pub rois: Vec<OmeROI>,
    pub annotations: Vec<OmeAnnotation>,
}

/// Metadata for one image series.
#[derive(Debug, Clone, Default)]
pub struct OmeImage {
    pub name: Option<String>,
    pub description: Option<String>,
    /// Physical pixel size in X (micrometres).
    pub physical_size_x: Option<f64>,
    /// Physical pixel size in Y (micrometres).
    pub physical_size_y: Option<f64>,
    /// Physical pixel size in Z / z-step (micrometres).
    pub physical_size_z: Option<f64>,
    /// Time between frames (seconds).
    pub time_increment: Option<f64>,
    pub channels: Vec<OmeChannel>,
    pub planes: Vec<OmePlane>,
    /// Reference to an instrument (index into `OmeMetadata::instruments`).
    pub instrument_ref: Option<usize>,
    /// Reference to an objective (index into the instrument's objectives).
    pub objective_ref: Option<usize>,
    /// Per-channel light paths.
    pub light_paths: Vec<OmeLightPath>,
}

/// Per-channel metadata.
#[derive(Debug, Clone, Default)]
pub struct OmeChannel {
    pub name: Option<String>,
    /// Samples (components) per pixel — 1 for greyscale, 3 for RGB.
    pub samples_per_pixel: u32,
    /// Packed RGBA colour as stored in OME-XML (may be negative due to sign).
    pub color: Option<i32>,
    /// Emission wavelength (nm).
    pub emission_wavelength: Option<f64>,
    /// Excitation wavelength (nm).
    pub excitation_wavelength: Option<f64>,
}

/// Instrument metadata (microscope, objectives, detectors, light sources).
#[derive(Debug, Clone, Default)]
pub struct OmeInstrument {
    pub id: Option<String>,
    pub microscope_model: Option<String>,
    pub microscope_manufacturer: Option<String>,
    pub objectives: Vec<OmeObjective>,
    pub detectors: Vec<OmeDetector>,
    pub light_sources: Vec<OmeLightSource>,
    pub filters: Vec<OmeFilter>,
    pub dichroics: Vec<OmeDichroic>,
}

/// Objective lens metadata.
#[derive(Debug, Clone, Default)]
pub struct OmeObjective {
    pub id: Option<String>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    /// Nominal magnification (e.g. 40.0 for 40×).
    pub nominal_magnification: Option<f64>,
    /// Calibrated magnification.
    pub calibrated_magnification: Option<f64>,
    /// Numerical aperture.
    pub lens_na: Option<f64>,
    /// Immersion medium (e.g. "Oil", "Water", "Air").
    pub immersion: Option<String>,
    /// Correction type (e.g. "PlanApo", "PlanFluor").
    pub correction: Option<String>,
    /// Working distance (micrometres).
    pub working_distance: Option<f64>,
}

/// Detector metadata.
#[derive(Debug, Clone, Default)]
pub struct OmeDetector {
    pub id: Option<String>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    /// Detector type (e.g. "CCD", "PMT", "EMCCD", "sCMOS").
    pub detector_type: Option<String>,
    pub gain: Option<f64>,
    pub offset: Option<f64>,
}

/// Light source metadata.
#[derive(Debug, Clone, Default)]
pub struct OmeLightSource {
    pub id: Option<String>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    /// Light source type (e.g. "Laser", "Arc", "LED", "Filament").
    pub light_source_type: Option<String>,
    /// Power (milliwatts).
    pub power: Option<f64>,
}

/// Optical filter metadata.
#[derive(Debug, Clone, Default)]
pub struct OmeFilter {
    pub id: Option<String>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    pub filter_type: Option<String>,
    /// Wavelength range: cut-in (nm).
    pub cut_in: Option<f64>,
    /// Wavelength range: cut-out (nm).
    pub cut_out: Option<f64>,
}

/// Dichroic mirror metadata.
#[derive(Debug, Clone, Default)]
pub struct OmeDichroic {
    pub id: Option<String>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
}

/// Light path: the combination of filters and dichroics used for a channel.
#[derive(Debug, Clone, Default)]
pub struct OmeLightPath {
    pub excitation_filter_ids: Vec<String>,
    pub dichroic_id: Option<String>,
    pub emission_filter_ids: Vec<String>,
}

/// Region of interest.
#[derive(Debug, Clone, Default)]
pub struct OmeROI {
    pub id: Option<String>,
    pub name: Option<String>,
    pub shapes: Vec<OmeShape>,
}

/// A single shape within an ROI.
#[derive(Debug, Clone)]
pub enum OmeShape {
    Rectangle { x: f64, y: f64, width: f64, height: f64, the_z: Option<u32>, the_t: Option<u32>, the_c: Option<u32> },
    Ellipse { x: f64, y: f64, radius_x: f64, radius_y: f64, the_z: Option<u32>, the_t: Option<u32>, the_c: Option<u32> },
    Point { x: f64, y: f64, the_z: Option<u32>, the_t: Option<u32>, the_c: Option<u32> },
    Line { x1: f64, y1: f64, x2: f64, y2: f64, the_z: Option<u32>, the_t: Option<u32>, the_c: Option<u32> },
    Polygon { points: Vec<(f64, f64)>, the_z: Option<u32>, the_t: Option<u32>, the_c: Option<u32> },
    Polyline { points: Vec<(f64, f64)>, the_z: Option<u32>, the_t: Option<u32>, the_c: Option<u32> },
}

/// Experimenter metadata.
#[derive(Debug, Clone, Default)]
pub struct OmeExperimenter {
    pub id: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub institution: Option<String>,
}

/// Annotation (key-value or structured).
#[derive(Debug, Clone)]
pub enum OmeAnnotation {
    /// Simple key-value string annotation.
    MapAnnotation { id: Option<String>, namespace: Option<String>, values: Vec<(String, String)> },
    /// Comment annotation.
    CommentAnnotation { id: Option<String>, namespace: Option<String>, value: String },
    /// Tag annotation.
    TagAnnotation { id: Option<String>, namespace: Option<String>, value: String },
}

/// Per-plane metadata.
#[derive(Debug, Clone, Default)]
pub struct OmePlane {
    pub the_z: u32,
    pub the_c: u32,
    pub the_t: u32,
    /// Time offset from acquisition start (seconds).
    pub delta_t: Option<f64>,
    /// Exposure / integration time (seconds).
    pub exposure_time: Option<f64>,
    pub position_x: Option<f64>,
    pub position_y: Option<f64>,
    pub position_z: Option<f64>,
}

// ─── Serialisation ───────────────────────────────────────────────────────────

impl OmeMetadata {
    /// Serialize to OME-XML string suitable for embedding in a TIFF ImageDescription tag.
    pub fn to_ome_xml(&self, meta: &crate::metadata::ImageMetadata) -> String {
        use std::fmt::Write;
        let mut xml = String::new();
        let _ = write!(xml, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        let _ = write!(xml, r#"<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.openmicroscopy.org/Schemas/OME/2016-06 http://www.openmicroscopy.org/Schemas/OME/2016-06/ome.xsd">"#);

        for (i, img) in self.images.iter().enumerate() {
            let _ = write!(xml, r#"<Image ID="Image:{i}" Name="{}">"#,
                xml_escape(img.name.as_deref().unwrap_or(&format!("Series {i}"))));

            if let Some(desc) = &img.description {
                let _ = write!(xml, "<Description>{}</Description>", xml_escape(desc));
            }

            // Pixels element
            let dim_order = format!("{:?}", meta.dimension_order);
            let pt_str = ome_pixel_type_str(meta.pixel_type);
            let _ = write!(xml,
                r#"<Pixels ID="Pixels:{i}" DimensionOrder="{dim_order}" Type="{pt_str}" SizeX="{}" SizeY="{}" SizeZ="{}" SizeC="{}" SizeT="{}""#,
                meta.size_x, meta.size_y, meta.size_z, meta.size_c, meta.size_t);

            if let Some(v) = img.physical_size_x {
                let _ = write!(xml, r#" PhysicalSizeX="{v}" PhysicalSizeXUnit="µm""#);
            }
            if let Some(v) = img.physical_size_y {
                let _ = write!(xml, r#" PhysicalSizeY="{v}" PhysicalSizeYUnit="µm""#);
            }
            if let Some(v) = img.physical_size_z {
                let _ = write!(xml, r#" PhysicalSizeZ="{v}" PhysicalSizeZUnit="µm""#);
            }
            if let Some(v) = img.time_increment {
                let _ = write!(xml, r#" TimeIncrement="{v}" TimeIncrementUnit="s""#);
            }
            xml.push('>');

            // Channels
            for (ci, ch) in img.channels.iter().enumerate() {
                let _ = write!(xml, r#"<Channel ID="Channel:{i}:{ci}" SamplesPerPixel="{}""#,
                    ch.samples_per_pixel);
                if let Some(name) = &ch.name {
                    let _ = write!(xml, r#" Name="{}""#, xml_escape(name));
                }
                if let Some(c) = ch.color {
                    let _ = write!(xml, r#" Color="{c}""#);
                }
                if let Some(v) = ch.emission_wavelength {
                    let _ = write!(xml, r#" EmissionWavelength="{v}""#);
                }
                if let Some(v) = ch.excitation_wavelength {
                    let _ = write!(xml, r#" ExcitationWavelength="{v}""#);
                }
                xml.push_str("/>");
            }

            // Planes
            for plane in &img.planes {
                let _ = write!(xml,
                    r#"<Plane TheZ="{}" TheC="{}" TheT="{}""#,
                    plane.the_z, plane.the_c, plane.the_t);
                if let Some(v) = plane.delta_t {
                    let _ = write!(xml, r#" DeltaT="{v}""#);
                }
                if let Some(v) = plane.exposure_time {
                    let _ = write!(xml, r#" ExposureTime="{v}""#);
                }
                if let Some(v) = plane.position_x {
                    let _ = write!(xml, r#" PositionX="{v}""#);
                }
                if let Some(v) = plane.position_y {
                    let _ = write!(xml, r#" PositionY="{v}""#);
                }
                if let Some(v) = plane.position_z {
                    let _ = write!(xml, r#" PositionZ="{v}""#);
                }
                xml.push_str("/>");
            }

            xml.push_str("</Pixels></Image>");
        }

        xml.push_str("</OME>");
        xml
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
        .replace('"', "&quot;").replace('\'', "&apos;")
}

fn ome_pixel_type_str(pt: crate::pixel::PixelType) -> &'static str {
    use crate::pixel::PixelType;
    match pt {
        PixelType::Bit => "bit",
        PixelType::Int8 => "int8",
        PixelType::Uint8 => "uint8",
        PixelType::Int16 => "int16",
        PixelType::Uint16 => "uint16",
        PixelType::Int32 => "int32",
        PixelType::Uint32 => "uint32",
        PixelType::Float32 => "float",
        PixelType::Float64 => "double",
    }
}

// ─── Parsers ──────────────────────────────────────────────────────────────────

impl OmeMetadata {
    /// Build a minimal `OmeMetadata` from any [`crate::metadata::ImageMetadata`].
    ///
    /// Every format reader uses this as a baseline — it captures channel count
    /// and samples-per-pixel, which are always available. Format-specific
    /// `ome_metadata()` overrides enrich this with physical sizes, channel
    /// names, wavelengths, etc.
    pub fn from_image_metadata(meta: &crate::metadata::ImageMetadata) -> Self {
        let spp = if meta.is_rgb { meta.size_c } else { 1 };
        let channels = (0..meta.size_c)
            .map(|_| OmeChannel { samples_per_pixel: spp, ..Default::default() })
            .collect();
        OmeMetadata {
            images: vec![OmeImage { channels, ..Default::default() }],
            ..Default::default()
        }
    }

    /// Parse OME-XML into structured metadata.
    ///
    /// Handles both standalone `.ome` files and OME-XML embedded in TIFF
    /// `ImageDescription` tags.
    pub fn from_ome_xml(xml: &str) -> Self {
        let mut images = Vec::new();
        let lower_xml = xml.to_ascii_lowercase();

        for img_start in all_tag_positions(xml, "Image") {
            let img_tag = start_tag_at(xml, img_start);
            let name = xml_attr(img_tag, "Name");

            let img_end = lower_xml[img_start..].find("</image>")
                .map(|p| p + img_start + "</image>".len())
                .unwrap_or(xml.len());
            let img_xml = &xml[img_start..img_end];
            let img_lower = img_xml.to_ascii_lowercase();

            let description = xml_inner_text(img_xml, "Description");

            let pixels_pos = all_tag_positions(img_xml, "Pixels").into_iter().next();

            let (physical_size_x, physical_size_y, physical_size_z, time_increment) =
                if let Some(p) = pixels_pos {
                    let pt = start_tag_at(img_xml, p);
                    let psx = xml_attr(pt, "PhysicalSizeX").and_then(|s| s.parse::<f64>().ok());
                    let psy = xml_attr(pt, "PhysicalSizeY").and_then(|s| s.parse::<f64>().ok());
                    let psz = xml_attr(pt, "PhysicalSizeZ").and_then(|s| s.parse::<f64>().ok());
                    let ti  = xml_attr(pt, "TimeIncrement").and_then(|s| s.parse::<f64>().ok());
                    let psx_u = xml_attr(pt, "PhysicalSizeXUnit").unwrap_or_else(|| "µm".into());
                    let psy_u = xml_attr(pt, "PhysicalSizeYUnit").unwrap_or_else(|| "µm".into());
                    let psz_u = xml_attr(pt, "PhysicalSizeZUnit").unwrap_or_else(|| "µm".into());
                    let ti_u  = xml_attr(pt, "TimeIncrementUnit").unwrap_or_else(|| "s".into());
                    (
                        psx.map(|v| to_microns(v, &psx_u)),
                        psy.map(|v| to_microns(v, &psy_u)),
                        psz.map(|v| to_microns(v, &psz_u)),
                        ti.map(|v|  to_seconds(v, &ti_u)),
                    )
                } else {
                    (None, None, None, None)
                };

            // Parse channels and planes from within the <Pixels> block.
            let pixels_end = pixels_pos.and_then(|p| {
                img_lower[p..].find("</pixels>").map(|e| p + e + "</pixels>".len())
            }).unwrap_or(img_xml.len());
            let pixels_xml = pixels_pos.map(|p| &img_xml[p..pixels_end]);

            let channels = pixels_xml.map(parse_channels).unwrap_or_default();
            let planes   = pixels_xml.map(parse_planes).unwrap_or_default();

            images.push(OmeImage {
                name, description,
                physical_size_x, physical_size_y, physical_size_z, time_increment,
                channels, planes,
                ..Default::default()
            });
        }

        OmeMetadata { images, ..Default::default() }
    }

    /// Parse Zeiss CZI metadata XML into structured metadata.
    pub fn from_czi_xml(xml: &str) -> Self {
        let image = OmeImage {
            physical_size_x: czi_distance(xml, "X"),
            physical_size_y: czi_distance(xml, "Y"),
            physical_size_z: czi_distance(xml, "Z"),
            channels: czi_channels(xml),
            ..Default::default()
        };
        OmeMetadata { images: vec![image], ..Default::default() }
    }
}

// ─── OME-XML helpers ─────────────────────────────────────────────────────────

fn parse_channels(pixels_xml: &str) -> Vec<OmeChannel> {
    all_tag_positions(pixels_xml, "Channel").into_iter().map(|pos| {
        let tag = start_tag_at(pixels_xml, pos);
        OmeChannel {
            name: xml_attr(tag, "Name"),
            samples_per_pixel: xml_attr(tag, "SamplesPerPixel")
                .and_then(|s| s.parse().ok()).unwrap_or(1),
            color: xml_attr(tag, "Color").and_then(|s| s.parse::<i32>().ok()),
            emission_wavelength:  xml_attr(tag, "EmissionWavelength").and_then(|s| s.parse().ok()),
            excitation_wavelength: xml_attr(tag, "ExcitationWavelength").and_then(|s| s.parse().ok()),
        }
    }).collect()
}

fn parse_planes(pixels_xml: &str) -> Vec<OmePlane> {
    all_tag_positions(pixels_xml, "Plane").into_iter().map(|pos| {
        let tag = start_tag_at(pixels_xml, pos);
        OmePlane {
            the_z: xml_attr(tag, "TheZ").and_then(|s| s.parse().ok()).unwrap_or(0),
            the_c: xml_attr(tag, "TheC").and_then(|s| s.parse().ok()).unwrap_or(0),
            the_t: xml_attr(tag, "TheT").and_then(|s| s.parse().ok()).unwrap_or(0),
            delta_t:       xml_attr(tag, "DeltaT").and_then(|s| s.parse().ok()),
            exposure_time: xml_attr(tag, "ExposureTime").and_then(|s| s.parse().ok()),
            position_x:   xml_attr(tag, "PositionX").and_then(|s| s.parse().ok()),
            position_y:   xml_attr(tag, "PositionY").and_then(|s| s.parse().ok()),
            position_z:   xml_attr(tag, "PositionZ").and_then(|s| s.parse().ok()),
        }
    }).collect()
}

// ─── CZI XML helpers ──────────────────────────────────────────────────────────

/// Extract a physical size from `<Distance Id="axis"><Value>…</Value></Distance>`.
/// CZI stores values in metres; returns micrometres.
fn czi_distance(xml: &str, axis: &str) -> Option<f64> {
    let lower = xml.to_ascii_lowercase();
    let needle = format!("<distance id=\"{}\">", axis.to_ascii_lowercase());
    let start = lower.find(&needle)?;
    let block_end = lower[start..].find("</distance>")
        .map(|p| p + start).unwrap_or(xml.len());
    let metres: f64 = xml_inner_text(&xml[start..block_end], "Value")?.trim().parse().ok()?;
    Some(metres * 1e6) // m → µm
}

/// Extract channel metadata from CZI `<DisplaySetting><Channels>` block.
fn czi_channels(xml: &str) -> Vec<OmeChannel> {
    let lower = xml.to_ascii_lowercase();
    let open = "<channel ";
    let close = "</channel>";
    let mut channels = Vec::new();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find(open) {
        let start = pos + rel;
        let end = lower[start..].find(close)
            .map(|e| start + e + close.len())
            .unwrap_or(xml.len());
        let block = &xml[start..end];
        let tag = start_tag_at(block, 0);
        let name = xml_attr(tag, "Name");
        // CZI colours are like "#FFFFA500" (ARGB hex)
        let color = xml_inner_text(block, "Color").and_then(|s| {
            let hex = s.trim().trim_start_matches('#');
            i64::from_str_radix(hex, 16).ok().map(|v| v as i32)
        });
        let emission    = xml_inner_text(block, "EmissionWavelength").and_then(|s| s.trim().parse().ok());
        let excitation  = xml_inner_text(block, "ExcitationWavelength").and_then(|s| s.trim().parse().ok());
        if name.is_some() || color.is_some() {
            channels.push(OmeChannel {
                name, samples_per_pixel: 1, color,
                emission_wavelength: emission,
                excitation_wavelength: excitation,
            });
        }
        pos = end;
    }
    channels
}

// ─── Low-level XML primitives ─────────────────────────────────────────────────

/// Extract the value of `attr` from an XML start-tag string (case-insensitive).
fn xml_attr(tag_text: &str, attr: &str) -> Option<String> {
    let lower = tag_text.to_ascii_lowercase();
    let needle = format!("{}=", attr.to_ascii_lowercase());
    let pos = lower.find(&needle)?;
    let rest = &tag_text[pos + needle.len()..];
    let quote = rest.chars().next()?;
    if quote == '"' || quote == '\'' {
        let inner = &rest[1..];
        let end = inner.find(quote)?;
        Some(inner[..end].to_string())
    } else {
        let end = rest.find(|c: char| c.is_whitespace() || c == '>' || c == '/')
            .unwrap_or(rest.len());
        Some(rest[..end].to_string())
    }
}

/// Return the start-tag string beginning at `pos` (from `<` up to and including `>`).
fn start_tag_at(xml: &str, pos: usize) -> &str {
    let end = xml[pos..].find('>')
        .map(|p| p + pos + 1)
        .unwrap_or(xml.len());
    &xml[pos..end]
}

/// Find the trimmed text content of the first `<tag>…</tag>` (case-insensitive).
fn xml_inner_text(xml: &str, tag: &str) -> Option<String> {
    let lower = xml.to_ascii_lowercase();
    let tag_lc = tag.to_ascii_lowercase();
    let open  = format!("<{}", tag_lc);
    let close = format!("</{}>", tag_lc);
    let tag_start = lower.find(&open)?;
    let content_start = lower[tag_start..].find('>')? + tag_start + 1;
    let content_end   = lower[content_start..].find(&close)? + content_start;
    Some(xml[content_start..content_end].trim().to_string())
}

/// Return byte positions of every `<tag` occurrence (case-insensitive),
/// being careful not to match longer tag names (e.g. `<Channel` vs `<Channels`).
fn all_tag_positions(xml: &str, tag: &str) -> Vec<usize> {
    let lower = xml.to_ascii_lowercase();
    let open  = format!("<{}", tag.to_ascii_lowercase());
    let open_len = open.len();
    let mut positions = Vec::new();
    let mut pos = 0;
    while let Some(rel) = lower[pos..].find(&open) {
        let abs = pos + rel;
        let after = abs + open_len;
        if after < lower.len() {
            let c = lower.as_bytes()[after];
            // Ensure this is not a longer tag name
            if c == b'>' || c == b'/' || c.is_ascii_whitespace() {
                positions.push(abs);
            }
        }
        pos = abs + 1;
    }
    positions
}

// ─── Unit conversions ─────────────────────────────────────────────────────────

fn to_microns(value: f64, unit: &str) -> f64 {
    match unit {
        "m"  => value * 1e6,
        "mm" => value * 1e3,
        "nm" => value * 1e-3,
        "pm" => value * 1e-6,
        _ => value, // assume µm
    }
}

fn to_seconds(value: f64, unit: &str) -> f64 {
    match unit {
        "ms"        => value * 1e-3,
        "µs" | "us" => value * 1e-6,
        "ns"        => value * 1e-9,
        "min"       => value * 60.0,
        "h"         => value * 3600.0,
        _           => value, // assume seconds
    }
}
