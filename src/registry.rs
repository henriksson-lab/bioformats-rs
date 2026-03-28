use std::path::Path;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::ome_metadata::OmeMetadata;
use crate::common::reader::FormatReader;
use crate::common::io::peek_header;

/// The top-level reader that auto-detects the file format and delegates to the
/// appropriate format-specific reader.
pub struct ImageReader {
    inner: Box<dyn FormatReader>,
}

/// Returns all registered format readers. Used internally by `ImageReader` and `Memoizer`.
pub fn all_readers_pub() -> Vec<Box<dyn FormatReader>> { all_readers() }

fn all_readers() -> Vec<Box<dyn FormatReader>> {
    vec![
        // Dedicated readers first (most precise magic bytes)
        Box::new(crate::formats::zip::ZipReader::new()),
        Box::new(crate::formats::imaris::ImarisReader::new()),
        // HDF5-based formats (extension-only, must come after ImarisReader magic check)
        Box::new(crate::formats::cellh5::CellH5Reader::new()),  // .ch5
        Box::new(crate::formats::bdv::BdvReader::new()),        // .h5
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
        Box::new(crate::formats::misc::Jpeg2000Reader::new()),  // magic-byte detection
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
        Box::new(crate::formats::extended::ImspectorReader::new()),  // magic "OMAS_BF_"
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
    ]
}

impl ImageReader {
    /// Open the file at `path`, detect its format, parse metadata.
    pub fn open(path: &Path) -> Result<Self> {
        let header = peek_header(path, 512).unwrap_or_default();

        // 1. Magic bytes
        for mut r in all_readers() {
            if r.is_this_type_by_bytes(&header) {
                r.set_id(path)?;
                return Ok(ImageReader { inner: r });
            }
        }

        // 2. Extension fallback
        for mut r in all_readers() {
            if r.is_this_type_by_name(path) {
                r.set_id(path)?;
                return Ok(ImageReader { inner: r });
            }
        }

        Err(BioFormatsError::UnsupportedFormat(path.display().to_string()))
    }

    /// Return structured OME metadata if supported by the detected format.
    ///
    /// Equivalent to Java Bio-Formats `reader.setMetadataStore(service.createOMEXMLMetadata())`.
    /// Returns `Some` for CZI, OME-TIFF, and OME-XML files; `None` for all others.
    pub fn ome_metadata(&self) -> Option<OmeMetadata> { self.inner.ome_metadata() }
    pub fn series_count(&self) -> usize { self.inner.series_count() }
    pub fn set_series(&mut self, series: usize) -> Result<()> { self.inner.set_series(series) }
    pub fn series(&self) -> usize { self.inner.series() }
    pub fn metadata(&self) -> &ImageMetadata { self.inner.metadata() }
    pub fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> { self.inner.open_bytes(plane_index) }
    pub fn open_bytes_region(&mut self, plane_index: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> {
        self.inner.open_bytes_region(plane_index, x, y, w, h)
    }
    pub fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> { self.inner.open_thumb_bytes(plane_index) }
    pub fn resolution_count(&self) -> usize { self.inner.resolution_count() }
    pub fn set_resolution(&mut self, level: usize) -> Result<()> { self.inner.set_resolution(level) }
    pub fn close(&mut self) -> Result<()> { self.inner.close() }
}
