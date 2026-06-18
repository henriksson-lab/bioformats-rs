use crate::error::Result;
use crate::metadata::ImageMetadata;
use std::path::Path;

/// Core trait that every format writer must implement.
///
/// Mirrors `IFormatWriter` from the Java library.
pub trait FormatWriter: Send + Sync {
    /// True if this writer can handle the file path (by extension).
    fn is_this_type(&self, path: &Path) -> bool;

    /// Open the output file and prepare for writing.
    /// Must be called after `set_metadata`.
    fn set_id(&mut self, path: &Path) -> Result<()>;

    /// Flush and close the output file.
    fn close(&mut self) -> Result<()>;

    /// Set the image metadata that describes what will be written.
    /// Must be called before `set_id`.
    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()>;

    /// Write raw pixel bytes for one plane (same layout as `FormatReader::open_bytes`).
    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()>;

    /// True if this writer supports multi-plane (Z/C/T stack) files.
    fn can_do_stacks(&self) -> bool {
        false
    }

    // --- Multi-series support (optional) ---
    fn set_series(&mut self, _series: usize) -> Result<()> {
        Ok(())
    }
    fn series(&self) -> usize {
        0
    }
}

/// Samples stored in a single `save_bytes` plane.
pub fn samples_per_pixel(meta: &ImageMetadata) -> usize {
    if meta.is_rgb {
        meta.size_c.max(1) as usize
    } else {
        1
    }
}

/// Expected full-plane byte count for writer input.
pub fn expected_plane_len(meta: &ImageMetadata) -> Result<usize> {
    if meta.size_x == 0 || meta.size_y == 0 {
        return Err(crate::error::BioFormatsError::InvalidData(
            "writer image dimensions must be positive (non-zero)".into(),
        ));
    }
    let len = meta.size_x as u64
        * meta.size_y as u64
        * samples_per_pixel(meta) as u64
        * meta.pixel_type.bytes_per_sample() as u64;
    usize::try_from(len).map_err(|_| {
        crate::error::BioFormatsError::Format(
            "writer expected plane byte count overflows usize".into(),
        )
    })
}

/// Convert planar RGB/sample planes to interleaved sample order when Java
/// writers would do so via `interleaved == false`. Non-RGB and already
/// interleaved planes are returned unchanged.
pub fn to_interleaved_samples(meta: &ImageMetadata, data: &[u8]) -> Result<Vec<u8>> {
    let expected = expected_plane_len(meta)?;
    if data.len() != expected {
        return Err(crate::error::BioFormatsError::Format(format!(
            "writer plane has {} bytes, expected {expected}",
            data.len()
        )));
    }
    let samples = samples_per_pixel(meta);
    if !meta.is_rgb || meta.is_interleaved || samples <= 1 {
        return Ok(data.to_vec());
    }

    let bytes = meta.pixel_type.bytes_per_sample();
    let pixels = meta.size_x as usize * meta.size_y as usize;
    let mut out = vec![0u8; data.len()];
    for pixel in 0..pixels {
        for sample in 0..samples {
            let src = (sample * pixels + pixel) * bytes;
            let dst = (pixel * samples + sample) * bytes;
            out[dst..dst + bytes].copy_from_slice(&data[src..src + bytes]);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::error::BioFormatsError;

    struct MinimalWriter;

    impl FormatWriter for MinimalWriter {
        fn is_this_type(&self, _path: &Path) -> bool {
            false
        }

        fn set_id(&mut self, _path: &Path) -> Result<()> {
            Err(BioFormatsError::NotInitialized)
        }

        fn close(&mut self) -> Result<()> {
            Ok(())
        }

        fn set_metadata(&mut self, _meta: &ImageMetadata) -> Result<()> {
            Ok(())
        }

        fn save_bytes(&mut self, _plane_index: u32, _data: &[u8]) -> Result<()> {
            Err(BioFormatsError::NotInitialized)
        }
    }

    #[test]
    fn format_writer_default_rejects_stacks_like_java() {
        assert!(!MinimalWriter.can_do_stacks());
    }
}
