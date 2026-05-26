use std::path::Path;

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::peek_header;
use crate::common::metadata::ImageMetadata;
use crate::common::ome_metadata::OmeMetadata;
use crate::common::reader::FormatReader;

/// The top-level reader that auto-detects the file format and delegates to the
/// appropriate format-specific reader.
pub struct ImageReader {
    inner: Box<dyn FormatReader>,
}

/// Returns all registered format readers. Used internally by `ImageReader` and `Memoizer`.
pub fn all_readers_pub() -> Vec<Box<dyn FormatReader>> {
    all_readers()
}

fn all_readers() -> Vec<Box<dyn FormatReader>> {
    vec![
        // Dedicated readers first (most precise magic bytes)
        Box::new(crate::formats::zip::ZipReader::new()),
        Box::new(crate::formats::imaris::ImarisReader::new()),
        // HDF5-based formats (extension-only, must come after ImarisReader magic check)
        Box::new(crate::formats::cellh5::CellH5Reader::new()), // .ch5
        Box::new(crate::formats::bdv::BdvReader::new()),       // .h5
        Box::new(crate::formats::viff::ViffReader::new()),
        Box::new(crate::formats::mias::Al3dReader::new()),
        Box::new(crate::formats::perkinelmer::OpenlabRawReader::new()),
        Box::new(crate::formats::incell::InCellReader::new()),
        Box::new(crate::tiff::TiffReader::new()),
        Box::new(crate::formats::png::PngReader::new()),
        Box::new(crate::formats::jpeg::JpegReader::new()),
        Box::new(crate::formats::bmp::BmpReader::new()),
        Box::new(crate::formats::czi::CziReader::new()),
        Box::new(crate::formats::nd2::Nd2Reader::new()),
        Box::new(crate::formats::lif::LifReader::new()),
        Box::new(crate::formats::mrc::MrcReader::new()),
        Box::new(crate::formats::fits::FitsReader::new()),
        Box::new(crate::formats::nrrd::NrrdReader::new()),
        Box::new(crate::formats::metaimage::MetaImageReader::new()),
        Box::new(crate::formats::ics::IcsReader::new()),
        Box::new(crate::formats::dicom::DicomReader::new()),
        Box::new(crate::formats::nifti::NiftiReader::new()),
        Box::new(crate::formats::gatan::GatanReader::new()),
        // Generic raster wrappers (via image crate)
        Box::new(crate::formats::raster::gif_reader()),
        Box::new(crate::formats::raster::webp_reader()),
        Box::new(crate::formats::raster::pnm_reader()),
        Box::new(crate::formats::raster::hdr_reader()),
        Box::new(crate::formats::raster::exr_reader()),
        Box::new(crate::formats::raster::dds_reader()),
        Box::new(crate::formats::raster::farbfeld_reader()),
        // Additional scientific formats
        Box::new(crate::formats::biorad::BioRadReader::new()),
        Box::new(crate::formats::deltavision::DeltavisionReader::new()),
        Box::new(crate::formats::spe::SpeReader::new()),
        Box::new(crate::formats::andor::AndorSifReader::new()),
        Box::new(crate::formats::amira::AmiraReader::new()),
        Box::new(crate::formats::amira::SpiderReader::new()),
        Box::new(crate::formats::imagic::ImagicReader::new()),
        Box::new(crate::formats::flim::SdtReader::new()),
        Box::new(crate::formats::flim::LiFlimReader::new()),
        Box::new(crate::formats::clinical::Ecat7Reader::new()),
        Box::new(crate::formats::clinical::FdfReader::new()),
        Box::new(crate::formats::hamamatsu::DcimgReader::new()),
        Box::new(crate::formats::norpix::NorpixReader::new()),
        Box::new(crate::formats::norpix::IplabReader::new()),
        Box::new(crate::formats::ome::OmeXmlReader::new()),
        Box::new(crate::formats::olympus::OifReader::new()),
        // Magic-byte detected formats
        Box::new(crate::formats::pcx::PcxReader::new()),
        Box::new(crate::formats::photoshop::PsdReader::new()),
        Box::new(crate::formats::aim::AimReader::new()),
        // Prairie/Leica XML+TIFF series (magic-byte detection via XML content)
        Box::new(crate::formats::prairie::PrairieReader::new()),
        Box::new(crate::formats::prairie::LeicaTcsReader::new()),
        // EPS/PostScript
        Box::new(crate::formats::eps::EpsReader::new()),
        // Extension-only TIFF-based formats (no distinct magic bytes)
        Box::new(crate::formats::lsm::LsmReader::new()),
        Box::new(crate::formats::metamorph::MetamorphReader::new()),
        Box::new(crate::formats::micromanager::MicromanagerReader::new()),
        // OpenSlide-based whole-slide formats (MRXS, VMS, BIF, etc.)
        #[cfg(feature = "openslide")]
        Box::new(crate::formats::openslide_reader::OpenSlideReader::new()),
        // Whole-slide TIFF wrappers (extension-only)
        Box::new(crate::formats::svs::WholeSlideTiffReader::new()),
        // Extension-only Inveon (hdr+img pair, extension-only detection)
        Box::new(crate::formats::clinical::InveonReader::new()),
        // SimFCS FLIM (extension-only)
        Box::new(crate::formats::simfcs::SimfcsReader::new()),
        Box::new(crate::formats::simfcs::LambertFlimReader::new()),
        // AFM formats (extension-only)
        Box::new(crate::formats::afm::TopoMetrixReader::new()),
        Box::new(crate::formats::afm::UnisokuReader::new()),
        // LIM / TillVision (extension-only)
        Box::new(crate::formats::lim::LimReader::new()),
        Box::new(crate::formats::lim::TillVisionReader::new()),
        // AIM/ISQ extension-only fallback
        // DM2 (extension-only, Gatan)
        Box::new(crate::formats::gatan::Dm2Reader::new()),
        // Extension-only (no magic bytes)
        Box::new(crate::formats::raster::tga_reader()),
        // New format readers (extension-only)
        Box::new(crate::formats::fake::FakeReader::new()),
        Box::new(crate::formats::visitech::VisitechReader::new()),
        Box::new(crate::formats::perkinelmer::PerkinElmerReader::new()),
        Box::new(crate::formats::perkinelmer::PhotonDynamicsReader::new()),
        Box::new(crate::formats::mias::CellWorxReader::new()),
        Box::new(crate::formats::mias::OxfordInstrumentsReader::new()),
        // MIAS (Beckman Coulter): well/field/Z/C/T TIFF series. Detected by name
        // only (Well<xxxx> directory + mode/z naming); generic TiffReader still
        // wins auto-detection of plain .tif via the magic pass.
        Box::new(crate::formats::mias::MiasReader::new()),
        Box::new(crate::formats::sem::FeiPhilipsReader::new()),
        // FEI SER (magic-byte detected: 0x97 0x01)
        Box::new(crate::formats::mias::FeiSerReader::new()),
        // AVI video (RIFF magic)
        Box::new(crate::formats::avi::AviReader::new()),
        // Leica LEI confocal (magic ILIS / 0x49494949)
        Box::new(crate::formats::lei::LeiReader::new()),
        // PerkinElmer FLEX HCS (TIFF-based)
        Box::new(crate::formats::flex::FlexReader::new()),
        // Bruker OPUS FTIR (magic 0x0A 0x00-0x02)
        Box::new(crate::formats::opus::BrukerOpusReader::new()),
        // Extension-only readers
        Box::new(crate::formats::volocity::VolocityReader::new()),
        Box::new(crate::formats::volocity::NikonNisReader::new()),
        Box::new(crate::formats::opus::IssFlimReader::new()),
        Box::new(crate::formats::legacy::KodakBipReader::new()),
        Box::new(crate::formats::legacy::WoolzReader::new()),
        Box::new(crate::formats::legacy::PictReader::new()),
        Box::new(crate::formats::xrm::XrmReader::new()),
        Box::new(crate::formats::zvi::ZviReader::new()),
        // TIFF-based whole-slide / variant formats (extension-only)
        Box::new(crate::formats::tiff_wrappers::NdpiReader::new()),
        Box::new(crate::formats::tiff_wrappers::LeicaScnReader::new()),
        Box::new(crate::formats::tiff_wrappers::VentanaReader::new()),
        Box::new(crate::formats::tiff_wrappers::NikonElementsTiffReader::new()),
        Box::new(crate::formats::tiff_wrappers::FeiTiffReader::new()),
        Box::new(crate::formats::tiff_wrappers::OlympusSisTiffReader::new()),
        Box::new(crate::formats::tiff_wrappers::ImprovisionTiffReader::new()),
        Box::new(crate::formats::tiff_wrappers::ZeissApotomeTiffReader::new()),
        Box::new(crate::formats::tiff_wrappers::FluoviewTiffReader::new()),
        Box::new(crate::formats::tiff_wrappers::MolecularDevicesTiffReader::new()),
        // Misc extension-only / placeholder formats
        Box::new(crate::formats::misc::Jpeg2000Reader::new()), // magic-byte detection
        Box::new(crate::formats::misc::QuickTimeReader::new()),
        Box::new(crate::formats::misc::MngReader::new()),
        Box::new(crate::formats::misc::VolocityLibraryReader::new()),
        Box::new(crate::formats::misc::SlideBookReader::new()),
        Box::new(crate::formats::misc::MincReader::new()),
        Box::new(crate::formats::misc::OpenlabLiffReader::new()),
        Box::new(crate::formats::misc::SedatReader::new()),
        Box::new(crate::formats::misc::SmCameraReader::new()),
        // Extended formats — TIFF wrappers
        Box::new(crate::formats::extended::DngReader::new()),
        Box::new(crate::formats::extended::QptiffReader::new()),
        Box::new(crate::formats::extended::GelReader::new()),
        // Extended formats — binary with magic/structure
        Box::new(crate::formats::extended::ImspectorReader::new()), // magic "OMAS_BF_"
        Box::new(crate::formats::extended::HamamatsuVmsReader::new()),
        Box::new(crate::formats::extended::CellomicsReader::new()),
        // Extended formats — extension-only placeholders
        Box::new(crate::formats::extended::MrwReader::new()),
        Box::new(crate::formats::extended::YokogawaReader::new()),
        Box::new(crate::formats::extended::LeicaLofReader::new()),
        Box::new(crate::formats::extended::ApngReader::new()),
        Box::new(crate::formats::extended::PovRayReader::new()),
        Box::new(crate::formats::extended::NafReader::new()),
        Box::new(crate::formats::extended::BurleighReader::new()),
        // HCS2 — TIFF-based HCS wrappers
        Box::new(crate::formats::hcs2::MetaxpressTiffReader::new()),
        Box::new(crate::formats::hcs2::SimplePciTiffReader::new()),
        Box::new(crate::formats::hcs2::IonpathMibiTiffReader::new()),
        Box::new(crate::formats::hcs2::MiasTiffReader::new()),
        Box::new(crate::formats::hcs2::TrestleReader::new()),
        Box::new(crate::formats::hcs2::TissueFaxsReader::new()),
        Box::new(crate::formats::hcs2::MikroscanTiffReader::new()),
        // HCS2 — extension-only plate readers
        Box::new(crate::formats::hcs2::BdReader::new()),
        Box::new(crate::formats::hcs2::ColumbusReader::new()),
        Box::new(crate::formats::hcs2::OperettaReader::new()),
        Box::new(crate::formats::hcs2::ScanrReader::new()),
        Box::new(crate::formats::hcs2::CellVoyagerReader::new()),
        Box::new(crate::formats::hcs2::TecanReader::new()),
        Box::new(crate::formats::hcs2::InCell3000Reader::new()),
        Box::new(crate::formats::hcs2::RcpnlReader::new()),
        // SEM — electron microscopy
        Box::new(crate::formats::sem::InrReader::new()),
        Box::new(crate::formats::sem::VeecoReader::new()),
        Box::new(crate::formats::sem::ZeissTiffReader::new()),
        Box::new(crate::formats::sem::JeolReader::new()),
        Box::new(crate::formats::sem::HitachiReader::new()),
        Box::new(crate::formats::sem::LeoReader::new()),
        Box::new(crate::formats::sem::ZeissLmsReader::new()),
        Box::new(crate::formats::sem::ImrodReader::new()),
        // SPM — scanning probe / AFM
        Box::new(crate::formats::spm::PicoQuantReader::new()),
        Box::new(crate::formats::spm::RhkReader::new()),
        Box::new(crate::formats::spm::QuesantReader::new()),
        Box::new(crate::formats::spm::JpkReader::new()),
        Box::new(crate::formats::spm::WatopReader::new()),
        Box::new(crate::formats::spm::VgSamReader::new()),
        Box::new(crate::formats::spm::UbmReader::new()),
        Box::new(crate::formats::spm::SeikoReader::new()),
        // Camera2 — camera/RAW formats
        Box::new(crate::formats::camera2::PcoRawReader::new()),
        Box::new(crate::formats::camera2::BioRadGelReader::new()),
        Box::new(crate::formats::camera2::L2dReader::new()),
        Box::new(crate::formats::camera2::PhotoshopTiffReader::new()),
        Box::new(crate::formats::camera2::CanonRawReader::new()),
        Box::new(crate::formats::camera2::ImaconReader::new()),
        Box::new(crate::formats::camera2::SbigReader::new()),
        Box::new(crate::formats::camera2::IpwReader::new()),
        // FLIM2 — additional FLIM/flow cytometry
        Box::new(crate::formats::flim2::FlowSightReader::new()),
        Box::new(crate::formats::flim2::Im3Reader::new()),
        Box::new(crate::formats::flim2::SlideBook7Reader::new()),
        Box::new(crate::formats::flim2::NdpisReader::new()),
        Box::new(crate::formats::flim2::IvisionReader::new()),
        Box::new(crate::formats::flim2::AfiFluorescenceReader::new()),
        Box::new(crate::formats::flim2::ImarisTiffReader::new()),
        Box::new(crate::formats::flim2::XlefReader::new()),
        Box::new(crate::formats::flim2::OirReader::new()),
        Box::new(crate::formats::flim2::CellSensReader::new()),
        Box::new(crate::formats::flim2::VolocityClippingReader::new()),
        Box::new(crate::formats::flim2::MicroCtReader::new()),
        Box::new(crate::formats::flim2::BioRadScnReader::new()),
        Box::new(crate::formats::flim2::SlidebookTiffReader::new()),
        // Misc4 — remaining obscure formats
        Box::new(crate::formats::misc4::AplReader::new()),
        Box::new(crate::formats::misc4::ArfReader::new()),
        Box::new(crate::formats::misc4::I2iReader::new()),
        Box::new(crate::formats::misc4::JdceReader::new()),
        Box::new(crate::formats::misc4::JpxReader::new()),
        Box::new(crate::formats::misc4::PciReader::new()),
        Box::new(crate::formats::misc4::PdsReader::new()),
        Box::new(crate::formats::misc4::HisReader::new()),
        Box::new(crate::formats::misc4::HrdgdfReader::new()),
        Box::new(crate::formats::misc4::TextImageReader::new()),
        Box::new(crate::formats::misc4::FilePatternReaderStub::new()),
        Box::new(crate::formats::misc4::KlbReader::new()),
        Box::new(crate::formats::misc4::ObfReader::new()),
        Box::new(crate::formats::misc::TextReader::new()),
    ]
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
    let header = peek_header(path, 512).unwrap_or_default();
    let mut best_error = None;

    // TIFF-based vendor wrappers often have no magic beyond TIFF itself.
    // Give non-generic TIFF extensions a chance before the broad TiffReader
    // byte signature accepts the file.
    if is_tiff_header(&header) {
        for mut r in tiff_wrapper_readers_for_extension(path) {
            match r.set_id(path) {
                Ok(()) => return Ok(r),
                Err(err) => remember_set_id_error(&mut best_error, err),
            }
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

fn is_tiff_header(header: &[u8]) -> bool {
    header.len() >= 4
        && (header[0..2] == [0x49, 0x49] || header[0..2] == [0x4D, 0x4D])
        && (header[2..4] == [42, 0]
            || header[2..4] == [0, 42]
            || header[2..4] == [43, 0]
            || header[2..4] == [0, 43])
}

fn tiff_wrapper_readers_for_extension(path: &Path) -> Vec<Box<dyn FormatReader>> {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase());

    match ext.as_deref() {
        Some("lsm") => vec![boxed_reader(crate::formats::lsm::LsmReader::new())],
        Some("stk") => vec![boxed_reader(
            crate::formats::metamorph::MetamorphReader::new(),
        )],
        Some("svs") => vec![boxed_reader(
            crate::formats::svs::WholeSlideTiffReader::new(),
        )],
        Some("ndpi") => vec![
            boxed_reader(crate::formats::tiff_wrappers::NdpiReader::new()),
            boxed_reader(crate::formats::svs::WholeSlideTiffReader::new()),
        ],
        Some("scn") => vec![
            boxed_reader(crate::formats::tiff_wrappers::LeicaScnReader::new()),
            boxed_reader(crate::formats::svs::WholeSlideTiffReader::new()),
            boxed_reader(crate::formats::flim2::BioRadScnReader::new()),
        ],
        Some("bif") => vec![
            boxed_reader(crate::formats::tiff_wrappers::VentanaReader::new()),
            boxed_reader(crate::formats::svs::WholeSlideTiffReader::new()),
        ],
        Some("vsi") => vec![
            boxed_reader(crate::formats::flim2::CellSensReader::new()),
            boxed_reader(crate::formats::svs::WholeSlideTiffReader::new()),
        ],
        Some("afi") => vec![
            boxed_reader(crate::formats::flim2::AfiFluorescenceReader::new()),
            boxed_reader(crate::formats::svs::WholeSlideTiffReader::new()),
        ],
        Some("dng") => vec![boxed_reader(crate::formats::extended::DngReader::new())],
        Some("qptiff") => vec![boxed_reader(crate::formats::extended::QptiffReader::new())],
        Some("gel") => vec![boxed_reader(crate::formats::extended::GelReader::new())],
        Some("flex") => vec![boxed_reader(crate::formats::flex::FlexReader::new())],
        Some("cr2") | Some("crw") | Some("cr3") => {
            vec![boxed_reader(crate::formats::camera2::CanonRawReader::new())]
        }
        Some("ipw") => vec![boxed_reader(crate::formats::camera2::IpwReader::new())],
        Some("ims") => vec![boxed_reader(crate::formats::flim2::ImarisTiffReader::new())],
        Some("xlef") => vec![boxed_reader(crate::formats::flim2::XlefReader::new())],
        Some("ctf") => vec![boxed_reader(crate::formats::flim2::MicroCtReader::new())],
        Some("ndpis") => vec![boxed_reader(crate::formats::flim2::NdpisReader::new())],
        // MIAS datasets are plain TIFFs, so the generic TiffReader would win the
        // magic pass. Give MiasReader a chance first, but ONLY when the file is
        // genuinely a MIAS plane (a Well<xxxx> directory + mode/z/t naming), so
        // ordinary .tif files still fall through to the generic TiffReader.
        Some("tif") | Some("tiff") => {
            let mias = crate::formats::mias::MiasReader::new();
            if mias.is_this_type_by_name(path) {
                vec![boxed_reader(mias)]
            } else {
                Vec::new()
            }
        }
        _ => Vec::new(),
    }
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
            if message.contains("not implemented")
                || message.contains("requires OLE2")
                || message.contains("requires HDF5")
    )
}

#[cfg(test)]
mod tests {
    use super::ImageReader;
    use crate::common::error::BioFormatsError;
    use crate::common::metadata::MetadataValue;
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

    #[test]
    fn open_returns_set_id_error_for_matching_corrupt_file() {
        let path = temp_path("corrupt.png");
        std::fs::write(&path, b"\x89PNG\r\n\x1a\nnot enough png data").unwrap();

        let err = match ImageReader::open(&path) {
            Ok(_) => panic!("corrupt PNG unexpectedly opened"),
            Err(err) => err,
        };

        assert!(
            matches!(err, BioFormatsError::Format(_)),
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
    fn registered_unsupported_stubs_do_not_open_with_fake_metadata() {
        for (name, expected) in [
            (
                "sample.mvd2",
                "Volocity MVD2 format reading is not yet implemented",
            ),
            ("sample.cif", "FlowSight CIF is not TIFF-like"),
            ("sample.lof", "Leica LOF is a proprietary binary format"),
            (
                "sample.oir",
                "Olympus OIR format requires OLE2 container parsing",
            ),
            (
                "sample.acff",
                "Volocity Library format requires OLE2/Compound Document container parsing",
            ),
            // NOTE: XRM/TXRM are no longer stubs — XrmReader is a real CFB-based
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
            ("sample.b16", "PCO B16 file is too short"),
            ("sample.1sc", "Bio-Rad GEL file is too short"),
            (
                "sample.obf",
                "Imspector OBF/MSR stack metadata and payload decoding is not implemented",
            ),
            (
                "sample.msr",
                "Imspector OBF/MSR stack metadata and payload decoding is not implemented",
            ),
            (
                "sample.vms",
                "Hamamatsu VMS/VMU JPEG tile payload decoding is not implemented",
            ),
            (
                "sample.vmu",
                "Hamamatsu VMS/VMU JPEG tile payload decoding is not implemented",
            ),
            (
                "sample.l2d",
                "Li-Cor L2D file is missing LI-COR LI2D marker",
            ),
            (
                "sample.ptu",
                "PicoQuant TCSPC event stream decoding to image planes is not implemented",
            ),
            (
                "sample.vws",
                "TillVision embedded VWS payload decoding is not implemented",
            ),
            (
                "sample.abs",
                "Bruker OPUS spectral image decoding is not implemented",
            ),
            (
                "sample.0",
                "Bruker OPUS spectral image decoding is not implemented",
            ),
            ("sample.iss", "ISS Vista FLIM decoding is not implemented"),
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
            } else if name == "sample.abs" || name == "sample.0" {
                std::fs::write(&path, b"\x0a\x01\0\0\x04\0\0\0not a real image").unwrap();
            } else {
                std::fs::write(&path, b"not a real image").unwrap();
            }

            let err = match ImageReader::open(&path) {
                Ok(_) => panic!("{name}: placeholder reader opened fake data"),
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
    fn visitech_registry_rejects_xys_without_companion_tiffs() {
        let dir = temp_path("hcs_no_tiffs_dir");
        std::fs::create_dir_all(&dir).unwrap();

        for (name, bytes, expected) in [
            (
                "sample.xys",
                b"Width=2\nHeight=2\n".as_slice(),
                "Visitech XYS does not have",
            ),
            (
                "sample.xdce",
                b"<InCell Width=\"2\" Height=\"2\"/>".as_slice(),
                "no TIFF image files found referenced in index",
            ),
        ] {
            let path = dir.join(name);
            std::fs::write(&path, bytes).unwrap();

            let err = match ImageReader::open(&path) {
                Ok(_) => panic!("{name}: index without companion TIFFs opened fake data"),
                Err(err) => err,
            };

            assert!(
                matches!(
                    err,
                    BioFormatsError::UnsupportedFormat(ref message) | BioFormatsError::Format(ref message)
                        if message.contains(expected)
                ),
                "expected rejection message containing {expected:?}, got {err:?}"
            );
        }

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
            hdf5_pure::FileBuilder::new().write(&path).unwrap();

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
        let path = temp_path("magic_imspector.fake");
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
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Imspector OBF/MSR stack metadata and payload decoding is not implemented")),
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
            tiff_entry(273, 4, 1, 8 + 2 + 11 * 12 + 4),        // StripOffsets
            tiff_entry(277, 3, 1, 1),                          // SamplesPerPixel
            tiff_entry(278, 4, 1, 1),                          // RowsPerStrip
            tiff_entry(279, 4, 1, 1),                          // StripByteCounts
            tiff_entry(284, 3, 1, 1),                          // PlanarConfiguration
            tiff_entry(65421, 11, 1, magnification.to_bits()), // NDPI magnification
        ];
        bytes.extend_from_slice(&(entries.len() as u16).to_le_bytes());
        for entry in entries {
            bytes.extend_from_slice(&entry);
        }
        bytes.extend_from_slice(&0u32.to_le_bytes());
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
