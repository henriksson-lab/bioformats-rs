//! HCS (High-Content Screening) format readers — group 2.
//!
//! TIFF-based HCS wrappers and extension-only placeholder readers for
//! various plate/HCS acquisition platforms.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ---------------------------------------------------------------------------
// Macro: thin TIFF wrapper (extension-only detection)
// ---------------------------------------------------------------------------
macro_rules! tiff_wrapper {
    (
        $(#[$attr:meta])*
        pub struct $name:ident;
        extensions: [$($ext:literal),+];
    ) => {
        $(#[$attr])*
        pub struct $name {
            inner: crate::tiff::TiffReader,
        }

        impl $name {
            pub fn new() -> Self {
                $name { inner: crate::tiff::TiffReader::new() }
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

            fn set_id(&mut self, path: &Path) -> Result<()> {
                self.inner.close()?;
                self.inner.set_id(path)?;
                for series in self.inner.series_list_mut() {
                    series.metadata.series_metadata.insert(
                        "hcs2.wrapper".to_string(),
                        MetadataValue::String(stringify!($name).to_string()),
                    );
                }
                Ok(())
            }

            fn close(&mut self) -> Result<()> {
                self.inner.close()
            }

            fn series_count(&self) -> usize {
                self.inner.series_count()
            }

            fn set_series(&mut self, s: usize) -> Result<()> {
                if self.inner.series_count() == 0 {
                    return Err(BioFormatsError::NotInitialized);
                }
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

            fn resolution_count(&self) -> usize {
                self.inner.resolution_count()
            }

            fn set_resolution(&mut self, level: usize) -> Result<()> {
                self.inner.set_resolution(level)
            }
        }
    };
}

// (placeholder_reader macro removed — all former stubs now have real implementations)

// ===========================================================================
// TIFF-based HCS wrappers
// ===========================================================================

// ---------------------------------------------------------------------------
// 1. MetaXpress (Molecular Devices) HCS
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// MetaXpress (Molecular Devices) HCS TIFF (`.tif`).
    pub struct MetaxpressTiffReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 2. SimplePCI / HCImage
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// SimplePCI/HCImage TIFF (`.tif`).
    pub struct SimplePciTiffReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 3. Ionpath MIBI-TOF
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Ionpath MIBI-TOF TIFF (`.tif`).
    pub struct IonpathMibiTiffReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 4. Beckman Coulter MIAS
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Beckman Coulter MIAS TIFF (`.tif`).
    pub struct MiasTiffReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 5. Trestle whole-slide
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Trestle whole-slide TIFF (`.tif`).
    pub struct TrestleReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 6. TissueFAXS
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// TissueFAXS TIFF (`.tif`).
    pub struct TissueFaxsReader;
    extensions: ["tif"];
}

// ---------------------------------------------------------------------------
// 7. Mikroscan
// ---------------------------------------------------------------------------
tiff_wrapper! {
    /// Mikroscan TIFF (`.tif`).
    pub struct MikroscanTiffReader;
    extensions: ["tif"];
}

// ===========================================================================
// HCS index-file readers (parse index, assemble plate/well/field, delegate
// pixel I/O to TiffReader)
// ===========================================================================

/// Placement of one source tile within a reconstructed (stitched/montaged)
/// plane. The tile is read from `filename` (IFD `file_index`); a sub-rectangle
/// of the source `(src_x, src_y, src_w, src_h)` is copied into the destination
/// plane at offset `(dst_x, dst_y)`.
///
/// For a plain 1:1 plane there is a single `Tile` with `dst_x = dst_y = 0`,
/// `src_x = src_y = 0` and `src_w/src_h` set to the source dimensions (or 0,
/// meaning "use the whole source plane").
#[derive(Clone)]
struct Tile {
    filename: PathBuf,
    file_index: u32,
    /// Sub-rectangle within the source TIFF plane. `src_w == 0 || src_h == 0`
    /// means "use the whole source plane" (the common 1:1 case).
    src_x: u32,
    src_y: u32,
    src_w: u32,
    src_h: u32,
    /// Destination offset within the reconstructed plane.
    dst_x: u32,
    dst_y: u32,
}

/// Reference to a single image plane: the set of source tiles that make it up.
///
/// Simple readers use exactly one whole-plane tile; CellVoyager (multi-tile
/// area stitching) and BD Pathway (montage field splitting) use cropped /
/// offset tiles.
#[derive(Clone, Default)]
struct PlaneRef {
    tiles: Vec<Tile>,
}

impl PlaneRef {
    /// A 1:1 plane backed by the whole source plane of `filename`.
    fn whole(filename: PathBuf, file_index: u32) -> Self {
        PlaneRef {
            tiles: vec![Tile {
                filename,
                file_index,
                src_x: 0,
                src_y: 0,
                src_w: 0,
                src_h: 0,
                dst_x: 0,
                dst_y: 0,
            }],
        }
    }
}

/// Compute the plane index for (z, c, t) given dimension order and sizes.
///
/// Mirrors `loci.formats.FormatTools.getIndex`.
fn get_index(
    order: DimensionOrder,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    z: u32,
    c: u32,
    t: u32,
) -> u32 {
    let (s0, s1) = match order {
        DimensionOrder::XYZCT => (size_z, size_c),
        DimensionOrder::XYZTC => (size_z, size_t),
        DimensionOrder::XYCZT => (size_c, size_z),
        DimensionOrder::XYCTZ => (size_c, size_t),
        DimensionOrder::XYTZC => (size_t, size_z),
        DimensionOrder::XYTCZ => (size_t, size_c),
    };
    // value of the three dims in the order they vary (fastest first)
    let (v0, v1, v2) = match order {
        DimensionOrder::XYZCT => (z, c, t),
        DimensionOrder::XYZTC => (z, t, c),
        DimensionOrder::XYCZT => (c, z, t),
        DimensionOrder::XYCTZ => (c, t, z),
        DimensionOrder::XYTZC => (t, z, c),
        DimensionOrder::XYTCZ => (t, c, z),
    };
    v0 + v1 * s0 + v2 * s0 * s1
}

/// Decompose `index` into (z, c, t) given dimension order and sizes.
/// Mirrors `loci.formats.FormatTools.getZCTCoords`.
fn get_zct_coords(
    order: DimensionOrder,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    index: u32,
) -> (u32, u32, u32) {
    let (s0, s1) = match order {
        DimensionOrder::XYZCT => (size_z, size_c),
        DimensionOrder::XYZTC => (size_z, size_t),
        DimensionOrder::XYCZT => (size_c, size_z),
        DimensionOrder::XYCTZ => (size_c, size_t),
        DimensionOrder::XYTZC => (size_t, size_z),
        DimensionOrder::XYTCZ => (size_t, size_c),
    };
    let s0 = s0.max(1);
    let s1 = s1.max(1);
    let v0 = index % s0;
    let v1 = (index / s0) % s1;
    let v2 = index / (s0 * s1);
    match order {
        DimensionOrder::XYZCT => (v0, v1, v2),
        DimensionOrder::XYZTC => (v0, v2, v1),
        DimensionOrder::XYCZT => (v1, v0, v2),
        DimensionOrder::XYCTZ => (v2, v0, v1),
        DimensionOrder::XYTZC => (v1, v2, v0),
        DimensionOrder::XYTCZ => (v2, v1, v0),
    }
}

/// Generic assembled-HCS reader state shared by the index-based readers.
///
/// Each parser produces a list of per-series `ImageMetadata` plus a parallel
/// list of per-series plane references. Pixel I/O is delegated to a
/// `TiffReader` opened on the referenced file.
struct HcsAssembly {
    series: Vec<ImageMetadata>,
    /// `planes[series][plane_index]` -> reference to the backing TIFF.
    planes: Vec<Vec<PlaneRef>>,
    current_series: usize,
    tiff_reader: crate::tiff::TiffReader,
    tiff_loaded_path: Option<PathBuf>,
}

impl HcsAssembly {
    fn new() -> Self {
        HcsAssembly {
            series: Vec::new(),
            planes: Vec::new(),
            current_series: 0,
            tiff_reader: crate::tiff::TiffReader::new(),
            tiff_loaded_path: None,
        }
    }

    fn meta(&self) -> Result<&ImageMetadata> {
        self.series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)
    }

    fn plane_bytes(meta: &ImageMetadata) -> usize {
        meta.size_x as usize * meta.size_y as usize * meta.pixel_type.bytes_per_sample()
    }

    /// Ensure the backing TIFF for `path` is loaded, then position it at `file_index`.
    fn ensure_loaded(&mut self, path: &Path) -> Result<()> {
        let need_load = self
            .tiff_loaded_path
            .as_deref()
            .map(|p| p != path)
            .unwrap_or(true);
        if need_load {
            let _ = self.tiff_reader.close();
            self.tiff_reader.set_id(path)?;
            self.tiff_loaded_path = Some(path.to_path_buf());
        }
        Ok(())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta()?.clone();
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let nbytes = Self::plane_bytes(&meta);
        let plane = self
            .planes
            .get(self.current_series)
            .and_then(|p| p.get(plane_index as usize))
            .cloned()
            .unwrap_or_default();

        if plane.tiles.is_empty() {
            // Missing plane: return a blank (fill 0) buffer, like Java's Arrays.fill.
            return Ok(vec![0u8; nbytes]);
        }

        let bps = meta.pixel_type.bytes_per_sample();
        let dst_w = meta.size_x as usize;
        let dst_h = meta.size_y as usize;
        let dst_row = dst_w * bps;

        // Fast path: a single whole-plane tile placed at the origin (the common
        // 1:1 case). Read the whole source plane and pad/truncate as before.
        if plane.tiles.len() == 1 {
            let t = &plane.tiles[0];
            if t.dst_x == 0
                && t.dst_y == 0
                && t.src_x == 0
                && t.src_y == 0
                && t.src_w == 0
                && t.src_h == 0
            {
                self.ensure_loaded(&t.filename)?;
                let buf = self.tiff_reader.open_bytes(t.file_index)?;
                if buf.len() == nbytes {
                    return Ok(buf);
                }
                let mut out = vec![0u8; nbytes];
                let n = buf.len().min(nbytes);
                out[..n].copy_from_slice(&buf[..n]);
                return Ok(out);
            }
        }

        // General path: composite each tile's sub-rectangle into the plane.
        let mut out = vec![0u8; nbytes];
        for t in &plane.tiles {
            self.ensure_loaded(&t.filename)?;
            // Source region: explicit crop, or the whole source plane.
            let (sx, sy, sw, sh) = if t.src_w == 0 || t.src_h == 0 {
                let sm = self.tiff_reader.metadata();
                (0, 0, sm.size_x, sm.size_y)
            } else {
                (t.src_x, t.src_y, t.src_w, t.src_h)
            };
            // Clip to the destination plane.
            let dx = t.dst_x as usize;
            let dy = t.dst_y as usize;
            if dx >= dst_w || dy >= dst_h {
                continue;
            }
            let copy_w = (sw as usize).min(dst_w - dx);
            let copy_h = (sh as usize).min(dst_h - dy);
            if copy_w == 0 || copy_h == 0 {
                continue;
            }
            let region = self.tiff_reader.open_bytes_region(
                t.file_index,
                sx,
                sy,
                copy_w as u32,
                copy_h as u32,
            )?;
            let src_row = copy_w * bps;
            let expected = src_row * copy_h;
            if region.len() < expected {
                return Err(BioFormatsError::Format(format!(
                    "HCS companion tile {} returned {} bytes for a {} byte region",
                    t.filename.display(),
                    region.len(),
                    expected
                )));
            }
            for row in 0..copy_h {
                let s = row * src_row;
                let d = (dy + row) * dst_row + dx * bps;
                if d + src_row > out.len() {
                    break;
                }
                out[d..d + src_row].copy_from_slice(&region[s..s + src_row]);
            }
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
        let meta = self.meta()?;
        crop_full_plane("BD Pathway", &full, meta, 1, x, y, w, h)
    }

    fn validate(&self, format_name: &str) -> Result<()> {
        if self.series.is_empty() {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "{format_name}: no series assembled"
            )));
        }
        if self.planes.len() != self.series.len() {
            return Err(BioFormatsError::Format(format!(
                "{format_name}: series/plane table length mismatch"
            )));
        }

        let mut saw_payload = false;
        for (series_index, meta) in self.series.iter().enumerate() {
            if meta.size_x == 0
                || meta.size_y == 0
                || meta.size_z == 0
                || meta.size_c == 0
                || meta.size_t == 0
            {
                return Err(BioFormatsError::Format(format!(
                    "{format_name}: series {series_index} has non-positive dimensions"
                )));
            }
            let expected = meta
                .size_z
                .checked_mul(meta.size_c)
                .and_then(|v| v.checked_mul(meta.size_t))
                .ok_or_else(|| {
                    BioFormatsError::Format(format!(
                        "{format_name}: series {series_index} plane count overflows"
                    ))
                })?;
            if meta.image_count != expected {
                return Err(BioFormatsError::Format(format!(
                    "{format_name}: series {series_index} image_count {} does not match dimensions {expected}",
                    meta.image_count
                )));
            }
            let planes = self.planes.get(series_index).ok_or_else(|| {
                BioFormatsError::Format(format!("{format_name}: missing plane table"))
            })?;
            if planes.len() < expected as usize {
                return Err(BioFormatsError::Format(format!(
                    "{format_name}: series {series_index} has {} plane slots for {expected} planes",
                    planes.len()
                )));
            }
            for (plane_index, plane) in planes.iter().take(expected as usize).enumerate() {
                for tile in &plane.tiles {
                    saw_payload = true;
                    let mut tr = crate::tiff::TiffReader::new();
                    tr.set_id(&tile.filename).map_err(|e| {
                        BioFormatsError::Format(format!(
                            "{format_name}: companion TIFF {} could not be initialized: {e}",
                            tile.filename.display()
                        ))
                    })?;
                    let tm = tr.metadata();
                    if tm.size_x == 0 || tm.size_y == 0 || tm.image_count == 0 {
                        return Err(BioFormatsError::Format(format!(
                            "{format_name}: companion TIFF {} has invalid image metadata",
                            tile.filename.display()
                        )));
                    }
                    if tile.file_index >= tm.image_count {
                        return Err(BioFormatsError::Format(format!(
                            "{format_name}: plane {plane_index} references TIFF page {} in {} but only {} page(s) are available",
                            tile.file_index,
                            tile.filename.display(),
                            tm.image_count
                        )));
                    }
                    let src_w = if tile.src_w == 0 {
                        tm.size_x
                    } else {
                        tile.src_w
                    };
                    let src_h = if tile.src_h == 0 {
                        tm.size_y
                    } else {
                        tile.src_h
                    };
                    let src_end_x = tile.src_x.checked_add(src_w).ok_or_else(|| {
                        BioFormatsError::Format(format!(
                            "{format_name}: source tile X range overflows for {}",
                            tile.filename.display()
                        ))
                    })?;
                    let src_end_y = tile.src_y.checked_add(src_h).ok_or_else(|| {
                        BioFormatsError::Format(format!(
                            "{format_name}: source tile Y range overflows for {}",
                            tile.filename.display()
                        ))
                    })?;
                    if src_end_x > tm.size_x || src_end_y > tm.size_y {
                        return Err(BioFormatsError::Format(format!(
                            "{format_name}: source tile region {}x{} at {},{} exceeds companion TIFF {} dimensions {}x{}",
                            src_w,
                            src_h,
                            tile.src_x,
                            tile.src_y,
                            tile.filename.display(),
                            tm.size_x,
                            tm.size_y
                        )));
                    }
                    let _ = tr.close();
                }
            }
        }
        if !saw_payload {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "{format_name}: index does not reference any readable companion TIFF payload"
            )));
        }
        Ok(())
    }
}

/// Build an `ImageMetadata` for an assembled HCS series.
#[allow(clippy::too_many_arguments)]
fn make_series_meta(
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    pixel_type: PixelType,
    bits: u8,
    little_endian: bool,
    order: DimensionOrder,
    format: &str,
) -> ImageMetadata {
    let mut meta_map = HashMap::new();
    meta_map.insert(
        "format".to_string(),
        MetadataValue::String(format.to_string()),
    );
    ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: bits,
        image_count: size_z * size_c * size_t,
        dimension_order: order,
        is_rgb: false,
        is_interleaved: false,
        is_indexed: false,
        is_little_endian: little_endian,
        resolution_count: 1,
        series_metadata: meta_map,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    }
}

/// Probe a TIFF for (size_x, size_y, pixel_type, bits, little_endian).
/// Returns `None` if the file cannot be opened.
fn probe_tiff(path: &Path) -> Option<(u32, u32, PixelType, u8, bool)> {
    let mut tr = crate::tiff::TiffReader::new();
    if tr.set_id(path).is_ok() {
        let m = tr.metadata();
        let out = (
            m.size_x,
            m.size_y,
            m.pixel_type,
            m.bits_per_pixel,
            m.is_little_endian,
        );
        let _ = tr.close();
        Some(out)
    } else {
        None
    }
}

/// Macro generating the full `FormatReader` impl that delegates pixel I/O to an
/// inner `HcsAssembly`. Detection (`is_this_type_by_name`) and parsing
/// (`set_id`) bodies are supplied by each reader.
///
/// `detect` is a `fn(&Path) -> bool`; `parse` is a `fn(&Path) -> Result<HcsAssembly>`.
macro_rules! impl_assembled_reader {
    ($name:ident, detect = $detect:expr, parse = $parse:expr) => {
        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl FormatReader for $name {
            fn is_this_type_by_name(&self, path: &Path) -> bool {
                let detect: fn(&Path) -> bool = $detect;
                detect(path)
            }

            fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
                false
            }

            fn set_id(&mut self, path: &Path) -> Result<()> {
                let parse: fn(&Path) -> Result<HcsAssembly> = $parse;
                self.asm = HcsAssembly::new();
                let asm = parse(path)?;
                asm.validate(stringify!($name))?;
                self.asm = asm;
                Ok(())
            }

            fn close(&mut self) -> Result<()> {
                self.asm = HcsAssembly::new();
                Ok(())
            }

            fn series_count(&self) -> usize {
                self.asm.series.len()
            }

            fn set_series(&mut self, s: usize) -> Result<()> {
                if self.asm.series.is_empty() {
                    Err(BioFormatsError::NotInitialized)
                } else if s >= self.asm.series.len() {
                    Err(BioFormatsError::SeriesOutOfRange(s))
                } else {
                    self.asm.current_series = s;
                    Ok(())
                }
            }

            fn series(&self) -> usize {
                self.asm.current_series
            }

            fn metadata(&self) -> &ImageMetadata {
                self.asm
                    .series
                    .get(self.asm.current_series)
                    .unwrap_or(crate::common::reader::uninitialized_metadata())
            }

            fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
                self.asm.open_bytes(plane_index)
            }

            fn open_bytes_region(
                &mut self,
                plane_index: u32,
                x: u32,
                y: u32,
                w: u32,
                h: u32,
            ) -> Result<Vec<u8>> {
                self.asm.open_bytes_region(plane_index, x, y, w, h)
            }

            fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
                let meta = self.asm.meta()?;
                let tw = meta.size_x.min(256);
                let th = meta.size_y.min(256);
                let tx = (meta.size_x - tw) / 2;
                let ty = (meta.size_y - th) / 2;
                self.asm.open_bytes_region(plane_index, tx, ty, tw, th)
            }
        }
    };
}

// ---------------------------------------------------------------------------
// 8. BD Biosciences Pathway (.exp — INI-style Experiment file)
// ---------------------------------------------------------------------------

/// BD Biosciences Pathway HCS reader (`.exp`).
///
/// Ported from the upstream Java `BDReader`. Reads the INI-style
/// `Experiment.exp` plus `.plt`/`.xyz`/`.dye` companion files, scans `Well NN`
/// directories for `<channel> - nNNNNNN.tif` images, and assembles one series
/// per well × field. Montaged acquisitions store several fields packed into a
/// single TIFF, which are split out in `open_bytes`.
pub struct BdReader {
    asm: HcsAssembly,
}

impl BdReader {
    pub fn new() -> Self {
        BdReader {
            asm: HcsAssembly::new(),
        }
    }
}

impl_assembled_reader!(
    BdReader,
    detect = |path| {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("exp"))
    },
    parse = bd::parse
);

// ---------------------------------------------------------------------------
// 9. PerkinElmer Columbus (.xml — MeasurementIndex.ColumbusIDX.xml)
// ---------------------------------------------------------------------------

/// PerkinElmer Columbus HCS reader (`.xml`).
///
/// Ported from the upstream Java `ColumbusReader`. Parses the
/// `MeasurementIndex.ColumbusIDX.xml` plate index plus per-timepoint
/// `*.columbusidx.xml` image lists, and assembles one series per well × field.
pub struct ColumbusReader {
    asm: HcsAssembly,
}

impl ColumbusReader {
    pub fn new() -> Self {
        ColumbusReader {
            asm: HcsAssembly::new(),
        }
    }
}

impl_assembled_reader!(
    ColumbusReader,
    detect = |path| {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if !matches!(ext.as_deref(), Some("xml")) {
            return false;
        }
        // Columbus index files are named MeasurementIndex.ColumbusIDX.xml; also
        // accept any .xml whose content carries the Columbus magic string.
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if name == "measurementindex.columbusidx.xml" || name.ends_with("columbusidx.xml") {
            return true;
        }
        if let Ok(data) = std::fs::read(path) {
            let snippet = std::str::from_utf8(&data[..data.len().min(1024)]).unwrap_or("");
            return snippet.contains("ColumbusMeasurementIndex");
        }
        false
    },
    parse = columbus::parse
);

// ---------------------------------------------------------------------------
// 10. PerkinElmer Operetta (.xml — Index.idx.xml)
// ---------------------------------------------------------------------------

/// PerkinElmer Operetta HCS reader (`.xml`).
///
/// Ported from the upstream Java `OperettaReader`. Parses `Index.idx.xml`
/// (Harmony/Operetta/Phenix) and assembles one series per well × field with
/// per-plane Z/C/T → TIFF mapping.
pub struct OperettaReader {
    asm: HcsAssembly,
}

impl OperettaReader {
    pub fn new() -> Self {
        OperettaReader {
            asm: HcsAssembly::new(),
        }
    }
}

impl_assembled_reader!(
    OperettaReader,
    detect = |path| {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if !matches!(ext.as_deref(), Some("xml")) {
            return false;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        if matches!(
            name.as_str(),
            "index.idx.xml" | "index.ref.xml" | "index.xml"
        ) {
            return true;
        }
        if let Ok(data) = std::fs::read(path) {
            let snippet = std::str::from_utf8(&data[..data.len().min(1024)]).unwrap_or("");
            return snippet.contains("Harmony") || snippet.contains("Operett");
        }
        false
    },
    parse = operetta::parse
);

// ---------------------------------------------------------------------------
// 11. Olympus ScanR (.xml — experiment_descriptor.xml)
// ---------------------------------------------------------------------------

/// Olympus ScanR HCS reader (`.xml`).
///
/// Ported from the upstream Java `ScanrReader`. Parses
/// `experiment_descriptor.xml`, derives plate/well/field/channel/Z/T geometry,
/// then matches the `data/` TIFF filenames (`...W#####...P#####...Z#####...T#####...<channel>...`)
/// into one series per well × field.
pub struct ScanrReader {
    asm: HcsAssembly,
}

impl ScanrReader {
    pub fn new() -> Self {
        ScanrReader {
            asm: HcsAssembly::new(),
        }
    }
}

impl_assembled_reader!(
    ScanrReader,
    detect = |path| {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if !matches!(ext.as_deref(), Some("xml")) {
            return false;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        name == "experiment_descriptor.xml"
    },
    parse = scanr::parse
);

// ---------------------------------------------------------------------------
// 12. Yokogawa CellVoyager (.mes, .mlf, MeasurementResult.xml)
// ---------------------------------------------------------------------------

/// Yokogawa CellVoyager HCS reader (`.mes`, `.mlf`, `MeasurementResult.xml`).
///
/// Port of the upstream Java `CellVoyagerReader`. Parses
/// `MeasurementResult.xml` for channel/well/field/timepoint geometry and
/// stitches each area's `Image/W#F###T####Z##C#.tif` field tiles on the fly,
/// pasting each tile at its computed pixel offset (see module docs).
pub struct CellVoyagerReader {
    asm: HcsAssembly,
}

impl CellVoyagerReader {
    pub fn new() -> Self {
        CellVoyagerReader {
            asm: HcsAssembly::new(),
        }
    }
}

impl_assembled_reader!(
    CellVoyagerReader,
    detect = |path| {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if matches!(ext.as_deref(), Some("mes") | Some("mlf")) {
            return true;
        }
        let name = path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        name == "measurementresult.xml"
    },
    parse = cellvoyager::parse
);

// ---------------------------------------------------------------------------
// 14. GE InCell 3000 (.frm — RLE-compressed binary frame)
// ---------------------------------------------------------------------------

/// GE InCell 3000 reader (`.frm`).
///
/// Ported from the upstream Java `InCell3000Reader`. A `.frm` file is a single
/// RLE-compressed 16-bit frame with a small binary header (NOT an XML index).
/// The XDCE-based GE InCell datasets are handled by `crate::formats::incell`.
pub struct InCell3000Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixels_offset: u64,
}

impl InCell3000Reader {
    pub fn new() -> Self {
        InCell3000Reader {
            path: None,
            meta: None,
            pixels_offset: 0,
        }
    }
}

impl Default for InCell3000Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for InCell3000Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("frm") | Some("xdce"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        // `.xdce` is the InCell XML index, handled by incell::InCellReader. This
        // reader only decodes the binary `.frm` frame; for an `.xdce` it falls
        // through with the historical "no TIFF" rejection so the registry's
        // companion-less rejection contract is preserved.
        let is_xdce = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("xdce"))
            .unwrap_or(false);
        if is_xdce {
            return Err(BioFormatsError::Format(
                "GE InCell 3000: no TIFF image files found referenced in index".to_string(),
            ));
        }

        // Header layout (little-endian), per Java InCell3000Reader.initFile:
        //   int16 pixelsOffset
        //   int16 sizeX
        //   int16 nLines  -> numPlanes = nLines % 32; sizeY = (nLines - numPlanes)/numPlanes
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
        if data.len() < 6 {
            return Err(BioFormatsError::Format(
                "InCell 3000: file too small for header".to_string(),
            ));
        }
        let rd16 = |off: usize| i16::from_le_bytes([data[off], data[off + 1]]) as i64;
        let pixels_offset = rd16(0);
        let size_x = rd16(2);
        let n_lines = rd16(4);
        let num_planes = n_lines.rem_euclid(32);
        let size_y = if num_planes != 0 {
            (n_lines - num_planes) / num_planes
        } else {
            0
        };
        if size_x <= 0 || size_y <= 0 {
            return Err(BioFormatsError::Format(format!(
                "InCell 3000: invalid dimensions {size_x}x{size_y}"
            )));
        }

        let mut meta_map = HashMap::new();
        meta_map.insert(
            "format".to_string(),
            MetadataValue::String("InCell 3000".to_string()),
        );
        self.meta = Some(ImageMetadata {
            size_x: size_x as u32,
            size_y: size_y as u32,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint16,
            bits_per_pixel: 16,
            image_count: 1,
            dimension_order: DimensionOrder::XYCZT,
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
        self.pixels_offset = pixels_offset.max(0) as u64;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.pixels_offset = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s != 0 {
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
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let data = std::fs::read(path).map_err(BioFormatsError::Io)?;

        // Decompress the custom RLE stream, mirroring Java openBytes.
        // totalElements is measured in BYTES (sizeX*sizeY*2) in the Java code.
        let total_bytes = size_x
            .checked_mul(size_y)
            .and_then(|v| v.checked_mul(2))
            .ok_or_else(|| BioFormatsError::Format("InCell 3000 plane size overflows".into()))?;
        let mut out: Vec<u8> = Vec::with_capacity(total_bytes);
        let mut pos = self.pixels_offset as usize;
        let rd16 = |buf: &[u8], off: usize| -> Option<u16> {
            if off + 2 <= buf.len() {
                Some(u16::from_le_bytes([buf[off], buf[off + 1]]))
            } else {
                None
            }
        };
        while out.len() < total_bytes {
            let Some(pixel) = rd16(&data, pos) else { break };
            pos += 2;
            if pixel as i64 > 32768 {
                let count = (pixel as i64 - 32768) as usize;
                let Some(start_value) = rd16(&data, pos) else {
                    break;
                };
                pos += 2;
                let fp = pos;
                for i in 0..count {
                    let off = fp + 2 * (i / 3);
                    let Some(raw) = rd16(&data, off) else { break };
                    let int_ofs = if i % 3 != 0 { raw >> 5 } else { raw };
                    let temp_val = (start_value as i64 + (int_ofs as i64 & 31)) as u16;
                    out.extend_from_slice(&temp_val.to_le_bytes());
                    if out.len() >= total_bytes {
                        break;
                    }
                }
                // advance over the packed run: ceil(count/3) shorts
                let consumed = 2 * count.div_ceil(3);
                pos = fp + consumed;
            } else {
                out.extend_from_slice(&pixel.to_le_bytes());
            }
        }
        if out.len() < total_bytes {
            return Err(BioFormatsError::InvalidData(format!(
                "InCell 3000 decoded {} bytes, expected {total_bytes}",
                out.len()
            )));
        } else if out.len() > total_bytes {
            out.truncate(total_bytes);
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("RCPNL", &full, meta, 1, x, y, w, h)
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
// 15. RCPNL (.rcpnl — Rarecyte multi-page OME-TIFF tile scan)
// ---------------------------------------------------------------------------

/// RCPNL reader (`.rcpnl`).
///
/// Rarecyte `.rcpnl` files are multi-image (OME-)TIFFs. Upstream Bio-Formats
/// reads them via the generic OME-TIFF reader; there is no dedicated Java
/// `RcpnlReader`. We therefore delegate directly to `TiffReader`, which already
/// exposes the per-IFD series and OME metadata.
pub struct RcpnlReader {
    inner: crate::tiff::TiffReader,
}

impl RcpnlReader {
    pub fn new() -> Self {
        RcpnlReader {
            inner: crate::tiff::TiffReader::new(),
        }
    }
}

impl Default for RcpnlReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for RcpnlReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("rcpnl"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.inner.set_id(path)
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
}

// ---------------------------------------------------------------------------
// 13. Tecan plate reader (.asc — tab-separated plate data)
// ---------------------------------------------------------------------------

/// Tecan plate reader (`.asc`).
///
/// Reads a tab-separated `.asc` text file containing plate reader measurements.
/// Each row corresponds to a plate row and each column to a plate column. Values
/// are stored as `Float32` pixel data in a 2-D image.
pub struct TecanReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    pixel_data: Vec<u8>,
}

impl TecanReader {
    pub fn new() -> Self {
        TecanReader {
            path: None,
            meta: None,
            pixel_data: Vec::new(),
        }
    }
}

impl Default for TecanReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for TecanReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        matches!(ext.as_deref(), Some("asc"))
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let mut rows: Vec<Vec<f32>> = Vec::new();
        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() {
                continue;
            }
            // Tecan .asc files are tab-separated; also accept spaces.
            let mut cells: Vec<f32> = Vec::new();
            for cell in line
                .split(|c: char| c == '\t' || c == ' ')
                .filter(|s| !s.is_empty())
            {
                let value = cell.trim().parse::<f64>().map_err(|_| {
                    BioFormatsError::Format(format!("Tecan: non-numeric cell {cell:?}"))
                })?;
                cells.push(value as f32);
            }
            if !cells.is_empty() {
                rows.push(cells);
            }
        }
        if rows.is_empty() {
            return Err(BioFormatsError::Format(
                "Tecan: .asc file contains no numeric data".to_string(),
            ));
        }
        let height = rows.len() as u32;
        let width = rows[0].len();
        if rows.iter().any(|row| row.len() != width) {
            return Err(BioFormatsError::Format(
                "Tecan: .asc rows have inconsistent column counts".to_string(),
            ));
        }
        let width = width as u32;
        // Build Float32 pixel buffer (row-major).
        let mut pixel_data = Vec::with_capacity((width * height * 4) as usize);
        for row in &rows {
            for &val in row {
                pixel_data.extend_from_slice(&val.to_le_bytes());
            }
        }
        let mut series_metadata = HashMap::new();
        series_metadata.insert(
            "format".to_string(),
            MetadataValue::String("Tecan".to_string()),
        );
        series_metadata.insert("plate_rows".to_string(), MetadataValue::Int(height as i64));
        series_metadata.insert(
            "plate_columns".to_string(),
            MetadataValue::Int(width as i64),
        );

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
            series_metadata,
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
        usize::from(self.meta.is_some())
    }

    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s != 0 {
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
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        crop_full_plane("Tecan", &self.pixel_data, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn resolution_count(&self) -> usize {
        1
    }

    fn set_resolution(&mut self, level: usize) -> Result<()> {
        if level != 0 {
            Err(BioFormatsError::Format(format!(
                "resolution {} out of range",
                level
            )))
        } else {
            Ok(())
        }
    }
}

// ===========================================================================
// Shared XML parsing helpers for the index-based HCS readers
// ===========================================================================

mod xmlutil {
    use quick_xml::events::{BytesEnd, BytesStart, Event};
    use quick_xml::Reader as XmlReader;

    /// Get an attribute value by (case-sensitive) local name.
    pub fn attr(e: &BytesStart, name: &str) -> Option<String> {
        for a in e.attributes().flatten() {
            if a.key.as_ref() == name.as_bytes() {
                return Some(String::from_utf8_lossy(&a.value).to_string());
            }
        }
        None
    }

    /// Anything that exposes a qualified element name (`BytesStart`/`BytesEnd`).
    pub trait HasName {
        fn qname_bytes(&self) -> Vec<u8>;
    }
    impl HasName for BytesStart<'_> {
        fn qname_bytes(&self) -> Vec<u8> {
            self.name().as_ref().to_vec()
        }
    }
    impl HasName for BytesEnd<'_> {
        fn qname_bytes(&self) -> Vec<u8> {
            self.name().as_ref().to_vec()
        }
    }

    /// The local element name (after any namespace prefix) as an owned String.
    pub fn local_name<E: HasName>(e: &E) -> String {
        let full = e.qname_bytes();
        let local = match full.iter().position(|&b| b == b':') {
            Some(i) => &full[i + 1..],
            None => &full[..],
        };
        String::from_utf8_lossy(local).to_string()
    }

    /// Run a SAX-style callback over an XML string. (Currently unused by the
    /// readers, which run their own stateful passes; retained as a utility.)
    #[allow(dead_code)]
    pub fn walk<S, T, E>(xml: &str, mut on_start: S, mut on_text: T, mut on_end: E)
    where
        S: FnMut(&str, &BytesStart),
        T: FnMut(&str),
        E: FnMut(&str),
    {
        let mut reader = XmlReader::from_str(xml);
        reader.config_mut().trim_text(false);
        let mut buf_text = String::new();
        loop {
            match reader.read_event() {
                Ok(Event::Start(ref e)) => {
                    buf_text.clear();
                    let ln = local_name(e);
                    on_start(&ln, e);
                }
                Ok(Event::Empty(ref e)) => {
                    let ln = local_name(e);
                    on_start(&ln, e);
                    on_text("");
                    on_end(&ln);
                }
                Ok(Event::Text(ref t)) => {
                    if let Ok(s) = t.unescape() {
                        buf_text.push_str(&s);
                    }
                }
                Ok(Event::CData(ref t)) => {
                    buf_text.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
                Ok(Event::End(ref e)) => {
                    let ln = local_name(e);
                    on_text(&buf_text);
                    buf_text.clear();
                    on_end(&ln);
                }
                Ok(Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
        }
    }
}

/// Java FormatTools.getWellName(row, col): row letter(s) + 1-based column.
fn well_name(row: i32, col: i32) -> String {
    // Row 0 -> 'A', 25 -> 'Z', 26 -> 'AA', etc.
    let mut r = row;
    let mut letters = String::new();
    loop {
        let rem = (r % 26) as u8;
        letters.insert(0, (b'A' + rem) as char);
        r = r / 26 - 1;
        if r < 0 {
            break;
        }
    }
    // Java FormatTools.getWellName zero-pads the 1-based column to a minimum
    // of 2 digits (FormatTools.java:1372-1376): "A1" -> "A01".
    format!("{}{:02}", letters, col + 1)
}

// ===========================================================================
// Operetta parser (Index.idx.xml)  -- port of OperettaReader.initFile
// ===========================================================================

mod operetta {
    use super::*;
    use std::collections::HashMap as Map;

    #[derive(Clone, Default)]
    struct Plane {
        filename: Option<PathBuf>,
        row: i32,
        col: i32,
        field: i32,
        z: i32,
        t: i32,
        c: i32,
        x: u32,
        y: u32,
    }

    #[derive(Clone, Default)]
    struct Channel {
        channel_id: i32,
        x: u32,
        y: u32,
    }

    pub fn parse(path: &Path) -> Result<HcsAssembly> {
        let xml = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        // "Images" directory may need to be located; Operetta URLs are relative
        // to the directory containing Index.idx.xml.
        let images_dir = locate_images_dir(&dir);

        let mut planes: Vec<Plane> = Vec::new();
        let mut channels: Map<i32, Channel> = Map::new();
        let mut plate_rows = 0i32;
        let mut plate_cols = 0i32;

        // Parser state. A single stateful SAX pass populates `planes`/`channels`.
        let mut active_plane: Option<Plane> = None;
        let mut active_channel: Option<Channel> = None;
        let mut active_channel_id: i32 = 0;

        let mut current_name = String::new();
        let mut reader = quick_xml::Reader::from_str(&xml);
        reader.config_mut().trim_text(false);
        let mut text_buf = String::new();
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Start(ref e)) => {
                    text_buf.clear();
                    current_name = super::xmlutil::local_name(e);
                    handle_start(
                        &current_name,
                        e,
                        &mut active_plane,
                        &mut active_channel,
                        &mut active_channel_id,
                        &mut channels,
                    );
                }
                Ok(quick_xml::events::Event::Empty(ref e)) => {
                    let name = super::xmlutil::local_name(e);
                    handle_start(
                        &name,
                        e,
                        &mut active_plane,
                        &mut active_channel,
                        &mut active_channel_id,
                        &mut channels,
                    );
                    handle_end(
                        &name,
                        "",
                        &mut active_plane,
                        &mut active_channel,
                        &mut channels,
                        &mut planes,
                        &mut plate_rows,
                        &mut plate_cols,
                        &dir,
                        &images_dir,
                    );
                    current_name.clear();
                }
                Ok(quick_xml::events::Event::Text(ref t)) => {
                    if let Ok(s) = t.unescape() {
                        text_buf.push_str(&s);
                    }
                }
                Ok(quick_xml::events::Event::CData(ref t)) => {
                    text_buf.push_str(&String::from_utf8_lossy(t.as_ref()));
                }
                Ok(quick_xml::events::Event::End(ref e)) => {
                    let name = super::xmlutil::local_name(e);
                    handle_end(
                        &current_name,
                        &text_buf,
                        &mut active_plane,
                        &mut active_channel,
                        &mut channels,
                        &mut planes,
                        &mut plate_rows,
                        &mut plate_cols,
                        &dir,
                        &images_dir,
                    );
                    // handle_end with element close ('Image'/'Entry') uses qName
                    handle_close(
                        &name,
                        &mut active_plane,
                        &mut active_channel,
                        &channels,
                        &mut planes,
                    );
                    current_name.clear();
                    text_buf.clear();
                }
                Ok(quick_xml::events::Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
        }

        if planes.is_empty() {
            return Err(BioFormatsError::Format(
                "PerkinElmer Operetta: no image planes found in index".to_string(),
            ));
        }

        // Collect unique coordinate sets (mirrors initFile).
        let mut rows = unique_sorted(planes.iter().map(|p| p.row));
        let cols = unique_sorted(planes.iter().map(|p| p.col));
        let fields = unique_sorted(planes.iter().map(|p| p.field));
        let zs = unique_sorted(planes.iter().map(|p| p.z));
        let cs = unique_sorted(planes.iter().map(|p| p.c));
        let ts = unique_sorted(planes.iter().map(|p| p.t));
        rows.dedup();

        let mut unique_wells: Vec<String> = Vec::new();
        for p in &planes {
            let w = super::well_name(p.row, p.col);
            if !unique_wells.contains(&w) {
                unique_wells.push(w);
            }
        }

        let size_z = zs.len().max(1) as u32;
        let size_c = cs.len().max(1) as u32;
        let size_t = ts.len().max(1) as u32;
        let n_planes = (size_z * size_c * size_t) as usize;
        let series_count = unique_wells.len() * fields.len().max(1);

        // hashToPlane keyed by row:col:field:c:z:t
        let mut hash: Map<String, Plane> = Map::new();
        for p in &planes {
            let key = format!("{}:{}:{}:{}:{}:{}", p.row, p.col, p.field, p.c, p.z, p.t);
            hash.insert(key, p.clone());
        }

        // Build planes[series][plane] in dimension order XYCZT
        // (Java nested loop: for t { for z { for c { nextPlane++ } } } => C fastest).
        let mut series_planes: Vec<Vec<Option<Plane>>> = vec![vec![None; n_planes]; series_count];
        let mut next_series = 0usize;
        for &r in &rows {
            for &cc in &cols {
                let well = super::well_name(r, cc);
                if !unique_wells.contains(&well) {
                    continue;
                }
                for &f in &fields {
                    let mut next_plane = 0usize;
                    for &t in &ts {
                        for &z in &zs {
                            for &ch in &cs {
                                let key = format!("{}:{}:{}:{}:{}:{}", r, cc, f, ch, z, t);
                                if let Some(p) = hash.get(&key) {
                                    if next_series < series_count && next_plane < n_planes {
                                        series_planes[next_series][next_plane] = Some(p.clone());
                                    }
                                }
                                next_plane += 1;
                            }
                        }
                    }
                    next_series += 1;
                }
            }
        }

        // Determine pixel type / size from the first valid TIFF found.
        let mut size_x = planes[0].x.max(1);
        let mut size_y = planes[0].y.max(1);
        let mut pixel_type = PixelType::Uint16;
        let mut bits = 16u8;
        let mut little_endian = true;
        'find: for sp in &series_planes {
            for p in sp.iter().flatten() {
                if let Some(f) = &p.filename {
                    if let Some((sx, sy, pt, b, le)) = super::probe_tiff(f) {
                        // Ignore uint32 (PerkinElmer flags these as invalid).
                        if pt != PixelType::Uint32 {
                            size_x = sx.max(p.x);
                            size_y = sy.max(p.y);
                            pixel_type = pt;
                            bits = b;
                            little_endian = le;
                            break 'find;
                        }
                    }
                }
            }
        }

        // Assemble HcsAssembly.
        let mut series = Vec::with_capacity(series_count);
        let mut asm_planes: Vec<Vec<PlaneRef>> = Vec::with_capacity(series_count);
        for sp in &series_planes {
            // per-series XY: use first non-null plane's stored dims if present
            let (sx, sy) = sp
                .iter()
                .flatten()
                .find(|p| p.x > 0 && p.y > 0)
                .map(|p| (p.x.max(size_x), p.y.max(size_y)))
                .unwrap_or((size_x, size_y));
            series.push(super::make_series_meta(
                sx.max(1),
                sy.max(1),
                size_z,
                size_c,
                size_t,
                pixel_type,
                bits,
                little_endian,
                DimensionOrder::XYCZT,
                "PerkinElmer Operetta",
            ));
            asm_planes.push(
                sp.iter()
                    .map(|p| match p {
                        Some(p) => match p.filename.clone() {
                            Some(f) => PlaneRef::whole(f, 0),
                            None => PlaneRef::default(),
                        },
                        None => PlaneRef::default(),
                    })
                    .collect(),
            );
        }

        let mut asm = HcsAssembly::new();
        asm.series = series;
        asm.planes = asm_planes;
        Ok(asm)
    }

    fn locate_images_dir(dir: &Path) -> PathBuf {
        // The XML's parent is usually the Images directory itself.
        if dir
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case("images"))
            .unwrap_or(false)
        {
            return dir.to_path_buf();
        }
        // Otherwise look for an "Images" subdirectory.
        if let Ok(entries) = std::fs::read_dir(dir) {
            for entry in entries.flatten() {
                let p = entry.path();
                if p.is_dir()
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .map(|n| n.eq_ignore_ascii_case("images"))
                        .unwrap_or(false)
                {
                    return p;
                }
            }
        }
        dir.to_path_buf()
    }

    fn resolve_url(value: &str, dir: &Path, images_dir: &Path) -> Option<PathBuf> {
        if value.is_empty() {
            return None;
        }
        if value.starts_with("http") {
            return Some(PathBuf::from(value));
        }
        let direct = dir.join(value);
        if direct.exists() {
            return Some(direct);
        }
        let via_images = images_dir.join(value);
        if via_images.exists() {
            return Some(via_images);
        }
        // Default to the images-dir candidate even if it doesn't exist yet, so
        // assembly can proceed and open_bytes can blank-fill missing planes.
        Some(via_images)
    }

    fn handle_start(
        name: &str,
        e: &quick_xml::events::BytesStart,
        active_plane: &mut Option<Plane>,
        active_channel: &mut Option<Channel>,
        active_channel_id: &mut i32,
        channels: &mut Map<i32, Channel>,
    ) {
        match name {
            "Image" => {
                if super::xmlutil::attr(e, "id").is_none() {
                    *active_plane = Some(Plane::default());
                }
            }
            "Entry" => {
                if let Some(cid) = super::xmlutil::attr(e, "ChannelID") {
                    if let Ok(cid) = cid.trim().parse::<i32>() {
                        *active_channel_id = cid;
                        let ch = Channel {
                            channel_id: cid,
                            ..Default::default()
                        };
                        channels.insert(cid, ch.clone());
                        *active_channel = Some(ch);
                    }
                }
            }
            _ => {}
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_end(
        current_name: &str,
        value: &str,
        active_plane: &mut Option<Plane>,
        active_channel: &mut Option<Channel>,
        channels: &mut Map<i32, Channel>,
        _planes: &mut Vec<Plane>,
        plate_rows: &mut i32,
        plate_cols: &mut i32,
        dir: &Path,
        images_dir: &Path,
    ) {
        let v = value.trim();
        match current_name {
            "PlateRows" => {
                if let Ok(n) = v.parse::<i32>() {
                    *plate_rows = n;
                }
            }
            "PlateColumns" => {
                if let Ok(n) = v.parse::<i32>() {
                    *plate_cols = n;
                }
            }
            _ => {}
        }

        // Channel/plane dimension fields.
        if active_plane.is_some() || active_channel.is_some() {
            match current_name {
                "ImageSizeX" => {
                    if let Ok(x) = v.parse::<u32>() {
                        if let Some(p) = active_plane.as_mut() {
                            p.x = x;
                        } else if let Some(c) = active_channel.as_mut() {
                            c.x = x;
                            if let Some(stored) = channels.get_mut(&c.channel_id) {
                                stored.x = x;
                            }
                        }
                    }
                }
                "ImageSizeY" => {
                    if let Ok(y) = v.parse::<u32>() {
                        if let Some(p) = active_plane.as_mut() {
                            p.y = y;
                        } else if let Some(c) = active_channel.as_mut() {
                            c.y = y;
                            if let Some(stored) = channels.get_mut(&c.channel_id) {
                                stored.y = y;
                            }
                        }
                    }
                }
                _ => {}
            }
        }

        // Plane-only fields.
        if let Some(p) = active_plane.as_mut() {
            match current_name {
                "URL" => {
                    if let Some(f) = resolve_url(v, dir, images_dir) {
                        p.filename = Some(f);
                    }
                }
                "Row" => {
                    if let Ok(n) = v.parse::<i32>() {
                        p.row = n - 1;
                    }
                }
                "Col" => {
                    if let Ok(n) = v.parse::<i32>() {
                        p.col = n - 1;
                    }
                }
                "FieldID" => {
                    if let Ok(n) = v.parse::<i32>() {
                        p.field = n;
                    }
                }
                "PlaneID" => {
                    if let Ok(n) = v.parse::<i32>() {
                        p.z = n;
                    }
                }
                "TimepointID" => {
                    if let Ok(n) = v.parse::<i32>() {
                        p.t = n;
                    }
                }
                "ChannelID" => {
                    if let Ok(n) = v.parse::<i32>() {
                        p.c = n;
                    }
                }
                _ => {}
            }
        }
    }

    fn handle_close(
        qname: &str,
        active_plane: &mut Option<Plane>,
        active_channel: &mut Option<Channel>,
        channels: &Map<i32, Channel>,
        planes: &mut Vec<Plane>,
    ) {
        match qname {
            "Image" => {
                if let Some(mut p) = active_plane.take() {
                    // Copy channel-level dims into the plane if unset (V6 layout).
                    if let Some(c) = channels.get(&p.c) {
                        if c.channel_id >= 0 && c.x != 0 && c.y != 0 {
                            if p.x == 0 {
                                p.x = c.x;
                            }
                            if p.y == 0 {
                                p.y = c.y;
                            }
                        }
                    }
                    planes.push(p);
                }
            }
            "Entry" => {
                *active_channel = None;
            }
            _ => {}
        }
    }

    fn unique_sorted<I: Iterator<Item = i32>>(it: I) -> Vec<i32> {
        let mut v: Vec<i32> = Vec::new();
        for x in it {
            if !v.contains(&x) {
                v.push(x);
            }
        }
        v.sort_unstable();
        v
    }
}

// ===========================================================================
// Columbus parser (MeasurementIndex.ColumbusIDX.xml)  -- port of ColumbusReader
// ===========================================================================

mod columbus {
    use super::*;
    use std::collections::HashMap as Map;

    #[derive(Clone, Default)]
    struct Plane {
        file: Option<PathBuf>,
        file_index: u32,
        row: i32,
        col: i32,
        field: i32,
        timepoint: i32,
        channel: i32,
        z: i32,
    }

    pub fn parse(path: &Path) -> Result<HcsAssembly> {
        // Resolve to the actual ColumbusIDX index file if a sibling was given.
        let xml_path = find_index(path).unwrap_or_else(|| path.to_path_buf());
        let parent = xml_path.parent().unwrap_or(Path::new(".")).to_path_buf();

        let main_xml = std::fs::read_to_string(&xml_path).map_err(BioFormatsError::Io)?;
        let (plate_rows, plate_cols, image_refs) = parse_measurement_index(&main_xml);

        // The per-image XML lists may live in timepoint subdirectories, or be
        // referenced directly. Discover all *.columbusidx.xml under the parent.
        let mut image_xmls: Vec<(PathBuf, i32)> = Vec::new();
        let mut timepoint_dirs: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&parent) {
            let mut dirs: Vec<PathBuf> = entries
                .flatten()
                .map(|e| e.path())
                .filter(|p| p.is_dir())
                .collect();
            dirs.sort();
            for d in &dirs {
                timepoint_dirs.push(d.clone());
            }
            for (ti, d) in dirs.iter().enumerate() {
                if let Ok(sub) = std::fs::read_dir(d) {
                    for f in sub.flatten() {
                        let p = f.path();
                        if is_columbus_idx(&p) {
                            image_xmls.push((p, ti as i32));
                        }
                    }
                }
            }
        }
        // Also accept references named in the measurement index itself.
        for r in &image_refs {
            let cand = parent.join(r);
            if is_columbus_idx(&cand) && !image_xmls.iter().any(|(p, _)| p == &cand) {
                image_xmls.push((cand, 0));
            }
        }
        // Fallback: ColumbusIDX files directly in parent.
        if image_xmls.is_empty() {
            if let Ok(entries) = std::fs::read_dir(&parent) {
                for f in entries.flatten() {
                    let p = f.path();
                    if is_columbus_idx(&p) && p != xml_path {
                        image_xmls.push((p, 0));
                    }
                }
            }
        }

        let mut planes: Vec<Plane> = Vec::new();
        for (p, t) in &image_xmls {
            parse_image_xml(p, *t, &mut planes);
        }

        if planes.is_empty() {
            return Err(BioFormatsError::Format(
                "PerkinElmer Columbus: no image planes found in index".to_string(),
            ));
        }

        // Sort planes by (row, col, field, t, c, z).
        planes.sort_by(|a, b| {
            a.row
                .cmp(&b.row)
                .then(a.col.cmp(&b.col))
                .then(a.field.cmp(&b.field))
                .then(a.timepoint.cmp(&b.timepoint))
                .then(a.channel.cmp(&b.channel))
                .then(a.z.cmp(&b.z))
        });

        // Java ColumbusReader uses the raw getPlateColumns() for the sample
        // index (ColumbusReader.java:316,375), with no minimum-of-1 clamp.
        let cols_for_sample = plate_cols;
        let mut unique_samples: Vec<i32> = Vec::new();
        let mut unique_rows: Vec<i32> = Vec::new();
        let mut unique_cols: Vec<i32> = Vec::new();
        let mut n_fields = 0i32;
        let mut size_c = 0i32;
        let mut size_t = 0i32;
        let mut size_z = 0i32;
        for p in &planes {
            let sample = p.row * cols_for_sample + p.col;
            if !unique_samples.contains(&sample) {
                unique_samples.push(sample);
            }
            if !unique_rows.contains(&p.row) {
                unique_rows.push(p.row);
            }
            if !unique_cols.contains(&p.col) {
                unique_cols.push(p.col);
            }
            n_fields = n_fields.max(p.field + 1);
            size_c = size_c.max(p.channel + 1);
            size_t = size_t.max(p.timepoint + 1);
            size_z = size_z.max(p.z + 1);
        }
        let size_c = size_c.max(1) as u32;
        let size_t = size_t.max(1) as u32;
        let size_z = size_z.max(1) as u32;
        let n_fields = n_fields.max(1);
        let order = DimensionOrder::XYCTZ;
        let n_planes = (size_z * size_c * size_t) as usize;

        // Probe the first plane's TIFF for pixel parameters.
        let mut size_x = 1u32;
        let mut size_y = 1u32;
        let mut pixel_type = PixelType::Uint16;
        let mut bits = 16u8;
        let mut little_endian = true;
        for p in &planes {
            if let Some(f) = &p.file {
                if let Some((sx, sy, pt, b, le)) = super::probe_tiff(f) {
                    size_x = sx;
                    size_y = sy;
                    pixel_type = pt;
                    bits = b;
                    little_endian = le;
                    break;
                }
            }
        }

        // Build wellSample index order: for each unique row, col (if sample present),
        // then field.
        let series_count = unique_samples.len() * n_fields as usize;
        let mut series = Vec::with_capacity(series_count);
        let mut asm_planes: Vec<Vec<PlaneRef>> = Vec::with_capacity(series_count);

        for &row in &unique_rows {
            for &col in &unique_cols {
                if !unique_samples.contains(&(row * cols_for_sample + col)) {
                    continue;
                }
                for field in 0..n_fields {
                    let mut sp = vec![PlaneRef::default(); n_planes];
                    for t in 0..size_t {
                        for c in 0..size_c {
                            for z in 0..size_z {
                                if let Some(p) = planes.iter().find(|p| {
                                    p.row == row
                                        && p.col == col
                                        && p.field == field
                                        && p.timepoint == t as i32
                                        && p.channel == c as i32
                                        && p.z == z as i32
                                }) {
                                    let idx =
                                        super::get_index(order, size_z, size_c, size_t, z, c, t)
                                            as usize;
                                    if idx < n_planes {
                                        if let Some(f) = p.file.clone() {
                                            sp[idx] = PlaneRef::whole(f, p.file_index);
                                        }
                                    }
                                }
                            }
                        }
                    }
                    series.push(super::make_series_meta(
                        size_x,
                        size_y,
                        size_z,
                        size_c,
                        size_t,
                        pixel_type,
                        bits,
                        little_endian,
                        order,
                        "PerkinElmer Columbus",
                    ));
                    asm_planes.push(sp);
                }
            }
        }
        let _ = plate_rows;

        let mut asm = HcsAssembly::new();
        asm.series = series;
        asm.planes = asm_planes;
        Ok(asm)
    }

    fn is_columbus_idx(p: &Path) -> bool {
        p.is_file()
            && p.file_name()
                .and_then(|n| n.to_str())
                .map(|n| n.to_ascii_lowercase().ends_with("columbusidx.xml"))
                .unwrap_or(false)
    }

    fn find_index(name: &Path) -> Option<PathBuf> {
        const XML_FILE: &str = "MeasurementIndex.ColumbusIDX.xml";
        // If the given file is itself the index, use it.
        if name
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.eq_ignore_ascii_case(XML_FILE))
            .unwrap_or(false)
        {
            return Some(name.to_path_buf());
        }
        let parent = name.parent()?;
        let cand = parent.join(XML_FILE);
        if cand.exists() {
            return Some(cand);
        }
        if let Some(grand) = parent.parent() {
            let cand = grand.join(XML_FILE);
            if cand.exists() {
                return Some(cand);
            }
        }
        None
    }

    /// Parse the top-level measurement index for plate dims + referenced files.
    fn parse_measurement_index(xml: &str) -> (i32, i32, Vec<String>) {
        let mut plate_rows = 0i32;
        let mut plate_cols = 0i32;
        let mut refs: Vec<String> = Vec::new();
        let mut cur = String::new();
        let mut reader = quick_xml::Reader::from_str(xml);
        reader.config_mut().trim_text(false);
        let mut text = String::new();
        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Start(ref e)) => {
                    cur = super::xmlutil::local_name(e);
                    text.clear();
                }
                Ok(quick_xml::events::Event::Text(ref t)) => {
                    if let Ok(s) = t.unescape() {
                        text.push_str(&s);
                    }
                }
                Ok(quick_xml::events::Event::End(_)) => {
                    let v = text.trim();
                    match cur.as_str() {
                        "PlateRows" => {
                            if let Ok(n) = v.parse() {
                                plate_rows = n;
                            }
                        }
                        "PlateColumns" => {
                            if let Ok(n) = v.parse() {
                                plate_cols = n;
                            }
                        }
                        "Reference" => {
                            if !v.is_empty() {
                                refs.push(v.to_string());
                            }
                        }
                        _ => {}
                    }
                    cur.clear();
                    text.clear();
                }
                Ok(quick_xml::events::Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
        }
        (plate_rows, plate_cols, refs)
    }

    /// Parse a per-timepoint image-list XML, appending discovered planes.
    fn parse_image_xml(path: &Path, external_time: i32, out: &mut Vec<Plane>) {
        let Ok(xml) = std::fs::read_to_string(path) else {
            return;
        };
        let parent = path.parent().unwrap_or(Path::new(".")).to_path_buf();

        let mut reader = quick_xml::Reader::from_str(&xml);
        reader.config_mut().trim_text(false);

        let mut in_image = false;
        let mut depth_image = 0i32; // distinguish <Images> from <Image>
        let mut cur = String::new();
        let mut text = String::new();
        let mut cur_attrs: Map<String, String> = Map::new();
        let mut plane = Plane::default();

        loop {
            match reader.read_event() {
                Ok(quick_xml::events::Event::Start(ref e)) => {
                    let ln = super::xmlutil::local_name(e);
                    if ln == "Image" {
                        in_image = true;
                        depth_image += 1;
                        plane = Plane::default();
                    }
                    cur = ln;
                    text.clear();
                    cur_attrs.clear();
                    for a in e.attributes().flatten() {
                        let k = String::from_utf8_lossy(a.key.as_ref()).to_string();
                        let v = String::from_utf8_lossy(&a.value).to_string();
                        cur_attrs.insert(k, v);
                    }
                }
                Ok(quick_xml::events::Event::Text(ref t)) => {
                    if let Ok(s) = t.unescape() {
                        text.push_str(&s);
                    }
                }
                Ok(quick_xml::events::Event::End(ref e)) => {
                    let ln = super::xmlutil::local_name(e);
                    let v = text.trim().to_string();
                    if in_image && ln != "Image" {
                        apply_image_field(&mut plane, &cur, &v, &cur_attrs, &parent, external_time);
                    }
                    if ln == "Image" {
                        in_image = false;
                        depth_image -= 1;
                        if depth_image >= 0 {
                            out.push(std::mem::take(&mut plane));
                        }
                    }
                    cur.clear();
                    text.clear();
                }
                Ok(quick_xml::events::Event::Eof) => break,
                Err(_) => break,
                _ => {}
            }
        }
    }

    fn apply_image_field(
        p: &mut Plane,
        name: &str,
        value: &str,
        attrs: &Map<String, String>,
        parent: &Path,
        external_time: i32,
    ) {
        match name {
            "URL" => {
                p.file = Some(parent.join(value));
                if let Some(buf) = attrs.get("BufferNo") {
                    if let Ok(n) = buf.trim().parse() {
                        p.file_index = n;
                    }
                }
            }
            "Row" => {
                if let Ok(n) = value.parse::<i32>() {
                    p.row = n - 1;
                }
            }
            "Col" => {
                if let Ok(n) = value.parse::<i32>() {
                    p.col = n - 1;
                }
            }
            "FieldID" => {
                if let Ok(n) = value.parse::<i32>() {
                    p.field = n - 1;
                }
            }
            "PlaneID" => {
                if let Ok(n) = value.parse::<i32>() {
                    p.z = n - 1;
                }
            }
            "TimepointID" => {
                if let Ok(n) = value.parse::<i32>() {
                    p.timepoint = n - 1;
                    if p.timepoint == 0 {
                        p.timepoint = external_time;
                    }
                }
            }
            "ChannelID" => {
                if let Ok(n) = value.parse::<i32>() {
                    p.channel = n - 1;
                }
            }
            _ => {}
        }
    }
}

// ===========================================================================
// ScanR parser (experiment_descriptor.xml)  -- port of ScanrReader
// ===========================================================================

mod scanr {
    use super::*;
    use std::collections::HashMap as Map;

    fn block(index: i32, axis: &str) -> String {
        format!("{}{:05}", axis, index)
    }

    fn adjust_well_dims(well_count: usize) -> (i32, i32) {
        // (wellColumns, wellRows)
        if well_count <= 8 {
            (2, 4)
        } else if well_count <= 96 {
            (12, 8)
        } else {
            (24, 16)
        }
    }

    pub fn parse(path: &Path) -> Result<HcsAssembly> {
        let dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();
        let raw = std::fs::read(path).map_err(BioFormatsError::Io)?;
        // ScanR XML may be ISO-8859-1; decode leniently.
        let xml = String::from_utf8_lossy(&raw).to_string();

        let mut h = ScanrHandler::default();
        h.run(&xml);

        let mut well_rows = h.well_rows;
        let mut well_columns = h.well_columns;
        if well_rows == 0 || well_columns == 0 {
            let mut urows: Vec<String> = Vec::new();
            let mut ucols: Vec<String> = Vec::new();
            for w in h.well_labels.keys() {
                let first = w.chars().next().unwrap_or('0');
                if !first.is_alphabetic() {
                    continue;
                }
                let row = w[..1].trim().to_string();
                let col = w[1..].trim().to_string();
                if !row.is_empty() && !urows.contains(&row) {
                    urows.push(row);
                }
                if !col.is_empty() && !ucols.contains(&col) {
                    ucols.push(col);
                }
            }
            well_rows = urows.len() as i32;
            well_columns = ucols.len() as i32;
            if well_rows * well_columns != h.well_count as i32 {
                let (c, r) = adjust_well_dims(h.well_count);
                well_columns = c;
                well_rows = r;
            }
        }

        let n_channels = if h.size_c == 0 {
            h.channel_names.len().max(1)
        } else {
            (h.channel_names.len()).min(h.size_c as usize).max(1)
        } as i32;
        let n_slices = if h.size_z == 0 { 1 } else { h.size_z } as i32;
        let mut n_timepoints = h.size_t.max(0) as i32;
        let n_wells = h.well_count as i32;
        let n_pos = if h.found_positions {
            h.field_position_count.max(1) as i32
        } else {
            (h.field_rows * h.field_columns).max(1)
        };

        let data_dir = dir.join("data");
        let mut list = if data_dir.is_dir() {
            list_tiffs(&data_dir)
        } else {
            list_tiffs(&dir)
        };

        if n_timepoints == 0
            || (list.len() as i32) < n_timepoints * n_channels * n_slices * n_wells * n_pos
        {
            let denom = n_channels * n_wells * n_pos * n_slices;
            n_timepoints = if denom > 0 {
                (list.len() as i32) / denom
            } else {
                0
            };
            if n_timepoints == 0 {
                n_timepoints = 1;
            }
        }

        list.sort_by(|a, b| {
            let la = well_label_of(a);
            let lb = well_label_of(b);
            let ia = h.well_labels.get(&la).copied();
            let ib = h.well_labels.get(&lb).copied();
            match (ia, ib) {
                (Some(x), Some(y)) if x != y => x.cmp(&y),
                _ => a.cmp(b),
            }
        });

        let total = (n_channels * n_wells * n_pos * n_timepoints * n_slices).max(0) as usize;
        let mut tiffs: Vec<Option<PathBuf>> = vec![None; total];
        let mut next = 0usize;
        let mut last_list_index = 0usize;

        // Sorted well-label keys (row letter, then numeric column), mirroring
        // the Java `keys` array used to drop empty wells from `well_labels`.
        let mut keys: Vec<String> = h.well_labels.keys().cloned().collect();
        keys.sort_by(|s1, s2| {
            let r1 = s1.chars().next().unwrap_or('\0');
            let r2 = s2.chars().next().unwrap_or('\0');
            if r1 != r2 {
                return r1.cmp(&r2);
            }
            let c1: i32 = s1[1..].trim().parse().unwrap_or(0);
            let c2: i32 = s2[1..].trim().parse().unwrap_or(0);
            c1.cmp(&c2)
        });

        // Port of ScanrReader's skip-missing-wells loop. `well_numbers` is the
        // mutable map series->wellNumber; entries for fully-empty wells are
        // removed (skipMissingWells defaults to true), and `next` is NOT
        // advanced past their blank slots so present wells compact forward.
        let mut realpos_count = 0i32;
        for well in 0..n_wells {
            let mut missing_well_files = 0i32;
            let well_index = h.well_numbers.get(&well).copied().unwrap_or(well + 1);
            let well_pos = block(well_index, "W");
            let original_index = next;

            for pos in 0..n_pos {
                let pos_pos = block(pos + 1, "P");
                let pos_index = next;
                for z in 0..n_slices {
                    let z_pos = block(z, "Z");
                    for t in 0..n_timepoints {
                        let t_pos = block(t, "T");
                        for c in 0..n_channels {
                            let cname =
                                h.channel_names.get(c as usize).cloned().unwrap_or_default();
                            for i in last_list_index..list.len() {
                                let f = &list[i];
                                let fname = f.file_name().and_then(|n| n.to_str()).unwrap_or("");
                                if fname.contains(&well_pos)
                                    && fname.contains(&z_pos)
                                    && fname.contains(&pos_pos)
                                    && fname.contains(&t_pos)
                                    && (cname.is_empty() || fname.contains(&cname))
                                {
                                    if next < total {
                                        tiffs[next] = Some(f.clone());
                                    }
                                    next += 1;
                                    if c == n_channels - 1 {
                                        last_list_index = i;
                                    }
                                    break;
                                }
                            }
                            // Java: increments missingWellFiles whenever the
                            // whole well has produced nothing so far.
                            if next == original_index {
                                missing_well_files += 1;
                            }
                        }
                    }
                }
                if pos_index != next {
                    realpos_count += 1;
                }
            }
            // Drop empty well label (matches keys[] removal in Java).
            if next == original_index && (well as usize) < keys.len() {
                h.well_labels.remove(&keys[well as usize]);
            }
            // Fully-empty well: skip it (default), compacting later wells.
            if next == original_index
                && missing_well_files == n_slices * n_timepoints * n_channels * n_pos
            {
                h.well_numbers.remove(&well);
            }
        }
        let mut n_wells = h.well_numbers.len() as i32;

        // Recompute plate dimensions if labels were dropped (Java block).
        if !h.well_labels.is_empty() && h.well_labels.len() as i32 != n_wells {
            let mut urows: Vec<String> = Vec::new();
            let mut ucols: Vec<String> = Vec::new();
            for w in h.well_labels.keys() {
                if !w.chars().next().map(|c| c.is_alphabetic()).unwrap_or(false) {
                    continue;
                }
                let row = w[..1].trim().to_string();
                let col = w[1..].trim().to_string();
                if !row.is_empty() && !urows.contains(&row) {
                    urows.push(row);
                }
                if !col.is_empty() && !ucols.contains(&col) {
                    ucols.push(col);
                }
            }
            n_wells = (urows.len() * ucols.len()) as i32;
            let (c, r) = adjust_well_dims(n_wells as usize);
            well_columns = c;
            well_rows = r;
        }

        let mut n_pos = n_pos;
        if realpos_count < n_pos {
            n_pos = realpos_count;
        }

        let mut size_x = 1u32;
        let mut size_y = 1u32;
        let mut pixel_type = PixelType::Uint16;
        let mut little_endian = true;
        for t in tiffs.iter().flatten() {
            if let Some((sx, sy, pt, _b, le)) = super::probe_tiff(t) {
                size_x = sx;
                size_y = sy;
                // ScanR records signed pixels incorrectly; coerce to unsigned.
                pixel_type = match pt {
                    PixelType::Int8 => PixelType::Uint8,
                    PixelType::Int16 => PixelType::Uint16,
                    other => other,
                };
                little_endian = le;
                break;
            }
        }

        let series_count = (n_wells * n_pos).max(1) as usize;
        let order = DimensionOrder::XYCTZ;
        let size_c = n_channels.max(1) as u32;
        let size_z = n_slices.max(1) as u32;
        let size_t = n_timepoints.max(1) as u32;
        let image_count = (size_z * size_t * size_c) as usize;

        let mut series = Vec::with_capacity(series_count);
        let mut asm_planes: Vec<Vec<PlaneRef>> = Vec::with_capacity(series_count);
        for s in 0..series_count {
            series.push(super::make_series_meta(
                size_x,
                size_y,
                size_z,
                size_c,
                size_t,
                pixel_type,
                12,
                little_endian,
                order,
                "Olympus ScanR",
            ));
            // tiffs layout: index = series * image_count + plane (per Java openBytes).
            // tiffs is compacted by the skip-missing-wells loop above, so series
            // indices map only onto wells/positions that actually have data.
            let mut sp = vec![PlaneRef::default(); image_count];
            for plane in 0..image_count {
                let idx = s * image_count + plane;
                if let Some(Some(f)) = tiffs.get(idx) {
                    sp[plane] = PlaneRef::whole(f.clone(), 0);
                }
            }
            asm_planes.push(sp);
        }
        let _ = (well_rows, well_columns);

        if series.is_empty() {
            return Err(BioFormatsError::Format(
                "Olympus ScanR: no series assembled".to_string(),
            ));
        }

        let mut asm = HcsAssembly::new();
        asm.series = series;
        asm.planes = asm_planes;
        Ok(asm)
    }

    fn list_tiffs(dir: &Path) -> Vec<PathBuf> {
        let mut v: Vec<PathBuf> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(dir) {
            for e in entries.flatten() {
                let p = e.path();
                if p.is_file()
                    && p.extension()
                        .and_then(|x| x.to_str())
                        .map(|x| x.eq_ignore_ascii_case("tif") || x.eq_ignore_ascii_case("tiff"))
                        .unwrap_or(false)
                {
                    v.push(p);
                }
            }
        }
        v.sort();
        v
    }

    fn well_label_of(p: &Path) -> String {
        let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
        match name.find('-') {
            Some(i) => name[..i].to_string(),
            None => String::new(),
        }
    }

    #[derive(Default)]
    struct ScanrHandler {
        well_rows: i32,
        well_columns: i32,
        field_rows: i32,
        field_columns: i32,
        well_count: usize,
        size_c: u32,
        size_z: u32,
        size_t: u32,
        channel_names: Vec<String>,
        well_labels: Map<String, i32>,
        well_numbers: Map<i32, i32>,
        found_positions: bool,
        field_position_count: usize,
    }

    impl ScanrHandler {
        fn run(&mut self, xml: &str) {
            let mut reader = quick_xml::Reader::from_str(xml);
            reader.config_mut().trim_text(false);
            let mut qname = String::new();
            let mut key = String::new();
            let mut valid_channel = false;
            let mut found_plate_layout = false;
            let mut well_index = String::new();
            let mut text = String::new();

            loop {
                match reader.read_event() {
                    Ok(quick_xml::events::Event::Start(ref e)) => {
                        qname = super::xmlutil::local_name(e);
                        text.clear();
                        if qname == "Array" || qname == "Cluster" {
                            valid_channel = true;
                        }
                    }
                    Ok(quick_xml::events::Event::Text(ref t)) => {
                        if let Ok(s) = t.unescape() {
                            text.push_str(&s);
                        }
                    }
                    Ok(quick_xml::events::Event::End(ref e)) => {
                        let v = text.trim().to_string();
                        if !v.is_empty() {
                            match qname.as_str() {
                                "Name" => {
                                    key = v.clone();
                                    if v == "subposition list" {
                                        self.found_positions = true;
                                    } else if v == "format typedef" {
                                        found_plate_layout = true;
                                    }
                                }
                                "Dimsize"
                                    if self.found_positions && self.field_position_count == 0 =>
                                {
                                    if let Ok(n) = v.parse::<usize>() {
                                        self.field_position_count = n;
                                    }
                                }
                                "Val" => {
                                    self.on_val(&key, &v, &mut valid_channel, &mut well_index);
                                }
                                _ => {
                                    if key == "Rows" && found_plate_layout {
                                        if let Ok(n) = v.parse() {
                                            self.well_rows = n;
                                        }
                                    } else if key == "Columns" && found_plate_layout {
                                        if let Ok(n) = v.parse() {
                                            self.well_columns = n;
                                        }
                                        found_plate_layout = false;
                                    }
                                }
                            }
                        }
                        let ln = super::xmlutil::local_name(e);
                        if ln == "Array" || ln == "Cluster" {
                            valid_channel = false;
                        }
                        text.clear();
                    }
                    Ok(quick_xml::events::Event::Eof) => break,
                    Err(_) => break,
                    _ => {}
                }
            }
        }

        fn on_val(
            &mut self,
            key: &str,
            v: &str,
            valid_channel: &mut bool,
            well_index: &mut String,
        ) {
            match key {
                "columns/well" => self.field_columns = v.parse().unwrap_or(0),
                "rows/well" => self.field_rows = v.parse().unwrap_or(0),
                "# slices" => self.size_z = v.parse().unwrap_or(0),
                "timeloop real" => self.size_t = v.parse().unwrap_or(0),
                "timeloop count" => self.size_t = v.parse::<u32>().unwrap_or(0) + 1,
                "name" if *valid_channel => {
                    if !self.channel_names.contains(&v.to_string()) {
                        self.channel_names.push(v.to_string());
                    }
                }
                "idle" if *valid_channel => {
                    if let Some(last) = self.channel_names.last().cloned() {
                        if v == "0" && last != "Autofocus" {
                            self.size_c += 1;
                        } else {
                            self.channel_names.pop();
                        }
                    }
                }
                "well selection table + cDNA" => {
                    if v.chars()
                        .next()
                        .map(|c| c.is_ascii_digit())
                        .unwrap_or(false)
                    {
                        *well_index = v.to_string();
                        if let Ok(n) = v.parse::<i32>() {
                            self.well_numbers.insert(self.well_count as i32, n);
                            self.well_count += 1;
                        }
                    } else if let Ok(n) = well_index.parse::<i32>() {
                        self.well_labels.insert(v.to_string(), n);
                    }
                }
                _ => {}
            }
        }
    }
}

// ===========================================================================
// BD Pathway parser (Experiment.exp + .plt/.xyz/Well NN dirs) -- port of BDReader
// ===========================================================================

mod bd {
    use super::*;
    use std::collections::HashMap as Map;

    /// Minimal INI parser: returns section -> (key -> value).
    fn parse_ini(text: &str) -> Map<String, Map<String, String>> {
        let mut out: Map<String, Map<String, String>> = Map::new();
        let mut section = String::new();
        out.insert(String::new(), Map::new());
        for line in text.lines() {
            let l = line.trim();
            if l.is_empty() || l.starts_with(';') || l.starts_with('#') {
                continue;
            }
            if l.starts_with('[') && l.ends_with(']') {
                section = l[1..l.len() - 1].trim().to_string();
                out.entry(section.clone()).or_default();
            } else if let Some(eq) = l.find('=') {
                let k = l[..eq].trim().to_string();
                let v = l[eq + 1..].trim().to_string();
                out.entry(section.clone()).or_default().insert(k, v);
            }
        }
        out
    }

    fn get<'a>(
        ini: &'a Map<String, Map<String, String>>,
        sect: &str,
        key: &str,
    ) -> Option<&'a String> {
        ini.get(sect).and_then(|s| s.get(key))
    }

    pub fn parse(path: &Path) -> Result<HcsAssembly> {
        // Locate Experiment.exp.
        let exp_path = if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("exp"))
            .unwrap_or(false)
        {
            path.to_path_buf()
        } else {
            let parent = path.parent().unwrap_or(Path::new("."));
            parent.join("Experiment.exp")
        };
        let dir = exp_path.parent().unwrap_or(Path::new(".")).to_path_buf();

        let exp_text = std::fs::read_to_string(&exp_path).map_err(BioFormatsError::Io)?;
        let exp = parse_ini(&exp_text);

        // Find the .plt (plate type) file in the directory tree.
        let mut well_rows = 0i32;
        let mut well_cols = 0i32;
        let mut z_axis_value: Option<f64> = None;
        if let Ok(entries) = std::fs::read_dir(&dir) {
            for e in entries.flatten() {
                let p = e.path();
                let ext = p
                    .extension()
                    .and_then(|x| x.to_str())
                    .map(|x| x.to_ascii_lowercase());
                if ext.as_deref() == Some("plt") {
                    if let Ok(t) = std::fs::read_to_string(&p) {
                        let plt = parse_ini(&t);
                        if let Some(w) = get(&plt, "PlateType", "Wells") {
                            match w.trim().parse::<i32>() {
                                Ok(96) => {
                                    well_rows = 8;
                                    well_cols = 12;
                                }
                                Ok(384) => {
                                    well_rows = 16;
                                    well_cols = 24;
                                }
                                _ => {}
                            }
                        }
                    }
                } else if ext.as_deref() == Some("xyz") {
                    if let Ok(t) = std::fs::read_to_string(&p) {
                        let xyz = parse_ini(&t);
                        let enabled = get(&xyz, "Z1Axis", "Z1AxisEnabled")
                            .map(|s| s == "1")
                            .unwrap_or(false)
                            && get(&xyz, "Z1Axis", "Z1AxisMode")
                                .map(|s| s == "1")
                                .unwrap_or(false);
                        if enabled {
                            z_axis_value = get(&xyz, "Z1Axis", "Z1AxisValue")
                                .and_then(|s| s.trim().parse::<f64>().ok());
                        }
                    }
                }
            }
        }

        // Channels (dyes) from [General].Dyes + [Dyes] table.
        let n_dyes = get(&exp, "General", "Dyes")
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(0);
        let mut channel_names: Vec<String> = Vec::new();
        for i in 1..=n_dyes {
            if let Some(name) = get(&exp, "Dyes", &i.to_string()) {
                channel_names.push(name.clone());
            }
        }
        if channel_names.is_empty() {
            channel_names.push("Channel 0".to_string());
        }
        let n_channels = channel_names.len() as i32;

        let bits = get(&exp, "Camera", "BitdepthUsed")
            .and_then(|s| s.trim().parse::<u8>().ok())
            .unwrap_or(16);

        // Montage (fields packed in a single TIFF).
        let montage = get(&exp, "Image", "Montaged")
            .map(|s| s == "1")
            .unwrap_or(false);
        let (field_rows, field_cols) = if montage {
            (
                get(&exp, "Image", "TilesY")
                    .and_then(|s| s.trim().parse::<i32>().ok())
                    .unwrap_or(1),
                get(&exp, "Image", "TilesX")
                    .and_then(|s| s.trim().parse::<i32>().ok())
                    .unwrap_or(1),
            )
        } else {
            (1, 1)
        };
        let n_fields = (field_rows * field_cols).max(1);

        let size_z = if let Some(zv) = z_axis_value {
            (zv as i32 + 1).max(1)
        } else {
            1
        } as u32;

        // Scan "Well NN" directories.
        let mut well_dirs: Vec<(String, PathBuf)> = Vec::new();
        if let Ok(entries) = std::fs::read_dir(&dir) {
            let mut all: Vec<PathBuf> = entries.flatten().map(|e| e.path()).collect();
            all.sort();
            for p in all {
                if p.is_dir() {
                    if let Some(name) = p.file_name().and_then(|n| n.to_str()) {
                        if let Some(rest) = name.strip_prefix("Well ") {
                            // label is first token after "Well " split on whitespace/'.'
                            let label = rest
                                .split(|c: char| c.is_whitespace() || c == '.')
                                .next()
                                .unwrap_or("")
                                .to_string();
                            if !label.is_empty() {
                                well_dirs.push((label, p));
                            }
                        }
                    }
                }
            }
        }
        if well_dirs.is_empty() {
            return Err(BioFormatsError::Format(
                "BD Pathway: no 'Well NN' directories found".to_string(),
            ));
        }

        // Collect per-well tiff lists matching ".* - nNNNNNN.tif".
        let mut well_tiffs: Vec<(String, Vec<PathBuf>)> = Vec::new();
        for (label, wdir) in &well_dirs {
            let mut tiffs: Vec<PathBuf> = Vec::new();
            if let Ok(entries) = std::fs::read_dir(wdir) {
                for e in entries.flatten() {
                    let p = e.path();
                    if matches_bd_tiff(&p) {
                        tiffs.push(p);
                    }
                }
            }
            tiffs.sort();
            well_tiffs.push((label.clone(), tiffs));
        }

        // Determine sizeT by counting per-channel images in a well.
        // Mirror Java BDReader.java:668-680: a running imageCount starts at 0,
        // so the first channel with any images sets sizeT = images/sizeZ, and
        // later channels only update if they have more images than the running count.
        // Java counts the SECOND well directory (wellList.get(1),
        // BDReader.java:671), not the first non-empty one; we guard the length
        // (Java would otherwise throw IndexOutOfBounds with a single well).
        let mut size_t = 0u32;
        if let Some((_, tiffs)) = well_tiffs.get(1) {
            let mut image_count = 0u32;
            for cname in &channel_names {
                let images = tiffs
                    .iter()
                    .filter(|p| {
                        p.file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.starts_with(cname.as_str()) && n.ends_with(".tif"))
                            .unwrap_or(false)
                    })
                    .count() as u32;
                if images > image_count {
                    size_t = images / size_z.max(1);
                    image_count = size_z.max(1) * size_t * n_channels as u32;
                }
            }
        }
        let size_t = size_t.max(1);
        let size_c = n_channels.max(1) as u32;

        // Probe first TIFF for pixel parameters.
        let mut full_x = 0u32;
        let mut full_y = 0u32;
        let mut pixel_type = PixelType::Uint16;
        let mut bits_pp = bits;
        let mut little_endian = true;
        for (_, tiffs) in &well_tiffs {
            if let Some(p) = tiffs.first() {
                if let Some((sx, sy, pt, b, le)) = super::probe_tiff(p) {
                    full_x = sx;
                    full_y = sy;
                    pixel_type = pt;
                    bits_pp = b;
                    little_endian = le;
                    break;
                }
            }
        }
        let size_x = (full_x / field_cols.max(1) as u32).max(1);
        let size_y = (full_y / field_rows.max(1) as u32).max(1);

        let order = DimensionOrder::XYZTC;
        let image_count = (size_z * size_t * size_c) as usize;
        let series_count = well_tiffs.len() * n_fields as usize;

        let mut series = Vec::with_capacity(series_count);
        let mut asm_planes: Vec<Vec<PlaneRef>> = Vec::with_capacity(series_count);

        for (_label, tiffs) in well_tiffs.iter() {
            for field in 0..n_fields {
                series.push(super::make_series_meta(
                    size_x,
                    size_y,
                    size_z,
                    size_c,
                    size_t,
                    pixel_type,
                    bits_pp,
                    little_endian,
                    order,
                    "BD Pathway",
                ));
                // Montaged datasets pack all fields in one TIFF; split them per
                // the Java openBytes: fieldRow = field/fieldCols,
                // fieldCol = field%fieldCols, and the sub-region is
                // (fieldCol*sizeX, fieldRow*sizeY, sizeX, sizeY). Single-field
                // datasets read the whole plane.
                let field_row = field / field_cols.max(1);
                let field_col = field % field_cols.max(1);
                let off_x = field_col as u32 * size_x;
                let off_y = field_row as u32 * size_y;
                // Map each plane to its file via getFilename logic.
                let mut sp = vec![PlaneRef::default(); image_count];
                for plane in 0..image_count {
                    let (z, c, t) =
                        super::get_zct_coords(order, size_z, size_c, size_t, plane as u32);
                    if let Some(f) =
                        bd_filename(tiffs, &channel_names, c, z, t, order, size_z, size_t)
                    {
                        sp[plane] = if n_fields == 1 {
                            PlaneRef::whole(f, 0)
                        } else {
                            PlaneRef {
                                tiles: vec![Tile {
                                    filename: f,
                                    file_index: 0,
                                    src_x: off_x,
                                    src_y: off_y,
                                    src_w: size_x,
                                    src_h: size_y,
                                    dst_x: 0,
                                    dst_y: 0,
                                }],
                            }
                        };
                    }
                }
                asm_planes.push(sp);
            }
        }
        let _ = (well_rows, well_cols);

        let mut asm = HcsAssembly::new();
        asm.series = series;
        asm.planes = asm_planes;
        Ok(asm)
    }

    fn matches_bd_tiff(p: &Path) -> bool {
        // Pattern ".* - nDDDDDD.tif"
        let name = match p.file_name().and_then(|n| n.to_str()) {
            Some(n) => n,
            None => return false,
        };
        if !name.ends_with(".tif") {
            return false;
        }
        let stem = &name[..name.len() - 4];
        if let Some(pos) = stem.rfind(" - n") {
            let digits = &stem[pos + 4..];
            digits.len() == 6 && digits.chars().all(|c| c.is_ascii_digit())
        } else {
            false
        }
    }

    /// getFilename: channel = channelNames[c]; realIndex = getIndex(z,0,t);
    /// match name starting with channel and trailing nNNNNNN == realIndex.
    #[allow(clippy::too_many_arguments)]
    fn bd_filename(
        tiffs: &[PathBuf],
        channel_names: &[String],
        c: u32,
        z: u32,
        t: u32,
        order: DimensionOrder,
        size_z: u32,
        size_t: u32,
    ) -> Option<PathBuf> {
        let channel = channel_names.get(c as usize)?;
        // Java: getIndex(z, 0, t) with sizeC forced to 1 (channel separated by name).
        let real_index = super::get_index(order, size_z, 1, size_t, z, 0, t);
        for p in tiffs {
            let name = p.file_name().and_then(|n| n.to_str()).unwrap_or("");
            let stem = name.strip_suffix(".tif").unwrap_or(name);
            if let Some(npos) = stem.rfind('n') {
                let idx_str = &stem[npos + 1..];
                if let Ok(idx) = idx_str.parse::<u32>() {
                    if name.starts_with(channel.as_str()) && idx == real_index {
                        return Some(p.clone());
                    }
                }
            }
        }
        None
    }
}

// ===========================================================================
// CellVoyager parser (MeasurementResult.xml)  -- port of CellVoyagerReader
//
// Faithful to the Java geometry parsing (channels/wells/areas/fields/Z/T and
// per-field pixel offsets), producing one series per well x area. Each series
// plane is stitched on the fly from all of the area's field tiles, each placed
// at its (xpixels, ypixels) offset within the reconstructed area image --
// mirroring the Java openBytes tile-paste loop.
// ===========================================================================

mod cellvoyager {
    use super::*;

    #[derive(Default, Clone)]
    struct Field {
        index: i32,
        // Stage position in micrometres; consumed for min/max during area
        // sizing.
        x: f64,
        y: f64,
        // Pixel offset of this tile within the reconstructed area image
        // (Java FieldInfo.xpixels / ypixels).
        xpixels: i64,
        ypixels: i64,
    }

    #[derive(Default, Clone)]
    struct Area {
        fields: Vec<Field>,
        width: i32,
        height: i32,
    }

    #[derive(Default, Clone)]
    struct Well {
        /// Well number from XML. Retained for metadata fidelity; the filename
        /// uses the wells-list position per Java, not this value.
        #[allow(dead_code)]
        number: i32,
        areas: Vec<Area>,
    }

    pub fn parse(path: &Path) -> Result<HcsAssembly> {
        // Resolve the measurement folder + Image dir.
        let start = path;
        let measurement_folder = if start.is_dir() {
            start.to_path_buf()
        } else {
            let mut p = start.parent().unwrap_or(Path::new(".")).to_path_buf();
            if p.file_name().and_then(|n| n.to_str()) == Some("Image") {
                p = p.parent().unwrap_or(Path::new(".")).to_path_buf();
            }
            p
        };
        let image_folder = measurement_folder.join("Image");
        let ms_file = measurement_folder.join("MeasurementResult.xml");
        let ms_file = if ms_file.exists() {
            ms_file
        } else if start.is_file() {
            start.to_path_buf()
        } else {
            ms_file
        };

        let xml = std::fs::read_to_string(&ms_file).map_err(BioFormatsError::Io)?;

        // Parse with the lightweight DOM builder.
        let dom = dom::parse(&xml);
        let root = dom.root();

        let magnification = root
            .child_text(&["ObjectiveLens", "Magnification"])
            .and_then(|s| s.trim().parse::<f64>().ok())
            .unwrap_or(1.0)
            .max(1e-9);

        // Channels: enabled only; collect tile size + unmagnified pixel size.
        let mut tile_w = 0i32;
        let mut tile_h = 0i32;
        let mut unmag_px_w = 1.0f64;
        let mut unmag_px_h = 1.0f64;
        let mut channel_names: Vec<String> = Vec::new();
        if let Some(channels_el) = root.child(&["Channels"]) {
            for ch in channels_el.children("Channel") {
                let enabled = ch
                    .child_text(&["IsEnabled"])
                    .map(|s| s.trim().eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                if !enabled {
                    continue;
                }
                if channel_names.is_empty() {
                    if let Some(cam) = ch.child(&["AcquisitionSetting", "Camera"]) {
                        tile_w = cam
                            .child_text(&["EffectiveHorizontalPixels_pixel"])
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(0);
                        tile_h = cam
                            .child_text(&["EffectiveVerticalPixels_pixel"])
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(0);
                        unmag_px_w = cam
                            .child_text(&["HorizonalCellSize_um"])
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(1.0);
                        unmag_px_h = cam
                            .child_text(&["VerticalCellSize_um"])
                            .and_then(|s| s.trim().parse().ok())
                            .unwrap_or(1.0);
                    }
                }
                let number = ch.child_text(&["Number"]).unwrap_or_default();
                channel_names.push(format!("Channel {}", number.trim()));
            }
        }
        if channel_names.is_empty() {
            return Err(BioFormatsError::Format(
                "CellVoyager: no enabled channels in MeasurementResult.xml".to_string(),
            ));
        }
        // Java CellVoyagerReader reads PhysicalSizeX/Y from the companion
        // MeasurementResult.ome.xml (Image/Pixels attributes) and divides by
        // magnification (CellVoyagerReader.java:533-534, 589-590). Tile
        // placement (xpixels = round((x-xmin)/pixelWidth)) depends on this, so
        // we read it from the OME XML; we only fall back to the camera
        // cell-size / magnification when the OME XML is absent or unparsable.
        let ome_file = measurement_folder.join("MeasurementResult.ome.xml");
        let ome_phys = std::fs::read_to_string(&ome_file).ok().and_then(|s| {
            let ome_dom = dom::parse(&s);
            let pixels = ome_dom.root().child(&["Image", "Pixels"])?;
            let px = pixels
                .attr("PhysicalSizeX")
                .and_then(|v| v.trim().parse::<f64>().ok());
            let py = pixels
                .attr("PhysicalSizeY")
                .and_then(|v| v.trim().parse::<f64>().ok());
            match (px, py) {
                (Some(px), Some(py)) => Some((px, py)),
                _ => None,
            }
        });
        let (pixel_width, pixel_height) = match ome_phys {
            Some((px, py)) => (
                (px / magnification).max(1e-9),
                (py / magnification).max(1e-9),
            ),
            None => (
                (unmag_px_w / magnification).max(1e-9),
                (unmag_px_h / magnification).max(1e-9),
            ),
        };

        // Areas may be shared per-well or defined per-well.
        let same_area_per_well = root
            .child_text(&["UsesSameAreaParWell"])
            .map(|s| s.trim().eq_ignore_ascii_case("true"))
            .unwrap_or(false);

        let shared_areas = if same_area_per_well {
            root.child(&["SameAreaUsingWell", "Areas"]).map(|areas_el| {
                let mut field_index = 1;
                let mut out = Vec::new();
                for a in areas_el.children("Area") {
                    let area = read_area(
                        &a,
                        &mut field_index,
                        pixel_width,
                        pixel_height,
                        tile_w,
                        tile_h,
                    );
                    out.push(area);
                }
                out
            })
        } else {
            None
        };

        let mut wells: Vec<Well> = Vec::new();
        if let Some(wells_el) = root.child(&["Wells"]) {
            for w in wells_el.children("Well") {
                let enabled = w
                    .child_text(&["IsEnabled"])
                    .map(|s| s.trim().eq_ignore_ascii_case("true"))
                    .unwrap_or(false);
                if !enabled {
                    continue;
                }
                let number = w
                    .child_text(&["Number"])
                    .and_then(|s| s.trim().parse().ok())
                    .unwrap_or(0);
                let areas = if let Some(shared) = &shared_areas {
                    shared.clone()
                } else if let Some(areas_el) = w.child(&["Areas"]) {
                    let mut field_index = 1;
                    areas_el
                        .children("Area")
                        .iter()
                        .map(|a| {
                            read_area(
                                a,
                                &mut field_index,
                                pixel_width,
                                pixel_height,
                                tile_w,
                                tile_h,
                            )
                        })
                        .collect()
                } else {
                    Vec::new()
                };
                wells.push(Well { number, areas });
            }
        }
        if wells.is_empty() {
            return Err(BioFormatsError::Format(
                "CellVoyager: no enabled wells in MeasurementResult.xml".to_string(),
            ));
        }

        let n_z = root
            .child_text(&["ZStackConditions", "NumberOfSlices"])
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(1)
            .max(1) as u32;
        let n_t = root
            .child_text(&["TimelapsCondition", "Iteration"])
            .and_then(|s| s.trim().parse::<i32>().ok())
            .unwrap_or(1)
            .max(1) as u32;
        let n_c = channel_names.len() as u32;
        let order = DimensionOrder::XYCZT;
        let image_count = (n_z * n_c * n_t) as usize;

        // Probe pixel type from any existing field-0 tile.
        let mut pixel_type = PixelType::Uint16;
        let mut bits = 16u8;
        let mut little_endian = true;
        'probe: for (wi, well) in wells.iter().enumerate() {
            for area in &well.areas {
                if let Some(f) = area.fields.first() {
                    let fname = single_tiff_name(wi as i32 + 1, f.index, 1, 1, 1);
                    let p = image_folder.join(&fname);
                    if let Some((_x, _y, pt, b, le)) = super::probe_tiff(&p) {
                        pixel_type = pt;
                        bits = b;
                        little_endian = le;
                        break 'probe;
                    }
                }
            }
        }

        // Build one series per well x area. Each area plane is stitched from
        // all of the area's field tiles, placing each tile at its pixel offset
        // (Java openBytes loops over area.fields and pastes each tile). The
        // well index used in the filename is the position in the wells list
        // (wi + 1), matching Java's seriesToWellArea / SINGLE_TIFF_PATH_BUILDER.
        let mut series = Vec::new();
        let mut asm_planes: Vec<Vec<PlaneRef>> = Vec::new();
        for (wi, well) in wells.iter().enumerate() {
            let well_index = wi as i32 + 1;
            for area in &well.areas {
                let size_x = area.width.max(tile_w).max(1) as u32;
                let size_y = area.height.max(tile_h).max(1) as u32;
                series.push(super::make_series_meta(
                    size_x,
                    size_y,
                    n_z,
                    n_c,
                    n_t,
                    pixel_type,
                    bits,
                    little_endian,
                    order,
                    "CellVoyager",
                ));
                let mut sp = vec![PlaneRef::default(); image_count];
                for plane in 0..image_count {
                    let (z, c, t) = super::get_zct_coords(order, n_z, n_c, n_t, plane as u32);
                    let mut tiles: Vec<Tile> = Vec::with_capacity(area.fields.len());
                    for field in &area.fields {
                        // SINGLE_TIFF_PATH_BUILDER = "W%dF%03dT%04dZ%02dC%d.tif"
                        let fname = single_tiff_name(
                            well_index,
                            field.index,
                            t as i32 + 1,
                            z as i32 + 1,
                            c as i32 + 1,
                        );
                        let p = image_folder.join(&fname);
                        // Place the whole tile at its pixel offset within the
                        // reconstructed area image. src_w/src_h = 0 -> the
                        // compositor reads the full tile plane.
                        tiles.push(Tile {
                            filename: p,
                            file_index: 0,
                            src_x: 0,
                            src_y: 0,
                            src_w: 0,
                            src_h: 0,
                            dst_x: field.xpixels.max(0) as u32,
                            dst_y: field.ypixels.max(0) as u32,
                        });
                    }
                    sp[plane] = PlaneRef { tiles };
                }
                asm_planes.push(sp);
            }
        }

        if series.is_empty() {
            return Err(BioFormatsError::Format(
                "CellVoyager: no series assembled".to_string(),
            ));
        }

        let mut asm = HcsAssembly::new();
        asm.series = series;
        asm.planes = asm_planes;
        Ok(asm)
    }

    fn single_tiff_name(well: i32, field: i32, t: i32, z: i32, c: i32) -> String {
        format!("W{}F{:03}T{:04}Z{:02}C{}.tif", well, field, t, z, c)
    }

    fn read_area(
        area_el: &dom::Node,
        starting_field_index: &mut i32,
        pixel_width: f64,
        pixel_height: f64,
        tile_w: i32,
        tile_h: i32,
    ) -> Area {
        let mut fields: Vec<Field> = Vec::new();
        let mut xmin = f64::INFINITY;
        let mut ymin = f64::INFINITY;
        let mut xmax = f64::NEG_INFINITY;
        let mut ymax = f64::NEG_INFINITY;

        if let Some(fields_el) = area_el.child(&["Fields"]) {
            for f in fields_el.children("Field") {
                let x = f
                    .child_text(&["StageX_um"])
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .unwrap_or(0.0);
                let y = f
                    .child_text(&["StageY_um"])
                    .and_then(|s| s.trim().parse::<f64>().ok())
                    .unwrap_or(0.0);
                xmin = xmin.min(x);
                xmax = xmax.max(x);
                let yum = -y;
                ymin = ymin.min(yum);
                ymax = ymax.max(yum);
                fields.push(Field {
                    index: 0,
                    x,
                    y,
                    xpixels: 0,
                    ypixels: 0,
                });
            }
        }
        for f in fields.iter_mut() {
            // Java: xpixels = round((x - xmin)/pixelWidth);
            //       ypixels = round((-ymin - y)/pixelHeight).
            f.xpixels = ((f.x - xmin) / pixel_width).round() as i64;
            f.ypixels = ((-ymin - f.y) / pixel_height).round() as i64;
            f.index = *starting_field_index;
            *starting_field_index += 1;
        }
        let (width, height) = if fields.is_empty() {
            (0, 0)
        } else {
            (
                1 + ((xmax - xmin) / pixel_width) as i32,
                1 + ((ymax - ymin) / pixel_height) as i32,
            )
        };
        Area {
            fields,
            width: width + tile_w,
            height: height + tile_h,
        }
    }

    // -- Minimal read-only DOM for navigating MeasurementResult.xml --
    mod dom {
        use quick_xml::events::Event;

        #[derive(Default)]
        pub struct Node {
            pub name: String,
            pub text: String,
            pub attrs: Vec<(String, String)>,
            pub children: Vec<Node>,
        }

        pub struct Dom {
            root: Node,
        }

        impl Dom {
            pub fn root(&self) -> &Node {
                &self.root
            }
        }

        impl Node {
            /// Descend a path of local element names, returning the node.
            pub fn child(&self, path: &[&str]) -> Option<&Node> {
                let mut cur = self;
                for seg in path {
                    cur = cur.children.iter().find(|c| c.name == *seg)?;
                }
                Some(cur)
            }

            /// Text content at the end of a path.
            pub fn child_text(&self, path: &[&str]) -> Option<String> {
                self.child(path).map(|n| n.text.clone())
            }

            /// All direct children with the given local name.
            pub fn children(&self, name: &str) -> Vec<&Node> {
                self.children.iter().filter(|c| c.name == name).collect()
            }

            /// Attribute value by (local) name.
            pub fn attr(&self, name: &str) -> Option<&str> {
                self.attrs
                    .iter()
                    .find(|(k, _)| k == name)
                    .map(|(_, v)| v.as_str())
            }
        }

        fn collect_attrs(e: &quick_xml::events::BytesStart) -> Vec<(String, String)> {
            let mut out = Vec::new();
            for a in e.attributes().flatten() {
                let k = local(a.key.as_ref());
                let v = a
                    .unescape_value()
                    .map(|c| c.into_owned())
                    .unwrap_or_else(|_| String::from_utf8_lossy(&a.value).into_owned());
                out.push((k, v));
            }
            out
        }

        fn local(name: &[u8]) -> String {
            let l = match name.iter().position(|&b| b == b':') {
                Some(i) => &name[i + 1..],
                None => name,
            };
            String::from_utf8_lossy(l).to_string()
        }

        pub fn parse(xml: &str) -> Dom {
            let mut reader = quick_xml::Reader::from_str(xml);
            reader.config_mut().trim_text(false);
            let mut stack: Vec<Node> = vec![Node {
                name: "__root__".to_string(),
                ..Default::default()
            }];
            loop {
                match reader.read_event() {
                    Ok(Event::Start(ref e)) => {
                        stack.push(Node {
                            name: local(e.name().as_ref()),
                            attrs: collect_attrs(e),
                            ..Default::default()
                        });
                    }
                    Ok(Event::Empty(ref e)) => {
                        let n = Node {
                            name: local(e.name().as_ref()),
                            attrs: collect_attrs(e),
                            ..Default::default()
                        };
                        if let Some(parent) = stack.last_mut() {
                            parent.children.push(n);
                        }
                    }
                    Ok(Event::Text(ref t)) => {
                        if let Ok(s) = t.unescape() {
                            if let Some(top) = stack.last_mut() {
                                top.text.push_str(&s);
                            }
                        }
                    }
                    Ok(Event::CData(ref t)) => {
                        if let Some(top) = stack.last_mut() {
                            top.text.push_str(&String::from_utf8_lossy(t.as_ref()));
                        }
                    }
                    Ok(Event::End(_)) => {
                        if stack.len() > 1 {
                            let node = stack.pop().unwrap();
                            // Trim accumulated text.
                            let mut node = node;
                            node.text = node.text.trim().to_string();
                            if let Some(parent) = stack.last_mut() {
                                parent.children.push(node);
                            }
                        }
                    }
                    Ok(Event::Eof) => break,
                    Err(_) => break,
                    _ => {}
                }
            }
            // The real document root is the single child of __root__.
            let mut root = stack.pop().unwrap_or_default();
            let real = root.children.pop().unwrap_or_default();
            Dom { root: real }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::writer::FormatWriter;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT_TEMP_ID: AtomicU64 = AtomicU64::new(0);

    fn temp_path(name: &str) -> PathBuf {
        let id = NEXT_TEMP_ID.fetch_add(1, Ordering::Relaxed);
        std::env::temp_dir().join(format!("bioformats_hcs2_{id}_{name}"))
    }

    fn test_meta(width: u32, height: u32) -> ImageMetadata {
        make_series_meta(
            width,
            height,
            1,
            1,
            1,
            PixelType::Uint8,
            8,
            true,
            DimensionOrder::XYZCT,
            "HCS test",
        )
    }

    fn assembly_with_plane(meta: ImageMetadata, plane: PlaneRef) -> HcsAssembly {
        let mut asm = HcsAssembly::new();
        asm.series = vec![meta];
        asm.planes = vec![vec![plane]];
        asm
    }

    fn write_tiff(path: &Path, meta: &ImageMetadata, data: &[u8]) {
        let mut writer = crate::tiff::TiffWriter::new();
        writer.set_metadata(meta).unwrap();
        writer.set_id(path).unwrap();
        writer.save_bytes(0, data).unwrap();
        writer.close().unwrap();
    }

    #[test]
    fn hcs_assembly_empty_plane_ref_stays_black() {
        let meta = test_meta(3, 2);
        let mut asm = assembly_with_plane(meta, PlaneRef::default());

        let bytes = asm.open_bytes(0).unwrap();

        assert_eq!(bytes, vec![0; 6]);
    }

    #[test]
    fn hcs_assembly_missing_referenced_whole_tile_returns_error() {
        let meta = test_meta(3, 2);
        let missing = temp_path("missing_whole_tile.tif");
        let mut asm = assembly_with_plane(meta, PlaneRef::whole(missing, 0));

        let err = asm.open_bytes(0).unwrap_err();

        assert!(
            err.to_string().contains("IO error"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn hcs_assembly_unreadable_referenced_region_tile_returns_error() {
        let meta = test_meta(4, 2);
        let bad = temp_path("bad_region_tile.tif");
        std::fs::write(&bad, b"not a tiff").unwrap();
        let plane = PlaneRef {
            tiles: vec![Tile {
                filename: bad.clone(),
                file_index: 0,
                src_x: 0,
                src_y: 0,
                src_w: 2,
                src_h: 2,
                dst_x: 1,
                dst_y: 0,
            }],
        };
        let mut asm = assembly_with_plane(meta, plane);

        let err = asm.open_bytes(0).unwrap_err();

        assert!(
            err.to_string().contains("TIFF") || err.to_string().contains("Unsupported format"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_file(bad);
    }

    #[test]
    fn hcs_assembly_referenced_region_read_error_is_not_black() {
        let tile_meta = test_meta(2, 2);
        let path = temp_path("one_plane_region_tile.tif");
        write_tiff(&path, &tile_meta, &[1, 2, 3, 4]);
        let plane = PlaneRef {
            tiles: vec![Tile {
                filename: path.clone(),
                file_index: 1,
                src_x: 0,
                src_y: 0,
                src_w: 2,
                src_h: 2,
                dst_x: 0,
                dst_y: 0,
            }],
        };
        let mut asm = assembly_with_plane(test_meta(2, 2), plane);

        let err = asm.open_bytes(0).unwrap_err();

        assert!(
            err.to_string().contains("Plane index 1 out of range"),
            "unexpected error: {err}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn incell3000_rejects_short_decoded_plane_instead_of_padding() {
        let path = temp_path("short.frm");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&6i16.to_le_bytes()); // pixels offset
        bytes.extend_from_slice(&2i16.to_le_bytes()); // size X
        bytes.extend_from_slice(&33i16.to_le_bytes()); // one plane, one row
        bytes.extend_from_slice(&0x1234u16.to_le_bytes()); // one of two pixels
        std::fs::write(&path, bytes).unwrap();

        let mut reader = InCell3000Reader::new();
        reader.set_id(&path).unwrap();
        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::InvalidData(ref message) if message.contains("decoded 2 bytes")),
            "{err:?}"
        );

        let _ = std::fs::remove_file(path);
    }
}
