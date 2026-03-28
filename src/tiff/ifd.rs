/// TIFF tag IDs — mirrors constants in Java `IFD.java`.
#[allow(dead_code)]
pub mod tag {
    pub const NEW_SUBFILE_TYPE: u16 = 254;
    pub const IMAGE_WIDTH: u16 = 256;
    pub const IMAGE_LENGTH: u16 = 257;
    pub const BITS_PER_SAMPLE: u16 = 258;
    pub const COMPRESSION: u16 = 259;
    pub const PHOTOMETRIC_INTERPRETATION: u16 = 262;
    pub const IMAGE_DESCRIPTION: u16 = 270;
    pub const STRIP_OFFSETS: u16 = 273;
    pub const SAMPLES_PER_PIXEL: u16 = 277;
    pub const ROWS_PER_STRIP: u16 = 278;
    pub const STRIP_BYTE_COUNTS: u16 = 279;
    pub const X_RESOLUTION: u16 = 282;
    pub const Y_RESOLUTION: u16 = 283;
    pub const PLANAR_CONFIGURATION: u16 = 284;
    pub const RESOLUTION_UNIT: u16 = 296;
    pub const SOFTWARE: u16 = 305;
    pub const DATE_TIME: u16 = 306;
    pub const PREDICTOR: u16 = 317;
    pub const COLOR_MAP: u16 = 320;
    pub const TILE_WIDTH: u16 = 322;
    pub const TILE_LENGTH: u16 = 323;
    pub const TILE_OFFSETS: u16 = 324;
    pub const TILE_BYTE_COUNTS: u16 = 325;
    pub const EXTRA_SAMPLES: u16 = 338;
    pub const SAMPLE_FORMAT: u16 = 339;
    pub const JPEG_TABLES: u16 = 347;
    pub const SUB_IFD: u16 = 330;
    pub const YCBCR_SUBSAMPLING: u16 = 530;
}

/// TIFF compression scheme codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compression {
    None,
    Ccitt,
    PackBits,
    Lzw,
    Jpeg,
    JpegNew,
    Deflate,
    DeflateOld,
    Zstd,
    Jpeg2000,
    Unknown(u16),
}

impl From<u16> for Compression {
    fn from(v: u16) -> Self {
        match v {
            1 => Compression::None,
            2 => Compression::Ccitt,
            5 => Compression::Lzw,
            6 => Compression::Jpeg,
            7 => Compression::JpegNew,
            8 => Compression::Deflate,
            32773 => Compression::PackBits,
            32946 => Compression::DeflateOld,
            34712 => Compression::Jpeg2000,
            50000 => Compression::Zstd,
            other => Compression::Unknown(other),
        }
    }
}

/// Photometric interpretation codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Photometric {
    MinIsWhite = 0,
    MinIsBlack = 1,
    Rgb = 2,
    Palette = 3,
    TransparencyMask = 4,
    Cmyk = 5,
    YCbCr = 6,
    CIELab = 8,
    Unknown,
}

impl From<u16> for Photometric {
    fn from(v: u16) -> Self {
        match v {
            0 => Photometric::MinIsWhite,
            1 => Photometric::MinIsBlack,
            2 => Photometric::Rgb,
            3 => Photometric::Palette,
            4 => Photometric::TransparencyMask,
            5 => Photometric::Cmyk,
            6 => Photometric::YCbCr,
            8 => Photometric::CIELab,
            _ => Photometric::Unknown,
        }
    }
}

/// A TIFF tag value — can hold different numeric types or a byte array.
#[derive(Debug, Clone)]
pub enum IfdValue {
    Byte(Vec<u8>),
    Ascii(String),
    Short(Vec<u16>),
    Long(Vec<u32>),
    Long8(Vec<u64>),   // BigTIFF
    Rational(Vec<(u32, u32)>),
    SByte(Vec<i8>),
    Undefined(Vec<u8>),
    SShort(Vec<i16>),
    SLong(Vec<i32>),
    SRational(Vec<(i32, i32)>),
    Float(Vec<f32>),
    Double(Vec<f64>),
    IFD(Vec<u32>),     // IFD offsets stored as LONG
    IFD8(Vec<u64>),    // BigTIFF IFD offsets
}

impl IfdValue {
    pub fn as_u64(&self) -> Option<u64> {
        match self {
            IfdValue::Short(v) if !v.is_empty() => Some(v[0] as u64),
            IfdValue::Long(v) if !v.is_empty() => Some(v[0] as u64),
            IfdValue::Long8(v) if !v.is_empty() => Some(v[0]),
            IfdValue::Byte(v) if !v.is_empty() => Some(v[0] as u64),
            _ => None,
        }
    }

    pub fn as_u32(&self) -> Option<u32> {
        self.as_u64().map(|v| v as u32)
    }

    pub fn as_u16(&self) -> Option<u16> {
        match self {
            IfdValue::Short(v) if !v.is_empty() => Some(v[0]),
            IfdValue::Long(v) if !v.is_empty() => Some(v[0] as u16),
            _ => None,
        }
    }

    pub fn as_vec_u64(&self) -> Vec<u64> {
        match self {
            IfdValue::Short(v) => v.iter().map(|&x| x as u64).collect(),
            IfdValue::Long(v) => v.iter().map(|&x| x as u64).collect(),
            IfdValue::Long8(v) => v.clone(),
            IfdValue::Byte(v) => v.iter().map(|&x| x as u64).collect(),
            IfdValue::IFD(v) => v.iter().map(|&x| x as u64).collect(),
            IfdValue::IFD8(v) => v.clone(),
            _ => vec![],
        }
    }

    pub fn as_vec_u32(&self) -> Vec<u32> {
        self.as_vec_u64().into_iter().map(|v| v as u32).collect()
    }

    pub fn as_vec_u16(&self) -> Vec<u16> {
        match self {
            IfdValue::Short(v) => v.clone(),
            _ => self.as_vec_u64().into_iter().map(|v| v as u16).collect(),
        }
    }

    pub fn as_vec_f32(&self) -> Option<&[f32]> {
        match self {
            IfdValue::Float(v) => Some(v),
            _ => None,
        }
    }

    pub fn as_f64(&self) -> Option<f64> {
        match self {
            IfdValue::Float(v) if !v.is_empty() => Some(v[0] as f64),
            IfdValue::Double(v) if !v.is_empty() => Some(v[0]),
            _ => None,
        }
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            IfdValue::Ascii(s) => Some(s.as_str()),
            _ => None,
        }
    }
}

/// One parsed IFD (Image File Directory).
#[derive(Debug, Clone, Default)]
pub struct Ifd {
    pub entries: std::collections::HashMap<u16, IfdValue>,
}

impl Ifd {
    pub fn get(&self, tag: u16) -> Option<&IfdValue> {
        self.entries.get(&tag)
    }

    pub fn get_u32(&self, tag: u16) -> Option<u32> {
        self.get(tag)?.as_u32()
    }

    pub fn get_u64(&self, tag: u16) -> Option<u64> {
        self.get(tag)?.as_u64()
    }

    pub fn get_u16(&self, tag: u16) -> Option<u16> {
        self.get(tag)?.as_u16()
    }

    pub fn get_vec_u64(&self, tag: u16) -> Vec<u64> {
        self.get(tag).map(|v| v.as_vec_u64()).unwrap_or_default()
    }

    pub fn get_vec_u32(&self, tag: u16) -> Vec<u32> {
        self.get(tag).map(|v| v.as_vec_u32()).unwrap_or_default()
    }

    pub fn get_vec_u16(&self, tag: u16) -> Vec<u16> {
        self.get(tag).map(|v| v.as_vec_u16()).unwrap_or_default()
    }

    pub fn get_str(&self, tag: u16) -> Option<&str> {
        self.get(tag)?.as_str()
    }

    // Convenience accessors for common structural tags

    pub fn image_width(&self) -> Option<u32> {
        self.get_u32(tag::IMAGE_WIDTH)
    }

    pub fn image_length(&self) -> Option<u32> {
        self.get_u32(tag::IMAGE_LENGTH)
    }

    pub fn compression(&self) -> Compression {
        Compression::from(self.get_u16(tag::COMPRESSION).unwrap_or(1))
    }

    pub fn photometric(&self) -> Photometric {
        Photometric::from(self.get_u16(tag::PHOTOMETRIC_INTERPRETATION).unwrap_or(1))
    }

    pub fn samples_per_pixel(&self) -> u16 {
        self.get_u16(tag::SAMPLES_PER_PIXEL).unwrap_or(1)
    }

    pub fn bits_per_sample(&self) -> Vec<u16> {
        let v = self.get_vec_u16(tag::BITS_PER_SAMPLE);
        if v.is_empty() { vec![1] } else { v }
    }

    pub fn planar_configuration(&self) -> u16 {
        self.get_u16(tag::PLANAR_CONFIGURATION).unwrap_or(1)
    }

    pub fn predictor(&self) -> u16 {
        self.get_u16(tag::PREDICTOR).unwrap_or(1)
    }

    pub fn is_tiled(&self) -> bool {
        self.entries.contains_key(&tag::TILE_OFFSETS)
    }

    pub fn tile_width(&self) -> Option<u32> {
        self.get_u32(tag::TILE_WIDTH)
    }

    pub fn tile_length(&self) -> Option<u32> {
        self.get_u32(tag::TILE_LENGTH)
    }
}
