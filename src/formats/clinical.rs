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
    i16::from_be_bytes([b[off], b[off + 1]])
}

pub struct Ecat7Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl Ecat7Reader {
    pub fn new() -> Self {
        Ecat7Reader {
            path: None,
            meta: None,
            data_offset: 1024,
        }
    }
}
impl Default for Ecat7Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for Ecat7Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("v"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 14 {
            return false;
        }
        // Magic starts with "MATRIX"
        header[..6] == b"MATRIX"[..]
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Per Ecat7Reader.java: main header is 512 bytes, then a 512-byte
        // directory block, then a per-matrix subheader. The first matrix
        // subheader begins at offset 1024; sizeZ/sizeT come from the main
        // header, while sizeX/sizeY/dataType come from the subheader after
        // skipping 512 bytes (i.e. at offset 1024).
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        // Read main header (512) + directory (512) + start of subheader.
        let mut hdr = vec![0u8; 1024 + 8];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        // Main header values (big-endian).
        let file_type = r_i16_be(&hdr, 50);
        // Following the Java field-by-field reads, facilityName ends at
        // offset 352; sizeZ (short) is at 352 and sizeT (short) at 354.
        let size_z = r_i16_be(&hdr, 352).max(1) as u32;
        let size_t = r_i16_be(&hdr, 354).max(1) as u32;

        // Subheader begins at offset 1024 (after main header + 512 skip).
        // Java: dataType (short), numDimensions (short), sizeX (short),
        // sizeY (short).
        let data_type = r_i16_be(&hdr, 1024);
        // numDimensions at 1026
        let size_x = r_i16_be(&hdr, 1026 + 2).max(1) as u32;
        let size_y = r_i16_be(&hdr, 1026 + 4).max(1) as u32;

        let (pixel_type, bpp): (PixelType, u8) = match data_type {
            6 => (PixelType::Uint16, 16),
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "ECAT7 unsupported data type: {}",
                    data_type
                )))
            }
        };

        let size_c = 1u32;
        let image_count = size_z * size_t * size_c;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("ECAT7 PET".into()));
        meta_map.insert("file_type".into(), MetadataValue::Int(file_type as i64));
        meta_map.insert("Data type".into(), MetadataValue::Int(data_type as i64));

        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel: bpp,
            image_count,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: false, // ECAT7 is big-endian
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        // HEADER_SIZE in Java is 1536: main header (512) + directory (512) +
        // first subheader (512). Plane data starts after the first subheader.
        self.data_offset = 1536;
        self.path = Some(path.to_path_buf());
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
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;

        // Java Ecat7Reader.openBytes: there is an interleaved 512-byte
        // subheader before every Z-plane, plus an extra 512 bytes every 30
        // planes. tSkip = sum over i in 0..z { 512; +512 if i>0 && i%30==0 }.
        // The Z-coordinate is derived from the plane index via getZCTCoords;
        // for dimensionOrder XYZTC with sizeC=1 the Z-coordinate is
        // plane_index % sizeZ.
        let size_z = meta.size_z.max(1);
        let z = plane_index % size_z;
        let mut t_skip: u64 = 0;
        for i in 0..z {
            t_skip += 512;
            if i > 0 && (i % 30) == 0 {
                t_skip += 512;
            }
        }
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64 + t_skip;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
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
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
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

fn parse_inveon_header(path: &Path) -> Result<(u32, u32, u32, PixelType, u8, bool)> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let reader = BufReader::new(f);

    let mut nx = 1u32;
    let mut ny = 1u32;
    let mut nz = 1u32;
    let mut data_type = 1i32; // default: INT8 (Java setDataType default)

    for line in reader.lines() {
        let line = line.map_err(BioFormatsError::Io)?;
        let t = line.trim();
        if t.starts_with('#') {
            continue;
        }
        let lo = t.to_ascii_lowercase();
        let parts: Vec<&str> = t.split_ascii_whitespace().collect();
        if lo.starts_with("x_dimension") {
            if let Some(v) = parts.get(1).and_then(|s| s.parse::<u32>().ok()) {
                nx = v.max(1);
            }
        } else if lo.starts_with("y_dimension") {
            if let Some(v) = parts.get(1).and_then(|s| s.parse::<u32>().ok()) {
                ny = v.max(1);
            }
        } else if lo.starts_with("z_dimension") {
            if let Some(v) = parts.get(1).and_then(|s| s.parse::<u32>().ok()) {
                nz = v.max(1);
            }
        } else if lo.starts_with("data_type") {
            if let Some(v) = parts.get(1).and_then(|s| s.parse::<i32>().ok()) {
                data_type = v;
            }
        }
    }

    // Per InveonReader.setDataType:
    //   default → INT8, little-endian
    //   2 → INT16  LE
    //   3 → INT32  LE
    //   4 → FLOAT  LE
    //   5 → FLOAT  BE
    //   6 → INT16  BE
    //   7 → INT32  BE
    // (case 1 is not listed, so it falls through to the INT8/LE default.)
    let (pixel_type, bpp, little_endian): (PixelType, u8, bool) = match data_type {
        2 => (PixelType::Int16, 16, true),
        3 => (PixelType::Int32, 32, true),
        4 => (PixelType::Float32, 32, true),
        5 => (PixelType::Float32, 32, false),
        6 => (PixelType::Int16, 16, false),
        7 => (PixelType::Int32, 32, false),
        _ => (PixelType::Int8, 8, true),
    };

    Ok((nx, ny, nz, pixel_type, bpp, little_endian))
}

pub struct InveonReader {
    hdr_path: Option<PathBuf>,
    img_path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl InveonReader {
    pub fn new() -> Self {
        InveonReader {
            hdr_path: None,
            img_path: None,
            meta: None,
        }
    }
}
impl Default for InveonReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for InveonReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // Inveon .hdr files could conflict with Analyze; check for .img companion
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if !matches!(ext.as_deref(), Some("hdr")) {
            return false;
        }
        // Check if a .img companion exists
        let stem = path.file_stem().unwrap_or_default();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        parent
            .join(format!("{}.img", stem.to_string_lossy()))
            .exists()
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let stem = path.file_stem().unwrap_or_default();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));

        let hdr_path = if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("hdr"))
            .unwrap_or(false)
        {
            path.to_path_buf()
        } else {
            parent.join(format!("{}.hdr", stem.to_string_lossy()))
        };
        let img_path = parent.join(format!("{}.img", stem.to_string_lossy()));

        let (nx, ny, nz, pixel_type, bpp, little_endian) = parse_inveon_header(&hdr_path)?;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert(
            "format".into(),
            MetadataValue::String("Siemens Inveon".into()),
        );

        self.meta = Some(ImageMetadata {
            size_x: nx,
            size_y: ny,
            size_z: nz,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
            image_count: nz,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: little_endian,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.hdr_path = Some(hdr_path);
        self.img_path = Some(img_path);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.hdr_path = None;
        self.img_path = None;
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
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let offset = plane_index as u64 * plane_bytes as u64;
        let img_path = self
            .img_path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(img_path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
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
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
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

/// Split a "{a, b, c}" style array value into trimmed, unquoted elements.
fn parse_fdf_array(value: &str) -> Vec<String> {
    value
        .replace(['{', '}'], "")
        .split(',')
        .map(|s| s.replace('"', "").trim().to_string())
        .collect()
}

#[allow(clippy::type_complexity)]
fn parse_fdf_header(path: &Path) -> Result<(u32, u32, u32, u32, PixelType, u8, bool, u64)> {
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

    // Per VarianFDFReader.parseFDF: dimensions come from matrix[]={x,y,z},
    // pixel type from bits + *storage, and endianness from the bigendian key.
    let mut size_x = 1u32;
    let mut size_y = 1u32;
    let mut size_z = 1u32;
    let mut size_t = 1u32;
    let mut stored_floats = false;
    let mut bits = 0u32;
    let mut pixel_type = PixelType::Uint8;
    // FDF default is big-endian unless "bigendian = 1" sets little-endian.
    // Java only sets littleEndian when a bigendian key is present; the
    // RandomAccessInputStream default is big-endian.
    let mut little_endian = false;

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if line.starts_with('#') {
            continue;
        }
        // Java: type = line[0..firstSpace]; var = line[firstSpace..'='];
        //       value = line['='+1 .. ';']
        let space = match line.find(' ') {
            Some(s) => s,
            None => continue,
        };
        let eq = match line.find('=') {
            Some(e) => e,
            None => continue,
        };
        if space >= eq {
            continue;
        }
        let var = line[space..eq].trim();
        let value_end = line.find(';').unwrap_or(line.len());
        if eq + 1 > value_end {
            continue;
        }
        let value = line[eq + 1..value_end].trim();

        if var == "*storage" {
            stored_floats = value == "\"float\"";
        }
        if var == "bits" {
            bits = value.parse::<u32>().unwrap_or(0);
            pixel_type = match value {
                "8" => PixelType::Uint8,
                "16" => PixelType::Uint16,
                "32" => {
                    if stored_floats {
                        PixelType::Float32
                    } else {
                        PixelType::Uint32
                    }
                }
                _ => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Unsupported FDF bits: {}",
                        value
                    )))
                }
            };
        } else if var == "matrix[]" {
            let values = parse_fdf_array(value);
            if let Some(v) = values.first() {
                if let Ok(p) = v.trim().parse::<f64>() {
                    size_x = (p as i64).max(1) as u32;
                }
            }
            if let Some(v) = values.get(1) {
                if let Ok(p) = v.trim().parse::<f64>() {
                    size_y = (p as i64).max(1) as u32;
                }
            }
            if let Some(v) = values.get(2) {
                if let Ok(p) = v.trim().parse::<f64>() {
                    size_z = (p as i64).max(1) as u32;
                }
            }
        } else if var == "slices" {
            size_z = value.parse::<u32>().unwrap_or(1).max(1);
        } else if var == "echoes" {
            // Java VarianFDFReader.parseFDF: m.sizeT = echoes.
            size_t = value.parse::<u32>().unwrap_or(1).max(1);
        } else if var == "bigendian" {
            little_endian = value == "0";
        }
    }

    let bpp = if bits > 0 {
        bits as u8
    } else {
        (pixel_type.bytes_per_sample() * 8) as u8
    };

    Ok((
        size_x,
        size_y,
        size_z,
        size_t,
        pixel_type,
        bpp,
        little_endian,
        data_offset,
    ))
}

pub struct FdfReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl FdfReader {
    pub fn new() -> Self {
        FdfReader {
            path: None,
            meta: None,
            data_offset: 0,
        }
    }
}
impl Default for FdfReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for FdfReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
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
        let (nx, ny, nz, nt, pixel_type, bpp, little_endian, data_offset) =
            parse_fdf_header(path)?;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert(
            "format".into(),
            MetadataValue::String("Varian FDF MRI".into()),
        );

        // Java VarianFDFReader: imageCount = sizeZ * sizeC * sizeT.
        let image_count = nz.max(1) * nt.max(1);
        self.meta = Some(ImageMetadata {
            size_x: nx,
            size_y: ny,
            size_z: nz,
            size_c: 1,
            size_t: nt,
            pixel_type,
            bits_per_pixel: bpp,
            image_count,
            // Java VarianFDFReader uses dimensionOrder "XYTZC".
            dimension_order: DimensionOrder::XYTZC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: little_endian,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.data_offset = data_offset;
        self.path = Some(path.to_path_buf());
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
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;

        // Java VarianFDFReader.openBytes flips the rows vertically
        // (lower-left origin → top-left origin).
        let row = meta.size_x as usize * bps;
        let h = meta.size_y as usize;
        let mut row_buf = vec![0u8; row];
        for r in 0..h / 2 {
            let src = r * row;
            let dest = (h - r - 1) * row;
            row_buf.copy_from_slice(&buf[src..src + row]);
            buf.copy_within(dest..dest + row, src);
            buf[dest..dest + row].copy_from_slice(&row_buf);
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
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let bps = meta.pixel_type.bytes_per_sample();
        let row = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src = &full[(y as usize + r) * row..];
            out.extend_from_slice(&src[x as usize * bps..x as usize * bps + out_row]);
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
