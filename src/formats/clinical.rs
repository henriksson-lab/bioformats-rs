//! Clinical scanner format readers: ECAT7 PET, Inveon PET/CT, Varian FDF MRI.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ─── ECAT7 PET ────────────────────────────────────────────────────────────────
//
// ECAT7 is a format used by CTI/Siemens PET scanners.
// Main header (512 bytes):
//   Offset 0:  magic_number[14] — "MATRIX72v\0" or similar (null-terminated)
//   Offset 14: original_file_name[32]
//   Offset 46: sw_version (i16)
//   Offset 48: system_type (i16)
//   Offset 50: file_type (i16)
//   Offset 52: serial_number[10]
//   Offset 62: scan_start_time (i32)
//   Offset 66: isotope_code[8]
//   ...
//   Offset 80: num_planes (i16)
//   Offset 82: num_frames (i16)
//   Offset 84: num_gates (i16)
//   Offset 86: num_bed_pos (i16)
//
// After the main header, a directory block (512 bytes) maps matrix codes to
// subheader+data blocks. For simplicity we read only the main header for dims.
// Pixel data type is always int16 for emission data (file_type=1) and
// float32 for sinogram data (file_type=2).

fn r_i16_be(b: &[u8], off: usize) -> i16 {
    i16::from_be_bytes([b[off], b[off+1]])
}

pub struct Ecat7Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl Ecat7Reader {
    pub fn new() -> Self { Ecat7Reader { path: None, meta: None, data_offset: 1024 } }
}
impl Default for Ecat7Reader { fn default() -> Self { Self::new() } }

impl FormatReader for Ecat7Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("v"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 14 { return false; }
        // Magic starts with "MATRIX"
        header[..6] == b"MATRIX"[..]
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; 512];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        let num_planes = r_i16_be(&hdr, 80).max(1) as u32;
        let num_frames = r_i16_be(&hdr, 82).max(1) as u32;
        let file_type  = r_i16_be(&hdr, 50);

        // For ECAT7 image files (file_type 7=volume image), dimensions are in the subheader.
        // As a best-effort, use common PET dimensions: 128×128 with num_planes slices.
        // A real reader would parse subheader blocks.
        let width  = 128u32;
        let height = 128u32;
        let image_count = num_planes * num_frames;

        let (pixel_type, bpp): (PixelType, u8) = if file_type == 7 || file_type == 1 {
            (PixelType::Int16, 16)
        } else {
            (PixelType::Float32, 32)
        };

        // Data starts at offset 1024 (main header 512 + directory block 512)
        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("ECAT7 PET".into()));
        meta_map.insert("file_type".into(), MetadataValue::Int(file_type as i64));

        self.meta = Some(ImageMetadata {
            size_x: width, size_y: height,
            size_z: num_planes, size_c: 1, size_t: num_frames,
            pixel_type, bits_per_pixel: bpp,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false, is_interleaved: false, is_indexed: false,
            is_little_endian: false, // ECAT7 is big-endian
            resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.data_offset = 1024;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> { self.path = None; self.meta = None; Ok(()) }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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

// ─── Inveon PET/CT ────────────────────────────────────────────────────────────
//
// Siemens Inveon preclinical PET/CT stores data as:
//   <stem>.hdr — ASCII text header with key=value lines
//   <stem>.img — raw binary pixel data (default little-endian, float32 or int16)
//
// Key header fields (lower-case):
//   x_dimension <n>
//   y_dimension <n>
//   z_dimension <n>
//   data_type <n>    — 1=uint8, 2=int16, 4=int32, 5=float32, 6=float64
//   scale_factor <f>

fn parse_inveon_header(path: &Path) -> Result<(u32, u32, u32, PixelType, u8)> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let reader = BufReader::new(f);

    let mut nx = 1u32;
    let mut ny = 1u32;
    let mut nz = 1u32;
    let mut data_type = 5i32; // default float32

    for line in reader.lines() {
        let line = line.map_err(BioFormatsError::Io)?;
        let t = line.trim();
        let lo = t.to_ascii_lowercase();
        let parts: Vec<&str> = t.split_ascii_whitespace().collect();
        if lo.starts_with("x_dimension") {
            if let Some(v) = parts.get(1).and_then(|s| s.parse::<u32>().ok()) { nx = v.max(1); }
        } else if lo.starts_with("y_dimension") {
            if let Some(v) = parts.get(1).and_then(|s| s.parse::<u32>().ok()) { ny = v.max(1); }
        } else if lo.starts_with("z_dimension") {
            if let Some(v) = parts.get(1).and_then(|s| s.parse::<u32>().ok()) { nz = v.max(1); }
        } else if lo.starts_with("data_type") {
            if let Some(v) = parts.get(1).and_then(|s| s.parse::<i32>().ok()) { data_type = v; }
        }
    }

    let (pixel_type, bpp): (PixelType, u8) = match data_type {
        1 => (PixelType::Uint8,   8),
        2 => (PixelType::Int16,  16),
        4 => (PixelType::Int32,  32),
        5 => (PixelType::Float32, 32),
        6 => (PixelType::Float64, 64),
        _ => (PixelType::Float32, 32),
    };

    Ok((nx, ny, nz, pixel_type, bpp))
}

pub struct InveonReader {
    hdr_path: Option<PathBuf>,
    img_path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl InveonReader {
    pub fn new() -> Self { InveonReader { hdr_path: None, img_path: None, meta: None } }
}
impl Default for InveonReader { fn default() -> Self { Self::new() } }

impl FormatReader for InveonReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // Inveon .hdr files could conflict with Analyze; check for .img companion
        let ext = path.extension().and_then(|e| e.to_str()).map(|e| e.to_ascii_lowercase());
        if !matches!(ext.as_deref(), Some("hdr")) { return false; }
        // Check if a .img companion exists
        let stem = path.file_stem().unwrap_or_default();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        parent.join(format!("{}.img", stem.to_string_lossy())).exists()
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool { false }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let stem = path.file_stem().unwrap_or_default();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));

        let hdr_path = if path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("hdr")).unwrap_or(false)
        {
            path.to_path_buf()
        } else {
            parent.join(format!("{}.hdr", stem.to_string_lossy()))
        };
        let img_path = parent.join(format!("{}.img", stem.to_string_lossy()));

        let (nx, ny, nz, pixel_type, bpp) = parse_inveon_header(&hdr_path)?;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Siemens Inveon".into()));

        self.meta = Some(ImageMetadata {
            size_x: nx, size_y: ny, size_z: nz, size_c: 1, size_t: 1,
            pixel_type, bits_per_pixel: bpp,
            image_count: nz,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false, is_interleaved: false, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.hdr_path = Some(hdr_path);
        self.img_path = Some(img_path);
        Ok(())
    }

    fn close(&mut self) -> Result<()> { self.hdr_path = None; self.img_path = None; self.meta = None; Ok(()) }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let offset = plane_index as u64 * plane_bytes as u64;
        let img_path = self.img_path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(img_path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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

// ─── Varian FDF MRI ───────────────────────────────────────────────────────────
//
// Varian FDF (Flexible Data Format) stores MRI data.
// The file is a text header followed by binary pixel data.
// The header is a series of C-style declarations:
//   int    ro_size = 256;
//   int    pe_size = 256;
//   int    slices = 16;
//   char   *storage = "float";
//   int    bits = 32;
// The header ends with a 0x0C (form-feed) byte immediately before the pixel data.

fn parse_fdf_header(path: &Path) -> Result<(u32, u32, u32, PixelType, u8, u64)> {
    let mut f = File::open(path).map_err(BioFormatsError::Io)?;
    // Read up to 8 KiB looking for the 0x0C terminator
    let max = 8192usize;
    let mut buf = vec![0u8; max];
    let n = f.read(&mut buf).map_err(BioFormatsError::Io)?;
    buf.truncate(n);

    let ff_pos = buf.iter().position(|&b| b == 0x0C);
    let (header_bytes, data_offset) = if let Some(pos) = ff_pos {
        (&buf[..pos], (pos + 1) as u64)
    } else {
        (&buf[..n], n as u64)
    };

    let text = String::from_utf8_lossy(header_bytes);

    let mut ro_size = 1u32;
    let mut pe_size = 1u32;
    let mut slices  = 1u32;
    let mut storage = "float".to_string();
    let mut bits    = 32u32;

    for line in text.lines() {
        let t = line.trim().trim_end_matches(';');
        let lo = t.to_ascii_lowercase();
        // Pattern: "type name = value" or "type *name = \"value\""
        let parts: Vec<&str> = t.split_ascii_whitespace().collect();
        if lo.contains("ro_size") {
            if let Some(&v) = parts.last() { if let Ok(n) = v.parse::<u32>() { ro_size = n.max(1); } }
        } else if lo.contains("pe_size") {
            if let Some(&v) = parts.last() { if let Ok(n) = v.parse::<u32>() { pe_size = n.max(1); } }
        } else if lo.contains("slices") || lo.contains("slice_no") {
            if let Some(&v) = parts.last() { if let Ok(n) = v.parse::<u32>().or_else(|_| Ok::<u32,()>(1)) { slices = n.max(1); } }
        } else if lo.contains("storage") {
            if let Some(&v) = parts.last() {
                storage = v.trim_matches('"').to_ascii_lowercase();
            }
        } else if lo.contains("bits") {
            if let Some(&v) = parts.last() { if let Ok(n) = v.parse::<u32>() { bits = n; } }
        }
    }

    let (pixel_type, bpp): (PixelType, u8) = if storage.contains("float") {
        match bits {
            64 => (PixelType::Float64, 64),
            _  => (PixelType::Float32, 32),
        }
    } else if storage.contains("integer") || storage.contains("int") {
        match bits {
            8  => (PixelType::Uint8,   8),
            16 => (PixelType::Int16,  16),
            32 => (PixelType::Int32,  32),
            _  => (PixelType::Int16,  16),
        }
    } else {
        (PixelType::Float32, 32)
    };

    Ok((ro_size, pe_size, slices, pixel_type, bpp, data_offset))
}

pub struct FdfReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl FdfReader {
    pub fn new() -> Self { FdfReader { path: None, meta: None, data_offset: 0 } }
}
impl Default for FdfReader { fn default() -> Self { Self::new() } }

impl FormatReader for FdfReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension().and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("fdf"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // FDF files start with "#!/usr/local/fdf/startup" or just "#!/"
        // or with "# " comments. Check for FDF-specific content.
        let s = std::str::from_utf8(&header[..header.len().min(32)]).unwrap_or("");
        s.starts_with("#!/usr/local/fdf") || s.starts_with("# FDF")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let (nx, ny, nz, pixel_type, bpp, data_offset) = parse_fdf_header(path)?;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Varian FDF MRI".into()));

        self.meta = Some(ImageMetadata {
            size_x: nx, size_y: ny, size_z: nz, size_c: 1, size_t: 1,
            pixel_type, bits_per_pixel: bpp,
            image_count: nz,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false, is_interleaved: false, is_indexed: false,
            is_little_endian: true, resolution_count: 1,
            series_metadata: meta_map, lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.data_offset = data_offset;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> { self.path = None; self.meta = None; Ok(()) }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s != 0 { Err(BioFormatsError::SeriesOutOfRange(s)) } else { Ok(()) }
    }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { self.meta.as_ref().expect("set_id not called") }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count { return Err(BioFormatsError::PlaneOutOfRange(plane_index)); }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset)).map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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
