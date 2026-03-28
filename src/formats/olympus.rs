//! Olympus FV1000 OIF directory format reader.
//!
//! An OIF file is a Windows INI-style text file that describes a multi-channel,
//! multi-z microscopy acquisition. Pixel data are stored as individual TIFF files
//! in a companion directory named `<stem>.files/` or `<stem>/`.
//!
//! OIF header sections of interest:
//!   [Axis 0 Info] → AxisName=X, MaxSize=<width>
//!   [Axis 1 Info] → AxisName=Y, MaxSize=<height>
//!   [Axis 2 Info] → AxisName=Z (or T or C), MaxSize=<count>
//!   [Reference Image Info] → Valid=1, ...
//!   [Channel 1 Info] → various channel params
//!
//! Each plane is stored as an individual TIFF in the companion dir, named e.g.:
//!   s_C001Z001T001.tif  or  C1Z001T001.tif  etc.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

fn parse_oif_header(path: &Path) -> Result<(u32, u32, u32, u32, u32, PixelType)> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let reader = BufReader::new(f);

    let mut axes: Vec<(String, u32)> = Vec::new(); // (name, max_size) per axis
    let mut current_axis_name = String::new();
    let mut current_axis_max  = 0u32;
    let mut in_axis = false;
    let mut bit_depth = 8u32;

    for line in reader.lines() {
        let line = line.map_err(BioFormatsError::Io)?;
        let t = line.trim();

        if t.starts_with('[') {
            // Save previous axis if any
            if in_axis && !current_axis_name.is_empty() {
                axes.push((current_axis_name.clone(), current_axis_max));
            }
            current_axis_name.clear();
            current_axis_max = 0;
            in_axis = t.to_ascii_lowercase().contains("axis") && t.to_ascii_lowercase().contains("info");
            continue;
        }

        if in_axis {
            let lo = t.to_ascii_lowercase();
            if lo.starts_with("axisname") {
                if let Some(v) = t.splitn(2, '=').nth(1) {
                    current_axis_name = v.trim().to_ascii_uppercase();
                }
            } else if lo.starts_with("maxsize") {
                if let Some(v) = t.splitn(2, '=').nth(1) {
                    if let Ok(n) = v.trim().parse::<u32>() {
                        current_axis_max = n;
                    }
                }
            }
        }

        // Bit depth from any section
        let lo = t.to_ascii_lowercase();
        if lo.starts_with("bitcountperchanel") || lo.starts_with("bitcount") || lo.starts_with("bitspersample") {
            if let Some(v) = t.splitn(2, '=').nth(1) {
                if let Ok(n) = v.trim().parse::<u32>() { bit_depth = n; }
            }
        }
    }
    // Save last axis
    if in_axis && !current_axis_name.is_empty() {
        axes.push((current_axis_name.clone(), current_axis_max));
    }

    let mut size_x = 1u32;
    let mut size_y = 1u32;
    let mut size_z = 1u32;
    let mut size_c = 1u32;
    let mut size_t = 1u32;
    for (name, max) in &axes {
        match name.as_str() {
            "X" => size_x = (*max).max(1),
            "Y" => size_y = (*max).max(1),
            "Z" => size_z = (*max).max(1),
            "C" | "LAMBDA" | "WAVELENGTH" => size_c = (*max).max(1),
            "T" | "TIME" => size_t = (*max).max(1),
            _ => {}
        }
    }

    let pixel_type = match bit_depth {
        8  => PixelType::Uint8,
        16 => PixelType::Uint16,
        32 => PixelType::Float32,
        _  => PixelType::Uint16,
    };

    Ok((size_x, size_y, size_z, size_c, size_t, pixel_type))
}

/// Find companion TIFF files in the .files/ directory.
fn find_companion_dir(oif_path: &Path) -> Option<PathBuf> {
    let stem = oif_path.file_stem()?;
    let parent = oif_path.parent()?;
    // Try "<stem>.files" then "<stem>"
    let d1 = parent.join(format!("{}.files", stem.to_string_lossy()));
    if d1.is_dir() { return Some(d1); }
    let d2 = parent.join(stem);
    if d2.is_dir() { return Some(d2); }
    None
}

pub struct OifReader {
    oif_path: Option<PathBuf>,
    companion_dir: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    tiff_files: Vec<PathBuf>, // sorted list of plane TIFF files
}

impl OifReader {
    pub fn new() -> Self {
        OifReader { oif_path: None, companion_dir: None, meta: None, tiff_files: Vec::new() }
    }
}
impl Default for OifReader { fn default() -> Self { Self::new() } }

impl FormatReader for OifReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("oif"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // OIF is a Windows INI (UTF-16 or UTF-8); check for "[FileInformation]" or "[File Info]"
        let s = std::str::from_utf8(&header[..header.len().min(128)]).unwrap_or("");
        s.contains("[FileInformation]") || s.contains("[File Info]") || s.contains("[Version Info]")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (size_x, size_y, size_z, size_c, size_t, pixel_type) = parse_oif_header(path)?;
        let image_count = size_z * size_c * size_t;
        let bpp = (pixel_type.bytes_per_sample() * 8) as u8;

        // Collect companion TIFF paths
        let mut tiff_files: Vec<PathBuf> = Vec::new();
        if let Some(dir) = find_companion_dir(path) {
            let mut entries: Vec<PathBuf> = std::fs::read_dir(&dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| {
                            let ext = p.extension().and_then(|e| e.to_str())
                                .map(|e| e.to_ascii_lowercase());
                            matches!(ext.as_deref(), Some("tif") | Some("tiff"))
                        })
                        .collect()
                })
                .unwrap_or_default();
            entries.sort();
            tiff_files = entries;
            self.companion_dir = Some(dir);
        }

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Olympus FV1000 OIF".into()));

        self.meta = Some(ImageMetadata {
            size_x, size_y, size_z, size_c, size_t,
            pixel_type, bits_per_pixel: bpp,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false, is_interleaved: false, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.tiff_files = tiff_files;
        self.oif_path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.oif_path = None; self.companion_dir = None;
        self.meta = None; self.tiff_files.clear(); Ok(())
    }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }

        // Load from companion TIFF file
        if let Some(tiff_path) = self.tiff_files.get(plane_index as usize) {
            let mut reader = crate::tiff::TiffReader::new();
            reader.set_id(tiff_path)?;
            return reader.open_bytes(0);
        }

        Err(BioFormatsError::Format(format!(
            "OIF: no companion TIFF for plane {}", plane_index
        )))
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize*bps .. x as usize*bps + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
