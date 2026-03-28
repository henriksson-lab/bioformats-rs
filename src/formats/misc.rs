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

// ---------------------------------------------------------------------------
// Macro for extension-only placeholder readers
// ---------------------------------------------------------------------------
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
placeholder_reader! {
    /// Apple QuickTime movie placeholder reader (`.mov`, `.qt`).
    pub struct QuickTimeReader;
    extensions: ["mov", "qt"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 2. Multiple-image Network Graphics
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// MNG (Multiple-image Network Graphics) placeholder reader (`.mng`).
    pub struct MngReader;
    extensions: ["mng"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 3. Volocity Library
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// Volocity Library placeholder reader (`.acff`).
    pub struct VolocityLibraryReader;
    extensions: ["acff"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 4. 3i SlideBook
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// 3i SlideBook placeholder reader (`.sld`).
    pub struct SlideBookReader;
    extensions: ["sld"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 5. MINC neuroimaging (NetCDF-based)
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// MINC neuroimaging placeholder reader (`.mnc`).
    pub struct MincReader;
    extensions: ["mnc"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 6. PerkinElmer Openlab LIFF
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// PerkinElmer Openlab LIFF placeholder reader (`.liff`).
    pub struct OpenlabLiffReader;
    extensions: ["liff"];
    magic_bytes: false;
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
        Jpeg2000Reader { path: None, meta: None, pixel_data: None }
    }
}

impl Default for Jpeg2000Reader {
    fn default() -> Self { Self::new() }
}

impl FormatReader for Jpeg2000Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("jp2") | Some("j2k") | Some("j2c") | Some("jpc"))
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
        });
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixel_data = None;
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
        if plane_index != 0 {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        self.pixel_data.clone().ok_or(BioFormatsError::NotInitialized)
    }

    fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
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
placeholder_reader! {
    /// Sedat Lab format placeholder reader (`.sedat`).
    pub struct SedatReader;
    extensions: ["sedat"];
    magic_bytes: false;
}

// ---------------------------------------------------------------------------
// 9. SM-Camera
// ---------------------------------------------------------------------------
placeholder_reader! {
    /// SM-Camera placeholder reader (`.smc`).
    pub struct SmCameraReader;
    extensions: ["smc"];
    magic_bytes: false;
}
