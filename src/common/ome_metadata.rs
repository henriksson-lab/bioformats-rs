//! Structured OME metadata — the Rust equivalent of Java Bio-Formats `IMetadata`.
//!
//! `FormatReader::ome_metadata` returns a baseline OME model derived from core
//! image metadata by default. Readers that carry OME-XML or equivalent rich
//! metadata (CZI, OME-TIFF, OME-XML, etc.) enrich that baseline.

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{ImageMetadata, MetadataValue};

const CHANNEL_GLOBALS_NS: &str = "openmicroscopy.org/bioformats/channel-global-min-max";
const ORIGINAL_METADATA_NS: &str = "openmicroscopy.org/OriginalMetadata";

// ─── Public types ────────────────────────────────────────────────────────────

/// Top-level metadata store — one [`OmeImage`] per image series.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeMetadata {
    pub images: Vec<OmeImage>,
    pub instruments: Vec<OmeInstrument>,
    pub experimenters: Vec<OmeExperimenter>,
    pub rois: Vec<OmeROI>,
    pub annotations: Vec<OmeAnnotation>,
    pub plates: Vec<OmePlate>,
    pub screens: Vec<OmeScreen>,
}

/// Create an OME-style LSID such as `Image:0` or `Channel:0:1`.
pub fn create_lsid(object_type: &str, indexes: &[usize]) -> String {
    let mut lsid = object_type.to_string();
    for index in indexes {
        lsid.push(':');
        lsid.push_str(&index.to_string());
    }
    lsid
}

/// Metadata for one image series.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeImage {
    pub name: Option<String>,
    pub description: Option<String>,
    /// Acquisition timestamp/date string as represented in OME-XML.
    pub acquisition_date: Option<String>,
    /// Physical pixel size in X (micrometres).
    pub physical_size_x: Option<f64>,
    /// Physical pixel size in Y (micrometres).
    pub physical_size_y: Option<f64>,
    /// Physical pixel size in Z / z-step (micrometres).
    pub physical_size_z: Option<f64>,
    /// Time between frames (seconds).
    pub time_increment: Option<f64>,
    /// `<ImagingEnvironment>` temperature (degrees Celsius). Set by readers such
    /// as MicroManager that record the detector/sample temperature.
    pub imaging_environment_temperature: Option<f64>,
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
    /// Confocal pinhole size (µm).
    pub pinhole_size: Option<f64>,
    /// Fluorophore name.
    pub fluor: Option<String>,
    /// Neutral-density filter value.
    pub nd_filter: Option<f64>,
    /// Illumination type (e.g. "Epifluorescence", "Transmitted").
    pub illumination_type: Option<String>,
    /// Contrast method (e.g. "Brightfield", "DIC", "Fluorescence").
    pub contrast_method: Option<String>,
    /// Acquisition mode (e.g. "WideField", "LaserScanningConfocalMicroscopy").
    pub acquisition_mode: Option<String>,
    /// `<DetectorSettings>` gain.
    pub detector_settings_gain: Option<f64>,
    /// `<DetectorSettings>` offset.
    pub detector_settings_offset: Option<f64>,
    /// `<DetectorSettings>` voltage.
    pub detector_settings_voltage: Option<f64>,
    /// `<DetectorSettings>` binning (e.g. "2x2").
    pub detector_settings_binning: Option<String>,
    /// Referenced detector ID for `<DetectorSettings>`.
    pub detector_ref: Option<String>,
    /// Referenced LightSource LSID for `<LightSourceSettings>`.
    pub light_source_settings_id: Option<String>,
    /// `<LightSourceSettings>` attenuation as a 0.0..1.0 fraction
    /// (Java `PercentFraction(intensity / 100f)`).
    pub light_source_settings_attenuation: Option<f64>,
}

/// Instrument metadata (microscope, objectives, detectors, light sources).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeLightSource {
    pub id: Option<String>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    /// Light source type (e.g. "Laser", "Arc", "LED", "Filament").
    pub light_source_type: Option<String>,
    /// Power (milliwatts).
    pub power: Option<f64>,
    /// Laser line wavelength (nm). Only meaningful for `Laser` light sources;
    /// serialized as the OME `Laser/@Wavelength` attribute.
    pub wavelength: Option<f64>,
}

/// Optical filter metadata.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeDichroic {
    pub id: Option<String>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
}

/// Light path: the combination of filters and dichroics used for a channel.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeLightPath {
    pub excitation_filter_ids: Vec<String>,
    pub dichroic_id: Option<String>,
    pub emission_filter_ids: Vec<String>,
}

/// Region of interest.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeROI {
    pub id: Option<String>,
    pub name: Option<String>,
    pub shapes: Vec<OmeShape>,
}

/// A single shape within an ROI.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum OmeShape {
    Rectangle {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        the_z: Option<u32>,
        the_t: Option<u32>,
        the_c: Option<u32>,
    },
    Ellipse {
        x: f64,
        y: f64,
        radius_x: f64,
        radius_y: f64,
        the_z: Option<u32>,
        the_t: Option<u32>,
        the_c: Option<u32>,
    },
    Point {
        x: f64,
        y: f64,
        the_z: Option<u32>,
        the_t: Option<u32>,
        the_c: Option<u32>,
    },
    Line {
        x1: f64,
        y1: f64,
        x2: f64,
        y2: f64,
        the_z: Option<u32>,
        the_t: Option<u32>,
        the_c: Option<u32>,
    },
    Polygon {
        points: Vec<(f64, f64)>,
        the_z: Option<u32>,
        the_t: Option<u32>,
        the_c: Option<u32>,
    },
    Polyline {
        points: Vec<(f64, f64)>,
        the_z: Option<u32>,
        the_t: Option<u32>,
        the_c: Option<u32>,
    },
}

/// Experimenter metadata.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeExperimenter {
    pub id: Option<String>,
    pub first_name: Option<String>,
    pub last_name: Option<String>,
    pub email: Option<String>,
    pub institution: Option<String>,
}

/// Annotation (key-value or structured).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum OmeAnnotation {
    /// Simple key-value string annotation.
    MapAnnotation {
        id: Option<String>,
        namespace: Option<String>,
        values: Vec<(String, String)>,
    },
    /// Comment annotation.
    CommentAnnotation {
        id: Option<String>,
        namespace: Option<String>,
        value: String,
    },
    /// Tag annotation.
    TagAnnotation {
        id: Option<String>,
        namespace: Option<String>,
        value: String,
    },
}

/// High-Content Screening plate metadata.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmePlate {
    pub id: Option<String>,
    pub name: Option<String>,
    pub rows: u32,
    pub columns: u32,
    pub wells: Vec<OmeWell>,
}

/// A well in an HCS plate.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeWell {
    pub id: Option<String>,
    pub row: u32,
    pub column: u32,
    pub well_samples: Vec<OmeWellSample>,
}

/// A field/site within a well.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeWellSample {
    pub id: Option<String>,
    pub index: u32,
    /// Reference to an Image (index into OmeMetadata::images).
    pub image_ref: Option<usize>,
    pub position_x: Option<f64>,
    pub position_y: Option<f64>,
}

/// Screen metadata (collection of plates).
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeScreen {
    pub id: Option<String>,
    pub name: Option<String>,
    pub protocol_description: Option<String>,
}

/// Experiment metadata.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeExperiment {
    pub id: Option<String>,
    pub experiment_type: Option<String>,
    pub description: Option<String>,
}

/// Dataset metadata — a named collection of images.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeDataset {
    pub id: Option<String>,
    pub name: Option<String>,
    pub description: Option<String>,
    /// Indices into OmeMetadata::images that belong to this dataset.
    pub image_refs: Vec<usize>,
}

/// Per-plane metadata.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
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
        let _ = write!(
            xml,
            r#"<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.openmicroscopy.org/Schemas/OME/2016-06 http://www.openmicroscopy.org/Schemas/OME/2016-06/ome.xsd">"#
        );

        for (ei, experimenter) in self.experimenters.iter().enumerate() {
            let default_id = format!("Experimenter:{ei}");
            let exp_id = experimenter.id.as_deref().unwrap_or(&default_id);
            let _ = write!(xml, r#"<Experimenter ID="{}""#, xml_escape(exp_id));
            if let Some(v) = &experimenter.first_name {
                let _ = write!(xml, r#" FirstName="{}""#, xml_escape(v));
            }
            if let Some(v) = &experimenter.last_name {
                let _ = write!(xml, r#" LastName="{}""#, xml_escape(v));
            }
            if let Some(v) = &experimenter.email {
                let _ = write!(xml, r#" Email="{}""#, xml_escape(v));
            }
            if let Some(v) = &experimenter.institution {
                let _ = write!(xml, r#" Institution="{}""#, xml_escape(v));
            }
            xml.push_str("/>");
        }

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
                if let Some(v) = &obj.model {
                    let _ = write!(xml, r#" Model="{}""#, xml_escape(v));
                }
                if let Some(v) = &obj.manufacturer {
                    let _ = write!(xml, r#" Manufacturer="{}""#, xml_escape(v));
                }
                if let Some(v) = obj.nominal_magnification {
                    let _ = write!(xml, r#" NominalMagnification="{v}""#);
                }
                if let Some(v) = obj.calibrated_magnification {
                    let _ = write!(xml, r#" CalibratedMagnification="{v}""#);
                }
                if let Some(v) = obj.lens_na {
                    let _ = write!(xml, r#" LensNA="{v}""#);
                }
                if let Some(v) = &obj.immersion {
                    let _ = write!(xml, r#" Immersion="{}""#, xml_escape(v));
                }
                if let Some(v) = &obj.correction {
                    let _ = write!(xml, r#" Correction="{}""#, xml_escape(v));
                }
                if let Some(v) = obj.working_distance {
                    let _ = write!(xml, r#" WorkingDistance="{v}""#);
                }
                xml.push_str("/>");
            }

            for (di, det) in inst.detectors.iter().enumerate() {
                let default_det_id = format!("Detector:{ii}:{di}");
                let det_id = det.id.as_deref().unwrap_or(&default_det_id);
                let _ = write!(xml, r#"<Detector ID="{}""#, xml_escape(det_id));
                if let Some(v) = &det.model {
                    let _ = write!(xml, r#" Model="{}""#, xml_escape(v));
                }
                if let Some(v) = &det.manufacturer {
                    let _ = write!(xml, r#" Manufacturer="{}""#, xml_escape(v));
                }
                if let Some(v) = &det.detector_type {
                    let _ = write!(xml, r#" Type="{}""#, xml_escape(v));
                }
                if let Some(v) = det.gain {
                    let _ = write!(xml, r#" Gain="{v}""#);
                }
                if let Some(v) = det.offset {
                    let _ = write!(xml, r#" Offset="{v}""#);
                }
                xml.push_str("/>");
            }

            for (li, ls) in inst.light_sources.iter().enumerate() {
                let ls_tag = ome_light_source_tag(ls.light_source_type.as_deref())
                    .unwrap_or("GenericExcitationSource");
                let default_ls_id = format!("LightSource:{ii}:{li}");
                let ls_id = ls.id.as_deref().unwrap_or(&default_ls_id);
                let _ = write!(xml, r#"<{ls_tag} ID="{}""#, xml_escape(ls_id));
                if let Some(v) = &ls.model {
                    let _ = write!(xml, r#" Model="{}""#, xml_escape(v));
                }
                if let Some(v) = &ls.manufacturer {
                    let _ = write!(xml, r#" Manufacturer="{}""#, xml_escape(v));
                }
                if let Some(v) = ls.power {
                    let _ = write!(xml, r#" Power="{v}""#);
                }
                // Wavelength is only valid on the OME <Laser> element.
                if ls_tag == "Laser" {
                    if let Some(v) = ls.wavelength {
                        let _ = write!(xml, r#" Wavelength="{v}""#);
                    }
                }
                xml.push_str("/>");
            }

            for (fi, filter) in inst.filters.iter().enumerate() {
                let default_f_id = format!("Filter:{ii}:{fi}");
                let f_id = filter.id.as_deref().unwrap_or(&default_f_id);
                let _ = write!(xml, r#"<Filter ID="{}""#, xml_escape(f_id));
                if let Some(v) = &filter.model {
                    let _ = write!(xml, r#" Model="{}""#, xml_escape(v));
                }
                if let Some(v) = &filter.manufacturer {
                    let _ = write!(xml, r#" Manufacturer="{}""#, xml_escape(v));
                }
                if let Some(v) = &filter.filter_type {
                    let _ = write!(xml, r#" Type="{}""#, xml_escape(v));
                }
                if let Some(v) = filter.cut_in {
                    let _ = write!(xml, r#" CutIn="{v}""#);
                }
                if let Some(v) = filter.cut_out {
                    let _ = write!(xml, r#" CutOut="{v}""#);
                }
                xml.push_str("/>");
            }

            for (di, dc) in inst.dichroics.iter().enumerate() {
                let default_d_id = format!("Dichroic:{ii}:{di}");
                let d_id = dc.id.as_deref().unwrap_or(&default_d_id);
                let _ = write!(xml, r#"<Dichroic ID="{}""#, xml_escape(d_id));
                if let Some(v) = &dc.model {
                    let _ = write!(xml, r#" Model="{}""#, xml_escape(v));
                }
                if let Some(v) = &dc.manufacturer {
                    let _ = write!(xml, r#" Manufacturer="{}""#, xml_escape(v));
                }
                xml.push_str("/>");
            }

            xml.push_str("</Instrument>");
        }

        let images = if self.images.is_empty() {
            Vec::new()
        } else {
            // This serializer is given one ImageMetadata, so it can only write
            // dimensions/pixel type honestly for one OME Image.
            self.images.iter().take(1).collect()
        };

        for (i, img) in images.into_iter().enumerate() {
            let _ = write!(
                xml,
                r#"<Image ID="Image:{i}" Name="{}">"#,
                xml_escape(img.name.as_deref().unwrap_or(&format!("Series {i}")))
            );

            if let Some(desc) = &img.description {
                let _ = write!(xml, "<Description>{}</Description>", xml_escape(desc));
            }
            if let Some(date) = &img.acquisition_date {
                let _ = write!(
                    xml,
                    "<AcquisitionDate>{}</AcquisitionDate>",
                    xml_escape(date)
                );
            }
            if let Some(v) = img.imaging_environment_temperature {
                let _ = write!(
                    xml,
                    r#"<ImagingEnvironment Temperature="{v}" TemperatureUnit="°C"/>"#
                );
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
            let _ = write!(
                xml,
                r#"<Pixels ID="Pixels:{i}" DimensionOrder="{dim_order}" Type="{pt_str}" SizeX="{}" SizeY="{}" SizeZ="{}" SizeC="{}" SizeT="{}" BigEndian="{}""#,
                meta.size_x,
                meta.size_y,
                meta.size_z,
                meta.size_c,
                meta.size_t,
                !meta.is_little_endian
            );

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
            let channel_count = effective_size_c(meta) as usize;
            for (ci, ch) in img.channels.iter().take(channel_count).enumerate() {
                let light_path = img.light_paths.get(ci);
                let _ = write!(
                    xml,
                    r#"<Channel ID="Channel:{i}:{ci}" SamplesPerPixel="{}""#,
                    ch.samples_per_pixel
                );
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
                if let Some(v) = ch.pinhole_size {
                    let _ = write!(xml, r#" PinholeSize="{v}""#);
                }
                if let Some(s) = &ch.fluor {
                    let _ = write!(xml, r#" Fluor="{}""#, xml_escape(s));
                }
                if let Some(v) = ch.nd_filter {
                    let _ = write!(xml, r#" NDFilter="{v}""#);
                }
                if let Some(s) = &ch.illumination_type {
                    let _ = write!(xml, r#" IlluminationType="{}""#, xml_escape(s));
                }
                if let Some(s) = &ch.contrast_method {
                    let _ = write!(xml, r#" ContrastMethod="{}""#, xml_escape(s));
                }
                if let Some(s) = &ch.acquisition_mode {
                    let _ = write!(xml, r#" AcquisitionMode="{}""#, xml_escape(s));
                }
                let has_light_source_settings = ch.light_source_settings_id.is_some()
                    || ch.light_source_settings_attenuation.is_some();
                let has_detector_settings = ch.detector_settings_gain.is_some()
                    || ch.detector_settings_offset.is_some()
                    || ch.detector_settings_voltage.is_some()
                    || ch.detector_settings_binning.is_some()
                    || ch.detector_ref.is_some();
                let has_light_path = light_path.is_some_and(|path| {
                    !path.excitation_filter_ids.is_empty()
                        || path.dichroic_id.is_some()
                        || !path.emission_filter_ids.is_empty()
                });
                if has_light_source_settings || has_detector_settings || has_light_path {
                    xml.push('>');
                    if has_light_source_settings {
                        let _ = write!(
                            xml,
                            r#"<LightSourceSettings ID="{}""#,
                            xml_escape(
                                ch.light_source_settings_id
                                    .as_deref()
                                    .unwrap_or("LightSource:0")
                            )
                        );
                        if let Some(v) = ch.light_source_settings_attenuation {
                            let _ = write!(xml, r#" Attenuation="{v}""#);
                        }
                        xml.push_str("/>");
                    }
                    if has_detector_settings {
                        let _ = write!(
                            xml,
                            r#"<DetectorSettings ID="{}""#,
                            xml_escape(ch.detector_ref.as_deref().unwrap_or("Detector:0"))
                        );
                        if let Some(v) = ch.detector_settings_gain {
                            let _ = write!(xml, r#" Gain="{v}""#);
                        }
                        if let Some(v) = ch.detector_settings_offset {
                            let _ = write!(xml, r#" Offset="{v}""#);
                        }
                        if let Some(v) = ch.detector_settings_voltage {
                            let _ = write!(xml, r#" Voltage="{v}""#);
                        }
                        if let Some(s) = &ch.detector_settings_binning {
                            let _ = write!(xml, r#" Binning="{}""#, xml_escape(s));
                        }
                        xml.push_str("/>");
                    }
                    if let Some(path) = light_path {
                        if has_light_path {
                            xml.push_str("<LightPath>");
                            for id in &path.excitation_filter_ids {
                                let _ = write!(
                                    xml,
                                    r#"<ExcitationFilterRef ID="{}"/>"#,
                                    xml_escape(id)
                                );
                            }
                            if let Some(id) = &path.dichroic_id {
                                let _ = write!(xml, r#"<DichroicRef ID="{}"/>"#, xml_escape(id));
                            }
                            for id in &path.emission_filter_ids {
                                let _ =
                                    write!(xml, r#"<EmissionFilterRef ID="{}"/>"#, xml_escape(id));
                            }
                            xml.push_str("</LightPath>");
                        }
                    }
                    xml.push_str("</Channel>");
                } else {
                    xml.push_str("/>");
                }
            }

            // Modulo annotations
            if let Some(m) = &img.modulo_z {
                let _ = write!(
                    xml,
                    r#"<ModuloAlongZ Type="{}" Start="{}" Step="{}" End="{}" Unit="{}"/>"#,
                    xml_escape(&m.modulo_type),
                    m.start,
                    m.step,
                    m.end,
                    xml_escape(&m.unit)
                );
            }
            if let Some(m) = &img.modulo_c {
                let _ = write!(
                    xml,
                    r#"<ModuloAlongC Type="{}" Start="{}" Step="{}" End="{}" Unit="{}"/>"#,
                    xml_escape(&m.modulo_type),
                    m.start,
                    m.step,
                    m.end,
                    xml_escape(&m.unit)
                );
            }
            if let Some(m) = &img.modulo_t {
                let _ = write!(
                    xml,
                    r#"<ModuloAlongT Type="{}" Start="{}" Step="{}" End="{}" Unit="{}"/>"#,
                    xml_escape(&m.modulo_type),
                    m.start,
                    m.step,
                    m.end,
                    xml_escape(&m.unit)
                );
            }

            // Planes
            for plane in &img.planes {
                let _ = write!(
                    xml,
                    r#"<Plane TheZ="{}" TheC="{}" TheT="{}""#,
                    plane.the_z, plane.the_c, plane.the_t
                );
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

        for (ri, roi) in self.rois.iter().enumerate() {
            let default_id = create_lsid("ROI", &[ri]);
            let roi_id = roi.id.as_deref().unwrap_or(&default_id);
            let _ = write!(xml, r#"<ROI ID="{}""#, xml_escape(roi_id));
            if let Some(name) = &roi.name {
                let _ = write!(xml, r#" Name="{}""#, xml_escape(name));
            }
            xml.push_str("><Union>");
            for shape in &roi.shapes {
                write_ome_shape_xml(&mut xml, shape);
            }
            xml.push_str("</Union></ROI>");
        }

        if !self.annotations.is_empty() {
            xml.push_str("<StructuredAnnotations>");
            for (ai, annotation) in self.annotations.iter().enumerate() {
                match annotation {
                    OmeAnnotation::MapAnnotation {
                        id,
                        namespace,
                        values,
                    } => {
                        let default_id = create_lsid("Annotation:Map", &[ai]);
                        let ann_id = id.as_deref().unwrap_or(&default_id);
                        let _ = write!(xml, r#"<MapAnnotation ID="{}""#, xml_escape(ann_id));
                        if let Some(ns) = namespace {
                            let _ = write!(xml, r#" Namespace="{}""#, xml_escape(ns));
                        }
                        xml.push_str("><Value>");
                        for (key, value) in values {
                            let _ = write!(
                                xml,
                                r#"<M K="{}">{}</M>"#,
                                xml_escape(key),
                                xml_escape(value)
                            );
                        }
                        xml.push_str("</Value></MapAnnotation>");
                    }
                    OmeAnnotation::CommentAnnotation {
                        id,
                        namespace,
                        value,
                    } => {
                        let default_id = create_lsid("Annotation:Comment", &[ai]);
                        let ann_id = id.as_deref().unwrap_or(&default_id);
                        let _ = write!(xml, r#"<CommentAnnotation ID="{}""#, xml_escape(ann_id));
                        if let Some(ns) = namespace {
                            let _ = write!(xml, r#" Namespace="{}""#, xml_escape(ns));
                        }
                        let _ = write!(
                            xml,
                            "><Value>{}</Value></CommentAnnotation>",
                            xml_escape(value)
                        );
                    }
                    OmeAnnotation::TagAnnotation {
                        id,
                        namespace,
                        value,
                    } => {
                        let default_id = create_lsid("Annotation:Tag", &[ai]);
                        let ann_id = id.as_deref().unwrap_or(&default_id);
                        let _ = write!(xml, r#"<TagAnnotation ID="{}""#, xml_escape(ann_id));
                        if let Some(ns) = namespace {
                            let _ = write!(xml, r#" Namespace="{}""#, xml_escape(ns));
                        }
                        let _ =
                            write!(xml, "><Value>{}</Value></TagAnnotation>", xml_escape(value));
                    }
                }
            }
            xml.push_str("</StructuredAnnotations>");
        }

        xml.push_str("</OME>");
        xml
    }
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&apos;")
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

fn ome_light_source_tag(value: Option<&str>) -> Option<&'static str> {
    match value.unwrap_or("GenericExcitationSource") {
        "Laser" => Some("Laser"),
        "Arc" => Some("Arc"),
        "Filament" => Some("Filament"),
        "LightEmittingDiode" | "LED" => Some("LightEmittingDiode"),
        "GenericExcitationSource" => Some("GenericExcitationSource"),
        _ => None,
    }
}

fn write_ome_shape_xml(xml: &mut String, shape: &OmeShape) {
    use std::fmt::Write;

    match shape {
        OmeShape::Rectangle {
            x,
            y,
            width,
            height,
            the_z,
            the_t,
            the_c,
        } => {
            let _ = write!(
                xml,
                r#"<Rectangle X="{x}" Y="{y}" Width="{width}" Height="{height}""#
            );
            write_shape_indices(xml, *the_z, *the_t, *the_c);
            xml.push_str("/>");
        }
        OmeShape::Ellipse {
            x,
            y,
            radius_x,
            radius_y,
            the_z,
            the_t,
            the_c,
        } => {
            let _ = write!(
                xml,
                r#"<Ellipse X="{x}" Y="{y}" RadiusX="{radius_x}" RadiusY="{radius_y}""#
            );
            write_shape_indices(xml, *the_z, *the_t, *the_c);
            xml.push_str("/>");
        }
        OmeShape::Point {
            x,
            y,
            the_z,
            the_t,
            the_c,
        } => {
            let _ = write!(xml, r#"<Point X="{x}" Y="{y}""#);
            write_shape_indices(xml, *the_z, *the_t, *the_c);
            xml.push_str("/>");
        }
        OmeShape::Line {
            x1,
            y1,
            x2,
            y2,
            the_z,
            the_t,
            the_c,
        } => {
            let _ = write!(xml, r#"<Line X1="{x1}" Y1="{y1}" X2="{x2}" Y2="{y2}""#);
            write_shape_indices(xml, *the_z, *the_t, *the_c);
            xml.push_str("/>");
        }
        OmeShape::Polygon {
            points,
            the_z,
            the_t,
            the_c,
        } => {
            let _ = write!(xml, r#"<Polygon Points="{}""#, format_points_attr(points));
            write_shape_indices(xml, *the_z, *the_t, *the_c);
            xml.push_str("/>");
        }
        OmeShape::Polyline {
            points,
            the_z,
            the_t,
            the_c,
        } => {
            let _ = write!(xml, r#"<Polyline Points="{}""#, format_points_attr(points));
            write_shape_indices(xml, *the_z, *the_t, *the_c);
            xml.push_str("/>");
        }
    }
}

fn write_shape_indices(
    xml: &mut String,
    the_z: Option<u32>,
    the_t: Option<u32>,
    the_c: Option<u32>,
) {
    use std::fmt::Write;

    if let Some(v) = the_z {
        let _ = write!(xml, r#" TheZ="{v}""#);
    }
    if let Some(v) = the_t {
        let _ = write!(xml, r#" TheT="{v}""#);
    }
    if let Some(v) = the_c {
        let _ = write!(xml, r#" TheC="{v}""#);
    }
}

fn format_points_attr(points: &[(f64, f64)]) -> String {
    points
        .iter()
        .map(|(x, y)| format!("{x},{y}"))
        .collect::<Vec<_>>()
        .join(" ")
}

// ─── Parsers ──────────────────────────────────────────────────────────────────

impl OmeMetadata {
    /// Java `MetadataTools.populateMetadata`-style conversion from core metadata.
    pub fn populate_metadata(meta: &ImageMetadata) -> Self {
        Self::from_image_metadata(meta)
    }

    /// Java `MetadataTools.convertMetadata`-style conversion from core metadata.
    pub fn convert_metadata(meta: &ImageMetadata) -> Self {
        Self::from_image_metadata(meta)
    }

    /// Populate the OME image/channel/modulo fields supported by this crate.
    pub fn populate_pixels(&mut self, meta: &ImageMetadata, image_index: usize) -> Result<()> {
        if self.images.len() <= image_index {
            self.images.resize_with(image_index + 1, OmeImage::default);
        }

        let spp = rgb_channel_count(meta);
        let channel_count = effective_size_c(meta) as usize;
        let image = &mut self.images[image_index];
        if image.channels.len() < channel_count {
            image
                .channels
                .resize_with(channel_count, OmeChannel::default);
        }
        for channel in &mut image.channels {
            if channel.samples_per_pixel == 0 {
                channel.samples_per_pixel = spp;
            }
        }

        image.modulo_z = meta.modulo_z.clone();
        image.modulo_c = meta.modulo_c.clone();
        image.modulo_t = meta.modulo_t.clone();
        Ok(())
    }

    /// Verify that the minimum OME metadata derivable from [`ImageMetadata`] exists.
    pub fn verify_minimum_populated(&self, meta: &ImageMetadata, image_index: usize) -> Result<()> {
        if meta.size_x == 0 || meta.size_y == 0 {
            return Err(BioFormatsError::InvalidData(
                "minimum metadata requires non-zero SizeX and SizeY".into(),
            ));
        }
        if meta.size_z == 0 || meta.size_c == 0 || meta.size_t == 0 {
            return Err(BioFormatsError::InvalidData(
                "minimum metadata requires non-zero SizeZ, SizeC and SizeT".into(),
            ));
        }

        let effective_c = effective_size_c(meta);
        let expected_planes = meta
            .size_z
            .checked_mul(effective_c)
            .and_then(|v| v.checked_mul(meta.size_t))
            .ok_or_else(|| BioFormatsError::InvalidData("Z/C/T plane count overflow".into()))?;
        if meta.image_count < expected_planes {
            return Err(BioFormatsError::InvalidData(format!(
                "ImageCount {} is smaller than SizeZ*SizeC*SizeT {}",
                meta.image_count, expected_planes
            )));
        }

        let image = self.images.get(image_index).ok_or_else(|| {
            BioFormatsError::InvalidData(format!("missing OME Image at index {image_index}"))
        })?;
        let expected_channels = effective_size_c(meta) as usize;
        if image.channels.len() < expected_channels {
            return Err(BioFormatsError::InvalidData(format!(
                "OME Image {image_index} has {} channels but requires {expected_channels}",
                image.channels.len(),
            )));
        }
        if image.channels.len() > expected_channels {
            return Err(BioFormatsError::InvalidData(format!(
                "OME Image {image_index} has {} channels but metadata SizeC requires {expected_channels}",
                image.channels.len(),
            )));
        }
        if image
            .channels
            .iter()
            .take(expected_channels)
            .any(|channel| channel.samples_per_pixel == 0)
        {
            return Err(BioFormatsError::InvalidData(
                "minimum metadata requires SamplesPerPixel for every channel".into(),
            ));
        }
        if meta.is_rgb {
            let samples_per_pixel = rgb_channel_count(meta);
            if image
                .channels
                .iter()
                .take(expected_channels)
                .any(|channel| channel.samples_per_pixel != samples_per_pixel)
            {
                return Err(BioFormatsError::InvalidData(format!(
                    "RGB metadata requires OME channels with SamplesPerPixel={samples_per_pixel}"
                )));
            }
        }
        Ok(())
    }

    /// Store per-channel global min/max values as a map annotation.
    pub fn add_channel_global_min_max(
        &mut self,
        image_index: usize,
        channel_index: usize,
        global_min: f64,
        global_max: f64,
    ) -> Result<()> {
        let image = self.images.get(image_index).ok_or_else(|| {
            BioFormatsError::InvalidData(format!("missing OME Image at index {image_index}"))
        })?;
        if channel_index >= image.channels.len() {
            return Err(BioFormatsError::InvalidData(format!(
                "missing OME Channel at index {image_index}:{channel_index}"
            )));
        }
        self.annotations.push(OmeAnnotation::MapAnnotation {
            id: Some(create_lsid(
                "Annotation:ChannelGlobalMinMax",
                &[image_index, channel_index],
            )),
            namespace: Some(CHANNEL_GLOBALS_NS.into()),
            values: vec![
                ("Image".into(), create_lsid("Image", &[image_index])),
                (
                    "Channel".into(),
                    create_lsid("Channel", &[image_index, channel_index]),
                ),
                ("GlobalMin".into(), global_min.to_string()),
                ("GlobalMax".into(), global_max.to_string()),
            ],
        });
        Ok(())
    }

    /// Store original/proprietary series metadata as OME map annotations.
    pub fn add_original_metadata_annotations(
        &mut self,
        meta: &ImageMetadata,
        image_index: usize,
    ) -> Result<()> {
        if self.images.get(image_index).is_none() {
            return Err(BioFormatsError::InvalidData(format!(
                "missing OME Image at index {image_index}"
            )));
        }
        if meta.series_metadata.is_empty() {
            return Ok(());
        }

        let mut values: Vec<(String, String)> = meta
            .series_metadata
            .iter()
            .map(|(key, value)| (key.clone(), metadata_value_to_string(value)))
            .collect();
        values.sort_by(|a, b| a.0.cmp(&b.0));
        values.insert(0, ("Image".into(), create_lsid("Image", &[image_index])));

        self.annotations.push(OmeAnnotation::MapAnnotation {
            id: Some(create_lsid("Annotation:OriginalMetadata", &[image_index])),
            namespace: Some(ORIGINAL_METADATA_NS.into()),
            values,
        });
        Ok(())
    }

    /// Build a minimal `OmeMetadata` from any [`crate::metadata::ImageMetadata`].
    ///
    /// Every format reader uses this as a baseline — it captures channel count
    /// and samples-per-pixel, which are always available. Format-specific
    /// `ome_metadata()` overrides enrich this with physical sizes, channel
    /// names, wavelengths, etc.
    pub fn from_image_metadata(meta: &ImageMetadata) -> Self {
        let mut ome = OmeMetadata::default();
        let _ = ome.populate_pixels(meta, 0);

        if let Some(image) = ome.images.get_mut(0) {
            image.name = generic_image_name_from_metadata(meta);
            image.description = generic_image_description_from_metadata(meta);
            image.acquisition_date = generic_acquisition_date_from_metadata(meta);
            let channel_count = image.channels.len();
            for channel_index in 0..channel_count {
                let prefix = format!("channel.{channel_index}");
                if let Some(name) =
                    metadata_string_by_suffix(&meta.series_metadata, &[&format!("{prefix}.name")])
                {
                    image.channels[channel_index].name = Some(name);
                }
                image.channels[channel_index].excitation_wavelength =
                    metadata_positive_f64_by_suffix(
                        &meta.series_metadata,
                        &[&format!("{prefix}.excitation_wavelength")],
                    );
                image.channels[channel_index].emission_wavelength = metadata_positive_f64_by_suffix(
                    &meta.series_metadata,
                    &[&format!("{prefix}.emission_wavelength")],
                );
            }
            image.planes = generic_planes_from_metadata(meta);
            image.light_paths = generic_light_paths_from_metadata(meta, channel_count);
        }

        let objective = generic_objective_from_metadata(meta);
        let detector = generic_detector_from_metadata(meta);
        let light_source = generic_light_source_from_metadata(meta);
        let filter = generic_filter_from_metadata(meta);
        let dichroic = generic_dichroic_from_metadata(meta);
        if objective.is_some()
            || detector.is_some()
            || light_source.is_some()
            || filter.is_some()
            || dichroic.is_some()
        {
            ome.instruments.push(OmeInstrument {
                id: Some(create_lsid("Instrument", &[0])),
                objectives: objective.into_iter().collect(),
                detectors: detector.into_iter().collect(),
                light_sources: light_source.into_iter().collect(),
                filters: filter.into_iter().collect(),
                dichroics: dichroic.into_iter().collect(),
                ..Default::default()
            });
            if let Some(image) = ome.images.get_mut(0) {
                image.instrument_ref = Some(0);
                if !ome.instruments[0].objectives.is_empty() {
                    image.objective_ref = Some(0);
                }
            }
        }
        if let Some(experimenter) = generic_experimenter_from_metadata(meta) {
            ome.experimenters.push(experimenter);
        }
        ome.rois = generic_rois_from_metadata(meta);
        ome
    }

    /// Parse OME-XML into structured metadata.
    ///
    /// Handles both standalone `.ome` files and OME-XML embedded in TIFF
    /// `ImageDescription` tags.
    pub fn from_ome_xml(xml: &str) -> Self {
        let mut images = Vec::new();

        for img_start in all_tag_positions(xml, "Image") {
            let img_tag = start_tag_at(xml, img_start);
            let name = xml_attr(img_tag, "Name");

            let img_end = find_end_tag(xml, "Image", img_start)
                .map(|p| xml[p..].find('>').map(|e| p + e + 1).unwrap_or(xml.len()))
                .unwrap_or(xml.len());
            let img_xml = &xml[img_start..img_end];

            let description = xml_inner_text(img_xml, "Description");
            let acquisition_date = xml_inner_text(img_xml, "AcquisitionDate");

            let pixels_pos = all_tag_positions(img_xml, "Pixels").into_iter().next();

            let (physical_size_x, physical_size_y, physical_size_z, time_increment) =
                if let Some(p) = pixels_pos {
                    let pt = start_tag_at(img_xml, p);
                    let psx = xml_attr(pt, "PhysicalSizeX").and_then(|s| s.parse::<f64>().ok());
                    let psy = xml_attr(pt, "PhysicalSizeY").and_then(|s| s.parse::<f64>().ok());
                    let psz = xml_attr(pt, "PhysicalSizeZ").and_then(|s| s.parse::<f64>().ok());
                    let ti = xml_attr(pt, "TimeIncrement").and_then(|s| s.parse::<f64>().ok());
                    let psx_u = xml_attr(pt, "PhysicalSizeXUnit").unwrap_or_else(|| "µm".into());
                    let psy_u = xml_attr(pt, "PhysicalSizeYUnit").unwrap_or_else(|| "µm".into());
                    let psz_u = xml_attr(pt, "PhysicalSizeZUnit").unwrap_or_else(|| "µm".into());
                    let ti_u = xml_attr(pt, "TimeIncrementUnit").unwrap_or_else(|| "s".into());
                    (
                        psx.map(|v| to_microns(v, &psx_u)),
                        psy.map(|v| to_microns(v, &psy_u)),
                        psz.map(|v| to_microns(v, &psz_u)),
                        ti.map(|v| to_seconds(v, &ti_u)),
                    )
                } else {
                    (None, None, None, None)
                };

            // Parse channels and planes from within the <Pixels> block.
            let pixels_end = pixels_pos
                .and_then(|p| {
                    find_end_tag(img_xml, "Pixels", p).map(|e| {
                        img_xml[e..]
                            .find('>')
                            .map(|gt| e + gt + 1)
                            .unwrap_or(img_xml.len())
                    })
                })
                .unwrap_or(img_xml.len());
            let pixels_xml = pixels_pos.map(|p| &img_xml[p..pixels_end]);

            let channels = pixels_xml.map(parse_channels).unwrap_or_default();
            let planes = pixels_xml.map(parse_planes).unwrap_or_default();
            let modulo_z = parse_modulo(xml, img_xml, "Z");
            let modulo_c = parse_modulo(xml, img_xml, "C");
            let modulo_t = parse_modulo(xml, img_xml, "T");

            images.push(OmeImage {
                name,
                description,
                acquisition_date,
                physical_size_x,
                physical_size_y,
                physical_size_z,
                time_increment,
                channels,
                planes,
                modulo_z,
                modulo_c,
                modulo_t,
                ..Default::default()
            });
        }

        // Parse <Instrument> elements (top-level, siblings of <Image>)
        let instruments = parse_instruments(xml);

        // Resolve InstrumentRef/ObjectiveRef for each image
        for (i, img_start) in all_tag_positions(xml, "Image").into_iter().enumerate() {
            if i >= images.len() {
                break;
            }
            let img_end = find_end_tag(xml, "Image", img_start)
                .map(|p| xml[p..].find('>').map(|e| p + e + 1).unwrap_or(xml.len()))
                .unwrap_or(xml.len());
            let img_xml = &xml[img_start..img_end];

            // <InstrumentRef ID="Instrument:0"/>
            if let Some(pos) = all_tag_positions(img_xml, "InstrumentRef")
                .into_iter()
                .next()
            {
                let tag = start_tag_at(img_xml, pos);
                if let Some(ref_id) = xml_attr(tag, "ID") {
                    images[i].instrument_ref = instruments
                        .iter()
                        .position(|inst| inst.id.as_deref() == Some(ref_id.as_str()));
                }
            }
            // <ObjectiveSettings ID="Objective:0:0"/>
            if let Some(pos) = all_tag_positions(img_xml, "ObjectiveSettings")
                .into_iter()
                .next()
            {
                let tag = start_tag_at(img_xml, pos);
                if let Some(ref_id) = xml_attr(tag, "ID") {
                    if let Some(inst_idx) = images[i].instrument_ref {
                        images[i].objective_ref = instruments[inst_idx]
                            .objectives
                            .iter()
                            .position(|obj| obj.id.as_deref() == Some(ref_id.as_str()));
                    }
                }
            }
        }

        // Parse <Experimenter> elements (top-level, exclude ExperimenterGroup/ExperimenterRef)
        let experimenters = all_tag_positions(xml, "Experimenter")
            .into_iter()
            .filter(|&pos| {
                let tag = start_tag_at(xml, pos);
                let tag_lower = tag.to_ascii_lowercase();
                !tag_lower.starts_with("<experimentergroup")
                    && !tag_lower.starts_with("<experimenterref")
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

        OmeMetadata {
            images,
            instruments,
            experimenters,
            rois,
            annotations,
            ..Default::default()
        }
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
        OmeMetadata {
            images: vec![image],
            ..Default::default()
        }
    }
}

// ─── OME-XML helpers ─────────────────────────────────────────────────────────

fn parse_channels(pixels_xml: &str) -> Vec<OmeChannel> {
    all_tag_positions(pixels_xml, "Channel")
        .into_iter()
        .map(|pos| {
            let tag = start_tag_at(pixels_xml, pos);
            // Round-trip the `<LightSourceSettings>` child element if present.
            let body_start = pos + tag.len();
            let body_end = find_end_tag(pixels_xml, "Channel", body_start).unwrap_or(body_start);
            let body = pixels_xml.get(pos..body_end.max(pos)).unwrap_or("");
            let (lss_id, lss_atten) = all_tag_positions(body, "LightSourceSettings")
                .into_iter()
                .next()
                .map(|lp| {
                    let lt = start_tag_at(body, lp);
                    (
                        xml_attr(lt, "ID"),
                        xml_attr(lt, "Attenuation").and_then(|s| s.parse::<f64>().ok()),
                    )
                })
                .unwrap_or((None, None));
            OmeChannel {
                name: xml_attr(tag, "Name"),
                samples_per_pixel: xml_attr(tag, "SamplesPerPixel")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(1),
                color: xml_attr(tag, "Color").and_then(|s| s.parse::<i32>().ok()),
                emission_wavelength: xml_attr(tag, "EmissionWavelength")
                    .and_then(|s| s.parse().ok()),
                excitation_wavelength: xml_attr(tag, "ExcitationWavelength")
                    .and_then(|s| s.parse().ok()),
                light_source_settings_id: lss_id,
                light_source_settings_attenuation: lss_atten,
                ..Default::default()
            }
        })
        .collect()
}

/// Parse a single `<ModuloAlong{dim}>` element, given the slice of XML that
/// directly contains it. Reads the Type/Start/Step/End/Unit attributes and any
/// nested `<Label>` children, mirroring `OMEXMLServiceImpl.getModuloAlong`.
///
/// Note: the upstream `TypeDescription` attribute is parsed by Java but the
/// crate's `ModuloAnnotation` struct has no matching field, so it is ignored.
fn parse_modulo_element(scope_xml: &str, dim: &str) -> Option<crate::metadata::ModuloAnnotation> {
    let tag_name = format!("ModuloAlong{}", dim);
    let pos = all_tag_positions(scope_xml, &tag_name).into_iter().next()?;
    let t = start_tag_at(scope_xml, pos);

    // Collect explicit <Label> children within this ModuloAlong* element.
    let elem_start = pos + t.len();
    let elem_end = find_end_tag(scope_xml, &tag_name, elem_start).unwrap_or(elem_start);
    let elem_body = scope_xml.get(elem_start..elem_end).unwrap_or("");
    let mut labels = Vec::new();
    for lpos in all_tag_positions(elem_body, "Label") {
        let ltag = start_tag_at(elem_body, lpos);
        let lstart = lpos + ltag.len();
        if let Some(lend) = find_end_tag(elem_body, "Label", lstart) {
            labels.push(xml_unescape(elem_body[lstart..lend].trim()));
        }
    }

    Some(crate::metadata::ModuloAnnotation {
        parent_dimension: dim.to_string(),
        modulo_type: xml_attr(t, "Type").unwrap_or_default(),
        start: xml_attr(t, "Start")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0),
        step: xml_attr(t, "Step")
            .and_then(|s| s.parse().ok())
            .unwrap_or(1.0),
        end: xml_attr(t, "End")
            .and_then(|s| s.parse().ok())
            .unwrap_or(0.0),
        unit: xml_attr(t, "Unit").unwrap_or_default(),
        labels,
    })
}

/// Locate a `ModuloAlong{dim}` annotation for one OME `<Image>`.
///
/// In OME-XML the `ModuloAlongZ/C/T` blocks live in
/// `<StructuredAnnotations><XMLAnnotation><Value><Modulo>…</Modulo></Value>`
/// and are linked to the image via `<AnnotationRef ID="…"/>`, mirroring
/// `OMEXMLServiceImpl.getModuloAlong`. This resolves those references against
/// the full document. For backward compatibility (and inline/non-standard
/// placements), it also falls back to searching the image's own XML.
///
/// `full_xml` is the complete OME-XML document; `image_xml` is the slice
/// covering a single `<Image>…</Image>` element. `dim` is `"Z"`, `"C"`, or
/// `"T"`.
pub fn parse_modulo(
    full_xml: &str,
    image_xml: &str,
    dim: &str,
) -> Option<crate::metadata::ModuloAnnotation> {
    // 1) Resolve via StructuredAnnotations linked by AnnotationRef.
    let annotation_ids: Vec<String> = all_tag_positions(image_xml, "AnnotationRef")
        .into_iter()
        .filter_map(|p| xml_attr(start_tag_at(image_xml, p), "ID"))
        .collect();

    if !annotation_ids.is_empty() {
        if let Some(sa_pos) = all_tag_positions(full_xml, "StructuredAnnotations")
            .into_iter()
            .next()
        {
            let sa_start = sa_pos + start_tag_at(full_xml, sa_pos).len();
            let sa_end =
                find_end_tag(full_xml, "StructuredAnnotations", sa_start).unwrap_or(full_xml.len());
            let sa_xml = &full_xml[sa_pos..sa_end.max(sa_pos)];

            for ann_pos in all_tag_positions(sa_xml, "XMLAnnotation") {
                let ann_tag = start_tag_at(sa_xml, ann_pos);
                let ann_id = xml_attr(ann_tag, "ID");
                if ann_id
                    .as_deref()
                    .map(|id| annotation_ids.iter().any(|r| r == id))
                    != Some(true)
                {
                    continue;
                }
                let ann_body_start = ann_pos + ann_tag.len();
                let ann_body_end =
                    find_end_tag(sa_xml, "XMLAnnotation", ann_body_start).unwrap_or(sa_xml.len());
                let ann_xml = &sa_xml[ann_pos..ann_body_end.max(ann_pos)];
                if let Some(m) = parse_modulo_element(ann_xml, dim) {
                    return Some(m);
                }
            }
        }
    }

    // 2) Fallback: search the image's own XML (covers inline/legacy placement).
    parse_modulo_element(image_xml, dim)
}

fn parse_planes(pixels_xml: &str) -> Vec<OmePlane> {
    all_tag_positions(pixels_xml, "Plane")
        .into_iter()
        .map(|pos| {
            let tag = start_tag_at(pixels_xml, pos);
            OmePlane {
                the_z: xml_attr(tag, "TheZ")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                the_c: xml_attr(tag, "TheC")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                the_t: xml_attr(tag, "TheT")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                delta_t: xml_attr(tag, "DeltaT").and_then(|s| s.parse().ok()),
                exposure_time: xml_attr(tag, "ExposureTime").and_then(|s| s.parse().ok()),
                position_x: xml_attr(tag, "PositionX").and_then(|s| s.parse().ok()),
                position_y: xml_attr(tag, "PositionY").and_then(|s| s.parse().ok()),
                position_z: xml_attr(tag, "PositionZ").and_then(|s| s.parse().ok()),
            }
        })
        .collect()
}

// ─── Instrument parsing ──────────────────────────────────────────────────────

fn parse_instruments(xml: &str) -> Vec<OmeInstrument> {
    all_tag_positions(xml, "Instrument")
        .into_iter()
        .map(|pos| {
            let tag = start_tag_at(xml, pos);
            let id = xml_attr(tag, "ID");

            let inst_end = find_end_tag(xml, "Instrument", pos)
                .map(|e| xml[e..].find('>').map(|gt| e + gt + 1).unwrap_or(xml.len()))
                .unwrap_or(xml.len());
            let inst_xml = &xml[pos..inst_end];

            // Microscope
            let microscope_model = all_tag_positions(inst_xml, "Microscope")
                .into_iter()
                .next()
                .and_then(|p| xml_attr(start_tag_at(inst_xml, p), "Model"));
            let microscope_manufacturer = all_tag_positions(inst_xml, "Microscope")
                .into_iter()
                .next()
                .and_then(|p| xml_attr(start_tag_at(inst_xml, p), "Manufacturer"));

            // Objectives
            let objectives = all_tag_positions(inst_xml, "Objective")
                .into_iter()
                .map(|p| {
                    let t = start_tag_at(inst_xml, p);
                    OmeObjective {
                        id: xml_attr(t, "ID"),
                        model: xml_attr(t, "Model"),
                        manufacturer: xml_attr(t, "Manufacturer"),
                        nominal_magnification: xml_attr(t, "NominalMagnification")
                            .and_then(|s| s.parse().ok()),
                        calibrated_magnification: xml_attr(t, "CalibratedMagnification")
                            .and_then(|s| s.parse().ok()),
                        lens_na: xml_attr(t, "LensNA").and_then(|s| s.parse().ok()),
                        immersion: xml_attr(t, "Immersion"),
                        correction: xml_attr(t, "Correction"),
                        working_distance: xml_attr(t, "WorkingDistance")
                            .and_then(|s| s.parse().ok()),
                    }
                })
                .collect();

            // Detectors
            let detectors = all_tag_positions(inst_xml, "Detector")
                .into_iter()
                .map(|p| {
                    let t = start_tag_at(inst_xml, p);
                    OmeDetector {
                        id: xml_attr(t, "ID"),
                        model: xml_attr(t, "Model"),
                        manufacturer: xml_attr(t, "Manufacturer"),
                        detector_type: xml_attr(t, "Type"),
                        gain: xml_attr(t, "Gain").and_then(|s| s.parse().ok()),
                        offset: xml_attr(t, "Offset").and_then(|s| s.parse().ok()),
                    }
                })
                .collect();

            // Light sources (Laser, Arc, Filament, LightEmittingDiode, GenericExcitationSource)
            let mut light_sources = Vec::new();
            for ls_tag in &[
                "Laser",
                "Arc",
                "Filament",
                "LightEmittingDiode",
                "GenericExcitationSource",
            ] {
                for p in all_tag_positions(inst_xml, ls_tag) {
                    let t = start_tag_at(inst_xml, p);
                    light_sources.push(OmeLightSource {
                        id: xml_attr(t, "ID"),
                        model: xml_attr(t, "Model"),
                        manufacturer: xml_attr(t, "Manufacturer"),
                        light_source_type: Some(ls_tag.to_string()),
                        power: xml_attr(t, "Power").and_then(|s| s.parse().ok()),
                        wavelength: xml_attr(t, "Wavelength").and_then(|s| s.parse().ok()),
                    });
                }
            }

            // Filters
            let filters = all_tag_positions(inst_xml, "Filter")
                .into_iter()
                .map(|p| {
                    let t = start_tag_at(inst_xml, p);
                    OmeFilter {
                        id: xml_attr(t, "ID"),
                        model: xml_attr(t, "Model"),
                        manufacturer: xml_attr(t, "Manufacturer"),
                        filter_type: xml_attr(t, "Type"),
                        cut_in: xml_attr(t, "CutIn").and_then(|s| s.parse().ok()),
                        cut_out: xml_attr(t, "CutOut").and_then(|s| s.parse().ok()),
                    }
                })
                .collect();

            // Dichroics
            let dichroics = all_tag_positions(inst_xml, "Dichroic")
                .into_iter()
                .map(|p| {
                    let t = start_tag_at(inst_xml, p);
                    OmeDichroic {
                        id: xml_attr(t, "ID"),
                        model: xml_attr(t, "Model"),
                        manufacturer: xml_attr(t, "Manufacturer"),
                    }
                })
                .collect();

            OmeInstrument {
                id,
                microscope_model,
                microscope_manufacturer,
                objectives,
                detectors,
                light_sources,
                filters,
                dichroics,
            }
        })
        .collect()
}

// ─── ROI parsing ────────────────────────────────────────────────────────────

fn parse_rois(xml: &str) -> Vec<OmeROI> {
    all_tag_positions(xml, "ROI")
        .into_iter()
        .map(|pos| {
            let tag = start_tag_at(xml, pos);
            let id = xml_attr(tag, "ID");
            let name = xml_attr(tag, "Name");

            let roi_end = find_end_tag(xml, "ROI", pos)
                .map(|e| xml[e..].find('>').map(|gt| e + gt + 1).unwrap_or(xml.len()))
                .unwrap_or(xml.len());
            let roi_xml = &xml[pos..roi_end];

            // Find the <Union> block containing shapes
            let union_xml = {
                let u_start = all_tag_positions(roi_xml, "Union").into_iter().next();
                let u_end = u_start.and_then(|s| find_end_tag(roi_xml, "Union", s));
                match (u_start, u_end) {
                    (Some(s), Some(e)) => {
                        let end = roi_xml[e..]
                            .find('>')
                            .map(|gt| e + gt + 1)
                            .unwrap_or(roi_xml.len());
                        &roi_xml[s..end]
                    }
                    _ => roi_xml,
                }
            };

            let mut shapes = Vec::new();

            // Rectangle
            for sp in all_tag_positions(union_xml, "Rectangle") {
                let t = start_tag_at(union_xml, sp);
                let x = xml_attr(t, "X").and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let y = xml_attr(t, "Y").and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let w = xml_attr(t, "Width")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let h = xml_attr(t, "Height")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
                let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
                let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
                shapes.push(OmeShape::Rectangle {
                    x,
                    y,
                    width: w,
                    height: h,
                    the_z,
                    the_t,
                    the_c,
                });
            }

            // Ellipse
            for sp in all_tag_positions(union_xml, "Ellipse") {
                let t = start_tag_at(union_xml, sp);
                let x = xml_attr(t, "X").and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let y = xml_attr(t, "Y").and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let rx = xml_attr(t, "RadiusX")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let ry = xml_attr(t, "RadiusY")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
                let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
                let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
                shapes.push(OmeShape::Ellipse {
                    x,
                    y,
                    radius_x: rx,
                    radius_y: ry,
                    the_z,
                    the_t,
                    the_c,
                });
            }

            // Point
            for sp in all_tag_positions(union_xml, "Point") {
                let t = start_tag_at(union_xml, sp);
                let x = xml_attr(t, "X").and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let y = xml_attr(t, "Y").and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
                let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
                let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
                shapes.push(OmeShape::Point {
                    x,
                    y,
                    the_z,
                    the_t,
                    the_c,
                });
            }

            // Line
            for sp in all_tag_positions(union_xml, "Line") {
                let t = start_tag_at(union_xml, sp);
                let x1 = xml_attr(t, "X1")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let y1 = xml_attr(t, "Y1")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let x2 = xml_attr(t, "X2")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let y2 = xml_attr(t, "Y2")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
                let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
                let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
                shapes.push(OmeShape::Line {
                    x1,
                    y1,
                    x2,
                    y2,
                    the_z,
                    the_t,
                    the_c,
                });
            }

            // Polygon
            for sp in all_tag_positions(union_xml, "Polygon") {
                let t = start_tag_at(union_xml, sp);
                let points = parse_points_attr(xml_attr(t, "Points").as_deref().unwrap_or(""));
                let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
                let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
                let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
                shapes.push(OmeShape::Polygon {
                    points,
                    the_z,
                    the_t,
                    the_c,
                });
            }

            // Polyline
            for sp in all_tag_positions(union_xml, "Polyline") {
                let t = start_tag_at(union_xml, sp);
                let points = parse_points_attr(xml_attr(t, "Points").as_deref().unwrap_or(""));
                let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
                let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
                let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
                shapes.push(OmeShape::Polyline {
                    points,
                    the_z,
                    the_t,
                    the_c,
                });
            }

            OmeROI { id, name, shapes }
        })
        .collect()
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
    let sa_start = match all_tag_positions(xml, "StructuredAnnotations")
        .into_iter()
        .next()
    {
        Some(p) => p,
        None => return Vec::new(),
    };
    let sa_end = find_end_tag(xml, "StructuredAnnotations", sa_start)
        .map(|e| xml[e..].find('>').map(|gt| e + gt + 1).unwrap_or(xml.len()))
        .unwrap_or(xml.len());
    let sa_xml = &xml[sa_start..sa_end];

    let mut annotations = Vec::new();

    // MapAnnotation
    for pos in all_tag_positions(sa_xml, "MapAnnotation") {
        let tag = start_tag_at(sa_xml, pos);
        let id = xml_attr(tag, "ID");
        let namespace = xml_attr(tag, "Namespace");
        let ann_end = find_end_tag(sa_xml, "MapAnnotation", pos)
            .map(|e| {
                sa_xml[e..]
                    .find('>')
                    .map(|gt| e + gt + 1)
                    .unwrap_or(sa_xml.len())
            })
            .unwrap_or(sa_xml.len());
        let ann_xml = &sa_xml[pos..ann_end];

        // Parse <Value><M K="key">value</M></Value> entries
        let mut values = Vec::new();
        for m_pos in all_tag_positions(ann_xml, "M") {
            let m_tag = start_tag_at(ann_xml, m_pos);
            if let Some(key) = xml_attr(m_tag, "K") {
                // Get inner text between > and </M>
                let after_tag = m_pos + m_tag.len();
                if let Some(close) = find_end_tag(ann_xml, "M", after_tag) {
                    let val = xml_unescape(ann_xml[after_tag..close].trim());
                    values.push((key, val));
                }
            }
        }
        annotations.push(OmeAnnotation::MapAnnotation {
            id,
            namespace,
            values,
        });
    }

    // CommentAnnotation
    for pos in all_tag_positions(sa_xml, "CommentAnnotation") {
        let tag = start_tag_at(sa_xml, pos);
        let id = xml_attr(tag, "ID");
        let namespace = xml_attr(tag, "Namespace");
        let ann_end = find_end_tag(sa_xml, "CommentAnnotation", pos)
            .map(|e| {
                sa_xml[e..]
                    .find('>')
                    .map(|gt| e + gt + 1)
                    .unwrap_or(sa_xml.len())
            })
            .unwrap_or(sa_xml.len());
        let ann_xml = &sa_xml[pos..ann_end];
        let value = xml_inner_text(ann_xml, "Value").unwrap_or_default();
        annotations.push(OmeAnnotation::CommentAnnotation {
            id,
            namespace,
            value,
        });
    }

    // TagAnnotation
    for pos in all_tag_positions(sa_xml, "TagAnnotation") {
        let tag = start_tag_at(sa_xml, pos);
        let id = xml_attr(tag, "ID");
        let namespace = xml_attr(tag, "Namespace");
        let ann_end = find_end_tag(sa_xml, "TagAnnotation", pos)
            .map(|e| {
                sa_xml[e..]
                    .find('>')
                    .map(|gt| e + gt + 1)
                    .unwrap_or(sa_xml.len())
            })
            .unwrap_or(sa_xml.len());
        let ann_xml = &sa_xml[pos..ann_end];
        let value = xml_inner_text(ann_xml, "Value").unwrap_or_default();
        annotations.push(OmeAnnotation::TagAnnotation {
            id,
            namespace,
            value,
        });
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
    let block_end = lower[start..]
        .find("</distance>")
        .map(|p| p + start)
        .unwrap_or(xml.len());
    let metres: f64 = xml_inner_text(&xml[start..block_end], "Value")?
        .trim()
        .parse()
        .ok()?;
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
        let end = lower[start..]
            .find(close)
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
        let emission =
            xml_inner_text(block, "EmissionWavelength").and_then(|s| s.trim().parse().ok());
        let excitation =
            xml_inner_text(block, "ExcitationWavelength").and_then(|s| s.trim().parse().ok());
        if name.is_some() || color.is_some() {
            channels.push(OmeChannel {
                name,
                samples_per_pixel: 1,
                color,
                emission_wavelength: emission,
                excitation_wavelength: excitation,
                ..Default::default()
            });
        }
        pos = end;
    }
    channels
}

fn metadata_value_to_string(value: &MetadataValue) -> String {
    match value {
        MetadataValue::String(v) => v.clone(),
        MetadataValue::Int(v) => v.to_string(),
        MetadataValue::Float(v) => v.to_string(),
        MetadataValue::Bool(v) => v.to_string(),
        MetadataValue::Bytes(v) => format!("<{} bytes>", v.len()),
    }
}

fn metadata_value_f64(value: Option<&MetadataValue>) -> Option<f64> {
    match value {
        Some(MetadataValue::Float(v)) => Some(*v),
        Some(MetadataValue::Int(v)) => Some(*v as f64),
        Some(MetadataValue::String(v)) => v.trim().parse::<f64>().ok(),
        _ => None,
    }
}

fn metadata_value_u32(value: Option<&MetadataValue>) -> Option<u32> {
    match value {
        Some(MetadataValue::Int(v)) => u32::try_from(*v).ok(),
        Some(MetadataValue::Float(v)) if v.is_finite() && v.fract() == 0.0 => {
            u32::try_from(*v as i64).ok()
        }
        Some(MetadataValue::String(v)) => v.trim().parse::<u32>().ok(),
        _ => None,
    }
}

fn metadata_value_string(value: Option<&MetadataValue>) -> Option<String> {
    match value {
        Some(MetadataValue::String(v)) => {
            let trimmed = v.trim();
            (!trimmed.is_empty()).then(|| trimmed.to_string())
        }
        _ => None,
    }
}

fn metadata_value_string_list(value: Option<&MetadataValue>) -> Vec<String> {
    let Some(value) = value else {
        return Vec::new();
    };
    match value {
        MetadataValue::String(v) => v
            .split([',', ';'])
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(ToString::to_string)
            .collect(),
        MetadataValue::Int(v) => vec![v.to_string()],
        _ => Vec::new(),
    }
}

fn metadata_by_suffix<'a>(
    metadata: &'a std::collections::HashMap<String, MetadataValue>,
    suffixes: &[&str],
) -> Option<&'a MetadataValue> {
    metadata_by_suffix_filtered(metadata, suffixes, &[])
}

fn metadata_by_suffix_filtered<'a>(
    metadata: &'a std::collections::HashMap<String, MetadataValue>,
    suffixes: &[&str],
    excluded_prefixes: &[&str],
) -> Option<&'a MetadataValue> {
    for suffix in suffixes {
        if let Some(value) = metadata.get(*suffix) {
            return Some(value);
        }
    }

    let mut keys: Vec<&str> = metadata.keys().map(String::as_str).collect();
    keys.sort_unstable();
    for suffix in suffixes {
        let dotted_suffix = format!(".{suffix}");
        for key in &keys {
            if excluded_prefixes
                .iter()
                .any(|prefix| key.starts_with(prefix))
            {
                continue;
            }
            if key.ends_with(&dotted_suffix) {
                return metadata.get(*key);
            }
        }
    }
    None
}

fn metadata_string_by_suffix(
    metadata: &std::collections::HashMap<String, MetadataValue>,
    suffixes: &[&str],
) -> Option<String> {
    metadata_value_string(metadata_by_suffix(metadata, suffixes))
}

fn metadata_positive_f64_by_suffix(
    metadata: &std::collections::HashMap<String, MetadataValue>,
    suffixes: &[&str],
) -> Option<f64> {
    metadata_value_f64(metadata_by_suffix(metadata, suffixes))
        .filter(|value| value.is_finite() && *value > 0.0)
}

fn metadata_string_by_suffix_filtered(
    metadata: &std::collections::HashMap<String, MetadataValue>,
    suffixes: &[&str],
    excluded_prefixes: &[&str],
) -> Option<String> {
    metadata_value_string(metadata_by_suffix_filtered(
        metadata,
        suffixes,
        excluded_prefixes,
    ))
}

fn generic_image_name_from_metadata(meta: &ImageMetadata) -> Option<String> {
    metadata_string_by_suffix(
        &meta.series_metadata,
        &[
            "image.name",
            "image_name",
            "series.name",
            "series_name",
            "name",
        ],
    )
}

fn generic_image_description_from_metadata(meta: &ImageMetadata) -> Option<String> {
    metadata_string_by_suffix(
        &meta.series_metadata,
        &[
            "image.description",
            "image_description",
            "series.description",
            "series_description",
            "description",
        ],
    )
}

fn generic_acquisition_date_from_metadata(meta: &ImageMetadata) -> Option<String> {
    metadata_string_by_suffix(
        &meta.series_metadata,
        &[
            "acquisition_date",
            "acquisition.datetime",
            "acquisition.date",
            "acquisition_datetime_iso8601",
            "acquisition_datetime",
        ],
    )
}

fn metadata_positive_f64_by_suffix_filtered(
    metadata: &std::collections::HashMap<String, MetadataValue>,
    suffixes: &[&str],
    excluded_prefixes: &[&str],
) -> Option<f64> {
    metadata_value_f64(metadata_by_suffix_filtered(
        metadata,
        suffixes,
        excluded_prefixes,
    ))
    .filter(|value| value.is_finite() && *value > 0.0)
}

fn metadata_finite_f64_by_suffix(
    metadata: &std::collections::HashMap<String, MetadataValue>,
    suffixes: &[&str],
) -> Option<f64> {
    metadata_value_f64(metadata_by_suffix(metadata, suffixes)).filter(|value| value.is_finite())
}

fn metadata_u32_by_suffix(
    metadata: &std::collections::HashMap<String, MetadataValue>,
    suffixes: &[&str],
) -> Option<u32> {
    metadata_value_u32(metadata_by_suffix(metadata, suffixes))
}

fn rgb_channel_count(meta: &ImageMetadata) -> u32 {
    if !meta.is_rgb {
        return 1;
    }
    let zt = meta.size_z.max(1).saturating_mul(meta.size_t.max(1));
    if zt > 0 && meta.image_count >= zt {
        let effective_c = (meta.image_count / zt).max(1);
        if effective_c > 0 && meta.size_c >= effective_c && meta.size_c % effective_c == 0 {
            return (meta.size_c / effective_c).max(1);
        }
    }
    meta.size_c.max(1)
}

fn effective_size_c(meta: &ImageMetadata) -> u32 {
    if meta.is_rgb {
        (meta.size_c / rgb_channel_count(meta)).max(1)
    } else {
        meta.size_c.max(1)
    }
}

fn generic_planes_from_metadata(meta: &ImageMetadata) -> Vec<OmePlane> {
    let mut planes = Vec::new();
    for plane_index in 0..meta.image_count {
        let prefix = format!("plane.{plane_index}");
        let delta_t = metadata_finite_f64_by_suffix(
            &meta.series_metadata,
            &[
                &format!("{prefix}.delta_t"),
                &format!("{prefix}.delta_time"),
                &format!("{prefix}.timestamp"),
                &format!("{prefix}.time"),
            ],
        );
        let exposure_time = metadata_positive_f64_by_suffix(
            &meta.series_metadata,
            &[
                &format!("{prefix}.exposure_time"),
                &format!("{prefix}.exposure"),
                &format!("{prefix}.integration_time"),
            ],
        );
        let position_x = metadata_finite_f64_by_suffix(
            &meta.series_metadata,
            &[
                &format!("{prefix}.position_x"),
                &format!("{prefix}.stage_x"),
                &format!("{prefix}.x_position"),
            ],
        );
        let position_y = metadata_finite_f64_by_suffix(
            &meta.series_metadata,
            &[
                &format!("{prefix}.position_y"),
                &format!("{prefix}.stage_y"),
                &format!("{prefix}.y_position"),
            ],
        );
        let position_z = metadata_finite_f64_by_suffix(
            &meta.series_metadata,
            &[
                &format!("{prefix}.position_z"),
                &format!("{prefix}.stage_z"),
                &format!("{prefix}.z_position"),
            ],
        );

        if delta_t.is_some()
            || exposure_time.is_some()
            || position_x.is_some()
            || position_y.is_some()
            || position_z.is_some()
        {
            let c_size = effective_size_c(meta);
            let z_size = meta.size_z.max(1);
            planes.push(OmePlane {
                the_z: (plane_index / c_size) % z_size,
                the_c: plane_index % c_size,
                the_t: plane_index / (c_size * z_size),
                delta_t,
                exposure_time,
                position_x,
                position_y,
                position_z,
            });
        }
    }
    planes
}

fn generic_light_paths_from_metadata(
    meta: &ImageMetadata,
    channel_count: usize,
) -> Vec<OmeLightPath> {
    let metadata = &meta.series_metadata;
    let mut paths = Vec::new();
    for channel_index in 0..channel_count {
        let prefix = format!("channel.{channel_index}");
        let excitation_filter_ids: Vec<String> = metadata_value_string_list(metadata_by_suffix(
            metadata,
            &[
                &format!("{prefix}.excitation_filter_id"),
                &format!("{prefix}.excitation_filter_ref"),
                &format!("{prefix}.excitation_filter"),
            ],
        ))
        .into_iter()
        .map(|id| normalize_ome_ref_id("Filter", &id))
        .collect();
        let emission_filter_ids: Vec<String> = metadata_value_string_list(metadata_by_suffix(
            metadata,
            &[
                &format!("{prefix}.emission_filter_id"),
                &format!("{prefix}.emission_filter_ref"),
                &format!("{prefix}.emission_filter"),
            ],
        ))
        .into_iter()
        .map(|id| normalize_ome_ref_id("Filter", &id))
        .collect();
        let dichroic_id = metadata_value_string(metadata_by_suffix(
            metadata,
            &[
                &format!("{prefix}.dichroic_id"),
                &format!("{prefix}.dichroic_ref"),
                &format!("{prefix}.dichroic"),
            ],
        ))
        .map(|id| normalize_ome_ref_id("Dichroic", &id));

        if !excitation_filter_ids.is_empty()
            || dichroic_id.is_some()
            || !emission_filter_ids.is_empty()
        {
            paths.resize_with(channel_index, OmeLightPath::default);
            paths.push(OmeLightPath {
                excitation_filter_ids,
                dichroic_id,
                emission_filter_ids,
            });
        }
    }
    paths
}

fn generic_rois_from_metadata(meta: &ImageMetadata) -> Vec<OmeROI> {
    let mut indices = std::collections::BTreeSet::new();
    for key in meta.series_metadata.keys() {
        if key.starts_with("xlef.lms.") {
            continue;
        }
        let parts: Vec<&str> = key.split('.').collect();
        for pair in parts.windows(2) {
            if pair[0] == "roi" {
                if let Ok(index) = pair[1].parse::<usize>() {
                    indices.insert(index);
                }
            }
        }
    }

    let mut rois = Vec::new();
    for index in indices {
        let prefix = format!("roi.{index}");
        let name = metadata_string_by_suffix(
            &meta.series_metadata,
            &[&format!("{prefix}.name"), &format!("{prefix}.label")],
        );
        let shape = generic_roi_shape_from_metadata(meta, &prefix);
        if name.is_some() || shape.is_some() {
            rois.push(OmeROI {
                id: Some(create_lsid("ROI", &[index])),
                name,
                shapes: shape.into_iter().collect(),
            });
        }
    }
    rois
}

fn generic_roi_shape_from_metadata(meta: &ImageMetadata, prefix: &str) -> Option<OmeShape> {
    let metadata = &meta.series_metadata;
    let x = metadata_finite_f64_by_suffix(
        metadata,
        &[
            &format!("{prefix}.x"),
            &format!("{prefix}.left"),
            &format!("{prefix}.pos_x"),
            &format!("{prefix}.center_x"),
            &format!("{prefix}.centerx"),
            &format!("{prefix}.x1"),
        ],
    )?;
    let y = metadata_finite_f64_by_suffix(
        metadata,
        &[
            &format!("{prefix}.y"),
            &format!("{prefix}.top"),
            &format!("{prefix}.pos_y"),
            &format!("{prefix}.center_y"),
            &format!("{prefix}.centery"),
            &format!("{prefix}.y1"),
        ],
    )?;
    let the_z = metadata_u32_by_suffix(
        metadata,
        &[
            &format!("{prefix}.the_z"),
            &format!("{prefix}.thez"),
            &format!("{prefix}.z_index"),
            &format!("{prefix}.zindex"),
        ],
    );
    let the_t = metadata_u32_by_suffix(
        metadata,
        &[
            &format!("{prefix}.the_t"),
            &format!("{prefix}.thet"),
            &format!("{prefix}.t_index"),
            &format!("{prefix}.tindex"),
        ],
    );
    let the_c = metadata_u32_by_suffix(
        metadata,
        &[
            &format!("{prefix}.the_c"),
            &format!("{prefix}.thec"),
            &format!("{prefix}.c_index"),
            &format!("{prefix}.cindex"),
        ],
    );

    let width = metadata_positive_f64_by_suffix(
        metadata,
        &[
            &format!("{prefix}.width"),
            &format!("{prefix}.w"),
            &format!("{prefix}.size_x"),
        ],
    );
    let height = metadata_positive_f64_by_suffix(
        metadata,
        &[
            &format!("{prefix}.height"),
            &format!("{prefix}.h"),
            &format!("{prefix}.size_y"),
        ],
    );

    let x2 = metadata_finite_f64_by_suffix(
        metadata,
        &[
            &format!("{prefix}.x2"),
            &format!("{prefix}.end_x"),
            &format!("{prefix}.endx"),
        ],
    );
    let y2 = metadata_finite_f64_by_suffix(
        metadata,
        &[
            &format!("{prefix}.y2"),
            &format!("{prefix}.end_y"),
            &format!("{prefix}.endy"),
        ],
    );
    if let (Some(x2), Some(y2)) = (x2, y2) {
        return Some(OmeShape::Line {
            x1: x,
            y1: y,
            x2,
            y2,
            the_z,
            the_t,
            the_c,
        });
    }

    let radius_x = metadata_positive_f64_by_suffix(
        metadata,
        &[
            &format!("{prefix}.radius_x"),
            &format!("{prefix}.radiusx"),
            &format!("{prefix}.rx"),
        ],
    );
    let radius_y = metadata_positive_f64_by_suffix(
        metadata,
        &[
            &format!("{prefix}.radius_y"),
            &format!("{prefix}.radiusy"),
            &format!("{prefix}.ry"),
        ],
    );
    if let (Some(radius_x), Some(radius_y)) = (radius_x, radius_y) {
        return Some(OmeShape::Ellipse {
            x,
            y,
            radius_x,
            radius_y,
            the_z,
            the_t,
            the_c,
        });
    }

    if let (Some(width), Some(height)) = (width, height) {
        return Some(OmeShape::Rectangle {
            x,
            y,
            width,
            height,
            the_z,
            the_t,
            the_c,
        });
    }

    if let (Some(right), Some(bottom)) = (
        metadata_finite_f64_by_suffix(metadata, &[&format!("{prefix}.right")]),
        metadata_finite_f64_by_suffix(metadata, &[&format!("{prefix}.bottom")]),
    ) {
        let width = right - x;
        let height = bottom - y;
        if width > 0.0 && height > 0.0 {
            return Some(OmeShape::Rectangle {
                x,
                y,
                width,
                height,
                the_z,
                the_t,
                the_c,
            });
        }
    }

    Some(OmeShape::Point {
        x,
        y,
        the_z,
        the_t,
        the_c,
    })
}

fn normalize_ome_ref_id(object_type: &str, value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.contains(':') {
        trimmed.to_string()
    } else if let Ok(index) = trimmed.parse::<usize>() {
        create_lsid(object_type, &[0, index])
    } else {
        trimmed.to_string()
    }
}

fn generic_objective_from_metadata(meta: &ImageMetadata) -> Option<OmeObjective> {
    let metadata = &meta.series_metadata;
    let excluded_prefixes = ["xlef.lms."];
    let objective = OmeObjective {
        id: Some(create_lsid("Objective", &[0, 0])),
        model: metadata_string_by_suffix_filtered(
            metadata,
            &[
                "objective.model",
                "objective.name",
                "objective.0.model",
                "objective.0.name",
            ],
            &excluded_prefixes,
        ),
        manufacturer: metadata_string_by_suffix_filtered(
            metadata,
            &["objective.manufacturer", "objective.0.manufacturer"],
            &excluded_prefixes,
        ),
        nominal_magnification: metadata_positive_f64_by_suffix_filtered(
            metadata,
            &[
                "objective.magnification",
                "objective.nominal_magnification",
                "objective.0.magnification",
                "objective.0.nominal_magnification",
            ],
            &excluded_prefixes,
        ),
        calibrated_magnification: metadata_positive_f64_by_suffix_filtered(
            metadata,
            &[
                "objective.calibrated_magnification",
                "objective.0.calibrated_magnification",
            ],
            &excluded_prefixes,
        ),
        lens_na: metadata_positive_f64_by_suffix_filtered(
            metadata,
            &[
                "objective.lens_na",
                "objective.na",
                "objective.numerical_aperture",
                "objective.0.lens_na",
                "objective.0.na",
                "objective.0.numerical_aperture",
            ],
            &excluded_prefixes,
        ),
        immersion: metadata_string_by_suffix_filtered(
            metadata,
            &["objective.immersion", "objective.0.immersion"],
            &excluded_prefixes,
        ),
        correction: metadata_string_by_suffix_filtered(
            metadata,
            &["objective.correction", "objective.0.correction"],
            &excluded_prefixes,
        ),
        working_distance: metadata_positive_f64_by_suffix_filtered(
            metadata,
            &["objective.working_distance", "objective.0.working_distance"],
            &excluded_prefixes,
        ),
    };

    (objective.model.is_some()
        || objective.manufacturer.is_some()
        || objective.nominal_magnification.is_some()
        || objective.calibrated_magnification.is_some()
        || objective.lens_na.is_some()
        || objective.immersion.is_some()
        || objective.correction.is_some()
        || objective.working_distance.is_some())
    .then_some(objective)
}

fn generic_detector_from_metadata(meta: &ImageMetadata) -> Option<OmeDetector> {
    let metadata = &meta.series_metadata;
    let excluded_prefixes = ["xlef.lms."];
    let detector = OmeDetector {
        id: Some(create_lsid("Detector", &[0, 0])),
        model: metadata_string_by_suffix_filtered(
            metadata,
            &[
                "detector.model",
                "detector.name",
                "detector.0.model",
                "detector.0.name",
            ],
            &excluded_prefixes,
        ),
        manufacturer: metadata_string_by_suffix_filtered(
            metadata,
            &["detector.manufacturer", "detector.0.manufacturer"],
            &excluded_prefixes,
        ),
        detector_type: metadata_string_by_suffix_filtered(
            metadata,
            &["detector.type", "detector.detector_type", "detector.0.type"],
            &excluded_prefixes,
        ),
        gain: metadata_positive_f64_by_suffix_filtered(
            metadata,
            &["detector.gain", "detector.0.gain"],
            &excluded_prefixes,
        ),
        offset: metadata_value_f64(metadata_by_suffix_filtered(
            metadata,
            &["detector.offset", "detector.0.offset"],
            &excluded_prefixes,
        ))
        .filter(|value| value.is_finite()),
    };

    (detector.model.is_some()
        || detector.manufacturer.is_some()
        || detector.detector_type.is_some()
        || detector.gain.is_some()
        || detector.offset.is_some())
    .then_some(detector)
}

fn generic_light_source_from_metadata(meta: &ImageMetadata) -> Option<OmeLightSource> {
    let metadata = &meta.series_metadata;
    let excluded_prefixes = ["xlef.lms."];
    let light_source = OmeLightSource {
        id: Some(create_lsid("LightSource", &[0, 0])),
        model: metadata_string_by_suffix_filtered(
            metadata,
            &[
                "light_source.model",
                "light_source.name",
                "light_source.0.model",
                "light_source.0.name",
                "lightsource.model",
                "lightsource.name",
                "laser.model",
                "laser.name",
                "illumination.model",
                "illumination.name",
                "illumination.0.model",
                "illumination.0.name",
            ],
            &excluded_prefixes,
        ),
        manufacturer: metadata_string_by_suffix_filtered(
            metadata,
            &[
                "light_source.manufacturer",
                "light_source.0.manufacturer",
                "lightsource.manufacturer",
                "laser.manufacturer",
                "illumination.manufacturer",
                "illumination.0.manufacturer",
            ],
            &excluded_prefixes,
        ),
        light_source_type: metadata_string_by_suffix_filtered(
            metadata,
            &[
                "light_source.type",
                "light_source.light_source_type",
                "light_source.0.type",
                "light_source.0.light_source_type",
                "lightsource.type",
                "laser.type",
                "illumination.type",
                "illumination.0.type",
            ],
            &excluded_prefixes,
        ),
        power: metadata_positive_f64_by_suffix_filtered(
            metadata,
            &[
                "light_source.power",
                "light_source.0.power",
                "lightsource.power",
                "laser.power",
                "illumination.power",
                "illumination.0.power",
            ],
            &excluded_prefixes,
        ),
        wavelength: metadata_positive_f64_by_suffix_filtered(
            metadata,
            &[
                "light_source.wavelength",
                "light_source.0.wavelength",
                "lightsource.wavelength",
                "laser.wavelength",
                "illumination.wavelength",
                "illumination.0.wavelength",
            ],
            &excluded_prefixes,
        ),
    };

    (light_source.model.is_some()
        || light_source.manufacturer.is_some()
        || light_source.light_source_type.is_some()
        || light_source.power.is_some())
    .then_some(light_source)
}

fn generic_filter_from_metadata(meta: &ImageMetadata) -> Option<OmeFilter> {
    let metadata = &meta.series_metadata;
    let excluded_prefixes = ["xlef.lms."];
    let filter = OmeFilter {
        id: Some(create_lsid("Filter", &[0, 0])),
        model: metadata_string_by_suffix_filtered(
            metadata,
            &[
                "filter.model",
                "filter.name",
                "filter.0.model",
                "filter.0.name",
            ],
            &excluded_prefixes,
        ),
        manufacturer: metadata_string_by_suffix_filtered(
            metadata,
            &["filter.manufacturer", "filter.0.manufacturer"],
            &excluded_prefixes,
        ),
        filter_type: metadata_string_by_suffix_filtered(
            metadata,
            &["filter.type", "filter.filter_type", "filter.0.type"],
            &excluded_prefixes,
        ),
        cut_in: metadata_positive_f64_by_suffix_filtered(
            metadata,
            &[
                "filter.cut_in",
                "filter.cut_in_wavelength",
                "filter.0.cut_in",
                "filter.0.cut_in_wavelength",
            ],
            &excluded_prefixes,
        ),
        cut_out: metadata_positive_f64_by_suffix_filtered(
            metadata,
            &[
                "filter.cut_out",
                "filter.cut_out_wavelength",
                "filter.0.cut_out",
                "filter.0.cut_out_wavelength",
            ],
            &excluded_prefixes,
        ),
    };

    (filter.model.is_some()
        || filter.manufacturer.is_some()
        || filter.filter_type.is_some()
        || filter.cut_in.is_some()
        || filter.cut_out.is_some())
    .then_some(filter)
}

fn generic_dichroic_from_metadata(meta: &ImageMetadata) -> Option<OmeDichroic> {
    let metadata = &meta.series_metadata;
    let excluded_prefixes = ["xlef.lms."];
    let dichroic = OmeDichroic {
        id: Some(create_lsid("Dichroic", &[0, 0])),
        model: metadata_string_by_suffix_filtered(
            metadata,
            &[
                "dichroic.model",
                "dichroic.name",
                "dichroic.0.model",
                "dichroic.0.name",
            ],
            &excluded_prefixes,
        ),
        manufacturer: metadata_string_by_suffix_filtered(
            metadata,
            &["dichroic.manufacturer", "dichroic.0.manufacturer"],
            &excluded_prefixes,
        ),
    };

    (dichroic.model.is_some() || dichroic.manufacturer.is_some()).then_some(dichroic)
}

fn generic_experimenter_from_metadata(meta: &ImageMetadata) -> Option<OmeExperimenter> {
    let metadata = &meta.series_metadata;
    let experimenter = OmeExperimenter {
        id: Some(create_lsid("Experimenter", &[0])),
        first_name: metadata_string_by_suffix(
            metadata,
            &[
                "experimenter.first_name",
                "experimenter.firstname",
                "experimenter.0.first_name",
                "experimenter.0.firstname",
                "user.first_name",
                "user.firstname",
                "operator.first_name",
                "operator.firstname",
            ],
        ),
        last_name: metadata_string_by_suffix(
            metadata,
            &[
                "experimenter.last_name",
                "experimenter.lastname",
                "experimenter.0.last_name",
                "experimenter.0.lastname",
                "user.last_name",
                "user.lastname",
                "operator.last_name",
                "operator.lastname",
            ],
        ),
        email: metadata_string_by_suffix(
            metadata,
            &[
                "experimenter.email",
                "experimenter.0.email",
                "user.email",
                "operator.email",
            ],
        ),
        institution: metadata_string_by_suffix(
            metadata,
            &[
                "experimenter.institution",
                "experimenter.0.institution",
                "user.institution",
                "operator.institution",
            ],
        ),
    };

    (experimenter.first_name.is_some()
        || experimenter.last_name.is_some()
        || experimenter.email.is_some()
        || experimenter.institution.is_some())
    .then_some(experimenter)
}

// ─── Low-level XML primitives ─────────────────────────────────────────────────

/// Extract the value of `attr` from an XML start-tag string (case-insensitive).
fn xml_attr(tag_text: &str, attr: &str) -> Option<String> {
    let attr_lc = attr.to_ascii_lowercase();
    let mut pos = tag_text.find(char::is_whitespace)?;
    let bytes = tag_text.as_bytes();

    while pos < tag_text.len() {
        while pos < tag_text.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= tag_text.len() || bytes[pos] == b'>' || bytes[pos] == b'/' {
            break;
        }

        let name_start = pos;
        while pos < tag_text.len() {
            let b = bytes[pos];
            if b == b'=' || b.is_ascii_whitespace() || b == b'>' || b == b'/' {
                break;
            }
            pos += 1;
        }
        let name = &tag_text[name_start..pos];

        while pos < tag_text.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= tag_text.len() || bytes[pos] != b'=' {
            while pos < tag_text.len() && bytes[pos] != b'>' && !bytes[pos].is_ascii_whitespace() {
                pos += 1;
            }
            continue;
        }
        pos += 1;
        while pos < tag_text.len() && bytes[pos].is_ascii_whitespace() {
            pos += 1;
        }
        if pos >= tag_text.len() {
            break;
        }

        let value = if bytes[pos] == b'"' || bytes[pos] == b'\'' {
            let quote = bytes[pos];
            pos += 1;
            let value_start = pos;
            while pos < tag_text.len() && bytes[pos] != quote {
                pos += 1;
            }
            let raw = &tag_text[value_start..pos];
            pos = (pos + 1).min(tag_text.len());
            raw
        } else {
            let value_start = pos;
            while pos < tag_text.len()
                && !bytes[pos].is_ascii_whitespace()
                && bytes[pos] != b'>'
                && bytes[pos] != b'/'
            {
                pos += 1;
            }
            &tag_text[value_start..pos]
        };

        if local_name(name).eq_ignore_ascii_case(&attr_lc) {
            return Some(xml_unescape(value));
        }
    }
    None
}

/// Return the start-tag string beginning at `pos` (from `<` up to and including `>`).
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

/// Find the trimmed text content of the first `<tag>…</tag>` (case-insensitive).
fn xml_inner_text(xml: &str, tag: &str) -> Option<String> {
    let tag_start = all_tag_positions(xml, tag).into_iter().next()?;
    let content_start = tag_start + start_tag_at(xml, tag_start).len();
    let content_end = find_end_tag(xml, tag, content_start)?;
    Some(xml_unescape(xml[content_start..content_end].trim()))
}

/// Return byte positions of every `<tag` occurrence (case-insensitive),
/// being careful not to match longer tag names (e.g. `<Channel` vs `<Channels`).
fn all_tag_positions(xml: &str, tag: &str) -> Vec<usize> {
    let tag_lc = tag.to_ascii_lowercase();
    let mut positions = Vec::new();
    let mut pos = 0;
    while let Some(rel) = xml[pos..].find('<') {
        let abs = pos + rel;
        if abs + 1 >= xml.len() {
            break;
        }
        let next = xml.as_bytes()[abs + 1];
        if next == b'/' || next == b'!' || next == b'?' {
            pos = abs + 1;
            continue;
        }
        let name_start = abs + 1;
        let mut name_end = name_start;
        while name_end < xml.len() {
            let b = xml.as_bytes()[name_end];
            if b == b'>' || b == b'/' || b.is_ascii_whitespace() {
                break;
            }
            name_end += 1;
        }
        if local_name(&xml[name_start..name_end]).eq_ignore_ascii_case(&tag_lc) {
            positions.push(abs);
        }
        pos = abs + 1;
    }
    positions
}

fn find_end_tag(xml: &str, tag: &str, start: usize) -> Option<usize> {
    let tag_lc = tag.to_ascii_lowercase();
    let mut pos = start;
    while let Some(rel) = xml[pos..].find("</") {
        let abs = pos + rel;
        let name_start = abs + 2;
        let mut name_end = name_start;
        while name_end < xml.len() {
            let b = xml.as_bytes()[name_end];
            if b == b'>' || b.is_ascii_whitespace() {
                break;
            }
            name_end += 1;
        }
        if local_name(&xml[name_start..name_end]).eq_ignore_ascii_case(&tag_lc) {
            return Some(abs);
        }
        pos = abs + 2;
    }
    None
}

fn local_name(name: &str) -> &str {
    name.rsplit_once(':')
        .map(|(_, local)| local)
        .unwrap_or(name)
}

fn xml_unescape(s: &str) -> String {
    quick_xml::escape::unescape(s)
        .map(|value| value.into_owned())
        .unwrap_or_else(|_| s.to_string())
}

// ─── Unit conversions ─────────────────────────────────────────────────────────

fn to_microns(value: f64, unit: &str) -> f64 {
    match unit {
        "m" => value * 1e6,
        "mm" => value * 1e3,
        "nm" => value * 1e-3,
        "pm" => value * 1e-6,
        _ => value, // assume µm
    }
}

fn to_seconds(value: f64, unit: &str) -> f64 {
    match unit {
        "ms" => value * 1e-3,
        "µs" | "us" => value * 1e-6,
        "ns" => value * 1e-9,
        "min" => value * 60.0,
        "h" => value * 3600.0,
        _ => value, // assume seconds
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
    use crate::common::pixel_type::PixelType;

    #[test]
    fn rgb_metadata_uses_java_effective_channel_count() {
        let mut meta = ImageMetadata {
            size_x: 2,
            size_y: 2,
            size_z: 1,
            size_c: 6,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 2,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: true,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: std::collections::HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };
        meta.series_metadata
            .insert("plane.0.delta_t".into(), MetadataValue::Float(0.0));
        meta.series_metadata
            .insert("plane.1.delta_t".into(), MetadataValue::Float(1.0));

        let ome = OmeMetadata::from_image_metadata(&meta);
        let image = ome.images.first().expect("OME image");

        assert_eq!(image.channels.len(), 2);
        assert!(image
            .channels
            .iter()
            .all(|channel| channel.samples_per_pixel == 3));
        assert_eq!(image.planes.len(), 2);
        assert_eq!(image.planes[0].the_c, 0);
        assert_eq!(image.planes[1].the_c, 1);
        ome.verify_minimum_populated(&meta, 0).unwrap();

        let xml = ome.to_ome_xml(&meta);
        assert_eq!(xml.matches("<Channel ").count(), 2);
        assert_eq!(xml.matches(r#"SamplesPerPixel="3""#).count(), 2);
        assert!(xml.contains(r#"<Pixels ID="Pixels:0" DimensionOrder="XYCZT" Type="uint8" SizeX="2" SizeY="2" SizeZ="1" SizeC="6" SizeT="1" BigEndian="false""#));
    }
}
