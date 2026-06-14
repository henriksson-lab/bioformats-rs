//! Vaa3D `.v3draw` raw-image-stack writer.
//!
//! Ported from the upstream Java `V3DrawWriter`. The `.v3draw` format is a
//! 24-byte "raw_image_stack_by_hpeng" signature, a 1-byte endian flag, a 2-byte
//! datatype-size code, and the X/Y/Z/C dimensions, followed by the raw pixel
//! block in XYZC order.

use std::fs::File;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata};
use crate::common::writer::FormatWriter;

/// The 24-byte format signature written at the start of every `.v3draw` file.
const FORMAT_KEY: &[u8] = b"raw_image_stack_by_hpeng";

/// The pixel block always begins at this byte offset in the file:
/// 24 (signature) + 1 (endian flag) + 2 (datatype size) + 4 * 4 (XYZC dims).
const PIXEL_OFFSET: u64 = 43;

/// Output dimension order used by the Vaa3D writer (XYZCT), mirroring the Java
/// `outputOrder` field.
const OUTPUT_ORDER: DimensionOrder = DimensionOrder::XYZCT;

/// Decompose a plane index into (z, c, t) for the given dimension order.
/// Mirrors `loci.formats.FormatTools.getZCTCoords`.
fn get_zct_coords(
    order: DimensionOrder,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    index: u32,
) -> (u32, u32, u32) {
    let (s0, s1) = match order {
        DimensionOrder::XYZCT => (size_z, size_c),
        DimensionOrder::XYZTC => (size_z, size_t),
        DimensionOrder::XYCZT => (size_c, size_z),
        DimensionOrder::XYCTZ => (size_c, size_t),
        DimensionOrder::XYTZC => (size_t, size_z),
        DimensionOrder::XYTCZ => (size_t, size_c),
    };
    let s0 = s0.max(1);
    let s1 = s1.max(1);
    let v0 = index % s0;
    let v1 = (index / s0) % s1;
    let v2 = index / (s0 * s1);
    match order {
        DimensionOrder::XYZCT => (v0, v1, v2),
        DimensionOrder::XYZTC => (v0, v2, v1),
        DimensionOrder::XYCZT => (v1, v0, v2),
        DimensionOrder::XYCTZ => (v2, v0, v1),
        DimensionOrder::XYTZC => (v1, v2, v0),
        DimensionOrder::XYTCZ => (v2, v1, v0),
    }
}

/// Compute the linear plane index for (z, c, t) in the given dimension order.
/// Mirrors `loci.formats.FormatTools.getIndex`.
fn get_index(
    order: DimensionOrder,
    size_z: u32,
    size_c: u32,
    size_t: u32,
    z: u32,
    c: u32,
    t: u32,
) -> u32 {
    let (s0, s1) = match order {
        DimensionOrder::XYZCT => (size_z, size_c),
        DimensionOrder::XYZTC => (size_z, size_t),
        DimensionOrder::XYCZT => (size_c, size_z),
        DimensionOrder::XYCTZ => (size_c, size_t),
        DimensionOrder::XYTZC => (size_t, size_z),
        DimensionOrder::XYTCZ => (size_t, size_c),
    };
    let (v0, v1, v2) = match order {
        DimensionOrder::XYZCT => (z, c, t),
        DimensionOrder::XYZTC => (z, t, c),
        DimensionOrder::XYCZT => (c, z, t),
        DimensionOrder::XYCTZ => (c, t, z),
        DimensionOrder::XYTZC => (t, z, c),
        DimensionOrder::XYTCZ => (t, c, z),
    };
    v0 + v1 * s0 + v2 * s0 * s1
}

/// Writes images in the Vaa3D `.v3draw` ("raw_image_stack_by_hpeng") format.
///
/// Port of the Java `loci.formats.out.V3DrawWriter`. Bio-Formats planes are
/// reordered into the XYZCT volume layout the format expects; planes are
/// buffered by their reordered index and the file is laid out on `close`.
pub struct V3DrawWriter {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    /// Buffered plane bytes indexed by the reordered (XYZCT) plane index.
    planes: Vec<Option<Vec<u8>>>,
}

impl V3DrawWriter {
    pub fn new() -> Self {
        V3DrawWriter {
            path: None,
            meta: None,
            planes: Vec::new(),
        }
    }

    /// Number of samples per pixel (RGB channels packed into a plane).
    fn samples_per_pixel(meta: &ImageMetadata) -> u32 {
        if meta.is_rgb {
            meta.size_c.max(1)
        } else {
            1
        }
    }
}

impl Default for V3DrawWriter {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatWriter for V3DrawWriter {
    fn is_this_type(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("v3draw"))
            .unwrap_or(false)
    }

    fn set_metadata(&mut self, meta: &ImageMetadata) -> Result<()> {
        // The format stores 1/2/4-byte samples only (UINT8, UINT16, FLOAT in
        // Java's getPixelTypes); reject 8-byte FLOAT64 which has no datatype
        // code.
        use crate::common::pixel_type::PixelType;
        match meta.pixel_type {
            PixelType::Uint8
            | PixelType::Int8
            | PixelType::Uint16
            | PixelType::Int16
            | PixelType::Uint32
            | PixelType::Int32
            | PixelType::Float32
            | PixelType::Bit => {}
            PixelType::Float64 => {
                return Err(BioFormatsError::UnsupportedFormat(
                    "V3Draw writer supports at most 4-byte samples, not FLOAT64".into(),
                ));
            }
        }
        self.meta = Some(meta.clone());
        Ok(())
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let meta = self
            .meta
            .as_ref()
            .ok_or_else(|| BioFormatsError::Format("set_metadata first".into()))?;
        // Reordered plane count = sizeZ * sizeC * sizeT (RGB samples are packed
        // within each plane, so they do not add planes).
        let effective_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        let count = meta
            .size_z
            .max(1)
            .checked_mul(effective_c)
            .and_then(|v| v.checked_mul(meta.size_t.max(1)))
            .ok_or_else(|| BioFormatsError::Format("V3Draw writer: plane count overflows".into()))?;
        self.path = Some(path.to_path_buf());
        self.planes = (0..count as usize).map(|_| None).collect();
        Ok(())
    }

    fn save_bytes(&mut self, plane_index: u32, data: &[u8]) -> Result<()> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let size_z = meta.size_z.max(1);
        let size_t = meta.size_t.max(1);
        let size_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        let planes = size_z * size_c * size_t;
        if plane_index >= planes {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }

        // Validate the plane byte count up front (full-plane only; no tiles).
        let spp = Self::samples_per_pixel(meta) as usize;
        let bytes_per_pixel = meta.pixel_type.bytes_per_sample();
        let expected_len = meta.size_x as usize * meta.size_y as usize * spp * bytes_per_pixel;
        if data.len() != expected_len {
            return Err(BioFormatsError::Format(format!(
                "V3Draw writer: plane {plane_index} has {} bytes, expected {expected_len}",
                data.len()
            )));
        }

        // Reorder the incoming plane (input dimension order) to the writer's
        // XYZCT output order, exactly as the Java does with getZCTCoords +
        // getIndex.
        let (z, c, t) =
            get_zct_coords(meta.dimension_order, size_z, size_c, size_t, plane_index);
        let real_index =
            get_index(OUTPUT_ORDER, size_z, size_c, size_t, z, c, t) as usize;

        match self.planes.get_mut(real_index) {
            Some(slot) => *slot = Some(data.to_vec()),
            None => return Err(BioFormatsError::PlaneOutOfRange(plane_index)),
        }
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        // Allow idempotent close when nothing was opened.
        let path = match self.path.take() {
            Some(p) => p,
            None => {
                self.meta = None;
                self.planes.clear();
                return Ok(());
            }
        };
        let meta = self.meta.take().ok_or(BioFormatsError::NotInitialized)?;

        let size_x = meta.size_x;
        let size_y = meta.size_y;
        let size_z = meta.size_z.max(1);
        let size_t = meta.size_t.max(1);
        let size_c = if meta.is_rgb { 1 } else { meta.size_c.max(1) };
        let rgb_channels = Self::samples_per_pixel(&meta);
        let bytes_per_pixel = meta.pixel_type.bytes_per_sample() as u64;
        let bigendian = !meta.is_little_endian;

        // Vaa3D xyzct dimensions: layer = Z*T aggregate, color = C*rgbChannels.
        let sz: [u32; 4] = [
            size_x,
            size_y,
            size_z * size_t,
            size_c * rgb_channels,
        ];

        let plane_size =
            size_x as u64 * size_y as u64 * bytes_per_pixel * rgb_channels as u64;

        let mut f = File::create(&path).map_err(BioFormatsError::Io)?;

        // -- header --
        f.write_all(FORMAT_KEY).map_err(BioFormatsError::Io)?;
        let endian_byte: u8 = if bigendian { b'B' } else { b'L' };
        f.write_all(&[endian_byte]).map_err(BioFormatsError::Io)?;

        // unitSize: bytes-per-pixel as a 2-byte int in the file's endianness.
        let unit = bytes_per_pixel as u16;
        let unit_bytes = if bigendian {
            unit.to_be_bytes()
        } else {
            unit.to_le_bytes()
        };
        f.write_all(&unit_bytes).map_err(BioFormatsError::Io)?;

        // Four dimensions, each a 4-byte int in the file's endianness.
        for d in sz {
            let d_bytes = if bigendian {
                d.to_be_bytes()
            } else {
                d.to_le_bytes()
            };
            f.write_all(&d_bytes).map_err(BioFormatsError::Io)?;
        }

        // -- pixel block: planes laid out by reordered (XYZCT) index --
        for (real_index, slot) in self.planes.iter().enumerate() {
            let buf = slot.as_ref().ok_or_else(|| {
                BioFormatsError::Format(format!(
                    "V3Draw writer: missing plane at output index {real_index}"
                ))
            })?;
            f.seek(SeekFrom::Start(plane_size * real_index as u64 + PIXEL_OFFSET))
                .map_err(BioFormatsError::Io)?;
            f.write_all(buf).map_err(BioFormatsError::Io)?;
        }

        f.flush().map_err(BioFormatsError::Io)?;
        self.planes.clear();
        Ok(())
    }

    fn can_do_stacks(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::common::pixel_type::PixelType;
    use std::fs;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_path(name: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!("bioformats_rs_{name}_{nonce}.v3draw"))
    }

    fn base_meta() -> ImageMetadata {
        ImageMetadata {
            size_x: 2,
            size_y: 3,
            size_z: 2,
            size_c: 1,
            size_t: 1,
            pixel_type: PixelType::Uint8,
            bits_per_pixel: 8,
            image_count: 2,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: Default::default(),
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        }
    }

    fn read_i32_le(d: &[u8], off: usize) -> i32 {
        i32::from_le_bytes([d[off], d[off + 1], d[off + 2], d[off + 3]])
    }
    fn read_u16_le(d: &[u8], off: usize) -> u16 {
        u16::from_le_bytes([d[off], d[off + 1]])
    }

    #[test]
    fn v3draw_round_trip_header_and_pixels() {
        let meta = base_meta();
        let path = temp_path("round_trip");

        let plane0: Vec<u8> = vec![10, 11, 12, 13, 14, 15]; // 2*3 uint8
        let plane1: Vec<u8> = vec![20, 21, 22, 23, 24, 25];

        let mut w = V3DrawWriter::new();
        w.set_metadata(&meta).unwrap();
        w.set_id(&path).unwrap();
        w.save_bytes(0, &plane0).unwrap();
        w.save_bytes(1, &plane1).unwrap();
        w.close().unwrap();

        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        // Signature
        assert_eq!(&bytes[0..24], FORMAT_KEY);
        // Endian flag
        assert_eq!(bytes[24], b'L');
        // Datatype size (1 byte per sample)
        assert_eq!(read_u16_le(&bytes, 25), 1);
        // Dimensions X, Y, Z(=Z*T), C(=C*rgb)
        assert_eq!(read_i32_le(&bytes, 27), 2);
        assert_eq!(read_i32_le(&bytes, 31), 3);
        assert_eq!(read_i32_le(&bytes, 35), 2);
        assert_eq!(read_i32_le(&bytes, 39), 1);
        // Pixel block in XYZC order
        assert_eq!(&bytes[43..49], &plane0[..]);
        assert_eq!(&bytes[49..55], &plane1[..]);
    }

    #[test]
    fn v3draw_uint16_datatype_and_endian() {
        let mut meta = base_meta();
        meta.pixel_type = PixelType::Uint16;
        meta.bits_per_pixel = 16;
        meta.size_z = 1;
        meta.image_count = 1;
        let path = temp_path("uint16");

        // 2*3 uint16 little-endian samples
        let plane: Vec<u8> = (0u16..6).flat_map(|v| v.to_le_bytes()).collect();

        let mut w = V3DrawWriter::new();
        w.set_metadata(&meta).unwrap();
        w.set_id(&path).unwrap();
        w.save_bytes(0, &plane).unwrap();
        w.close().unwrap();

        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        assert_eq!(&bytes[0..24], FORMAT_KEY);
        assert_eq!(bytes[24], b'L');
        assert_eq!(read_u16_le(&bytes, 25), 2); // 2 bytes per sample
        assert_eq!(read_i32_le(&bytes, 35), 1); // Z*T
        assert_eq!(&bytes[43..43 + plane.len()], &plane[..]);
    }

    #[test]
    fn v3draw_reorders_input_planes_to_xyzct() {
        // Input order XYCZT with Z=2, C=2: input plane index must be remapped
        // into the XYZCT output layout before being written.
        let mut meta = base_meta();
        meta.size_z = 2;
        meta.size_c = 2;
        meta.image_count = 4;
        meta.dimension_order = DimensionOrder::XYCZT;
        let path = temp_path("reorder");

        // Tag each plane by its (z,c) so we can verify placement.
        // Input XYCZT order: index = c + z*C  -> (c,z): 0:(0,0) 1:(1,0) 2:(0,1) 3:(1,1)
        let planes: Vec<Vec<u8>> = (0u8..4).map(|i| vec![i; 6]).collect();

        let mut w = V3DrawWriter::new();
        w.set_metadata(&meta).unwrap();
        w.set_id(&path).unwrap();
        for (i, p) in planes.iter().enumerate() {
            w.save_bytes(i as u32, p).unwrap();
        }
        w.close().unwrap();

        let bytes = fs::read(&path).unwrap();
        fs::remove_file(&path).ok();

        // Output XYZCT order: index = z + c*Z. Verify each output slot holds the
        // plane whose (z,c) maps there.
        let z_size = 2u32;
        let plane_bytes = 6usize;
        let get_input_index = |c: u32, z: u32| (c + z * 2) as u8; // XYCZT
        for c in 0..2u32 {
            for z in 0..2u32 {
                let out = (z + c * z_size) as usize;
                let off = 43 + out * plane_bytes;
                let expected = get_input_index(c, z);
                assert_eq!(
                    bytes[off], expected,
                    "output slot z={z} c={c} should hold input plane {expected}"
                );
            }
        }
    }
}
