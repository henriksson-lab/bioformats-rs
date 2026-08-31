use crate::common::writer::FormatWriter;

const JAVA_WRITERS_TXT: &str = include_str!("writers.txt");

pub(crate) fn all_writers() -> Vec<Box<dyn FormatWriter>> {
    let mut writers: Vec<Box<dyn FormatWriter>> = Vec::new();
    for class in java_writer_class_names() {
        if let Some(writer) = java_writer_for_class(class) {
            writers.push(writer);
        }
        if class == "loci.formats.out.APNGWriter" {
            // Java has one PNG writer slot here. Rust keeps a direct still-PNG
            // writer too, but APNGWriter remains first for `.png` dispatch.
            writers.push(Box::new(crate::formats::png::PngWriter::new()));
        }
    }
    append_rust_extra_writers(&mut writers);
    writers
}

fn java_writer_class_names() -> impl Iterator<Item = &'static str> {
    JAVA_WRITERS_TXT.lines().filter_map(|line| {
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

fn java_writer_for_class(class: &str) -> Option<Box<dyn FormatWriter>> {
    Some(match class {
        "loci.formats.out.OMEXMLWriter" => {
            Box::new(crate::formats::bsd::ome_xml::OmeXmlWriter::new())
        }
        "loci.formats.out.PyramidOMETiffWriter" => {
            Box::new(crate::tiff::PyramidOmeTiffWriter::new())
        }
        "loci.formats.out.OMETiffWriter" => return None,
        "loci.formats.out.TiffWriter" => Box::new(crate::tiff::TiffWriter::new()),
        "loci.formats.out.JPEGWriter" => Box::new(crate::formats::bsd::jpeg::JpegWriter::new()),
        "loci.formats.out.JPEG2000Writer" => Box::new(crate::formats::misc::Jpeg2000Writer::new()),
        "loci.formats.out.APNGWriter" => Box::new(crate::formats::extended::ApngWriter::new()),
        "loci.formats.out.AVIWriter" => Box::new(crate::formats::bsd::avi::AviWriter::new()),
        "loci.formats.out.QTWriter" => Box::new(crate::formats::misc::QtWriter::new()),
        "loci.formats.out.EPSWriter" => Box::new(crate::formats::bsd::eps::EpsWriter::new()),
        "loci.formats.out.ICSWriter" => Box::new(crate::formats::bsd::ics::IcsWriter::new()),
        "loci.formats.out.JavaWriter" => Box::new(crate::formats::java_writer::JavaWriter::new()),
        "loci.formats.out.V3DrawWriter" => Box::new(crate::formats::v3draw::V3DrawWriter::new()),
        "loci.formats.out.DicomWriter" => Box::new(crate::formats::bsd::dicom::DicomWriter::new()),
        "loci.formats.out.CellH5Writer" => {
            Box::new(crate::formats::bsd::cellh5::CellH5Writer::new())
        }
        _ => return None,
    })
}

fn append_rust_extra_writers(writers: &mut Vec<Box<dyn FormatWriter>>) {
    writers.push(Box::new(crate::formats::bsd::bmp::BmpWriter::new()));
    writers.push(Box::new(crate::formats::raster::TgaWriter::new()));
    writers.push(Box::new(crate::formats::raster::PnmWriter::new()));
    #[cfg(feature = "gpl")]
    writers.push(Box::new(crate::formats::gpl::mrc::MrcWriter::new()));
    writers.push(Box::new(crate::formats::bsd::fits::FitsWriter::new()));
    writers.push(Box::new(crate::formats::bsd::nrrd::NrrdWriter::new()));
    writers.push(Box::new(crate::formats::metaimage::MetaImageWriter::new()));
}
