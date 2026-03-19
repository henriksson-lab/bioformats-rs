//! NIfTI-1 and Analyze 7.5 format reader (neuroimaging).
//!
//! Supports:
//! - NIfTI-1 single file (.nii, .nii.gz)
//! - NIfTI-1 paired files (.hdr + .img)
//! - Analyze 7.5 paired files (.hdr + .img)

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use bioformats_common::pixel_type::PixelType;
use bioformats_common::reader::FormatReader;

// ── NIfTI datatype codes ─────────────────────────────────────────────────────
fn nifti_pixel_type(datatype: i16) -> PixelType {
    match datatype {
        2 => PixelType::Uint8,
        4 => PixelType::Int16,
        8 => PixelType::Int32,
        16 => PixelType::Float32,
        64 => PixelType::Float64,
        256 => PixelType::Int8,
        512 => PixelType::Uint16,
        768 => PixelType::Uint32,
        _ => PixelType::Uint8,
    }
}

// ── Header parsing ────────────────────────────────────────────────────────────
//
// NIfTI-1 / Analyze 7.5 header is exactly 348 bytes.
//
// Key offsets (all same between Analyze and NIfTI-1):
//   0-3:   sizeof_hdr (int32, must be 348)
//  40-55:  dim[0..7]  (int16 × 8)
//  70-71:  datatype   (int16)
//  72-73:  bitpix     (int16)
//  76-107: pixdim[0..7] (float32 × 8)
// 108-111: vox_offset (float32) — only meaningful for NIfTI
// 148-227: descrip[80] (char)
// 344-347: magic[4]

const HDR_SIZE: usize = 348;

#[derive(Debug)]
struct NiftiHeader {
    /// Number of dimensions (dim[0])
    ndim: u16,
    /// dim[1..ndim]
    dim: [u16; 7],
    datatype: i16,
    bitpix: i16,
    /// pixdim[1..ndim] — voxel spacing
    pixdim: [f32; 7],
    /// Byte offset of data in the data file (for .nii single-file)
    vox_offset: f32,
    /// "n+1\0" = single .nii, "ni1\0" = paired, "\0\0\0\0" = Analyze
    magic: [u8; 4],
    little_endian: bool,
    descrip: String,
}

fn read_i16(buf: &[u8], off: usize, le: bool) -> i16 {
    let b = [buf[off], buf[off + 1]];
    if le { i16::from_le_bytes(b) } else { i16::from_be_bytes(b) }
}
fn read_u16_h(buf: &[u8], off: usize, le: bool) -> u16 {
    let b = [buf[off], buf[off + 1]];
    if le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) }
}
fn read_i32(buf: &[u8], off: usize, le: bool) -> i32 {
    let b = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
    if le { i32::from_le_bytes(b) } else { i32::from_be_bytes(b) }
}
fn read_f32(buf: &[u8], off: usize, le: bool) -> f32 {
    let b = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
    if le { f32::from_le_bytes(b) } else { f32::from_be_bytes(b) }
}

fn parse_header(buf: &[u8]) -> Result<NiftiHeader> {
    if buf.len() < HDR_SIZE {
        return Err(BioFormatsError::Format(
            "NIfTI/Analyze: header too short".into(),
        ));
    }

    // Detect endianness: sizeof_hdr at offset 0 must be 348.
    let sizeof_le = i32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let sizeof_be = i32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]);
    let le = if sizeof_le == 348 {
        true
    } else if sizeof_be == 348 {
        false
    } else {
        return Err(BioFormatsError::Format(
            "NIfTI/Analyze: invalid sizeof_hdr".into(),
        ));
    };

    let ndim = read_u16_h(buf, 40, le);
    let mut dim = [0u16; 7];
    for i in 0..7 {
        dim[i] = read_u16_h(buf, 42 + i * 2, le);
    }

    let datatype = read_i16(buf, 70, le);
    let bitpix = read_i16(buf, 72, le);

    let mut pixdim = [0f32; 7];
    for i in 0..7 {
        pixdim[i] = read_f32(buf, 80 + i * 4, le);
    }

    let vox_offset = read_f32(buf, 108, le);

    let magic: [u8; 4] = [buf[344], buf[345], buf[346], buf[347]];

    let descrip = std::str::from_utf8(&buf[148..228])
        .unwrap_or("")
        .trim_end_matches('\0')
        .to_string();

    Ok(NiftiHeader { ndim, dim, datatype, bitpix, pixdim, vox_offset, magic, little_endian: le, descrip })
}

fn is_nifti_magic(magic: &[u8; 4]) -> bool {
    magic == b"n+1\0" || magic == b"ni1\0"
}

fn is_nifti_single(magic: &[u8; 4]) -> bool {
    magic == b"n+1\0"
}

fn build_metadata(hdr: &NiftiHeader) -> ImageMetadata {
    let ndim = hdr.ndim.max(1) as usize;

    // dim[1]=x, dim[2]=y, dim[3]=z, dim[4]=t, dim[5]=channels (or 5th dim)
    let size_x = if ndim >= 1 { hdr.dim[0].max(1) as u32 } else { 1 };
    let size_y = if ndim >= 2 { hdr.dim[1].max(1) as u32 } else { 1 };
    let size_z = if ndim >= 3 { hdr.dim[2].max(1) as u32 } else { 1 };
    let size_t = if ndim >= 4 { hdr.dim[3].max(1) as u32 } else { 1 };
    let size_c = if ndim >= 5 { hdr.dim[4].max(1) as u32 } else { 1 };

    let pixel_type = nifti_pixel_type(hdr.datatype);
    let image_count = size_z * size_t * size_c;

    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    if !hdr.descrip.is_empty() {
        meta_map.insert("description".into(), MetadataValue::String(hdr.descrip.clone()));
    }
    meta_map.insert("datatype".into(), MetadataValue::Int(hdr.datatype as i64));
    let format_name = if is_nifti_magic(&hdr.magic) { "NIfTI-1" } else { "Analyze7.5" };
    meta_map.insert("format".into(), MetadataValue::String(format_name.into()));

    ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: hdr.bitpix.max(0) as u8,
        image_count,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: hdr.little_endian,
        resolution_count: 1,
        series_metadata: meta_map,
        lookup_table: None,
    }
}

// ── Reader ────────────────────────────────────────────────────────────────────

pub struct NiftiReader {
    hdr_path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_path: Option<PathBuf>,
    data_offset: u64,
    little_endian: bool,
    is_gz: bool,
}

impl NiftiReader {
    pub fn new() -> Self {
        NiftiReader {
            hdr_path: None,
            meta: None,
            data_path: None,
            data_offset: 0,
            little_endian: true,
            is_gz: false,
        }
    }

    fn load_raw(&self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let data_path = self.data_path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y * meta.size_c) as usize * bps;
        let plane_offset = plane_index as u64 * plane_bytes as u64;

        let f = File::open(data_path).map_err(BioFormatsError::Io)?;

        if self.is_gz {
            // Decompress all then seek (gzip has no random access)
            let mut dec = flate2::read::GzDecoder::new(BufReader::new(f));
            let mut all = Vec::new();
            dec.read_to_end(&mut all).map_err(BioFormatsError::Io)?;
            let start = (self.data_offset + plane_offset) as usize;
            let end = start + plane_bytes;
            if end > all.len() {
                return Err(BioFormatsError::InvalidData("plane out of range".into()));
            }
            Ok(all[start..end].to_vec())
        } else {
            let mut f = f;
            f.seek(SeekFrom::Start(self.data_offset + plane_offset))
                .map_err(BioFormatsError::Io)?;
            let mut buf = vec![0u8; plane_bytes];
            f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
            Ok(buf)
        }
    }
}

impl Default for NiftiReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NiftiReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let name = path.to_string_lossy().to_ascii_lowercase();
        name.ends_with(".nii")
            || name.ends_with(".nii.gz")
            || path.extension().and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("hdr"))
                .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Check sizeof_hdr == 348 at offset 0 (LE or BE)
        if header.len() < 4 {
            return false;
        }
        let le = i32::from_le_bytes([header[0], header[1], header[2], header[3]]) == 348;
        let be = i32::from_be_bytes([header[0], header[1], header[2], header[3]]) == 348;
        // Also verify magic for NIfTI if available
        if (le || be) && header.len() >= 348 {
            // Check magic for NIfTI or zeros for Analyze
            let magic = &header[344..348];
            return magic == b"n+1\0" || magic == b"ni1\0"
                || magic == [0, 0, 0, 0]
                || magic == b"ni1 "; // some older files
        }
        le || be
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy().to_ascii_lowercase();
        let is_gz = path_str.ends_with(".nii.gz");

        // Read and parse header
        let mut hdr_bytes = vec![0u8; HDR_SIZE];
        if is_gz {
            let f = File::open(path).map_err(BioFormatsError::Io)?;
            let mut dec = flate2::read::GzDecoder::new(BufReader::new(f));
            dec.read_exact(&mut hdr_bytes).map_err(BioFormatsError::Io)?;
        } else {
            let mut f = File::open(path).map_err(BioFormatsError::Io)?;
            f.read_exact(&mut hdr_bytes).map_err(BioFormatsError::Io)?;
        }

        let hdr = parse_header(&hdr_bytes)?;
        let meta = build_metadata(&hdr);

        // Determine data file and offset
        let (data_path, data_offset) = if is_nifti_single(&hdr.magic) || is_gz {
            // Single .nii or .nii.gz: data follows header in same file
            let off = if hdr.vox_offset >= HDR_SIZE as f32 {
                hdr.vox_offset as u64
            } else {
                HDR_SIZE as u64 // default to end of header
            };
            (path.to_path_buf(), off)
        } else {
            // Paired: find companion .img file
            let stem = path.file_stem().unwrap_or_default();
            let img_path = path.with_file_name(format!("{}.img", stem.to_string_lossy()));
            (img_path, 0u64)
        };

        self.meta = Some(meta);
        self.hdr_path = Some(path.to_path_buf());
        self.data_path = Some(data_path);
        self.data_offset = data_offset;
        self.little_endian = hdr.little_endian;
        self.is_gz = is_gz;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.hdr_path = None;
        self.meta = None;
        self.data_path = None;
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
        let count = self.meta.as_ref().map(|m| m.image_count).unwrap_or(0);
        if plane_index >= count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.load_raw(plane_index)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().unwrap();
        let spp = meta.size_c as usize;
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = meta.size_x as usize * spp * bps;
        let out_row = w as usize * spp * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * spp * bps;
            out.extend_from_slice(&src[s..s + out_row]);
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
