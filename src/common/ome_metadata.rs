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
    pub plates: Vec<OmePlate>,
    pub screens: Vec<OmeScreen>,
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
    /// Modulo annotation for Z (sub-dimensions within Z).
    pub modulo_z: Option<crate::metadata::ModuloAnnotation>,
    /// Modulo annotation for C (sub-dimensions within C).
    pub modulo_c: Option<crate::metadata::ModuloAnnotation>,
    /// Modulo annotation for T (sub-dimensions within T).
    pub modulo_t: Option<crate::metadata::ModuloAnnotation>,
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

/// High-Content Screening plate metadata.
#[derive(Debug, Clone, Default)]
pub struct OmePlate {
    pub id: Option<String>,
    pub name: Option<String>,
    pub rows: u32,
    pub columns: u32,
    pub wells: Vec<OmeWell>,
}

/// A well in an HCS plate.
#[derive(Debug, Clone, Default)]
pub struct OmeWell {
    pub id: Option<String>,
    pub row: u32,
    pub column: u32,
    pub well_samples: Vec<OmeWellSample>,
}

/// A field/site within a well.
#[derive(Debug, Clone, Default)]
pub struct OmeWellSample {
    pub id: Option<String>,
    pub index: u32,
    /// Reference to an Image (index into OmeMetadata::images).
    pub image_ref: Option<usize>,
    pub position_x: Option<f64>,
    pub position_y: Option<f64>,
}

/// Screen metadata (collection of plates).
#[derive(Debug, Clone, Default)]
pub struct OmeScreen {
    pub id: Option<String>,
    pub name: Option<String>,
    pub protocol_description: Option<String>,
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

        // Instrument elements
        for (ii, inst) in self.instruments.iter().enumerate() {
            let default_inst_id = format!("Instrument:{ii}");
            let inst_id = inst.id.as_deref().unwrap_or(&default_inst_id);
            let _ = write!(xml, r#"<Instrument ID="{}">"#, xml_escape(inst_id));

            if inst.microscope_model.is_some() || inst.microscope_manufacturer.is_some() {
                let _ = write!(xml, "<Microscope");
                if let Some(m) = &inst.microscope_model {
                    let _ = write!(xml, r#" Model="{}""#, xml_escape(m));
                }
                if let Some(m) = &inst.microscope_manufacturer {
                    let _ = write!(xml, r#" Manufacturer="{}""#, xml_escape(m));
                }
                xml.push_str("/>");
            }

            for (oi, obj) in inst.objectives.iter().enumerate() {
                let default_obj_id = format!("Objective:{ii}:{oi}");
                let obj_id = obj.id.as_deref().unwrap_or(&default_obj_id);
                let _ = write!(xml, r#"<Objective ID="{}""#, xml_escape(obj_id));
                if let Some(v) = &obj.model { let _ = write!(xml, r#" Model="{}""#, xml_escape(v)); }
                if let Some(v) = &obj.manufacturer { let _ = write!(xml, r#" Manufacturer="{}""#, xml_escape(v)); }
                if let Some(v) = obj.nominal_magnification { let _ = write!(xml, r#" NominalMagnification="{v}""#); }
                if let Some(v) = obj.calibrated_magnification { let _ = write!(xml, r#" CalibratedMagnification="{v}""#); }
                if let Some(v) = obj.lens_na { let _ = write!(xml, r#" LensNA="{v}""#); }
                if let Some(v) = &obj.immersion { let _ = write!(xml, r#" Immersion="{}""#, xml_escape(v)); }
                if let Some(v) = &obj.correction { let _ = write!(xml, r#" Correction="{}""#, xml_escape(v)); }
                if let Some(v) = obj.working_distance { let _ = write!(xml, r#" WorkingDistance="{v}""#); }
                xml.push_str("/>");
            }

            for (di, det) in inst.detectors.iter().enumerate() {
                let default_det_id = format!("Detector:{ii}:{di}");
                let det_id = det.id.as_deref().unwrap_or(&default_det_id);
                let _ = write!(xml, r#"<Detector ID="{}""#, xml_escape(det_id));
                if let Some(v) = &det.model { let _ = write!(xml, r#" Model="{}""#, xml_escape(v)); }
                if let Some(v) = &det.detector_type { let _ = write!(xml, r#" Type="{}""#, xml_escape(v)); }
                if let Some(v) = det.gain { let _ = write!(xml, r#" Gain="{v}""#); }
                xml.push_str("/>");
            }

            for ls in &inst.light_sources {
                let ls_tag = ls.light_source_type.as_deref().unwrap_or("GenericExcitationSource");
                let ls_id = ls.id.as_deref().unwrap_or("LightSource:0");
                let _ = write!(xml, r#"<{ls_tag} ID="{}""#, xml_escape(ls_id));
                if let Some(v) = &ls.model { let _ = write!(xml, r#" Model="{}""#, xml_escape(v)); }
                if let Some(v) = ls.power { let _ = write!(xml, r#" Power="{v}""#); }
                xml.push_str("/>");
            }

            for fi in &inst.filters {
                let f_id = fi.id.as_deref().unwrap_or("Filter:0");
                let _ = write!(xml, r#"<Filter ID="{}""#, xml_escape(f_id));
                if let Some(v) = &fi.model { let _ = write!(xml, r#" Model="{}""#, xml_escape(v)); }
                if let Some(v) = &fi.filter_type { let _ = write!(xml, r#" Type="{}""#, xml_escape(v)); }
                if let Some(v) = fi.cut_in { let _ = write!(xml, r#" CutIn="{v}""#); }
                if let Some(v) = fi.cut_out { let _ = write!(xml, r#" CutOut="{v}""#); }
                xml.push_str("/>");
            }

            for dc in &inst.dichroics {
                let d_id = dc.id.as_deref().unwrap_or("Dichroic:0");
                let _ = write!(xml, r#"<Dichroic ID="{}""#, xml_escape(d_id));
                if let Some(v) = &dc.model { let _ = write!(xml, r#" Model="{}""#, xml_escape(v)); }
                xml.push_str("/>");
            }

            xml.push_str("</Instrument>");
        }

        for (i, img) in self.images.iter().enumerate() {
            let _ = write!(xml, r#"<Image ID="Image:{i}" Name="{}">"#,
                xml_escape(img.name.as_deref().unwrap_or(&format!("Series {i}"))));

            if let Some(desc) = &img.description {
                let _ = write!(xml, "<Description>{}</Description>", xml_escape(desc));
            }

            // InstrumentRef
            if let Some(inst_idx) = img.instrument_ref {
                if let Some(inst) = self.instruments.get(inst_idx) {
                    let default_id = format!("Instrument:{inst_idx}");
                    let inst_id = inst.id.as_deref().unwrap_or(&default_id);
                    let _ = write!(xml, r#"<InstrumentRef ID="{}"/>"#, xml_escape(inst_id));
                }
            }
            // ObjectiveSettings
            if let (Some(inst_idx), Some(obj_idx)) = (img.instrument_ref, img.objective_ref) {
                if let Some(inst) = self.instruments.get(inst_idx) {
                    if let Some(obj) = inst.objectives.get(obj_idx) {
                        let default_id = format!("Objective:{inst_idx}:{obj_idx}");
                        let obj_id = obj.id.as_deref().unwrap_or(&default_id);
                        let _ = write!(xml, r#"<ObjectiveSettings ID="{}"/>"#, xml_escape(obj_id));
                    }
                }
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

            // Modulo annotations
            if let Some(m) = &img.modulo_z {
                let _ = write!(xml, r#"<ModuloAlongZ Type="{}" Start="{}" Step="{}" End="{}" Unit="{}"/>"#,
                    xml_escape(&m.modulo_type), m.start, m.step, m.end, xml_escape(&m.unit));
            }
            if let Some(m) = &img.modulo_c {
                let _ = write!(xml, r#"<ModuloAlongC Type="{}" Start="{}" Step="{}" End="{}" Unit="{}"/>"#,
                    xml_escape(&m.modulo_type), m.start, m.step, m.end, xml_escape(&m.unit));
            }
            if let Some(m) = &img.modulo_t {
                let _ = write!(xml, r#"<ModuloAlongT Type="{}" Start="{}" Step="{}" End="{}" Unit="{}"/>"#,
                    xml_escape(&m.modulo_type), m.start, m.step, m.end, xml_escape(&m.unit));
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
            let modulo_z = pixels_xml.and_then(|px| parse_modulo(px, "Z"));
            let modulo_c = pixels_xml.and_then(|px| parse_modulo(px, "C"));
            let modulo_t = pixels_xml.and_then(|px| parse_modulo(px, "T"));

            images.push(OmeImage {
                name, description,
                physical_size_x, physical_size_y, physical_size_z, time_increment,
                channels, planes, modulo_z, modulo_c, modulo_t,
                ..Default::default()
            });
        }

        // Parse <Instrument> elements (top-level, siblings of <Image>)
        let instruments = parse_instruments(xml);

        // Resolve InstrumentRef/ObjectiveRef for each image
        for (i, img_start) in all_tag_positions(xml, "Image").into_iter().enumerate() {
            if i >= images.len() { break; }
            let img_end = lower_xml[img_start..].find("</image>")
                .map(|p| p + img_start + "</image>".len())
                .unwrap_or(xml.len());
            let img_xml = &xml[img_start..img_end];

            // <InstrumentRef ID="Instrument:0"/>
            if let Some(pos) = all_tag_positions(img_xml, "InstrumentRef").into_iter().next() {
                let tag = start_tag_at(img_xml, pos);
                if let Some(ref_id) = xml_attr(tag, "ID") {
                    images[i].instrument_ref = instruments.iter().position(|inst| {
                        inst.id.as_deref() == Some(ref_id.as_str())
                    });
                }
            }
            // <ObjectiveSettings ID="Objective:0:0"/>
            if let Some(pos) = all_tag_positions(img_xml, "ObjectiveSettings").into_iter().next() {
                let tag = start_tag_at(img_xml, pos);
                if let Some(ref_id) = xml_attr(tag, "ID") {
                    if let Some(inst_idx) = images[i].instrument_ref {
                        images[i].objective_ref = instruments[inst_idx].objectives.iter()
                            .position(|obj| obj.id.as_deref() == Some(ref_id.as_str()));
                    }
                }
            }
        }

        // Parse <Experimenter> elements (top-level, exclude ExperimenterGroup/ExperimenterRef)
        let experimenters = all_tag_positions(xml, "Experimenter").into_iter()
            .filter(|&pos| {
                let tag = start_tag_at(xml, pos);
                let tag_lower = tag.to_ascii_lowercase();
                !tag_lower.starts_with("<experimentergroup") && !tag_lower.starts_with("<experimenterref")
            })
            .map(|pos| {
                let tag = start_tag_at(xml, pos);
                OmeExperimenter {
                    id: xml_attr(tag, "ID"),
                    first_name: xml_attr(tag, "FirstName"),
                    last_name: xml_attr(tag, "LastName"),
                    email: xml_attr(tag, "Email"),
                    institution: xml_attr(tag, "Institution"),
                }
            })
            .collect();

        // Parse <ROI> elements
        let rois = parse_rois(xml);

        // Parse <StructuredAnnotations> block
        let annotations = parse_structured_annotations(xml);

        OmeMetadata { images, instruments, experimenters, rois, annotations, ..Default::default() }
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

fn parse_modulo(pixels_xml: &str, dim: &str) -> Option<crate::metadata::ModuloAnnotation> {
    let tag_name = format!("ModuloAlong{}", dim);
    let pos = all_tag_positions(pixels_xml, &tag_name).into_iter().next()?;
    let t = start_tag_at(pixels_xml, pos);
    Some(crate::metadata::ModuloAnnotation {
        parent_dimension: dim.to_string(),
        modulo_type: xml_attr(t, "Type").unwrap_or_default(),
        start: xml_attr(t, "Start").and_then(|s| s.parse().ok()).unwrap_or(0.0),
        step: xml_attr(t, "Step").and_then(|s| s.parse().ok()).unwrap_or(1.0),
        end: xml_attr(t, "End").and_then(|s| s.parse().ok()).unwrap_or(0.0),
        unit: xml_attr(t, "Unit").unwrap_or_default(),
        labels: Vec::new(),
    })
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

// ─── Instrument parsing ──────────────────────────────────────────────────────

fn parse_instruments(xml: &str) -> Vec<OmeInstrument> {
    let lower = xml.to_ascii_lowercase();
    all_tag_positions(xml, "Instrument").into_iter().map(|pos| {
        let tag = start_tag_at(xml, pos);
        let id = xml_attr(tag, "ID");

        let inst_end = lower[pos..].find("</instrument>")
            .map(|e| pos + e + "</instrument>".len())
            .unwrap_or(xml.len());
        let inst_xml = &xml[pos..inst_end];

        // Microscope
        let microscope_model = all_tag_positions(inst_xml, "Microscope")
            .into_iter().next()
            .and_then(|p| xml_attr(start_tag_at(inst_xml, p), "Model"));
        let microscope_manufacturer = all_tag_positions(inst_xml, "Microscope")
            .into_iter().next()
            .and_then(|p| xml_attr(start_tag_at(inst_xml, p), "Manufacturer"));

        // Objectives
        let objectives = all_tag_positions(inst_xml, "Objective").into_iter().map(|p| {
            let t = start_tag_at(inst_xml, p);
            OmeObjective {
                id: xml_attr(t, "ID"),
                model: xml_attr(t, "Model"),
                manufacturer: xml_attr(t, "Manufacturer"),
                nominal_magnification: xml_attr(t, "NominalMagnification").and_then(|s| s.parse().ok()),
                calibrated_magnification: xml_attr(t, "CalibratedMagnification").and_then(|s| s.parse().ok()),
                lens_na: xml_attr(t, "LensNA").and_then(|s| s.parse().ok()),
                immersion: xml_attr(t, "Immersion"),
                correction: xml_attr(t, "Correction"),
                working_distance: xml_attr(t, "WorkingDistance").and_then(|s| s.parse().ok()),
            }
        }).collect();

        // Detectors
        let detectors = all_tag_positions(inst_xml, "Detector").into_iter().map(|p| {
            let t = start_tag_at(inst_xml, p);
            OmeDetector {
                id: xml_attr(t, "ID"),
                model: xml_attr(t, "Model"),
                manufacturer: xml_attr(t, "Manufacturer"),
                detector_type: xml_attr(t, "Type"),
                gain: xml_attr(t, "Gain").and_then(|s| s.parse().ok()),
                offset: xml_attr(t, "Offset").and_then(|s| s.parse().ok()),
            }
        }).collect();

        // Light sources (Laser, Arc, Filament, LightEmittingDiode, GenericExcitationSource)
        let mut light_sources = Vec::new();
        for ls_tag in &["Laser", "Arc", "Filament", "LightEmittingDiode", "GenericExcitationSource"] {
            for p in all_tag_positions(inst_xml, ls_tag) {
                let t = start_tag_at(inst_xml, p);
                light_sources.push(OmeLightSource {
                    id: xml_attr(t, "ID"),
                    model: xml_attr(t, "Model"),
                    manufacturer: xml_attr(t, "Manufacturer"),
                    light_source_type: Some(ls_tag.to_string()),
                    power: xml_attr(t, "Power").and_then(|s| s.parse().ok()),
                });
            }
        }

        // Filters
        let filters = all_tag_positions(inst_xml, "Filter").into_iter().map(|p| {
            let t = start_tag_at(inst_xml, p);
            OmeFilter {
                id: xml_attr(t, "ID"),
                model: xml_attr(t, "Model"),
                manufacturer: xml_attr(t, "Manufacturer"),
                filter_type: xml_attr(t, "Type"),
                cut_in: xml_attr(t, "CutIn").and_then(|s| s.parse().ok()),
                cut_out: xml_attr(t, "CutOut").and_then(|s| s.parse().ok()),
            }
        }).collect();

        // Dichroics
        let dichroics = all_tag_positions(inst_xml, "Dichroic").into_iter().map(|p| {
            let t = start_tag_at(inst_xml, p);
            OmeDichroic {
                id: xml_attr(t, "ID"),
                model: xml_attr(t, "Model"),
                manufacturer: xml_attr(t, "Manufacturer"),
            }
        }).collect();

        OmeInstrument {
            id, microscope_model, microscope_manufacturer,
            objectives, detectors, light_sources, filters, dichroics,
        }
    }).collect()
}

// ─── ROI parsing ────────────────────────────────────────────────────────────

fn parse_rois(xml: &str) -> Vec<OmeROI> {
    let lower = xml.to_ascii_lowercase();
    all_tag_positions(xml, "ROI").into_iter().map(|pos| {
        let tag = start_tag_at(xml, pos);
        let id = xml_attr(tag, "ID");
        let name = xml_attr(tag, "Name");

        let roi_end = lower[pos..].find("</roi>")
            .map(|e| pos + e + "</roi>".len())
            .unwrap_or(xml.len());
        let roi_xml = &xml[pos..roi_end];

        // Find the <Union> block containing shapes
        let union_xml = {
            let roi_lower = roi_xml.to_ascii_lowercase();
            let u_start = roi_lower.find("<union");
            let u_end = roi_lower.find("</union>");
            match (u_start, u_end) {
                (Some(s), Some(e)) => &roi_xml[s..e + "</union>".len()],
                _ => roi_xml,
            }
        };

        let mut shapes = Vec::new();

        // Rectangle
        for sp in all_tag_positions(union_xml, "Rectangle") {
            let t = start_tag_at(union_xml, sp);
            let x = xml_attr(t, "X").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let y = xml_attr(t, "Y").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let w = xml_attr(t, "Width").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let h = xml_attr(t, "Height").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
            let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
            let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
            shapes.push(OmeShape::Rectangle { x, y, width: w, height: h, the_z, the_t, the_c });
        }

        // Ellipse
        for sp in all_tag_positions(union_xml, "Ellipse") {
            let t = start_tag_at(union_xml, sp);
            let x = xml_attr(t, "X").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let y = xml_attr(t, "Y").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let rx = xml_attr(t, "RadiusX").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let ry = xml_attr(t, "RadiusY").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
            let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
            let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
            shapes.push(OmeShape::Ellipse { x, y, radius_x: rx, radius_y: ry, the_z, the_t, the_c });
        }

        // Point
        for sp in all_tag_positions(union_xml, "Point") {
            let t = start_tag_at(union_xml, sp);
            let x = xml_attr(t, "X").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let y = xml_attr(t, "Y").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
            let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
            let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
            shapes.push(OmeShape::Point { x, y, the_z, the_t, the_c });
        }

        // Line
        for sp in all_tag_positions(union_xml, "Line") {
            let t = start_tag_at(union_xml, sp);
            let x1 = xml_attr(t, "X1").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let y1 = xml_attr(t, "Y1").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let x2 = xml_attr(t, "X2").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let y2 = xml_attr(t, "Y2").and_then(|s| s.parse().ok()).unwrap_or(0.0);
            let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
            let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
            let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
            shapes.push(OmeShape::Line { x1, y1, x2, y2, the_z, the_t, the_c });
        }

        // Polygon
        for sp in all_tag_positions(union_xml, "Polygon") {
            let t = start_tag_at(union_xml, sp);
            let points = parse_points_attr(xml_attr(t, "Points").as_deref().unwrap_or(""));
            let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
            let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
            let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
            shapes.push(OmeShape::Polygon { points, the_z, the_t, the_c });
        }

        // Polyline
        for sp in all_tag_positions(union_xml, "Polyline") {
            let t = start_tag_at(union_xml, sp);
            let points = parse_points_attr(xml_attr(t, "Points").as_deref().unwrap_or(""));
            let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
            let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
            let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
            shapes.push(OmeShape::Polyline { points, the_z, the_t, the_c });
        }

        OmeROI { id, name, shapes }
    }).collect()
}

/// Parse an OME Points attribute string like "1.5,2.5 3.0,4.0 5.5,6.5".
fn parse_points_attr(s: &str) -> Vec<(f64, f64)> {
    s.split_whitespace()
        .filter_map(|pair| {
            let mut parts = pair.split(',');
            let x = parts.next()?.parse::<f64>().ok()?;
            let y = parts.next()?.parse::<f64>().ok()?;
            Some((x, y))
        })
        .collect()
}

// ─── Annotation parsing ─────────────────────────────────────────────────────

fn parse_structured_annotations(xml: &str) -> Vec<OmeAnnotation> {
    let lower = xml.to_ascii_lowercase();
    let sa_start = match lower.find("<structuredannotations") {
        Some(p) => p,
        None => return Vec::new(),
    };
    let sa_end = lower[sa_start..].find("</structuredannotations>")
        .map(|e| sa_start + e + "</structuredannotations>".len())
        .unwrap_or(xml.len());
    let sa_xml = &xml[sa_start..sa_end];

    let mut annotations = Vec::new();

    // MapAnnotation
    for pos in all_tag_positions(sa_xml, "MapAnnotation") {
        let tag = start_tag_at(sa_xml, pos);
        let id = xml_attr(tag, "ID");
        let namespace = xml_attr(tag, "Namespace");
        let ann_lower = sa_xml[pos..].to_ascii_lowercase();
        let ann_end = ann_lower.find("</mapannotation>")
            .map(|e| pos + e + "</mapannotation>".len())
            .unwrap_or(sa_xml.len());
        let ann_xml = &sa_xml[pos..ann_end];

        // Parse <Value><M K="key">value</M></Value> entries
        let mut values = Vec::new();
        for m_pos in all_tag_positions(ann_xml, "M") {
            let m_tag = start_tag_at(ann_xml, m_pos);
            if let Some(key) = xml_attr(m_tag, "K") {
                // Get inner text between > and </M>
                let after_tag = m_pos + m_tag.len();
                let m_lower = ann_xml[after_tag..].to_ascii_lowercase();
                if let Some(close) = m_lower.find("</m>") {
                    let val = ann_xml[after_tag..after_tag + close].trim().to_string();
                    values.push((key, val));
                }
            }
        }
        annotations.push(OmeAnnotation::MapAnnotation { id, namespace, values });
    }

    // CommentAnnotation
    for pos in all_tag_positions(sa_xml, "CommentAnnotation") {
        let tag = start_tag_at(sa_xml, pos);
        let id = xml_attr(tag, "ID");
        let namespace = xml_attr(tag, "Namespace");
        let ann_lower = sa_xml[pos..].to_ascii_lowercase();
        let ann_end = ann_lower.find("</commentannotation>")
            .map(|e| pos + e + "</commentannotation>".len())
            .unwrap_or(sa_xml.len());
        let ann_xml = &sa_xml[pos..ann_end];
        let value = xml_inner_text(ann_xml, "Value").unwrap_or_default();
        annotations.push(OmeAnnotation::CommentAnnotation { id, namespace, value });
    }

    // TagAnnotation
    for pos in all_tag_positions(sa_xml, "TagAnnotation") {
        let tag = start_tag_at(sa_xml, pos);
        let id = xml_attr(tag, "ID");
        let namespace = xml_attr(tag, "Namespace");
        let ann_lower = sa_xml[pos..].to_ascii_lowercase();
        let ann_end = ann_lower.find("</tagannotation>")
            .map(|e| pos + e + "</tagannotation>".len())
            .unwrap_or(sa_xml.len());
        let ann_xml = &sa_xml[pos..ann_end];
        let value = xml_inner_text(ann_xml, "Value").unwrap_or_default();
        annotations.push(OmeAnnotation::TagAnnotation { id, namespace, value });
    }

    annotations
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
