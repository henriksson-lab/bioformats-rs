//! Scanning Probe Microscopy (SPM) and related format readers.
//!
//! Includes a real binary reader for PicoQuant TCSPC data and
//! extension-only placeholder readers for various SPM/AFM platforms.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ---------------------------------------------------------------------------
// Macro: extension-only placeholder reader (512x512 uint16)
// ---------------------------------------------------------------------------
#[allow(unused_macros)]
macro_rules! placeholder_reader {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
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
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn close(&mut self) -> Result<()> {
                self.path = None;
                self.meta = None;
                Ok(())
            }

            fn series_count(&self) -> usize { 1 }

            fn set_series(&mut self, s: usize) -> Result<()> {
                if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
            }

            fn series(&self) -> usize { 0 }

            fn metadata(&self) -> &ImageMetadata {
                self.meta.as_ref().expect("set_id not called")
            }

            fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn open_bytes_region(&mut self, _plane_index: u32, _x: u32, _y: u32, _w: u32, _h: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
                Err(BioFormatsError::UnsupportedFormat(
                    concat!(stringify!($name), " format reading is not yet implemented").to_string()
                ))
            }

            fn resolution_count(&self) -> usize { 1 }

            fn set_resolution(&mut self, level: usize) -> Result<()> {
                if level != 0 {
                    Err(BioFormatsError::Format(format!("resolution {} out of range", level)))
                } else {
                    Ok(())
                }
            }
        }
    };
}

// ===========================================================================
// Binary reader — PicoQuant TCSPC / FLIM
// ===========================================================================

/// PicoQuant PTU/PQRES time-correlated single-photon counting format.
///
/// Magic: first 6 bytes == `PQTTTR`. Image dimensions parsed from text header.
pub struct PicoQuantReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl PicoQuantReader {
    pub fn new() -> Self {
        PicoQuantReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for PicoQuantReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for PicoQuantReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("ptu") | Some("pqres"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 6 && &header[0..6] == b"PQTTTR"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;

        // Read first 4096 bytes as lossy string for header parsing
        let header_bytes = &data[..data.len().min(4096)];
        let text = String::from_utf8_lossy(header_bytes).into_owned();

        let mut width: u32 = 64;
        let mut height: u32 = 64;
        let mut size_z: u32 = 1;

        for line in text.lines() {
            if let Some(val) = line.strip_prefix("ImgHdr_Pixels=") {
                if let Ok(n) = val.trim().parse::<u32>() {
                    width = n;
                }
            } else if let Some(val) = line.strip_prefix("ImgHdr_Lines=") {
                if let Ok(n) = val.trim().parse::<u32>() {
                    height = n;
                }
            } else if let Some(val) = line.strip_prefix("ImgHdr_Frame=") {
                if let Ok(n) = val.trim().parse::<u32>() {
                    size_z = n;
                }
            }
        }

        let _ = (width, height, size_z);
        Err(BioFormatsError::UnsupportedFormat(
            "PicoQuant TCSPC event stream decoding to image planes is not implemented".into(),
        ))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Err(BioFormatsError::UnsupportedFormat(
            "PicoQuant TCSPC event stream decoding to image planes is not implemented".into(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let _ = (plane_index, w, h);
        Err(BioFormatsError::UnsupportedFormat(
            "PicoQuant TCSPC event stream decoding to image planes is not implemented".into(),
        ))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Helper: compute square dimensions from file size assuming uint16
// ===========================================================================

/// Given a file size and a data offset, compute square dimensions assuming
/// uint16 (2 bytes per pixel). Returns (width, height).
fn unsupported_raw_spm(format_name: &str) -> BioFormatsError {
    BioFormatsError::UnsupportedFormat(format!(
        "{format_name} binary layout is not implemented; refusing heuristic dimensions"
    ))
}

// ===========================================================================
// Real binary reader — RHK Technology SPM
// ===========================================================================

/// RHK Technology SPM reader (`.sm2`, `.sm3`, `.sm4`).
///
/// Port of Bio-Formats `RHKReader.java`. The file begins with a 512-byte
/// page header. There are two layouts:
///
///   * **XPM** (binary): the first little-endian `short` equals `0xaa`.
///     Integer fields live at fixed offsets (image/page/data/line type at 40,
///     `sizeX`/`sizeY` after them, then the pixel offset; float X/Y scales
///     follow).
///   * **text**: a space-separated ASCII record at offset 32 carries the same
///     type codes and dimensions; pixels start at the fixed 512-byte boundary
///     and the X/Y scales come from two further 32-byte axis records.
///
/// `dataType` selects the pixel type (0=float32, 1=int16, 2=int32, 3=uint8).
/// In the text layout the X/Y scale signs drive `invertX`/`invertY`, which
/// mirror the stored plane horizontally/vertically when reading pixels.
pub struct RhkReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_offset: u64,
    invert_x: bool,
    invert_y: bool,
}

impl RhkReader {
    const HEADER_SIZE: u64 = 512;

    pub fn new() -> Self {
        RhkReader {
            path: None,
            meta: None,
            pixel_offset: 0,
            invert_x: false,
            invert_y: false,
        }
    }

    fn read_i32_le(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("RHK SPM header missing {label}"))
        })?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn read_f32_le(data: &[u8], offset: usize, label: &str) -> Result<f32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("RHK SPM header missing {label}"))
        })?;
        Ok(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    /// Read a fixed-width ASCII record (Java `readString(len).trim()`).
    fn read_string(data: &[u8], offset: usize, len: usize) -> String {
        let end = (offset + len).min(data.len());
        let slice = data.get(offset..end).unwrap_or(&[]);
        // Stop at the first NUL like Java's String construction over the bytes,
        // then trim surrounding whitespace.
        let nul = slice.iter().position(|&b| b == 0).unwrap_or(slice.len());
        String::from_utf8_lossy(&slice[..nul]).trim().to_string()
    }

    /// Map RHK dataType code → (PixelType, bits-per-pixel).
    fn pixel_type_from_data_type(data_type: i32) -> Result<(PixelType, u8)> {
        match data_type {
            0 => Ok((PixelType::Float32, 32)),
            1 => Ok((PixelType::Int16, 16)),
            2 => Ok((PixelType::Int32, 32)),
            3 => Ok((PixelType::Uint8, 8)),
            other => Err(BioFormatsError::UnsupportedFormat(format!(
                "RHK SPM unsupported data type: {other}"
            ))),
        }
    }
}

impl Default for RhkReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for RhkReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sm2") | Some("sm3") | Some("sm4"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE as usize {
            return Err(BioFormatsError::UnsupportedFormat(
                "RHK SPM file is shorter than the 512-byte page header".into(),
            ));
        }

        // Java: little-endian; xpm = (readShort() == 0xaa).
        let first_short = i16::from_le_bytes([data[0], data[1]]);
        let xpm = first_short == 0xaa;

        let mut width: u32;
        let mut height: u32;
        let pixel_offset: u64;
        let data_type: i32;
        let mut invert_x = false;
        let mut invert_y = false;
        let x_scale: f64;
        let y_scale: f64;

        if xpm {
            // seek(40): imageType, pageType, dataType, lineType ints.
            let _image_type = Self::read_i32_le(&data, 40, "image type")?;
            let _page_type = Self::read_i32_le(&data, 44, "page type")?;
            data_type = Self::read_i32_le(&data, 48, "data type")?;
            let _line_type = Self::read_i32_le(&data, 52, "line type")?;
            // skipBytes(8) → offset 56..64.
            width = Self::read_i32_le(&data, 64, "width")? as u32;
            height = Self::read_i32_le(&data, 68, "height")? as u32;
            // skipBytes(16) → offset 72..88.
            pixel_offset = Self::read_i32_le(&data, 88, "pixel offset")? as u32 as u64;
            // After the int read, the stream is at offset 92; skipBytes(8) → 100.
            x_scale = Self::read_f32_le(&data, 100, "x scale")? as f64 * 1_000_000.0;
            y_scale = Self::read_f32_le(&data, 104, "y scale")? as f64 * 1_000_000.0;
        } else {
            // seek(32): 32-byte space-separated ASCII type/dimension record.
            let type_record = Self::read_string(&data, 32, 32);
            let type_data: Vec<&str> = type_record.split_whitespace().collect();
            let parse = |idx: usize, label: &str| -> Result<i32> {
                type_data
                    .get(idx)
                    .and_then(|v| v.parse::<i32>().ok())
                    .ok_or_else(|| {
                        BioFormatsError::UnsupportedFormat(format!(
                            "RHK SPM text header missing {label}"
                        ))
                    })
            };
            let _image_type = parse(0, "image type")?;
            data_type = parse(1, "data type")?;
            let _line_type = parse(2, "line type")?;
            width = parse(3, "width")? as u32;
            height = parse(4, "height")? as u32;
            let _page_type = parse(6, "page type")?;
            pixel_offset = Self::HEADER_SIZE;

            // Two further 32-byte axis records (X then Y); field [1] is the scale.
            let x_axis = Self::read_string(&data, 64, 32);
            let y_axis = Self::read_string(&data, 96, 32);
            let x_axis_fields: Vec<&str> = x_axis.split_whitespace().collect();
            let y_axis_fields: Vec<&str> = y_axis.split_whitespace().collect();
            let x_raw = x_axis_fields
                .get(1)
                .and_then(|v| v.parse::<f64>().ok())
                .ok_or_else(|| {
                    BioFormatsError::UnsupportedFormat("RHK SPM text header missing X scale".into())
                })?;
            let y_raw = y_axis_fields
                .get(1)
                .and_then(|v| v.parse::<f64>().ok())
                .ok_or_else(|| {
                    BioFormatsError::UnsupportedFormat("RHK SPM text header missing Y scale".into())
                })?;
            x_scale = x_raw * 1_000_000.0;
            y_scale = y_raw * 1_000_000.0;
            invert_x = x_scale < 0.0;
            invert_y = y_scale > 0.0;
        }

        if width == 0 || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "RHK SPM header contains invalid image dimensions".into(),
            ));
        }
        let _ = (&mut width, &mut height);

        let (pixel_type, bits_per_pixel) = Self::pixel_type_from_data_type(data_type)?;
        let bps = pixel_type.bytes_per_sample() as u64;
        let expected = pixel_offset
            .checked_add(
                (width as u64)
                    .checked_mul(height as u64)
                    .and_then(|p| p.checked_mul(bps))
                    .ok_or_else(|| BioFormatsError::Format("RHK SPM plane size overflows".into()))?,
            )
            .ok_or_else(|| BioFormatsError::Format("RHK SPM plane size overflows".into()))?;
        if expected > data.len() as u64 {
            return Err(BioFormatsError::UnsupportedFormat(
                "RHK SPM pixel payload is shorter than declared dimensions".into(),
            ));
        }

        // seek(352): 32-byte description string.
        let description = Self::read_string(&data, 352, 32);
        let mut series_metadata = HashMap::new();
        if !description.is_empty() {
            series_metadata.insert(
                "Description".into(),
                crate::common::metadata::MetadataValue::String(description),
            );
        }
        series_metadata.insert(
            "X scale (um)".into(),
            crate::common::metadata::MetadataValue::Float(x_scale),
        );
        series_metadata.insert(
            "Y scale (um)".into(),
            crate::common::metadata::MetadataValue::Float(y_scale),
        );

        self.pixel_offset = pixel_offset;
        self.invert_x = invert_x;
        self.invert_y = invert_y;
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
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
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_offset = 0;
        self.invert_x = false;
        self.invert_y = false;
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let (sx, sy) = (meta.size_x, meta.size_y);
        self.open_bytes_region(plane_index, 0, 0, sx, sy)
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
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let sx = meta.size_x as usize;
        let sy = meta.size_y as usize;
        let n_bytes = sx
            .checked_mul(sy)
            .and_then(|p| p.checked_mul(bps))
            .ok_or_else(|| BioFormatsError::Format("RHK SPM plane size overflows".into()))?;

        let path = self.path.clone().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(&path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(self.pixel_offset))
            .map_err(BioFormatsError::Io)?;
        let mut plane = vec![0u8; n_bytes];
        f.read_exact(&mut plane).map_err(BioFormatsError::Io)?;

        // RHKReader.java reads pixels from the mirrored corner and then flips
        // the returned tile. Mirroring the whole stored plane (per axis) before
        // cropping at (x,y,w,h) is equivalent and reuses the crop helper.
        let row_len = sx * bps;
        if self.invert_y {
            for row in 0..sy / 2 {
                let top = row * row_len;
                let bottom = (sy - row - 1) * row_len;
                for i in 0..row_len {
                    plane.swap(top + i, bottom + i);
                }
            }
        }
        if self.invert_x {
            for row in 0..sy {
                let base = row * row_len;
                for col in 0..sx / 2 {
                    let left = base + col * bps;
                    let right = base + (sx - col - 1) * bps;
                    for i in 0..bps {
                        plane.swap(left + i, right + i);
                    }
                }
            }
        }

        crop_full_plane("RHK SPM", &plane, &meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — Quesant AFM
// ===========================================================================

/// Quesant AFM reader (`.afm`).
///
/// Binary header then raw data. Falls back to raw uint16 square heuristic.
pub struct QuesantReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl QuesantReader {
    pub fn new() -> Self {
        QuesantReader {
            path: None,
            meta: None,
            data_offset: 0,
        }
    }
}

impl Default for QuesantReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for QuesantReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        // Note: .afm is also used by VeecoReader (Nanoscope). Quesant AFM
        // files lack the NANOSCOPE header, so this reader is a fallback.
        matches!(ext.as_deref(), Some("afm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let _ = path;
        Err(unsupported_raw_spm("Quesant AFM"))
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let _ = plane_index;
        Err(unsupported_raw_spm("Quesant AFM"))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let _ = (plane_index, _x, _y, w, h);
        Err(unsupported_raw_spm("Quesant AFM"))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// TIFF reader — JPK Instruments AFM
// ===========================================================================

/// JPK Instruments AFM reader (`.jpk`).
///
/// Port of JPKReader.java: a `.jpk` file IS a TIFF (JPKReader extends
/// BaseTiffReader). Exposes two series: series 0 = IFD 0 (a single-plane
/// thumbnail), series 1 = IFDs 1..n grouped as a T-stack.
pub struct JpkReader {
    extracted_path: Option<PathBuf>,
    inner: crate::tiff::TiffReader,
    meta: Option<ImageMetadata>,
    is_tiff: bool,
}

impl JpkReader {
    pub fn new() -> Self {
        JpkReader {
            extracted_path: None,
            inner: crate::tiff::TiffReader::new(),
            meta: None,
            is_tiff: false,
        }
    }
}

impl Default for JpkReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for JpkReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("jpk"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // A .jpk file is itself a TIFF; parse it directly.
        self.inner.set_id(path)?;

        let ifd_count = self.inner.ifd_count();
        if ifd_count == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "JPK: TIFF contains no IFDs".to_string(),
            ));
        }

        // Build a per-IFD metadata lookup from the default series grouping so we
        // can reconstruct accurate dimensions/pixel-type for the JPK layout.
        // We clone existing TiffSeries values (the type is not re-exported) and
        // mutate their public fields rather than constructing literals.
        let default_series = self.inner.series_list();
        let mut meta_for_ifd: Vec<Option<ImageMetadata>> = vec![None; ifd_count];
        for series in default_series {
            for &idx in &series.ifd_indices {
                if idx < ifd_count {
                    meta_for_ifd[idx] = Some(series.metadata.clone());
                }
            }
        }
        // A template TiffSeries to clone (carries the unexported type).
        let template = default_series[0].clone();
        let ifd_meta = |idx: usize| -> ImageMetadata {
            meta_for_ifd
                .get(idx)
                .and_then(|m| m.clone())
                .unwrap_or_else(|| template.metadata.clone())
        };

        let mut new_series = Vec::new();

        // Series 0: IFD 0 only, a single-plane thumbnail.
        {
            let mut s = template.clone();
            let mut m = ifd_meta(0);
            m.size_z = 1;
            m.size_t = 1;
            m.image_count = 1;
            m.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;
            s.ifd_indices = vec![0];
            s.plane_ifd_indices = Vec::new();
            s.sub_resolutions = Vec::new();
            s.metadata = m;
            new_series.push(s);
        }

        // Series 1 (only if there is more than one IFD): IFDs 1..n as a T-stack.
        if ifd_count > 1 {
            let t = (ifd_count - 1) as u32;
            let mut s = template.clone();
            let mut m = ifd_meta(1);
            m.size_z = 1;
            m.size_t = t;
            m.size_c = if m.is_rgb { m.size_c } else { 1 };
            m.image_count = t;
            m.dimension_order = crate::common::metadata::DimensionOrder::XYCZT;
            s.ifd_indices = (1..ifd_count).collect();
            s.plane_ifd_indices = Vec::new();
            s.sub_resolutions = Vec::new();
            s.metadata = m;
            new_series.push(s);
        }

        self.inner.replace_series(new_series);
        self.inner.set_series(0)?;
        self.meta = Some(self.inner.metadata().clone());
        self.is_tiff = true;

        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        if self.is_tiff {
            let _ = self.inner.close();
        }
        if let Some(p) = self.extracted_path.take() {
            let _ = std::fs::remove_file(p);
        }
        self.meta = None;
        self.is_tiff = false;
        Ok(())
    }

    fn series_count(&self) -> usize {
        if self.is_tiff {
            self.inner.series_count()
        } else {
            1
        }
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.is_tiff {
            self.inner.set_series(s)
        } else if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
        }
    }

    fn series(&self) -> usize {
        if self.is_tiff {
            self.inner.series()
        } else {
            0
        }
    }

    fn metadata(&self) -> &ImageMetadata {
        if self.is_tiff {
            self.inner.metadata()
        } else {
            self.meta.as_ref().expect("set_id not called")
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_bytes(plane_index);
        }
        let _ = plane_index;
        Err(BioFormatsError::UnsupportedFormat(
            "JPK ZIP archive does not contain delegated TIFF data".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_bytes_region(plane_index, x, y, w, h);
        }
        let _ = (plane_index, x, y, w, h);
        Err(BioFormatsError::UnsupportedFormat(
            "JPK ZIP archive does not contain delegated TIFF data".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        if self.is_tiff {
            return self.inner.open_thumb_bytes(plane_index);
        }
        let _ = plane_index;
        Err(BioFormatsError::UnsupportedFormat(
            "JPK ZIP archive does not contain delegated TIFF data".to_string(),
        ))
    }

    fn resolution_count(&self) -> usize {
        if self.is_tiff {
            self.inner.resolution_count()
        } else {
            1
        }
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if self.is_tiff {
            self.inner.set_resolution(level)
        } else if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — WaTom SPM
// ===========================================================================

/// WA Technology TOP reader (`.wat`, plus legacy aliases).
///
/// Java Bio-Formats uses a 4864-byte little-endian header followed by raw
/// signed 16-bit pixels.
pub struct WatopReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl WatopReader {
    const HEADER_SIZE: usize = 4864;
    const MAGIC: &'static [u8] = b"0TOPSystem W.A.Technology";

    pub fn new() -> Self {
        WatopReader {
            path: None,
            meta: None,
        }
    }

    fn read_i32_le(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("WA Technology TOP header missing {label}"))
        })?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Default for WatopReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for WatopReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(
            ext.as_deref(),
            Some("wat") | Some("wap") | Some("opo") | Some("opz") | Some("opt")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(Self::MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "WA Technology TOP file is shorter than the 4864-byte header".into(),
            ));
        }
        if !data.starts_with(Self::MAGIC) {
            return Err(BioFormatsError::UnsupportedFormat(
                "WA Technology TOP file is missing 0TOPSystem W.A.Technology magic".into(),
            ));
        }

        let width = Self::read_i32_le(&data, 259, "width")?;
        let height = Self::read_i32_le(&data, 263, "height")?;
        if width <= 0 || height <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "WA Technology TOP header contains invalid image dimensions".into(),
            ));
        }
        let width = width as u32;
        let height = height as u32;
        let expected = (Self::HEADER_SIZE as u64)
            .checked_add(width as u64 * height as u64 * 2)
            .ok_or_else(|| BioFormatsError::Format("WA Technology TOP size overflows".into()))?;
        let file_len = data.len() as u64;
        if file_len < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "WA Technology TOP pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }

        let comment_bytes = data.get(49..82).unwrap_or(&[]);
        let comment = String::from_utf8_lossy(comment_bytes)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        let mut series_metadata = HashMap::new();
        if !comment.is_empty() {
            series_metadata.insert(
                "Comment".to_string(),
                crate::common::metadata::MetadataValue::String(comment),
            );
        }
        if let Ok(x_size) = Self::read_i32_le(&data, 247, "x size") {
            series_metadata.insert(
                "X size (in um)".to_string(),
                crate::common::metadata::MetadataValue::Float(x_size as f64 / 100.0),
            );
        }
        if let Ok(y_size) = Self::read_i32_le(&data, 251, "y size") {
            series_metadata.insert(
                "Y size (in um)".to_string(),
                crate::common::metadata::MetadataValue::Float(y_size as f64 / 100.0),
            );
        }
        if let Ok(z_size) = Self::read_i32_le(&data, 255, "z size") {
            series_metadata.insert(
                "Z size (in um)".to_string(),
                crate::common::metadata::MetadataValue::Float(z_size as f64 / 100.0),
            );
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Int16,
            bits_per_pixel: 16,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
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
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::HEADER_SIZE as u64))
            .map_err(BioFormatsError::Io)?;
        let n_bytes = meta.size_x as usize * meta.size_y as usize * 2;
        let mut buf = vec![0; n_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("WA Technology TOP", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — VG SAM
// ===========================================================================

/// VG SAM reader (`.dti`, plus legacy `.vgsam` alias).
///
/// Java Bio-Formats uses `VGS` magic, big-endian dimensions at offsets
/// 348/352, bytes-per-pixel at 360, and pixels at offset 368.
pub struct VgSamReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl VgSamReader {
    const PIXEL_OFFSET: usize = 368;
    const MAGIC: &'static [u8] = b"VGS";

    pub fn new() -> Self {
        VgSamReader {
            path: None,
            meta: None,
        }
    }

    fn read_i32_be(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("VG SAM header missing {label}"))
        })?;
        Ok(i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }

    fn pixel_type_from_bpp(bytes_per_pixel: i32) -> Result<(PixelType, u8)> {
        match bytes_per_pixel {
            1 => Ok((PixelType::Uint8, 8)),
            2 => Ok((PixelType::Uint16, 16)),
            4 => Ok((PixelType::Float32, 32)),
            _ => Err(BioFormatsError::UnsupportedFormat(format!(
                "VG SAM unsupported bytes per pixel: {bytes_per_pixel}"
            ))),
        }
    }
}

impl Default for VgSamReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for VgSamReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("dti") | Some("vgsam"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(Self::MAGIC)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::PIXEL_OFFSET {
            return Err(BioFormatsError::UnsupportedFormat(
                "VG SAM file is shorter than the 368-byte header".into(),
            ));
        }
        if !data.starts_with(Self::MAGIC) {
            return Err(BioFormatsError::UnsupportedFormat(
                "VG SAM file is missing VGS magic".into(),
            ));
        }
        let width = Self::read_i32_be(&data, 348, "width")?;
        let height = Self::read_i32_be(&data, 352, "height")?;
        let bytes_per_pixel = Self::read_i32_be(&data, 360, "bytes per pixel")?;
        if width <= 0 || height <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "VG SAM header contains invalid image dimensions".into(),
            ));
        }
        let (pixel_type, bits_per_pixel) = Self::pixel_type_from_bpp(bytes_per_pixel)?;
        let width = width as u32;
        let height = height as u32;
        let expected = (Self::PIXEL_OFFSET as u64)
            .checked_add(width as u64 * height as u64 * bytes_per_pixel as u64)
            .ok_or_else(|| BioFormatsError::Format("VG SAM size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "VG SAM pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }

        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "Bytes per pixel".into(),
            crate::common::metadata::MetadataValue::Int(bytes_per_pixel as i64),
        );
        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: false,
            resolution_count: 1,
            series_metadata,
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::PIXEL_OFFSET as u64))
            .map_err(BioFormatsError::Io)?;
        let mut buf =
            vec![
                0u8;
                meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample()
            ];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("VG SAM", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — UBM Messtechnik
// ===========================================================================

/// UBM reader (`.pr3`, plus legacy `.ubm` alias).
///
/// Java Bio-Formats stores dimensions at offsets 44/48 in a 128-byte
/// little-endian header, followed by uint32 pixels with optional row padding.
pub struct UbmReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    padding_pixels: usize,
}

impl UbmReader {
    const HEADER_SIZE: usize = 128;

    pub fn new() -> Self {
        UbmReader {
            path: None,
            meta: None,
            padding_pixels: 0,
        }
    }

    fn read_i32_le(data: &[u8], offset: usize, label: &str) -> Result<i32> {
        let bytes = data.get(offset..offset + 4).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("UBM header missing {label}"))
        })?;
        Ok(i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Default for UbmReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for UbmReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("pr3") | Some("ubm"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "UBM file is shorter than the 128-byte header".into(),
            ));
        }
        let width = Self::read_i32_le(&data, 44, "width")?;
        let height = Self::read_i32_le(&data, 48, "height")?;
        if width <= 0 || height <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "UBM header contains invalid image dimensions".into(),
            ));
        }
        let width = width as u32;
        let height = height as u32;
        let plane_bytes = width as u64 * height as u64 * 4;
        let min_len = Self::HEADER_SIZE as u64 + plane_bytes;
        if (data.len() as u64) < min_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "UBM pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }
        let extra = data.len() as u64 - min_len;
        let row_padding_bytes = extra
            .checked_div(height as u64)
            .ok_or_else(|| BioFormatsError::Format("UBM row padding overflows".into()))?;
        if row_padding_bytes % 4 != 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "UBM row padding is not aligned to uint32 pixels".into(),
            ));
        }
        let padding_pixels = (row_padding_bytes / 4) as usize;

        self.path = Some(path.to_path_buf());
        self.padding_pixels = padding_pixels;
        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "Padding pixels".to_string(),
            crate::common::metadata::MetadataValue::Int(padding_pixels as i64),
        );
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint32,
            bits_per_pixel: 32,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
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
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.padding_pixels = 0;
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.open_bytes_region(plane_index, 0, 0, meta.size_x, meta.size_y)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        crate::common::region::validate_region("UBM", meta.size_x, meta.size_y, _x, _y, w, h)?;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        let row_stride = (meta.size_x as usize + self.padding_pixels)
            .checked_mul(4)
            .ok_or_else(|| BioFormatsError::Format("UBM row stride overflows".into()))?;
        let out_row = (w as usize)
            .checked_mul(4)
            .ok_or_else(|| BioFormatsError::Format("UBM output row size overflows".into()))?;
        let mut out = Vec::with_capacity(out_row * h as usize);
        for row in 0..h as usize {
            let source_row = _y as usize + row;
            let offset =
                Self::HEADER_SIZE as u64 + source_row as u64 * row_stride as u64 + _x as u64 * 4;
            f.seek(SeekFrom::Start(offset))
                .map_err(BioFormatsError::Io)?;
            let start = out.len();
            out.resize(start + out_row, 0);
            f.read_exact(&mut out[start..start + out_row])
                .map_err(BioFormatsError::Io)?;
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

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Raw binary reader — Seiko SPM
// ===========================================================================

/// Seiko SPM reader (`.xqd`, `.xqf`).
///
/// Java Bio-Formats stores dimensions at offset 1402 in a 2944-byte
/// little-endian header, followed by raw uint16 pixels.
pub struct SeikoReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl SeikoReader {
    const HEADER_SIZE: usize = 2944;

    pub fn new() -> Self {
        SeikoReader {
            path: None,
            meta: None,
        }
    }

    fn read_u16_le(data: &[u8], offset: usize, label: &str) -> Result<u16> {
        let bytes = data.get(offset..offset + 2).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!("Seiko SPM header missing {label}"))
        })?;
        Ok(u16::from_le_bytes([bytes[0], bytes[1]]))
    }

    fn read_f32_le(data: &[u8], offset: usize) -> Option<f32> {
        let bytes = data.get(offset..offset + 4)?;
        Some(f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    }
}

impl Default for SeikoReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SeikoReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("xqd") | Some("xqf"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < Self::HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(
                "Seiko SPM file is shorter than the 2944-byte header".into(),
            ));
        }
        let width = Self::read_u16_le(&data, 1402, "width")? as u32;
        let height = Self::read_u16_le(&data, 1404, "height")? as u32;
        if width == 0 || height == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "Seiko SPM header contains invalid image dimensions".into(),
            ));
        }
        let expected = (Self::HEADER_SIZE as u64)
            .checked_add(width as u64 * height as u64 * 2)
            .ok_or_else(|| BioFormatsError::Format("Seiko SPM size overflows".into()))?;
        if (data.len() as u64) < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Seiko SPM pixel payload is shorter than declared dimensions {width}x{height}"
            )));
        }

        let mut series_metadata = HashMap::new();
        let comment_bytes = &data[40..data.len().min(156)];
        let nul = comment_bytes
            .iter()
            .position(|b| *b == 0)
            .unwrap_or(comment_bytes.len());
        let comment = String::from_utf8_lossy(&comment_bytes[..nul])
            .trim()
            .to_string();
        if !comment.is_empty() {
            series_metadata.insert(
                "Comment".into(),
                crate::common::metadata::MetadataValue::String(comment),
            );
        }
        if let Some(x_size) = Self::read_f32_le(&data, 156) {
            series_metadata.insert(
                "X size".into(),
                crate::common::metadata::MetadataValue::Float(x_size as f64),
            );
        }
        if let Some(y_size) = Self::read_f32_le(&data, 164) {
            series_metadata.insert(
                "Y size".into(),
                crate::common::metadata::MetadataValue::Float(y_size as f64),
            );
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
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
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
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
        self.meta.as_ref().expect("set_id not called")
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(Self::HEADER_SIZE as u64))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; meta.size_x as usize * meta.size_y as usize * 2];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        _x: u32,
        _y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let meta = self
            .meta
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let full = self.open_bytes(plane_index)?;
        crop_full_plane("Seiko SPM", &full, &meta, 1, _x, _y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}
