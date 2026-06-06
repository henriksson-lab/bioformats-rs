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
use crate::common::path::confined_join;
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

            let tokens: Vec<&str> = line.split_ascii_whitespace().collect();
            if tokens.is_empty() {
                continue;
            }

            match tokens[0].to_ascii_lowercase().as_str() {
                "ics_version" if tokens.len() >= 2 => {
                    hdr.version = parse_ics_scalar(tokens[1], "ics_version")?;
                }
                "filename" if tokens.len() >= 2 => {
                    hdr.filename = Some(PathBuf::from(tokens[1..].join(" ")));
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
                            parse_ics_scalar(tokens[2], "layout significant_bits")?;
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

fn parse_ics_list<T>(values: &[&str], field: &str) -> Result<Vec<T>>
where
    T: std::str::FromStr,
{
    values
        .iter()
        .map(|value| parse_ics_scalar(value, field))
        .collect()
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

    fn data_path(ics_path: &Path, hdr: &IcsHeader) -> Result<PathBuf> {
        if hdr.version < 2.0 {
            // ICS1: the companion .ids lives next to the .ics on disk with the
            // same stem (Bio-Formats derives it from the actual file path, not
            // from the `filename` recorded inside the header — that internal
            // name may differ from the on-disk name). Match the .ics extension
            // case so .ICS -> .IDS, .ics -> .ids.
            let derived = match ics_path
                .extension()
                .and_then(|e| e.to_str())
            {
                Some(ext) if ext.chars().next().is_some_and(|c| c.is_ascii_uppercase()) => {
                    ics_path.with_extension("IDS")
                }
                _ => ics_path.with_extension("ids"),
            };
            if derived.exists() {
                return Ok(derived);
            }
            // The sibling .ids is not present on disk; fall back to the
            // header-recorded companion name when available (it may differ from
            // the on-disk stem). Reject names that escape the image directory.
            if let Some(filename) = &hdr.filename {
                let name = filename.to_string_lossy();
                return confined_join(
                    ics_path.parent().unwrap_or_else(|| Path::new("")),
                    &name,
                )
                .ok_or_else(|| {
                    BioFormatsError::Format(format!(
                        "ICS companion filename escapes image directory: {name}"
                    ))
                });
            }
            Ok(derived)
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

    fn data_payload(&self) -> Result<Vec<u8>> {
        let hdr = self
            .header
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let ics_path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let data_path = Self::data_path(ics_path, hdr)?;
        let mut f = File::open(&data_path).map_err(BioFormatsError::Io)?;

        f.seek(SeekFrom::Start(hdr.data_offset))
            .map_err(BioFormatsError::Io)?;
        let mut data = Vec::new();
        if hdr.gzip_compressed {
            let mut dec = flate2::read::GzDecoder::new(f);
            dec.read_to_end(&mut data).map_err(BioFormatsError::Io)?;
        } else {
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
            .map(|e| e.eq_ignore_ascii_case("ics"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // ICS header starts with "ics_version" or whitespace-then-ics_version
        let s = std::str::from_utf8(&header[..header.len().min(64)]).unwrap_or("");
        s.trim_start().starts_with("ics_version")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let hdr = IcsHeader::parse(path)?;
        let meta = build_metadata(&hdr)?;
        self.path = Some(path.to_path_buf());
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

impl FormatWriter for IcsWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ics"))
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
        self.planes.push(data.to_vec());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let _path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crate::formats::stack_writer::validate_complete("ICS", meta, self.planes.len())?;
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.take().ok_or(BioFormatsError::NotInitialized)?;

        // Write ICS2 format: header + "end\r\n" + raw binary (all in one .ics file)
        let mut f = File::create(&path).map_err(BioFormatsError::Io)?;

        let bps = meta.pixel_type.bytes_per_sample() * 8;
        let (format_str, sign_str) = match meta.pixel_type {
            PixelType::Float32 | PixelType::Float64 => ("real", "signed"),
            PixelType::Int8 | PixelType::Int16 | PixelType::Int32 => ("integer", "signed"),
            _ => ("integer", "unsigned"),
        };

        writeln!(f, "ics_version\t2.0").map_err(BioFormatsError::Io)?;
        writeln!(
            f,
            "filename\t{}",
            path.file_stem().unwrap_or_default().to_string_lossy()
        )
        .map_err(BioFormatsError::Io)?;
        let mut order_parts = vec!["bits", "x", "y"];
        let mut size_parts = vec![
            bps.to_string(),
            meta.size_x.to_string(),
            meta.size_y.to_string(),
        ];
        if meta.size_z > 1 {
            order_parts.push("z");
            size_parts.push(meta.size_z.to_string());
        }
        if meta.size_t > 1 {
            order_parts.push("t");
            size_parts.push(meta.size_t.to_string());
        }
        if meta.size_c > 1 {
            order_parts.push("ch");
            size_parts.push(meta.size_c.to_string());
        }

        writeln!(f, "layout\tparameters\t{}", order_parts.len()).map_err(BioFormatsError::Io)?;
        writeln!(f, "layout\torder\t{}", order_parts.join(" ")).map_err(BioFormatsError::Io)?;
        writeln!(f, "layout\tsizes\t{}", size_parts.join(" ")).map_err(BioFormatsError::Io)?;
        writeln!(f, "layout\tsignificant_bits\t{}", bps).map_err(BioFormatsError::Io)?;
        writeln!(f, "representation\tformat\t{}", format_str).map_err(BioFormatsError::Io)?;
        writeln!(f, "representation\tsign\t{}", sign_str).map_err(BioFormatsError::Io)?;
        writeln!(f, "representation\tbyte_order\t1 2 3 4").map_err(BioFormatsError::Io)?;
        writeln!(f, "representation\tcompression\tuncompressed").map_err(BioFormatsError::Io)?;
        writeln!(f, "end\r").map_err(BioFormatsError::Io)?;

        for plane in &self.planes {
            f.write_all(plane).map_err(BioFormatsError::Io)?;
        }
        self.planes.clear();
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        true
    }
}
