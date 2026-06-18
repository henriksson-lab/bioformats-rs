use std::path::Path;

use crate::common::error::{BioFormatsError, Result};
use crate::common::io::peek_header;
use crate::common::metadata::ImageMetadata;
use crate::common::ome_metadata::OmeMetadata;
use crate::common::path::confined_join;
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
        Box::new(crate::formats::imaris_hdf::ImarisHdfReader::new()),
        // Classic native Bitplane Imaris .ims (magic int 5021964) — distinct from
        // the HDF5 and TIFF Imaris variants; disambiguated by its own magic.
        Box::new(crate::formats::flim2::ImarisReader::new()),
        // HDF5-based formats (extension-only, must come after ImarisHdfReader magic check)
        Box::new(crate::formats::cellh5::CellH5Reader::new()), // .ch5
        Box::new(crate::formats::bdv::BdvReader::new()),       // .h5
        Box::new(crate::formats::khoros::KhorosReader::new()),
        Box::new(crate::formats::mias::AliconaReader::new()),
        Box::new(crate::formats::perkinelmer::OpenlabRawReader::new()),
        Box::new(crate::formats::incell::InCellReader::new()),
        Box::new(crate::tiff::TiffReader::new()),
        // ApngReader must precede PngReader: both match the PNG byte signature,
        // but ApngReader only claims animated PNGs (those with an acTL chunk),
        // so still PNGs fall through to PngReader.
        Box::new(crate::formats::extended::ApngReader::new()),
        Box::new(crate::formats::png::PngReader::new()),
        Box::new(crate::formats::jpeg::JpegReader::new()),
        Box::new(crate::formats::bmp::BmpReader::new()),
        Box::new(crate::formats::zeiss_czi::ZeissCziReader::new()),
        Box::new(crate::formats::nd2::Nd2Reader::new()),
        Box::new(crate::formats::lif::LifReader::new()),
        // DeltaVision must precede MRC: both readers' byte signatures accept a
        // file with plausible NX/NY/NZ in the first 12 bytes, and a .dv file
        // qualifies. Java's readers.txt lists DeltavisionReader before MRCReader
        // for exactly this reason, so the PRIISM magic (offset 96 == -16224)
        // wins over MRC's looser heuristic.
        Box::new(crate::formats::deltavision::DeltavisionReader::new()),
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
        Box::new(crate::formats::spe::SpeReader::new()),
        Box::new(crate::formats::sif::SifReader::new()),
        Box::new(crate::formats::amira::AmiraReader::new()),
        Box::new(crate::formats::amira::SpiderReader::new()),
        // Fuji LAS gel (.img + .inf companion); detected via the .inf sibling,
        // so it must precede the other extension-only .img readers below.
        Box::new(crate::formats::legacy::FujiReader::new()),
        Box::new(crate::formats::imagic::ImagicReader::new()),
        Box::new(crate::formats::flim::SdtReader::new()),
        // Becker & Hickl SPC FIFO photon stream (.spc/.set); distinct format
        // from SdtReader's SDT container (.sdt).
        Box::new(crate::formats::flim2::SpcReader::new()),
        Box::new(crate::formats::flim::LiFlimReader::new()),
        Box::new(crate::formats::clinical::Ecat7Reader::new()),
        Box::new(crate::formats::clinical::VarianFdfReader::new()),
        Box::new(crate::formats::dcimg::DcimgReader::new()),
        Box::new(crate::formats::norpix::NorpixReader::new()),
        Box::new(crate::formats::norpix::IplabReader::new()),
        Box::new(crate::formats::ome_xml::OmeXmlReader::new()),
        Box::new(crate::formats::olympus::Fv1000Reader::new()),
        // Magic-byte detected formats
        Box::new(crate::formats::pcx::PcxReader::new()),
        Box::new(crate::formats::psd::PsdReader::new()),
        Box::new(crate::formats::aim::AimReader::new()),
        // Molecular Imaging STP (.stp) — distinctive "UK SOFT" magic string.
        Box::new(crate::formats::misc4::MolecularImagingReader::new()),
        // Prairie/Leica XML+TIFF series (magic-byte detection via XML content)
        Box::new(crate::formats::prairie::PrairieReader::new()),
        Box::new(crate::formats::prairie::TcsReader::new()),
        // EPS/PostScript
        Box::new(crate::formats::eps::EpsReader::new()),
        // Extension-only TIFF-based formats (no distinct magic bytes)
        Box::new(crate::formats::zeiss_lsm::ZeissLsmReader::new()),
        Box::new(crate::formats::metamorph::MetamorphReader::new()),
        Box::new(crate::formats::micromanager::MicromanagerReader::new()),
        // OpenSlide-based whole-slide formats (MRXS, VMS, BIF, etc.)
        #[cfg(feature = "openslide")]
        Box::new(crate::formats::openslide_reader::OpenSlideReader::new()),
        // Whole-slide TIFF wrappers (extension-only)
        Box::new(crate::formats::svs::SvsReader::new()),
        // Extension-only Inveon (hdr+img pair, extension-only detection)
        Box::new(crate::formats::clinical::InveonReader::new()),
        // SimFCS FLIM (extension-only). Non-upstream extension: Bio-Formats has
        // no SimFCS reader; kept as a documented extra (reads 256x256 .r64/.ref).
        Box::new(crate::formats::simfcs::SimfcsReader::new()),
        // AFM formats (extension-only)
        Box::new(crate::formats::afm::TopometrixReader::new()),
        Box::new(crate::formats::afm::UnisokuReader::new()),
        // LIM / TillVision (extension-only)
        Box::new(crate::formats::lim::LimReader::new()),
        Box::new(crate::formats::lim::TillVisionReader::new()),
        // AIM/ISQ extension-only fallback
        // DM2 (extension-only, Gatan)
        Box::new(crate::formats::gatan::GatanDm2Reader::new()),
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
        Box::new(crate::formats::sem::FeiReader::new()),
        // FEI SER (magic-byte detected: 0x97 0x01)
        Box::new(crate::formats::mias::FeiSerReader::new()),
        // AVI video (RIFF magic)
        Box::new(crate::formats::avi::AviReader::new()),
        // Leica LEI confocal (magic ILIS / 0x49494949)
        Box::new(crate::formats::leica::LeicaReader::new()),
        // PerkinElmer FLEX HCS (TIFF-based)
        Box::new(crate::formats::flex::FlexReader::new()),
        // Bruker MRI / ParaVision (filename "fid"/"acqp", 2dseq pixel blocks)
        Box::new(crate::formats::bruker::BrukerReader::new()),
        // GE MicroCT VFF (magic "ncaa", `.vff` slice datasets)
        Box::new(crate::formats::bruker::MicroCtVffReader::new()),
        // Extension-only readers
        Box::new(crate::formats::volocity::VolocityReader::new()),
        Box::new(crate::formats::volocity::NikonNisReader::new()),
        Box::new(crate::formats::legacy::KodakReader::new()),
        Box::new(crate::formats::legacy::PictReader::new()),
        Box::new(crate::formats::zeiss_xrm::ZeissXrmReader::new()),
        Box::new(crate::formats::zeiss_zvi::ZeissZviReader::new()),
        // TIFF-based whole-slide / variant formats (extension-only)
        Box::new(crate::formats::tiff_wrappers::NdpiReader::new()),
        Box::new(crate::formats::tiff_wrappers::LeicaScnReader::new()),
        Box::new(crate::formats::tiff_wrappers::VentanaReader::new()),
        Box::new(crate::formats::tiff_wrappers::NikonElementsTiffReader::new()),
        Box::new(crate::formats::tiff_wrappers::FeiTiffReader::new()),
        Box::new(crate::formats::tiff_wrappers::SisReader::new()),
        Box::new(crate::formats::tiff_wrappers::ImprovisionTiffReader::new()),
        Box::new(crate::formats::tiff_wrappers::ZeissApotomeTiffReader::new()),
        Box::new(crate::formats::tiff_wrappers::FluoviewReader::new()),
        Box::new(crate::formats::tiff_wrappers::MolecularDevicesTiffReader::new()),
        // Misc readers: partial native ports plus explicit unsupported detectors
        Box::new(crate::formats::misc::Jpeg2000Reader::new()), // magic-byte detection
        Box::new(crate::formats::misc::QtReader::new()),
        Box::new(crate::formats::misc::MngReader::new()),
        Box::new(crate::formats::misc::SlidebookReader::new()),
        Box::new(crate::formats::misc::MincReader::new()),
        Box::new(crate::formats::misc::OpenlabReader::new()),
        Box::new(crate::formats::misc::SmCameraReader::new()),
        // Extended formats — TIFF wrappers
        Box::new(crate::formats::extended::DngReader::new()),
        Box::new(crate::formats::extended::VectraReader::new()),
        Box::new(crate::formats::extended::GelReader::new()),
        // Extended formats — binary with magic/structure
        Box::new(crate::formats::extended::ImspectorReader::new()), // magic "OMAS_BF_"
        Box::new(crate::formats::extended::HamamatsuVmsReader::new()),
        Box::new(crate::formats::extended::CellomicsReader::new()),
        // Extended formats — real native readers plus explicit unsupported detectors
        Box::new(crate::formats::extended::MrwReader::new()),
        Box::new(crate::formats::extended::YokogawaReader::new()),
        Box::new(crate::formats::extended::LofReader::new()),
        Box::new(crate::formats::extended::PovrayReader::new()),
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
        Box::new(crate::formats::sem::ImodReader::new()),
        // SPM — scanning probe / AFM
        Box::new(crate::formats::spm::PicoQuantReader::new()),
        // PicoQuant .bin time-resolved histogram cube; strict length-magic in
        // set_id keeps it from claiming arbitrary .bin files.
        Box::new(crate::formats::spm::PqBinReader::new()),
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
        Box::new(crate::formats::flim2::AfiReader::new()),
        Box::new(crate::formats::flim2::ImarisTiffReader::new()),
        Box::new(crate::formats::flim2::XlefReader::new()),
        // Olympus OMP2 tiled mosaic (.omp2info); delegates tile pixels to the
        // OIR/VSI readers. Distinct XML magic, so it claims .omp2info first.
        Box::new(crate::formats::olympus::OlympusTileReader::new()),
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
        Box::new(crate::formats::misc4::FilePatternReader::new()),
        Box::new(crate::formats::misc4::KlbReader::new()),
        Box::new(crate::formats::misc4::ObfReader::new()),
        // OME-Zarr / OME-NGFF (directory-based; detected by `.zarr` path or a
        // Zarr group marker). Handled explicitly in `open_reader` before
        // `peek_header`, which cannot read a directory.
#[cfg(feature = "zarr")]
        Box::new(crate::formats::zarr::OmeZarrReader::new()),
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

    let header = peek_header(path, 512)?;
    let mut best_error = None;

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

    // TIFF-based vendor wrappers often have no magic beyond TIFF itself.
    // Give non-generic TIFF extensions a chance before the broad TiffReader
    // byte signature accepts the file.
    if is_tiff_header(&header) {
        for mut r in tiff_wrapper_readers_for_extension(path, &header) {
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

/// Select a likely reader without calling `set_id`.
///
/// This is used only for memoized metadata cache hits where the file stamp and
/// cached metadata shape have already been validated and callers may never read
/// pixels. Name matching is tried before broad magic matching so extension
/// fallbacks that previously succeeded after a magic-reader `set_id` rejection
/// can still use their cached metadata without paying the full parse cost.
pub(crate) fn detect_reader_without_set_id(path: &Path) -> Result<Box<dyn FormatReader>> {
    let header = peek_header(path, 512)?;

    if is_tiff_header(&header) {
        let readers = if is_generic_tiff_extension(path) {
            generic_tiff_name_wrappers(path, &header)
        } else {
            tiff_wrapper_readers_for_extension(path, &header)
        };
        if let Some(reader) = readers.into_iter().next() {
            return Ok(reader);
        }
    }

    for r in all_readers() {
        if r.is_this_type_by_name(path) {
            return Ok(r);
        }
    }

    for r in all_readers() {
        if r.is_this_type_by_bytes(&header) {
            return Ok(r);
        }
    }

    Err(BioFormatsError::UnsupportedFormat(
        path.display().to_string(),
    ))
}

fn has_ims_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("ims"))
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
        Some("lsm") => vec![boxed_reader(crate::formats::zeiss_lsm::ZeissLsmReader::new())],
        Some("stk") => vec![boxed_reader(
            crate::formats::metamorph::MetamorphReader::new(),
        )],
        Some("svs") => vec![boxed_reader(
            crate::formats::svs::SvsReader::new(),
        )],
        Some("ndpi") => vec![
            boxed_reader(crate::formats::tiff_wrappers::NdpiReader::new()),
            boxed_reader(crate::formats::svs::SvsReader::new()),
        ],
        Some("scn") => vec![
            boxed_reader(crate::formats::tiff_wrappers::LeicaScnReader::new()),
            boxed_reader(crate::formats::svs::SvsReader::new()),
            boxed_reader(crate::formats::flim2::BioRadScnReader::new()),
        ],
        Some("bif") => vec![
            boxed_reader(crate::formats::tiff_wrappers::VentanaReader::new()),
            boxed_reader(crate::formats::svs::SvsReader::new()),
        ],
        Some("vsi") => vec![
            boxed_reader(crate::formats::flim2::CellSensReader::new()),
            boxed_reader(crate::formats::svs::SvsReader::new()),
        ],
        Some("afi") => vec![
            boxed_reader(crate::formats::flim2::AfiReader::new()),
            boxed_reader(crate::formats::svs::SvsReader::new()),
        ],
        Some("dng") => vec![boxed_reader(crate::formats::extended::DngReader::new())],
        Some("qptiff") => vec![boxed_reader(crate::formats::extended::VectraReader::new())],
        Some("gel") => vec![boxed_reader(crate::formats::extended::GelReader::new())],
        Some("flex") => vec![boxed_reader(crate::formats::flex::FlexReader::new())],
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
            // MIAS datasets are plain TIFFs, so the generic TiffReader would
            // win the magic pass. Give MiasReader a chance first, but ONLY when
            // the file is genuinely a MIAS plane (a Well<xxxx> directory +
            // mode/z/t naming), so ordinary .tif files still fall through.
            let mut readers = generic_tiff_name_wrappers(path, header);

            // Nikon EZ-C1 confocal TIFFs are plain TIFFs identified only by a
            // SOFTWARE tag containing "EZ-C1". Gate on that tag (mirroring the
            // ImageDescription gating below) so ordinary TIFFs are untouched.
            if let Some(software) = tiff_software_tag(path) {
                if software.contains("EZ-C1") {
                    readers.push(boxed_reader(
                        crate::formats::tiff_wrappers::NikonTiffReader::new(),
                    ));
                }
                // Faas-format pyramid TIFFs are identified by a SOFTWARE tag
                // containing "Faas" (mirrors Java PyramidTiffReader.isThisType).
                if software.contains("Faas") {
                    readers.push(boxed_reader(crate::formats::svs::PyramidTiffReader::new()));
                }
            }

            if let Some(description) = tiff_image_description(path) {
                readers.extend(generic_tiff_wrappers_for_description(
                    ext.as_deref().unwrap_or_default(),
                    &description,
                ));
            }

            readers
        }
        _ => Vec::new(),
    }
}

fn is_generic_tiff_extension(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| matches!(e.to_ascii_lowercase().as_str(), "tif" | "tiff"))
        .unwrap_or(false)
}

fn generic_tiff_name_wrappers(path: &Path, header: &[u8]) -> Vec<Box<dyn FormatReader>> {
    let mut readers = Vec::new();
    let mias = crate::formats::mias::MiasReader::new();
    if mias.is_this_type_by_name(path) {
        readers.push(boxed_reader(mias));
    }
    if has_prairie_xml_sibling(path) {
        readers.push(boxed_reader(crate::formats::prairie::PrairieReader::new()));
    }
    let lei = crate::formats::leica::LeicaReader::new();
    if has_lei_sibling(path) && lei.is_this_type_by_bytes(header) {
        readers.push(boxed_reader(lei));
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

fn generic_tiff_wrappers_for_description(
    ext: &str,
    description: &str,
) -> Vec<Box<dyn FormatReader>> {
    let mut readers = Vec::new();

    // Metamorph TIFF carries a `<MetaData>...</MetaData>` ImageDescription
    // comment (mirrors Java MetamorphTiffReader.isThisType); applies to .tif/.tiff.
    if description.trim_start().starts_with("<MetaData>") {
        readers.push(boxed_reader(
            crate::formats::tiff_wrappers::MetamorphTiffReader::new(),
        ));
    }

    match ext {
        "tif" => {
            readers.extend(hcs_tiff_wrappers_for_description(description));

            if description.contains("[Acquisition Parameters]") || description.contains("FluoView")
            {
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
    if lower.contains("simplepci") || lower.contains("simple pci") || lower.contains("hcimage") {
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
    fn generic_tif_wrapper_dispatch_uses_fluoview_metadata_signature() {
        let path = temp_path("fluoview_metadata.tif");
        write_minimal_tiff_with_description(&path, "[Acquisition Parameters]\nLaser=488\n");

        let reader = ImageReader::open(&path).expect("Fluoview wrapper dispatch failed");

        assert!(matches!(
            reader.metadata().series_metadata.get("fluoview.Laser"),
            Some(MetadataValue::String(value)) if value == "488"
        ));
        let _ = std::fs::remove_file(path);
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
    fn generic_tif_hcs_wrapper_dispatch_uses_metadata_signature() {
        let path = temp_path("simplepci_metadata.tif");
        write_minimal_tiff_with_description(&path, "Created by SimplePCI HCImage\n");

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
            ("sample.b16", "PCO B16 file is too short"),
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
    fn visitech_registry_rejects_xys_without_companion_tiffs() {
        let dir = temp_path("hcs_no_tiffs_dir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("sample Report.html"),
            b"Image dimensions: (2, 2)\nNumber of steps: 1\nMicroscope XY: 0\nImage bit depth: 16\nChannel Selection: 1\nTime Series; 1\n",
        )
        .unwrap();

        for (name, bytes, expected) in [
            (
                "sample.xys",
                b"no pixel marker here".as_slice(),
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

    fn tiff_entry(tag: u16, field_type: u16, count: u32, value_or_offset: u32) -> [u8; 12] {
        let mut entry = [0u8; 12];
        entry[0..2].copy_from_slice(&tag.to_le_bytes());
        entry[2..4].copy_from_slice(&field_type.to_le_bytes());
        entry[4..8].copy_from_slice(&count.to_le_bytes());
        entry[8..12].copy_from_slice(&value_or_offset.to_le_bytes());
        entry
    }
}
