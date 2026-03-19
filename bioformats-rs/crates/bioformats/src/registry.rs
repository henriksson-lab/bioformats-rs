use std::path::Path;

use bioformats_common::error::{BioFormatsError, Result};
use bioformats_common::metadata::ImageMetadata;
use bioformats_common::reader::FormatReader;
use bioformats_common::io::peek_header;

/// The top-level reader that auto-detects the file format and delegates to the
/// appropriate format-specific reader.
pub struct ImageReader {
    inner: Box<dyn FormatReader>,
}

fn all_readers() -> Vec<Box<dyn FormatReader>> {
    vec![
        // Dedicated readers first (most precise magic bytes)
        Box::new(bioformats_zip::ZipReader::new()),
        Box::new(bioformats_imaris::ImarisReader::new()),
        // HDF5-based formats (extension-only, must come after ImarisReader magic check)
        Box::new(bioformats_cellh5::CellH5Reader::new()),  // .ch5
        Box::new(bioformats_bdv::BdvReader::new()),        // .h5
        Box::new(bioformats_viff::ViffReader::new()),
        Box::new(bioformats_mias::Al3dReader::new()),
        Box::new(bioformats_perkinelmer::OpenlabRawReader::new()),
        Box::new(bioformats_incell::InCellReader::new()),
        Box::new(bioformats_tiff::TiffReader::new()),
        Box::new(bioformats_png::PngReader::new()),
        Box::new(bioformats_jpeg::JpegReader::new()),
        Box::new(bioformats_bmp::BmpReader::new()),
        Box::new(bioformats_czi::CziReader::new()),
        Box::new(bioformats_nd2::Nd2Reader::new()),
        Box::new(bioformats_lif::LifReader::new()),
        Box::new(bioformats_mrc::MrcReader::new()),
        Box::new(bioformats_fits::FitsReader::new()),
        Box::new(bioformats_nrrd::NrrdReader::new()),
        Box::new(bioformats_metaimage::MetaImageReader::new()),
        Box::new(bioformats_ics::IcsReader::new()),
        Box::new(bioformats_dicom::DicomReader::new()),
        Box::new(bioformats_nifti::NiftiReader::new()),
        Box::new(bioformats_gatan::GatanReader::new()),
        // Generic raster wrappers (via image crate)
        Box::new(bioformats_raster::gif_reader()),
        Box::new(bioformats_raster::webp_reader()),
        Box::new(bioformats_raster::pnm_reader()),
        Box::new(bioformats_raster::hdr_reader()),
        Box::new(bioformats_raster::exr_reader()),
        Box::new(bioformats_raster::dds_reader()),
        Box::new(bioformats_raster::farbfeld_reader()),
        // Additional scientific formats
        Box::new(bioformats_biorad::BioRadReader::new()),
        Box::new(bioformats_deltavision::DeltavisionReader::new()),
        Box::new(bioformats_spe::SpeReader::new()),
        Box::new(bioformats_andor::AndorSifReader::new()),
        Box::new(bioformats_amira::AmiraReader::new()),
        Box::new(bioformats_amira::SpiderReader::new()),
        Box::new(bioformats_imagic::ImagicReader::new()),
        Box::new(bioformats_flim::SdtReader::new()),
        Box::new(bioformats_clinical::Ecat7Reader::new()),
        Box::new(bioformats_clinical::FdfReader::new()),
        Box::new(bioformats_hamamatsu::DcimgReader::new()),
        Box::new(bioformats_norpix::NorpixReader::new()),
        Box::new(bioformats_norpix::IplabReader::new()),
        Box::new(bioformats_ome::OmeXmlReader::new()),
        Box::new(bioformats_olympus::OifReader::new()),
        // Magic-byte detected formats
        Box::new(bioformats_pcx::PcxReader::new()),
        Box::new(bioformats_photoshop::PsdReader::new()),
        Box::new(bioformats_aim::AimReader::new()),
        // Prairie/Leica XML+TIFF series (magic-byte detection via XML content)
        Box::new(bioformats_prairie::PrairieReader::new()),
        Box::new(bioformats_prairie::LeicaTcsReader::new()),
        // EPS/PostScript
        Box::new(bioformats_eps::EpsReader::new()),
        // Extension-only TIFF-based formats (no distinct magic bytes)
        Box::new(bioformats_lsm::LsmReader::new()),
        Box::new(bioformats_metamorph::MetamorphReader::new()),
        Box::new(bioformats_micromanager::MicromanagerReader::new()),
        // Whole-slide TIFF wrappers (extension-only)
        Box::new(bioformats_svs::WholeSlideTiffReader::new()),
        // Extension-only Inveon (hdr+img pair, extension-only detection)
        Box::new(bioformats_clinical::InveonReader::new()),
        // SimFCS FLIM (extension-only)
        Box::new(bioformats_simfcs::SimfcsReader::new()),
        Box::new(bioformats_simfcs::LambertFlimReader::new()),
        // AFM formats (extension-only)
        Box::new(bioformats_afm::TopoMetrixReader::new()),
        Box::new(bioformats_afm::UnisokuReader::new()),
        // LIM / TillVision (extension-only)
        Box::new(bioformats_lim::LimReader::new()),
        Box::new(bioformats_lim::TillVisionReader::new()),
        // AIM/ISQ extension-only fallback
        // DM2 (extension-only, Gatan)
        Box::new(bioformats_gatan::Dm2Reader::new()),
        // Extension-only (no magic bytes)
        Box::new(bioformats_raster::tga_reader()),
        // New format readers (extension-only)
        Box::new(bioformats_fake::FakeReader::new()),
        Box::new(bioformats_visitech::VisitechReader::new()),
        Box::new(bioformats_perkinelmer::PerkinElmerReader::new()),
        Box::new(bioformats_perkinelmer::PhotonDynamicsReader::new()),
        Box::new(bioformats_mias::CellWorxReader::new()),
        Box::new(bioformats_mias::OxfordInstrumentsReader::new()),
        // FEI SER (magic-byte detected: 0x97 0x01)
        Box::new(bioformats_mias::FeiSerReader::new()),
        // AVI video (RIFF magic)
        Box::new(bioformats_avi::AviReader::new()),
        // Leica LEI confocal (magic ILIS / 0x49494949)
        Box::new(bioformats_lei::LeiReader::new()),
        // PerkinElmer FLEX HCS (TIFF-based)
        Box::new(bioformats_flex::FlexReader::new()),
        // Bruker OPUS FTIR (magic 0x0A 0x00-0x02)
        Box::new(bioformats_opus::BrukerOpusReader::new()),
        // Extension-only readers
        Box::new(bioformats_volocity::VolocityReader::new()),
        Box::new(bioformats_volocity::NikonNisReader::new()),
        Box::new(bioformats_opus::IssFlimReader::new()),
        Box::new(bioformats_legacy::KodakBipReader::new()),
        Box::new(bioformats_legacy::WoolzReader::new()),
        Box::new(bioformats_legacy::PictReader::new()),
        Box::new(bioformats_xrm::XrmReader::new()),
        Box::new(bioformats_zvi::ZviReader::new()),
        // TIFF-based whole-slide / variant formats (extension-only)
        Box::new(bioformats_tiff_wrappers::NdpiReader::new()),
        Box::new(bioformats_tiff_wrappers::LeicaScnReader::new()),
        Box::new(bioformats_tiff_wrappers::VentanaReader::new()),
        Box::new(bioformats_tiff_wrappers::NikonElementsTiffReader::new()),
        Box::new(bioformats_tiff_wrappers::FeiTiffReader::new()),
        Box::new(bioformats_tiff_wrappers::OlympusSisTiffReader::new()),
        Box::new(bioformats_tiff_wrappers::ImprovisionTiffReader::new()),
        Box::new(bioformats_tiff_wrappers::ZeissApotomeTiffReader::new()),
        Box::new(bioformats_tiff_wrappers::FluoviewTiffReader::new()),
        Box::new(bioformats_tiff_wrappers::MolecularDevicesTiffReader::new()),
        // Misc extension-only / placeholder formats
        Box::new(bioformats_misc::Jpeg2000Reader::new()),  // magic-byte detection
        Box::new(bioformats_misc::QuickTimeReader::new()),
        Box::new(bioformats_misc::MngReader::new()),
        Box::new(bioformats_misc::VolocityLibraryReader::new()),
        Box::new(bioformats_misc::SlideBookReader::new()),
        Box::new(bioformats_misc::MincReader::new()),
        Box::new(bioformats_misc::OpenlabLiffReader::new()),
        Box::new(bioformats_misc::SedatReader::new()),
        Box::new(bioformats_misc::SmCameraReader::new()),
        // Extended formats — TIFF wrappers
        Box::new(bioformats_extended::DngReader::new()),
        Box::new(bioformats_extended::QptiffReader::new()),
        Box::new(bioformats_extended::GelReader::new()),
        // Extended formats — binary with magic/structure
        Box::new(bioformats_extended::ImspectorReader::new()),  // magic "OMAS_BF_"
        Box::new(bioformats_extended::HamamatsuVmsReader::new()),
        Box::new(bioformats_extended::CellomicsReader::new()),
        // Extended formats — extension-only placeholders
        Box::new(bioformats_extended::MrwReader::new()),
        Box::new(bioformats_extended::YokogawaReader::new()),
        Box::new(bioformats_extended::LeicaLofReader::new()),
        Box::new(bioformats_extended::ApngReader::new()),
        Box::new(bioformats_extended::PovRayReader::new()),
        Box::new(bioformats_extended::NafReader::new()),
        Box::new(bioformats_extended::BurleighReader::new()),
        // HCS2 — TIFF-based HCS wrappers
        Box::new(bioformats_hcs2::MetaxpressTiffReader::new()),
        Box::new(bioformats_hcs2::SimplePciTiffReader::new()),
        Box::new(bioformats_hcs2::IonpathMibiTiffReader::new()),
        Box::new(bioformats_hcs2::MiasTiffReader::new()),
        Box::new(bioformats_hcs2::TrestleReader::new()),
        Box::new(bioformats_hcs2::TissueFaxsReader::new()),
        Box::new(bioformats_hcs2::MikroscanTiffReader::new()),
        // HCS2 — extension-only plate readers
        Box::new(bioformats_hcs2::BdReader::new()),
        Box::new(bioformats_hcs2::ColumbusReader::new()),
        Box::new(bioformats_hcs2::OperettaReader::new()),
        Box::new(bioformats_hcs2::ScanrReader::new()),
        Box::new(bioformats_hcs2::CellVoyagerReader::new()),
        Box::new(bioformats_hcs2::TecanReader::new()),
        Box::new(bioformats_hcs2::InCell3000Reader::new()),
        Box::new(bioformats_hcs2::RcpnlReader::new()),
        // SEM — electron microscopy
        Box::new(bioformats_sem::InrReader::new()),
        Box::new(bioformats_sem::VeecoReader::new()),
        Box::new(bioformats_sem::ZeissTiffReader::new()),
        Box::new(bioformats_sem::JeolReader::new()),
        Box::new(bioformats_sem::HitachiReader::new()),
        Box::new(bioformats_sem::LeoReader::new()),
        Box::new(bioformats_sem::ZeissLmsReader::new()),
        Box::new(bioformats_sem::ImrodReader::new()),
        // SPM — scanning probe / AFM
        Box::new(bioformats_spm::PicoQuantReader::new()),
        Box::new(bioformats_spm::RhkReader::new()),
        Box::new(bioformats_spm::QuesantReader::new()),
        Box::new(bioformats_spm::JpkReader::new()),
        Box::new(bioformats_spm::WatopReader::new()),
        Box::new(bioformats_spm::VgSamReader::new()),
        Box::new(bioformats_spm::UbmReader::new()),
        Box::new(bioformats_spm::SeikoReader::new()),
        // Camera2 — camera/RAW formats
        Box::new(bioformats_camera2::PcoRawReader::new()),
        Box::new(bioformats_camera2::BioRadGelReader::new()),
        Box::new(bioformats_camera2::L2dReader::new()),
        Box::new(bioformats_camera2::PhotoshopTiffReader::new()),
        Box::new(bioformats_camera2::CanonRawReader::new()),
        Box::new(bioformats_camera2::ImaconReader::new()),
        Box::new(bioformats_camera2::SbigReader::new()),
        Box::new(bioformats_camera2::IpwReader::new()),
        // FLIM2 — additional FLIM/flow cytometry
        Box::new(bioformats_flim2::FlowSightReader::new()),
        Box::new(bioformats_flim2::Im3Reader::new()),
        Box::new(bioformats_flim2::SlideBook7Reader::new()),
        Box::new(bioformats_flim2::NdpisReader::new()),
        Box::new(bioformats_flim2::IvisionReader::new()),
        Box::new(bioformats_flim2::AfiFluorescenceReader::new()),
        Box::new(bioformats_flim2::ImarisTiffReader::new()),
        Box::new(bioformats_flim2::XlefReader::new()),
        Box::new(bioformats_flim2::OirReader::new()),
        Box::new(bioformats_flim2::CellSensReader::new()),
        Box::new(bioformats_flim2::VolocityClippingReader::new()),
        Box::new(bioformats_flim2::MicroCtReader::new()),
        Box::new(bioformats_flim2::BioRadScnReader::new()),
        Box::new(bioformats_flim2::SlidebookTiffReader::new()),
        // Misc4 — remaining obscure formats
        Box::new(bioformats_misc4::AplReader::new()),
        Box::new(bioformats_misc4::ArfReader::new()),
        Box::new(bioformats_misc4::I2iReader::new()),
        Box::new(bioformats_misc4::JdceReader::new()),
        Box::new(bioformats_misc4::JpxReader::new()),
        Box::new(bioformats_misc4::PciReader::new()),
        Box::new(bioformats_misc4::PdsReader::new()),
        Box::new(bioformats_misc4::HisReader::new()),
        Box::new(bioformats_misc4::HrdgdfReader::new()),
        Box::new(bioformats_misc4::TextImageReader::new()),
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
