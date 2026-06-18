//! Norpix StreamPix SEQ, Image-Pro Sequence (SEQ), and IPLab format readers.

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
fn r_i32_be(b: &[u8], off: usize) -> i32 {
    i32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
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
    data_offset: u64,
    plane_samples: usize,
}

impl IplabReader {
    pub fn new() -> Self {
        IplabReader {
            path: None,
            meta: None,
            data_offset: 96,
            plane_samples: 1,
        }
    }
}
impl Default for IplabReader {
    fn default() -> Self {
        Self::new()
    }
}

fn read_iplab_tags(
    path: &Path,
    offset: u64,
    little_endian: bool,
) -> Result<HashMap<String, MetadataValue>> {
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
        let size = if little_endian {
            u32::from_le_bytes(size_bytes)
        } else {
            u32::from_be_bytes(size_bytes)
        } as usize;
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
                let kind = if little_endian {
                    r_i32_le(&payload, 4)
                } else {
                    r_i32_be(&payload, 4)
                };
                let label = lut_types
                    .get(kind as usize)
                    .copied()
                    .unwrap_or("unknown")
                    .to_string();
                meta_map.insert("LUT type".into(), MetadataValue::String(label));
            }
            b"head" => {
                for chunk in payload.chunks_exact(22) {
                    let num = if little_endian {
                        i16::from_le_bytes([chunk[0], chunk[1]])
                    } else {
                        i16::from_be_bytes([chunk[0], chunk[1]])
                    };
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
        if header.len() >= 8 && &header[..8] == b"ipl bina" {
            return true;
        }
        if header.len() < 12 {
            return false;
        }
        let little = &header[..4] == b"iiii";
        let big = &header[..4] == b"mmmm";
        if !little && !big {
            return false;
        }
        let size = if little {
            r_i32_le(header, 4)
        } else {
            r_i32_be(header, 4)
        };
        let version = if little {
            r_i32_le(header, 8)
        } else {
            r_i32_be(header, 8)
        };
        size == 4 && version >= 0x100e
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; 96];
        let n = f.read(&mut hdr).map_err(BioFormatsError::Io)?;
        if n < 12 {
            return Err(BioFormatsError::UnsupportedFormat(
                "IPLab header is truncated".into(),
            ));
        }

        let is_java_iplab = (&hdr[..4] == b"iiii" || &hdr[..4] == b"mmmm") && {
            let little = &hdr[..4] == b"iiii";
            let block_size = if little {
                r_i32_le(&hdr, 4)
            } else {
                r_i32_be(&hdr, 4)
            };
            let version = if little {
                r_i32_le(&hdr, 8)
            } else {
                r_i32_be(&hdr, 8)
            };
            block_size == 4 && version >= 0x100e
        };

        let (
            width,
            height,
            depth,
            n_channels,
            n_frames,
            data_type,
            pixel_type,
            bpp,
            spp,
            is_little_endian,
            data_offset,
            tag_offset,
            dimension_order,
            image_count,
            plane_samples,
            is_rgb,
        ) = if is_java_iplab {
            let little = &hdr[..4] == b"iiii";
            let read_i32 = |off: usize| {
                if little {
                    r_i32_le(&hdr, off)
                } else {
                    r_i32_be(&hdr, off)
                }
            };
            let data_size = read_i32(16) - 28;
            if data_size < 0 {
                return Err(BioFormatsError::Format(format!(
                    "IPLab data block size is negative ({data_size})"
                )));
            }
            let width = positive_i32_dim(read_i32(20), "width")?;
            let height = positive_i32_dim(read_i32(24), "height")?;
            let n_channels = positive_i32_dim(read_i32(28), "channel count")?;
            let depth = positive_i32_dim(read_i32(32), "depth")?;
            let n_frames = positive_i32_dim(read_i32(36), "frame count")?;
            let data_type = read_i32(40);
            let (pixel_type, bpp, spp): (PixelType, u8, u32) = match data_type {
                0 => (PixelType::Uint8, 8, 1),
                1 => (PixelType::Int16, 16, 1),
                2 => (PixelType::Uint16, 16, 1),
                3 => (PixelType::Int32, 32, 1),
                4 => (PixelType::Float32, 32, 1),
                5 => (PixelType::Uint32, 32, 1),
                6 => (PixelType::Uint16, 16, 1),
                10 => (PixelType::Float64, 64, 1),
                _ => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "IPLab unsupported data type {data_type}"
                    )))
                }
            };
            let image_count = depth
                .checked_mul(n_frames)
                .ok_or_else(|| BioFormatsError::Format("IPLab image count overflows".into()))?;
            (
                width,
                height,
                depth,
                n_channels,
                n_frames,
                data_type,
                pixel_type,
                bpp,
                spp,
                little,
                44u64,
                Some(44u64 + data_size as u64),
                if n_channels > 1 {
                    DimensionOrder::XYCZT
                } else {
                    DimensionOrder::XYZTC
                },
                image_count,
                (n_channels * spp) as usize,
                n_channels > 1,
            )
        } else {
            if n < 40 {
                return Err(BioFormatsError::UnsupportedFormat(
                    "IPLab binary header is truncated".into(),
                ));
            }
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
            let image_count = depth
                .checked_mul(n_channels)
                .and_then(|v| v.checked_mul(n_frames))
                .ok_or_else(|| BioFormatsError::Format("IPLab image count overflows".into()))?;
            (
                width,
                height,
                depth,
                n_channels,
                n_frames,
                data_type,
                pixel_type,
                bpp,
                spp,
                true,
                96u64,
                None,
                if n_channels * spp > 1 {
                    DimensionOrder::XYCZT
                } else {
                    DimensionOrder::XYZTC
                },
                image_count,
                spp as usize,
                spp == 3,
            )
        };

        let plane_bytes = (width as u64)
            .checked_mul(height as u64)
            .and_then(|v| v.checked_mul(plane_samples as u64))
            .and_then(|v| v.checked_mul(bpp as u64 / 8))
            .ok_or_else(|| BioFormatsError::Format("IPLab plane byte count overflows".into()))?;
        let pixel_bytes = plane_bytes
            .checked_mul(image_count as u64)
            .ok_or_else(|| BioFormatsError::Format("IPLab pixel byte count overflows".into()))?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        let required_len = data_offset
            .checked_add(pixel_bytes)
            .ok_or_else(|| BioFormatsError::Format("IPLab payload offset overflows".into()))?;
        if file_len < required_len {
            return Err(BioFormatsError::Format(format!(
                "IPLab pixel payload is truncated: need {required_len} bytes, found {file_len}"
            )));
        }

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("IPLab".into()));
        if is_java_iplab {
            let version = if is_little_endian {
                r_i32_le(&hdr, 8)
            } else {
                r_i32_be(&hdr, 8)
            };
            meta_map.insert("iplab.version".into(), MetadataValue::Int(version as i64));
        } else {
            meta_map.insert(
                "iplab.version".into(),
                MetadataValue::Int(r_i32_le(&hdr, 8) as i64),
            );
            meta_map.insert(
                "iplab.color_mode".into(),
                MetadataValue::Int(r_i32_le(&hdr, 36) as i64),
            );
        }
        meta_map.insert(
            "iplab.data_type".into(),
            MetadataValue::Int(data_type as i64),
        );
        let post_pixel_tags = tag_offset.unwrap_or(data_offset + pixel_bytes);
        meta_map
            .extend(read_iplab_tags(path, post_pixel_tags, is_little_endian).unwrap_or_default());

        self.data_offset = data_offset;
        self.plane_samples = plane_samples;

        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: depth,
            size_c: n_channels * spp,
            size_t: n_frames,
            pixel_type,
            bits_per_pixel: bpp,
            image_count,
            dimension_order,
            is_rgb,
            is_interleaved: is_rgb,
            is_indexed: false,
            is_little_endian,
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
        self.data_offset = 96;
        self.plane_samples = 1;
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
        let spp = self.plane_samples;
        let plane_bytes = (meta.size_x * meta.size_y) as usize * spp * bps;
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
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
        let spp = self.plane_samples;
        crop_full_plane("IPLab", &full, meta, spp, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ─── Image-Pro Sequence (SEQ) ─────────────────────────────────────────────────
//
// SEQReader (Java `loci.formats.in.SEQReader`) reads Image-Pro Sequence files
// (`.seq`/`.ips`). Unlike Norpix StreamPix `.seq` (a raw frame stream, handled by
// `NorpixReader` above), Image-Pro SEQ files are *TIFF* containers that carry a
// set of private "Image-Pro" tags. Pixel and standard TIFF metadata work is
// delegated to `crate::tiff::TiffReader`; the custom tags drive the Z/T counts,
// frame rate and dimension order, mirroring `BaseTiffReader` + SEQReader.

/// An array of shorts (length 12) with identical values in all of Bio-Formats'
/// samples; assumed to be some sort of format identifier.
const IMAGE_PRO_TAG_1: u16 = 50288;

/// Frame rate.
const IMAGE_PRO_TAG_2: u16 = 40105;

const IMAGE_PRO_TAG_3: u16 = 40100;

/// Image-Pro Sequence reader (`.seq`/`.ips`), TIFF-based.
pub struct SeqReader {
    inner: crate::tiff::TiffReader,
}

impl SeqReader {
    /// Constructs a new Image-Pro SEQ reader.
    pub fn new() -> Self {
        SeqReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }

    /// Mirror Java `SEQReader.isThisType(RandomAccessInputStream)`: parse the
    /// first IFD and require IMAGE_PRO_TAG_1 (stored as a `short[]`) or
    /// IMAGE_PRO_TAG_3 (stored as an `int[]`, i.e. TIFF SHORT). Operates on
    /// whatever header bytes are available; if the tags lie beyond the supplied
    /// window the parse fails gracefully and detection returns `false`. This
    /// keeps the TIFF-based Image-Pro reader from colliding with the raw
    /// `NorpixReader` (not a TIFF) or with generic TIFFs (which lack these tags).
    fn is_this_type_from_bytes(header: &[u8]) -> bool {
        let cursor = std::io::Cursor::new(header);
        let mut parser = match crate::tiff::parser::TiffParser::new(cursor) {
            Ok(p) => p,
            Err(_) => return false,
        };
        let offset = parser.first_ifd_offset;
        let ifd = match parser.read_ifd(offset) {
            Ok((ifd, _)) => ifd,
            Err(_) => return false,
        };
        let tag1_short = matches!(
            ifd.get(IMAGE_PRO_TAG_1),
            Some(crate::tiff::ifd::IfdValue::Short(_))
        );
        // TIFF SHORT values surface as `int[]` in Java; `IfdValue::Short` here.
        let tag3_int = matches!(
            ifd.get(IMAGE_PRO_TAG_3),
            Some(crate::tiff::ifd::IfdValue::Short(_))
        );
        tag1_short || tag3_int
    }

    /// Compute the OME dimension order string from the dominant axis, mirroring
    /// the tail of Java `SEQReader.initStandardMetadata` (lines 291-316).
    fn compute_dimension_order(size_z: u32, size_c: u32, size_t: u32) -> DimensionOrder {
        let mut order = String::from("XY");
        let dims = [size_z, size_c, size_t];
        let axes = ['Z', 'C', 'T'];

        let mut max_ndx = 0usize;
        let mut max = 0u32;
        for (i, &d) in dims.iter().enumerate() {
            if d > max {
                max = d;
                max_ndx = i;
            }
        }

        order.push(axes[max_ndx]);

        if max_ndx != 1 {
            if size_c > 1 {
                order.push('C');
                order.push(if max_ndx == 0 { axes[2] } else { axes[0] });
            } else {
                order.push(if max_ndx == 0 { axes[2] } else { axes[0] });
                order.push('C');
            }
        } else if size_z > size_t {
            order.push_str("ZT");
        } else {
            order.push_str("TZ");
        }

        match order.as_str() {
            "XYCTZ" => DimensionOrder::XYCTZ,
            "XYCZT" => DimensionOrder::XYCZT,
            "XYTCZ" => DimensionOrder::XYTCZ,
            "XYTZC" => DimensionOrder::XYTZC,
            "XYZCT" => DimensionOrder::XYZCT,
            "XYZTC" => DimensionOrder::XYZTC,
            _ => DimensionOrder::XYZTC,
        }
    }

    /// Mirror Java `SEQReader.initStandardMetadata`: after the standard TIFF
    /// metadata is built, derive Z/T from the Image-Pro tags and the first IFD's
    /// comment, then recompute the dimension order and plane count.
    fn init_standard_metadata(&mut self) {
        // super.initStandardMetadata() already ran inside TiffReader::set_id.
        let ifd_count = self.inner.ifd_count();

        // Read the per-IFD values we need before mutating the series metadata.
        let mut size_z: u32 = 0;
        let mut frame_rate: Option<i64> = None;
        let mut seq_id: Option<String> = None;
        for i in 0..ifd_count {
            let Some(ifd) = self.inner.ifd(i) else {
                continue;
            };
            if i == 0 {
                if let Some(crate::tiff::ifd::IfdValue::Short(vals)) = ifd.get(IMAGE_PRO_TAG_1) {
                    let mut id = String::new();
                    for v in vals {
                        id.push_str(&v.to_string());
                    }
                    seq_id = Some(id);
                }
            }
            // IMAGE_PRO_TAG_2 is read from the *first* IFD in Java
            // (ifds.get(0)) on every loop iteration; one image plane per IFD.
            if let Some(rate) = self
                .inner
                .ifd(0)
                .and_then(|f| f.get(IMAGE_PRO_TAG_2))
                .and_then(|v| v.as_u32())
            {
                size_z += 1;
                frame_rate = Some(rate as i64);
            }
        }

        let mut size_t: u32 = 0;

        if size_z == 0 {
            size_z = 1;
        }
        if size_t == 0 {
            size_t = 1;
        }
        if size_z == 1 && size_t == 1 {
            size_z = ifd_count as u32;
        }

        // Parse the description (first IFD comment) for channels/slices/frames.
        let comment = self
            .inner
            .ifd(0)
            .and_then(|ifd| ifd.get_str(crate::tiff::ifd::tag::IMAGE_DESCRIPTION))
            .map(str::to_owned);

        // Pull the current series metadata to read sizeC and write results back.
        let series = self.inner.series_list_mut();
        let Some(s0) = series.first_mut() else {
            return;
        };
        let m = &mut s0.metadata;
        let mut size_c = m.size_c;
        let is_rgb = m.is_rgb;

        if let Some(id) = seq_id {
            m.series_metadata
                .insert("Image-Pro SEQ ID".into(), MetadataValue::String(id));
        }
        if let Some(rate) = frame_rate {
            m.series_metadata
                .insert("Frame Rate".into(), MetadataValue::Int(rate));
        }
        m.series_metadata
            .insert("Number of images".into(), MetadataValue::Int(size_z as i64));
        m.series_metadata
            .insert("frames".into(), MetadataValue::Int(size_z as i64));
        m.series_metadata
            .insert("channels".into(), MetadataValue::Int(size_c as i64));
        m.series_metadata
            .insert("slices".into(), MetadataValue::Int(size_t as i64));

        if let Some(descr) = comment {
            m.series_metadata.remove("Comment");
            for token in descr.split('\n') {
                let token = token.trim();
                let eq = token.find('=').or_else(|| token.find(':'));
                if let Some(eq) = eq {
                    let label = &token[..eq];
                    let data = &token[eq + 1..];
                    m.series_metadata
                        .insert(label.to_string(), MetadataValue::String(data.to_string()));
                    if label == "channels" {
                        if let Ok(v) = data.trim().parse::<u32>() {
                            size_c = v;
                        }
                    } else if label == "frames" {
                        if let Ok(v) = data.trim().parse::<u32>() {
                            size_t = v;
                        }
                    } else if label == "slices" {
                        if let Ok(v) = data.trim().parse::<u32>() {
                            size_z = v;
                        }
                    }
                }
            }
        }

        if is_rgb && size_c != 3 {
            size_c *= 3;
        }

        let dimension_order = Self::compute_dimension_order(size_z, size_c, size_t);

        m.size_z = size_z;
        m.size_c = size_c;
        m.size_t = size_t;
        m.dimension_order = dimension_order;
        // imageCount mirrors getImageCount() = sizeZ * effectiveSizeC * sizeT;
        // for RGB data the samples share one plane, so use the IFD count when it
        // matches, otherwise recompute from the planar dimensions.
        let effective_c = if is_rgb { 1 } else { size_c };
        m.image_count = size_z.saturating_mul(effective_c).saturating_mul(size_t);
    }
}

impl Default for SeqReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SeqReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // Java: checkSuffix(name, "ips") => true; otherwise suffixSufficient is
        // false, so a plain ".seq" name alone is not sufficient (it is shared
        // with Norpix). The byte check disambiguates ".seq".
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("ips") || e.eq_ignore_ascii_case("seq"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::is_this_type_from_bytes(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.close()?;
        self.inner.set_id(path)?;
        self.init_standard_metadata();
        Ok(())
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
    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(plane_index)
    }
    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(plane_index, x, y, w, h)
    }
    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(plane_index)
    }
    fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    fn resolution(&self) -> usize {
        self.inner.resolution()
    }
}

#[cfg(test)]
mod seq_tests {
    use super::*;

    /// Build a minimal little-endian classic TIFF carrying the given extra IFD
    /// entries (tag, type, count, inline-value-or-offset bytes are computed by the
    /// caller via the `entries` list of (tag, field_type, count, value_u32)). For
    /// our purposes every custom tag value fits inline (<= 4 bytes) or is laid out
    /// after the IFD. To keep things simple we only emit tags whose data is laid
    /// out after the IFD, with the offset pointing at appended payload.
    ///
    /// Layout:
    ///   [0..8)   header: "II", 42, first-IFD-offset = 8
    ///   [8..)    IFD: entry count, entries (12 bytes each), next-IFD = 0
    ///   then     out-of-line payloads (strip pixels + tag arrays)
    fn build_tiff(extra: &[(u16, u16, u32, Vec<u8>)]) -> Vec<u8> {
        // Minimal 2x2 8-bit grayscale, single strip.
        let width = 2u32;
        let height = 2u32;
        let pixels: Vec<u8> = vec![0, 1, 2, 3];

        // Base structural tags (tag, type, count, inline-or-offset value).
        // type: 3 = SHORT, 4 = LONG.
        // We will place pixels and any out-of-line arrays after the IFD.
        let mut entries: Vec<(u16, u16, u32, Vec<u8>)> = Vec::new();

        // Placeholder for StripOffsets — filled once layout is known.
        entries.push((256, 4, 1, width.to_le_bytes().to_vec())); // ImageWidth
        entries.push((257, 4, 1, height.to_le_bytes().to_vec())); // ImageLength
        entries.push((258, 3, 1, vec![8, 0, 0, 0])); // BitsPerSample = 8
        entries.push((259, 3, 1, vec![1, 0, 0, 0])); // Compression = none
        entries.push((262, 3, 1, vec![1, 0, 0, 0])); // Photometric = BlackIsZero
        entries.push((273, 4, 1, vec![0, 0, 0, 0])); // StripOffsets (fixup later)
        entries.push((277, 3, 1, vec![1, 0, 0, 0])); // SamplesPerPixel = 1
        entries.push((278, 4, 1, height.to_le_bytes().to_vec())); // RowsPerStrip
        entries.push((279, 4, 1, (pixels.len() as u32).to_le_bytes().to_vec())); // StripByteCounts

        for e in extra {
            entries.push(e.clone());
        }
        // Sort by tag id (TIFF requires ascending tag order).
        entries.sort_by_key(|e| e.0);

        let n = entries.len();
        let ifd_start = 8usize;
        let ifd_len = 2 + n * 12 + 4; // count + entries + next-ifd ptr
        let mut out_of_line_start = ifd_start + ifd_len;

        // Compute out-of-line offsets: each entry whose payload > 4 bytes is
        // appended; the 4-byte value field then stores its offset.
        let mut payloads: Vec<u8> = Vec::new();
        let mut entry_values: Vec<[u8; 4]> = Vec::with_capacity(n);
        // First, reserve space for the pixel strip right after the IFD so we know
        // its offset; StripOffsets is fixed up afterwards.
        let strip_offset = out_of_line_start as u32;
        payloads.extend_from_slice(&pixels);
        out_of_line_start += pixels.len();

        for e in &entries {
            let (_tag, _ty, _count, value) = e;
            if value.len() <= 4 {
                let mut v = [0u8; 4];
                v[..value.len()].copy_from_slice(value);
                entry_values.push(v);
            } else {
                let off = out_of_line_start as u32;
                payloads.extend_from_slice(value);
                out_of_line_start += value.len();
                entry_values.push(off.to_le_bytes());
            }
        }

        // Fix up StripOffsets (tag 273) inline value to the strip offset.
        for (i, e) in entries.iter().enumerate() {
            if e.0 == 273 {
                entry_values[i] = strip_offset.to_le_bytes();
            }
        }

        let mut tiff = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&(ifd_start as u32).to_le_bytes());

        tiff.extend_from_slice(&(n as u16).to_le_bytes());
        for (i, e) in entries.iter().enumerate() {
            tiff.extend_from_slice(&e.0.to_le_bytes()); // tag
            tiff.extend_from_slice(&e.1.to_le_bytes()); // type
            tiff.extend_from_slice(&e.2.to_le_bytes()); // count
            tiff.extend_from_slice(&entry_values[i]); // value/offset
        }
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0

        tiff.extend_from_slice(&payloads);
        tiff
    }

    fn image_pro_tags() -> Vec<(u16, u16, u32, Vec<u8>)> {
        // IMAGE_PRO_TAG_1 (50288): SHORT[12] identical values.
        let mut tag1 = Vec::new();
        for _ in 0..12 {
            tag1.extend_from_slice(&7u16.to_le_bytes());
        }
        // IMAGE_PRO_TAG_2 (40105): SHORT frame rate = 30 (inline).
        // IMAGE_PRO_TAG_3 (40100): SHORT (int[]) marker = 1 (inline).
        vec![
            (IMAGE_PRO_TAG_1, 3, 12, tag1),
            (IMAGE_PRO_TAG_2, 3, 1, 30u16.to_le_bytes().to_vec()),
            (IMAGE_PRO_TAG_3, 3, 1, 1u16.to_le_bytes().to_vec()),
        ]
    }

    #[test]
    fn detects_image_pro_tiff_via_tag1_and_tag3() {
        let tiff = build_tiff(&image_pro_tags());
        assert!(
            SeqReader::is_this_type_from_bytes(&tiff),
            "Image-Pro TIFF (tag1 short[] + tag3 int[]) should be detected"
        );
    }

    #[test]
    fn rejects_plain_tiff() {
        let tiff = build_tiff(&[]);
        assert!(
            !SeqReader::is_this_type_from_bytes(&tiff),
            "generic TIFF without Image-Pro tags must not be claimed"
        );
    }

    #[test]
    fn rejects_norpix_streampix_seq() {
        // A raw Norpix StreamPix header is not a TIFF at all.
        let mut hdr = vec![0u8; 1024];
        hdr[..10].copy_from_slice(b"Norpix seq");
        assert!(
            !SeqReader::is_this_type_from_bytes(&hdr),
            "raw Norpix StreamPix .seq must not be claimed by the TIFF-based reader"
        );
        // And NorpixReader must still claim it.
        let norpix = NorpixReader::new();
        assert!(norpix.is_this_type_by_bytes(&hdr));
    }

    #[test]
    fn parses_dimensions_from_image_pro_tags() {
        let dir = std::env::temp_dir();
        let path = dir.join("imagepro_seq_test.seq");
        let tiff = build_tiff(&image_pro_tags());
        std::fs::write(&path, &tiff).unwrap();

        let mut reader = SeqReader::new();
        reader.set_id(&path).unwrap();
        let m = reader.metadata();
        assert_eq!(m.size_x, 2);
        assert_eq!(m.size_y, 2);
        // One IFD with IMAGE_PRO_TAG_2 present => sizeZ counted to 1, then the
        // (sizeZ==1 && sizeT==1) rule sets sizeZ = ifds.size() = 1.
        assert_eq!(m.size_z, 1, "single-plane Image-Pro SEQ => sizeZ = 1");
        assert_eq!(m.size_t, 1);
        // Frame Rate and SEQ ID recorded as global metadata.
        assert!(matches!(
            m.series_metadata.get("Frame Rate"),
            Some(MetadataValue::Int(30))
        ));
        assert!(matches!(
            m.series_metadata.get("Image-Pro SEQ ID"),
            Some(MetadataValue::String(_))
        ));

        // Pixel readback round-trips through the inner TIFF reader.
        let plane = reader.open_bytes(0).unwrap();
        assert_eq!(plane, vec![0, 1, 2, 3]);

        std::fs::remove_file(&path).ok();
    }

    #[test]
    fn dimension_order_picks_dominant_axis() {
        // Z dominant, C=1 => "XY" + "Z" + "TC" = XYZTC.
        assert_eq!(
            SeqReader::compute_dimension_order(5, 1, 2),
            DimensionOrder::XYZTC
        );
        // C dominant => "XY" + "C" + (Z>T? "ZT":"TZ").
        assert_eq!(
            SeqReader::compute_dimension_order(3, 9, 2),
            DimensionOrder::XYCZT
        );
        // T dominant, C>1 => "XY" + "T" + "C" + "Z" = XYTCZ.
        assert_eq!(
            SeqReader::compute_dimension_order(2, 3, 9),
            DimensionOrder::XYTCZ
        );
    }
}
