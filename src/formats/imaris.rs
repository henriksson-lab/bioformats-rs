//! Imaris IMS format reader (HDF5-based).
//!
//! Reads Bitplane/Oxford Instruments Imaris .ims files.
//! These are HDF5 files containing multi-channel, multi-timepoint,
//! multi-resolution 3-D fluorescence microscopy volumes.
//!
//! Group layout:
//!   DataSetInfo/Image — attributes X, Y, Z (string), ExtMin*/ExtMax* (physical size)
//!   DataSetInfo/Channel N — attribute Name, Color
//!   DataSet/ResolutionLevel R/TimePoint T/Channel C/Data — uint8 or uint16 [z,y,x]

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, LookupTable, MetadataValue};
use crate::common::ome_metadata::{
    create_lsid, OmeDetector, OmeInstrument, OmeLightSource, OmeObjective, OmeROI, OmeShape,
};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use hdf5_pure_rust::format::messages::datatype::DatatypeClass;
use hdf5_pure_rust::{HyperslabDim, Selection};

const IMARIS_SURPASS_METADATA_NODE_LIMIT: usize = 1024;
const IMARIS_SURPASS_DATASET_VALUE_LIMIT: u64 = 32;
const IMARIS_SURPASS_STATISTICS_TABLE_VALUE_LIMIT: u64 = 4096;

pub struct ImarisReader {
    path: Option<PathBuf>,
    file: Option<hdf5_pure_rust::File>,
    // One ImageMetadata per Imaris ResolutionLevel_N. Index 0 is full-resolution.
    // This is the flat, file-order list; the `series` grouping below references
    // these by absolute Imaris level so that (series, resolution) coordinates can
    // be mapped back to the on-disk ResolutionLevel path for I/O.
    resolutions: Vec<ImageMetadata>,
    // Java SubResolutionFormatReader series model (ImarisHDFReader.initFile
    // ~287-311): each series is a list of absolute Imaris ResolutionLevel indices
    // (its resolutions, finest first). Resolution levels whose Z/C/T match
    // level 0 collapse into one pyramid series; mismatched levels split off into
    // their own series.
    series: Vec<Vec<usize>>,
    current_series: usize,
    current_resolution: usize,
    // pixel type for raw reads
    bytes_per_sample: usize,
    // Spatial extents from DataSetInfo/Image: [minX,minY,minZ,maxX,maxY,maxZ].
    extents: Option<[f64; 6]>,
    // RecordingEntrySample/Line/PlaneSpacing from DataSetInfo/Image, in X/Y/Z order.
    recording_spacing: [Option<f64>; 3],
    image_description: Option<String>,
    // Per-channel names from DataSetInfo/Channel N.
    channel_names: Vec<Option<String>>,
    channel_colors: Vec<Option<i32>>,
    // Original per-channel `Color` attribute as the 0..1 RGB doubles Java keeps
    // in its `colors` list. Retained (in addition to the packed RGBA in
    // `channel_colors`) because the get8BitLookupTable/get16BitLookupTable ramp
    // computation needs the raw double components, not the rounded bytes.
    channel_colors_normalized: Vec<Option<[f64; 3]>>,
    channel_emission_wavelengths: Vec<Option<f64>>,
    channel_excitation_wavelengths: Vec<Option<f64>>,
    instrument: ImarisInstrumentMetadata,
    // Cache of the most recently decoded plane so that repeated reads of the
    // same plane do not re-read from disk. Keyed by (resolution, t, c, z).
    // Mirrors the per-Z-block buffer cache in ImarisHDFReader.java. The new
    // hdf5-pure-rust crate supports hyperslab partial I/O, so we read only the
    // requested z-plane via read_slice instead of the whole channel volume.
    cache: Option<VolumeCache>,
}

/// Cached decoded plane for one (resolution, timepoint, channel, z) location.
struct VolumeCache {
    res: usize,
    t: usize,
    c: usize,
    z: usize,
    raw: Vec<u8>,
}

impl ImarisReader {
    pub fn new() -> Self {
        ImarisReader {
            path: None,
            file: None,
            resolutions: Vec::new(),
            series: Vec::new(),
            current_series: 0,
            current_resolution: 0,
            bytes_per_sample: 1,
            extents: None,
            recording_spacing: [None; 3],
            image_description: None,
            channel_names: Vec::new(),
            channel_colors: Vec::new(),
            channel_colors_normalized: Vec::new(),
            channel_emission_wavelengths: Vec::new(),
            channel_excitation_wavelengths: Vec::new(),
            instrument: ImarisInstrumentMetadata::default(),
            cache: None,
        }
    }

    /// Map the active (series, resolution) coordinate to the absolute Imaris
    /// ResolutionLevel index used for the on-disk DataSet path. Mirrors Java's
    /// getCoreIndex(): the resolution within the current series.
    fn current_imaris_level(&self) -> Option<usize> {
        self.series
            .get(self.current_series)
            .and_then(|group| group.get(self.current_resolution))
            .copied()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_ims_path(label: &str) -> PathBuf {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_rs_{label}_{unique}.ims"))
    }

    fn make_statistics_file<F>(label: &str, build_statistics: F) -> PathBuf
    where
        F: FnOnce(&mut hdf5_pure_rust::hl::writable_file::WritableGroup<'_>),
    {
        let path = tmp_ims_path(label);
        let mut writable = hdf5_pure_rust::WritableFile::create(&path).unwrap();
        {
            let mut scene = writable.create_group("Scene").unwrap();
            let mut surface = scene.create_group("Surfaces 0").unwrap();
            let mut statistics = surface.create_group("Statistics").unwrap();
            build_statistics(&mut statistics);
        }
        writable.flush().unwrap();
        path
    }

    #[test]
    fn imaris_channel_value_delimiter_stripping_mirrors_java() {
        // Mirrors ImarisHDFReader.parseAttributes()'s DELIMITERS loop: for each
        // of {" ", "-", "."} in turn, keep the substring after its first match.
        assert_eq!(strip_imaris_channel_value_delimiters("Gain 7"), "7");
        assert_eq!(
            strip_imaris_channel_value_delimiters("Channel-1.5"),
            "5"
        );
        assert_eq!(strip_imaris_channel_value_delimiters("488"), "488");
    }

    #[test]
    fn imaris_channel_parsed_attributes_translate_gain_pinhole_min_max_mode() {
        let path = tmp_ims_path("channel_parsed_attributes");
        let mut writable = hdf5_pure_rust::WritableFile::create(&path).unwrap();
        {
            let mut info = writable.create_group("DataSetInfo").unwrap();
            let mut channel = info.create_group("Channel 0").unwrap();
            channel.add_fixed_ascii_attr("Gain", "7", 8).unwrap();
            channel.add_fixed_ascii_attr("Pinhole", "1.2", 8).unwrap();
            channel.add_fixed_ascii_attr("Min", "0", 8).unwrap();
            channel.add_fixed_ascii_attr("Max", "255", 8).unwrap();
            channel
                .add_fixed_ascii_attr("MicroscopyMode", "Confocal", 16)
                .unwrap();
        }
        writable.flush().unwrap();

        let file = hdf5_pure_rust::File::open(&path).unwrap();
        let ch_group = file.group("DataSetInfo/Channel 0").unwrap();
        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        let c = 0u32;
        if let Some(gain) = read_str_attr(&ch_group, "Gain")
            .map(|v| strip_imaris_channel_value_delimiters(&v))
            .and_then(|v| parse_imaris_f64(&v))
        {
            meta_map.insert(
                format!("imaris.channel.{c}.gain"),
                MetadataValue::Float(gain),
            );
        }
        if let Some(pinhole) = read_str_attr(&ch_group, "Pinhole")
            .map(|v| strip_imaris_channel_value_delimiters(&v))
            .and_then(|v| parse_imaris_f64(&v))
        {
            meta_map.insert(
                format!("imaris.channel.{c}.pinhole"),
                MetadataValue::Float(pinhole),
            );
        }
        if let Some(min) = read_str_attr(&ch_group, "Min")
            .map(|v| strip_imaris_channel_value_delimiters(&v))
            .and_then(|v| parse_imaris_f64(&v))
        {
            meta_map.insert(format!("imaris.channel.{c}.min"), MetadataValue::Float(min));
        }
        if let Some(max) = read_str_attr(&ch_group, "Max")
            .map(|v| strip_imaris_channel_value_delimiters(&v))
            .and_then(|v| parse_imaris_f64(&v))
        {
            meta_map.insert(format!("imaris.channel.{c}.max"), MetadataValue::Float(max));
        }
        if let Some(mode) = read_str_attr(&ch_group, "MicroscopyMode")
            .map(|v| strip_imaris_channel_value_delimiters(&v))
            .filter(|v| !v.is_empty())
        {
            meta_map.insert(
                format!("imaris.channel.{c}.microscopy_mode"),
                MetadataValue::String(mode),
            );
        }

        assert!(matches!(
            meta_map.get("imaris.channel.0.gain"),
            Some(MetadataValue::Float(v)) if *v == 7.0
        ));
        assert!(matches!(
            meta_map.get("imaris.channel.0.pinhole"),
            Some(MetadataValue::Float(v)) if *v == 2.0
        ));
        assert!(matches!(
            meta_map.get("imaris.channel.0.min"),
            Some(MetadataValue::Float(v)) if *v == 0.0
        ));
        assert!(matches!(
            meta_map.get("imaris.channel.0.max"),
            Some(MetadataValue::Float(v)) if *v == 255.0
        ));
        assert!(matches!(
            meta_map.get("imaris.channel.0.microscopy_mode"),
            Some(MetadataValue::String(v)) if v == "Confocal"
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imaris_surpass_bounded_larger_statistics_table_translates_values() {
        let path = make_statistics_file("larger_statistics", |statistics| {
            let names = (0..12)
                .map(|index| format!("Stat {index}"))
                .collect::<Vec<_>>();
            let name_refs = names.iter().map(String::as_str).collect::<Vec<_>>();
            statistics
                .new_dataset_builder("Names")
                .shape(&[12])
                .write_fixed_ascii_strings(&name_refs, 12)
                .unwrap();
            let values = (1..=36).map(|value| value as f64).collect::<Vec<_>>();
            statistics
                .new_dataset_builder("Values")
                .shape(&[12, 3])
                .write::<f64>(&values)
                .unwrap();
        });
        let file = hdf5_pure_rust::File::open(&path).unwrap();
        let mut metadata = HashMap::new();

        collect_imaris_surpass_statistics_table(
            &file,
            "Scene/Surfaces 0/Statistics",
            "imaris.surpass.Scene.Surfaces_0.Statistics",
            &mut metadata,
        );

        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.names_shape"),
            Some(MetadataValue::String(value)) if value == "12"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.value_shape"),
            Some(MetadataValue::String(value)) if value == "12 3"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.value_count"),
            Some(MetadataValue::Int(36))
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.layout"),
            Some(MetadataValue::String(value)) if value == "stat_rows"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.Stat_0"),
            Some(MetadataValue::String(value)) if value == "1 2 3"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.Stat_11"),
            Some(MetadataValue::String(value)) if value == "34 35 36"
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imaris_surpass_oversized_statistics_table_remains_diagnostic_only() {
        let path = make_statistics_file("oversized_statistics", |statistics| {
            statistics
                .new_dataset_builder("Names")
                .shape(&[2])
                .write_fixed_ascii_strings(&["Area", "Volume"], 12)
                .unwrap();
            statistics
                .new_dataset_builder("Values")
                .shape(&[2049, 2])
                .write::<f64>(&vec![1.0; 4098])
                .unwrap();
        });
        let file = hdf5_pure_rust::File::open(&path).unwrap();
        let mut metadata = HashMap::new();

        collect_imaris_surpass_statistics_table(
            &file,
            "Scene/Surfaces 0/Statistics",
            "imaris.surpass.Scene.Surfaces_0.Statistics",
            &mut metadata,
        );

        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.value_shape"),
            Some(MetadataValue::String(value)) if value == "2049 2"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.value_count"),
            Some(MetadataValue::Int(4098))
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.value_status"),
            Some(MetadataValue::String(value)) if value == "not_read_large_statistics_table"
        ));
        assert!(!metadata.contains_key("imaris.surpass.Scene.Surfaces_0.Statistics.table.layout"));
        assert!(!metadata.contains_key("imaris.surpass.Scene.Surfaces_0.Statistics.table.Area"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imaris_surpass_singleton_axis_statistics_table_translates_values() {
        let path = make_statistics_file("singleton_axis_statistics", |statistics| {
            statistics
                .new_dataset_builder("Names")
                .shape(&[2])
                .write_fixed_ascii_strings(&["Area", "Volume"], 12)
                .unwrap();
            statistics
                .new_dataset_builder("Values")
                .shape(&[1, 2, 3])
                .write::<f64>(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
                .unwrap();
        });
        let file = hdf5_pure_rust::File::open(&path).unwrap();
        let mut metadata = HashMap::new();

        collect_imaris_surpass_statistics_table(
            &file,
            "Scene/Surfaces 0/Statistics",
            "imaris.surpass.Scene.Surfaces_0.Statistics",
            &mut metadata,
        );

        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.value_shape"),
            Some(MetadataValue::String(value)) if value == "1 2 3"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.effective_value_shape"),
            Some(MetadataValue::String(value)) if value == "2 3"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.layout"),
            Some(MetadataValue::String(value)) if value == "stat_rows"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.Area"),
            Some(MetadataValue::String(value)) if value == "1 2 3"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.Volume"),
            Some(MetadataValue::String(value)) if value == "4 5 6"
        ));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imaris_surpass_complex_statistics_table_reports_unsupported_layout() {
        let path = make_statistics_file("complex_statistics_layout", |statistics| {
            statistics
                .new_dataset_builder("Names")
                .shape(&[2])
                .write_fixed_ascii_strings(&["Area", "Volume"], 12)
                .unwrap();
            statistics
                .new_dataset_builder("Values")
                .shape(&[3, 3])
                .write::<f64>(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0, 9.0])
                .unwrap();
        });
        let file = hdf5_pure_rust::File::open(&path).unwrap();
        let mut metadata = HashMap::new();

        collect_imaris_surpass_statistics_table(
            &file,
            "Scene/Surfaces 0/Statistics",
            "imaris.surpass.Scene.Surfaces_0.Statistics",
            &mut metadata,
        );

        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.stat_count"),
            Some(MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.row_count"),
            Some(MetadataValue::Int(3))
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.column_count"),
            Some(MetadataValue::Int(3))
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.layout_status"),
            Some(MetadataValue::String(value)) if value == "unsupported_statistics_layout"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.layout_reason"),
            Some(MetadataValue::String(value)) if value == "statistic_name_count_mismatch"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.supported_layouts"),
            Some(MetadataValue::String(value)) if value == "stat_rows,stat_columns"
        ));
        assert!(!metadata.contains_key("imaris.surpass.Scene.Surfaces_0.Statistics.table.Area"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imaris_surpass_higher_rank_statistics_table_reports_unsupported_rank() {
        let path = make_statistics_file("higher_rank_statistics_layout", |statistics| {
            statistics
                .new_dataset_builder("Names")
                .shape(&[2])
                .write_fixed_ascii_strings(&["Area", "Volume"], 12)
                .unwrap();
            statistics
                .new_dataset_builder("Values")
                .shape(&[2, 2, 2])
                .write::<f64>(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0, 7.0, 8.0])
                .unwrap();
        });
        let file = hdf5_pure_rust::File::open(&path).unwrap();
        let mut metadata = HashMap::new();

        collect_imaris_surpass_statistics_table(
            &file,
            "Scene/Surfaces 0/Statistics",
            "imaris.surpass.Scene.Surfaces_0.Statistics",
            &mut metadata,
        );

        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.value_shape"),
            Some(MetadataValue::String(value)) if value == "2 2 2"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.layout_status"),
            Some(MetadataValue::String(value)) if value == "unsupported_statistics_layout"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.layout_reason"),
            Some(MetadataValue::String(value)) if value == "unsupported_statistics_rank"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.Statistics.table.supported_layouts"),
            Some(MetadataValue::String(value)) if value == "stat_rows,stat_columns"
        ));
        assert!(!metadata.contains_key("imaris.surpass.Scene.Surfaces_0.Statistics.table.Area"));

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imaris_surpass_group_provenance_preserves_object_graph_shape() {
        let path = tmp_ims_path("surpass_group_provenance");
        let mut writable = hdf5_pure_rust::WritableFile::create(&path).unwrap();
        {
            let mut scene = writable.create_group("Scene").unwrap();
            let mut surface = scene.create_group("Surfaces 0").unwrap();
            surface
                .add_fixed_ascii_attr("Name", "Membranes", 16)
                .unwrap();
            surface
                .new_dataset_builder("Vertices")
                .shape(&[2, 3])
                .write::<f64>(&[1.0, 2.0, 3.0, 4.0, 5.0, 6.0])
                .unwrap();
            surface.create_group("Statistics").unwrap();
            let mut spots = scene.create_group("Spots_2").unwrap();
            spots.add_fixed_ascii_attr("Name", "Seeds", 16).unwrap();
            spots
                .new_dataset_builder("TrackIds")
                .shape(&[2])
                .write::<i64>(&[10, 11])
                .unwrap();
        }
        writable.flush().unwrap();

        let file = hdf5_pure_rust::File::open(&path).unwrap();
        let mut metadata = HashMap::new();
        collect_imaris_surpass_metadata(&file, &mut metadata);

        assert!(matches!(
            metadata.get("imaris.surpass.Scene.hdf5_path"),
            Some(MetadataValue::String(value)) if value == "Scene"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.member_count"),
            Some(MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.child_group_count"),
            Some(MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.dataset_count"),
            Some(MetadataValue::Int(0))
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.parent_key"),
            Some(MetadataValue::String(value)) if value == "imaris.surpass.Scene"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.object_kind"),
            Some(MetadataValue::String(value)) if value == "Surfaces"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.object_index"),
            Some(MetadataValue::Int(0))
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.child_group_count"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Surfaces_0.dataset_count"),
            Some(MetadataValue::Int(1))
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Spots_2.object_kind"),
            Some(MetadataValue::String(value)) if value == "Spots"
        ));
        assert!(matches!(
            metadata.get("imaris.surpass.Scene.Spots_2.object_index"),
            Some(MetadataValue::Int(2))
        ));

        let _ = std::fs::remove_file(path);
    }
}

impl Default for ImarisReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Read a string attribute from an HDF5 group (tries VarLenAscii then FixedAscii).
fn read_str_attr(group: &hdf5_pure_rust::Group, attr: &str) -> Option<String> {
    let a = group.attr(attr).ok()?;
    // String-typed attributes: read directly. Imaris stores strings as arrays
    // of single-character (|S1) elements, so read_strings() returns one entry
    // per character — concatenate them all rather than taking just the first.
    if let Ok(v) = a.read_strings() {
        if !v.is_empty() {
            let joined: String = v.concat();
            let trimmed = joined.trim_matches('\0').trim();
            if !trimmed.is_empty() {
                return Some(trimmed.to_string());
            }
        }
    }
    let s = a.read_string();
    if !s.is_empty() {
        return Some(s.trim_matches('\0').trim().to_string());
    }
    // Numeric attributes: format the scalar value as a string.
    if let Some(i) = a.read_scalar_i64() {
        return Some(i.to_string());
    }
    if let Some(f) = a.read_scalar_f64() {
        return Some(f.to_string());
    }
    None
}

struct ImsParse {
    resolutions: Vec<ImageMetadata>,
    bytes_per_sample: usize,
    extents: Option<[f64; 6]>,
    recording_spacing: [Option<f64>; 3],
    image_description: Option<String>,
    channel_names: Vec<Option<String>>,
    channel_colors: Vec<Option<i32>>,
    channel_colors_normalized: Vec<Option<[f64; 3]>>,
    channel_emission_wavelengths: Vec<Option<f64>>,
    channel_excitation_wavelengths: Vec<Option<f64>>,
    instrument: ImarisInstrumentMetadata,
}

#[derive(Clone, Default)]
struct ImarisInstrumentMetadata {
    microscope_model: Option<String>,
    microscope_manufacturer: Option<String>,
    objective_model: Option<String>,
    objective_manufacturer: Option<String>,
    objective_nominal_magnification: Option<f64>,
    objective_calibrated_magnification: Option<f64>,
    objective_lens_na: Option<f64>,
    objective_immersion: Option<String>,
    objective_correction: Option<String>,
    objective_working_distance: Option<f64>,
    detector_model: Option<String>,
    detector_manufacturer: Option<String>,
    detector_type: Option<String>,
    detector_gain: Option<f64>,
    detector_offset: Option<f64>,
    light_source_model: Option<String>,
    light_source_manufacturer: Option<String>,
    light_source_type: Option<String>,
    light_source_power: Option<f64>,
}

fn parse_ims(path: &Path) -> Result<ImsParse> {
    let file = hdf5_pure_rust::File::open(path)
        .map_err(|e| BioFormatsError::Format(format!("HDF5 open error: {e}")))?;

    // ── Read dimensions from DataSetInfo/Image ──────────────────────────────
    let img_group = file
        .group("DataSetInfo/Image")
        .map_err(|e| BioFormatsError::Format(format!("DataSetInfo/Image missing: {e}")))?;

    // Spatial extents (ExtMin0..2 / ExtMax0..2) for deriving physical sizes.
    let ext_val = |attr: &str| -> Option<f64> {
        read_str_attr(&img_group, attr).and_then(|s| s.trim().parse::<f64>().ok())
    };
    let extents = match (
        ext_val("ExtMin0"),
        ext_val("ExtMin1"),
        ext_val("ExtMin2"),
        ext_val("ExtMax0"),
        ext_val("ExtMax1"),
        ext_val("ExtMax2"),
    ) {
        (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f)) => Some([a, b, c, d, e, f]),
        _ => None,
    };
    let recording_spacing = [
        ext_val("RecordingEntrySampleSpacing"),
        ext_val("RecordingEntryLineSpacing"),
        ext_val("RecordingEntryPlaneSpacing"),
    ];
    let image_description = read_str_attr(&img_group, "Description");

    // The DataSetInfo/Image X/Y/Z attributes are advisory and unreliable — some
    // writers store 1/1/1 (observed in real .ims files). The authoritative pixel
    // dimensions are the full-resolution Data dataset shape [z, y, x], so derive
    // X/Y/Z from it instead of the attributes.
    let (size_z, size_y, size_x) = ims_level_dims(&file, 0)?;

    // ── Count channels ──────────────────────────────────────────────────────
    // Count groups named "Channel N" or "Channel_N" under DataSetInfo.
    let ds_info = file
        .group("DataSetInfo")
        .map_err(|e| BioFormatsError::Format(format!("DataSetInfo missing: {e}")))?;
    let mut size_c: u32 = 0;
    if let Ok(members) = hdf5_group_members(&ds_info) {
        size_c = count_imaris_indexed_members(&members, "Channel ")?
            .max(count_imaris_indexed_members(&members, "Channel_")?);
    }
    if size_c == 0 {
        let tp0_path = ims_timepoint_group_path(&file, 0, 0).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(
                "Imaris: no channel metadata and TimePoint 0 missing".into(),
            )
        })?;
        let tp0 = file.group(&tp0_path).map_err(|e| {
            BioFormatsError::UnsupportedFormat(format!(
                "Imaris: no channel metadata and {tp0_path} missing: {e}"
            ))
        })?;
        size_c = hdf5_group_members(&tp0)
            .map(|members| {
                count_imaris_indexed_members(&members, "Channel ").and_then(|space_count| {
                    count_imaris_indexed_members(&members, "Channel_")
                        .map(|underscore_count| space_count.max(underscore_count))
                })
            })
            .unwrap_or(Ok(0))?;
        if size_c == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Imaris: no channels found".into(),
            ));
        }
    }

    // ── Count timepoints from DataSet/ResolutionLevel 0 ────────────────────
    let size_t: u32 = if let Some(rl0_path) = ims_resolution_group_path(&file, 0) {
        let rl0 = file
            .group(&rl0_path)
            .map_err(|e| BioFormatsError::Format(format!("Imaris: cannot open {rl0_path}: {e}")))?;
        if let Ok(members) = hdf5_group_members(&rl0) {
            count_imaris_indexed_members(&members, "TimePoint ")?
                .max(count_imaris_indexed_members(&members, "TimePoint_")?)
        } else {
            0
        }
    } else {
        0
    };
    if size_t == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Imaris: no timepoints found".into(),
        ));
    }

    // ── Count resolution levels ─────────────────────────────────────────────
    let n_resolutions: usize = if let Ok(ds_group) = file.group("DataSet") {
        if let Ok(members) = hdf5_group_members(&ds_group) {
            count_imaris_indexed_members(&members, "ResolutionLevel ")?
                .max(count_imaris_indexed_members(&members, "ResolutionLevel_")?)
                as usize
        } else {
            0
        }
    } else {
        0
    };
    if n_resolutions == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Imaris: no resolution levels found".into(),
        ));
    }

    // ── Determine pixel type from first Data dataset ────────────────────────
    let data_path = ims_data_path(&file, 0, 0, 0).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(
            "Imaris: missing DataSet/ResolutionLevel 0/TimePoint 0/Channel 0/Data or DataSet/ResolutionLevel_0/TimePoint_0/Channel_0/Data".into(),
        )
    })?;
    let ds = file.dataset(&data_path).map_err(|e| {
        BioFormatsError::UnsupportedFormat(format!("Imaris: missing {data_path}: {e}"))
    })?;
    let (pixel_type, bytes_per_sample) = {
        let dtype = ds.dtype().map_err(|e| {
            BioFormatsError::Format(format!("Imaris: cannot read dtype for {data_path}: {e}"))
        })?;
        // Java ImarisHDFReader.java:336-337 maps the sample array type to the
        // pixel type, including FLOAT and DOUBLE. Distinguish float/double from
        // the integer types of the same element size by inspecting the dtype
        // class and element size.
        let class = dtype.class();
        let size = dtype.size();
        let signed = dtype.is_signed().unwrap_or(false);
        match (class, size) {
            (DatatypeClass::FloatingPoint, 4) => (PixelType::Float32, 4usize),
            (DatatypeClass::FloatingPoint, 8) => (PixelType::Float64, 8usize),
            (DatatypeClass::FixedPoint, 1) => {
                if signed {
                    (PixelType::Int8, 1usize)
                } else {
                    (PixelType::Uint8, 1usize)
                }
            }
            (DatatypeClass::FixedPoint, 2) => {
                if signed {
                    (PixelType::Int16, 2usize)
                } else {
                    (PixelType::Uint16, 2usize)
                }
            }
            (DatatypeClass::FixedPoint, 4) => {
                if signed {
                    (PixelType::Int32, 4usize)
                } else {
                    (PixelType::Uint32, 4usize)
                }
            }
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Imaris: unsupported dtype (class {class:?}, size {size}) for {data_path}"
                )));
            }
        }
    };
    validate_ims_data_dataset(&file, &data_path, size_x, size_y, size_z, bytes_per_sample)?;

    // ── Collect channel metadata ────────────────────────────────────────────
    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    meta_map.insert("format".into(), MetadataValue::String("Imaris IMS".into()));
    copy_group_attrs(&file, "DataSetInfo/Image", "imaris.image", &mut meta_map);
    copy_group_attrs(&file, "DataSetInfo/Imaris", "imaris.info", &mut meta_map);
    copy_group_attrs(&file, "DataSetInfo/Log", "imaris.log", &mut meta_map);
    copy_group_attrs(
        &file,
        "DataSetInfo/TimeInfo",
        "imaris.time_info",
        &mut meta_map,
    );
    collect_imaris_surpass_metadata(&file, &mut meta_map);
    let instrument = collect_imaris_instrument_metadata(&file, &mut meta_map);
    insert_optional_float(
        &mut meta_map,
        "imaris.recording_spacing_x",
        recording_spacing[0],
    );
    insert_optional_float(
        &mut meta_map,
        "imaris.recording_spacing_y",
        recording_spacing[1],
    );
    insert_optional_float(
        &mut meta_map,
        "imaris.recording_spacing_z",
        recording_spacing[2],
    );
    insert_ims_dataset_metadata(&file, &data_path, &mut meta_map);
    let mut channel_names: Vec<Option<String>> = vec![None; size_c as usize];
    let mut channel_colors: Vec<Option<i32>> = vec![None; size_c as usize];
    let mut channel_colors_normalized: Vec<Option<[f64; 3]>> = vec![None; size_c as usize];
    let mut channel_emission_wavelengths: Vec<Option<f64>> = vec![None; size_c as usize];
    let mut channel_excitation_wavelengths: Vec<Option<f64>> = vec![None; size_c as usize];
    for c in 0..size_c {
        if let Some(ch_path) = ims_dataset_info_channel_path(&file, c) {
            let Ok(ch_group) = file.group(&ch_path) else {
                continue;
            };
            copy_group_attrs(
                &file,
                &ch_path,
                &format!("imaris.channel.{c}"),
                &mut meta_map,
            );
            if let Some(name) = read_str_attr(&ch_group, "Name") {
                meta_map.insert(
                    format!("channel_{c}_name"),
                    MetadataValue::String(name.clone()),
                );
                channel_names[c as usize] = Some(name);
            }
            if let Some(color) = read_str_attr(&ch_group, "Color") {
                if let Some(parsed) = parse_imaris_channel_color(&color) {
                    insert_imaris_channel_color_metadata(&mut meta_map, c, parsed);
                    channel_colors[c as usize] = Some(pack_rgba_color(parsed));
                }
                // Mirror ImarisHDFReader.parseAttributes()'s `colors` list: the
                // raw 0..1 RGB doubles from the `Color` attribute, kept verbatim
                // for the get8BitLookupTable/get16BitLookupTable ramp.
                if let Some(rgb) = parse_imaris_channel_color_doubles(&color) {
                    channel_colors_normalized[c as usize] = Some(rgb);
                }
                meta_map.insert(format!("channel_{c}_color"), MetadataValue::String(color));
            }
            if let Some(emission) =
                read_str_attr(&ch_group, "LSMEmissionWavelength").and_then(|v| parse_imaris_f64(&v))
            {
                meta_map.insert(
                    format!("imaris.channel.{c}.emission_wavelength"),
                    MetadataValue::Float(emission),
                );
                channel_emission_wavelengths[c as usize] = Some(emission);
            }
            if let Some(excitation) = read_str_attr(&ch_group, "LSMExcitationWavelength")
                .and_then(|v| parse_imaris_f64(&v))
            {
                meta_map.insert(
                    format!("imaris.channel.{c}.excitation_wavelength"),
                    MetadataValue::Float(excitation),
                );
                channel_excitation_wavelengths[c as usize] = Some(excitation);
            }
            // ImarisHDFReader.parseAttributes() additionally parses the per-
            // channel Gain, Min, Max, Pinhole and MicroscopyMode attributes
            // into typed channel lists (after the DELIMITERS value stripping it
            // applies to every DataSetInfo/Channel_ value). Translate the same
            // branches into parsed metadata keys here.
            if let Some(gain) = read_str_attr(&ch_group, "Gain")
                .map(|v| strip_imaris_channel_value_delimiters(&v))
                .and_then(|v| parse_imaris_f64(&v))
            {
                meta_map.insert(
                    format!("imaris.channel.{c}.gain"),
                    MetadataValue::Float(gain),
                );
            }
            if let Some(pinhole) = read_str_attr(&ch_group, "Pinhole")
                .map(|v| strip_imaris_channel_value_delimiters(&v))
                .and_then(|v| parse_imaris_f64(&v))
            {
                meta_map.insert(
                    format!("imaris.channel.{c}.pinhole"),
                    MetadataValue::Float(pinhole),
                );
            }
            if let Some(min) = read_str_attr(&ch_group, "Min")
                .map(|v| strip_imaris_channel_value_delimiters(&v))
                .and_then(|v| parse_imaris_f64(&v))
            {
                meta_map.insert(format!("imaris.channel.{c}.min"), MetadataValue::Float(min));
            }
            if let Some(max) = read_str_attr(&ch_group, "Max")
                .map(|v| strip_imaris_channel_value_delimiters(&v))
                .and_then(|v| parse_imaris_f64(&v))
            {
                meta_map.insert(format!("imaris.channel.{c}.max"), MetadataValue::Float(max));
            }
            if let Some(mode) = read_str_attr(&ch_group, "MicroscopyMode")
                .map(|v| strip_imaris_channel_value_delimiters(&v))
                .filter(|v| !v.is_empty())
            {
                meta_map.insert(
                    format!("imaris.channel.{c}.microscopy_mode"),
                    MetadataValue::String(mode),
                );
            }
        }
    }

    // ── Build per-resolution-level metadata ─────────────────────────────────
    // Java reads ImageSizeX/Y/Z attributes from the group
    // DataSet/ResolutionLevel_N/TimePoint_0/Channel_0 for each sub-resolution
    // (level 0 uses the DataSetInfo/Image dimensions). sizeC and sizeT are
    // shared across all levels.
    let image_count0 = checked_image_count(size_z, size_c, size_t, "base")?;

    // Java ImarisHDFReader.initFile() line 353: ms.indexed = colors.size() >=
    // getSizeC(). Java's `colors` list reaches sizeC entries only when a Color
    // attribute was parsed for every channel index; mirror that by requiring a
    // colour for every channel.
    let colored_channels = channel_colors_normalized
        .iter()
        .filter(|c| c.is_some())
        .count();
    let is_indexed = size_c > 0 && colored_channels >= size_c as usize;

    // Java surfaces the per-channel ramp LUT through get8/16BitLookupTable, which
    // only returns a table for UINT8/UINT16 indexed data and uses lastChannel
    // (default 0). Build that channel-0 ramp once so callers reading the single
    // ImageMetadata.lookup_table slot see the same table Java's lastChannel=0
    // default would yield.
    let lookup_table = if is_indexed {
        match pixel_type {
            PixelType::Uint8 => channel_colors_normalized
                .first()
                .and_then(|c| *c)
                .map(imaris_8bit_lookup_table),
            PixelType::Uint16 => channel_colors_normalized
                .first()
                .and_then(|c| *c)
                .map(imaris_16bit_lookup_table),
            _ => None,
        }
    } else {
        None
    };

    let base_meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (bytes_per_sample * 8) as u8,
        image_count: image_count0,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed,
        is_little_endian: true,
        resolution_count: n_resolutions as u32,
        // Level 0 is the full-resolution primary (Java ms0.thumbnail = false).
        thumbnail: false,
        series_metadata: meta_map,
        lookup_table,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    let mut resolutions = Vec::with_capacity(n_resolutions);
    resolutions.push(base_meta.clone());
    for level in 1..n_resolutions {
        let mut lvl = base_meta.clone();
        // Derive this level's dimensions from its own Data dataset shape rather
        // than the ImageSize* attributes (same rationale as level 0).
        let (lz, ly, lx) = ims_level_dims(&file, level)?;
        lvl.size_z = lz;
        lvl.size_y = ly;
        lvl.size_x = lx;
        let level_data_path = ims_data_path(&file, level, 0, 0).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!(
                "Imaris: missing DataSet/ResolutionLevel {level}/TimePoint 0/Channel 0/Data or DataSet/ResolutionLevel_{level}/TimePoint_0/Channel_0/Data"
            ))
        })?;
        validate_ims_data_dataset(
            &file,
            &level_data_path,
            lvl.size_x,
            lvl.size_y,
            lvl.size_z,
            bytes_per_sample,
        )?;
        lvl.image_count = checked_image_count(lvl.size_z, lvl.size_c, lvl.size_t, "resolution")?;
        lvl.resolution_count = n_resolutions as u32;
        // Java ImarisHDFReader.initFile line 301: every sub-resolution level
        // (i >= 1) is flagged as a thumbnail, whether it later collapses into
        // the pyramid or splits off as its own series.
        lvl.thumbnail = true;
        resolutions.push(lvl);
    }

    Ok(ImsParse {
        resolutions,
        bytes_per_sample,
        extents,
        recording_spacing,
        image_description,
        channel_names,
        channel_colors,
        channel_colors_normalized,
        channel_emission_wavelengths,
        channel_excitation_wavelengths,
        instrument,
    })
}

/// Read an integer attribute (string- or numeric-encoded) from an HDF5 group.

fn insert_optional_float(
    meta_map: &mut HashMap<String, MetadataValue>,
    key: &str,
    value: Option<f64>,
) {
    if let Some(v) = value.filter(|v| v.is_finite()) {
        meta_map.insert(key.to_string(), MetadataValue::Float(v));
    }
}

fn parse_imaris_channel_color(value: &str) -> Option<[u8; 4]> {
    let mut components = Vec::new();
    for part in value
        .split(|ch: char| ch.is_ascii_whitespace() || ch == ',' || ch == ';')
        .filter(|part| !part.is_empty())
    {
        components.push(part.parse::<f64>().ok()?);
    }
    if !(3..=4).contains(&components.len()) || components.iter().any(|v| !v.is_finite()) {
        return None;
    }

    let normalized = components.iter().all(|v| (0.0..=1.0).contains(v));
    let to_u8 = |v: f64| -> u8 {
        let scaled = if normalized { v * 255.0 } else { v };
        scaled.round().clamp(0.0, 255.0) as u8
    };

    Some([
        to_u8(components[0]),
        to_u8(components[1]),
        to_u8(components[2]),
        components.get(3).map(|v| to_u8(*v)).unwrap_or(255),
    ])
}

/// Mirror ImarisHDFReader.parseAttributes()'s per-channel `Color` parse for the
/// `colors` list: split the value on spaces and parse each token as a double,
/// keeping the raw 0..1 RGB components (Java fills a `double[3]`, parsing up to
/// the number of tokens present and leaving any missing component at 0.0).
fn parse_imaris_channel_color_doubles(value: &str) -> Option<[f64; 3]> {
    let mut color = [0.0f64; 3];
    let mut any = false;
    for (i, token) in value.split(' ').filter(|t| !t.is_empty()).enumerate() {
        if i >= 3 {
            break;
        }
        color[i] = token.parse::<f64>().ok()?;
        any = true;
    }
    if any {
        Some(color)
    } else {
        None
    }
}

/// Java ImarisHDFReader.get8BitLookupTable(): build a 3x256 ramp LUT for one
/// channel from its 0..1 colour, where `lut[c][p] = (p/255)*(color[c]*255)`.
/// Returned as a [`LookupTable`] with the 8-bit ramp widened into u16 slots
/// (the crate's LUT element type), preserving the 0..255 ramp values.
fn imaris_8bit_lookup_table(color: [f64; 3]) -> LookupTable {
    let ramp = |component: f64| -> Vec<u16> {
        let max = component * 255.0;
        (0..256)
            .map(|p| (((p as f64) / 255.0) * max) as u8 as u16)
            .collect()
    };
    LookupTable {
        red: ramp(color[0]),
        green: ramp(color[1]),
        blue: ramp(color[2]),
    }
}

/// Java ImarisHDFReader.get16BitLookupTable(): build a 3x65536 ramp LUT for one
/// channel from its 0..1 colour, where `lut[c][p] = (p/65535)*(color[c]*65535)`.
fn imaris_16bit_lookup_table(color: [f64; 3]) -> LookupTable {
    let ramp = |component: f64| -> Vec<u16> {
        let max = component * 65535.0;
        (0..65536)
            .map(|p| (((p as f64) / 65535.0) * max) as u16)
            .collect()
    };
    LookupTable {
        red: ramp(color[0]),
        green: ramp(color[1]),
        blue: ramp(color[2]),
    }
}

fn parse_imaris_f64(value: &str) -> Option<f64> {
    value
        .split(|ch: char| ch.is_ascii_whitespace() || ch == ',' || ch == ';' || ch == '=')
        .filter(|part| !part.is_empty())
        .filter_map(|part| part.parse::<f64>().ok())
        .next_back()
        .filter(|v| v.is_finite())
}

/// Imaris DataSetInfo/Channel attribute-value normalisation, mirroring the
/// `DELIMITERS` loop in ImarisHDFReader.parseAttributes(): for each delimiter
/// in {" ", "-", "."} in turn, if it occurs in the value, keep only the
/// substring following its first occurrence.
fn strip_imaris_channel_value_delimiters(value: &str) -> String {
    const DELIMITERS: [char; 3] = [' ', '-', '.'];
    let mut value = value.to_string();
    for delimiter in DELIMITERS {
        if let Some(index) = value.find(delimiter) {
            value = value[index + delimiter.len_utf8()..].to_string();
        }
    }
    value
}

fn pack_rgba_color([r, g, b, a]: [u8; 4]) -> i32 {
    u32::from_be_bytes([r, g, b, a]) as i32
}

fn insert_imaris_channel_color_metadata(
    meta_map: &mut HashMap<String, MetadataValue>,
    channel: u32,
    [r, g, b, a]: [u8; 4],
) {
    let prefix = format!("imaris.channel.{channel}.color");
    meta_map.insert(format!("{prefix}.red"), MetadataValue::Int(r as i64));
    meta_map.insert(format!("{prefix}.green"), MetadataValue::Int(g as i64));
    meta_map.insert(format!("{prefix}.blue"), MetadataValue::Int(b as i64));
    meta_map.insert(format!("{prefix}.alpha"), MetadataValue::Int(a as i64));
    meta_map.insert(
        format!("{prefix}.rgba"),
        MetadataValue::Int(pack_rgba_color([r, g, b, a]) as i64),
    );
}

fn copy_group_attrs(
    file: &hdf5_pure_rust::File,
    path: &str,
    prefix: &str,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    let Ok(group) = file.group(path) else {
        return;
    };
    let Ok(names) = group.attr_names() else {
        return;
    };
    for name in names {
        let Ok(attr) = group.attr(&name) else {
            continue;
        };
        if let Some(value) = attr_to_metadata_value(&attr) {
            meta_map.insert(format!("{prefix}.{name}"), value);
        }
    }
}

fn insert_ims_dataset_metadata(
    file: &hdf5_pure_rust::File,
    path: &str,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    let Ok(dataset) = file.dataset(path) else {
        return;
    };
    copy_dataset_attrs(&dataset, "imaris.dataset.0", meta_map);
    if let Ok(shape) = dataset.shape() {
        meta_map.insert(
            "imaris.dataset.0.shape".into(),
            MetadataValue::String(join_u64s(&shape)),
        );
    }
    if let Ok(dtype) = dataset.dtype() {
        meta_map.insert(
            "imaris.dataset.0.dtype_class".into(),
            MetadataValue::String(format!("{:?}", dtype.class())),
        );
        meta_map.insert(
            "imaris.dataset.0.dtype_size".into(),
            MetadataValue::Int(dtype.size() as i64),
        );
    }
    if let Ok(info) = dataset.info() {
        meta_map.insert(
            "imaris.dataset.0.layout_class".into(),
            MetadataValue::String(format!("{:?}", info.layout.layout_class)),
        );
        if let Some(dims) = info.layout.chunk_dims {
            meta_map.insert(
                "imaris.dataset.0.chunk_dims".into(),
                MetadataValue::String(join_u64s(&dims)),
            );
        }
        if let Some(mask) = info.layout.single_chunk_filter_mask {
            meta_map.insert(
                "imaris.dataset.0.single_chunk_filter_mask".into(),
                MetadataValue::Int(mask as i64),
            );
        }
        if let Some(size) = info.layout.single_chunk_filtered_size {
            meta_map.insert(
                "imaris.dataset.0.single_chunk_filtered_size".into(),
                MetadataValue::Int(size as i64),
            );
        }
    }
}

fn copy_dataset_attrs(
    dataset: &hdf5_pure_rust::Dataset,
    prefix: &str,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    let Ok(names) = dataset.attr_names() else {
        return;
    };
    for name in names {
        let Ok(attr) = dataset.attr(&name) else {
            continue;
        };
        if let Some(value) = attr_to_metadata_value(&attr) {
            meta_map.insert(format!("{prefix}.{name}"), value);
        }
    }
}

fn attr_to_metadata_value(attr: &hdf5_pure_rust::Attribute) -> Option<MetadataValue> {
    if let Ok(v) = attr.read_strings() {
        if !v.is_empty() {
            let joined = v.concat();
            let trimmed = joined.trim_matches('\0').trim();
            if !trimmed.is_empty() {
                return Some(MetadataValue::String(trimmed.to_string()));
            }
        }
    }
    let s = attr.read_string();
    if !s.is_empty() {
        let trimmed = s.trim_matches('\0').trim();
        if !trimmed.is_empty() {
            return Some(MetadataValue::String(trimmed.to_string()));
        }
    }
    if let Some(i) = attr.read_scalar_i64() {
        return Some(MetadataValue::Int(i));
    }
    if let Some(f) = attr.read_scalar_f64().filter(|v| v.is_finite()) {
        return Some(MetadataValue::Float(f));
    }
    if let Ok(b) = attr.read_scalar_bool() {
        return Some(MetadataValue::Bool(b));
    }
    None
}

fn collect_imaris_surpass_metadata(
    file: &hdf5_pure_rust::File,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    let roots = [
        "DataSetInfo/Scene",
        "DataSetInfo/Scene8",
        "DataSetInfo/Surpass",
        "Scene",
        "Scene8",
        "Surpass",
    ];
    let mut found = Vec::new();
    let mut visited = 0usize;
    for root in roots {
        if file.group(root).is_err() {
            continue;
        }
        found.push(root.to_string());
        let key_root = format!("imaris.surpass.{}", imaris_metadata_path_key(root));
        collect_imaris_hdf5_metadata_tree(file, root, &key_root, meta_map, &mut visited);
        if visited >= IMARIS_SURPASS_METADATA_NODE_LIMIT {
            break;
        }
    }
    if !found.is_empty() {
        meta_map.insert(
            "imaris.surpass.roots".into(),
            MetadataValue::String(found.join(",")),
        );
        meta_map.insert(
            "imaris.surpass.node_count".into(),
            MetadataValue::Int(visited as i64),
        );
        if visited >= IMARIS_SURPASS_METADATA_NODE_LIMIT {
            meta_map.insert("imaris.surpass.truncated".into(), MetadataValue::Bool(true));
        }
    }
}

fn collect_imaris_hdf5_metadata_tree(
    file: &hdf5_pure_rust::File,
    path: &str,
    key_prefix: &str,
    meta_map: &mut HashMap<String, MetadataValue>,
    visited: &mut usize,
) {
    if *visited >= IMARIS_SURPASS_METADATA_NODE_LIMIT {
        return;
    }
    let Ok(group) = file.group(path) else {
        return;
    };
    *visited += 1;

    copy_group_attrs_from_group(&group, key_prefix, meta_map);
    let Ok(members) = hdf5_group_members(&group) else {
        return;
    };
    collect_imaris_surpass_group_provenance(file, path, key_prefix, &members, meta_map);
    for member in members {
        if *visited >= IMARIS_SURPASS_METADATA_NODE_LIMIT {
            return;
        }
        let child_path = format!("{path}/{member}");
        let child_key = format!("{key_prefix}.{}", imaris_metadata_path_key(&member));
        if let Ok(dataset) = file.dataset(&child_path) {
            *visited += 1;
            collect_imaris_dataset_node_metadata(&dataset, &child_key, meta_map);
        } else if file.group(&child_path).is_ok() {
            collect_imaris_hdf5_metadata_tree(file, &child_path, &child_key, meta_map, visited);
        }
    }
    collect_imaris_surpass_statistics_table(file, path, key_prefix, meta_map);
}

fn collect_imaris_surpass_group_provenance(
    file: &hdf5_pure_rust::File,
    path: &str,
    key_prefix: &str,
    members: &[String],
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    let mut group_count = 0i64;
    let mut dataset_count = 0i64;
    for member in members {
        let child_path = format!("{path}/{member}");
        if file.dataset(&child_path).is_ok() {
            dataset_count += 1;
        } else if file.group(&child_path).is_ok() {
            group_count += 1;
        }
    }

    meta_map.insert(
        format!("{key_prefix}.hdf5_path"),
        MetadataValue::String(path.to_string()),
    );
    meta_map.insert(
        format!("{key_prefix}.member_count"),
        MetadataValue::Int(members.len().min(i64::MAX as usize) as i64),
    );
    meta_map.insert(
        format!("{key_prefix}.child_group_count"),
        MetadataValue::Int(group_count),
    );
    meta_map.insert(
        format!("{key_prefix}.dataset_count"),
        MetadataValue::Int(dataset_count),
    );
    if let Some((parent, _)) = key_prefix.rsplit_once('.') {
        meta_map.insert(
            format!("{key_prefix}.parent_key"),
            MetadataValue::String(parent.to_string()),
        );
    }
    if let Some(name) = path.rsplit('/').next() {
        if let Some((kind, index)) = imaris_surpass_indexed_object_name(name) {
            meta_map.insert(
                format!("{key_prefix}.object_kind"),
                MetadataValue::String(kind),
            );
            meta_map.insert(
                format!("{key_prefix}.object_index"),
                MetadataValue::Int(index as i64),
            );
        }
    }
}

fn imaris_surpass_indexed_object_name(name: &str) -> Option<(String, u32)> {
    let (kind, index) = name.rsplit_once([' ', '_'])?;
    let index = index.parse::<u32>().ok()?;
    let compact = kind
        .chars()
        .filter(|ch| ch.is_ascii_alphabetic())
        .collect::<String>();
    match compact.as_str() {
        "Surfaces" | "Spots" | "Cells" | "Filaments" | "MeasurementPoints" => {
            Some((compact, index))
        }
        _ => None,
    }
}

fn copy_group_attrs_from_group(
    group: &hdf5_pure_rust::Group,
    prefix: &str,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    let Ok(names) = group.attr_names() else {
        return;
    };
    for name in names {
        let Ok(attr) = group.attr(&name) else {
            continue;
        };
        if let Some(value) = attr_to_metadata_value(&attr) {
            meta_map.insert(
                format!("{prefix}.{}", imaris_metadata_path_key(&name)),
                value,
            );
        }
    }
}

fn collect_imaris_dataset_node_metadata(
    dataset: &hdf5_pure_rust::Dataset,
    prefix: &str,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    copy_dataset_attrs(dataset, prefix, meta_map);
    let shape = dataset.shape().unwrap_or_default();
    meta_map.insert(
        format!("{prefix}.shape"),
        MetadataValue::String(join_u64s(&shape)),
    );
    if let Ok(dtype) = dataset.dtype() {
        meta_map.insert(
            format!("{prefix}.dtype_class"),
            MetadataValue::String(format!("{:?}", dtype.class())),
        );
        meta_map.insert(
            format!("{prefix}.dtype_size"),
            MetadataValue::Int(dtype.size() as i64),
        );
    }

    let n_values = shape.iter().copied().product::<u64>().max(1);
    collect_imaris_surpass_geometry_diagnostics(prefix, &shape, n_values, meta_map);
    if n_values > IMARIS_SURPASS_DATASET_VALUE_LIMIT {
        meta_map.insert(
            format!("{prefix}.value_status"),
            MetadataValue::String("not_read_large_dataset".into()),
        );
        return;
    }
    if let Some(value) = imaris_dataset_value(dataset) {
        meta_map.insert(format!("{prefix}.value"), value);
    }
}

fn collect_imaris_surpass_geometry_diagnostics(
    prefix: &str,
    shape: &[u64],
    value_count: u64,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    let Some(role) = imaris_surpass_geometry_role(prefix) else {
        return;
    };
    let component_count = imaris_surpass_geometry_component_count(shape, role.default_components());
    let element_count = component_count
        .filter(|components| *components > 0 && value_count % *components == 0)
        .map(|components| value_count / components)
        .unwrap_or(value_count);

    meta_map.insert(
        format!("{prefix}.geometry_role"),
        MetadataValue::String(role.metadata_value().into()),
    );
    meta_map.insert(
        format!("{prefix}.geometry_value_count"),
        MetadataValue::Int(value_count.min(i64::MAX as u64) as i64),
    );
    meta_map.insert(
        format!("{prefix}.geometry_element_count"),
        MetadataValue::Int(element_count.min(i64::MAX as u64) as i64),
    );
    if let Some(component_count) = component_count {
        meta_map.insert(
            format!("{prefix}.geometry_component_count"),
            MetadataValue::Int(component_count.min(i64::MAX as u64) as i64),
        );
    }
    if value_count > IMARIS_SURPASS_DATASET_VALUE_LIMIT {
        meta_map.insert(
            format!("{prefix}.geometry_status"),
            MetadataValue::String("not_read_large_geometry".into()),
        );
    }
}

#[derive(Clone, Copy)]
enum ImarisSurpassGeometryRole {
    Vertices,
    Normals,
    Triangles,
    Edges,
}

impl ImarisSurpassGeometryRole {
    fn metadata_value(self) -> &'static str {
        match self {
            Self::Vertices => "vertices",
            Self::Normals => "normals",
            Self::Triangles => "triangles",
            Self::Edges => "edges",
        }
    }

    fn default_components(self) -> Option<u64> {
        match self {
            Self::Vertices | Self::Normals | Self::Triangles => Some(3),
            Self::Edges => Some(2),
        }
    }
}

fn imaris_surpass_geometry_role(prefix: &str) -> Option<ImarisSurpassGeometryRole> {
    let name = prefix.rsplit('.').next()?.to_ascii_lowercase();
    let compact = name
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect::<String>();
    match compact.as_str() {
        "vertices" | "vertexpositions" | "positions" | "points" => {
            Some(ImarisSurpassGeometryRole::Vertices)
        }
        "normals" | "vertexnormals" => Some(ImarisSurpassGeometryRole::Normals),
        "triangles" | "triangleindices" | "faces" | "faceindices" => {
            Some(ImarisSurpassGeometryRole::Triangles)
        }
        "edges" | "edgeindices" | "lines" | "lineindices" => Some(ImarisSurpassGeometryRole::Edges),
        _ => None,
    }
}

fn imaris_surpass_geometry_component_count(shape: &[u64], default: Option<u64>) -> Option<u64> {
    if let Some(last) = shape
        .last()
        .copied()
        .filter(|value| (2..=4).contains(value))
    {
        return Some(last);
    }
    default
}

fn imaris_dataset_value(dataset: &hdf5_pure_rust::Dataset) -> Option<MetadataValue> {
    let dtype = dataset.dtype().ok()?;
    match dtype.class() {
        DatatypeClass::String | DatatypeClass::VarLen => {
            let strings = dataset.read_strings().ok()?;
            let joined = strings.join(",");
            let trimmed = joined.trim_matches('\0').trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(MetadataValue::String(trimmed.to_string()))
            }
        }
        DatatypeClass::FloatingPoint => {
            let values = match dtype.size() {
                4 => dataset
                    .read::<f32>()
                    .ok()?
                    .into_iter()
                    .map(f64::from)
                    .collect::<Vec<_>>(),
                8 => dataset.read::<f64>().ok()?,
                _ => return None,
            };
            if values.iter().all(|v| v.is_finite()) {
                imaris_float_values_to_metadata(values)
            } else {
                None
            }
        }
        DatatypeClass::FixedPoint => {
            let signed = dtype.is_signed().unwrap_or(true);
            if signed {
                let values = match dtype.size() {
                    1 => dataset
                        .read::<i8>()
                        .ok()?
                        .into_iter()
                        .map(i64::from)
                        .collect::<Vec<_>>(),
                    2 => dataset
                        .read::<i16>()
                        .ok()?
                        .into_iter()
                        .map(i64::from)
                        .collect::<Vec<_>>(),
                    4 => dataset
                        .read::<i32>()
                        .ok()?
                        .into_iter()
                        .map(i64::from)
                        .collect::<Vec<_>>(),
                    8 => dataset.read::<i64>().ok()?,
                    _ => return None,
                };
                imaris_i64_values_to_metadata(values)
            } else {
                let values = match dtype.size() {
                    1 => dataset
                        .read::<u8>()
                        .ok()?
                        .into_iter()
                        .map(u64::from)
                        .collect::<Vec<_>>(),
                    2 => dataset
                        .read::<u16>()
                        .ok()?
                        .into_iter()
                        .map(u64::from)
                        .collect::<Vec<_>>(),
                    4 => dataset
                        .read::<u32>()
                        .ok()?
                        .into_iter()
                        .map(u64::from)
                        .collect::<Vec<_>>(),
                    8 => dataset.read::<u64>().ok()?,
                    _ => return None,
                };
                imaris_u64_values_to_metadata(values)
            }
        }
        _ => None,
    }
}

fn imaris_float_values_to_metadata(values: Vec<f64>) -> Option<MetadataValue> {
    if values.len() == 1 {
        values.first().copied().map(MetadataValue::Float)
    } else {
        Some(MetadataValue::String(
            values
                .iter()
                .map(f64::to_string)
                .collect::<Vec<_>>()
                .join(" "),
        ))
    }
}

fn imaris_i64_values_to_metadata(values: Vec<i64>) -> Option<MetadataValue> {
    if values.len() == 1 {
        values.first().copied().map(MetadataValue::Int)
    } else {
        Some(MetadataValue::String(
            values
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(" "),
        ))
    }
}

fn imaris_u64_values_to_metadata(values: Vec<u64>) -> Option<MetadataValue> {
    if values.len() == 1 && values[0] <= i64::MAX as u64 {
        Some(MetadataValue::Int(values[0] as i64))
    } else {
        Some(MetadataValue::String(
            values
                .iter()
                .map(u64::to_string)
                .collect::<Vec<_>>()
                .join(" "),
        ))
    }
}

fn collect_imaris_surpass_statistics_table(
    file: &hdf5_pure_rust::File,
    path: &str,
    key_prefix: &str,
    meta_map: &mut HashMap<String, MetadataValue>,
) {
    if !key_prefix.ends_with(".Statistics") && !key_prefix.contains(".Statistics.") {
        return;
    }
    let table_prefix = format!("{key_prefix}.table");
    let Some(names_dataset) = first_existing_dataset(file, path, &["Names", "StatisticNames"])
    else {
        return;
    };
    let Some(values_dataset) = first_existing_dataset(file, path, &["Values", "StatisticValues"])
    else {
        return;
    };
    let Ok(name_shape) = names_dataset.shape() else {
        meta_map.insert(
            format!("{table_prefix}.names_status"),
            MetadataValue::String("unreadable_statistics_names".into()),
        );
        return;
    };
    let name_count = hdf5_shape_value_count(&name_shape);
    meta_map.insert(
        format!("{table_prefix}.names_shape"),
        MetadataValue::String(join_u64s(&name_shape)),
    );
    meta_map.insert(
        format!("{table_prefix}.name_count"),
        MetadataValue::Int(name_count.min(i64::MAX as u64) as i64),
    );
    if name_count > IMARIS_SURPASS_STATISTICS_TABLE_VALUE_LIMIT {
        meta_map.insert(
            format!("{table_prefix}.names_status"),
            MetadataValue::String("not_read_large_statistics_names".into()),
        );
        return;
    }

    let Some(names) = imaris_dataset_string_values(&names_dataset) else {
        meta_map.insert(
            format!("{table_prefix}.names_status"),
            MetadataValue::String("unsupported_statistics_names".into()),
        );
        return;
    };
    if names.is_empty() {
        meta_map.insert(
            format!("{table_prefix}.names_status"),
            MetadataValue::String("empty_statistics_names".into()),
        );
        return;
    }
    let Ok(value_shape) = values_dataset.shape() else {
        meta_map.insert(
            format!("{table_prefix}.value_status"),
            MetadataValue::String("unreadable_statistics_values".into()),
        );
        return;
    };
    let value_count = hdf5_shape_value_count(&value_shape);
    meta_map.insert(
        format!("{table_prefix}.stat_count"),
        MetadataValue::Int(names.len() as i64),
    );
    meta_map.insert(
        format!("{table_prefix}.value_shape"),
        MetadataValue::String(join_u64s(&value_shape)),
    );
    meta_map.insert(
        format!("{table_prefix}.value_count"),
        MetadataValue::Int(value_count.min(i64::MAX as u64) as i64),
    );
    if value_count > IMARIS_SURPASS_STATISTICS_TABLE_VALUE_LIMIT {
        meta_map.insert(
            format!("{table_prefix}.value_status"),
            MetadataValue::String("not_read_large_statistics_table".into()),
        );
        return;
    }
    let Some(values) = imaris_dataset_numeric_f64_values(&values_dataset) else {
        meta_map.insert(
            format!("{table_prefix}.value_status"),
            MetadataValue::String("unsupported_statistics_values".into()),
        );
        return;
    };
    let Some((rows, columns, effective_shape)) =
        imaris_statistics_matrix_shape(&value_shape, values.len())
    else {
        meta_map.insert(
            format!("{table_prefix}.layout_status"),
            MetadataValue::String("unsupported_statistics_layout".into()),
        );
        meta_map.insert(
            format!("{table_prefix}.layout_reason"),
            MetadataValue::String(
                imaris_statistics_unsupported_layout_reason(
                    &value_shape,
                    names.len(),
                    values.len(),
                )
                .into(),
            ),
        );
        meta_map.insert(
            format!("{table_prefix}.supported_layouts"),
            MetadataValue::String("stat_rows,stat_columns".into()),
        );
        return;
    };
    if effective_shape.as_slice() != value_shape.as_slice() {
        meta_map.insert(
            format!("{table_prefix}.effective_value_shape"),
            MetadataValue::String(join_u64s(&effective_shape)),
        );
    }
    meta_map.insert(
        format!("{table_prefix}.row_count"),
        MetadataValue::Int(rows.min(i64::MAX as usize) as i64),
    );
    meta_map.insert(
        format!("{table_prefix}.column_count"),
        MetadataValue::Int(columns.min(i64::MAX as usize) as i64),
    );
    let layout = if rows == names.len() {
        ImarisStatisticsTableLayout::StatRows
    } else if columns == names.len() {
        ImarisStatisticsTableLayout::StatColumns
    } else {
        meta_map.insert(
            format!("{table_prefix}.layout_status"),
            MetadataValue::String("unsupported_statistics_layout".into()),
        );
        meta_map.insert(
            format!("{table_prefix}.layout_reason"),
            MetadataValue::String("statistic_name_count_mismatch".into()),
        );
        meta_map.insert(
            format!("{table_prefix}.supported_layouts"),
            MetadataValue::String("stat_rows,stat_columns".into()),
        );
        return;
    };

    meta_map.insert(
        format!("{table_prefix}.layout"),
        MetadataValue::String(layout.metadata_value().into()),
    );
    for (row, name) in names.iter().enumerate() {
        let key_name = imaris_metadata_path_key(name);
        if key_name.is_empty() {
            continue;
        }
        let stat_values = match layout {
            ImarisStatisticsTableLayout::StatRows => {
                values[row * columns..(row + 1) * columns].to_vec()
            }
            ImarisStatisticsTableLayout::StatColumns => (0..rows)
                .map(|value_row| values[value_row * columns + row])
                .collect(),
        };
        let value = imaris_float_values_to_metadata(stat_values);
        if let Some(value) = value {
            meta_map.insert(format!("{table_prefix}.{key_name}"), value);
        }
    }
}

fn imaris_statistics_matrix_shape(
    shape: &[u64],
    value_len: usize,
) -> Option<(usize, usize, Vec<u64>)> {
    if value_len == 0 {
        return None;
    }
    let mut axes = if shape.len() <= 2 {
        shape.to_vec()
    } else {
        shape
            .iter()
            .copied()
            .filter(|axis| *axis > 1)
            .collect::<Vec<_>>()
    };
    if axes.is_empty() {
        axes.push(value_len as u64);
    }

    let (rows, columns) = match axes.as_slice() {
        [rows] => {
            let rows = usize::try_from(*rows).ok()?;
            if rows == 0 || value_len % rows != 0 {
                return None;
            }
            (rows, value_len / rows)
        }
        [rows, columns] => {
            let rows = usize::try_from(*rows).ok()?;
            let columns = usize::try_from(*columns).ok()?;
            if rows == 0 || columns == 0 || rows.checked_mul(columns)? != value_len {
                return None;
            }
            (rows, columns)
        }
        _ => return None,
    };
    Some((rows, columns, axes))
}

fn imaris_statistics_effective_axes(shape: &[u64], value_len: usize) -> Vec<u64> {
    let mut axes = if shape.len() <= 2 {
        shape.to_vec()
    } else {
        shape
            .iter()
            .copied()
            .filter(|axis| *axis > 1)
            .collect::<Vec<_>>()
    };
    if axes.is_empty() {
        axes.push(value_len as u64);
    }
    axes
}

fn imaris_statistics_unsupported_layout_reason(
    shape: &[u64],
    name_count: usize,
    value_len: usize,
) -> &'static str {
    if value_len == 0 {
        return "empty_statistics_values";
    }
    let axes = imaris_statistics_effective_axes(shape, value_len);
    match axes.as_slice() {
        [rows] => {
            let Ok(rows) = usize::try_from(*rows) else {
                return "statistics_axis_overflow";
            };
            if rows == 0 || value_len % rows != 0 {
                "inconsistent_statistics_value_count"
            } else if rows != name_count && value_len / rows != name_count {
                "statistic_name_count_mismatch"
            } else {
                "unsupported_statistics_layout"
            }
        }
        [rows, columns] => {
            let (Ok(rows), Ok(columns)) = (usize::try_from(*rows), usize::try_from(*columns))
            else {
                return "statistics_axis_overflow";
            };
            if rows == 0
                || columns == 0
                || rows
                    .checked_mul(columns)
                    .is_none_or(|count| count != value_len)
            {
                "inconsistent_statistics_value_count"
            } else if rows != name_count && columns != name_count {
                "statistic_name_count_mismatch"
            } else {
                "unsupported_statistics_layout"
            }
        }
        _ => "unsupported_statistics_rank",
    }
}

fn hdf5_shape_value_count(shape: &[u64]) -> u64 {
    shape
        .iter()
        .copied()
        .fold(1u64, |count, axis| count.saturating_mul(axis))
        .max(1)
}

#[derive(Clone, Copy)]
enum ImarisStatisticsTableLayout {
    StatRows,
    StatColumns,
}

impl ImarisStatisticsTableLayout {
    fn metadata_value(self) -> &'static str {
        match self {
            Self::StatRows => "stat_rows",
            Self::StatColumns => "stat_columns",
        }
    }
}

fn first_existing_dataset(
    file: &hdf5_pure_rust::File,
    group_path: &str,
    names: &[&str],
) -> Option<hdf5_pure_rust::Dataset> {
    names
        .iter()
        .find_map(|name| file.dataset(&format!("{group_path}/{name}")).ok())
}

fn imaris_dataset_string_values(dataset: &hdf5_pure_rust::Dataset) -> Option<Vec<String>> {
    let shape = dataset.shape().ok()?;
    let n_values = shape.iter().copied().product::<u64>().max(1);
    if n_values > IMARIS_SURPASS_STATISTICS_TABLE_VALUE_LIMIT {
        return None;
    }
    let dtype = dataset.dtype().ok()?;
    if !matches!(dtype.class(), DatatypeClass::String | DatatypeClass::VarLen) {
        return None;
    }
    let values = dataset
        .read_strings()
        .ok()?
        .into_iter()
        .map(|value| value.trim_matches('\0').trim().to_string())
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn imaris_dataset_numeric_f64_values(dataset: &hdf5_pure_rust::Dataset) -> Option<Vec<f64>> {
    let dtype = dataset.dtype().ok()?;
    let values = match dtype.class() {
        DatatypeClass::FloatingPoint => match dtype.size() {
            4 => dataset
                .read::<f32>()
                .ok()?
                .into_iter()
                .map(f64::from)
                .collect::<Vec<_>>(),
            8 => dataset.read::<f64>().ok()?,
            _ => return None,
        },
        DatatypeClass::FixedPoint => {
            if dtype.is_signed().unwrap_or(true) {
                match dtype.size() {
                    1 => dataset
                        .read::<i8>()
                        .ok()?
                        .into_iter()
                        .map(|value| value as f64)
                        .collect::<Vec<_>>(),
                    2 => dataset
                        .read::<i16>()
                        .ok()?
                        .into_iter()
                        .map(|value| value as f64)
                        .collect::<Vec<_>>(),
                    4 => dataset
                        .read::<i32>()
                        .ok()?
                        .into_iter()
                        .map(|value| value as f64)
                        .collect::<Vec<_>>(),
                    8 => dataset
                        .read::<i64>()
                        .ok()?
                        .into_iter()
                        .map(|value| value as f64)
                        .collect::<Vec<_>>(),
                    _ => return None,
                }
            } else {
                match dtype.size() {
                    1 => dataset
                        .read::<u8>()
                        .ok()?
                        .into_iter()
                        .map(|value| value as f64)
                        .collect::<Vec<_>>(),
                    2 => dataset
                        .read::<u16>()
                        .ok()?
                        .into_iter()
                        .map(|value| value as f64)
                        .collect::<Vec<_>>(),
                    4 => dataset
                        .read::<u32>()
                        .ok()?
                        .into_iter()
                        .map(|value| value as f64)
                        .collect::<Vec<_>>(),
                    8 => dataset
                        .read::<u64>()
                        .ok()?
                        .into_iter()
                        .map(|value| value as f64)
                        .collect::<Vec<_>>(),
                    _ => return None,
                }
            }
        }
        _ => return None,
    };
    if values.iter().all(|value| value.is_finite()) {
        Some(values)
    } else {
        None
    }
}

fn imaris_metadata_path_key(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>()
        .trim_matches('_')
        .to_string()
}

fn imaris_ome_rois_from_metadata(meta: &ImageMetadata) -> Vec<OmeROI> {
    let mut bases = meta
        .series_metadata
        .keys()
        .filter_map(|key| key.strip_suffix(".Statistics.Center.value"))
        .filter(|base| imaris_surpass_object_base(base))
        .map(str::to_string)
        .collect::<Vec<_>>();
    bases.sort();
    bases.dedup();

    let mut rois = Vec::new();
    for base in bases {
        let Some(center) = imaris_metadata_float_list(
            meta.series_metadata
                .get(&format!("{base}.Statistics.Center.value")),
        )
        .filter(|values| values.len() >= 2) else {
            continue;
        };
        let x = center[0];
        let y = center[1];
        let z = center
            .get(2)
            .and_then(|value| imaris_nonnegative_plane_index(*value));
        let t = imaris_surpass_roi_index(meta, &base, ImarisSurpassRoiAxis::T);
        let c = imaris_surpass_roi_index(meta, &base, ImarisSurpassRoiAxis::C);
        let radius = imaris_metadata_float_list(
            meta.series_metadata
                .get(&format!("{base}.Statistics.RadiusXYZ.value")),
        );
        let shape = match radius.filter(|values| values.len() >= 2) {
            Some(radius) if radius[0] >= 0.0 && radius[1] >= 0.0 => OmeShape::Ellipse {
                x,
                y,
                radius_x: radius[0],
                radius_y: radius[1],
                the_z: z,
                the_t: t,
                the_c: c,
            },
            _ => OmeShape::Point {
                x,
                y,
                the_z: z,
                the_t: t,
                the_c: c,
            },
        };
        let name = imaris_metadata_string(meta.series_metadata.get(&format!("{base}.Name")))
            .or_else(|| imaris_metadata_string(meta.series_metadata.get(&format!("{base}.Type"))));
        rois.push(OmeROI {
            id: Some(create_lsid("ROI", &[rois.len()])),
            name,
            shapes: vec![shape],
        });
    }
    rois
}

#[derive(Clone, Copy)]
enum ImarisSurpassRoiAxis {
    T,
    C,
}

fn imaris_surpass_roi_index(
    meta: &ImageMetadata,
    base: &str,
    axis: ImarisSurpassRoiAxis,
) -> Option<u32> {
    let names: &[&str] = match axis {
        ImarisSurpassRoiAxis::T => &["IndexT", "TimeIndex", "Time_Index", "TheT", "T"],
        ImarisSurpassRoiAxis::C => &["IndexC", "ChannelIndex", "Channel_Index", "TheC", "C"],
    };

    names.iter().find_map(|name| {
        let dataset_key = format!("{base}.Statistics.{name}.value");
        let table_key = format!("{base}.Statistics.table.{name}");
        imaris_metadata_float_list(meta.series_metadata.get(&dataset_key))
            .or_else(|| imaris_metadata_float_list(meta.series_metadata.get(&table_key)))
            .and_then(|values| values.first().copied())
            .and_then(imaris_nonnegative_plane_index)
    })
}

fn imaris_surpass_object_base(base: &str) -> bool {
    base.contains(".Surfaces_")
        || base.contains(".Spots_")
        || base.contains(".Cells_")
        || base.contains(".Filaments_")
        || base.contains(".MeasurementPoints_")
}

fn imaris_metadata_float_list(value: Option<&MetadataValue>) -> Option<Vec<f64>> {
    match value? {
        MetadataValue::Float(value) if value.is_finite() => Some(vec![*value]),
        MetadataValue::Int(value) => Some(vec![*value as f64]),
        MetadataValue::String(value) => {
            let values = value
                .split(|ch: char| ch.is_ascii_whitespace() || ch == ',' || ch == ';')
                .filter(|part| !part.is_empty())
                .map(str::parse::<f64>)
                .collect::<std::result::Result<Vec<_>, _>>()
                .ok()?;
            if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
                None
            } else {
                Some(values)
            }
        }
        _ => None,
    }
}

fn imaris_metadata_string(value: Option<&MetadataValue>) -> Option<String> {
    match value? {
        MetadataValue::String(value) if !value.is_empty() => Some(value.clone()),
        MetadataValue::Int(value) => Some(value.to_string()),
        MetadataValue::Float(value) if value.is_finite() => Some(value.to_string()),
        MetadataValue::Bool(value) => Some(value.to_string()),
        _ => None,
    }
}

fn imaris_nonnegative_plane_index(value: f64) -> Option<u32> {
    if value.is_finite() && value >= 0.0 && value.fract().abs() <= f64::EPSILON {
        Some(value as u32)
    } else {
        None
    }
}

fn collect_imaris_instrument_metadata(
    file: &hdf5_pure_rust::File,
    meta_map: &mut HashMap<String, MetadataValue>,
) -> ImarisInstrumentMetadata {
    let microscope = first_existing_group(file, &["DataSetInfo/Microscope"]);
    let objective = first_existing_group(
        file,
        &[
            "DataSetInfo/Objective",
            "DataSetInfo/Objective 0",
            "DataSetInfo/Objective_0",
            "DataSetInfo/Lens",
        ],
    );
    let detector = first_existing_group(
        file,
        &[
            "DataSetInfo/Detector",
            "DataSetInfo/Detector 0",
            "DataSetInfo/Detector_0",
        ],
    );
    let light_source = first_existing_group(
        file,
        &[
            "DataSetInfo/LightSource",
            "DataSetInfo/LightSource 0",
            "DataSetInfo/LightSource_0",
            "DataSetInfo/Laser",
            "DataSetInfo/Laser 0",
            "DataSetInfo/Laser_0",
        ],
    );

    if let Some((path, _)) = microscope.as_ref() {
        copy_group_attrs(file, path, "imaris.microscope", meta_map);
    }
    if let Some((path, _)) = objective.as_ref() {
        copy_group_attrs(file, path, "imaris.objective", meta_map);
    }
    if let Some((path, _)) = detector.as_ref() {
        copy_group_attrs(file, path, "imaris.detector", meta_map);
    }
    if let Some((path, _)) = light_source.as_ref() {
        copy_group_attrs(file, path, "imaris.light_source", meta_map);
    }

    ImarisInstrumentMetadata {
        microscope_model: microscope
            .as_ref()
            .and_then(|(_, group)| first_str_attr(group, &["Model", "Name", "MicroscopeModel"])),
        microscope_manufacturer: microscope.as_ref().and_then(|(_, group)| {
            first_str_attr(group, &["Manufacturer", "MicroscopeManufacturer"])
        }),
        objective_model: objective
            .as_ref()
            .and_then(|(_, group)| first_str_attr(group, &["Model", "Name", "ObjectiveName"])),
        objective_manufacturer: objective.as_ref().and_then(|(_, group)| {
            first_str_attr(group, &["Manufacturer", "ObjectiveManufacturer"])
        }),
        objective_nominal_magnification: objective.as_ref().and_then(|(_, group)| {
            first_float_attr(
                group,
                &[
                    "NominalMagnification",
                    "Magnification",
                    "ObjectiveMagnification",
                ],
            )
        }),
        objective_calibrated_magnification: objective
            .as_ref()
            .and_then(|(_, group)| first_float_attr(group, &["CalibratedMagnification"])),
        objective_lens_na: objective
            .as_ref()
            .and_then(|(_, group)| first_float_attr(group, &["LensNA", "NumericalAperture", "NA"])),
        objective_immersion: objective
            .as_ref()
            .and_then(|(_, group)| first_str_attr(group, &["Immersion"])),
        objective_correction: objective
            .as_ref()
            .and_then(|(_, group)| first_str_attr(group, &["Correction"])),
        objective_working_distance: objective
            .as_ref()
            .and_then(|(_, group)| first_float_attr(group, &["WorkingDistance"])),
        detector_model: detector
            .as_ref()
            .and_then(|(_, group)| first_str_attr(group, &["Model", "Name", "DetectorName"])),
        detector_manufacturer: detector
            .as_ref()
            .and_then(|(_, group)| first_str_attr(group, &["Manufacturer"])),
        detector_type: detector
            .as_ref()
            .and_then(|(_, group)| first_str_attr(group, &["Type", "DetectorType"])),
        detector_gain: detector
            .as_ref()
            .and_then(|(_, group)| first_float_attr(group, &["Gain", "DetectorGain"])),
        detector_offset: detector
            .as_ref()
            .and_then(|(_, group)| first_float_attr(group, &["Offset", "DetectorOffset"])),
        light_source_model: light_source
            .as_ref()
            .and_then(|(_, group)| first_str_attr(group, &["Model", "Name", "LightSourceName"])),
        light_source_manufacturer: light_source.as_ref().and_then(|(_, group)| {
            first_str_attr(group, &["Manufacturer", "LightSourceManufacturer"])
        }),
        light_source_type: light_source
            .as_ref()
            .and_then(|(_, group)| first_str_attr(group, &["Type", "LightSourceType"]))
            .or_else(|| {
                light_source.as_ref().map(|(path, _)| {
                    if path.contains("Laser") {
                        "Laser".to_string()
                    } else {
                        "GenericExcitationSource".to_string()
                    }
                })
            }),
        light_source_power: light_source.as_ref().and_then(|(_, group)| {
            first_float_attr(group, &["Power", "PowerMilliWatts", "LaserPower"])
        }),
    }
}

fn first_existing_group(
    file: &hdf5_pure_rust::File,
    paths: &[&str],
) -> Option<(String, hdf5_pure_rust::Group)> {
    paths.iter().find_map(|path| {
        file.group(path)
            .ok()
            .map(|group| ((*path).to_string(), group))
    })
}

fn first_str_attr(group: &hdf5_pure_rust::Group, names: &[&str]) -> Option<String> {
    names.iter().find_map(|name| read_str_attr(group, name))
}

fn first_float_attr(group: &hdf5_pure_rust::Group, names: &[&str]) -> Option<f64> {
    names
        .iter()
        .find_map(|name| read_str_attr(group, name).and_then(|value| parse_imaris_f64(&value)))
}

fn imaris_ome_instrument(instrument: &ImarisInstrumentMetadata) -> Option<OmeInstrument> {
    let mut ome_instrument = OmeInstrument {
        id: Some(create_lsid("Instrument", &[0])),
        microscope_model: instrument.microscope_model.clone(),
        microscope_manufacturer: instrument.microscope_manufacturer.clone(),
        ..OmeInstrument::default()
    };

    if instrument.objective_model.is_some()
        || instrument.objective_manufacturer.is_some()
        || instrument.objective_nominal_magnification.is_some()
        || instrument.objective_calibrated_magnification.is_some()
        || instrument.objective_lens_na.is_some()
        || instrument.objective_immersion.is_some()
        || instrument.objective_correction.is_some()
        || instrument.objective_working_distance.is_some()
    {
        ome_instrument.objectives.push(OmeObjective {
            id: Some(create_lsid("Objective", &[0, 0])),
            model: instrument.objective_model.clone(),
            manufacturer: instrument.objective_manufacturer.clone(),
            nominal_magnification: instrument.objective_nominal_magnification,
            calibrated_magnification: instrument.objective_calibrated_magnification,
            lens_na: instrument.objective_lens_na,
            immersion: instrument.objective_immersion.clone(),
            correction: instrument.objective_correction.clone(),
            working_distance: instrument.objective_working_distance,
        });
    }

    if instrument.detector_model.is_some()
        || instrument.detector_manufacturer.is_some()
        || instrument.detector_type.is_some()
        || instrument.detector_gain.is_some()
        || instrument.detector_offset.is_some()
    {
        ome_instrument.detectors.push(OmeDetector {
            id: Some(create_lsid("Detector", &[0, 0])),
            model: instrument.detector_model.clone(),
            manufacturer: instrument.detector_manufacturer.clone(),
            detector_type: instrument.detector_type.clone(),
            gain: instrument.detector_gain,
            offset: instrument.detector_offset,
        });
    }

    if instrument.light_source_model.is_some()
        || instrument.light_source_manufacturer.is_some()
        || instrument.light_source_type.is_some()
        || instrument.light_source_power.is_some()
    {
        ome_instrument.light_sources.push(OmeLightSource {
            id: Some(create_lsid("LightSource", &[0, 0])),
            model: instrument.light_source_model.clone(),
            manufacturer: instrument.light_source_manufacturer.clone(),
            light_source_type: instrument.light_source_type.clone(),
            power: instrument.light_source_power,
            wavelength: None,
        });
    }

    if ome_instrument.microscope_model.is_some()
        || ome_instrument.microscope_manufacturer.is_some()
        || !ome_instrument.objectives.is_empty()
        || !ome_instrument.detectors.is_empty()
        || !ome_instrument.light_sources.is_empty()
    {
        Some(ome_instrument)
    } else {
        None
    }
}

fn join_u64s(values: &[u64]) -> String {
    values
        .iter()
        .map(u64::to_string)
        .collect::<Vec<_>>()
        .join(" ")
}

/// Group the flat per-ResolutionLevel metadata into series, mirroring
/// ImarisHDFReader.initFile() lines ~287-311.
///
/// Java only runs the loop when `seriesCount > 1` (i.e. there is more than one
/// ResolutionLevel). Level 0 always seeds series 0 (`core.get(0,0)` / `ms0`).
/// For each subsequent level `i`, Java compares its sizeZ/sizeC/sizeT against
/// `ms0` (level 0, always). On a match it appends the level as a resolution of
/// the *current* series (`core.add(currentSeries, ms)`); on a mismatch it opens
/// a fresh series (`core.add(ms); currentSeries++`). The returned outer Vec is
/// the series list; each inner Vec holds that series' absolute Imaris levels,
/// finest first.
fn imaris_group_series(resolutions: &[ImageMetadata]) -> Vec<Vec<usize>> {
    if resolutions.is_empty() {
        return Vec::new();
    }
    // Series 0 is seeded by level 0 (ms0), independent of how many levels exist.
    let mut series: Vec<Vec<usize>> = vec![vec![0]];
    if resolutions.len() <= 1 {
        return series;
    }
    let ms0 = &resolutions[0];
    let mut current_series = 0usize;
    for (level, ms) in resolutions.iter().enumerate().skip(1) {
        if ms.size_z == ms0.size_z && ms.size_c == ms0.size_c && ms.size_t == ms0.size_t {
            // core.add(currentSeries, ms): append as a resolution of the
            // current series.
            series[current_series].push(level);
        } else {
            // core.add(ms); currentSeries++: this mismatched level becomes the
            // sole resolution of a brand-new series.
            series.push(vec![level]);
            current_series += 1;
        }
    }
    series
}

fn checked_image_count(size_z: u32, size_c: u32, size_t: u32, label: &str) -> Result<u32> {
    size_z
        .checked_mul(size_c)
        .and_then(|v| v.checked_mul(size_t))
        .ok_or_else(|| BioFormatsError::Format(format!("Imaris {label} image count overflows")))
}

fn count_imaris_indexed_members(members: &[String], prefix: &str) -> Result<u32> {
    let mut max_count = 0u32;
    for name in members {
        if let Some(index) = imaris_indexed_member_number(name, prefix) {
            max_count = max_count.max(index.checked_add(1).ok_or_else(|| {
                BioFormatsError::Format(format!("Imaris {prefix} index overflows"))
            })?);
        }
    }
    Ok(max_count)
}

fn imaris_indexed_member_number(name: &str, prefix: &str) -> Option<u32> {
    name.strip_prefix(prefix)?.parse::<u32>().ok()
}

fn ims_resolution_group_path(file: &hdf5_pure_rust::File, level: usize) -> Option<String> {
    [
        format!("DataSet/ResolutionLevel {level}"),
        format!("DataSet/ResolutionLevel_{level}"),
    ]
    .into_iter()
    .find(|path| file.group(path).is_ok())
}

fn ims_timepoint_group_path(
    file: &hdf5_pure_rust::File,
    level: usize,
    timepoint: usize,
) -> Option<String> {
    let resolution = ims_resolution_group_path(file, level)?;
    [
        format!("{resolution}/TimePoint {timepoint}"),
        format!("{resolution}/TimePoint_{timepoint}"),
    ]
    .into_iter()
    .find(|path| file.group(path).is_ok())
}

fn ims_data_path(
    file: &hdf5_pure_rust::File,
    level: usize,
    timepoint: usize,
    channel: usize,
) -> Option<String> {
    let timepoint = ims_timepoint_group_path(file, level, timepoint)?;
    [
        format!("{timepoint}/Channel {channel}/Data"),
        format!("{timepoint}/Channel_{channel}/Data"),
    ]
    .into_iter()
    .find(|path| file.dataset(path).is_ok())
}

fn ims_dataset_info_channel_path(file: &hdf5_pure_rust::File, channel: u32) -> Option<String> {
    [
        format!("DataSetInfo/Channel {channel}"),
        format!("DataSetInfo/Channel_{channel}"),
    ]
    .into_iter()
    .find(|path| file.group(path).is_ok())
}

/// Read the (z, y, x) pixel dimensions of a resolution level from its
/// full-resolution Channel-0 `Data` dataset shape (the authoritative source,
/// vs. the unreliable DataSetInfo X/Y/Z and per-level ImageSize* attributes).
fn ims_level_dims(file: &hdf5_pure_rust::File, level: usize) -> Result<(u32, u32, u32)> {
    let path = ims_data_path(file, level, 0, 0).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "Imaris: missing DataSet/ResolutionLevel {level}/TimePoint 0/Channel 0/Data or DataSet/ResolutionLevel_{level}/TimePoint_0/Channel_0/Data"
        ))
    })?;
    let ds = file
        .dataset(&path)
        .map_err(|e| BioFormatsError::UnsupportedFormat(format!("Imaris: missing {path}: {e}")))?;
    let shape = ds.shape().map_err(|e| {
        BioFormatsError::Format(format!("Imaris: cannot read shape for {path}: {e}"))
    })?;
    if shape.len() != 3 || shape.iter().any(|&d| d == 0) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imaris: unsupported Data shape {shape:?} for {path}"
        )));
    }
    let to_u32 = |d: u64| -> Result<u32> {
        u32::try_from(d)
            .map_err(|_| BioFormatsError::Format("Imaris dimension overflows u32".into()))
    };
    Ok((to_u32(shape[0])?, to_u32(shape[1])?, to_u32(shape[2])?))
}

fn validate_ims_data_dataset(
    file: &hdf5_pure_rust::File,
    path: &str,
    size_x: u32,
    size_y: u32,
    size_z: u32,
    bytes_per_sample: usize,
) -> Result<()> {
    let ds = file
        .dataset(path)
        .map_err(|e| BioFormatsError::UnsupportedFormat(format!("Imaris: missing {path}: {e}")))?;
    let shape = ds.shape().map_err(|e| {
        BioFormatsError::Format(format!("Imaris: cannot read shape for {path}: {e}"))
    })?;
    if shape.len() != 3 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imaris: {path} has unsupported rank {}",
            shape.len()
        )));
    }
    if shape[0] == 0 || shape[1] == 0 || shape[2] == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imaris: {path} has zero dataset axis"
        )));
    }
    let declared = [size_z as u64, size_y as u64, size_x as u64];
    if shape != declared {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imaris: {path} shape {shape:?} does not match declared {declared:?}"
        )));
    }
    let dtype_size = ds.dtype().map(|dt| hdf5_dtype_size(&dt)).map_err(|e| {
        BioFormatsError::Format(format!("Imaris: cannot read dtype for {path}: {e}"))
    })?;
    if dtype_size != bytes_per_sample {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Imaris: {path} dtype size {dtype_size} does not match declared {bytes_per_sample}"
        )));
    }
    Ok(())
}

fn hdf5_group_members(
    group: &hdf5_pure_rust::Group,
) -> std::result::Result<Vec<String>, hdf5_pure_rust::Error> {
    group.member_names()
}

/// Element byte size of an HDF5 datatype (the `size()` already reported by the
/// crate, which for Array types is the total array byte size).
fn hdf5_dtype_size(dtype: &hdf5_pure_rust::Datatype) -> usize {
    dtype.size()
}

impl FormatReader for ImarisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ims"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // HDF5 signature: bytes 0-7 = \x89HDF\r\n\x1a\n
        header.len() >= 8 && header[0..8] == [0x89, 0x48, 0x44, 0x46, 0x0d, 0x0a, 0x1a, 0x0a]
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let parsed = parse_ims(path)?;
        let file = hdf5_pure_rust::File::open(path)
            .map_err(|e| BioFormatsError::Format(format!("HDF5: {e}")))?;
        let mut resolutions = parsed.resolutions;
        // Build the Java series grouping, then fix up each level's reported
        // resolution_count to its series' resolution count (Java's per-series
        // core.size(series)) rather than the global ResolutionLevel total.
        let series = imaris_group_series(&resolutions);
        for group in &series {
            let count = group.len() as u32;
            for &level in group {
                if let Some(meta) = resolutions.get_mut(level) {
                    meta.resolution_count = count;
                }
            }
        }
        self.resolutions = resolutions;
        self.series = series;
        self.file = Some(file);
        self.path = Some(path.to_path_buf());
        self.current_series = 0;
        self.current_resolution = 0;
        self.bytes_per_sample = parsed.bytes_per_sample;
        self.extents = parsed.extents;
        self.recording_spacing = parsed.recording_spacing;
        self.image_description = parsed.image_description;
        self.channel_names = parsed.channel_names;
        self.channel_colors = parsed.channel_colors;
        self.channel_colors_normalized = parsed.channel_colors_normalized;
        self.channel_emission_wavelengths = parsed.channel_emission_wavelengths;
        self.channel_excitation_wavelengths = parsed.channel_excitation_wavelengths;
        self.instrument = parsed.instrument;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.file = None;
        self.resolutions.clear();
        self.series.clear();
        self.current_series = 0;
        self.current_resolution = 0;
        self.extents = None;
        self.recording_spacing = [None; 3];
        self.image_description = None;
        self.channel_names.clear();
        self.channel_colors.clear();
        self.channel_colors_normalized.clear();
        self.channel_emission_wavelengths.clear();
        self.channel_excitation_wavelengths.clear();
        self.instrument = ImarisInstrumentMetadata::default();
        self.cache = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.series.is_empty() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s >= self.series.len() {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.current_series = s;
        // Java setSeries resets the active resolution to the finest level.
        self.current_resolution = 0;
        Ok(())
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.current_imaris_level()
            .and_then(|level| self.resolutions.get(level))
            .or_else(|| self.resolutions.first())
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn resolution_count(&self) -> usize {
        self.series
            .get(self.current_series)
            .map(Vec::len)
            .unwrap_or(0)
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        let count = self
            .series
            .get(self.current_series)
            .map(Vec::len)
            .unwrap_or(0);
        if level >= count {
            return Err(BioFormatsError::Format(format!(
                "resolution {level} out of range"
            )));
        }
        self.current_resolution = level;
        Ok(())
    }

    /// Java ImarisHDFReader.get8BitLookupTable()/get16BitLookupTable(): return
    /// the ramp LUT for the plane's channel. openBytes() sets `lastChannel =
    /// getZCTCoords(no)[1]` before reading, and the LUT getters key off
    /// `lastChannel`. We fold that into this accessor: decode the channel from
    /// `plane_index` (XYZCT) and build that channel's ramp.
    ///
    /// Mirrors Java's guards exactly: only UINT8/UINT16 indexed data yields a
    /// table; otherwise (or if the channel has no colour) return None.
    fn lookup_table(&mut self, plane_index: u32) -> Result<Option<LookupTable>> {
        let level = match self.current_imaris_level() {
            Some(level) => level,
            None => return Ok(None),
        };
        let Some(meta) = self.resolutions.get(level) else {
            return Ok(None);
        };
        // get8/16BitLookupTable bail unless the data is indexed and UINT8/UINT16.
        if !meta.is_indexed {
            return Ok(None);
        }
        // Decode the channel (lastChannel = getZCTCoords(no)[1]) for XYZCT order.
        let sz = meta.size_z.max(1) as usize;
        let sc = meta.size_c.max(1) as usize;
        let c = (plane_index as usize / sz) % sc;
        // Java: `if (lastChannel < 0 || lastChannel >= colors.size() ...)` → null.
        // colors.get(lastChannel) being null also yields null (16-bit path).
        let color = match self.channel_colors_normalized.get(c).copied().flatten() {
            Some(color) => color,
            None => return Ok(None),
        };
        Ok(match meta.pixel_type {
            PixelType::Uint8 => Some(imaris_8bit_lookup_table(color)),
            PixelType::Uint16 => Some(imaris_16bit_lookup_table(color)),
            _ => None,
        })
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let res = self
            .current_imaris_level()
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = self
            .resolutions
            .get(res)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        // Decode plane_index → (z, c, t) for XYZCT order using this level's dims
        let sz = meta.size_z as usize;
        let sc = meta.size_c as usize;
        let z = (plane_index as usize) % sz;
        let c = (plane_index as usize / sz) % sc;
        let t = (plane_index as usize) / (sz * sc);
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;
        let bps = self.bytes_per_sample;
        let plane_bytes = size_x * size_y * bps;

        // Reuse the cached plane if it is for the same (resolution, t, c, z).
        let need_load = match &self.cache {
            Some(cache) => cache.res != res || cache.t != t || cache.c != c || cache.z != z,
            None => true,
        };
        if need_load {
            let file = self.file.as_ref().ok_or(BioFormatsError::NotInitialized)?;
            let data_path = ims_data_path(&file, res, t, c).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!(
                    "Imaris: missing DataSet/ResolutionLevel {res}/TimePoint {t}/Channel {c}/Data or DataSet/ResolutionLevel_{res}/TimePoint_{t}/Channel_{c}/Data"
                ))
            })?;
            let ds = file
                .dataset(&data_path)
                .map_err(|e| BioFormatsError::Format(format!("dataset {data_path}: {e}")))?;

            // The dataset is shaped [z, y, x]; use a hyperslab selection to read
            // ONLY the requested z-plane. The returned vec is exactly that plane
            // (Y*X elements) indexed from 0, so it is cached and returned whole.
            let sel = Selection::Hyperslab(vec![
                HyperslabDim::new(z as u64, 1, 1, 1),      // single z slice
                HyperslabDim::new(0, 1, size_y as u64, 1), // all rows
                HyperslabDim::new(0, 1, size_x as u64, 1), // all cols
            ]);

            let raw: Vec<u8> = match bps {
                1 => ds
                    .read_slice::<u8, _>(sel)
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
                2 => {
                    let words: Vec<u16> = ds
                        .read_slice::<u16, _>(sel)
                        .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                    words.iter().flat_map(|w| w.to_le_bytes()).collect()
                }
                4 => {
                    let dwords: Vec<u32> = ds
                        .read_slice::<u32, _>(sel)
                        .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                    dwords.iter().flat_map(|d| d.to_le_bytes()).collect()
                }
                _ => ds
                    .read_slice::<u8, _>(sel)
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
            };
            self.cache = Some(VolumeCache { res, t, c, z, raw });
        }

        let raw = &self.cache.as_ref().unwrap().raw;
        // raw is now exactly plane z, indexed from offset 0.
        if plane_bytes <= raw.len() {
            Ok(raw[..plane_bytes].to_vec())
        } else {
            Err(BioFormatsError::UnsupportedFormat(format!(
                "Imaris ResolutionLevel {res}/TimePoint {t}/Channel {c} plane {plane_index} is \
                 shorter than declared (need {} bytes, have {})",
                plane_bytes,
                raw.len()
            )))
        }
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let res = self
            .current_imaris_level()
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = self
            .resolutions
            .get(res)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let x2 = x
            .checked_add(w)
            .ok_or_else(|| BioFormatsError::Format("Imaris region width overflows".into()))?;
        let y2 = y
            .checked_add(h)
            .ok_or_else(|| BioFormatsError::Format("Imaris region height overflows".into()))?;
        if x2 > meta.size_x || y2 > meta.size_y {
            return Err(BioFormatsError::Format(
                "Imaris region is outside image bounds".into(),
            ));
        }

        let sz = meta.size_z as usize;
        let sc = meta.size_c as usize;
        let z = (plane_index as usize) % sz;
        let c = (plane_index as usize / sz) % sc;
        let t = (plane_index as usize) / (sz * sc);
        let bps = self.bytes_per_sample;
        let expected = (w as usize)
            .checked_mul(h as usize)
            .and_then(|v| v.checked_mul(bps))
            .ok_or_else(|| BioFormatsError::Format("Imaris region byte count overflows".into()))?;

        let file = self.file.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let data_path = ims_data_path(&file, res, t, c).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!(
                "Imaris: missing DataSet/ResolutionLevel {res}/TimePoint {t}/Channel {c}/Data or DataSet/ResolutionLevel_{res}/TimePoint_{t}/Channel_{c}/Data"
            ))
        })?;
        let ds = file
            .dataset(&data_path)
            .map_err(|e| BioFormatsError::Format(format!("dataset {data_path}: {e}")))?;
        let sel = Selection::Hyperslab(vec![
            HyperslabDim::new(z as u64, 1, 1, 1),
            HyperslabDim::new(y as u64, 1, h as u64, 1),
            HyperslabDim::new(x as u64, 1, w as u64, 1),
        ]);

        let raw: Vec<u8> = match bps {
            1 => ds
                .read_slice::<u8, _>(sel)
                .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
            2 => {
                let words: Vec<u16> = ds
                    .read_slice::<u16, _>(sel)
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                words.iter().flat_map(|w| w.to_le_bytes()).collect()
            }
            4 => {
                let dwords: Vec<u32> = ds
                    .read_slice::<u32, _>(sel)
                    .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?;
                dwords.iter().flat_map(|d| d.to_le_bytes()).collect()
            }
            _ => ds
                .read_slice::<u8, _>(sel)
                .map_err(|e| BioFormatsError::Format(format!("HDF5 read: {e}")))?,
        };

        if raw.len() == expected {
            Ok(raw)
        } else {
            Err(BioFormatsError::UnsupportedFormat(format!(
                "Imaris ResolutionLevel {res}/TimePoint {t}/Channel {c} plane {plane_index} \
                 region is shorter than declared (need {expected} bytes, have {})",
                raw.len()
            )))
        }
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        // Try to read the Imaris built-in thumbnail
        if let Some(file) = self.file.as_ref() {
            if let Ok(ds) = file.dataset("Thumbnail/Data") {
                if let Ok(data) = ds.read::<u8>() {
                    return Ok(data);
                }
            }
        }
        // Fall back to center crop of plane 0
        let level = self
            .current_imaris_level()
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = self
            .resolutions
            .get(level)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(0, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let level = self.current_imaris_level()?;
        let meta = self.resolutions.get(level)?;
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        if let Some(instrument) = imaris_ome_instrument(&self.instrument) {
            ome.instruments.push(instrument);
        }
        {
            let img = ome.images.get_mut(0)?;

            // Image name = "<basename> Resolution Level <level+1>" (Java
            // ImarisHDFReader sets the name per series/resolution-level).
            if let Some(path) = self.path.as_ref() {
                let base = path
                    .file_name()
                    .map(|n| n.to_string_lossy().into_owned())
                    .unwrap_or_default();
                // Java sets store.setImageName(name + " Resolution Level " +
                // (s + 1), s) per series s (initFile ~361-363).
                img.name = Some(format!(
                    "{base} Resolution Level {}",
                    self.current_series + 1
                ));
            }
            img.description = self.image_description.clone();

            // Physical pixel size: Java uses RecordingEntry*Spacing unless the
            // value is the default 1, then falls back to (ExtMax - ExtMin) / size.
            if let Some(ext) = self.extents {
                let span = |hi: f64, lo: f64, n: u32| {
                    if n > 0 {
                        Some((hi - lo) / n as f64)
                    } else {
                        None
                    }
                };
                img.physical_size_x = span(ext[3], ext[0], meta.size_x);
                img.physical_size_y = span(ext[4], ext[1], meta.size_y);
                img.physical_size_z = span(ext[5], ext[2], meta.size_z);
            }
            let spacing_or_existing = |spacing: Option<f64>, existing: Option<f64>| {
                spacing
                    .filter(|v| v.is_finite() && *v > 0.0 && (*v - 1.0).abs() > f64::EPSILON)
                    .or(existing)
            };
            img.physical_size_x =
                spacing_or_existing(self.recording_spacing[0], img.physical_size_x);
            img.physical_size_y =
                spacing_or_existing(self.recording_spacing[1], img.physical_size_y);
            img.physical_size_z =
                spacing_or_existing(self.recording_spacing[2], img.physical_size_z);

            // Per-channel names.
            for (ci, ch) in img.channels.iter_mut().enumerate() {
                if let Some(Some(name)) = self.channel_names.get(ci) {
                    ch.name = Some(name.clone());
                }
                if let Some(Some(color)) = self.channel_colors.get(ci) {
                    ch.color = Some(*color);
                }
                if let Some(Some(emission)) = self.channel_emission_wavelengths.get(ci) {
                    ch.emission_wavelength = Some(*emission);
                }
                if let Some(Some(excitation)) = self.channel_excitation_wavelengths.get(ci) {
                    ch.excitation_wavelength = Some(*excitation);
                }
            }
            if !ome.instruments.is_empty() {
                img.instrument_ref = Some(0);
                if !ome.instruments[0].objectives.is_empty() {
                    img.objective_ref = Some(0);
                }
            }
        }
        ome.rois.extend(imaris_ome_rois_from_metadata(meta));
        let _ = ome.add_original_metadata_annotations(meta, 0);

        Some(ome)
    }
}
