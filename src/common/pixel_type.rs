/// The primitive data type of each sample in a pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum PixelType {
    Int8,
    Uint8,
    Int16,
    Uint16,
    Int32,
    Uint32,
    Float32,
    Float64,
    /// 1-bit packed pixels (e.g. bilevel TIFF).
    Bit,
}

impl PixelType {
    /// Size in bytes of one decoded sample.
    ///
    /// `Bit` planes are exposed by readers as one byte per unpacked sample, not
    /// as packed on-disk bits.
    pub fn bytes_per_sample(self) -> usize {
        match self {
            PixelType::Int8 | PixelType::Uint8 => 1,
            PixelType::Int16 | PixelType::Uint16 => 2,
            PixelType::Int32 | PixelType::Uint32 | PixelType::Float32 => 4,
            PixelType::Float64 => 8,
            PixelType::Bit => 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::PixelType;

    #[test]
    fn bit_pixels_have_decoded_sample_width() {
        assert_eq!(PixelType::Bit.bytes_per_sample(), 1);
    }
}
