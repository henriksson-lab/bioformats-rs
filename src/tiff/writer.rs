use std::fs::File;
use std::io::{BufWriter, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::pixel_type::PixelType;
use crate::common::writer::FormatWriter;

/// Compression scheme for the TIFF writer.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum WriteCompression {
    /// No compression (fastest).
    #[default]
    None,
    /// Deflate/Zlib (good ratio, moderate speed).
    Deflate,
    /// LZW (classic TIFF compression).
    Lzw,
}

/// TIFF writer — supports 8/16/32-bit integer and 32/64-bit float images,
/// single-plane and multi-plane (Z/C/T stacks), grayscale and RGB.
pub struct TiffWriter {
    compression: WriteCompression,
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    file: Option<BufWriter<File>>,
    /// (strip_offset, strip_byte_count) recorded per plane as they are written.
    plane_strips: Vec<(u64, u64)>,
    planes_written: u32,
}

impl TiffWriter {
    pub fn new() -> Self {
        TiffWriter {
            compression: WriteCompression::default(),
            path: None,
            meta: None,
            file: None,
            plane_strips: Vec::new(),
            planes_written: 0,
        }
    }

    pub fn with_compression(mut self, c: WriteCompression) -> Self {
        self.compression = c;
        self
    }
}

impl Default for TiffWriter {
    fn default() -> Self { Self::new() }
}

// ---- helpers ----------------------------------------------------------------

fn write_le_u16(w: &mut impl Write, v: u16) -> std::io::Result<()> { w.write_all(&v.to_le_bytes()) }
fn write_le_u32(w: &mut impl Write, v: u32) -> std::io::Result<()> { w.write_all(&v.to_le_bytes()) }

/// Returns (TIFF type code, bytes per element)
fn short_type() -> (u16, u32) { (3, 2) }
fn long_type() -> (u16, u32) { (4, 4) }
fn rational_type() -> (u16, u32) { (5, 8) }

/// One IFD entry — tag, type, count, value_or_offset.
struct Entry {
    tag: u16,
    typ: u16,
    count: u32,
    /// Either the value inline (≤ 4 bytes) as a u32, or an offset into the file.
    value_or_offset: u32,
}

/// Write a SHORT entry with a single value stored inline.
fn short_entry(tag: u16, value: u16) -> Entry {
    Entry { tag, typ: short_type().0, count: 1, value_or_offset: value as u32 }
}

/// Write a LONG entry with a single value stored inline.
fn long_entry(tag: u16, value: u32) -> Entry {
    Entry { tag, typ: long_type().0, count: 1, value_or_offset: value }
}

fn sample_format(pt: PixelType) -> u16 {
    match pt {
        PixelType::Int8 | PixelType::Int16 | PixelType::Int32 => 2,
        PixelType::Float32 | PixelType::Float64 => 3,
        _ => 1, // unsigned integer (default)
    }
}

fn bits_per_sample_value(pt: PixelType) -> u16 {
    match pt {
        PixelType::Bit => 1,
        PixelType::Int8 | PixelType::Uint8 => 8,
        PixelType::Int16 | PixelType::Uint16 => 16,
        PixelType::Int32 | PixelType::Uint32 | PixelType::Float32 => 32,
        PixelType::Float64 => 64,
    }
}

/// Compress one strip's worth of data.
fn compress(data: &[u8], scheme: WriteCompression) -> Result<Vec<u8>> {
    match scheme {
        WriteCompression::None => Ok(data.to_vec()),
        WriteCompression::Deflate => {
            use flate2::write::ZlibEncoder;
            use flate2::Compression;
            use std::io::Write;
            let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
            enc.write_all(data).map_err(BioFormatsError::Io)?;
            enc.finish().map_err(BioFormatsError::Io)
        }
        WriteCompression::Lzw => {
            use weezl::{BitOrder, encode::Encoder};
            let mut enc = Encoder::with_tiff_size_switch(BitOrder::Msb, 8);
            enc.encode(data).map_err(|e| BioFormatsError::Codec(e.to_string()))
        }
    }
}

fn compression_tag(scheme: WriteCompression) -> u16 {
    match scheme {
        WriteCompression::None => 1,
        WriteCompression::Lzw => 5,
        WriteCompression::Deflate => 8,
    }
}

// ---- FormatWriter impl -------------------------------------------------------

impl FormatWriter for TiffWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        let ext = path.extension().and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("tif") | Some("tiff") | Some("btf"))
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        self.meta = Some(meta.clone());
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.meta.as_ref().ok_or_else(|| {
            BioFormatsError::Format("set_metadata must be called before set_id".into())
        })?;
        let f = File::create(path).map_err(BioFormatsError::Io)?;
        let mut w = BufWriter::new(f);

        // Write TIFF header: II (LE), magic 42, placeholder IFD offset = 8
        w.write_all(b"II").map_err(BioFormatsError::Io)?;
        write_le_u16(&mut w, 42).map_err(BioFormatsError::Io)?;
        write_le_u32(&mut w, 8).map_err(BioFormatsError::Io)?; // IFD offset — will patch in close()

        self.path = Some(path.to_path_buf());
        self.file = Some(w);
        self.plane_strips.clear();
        self.planes_written = 0;
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index != self.planes_written {
            return Err(BioFormatsError::Format(format!(
                "TIFF writer: planes must be written in order; expected {}, got {}",
                self.planes_written, plane_index
            )));
        }

        let compressed = compress(data, self.compression)?;
        let w = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;

        let offset = w.seek(SeekFrom::Current(0)).map_err(BioFormatsError::Io)?;
        w.write_all(&compressed).map_err(BioFormatsError::Io)?;

        self.plane_strips.push((offset, compressed.len() as u64));
        self.planes_written += 1;
        let _ = meta; // used above for validation
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;
        let w = self.file.as_mut().ok_or(BioFormatsError::NotInitialized)?;

        let spp = if meta.is_rgb { meta.size_c } else { 1 } as u16;
        let bps = bits_per_sample_value(meta.pixel_type);
        let sf = sample_format(meta.pixel_type);
        let comp_tag = compression_tag(self.compression);
        let photometric: u16 = if meta.is_rgb { 2 } else { 1 }; // RGB=2, MinIsBlack=1

        // We write all IFDs after the image data.
        // Each IFD may need extra data (BitsPerSample array if spp>1, rational for resolution).

        let plane_count = self.plane_strips.len();

        // We collect the IFDs in reverse so we can chain them.
        // Gather byte blobs for each IFD.
        struct IfdBlob {
            ifd_bytes: Vec<u8>,
            extra_bytes: Vec<u8>,
        }

        let mut ifd_blobs: Vec<IfdBlob> = Vec::with_capacity(plane_count);

        for plane_idx in 0..plane_count {
            let (strip_offset, strip_byte_count) = self.plane_strips[plane_idx];

            // Build extra data (placed right after the IFD).
            // We'll store BitsPerSample array here if spp > 1, and resolution rationals.
            let mut extra: Vec<u8> = Vec::new();

            // IFD entry count (2 bytes) + entries (12 each) + next IFD offset (4 bytes)
            // We'll compute the IFD offset for this pass.
            // Pass 1: collect entries that need offsets.

            // BitsPerSample: if spp == 1, store inline; if > 1, needs offset
            let bps_offset_placeholder: u32; // offset into extra where BPS array lives
            let bps_entry;
            if spp == 1 {
                bps_entry = short_entry(258, bps);
                bps_offset_placeholder = 0;
            } else {
                bps_offset_placeholder = extra.len() as u32;
                for _ in 0..spp {
                    extra.extend_from_slice(&bps.to_le_bytes());
                }
                bps_entry = Entry { tag: 258, typ: short_type().0, count: spp as u32, value_or_offset: 0 /* filled later */ };
            }

            // XResolution and YResolution rationals (72/1)
            let xres_offset = extra.len() as u32;
            extra.extend_from_slice(&72u32.to_le_bytes());
            extra.extend_from_slice(&1u32.to_le_bytes());
            let yres_offset = extra.len() as u32;
            extra.extend_from_slice(&72u32.to_le_bytes());
            extra.extend_from_slice(&1u32.to_le_bytes());

            // Build sorted entry list
            let mut entries: Vec<Entry> = vec![
                long_entry(256, meta.size_x),
                long_entry(257, meta.size_y),
                bps_entry,
                short_entry(259, comp_tag),
                short_entry(262, photometric),
                Entry { tag: 273, typ: long_type().0, count: 1, value_or_offset: strip_offset as u32 },
                short_entry(277, spp as u16),
                long_entry(278, meta.size_y), // RowsPerStrip = full image height
                Entry { tag: 279, typ: long_type().0, count: 1, value_or_offset: strip_byte_count as u32 },
                Entry { tag: 282, typ: rational_type().0, count: 1, value_or_offset: 0 }, // XResolution
                Entry { tag: 283, typ: rational_type().0, count: 1, value_or_offset: 0 }, // YResolution
                short_entry(284, 1), // PlanarConfiguration = chunky
                short_entry(296, 2), // ResolutionUnit = inch
            ];

            // Add SampleFormat if not default (unsigned int = 1)
            if sf != 1 {
                entries.push(short_entry(339, sf));
            }

            entries.sort_by_key(|e| e.tag);

            // We'll write the IFD blob (we don't know the file offset yet, so we record
            // where the extra data is *relative to the IFD start*, then patch at write time).
            // Build the raw IFD bytes with placeholder offsets for extra data.
            let mut ifd_bytes: Vec<u8> = Vec::new();
            let entry_count = entries.len() as u16;
            ifd_bytes.extend_from_slice(&entry_count.to_le_bytes());

            for e in &entries {
                ifd_bytes.extend_from_slice(&e.tag.to_le_bytes());
                ifd_bytes.extend_from_slice(&e.typ.to_le_bytes());
                ifd_bytes.extend_from_slice(&e.count.to_le_bytes());
                ifd_bytes.extend_from_slice(&e.value_or_offset.to_le_bytes());
            }

            // Append next IFD placeholder (4 bytes)
            ifd_bytes.extend_from_slice(&0u32.to_le_bytes());

            ifd_blobs.push(IfdBlob { ifd_bytes, extra_bytes: extra });

            // Remember what we need to patch later:
            // - BitsPerSample offset (if spp > 1)
            // - XResolution offset
            // - YResolution offset
            // We'll do a second pass once we know the IFD file offsets.
            let _ = (bps_offset_placeholder, xres_offset, yres_offset);
        }

        // Now write IFDs to the file and patch offsets.
        // Write IFD chain: IFD0 extra0 IFD1 extra1 ...
        let first_ifd_file_offset = w.seek(SeekFrom::Current(0)).map_err(BioFormatsError::Io)?;

        let mut ifd_file_offsets: Vec<u64> = Vec::with_capacity(plane_count);
        let mut cursor = first_ifd_file_offset;
        for blob in &ifd_blobs {
            ifd_file_offsets.push(cursor);
            cursor += blob.ifd_bytes.len() as u64 + blob.extra_bytes.len() as u64;
        }

        for (plane_idx, blob) in ifd_blobs.iter_mut().enumerate() {
            let ifd_file_off = ifd_file_offsets[plane_idx];
            let extra_file_off = ifd_file_off + blob.ifd_bytes.len() as u64;

            // Patch the entry values that point into extra data.
            // We need to walk through entries again.
            // IFD layout: 2-byte count, then 12-byte entries.
            let entry_count = u16::from_le_bytes([blob.ifd_bytes[0], blob.ifd_bytes[1]]) as usize;
            for i in 0..entry_count {
                let off = 2 + i * 12;
                let tag = u16::from_le_bytes([blob.ifd_bytes[off], blob.ifd_bytes[off + 1]]);
                match tag {
                    258 => {
                        // BitsPerSample — patch to file offset of BPS array in extra
                        // (only if count > 1)
                        let count = u32::from_le_bytes([
                            blob.ifd_bytes[off+4], blob.ifd_bytes[off+5],
                            blob.ifd_bytes[off+6], blob.ifd_bytes[off+7]
                        ]);
                        if count > 1 {
                            // extra starts with BPS array at offset 0
                            let abs_off = (extra_file_off) as u32;
                            blob.ifd_bytes[off+8..off+12].copy_from_slice(&abs_off.to_le_bytes());
                        }
                    }
                    282 => {
                        // XResolution — 8 bytes before YResolution in extra
                        // Extra layout: [BPS array if spp>1], XRes(8), YRes(8)
                        // Find where XRes starts in extra:
                        let bps_extra_bytes = if spp > 1 { spp as u64 * 2 } else { 0 };
                        let abs_off = (extra_file_off + bps_extra_bytes) as u32;
                        blob.ifd_bytes[off+8..off+12].copy_from_slice(&abs_off.to_le_bytes());
                    }
                    283 => {
                        // YResolution
                        let bps_extra_bytes = if spp > 1 { spp as u64 * 2 } else { 0 };
                        let abs_off = (extra_file_off + bps_extra_bytes + 8) as u32;
                        blob.ifd_bytes[off+8..off+12].copy_from_slice(&abs_off.to_le_bytes());
                    }
                    // StripOffsets / StripByteCounts are already absolute (set from plane_strips)
                    _ => {}
                }
            }

            // Patch next-IFD offset (last 4 bytes of ifd_bytes)
            let next_ifd: u32 = if plane_idx + 1 < plane_count {
                ifd_file_offsets[plane_idx + 1] as u32
            } else {
                0
            };
            let last = blob.ifd_bytes.len() - 4;
            blob.ifd_bytes[last..].copy_from_slice(&next_ifd.to_le_bytes());

            w.write_all(&blob.ifd_bytes).map_err(BioFormatsError::Io)?;
            w.write_all(&blob.extra_bytes).map_err(BioFormatsError::Io)?;
        }

        // Patch header: write first_ifd_file_offset at byte 4
        w.seek(SeekFrom::Start(4)).map_err(BioFormatsError::Io)?;
        write_le_u32(w, first_ifd_file_offset as u32).map_err(BioFormatsError::Io)?;

        w.flush().map_err(BioFormatsError::Io)?;
        self.file = None;
        self.plane_strips.clear();
        self.planes_written = 0;
        Ok(())
    }

    fn can_do_stacks(&self) -> bool { true }
}
