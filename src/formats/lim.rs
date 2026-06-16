//! LIM (Laboratory Imaging) and TillVision format readers.

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::ole::{is_ole2_header, OleFile};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;

// ── LIM Reader ────────────────────────────────────────────────────────────────

pub struct LimReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl LimReader {
    pub fn new() -> Self {
        LimReader {
            path: None,
            meta: None,
            data_offset: 0,
        }
    }
}

impl Default for LimReader {
    fn default() -> Self {
        Self::new()
    }
}

/// Fixed pixel-data offset used by LIMReader.java.
const LIM_PIXELS_OFFSET: u64 = 0x94b;

fn load_lim_header(path: &Path) -> Result<(ImageMetadata, u64)> {
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    let mut header = [0u8; 8];
    f.read_exact(&mut header).map_err(BioFormatsError::Io)?;

    // Header layout (matching LIMReader.initFile, little-endian):
    //   0  sizeX = readShort() & 0x7fff
    //   2  sizeY = readShort()
    //   4  bits  = readShort()
    //   6  isCompressed = readShort() != 0
    let size_x = (i16::from_le_bytes([header[0], header[1]]) as i32 & 0x7fff) as u32;
    let size_y_raw = i16::from_le_bytes([header[2], header[3]]) as i32;
    let mut bits = i16::from_le_bytes([header[4], header[5]]) as i32;
    let is_compressed = i16::from_le_bytes([header[6], header[7]]) != 0;

    if size_x == 0 || size_y_raw <= 0 || bits <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "LIM header is missing required dimensions".to_string(),
        ));
    }
    let size_y = size_y_raw as u32;

    // Round bits up to the next multiple of 8.
    while bits % 8 != 0 {
        bits += 1;
    }

    // RGB images store 3 channels packed; bits is divided across them.
    let mut size_c: u32 = 1;
    if bits % 3 == 0 {
        size_c = 3;
        bits /= 3;
    }

    // FormatTools.pixelTypeFromBytes(bits/8, false, false) -> unsigned integer.
    let pixel_type = match bits / 8 {
        1 => PixelType::Uint8,
        2 => PixelType::Uint16,
        4 => PixelType::Uint32,
        other => {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "LIM byte depth {other} is not supported"
            )));
        }
    };

    // LIMReader.java itself rejects compressed planes with
    // UnsupportedCompressionException("Compressed LIM files not supported."),
    // i.e. the Java reference does NOT decompress LIM data. Being faithful to
    // the reference, we reject compressed files here as well rather than
    // inventing an undocumented decompression scheme.
    if is_compressed {
        return Err(BioFormatsError::UnsupportedFormat(
            "Compressed LIM files not supported.".to_string(),
        ));
    }

    let is_rgb = size_c > 1;
    let bps = pixel_type.bytes_per_sample();
    let plane_bytes = (size_x as u64)
        .checked_mul(size_y as u64)
        .and_then(|px| px.checked_mul(size_c as u64))
        .and_then(|samples| samples.checked_mul(bps as u64))
        .ok_or_else(|| BioFormatsError::Format("LIM plane size overflows".to_string()))?;
    let required_len = LIM_PIXELS_OFFSET
        .checked_add(plane_bytes)
        .ok_or_else(|| BioFormatsError::Format("LIM file size overflows".to_string()))?;
    let actual_len = f.metadata().map_err(BioFormatsError::Io)?.len();
    if actual_len < required_len {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "LIM pixel payload is shorter than declared ({actual_len} < {required_len})"
        )));
    }

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z: 1,
        size_c,
        size_t: 1,
        pixel_type,
        bits_per_pixel: (bps * 8) as u8,
        image_count: 1,
        dimension_order: DimensionOrder::XYZCT,
        is_rgb,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        thumbnail: false,
        series_metadata: HashMap::new(),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok((meta, LIM_PIXELS_OFFSET))
}

impl FormatReader for LimReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("lim"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let (meta, data_offset) = load_lim_header(path)?;
        self.path = Some(path.to_path_buf());
        self.meta = Some(meta);
        self.data_offset = data_offset;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.data_offset = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            Err(BioFormatsError::NotInitialized)
        } else if s == 0 {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
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
        let is_rgb = meta.is_rgb;
        let size_c = meta.size_c as usize;
        let plane_bytes = meta.size_x as usize * meta.size_y as usize * size_c * bps;
        // LIM always reads from the fixed PIXELS_OFFSET (single image plane).
        let file_offset = self.data_offset;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(file_offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;

        // Swap red and blue channels for RGB images (BGR storage), matching
        // LIMReader.openBytes. The swap is per-channel byte-wise (3 channels).
        if is_rgb {
            let i = 0..buf.len() / 3;
            for px in i {
                buf.swap(px * 3, px * 3 + 2);
            }
        }
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
        let meta = self.meta.as_ref().unwrap();
        validate_region(meta, x, y, w, h)?;
        let bps = meta.pixel_type.bytes_per_sample() * meta.size_c as usize;
        let row_bytes = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * bps;
            out.extend_from_slice(&src[s..s + out_row]);
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

fn validate_region(meta: &ImageMetadata, x: u32, y: u32, w: u32, h: u32) -> Result<()> {
    let x2 = x
        .checked_add(w)
        .ok_or_else(|| BioFormatsError::Format("LIM region width overflows".to_string()))?;
    let y2 = y
        .checked_add(h)
        .ok_or_else(|| BioFormatsError::Format("LIM region height overflows".to_string()))?;
    if x2 > meta.size_x || y2 > meta.size_y {
        return Err(BioFormatsError::Format(
            "LIM region is outside image bounds".to_string(),
        ));
    }
    Ok(())
}

// ── TillVision Reader ─────────────────────────────────────────────────────────

pub struct TillVisionReader {
    series: Vec<TillVisionSeries>,
    current_series: usize,
}

#[derive(Clone)]
struct TillVisionSeries {
    pixel_source: TillVisionPixelSource,
    plane_bytes: usize,
    meta: ImageMetadata,
}

#[derive(Clone)]
enum TillVisionPixelSource {
    File { path: PathBuf, data_offset: u64 },
    EmbeddedContents { bytes: Vec<u8>, data_offset: usize },
    EmbeddedDecoded { bytes: Vec<u8> },
}

const TILLVISION_DESCRIPTION_MARKER: &[u8; 6] = b"\0\0\0\0\0\xff";

// TillVisionReader.java image-name markers (MARKER_0..MARKER_3). The first
// initFile loop locates image names by searching for whichever of these markers
// occurs next.
const TILLVISION_MARKER_0: &[u8] = &[0x80, 3, 0];
const TILLVISION_MARKER_1: &[u8] = &[0x81, 3, 0];
const TILLVISION_MARKER_2: &[u8] = &[0x43, 0x49, 0x6d, 0x61, 0x67, 0x65, 0x03, 0x00];
const TILLVISION_MARKER_3: &[u8] = &[0x83, 3, 0];

impl TillVisionReader {
    pub fn new() -> Self {
        TillVisionReader {
            series: Vec::new(),
            current_series: 0,
        }
    }

    fn unsupported() -> BioFormatsError {
        BioFormatsError::UnsupportedFormat(
            "TillVision file contains no supported companion PST/INF pixels, strict raw BFTILLVISIONVWS1 payload, or OLE Root Entry/Contents CImage records".to_string(),
        )
    }
}

impl Default for TillVisionReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for TillVisionReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("vws") || e.eq_ignore_ascii_case("pst"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let series = load_tillvision_series(path)?;
        if series.is_empty() {
            return Err(Self::unsupported());
        }
        self.series = series;
        self.current_series = 0;
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.series.clear();
        self.current_series = 0;
        Ok(())
    }

    fn series_count(&self) -> usize {
        self.series.len()
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.series.is_empty() {
            Err(BioFormatsError::NotInitialized)
        } else if s >= self.series.len() {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            self.current_series = s;
            Ok(())
        }
    }
    fn series(&self) -> usize {
        self.current_series
    }

    fn metadata(&self) -> &ImageMetadata {
        self.series
            .get(self.current_series)
            .map(|series| &series.meta)
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let series = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let meta = &series.meta;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        let plane_bytes = series.plane_bytes;
        let plane_offset = (plane_index as u64)
            .checked_mul(plane_bytes as u64)
            .ok_or_else(|| BioFormatsError::Format("TillVision plane offset overflows".into()))?;
        let mut buf = vec![0u8; plane_bytes];
        match &series.pixel_source {
            TillVisionPixelSource::File { path, data_offset } => {
                let offset = data_offset.checked_add(plane_offset).ok_or_else(|| {
                    BioFormatsError::Format("TillVision plane offset overflows".into())
                })?;
                let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
                f.seek(SeekFrom::Start(offset))
                    .map_err(BioFormatsError::Io)?;
                f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
            }
            TillVisionPixelSource::EmbeddedContents { bytes, data_offset } => {
                let offset = (*data_offset)
                    .checked_add(plane_offset as usize)
                    .ok_or_else(|| {
                        BioFormatsError::Format("TillVision plane offset overflows".into())
                    })?;
                let end = offset.checked_add(plane_bytes).ok_or_else(|| {
                    BioFormatsError::Format("TillVision plane offset overflows".into())
                })?;
                if end > bytes.len() {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "TillVision embedded VWS CImage payload is shorter than declared ({end} > {})",
                        bytes.len()
                    )));
                }
                buf.copy_from_slice(&bytes[offset..end]);
            }
            TillVisionPixelSource::EmbeddedDecoded { bytes } => {
                let offset = plane_offset as usize;
                let end = offset.checked_add(plane_bytes).ok_or_else(|| {
                    BioFormatsError::Format("TillVision plane offset overflows".into())
                })?;
                if end > bytes.len() {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "TillVision decoded embedded VWS CImage payload is shorter than declared ({end} > {})",
                        bytes.len()
                    )));
                }
                buf.copy_from_slice(&bytes[offset..end]);
            }
        }
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
        let meta = self.metadata();
        validate_region(meta, x, y, w, h)?;
        let series = self
            .series
            .get(self.current_series)
            .ok_or(BioFormatsError::NotInitialized)?;
        let pixels = (meta.size_x as usize)
            .checked_mul(meta.size_y as usize)
            .ok_or_else(|| BioFormatsError::Format("TillVision plane size overflows".into()))?;
        let bps = series
            .plane_bytes
            .checked_div(pixels)
            .filter(|bytes| *bytes > 0)
            .ok_or_else(|| BioFormatsError::Format("TillVision plane size is invalid".into()))?;
        let row_bytes = meta.size_x as usize * bps;
        let out_row = w as usize * bps;
        let mut out = Vec::with_capacity(h as usize * out_row);
        for row in 0..h as usize {
            let src = &full[(y as usize + row) * row_bytes..];
            let s = x as usize * bps;
            out.extend_from_slice(&src[s..s + out_row]);
        }
        Ok(out)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.metadata();
        let tw = meta.size_x.min(256);
        let th = meta.size_y.min(256);
        let tx = (meta.size_x - tw) / 2;
        let ty = (meta.size_y - th) / 2;
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{create_lsid, OmeAnnotation, OmeMetadata, OmePlane};

        let series = self.series.get(self.current_series)?;
        let meta = &series.meta;
        let mut ome = OmeMetadata::from_image_metadata(meta);
        let acquisition_date =
            tillvision_metadata_text(meta, "tillvision.acquisition_datetime_iso8601")
                .or_else(|| tillvision_metadata_text(meta, "tillvision.acquisition_datetime"))
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map(str::to_string);

        {
            let image = ome.images.get_mut(0)?;

            if image.name.is_none() {
                if let Some(MetadataValue::String(name)) =
                    meta.series_metadata.get("Info image_name")
                {
                    if !name.trim().is_empty() {
                        image.name = Some(name.clone());
                    }
                }
            }
            if image.name.is_none() {
                if let Some(name) = tillvision_metadata_text(meta, "tillvision.image_name") {
                    if !name.trim().is_empty() {
                        image.name = Some(name.trim().to_string());
                    }
                }
            }

            image.physical_size_x =
                tillvision_positive_metadata_float(meta, "tillvision.physical_size_x_um");
            image.physical_size_y =
                tillvision_positive_metadata_float(meta, "tillvision.physical_size_y_um");
            image.physical_size_z =
                tillvision_positive_metadata_float(meta, "tillvision.physical_size_z_um");
            image.time_increment =
                tillvision_positive_metadata_float(meta, "tillvision.time_increment_seconds");

            for (channel_index, channel) in image.channels.iter_mut().enumerate() {
                let name_key = format!("tillvision.channel.{channel_index}.name");
                if let Some(name) = tillvision_metadata_text(meta, &name_key) {
                    if !name.trim().is_empty() {
                        channel.name = Some(name.trim().to_string());
                    }
                }
                let excitation_key =
                    format!("tillvision.channel.{channel_index}.excitation_wavelength_nm");
                channel.excitation_wavelength =
                    tillvision_positive_metadata_float(meta, &excitation_key);
                let emission_key =
                    format!("tillvision.channel.{channel_index}.emission_wavelength_nm");
                channel.emission_wavelength =
                    tillvision_positive_metadata_float(meta, &emission_key);
            }

            let exposure_time = tillvision_metadata_float(meta, "tillvision.exposure_time_seconds");
            if let Some(exposure_time) = exposure_time {
                image.planes = (0..meta.image_count)
                    .map(|plane| {
                        let (z, c, t) = tillvision_plane_zct(meta, plane);
                        OmePlane {
                            the_z: z,
                            the_c: c,
                            the_t: t,
                            exposure_time: Some(exposure_time),
                            ..Default::default()
                        }
                    })
                    .collect();
            }
        }

        if let Some(acquisition_date) = acquisition_date {
            ome.annotations.push(OmeAnnotation::MapAnnotation {
                id: Some(create_lsid("Annotation:TillVisionAcquisition", &[0])),
                namespace: Some("openmicroscopy.org/bioformats/tillvision-acquisition".into()),
                values: vec![
                    ("Image".into(), create_lsid("Image", &[0])),
                    ("AcquisitionDate".into(), acquisition_date),
                ],
            });
        }

        Some(ome)
    }
}

fn load_tillvision_series(path: &Path) -> Result<Vec<TillVisionSeries>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    let mut pixel_files = Vec::new();

    if ext == "pst" && path.is_file() {
        pixel_files.push(path.to_path_buf());
    } else if ext == "vws" {
        let stem = path
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or_default()
            .to_ascii_lowercase();
        for entry in std::fs::read_dir(parent).map_err(BioFormatsError::Io)? {
            let entry = entry.map_err(BioFormatsError::Io)?;
            let entry_path = entry.path();
            let entry_name = entry.file_name().to_string_lossy().to_ascii_lowercase();
            if entry_path.is_file() && entry_name.ends_with(".pst") {
                pixel_files.push(entry_path);
            } else if entry_path.is_dir()
                && entry_name.ends_with(".pst")
                && (stem.is_empty() || entry_name.starts_with(&stem))
            {
                for sub in std::fs::read_dir(&entry_path).map_err(BioFormatsError::Io)? {
                    let sub = sub.map_err(BioFormatsError::Io)?;
                    let sub_path = sub.path();
                    if sub_path
                        .extension()
                        .and_then(|e| e.to_str())
                        .map(|e| e.eq_ignore_ascii_case("pst"))
                        .unwrap_or(false)
                    {
                        pixel_files.push(sub_path);
                    }
                }
            }
        }
    }

    pixel_files.sort();

    // TillVisionReader.initFile reads the .vws OLE "Root Entry/Contents" stream
    // first, recovering per-series image names (first initFile loop) and the
    // acquisition description text blocks (second initFile loop), then reads the
    // on-disk .pst/.inf files for dimensions. When the entry point is a .vws
    // workspace with on-disk .pst pixels, recover the image names here so they
    // can be associated with the INF-driven series by index, mirroring Java's
    // populateMetadataStore() (imageNames.get(i)).
    let contents_image_names = if ext == "vws" && !pixel_files.is_empty() {
        load_tillvision_vws_contents_image_names(path).unwrap_or_default()
    } else {
        Vec::new()
    };

    let mut series = Vec::new();
    for (series_index, pixel_path) in pixel_files.into_iter().enumerate() {
        let inf_path = pixel_path.with_extension("inf");
        let mut meta = load_tillvision_inf(&inf_path)?;
        let plane_bytes = tillvision_plane_bytes(&meta)?;
        let expected = plane_bytes
            .checked_mul(meta.image_count as usize)
            .ok_or_else(|| BioFormatsError::Format("TillVision pixel size overflows".into()))?;
        let actual = std::fs::metadata(&pixel_path)
            .map_err(BioFormatsError::Io)?
            .len() as usize;
        if actual < expected {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "TillVision PST pixel payload is shorter than declared ({actual} < {expected})"
            )));
        }
        if let Some(image_name) = contents_image_names.get(series_index) {
            if !image_name.trim().is_empty()
                && !meta.series_metadata.contains_key("Info image_name")
            {
                meta.series_metadata.insert(
                    "Info image_name".to_string(),
                    MetadataValue::String(image_name.clone()),
                );
            }
        }
        series.push(TillVisionSeries {
            pixel_source: TillVisionPixelSource::File {
                path: pixel_path,
                data_offset: 0,
            },
            plane_bytes,
            meta,
        });
    }
    if series.is_empty() && ext == "vws" {
        if let Some(embedded) = load_tillvision_embedded_strict_raw(path)? {
            series.push(embedded);
        } else if let Some(mut embedded) = load_tillvision_embedded_native_vws(path)? {
            series.append(&mut embedded);
        }
    }
    Ok(series)
}

/// Faithful translation of the first non-embedded image-name parsing loop in
/// TillVisionReader.initFile (java lines ~341-368): opens the .vws OLE
/// "Root Entry/Contents" stream and walks it with findNextOffset(s) to recover
/// the ordered list of image names. Returns an empty list when the file is not
/// an OLE2 container or has no Contents stream (the on-disk .pst pixels remain
/// usable in that case). The image names are associated with the INF-driven
/// series by index, mirroring Java's populateMetadataStore (imageNames.get(i)).
fn load_tillvision_vws_contents_image_names(path: &Path) -> Result<Vec<String>> {
    let mut header = [0u8; 8];
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    let n = f.read(&mut header).map_err(BioFormatsError::Io)?;
    if !is_ole2_header(&header[..n]) {
        return Ok(Vec::new());
    }

    let mut ole = OleFile::open(path)?;
    if !ole
        .document_list()
        .iter()
        .any(|doc| doc.replace('\\', "/") == "Root Entry/Contents")
    {
        return Ok(Vec::new());
    }
    let s = ole.document_bytes("Root Entry/Contents")?;

    // s.seek(0); the stream-relative cursor mirrors Java's getFilePointer().
    let mut image_names = Vec::new();
    let mut fp: usize = 0;

    let mut lower_bound: usize = 0;
    let mut upper_bound: usize = 0x1000;

    // parse main metadata stream in two steps:
    // first get the list of image names...
    while fp + 2 < s.len() {
        // s.order(false); (big-endian for the name length)
        let Some(next_offset) = tillvision_find_next_offset(&s, fp) else {
            break;
        };
        if next_offset >= s.len() {
            break;
        }
        // s.seek(nextOffset); s.skipBytes(3); len = s.readShort() (big-endian)
        fp = next_offset;
        if fp + 3 + 2 > s.len() {
            break;
        }
        fp += 3;
        let len = i16::from_be_bytes([s[fp], s[fp + 1]]) as i32;
        fp += 2;
        if len <= 0 {
            continue;
        }
        let len = len as usize;
        if fp + len > s.len() {
            break;
        }
        image_names.push(String::from_utf8_lossy(&s[fp..fp + len]).to_string());
        fp += len;

        if fp + 8 >= s.len() {
            break;
        }
        // s.skipBytes(6); s.order(true); len = s.readShort() (little-endian)
        fp += 6;
        let skip_len = i16::from_le_bytes([s[fp], s[fp + 1]]) as i32;
        fp += 2;
        if image_names.len() == 1
            && skip_len > (upper_bound as i32) * 2
            && skip_len < (upper_bound as i32) * 4
        {
            lower_bound = 512;
            upper_bound = 0x4000;
        }
        if skip_len < lower_bound as i32 || skip_len > upper_bound as i32 {
            continue;
        }
        fp += skip_len as usize;
    }

    Ok(image_names)
}

/// Faithful translation of TillVisionReader.findNextOffset(s) (java lines
/// ~634-662): searches for whichever of MARKER_0..MARKER_3 occurs next from the
/// current file pointer and returns the offset just past the matched marker, or
/// None when none of the markers are found.
fn tillvision_find_next_offset(s: &[u8], fp: usize) -> Option<usize> {
    let offset0 = tillvision_find_next_offset_marker(s, fp, TILLVISION_MARKER_0);
    let offset1 = tillvision_find_next_offset_marker(s, fp, TILLVISION_MARKER_1);
    let offset2 = tillvision_find_next_offset_marker(s, fp, TILLVISION_MARKER_2);
    let offset3 = tillvision_find_next_offset_marker(s, fp, TILLVISION_MARKER_3);

    let offset0 = offset0.unwrap_or(usize::MAX);
    let offset1 = offset1.unwrap_or(usize::MAX);
    let offset2 = offset2.unwrap_or(usize::MAX);
    let offset3 = offset3.unwrap_or(usize::MAX);

    if offset0 < offset1 && offset0 < offset2 && offset0 < offset3 {
        return Some(offset0);
    }
    if offset1 < offset0 && offset1 < offset2 && offset1 < offset3 {
        return Some(offset1);
    }
    if offset2 < offset1 && offset2 < offset0 && offset2 < offset3 {
        return Some(offset2);
    }
    if offset3 < offset1 && offset3 < offset0 && offset3 < offset2 {
        return Some(offset3);
    }
    None
}

/// Faithful translation of TillVisionReader.findNextOffset(s, marker) (java
/// lines ~664-679): scans from the current file pointer for the marker byte
/// sequence and returns the offset just past it, or None when not found.
fn tillvision_find_next_offset_marker(s: &[u8], fp: usize, marker: &[u8]) -> Option<usize> {
    if marker.is_empty() || s.len() < marker.len() {
        return None;
    }
    let mut i = fp;
    while i < s.len() - marker.len() {
        let mut found = true;
        for q in 0..marker.len() {
            if marker[q] != s[i + q] {
                found = false;
                break;
            }
        }
        if found {
            return Some(i + marker.len());
        }
        i += 1;
    }
    None
}

fn load_tillvision_inf(path: &Path) -> Result<ImageMetadata> {
    let text = std::fs::read_to_string(path).map_err(BioFormatsError::Io)?;
    let mut values = HashMap::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with(';') || line.starts_with('[') {
            continue;
        }
        if let Some((key, value)) = line.split_once('=') {
            values.insert(key.trim().to_ascii_lowercase(), value.trim().to_string());
        }
    }

    let int_value = |key: &str| -> Result<u32> {
        values
            .get(&key.to_ascii_lowercase())
            .ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!("TillVision INF missing {key}"))
            })?
            .parse::<u32>()
            .map_err(|_| {
                BioFormatsError::UnsupportedFormat(format!("TillVision INF invalid {key}"))
            })
    };

    let size_x = int_value("Width")?;
    let size_y = int_value("Height")?;
    let size_c = int_value("Bands")?;
    let size_z = int_value("Slices")?;
    let size_t = int_value("Frames")?;
    let datatype = int_value("Datatype")?;
    if size_x == 0 || size_y == 0 || size_c == 0 || size_z == 0 || size_t == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "TillVision INF dimensions and counts must be positive".into(),
        ));
    }
    let pixel_type = tillvision_pixel_type(datatype)?;
    let image_count = size_z
        .checked_mul(size_t)
        .ok_or_else(|| BioFormatsError::Format("TillVision image count overflows".into()))?;

    let mut series_metadata: HashMap<String, MetadataValue> = values
        .iter()
        .map(|(k, v)| (format!("Info {k}"), MetadataValue::String(v.clone())))
        .collect();
    add_tillvision_normalized_metadata(&mut series_metadata, &values)?;

    Ok(ImageMetadata {
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        pixel_type,
        bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb: false,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        thumbnail: false,
        series_metadata,
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    })
}

fn tillvision_pixel_type(datatype: u32) -> Result<PixelType> {
    let signed = datatype % 2 == 1;
    let bytes = datatype / 2 + u32::from(signed);
    match (bytes, signed) {
        (1, false) => Ok(PixelType::Uint8),
        (1, true) => Ok(PixelType::Int8),
        (2, false) => Ok(PixelType::Uint16),
        (2, true) => Ok(PixelType::Int16),
        (4, false) => Ok(PixelType::Uint32),
        (4, true) => Ok(PixelType::Int32),
        _ => Err(BioFormatsError::UnsupportedFormat(format!(
            "TillVision datatype {datatype} is not supported"
        ))),
    }
}

fn tillvision_plane_bytes(meta: &ImageMetadata) -> Result<usize> {
    meta.size_x
        .checked_mul(meta.size_y)
        .and_then(|px| px.checked_mul(meta.size_c))
        .and_then(|samples| samples.checked_mul(meta.pixel_type.bytes_per_sample() as u32))
        .map(|n| n as usize)
        .ok_or_else(|| BioFormatsError::Format("TillVision plane size overflows".into()))
}

const TILLVISION_VWS_STRICT_RAW_MAGIC: &[u8; 16] = b"BFTILLVISIONVWS1";
const TILLVISION_VWS_STRICT_RAW_HEADER_LEN: usize = 40;

fn read_le_u16(buf: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes([buf[offset], buf[offset + 1]])
}

fn read_le_u32(buf: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
    ])
}

fn read_le_u64(buf: &[u8], offset: usize) -> u64 {
    u64::from_le_bytes([
        buf[offset],
        buf[offset + 1],
        buf[offset + 2],
        buf[offset + 3],
        buf[offset + 4],
        buf[offset + 5],
        buf[offset + 6],
        buf[offset + 7],
    ])
}

fn tillvision_strict_raw_pixel_type(code: u16) -> Result<PixelType> {
    match code {
        1 => Ok(PixelType::Uint8),
        2 => Ok(PixelType::Uint16),
        3 => Ok(PixelType::Float32),
        _ => Err(BioFormatsError::Format(format!(
            "TillVision embedded VWS strict raw subset has unsupported pixel type code {code}"
        ))),
    }
}

fn load_tillvision_embedded_strict_raw(path: &Path) -> Result<Option<TillVisionSeries>> {
    let data = std::fs::read(path).map_err(BioFormatsError::Io)?;
    if data.len() < TILLVISION_VWS_STRICT_RAW_MAGIC.len() {
        return Ok(None);
    }
    if &data[..TILLVISION_VWS_STRICT_RAW_MAGIC.len()] != TILLVISION_VWS_STRICT_RAW_MAGIC {
        return Ok(None);
    }
    if data.len() < TILLVISION_VWS_STRICT_RAW_HEADER_LEN {
        return Err(BioFormatsError::Format(
            "TillVision embedded VWS strict raw subset header is truncated".into(),
        ));
    }

    let size_x = read_le_u32(&data, 16);
    let size_y = read_le_u32(&data, 20);
    let image_count = read_le_u32(&data, 24);
    let pixel_type_code = read_le_u16(&data, 28);
    let reserved = read_le_u16(&data, 30);
    let data_offset = read_le_u64(&data, 32);

    if size_x == 0 || size_y == 0 || image_count == 0 {
        return Err(BioFormatsError::Format(
            "TillVision embedded VWS strict raw subset dimensions must be non-zero".into(),
        ));
    }
    if reserved != 0 {
        return Err(BioFormatsError::Format(
            "TillVision embedded VWS strict raw subset reserved header bytes must be zero".into(),
        ));
    }
    if data_offset != TILLVISION_VWS_STRICT_RAW_HEADER_LEN as u64 {
        return Err(BioFormatsError::Format(format!(
            "TillVision embedded VWS strict raw subset data offset must equal {TILLVISION_VWS_STRICT_RAW_HEADER_LEN}"
        )));
    }

    let pixel_type = tillvision_strict_raw_pixel_type(pixel_type_code)?;
    let plane_bytes = (size_x as usize)
        .checked_mul(size_y as usize)
        .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample()))
        .ok_or_else(|| {
            BioFormatsError::Format(
                "TillVision embedded VWS strict raw subset plane size overflows".into(),
            )
        })?;
    let payload_bytes = plane_bytes
        .checked_mul(image_count as usize)
        .ok_or_else(|| {
            BioFormatsError::Format(
                "TillVision embedded VWS strict raw subset payload size overflows".into(),
            )
        })?;
    let expected_len = TILLVISION_VWS_STRICT_RAW_HEADER_LEN
        .checked_add(payload_bytes)
        .ok_or_else(|| {
            BioFormatsError::Format(
                "TillVision embedded VWS strict raw subset file size overflows".into(),
            )
        })?;
    if data.len() != expected_len {
        return Err(BioFormatsError::Format(format!(
            "TillVision embedded VWS strict raw subset payload length mismatch: got {} bytes, expected {expected_len}",
            data.len()
        )));
    }

    let meta = ImageMetadata {
        size_x,
        size_y,
        size_z: 1,
        size_c: 1,
        size_t: image_count,
        pixel_type,
        bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
        image_count,
        dimension_order: DimensionOrder::XYCZT,
        is_rgb: false,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: true,
        resolution_count: 1,
        thumbnail: false,
        series_metadata: HashMap::from([(
            "Info embedded_vws_fallback".to_string(),
            MetadataValue::String("strict-raw".to_string()),
        )]),
        lookup_table: None,
        modulo_z: None,
        modulo_c: None,
        modulo_t: None,
    };

    Ok(Some(TillVisionSeries {
        pixel_source: TillVisionPixelSource::File {
            path: path.to_path_buf(),
            data_offset,
        },
        plane_bytes,
        meta,
    }))
}

fn load_tillvision_embedded_native_vws(path: &Path) -> Result<Option<Vec<TillVisionSeries>>> {
    let mut header = [0u8; 8];
    let mut f = std::fs::File::open(path).map_err(BioFormatsError::Io)?;
    let n = f.read(&mut header).map_err(BioFormatsError::Io)?;
    if !is_ole2_header(&header[..n]) {
        return Ok(None);
    }

    let mut ole = OleFile::open(path)?;
    let documents = ole.document_list();
    if !documents
        .iter()
        .any(|doc| doc.replace('\\', "/") == "Root Entry/Contents")
    {
        return Err(BioFormatsError::UnsupportedFormat(
            "TillVision embedded VWS native file is OLE2 but lacks Root Entry/Contents".into(),
        ));
    }
    let contents = ole.document_bytes("Root Entry/Contents")?;
    let records = parse_tillvision_cimage_records(&contents)?;
    if records.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "TillVision embedded VWS native Root Entry/Contents contains no supported CImage records"
                .into(),
        ));
    }
    let descriptions = parse_tillvision_description_blocks(&contents);

    let mut series = Vec::with_capacity(records.len());
    for (series_index, record) in records.into_iter().enumerate() {
        let pixel_type = tillvision_pixel_type(record.datatype)?;
        let plane_bytes = (record.size_x as usize)
            .checked_mul(record.size_y as usize)
            .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample()))
            .ok_or_else(|| {
                BioFormatsError::Format("TillVision embedded CImage plane size overflows".into())
            })?;
        let image_count = record
            .size_z
            .checked_mul(record.size_c)
            .and_then(|zc| zc.checked_mul(record.size_t))
            .ok_or_else(|| {
                BioFormatsError::Format("TillVision embedded CImage image count overflows".into())
            })?;
        if image_count == 0 || record.size_x == 0 || record.size_y == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "TillVision embedded CImage dimensions and counts must be positive".into(),
            ));
        }
        let payload_bytes = plane_bytes
            .checked_mul(image_count as usize)
            .ok_or_else(|| {
                BioFormatsError::Format("TillVision embedded CImage payload size overflows".into())
            })?;
        let description = descriptions.get(series_index);
        let compression = tillvision_cimage_compression(description)?;
        let mut fragment_table =
            tillvision_cimage_fragment_table(description, record.data_offset, contents.len())?;
        if fragment_table.is_none() && description.is_none() {
            fragment_table = infer_tillvision_cimage_binary_fragment_table_without_description(
                &contents,
                record.data_offset,
                payload_bytes,
            )?;
        }
        if fragment_table.is_none() && description.is_none() {
            fragment_table = infer_tillvision_cimage_fragment_table_without_description(
                &contents,
                record.data_offset,
                payload_bytes,
            )?;
        }
        let data_offset = if fragment_table.is_none() {
            tillvision_cimage_payload_offset(description, record.data_offset)?
        } else {
            record.data_offset
        };
        let encoded_payload = if let Some(fragments) = fragment_table.as_ref() {
            let total = fragments.iter().try_fold(0usize, |sum, (_, len)| {
                sum.checked_add(*len).ok_or_else(|| {
                    BioFormatsError::Format(
                        "TillVision embedded CImage fragment table size overflows".into(),
                    )
                })
            })?;
            let mut payload = Vec::with_capacity(total);
            for &(offset, len) in fragments {
                let end = offset.checked_add(len).ok_or_else(|| {
                    BioFormatsError::Format(
                        "TillVision embedded CImage fragment table size overflows".into(),
                    )
                })?;
                if end > contents.len() {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "TillVision embedded CImage fragment {offset}:{len} exceeds Contents length {}",
                        contents.len()
                    )));
                }
                payload.extend_from_slice(&contents[offset..end]);
            }
            Some(payload)
        } else {
            None
        };
        let pixel_source = match compression {
            TillVisionCImageCompression::Uncompressed => {
                if let Some(payload) = encoded_payload {
                    if payload.len() != payload_bytes {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "TillVision embedded CImage fragments assemble to {} bytes, expected {payload_bytes}",
                            payload.len()
                        )));
                    }
                    TillVisionPixelSource::EmbeddedDecoded { bytes: payload }
                } else {
                    let expected = data_offset.checked_add(payload_bytes).ok_or_else(|| {
                        BioFormatsError::Format(
                            "TillVision embedded CImage payload size overflows".into(),
                        )
                    })?;
                    if expected > contents.len() {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "TillVision embedded VWS CImage payload is shorter than declared ({expected} > {})",
                            contents.len()
                        )));
                    }
                    TillVisionPixelSource::EmbeddedContents {
                        bytes: contents.clone(),
                        data_offset,
                    }
                }
            }
            TillVisionCImageCompression::Zlib => {
                let decoded = if let Some(payload) = encoded_payload {
                    crate::common::codec::decompress_deflate(&payload).map_err(|err| {
                        BioFormatsError::UnsupportedFormat(format!(
                            "TillVision embedded compressed CImage zlib payload could not be decompressed: {err}"
                        ))
                    })?
                } else {
                    if data_offset >= contents.len() {
                        return Err(BioFormatsError::UnsupportedFormat(
                            "TillVision embedded compressed CImage payload is missing".into(),
                        ));
                    }
                    crate::common::codec::decompress_deflate(&contents[data_offset..]).map_err(
                        |err| {
                            BioFormatsError::UnsupportedFormat(format!(
                                "TillVision embedded compressed CImage zlib payload could not be decompressed: {err}"
                            ))
                        },
                    )?
                };
                if decoded.len() != payload_bytes {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "TillVision embedded compressed CImage decoded to {} bytes, expected {payload_bytes}",
                        decoded.len()
                    )));
                }
                TillVisionPixelSource::EmbeddedDecoded { bytes: decoded }
            }
            TillVisionCImageCompression::RawDeflate => {
                let decoded = if let Some(payload) = encoded_payload {
                    crate::common::codec::decompress_deflate_raw(&payload).map_err(|err| {
                        BioFormatsError::UnsupportedFormat(format!(
                            "TillVision embedded compressed CImage raw deflate payload could not be decompressed: {err}"
                        ))
                    })?
                } else {
                    if data_offset >= contents.len() {
                        return Err(BioFormatsError::UnsupportedFormat(
                            "TillVision embedded compressed CImage payload is missing".into(),
                        ));
                    }
                    crate::common::codec::decompress_deflate_raw(&contents[data_offset..])
                        .map_err(|err| {
                            BioFormatsError::UnsupportedFormat(format!(
                                "TillVision embedded compressed CImage raw deflate payload could not be decompressed: {err}"
                            ))
                        })?
                };
                if decoded.len() != payload_bytes {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "TillVision embedded compressed CImage decoded to {} bytes, expected {payload_bytes}",
                        decoded.len()
                    )));
                }
                TillVisionPixelSource::EmbeddedDecoded { bytes: decoded }
            }
            TillVisionCImageCompression::Gzip => {
                let decoded = if let Some(payload) = encoded_payload {
                    decompress_tillvision_cimage_gzip(&payload).map_err(|err| {
                        BioFormatsError::UnsupportedFormat(format!(
                            "TillVision embedded compressed CImage gzip payload could not be decompressed: {err}"
                        ))
                    })?
                } else {
                    if data_offset >= contents.len() {
                        return Err(BioFormatsError::UnsupportedFormat(
                            "TillVision embedded compressed CImage payload is missing".into(),
                        ));
                    }
                    decompress_tillvision_cimage_gzip(&contents[data_offset..]).map_err(|err| {
                        BioFormatsError::UnsupportedFormat(format!(
                            "TillVision embedded compressed CImage gzip payload could not be decompressed: {err}"
                        ))
                    })?
                };
                if decoded.len() != payload_bytes {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "TillVision embedded compressed CImage decoded to {} bytes, expected {payload_bytes}",
                        decoded.len()
                    )));
                }
                TillVisionPixelSource::EmbeddedDecoded { bytes: decoded }
            }
        };

        let mut series_metadata = HashMap::from([(
            "Info embedded_vws".to_string(),
            MetadataValue::String("Root Entry/Contents CImage".to_string()),
        )]);
        if !record.name.is_empty() {
            series_metadata.insert(
                "Info image_name".to_string(),
                MetadataValue::String(record.name),
            );
        }
        series_metadata.insert(
            "Info cimage_layout".to_string(),
            MetadataValue::String(record.layout.metadata_value().to_string()),
        );
        if let Some(description) = description {
            add_tillvision_description_metadata(&mut series_metadata, description)?;
        }
        if let Some(fragments) = fragment_table.as_ref() {
            if description.is_none() {
                let value = fragments
                    .iter()
                    .map(|(offset, len)| format!("{offset}:{len}"))
                    .collect::<Vec<_>>()
                    .join(", ");
                series_metadata.insert(
                    "Info inferred_payload_fragments".to_string(),
                    MetadataValue::String(value),
                );
            }
        }

        series.push(TillVisionSeries {
            pixel_source,
            plane_bytes,
            meta: ImageMetadata {
                size_x: record.size_x,
                size_y: record.size_y,
                size_z: record.size_z,
                size_c: record.size_c,
                size_t: record.size_t,
                pixel_type,
                bits_per_pixel: (pixel_type.bytes_per_sample() * 8) as u8,
                image_count,
                dimension_order: DimensionOrder::XYCZT,
                is_rgb: false,
                is_interleaved: false,
                is_indexed: false,
                is_little_endian: true,
                resolution_count: 1,
                thumbnail: false,
                series_metadata,
                lookup_table: None,
                modulo_z: None,
                modulo_c: None,
                modulo_t: None,
            },
        });
    }
    Ok(Some(series))
}

enum TillVisionCImageCompression {
    Uncompressed,
    Zlib,
    RawDeflate,
    Gzip,
}

fn tillvision_cimage_compression(
    description: Option<&HashMap<String, String>>,
) -> Result<TillVisionCImageCompression> {
    let Some(description) = description else {
        return Ok(TillVisionCImageCompression::Uncompressed);
    };

    let mut compressed_flag: Option<(&str, &str)> = None;
    let mut supported_algorithm: Option<&str> = None;
    let mut unsupported: Option<(&str, &str)> = None;

    for (key, value) in description {
        let normalized_key = key.trim().to_ascii_lowercase();
        if normalized_key != "compressed" && !normalized_key.contains("compression") {
            continue;
        }

        let value = value.trim();
        let normalized_value = value.to_ascii_lowercase();
        if normalized_value.is_empty()
            || matches!(
                normalized_value.as_str(),
                "0" | "false" | "no" | "none" | "raw" | "uncompressed" | "not compressed"
            )
        {
            continue;
        }
        if tillvision_cimage_zlib_alias(&normalized_value) {
            supported_algorithm = Some("zlib");
        } else if tillvision_cimage_raw_deflate_alias(&normalized_value) {
            supported_algorithm = Some("deflate");
        } else if tillvision_cimage_gzip_alias(&normalized_value) {
            supported_algorithm = Some("gzip");
        } else if normalized_key == "compressed"
            && matches!(normalized_value.as_str(), "1" | "true" | "yes")
        {
            compressed_flag = Some((key.as_str(), value));
        } else {
            unsupported = Some((key.as_str(), value));
        }
    }

    if let Some((key, value)) = unsupported {
        Err(BioFormatsError::UnsupportedFormat(format!(
            "TillVision embedded CImage declares unsupported compression {key}: {value}"
        )))
    } else if let Some(algorithm) = supported_algorithm {
        match algorithm {
            "deflate" => Ok(TillVisionCImageCompression::RawDeflate),
            "gzip" => Ok(TillVisionCImageCompression::Gzip),
            _ => Ok(TillVisionCImageCompression::Zlib),
        }
    } else if let Some((key, value)) = compressed_flag {
        Err(BioFormatsError::UnsupportedFormat(format!(
            "TillVision embedded CImage declares compressed payload without a supported algorithm {key}: {value}"
        )))
    } else {
        Ok(TillVisionCImageCompression::Uncompressed)
    }
}

fn tillvision_cimage_zlib_alias(value: &str) -> bool {
    matches!(
        tillvision_normalize_token(value).as_str(),
        "zlib"
            | "zlibcompressed"
            | "zlibdeflate"
            | "zlibwrappeddeflate"
            | "deflatezlib"
            | "deflatezlibwrapped"
            | "zip"
            | "zipcompressed"
            | "zipdeflate"
            | "rfc1950"
    )
}

fn tillvision_cimage_raw_deflate_alias(value: &str) -> bool {
    matches!(
        tillvision_normalize_token(value).as_str(),
        "deflate"
            | "deflatenowrap"
            | "nowrapdeflate"
            | "rawdeflate"
            | "rawrfc1951"
            | "deflateraw"
            | "rfc1951"
    )
}

fn tillvision_cimage_gzip_alias(value: &str) -> bool {
    matches!(
        tillvision_normalize_token(value).as_str(),
        "gz" | "gzip"
            | "gzipped"
            | "gzipcompressed"
            | "gzipdeflate"
            | "gzipwrappeddeflate"
            | "deflategzip"
            | "rfc1952"
    )
}

fn decompress_tillvision_cimage_gzip(data: &[u8]) -> std::io::Result<Vec<u8>> {
    let mut decoder = flate2::read::GzDecoder::new(data);
    let mut out = Vec::new();
    decoder.read_to_end(&mut out)?;
    Ok(out)
}

fn tillvision_normalize_token(value: &str) -> String {
    value
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

fn tillvision_cimage_payload_offset(
    description: Option<&HashMap<String, String>>,
    default_offset: usize,
) -> Result<usize> {
    let Some(description) = description else {
        return Ok(default_offset);
    };
    let Some((key, value)) = description.iter().find(|(key, _)| {
        let key = key.trim().to_ascii_lowercase();
        key.contains("offset")
            && (key.contains("payload") || key.contains("pixel") || key.contains("data"))
    }) else {
        return Ok(default_offset);
    };
    let offset = parse_tillvision_usize_value(value).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "TillVision embedded CImage has invalid payload offset {key}: {value}"
        ))
    })?;
    if offset < default_offset {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "TillVision embedded CImage declares payload offset {offset} before parsed payload start {default_offset}"
        )));
    }
    Ok(offset)
}

fn tillvision_cimage_fragment_table(
    description: Option<&HashMap<String, String>>,
    default_offset: usize,
    contents_len: usize,
) -> Result<Option<Vec<(usize, usize)>>> {
    let Some(description) = description else {
        return Ok(None);
    };

    let inline = description.iter().find(|(key, _)| {
        let key = key.trim().to_ascii_lowercase();
        tillvision_fragment_table_key(&key)
            || (tillvision_fragment_key(&key) && tillvision_payload_key(&key))
    });
    if let Some((key, value)) = inline {
        let fragments = parse_tillvision_fragment_pairs(value).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!(
                "TillVision embedded CImage has invalid fragment table {key}: {value}"
            ))
        })?;
        return validate_tillvision_fragments(fragments, default_offset, contents_len).map(Some);
    }

    let offsets = description.iter().find(|(key, _)| {
        let key = key.trim().to_ascii_lowercase();
        tillvision_fragment_key(&key) && tillvision_fragment_offset_key(&key)
    });
    let lengths = description.iter().find(|(key, _)| {
        let key = key.trim().to_ascii_lowercase();
        (tillvision_fragment_key(&key) && tillvision_fragment_length_key(&key))
            || ((tillvision_fragment_key(&key) || tillvision_payload_key(&key))
                && tillvision_fragment_length_key(&key))
    });
    let ends = description.iter().find(|(key, _)| {
        let key = key.trim().to_ascii_lowercase();
        (tillvision_fragment_key(&key) && tillvision_fragment_end_key(&key))
            || ((tillvision_fragment_key(&key) || tillvision_payload_key(&key))
                && tillvision_fragment_end_key(&key))
    });
    match (offsets, lengths, ends) {
        (Some((offset_key, offset_value)), Some((length_key, length_value)), _) => {
            let offsets = parse_tillvision_usize_list(offset_value).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!(
                    "TillVision embedded CImage has invalid fragment offsets {offset_key}: {offset_value}"
                ))
            })?;
            let lengths = parse_tillvision_usize_list(length_value).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!(
                    "TillVision embedded CImage has invalid fragment lengths {length_key}: {length_value}"
                ))
            })?;
            if offsets.len() != lengths.len() {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "TillVision embedded CImage fragment offset/length count mismatch: {} offsets, {} lengths",
                    offsets.len(),
                    lengths.len()
                )));
            }
            let fragments = offsets.into_iter().zip(lengths).collect();
            validate_tillvision_fragments(fragments, default_offset, contents_len).map(Some)
        }
        (Some((offset_key, offset_value)), None, Some((end_key, end_value))) => {
            let offsets = parse_tillvision_usize_list(offset_value).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!(
                    "TillVision embedded CImage has invalid fragment offsets {offset_key}: {offset_value}"
                ))
            })?;
            let ends = parse_tillvision_usize_list(end_value).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!(
                    "TillVision embedded CImage has invalid fragment ends {end_key}: {end_value}"
                ))
            })?;
            if offsets.len() != ends.len() {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "TillVision embedded CImage fragment offset/end count mismatch: {} offsets, {} ends",
                    offsets.len(),
                    ends.len()
                )));
            }
            let fragments = offsets
                .into_iter()
                .zip(ends)
                .map(|(offset, end)| {
                    end.checked_sub(offset)
                        .map(|len| (offset, len))
                        .ok_or_else(|| {
                            BioFormatsError::UnsupportedFormat(format!(
                                "TillVision embedded CImage fragment end {end} before offset {offset}"
                            ))
                        })
                })
                .collect::<Result<Vec<_>>>()?;
            validate_tillvision_fragments(fragments, default_offset, contents_len).map(Some)
        }
        (Some((key, _)), None, None) | (None, Some((key, _)), _) | (None, None, Some((key, _))) => {
            Err(BioFormatsError::UnsupportedFormat(format!(
                "TillVision embedded CImage declares incomplete fragment table at {key}"
            )))
        }
        (None, None, None) => Ok(None),
    }
}

fn tillvision_fragment_key(key: &str) -> bool {
    key.contains("fragment") || key.contains("block") || key.contains("chunk")
}

fn tillvision_payload_key(key: &str) -> bool {
    key.contains("payload") || key.contains("pixel") || key.contains("data")
}

fn tillvision_fragment_table_key(key: &str) -> bool {
    tillvision_fragment_key(key)
        && (key.contains("table")
            || key.contains("list")
            || key.contains("map")
            || key.contains("descriptor"))
}

fn tillvision_fragment_offset_key(key: &str) -> bool {
    key.contains("offset") || key.contains("start") || key.contains("position")
}

fn tillvision_fragment_length_key(key: &str) -> bool {
    key.contains("length")
        || key.contains("size")
        || key.contains("byte count")
        || key.contains("bytecount")
        || key.contains("count")
        || key.contains("bytes")
}

fn tillvision_fragment_end_key(key: &str) -> bool {
    !key.contains("endian") && (key.contains("end") || key.contains("stop"))
}

fn infer_tillvision_cimage_binary_fragment_table_without_description(
    contents: &[u8],
    default_offset: usize,
    payload_bytes: usize,
) -> Result<Option<Vec<(usize, usize)>>> {
    if let Some(fragments) = infer_tillvision_cimage_binary_fragment_table_with_pair_width(
        contents,
        default_offset,
        payload_bytes,
        8,
        |bytes, offset| Some(read_le_u32(bytes, offset) as usize),
        TillVisionBinaryFragmentPairLayout::OffsetLength,
    )? {
        return Ok(Some(fragments));
    }
    if let Some(fragments) = infer_tillvision_cimage_binary_fragment_table_with_pair_width(
        contents,
        default_offset,
        payload_bytes,
        16,
        read_le_u64_as_usize,
        TillVisionBinaryFragmentPairLayout::OffsetLength,
    )? {
        return Ok(Some(fragments));
    }
    if let Some(fragments) = infer_tillvision_cimage_binary_fragment_table_with_pair_width(
        contents,
        default_offset,
        payload_bytes,
        8,
        |bytes, offset| Some(read_le_u32(bytes, offset) as usize),
        TillVisionBinaryFragmentPairLayout::OffsetEnd,
    )? {
        return Ok(Some(fragments));
    }
    infer_tillvision_cimage_binary_fragment_table_with_pair_width(
        contents,
        default_offset,
        payload_bytes,
        16,
        read_le_u64_as_usize,
        TillVisionBinaryFragmentPairLayout::OffsetEnd,
    )
}

#[derive(Debug, Clone, Copy)]
enum TillVisionBinaryFragmentPairLayout {
    OffsetLength,
    OffsetEnd,
}

fn infer_tillvision_cimage_binary_fragment_table_with_pair_width(
    contents: &[u8],
    default_offset: usize,
    payload_bytes: usize,
    pair_width: usize,
    read_value: fn(&[u8], usize) -> Option<usize>,
    layout: TillVisionBinaryFragmentPairLayout,
) -> Result<Option<Vec<(usize, usize)>>> {
    let Some(header_end) = default_offset.checked_add(4) else {
        return Err(BioFormatsError::Format(
            "TillVision embedded CImage binary fragment table overflows".into(),
        ));
    };
    if header_end > contents.len() {
        return Ok(None);
    }

    let count = read_le_u32(contents, default_offset) as usize;
    if count == 0 || count > 4096 {
        return Ok(None);
    }
    let table_bytes = count
        .checked_mul(pair_width)
        .and_then(|n| n.checked_add(4))
        .ok_or_else(|| {
            BioFormatsError::Format(
                "TillVision embedded CImage binary fragment table size overflows".into(),
            )
        })?;
    let table_end = default_offset.checked_add(table_bytes).ok_or_else(|| {
        BioFormatsError::Format(
            "TillVision embedded CImage binary fragment table end overflows".into(),
        )
    })?;
    if table_end > contents.len() {
        return Ok(None);
    }

    let mut fragments = Vec::with_capacity(count);
    let mut total = 0usize;
    for index in 0..count {
        let pair_offset = default_offset + 4 + index * pair_width;
        let value_width = pair_width / 2;
        let Some(offset) = read_value(contents, pair_offset) else {
            return Ok(None);
        };
        let Some(second) = read_value(contents, pair_offset + value_width) else {
            return Ok(None);
        };
        let Some(len) = (match layout {
            TillVisionBinaryFragmentPairLayout::OffsetLength => Some(second),
            TillVisionBinaryFragmentPairLayout::OffsetEnd => second.checked_sub(offset),
        }) else {
            return Ok(None);
        };
        if len == 0 || offset < table_end {
            return Ok(None);
        }
        total = total.checked_add(len).ok_or_else(|| {
            BioFormatsError::Format(
                "TillVision embedded CImage binary fragment table size overflows".into(),
            )
        })?;
        fragments.push((offset, len));
    }
    if total != payload_bytes {
        return Ok(None);
    }

    validate_tillvision_fragments(fragments, default_offset, contents.len()).map(Some)
}

fn read_le_u64_as_usize(bytes: &[u8], offset: usize) -> Option<usize> {
    usize::try_from(read_le_u64(bytes, offset)).ok()
}

fn infer_tillvision_cimage_fragment_table_without_description(
    contents: &[u8],
    default_offset: usize,
    payload_bytes: usize,
) -> Result<Option<Vec<(usize, usize)>>> {
    let default_end = default_offset.checked_add(payload_bytes).ok_or_else(|| {
        BioFormatsError::Format("TillVision embedded CImage payload size overflows".into())
    })?;
    if default_end > contents.len() {
        return Ok(None);
    }
    let default_payload = &contents[default_offset..default_end];
    if default_payload.is_empty() || default_payload.iter().any(|byte| *byte != 0xaa) {
        return Ok(None);
    }

    let mut fragments = Vec::new();
    let mut offset = default_end;
    while offset < contents.len() {
        while offset < contents.len() && contents[offset] == 0xaa {
            offset += 1;
        }
        if offset >= contents.len() {
            break;
        }
        let start = offset;
        while offset < contents.len() && contents[offset] != 0xaa {
            offset += 1;
        }
        fragments.push((start, offset - start));
    }
    if fragments.is_empty() {
        return Ok(None);
    }

    let total = fragments.iter().try_fold(0usize, |sum, (_, len)| {
        sum.checked_add(*len).ok_or_else(|| {
            BioFormatsError::Format(
                "TillVision embedded CImage inferred fragment table size overflows".into(),
            )
        })
    })?;
    if total != payload_bytes {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "TillVision embedded CImage payload appears noncontiguous without description metadata, but inferred non-padding fragments assemble to {total} bytes, expected {payload_bytes}"
        )));
    }

    validate_tillvision_fragments(fragments, default_offset, contents.len()).map(Some)
}

fn parse_tillvision_fragment_pairs(value: &str) -> Option<Vec<(usize, usize)>> {
    let mut fragments = Vec::new();
    let mut saw_unpaired_token = false;
    for part in value
        .split(|ch: char| ch == ',' || ch == ';' || ch.is_ascii_whitespace())
        .filter(|part| !part.trim().is_empty())
    {
        let Some((offset, len)) = part
            .split_once(':')
            .or_else(|| part.split_once('+'))
            .or_else(|| part.split_once('@'))
        else {
            saw_unpaired_token = true;
            continue;
        };
        fragments.push((
            parse_tillvision_usize_token(offset)?,
            parse_tillvision_usize_token(len)?,
        ));
    }
    if !fragments.is_empty() {
        if saw_unpaired_token {
            return None;
        }
        return Some(fragments);
    }

    let values = parse_tillvision_usize_list(value)?;
    if values.len() < 2 || values.len() % 2 != 0 {
        return None;
    }
    Some(
        values
            .chunks_exact(2)
            .map(|pair| (pair[0], pair[1]))
            .collect(),
    )
}

fn parse_tillvision_usize_list(value: &str) -> Option<Vec<usize>> {
    let mut values = Vec::new();
    for token in value
        .split(|ch: char| {
            ch == ','
                || ch == ';'
                || ch == '['
                || ch == ']'
                || ch == '('
                || ch == ')'
                || ch.is_ascii_whitespace()
        })
        .filter(|part| !part.trim().is_empty())
    {
        values.push(parse_tillvision_usize_token(token)?);
    }
    if values.is_empty() {
        None
    } else {
        Some(values)
    }
}

fn parse_tillvision_usize_value(value: &str) -> Option<usize> {
    let token = value
        .trim()
        .split(|ch: char| ch.is_ascii_whitespace() || ch == ',')
        .find(|part| !part.is_empty())?;
    parse_tillvision_usize_token(token)
}

fn parse_tillvision_usize_token(token: &str) -> Option<usize> {
    let token = token.trim();
    if let Some(hex) = token
        .strip_prefix("0x")
        .or_else(|| token.strip_prefix("0X"))
    {
        usize::from_str_radix(hex, 16).ok()
    } else {
        token.parse().ok()
    }
}

fn validate_tillvision_fragments(
    fragments: Vec<(usize, usize)>,
    default_offset: usize,
    contents_len: usize,
) -> Result<Vec<(usize, usize)>> {
    if fragments.is_empty() {
        return Err(BioFormatsError::UnsupportedFormat(
            "TillVision embedded CImage fragment table is empty".into(),
        ));
    }
    for &(offset, len) in &fragments {
        if len == 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "TillVision embedded CImage fragment table contains a zero-length fragment".into(),
            ));
        }
        if offset < default_offset {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "TillVision embedded CImage fragment offset {offset} before parsed payload start {default_offset}"
            )));
        }
        let end = offset.checked_add(len).ok_or_else(|| {
            BioFormatsError::Format("TillVision embedded CImage fragment end overflows".into())
        })?;
        if end > contents_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "TillVision embedded CImage fragment {offset}:{len} exceeds Contents length {contents_len}"
            )));
        }
    }
    Ok(fragments)
}

#[derive(Debug)]
struct TillVisionCImageRecord {
    name: String,
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    datatype: u32,
    data_offset: usize,
    layout: TillVisionCImageLayout,
}

#[derive(Debug, Clone, Copy)]
enum TillVisionCImageLayout {
    Marker,
    ClassNameFixedOffset,
}

impl TillVisionCImageLayout {
    fn metadata_value(self) -> &'static str {
        match self {
            TillVisionCImageLayout::Marker => "marker-sB",
            TillVisionCImageLayout::ClassNameFixedOffset => "class-name-fixed-offset",
        }
    }
}

fn parse_tillvision_cimage_records(contents: &[u8]) -> Result<Vec<TillVisionCImageRecord>> {
    let offsets = find_tillvision_cimage_offsets(contents);
    if !offsets.is_empty() {
        let mut records = Vec::with_capacity(offsets.len());
        for offset in offsets {
            records.push(parse_tillvision_cimage_record(
                contents,
                offset,
                TillVisionCImageLayout::Marker,
            )?);
        }
        return Ok(records);
    }

    if contents.len() >= 21 {
        let len = read_le_u16(contents, 13) as usize;
        let start = 15usize;
        let end = start.saturating_add(len);
        if end <= contents.len() && contents[start..end] == *b"CImage" {
            let offset = end + 6;
            return Ok(vec![parse_tillvision_cimage_record(
                contents,
                offset,
                TillVisionCImageLayout::ClassNameFixedOffset,
            )?]);
        }
    }
    Ok(Vec::new())
}

fn parse_tillvision_cimage_record(
    contents: &[u8],
    offset: usize,
    layout: TillVisionCImageLayout,
) -> Result<TillVisionCImageRecord> {
    if offset >= contents.len() {
        return Err(BioFormatsError::UnsupportedFormat(
            "TillVision embedded CImage record offset is outside Contents".into(),
        ));
    }
    let name_len = contents[offset] as usize;
    let name_start = offset + 1;
    let name_end = name_start.checked_add(name_len).ok_or_else(|| {
        BioFormatsError::Format("TillVision embedded CImage name length overflows".into())
    })?;
    if name_end > contents.len() {
        return Err(BioFormatsError::UnsupportedFormat(
            "TillVision embedded CImage name is truncated".into(),
        ));
    }
    let name = String::from_utf8_lossy(&contents[name_start..name_end]).to_string();
    let dims_start = match layout {
        TillVisionCImageLayout::ClassNameFixedOffset => 1280usize + 20,
        TillVisionCImageLayout::Marker => {
            let marker = find_bytes_from(contents, b"sB", name_end).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(
                    "TillVision embedded CImage record lacks sB dimension marker".into(),
                )
            })?;
            marker + 2 + 20
        }
    };
    let fields_end = dims_start.checked_add(24).ok_or_else(|| {
        BioFormatsError::Format("TillVision embedded CImage dimensions overflow".into())
    })?;
    if fields_end > contents.len() {
        let message = match layout {
            TillVisionCImageLayout::ClassNameFixedOffset => {
                "TillVision embedded class-name CImage layout dimensions are truncated"
            }
            TillVisionCImageLayout::Marker => "TillVision embedded CImage dimensions are truncated",
        };
        return Err(BioFormatsError::UnsupportedFormat(message.into()));
    }
    let size_x = read_le_u32(contents, dims_start);
    let size_y = read_le_u32(contents, dims_start + 4);
    let size_z = read_le_u32(contents, dims_start + 8);
    let size_c = read_le_u32(contents, dims_start + 12);
    let size_t = read_le_u32(contents, dims_start + 16);
    let datatype = read_le_u32(contents, dims_start + 20);
    let data_offset = fields_end
        .checked_add(match layout {
            TillVisionCImageLayout::ClassNameFixedOffset => 27,
            TillVisionCImageLayout::Marker => 31,
        })
        .ok_or_else(|| {
            BioFormatsError::Format("TillVision embedded CImage data offset overflows".into())
        })?;
    Ok(TillVisionCImageRecord {
        name,
        size_x,
        size_y,
        size_z,
        size_c,
        size_t,
        datatype,
        data_offset,
        layout,
    })
}

/// Faithful translation of TillVisionReader.findImages (java lines ~681-722).
/// Java scans the Contents stream for the four-int signature
/// `0xf03fff00, 0, 0, 0xff00` (each read big-endian, i.e. the literal byte
/// sequences below), then computes a fixed pointer (i+22), optionally skips an
/// inline "CImage" class name, and verifies the big-endian `0x08000400` marker.
/// The returned offsets point at the per-image name-length byte. Java's
/// sliding 8192-byte buffer with a 128-byte overlap is collapsed here to a
/// single in-memory scan over `contents` (the OLE stream is already fully
/// resident), which yields the same offsets.
fn find_tillvision_cimage_offsets(contents: &[u8]) -> Vec<usize> {
    let mut offsets = Vec::new();
    if contents.len() < 16 {
        return offsets;
    }
    // i + 22 + 10 (CImage skip) + 4 (0x08000400) + 5 (name length byte + 1) is
    // the furthest position touched; bound the scan so all reads stay in range.
    let limit = contents.len().saturating_sub(16);
    for i in 0..limit {
        // DataTools.bytesToInt(buf, i, 4, false) == 0xf03fff00 && ... == 0 &&
        // ... == 0 && DataTools.bytesToInt(buf, i + 12, 4, false) == 0xff00
        let marker = &contents[i..i + 4] == b"\xf0\x3f\xff\x00"
            && contents[i + 4..i + 12] == [0u8; 8]
            && &contents[i + 12..i + 16] == b"\x00\x00\xff\x00";
        if !marker {
            continue;
        }

        let mut pointer = i + 22;
        if pointer + 2 > contents.len() {
            continue;
        }
        // int length = DataTools.bytesToShort(buf, pointer, 2, true); (LE)
        let length = read_le_u16(contents, pointer) as usize;
        if length == 6 {
            let name_start = pointer + 2;
            let name_end = name_start + length;
            if name_end <= contents.len() && &contents[name_start..name_end] == b"CImage" {
                pointer += length + 4;
            }
        }

        // if (DataTools.bytesToInt(buf, pointer, 4, false) == 0x08000400)
        if pointer + 4 > contents.len() || &contents[pointer..pointer + 4] != b"\x08\x00\x04\x00" {
            continue;
        }
        let offset = pointer + 4;
        if offsets.contains(&offset) || offset >= contents.len() {
            continue;
        }
        // int len = buf[pointer + 4]; -> signed byte cast to int
        let name_len = contents[pointer + 4] as i8 as i32;
        if name_len < 0 {
            continue;
        }
        let name_len = name_len as usize;
        let name_start = pointer + 5;
        let name_end = name_start.saturating_add(name_len);
        if name_end > contents.len() {
            continue;
        }
        let name = String::from_utf8_lossy(&contents[name_start..name_end]);
        if !name.contains("Palette") {
            offsets.push(offset);
        }
    }
    offsets
}

fn find_bytes_from(haystack: &[u8], needle: &[u8], start: usize) -> Option<usize> {
    if needle.is_empty() || start >= haystack.len() || haystack.len() < needle.len() {
        return None;
    }
    haystack[start..]
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|pos| start + pos)
}

fn parse_tillvision_description_blocks(contents: &[u8]) -> Vec<HashMap<String, String>> {
    let mut blocks = Vec::new();
    let mut search_from = 0usize;
    while let Some(marker) = find_bytes_from(contents, TILLVISION_DESCRIPTION_MARKER, search_from) {
        let len_offset = marker + TILLVISION_DESCRIPTION_MARKER.len();
        if len_offset + 2 > contents.len() {
            break;
        }
        let len = read_le_u16(contents, len_offset) as usize;
        let text_start = len_offset + 2;
        let text_end = text_start.saturating_add(len);
        search_from = len_offset + 1;
        if len == 0 || len > 0x1000 || text_end > contents.len() {
            continue;
        }

        let text = String::from_utf8_lossy(&contents[text_start..text_end]);
        let mut parsed = HashMap::new();
        for line in text.split(['\r', '\n']) {
            let line = line.trim();
            if line.starts_with(';') {
                continue;
            }
            let Some((key, value)) = line.split_once(':') else {
                continue;
            };
            let key = key.trim();
            if key.is_empty() {
                continue;
            }
            parsed.insert(key.to_string(), value.trim().to_string());
        }
        if !parsed.is_empty() {
            blocks.push(parsed);
        }
    }
    blocks
}

fn add_tillvision_description_metadata(
    series_metadata: &mut HashMap<String, MetadataValue>,
    description: &HashMap<String, String>,
) -> Result<()> {
    for (key, value) in description {
        series_metadata.insert(format!("Info {key}"), MetadataValue::String(value.clone()));
    }
    add_tillvision_normalized_metadata(series_metadata, description)?;

    if let Some(exposure_ms) = description.get("Exposure time [ms]") {
        let seconds = exposure_ms.parse::<f64>().map_err(|_| {
            BioFormatsError::UnsupportedFormat(format!(
                "TillVision embedded VWS description has invalid Exposure time [ms]: {exposure_ms}"
            ))
        })? / 1000.0;
        series_metadata.insert(
            "Info Exposure time [s]".to_string(),
            MetadataValue::String(seconds.to_string()),
        );
    }

    let date = description
        .get("Date")
        .map(String::as_str)
        .unwrap_or("")
        .trim();
    let start = description
        .get("Start time of experiment")
        .map(String::as_str)
        .unwrap_or("")
        .trim();
    if !date.is_empty() || !start.is_empty() {
        let value = if date.is_empty() {
            start.to_string()
        } else if start.is_empty() {
            date.to_string()
        } else {
            format!("{date} {start}")
        };
        series_metadata.insert(
            "Info Acquisition date/time".to_string(),
            MetadataValue::String(value),
        );
    }

    Ok(())
}

fn add_tillvision_normalized_metadata(
    series_metadata: &mut HashMap<String, MetadataValue>,
    values: &HashMap<String, String>,
) -> Result<()> {
    if let Some((key, value, unit)) = find_tillvision_exposure_value(values) {
        let raw = first_numeric_token(value).ok_or_else(|| {
            BioFormatsError::UnsupportedFormat(format!(
                "TillVision metadata has invalid {key}: {value}"
            ))
        })?;
        let seconds = match unit {
            TillVisionTimeUnit::Milliseconds => raw / 1000.0,
            TillVisionTimeUnit::Seconds => raw,
        };
        series_metadata.insert(
            "tillvision.exposure_time_seconds".to_string(),
            MetadataValue::Float(seconds),
        );
    }

    if let Some(image_type) = find_tillvision_text_value(values, &["image type", "imagetype"]) {
        if !image_type.trim().is_empty() {
            series_metadata.insert(
                "tillvision.image_type".to_string(),
                MetadataValue::String(image_type.trim().to_string()),
            );
        }
    }

    if let Some(image_name) =
        find_tillvision_text_value(values, &["image name", "imagename", "name"])
    {
        if !image_name.trim().is_empty() {
            series_metadata.insert(
                "tillvision.image_name".to_string(),
                MetadataValue::String(image_name.trim().to_string()),
            );
        }
    }

    add_tillvision_physical_size_metadata(series_metadata, values)?;
    add_tillvision_time_increment_metadata(series_metadata, values)?;
    add_tillvision_channel_metadata(series_metadata, values)?;

    let date = find_tillvision_text_value(values, &["date"])
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let start =
        find_tillvision_text_value(values, &["start time of experiment", "start time", "time"])
            .map(str::trim)
            .filter(|value| !value.is_empty());
    if date.is_some() || start.is_some() {
        let acquisition = match (date, start) {
            (Some(date), Some(start)) => format!("{date} {start}"),
            (Some(date), None) => date.to_string(),
            (None, Some(start)) => start.to_string(),
            (None, None) => String::new(),
        };
        if !acquisition.is_empty() {
            series_metadata.insert(
                "tillvision.acquisition_datetime".to_string(),
                MetadataValue::String(acquisition),
            );
        }
        if let Some(date) = date {
            if let Some(normalized) = normalize_tillvision_acquisition_datetime(date, start) {
                series_metadata.insert(
                    "tillvision.acquisition_datetime_iso8601".to_string(),
                    MetadataValue::String(normalized),
                );
            }
        }
    }

    Ok(())
}

fn add_tillvision_physical_size_metadata(
    series_metadata: &mut HashMap<String, MetadataValue>,
    values: &HashMap<String, String>,
) -> Result<()> {
    for &(axis, output_key) in &[
        ("x", "tillvision.physical_size_x_um"),
        ("y", "tillvision.physical_size_y_um"),
        ("z", "tillvision.physical_size_z_um"),
    ] {
        if let Some((key, value)) = find_tillvision_physical_size_value(values, axis) {
            let raw = first_numeric_token(value).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!(
                    "TillVision metadata has invalid {key}: {value}"
                ))
            })?;
            let microns = tillvision_length_to_microns(raw, key);
            if microns.is_finite() && microns > 0.0 {
                series_metadata.insert(output_key.to_string(), MetadataValue::Float(microns));
            }
        }
    }
    Ok(())
}

fn add_tillvision_time_increment_metadata(
    series_metadata: &mut HashMap<String, MetadataValue>,
    values: &HashMap<String, String>,
) -> Result<()> {
    let Some((key, value, unit)) = find_tillvision_time_increment_value(values) else {
        return Ok(());
    };
    let raw = first_numeric_token(value).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(format!(
            "TillVision metadata has invalid {key}: {value}"
        ))
    })?;
    let seconds = match unit {
        TillVisionTimeUnit::Milliseconds => raw / 1000.0,
        TillVisionTimeUnit::Seconds => raw,
    };
    if seconds.is_finite() && seconds > 0.0 {
        series_metadata.insert(
            "tillvision.time_increment_seconds".to_string(),
            MetadataValue::Float(seconds),
        );
    }
    Ok(())
}

fn add_tillvision_channel_metadata(
    series_metadata: &mut HashMap<String, MetadataValue>,
    values: &HashMap<String, String>,
) -> Result<()> {
    for (key, value) in values {
        let normalized = normalize_tillvision_key(key);
        if !normalized.contains("channel") {
            continue;
        }
        let Some(channel_index) = tillvision_channel_index_from_key(&normalized) else {
            continue;
        };
        if normalized.contains("name") || normalized.contains("selection") {
            if !value.trim().is_empty() {
                series_metadata.insert(
                    format!("tillvision.channel.{channel_index}.name"),
                    MetadataValue::String(value.trim().to_string()),
                );
            }
        } else if normalized.contains("excitation") && normalized.contains("wavelength") {
            let wavelength = first_numeric_token(value).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!(
                    "TillVision metadata has invalid {key}: {value}"
                ))
            })?;
            if wavelength.is_finite() && wavelength > 0.0 {
                series_metadata.insert(
                    format!("tillvision.channel.{channel_index}.excitation_wavelength_nm"),
                    MetadataValue::Float(tillvision_length_to_nanometres(wavelength, key)),
                );
            }
        } else if normalized.contains("emission") && normalized.contains("wavelength") {
            let wavelength = first_numeric_token(value).ok_or_else(|| {
                BioFormatsError::UnsupportedFormat(format!(
                    "TillVision metadata has invalid {key}: {value}"
                ))
            })?;
            if wavelength.is_finite() && wavelength > 0.0 {
                series_metadata.insert(
                    format!("tillvision.channel.{channel_index}.emission_wavelength_nm"),
                    MetadataValue::Float(tillvision_length_to_nanometres(wavelength, key)),
                );
            }
        }
    }
    Ok(())
}

fn normalize_tillvision_acquisition_datetime(date: &str, start: Option<&str>) -> Option<String> {
    let (year, month, day) = parse_tillvision_date(date)?;
    let Some(start) = start else {
        return Some(format!("{year:04}-{month:02}-{day:02}"));
    };
    let (hour, minute, second) = parse_tillvision_time(start)?;
    Some(format!(
        "{year:04}-{month:02}-{day:02}T{hour:02}:{minute:02}:{second:02}"
    ))
}

fn parse_tillvision_date(date: &str) -> Option<(u32, u32, u32)> {
    let parts = date
        .trim()
        .split(|ch: char| matches!(ch, '/' | '-' | '.'))
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>();
    if parts.len() != 3 {
        return None;
    }
    let month = parts[0].trim().parse::<u32>().ok()?;
    let day = parts[1].trim().parse::<u32>().ok()?;
    let raw_year = parts[2].trim();
    let mut year = raw_year.parse::<u32>().ok()?;
    if raw_year.len() == 2 {
        year = if year <= 69 { 2000 + year } else { 1900 + year };
    }
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    Some((year, month, day))
}

fn parse_tillvision_time(time: &str) -> Option<(u32, u32, u32)> {
    let mut parts = time.split_whitespace();
    let clock = parts.next()?;
    let am_pm = parts.next().map(|value| value.to_ascii_lowercase());
    let clock_parts = clock
        .split(':')
        .filter(|part| !part.trim().is_empty())
        .collect::<Vec<_>>();
    if !(2..=3).contains(&clock_parts.len()) {
        return None;
    }
    let mut hour = clock_parts[0].trim().parse::<u32>().ok()?;
    let minute = clock_parts[1].trim().parse::<u32>().ok()?;
    let second = if clock_parts.len() == 3 {
        clock_parts[2].trim().parse::<u32>().ok()?
    } else {
        0
    };
    if minute > 59 || second > 59 {
        return None;
    }
    match am_pm.as_deref() {
        Some("am") => {
            if hour == 12 {
                hour = 0;
            } else if hour == 0 || hour > 12 {
                return None;
            }
        }
        Some("pm") => {
            if hour == 0 || hour > 12 {
                return None;
            }
            if hour != 12 {
                hour += 12;
            }
        }
        Some(_) => return None,
        None if hour > 23 => return None,
        None => {}
    }
    Some((hour, minute, second))
}

#[derive(Clone, Copy)]
enum TillVisionTimeUnit {
    Milliseconds,
    Seconds,
}

fn find_tillvision_exposure_value<'a>(
    values: &'a HashMap<String, String>,
) -> Option<(&'a str, &'a str, TillVisionTimeUnit)> {
    for (key, value) in values {
        let normalized = normalize_tillvision_key(key);
        if !normalized.contains("exposure") {
            continue;
        }
        let unit = if normalized.contains("ms")
            || normalized.contains("millisecond")
            || normalized == "exposuretime"
        {
            TillVisionTimeUnit::Milliseconds
        } else if normalized.contains(" s") || normalized.contains("sec") {
            TillVisionTimeUnit::Seconds
        } else {
            continue;
        };
        return Some((key.as_str(), value.as_str(), unit));
    }
    None
}

fn find_tillvision_physical_size_value<'a>(
    values: &'a HashMap<String, String>,
    axis: &str,
) -> Option<(&'a str, &'a str)> {
    values.iter().find_map(|(key, value)| {
        let normalized = normalize_tillvision_key(key);
        let compact = compact_tillvision_key(&normalized);
        if axis == "x"
            && (compact.contains("physicalsizex")
                || compact.contains("pixelsizex")
                || compact.contains("xpixelsize")
                || compact.contains("pixelwidth"))
        {
            return Some((key.as_str(), value.as_str()));
        }
        if axis == "y"
            && (compact.contains("physicalsizey")
                || compact.contains("pixelsizey")
                || compact.contains("ypixelsize")
                || compact.contains("pixelheight"))
        {
            return Some((key.as_str(), value.as_str()));
        }
        if axis == "z"
            && (compact.contains("physicalsizez")
                || compact.contains("pixelsizez")
                || compact.contains("zpixelsize")
                || compact.contains("zstep")
                || compact.contains("zspacing")
                || compact.contains("slicethickness"))
        {
            return Some((key.as_str(), value.as_str()));
        }
        None
    })
}

fn find_tillvision_time_increment_value<'a>(
    values: &'a HashMap<String, String>,
) -> Option<(&'a str, &'a str, TillVisionTimeUnit)> {
    for (key, value) in values {
        let normalized = normalize_tillvision_key(key);
        let compact = compact_tillvision_key(&normalized);
        if normalized.contains("exposure") || compact.contains("starttime") {
            continue;
        }
        let is_time_increment = compact.contains("timeincrement")
            || compact.contains("frameinterval")
            || compact.contains("timestep")
            || compact.contains("cycletime");
        if !is_time_increment {
            continue;
        }
        let unit = if normalized.contains("ms") || normalized.contains("millisecond") {
            TillVisionTimeUnit::Milliseconds
        } else {
            TillVisionTimeUnit::Seconds
        };
        return Some((key.as_str(), value.as_str(), unit));
    }
    None
}

fn find_tillvision_text_value<'a>(
    values: &'a HashMap<String, String>,
    candidates: &[&str],
) -> Option<&'a str> {
    values.iter().find_map(|(key, value)| {
        let normalized = normalize_tillvision_key(key);
        candidates
            .iter()
            .any(|candidate| normalized == normalize_tillvision_key(candidate))
            .then_some(value.as_str())
    })
}

fn normalize_tillvision_key(key: &str) -> String {
    key.trim()
        .trim_matches(|ch: char| ch == '[' || ch == ']')
        .to_ascii_lowercase()
}

fn compact_tillvision_key(key: &str) -> String {
    key.chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .collect()
}

fn first_numeric_token(value: &str) -> Option<f64> {
    value
        .split(|ch: char| ch.is_ascii_whitespace() || ch == ',' || ch == ';')
        .find_map(|token| {
            token
                .trim_matches(|ch: char| ch == ':' || ch == '=')
                .parse::<f64>()
                .ok()
        })
}

fn tillvision_length_to_microns(value: f64, key: &str) -> f64 {
    let key = normalize_tillvision_key(key);
    if key.contains("[nm]") || key.contains(" nm") || key.contains("nanometer") {
        value / 1000.0
    } else if key.contains("[mm]") || key.contains(" mm") || key.contains("millimeter") {
        value * 1000.0
    } else {
        value
    }
}

fn tillvision_length_to_nanometres(value: f64, key: &str) -> f64 {
    let key = normalize_tillvision_key(key);
    if key.contains("[um]")
        || key.contains("[µm]")
        || key.contains(" um")
        || key.contains(" µm")
        || key.contains("micrometer")
    {
        value * 1000.0
    } else if key.contains("[mm]") || key.contains(" mm") || key.contains("millimeter") {
        value * 1_000_000.0
    } else {
        value
    }
}

fn tillvision_channel_index_from_key(key: &str) -> Option<usize> {
    let digits: String = key
        .chars()
        .skip_while(|ch| !ch.is_ascii_digit())
        .take_while(|ch| ch.is_ascii_digit())
        .collect();
    let parsed = digits.parse::<usize>().ok()?;
    Some(parsed.saturating_sub(1))
}

fn tillvision_metadata_float(meta: &ImageMetadata, key: &str) -> Option<f64> {
    match meta.series_metadata.get(key) {
        Some(MetadataValue::Float(value)) => Some(*value),
        Some(MetadataValue::String(value)) => value.parse().ok(),
        _ => None,
    }
}

fn tillvision_positive_metadata_float(meta: &ImageMetadata, key: &str) -> Option<f64> {
    tillvision_metadata_float(meta, key).filter(|value| value.is_finite() && *value > 0.0)
}

fn tillvision_metadata_text<'a>(meta: &'a ImageMetadata, key: &str) -> Option<&'a str> {
    match meta.series_metadata.get(key) {
        Some(MetadataValue::String(value)) => Some(value.as_str()),
        _ => None,
    }
}

fn tillvision_plane_zct(meta: &ImageMetadata, plane: u32) -> (u32, u32, u32) {
    let full_planes = meta
        .size_z
        .checked_mul(meta.size_c)
        .and_then(|zc| zc.checked_mul(meta.size_t));
    if full_planes == Some(meta.image_count) {
        let c = plane % meta.size_c.max(1);
        let z = (plane / meta.size_c.max(1)) % meta.size_z.max(1);
        let t = plane / (meta.size_c.max(1) * meta.size_z.max(1));
        (z, c, t)
    } else {
        let z = plane % meta.size_z.max(1);
        let t = plane / meta.size_z.max(1);
        (z, 0, t)
    }
}
