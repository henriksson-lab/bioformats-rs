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

/// Compact OME Pixels description; proprietary metadata remains on the reader.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct OmePixels {
    pub size_x: u32,
    pub size_y: u32,
    pub size_z: u32,
    pub size_c: u32,
    pub size_t: u32,
    pub pixel_type: crate::common::pixel_type::PixelType,
    pub bits_per_pixel: u16,
    pub dimension_order: crate::common::metadata::DimensionOrder,
    pub is_little_endian: bool,
}

impl From<&ImageMetadata> for OmePixels {
    fn from(meta: &ImageMetadata) -> Self {
        Self {
            size_x: meta.size_x,
            size_y: meta.size_y,
            size_z: meta.size_z,
            size_c: meta.size_c,
            size_t: meta.size_t,
            pixel_type: meta.pixel_type,
            bits_per_pixel: meta.bits_per_pixel,
            dimension_order: meta.dimension_order,
            is_little_endian: meta.is_little_endian,
        }
    }
}

impl From<&OmePixels> for ImageMetadata {
    fn from(pixels: &OmePixels) -> Self {
        Self {
            size_x: pixels.size_x,
            size_y: pixels.size_y,
            size_z: pixels.size_z,
            size_c: pixels.size_c,
            size_t: pixels.size_t,
            pixel_type: pixels.pixel_type,
            bits_per_pixel: pixels.bits_per_pixel,
            dimension_order: pixels.dimension_order,
            is_little_endian: pixels.is_little_endian,
            ..Default::default()
        }
    }
}

/// Metadata for one image series.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeImage {
    /// Core pixel description for this image, retained independently of other series.
    #[serde(default)]
    pub pixels: Option<OmePixels>,
    pub name: Option<String>,
    pub description: Option<String>,
    /// Acquisition timestamp/date string as represented in OME-XML.
    pub acquisition_date: Option<String>,
    /// Physical pixel size in X (micrometres, unless `physical_size_x_unit`).
    pub physical_size_x: Option<f64>,
    /// Physical pixel size in Y (micrometres, unless `physical_size_y_unit`).
    pub physical_size_y: Option<f64>,
    /// Physical pixel size in Z / z-step (micrometres, unless `physical_size_z_unit`).
    pub physical_size_z: Option<f64>,
    /// Unit of `physical_size_x` when it is not a length convertible to
    /// micrometres: `"pixel"` or `"reference frame"`. `None` means µm.
    #[serde(default)]
    pub physical_size_x_unit: Option<String>,
    /// Unit of `physical_size_y`; see `physical_size_x_unit`.
    #[serde(default)]
    pub physical_size_y_unit: Option<String>,
    /// Unit of `physical_size_z`; see `physical_size_x_unit`.
    #[serde(default)]
    pub physical_size_z_unit: Option<String>,
    /// Time between frames (seconds).
    pub time_increment: Option<f64>,
    /// `<ImagingEnvironment>` temperature (degrees Celsius). Set by readers such
    /// as MicroManager that record the detector/sample temperature.
    pub imaging_environment_temperature: Option<f64>,
    /// Image-level stage label (`<StageLabel>`).
    #[serde(default)]
    pub stage_label: Option<OmeStageLabel>,
    pub channels: Vec<OmeChannel>,
    pub planes: Vec<OmePlane>,
    /// Reference to an instrument (index into `OmeMetadata::instruments`).
    pub instrument_ref: Option<usize>,
    /// Reference to an objective (index into the instrument's objectives).
    pub objective_ref: Option<usize>,
    /// Image-level ROI references (`<ROIRef ID="..."/>`).
    #[serde(default)]
    pub roi_refs: Vec<String>,
    /// Refractive index from `<ObjectiveSettings RefractiveIndex="...">`.
    pub objective_settings_refractive_index: Option<f64>,
    #[serde(default)]
    pub objective_settings_correction_collar: Option<f64>,
    #[serde(default)]
    pub objective_settings_medium: Option<String>,
    /// Per-channel light paths.
    pub light_paths: Vec<OmeLightPath>,
    /// Modulo annotation for Z (sub-dimensions within Z).
    pub modulo_z: Option<crate::metadata::ModuloAnnotation>,
    /// Modulo annotation for C (sub-dimensions within C).
    pub modulo_c: Option<crate::metadata::ModuloAnnotation>,
    /// Modulo annotation for T (sub-dimensions within T).
    pub modulo_t: Option<crate::metadata::ModuloAnnotation>,
}

/// Image-level stage position label.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize, PartialEq)]
pub struct OmeStageLabel {
    pub name: Option<String>,
    pub x: Option<f64>,
    pub y: Option<f64>,
    pub z: Option<f64>,
    /// Unit of `x`, as Java's `getStageLabelX().unit().getSymbol()`: a
    /// `UnitsLength` symbol (`"µm"`, `"mm"`, `"reference frame"`, ...), kept
    /// unconverted as Java keeps it. `None`: the reader recorded none.
    #[serde(default)]
    pub x_unit: Option<String>,
    /// Unit of `y`; see `x_unit`.
    #[serde(default)]
    pub y_unit: Option<String>,
    /// Unit of `z`; see `x_unit`.
    #[serde(default)]
    pub z_unit: Option<String>,
}

/// Per-channel metadata.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeChannel {
    #[serde(default)]
    pub filter_set_ref: Option<String>,
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
    #[serde(default)]
    pub microscope_type: Option<String>,

    pub id: Option<String>,
    pub microscope_model: Option<String>,
    pub microscope_manufacturer: Option<String>,
    pub microscope_serial_number: Option<String>,
    pub objectives: Vec<OmeObjective>,
    pub detectors: Vec<OmeDetector>,
    pub light_sources: Vec<OmeLightSource>,
    pub filters: Vec<OmeFilter>,
    pub dichroics: Vec<OmeDichroic>,
    #[serde(default)]
    pub filter_sets: Vec<OmeFilterSet>,
}

/// Objective lens metadata.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeObjective {
    #[serde(default)]
    pub iris: Option<bool>,
    pub id: Option<String>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    pub serial_number: Option<String>,
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
    #[serde(default)]
    pub zoom: Option<f64>,
    #[serde(default)]
    pub amplification_gain: Option<f64>,
    #[serde(default)]
    pub serial_number: Option<String>,
    #[serde(default)]
    pub lot_number: Option<String>,

    /// Detector voltage in volts, distinct from channel DetectorSettings voltage.
    #[serde(default)]
    pub voltage: Option<f64>,
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
    #[serde(default)]
    pub laser_type: Option<String>,
    #[serde(default)]
    pub laser_medium: Option<String>,
    #[serde(default)]
    pub serial_number: Option<String>,
    #[serde(default)]
    pub lot_number: Option<String>,

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

/// Instrument filter set and its optical component references.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeFilterSet {
    #[serde(default)]
    pub serial_number: Option<String>,
    #[serde(default)]
    pub lot_number: Option<String>,

    pub id: Option<String>,
    pub model: Option<String>,
    pub manufacturer: Option<String>,
    pub excitation_filter_ids: Vec<String>,
    pub dichroic_ref: Option<String>,
    pub emission_filter_ids: Vec<String>,
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
    /// OME Shape styling/affine transform without changing its geometry.
    /// The transform stores A00, A01, A02, A10, A11, A12 in that order.
    Styled {
        shape: Box<OmeShape>,
        font_size: Option<f64>,
        stroke_width: Option<f64>,
        transform: Option<[f64; 6]>,
    },
    Label {
        x: f64,
        y: f64,
        text: Option<String>,
        font_size: Option<f64>,
        stroke_width: Option<f64>,
        the_z: Option<u32>,
        the_t: Option<u32>,
        the_c: Option<u32>,
    },
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
    Mask {
        x: f64,
        y: f64,
        width: f64,
        height: f64,
        stroke_color: Option<i32>,
        fill_color: Option<i32>,
        bin_data: Option<Vec<u8>>,
        the_z: Option<u32>,
        the_t: Option<u32>,
        the_c: Option<u32>,
    },
}

/// Experimenter metadata.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct OmeExperimenter {
    #[serde(default)]
    pub user_name: Option<String>,
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
    /// Unit of `position_x`, as Java's `getPlanePositionX().unit().getSymbol()`:
    /// a `UnitsLength` symbol (`"µm"`, `"mm"`, `"reference frame"`, ...).
    /// Stage positions are not converted (`FormatTools.getStagePosition`
    /// keeps the unit it is given), so the value is in this unit. `None`: the
    /// reader recorded none.
    #[serde(default)]
    pub position_x_unit: Option<String>,
    /// Unit of `position_y`; see `position_x_unit`.
    #[serde(default)]
    pub position_y_unit: Option<String>,
    /// Unit of `position_z`; see `position_x_unit`.
    #[serde(default)]
    pub position_z_unit: Option<String>,
}

// ─── Serialisation ───────────────────────────────────────────────────────────

impl OmeMetadata {
    /// Serialize a metadata-only OME-XML document. Pixel storage mappings are
    /// supplied by the writer when embedding this metadata in an image file.
    pub fn to_ome_xml(&self, meta: &crate::metadata::ImageMetadata) -> String {
        use std::fmt::Write;
        let mut xml = String::new();
        let _ = write!(xml, r#"<?xml version="1.0" encoding="UTF-8"?>"#);
        let _ = write!(
            xml,
            r#"<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance" xsi:schemaLocation="http://www.openmicroscopy.org/Schemas/OME/2016-06 http://www.openmicroscopy.org/Schemas/OME/2016-06/ome.xsd">"#
        );

        for (pi, plate) in self.plates.iter().enumerate() {
            let default_id = create_lsid("Plate", &[pi]);
            let _ = write!(
                xml,
                r#"<Plate ID="{}""#,
                xml_escape(plate.id.as_deref().unwrap_or(&default_id))
            );
            if let Some(v) = &plate.name {
                let _ = write!(xml, r#" Name="{}""#, xml_escape(v));
            }
            if plate.rows > 0 {
                let _ = write!(xml, r#" Rows="{}""#, plate.rows);
            }
            if plate.columns > 0 {
                let _ = write!(xml, r#" Columns="{}""#, plate.columns);
            }
            xml.push('>');
            for (wi, well) in plate.wells.iter().enumerate() {
                let default_id = create_lsid("Well", &[pi, wi]);
                let _ = write!(
                    xml,
                    r#"<Well ID="{}" Row="{}" Column="{}">"#,
                    xml_escape(well.id.as_deref().unwrap_or(&default_id)),
                    well.row,
                    well.column
                );
                for (si, sample) in well.well_samples.iter().enumerate() {
                    let default_id = create_lsid("WellSample", &[pi, wi, si]);
                    let _ = write!(
                        xml,
                        r#"<WellSample ID="{}" Index="{}""#,
                        xml_escape(sample.id.as_deref().unwrap_or(&default_id)),
                        sample.index
                    );
                    if let Some(v) = sample.position_x {
                        let _ = write!(xml, r#" PositionX="{v}""#);
                    }
                    if let Some(v) = sample.position_y {
                        let _ = write!(xml, r#" PositionY="{v}""#);
                    }
                    xml.push('>');
                    if let Some(i) = sample.image_ref.filter(|&i| i < self.images.len()) {
                        let _ = write!(xml, r#"<ImageRef ID="Image:{i}"/>"#);
                    }
                    xml.push_str("</WellSample>");
                }
                xml.push_str("</Well>");
            }
            xml.push_str("</Plate>");
        }
        for (si, screen) in self.screens.iter().enumerate() {
            let default_id = create_lsid("Screen", &[si]);
            let _ = write!(
                xml,
                r#"<Screen ID="{}""#,
                xml_escape(screen.id.as_deref().unwrap_or(&default_id))
            );
            if let Some(v) = &screen.name {
                let _ = write!(xml, r#" Name="{}""#, xml_escape(v));
            }
            if let Some(v) = &screen.protocol_description {
                let _ = write!(xml, r#" ProtocolDescription="{}""#, xml_escape(v));
            }
            xml.push_str("/>");
        }

        for (ei, experimenter) in self.experimenters.iter().enumerate() {
            let default_id = format!("Experimenter:{ei}");
            let exp_id = experimenter.id.as_deref().unwrap_or(&default_id);
            let _ = write!(xml, r#"<Experimenter ID="{}""#, xml_escape(exp_id));
            if let Some(v) = &experimenter.user_name {
                let _ = write!(xml, r#" UserName="{}""#, xml_escape(v));
            }
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

            if inst.microscope_model.is_some()
                || inst.microscope_manufacturer.is_some()
                || inst.microscope_serial_number.is_some()
                || inst.microscope_type.is_some()
            {
                let _ = write!(xml, "<Microscope");
                if let Some(v) = &inst.microscope_type {
                    let _ = write!(xml, r#" Type="{}""#, xml_escape(v));
                }
                if let Some(m) = &inst.microscope_model {
                    let _ = write!(xml, r#" Model="{}""#, xml_escape(m));
                }
                if let Some(m) = &inst.microscope_manufacturer {
                    let _ = write!(xml, r#" Manufacturer="{}""#, xml_escape(m));
                }
                if let Some(s) = &inst.microscope_serial_number {
                    let _ = write!(xml, r#" SerialNumber="{}""#, xml_escape(s));
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
                if let Some(v) = &ls.serial_number {
                    let _ = write!(xml, r#" SerialNumber="{}""#, xml_escape(v));
                }
                if let Some(v) = &ls.lot_number {
                    let _ = write!(xml, r#" LotNumber="{}""#, xml_escape(v));
                }
                if ls_tag == "Laser" {
                    if let Some(v) = &ls.laser_type {
                        let _ = write!(xml, r#" Type="{}""#, xml_escape(v));
                    }
                    if let Some(v) = &ls.laser_medium {
                        let _ = write!(xml, r#" LaserMedium="{}""#, xml_escape(v));
                    }
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
                if let Some(v) = det.zoom {
                    let _ = write!(xml, r#" Zoom="{v}""#);
                }
                if let Some(v) = det.amplification_gain {
                    let _ = write!(xml, r#" AmplificationGain="{v}""#);
                }
                if let Some(v) = &det.serial_number {
                    let _ = write!(xml, r#" SerialNumber="{}""#, xml_escape(v));
                }
                if let Some(v) = &det.lot_number {
                    let _ = write!(xml, r#" LotNumber="{}""#, xml_escape(v));
                }
                if let Some(v) = det.voltage {
                    let _ = write!(xml, r#" Voltage="{v}" VoltageUnit="V""#);
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
                if let Some(v) = &obj.serial_number {
                    let _ = write!(xml, r#" SerialNumber="{}""#, xml_escape(v));
                }
                if let Some(v) = obj.nominal_magnification {
                    let _ = write!(xml, r#" NominalMagnification="{v}""#);
                }
                if let Some(v) = obj.calibrated_magnification {
                    let _ = write!(xml, r#" CalibratedMagnification="{v}""#);
                }
                if let Some(v) = obj.iris {
                    let _ = write!(xml, r#" Iris="{v}""#);
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

            for (fi, set) in inst.filter_sets.iter().enumerate() {
                let default_id = create_lsid("FilterSet", &[ii, fi]);
                let _ = write!(
                    xml,
                    r#"<FilterSet ID="{}""#,
                    xml_escape(set.id.as_deref().unwrap_or(&default_id))
                );
                if let Some(model) = &set.model {
                    let _ = write!(xml, r#" Model="{}""#, xml_escape(model));
                }
                if let Some(manufacturer) = &set.manufacturer {
                    let _ = write!(xml, r#" Manufacturer="{}""#, xml_escape(manufacturer));
                }
                if let Some(v) = &set.serial_number {
                    let _ = write!(xml, r#" SerialNumber="{}""#, xml_escape(v));
                }
                if let Some(v) = &set.lot_number {
                    let _ = write!(xml, r#" LotNumber="{}""#, xml_escape(v));
                }
                xml.push('>');
                for id in &set.excitation_filter_ids {
                    let _ = write!(xml, r#"<ExcitationFilterRef ID="{}"/>"#, xml_escape(id));
                }
                if let Some(id) = &set.dichroic_ref {
                    let _ = write!(xml, r#"<DichroicRef ID="{}"/>"#, xml_escape(id));
                }
                for id in &set.emission_filter_ids {
                    let _ = write!(xml, r#"<EmissionFilterRef ID="{}"/>"#, xml_escape(id));
                }
                xml.push_str("</FilterSet>");
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
                if filter.cut_in.is_some() || filter.cut_out.is_some() {
                    xml.push_str("><TransmittanceRange");
                    if let Some(v) = filter.cut_in {
                        let _ = write!(xml, r#" CutIn="{v}""#);
                    }
                    if let Some(v) = filter.cut_out {
                        let _ = write!(xml, r#" CutOut="{v}""#);
                    }
                    xml.push_str("/></Filter>");
                } else {
                    xml.push_str("/>");
                }
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

        for (i, img) in self.images.iter().enumerate() {
            let per_image_metadata = img.pixels.as_ref().map(ImageMetadata::from);
            let meta = per_image_metadata.as_ref().unwrap_or(meta);
            let _ = write!(
                xml,
                r#"<Image ID="Image:{i}" Name="{}">"#,
                xml_escape(img.name.as_deref().unwrap_or(&format!("Series {i}")))
            );

            if let Some(date) = &img.acquisition_date {
                let _ = write!(
                    xml,
                    "<AcquisitionDate>{}</AcquisitionDate>",
                    xml_escape(date)
                );
            }
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
                        let _ = write!(xml, r#"<ObjectiveSettings ID="{}""#, xml_escape(obj_id));
                        if let Some(v) = img.objective_settings_refractive_index {
                            let _ = write!(xml, r#" RefractiveIndex="{v}""#);
                        }
                        if let Some(v) = img.objective_settings_correction_collar {
                            let _ = write!(xml, r#" CorrectionCollar="{v}""#);
                        }
                        if let Some(v) = &img.objective_settings_medium {
                            let _ = write!(xml, r#" Medium="{}""#, xml_escape(v));
                        }
                        xml.push_str("/>");
                    }
                }
            }

            if let Some(v) = img.imaging_environment_temperature {
                let _ = write!(
                    xml,
                    r#"<ImagingEnvironment Temperature="{v}" TemperatureUnit="°C"/>"#
                );
            }
            if let Some(stage_label) = &img.stage_label {
                let _ = write!(
                    xml,
                    r#"<StageLabel Name="{}""#,
                    xml_escape(stage_label.name.as_deref().unwrap_or(""))
                );
                if let Some(v) = stage_label.x {
                    let _ = write!(xml, r#" X="{v}""#);
                    if let Some(unit) = &stage_label.x_unit {
                        let _ = write!(xml, r#" XUnit="{}""#, xml_escape(unit));
                    }
                }
                if let Some(v) = stage_label.y {
                    let _ = write!(xml, r#" Y="{v}""#);
                    if let Some(unit) = &stage_label.y_unit {
                        let _ = write!(xml, r#" YUnit="{}""#, xml_escape(unit));
                    }
                }
                if let Some(v) = stage_label.z {
                    let _ = write!(xml, r#" Z="{v}""#);
                    if let Some(unit) = &stage_label.z_unit {
                        let _ = write!(xml, r#" ZUnit="{}""#, xml_escape(unit));
                    }
                }
                xml.push_str("/>");
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

            if meta.bits_per_pixel > 0 {
                let _ = write!(xml, r#" SignificantBits="{}""#, meta.bits_per_pixel);
            }
            if let Some(v) = img.physical_size_x {
                let unit = img.physical_size_x_unit.as_deref().unwrap_or("µm");
                let _ = write!(xml, r#" PhysicalSizeX="{v}" PhysicalSizeXUnit="{unit}""#);
            }
            if let Some(v) = img.physical_size_y {
                let unit = img.physical_size_y_unit.as_deref().unwrap_or("µm");
                let _ = write!(xml, r#" PhysicalSizeY="{v}" PhysicalSizeYUnit="{unit}""#);
            }
            if let Some(v) = img.physical_size_z {
                let unit = img.physical_size_z_unit.as_deref().unwrap_or("µm");
                let _ = write!(xml, r#" PhysicalSizeZ="{v}" PhysicalSizeZUnit="{unit}""#);
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
                if has_light_source_settings
                    || has_detector_settings
                    || has_light_path
                    || ch.filter_set_ref.is_some()
                {
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
                    if let Some(id) = &ch.filter_set_ref {
                        let _ = write!(xml, r#"<FilterSetRef ID="{}"/>"#, xml_escape(id));
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

            // The model describes pixels but carries no file/IFD storage mappings.
            xml.push_str("<MetadataOnly/>");

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
                    if let Some(unit) = &plane.position_x_unit {
                        let _ = write!(xml, r#" PositionXUnit="{}""#, xml_escape(unit));
                    }
                }
                if let Some(v) = plane.position_y {
                    let _ = write!(xml, r#" PositionY="{v}""#);
                    if let Some(unit) = &plane.position_y_unit {
                        let _ = write!(xml, r#" PositionYUnit="{}""#, xml_escape(unit));
                    }
                }
                if let Some(v) = plane.position_z {
                    let _ = write!(xml, r#" PositionZ="{v}""#);
                    if let Some(unit) = &plane.position_z_unit {
                        let _ = write!(xml, r#" PositionZUnit="{}""#, xml_escape(unit));
                    }
                }
                xml.push_str("/>");
            }

            xml.push_str("</Pixels>");
            for roi_id in &img.roi_refs {
                let _ = write!(xml, r#"<ROIRef ID="{}"/>"#, xml_escape(roi_id));
            }

            if img.modulo_z.is_some() || img.modulo_c.is_some() || img.modulo_t.is_some() {
                let _ = write!(xml, r#"<AnnotationRef ID="Annotation:Modulo:{i}"/>"#);
            }
            xml.push_str("</Image>");
        }

        if !self.annotations.is_empty()
            || self.images.iter().any(|img| {
                img.modulo_z.is_some() || img.modulo_c.is_some() || img.modulo_t.is_some()
            })
        {
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
            for (i, img) in self.images.iter().enumerate() {
                if img.modulo_z.is_none() && img.modulo_c.is_none() && img.modulo_t.is_none() {
                    continue;
                }
                let _ = write!(
                    xml,
                    r#"<XMLAnnotation ID="Annotation:Modulo:{i}" Namespace="openmicroscopy.org/omero/dimension/modulo"><Value><Modulo xmlns="http://www.openmicroscopy.org/Schemas/Additions/2011-09">"#
                );
                for (dim, modulo) in [
                    ("Z", &img.modulo_z),
                    ("C", &img.modulo_c),
                    ("T", &img.modulo_t),
                ] {
                    if let Some(m) = modulo {
                        let _ = write!(
                            xml,
                            r#"<ModuloAlong{dim} Type="{}" Start="{}" Step="{}" End="{}" Unit="{}">"#,
                            xml_escape(&m.modulo_type),
                            m.start,
                            m.step,
                            m.end,
                            xml_escape(&m.unit)
                        );
                        for label in &m.labels {
                            let _ = write!(xml, "<Label>{}</Label>", xml_escape(label));
                        }
                        let _ = write!(xml, "</ModuloAlong{dim}>");
                    }
                }
                xml.push_str("</Modulo></Value></XMLAnnotation>");
            }
            xml.push_str("</StructuredAnnotations>");
        }

        for (ri, roi) in self.rois.iter().enumerate() {
            let default_id = create_lsid("ROI", &[ri]);
            let roi_id = roi.id.as_deref().unwrap_or(&default_id);
            let _ = write!(xml, r#"<ROI ID="{}""#, xml_escape(roi_id));
            if let Some(name) = &roi.name {
                let _ = write!(xml, r#" Name="{}""#, xml_escape(name));
            }
            xml.push_str("><Union>");
            for (si, shape) in roi.shapes.iter().enumerate() {
                let start = xml.len();
                write_ome_shape_xml(&mut xml, shape);
                if let Some(end) = xml[start..].find(char::is_whitespace) {
                    xml.insert_str(start + end, &format!(r#" ID="Shape:{ri}:{si}""#));
                }
            }
            xml.push_str("</Union></ROI>");
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
        OmeShape::Styled {
            shape,
            font_size,
            stroke_width,
            transform,
        } => {
            let mut inner = String::new();
            write_ome_shape_xml(&mut inner, shape);
            for (name, value) in [("FontSize", font_size), ("StrokeWidth", stroke_width)] {
                if let Some(value) = value {
                    let prefix = format!(" {name}=\"");
                    if let Some(start) = inner.find(&prefix) {
                        if let Some(end) = inner[start + prefix.len()..].find('"') {
                            inner.replace_range(start..start + prefix.len() + end + 1, "");
                        }
                    }
                    if let Some(close) = inner.find('>') {
                        let insert = if inner.as_bytes().get(close.wrapping_sub(1)) == Some(&b'/') {
                            close - 1
                        } else {
                            close
                        };
                        inner.insert_str(insert, &format!(r#" {name}="{value}""#));
                    }
                }
            }
            if let Some(matrix) = transform {
                if let Some(close) = inner.find('>') {
                    let tag_end = inner.find(char::is_whitespace).unwrap_or(close);
                    let tag_name = inner[1..tag_end].to_string();
                    let element = format!(
                        r#"<Transform A00="{}" A01="{}" A02="{}" A10="{}" A11="{}" A12="{}"/>"#,
                        matrix[0], matrix[1], matrix[2], matrix[3], matrix[4], matrix[5]
                    );
                    if inner.as_bytes().get(close.wrapping_sub(1)) == Some(&b'/') {
                        inner.replace_range(close - 1..=close, &format!(">{element}</{tag_name}>"));
                    } else {
                        inner.insert_str(close + 1, &element);
                    }
                }
            }
            xml.push_str(&inner);
        }
        OmeShape::Label {
            x,
            y,
            text,
            font_size,
            stroke_width,
            the_z,
            the_t,
            the_c,
        } => {
            let _ = write!(xml, r#"<Label X="{x}" Y="{y}""#);
            if let Some(text) = text {
                let _ = write!(xml, r#" Text="{}""#, xml_escape(text));
            }
            if let Some(font_size) = font_size {
                let _ = write!(xml, r#" FontSize="{font_size}""#);
            }
            if let Some(stroke_width) = stroke_width {
                let _ = write!(xml, r#" StrokeWidth="{stroke_width}""#);
            }
            write_shape_indices(xml, *the_z, *the_t, *the_c);
            xml.push_str("/>");
        }
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
        OmeShape::Mask {
            x,
            y,
            width,
            height,
            stroke_color,
            fill_color,
            bin_data,
            the_z,
            the_t,
            the_c,
        } => {
            let _ = write!(
                xml,
                r#"<Mask X="{x}" Y="{y}" Width="{width}" Height="{height}""#
            );
            if let Some(v) = stroke_color {
                let _ = write!(xml, r#" StrokeColor="{v}""#);
            }
            if let Some(v) = fill_color {
                let _ = write!(xml, r#" FillColor="{v}""#);
            }
            write_shape_indices(xml, *the_z, *the_t, *the_c);
            if let Some(bytes) = bin_data {
                let encoded = base64_encode(bytes);
                let _ = write!(
                    xml,
                    r#"><BinData Length="{}">{encoded}</BinData></Mask>"#,
                    bytes.len()
                );
            } else {
                xml.push_str("/>");
            }
        }
    }
}

fn base64_encode(data: &[u8]) -> String {
    const CHARS: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity((data.len() + 2) / 3 * 4);
    for chunk in data.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let triple = (b0 << 16) | (b1 << 8) | b2;
        out.push(CHARS[((triple >> 18) & 0x3f) as usize] as char);
        out.push(CHARS[((triple >> 12) & 0x3f) as usize] as char);
        if chunk.len() > 1 {
            out.push(CHARS[((triple >> 6) & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(CHARS[(triple & 0x3f) as usize] as char);
        } else {
            out.push('=');
        }
    }
    out
}

fn base64_decode(input: &str) -> Vec<u8> {
    let bytes: Vec<u8> = input.bytes().filter(|b| !b.is_ascii_whitespace()).collect();
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    let mut index = 0usize;
    while index + 3 < bytes.len() {
        let Some(a) = base64_value(bytes[index]) else {
            break;
        };
        let Some(b) = base64_value(bytes[index + 1]) else {
            break;
        };
        let c_byte = bytes[index + 2];
        let d_byte = bytes[index + 3];
        out.push((a << 2) | (b >> 4));
        if c_byte != b'=' {
            if let Some(c) = base64_value(c_byte) {
                out.push((b << 4) | (c >> 2));
                if d_byte != b'=' {
                    if let Some(d) = base64_value(d_byte) {
                        out.push((c << 6) | d);
                    }
                }
            }
        }
        index += 4;
    }
    out
}

fn base64_value(byte: u8) -> Option<u8> {
    match byte {
        b'A'..=b'Z' => Some(byte - b'A'),
        b'a'..=b'z' => Some(byte - b'a' + 26),
        b'0'..=b'9' => Some(byte - b'0' + 52),
        b'+' => Some(62),
        b'/' => Some(63),
        _ => None,
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
        image.pixels = Some(OmePixels::from(meta));
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
            image.stage_label = generic_stage_label_from_metadata(meta);
            image.roi_refs = generic_image_roi_refs_from_metadata(meta);
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
            let stage_label = all_tag_positions(img_xml, "StageLabel")
                .into_iter()
                .next()
                .map(|p| {
                    let tag = start_tag_at(img_xml, p);
                    let x = xml_attr(tag, "X").and_then(|s| s.parse::<f64>().ok());
                    let y = xml_attr(tag, "Y").and_then(|s| s.parse::<f64>().ok());
                    let z = xml_attr(tag, "Z").and_then(|s| s.parse::<f64>().ok());
                    OmeStageLabel {
                        name: xml_attr(tag, "Name"),
                        // StageLabel.update: XUnit, else getXUnitXsdDefault()
                        x_unit: x.map(|_| {
                            xml_attr(tag, "XUnit")
                                .filter(|u| !u.is_empty())
                                .unwrap_or_else(|| "reference frame".to_string())
                        }),
                        y_unit: y.map(|_| {
                            xml_attr(tag, "YUnit")
                                .filter(|u| !u.is_empty())
                                .unwrap_or_else(|| "reference frame".to_string())
                        }),
                        z_unit: z.map(|_| {
                            xml_attr(tag, "ZUnit")
                                .filter(|u| !u.is_empty())
                                .unwrap_or_else(|| "reference frame".to_string())
                        }),
                        x,
                        y,
                        z,
                    }
                });
            let roi_refs = all_tag_positions(img_xml, "ROIRef")
                .into_iter()
                .filter_map(|p| xml_attr(start_tag_at(img_xml, p), "ID"))
                .collect();

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
                        psx.filter(|v| *v > 0.0)
                            .and_then(|v| get_physical_size(v, Some(&psx_u))),
                        psy.filter(|v| *v > 0.0)
                            .and_then(|v| get_physical_size(v, Some(&psy_u))),
                        psz.filter(|v| *v > 0.0)
                            .and_then(|v| get_physical_size(v, Some(&psz_u))),
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

            let pixels = pixels_xml.and_then(|xml| {
                let pt = start_tag_at(xml, all_tag_positions(xml, "Pixels").into_iter().next()?);
                use crate::common::metadata::DimensionOrder::*;
                use crate::common::pixel_type::PixelType;
                let pixel_type = match xml_attr(pt, "Type").as_deref()? {
                    "bit" => PixelType::Bit,
                    "int8" => PixelType::Int8,
                    "uint8" => PixelType::Uint8,
                    "int16" => PixelType::Int16,
                    "uint16" => PixelType::Uint16,
                    "int32" => PixelType::Int32,
                    "uint32" => PixelType::Uint32,
                    "float" => PixelType::Float32,
                    "double" => PixelType::Float64,
                    _ => return None,
                };
                let dimension_order = match xml_attr(pt, "DimensionOrder").as_deref()? {
                    "XYCZT" => XYCZT,
                    "XYCTZ" => XYCTZ,
                    "XYTCZ" => XYTCZ,
                    "XYTZC" => XYTZC,
                    "XYZCT" => XYZCT,
                    "XYZTC" => XYZTC,
                    _ => return None,
                };
                Some(OmePixels {
                    size_x: xml_attr(pt, "SizeX")?.parse().ok()?,
                    size_y: xml_attr(pt, "SizeY")?.parse().ok()?,
                    size_z: xml_attr(pt, "SizeZ")?.parse().ok()?,
                    size_c: xml_attr(pt, "SizeC")?.parse().ok()?,
                    size_t: xml_attr(pt, "SizeT")?.parse().ok()?,
                    pixel_type,
                    dimension_order,
                    bits_per_pixel: xml_attr(pt, "SignificantBits")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(if pixel_type == PixelType::Bit {
                            1
                        } else {
                            (pixel_type.bytes_per_sample() * 8) as u16
                        }),
                    is_little_endian: xml_attr(pt, "BigEndian").as_deref() != Some("true"),
                })
            });
            let mut channels = pixels_xml.map(parse_channels).unwrap_or_default();
            if let Some(pixels_xml) = pixels_xml {
                let pixels_tag = start_tag_at(pixels_xml, 0);
                pad_channels_to_declared_size_c(&mut channels, pixels_tag);
            }
            if channels.is_empty() {
                let logical_channels = all_tag_positions(img_xml, "LogicalChannel").len();
                channels.resize_with(logical_channels, || OmeChannel {
                    samples_per_pixel: 1,
                    ..Default::default()
                });
            }
            let light_paths = pixels_xml
                .map(|pixels| {
                    all_tag_positions(pixels, "Channel")
                        .into_iter()
                        .map(|p| {
                            let channel = first_xml_block(&pixels[p..], "Channel").unwrap_or("");
                            let path = first_xml_block(channel, "LightPath").unwrap_or("");
                            let refs = |tag| {
                                all_tag_positions(path, tag)
                                    .into_iter()
                                    .filter_map(|p| xml_attr(start_tag_at(path, p), "ID"))
                                    .collect()
                            };
                            OmeLightPath {
                                excitation_filter_ids: refs("ExcitationFilterRef"),
                                dichroic_id: all_tag_positions(path, "DichroicRef")
                                    .into_iter()
                                    .next()
                                    .and_then(|p| xml_attr(start_tag_at(path, p), "ID")),
                                emission_filter_ids: refs("EmissionFilterRef"),
                            }
                        })
                        .collect()
                })
                .unwrap_or_default();
            let imaging_environment_temperature = all_tag_positions(img_xml, "ImagingEnvironment")
                .into_iter()
                .next()
                .and_then(|p| {
                    let tag = start_tag_at(img_xml, p);
                    let value = xml_attr(tag, "Temperature")?.parse::<f64>().ok()?;
                    Some(match xml_attr(tag, "TemperatureUnit").as_deref() {
                        Some("K") => value - 273.15,
                        Some("°F") => (value - 32.0) * 5.0 / 9.0,
                        _ => value,
                    })
                });
            let planes = pixels_xml.map(parse_planes).unwrap_or_default();
            let modulo_z = parse_modulo(xml, img_xml, "Z");
            let modulo_c = parse_modulo(xml, img_xml, "C");
            let modulo_t = parse_modulo(xml, img_xml, "T");

            images.push(OmeImage {
                pixels,
                name,
                description,
                acquisition_date,
                physical_size_x: physical_size_x.as_ref().map(|(v, _)| *v),
                physical_size_y: physical_size_y.as_ref().map(|(v, _)| *v),
                physical_size_z: physical_size_z.as_ref().map(|(v, _)| *v),
                physical_size_x_unit: physical_size_x.and_then(|(_, u)| u),
                physical_size_y_unit: physical_size_y.and_then(|(_, u)| u),
                physical_size_z_unit: physical_size_z.and_then(|(_, u)| u),
                time_increment,
                stage_label,
                channels,
                light_paths,
                imaging_environment_temperature,
                planes,
                roi_refs,
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
                images[i].objective_settings_refractive_index =
                    xml_attr(tag, "RefractiveIndex").and_then(|s| s.parse().ok());
                images[i].objective_settings_correction_collar =
                    xml_attr(tag, "CorrectionCollar").and_then(|s| s.parse().ok());
                images[i].objective_settings_medium = xml_attr(tag, "Medium");
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
                    user_name: xml_attr(tag, "UserName"),
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

        let image_ids: Vec<_> = all_tag_positions(xml, "Image")
            .into_iter()
            .map(|p| xml_attr(start_tag_at(xml, p), "ID"))
            .collect();
        let plates = all_tag_positions(xml, "Plate")
            .into_iter()
            .map(|p| {
                let body = first_xml_block(&xml[p..], "Plate").unwrap_or("");
                let tag = start_tag_at(xml, p);
                OmePlate {
                    id: xml_attr(tag, "ID"),
                    name: xml_attr(tag, "Name"),
                    rows: xml_attr(tag, "Rows")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    columns: xml_attr(tag, "Columns")
                        .and_then(|v| v.parse().ok())
                        .unwrap_or(0),
                    wells: all_tag_positions(body, "Well")
                        .into_iter()
                        .map(|p| {
                            let well_body = first_xml_block(&body[p..], "Well").unwrap_or("");
                            let tag = start_tag_at(body, p);
                            OmeWell {
                                id: xml_attr(tag, "ID"),
                                row: xml_attr(tag, "Row")
                                    .and_then(|v| v.parse().ok())
                                    .unwrap_or(0),
                                column: xml_attr(tag, "Column")
                                    .and_then(|v| v.parse().ok())
                                    .unwrap_or(0),
                                well_samples: all_tag_positions(well_body, "WellSample")
                                    .into_iter()
                                    .map(|p| {
                                        let sample = first_xml_block(&well_body[p..], "WellSample")
                                            .unwrap_or("");
                                        let tag = start_tag_at(well_body, p);
                                        let image_ref = all_tag_positions(sample, "ImageRef")
                                            .into_iter()
                                            .next()
                                            .and_then(|p| xml_attr(start_tag_at(sample, p), "ID"))
                                            .and_then(|id| {
                                                image_ids
                                                    .iter()
                                                    .position(|v| v.as_deref() == Some(&id))
                                            });
                                        OmeWellSample {
                                            id: xml_attr(tag, "ID"),
                                            index: xml_attr(tag, "Index")
                                                .and_then(|v| v.parse().ok())
                                                .unwrap_or(0),
                                            image_ref,
                                            position_x: xml_attr(tag, "PositionX")
                                                .and_then(|v| v.parse().ok()),
                                            position_y: xml_attr(tag, "PositionY")
                                                .and_then(|v| v.parse().ok()),
                                        }
                                    })
                                    .collect(),
                            }
                        })
                        .collect(),
                }
            })
            .collect();
        let screens = all_tag_positions(xml, "Screen")
            .into_iter()
            .map(|p| {
                let tag = start_tag_at(xml, p);
                OmeScreen {
                    id: xml_attr(tag, "ID"),
                    name: xml_attr(tag, "Name"),
                    protocol_description: xml_attr(tag, "ProtocolDescription"),
                }
            })
            .collect();

        OmeMetadata {
            images,
            instruments,
            experimenters,
            rois,
            annotations,
            plates,
            screens,
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
            instruments: czi_instruments(xml),
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
            let body_end = if tag.trim_end().ends_with("/>") {
                body_start
            } else {
                find_end_tag(pixels_xml, "Channel", body_start).unwrap_or(body_start)
            };
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
            let detector = all_tag_positions(body, "DetectorSettings")
                .into_iter()
                .next()
                .map(|p| start_tag_at(body, p))
                .unwrap_or("");
            OmeChannel {
                pinhole_size: xml_attr(tag, "PinholeSize").and_then(|v| v.parse().ok()),
                fluor: xml_attr(tag, "Fluor"),
                nd_filter: xml_attr(tag, "NDFilter").and_then(|v| v.parse().ok()),
                illumination_type: xml_attr(tag, "IlluminationType"),
                contrast_method: xml_attr(tag, "ContrastMethod"),
                acquisition_mode: xml_attr(tag, "AcquisitionMode"),
                detector_ref: xml_attr(detector, "ID"),
                detector_settings_gain: xml_attr(detector, "Gain").and_then(|v| v.parse().ok()),
                detector_settings_offset: xml_attr(detector, "Offset").and_then(|v| v.parse().ok()),
                detector_settings_voltage: xml_attr(detector, "Voltage")
                    .and_then(|v| v.parse().ok()),
                detector_settings_binning: xml_attr(detector, "Binning"),
                filter_set_ref: all_tag_positions(body, "FilterSetRef")
                    .into_iter()
                    .next()
                    .and_then(|pos| xml_attr(start_tag_at(body, pos), "ID")),
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
            }
        })
        .collect()
}

fn pad_channels_to_declared_size_c(channels: &mut Vec<OmeChannel>, pixels_tag: &str) {
    let Some(size_c) = xml_attr(pixels_tag, "SizeC").and_then(|s| s.parse::<u32>().ok()) else {
        return;
    };
    if size_c == 0 {
        return;
    }

    let mut samples = channels
        .iter()
        .map(|channel| channel.samples_per_pixel.max(1))
        .sum::<u32>();
    while samples < size_c {
        channels.push(OmeChannel {
            samples_per_pixel: 1,
            ..Default::default()
        });
        samples += 1;
    }
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
            let position_x = xml_attr(tag, "PositionX").and_then(|s| s.parse().ok());
            let position_y = xml_attr(tag, "PositionY").and_then(|s| s.parse().ok());
            let position_z = xml_attr(tag, "PositionZ").and_then(|s| s.parse().ok());
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
                // Plane.update: PositionXUnit, else getPositionXUnitXsdDefault()
                // ("reference frame"); Java's UnitsLength.fromString then throws
                // on an unknown symbol, which is kept here for the caller to judge
                position_x_unit: position_x.map(|_| {
                    xml_attr(tag, "PositionXUnit")
                        .filter(|u| !u.is_empty())
                        .unwrap_or_else(|| "reference frame".to_string())
                }),
                position_y_unit: position_y.map(|_| {
                    xml_attr(tag, "PositionYUnit")
                        .filter(|u| !u.is_empty())
                        .unwrap_or_else(|| "reference frame".to_string())
                }),
                position_z_unit: position_z.map(|_| {
                    xml_attr(tag, "PositionZUnit")
                        .filter(|u| !u.is_empty())
                        .unwrap_or_else(|| "reference frame".to_string())
                }),
                position_x,
                position_y,
                position_z,
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

            let inst_xml = first_xml_block(&xml[pos..], "Instrument").unwrap_or(tag);

            // Microscope
            let microscope_type = all_tag_positions(inst_xml, "Microscope")
                .into_iter()
                .next()
                .and_then(|pos| xml_attr(start_tag_at(inst_xml, pos), "Type"));
            let microscope_model = all_tag_positions(inst_xml, "Microscope")
                .into_iter()
                .next()
                .and_then(|p| xml_attr(start_tag_at(inst_xml, p), "Model"));
            let microscope_manufacturer = all_tag_positions(inst_xml, "Microscope")
                .into_iter()
                .next()
                .and_then(|p| xml_attr(start_tag_at(inst_xml, p), "Manufacturer"));
            let microscope_serial_number = all_tag_positions(inst_xml, "Microscope")
                .into_iter()
                .next()
                .and_then(|p| xml_attr(start_tag_at(inst_xml, p), "SerialNumber"));

            // Objectives
            let objectives = all_tag_positions(inst_xml, "Objective")
                .into_iter()
                .map(|p| {
                    let t = start_tag_at(inst_xml, p);
                    OmeObjective {
                        iris: xml_attr(t, "Iris").and_then(|v| v.parse().ok()),
                        id: xml_attr(t, "ID"),
                        model: xml_attr(t, "Model"),
                        manufacturer: xml_attr(t, "Manufacturer"),
                        serial_number: xml_attr(t, "SerialNumber"),
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
                        zoom: xml_attr(t, "Zoom").and_then(|s| s.parse().ok()),
                        amplification_gain: xml_attr(t, "AmplificationGain")
                            .and_then(|s| s.parse().ok()),
                        serial_number: xml_attr(t, "SerialNumber"),
                        lot_number: xml_attr(t, "LotNumber"),
                        gain: xml_attr(t, "Gain").and_then(|s| s.parse().ok()),
                        offset: xml_attr(t, "Offset").and_then(|s| s.parse().ok()),
                        voltage: xml_attr(t, "Voltage").and_then(|s| s.parse().ok()).map(
                            |value: f64| match xml_attr(t, "VoltageUnit").as_deref().unwrap_or("V")
                            {
                                "mV" => value / 1000.0,
                                "µV" | "uV" => value / 1_000_000.0,
                                "kV" => value * 1000.0,
                                _ => value,
                            },
                        ),
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
                        laser_type: xml_attr(t, "Type"),
                        laser_medium: xml_attr(t, "LaserMedium"),
                        serial_number: xml_attr(t, "SerialNumber"),
                        lot_number: xml_attr(t, "LotNumber"),
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
                    let end = if t.trim_end().ends_with("/>") {
                        p + t.len()
                    } else {
                        find_end_tag(inst_xml, "Filter", p + t.len()).unwrap_or(p + t.len())
                    };
                    let body = &inst_xml[p..end];
                    let range = all_tag_positions(body, "TransmittanceRange")
                        .into_iter()
                        .next()
                        .map(|p| start_tag_at(body, p))
                        .unwrap_or(t);
                    OmeFilter {
                        id: xml_attr(t, "ID"),
                        model: xml_attr(t, "Model"),
                        manufacturer: xml_attr(t, "Manufacturer"),
                        filter_type: xml_attr(t, "Type"),
                        cut_in: xml_attr(range, "CutIn").and_then(|s| s.parse().ok()),
                        cut_out: xml_attr(range, "CutOut").and_then(|s| s.parse().ok()),
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

            let filter_sets = all_tag_positions(inst_xml, "FilterSet")
                .into_iter()
                .map(|pos| {
                    let tag = start_tag_at(inst_xml, pos);
                    let end = if tag.trim_end().ends_with("/>") {
                        pos + tag.len()
                    } else {
                        find_end_tag(inst_xml, "FilterSet", pos + tag.len())
                            .unwrap_or(pos + tag.len())
                    };
                    let body = &inst_xml[pos..end];
                    let refs = |name| {
                        all_tag_positions(body, name)
                            .into_iter()
                            .filter_map(|i| xml_attr(start_tag_at(body, i), "ID"))
                            .collect()
                    };
                    OmeFilterSet {
                        serial_number: xml_attr(tag, "SerialNumber"),
                        lot_number: xml_attr(tag, "LotNumber"),
                        id: xml_attr(tag, "ID"),
                        model: xml_attr(tag, "Model"),
                        manufacturer: xml_attr(tag, "Manufacturer"),
                        excitation_filter_ids: refs("ExcitationFilterRef"),
                        dichroic_ref: all_tag_positions(body, "DichroicRef")
                            .into_iter()
                            .next()
                            .and_then(|i| xml_attr(start_tag_at(body, i), "ID")),
                        emission_filter_ids: refs("EmissionFilterRef"),
                    }
                })
                .collect();
            OmeInstrument {
                microscope_type,
                filter_sets,
                id,
                microscope_model,
                microscope_manufacturer,
                microscope_serial_number,
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

            // Label
            for sp in all_tag_positions(union_xml, "Label") {
                let t = start_tag_at(union_xml, sp);
                let x = xml_attr(t, "X").and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let y = xml_attr(t, "Y").and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let text = xml_attr(t, "Text");
                let font_size = xml_attr(t, "FontSize").and_then(|s| s.parse().ok());
                let stroke_width = xml_attr(t, "StrokeWidth").and_then(|s| s.parse().ok());
                let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
                let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
                let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
                shapes.push(OmeShape::Label {
                    x,
                    y,
                    text,
                    font_size,
                    stroke_width,
                    the_z,
                    the_t,
                    the_c,
                });
            }

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

            // Mask
            for sp in all_tag_positions(union_xml, "Mask") {
                let t = start_tag_at(union_xml, sp);
                let x = xml_attr(t, "X").and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let y = xml_attr(t, "Y").and_then(|s| s.parse().ok()).unwrap_or(0.0);
                let width = xml_attr(t, "Width")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let height = xml_attr(t, "Height")
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0.0);
                let stroke_color = xml_attr(t, "StrokeColor").and_then(|s| s.parse().ok());
                let fill_color = xml_attr(t, "FillColor").and_then(|s| s.parse().ok());
                let the_z = xml_attr(t, "TheZ").and_then(|s| s.parse().ok());
                let the_t = xml_attr(t, "TheT").and_then(|s| s.parse().ok());
                let the_c = xml_attr(t, "TheC").and_then(|s| s.parse().ok());
                let bin_data = find_end_tag(union_xml, "Mask", sp).and_then(|end| {
                    let end = union_xml[end..]
                        .find('>')
                        .map(|gt| end + gt + 1)
                        .unwrap_or(union_xml.len());
                    let mask_xml = &union_xml[sp..end];
                    xml_inner_text(mask_xml, "BinData").map(|text| base64_decode(&text))
                });
                shapes.push(OmeShape::Mask {
                    x,
                    y,
                    width,
                    height,
                    stroke_color,
                    fill_color,
                    bin_data,
                    the_z,
                    the_t,
                    the_c,
                });
            }

            // Preserve the common OME Shape attributes on each parsed geometry.
            // The geometry loops above group shapes by type, so use that same order.
            let mut shape_index = 0;
            for name in [
                "Label",
                "Rectangle",
                "Ellipse",
                "Point",
                "Line",
                "Polygon",
                "Polyline",
                "Mask",
            ] {
                for sp in all_tag_positions(union_xml, name) {
                    let tag = start_tag_at(union_xml, sp);
                    let font_size = if name == "Label" {
                        None
                    } else {
                        xml_attr(tag, "FontSize").and_then(|v| v.parse().ok())
                    };
                    let stroke_width = if name == "Label" {
                        None
                    } else {
                        xml_attr(tag, "StrokeWidth").and_then(|v| v.parse().ok())
                    };
                    let transform = if tag.trim_end().ends_with("/>") {
                        None
                    } else {
                        find_end_tag(union_xml, name, sp).and_then(|end| {
                            let body = &union_xml[sp..end];
                            let pos = all_tag_positions(body, "Transform").into_iter().next()?;
                            let t = start_tag_at(body, pos);
                            let mut matrix = [0.0; 6];
                            for (i, key) in ["A00", "A01", "A02", "A10", "A11", "A12"]
                                .into_iter()
                                .enumerate()
                            {
                                matrix[i] = xml_attr(t, key)?.parse().ok()?;
                            }
                            Some(matrix)
                        })
                    };
                    if font_size.is_some() || stroke_width.is_some() || transform.is_some() {
                        if let Some(shape) = shapes.get_mut(shape_index) {
                            *shape = OmeShape::Styled {
                                shape: Box::new(shape.clone()),
                                font_size,
                                stroke_width,
                                transform,
                            };
                        }
                    }
                    shape_index += 1;
                }
            }
            let positions: Vec<usize> = [
                "Label",
                "Rectangle",
                "Ellipse",
                "Point",
                "Line",
                "Polygon",
                "Polyline",
                "Mask",
            ]
            .into_iter()
            .flat_map(|name| all_tag_positions(union_xml, name))
            .collect();
            let mut ordered: Vec<_> = positions.into_iter().zip(shapes).collect();
            ordered.sort_by_key(|(pos, _)| *pos);
            let shapes = ordered.into_iter().map(|(_, shape)| shape).collect();
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

fn czi_instruments(xml: &str) -> Vec<OmeInstrument> {
    let Some(information) = first_xml_block(xml, "Information") else {
        return Vec::new();
    };
    let Some(instrument_xml) = first_xml_block(information, "Instrument") else {
        return Vec::new();
    };

    let objectives = czi_objectives(xml);
    let detectors = czi_detectors(instrument_xml);
    let filters = czi_filters(instrument_xml);
    let dichroics = czi_dichroics(instrument_xml);
    if objectives.is_empty() && detectors.is_empty() && filters.is_empty() && dichroics.is_empty() {
        return Vec::new();
    }

    vec![OmeInstrument {
        id: Some(create_lsid("Instrument", &[0])),
        microscope_model: czi_microscope_model(xml),
        objectives,
        detectors,
        filters,
        dichroics,
        ..Default::default()
    }]
}

fn czi_microscope_model(xml: &str) -> Option<String> {
    let microscope_pos = all_tag_positions(xml, "Microscope").into_iter().next()?;
    let tag = start_tag_at(xml, microscope_pos);
    xml_attr(tag, "Name").filter(|s| !s.is_empty())
}

fn czi_objectives(xml: &str) -> Vec<OmeObjective> {
    let Some(objective_changer) = first_xml_block(xml, "ObjectiveChanger") else {
        return Vec::new();
    };
    all_tag_positions(objective_changer, "Objective")
        .into_iter()
        .enumerate()
        .map(|(index, pos)| {
            let tag = start_tag_at(objective_changer, pos);
            let end = find_end_tag(
                objective_changer,
                "Objective",
                pos + start_tag_at(objective_changer, pos).len(),
            )
            .unwrap_or_else(|| pos + tag.len());
            let body = objective_changer.get(pos..end).unwrap_or(tag);
            OmeObjective {
                id: Some(create_lsid("Objective", &[0, index])),
                model: xml_attr(tag, "Model").filter(|s| !s.is_empty()),
                serial_number: xml_attr(tag, "UniqueName").filter(|s| !s.is_empty()),
                nominal_magnification: xml_inner_text(body, "Magnification")
                    .and_then(|s| s.parse::<f64>().ok()),
                lens_na: xml_inner_text(body, "NumericalAperture")
                    .and_then(|s| s.parse::<f64>().ok()),
                immersion: xml_inner_text(body, "Immersions").filter(|s| !s.is_empty()),
                correction: Some("Other".into()),
                working_distance: xml_inner_text(body, "WorkingDistance")
                    .and_then(|s| s.parse::<f64>().ok()),
                ..Default::default()
            }
        })
        .collect()
}

fn czi_detectors(instrument_xml: &str) -> Vec<OmeDetector> {
    let Some(detectors_xml) = first_xml_block(instrument_xml, "Detectors") else {
        return Vec::new();
    };
    let mut detectors = Vec::new();
    for pos in all_tag_positions(detectors_xml, "Detector") {
        let tag = start_tag_at(detectors_xml, pos);
        let mut id = xml_attr(tag, "Id").unwrap_or_default().replace(' ', "");
        if id.is_empty() {
            continue;
        }
        if !id.starts_with("Detector:") {
            id = format!("Detector:{id}");
        }
        if detectors
            .iter()
            .any(|detector: &OmeDetector| detector.id.as_deref() == Some(id.as_str()))
        {
            continue;
        }
        let end = find_end_tag(detectors_xml, "Detector", pos + tag.len())
            .unwrap_or_else(|| pos + tag.len());
        let body = detectors_xml.get(pos..end).unwrap_or(tag);
        detectors.push(OmeDetector {
            id: Some(id),
            model: xml_attr(tag, "Name").filter(|s| !s.is_empty()),
            gain: xml_inner_text(body, "Gain").and_then(|s| s.parse::<f64>().ok()),
            offset: xml_inner_text(body, "Offset").and_then(|s| s.parse::<f64>().ok()),
            detector_type: xml_inner_text(body, "Type").filter(|s| !s.is_empty()),
            ..Default::default()
        });
    }
    detectors
}

fn czi_filters(instrument_xml: &str) -> Vec<OmeFilter> {
    let Some(filters_xml) = first_xml_block(instrument_xml, "Filters") else {
        return Vec::new();
    };
    all_tag_positions(filters_xml, "Filter")
        .into_iter()
        .enumerate()
        .map(|(index, pos)| {
            let tag = start_tag_at(filters_xml, pos);
            let end = find_end_tag(filters_xml, "Filter", pos + tag.len())
                .unwrap_or_else(|| pos + tag.len());
            let body = filters_xml.get(pos..end).unwrap_or(tag);
            let transmittance = first_xml_block(body, "TransmittanceRange").unwrap_or(body);
            OmeFilter {
                id: xml_attr(tag, "Id")
                    .filter(|s| !s.is_empty())
                    .or_else(|| Some(create_lsid("Filter", &[0, index]))),
                model: xml_attr(tag, "Name").filter(|s| !s.is_empty()),
                filter_type: xml_inner_text(body, "Type").filter(|s| !s.is_empty()),
                cut_in: xml_inner_text(transmittance, "CutIn").and_then(|s| s.parse().ok()),
                cut_out: xml_inner_text(transmittance, "CutOut").and_then(|s| s.parse().ok()),
                ..Default::default()
            }
        })
        .collect()
}

fn czi_dichroics(instrument_xml: &str) -> Vec<OmeDichroic> {
    let Some(dichroics_xml) = first_xml_block(instrument_xml, "Dichroics") else {
        return Vec::new();
    };
    all_tag_positions(dichroics_xml, "Dichroic")
        .into_iter()
        .enumerate()
        .map(|(index, pos)| {
            let tag = start_tag_at(dichroics_xml, pos);
            OmeDichroic {
                id: xml_attr(tag, "Id")
                    .filter(|s| !s.is_empty())
                    .or_else(|| Some(create_lsid("Dichroic", &[0, index]))),
                model: xml_attr(tag, "Name").filter(|s| !s.is_empty()),
                ..Default::default()
            }
        })
        .collect()
}

fn first_xml_block<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let pos = all_tag_positions(xml, tag).into_iter().next()?;
    let start_tag = start_tag_at(xml, pos);
    let content_start = pos + start_tag.len();
    let end = if start_tag.trim_end().ends_with("/>") {
        content_start
    } else {
        find_end_tag(xml, tag, content_start).unwrap_or(content_start)
    };
    xml.get(pos..end)
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

fn metadata_value_i32(value: Option<&MetadataValue>) -> Option<i32> {
    match value {
        Some(MetadataValue::Int(v)) => i32::try_from(*v).ok(),
        Some(MetadataValue::Float(v)) if v.is_finite() && v.fract() == 0.0 => {
            i32::try_from(*v as i64).ok()
        }
        Some(MetadataValue::String(v)) => v.trim().parse::<i32>().ok(),
        _ => None,
    }
}

fn metadata_value_bytes(value: Option<&MetadataValue>) -> Option<Vec<u8>> {
    match value {
        Some(MetadataValue::Bytes(v)) => Some(v.clone()),
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

fn generic_stage_label_from_metadata(meta: &ImageMetadata) -> Option<OmeStageLabel> {
    let name = metadata_string_by_suffix(&meta.series_metadata, &["image.stage_label.name"]);
    let x = metadata_finite_f64_by_suffix(&meta.series_metadata, &["image.stage_label.x"]);
    let y = metadata_finite_f64_by_suffix(&meta.series_metadata, &["image.stage_label.y"]);
    let z = metadata_finite_f64_by_suffix(&meta.series_metadata, &["image.stage_label.z"]);
    if name.is_some() || x.is_some() || y.is_some() || z.is_some() {
        Some(OmeStageLabel {
            name,
            x,
            y,
            z,
            ..Default::default()
        })
    } else {
        None
    }
}

fn generic_image_roi_refs_from_metadata(meta: &ImageMetadata) -> Vec<String> {
    let mut refs: Vec<(usize, String)> = meta
        .series_metadata
        .iter()
        .filter_map(|(key, value)| {
            let index = key.strip_prefix("image.roi_ref.")?.parse::<usize>().ok()?;
            match value {
                MetadataValue::String(id) if !id.trim().is_empty() => Some((index, id.clone())),
                MetadataValue::Int(roi_index) if *roi_index >= 0 => {
                    Some((index, create_lsid("ROI", &[*roi_index as usize])))
                }
                _ => None,
            }
        })
        .collect();
    refs.sort_by_key(|(index, _)| *index);
    refs.into_iter().map(|(_, id)| id).collect()
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
    if let Some(count) = metadata_u32_by_suffix(&meta.series_metadata, &["rgb_channel_count"])
        .filter(|count| *count > 0)
    {
        return count;
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

fn zct_coords_from_order(
    order: crate::common::metadata::DimensionOrder,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    plane_index: u32,
) -> (u32, u32, u32) {
    let mut rem = plane_index;
    let mut z = 0;
    let mut c = 0;
    let mut t = 0;
    for axis in match order {
        crate::common::metadata::DimensionOrder::XYZCT => ['Z', 'C', 'T'],
        crate::common::metadata::DimensionOrder::XYZTC => ['Z', 'T', 'C'],
        crate::common::metadata::DimensionOrder::XYCZT => ['C', 'Z', 'T'],
        crate::common::metadata::DimensionOrder::XYCTZ => ['C', 'T', 'Z'],
        crate::common::metadata::DimensionOrder::XYTCZ => ['T', 'C', 'Z'],
        crate::common::metadata::DimensionOrder::XYTZC => ['T', 'Z', 'C'],
    } {
        match axis {
            'Z' => {
                z = rem % size_z;
                rem /= size_z;
            }
            'C' => {
                c = rem % size_c;
                rem /= size_c;
            }
            'T' => {
                t = rem % size_t;
                rem /= size_t;
            }
            _ => {}
        }
    }
    (z, c, t)
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
            let t_size = meta.size_t.max(1);
            let (the_z, the_c, the_t) =
                zct_coords_from_order(meta.dimension_order, z_size, c_size, t_size, plane_index);
            planes.push(OmePlane {
                the_z,
                the_c,
                the_t,
                delta_t,
                exposure_time,
                position_x,
                position_y,
                position_z,
                ..Default::default()
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

    let shape_type = metadata_string_by_suffix(
        metadata,
        &[
            &format!("{prefix}.shape"),
            &format!("{prefix}.shape_type"),
            &format!("{prefix}.type"),
        ],
    )
    .unwrap_or_default();
    if shape_type.eq_ignore_ascii_case("mask") {
        let width = width?;
        let height = height?;
        return Some(OmeShape::Mask {
            x,
            y,
            width,
            height,
            stroke_color: metadata_value_i32(metadata_by_suffix(
                metadata,
                &[
                    &format!("{prefix}.stroke_color"),
                    &format!("{prefix}.strokecolor"),
                ],
            )),
            fill_color: metadata_value_i32(metadata_by_suffix(
                metadata,
                &[
                    &format!("{prefix}.fill_color"),
                    &format!("{prefix}.fillcolor"),
                ],
            )),
            bin_data: metadata_value_bytes(metadata_by_suffix(
                metadata,
                &[&format!("{prefix}.bin_data"), &format!("{prefix}.bindata")],
            )),
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
        iris: None,
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
        serial_number: metadata_string_by_suffix_filtered(
            metadata,
            &["objective.serial_number", "objective.0.serial_number"],
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
        || objective.serial_number.is_some()
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
        zoom: metadata_value_f64(metadata_by_suffix_filtered(
            metadata,
            &["detector.zoom", "detector.0.zoom"],
            &excluded_prefixes,
        ))
        .filter(|v| v.is_finite()),
        amplification_gain: None,
        serial_number: None,
        lot_number: None,
        voltage: metadata_value_f64(metadata_by_suffix_filtered(
            metadata,
            &["detector.voltage", "detector.0.voltage"],
            &excluded_prefixes,
        ))
        .filter(|v| v.is_finite()),
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
        || detector.offset.is_some()
        || detector.voltage.is_some()
        || detector.zoom.is_some())
    .then_some(detector)
}

fn generic_light_source_from_metadata(meta: &ImageMetadata) -> Option<OmeLightSource> {
    let metadata = &meta.series_metadata;
    let excluded_prefixes = ["xlef.lms."];
    let light_source = OmeLightSource {
        laser_type: None,
        laser_medium: None,
        serial_number: None,
        lot_number: None,
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
        user_name: metadata_string_by_suffix(
            metadata,
            &[
                "experimenter.user_name",
                "experimenter.username",
                "user.username",
            ],
        ),
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

    (experimenter.user_name.is_some()
        || experimenter.first_name.is_some()
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

/// Java `FormatTools.getPhysicalSize(value, unit)`, normalised to this
/// crate's convention: a length in any `UnitsLength` unit becomes micrometres
/// with unit `None`; `"pixel"` and `"reference frame"` keep their value and
/// unit; an unrecognised or absent unit means µm, as Java's
/// `EnumerationException` fallback does. `None` for 0 and infinite values.
pub fn get_physical_size(value: f64, unit: Option<&str>) -> Option<(f64, Option<String>)> {
    if value == 0.0 || value >= f64::INFINITY || value.is_nan() {
        return None;
    }
    match unit {
        Some(unit @ ("pixel" | "reference frame")) => Some((value, Some(unit.to_string()))),
        Some(unit) => Some((microns_per_unit(unit).map_or(value, |m| value * m), None)),
        None => Some((value, None)),
    }
}

/// Micrometres per `UnitsLength` unit (ome-xml symbols); `None` for units that
/// are not convertible lengths or are unknown.
fn microns_per_unit(unit: &str) -> Option<f64> {
    Some(match unit {
        "Ym" => 1e30,
        "Zm" => 1e27,
        "Em" => 1e24,
        "Pm" => 1e21,
        "Tm" => 1e18,
        "Gm" => 1e15,
        "Mm" => 1e12,
        "km" => 1e9,
        "hm" => 1e8,
        "dam" => 1e7,
        "m" => 1e6,
        "dm" => 1e5,
        "cm" => 1e4,
        "mm" => 1e3,
        "µm" => 1.0,
        "nm" => 1e-3,
        "pm" => 1e-6,
        "fm" => 1e-9,
        "am" => 1e-12,
        "zm" => 1e-15,
        "ym" => 1e-18,
        "Å" => 1e-4,
        "thou" => 25.4,
        "li" => 25_400.0 / 12.0,
        "in" => 25_400.0,
        "ft" => 304_800.0,
        "yd" => 914_400.0,
        "mi" => 1_609_344_000.0,
        "ua" => 1.495_978_707e17,
        "ly" => 9.460_730_472_580_8e21,
        "pc" => 3.085_677_581_491_367e22,
        "pt" => 25_400.0 / 72.0,
        _ => return None,
    })
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
    fn self_closing_channel_and_filter_set_do_not_inherit_sibling_children() {
        let xml = r#"<OME><Instrument ID="Instrument:0"><FilterSet ID="FilterSet:0"/><FilterSet ID="FilterSet:1"><ExcitationFilterRef ID="Filter:1"/></FilterSet></Instrument><Image ID="Image:0"><Pixels><Channel ID="Channel:0"/><Channel ID="Channel:1"><LightSourceSettings ID="LightSource:1"/><DetectorSettings ID="Detector:1"/><FilterSetRef ID="FilterSet:1"/><LightPath><EmissionFilterRef ID="Filter:1"/></LightPath></Channel></Pixels></Image></OME>"#;
        let ome = OmeMetadata::from_ome_xml(xml);
        assert!(ome.instruments[0].filter_sets[0]
            .excitation_filter_ids
            .is_empty());
        assert_eq!(
            ome.instruments[0].filter_sets[1].excitation_filter_ids,
            ["Filter:1"]
        );
        let first = &ome.images[0].channels[0];
        assert!(first.light_source_settings_id.is_none());
        assert!(first.detector_ref.is_none());
        assert!(first.filter_set_ref.is_none());
        assert_eq!(
            ome.images[0].channels[1].filter_set_ref.as_deref(),
            Some("FilterSet:1")
        );
        assert!(ome.images[0].light_paths[0].emission_filter_ids.is_empty());
    }

    #[test]
    fn rich_ome_graph_roundtrip_and_optional_java_schema_validation() {
        // Arbitrary input image IDs ensure HCS refs resolve by ID, not a numeric suffix.
        let input = r#"<OME><Plate ID="Plate:custom" Name="plate &amp; one" Rows="8" Columns="12"><Well ID="Well:custom" Row="2" Column="3"><WellSample ID="WellSample:custom" Index="7" PositionX="-1.5" PositionY="2.5"><ImageRef ID="Image:second"/></WellSample></Well></Plate><Screen ID="Screen:custom" Name="screen" ProtocolDescription="protocol"/><Instrument ID="Instrument:0"><Microscope Type="Other"/><Laser ID="LightSource:0" Type="Other" LaserMedium="Other"/><Detector ID="Detector:0" Type="PMT" Voltage="100"/><Objective ID="Objective:0" Immersion="Oil" Correction="PlanApo"/><FilterSet ID="FilterSet:0"><ExcitationFilterRef ID="Filter:0"/><DichroicRef ID="Dichroic:0"/><EmissionFilterRef ID="Filter:0"/></FilterSet><Filter ID="Filter:0" Type="BandPass"><TransmittanceRange CutIn="400" CutOut="500"/></Filter><Dichroic ID="Dichroic:0"/></Instrument><Image ID="Image:first"><AcquisitionDate>2026-10-26T12:00:00</AcquisitionDate><Description>description</Description><InstrumentRef ID="Instrument:0"/><ObjectiveSettings ID="Objective:0" RefractiveIndex="1.5"/><ImagingEnvironment Temperature="20"/><StageLabel Name="origin" X="1"/><Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint8" SizeX="2" SizeY="3" SizeZ="1" SizeC="1" SizeT="1"><Channel ID="Channel:0" SamplesPerPixel="1"><LightSourceSettings ID="LightSource:0"/><DetectorSettings ID="Detector:0"/><FilterSetRef ID="FilterSet:0"/><LightPath><EmissionFilterRef ID="Filter:0"/></LightPath></Channel><MetadataOnly/><Plane TheZ="0" TheC="0" TheT="0"/></Pixels><ROIRef ID="ROI:0"/></Image><Image ID="Image:second"><Pixels ID="Pixels:1" DimensionOrder="XYCZT" Type="uint16" SizeX="4" SizeY="5" SizeZ="2" SizeC="1" SizeT="1"><Channel ID="Channel:1" SamplesPerPixel="1"/><MetadataOnly/></Pixels></Image><StructuredAnnotations><CommentAnnotation ID="Annotation:0"><Value>annotation</Value></CommentAnnotation></StructuredAnnotations><ROI ID="ROI:0"><Union><Rectangle ID="Shape:0" X="1" Y="2" Width="3" Height="4" FontSize="12" StrokeWidth="0.5"><Transform A00="1" A01="0" A02="10" A10="0" A11="1" A12="20"/></Rectangle></Union></ROI></OME>"#;
        let mut ome = OmeMetadata::from_ome_xml(input);
        ome.images[1].modulo_z = Some(crate::metadata::ModuloAnnotation {
            parent_dimension: "Z".into(),
            modulo_type: "angle".into(),
            start: 0.0,
            step: 90.0,
            end: 90.0,
            unit: "degree".into(),
            labels: vec!["first & zero".into(), "second".into()],
        });
        let xml = ome.to_ome_xml(&ImageMetadata::default());
        let parsed = OmeMetadata::from_ome_xml(&xml);
        assert_eq!(parsed.images.len(), 2);
        assert_eq!(parsed.images[1].pixels.as_ref().unwrap().size_x, 4);
        assert_eq!(parsed.plates[0].name.as_deref(), Some("plate & one"));
        let well = &parsed.plates[0].wells[0];
        assert_eq!((well.row, well.column), (2, 3));
        let sample = &well.well_samples[0];
        assert_eq!(
            (sample.index, sample.image_ref, sample.position_x),
            (7, Some(1), Some(-1.5))
        );
        assert_eq!(
            parsed.screens[0].protocol_description.as_deref(),
            Some("protocol")
        );
        assert_eq!(parsed.instruments[0].filters[0].cut_in, Some(400.0));
        assert_eq!(parsed.images[0].imaging_environment_temperature, Some(20.0));
        assert_eq!(
            parsed.images[0].channels[0].detector_ref.as_deref(),
            Some("Detector:0")
        );
        assert_eq!(
            parsed.images[0].light_paths[0].emission_filter_ids,
            ["Filter:0"]
        );
        assert_eq!(
            parsed.images[1].modulo_z.as_ref().unwrap().labels,
            ["first & zero", "second"]
        );
        assert_eq!(xml.matches("<MetadataOnly/>").count(), 2);
        assert!(xml.find("<StructuredAnnotations>").unwrap() < xml.find("<ROI ID=").unwrap());
        assert!(xml.find("</Pixels>").unwrap() < xml.find("<ROIRef").unwrap());
        if let Some(jar) = std::env::var_os("BIOFORMATS_RS_OME_SCHEMA_JAR") {
            let dir =
                std::env::temp_dir().join(format!("bioformats-ome-schema-{}", std::process::id()));
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("rich.ome.xml"), &xml).unwrap();
            use crate::FormatWriter;
            let tiff_meta = ImageMetadata {
                size_x: 2,
                size_y: 3,
                size_c: 2,
                image_count: 2,
                ..Default::default()
            };
            let mut writer = crate::tiff::TiffWriter::new().with_auto_ome_xml();
            writer.set_metadata(&tiff_meta).unwrap();
            writer.set_id(&dir.join("mapped.ome.tif")).unwrap();
            writer.save_bytes(0, &[1, 2, 3, 4, 5, 6]).unwrap();
            writer.save_bytes(1, &[7, 8, 9, 10, 11, 12]).unwrap();
            writer.close().unwrap();

            std::fs::write(
                dir.join("ValidateOme.java"),
                r#"import java.nio.file.*;
import loci.common.services.ServiceFactory;
import loci.formats.services.OMEXMLService;
public class ValidateOme { public static void main(String[] args) throws Exception {
  String xml = Files.readString(Path.of(args[0]));
  OMEXMLService service = new ServiceFactory().getInstance(OMEXMLService.class);
  if (!service.validateOMEXML(xml, false)) throw new Exception("OME schema validation failed");
  System.out.println("OME 2016-06 schema validation passed");
  loci.formats.ImageReader reader = new loci.formats.ImageReader();
  reader.setId(args[1]);
  if (reader.getImageCount() != 2 || reader.getSizeC() != 2) throw new Exception("TIFF dimensions changed");
  for (int p = 0; p < 2; p++) {
    byte[] bytes = reader.openBytes(p);
    if (bytes.length != 6) throw new Exception("TIFF plane length changed");
    for (int k = 0; k < 6; k++) if (bytes[k] != 1 + p * 6 + k) throw new Exception("TIFF pixel mapping changed");
  }
  reader.close();
  System.out.println("Java TIFF core and both mapped planes passed");
} }"#,
            )
            .unwrap();
            let jar = std::fs::canonicalize(jar).unwrap();
            let compile = std::process::Command::new("javac")
                .arg("-cp")
                .arg(&jar)
                .arg(dir.join("ValidateOme.java"))
                .output()
                .unwrap();
            assert!(
                compile.status.success(),
                "{}",
                String::from_utf8_lossy(&compile.stderr)
            );
            let cp = std::env::join_paths([jar.as_path(), dir.as_path()]).unwrap();
            let validate = std::process::Command::new("java")
                .arg("-cp")
                .arg(cp)
                .arg("ValidateOme")
                .arg(dir.join("rich.ome.xml"))
                .arg(dir.join("mapped.ome.tif"))
                .output()
                .unwrap();
            assert!(
                validate.status.success(),
                "{}\n{}",
                String::from_utf8_lossy(&validate.stdout),
                String::from_utf8_lossy(&validate.stderr)
            );
            eprintln!("{}", String::from_utf8_lossy(&validate.stdout));
        }
    }

    #[test]
    fn ome_xml_preserves_different_series_pixel_descriptions() {
        use crate::common::metadata::DimensionOrder;
        use crate::common::pixel_type::PixelType;
        let first = ImageMetadata {
            size_x: 12,
            size_y: 8,
            ..Default::default()
        };
        let second = ImageMetadata {
            size_x: 4,
            size_y: 3,
            size_z: 2,
            size_c: 3,
            size_t: 5,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 12,
            dimension_order: DimensionOrder::XYZTC,
            is_little_endian: false,
            ..Default::default()
        };
        let mut ome = OmeMetadata::default();
        ome.populate_pixels(&first, 0).unwrap();
        ome.populate_pixels(&second, 1).unwrap();
        let xml = ome.to_ome_xml(&first);
        let parsed = OmeMetadata::from_ome_xml(&xml);
        let p0 = parsed.images[0].pixels.as_ref().unwrap();
        let p1 = parsed.images[1].pixels.as_ref().unwrap();
        assert_eq!((p0.size_x, p0.size_y), (12, 8));
        assert_eq!(
            (p1.size_x, p1.size_y, p1.size_z, p1.size_c, p1.size_t),
            (4, 3, 2, 3, 5)
        );
        assert_eq!(p1.pixel_type, PixelType::Uint16);
        assert_eq!(p1.dimension_order, DimensionOrder::XYZTC);
        assert_eq!(p1.bits_per_pixel, 12);
        assert!(!p1.is_little_endian);
    }

    #[test]
    fn ome_optical_component_and_objective_settings_roundtrip() {
        let input = r#"<OME><Instrument ID="Instrument:0"><Microscope Type="Other"/><Objective ID="Objective:0:0" Iris="true"/><Detector ID="Detector:0:0" Zoom="2" AmplificationGain="3" SerialNumber="det-1" LotNumber="det-lot"/><Laser ID="LightSource:0:0" Type="Other" LaserMedium="Other" SerialNumber="laser-1" LotNumber="laser-lot"/><FilterSet ID="FilterSet:0:0" Model="cube" Manufacturer="maker" SerialNumber="cube-1" LotNumber="cube-lot"><ExcitationFilterRef ID="Filter:0:0"/><DichroicRef ID="Dichroic:0:0"/><EmissionFilterRef ID="Filter:0:1"/></FilterSet></Instrument><Image ID="Image:0"><InstrumentRef ID="Instrument:0"/><ObjectiveSettings ID="Objective:0:0" Medium="Oil" CorrectionCollar="0.17" RefractiveIndex="1.515"/><Pixels ID="Pixels:0" DimensionOrder="XYCZT" Type="uint8" SizeX="2" SizeY="2" SizeC="1" SizeZ="1" SizeT="1"><Channel ID="Channel:0:0" SamplesPerPixel="1"><FilterSetRef ID="FilterSet:0:0"/></Channel></Pixels></Image></OME>"#;
        let mut metadata = OmeMetadata::from_ome_xml(input);
        // Assert both parsing and serialization, including reference identity.
        for iteration in 0..2 {
            let inst = &metadata.instruments[0];
            assert_eq!(inst.microscope_type.as_deref(), Some("Other"));
            assert_eq!(inst.objectives[0].iris, Some(true));
            assert_eq!(
                (inst.detectors[0].zoom, inst.detectors[0].amplification_gain),
                (Some(2.0), Some(3.0))
            );
            assert_eq!(inst.detectors[0].serial_number.as_deref(), Some("det-1"));
            assert_eq!(inst.light_sources[0].laser_medium.as_deref(), Some("Other"));
            assert_eq!(inst.light_sources[0].laser_type.as_deref(), Some("Other"));
            let set = &inst.filter_sets[0];
            assert_eq!(set.serial_number.as_deref(), Some("cube-1"));
            assert_eq!(set.lot_number.as_deref(), Some("cube-lot"));
            assert_eq!(set.excitation_filter_ids, ["Filter:0:0"]);
            assert_eq!(set.dichroic_ref.as_deref(), Some("Dichroic:0:0"));
            assert_eq!(set.emission_filter_ids, ["Filter:0:1"]);
            let image = &metadata.images[0];
            assert_eq!(
                image.channels[0].filter_set_ref.as_deref(),
                Some("FilterSet:0:0")
            );
            assert_eq!(image.objective_settings_medium.as_deref(), Some("Oil"));
            assert_eq!(image.objective_settings_correction_collar, Some(0.17));
            if iteration == 0 {
                let xml = metadata.to_ome_xml(&crate::common::metadata::ImageMetadata {
                    size_x: 2,
                    size_y: 2,
                    ..Default::default()
                });
                metadata = OmeMetadata::from_ome_xml(&xml);
            }
        }
    }

    #[test]
    fn ome_rich_shape_and_experimenter_fields_roundtrip() {
        let input = r#"<OME><Experimenter ID="Experimenter:0" UserName="analyst&amp;1"/><Instrument ID="Instrument:0"><Detector ID="Detector:0:0" Voltage="-250" VoltageUnit="mV"/></Instrument><ROI ID="ROI:0"><Union><Rectangle ID="Shape:0:0" X="1" Y="2" Width="3" Height="4" FontSize="12" StrokeWidth="0.5" TheZ="2"><Transform A00="0" A01="-1" A02="10" A10="1" A11="0" A12="20"/></Rectangle></Union></ROI></OME>"#;
        let ome = OmeMetadata::from_ome_xml(input);
        assert_eq!(ome.experimenters[0].user_name.as_deref(), Some("analyst&1"));
        assert_eq!(ome.instruments[0].detectors[0].voltage, Some(-0.25));
        let xml = ome.to_ome_xml(&crate::common::metadata::ImageMetadata::default());
        assert!(xml.contains(r#"UserName="analyst&amp;1""#));
        assert!(xml.contains(r#"Voltage="-0.25" VoltageUnit="V""#));
        assert!(xml.contains(r#"ID="Shape:0:0""#));
        let reparsed = OmeMetadata::from_ome_xml(&xml);
        match &reparsed.rois[0].shapes[0] {
            OmeShape::Styled {
                shape,
                font_size,
                stroke_width,
                transform,
            } => {
                assert_eq!(*font_size, Some(12.0));
                assert_eq!(*stroke_width, Some(0.5));
                assert_eq!(*transform, Some([0.0, -1.0, 10.0, 1.0, 0.0, 20.0]));
                assert!(matches!(
                    shape.as_ref(),
                    OmeShape::Rectangle {
                        x: 1.0,
                        the_z: Some(2),
                        ..
                    }
                ));
            }
            other => panic!("lost shape style: {other:?}"),
        }
    }

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

    #[test]
    fn ome_xml_roundtrips_image_roi_refs() {
        let mut meta = ImageMetadata {
            size_x: 8,
            size_y: 8,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            image_count: 1,
            ..ImageMetadata::default()
        };
        meta.series_metadata.insert(
            "image.roi_ref.0".into(),
            MetadataValue::String("ROI:0".into()),
        );
        meta.series_metadata.insert(
            "roi.0.shape".into(),
            MetadataValue::String("rectangle".into()),
        );
        meta.series_metadata
            .insert("roi.0.x".into(), MetadataValue::Float(1.0));
        meta.series_metadata
            .insert("roi.0.y".into(), MetadataValue::Float(2.0));
        meta.series_metadata
            .insert("roi.0.width".into(), MetadataValue::Float(3.0));
        meta.series_metadata
            .insert("roi.0.height".into(), MetadataValue::Float(4.0));

        let ome = OmeMetadata::from_image_metadata(&meta);
        assert_eq!(ome.images[0].roi_refs, vec!["ROI:0".to_string()]);
        let xml = ome.to_ome_xml(&meta);
        assert!(xml.contains(r#"<ROIRef ID="ROI:0"/>"#));

        let parsed = OmeMetadata::from_ome_xml(&xml);
        assert_eq!(parsed.images[0].roi_refs, vec!["ROI:0".to_string()]);
    }

    #[test]
    fn ome_xml_roundtrips_stage_label() {
        let mut meta = ImageMetadata {
            size_x: 4,
            size_y: 4,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            image_count: 1,
            ..ImageMetadata::default()
        };
        meta.series_metadata.insert(
            "image.stage_label.name".into(),
            MetadataValue::String("Position".into()),
        );
        meta.series_metadata
            .insert("image.stage_label.z".into(), MetadataValue::Float(42.5));

        let ome = OmeMetadata::from_image_metadata(&meta);
        assert_eq!(
            ome.images[0].stage_label.as_ref().unwrap().name.as_deref(),
            Some("Position")
        );
        assert_eq!(ome.images[0].stage_label.as_ref().unwrap().z, Some(42.5));
        let xml = ome.to_ome_xml(&meta);
        assert!(xml.contains(r#"<StageLabel Name="Position" Z="42.5"/>"#));

        let parsed = OmeMetadata::from_ome_xml(&xml);
        let parsed_label = parsed.images[0].stage_label.as_ref().unwrap();
        assert_eq!(parsed_label.name.as_deref(), Some("Position"));
        assert_eq!(parsed_label.z, Some(42.5));
        // no ZUnit was recorded, so reading back yields the XSD default
        // (StageLabel.getZUnitXsdDefault())
        assert_eq!(parsed_label.z_unit.as_deref(), Some("reference frame"));
    }

    #[test]
    fn ome_xml_pads_sparse_non_rgb_channels_to_declared_size_c() {
        let xml = r#"
<OME>
  <Image ID="Image:0">
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint16" SizeX="2" SizeY="2" SizeZ="1" SizeC="3" SizeT="1">
      <Channel ID="Channel:0:0" Name="DAPI" SamplesPerPixel="1"/>
    </Pixels>
  </Image>
</OME>"#;

        let ome = OmeMetadata::from_ome_xml(xml);

        assert_eq!(ome.images[0].channels.len(), 3);
        assert_eq!(ome.images[0].channels[0].name.as_deref(), Some("DAPI"));
        assert_eq!(ome.images[0].channels[1].samples_per_pixel, 1);
        assert_eq!(ome.images[0].channels[2].samples_per_pixel, 1);
    }

    #[test]
    fn get_physical_size_matches_java_units() {
        assert_eq!(
            get_physical_size(2.0, Some("pixel")),
            Some((2.0, Some("pixel".into())))
        );
        assert_eq!(get_physical_size(2.0, Some("nm")), Some((0.002, None)));
        assert_eq!(get_physical_size(2.0, Some("in")), Some((50_800.0, None)));
        // not UnitsLength symbols: Java falls back to µm
        assert_eq!(get_physical_size(2.0, Some("micron")), Some((2.0, None)));
        assert_eq!(get_physical_size(2.0, Some("inch")), Some((2.0, None)));
        assert_eq!(get_physical_size(2.0, None), Some((2.0, None)));
        assert_eq!(get_physical_size(0.0, Some("pixel")), None);
        assert_eq!(get_physical_size(f64::INFINITY, None), None);
    }

    #[test]
    fn ome_xml_keeps_non_length_physical_size_units() {
        let xml = r#"
<OME>
  <Image ID="Image:0">
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint16" SizeX="2" SizeY="2" SizeZ="1" SizeC="1" SizeT="1"
      PhysicalSizeX="2.0" PhysicalSizeXUnit="pixel" PhysicalSizeY="500" PhysicalSizeYUnit="nm" PhysicalSizeZ="2.0">
      <Channel ID="Channel:0:0" SamplesPerPixel="1"/>
    </Pixels>
  </Image>
</OME>"#;

        let ome = OmeMetadata::from_ome_xml(xml);
        let image = &ome.images[0];
        assert_eq!(image.physical_size_x, Some(2.0));
        assert_eq!(image.physical_size_x_unit.as_deref(), Some("pixel"));
        assert_eq!(image.physical_size_y, Some(0.5));
        assert_eq!(image.physical_size_y_unit, None);
        assert_eq!(image.physical_size_z, Some(2.0));
        assert_eq!(image.physical_size_z_unit, None);
    }

    #[test]
    fn ome_xml_does_not_split_packed_rgb_channel() {
        let xml = r#"
<OME>
  <Image ID="Image:0">
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint8" SizeX="2" SizeY="2" SizeZ="1" SizeC="3" SizeT="1">
      <Channel ID="Channel:0:0" SamplesPerPixel="3"/>
    </Pixels>
  </Image>
</OME>"#;

        let ome = OmeMetadata::from_ome_xml(xml);

        assert_eq!(ome.images[0].channels.len(), 1);
        assert_eq!(ome.images[0].channels[0].samples_per_pixel, 3);
    }

    #[test]
    fn ome_xml_roundtrips_objective_serial_and_settings_refractive_index() {
        let meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..ImageMetadata::default()
        };
        let mut ome = OmeMetadata::from_image_metadata(&meta);
        ome.instruments = vec![OmeInstrument {
            id: Some(create_lsid("Instrument", &[0])),
            objectives: vec![OmeObjective {
                id: Some(create_lsid("Objective", &[0, 0])),
                serial_number: Some("OBJ-123".into()),
                ..Default::default()
            }],
            ..Default::default()
        }];
        ome.images[0].instrument_ref = Some(0);
        ome.images[0].objective_ref = Some(0);
        ome.images[0].objective_settings_refractive_index = Some(1.515);

        let xml = ome.to_ome_xml(&meta);

        assert!(xml.contains(r#"SerialNumber="OBJ-123""#));
        assert!(xml.contains(r#"<ObjectiveSettings ID="Objective:0:0" RefractiveIndex="1.515"/>"#));
        let parsed = OmeMetadata::from_ome_xml(&xml);
        assert_eq!(
            parsed.instruments[0].objectives[0].serial_number.as_deref(),
            Some("OBJ-123")
        );
        assert_eq!(
            parsed.images[0].objective_settings_refractive_index,
            Some(1.515)
        );
    }

    #[test]
    fn ome_xml_roundtrips_mask_shape_with_bin_data() {
        let mut meta = ImageMetadata {
            size_x: 8,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..ImageMetadata::default()
        };
        meta.series_metadata
            .insert("roi.0.shape".into(), MetadataValue::String("mask".into()));
        meta.series_metadata
            .insert("roi.0.x".into(), MetadataValue::Float(0.0));
        meta.series_metadata
            .insert("roi.0.y".into(), MetadataValue::Float(0.0));
        meta.series_metadata
            .insert("roi.0.width".into(), MetadataValue::Float(8.0));
        meta.series_metadata
            .insert("roi.0.height".into(), MetadataValue::Float(1.0));
        meta.series_metadata
            .insert("roi.0.stroke_color".into(), MetadataValue::Int(-16776961));
        meta.series_metadata
            .insert("roi.0.fill_color".into(), MetadataValue::Int(-16776961));
        meta.series_metadata.insert(
            "roi.0.bin_data".into(),
            MetadataValue::Bytes(vec![0b1010_0000]),
        );

        let ome = OmeMetadata::from_image_metadata(&meta);
        let xml = ome.to_ome_xml(&meta);

        assert!(xml.contains(r#"<Mask ID="Shape:0:0" X="0" Y="0" Width="8" Height="1" StrokeColor="-16776961" FillColor="-16776961"><BinData Length="1">oA==</BinData></Mask>"#));
        let parsed = OmeMetadata::from_ome_xml(&xml);
        match &parsed.rois[0].shapes[0] {
            OmeShape::Mask {
                width,
                height,
                stroke_color,
                fill_color,
                bin_data,
                ..
            } => {
                assert_eq!((*width, *height), (8.0, 1.0));
                assert_eq!(*stroke_color, Some(-16776961));
                assert_eq!(*fill_color, Some(-16776961));
                assert_eq!(bin_data.as_deref(), Some(&[0b1010_0000][..]));
            }
            other => panic!("expected mask shape, got {other:?}"),
        }
    }

    #[test]
    fn plane_metadata_uses_dimension_order_for_zct_coords() {
        let mut meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 2,
            size_c: 2,
            size_t: 2,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 8,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
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
        for plane in 0..meta.image_count {
            meta.series_metadata.insert(
                format!("plane.{plane}.delta_t"),
                MetadataValue::Float(plane as f64),
            );
        }

        let ome = OmeMetadata::from_image_metadata(&meta);
        let planes = &ome.images.first().expect("OME image").planes;
        let coords: Vec<(u32, u32, u32)> = planes
            .iter()
            .map(|plane| (plane.the_z, plane.the_c, plane.the_t))
            .collect();

        assert_eq!(
            coords,
            vec![
                (0, 0, 0),
                (1, 0, 0),
                (0, 0, 1),
                (1, 0, 1),
                (0, 1, 0),
                (1, 1, 0),
                (0, 1, 1),
                (1, 1, 1),
            ]
        );
    }

    #[test]
    fn ome_xml_plane_and_stage_label_position_units() {
        let xml = r#"
<OME>
  <Image ID="Image:0">
    <StageLabel Name="Pos" X="1.5" XUnit="mm" Y="2.5"/>
    <Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint8" SizeX="1" SizeY="1" SizeZ="1" SizeC="1" SizeT="3">
      <Channel ID="Channel:0:0" SamplesPerPixel="1"/>
      <Plane TheZ="0" TheC="0" TheT="0" PositionX="1.5" PositionXUnit="mm" PositionY="2.5" PositionYUnit="µm"/>
      <Plane TheZ="0" TheC="0" TheT="1" PositionX="3.0" PositionZ="4.0"/>
      <Plane TheZ="0" TheC="0" TheT="2" DeltaT="1.0"/>
    </Pixels>
  </Image>
</OME>"#;

        let ome = OmeMetadata::from_ome_xml(xml);
        let planes = &ome.images[0].planes;
        assert_eq!(planes[0].position_x, Some(1.5));
        assert_eq!(planes[0].position_x_unit.as_deref(), Some("mm"));
        assert_eq!(planes[0].position_y_unit.as_deref(), Some("µm"));
        assert_eq!(planes[0].position_z_unit, None);
        // Plane.getPositionXUnitXsdDefault() is "reference frame"
        assert_eq!(
            planes[1].position_x_unit.as_deref(),
            Some("reference frame")
        );
        assert_eq!(
            planes[1].position_z_unit.as_deref(),
            Some("reference frame")
        );
        assert_eq!(planes[1].position_y, None);
        assert_eq!(planes[1].position_y_unit, None);
        assert_eq!(planes[2].position_x, None);
        assert_eq!(planes[2].position_x_unit, None);

        // StageLabel.getXUnitXsdDefault() is "reference frame"
        let stage_label = ome.images[0].stage_label.as_ref().unwrap();
        assert_eq!(stage_label.x, Some(1.5));
        assert_eq!(stage_label.x_unit.as_deref(), Some("mm"));
        assert_eq!(stage_label.y_unit.as_deref(), Some("reference frame"));
        assert_eq!(stage_label.z, None);
        assert_eq!(stage_label.z_unit, None);
    }

    #[test]
    fn ome_xml_roundtrips_plane_and_stage_label_position_units() {
        let meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..ImageMetadata::default()
        };
        let mut ome = OmeMetadata::from_image_metadata(&meta);
        ome.images[0].planes = vec![OmePlane {
            position_x: Some(1.5),
            position_x_unit: Some("mm".into()),
            position_y: Some(2.5),
            position_y_unit: Some("µm".into()),
            position_z: Some(3.5),
            position_z_unit: Some("reference frame".into()),
            ..Default::default()
        }];
        ome.images[0].stage_label = Some(OmeStageLabel {
            name: Some("Position".into()),
            x: Some(10.0),
            x_unit: Some("nm".into()),
            z: Some(30.0),
            z_unit: Some("reference frame".into()),
            ..Default::default()
        });

        let xml = ome.to_ome_xml(&meta);
        assert!(xml.contains(r#"PositionXUnit="mm""#));
        assert!(xml.contains(r#"XUnit="nm""#));
        let parsed = OmeMetadata::from_ome_xml(&xml);
        let plane = &parsed.images[0].planes[0];
        assert_eq!(plane.position_x, Some(1.5));
        assert_eq!(plane.position_x_unit.as_deref(), Some("mm"));
        assert_eq!(plane.position_y_unit.as_deref(), Some("µm"));
        assert_eq!(plane.position_z_unit.as_deref(), Some("reference frame"));
        let stage_label = parsed.images[0].stage_label.as_ref().unwrap();
        assert_eq!(stage_label.x, Some(10.0));
        assert_eq!(stage_label.x_unit.as_deref(), Some("nm"));
        assert_eq!(stage_label.y, None);
        assert_eq!(stage_label.y_unit, None);
        assert_eq!(stage_label.z_unit.as_deref(), Some("reference frame"));
    }
}
