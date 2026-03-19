//! Zeiss ZVI format reader (OLE2/CFB container).
//!
//! ZVI is the Zeiss AxioVision proprietary microscopy format.
//! It uses OLE2 Compound File Binary (CFB) as its container — the same
//! format as old Microsoft Office .doc/.xls files.
//!
//! Key streams:
//!   /Image/CONTENTS            — global metadata (width, height, pixel type)
//!   /Image/Item(N)/CONTENTS    — per-plane pixel data (N is 1-based)
//!   /Image/Item(N)/Tags/CONTENTS — per-plane z/c/t indices

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

pub struct ZviReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    planes: Vec<ZviPlane>,
    bytes_per_pixel: usize,
    is_rgb: bool,
}

struct ZviPlane {
    /// Stream path inside the CFB, e.g. "/Image/Item(1)/CONTENTS"
    stream_path: String,
    z: u32,
    c: u32,
    t: u32,
}

impl ZviReader {
    pub fn new() -> Self {
        ZviReader {
            path: None,
            meta: None,
            planes: Vec::new(),
            bytes_per_pixel: 1,
            is_rgb: false,
        }
    }
}

impl Default for ZviReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Read 4 little-endian u32s from a byte slice at the given offset.
fn read_u32_le(data: &[u8], offset: usize) -> Option<u32> {
    let b = data.get(offset..offset + 4)?;
    Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
}

fn parse_zvi(path: &Path) -> Result<(ImageMetadata, Vec<ZviPlane>, usize, bool)> {
    let mut comp = cfb::open(path)
        .map_err(|e| BioFormatsError::Format(format!("ZVI CFB open error: {e}")))?;

    // ── Read global image metadata from /Image/CONTENTS ─────────────────────
    let (size_x, size_y, pixel_type, bytes_per_pixel, is_rgb) = {
        let mut stream = comp.open_stream("/Image/CONTENTS")
            .map_err(|e| BioFormatsError::Format(format!("ZVI: /Image/CONTENTS missing: {e}")))?;
        let mut data = Vec::new();
        stream.read_to_end(&mut data)
            .map_err(|e| BioFormatsError::Io(e))?;

        // Java source: width at offset 0, height at 4, pixel_type at 8 (all u32 LE)
        let w = read_u32_le(&data, 0).unwrap_or(512);
        let h = read_u32_le(&data, 4).unwrap_or(512);
        let pt_raw = read_u32_le(&data, 8).unwrap_or(1);

        // Pixel type codes from ZeissZVIReader.java:
        // 1 = uint8 (grayscale), 2 = uint16, 3 = float32
        // 4 = RGB24 (3 bytes/pixel), 5 = BGR32, 6 = RGBA
        let (pixel_type, bpp, rgb) = match pt_raw {
            1 => (PixelType::Uint8,  1usize, false),
            2 => (PixelType::Uint16, 2usize, false),
            3 => (PixelType::Float32, 4usize, false),
            4 => (PixelType::Uint8,  3usize, true),   // RGB24
            5 => (PixelType::Uint8,  4usize, true),   // BGR32 — treat as RGBA
            6 => (PixelType::Uint8,  4usize, true),   // RGBA
            _ => (PixelType::Uint8,  1usize, false),
        };

        (w, h, pixel_type, bpp, rgb)
    };

    // ── Enumerate plane streams ───────────────────────────────────────────────
    // Collect all "/Image/Item(N)/CONTENTS" paths
    let mut item_paths: Vec<String> = comp
        .walk()
        .filter_map(|entry| {
            let p = entry.path().to_string_lossy().to_string();
            // Match "/Image/Item(N)/CONTENTS"
            if p.starts_with("/Image/Item(") && p.ends_with(")/CONTENTS") {
                Some(p)
            } else {
                None
            }
        })
        .collect();

    // Sort lexicographically — Item(1), Item(10), Item(2)… numeric sort is nicer
    item_paths.sort_by(|a, b| {
        let num = |s: &str| -> u32 {
            s.trim_start_matches("/Image/Item(")
                .split(')')
                .next()
                .and_then(|n| n.parse().ok())
                .unwrap_or(0)
        };
        num(a).cmp(&num(b))
    });

    let mut planes: Vec<ZviPlane> = Vec::with_capacity(item_paths.len());

    for stream_path in item_paths {
        let mut stream = match comp.open_stream(&stream_path) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let mut data = Vec::new();
        if stream.read_to_end(&mut data).is_err() || data.len() < 16 {
            continue;
        }
        // First 16 bytes: 4× little-endian u32 → [z, channel, timepoint, tile]
        let z    = read_u32_le(&data, 0).unwrap_or(0);
        let c    = read_u32_le(&data, 4).unwrap_or(0);
        let t    = read_u32_le(&data, 8).unwrap_or(0);
        let tile = read_u32_le(&data, 12).unwrap_or(0);

        // Skip tiled data (tile > 0) — not supported in v1
        if tile > 0 {
            continue;
        }

        planes.push(ZviPlane { stream_path, z, c, t });
    }

    if planes.is_empty() {
        return Err(BioFormatsError::Format("ZVI: no image planes found".into()));
    }

    // ── Derive dimension sizes from max indices ───────────────────────────────
    let size_z = planes.iter().map(|p| p.z).max().unwrap_or(0) + 1;
    let size_c = planes.iter().map(|p| p.c).max().unwrap_or(0) + 1;
    let size_t = planes.iter().map(|p| p.t).max().unwrap_or(0) + 1;

    // Sort planes by (t, c, z) to give a canonical XYZCT order
    planes.sort_by_key(|p| (p.t, p.c, p.z));

    let image_count = size_z * size_c * size_t;

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (bytes_per_pixel * 8) as u8,
        image_count,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb,
        is_interleaved: is_rgb,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        series_metadata: HashMap::new(),
        lookup_table: None,
    };

    Ok((meta, planes, bytes_per_pixel, is_rgb))
}

/// Decode pixel data from a ZVI plane stream (after the 16-byte header).
fn decode_plane_data(data: &[u8]) -> Result<Vec<u8>> {
    if data.len() < 16 {
        return Err(BioFormatsError::Format("ZVI: plane stream too short".into()));
    }
    let payload = &data[16..];

    // Compression detection
    if payload.len() >= 2 && payload[0] == 0xFF && payload[1] == 0xD8 {
        // JPEG
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(payload));
        let pixels = decoder
            .decode()
            .map_err(|e| BioFormatsError::Format(format!("ZVI JPEG decode: {e}")))?;
        return Ok(pixels);
    }

    // Check for "WZL" Zlib marker in first 32 bytes
    let wzl_pos = payload
        .windows(3)
        .take(32)
        .position(|w| w == b"WZL");
    if let Some(pos) = wzl_pos {
        // Skip to after the 8-byte sub-header that follows "WZL"
        let zlib_start = pos + 8;
        if zlib_start < payload.len() {
            let mut decoder = flate2::read::ZlibDecoder::new(&payload[zlib_start..]);
            let mut out = Vec::new();
            decoder
                .read_to_end(&mut out)
                .map_err(|e| BioFormatsError::Format(format!("ZVI zlib decode: {e}")))?;
            return Ok(out);
        }
    }

    // Raw uncompressed
    Ok(payload.to_vec())
}

impl FormatReader for ZviReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("zvi"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // OLE2 CFB magic — shared with other OLE2 files, so also require the context
        // that the caller will have already checked the extension separately.
        // For the magic-byte pass we require both magic + a deferred extension check
        // is not possible here (no path), so we return false to force extension path.
        // Actually we CAN check: bytes 0-3 must match AND the call site checks extension
        // too via is_this_type_by_name. But the registry tries magic first; to avoid
        // false-matching .doc/.xls/.oib etc. we intentionally return false here
        // and let the extension fallback handle ZVI.
        //
        // Returning false from magic means the registry will try is_this_type_by_name
        // next, which checks the .zvi extension.
        let _ = header;
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (meta, planes, bpp, is_rgb) = parse_zvi(path)?;
        self.meta = Some(meta);
        self.planes = planes;
        self.path = Some(path.to_path_buf());
        self.bytes_per_pixel = bpp;
        self.is_rgb = is_rgb;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.planes.clear();
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

    fn resolution_count(&self) -> usize { 1 }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!("ZVI: resolution {level} out of range")))
        } else {
            Ok(())
        }
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let stream_path = self.planes
            .get(plane_index as usize)
            .map(|p| p.stream_path.clone())
            .ok_or_else(|| BioFormatsError::PlaneOutOfRange(plane_index))?;

        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?.clone();
        let mut comp = cfb::open(&path)
            .map_err(|e| BioFormatsError::Format(format!("ZVI CFB open: {e}")))?;

        let mut stream = comp.open_stream(&stream_path)
            .map_err(|e| BioFormatsError::Format(format!("ZVI stream {stream_path}: {e}")))?;
        let mut data = Vec::new();
        stream.read_to_end(&mut data)
            .map_err(|e| BioFormatsError::Io(e))?;

        let mut pixels = decode_plane_data(&data)?;

        // BGR→RGB swap for 3-channel (RGB/BGR) images: swap byte[0] ↔ byte[2] per pixel
        if self.is_rgb && self.bytes_per_pixel >= 3 {
            let bpp = self.bytes_per_pixel;
            let mut i = 0;
            while i + bpp <= pixels.len() {
                pixels.swap(i, i + 2);
                i += bpp;
            }
        }

        Ok(pixels)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bpp = self.bytes_per_pixel;
        let row_bytes = meta.size_x as usize * bpp;
        let out_row = w as usize * bpp;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src_start = (y as usize + r) * row_bytes + x as usize * bpp;
            out.extend_from_slice(&full[src_start..src_start + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(0, tx, ty, tw, th)
    }
}
