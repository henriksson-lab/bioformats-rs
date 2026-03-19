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
