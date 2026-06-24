//! ICS (Image Cytometry Standard) reader and writer.
//!
//! Supports ICS version 1.0 (`.ics` + `.ids` pair) and 2.0 (single `.ics` file).
//! Handles gzip-compressed data and all standard pixel types.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;
use crate::common::writer::FormatWriter;

// ---- header parsing ---------------------------------------------------------

#[derive(Debug, Default)]
struct IcsHeader {
    version: f32,
    filename: Option<PathBuf>,
    /// Axis names (e.g. ["bits","x","y","z","t"])
    order: Vec<String>,
    /// Axis sizes in the same order as `order`
    sizes: Vec<u32>,
    significant_bits: u8,
    format: String,      // "real" or "integer"
    sign: String,        // "signed" or "unsigned"
    byte_order: Vec<u8>, // e.g. [1,2,3,4]
    gzip_compressed: bool,
    /// Byte offset of pixel data in the data file
    data_offset: u64,
    /// SVI Huygens files are stored Y-inverted (history software contains "SVI").
    invert_y: bool,
    /// Image name (from the `filename` header token, basename only).
    image_name: Option<String>,
    /// Per-axis physical scales (from `parameter scale`), aligned with `scale_axes`.
    scales: Vec<f64>,
    /// Axis labels (from `parameter labels`), aligned with `scales`.
    scale_axes: Vec<String>,
    /// Per-axis units (from `parameter units`), aligned with `scales`.
    scale_units: Vec<String>,
    /// Per-timepoint timestamps in seconds (from `parameter t`).
    timestamps: Vec<Option<f64>>,
    /// Emission wavelengths (from `sensor s_params LambdaEm`).
    em_waves: Vec<f64>,
    /// Excitation wavelengths (from `sensor s_params LambdaEx`).
    ex_waves: Vec<f64>,
    extra: HashMap<String, String>,
}

impl IcsHeader {
    fn parse(path: &Path) -> Result<IcsHeader> {
        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut reader = BufReader::new(f);
        let mut hdr = IcsHeader::default();

        let mut data_offset = 0u64;

        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).map_err(BioFormatsError::Io)?;
            if n == 0 {
                break;
            }

            let line = line.trim_end_matches(|c| c == '\r' || c == '\n');
            if line.eq_ignore_ascii_case("end") {
                // For ICS2, data immediately follows
                data_offset = reader.stream_position().map_err(BioFormatsError::Io)?;
                break;
            }

            let tokens = tokenize_ics_line(line);
            if tokens.is_empty() {
                continue;
            }

            match tokens[0].to_ascii_lowercase().as_str() {
                "ics_version" if tokens.len() >= 2 => {
                    hdr.version = parse_ics_scalar(&tokens[1], "ics_version")?;
                }
                "filename" if tokens.len() >= 2 => {
                    let joined = tokens[1..].join(" ");
                    // Image name is the basename (Java strips path separators).
                    let base = joined
                        .rsplit(['/', '\\'])
                        .next()
                        .unwrap_or(&joined)
                        .to_string();
                    hdr.image_name = Some(base);
                    hdr.filename = Some(PathBuf::from(joined));
                }
                "parameter" if tokens.len() >= 2 => match tokens[1].to_ascii_lowercase().as_str() {
                    "scale" => {
                        hdr.scales = tokens[2..]
                            .iter()
                            .map(|s| s.parse::<f64>().unwrap_or(f64::NAN))
                            .collect();
                    }
                    "labels" => {
                        hdr.scale_axes =
                            tokens[2..].iter().map(|s| s.to_ascii_lowercase()).collect();
                    }
                    "units" => {
                        hdr.scale_units =
                            tokens[2..].iter().map(|s| s.to_ascii_lowercase()).collect();
                    }
                    "t" => {
                        hdr.timestamps = parse_ics_optional_doubles(&tokens[2..]);
                    }
                    _ => {}
                },
                "sensor" if tokens.len() >= 4 && tokens[1].eq_ignore_ascii_case("s_params") => {
                    hdr.extra.insert(
                        format!("sensor\ts_params\t{}", tokens[2]),
                        tokens[3..].join(" "),
                    );
                    match tokens[2].as_str() {
                        "LambdaEm" => {
                            hdr.em_waves = tokens[3..]
                                .iter()
                                .filter_map(|s| s.parse::<f64>().ok())
                                .collect();
                        }
                        "LambdaEx" => {
                            hdr.ex_waves = tokens[3..]
                                .iter()
                                .filter_map(|s| s.parse::<f64>().ok())
                                .collect();
                        }
                        _ => {}
                    }
                }
                "history" if tokens.len() >= 3 && tokens[1].eq_ignore_ascii_case("software") => {
                    if tokens[2..].join(" ").contains("SVI") {
                        hdr.invert_y = true;
                    }
                }
                "layout" if tokens.len() >= 3 => match tokens[1].to_ascii_lowercase().as_str() {
                    "order" => {
                        hdr.order = tokens[2..].iter().map(|s| s.to_ascii_lowercase()).collect();
                    }
                    "sizes" => {
                        hdr.sizes = parse_ics_list(&tokens[2..], "layout sizes")?;
                    }
                    "significant_bits" | "significant bits" if tokens.len() >= 3 => {
                        hdr.significant_bits =
                            parse_ics_scalar(&tokens[2], "layout significant_bits")?;
                    }
                    _ => {}
                },
                "representation" if tokens.len() >= 3 => {
                    match tokens[1].to_ascii_lowercase().as_str() {
                        "format" => hdr.format = tokens[2].to_ascii_lowercase(),
                        "sign" => hdr.sign = tokens[2].to_ascii_lowercase(),
                        "byte_order" | "byteorder" => {
                            hdr.byte_order =
                                parse_ics_list(&tokens[2..], "representation byte_order")?;
                        }
                        "compression" if tokens.len() >= 3 => {
                            hdr.gzip_compressed =
                                tokens[2].contains("gzip") || tokens[2].contains("gz");
                        }
                        _ => {}
                    }
                }
                _ => {
                    // Store all other metadata as key-value
                    if tokens.len() >= 3 {
                        let key = format!("{}\t{}", tokens[0], tokens[1]);
                        let val = tokens[2..].join(" ");
                        hdr.extra.insert(key, val);
                    }
                }
            }
        }

        hdr.data_offset = data_offset;
        Ok(hdr)
    }
}

fn parse_ics_scalar<T>(value: &str, field: &str) -> Result<T>
where
    T: std::str::FromStr,
{
    value.parse().map_err(|_| {
        BioFormatsError::Format(format!("ICS invalid numeric value for {field}: {value}"))
    })
}

fn parse_ics_list<T>(values: &[String], field: &str) -> Result<Vec<T>>
where
    T: std::str::FromStr,
{
    values
        .iter()
        .map(|value| parse_ics_scalar(value.as_str(), field))
        .collect()
}

fn parse_ics_optional_doubles(values: &[String]) -> Vec<Option<f64>> {
    values
        .iter()
        .map(|value| value.parse::<f64>().ok())
        .collect()
}

fn tokenize_ics_line(line: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut within_quotes = false;

    for c in line.chars() {
        if (c.is_whitespace() || c == '\x04') && !within_quotes {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
        } else {
            if c == '"' {
                within_quotes = !within_quotes;
            }
            token.push(c);
        }
    }

    if !token.is_empty() {
        tokens.push(token);
    }
    tokens
}

/// Port of MetadataTools.makeSaneDimensionOrder: ensure all of X,Y,Z,C,T are
/// present (appending any missing in canonical order), then map to the enum.
fn make_sane_dimension_order(order: &str) -> DimensionOrder {
    let mut s: String = order.to_uppercase();
    for c in ['X', 'Y', 'Z', 'C', 'T'] {
        if !s.contains(c) {
            s.push(c);
        }
    }
    // Drop the leading XY (always present) and inspect the trailing ZCT order.
    let tail: String = s.chars().filter(|c| matches!(c, 'Z' | 'C' | 'T')).collect();
    match tail.as_str() {
        "CTZ" => DimensionOrder::XYCTZ,
        "CZT" => DimensionOrder::XYCZT,
        "TCZ" => DimensionOrder::XYTCZ,
        "TZC" => DimensionOrder::XYTZC,
        "ZCT" => DimensionOrder::XYZCT,
        "ZTC" => DimensionOrder::XYZTC,
        _ => DimensionOrder::XYCZT,
    }
}

fn pixel_type_from_ics(significant_bits: u8, format: &str, sign: &str) -> PixelType {
    match (significant_bits, format, sign) {
        (1, _, _) => PixelType::Bit,
        (8, _, "signed") => PixelType::Int8,
        (8, _, _) => PixelType::Uint8,
        (16, _, "signed") => PixelType::Int16,
        (16, _, _) => PixelType::Uint16,
        (32, "real", _) => PixelType::Float32,
        (32, _, "signed") => PixelType::Int32,
        (32, _, _) => PixelType::Uint32,
        // Java pixelTypeFromBytes(8, ...) returns DOUBLE for any 8-byte type,
        // so a 64-bit integer maps to Float64 (there is no Int64 pixel type).
        (64, _, _) => PixelType::Float64,
        _ => PixelType::Uint8,
    }
}

fn build_metadata(hdr: &IcsHeader) -> Result<ImageMetadata> {
    // ICS axis order: the first axis is usually "bits" (samples per pixel).
    // The remaining axes are spatial/temporal dimensions.
    let axes = &hdr.order;
    let sizes = &hdr.sizes;
    if axes.len() != sizes.len() {
        return Err(BioFormatsError::Format(
            "ICS: order and sizes length mismatch".into(),
        ));
    }

    // Port of ICSReader.java core-metadata construction.
    // sizes default to 0 (matching Java's m.sizeX==0 sentinel used by storedRGB).
    let mut size_x = 0u32;
    let mut size_y = 0u32;
    let mut size_z = 0u32;
    let mut size_c = 0u32;
    let mut size_t = 0u32;

    // dimensionOrder begins as "XY" and gains Z/T/C in axis order (first occurrence).
    let mut dim_order = String::from("XY");
    let mut bits_per_pixel = 0u32;
    // storedRGB: channel axis appears before the X axis.
    let mut stored_rgb = false;
    let mut is_rgb = false;

    for (axis, &sz) in axes.iter().zip(sizes.iter()) {
        match axis.as_str() {
            "bits" => {
                bits_per_pixel = sz;
                while bits_per_pixel % 8 != 0 {
                    bits_per_pixel += 1;
                }
                if bits_per_pixel == 24 || bits_per_pixel == 48 {
                    bits_per_pixel /= 3;
                }
            }
            "x" | "width" => size_x = sz,
            "y" | "height" => size_y = sz,
            "z" | "depth" => {
                size_z = sz;
                if !dim_order.contains('Z') {
                    dim_order.push('Z');
                }
            }
            "t" | "time" => {
                if size_t == 0 {
                    size_t = sz;
                } else {
                    size_t *= sz;
                }
                if !dim_order.contains('T') {
                    dim_order.push('T');
                }
            }
            // Any other axis (c, ch, channel, p, f, ...) is treated as a channel axis.
            _ => {
                if size_c == 0 {
                    size_c = sz;
                } else {
                    size_c *= sz;
                }
                // storedRGB / rgb depend on whether channel axis preceded X.
                stored_rgb = size_x == 0;
                is_rgb = size_x == 0 && size_c <= 4 && size_c > 1;
                if !dim_order.contains('C') {
                    dim_order.push('C');
                }
            }
        }
    }

    // Java ICSReader treats stored RGB as separate channel planes when each
    // stored channel has an emission wavelength annotation.
    if is_rgb && hdr.em_waves.len() == size_c as usize {
        is_rgb = false;
        stored_rgb = true;
    }

    let dimension_order = make_sane_dimension_order(&dim_order);

    if size_z == 0 {
        size_z = 1;
    }
    if size_c == 0 {
        size_c = 1;
    }
    if size_t == 0 {
        size_t = 1;
    }

    // Significant bits: prefer rounded bits-per-pixel from the "bits" axis.
    let sig = if bits_per_pixel != 0 {
        bits_per_pixel as u8
    } else if hdr.significant_bits != 0 {
        hdr.significant_bits
    } else {
        8
    };

    let pixel_type = pixel_type_from_ics(sig, &hdr.format, &hdr.sign);

    // imageCount = sizeZ * sizeT, times sizeC only when not RGB.
    let mut image_count = size_z * size_t;
    if !is_rgb {
        image_count *= size_c;
    }
    let _ = stored_rgb;

    let mut series_metadata: HashMap<String, MetadataValue> = hdr
        .extra
        .iter()
        .map(|(k, v)| (k.clone(), MetadataValue::String(v.clone())))
        .collect();
    series_metadata.insert(
        "ics_version".into(),
        MetadataValue::Float(hdr.version as f64),
    );

    // Endianness (ICSReader.java):
    //   littleEndian = real ? first==1 : first!=1
    // i.e. for INTEGER ics, first==1 means BIG-endian.
    let real = hdr.format == "real";
    let mut little_endian = true;
    if let Some(&first) = hdr.byte_order.first() {
        little_endian = if real { first == 1 } else { first != 1 };
    }
    // Sub-32-bit pixels: endianness is unconditionally flipped.
    if (sig as u32) < 32 {
        little_endian = !little_endian;
    }

    Ok(ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: sig,
        image_count,
        dimension_order,
        is_rgb,
        is_interleaved: is_rgb,
        is_indexed: false,
        is_little_endian: little_endian,
        resolution_count: 1,
        thumbnail: false,
        series_metadata,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    })
}

// ---- reader -----------------------------------------------------------------

pub struct IcsReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    header: Option<IcsHeader>,
}

impl IcsReader {
    pub fn new() -> Self {
        IcsReader {
            path: None,
            meta: None,
            header: None,
        }
    }

    fn header_path(path: &Path) -> PathBuf {
        match path.extension().and_then(|e| e.to_str()) {
            Some(ext) if ext.eq_ignore_ascii_case("ids") => {
                // Java ICSReader accepts either side of an ICS1 pair. Given
                // .ids, it derives the .ics path by converting D/d to C/c in
                // the extension, preserving the common all-uppercase case.
                let ics_ext = if ext.chars().next().is_some_and(|c| c.is_ascii_uppercase()) {
                    "ICS"
                } else {
                    "ics"
                };
                path.with_extension(ics_ext)
            }
            _ => path.to_path_buf(),
        }
    }

    fn data_path(ics_path: &Path, hdr: &IcsHeader) -> Result<PathBuf> {
        if hdr.version < 2.0 {
            // ICS1: the companion .ids lives next to the .ics on disk with the
            // same stem (Bio-Formats derives it from the actual file path, not
            // from the `filename` recorded inside the header). Match the .ics
            // extension case so .ICS -> .IDS, .ics -> .ids.
            Ok(match ics_path.extension().and_then(|e| e.to_str()) {
                Some(ext) if ext.chars().next().is_some_and(|c| c.is_ascii_uppercase()) => {
                    ics_path.with_extension("IDS")
                }
                _ => ics_path.with_extension("ids"),
            })
        } else {
            Ok(ics_path.to_path_buf())
        }
    }

    fn normalize_endianness(&self, mut buf: Vec<u8>) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let bps = meta.pixel_type.bytes_per_sample();
        if !meta.is_little_endian && bps > 1 {
            for chunk in buf.chunks_exact_mut(bps) {
                chunk.reverse();
            }
        }
        Ok(buf)
    }

    fn plane_coords(meta: &ImageMetadata, plane_index: u32) -> (u32, u32, u32) {
        let c_count = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        let z_count = meta.size_z.max(1);
        let t_count = meta.size_t.max(1);
        let mut rem = plane_index;
        let mut z = 0;
        let mut c = 0;
        let mut t = 0;

        for axis in match meta.dimension_order {
            DimensionOrder::XYCTZ => ['C', 'T', 'Z'],
            DimensionOrder::XYCZT => ['C', 'Z', 'T'],
            DimensionOrder::XYTCZ => ['T', 'C', 'Z'],
            DimensionOrder::XYTZC => ['T', 'Z', 'C'],
            DimensionOrder::XYZCT => ['Z', 'C', 'T'],
            DimensionOrder::XYZTC => ['Z', 'T', 'C'],
        } {
            match axis {
                'Z' => {
                    z = rem % z_count;
                    rem /= z_count;
                }
                'C' => {
                    c = rem % c_count;
                    rem /= c_count;
                }
                'T' => {
                    t = rem % t_count;
                    rem /= t_count;
                }
                _ => {}
            }
        }
        (z, c, t)
    }

    fn axis_coords_for_plane(&self, meta: &ImageMetadata, plane_index: u32) -> Result<Vec<u32>> {
        let hdr = self
            .header
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let (z, mut c_linear, mut t_linear) = Self::plane_coords(meta, plane_index);
        let mut coords = Vec::with_capacity(hdr.order.len());

        for (axis, &size) in hdr.order.iter().zip(hdr.sizes.iter()) {
            let coord = match axis.as_str() {
                "bits" | "x" | "width" | "y" | "height" => 0,
                "z" | "depth" => z,
                "t" | "time" => {
                    let n = size.max(1);
                    let coord = t_linear % n;
                    t_linear /= n;
                    coord
                }
                _ => {
                    if meta.is_rgb {
                        0
                    } else {
                        let n = size.max(1);
                        let coord = c_linear % n;
                        c_linear /= n;
                        coord
                    }
                }
            };
            coords.push(coord);
        }
        Ok(coords)
    }

    fn plane_index_for_coords(meta: &ImageMetadata, z: u32, c: u32, t: u32) -> u32 {
        let mut index = 0u32;
        let mut stride = 1u32;
        let c_count = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        for axis in match meta.dimension_order {
            DimensionOrder::XYCTZ => ['C', 'T', 'Z'],
            DimensionOrder::XYCZT => ['C', 'Z', 'T'],
            DimensionOrder::XYTCZ => ['T', 'C', 'Z'],
            DimensionOrder::XYTZC => ['T', 'Z', 'C'],
            DimensionOrder::XYZCT => ['Z', 'C', 'T'],
            DimensionOrder::XYZTC => ['Z', 'T', 'C'],
        } {
            let (coord, count) = match axis {
                'Z' => (z, meta.size_z.max(1)),
                'C' => (c, c_count),
                'T' => (t, meta.size_t.max(1)),
                _ => (0, 1),
            };
            index += coord * stride;
            stride *= count;
        }
        index
    }

    fn data_payload(&self) -> Result<Vec<u8>> {
        let hdr = self
            .header
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let ics_path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let data_path = Self::data_path(ics_path, hdr)?;
        let mut f = File::open(&data_path).map_err(BioFormatsError::Io)?;

        // Java opens the companion .ids stream for ICS1 and records offset 0;
        // the header-derived data offset applies only to embedded ICS2 pixels.
        let data_offset = if hdr.version < 2.0 {
            0
        } else {
            hdr.data_offset
        };
        let mut data = Vec::new();
        if hdr.gzip_compressed {
            f.seek(SeekFrom::Start(data_offset))
                .map_err(BioFormatsError::Io)?;
            let mut dec = flate2::read::GzDecoder::new(f);
            if dec.read_to_end(&mut data).is_ok() {
                return Ok(data);
            }

            // Java ICSReader falls back to raw reads when a file declares gzip
            // compression but the stream is not actually gzip-compressed.
            data.clear();
            let mut raw = File::open(&data_path).map_err(BioFormatsError::Io)?;
            raw.seek(SeekFrom::Start(data_offset))
                .map_err(BioFormatsError::Io)?;
            raw.read_to_end(&mut data).map_err(BioFormatsError::Io)?;
        } else {
            f.seek(SeekFrom::Start(data_offset))
                .map_err(BioFormatsError::Io)?;
            f.read_to_end(&mut data).map_err(BioFormatsError::Io)?;
        }
        Ok(data)
    }

    fn load_raw_data(&self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let hdr = self
            .header
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;

        let bytes_per_sample = meta.pixel_type.bytes_per_sample();
        let samples_per_pixel = if meta.is_rgb { meta.size_c.max(1) } else { 1 } as usize;
        let plane_samples = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|px| px.checked_mul(samples_per_pixel))
            .ok_or_else(|| BioFormatsError::InvalidData("ICS plane size overflow".into()))?;
        let plane_bytes = plane_samples
            .checked_mul(bytes_per_sample)
            .ok_or_else(|| BioFormatsError::InvalidData("ICS plane byte size overflow".into()))?;

        let payload = self.data_payload()?;
        let fixed_coords = self.axis_coords_for_plane(meta, plane_index)?;
        let mut strides = vec![0usize; hdr.order.len()];
        let mut stride = 1usize;
        for (i, (axis, &size)) in hdr.order.iter().zip(hdr.sizes.iter()).enumerate() {
            if axis == "bits" {
                strides[i] = 0;
                continue;
            }
            strides[i] = stride;
            stride = stride
                .checked_mul(size.max(1) as usize)
                .ok_or_else(|| BioFormatsError::InvalidData("ICS axis size overflow".into()))?;
        }

        let x_axis = hdr
            .order
            .iter()
            .position(|axis| axis == "x" || axis == "width")
            .ok_or_else(|| BioFormatsError::Format("ICS missing X axis".into()))?;
        let y_axis = hdr
            .order
            .iter()
            .position(|axis| axis == "y" || axis == "height")
            .ok_or_else(|| BioFormatsError::Format("ICS missing Y axis".into()))?;
        let channel_axis = meta
            .is_rgb
            .then(|| {
                hdr.order.iter().position(|axis| {
                    !matches!(
                        axis.as_str(),
                        "bits" | "x" | "width" | "y" | "height" | "z" | "depth" | "t" | "time"
                    )
                })
            })
            .flatten();

        let mut out = vec![0u8; plane_bytes];
        for y in 0..meta.size_y as usize {
            for x in 0..meta.size_x as usize {
                for s in 0..samples_per_pixel {
                    let mut coords = fixed_coords.clone();
                    coords[x_axis] = x as u32;
                    coords[y_axis] = y as u32;
                    if let Some(axis) = channel_axis {
                        coords[axis] = s as u32;
                    }
                    let sample_index = coords
                        .iter()
                        .zip(strides.iter())
                        .try_fold(0usize, |acc, (&coord, &stride)| {
                            (coord as usize)
                                .checked_mul(stride)
                                .and_then(|v| acc.checked_add(v))
                        })
                        .ok_or_else(|| {
                            BioFormatsError::InvalidData("ICS sample offset overflow".into())
                        })?;
                    let src = sample_index.checked_mul(bytes_per_sample).ok_or_else(|| {
                        BioFormatsError::InvalidData("ICS byte offset overflow".into())
                    })?;
                    let end = src.checked_add(bytes_per_sample).ok_or_else(|| {
                        BioFormatsError::InvalidData("ICS byte offset overflow".into())
                    })?;
                    if end > payload.len() {
                        return Err(BioFormatsError::InvalidData(
                            "plane out of range in ICS data".into(),
                        ));
                    }
                    let dst =
                        ((y * meta.size_x as usize + x) * samples_per_pixel + s) * bytes_per_sample;
                    out[dst..dst + bytes_per_sample].copy_from_slice(&payload[src..end]);
                }
            }
        }
        // SVI Huygens files are inverted on the Y axis (Java ICSReader.openBytes
        // flips full rows top-to-bottom when invertY is set).
        if hdr.invert_y && meta.size_y > 1 {
            let row_len = meta.size_x as usize * samples_per_pixel * bytes_per_sample;
            let h = meta.size_y as usize;
            for r in 0..h / 2 {
                let top = r * row_len;
                let bottom = (h - r - 1) * row_len;
                for i in 0..row_len {
                    out.swap(top + i, bottom + i);
                }
            }
        }
        self.normalize_endianness(out)
    }
}

impl Default for IcsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for IcsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ics") || e.eq_ignore_ascii_case("ids"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // ICS header starts with "ics_version" or whitespace-then-ics_version
        let s = std::str::from_utf8(&header[..header.len().min(64)]).unwrap_or("");
        s.trim_start().starts_with("ics_version")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.header = None;
        self.meta = None;

        let ics_path = Self::header_path(path);
        let hdr = IcsHeader::parse(&ics_path)?;
        let meta = build_metadata(&hdr)?;
        if hdr.version < 2.0 {
            let ids_path = Self::data_path(&ics_path, &hdr)?;
            if !ids_path.exists() {
                return Err(BioFormatsError::Format("IDS file not found.".into()));
            }
        }
        self.path = Some(ics_path);
        self.header = Some(hdr);
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.header = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        1
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 {
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
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.load_raw_data(plane_index)
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
        let samples_per_pixel = if meta.is_rgb { meta.size_c.max(1) } else { 1 } as usize;
        crop_full_plane("ICS", &full, meta, samples_per_pixel, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let hdr = self.header.as_ref()?;
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        if ome.instruments.is_empty() {
            ome.instruments
                .push(crate::common::ome_metadata::OmeInstrument {
                    id: Some(crate::common::ome_metadata::create_lsid("Instrument", &[0])),
                    objectives: vec![crate::common::ome_metadata::OmeObjective::default()],
                    detectors: vec![crate::common::ome_metadata::OmeDetector::default()],
                    ..Default::default()
                });
        }
        let sensor_value = |name: &str| {
            hdr.extra
                .get(&format!("sensor\ts_params\t{name}"))
                .map(String::as_str)
        };
        let sensor_f64 =
            |name: &str| sensor_value(name).and_then(|value| value.parse::<f64>().ok());
        if let Some(instrument) = ome.instruments.get_mut(0) {
            if instrument.microscope_model.is_none() {
                instrument.microscope_model = hdr.extra.get("sensor\ttype").cloned();
            }
            if instrument.objectives.is_empty() {
                instrument
                    .objectives
                    .push(crate::common::ome_metadata::OmeObjective::default());
            }
            if let Some(objective) = instrument.objectives.get_mut(0) {
                if objective.id.is_none() {
                    objective.id = Some(crate::common::ome_metadata::create_lsid(
                        "Objective",
                        &[0, 0],
                    ));
                }
                if objective.lens_na.is_none() {
                    objective.lens_na = sensor_f64("NumAperture").filter(|v| *v > 0.0);
                }
                if objective.immersion.is_none() {
                    objective.immersion = sensor_f64("RefrInxMedium")
                        .map(|ri| if ri > 1.1 { "Oil" } else { "Air" }.to_string());
                }
            }
            if instrument.detectors.is_empty() {
                instrument
                    .detectors
                    .push(crate::common::ome_metadata::OmeDetector::default());
            }
            if let Some(detector) = instrument.detectors.get_mut(0) {
                if detector.id.is_none() {
                    detector.id = Some(crate::common::ome_metadata::create_lsid(
                        "Detector",
                        &[0, 0],
                    ));
                }
                if detector.offset.is_none() {
                    detector.offset = sensor_f64("DetectorBaseline").filter(|v| v.is_finite());
                }
                if detector.gain.is_none() {
                    detector.gain = sensor_f64("DetectorSensitivity[0]").filter(|v| *v > 0.0);
                }
            }
        }
        let img = ome.images.get_mut(0)?;

        img.name = hdr.image_name.clone();
        img.instrument_ref = Some(0);
        img.objective_ref = Some(0);
        if img.planes.is_empty() {
            let effective_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
            for t in 0..meta.size_t.max(1) {
                for c in 0..effective_c {
                    for z in 0..meta.size_z.max(1) {
                        img.planes.push(crate::common::ome_metadata::OmePlane {
                            the_z: z,
                            the_c: c,
                            the_t: t,
                            delta_t: None,
                            exposure_time: None,
                            position_x: None,
                            position_y: None,
                            position_z: None,
                        });
                    }
                }
            }
        }
        for ch in &mut img.channels {
            ch.detector_ref = Some("Detector:0".into());
            if ch.pinhole_size.is_none() {
                ch.pinhole_size = sensor_f64("PinholeRadius").filter(|v| *v > 0.0);
            }
        }

        // Physical sizes from `parameter scale`, gated on micrometre units, with
        // axis labels taken from `layout order` (Java ICSReader uses `axes`).
        let is_micron = |u: Option<&String>| {
            matches!(
                u.map_or("", String::as_str),
                "" | "um"
                    | "microns"
                    | "micron"
                    | "micrometer"
                    | "micrometers"
                    | "micrometre"
                    | "micrometres"
            )
        };
        let time_scale_seconds = |scale: f64, u: Option<&String>| match u.map_or("", String::as_str)
        {
            "" | "ms" | "millisecond" | "milliseconds" => Some(scale / 1000.0),
            "seconds" | "second" | "s" => Some(scale),
            _ => None,
        };
        let scale_units = if hdr.scale_units.len() + 1 == hdr.scales.len() {
            let mut units = Vec::with_capacity(hdr.scales.len());
            let mut unit_index = 0usize;
            for axis in &hdr.order {
                if axis.eq_ignore_ascii_case("ch") || unit_index >= hdr.scale_units.len() {
                    units.push("nm".to_string());
                } else {
                    units.push(hdr.scale_units[unit_index].clone());
                    unit_index += 1;
                }
            }
            units
        } else {
            hdr.scale_units.clone()
        };

        for (i, &scale) in hdr.scales.iter().enumerate() {
            if scale.is_nan() {
                continue;
            }
            let axis = hdr.order.get(i).map(String::as_str).unwrap_or("");
            let unit = scale_units.get(i);
            match axis {
                "x" if is_micron(unit) => img.physical_size_x = Some(scale),
                "y" if is_micron(unit) => img.physical_size_y = Some(scale),
                "z" | "depth" if is_micron(unit) => img.physical_size_z = Some(scale),
                "t" | "time" => {
                    img.time_increment = time_scale_seconds(scale, unit);
                }
                _ => {}
            }
        }

        // Per-channel emission/excitation wavelengths.
        for (ci, ch) in img.channels.iter_mut().enumerate() {
            if let Some(&w) = hdr.em_waves.get(ci) {
                if w > 0.0 {
                    ch.emission_wavelength = Some(w);
                }
            }
            if let Some(&w) = hdr.ex_waves.get(ci) {
                if w > 0.0 {
                    ch.excitation_wavelength = Some(w);
                }
            }
        }

        for (t, timestamp) in hdr.timestamps.iter().enumerate() {
            if t >= meta.size_t as usize {
                break;
            }
            let Some(delta_t) = timestamp.filter(|v| v.is_finite()) else {
                continue;
            };
            let effective_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
            for z in 0..meta.size_z.max(1) {
                for c in 0..effective_c {
                    let plane_index = Self::plane_index_for_coords(meta, z, c, t as u32);
                    if let Some(plane) = img
                        .planes
                        .iter_mut()
                        .find(|p| p.the_z == z && p.the_c == c && p.the_t == t as u32)
                    {
                        plane.delta_t = Some(delta_t);
                    } else {
                        img.planes.push(crate::common::ome_metadata::OmePlane {
                            the_z: z,
                            the_c: c,
                            the_t: t as u32,
                            delta_t: Some(delta_t),
                            exposure_time: None,
                            position_x: None,
                            position_y: None,
                            position_z: None,
                        });
                    }
                    debug_assert!(plane_index < meta.image_count);
                }
            }
        }
        img.planes.sort_by_key(|plane| {
            Self::plane_index_for_coords(meta, plane.the_z, plane.the_c, plane.the_t)
        });

        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("bioformats_ics_{name}_{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn failed_reopen_clears_previous_reader_state() {
        let dir = tmp_dir("failed_reopen");
        let valid_ics = dir.join("valid_then_bad.ics");
        let valid_companion = dir.join("valid_then_bad.ids");
        let bad_ics = dir.join("bad_after_valid.ics");

        std::fs::write(
            &valid_ics,
            "ics_version\t1.0\nlayout\torder\tbits x y\nlayout\tsizes\t8 2 1\nlayout\tsignificant_bits\t8\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t1 2 3 4\nrepresentation\tcompression\tuncompressed\n",
        )
        .unwrap();
        std::fs::write(&valid_companion, [3, 4]).unwrap();
        std::fs::write(
            &bad_ics,
            "ics_version\t1.0\nlayout\torder\tbits x y\nlayout\tsizes\t8 1 1\nlayout\tsignificant_bits\t8\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t1 2 3 4\nrepresentation\tcompression\tuncompressed\n",
        )
        .unwrap();

        let mut reader = IcsReader::new();
        reader.set_id(&valid_ics).unwrap();
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![3, 4]);

        let err = reader.set_id(&bad_ics).unwrap_err();
        assert!(
            err.to_string().contains("IDS file not found"),
            "unexpected error: {err}"
        );
        assert_eq!(reader.metadata().size_x, 0);
        assert!(reader.ome_metadata().is_none());
        assert!(reader.open_bytes(0).is_err());

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ics1_requires_same_stem_ids_and_keeps_header_filename_as_image_name() {
        let dir = tmp_dir("same_stem");
        let ics = dir.join("same_stem_required.ics");
        let matching_companion = dir.join("same_stem_required.ids");
        let mismatched_companion = dir.join("same_stem_required_pixels.ids");

        let header = format!(
            "ics_version\t1.0\nfilename\tfolder/{}\nlayout\torder\tbits x y\nlayout\tsizes\t8 2 2\nlayout\tsignificant_bits\t8\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t1 2 3 4\nrepresentation\tcompression\tuncompressed\n",
            mismatched_companion.file_name().unwrap().to_string_lossy()
        );
        std::fs::write(&ics, header).unwrap();
        std::fs::write(&mismatched_companion, [9, 9, 9, 9]).unwrap();

        let mut reader = IcsReader::new();
        let err = reader.set_id(&ics).unwrap_err();
        assert!(
            err.to_string().contains("IDS file not found"),
            "unexpected error: {err}"
        );

        std::fs::write(&matching_companion, [1, 2, 3, 4]).unwrap();
        reader.set_id(&ics).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
        assert_eq!(reader.open_bytes_region(0, 1, 1, 1, 1).unwrap(), vec![4]);

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(
            ome.images.first().unwrap().name.as_deref(),
            mismatched_companion
                .file_name()
                .and_then(|name| name.to_str())
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn ics2_embedded_pixels_ignore_header_filename_and_need_no_companion() {
        let dir = tmp_dir("ics2_embedded");
        let ics = dir.join("embedded.ics");
        let misleading_companion = dir.join("embedded.ids");

        let header = format!(
            "ics_version\t2.0\nfilename\t{}\nlayout\torder\tbits x y\nlayout\tsizes\t8 3 2\nlayout\tsignificant_bits\t8\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t1 2 3 4\nrepresentation\tcompression\tuncompressed\nend\n",
            misleading_companion.file_name().unwrap().to_string_lossy()
        );
        let mut contents = header.into_bytes();
        contents.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
        std::fs::write(&ics, contents).unwrap();

        let mut reader = IcsReader::new();
        reader.set_id(&ics).unwrap();
        assert_eq!(reader.metadata().image_count, 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(reader.open_bytes_region(0, 1, 1, 2, 1).unwrap(), vec![5, 6]);

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(
            ome.images.first().unwrap().name.as_deref(),
            misleading_companion
                .file_name()
                .and_then(|name| name.to_str())
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn rgb_ome_planes_match_logical_image_count_and_nonzero_region() {
        let dir = tmp_dir("rgb_ome");
        let ics = dir.join("rgb_ome_planes.ics");
        let companion = dir.join("rgb_ome_planes.ids");

        let header = format!(
            "ics_version\t1.0\nfilename\t{}\nlayout\torder\tbits ch x y\nlayout\tsizes\t8 3 2 1\nlayout\tsignificant_bits\t8\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t1 2 3 4\nrepresentation\tcompression\tuncompressed\n",
            companion.file_name().unwrap().to_string_lossy()
        );
        std::fs::write(&ics, header).unwrap();
        std::fs::write(&companion, [1, 2, 3, 4, 5, 6]).unwrap();

        let mut reader = IcsReader::new();
        reader.set_id(&ics).unwrap();
        assert!(reader.metadata().is_rgb);
        assert!(reader.metadata().is_interleaved);
        assert_eq!(reader.metadata().size_c, 3);
        assert_eq!(reader.metadata().image_count, 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 1, 1).unwrap(),
            vec![4, 5, 6]
        );

        let ome = reader.ome_metadata().unwrap();
        let image = ome.images.first().unwrap();
        assert_eq!(image.channels.len(), 1);
        assert_eq!(image.channels[0].samples_per_pixel, 3);
        assert_eq!(image.planes.len(), 1);
        assert_eq!(
            (
                image.planes[0].the_z,
                image.planes[0].the_c,
                image.planes[0].the_t
            ),
            (0, 0, 0)
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn stored_rgb_with_emission_waves_becomes_separate_channel_planes() {
        let dir = tmp_dir("stored_rgb_waves");
        let ics = dir.join("stored_rgb_with_waves.ics");
        let companion = dir.join("stored_rgb_with_waves.ids");

        let header = format!(
            "ics_version\t1.0\nfilename\t{}\nlayout\torder\tbits ch x y\nlayout\tsizes\t8 2 2 1\nlayout\tsignificant_bits\t8\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t1 2 3 4\nrepresentation\tcompression\tuncompressed\nsensor\ts_params\tLambdaEm\t510 620\n",
            companion.file_name().unwrap().to_string_lossy()
        );
        std::fs::write(&ics, header).unwrap();
        std::fs::write(&companion, [1, 11, 2, 12]).unwrap();

        let mut reader = IcsReader::new();
        reader.set_id(&ics).unwrap();
        assert!(!reader.metadata().is_rgb);
        assert_eq!(reader.metadata().size_c, 2);
        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![11, 12]);
        assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(), vec![12]);

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn declared_gzip_fallback_replaces_partial_decoder_output() {
        let dir = tmp_dir("gzip_fallback");
        let ics = dir.join("fallback.ics");

        let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
        std::io::Write::write_all(&mut encoder, &[9, 8, 7, 6]).unwrap();
        let mut invalid_gzip = encoder.finish().unwrap();
        invalid_gzip.truncate(invalid_gzip.len() - 4);

        let header = format!(
            "ics_version\t2.0\nlayout\torder\tbits x y\nlayout\tsizes\t8 {} 1\nlayout\tsignificant_bits\t8\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t1 2 3 4\nrepresentation\tcompression\tgzip\nend\n",
            invalid_gzip.len()
        );
        let mut contents = header.into_bytes();
        contents.extend_from_slice(&invalid_gzip);
        std::fs::write(&ics, contents).unwrap();

        let mut reader = IcsReader::new();
        reader.set_id(&ics).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), invalid_gzip);

        let _ = std::fs::remove_dir_all(dir);
    }
}

// ---- writer -----------------------------------------------------------------

pub struct IcsWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<Vec<u8>>,
}

impl IcsWriter {
    pub fn new() -> Self {
        IcsWriter {
            path: None,
            meta: None,
            planes: Vec::new(),
        }
    }
}

impl Default for IcsWriter {
    fn default() -> Self {
        Self::new()
    }
}

fn ics_plane_coords(meta: &ImageMetadata, plane_index: u32) -> (u32, u32, u32) {
    let size_z = meta.size_z.max(1);
    let size_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
    let size_t = meta.size_t.max(1);
    let mut rem = plane_index;
    let mut z = 0;
    let mut c = 0;
    let mut t = 0;
    for axis in match meta.dimension_order {
        DimensionOrder::XYCTZ => ['C', 'T', 'Z'],
        DimensionOrder::XYCZT => ['C', 'Z', 'T'],
        DimensionOrder::XYTCZ => ['T', 'C', 'Z'],
        DimensionOrder::XYTZC => ['T', 'Z', 'C'],
        DimensionOrder::XYZCT => ['Z', 'C', 'T'],
        DimensionOrder::XYZTC => ['Z', 'T', 'C'],
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

fn ics_xyztc_index(meta: &ImageMetadata, z: u32, c: u32, t: u32) -> usize {
    let size_z = meta.size_z.max(1);
    let size_t = meta.size_t.max(1);
    ((c * size_t + t) * size_z + z) as usize
}

fn bytes_as_little_endian(meta: &ImageMetadata, data: &[u8]) -> Vec<u8> {
    let bps = meta.pixel_type.bytes_per_sample();
    if meta.is_little_endian || bps <= 1 {
        return data.to_vec();
    }
    let mut out = data.to_vec();
    for chunk in out.chunks_exact_mut(bps) {
        chunk.reverse();
    }
    out
}

fn ics_writer_byte_order(meta: &ImageMetadata) -> String {
    let bits = meta.pixel_type.bytes_per_sample() * 8;
    let bytes = (bits / 8).max(1);
    let is_float = matches!(meta.pixel_type, PixelType::Float32 | PixelType::Float64);
    // Pixel samples are normalized to little-endian before writing. Java's ICS
    // byte-order convention is inverted for integer samples at 32 bits and up.
    let ascending = bits < 32 || is_float;
    let values: Vec<String> = if ascending {
        (1..=bytes).map(|v| v.to_string()).collect()
    } else {
        (1..=bytes).rev().map(|v| v.to_string()).collect()
    };
    values.join(" ")
}

fn metadata_f64(meta: &ImageMetadata, key: &str) -> Option<f64> {
    meta.series_metadata.get(key).and_then(|v| match v {
        MetadataValue::Float(v) if v.is_finite() => Some(*v),
        MetadataValue::Int(v) => Some(*v as f64),
        MetadataValue::String(s) => s.parse::<f64>().ok().filter(|v| v.is_finite()),
        _ => None,
    })
}

fn ics_parameter_scale_and_units(meta: &ImageMetadata) -> (String, String) {
    let physical_x = metadata_f64(meta, "PhysicalSizeX").unwrap_or(1.0);
    let physical_y = metadata_f64(meta, "PhysicalSizeY").unwrap_or(1.0);
    let physical_z = metadata_f64(meta, "PhysicalSizeZ").unwrap_or(1.0);
    let time_increment = metadata_f64(meta, "TimeIncrement");

    // Java ICSWriter emits one leading bits scale, then values in its fixed
    // outputOrder ("XYZTC"), even when RGB layout has a leading "ch" axis.
    let scales = [
        "1".to_string(),
        physical_x.to_string(),
        physical_y.to_string(),
        physical_z.to_string(),
        time_increment.unwrap_or(1.0).to_string(),
        "1".to_string(),
    ];
    let mut units = vec![
        "bits".to_string(),
        "micrometers".to_string(),
        "micrometers".to_string(),
        "micrometers".to_string(),
    ];
    if time_increment.is_some() {
        units.push("seconds".to_string());
    }
    (
        scales.into_iter().collect::<Vec<_>>().join(" "),
        units.join(" "),
    )
}

fn ics_metadata_path_for_ids(path: &Path) -> PathBuf {
    match path.extension().and_then(|e| e.to_str()) {
        Some(ext) if ext.chars().next().is_some_and(|c| c.is_ascii_uppercase()) => {
            path.with_extension("ICS")
        }
        _ => path.with_extension("ics"),
    }
}

impl FormatWriter for IcsWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ics") || e.eq_ignore_ascii_case("ids"))
            .unwrap_or(false)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        self.meta = Some(meta.clone());
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta
            .as_ref()
            .ok_or_else(|| BioFormatsError::Format("set_metadata first".into()))?;
        self.path = Some(path.to_path_buf());
        self.planes.clear();
        Ok(())
    }

    fn save_bytes(&mut self, idx: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_next_plane(
            "ICS",
            meta,
            self.planes.len(),
            idx,
            data.len(),
        )?;
        self.planes.push(bytes_as_little_endian(meta, data));
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let _path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_complete("ICS", meta, self.planes.len())?;
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.take().ok_or(BioFormatsError::NotInitialized)?;
        let write_ics1_pair = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ids"))
            .unwrap_or(false);
        let metadata_path = if write_ics1_pair {
            ics_metadata_path_for_ids(&path)
        } else {
            path.clone()
        };

        // Write ICS2 as one .ics file, or Java-style ICS1 as .ics metadata
        // plus a sibling .ids pixel file when the requested path ends in .ids.
        let mut f = File::create(&metadata_path).map_err(BioFormatsError::Io)?;

        let bps = meta.pixel_type.bytes_per_sample() * 8;
        let (format_str, sign_str) = match meta.pixel_type {
            PixelType::Float32 | PixelType::Float64 => ("real", "signed"),
            PixelType::Int8 | PixelType::Int16 | PixelType::Int32 => ("integer", "signed"),
            _ => ("integer", "unsigned"),
        };

        // ICS headers use CRLF line endings. In particular Java's ICSReader v2
        // probe does readString(17).trim() == "ics_version\t2.0", which only
        // matches when this line is exactly "ics_version\t2.0\r\n" (17 bytes);
        // a bare "\n" makes Java fall back to ICS v1 and demand a .ids file.
        if write_ics1_pair {
            write!(f, "ics_version\t1.0\r\n").map_err(BioFormatsError::Io)?;
        } else {
            write!(f, "ics_version\t2.0\r\n").map_err(BioFormatsError::Io)?;
        }
        write!(
            f,
            "filename\t{}\r\n",
            path.file_name().unwrap_or_default().to_string_lossy()
        )
        .map_err(BioFormatsError::Io)?;
        let (order_parts, size_parts) = if meta.is_rgb {
            (
                vec!["bits", "ch", "x", "y", "z", "t"],
                vec![
                    bps.to_string(),
                    meta.size_c.max(1).to_string(),
                    meta.size_x.to_string(),
                    meta.size_y.to_string(),
                    meta.size_z.max(1).to_string(),
                    meta.size_t.max(1).to_string(),
                ],
            )
        } else {
            (
                vec!["bits", "x", "y", "z", "t", "ch"],
                vec![
                    bps.to_string(),
                    meta.size_x.to_string(),
                    meta.size_y.to_string(),
                    meta.size_z.max(1).to_string(),
                    meta.size_t.max(1).to_string(),
                    meta.size_c.max(1).to_string(),
                ],
            )
        };

        write!(f, "layout\tparameters\t{}\r\n", order_parts.len()).map_err(BioFormatsError::Io)?;
        write!(f, "layout\torder\t{}\r\n", order_parts.join(" ")).map_err(BioFormatsError::Io)?;
        write!(f, "layout\tsizes\t{}\r\n", size_parts.join(" ")).map_err(BioFormatsError::Io)?;
        write!(f, "layout\tsignificant_bits\t{}\r\n", bps).map_err(BioFormatsError::Io)?;
        write!(f, "representation\tformat\t{}\r\n", format_str).map_err(BioFormatsError::Io)?;
        write!(f, "representation\tsign\t{}\r\n", sign_str).map_err(BioFormatsError::Io)?;
        write!(
            f,
            "representation\tbyte_order\t{}\r\n",
            ics_writer_byte_order(&meta)
        )
        .map_err(BioFormatsError::Io)?;
        write!(f, "representation\tcompression\tuncompressed\r\n").map_err(BioFormatsError::Io)?;
        let (scale, units) = ics_parameter_scale_and_units(&meta);
        write!(f, "parameter\tscale\t{}\r\n", scale).map_err(BioFormatsError::Io)?;
        write!(f, "parameter\tunits\t{}\r\n", units).map_err(BioFormatsError::Io)?;
        // The ICS2 terminator MUST end in a bare LF, not CRLF. Java's ICSReader locates
        // the pixel-data offset for ICS v2 with `in.readString(NL)` (NL = "\r\n"),
        // which stops at the FIRST terminator character and consumes only that one
        // byte. With "end\r\n" it stops on the `\r`, leaving the trailing `\n`
        // inside the pixel stream — every plane is then shifted one byte and reads
        // garbage. Java's own ICSWriter emits "\nend\n" (LF) for exactly this
        // reason. The leading `ics_version\t2.0\r\n` line stays CRLF because the v2
        // probe reads a fixed 17 bytes and needs that line to be exactly 17 long.
        write!(f, "end\n").map_err(BioFormatsError::Io)?;
        let mut pixel_file = if write_ics1_pair {
            Some(File::create(&path).map_err(BioFormatsError::Io)?)
        } else {
            None
        };

        let mut output_planes = vec![None; self.planes.len()];
        for (input_index, plane) in self.planes.iter().enumerate() {
            let (z, c, t) = ics_plane_coords(&meta, input_index as u32);
            let output_index = ics_xyztc_index(&meta, z, c, t);
            if output_index < output_planes.len() {
                output_planes[output_index] = Some(plane);
            }
        }
        for plane in output_planes {
            let plane = plane.ok_or_else(|| {
                BioFormatsError::Format("ICS writer: internal plane reordering gap".into())
            })?;
            if let Some(pixel_file) = pixel_file.as_mut() {
                pixel_file.write_all(plane).map_err(BioFormatsError::Io)?;
            } else {
                f.write_all(plane).map_err(BioFormatsError::Io)?;
            }
        }
        self.planes.clear();
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        true
    }
}
