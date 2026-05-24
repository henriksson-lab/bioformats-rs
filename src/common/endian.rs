use byteorder::{BigEndian, LittleEndian, ReadBytesExt};
use std::io::Read;

use crate::error::BioFormatsError;
use crate::error::Result;

/// Read a u16 with the given endianness.
pub fn read_u16<R: Read>(r: &mut R, little_endian: bool) -> Result<u16> {
    if little_endian {
        r.read_u16::<LittleEndian>().map_err(BioFormatsError::Io)
    } else {
        r.read_u16::<BigEndian>().map_err(BioFormatsError::Io)
    }
}

pub fn read_u32<R: Read>(r: &mut R, little_endian: bool) -> Result<u32> {
    if little_endian {
        r.read_u32::<LittleEndian>().map_err(BioFormatsError::Io)
    } else {
        r.read_u32::<BigEndian>().map_err(BioFormatsError::Io)
    }
}

pub fn read_u64<R: Read>(r: &mut R, little_endian: bool) -> Result<u64> {
    if little_endian {
        r.read_u64::<LittleEndian>().map_err(BioFormatsError::Io)
    } else {
        r.read_u64::<BigEndian>().map_err(BioFormatsError::Io)
    }
}

pub fn read_i16<R: Read>(r: &mut R, little_endian: bool) -> Result<i16> {
    if little_endian {
        r.read_i16::<LittleEndian>().map_err(BioFormatsError::Io)
    } else {
        r.read_i16::<BigEndian>().map_err(BioFormatsError::Io)
    }
}

pub fn read_i32<R: Read>(r: &mut R, little_endian: bool) -> Result<i32> {
    if little_endian {
        r.read_i32::<LittleEndian>().map_err(BioFormatsError::Io)
    } else {
        r.read_i32::<BigEndian>().map_err(BioFormatsError::Io)
    }
}

pub fn read_i64<R: Read>(r: &mut R, little_endian: bool) -> Result<i64> {
    if little_endian {
        r.read_i64::<LittleEndian>().map_err(BioFormatsError::Io)
    } else {
        r.read_i64::<BigEndian>().map_err(BioFormatsError::Io)
    }
}

pub fn read_f32<R: Read>(r: &mut R, little_endian: bool) -> Result<f32> {
    if little_endian {
        r.read_f32::<LittleEndian>().map_err(BioFormatsError::Io)
    } else {
        r.read_f32::<BigEndian>().map_err(BioFormatsError::Io)
    }
}

pub fn read_f64<R: Read>(r: &mut R, little_endian: bool) -> Result<f64> {
    if little_endian {
        r.read_f64::<LittleEndian>().map_err(BioFormatsError::Io)
    } else {
        r.read_f64::<BigEndian>().map_err(BioFormatsError::Io)
    }
}

/// Convert a byte slice to u16 array with given endianness.
pub fn bytes_to_u16_vec(data: &[u8], little_endian: bool) -> Vec<u16> {
    data.chunks_exact(2)
        .map(|c| {
            let arr = [c[0], c[1]];
            if little_endian {
                u16::from_le_bytes(arr)
            } else {
                u16::from_be_bytes(arr)
            }
        })
        .collect()
}
