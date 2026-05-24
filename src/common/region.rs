use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::ImageMetadata;

pub fn validate_region(
    format_name: &str,
    image_width: u32,
    image_height: u32,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<()> {
    if w == 0 || h == 0 {
        return Err(BioFormatsError::Format(format!(
            "{format_name} region width and height must be non-zero"
        )));
    }
    let end_x = x.checked_add(w).ok_or_else(|| {
        BioFormatsError::Format(format!("{format_name} region x range overflows"))
    })?;
    let end_y = y.checked_add(h).ok_or_else(|| {
        BioFormatsError::Format(format!("{format_name} region y range overflows"))
    })?;
    if end_x > image_width || end_y > image_height {
        return Err(BioFormatsError::Format(format!(
            "{format_name} region x={x}, y={y}, w={w}, h={h} is outside image bounds {image_width}x{image_height}"
        )));
    }
    Ok(())
}

pub fn crop_full_plane(
    format_name: &str,
    full: &[u8],
    meta: &ImageMetadata,
    samples_per_pixel: usize,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
) -> Result<Vec<u8>> {
    validate_region(format_name, meta.size_x, meta.size_y, x, y, w, h)?;

    let bps = meta.pixel_type.bytes_per_sample();
    let pixel_bytes = samples_per_pixel
        .checked_mul(bps)
        .ok_or_else(|| BioFormatsError::Format(format!("{format_name} pixel size overflows")))?;
    let row_bytes = (meta.size_x as usize)
        .checked_mul(pixel_bytes)
        .ok_or_else(|| BioFormatsError::Format(format!("{format_name} row size overflows")))?;
    let out_row = (w as usize).checked_mul(pixel_bytes).ok_or_else(|| {
        BioFormatsError::Format(format!("{format_name} output row size overflows"))
    })?;
    let expected_len = row_bytes
        .checked_mul(meta.size_y as usize)
        .ok_or_else(|| BioFormatsError::Format(format!("{format_name} plane size overflows")))?;
    if full.len() < expected_len {
        return Err(BioFormatsError::InvalidData(format!(
            "{format_name} plane buffer is too short: got {}, expected at least {expected_len}",
            full.len()
        )));
    }

    let mut out =
        Vec::with_capacity((h as usize).checked_mul(out_row).ok_or_else(|| {
            BioFormatsError::Format(format!("{format_name} output size overflows"))
        })?);
    let start_x = (x as usize).checked_mul(pixel_bytes).ok_or_else(|| {
        BioFormatsError::Format(format!("{format_name} region x offset overflows"))
    })?;
    for row in 0..h as usize {
        let src_row = (y as usize + row).checked_mul(row_bytes).ok_or_else(|| {
            BioFormatsError::Format(format!("{format_name} row offset overflows"))
        })?;
        let start = src_row.checked_add(start_x).ok_or_else(|| {
            BioFormatsError::Format(format!("{format_name} region offset overflows"))
        })?;
        let end = start.checked_add(out_row).ok_or_else(|| {
            BioFormatsError::Format(format!("{format_name} region end overflows"))
        })?;
        out.extend_from_slice(&full[start..end]);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::pixel_type::PixelType;

    fn meta() -> ImageMetadata {
        ImageMetadata {
            size_x: 3,
            size_y: 2,
            pixel_type: PixelType::Uint8,
            ..Default::default()
        }
    }

    #[test]
    fn crop_full_plane_rejects_out_of_bounds_regions() {
        let full = vec![0; 6];
        let meta = meta();
        assert!(crop_full_plane("test", &full, &meta, 1, 3, 0, 1, 1).is_err());
        assert!(crop_full_plane("test", &full, &meta, 1, 2, 0, 2, 1).is_err());
        assert!(crop_full_plane("test", &full, &meta, 1, 0, 1, 1, 2).is_err());
        assert!(crop_full_plane("test", &full, &meta, 1, 0, 0, 0, 1).is_err());
        assert!(crop_full_plane("test", &full, &meta, 1, 0, 0, 1, 0).is_err());
    }

    #[test]
    fn crop_full_plane_rejects_short_buffers() {
        let meta = meta();
        let err = crop_full_plane("test", &[1, 2, 3], &meta, 1, 0, 0, 1, 1).unwrap_err();
        assert!(matches!(err, BioFormatsError::InvalidData(_)));
    }

    #[test]
    fn crop_full_plane_returns_requested_region() {
        let full = vec![1, 2, 3, 4, 5, 6];
        let meta = meta();
        assert_eq!(
            crop_full_plane("test", &full, &meta, 1, 1, 0, 2, 2).unwrap(),
            vec![2, 3, 5, 6]
        );
    }
}
