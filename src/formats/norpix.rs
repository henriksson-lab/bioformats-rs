//! Norpix StreamPix SEQ and IPLab format readers.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

fn r_u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn r_i32_le(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn positive_i32_dim(value: i32, label: &str) -> Result<u32> {
    if value <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "IPLab {label} is non-positive ({value})"
        )));
    }
    Ok(value as u32)
}

fn positive_u32_seq_dim(value: u32, label: &str) -> Result<u32> {
    if value == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Norpix SEQ {label} is non-positive"
        )));
    }
    Ok(value)
}

/// Read a StreamPix per-frame timestamp at `off`: u32 seconds since the Unix
/// epoch followed by u16 milliseconds and u16 microseconds. Returns seconds as
/// f64, or 0.0 if the timestamp lies past EOF.
fn read_seq_timestamp(f: &mut File, off: u64, file_len: u64) -> f64 {
    if off + 8 > file_len {
        return 0.0;
    }
    if f.seek(SeekFrom::Start(off)).is_err() {
        return 0.0;
    }
    let mut buf = [0u8; 8];
    if f.read_exact(&mut buf).is_err() {
        return 0.0;
    }
    let secs = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as f64;
    let millis = u16::from_le_bytes([buf[4], buf[5]]) as f64;
    let micros = u16::from_le_bytes([buf[6], buf[7]]) as f64;
    secs + millis / 1_000.0 + micros / 1_000_000.0
}

fn printable_ascii(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).trim().to_string()
}

// ─── Norpix StreamPix SEQ ─────────────────────────────────────────────────────
//
// StreamPix .seq files have a 1024-byte header with the following layout:
//   Offset   0: Description (24 bytes), often "Norpix seq\0..."
//   Offset  24: Version (i64)
//   Offset  32: Header size (i32)
//   Offset 548: Allocated frames (u32)
//   Offset 572: True image size (u32) = width * height * bytes_per_pixel
//   Offset 592: Description format (u32): 0=mono8, 1=mono16, 2=color24, 100=jpg
//   Offset 596: Width (u32)
//   Offset 600: Height (u32)
//   Offset 604: Bit depth (u32) — bits per pixel (8 or 16)
//   Offset 612: Compression (u32): 0=uncompressed
//
// Pixel data starts at offset 1024.
// Each frame may be preceded by a 4-byte offset table if indexed,
// but for uncompressed data frames are tightly packed.

pub struct NorpixReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    frame_size: usize,
    /// True when frames are JPEG-compressed (description format 100/102).
    compressed: bool,
    /// Per-frame absolute byte offsets of the image payload (excludes the
    /// trailing 8/10-byte timestamp). Empty for the uncompressed fast path.
    frame_offsets: Vec<u64>,
    /// Per-frame compressed payload lengths. Empty for uncompressed data.
    frame_lengths: Vec<usize>,
    /// Per-frame timestamps in seconds since the Unix epoch.
    timestamps: Vec<f64>,
}

impl NorpixReader {
    pub fn new() -> Self {
        NorpixReader {
            path: None,
            meta: None,
            frame_size: 0,
            compressed: false,
            frame_offsets: Vec::new(),
            frame_lengths: Vec::new(),
            timestamps: Vec::new(),
        }
    }
}
impl Default for NorpixReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for NorpixReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("seq"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        if header.len() < 24 {
            return false;
        }
        // Check description starts with "Norpix seq"
        let desc = std::str::from_utf8(&header[..24]).unwrap_or("");
        desc.starts_with("Norpix seq") || desc.starts_with("Norpix SEQ")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; 1024];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        let n_frames = positive_u32_seq_dim(r_u32_le(&hdr, 548), "frame count")?;
        let true_image_size = r_u32_le(&hdr, 572);
        let desc_fmt = r_u32_le(&hdr, 592);
        let width = positive_u32_seq_dim(r_u32_le(&hdr, 596), "width")?;
        let height = positive_u32_seq_dim(r_u32_le(&hdr, 600), "height")?;
        let header_size = r_i32_le(&hdr, 32);
        let compression = r_u32_le(&hdr, 612);
        // StreamPix description-format codes: 0=mono8, 1=mono16, 2=BGR24,
        // 100=JPEG mono8, 101=mono16 (uncompressed), 102=JPEG BGR24.
        let compressed = matches!(desc_fmt, 100 | 102);
        if !compressed && compression != 0 {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Norpix SEQ unsupported compression code {compression} for description format {desc_fmt}"
            )));
        }

        let (pixel_type, bpp, channels): (PixelType, u8, u32) = match desc_fmt {
            0 | 100 => (PixelType::Uint8, 8, 1), // mono 8-bit (raw / JPEG)
            1 => (PixelType::Uint16, 16, 1),     // mono 16-bit
            2 | 102 => (PixelType::Uint8, 8, 3), // color BGR24 (raw / JPEG)
            101 => (PixelType::Uint16, 16, 1),   // mono 16-bit alt
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Norpix SEQ unsupported description format {desc_fmt}"
                )))
            }
        };

        let bps = pixel_type.bytes_per_sample();
        // Uncompressed (raw) plane payload in bytes.
        let plane_bytes = (width as usize)
            .checked_mul(height as usize)
            .and_then(|v| v.checked_mul(bps))
            .and_then(|v| v.checked_mul(channels as usize))
            .ok_or_else(|| BioFormatsError::Format("Norpix SEQ plane size overflows".into()))?;
        // trueImageSize is the padded per-frame stride (image payload + trailing
        // timestamp + alignment) for uncompressed data.
        let frame_size = if !compressed && true_image_size as usize >= plane_bytes {
            true_image_size as usize
        } else {
            plane_bytes
        };
        let is_rgb = channels == 3;

        // Build the per-frame offset table and read timestamps. For uncompressed
        // data frames are at fixed stride; for JPEG data each frame is stored as
        // a 4-byte little-endian size followed by the JPEG codestream.
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        let mut frame_offsets = Vec::with_capacity(n_frames as usize);
        let mut frame_lengths = Vec::with_capacity(n_frames as usize);
        let mut timestamps = Vec::with_capacity(n_frames as usize);
        if compressed {
            let mut pos = 1024u64;
            for i in 0..n_frames {
                if pos + 4 > file_len {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Norpix SEQ compressed frame {i} is missing its size prefix"
                    )));
                }
                f.seek(SeekFrom::Start(pos)).map_err(BioFormatsError::Io)?;
                let mut size_buf = [0u8; 4];
                f.read_exact(&mut size_buf).map_err(BioFormatsError::Io)?;
                let jpeg_size = u32::from_le_bytes(size_buf) as u64;
                if jpeg_size == 0 {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Norpix SEQ compressed frame {i} has zero JPEG payload length"
                    )));
                }
                let img_off = pos + 4;
                if img_off + jpeg_size > file_len {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Norpix SEQ compressed frame {i} payload is shorter than declared: need {} bytes, found {file_len}",
                        img_off + jpeg_size
                    )));
                }
                frame_offsets.push(img_off);
                frame_lengths.push(jpeg_size as usize);
                // Timestamp follows the JPEG payload.
                let ts = read_seq_timestamp(&mut f, img_off + jpeg_size, file_len);
                timestamps.push(ts);
                pos = img_off + jpeg_size;
                // Some writers pad/align; advance past the 8-byte timestamp too.
                pos += 8;
            }
        } else {
            let required_len = 1024u64
                .checked_add(
                    (n_frames as u64 - 1)
                        .checked_mul(frame_size as u64)
                        .and_then(|v| v.checked_add(plane_bytes as u64))
                        .ok_or_else(|| {
                            BioFormatsError::Format("Norpix SEQ payload size overflows".into())
                        })?,
                )
                .ok_or_else(|| {
                    BioFormatsError::Format("Norpix SEQ payload offset overflows".into())
                })?;
            if file_len < required_len {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "Norpix SEQ pixel payload is shorter than declared: need {required_len} bytes, found {file_len}"
                )));
            }
            for i in 0..n_frames as u64 {
                let img_off = 1024 + i * frame_size as u64;
                frame_offsets.push(img_off);
                // Timestamp sits immediately after the raw image payload.
                let ts = read_seq_timestamp(&mut f, img_off + plane_bytes as u64, file_len);
                timestamps.push(ts);
            }
        }

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert(
            "format".into(),
            MetadataValue::String("Norpix StreamPix SEQ".into()),
        );
        if !printable_ascii(&hdr[..24]).is_empty() {
            meta_map.insert(
                "norpix.description".into(),
                MetadataValue::String(printable_ascii(&hdr[..24])),
            );
        }
        meta_map.insert(
            "norpix.version".into(),
            MetadataValue::Int(i64::from_le_bytes([
                hdr[24], hdr[25], hdr[26], hdr[27], hdr[28], hdr[29], hdr[30], hdr[31],
            ])),
        );
        meta_map.insert(
            "norpix.header_size".into(),
            MetadataValue::Int(header_size as i64),
        );
        meta_map.insert(
            "norpix.allocated_frames".into(),
            MetadataValue::Int(n_frames as i64),
        );
        meta_map.insert(
            "norpix.true_image_size".into(),
            MetadataValue::Int(true_image_size as i64),
        );
        meta_map.insert(
            "norpix.description_format".into(),
            MetadataValue::Int(desc_fmt as i64),
        );
        meta_map.insert(
            "norpix.compression".into(),
            MetadataValue::Int(compression as i64),
        );
        meta_map.insert("norpix.compressed".into(), MetadataValue::Bool(compressed));
        if timestamps.iter().any(|&t| t != 0.0) {
            meta_map.insert(
                "norpix.timestamps_unix_seconds".into(),
                MetadataValue::String(
                    timestamps
                        .iter()
                        .map(|t| t.to_string())
                        .collect::<Vec<_>>()
                        .join(","),
                ),
            );
        }

        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: n_frames,
            size_c: channels,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
            image_count: n_frames,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb,
            is_interleaved: true,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.frame_size = frame_size;
        self.compressed = compressed;
        self.frame_offsets = frame_offsets;
        self.frame_lengths = frame_lengths;
        self.timestamps = timestamps;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.compressed = false;
        self.frame_offsets.clear();
        self.frame_lengths.clear();
        self.timestamps.clear();
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
        let plane_bytes = (meta.size_x * meta.size_y * meta.size_c) as usize * bps;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;

        if self.compressed {
            // Decode exactly the length declared by the frame's size prefix.
            let start = *self
                .frame_offsets
                .get(plane_index as usize)
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
            let len = *self
                .frame_lengths
                .get(plane_index as usize)
                .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
            f.seek(SeekFrom::Start(start))
                .map_err(BioFormatsError::Io)?;
            let mut jpeg = vec![0u8; len];
            f.read_exact(&mut jpeg).map_err(BioFormatsError::Io)?;
            let decoded = crate::common::codec::decompress_jpeg(&jpeg)?;
            // jpeg-decoder returns interleaved samples in the natural order; for
            // BGR24 frames StreamPix stores blue-first, but the JPEG codec yields
            // RGB, so return as-is (matching the channel order of the decoder).
            return Ok(decoded);
        }

        let frame = if self.frame_size > 0 {
            self.frame_size
        } else {
            plane_bytes
        };
        let offset = self
            .frame_offsets
            .get(plane_index as usize)
            .copied()
            .unwrap_or(1024u64 + plane_index as u64 * frame as u64);
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
        let spp = meta.size_c as usize;
        crop_full_plane("Norpix SEQ", &full, meta, spp, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{OmeMetadata, OmePlane};
        let meta = self.meta.as_ref()?;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let _ = ome.add_original_metadata_annotations(meta, 0);
        if !self.timestamps.is_empty() {
            // Expose per-frame DeltaT relative to the first frame's timestamp.
            let base = self.timestamps[0];
            let img = &mut ome.images[0];
            img.planes = (0..meta.image_count)
                .map(|i| {
                    let z = i % meta.size_z;
                    OmePlane {
                        the_z: z,
                        the_c: 0,
                        the_t: 0,
                        delta_t: self.timestamps.get(i as usize).map(|t| t - base),
                        ..Default::default()
                    }
                })
                .collect();
        }
        Some(ome)
    }
}

// ─── IPLab ────────────────────────────────────────────────────────────────────
//
// IPLab (.ipl) is a format from Scanalytics used for multi-dimensional images.
//
// Header layout (little-endian):
//   Offset  0: magic — "ipl bina" (8 bytes) for binary data files
//   Offset  8: version (i32)
//   Offset 12: width (i32)
//   Offset 16: height (i32)
//   Offset 20: depth (i32) — number of z planes
//   Offset 24: n_channels (i32)
//   Offset 28: n_frames (i32) — time points
//   Offset 32: data_type (i32): 0=int8, 1=uint16, 2=int16, 3=float32, 4=uint8, 5=RGB, ...
//   Offset 36: color_mode (i32)
//   Pixel data starts at offset 96.

pub struct IplabReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl IplabReader {
    pub fn new() -> Self {
        IplabReader {
            path: None,
            meta: None,
        }
    }
}
impl Default for IplabReader {
    fn default() -> Self {
        Self::new()
    }
}

fn read_iplab_tags(path: &Path, offset: u64) -> Result<HashMap<String, MetadataValue>> {
    let mut meta_map = HashMap::new();
    let mut f = File::open(path).map_err(BioFormatsError::Io)?;
    let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
    if offset + 8 > file_len {
        return Ok(meta_map);
    }

    f.seek(SeekFrom::Start(offset))
        .map_err(BioFormatsError::Io)?;
    while f.stream_position().map_err(BioFormatsError::Io)? + 4 <= file_len {
        let mut tag = [0u8; 4];
        f.read_exact(&mut tag).map_err(BioFormatsError::Io)?;
        if &tag == b"fini" {
            break;
        }
        if f.stream_position().map_err(BioFormatsError::Io)? + 4 > file_len {
            break;
        }

        let mut size_bytes = [0u8; 4];
        f.read_exact(&mut size_bytes).map_err(BioFormatsError::Io)?;
        let size = u32::from_le_bytes(size_bytes) as usize;
        if f.stream_position().map_err(BioFormatsError::Io)? + size as u64 > file_len {
            break;
        }

        let mut payload = vec![0u8; size];
        f.read_exact(&mut payload).map_err(BioFormatsError::Io)?;
        let tag_name = printable_ascii(&tag);
        meta_map.insert(
            format!("iplab.tag.{tag_name}.size"),
            MetadataValue::Int(size as i64),
        );

        match &tag {
            b"clut" if size == 8 => {
                let lut_types = [
                    "monochrome",
                    "reverse monochrome",
                    "BGR",
                    "classify",
                    "rainbow",
                    "red",
                    "green",
                    "blue",
                    "cyan",
                    "magenta",
                    "yellow",
                    "saturated pixels",
                ];
                let kind = r_i32_le(&payload, 4);
                let label = lut_types
                    .get(kind as usize)
                    .copied()
                    .unwrap_or("unknown")
                    .to_string();
                meta_map.insert("LUT type".into(), MetadataValue::String(label));
            }
            b"head" => {
                for chunk in payload.chunks_exact(22) {
                    let num = i16::from_le_bytes([chunk[0], chunk[1]]);
                    meta_map.insert(
                        format!("Header{num}"),
                        MetadataValue::String(printable_ascii(&chunk[2..22])),
                    );
                }
            }
            b"note" if size >= 576 => {
                meta_map.insert(
                    "Descriptor".into(),
                    MetadataValue::String(printable_ascii(&payload[..64])),
                );
                meta_map.insert(
                    "Notes".into(),
                    MetadataValue::String(printable_ascii(&payload[64..576])),
                );
            }
            _ => {}
        }
    }

    Ok(meta_map)
}

impl FormatReader for IplabReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ipl") || e.eq_ignore_ascii_case("ipm"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.len() >= 8 && &header[..8] == b"ipl bina"
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; 96];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        let width = positive_i32_dim(r_i32_le(&hdr, 12), "width")?;
        let height = positive_i32_dim(r_i32_le(&hdr, 16), "height")?;
        let depth = positive_i32_dim(r_i32_le(&hdr, 20), "depth")?;
        let n_channels = positive_i32_dim(r_i32_le(&hdr, 24), "channel count")?;
        let n_frames = positive_i32_dim(r_i32_le(&hdr, 28), "frame count")?;
        let data_type = r_i32_le(&hdr, 32);

        let (pixel_type, bpp, spp): (PixelType, u8, u32) = match data_type {
            0 => (PixelType::Uint8, 8, 1), // int8 → report as uint8
            1 => (PixelType::Uint16, 16, 1),
            2 => (PixelType::Int16, 16, 1),
            3 => (PixelType::Float32, 32, 1),
            4 => (PixelType::Uint8, 8, 1),
            5 => (PixelType::Uint8, 8, 3), // RGB
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "IPLab unsupported data type {data_type}"
                )))
            }
        };
        let is_rgb = spp == 3;
        let image_count = depth * n_channels * n_frames;
        let plane_bytes = (width as u64)
            .checked_mul(height as u64)
            .and_then(|v| v.checked_mul(spp as u64))
            .and_then(|v| v.checked_mul(bpp as u64 / 8))
            .ok_or_else(|| BioFormatsError::Format("IPLab plane byte count overflows".into()))?;
        let pixel_bytes = plane_bytes
            .checked_mul(image_count as u64)
            .ok_or_else(|| BioFormatsError::Format("IPLab pixel byte count overflows".into()))?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        let required_len = 96u64
            .checked_add(pixel_bytes)
            .ok_or_else(|| BioFormatsError::Format("IPLab payload offset overflows".into()))?;
        if file_len < required_len {
            return Err(BioFormatsError::Format(format!(
                "IPLab pixel payload is truncated: need {required_len} bytes, found {file_len}"
            )));
        }

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("IPLab".into()));
        meta_map.insert(
            "iplab.version".into(),
            MetadataValue::Int(r_i32_le(&hdr, 8) as i64),
        );
        meta_map.insert(
            "iplab.data_type".into(),
            MetadataValue::Int(data_type as i64),
        );
        meta_map.insert(
            "iplab.color_mode".into(),
            MetadataValue::Int(r_i32_le(&hdr, 36) as i64),
        );
        meta_map.extend(read_iplab_tags(path, 96 + pixel_bytes).unwrap_or_default());

        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: depth,
            size_c: n_channels * spp,
            size_t: n_frames,
            pixel_type,
            bits_per_pixel: bpp,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb,
            is_interleaved: is_rgb,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
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
        let spp = if meta.is_rgb { 3usize } else { 1usize };
        let plane_bytes = (meta.size_x * meta.size_y) as usize * spp * bps;
        let offset = 96u64 + plane_index as u64 * plane_bytes as u64;
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
        let spp = if meta.is_rgb { 3usize } else { 1usize };
        crop_full_plane("IPLab", &full, meta, spp, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}
