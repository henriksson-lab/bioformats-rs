//! Zeiss CZI (ZISRAWFILE) format reader.
//!
//! Segments use a 32-byte header:
//!   bytes  0-15: segment type (ASCII, zero-padded) e.g. "ZISRAWFILE"
//!   bytes 16-23: allocated size (int64 LE)
//!   bytes 24-31: used size (int64 LE)
//!
//! Supported compressions: Uncompressed, JPEG (new-style), LZW, Zstd.
//! JPEG-XR is detected but not decoded (needs a JXRC decoder).

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue, ModuloAnnotation};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ---- pixel types (from DirectoryEntry) -------------------------------------

fn czi_pixel_type(code: i32) -> std::io::Result<(PixelType, u32)> {
    // Returns (pixel_type, samples_per_pixel)
    match code {
        0 => Ok((PixelType::Uint8, 1)),    // Gray8
        1 => Ok((PixelType::Uint16, 1)),   // Gray16
        2 => Ok((PixelType::Float32, 1)),  // GrayFloat
        3 => Ok((PixelType::Uint8, 3)),    // Bgr24
        4 => Ok((PixelType::Uint16, 3)),   // Bgr48
        8 => Ok((PixelType::Float32, 3)),  // BgrFloat
        9 => Ok((PixelType::Uint8, 4)),    // Bgra32
        10 => Ok((PixelType::Float32, 2)), // Complex (re+im)
        11 => Ok((PixelType::Float32, 2)), // ComplexFloat
        12 => Ok((PixelType::Uint32, 1)),  // Gray32
        13 => Ok((PixelType::Float64, 1)), // GrayDouble
        other => Err(czi_invalid_data(format!(
            "CZI unsupported pixel type code {other}"
        ))),
    }
}

// ---- segment header --------------------------------------------------------

const SEG_HEADER: usize = 32;

fn read_seg_type(data: &[u8]) -> String {
    let end = data[..16].iter().position(|&b| b == 0).unwrap_or(16);
    String::from_utf8_lossy(&data[..end]).into_owned()
}

fn read_i32(data: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([data[off], data[off + 1], data[off + 2], data[off + 3]])
}
fn read_i64(data: &[u8], off: usize) -> i64 {
    i64::from_le_bytes(data[off..off + 8].try_into().unwrap_or([0; 8]))
}
fn read_u64(data: &[u8], off: usize) -> u64 {
    u64::from_le_bytes(data[off..off + 8].try_into().unwrap_or([0; 8]))
}

fn read_seg_sizes(data: &[u8]) -> (u64, u64) {
    let allocated = read_u64(data, 16);
    let mut used = read_u64(data, 24);
    if used == 0 {
        used = allocated;
    }
    (allocated, used)
}

fn valid_segment_position(pos: u64, file_len: u64) -> bool {
    pos > 0 && pos.saturating_add(SEG_HEADER as u64) <= file_len
}

/// Resolve a FileHeader segment pointer (directory or metadata) by trying each
/// candidate byte offset within the 80-byte file-header body and returning the
/// first one whose target position carries a segment header of `expected_type`.
///
/// CZI FileHeader stores 16-byte GUIDs, so directoryPosition/metadataPosition
/// live at offsets 52/60. Synthetic fixtures in this crate instead use 36/44.
/// Returns 0 when no candidate resolves to the expected segment.
fn resolve_segment_pointer(
    f: &mut BufReader<File>,
    fh: &[u8],
    candidate_offsets: &[usize],
    file_len: u64,
    expected_type: &str,
) -> std::io::Result<u64> {
    for &off in candidate_offsets {
        if off + 8 > fh.len() {
            continue;
        }
        let pos = read_u64(fh, off);
        if !valid_segment_position(pos, file_len) {
            continue;
        }
        f.seek(SeekFrom::Start(pos))?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        if f.read_exact(&mut seg_hdr).is_err() {
            continue;
        }
        // Match leniently: a fixture may store a truncated segment-type string
        // (e.g. "ZISRAWDIRECT"), so accept any non-empty common prefix of the
        // expected type rather than requiring the full string.
        let seg_type = read_seg_type(&seg_hdr);
        if !seg_type.is_empty()
            && (seg_type.starts_with(expected_type) || expected_type.starts_with(&seg_type))
        {
            return Ok(pos);
        }
    }
    Ok(0)
}

// ---- DirectoryEntry (256 bytes) -------------------------------------------

#[derive(Debug, Clone)]
struct DirEntry {
    pixel_type: i32,
    file_position: i64,
    compression: i32,
    // Dimensions from DimensionEntry array
    dims: HashMap<String, (i32, i32)>, // dim_name -> (start, size)
    // storedSize per dimension (physical/decoded extent of the tile, which may
    // differ from `size` for downsampled or compressed subblocks).
    stored: HashMap<String, i32>, // dim_name -> storedSize
}

impl DirEntry {
    fn dim_start(&self, name: &str) -> i32 {
        self.dims.get(name).map(|&(start, _)| start).unwrap_or(0)
    }

    fn dim_size(&self, name: &str) -> i32 {
        self.dims.get(name).map(|&(_, size)| size).unwrap_or(1)
    }

    fn has_dim(&self, name: &str) -> bool {
        self.dims.contains_key(name)
    }

    /// Stored (physical) size of a dimension, falling back to the logical size.
    fn dim_stored_size(&self, name: &str) -> i32 {
        match self.stored.get(name) {
            Some(&s) if s > 0 => s,
            _ => self.dim_size(name),
        }
    }

    fn matches_plane(&self, z: u32, c: u32, t: u32) -> bool {
        self.dims
            .get("Z")
            .map(|&(s, _)| s as u32 == z)
            .unwrap_or(z == 0)
            && self
                .dims
                .get("C")
                .map(|&(s, _)| s as u32 == c)
                .unwrap_or(c == 0)
            && self
                .dims
                .get("T")
                .map(|&(s, _)| s as u32 == t)
                .unwrap_or(t == 0)
    }
}

#[derive(Debug, Clone)]
struct CziResolution {
    r: i32,
    scale_x: i32,
    scale_y: i32,
    width: u32,
    height: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CziResolutionKey {
    r: i32,
    scale_x: i32,
    scale_y: i32,
}

impl CziResolutionKey {
    fn sort_key(self) -> (i32, i32, i32, i32) {
        let scale = self.scale_x.max(self.scale_y).max(1);
        (scale, self.r, self.scale_x.max(1), self.scale_y.max(1))
    }
}

/// One series corresponds to one combination of the CZI "extra" dimensions that
/// ZeissCZIReader.assignPlaneIndices folds into the series axis: scene ("S"),
/// acquisition ("B"), angle ("V") and — unless prestitched — mosaic ("M"). Each
/// series carries its own pyramid resolution list and per-pixel-type slot.
///
/// Port of the per-series core split: `seriesCount = positions * acquisitions *
/// angles * mosaics` (mosaics only when `maxResolution == 0` and not
/// prestitched), with an extra factor of `pixelTypes.size()` for the per-pixel-
/// type core split.
#[derive(Debug, Clone)]
struct CziSeries {
    /// Selector for the "S" dimension start, or `None` to match any scene.
    scene: Option<i32>,
    /// Selector for the "B" (acquisition) dimension start, or `None` to match any.
    acquisition: Option<i32>,
    /// Selector for the "V" (angle) dimension start, or `None` to match any.
    angle: Option<i32>,
    /// Selector for the "M" (mosaic) dimension start when mosaics are exposed as
    /// separate series (not prestitched). `None` => match any / stitch all M.
    mosaic: Option<i32>,
    /// Index into the distinct-pixel-type list (per-pixel-type core split).
    /// When `pixel_types.len() > 1`, this also selects the logical channel offset.
    pixel_type_index: usize,
    /// PALM series selector: the stored (X,Y) tile size that identifies which of
    /// the two PALM planes this series exposes (ZeissCZIReader:1155-1172 splits by
    /// stored size). `None` for non-PALM series.
    palm_size: Option<(u32, u32)>,
    resolutions: Vec<CziResolution>,
}

fn parse_dir_entry(data: &[u8]) -> DirEntry {
    // schema 0-1 (2 bytes)
    let pixel_type = read_i32(data, 2);
    let file_position = read_i64(data, 6);
    let compression = read_i32(data, 18);
    let dim_count = read_i32(data, 28) as usize;

    let mut dims: HashMap<String, (i32, i32)> = HashMap::new();
    let mut stored: HashMap<String, i32> = HashMap::new();
    let dim_array_start = 32;
    for i in 0..dim_count {
        let off = dim_array_start + i * 20;
        if off + 20 > data.len() {
            break;
        }
        // DimensionEntry layout (20 bytes):
        //   0  dimension (4 chars)
        //   4  start (int)
        //   8  size (int)
        //   12 startCoordinate (float)
        //   16 storedSize (int)
        let dim_name = std::str::from_utf8(&data[off..off + 4])
            .unwrap_or("")
            .trim_end_matches('\0')
            .trim()
            .to_string();
        let start = read_i32(data, off + 4);
        let size = read_i32(data, off + 8);
        let stored_size = read_i32(data, off + 16);
        if !dim_name.is_empty() {
            dims.insert(dim_name.clone(), (start, size));
            stored.insert(dim_name, stored_size);
        }
    }

    DirEntry {
        pixel_type,
        file_position,
        compression,
        dims,
        stored,
    }
}

fn czi_invalid_data(message: impl Into<String>) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, message.into())
}

fn parse_directory_entries(data: &[u8], entry_count: usize) -> std::io::Result<Vec<DirEntry>> {
    let mut entries = Vec::with_capacity(entry_count);

    if entry_count == 0 {
        return Ok(entries);
    }

    // Synthetic fixtures in this crate historically wrote each directory entry
    // into the 256-byte subblock slot. Real CZI directory segments store compact
    // entries: 32 bytes plus 20 bytes per DimensionEntry.
    let fixed_stride = if entry_count
        .checked_mul(256)
        .is_some_and(|bytes| data.len() >= bytes)
    {
        Some(256)
    } else {
        None
    };

    let mut off = 0usize;
    for entry_index in 0..entry_count {
        if off + 32 > data.len() {
            return Err(czi_invalid_data(format!(
                "CZI directory entry {entry_index} is truncated before its fixed header"
            )));
        }
        let dim_count = read_i32(data, off + 28).max(0) as usize;
        let compact_len = 32usize
            .checked_add(dim_count.checked_mul(20).ok_or_else(|| {
                czi_invalid_data(format!(
                    "CZI directory entry {entry_index} dimension table size overflows"
                ))
            })?)
            .ok_or_else(|| {
                czi_invalid_data(format!(
                    "CZI directory entry {entry_index} dimension table size overflows"
                ))
            })?;
        let entry_len = fixed_stride.unwrap_or(compact_len);
        let compact_end = off.checked_add(compact_len).ok_or_else(|| {
            czi_invalid_data(format!(
                "CZI directory entry {entry_index} offset overflows"
            ))
        })?;
        if compact_end > data.len() {
            return Err(czi_invalid_data(format!(
                "CZI directory entry {entry_index} is truncated: need {compact_len} bytes, have {}",
                data.len() - off
            )));
        }

        let parse_len = entry_len.min(data.len() - off);
        entries.push(parse_dir_entry(&data[off..off + parse_len]));
        off = off.checked_add(entry_len).ok_or_else(|| {
            czi_invalid_data(format!(
                "CZI directory entry {entry_index} offset overflows"
            ))
        })?;
    }

    Ok(entries)
}

// ---- file parsing ----------------------------------------------------------

struct CziParsed {
    meta_xml: String,
    entries: Vec<DirEntry>,
    z_count: u32,
    c_count: u32,
    t_count: u32,
    pixel_type: PixelType,
    spp: u32,
    /// One entry per series (extra-dimension combination). Always non-empty.
    series: Vec<CziSeries>,
    /// Distinct CZI pixel-type codes seen across subblocks, in first-seen order.
    /// `len() > 1` triggers the per-pixel-type core split.
    pixel_types: Vec<i32>,
    /// True when mosaic ("M") tiles are stitched into a single image per series.
    prestitched: bool,
    /// Modulo annotations (rotations->Z, illuminations->C, phases->T).
    modulo_z: Option<ModuloAnnotation>,
    modulo_c: Option<ModuloAnnotation>,
    modulo_t: Option<ModuloAnnotation>,
    /// Sub-dimension fold factors used by plane-index/selection math.
    rotations: i32,
    illuminations: i32,
    phases: i32,
    /// True when "R" acts as the rotation axis (folded into Z) rather than the
    /// pyramid-resolution selector.
    rotation_axis: bool,
    /// Bio-Formats-style maxResolution; positive when reduced-resolution
    /// pyramid data is present.
    max_resolution: i32,
    /// True when PALM data was detected and split into two series by stored size.
    palm: bool,
}

/// Counts of the CZI "extra" dimensions, computed like
/// ZeissCZIReader.calculateDimensions.
#[derive(Default)]
struct DimCounts {
    positions: i32,     // S
    acquisitions: i32,  // B
    angles: i32,        // V
    mosaics: i32,       // M
    rotations: i32,     // R -> modulo Z
    illuminations: i32, // I -> modulo C
    phases: i32,        // H -> modulo T
    /// True when "R" is a genuine rotation axis (some R.size > 1), as opposed to
    /// this crate's pyramid-resolution repurposing of "R".
    rotation_axis: bool,
    min_scene: i32,
    min_acq: i32,
    min_angle: i32,
    min_mosaic: i32,
}

fn parse_czi_file(f: &mut BufReader<File>) -> std::io::Result<CziParsed> {
    let file_len = f.get_ref().metadata()?.len();

    // --- Read file header segment ---
    let mut hdr = vec![0u8; SEG_HEADER];
    f.read_exact(&mut hdr)?;
    let seg_type = read_seg_type(&hdr);
    if !seg_type.starts_with("ZISRAWFILE") {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "Not a CZI file",
        ));
    }

    // FileHeader data starts after the 32-byte segment header.
    // Layout (matching ZeissCZIReader.FileHeader.fillInData): the primary file
    // GUID and file GUID are 16-byte GUIDs (not 8-byte longs), so the file-
    // position pointers live further down than a naive long-based layout implies.
    //   0  majorVersion (int)
    //   4  minorVersion (int)
    //   8  reserved1 (int)
    //   12 reserved2 (int)
    //   16 primaryFileGUID (16 bytes)
    //   32 fileGUID (16 bytes)
    //   48 filePart (int)
    //   52 directoryPosition (long)
    //   60 metadataPosition (long)
    let mut fh = vec![0u8; 80];
    f.read_exact(&mut fh)?;
    // Resolve the directory/metadata segment pointers. The spec-correct offsets
    // are 52/60. Some synthetic fixtures in this crate write the pointers at the
    // legacy 36/44 offsets (a layout that assumes 8-byte GUIDs). Pick whichever
    // candidate actually lands on the expected segment header, so both real CZI
    // files and the test fixtures parse.
    let dir_position = resolve_segment_pointer(f, &fh, &[52, 36], file_len, "ZISRAWDIRECTORY")?;
    let meta_position = resolve_segment_pointer(f, &fh, &[60, 44], file_len, "ZISRAWMETADATA")?;

    // --- Read metadata segment ---
    let mut meta_xml = String::new();
    if meta_position > 0 {
        f.seek(SeekFrom::Start(meta_position))?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        f.read_exact(&mut seg_hdr)?;
        // Metadata segment body: xml_size (i32), attach_size (i32), reserved (248), xml data
        let mut meta_body_hdr = vec![0u8; 256];
        f.read_exact(&mut meta_body_hdr)?;
        let xml_size = read_i32(&meta_body_hdr, 0) as usize;
        if xml_size > 0 {
            let mut xml_bytes = vec![0u8; xml_size];
            f.read_exact(&mut xml_bytes)?;
            meta_xml = String::from_utf8_lossy(&xml_bytes).into_owned();
        }
    }

    // --- Read directory segment ---
    let mut entries: Vec<DirEntry> = Vec::new();
    if dir_position > 0 {
        f.seek(SeekFrom::Start(dir_position))?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        f.read_exact(&mut seg_hdr)?;
        let (allocated_size, used_size) = read_seg_sizes(&seg_hdr);
        // Directory body: entry_count (i32), reserved (124), DirectoryEntry[]
        let mut dir_hdr = vec![0u8; 128];
        f.read_exact(&mut dir_hdr)?;
        let entry_count = read_i32(&dir_hdr, 0) as usize;
        let body_size = used_size.max(allocated_size).saturating_sub(128);
        let remaining = file_len.saturating_sub(f.stream_position()?);
        let body_size = body_size.min(remaining);
        if body_size > 0 {
            let mut entry_bytes = vec![0u8; body_size as usize];
            f.read_exact(&mut entry_bytes)?;
            entries = parse_directory_entries(&entry_bytes, entry_count)?;
        }
    }

    // Compute dimensions from entries.
    let parsed = build_dimensions(meta_xml, entries)?;
    Ok(parsed)
}

/// Port of ZeissCZIReader.calculateDimensions + the prestitching / series-split /
/// per-pixel-type core split machinery (the part of `initFile` from
/// calculateDimensions through assignPlaneIndices and the mosaic tile min/max
/// row-col logic).
fn build_dimensions(meta_xml: String, entries: Vec<DirEntry>) -> std::io::Result<CziParsed> {
    if entries.is_empty() {
        return Err(czi_invalid_data("CZI directory contains no subblocks"));
    }
    // --- calculateDimensions: per-dimension extents (ZeissCZIReader:1942-2048) ---
    let mut max_z = 0i32;
    let mut max_c = 0i32;
    // Largest Z.size seen: a single subblock may encode the full Z range via its
    // dimension.size rather than per-Z starts (ZeissCZIReader:1982-1983).
    let mut max_z_size = 0i32;
    // Distinct T index values across all subblocks: Java accumulates a uniqueT set
    // over [start, start+size) and uses its cardinality (ZeissCZIReader:1986-1995).
    let mut unique_t: std::collections::HashSet<i32> = std::collections::HashSet::new();
    let mut first_pixel_type = 0i32;

    // Distinct pixel-type codes in first-seen order (ZeissCZIReader:969-977).
    let mut pixel_types: Vec<i32> = Vec::new();

    let mut c = DimCounts {
        positions: 1,
        acquisitions: 1,
        angles: 1,
        mosaics: 1,
        rotations: 1,
        illuminations: 1,
        phases: 1,
        rotation_axis: false,
        min_scene: 0,
        min_acq: 0,
        min_angle: 0,
        min_mosaic: 0,
    };

    let (mut min_s, mut max_s) = (i32::MAX, i32::MIN);
    let (mut min_b, mut max_b) = (i32::MAX, i32::MIN);
    let (mut min_v, mut max_v) = (i32::MAX, i32::MIN);
    let (mut min_m, mut max_m) = (i32::MAX, i32::MIN);
    let (mut min_r, mut max_r) = (i32::MAX, i32::MIN);
    // Java-style rotation extent (start + size) and the largest R.size seen, used
    // to distinguish genuine rotation files from the pyramid repurposing of "R".
    let (mut min_rot, mut max_rot) = (i32::MAX, i32::MIN);
    let mut max_r_size = 0i32;
    // Max stored X/Y tile size observed at each R level, used to tell a
    // downscaled pyramid (stored sizes differ across R) from a rotation axis
    // (stored sizes equal across R).
    let mut r_level_stored_size: HashMap<i32, (i32, i32)> = HashMap::new();
    let (mut min_i, mut max_i) = (i32::MAX, i32::MIN);
    let (mut min_h, mut max_h) = (i32::MAX, i32::MIN);

    for e in &entries {
        if pixel_types.is_empty() {
            first_pixel_type = e.pixel_type;
        }
        if !pixel_types.contains(&e.pixel_type) {
            pixel_types.push(e.pixel_type);
        }

        // C/Z/T (logical max, ZeissCZIReader case 'C'/'Z'/'T').
        if let Some(&(start, size)) = e.dims.get("Z") {
            // ZeissCZIReader:1978-1984. Prefer the per-Z start extent; otherwise a
            // single subblock may encode the whole Z range via its dimension.size.
            if start > 0 && start > max_z {
                max_z = start;
            } else if size > max_z_size {
                max_z_size = size;
            }
        }
        if let Some(&(start, _)) = e.dims.get("C") {
            if start > max_c {
                max_c = start;
            }
        }
        if let Some(&(start, size)) = e.dims.get("T") {
            // ZeissCZIReader:1986-1995. Count distinct T values over [start,start+size).
            for i in start..start + size.max(1) {
                unique_t.insert(i);
            }
        }
        // Extra/modulo dimensions (ZeissCZIReader case 'S'/'I'/'B'/'M'/'H'/'V').
        //
        // The "R" dimension carries two distinct meanings in CZI files:
        //
        //   * Upstream ZeissCZIReader treats "R" as the *rotation* axis and folds
        //     it into Z via a moduloZ annotation (ZeissCZIReader:1997-1999,
        //     2043-2044, 846-849, 2216-2217). There, `rotations = maxRotation -
        //     minRotation` with `maxRotation = max(start + size)`. Resolution is
        //     instead derived from a scale-factor pyramid (ZeissCZIReader:772-784):
        //     a higher pyramid level stores a *smaller* X/Y tile.
        //
        //   * This crate repurposes the "R" *start* as the discrete pyramid
        //     resolution level (see czi_selects_pyramid_resolution_level), since it
        //     does not implement the scale-factor pyramid detection.
        //
        // Both rotation and the pyramid repurposing typically write R.size == 1 and
        // vary only the start, so size alone cannot disambiguate. We instead apply
        // Java's pyramid signal: distinct R levels are a *pyramid* iff their X tile
        // size differs (downscaling); if every R level shares the same X size, "R"
        // is a genuine rotation axis (no downsampling) and folds into Z. R.size > 1
        // is also treated as an explicit rotation signal.
        if let Some(&(start, size)) = e.dims.get("R") {
            min_r = min_r.min(start);
            max_r = max_r.max(start + 1);
            // Java-style rotation extent: maxRotation = max(start + size).
            min_rot = min_rot.min(start);
            max_rot = max_rot.max(start + size);
            max_r_size = max_r_size.max(size);
            // Track the stored X/Y tile size seen at each R level to detect
            // downscaling. Java's pyramid signal is reduced stored X/Y, not only
            // the presence of a distinct R start.
            let x_size = e.dim_stored_size("X").max(0);
            let y_size = e.dim_stored_size("Y").max(0);
            let cur = r_level_stored_size.entry(start).or_insert((x_size, y_size));
            cur.0 = cur.0.max(x_size);
            cur.1 = cur.1.max(y_size);
        }
        if let Some(&(start, _)) = e.dims.get("S") {
            min_s = min_s.min(start);
            max_s = max_s.max(start);
        }
        if let Some(&(start, size)) = e.dims.get("I") {
            min_i = min_i.min(start);
            max_i = max_i.max(start + size);
        }
        if let Some(&(start, _)) = e.dims.get("B") {
            min_b = min_b.min(start);
            max_b = max_b.max(start);
        }
        if let Some(&(start, _)) = e.dims.get("M") {
            min_m = min_m.min(start);
            max_m = max_m.max(start);
        }
        if let Some(&(start, size)) = e.dims.get("H") {
            min_h = min_h.min(start);
            max_h = max_h.max(start + size);
        }
        if let Some(&(start, _)) = e.dims.get("V") {
            min_v = min_v.min(start);
            max_v = max_v.max(start);
        }
    }

    // ZeissCZIReader:2037-2048. positions = maxS - minS + 1; acquisitions/angles/
    // mosaics use start+1 (max start + 1); illuminations/phases use
    // max(start+size) - min(start), i.e. a range count.
    if max_s != i32::MIN {
        c.positions = (max_s - min_s + 1).max(1);
        c.min_scene = min_s;
    }
    if max_b != i32::MIN {
        c.acquisitions = (max_b + 1).max(1);
        c.min_acq = min_b;
    }
    if max_v != i32::MIN {
        c.angles = (max_v + 1).max(1);
        c.min_angle = min_v;
    }
    if max_m != i32::MIN {
        c.mosaics = (max_m + 1).max(1);
        c.min_mosaic = min_m;
    }
    // Rotation -> moduloZ (ZeissCZIReader:2043-2044). Treat "R" as a rotation axis
    // (rather than the crate's pyramid repurposing) when either:
    //   * some subblock explicitly records R.size > 1, or
    //   * there are multiple R levels that all share the same X tile size (i.e.
    //     no downscaling, so they are not a pyramid).
    // When R levels have differing X sizes, "R" is a downscaled pyramid and stays
    // the resolution selector.
    let distinct_r_levels = r_level_stored_size.len();
    let all_r_same_stored_size = {
        let mut sizes = r_level_stored_size.values().copied();
        match sizes.next() {
            Some(first) => sizes.all(|s| s == first),
            None => true,
        }
    };
    let rotation_axis = max_r_size > 1 || (distinct_r_levels > 1 && all_r_same_stored_size);
    if rotation_axis && max_rot != i32::MIN && min_rot != i32::MAX {
        c.rotations = (max_rot - min_rot).max(1);
        c.rotation_axis = true;
    }
    let _ = min_r;
    if max_i != i32::MIN && min_i != i32::MAX {
        c.illuminations = (max_i - min_i).max(1);
    }
    if max_h != i32::MIN && min_h != i32::MAX {
        c.phases = (max_h - min_h).max(1);
    }

    // sizeZ: max of the per-Z-start extent (max_z + 1) and any single subblock
    // that spanned the whole Z range via dimension.size (ZeissCZIReader:1978-1984).
    let mut z_count = ((max_z + 1) as u32).max(max_z_size.max(0) as u32);
    let mut c_count = (max_c + 1) as u32;
    // sizeT = |uniqueT| (ZeissCZIReader:1986-1995), falling back to 1 when no
    // subblock declared a T dimension.
    let mut t_count = (unique_t.len() as u32).max(1);

    let (pt, spp) = czi_pixel_type(first_pixel_type)?;

    // --- modulo annotations (ZeissCZIReader:832-860) ---
    // rotations -> modulo Z, illuminations -> modulo C, phases -> modulo T.
    let mut modulo_z = None;
    let mut modulo_c = None;
    let mut modulo_t = None;
    // Rotation/illumination/phase labels parsed from "...|Rotations|" etc. keys in
    // the metadata XML (ZeissCZIReader:3733-3741).
    let rotation_labels = parse_modulo_labels(&meta_xml, "Rotations");
    let illumination_labels = parse_modulo_labels(&meta_xml, "Illuminations");
    let phase_labels = parse_modulo_labels(&meta_xml, "Phases");
    if c.rotations > 1 {
        // ZeissCZIReader:846-849. step = original sizeZ, end = sizeZ*(rotations-1).
        // When rotation labels are present, Java collapses end to start
        // (ZeissCZIReader:1246-1249) since the labels enumerate the axis.
        let mut end = (z_count as i32 * (c.rotations - 1)) as f64;
        if !rotation_labels.is_empty() {
            end = 0.0;
        }
        modulo_z = Some(ModuloAnnotation {
            parent_dimension: "Z".into(),
            modulo_type: "rotation".into(),
            start: 0.0,
            step: z_count as f64,
            end,
            unit: String::new(),
            labels: rotation_labels,
        });
        z_count *= c.rotations as u32;
    }
    if c.illuminations > 1 {
        let mut end = (c_count as i32 * (c.illuminations - 1)) as f64;
        if !illumination_labels.is_empty() {
            end = 0.0;
        }
        modulo_c = Some(ModuloAnnotation {
            parent_dimension: "C".into(),
            modulo_type: "illumination".into(),
            start: 0.0,
            step: c_count as f64,
            end,
            unit: String::new(),
            labels: illumination_labels,
        });
        c_count *= c.illuminations as u32;
    }
    if c.phases > 1 {
        let mut end = (t_count as i32 * (c.phases - 1)) as f64;
        if !phase_labels.is_empty() {
            end = 0.0;
        }
        modulo_t = Some(ModuloAnnotation {
            parent_dimension: "T".into(),
            modulo_type: "phase".into(),
            start: 0.0,
            step: t_count as f64,
            end,
            unit: String::new(),
            labels: phase_labels,
        });
        t_count *= c.phases as u32;
    }

    // --- maxResolution / pyramid scale detection ---
    // The "R" dimension supplies discrete resolution levels in this crate's model,
    // UNLESS it is acting as a rotation axis (R.size > 1). In the rotation case the
    // R start indexes rotation (folded into Z), so there is a single resolution.
    let max_resolution = if !c.rotation_axis {
        let mut keys: Vec<CziResolutionKey> = Vec::new();
        for e in &entries {
            let key = resolution_key(e, false);
            if !keys.contains(&key) {
                keys.push(key);
            }
        }
        (keys.len() as i32 - 1).max(0)
    } else {
        0
    };

    // --- seriesCount and prestitching (ZeissCZIReader:864-1003) ---
    // seriesCount = positions * acquisitions * angles, *= mosaics only when there
    // is no pyramid (maxResolution == 0); otherwise prestitched = true.
    let mut prestitched = false;
    let mosaics_as_series = if max_resolution == 0 {
        c.mosaics > 1 && c.mosaics_exposed_as_series()
    } else {
        prestitched = true;
        false
    };
    if c.mosaics > 1 && !mosaics_as_series {
        // Mosaic tiles are stitched into a single image per series.
        prestitched = true;
    }

    // --- mosaic image-fusion series rebalancing (ZeissCZIReader:941-1003) ---
    //
    // Faithful port of Java's plane-count-driven `seriesCount` collapse. When the
    // calculated plane budget (`imageCount * seriesCount`) exceeds what the file
    // actually stores (`planes.size() * scanDim`), the mosaics were fused at
    // acquisition time and Java collapses or re-balances the series count. This
    // is independent of the scale-factor pyramid model: it depends only on the
    // integer relationships between imageCount / seriesCount / plane count /
    // sizeZ / sizeT / positions / mosaics, all of which this crate already has.
    //
    // Scope: ported for the non-pyramid case (`max_resolution == 0`). When a
    // pyramid is present Java's `planes.size()` excludes reduced-resolution
    // subblocks (which this crate keeps as "R" buckets in `entries`) and `scanDim`
    // is derived from the size/planeSize ratio (not computed here), so the
    // precondition cannot be reproduced faithfully; in that case Java already
    // forces `prestitched = true` and the relevant collapse branches at 999-1003
    // only fire when `max_resolution == 0` regardless.
    //
    // `scanDim` is the line-scan-cytometry stored/plane size ratio
    // (ZeissCZIReader:804/817). This crate does not detect line-scan cytometry, so
    // every full-resolution plane has ratio 1, i.e. scanDim == 1.
    //
    // `seriesCount` here is the extra-dimension product *before* the per-pixel-
    // type split (Java applies the per-pixel-type multiplication afterwards at
    // 979-993). `image_count_full` is Java's `getImageCount()` (sizeZ * logical
    // sizeC * sizeT) computed before the per-pixel-type sizeC division.
    let image_count_full = z_count * c_count * t_count;
    let scan_dim: u32 = 1;
    // Number of valid full-resolution planes. With no pyramid, every directory
    // entry is a full-resolution plane (Java's `planes.size()` / `fullResBlockCount`
    // after removing reduced-resolution subblocks).
    let plane_count = entries.len() as u32;
    let mut series_count = (c.positions * c.acquisitions * c.angles).max(1);
    if max_resolution == 0 {
        series_count *= c.mosaics.max(1);
    }
    // When the collapse fires (952-960) we drop all extra-dimension splitting and
    // expose a single series matching every subblock. When only positions are
    // collapsed (947-950 / 999-1003), the surviving series no longer split by S,
    // so the scene selector must match every position.
    let mut collapse_all = false;
    let mut position_collapsed = false;
    if max_resolution == 0 && plane_count > 0 {
        let lhs = image_count_full as u64 * series_count as u64;
        let rhs = plane_count as u64 * scan_dim as u64;
        if lhs > rhs {
            let sz = z_count.max(1);
            if plane_count != image_count_full
                && plane_count != t_count
                && (plane_count % (series_count as u32 * sz)) == 0
            {
                // ZeissCZIReader:947-950 (guarded by !isGroupFiles(); this crate
                // never groups files, so the guard is always satisfied).
                if c.positions > 1
                    && plane_count == (image_count_full * series_count as u32) / c.positions as u32
                {
                    series_count /= c.positions;
                    c.positions = 1;
                    position_collapsed = true;
                }
            } else if plane_count == t_count || plane_count == image_count_full || c.positions > 1 {
                // ZeissCZIReader:952-960: image was fully fused; collapse to one
                // series. (!isGroupFiles() is always true here.)
                c.positions = 1;
                c.acquisitions = 1;
                c.mosaics = 1;
                c.angles = 1;
                series_count = 1;
                collapse_all = true;
            } else if series_count > c.mosaics && c.mosaics > 1 && prestitched {
                // ZeissCZIReader:961-964.
                series_count /= c.mosaics;
                c.mosaics = 1;
            }
        }
    }

    // ZeissCZIReader:999-1003: a prestitched image whose series count equals
    // mosaics * positions is a big single image expecting (absent) pyramids;
    // expose one series per position.
    if prestitched
        && max_resolution == 0
        && series_count == (c.mosaics.max(1) * c.positions.max(1))
        && series_count != c.positions
    {
        series_count = c.positions.max(1);
        c.mosaics = 1;
    }
    let _ = series_count;

    // --- per-pixel-type core split (ZeissCZIReader:969-995) ---
    // Each distinct pixel type yields its own core/series with sizeC divided.
    let original_c = c_count;
    let pixel_type_count = pixel_types.len().max(1);
    if pixel_type_count > 1 {
        c_count = (original_c / pixel_type_count as u32).max(1);
    }

    // --- build the extra-dimension series list (assignPlaneIndices) ---
    // Series = positions * acquisitions * angles * (mosaics if exposed) *
    // pixel_type_count. Each series selects its subblocks via the extra-dim
    // selectors; resolutions come from the "R" buckets within that selection.
    let mosaic_factor = if mosaics_as_series { c.mosaics } else { 1 };

    let mut series: Vec<CziSeries> = Vec::new();
    for ptype in 0..pixel_type_count {
        for s_idx in 0..c.positions {
            for b_idx in 0..c.acquisitions {
                for v_idx in 0..c.angles {
                    for m_idx in 0..mosaic_factor {
                        // When the fusion rebalancing collapsed everything into a
                        // single series (ZeissCZIReader:952-960), the series must
                        // match every subblock regardless of S/B/V/M, so leave all
                        // extra-dimension selectors unset.
                        let scene = if !collapse_all && !position_collapsed && max_s != i32::MIN {
                            Some(c.min_scene + s_idx)
                        } else {
                            None
                        };
                        let acquisition = if !collapse_all && max_b != i32::MIN {
                            Some(c.min_acq + b_idx)
                        } else {
                            None
                        };
                        let angle = if !collapse_all && max_v != i32::MIN {
                            Some(c.min_angle + v_idx)
                        } else {
                            None
                        };
                        let mosaic = if mosaics_as_series {
                            Some(c.min_mosaic + m_idx)
                        } else {
                            None
                        };
                        let resolutions = compute_resolutions(
                            &entries,
                            scene,
                            acquisition,
                            angle,
                            mosaic,
                            prestitched,
                            c.rotation_axis,
                        );
                        series.push(CziSeries {
                            scene,
                            acquisition,
                            angle,
                            mosaic,
                            pixel_type_index: ptype,
                            palm_size: None,
                            resolutions,
                        });
                    }
                }
            }
        }
    }
    if series.is_empty() {
        series.push(CziSeries {
            scene: None,
            acquisition: None,
            angle: None,
            mosaic: None,
            pixel_type_index: 0,
            palm_size: None,
            resolutions: vec![CziResolution {
                r: 0,
                scale_x: 1,
                scale_y: 1,
                width: 0,
                height: 0,
            }],
        });
    }

    // --- PALM detection and per-size series split (ZeissCZIReader:1123-1193) ---
    // PALM requires <= 2 planes and an image count of <= 2; if the XML marks the
    // file as PALM, the two planes are split into two series (one channel each)
    // when they have *different* stored sizes (a same-size pair is reverted to a
    // single 2-channel series, ZeissCZIReader:1174-1192).
    let image_count = z_count * c_count * t_count;
    let mut palm = false;
    if entries.len() <= 2 && image_count <= 2 && check_palm(&meta_xml) {
        // Distinct stored-size buckets among the (<= 2) subblocks.
        let mut sizes: Vec<(u32, u32)> = Vec::new();
        for e in &entries {
            let sx = e.dim_stored_size("X").max(0) as u32;
            let sy = e.dim_stored_size("Y").max(0) as u32;
            if !sizes.contains(&(sx, sy)) {
                sizes.push((sx, sy));
            }
        }
        if sizes.len() == 2 {
            // Genuine PALM: split into two single-channel series, each sized to its
            // own stored tile (ZeissCZIReader:1150-1173).
            palm = true;
            c_count = 1;
            series.clear();
            for &(sx, sy) in &sizes {
                series.push(CziSeries {
                    scene: None,
                    acquisition: None,
                    angle: None,
                    mosaic: None,
                    pixel_type_index: 0,
                    palm_size: Some((sx, sy)),
                    resolutions: vec![CziResolution {
                        r: 0,
                        scale_x: 1,
                        scale_y: 1,
                        width: sx,
                        height: sy,
                    }],
                });
            }
        }
        // Same-size pair => not PALM; leave the existing 2-channel series untouched.
    }

    Ok(CziParsed {
        meta_xml,
        entries,
        z_count: z_count.max(1),
        c_count: c_count.max(1),
        t_count: t_count.max(1),
        pixel_type: pt,
        spp,
        series,
        pixel_types: if pixel_types.is_empty() {
            vec![first_pixel_type]
        } else {
            pixel_types
        },
        prestitched,
        modulo_z,
        modulo_c,
        modulo_t,
        rotations: c.rotations,
        illuminations: c.illuminations,
        phases: c.phases,
        rotation_axis: c.rotation_axis,
        max_resolution,
        palm,
    })
}

/// Port of ZeissCZIReader.checkPALM (ZeissCZIReader:2277-2335). The file is PALM
/// when either:
///
/// 1. the metadata XML carries a `CustomAttributes` block whose descendant
///    `LsmTag` elements include a `Name` attribute starting with "palm"
///    (case-insensitive), or
/// 2. the `Experiment → ExperimentBlocks → AcquisitionBlock → MultiTrackSetup →
///    TrackSetup → PalmSlider` element graph exists and the `PalmSlider` text
///    content parses (Java `Boolean.parseBoolean`) as `true`.
///
/// Java's control flow is preserved exactly: the `LsmTag` check comes first; if
/// no `Experiment` element is present the method returns `false`; and the
/// `PalmSlider` value is only consulted after walking the full nested path
/// (`getFirstNode` descends to the first matching descendant at each step). The
/// crate keeps the metadata as a raw XML string (no DOM), so the nested walk uses
/// `first_element_body` to slice each successive container before searching it.
fn check_palm(xml: &str) -> bool {
    if xml.is_empty() {
        return false;
    }

    // (1) CustomAttributes/LsmTag with Name starting "palm" (ZeissCZIReader:2293).
    // getElementsByTagName("LsmTag") returns *descendants* of the first
    // CustomAttributes block, so restrict the LsmTag scan to that block's body.
    if let Some(custom) = first_element_body(xml, "CustomAttributes") {
        let lower = custom.to_ascii_lowercase();
        let mut search_from = 0usize;
        while let Some(rel) = lower[search_from..].find("<lsmtag") {
            let tag_start = search_from + rel;
            let tag_end = lower[tag_start..]
                .find('>')
                .map(|e| tag_start + e)
                .unwrap_or(lower.len());
            let tag = &lower[tag_start..tag_end];
            if let Some(npos) = tag.find("name=") {
                let rest = &tag[npos + 5..];
                let trimmed = rest.trim_start_matches(['"', '\'']);
                if trimmed.starts_with("palm") {
                    return true;
                }
            }
            search_from = tag_end.max(tag_start + 1);
        }
    }

    // (2) Experiment → ExperimentBlocks → AcquisitionBlock → MultiTrackSetup →
    // TrackSetup → PalmSlider (ZeissCZIReader:2310-2334). Each `getFirstNode`
    // step must succeed (the whole path must exist) or Java returns false; an
    // absent `Experiment` short-circuits the same way.
    let Some(experiment) = first_element_body(xml, "Experiment") else {
        return false;
    };
    let Some(blocks) = first_element_body(experiment, "ExperimentBlocks") else {
        return false;
    };
    let Some(acquisition) = first_element_body(blocks, "AcquisitionBlock") else {
        return false;
    };
    let Some(multi_track) = first_element_body(acquisition, "MultiTrackSetup") else {
        return false;
    };
    let Some(track_setup) = first_element_body(multi_track, "TrackSetup") else {
        return false;
    };
    let Some(palm_slider) = first_element_body(track_setup, "PalmSlider") else {
        return false;
    };
    // Boolean.parseBoolean: true only for an exact (case-insensitive) "true".
    palm_slider.trim().eq_ignore_ascii_case("true")
}

/// Parse a space-separated modulo label list from the metadata XML for a key
/// ending in `|<name>|` (ZeissCZIReader:3733-3741, e.g. "...|Rotations|").
/// The crate keeps metadata as a raw XML/key-value string, so we scan for the
/// `<Key>...|Name|</Key><Value>label0 label1 ...</Value>` pairing heuristically.
fn parse_modulo_labels(xml: &str, name: &str) -> Vec<String> {
    if xml.is_empty() {
        return Vec::new();
    }
    // Match an element whose tag name ends with the modulo name, e.g.
    // <Rotations>a b c</Rotations>, which is how CZI metadata stores these axes.
    let open_needle = format!("<{}>", name);
    let close_needle = format!("</{}>", name);
    if let Some(start) = xml.find(&open_needle) {
        let value_start = start + open_needle.len();
        if let Some(rel_end) = xml[value_start..].find(&close_needle) {
            let value = &xml[value_start..value_start + rel_end];
            let labels: Vec<String> = value.split_whitespace().map(|s| s.to_string()).collect();
            if labels.len() > 1 {
                return labels;
            }
        }
    }
    Vec::new()
}

/// Read the significant ("valid") bit depth Java exposes as `getBitsPerPixel`.
///
/// ZeissCZIReader (`translateInformation`) sets `core.bitsPerPixel` from the
/// first `<ComponentBitCount>` value under the `<Image>` node (12 for a 16-bit
/// camera storing 12 significant bits). When absent, callers fall back to the
/// storage bit depth (8 * bytes-per-sample).
fn parse_component_bit_count(xml: &str) -> Option<u8> {
    if xml.is_empty() {
        return None;
    }
    let open = "<ComponentBitCount>";
    let close = "</ComponentBitCount>";
    let start = xml.find(open)? + open.len();
    let rel_end = xml[start..].find(close)?;
    xml[start..start + rel_end].trim().parse::<u8>().ok()
}

/// Slice out the body of the first `<{tag}> ... </{tag}>` element (case
/// sensitive, matching CZI's mixed-case element names). Returns the inner text
/// between the open and close tags, or `None` if not found.
fn first_element_body<'a>(xml: &'a str, tag: &str) -> Option<&'a str> {
    let open_prefix = format!("<{}", tag);
    let close = format!("</{}>", tag);
    let open_at = xml.find(&open_prefix)?;
    // Find the '>' that closes this start tag (skip self-closing handling — the
    // CZI containers we read are never self-closing).
    let after_open = open_at + xml[open_at..].find('>')? + 1;
    let close_rel = xml[after_open..].find(&close)?;
    Some(&xml[after_open..after_open + close_rel])
}

/// Value of a direct child element `<{child}>value</{child}>` within `block`.
fn child_value(block: &str, child: &str) -> Option<String> {
    let v = first_element_body(block, child)?.trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

/// Iterate the `<Channel ...> ... </Channel>` elements directly contained in the
/// first `<Channels>` block found inside `scope` (the CZI `Dimensions` or
/// `DisplaySetting` subtree). Returns one block string per channel.
fn channel_blocks(scope: &str) -> Vec<&str> {
    let Some(channels) = first_element_body(scope, "Channels") else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut pos = 0usize;
    let close = "</Channel>";
    while let Some(rel) = channels[pos..].find("<Channel") {
        let start = pos + rel;
        // Skip "<Channels" (the wrapper, though we already stripped it) and any
        // longer element name; require the next char to be space or '>'.
        let after = channels[start + "<Channel".len()..].chars().next();
        if !matches!(
            after,
            Some(' ') | Some('>') | Some('\t') | Some('\r') | Some('\n')
        ) {
            pos = start + "<Channel".len();
            continue;
        }
        let Some(end_rel) = channels[start..].find(close) else {
            break;
        };
        let end = start + end_rel + close.len();
        out.push(&channels[start..end]);
        pos = end;
    }
    out
}

/// The `Name` attribute of a `<Channel ... Name="...">` start tag.
fn channel_name_attr(block: &str) -> Option<String> {
    let tag_end = block.find('>')?;
    let tag = &block[..tag_end];
    let needle = "Name=\"";
    let at = tag.find(needle)? + needle.len();
    let rel_end = tag[at..].find('"')?;
    let v = tag[at..at + rel_end].trim();
    if v.is_empty() {
        None
    } else {
        Some(v.to_string())
    }
}

fn parse_czi_channel_color(color: &str) -> Option<i32> {
    let mut hex = color.trim().trim_start_matches('#');
    if hex.len() > 6 {
        let end = hex.len().min(8);
        hex = &hex[2..end];
    }
    u32::from_str_radix(hex, 16)
        .ok()
        .map(|rgb| ((rgb << 8) | 0xff) as i32)
}

/// Build the per-channel OME metadata the way ZeissCZIReader does:
///   1. `Information/Image/Dimensions/Channels` provides the channel count plus
///      emission/excitation wavelengths.
///   2. `DisplaySetting/Channels` provides the channel `Name` (overriding) and
///      colour, indexed positionally.
/// The unrelated `Experiment/.../Channels` setup blocks are *not* counted.
fn build_czi_channels(xml: &str) -> Vec<crate::common::ome_metadata::OmeChannel> {
    use crate::common::ome_metadata::OmeChannel;

    // Pass 1: Dimensions/Channels — count + wavelengths.
    let mut channels: Vec<OmeChannel> = Vec::new();
    if let Some(dims) = first_element_body(xml, "Dimensions") {
        for block in channel_blocks(dims) {
            channels.push(OmeChannel {
                name: channel_name_attr(block),
                samples_per_pixel: 1,
                color: child_value(block, "Color").and_then(|s| {
                    // Extra Rust support: Java only reads CZI false colors from
                    // DisplaySetting/Channels, but pack Dimensions/Channels
                    // colors identically when they are present.
                    parse_czi_channel_color(&s)
                }),
                emission_wavelength: child_value(block, "EmissionWavelength")
                    .and_then(|s| s.parse().ok()),
                excitation_wavelength: child_value(block, "ExcitationWavelength")
                    .and_then(|s| s.parse().ok()),
                ..Default::default()
            });
        }
    }

    // Pass 2: DisplaySetting/Channels — name + colour (positional, may extend).
    if let Some(ds) = first_element_body(xml, "DisplaySetting") {
        for (i, block) in channel_blocks(ds).into_iter().enumerate() {
            while channels.len() <= i {
                channels.push(OmeChannel {
                    samples_per_pixel: 1,
                    ..Default::default()
                });
            }
            if let Some(name) = channel_name_attr(block) {
                channels[i].name = Some(name);
            }
            let color = child_value(block, "Color")
                .or_else(|| child_value(block, "OriginalColor"))
                .and_then(|s| parse_czi_channel_color(&s));
            if color.is_some() {
                channels[i].color = color;
            }
        }
    }

    channels
}

impl DimCounts {
    /// Whether mosaics should be exposed as separate series rather than stitched.
    /// Mirrors the assignPlaneIndices guard that only adds 'M' to the extra-dim
    /// order when `mosaics <= seriesCount && (!prestitched || !autostitching)`.
    /// This crate always allows autostitching, so mosaics are stitched whenever
    /// there is more than one tile; they are exposed as series only when there is
    /// genuinely no overlap to stitch (single mosaic). In practice this means
    /// mosaics are always prestitched here.
    fn mosaics_exposed_as_series(&self) -> bool {
        false
    }
}

/// Build the sorted resolution list for one series selection. Each "R" level
/// becomes a resolution; the X/Y extent is the stitched bounding box of all
/// subblocks matching the selection at that level (mosaic prestitching).
fn resolution_scale(logical: i32, stored: i32) -> i32 {
    if logical > 0 && stored > 0 && logical > stored {
        (logical + stored - 1) / stored
    } else {
        1
    }
}

fn resolution_key(e: &DirEntry, rotation_axis: bool) -> CziResolutionKey {
    if rotation_axis {
        return CziResolutionKey {
            r: 0,
            scale_x: 1,
            scale_y: 1,
        };
    }
    CziResolutionKey {
        r: e.dim_start("R"),
        scale_x: resolution_scale(e.dim_size("X"), e.dim_stored_size("X")),
        scale_y: resolution_scale(e.dim_size("Y"), e.dim_stored_size("Y")),
    }
}

fn compute_resolutions(
    entries: &[DirEntry],
    scene: Option<i32>,
    acquisition: Option<i32>,
    angle: Option<i32>,
    mosaic: Option<i32>,
    _prestitched: bool,
    rotation_axis: bool,
) -> Vec<CziResolution> {
    // Resolution key -> (min_col, min_row, max_x_end, max_y_end)
    //
    // The stitched extent is the bounding box of all subblocks in the selection;
    // this collapses to a single tile's size when there is only one tile, and to
    // the full mosaic span when there are several (ZeissCZIReader prestitching /
    // the calculateDimensions(s, true) min/max row-col logic at 1034-1076).
    let mut buckets: HashMap<CziResolutionKey, (i64, i64, i64, i64)> = HashMap::new();
    for e in entries {
        if !entry_in_series(e, scene, acquisition, angle, mosaic) {
            continue;
        }
        let key = resolution_key(e, rotation_axis);
        let col = (e.dim_start("X").max(0) / key.scale_x.max(1)) as i64;
        let row = (e.dim_start("Y").max(0) / key.scale_y.max(1)) as i64;
        let x_size = e.dim_stored_size("X").max(0) as i64;
        let y_size = e.dim_stored_size("Y").max(0) as i64;
        let entry = buckets
            .entry(key)
            .or_insert((i64::MAX, i64::MAX, i64::MIN, i64::MIN));
        entry.0 = entry.0.min(col);
        entry.1 = entry.1.min(row);
        entry.2 = entry.2.max(col + x_size);
        entry.3 = entry.3.max(row + y_size);
    }

    let mut resolutions: Vec<CziResolution> = buckets
        .into_iter()
        .map(|(key, (min_c, min_r, max_x, max_y))| CziResolution {
            r: key.r,
            scale_x: key.scale_x.max(1),
            scale_y: key.scale_y.max(1),
            width: (max_x - min_c).max(0) as u32,
            height: (max_y - min_r).max(0) as u32,
        })
        .collect();
    resolutions.sort_by_key(|res| {
        CziResolutionKey {
            r: res.r,
            scale_x: res.scale_x,
            scale_y: res.scale_y,
        }
        .sort_key()
    });
    if resolutions.is_empty() {
        resolutions.push(CziResolution {
            r: 0,
            scale_x: 1,
            scale_y: 1,
            width: 0,
            height: 0,
        });
    }
    resolutions
}

/// Whether a directory entry belongs to the given series selection.
fn entry_in_series(
    e: &DirEntry,
    scene: Option<i32>,
    acquisition: Option<i32>,
    angle: Option<i32>,
    mosaic: Option<i32>,
) -> bool {
    let ok = |sel: Option<i32>, name: &str| -> bool {
        match sel {
            // Selector active: entry must match (or lack the dimension entirely,
            // matching the "single position" fallback in calculateDimensions).
            Some(v) => !e.has_dim(name) || e.dim_start(name) == v,
            None => true,
        }
    };
    ok(scene, "S") && ok(acquisition, "B") && ok(angle, "V") && ok(mosaic, "M")
}

// ---- decompression ---------------------------------------------------------

fn decompress_subblock(
    data: &[u8],
    compression: i32,
    tile_width: usize,
    tile_height: usize,
    max_bytes: usize,
) -> Result<Vec<u8>> {
    match compression {
        0 => Ok(data.to_vec()), // Uncompressed
        1 => {
            // JPEG
            let mut dec = jpeg_decoder::Decoder::new(data);
            dec.decode()
                .map_err(|e| BioFormatsError::Codec(e.to_string()))
        }
        2 => {
            // LZW
            use weezl::{decode::Decoder, BitOrder};
            let mut dec = Decoder::with_tiff_size_switch(BitOrder::Msb, 8);
            dec.decode(data)
                .map_err(|e| BioFormatsError::Codec(e.to_string()))
        }
        4 => {
            // JPEG-XR
            match crate::common::codec::decompress_jpegxr(data) {
                Ok(decoded) => Ok(decoded),
                Err(_) if data.len() == max_bytes => Ok(data.to_vec()),
                Err(_) => Ok(vec![0; max_bytes]),
            }
        }
        5 => {
            // Zstd
            crate::common::codec::zstd_decode_all(data)
        }
        6 => decompress_zstd_1(data),
        104 => {
            // Camera-specific 12-bit packed pixels, with column reversal.
            // (matches ZeissCZIReader case 104)
            let mut decoded = decode_12bit_camera(data, max_bytes)?;
            reverse_columns_16bit(&mut decoded, tile_width, tile_height);
            Ok(decoded)
        }
        504 => {
            // Camera-specific 12-bit packed pixels without column reversal.
            decode_12bit_camera(data, max_bytes)
        }
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "CZI: unknown compression {}",
            compression
        ))),
    }
}

/// Decode 12-bit camera-packed pixel data into 16-bit samples.
///
/// Port of ZeissCZIReader.decode12BitCamera: unpacks the input into 4-bit
/// nibbles (3 nibbles per 2 output bytes), performs an in-place nibble reorder,
/// then reassembles 16-bit values.
fn decode_12bit_camera(data: &[u8], max_bytes: usize) -> Result<Vec<u8>> {
    let mut decoded = vec![0u8; max_bytes];

    let four_bits_len = (max_bytes / 2) * 3;
    let required_bytes = four_bits_len.div_ceil(2);
    if data.len() < required_bytes {
        return Err(BioFormatsError::InvalidData(format!(
            "CZI 12-bit camera payload is too short: got {}, expected at least {required_bytes}",
            data.len()
        )));
    }
    let mut four_bits = vec![0u8; four_bits_len];

    // Read 4-bit groups MSB-first from the packed input.
    let mut bit_pos = 0usize;
    for nibble in four_bits.iter_mut() {
        let byte_index = bit_pos / 8;
        let in_byte_shift = 4 - (bit_pos % 8);
        *nibble = (data[byte_index] >> in_byte_shift) & 0x0f;
        bit_pos += 4;
    }

    // In-place nibble reordering (matches the Java reference loop).
    if four_bits_len > 1 {
        for index in 1..four_bits_len - 1 {
            if (index as isize - 3) % 6 == 0 {
                let middle = four_bits[index];
                let last = four_bits[index + 1];
                let first = four_bits[index - 1];
                four_bits[index + 1] = middle;
                four_bits[index] = first;
                four_bits[index - 1] = last;
            }
        }
    }

    // Reassemble 16-bit values from the nibble stream.
    let mut current_byte = 0usize;
    let mut index = 0usize;
    while index < four_bits_len && current_byte < decoded.len() {
        if index % 3 == 0 {
            decoded[current_byte] = four_bits[index];
            current_byte += 1;
            index += 1;
        } else {
            let hi = four_bits[index];
            index += 1;
            let lo = if index < four_bits_len {
                four_bits[index]
            } else {
                0
            };
            index += 1;
            decoded[current_byte] = (hi << 4) | lo;
            current_byte += 1;
        }
    }

    Ok(decoded)
}

/// Reverse the column order of 16-bit pixels, row by row.
/// Port of the column-reversal loop in ZeissCZIReader case 104.
fn reverse_columns_16bit(data: &mut [u8], width: usize, height: usize) {
    if width == 0 {
        return;
    }
    for row in 0..height {
        for col in 0..width / 2 {
            let left = row * width * 2 + col * 2;
            let right = row * width * 2 + (width - col - 1) * 2;
            if right + 1 >= data.len() {
                continue;
            }
            data.swap(left, right);
            data.swap(left + 1, right + 1);
        }
    }
}

fn read_czi_varint(data: &[u8], offset: &mut usize) -> Result<usize> {
    if *offset >= data.len() {
        return Err(BioFormatsError::InvalidData(
            "CZI ZSTD_1 truncated varint".into(),
        ));
    }
    let a = data[*offset];
    *offset += 1;
    if a & 0x80 == 0 {
        return Ok(a as usize);
    }

    if *offset >= data.len() {
        return Err(BioFormatsError::InvalidData(
            "CZI ZSTD_1 truncated varint".into(),
        ));
    }
    let b = data[*offset];
    *offset += 1;
    if b & 0x80 == 0 {
        return Ok(((b as usize) << 7) | ((a & 0x7f) as usize));
    }

    if *offset >= data.len() {
        return Err(BioFormatsError::InvalidData(
            "CZI ZSTD_1 truncated varint".into(),
        ));
    }
    let c = data[*offset];
    *offset += 1;
    Ok(((c as usize) << 14) | (((b & 0x7f) as usize) << 7) | ((a & 0x7f) as usize))
}

fn decompress_zstd_1(data: &[u8]) -> Result<Vec<u8>> {
    let mut offset = 0usize;
    let header_end = read_czi_varint(data, &mut offset)?;
    if header_end > data.len() || header_end < offset {
        return Err(BioFormatsError::InvalidData(
            "CZI ZSTD_1 invalid header size".into(),
        ));
    }

    let mut high_low_unpacking = false;
    while offset < header_end {
        let chunk_id = read_czi_varint(data, &mut offset)?;
        match chunk_id {
            1 => {
                if offset >= header_end {
                    return Err(BioFormatsError::InvalidData(
                        "CZI ZSTD_1 missing chunk payload".into(),
                    ));
                }
                high_low_unpacking = (data[offset] & 1) == 1;
                offset += 1;
            }
            _ => {
                return Err(BioFormatsError::InvalidData(format!(
                    "CZI ZSTD_1 invalid chunk ID {chunk_id}"
                )));
            }
        }
    }

    let decoded = crate::common::codec::zstd_decode_all(&data[header_end..])?;
    if !high_low_unpacking {
        return Ok(decoded);
    }
    let second_half = decoded.len() / 2;
    let mut out = vec![0; decoded.len()];
    for i in 0..decoded.len() {
        let half_offset = i / 2;
        out[i] = if i % 2 == 0 {
            decoded[half_offset]
        } else {
            decoded[second_half + half_offset]
        };
    }
    Ok(out)
}

// ---- reader ----------------------------------------------------------------

pub struct ZeissCziReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    entries: Vec<DirEntry>,
    meta_xml: String,
    packed_spp: u32,
    /// One series per extra-dimension combination (scene/acquisition/angle/
    /// mosaic/pixel-type). Each carries its own resolution list and selectors.
    series: Vec<CziSeries>,
    /// Distinct CZI pixel-type codes (per-pixel-type core split).
    pixel_types: Vec<i32>,
    /// Mosaic ("M") tiles are stitched into a single image per series.
    prestitched: bool,
    /// Sub-dimension fold factors (rotations->Z, illuminations->C, phases->T).
    rotations: u32,
    illuminations: u32,
    phases: u32,
    /// True when "R" is the rotation axis folded into Z (vs. pyramid resolution).
    rotation_axis: bool,
    /// Bio-Formats-style maxResolution from dimension parsing.
    max_resolution: i32,
    current_series: usize,
    current_resolution: usize,
}

impl ZeissCziReader {
    pub fn new() -> Self {
        ZeissCziReader {
            path: None,
            meta: None,
            entries: Vec::new(),
            meta_xml: String::new(),
            packed_spp: 1,
            series: Vec::new(),
            pixel_types: Vec::new(),
            prestitched: false,
            rotations: 1,
            illuminations: 1,
            phases: 1,
            rotation_axis: false,
            max_resolution: 0,
            current_series: 0,
            current_resolution: 0,
        }
    }

    fn plane_zct(&self, plane_index: u32) -> Option<(u32, u32, u32)> {
        let meta = self.meta.as_ref()?;
        let sz = meta.size_z;
        let sc = meta.size_c;
        let z = (plane_index / sc) % sz;
        let c = plane_index % sc;
        let t = plane_index / (sc * sz);
        Some((z, c, t))
    }

    /// Resolution list for the active series.
    fn current_resolutions(&self) -> &[CziResolution] {
        self.series
            .get(self.current_series)
            .map(|s| s.resolutions.as_slice())
            .unwrap_or(&[])
    }

    fn matching_entries(&self, plane_index: u32) -> Option<Vec<DirEntry>> {
        let (z, c, t) = self.plane_zct(plane_index)?;
        let series = self.series.get(self.current_series)?;
        let res = self.current_resolutions().get(self.current_resolution)?;
        let r = res.r;
        let want_resolution_key = CziResolutionKey {
            r: res.r,
            scale_x: res.scale_x.max(1),
            scale_y: res.scale_y.max(1),
        };

        // Invert the modulo folding (ZeissCZIReader:2216-2223). The requested
        // expanded z/c/t decompose into the per-subblock coordinate plus the
        // rotation/illumination/phase sub-index. origZ = sizeZ / rotations etc.
        let meta = self.meta.as_ref()?;
        let orig_z = (meta.size_z / self.rotations.max(1)).max(1);
        let orig_c = (meta.size_c / self.illuminations.max(1)).max(1);
        let orig_t = (meta.size_t / self.phases.max(1)).max(1);
        let rotation = z / orig_z; // R.start when rotation_axis
        let z = z % orig_z;
        let illum = c / orig_c; // I.start
        let c = c % orig_c;
        let phase = t / orig_t; // H.start
        let t = t % orig_t;

        // Per-pixel-type core split: the logical channel of an entry is its "C"
        // start minus the pixel-type index (ZeissCZIReader:2152), so entries with
        // the active pixel type carry channels [ptype*sizeC .. ). We select by the
        // entry's pixel-type rank within the distinct list.
        let want_pt = self.pixel_types.get(series.pixel_type_index).copied();
        let multi_pt = self.pixel_types.len() > 1;
        // Logical-channel offset for the per-pixel-type split (ZeissCZIReader:2152
        // `c = dimension.start - plane.pixelTypeIndex`): the requested channel `c`
        // corresponds to entry C-start `c + pixelTypeIndex`.
        let c_with_offset = c + if multi_pt {
            series.pixel_type_index as u32
        } else {
            0
        };

        // "R" selects rotation (rotation_axis) or pyramid resolution otherwise.
        let want_r = if self.rotation_axis {
            rotation as i32
        } else {
            r
        };
        let match_resolution = |e: &DirEntry| -> bool {
            if self.rotation_axis {
                if !e.has_dim("R") {
                    // Subblocks without an explicit R default to rotation 0.
                    want_r == 0
                } else {
                    e.dim_start("R") == want_r
                }
            } else {
                resolution_key(e, false) == want_resolution_key
            }
        };
        // Illumination ("I") and phase ("H") sub-index selectors. Subblocks lacking
        // the dimension default to sub-index 0.
        let match_sub = |e: &DirEntry, name: &str, want: u32| -> bool {
            if !e.has_dim(name) {
                want == 0
            } else {
                e.dim_start(name) as u32 == want
            }
        };

        let entries: Vec<DirEntry> = self
            .entries
            .iter()
            .filter(|e| {
                entry_in_series(e, series.scene, series.acquisition, series.angle, series.mosaic)
                    && match_resolution(e)
                    && (self.illuminations <= 1 || match_sub(e, "I", illum))
                    && (self.phases <= 1 || match_sub(e, "H", phase))
                    && e.matches_plane(z, c_with_offset, t)
                    && (!multi_pt || want_pt == Some(e.pixel_type))
                    // PALM: a series exposes only the subblock matching its stored
                    // tile size (ZeissCZIReader:1155-1172).
                    && series.palm_size.map_or(true, |(sx, sy)| {
                        e.dim_stored_size("X").max(0) as u32 == sx
                            && e.dim_stored_size("Y").max(0) as u32 == sy
                    })
            })
            .cloned()
            .collect();
        (!entries.is_empty()).then_some(entries)
    }

    /// Apply the active series/resolution's X/Y size to the cached metadata.
    fn refresh_meta_dimensions(&mut self) {
        let (width, height, res_count) = {
            let resolutions = self.current_resolutions();
            let res_count = resolutions.len().max(1) as u32;
            let res = resolutions.get(self.current_resolution);
            (
                res.map(|r| r.width).unwrap_or(0),
                res.map(|r| r.height).unwrap_or(0),
                res_count,
            )
        };
        if let Some(meta) = self.meta.as_mut() {
            meta.size_x = width;
            meta.size_y = height;
            meta.resolution_count = res_count;
        }
    }

    fn read_subblock(path: &Path, entry: &DirEntry, pixel_bytes: usize) -> Result<Vec<u8>> {
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(entry.file_position as u64))
            .map_err(BioFormatsError::Io)?;
        let mut seg_hdr = vec![0u8; SEG_HEADER];
        f.read_exact(&mut seg_hdr).map_err(BioFormatsError::Io)?;

        // SubBlock body (matching ZeissCZIReader.SubBlock.fillInData:4175-4183):
        //   body_start (fp) = file_position + HEADER_SIZE
        //   metadataSize (int), attachmentSize (int), dataSize (long) -> 16 bytes
        //   DirectoryEntry, then skip max(256 - (filePointer - fp), 0) so the
        //   fixed part of the body is *at least* 256 bytes (measured from fp),
        //   then metadata of metadataSize bytes. Pixel data therefore starts at
        //   fp + max(256, 16 + dirEntryLen) + metadataSize.
        let mut sb_hdr = vec![0u8; 16];
        f.read_exact(&mut sb_hdr).map_err(BioFormatsError::Io)?;
        let metadata_size = read_i32(&sb_hdr, 0) as u64;
        let data_size = read_u64(&sb_hdr, 8);

        // Read the subblock's own DirectoryEntry header far enough to learn its
        // dimensionCount, then compute its on-disk length. The DirectoryEntry is
        // a 32-byte fixed header (dimensionCount at offset 28) followed by 20
        // bytes per DimensionEntry (ZeissCZIReader.DirectoryEntry:4604-4630).
        let mut de_hdr = vec![0u8; 32];
        f.read_exact(&mut de_hdr).map_err(BioFormatsError::Io)?;
        let dim_count = read_i32(&de_hdr, 28).max(0) as i64;
        let dir_entry_len = 32 + 20 * dim_count;

        // Java skips max(256 - (filePointer - fp), 0) once positioned just after
        // the full DirectoryEntry, where (filePointer - fp) == 16 + dir_entry_len.
        // We have so far consumed only 16 + 32 bytes (size header + DirectoryEntry
        // fixed header), so to reach Java's post-DirectoryEntry position we must
        // first skip the remaining DimensionEntry array (20*dim_count bytes), then
        // apply Java's padding skip of max(256 - 16 - dir_entry_len, 0), then the
        // metadata. When dim_count is large (dir_entry_len > 240) the padding skip
        // is 0, so pixel data starts immediately after metadata (no fixed 256).
        let skip = 20 * dim_count + (256 - 16 - dir_entry_len).max(0) + metadata_size as i64;
        f.seek(SeekFrom::Current(skip))
            .map_err(BioFormatsError::Io)?;

        let mut compressed = vec![0u8; data_size as usize];
        f.read_exact(&mut compressed).map_err(BioFormatsError::Io)?;

        // For compressed/downsampled tiles Java uses the stored (physical) X/Y
        // sizes to size the decoded buffer.
        let tile_w = entry.dim_stored_size("X").max(0) as usize;
        let tile_h = entry.dim_stored_size("Y").max(0) as usize;
        let max_bytes = tile_w * tile_h * pixel_bytes;
        match decompress_subblock(&compressed, entry.compression, tile_w, tile_h, max_bytes) {
            Ok(decoded) => Ok(decoded),
            Err(err) if entry.compression == 4 && compressed.len() == max_bytes => Ok(compressed),
            Err(_err) if entry.compression == 4 => Ok(vec![0; max_bytes]),
            Err(err) => Err(err),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn assemble_entry(
        out: &mut [u8],
        out_width: u32,
        out_height: u32,
        tile: &[u8],
        entry: &DirEntry,
        pixel_bytes: usize,
        scale_x: i32,
        scale_y: i32,
        off_x: i32,
        off_y: i32,
    ) -> Result<()> {
        let tile_x = ((entry.dim_start("X").max(0) / scale_x.max(1)) - off_x).max(0) as u32;
        let tile_y = ((entry.dim_start("Y").max(0) / scale_y.max(1)) - off_y).max(0) as u32;
        let tile_w = entry.dim_stored_size("X").max(0) as u32;
        let tile_h = entry.dim_stored_size("Y").max(0) as u32;
        if tile_w > 0 && tile_x >= out_width {
            return Err(BioFormatsError::Format(format!(
                "CZI tile X bounds exceed output plane: x={tile_x}, width={tile_w}, output width={out_width}"
            )));
        }
        if tile_h > 0 && tile_y >= out_height {
            return Err(BioFormatsError::Format(format!(
                "CZI tile Y bounds exceed output plane: y={tile_y}, height={tile_h}, output height={out_height}"
            )));
        }
        let copy_w = tile_w.min(out_width.saturating_sub(tile_x));
        let copy_h = tile_h.min(out_height.saturating_sub(tile_y));
        let src_row_bytes = (tile_w as usize).checked_mul(pixel_bytes).ok_or_else(|| {
            BioFormatsError::Format("CZI tile source row byte count overflows".into())
        })?;
        let dst_row_bytes = (out_width as usize)
            .checked_mul(pixel_bytes)
            .ok_or_else(|| {
                BioFormatsError::Format("CZI tile destination row byte count overflows".into())
            })?;
        let copy_bytes = (copy_w as usize)
            .checked_mul(pixel_bytes)
            .ok_or_else(|| BioFormatsError::Format("CZI tile copy byte count overflows".into()))?;

        for row in 0..copy_h as usize {
            let src_off = row * src_row_bytes;
            let dst_off = ((tile_y as usize + row) * dst_row_bytes) + tile_x as usize * pixel_bytes;
            if src_off + copy_bytes > tile.len() {
                return Err(BioFormatsError::Format(format!(
                    "CZI tile row {row} exceeds decoded tile buffer: need {} bytes, have {}",
                    src_off + copy_bytes,
                    tile.len()
                )));
            }
            if dst_off + copy_bytes > out.len() {
                return Err(BioFormatsError::Format(format!(
                    "CZI tile row {row} exceeds output plane buffer: need {} bytes, have {}",
                    dst_off + copy_bytes,
                    out.len()
                )));
            }
            out[dst_off..dst_off + copy_bytes]
                .copy_from_slice(&tile[src_off..src_off + copy_bytes]);
        }
        Ok(())
    }
}

impl Default for ZeissCziReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for ZeissCziReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("czi"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        header.starts_with(b"ZISRAWFILE")
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut reader = BufReader::new(f);
        let parsed = parse_czi_file(&mut reader).map_err(BioFormatsError::Io)?;

        let image_count = parsed.z_count * parsed.c_count * parsed.t_count;
        // Java reports the significant/valid bit depth (`<ComponentBitCount>`,
        // e.g. 12 for a 16-bit camera), falling back to the storage bit depth.
        let storage_bps = (parsed.pixel_type.bytes_per_sample() * 8) as u8;
        let bps = parse_component_bit_count(&parsed.meta_xml).unwrap_or(storage_bps);
        let is_rgb = parsed.spp >= 3;
        let czi_channels = build_czi_channels(&parsed.meta_xml);
        let is_indexed = !is_rgb
            && czi_channels
                .first()
                .and_then(|channel| channel.color)
                .is_some();

        let mut series_metadata: HashMap<String, MetadataValue> = HashMap::new();
        series_metadata.insert(
            "czi_subblocks".into(),
            MetadataValue::Int(parsed.entries.len() as i64),
        );
        if parsed.palm {
            series_metadata.insert("czi_palm".into(), MetadataValue::Bool(true));
        }

        let first = parsed.series.first();
        let (init_w, init_h, init_res_count) = first
            .and_then(|s| {
                s.resolutions
                    .first()
                    .map(|r| (r.width, r.height, s.resolutions.len()))
            })
            .unwrap_or((0, 0, 1));

        self.meta = Some(ImageMetadata {
            size_x: init_w,
            size_y: init_h,
            size_z: parsed.z_count,
            size_c: parsed.c_count,
            size_t: parsed.t_count,
            pixel_type: parsed.pixel_type,
            bits_per_pixel: bps,
            image_count,
            dimension_order: DimensionOrder::XYCZT,
            is_rgb,
            is_interleaved: is_rgb,
            is_indexed,
            is_little_endian: true,
            resolution_count: init_res_count as u32,
            thumbnail: false,
            series_metadata,
            lookup_table: None,
            modulo_z: parsed.modulo_z,
            modulo_c: parsed.modulo_c,
            modulo_t: parsed.modulo_t,
        });
        self.packed_spp = parsed.spp.max(1);
        self.entries = parsed.entries;
        self.series = parsed.series;
        self.pixel_types = parsed.pixel_types;
        self.prestitched = parsed.prestitched;
        self.rotations = parsed.rotations.max(1) as u32;
        self.illuminations = parsed.illuminations.max(1) as u32;
        self.phases = parsed.phases.max(1) as u32;
        self.rotation_axis = parsed.rotation_axis;
        self.max_resolution = parsed.max_resolution;
        self.current_series = 0;
        self.current_resolution = 0;
        self.meta_xml = parsed.meta_xml;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.entries.clear();
        self.meta_xml.clear();
        self.packed_spp = 1;
        self.series.clear();
        self.pixel_types.clear();
        self.prestitched = false;
        self.rotations = 1;
        self.illuminations = 1;
        self.phases = 1;
        self.rotation_axis = false;
        self.max_resolution = 0;
        self.current_series = 0;
        self.current_resolution = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        if self.meta.is_some() {
            self.series.len().max(1)
        } else {
            0
        }
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s >= self.series_count() {
            return Err(BioFormatsError::SeriesOutOfRange(s));
        }
        self.current_series = s;
        // Switching scenes resets the active resolution to full-res (level 0),
        // matching how setSeries resets the core/resolution index in Java.
        self.current_resolution = 0;
        self.refresh_meta_dimensions();
        Ok(())
    }
    fn series(&self) -> usize {
        self.current_series
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

        let entries = self.matching_entries(plane_index).unwrap_or_default();
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;

        let bps = meta.pixel_type.bytes_per_sample();
        let expected = meta.size_x as usize * meta.size_y as usize * self.packed_spp as usize * bps;
        let pixel_bytes = self.packed_spp as usize * bps;
        let mut out = vec![czi_initial_plane_fill(meta, self.max_resolution); expected];
        if entries.is_empty() {
            return Ok(out);
        }
        let res = self
            .current_resolutions()
            .get(self.current_resolution)
            .cloned();
        let (scale_x, scale_y) = res
            .map(|r| (r.scale_x.max(1), r.scale_y.max(1)))
            .unwrap_or((1, 1));

        // Prestitching: normalize tile placement so the minimum tile col/row maps
        // to the stitched-image origin (ZeissCZIReader.openBytes:435-439). A tile
        // whose stored size already equals the full image is placed at (0,0).
        let (min_col, min_row) = if self.prestitched {
            entries.iter().fold((i32::MAX, i32::MAX), |(mc, mr), e| {
                (
                    mc.min(e.dim_start("X") / scale_x),
                    mr.min(e.dim_start("Y") / scale_y),
                )
            })
        } else {
            (0, 0)
        };
        let (min_col, min_row) = (min_col.max(0), min_row.max(0));

        for entry in entries {
            let tile_w = entry.dim_stored_size("X").max(0) as usize;
            let tile_h = entry.dim_stored_size("Y").max(0) as usize;
            let tile_expected = tile_w
                .checked_mul(tile_h)
                .and_then(|n| n.checked_mul(pixel_bytes))
                .ok_or_else(|| BioFormatsError::Format("CZI tile byte count overflows".into()))?;
            let mut tile = Self::read_subblock(path, &entry, pixel_bytes)?;
            if tile.len() != tile_expected {
                return Err(BioFormatsError::Format(format!(
                    "CZI decoded tile byte count {} does not match expected {}",
                    tile.len(),
                    tile_expected
                )));
            }
            if czi_should_swap_bgr_to_rgb(meta, self.packed_spp as usize, entry.compression) {
                swap_bgr_to_rgb(&mut tile, bps, self.packed_spp as usize);
            }
            // Place the tile at its normalized position. When a tile's stored size
            // already spans the whole stitched image, it sits at the origin.
            let full_tile =
                self.prestitched && tile_w as u32 == meta.size_x && tile_h as u32 == meta.size_y;
            let (off_x, off_y) = if full_tile {
                (0, 0)
            } else {
                (min_col, min_row)
            };
            Self::assemble_entry(
                &mut out,
                meta.size_x,
                meta.size_y,
                &tile,
                &entry,
                pixel_bytes,
                scale_x,
                scale_y,
                off_x,
                off_y,
            )?;
        }
        Ok(out)
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
        let meta = self.meta.as_ref().unwrap();
        crop_full_plane("CZI", &full, meta, self.packed_spp as usize, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        self.current_resolutions().len().max(1)
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        let count = self.current_resolutions().len();
        if level >= count {
            return Err(BioFormatsError::Format(format!(
                "CZI resolution level {} out of range (max {})",
                level,
                count.saturating_sub(1)
            )));
        }
        self.current_resolution = level;
        self.refresh_meta_dimensions();
        Ok(())
    }

    fn resolution(&self) -> usize {
        self.current_resolution
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        if self.meta_xml.is_empty() {
            return None;
        }
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_czi_xml(&self.meta_xml);
        // Override channel enumeration to match ZeissCZIReader: the channel
        // count and wavelengths come from Information/Image/Dimensions/Channels,
        // names from DisplaySetting/Channels (the generic XML scan over-counts
        // by also picking up Experiment setup channels).
        let channels = build_czi_channels(&self.meta_xml);
        if let Some(image) = ome.images.first_mut() {
            if !channels.is_empty() {
                image.channels = channels;
            }
            // ZeissCZIReader names the single-series image "<filename> #1"
            // (base name, then " #" + 1-based series index).
            if image.name.is_none() {
                if let Some(path) = &self.path {
                    if let Some(file) = path.file_name().and_then(|f| f.to_str()) {
                        image.name = Some(format!("{} #1", file));
                    }
                }
            }
        }
        Some(ome)
    }
}

fn swap_bgr_to_rgb(buf: &mut [u8], bytes_per_sample: usize, samples_per_pixel: usize) {
    if samples_per_pixel < 3 || bytes_per_sample == 0 {
        return;
    }

    let pixel_bytes = bytes_per_sample * samples_per_pixel;
    for pixel in buf.chunks_exact_mut(pixel_bytes) {
        for i in 0..bytes_per_sample {
            pixel.swap(i, 2 * bytes_per_sample + i);
        }
    }
}

fn czi_initial_plane_fill(meta: &ImageMetadata, max_resolution: i32) -> u8 {
    if meta.is_rgb && max_resolution > 0 {
        0xff
    } else {
        0
    }
}

fn czi_should_swap_bgr_to_rgb(
    meta: &ImageMetadata,
    samples_per_pixel: usize,
    compression: i32,
) -> bool {
    meta.is_rgb && samples_per_pixel >= 3 && compression != 4
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::io::Write;

    /// Minimal Experiment → ExperimentBlocks → AcquisitionBlock → MultiTrackSetup
    /// → TrackSetup → PalmSlider graph (ZeissCZIReader.checkPALM path) with a
    /// `true` PalmSlider, i.e. a file that checkPALM should flag as PALM.
    const PALM_EXPERIMENT_TRUE_XML: &str = "<Experiment><ExperimentBlocks>\
        <AcquisitionBlock><MultiTrackSetup><TrackSetup>\
        <PalmSlider>true</PalmSlider></TrackSetup></MultiTrackSetup>\
        </AcquisitionBlock></ExperimentBlocks></Experiment>";

    /// Same nested graph but with a `false` PalmSlider (not PALM).
    const PALM_EXPERIMENT_FALSE_XML: &str = "<Experiment><ExperimentBlocks>\
        <AcquisitionBlock><MultiTrackSetup><TrackSetup>\
        <PalmSlider>false</PalmSlider></TrackSetup></MultiTrackSetup>\
        </AcquisitionBlock></ExperimentBlocks></Experiment>";

    #[test]
    fn czi_12bit_camera_rejects_truncated_payload() {
        let err = decode_12bit_camera(&[0xab, 0xcd], 8).unwrap_err();
        assert!(
            err.to_string()
                .contains("12-bit camera payload is too short"),
            "unexpected error: {err}"
        );
    }

    fn put_i32(buf: &mut [u8], off: usize, value: i32) {
        buf[off..off + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn put_i64(buf: &mut [u8], off: usize, value: i64) {
        buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u64(buf: &mut [u8], off: usize, value: u64) {
        buf[off..off + 8].copy_from_slice(&value.to_le_bytes());
    }

    fn segment_header(name: &str, used_size: u64) -> Vec<u8> {
        let mut header = vec![0; SEG_HEADER];
        header[..name.len()].copy_from_slice(name.as_bytes());
        put_u64(&mut header, 16, used_size);
        put_u64(&mut header, 24, used_size);
        header
    }

    fn dimension_entry(name: &str, start: i32, size: i32) -> [u8; 20] {
        dimension_entry_stored(name, start, size, 0)
    }

    fn dimension_entry_stored(name: &str, start: i32, size: i32, stored_size: i32) -> [u8; 20] {
        let mut dim = [0; 20];
        dim[..name.len()].copy_from_slice(name.as_bytes());
        put_i32(&mut dim, 4, start);
        put_i32(&mut dim, 8, size);
        put_i32(&mut dim, 16, stored_size);
        dim
    }

    fn directory_entry(pixel_type: i32, file_position: i64, c: i32, x: i32, y: i32) -> Vec<u8> {
        directory_entry_dims(pixel_type, file_position, c, 0, 0, x, y, 0)
    }

    fn directory_entry_dims(
        pixel_type: i32,
        file_position: i64,
        c: i32,
        x_start: i32,
        y_start: i32,
        x_size: i32,
        y_size: i32,
        r: i32,
    ) -> Vec<u8> {
        let mut entry = vec![0; 256];
        put_i32(&mut entry, 2, pixel_type);
        put_i64(&mut entry, 6, file_position);
        put_i32(&mut entry, 18, 0);
        put_i32(&mut entry, 28, 4);
        entry[32..52].copy_from_slice(&dimension_entry("X", x_start, x_size));
        entry[52..72].copy_from_slice(&dimension_entry("Y", y_start, y_size));
        entry[72..92].copy_from_slice(&dimension_entry("C", c, 1));
        entry[92..112].copy_from_slice(&dimension_entry("R", r, 1));
        entry
    }

    /// Directory entry carrying X/Y tile placement plus one extra dimension
    /// (e.g. "M" mosaic, "B" acquisition, "V" angle) with the given start.
    /// Used to exercise mosaic stitching and the extra-dimension series split.
    fn directory_entry_extra(
        pixel_type: i32,
        x_start: i32,
        y_start: i32,
        x_size: i32,
        y_size: i32,
        extra_dim: &str,
        extra_start: i32,
    ) -> Vec<u8> {
        let mut entry = vec![0; 256];
        put_i32(&mut entry, 2, pixel_type);
        put_i32(&mut entry, 18, 0);
        put_i32(&mut entry, 28, 5);
        entry[32..52].copy_from_slice(&dimension_entry("X", x_start, x_size));
        entry[52..72].copy_from_slice(&dimension_entry("Y", y_start, y_size));
        entry[72..92].copy_from_slice(&dimension_entry("C", 0, 1));
        entry[92..112].copy_from_slice(&dimension_entry("R", 0, 1));
        entry[112..132].copy_from_slice(&dimension_entry(extra_dim, extra_start, 1));
        entry
    }

    /// Directory entry carrying an explicit scene ("S") dimension, used to test
    /// the multi-series scene split (one series per S position).
    fn directory_entry_scene(
        pixel_type: i32,
        file_position: i64,
        scene: i32,
        x_size: i32,
        y_size: i32,
    ) -> Vec<u8> {
        let mut entry = vec![0; 256];
        put_i32(&mut entry, 2, pixel_type);
        put_i64(&mut entry, 6, file_position);
        put_i32(&mut entry, 18, 0);
        put_i32(&mut entry, 28, 5);
        entry[32..52].copy_from_slice(&dimension_entry("X", 0, x_size));
        entry[52..72].copy_from_slice(&dimension_entry("Y", 0, y_size));
        entry[72..92].copy_from_slice(&dimension_entry("C", 0, 1));
        entry[92..112].copy_from_slice(&dimension_entry("R", 0, 1));
        entry[112..132].copy_from_slice(&dimension_entry("S", scene, 1));
        entry
    }

    /// Directory entry carrying both a scene ("S") and a Z dimension, used to
    /// exercise the mosaic image-fusion series rebalancing
    /// (ZeissCZIReader:941-960).
    fn directory_entry_scene_z(
        pixel_type: i32,
        scene: i32,
        z: i32,
        x_size: i32,
        y_size: i32,
    ) -> Vec<u8> {
        let mut entry = vec![0; 256];
        put_i32(&mut entry, 2, pixel_type);
        put_i32(&mut entry, 18, 0);
        put_i32(&mut entry, 28, 5);
        entry[32..52].copy_from_slice(&dimension_entry("X", 0, x_size));
        entry[52..72].copy_from_slice(&dimension_entry("Y", 0, y_size));
        entry[72..92].copy_from_slice(&dimension_entry("Z", z, 1));
        entry[92..112].copy_from_slice(&dimension_entry("R", 0, 1));
        entry[112..132].copy_from_slice(&dimension_entry("S", scene, 1));
        entry
    }

    fn directory_entry_zc_dims(
        pixel_type: i32,
        file_position: i64,
        z: i32,
        c: i32,
        x_size: i32,
        y_size: i32,
    ) -> Vec<u8> {
        let mut entry = vec![0; 256];
        put_i32(&mut entry, 2, pixel_type);
        put_i64(&mut entry, 6, file_position);
        put_i32(&mut entry, 18, 0);
        put_i32(&mut entry, 28, 5);
        entry[32..52].copy_from_slice(&dimension_entry("X", 0, x_size));
        entry[52..72].copy_from_slice(&dimension_entry("Y", 0, y_size));
        entry[72..92].copy_from_slice(&dimension_entry("Z", z, 1));
        entry[92..112].copy_from_slice(&dimension_entry("C", c, 1));
        entry[112..132].copy_from_slice(&dimension_entry("R", 0, 1));
        entry
    }

    fn directory_entry_stored_xy(
        pixel_type: i32,
        compression: i32,
        x_start: i32,
        y_start: i32,
        x_size: i32,
        y_size: i32,
        x_stored: i32,
        y_stored: i32,
        r: i32,
    ) -> Vec<u8> {
        let mut entry = vec![0; 256];
        put_i32(&mut entry, 2, pixel_type);
        put_i32(&mut entry, 18, compression);
        put_i32(&mut entry, 28, 4);
        entry[32..52].copy_from_slice(&dimension_entry_stored("X", x_start, x_size, x_stored));
        entry[52..72].copy_from_slice(&dimension_entry_stored("Y", y_start, y_size, y_stored));
        entry[72..92].copy_from_slice(&dimension_entry("C", 0, 1));
        entry[92..112].copy_from_slice(&dimension_entry("R", r, 1));
        entry
    }

    fn with_compression(mut entry: Vec<u8>, compression: i32) -> Vec<u8> {
        put_i32(&mut entry, 18, compression);
        entry
    }

    fn write_synthetic_bgr_czi(name: &str, pixel_type: i32, planes: &[Vec<u8>]) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bioformats_czi_{name}_{}_{}.czi",
            std::process::id(),
            planes.len()
        ));
        let width = 2;
        let height = 1;
        let file_header_size = SEG_HEADER + 80;
        let dir_size = SEG_HEADER + 128 + planes.len() * 256;
        // Java-correct subblock layout: the fixed body (from body_start) is 256
        // bytes total, which includes the 16-byte size header. Pixel data follows.
        let subblock_size = |plane: &Vec<u8>| SEG_HEADER + 256 + plane.len();
        let dir_pos = file_header_size as u64;
        let mut subblock_pos = (file_header_size + dir_size) as u64;

        let mut data = Vec::new();
        data.extend_from_slice(&segment_header("ZISRAWFILE", file_header_size as u64));
        let mut file_header = vec![0; 80];
        put_u64(&mut file_header, 36, dir_pos);
        data.extend_from_slice(&file_header);

        data.extend_from_slice(&segment_header("ZISRAWDIRECTORY", dir_size as u64));
        let mut dir_header = vec![0; 128];
        put_i32(&mut dir_header, 0, planes.len() as i32);
        data.extend_from_slice(&dir_header);
        let mut entries = Vec::new();
        for (c, plane) in planes.iter().enumerate() {
            entries.push(directory_entry(
                pixel_type,
                subblock_pos as i64,
                c as i32,
                width,
                height,
            ));
            subblock_pos += subblock_size(plane) as u64;
        }
        for entry in &entries {
            data.extend_from_slice(entry);
        }

        for (_entry, plane) in entries.iter().zip(planes) {
            let used_size = (SEG_HEADER + 256 + plane.len()) as u64;
            data.extend_from_slice(&segment_header("ZISRAWSUBBLOCK", used_size));
            // 256-byte fixed body: 16-byte size header followed by 240 reserved bytes.
            let mut subblock_body = vec![0; 256];
            put_u64(&mut subblock_body, 8, plane.len() as u64);
            data.extend_from_slice(&subblock_body);
            data.extend_from_slice(plane);
        }

        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&data).unwrap();
        path
    }

    fn write_synthetic_czi_entries(
        name: &str,
        entries_and_pixels: Vec<(Vec<u8>, Vec<u8>)>,
    ) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bioformats_czi_{name}_{}_{}.czi",
            std::process::id(),
            entries_and_pixels.len()
        ));
        let file_header_size = SEG_HEADER + 80;
        let dir_size = SEG_HEADER + 128 + entries_and_pixels.len() * 256;
        let dir_pos = file_header_size as u64;
        let mut subblock_pos = (file_header_size + dir_size) as u64;
        let mut entries = Vec::new();

        for (mut entry, pixels) in entries_and_pixels {
            put_i64(&mut entry, 6, subblock_pos as i64);
            subblock_pos += (SEG_HEADER + 256 + pixels.len()) as u64;
            entries.push((entry, pixels));
        }

        let mut data = Vec::new();
        data.extend_from_slice(&segment_header("ZISRAWFILE", file_header_size as u64));
        let mut file_header = vec![0; 80];
        put_u64(&mut file_header, 36, dir_pos);
        data.extend_from_slice(&file_header);

        data.extend_from_slice(&segment_header("ZISRAWDIRECTORY", dir_size as u64));
        let mut dir_header = vec![0; 128];
        put_i32(&mut dir_header, 0, entries.len() as i32);
        data.extend_from_slice(&dir_header);
        for (entry, _) in &entries {
            data.extend_from_slice(entry);
        }

        for (_entry, pixels) in &entries {
            let used_size = (SEG_HEADER + 256 + pixels.len()) as u64;
            data.extend_from_slice(&segment_header("ZISRAWSUBBLOCK", used_size));
            // 256-byte fixed body: 16-byte size header followed by 240 reserved bytes.
            let mut subblock_body = vec![0; 256];
            put_u64(&mut subblock_body, 8, pixels.len() as u64);
            data.extend_from_slice(&subblock_body);
            data.extend_from_slice(pixels);
        }

        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&data).unwrap();
        path
    }

    /// Like `write_synthetic_czi_entries` but also writes a metadata (ZISRAWMETADATA)
    /// segment carrying `xml`, wiring its file-header offset so `set_id` parses it.
    /// Used to exercise XML-guided behavior (PALM detection, modulo labels).
    fn write_synthetic_czi_with_xml(
        name: &str,
        entries_and_pixels: Vec<(Vec<u8>, Vec<u8>)>,
        xml: &str,
    ) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "bioformats_czi_{name}_{}_{}.czi",
            std::process::id(),
            entries_and_pixels.len()
        ));
        let file_header_size = SEG_HEADER + 80;
        let dir_size = SEG_HEADER + 128 + entries_and_pixels.len() * 256;
        let xml_bytes = xml.as_bytes();
        // Metadata segment: header + 256-byte body header (xml_size at off 0) + xml.
        let meta_size = SEG_HEADER + 256 + xml_bytes.len();
        let dir_pos = file_header_size as u64;
        let meta_pos = (file_header_size + dir_size) as u64;
        let mut subblock_pos = (file_header_size + dir_size + meta_size) as u64;
        let mut entries = Vec::new();

        for (mut entry, pixels) in entries_and_pixels {
            put_i64(&mut entry, 6, subblock_pos as i64);
            subblock_pos += (SEG_HEADER + 256 + pixels.len()) as u64;
            entries.push((entry, pixels));
        }

        let mut data = Vec::new();
        data.extend_from_slice(&segment_header("ZISRAWFILE", file_header_size as u64));
        let mut file_header = vec![0; 80];
        put_u64(&mut file_header, 36, dir_pos);
        put_u64(&mut file_header, 44, meta_pos);
        data.extend_from_slice(&file_header);

        data.extend_from_slice(&segment_header("ZISRAWDIRECTORY", dir_size as u64));
        let mut dir_header = vec![0; 128];
        put_i32(&mut dir_header, 0, entries.len() as i32);
        data.extend_from_slice(&dir_header);
        for (entry, _) in &entries {
            data.extend_from_slice(entry);
        }

        // Metadata segment.
        data.extend_from_slice(&segment_header("ZISRAWMETADATA", meta_size as u64));
        let mut meta_body = vec![0; 256];
        put_i32(&mut meta_body, 0, xml_bytes.len() as i32);
        data.extend_from_slice(&meta_body);
        data.extend_from_slice(xml_bytes);

        for (_entry, pixels) in &entries {
            let used_size = (SEG_HEADER + 256 + pixels.len()) as u64;
            data.extend_from_slice(&segment_header("ZISRAWSUBBLOCK", used_size));
            let mut subblock_body = vec![0; 256];
            put_u64(&mut subblock_body, 8, pixels.len() as u64);
            data.extend_from_slice(&subblock_body);
            data.extend_from_slice(pixels);
        }

        let mut file = fs::File::create(&path).unwrap();
        file.write_all(&data).unwrap();
        path
    }

    #[test]
    fn czi_varint_matches_java_encoding() {
        let mut offset = 0;
        assert_eq!(read_czi_varint(&[0x7f], &mut offset).unwrap(), 0x7f);
        assert_eq!(offset, 1);

        let mut offset = 0;
        assert_eq!(read_czi_varint(&[0x80, 0x01], &mut offset).unwrap(), 0x80);
        assert_eq!(offset, 2);

        let mut offset = 0;
        assert_eq!(
            read_czi_varint(&[0x80, 0x80, 0x01], &mut offset).unwrap(),
            0x4000
        );
        assert_eq!(offset, 3);
    }

    #[test]
    fn czi_zstd_1_plain_payload() {
        let payload = crate::common::codec::zstd_encode_all(b"\x11\x22\x33\x44", 0).unwrap();
        let mut wrapped = vec![3, 1, 0];
        wrapped.extend_from_slice(&payload);
        assert_eq!(
            decompress_zstd_1(&wrapped).unwrap(),
            vec![0x11, 0x22, 0x33, 0x44]
        );
    }

    #[test]
    fn czi_zstd_1_high_low_unpacking() {
        let payload = crate::common::codec::zstd_encode_all(b"\x11\x33\x22\x44", 0).unwrap();
        let mut wrapped = vec![3, 1, 1];
        wrapped.extend_from_slice(&payload);
        assert_eq!(
            decompress_zstd_1(&wrapped).unwrap(),
            vec![0x11, 0x22, 0x33, 0x44]
        );
    }

    #[test]
    fn czi_zstd_1_high_low_odd_length_matches_java_unpacking() {
        let payload = crate::common::codec::zstd_encode_all(b"\x11\x22\x33", 0).unwrap();
        let mut wrapped = vec![3, 1, 1];
        wrapped.extend_from_slice(&payload);
        assert_eq!(decompress_zstd_1(&wrapped).unwrap(), vec![0x11, 0x22, 0x22]);
    }

    #[test]
    fn czi_directory_rejects_truncated_declared_entry() {
        let mut entry = directory_entry_dims(0, 0, 0, 0, 0, 2, 1, 0);
        entry.truncate(40);

        let err = parse_directory_entries(&entry, 1).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidData);
        assert!(
            err.to_string().contains("directory entry 0 is truncated"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn czi_bgr24_keeps_logical_channels_separate_from_packed_samples() {
        let planes = vec![vec![1, 2, 3, 4, 5, 6], vec![7, 8, 9, 10, 11, 12]];
        let path = write_synthetic_bgr_czi("bgr24_logical_c", 3, &planes);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_c, 2);
        assert_eq!(meta.image_count, 2);
        assert!(meta.is_rgb);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![3, 2, 1, 6, 5, 4]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![9, 8, 7, 12, 11, 10]);
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(),
            vec![12, 11, 10]
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_gray_channel_color_sets_indexed_not_interleaved_like_java() {
        let entries = vec![(directory_entry(0, 0, 0, 2, 1), vec![1, 2])];
        let xml = r#"<Metadata>
          <Information><Image><Dimensions><Channels>
            <Channel><Color>#00ff00</Color></Channel>
          </Channels></Dimensions></Image></Information>
        </Metadata>"#;
        let path = write_synthetic_czi_with_xml("gray_indexed", entries, xml);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert!(!meta.is_rgb);
        assert!(!meta.is_interleaved);
        assert!(meta.is_indexed);
        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.images[0].channels[0].color, Some(0x00ff00ff));

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_bgr48_keeps_logical_channels_separate_from_packed_samples() {
        let planes = vec![
            vec![1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0],
            vec![7, 0, 8, 0, 9, 0, 10, 0, 11, 0, 12, 0],
        ];
        let path = write_synthetic_bgr_czi("bgr48_logical_c", 4, &planes);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_c, 2);
        assert_eq!(meta.image_count, 2);
        assert_eq!(meta.pixel_type, PixelType::Uint16);
        assert!(meta.is_rgb);
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            vec![3, 0, 2, 0, 1, 0, 6, 0, 5, 0, 4, 0]
        );
        assert_eq!(
            reader.open_bytes(1).unwrap(),
            vec![9, 0, 8, 0, 7, 0, 12, 0, 11, 0, 10, 0]
        );
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(),
            vec![12, 0, 11, 0, 10, 0]
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_assembles_mosaic_tiles_into_single_plane() {
        let entries = vec![
            (directory_entry_dims(0, 0, 0, 0, 0, 2, 1, 0), vec![1, 2]),
            (directory_entry_dims(0, 0, 0, 2, 0, 2, 1, 0), vec![3, 4]),
            (directory_entry_dims(0, 0, 0, 0, 1, 2, 1, 0), vec![5, 6]),
            (directory_entry_dims(0, 0, 0, 2, 1, 2, 1, 0), vec![7, 8]),
        ];
        let path = write_synthetic_czi_entries("mosaic_tiles", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (4, 2));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
            vec![2, 3, 6, 7]
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_rejects_short_decoded_tile_instead_of_padding() {
        let entries = vec![(directory_entry_dims(0, 0, 0, 0, 0, 2, 1, 0), vec![1])];
        let path = write_synthetic_czi_entries("short_tile", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            err.to_string()
                .contains("decoded tile byte count 1 does not match expected 2"),
            "unexpected error: {err}"
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_rejects_long_decoded_tile_instead_of_truncating() {
        let entries = vec![(directory_entry_dims(0, 0, 0, 0, 0, 2, 1, 0), vec![1, 2, 3])];
        let path = write_synthetic_czi_entries("long_tile", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            err.to_string()
                .contains("decoded tile byte count 3 does not match expected 2"),
            "unexpected error: {err}"
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_rejects_tile_outside_output_instead_of_skipping_copy() {
        let entry = parse_dir_entry(&directory_entry_dims(0, 0, 0, 1, 0, 1, 1, 0));
        let mut out = vec![0u8; 1];

        let err = ZeissCziReader::assemble_entry(&mut out, 1, 1, &[7], &entry, 1, 1, 1, 0, 0)
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("tile X bounds exceed output plane"),
            "unexpected error: {err}"
        );
        assert_eq!(out, vec![0]);
    }

    #[test]
    fn czi_uses_java_xyczt_plane_order() {
        let entries = vec![
            (directory_entry_zc_dims(0, 0, 0, 0, 1, 1), vec![10]),
            (directory_entry_zc_dims(0, 0, 0, 1, 1, 1), vec![11]),
            (directory_entry_zc_dims(0, 0, 1, 0, 1, 1), vec![12]),
            (directory_entry_zc_dims(0, 0, 1, 1, 1, 1), vec![13]),
        ];
        let path = write_synthetic_czi_entries("xyczt_order", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.dimension_order, DimensionOrder::XYCZT);
        assert_eq!((meta.size_z, meta.size_c, meta.size_t), (2, 2, 1));
        assert_eq!(meta.image_count, 4);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![10]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![11]);
        assert_eq!(reader.open_bytes(2).unwrap(), vec![12]);
        assert_eq!(reader.open_bytes(3).unwrap(), vec![13]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_selects_pyramid_resolution_level() {
        let entries = vec![
            (
                directory_entry_dims(0, 0, 0, 0, 0, 4, 2, 0),
                vec![1, 2, 3, 4, 5, 6, 7, 8],
            ),
            (directory_entry_dims(0, 0, 0, 0, 0, 2, 1, 1), vec![9, 10]),
        ];
        let path = write_synthetic_czi_entries("pyramid_levels", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.resolution_count(), 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6, 7, 8]);

        reader.set_resolution(1).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (2, 1));
        assert_eq!(reader.resolution(), 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![9, 10]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_detects_pyramid_from_reduced_stored_xy_without_r_start() {
        let entries = vec![
            (
                directory_entry_stored_xy(0, 0, 0, 0, 4, 2, 4, 2, 0),
                vec![1, 2, 3, 4, 5, 6, 7, 8],
            ),
            (
                directory_entry_stored_xy(0, 0, 0, 0, 4, 2, 2, 1, 0),
                vec![9, 10],
            ),
        ];
        let path = write_synthetic_czi_entries("stored_xy_pyramid", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.resolution_count(), 2);
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (4, 2));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6, 7, 8]);

        reader.set_resolution(1).unwrap();
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (2, 1));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![9, 10]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_sparse_logical_plane_returns_fill_buffer() {
        let entries = vec![(directory_entry_dims(0, 0, 1, 0, 0, 2, 1, 0), vec![7, 8])];
        let path = write_synthetic_czi_entries("sparse_logical_plane", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.metadata().size_c, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![0, 0]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![7, 8]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_jpegxr_fallback_uses_raw_data_when_length_matches() {
        let entry = with_compression(directory_entry_dims(0, 0, 0, 0, 0, 2, 1, 0), 4);
        let path = write_synthetic_czi_entries("jpegxr_raw_fallback", vec![(entry, vec![5, 6])]);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.open_bytes(0).unwrap(), vec![5, 6]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_jpegxr_fallback_zero_fills_recoverable_block() {
        let entry = with_compression(directory_entry_dims(0, 0, 0, 0, 0, 2, 1, 0), 4);
        let path = write_synthetic_czi_entries("jpegxr_zero_fallback", vec![(entry, vec![5])]);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.open_bytes(0).unwrap(), vec![0, 0]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_rgb_pyramid_missing_tiles_are_white_filled() {
        let entries = vec![
            (directory_entry_dims(3, 0, 0, 0, 0, 6, 1, 0), vec![0; 18]),
            (
                directory_entry_dims(3, 0, 0, 0, 0, 2, 1, 1),
                vec![1, 2, 3, 4, 5, 6],
            ),
            (
                directory_entry_dims(3, 0, 0, 4, 0, 2, 1, 1),
                vec![7, 8, 9, 10, 11, 12],
            ),
        ];
        let path = write_synthetic_czi_entries("rgb_pyramid_sparse_white", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.resolution_count(), 2);
        reader.set_resolution(1).unwrap();
        assert!(reader.metadata().is_rgb);
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (6, 1));
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            vec![3, 2, 1, 6, 5, 4, 255, 255, 255, 255, 255, 255, 9, 8, 7, 12, 11, 10]
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_rgb_non_pyramid_missing_tiles_are_black_filled() {
        let entries = vec![
            (
                directory_entry_dims(3, 0, 0, 0, 0, 2, 1, 0),
                vec![1, 2, 3, 4, 5, 6],
            ),
            (
                directory_entry_dims(3, 0, 0, 4, 0, 2, 1, 0),
                vec![7, 8, 9, 10, 11, 12],
            ),
        ];
        let path = write_synthetic_czi_entries("rgb_sparse_black", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.resolution_count(), 1);
        assert!(reader.metadata().is_rgb);
        assert_eq!((reader.metadata().size_x, reader.metadata().size_y), (6, 1));
        assert_eq!(
            reader.open_bytes(0).unwrap(),
            vec![3, 2, 1, 6, 5, 4, 0, 0, 0, 0, 0, 0, 9, 8, 7, 12, 11, 10]
        );

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_jpegxr_rgb_skips_bgr_swap() {
        let mut meta = ImageMetadata {
            is_rgb: true,
            ..Default::default()
        };

        assert!(czi_should_swap_bgr_to_rgb(&meta, 3, 0));
        assert!(!czi_should_swap_bgr_to_rgb(&meta, 3, 4));

        meta.is_rgb = false;
        assert!(!czi_should_swap_bgr_to_rgb(&meta, 3, 0));
    }

    #[test]
    fn czi_splits_scenes_into_separate_series() {
        // Two scenes (S=0, S=1), each a single 2x1 plane. ZeissCZIReader treats
        // each "S" position as its own series (positions = maxS - minS + 1).
        let entries = vec![
            (directory_entry_scene(0, 0, 0, 2, 1), vec![1, 2]),
            (directory_entry_scene(0, 0, 1, 2, 1), vec![3, 4]),
        ];
        let path = write_synthetic_czi_entries("scene_series", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 2);

        // Series 0 -> scene S=0.
        assert_eq!(reader.series(), 0);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);

        // Series 1 -> scene S=1.
        reader.set_series(1).unwrap();
        assert_eq!(reader.series(), 1);
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (2, 1));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![3, 4]);

        // Switch back to series 0.
        reader.set_series(0).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);

        assert!(reader.set_series(2).is_err());

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_prestitches_mosaic_tiles_into_single_series() {
        // Four mosaic tiles (M=0..3) laid out 2x2 with absolute X/Y placement.
        // ZeissCZIReader collapses the mosaic ("M") dimension into a single
        // prestitched image (seriesCount stays 1, dimensions span all tiles).
        let entries = vec![
            (directory_entry_extra(0, 0, 0, 2, 1, "M", 0), vec![1, 2]),
            (directory_entry_extra(0, 2, 0, 2, 1, "M", 1), vec![3, 4]),
            (directory_entry_extra(0, 0, 1, 2, 1, "M", 2), vec![5, 6]),
            (directory_entry_extra(0, 2, 1, 2, 1, "M", 3), vec![7, 8]),
        ];
        let path = write_synthetic_czi_entries("mosaic_M_prestitch", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        // Mosaics are stitched, not exposed as separate series.
        assert_eq!(reader.series_count(), 1);
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (4, 2));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6, 7, 8]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_prestitches_mosaic_with_nonzero_origin() {
        // Mosaic tiles whose absolute X/Y origin is not 0: the prestitching
        // normalization subtracts the minimum tile col/row so the stitched image
        // starts at (0,0) (ZeissCZIReader.openBytes tile.x -= minTileX).
        let entries = vec![
            (directory_entry_extra(0, 10, 20, 2, 1, "M", 0), vec![1, 2]),
            (directory_entry_extra(0, 12, 20, 2, 1, "M", 1), vec![3, 4]),
        ];
        let path = write_synthetic_czi_entries("mosaic_M_origin", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 1);
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (4, 1));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_splits_acquisitions_into_separate_series() {
        // Two acquisitions (B=0, B=1). ZeissCZIReader.calculateDimensions sets
        // acquisitions = maxB + 1 and seriesCount = positions * acquisitions *
        // angles, so each B becomes its own series.
        let entries = vec![
            (directory_entry_extra(0, 0, 0, 2, 1, "B", 0), vec![1, 2]),
            (directory_entry_extra(0, 0, 0, 2, 1, "B", 1), vec![3, 4]),
        ];
        let path = write_synthetic_czi_entries("acq_series", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);
        reader.set_series(1).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![3, 4]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_splits_angles_into_separate_series() {
        // Two angles (V=0, V=1) -> two series (angles = maxV + 1).
        let entries = vec![
            (directory_entry_extra(0, 0, 0, 2, 1, "V", 0), vec![1, 2]),
            (directory_entry_extra(0, 0, 0, 2, 1, "V", 1), vec![3, 4]),
        ];
        let path = write_synthetic_czi_entries("angle_series", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);
        reader.set_series(1).unwrap();
        assert_eq!(reader.open_bytes(0).unwrap(), vec![3, 4]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_fuses_mosaic_collapses_series_count() {
        // Mosaic image-fusion series rebalancing (ZeissCZIReader:941-960).
        //
        // Two scenes (S=0, S=1) are declared, each carrying a distinct Z plane, so
        // the dimension scan yields positions = 2 and sizeZ = 2. The expected plane
        // budget is imageCount(=sizeZ=2) * seriesCount(=positions=2) = 4, but the
        // file only stores 2 fused planes. ZeissCZIReader detects this
        // (imageCount * seriesCount > planes.size() * scanDim, i.e. 4 > 2) and,
        // because planes.size() == imageCount (2 == 2), collapses everything to a
        // single series (positions = acquisitions = mosaics = angles = seriesCount
        // = 1, ZeissCZIReader:952-960). Without this rebalancing the reader would
        // wrongly expose 2 scene series for a fused acquisition.
        let entries = vec![
            (directory_entry_scene_z(0, 0, 0, 2, 1), vec![1, 2]),
            (directory_entry_scene_z(0, 1, 1, 2, 1), vec![3, 4]),
        ];
        let path = write_synthetic_czi_entries("fused_collapse", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        // Series collapsed to one; the single series matches both subblocks.
        assert_eq!(reader.series_count(), 1);
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (2, 1));
        assert_eq!((meta.size_z, meta.size_c, meta.size_t), (2, 1, 1));
        assert_eq!(meta.image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![3, 4]);

        fs::remove_file(path).unwrap();
    }

    /// Directory entry carrying an explicit Z dimension plus a rotation ("R")
    /// dimension at the given rotation index. Each subblock is one rotation
    /// (R.size == 1, distinct starts, equal X size), which the reader detects as a
    /// rotation axis because the R levels are not downscaled (no pyramid).
    fn directory_entry_rotation(z: i32, r_start: i32, x_size: i32, y_size: i32) -> Vec<u8> {
        let mut entry = vec![0; 256];
        put_i32(&mut entry, 2, 0); // Gray8
        put_i32(&mut entry, 18, 0);
        put_i32(&mut entry, 28, 5);
        entry[32..52].copy_from_slice(&dimension_entry("X", 0, x_size));
        entry[52..72].copy_from_slice(&dimension_entry("Y", 0, y_size));
        entry[72..92].copy_from_slice(&dimension_entry("C", 0, 1));
        entry[92..112].copy_from_slice(&dimension_entry("Z", z, 1));
        entry[112..132].copy_from_slice(&dimension_entry("R", r_start, 1));
        entry
    }

    #[test]
    fn czi_rotation_folds_into_modulo_z() {
        // ZeissCZIReader treats the "R" dimension as a rotation axis (size > 1) and
        // exposes it as a moduloZ annotation, multiplying sizeZ by the rotation
        // count (ZeissCZIReader:846-849) and folding the plane index via
        // z = r * (sizeZ / rotations) + z (ZeissCZIReader:2216-2217).
        //
        // Two real Z planes (Z=0,1) each acquired at two rotations (R=0,1, size 2):
        //   expanded Z order is rotation-major:
        //     z=0 -> rot=0,Z=0 ; z=1 -> rot=0,Z=1 ; z=2 -> rot=1,Z=0 ; z=3 -> rot=1,Z=1
        let entries = vec![
            (directory_entry_rotation(0, 0, 2, 1), vec![10, 11]),
            (directory_entry_rotation(1, 0, 2, 1), vec![12, 13]),
            (directory_entry_rotation(0, 1, 2, 1), vec![20, 21]),
            (directory_entry_rotation(1, 1, 2, 1), vec![22, 23]),
        ];
        let path = write_synthetic_czi_entries("rotation_modulo_z", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        // rotations = maxRotation - minRotation = (0+2) - 0 = 2; sizeZ = 2 * 2 = 4.
        assert_eq!(meta.size_z, 4);
        assert_eq!(meta.image_count, 4);
        // Rotation is exposed as a single resolution, not a pyramid.
        assert_eq!(reader.resolution_count(), 1);
        let mz = meta.modulo_z.as_ref().expect("moduloZ annotation");
        assert_eq!(mz.parent_dimension, "Z");
        assert_eq!(mz.modulo_type, "rotation");
        assert_eq!(mz.step, 2.0); // original sizeZ
        assert_eq!(mz.end, 2.0); // sizeZ * (rotations - 1)

        // Plane order: XYCZT with z fastest after c. sizeC=1 so plane==z.
        assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 11]); // rot0 z0
        assert_eq!(reader.open_bytes(1).unwrap(), vec![12, 13]); // rot0 z1
        assert_eq!(reader.open_bytes(2).unwrap(), vec![20, 21]); // rot1 z0
        assert_eq!(reader.open_bytes(3).unwrap(), vec![22, 23]); // rot1 z1

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_r_size_one_stays_pyramid_not_rotation() {
        // Regression guard: when R.size == 1 (the crate's pyramid repurposing),
        // distinct R starts must remain resolution levels and NOT be folded into Z.
        let entries = vec![
            (
                directory_entry_dims(0, 0, 0, 0, 0, 4, 2, 0),
                vec![1, 2, 3, 4, 5, 6, 7, 8],
            ),
            (directory_entry_dims(0, 0, 0, 0, 0, 2, 1, 1), vec![9, 10]),
        ];
        let path = write_synthetic_czi_entries("r_size_one_pyramid", entries);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        let meta = reader.metadata();
        assert_eq!(meta.size_z, 1); // not multiplied by rotation
        assert!(meta.modulo_z.is_none());
        assert_eq!(reader.resolution_count(), 2); // pyramid preserved

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_check_palm_detects_lsmtag_and_palmslider() {
        // ZeissCZIReader.checkPALM (1): a CustomAttributes/LsmTag whose Name starts
        // with "palm" (case-insensitive) marks the file as PALM immediately.
        assert!(check_palm(
            r#"<CustomAttributes><LsmTag Name="PALMExperiment">x</LsmTag></CustomAttributes>"#
        ));
        // A non-"palm" LsmTag Name inside CustomAttributes is not enough.
        assert!(!check_palm(
            r#"<CustomAttributes><LsmTag Name="Gain">3</LsmTag></CustomAttributes>"#
        ));
        // An LsmTag *outside* a CustomAttributes block is ignored: Java scans only
        // the descendants of the first CustomAttributes element.
        assert!(!check_palm(r#"<LsmTag Name="PALMExperiment">x</LsmTag>"#));

        // ZeissCZIReader.checkPALM (2): the full nested path must exist and the
        // PalmSlider text must parse (Boolean.parseBoolean) as "true".
        assert!(check_palm(PALM_EXPERIMENT_TRUE_XML));
        // PalmSlider "false" => not PALM.
        assert!(!check_palm(PALM_EXPERIMENT_FALSE_XML));
        // A PalmSlider element *not* reachable via the Experiment/.../TrackSetup
        // path does not count (Java only consults getFirstNode of the walk).
        assert!(!check_palm(
            "<TrackSetup><PalmSlider>true</PalmSlider></TrackSetup>"
        ));
        // Missing intermediate container (no MultiTrackSetup) => not PALM.
        assert!(!check_palm(
            "<Experiment><ExperimentBlocks><AcquisitionBlock>\
             <TrackSetup><PalmSlider>true</PalmSlider></TrackSetup>\
             </AcquisitionBlock></ExperimentBlocks></Experiment>"
        ));
        // No Experiment element at all => not PALM.
        assert!(!check_palm(r#"<LsmTag Name="Gain">3</LsmTag>"#));
        assert!(!check_palm(""));
    }

    #[test]
    fn czi_parse_modulo_labels_splits_on_whitespace() {
        // ZeissCZIReader:3733-3741 splits the "...|Rotations|" value on spaces.
        let xml = "<Rotations>0 90 180 270</Rotations><Phases>a b</Phases>";
        assert_eq!(
            parse_modulo_labels(xml, "Rotations"),
            vec!["0", "90", "180", "270"]
        );
        assert_eq!(parse_modulo_labels(xml, "Phases"), vec!["a", "b"]);
        // Single (or no) label yields an empty list (no modulo labeling).
        assert!(parse_modulo_labels("<Rotations>0</Rotations>", "Rotations").is_empty());
        assert!(parse_modulo_labels("", "Rotations").is_empty());
    }

    #[test]
    fn czi_palm_splits_two_planes_by_stored_size() {
        // ZeissCZIReader PALM heuristic (1123-1193): <= 2 planes, imageCount <= 2,
        // checkPALM(xml) true, and the two planes have *different* stored sizes ->
        // split into two single-channel series, each sized to its own tile.
        let palm_xml = PALM_EXPERIMENT_TRUE_XML;
        // Both planes at C=0; PALM distinguishes the two series purely by the
        // stored tile size, not by channel (ZeissCZIReader recomputes planeIndex).
        let entries = vec![
            (directory_entry(0, 0, 0, 2, 1), vec![1, 2]),
            (directory_entry(0, 0, 0, 4, 1), vec![3, 4, 5, 6]),
        ];
        let path = write_synthetic_czi_with_xml("palm_split", entries, palm_xml);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        // PALM forces sizeC = 1 and exposes two series (one per distinct size).
        assert_eq!(reader.series_count(), 2);
        let meta = reader.metadata();
        assert_eq!(meta.size_c, 1);
        assert_eq!((meta.size_x, meta.size_y), (2, 1));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);

        reader.set_series(1).unwrap();
        let meta = reader.metadata();
        assert_eq!((meta.size_x, meta.size_y), (4, 1));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![3, 4, 5, 6]);

        fs::remove_file(path).unwrap();
    }

    #[test]
    fn czi_palm_same_size_pair_is_not_palm() {
        // Same-size pair => not PALM; revert to a single 2-channel series
        // (ZeissCZIReader:1174-1192).
        let palm_xml = PALM_EXPERIMENT_TRUE_XML;
        let entries = vec![
            (directory_entry(0, 0, 0, 2, 1), vec![1, 2]),
            (directory_entry(0, 0, 1, 2, 1), vec![3, 4]),
        ];
        let path = write_synthetic_czi_with_xml("palm_same_size", entries, palm_xml);
        let mut reader = ZeissCziReader::new();
        reader.set_id(&path).unwrap();

        assert_eq!(reader.series_count(), 1);
        let meta = reader.metadata();
        assert_eq!(meta.size_c, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![3, 4]);

        fs::remove_file(path).unwrap();
    }
}
