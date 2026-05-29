//! Leica LEI confocal format reader.
//!
//! A Leica dataset consists of one `.lei` file plus one or more companion
//! `.tif` files holding the pixel data. All Leica TIFFs carry the private tag
//! `LEICA_MAGIC_TAG = 33923`.
//!
//! The `.lei` file is a custom binary container (not a flat pixel blob): it
//! begins with four endianness marker bytes, then a linked list of header
//! "IFD"-like blocks keyed by integer tags (SERIES=10, IMAGES=15,
//! DIMDESCR=20, ...). The IMAGES block lists the companion TIFF filenames
//! (stored as UTF-16) and the DIMDESCR block describes the Z/C/T dimensions
//! and dimension order. Pixel data is then read from the referenced TIFFs.
//!
//! This is a faithful (if partial) port of the upstream Java `LeicaReader`.

use std::collections::HashMap;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::path::confined_join;
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::tiff::TiffReader;

/// All Leica TIFFs carry this private IFD tag.
const LEICA_MAGIC_TAG: u16 = 33923;

// Header block (pseudo-IFD) tags.
const SERIES: i32 = 10;
const IMAGES: i32 = 15;
const DIMDESCR: i32 = 20;

/// Maps the Leica dimension id to an axis kind.
fn dimension_name(id: i32) -> &'static str {
    match id {
        120 => "x",
        121 => "y",
        122 => "z",
        116 => "t",
        6815843 => "channel",
        _ => "",
    }
}

// ── Little/big endian byte readers over an in-memory buffer ───────────────────

struct Cursor<'a> {
    data: &'a [u8],
    pos: usize,
    little: bool,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8], little: bool) -> Self {
        Cursor {
            data,
            pos: 0,
            little,
        }
    }
    fn seek(&mut self, p: usize) {
        self.pos = p.min(self.data.len());
    }
    fn skip(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.data.len());
    }
    fn read_i32(&mut self) -> i32 {
        if self.pos + 4 > self.data.len() {
            self.pos = self.data.len();
            return 0;
        }
        let b = &self.data[self.pos..self.pos + 4];
        self.pos += 4;
        if self.little {
            i32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            i32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    }
    /// Read `len` bytes and strip null bytes, mirroring DataTools.stripString
    /// over a UTF-16 buffer (keeps only the non-null bytes as ASCII).
    fn read_string(&mut self, len: usize) -> String {
        let end = (self.pos + len).min(self.data.len());
        let slice = &self.data[self.pos..end];
        self.pos = end;
        let bytes: Vec<u8> = slice.iter().copied().filter(|&c| c != 0).collect();
        String::from_utf8_lossy(&bytes).to_string()
    }
}

/// Per-series parsed state.
struct LeiSeries {
    meta: ImageMetadata,
    /// Companion TIFF file paths in raster order.
    files: Vec<PathBuf>,
}

/// A parsed header block: tag -> file pointer (position just past the size word).
type HeaderIfd = HashMap<i32, usize>;

/// Locate the .lei file for a given entry path.
///
/// - `.lei` entry: returns it directly.
/// - `.tif` entry: looks for a sibling `<prefix>.lei`, trimming `_` suffixes.
fn find_lei_file(path: &Path) -> Option<PathBuf> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    if ext.as_deref() == Some("lei") {
        return Some(path.to_path_buf());
    }
    if matches!(ext.as_deref(), Some("tif") | Some("tiff")) {
        let parent = path.parent()?;
        let mut prefix = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_string();
        loop {
            for cand in [format!("{prefix}.lei"), format!("{prefix}.LEI")] {
                let p = parent.join(&cand);
                if p.exists() {
                    return Some(p);
                }
            }
            match prefix.rfind('_') {
                Some(i) => prefix.truncate(i),
                None => break,
            }
        }
    }
    None
}

/// Parse the .lei binary container into a list of series.
fn parse_lei(lei_path: &Path) -> Result<Vec<LeiSeries>> {
    let mut f = File::open(lei_path).map_err(BioFormatsError::Io)?;
    let mut data = Vec::new();
    f.read_to_end(&mut data).map_err(BioFormatsError::Io)?;

    if data.len() < 12 {
        return Err(BioFormatsError::Format("LEI: file too small".into()));
    }

    // Endianness: the four marker bytes are all 0x49 ('I') for little-endian.
    let little = data[0] == 0x49 && data[1] == 0x49 && data[2] == 0x49 && data[3] == 0x49;

    let mut c = Cursor::new(&data, little);
    c.seek(0);
    c.skip(8);
    let mut addr = c.read_i32();

    // Walk the linked list of header IFD blocks.
    let mut header_ifds: Vec<HeaderIfd> = Vec::new();
    let mut guard = 0;
    while addr != 0 && guard < 4096 {
        guard += 1;
        let mut ifd: HeaderIfd = HashMap::new();
        c.seek(addr as usize + 4);
        let mut tag = c.read_i32();
        let mut tag_guard = 0;
        while tag != 0 && tag_guard < 65536 {
            tag_guard += 1;
            let offset = c.read_i32();
            let pos = c.pos;
            c.seek(offset as usize + 12);
            let _size = c.read_i32();
            ifd.insert(tag, c.pos);
            c.seek(pos);
            tag = c.read_i32();
        }
        header_ifds.push(ifd);
        addr = c.read_i32();
    }

    if header_ifds.is_empty() {
        return Err(BioFormatsError::Format("LEI: no header blocks".into()));
    }

    let dir = lei_path.parent().unwrap_or_else(|| Path::new("."));
    let mut name_length = 0usize;
    let mut series: Vec<LeiSeries> = Vec::new();

    for ifd in &header_ifds {
        if let Some(&series_ptr) = ifd.get(&SERIES) {
            c.seek(series_ptr);
            c.skip(8);
            name_length = (c.read_i32() as usize).saturating_mul(2);
        }

        let images_ptr = match ifd.get(&IMAGES) {
            Some(&p) => p,
            None => continue,
        };

        // parseFilenames
        c.seek(images_ptr);
        let mut temp_images = c.read_i32();
        if (temp_images as i64).saturating_mul(name_length as i64) > data.len() as i64 {
            // wrong endianness guess for this count
            let other = !little;
            let mut c2 = Cursor::new(&data, other);
            c2.seek(images_ptr);
            temp_images = c2.read_i32();
        }
        if temp_images <= 0 {
            return Err(BioFormatsError::Format(
                "LEI: image count must be positive".into(),
            ));
        }
        let temp_images = temp_images as usize;

        let raw_size_x = c.read_i32();
        let raw_size_y = c.read_i32();
        if raw_size_x <= 0 || raw_size_y <= 0 {
            return Err(BioFormatsError::Format(format!(
                "LEI: invalid image dimensions {raw_size_x}x{raw_size_y}"
            )));
        }
        let mut size_x = raw_size_x as u32;
        let mut size_y = raw_size_y as u32;
        c.skip(4);
        let raw_samples_per_pixel = c.read_i32();
        if raw_samples_per_pixel <= 0 {
            return Err(BioFormatsError::Format(format!(
                "LEI: invalid samples per pixel {raw_samples_per_pixel}"
            )));
        }
        let samples_per_pixel = raw_samples_per_pixel as u32;
        let mut is_rgb = samples_per_pixel > 1;
        let mut size_c = samples_per_pixel;

        let mut files: Vec<PathBuf> = Vec::with_capacity(temp_images);
        if name_length > 0 {
            for _ in 0..temp_images {
                let name = c.read_string(name_length);
                if !name.is_empty() {
                    if let Some(path) = confined_join(dir, &name) {
                        files.push(path);
                    }
                }
            }
        }
        // Fall back to scanning the directory for TIFFs if names were not usable.
        if files.is_empty() {
            let mut listing: Vec<PathBuf> = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .map(|e| e.path())
                        .filter(|p| {
                            p.extension()
                                .and_then(|e| e.to_str())
                                .map(|e| {
                                    e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff")
                                })
                                .unwrap_or(false)
                        })
                        .collect()
                })
                .unwrap_or_default();
            listing.sort();
            files = listing;
        } else {
            files.sort();
        }

        let mut size_z = 1u32;
        let mut size_t = 1u32;
        let mut pixel_type = PixelType::Uint8;
        let mut bpp_bytes = 1u32;
        let mut order_axes: Vec<char> = Vec::new();

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("Leica LEI".into()));
        // physicalSizes[0..5] = X, Y, Z, C, T physical sizes (µm / s), mirroring
        // Java LeicaReader.physicalSizes[seriesIndex].
        let mut physical_sizes = [0.0f64; 5];

        // DIMDESCR block: pixel type and dimensions.
        if let Some(&dim_ptr) = ifd.get(&DIMDESCR) {
            c.seek(dim_ptr);
            c.skip(4); // version/unused
                       // ms.rgb = in.readInt() == 20
            let voxel = c.read_i32();
            if voxel == 20 {
                is_rgb = true;
            }
            let mut bpp = c.read_i32();
            if bpp <= 0 {
                return Err(BioFormatsError::Format(format!(
                    "LEI: invalid bytes per pixel {bpp}"
                )));
            }
            if bpp % 3 == 0 {
                size_c = 3;
                is_rgb = true;
                bpp /= 3;
            }
            bpp_bytes = bpp as u32;
            pixel_type = match bpp_bytes {
                1 => PixelType::Uint8,
                2 => PixelType::Uint16,
                4 => PixelType::Float32,
                _ => {
                    return Err(BioFormatsError::Format(format!(
                        "LEI: unsupported bytes per pixel {bpp_bytes}"
                    )))
                }
            };

            let _resolution = c.read_i32(); // bits per pixel / real-world resolution
                                            // Maximum/Minimum voxel intensity strings (getString(true)).
            for _ in 0..2 {
                let l = c.read_i32().max(0) as usize * 2;
                c.skip(l);
            }
            let len = c.read_i32().max(0) as usize;
            c.skip(len * 2 + 4);

            let dim_count = c.read_i32().max(0);
            for j in 0..dim_count {
                let dim_id = c.read_i32();
                let dim_type = dimension_name(dim_id);
                let raw_size = c.read_i32();
                if raw_size <= 0 {
                    return Err(BioFormatsError::Format(format!(
                        "LEI: invalid dimension size {raw_size}"
                    )));
                }
                let size = raw_size as u32;
                let distance = c.read_i32();
                let strlen = c.read_i32().max(0) as usize * 2;
                let size_data = c.read_string(strlen);

                // Java: sizeData.split(" "); physical = value / size; "m" -> µm.
                let mut parts = size_data.split_whitespace();
                let physical_str = parts.next().unwrap_or("");
                let unit = parts.next().unwrap_or("");
                let mut physical = physical_str.parse::<f64>().unwrap_or(0.0) / size.max(1) as f64;
                if unit == "m" {
                    physical *= 1_000_000.0;
                }

                match dim_type {
                    "x" => {
                        size_x = size;
                        physical_sizes[0] = physical;
                    }
                    "y" => {
                        size_y = size;
                        physical_sizes[1] = physical;
                    }
                    "channel" => {
                        if size_c == 0 {
                            size_c = 1;
                        }
                        size_c *= size;
                        if !order_axes.contains(&'C') {
                            order_axes.push('C');
                        }
                        physical_sizes[3] = physical;
                    }
                    "z" => {
                        size_z = size;
                        if !order_axes.contains(&'Z') {
                            order_axes.push('Z');
                        }
                        physical_sizes[2] = physical;
                    }
                    _ => {
                        size_t = size;
                        if !order_axes.contains(&'T') {
                            order_axes.push('T');
                        }
                        physical_sizes[4] = physical;
                    }
                }

                // Per-dimension original metadata (Java "Dim<j> ..." keys).
                let dim_prefix = format!("Dim{}", j);
                meta_map.insert(
                    format!("{dim_prefix} type"),
                    MetadataValue::String(dim_type.to_string()),
                );
                meta_map.insert(
                    format!("{dim_prefix} size"),
                    MetadataValue::Int(size as i64),
                );
                meta_map.insert(
                    format!("{dim_prefix} distance between sub-dimensions"),
                    MetadataValue::Int(distance as i64),
                );
                meta_map.insert(
                    format!("{dim_prefix} physical length"),
                    MetadataValue::String(format!("{physical_str} {unit}")),
                );
                // physical origin (getString(true)): length-prefixed UTF-16.
                let origin_len = c.read_i32().max(0) as usize * 2;
                let origin = c.read_string(origin_len);
                meta_map.insert(
                    format!("{dim_prefix} physical origin"),
                    MetadataValue::String(origin),
                );
            }

            // Series name and description (getString(false)).
            let name_len = c.read_i32().max(0) as usize * 2;
            let series_name = c.read_string(name_len);
            meta_map.insert("Series name".into(), MetadataValue::String(series_name));
            let descr_len = c.read_i32().max(0) as usize * 2;
            let series_descr = c.read_string(descr_len);
            meta_map.insert(
                "Series description".into(),
                MetadataValue::String(series_descr),
            );
        }

        // Record physical sizes (µm for X/Y/Z/C, seconds for T time increment).
        for (idx, key) in [
            "physicalSizeX",
            "physicalSizeY",
            "physicalSizeZ",
            "physicalSizeC",
            "timeIncrement",
        ]
        .iter()
        .enumerate()
        {
            if physical_sizes[idx] > 0.0 {
                meta_map.insert((*key).into(), MetadataValue::Float(physical_sizes[idx]));
            }
        }

        if size_z == 0 {
            size_z = 1;
        }
        if size_t == 0 {
            size_t = 1;
        }
        if size_c == 0 {
            size_c = 1;
        }

        // Complete the dimension order (Java appends remaining axes).
        for a in ['C', 'Z', 'T'] {
            if !order_axes.contains(&a) {
                order_axes.push(a);
            }
        }
        let dimension_order = match (order_axes.first(), order_axes.get(1), order_axes.get(2)) {
            (Some('C'), Some('Z'), Some('T')) => DimensionOrder::XYCZT,
            (Some('C'), Some('T'), Some('Z')) => DimensionOrder::XYCTZ,
            (Some('Z'), Some('C'), Some('T')) => DimensionOrder::XYZCT,
            (Some('Z'), Some('T'), Some('C')) => DimensionOrder::XYZTC,
            (Some('T'), Some('C'), Some('Z')) => DimensionOrder::XYTCZ,
            (Some('T'), Some('Z'), Some('C')) => DimensionOrder::XYTZC,
            _ => DimensionOrder::XYZCT,
        };

        if files.is_empty() {
            continue;
        }

        let image_count = (size_z * size_c * size_t).max(files.len() as u32);

        let meta = ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel: (bpp_bytes * 8) as u8,
            image_count,
            dimension_order,
            is_rgb,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: little,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        };

        series.push(LeiSeries { meta, files });
    }

    if series.is_empty() {
        return Err(BioFormatsError::Format(
            "LEI: no valid series / TIFF files found".into(),
        ));
    }

    Ok(series)
}

pub struct LeiReader {
    path: Option<PathBuf>,
    series_list: Vec<LeiSeries>,
    series: usize,
}

impl LeiReader {
    pub fn new() -> Self {
        LeiReader {
            path: None,
            series_list: Vec::new(),
            series: 0,
        }
    }
}
impl Default for LeiReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for LeiReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lei"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // A Leica TIFF carries the private tag LEICA_MAGIC_TAG (33923). Scan the
        // first IFD's tag list for that tag id.
        tiff_has_tag(header, LEICA_MAGIC_TAG)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let lei = find_lei_file(path)
            .ok_or_else(|| BioFormatsError::Format("LEI file not found".into()))?;
        self.series_list = parse_lei(&lei)?;
        self.series = 0;
        self.path = Some(lei);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.series_list.clear();
        self.series = 0;
        Ok(())
    }
    fn series_count(&self) -> usize {
        self.series_list.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_list.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.series = s;
            Ok(())
        }
    }
    fn series(&self) -> usize {
        self.series
    }
    fn metadata(&self) -> &ImageMetadata {
        self.series_list
            .get(self.series)
            .map(|series| &series.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let s = self
            .series_list
            .get(self.series)
            .ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= s.meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        // Java: fileIndex = no < files.size() ? no : 0;
        //       planeIndex = no < files.size() ? 0 : no;
        let (file_index, page) = if (plane_index as usize) < s.files.len() {
            (plane_index as usize, 0u32)
        } else {
            (0usize, plane_index)
        };
        let file = s
            .files
            .get(file_index)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let mut r = TiffReader::new();
        r.set_id(file)?;
        let inner = r.metadata().image_count.max(1);
        if page >= inner {
            return Err(BioFormatsError::Format(format!(
                "LEI: TIFF page {page} out of range for {} ({} pages)",
                file.display(),
                inner
            )));
        }
        r.open_bytes(page)
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
        let meta = self.metadata();
        crop_region(&full, meta, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self
            .series_list
            .get(self.series)
            .map(|s| &s.meta)
            .ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

/// Clip an (x, y, w, h) region out of a full plane, with bounds validation.
pub(crate) fn crop_region(
    full: &[u8],
    meta: &ImageMetadata,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    let bps = meta.pixel_type.bytes_per_sample();
    let samples = if meta.is_rgb {
        meta.size_c.max(1) as usize
    } else {
        1
    };
    let pixel = bps * samples;
    let full_w = meta.size_x as usize;
    let full_h = meta.size_y as usize;
    let row = full_w * pixel;

    // Validate that the requested region lies within the plane.
    if x.checked_add(w).is_none_or(|end| end as usize > full_w)
        || y.checked_add(h).is_none_or(|end| end as usize > full_h)
    {
        return Err(BioFormatsError::Format(format!(
            "region {}x{}+{}+{} exceeds plane {}x{}",
            w, h, x, y, full_w, full_h
        )));
    }
    let out_row = w as usize * pixel;
    let mut out = Vec::with_capacity(h as usize * out_row);
    for r in 0..h as usize {
        let row_start = (y as usize + r) * row;
        let start = row_start + x as usize * pixel;
        let end = start + out_row;
        if end > full.len() {
            return Err(BioFormatsError::Format(
                "region extends past available pixel data".into(),
            ));
        }
        out.extend_from_slice(&full[start..end]);
    }
    Ok(out)
}

/// Minimal TIFF IFD tag scan: returns true if the first IFD contains `target`.
fn tiff_has_tag(header: &[u8], target: u16) -> bool {
    if header.len() < 8 {
        return false;
    }
    let little = match &header[0..2] {
        [0x49, 0x49] => true,
        [0x4D, 0x4D] => false,
        _ => return false,
    };
    let rd16 = |b: &[u8]| -> u16 {
        if little {
            u16::from_le_bytes([b[0], b[1]])
        } else {
            u16::from_be_bytes([b[0], b[1]])
        }
    };
    let rd32 = |b: &[u8]| -> u32 {
        if little {
            u32::from_le_bytes([b[0], b[1], b[2], b[3]])
        } else {
            u32::from_be_bytes([b[0], b[1], b[2], b[3]])
        }
    };
    let ifd_off = rd32(&header[4..8]) as usize;
    if ifd_off + 2 > header.len() {
        return false;
    }
    let entries = rd16(&header[ifd_off..ifd_off + 2]) as usize;
    let mut p = ifd_off + 2;
    for _ in 0..entries {
        if p + 2 > header.len() {
            break;
        }
        let tag = rd16(&header[p..p + 2]);
        if tag == target {
            return true;
        }
        p += 12; // each IFD entry is 12 bytes
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ImageWriter;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_lei_{nanos}_{name}"))
    }

    #[test]
    fn lei_companion_tiff_page_uses_exact_index() {
        let tiff = temp_path("single_page.tif");
        let meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 2,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 2,
            ..Default::default()
        };
        let tiff_meta = ImageMetadata {
            size_z: 1,
            image_count: 1,
            ..meta.clone()
        };
        ImageWriter::save(&tiff, &tiff_meta, &[vec![17]]).unwrap();
        let mut reader = LeiReader {
            path: None,
            series_list: vec![LeiSeries {
                meta,
                files: vec![tiff.clone()],
            }],
            series: 0,
        };

        let err = reader.open_bytes(1).unwrap_err();

        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("TIFF page 1 out of range")),
            "unexpected error: {err:?}"
        );
        let _ = std::fs::remove_file(tiff);
    }
}
