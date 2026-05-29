//! Bio-Rad PIC confocal format reader.
//!
//! 76-byte little-endian header followed by raw pixel data.
//! Magic: int16 at offset 54 == 12345

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

const HEADER_SIZE: u64 = 76;
const FILE_ID: i16 = 12345;

fn r_i16(b: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([b[off], b[off + 1]])
}
fn r_f32(b: &[u8], off: usize) -> f32 {
    f32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn positive_i16_dim(value: i16, label: &str) -> Result<u32> {
    if value <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Bio-Rad PIC {label} is non-positive ({value})"
        )));
    }
    Ok(value as u32)
}

pub struct BioRadReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    npic: u32,
    bytes_per_pixel: usize,
    /// All PIC files contributing planes, sorted. When more than one, each
    /// file supplies `npic` planes that together form the channels.
    pic_files: Vec<PathBuf>,
}

impl BioRadReader {
    pub fn new() -> Self {
        BioRadReader {
            path: None,
            meta: None,
            npic: 1,
            bytes_per_pixel: 1,
            pic_files: Vec::new(),
        }
    }
}

/// The maximum valid note type (NOTE_NAMES has 23 entries, indices 0..=22).
const NOTE_NAMES_LEN: i16 = 23;

/// A single Bio-Rad note (mirrors the Java `Note` class).
struct Note {
    x: i16,
    y: i16,
    /// The note text, trimmed of trailing binary/whitespace.
    p: String,
}

/// Read all of the note strings from the file, following the pixel data.
///
/// Mirrors Java `readNotes(s, true)`. The seek offset depends on whether a
/// multi-file group has already been established: if `pic_files` is None each
/// note block follows all `image_count` planes; otherwise it follows just the
/// per-file plane count (`image_count / n_files`).
///
/// Returns the collected notes plus any sizeZ/sizeT override implied by an
/// AXIS_4 note whose note-type token is 2 (single Z, time series).
struct ReadNotesResult {
    notes: Vec<Note>,
    size_z: Option<u32>,
    size_t: Option<u32>,
}

fn read_notes(
    f: &mut File,
    size_x: u32,
    size_y: u32,
    image_count: u32,
    bpp: usize,
    n_files: Option<u32>,
) -> ReadNotesResult {
    let mut result = ReadNotesResult {
        notes: Vec::new(),
        size_z: None,
        size_t: None,
    };

    // Java seeks to 70, then skips bpp * imageLen + 6.
    let mut image_len = size_x as u64 * size_y as u64;
    match n_files {
        None => image_len *= image_count as u64,
        Some(nf) if nf > 0 => image_len *= (image_count / nf) as u64,
        _ => image_len *= image_count as u64,
    }
    let notes_start = 70 + bpp as u64 * image_len + 6;
    if f.seek(SeekFrom::Start(notes_start)).is_err() {
        return result;
    }

    // Each note: level(i16), notesFlag(i32), num(i16), status(i16), type(i16),
    // x(i16), y(i16), text(80 bytes) = 16 + 80 bytes.
    let mut more = true;
    let mut guard = 0;
    while more && guard < 1_000_000 {
        guard += 1;
        let mut hdr = [0u8; 16];
        if f.read_exact(&mut hdr).is_err() {
            break;
        }
        more = i32::from_le_bytes([hdr[2], hdr[3], hdr[4], hdr[5]]) != 0;
        let note_type = i16::from_le_bytes([hdr[10], hdr[11]]);
        let x = i16::from_le_bytes([hdr[12], hdr[13]]);
        let y = i16::from_le_bytes([hdr[14], hdr[15]]);
        // Java: if type < 0 || type >= NOTE_NAMES.length -> broken notes, stop.
        if note_type < 0 || note_type >= NOTE_NAMES_LEN {
            break;
        }
        let mut text = [0u8; 80];
        if f.read_exact(&mut text).is_err() {
            break;
        }
        // Remove binary data (trim at first NUL), then trim whitespace.
        let end = text.iter().position(|&c| c == 0).unwrap_or(80);
        let p = String::from_utf8_lossy(&text[..end]).trim().to_string();

        // Java readNotes: tokenize value (with '=' removed); if tokens.len > 1
        // and tokens[1] parses to 2 and value contains "AXIS_4" -> sizeZ=1,
        // sizeT=imageCount.
        let value = p.replace('=', "");
        let tokens: Vec<&str> = value.split_whitespace().collect();
        if tokens.len() > 1 {
            if let Ok(nt) = tokens[1].parse::<i32>() {
                if nt == 2 && value.contains("AXIS_4") {
                    result.size_z = Some(1);
                    result.size_t = Some(image_count);
                }
            }
        }

        result.notes.push(Note { x, y, p });
    }
    result
}

/// Dimension overrides derived from the AXIS notes (mirrors Java `parseNotes`).
struct ParseNotesResult {
    multiple_files: bool,
    size_c: Option<u32>,
    size_z: Option<u32>,
    size_t: Option<u32>,
}

/// Port of the AXIS-parsing portion of Java `parseNotes`. For each note whose
/// text contains "AXIS" we read the axis type from token[1]; axisType 11 with
/// AXIS_4 marks a single-section multi-channel dataset, and with AXIS_9 marks a
/// multi-file channel split.
fn parse_notes(notes: &[Note], image_count: u32) -> ParseNotesResult {
    let mut result = ParseNotesResult {
        multiple_files: false,
        size_c: None,
        size_z: None,
        size_t: None,
    };

    for n in notes {
        if !n.p.contains("AXIS") {
            continue;
        }
        let cleaned = n.p.replace('=', "");
        let values: Vec<&str> = cleaned.split_whitespace().collect();
        if values.len() < 2 {
            continue;
        }
        let key = values[0];
        let axis_type: i32 = match values[1].parse() {
            Ok(v) => v,
            Err(_) => continue,
        };

        if axis_type == 11 && values.len() > 2 {
            // We currently rely on x/y for the RGB type metadata in Java, but
            // for dimension purposes only the key matters.
            let _ = (n.x, n.y);
            if key == "AXIS_4" {
                // single section multi-channel dataset
                result.size_c = Some(image_count);
                result.size_z = Some(1);
                result.size_t = Some(1);
            } else if key == "AXIS_9" {
                result.multiple_files = true;
                // sizeC = (int) Double.parseDouble(values[3])
                if let Some(v) = values.get(3) {
                    if let Ok(c) = v.parse::<f64>() {
                        result.size_c = Some(c as u32);
                    }
                }
            }
        }
    }
    result
}

impl Default for BioRadReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for BioRadReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("pic"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Magic: file_id at offset 54 == 12345 (little-endian)
        header.len() >= 56 && i16::from_le_bytes([header[54], header[55]]) == FILE_ID
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = [0u8; HEADER_SIZE as usize];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        if r_i16(&hdr, 54) != FILE_ID {
            return Err(BioFormatsError::Format("Not a Bio-Rad PIC file".into()));
        }

        let nx = positive_i16_dim(r_i16(&hdr, 0), "width")?;
        let ny = positive_i16_dim(r_i16(&hdr, 2), "height")?;
        let npic = positive_i16_dim(r_i16(&hdr, 4), "image count")?;
        // Java: pixelType = (byteFormat == 0) ? UINT16 : UINT8. Any nonzero
        // byteFormat means 8-bit data.
        let byte_format = r_i16(&hdr, 14); // 0=uint16 (2 bytes), nonzero=uint8 (1 byte)
        let bpp = if byte_format != 0 { 1usize } else { 2usize };
        let pixel_type = if bpp == 1 {
            PixelType::Uint8
        } else {
            PixelType::Uint16
        };
        let plane_bytes = (nx as u64)
            .checked_mul(ny as u64)
            .and_then(|v| v.checked_mul(bpp as u64))
            .ok_or_else(|| {
                BioFormatsError::Format("Bio-Rad PIC plane byte count overflows".into())
            })?;
        let pixel_bytes = plane_bytes.checked_mul(npic as u64).ok_or_else(|| {
            BioFormatsError::Format("Bio-Rad PIC pixel byte count overflows".into())
        })?;
        let required_len = HEADER_SIZE.checked_add(pixel_bytes).ok_or_else(|| {
            BioFormatsError::Format("Bio-Rad PIC payload offset overflows".into())
        })?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        if file_len < required_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Bio-Rad PIC pixel payload is shorter than declared: need {required_len} bytes, found {file_len}"
            )));
        }
        let name_bytes = &hdr[18..50];
        let name = String::from_utf8_lossy(name_bytes)
            .trim_end_matches('\0')
            .to_string();

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        if !name.is_empty() {
            meta_map.insert("name".into(), MetadataValue::String(name));
        }
        meta_map.insert("lens".into(), MetadataValue::Int(r_i16(&hdr, 64) as i64));
        meta_map.insert(
            "mag_factor".into(),
            MetadataValue::Float(r_f32(&hdr, 66) as f64),
        );

        // Java defaults: sizeZ = imageCount (npic), sizeC = 1, sizeT = 1.
        let mut size_z = npic;
        let mut size_c = 1u32;
        let mut size_t = 1u32;

        // Read notes (no group established yet, so notes follow all planes).
        let notes = read_notes(&mut f, nx, ny, npic, bpp, None);
        // readNotes AXIS_4/noteType==2 override: sizeZ=1, sizeT=imageCount.
        if let (Some(z), Some(t)) = (notes.size_z, notes.size_t) {
            size_z = z;
            size_t = t;
        }

        // parseNotes: AXIS-driven sizeC/sizeZ/sizeT derivation + multiple-files.
        let parsed = parse_notes(&notes.notes, npic);
        if let Some(c) = parsed.size_c {
            size_c = c;
        }
        if let Some(z) = parsed.size_z {
            size_z = z;
        }
        if let Some(t) = parsed.size_t {
            size_t = t;
        }
        let multiple_files = parsed.multiple_files;

        // File grouping: when notes indicate multiple files, enumerate the
        // sibling PIC files via a FilePattern over the numbered filename and
        // keep those whose length matches this file's length (Java
        // initFile/FilePattern path). Order by name (Arrays.sort(picFiles)).
        let mut pics: Vec<PathBuf> = Vec::new();
        if multiple_files {
            if let Ok(this_len) = std::fs::metadata(path).map(|m| m.len()) {
                if let Ok(pattern) = crate::stitcher::FilePattern::from_file(path) {
                    for file in pattern.filenames() {
                        let is_pic = file
                            .extension()
                            .and_then(|e| e.to_str())
                            .map(|e| e.eq_ignore_ascii_case("pic"))
                            .unwrap_or(false);
                        if is_pic
                            && std::fs::metadata(&file)
                                .map(|m| m.len() == this_len)
                                .unwrap_or(false)
                        {
                            pics.push(file);
                        }
                    }
                }
            }
            // Java: if pics.size() == 1, sizeC = 1.
            if pics.len() == 1 {
                size_c = 1;
            }
        }
        pics.sort();
        pics.dedup();

        // Java: if picFiles.length > 0 -> imageCount = npic * picFiles.length,
        // then sizeT or sizeC derived from the remainder. Otherwise picFiles is
        // null and imageCount stays npic.
        let pic_files: Vec<PathBuf>;
        let image_count: u32;
        if !pics.is_empty() {
            if size_c == 0 {
                size_c = 1;
            }
            let n_files = pics.len() as u32;
            image_count = npic * n_files;
            if multiple_files {
                let denom = (size_z * size_c).max(1);
                size_t = (image_count / denom).max(1);
            } else {
                let denom = (size_z * size_t).max(1);
                size_c = (image_count / denom).max(1);
            }
            pic_files = pics;
        } else {
            image_count = npic;
            pic_files = vec![path.to_path_buf()];
        }

        self.meta = Some(ImageMetadata {
            size_x: nx,
            size_y: ny,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel: (bpp * 8) as u8,
            image_count,
            dimension_order: DimensionOrder::XYCTZ,
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
        self.npic = npic;
        self.bytes_per_pixel = bpp;
        self.pic_files = pic_files;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pic_files.clear();
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
        let plane_bytes = (meta.size_x * meta.size_y) as usize * self.bytes_per_pixel;

        // Java openBytes: file = no % picFiles.length;
        // offset = (no / picFiles.length) * planeSize; then seek(offset + 76).
        // pic_files always holds >= 1 entry (the single source for one-file PICs).
        let n_files = self.pic_files.len().max(1) as u32;
        let file_idx = (plane_index % n_files) as usize;
        let local_plane = (plane_index / n_files) as u64;
        let path = self
            .pic_files
            .get(file_idx)
            .or_else(|| self.path.as_ref())
            .ok_or(BioFormatsError::NotInitialized)?;
        let offset = HEADER_SIZE + local_plane * plane_bytes as u64;
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
        crop_full_plane("Bio-Rad PIC", &full, meta, 1, x, y, w, h)
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
        if let Some(MetadataValue::String(n)) = meta.series_metadata.get("name") {
            img.name = Some(n.clone());
        }
        Some(ome)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(p: &str) -> Note {
        Note {
            x: 0,
            y: 0,
            p: p.to_string(),
        }
    }

    #[test]
    fn parse_notes_axis_4_multichannel() {
        // AXIS_4 with axisType 11 -> single section multi-channel: sizeC=imageCount.
        let notes = vec![note("AXIS_4 11 0 0")];
        let r = parse_notes(&notes, 3);
        assert!(!r.multiple_files);
        assert_eq!(r.size_c, Some(3));
        assert_eq!(r.size_z, Some(1));
        assert_eq!(r.size_t, Some(1));
    }

    #[test]
    fn parse_notes_axis_9_multifile() {
        // AXIS_9 with axisType 11 -> multiple files, sizeC from values[3].
        let notes = vec![note("AXIS_9 11 1 2")];
        let r = parse_notes(&notes, 4);
        assert!(r.multiple_files);
        assert_eq!(r.size_c, Some(2));
    }

    #[test]
    fn parse_notes_ignores_non_axis_11() {
        // axisType != 11 should not affect dimensions.
        let notes = vec![note("AXIS_2 257 0 1.0")];
        let r = parse_notes(&notes, 5);
        assert!(!r.multiple_files);
        assert_eq!(r.size_c, None);
        assert_eq!(r.size_z, None);
        assert_eq!(r.size_t, None);
    }
}
