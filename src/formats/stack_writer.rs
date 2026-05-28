use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;

pub(crate) fn expected_plane_len(format_name: &str, meta: &ImageMetadata) -> Result<usize> {
    if meta.size_x == 0 || meta.size_y == 0 {
        return Err(BioFormatsError::InvalidData(format!(
            "{format_name} writer: image dimensions must be positive (non-zero)"
        )));
    }
    let samples_per_pixel = if meta.is_rgb { meta.size_c.max(1) } else { 1 };
    let bytes_per_sample = meta.pixel_type.bytes_per_sample() as u64;
    let len = meta.size_x as u64 * meta.size_y as u64 * samples_per_pixel as u64 * bytes_per_sample;
    usize::try_from(len).map_err(|_| {
        BioFormatsError::Format(format!(
            "{format_name} writer: expected plane byte count overflows usize"
        ))
    })
}

pub(crate) fn expected_plane_count(format_name: &str, meta: &ImageMetadata) -> Result<u32> {
    let effective_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
    let dimension_planes = meta
        .size_z
        .max(1)
        .checked_mul(effective_c)
        .and_then(|v| v.checked_mul(meta.size_t.max(1)))
        .ok_or_else(|| {
            BioFormatsError::Format(format!("{format_name} writer: plane count overflows u32"))
        })?;
    let image_count = meta.image_count.max(1);
    if image_count > dimension_planes {
        return Err(BioFormatsError::Format(format!(
            "{format_name} writer: metadata image_count {image_count} exceeds dimensional plane count {dimension_planes}"
        )));
    }
    Ok(dimension_planes)
}

pub(crate) fn validate_next_plane(
    format_name: &str,
    meta: &ImageMetadata,
    written_planes: usize,
    plane_index: u32,
    data_len: usize,
) -> Result<()> {
    let expected_plane = u32::try_from(written_planes).map_err(|_| {
        BioFormatsError::Format(format!("{format_name} writer: plane index overflows u32"))
    })?;
    if plane_index != expected_plane {
        return Err(BioFormatsError::Format(format!(
            "{format_name} writer: planes must be written in order; expected {expected_plane}, got {plane_index}"
        )));
    }

    let expected_count = expected_plane_count(format_name, meta)?;
    if plane_index >= expected_count {
        return Err(BioFormatsError::PlaneOutOfRange(plane_index));
    }

    let expected_len = expected_plane_len(format_name, meta)?;
    if data_len != expected_len {
        return Err(BioFormatsError::Format(format!(
            "{format_name} writer: plane {plane_index} has {data_len} bytes, expected {expected_len}"
        )));
    }

    Ok(())
}

pub(crate) fn validate_complete(
    format_name: &str,
    meta: &ImageMetadata,
    written_planes: usize,
) -> Result<()> {
    let expected_count = expected_plane_count(format_name, meta)?;
    if written_planes != expected_count as usize {
        return Err(BioFormatsError::Format(format!(
            "{format_name} writer: wrote {written_planes} planes, expected {expected_count}"
        )));
    }
    Ok(())
}
