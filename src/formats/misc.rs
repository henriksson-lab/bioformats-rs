//! Placeholder readers for miscellaneous / proprietary formats.
//!
//! These readers are extension-only (or magic-byte only for JPEG 2000) and
//! return 512×512 uint8 placeholder metadata with zeroed pixel data.
//! Full decoding is not implemented.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

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
        }
    };
}

// ---------------------------------------------------------------------------
// 1. Apple QuickTime
// ---------------------------------------------------------------------------
/// Apple QuickTime movie reader (`.mov`, `.qt`).
///
/// QuickTime/MOV container parsing is complex (nested atom structure with
/// multiple codec variants). Returns `UnsupportedFormat` with a descriptive
/// message instead of a generic "not yet implemented".
pub struct QuickTimeReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl QuickTimeReader {
    pub fn new() -> Self {
        QuickTimeReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for QuickTimeReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for QuickTimeReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("mov") | Some("qt"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "QuickTime container parsing not yet implemented (MOV/QT files require complex atom-based container parsing with multiple codec variants)".to_string()
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

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "QuickTime container parsing not yet implemented".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "QuickTime container parsing not yet implemented".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "QuickTime container parsing not yet implemented".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// 2. Multiple-image Network Graphics — delegates to PNG
// ---------------------------------------------------------------------------
/// MNG (Multiple-image Network Graphics) reader (`.mng`).
///
/// MNG is PNG-based; delegates to `PngReader` for the first frame.
pub struct MngReader {
    inner: crate::formats::png::PngReader,
}

impl MngReader {
    pub fn new() -> Self {
        MngReader {
            inner: crate::formats::png::PngReader::new(),
        }
    }
}

impl Default for MngReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MngReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("mng"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path).map_err(|_| {
            BioFormatsError::UnsupportedFormat(
                "MNG file could not be opened as PNG (MNG animation may require dedicated parser)"
                    .to_string(),
            )
        })
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
// 3. Volocity Library
// ---------------------------------------------------------------------------
/// Volocity Library reader (`.acff`).
///
/// Volocity Library files use OLE2/Compound Document format which requires
/// a dedicated OLE2 container parser not currently available in pure Rust.
pub struct VolocityLibraryReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl VolocityLibraryReader {
    pub fn new() -> Self {
        VolocityLibraryReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for VolocityLibraryReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for VolocityLibraryReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("acff"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity Library format requires OLE2/Compound Document container parsing".to_string(),
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

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity Library format requires OLE2/Compound Document container parsing".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity Library format requires OLE2/Compound Document container parsing".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Volocity Library format requires OLE2/Compound Document container parsing".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// 4. 3i SlideBook
// ---------------------------------------------------------------------------
/// 3i SlideBook reader (`.sld`).
///
/// SlideBook uses a proprietary binary format from 3i (Intelligent Imaging
/// Innovations). The internal structure is undocumented.
pub struct SlideBookReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl SlideBookReader {
    pub fn new() -> Self {
        SlideBookReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for SlideBookReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SlideBookReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sld"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "3i SlideBook format is proprietary with undocumented binary structure".to_string(),
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

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "3i SlideBook format is proprietary with undocumented binary structure".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "3i SlideBook format is proprietary with undocumented binary structure".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "3i SlideBook format is proprietary with undocumented binary structure".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// 5. MINC neuroimaging (HDF5-based)
// ---------------------------------------------------------------------------
/// MINC neuroimaging reader (`.mnc`).
///
/// MINC files are HDF5-based. Attempts to open the file via HDF5 and locate
/// image data in common MINC dataset paths.
pub struct MincReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Option<Vec<u8>>,
}

impl MincReader {
    pub fn new() -> Self {
        MincReader {
            path: None,
            meta: None,
            pixel_data: None,
        }
    }
}

impl Default for MincReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for MincReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("mnc"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // HDF5 magic: 0x89 H D F \r \n 0x1a \n
        header.len() >= 8 && header[..8] == [0x89, 0x48, 0x44, 0x46, 0x0D, 0x0A, 0x1A, 0x0A]
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        use hdf5_pure::DType;

        let file = hdf5_pure::File::open(path)
            .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5: {e}")))?;

        // Mirror MINCReader.initFile: only the HDF5-backed MINC-2.0 path is
        // reachable here (classic-NetCDF MINC-1 is not HDF5 and is rejected by
        // is_this_type_by_bytes). Java tries "/image" first, then
        // "/minc-2.0/image/0/image" and sets isMINC2 in the latter case.
        let minc2_path = "/minc-2.0/image/0/image";
        let (ds, is_minc2) = if let Ok(ds) = file.dataset("/image") {
            (ds, false)
        } else if let Ok(ds) = file.dataset(minc2_path) {
            (ds, true)
        } else if let Ok(ds) = file.dataset("/minc-2.0/image/image") {
            (ds, true)
        } else {
            return Err(BioFormatsError::UnsupportedFormat(
                "MINC/HDF5: could not find image dataset in known paths".to_string(),
            ));
        };

        let shape = ds.shape().unwrap_or_default();
        // MINC stores dimensions slowest-to-fastest; the image axes are the
        // trailing two (..., y, x), Z is the next, and any leading axis is T.
        // Java reads sizeX/sizeY/sizeZ from the xspace/yspace/zspace dimension
        // variables, but the dataset shape encodes the same values.
        let (size_x, size_y, size_z, size_t) = match shape.len() {
            0 => (1u32, 1u32, 1u32, 1u32),
            1 => (shape[0] as u32, 1u32, 1u32, 1u32),
            2 => (shape[1] as u32, shape[0] as u32, 1u32, 1u32),
            3 => (shape[2] as u32, shape[1] as u32, shape[0] as u32, 1u32),
            n => (
                shape[n - 1] as u32,
                shape[n - 2] as u32,
                shape[n - 3] as u32,
                // Collapse all remaining leading axes into T (Java flattens
                // byte[t][z][...] into a single plane list).
                shape[..n - 3]
                    .iter()
                    .fold(1u64, |a, &d| a.saturating_mul(d.max(1))) as u32,
            ),
        };

        // Determine the real datatype. The HDF5 dataset datatype already
        // reports the storage size and intrinsic signedness; for MINC-2 the
        // "_Unsigned" attribute can override it (HDF5 commonly stores unsigned
        // image data as a signed fixed-point type plus _Unsigned="true"),
        // mirroring the signed/_Unsigned handling in MINCReader.initFile.
        let dtype = ds
            .dtype()
            .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 dtype: {e}")))?;

        let unsigned_attr: Option<bool> = if is_minc2 {
            ds.attrs().ok().and_then(|attrs| {
                attrs.get("_Unsigned").map(|v| {
                    // Java: signed unless the attribute starts with "true".
                    format!("{v:?}").to_ascii_lowercase().contains("true")
                })
            })
        } else {
            None
        };

        // Read the raw values via the matching typed reader and re-emit them as
        // little-endian bytes (MINCReader uses isLittleEndian()==isMINC2 for the
        // byte conversion; we always materialise little-endian and flag the
        // metadata accordingly).
        let (pixel_type, bits, pixels): (PixelType, u8, Vec<u8>) = match &dtype {
            DType::U8 | DType::I8 => {
                let signed = matches!(dtype, DType::I8) && unsigned_attr != Some(false);
                let raw = ds
                    .read_u8()
                    .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                let pt = if signed { PixelType::Int8 } else { PixelType::Uint8 };
                (pt, 8, raw)
            }
            DType::U16 | DType::I16 => {
                let signed = matches!(dtype, DType::I16) && unsigned_attr != Some(false);
                let raw = ds
                    .read_i16()
                    .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                let mut bytes = Vec::with_capacity(raw.len() * 2);
                for v in &raw {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                let pt = if signed { PixelType::Int16 } else { PixelType::Uint16 };
                (pt, 16, bytes)
            }
            DType::U32 | DType::I32 => {
                let signed = matches!(dtype, DType::I32) && unsigned_attr != Some(false);
                let raw = ds
                    .read_i32()
                    .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                let mut bytes = Vec::with_capacity(raw.len() * 4);
                for v in &raw {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                let pt = if signed { PixelType::Int32 } else { PixelType::Uint32 };
                (pt, 32, bytes)
            }
            DType::F32 => {
                let raw = ds
                    .read_f32()
                    .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                let mut bytes = Vec::with_capacity(raw.len() * 4);
                for v in &raw {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                (PixelType::Float32, 32, bytes)
            }
            DType::F64 => {
                let raw = ds
                    .read_f64()
                    .map_err(|e| BioFormatsError::Format(format!("MINC/HDF5 read: {e}")))?;
                let mut bytes = Vec::with_capacity(raw.len() * 8);
                for v in &raw {
                    bytes.extend_from_slice(&v.to_le_bytes());
                }
                (PixelType::Float64, 64, bytes)
            }
            other => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "MINC/HDF5: unsupported image datatype {other}"
                )));
            }
        };

        // Java: imageCount = sizeZ * sizeT * sizeC (sizeC == 1).
        let image_count = size_z.max(1) * size_t.max(1);
        self.path = Some(path.to_path_buf());
        self.pixel_data = Some(pixels);
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c: 1,
            size_t,
            pixel_type,
            bits_per_pixel: bits,
            image_count,
            // Java MINCReader: dimensionOrder = "XYZCT".
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            // Java sets littleEndian = isMINC2.
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
        self.path = None;
        self.meta = None;
        self.pixel_data = None;
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
        let pixels = self
            .pixel_data
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * bps;
        let offset = plane_index as usize * plane_bytes;
        let end = offset + plane_bytes;
        if end > pixels.len() {
            return Err(BioFormatsError::Format(format!(
                "MINC/HDF5: dataset is too short for plane {plane_index}"
            )));
        }
        Ok(pixels[offset..end].to_vec())
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
        let bps = meta.pixel_type.bytes_per_sample();
        let row_bytes = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let start = (y as usize + row) * row_bytes + x as usize * bps;
            out.extend_from_slice(&full[start..start + out_row]);
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
// 6. PerkinElmer Openlab LIFF
// ---------------------------------------------------------------------------
/// PerkinElmer Openlab LIFF reader (`.liff`).
///
/// Openlab LIFF is a proprietary binary format from PerkinElmer/Improvision.
/// The internal structure is undocumented and not publicly specified.
pub struct OpenlabLiffReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl OpenlabLiffReader {
    pub fn new() -> Self {
        OpenlabLiffReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for OpenlabLiffReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for OpenlabLiffReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("liff"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "PerkinElmer Openlab LIFF is a proprietary format with undocumented binary structure"
                .to_string(),
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

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "PerkinElmer Openlab LIFF is a proprietary format with undocumented binary structure"
                .to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "PerkinElmer Openlab LIFF is a proprietary format with undocumented binary structure"
                .to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "PerkinElmer Openlab LIFF is a proprietary format with undocumented binary structure"
                .to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// 7. JPEG 2000 — magic-byte detection + extension + full decoding
// ---------------------------------------------------------------------------
/// JPEG 2000 reader (`.jp2`, `.j2k`).
///
/// Detects via magic bytes:
/// - `FF 4F FF 51` — JPEG 2000 codestream (J2C)
/// - `00 00 00 0C 6A 50 20 20` — JP2 container
///
/// Decodes pixel data using the `jpeg2k` crate (pure-Rust OpenJPEG port).
pub struct Jpeg2000Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Option<Vec<u8>>,
}

impl Jpeg2000Reader {
    pub fn new() -> Self {
        Jpeg2000Reader {
            path: None,
            meta: None,
            pixel_data: None,
        }
    }
}

impl Default for Jpeg2000Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for Jpeg2000Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(
            ext.as_deref(),
            Some("jp2") | Some("j2k") | Some("j2c") | Some("jpc")
        )
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // J2C codestream: FF 4F FF 51
        if header.len() >= 4 && header[..4] == [0xFF, 0x4F, 0xFF, 0x51] {
            return true;
        }
        // JP2 container: 00 00 00 0C 6A 50 20 20
        if header.len() >= 8 && header[..8] == [0x00, 0x00, 0x00, 0x0C, 0x6A, 0x50, 0x20, 0x20] {
            return true;
        }
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let file_data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let image = jpeg2k::Image::from_bytes(&file_data)
            .map_err(|e| BioFormatsError::Codec(format!("JPEG 2000: {e}")))?;

        let components = image.components();
        if components.is_empty() {
            return Err(BioFormatsError::Codec("JPEG 2000: no components".into()));
        }

        let width = components[0].width() as u32;
        let height = components[0].height() as u32;
        let n_components = components.len() as u32;
        let prec = components[0].precision() as u8;
        let (pixel_type, bpp) = if prec <= 8 {
            (PixelType::Uint8, 8u8)
        } else if prec <= 16 {
            (PixelType::Uint16, 16u8)
        } else {
            (PixelType::Uint32, 32u8)
        };
        let bps = (bpp / 8) as usize;
        let is_rgb = n_components >= 3;

        // Decode pixel data: interleave components
        let w = width as usize;
        let h = height as usize;
        let nc = n_components as usize;
        let mut pixels = Vec::with_capacity(w * h * nc * bps);
        for y in 0..h {
            for x in 0..w {
                for c in 0..nc {
                    let val = components[c].data()[y * w + x];
                    match bps {
                        1 => pixels.push(val as u8),
                        2 => pixels.extend_from_slice(&(val as u16).to_le_bytes()),
                        _ => pixels.extend_from_slice(&val.to_le_bytes()),
                    }
                }
            }
        }

        self.path = Some(path.to_path_buf());
        self.pixel_data = Some(pixels);
        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: n_components,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb,
            is_interleaved: true,
            is_indexed: false,
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
        self.path = None;
        self.meta = None;
        self.pixel_data = None;
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.pixel_data
            .clone()
            .ok_or(BioFormatsError::NotInitialized)
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
        let bps = (meta.bits_per_pixel / 8) as usize;
        let nc = meta.size_c as usize;
        let pixel_bytes = bps * nc;
        let row_bytes = meta.size_x as usize * pixel_bytes;
        let out_row = w as usize * pixel_bytes;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for r in 0..h as usize {
            let src_offset = (y as usize + r) * row_bytes + x as usize * pixel_bytes;
            out.extend_from_slice(&full[src_offset..src_offset + out_row]);
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
// 8. Sedat Lab format
// ---------------------------------------------------------------------------
/// Sedat Lab format reader (`.sedat`).
///
/// The Sedat format is a proprietary format from the Sedat Lab at UCSF.
/// The binary structure is not publicly documented.
pub struct SedatReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl SedatReader {
    pub fn new() -> Self {
        SedatReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for SedatReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SedatReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("sedat"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, _path: &Path) -> Result<()> {
        Err(BioFormatsError::UnsupportedFormat(
            "Sedat Lab format is proprietary with undocumented binary structure".to_string(),
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

    fn open_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Sedat Lab format is proprietary with undocumented binary structure".to_string(),
        ))
    }

    fn open_bytes_region(
        &mut self,
        _plane_index: u32,
        _x: u32,
        _y: u32,
        _w: u32,
        _h: u32,
    ) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Sedat Lab format is proprietary with undocumented binary structure".to_string(),
        ))
    }

    fn open_thumb_bytes(&mut self, _plane_index: u32) -> Result<Vec<u8>> {
        Err(BioFormatsError::UnsupportedFormat(
            "Sedat Lab format is proprietary with undocumented binary structure".to_string(),
        ))
    }
}

// ---------------------------------------------------------------------------
// 9. SM-Camera
// ---------------------------------------------------------------------------
/// SM-Camera reader.
///
/// Java Bio-Formats identifies this format by a fixed 16-byte magic and stores
/// one UINT8 plane after a 548-byte header.
pub struct SmCameraReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

const SMC_MAGIC: [u8; 16] = [0, 0, 0, 0, 2, 0, 0, 5, 0xc9, 0x88, 0, 5, 0xcb, 0x88, 0, 0];
const SMC_HEADER_SIZE: usize = 548;

impl SmCameraReader {
    pub fn new() -> Self {
        SmCameraReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for SmCameraReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SmCameraReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("smc"))
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= SMC_MAGIC.len() && header[..SMC_MAGIC.len()] == SMC_MAGIC
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if !self.is_this_type_by_bytes(&data) {
            return Err(BioFormatsError::UnsupportedFormat(
                "SM-Camera file is missing the expected SMC magic".to_string(),
            ));
        }
        if data.len() < SMC_HEADER_SIZE {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "SM-Camera header is shorter than {SMC_HEADER_SIZE} bytes"
            )));
        }

        let size_y = u16::from_be_bytes([data[524], data[525]]) as u32;
        let size_x = u16::from_be_bytes([data[532], data[533]]) as u32;
        if size_x == 0 || size_y == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "SM-Camera header has invalid image dimensions".to_string(),
            ));
        }

        let plane_bytes = (size_x as usize)
            .checked_mul(size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("SM-Camera plane size overflows".to_string()))?;
        let required = SMC_HEADER_SIZE
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("SM-Camera file size overflows".to_string()))?;
        if data.len() < required {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "SM-Camera payload is shorter than declared {size_x}x{size_y} plane"
            )));
        }

        self.path = Some(path.to_path_buf());
        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: false,
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
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        let plane_bytes = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("SM-Camera plane size overflows".to_string()))?;
        let end = SMC_HEADER_SIZE
            .checked_add(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("SM-Camera plane end overflows".to_string()))?;
        if data.len() < end {
            return Err(BioFormatsError::InvalidData(format!(
                "SM-Camera payload is too short: got {}, expected at least {end}",
                data.len()
            )));
        }
        Ok(data[SMC_HEADER_SIZE..end].to_vec())
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
        crop_full_plane("SM-Camera", &full, meta, 1, x, y, w, h)
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
// 10. Plain text image — CSV/TSV parsing like TextImageReader
// ---------------------------------------------------------------------------
/// Plain text image reader (`.txt`).
///
/// Parses tab/comma/space-separated numeric values from a text file,
/// treating each row as a line of pixels and each value as a Float32 sample.
pub struct TextReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Vec<u8>,
}

impl TextReader {
    pub fn new() -> Self {
        TextReader {
            path: None,
            meta: None,
            pixel_data: Vec::new(),
        }
    }
}

impl Default for TextReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for TextReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("txt"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let mut rows: Vec<Vec<f32>> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            let cells: Vec<f32> = line
                .split(|c: char| c == ',' || c == '\t' || c == ' ')
                .filter(|s| !s.is_empty())
                .map(|s| s.trim().parse::<f64>().unwrap_or(0.0) as f32)
                .collect();
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        if rows.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(
                "TextReader: file contains no numeric data".to_string(),
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
        self.pixel_data.clear();
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
        Ok(self.pixel_data.clone())
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
        crop_full_plane("Text", &full, meta, 1, x, y, w, h)
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
