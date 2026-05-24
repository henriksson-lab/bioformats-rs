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
        let header = peek_header(path, 512).unwrap_or_default();
        let mut best_error = None;

        // 1. Magic bytes
        for mut r in all_readers() {
            if r.is_this_type_by_bytes(&header) {
                match r.set_id(path) {
                    Ok(()) => return Ok(ImageReader { inner: r }),
                    Err(err) => remember_set_id_error(&mut best_error, err),
                }
            }
        }

        // 2. Extension fallback
        let mut replacing_magic_error = best_error.is_some();
        for mut r in all_readers() {
            if r.is_this_type_by_name(path) {
                match r.set_id(path) {
                    Ok(()) => return Ok(ImageReader { inner: r }),
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

#[cfg(test)]
mod tests {
    use super::ImageReader;
    use crate::common::error::BioFormatsError;
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
    fn registered_unsupported_stubs_do_not_open_with_fake_metadata() {
        for (name, expected) in [
            (
                "sample.mvd2",
                "Volocity MVD2 format reading is not yet implemented",
            ),
            (
                "sample.cif",
                "FlowSight CIF format reading is not yet implemented",
            ),
            ("sample.lof", "Leica LOF is a proprietary binary format"),
            (
                "sample.oir",
                "Olympus OIR format requires OLE2 container parsing",
            ),
            (
                "sample.acff",
                "Volocity Library format requires OLE2/Compound Document container parsing",
            ),
            (
                "sample.xrm",
                "Zeiss XRM format reading is not yet implemented",
            ),
            (
                "sample.eps",
                "EPS/PostScript rasterization requires a PostScript interpreter",
            ),
            ("sample.b16", "PCO B16 file is too short"),
            ("sample.1sc", "Bio-Rad GEL file is too short"),
            (
                "sample.l2d",
                "Hamamatsu L2D image payload decoding is not implemented",
            ),
            (
                "sample.ptu",
                "PicoQuant TCSPC event stream decoding to image planes is not implemented",
            ),
        ] {
            let path = temp_path(name);
            std::fs::write(&path, b"not a real image").unwrap();

            let err = match ImageReader::open(&path) {
                Ok(_) => panic!("placeholder reader opened fake data"),
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
            let _file = hdf5::File::create(&path).unwrap();
            drop(_file);

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
    fn zip_reader_rejects_archives_without_delegated_tiff() {
        let no_image_path = temp_path("no_image.zip");
        write_zip_entry(&no_image_path, "README.txt", b"not image data");

        let err = match ImageReader::open(&no_image_path) {
            Ok(_) => panic!("ZIP without TIFF opened"),
            Err(err) => err,
        };
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("does not contain a supported TIFF image entry")),
            "expected unsupported ZIP message, got {err:?}"
        );
        let _ = std::fs::remove_file(&no_image_path);

        let png_path = temp_path("png_only.zip");
        write_zip_entry(&png_path, "image.png", b"not a png");

        let err = match ImageReader::open(&png_path) {
            Ok(_) => panic!("ZIP PNG placeholder opened"),
            Err(err) => err,
        };
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("only TIFF entries are currently delegated")),
            "expected unsupported ZIP image message, got {err:?}"
        );
        let _ = std::fs::remove_file(png_path);
    }

    fn write_zip_entry(path: &PathBuf, name: &str, bytes: &[u8]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
        zip.finish().unwrap();
    }
}
