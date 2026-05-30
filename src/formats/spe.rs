//! Princeton Instruments SPE format reader.
//!
//! The SPE file has a 4100-byte binary header followed by raw pixel data.
//! Key header fields (offsets from SPEReader.java SpeHeaderEntry, all
//! little-endian): DATATYPE at 108 (short), WIDTH at 42 (short),
//! HEIGHT at 656 (short), NUM_FRAMES at 1446 (int), XML_OFFSET at 678 (long),
//! HEADER_VER at 1992 (int).
//!
//! SPE 3.0 introduced a trailing XML footer at `XML_OFFSET`. Matching the Java
//! reference (SPEReader.initFile), the pixel dimensions are still taken from the
//! binary header for both 2.x and 3.x; the v3 XML footer is detected (via
//! HEADER_VER >= 3 or XML_OFFSET > 0) and exposed in metadata, but Java marks
//! such files as `metadataComplete = false` rather than parsing the XML, so we
//! do the same and additionally surface the raw footer XML string.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

const HEADER_SIZE: u64 = 4100;

fn r_i16_le(b: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([b[off], b[off + 1]])
}
fn r_u16_le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn r_i32_le(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn r_i64_le(b: &[u8], off: usize) -> i64 {
    i64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

/// SPE datatype codes
fn spe_pixel_type(datatype: i16) -> (PixelType, u8) {
    // Per SPEReader.java: FLOAT=0, INT32=1, INT16=2, UNINT16=3, UNINT32=4.
    match datatype {
        0 => (PixelType::Float32, 32),
        1 => (PixelType::Int32, 32),
        2 => (PixelType::Int16, 16),
        3 => (PixelType::Uint16, 16),
        4 => (PixelType::Uint32, 32),
        _ => (PixelType::Uint16, 16),
    }
}

/// Replicate Java SPEReader.SpeHeader.getStackSize (904-919): used as a
/// fallback to derive the frame count when NUM_FRAMES < 1.
///
/// Offsets (all little-endian, matching SpeHeaderEntry):
///   HEIGHT     = 656 (short, Y dim of raw data / "stripe")
///   NOSCAN     =  34 (short, old num scans; usually -1, i.e. 65535 unsigned)
///   LNOSCAN    = 664 (int, number of scans for early WinX)
///   NUM_FRAMES =1446 (int)
///
/// Note: Java's getShort reads an UNSIGNED 16-bit value (no sign extension),
/// so the `noscan == 65535` check is performed against the unsigned reading
/// (r_u16_le), matching Java exactly.
fn spe_stack_size(hdr: &[u8]) -> i32 {
    let stripe = r_u16_le(hdr, 656) as i32; // HEIGHT
    let noscan = r_u16_le(hdr, 34) as i32; // NOSCAN
    let num_frames = r_i32_le(hdr, 1446); // NUM_FRAMES
    if stripe == 0 || noscan == 0 {
        return num_frames;
    }
    if noscan == 65535 {
        let lnoscan = r_i32_le(hdr, 664); // LNOSCAN
        if lnoscan == -1 || lnoscan == 0 {
            num_frames
        } else {
            lnoscan / stripe
        }
    } else {
        noscan / stripe
    }
}

/// Read the SPE 3.0 trailing XML footer starting at `offset` to EOF.
fn read_xml_footer(f: &mut File, offset: u64) -> Result<String> {
    let len = f.metadata().map_err(BioFormatsError::Io)?.len();
    if offset >= len {
        return Ok(String::new());
    }
    f.seek(SeekFrom::Start(offset))
        .map_err(BioFormatsError::Io)?;
    let mut buf = Vec::with_capacity((len - offset) as usize);
    f.read_to_end(&mut buf).map_err(BioFormatsError::Io)?;
    Ok(String::from_utf8_lossy(&buf)
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string())
}

pub struct SpeReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl SpeReader {
    pub fn new() -> Self {
        SpeReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for SpeReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SpeReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("spe"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        // No universal magic byte; rely on extension
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; HEADER_SIZE as usize];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        // Offsets from SPEReader.java SpeHeaderEntry (all little-endian):
        //  DATATYPE   = 108 (short)
        //  WIDTH      =  42 (short)
        //  HEIGHT     = 656 (short)
        //  NUM_FRAMES =1446 (int)
        //  EXPOSURE   =  10 (int)
        //  DATE       =  20 (10 bytes, byte string)
        //  XML_OFFSET = 678 (long)
        //  HEADER_VER =1992 (int)
        let datatype = r_i16_le(&hdr, 108);
        let xdim = positive_u16_dim(r_u16_le(&hdr, 42), "width")?;
        let ydim = positive_u16_dim(r_u16_le(&hdr, 656), "height")?;
        // NUM_FRAMES (offset 1446, int). When < 1, Java SPEReader.java:152-155
        // falls back to header.getStackSize() before erroring.
        let raw_numframes = r_i32_le(&hdr, 1446);
        let numframes = if raw_numframes < 1 {
            let stack_size = spe_stack_size(&hdr);
            if stack_size >= 1 {
                stack_size as u32
            } else {
                // Still non-positive after the fallback: reject as Java would
                // produce an invalid (<1) frame count.
                positive_i32_dim(raw_numframes, "frame count")?
            }
        } else {
            positive_i32_dim(raw_numframes, "frame count")?
        };
        let exposure = r_i32_le(&hdr, 10);
        let header_ver = r_i32_le(&hdr, 1992);
        let xml_offset = r_i64_le(&hdr, 678);

        // Date string (best-effort)
        let date = std::str::from_utf8(&hdr[20..30])
            .unwrap_or("")
            .trim_end_matches('\0')
            .trim()
            .to_string();

        // Java throws "Invalid pixel type" for unknown datatypes (FLOAT=0,
        // INT32=1, INT16=2, UNINT16=3, UNINT32=4).
        if !matches!(datatype, 0..=4) {
            return Err(BioFormatsError::Format(format!(
                "SPE: invalid pixel type {datatype}"
            )));
        }
        let (pixel_type, bpp) = spe_pixel_type(datatype);
        validate_spe_payload(
            f.metadata().map_err(BioFormatsError::Io)?.len(),
            xdim,
            ydim,
            numframes,
            pixel_type,
        )?;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        if exposure > 0 {
            meta_map.insert("EXPOSURE".into(), MetadataValue::Int(exposure as i64));
        }
        if !date.is_empty() {
            meta_map.insert("date".into(), MetadataValue::String(date));
        }
        meta_map.insert("HEADER_VER".into(), MetadataValue::Int(header_ver as i64));

        // SPE 3.0 XML footer: detected when HEADER_VER >= 3 or XML_OFFSET > 0.
        // Matching SPEReader.java, the binary-header dimensions are authoritative
        // and the file is flagged metadata-incomplete; we additionally expose the
        // raw footer XML text so downstream callers can inspect it.
        if header_ver >= 3 || xml_offset > 0 {
            meta_map.insert("XML_OFFSET".into(), MetadataValue::Int(xml_offset));
            meta_map.insert("metadataComplete".into(), MetadataValue::Bool(false));
            if xml_offset > 0 {
                if let Ok(xml) = read_xml_footer(&mut f, xml_offset as u64) {
                    if !xml.is_empty() {
                        meta_map.insert("XMLFooter".into(), MetadataValue::String(xml));
                    }
                }
            }
        } else {
            meta_map.insert("metadataComplete".into(), MetadataValue::Bool(true));
        }

        self.meta = Some(ImageMetadata {
            size_x: xdim,
            size_y: ydim,
            // Java: sizeZ=1, sizeC=1, sizeT=numFrames, order "XYZTC".
            size_z: 1,
            size_c: 1,
            size_t: numframes,
            pixel_type,
            bits_per_pixel: bpp,
            image_count: numframes,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let offset = HEADER_SIZE + plane_index as u64 * plane_bytes as u64;
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("SPE", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::OmeMetadata;
        let meta = self.meta.as_ref()?;
        // SPEReader.java populates pixels only; exposure time is stored as a
        // global metadata int (microseconds, per the SPE spec) and is not mapped
        // to per-plane OME PlaneDeltaT, so we mirror the pixel-only mapping.
        Some(OmeMetadata::from_image_metadata(meta))
    }
}

fn positive_u16_dim(value: u16, label: &str) -> Result<u32> {
    if value == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "SPE header has non-positive {label}"
        )));
    }
    Ok(value as u32)
}

fn positive_i32_dim(value: i32, label: &str) -> Result<u32> {
    if value <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "SPE header has non-positive {label}"
        )));
    }
    Ok(value as u32)
}

fn validate_spe_payload(
    file_len: u64,
    size_x: u32,
    size_y: u32,
    frames: u32,
    pixel_type: PixelType,
) -> Result<()> {
    let plane_bytes = (size_x as u64)
        .checked_mul(size_y as u64)
        .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample() as u64))
        .ok_or_else(|| BioFormatsError::Format("SPE plane size overflows".into()))?;
    let required_len = HEADER_SIZE
        .checked_add(
            plane_bytes
                .checked_mul(frames as u64)
                .ok_or_else(|| BioFormatsError::Format("SPE payload size overflows".into()))?,
        )
        .ok_or_else(|| BioFormatsError::Format("SPE payload size overflows".into()))?;
    if file_len < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "SPE pixel payload is shorter than declared ({file_len} < {required_len})"
        )));
    }
    Ok(())
}
