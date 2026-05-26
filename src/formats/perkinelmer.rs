//! PerkinElmer format readers.
//!
//! - PerkinElmerReader: UltraVIEW spinning disk (.cfg + .rec)
//! - OpenlabRawReader: Openlab Raw (.raw) with "LBLB" magic
//! - PhotonDynamicsReader: Photon Dynamics (.pds) extension-only

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ── Helpers ──────────────────────────────────────────────────────────────────

fn default_meta(w: u32, h: u32, pt: PixelType) -> ImageMetadata {
    let bps = pt.bytes_per_sample();
    ImageMetadata {
        size_x: w,
        size_y: h,
        size_z: 1,
        size_c: 1,
        size_t: 1,
        pixel_type: pt,
        bits_per_pixel: (bps * 8) as u8,
        image_count: 1,
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
    }
}

fn open_bytes_impl(
    path: &Path,
    offset: u64,
    meta: &ImageMetadata,
    plane_index: u32,
) -> Result<Vec<u8>> {
    if plane_index != 0 {
        return Err(BioFormatsError::PlaneOutOfRange(plane_index));
    }
    let bps = meta.pixel_type.bytes_per_sample();
    let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    f.seek(SeekFrom::Start(offset))
        .map_err(BioFormatsError::Io)?;
    let mut buf = vec![0u8; plane_bytes];
    f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
    Ok(buf)
}

fn region_from_full(full: &[u8], meta: &ImageMetadata, x: u32, y: u32, w: u32, h: u32) -> Vec<u8> {
    let bps = meta.pixel_type.bytes_per_sample();
    let row = meta.size_x as usize * bps;
    let out_row = w as usize * bps;
    let mut out = Vec::with_capacity(h as usize * out_row);
    for r in 0..h as usize {
        let src = &full[(y as usize + r) * row..];
        out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
    }
    out
}

// ── PerkinElmerReader ─────────────────────────────────────────────────────────

pub struct PerkinElmerReader {
    path: Option<PathBuf>,
    rec_path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl PerkinElmerReader {
    pub fn new() -> Self {
        PerkinElmerReader {
            path: None,
            rec_path: None,
            meta: None,
        }
    }
}

impl Default for PerkinElmerReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_pe_cfg(path: &Path) -> Result<(ImageMetadata, PathBuf)> {
    let content = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let mut width = None;
    let mut height = None;
    let mut bytes_per_pixel = None;

    for line in content.lines() {
        let line = line.trim();
        if let Some(v) = kv(line, "Image Width") {
            if let Ok(n) = v.parse() {
                width = Some(n);
            }
        } else if let Some(v) = kv(line, "Image Height") {
            if let Ok(n) = v.parse() {
                height = Some(n);
            }
        } else if let Some(v) = kv(line, "Bytes Per Pixel") {
            if let Ok(n) = v.parse() {
                bytes_per_pixel = Some(n);
            }
        }
    }

    let width = width.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("PerkinElmer CFG missing Image Width".to_string())
    })?;
    let height = height.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("PerkinElmer CFG missing Image Height".to_string())
    })?;
    let bytes_per_pixel = bytes_per_pixel.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("PerkinElmer CFG missing Bytes Per Pixel".to_string())
    })?;
    if width == 0 || height == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "PerkinElmer CFG has invalid dimensions {width}x{height}"
        )));
    }
    let pixel_type = match bytes_per_pixel {
        1 => PixelType::Uint8,
        2 => PixelType::Uint16,
        4 => PixelType::Uint32,
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "PerkinElmer CFG Bytes Per Pixel {bytes_per_pixel} is not supported"
            )));
        }
    };
    let rec_path = path.with_extension("rec");
    let meta = default_meta(width, height, pixel_type);
    let required_len = (meta.size_x as u64)
        .checked_mul(meta.size_y as u64)
        .and_then(|n| n.checked_mul(meta.pixel_type.bytes_per_sample() as u64))
        .ok_or_else(|| BioFormatsError::Format("PerkinElmer REC plane size overflows".into()))?;
    let actual_len = std::fs::metadata(&rec_path)
        .map_err(BioFormatsError::Io)?
        .len();
    if actual_len < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "PerkinElmer REC payload is shorter than declared image: got {actual_len} bytes, expected at least {required_len}"
        )));
    }
    Ok((meta, rec_path))
}

fn kv<'a>(line: &'a str, key: &str) -> Option<&'a str> {
    let stripped = line.strip_prefix(key)?.trim_start();
    Some(stripped.strip_prefix('=')?.trim_start())
}

impl FormatReader for PerkinElmerReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if matches!(ext.as_deref(), Some("cfg")) {
            return path.with_extension("rec").exists();
        }
        false
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, rec_path) = parse_pe_cfg(path)?;
        self.path = Some(path.to_path_buf());
        self.rec_path = Some(rec_path);
        self.meta = Some(meta);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.rec_path = None;
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
        let rec = self
            .rec_path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        open_bytes_impl(&rec, 0, meta, plane_index)
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
        let meta = self.meta.as_ref().unwrap();
        Ok(region_from_full(&full, meta, x, y, w, h))
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

// ── OpenlabRawReader ──────────────────────────────────────────────────────────

const OPENLAB_MAGIC: &[u8] = b"LBLB";
const OPENLAB_HEADER_SIZE: u64 = 288;

pub struct OpenlabRawReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl OpenlabRawReader {
    pub fn new() -> Self {
        OpenlabRawReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for OpenlabRawReader {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_openlab(path: &Path) -> Result<ImageMetadata> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < OPENLAB_HEADER_SIZE as usize {
        return Err(BioFormatsError::Format("Openlab header too short".into()));
    }

    // Width at offset 8, Height at offset 12, bit_depth at offset 16 (i32 BE)
    let width = i32::from_be_bytes([data[8], data[9], data[10], data[11]]);
    let height = i32::from_be_bytes([data[12], data[13], data[14], data[15]]);
    let bit_depth = i32::from_be_bytes([data[16], data[17], data[18], data[19]]);
    if width <= 0 || height <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Openlab raw header has invalid dimensions {width}x{height}"
        )));
    }

    let pixel_type = match bit_depth {
        8 => PixelType::Uint8,
        16 => PixelType::Uint16,
        32 => PixelType::Float32,
        _ => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Openlab raw bit depth {bit_depth} is not supported"
            )));
        }
    };

    let meta = default_meta(width as u32, height as u32, pixel_type);
    let required_len = OPENLAB_HEADER_SIZE
        .checked_add(
            (meta.size_x as u64)
                .checked_mul(meta.size_y as u64)
                .and_then(|n| n.checked_mul(meta.pixel_type.bytes_per_sample() as u64))
                .ok_or_else(|| {
                    BioFormatsError::Format("Openlab raw plane size overflows".into())
                })?,
        )
        .ok_or_else(|| BioFormatsError::Format("Openlab raw file size overflows".into()))?;
    if (data.len() as u64) < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Openlab raw pixel payload is shorter than declared image: got {} bytes, expected at least {required_len}",
            data.len()
        )));
    }

    Ok(meta)
}

impl FormatReader for OpenlabRawReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("raw"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 4 && header[0..4] == *OPENLAB_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = parse_openlab(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
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
        let path = self
            .path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?
            .clone();
        open_bytes_impl(&path, OPENLAB_HEADER_SIZE, meta, plane_index)
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
        let meta = self.meta.as_ref().unwrap();
        Ok(region_from_full(&full, meta, x, y, w, h))
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

// ── PhotonDynamicsReader ──────────────────────────────────────────────────────

pub struct PhotonDynamicsReader {
    path: Option<PathBuf>,
    pixels_path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    record_width: usize,
    reverse_x: bool,
    reverse_y: bool,
}

impl PhotonDynamicsReader {
    pub fn new() -> Self {
        PhotonDynamicsReader {
            path: None,
            pixels_path: None,
            meta: None,
            record_width: 0,
            reverse_x: false,
            reverse_y: false,
        }
    }
}

impl Default for PhotonDynamicsReader {
    fn default() -> Self {
        Self::new()
    }
}

fn photon_dynamics_header_path(path: &Path) -> PathBuf {
    if path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("img"))
        .unwrap_or(false)
    {
        path.with_extension("hdr")
    } else {
        path.to_path_buf()
    }
}

fn photon_dynamics_pixels_path(header_path: &Path) -> PathBuf {
    let upper = header_path.with_extension("IMG");
    if upper.exists() {
        upper
    } else {
        header_path.with_extension("img")
    }
}

fn parse_photon_dynamics_header(
    path: &Path,
) -> Result<(ImageMetadata, PathBuf, usize, bool, bool)> {
    let header_path = photon_dynamics_header_path(path);
    let content = std::fs::read_to_string(&header_path).map_err(BioFormatsError::Io)?;
    if !content.starts_with(" IDENTIFICATION") {
        return Err(BioFormatsError::UnsupportedFormat(
            "Photon Dynamics PDS header missing IDENTIFICATION magic".into(),
        ));
    }

    let mut size_x = None;
    let mut size_y = None;
    let mut record_width = None;
    let mut reverse_x = false;
    let mut reverse_y = false;
    let mut color = None;
    let mut metadata = HashMap::new();

    for raw_line in content.lines() {
        let Some(eq) = raw_line.find('=') else {
            continue;
        };
        let end = raw_line.find('/').unwrap_or(raw_line.len());
        let key = raw_line[..eq].trim();
        let value = raw_line[eq + 1..end].trim().trim_matches('\'').trim();
        metadata.insert(key.to_string(), MetadataValue::String(value.to_string()));

        match key {
            "NXP" => size_x = value.parse::<u32>().ok(),
            "NYP" => size_y = value.parse::<u32>().ok(),
            "SIGNX" => reverse_x = value == "-",
            "SIGNY" => reverse_y = value == "-",
            "COLOR" => color = value.parse::<u32>().ok(),
            "FILE REC LEN" => {
                record_width = value.parse::<usize>().ok().map(|bytes| bytes / 2);
            }
            _ => {}
        }
    }

    let size_x = size_x.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Photon Dynamics PDS header missing NXP".into())
    })?;
    let size_y = size_y.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Photon Dynamics PDS header missing NYP".into())
    })?;
    if size_x == 0 || size_y == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Photon Dynamics PDS has invalid dimensions {size_x}x{size_y}"
        )));
    }

    let mut meta = default_meta(size_x, size_y, PixelType::Uint16);
    meta.dimension_order = DimensionOrder::XYCZT;
    if color == Some(4) {
        meta.size_c = 3;
        meta.is_rgb = true;
    } else if let Some(color) = color {
        meta.is_indexed = color > 0;
    }
    meta.series_metadata = metadata;

    let pixels_path = photon_dynamics_pixels_path(&header_path);
    let record_width = record_width.unwrap_or(size_x as usize).max(size_x as usize);
    let row_pixels = record_width;
    let required_len = (row_pixels as u64)
        .checked_mul(size_y as u64)
        .and_then(|n| n.checked_mul(2))
        .ok_or_else(|| BioFormatsError::Format("Photon Dynamics IMG size overflows".into()))?;
    let actual_len = std::fs::metadata(&pixels_path)
        .map_err(BioFormatsError::Io)?
        .len();
    if actual_len < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Photon Dynamics IMG payload is shorter than declared image: got {actual_len} bytes, expected at least {required_len}"
        )));
    }

    Ok((meta, pixels_path, record_width, reverse_x, reverse_y))
}

fn read_photon_dynamics_plane(
    path: &Path,
    meta: &ImageMetadata,
    record_width: usize,
    reverse_x: bool,
    reverse_y: bool,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    if x.checked_add(w).is_none_or(|end| end > meta.size_x)
        || y.checked_add(h).is_none_or(|end| end > meta.size_y)
    {
        return Err(BioFormatsError::InvalidData(
            "Photon Dynamics region exceeds image bounds".into(),
        ));
    }

    let mut file = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    let mut out = vec![0u8; w as usize * h as usize * 2];
    let read_x = if reverse_x { meta.size_x - w - x } else { x } as usize;
    let read_y = if reverse_y { meta.size_y - h - y } else { y } as usize;
    let row_stride = record_width.max(meta.size_x as usize) * 2;

    for row in 0..h as usize {
        let src = ((read_y + row) * row_stride + read_x * 2) as u64;
        file.seek(SeekFrom::Start(src))
            .map_err(BioFormatsError::Io)?;
        let dst = row * w as usize * 2;
        file.read_exact(&mut out[dst..dst + w as usize * 2])
            .map_err(BioFormatsError::Io)?;
    }

    if reverse_x {
        for row in out.chunks_exact_mut(w as usize * 2) {
            for col in 0..w as usize / 2 {
                let left = col * 2;
                let right = (w as usize - col - 1) * 2;
                row.swap(left, right);
                row.swap(left + 1, right + 1);
            }
        }
    }

    if reverse_y {
        let row_bytes = w as usize * 2;
        for row in 0..h as usize / 2 {
            let top = row * row_bytes;
            let bottom = (h as usize - row - 1) * row_bytes;
            for col in 0..row_bytes {
                out.swap(top + col, bottom + col);
            }
        }
    }

    Ok(out)
}

impl FormatReader for PhotonDynamicsReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("hdr") | Some("img") | Some("pds"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b" IDENTIFICATION")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, pixels_path, record_width, reverse_x, reverse_y) =
            parse_photon_dynamics_header(path)?;
        self.path = Some(photon_dynamics_header_path(path));
        self.pixels_path = Some(pixels_path);
        self.meta = Some(meta);
        self.record_width = record_width;
        self.reverse_x = reverse_x;
        self.reverse_y = reverse_y;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.pixels_path = None;
        self.meta = None;
        self.record_width = 0;
        self.reverse_x = false;
        self.reverse_y = false;
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let pixels = self
            .pixels_path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        read_photon_dynamics_plane(
            pixels,
            meta,
            self.record_width,
            self.reverse_x,
            self.reverse_y,
            0,
            0,
            meta.size_x,
            meta.size_y,
        )
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let pixels = self
            .pixels_path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        read_photon_dynamics_plane(
            pixels,
            meta,
            self.record_width,
            self.reverse_x,
            self.reverse_y,
            x,
            y,
            w,
            h,
        )
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

#[cfg(test)]
mod photon_dynamics_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn tmp_pair(name: &str) -> (PathBuf, PathBuf) {
        let id = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let hdr = std::env::temp_dir().join(format!("{name}_{id}.hdr"));
        let img = hdr.with_extension("IMG");
        (hdr, img)
    }

    fn write_header(path: &Path, sign_x: &str, sign_y: &str, rec_len: usize) {
        std::fs::write(
            path,
            format!(
                " IDENTIFICATION\nNXP = 3\nNYP = 2\nSIGNX = '{sign_x}'\nSIGNY = '{sign_y}'\nCOLOR = 1\nFILE REC LEN = {}\n",
                rec_len * 2
            ),
        )
        .unwrap();
    }

    #[test]
    fn photon_dynamics_reads_companion_img_with_record_padding() {
        let (hdr, img) = tmp_pair("photon_padded");
        write_header(&hdr, "+", "+", 4);
        let samples = [1u16, 2, 3, 99, 4, 5, 6, 88];
        let bytes: Vec<u8> = samples.into_iter().flat_map(u16::to_le_bytes).collect();
        std::fs::write(&img, bytes).unwrap();

        let mut reader = PhotonDynamicsReader::new();
        reader.set_id(&hdr).unwrap();

        let expected: Vec<u8> = [1u16, 2, 3, 4, 5, 6]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(reader.open_bytes(0).unwrap(), expected);

        let crop: Vec<u8> = [2u16, 3, 5, 6]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(), crop);

        let _ = std::fs::remove_file(hdr);
        let _ = std::fs::remove_file(img);
    }

    #[test]
    fn photon_dynamics_applies_reverse_axes_after_reading_region() {
        let (hdr, img) = tmp_pair("photon_reversed");
        write_header(&hdr, "-", "-", 3);
        let samples = [1u16, 2, 3, 4, 5, 6];
        let bytes: Vec<u8> = samples.into_iter().flat_map(u16::to_le_bytes).collect();
        std::fs::write(&img, bytes).unwrap();

        let mut reader = PhotonDynamicsReader::new();
        reader.set_id(&hdr).unwrap();

        let expected: Vec<u8> = [6u16, 5, 4, 3, 2, 1]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(reader.open_bytes(0).unwrap(), expected);

        let crop: Vec<u8> = [6u16, 5, 3, 2]
            .into_iter()
            .flat_map(u16::to_le_bytes)
            .collect();
        assert_eq!(reader.open_bytes_region(0, 0, 0, 2, 2).unwrap(), crop);

        let _ = std::fs::remove_file(hdr);
        let _ = std::fs::remove_file(img);
    }

    #[test]
    fn photon_dynamics_rejects_missing_magic_and_short_img() {
        let (hdr, img) = tmp_pair("photon_invalid");
        std::fs::write(&hdr, b"NXP = 3\nNYP = 2\n").unwrap();
        std::fs::write(&img, []).unwrap();
        let err = PhotonDynamicsReader::new().set_id(&hdr).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message) if message.contains("IDENTIFICATION")
        ));

        write_header(&hdr, "+", "+", 3);
        let err = PhotonDynamicsReader::new().set_id(&hdr).unwrap_err();
        assert!(matches!(
            err,
            BioFormatsError::UnsupportedFormat(message) if message.contains("shorter")
        ));

        let _ = std::fs::remove_file(hdr);
        let _ = std::fs::remove_file(img);
    }
}
