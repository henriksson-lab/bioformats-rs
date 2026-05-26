use crate::common::error::{BioFormatsError, Result};
use std::io::{Cursor as IoCursor, Read, Seek};

use super::ifd::{Ifd, IfdValue};
use super::parser::TiffParser;

/// Nikon maker-note tag containing NEF compression parameters.
pub(crate) const MAKER_NOTE_COMPRESSION_TAG: u16 = 150;
pub(crate) const EXIF_IFD_TAG: u16 = 34665;
pub(crate) const EXIF_MAKER_NOTE_TAG: u16 = 37500;

/// Parameters required by the Nikon NEF compression 34713 decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct NikonCompressionOptions {
    pub lossless: bool,
    pub v_predictor: [i32; 4],
    pub curve: Vec<i32>,
    pub split: i32,
}

const LOSSY_DECODER_CONFIGURATION_12: &[u8] = &[
    0, 1, 5, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 5, 4, 3, 6, 2, 7, 1, 0, 8, 9, 11, 10, 12,
];
const SPLIT_LOSSY_DECODER_CONFIGURATION_12: &[u8] = &[
    0, 1, 5, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 0x39, 0x5a, 0x38, 0x27, 0x16, 5, 4, 3, 2, 1, 0,
    11, 12, 12,
];
const LOSSLESS_DECODER_CONFIGURATION_12: &[u8] = &[
    0, 1, 4, 2, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 0, 5, 4, 6, 3, 7, 2, 8, 1, 9, 0, 10, 11, 12,
];
const LOSSY_DECODER_CONFIGURATION_14: &[u8] = &[
    0, 1, 4, 3, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 0, 5, 6, 4, 7, 8, 3, 9, 2, 1, 0, 10, 11, 12, 13,
    14,
];
const SPLIT_LOSSY_DECODER_CONFIGURATION_14: &[u8] = &[
    0, 1, 5, 1, 1, 1, 1, 1, 1, 1, 2, 0, 0, 0, 0, 0, 8, 0x5c, 0x4b, 0x3a, 0x29, 7, 6, 5, 4, 3, 2, 1,
    0, 13, 14,
];
const LOSSLESS_DECODER_CONFIGURATION_14: &[u8] = &[
    0, 1, 4, 2, 2, 3, 1, 2, 0, 0, 0, 0, 0, 0, 0, 0, 7, 6, 8, 5, 9, 4, 10, 3, 11, 12, 2, 0, 1, 13,
    14,
];

/// Parse the raw value of Nikon maker-note IFD tag 150.
///
/// Bio-Formats extracts this byte array from the nested Nikon maker-note IFD
/// before building `NikonCodecOptions`. This parser only recovers the options;
/// it does not decode compression 34713 pixel data.
pub(crate) fn parse_maker_note_compression_options(
    data: &[u8],
    bits_per_sample: u16,
    little_endian: bool,
) -> Result<NikonCompressionOptions> {
    let mut cursor = Cursor::new(data, little_endian);
    let check1 = cursor.read_u8()?;
    let check2 = cursor.read_u8()?;
    let lossless = check1 == 0x46;

    let mut v_predictor = [0i32; 4];
    for predictor in &mut v_predictor {
        *predictor = cursor.read_u16()? as i32;
    }

    let mut curve = vec![0i32; 16_385];
    let max = (1usize)
        .checked_shl(bits_per_sample as u32)
        .unwrap_or(0)
        .min(0x7fff);
    let csize = cursor.read_u16()? as usize;
    let step = if csize > 1 { max / (csize - 1) } else { 0 };

    if check1 == 0x44 && check2 == 0x20 && step > 0 {
        for i in 0..csize {
            let index = i * step;
            if index < curve.len() {
                curve[index] = cursor.read_u16()? as i32;
            } else {
                let _ = cursor.read_u16()?;
            }
        }
        for i in 0..max.min(curve.len()) {
            let n = i % step;
            let left = i - n;
            let right = (left + step).min(curve.len() - 1);
            curve[i] = (curve[left] * (step - n) as i32 + curve[right] * n as i32) / step as i32;
        }
        cursor.seek(562)?;
        let split = cursor.read_u16()? as i32;
        return Ok(NikonCompressionOptions {
            lossless,
            v_predictor,
            curve,
            split,
        });
    }

    let max_value = (1i32)
        .checked_shl(bits_per_sample as u32)
        .map(|value| value - 1)
        .unwrap_or(i32::MAX);
    curve.fill(max_value);
    let n_elements = cursor.remaining() / 2;
    if n_elements < 100 {
        for (i, value) in curve.iter_mut().enumerate() {
            *value = i as i32;
        }
    } else {
        for value in curve.iter_mut().take(n_elements) {
            *value = cursor.read_u16()? as i32;
        }
    }

    Ok(NikonCompressionOptions {
        lossless,
        v_predictor,
        curve,
        split: -1,
    })
}

/// Extract Nikon compression 34713 options from EXIF MakerNote metadata.
///
/// Nikon NEF stores these options in TIFF EXIF tag 37500 (MakerNote), which
/// wraps a Nikon-local IFD. Bio-Formats skips the 10-byte `Nikon...` prefix
/// before reading the nested IFD, then pulls tag 150 from that IFD.
pub(crate) fn extract_compression_options<R: Read + Seek>(
    parser: &mut TiffParser<R>,
    main_ifds: &[Ifd],
    bits_per_sample: u16,
) -> Result<Option<NikonCompressionOptions>> {
    let little_endian = parser.little_endian;
    for ifd in main_ifds {
        let Some(exif_offset) = ifd.get_u64(EXIF_IFD_TAG) else {
            continue;
        };
        if exif_offset == 0 {
            continue;
        }

        let (exif_ifd, _) = parser.read_ifd(exif_offset)?;
        let Some(maker_note) = exif_ifd.get(EXIF_MAKER_NOTE_TAG).and_then(ifd_value_bytes) else {
            continue;
        };
        let Some(note_ifd) = parse_maker_note_ifd(maker_note, little_endian)? else {
            continue;
        };
        let Some(tag_150) = note_ifd
            .get(MAKER_NOTE_COMPRESSION_TAG)
            .and_then(ifd_value_bytes)
        else {
            continue;
        };
        return parse_maker_note_compression_options(tag_150, bits_per_sample, false).map(Some);
    }
    Ok(None)
}

/// Decode Nikon TIFF compression 34713 using the maker-note options parsed
/// from tag 150. The entropy decoder follows Bio-Formats' NikonCodec/HuffmanCodec.
pub(crate) fn decompress_nikon(
    data: &[u8],
    width: u32,
    height: u32,
    bits_per_sample: u16,
    options: &NikonCompressionOptions,
) -> Result<Vec<u8>> {
    if !matches!(bits_per_sample, 12 | 14) {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "Nikon NEF compression 34713 supports 12- or 14-bit samples, got {bits_per_sample}"
        )));
    }
    if width == 0 || height == 0 {
        return Ok(Vec::new());
    }

    let mut bits = BitReader::new(data);
    let mut out =
        BitWriter::new((width as usize * height as usize * bits_per_sample as usize).div_ceil(8));
    let mut h_predictor = [0i32; 2];
    let mut v_predictor = options.v_predictor;
    let mut table = nikon_huffman_table(options.lossless, bits_per_sample, false);
    let split_table =
        (!options.lossless).then(|| nikon_huffman_table(false, bits_per_sample, true));
    let curve = if options.curve.is_empty() {
        None
    } else {
        Some(options.curve.as_slice())
    };

    for row in 0..height {
        if options.split >= 0 && row as i32 == options.split {
            if let Some(split_table) = split_table.as_ref() {
                table = split_table.clone();
                h_predictor = [0, 0];
            }
        }
        for col in 0..width {
            let cfa_index = (2 * (row as usize & 1)) + (col as usize & 1);
            let diff = decode_nikon_huffman_sample(&mut bits, &table)?;
            let predictor_index = col as usize & 1;
            if col < 2 {
                v_predictor[cfa_index] += diff;
                h_predictor[predictor_index] = v_predictor[cfa_index];
            } else {
                h_predictor[predictor_index] += diff;
            }

            let mut index = h_predictor[predictor_index].max(0) as usize;
            if let Some(curve) = curve {
                if index >= curve.len() {
                    index = curve.len() - 1;
                }
                out.write(curve[index] as u32, bits_per_sample);
            } else {
                out.write(index as u32, bits_per_sample);
            }
        }
    }

    Ok(out.into_bytes())
}

fn parse_maker_note_ifd(data: &[u8], little_endian: bool) -> Result<Option<Ifd>> {
    let nested = if data.len() >= 10 && data.starts_with(b"Nikon") {
        &data[10..]
    } else {
        data
    };

    let mut parser = match TiffParser::new(IoCursor::new(nested)) {
        Ok(parser) => parser,
        Err(_) => return Ok(None),
    };
    if parser.little_endian != little_endian {
        return Ok(None);
    }
    parser
        .read_ifd(parser.first_ifd_offset)
        .map(|(ifd, _)| Some(ifd))
}

fn ifd_value_bytes(value: &IfdValue) -> Option<&[u8]> {
    match value {
        IfdValue::Byte(bytes) | IfdValue::Undefined(bytes) => Some(bytes),
        _ => None,
    }
}

fn nikon_huffman_table(lossless: bool, bits_per_sample: u16, split: bool) -> HuffmanDecoder {
    let config = match (lossless, bits_per_sample, split) {
        (false, 12, false) => LOSSY_DECODER_CONFIGURATION_12,
        (false, 12, true) => SPLIT_LOSSY_DECODER_CONFIGURATION_12,
        (true, 12, false | true) => LOSSLESS_DECODER_CONFIGURATION_12,
        (false, 14, false) => LOSSY_DECODER_CONFIGURATION_14,
        (false, 14, true) => SPLIT_LOSSY_DECODER_CONFIGURATION_14,
        (true, 14, false | true) => LOSSLESS_DECODER_CONFIGURATION_14,
        _ => unreachable!("validated Nikon bit depth"),
    };
    HuffmanDecoder::from_config(config)
}

fn decode_nikon_huffman_sample(bits: &mut BitReader<'_>, decoder: &HuffmanDecoder) -> Result<i32> {
    let len = decoder.decode(bits)?;
    if len == 16 {
        return Ok(32_768);
    }
    let len = len.max(0) as u8;
    let mut value = bits.read_bits(len)?;
    if len > 0 && (value & (1 << (len - 1))) == 0 {
        value -= (1 << len) - 1;
    }
    Ok(value)
}

#[derive(Clone, Debug)]
struct HuffmanDecoder {
    nodes: Vec<HuffmanNode>,
}

#[derive(Clone, Debug)]
struct HuffmanNode {
    branch: [Option<usize>; 2],
    leaf_value: i32,
}

impl HuffmanDecoder {
    fn from_config(config: &[u8]) -> Self {
        let mut decoder = Self {
            nodes: vec![HuffmanNode {
                branch: [None, None],
                leaf_value: -1,
            }],
        };
        let mut leaf_counter = 0usize;
        decoder.create_decoder(0, config, 0, 0, &mut leaf_counter);
        decoder
    }

    fn create_decoder(
        &mut self,
        node: usize,
        config: &[u8],
        offset: usize,
        level: usize,
        leaf_counter: &mut usize,
    ) {
        let mut leaf_total = 0usize;
        let mut i = 0usize;
        while leaf_total <= *leaf_counter && i < 16 && offset + i < config.len() {
            leaf_total += config[offset + i] as usize;
            i += 1;
        }

        if level < i && i <= 16 {
            let zero = self.push_node();
            self.nodes[node].branch[0] = Some(zero);
            self.create_decoder(zero, config, offset, level + 1, leaf_counter);

            let one = self.push_node();
            self.nodes[node].branch[1] = Some(one);
            self.create_decoder(one, config, offset, level + 1, leaf_counter);
        } else {
            let leaf_index = offset + 16 + *leaf_counter;
            *leaf_counter += 1;
            if leaf_index < config.len() {
                self.nodes[node].leaf_value = config[leaf_index] as i32;
            }
        }
    }

    fn push_node(&mut self) -> usize {
        let index = self.nodes.len();
        self.nodes.push(HuffmanNode {
            branch: [None, None],
            leaf_value: -1,
        });
        index
    }

    fn decode(&self, bits: &mut BitReader<'_>) -> Result<i32> {
        let mut node = 0usize;
        while let Some(zero_branch) = self.nodes[node].branch[0] {
            let bit = bits.read_bits(1)? as usize;
            node = if bit == 0 {
                zero_branch
            } else {
                self.nodes[node].branch[1].ok_or_else(|| {
                    BioFormatsError::InvalidData(
                        "Nikon Huffman decoder has a missing branch".into(),
                    )
                })?
            };
        }
        Ok(self.nodes[node].leaf_value)
    }
}

struct BitReader<'a> {
    data: &'a [u8],
    bit_offset: usize,
}

impl<'a> BitReader<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            bit_offset: 0,
        }
    }

    fn read_bits(&mut self, count: u8) -> Result<i32> {
        let mut value = 0i32;
        for _ in 0..count {
            if self.bit_offset >= self.data.len() * 8 {
                return Ok(value);
            }
            let byte = self.data[self.bit_offset / 8];
            let bit = (byte >> (7 - (self.bit_offset % 8))) & 1;
            self.bit_offset += 1;
            value = (value << 1) | bit as i32;
        }
        Ok(value)
    }
}

struct BitWriter {
    buf: Vec<u8>,
    bit_offset: usize,
}

impl BitWriter {
    fn new(capacity: usize) -> Self {
        Self {
            buf: Vec::with_capacity(capacity),
            bit_offset: 0,
        }
    }

    fn write(&mut self, value: u32, count: u16) {
        for shift in (0..count).rev() {
            if self.bit_offset % 8 == 0 {
                self.buf.push(0);
            }
            if ((value >> shift) & 1) != 0 {
                let last = self.buf.len() - 1;
                self.buf[last] |= 1 << (7 - (self.bit_offset % 8));
            }
            self.bit_offset += 1;
        }
    }

    fn into_bytes(self) -> Vec<u8> {
        self.buf
    }
}

struct Cursor<'a> {
    data: &'a [u8],
    offset: usize,
    little_endian: bool,
}

impl<'a> Cursor<'a> {
    fn new(data: &'a [u8], little_endian: bool) -> Self {
        Self {
            data,
            offset: 0,
            little_endian,
        }
    }

    fn remaining(&self) -> usize {
        self.data.len().saturating_sub(self.offset)
    }

    fn seek(&mut self, offset: usize) -> Result<()> {
        if offset > self.data.len() {
            return Err(BioFormatsError::Format(format!(
                "Nikon maker-note tag {MAKER_NOTE_COMPRESSION_TAG} is too short for split offset"
            )));
        }
        self.offset = offset;
        Ok(())
    }

    fn read_u8(&mut self) -> Result<u8> {
        if self.offset >= self.data.len() {
            return Err(BioFormatsError::Format(format!(
                "Nikon maker-note tag {MAKER_NOTE_COMPRESSION_TAG} is truncated"
            )));
        }
        let value = self.data[self.offset];
        self.offset += 1;
        Ok(value)
    }

    fn read_u16(&mut self) -> Result<u16> {
        if self.remaining() < 2 {
            return Err(BioFormatsError::Format(format!(
                "Nikon maker-note tag {MAKER_NOTE_COMPRESSION_TAG} has a truncated short value"
            )));
        }
        let bytes = [self.data[self.offset], self.data[self.offset + 1]];
        self.offset += 2;
        Ok(if self.little_endian {
            u16::from_le_bytes(bytes)
        } else {
            u16::from_be_bytes(bytes)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor as IoCursor;

    fn push_u16_le(data: &mut Vec<u8>, value: u16) {
        data.extend_from_slice(&value.to_le_bytes());
    }

    fn push_u16_be(data: &mut Vec<u8>, value: u16) {
        data.extend_from_slice(&value.to_be_bytes());
    }

    fn push_u32_le(data: &mut Vec<u8>, value: u32) {
        data.extend_from_slice(&value.to_le_bytes());
    }

    fn classic_tiff_with_one_ifd(tag: u16, type_code: u16, count: u32, value: u32) -> Vec<u8> {
        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        push_u16_le(&mut data, 42);
        push_u32_le(&mut data, 8);
        push_u16_le(&mut data, 1);
        push_u16_le(&mut data, tag);
        push_u16_le(&mut data, type_code);
        push_u32_le(&mut data, count);
        push_u32_le(&mut data, value);
        push_u32_le(&mut data, 0);
        data
    }

    fn synthetic_nef_with_maker_note(tag_150: &[u8]) -> Vec<u8> {
        let tag_150_offset = 26;
        let mut nested = classic_tiff_with_one_ifd(
            MAKER_NOTE_COMPRESSION_TAG,
            7,
            tag_150.len() as u32,
            tag_150_offset,
        );
        nested.extend_from_slice(tag_150);

        let mut maker_note = b"Nikon\0\x02\0\0\0".to_vec();
        maker_note.extend_from_slice(&nested);

        let main_ifd_offset = 8;
        let exif_ifd_offset = main_ifd_offset + 18;
        let maker_note_offset = exif_ifd_offset + 18;

        let mut data = Vec::new();
        data.extend_from_slice(b"II");
        push_u16_le(&mut data, 42);
        push_u32_le(&mut data, main_ifd_offset as u32);

        push_u16_le(&mut data, 1);
        push_u16_le(&mut data, EXIF_IFD_TAG);
        push_u16_le(&mut data, 4);
        push_u32_le(&mut data, 1);
        push_u32_le(&mut data, exif_ifd_offset as u32);
        push_u32_le(&mut data, 0);

        push_u16_le(&mut data, 1);
        push_u16_le(&mut data, EXIF_MAKER_NOTE_TAG);
        push_u16_le(&mut data, 7);
        push_u32_le(&mut data, maker_note.len() as u32);
        push_u32_le(&mut data, maker_note_offset as u32);
        push_u32_le(&mut data, 0);

        data.extend_from_slice(&maker_note);
        data
    }

    #[test]
    fn parses_lossless_identity_curve_from_short_tag_150_value() {
        let mut data = vec![0x46, 0x00];
        for value in [10, 20, 30, 40] {
            push_u16_be(&mut data, value);
        }
        push_u16_be(&mut data, 0);

        let options = parse_maker_note_compression_options(&data, 12, false).unwrap();

        assert!(options.lossless);
        assert_eq!(options.v_predictor, [10, 20, 30, 40]);
        assert_eq!(options.split, -1);
        assert_eq!(&options.curve[..5], &[0, 1, 2, 3, 4]);
        assert_eq!(options.curve[4096], 4096);
    }

    #[test]
    fn parses_lossy_interpolated_curve_and_split_from_tag_150_value() {
        let mut data = vec![0x44, 0x20];
        for value in [1, 2, 3, 4] {
            push_u16_be(&mut data, value);
        }
        push_u16_be(&mut data, 5);
        for value in [0, 100, 200, 300, 400] {
            push_u16_be(&mut data, value);
        }
        data.resize(562, 0);
        push_u16_be(&mut data, 1234);

        let options = parse_maker_note_compression_options(&data, 12, false).unwrap();

        assert!(!options.lossless);
        assert_eq!(options.v_predictor, [1, 2, 3, 4]);
        assert_eq!(options.split, 1234);
        assert_eq!(options.curve[0], 0);
        assert_eq!(options.curve[1024], 100);
        assert_eq!(options.curve[512], 50);
    }

    #[test]
    fn reports_truncated_tag_150_values() {
        let err = parse_maker_note_compression_options(&[0x46], 12, true)
            .expect_err("truncated tag 150 should fail");

        assert!(matches!(
            err,
            BioFormatsError::Format(message)
                if message.contains("Nikon maker-note tag 150 is truncated")
        ));
    }

    #[test]
    fn extracts_tag_150_from_exif_maker_note_ifd() {
        let mut tag_150 = vec![0x46, 0x00];
        for value in [11, 22, 33, 44] {
            push_u16_be(&mut tag_150, value);
        }
        push_u16_be(&mut tag_150, 0);

        let data = synthetic_nef_with_maker_note(&tag_150);
        let mut parser = TiffParser::new(IoCursor::new(data)).unwrap();
        let main_ifds = parser.read_ifds().unwrap();

        let options = extract_compression_options(&mut parser, &main_ifds, 12)
            .unwrap()
            .expect("nested Nikon maker-note tag 150 should parse");

        assert!(options.lossless);
        assert_eq!(options.v_predictor, [11, 22, 33, 44]);
        assert_eq!(options.split, -1);
        assert_eq!(&options.curve[..4], &[0, 1, 2, 3]);
    }
}
