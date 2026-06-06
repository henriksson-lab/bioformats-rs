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

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ── NIfTI datatype codes ─────────────────────────────────────────────────────
//
// Mirrors NiftiReader.populatePixelType. Datatypes 128 (RGB24) and 2304
// (RGBA32) are colour types: they set the pixel type to UINT8 and fix the
// channel count (3 / 4 respectively). The returned `Option<u32>`, when set,
// overrides sizeC for these colour types.
//
// Note: the Java switch has fall-through bugs (missing `break` after cases 128
// and 2304); this uses the clearly intended mapping
// (128 → UINT8 RGB sizeC=3, 2304 → UINT8 RGBA sizeC=4).
fn nifti_pixel_type(datatype: i16) -> Result<(PixelType, Option<u32>)> {
    Ok(match datatype {
        1 | 2 => (PixelType::Uint8, None),
        4 => (PixelType::Int16, None),
        8 => (PixelType::Int32, None),
        16 => (PixelType::Float32, None),
        64 => (PixelType::Float64, None),
        128 => (PixelType::Uint8, Some(3)),
        256 => (PixelType::Int8, None),
        512 => (PixelType::Uint16, None),
        768 => (PixelType::Uint32, None),
        2304 => (PixelType::Uint8, Some(4)),
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Unsupported NIfTI data type: {}",
                other
            )))
        }
    })
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
    ndim: i16,
    /// dim[1..ndim]
    dim: [i16; 7],
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
    if le {
        i16::from_le_bytes(b)
    } else {
        i16::from_be_bytes(b)
    }
}
fn read_f32(buf: &[u8], off: usize, le: bool) -> f32 {
    let b = [buf[off], buf[off + 1], buf[off + 2], buf[off + 3]];
    if le {
        f32::from_le_bytes(b)
    } else {
        f32::from_be_bytes(b)
    }
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

    let ndim = read_i16(buf, 40, le);
    if !(1..=7).contains(&ndim) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "NIfTI/Analyze invalid dimension count {ndim}"
        )));
    }
    let mut dim = [0i16; 7];
    for i in 0..7 {
        dim[i] = read_i16(buf, 42 + i * 2, le);
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

    Ok(NiftiHeader {
        ndim,
        dim,
        datatype,
        bitpix,
        pixdim,
        vox_offset,
        magic,
        little_endian: le,
        descrip,
    })
}

fn is_nifti_magic(magic: &[u8; 4]) -> bool {
    magic == b"n+1\0" || magic == b"ni1\0"
}

fn is_nifti_single(magic: &[u8; 4]) -> bool {
    magic == b"n+1\0"
}

fn build_metadata(hdr: &NiftiHeader) -> Result<ImageMetadata> {
    // Java reads sizeX=dim[1], sizeY=dim[2], sizeZ=dim[3], sizeT=dim[4] and
    // then multiplies sizeC by the extra dims dim[5..] when nDimensions > 4.
    // In this struct hdr.dim[0..6] correspond to NIfTI dim[1..7].
    let size_x = positive_dim(hdr.dim[0], "SizeX")?;
    let size_y = positive_dim(hdr.dim[1], "SizeY")?;
    let mut size_z = optional_dim(hdr.dim[2], "SizeZ")?;
    let mut size_t = optional_dim(hdr.dim[3], "SizeT")?;

    // extraDims = dim[5], dim[6], dim[7] → hdr.dim[4], hdr.dim[5], hdr.dim[6].
    let extra_dims = [hdr.dim[4], hdr.dim[5], hdr.dim[6]];
    let mut size_c = 1u32;
    if hdr.ndim > 4 {
        for d in extra_dims.iter().take(hdr.ndim as usize - 4) {
            size_c = size_c
                .checked_mul(positive_dim(*d, "extra dimension")?)
                .ok_or_else(|| {
                    BioFormatsError::Format("NIfTI/Analyze channel count overflows".into())
                })?;
        }
    }

    if size_z == 0 {
        size_z = 1;
    }
    if size_t == 0 {
        size_t = 1;
    }

    // Java computes imageCount = sizeZ * sizeT * sizeC BEFORE populatePixelType
    // overrides sizeC for colour datatypes, so the colour override does not
    // change imageCount.
    let image_count = size_z
        .checked_mul(size_t)
        .and_then(|n| n.checked_mul(size_c))
        .ok_or_else(|| BioFormatsError::Format("NIfTI/Analyze image count overflows".into()))?;

    // Pixel type; colour datatypes (128/2304) also override sizeC.
    let (pixel_type, color_size_c) = nifti_pixel_type(hdr.datatype)?;
    if let Some(c) = color_size_c {
        size_c = c;
    }

    // Java: rgb = sizeC > 1 && imageCount == sizeZ*sizeT.
    let is_rgb = size_c > 1 && image_count == size_z * size_t;

    let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
    if !hdr.descrip.is_empty() {
        meta_map.insert(
            "description".into(),
            MetadataValue::String(hdr.descrip.clone()),
        );
    }
    meta_map.insert("datatype".into(), MetadataValue::Int(hdr.datatype as i64));
    let format_name = if is_nifti_magic(&hdr.magic) {
        "NIfTI-1"
    } else {
        "Analyze7.5"
    };
    meta_map.insert("format".into(), MetadataValue::String(format_name.into()));
    // Voxel spacings — NIfTI typically stores in mm; expose for OmeMetadata.
    if hdr.pixdim[0] > 0.0 {
        meta_map.insert(
            "voxel_size_x_mm".into(),
            MetadataValue::Float(hdr.pixdim[0] as f64),
        );
    }
    if hdr.pixdim[1] > 0.0 {
        meta_map.insert(
            "voxel_size_y_mm".into(),
            MetadataValue::Float(hdr.pixdim[1] as f64),
        );
    }
    if hdr.pixdim[2] > 0.0 {
        meta_map.insert(
            "voxel_size_z_mm".into(),
            MetadataValue::Float(hdr.pixdim[2] as f64),
        );
    }

    Ok(ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: hdr.bitpix.max(0) as u8,
        image_count,
        // Java NiftiReader uses dimensionOrder "XYCZT".
        dimension_order: DimensionOrder::XYCZT,
        is_rgb,
        is_interleaved: is_rgb,
        is_indexed: false,
        is_little_endian: hdr.little_endian,
        resolution_count: 1,
        series_metadata: meta_map,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    })
}

fn positive_dim(value: i16, label: &str) -> Result<u32> {
    if value <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "NIfTI/Analyze header has non-positive {label}"
        )));
    }
    Ok(value as u32)
}

fn optional_dim(value: i16, label: &str) -> Result<u32> {
    if value < 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "NIfTI/Analyze header has negative {label}"
        )));
    }
    Ok(value.max(1) as u32)
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
        let data_path = self
            .data_path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;

        let bps = meta.pixel_type.bytes_per_sample();
        let samples = if meta.is_rgb {
            meta.size_c.max(1) as usize
        } else {
            1
        };
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * samples * bps;
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
            || path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("hdr") || e.eq_ignore_ascii_case("img"))
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
            return magic == b"n+1\0"
                || magic == b"ni1\0"
                || magic == [0, 0, 0, 0]
                || magic == b"ni1 "; // some older files
        }
        le || be
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let path_str = path.to_string_lossy().to_ascii_lowercase();

        // The paired dataset has two files; we want the one ending in '.hdr'.
        // Java NiftiReader.initFile redirects a '.img' argument to its sibling
        // '.hdr' and re-inits (erroring if the header is missing).
        if path_str.ends_with(".img") {
            let header = path.with_extension("hdr");
            if header.exists() {
                return self.set_id(&header);
            }
            return Err(BioFormatsError::Format(
                "NIfTI/Analyze: header (.hdr) file not found for .img".into(),
            ));
        }

        let is_gz = path_str.ends_with(".nii.gz");

        // Read and parse header
        let mut hdr_bytes = vec![0u8; HDR_SIZE];
        if is_gz {
            let f = File::open(path).map_err(BioFormatsError::Io)?;
            let mut dec = flate2::read::GzDecoder::new(BufReader::new(f));
            dec.read_exact(&mut hdr_bytes)
                .map_err(BioFormatsError::Io)?;
        } else {
            let mut f = File::open(path).map_err(BioFormatsError::Io)?;
            f.read_exact(&mut hdr_bytes).map_err(BioFormatsError::Io)?;
        }

        let hdr = parse_header(&hdr_bytes)?;
        let meta = build_metadata(&hdr)?;

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
        self.load_raw(plane_index)
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
        let samples = if meta.is_rgb {
            meta.size_c.max(1) as usize
        } else {
            1
        };
        crop_full_plane("NIfTI", &full, meta, samples, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::metadata::MetadataValue;
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let img = &mut ome.images[0];
        let get_f = |k: &str| -> Option<f64> {
            if let Some(MetadataValue::Float(v)) = meta.series_metadata.get(k) {
                Some(*v)
            } else {
                None
            }
        };
        // Java stores pixdim directly as an OME Length in the file's spatial unit
        // (FormatTools.getPhysicalSizeX(value, spatialUnit)); the OME value keeps
        // the raw pixdim number, so we must NOT rescale it.
        img.physical_size_x = get_f("voxel_size_x_mm");
        img.physical_size_y = get_f("voxel_size_y_mm");
        img.physical_size_z = get_f("voxel_size_z_mm");
        if let Some(MetadataValue::String(d)) = meta.series_metadata.get("description") {
            img.description = Some(d.clone());
        }
        // Java leaves the default image name (the file name).
        if let Some(p) = self.hdr_path.as_ref() {
            img.name = p
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
        }
        Some(ome)
    }
}
