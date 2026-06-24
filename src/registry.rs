use std::path::Path;

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::peek_header;
use crate::common::metadata::ImageMetadata;
use crate::common::ome_metadata::OmeMetadata;
use crate::common::path::confined_join;
use crate::common::reader::FormatReader;

const DETECTION_HEADER_BYTES: usize = 2048;

/// The top-level reader that auto-detects the file format and delegates to the
/// appropriate format-specific reader.
pub struct ImageReader {
    inner: Box<dyn FormatReader>,
}

/// Returns all registered format readers. Used internally by `ImageReader` and `Memoizer`.
pub fn all_readers_pub() -> Vec<Box<dyn FormatReader>> {
    all_readers()
}

/// Open and return the detected concrete reader as a boxed trait object.
pub fn open_reader_boxed(path: &Path) -> Result<Box<dyn FormatReader>> {
    open_reader(path)
}

fn all_readers() -> Vec<Box<dyn FormatReader>> {
    crate::reader_order::all_readers()
}

impl ImageReader {
    /// Open the file at `path`, detect its format, parse metadata.
    pub fn open(path: &Path) -> Result<Self> {
        Ok(ImageReader {
            inner: open_reader(path)?,
        })
    }

    /// Return structured OME metadata.
    ///
    /// Equivalent to Java Bio-Formats `reader.setMetadataStore(service.createOMEXMLMetadata())`.
    /// Returns baseline OME metadata for all readers and enriched metadata for
    /// formats that parse additional OME-compatible fields.
    pub fn ome_metadata(&self) -> Option<OmeMetadata> {
        self.inner.ome_metadata()
    }
    pub fn series_count(&self) -> usize {
        self.inner.series_count()
    }
    pub fn set_series(&mut self, series: usize) -> Result<()> {
        self.inner.set_series(series)
    }
    pub fn series(&self) -> usize {
        self.inner.series()
    }
    pub fn metadata(&self) -> &ImageMetadata {
        self.inner.metadata()
    }
    pub fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes(plane_index)
    }
    pub fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(plane_index, x, y, w, h)
    }
    pub fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        self.inner.open_thumb_bytes(plane_index)
    }
    pub fn resolution_count(&self) -> usize {
        self.inner.resolution_count()
    }
    pub fn set_resolution(&mut self, level: usize) -> Result<()> {
        self.inner.set_resolution(level)
    }
    pub fn close(&mut self) -> Result<()> {
        self.inner.close()
    }
}

pub(crate) fn open_reader(path: &Path) -> Result<Box<dyn FormatReader>> {
    // OME-Zarr is a directory-based format. `peek_header` cannot read a
    // directory, so detect and dispatch it before any byte sniffing. Mirrors
    // Java `ZarrReader.isThisType`, which matches on the `.zarr` path.
    #[cfg(feature = "zarr")]
    if crate::formats::zarr::is_zarr_path(path) {
        let mut r = boxed_reader(crate::formats::zarr::OmeZarrReader::new());
        match r.set_id(path) {
            Ok(()) => return Ok(r),
            // A directory cannot fall through to `peek_header`; surface the error.
            Err(err) if path.is_dir() => return Err(err),
            Err(_) => {}
        }
    }

    // Java readers.txt lists FilePatternReader first and its suffix is
    // sufficient, so a `.pattern` path is terminally a pattern file even if the
    // text happens to begin with image/container magic bytes.
    if has_pattern_extension(path) {
        let mut r = boxed_reader(crate::formats::misc4::FilePatternReader::new());
        r.set_id(path)?;
        return Ok(r);
    }

    let header = peek_header(path, DETECTION_HEADER_BYTES)?;
    let mut best_error = None;

    if let Some(err) = terminal_text_companion_error(path, &header) {
        return Err(err);
    }

    // `.ims` is shared by two unrelated formats: the HDF5-based Imaris
    // (`imaris::ImarisHdfReader`) and the older Bitplane Imaris 3 TIFF variant
    // (`flim2::ImarisTiffReader`). The TIFF wrapper accepts `.ims` purely by
    // extension, so for a genuine HDF5 `.ims` file the HDF5 reader must win.
    // Dispatch on the actual header here: if the file carries the HDF5 magic,
    // route straight to the HDF5 Imaris reader before any TIFF-based handling.
    if has_ims_extension(path) && is_hdf5_header(&header) {
        let mut r = boxed_reader(crate::formats::imaris_hdf::ImarisHdfReader::new());
        match r.set_id(path) {
            Ok(()) => return Ok(r),
            Err(err) => remember_set_id_error(&mut best_error, err),
        }
    }

    // ZVI is an OLE/CFB container whose magic bytes are shared with many other
    // formats. Java's ZeissZVIReader is extension-driven for this case; routing
    // `.zvi` directly avoids probing unrelated OLE readers that may parse large
    // streams before rejecting the file.
    if has_zvi_extension(path) {
        let mut r = boxed_reader(crate::formats::zeiss_zvi::ZeissZviReader::new());
        match r.set_id(path) {
            Ok(()) => return Ok(r),
            Err(err) => remember_set_id_error(&mut best_error, err),
        }
    }

    // Java CanonRawReader byte-detects legacy 300D CRW files solely by exact
    // file length. `peek_header` cannot represent that full-stream predicate,
    // so mirror it before extension/generic probing.
    if path
        .metadata()
        .map(|m| crate::formats::camera2::CanonRawReader::is_legacy_file_length(m.len()))
        .unwrap_or(false)
    {
        let mut r = boxed_reader(crate::formats::camera2::CanonRawReader::new());
        match r.set_id(path) {
            Ok(()) => return Ok(r),
            Err(err) => remember_set_id_error(&mut best_error, err),
        }
    }

    // Java readers.txt places DicomReader before the final generic TIFF reader
    // because DICOM-TIFF files can carry a valid TIFF-looking preamble. Our
    // split probe still needs this explicit guard before TIFF wrapper probing.
    let dicom_probe = crate::formats::dicom::DicomReader::new();
    if dicom_probe.is_this_type_by_bytes(&header) {
        let mut r = boxed_reader(crate::formats::dicom::DicomReader::new());
        match r.set_id(path) {
            Ok(()) => return Ok(r),
            Err(err) => remember_set_id_error(&mut best_error, err),
        }
    }

    // TIFF-based vendor wrappers often have no magic beyond TIFF itself.
    // Give non-generic TIFF extensions a chance before the broad TiffReader
    // byte signature accepts the file.
    if is_tiff_header(&header) {
        let had_tiff_wrappers = !tiff_wrapper_readers_for_extension(path, &header).is_empty();
        for mut r in tiff_wrapper_readers_for_extension(path, &header) {
            match r.set_id(path) {
                Ok(()) => return Ok(r),
                Err(err) => remember_set_id_error(&mut best_error, err),
            }
        }
        if had_tiff_wrappers && has_ndpi_extension(path) {
            return Err(best_error.unwrap_or_else(|| {
                BioFormatsError::UnsupportedFormat("NDPI TIFF could not be initialized".into())
            }));
        }
        if has_ndpi_extension(path) {
            let mut r = boxed_reader(crate::tiff::TiffReader::new());
            return match r.set_id(path) {
                Ok(()) => Ok(r),
                Err(err) => Err(err),
            };
        }
        if has_tiff_extension(path) {
            let mut r = boxed_reader(crate::tiff::TiffReader::new());
            match r.set_id(path) {
                Ok(()) => return Ok(r),
                Err(err) => remember_set_id_error(&mut best_error, err),
            }
        }
    }

    // SpiderReader's Java byte probe compares declared payload/header sizes to
    // the full stream length. The generic registry byte loop only passes a
    // prefix, so bridge that full-stream predicate here before suffix fallback.
    if crate::formats::amira::is_spider_file(path) {
        let mut r = boxed_reader(crate::formats::amira::SpiderReader::new());
        match r.set_id(path) {
            Ok(()) => return Ok(r),
            Err(err) => remember_set_id_error(&mut best_error, err),
        }
    }

    // 1. Magic bytes
    for mut r in all_readers() {
        if r.is_this_type_by_bytes(&header) {
            match r.set_id(path) {
                Ok(()) => return Ok(r),
                Err(err @ BioFormatsError::UnsupportedFormat(_))
                    if unsupported_magic_error_is_terminal(&err) =>
                {
                    if has_fake_extension(path) && terminal_magic_allows_fake_fallback(&err) {
                        remember_set_id_error(&mut best_error, err);
                        continue;
                    }
                    return Err(err);
                }
                Err(err) => remember_set_id_error(&mut best_error, err),
            }
        }
    }

    // 2. Extension fallback
    let mut replacing_magic_error = best_error.is_some();
    for mut r in all_readers() {
        if r.is_this_type_by_name(path) {
            match r.set_id(path) {
                Ok(()) => return Ok(r),
                Err(err) => {
                    if replacing_magic_error {
                        best_error = Some(err);
                        replacing_magic_error = false;
                    } else if terminal_extension_error(path, &err) {
                        return Err(err);
                    } else {
                        remember_set_id_error(&mut best_error, err);
                    }
                }
            }
        }
    }

    Err(best_error
        .unwrap_or_else(|| BioFormatsError::UnsupportedFormat(path.display().to_string())))
}

fn has_fake_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("fake"))
        .unwrap_or(false)
}

fn has_pattern_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("pattern"))
        .unwrap_or(false)
}

fn has_zvi_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.eq_ignore_ascii_case("zvi"))
        .unwrap_or(false)
}

fn terminal_magic_allows_fake_fallback(err: &BioFormatsError) -> bool {
    matches!(
        err,
        BioFormatsError::UnsupportedFormat(message)
            if message.contains("Bruker OPUS native spectral image decoding")
    )
}

fn terminal_text_companion_error(path: &Path, header: &[u8]) -> Option<BioFormatsError> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    let text = std::str::from_utf8(header).ok()?;
    let trimmed = text.trim_start_matches('\u{feff}').trim_start();
    match ext.as_deref() {
        Some("mlf")
            if trimmed.contains("<bts:MeasurementData")
                || trimmed.contains("<MeasurementData") =>
        {
            Some(BioFormatsError::UnsupportedFormat(
                "Yokogawa CV7000 MeasurementData.mlf is an XML index; open the .wpi/complete acquisition with its referenced TIFF planes".into(),
            ))
        }
        Some("mrf")
            if trimmed.contains("<bts:MeasurementDetail")
                || trimmed.contains("<MeasurementDetail") =>
        {
            Some(BioFormatsError::UnsupportedFormat(
                "Yokogawa CV7000 MeasurementDetail.mrf is channel metadata, not a standalone image; open the .wpi/complete acquisition with MeasurementData.mlf and TIFF planes".into(),
            ))
        }
        Some("pict") | Some("pct")
            if trimmed.starts_with("<!DOCTYPE html")
                || trimmed.starts_with("<html")
                || trimmed.starts_with("<?xml") =>
        {
            Some(BioFormatsError::UnsupportedFormat(
                "PICT reader received a text/HTML document, not an Apple PICT image".into(),
            ))
        }
        Some("pov")
            if trimmed.starts_with("//")
                || trimmed.starts_with("#")
                || trimmed.contains("camera")
                || trimmed.contains("light_source")
                || trimmed.contains("object") =>
        {
            Some(BioFormatsError::UnsupportedFormat(
                "POV-Ray .pov scene source is not a DF3 density grid image; only .df3-style voxel grids are readable".into(),
            ))
        }
        _ => None,
    }
}

/// Select a likely reader without calling `set_id`.
///
/// This is used only for memoized metadata cache hits where the file stamp and
/// cached metadata shape have already been validated and callers may never read
/// pixels. The order mirrors `open_reader` as closely as possible without
/// parsing: precise directory/header/TIFF dispatch, then magic-byte readers,
/// then extension fallbacks. If a magic-byte candidate later fails during lazy
/// pixel access, `Memoizer` falls back to the full `open_reader` path.
pub(crate) fn detect_reader_without_set_id(path: &Path) -> Result<Box<dyn FormatReader>> {
    #[cfg(feature = "zarr")]
    if crate::formats::zarr::is_zarr_path(path) {
        return Ok(boxed_reader(crate::formats::zarr::OmeZarrReader::new()));
    }

    if has_pattern_extension(path) {
        return Ok(boxed_reader(crate::formats::misc4::FilePatternReader::new()));
    }

    let header = peek_header(path, DETECTION_HEADER_BYTES)?;

    if has_ims_extension(path) && is_hdf5_header(&header) {
        return Ok(boxed_reader(
            crate::formats::imaris_hdf::ImarisHdfReader::new(),
        ));
    }

    if has_zvi_extension(path) {
        return Ok(boxed_reader(
            crate::formats::zeiss_zvi::ZeissZviReader::new(),
        ));
    }

    if path
        .metadata()
        .map(|m| crate::formats::camera2::CanonRawReader::is_legacy_file_length(m.len()))
        .unwrap_or(false)
    {
        return Ok(boxed_reader(crate::formats::camera2::CanonRawReader::new()));
    }

    if is_tiff_header(&header) {
        let readers = tiff_wrapper_readers_for_extension(path, &header);
        if let Some(reader) = readers.into_iter().next() {
            return Ok(reader);
        }
    }

    if crate::formats::amira::is_spider_file(path) {
        return Ok(boxed_reader(crate::formats::amira::SpiderReader::new()));
    }

    for r in all_readers() {
        if r.is_this_type_by_bytes(&header) {
            return Ok(r);
        }
    }

    for r in all_readers() {
        if r.is_this_type_by_name(path) {
            return Ok(r);
        }
    }

    Err(BioFormatsError::UnsupportedFormat(
        path.display().to_string(),
    ))
}

fn terminal_extension_error(path: &Path, err: &BioFormatsError) -> bool {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());
    matches!(
        (ext.as_deref(), err),
        (
            Some("vms") | Some("vmu"),
            BioFormatsError::Format(message) | BioFormatsError::UnsupportedFormat(message)
        ) if message.contains("Hamamatsu VMS")
            || message.contains("Hamamatsu VMS/VMU")
    )
}

fn has_ims_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("ims"))
        .unwrap_or(false)
}

fn has_tiff_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("tif") || e.eq_ignore_ascii_case("tiff"))
        .unwrap_or(false)
}

fn has_ndpi_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("ndpi"))
        .unwrap_or(false)
}

fn is_hdf5_header(header: &[u8]) -> bool {
    // HDF5 signature: bytes 0-7 = \x89 H D F \r \n \x1a \n
    header.len() >= 8 && header[0..8] == [0x89, 0x48, 0x44, 0x46, 0x0d, 0x0a, 0x1a, 0x0a]
}

fn is_tiff_header(header: &[u8]) -> bool {
    header.len() >= 4
        && (header[0..2] == [0x49, 0x49] || header[0..2] == [0x4D, 0x4D])
        && (header[2..4] == [42, 0]
            || header[2..4] == [0, 42]
            || header[2..4] == [43, 0]
            || header[2..4] == [0, 43])
}

fn tiff_wrapper_readers_for_extension(path: &Path, header: &[u8]) -> Vec<Box<dyn FormatReader>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    match ext.as_deref() {
        Some("lsm") => vec![boxed_reader(
            crate::formats::zeiss_lsm::ZeissLsmReader::new(),
        )],
        Some("stk") => vec![boxed_reader(
            crate::formats::metamorph::MetamorphReader::new(),
        )],
        Some("svs") => vec![boxed_reader(crate::formats::svs::SvsReader::new())],
        Some("ndpi") => {
            let mut readers = Vec::new();
            if crate::formats::tiff_wrappers::ndpi_has_hamamatsu_tags(path) {
                readers.push(boxed_reader(
                    crate::formats::tiff_wrappers::NdpiReader::new(),
                ));
            }
            readers
        }
        Some("scn") => vec![
            boxed_reader(crate::formats::tiff_wrappers::LeicaScnReader::new()),
            boxed_reader(crate::formats::flim2::BioRadScnReader::new()),
        ],
        Some("bif") => vec![boxed_reader(
            crate::formats::tiff_wrappers::VentanaReader::new(),
        )],
        Some("vsi") => vec![boxed_reader(crate::formats::flim2::CellSensReader::new())],
        Some("afi") => vec![boxed_reader(crate::formats::flim2::AfiReader::new())],
        Some("dng") => vec![boxed_reader(crate::formats::extended::DngReader::new())],
        Some("qptiff") => vec![boxed_reader(crate::formats::extended::VectraReader::new())],
        Some("gel") => vec![boxed_reader(crate::formats::extended::GelReader::new())],
        Some("flex") => vec![boxed_reader(crate::formats::flex::FlexReader::new())],
        Some("fff") => vec![boxed_reader(crate::formats::camera2::ImaconReader::new())],
        Some("pcoraw") => vec![boxed_reader(crate::formats::camera2::PcoRawReader::new())],
        Some("cr2") | Some("crw") | Some("cr3") => {
            vec![boxed_reader(crate::formats::camera2::CanonRawReader::new())]
        }
        // Nikon NEF camera RAW (TIFF-based; is_this_type_by_bytes requires the
        // EPS-standard tag or a Nikon Make, so it won't grab arbitrary TIFFs).
        Some("nef") => vec![boxed_reader(crate::formats::camera2::NikonReader::new())],
        // Image-Pro Sequence (TIFF-based with IMAGE_PRO custom tags). Raw Norpix
        // StreamPix `.seq` is not TIFF and is handled by NorpixReader instead.
        Some("seq") | Some("ips") => {
            vec![boxed_reader(crate::formats::norpix::SeqReader::new())]
        }
        Some("ipw") => vec![boxed_reader(crate::formats::camera2::IpwReader::new())],
        Some("ims") => vec![boxed_reader(crate::formats::flim2::ImarisTiffReader::new())],
        Some("xlef") => vec![boxed_reader(crate::formats::flim2::XlefReader::new())],
        Some("ctf") => vec![boxed_reader(crate::formats::flim2::MicroCtReader::new())],
        Some("ndpis") => vec![boxed_reader(crate::formats::flim2::NdpisReader::new())],
        // Generic .tif/.tiff TIFF wrappers only run early when they can be
        // identified from wrapper-specific metadata. Several wrapper readers
        // otherwise accept by extension and delegate to TiffReader, which would
        // steal ordinary TIFF files from explicit generic TIFF handling.
        Some("tif") | Some("tiff") => {
            let description = tiff_image_description(path);
            let software = tiff_software_tag(path);

            let mut readers = Vec::new();
            // Java readers.txt probes OMETiffReader before every other slow
            // TIFF wrapper. Rust's TiffReader is the OME-TIFF implementation,
            // so give valid-looking OME-XML comments the same early slot before
            // vendor text such as "Molecular Devices" can steal the file.
            if tiff_first_ifd_has_ome_xml_description(path) {
                readers.push(boxed_reader(crate::tiff::TiffReader::new()));
            }
            // Faas-format pyramid TIFFs are identified by a SOFTWARE tag
            // containing "Faas" (mirrors Java PyramidTiffReader.isThisType).
            if software
                .as_deref()
                .map(|software| software.contains("Faas"))
                .unwrap_or(false)
            {
                readers.push(boxed_reader(crate::formats::svs::PyramidTiffReader::new()));
            }

            // MIAS datasets are plain TIFFs, so the generic TiffReader would
            // win the magic pass. Give MiasReader a chance first, but ONLY when
            // the file is genuinely a MIAS plane (a Well<xxxx> directory +
            // mode/z/t naming), so ordinary .tif files still fall through.
            readers.extend(generic_tiff_name_wrappers(path, header));

            // Java readers.txt probes NikonReader before the other generic
            // `.tif` wrappers and before the final generic TIFF reader.
            if tiff_first_ifd_is_nikon_raw(path) {
                readers.push(boxed_reader(crate::formats::camera2::NikonReader::new()));
            }

            if description
                .as_deref()
                .map(|description| tiff_first_ifd_matches_fluoview(path, description))
                .unwrap_or(false)
            {
                readers.push(boxed_reader(
                    crate::formats::tiff_wrappers::FluoviewReader::new(),
                ));
            }
            if has_prairie_xml_sibling(path) {
                readers.push(boxed_reader(crate::formats::prairie::PrairieReader::new()));
            }

            // Nikon EZ-C1 confocal TIFFs are plain TIFFs identified only by a
            // SOFTWARE tag containing "EZ-C1". Gate on that tag (mirroring the
            // ImageDescription gating below) so ordinary TIFFs are untouched.
            if let Some(software) = software.as_deref() {
                // Classic MetaMorph/STK can also use .tif/.tiff. Java
                // MetamorphReader.isThisType checks the SOFTWARE tag before
                // the later MetamorphTiffReader XML-comment reader gets a turn.
                if software
                    .trim()
                    .to_ascii_lowercase()
                    .starts_with("metamorph")
                {
                    readers.push(boxed_reader(
                        crate::formats::metamorph::MetamorphReader::new(),
                    ));
                }
            }

            // Java also accepts classic MetaMorph TIFFs by UIC tags even when
            // SOFTWARE is absent: UIC1 + UIC3 + UIC4 on the first IFD.
            if tiff_first_ifd_has_all_tags(path, &[33628, 33630, 33631]) {
                readers.push(boxed_reader(
                    crate::formats::metamorph::MetamorphReader::new(),
                ));
            }
            let micromanager = crate::formats::micromanager::MicromanagerReader::new();
            if micromanager.is_this_type_by_name(path) {
                readers.push(boxed_reader(micromanager));
            }
            if let Some(description) = description.as_deref() {
                if description.contains("Improvision") {
                    readers.push(boxed_reader(
                        crate::formats::tiff_wrappers::ImprovisionTiffReader::new(),
                    ));
                }
                let trimmed = description.trim();
                if trimmed.starts_with("<MetaData>") && trimmed.ends_with("</MetaData>") {
                    readers.push(boxed_reader(
                        crate::formats::tiff_wrappers::MetamorphTiffReader::new(),
                    ));
                }
            }
            if software
                .as_deref()
                .map(|software| software.contains("EZ-C1"))
                .unwrap_or(false)
            {
                readers.push(boxed_reader(
                    crate::formats::tiff_wrappers::NikonTiffReader::new(),
                ));
            }
            if tiff_first_ifd_has_all_tags(path, &[50457]) {
                readers.push(boxed_reader(crate::formats::camera2::ImaconReader::new()));
            }
            if tiff_first_ifd_has_all_tags(path, &[37724]) {
                readers.push(boxed_reader(
                    crate::formats::camera2::PhotoshopTiffReader::new(),
                ));
            }
            if tiff_first_ifd_has_any_tag(path, &[34680, 34682, 34683]) {
                readers.push(boxed_reader(
                    crate::formats::tiff_wrappers::FeiTiffReader::new(),
                ));
            }
            if let Some(description) = description.as_deref() {
                readers.extend(simplepci_tiff_wrappers_for_description(description));
            }
            if tiff_first_ifd_has_all_tags(path, &[65332]) {
                readers.push(boxed_reader(
                    crate::formats::tiff_wrappers::NikonElementsTiffReader::new(),
                ));
            }
            if tiff_first_ifd_copyright_contains(path, "Trestle Corp.") {
                readers.push(boxed_reader(crate::formats::hcs2::TrestleReader::new()));
            }
            if tiff_first_ifd_matches_sis(path) {
                readers.push(boxed_reader(crate::formats::tiff_wrappers::SisReader::new()));
            }
            if tiff_first_ifd_matches_dng(path) {
                readers.push(boxed_reader(crate::formats::extended::DngReader::new()));
            }
            if let Some(software) = software.as_deref() {
                if software.starts_with("PerkinElmer-QPI") {
                    readers.push(boxed_reader(crate::formats::extended::VectraReader::new()));
                }
                if software.starts_with("IonpathMIBI") {
                    readers.push(boxed_reader(
                        crate::formats::hcs2::IonpathMibiTiffReader::new(),
                    ));
                }
            }
            if zeiss_tiff_meta_xml_exists(path) {
                readers.push(boxed_reader(crate::formats::sem::ZeissTiffReader::new()));
            }

            if let Some(description) = description.as_deref() {
                readers.extend(remaining_generic_tiff_wrappers_for_description(
                    path,
                    ext.as_deref().unwrap_or_default(),
                    description,
                ));
            }

            readers
        }
        _ => Vec::new(),
    }
}

fn generic_tiff_name_wrappers(path: &Path, _header: &[u8]) -> Vec<Box<dyn FormatReader>> {
    let mut readers = Vec::new();
    let mias = crate::formats::mias::MiasReader::new();
    if mias.is_this_type_by_name(path) {
        readers.push(boxed_reader(mias));
    }
    if !has_lei_sibling(path)
        && (crate::formats::prairie::tcs_xml_sibling_references_tiff(path)
            || crate::formats::prairie::is_tcs_tagged_tiff(path))
    {
        readers.push(boxed_reader(crate::formats::prairie::TcsReader::new()));
    }
    if has_lei_sibling(path) {
        readers.push(boxed_reader(crate::formats::leica::LeicaReader::new()));
    }
    readers
}

fn has_prairie_xml_sibling(path: &Path) -> bool {
    find_prairie_xml_sibling(path)
        .and_then(|xml| std::fs::read_to_string(&xml).ok().map(|text| (xml, text)))
        .map(|(xml_path, text)| {
            let prefix = text.get(..text.len().min(256)).unwrap_or(&text);
            prefix.contains("<PVScan") && prairie_xml_references_tiff(&xml_path, &text, path)
        })
        .unwrap_or(false)
}

fn prairie_xml_references_tiff(xml_path: &Path, xml: &str, tiff_path: &Path) -> bool {
    let Some(xml_dir) = xml_path.parent() else {
        return false;
    };
    xml.lines().any(|line| {
        line.contains("<File")
            && extract_xml_attr(line, "filename")
                .and_then(|value| confined_join(xml_dir, value))
                .map(|candidate| candidate == tiff_path)
                .unwrap_or(false)
    })
}

fn extract_xml_attr<'a>(text: &'a str, attr: &str) -> Option<&'a str> {
    let search = format!("{attr}=\"");
    let start = text.find(search.as_str())? + search.len();
    let end = text[start..].find('"')? + start;
    Some(&text[start..end])
}

fn find_prairie_xml_sibling(path: &Path) -> Option<std::path::PathBuf> {
    let parent = path.parent()?;
    let mut prefix = path.file_stem()?.to_str()?.to_string();
    loop {
        let cand = parent.join(format!("{prefix}.xml"));
        if cand.exists() {
            return Some(cand);
        }
        match prefix.rfind('_') {
            Some(i) => prefix.truncate(i),
            None => break,
        }
    }
    std::fs::read_dir(parent).ok().and_then(|rd| {
        rd.filter_map(|e| e.ok()).map(|e| e.path()).find(|p| {
            p.extension()
                .and_then(|e| e.to_str())
                .map(|e| e.eq_ignore_ascii_case("xml"))
                .unwrap_or(false)
        })
    })
}

fn has_lei_sibling(path: &Path) -> bool {
    let Some(parent) = path.parent() else {
        return false;
    };
    let Some(mut prefix) = path
        .file_stem()
        .and_then(|stem| stem.to_str())
        .map(str::to_string)
    else {
        return false;
    };
    loop {
        if parent.join(format!("{prefix}.lei")).exists()
            || parent.join(format!("{prefix}.LEI")).exists()
        {
            return true;
        }
        match prefix.rfind('_') {
            Some(i) => prefix.truncate(i),
            None => return false,
        }
    }
}

fn tiff_image_description(path: &Path) -> Option<String> {
    let mut reader = crate::tiff::TiffReader::new();
    reader.set_id(path).ok()?;
    let description = reader
        .series_list()
        .first()?
        .metadata
        .series_metadata
        .get("ImageDescription")?;
    if let crate::common::metadata::MetadataValue::String(value) = description {
        Some(value.clone())
    } else {
        None
    }
}

fn tiff_first_ifd_has_ome_xml_description(path: &Path) -> bool {
    let Some(ifd) = tiff_first_ifd(path) else {
        return false;
    };
    let Some(description) = ifd.get_str(crate::tiff::ifd::tag::IMAGE_DESCRIPTION) else {
        return false;
    };
    let trimmed = description.trim();
    trimmed.starts_with('<')
        && trimmed.ends_with('>')
        && (trimmed.starts_with("<OME")
            || trimmed.starts_with("<ome:OME")
            || trimmed.contains("<OME ")
            || trimmed.contains("<OME>")
            || trimmed.contains("<ome:OME "))
}

/// Read the first IFD's SOFTWARE (tag 305) value from a TIFF, if present.
/// Reads a generous header window so out-of-line tag values are usually
/// covered; used to gate the Nikon EZ-C1 wrapper without a full open.
fn tiff_software_tag(path: &Path) -> Option<String> {
    let header = peek_header(path, 64 * 1024).ok()?;
    let cursor = std::io::Cursor::new(header);
    let mut parser = crate::tiff::parser::TiffParser::new(cursor).ok()?;
    let offset = parser.first_ifd_offset;
    let (ifd, _) = parser.read_ifd(offset).ok()?;
    ifd.get(crate::tiff::ifd::tag::SOFTWARE)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn tiff_first_ifd_has_all_tags(path: &Path, tags: &[u16]) -> bool {
    let Some(ifd) = tiff_first_ifd(path) else {
        return false;
    };
    tags.iter().all(|tag| ifd.get(*tag).is_some())
}

fn tiff_first_ifd_has_any_tag(path: &Path, tags: &[u16]) -> bool {
    let Some(ifd) = tiff_first_ifd(path) else {
        return false;
    };
    tags.iter().any(|tag| ifd.get(*tag).is_some())
}

fn tiff_first_ifd_matches_sis(path: &Path) -> bool {
    let Some(ifd) = tiff_first_ifd(path) else {
        return false;
    };
    let software = ifd.get_str(crate::tiff::ifd::tag::SOFTWARE);
    let make = ifd.get_str(271);
    (ifd.get(33560).is_some() && software.map(|s| s.starts_with("analySIS")).unwrap_or(true))
        || (ifd.get(34853).is_some() && make.map(|s| s.starts_with("Olympus")).unwrap_or(false))
}

fn tiff_first_ifd_matches_fluoview(path: &Path, description: &str) -> bool {
    let Some(ifd) = tiff_first_ifd(path) else {
        return false;
    };
    (description.contains("FLUOVIEW") && ifd.get(34361).is_some())
        || ifd.get(34362).is_some()
        || description.starts_with("Andor")
}

fn tiff_first_ifd_is_nikon_raw(path: &Path) -> bool {
    let Some(ifd) = tiff_first_ifd(path) else {
        return false;
    };
    matches!(ifd.get_str(271), Some(make) if make.contains("Nikon"))
        && (ifd.get(37398).is_some() || ifd.get(37500).is_some())
}

fn tiff_first_ifd_matches_dng(path: &Path) -> bool {
    let Some(ifd) = tiff_first_ifd(path) else {
        return false;
    };
    let has_eps_tag = ifd.get(37398).is_some() || ifd.get(37500).is_some();
    let make = ifd.get_str(271);
    let model = ifd.get_str(272);
    let software = ifd.get_str(crate::tiff::ifd::tag::SOFTWARE);
    matches!(make, Some(make) if make.contains("Canon"))
        && has_eps_tag
        && !matches!(model, Some(model) if model.ends_with("S1 IS"))
        && software.is_none_or(|software| software.contains("Canon"))
}

fn tiff_first_ifd_copyright_contains(path: &Path, needle: &str) -> bool {
    const COPYRIGHT: u16 = 33432;
    tiff_first_ifd(path)
        .and_then(|ifd| ifd.get_str(COPYRIGHT).map(str::to_string))
        .map(|value| value.contains(needle))
        .unwrap_or(false)
}

fn zeiss_tiff_meta_xml_exists(path: &Path) -> bool {
    let mut meta = path.as_os_str().to_os_string();
    meta.push("_meta.xml");
    Path::new(&meta).exists()
}

fn tiff_first_ifd(path: &Path) -> Option<crate::tiff::ifd::Ifd> {
    let Ok(file) = std::fs::File::open(path) else {
        return None;
    };
    let mut parser = match crate::tiff::parser::TiffParser::new(file) {
        Ok(parser) => parser,
        Err(_) => return None,
    };
    let offset = parser.first_ifd_offset;
    parser.read_ifd(offset).ok().map(|(ifd, _)| ifd)
}

fn simplepci_tiff_wrappers_for_description(description: &str) -> Vec<Box<dyn FormatReader>> {
    let mut readers = Vec::new();
    if description
        .trim_start()
        .starts_with("Created by Hamamatsu Inc.")
    {
        readers.push(boxed_reader(
            crate::formats::hcs2::SimplePciTiffReader::new(),
        ));
    }
    readers
}

fn remaining_generic_tiff_wrappers_for_description(
    path: &Path,
    ext: &str,
    description: &str,
) -> Vec<Box<dyn FormatReader>> {
    let mut readers = Vec::new();

    match ext {
        "tif" => {
            readers.extend(hcs_tiff_wrappers_for_description(description));

            if tiff_first_ifd_matches_fluoview(path, description) {
                readers.push(boxed_reader(
                    crate::formats::tiff_wrappers::FluoviewReader::new(),
                ));
            }
            if description.contains("<Zeiss")
                || description.contains("<zeiss")
                || description.contains("<ApoTome")
                || description.contains("AxioVision")
            {
                readers.push(boxed_reader(
                    crate::formats::tiff_wrappers::ZeissApotomeTiffReader::new(),
                ));
            }
            if description.contains("<MetaXpress")
                || description.contains("Molecular Devices")
                || description.contains("<PlateID")
            {
                readers.push(boxed_reader(
                    crate::formats::tiff_wrappers::MolecularDevicesTiffReader::new(),
                ));
            }
        }
        "tiff" => {
            if description.contains("<variant") || description.contains("NIS-Elements") {
                readers.push(boxed_reader(
                    crate::formats::tiff_wrappers::NikonElementsTiffReader::new(),
                ));
            }
        }
        _ => {}
    }

    readers
}

fn hcs_tiff_wrappers_for_description(description: &str) -> Vec<Box<dyn FormatReader>> {
    let mut readers = Vec::new();
    let lower = description.to_ascii_lowercase();

    if lower.contains("metaxpress")
        || lower.contains("molecular devices")
        || lower.contains("<plateid")
    {
        readers.push(boxed_reader(
            crate::formats::hcs2::MetaxpressTiffReader::new(),
        ));
    }
    if description
        .trim_start()
        .starts_with("Created by Hamamatsu Inc.")
    {
        readers.push(boxed_reader(
            crate::formats::hcs2::SimplePciTiffReader::new(),
        ));
    }
    if lower.contains("ionpath") || lower.contains("mibi") || lower.contains("mibiscope") {
        readers.push(boxed_reader(
            crate::formats::hcs2::IonpathMibiTiffReader::new(),
        ));
    }
    if lower.contains("beckman coulter mias") {
        readers.push(boxed_reader(crate::formats::hcs2::MiasTiffReader::new()));
    }
    if lower.contains("trestle") {
        readers.push(boxed_reader(crate::formats::hcs2::TrestleReader::new()));
    }
    if lower.contains("tissuefaxs") || lower.contains("tissuegnostics") {
        readers.push(boxed_reader(crate::formats::hcs2::TissueFaxsReader::new()));
    }
    if lower.contains("mikroscan") || lower.contains("microvision instruments") {
        readers.push(boxed_reader(
            crate::formats::hcs2::MikroscanTiffReader::new(),
        ));
    }

    readers
}

fn boxed_reader<T: FormatReader + 'static>(reader: T) -> Box<dyn FormatReader> {
    Box::new(reader)
}

fn remember_set_id_error(best_error: &mut Option<BioFormatsError>, err: BioFormatsError) {
    match best_error {
        None => *best_error = Some(err),
        Some(BioFormatsError::UnsupportedFormat(_))
            if !matches!(err, BioFormatsError::UnsupportedFormat(_)) =>
        {
            *best_error = Some(err);
        }
        _ => {}
    }
}

fn unsupported_magic_error_is_terminal(err: &BioFormatsError) -> bool {
    matches!(
        err,
        BioFormatsError::UnsupportedFormat(message)
            if message.contains("native decoding is unsupported")
                || message.contains("native payload decoding is unsupported")
                || message.contains("native stack decoding is unsupported")
                || message.contains("native Metakit decoding is unsupported")
                || message.contains("native companion image payload decoding is unsupported")
                || message.contains("native spectral image decoding is unsupported")
                || message.contains("native JPEG tile payload decoding is unsupported")
                || message.contains("requires OLE2")
                || message.contains("requires HDF5")
    )
}

#[cfg(test)]
mod tests {
    use super::ImageReader;
    use crate::common::error::BioFormatsError;
    use crate::common::metadata::{ImageMetadata, MetadataValue};
    use crate::common::pixel_type::PixelType;
    use crate::common::reader::FormatReader;
    use crate::ImageWriter;
    use std::io::Write;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_registry_{nanos}_{name}"))
    }

    fn temp_dir(name: &str) -> PathBuf {
        let dir = temp_path(name);
        std::fs::create_dir(&dir).unwrap();
        dir
    }

    fn write_minimal_sbig(path: &PathBuf) {
        let mut bytes = vec![0u8; 2048];
        bytes[..21].copy_from_slice(b"ST-7 Compressed Image");
        let header = b"\nWidth = 1\nHeight = 1\nEnd\n";
        bytes[21..21 + header.len()].copy_from_slice(header);
        bytes.extend_from_slice(&2u16.to_le_bytes());
        bytes.extend_from_slice(&17u16.to_le_bytes());
        std::fs::write(path, bytes).unwrap();
    }

    fn write_dicom(path: &PathBuf, width: u32, height: u32) {
        let meta = ImageMetadata {
            size_x: width,
            size_y: height,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..Default::default()
        };
        let pixels = vec![7; (width * height) as usize];
        ImageWriter::save(path, &meta, &[pixels]).unwrap();
    }

    fn push_i32(buf: &mut Vec<u8>, value: i32) {
        buf.extend_from_slice(&value.to_le_bytes());
    }

    fn put_i32(buf: &mut [u8], offset: usize, value: i32) {
        buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }

    fn push_utf16le_fixed_ascii(buf: &mut Vec<u8>, text: &str, chars: usize) {
        let bytes = text.as_bytes();
        for i in 0..chars {
            buf.push(bytes.get(i).copied().unwrap_or(0));
            buf.push(0);
        }
    }

    fn append_leica_block(buf: &mut Vec<u8>, payload: &[u8]) -> i32 {
        let offset = buf.len();
        buf.resize(offset + 12, 0);
        push_i32(buf, payload.len() as i32);
        buf.extend_from_slice(payload);
        offset as i32
    }

    fn minimal_lei(filename: &str, declared_x: i32, declared_y: i32) -> Vec<u8> {
        const SERIES: i32 = 10;
        const IMAGES: i32 = 15;

        let header_offset = 32usize;
        let file_length = 32usize;

        let mut data = vec![0; 64];
        data[0..4].copy_from_slice(b"IIII");
        put_i32(&mut data, 12, header_offset as i32);

        let mut series_payload = Vec::new();
        push_i32(&mut series_payload, 1);
        push_i32(&mut series_payload, 1);
        push_i32(&mut series_payload, file_length as i32);
        push_i32(&mut series_payload, 3);
        series_payload.extend_from_slice(b"t\0i\0f\0");
        let series_offset = append_leica_block(&mut data, &series_payload);

        let mut images_payload = Vec::new();
        push_i32(&mut images_payload, 1);
        push_i32(&mut images_payload, declared_x);
        push_i32(&mut images_payload, declared_y);
        push_i32(&mut images_payload, 8);
        push_i32(&mut images_payload, 1);
        push_utf16le_fixed_ascii(&mut images_payload, filename, file_length);
        let images_offset = append_leica_block(&mut data, &images_payload);

        let tag_base = header_offset + 4;
        put_i32(&mut data, tag_base, SERIES);
        put_i32(&mut data, tag_base + 4, series_offset);
        put_i32(&mut data, tag_base + 8, IMAGES);
        put_i32(&mut data, tag_base + 12, images_offset);
        put_i32(&mut data, tag_base + 16, 0);
        put_i32(&mut data, tag_base + 20, 0);
        data
    }

    fn install_minimal_tiff_preamble(bytes: &mut [u8]) {
        assert!(bytes.len() >= 132);
        bytes[..128].fill(0);
        bytes[0..2].copy_from_slice(b"II");
        bytes[2..4].copy_from_slice(&42u16.to_le_bytes());
        bytes[4..8].copy_from_slice(&8u32.to_le_bytes());
        bytes[8..10].copy_from_slice(&9u16.to_le_bytes());

        let entries = [
            tiff_entry(256, 4, 1, 1),   // ImageWidth
            tiff_entry(257, 4, 1, 1),   // ImageLength
            tiff_entry(258, 3, 1, 8),   // BitsPerSample
            tiff_entry(259, 3, 1, 1),   // Compression
            tiff_entry(262, 3, 1, 1),   // PhotometricInterpretation
            tiff_entry(273, 4, 1, 122), // StripOffsets
            tiff_entry(277, 3, 1, 1),   // SamplesPerPixel
            tiff_entry(278, 4, 1, 1),   // RowsPerStrip
            tiff_entry(279, 4, 1, 1),   // StripByteCounts
        ];
        let mut offset = 10;
        for entry in entries {
            bytes[offset..offset + 12].copy_from_slice(&entry);
            offset += 12;
        }
        bytes[offset..offset + 4].copy_from_slice(&0u32.to_le_bytes());
        bytes[122] = 99;
        bytes[128..132].copy_from_slice(b"DICM");
    }

    #[test]
    fn open_returns_set_id_error_for_matching_corrupt_file() {
        let path = temp_path("corrupt.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nnot enough png data").unwrap();

        let err = match ImageReader::open(&path) {
            Ok(_) => panic!("corrupt PNG unexpectedly opened"),
            Err(err) => err,
        };

        assert!(
            matches!(err, BioFormatsError::Format(_) | BioFormatsError::Io(_)),
            "expected parser error, got {err:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn open_still_tries_extension_fallback_after_magic_set_id_error() {
        let path = temp_path("magic_png_but_fake.fake");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nnot enough png data").unwrap();

        let reader = ImageReader::open(&path).expect("fake extension fallback failed");

        assert_eq!(reader.metadata().size_x, 512);
        assert_eq!(reader.metadata().size_y, 512);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn terminal_unsupported_magic_still_allows_extension_fallback() {
        let path = temp_path("magic_opus_but_fake.fake");
        std::fs::write(&path, b"\x0a\x01not really opus").unwrap();

        let reader = ImageReader::open(&path).expect("fake extension fallback failed");

        assert_eq!(reader.metadata().size_x, 512);
        assert_eq!(reader.metadata().size_y, 512);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn open_missing_file_preserves_io_not_found() {
        let path = temp_path("missing.fake");
        let _ = std::fs::remove_file(&path);

        let err = match ImageReader::open(&path) {
            Ok(_) => panic!("missing file unexpectedly opened"),
            Err(err) => err,
        };

        assert!(
            matches!(err, BioFormatsError::Io(ref io) if io.kind() == std::io::ErrorKind::NotFound),
            "expected NotFound IO error, got {err:?}"
        );
    }

    #[test]
    fn tiff_wrapper_extension_dispatch_runs_before_generic_tiff_reader() {
        let path = temp_path("wrapper_metadata.ndpi");
        write_minimal_ndpi_tiff(&path, 20.0);

        let reader = ImageReader::open(&path).expect("NDPI wrapper dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("ndpi.magnification"),
            Some(MetadataValue::Float(value)) if (*value - 20.0).abs() < f64::EPSILON
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ndpi_extension_without_hamamatsu_tags_falls_back_to_generic_tiff() {
        let path = temp_path("plain_renamed.ndpi");
        write_minimal_tiff_with_description(&path, "plain TIFF");

        let mut reader = ImageReader::open(&path).expect("generic TIFF fallback failed");

        assert_eq!(reader.open_bytes(0).unwrap(), vec![7]);
        assert!(!reader
            .metadata()
            .series_metadata
            .contains_key("ndpi.magnification"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn generic_tif_wrapper_dispatch_uses_fluoview_metadata_signature() {
        let path = temp_path("fluoview_metadata.tif");
        write_minimal_tiff_with_extra_tags(
            &path,
            &[
                (270, 2, b"[Acquisition Parameters]\nLaser=488\n\0"),
                (34362, 4, &1u32.to_le_bytes()),
            ],
        );

        let reader = ImageReader::open(&path).expect("Fluoview wrapper dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("fluoview.Laser"),
            Some(MetadataValue::Int(value)) if *value == 488
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn generic_tif_fluoview_text_without_private_tags_uses_generic_tiff() {
        let path = temp_path("fluoview_text_only.tif");
        write_minimal_tiff_with_description(&path, "FLUOVIEW\nLaser=488\n");

        let reader = ImageReader::open(&path).expect("generic TIFF dispatch failed");

        assert_eq!(reader.metadata().size_x, 1);
        assert_eq!(reader.metadata().size_y, 1);
        assert!(
            !reader
                .metadata()
                .series_metadata
                .keys()
                .any(|key| key.starts_with("fluoview.")),
            "Fluoview text without Java private tags should not select Fluoview"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ome_tiff_dispatch_precedes_vendor_tiff_wrappers_like_java() {
        let path = temp_path("ome_with_vendor_words.tif");
        let mut ome_xml = br#"<OME xmlns="http://www.openmicroscopy.org/Schemas/OME/2016-06"><Image ID="Image:0" Name="Molecular Devices"><Pixels ID="Pixels:0" DimensionOrder="XYCZT" Type="uint8" SizeX="1" SizeY="1" SizeZ="1" SizeC="1" SizeT="1"><Channel ID="Channel:0:0" SamplesPerPixel="1"/><TiffData IFD="0" PlaneCount="1"/></Pixels></Image></OME>"#.to_vec();
        ome_xml.push(0);
        write_minimal_tiff_with_extra_tags(&path, &[(270, 2, &ome_xml)]);

        let reader = ImageReader::open(&path).expect("OME-TIFF dispatch failed");

        let ome = reader.ome_metadata().expect("OME metadata missing");
        assert!(matches!(
            ome.images.first().and_then(|image| image.name.as_deref()),
            Some("Molecular Devices")
        ));
        assert!(
            !reader
                .metadata()
                .series_metadata
                .keys()
                .any(|key| key.starts_with("moldev.")),
            "vendor TIFF wrapper claimed OME-TIFF before Java's OMETiffReader slot"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dng_tif_dispatch_runs_before_generic_tiff_like_java() {
        let path = temp_path("canon_dng_as_tif.tif");
        write_minimal_tiff_with_extra_tags(
            &path,
            &[
                (262, 3, &32803u16.to_le_bytes()),
                (271, 2, b"Canon\0"),
                (305, 2, b"Canon Digital Camera\0"),
                (33422, 3, &[0, 0, 1, 0, 1, 0, 2, 0]),
                (37398, 3, &1u16.to_le_bytes()),
            ],
        );

        let reader = ImageReader::open(&path).expect("DNG TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("format"),
            Some(MetadataValue::String(value)) if value == "DNG"
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn vectra_tif_software_dispatch_runs_before_generic_tiff_like_java() {
        let path = temp_path("vectra_as_tif.tif");
        write_minimal_tiff_with_software(&path, "PerkinElmer-QPI 1.0");

        let reader = ImageReader::open(&path).expect("Vectra TIFF dispatch failed");

        assert!(reader
            .metadata()
            .series_metadata
            .contains_key("qptiff.ifd_count"));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pyramid_tiff_faas_dispatch_runs_before_generic_tiff_like_java() {
        let path = temp_path("faas_pyramid_as_tif.tif");
        write_minimal_faas_pyramid_tiff(&path);

        let reader = ImageReader::open(&path).expect("Pyramid TIFF dispatch failed");

        assert_eq!(
            reader.resolution_count(),
            2,
            "Faas TIFF should dispatch to PyramidTiffReader before generic TIFF"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn ionpath_tif_software_dispatch_runs_before_generic_tiff_like_java() {
        let path = temp_path("ionpath_as_tif.tif");
        write_minimal_tiff_with_extra_tags(
            &path,
            &[
                (
                    270,
                    2,
                    br#"{"image.type":"SIMS","channel.mass":12,"channel.target":"C12"}"#,
                ),
                (305, 2, b"IonpathMIBI 1.0\0"),
            ],
        );

        let reader = ImageReader::open(&path).expect("Ionpath TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("hcs2.wrapper"),
            Some(MetadataValue::String(value)) if value == "IonpathMibiTiffReader"
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zeiss_tiff_companion_xml_dispatch_runs_before_generic_tiff_like_java() {
        let path = temp_path("zeiss_axiovision.tif");
        write_minimal_tiff_with_description(&path, "plain TIFF");
        let meta_path = PathBuf::from(format!("{}{}", path.display(), "_meta.xml"));
        let file_name = path.file_name().unwrap().to_string_lossy();
        std::fs::write(
            &meta_path,
            format!("<Root><V>{file_name}>Filename</V></Root>"),
        )
        .unwrap();

        let reader = ImageReader::open(&path).expect("Zeiss TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("format"),
            Some(MetadataValue::String(value)) if value == "Zeiss AxioVision TIFF"
        ));
        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(meta_path);
    }

    #[test]
    fn generic_tif_without_wrapper_signature_still_uses_generic_tiff() {
        let path = temp_path("plain_metadata.tif");
        write_minimal_tiff_with_description(&path, "Laser=488\n");

        let reader = ImageReader::open(&path).expect("generic TIFF dispatch failed");

        assert_eq!(reader.metadata().size_x, 1);
        assert_eq!(reader.metadata().size_y, 1);
        assert!(
            !reader
                .metadata()
                .series_metadata
                .keys()
                .any(|key| key.starts_with("fluoview.")),
            "plain TIFF should not be claimed by Fluoview wrapper"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn pattern_extension_preempts_tiff_magic_like_java_suffix_sufficient() {
        let tiff = temp_path("pattern_payload_source.tif");
        write_minimal_tiff_with_description(&tiff, "valid TIFF bytes");
        let pattern = temp_path("pattern_payload.pattern");
        std::fs::write(&pattern, std::fs::read(&tiff).unwrap()).unwrap();

        let err = match ImageReader::open(&pattern) {
            Ok(_) => panic!(".pattern payload was opened as a TIFF image"),
            Err(err) => err,
        };

        assert!(
            matches!(
                err,
                BioFormatsError::Io(_)
                    | BioFormatsError::Format(_)
                    | BioFormatsError::UnsupportedFormat(_)
            ),
            "unexpected error from FilePattern dispatch: {err:?}"
        );
        let _ = std::fs::remove_file(tiff);
        let _ = std::fs::remove_file(pattern);
    }

    #[test]
    fn dicom_with_tiff_preamble_dispatches_before_generic_tiff() {
        let path = temp_path("dicom_tiff_preamble.dcm");
        write_dicom(&path, 2, 1);
        let mut bytes = std::fs::read(&path).unwrap();
        install_minimal_tiff_preamble(&mut bytes);
        std::fs::write(&path, bytes).unwrap();

        let reader = ImageReader::open(&path).expect("DICOM-TIFF dispatch failed");

        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 1);
        assert!(
            reader
                .metadata()
                .series_metadata
                .contains_key("TransferSyntaxUID"),
            "TIFF preamble claimed file before DICOM"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn preambleless_dicom_dataset_dispatches_by_bytes() {
        let path = temp_path("preambleless.dcm");
        write_dicom(&path, 2, 1);
        let bytes = std::fs::read(&path).unwrap();
        std::fs::write(&path, &bytes[132..]).unwrap();

        let reader = ImageReader::open(&path).expect("preambleless DICOM dispatch failed");

        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 1);
        assert!(
            reader
                .metadata()
                .series_metadata
                .contains_key("TransferSyntaxUID"),
            "preambleless dataset was not parsed as DICOM"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn generic_tif_metamorph_prefix_without_end_tag_stays_generic_tiff() {
        let path = temp_path("metamorph_prefix_only.tif");
        write_minimal_tiff_with_description(
            &path,
            "<MetaData><prop id=\"image-name\" value=\"x\"/>",
        );

        let reader = ImageReader::open(&path).expect("generic TIFF dispatch failed");

        assert_eq!(reader.metadata().size_x, 1);
        assert_eq!(reader.metadata().size_y, 1);
        assert!(
            !reader
                .metadata()
                .series_metadata
                .contains_key("metamorph.wrapper"),
            "malformed MetaMorph prefix should not select MetamorphTiffReader"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn generic_tif_classic_metamorph_software_dispatches_before_generic_tiff() {
        let path = temp_path("classic_metamorph.tif");
        write_minimal_tiff_with_software(&path, "MetaMorph Offline 7.8");

        let reader = ImageReader::open(&path).expect("classic MetaMorph TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("format"),
            Some(MetadataValue::String(value)) if value == "MetaMorph STK"
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn micromanager_tif_dispatches_before_generic_tiff_like_java_readers_txt() {
        let dir = temp_dir("micromanager_tiff_entry");
        let path = dir.join("img_0.tif");
        let meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..Default::default()
        };
        ImageWriter::save(&path, &meta, &[vec![99]]).unwrap();
        std::fs::write(
            dir.join("metadata.txt"),
            r#"{
  "Summary": {
    "Width": 1,
    "Height": 1,
    "Channels": 1,
    "Slices": 1,
    "Frames": 1,
    "PixelType": "GRAY8",
    "MicroManagerVersion": "2.0"
  },
  "FrameKey-0-0-0": {
    "FileName": "img_0.tif"
  }
}"#,
        )
        .unwrap();

        let mut reader = ImageReader::open(&path).expect("MicroManager TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("format"),
            Some(MetadataValue::String(value)) if value == "MicroManager"
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![99]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn pcoraw_dispatch_runs_before_generic_tiff_and_reads_rec_companion() {
        let image = temp_path("pco_pair.pcoraw");
        let rec = image.with_extension("rec");
        write_minimal_tiff_with_description(&image, "PCO pixels");
        std::fs::write(&rec, "Exposure / Delay: 50 ms\n").unwrap();

        let reader = ImageReader::open(&image).expect("PCO-RAW dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("Exposure / Delay"),
            Some(MetadataValue::String(value)) if value == "50 ms"
        ));
        let _ = std::fs::remove_file(image);
        let _ = std::fs::remove_file(rec);
    }

    #[test]
    fn imacon_fff_dispatch_runs_before_generic_tiff() {
        let path = temp_path("imacon.fff");
        write_minimal_tiff_with_extra_tags(
            &path,
            &[
                (
                    34377,
                    2,
                    b"0\n1\n2\n3\nAda Lovelace\n5\nScan\n7\n20240102\n9\n030405+0000\0",
                ),
                (
                    50457,
                    2,
                    b"prefix <root><key>Camera</key><value>Imacon 949</value></root>\0",
                ),
            ],
        );

        let reader = ImageReader::open(&path).expect("Imacon dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("Camera"),
            Some(MetadataValue::String(value)) if value == "Imacon 949"
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn imacon_tiff_tag_dispatch_runs_before_generic_tiff() {
        let path = temp_path("imacon_tagged.tif");
        write_minimal_tiff_with_extra_tags(
            &path,
            &[(
                50457,
                2,
                b"<root><key>Scanner</key><value>Flextight</value></root>\0",
            )],
        );

        let reader = ImageReader::open(&path).expect("Imacon TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("Scanner"),
            Some(MetadataValue::String(value)) if value == "Flextight"
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn fei_tiff_tag_dispatch_runs_before_generic_tiff() {
        let path = temp_path("fei_tagged.tif");
        write_minimal_tiff_with_extra_tags(
            &path,
            &[(
                34682,
                2,
                b"[User]\nDate=01/02/2020\nTime=03:04:05 PM\nUser=Operator\n\0",
            )],
        );

        let reader = ImageReader::open(&path).expect("FEI TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("Software"),
            Some(MetadataValue::String(value)) if value == "Helios NanoLab"
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_raw_tif_make_dispatch_runs_before_generic_tiff() {
        let path = temp_path("nikon_raw_make.tif");
        write_minimal_tiff_with_extra_tags(
            &path,
            &[(271, 2, b"Nikon\0"), (37398, 3, &1u16.to_le_bytes())],
        );

        let reader = ImageReader::open(&path).expect("Nikon RAW TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("format"),
            Some(MetadataValue::String(value)) if value == "Nikon NEF"
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn nikon_elements_tiff_tag_dispatch_runs_before_generic_tiff() {
        let path = temp_path("nikon_elements_tagged.tif");
        write_minimal_tiff_with_extra_tags(
            &path,
            &[(65332, 2, b"<variant type=\"camera\" cameraName=\"Ti2\"/>\0")],
        );

        let reader = ImageReader::open(&path).expect("Nikon Elements TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("nikon.variant_count"),
            Some(MetadataValue::Int(value)) if *value == 1
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn simplepci_tiff_precedes_nikon_elements_like_java_readers_txt() {
        let path = temp_path("simplepci_and_nikon_elements.tif");
        write_minimal_tiff_with_extra_tags(
            &path,
            &[
                (270, 2, b"Created by Hamamatsu Inc.\n\0"),
                (65332, 2, b"<variant type=\"camera\" cameraName=\"Ti2\"/>\0"),
            ],
        );

        let reader = ImageReader::open(&path).expect("overlapping TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("hcs2.wrapper"),
            Some(MetadataValue::String(value)) if value == "SimplePciTiffReader"
        ));
        assert!(
            !reader
                .metadata()
                .series_metadata
                .contains_key("nikon.variant_count"),
            "Nikon Elements was selected before earlier SimplePCI"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn non_java_simplepci_extra_does_not_override_nikon_elements() {
        let path = temp_path("hcimage_and_nikon_elements.tif");
        write_minimal_tiff_with_extra_tags(
            &path,
            &[
                (270, 2, b"Created by SimplePCI HCImage\n\0"),
                (65332, 2, b"<variant type=\"camera\" cameraName=\"Ti2\"/>\0"),
            ],
        );

        let reader = ImageReader::open(&path).expect("overlapping TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("nikon.variant_count"),
            Some(MetadataValue::Int(value)) if *value == 1
        ));
        assert!(
            !matches!(
                reader.metadata().series_metadata.get("hcs2.wrapper"),
                Some(MetadataValue::String(value)) if value == "SimplePciTiffReader"
            ),
            "Rust-only SimplePCI text overrode Java-supported Nikon Elements"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn improvision_tiff_comment_dispatch_runs_before_generic_tiff() {
        let path = temp_path("improvision_tagged.tif");
        write_minimal_tiff_with_description(
            &path,
            "Improvision\nTotalZPlanes=1\nTotalChannels=1\nTotalTimepoints=1\n",
        );

        let reader = ImageReader::open(&path).expect("Improvision TIFF dispatch failed");

        assert!(matches!(
            reader
                .metadata()
                .series_metadata
                .get("improvision.TotalZPlanes"),
            Some(MetadataValue::Int(value)) if *value == 1
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn trestle_tiff_precedes_sis_like_java_readers_txt() {
        let path = temp_path("trestle_and_sis.tif");
        write_minimal_tiff_with_extra_tags(
            &path,
            &[
                (33432, 2, b"Copyright Trestle Corp.\0"),
                (33560, 4, &1u32.to_le_bytes()),
            ],
        );

        let reader = ImageReader::open(&path).expect("overlapping TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("hcs2.wrapper"),
            Some(MetadataValue::String(value)) if value == "TrestleReader"
        ));
        assert!(
            !reader
                .metadata()
                .series_metadata
                .contains_key("sis.wrapper"),
            "SIS was selected before earlier Trestle"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn photoshop_tiff_dispatch_runs_before_generic_tiff() {
        let path = temp_path("photoshop_layers.tif");
        write_minimal_tiff_with_extra_tags(&path, &[(37724, 7, b"8BPS\0")]);

        let reader = ImageReader::open(&path).expect("Photoshop TIFF dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("Photoshop layer count"),
            Some(MetadataValue::Int(value)) if *value == 0
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn extensionless_fixed_length_canon_crw_dispatches_like_java_byte_probe() {
        let path = temp_path("legacy_canon_raw");
        let file = std::fs::File::create(&path).unwrap();
        file.set_len(18_653_760).unwrap();
        drop(file);

        let reader = ImageReader::open(&path).expect("Canon RAW dispatch failed");

        assert_eq!(reader.metadata().size_x, 4080);
        assert_eq!(reader.metadata().size_y, 3048);
        assert_eq!(reader.metadata().size_c, 3);
        assert!(reader.metadata().is_rgb);
        assert!(reader.metadata().is_interleaved);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn extensionless_sbig_dispatch_reads_full_java_probe_window() {
        let path = temp_path("sbig_no_suffix");
        write_minimal_sbig(&path);

        let mut reader = ImageReader::open(&path).expect("SBIG dispatch failed");

        assert_eq!(reader.metadata().size_x, 1);
        assert_eq!(reader.metadata().size_y, 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![17, 0]);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn dicom_name_detection_includes_jpeg2000_transfer_extensions() {
        let reader = crate::formats::dicom::DicomReader::new();

        assert!(reader.is_this_type_by_name(&PathBuf::from("ct.j2ki")));
        assert!(reader.is_this_type_by_name(&PathBuf::from("mr.j2kr")));
    }

    #[test]
    fn csv_dispatches_to_text_reader() {
        let path = temp_path("numeric_grid.csv");
        std::fs::write(&path, "1,2\n3,4\n").unwrap();

        let mut reader = ImageReader::open(&path).expect("CSV TextReader dispatch failed");
        let bytes = reader.open_bytes(0).unwrap();
        let first = f32::from_be_bytes(bytes[0..4].try_into().unwrap());
        let last = f32::from_be_bytes(bytes[12..16].try_into().unwrap());

        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.metadata().pixel_type, PixelType::Float32);
        assert!(!reader.metadata().is_little_endian);
        assert_eq!(first, 1.0);
        assert_eq!(last, 4.0);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn generic_tif_hcs_wrapper_dispatch_uses_metadata_signature() {
        let path = temp_path("simplepci_metadata.tif");
        write_minimal_tiff_with_description(&path, "Created by Hamamatsu Inc.\n");

        let reader = ImageReader::open(&path).expect("SimplePCI wrapper dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("hcs2.wrapper"),
            Some(MetadataValue::String(value)) if value == "SimplePciTiffReader"
        ));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn generic_tif_hcs_words_without_vendor_signature_still_uses_generic_tiff() {
        let path = temp_path("plain_hcs_words.tif");
        write_minimal_tiff_with_description(&path, "Plate image with well A01\n");

        let reader = ImageReader::open(&path).expect("generic TIFF dispatch failed");

        assert!(
            !reader
                .metadata()
                .series_metadata
                .contains_key("hcs2.wrapper"),
            "plain TIFF should not be claimed by HCS2 TIFF wrapper"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn prairie_companion_tiff_entry_dispatches_before_generic_tiff() {
        let dir = temp_dir("prairie_tiff_entry");
        let tiff = dir.join("scan_001.tif");
        write_minimal_tiff_with_description(&tiff, "plain TIFF");
        std::fs::write(
            dir.join("scan.xml"),
            r#"<PVScan>
<PVStateValue key="pixelsPerLine" value="1"/>
<PVStateValue key="linesPerFrame" value="1"/>
<PVStateValue key="bitDepth" value="8"/>
<Sequence>
<Frame index="0">
<File filename="scan_001.tif" channel="1"/>
</Frame>
</Sequence>
</PVScan>"#,
        )
        .unwrap();

        let reader = ImageReader::open(&tiff).expect("Prairie TIFF entry should dispatch");

        assert!(matches!(
            reader.metadata().series_metadata.get("format"),
            Some(MetadataValue::String(value)) if value == "Prairie TIFF"
        ));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prairie_tiff_predispatch_requires_xml_file_reference() {
        let dir = temp_dir("prairie_tiff_unrelated");
        let prairie_tiff = dir.join("scan_001.tif");
        let unrelated_tiff = dir.join("unrelated.tif");
        write_minimal_tiff_with_description(&prairie_tiff, "plain TIFF");
        write_minimal_tiff_with_description(&unrelated_tiff, "plain TIFF");
        std::fs::write(
            dir.join("scan.xml"),
            r#"<PVScan>
<PVStateValue key="pixelsPerLine" value="1"/>
<PVStateValue key="linesPerFrame" value="1"/>
<PVStateValue key="bitDepth" value="8"/>
<Sequence>
<Frame index="0">
<File filename="scan_001.tif" channel="1"/>
</Frame>
</Sequence>
</PVScan>"#,
        )
        .unwrap();

        let reader = ImageReader::open(&unrelated_tiff).expect("unrelated TIFF should open");

        assert!(
            !matches!(
                reader.metadata().series_metadata.get("format"),
                Some(MetadataValue::String(value)) if value == "Prairie TIFF"
            ),
            "unreferenced TIFF was claimed by Prairie pre-dispatch"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn prairie_tiff_predispatch_rejects_escaping_xml_reference() {
        let dir = temp_dir("prairie_tiff_escape");
        let tiff = dir.join("scan_001.tif");
        write_minimal_tiff_with_description(&tiff, "plain TIFF");
        std::fs::write(
            dir.join("scan.xml"),
            r#"<PVScan>
<PVStateValue key="pixelsPerLine" value="1"/>
<PVStateValue key="linesPerFrame" value="1"/>
<PVStateValue key="bitDepth" value="8"/>
<Sequence>
<Frame index="0">
<File filename="../scan_001.tif" channel="1"/>
</Frame>
</Sequence>
</PVScan>"#,
        )
        .unwrap();

        let reader = ImageReader::open(&tiff).expect("TIFF should still open generically");

        assert!(
            !matches!(
                reader.metadata().series_metadata.get("format"),
                Some(MetadataValue::String(value)) if value == "Prairie TIFF"
            ),
            "escaping XML reference was accepted by Prairie pre-dispatch"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn leica_lei_companion_dispatches_before_generic_tiff_without_private_tag() {
        let dir = temp_dir("leica_lei_companion");
        let tiff = dir.join("sample_001.tif");
        let lei = dir.join("sample.lei");
        let meta = ImageMetadata {
            size_x: 2,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..Default::default()
        };
        ImageWriter::save(&tiff, &meta, &[vec![41, 42]]).unwrap();
        std::fs::write(&lei, minimal_lei("sample_001.tif", 99, 88)).unwrap();

        let mut reader = ImageReader::open(&tiff).expect("LEI companion dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("format"),
            Some(MetadataValue::String(value)) if value == "Leica LEI"
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![41, 42]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn leica_tcs_single_multipage_tiff_maps_later_planes_to_later_pages() {
        let dir = temp_dir("leica_tcs_pages");
        let tiff = dir.join("stack.tif");
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
        ImageWriter::save(&tiff, &meta, &[vec![11], vec![22]]).unwrap();
        let xml = dir.join("scan.xml");
        std::fs::write(
            &xml,
            r#"<LEICA>
<Image Width="1" Height="1"/>
<DimensionDescription DimID="1" NumberOfElements="1" BytesInc="1"/>
<DimensionDescription DimID="2" NumberOfElements="1"/>
<DimensionDescription DimID="3" NumberOfElements="2"/>
<Attachment Name="stack.tif"/>
</LEICA>"#,
        )
        .unwrap();

        let mut reader = crate::formats::prairie::TcsReader::new();
        reader.set_id(&xml).unwrap();

        assert_eq!(reader.metadata().image_count, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![11]);
        assert_eq!(reader.open_bytes(1).unwrap(), vec![22]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn leica_tcs_companion_tiff_entry_dispatches_before_generic_tiff() {
        let dir = temp_dir("leica_tcs_tiff_entry");
        let tiff = dir.join("stack.tif");
        let meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..Default::default()
        };
        ImageWriter::save(&tiff, &meta, &[vec![33]]).unwrap();
        std::fs::write(
            dir.join("scan.xml"),
            r#"<LEICA>
<Image Width="1" Height="1"/>
<DimensionDescription DimID="1" NumberOfElements="1" BytesInc="1"/>
<DimensionDescription DimID="2" NumberOfElements="1"/>
<Attachment Name="stack.tif"/>
</LEICA>"#,
        )
        .unwrap();

        let mut reader = ImageReader::open(&tiff).expect("Leica TCS TIFF entry should dispatch");

        assert!(matches!(
            reader.metadata().series_metadata.get("format"),
            Some(MetadataValue::String(value)) if value == "Leica TCS TIFF"
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![33]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn leica_lei_companion_excludes_tcs_tiff_predispatch_like_java() {
        let dir = temp_dir("leica_lei_excludes_tcs_tiff");
        let tiff = dir.join("stack_001.tif");
        let lei = dir.join("stack.lei");
        let meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..Default::default()
        };
        ImageWriter::save(&tiff, &meta, &[vec![44]]).unwrap();
        std::fs::write(&lei, minimal_lei("stack_001.tif", 1, 1)).unwrap();
        std::fs::write(
            dir.join("stack.xml"),
            r#"<LEICA>
<Image Width="1" Height="1"/>
<DimensionDescription DimID="1" NumberOfElements="1" BytesInc="1"/>
<DimensionDescription DimID="2" NumberOfElements="1"/>
<Attachment Name="stack_001.tif"/>
</LEICA>"#,
        )
        .unwrap();

        let mut reader = ImageReader::open(&tiff).expect("LEI companion dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("format"),
            Some(MetadataValue::String(value)) if value == "Leica LEI"
        ));
        assert_eq!(reader.open_bytes(0).unwrap(), vec![44]);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn leica_tcs_single_multipage_tiff_rejects_missing_exact_page() {
        let dir = temp_dir("leica_tcs_page_out_of_range");
        let tiff = dir.join("stack.tif");
        let meta = ImageMetadata {
            size_x: 1,
            size_y: 1,
            size_z: 1,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 1,
            ..Default::default()
        };
        ImageWriter::save(&tiff, &meta, &[vec![11]]).unwrap();
        let xml = dir.join("scan.xml");
        std::fs::write(
            &xml,
            r#"<LEICA>
<Image Width="1" Height="1"/>
<DimensionDescription DimID="1" NumberOfElements="1" BytesInc="1"/>
<DimensionDescription DimID="2" NumberOfElements="1"/>
<DimensionDescription DimID="3" NumberOfElements="2"/>
<Attachment Name="stack.tif"/>
</LEICA>"#,
        )
        .unwrap();

        let mut reader = crate::formats::prairie::TcsReader::new();
        reader.set_id(&xml).unwrap();
        let err = reader.open_bytes(1).unwrap_err();

        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("TIFF page 1 out of range")),
            "unexpected error: {err:?}"
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn registered_unsupported_stubs_do_not_open_with_fake_metadata() {
        for (name, expected) in [
            (
                "sample.mvd2",
                "Volocity MVD2 native Metakit decoding is unsupported",
            ),
            ("sample.cif", "FlowSight CIF is not TIFF-like"),
            // NOTE: LOF (Leica) is no longer a stub — LofReader is a real
            // reader now; it rejects fake data with its own header error.
            // NOTE: OIR and ACFF (Volocity clipping) are no longer stubs —
            // OirReader and VolocityClippingReader are now real readers. They
            // still reject fake data (no fabricated metadata), just with
            // reader-specific messages covered by their own unit tests, so they
            // are intentionally excluded from this not-yet-implemented list.
            // NOTE: XRM/TXRM are no longer stubs — ZeissXrmReader is a real CFB-based
            // reader. It rejects fake/short input with a genuine CFB error
            // ("XRM CFB open: Invalid CFB file ..."), not fabricated metadata.
            // That rejection is covered by xrm.rs's own unit tests, so XRM is
            // intentionally excluded from this not-yet-implemented stub list.
            // EPS now rasterizes inline PostScript images and reads embedded
            // TIFF previews; fake data is still rejected (no fabricated metadata),
            // just with a content-specific message.
            (
                "sample.eps",
                "EPS: not a PostScript file and no embedded TIFF preview found",
            ),
            ("sample.1sc", "Bio-Rad GEL file is too short"),
            (
                "sample.obf",
                "Imspector OBF/MSR native stack decoding is unsupported",
            ),
            (
                "sample.msr",
                "Imspector OBF/MSR native stack decoding is unsupported",
            ),
            ("sample.vms", "Not a Hamamatsu VMS/VMU text index file"),
            ("sample.vmu", "Not a Hamamatsu VMS/VMU text index file"),
            (
                "sample.l2d",
                "Li-Cor L2D file is missing LI-COR LI2D marker",
            ),
            ("sample.ptu", "PicoQuant PTU missing PQTTTR magic"),
            (
                "sample.vws",
                "TillVision file contains no supported companion PST/INF pixels",
            ),
            // NOTE: Bruker OPUS (.abs/.0) and ISS Vista FLIM (.iss) were removed
            // entirely — they were fabricated readers for formats Bio-Formats has
            // no reader for. (Bruker MRI/ParaVision is now a real reader; SkyScan
            // microCT remains MicroCtReader.)
            // NOTE: sample.gel was removed here: GEL is no longer a stub.
            // extended::GelReader is a real Molecular Dynamics GEL reader (TIFF-based),
            // so on fake data it rejects via the underlying TIFF parser
            // ("Not a TIFF file: bad byte-order mark") rather than fabricating metadata.
        ] {
            let path = temp_path(name);
            if name == "sample.obf" || name == "sample.msr" {
                let mut bytes = Vec::new();
                bytes.extend_from_slice(b"OMAS_BF\n");
                bytes.extend_from_slice(&0xffffu16.to_le_bytes());
                bytes.extend_from_slice(&1i32.to_le_bytes());
                bytes.extend_from_slice(&0u64.to_le_bytes());
                bytes.extend_from_slice(&0i32.to_le_bytes());
                std::fs::write(&path, bytes).unwrap();
            } else {
                std::fs::write(&path, b"not a real image").unwrap();
            }

            let err = match ImageReader::open(&path) {
                Ok(_) => panic!("{name}: placeholder reader opened fake data"),
                Err(err) => err,
            };

            assert!(
                matches!(
                    err,
                    BioFormatsError::UnsupportedFormat(ref message)
                        | BioFormatsError::Format(ref message)
                        if message.contains(expected)
                ),
                "expected rejection message containing {expected:?}, got {err:?}"
            );
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn visitech_registry_opens_metadata_only_xys_but_rejects_hcs_without_tiffs() {
        let dir = temp_path("hcs_no_tiffs_dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("sample Report.html"),
            b"Image dimensions: (2, 2)\nNumber of steps: 1\nMicroscope XY: 0\nImage bit depth: 16\nChannel Selection: 1\nTime Series; 1\n",
        )
        .unwrap();

        let xys = dir.join("sample.xys");
        std::fs::write(&xys, b"no pixel marker here").unwrap();
        let mut reader = ImageReader::open(&xys).expect("metadata-only Visitech should open");
        assert_eq!(reader.metadata().image_count, 1);
        assert_eq!(reader.open_bytes(0).unwrap(), vec![0; 8]);

        let xdce = dir.join("sample.xdce");
        std::fs::write(&xdce, b"<InCell Width=\"2\" Height=\"2\"/>").unwrap();
        let err = match ImageReader::open(&xdce) {
            Ok(_) => panic!("HCS index without companion TIFFs opened fake data"),
            Err(err) => err,
        };
        assert!(
            matches!(
                err,
                BioFormatsError::UnsupportedFormat(ref message) | BioFormatsError::Format(ref message)
                    if message.contains("no TIFF image files found referenced in index")
            ),
            "expected HCS missing-TIFF rejection, got {err:?}"
        );

        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn registered_hdf5_readers_reject_unknown_layouts_without_fake_metadata() {
        for (name, expected) in [
            (
                "unknown.ch5",
                "CellH5: no image datasets found in supported sample/plate channel layouts",
            ),
            (
                "unknown.mnc",
                "MINC/HDF5: could not find image dataset in known paths",
            ),
        ] {
            let path = temp_path(name);
            // Create an empty-but-valid HDF5 file (no image datasets) so the
            // CellH5/MINC readers exercise their "not found" rejection paths.
            let mut wf = hdf5_pure_rust::WritableFile::create(&path).unwrap();
            wf.flush().unwrap();
            drop(wf);

            let err = match ImageReader::open(&path) {
                Ok(_) => panic!("HDF5 placeholder reader opened fake data"),
                Err(err) => err,
            };

            assert!(
                matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains(expected)),
                "expected unsupported message containing {expected:?}, got {err:?}"
            );
            let _ = std::fs::remove_file(path);
        }
    }

    #[test]
    fn imspector_magic_rejects_without_fake_metadata() {
        let path = temp_path("magic_imspector.obf");
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"OMAS_BF\n");
        bytes.extend_from_slice(&0xffffu16.to_le_bytes());
        bytes.extend_from_slice(&1i32.to_le_bytes());
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0i32.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        let err = match ImageReader::open(&path) {
            Ok(_) => panic!("Imspector magic placeholder opened fake data"),
            Err(err) => err,
        };

        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Imspector OBF/MSR native stack decoding is unsupported")),
            "expected unsupported Imspector message, got {err:?}"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn zip_reader_opens_inner_image_and_rejects_unrecognized_entries() {
        // Like the Java ZipReader, ZipReader now delegates the primary archive
        // entry to the auto-detecting ImageReader (any inner format, not only
        // TIFF). A ZIP whose primary entry is a recognized image opens and
        // reads its real pixels; a ZIP whose entries match no reader is
        // rejected (the inner auto-detection fails and the error propagates).

        // Positive case: a real 2x2 uint8 TIFF entry decodes to its real pixels.
        let tiff_src = temp_path("zip_inner_source.tif");
        let mut meta = crate::common::metadata::ImageMetadata::default();
        meta.size_x = 2;
        meta.size_y = 2;
        meta.pixel_type = crate::common::pixel_type::PixelType::Uint8;
        meta.image_count = 1;
        let pixels = vec![10u8, 20, 30, 40];
        crate::writer_registry::ImageWriter::save(&tiff_src, &meta, &[pixels.clone()]).unwrap();
        let tiff_bytes = std::fs::read(&tiff_src).unwrap();
        let _ = std::fs::remove_file(&tiff_src);

        let tiff_zip = temp_path("inner_tiff.zip");
        write_zip_entry(&tiff_zip, "frame.tif", &tiff_bytes);
        let mut reader = ImageReader::open(&tiff_zip).expect("ZIP with inner TIFF should open");
        assert_eq!(reader.metadata().size_x, 2);
        assert_eq!(reader.metadata().size_y, 2);
        assert_eq!(reader.open_bytes(0).unwrap(), pixels);
        let _ = reader.close();
        let _ = std::fs::remove_file(&tiff_zip);

        // Negative case: an entry name + bytes that match no registered reader.
        let no_image_path = temp_path("no_image.zip");
        write_zip_entry(&no_image_path, "data.unknownfmt", b"not image data at all");

        let err = match ImageReader::open(&no_image_path) {
            Ok(_) => panic!("ZIP without a recognized image entry opened"),
            Err(err) => err,
        };
        // The inner ImageReader finds no matching reader and rejects with
        // UnsupportedFormat (the extracted entry path it could not detect).
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(_)),
            "expected unsupported ZIP message, got {err:?}"
        );
        let _ = std::fs::remove_file(&no_image_path);
    }

    fn write_zip_entry(path: &PathBuf, name: &str, bytes: &[u8]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
        zip.finish().unwrap();
    }

    fn write_minimal_ndpi_tiff(path: &PathBuf, magnification: f32) {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&8u32.to_le_bytes());

        let entries = [
            tiff_entry(256, 4, 1, 1),                          // ImageWidth
            tiff_entry(257, 4, 1, 1),                          // ImageLength
            tiff_entry(258, 3, 1, 8),                          // BitsPerSample
            tiff_entry(259, 3, 1, 1),                          // Compression
            tiff_entry(262, 3, 1, 1),                          // PhotometricInterpretation
            tiff_entry(273, 4, 1, 8 + 2 + 12 * 12 + 4),        // StripOffsets
            tiff_entry(277, 3, 1, 1),                          // SamplesPerPixel
            tiff_entry(278, 4, 1, 1),                          // RowsPerStrip
            tiff_entry(279, 4, 1, 1),                          // StripByteCounts
            tiff_entry(284, 3, 1, 1),                          // PlanarConfiguration
            tiff_entry(65421, 11, 1, magnification.to_bits()), // NDPI magnification
            tiff_entry(65449, 4, 1, 1),                        // NDPI metadata tag
        ];
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.push(7);

        std::fs::write(path, bytes).unwrap();
    }

    fn write_minimal_tiff_with_software(path: &PathBuf, software: &str) {
        let mut soft = software.as_bytes().to_vec();
        soft.push(0);

        let ifd_entry_count = 11u32;
        let ifd_start = 8u32;
        let soft_start = ifd_start + 2 + ifd_entry_count * 12 + 4;
        let pixel_start = soft_start + soft.len() as u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());

        let entries = [
            tiff_entry(256, 4, 1, 1),                          // ImageWidth
            tiff_entry(257, 4, 1, 1),                          // ImageLength
            tiff_entry(258, 3, 1, 8),                          // BitsPerSample
            tiff_entry(259, 3, 1, 1),                          // Compression
            tiff_entry(262, 3, 1, 1),                          // PhotometricInterpretation
            tiff_entry(273, 4, 1, pixel_start),                // StripOffsets
            tiff_entry(277, 3, 1, 1),                          // SamplesPerPixel
            tiff_entry(278, 4, 1, 1),                          // RowsPerStrip
            tiff_entry(279, 4, 1, 1),                          // StripByteCounts
            tiff_entry(284, 3, 1, 1),                          // PlanarConfiguration
            tiff_entry(305, 2, soft.len() as u32, soft_start), // Software
        ];

        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&soft);
        bytes.push(7);

        std::fs::write(path, bytes).unwrap();
    }

    fn write_minimal_faas_pyramid_tiff(path: &PathBuf) {
        let software = b"Faas\0";
        let ifd_entry_count = 11u32;
        let ifd_size = 2 + ifd_entry_count * 12 + 4;
        let ifd0_start = 8u32;
        let ifd1_start = ifd0_start + ifd_size;
        let software_start = ifd1_start + ifd_size;
        let pixel0_start = software_start + software.len() as u32;
        let pixel1_start = pixel0_start + 4;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd0_start.to_le_bytes());

        bytes.extend_from_slice(&(ifd_entry_count as u16).to_le_bytes());
        let entries0 = [
            tiff_entry(256, 4, 1, 2),                                  // ImageWidth
            tiff_entry(257, 4, 1, 2),                                  // ImageLength
            tiff_entry(258, 3, 1, 8),                                  // BitsPerSample
            tiff_entry(259, 3, 1, 1),                                  // Compression
            tiff_entry(262, 3, 1, 1),                                  // PhotometricInterpretation
            tiff_entry(273, 4, 1, pixel0_start),                       // StripOffsets
            tiff_entry(277, 3, 1, 1),                                  // SamplesPerPixel
            tiff_entry(278, 4, 1, 2),                                  // RowsPerStrip
            tiff_entry(279, 4, 1, 4),                                  // StripByteCounts
            tiff_entry(284, 3, 1, 1),                                  // PlanarConfiguration
            tiff_entry(305, 2, software.len() as u32, software_start), // Software
        ];
        for entry in entries0 {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&ifd1_start.to_le_bytes());

        bytes.extend_from_slice(&(ifd_entry_count as u16).to_le_bytes());
        let entries1 = [
            tiff_entry(256, 4, 1, 1),                                  // ImageWidth
            tiff_entry(257, 4, 1, 1),                                  // ImageLength
            tiff_entry(258, 3, 1, 8),                                  // BitsPerSample
            tiff_entry(259, 3, 1, 1),                                  // Compression
            tiff_entry(262, 3, 1, 1),                                  // PhotometricInterpretation
            tiff_entry(273, 4, 1, pixel1_start),                       // StripOffsets
            tiff_entry(277, 3, 1, 1),                                  // SamplesPerPixel
            tiff_entry(278, 4, 1, 1),                                  // RowsPerStrip
            tiff_entry(279, 4, 1, 1),                                  // StripByteCounts
            tiff_entry(284, 3, 1, 1),                                  // PlanarConfiguration
            tiff_entry(305, 2, software.len() as u32, software_start), // Software
        ];
        for entry in entries1 {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());

        bytes.extend_from_slice(software);
        bytes.extend_from_slice(&[1, 2, 3, 4]);
        bytes.push(5);

        std::fs::write(path, bytes).unwrap();
    }

    fn write_minimal_tiff_with_description(path: &PathBuf, description: &str) {
        let mut desc = description.as_bytes().to_vec();
        desc.push(0);

        let ifd_entry_count = 11u32;
        let ifd_start = 8u32;
        let desc_start = ifd_start + 2 + ifd_entry_count * 12 + 4;
        let pixel_start = desc_start + desc.len() as u32;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());

        let entries = [
            tiff_entry(256, 4, 1, 1),                          // ImageWidth
            tiff_entry(257, 4, 1, 1),                          // ImageLength
            tiff_entry(258, 3, 1, 8),                          // BitsPerSample
            tiff_entry(259, 3, 1, 1),                          // Compression
            tiff_entry(262, 3, 1, 1),                          // PhotometricInterpretation
            tiff_entry(270, 2, desc.len() as u32, desc_start), // ImageDescription
            tiff_entry(273, 4, 1, pixel_start),                // StripOffsets
            tiff_entry(277, 3, 1, 1),                          // SamplesPerPixel
            tiff_entry(278, 4, 1, 1),                          // RowsPerStrip
            tiff_entry(279, 4, 1, 1),                          // StripByteCounts
            tiff_entry(284, 3, 1, 1),                          // PlanarConfiguration
        ];

        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&desc);
        bytes.push(7);

        std::fs::write(path, bytes).unwrap();
    }

    fn write_minimal_tiff_with_extra_tags(path: &PathBuf, extras: &[(u16, u16, &[u8])]) {
        let ifd_entry_count = 10u32 + extras.len() as u32;
        let ifd_start = 8u32;
        let mut next_offset = ifd_start + 2 + ifd_entry_count * 12 + 4;

        let mut extra_entries = Vec::new();
        let mut extra_data = Vec::new();
        for &(tag, field_type, data) in extras {
            extra_entries.push(tiff_entry(tag, field_type, data.len() as u32, next_offset));
            extra_data.extend_from_slice(data);
            next_offset += data.len() as u32;
        }
        let pixel_start = next_offset;

        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&ifd_start.to_le_bytes());

        let entries = [
            tiff_entry(256, 4, 1, 1),           // ImageWidth
            tiff_entry(257, 4, 1, 1),           // ImageLength
            tiff_entry(258, 3, 1, 8),           // BitsPerSample
            tiff_entry(259, 3, 1, 1),           // Compression
            tiff_entry(262, 3, 1, 1),           // PhotometricInterpretation
            tiff_entry(273, 4, 1, pixel_start), // StripOffsets
            tiff_entry(277, 3, 1, 1),           // SamplesPerPixel
            tiff_entry(278, 4, 1, 1),           // RowsPerStrip
            tiff_entry(279, 4, 1, 1),           // StripByteCounts
            tiff_entry(284, 3, 1, 1),           // PlanarConfiguration
        ];

        bytes.extend_from_slice(&(ifd_entry_count as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        for entry in extra_entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&extra_data);
        bytes.push(7);

        std::fs::write(path, bytes).unwrap();
    }

    fn tiff_entry(tag: u16, field_type: u16, count: u32, value_or_offset: u32) -> [u8; 12] {
        let mut entry = [0u8; 12];
        entry[0..2].copy_from_slice(&tag.to_le_bytes());
        entry[2..4].copy_from_slice(&field_type.to_le_bytes());
        entry[4..8].copy_from_slice(&count.to_le_bytes());
        entry[8..12].copy_from_slice(&value_or_offset.to_le_bytes());
        entry
    }
}
