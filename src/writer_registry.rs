use std::collections::HashSet;
use std::path::Path;

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;
use crate::common::writer::FormatWriter;

/// Auto-detecting image writer. Choose an output format by file extension.
pub struct ImageWriter {
    inner: Box<dyn FormatWriter>,
    expected_planes: u32,
    expected_plane_len: usize,
    written_planes: HashSet<u32>,
    closed: bool,
}

fn writer_for(path: &Path) -> Option<Box<dyn FormatWriter>> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .map(|n| n.to_ascii_lowercase())
        .unwrap_or_default();
    if matches!(
        name.as_str(),
        n if n.ends_with(".ome.tif")
            || n.ends_with(".ome.tiff")
            || n.ends_with(".ome.tf2")
            || n.ends_with(".ome.tf8")
            || n.ends_with(".ome.btf")
    ) {
        return Some(Box::new(crate::tiff::TiffWriter::new().with_auto_ome_xml()));
    }

    #[allow(unused_mut)]
    let mut writers: Vec<Box<dyn FormatWriter>> = vec![
        Box::new(crate::tiff::TiffWriter::new()),
        // Java writers.txt registers APNGWriter for ".png"; it can write both
        // single-frame PNG/APNG and multi-frame PNG stacks.
        Box::new(crate::formats::extended::ApngWriter::new()),
        Box::new(crate::formats::png::PngWriter::new()),
        Box::new(crate::formats::jpeg::JpegWriter::new()),
        Box::new(crate::formats::bmp::BmpWriter::new()),
        Box::new(crate::formats::raster::TgaWriter::new()),
        Box::new(crate::formats::raster::PnmWriter::new()),
        Box::new(crate::formats::ics::IcsWriter::new()),
        Box::new(crate::formats::mrc::MrcWriter::new()),
        Box::new(crate::formats::fits::FitsWriter::new()),
        Box::new(crate::formats::nrrd::NrrdWriter::new()),
        Box::new(crate::formats::metaimage::MetaImageWriter::new()),
        Box::new(crate::formats::ome_xml::OmeXmlWriter::new()),
        Box::new(crate::formats::dicom::DicomWriter::new()),
        Box::new(crate::formats::avi::AviWriter::new()),
        Box::new(crate::formats::eps::EpsWriter::new()),
        Box::new(crate::formats::v3draw::V3DrawWriter::new()),
        Box::new(crate::formats::cellh5::CellH5Writer::new()),
        Box::new(crate::formats::misc::QtWriter::new()),
    ];
    #[cfg(feature = "jpeg2000-write")]
    writers.push(Box::new(crate::formats::misc::Jpeg2000Writer::new()));
    writers.into_iter().find(|w| w.is_this_type(path))
}

fn unsupported_native_writer_reason(path: &Path) -> Option<String> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "lif" | "nd2" | "czi" => Some(format!(
            "native .{ext} writing is not registered: the local Bio-Formats writer list has no LIF/ND2/CZI writer to translate; use OME-TIFF/TIFF for export"
        )),
        _ => None,
    }
}

fn writer_for_or_error(path: &Path) -> Result<Box<dyn FormatWriter>> {
    writer_for(path).ok_or_else(|| {
        BioFormatsError::UnsupportedFormat(
            unsupported_native_writer_reason(path).unwrap_or_else(|| path.display().to_string()),
        )
    })
}

impl ImageWriter {
    fn expected_plane_count(meta: &ImageMetadata) -> Result<u32> {
        let effective_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        let dimension_planes = meta
            .size_z
            .max(1)
            .checked_mul(effective_c)
            .and_then(|v| v.checked_mul(meta.size_t.max(1)))
            .ok_or_else(|| {
                BioFormatsError::Format("writer metadata plane count overflows u32".into())
            })?;
        let image_count = meta.image_count.max(1);
        if image_count > dimension_planes {
            return Err(BioFormatsError::Format(format!(
                "writer metadata image_count {image_count} exceeds dimensional plane count {dimension_planes}"
            )));
        }
        Ok(dimension_planes)
    }

    fn validate_stack_support(writer: &dyn FormatWriter, meta: &ImageMetadata) -> Result<u32> {
        let expected_planes = Self::expected_plane_count(meta)?;
        if expected_planes > 1 && !writer.can_do_stacks() {
            return Err(BioFormatsError::Format(format!(
                "writer does not support stacks: metadata requires {expected_planes} planes"
            )));
        }
        Ok(expected_planes)
    }

    fn validate_save_plane_count(expected_planes: u32, actual_planes: usize) -> Result<()> {
        if actual_planes != expected_planes as usize {
            return Err(BioFormatsError::Format(format!(
                "writer received {actual_planes} planes, expected {expected_planes}"
            )));
        }
        Ok(())
    }

    fn expected_plane_len(meta: &ImageMetadata) -> Result<usize> {
        crate::common::writer::expected_plane_len(meta)
    }

    fn validate_save_plane_sizes(meta: &ImageMetadata, planes: &[Vec<u8>]) -> Result<usize> {
        let expected_len = Self::expected_plane_len(meta)?;
        for (plane_index, plane) in planes.iter().enumerate() {
            if plane.len() < expected_len {
                return Err(BioFormatsError::Format(format!(
                    "writer plane {plane_index} has {} bytes, expected {expected_len} bytes or more",
                    plane.len()
                )));
            }
        }
        Ok(expected_len)
    }

    /// Write an OME-TIFF file with embedded OME-XML metadata.
    pub fn save_ome_tiff(
        path: &Path,
        meta: &ImageMetadata,
        ome: &crate::common::ome_metadata::OmeMetadata,
        planes: &[Vec<u8>],
    ) -> Result<()> {
        let mut ome = ome.clone();
        ome.populate_pixels(meta, 0)?;
        ome.verify_minimum_populated(meta, 0)?;
        let ome_xml = ome.to_ome_xml(meta);
        let mut w = crate::tiff::TiffWriter::new().with_ome_xml(ome_xml);
        let expected_planes = Self::validate_stack_support(&w, meta)?;
        Self::validate_save_plane_count(expected_planes, planes.len())?;
        let expected_plane_len = Self::validate_save_plane_sizes(meta, planes)?;
        w.set_metadata(meta)?;
        w.set_id(path)?;
        for (i, plane) in planes.iter().enumerate() {
            w.save_bytes(i as u32, &plane[..expected_plane_len])?;
        }
        w.close()
    }

    /// Convenience: write all planes in one call.
    pub fn save(path: &Path, meta: &ImageMetadata, planes: &[Vec<u8>]) -> Result<()> {
        let mut w = writer_for_or_error(path)?;
        let expected_planes = Self::validate_stack_support(w.as_ref(), meta)?;
        Self::validate_save_plane_count(expected_planes, planes.len())?;
        let expected_plane_len = Self::validate_save_plane_sizes(meta, planes)?;
        w.set_metadata(meta)?;
        w.set_id(path)?;
        for (i, plane) in planes.iter().enumerate() {
            w.save_bytes(i as u32, &plane[..expected_plane_len])?;
        }
        w.close()
    }

    /// Lower-level: stream planes manually.
    pub fn open(path: &Path, meta: &ImageMetadata) -> Result<Self> {
        let mut w = writer_for_or_error(path)?;
        let expected_planes = Self::validate_stack_support(w.as_ref(), meta)?;
        let expected_plane_len = Self::expected_plane_len(meta)?;
        w.set_metadata(meta)?;
        w.set_id(path)?;
        Ok(ImageWriter {
            inner: w,
            expected_planes,
            expected_plane_len,
            written_planes: HashSet::new(),
            closed: false,
        })
    }

    pub fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        if self.closed {
            return Err(BioFormatsError::Format("writer already closed".into()));
        }
        if plane_index >= self.expected_planes {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        if self.expected_planes > 1 && !self.inner.can_do_stacks() {
            return Err(BioFormatsError::Format(format!(
                "writer does not support stacks: metadata requires {} planes",
                self.expected_planes
            )));
        }
        if self.written_planes.contains(&plane_index) {
            return Err(BioFormatsError::Format(format!(
                "plane {plane_index} was already written"
            )));
        }
        if data.len() < self.expected_plane_len {
            return Err(BioFormatsError::Format(format!(
                "writer plane {plane_index} has {} bytes, expected {} bytes or more",
                data.len(),
                self.expected_plane_len
            )));
        }
        self.inner
            .save_bytes(plane_index, &data[..self.expected_plane_len])?;
        self.written_planes.insert(plane_index);
        Ok(())
    }

    pub fn close(&mut self) -> Result<()> {
        if self.closed {
            return Err(BioFormatsError::Format("writer already closed".into()));
        }
        if self.written_planes.len() != self.expected_planes as usize {
            return Err(BioFormatsError::Format(format!(
                "writer wrote {} planes, expected {}",
                self.written_planes.len(),
                self.expected_planes
            )));
        }
        self.inner.close()?;
        self.closed = true;
        Ok(())
    }
}
