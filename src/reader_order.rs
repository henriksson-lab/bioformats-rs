use crate::common::reader::FormatReader;

const JAVA_READERS_TXT: &str = include_str!("readers.txt");

pub(crate) fn all_readers() -> Vec<Box<dyn FormatReader>> {
    let mut readers: Vec<Box<dyn FormatReader>> = Vec::new();
    // Rust-supported extra not present in the local Java readers.txt. Keep
    // it before the Java list so `.ptu` is not shadowed by broad suffix
    // readers such as NDPIS.
    #[cfg(feature = "gpl")]
    readers.push(Box::new(crate::formats::gpl::spm::PicoQuantReader::new()));
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
        "loci.formats.in.ZipReader" => Box::new(crate::formats::bsd::zip::ZipReader::new()),
        "loci.formats.in.APNGReader" => Box::new(crate::formats::extended::ApngReader::new()),
        "loci.formats.in.JPEGReader" => Box::new(crate::formats::bsd::jpeg::JpegReader::new()),
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
        "loci.formats.in.FitsReader" => Box::new(crate::formats::bsd::fits::FitsReader::new()),
        "loci.formats.in.PCXReader" => Box::new(crate::formats::bsd::pcx::PcxReader::new()),
        "loci.formats.in.GIFReader" => Box::new(crate::formats::raster::gif_reader()),
        "loci.formats.in.BMPReader" => Box::new(crate::formats::bsd::bmp::BmpReader::new()),
        "loci.formats.in.IPLabReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::norpix::IplabReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.IvisionReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::IvisionReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.RCPNLReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::RcpnlReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.DeltavisionReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::deltavision::DeltavisionReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.MRCReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::mrc::MrcReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.GatanReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::gatan::GatanReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.GatanDM2Reader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::gatan::GatanDm2Reader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ImarisReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::ImarisReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.OpenlabRawReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::perkinelmer::OpenlabRawReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.OMEXMLReader" => {
            Box::new(crate::formats::bsd::ome_xml::OmeXmlReader::new())
        }
        "loci.formats.in.LIFReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::lif::LifReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.AVIReader" => Box::new(crate::formats::bsd::avi::AviReader::new()),
        "loci.formats.in.PictReader" => Box::new(crate::formats::legacy::PictReader::new()),
        "loci.formats.in.SDTReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::flim::SdtReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.SPCReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::SpcReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.EPSReader" => Box::new(crate::formats::bsd::eps::EpsReader::new()),
        "loci.formats.in.SlidebookReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc::SlidebookReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.AliconaReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::mias::AliconaReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.MNGReader" => Box::new(crate::formats::misc::MngReader::new()),
        "loci.formats.in.KhorosReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::khoros::KhorosReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.VisitechReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::visitech::VisitechReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.LIMReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::lim::LimReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.PSDReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::psd::PsdReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.InCellReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::incell::InCellReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.L2DReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::camera2::L2dReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.FEIReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::sem::FeiReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.NAFReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::extended::NafReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.MINCReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc::MincReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.QTReader" => Box::new(crate::formats::misc::QtReader::new()),
        "loci.formats.in.MRWReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::extended::MrwReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.TillVisionReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::lim::TillVisionReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ARFReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc4::ArfReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.CellomicsReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::extended::CellomicsReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.LiFlimReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::flim::LiFlimReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.TargaReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::raster::tga_reader())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.OxfordInstrumentsReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::mias::OxfordInstrumentsReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.VGSAMReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::spm::VgSamReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.HISReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc4::HisReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.WATOPReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::spm::WatopReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.SeikoReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::spm::SeikoReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.TopometrixReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::afm::TopometrixReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.UBMReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::spm::UbmReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.QuesantReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::spm::QuesantReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.BioRadGelReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::camera2::BioRadGelReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.RHKReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::spm::RhkReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.MolecularImagingReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc4::MolecularImagingReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.CellWorxReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::mias::CellWorxReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.MetaxpressTiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::MetaxpressTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.Ecat7Reader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::clinical::Ecat7Reader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.VarianFDFReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::clinical::VarianFdfReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.AIMReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::aim::AimReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.InCell3000Reader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::InCell3000Reader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.SpiderReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::amira::SpiderReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.VolocityReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::volocity::VolocityReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ImagicReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::imagic::ImagicReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.HamamatsuVMSReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::extended::HamamatsuVmsReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.CellSensReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::CellSensReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.INRReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::sem::InrReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.KodakReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::legacy::KodakReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.VolocityClippingReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::VolocityClippingReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ZeissCZIReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::zeiss_czi::ZeissCziReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.SIFReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::sif::SifReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.NDPISReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::NdpisReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.PovrayReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::extended::PovrayReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.IMODReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::sem::ImodReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.FakeReader" => Box::new(crate::formats::bsd::fake::FakeReader::new()),
        "loci.formats.in.AFIReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::AfiReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ImspectorReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::extended::ImspectorReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.BioRadSCNReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::BioRadScnReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ZeissLMSReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::sem::ZeissLmsReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.PQBinReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::spm::PqBinReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.FlowSightReader" => {
            Box::new(crate::formats::flim2::FlowSightReader::new())
        }
        "loci.formats.in.IM3Reader" => Box::new(crate::formats::flim2::Im3Reader::new()),
        "loci.formats.in.I2IReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc4::I2iReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.SPEReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::spe::SpeReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.OIRReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::OirReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.KLBReader" => Box::new(crate::formats::misc4::KlbReader::new()),
        "loci.formats.in.MicroCTReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::MicroCtReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.LOFReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::extended::LofReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.XLEFReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::XlefReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.OlympusTileReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::olympus::OlympusTileReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.DCIMGReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::dcimg::DcimgReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.JDCEReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc4::JdceReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.TissueFAXSReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::TissueFaxsReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ZeissXRMReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::zeiss_xrm::ZeissXrmReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.JEOLReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::sem::JeolReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.NiftiReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::nifti::NiftiReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.APLReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc4::AplReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.NRRDReader" => Box::new(crate::formats::bsd::nrrd::NrrdReader::new()),
        "loci.formats.in.ICSReader" => Box::new(crate::formats::bsd::ics::IcsReader::new()),
        "loci.formats.in.PerkinElmerReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::perkinelmer::PerkinElmerReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.AmiraReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::amira::AmiraReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ScanrReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::ScanrReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.BDReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::BdReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.UnisokuReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::afm::UnisokuReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.PDSReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc4::PdsReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.FujiReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::legacy::FujiReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.OperettaReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::OperettaReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.InveonReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::clinical::InveonReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.CellVoyagerReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::CellVoyagerReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ColumbusReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::ColumbusReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.CV7000Reader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::extended::YokogawaReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.BioRadReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::biorad::BioRadReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.FV1000Reader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::olympus::Fv1000Reader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ZeissZVIReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::zeiss_zvi::ZeissZviReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.IPWReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::camera2::IpwReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.JPEG2000Reader" => Box::new(crate::formats::misc::Jpeg2000Reader::new()),
        "loci.formats.in.JPXReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc4::JpxReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ND2Reader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::nd2::Nd2Reader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.PCIReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc4::PciReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ImarisHDFReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::imaris_hdf::ImarisHdfReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.CellH5Reader" => {
            Box::new(crate::formats::bsd::cellh5::CellH5Reader::new())
        }
        "loci.formats.in.VeecoReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::sem::VeecoReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.TecanReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::TecanReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ZeissLSMReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::zeiss_lsm::ZeissLsmReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.SEQReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::norpix::SeqReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.GelReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::extended::GelReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ImarisTiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::ImarisTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.FlexReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::flex::FlexReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.SVSReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::svs::SvsReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ImaconReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::camera2::ImaconReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.LEOReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::sem::LeoReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.JPKReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::spm::JpkReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.NDPIReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::tiff_wrappers::NdpiReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.PCORAWReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::camera2::PcoRawReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.VentanaReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::tiff_wrappers::VentanaReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.OMETiffReader" => return None,
        "loci.formats.in.PyramidTiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::svs::PyramidTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.MIASReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::mias::MiasReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.TCSReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::prairie::TcsReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.LeicaReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::leica::LeicaReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.NikonReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::camera2::NikonReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.FluoviewReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::tiff_wrappers::FluoviewReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.PrairieReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::prairie::PrairieReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.MetamorphReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::metamorph::MetamorphReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.MicromanagerReader" => {
            Box::new(crate::formats::bsd::micromanager::MicromanagerReader::new())
        }
        "loci.formats.in.ImprovisionTiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::tiff_wrappers::ImprovisionTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.MetamorphTiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::tiff_wrappers::MetamorphTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.NikonTiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::tiff_wrappers::NikonTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.MikroscanTiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::MikroscanTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.PhotoshopTiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::camera2::PhotoshopTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.FEITiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::tiff_wrappers::FeiTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.SimplePCITiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::SimplePciTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.NikonElementsTiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::tiff_wrappers::NikonElementsTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.TrestleReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::TrestleReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.SISReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::tiff_wrappers::SisReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.DNGReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::extended::DngReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.ZeissTIFFReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::sem::ZeissTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.LeicaSCNReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::tiff_wrappers::LeicaScnReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.VectraReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::extended::VectraReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.SlidebookTiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::flim2::SlidebookTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.IonpathMIBITiffReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::hcs2::IonpathMibiTiffReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.DicomReader" => Box::new(crate::formats::bsd::dicom::DicomReader::new()),
        "loci.formats.in.HitachiReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::sem::HitachiReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.TiffDelegateReader" => Box::new(crate::tiff::TiffReader::new()),
        "loci.formats.in.TextReader" => Box::new(crate::formats::misc::TextReader::new()),
        "loci.formats.in.BurleighReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::extended::BurleighReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.OpenlabReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc::OpenlabReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.SMCameraReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc::SmCameraReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.SBIGReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::camera2::SbigReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.HRDGDFReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::misc4::HrdgdfReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.BrukerReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::bruker::BrukerReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.CanonRawReader" => {
            #[cfg(feature = "gpl")]
            {
                Box::new(crate::formats::gpl::camera2::CanonRawReader::new())
            }
            #[cfg(not(feature = "gpl"))]
            {
                return None;
            }
        }
        "loci.formats.in.OBFReader" => Box::new(crate::formats::misc4::ObfReader::new()),
        "loci.formats.in.BDVReader" => Box::new(crate::formats::bsd::bdv::BdvReader::new()),
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
    #[cfg(feature = "gpl")]
    readers.push(Box::new(crate::formats::gpl::mias::FeiSerReader::new()));
    #[cfg(feature = "gpl")]
    readers.push(Box::new(
        crate::formats::gpl::volocity::NikonNisReader::new(),
    ));
    #[cfg(feature = "gpl")]
    readers.push(Box::new(
        crate::formats::gpl::tiff_wrappers::MolecularDevicesTiffReader::new(),
    ));
    #[cfg(feature = "openslide")]
    readers.push(Box::new(
        crate::formats::openslide_reader::OpenSlideReader::new(),
    ));
    #[cfg(feature = "gpl")]
    readers.push(Box::new(
        crate::formats::gpl::perkinelmer::PhotonDynamicsReader::new(),
    ));
    #[cfg(feature = "gpl")]
    readers.push(Box::new(
        crate::formats::gpl::bruker::MicroCtVffReader::new(),
    ));
}
