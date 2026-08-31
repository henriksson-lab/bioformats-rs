//! MicroManager format reader (open-source microscopy platform).
//!
//! MicroManager saves data as:
//!   - `metadata.txt` (or `*_metadata.txt`) — JSON with image dimensions
//!   - TIFF files (`MMStack_*.tif`, `img_*.tif`, etc.) — the actual pixel data
//!
//! Detection: file named `*_metadata.txt` or `metadata.txt`.
//! The JSON `Summary` block contains Width, Height, Channels, Slices, Frames,
//! PixelType, plus per-frame `FrameKey-<t>-<c>-<z>` blocks that map each plane
//! coordinate to a TIFF file.
//!
//! This follows the Java `MicromanagerReader`:
//!   - Each stage `Pos_*` sibling directory is a separate series.
//!   - Each plane index is mapped through a (z,c,t) -> filename map, falling
//!     back to a sorted TIFF list keyed by raster order.
//!   - Each TIFF may hold multiple pages; the inner page index is
//!     `plane % tiffReader.getImageCount()`.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ome_metadata::{create_lsid, OmeMetadata, OmePlane};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::tiff::ifd::{Ifd, IfdValue};
use crate::tiff::TiffReader;

const JSON_TAG: u16 = 50839;
const MM_JSON_TAG: u16 = 51123;
const ACQUISITION_XML: &str = "Acqusition.xml";

// ── Minimal JSON key extractor ────────────────────────────────────────────────
/// Extract the integer value of a JSON key, e.g. `"Width": 512` or `"Width":512`.
fn json_int(json: &str, key: &str) -> Option<i64> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').map(str::trim_start).unwrap_or(rest);
    let end = rest
        .find(|c: char| !c.is_ascii_digit() && c != '-')
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_str(json: &str, key: &str) -> Option<String> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').map(str::trim_start).unwrap_or(rest);
    let rest = rest.strip_prefix('"')?;
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// Extract a JSON array of strings (or numbers), e.g. `"ChNames": ["a","b"]`.
/// Returns the trimmed, unquoted elements. Mirrors Java `value.split(",")`.
fn json_str_array(json: &str, key: &str) -> Option<Vec<String>> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').map(str::trim_start).unwrap_or(rest);
    let rest = rest.strip_prefix('[')?;
    let end = rest.find(']')?;
    let body = &rest[..end];
    let items: Vec<String> = body
        .split(',')
        .map(|s| s.trim().trim_matches('"').trim().to_string())
        .filter(|s| !s.is_empty())
        .collect();
    if items.is_empty() {
        None
    } else {
        Some(items)
    }
}

fn json_float(json: &str, key: &str) -> Option<f64> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').map(str::trim_start).unwrap_or(rest);
    let rest = rest.trim_start_matches('"');
    let end = rest
        .find(|c: char| {
            !c.is_ascii_digit() && c != '-' && c != '.' && c != 'e' && c != 'E' && c != '+'
        })
        .unwrap_or(rest.len());
    rest[..end].parse().ok()
}

fn json_bool(json: &str, key: &str) -> Option<bool> {
    let pattern = format!("\"{}\"", key);
    let idx = json.find(&pattern)?;
    let rest = &json[idx + pattern.len()..];
    let rest = rest.trim_start();
    let rest = rest.strip_prefix(':').map(str::trim_start).unwrap_or(rest);
    if rest.starts_with("true") {
        Some(true)
    } else if rest.starts_with("false") {
        Some(false)
    } else {
        None
    }
}

fn positive_u32_from_json(json: &str, key: &str) -> Result<u32> {
    let value = json_int(json, key)
        .ok_or_else(|| BioFormatsError::Format(format!("MicroManager: missing {key}")))?;
    u32::try_from(value)
        .ok()
        .filter(|&v| v > 0)
        .ok_or_else(|| BioFormatsError::Format(format!("MicroManager: invalid {key} {value}")))
}

fn optional_positive_u32_from_json(json: &str, key: &str, default: u32) -> Result<u32> {
    match json_int(json, key) {
        Some(value) => u32::try_from(value)
            .ok()
            .filter(|&v| v > 0)
            .ok_or_else(|| BioFormatsError::Format(format!("MicroManager: invalid {key} {value}"))),
        None => Ok(default),
    }
}

fn pixel_type_from_str(s: &str) -> Result<PixelType> {
    match s.to_uppercase().as_str() {
        "GRAY8" | "RGB8" => Ok(PixelType::Uint8),
        "GRAY16" | "RGB16" => Ok(PixelType::Uint16),
        // Micro-Manager GRAY32 is 32-bit unsigned int, not float. The actual
        // pixel type is overridden from the first TIFF IFD; this matches the
        // declared 32-bit unsigned-integer mapping.
        "GRAY32" | "RGB32" => Ok(PixelType::Uint32),
        other => Err(BioFormatsError::UnsupportedFormat(format!(
            "MicroManager: unsupported PixelType {other}"
        ))),
    }
}

/// A (z, c, t) coordinate key used in the per-plane file map.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
struct Index {
    z: u32,
    c: u32,
    t: u32,
}

/// Per-position (per-series) parsed state, mirroring Java's `Position`.
struct Position {
    #[allow(dead_code)]
    metadata_file: PathBuf,
    meta: ImageMetadata,
    /// Map (z,c,t) -> absolute TIFF path.
    file_name_map: HashMap<Index, PathBuf>,
    /// Sorted fallback list of TIFF files (raster order).
    tiffs: Vec<PathBuf>,

    // Per-plane / camera metadata, mirroring Java `Position` (~1226-1237).
    /// Exposure time in milliseconds (Java `exposureTime`, `Exposure-ms`).
    exposure_time: Option<f64>,
    /// Sorted elapsed timestamps in milliseconds (Java `timestamps`,
    /// `ElapsedTime-ms`).
    timestamps: Vec<f64>,
    /// Per-plane stage positions `[plane][x,y,z]` in micrometres (Java
    /// `positions`, populated by `parseKeyAndValue`).
    positions: Vec<[Option<f64>; 3]>,
    /// Detector binning string, e.g. "2x2" (Java `binning`).
    binning: Option<String>,
    /// Detector gain (Java `gain`).
    gain: Option<i32>,
    /// Detector serial number / id (Java `detectorID`, from `<camera>-CameraID`;
    /// Java uses it as the detector serial number). Retained for fidelity; the
    /// structured OME model has no serial-number slot to project it into.
    #[allow(dead_code)]
    detector_id: Option<String>,
    /// Detector model (Java `detectorModel`, from `<camera>-CameraName`).
    detector_model: Option<String>,
    /// Detector manufacturer (Java `detectorManufacturer`, from `<camera>-Name`).
    detector_manufacturer: Option<String>,
    /// Imaging environment temperature in Celsius (Java `temperature`).
    temperature: Option<f64>,
    /// Per-channel detector voltages in volts (Java `voltage`, `DAC-*-Volts`).
    voltage: Vec<f64>,
    /// Reference camera name (Java `cameraRef`, from `Core-Camera`). A parsing
    /// intermediate; Java keeps it on `Position` but does not emit it directly.
    #[allow(dead_code)]
    camera_ref: Option<String>,
    /// Camera mode / detector type hint (Java `cameraMode`, `<camera>-CCDMode`).
    camera_mode: Option<String>,
}

impl Position {
    fn samples_per_pixel(&self) -> usize {
        if self.meta.is_rgb {
            self.meta.size_c.max(1) as usize
        } else {
            1
        }
    }

    fn plane_byte_count(&self, w: u32, h: u32) -> Result<usize> {
        (w as usize)
            .checked_mul(h as usize)
            .and_then(|px| px.checked_mul(self.samples_per_pixel()))
            .and_then(|px| px.checked_mul(self.meta.pixel_type.bytes_per_sample()))
            .ok_or_else(|| {
                BioFormatsError::Format("MicroManager plane byte count overflows".into())
            })
    }

    /// Convert a 1D raster plane index to (z, c, t) for the given dimension order.
    fn zct_coords(order: DimensionOrder, z: u32, c: u32, t: u32, no: u32) -> (u32, u32, u32) {
        let z = z.max(1);
        let c = c.max(1);
        let t = t.max(1);
        let dims: &[(char, u32)] = match order {
            DimensionOrder::XYCTZ => &[('C', c), ('T', t), ('Z', z)],
            DimensionOrder::XYCZT => &[('C', c), ('Z', z), ('T', t)],
            DimensionOrder::XYTCZ => &[('T', t), ('C', c), ('Z', z)],
            DimensionOrder::XYTZC => &[('T', t), ('Z', z), ('C', c)],
            DimensionOrder::XYZCT => &[('Z', z), ('C', c), ('T', t)],
            DimensionOrder::XYZTC => &[('Z', z), ('T', t), ('C', c)],
        };
        let mut remaining = no;
        let (mut zz, mut cc, mut tt) = (0u32, 0u32, 0u32);
        for (dim, len) in dims {
            let len = (*len).max(1);
            let value = remaining % len;
            remaining /= len;
            match dim {
                'Z' => zz = value,
                'C' => cc = value,
                'T' => tt = value,
                _ => {}
            }
        }
        (zz, cc, tt)
    }

    /// Resolve the TIFF file for a given plane index, mirroring Java `getFile`.
    fn file_for_plane(&self, no: u32) -> Option<PathBuf> {
        let m = &self.meta;
        let (z, c, t) = Self::zct_coords(m.dimension_order, m.size_z, m.size_c, m.size_t, no);
        let key = Index { z, c, t };
        if let Some(p) = self.file_name_map.get(&key) {
            return Some(p.clone());
        }
        // Java Position.getFile (~1260): only fall back to the raster list when
        // the FrameKey map is EMPTY; if the map is populated but lacks this
        // coordinate, return null (None here) rather than a raster fallback.
        if self.file_name_map.is_empty() {
            return self.tiffs.get(no as usize).cloned();
        }
        None
    }

    /// Project this position's parsed per-plane and camera/detector metadata
    /// into structured OME metadata, mirroring Java
    /// `MicromanagerReader.populateMetadataStore` / `populateMetadata` (~340-444)
    /// for a single series.
    fn populate_metadata_store(&self, series: usize) -> OmeMetadata {
        // Start from the generic projection (pixel sizes, channel names/colours,
        // image name/description, acquisition date) already encoded in
        // `series_metadata`, then overlay the MicroManager-specific per-plane and
        // detector fields.
        let mut ome = OmeMetadata::from_image_metadata(&self.meta);

        let image_count = self.meta.image_count as usize;
        let c_size = self.meta.size_c.max(1) as usize;
        let z_size = self.meta.size_z.max(1) as usize;
        let channel_count = self.meta.size_c.max(1) as usize;

        // Per-plane exposure / deltaT / stage position (Java ~390-411).
        let mut planes: Vec<OmePlane> = Vec::with_capacity(image_count);
        let mut next_stamp = 0usize;
        for q in 0..image_count {
            let mut plane = OmePlane {
                the_z: ((q / c_size) % z_size) as u32,
                the_c: (q % c_size) as u32,
                the_t: (q / (c_size * z_size)) as u32,
                ..Default::default()
            };
            plane.exposure_time = self.exposure_time;
            // deltaT only when the plane's TIFF exists and a stamp remains.
            if let Some(file) = self.file_for_plane(q as u32) {
                if file.exists() && next_stamp < self.timestamps.len() {
                    plane.delta_t = Some(self.timestamps[next_stamp]);
                    next_stamp += 1;
                }
            }
            if let Some(p) = self.positions.get(q) {
                plane.position_x = p[0];
                plane.position_y = p[1];
                plane.position_z = p[2];
            }
            planes.push(plane);
        }

        let detector_id = create_lsid("Detector", &[0, series]);

        if let Some(image) = ome.images.get_mut(0) {
            if image.name.is_none() {
                if let Some(MetadataValue::String(name)) =
                    self.meta.series_metadata.get("image_name")
                {
                    image.name = Some(name.clone());
                }
            }
            if image.physical_size_x.is_none() {
                if let Some(MetadataValue::Float(value)) =
                    self.meta.series_metadata.get("physicalSizeX")
                {
                    image.physical_size_x = Some(*value);
                }
            }
            if image.physical_size_y.is_none() {
                if let Some(MetadataValue::Float(value)) =
                    self.meta.series_metadata.get("physicalSizeY")
                {
                    image.physical_size_y = Some(*value);
                }
            }
            if image.time_increment.is_none() {
                if let Some(MetadataValue::Float(value)) =
                    self.meta.series_metadata.get("timeIncrement")
                {
                    image.time_increment = Some(*value);
                }
            }
            image.planes = planes;
            image.imaging_environment_temperature = self.temperature;

            // DetectorSettings per channel (Java ~416-424).
            for c in 0..channel_count {
                if image.channels.len() <= c {
                    break;
                }
                let ch = &mut image.channels[c];
                if ch.name.is_none() {
                    if let Some(MetadataValue::String(name)) =
                        self.meta.series_metadata.get(&format!("channel_name[{c}]"))
                    {
                        ch.name = Some(name.clone());
                    }
                }
                ch.detector_settings_binning = self.binning.clone();
                ch.detector_settings_gain = self.gain.map(|g| g as f64);
                if let Some(v) = self.voltage.get(c) {
                    ch.detector_settings_voltage = Some(*v);
                }
                ch.detector_ref = Some(detector_id.clone());
            }
        }

        // Detector element (Java ~426-440). `cameraMode` defaults to "Other".
        let detector = crate::common::ome_metadata::OmeDetector {
            id: Some(detector_id),
            model: self.detector_model.clone(),
            manufacturer: self.detector_manufacturer.clone(),
            detector_type: Some(
                self.camera_mode
                    .clone()
                    .unwrap_or_else(|| "Other".to_string()),
            ),
            ..Default::default()
        };
        let instrument_index = if ome.instruments.is_empty() {
            ome.instruments
                .push(crate::common::ome_metadata::OmeInstrument {
                    id: Some(create_lsid("Instrument", &[0])),
                    detectors: vec![detector],
                    ..Default::default()
                });
            0
        } else {
            ome.instruments[0].detectors.push(detector);
            0
        };
        if let Some(image) = ome.images.get_mut(0) {
            image.instrument_ref = Some(instrument_index);
        }

        ome
    }
}

/// Parse a metadata.txt JSON file for a single position.
fn parse_position(meta_path: &Path) -> Result<Position> {
    let f = File::open(meta_path).map_err(BioFormatsError::Io)?;
    let mut json = String::new();
    BufReader::new(f)
        .read_to_string(&mut json)
        .map_err(BioFormatsError::Io)?;

    // Summary block: dimensions
    let summary_start = json.find("\"Summary\"").unwrap_or(0);
    let summary = &json[summary_start..];

    let width = positive_u32_from_json(summary, "Width")?;
    let height = positive_u32_from_json(summary, "Height")?;
    let channels = optional_positive_u32_from_json(summary, "Channels", 1)?;
    let slices = optional_positive_u32_from_json(summary, "Slices", 1)?;
    let frames = optional_positive_u32_from_json(summary, "Frames", 1)?;
    let pixel_type_str = json_str(summary, "PixelType").unwrap_or_else(|| "GRAY16".into());
    let mut pixel_type = pixel_type_from_str(&pixel_type_str)?;
    let mut bits = match json_int(summary, "BitDepth") {
        Some(value) => u16::try_from(value)
            .ok()
            .filter(|&v| v > 0)
            .ok_or_else(|| {
                BioFormatsError::Format(format!("MicroManager: invalid BitDepth {value}"))
            })?,
        None => pixel_type.bytes_per_sample() as u16 * 8,
    };
    let is_rgb_summary = pixel_type_str.starts_with("RGB");

    // Dimension order from "SlicesFirst": false -> XYCZT, else XYZCT (Java default).
    let dimension_order = match json_str(&json, "SlicesFirst")
        .or_else(|| json_int(&json, "SlicesFirst").map(|v| v.to_string()))
        .or_else(|| json_bool(&json, "SlicesFirst").map(|v| v.to_string()))
    {
        Some(ref v) if v.eq_ignore_ascii_case("false") || v == "0" => DimensionOrder::XYCZT,
        _ => DimensionOrder::XYZCT,
    };

    let dir = meta_path.parent().unwrap_or_else(|| Path::new("."));

    // Build the per-plane file name map and per-plane/camera metadata from the
    // "FrameKey-<t>-<c>-<z>" and "Coords-<file>" blocks.
    let frame_block_count = json.matches("\"FrameKey-").count();
    let coords_block_count = json.matches("\"Coords-").count();
    let digits = frame_block_count
        .max(coords_block_count)
        .saturating_sub(1)
        .to_string()
        .len();
    let mut frame = FrameData::default();
    parse_frame_keys(&json, dir, &mut frame, digits);
    parse_coords_blocks(&json, dir, &mut frame);
    let file_name_map = std::mem::take(&mut frame.file_name_map);

    // Fallback: sorted list of all TIFF files in the directory.
    let mut tiffs: Vec<PathBuf> = std::fs::read_dir(dir)
        .map(|rd| {
            rd.filter_map(|e| e.ok())
                .map(|e| e.path())
                .filter(|p| {
                    p.extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
                        .unwrap_or(false)
                })
                .collect()
        })
        .unwrap_or_default();
    tiffs.sort();

    parse_tiff_private_metadata(
        &tiffs,
        &file_name_map,
        dimension_order,
        slices,
        channels,
        frames,
        &mut frame,
        digits,
    );

    // Derive endianness/pixel type from the first available TIFF (Java reads the
    // first IFD: littleEndian and pixelType come from the IFD, not the JSON).
    let mut is_little_endian = true;
    let probe = file_name_map
        .values()
        .next()
        .cloned()
        .or_else(|| tiffs.first().cloned());
    if let Some(probe_path) = probe {
        let mut r = TiffReader::new();
        if r.set_id(&probe_path).is_ok() {
            let tm = r.metadata();
            is_little_endian = tm.is_little_endian;
            pixel_type = tm.pixel_type;
            if tm.bits_per_pixel > 0 {
                bits = tm.bits_per_pixel;
            }
            let _ = r.close();
        }
    }

    let frames = truncate_trailing_empty_timepoints(frames, channels, slices, &file_name_map);
    let image_count = channels
        .checked_mul(slices)
        .and_then(|v| v.checked_mul(frames))
        .ok_or_else(|| BioFormatsError::Format("MicroManager: image count overflow".into()))?;

    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    meta_map.insert(
        "format".into(),
        MetadataValue::String("MicroManager".into()),
    );
    meta_map.insert(
        "pixel_type_str".into(),
        MetadataValue::String(pixel_type_str),
    );

    // Richer metadata, mirroring Java MicromanagerReader.parsePosition:
    //   channel names (ChNames), channel colors (ChColors), pixel calibration
    //   (PixelSize_um / z-step_um), comment, time, position name.
    if let Some(names) = json_str_array(summary, "ChNames") {
        for (q, name) in names.iter().enumerate() {
            meta_map.insert(
                format!("channel_name[{q}]"),
                MetadataValue::String(name.clone()),
            );
            meta_map.insert(
                format!("channel.{q}.name"),
                MetadataValue::String(name.clone()),
            );
        }
    }
    if let Some(colors) = json_str_array(summary, "ChColors") {
        for (q, color) in colors.iter().enumerate() {
            meta_map.insert(
                format!("channel_color[{q}]"),
                MetadataValue::String(color.clone()),
            );
        }
    }
    if let Some(px) = json_float(summary, "PixelSize_um") {
        if px > 0.0 {
            meta_map.insert("physicalSizeX".into(), MetadataValue::Float(px));
            meta_map.insert("physicalSizeY".into(), MetadataValue::Float(px));
        }
    }
    if let Some(interval) = json_float(summary, "Interval_ms") {
        if interval > 0.0 {
            meta_map.insert("timeIncrement".into(), MetadataValue::Float(interval));
        }
    }
    if let Some(step) = json_float(summary, "z-step_um") {
        if step > 0.0 {
            meta_map.insert("physicalSizeZ".into(), MetadataValue::Float(step));
        }
    }
    if let Some(comment) = json_str(summary, "Comment") {
        meta_map.insert("comment".into(), MetadataValue::String(comment));
    }
    if let Some(time) = json_str(summary, "Time") {
        meta_map.insert("time".into(), MetadataValue::String(time));
    }
    parse_acquisition_xml(dir, &mut meta_map);
    // PositionName appears in per-frame blocks; scan the whole document.
    if let Some(name) = json_str(&json, "PositionName") {
        if name != "null" && !name.is_empty() {
            meta_map.insert("image_name".into(), MetadataValue::String(name));
        }
    }
    if !meta_map.contains_key("image_name") {
        if let Some(name) = meta_path.file_name().and_then(|name| name.to_str()) {
            let name = name
                .strip_suffix("_metadata.txt")
                .or_else(|| name.strip_suffix("_metadata.json"))
                .unwrap_or(name);
            meta_map.insert("image_name".into(), MetadataValue::String(name.to_string()));
        }
    }

    // Fold the per-plane `Plane #NNNN <key>` entries produced by
    // `parse_key_and_value` into the series metadata.
    for (k, v) in std::mem::take(&mut frame.series_meta) {
        meta_map.insert(k, v);
    }

    let meta = ImageMetadata {
        size_x: width,
        size_y: height,
        size_z: slices,
        size_c: channels,
        size_t: frames,
        pixel_type,
        bits_per_pixel: (bits),
        image_count,
        dimension_order,
        is_rgb: is_rgb_summary,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian,
        resolution_count: 1,
        thumbnail: false,
        series_metadata: meta_map,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok(Position {
        metadata_file: meta_path.to_path_buf(),
        meta,
        file_name_map,
        tiffs,
        exposure_time: frame.exposure_time,
        timestamps: frame.timestamps,
        positions: frame.positions,
        binning: frame.binning,
        gain: frame.gain,
        detector_id: frame.detector_id,
        detector_model: frame.detector_model,
        detector_manufacturer: frame.detector_manufacturer,
        temperature: frame.temperature,
        voltage: frame.voltage,
        camera_ref: frame.camera_ref,
        camera_mode: frame.camera_mode,
    })
}

/// Per-plane / camera metadata accumulated while walking the `FrameKey-*`
/// blocks, mirroring the locals and `Position` fields populated by Java
/// `MicromanagerReader.parsePosition(String jsonData, int)` (~679-940).
#[derive(Default)]
struct FrameData {
    /// (z,c,t) -> absolute TIFF path (Java `fileNameMap`).
    file_name_map: HashMap<Index, PathBuf>,
    /// Series metadata produced by `parseKeyAndValue` (`Plane #NNNN <key>`).
    series_meta: HashMap<String, MetadataValue>,
    /// Per-plane stage positions (Java `positions`).
    positions: Vec<[Option<f64>; 3]>,
    exposure_time: Option<f64>,
    timestamps: Vec<f64>,
    binning: Option<String>,
    gain: Option<i32>,
    detector_id: Option<String>,
    detector_model: Option<String>,
    detector_manufacturer: Option<String>,
    temperature: Option<f64>,
    voltage: Vec<f64>,
    camera_ref: Option<String>,
    camera_mode: Option<String>,
}

/// Store a `Plane #NNNN <key>` series-metadata entry and, for the three
/// position keys, the per-plane stage coordinate. Faithful port of Java
/// `MicromanagerReader.parseKeyAndValue(key, value, digits, plane, nPlanes)`
/// (~609-626).
fn parse_key_and_value(
    data: &mut FrameData,
    key: &str,
    value: &str,
    digits: usize,
    plane: usize,
    n_planes: usize,
) {
    for i in plane..plane + n_planes {
        // using key alone will result in conflicts with metadata.txt values
        data.series_meta.insert(
            format!("Plane #{i:0digits$} {key}"),
            MetadataValue::String(value.to_string()),
        );
        if i >= data.positions.len() {
            data.positions.resize(i + 1, [None, None, None]);
        }
        if key == "XPositionUm" {
            data.positions[i][0] = value.parse().ok();
        } else if key == "YPositionUm" {
            data.positions[i][1] = value.parse().ok();
        } else if key == "ZPositionUm" {
            data.positions[i][2] = value.parse().ok();
        }
    }
}

/// Scan the JSON for `"FrameKey-<t>-<c>-<z>"` blocks. For each block (one per
/// plane), extract the `FileName` for the (z,c,t) -> path map and the per-plane
/// / camera metadata, mirroring the FrameKey branch of Java
/// `parsePosition(String jsonData, int)` (~827-940) and its `parseKeyAndValue`
/// per-plane handling. `digits` is the zero-padding width Java derives from the
/// plane count.
fn parse_frame_keys(json: &str, dir: &Path, data: &mut FrameData, digits: usize) {
    let mut plane = 0usize;
    let mut search = 0;
    while let Some(rel) = json[search..].find("\"FrameKey-") {
        let abs = search + rel + 1; // position after the opening quote
        let rest = &json[abs..];
        // rest starts with FrameKey-<t>-<c>-<z>"
        let end_quote = match rest.find('"') {
            Some(e) => e,
            None => break,
        };
        let key = &rest[..end_quote]; // FrameKey-t-c-z
        search = abs + end_quote;

        let parts: Vec<&str> = key.trim_start_matches("FrameKey-").split('-').collect();
        if parts.len() < 3 {
            continue;
        }
        let t: u32 = match parts[0].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let c: u32 = match parts[1].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };
        let z: u32 = match parts[2].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        // Isolate the block following this key, stopping at the next FrameKey
        // (or end of document).
        let block_start = search;
        let block_end = json[block_start..]
            .find("\"FrameKey-")
            .map(|p| block_start + p)
            .unwrap_or(json.len());
        let block = &json[block_start..block_end];

        if let Some(fname) = json_str(block, "FileName") {
            let real = dir.join(&fname);
            if real.exists() {
                data.file_name_map.insert(Index { z, c, t }, real);
            } else {
                data.file_name_map
                    .insert(Index { z, c, t }, dir.join(fname));
            }
        }

        // Per-plane position keys (Java parseKeyAndValue, nPlanes = 1 here since
        // each FrameKey block corresponds to a single plane).
        for pos_key in ["XPositionUm", "YPositionUm", "ZPositionUm"] {
            if let Some(v) = json_float(block, pos_key) {
                parse_key_and_value(data, pos_key, &v.to_string(), digits, plane, 1);
            }
        }

        // Exposure-ms / ElapsedTime-ms (Java ~885-890).
        if let Some(exp) = json_float(block, "Exposure-ms") {
            data.exposure_time = Some(exp);
        }
        if let Some(elapsed) = json_float(block, "ElapsedTime-ms") {
            data.timestamps.push(elapsed);
        }

        // Camera/detector keys (Java ~891-920). cameraRef (Core-Camera) gates
        // the camera-prefixed keys.
        if let Some(cam) = json_str(block, "Core-Camera") {
            data.camera_ref = Some(cam);
        }
        if let Some(cam) = data.camera_ref.clone() {
            if let Some(v) = json_str(block, &format!("{cam}-Binning")) {
                data.binning = Some(if v.contains('x') {
                    v
                } else {
                    format!("{v}x{v}")
                });
            }
            if let Some(v) = json_str(block, &format!("{cam}-CameraID")) {
                data.detector_id = Some(v);
            }
            if let Some(v) = json_str(block, &format!("{cam}-CameraName")) {
                data.detector_model = Some(v);
            }
            if let Some(v) = json_float(block, &format!("{cam}-Gain")) {
                data.gain = Some(v as i32);
            }
            if let Some(v) = json_str(block, &format!("{cam}-Name")) {
                data.detector_manufacturer = Some(v);
            }
            if let Some(v) = json_float(block, &format!("{cam}-Temperature")) {
                data.temperature = Some(v);
            }
            if let Some(v) = json_str(block, &format!("{cam}-CCDMode")) {
                data.camera_mode = Some(v);
            }
        }

        // DAC-*-Volts voltages (Java ~918-920).
        collect_dac_volts(block, &mut data.voltage);

        plane += 1;
    }

    // Java sorts the gathered timestamps (`Arrays.sort(p.timestamps)`, ~977).
    data.timestamps
        .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
}

/// Scan `"Coords-<file>"` blocks and map their `z`/`channel`/`time` coordinates
/// to the named TIFF file. Java stores `position` too, but `Position.getFile`
/// matches only Z/C/T when resolving a logical plane.
fn parse_coords_blocks(json: &str, dir: &Path, data: &mut FrameData) {
    let mut search = 0;
    while let Some(rel) = json[search..].find("\"Coords-") {
        let abs = search + rel + 1;
        let rest = &json[abs..];
        let Some(end_quote) = rest.find('"') else {
            break;
        };
        let path = rest["Coords-".len()..end_quote].trim();
        search = abs + end_quote + 1;

        let Some(block_start_rel) = json[search..].find('{') else {
            continue;
        };
        let block_start = search + block_start_rel;
        let Some(block_end) = find_matching_brace(json, block_start) else {
            continue;
        };
        let block = &json[block_start..=block_end];
        let z = json_int(block, "z").unwrap_or(0).max(0) as u32;
        let c = json_int(block, "channel").unwrap_or(0).max(0) as u32;
        let t = json_int(block, "time").unwrap_or(0).max(0) as u32;
        data.file_name_map.insert(Index { z, c, t }, dir.join(path));
        search = block_end + 1;
    }
}

fn find_matching_brace(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for (rel, ch) in s[open..].char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' && in_string {
            escaped = true;
            continue;
        }
        if ch == '"' {
            in_string = !in_string;
            continue;
        }
        if in_string {
            continue;
        }
        match ch {
            '{' => depth += 1,
            '}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(open + rel);
                }
            }
            _ => {}
        }
    }
    None
}

fn parse_acquisition_xml(dir: &Path, meta: &mut HashMap<String, MetadataValue>) {
    let path = dir.join(ACQUISITION_XML);
    let Ok(xml) = std::fs::read_to_string(path) else {
        return;
    };
    let mut reader = quick_xml::Reader::from_str(&xml);
    reader.config_mut().trim_text(true);
    loop {
        match reader.read_event() {
            Ok(quick_xml::events::Event::Start(e)) | Ok(quick_xml::events::Event::Empty(e))
                if e.local_name().as_ref().eq_ignore_ascii_case(b"entry") =>
            {
                let mut key = None;
                let mut value = None;
                for attr in e.attributes().flatten() {
                    if attr.key.as_ref().eq_ignore_ascii_case(b"key") {
                        key = attr
                            .decoded_and_normalized_value(
                                quick_xml::XmlVersion::Implicit1_0,
                                reader.decoder(),
                            )
                            .ok()
                            .map(|v| v.into_owned());
                    } else if attr.key.as_ref().eq_ignore_ascii_case(b"value") {
                        value = attr
                            .decoded_and_normalized_value(
                                quick_xml::XmlVersion::Implicit1_0,
                                reader.decoder(),
                            )
                            .ok()
                            .map(|v| v.into_owned());
                    }
                }
                if let (Some(k), Some(v)) = (key, value) {
                    meta.insert(k, MetadataValue::String(v));
                }
            }
            Ok(quick_xml::events::Event::Eof) => break,
            Err(_) => break,
            _ => {}
        }
    }
}

fn parse_tiff_private_metadata(
    tiffs: &[PathBuf],
    file_name_map: &HashMap<Index, PathBuf>,
    dimension_order: DimensionOrder,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    data: &mut FrameData,
    digits: usize,
) {
    let mut parse_mm_json_tag = true;
    let mut plane = 0usize;
    while plane < tiffs.len() {
        let mut path = tiffs[plane].clone();
        if tiffs.len() == file_name_map.len() {
            if let Some(mapped) = file_name_for_raster_plane(
                file_name_map,
                dimension_order,
                size_z,
                size_c,
                size_t,
                plane as u32,
            ) {
                path = mapped;
            }
        }
        if !path.exists() {
            plane += 1;
            continue;
        }
        let mut reader = TiffReader::new();
        if reader.set_id(&path).is_err() {
            plane += 1;
            continue;
        }
        let n_ifds = reader.ifd_count().max(1);
        if let Some(json) = reader.ifd(0).and_then(|ifd| ifd_text_value(ifd, JSON_TAG)) {
            parse_first_ifd_json_tag(data, &json, digits, plane * n_ifds, n_ifds);
        }
        for i in 0..n_ifds {
            if !parse_mm_json_tag {
                break;
            }
            let Some(json) = reader
                .ifd(i)
                .and_then(|ifd| ifd_text_value(ifd, MM_JSON_TAG))
            else {
                parse_mm_json_tag = false;
                break;
            };
            parse_mm_json_tag_value(data, &json, digits, plane + i);
        }
        plane += n_ifds;
    }
    data.timestamps
        .sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
}

fn file_name_for_raster_plane(
    file_name_map: &HashMap<Index, PathBuf>,
    dimension_order: DimensionOrder,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    no: u32,
) -> Option<PathBuf> {
    let (z, c, t) = Position::zct_coords(dimension_order, size_z, size_c, size_t, no);
    let key = Index { z, c, t };
    file_name_map.get(&key).cloned()
}

fn ifd_text_value(ifd: &Ifd, tag: u16) -> Option<String> {
    match ifd.get(tag)? {
        IfdValue::Ascii(s) => Some(s.trim_end_matches('\0').to_string()),
        IfdValue::Byte(v) | IfdValue::Undefined(v) => Some(
            String::from_utf8_lossy(v)
                .trim_end_matches('\0')
                .to_string(),
        ),
        _ => None,
    }
}

fn parse_first_ifd_json_tag(
    data: &mut FrameData,
    json: &str,
    digits: usize,
    plane: usize,
    n_planes: usize,
) {
    for line in json.lines() {
        let to_split = line.trim().trim_end_matches(',');
        let Some((key, value)) = to_split.split_once("\": ") else {
            continue;
        };
        let key = key.replace('"', "");
        let value = value.replace('"', "");
        if !key.is_empty() && !value.is_empty() {
            parse_key_and_value(data, &key, &value, digits, plane, n_planes);
        }
    }
}

fn parse_mm_json_tag_value(data: &mut FrameData, json: &str, digits: usize, plane: usize) {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(json) else {
        return;
    };
    parse_mm_json_value(data, &value, digits, plane);
}

fn parse_mm_json_value(
    data: &mut FrameData,
    value: &serde_json::Value,
    digits: usize,
    plane: usize,
) {
    match value {
        serde_json::Value::Object(map) => {
            for (key, child) in map {
                if key == "completeCoords" {
                    parse_mm_json_value(data, child, digits, plane);
                    continue;
                }
                if let Some(prop) = child.get("PropVal") {
                    let text = json_scalar_to_java_string(prop);
                    parse_key_and_value(data, key, &text, digits, plane, 1);
                    parse_frame_scalar(data, key, &text);
                } else if child.is_object() || child.is_array() {
                    parse_mm_json_value(data, child, digits, plane);
                } else {
                    let text = json_scalar_to_java_string(child);
                    parse_key_and_value(data, key, &text, digits, plane, 1);
                    parse_frame_scalar(data, key, &text);
                }
            }
        }
        serde_json::Value::Array(items) => {
            for item in items {
                parse_mm_json_value(data, item, digits, plane);
            }
        }
        _ => {}
    }
}

fn json_scalar_to_java_string(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::Bool(v) => v.to_string(),
        serde_json::Value::Array(items) => items
            .iter()
            .map(json_scalar_to_java_string)
            .collect::<Vec<_>>()
            .join(", "),
        serde_json::Value::Null => "null".into(),
        serde_json::Value::Object(_) => value.to_string(),
    }
}

fn parse_frame_scalar(data: &mut FrameData, key: &str, value: &str) {
    if key == "Exposure-ms" {
        data.exposure_time = value.parse().ok();
    } else if key == "ElapsedTime-ms" {
        if let Ok(v) = value.parse() {
            data.timestamps.push(v);
        }
    } else if key == "Core-Camera" {
        data.camera_ref = Some(value.to_string());
    } else if let Some(cam) = data.camera_ref.clone() {
        if key == format!("{cam}-Binning") {
            data.binning = Some(if value.contains('x') {
                value.to_string()
            } else {
                format!("{value}x{value}")
            });
        } else if key == format!("{cam}-CameraID") {
            data.detector_id = Some(value.to_string());
        } else if key == format!("{cam}-CameraName") {
            data.detector_model = Some(value.to_string());
        } else if key == format!("{cam}-Gain") {
            data.gain = value.parse::<f64>().ok().map(|v| v as i32);
        } else if key == format!("{cam}-Name") {
            data.detector_manufacturer = Some(value.to_string());
        } else if key == format!("{cam}-Temperature") {
            data.temperature = value.parse().ok();
        } else if key == format!("{cam}-CCDMode") {
            data.camera_mode = Some(value.to_string());
        } else if key.starts_with("DAC-") && key.ends_with("-Volts") {
            if let Ok(v) = value.parse() {
                data.voltage.push(v);
            }
        }
    }
}

/// Append the voltage of every `DAC-*-Volts` key found in `block`, mirroring the
/// Java branch `key.startsWith("DAC-") && key.endsWith("-Volts")` (~918-920).
fn collect_dac_volts(block: &str, voltage: &mut Vec<f64>) {
    let mut search = 0;
    while let Some(rel) = block[search..].find("\"DAC-") {
        let abs = search + rel + 1; // after the opening quote
        let rest = &block[abs..];
        let end_quote = match rest.find('"') {
            Some(e) => e,
            None => break,
        };
        let key = &rest[..end_quote];
        // Advance past the closing quote of the key.
        search = abs + end_quote + 1;
        if key.ends_with("-Volts") {
            if let Some(v) = read_value_after(&block[search..]) {
                voltage.push(v);
            }
        }
    }
}

/// Match Java `buildTIFFList`'s adjustment for acquisitions stopped before the
/// declared frame count: if one or more final timepoints have no TIFF files,
/// `sizeT` is truncated to the first empty trailing timepoint.
fn truncate_trailing_empty_timepoints(
    frames: u32,
    channels: u32,
    slices: u32,
    file_name_map: &HashMap<Index, PathBuf>,
) -> u32 {
    if frames <= 1 || file_name_map.is_empty() {
        return frames;
    }

    let mut first_empty_timepoint: Option<u32> = None;
    for t in 0..frames {
        let mut empty = true;
        for c in 0..channels.max(1) {
            for z in 0..slices.max(1) {
                if file_name_map
                    .get(&Index { z, c, t })
                    .is_some_and(|path| path.exists())
                {
                    empty = false;
                    break;
                }
            }
            if !empty {
                break;
            }
        }

        if empty && first_empty_timepoint.is_none() {
            first_empty_timepoint = Some(t);
        } else if !empty && first_empty_timepoint.is_some() {
            first_empty_timepoint = None;
        }
    }

    first_empty_timepoint.unwrap_or(frames).max(1)
}

/// Read the numeric value following the next `:` in `s` (the value of the JSON
/// key whose closing quote `s` begins at). Used for `DAC-*-Volts` entries whose
/// key names are dynamic and cannot be matched with the fixed-key helpers.
fn read_value_after(s: &str) -> Option<f64> {
    let after_colon = s.trim_start().strip_prefix(':')?.trim_start();
    let after_colon = after_colon.trim_start_matches('"');
    let end = after_colon
        .find(|c: char| {
            !c.is_ascii_digit() && c != '-' && c != '.' && c != 'e' && c != 'E' && c != '+'
        })
        .unwrap_or(after_colon.len());
    after_colon[..end].parse().ok()
}

/// Java `isThisType(String, true)` accepts Micro-Manager metadata names only
/// when the first metadata block contains "micro-manager" or "micromanager".
fn metadata_file_has_micromanager_marker(path: &Path) -> bool {
    const BLOCK_SIZE: u64 = 1_048_576;
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let len = file.metadata().map(|m| m.len()).unwrap_or(0);
    if len == 0 {
        return false;
    }
    let mut buf = Vec::new();
    if file
        .by_ref()
        .take(BLOCK_SIZE)
        .read_to_end(&mut buf)
        .is_err()
    {
        return false;
    }
    let data = String::from_utf8_lossy(&buf).to_ascii_lowercase();
    data.contains("micro-manager") || data.contains("micromanager")
}

fn prefix_metadata_name(name: &str) -> String {
    let lower = name.to_ascii_lowercase();
    let stem = if lower.ends_with(".ome.tiff") {
        &name[..name.len() - ".ome.tiff".len()]
    } else if lower.ends_with(".ome.tif") {
        &name[..name.len() - ".ome.tif".len()]
    } else {
        name.rsplit_once('.').map(|(stem, _)| stem).unwrap_or(name)
    };
    format!("{stem}_metadata.txt")
}

fn has_tiff_header(path: &Path) -> bool {
    let Ok(mut file) = File::open(path) else {
        return false;
    };
    let mut header = [0u8; 4];
    if file.read_exact(&mut header).is_err() {
        return false;
    }
    matches!(
        header,
        [b'I', b'I', 42, 0] | [b'M', b'M', 0, 42] | [b'I', b'I', 43, 0] | [b'M', b'M', 0, 43]
    )
}

fn micro_manager_metadata_entry(path: &Path) -> PathBuf {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();
    let lower = name.to_ascii_lowercase();
    if lower == "metadata.txt" || lower.ends_with("_metadata.txt") || lower == "metadata.json" {
        return path.to_path_buf();
    }
    if name.eq_ignore_ascii_case("Acqusition.xml") {
        if let Some(parent) = path.parent() {
            let metadata = parent.join("metadata.txt");
            if metadata.exists() {
                return metadata;
            }
            return parent.join(prefix_metadata_name(name));
        }
    }
    if let Some(parent) = path.parent() {
        let metadata = parent.join("metadata.txt");
        if metadata.exists() {
            return metadata;
        }
        return parent.join(prefix_metadata_name(name));
    }
    path.to_path_buf()
}

/// Discover all sibling `Pos_*` position directories (multi-position series).
/// Returns the list of metadata.txt paths in sorted order, or just the single
/// supplied file if this is not a multi-position dataset.
fn discover_positions(meta_path: &Path) -> Vec<PathBuf> {
    let parent = match meta_path.parent() {
        Some(p) => p,
        None => return vec![meta_path.to_path_buf()],
    };
    let parent_name = parent
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or_default();

    if parent_name.contains("Pos_") {
        // Sibling Pos_* directories each contain their own metadata.txt.
        let grandparent = match parent.parent() {
            Some(gp) => gp,
            None => return vec![meta_path.to_path_buf()],
        };
        let mut dirs: Vec<PathBuf> = std::fs::read_dir(grandparent)
            .map(|rd| {
                rd.filter_map(|e| e.ok())
                    .map(|e| e.path())
                    .filter(|p| {
                        p.is_dir()
                            && p.file_name()
                                .and_then(|n| n.to_str())
                                .map(|n| n.contains("Pos_"))
                                .unwrap_or(false)
                    })
                    .map(|p| p.join("metadata.txt"))
                    .filter(|p| p.exists())
                    .collect()
            })
            .unwrap_or_default();
        dirs.sort();
        if dirs.is_empty() {
            return vec![meta_path.to_path_buf()];
        }
        return dirs;
    }

    vec![meta_path.to_path_buf()]
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct MicromanagerReader {
    meta_path: Option<PathBuf>,
    positions: Vec<Position>,
    series: usize,
}

impl MicromanagerReader {
    pub fn new() -> Self {
        MicromanagerReader {
            meta_path: None,
            positions: Vec::new(),
            series: 0,
        }
    }
}

impl Default for MicromanagerReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MicromanagerReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_ascii_lowercase())
            .unwrap_or_default();
        if name == "metadata.txt" || name.ends_with("_metadata.txt") {
            return metadata_file_has_micromanager_marker(path);
        }
        if name == "acqusition.xml" {
            return metadata_file_has_micromanager_marker(path);
        }
        let is_tiff = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| {
                let e = e.to_ascii_lowercase();
                e == "tif" || e == "tiff"
            })
            .unwrap_or(false);
        if is_tiff {
            let metadata = micro_manager_metadata_entry(path);
            return has_tiff_header(path) && metadata_file_has_micromanager_marker(&metadata);
        }
        // Extra Rust support for JSON-named metadata; Java uses metadata.txt or
        // *_metadata.txt, but accepting metadata.json does not alter those paths.
        name == "metadata.json"
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Java MicromanagerReader.isThisType(RandomAccessInputStream)
        // delegates directly to MinimalTiffReader.
        matches!(
            header.get(..4),
            Some([b'I', b'I', 42, 0])
                | Some([b'M', b'M', 0, 42])
                | Some([b'I', b'I', 43, 0])
                | Some([b'M', b'M', 0, 43])
        )
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let metadata_entry = micro_manager_metadata_entry(path);
        let position_files = discover_positions(&metadata_entry);
        let mut positions = Vec::with_capacity(position_files.len());
        for pf in position_files {
            positions.push(parse_position(&pf)?);
        }
        if positions.is_empty() {
            return Err(BioFormatsError::Format(
                "MicroManager: no positions found".into(),
            ));
        }
        self.positions = positions;
        self.series = 0;
        self.meta_path = Some(metadata_entry);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta_path = None;
        self.positions.clear();
        self.series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.positions.len().max(1)
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.positions.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.series = s;
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.positions
            .get(self.series)
            .map(|position| &position.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn ome_metadata(&self) -> Option<OmeMetadata> {
        let pos = self.positions.get(self.series)?;
        Some(pos.populate_metadata_store(self.series))
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let pos = self
            .positions
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= pos.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let Some(file) = pos.file_for_plane(plane_index) else {
            return Ok(vec![
                0;
                pos.plane_byte_count(pos.meta.size_x, pos.meta.size_y)?
            ]);
        };
        if !file.exists() {
            return Ok(vec![
                0;
                pos.plane_byte_count(pos.meta.size_x, pos.meta.size_y)?
            ]);
        }
        let mut r = TiffReader::new();
        r.set_id(&file)?;
        let inner_count = r.metadata().image_count.max(1);
        let inner_idx = plane_index % inner_count;
        r.open_bytes(inner_idx)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let pos = self
            .positions
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= pos.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let Some(file) = pos.file_for_plane(plane_index) else {
            return Ok(vec![0; pos.plane_byte_count(w, h)?]);
        };
        if !file.exists() {
            return Ok(vec![0; pos.plane_byte_count(w, h)?]);
        }
        let mut r = TiffReader::new();
        r.set_id(&file)?;
        let inner_count = r.metadata().image_count.max(1);
        let inner_idx = plane_index % inner_count;
        r.open_bytes_region(inner_idx, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .positions
            .get(self.series)
            .map(|p| &p.meta)
            .ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn write_mm_private_tiff(
        path: &Path,
        pixel: u8,
        json_tag: Option<&str>,
        mm_json_tag: Option<&str>,
    ) {
        let entry_count = 9 + u16::from(json_tag.is_some()) + u16::from(mm_json_tag.is_some());
        let mut payloads = Vec::<(u16, Vec<u8>)>::new();
        if let Some(text) = json_tag {
            let mut bytes = text.as_bytes().to_vec();
            bytes.push(0);
            payloads.push((JSON_TAG, bytes));
        }
        if let Some(text) = mm_json_tag {
            let mut bytes = text.as_bytes().to_vec();
            bytes.push(0);
            payloads.push((MM_JSON_TAG, bytes));
        }
        let mut next_offset = 8 + 2 + u32::from(entry_count) * 12 + 4;
        let payload_offsets: Vec<u32> = payloads
            .iter()
            .map(|(_, bytes)| {
                let offset = next_offset;
                next_offset += bytes.len() as u32;
                offset
            })
            .collect();
        let pixel_offset = next_offset;

        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        push_u16_le(&mut data, 42);
        push_u32_le(&mut data, 8);
        push_u16_le(&mut data, entry_count);
        push_ifd_long(&mut data, crate::tiff::ifd::tag::IMAGE_WIDTH, 1);
        push_ifd_long(&mut data, crate::tiff::ifd::tag::IMAGE_LENGTH, 1);
        push_ifd_short(&mut data, crate::tiff::ifd::tag::BITS_PER_SAMPLE, 8);
        push_ifd_short(&mut data, crate::tiff::ifd::tag::COMPRESSION, 1);
        push_ifd_short(
            &mut data,
            crate::tiff::ifd::tag::PHOTOMETRIC_INTERPRETATION,
            1,
        );
        push_ifd_long(
            &mut data,
            crate::tiff::ifd::tag::STRIP_OFFSETS,
            pixel_offset,
        );
        push_ifd_short(&mut data, crate::tiff::ifd::tag::SAMPLES_PER_PIXEL, 1);
        push_ifd_long(&mut data, crate::tiff::ifd::tag::ROWS_PER_STRIP, 1);
        push_ifd_long(&mut data, crate::tiff::ifd::tag::STRIP_BYTE_COUNTS, 1);
        for ((tag, bytes), offset) in payloads.iter().zip(payload_offsets.iter()) {
            push_ifd_ascii(&mut data, *tag, bytes.len() as u32, *offset);
        }
        push_u32_le(&mut data, 0);
        for (_, bytes) in payloads {
            data.extend_from_slice(&bytes);
        }
        data.push(pixel);
        std::fs::write(path, data).unwrap();
    }

    /// Build a temp MicroManager dataset: a `metadata.txt` with two FrameKey
    /// blocks (two time points) carrying per-plane position/exposure/elapsed and
    /// camera/detector keys, plus the real TIFF files the FrameKeys reference
    /// (copied from the test fixture so `file_for_plane(..).exists()` holds).
    fn write_dataset() -> PathBuf {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_8x8_gray8.tif");
        let dir = std::env::temp_dir().join(format!(
            "mm_meta_test_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&fixture, dir.join("img_0.tif")).unwrap();
        std::fs::copy(&fixture, dir.join("img_1.tif")).unwrap();

        let json = r#"{
  "Summary": {
    "Width": 8,
    "Height": 8,
    "Channels": 1,
    "Slices": 1,
    "Frames": 2,
    "PixelType": "GRAY8",
    "MicroManagerVersion": "2.0",
    "SlicesFirst": false,
    "ChNames": ["DAPI"],
    "PixelSize_um": 0.5
  },
  "FrameKey-0-0-0": {
    "FileName": "img_0.tif",
    "XPositionUm": 10.5,
    "YPositionUm": 20.25,
    "ZPositionUm": 1.0,
    "Exposure-ms": 100.0,
    "ElapsedTime-ms": 50.0,
    "Core-Camera": "Camera",
    "Camera-Binning": "2",
    "Camera-CameraID": "SN12345",
    "Camera-CameraName": "ZylaModel",
    "Camera-Gain": "3",
    "Camera-Name": "Andor",
    "Camera-Temperature": "-20.0",
    "Camera-CCDMode": "EMCCD",
    "DAC-Dev-Volts": "1.5"
  },
  "FrameKey-1-0-0": {
    "FileName": "img_1.tif",
    "XPositionUm": 11.5,
    "YPositionUm": 21.25,
    "ZPositionUm": 2.0,
    "Exposure-ms": 100.0,
    "ElapsedTime-ms": 150.0
  }
}"#;
        std::fs::write(dir.join("metadata.txt"), json).unwrap();
        dir.join("metadata.txt")
    }

    #[test]
    fn per_plane_keys_and_detector_fields_are_extracted() {
        let meta_path = write_dataset();
        let dir = meta_path.parent().unwrap().to_path_buf();

        let mut reader = MicromanagerReader::new();
        reader.set_id(&meta_path).unwrap();
        let m = reader.metadata();

        // `Plane #N <key>` series-metadata, with single-digit padding (2 planes
        // -> digits = String.valueOf(1).length() = 1).
        let sm = &m.series_metadata;
        let str_meta = |k: &str| match sm.get(k) {
            Some(MetadataValue::String(s)) => Some(s.clone()),
            _ => None,
        };
        assert_eq!(str_meta("Plane #0 XPositionUm").as_deref(), Some("10.5"));
        assert_eq!(str_meta("Plane #0 YPositionUm").as_deref(), Some("20.25"));
        assert_eq!(str_meta("Plane #0 ZPositionUm").as_deref(), Some("1"));
        assert_eq!(str_meta("Plane #1 XPositionUm").as_deref(), Some("11.5"));

        // Per-plane stage positions, exposure, sorted elapsed timestamps.
        let pos = &reader.positions[0];
        assert_eq!(pos.positions[0], [Some(10.5), Some(20.25), Some(1.0)]);
        assert_eq!(pos.positions[1], [Some(11.5), Some(21.25), Some(2.0)]);
        assert_eq!(pos.exposure_time, Some(100.0));
        assert_eq!(pos.timestamps, vec![50.0, 150.0]);

        // Camera / detector fields.
        assert_eq!(pos.camera_ref.as_deref(), Some("Camera"));
        assert_eq!(pos.binning.as_deref(), Some("2x2"));
        assert_eq!(pos.detector_id.as_deref(), Some("SN12345"));
        assert_eq!(pos.detector_model.as_deref(), Some("ZylaModel"));
        assert_eq!(pos.detector_manufacturer.as_deref(), Some("Andor"));
        assert_eq!(pos.gain, Some(3));
        assert_eq!(pos.temperature, Some(-20.0));
        assert_eq!(pos.camera_mode.as_deref(), Some("EMCCD"));
        assert_eq!(pos.voltage, vec![1.5]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn tiff_entry_point_resolves_sibling_metadata_like_java() {
        let meta_path = write_dataset();
        let dir = meta_path.parent().unwrap().to_path_buf();
        let tiff_path = dir.join("img_0.tif");

        let probe = MicromanagerReader::new();
        assert!(probe.is_this_type_by_name(&tiff_path));

        let mut reader = MicromanagerReader::new();
        reader.set_id(&tiff_path).unwrap();
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.open_bytes(1).unwrap().len(), 64);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn byte_probe_delegates_to_tiff_header_like_java() {
        let reader = MicromanagerReader::new();
        assert!(reader.is_this_type_by_bytes(&[b'I', b'I', 42, 0]));
        assert!(reader.is_this_type_by_bytes(&[b'M', b'M', 0, 42, 0]));
        assert!(reader.is_this_type_by_bytes(&[b'I', b'I', 43, 0]));
        assert!(reader.is_this_type_by_bytes(&[b'M', b'M', 0, 43, 0]));
        assert!(!reader.is_this_type_by_bytes(&[b'I', b'I', 0]));
        assert!(!reader.is_this_type_by_bytes(b"metadata"));
    }

    #[test]
    fn ome_metadata_projects_planes_and_detector() {
        let meta_path = write_dataset();
        let dir = meta_path.parent().unwrap().to_path_buf();

        let mut reader = MicromanagerReader::new();
        reader.set_id(&meta_path).unwrap();
        let ome = reader.ome_metadata().unwrap();
        let img = &ome.images[0];

        // Two planes with stage positions, exposure and deltaT (from sorted
        // ElapsedTime-ms, since both planes' TIFFs exist).
        assert_eq!(img.planes.len(), 2);
        assert_eq!(img.planes[0].position_x, Some(10.5));
        assert_eq!(img.planes[0].position_y, Some(20.25));
        assert_eq!(img.planes[0].position_z, Some(1.0));
        assert_eq!(img.planes[0].exposure_time, Some(100.0));
        assert_eq!(img.planes[0].delta_t, Some(50.0));
        assert_eq!(img.planes[1].delta_t, Some(150.0));

        // Imaging-environment temperature.
        assert_eq!(img.imaging_environment_temperature, Some(-20.0));

        // DetectorSettings on the (single) channel.
        let ch = &img.channels[0];
        assert_eq!(ch.detector_settings_binning.as_deref(), Some("2x2"));
        assert_eq!(ch.detector_settings_gain, Some(3.0));
        assert_eq!(ch.detector_settings_voltage, Some(1.5));
        assert_eq!(ch.detector_ref.as_deref(), Some("Detector:0:0"));

        // Detector element.
        let inst = &ome.instruments[0];
        let det = &inst.detectors[0];
        assert_eq!(det.model.as_deref(), Some("ZylaModel"));
        assert_eq!(det.manufacturer.as_deref(), Some("Andor"));
        assert_eq!(det.detector_type.as_deref(), Some("EMCCD"));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn missing_mapped_tiff_returns_zero_filled_buffer_like_java() {
        let dir = std::env::temp_dir().join(format!(
            "mm_missing_tiff_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let json = r#"{
  "Summary": {
    "Width": 4,
    "Height": 3,
    "Channels": 1,
    "Slices": 1,
    "Frames": 1,
    "PixelType": "GRAY8"
  },
  "FrameKey-0-0-0": {
    "FileName": "missing.tif"
  }
}"#;
        let meta_path = dir.join("metadata.txt");
        std::fs::write(&meta_path, json).unwrap();

        let mut reader = MicromanagerReader::new();
        reader.set_id(&meta_path).unwrap();

        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(plane, vec![0; 12]);

        let region = reader.open_bytes_region(0, 1, 1, 2, 1).unwrap();
        assert_eq!(region, vec![0; 2]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn trailing_empty_timepoints_are_truncated_like_java() {
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/test_8x8_gray8.tif");
        let dir = std::env::temp_dir().join(format!(
            "mm_trailing_empty_timepoint_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::copy(&fixture, dir.join("img_0.tif")).unwrap();

        let json = r#"{
  "Summary": {
    "Width": 8,
    "Height": 8,
    "Channels": 1,
    "Slices": 1,
    "Frames": 3,
    "PixelType": "GRAY8"
  },
  "FrameKey-0-0-0": {
    "FileName": "img_0.tif"
  },
  "FrameKey-1-0-0": {
    "FileName": "img_1.tif"
  },
  "FrameKey-2-0-0": {
    "FileName": "img_2.tif"
  }
}"#;
        let meta_path = dir.join("metadata.txt");
        std::fs::write(&meta_path, json).unwrap();

        let mut reader = MicromanagerReader::new();
        reader.set_id(&meta_path).unwrap();
        let meta = reader.metadata();

        assert_eq!(meta.size_t, 1);
        assert_eq!(meta.image_count, 1);
        assert!(matches!(
            reader.open_bytes(1),
            Err(BioFormatsError::PlaneOutOfRange(1))
        ));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn metadata_name_detection_requires_java_marker() {
        let dir = std::env::temp_dir().join(format!(
            "mm_detection_marker_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("metadata.txt");
        let reader = MicromanagerReader::new();

        std::fs::write(&path, r#"{"Summary":{"Width":1}}"#).unwrap();
        assert!(!reader.is_this_type_by_name(&path));

        std::fs::write(&path, r#"{"Summary":{"Source":"Micro-Manager","Width":1}}"#).unwrap();
        assert!(reader.is_this_type_by_name(&path));

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn acqusition_xml_entry_resolves_sibling_metadata_like_java() {
        let metadata = write_dataset();
        let dir = metadata.parent().unwrap().to_path_buf();
        let xml = dir.join("Acqusition.xml");
        std::fs::write(
            &xml,
            r#"<root>Micro-Manager<entry key="XMLKey" value="XMLValue"/></root>"#,
        )
        .unwrap();

        let mut reader = MicromanagerReader::new();
        assert!(reader.is_this_type_by_name(&xml));
        reader.set_id(&xml).unwrap();
        assert_eq!(reader.metadata().size_x, 8);
        assert!(matches!(
            reader.metadata().series_metadata.get("XMLKey"),
            Some(MetadataValue::String(value)) if value == "XMLValue"
        ));
        assert_eq!(reader.open_bytes(0).unwrap()[0], 0);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn coords_blocks_drive_logical_plane_mapping_like_java() {
        let dir = std::env::temp_dir().join(format!(
            "mm_coords_mapping_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        write_mm_private_tiff(&dir.join("z1.tif"), 9, None, None);
        write_mm_private_tiff(&dir.join("z0.tif"), 7, None, None);
        std::fs::write(
            dir.join("metadata.txt"),
            r#"{
  "Summary": {
    "Source": "Micro-Manager",
    "Width": 1,
    "Height": 1,
    "Channels": 1,
    "Slices": 2,
    "Frames": 1,
    "PixelType": "GRAY8"
  },
  "Coords-z1.tif": {
    "position": 0,
    "time": 0,
    "z": 1,
    "channel": 0
  },
  "Coords-z0.tif": {
    "position": 0,
    "time": 0,
    "z": 0,
    "channel": 0
  }
}"#,
        )
        .unwrap();

        let mut reader = MicromanagerReader::new();
        reader.set_id(&dir.join("metadata.txt")).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![7]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![9]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn private_tiff_json_tags_populate_per_plane_metadata_like_java() {
        let dir = std::env::temp_dir().join(format!(
            "mm_private_json_tags_{}_{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        write_mm_private_tiff(
            &dir.join("plane.tif"),
            5,
            Some("\"YPositionUm\": 20.25,\n"),
            Some(
                r#"{
  "XPositionUm": {"PropVal": 10.5, "PropType": "Double"},
  "ZPositionUm": {"PropVal": 1.25, "PropType": "Double"},
  "Exposure-ms": {"PropVal": 12.5, "PropType": "Double"},
  "ElapsedTime-ms": {"PropVal": 3.5, "PropType": "Double"},
  "Core-Camera": {"PropVal": "Camera", "PropType": "String"},
  "Camera-Binning": {"PropVal": "2", "PropType": "String"},
  "Camera-Gain": {"PropVal": 4, "PropType": "Integer"}
}"#,
            ),
        );
        std::fs::write(
            dir.join("metadata.txt"),
            r#"{
  "Summary": {
    "Source": "Micro-Manager",
    "Width": 1,
    "Height": 1,
    "Channels": 1,
    "Slices": 1,
    "Frames": 1,
    "PixelType": "GRAY8"
  }
}"#,
        )
        .unwrap();

        let mut reader = MicromanagerReader::new();
        reader.set_id(&dir.join("metadata.txt")).unwrap();
        let m = reader.metadata();
        assert!(matches!(
            m.series_metadata.get("Plane #0 XPositionUm"),
            Some(MetadataValue::String(value)) if value == "10.5"
        ));
        assert!(matches!(
            m.series_metadata.get("Plane #0 YPositionUm"),
            Some(MetadataValue::String(value)) if value == "20.25"
        ));
        let ome = reader.ome_metadata().unwrap();
        let plane = &ome.images[0].planes[0];
        assert_eq!(plane.position_x, Some(10.5));
        assert_eq!(plane.position_y, Some(20.25));
        assert_eq!(plane.position_z, Some(1.25));
        assert_eq!(plane.exposure_time, Some(12.5));
        assert_eq!(plane.delta_t, Some(3.5));
        let channel = &ome.images[0].channels[0];
        assert_eq!(channel.detector_settings_binning.as_deref(), Some("2x2"));
        assert_eq!(channel.detector_settings_gain, Some(4.0));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![5]);

        let _ = std::fs::remove_dir_all(dir);
    }
}
