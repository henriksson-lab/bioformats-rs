use crate::common::reader::FormatReader;

const JAVA_READERS_TXT: &str = include_str!("readers.txt");

pub(crate) fn all_readers() -> Vec<Box<dyn FormatReader>> {
    let mut readers: Vec<Box<dyn FormatReader>> = vec![
        // Rust-supported extra not present in the local Java readers.txt. Keep
        // it before the Java list so `.ptu` is not shadowed by broad suffix
        // readers such as NDPIS.
        Box::new(crate::formats::spm::PicoQuantReader::new()),
    ];
    for class in java_reader_class_names() {
        if let Some(reader) = java_reader_for_class(class) {
            readers.push(reader);
        }
        if class == "loci.formats.in.APNGReader" {
            // Java has one PNG-family entry here; Rust keeps still PNG and APNG
            // as separate readers, so insert the still-PNG reader at the same
            // Java-order point.
            readers.push(Box::new(crate::formats::png::PngReader::new()));
        }
    }
    append_rust_extra_readers(&mut readers);
    readers
}

fn java_reader_class_names() -> impl Iterator<Item = &'static str> {
    JAVA_READERS_TXT.lines().filter_map(|line| {
        let before_comment = line.split('#').next()?.trim();
        if before_comment.is_empty() {
            return None;
        }
        Some(
            before_comment
                .split('[')
                .next()
                .unwrap_or(before_comment)
                .trim(),
        )
    })
}

fn java_reader_for_class(class: &str) -> Option<Box<dyn FormatReader>> {
    Some(match class {
        "loci.formats.in.FilePatternReader" => {
            Box::new(crate::formats::misc4::FilePatternReader::new())
        }
        "loci.formats.in.ZipReader" => Box::new(crate::formats::zip::ZipReader::new()),
        "loci.formats.in.APNGReader" => Box::new(crate::formats::extended::ApngReader::new()),
        "loci.formats.in.JPEGReader" => Box::new(crate::formats::jpeg::JpegReader::new()),
        "loci.formats.in.SlideBook7Reader" => {
            Box::new(crate::formats::flim2::SlideBook7Reader::new())
        }
        "loci.formats.in.ZarrReader" => {
            #[cfg(feature = "zarr")]
            {
                Box::new(crate::formats::zarr::OmeZarrReader::new())
            }
            #[cfg(not(feature = "zarr"))]
            {
                return None;
            }
        }
        "loci.formats.in.PGMReader" => Box::new(crate::formats::raster::pnm_reader()),
        "loci.formats.in.FitsReader" => Box::new(crate::formats::fits::FitsReader::new()),
        "loci.formats.in.PCXReader" => Box::new(crate::formats::pcx::PcxReader::new()),
        "loci.formats.in.GIFReader" => Box::new(crate::formats::raster::gif_reader()),
        "loci.formats.in.BMPReader" => Box::new(crate::formats::bmp::BmpReader::new()),
        "loci.formats.in.IPLabReader" => Box::new(crate::formats::norpix::IplabReader::new()),
        "loci.formats.in.IvisionReader" => Box::new(crate::formats::flim2::IvisionReader::new()),
        "loci.formats.in.RCPNLReader" => Box::new(crate::formats::hcs2::RcpnlReader::new()),
        "loci.formats.in.DeltavisionReader" => {
            Box::new(crate::formats::deltavision::DeltavisionReader::new())
        }
        "loci.formats.in.MRCReader" => Box::new(crate::formats::mrc::MrcReader::new()),
        "loci.formats.in.GatanReader" => Box::new(crate::formats::gatan::GatanReader::new()),
        "loci.formats.in.GatanDM2Reader" => Box::new(crate::formats::gatan::GatanDm2Reader::new()),
        "loci.formats.in.ImarisReader" => Box::new(crate::formats::flim2::ImarisReader::new()),
        "loci.formats.in.OpenlabRawReader" => {
            Box::new(crate::formats::perkinelmer::OpenlabRawReader::new())
        }
        "loci.formats.in.OMEXMLReader" => Box::new(crate::formats::ome_xml::OmeXmlReader::new()),
        "loci.formats.in.LIFReader" => Box::new(crate::formats::lif::LifReader::new()),
        "loci.formats.in.AVIReader" => Box::new(crate::formats::avi::AviReader::new()),
        "loci.formats.in.PictReader" => Box::new(crate::formats::legacy::PictReader::new()),
        "loci.formats.in.SDTReader" => Box::new(crate::formats::flim::SdtReader::new()),
        "loci.formats.in.SPCReader" => Box::new(crate::formats::flim2::SpcReader::new()),
        "loci.formats.in.EPSReader" => Box::new(crate::formats::eps::EpsReader::new()),
        "loci.formats.in.SlidebookReader" => Box::new(crate::formats::misc::SlidebookReader::new()),
        "loci.formats.in.AliconaReader" => Box::new(crate::formats::mias::AliconaReader::new()),
        "loci.formats.in.MNGReader" => Box::new(crate::formats::misc::MngReader::new()),
        "loci.formats.in.KhorosReader" => Box::new(crate::formats::khoros::KhorosReader::new()),
        "loci.formats.in.VisitechReader" => {
            Box::new(crate::formats::visitech::VisitechReader::new())
        }
        "loci.formats.in.LIMReader" => Box::new(crate::formats::lim::LimReader::new()),
        "loci.formats.in.PSDReader" => Box::new(crate::formats::psd::PsdReader::new()),
        "loci.formats.in.InCellReader" => Box::new(crate::formats::incell::InCellReader::new()),
        "loci.formats.in.L2DReader" => Box::new(crate::formats::camera2::L2dReader::new()),
        "loci.formats.in.FEIReader" => Box::new(crate::formats::sem::FeiReader::new()),
        "loci.formats.in.NAFReader" => Box::new(crate::formats::extended::NafReader::new()),
        "loci.formats.in.MINCReader" => Box::new(crate::formats::misc::MincReader::new()),
        "loci.formats.in.QTReader" => Box::new(crate::formats::misc::QtReader::new()),
        "loci.formats.in.MRWReader" => Box::new(crate::formats::extended::MrwReader::new()),
        "loci.formats.in.TillVisionReader" => {
            Box::new(crate::formats::lim::TillVisionReader::new())
        }
        "loci.formats.in.ARFReader" => Box::new(crate::formats::misc4::ArfReader::new()),
        "loci.formats.in.CellomicsReader" => {
            Box::new(crate::formats::extended::CellomicsReader::new())
        }
        "loci.formats.in.LiFlimReader" => Box::new(crate::formats::flim::LiFlimReader::new()),
        "loci.formats.in.TargaReader" => Box::new(crate::formats::raster::tga_reader()),
        "loci.formats.in.OxfordInstrumentsReader" => {
            Box::new(crate::formats::mias::OxfordInstrumentsReader::new())
        }
        "loci.formats.in.VGSAMReader" => Box::new(crate::formats::spm::VgSamReader::new()),
        "loci.formats.in.HISReader" => Box::new(crate::formats::misc4::HisReader::new()),
        "loci.formats.in.WATOPReader" => Box::new(crate::formats::spm::WatopReader::new()),
        "loci.formats.in.SeikoReader" => Box::new(crate::formats::spm::SeikoReader::new()),
        "loci.formats.in.TopometrixReader" => {
            Box::new(crate::formats::afm::TopometrixReader::new())
        }
        "loci.formats.in.UBMReader" => Box::new(crate::formats::spm::UbmReader::new()),
        "loci.formats.in.QuesantReader" => Box::new(crate::formats::spm::QuesantReader::new()),
        "loci.formats.in.BioRadGelReader" => {
            Box::new(crate::formats::camera2::BioRadGelReader::new())
        }
        "loci.formats.in.RHKReader" => Box::new(crate::formats::spm::RhkReader::new()),
        "loci.formats.in.MolecularImagingReader" => {
            Box::new(crate::formats::misc4::MolecularImagingReader::new())
        }
        "loci.formats.in.CellWorxReader" => Box::new(crate::formats::mias::CellWorxReader::new()),
        "loci.formats.in.MetaxpressTiffReader" => {
            Box::new(crate::formats::hcs2::MetaxpressTiffReader::new())
        }
        "loci.formats.in.Ecat7Reader" => Box::new(crate::formats::clinical::Ecat7Reader::new()),
        "loci.formats.in.VarianFDFReader" => {
            Box::new(crate::formats::clinical::VarianFdfReader::new())
        }
        "loci.formats.in.AIMReader" => Box::new(crate::formats::aim::AimReader::new()),
        "loci.formats.in.InCell3000Reader" => {
            Box::new(crate::formats::hcs2::InCell3000Reader::new())
        }
        "loci.formats.in.SpiderReader" => Box::new(crate::formats::amira::SpiderReader::new()),
        "loci.formats.in.VolocityReader" => {
            Box::new(crate::formats::volocity::VolocityReader::new())
        }
        "loci.formats.in.ImagicReader" => Box::new(crate::formats::imagic::ImagicReader::new()),
        "loci.formats.in.HamamatsuVMSReader" => {
            Box::new(crate::formats::extended::HamamatsuVmsReader::new())
        }
        "loci.formats.in.CellSensReader" => Box::new(crate::formats::flim2::CellSensReader::new()),
        "loci.formats.in.INRReader" => Box::new(crate::formats::sem::InrReader::new()),
        "loci.formats.in.KodakReader" => Box::new(crate::formats::legacy::KodakReader::new()),
        "loci.formats.in.VolocityClippingReader" => {
            Box::new(crate::formats::flim2::VolocityClippingReader::new())
        }
        "loci.formats.in.ZeissCZIReader" => {
            Box::new(crate::formats::zeiss_czi::ZeissCziReader::new())
        }
        "loci.formats.in.SIFReader" => Box::new(crate::formats::sif::SifReader::new()),
        "loci.formats.in.NDPISReader" => Box::new(crate::formats::flim2::NdpisReader::new()),
        "loci.formats.in.PovrayReader" => Box::new(crate::formats::extended::PovrayReader::new()),
        "loci.formats.in.IMODReader" => Box::new(crate::formats::sem::ImodReader::new()),
        "loci.formats.in.FakeReader" => Box::new(crate::formats::fake::FakeReader::new()),
        "loci.formats.in.AFIReader" => Box::new(crate::formats::flim2::AfiReader::new()),
        "loci.formats.in.ImspectorReader" => {
            Box::new(crate::formats::extended::ImspectorReader::new())
        }
        "loci.formats.in.BioRadSCNReader" => {
            Box::new(crate::formats::flim2::BioRadScnReader::new())
        }
        "loci.formats.in.ZeissLMSReader" => Box::new(crate::formats::sem::ZeissLmsReader::new()),
        "loci.formats.in.PQBinReader" => Box::new(crate::formats::spm::PqBinReader::new()),
        "loci.formats.in.FlowSightReader" => {
            Box::new(crate::formats::flim2::FlowSightReader::new())
        }
        "loci.formats.in.IM3Reader" => Box::new(crate::formats::flim2::Im3Reader::new()),
        "loci.formats.in.I2IReader" => Box::new(crate::formats::misc4::I2iReader::new()),
        "loci.formats.in.SPEReader" => Box::new(crate::formats::spe::SpeReader::new()),
        "loci.formats.in.OIRReader" => Box::new(crate::formats::flim2::OirReader::new()),
        "loci.formats.in.KLBReader" => Box::new(crate::formats::misc4::KlbReader::new()),
        "loci.formats.in.MicroCTReader" => Box::new(crate::formats::flim2::MicroCtReader::new()),
        "loci.formats.in.LOFReader" => Box::new(crate::formats::extended::LofReader::new()),
        "loci.formats.in.XLEFReader" => Box::new(crate::formats::flim2::XlefReader::new()),
        "loci.formats.in.OlympusTileReader" => {
            Box::new(crate::formats::olympus::OlympusTileReader::new())
        }
        "loci.formats.in.DCIMGReader" => Box::new(crate::formats::dcimg::DcimgReader::new()),
        "loci.formats.in.JDCEReader" => Box::new(crate::formats::misc4::JdceReader::new()),
        "loci.formats.in.TissueFAXSReader" => {
            Box::new(crate::formats::hcs2::TissueFaxsReader::new())
        }
        "loci.formats.in.ZeissXRMReader" => {
            Box::new(crate::formats::zeiss_xrm::ZeissXrmReader::new())
        }
        "loci.formats.in.JEOLReader" => Box::new(crate::formats::sem::JeolReader::new()),
        "loci.formats.in.NiftiReader" => Box::new(crate::formats::nifti::NiftiReader::new()),
        "loci.formats.in.APLReader" => Box::new(crate::formats::misc4::AplReader::new()),
        "loci.formats.in.NRRDReader" => Box::new(crate::formats::nrrd::NrrdReader::new()),
        "loci.formats.in.ICSReader" => Box::new(crate::formats::ics::IcsReader::new()),
        "loci.formats.in.PerkinElmerReader" => {
            Box::new(crate::formats::perkinelmer::PerkinElmerReader::new())
        }
        "loci.formats.in.AmiraReader" => Box::new(crate::formats::amira::AmiraReader::new()),
        "loci.formats.in.ScanrReader" => Box::new(crate::formats::hcs2::ScanrReader::new()),
        "loci.formats.in.BDReader" => Box::new(crate::formats::hcs2::BdReader::new()),
        "loci.formats.in.UnisokuReader" => Box::new(crate::formats::afm::UnisokuReader::new()),
        "loci.formats.in.PDSReader" => Box::new(crate::formats::misc4::PdsReader::new()),
        "loci.formats.in.FujiReader" => Box::new(crate::formats::legacy::FujiReader::new()),
        "loci.formats.in.OperettaReader" => Box::new(crate::formats::hcs2::OperettaReader::new()),
        "loci.formats.in.InveonReader" => Box::new(crate::formats::clinical::InveonReader::new()),
        "loci.formats.in.CellVoyagerReader" => {
            Box::new(crate::formats::hcs2::CellVoyagerReader::new())
        }
        "loci.formats.in.ColumbusReader" => Box::new(crate::formats::hcs2::ColumbusReader::new()),
        "loci.formats.in.BioRadReader" => Box::new(crate::formats::biorad::BioRadReader::new()),
        "loci.formats.in.FV1000Reader" => Box::new(crate::formats::olympus::Fv1000Reader::new()),
        "loci.formats.in.ZeissZVIReader" => {
            Box::new(crate::formats::zeiss_zvi::ZeissZviReader::new())
        }
        "loci.formats.in.IPWReader" => Box::new(crate::formats::camera2::IpwReader::new()),
        "loci.formats.in.JPEG2000Reader" => Box::new(crate::formats::misc::Jpeg2000Reader::new()),
        "loci.formats.in.JPXReader" => Box::new(crate::formats::misc4::JpxReader::new()),
        "loci.formats.in.ND2Reader" => Box::new(crate::formats::nd2::Nd2Reader::new()),
        "loci.formats.in.PCIReader" => Box::new(crate::formats::misc4::PciReader::new()),
        "loci.formats.in.ImarisHDFReader" => {
            Box::new(crate::formats::imaris_hdf::ImarisHdfReader::new())
        }
        "loci.formats.in.CellH5Reader" => Box::new(crate::formats::cellh5::CellH5Reader::new()),
        "loci.formats.in.VeecoReader" => Box::new(crate::formats::sem::VeecoReader::new()),
        "loci.formats.in.TecanReader" => Box::new(crate::formats::hcs2::TecanReader::new()),
        "loci.formats.in.ZeissLSMReader" => {
            Box::new(crate::formats::zeiss_lsm::ZeissLsmReader::new())
        }
        "loci.formats.in.SEQReader" => Box::new(crate::formats::norpix::SeqReader::new()),
        "loci.formats.in.GelReader" => Box::new(crate::formats::extended::GelReader::new()),
        "loci.formats.in.ImarisTiffReader" => {
            Box::new(crate::formats::flim2::ImarisTiffReader::new())
        }
        "loci.formats.in.FlexReader" => Box::new(crate::formats::flex::FlexReader::new()),
        "loci.formats.in.SVSReader" => Box::new(crate::formats::svs::SvsReader::new()),
        "loci.formats.in.ImaconReader" => Box::new(crate::formats::camera2::ImaconReader::new()),
        "loci.formats.in.LEOReader" => Box::new(crate::formats::sem::LeoReader::new()),
        "loci.formats.in.JPKReader" => Box::new(crate::formats::spm::JpkReader::new()),
        "loci.formats.in.NDPIReader" => Box::new(crate::formats::tiff_wrappers::NdpiReader::new()),
        "loci.formats.in.PCORAWReader" => Box::new(crate::formats::camera2::PcoRawReader::new()),
        "loci.formats.in.VentanaReader" => {
            Box::new(crate::formats::tiff_wrappers::VentanaReader::new())
        }
        "loci.formats.in.OMETiffReader" => return None,
        "loci.formats.in.PyramidTiffReader" => {
            Box::new(crate::formats::svs::PyramidTiffReader::new())
        }
        "loci.formats.in.MIASReader" => Box::new(crate::formats::mias::MiasReader::new()),
        "loci.formats.in.TCSReader" => Box::new(crate::formats::prairie::TcsReader::new()),
        "loci.formats.in.LeicaReader" => Box::new(crate::formats::leica::LeicaReader::new()),
        "loci.formats.in.NikonReader" => Box::new(crate::formats::camera2::NikonReader::new()),
        "loci.formats.in.FluoviewReader" => {
            Box::new(crate::formats::tiff_wrappers::FluoviewReader::new())
        }
        "loci.formats.in.PrairieReader" => Box::new(crate::formats::prairie::PrairieReader::new()),
        "loci.formats.in.MetamorphReader" => {
            Box::new(crate::formats::metamorph::MetamorphReader::new())
        }
        "loci.formats.in.MicromanagerReader" => {
            Box::new(crate::formats::micromanager::MicromanagerReader::new())
        }
        "loci.formats.in.ImprovisionTiffReader" => {
            Box::new(crate::formats::tiff_wrappers::ImprovisionTiffReader::new())
        }
        "loci.formats.in.MetamorphTiffReader" => {
            Box::new(crate::formats::tiff_wrappers::MetamorphTiffReader::new())
        }
        "loci.formats.in.NikonTiffReader" => {
            Box::new(crate::formats::tiff_wrappers::NikonTiffReader::new())
        }
        "loci.formats.in.MikroscanTiffReader" => {
            Box::new(crate::formats::hcs2::MikroscanTiffReader::new())
        }
        "loci.formats.in.PhotoshopTiffReader" => {
            Box::new(crate::formats::camera2::PhotoshopTiffReader::new())
        }
        "loci.formats.in.FEITiffReader" => {
            Box::new(crate::formats::tiff_wrappers::FeiTiffReader::new())
        }
        "loci.formats.in.SimplePCITiffReader" => {
            Box::new(crate::formats::hcs2::SimplePciTiffReader::new())
        }
        "loci.formats.in.NikonElementsTiffReader" => {
            Box::new(crate::formats::tiff_wrappers::NikonElementsTiffReader::new())
        }
        "loci.formats.in.TrestleReader" => Box::new(crate::formats::hcs2::TrestleReader::new()),
        "loci.formats.in.SISReader" => Box::new(crate::formats::tiff_wrappers::SisReader::new()),
        "loci.formats.in.DNGReader" => Box::new(crate::formats::extended::DngReader::new()),
        "loci.formats.in.ZeissTIFFReader" => Box::new(crate::formats::sem::ZeissTiffReader::new()),
        "loci.formats.in.LeicaSCNReader" => {
            Box::new(crate::formats::tiff_wrappers::LeicaScnReader::new())
        }
        "loci.formats.in.VectraReader" => Box::new(crate::formats::extended::VectraReader::new()),
        "loci.formats.in.SlidebookTiffReader" => {
            Box::new(crate::formats::flim2::SlidebookTiffReader::new())
        }
        "loci.formats.in.IonpathMIBITiffReader" => {
            Box::new(crate::formats::hcs2::IonpathMibiTiffReader::new())
        }
        "loci.formats.in.DicomReader" => Box::new(crate::formats::dicom::DicomReader::new()),
        "loci.formats.in.HitachiReader" => Box::new(crate::formats::sem::HitachiReader::new()),
        "loci.formats.in.TiffDelegateReader" => Box::new(crate::tiff::TiffReader::new()),
        "loci.formats.in.TextReader" => Box::new(crate::formats::misc::TextReader::new()),
        "loci.formats.in.BurleighReader" => {
            Box::new(crate::formats::extended::BurleighReader::new())
        }
        "loci.formats.in.OpenlabReader" => Box::new(crate::formats::misc::OpenlabReader::new()),
        "loci.formats.in.SMCameraReader" => Box::new(crate::formats::misc::SmCameraReader::new()),
        "loci.formats.in.SBIGReader" => Box::new(crate::formats::camera2::SbigReader::new()),
        "loci.formats.in.HRDGDFReader" => Box::new(crate::formats::misc4::HrdgdfReader::new()),
        "loci.formats.in.BrukerReader" => Box::new(crate::formats::bruker::BrukerReader::new()),
        "loci.formats.in.CanonRawReader" => {
            Box::new(crate::formats::camera2::CanonRawReader::new())
        }
        "loci.formats.in.OBFReader" => Box::new(crate::formats::misc4::ObfReader::new()),
        "loci.formats.in.BDVReader" => Box::new(crate::formats::bdv::BdvReader::new()),
        _ => return None,
    })
}

fn append_rust_extra_readers(readers: &mut Vec<Box<dyn FormatReader>>) {
    readers.push(Box::new(crate::formats::metaimage::MetaImageReader::new()));
    readers.push(Box::new(crate::formats::raster::webp_reader()));
    readers.push(Box::new(crate::formats::raster::hdr_reader()));
    readers.push(Box::new(crate::formats::raster::exr_reader()));
    readers.push(Box::new(crate::formats::raster::dds_reader()));
    readers.push(Box::new(crate::formats::raster::farbfeld_reader()));
    readers.push(Box::new(crate::formats::simfcs::SimfcsReader::new()));
    readers.push(Box::new(crate::formats::mias::FeiSerReader::new()));
    readers.push(Box::new(crate::formats::volocity::NikonNisReader::new()));
    readers.push(Box::new(
        crate::formats::tiff_wrappers::MolecularDevicesTiffReader::new(),
    ));
    #[cfg(feature = "openslide")]
    readers.push(Box::new(
        crate::formats::openslide_reader::OpenSlideReader::new(),
    ));
    readers.push(Box::new(
        crate::formats::perkinelmer::PhotonDynamicsReader::new(),
    ));
    readers.push(Box::new(crate::formats::bruker::MicroCtVffReader::new()));
}
