//! Zeiss XRM/TXRM X-ray tomography format reader.
//!
//! Bio-Formats' Java `ZeissXRMReader` reads these files as CFB/OLE2 compound
//! documents.  This Rust reader implements the same bounded core path:
//! dimensions and datatype from `Root Entry/ImageInfo/*`, and uncompressed
//! plane streams from `Root Entry/ImageData/ImageN`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ole::{cfb_path_without_root, OleFile};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

const IMAGE_DATA: &str = "/ImageData/";
const IMAGE_INFO: &str = "/ImageInfo/";
const RECON_SETTINGS: &str = "/ReconSettings/";
const AUTORECON: &str = "/AutoRecon/";
const REFERENCE: &str = "/ReferenceData/";
const OLE2_MAGIC: &[u8; 8] = &[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1];

pub struct ZeissXrmReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    image_paths: Vec<Option<String>>,
}

impl ZeissXrmReader {
    pub fn new() -> Self {
        ZeissXrmReader {
            path: None,
            meta: None,
            image_paths: Vec::new(),
        }
    }
}

impl Default for ZeissXrmReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ZeissXrmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        // Java ZeissXRMReader registers only "txm" and "txrm".
        matches!(ext.as_deref(), Some("txrm") | Some("txm"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Java ZeissXRMReader.isThisType(RandomAccessInputStream) checks only
        // the POI/OLE2 compound-file magic.
        header.starts_with(OLE2_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let (meta, image_paths) = parse_xrm(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.image_paths = image_paths;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.image_paths.clear();
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() || s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        0
    }

    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let stream_path = self
            .image_paths
            .get(plane_index as usize)
            .and_then(|path| path.as_ref())
            .ok_or_else(|| {
                BioFormatsError::Format(format!("XRM plane {plane_index} has no ImageData stream"))
            })?
            .clone();
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();

        let mut ole = OleFile::open(&path)?;
        let raw = ole.document_bytes(&stream_path)?;

        xrm_flip_rows(&raw, meta)
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("XRM", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        let pixel_size = meta
            .series_metadata
            .get("Image Details: Pixel size (µm)")
            .and_then(|value| match value {
                MetadataValue::Float(v) if *v > 0.0 => Some(*v),
                MetadataValue::Int(v) if *v > 0 => Some(*v as f64),
                _ => None,
            });
        if let (Some(image), Some(pixel_size)) = (ome.images.get_mut(0), pixel_size) {
            image.physical_size_x = Some(pixel_size);
            image.physical_size_y = Some(pixel_size);
            image.physical_size_z = Some(pixel_size);
        }
        Some(ome)
    }
}

fn parse_xrm(path: &Path) -> Result<(ImageMetadata, Vec<Option<String>>)> {
    let mut ole = OleFile::open(path)?;

    // Java keys metadata emission off the .txm/.txrm suffix (initFile: isTXM/isTXRM).
    let suffix = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let is_txm = suffix.as_deref() == Some("txm");
    let is_txrm = suffix.as_deref() == Some("txrm");
    // Java initFile: paramsPrefix = isTXM ? GENERAL_PARAMS : PROJECTION.
    let params_prefix = if is_txm {
        "General Parameters: "
    } else {
        "Projection Info: "
    };

    let mut size_x = None;
    let mut size_y = None;
    let mut pixel_type = None;
    let mut metadata = HashMap::new();
    let mut image_paths = Vec::new();
    let mut pixel_size = None;
    let mut exposure_times: Option<Vec<f64>> = None;
    let mut current: Option<Vec<f64>> = None;
    let mut voltage: Option<Vec<f64>> = None;
    let mut x_pos: Option<Vec<f64>> = None;
    let mut y_pos: Option<Vec<f64>> = None;
    let mut z_pos: Option<Vec<f64>> = None;
    let mut datestamps: Option<Vec<String>> = None;

    let paths: Vec<String> = ole
        .document_list()
        .into_iter()
        .map(|path| normalize_cfb_path(&path))
        .collect();

    for path in paths {
        if path.starts_with(IMAGE_DATA) {
            let index = xrm_image_index(&path).ok_or_else(|| {
                BioFormatsError::Format(format!("Zeiss XRM/TXRM invalid image stream path: {path}"))
            })?;
            if index == 0 {
                return Err(BioFormatsError::Format(format!(
                    "Zeiss XRM/TXRM invalid one-based image stream index in {path}"
                )));
            }
            let slot = (index - 1) as usize;
            if slot >= image_paths.len() {
                image_paths.resize(slot + 1, None);
            }
            image_paths[slot] = Some(path);
        } else if is_txm && handle_txm_metadata(&mut ole, &path, &mut metadata, &mut voltage)? {
        } else if is_txrm
            && handle_txrm_metadata(
                &mut ole,
                &path,
                &mut metadata,
                &mut current,
                &mut x_pos,
                &mut y_pos,
                &mut z_pos,
                &mut datestamps,
            )?
        {
        } else if path == "/ImageInfo/ImageWidth" {
            let v = read_xrm_i32(&mut ole, &path)?;
            size_x = Some(v);
            metadata.insert(
                "Image Details: Image width (pixels)".into(),
                MetadataValue::Int(v as i64),
            );
        } else if path == "/ImageInfo/ImageHeight" {
            let v = read_xrm_i32(&mut ole, &path)?;
            size_y = Some(v);
            metadata.insert(
                "Image Details: Image height (pixels)".into(),
                MetadataValue::Int(v as i64),
            );
        } else if path == "/ImageInfo/DataType" {
            let code = read_xrm_i32(&mut ole, &path)?;
            let (ty, label) = xrm_pixel_type(code)?;
            pixel_type = Some(ty);
            if is_txm {
                metadata.insert(
                    "Reconstruction Settings: Output data type".into(),
                    MetadataValue::String(label.into()),
                );
            }
            metadata.insert(
                "Image Details: Data type".into(),
                MetadataValue::String(label.into()),
            );
        } else if path == "/ImageInfo/FileType" {
            if let Ok(value) = read_xrm_string(&mut ole, &path) {
                metadata.insert(
                    "Image Details: File type".into(),
                    MetadataValue::String(value),
                );
            }
        } else if path == "/ImageInfo/PixelSize" {
            if let Ok(value) = read_xrm_f32(&mut ole, &path) {
                pixel_size = Some(value as f64);
                metadata.insert(
                    "Image Details: Pixel size (µm)".into(),
                    MetadataValue::Float(value as f64),
                );
            }
        } else if path == "/ImageInfo/AcquisitionMode" {
            let mode = read_xrm_i32(&mut ole, &path)?;
            let mode_value = match mode {
                0 => "Tomography".to_string(),
                10 => "Recon".to_string(),
                other => other.to_string(),
            };
            metadata.insert(
                "Image Details: Acquisition mode".into(),
                MetadataValue::String(mode_value),
            );
        } else if path == "/ImageInfo/Current" && current.is_none() {
            let values = read_xrm_f32_array(&mut ole, &path)?;
            if let Some(&value) = values.first() {
                metadata.insert(
                    "Source Assembly Info: Current (µA)".into(),
                    MetadataValue::Float(value),
                );
                if is_txm {
                    metadata.insert(
                        "General Parameters: X-ray current (µA)".into(),
                        MetadataValue::Float(value),
                    );
                }
            }
            current = Some(values);
        } else if path == "/ImageInfo/XrayVoltage" && voltage.is_none() {
            let values = read_xrm_f32_array(&mut ole, &path)?;
            add_xrm_metadata_list(
                &mut metadata,
                &format!("{params_prefix}X-ray voltage (kV)"),
                &values,
                !is_txm,
            );
            voltage = Some(values);
        } else if path == "/ImageInfo/SourceFilterName" {
            if let Ok(value) = read_xrm_string(&mut ole, &path) {
                metadata.insert(
                    "Source Assembly Info: Source Filter Name".into(),
                    MetadataValue::String(value.clone()),
                );
                metadata.insert(
                    format!("{params_prefix}Source filter name"),
                    MetadataValue::String(value),
                );
            }
        } else if path == "/ImageInfo/Voltage" {
            if let Ok(value) = read_xrm_f32(&mut ole, &path) {
                metadata.insert(
                    "Source Assembly Info: Voltage (kV)".into(),
                    MetadataValue::Float(value as f64),
                );
            }
        } else if path == "/exeVersion" {
            if let Ok(value) = read_xrm_string(&mut ole, &path) {
                metadata.insert(
                    "Dataset Info: Executable version".into(),
                    MetadataValue::String(value),
                );
            }
        } else if path == "/DetAssemblyInfo/LensInfo/LensName" {
            if let Ok(value) = read_xrm_string(&mut ole, &path) {
                metadata.insert(
                    format!("{params_prefix}Objective name"),
                    MetadataValue::String(value),
                );
            }
        } else if path == "/ImageInfo/CameraNumberOfFramesPerImage" {
            let v = read_xrm_i32(&mut ole, &path)?;
            metadata.insert(
                format!("{params_prefix}Frames per image"),
                MetadataValue::Int(v as i64),
            );
        } else if path == "/ImageInfo/NoOfImagesAveraged" {
            let v = read_xrm_i32(&mut ole, &path)?;
            metadata.insert(
                format!("{params_prefix}Images per projection"),
                MetadataValue::Int(v as i64),
            );
        } else if path == "/ImageInfo/ExpTimes" {
            let values = read_xrm_f32_array(&mut ole, &path)?;
            if values.len() > 1 {
                metadata.insert(
                    format!("{params_prefix}Exposure time (s)"),
                    MetadataValue::String(format_xrm_double(values[0])),
                );
            }
            exposure_times = Some(values);
        } else if path == "/ImageInfo/CameraBinning" {
            let v = read_xrm_i32(&mut ole, &path)?;
            metadata.insert(
                format!("{params_prefix}Camera binning"),
                MetadataValue::Int(v as i64),
            );
        }
    }

    if image_paths.iter().all(Option::is_none) {
        return Err(BioFormatsError::UnsupportedFormat(
            "Zeiss XRM/TXRM contains no Root Entry/ImageData/ImageN streams".into(),
        ));
    }

    let size_x = size_x.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Zeiss XRM/TXRM missing ImageInfo/ImageWidth".into())
    })?;
    let size_y = size_y.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Zeiss XRM/TXRM missing ImageInfo/ImageHeight".into())
    })?;
    if size_x <= 0 || size_y <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Zeiss XRM/TXRM has invalid non-positive image dimensions".into(),
        ));
    }
    let size_x = size_x as u32;
    let size_y = size_y as u32;
    let pixel_type = pixel_type.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Zeiss XRM/TXRM missing ImageInfo/DataType".into())
    })?;

    let bits = (pixel_type.bytes_per_sample() * 8) as u8;
    let image_count = image_paths.len() as u32;
    add_derived_xrm_metadata(
        &mut metadata,
        path,
        suffix.as_deref().unwrap_or_default(),
        is_txm,
        is_txrm,
        size_x,
        size_y,
        pixel_size,
        current.as_deref(),
        voltage.as_deref(),
        exposure_times.as_deref(),
        x_pos.as_deref(),
        y_pos.as_deref(),
        z_pos.as_deref(),
        datestamps.as_deref(),
        image_count,
    );
    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z: image_count,
        size_c: 1,
        size_t: 1,
        pixel_type,
        bits_per_pixel: (bits).into(),
        image_count,
        dimension_order: DimensionOrder::XYZTC,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        thumbnail: false,
        series_metadata: metadata,
        lookup_table: None,
        modulo_z: Some(crate::common::metadata::ModuloAnnotation {
            parent_dimension: "Z".into(),
            modulo_type: "rotation".into(),
            start: 0.0,
            step: 1.0,
            end: image_count.saturating_sub(1) as f64,
            unit: String::new(),
            labels: Vec::new(),
        }),
        modulo_c: None,
        modulo_t: None,
    };
    Ok((meta, image_paths))
}

fn normalize_cfb_path(path: &str) -> String {
    format!("/{}", cfb_path_without_root(path))
}

fn read_xrm_stream(ole: &mut OleFile, path: &str) -> Result<Vec<u8>> {
    ole.document_bytes(path)
}

fn read_xrm_i32(ole: &mut OleFile, path: &str) -> Result<i32> {
    let data = read_xrm_stream(ole, path)?;
    let bytes = data
        .get(..4)
        .ok_or_else(|| BioFormatsError::Format(format!("XRM stream {path} is shorter than i32")))?;
    Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_xrm_f32(ole: &mut OleFile, path: &str) -> Result<f32> {
    let data = read_xrm_stream(ole, path)?;
    let bytes = data
        .get(..4)
        .ok_or_else(|| BioFormatsError::Format(format!("XRM stream {path} is shorter than f32")))?;
    Ok(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_xrm_f32_array(ole: &mut OleFile, path: &str) -> Result<Vec<f64>> {
    let data = read_xrm_stream(ole, path)?;
    Ok(data
        .as_chunks::<4>()
        .0
        .iter()
        .map(|bytes| f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as f64)
        .collect())
}

fn read_xrm_string(ole: &mut OleFile, path: &str) -> Result<String> {
    let data = read_xrm_stream(ole, path)?;
    Ok(String::from_utf8_lossy(&data)
        .trim_matches(char::from(0))
        .trim()
        .to_string())
}

fn read_xrm_date_array(ole: &mut OleFile, path: &str) -> Result<Vec<String>> {
    let data = read_xrm_stream(ole, path)?;
    Ok(data
        .as_chunks::<40>()
        .0
        .iter()
        .map(|chunk| {
            String::from_utf8_lossy(&chunk[..23])
                .trim_matches(char::from(0))
                .trim()
                .to_string()
        })
        .collect())
}

fn read_xrm_yes_no(ole: &mut OleFile, path: &str) -> Result<&'static str> {
    let data = read_xrm_stream(ole, path)?;
    Ok(if data.first().copied().unwrap_or(0) == 0 {
        "No"
    } else {
        "Yes"
    })
}

fn handle_txm_metadata(
    ole: &mut OleFile,
    path: &str,
    metadata: &mut HashMap<String, MetadataValue>,
    voltage: &mut Option<Vec<f64>>,
) -> Result<bool> {
    match path {
        p if p == xrm_path(AUTORECON, "MeanSampleX") => {
            insert_formatted_f32(ole, path, metadata, "Positions: Mean sample X (µm)")?;
        }
        p if p == xrm_path(AUTORECON, "MeanSampleY") => {
            insert_formatted_f32(ole, path, metadata, "Positions: Mean sample Y (µm)")?;
        }
        p if p == xrm_path(AUTORECON, "MeanSampleZ") => {
            insert_formatted_f32(ole, path, metadata, "Positions: Mean sample Z (µm)")?;
        }
        p if p == xrm_path(RECON_SETTINGS, "SourceVoltage") => {
            let values = read_xrm_f32_array(ole, path)?;
            if let Some(&value) = values.first() {
                metadata.insert(
                    "General Parameters: X-ray voltage (kV)".into(),
                    MetadataValue::Float(value),
                );
            }
            *voltage = Some(values);
        }
        p if p == xrm_path(RECON_SETTINGS, "OutputFileLocation") => {
            insert_string(
                ole,
                path,
                metadata,
                "Reconstruction Settings: Output file location",
            )?;
        }
        p if p == xrm_path(RECON_SETTINGS, "InputFileName") => {
            insert_string(
                ole,
                path,
                metadata,
                "Reconstruction Settings: Input filename",
            )?;
        }
        p if p == xrm_path(RECON_SETTINGS, "CenterShift") => {
            insert_f32(ole, path, metadata, "Reconstruction Settings: Center shift")?;
        }
        p if p == xrm_path(RECON_SETTINGS, "BeamHardeningFileName") => {
            insert_string(
                ole,
                path,
                metadata,
                "Reconstruction Settings: Beam hardening",
            )?;
        }
        p if p == xrm_path(RECON_SETTINGS, "BeamHardening") => {
            insert_f32(
                ole,
                path,
                metadata,
                "Reconstruction Settings: Beam-hardening constant",
            )?;
        }
        p if p == xrm_path(RECON_SETTINGS, "RotationAngle") => {
            insert_f32(
                ole,
                path,
                metadata,
                "Reconstruction Settings: Rotation angle",
            )?;
        }
        p if p == xrm_path(RECON_SETTINGS, "ReconFilterChoice") => {
            let filter = read_xrm_i32(ole, path)?;
            metadata.insert(
                "Reconstruction Settings: Recon filter".into(),
                MetadataValue::String(if filter == 2 {
                    "Smooth".into()
                } else {
                    filter.to_string()
                }),
            );
        }
        p if p == xrm_path(RECON_SETTINGS, "ReconFilterSmoothFactor") => {
            insert_f32(ole, path, metadata, "Reconstruction Settings: Sigma")?;
        }
        p if p == xrm_path(RECON_SETTINGS, "ReconScalingEnum") => {
            let scaling = read_xrm_i32(ole, path)?;
            metadata.insert(
                "Reconstruction Settings: Recon scaling".into(),
                MetadataValue::String(if scaling == 0 {
                    "Global".into()
                } else {
                    scaling.to_string()
                }),
            );
        }
        p if p == xrm_path(RECON_SETTINGS, "GlobalMax") => {
            insert_f32(ole, path, metadata, "Reconstruction Settings: Global max")?;
        }
        p if p == xrm_path(RECON_SETTINGS, "GlobalMin") => {
            insert_f32(ole, path, metadata, "Reconstruction Settings: Global min")?;
        }
        p if p == xrm_path(RECON_SETTINGS, "UserMinMax") => {
            insert_yes_no(ole, path, metadata, "Reconstruction Settings: User min-max")?;
        }
        p if p == xrm_path(RECON_SETTINGS, "UseCTScaleFilter") => {
            insert_yes_no(
                ole,
                path,
                metadata,
                "Reconstruction Settings: Use CT-Scaling",
            )?;
        }
        p if p == xrm_path(RECON_SETTINGS, "CTScaleFilter") => {
            insert_string(
                ole,
                path,
                metadata,
                "Reconstruction Settings: CT-scale name",
            )?;
        }
        p if p == xrm_path(RECON_SETTINGS, "SecondaryReferenceFileName") => {
            insert_string(
                ole,
                path,
                metadata,
                "Reconstruction Settings: Secondary ref. filename",
            )?;
        }
        p if p == xrm_path(RECON_SETTINGS, "SecRefSourceFilterName") => {
            insert_string(
                ole,
                path,
                metadata,
                "Reconstruction Settings: Secondary ref. filter name",
            )?;
        }
        p if p == xrm_path(RECON_SETTINGS, "SecondaryRefCollectionMode") => {
            let mode = read_xrm_i32(ole, path)?;
            metadata.insert(
                "Reconstruction Settings: Secondary ref. collection".into(),
                MetadataValue::String(if mode == 0 {
                    "None".into()
                } else {
                    mode.to_string()
                }),
            );
        }
        p if p == xrm_path(RECON_SETTINGS, "ReconOperation") => {
            let value = read_xrm_i32(ole, path)?;
            metadata.insert(
                "Reconstruction Settings: Output down-sampling".into(),
                MetadataValue::Int(value as i64),
            );
        }
        p if p == xrm_path(AUTORECON, "StoRADistance") => {
            let value = read_xrm_f32(ole, path)? as f64 / -1000.0;
            let formatted = format_xrm_double(value);
            metadata.insert(
                "Positions: Source to RA (mm)".into(),
                MetadataValue::String(formatted.clone()),
            );
            metadata.insert(
                "General Parameters: Source-RA (mm)".into(),
                MetadataValue::String(formatted),
            );
        }
        p if p == xrm_path(AUTORECON, "DtoRADistance") => {
            let value = read_xrm_f32(ole, path)? as f64 / 1000.0;
            let formatted = format_xrm_double(value);
            metadata.insert(
                "Positions: Detector to RA (mm)".into(),
                MetadataValue::String(formatted.clone()),
            );
            metadata.insert(
                "General Parameters: Detector-RA (mm)".into(),
                MetadataValue::String(formatted),
            );
        }
        p if p == xrm_path(AUTORECON, "NumOfProjects") => {
            let value = read_xrm_i32(ole, path)?;
            metadata.insert(
                "General Parameters: Number of projections used".into(),
                MetadataValue::Int(value as i64),
            );
        }
        p if p == xrm_path(RECON_SETTINGS, "ReconServiceVersion") => {
            insert_string(ole, path, metadata, "Dataset Info: Recon Service Version")?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn handle_txrm_metadata(
    ole: &mut OleFile,
    path: &str,
    metadata: &mut HashMap<String, MetadataValue>,
    current: &mut Option<Vec<f64>>,
    x_pos: &mut Option<Vec<f64>>,
    y_pos: &mut Option<Vec<f64>>,
    z_pos: &mut Option<Vec<f64>>,
    datestamps: &mut Option<Vec<String>>,
) -> Result<bool> {
    match path {
        p if p == xrm_path(REFERENCE, "ImageInfo/XrayMagnification") => {
            insert_formatted_f32(
                ole,
                path,
                metadata,
                "Projection Info: Geometric Magnification",
            )?;
        }
        "/Selection/SelectedImages" => {
            insert_yes_no(ole, path, metadata, "Projection Info: Selected")?;
        }
        p if p == xrm_path(IMAGE_INFO, "XrayCurrent") => {
            let values = read_xrm_f32_array(ole, path)?;
            add_xrm_metadata_list(
                metadata,
                "Projection Info: X-ray current (µA)",
                &values,
                true,
            );
            *current = Some(values);
        }
        p if p == xrm_path(IMAGE_INFO, "XPosition") => {
            *x_pos = Some(read_xrm_f32_array(ole, path)?)
        }
        p if p == xrm_path(IMAGE_INFO, "YPosition") => {
            *y_pos = Some(read_xrm_f32_array(ole, path)?)
        }
        p if p == xrm_path(IMAGE_INFO, "ZPosition") => {
            *z_pos = Some(read_xrm_f32_array(ole, path)?)
        }
        p if p == xrm_path(IMAGE_INFO, "DtoRADistance") => {
            insert_formatted_f32(ole, path, metadata, "Projection Info: Detector-RA (mm)")?;
        }
        p if p == xrm_path(IMAGE_INFO, "StoRADistance") => {
            insert_formatted_f32(ole, path, metadata, "Projection Info: Source-RA (mm)")?;
        }
        p if p == xrm_path(IMAGE_INFO, "FanAngle") => {
            let values = read_xrm_f32_array(ole, path)?;
            add_xrm_metadata_list(metadata, "Projection Info: Fan angle", &values, true);
        }
        p if p == xrm_path(IMAGE_INFO, "ConeAngle") => {
            let values = read_xrm_f32_array(ole, path)?;
            add_xrm_metadata_list(metadata, "Projection Info: Cone angle", &values, true);
        }
        p if p == xrm_path(IMAGE_INFO, "Angles") => {
            let values = read_xrm_f32_array(ole, path)?;
            add_xrm_metadata_list(metadata, "Projection Info: Angle", &values, true);
        }
        p if p == xrm_path(IMAGE_INFO, "ReadOutTime") => {
            let value = read_xrm_i32(ole, path)?;
            metadata.insert(
                "Projection Info: Camera Readout Speed".into(),
                MetadataValue::Int(value as i64),
            );
        }
        p if p == xrm_path(IMAGE_INFO, "Temperature") => {
            let value = read_xrm_i32(ole, path)?;
            metadata.insert(
                "Projection Info: Camera Temperature".into(),
                MetadataValue::Int(value as i64),
            );
        }
        p if p == xrm_path(IMAGE_INFO, "Date") => {
            let values = read_xrm_date_array(ole, path)?;
            for value in &values {
                add_xrm_metadata_list_value(
                    metadata,
                    "Projection Info: Date",
                    MetadataValue::String(value.clone()),
                );
            }
            *datestamps = Some(values);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

fn add_derived_xrm_metadata(
    metadata: &mut HashMap<String, MetadataValue>,
    path: &Path,
    suffix: &str,
    is_txm: bool,
    is_txrm: bool,
    size_x: u32,
    size_y: u32,
    pixel_size: Option<f64>,
    current: Option<&[f64]>,
    voltage: Option<&[f64]>,
    exposure_times: Option<&[f64]>,
    x_pos: Option<&[f64]>,
    y_pos: Option<&[f64]>,
    z_pos: Option<&[f64]>,
    datestamps: Option<&[String]>,
    image_count: u32,
) {
    metadata.insert(
        "Dataset Info: Data file name".into(),
        MetadataValue::String(path.to_string_lossy().into_owned()),
    );
    if is_txm {
        metadata.insert(
            "Reconstruction Settings: Output file-format".into(),
            MetadataValue::String(suffix.into()),
        );
        if let (Some(current), Some(voltage)) = (current, voltage) {
            if let (Some(&c), Some(&v)) = (current.first(), voltage.first()) {
                metadata.insert(
                    "General Parameters: X-ray power (W)".into(),
                    MetadataValue::Float((c * v) / 1000.0),
                );
            }
        }
    } else if is_txrm {
        if let (Some(current), Some(voltage)) = (current, voltage) {
            for i in 0..current.len().min(voltage.len()) {
                add_xrm_metadata_list_value(
                    metadata,
                    "Projection Info: X-ray power (W)",
                    MetadataValue::String(format_xrm_double((current[i] * voltage[i]) / 1000.0)),
                );
            }
        }
    }
    metadata.insert(
        "Image Details: File type".into(),
        MetadataValue::String(suffix.into()),
    );
    if let Some(pixel_size) = pixel_size {
        metadata.insert(
            "Image Details: Field of view (µm)".into(),
            MetadataValue::String(format!(
                "{}, {}",
                format_xrm_double(size_x as f64 * pixel_size),
                format_xrm_double(size_y as f64 * pixel_size)
            )),
        );
    }

    for plane in 0..image_count as usize {
        let prefix = format!("plane.{plane}");
        if let Some(values) = exposure_times {
            if let Some(&value) = values.get(plane) {
                metadata.insert(
                    format!("{prefix}.exposure_time"),
                    MetadataValue::Float(value),
                );
            }
        }
        if let Some(values) = x_pos {
            if let Some(&value) = values.get(plane) {
                metadata.insert(format!("{prefix}.position_x"), MetadataValue::Float(value));
            }
        }
        if let Some(values) = y_pos {
            if let Some(&value) = values.get(plane) {
                metadata.insert(format!("{prefix}.position_y"), MetadataValue::Float(value));
            }
        }
        if let Some(values) = z_pos {
            if let Some(&value) = values.get(plane) {
                metadata.insert(format!("{prefix}.position_z"), MetadataValue::Float(value));
            }
        }
    }

    if let Some(datestamps) = datestamps {
        if let Some(first) = datestamps.first() {
            if let Some(iso) = xrm_datestamp_iso(first) {
                metadata.insert(
                    "xrm.acquisition_datetime_iso8601".into(),
                    MetadataValue::String(iso),
                );
            }
            if let Some(first_ms) = xrm_datestamp_millis(first) {
                for (plane, stamp) in datestamps.iter().enumerate().take(image_count as usize) {
                    if let Some(ms) = xrm_datestamp_millis(stamp) {
                        metadata.insert(
                            format!("plane.{plane}.delta_t"),
                            MetadataValue::Float(ms - first_ms),
                        );
                    }
                }
            }
        }
    }
}

fn xrm_path(prefix: &str, leaf: &str) -> String {
    format!("{prefix}{leaf}")
}

fn insert_f32(
    ole: &mut OleFile,
    path: &str,
    metadata: &mut HashMap<String, MetadataValue>,
    key: &str,
) -> Result<()> {
    metadata.insert(
        key.into(),
        MetadataValue::Float(read_xrm_f32(ole, path)? as f64),
    );
    Ok(())
}

fn insert_formatted_f32(
    ole: &mut OleFile,
    path: &str,
    metadata: &mut HashMap<String, MetadataValue>,
    key: &str,
) -> Result<()> {
    metadata.insert(
        key.into(),
        MetadataValue::String(format_xrm_double(read_xrm_f32(ole, path)? as f64)),
    );
    Ok(())
}

fn insert_string(
    ole: &mut OleFile,
    path: &str,
    metadata: &mut HashMap<String, MetadataValue>,
    key: &str,
) -> Result<()> {
    metadata.insert(
        key.into(),
        MetadataValue::String(read_xrm_string(ole, path)?),
    );
    Ok(())
}

fn insert_yes_no(
    ole: &mut OleFile,
    path: &str,
    metadata: &mut HashMap<String, MetadataValue>,
    key: &str,
) -> Result<()> {
    metadata.insert(
        key.into(),
        MetadataValue::String(read_xrm_yes_no(ole, path)?.into()),
    );
    Ok(())
}

fn add_xrm_metadata_list(
    metadata: &mut HashMap<String, MetadataValue>,
    key: &str,
    values: &[f64],
    format_doubles: bool,
) {
    if values.is_empty() {
        return;
    }
    let single_value = values
        .iter()
        .all(|value| (*value - values[0]).abs() <= f64::EPSILON);
    if single_value {
        add_xrm_metadata_list_value(
            metadata,
            key,
            if format_doubles {
                MetadataValue::String(format_xrm_double(values[0]))
            } else {
                MetadataValue::Float(values[0])
            },
        );
    } else {
        for value in values {
            add_xrm_metadata_list_value(
                metadata,
                key,
                if format_doubles {
                    MetadataValue::String(format_xrm_double(*value))
                } else {
                    MetadataValue::Float(*value)
                },
            );
        }
    }
}

fn add_xrm_metadata_list_value(
    metadata: &mut HashMap<String, MetadataValue>,
    key: &str,
    value: MetadataValue,
) {
    if !metadata.contains_key(key) {
        metadata.insert(key.into(), value);
        return;
    }
    let mut index = 2;
    loop {
        let next_key = format!("{key} #{index}");
        if let std::collections::hash_map::Entry::Vacant(slot) = metadata.entry(next_key) {
            slot.insert(value);
            return;
        }
        index += 1;
    }
}

fn format_xrm_double(value: f64) -> String {
    format!("{value:.02}")
}

fn xrm_datestamp_millis(value: &str) -> Option<f64> {
    let parts = parse_xrm_datestamp(value)?;
    let days = days_from_civil(parts.year, parts.month, parts.day);
    Some(
        (days * 86_400_000
            + parts.hour * 3_600_000
            + parts.minute * 60_000
            + parts.second * 1000
            + parts.millis) as f64,
    )
}

fn xrm_datestamp_iso(value: &str) -> Option<String> {
    let parts = parse_xrm_datestamp(value)?;
    Some(format!(
        "{:04}-{:02}-{:02}T{:02}:{:02}:{:02}.{:03}",
        parts.year, parts.month, parts.day, parts.hour, parts.minute, parts.second, parts.millis
    ))
}

struct XrmDatestampParts {
    year: i64,
    month: i64,
    day: i64,
    hour: i64,
    minute: i64,
    second: i64,
    millis: i64,
}

fn parse_xrm_datestamp(value: &str) -> Option<XrmDatestampParts> {
    let (date, time) = value.split_once(' ')?;
    let mut date_parts = date.split('/');
    let month: i64 = date_parts.next()?.parse().ok()?;
    let day: i64 = date_parts.next()?.parse().ok()?;
    let year: i64 = date_parts.next()?.parse().ok()?;
    let mut time_parts = time.split(':');
    let hour: i64 = time_parts.next()?.parse().ok()?;
    let minute: i64 = time_parts.next()?.parse().ok()?;
    let seconds = time_parts.next()?;
    let (second, millis) = seconds
        .split_once('.')
        .map_or((seconds, "0"), |(s, ms)| (s, ms));
    let second: i64 = second.parse().ok()?;
    let millis: i64 = millis.get(..3.min(millis.len()))?.parse().ok()?;
    if !(1..=12).contains(&month)
        || !(1..=31).contains(&day)
        || !(0..=23).contains(&hour)
        || !(0..=59).contains(&minute)
        || !(0..=60).contains(&second)
    {
        return None;
    }
    Some(XrmDatestampParts {
        year,
        month,
        day,
        hour,
        minute,
        second,
        millis,
    })
}

fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let yoe = year - era * 400;
    let mp = month + if month > 2 { -3 } else { 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn xrm_pixel_type(data_type: i32) -> Result<(PixelType, &'static str)> {
    match data_type {
        2 => Ok((PixelType::Int8, "byte")),
        3 => Ok((PixelType::Uint8, "ubyte")),
        4 => Ok((PixelType::Int16, "short")),
        5 => Ok((PixelType::Uint16, "ushort")),
        6 => Ok((PixelType::Int32, "int")),
        7 => Ok((PixelType::Uint32, "uint")),
        10 => Ok((PixelType::Float32, "float")),
        11 => Ok((PixelType::Float64, "double")),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "Zeiss XRM/TXRM unsupported data type: {other}"
        ))),
    }
}

fn xrm_image_index(path: &str) -> Option<u32> {
    let tail = path.rsplit('/').next()?;
    let digits = tail.strip_prefix("Image")?;
    digits.parse().ok()
}

fn xrm_flip_rows(raw: &[u8], meta: &ImageMetadata) -> Result<Vec<u8>> {
    let row_len = meta
        .size_x
        .checked_mul(meta.pixel_type.bytes_per_sample() as u32)
        .ok_or_else(|| BioFormatsError::Format("XRM row size overflows".into()))?
        as usize;
    let expected = row_len
        .checked_mul(meta.size_y as usize)
        .ok_or_else(|| BioFormatsError::Format("XRM plane size overflows".into()))?;
    let mut out = vec![0; expected];
    for row in (0..meta.size_y as usize).rev() {
        let start = row * row_len;
        let dest = (meta.size_y as usize - 1 - row) * row_len;
        if start >= raw.len() {
            continue;
        }
        let available = (raw.len() - start).min(row_len);
        out[dest..dest + available].copy_from_slice(&raw[start..start + available]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::reader::FormatReader;
    use std::io::Write;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_xrm_{nanos}_{name}"))
    }

    fn write_stream(comp: &mut cfb::CompoundFile<std::fs::File>, path: &str, data: &[u8]) {
        if let Some(parent) = Path::new(path).parent() {
            comp.create_storage_all(parent).unwrap();
        }
        comp.create_stream(path).unwrap().write_all(data).unwrap();
    }

    fn write_i32_stream(comp: &mut cfb::CompoundFile<std::fs::File>, path: &str, value: i32) {
        write_stream(comp, path, &value.to_le_bytes());
    }

    fn write_f32_stream(comp: &mut cfb::CompoundFile<std::fs::File>, path: &str, value: f32) {
        write_stream(comp, path, &value.to_le_bytes());
    }

    fn write_f32_array(comp: &mut cfb::CompoundFile<std::fs::File>, path: &str, values: &[f32]) {
        let mut data = Vec::with_capacity(values.len() * 4);
        for value in values {
            data.extend_from_slice(&value.to_le_bytes());
        }
        write_stream(comp, path, &data);
    }

    fn write_date_array(comp: &mut cfb::CompoundFile<std::fs::File>, path: &str, values: &[&str]) {
        let mut data = Vec::with_capacity(values.len() * 40);
        for value in values {
            let mut chunk = [0u8; 40];
            let bytes = value.as_bytes();
            chunk[..bytes.len().min(40)].copy_from_slice(&bytes[..bytes.len().min(40)]);
            data.extend_from_slice(&chunk);
        }
        write_stream(comp, path, &data);
    }

    fn assert_close(actual: Option<f64>, expected: f64) {
        let actual = actual.expect("value");
        assert!(
            (actual - expected).abs() < 1e-6,
            "actual {actual} expected {expected}"
        );
    }

    #[test]
    fn xrm_byte_detection_matches_java_ole2_magic() {
        let reader = ZeissXrmReader::new();
        assert!(reader.is_this_type_by_bytes(OLE2_MAGIC));
        assert!(reader
            .is_this_type_by_bytes(&[0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1, 0, 1, 2, 3]));
        assert!(!reader.is_this_type_by_bytes(&OLE2_MAGIC[..7]));
        assert!(!reader.is_this_type_by_bytes(b"not a compound document"));
    }

    #[test]
    fn xrm_name_detection_matches_java_suffixes() {
        let reader = ZeissXrmReader::new();
        assert!(reader.is_this_type_by_name(Path::new("sample.txm")));
        assert!(reader.is_this_type_by_name(Path::new("sample.txrm")));
        assert!(reader.is_this_type_by_name(Path::new("sample.TXRM")));
        assert!(!reader.is_this_type_by_name(Path::new("sample.xrm")));
    }

    #[test]
    fn xrm_reads_cfb_imageinfo_and_flipped_image_planes() {
        let path = temp_path("synthetic.txrm");
        {
            let mut comp = cfb::create(&path).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", 3);
            write_i32_stream(&mut comp, "/ImageInfo/ImageHeight", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 3);
            write_stream(&mut comp, "/ImageInfo/FileType", b"txrm\0");
            write_stream(&mut comp, "/ImageData/Image2", &[21, 22, 23, 24, 25, 26]);
            write_stream(&mut comp, "/ImageData/Image1", &[1, 2, 3, 4, 5, 6]);
        }

        let mut reader = ZeissXrmReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        assert_eq!(meta.size_x, 3);
        assert_eq!(meta.size_y, 2);
        assert_eq!(meta.size_z, 2);
        assert_eq!(meta.pixel_type, PixelType::Uint8);
        assert_eq!(meta.image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![4, 5, 6, 1, 2, 3]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![24, 25, 26, 21, 22, 23]);
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
            vec![5, 6, 2, 3]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn xrm_captures_named_global_metadata_keys() {
        // .txrm: paramsPrefix == "Projection Info: ", no "Output data type".
        let txrm = temp_path("named_meta.txrm");
        {
            let mut comp = cfb::create(&txrm).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", 2);
            write_i32_stream(&mut comp, "/ImageInfo/ImageHeight", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 5);
            write_i32_stream(&mut comp, "/ImageInfo/AcquisitionMode", 0);
            write_stream(&mut comp, "/ImageInfo/PixelSize", &1.25f32.to_le_bytes());
            write_stream(&mut comp, "/ImageInfo/SourceFilterName", b"LE1\0");
            write_stream(&mut comp, "/ImageInfo/Voltage", &40.0f32.to_le_bytes());
            write_stream(&mut comp, "/exeVersion", b"1.2.3\0");
            write_stream(&mut comp, "/DetAssemblyInfo/LensInfo/LensName", b"20X\0");
            write_i32_stream(&mut comp, "/ImageInfo/CameraNumberOfFramesPerImage", 4);
            write_i32_stream(&mut comp, "/ImageInfo/NoOfImagesAveraged", 3);
            write_i32_stream(&mut comp, "/ImageInfo/CameraBinning", 2);
            write_stream(&mut comp, "/ImageData/Image1", &[0u8; 8]);
        }

        let mut reader = ZeissXrmReader::new();
        reader.set_id(&txrm).unwrap();
        let md = &reader.metadata().series_metadata;
        assert_eq!(
            md.get("Image Details: Pixel size (µm)")
                .map(|v| v.to_string()),
            Some("1.25".to_string())
        );
        assert_eq!(
            md.get("Image Details: Acquisition mode")
                .map(|v| v.to_string()),
            Some("Tomography".to_string())
        );
        assert_eq!(
            md.get("Source Assembly Info: Source Filter Name")
                .map(|v| v.to_string()),
            Some("LE1".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Source filter name")
                .map(|v| v.to_string()),
            Some("LE1".to_string())
        );
        assert!(md.contains_key("Source Assembly Info: Voltage (kV)"));
        assert_eq!(
            md.get("Dataset Info: Executable version")
                .map(|v| v.to_string()),
            Some("1.2.3".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Objective name")
                .map(|v| v.to_string()),
            Some("20X".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Frames per image")
                .map(|v| v.to_string()),
            Some("4".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Images per projection")
                .map(|v| v.to_string()),
            Some("3".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Camera binning")
                .map(|v| v.to_string()),
            Some("2".to_string())
        );
        // TXRM must NOT carry the TXM-only "Output data type" key.
        assert!(!md.contains_key("Reconstruction Settings: Output data type"));
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].physical_size_x, Some(1.25));
        assert_eq!(ome.images[0].physical_size_y, Some(1.25));
        assert_eq!(ome.images[0].physical_size_z, Some(1.25));
        let _ = std::fs::remove_file(txrm);

        // .txm: emits "Output data type" and uses "General Parameters: " prefix.
        let txm = temp_path("named_meta.txm");
        {
            let mut comp = cfb::create(&txm).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", 2);
            write_i32_stream(&mut comp, "/ImageInfo/ImageHeight", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 5);
            write_i32_stream(&mut comp, "/ImageInfo/CameraBinning", 2);
            write_stream(&mut comp, "/ImageData/Image1", &[0u8; 8]);
        }
        let mut reader = ZeissXrmReader::new();
        reader.set_id(&txm).unwrap();
        let md = &reader.metadata().series_metadata;
        assert_eq!(
            md.get("Reconstruction Settings: Output data type")
                .map(|v| v.to_string()),
            Some("ushort".to_string())
        );
        assert_eq!(
            md.get("General Parameters: Camera binning")
                .map(|v| v.to_string()),
            Some("2".to_string())
        );
        let _ = std::fs::remove_file(txm);
    }

    #[test]
    fn txrm_projects_java_plane_and_modulo_metadata() {
        let path = temp_path("plane_meta.txrm");
        {
            let mut comp = cfb::create(&path).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", 2);
            write_i32_stream(&mut comp, "/ImageInfo/ImageHeight", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 3);
            write_f32_stream(&mut comp, "/ImageInfo/PixelSize", 0.5);
            write_f32_array(&mut comp, "/ImageInfo/ExpTimes", &[0.1, 0.2]);
            write_f32_array(&mut comp, "/ImageInfo/XrayCurrent", &[100.0, 200.0]);
            write_f32_array(&mut comp, "/ImageInfo/XrayVoltage", &[40.0, 50.0]);
            write_f32_array(&mut comp, "/ImageInfo/XPosition", &[1.0, 2.0]);
            write_f32_array(&mut comp, "/ImageInfo/YPosition", &[3.0, 4.0]);
            write_f32_array(&mut comp, "/ImageInfo/ZPosition", &[5.0, 6.0]);
            write_f32_array(&mut comp, "/ImageInfo/Angles", &[0.0, 90.0]);
            write_f32_array(&mut comp, "/ImageInfo/FanAngle", &[1.25, 1.25]);
            write_i32_stream(&mut comp, "/ImageInfo/ReadOutTime", 7);
            write_i32_stream(&mut comp, "/ImageInfo/Temperature", -12);
            write_date_array(
                &mut comp,
                "/ImageInfo/Date",
                &["01/02/2024 03:04:05.006", "01/02/2024 03:04:06.256"],
            );
            write_stream(&mut comp, "/Selection/SelectedImages", &[1]);
            write_stream(&mut comp, "/ImageData/Image1", &[1, 2, 3, 4]);
            write_stream(&mut comp, "/ImageData/Image2", &[5, 6, 7, 8]);
        }

        let mut reader = ZeissXrmReader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();
        let md = &meta.series_metadata;
        assert_eq!(
            md.get("Projection Info: X-ray current (µA)")
                .map(|v| v.to_string()),
            Some("100.00".to_string())
        );
        assert_eq!(
            md.get("Projection Info: X-ray current (µA) #2")
                .map(|v| v.to_string()),
            Some("200.00".to_string())
        );
        assert_eq!(
            md.get("Projection Info: X-ray power (W)")
                .map(|v| v.to_string()),
            Some("4.00".to_string())
        );
        assert_eq!(
            md.get("Projection Info: X-ray power (W) #2")
                .map(|v| v.to_string()),
            Some("10.00".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Fan angle").map(|v| v.to_string()),
            Some("1.25".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Angle #2").map(|v| v.to_string()),
            Some("90.00".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Selected").map(|v| v.to_string()),
            Some("Yes".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Camera Readout Speed")
                .map(|v| v.to_string()),
            Some("7".to_string())
        );
        assert_eq!(
            md.get("Projection Info: Camera Temperature")
                .map(|v| v.to_string()),
            Some("-12".to_string())
        );
        assert_eq!(
            md.get("Image Details: Field of view (µm)")
                .map(|v| v.to_string()),
            Some("1.00, 1.00".to_string())
        );

        let modulo = meta.modulo_z.as_ref().expect("modulo Z");
        assert_eq!(modulo.modulo_type, "rotation");
        assert_eq!(modulo.start, 0.0);
        assert_eq!(modulo.step, 1.0);
        assert_eq!(modulo.end, 1.0);

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(
            ome.images[0].acquisition_date.as_deref(),
            Some("2024-01-02T03:04:05.006")
        );
        assert_eq!(ome.images[0].physical_size_x, Some(0.5));
        assert_eq!(ome.images[0].planes.len(), 2);
        assert_close(ome.images[0].planes[0].exposure_time, 0.1);
        assert_eq!(ome.images[0].planes[0].position_x, Some(1.0));
        assert_eq!(ome.images[0].planes[0].position_y, Some(3.0));
        assert_eq!(ome.images[0].planes[0].position_z, Some(5.0));
        assert_eq!(ome.images[0].planes[0].delta_t, Some(0.0));
        assert_close(ome.images[0].planes[1].exposure_time, 0.2);
        assert_eq!(ome.images[0].planes[1].position_x, Some(2.0));
        assert_eq!(ome.images[0].planes[1].position_y, Some(4.0));
        assert_eq!(ome.images[0].planes[1].position_z, Some(6.0));
        assert_eq!(ome.images[0].planes[1].delta_t, Some(1250.0));
        assert_eq!(
            ome.images[0]
                .modulo_z
                .as_ref()
                .map(|m| m.modulo_type.as_str()),
            Some("rotation")
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn txm_captures_java_reconstruction_metadata() {
        let path = temp_path("recon_meta.txm");
        {
            let mut comp = cfb::create(&path).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", 2);
            write_i32_stream(&mut comp, "/ImageInfo/ImageHeight", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 5);
            write_f32_array(&mut comp, "/ImageInfo/Current", &[100.0]);
            write_f32_array(&mut comp, "/ReconSettings/SourceVoltage", &[40.0]);
            write_stream(
                &mut comp,
                "/ReconSettings/OutputFileLocation",
                b"/tmp/out\0",
            );
            write_stream(&mut comp, "/ReconSettings/InputFileName", b"in.txrm\0");
            write_f32_stream(&mut comp, "/ReconSettings/CenterShift", 1.5);
            write_stream(
                &mut comp,
                "/ReconSettings/BeamHardeningFileName",
                b"beam.dat\0",
            );
            write_f32_stream(&mut comp, "/ReconSettings/BeamHardening", 0.25);
            write_f32_stream(&mut comp, "/ReconSettings/RotationAngle", 180.0);
            write_i32_stream(&mut comp, "/ReconSettings/ReconFilterChoice", 2);
            write_f32_stream(&mut comp, "/ReconSettings/ReconFilterSmoothFactor", 0.75);
            write_i32_stream(&mut comp, "/ReconSettings/ReconScalingEnum", 0);
            write_f32_stream(&mut comp, "/ReconSettings/GlobalMax", 9.0);
            write_f32_stream(&mut comp, "/ReconSettings/GlobalMin", 1.0);
            write_stream(&mut comp, "/ReconSettings/UserMinMax", &[1]);
            write_stream(&mut comp, "/ReconSettings/UseCTScaleFilter", &[0]);
            write_stream(&mut comp, "/ReconSettings/CTScaleFilter", b"scale\0");
            write_stream(
                &mut comp,
                "/ReconSettings/SecondaryReferenceFileName",
                b"secondary.txm\0",
            );
            write_stream(&mut comp, "/ReconSettings/SecRefSourceFilterName", b"LE2\0");
            write_i32_stream(&mut comp, "/ReconSettings/SecondaryRefCollectionMode", 0);
            write_i32_stream(&mut comp, "/ReconSettings/ReconOperation", 2);
            write_stream(&mut comp, "/ReconSettings/ReconServiceVersion", b"8.1\0");
            write_f32_stream(&mut comp, "/AutoRecon/MeanSampleX", 10.0);
            write_f32_stream(&mut comp, "/AutoRecon/StoRADistance", -5000.0);
            write_f32_stream(&mut comp, "/AutoRecon/DtoRADistance", 6000.0);
            write_i32_stream(&mut comp, "/AutoRecon/NumOfProjects", 720);
            write_stream(&mut comp, "/ImageData/Image1", &[0u8; 8]);
        }

        let mut reader = ZeissXrmReader::new();
        reader.set_id(&path).unwrap();
        let md = &reader.metadata().series_metadata;
        assert_eq!(
            md.get("Reconstruction Settings: Output file-format")
                .map(|v| v.to_string()),
            Some("txm".to_string())
        );
        assert_eq!(
            md.get("General Parameters: X-ray power (W)")
                .map(|v| v.to_string()),
            Some("4".to_string())
        );
        assert_eq!(
            md.get("Reconstruction Settings: Recon filter")
                .map(|v| v.to_string()),
            Some("Smooth".to_string())
        );
        assert_eq!(
            md.get("Reconstruction Settings: Recon scaling")
                .map(|v| v.to_string()),
            Some("Global".to_string())
        );
        assert_eq!(
            md.get("Reconstruction Settings: User min-max")
                .map(|v| v.to_string()),
            Some("Yes".to_string())
        );
        assert_eq!(
            md.get("Reconstruction Settings: Use CT-Scaling")
                .map(|v| v.to_string()),
            Some("No".to_string())
        );
        assert_eq!(
            md.get("General Parameters: Source-RA (mm)")
                .map(|v| v.to_string()),
            Some("5.00".to_string())
        );
        assert_eq!(
            md.get("General Parameters: Detector-RA (mm)")
                .map(|v| v.to_string()),
            Some("6.00".to_string())
        );
        assert_eq!(
            md.get("Dataset Info: Recon Service Version")
                .map(|v| v.to_string()),
            Some("8.1".to_string())
        );
        assert_eq!(
            md.get("General Parameters: Number of projections used")
                .map(|v| v.to_string()),
            Some("720".to_string())
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn xrm_short_image_stream_zero_fills_like_java() {
        let path = temp_path("short_stream.txrm");
        {
            let mut comp = cfb::create(&path).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", 2);
            write_i32_stream(&mut comp, "/ImageInfo/ImageHeight", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 3);
            write_stream(&mut comp, "/ImageData/Image1", &[1, 2, 3]);
        }

        let mut reader = ZeissXrmReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![3, 0, 1, 2]);
        assert_eq!(
            reader.open_bytes_region(0, 0, 0, 2, 2).unwrap(),
            vec![3, 0, 1, 2]
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn xrm_preserves_one_based_image_stream_slots_like_java() {
        let path = temp_path("missing_image1.txrm");
        {
            let mut comp = cfb::create(&path).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", 2);
            write_i32_stream(&mut comp, "/ImageInfo/ImageHeight", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 3);
            write_stream(&mut comp, "/ImageData/Image2", &[1, 2, 3, 4]);
        }

        let mut reader = ZeissXrmReader::new();
        reader.set_id(&path).unwrap();
        assert_eq!(reader.metadata().size_z, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert!(reader.open_bytes(0).is_err());
        assert_eq!(reader.open_bytes(1).unwrap(), vec![3, 4, 1, 2]);

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn xrm_rejects_missing_required_imageinfo() {
        let path = temp_path("missing.txm");
        {
            let mut comp = cfb::create(&path).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 5);
            write_stream(&mut comp, "/ImageData/Image1", &[0; 8]);
        }

        let err = ZeissXrmReader::new().set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("ImageHeight")),
            "{err:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn xrm_rejects_non_positive_dimensions_before_casting() {
        let path = temp_path("negative_width.txm");
        {
            let mut comp = cfb::create(&path).unwrap();
            write_i32_stream(&mut comp, "/ImageInfo/ImageWidth", -2);
            write_i32_stream(&mut comp, "/ImageInfo/ImageHeight", 2);
            write_i32_stream(&mut comp, "/ImageInfo/DataType", 3);
            write_stream(&mut comp, "/ImageData/Image1", &[0; 4]);
        }

        let err = ZeissXrmReader::new().set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("non-positive")),
            "{err:?}"
        );
        let _ = std::fs::remove_file(path);
    }
}
