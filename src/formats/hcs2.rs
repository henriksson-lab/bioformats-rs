//! HCS (High-Content Screening) format readers — group 2.
//!
//! TIFF-based HCS wrappers and extension-only placeholder readers for
//! various plate/HCS acquisition platforms.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ---------------------------------------------------------------------------
// Shared helper: find a TIFF file referenced in an index/text file
// ---------------------------------------------------------------------------

/// Searches `text` for substrings that look like TIFF filenames (`.tif` or
/// `.tiff`) and returns the first one that exists relative to `dir`.
fn find_referenced_tiff(text: &str, dir: &Path) -> Option<PathBuf> {
    // Regex-free: scan for tokens ending with .tif or .tiff
    // We look at every "word" (split on whitespace, quotes, angle brackets, etc.)
    let separators = |c: char| {
        c == '"' || c == '\'' || c == '<' || c == '>' || c == '='
            || c == '(' || c == ')' || c == '[' || c == ']'
    };

    for token in text.split(|c: char| c.is_whitespace() || separators(c)) {
        let token = token.trim_matches(|c: char| c == ',' || c == ';' || c == '"' || c == '\'');
        let lower = token.to_ascii_lowercase();
        if lower.ends_with(".tif") || lower.ends_with(".tiff") {
            // Try as-is (relative to dir)
            let candidate = dir.join(token);
            if candidate.is_file() {
                return Some(candidate);
            }
            // Try just the filename component (flat directory)
            if let Some(fname) = Path::new(token).file_name() {
                let candidate = dir.join(fname);
                if candidate.is_file() {
                    return Some(candidate);
                }
            }
        }
    }

    // Fallback: scan directory for any .tif/.tiff file
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let p = entry.path();
            if let Some(ext) = p.extension().and_then(|e| e.to_str()) {
                let ext_lower = ext.to_ascii_lowercase();
                if (ext_lower == "tif" || ext_lower == "tiff") && p.is_file() {
                    return Some(p);
                }
            }
        }
    }

    None
}

// ---------------------------------------------------------------------------
// Macro: thin TIFF wrapper (extension-only detection)
// ---------------------------------------------------------------------------
macro_rules! tiff_wrapper {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
    ) => {
        $(#[$attr])*
        pub struct $name {
            inner: crate::tiff::TiffReader,
        }

        impl $name {
            pub fn new() -> Self {
                $name { inner: crate::tiff::TiffReader::new() }
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

            fn resolution_count(&self) -> usize {
                self.inner.resolution_count()
            }

            fn set_resolution(&mut self, level: usize) -> Result<()> {
                self.inner.set_resolution(level)
            }
        }
    };
}

// (placeholder_reader macro removed — all former stubs now have real implementations)

// ===========================================================================
// TIFF-based HCS wrappers
// ===========================================================================

// ---------------------------------------------------------------------------
// 1. MetaXpress (Molecular Devices) HCS
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// MetaXpress (Molecular Devices) HCS TIFF (`.tif`).
    pub struct MetaxpressTiffReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 2. SimplePCI / HCImage
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// SimplePCI/HCImage TIFF (`.tif`).
    pub struct SimplePciTiffReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 3. Ionpath MIBI-TOF
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Ionpath MIBI-TOF TIFF (`.tif`).
    pub struct IonpathMibiTiffReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 4. Beckman Coulter MIAS
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Beckman Coulter MIAS TIFF (`.tif`).
    pub struct MiasTiffReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 5. Trestle whole-slide
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Trestle whole-slide TIFF (`.tif`).
    pub struct TrestleReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 6. TissueFAXS
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// TissueFAXS TIFF (`.tif`).
    pub struct TissueFaxsReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 7. Mikroscan
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Mikroscan TIFF (`.tif`).
    pub struct MikroscanTiffReader;
    extensions: ["tif"];
}

// ===========================================================================
// HCS index-file readers (parse index, delegate to TiffReader)
// ===========================================================================

// ---------------------------------------------------------------------------
// Macro: HCS index reader that parses a text/XML index file, finds referenced
// TIFF images, and delegates pixel I/O to TiffReader.
// ---------------------------------------------------------------------------
macro_rules! hcs_index_reader {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
        format_label: $label:literal;
    ) => {
        $(#[$attr])*
        pub struct $name {
            inner: Option<crate::tiff::TiffReader>,
            meta: Option<ImageMetadata>,
            #[allow(dead_code)]
            path: Option<PathBuf>,
        }

        impl $name {
            pub fn new() -> Self {
                $name { inner: None, meta: None, path: None }
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

            fn set_id(&mut self, path: &Path) -> Result<()> {
                self.path = Some(path.to_path_buf());
                let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
                let dir = path.parent().unwrap_or(Path::new("."));

                // Store format-specific metadata from index text
                let mut series_metadata = HashMap::new();
                series_metadata.insert(
                    "format".to_string(),
                    MetadataValue::String($label.to_string()),
                );
                series_metadata.insert(
                    "index_file".to_string(),
                    MetadataValue::String(path.display().to_string()),
                );

                if let Some(tiff_path) = find_referenced_tiff(&text, dir) {
                    series_metadata.insert(
                        "image_file".to_string(),
                        MetadataValue::String(tiff_path.display().to_string()),
                    );
                    let mut inner = crate::tiff::TiffReader::new();
                    inner.set_id(&tiff_path)?;
                    let mut meta = inner.metadata().clone();
                    // Merge our metadata into the TIFF metadata
                    for (k, v) in series_metadata {
                        meta.series_metadata.insert(k, v);
                    }
                    self.meta = Some(meta);
                    self.inner = Some(inner);
                } else {
                    return Err(BioFormatsError::Format(
                        format!("{}: no TIFF image files found referenced in index", $label),
                    ));
                }
                Ok(())
            }

            fn close(&mut self) -> Result<()> {
                if let Some(ref mut inner) = self.inner {
                    inner.close()?;
                }
                self.inner = None;
                self.meta = None;
                self.path = None;
                Ok(())
            }

            fn series_count(&self) -> usize {
                self.inner.as_ref().map_or(1, |i| i.series_count())
            }

            fn set_series(&mut self, s: usize) -> Result<()> {
                match self.inner.as_mut() {
                    Some(inner) => inner.set_series(s),
                    None => if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) },
                }
            }

            fn series(&self) -> usize {
                self.inner.as_ref().map_or(0, |i| i.series())
            }

            fn metadata(&self) -> &ImageMetadata {
                self.meta.as_ref().expect("set_id not called")
            }

            fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
                match self.inner.as_mut() {
                    Some(inner) => inner.open_bytes(plane_index),
                    None => Err(BioFormatsError::NotInitialized),
                }
            }

            fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
                match self.inner.as_mut() {
                    Some(inner) => inner.open_bytes_region(plane_index, x, y, w, h),
                    None => Err(BioFormatsError::NotInitialized),
                }
            }

            fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
                match self.inner.as_mut() {
                    Some(inner) => inner.open_thumb_bytes(plane_index),
                    None => Err(BioFormatsError::NotInitialized),
                }
            }

            fn resolution_count(&self) -> usize {
                self.inner.as_ref().map_or(1, |i| i.resolution_count())
            }

            fn set_resolution(&mut self, level: usize) -> Result<()> {
                match self.inner.as_mut() {
                    Some(inner) => inner.set_resolution(level),
                    None => {
                        if level != 0 {
                            Err(BioFormatsError::Format(format!("resolution {} out of range", level)))
                        } else {
                            Ok(())
                        }
                    }
                }
            }
        }
    };
}

// ---------------------------------------------------------------------------
// 8. BD Biosciences Pathway (.exp — INI-style index)
// ---------------------------------------------------------------------------
hcs_index_reader! {
    /// BD Biosciences Pathway HCS reader (`.exp`).
    ///
    /// Reads an INI-style `.exp` index file, locates referenced TIFF images in the
    /// same directory, and delegates pixel I/O to `TiffReader`.
    pub struct BdReader;
    extensions: ["exp"];
    format_label: "BD Pathway";
}

// ---------------------------------------------------------------------------
// 9. PerkinElmer Columbus (.xml — XML index)
// ---------------------------------------------------------------------------
hcs_index_reader! {
    /// PerkinElmer Columbus HCS reader (`.xml`).
    ///
    /// Reads an XML index file listing plate well images, locates the first
    /// referenced TIFF image, and delegates pixel I/O to `TiffReader`.
    pub struct ColumbusReader;
    extensions: ["xml"];
    format_label: "PerkinElmer Columbus";
}

// ---------------------------------------------------------------------------
// 10. PerkinElmer Operetta (.xml — XML index)
// ---------------------------------------------------------------------------
hcs_index_reader! {
    /// PerkinElmer Operetta HCS reader (`.xml`).
    ///
    /// Reads an XML index file, locates referenced TIFF images, and delegates
    /// pixel I/O to `TiffReader`.
    pub struct OperettaReader;
    extensions: ["xml"];
    format_label: "PerkinElmer Operetta";
}

// ---------------------------------------------------------------------------
// 11. Olympus ScanR (.xml — XML index)
// ---------------------------------------------------------------------------
hcs_index_reader! {
    /// Olympus ScanR HCS reader (`.xml`).
    ///
    /// Reads an XML index file describing a plate scan, locates TIFF images,
    /// and delegates pixel I/O to `TiffReader`.
    pub struct ScanrReader;
    extensions: ["xml"];
    format_label: "Olympus ScanR";
}

// ---------------------------------------------------------------------------
// 12. Yokogawa CellVoyager (.mes, .mlf)
// ---------------------------------------------------------------------------
hcs_index_reader! {
    /// Yokogawa CellVoyager HCS reader (`.mes`, `.mlf`).
    ///
    /// Reads a MES/MLF measurement index file, locates referenced TIFF images,
    /// and delegates pixel I/O to `TiffReader`.
    pub struct CellVoyagerReader;
    extensions: ["mes", "mlf"];
    format_label: "Yokogawa CellVoyager";
}

// ---------------------------------------------------------------------------
// 14. GE InCell 3000 (.xdce — XML index)
// ---------------------------------------------------------------------------
hcs_index_reader! {
    /// GE InCell 3000 HCS reader (`.xdce`).
    ///
    /// Reads an XDCE XML index file, locates referenced TIFF images, and
    /// delegates pixel I/O to `TiffReader`.
    pub struct InCell3000Reader;
    extensions: ["xdce"];
    format_label: "GE InCell 3000";
}

// ---------------------------------------------------------------------------
// 15. RCPNL (.rcpnl)
// ---------------------------------------------------------------------------
hcs_index_reader! {
    /// RCPNL format reader (`.rcpnl`).
    ///
    /// Reads an RCPNL index file, locates referenced TIFF images, and delegates
    /// pixel I/O to `TiffReader`.
    pub struct RcpnlReader;
    extensions: ["rcpnl"];
    format_label: "RCPNL";
}

// ---------------------------------------------------------------------------
// 13. Tecan plate reader (.asc — tab-separated plate data)
// ---------------------------------------------------------------------------

/// Tecan plate reader (`.asc`).
///
/// Reads a tab-separated `.asc` text file containing plate reader measurements.
/// Each row corresponds to a plate row and each column to a plate column. Values
/// are stored as `Float32` pixel data in a 2-D image.
pub struct TecanReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Vec<u8>,
}

impl TecanReader {
    pub fn new() -> Self {
        TecanReader { path: None, meta: None, pixel_data: Vec::new() }
    }
}

impl Default for TecanReader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for TecanReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("asc"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let mut rows: Vec<Vec<f32>> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Tecan .asc files are tab-separated; also accept spaces
            let cells: Vec<f32> = line
                .split(|c: char| c == '\t' || c == ' ')
                .filter(|s| !s.is_empty())
                .filter_map(|s| s.trim().parse::<f64>().ok().map(|v| v as f32))
                .collect();
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        if rows.is_empty() {
            return Err(BioFormatsError::Format(
                "Tecan: .asc file contains no numeric data".to_string(),
            ));
        }
        let height = rows.len() as u32;
        let width = rows.iter().map(|r| r.len()).max().unwrap_or(0) as u32;
        // Build Float32 pixel buffer (row-major, zero-padded for short rows)
        let mut pixel_data = Vec::with_capacity((width * height * 4) as usize);
        for row in &rows {
            for x in 0..width as usize {
                let val = if x < row.len() { row[x] } else { 0.0f32 };
                pixel_data.extend_from_slice(&val.to_le_bytes());
            }
        }
        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "format".to_string(),
            MetadataValue::String("Tecan".to_string()),
        );
        series_metadata.insert(
            "plate_rows".to_string(),
            MetadataValue::Int(height as i64),
        );
        series_metadata.insert(
            "plate_columns".to_string(),
            MetadataValue::Int(width as i64),
        );

        self.path = Some(path.to_path_buf());
        self.pixel_data = pixel_data;
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Float32,
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
        self.pixel_data.clear();
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

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        Ok(self.pixel_data.clone())
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bpp = 4usize; // Float32
        let row_stride = meta.size_x as usize * bpp;
        let mut buf = Vec::with_capacity(w as usize * h as usize * bpp);
        for row in y..(y + h) {
            let start = row as usize * row_stride + x as usize * bpp;
            let end = start + w as usize * bpp;
            if end <= self.pixel_data.len() {
                buf.extend_from_slice(&self.pixel_data[start..end]);
            } else {
                buf.extend(std::iter::repeat(0u8).take(w as usize * bpp));
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

    fn resolution_count(&self) -> usize { 1 }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!("resolution {} out of range", level)))
        } else {
            Ok(())
        }
    }
}
