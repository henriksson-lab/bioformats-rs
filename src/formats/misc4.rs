//! Readers and explicit unsupported detectors for obscure and proprietary formats.
//!
//! Partial readers decode only simple documented/raw payload cases. Formats
//! without enough structure to read pixels fail with `UnsupportedFormat` instead
//! of exposing placeholder metadata or synthetic planes.

use std::collections::HashMap;
use std::io::{ErrorKind, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::stitcher::{FilePattern, FileStitcher};

fn checked_plane_len(meta: &ImageMetadata) -> Result<usize> {
    let bytes_per_pixel = (meta.bits_per_pixel as usize)
        .checked_div(8)
        .filter(|bps| *bps > 0)
        .ok_or_else(|| BioFormatsError::Format("invalid bits per pixel".to_string()))?;
    (meta.size_x as usize)
        .checked_mul(meta.size_y as usize)
        .and_then(|px| px.checked_mul(bytes_per_pixel))
        .ok_or_else(|| BioFormatsError::Format("image plane is too large".to_string()))
}

fn crop_plane(
    plane: &[u8],
    meta: &ImageMetadata,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    if x.checked_add(w).is_none_or(|end| end > meta.size_x)
        || y.checked_add(h).is_none_or(|end| end > meta.size_y)
    {
        return Err(BioFormatsError::Format(
            "requested region is outside the image bounds".to_string(),
        ));
    }
    let bytes_per_pixel = (meta.bits_per_pixel / 8) as usize;
    let row_bytes = meta.size_x as usize * bytes_per_pixel;
    let crop_row_bytes = w as usize * bytes_per_pixel;
    let x_offset = x as usize * bytes_per_pixel;
    let mut out = Vec::with_capacity(crop_row_bytes * h as usize);
    for row in y as usize..(y + h) as usize {
        let start = row
            .checked_mul(row_bytes)
            .and_then(|base| base.checked_add(x_offset))
            .ok_or_else(|| BioFormatsError::Format("requested region is too large".to_string()))?;
        let end = start
            .checked_add(crop_row_bytes)
            .ok_or_else(|| BioFormatsError::Format("requested region is too large".to_string()))?;
        if end > plane.len() {
            return Err(BioFormatsError::Format(
                "decoded plane is shorter than expected".to_string(),
            ));
        }
        out.extend_from_slice(&plane[start..end]);
    }
    Ok(out)
}

const MISC4_STRICT_RAW_HEADER_LEN: usize = 32;

#[derive(Clone, Copy)]
struct StrictRawLayout {
    data_offset: u64,
    plane_bytes: usize,
}

fn read_u16_le(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

fn read_u32_le(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

fn read_u64_le(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
        buf[offset + 4],
        buf[offset + 5],
        buf[offset + 6],
        buf[offset + 7],
    ])
}

fn strict_raw_unsupported(format_name: &str) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "{format_name} native decoding is unsupported unless explicit strict raw data is present; refusing guessed proprietary metadata"
    ))
}

fn strict_raw_pixel_type(code: u16, format_name: &str) -> Result<PixelType> {
    match code {
        1 => Ok(PixelType::Uint8),
        2 => Ok(PixelType::Uint16),
        3 => Ok(PixelType::Float32),
        _ => Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset has unsupported pixel type code {code}"
        ))),
    }
}

fn parse_strict_raw_subset(
    path: &Path,
    magic: &[u8; 8],
    format_name: &str,
) -> Result<(ImageMetadata, StrictRawLayout)> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(err) if err.kind() == ErrorKind::NotFound => {
            return Err(strict_raw_unsupported(format_name));
        }
        Err(err) => return Err(BioFormatsError::Io(err)),
    };
    let file_len = file.metadata().map_err(BioFormatsError::Io)?.len();
    if file_len < magic.len() as u64 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} file is too short for strict raw subset magic"
        )));
    }

    let mut prefix = [0u8; 8];
    file.read_exact(&mut prefix).map_err(BioFormatsError::Io)?;
    if &prefix != magic {
        return Err(strict_raw_unsupported(format_name));
    }
    if file_len < MISC4_STRICT_RAW_HEADER_LEN as u64 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset header is truncated"
        )));
    }

    let mut header = [0u8; MISC4_STRICT_RAW_HEADER_LEN];
    header[..8].copy_from_slice(&prefix);
    file.read_exact(&mut header[8..])
        .map_err(BioFormatsError::Io)?;

    let size_x = read_u32_le(&header, 8);
    let size_y = read_u32_le(&header, 12);
    let image_count = read_u32_le(&header, 16);
    let pixel_type_code = read_u16_le(&header, 20);
    let reserved = read_u16_le(&header, 22);
    let data_offset = read_u64_le(&header, 24);
    if size_x == 0 || size_y == 0 || image_count == 0 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset dimensions must be non-zero"
        )));
    }
    if reserved != 0 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset reserved header bytes must be zero"
        )));
    }
    if data_offset < MISC4_STRICT_RAW_HEADER_LEN as u64 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset data offset points into header"
        )));
    }

    let pixel_type = strict_raw_pixel_type(pixel_type_code, format_name)?;
    let plane_bytes = (size_x as usize)
        .checked_mul(size_y as usize)
        .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample()))
        .ok_or_else(|| {
            BioFormatsError::Format(format!(
                "{format_name} strict raw subset plane size overflows"
            ))
        })?;
    let payload_bytes = (plane_bytes as u64)
        .checked_mul(image_count as u64)
        .ok_or_else(|| {
            BioFormatsError::Format(format!(
                "{format_name} strict raw subset payload size overflows"
            ))
        })?;
    let required_len = data_offset.checked_add(payload_bytes).ok_or_else(|| {
        BioFormatsError::Format(format!(
            "{format_name} strict raw subset file size overflows"
        ))
    })?;
    if file_len < required_len {
        return Err(BioFormatsError::Format(format!(
            "{format_name} strict raw subset payload is truncated: got {file_len} bytes, expected at least {required_len}"
        )));
    }

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z: 1,
        size_c: 1,
        size_t: image_count,
        pixel_type,
        bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_little_endian: true,
        ..ImageMetadata::default()
    };
    Ok((
        meta,
        StrictRawLayout {
            data_offset,
            plane_bytes,
        },
    ))
}

fn read_strict_raw_plane(
    path: &Path,
    layout: StrictRawLayout,
    plane_index: u32,
) -> Result<Vec<u8>> {
    let offset = layout
        .data_offset
        .checked_add(
            (layout.plane_bytes as u64)
                .checked_mul(plane_index as u64)
                .ok_or_else(|| {
                    BioFormatsError::Format("strict raw subset plane offset overflows".to_string())
                })?,
        )
        .ok_or_else(|| {
            BioFormatsError::Format("strict raw subset plane offset overflows".to_string())
        })?;
    let mut file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    file.seek(SeekFrom::Start(offset))
        .map_err(BioFormatsError::Io)?;
    let mut plane = vec![0u8; layout.plane_bytes];
    file.read_exact(&mut plane).map_err(BioFormatsError::Io)?;
    Ok(plane)
}

// ---------------------------------------------------------------------------
// Macro for extension-only placeholder readers
// ---------------------------------------------------------------------------
#[allow(unused_macros)]
macro_rules! placeholder_reader {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
        magic_bytes: false;
    ) => {
        $(#[$attr])*
        pub struct $name {
            path: Option<PathBuf>,
            meta: Option<ImageMetadata>,
        }

        impl $name {
            pub fn new() -> Self {
                $name { path: None, meta: None }
            }
        }

        impl Default for $name {
            fn default() -> Self { Self::new() }
        }

        impl FormatReader for $name {
            fn is_this_type_by_name(&self, path: &Path) -> bool {
                let ext = path.extension()
                    .and_then(|e| e.to_str())
                    .map(|e| e.to_ascii_lowercase());
                matches!(ext.as_deref(), $(Some($ext))|+)
            }

            fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

            fn set_id(&mut self, _path: &Path) -> Result<()> {
                Err(BioFormatsError::UnsupportedFormat(format!(
                    "{} native decoding is unsupported; refusing guessed proprietary metadata",
                    stringify!($name)
                )))
            }

            fn close(&mut self) -> Result<()> {
                self.path = None;
                self.meta = None;
                Ok(())
            }

            fn series_count(&self) -> usize { 0 }

            fn set_series(&mut self, s: usize) -> Result<()> {
                let _ = s;
                Err(BioFormatsError::NotInitialized)
            }

            fn series(&self) -> usize { 0 }

            fn metadata(&self) -> &ImageMetadata {
                self.meta.as_ref().unwrap_or(crate::common::reader::uninitialized_metadata())
            }

            fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(format!(
                    "{} native decoding is unsupported; refusing guessed proprietary metadata",
                    stringify!($name)
                )))
            }

            fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(format!(
                    "{} native decoding is unsupported; refusing guessed proprietary metadata",
                    stringify!($name)
                )))
            }

            fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(format!(
                    "{} native decoding is unsupported; refusing guessed proprietary metadata",
                    stringify!($name)
                )))
            }
        }
    };
}

// ---------------------------------------------------------------------------
// 1. Applied Precision APL
// ---------------------------------------------------------------------------
/// Olympus APL format reader (`.apl`/`.mtb`/`.tnb`).
///
/// Faithful port of Bio-Formats `loci.formats.in.APLReader`. An APL dataset is
/// an MS-Access database sidecar (`*_d.mtb`) listing one multi-page TIFF per
/// acquisition, stored in a `*_DocumentFiles` directory. Each listed TIFF
/// becomes one Bio-Formats series; dimensions come from the database row
/// (Frames/Z-Layers/Color Channels) reconciled against the TIFF's IFDs, and
/// pixel reads are delegated to [`crate::tiff::TiffReader`].
///
/// The `.mtb` is parsed via [`crate::common::mdb`] (mdbtools-rs), matching
/// Java's `MDBService`. Per-channel/physical-size OME store calls are not
/// represented in `ImageMetadata` and are omitted.
struct AplSeries {
    meta: ImageMetadata,
    tiff_path: PathBuf,
}

pub struct AplReader {
    series_list: Vec<AplSeries>,
    series: usize,
    cache: Option<(usize, crate::tiff::TiffReader)>,
}

impl AplReader {
    pub fn new() -> Self {
        AplReader {
            series_list: Vec::new(),
            series: 0,
            cache: None,
        }
    }

    fn check_suffix(name: &str, suffix: &str) -> bool {
        name.to_ascii_lowercase()
            .ends_with(&format!(".{}", suffix.to_ascii_lowercase()))
    }

    fn index_of(columns: &[String], name: &str) -> Option<usize> {
        columns.iter().position(|c| c == name)
    }

    /// Parse an integer dimension; Java falls back to 1 on parse failure.
    fn parse_dimension(s: &str) -> u32 {
        s.trim()
            .parse::<i64>()
            .map(|v| v.max(0) as u32)
            .unwrap_or(1)
    }

    fn cell<'a>(row: &'a [String], idx: Option<usize>) -> &'a str {
        idx.and_then(|i| row.get(i)).map(|s| s.trim()).unwrap_or("")
    }

    /// Port of Java `parseFilename`.
    fn parse_filename(
        row: &[String],
        filename_idx: Option<usize>,
        path_idx: Option<usize>,
    ) -> String {
        let file = Self::cell(row, filename_idx);
        if Self::check_suffix(file, "tif") {
            return file.to_string();
        }
        let file_path = Self::cell(row, path_idx).replace('\\', "/");
        file_path
            .rsplit('/')
            .next()
            .map(|s| s.trim().to_string())
            .unwrap_or_default()
    }
}

impl Default for AplReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for AplReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // Java METADATA_SUFFIXES = {apl, tnb, mtb}.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("apl") | Some("tnb") | Some("mtb"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // APL has no magic-byte signature (suffixSufficient is false but there
        // is no isThisType(stream) override either).
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.series_list.clear();
        self.series = 0;
        self.cache = None;

        let id = path.to_string_lossy().to_string();

        // -- locate the corresponding .mtb file --
        let mtb_path: PathBuf = if Self::check_suffix(&id, "mtb") {
            path.to_path_buf()
        } else if Self::check_suffix(&id, "apl") || Self::check_suffix(&id, "tnb") {
            let separator = id.rfind('/').map(|i| i as i64).unwrap_or(0);
            let mut underscore = id.rfind('_').map(|i| i as i64).unwrap_or(-1);
            if underscore < separator || Self::check_suffix(&id, "apl") {
                underscore = id.rfind('.').map(|i| i as i64).unwrap_or(-1);
            }
            if underscore < 0 {
                return Err(BioFormatsError::Format(
                    "APL: .mtb file not found".to_string(),
                ));
            }
            let mtb = format!("{}_d.mtb", &id[..underscore as usize]);
            let mtb_pb = PathBuf::from(&mtb);
            if !mtb_pb.exists() {
                return Err(BioFormatsError::Format(
                    "APL: .mtb file not found".to_string(),
                ));
            }
            mtb_pb
        } else {
            // Some other file (e.g. a .tif): look two directories up for a .mtb.
            let parent = path
                .parent()
                .and_then(|p| p.parent())
                .ok_or_else(|| BioFormatsError::Format("APL: .mtb file not found".to_string()))?;
            let mut found = None;
            for entry in std::fs::read_dir(parent).map_err(BioFormatsError::Io)? {
                let entry = entry.map_err(BioFormatsError::Io)?;
                let name = entry.file_name().to_string_lossy().to_string();
                if Self::check_suffix(&name, "mtb") {
                    found = Some(entry.path());
                    break;
                }
            }
            found.ok_or_else(|| BioFormatsError::Format("APL: .mtb file not found".to_string()))?
        };

        // -- parse the .mtb database (first table) --
        let tables = crate::common::mdb::parse_database(&mtb_path)?;
        let table = tables
            .into_iter()
            .next()
            .ok_or_else(|| BioFormatsError::Format("APL: empty .mtb database".to_string()))?;
        let columns = &table.columns;
        let data_rows = &table.rows;

        let color_channels = Self::index_of(columns, "Color Channels");
        let frames = Self::index_of(columns, "Frames");
        let path_idx =
            Self::index_of(columns, "Image Path").or_else(|| Self::index_of(columns, "Path"));
        let filename_idx = Self::index_of(columns, "File Name");
        let image_type = Self::index_of(columns, "Image Type");
        let z_layers = Self::index_of(columns, "Z-Layers");

        let parent_directory = mtb_path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        // -- find the *_DocumentFiles directory holding the TIFFs --
        // First, use the path recorded in the database rows (Java starts at
        // database row index 2, i.e. the second data row).
        let mut path_name = String::new();
        for r in 1..data_rows.len() {
            let v = Self::cell(&data_rows[r], path_idx);
            if !v.is_empty() {
                path_name = v.to_string();
                break;
            }
        }
        path_name = path_name.replace('\\', "/");

        let mut top_directory: Option<PathBuf> = None;
        for component in path_name.split('/').rev() {
            if component
                .find("_DocumentFiles")
                .map(|i| i > 0)
                .unwrap_or(false)
            {
                let candidate = parent_directory.join(component);
                if candidate.exists() {
                    top_directory = Some(candidate);
                    break;
                }
            }
        }
        if top_directory.is_none() {
            if let Ok(entries) = std::fs::read_dir(&parent_directory) {
                for entry in entries.flatten() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    if entry.path().is_dir()
                        && name.find("_DocumentFiles").map(|i| i > 0).unwrap_or(false)
                    {
                        top_directory = Some(entry.path());
                        break;
                    }
                }
            }
        }
        let top_directory = top_directory.ok_or_else(|| {
            BioFormatsError::Format("APL: could not find a directory with TIFF files".to_string())
        })?;

        // -- collect the data rows that reference an existing TIFF --
        let mut series_rows: Vec<usize> = Vec::new();
        for (di, row) in data_rows.iter().enumerate() {
            let file = Self::parse_filename(row, filename_idx, path_idx);
            if file.is_empty() {
                continue;
            }
            let full = top_directory.join(&file);
            if full.exists() && Self::check_suffix(&file, "tif") {
                series_rows.push(di);
            }
        }
        if series_rows.is_empty() {
            return Err(BioFormatsError::Format(
                "APL: no referenced TIFF files were found".to_string(),
            ));
        }

        for &di in &series_rows {
            let row3 = &data_rows[di];

            let mut size_t = 1u32;
            let mut size_z = 1u32;
            let mut size_c = 1u32;
            if frames.is_some() {
                size_t = Self::parse_dimension(Self::cell(row3, frames));
            }
            if z_layers.is_some() {
                size_z = Self::parse_dimension(Self::cell(row3, z_layers));
            }
            if color_channels.is_some() {
                size_c = Self::parse_dimension(Self::cell(row3, color_channels));
            } else if image_type.is_some() && Self::cell(row3, image_type) == "RGB" {
                size_c = 3;
            }
            if size_z == 0 {
                size_z = 1;
            }
            if size_c == 0 {
                size_c = 1;
            }
            if size_t == 0 {
                size_t = 1;
            }

            let tiff_path = top_directory.join(Self::parse_filename(row3, filename_idx, path_idx));

            // Read core metadata from the TIFF.
            let mut tiff = crate::tiff::TiffReader::new();
            tiff.set_id(&tiff_path)?;
            let tm = tiff.metadata();
            let size_x = tm.size_x;
            let size_y = tm.size_y;
            let pixel_type = tm.pixel_type;
            let little_endian = tm.is_little_endian;
            let is_rgb = tm.is_rgb;
            let image_count = tm.image_count;

            // Reconcile dimensions with the IFD count (Java's correction).
            let effective_c = if is_rgb { 1 } else { size_c };
            if effective_c > 0 && size_z * size_t * effective_c != image_count {
                size_t = image_count / effective_c;
                size_z = 1;
            }

            let meta = ImageMetadata {
                size_x,
                size_y,
                size_z,
                size_c,
                size_t,
                pixel_type,
                bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
                image_count,
                dimension_order: DimensionOrder::XYCZT,
                is_rgb,
                is_little_endian: little_endian,
                ..ImageMetadata::default()
            };
            self.series_list.push(AplSeries { meta, tiff_path });
        }

        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.series_list.clear();
        self.series = 0;
        self.cache = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series_list.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_list.len() {
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
        self.series_list
            .get(self.series)
            .map(|s| &s.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let series = self
            .series_list
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= series.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let tiff_path = series.tiff_path.clone();
        let needs_open = !matches!(&self.cache, Some((s, _)) if *s == self.series);
        if needs_open {
            let mut tiff = crate::tiff::TiffReader::new();
            tiff.set_id(&tiff_path)?;
            self.cache = Some((self.series, tiff));
        }
        let tiff = &mut self.cache.as_mut().unwrap().1;
        tiff.open_bytes(plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .series_list
            .get(self.series)
            .map(|s| s.meta.clone())
            .ok_or(BioFormatsError::NotInitialized)?;
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series_list
            .get(self.series)
            .map(|s| &s.meta)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 2. ARF format — raw uint16 heuristic
// ---------------------------------------------------------------------------
/// Axon Raw Format (ARF) reader (`.arf`).
///
/// Reads the real file header per the upstream Java ARFReader:
/// 2 endianness bytes, "AR" signature, then version/width/height/bitsPerPixel
/// as unsigned shorts. Pixel data begins at `PIXELS_OFFSET` (524).
pub struct ArfReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

const ARF_PIXELS_OFFSET: u64 = 524;

impl ArfReader {
    pub fn new() -> Self {
        ArfReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for ArfReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ArfReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("arf"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // 2 endianness bytes followed by the "AR" signature.
        if header.len() < 4 {
            return false;
        }
        let valid_endian = (header[0] == 1 && header[1] == 0) || (header[0] == 0 && header[1] == 1);
        valid_endian && &header[2..4] == b"AR"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;

        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = [0u8; 12];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        // Determine endianness from the first two bytes.
        let little = if hdr[0] == 1 && hdr[1] == 0 {
            true
        } else if hdr[0] == 0 && hdr[1] == 1 {
            false
        } else {
            return Err(BioFormatsError::InvalidData(
                "ARF: undefined endianness".to_string(),
            ));
        };

        if &hdr[2..4] != b"AR" {
            return Err(BioFormatsError::InvalidData(
                "ARF: missing 'AR' signature".to_string(),
            ));
        }

        let read_u16 = |b: &[u8]| -> u32 {
            if little {
                u16::from_le_bytes([b[0], b[1]]) as u32
            } else {
                u16::from_be_bytes([b[0], b[1]]) as u32
            }
        };

        let version = read_u16(&hdr[4..6]);
        let width = read_u16(&hdr[6..8]);
        let height = read_u16(&hdr[8..10]);
        let bits_per_pixel = read_u16(&hdr[10..12]);
        if width == 0 || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "ARF header has zero image dimensions".to_string(),
            ));
        }

        // For version 2, the image count follows; otherwise a single image.
        let num_images = if version == 2 {
            let mut nb = [0u8; 2];
            f.read_exact(&mut nb).map_err(BioFormatsError::Io)?;
            let count = read_u16(&nb);
            if count == 0 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "ARF header declares zero image count".to_string(),
                ));
            }
            count
        } else {
            1
        };

        // pixelTypeFromBytes(bpp, false, false): unsigned integer of bpp bytes.
        let mut bpp = bits_per_pixel / 8;
        if bits_per_pixel % 8 != 0 {
            bpp += 1;
        }
        let pixel_type = match bpp {
            1 => PixelType::Uint8,
            2 => PixelType::Uint16,
            4 => PixelType::Uint32,
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "ARF: unsupported bits per pixel {}",
                    bits_per_pixel
                )))
            }
        };
        let plane_bytes = (width as u64)
            .checked_mul(height as u64)
            .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample() as u64))
            .ok_or_else(|| BioFormatsError::Format("ARF image plane is too large".to_string()))?;
        let required_len = ARF_PIXELS_OFFSET
            .checked_add(plane_bytes.checked_mul(num_images as u64).ok_or_else(|| {
                BioFormatsError::Format("ARF image payload size overflows".to_string())
            })?)
            .ok_or_else(|| BioFormatsError::Format("ARF file size overflows".to_string()))?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        if file_len < required_len {
            return Err(BioFormatsError::UnsupportedFormat(
                "ARF payload is shorter than declared image dimensions".to_string(),
            ));
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: num_images,
            pixel_type,
            bits_per_pixel: bits_per_pixel as u8,
            image_count: num_images,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: little,
            resolution_count: 1,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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
        let n_bytes = checked_plane_len(meta)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(
            ARF_PIXELS_OFFSET + plane_index as u64 * n_bytes as u64,
        ))
        .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; n_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 3. I2I format
// ---------------------------------------------------------------------------
/// I2I format reader (`.i2i`).
///
/// Faithful port of Bio-Formats `loci.formats.in.I2IReader`. I2I is a simple
/// raw format with a fixed 1024-byte ASCII/binary header:
///   - byte 0: pixel-type character `'I'` (INT16), `'R'` (FLOAT32) or `'C'`
///     (complex, unsupported);
///   - byte 1: a space `' '`;
///   - bytes 2..8, 8..14, 14..20: sizeX, sizeY, sizeZ as 6-char ASCII integers;
///   - byte 20: endianness flag (`'B'` ⇒ big-endian, otherwise little-endian);
///   - then int16 min/max/x/y and the additional-dimension count `n` (`sizeT`),
///     33 reserved bytes and 15×64 history strings.
/// Pixel data starts at offset `HEADER_SIZE` (1024); planes are stored raw and
/// contiguous.
pub struct I2iReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

const I2I_HEADER_SIZE: u64 = 1024;

impl I2iReader {
    pub fn new() -> Self {
        I2iReader {
            path: None,
            meta: None,
        }
    }

    /// Parse a 6-byte ASCII dimension field, trimming whitespace. Mirrors the
    /// Java `getDimension`: a non-numeric/blank field yields 0.
    fn parse_dimension(bytes: &[u8]) -> i32 {
        String::from_utf8_lossy(bytes)
            .trim()
            .parse::<i32>()
            .unwrap_or(0)
    }
}

impl Default for I2iReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for I2iReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("i2i"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Java isThisType: requires at least HEADER_SIZE bytes, a valid pixel
        // type character, a space separator, and a positive pixel count.
        if header.len() < 20 {
            return false;
        }
        let pixel_type = header[0];
        if pixel_type != b'I' && pixel_type != b'R' && pixel_type != b'C' {
            return false;
        }
        if header[1] != b' ' {
            return false;
        }
        let sx = Self::parse_dimension(&header[2..8]) as i64;
        let sy = Self::parse_dimension(&header[8..14]) as i64;
        let sz = Self::parse_dimension(&header[14..20]) as i64;
        sx * sy * sz > 0
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;

        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut header = [0u8; 64];
        f.read_exact(&mut header).map_err(BioFormatsError::Io)?;

        let pixel_type = match header[0] {
            b'I' => PixelType::Int16,
            b'R' => PixelType::Float32,
            b'C' => {
                return Err(BioFormatsError::UnsupportedFormat(
                    "I2I complex pixel data not yet supported".to_string(),
                ))
            }
            other => {
                return Err(BioFormatsError::InvalidData(format!(
                    "I2I invalid pixel type: {}",
                    other as char
                )))
            }
        };
        if header[1] != b' ' {
            return Err(BioFormatsError::InvalidData(
                "I2I expected space after pixel type character".to_string(),
            ));
        }

        let size_x = Self::parse_dimension(&header[2..8]);
        let size_y = Self::parse_dimension(&header[8..14]);
        let mut size_z = Self::parse_dimension(&header[14..20]);

        // byte 20: endianness flag.
        let little_endian = header[20] != b'B';
        let read_i16 = |b: &[u8]| -> i16 {
            if little_endian {
                i16::from_le_bytes([b[0], b[1]])
            } else {
                i16::from_be_bytes([b[0], b[1]])
            }
        };

        // shorts at offset 21: min, max, x, y, then n.
        let n = read_i16(&header[29..31]) as i32;

        // The stored Z value is the total plane count; divide by n (the
        // additional dimension) to get the true Z count, per the Java reader.
        if n > 0 {
            size_z /= n;
        }
        let size_t = n;

        if size_x <= 0 || size_y <= 0 || size_z <= 0 || size_t <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "I2I header has non-positive dimensions".to_string(),
            ));
        }

        let size_z = size_z as u32;
        let size_t = size_t as u32;
        let image_count = size_z
            .checked_mul(size_t)
            .ok_or_else(|| BioFormatsError::Format("I2I image count overflows".to_string()))?;

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: size_x as u32,
            size_y: size_y as u32,
            size_z,
            size_c: 1,
            size_t,
            pixel_type,
            bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
            image_count,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: little_endian,
            resolution_count: 1,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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
        let plane_size = checked_plane_len(meta)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let offset = I2I_HEADER_SIZE + plane_index as u64 * plane_size as u64;

        // Java leaves the buffer zero-filled when the offset is out of range.
        let mut buf = vec![0u8; plane_size];
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        if offset + plane_size as u64 <= file_len {
            f.seek(SeekFrom::Start(offset))
                .map_err(BioFormatsError::Io)?;
            f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        }
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 4. JDCE format
// ---------------------------------------------------------------------------
/// Molecular Devices JDCE plate reader (`.jdce`).
///
/// Faithful port of Bio-Formats `loci.formats.in.JDCEReader`. A `.jdce` file is
/// a JSON description of a high-content-screening plate that references a CSV
/// image-metadata file; the CSV in turn lists one TIFF file per plane. Each
/// well/field becomes one Bio-Formats series and pixel reads are delegated to
/// the matching TIFF via [`crate::tiff::TiffReader`].
///
/// Blind translation (no sample). The plate/channel/timestamp metadata that the
/// Java reader pushes into the OME store is not represented in `ImageMetadata`
/// and is therefore parsed only as far as needed to size the planes; per-plane
/// CSV sizes fall back to the first TIFF's dimensions.
pub struct JdceReader {
    series_list: Vec<JdceSeries>,
    series: usize,
}

struct JdceSeries {
    meta: ImageMetadata,
    /// Plane index -> absolute TIFF path (None = missing plane, zero-filled).
    files: Vec<Option<String>>,
}

#[derive(Default)]
struct JdceWell {
    row: i32,
    col: i32,
    field_count: usize,
    /// (field, plane) -> absolute TIFF path.
    files: HashMap<(u32, u32), String>,
}

impl JdceReader {
    pub fn new() -> Self {
        JdceReader {
            series_list: Vec::new(),
            series: 0,
        }
    }

    /// FormatTools.getIndex for dimension order "XYCZT": C varies fastest,
    /// then Z, then T.
    fn get_index(z: u32, c: u32, t: u32, size_z: u32, size_c: u32) -> u32 {
        ((t * size_z) + z) * size_c + c
    }

    fn json_obj<'a>(v: &'a serde_json::Value, key: &str) -> Result<&'a serde_json::Value> {
        v.get(key)
            .ok_or_else(|| BioFormatsError::Format(format!("JDCE: missing JSON element \"{key}\"")))
    }
}

impl Default for JdceReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for JdceReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("jdce"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // JDCE is detected by its ".jdce" suffix (suffixSufficient in Java);
        // the payload is generic JSON with no fixed magic.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.series_list.clear();
        self.series = 0;

        let parent_dir = path
            .parent()
            .map(|p| p.to_path_buf())
            .unwrap_or_else(|| PathBuf::from("."));

        let mut json_text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        if !json_text.starts_with('{') {
            if let Some(brace) = json_text.find('{') {
                json_text = json_text[brace..].to_string();
            }
        }
        let root: serde_json::Value = serde_json::from_str(&json_text)
            .map_err(|e| BioFormatsError::Format(format!("Could not parse .jdce file: {e}")))?;

        let image_stack = Self::json_obj(&root, "ImageStack")?;
        let image_format = Self::json_obj(image_stack, "ImageFormat")?
            .as_str()
            .unwrap_or("");
        if !image_format.eq_ignore_ascii_case("TIFF") {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "JDCE unsupported image format {image_format}"
            )));
        }

        let acquisition = Self::json_obj(image_stack, "AutoLeadAcquisitionProtocol")?;
        let plate_map = Self::json_obj(acquisition, "PlateMap")?;
        let time_schedule = Self::json_obj(plate_map, "TimeSchedule")?;
        let timepoints = Self::json_obj(time_schedule, "Times")?
            .as_array()
            .ok_or_else(|| BioFormatsError::Format("JDCE: Times is not an array".to_string()))?;
        let size_t = timepoints.len().max(1) as u32;

        let z_dimension = Self::json_obj(plate_map, "ZDimensionParameters")?;
        let mut size_z = Self::json_obj(z_dimension, "NumberOfSlices")?
            .as_i64()
            .unwrap_or(1)
            .max(1) as u32;

        let wavelengths = Self::json_obj(acquisition, "Wavelengths")?
            .as_array()
            .ok_or_else(|| {
                BioFormatsError::Format("JDCE: Wavelengths is not an array".to_string())
            })?;
        let wavelength_count = wavelengths.len().max(1) as u32;

        // Reset Z to 1 when every channel is a "Max Intensity Projection".
        let mut single_z = true;
        let mut first_mode: Option<String> = None;
        for w in wavelengths {
            let mode = w.get("ImagingMode").and_then(|m| m.as_str()).unwrap_or("");
            let first = first_mode.get_or_insert_with(|| mode.to_string()).clone();
            if mode != "Max Intensity Projection" || mode != first {
                single_z = false;
            }
        }
        if single_z {
            size_z = 1;
        }

        let metadata_files = Self::json_obj(image_stack, "ImageMetadataFiles")?
            .as_array()
            .ok_or_else(|| {
                BioFormatsError::Format(
                    "JDCE: ImageMetadataFiles missing, cannot find TIFF list".to_string(),
                )
            })?;
        let csv_name = metadata_files
            .first()
            .and_then(|v| v.as_str())
            .ok_or_else(|| BioFormatsError::Format("JDCE: empty ImageMetadataFiles".to_string()))?;
        let csv_path = parent_dir.join(csv_name);

        // image_count uses the wavelength channel count (matching Java, which
        // sets ms0.imageCount before multiplying sizeC by the TIFF channels).
        let image_count = size_z * wavelength_count * size_t;

        let csv_text = std::fs::read_to_string(&csv_path).map_err(BioFormatsError::Io)?;
        let mut lines = csv_text.split("\r\n");
        let header = lines
            .next()
            .ok_or_else(|| BioFormatsError::Format("JDCE: empty CSV".to_string()))?;
        let columns: Vec<&str> = header.split(',').collect();
        let col = |name: &str| columns.iter().position(|c| *c == name);

        let well_row_index = col("Row");
        let well_col_index = col("Column");
        let field_index = col("Field");
        let wavelength_index = col("Wavelength");
        let timepoint_index = col("Timepoint");
        let z_index = col("ZIndex");
        let subfolder_index = col("ImageSubFolderPath");
        let filename_index = col("ImageFileName");
        let width_index = col("ImageSizeXPx");
        let height_index = col("ImageSizeYPx");

        let (
            well_row_index,
            well_col_index,
            field_index,
            wavelength_index,
            timepoint_index,
            z_index,
            subfolder_index,
            filename_index,
        ) = match (
            well_row_index,
            well_col_index,
            field_index,
            wavelength_index,
            timepoint_index,
            z_index,
            subfolder_index,
            filename_index,
        ) {
            (Some(a), Some(b), Some(c), Some(d), Some(e), Some(f), Some(g), Some(h)) => {
                (a, b, c, d, e, f, g, h)
            }
            _ => {
                return Err(BioFormatsError::Format(
                    "JDCE: CSV missing required columns".to_string(),
                ))
            }
        };

        let mut wells: Vec<JdceWell> = Vec::new();
        let mut size_x: u32 = 0;
        let mut size_y: u32 = 0;
        let mut pixel_type = PixelType::Uint8;
        let mut little_endian = true;
        let mut tiff_size_c: u32 = 1;
        let mut is_rgb = false;
        let mut first_file = true;

        for line in lines {
            if line.trim().is_empty() {
                continue;
            }
            let fields: Vec<&str> = line.split(',').collect();
            let get = |i: usize| fields.get(i).map(|s| s.trim()).unwrap_or("");
            let parse = |i: usize| get(i).parse::<i64>().unwrap_or(0);

            let field = parse(field_index).max(0) as u32;
            let z = parse(z_index).max(0) as u32;
            let wavelength = parse(wavelength_index).max(0) as u32;
            let timepoint = parse(timepoint_index).max(0) as u32;

            let subfolder = get(subfolder_index);
            let filename = get(filename_index);
            let image_path = parent_dir
                .join(subfolder)
                .join(filename)
                .to_string_lossy()
                .into_owned();

            let well_row = parse(well_row_index) as i32 - 1;
            let well_col = parse(well_col_index) as i32 - 1;

            let well_pos = wells
                .iter()
                .position(|w| w.row == well_row && w.col == well_col);
            let well = match well_pos {
                Some(i) => &mut wells[i],
                None => {
                    wells.push(JdceWell {
                        row: well_row,
                        col: well_col,
                        field_count: 0,
                        files: HashMap::new(),
                    });
                    wells.last_mut().unwrap()
                }
            };
            let plane = Self::get_index(z, wavelength, timepoint, size_z, wavelength_count);
            well.files.insert((field, plane), image_path.clone());
            well.field_count = well.field_count.max(field as usize + 1);

            // CSV per-plane sizes (used as fallback for the TIFF width/height).
            let csv_w = width_index
                .and_then(|i| get(i).parse::<u32>().ok())
                .unwrap_or(0);
            let csv_h = height_index
                .and_then(|i| get(i).parse::<u32>().ok())
                .unwrap_or(0);

            if first_file {
                let mut tiff = crate::tiff::TiffReader::new();
                if tiff.set_id(Path::new(&image_path)).is_ok() {
                    let m = tiff.metadata();
                    size_x = if csv_w == 0 { m.size_x } else { csv_w };
                    size_y = if csv_h == 0 { m.size_y } else { csv_h };
                    pixel_type = m.pixel_type;
                    little_endian = m.is_little_endian;
                    tiff_size_c = m.size_c.max(1);
                    is_rgb = m.is_rgb;
                    first_file = false;
                } else {
                    size_x = csv_w;
                    size_y = csv_h;
                }
            }
        }

        if wells.is_empty() {
            return Err(BioFormatsError::Format(
                "JDCE: no image entries found in CSV".to_string(),
            ));
        }
        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "JDCE: could not determine plane dimensions".to_string(),
            ));
        }

        let size_c = wavelength_count * tiff_size_c;
        let bits_per_pixel = (pixel_type.bytes_per_sample() * 8) as u8;

        // Sort wells by (row, column), then expand fields into series.
        wells.sort_by_key(|w| (w.row, w.col));
        for well in &wells {
            for field in 0..well.field_count as u32 {
                let mut files = vec![None; image_count as usize];
                for (plane, slot) in files.iter_mut().enumerate() {
                    if let Some(f) = well.files.get(&(field, plane as u32)) {
                        *slot = Some(f.clone());
                    }
                }
                let meta = ImageMetadata {
                    size_x,
                    size_y,
                    size_z,
                    size_c,
                    size_t,
                    pixel_type,
                    bits_per_pixel,
                    image_count,
                    dimension_order: DimensionOrder::XYCZT,
                    is_rgb,
                    is_interleaved: false,
                    is_indexed: false,
                    is_little_endian: little_endian,
                    resolution_count: 1,
                    series_metadata: HashMap::new(),
                    lookup_table: None,
                    modulo_z: None,
                    modulo_c: None,
                    modulo_t: None,
                };
                self.series_list.push(JdceSeries { meta, files });
            }
        }

        if self.series_list.is_empty() {
            return Err(BioFormatsError::Format("JDCE: no series found".to_string()));
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.series_list.clear();
        self.series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series_list.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_list.len() {
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
        self.series_list
            .get(self.series)
            .map(|s| &s.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let series = self
            .series_list
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let (w, h) = (series.meta.size_x, series.meta.size_y);
        self.open_bytes_region(plane_index, 0, 0, w, h)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let series = self
            .series_list
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= series.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bpp = series.meta.pixel_type.bytes_per_sample();
        let fill_len = (w as usize) * (h as usize) * bpp;
        let file = series
            .files
            .get(plane_index as usize)
            .and_then(|f| f.clone());
        if let Some(file) = file {
            let mut tiff = crate::tiff::TiffReader::new();
            if tiff.set_id(Path::new(&file)).is_ok() {
                if let Ok(bytes) = tiff.open_bytes_region(0, x, y, w, h) {
                    return Ok(bytes);
                }
            }
        }
        // Missing/unreadable plane: zero-filled, as in the Java reader.
        Ok(vec![0u8; fill_len])
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let series = self
            .series_list
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = series.meta.size_x.min(256);
        let th = series.meta.size_y.min(256);
        let tx = (series.meta.size_x - tw) / 2;
        let ty = (series.meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 5. JPX (JPEG 2000 Part 2)
// ---------------------------------------------------------------------------
/// JPX (JPEG 2000 Part 2) format reader (`.jpx`).
///
/// JPX files are JPEG 2000 Part 2; delegates to `Jpeg2000Reader`.
pub struct JpxReader {
    inner: crate::formats::misc::Jpeg2000Reader,
}

impl JpxReader {
    pub fn new() -> Self {
        JpxReader {
            inner: crate::formats::misc::Jpeg2000Reader::new(),
        }
    }
}

impl Default for JpxReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for JpxReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("jpx"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        self.inner.is_this_type_by_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)
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
}

// ---------------------------------------------------------------------------
// 6. Capture Pro Image (PCI)
// ---------------------------------------------------------------------------
/// SimplePCI / Compix `.cxd`/`.pci` format reader.
///
/// Faithful port of Bio-Formats `loci.formats.in.PCIReader`. The file is an
/// OLE2/POI compound document whose streams hold either little-endian scalar
/// metadata (8-byte doubles, ints, shorts) or pixel data (raw planes or
/// embedded TIFFs in `Bitmap*` / `Image*/Data` streams). Dimensions are derived
/// from `Image_Width`/`Image_Height`/`Image_Depth`, `Field Count`, `GroupMode`,
/// `GroupSelectedFields` and Z positions exactly as in the Java reader.
///
/// Blind translation (no sample available). Metadata parsing and raw/TIFF plane
/// reading are implemented; the `getImageIndex` field re-indexing is ported
/// literally but unverified against real data.
pub struct PciReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Plane index -> OLE2 stream path holding that plane's pixels.
    image_files: HashMap<u32, String>,
}

impl PciReader {
    pub fn new() -> Self {
        PciReader {
            path: None,
            meta: None,
            image_files: HashMap::new(),
        }
    }

    fn read_f64_le(b: &[u8]) -> Option<f64> {
        if b.len() < 8 {
            return None;
        }
        Some(f64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn read_i32_le(b: &[u8]) -> Option<i32> {
        if b.len() < 4 {
            return None;
        }
        Some(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn read_i16_le(b: &[u8]) -> Option<i16> {
        if b.len() < 2 {
            return None;
        }
        Some(i16::from_le_bytes([b[0], b[1]]))
    }

    fn pixel_type_from_bytes(bytes: i64) -> Result<PixelType> {
        Ok(match bytes {
            1 => PixelType::Uint8,
            2 => PixelType::Uint16,
            4 => PixelType::Uint32,
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "PCI unsupported sample size {bytes} bytes"
                )))
            }
        })
    }

    /// Split an OLE2 document path into (parent, trimmed last component).
    fn split_path(name: &str) -> (&str, &str) {
        match name.rfind('/') {
            Some(sep) => (&name[..sep], name[sep + 1..].trim()),
            None => ("", name.trim()),
        }
    }

    /// Port of Java's `addGlobalMeta` key derivation in `initFile`:
    /// replace path separators with spaces, then strip the `Root Entry `,
    /// `Field Data ` and `Details ` prefixes wherever they occur.
    fn global_meta_key(name: &str) -> String {
        let spaced = name.replace('/', " ");
        spaced
            .replace("Root Entry ", "")
            .replace("Field Data ", "")
            .replace("Details ", "")
    }

    /// Port of Java `getTimestampIndex`: the 1-based field number that follows
    /// the last space in the parent path, returned 0-based.
    fn timestamp_index(path: &str) -> Option<i64> {
        let space = path.rfind(' ').map(|i| i + 1)?;
        if space >= path.len() {
            return None;
        }
        let end = path[space..].find('/').map(|i| space + i)?;
        path[space..end].parse::<i64>().ok().map(|v| v - 1)
    }

    /// Port of Java `DateTools.convertDate(date, COBOL)`: `date` is milliseconds
    /// since the COBOL epoch (1582-10-15 00:00:00 UTC). Produces the Bio-Formats
    /// ISO-8601 `yyyy-MM-dd'T'HH:mm:ss` timestamp string.
    fn convert_cobol_date(date_ms: i64) -> String {
        // Milliseconds between the COBOL epoch (1582-10-15) and the Unix epoch
        // (1970-01-01). Matches Bio-Formats `DateTools.COBOL`.
        const COBOL_TO_UNIX_MS: i64 = 12_219_292_800_000;
        let unix_ms = date_ms - COBOL_TO_UNIX_MS;
        Self::format_iso8601(unix_ms)
    }

    /// Format Unix epoch milliseconds as `yyyy-MM-dd'T'HH:mm:ss` (UTC).
    fn format_iso8601(unix_ms: i64) -> String {
        let secs = unix_ms.div_euclid(1000);
        let days = secs.div_euclid(86_400);
        let tod = secs.rem_euclid(86_400);
        let (hh, mm, ss) = (tod / 3600, (tod % 3600) / 60, tod % 60);
        // Civil-from-days algorithm (Howard Hinnant), epoch 1970-01-01.
        let z = days + 719_468;
        let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = if m <= 2 { y + 1 } else { y };
        format!("{year:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}")
    }

    /// Port of Java's `Comments` parsing: each `key=value` line becomes a global
    /// metadata entry; `factor`, `magnification` and `units` adjust calibration.
    fn parse_comments(
        comments: &str,
        meta: &mut HashMap<String, MetadataValue>,
        scale_factor: &mut f64,
        magnification: &mut f64,
        units_is_pixel: &mut bool,
    ) {
        for line in comments.split('\n') {
            let Some(eq) = line.find('=') else { continue };
            let key = line[..eq].trim();
            let value = line[eq + 1..].trim();
            meta.insert(key.to_string(), MetadataValue::String(value.to_string()));

            // Java strips a trailing `;...` suffix before parsing the number.
            let trimmed = value.split(';').next().unwrap_or(value).trim();
            match key {
                "factor" => {
                    if let Ok(v) = trimmed.parse::<f64>() {
                        *scale_factor = v;
                    }
                }
                "magnification" => {
                    if let Ok(v) = trimmed.parse::<f64>() {
                        *magnification = v;
                    }
                }
                "units" => {
                    // Java only acts on `units` when a `;` is present.
                    if value.contains(';') && trimmed.eq_ignore_ascii_case("pixels") {
                        *units_is_pixel = true;
                    }
                }
                _ => {}
            }
        }
    }

    /// Port of Java `getImageIndex`.
    fn image_index(path: &str, effective_size_c: i64) -> Option<u32> {
        let space = path.rfind(' ').map(|i| i + 1)?;
        if space >= path.len() {
            return None;
        }
        let end = path[space..].find('/').map(|i| space + i)?;
        let field = &path[space..end];

        let mut image = "1".to_string();
        if let Some(pos) = path.find("Image") {
            let image_index = pos + 5;
            let end2 = path[image_index..]
                .find('/')
                .map(|i| image_index + i)
                .unwrap_or(path.len());
            image = path[image_index..end2].to_string();
        }
        let channel = image.parse::<i64>().ok()? - 1;
        let field_num = field.parse::<i64>().ok()? - 1;
        let idx = effective_size_c * field_num + channel;
        if idx < 0 {
            None
        } else {
            Some(idx as u32)
        }
    }

    /// Decode one image stream: an embedded TIFF (delegated to `TiffReader`
    /// via a temp file) or a raw planar block cropped to the requested region.
    fn read_image_stream(
        &self,
        ole: &mut crate::common::ole::OleFile,
        file: &str,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let bytes = ole.document_bytes(file)?;

        let is_tiff = bytes.len() >= 4
            && (bytes[..2] == *b"II" || bytes[..2] == *b"MM")
            && (bytes[2] == 42 || bytes[3] == 42);
        if is_tiff {
            let mut tmp = std::env::temp_dir();
            tmp.push(format!(
                "bioformats_pci_{}_{}.tif",
                std::process::id(),
                x as u64 * 131 + y as u64 * 17 + w as u64
            ));
            std::fs::write(&tmp, &bytes).map_err(BioFormatsError::Io)?;
            let mut tiff = crate::tiff::TiffReader::new();
            let result = (|| {
                tiff.set_id(&tmp)?;
                tiff.open_bytes_region(0, x, y, w, h)
            })();
            let _ = std::fs::remove_file(&tmp);
            return result;
        }

        // Raw planar block: tightly packed channel planes, one after another.
        let bpp = meta.pixel_type.bytes_per_sample();
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;
        let size_c = meta.size_c.max(1) as usize;
        let channel_plane = size_x
            .checked_mul(size_y)
            .and_then(|p| p.checked_mul(bpp))
            .ok_or_else(|| BioFormatsError::Format("PCI plane too large".to_string()))?;

        if x as usize + w as usize > size_x || y as usize + h as usize > size_y {
            return Err(BioFormatsError::Format(
                "requested region is outside the image bounds".to_string(),
            ));
        }

        let crop_row = w as usize * bpp;
        let mut out = vec![0u8; crop_row * h as usize * size_c];
        for c in 0..size_c {
            let cbase = c * channel_plane;
            for row in 0..h as usize {
                let src = cbase + ((y as usize + row) * size_x + x as usize) * bpp;
                let dst = c * crop_row * h as usize + row * crop_row;
                if src + crop_row <= bytes.len() {
                    out[dst..dst + crop_row].copy_from_slice(&bytes[src..src + crop_row]);
                }
                // Missing bytes stay zero (truncated/short streams).
            }
        }
        Ok(out)
    }
}

impl Default for PciReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PciReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pci") | Some("cxd"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // PCI files are OLE2 compound documents (magic 0xD0CF11E0...).
        crate::common::ole::is_ole2_header(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.image_files.clear();

        let mut ole = crate::common::ole::OleFile::open(path)?;
        let all_files = ole.document_list();
        if all_files.is_empty() {
            return Err(BioFormatsError::Format(
                "No files were found - the .cxd may be corrupt.".to_string(),
            ));
        }

        let mut size_x: i64 = 0;
        let mut size_y: i64 = 0;
        let mut size_z: i64 = 0;
        let mut size_c: i64 = 0;
        let mut image_count: i64 = 0;
        let mut bits_per_pixel: i64 = 0;
        let mut pixel_type = PixelType::Uint8;
        let mut mode: i32 = 0;
        let mut first_z = 0.0f64;
        let mut second_z = 0.0f64;
        let mut unique_z: Vec<f64> = Vec::new();
        let mut insertion: Vec<String> = Vec::new();
        let mut binning: i64 = 0;
        let mut creation_date: Option<String> = None;
        // Per-field/timepoint `Time_From_Start` values (Java `timestamps`).
        let mut timestamps: HashMap<i64, f64> = HashMap::new();
        // Named scalar metadata (Java `addGlobalMeta`), emitted into `series_metadata`.
        let mut series_metadata: HashMap<String, MetadataValue> = HashMap::new();
        let mut scale_factor: f64 = 1.0;
        let mut magnification: f64 = 1.0;
        // True once `units=pixels` is seen (Java leaves `units == UNITS.PIXEL`).
        let mut units_is_pixel = false;

        for name in &all_files {
            let (parent, relative) = Self::split_path(name);
            let is_image_stream =
                relative.starts_with("Bitmap") || (relative == "Data" && parent.contains("Image"));

            let stream = if !is_image_stream {
                Some(ole.document_bytes(name)?)
            } else {
                None
            };

            // Java: every non-image 8-byte stream is a named scalar double.
            // Key = full path with separators replaced by spaces, stripping the
            // `Root Entry `, `Field Data ` and `Details ` path prefixes.
            if let Some(bytes) = stream.as_deref() {
                if bytes.len() == 8 {
                    if let Some(value) = Self::read_f64_le(bytes) {
                        let key = Self::global_meta_key(name);
                        series_metadata.insert(key, MetadataValue::Float(value));
                    }
                }
            }

            if relative == "Field Count" {
                if let Some(v) = stream.as_deref().and_then(Self::read_i32_le) {
                    image_count = v as i64;
                }
            } else if relative == "File Has Image" {
                if stream.as_deref().and_then(Self::read_i16_le) == Some(0) {
                    return Err(BioFormatsError::Format(
                        "This file does not contain image data.".to_string(),
                    ));
                }
            } else if is_image_stream {
                insertion.push(name.clone());
                if size_x != 0 && size_y != 0 {
                    let bpp = pixel_type.bytes_per_sample() as i64;
                    let plane = size_x * size_y * bpp;
                    let file_size = ole.file_size(name).unwrap_or(0) as i64;
                    if plane > 0 && (size_c == 0 || size_c * plane > file_size) {
                        size_c = file_size / plane;
                    }
                }
            } else if relative.contains("Image_Depth") {
                if let Some(d) = stream.as_deref().and_then(Self::read_f64_le) {
                    let first_bits = bits_per_pixel == 0;
                    let mut bits = d as i64;
                    bits_per_pixel = bits;
                    while bits % 8 != 0 || bits == 0 {
                        bits += 1;
                    }
                    if bits % 3 == 0 {
                        size_c = 3;
                        bits /= 3;
                        bits_per_pixel /= 3;
                    }
                    bits /= 8;
                    pixel_type = Self::pixel_type_from_bytes(bits)?;
                    if size_c > 1 && first_bits && bits > 0 {
                        size_c /= bits;
                    }
                }
            } else if relative.contains("Image_Height") && size_y == 0 {
                if let Some(d) = stream.as_deref().and_then(Self::read_f64_le) {
                    size_y = d as i64;
                }
            } else if relative.contains("Image_Width") && size_x == 0 {
                if let Some(d) = stream.as_deref().and_then(Self::read_f64_le) {
                    size_x = d as i64;
                }
            } else if relative.contains("Time_From_Start") {
                if let Some(t) = stream.as_deref().and_then(Self::read_f64_le) {
                    if let Some(idx) = Self::timestamp_index(parent) {
                        timestamps.insert(idx, t);
                    }
                }
            } else if relative.ends_with("Position_Z") {
                if let Some(z) = stream.as_deref().and_then(Self::read_f64_le) {
                    if !unique_z.contains(&z) && size_z <= 1 {
                        unique_z.push(z);
                    }
                    if name.contains("Field 1/") {
                        first_z = z;
                    } else if name.contains("Field 2/") {
                        second_z = z;
                    }
                }
            } else if relative == "First Field Date & Time" {
                if let Some(d) = stream.as_deref().and_then(Self::read_f64_le) {
                    let date_ms = (d as i64).saturating_mul(1000);
                    creation_date = Some(Self::convert_cobol_date(date_ms));
                }
            } else if relative == "GroupMode" {
                if let Some(v) = stream.as_deref().and_then(Self::read_i32_le) {
                    mode = v;
                }
            } else if relative == "GroupSelectedFields" {
                size_z = stream.as_ref().map(|s| s.len() as i64 / 8).unwrap_or(0);
            } else if relative == "Binning" {
                if let Some(d) = stream.as_deref().and_then(Self::read_f64_le) {
                    binning = d as i64;
                }
            } else if relative == "Comments" {
                if let Some(bytes) = stream.as_deref() {
                    let comments = String::from_utf8_lossy(bytes);
                    Self::parse_comments(
                        &comments,
                        &mut series_metadata,
                        &mut scale_factor,
                        &mut magnification,
                        &mut units_is_pixel,
                    );
                }
            }
        }

        let z_first = (first_z - second_z).abs() > f64::EPSILON;

        if size_c == 0 {
            size_c = 1;
        }
        if mode == 0 {
            size_z = 0;
        }
        if size_z <= 1 || (size_z != 0 && image_count % size_z != 0) {
            size_z = if unique_z.is_empty() {
                1
            } else {
                unique_z.len() as i64
            };
        }
        if size_z == 0 {
            size_z = 1;
        }
        let mut size_t = image_count / size_z;
        while size_z * size_t < image_count {
            size_z += 1;
            size_t = image_count / size_z;
        }
        if size_t == 0 {
            size_t = 1;
        }

        let rgb = size_c > 1;
        if insertion.len() as i64 > image_count && size_c == 1 && image_count > 0 {
            size_c = insertion.len() as i64 / image_count;
            image_count *= size_c;
        } else {
            image_count = size_z * size_t;
        }

        if size_x <= 0 || size_y <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "PCI: could not determine image dimensions".to_string(),
            ));
        }

        let dimension_order = if z_first {
            DimensionOrder::XYCZT
        } else {
            DimensionOrder::XYCTZ
        };

        let effective_size_c = if rgb { 1 } else { size_c };
        let bpp = pixel_type.bytes_per_sample() as i64;
        let expected_plane_size = size_x * size_y * bpp * size_c;

        // Build index -> file map: insertion order first, then overlay the
        // computed field/channel index (Java overwrites the HashMap entries).
        for (i, f) in insertion.iter().enumerate() {
            self.image_files.insert(i as u32, f.clone());
        }
        for f in &insertion {
            let (parent, _) = Self::split_path(f);
            if let Some(idx) = Self::image_index(parent, effective_size_c) {
                self.image_files.insert(idx, f.clone());
            }
        }

        // Correct sizeX when a raw stream is larger than the expected plane.
        if let Some(first) = self.image_files.get(&0).cloned() {
            if let Ok(bytes) = ole.document_bytes(&first) {
                let is_tiff = bytes.len() >= 4
                    && (bytes[..2] == *b"II" || bytes[..2] == *b"MM")
                    && (bytes[2] == 42 || bytes[3] == 42);
                if !is_tiff && (bytes.len() as i64) > expected_plane_size && size_c > 0 {
                    let extra = bytes.len() as i64 - expected_plane_size;
                    let per_row = size_y * bpp * size_c;
                    if per_row > 0 {
                        size_x += extra / per_row;
                    }
                }
            }
        }

        // Java stores the following into the OME MetadataStore. `ImageMetadata`
        // has no equivalent typed fields, so capture them as named global
        // metadata under stable keys so the data is not lost.
        if let Some(date) = &creation_date {
            series_metadata.insert(
                "Acquisition Date".to_string(),
                MetadataValue::String(date.clone()),
            );
        }
        // PhysicalSizeX/Y = scaleFactor, scaled by magnification unless the
        // calibration unit is pixels (Java multiplies only when not PIXEL).
        let physical_size = if units_is_pixel {
            scale_factor
        } else {
            scale_factor * magnification
        };
        if physical_size > 0.0 {
            series_metadata.insert(
                "PhysicalSizeX".to_string(),
                MetadataValue::Float(physical_size),
            );
            series_metadata.insert(
                "PhysicalSizeY".to_string(),
                MetadataValue::Float(physical_size),
            );
        }
        if binning > 0 {
            series_metadata.insert(
                "Binning".to_string(),
                MetadataValue::String(format!("{binning}x{binning}")),
            );
        }
        // Per-plane delta-T (Java `setPlaneDeltaT`) and the derived time
        // increment between the first two acquired planes.
        for i in 0..image_count {
            if let Some(t) = timestamps.get(&i) {
                series_metadata.insert(
                    format!("DeltaT {i}"),
                    MetadataValue::Float(*t),
                );
            }
        }
        if let (Some(first), Some(second)) = (timestamps.get(&1), timestamps.get(&2)) {
            series_metadata.insert(
                "TimeIncrement".to_string(),
                MetadataValue::Float(second - first),
            );
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: size_x as u32,
            size_y: size_y as u32,
            size_z: size_z as u32,
            size_c: size_c.max(1) as u32,
            size_t: size_t as u32,
            pixel_type,
            bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
            image_count: image_count.max(0) as u32,
            dimension_order,
            is_rgb: rgb,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        let _ = bits_per_pixel;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.image_files.clear();
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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
        let (w, h) = (meta.size_x, meta.size_y);
        self.open_bytes_region(plane_index, 0, 0, w, h)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let file = self
            .image_files
            .get(&plane_index)
            .cloned()
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut ole = crate::common::ole::OleFile::open(path)?;
        self.read_image_stream(&mut ole, &file, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 7. PDS — Perkin Elmer Densitometer format
// ---------------------------------------------------------------------------
/// PDS (Perkin Elmer Densitometer) format reader.
///
/// Faithful port of Bio-Formats `loci.formats.in.PDSReader`. PDS is NOT the
/// NASA Planetary-Data-System format: it is a Perkin Elmer densitometer dataset
/// consisting of a text header (`.hdr`/`.pds`, magic ` IDENTIFICATION`) holding
/// `KEY = value / comment` lines, plus a companion binary pixel file
/// (`.IMG`/`.img`). Pixels are always UINT16 little-endian. Each on-disk row is
/// `recordWidth`-aligned: there are `pad = recordWidth - (sizeX % recordWidth)`
/// extra samples of padding after each row of `sizeX` samples. `SIGNX`/`SIGNY`
/// values of `-` request horizontal/vertical mirroring of the plane.
///
/// Relevant Java fields (PDSReader.java):
///   - magic `" IDENTIFICATION"` (15 bytes), `isThisType` (lines 76-92).
///   - `NXP`→sizeX, `NYP`→sizeY (lines 214-219).
///   - `SIGNX`/`SIGNY` (`-` ⇒ reverseX/reverseY, lines 230-235).
///   - `COLOR`: 4 ⇒ RGB sizeC=3; else sizeC=1, lutIndex=color-1 (lines 242-254).
///   - `FILE REC LEN` ⇒ recordWidth = value / 2 (lines 255-257).
///   - pixelType UINT16, littleEndian, dimensionOrder XYCZT (lines 262-267).
///   - companion `base + ".IMG"` then `base + ".img"` (lines 269-273).
///   - `openBytes` pad = recordWidth - (sizeX % recordWidth), realX/realY for
///     reverse, readPlane, then byte-swap mirroring (lines 120-162).
pub struct PdsReader {
    /// Path passed to `set_id` (the header file once resolved).
    header_path: Option<PathBuf>,
    /// Companion pixel file (`.IMG`/`.img`).
    pixels_file: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    record_width: u32,
    reverse_x: bool,
    reverse_y: bool,
}

impl PdsReader {
    pub fn new() -> Self {
        PdsReader {
            header_path: None,
            pixels_file: None,
            meta: None,
            record_width: 0,
            reverse_x: false,
            reverse_y: false,
        }
    }

    /// True if `data` begins with the PDS magic `" IDENTIFICATION"`.
    fn header_has_magic(data: &[u8]) -> bool {
        const MAGIC: &[u8] = b" IDENTIFICATION";
        data.len() >= MAGIC.len() && &data[..MAGIC.len()] == MAGIC
    }

    /// Replace the extension of `path` with `ext` (case as given). Mirrors the
    /// Java `name.substring(0, name.lastIndexOf(".")) + ext` logic.
    fn with_extension(path: &Path, ext: &str) -> Option<PathBuf> {
        let s = path.to_str()?;
        let dot = s.rfind('.')?;
        Some(PathBuf::from(format!("{}.{}", &s[..dot], ext)))
    }

    /// Resolve `path` to its header file (the `.hdr`/`.HDR` sibling when `path`
    /// is a `.img`/companion) and read its raw bytes.
    fn resolve_header(path: &Path) -> Result<(PathBuf, Vec<u8>)> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        // Java initFile: if the id is not a .hdr, look for sibling .hdr then .HDR.
        if ext.as_deref() != Some("hdr") && ext.as_deref() != Some("pds") {
            for hdr_ext in ["hdr", "HDR"] {
                if let Some(hdr) = Self::with_extension(path, hdr_ext) {
                    if hdr.exists() {
                        let data = std::fs::read(&hdr).map_err(BioFormatsError::Io)?;
                        return Ok((hdr, data));
                    }
                }
            }
            return Err(BioFormatsError::Format(
                "Could not find matching .hdr file.".to_string(),
            ));
        }
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        Ok((path.to_path_buf(), data))
    }
}

impl Default for PdsReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PdsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        match ext.as_deref() {
            // Java accepts "hdr" (and we also accept "pds") directly when the
            // file content matches; isThisType(name, open=true) defers to the
            // byte check, which we approximate by reading the header here.
            Some("hdr") | Some("pds") => std::fs::read(path)
                .map(|d| Self::header_has_magic(&d))
                .unwrap_or(false),
            // Java: for ".img", look up the sibling ".hdr" and check its magic.
            Some("img") => {
                if let Some(hdr) = Self::with_extension(path, "hdr") {
                    if hdr.exists() {
                        return std::fs::read(&hdr)
                            .map(|d| Self::header_has_magic(&d))
                            .unwrap_or(false);
                    }
                }
                if let Some(hdr) = Self::with_extension(path, "HDR") {
                    if hdr.exists() {
                        return std::fs::read(&hdr)
                            .map(|d| Self::header_has_magic(&d))
                            .unwrap_or(false);
                    }
                }
                false
            }
            _ => false,
        }
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Java isThisType(stream): the first 15 bytes equal " IDENTIFICATION".
        Self::header_has_magic(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.header_path = None;
        self.pixels_file = None;
        self.meta = None;
        self.record_width = 0;
        self.reverse_x = false;
        self.reverse_y = false;

        let (header_path, header_data) = Self::resolve_header(path)?;

        // Java splits on "\r\n"; if that yields one element, it re-splits on
        // "\r". We normalize all line endings and iterate lines.
        let header_text = String::from_utf8_lossy(&header_data);

        let mut size_x: Option<u32> = None;
        let mut size_y: Option<u32> = None;
        let mut size_c: u32 = 1;
        let mut is_rgb = false;
        let mut is_indexed = false;
        let mut record_width: u32 = 0;
        let mut reverse_x = false;
        let mut reverse_y = false;

        for raw_line in header_text.split(['\n', '\r']) {
            let line = raw_line;
            // Java: int eq = line.indexOf('='); if (eq < 0) continue;
            let Some(eq) = line.find('=') else { continue };
            // Java: int end = line.indexOf('/'); if (end < 0) end = line.length();
            let value_end = line.find('/').unwrap_or(line.len());
            if value_end < eq + 1 {
                // A '/' before '=' would make the slice invalid; skip such lines.
                continue;
            }
            let key = line[..eq].trim();
            let value = line[eq + 1..value_end].trim();

            match key {
                "NXP" => {
                    size_x = Some(value.parse::<u32>().map_err(|_| {
                        BioFormatsError::Format("PDS NXP is not a valid integer".to_string())
                    })?);
                }
                "NYP" => {
                    size_y = Some(value.parse::<u32>().map_err(|_| {
                        BioFormatsError::Format("PDS NYP is not a valid integer".to_string())
                    })?);
                }
                "SIGNX" => {
                    reverse_x = value.replace('\'', "").trim() == "-";
                }
                "SIGNY" => {
                    reverse_y = value.replace('\'', "").trim() == "-";
                }
                "COLOR" => {
                    let color = value.parse::<i32>().map_err(|_| {
                        BioFormatsError::Format("PDS COLOR is not a valid integer".to_string())
                    })?;
                    if color == 4 {
                        size_c = 3;
                        is_rgb = true;
                    } else {
                        size_c = 1;
                        is_rgb = false;
                        let lut_index = color - 1;
                        is_indexed = lut_index >= 0;
                    }
                }
                "FILE REC LEN" => {
                    record_width = value.parse::<u32>().map_err(|_| {
                        BioFormatsError::Format(
                            "PDS FILE REC LEN is not a valid integer".to_string(),
                        )
                    })? / 2;
                }
                _ => {}
            }
        }

        let size_x = size_x
            .ok_or_else(|| BioFormatsError::Format("PDS header missing NXP keyword".to_string()))?;
        let size_y = size_y
            .ok_or_else(|| BioFormatsError::Format("PDS header missing NYP keyword".to_string()))?;
        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::Format(
                "PDS NXP/NYP must be non-zero".to_string(),
            ));
        }
        // pad = recordWidth - (sizeX % recordWidth) requires recordWidth > 0.
        if record_width == 0 {
            return Err(BioFormatsError::Format(
                "PDS header missing FILE REC LEN keyword".to_string(),
            ));
        }

        // Resolve companion pixel file: base + ".IMG" then base + ".img".
        let base = Self::with_extension(&header_path, "IMG").ok_or_else(|| {
            BioFormatsError::Format("PDS header path has no extension".to_string())
        })?;
        let pixels_file = if base.exists() {
            base
        } else {
            Self::with_extension(&header_path, "img").ok_or_else(|| {
                BioFormatsError::Format("PDS header path has no extension".to_string())
            })?
        };
        if !pixels_file.exists() {
            return Err(BioFormatsError::Format(
                "PDS companion .IMG/.img pixel file not found".to_string(),
            ));
        }

        // Validate the companion file is large enough for the declared plane,
        // so truncated datasets fail in set_id like the reference would on read.
        let pad = record_width - (size_x % record_width);
        let scanline = (size_x as u64) + (pad as u64);
        let required = scanline
            .checked_mul(size_y as u64)
            .and_then(|rows| rows.checked_mul(size_c as u64))
            .and_then(|samples| samples.checked_mul(2)) // UINT16
            .ok_or_else(|| BioFormatsError::Format("PDS plane is too large".to_string()))?;
        let available = std::fs::metadata(&pixels_file)
            .map_err(BioFormatsError::Io)?
            .len();
        if available < required {
            return Err(BioFormatsError::UnsupportedFormat(
                "PDS companion file is shorter than declared image dimensions".to_string(),
            ));
        }

        self.record_width = record_width;
        self.reverse_x = reverse_x;
        self.reverse_y = reverse_y;
        self.header_path = Some(header_path);
        self.pixels_file = Some(pixels_file);
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z: 1,
            size_c,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: 1,
            // Java: dimensionOrder = "XYCZT".
            dimension_order: DimensionOrder::XYCZT,
            is_rgb,
            // Java leaves interleaved at its default (false) -> planar RGB.
            is_interleaved: false,
            is_indexed,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.header_path = None;
        self.pixels_file = None;
        self.meta = None;
        self.record_width = 0;
        self.reverse_x = false;
        self.reverse_y = false;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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
        let (size_x, size_y) = (meta.size_x, meta.size_y);
        self.open_bytes_region(plane_index, 0, 0, size_x, size_y)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let size_x = meta.size_x;
        let size_y = meta.size_y;
        let size_c = meta.size_c;
        if x.checked_add(w).is_none_or(|end| end > size_x)
            || y.checked_add(h).is_none_or(|end| end > size_y)
        {
            return Err(BioFormatsError::Format(
                "requested region is outside the image bounds".to_string(),
            ));
        }

        let pixels_file = self
            .pixels_file
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let data = std::fs::read(pixels_file).map_err(BioFormatsError::Io)?;

        let bpp = 2usize; // UINT16
                          // Java: pad = recordWidth - (sizeX % recordWidth)
        let pad = self.record_width - (size_x % self.record_width);
        let scanline = (size_x + pad) as usize; // samples per on-disk row
                                                // On-disk size (in samples) of one full padded channel plane.
        let channel_plane = scanline
            .checked_mul(size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("PDS plane is too large".to_string()))?;

        // Java: realX/realY flip the read origin when mirroring is requested.
        let real_x = if self.reverse_x { size_x - w - x } else { x };
        let real_y = if self.reverse_y { size_y - h - y } else { y };

        // Output buffer: planar, tightly packed (w*h per channel), w*bpp stride.
        let out_channel_bytes = (w as usize)
            .checked_mul(h as usize)
            .and_then(|px| px.checked_mul(bpp))
            .ok_or_else(|| BioFormatsError::Format("PDS region is too large".to_string()))?;
        let total = out_channel_bytes
            .checked_mul(size_c as usize)
            .ok_or_else(|| BioFormatsError::Format("PDS region is too large".to_string()))?;
        let mut buf = vec![0u8; total];

        // readPlane (non-interleaved): for each channel, for each row, copy a
        // contiguous run of w samples starting at realX within that on-disk row.
        for channel in 0..size_c as usize {
            let channel_base_samples = channel * channel_plane;
            for row in 0..h as usize {
                let src_sample =
                    channel_base_samples + (real_y as usize + row) * scanline + real_x as usize;
                let src_byte = src_sample * bpp;
                let run = w as usize * bpp;
                let src_end = src_byte + run;
                if src_end > data.len() {
                    return Err(BioFormatsError::Format(
                        "PDS companion file is shorter than expected".to_string(),
                    ));
                }
                let dst = channel * out_channel_bytes + row * (w as usize) * bpp;
                buf[dst..dst + run].copy_from_slice(&data[src_byte..src_end]);
            }
        }

        // Java reverseX: swap UINT16 samples within each row (per channel).
        if self.reverse_x {
            for channel in 0..size_c as usize {
                let cbase = channel * out_channel_bytes;
                for row in 0..h as usize {
                    let rbase = cbase + row * (w as usize) * bpp;
                    for col in 0..(w as usize) / 2 {
                        let begin = rbase + 2 * col;
                        let end = rbase + 2 * (w as usize - col - 1);
                        buf.swap(begin, end);
                        buf.swap(begin + 1, end + 1);
                    }
                }
            }
        }

        // Java reverseY: swap whole rows top-to-bottom (per channel).
        if self.reverse_y {
            let row_bytes = (w as usize) * bpp;
            for channel in 0..size_c as usize {
                let cbase = channel * out_channel_bytes;
                for row in 0..(h as usize) / 2 {
                    let start = cbase + row * row_bytes;
                    let end = cbase + (h as usize - row - 1) * row_bytes;
                    for k in 0..row_bytes {
                        buf.swap(start + k, end + k);
                    }
                }
            }
        }

        Ok(buf)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 8. Hiscan HIS format
// ---------------------------------------------------------------------------
/// Hamamatsu HIS format reader (`.his`).
///
/// Translated from Bio-Formats `HISReader`: each series starts with the `IM`
/// magic, a compact little-endian header, an optional semicolon-delimited
/// comment block, and then one image plane. Packed 12-bit variants are unpacked
/// to little-endian `u16` samples; byte-aligned UINT8/UINT16 grayscale and RGB
/// planes are decoded directly.
pub struct HisReader {
    path: Option<PathBuf>,
    metas: Vec<ImageMetadata>,
    pixel_offsets: Vec<u64>,
    packed_12_bit: Vec<bool>,
    current_series: usize,
}

impl HisReader {
    pub fn new() -> Self {
        HisReader {
            path: None,
            metas: Vec::new(),
            pixel_offsets: Vec::new(),
            packed_12_bit: Vec::new(),
            current_series: 0,
        }
    }

    fn current_meta(&self) -> Result<&ImageMetadata> {
        self.metas
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)
    }
}

fn unpack_his_packed_12(data: &[u8], samples: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(samples * 2);
    for sample in 0..samples {
        let mut value = 0u16;
        let bit_base = sample * 12;
        for bit_offset in 0..12 {
            let bit = bit_base + bit_offset;
            let byte = data.get(bit / 8).copied().unwrap_or(0);
            let bit_value = (byte >> (7 - (bit % 8))) & 1;
            value = (value << 1) | bit_value as u16;
        }
        out.extend_from_slice(&value.to_le_bytes());
    }
    out
}

impl Default for HisReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for HisReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("his"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 2 && &header[..2] == b"IM"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.metas.clear();
        self.pixel_offsets.clear();
        self.packed_12_bit.clear();
        self.current_series = 0;

        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 16 || &data[..2] != b"IM" {
            return Err(BioFormatsError::UnsupportedFormat(
                "HIS header missing IM magic".to_string(),
            ));
        }

        let series_count = u16::from_le_bytes([data[14], data[15]]) as usize;
        if series_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "HIS header declares zero image series".to_string(),
            ));
        }

        let mut metas: Vec<ImageMetadata> = Vec::with_capacity(series_count);
        let mut pixel_offsets: Vec<u64> = Vec::with_capacity(series_count);
        let mut packed_12_bit: Vec<bool> = Vec::with_capacity(series_count);
        let mut offset = 0usize;
        // Java HISReader.initFile (lines 129, 138-148): a series after the first
        // that does not start with the "IM" magic indicates that the previous
        // 12-bit plane was actually stored padded to 16 bits. When that happens
        // we retroactively promote the previous series to 16-bit, recompute its
        // (padded) plane size so the current series begins at the correct
        // offset, and latch `adjusted_bit_depth` so the 12-bit data types (6 and
        // 14) are treated as 16-bit for the remainder of the file.
        let mut adjusted_bit_depth = false;
        for series in 0..series_count {
            if offset.checked_add(64).is_none_or(|end| end > data.len()) {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "HIS series {series} header is truncated"
                )));
            }
            if &data[offset..offset + 2] != b"IM" {
                // Mirror Java: only the previous series being 12-bit allows us to
                // recover; otherwise the magic really is missing/corrupt.
                if series > 0 && metas[series - 1].bits_per_pixel == 12 {
                    let prev = &mut metas[series - 1];
                    prev.bits_per_pixel = 16;
                    // prevSkip = sizeX*sizeY*sizeC*12/8 (already-consumed packed
                    // plane); totalBytes = sizeX*sizeY*sizeC*2 (16-bit padded).
                    let prev_samples = (prev.size_x as u64)
                        .checked_mul(prev.size_y as u64)
                        .and_then(|px| px.checked_mul(prev.size_c as u64))
                        .ok_or_else(|| {
                            BioFormatsError::Format("HIS image plane is too large".to_string())
                        })?;
                    let prev_pixel_offset = pixel_offsets[series - 1];
                    let total_bytes = prev_samples.checked_mul(2).ok_or_else(|| {
                        BioFormatsError::Format("HIS image plane is too large".to_string())
                    })?;
                    // The previous (12-bit packed) plane is no longer valid; this
                    // series really starts after the 16-bit padded plane.
                    packed_12_bit[series - 1] = false;
                    offset = (prev_pixel_offset + total_bytes) as usize;
                    adjusted_bit_depth = true;

                    if offset.checked_add(64).is_none_or(|end| end > data.len()) {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "HIS series {series} header is truncated"
                        )));
                    }
                    if &data[offset..offset + 2] != b"IM" {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "HIS series {series} missing IM magic"
                        )));
                    }
                } else {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "HIS series {series} missing IM magic"
                    )));
                }
            }

            let comment_bytes = u16::from_le_bytes([data[offset + 2], data[offset + 3]]) as usize;
            let w = u16::from_le_bytes([data[offset + 4], data[offset + 5]]) as u32;
            let h = u16::from_le_bytes([data[offset + 6], data[offset + 7]]) as u32;
            let data_type = u16::from_le_bytes([data[offset + 12], data[offset + 13]]);
            if w == 0 || h == 0 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "HIS header is missing image dimensions".to_string(),
                ));
            }

            // Java: data types 6 and 14 are nominally 12-bit, but once a prior
            // series has been promoted (`adjusted_bit_depth`) they are stored as
            // unpacked 16-bit samples.
            let (pixel_type, bits_per_pixel, size_c, bytes_per_sample, is_packed_12) =
                match data_type {
                    1 => (PixelType::Uint8, 8u8, 1u32, 1u64, false),
                    2 => (PixelType::Uint16, 16u8, 1u32, 2u64, false),
                    6 if adjusted_bit_depth => (PixelType::Uint16, 16u8, 1u32, 2u64, false),
                    6 => (PixelType::Uint16, 12u8, 1u32, 2u64, true),
                    11 => (PixelType::Uint8, 8u8, 3u32, 1u64, false),
                    12 => (PixelType::Uint16, 16u8, 3u32, 2u64, false),
                    14 if adjusted_bit_depth => (PixelType::Uint16, 16u8, 3u32, 2u64, false),
                    14 => (PixelType::Uint16, 12u8, 3u32, 2u64, true),
                    other => {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "HIS data type {other} is not supported"
                        )));
                    }
                };

            let pixel_offset = offset
                .checked_add(64)
                .and_then(|base| base.checked_add(comment_bytes))
                .ok_or_else(|| BioFormatsError::Format("HIS header is too large".to_string()))?;
            let samples = (w as u64)
                .checked_mul(h as u64)
                .and_then(|px| px.checked_mul(size_c as u64))
                .ok_or_else(|| {
                    BioFormatsError::Format("HIS image plane is too large".to_string())
                })?;
            let plane_bytes = if is_packed_12 {
                samples
                    .checked_mul(12)
                    .and_then(|bits| bits.checked_add(7))
                    .map(|bits| bits / 8)
                    .ok_or_else(|| {
                        BioFormatsError::Format("HIS image plane is too large".to_string())
                    })?
            } else {
                samples.checked_mul(bytes_per_sample).ok_or_else(|| {
                    BioFormatsError::Format("HIS image plane is too large".to_string())
                })?
            };
            let next_offset = (pixel_offset as u64)
                .checked_add(plane_bytes)
                .ok_or_else(|| {
                    BioFormatsError::Format("HIS image plane is too large".to_string())
                })?;
            if next_offset > data.len() as u64 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "HIS payload is shorter than declared image dimensions".to_string(),
                ));
            }

            let mut series_metadata = HashMap::new();
            if comment_bytes > 0 {
                let comment_end = pixel_offset;
                let comment_start = comment_end - comment_bytes;
                let comment = String::from_utf8_lossy(&data[comment_start..comment_end]);
                for token in comment.split(';') {
                    if let Some((key, value)) = token.split_once('=') {
                        series_metadata
                            .insert(key.to_string(), MetadataValue::String(value.to_string()));
                    }
                }
            }

            metas.push(ImageMetadata {
                size_x: w,
                size_y: h,
                size_z: 1,
                size_c,
                size_t: 1,
                pixel_type,
                bits_per_pixel,
                image_count: 1,
                dimension_order: DimensionOrder::XYCZT,
                is_rgb: size_c > 1,
                is_interleaved: size_c > 1,
                is_indexed: false,
                is_little_endian: true,
                resolution_count: 1,
                series_metadata,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            });
            pixel_offsets.push(pixel_offset as u64);
            packed_12_bit.push(is_packed_12);
            offset = next_offset as usize;
        }

        self.path = Some(path.to_path_buf());
        self.metas = metas;
        self.pixel_offsets = pixel_offsets;
        self.packed_12_bit = packed_12_bit;
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.metas.clear();
        self.pixel_offsets.clear();
        self.packed_12_bit.clear();
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.metas.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.metas.is_empty() {
            Err(BioFormatsError::NotInitialized)
        } else if s >= self.metas.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }

    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.metas
            .get(self.current_series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.current_meta()?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let sample_count = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .and_then(|px| px.checked_mul(meta.size_c as usize))
            .ok_or_else(|| BioFormatsError::Format("HIS image plane is too large".to_string()))?;
        let is_packed_12 = *self
            .packed_12_bit
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let n_bytes = if is_packed_12 {
            sample_count
                .checked_mul(12)
                .and_then(|bits| bits.checked_add(7))
                .map(|bits| bits / 8)
                .ok_or_else(|| {
                    BioFormatsError::Format("HIS image plane is too large".to_string())
                })?
        } else {
            sample_count
                .checked_mul(meta.pixel_type.bytes_per_sample())
                .ok_or_else(|| {
                    BioFormatsError::Format("HIS image plane is too large".to_string())
                })?
        };
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let pixel_offset = *self
            .pixel_offsets
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(pixel_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; n_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        if is_packed_12 {
            Ok(unpack_his_packed_12(&buf, sample_count))
        } else {
            Ok(buf)
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
        let meta = self.current_meta()?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if x.checked_add(w).is_none_or(|end| end > meta.size_x)
            || y.checked_add(h).is_none_or(|end| end > meta.size_y)
        {
            return Err(BioFormatsError::Format(
                "requested region is outside the image bounds".to_string(),
            ));
        }
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        let bytes_per_pixel = meta.pixel_type.bytes_per_sample() * meta.size_c as usize;
        let row_bytes = meta.size_x as usize * bytes_per_pixel;
        let crop_row_bytes = w as usize * bytes_per_pixel;
        let x_offset = x as usize * bytes_per_pixel;
        let mut out = Vec::with_capacity(crop_row_bytes * h as usize);
        for row in y as usize..(y + h) as usize {
            let start = row
                .checked_mul(row_bytes)
                .and_then(|base| base.checked_add(x_offset))
                .ok_or_else(|| {
                    BioFormatsError::Format("requested region is too large".to_string())
                })?;
            let end = start.checked_add(crop_row_bytes).ok_or_else(|| {
                BioFormatsError::Format("requested region is too large".to_string())
            })?;
            out.extend_from_slice(&plane[start..end]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.current_meta()?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 9. HRDC GDF format
// ---------------------------------------------------------------------------
/// NOAA-HRD Gridded Data Format reader.
///
/// Faithful port of Bio-Formats `loci.formats.in.HRDGDFReader`. These are ASCII
/// files describing hurricane surface wind components produced by NOAA's
/// Hurricane Research Division. The two wind-speed components (east-west and
/// north-south) are exposed as two channels of `double` (Float64) pixels stored
/// big-endian, matching the Java reader.
pub struct HrdgdfReader {
    meta: Option<ImageMetadata>,
    /// Two channels (`[0]` = east-west, `[1]` = north-south), each `sizeX*sizeY`.
    surface_wind: Vec<Vec<f64>>,
}

const HRDGDF_MAGIC: &[u8] = b"SURFACE WIND COMPONENTS";

impl HrdgdfReader {
    pub fn new() -> Self {
        HrdgdfReader {
            meta: None,
            surface_wind: Vec::new(),
        }
    }
}

impl Default for HrdgdfReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for HrdgdfReader {
    fn is_this_type_by_name(&self, _path: &Path) -> bool {
        // Java: empty suffix, suffixSufficient=false, suffixNecessary=false.
        // Detection is purely by the magic string at the start of the file.
        false
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(HRDGDF_MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta = None;
        self.surface_wind = Vec::new();

        let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        // Java splits on the regex "[\r\n]", i.e. on each CR or LF character.
        let data: Vec<&str> = text.split(['\r', '\n']).collect();
        if data.is_empty() {
            return Err(BioFormatsError::Format("HRDGDF: empty file".to_string()));
        }

        // Header lines (metadata only; not required to build the image).
        let hurricane = data[0].rsplit(' ').next().unwrap_or("").to_string();

        // Skip ahead to the surface wind section.
        let mut line_number = 3usize;
        while line_number < data.len() && !data[line_number].starts_with("SURFACE WIND COMPONENTS")
        {
            line_number += 1;
        }
        // Consume the "SURFACE WIND COMPONENTS" marker line.
        line_number += 1;
        if line_number >= data.len() {
            return Err(BioFormatsError::Format(
                "HRDGDF: missing surface wind section".to_string(),
            ));
        }

        // Dimensions line: "X Y".
        let dims = data[line_number].trim();
        line_number += 1;
        let mut dim_iter = dims.split_whitespace();
        let size_x: u32 = dim_iter
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| BioFormatsError::Format("HRDGDF: invalid dimensions".to_string()))?;
        let size_y: u32 = dim_iter
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| BioFormatsError::Format("HRDGDF: invalid dimensions".to_string()))?;
        let n = (size_x as usize)
            .checked_mul(size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("HRDGDF: image too large".to_string()))?;

        let mut surface_wind = vec![vec![0.0f64; n]; 2];
        let mut pix_index = 0usize;
        while line_number < data.len() {
            let mut line = data[line_number];
            line_number += 1;
            while let Some(open) = line.find('(') {
                let Some(close_rel) = line[open..].find(')') else {
                    break;
                };
                let close = open + close_rel;
                let pixel = &line[open + 1..close];
                line = &line[close + 1..];
                let Some(comma) = pixel.find(',') else {
                    continue;
                };
                if pix_index >= n {
                    break;
                }
                let ew = pixel[..comma].trim().parse::<f64>().unwrap_or(0.0);
                let ns = pixel[comma + 1..].trim().parse::<f64>().unwrap_or(0.0);
                surface_wind[0][pix_index] = ew;
                surface_wind[1][pix_index] = ns;
                pix_index += 1;
            }
        }

        let mut series_metadata = HashMap::new();
        series_metadata.insert("Hurricane".to_string(), MetadataValue::String(hurricane));

        self.surface_wind = surface_wind;
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z: 1,
            size_c: 2,
            size_t: 1,
            pixel_type: PixelType::Float64,
            bits_per_pixel: 64,
            image_count: 2,
            dimension_order: DimensionOrder::XYCTZ,
            is_rgb: false,
            is_little_endian: false,
            series_metadata,
            ..ImageMetadata::default()
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.meta = None;
        self.surface_wind = Vec::new();
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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
        let (w, h) = (meta.size_x, meta.size_y);
        self.open_bytes_region(plane_index, 0, 0, w, h)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if x.checked_add(w).is_none_or(|e| e > meta.size_x)
            || y.checked_add(h).is_none_or(|e| e > meta.size_y)
        {
            return Err(BioFormatsError::Format(
                "requested region is outside the image bounds".to_string(),
            ));
        }
        let size_x = meta.size_x as usize;
        let channel = &self.surface_wind[plane_index as usize];
        let mut out = Vec::with_capacity((w as usize) * (h as usize) * 8);
        for row in y..y + h {
            for col in x..x + w {
                let v = channel[row as usize * size_x + col as usize];
                // Java: big-endian double (isLittleEndian() == false).
                out.extend_from_slice(&v.to_be_bytes());
            }
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 10. FilePatternReader - reads file patterns
// ---------------------------------------------------------------------------
/// File pattern reader (`.pattern`).
///
/// Pattern files describe a set of files to combine into a multi-dimensional
/// dataset. A bounded Java-style subset of `<...>`, `[...]`, and `{...}`
/// blocks is supported: comma-separated values and ranges with optional
/// positive steps, including nested blocks inside brace/class alternatives.
/// Simple `*` and `?` path-component globs plus bounded recursive `**`
/// directory globs are expanded from the pattern file's directory. A terminal
/// `**` is limited to files that a registered reader can plausibly open.
pub struct FilePatternReaderStub {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<StrictRawLayout>,
    stitcher: Option<FileStitcher>,
}

impl FilePatternReaderStub {
    pub fn new() -> Self {
        FilePatternReaderStub {
            path: None,
            meta: None,
            layout: None,
            stitcher: None,
        }
    }

    fn resolve_pattern_path(pattern_file: &Path, pattern: &str) -> PathBuf {
        let target = PathBuf::from(pattern);
        if target.is_absolute() {
            return target;
        }
        pattern_file
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join(target)
    }

    fn expand_pattern(pattern: &Path) -> Result<Option<ExpandedFilePattern>> {
        let text = pattern.to_string_lossy();
        if should_expand_as_glob_before_blocks(&text)? {
            let files = expand_simple_glob(pattern)?;
            let glob_pattern = normalize_path_lexically(pattern);
            let file_pattern = FilePattern::from_expanded_glob(&glob_pattern, &files)?;
            return Ok(Some(ExpandedFilePattern::Glob {
                files,
                pattern: file_pattern,
            }));
        }
        if !contains_filepattern_block(&text) {
            reject_unsupported_filepattern_syntax(&text)?;
            if contains_glob_syntax(&text) {
                let files = expand_simple_glob(pattern)?;
                let glob_pattern = normalize_path_lexically(pattern);
                let file_pattern = FilePattern::from_expanded_glob(&glob_pattern, &files)?;
                return Ok(Some(ExpandedFilePattern::Glob {
                    files,
                    pattern: file_pattern,
                }));
            }
            return Ok(None);
        }
        let mut names = Vec::new();
        expand_pattern_text(&text, &mut names)?;
        if names.is_empty() {
            return Err(BioFormatsError::Format(
                "FilePattern: expanded pattern produced no files".to_string(),
            ));
        }
        if contains_simple_glob(&text) {
            reject_mixed_file_component_globs(pattern)?;
            let mut files = Vec::new();
            for name in names {
                files.extend(expand_simple_glob(Path::new(&name))?);
            }
            files.sort();
            files.dedup();
            if files.is_empty() {
                return Err(BioFormatsError::Format(
                    "FilePattern: expanded pattern produced no files".to_string(),
                ));
            }
            let normalized_pattern =
                normalize_path_lexically(Path::new(&replace_pattern_blocks_with_globs(&text)?));
            let file_pattern = FilePattern::from_expanded_glob(&normalized_pattern, &files)?;
            return Ok(Some(ExpandedFilePattern::Glob {
                files,
                pattern: file_pattern,
            }));
        }
        let mut missing = Vec::new();
        let mut files = Vec::with_capacity(names.len());
        for name in names {
            let path = PathBuf::from(name);
            if path.exists() {
                files.push(path);
            } else {
                missing.push(path);
            }
        }
        if !missing.is_empty() {
            let shown = missing
                .iter()
                .take(3)
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ");
            let suffix = if missing.len() > 3 {
                format!(" and {} more", missing.len() - 3)
            } else {
                String::new()
            };
            return Err(BioFormatsError::Format(format!(
                "FilePattern: expanded pattern references missing files: {shown}{suffix}"
            )));
        }
        Ok(Some(ExpandedFilePattern::Explicit(files)))
    }
}

enum ExpandedFilePattern {
    Explicit(Vec<PathBuf>),
    Glob {
        files: Vec<PathBuf>,
        pattern: FilePattern,
    },
}

impl Default for FilePatternReaderStub {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for FilePatternReaderStub {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pattern"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"BFPATT\0\0")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        self.stitcher = None;

        let header = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if header.starts_with(b"BFPATT\0\0") {
            let (meta, layout) =
                parse_strict_raw_subset(path, b"BFPATT\0\0", "FilePattern synthetic raw")?;
            self.path = Some(path.to_path_buf());
            self.meta = Some(meta);
            self.layout = Some(layout);
            return Ok(());
        }

        let pattern = String::from_utf8_lossy(&header).trim().to_string();
        if pattern.is_empty() {
            return Err(BioFormatsError::Format(
                "FilePattern: pattern file is empty".to_string(),
            ));
        }
        let target = Self::resolve_pattern_path(path, &pattern);
        if target
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| e.eq_ignore_ascii_case("pattern"))
        {
            return Err(BioFormatsError::UnsupportedFormat(
                "FilePattern: nested .pattern files are not supported".to_string(),
            ));
        }

        let stitcher = if let Some(expanded) = Self::expand_pattern(&target)? {
            match expanded {
                ExpandedFilePattern::Explicit(files) => {
                    FileStitcher::from_files_with_pattern(files, &target)?
                }
                ExpandedFilePattern::Glob { files, pattern } => {
                    FileStitcher::from_files_with_file_pattern(files, pattern)?
                }
            }
        } else {
            FileStitcher::open(&target)?
        };
        self.path = Some(path.to_path_buf());
        self.stitcher = Some(stitcher);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        if let Some(mut stitcher) = self.stitcher.take() {
            let _ = stitcher.close();
        }
        Ok(())
    }

    fn series_count(&self) -> usize {
        if let Some(stitcher) = &self.stitcher {
            stitcher.series_count()
        } else {
            usize::from(self.meta.is_some())
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if let Some(stitcher) = &mut self.stitcher {
            return stitcher.set_series(s);
        }
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
        }
    }

    fn series(&self) -> usize {
        self.stitcher
            .as_ref()
            .map_or(0, |stitcher| stitcher.series())
    }

    fn metadata(&self) -> &ImageMetadata {
        if let Some(stitcher) = &self.stitcher {
            stitcher.metadata()
        } else {
            self.meta
                .as_ref()
                .unwrap_or(crate::common::reader::uninitialized_metadata())
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if let Some(stitcher) = &mut self.stitcher {
            return stitcher.open_bytes(plane_index);
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let layout = self.layout.ok_or(BioFormatsError::NotInitialized)?;
        read_strict_raw_plane(path, layout, plane_index)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if let Some(stitcher) = &mut self.stitcher {
            return stitcher.open_bytes_region(plane_index, x, y, w, h);
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if let Some(stitcher) = &mut self.stitcher {
            return stitcher.open_thumb_bytes(plane_index);
        }
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if let Some(stitcher) = &self.stitcher {
            return stitcher.ome_metadata();
        }
        let meta = self.meta.as_ref()?;
        Some(crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta))
    }
}

fn expand_pattern_text(pattern: &str, out: &mut Vec<String>) -> Result<()> {
    let Some((start, open, close)) = find_next_filepattern_block(pattern) else {
        reject_unsupported_filepattern_syntax(pattern)?;
        out.push(pattern.to_string());
        return Ok(());
    };
    let end = find_matching_filepattern_block_end(pattern, start, open, close)?;
    let prefix = &pattern[..start];
    let suffix = &pattern[end + close.len_utf8()..];
    for value in parse_pattern_block(&pattern[start + open.len_utf8()..end])? {
        let candidate = format!("{prefix}{value}{suffix}");
        expand_pattern_text(&candidate, out)?;
    }
    Ok(())
}

fn reject_mixed_file_component_globs(pattern: &Path) -> Result<()> {
    let normal_components = pattern
        .components()
        .filter_map(|component| match component {
            std::path::Component::Normal(part) => Some(part),
            _ => None,
        })
        .collect::<Vec<_>>();
    if let Some(file_component) = normal_components.last() {
        let text = file_component.to_str().ok_or_else(|| {
            BioFormatsError::Format("FilePattern: glob path is not UTF-8".to_string())
        })?;
        if contains_simple_glob(text) {
            return Err(BioFormatsError::UnsupportedFormat(
                "FilePattern: mixing pattern blocks with glob wildcards is only supported for directory components"
                    .to_string(),
            ));
        }
    }
    Ok(())
}

fn replace_pattern_blocks_with_globs(pattern: &str) -> Result<String> {
    let Some((start, open, close)) = find_next_filepattern_block(pattern) else {
        reject_unsupported_filepattern_syntax(pattern)?;
        return Ok(pattern.to_string());
    };
    let end = find_matching_filepattern_block_end(pattern, start, open, close)?;
    let _ = parse_pattern_block(&pattern[start + open.len_utf8()..end])?;
    let suffix = replace_pattern_blocks_with_globs(&pattern[end + close.len_utf8()..])?;
    Ok(format!("{}*{suffix}", &pattern[..start]))
}

fn reject_unsupported_filepattern_syntax(pattern: &str) -> Result<()> {
    for ch in ['>', ']', '}'] {
        if pattern.contains(ch) {
            return Err(BioFormatsError::Format(format!(
                "FilePattern: unmatched pattern block delimiter {ch}"
            )));
        }
    }
    Ok(())
}

fn contains_filepattern_block(pattern: &str) -> bool {
    find_next_filepattern_block(pattern).is_some()
}

fn find_next_filepattern_block(pattern: &str) -> Option<(usize, char, char)> {
    pattern.char_indices().find_map(|(idx, ch)| match ch {
        '<' => Some((idx, '<', '>')),
        '[' => Some((idx, '[', ']')),
        '{' => Some((idx, '{', '}')),
        _ => None,
    })
}

fn find_matching_filepattern_block_end(
    pattern: &str,
    start: usize,
    open: char,
    close: char,
) -> Result<usize> {
    let mut stack = vec![close];
    for (rel_idx, ch) in pattern[start + open.len_utf8()..].char_indices() {
        let idx = start + open.len_utf8() + rel_idx;
        match ch {
            '<' => stack.push('>'),
            '[' => stack.push(']'),
            '{' => stack.push('}'),
            '>' | ']' | '}' => {
                if stack.pop() != Some(ch) {
                    return Err(BioFormatsError::Format(format!(
                        "FilePattern: unmatched pattern block delimiter {ch}"
                    )));
                }
                if stack.is_empty() {
                    return Ok(idx);
                }
            }
            _ => {}
        }
    }
    Err(BioFormatsError::Format(format!(
        "FilePattern: unterminated {open}{close} pattern block"
    )))
}

fn contains_simple_glob(pattern: &str) -> bool {
    pattern.contains('*') || pattern.contains('?')
}

fn contains_glob_syntax(pattern: &str) -> bool {
    contains_simple_glob(pattern) || contains_shell_glob_class(pattern)
}

fn should_expand_as_glob_before_blocks(pattern: &str) -> Result<bool> {
    if !contains_simple_glob(pattern) {
        return Ok(false);
    }
    if pattern.contains('<') || pattern.contains('{') {
        return Ok(false);
    }
    let mut saw_class = false;
    let mut idx = 0usize;
    while let Some(rel_start) = pattern[idx..].find('[') {
        let start = idx + rel_start;
        let Some(rel_end) = pattern[start + 1..].find(']') else {
            return Ok(false);
        };
        let end = start + 1 + rel_end;
        if !is_shell_glob_class_body(&pattern[start + 1..end]) {
            return Ok(false);
        }
        saw_class = true;
        idx = end + 1;
    }
    Ok(saw_class)
}

fn contains_shell_glob_class(pattern: &str) -> bool {
    let mut idx = 0usize;
    while let Some(rel_start) = pattern[idx..].find('[') {
        let start = idx + rel_start;
        if let Some(rel_end) = pattern[start + 1..].find(']') {
            let end = start + 1 + rel_end;
            if is_shell_glob_class_body(&pattern[start + 1..end]) {
                return true;
            }
            idx = end + 1;
        } else {
            return false;
        }
    }
    false
}

fn is_shell_glob_class_body(body: &str) -> bool {
    if body.is_empty()
        || body.contains(',')
        || body.contains(':')
        || body.contains('<')
        || body.contains('>')
        || body.contains('{')
        || body.contains('}')
        || body.contains('[')
        || body.contains(']')
    {
        return false;
    }
    let body = body
        .strip_prefix('!')
        .or_else(|| body.strip_prefix('^'))
        .unwrap_or(body);
    if body.is_empty() {
        return false;
    }
    if let Some((first, last)) = body.split_once('-') {
        return first.chars().count() == 1 && last.chars().count() == 1;
    }
    true
}

const MAX_RECURSIVE_GLOB_ENTRIES: usize = 4096;

fn expand_simple_glob(pattern: &Path) -> Result<Vec<PathBuf>> {
    if !contains_glob_syntax(&pattern.to_string_lossy()) {
        return Ok(vec![pattern.to_path_buf()]);
    }

    let mut components = pattern.components();
    let mut roots = Vec::new();
    let mut globs = Vec::new();
    for component in components.by_ref() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                if globs.is_empty() {
                    roots.push(PathBuf::from(component.as_os_str()));
                } else {
                    return Err(BioFormatsError::Format(
                        "FilePattern: invalid glob path".to_string(),
                    ));
                }
            }
            std::path::Component::CurDir => {
                if globs.is_empty() && roots.is_empty() {
                    roots.push(PathBuf::from("."));
                }
            }
            std::path::Component::ParentDir => {
                if globs.is_empty() {
                    if roots.is_empty() {
                        roots.push(PathBuf::from(".."));
                    } else if let Some(root) = roots.last_mut() {
                        root.push("..");
                    }
                } else {
                    globs.push("..".to_string());
                }
            }
            std::path::Component::Normal(part) => {
                let text = part.to_str().ok_or_else(|| {
                    BioFormatsError::Format("FilePattern: glob path is not UTF-8".to_string())
                })?;
                globs.push(text.to_string());
            }
        }
    }
    if roots.is_empty() {
        roots.push(PathBuf::from("."));
    }
    let globs = collapse_adjacent_recursive_glob_components(globs);

    let mut paths = roots;
    let mut confinement_roots: Option<Vec<PathBuf>> = None;
    let mut saw_wildcard = false;
    for (idx, component_pattern) in globs.iter().enumerate() {
        let last = idx + 1 == globs.len();
        if component_pattern == ".." && saw_wildcard {
            paths = expand_parent_after_glob_component(
                paths,
                confinement_roots.as_deref().unwrap_or(&[]),
            )?;
            continue;
        }
        if !saw_wildcard && is_glob_component(component_pattern) {
            confinement_roots = Some(
                paths
                    .iter()
                    .map(|path| normalize_path_lexically(path))
                    .collect(),
            );
            saw_wildcard = true;
        }
        paths = expand_glob_component(paths, component_pattern, last)?;
        if paths.is_empty() {
            break;
        }
    }
    let mut files: Vec<PathBuf> = paths
        .into_iter()
        .filter(|path| path.is_file())
        .map(|path| normalize_path_lexically(&path))
        .collect();
    files.sort();
    files.dedup();
    if files.is_empty() {
        return Err(BioFormatsError::Format(format!(
            "FilePattern: glob pattern matched no files: {}",
            pattern.display()
        )));
    }
    Ok(files)
}

fn collapse_adjacent_recursive_glob_components(globs: Vec<String>) -> Vec<String> {
    let mut collapsed = Vec::with_capacity(globs.len());
    for glob in globs {
        if glob == "**" && collapsed.last().is_some_and(|previous| previous == "**") {
            continue;
        }
        collapsed.push(glob);
    }
    collapsed
}

fn is_glob_component(component: &str) -> bool {
    component == "**" || contains_glob_syntax(component)
}

fn expand_parent_after_glob_component(
    bases: Vec<PathBuf>,
    confinement_roots: &[PathBuf],
) -> Result<Vec<PathBuf>> {
    let mut parents = Vec::new();
    for base in bases {
        let candidate = base.join("..");
        let normalized = normalize_path_lexically(&candidate);
        if !path_is_confined_to_roots(&normalized, confinement_roots) {
            return Err(BioFormatsError::UnsupportedFormat(
                "FilePattern: parent directory traversal after a glob escapes the pattern root"
                    .to_string(),
            ));
        }
        parents.push(candidate);
    }
    parents.sort();
    parents.dedup();
    Ok(parents)
}

fn path_is_confined_to_roots(path: &Path, roots: &[PathBuf]) -> bool {
    roots
        .iter()
        .any(|root| path == root || path.starts_with(root))
}

fn normalize_path_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::Prefix(_) | std::path::Component::RootDir => {
                normalized.push(component.as_os_str());
            }
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                if !normalized.pop() {
                    normalized.push("..");
                }
            }
            std::path::Component::Normal(part) => normalized.push(part),
        }
    }
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

fn expand_glob_component(
    bases: Vec<PathBuf>,
    component_pattern: &str,
    last: bool,
) -> Result<Vec<PathBuf>> {
    if component_pattern == "**" {
        return expand_recursive_glob_component(bases, last);
    }

    if !contains_glob_syntax(component_pattern) {
        return Ok(bases
            .into_iter()
            .map(|mut base| {
                base.push(component_pattern);
                base
            })
            .collect());
    }

    let mut matches = Vec::new();
    for base in bases {
        let entries = match std::fs::read_dir(&base) {
            Ok(entries) => entries,
            Err(err) if err.kind() == ErrorKind::NotFound => continue,
            Err(err) => return Err(BioFormatsError::Io(err)),
        };
        for entry in entries {
            let entry = entry.map_err(BioFormatsError::Io)?;
            let file_type = entry.file_type().map_err(BioFormatsError::Io)?;
            if !last && !file_type.is_dir() {
                continue;
            }
            if last && !file_type.is_file() {
                continue;
            }
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if simple_glob_matches(component_pattern, name) {
                matches.push(entry.path());
            }
        }
    }
    matches.sort();
    Ok(matches)
}

fn expand_recursive_glob_component(bases: Vec<PathBuf>, last: bool) -> Result<Vec<PathBuf>> {
    let mut matches = Vec::new();
    for base in bases {
        collect_recursive_glob_paths(&base, last, &mut matches)?;
    }
    matches.sort();
    matches.dedup();
    if last && matches.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "FilePattern: recursive ** glob matched no supported reader files".to_string(),
        ));
    }
    Ok(matches)
}

fn collect_recursive_glob_paths(
    base: &Path,
    files: bool,
    matches: &mut Vec<PathBuf>,
) -> Result<()> {
    if matches.len() >= MAX_RECURSIVE_GLOB_ENTRIES {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "FilePattern: recursive ** glob exceeded {MAX_RECURSIVE_GLOB_ENTRIES} entries"
        )));
    }

    if files {
        if base.is_file() {
            if is_supported_recursive_glob_file(base) {
                matches.push(base.to_path_buf());
            }
            return Ok(());
        }
    } else if base.is_dir() {
        matches.push(base.to_path_buf());
    }

    let entries = match std::fs::read_dir(base) {
        Ok(entries) => entries,
        Err(err) if err.kind() == ErrorKind::NotFound => return Ok(()),
        Err(err) => return Err(BioFormatsError::Io(err)),
    };
    for entry in entries {
        let entry = entry.map_err(BioFormatsError::Io)?;
        let file_type = entry.file_type().map_err(BioFormatsError::Io)?;
        if file_type.is_dir() {
            collect_recursive_glob_paths(&entry.path(), files, matches)?;
        } else if files && is_supported_recursive_glob_file(&entry.path()) {
            matches.push(entry.path());
            if matches.len() >= MAX_RECURSIVE_GLOB_ENTRIES {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "FilePattern: recursive ** glob exceeded {MAX_RECURSIVE_GLOB_ENTRIES} entries"
                )));
            }
        }
    }
    Ok(())
}

fn is_supported_recursive_glob_file(path: &Path) -> bool {
    if path
        .extension()
        .and_then(|ext| ext.to_str())
        .is_some_and(|ext| ext.eq_ignore_ascii_case("pattern"))
    {
        return false;
    }

    let header = crate::common::io::peek_header(path, 512).unwrap_or_default();
    crate::registry::all_readers_pub()
        .into_iter()
        .any(|reader| reader.is_this_type_by_bytes(&header) || reader.is_this_type_by_name(path))
}

fn simple_glob_matches(pattern: &str, name: &str) -> bool {
    let pattern = pattern.as_bytes();
    let name = name.as_bytes();
    let mut p = 0usize;
    let mut n = 0usize;
    let mut star = None;
    let mut after_star_name = 0usize;

    while n < name.len() {
        if p < pattern.len() && (pattern[p] == b'?' || pattern[p] == name[n]) {
            p += 1;
            n += 1;
        } else if p < pattern.len() && pattern[p] == b'[' {
            let Some((end, matched)) = glob_bracket_class_matches(pattern, p, name[n]) else {
                return false;
            };
            if matched {
                p = end + 1;
                n += 1;
            } else if let Some(star_pos) = star {
                p = star_pos + 1;
                after_star_name += 1;
                n = after_star_name;
            } else {
                return false;
            }
        } else if p < pattern.len() && pattern[p] == b'*' {
            star = Some(p);
            p += 1;
            after_star_name = n;
        } else if let Some(star_pos) = star {
            p = star_pos + 1;
            after_star_name += 1;
            n = after_star_name;
        } else {
            return false;
        }
    }

    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

fn glob_bracket_class_matches(pattern: &[u8], start: usize, byte: u8) -> Option<(usize, bool)> {
    let mut idx = start + 1;
    if idx >= pattern.len() {
        return None;
    }
    let negated = pattern[idx] == b'!' || pattern[idx] == b'^';
    if negated {
        idx += 1;
    }

    let mut matched = false;
    let mut saw_member = false;
    while idx < pattern.len() {
        if pattern[idx] == b']' && saw_member {
            return Some((idx, if negated { !matched } else { matched }));
        }

        let first = pattern[idx];
        saw_member = true;
        if idx + 2 < pattern.len() && pattern[idx + 1] == b'-' && pattern[idx + 2] != b']' {
            let last = pattern[idx + 2];
            let (lo, hi) = if first <= last {
                (first, last)
            } else {
                (last, first)
            };
            if lo <= byte && byte <= hi {
                matched = true;
            }
            idx += 3;
        } else {
            if first == byte {
                matched = true;
            }
            idx += 1;
        }
    }
    None
}

fn parse_pattern_block(block: &str) -> Result<Vec<String>> {
    if block.trim().is_empty() {
        return Err(BioFormatsError::Format(
            "FilePattern: empty pattern block".to_string(),
        ));
    }

    let mut values = Vec::new();
    for part in split_top_level_pattern_commas(block)? {
        let part = part.trim();
        if part.is_empty() {
            return Err(BioFormatsError::Format(
                "FilePattern: empty pattern list entry".to_string(),
            ));
        }
        values.extend(parse_pattern_block_part(part)?);
    }
    Ok(values)
}

fn parse_pattern_block_part(part: &str) -> Result<Vec<String>> {
    if contains_filepattern_block(part) {
        let mut nested = Vec::new();
        expand_pattern_text(part, &mut nested)?;
        for value in &nested {
            reject_pattern_value(value)?;
        }
        return Ok(nested);
    }

    let (range, step) = match part.split_once(':') {
        Some((range, step)) => (
            range,
            step.parse::<i64>().map_err(|_| {
                BioFormatsError::Format("FilePattern: invalid range step".to_string())
            })?,
        ),
        None => (part, 1),
    };
    if step <= 0 {
        return Err(BioFormatsError::Format(
            "FilePattern: range step must be positive".to_string(),
        ));
    }

    if let Some((first_text, last_text)) = range.split_once('-') {
        let width = first_text.len().max(last_text.len());
        if let (Ok(first), Ok(last)) = (first_text.parse::<i64>(), last_text.parse::<i64>()) {
            let mut out = Vec::new();
            let mut value = first;
            if first <= last {
                while value <= last {
                    out.push(format!("{value:0width$}"));
                    value += step;
                }
            } else {
                while value >= last {
                    out.push(format!("{value:0width$}"));
                    value -= step;
                }
            }
            return Ok(out);
        }

        if first_text.chars().count() == 1 && last_text.chars().count() == 1 {
            let first = first_text.chars().next().unwrap();
            let last = last_text.chars().next().unwrap();
            if first.is_ascii_alphabetic() && last.is_ascii_alphabetic() {
                let mut out = Vec::new();
                let mut value = first as i64;
                let last = last as i64;
                if value <= last {
                    while value <= last {
                        out.push(char::from_u32(value as u32).unwrap().to_string());
                        value += step;
                    }
                } else {
                    while value >= last {
                        out.push(char::from_u32(value as u32).unwrap().to_string());
                        value -= step;
                    }
                }
                return Ok(out);
            }
        }

        return Err(BioFormatsError::Format(
            "FilePattern: invalid range bounds".to_string(),
        ));
    }

    reject_pattern_value(range)?;
    Ok(vec![range.to_string()])
}

fn split_top_level_pattern_commas(block: &str) -> Result<Vec<&str>> {
    let mut parts = Vec::new();
    let mut start = 0usize;
    let mut stack = Vec::new();
    for (idx, ch) in block.char_indices() {
        match ch {
            '<' => stack.push('>'),
            '[' => stack.push(']'),
            '{' => stack.push('}'),
            '>' | ']' | '}' => {
                if stack.pop() != Some(ch) {
                    return Err(BioFormatsError::Format(format!(
                        "FilePattern: unmatched pattern block delimiter {ch}"
                    )));
                }
            }
            ',' if stack.is_empty() => {
                parts.push(&block[start..idx]);
                start = idx + ch.len_utf8();
            }
            _ => {}
        }
    }
    if let Some(close) = stack.pop() {
        return Err(BioFormatsError::Format(format!(
            "FilePattern: unterminated pattern block delimiter {close}"
        )));
    }
    parts.push(&block[start..]);
    Ok(parts)
}

fn reject_pattern_value(value: &str) -> Result<()> {
    if value.is_empty()
        || value.contains('<')
        || value.contains('>')
        || value.contains('[')
        || value.contains(']')
        || value.contains('{')
        || value.contains('}')
        || value.contains('/')
        || value.contains('\\')
    {
        return Err(BioFormatsError::Format(
            "FilePattern: invalid value".to_string(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 12. KLB (Keller Lab Block) format
// ---------------------------------------------------------------------------
/// KLB (Keller Lab Block) format reader (`.klb`).
///
/// Faithful port of Bio-Formats `loci.formats.in.KLBReader` (the single-file
/// case). KLB stores a 5D (x, y, z, c, t) volume as a grid of independently
/// compressed blocks. The header gives the dimensions, block size, pixel type
/// and compression scheme; an array of cumulative block end-offsets follows.
/// Each block is compressed with zlib, bzip2 or stored raw.
///
/// Only the single-file layout is implemented (Java's `isGroupFiles` multi-file
/// channel/timepoint grouping is not). For a single file the Java reader forces
/// `sizeC = sizeT = 1`, so the plane count equals `sizeZ`.
const KLB_DATA_DIMS: usize = 5;
const KLB_METADATA_SIZE: usize = 256;
const KLB_COMPRESSION_NONE: u8 = 0;
const KLB_COMPRESSION_BZIP2: u8 = 1;
const KLB_COMPRESSION_ZLIB: u8 = 2;

#[derive(Clone)]
struct KlbLayout {
    block_size: [u32; KLB_DATA_DIMS],
    compression_type: u8,
    header_size: u64,
    /// Cumulative end-offsets (relative to `header_size`) of each compressed
    /// block, one per block in x-fastest then y, z, c, t order.
    block_offsets: Vec<u64>,
    blocks_per_plane: usize,
}

pub struct KlbReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    layout: Option<KlbLayout>,
}

impl KlbReader {
    pub fn new() -> Self {
        KlbReader {
            path: None,
            meta: None,
            layout: None,
        }
    }

    /// Port of `KLBReader.convertPixelType`. Note the Java quirk that FLOAT64
    /// maps to the 32-bit FormatTools.FLOAT and (U)INT64 map to DOUBLE.
    fn convert_pixel_type(code: u8) -> Result<PixelType> {
        Ok(match code {
            0 => PixelType::Uint8,
            1 => PixelType::Uint16,
            2 => PixelType::Uint32,
            3 | 7 => PixelType::Float64, // UINT64 / INT64 -> DOUBLE
            4 => PixelType::Int8,
            5 => PixelType::Int16,
            6 => PixelType::Int32,
            8 | 9 => PixelType::Float32, // FLOAT32 / FLOAT64 -> FLOAT
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "KLB unknown pixel type: {other}"
                )))
            }
        })
    }

    fn div_ceil_u32(a: u32, b: u32) -> u32 {
        if b == 0 {
            0
        } else {
            a.div_ceil(b)
        }
    }
}

impl Default for KlbReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for KlbReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // Java sets suffixSufficient=true: KLB is recognised by its extension.
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("klb"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // KLB has no magic-byte signature (the file starts with a version byte);
        // upstream relies on the suffix.
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;

        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;

        // -- readHeader --
        let mut byte = [0u8; 1];
        f.read_exact(&mut byte).map_err(BioFormatsError::Io)?; // headerVersion

        let mut buf32 = [0u8; 4];
        let mut read_u32 = |f: &mut std::fs::File| -> Result<u32> {
            f.read_exact(&mut buf32).map_err(BioFormatsError::Io)?;
            Ok(u32::from_le_bytes(buf32))
        };
        let mut dims_xyzct = [0u32; KLB_DATA_DIMS];
        for d in dims_xyzct.iter_mut() {
            *d = read_u32(&mut f)?;
        }

        let size_x = dims_xyzct[0];
        let size_y = dims_xyzct[1];
        let size_z = dims_xyzct[2];
        // Single-file case: Java forces sizeC = sizeT = 1.
        let size_c = 1u32;
        let size_t = 1u32;
        if size_x == 0 || size_y == 0 || size_z == 0 {
            return Err(BioFormatsError::Format(
                "KLB header has zero image dimensions".to_string(),
            ));
        }

        // dims_pixelSize: 5 x float32 (parsed but unused here)
        f.seek(SeekFrom::Current((KLB_DATA_DIMS * 4) as i64))
            .map_err(BioFormatsError::Io)?;

        f.read_exact(&mut byte).map_err(BioFormatsError::Io)?;
        let pixel_type = Self::convert_pixel_type(byte[0])?;

        f.read_exact(&mut byte).map_err(BioFormatsError::Io)?;
        let compression_type = byte[0];
        if !matches!(
            compression_type,
            KLB_COMPRESSION_NONE | KLB_COMPRESSION_BZIP2 | KLB_COMPRESSION_ZLIB
        ) {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "KLB unsupported compression type {compression_type}"
            )));
        }

        // user_metadata
        f.seek(SeekFrom::Current(KLB_METADATA_SIZE as i64))
            .map_err(BioFormatsError::Io)?;

        let mut block_size = [0u32; KLB_DATA_DIMS];
        for b in block_size.iter_mut() {
            *b = read_u32(&mut f)?;
        }
        if block_size.iter().any(|&b| b == 0) {
            return Err(BioFormatsError::Format(
                "KLB header has zero block size".to_string(),
            ));
        }

        let blocks_per_plane = (Self::div_ceil_u32(size_x, block_size[0]) as usize)
            .checked_mul(Self::div_ceil_u32(size_y, block_size[1]) as usize)
            .ok_or_else(|| BioFormatsError::Format("KLB block count overflows".to_string()))?;

        let mut num_blocks: u64 = 1;
        for i in 0..KLB_DATA_DIMS {
            num_blocks = num_blocks
                .checked_mul(Self::div_ceil_u32(dims_xyzct[i], block_size[i]) as u64)
                .ok_or_else(|| BioFormatsError::Format("KLB block count overflows".to_string()))?;
        }
        let num_blocks_usize = usize::try_from(num_blocks)
            .map_err(|_| BioFormatsError::Format("KLB block count overflows".to_string()))?;

        let header_size = (KLB_DATA_DIMS as u64 * 12)
            .checked_add(2)
            .and_then(|v| v.checked_add(num_blocks.checked_mul(8)?))
            .and_then(|v| v.checked_add(KLB_METADATA_SIZE as u64))
            .and_then(|v| v.checked_add(1))
            .ok_or_else(|| BioFormatsError::Format("KLB header size overflows".to_string()))?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        if file_len < header_size {
            return Err(BioFormatsError::Format(format!(
                "KLB header is truncated: got {file_len} bytes, expected at least {header_size}"
            )));
        }

        // The cumulative block end-offsets follow immediately (offsetFilePointer).
        let mut offset_bytes = vec![0u8; num_blocks_usize * 8];
        f.read_exact(&mut offset_bytes)
            .map_err(BioFormatsError::Io)?;
        let block_offsets: Vec<u64> = offset_bytes
            .chunks_exact(8)
            .map(|c| u64::from_le_bytes(c.try_into().unwrap()))
            .collect();
        let payload_len = file_len - header_size;
        let mut prev = 0u64;
        for &end in &block_offsets {
            if end < prev {
                return Err(BioFormatsError::Format(
                    "KLB block offset table is not monotonic".to_string(),
                ));
            }
            if end > payload_len {
                return Err(BioFormatsError::Format(format!(
                    "KLB block offset table points past EOF: block end {end}, payload {payload_len}"
                )));
            }
            prev = end;
        }

        let image_count = size_z
            .checked_mul(size_c)
            .and_then(|v| v.checked_mul(size_t))
            .ok_or_else(|| BioFormatsError::Format("KLB image count overflows".to_string()))?;

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_little_endian: true,
            ..ImageMetadata::default()
        });
        self.layout = Some(KlbLayout {
            block_size,
            compression_type,
            header_size,
            block_offsets,
            blocks_per_plane,
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.layout = None;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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
        let layout = self
            .layout
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let bpp = meta.pixel_type.bytes_per_sample();
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;
        let size_z = meta.size_z;
        let bs0 = layout.block_size[0] as usize;
        let bs1 = layout.block_size[1] as usize;
        let bs2 = layout.block_size[2];

        // Single-file: c = t = 0, so the plane index is the z coordinate.
        let z = plane_index;
        if z >= size_z {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let z_block = (z / bs2) as usize;
        let local_z = (z % bs2) as usize;

        let blocks_per_row = Self::div_ceil_u32(meta.size_x, layout.block_size[0]) as usize;
        let blocks_per_col = Self::div_ceil_u32(meta.size_y, layout.block_size[1]) as usize;
        let blocks_per_plane = layout.blocks_per_plane;

        let out_len = size_x
            .checked_mul(size_y)
            .and_then(|px| px.checked_mul(bpp))
            .ok_or_else(|| BioFormatsError::Format("KLB plane is too large".to_string()))?;
        let mut out = vec![0u8; out_len];
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;

        for by in 0..blocks_per_col {
            for bx in 0..blocks_per_row {
                let block_in_plane = by * blocks_per_row + bx;
                let global_block = z_block * blocks_per_plane + block_in_plane;
                if global_block >= layout.block_offsets.len() {
                    return Err(BioFormatsError::Format(
                        "KLB block offset index out of range".to_string(),
                    ));
                }

                let x0 = bx * bs0;
                let y0 = by * bs1;
                let bw = bs0.min(size_x - x0); // actual block width
                let bh = bs1.min(size_y - y0); // actual block height

                let start = if global_block == 0 {
                    0
                } else {
                    layout.block_offsets[global_block - 1]
                };
                let end = layout.block_offsets[global_block];
                if end < start {
                    return Err(BioFormatsError::Format(
                        "KLB block has negative size".to_string(),
                    ));
                }
                let comp_len = (end - start) as usize;

                f.seek(SeekFrom::Start(layout.header_size + start))
                    .map_err(BioFormatsError::Io)?;
                let mut compressed = vec![0u8; comp_len];
                f.read_exact(&mut compressed).map_err(BioFormatsError::Io)?;

                let block = match layout.compression_type {
                    KLB_COMPRESSION_NONE => compressed,
                    KLB_COMPRESSION_ZLIB => crate::common::codec::decompress_deflate(&compressed)?,
                    KLB_COMPRESSION_BZIP2 => {
                        // The on-disk block is a standard "BZh" stream; our
                        // decoder consumes it whole (unlike Java's Ant
                        // CBZip2InputStream, which strips the first two bytes).
                        crate::common::codec::decompress_bzip2(&compressed)?
                    }
                    other => {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "KLB unsupported compression type {other}"
                        )))
                    }
                };

                // Within the decompressed block, planes are compacted to the
                // actual (bw x bh) extent; the wanted z-slice is at local_z.
                let plane_bytes = bw
                    .checked_mul(bh)
                    .and_then(|px| px.checked_mul(bpp))
                    .ok_or_else(|| BioFormatsError::Format("KLB block is too large".to_string()))?;
                let slice_off = local_z
                    .checked_mul(plane_bytes)
                    .ok_or_else(|| BioFormatsError::Format("KLB block is too large".to_string()))?;
                let row_bytes = bw
                    .checked_mul(bpp)
                    .ok_or_else(|| BioFormatsError::Format("KLB block is too large".to_string()))?;
                if slice_off
                    .checked_add(plane_bytes)
                    .is_none_or(|end| end > block.len())
                {
                    return Err(BioFormatsError::Format(
                        "KLB decompressed block is shorter than expected".to_string(),
                    ));
                }
                for r in 0..bh {
                    let src = slice_off + r * row_bytes;
                    let dst = ((y0 + r) * size_x + x0) * bpp;
                    if dst + row_bytes <= out.len() {
                        out[dst..dst + row_bytes].copy_from_slice(&block[src..src + row_bytes]);
                    }
                }
            }
        }

        Ok(out)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let meta = meta.clone();
        let plane = self.open_bytes(plane_index)?;
        crop_plane(&plane, &meta, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ---------------------------------------------------------------------------
// 13. OBF (Imspector OBF)
// ---------------------------------------------------------------------------
const OBF_FILE_MAGIC: &[u8] = b"OMAS_BF\n";
const OBF_STACK_MAGIC: &[u8] = b"OMAS_BF_STACK\n";
const OBF_MAGIC_NUMBER: u16 = 0xFFFF;
const OBF_MAX_DIMS: usize = 15;

/// One OBF stack (= one Bio-Formats series).
struct ObfStack {
    /// File offset where this stack's pixel data begins.
    position: u64,
    compression: bool,
    samples_written: i64,
    bytes_per_sample: usize,
    flush_points: Option<Vec<u64>>,
    flush_block_size: u64,
    chunk_logical_positions: Option<Vec<u64>>,
    chunk_file_positions: Option<Vec<u64>>,
}

/// Little-endian sequential reader over the OBF file used during parsing.
struct ObfIn {
    file: std::fs::File,
}

impl ObfIn {
    fn seek(&mut self, p: u64) -> Result<()> {
        self.file
            .seek(SeekFrom::Start(p))
            .map_err(BioFormatsError::Io)?;
        Ok(())
    }
    fn pos(&mut self) -> Result<u64> {
        self.file.stream_position().map_err(BioFormatsError::Io)
    }
    fn read_n(&mut self, n: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; n];
        self.file
            .read_exact(&mut buf)
            .map_err(BioFormatsError::Io)?;
        Ok(buf)
    }
    fn skip(&mut self, n: u64) -> Result<()> {
        self.file
            .seek(SeekFrom::Current(n as i64))
            .map_err(BioFormatsError::Io)?;
        Ok(())
    }
    fn u16(&mut self) -> Result<u16> {
        let b = self.read_n(2)?;
        Ok(u16::from_le_bytes([b[0], b[1]]))
    }
    fn i32(&mut self) -> Result<i32> {
        let b = self.read_n(4)?;
        Ok(i32::from_le_bytes([b[0], b[1], b[2], b[3]]))
    }
    fn i64(&mut self) -> Result<i64> {
        let b = self.read_n(8)?;
        Ok(i64::from_le_bytes([
            b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7],
        ]))
    }
    fn f64(&mut self) -> Result<f64> {
        Ok(f64::from_bits(self.i64()? as u64))
    }
}

/// Decode state for one frame read, mirroring OBFReader.State.
struct ObfReadState {
    file: std::fs::File,
    pos: u64,
    next_read_position: i64,
    current_chunk: i64,
    chunk_logical_start: u64,
    chunk_file_start: u64,
    chunk_size: u64,
    decompress: Option<flate2::Decompress>,
    inflate_input: Vec<u8>,
    inflate_in_cursor: usize,
    inflate_in_len: usize,
}

impl ObfReadState {
    fn new(file: std::fs::File) -> Self {
        ObfReadState {
            file,
            pos: 0,
            next_read_position: -1,
            current_chunk: -1,
            chunk_logical_start: 0,
            chunk_file_start: 0,
            chunk_size: 0,
            decompress: None,
            inflate_input: Vec::new(),
            inflate_in_cursor: 0,
            inflate_in_len: 0,
        }
    }

    fn seek(&mut self, p: u64) -> Result<()> {
        self.pos = p;
        self.file
            .seek(SeekFrom::Start(p))
            .map_err(BioFormatsError::Io)?;
        Ok(())
    }

    fn remaining_in_chunk(&self) -> i64 {
        (self.chunk_file_start + self.chunk_size) as i64 - self.pos as i64
    }

    fn switch_chunk(&mut self, stack: &ObfStack, chunk_index: i64) -> Result<()> {
        let logical = stack
            .chunk_logical_positions
            .as_ref()
            .ok_or_else(|| BioFormatsError::Format("OBF: missing chunk positions".to_string()))?;
        let filepos = stack.chunk_file_positions.as_ref().unwrap();
        if chunk_index < 0 || chunk_index as usize >= logical.len() {
            return Err(BioFormatsError::Format(
                "Missing OBF data chunks".to_string(),
            ));
        }
        let ci = chunk_index as usize;
        self.current_chunk = chunk_index;
        self.chunk_logical_start = logical[ci];
        self.chunk_file_start = filepos[ci] + stack.position;
        let stack_byte_count = stack.samples_written as u64 * stack.bytes_per_sample as u64;
        if ci + 1 == logical.len() {
            self.chunk_size = stack_byte_count.saturating_sub(self.chunk_logical_start);
        } else {
            self.chunk_size = logical[ci + 1] - self.chunk_logical_start;
        }
        Ok(())
    }

    /// Read `bytes` raw bytes from the stack, walking chunk boundaries. When
    /// `out` is `Some`, fills it; otherwise skips. Short reads past EOF are
    /// zero-filled (matching Java's lenient read past a truncated stream).
    fn read_from_stack_raw(
        &mut self,
        stack: &ObfStack,
        mut out: Option<&mut [u8]>,
        mut bytes: usize,
    ) -> Result<()> {
        let mut done = 0usize;
        let mut remaining = self.remaining_in_chunk();
        while bytes > 0 {
            while remaining == 0 {
                self.switch_chunk(stack, self.current_chunk + 1)?;
                self.seek(self.chunk_file_start)?;
                remaining = self.remaining_in_chunk();
            }
            if remaining < 0 {
                return Err(BioFormatsError::Format(
                    "Negative remaining bytes in chunk; malformed OBF file".to_string(),
                ));
            }
            let to_read = std::cmp::min(bytes as i64, remaining) as usize;
            self.file
                .seek(SeekFrom::Start(self.pos))
                .map_err(BioFormatsError::Io)?;
            if let Some(o) = out.as_deref_mut() {
                let region = &mut o[done..done + to_read];
                let mut filled = 0;
                while filled < to_read {
                    let n = self
                        .file
                        .read(&mut region[filled..])
                        .map_err(BioFormatsError::Io)?;
                    if n == 0 {
                        for b in &mut region[filled..] {
                            *b = 0;
                        }
                        break;
                    }
                    filled += n;
                }
            }
            self.pos += to_read as u64;
            done += to_read;
            remaining -= to_read as i64;
            bytes -= to_read;
        }
        Ok(())
    }

    /// Read `region.len()` decompressed bytes into `region`.
    fn read_from_stack(&mut self, stack: &ObfStack, region: &mut [u8]) -> Result<()> {
        let bytes = region.len();
        let stack_byte_count = stack.samples_written as u64 * stack.bytes_per_sample as u64;

        if !stack.compression {
            self.read_from_stack_raw(stack, Some(region), bytes)?;
        } else {
            let mut produced_total = 0usize;
            while produced_total < bytes {
                if self.inflate_in_cursor >= self.inflate_in_len {
                    let logical_offset =
                        self.chunk_logical_start + (self.pos - self.chunk_file_start);
                    let remainder = stack_byte_count as i64 - logical_offset as i64;
                    if remainder > 0 {
                        let length = std::cmp::min(remainder as usize, self.inflate_input.len());
                        let mut tmp = std::mem::take(&mut self.inflate_input);
                        self.read_from_stack_raw(stack, Some(&mut tmp[0..length]), length)?;
                        self.inflate_input = tmp;
                        self.inflate_in_cursor = 0;
                        self.inflate_in_len = length;
                    } else {
                        return Err(BioFormatsError::Format(
                            "Corrupted zlib compression".to_string(),
                        ));
                    }
                }
                let cursor = self.inflate_in_cursor;
                let len = self.inflate_in_len;
                let dec = self
                    .decompress
                    .as_mut()
                    .ok_or_else(|| BioFormatsError::Format("OBF: no inflater".to_string()))?;
                let before_in = dec.total_in();
                let before_out = dec.total_out();
                let status = dec
                    .decompress(
                        &self.inflate_input[cursor..len],
                        &mut region[produced_total..bytes],
                        flate2::FlushDecompress::None,
                    )
                    .map_err(|e| BioFormatsError::Format(format!("OBF inflate: {e}")))?;
                let consumed = (dec.total_in() - before_in) as usize;
                let produced = (dec.total_out() - before_out) as usize;
                self.inflate_in_cursor += consumed;
                produced_total += produced;
                if status == flate2::Status::StreamEnd {
                    // Remaining output (if any) stays zero, matching the Java
                    // tolerance for a zlib error past the end of the stream.
                    break;
                }
                if consumed == 0 && produced == 0 && self.inflate_in_cursor < self.inflate_in_len {
                    // No forward progress with input still available: bail out
                    // to avoid an infinite loop on malformed data.
                    break;
                }
            }
        }
        self.next_read_position += bytes as i64;
        Ok(())
    }

    fn skip_bytes(&mut self, stack: &ObfStack, byte_count: u64) -> Result<()> {
        let has_chunks = stack.chunk_logical_positions.is_some();
        if !stack.compression && !has_chunks {
            self.seek(self.pos + byte_count)?;
            self.next_read_position += byte_count as i64;
        } else if stack.compression {
            let mut remaining = byte_count;
            let mut skip_buf = vec![0u8; 8192];
            while remaining > 0 {
                let read_size = std::cmp::min(8192u64, remaining) as usize;
                self.read_from_stack(stack, &mut skip_buf[0..read_size])?;
                remaining -= read_size as u64;
            }
        } else {
            let mut remaining = byte_count;
            while remaining > 0 {
                let skip_size = std::cmp::min(remaining, i32::MAX as u64) as usize;
                self.read_from_stack_raw(stack, None, skip_size)?;
                remaining -= skip_size as u64;
            }
        }
        Ok(())
    }

    fn seek_to_frame_start(&mut self, stack: &ObfStack, sample_offset: i64) -> Result<()> {
        let has_chunks = stack.chunk_logical_positions.is_some();
        let stack_byte_offset = (sample_offset * stack.bytes_per_sample as i64) as u64;

        if self.next_read_position == stack_byte_offset as i64 {
            return Ok(());
        }
        self.next_read_position = stack_byte_offset as i64;

        if !has_chunks {
            self.chunk_logical_start = 0;
            self.chunk_file_start = stack.position;
            self.chunk_size = stack.samples_written as u64 * stack.bytes_per_sample as u64;
            if !stack.compression {
                self.seek(stack.position + stack_byte_offset)?;
                return Ok(());
            }
        }

        let mut seek_destination: u64 = 0;
        let mut extra_skip_bytes: u64 = stack_byte_offset;

        if let Some(fp) = stack.flush_points.as_ref() {
            if stack.flush_block_size != 0 {
                let flush_block_index = (stack_byte_offset / stack.flush_block_size) as usize;
                if flush_block_index > 0 {
                    seek_destination = fp[flush_block_index - 1];
                    extra_skip_bytes -= flush_block_index as u64 * stack.flush_block_size;
                }
            }
        }

        if stack.compression {
            // new Inflater(nowrap = seekDestination != 0); flate2's zlib_header
            // flag is the inverse of nowrap.
            self.decompress = Some(flate2::Decompress::new(seek_destination == 0));
            self.inflate_in_cursor = 0;
            self.inflate_in_len = 0;
            if self.inflate_input.is_empty() {
                self.inflate_input = vec![0u8; 8192];
            }
        }

        if !has_chunks {
            self.seek(stack.position + seek_destination)?;
            self.skip_bytes(stack, extra_skip_bytes)?;
            return Ok(());
        }

        let logical = stack.chunk_logical_positions.as_ref().unwrap();
        let idx = match logical.binary_search(&seek_destination) {
            Ok(i) => i as i64,
            Err(i) => i as i64,
        };
        self.switch_chunk(stack, idx)?;
        self.seek(self.chunk_file_start + seek_destination - self.chunk_logical_start)?;
        self.skip_bytes(stack, extra_skip_bytes)?;
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    fn read_stack_frame(
        &mut self,
        stack: &ObfStack,
        size_x: u32,
        size_y: u32,
        sample_offset: i64,
        region: &mut [u8],
        w: u32,
        h: u32,
    ) -> Result<()> {
        let columns = size_x as i64;
        let rows = size_y as i64;
        let bps = stack.bytes_per_sample as i64;
        let frame_samples_total = columns * rows;
        let mut frame_bytes_written = std::cmp::max(
            std::cmp::min(stack.samples_written - sample_offset, frame_samples_total) * bps,
            0,
        );
        if frame_bytes_written > 0 {
            self.seek_to_frame_start(stack, sample_offset)?;
        }
        let row_skip_bytes = (columns - w as i64) * bps;
        let mut cur = 0usize;
        for yy in 0..h as i64 {
            if yy != 0 && row_skip_bytes > 0 {
                let written_skip = std::cmp::min(row_skip_bytes, frame_bytes_written);
                self.skip_bytes(stack, written_skip as u64)?;
                frame_bytes_written -= written_skip;
            }
            let total_row_bytes = (w as i64) * bps;
            let written_row_bytes = std::cmp::min(total_row_bytes, frame_bytes_written);
            if written_row_bytes > 0 {
                let wrb = written_row_bytes as usize;
                self.read_from_stack(stack, &mut region[cur..cur + wrb])?;
                cur += wrb;
                frame_bytes_written -= written_row_bytes;
            }
            // Unwritten row bytes remain zero (region is pre-zeroed).
        }
        Ok(())
    }
}

/// OBF / MSR (Imspector / Abberior STED) format reader.
///
/// Faithful port of Bio-Formats `loci.formats.in.OBFReader`. OBF files start
/// with the `OMAS_BF\n` magic plus a version, then a linked list of stacks;
/// each stack carries its own dimensions, data type and an optionally
/// zlib-compressed, possibly chunked pixel block. Each stack maps to one
/// Bio-Formats series. The non-FLIM read path is implemented (chunk walking,
/// flush-point seeking and streaming inflate); the OME-XML side metadata block
/// (file version >= 2) is skipped, so dimensions always come from the raw
/// stack headers.
pub struct ObfReader {
    path: Option<PathBuf>,
    metas: Vec<ImageMetadata>,
    stacks: Vec<ObfStack>,
    series: usize,
}

impl ObfReader {
    pub fn new() -> Self {
        ObfReader {
            path: None,
            metas: Vec::new(),
            stacks: Vec::new(),
            series: 0,
        }
    }

    fn pixel_type(type_code: i32) -> Result<(PixelType, u8)> {
        Ok(match type_code {
            0x01 => (PixelType::Uint8, 8),
            0x02 => (PixelType::Int8, 8),
            0x04 => (PixelType::Uint16, 16),
            0x08 => (PixelType::Int16, 16),
            0x10 => (PixelType::Uint32, 32),
            0x20 => (PixelType::Int32, 32),
            0x40 => (PixelType::Float32, 32),
            0x80 => (PixelType::Float64, 64),
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "OBF unsupported data type {other}"
                )))
            }
        })
    }

    fn file_version(input: &mut ObfIn) -> Result<i32> {
        input.seek(0)?;
        let magic = match input.read_n(OBF_FILE_MAGIC.len()) {
            Ok(m) => m,
            Err(_) => return Ok(-1),
        };
        let magic_number = input.u16().unwrap_or(0);
        let version = input.i32().unwrap_or(-1);
        if magic == OBF_FILE_MAGIC && magic_number == OBF_MAGIC_NUMBER {
            Ok(version)
        } else {
            Ok(-1)
        }
    }

    /// Parse one stack starting at `current`; returns the offset of the next
    /// stack (0 = end of list).
    fn init_stack(&mut self, input: &mut ObfIn, current: u64) -> Result<u64> {
        input.seek(current)?;
        let magic = input.read_n(OBF_STACK_MAGIC.len())?;
        let magic_number = input.u16()?;
        let stack_version = input.i32()?;

        if magic != OBF_STACK_MAGIC || magic_number != OBF_MAGIC_NUMBER {
            return Err(BioFormatsError::Format(
                "Unsupported OBF stack format".to_string(),
            ));
        }

        let num_dims = input.i32()?;
        if num_dims > 5 {
            return Err(BioFormatsError::Format(format!(
                "Unsupported number of {num_dims} dimensions"
            )));
        }
        let num_dims = num_dims.max(0) as usize;

        let mut samples_written: i64 = 1;
        let mut sizes = [1i32; OBF_MAX_DIMS];
        for (d, slot) in sizes.iter_mut().enumerate() {
            let size = input.i32()?;
            if d < num_dims {
                samples_written *= size as i64;
                *slot = size;
            } else {
                *slot = 1;
            }
        }

        let size_x = sizes[0].max(0) as u32;
        let size_y = sizes[1].max(0) as u32;
        let size_z = sizes[2].max(0) as u32;
        let size_c = sizes[3].max(0) as u32;
        let size_t = sizes[4].max(0) as u32;
        let image_count = size_z * size_c * size_t;

        // lengths (15 doubles) and offsets (15 doubles) - parsed but unused.
        for _ in 0..OBF_MAX_DIMS {
            input.f64()?;
        }
        for _ in 0..OBF_MAX_DIMS {
            input.f64()?;
        }

        let type_code = input.i32()?;
        let (pixel_type, bits_per_pixel) = Self::pixel_type(type_code)?;
        let bytes_per_sample = (bits_per_pixel / 8) as usize;

        let compression = match input.i32()? {
            0 => false,
            1 => true,
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "OBF unsupported compression {other}"
                )))
            }
        };

        input.skip(4)?;
        let length_of_name = input.i32()?;
        let length_of_description = input.i32()?;
        input.skip(8)?;
        let length_of_data = input.i64()?;
        if length_of_data < 0 {
            return Err(BioFormatsError::Format(
                "Negative OBF stack length on disk".to_string(),
            ));
        }
        let next = input.i64()?;
        input.skip(length_of_name.max(0) as u64)?;
        input.skip(length_of_description.max(0) as u64)?;

        let position = input.pos()?;

        let mut stack = ObfStack {
            position,
            compression,
            samples_written,
            bytes_per_sample,
            flush_points: None,
            flush_block_size: 0,
            chunk_logical_positions: None,
            chunk_file_positions: None,
        };

        if stack_version >= 1 {
            input.skip(length_of_data as u64)?;
            let footer = input.pos()?;
            let offset = input.i32()?;

            // stepsPresent / stepLabelsPresent (15 ints each).
            let mut steps_present = [false; OBF_MAX_DIMS];
            for (d, slot) in steps_present.iter_mut().enumerate() {
                let v = input.i32()?;
                if d < num_dims {
                    *slot = v != 0;
                }
            }
            let mut step_labels_present = [false; OBF_MAX_DIMS];
            for (d, slot) in step_labels_present.iter_mut().enumerate() {
                let v = input.i32()?;
                if d < num_dims {
                    *slot = v != 0;
                }
            }

            let mut obsolete_metadata_length: i64 = 0;
            let mut num_flush_points: i64 = 0;
            if stack_version >= 3 {
                const SI_UNIT_SIZE: u64 = 80;
                obsolete_metadata_length = input.i32()? as i64;
                input.skip(SI_UNIT_SIZE * (OBF_MAX_DIMS as u64 + 1))?;
                num_flush_points = input.i64()?;
                stack.flush_block_size = input.i64()?.max(0) as u64;
            }

            let mut tag_dictionary_length: i64 = 0;
            if stack_version >= 4 {
                tag_dictionary_length = input.i64()?;
                let _stack_end_disk = input.i64()?;
                let _min_format_version = input.i32()?;
            }

            let mut num_chunk_positions: i64 = 0;
            if stack_version >= 6 {
                let _stack_end_used_disk = input.i64()?;
                samples_written = input.i64()?;
                stack.samples_written = samples_written;
                num_chunk_positions = input.i64()?;
            }

            input.seek(footer + offset.max(0) as u64)?;

            // labels (one length-prefixed string per real dimension).
            for _ in 0..num_dims {
                let length = input.i32()?;
                input.skip(length.max(0) as u64)?;
            }

            // steps (doubles) per dimension when present.
            for d in 0..num_dims {
                if steps_present[d] {
                    for _ in 0..sizes[d].max(0) {
                        input.f64()?;
                    }
                }
            }
            // step labels (length-prefixed strings) per dimension when present.
            for d in 0..num_dims {
                if step_labels_present[d] {
                    for _ in 0..sizes[d].max(0) {
                        let length = input.i32()?;
                        input.skip(length.max(0) as u64)?;
                    }
                }
            }

            input.skip(obsolete_metadata_length.max(0) as u64)?;

            if num_flush_points > 0 {
                let mut flush_points = Vec::with_capacity(num_flush_points as usize);
                for _ in 0..num_flush_points {
                    flush_points.push(input.i64()?.max(0) as u64);
                }
                stack.flush_points = Some(flush_points);
            }

            input.skip(tag_dictionary_length.max(0) as u64)?;

            if num_chunk_positions > 0 {
                let mut logical = Vec::with_capacity(num_chunk_positions as usize + 1);
                let mut file = Vec::with_capacity(num_chunk_positions as usize + 1);
                logical.push(0u64);
                file.push(0u64);
                for _ in 0..num_chunk_positions {
                    logical.push(input.i64()?.max(0) as u64);
                    file.push(input.i64()?.max(0) as u64);
                }
                stack.chunk_logical_positions = Some(logical);
                stack.chunk_file_positions = Some(file);
            }
        } else {
            return Err(BioFormatsError::Format(
                "Unsupported OBF stack format".to_string(),
            ));
        }

        self.stacks.push(stack);
        self.metas.push(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: HashMap::new(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });

        Ok(next.max(0) as u64)
    }

    fn read_region(&self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let stack = &self.stacks[self.series];
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if x.checked_add(w).is_none_or(|e| e > meta.size_x)
            || y.checked_add(h).is_none_or(|e| e > meta.size_y)
        {
            return Err(BioFormatsError::Format(
                "requested region is outside the image bounds".to_string(),
            ));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;

        let bps = stack.bytes_per_sample;
        let region_bytes = (w as usize)
            .checked_mul(h as usize)
            .and_then(|px| px.checked_mul(bps))
            .ok_or_else(|| BioFormatsError::Format("OBF region too large".to_string()))?;
        let mut region = vec![0u8; region_bytes];

        let columns = meta.size_x as i64;
        let rows = meta.size_y as i64;
        let frame_start_sample_offset =
            (plane_index as i64) * rows * columns + (y as i64) * columns + x as i64;

        let mut state = ObfReadState::new(file);
        state.read_stack_frame(
            stack,
            meta.size_x,
            meta.size_y,
            frame_start_sample_offset,
            &mut region,
            w,
            h,
        )?;
        Ok(region)
    }
}

impl Default for ObfReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ObfReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("obf") | Some("msr"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Magic "OMAS_BF\n" + 0xFFFF + a non-negative version int.
        if header.len() < OBF_FILE_MAGIC.len() + 6 {
            return false;
        }
        if &header[..OBF_FILE_MAGIC.len()] != OBF_FILE_MAGIC {
            return false;
        }
        let mn = u16::from_le_bytes([header[8], header[9]]);
        if mn != OBF_MAGIC_NUMBER {
            return false;
        }
        let version = i32::from_le_bytes([header[10], header[11], header[12], header[13]]);
        version >= 0
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.path = None;
        self.metas.clear();
        self.stacks.clear();
        self.series = 0;

        let file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let mut input = ObfIn { file };

        let version = Self::file_version(&mut input)?;
        if version < 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "not an OBF file".to_string(),
            ));
        }

        // After the magic check the pointer sits past the 14-byte file header.
        input.seek(OBF_FILE_MAGIC.len() as u64 + 2 + 4)?;
        let stack_position = input.i64()?;
        let length_of_description = input.i32()?;
        input.skip(length_of_description.max(0) as u64)?;
        if version >= 2 {
            // meta_data_position: OME-XML side metadata is skipped; dimensions
            // come from the raw stack headers instead.
            let _meta_data_position = input.i64()?;
        }

        if stack_position != 0 {
            let mut cur = stack_position.max(0) as u64;
            loop {
                let next = self.init_stack(&mut input, cur)?;
                if next == 0 {
                    break;
                }
                cur = next;
            }
        }

        if self.stacks.is_empty() {
            return Err(BioFormatsError::Format(
                "OBF file has no stacks".to_string(),
            ));
        }

        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.metas.clear();
        self.stacks.clear();
        self.series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.stacks.len()
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.stacks.len() {
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
        self.metas
            .get(self.series)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let (w, h) = (meta.size_x, meta.size_y);
        self.read_region(plane_index, 0, 0, w, h)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.read_region(plane_index, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .metas
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

#[cfg(test)]
mod pds_tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    static COUNTER: AtomicU64 = AtomicU64::new(0);

    /// Build a unique base path (no intermediate dots) in the temp directory.
    fn unique_base(tag: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bioformats_pds_{tag}_{nanos}_{n}"))
    }

    /// Encode `samples` as little-endian UINT16 bytes.
    fn le_u16(samples: &[u16]) -> Vec<u8> {
        let mut v = Vec::with_capacity(samples.len() * 2);
        for s in samples {
            v.extend_from_slice(&s.to_le_bytes());
        }
        v
    }

    /// Write a minimal grayscale PDS fixture: `.hdr` + companion `.IMG`.
    ///
    /// Header carries the ` IDENTIFICATION` magic, `NXP`/`NYP`, `COLOR = 1`
    /// (grayscale, lutIndex 0), `FILE REC LEN` (= 2 * record_width), and the
    /// requested SIGNX/SIGNY. The companion holds `(size_x + pad)` UINT16
    /// samples per row, where `pad = record_width - (size_x % record_width)`.
    /// `pixels` must be a row-major `size_x * size_y` grid of sample values.
    fn write_gray_fixture(
        tag: &str,
        size_x: u32,
        size_y: u32,
        record_width: u32,
        signx: &str,
        signy: &str,
        pixels: &[u16],
    ) -> (PathBuf, PathBuf) {
        assert_eq!(pixels.len(), (size_x * size_y) as usize);
        let base = unique_base(tag);
        let hdr = base.with_extension("hdr");
        let img = base.with_extension("IMG");

        let header = format!(
            " IDENTIFICATION\r\n\
             NXP = {size_x} / x samples\r\n\
             NYP = {size_y} / y samples\r\n\
             SIGNX = '{signx}' / x sign\r\n\
             SIGNY = '{signy}' / y sign\r\n\
             COLOR = 1 / grayscale\r\n\
             FILE REC LEN = {rec_len} / record length in bytes\r\n\
             END\r\n",
            rec_len = record_width * 2,
        );
        std::fs::write(&hdr, header.as_bytes()).unwrap();

        // Companion: one padded row at a time. Padding samples are sentinel
        // 0xFFFF so a bug that reads padding instead of real data is visible.
        let pad = record_width - (size_x % record_width);
        let mut img_samples: Vec<u16> = Vec::new();
        for row in 0..size_y as usize {
            let start = row * size_x as usize;
            img_samples.extend_from_slice(&pixels[start..start + size_x as usize]);
            img_samples.extend(std::iter::repeat(0xFFFFu16).take(pad as usize));
        }
        std::fs::write(&img, le_u16(&img_samples)).unwrap();

        (hdr, img)
    }

    fn cleanup(paths: &[&PathBuf]) {
        for p in paths {
            let _ = std::fs::remove_file(p);
        }
    }

    #[test]
    fn pds_grayscale_full_and_region() {
        // 3x2 image; record_width = 4 => pad = 4 - (3 % 4) = 1 sample per row.
        let size_x = 3u32;
        let size_y = 2u32;
        let record_width = 4u32;
        let pixels: Vec<u16> = vec![
            10, 20, 30, // row 0
            40, 50, 60, // row 1
        ];
        let (hdr, img) =
            write_gray_fixture("gray", size_x, size_y, record_width, "+", "+", &pixels);

        let mut r = PdsReader::new();
        // Detection by name (header magic present).
        assert!(r.is_this_type_by_name(&hdr));
        // Detection of the companion via sibling header.
        assert!(r.is_this_type_by_name(&img));
        // Magic byte detection.
        assert!(r.is_this_type_by_bytes(b" IDENTIFICATION extra"));
        assert!(!r.is_this_type_by_bytes(b"NOT A PDS FILE."));

        r.set_id(&hdr).unwrap();
        let meta = r.metadata();
        assert_eq!(meta.size_x, size_x);
        assert_eq!(meta.size_y, size_y);
        assert_eq!(meta.size_c, 1);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert!(meta.is_little_endian);
        assert!(!meta.is_rgb);

        // Full plane: padding must be stripped, row-major order preserved.
        let full = r.open_bytes(0).unwrap();
        assert_eq!(full, le_u16(&pixels));

        // Region crop: 2x2 starting at (1,0) => columns 1,2 of both rows.
        let region = r.open_bytes_region(0, 1, 0, 2, 2).unwrap();
        assert_eq!(region, le_u16(&[20, 30, 50, 60]));

        // Single-pixel crop bottom-right.
        let one = r.open_bytes_region(0, 2, 1, 1, 1).unwrap();
        assert_eq!(one, le_u16(&[60]));

        // Out-of-bounds region rejected.
        assert!(r.open_bytes_region(0, 2, 0, 2, 1).is_err());

        cleanup(&[&hdr, &img]);
    }

    #[test]
    fn pds_grayscale_reverse_xy() {
        // SIGNX = '-' and SIGNY = '-' mirror horizontally and vertically.
        let size_x = 3u32;
        let size_y = 2u32;
        let record_width = 4u32;
        let pixels: Vec<u16> = vec![
            10, 20, 30, // row 0
            40, 50, 60, // row 1
        ];
        let (hdr, img) = write_gray_fixture("rev", size_x, size_y, record_width, "-", "-", &pixels);

        let mut r = PdsReader::new();
        r.set_id(&hdr).unwrap();
        assert!(r.reverse_x);
        assert!(r.reverse_y);

        // Full plane mirrored in both axes:
        // Original rows: [10,20,30],[40,50,60]
        // reverseX per row: [30,20,10],[60,50,40]
        // reverseY swaps rows: [60,50,40],[30,20,10]
        let full = r.open_bytes(0).unwrap();
        assert_eq!(full, le_u16(&[60, 50, 40, 30, 20, 10]));

        cleanup(&[&hdr, &img]);
    }

    #[test]
    fn pds_reject_missing_companion() {
        // Header present and valid, but no .IMG/.img companion exists.
        let base = unique_base("nocomp");
        let hdr = base.with_extension("hdr");
        let header = " IDENTIFICATION\r\n\
             NXP = 4 / x\r\n\
             NYP = 4 / y\r\n\
             COLOR = 1 /\r\n\
             FILE REC LEN = 8 /\r\n\
             END\r\n";
        std::fs::write(&hdr, header).unwrap();

        let mut r = PdsReader::new();
        assert!(r.set_id(&hdr).is_err());
        // State stays uninitialized after the failure.
        assert_eq!(r.series_count(), 0);

        cleanup(&[&hdr]);
    }

    #[test]
    fn pds_reject_truncated_companion() {
        // Companion exists but is shorter than the declared (padded) plane.
        let base = unique_base("trunc");
        let hdr = base.with_extension("hdr");
        let img = base.with_extension("IMG");
        let header = " IDENTIFICATION\r\n\
             NXP = 8 / x\r\n\
             NYP = 8 / y\r\n\
             COLOR = 1 /\r\n\
             FILE REC LEN = 16 /\r\n\
             END\r\n";
        std::fs::write(&hdr, header).unwrap();
        // Only a handful of bytes, far short of 8 rows of (8 + pad) UINT16.
        std::fs::write(&img, [0u8; 16]).unwrap();

        let mut r = PdsReader::new();
        assert!(r.set_id(&hdr).is_err());

        cleanup(&[&hdr, &img]);
    }
}

#[cfg(test)]
mod pci_tests {
    use super::*;

    #[test]
    fn global_meta_key_strips_prefixes() {
        // Java replaces separators with spaces and drops the path prefixes.
        assert_eq!(
            PciReader::global_meta_key("Root Entry/Field Data/Field 1/Exposure"),
            "Field 1 Exposure"
        );
        assert_eq!(
            PciReader::global_meta_key("Root Entry/Details/Magnification"),
            "Magnification"
        );
    }

    #[test]
    fn timestamp_index_is_zero_based_field() {
        // Java reads the number after the last space, up to the next separator.
        assert_eq!(
            PciReader::timestamp_index("Root Entry/Field 3/Details"),
            Some(2)
        );
        assert_eq!(PciReader::timestamp_index("no-space-here"), None);
    }

    #[test]
    fn cobol_date_converts_to_iso8601() {
        // COBOL epoch itself (date == 0 ms) is 1582-10-15T00:00:00.
        assert_eq!(
            PciReader::convert_cobol_date(0),
            "1582-10-15T00:00:00"
        );
        // The Unix epoch in COBOL milliseconds round-trips to 1970-01-01.
        let unix_epoch_in_cobol_ms = 12_219_292_800_000;
        assert_eq!(
            PciReader::convert_cobol_date(unix_epoch_in_cobol_ms),
            "1970-01-01T00:00:00"
        );
    }

    #[test]
    fn parse_comments_captures_factor_and_magnification() {
        let mut meta: HashMap<String, MetadataValue> = HashMap::new();
        let mut scale_factor = 1.0;
        let mut magnification = 1.0;
        let mut units_is_pixel = false;
        let comments = "factor = 0.5; um\nmagnification = 40\nunits = pixels; foo\n";
        PciReader::parse_comments(
            comments,
            &mut meta,
            &mut scale_factor,
            &mut magnification,
            &mut units_is_pixel,
        );
        assert_eq!(scale_factor, 0.5);
        assert_eq!(magnification, 40.0);
        assert!(units_is_pixel);
        // Each key=value line is captured as named global metadata.
        assert!(matches!(
            meta.get("factor"),
            Some(MetadataValue::String(s)) if s == "0.5; um"
        ));
        assert!(meta.contains_key("magnification"));
    }
}
