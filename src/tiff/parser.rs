use std::collections::{HashMap, HashSet};
use std::io::{Read, Seek, SeekFrom};

use crate::common::endian::*;
use crate::common::error::{BioFormatsError, Result};

use super::ifd::{Ifd, IfdValue};

/// Whether the file is standard (32-bit offsets) or BigTIFF (64-bit offsets).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TiffVariant {
    Classic,
    Big,
}

/// Parsed state of the TIFF stream header.
pub struct TiffParser<R: Read + Seek> {
    pub reader: R,
    pub little_endian: bool,
    pub variant: TiffVariant,
    /// Offset of the first IFD.
    pub first_ifd_offset: u64,
    /// Remaining budget (bytes) for out-of-line IFD value arrays across the whole
    /// IFD chain. A malformed chain (e.g. a garbage >4 GB-NDPI offset table) can
    /// otherwise accumulate gigabytes of clamped "rest of file" values; this caps
    /// total parse allocation. Reset at the start of `read_ifds`.
    ifd_value_budget: u64,
}

/// Total bytes allowed for out-of-line IFD value arrays while parsing one IFD
/// chain. Real files use kilobytes-to-low-megabytes; this is a generous backstop.
const IFD_VALUE_BUDGET: u64 = 256 << 20; // 256 MiB

impl<R: Read + Seek> TiffParser<R> {
    /// Parse the TIFF/BigTIFF file header.
    pub fn new(mut reader: R) -> Result<Self> {
        reader.seek(SeekFrom::Start(0))?;
        let mut magic = [0u8; 4];
        reader.read_exact(&mut magic)?;

        let little_endian = match &magic[0..2] {
            b"II" => true,
            b"MM" => false,
            _ => {
                return Err(BioFormatsError::Format(
                    "Not a TIFF file: bad byte-order mark".into(),
                ))
            }
        };

        let bigtiff_magic: u16 = if little_endian {
            u16::from_le_bytes([magic[2], magic[3]])
        } else {
            u16::from_be_bytes([magic[2], magic[3]])
        };

        let (variant, first_ifd_offset) = match bigtiff_magic {
            42 => {
                // Classic TIFF
                let mut off_bytes = [0u8; 4];
                reader.read_exact(&mut off_bytes)?;
                let off = if little_endian {
                    u32::from_le_bytes(off_bytes)
                } else {
                    u32::from_be_bytes(off_bytes)
                };
                (TiffVariant::Classic, off as u64)
            }
            43 => {
                // BigTIFF — 2 extra header fields before IFD offset
                let bytesize = read_u16(&mut reader, little_endian)?;
                if bytesize != 8 {
                    return Err(BioFormatsError::Format(format!(
                        "Invalid BigTIFF offset byte-size {}; expected 8",
                        bytesize
                    )));
                }
                let always_zero = read_u16(&mut reader, little_endian)?;
                if always_zero != 0 {
                    return Err(BioFormatsError::Format(format!(
                        "Invalid BigTIFF reserved field {}; expected 0",
                        always_zero
                    )));
                }
                let off = read_u64(&mut reader, little_endian)?;
                (TiffVariant::Big, off)
            }
            other => {
                return Err(BioFormatsError::Format(format!(
                    "Not a TIFF file: unknown magic {:#06x}",
                    other
                )))
            }
        };

        Ok(TiffParser {
            reader,
            little_endian,
            variant,
            first_ifd_offset,
            ifd_value_budget: IFD_VALUE_BUDGET,
        })
    }

    /// Read all IFDs in the main IFD chain.
    pub fn read_ifds(&mut self) -> Result<Vec<Ifd>> {
        self.ifd_value_budget = IFD_VALUE_BUDGET;
        let mut ifds = Vec::new();
        let mut offset = self.first_ifd_offset;
        let mut visited_offsets = HashSet::new();
        let file_len = self.file_len()?;
        while offset != 0 {
            // A next-IFD pointer at/after EOF means the chain is truncated — this
            // "can easily happen when writing multiple planes" (Java's words).
            // getIFDOffsets stops and keeps the IFDs already read rather than
            // failing, so do the same instead of erroring on a partial final IFD.
            if offset >= file_len {
                break;
            }
            if !visited_offsets.insert(offset) {
                return Err(BioFormatsError::Format(format!(
                    "TIFF IFD chain contains a cycle at offset {}",
                    offset
                )));
            }
            match self.read_ifd(offset) {
                Ok((ifd, next)) => {
                    ifds.push(ifd);
                    offset = next;
                }
                // Tolerate a truncated/corrupt trailing IFD once at least one
                // good IFD has been read (best-effort, matching Java); the first
                // IFD must still parse cleanly.
                Err(_) if !ifds.is_empty() => break,
                Err(e) => return Err(e),
            }
        }
        Ok(ifds)
    }

    /// Read one IFD at `offset`; return the IFD and the offset of the next IFD.
    pub fn read_ifd(&mut self, offset: u64) -> Result<(Ifd, u64)> {
        let file_len = self.file_len()?;
        if offset >= file_len {
            return Err(BioFormatsError::Format(format!(
                "TIFF IFD offset {} is outside file length {}",
                offset, file_len
            )));
        }

        let count_bytes = if self.variant == TiffVariant::Big {
            8u64
        } else {
            2u64
        };
        Self::checked_range_end(offset, count_bytes, file_len, "TIFF IFD entry count")?;

        self.reader.seek(SeekFrom::Start(offset))?;

        let entry_count = if self.variant == TiffVariant::Big {
            read_u64(&mut self.reader, self.little_endian)?
        } else {
            read_u16(&mut self.reader, self.little_endian)? as u64
        };

        let entry_size = if self.variant == TiffVariant::Big {
            20u64
        } else {
            12u64
        };
        let next_ifd_bytes = if self.variant == TiffVariant::Big {
            8u64
        } else {
            4u64
        };
        let entries_bytes = entry_count.checked_mul(entry_size).ok_or_else(|| {
            BioFormatsError::Format("TIFF IFD entry table byte count overflows u64".into())
        })?;
        let ifd_body_bytes = count_bytes
            .checked_add(entries_bytes)
            .and_then(|v| v.checked_add(next_ifd_bytes))
            .ok_or_else(|| BioFormatsError::Format("TIFF IFD byte range overflows u64".into()))?;
        Self::checked_range_end(offset, ifd_body_bytes, file_len, "TIFF IFD entry table")?;
        let entry_count = usize::try_from(entry_count).map_err(|_| {
            BioFormatsError::Format("TIFF IFD entry count does not fit in memory".into())
        })?;

        let mut entries = HashMap::new();

        for _ in 0..entry_count {
            let tag = read_u16(&mut self.reader, self.little_endian)?;
            let type_code = read_u16(&mut self.reader, self.little_endian)?;
            let (count, value_or_offset, value_bytes) = if self.variant == TiffVariant::Big {
                let c = read_u64(&mut self.reader, self.little_endian)?;
                let mut raw = [0u8; 8];
                self.reader.read_exact(&mut raw)?;
                let v = if self.little_endian {
                    u64::from_le_bytes(raw)
                } else {
                    u64::from_be_bytes(raw)
                };
                (c, v, raw.to_vec())
            } else {
                let c = read_u32(&mut self.reader, self.little_endian)? as u64;
                let mut raw = [0u8; 4];
                self.reader.read_exact(&mut raw)?;
                let v = if self.little_endian {
                    u32::from_le_bytes(raw)
                } else {
                    u32::from_be_bytes(raw)
                } as u64;
                (c, v, raw.to_vec())
            };

            if Self::ifd_type_size(type_code).is_none() {
                continue;
            }
            let value = self.read_ifd_value(type_code, count, value_or_offset, &value_bytes)?;
            // Java TiffParser keeps the first occurrence of a duplicate tag
            // (`!ifd.containsKey(tag)` before put). Preserve that tolerance for
            // malformed TIFFs rather than letting later duplicates override
            // core values like dimensions, offsets, or comments.
            entries.entry(tag).or_insert(value);
        }

        // Read next-IFD offset
        let next_ifd = if self.variant == TiffVariant::Big {
            read_u64(&mut self.reader, self.little_endian)?
        } else {
            read_u32(&mut self.reader, self.little_endian)? as u64
        };

        Ok((Ifd { entries }, next_ifd))
    }

    /// Parse the IFD chain of a >4 GB Hamamatsu NDPI file.
    ///
    /// NDPI is a classic (32-bit-field) TIFF, but for files larger than 4 GB it
    /// uses Hamamatsu's "fake BigTIFF" trick (Java `NDPIReader` +
    /// `TiffParser.setUse64BitOffsets`): every file offset is effectively 64-bit.
    /// Concretely each IFD is laid out as
    ///   `count(2) | count*entry(12) | nextIFD(8) | count*highWord(4)`
    /// where the 4-byte inline value/offset field of each entry holds only the
    /// *low* 32 bits and the trailing per-entry `highWord` supplies the high 32
    /// bits (`true = low | (high << 32)`). The header's first-IFD pointer is
    /// likewise a 64-bit value read at byte 4. A naive 32-bit-pointer walk wraps
    /// (mod 2^32) and lands on garbage, so this dedicated walk is required.
    ///
    /// Mirrors `NDPIReader.initStandardMetadata` (per-entry high-word fix-up);
    /// the multi-strip `OFFSET_HIGH_BYTES`/`BYTE_COUNT_HIGH_BYTES` arrays
    /// (Mechanism B) are applied separately by the NDPI reader.
    pub fn read_ifds_ndpi64(&mut self) -> Result<Vec<Ifd>> {
        self.ifd_value_budget = IFD_VALUE_BUDGET;
        let file_len = self.file_len()?;
        // First-IFD pointer is a 64-bit value stored at byte 4 (after II/MM + 42).
        self.reader.seek(SeekFrom::Start(4))?;
        let mut offset = read_u64(&mut self.reader, self.little_endian)?;

        let mut ifds = Vec::new();
        let mut visited = HashSet::new();
        while offset != 0 && offset < file_len {
            if !visited.insert(offset) {
                break;
            }
            let (ifd, next) = self.read_ifd_ndpi64(offset, file_len)?;
            ifds.push(ifd);
            offset = next;
            if ifds.len() > 100_000 {
                break; // runaway guard
            }
        }
        if ifds.is_empty() {
            return Err(BioFormatsError::Format(
                "NDPI 64-bit IFD walk found no IFDs".into(),
            ));
        }
        Ok(ifds)
    }

    /// Read one NDPI 64-bit IFD at `offset`; returns the IFD and the 64-bit
    /// next-IFD offset. See [`read_ifds_ndpi64`](Self::read_ifds_ndpi64).
    fn read_ifd_ndpi64(&mut self, offset: u64, file_len: u64) -> Result<(Ifd, u64)> {
        self.reader.seek(SeekFrom::Start(offset))?;
        let entry_count = read_u16(&mut self.reader, self.little_endian)? as usize;

        // Pass 1: read the raw entry table (low 32 bits only), the 64-bit
        // next-IFD pointer, then the per-entry high-word trailer.
        let mut raw: Vec<(u16, u16, u64, u64)> = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            let tag = read_u16(&mut self.reader, self.little_endian)?;
            let type_code = read_u16(&mut self.reader, self.little_endian)?;
            let count = read_u32(&mut self.reader, self.little_endian)? as u64;
            let low32 = read_u32(&mut self.reader, self.little_endian)? as u64;
            raw.push((tag, type_code, count, low32));
        }
        let next_ifd = read_u64(&mut self.reader, self.little_endian)?;
        let mut high = Vec::with_capacity(entry_count);
        for _ in 0..entry_count {
            high.push(read_u32(&mut self.reader, self.little_endian)? as u64);
        }

        // Pass 2: materialise each entry, combining low + (high << 32).
        let mut entries = HashMap::new();
        for (i, &(tag, type_code, count, low32)) in raw.iter().enumerate() {
            let Some(type_size) = Self::ifd_type_size(type_code) else {
                continue;
            };
            let high_word = high.get(i).copied().unwrap_or(0);
            let total_bytes = count.saturating_mul(type_size);
            let value = if total_bytes <= 4 {
                if high_word > 0 {
                    // An inline scalar offset/value that overflows 32 bits
                    // (e.g. single-strip StripOffsets/StripByteCounts past 4 GB):
                    // the true value is low | (high << 32).
                    IfdValue::Long8(vec![low32 | (high_word << 32)])
                } else {
                    // Ordinary inline value: decode the 4 field bytes in file order.
                    let fb = if self.little_endian {
                        (low32 as u32).to_le_bytes()
                    } else {
                        (low32 as u32).to_be_bytes()
                    };
                    self.read_ifd_value(type_code, count, low32, &fb)?
                }
            } else {
                // Out-of-line array: its storage offset is corrected by the high
                // word, then the array is read from there as usual.
                let corrected = low32 | (high_word << 32);
                if corrected >= file_len {
                    continue;
                }
                self.read_ifd_value(type_code, count, corrected, &[0u8; 4])?
            };
            // Match standard IFD parsing and Java TiffParser: first duplicate
            // tag wins.
            entries.entry(tag).or_insert(value);
        }

        Ok((Ifd { entries }, next_ifd))
    }

    fn file_len(&mut self) -> Result<u64> {
        let pos = self.reader.stream_position()?;
        let len = self.reader.seek(SeekFrom::End(0))?;
        self.reader.seek(SeekFrom::Start(pos))?;
        Ok(len)
    }

    fn checked_range_end(
        offset: u64,
        len: u64,
        file_len: u64,
        context: &'static str,
    ) -> Result<u64> {
        let end = offset
            .checked_add(len)
            .ok_or_else(|| BioFormatsError::Format(format!("{} range overflows u64", context)))?;
        if end > file_len {
            return Err(BioFormatsError::Format(format!(
                "{} range {}..{} exceeds file length {}",
                context, offset, end, file_len
            )));
        }
        Ok(end)
    }

    fn read_ifd_value(
        &mut self,
        type_code: u16,
        count: u64,
        value_or_offset: u64,
        inline_value_bytes: &[u8],
    ) -> Result<IfdValue> {
        let type_size = Self::ifd_type_size(type_code)
            .ok_or_else(|| BioFormatsError::Format(format!("Unknown IFD type {}", type_code)))?;

        let total_bytes = count.checked_mul(type_size).ok_or_else(|| {
            BioFormatsError::Format("TIFF IFD value byte count overflows u64".into())
        })?;

        // Determine if value fits inline or must be read from an offset.
        let inline_limit: u64 = if self.variant == TiffVariant::Big {
            8
        } else {
            4
        };

        let (data, effective_count) = if total_bytes <= inline_limit {
            // Inline values are stored in the IFD entry's value/offset field
            // using the file byte order. Keep those raw bytes; converting the
            // field through an integer first corrupts big-endian SHORT/BYTE
            // values because TIFF stores them left-justified in the field.
            (inline_value_bytes[..total_bytes as usize].to_vec(), count)
        } else {
            // An out-of-range value array is not fatal: Java's TiffParser
            // truncates the element count to what actually fits in the file
            // (count = (fileLen - offset) / bytesPerElement) rather than
            // erroring. This tolerates slightly-truncated files that Java reads.
            //
            // But the clamp is to "rest of file", so a garbage IFD (e.g. from a
            // malformed >4 GB NDPI offset chain, where a bogus tag carries a
            // billion-element count) would clamp to multiple GB and eagerly
            // allocate them. No legitimate TIFF tag value is anywhere near this
            // large, so cap the eager read: real arrays are unaffected, garbage
            // tags get a bounded (harmless) truncated value instead of an OOM.
            const MAX_IFD_VALUE_BYTES: u64 = 64 << 20; // 64 MiB
            let file_len = self.file_len()?;
            let available = file_len.saturating_sub(value_or_offset);
            let usable_bytes =
                (total_bytes.min(available).min(MAX_IFD_VALUE_BYTES) / type_size) * type_size;
            // Charge against the per-chain budget so a malformed IFD chain can't
            // accumulate gigabytes of clamped value arrays during open().
            self.ifd_value_budget =
                self.ifd_value_budget
                    .checked_sub(usable_bytes)
                    .ok_or_else(|| {
                        BioFormatsError::Format(
                    "TIFF IFD value arrays exceed the parse budget (malformed/garbage IFD chain)"
                        .into(),
                )
                    })?;
            let usable_count = usable_bytes / type_size;
            let usable_bytes = usize::try_from(usable_bytes).map_err(|_| {
                BioFormatsError::Format("TIFF IFD value byte count does not fit in memory".into())
            })?;
            let mut buf = vec![0u8; usable_bytes];
            if usable_bytes > 0 && value_or_offset < file_len {
                let pos_after_entry = self.reader.stream_position()?;
                self.reader.seek(SeekFrom::Start(value_or_offset))?;
                self.reader.read_exact(&mut buf)?;
                self.reader.seek(SeekFrom::Start(pos_after_entry))?;
            }
            (buf, usable_count)
        };

        let count = usize::try_from(effective_count)
            .map_err(|_| BioFormatsError::Format("TIFF IFD value count is too large".into()))?;
        self.decode_ifd_value(type_code, count, &data)
    }

    fn ifd_type_size(type_code: u16) -> Option<u64> {
        match type_code {
            1 | 2 | 6 | 7 => Some(1), // BYTE, ASCII, SBYTE, UNDEFINED
            3 | 8 => Some(2),         // SHORT, SSHORT
            4 | 9 | 13 => Some(4),    // LONG, SLONG, IFD
            5 | 10 => Some(8),        // RATIONAL, SRATIONAL
            11 => Some(4),            // FLOAT
            12 => Some(8),            // DOUBLE
            16 | 17 | 18 => Some(8),  // LONG8, SLONG8, IFD8 (BigTIFF)
            _ => None,
        }
    }

    fn decode_ifd_value(&self, type_code: u16, count: usize, data: &[u8]) -> Result<IfdValue> {
        let le = self.little_endian;
        Ok(match type_code {
            1 => IfdValue::Byte(data.to_vec()),
            2 => {
                // ASCII: null-separated strings; take first
                let end = data.iter().position(|&b| b == 0).unwrap_or(data.len());
                IfdValue::Ascii(String::from_utf8_lossy(&data[..end]).into_owned())
            }
            3 => IfdValue::Short(
                data.chunks_exact(2)
                    .map(|c| {
                        if le {
                            u16::from_le_bytes([c[0], c[1]])
                        } else {
                            u16::from_be_bytes([c[0], c[1]])
                        }
                    })
                    .collect(),
            ),
            4 | 13 => IfdValue::Long(
                data.chunks_exact(4)
                    .map(|c| {
                        if le {
                            u32::from_le_bytes([c[0], c[1], c[2], c[3]])
                        } else {
                            u32::from_be_bytes([c[0], c[1], c[2], c[3]])
                        }
                    })
                    .collect(),
            ),
            5 => IfdValue::Rational(
                data.chunks_exact(8)
                    .map(|c| {
                        let n = if le {
                            u32::from_le_bytes([c[0], c[1], c[2], c[3]])
                        } else {
                            u32::from_be_bytes([c[0], c[1], c[2], c[3]])
                        };
                        let d = if le {
                            u32::from_le_bytes([c[4], c[5], c[6], c[7]])
                        } else {
                            u32::from_be_bytes([c[4], c[5], c[6], c[7]])
                        };
                        (n, d)
                    })
                    .collect(),
            ),
            6 => IfdValue::SByte(data.iter().map(|&b| b as i8).collect()),
            7 => IfdValue::Undefined(data.to_vec()),
            8 => IfdValue::SShort(
                data.chunks_exact(2)
                    .map(|c| {
                        if le {
                            i16::from_le_bytes([c[0], c[1]])
                        } else {
                            i16::from_be_bytes([c[0], c[1]])
                        }
                    })
                    .collect(),
            ),
            9 => IfdValue::SLong(
                data.chunks_exact(4)
                    .map(|c| {
                        if le {
                            i32::from_le_bytes([c[0], c[1], c[2], c[3]])
                        } else {
                            i32::from_be_bytes([c[0], c[1], c[2], c[3]])
                        }
                    })
                    .collect(),
            ),
            10 => IfdValue::SRational(
                data.chunks_exact(8)
                    .map(|c| {
                        let n = if le {
                            i32::from_le_bytes([c[0], c[1], c[2], c[3]])
                        } else {
                            i32::from_be_bytes([c[0], c[1], c[2], c[3]])
                        };
                        let d = if le {
                            i32::from_le_bytes([c[4], c[5], c[6], c[7]])
                        } else {
                            i32::from_be_bytes([c[4], c[5], c[6], c[7]])
                        };
                        (n, d)
                    })
                    .collect(),
            ),
            11 => IfdValue::Float(
                data.chunks_exact(4)
                    .map(|c| {
                        f32::from_bits(if le {
                            u32::from_le_bytes([c[0], c[1], c[2], c[3]])
                        } else {
                            u32::from_be_bytes([c[0], c[1], c[2], c[3]])
                        })
                    })
                    .collect(),
            ),
            12 => IfdValue::Double(
                data.chunks_exact(8)
                    .map(|c| {
                        f64::from_bits(if le {
                            u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                        } else {
                            u64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                        })
                    })
                    .collect(),
            ),
            16 | 18 => IfdValue::Long8(
                data.chunks_exact(8)
                    .map(|c| {
                        if le {
                            u64::from_le_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                        } else {
                            u64::from_be_bytes([c[0], c[1], c[2], c[3], c[4], c[5], c[6], c[7]])
                        }
                    })
                    .collect(),
            ),
            _ => {
                let _ = count;
                IfdValue::Undefined(data.to_vec())
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::super::ifd::IfdValue;
    use super::*;
    use std::io::Cursor;

    fn classic_le_header(first_ifd_offset: u32) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&first_ifd_offset.to_le_bytes());
        bytes
    }

    fn bigtiff_le_header(first_ifd_offset: u64) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&43u16.to_le_bytes());
        bytes.extend_from_slice(&8u16.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&first_ifd_offset.to_le_bytes());
        bytes
    }

    fn bigtiff_header(
        little_endian: bool,
        bytesize: u16,
        reserved: u16,
        first_ifd_offset: u64,
    ) -> Vec<u8> {
        let mut bytes = Vec::new();
        if little_endian {
            bytes.extend_from_slice(b"II");
            bytes.extend_from_slice(&43u16.to_le_bytes());
            bytes.extend_from_slice(&bytesize.to_le_bytes());
            bytes.extend_from_slice(&reserved.to_le_bytes());
            bytes.extend_from_slice(&first_ifd_offset.to_le_bytes());
        } else {
            bytes.extend_from_slice(b"MM");
            bytes.extend_from_slice(&43u16.to_be_bytes());
            bytes.extend_from_slice(&bytesize.to_be_bytes());
            bytes.extend_from_slice(&reserved.to_be_bytes());
            bytes.extend_from_slice(&first_ifd_offset.to_be_bytes());
        }
        bytes
    }

    fn short_entry(bytes: &mut Vec<u8>, tag: u16, count: u32, value: u16) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&3u16.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&value.to_le_bytes());
        bytes.extend_from_slice(&0u16.to_le_bytes());
    }

    fn offset_entry(bytes: &mut Vec<u8>, tag: u16, typ: u16, count: u32, offset: u32) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&typ.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
    }

    fn long_entry(bytes: &mut Vec<u8>, tag: u16, value: u32) {
        offset_entry(bytes, tag, 4, 1, value);
    }

    fn big_offset_entry(bytes: &mut Vec<u8>, tag: u16, typ: u16, count: u64, offset: u64) {
        bytes.extend_from_slice(&tag.to_le_bytes());
        bytes.extend_from_slice(&typ.to_le_bytes());
        bytes.extend_from_slice(&count.to_le_bytes());
        bytes.extend_from_slice(&offset.to_le_bytes());
    }

    fn push_u16(bytes: &mut Vec<u8>, value: u16, little_endian: bool) {
        if little_endian {
            bytes.extend_from_slice(&value.to_le_bytes());
        } else {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
    }

    fn push_u64(bytes: &mut Vec<u8>, value: u64, little_endian: bool) {
        if little_endian {
            bytes.extend_from_slice(&value.to_le_bytes());
        } else {
            bytes.extend_from_slice(&value.to_be_bytes());
        }
    }

    fn bigtiff_short_entry(bytes: &mut Vec<u8>, tag: u16, value: u16, little_endian: bool) {
        push_u16(bytes, tag, little_endian);
        push_u16(bytes, 3, little_endian);
        push_u64(bytes, 1, little_endian);
        push_u16(bytes, value, little_endian);
        bytes.extend_from_slice(&[0; 6]);
    }

    fn bigtiff_offset_entry(
        bytes: &mut Vec<u8>,
        tag: u16,
        typ: u16,
        count: u64,
        offset: u64,
        little_endian: bool,
    ) {
        push_u16(bytes, tag, little_endian);
        push_u16(bytes, typ, little_endian);
        push_u64(bytes, count, little_endian);
        push_u64(bytes, offset, little_endian);
    }

    fn minimal_bigtiff_with_inline_and_offset(little_endian: bool) -> Vec<u8> {
        let text = b"offset tag\0";
        let text_offset = 16 + 8 + (2 * 20) + 8;
        let mut bytes = bigtiff_header(little_endian, 8, 0, 16);
        push_u64(&mut bytes, 2, little_endian);
        bigtiff_short_entry(&mut bytes, 256, 0x1234, little_endian);
        bigtiff_offset_entry(
            &mut bytes,
            270,
            2,
            text.len() as u64,
            text_offset,
            little_endian,
        );
        push_u64(&mut bytes, 0, little_endian);
        assert_eq!(bytes.len(), text_offset as usize);
        bytes.extend_from_slice(text);
        bytes
    }

    fn parse(bytes: Vec<u8>) -> TiffParser<Cursor<Vec<u8>>> {
        TiffParser::new(Cursor::new(bytes)).expect("valid TIFF header")
    }

    #[test]
    fn read_ifds_rejects_cyclic_next_ifd() {
        let mut bytes = classic_le_header(8);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        short_entry(&mut bytes, 256, 1, 1);
        bytes.extend_from_slice(&8u32.to_le_bytes());

        let mut parser = parse(bytes);
        let err = parser.read_ifds().expect_err("cycle should fail");
        assert!(err.to_string().contains("cycle"), "unexpected error: {err}");
    }

    #[test]
    fn read_ifd_rejects_oversized_entry_count() {
        let mut bytes = classic_le_header(8);
        bytes.extend_from_slice(&u16::MAX.to_le_bytes());

        let mut parser = parse(bytes);
        let err = parser.read_ifds().expect_err("oversized table should fail");
        assert!(
            err.to_string().contains("entry table"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn read_ifd_truncates_out_of_file_value_range() {
        // Tag 270 (ASCII) declares 16 bytes at offset 26, but only 4 bytes
        // ("abcd") actually exist before EOF. Java's TiffParser truncates the
        // value to what fits rather than erroring, so we must read "abcd".
        let mut bytes = classic_le_header(8);
        bytes.extend_from_slice(&1u16.to_le_bytes()); // entry count
        offset_entry(&mut bytes, 270, 2, 16, 26); // value offset 26 = end of IFD
        bytes.extend_from_slice(&0u32.to_le_bytes()); // next IFD
        assert_eq!(bytes.len(), 26);
        bytes.extend_from_slice(b"abcd");

        let mut parser = parse(bytes);
        let ifds = parser
            .read_ifds()
            .expect("over-long value should be truncated, not rejected");
        assert_eq!(ifds.len(), 1);
        assert!(
            matches!(ifds[0].get(270), Some(IfdValue::Ascii(v)) if v == "abcd"),
            "expected truncated ASCII \"abcd\", got {:?}",
            ifds[0].get(270)
        );
    }

    #[test]
    fn read_ifd_tolerates_value_offset_past_eof() {
        // A value offset entirely past EOF yields an empty value (0 usable
        // elements) rather than an error, matching Java's best-effort parsing.
        let mut bytes = classic_le_header(8);
        bytes.extend_from_slice(&1u16.to_le_bytes());
        offset_entry(&mut bytes, 270, 2, 16, 1_000);
        bytes.extend_from_slice(&0u32.to_le_bytes());

        let mut parser = parse(bytes);
        let ifds = parser
            .read_ifds()
            .expect("out-of-file value should be tolerated");
        assert_eq!(ifds.len(), 1);
        assert!(
            matches!(ifds[0].get(270), Some(IfdValue::Ascii(v)) if v.is_empty()),
            "expected empty ASCII value, got {:?}",
            ifds[0].get(270)
        );
    }

    #[test]
    fn read_ifd_keeps_first_duplicate_tag_value() {
        // Java TiffParser only inserts a tag when the IFD does not already
        // contain it. Later duplicate tags must not overwrite earlier values.
        let mut bytes = classic_le_header(8);
        bytes.extend_from_slice(&2u16.to_le_bytes());
        long_entry(&mut bytes, 256, 1);
        long_entry(&mut bytes, 256, 2);
        bytes.extend_from_slice(&0u32.to_le_bytes());

        let mut parser = parse(bytes);
        let ifds = parser.read_ifds().expect("duplicate-tag IFD should parse");

        assert_eq!(ifds[0].get(256).and_then(IfdValue::as_u32), Some(1));
    }

    #[test]
    fn read_ifds_ndpi64_keeps_first_duplicate_tag_value() {
        // NDPI's fake-64-bit IFD walk has its own materialisation pass; it must
        // match ordinary IFD parsing and Java's first-tag-wins behavior.
        let mut bytes = Vec::new();
        bytes.extend_from_slice(b"II");
        bytes.extend_from_slice(&42u16.to_le_bytes());
        bytes.extend_from_slice(&16u64.to_le_bytes());
        bytes.resize(16, 0);
        bytes.extend_from_slice(&2u16.to_le_bytes());
        long_entry(&mut bytes, 256, 1);
        long_entry(&mut bytes, 256, 2);
        bytes.extend_from_slice(&0u64.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes());

        let mut parser = parse(bytes);
        let ifds = parser
            .read_ifds_ndpi64()
            .expect("duplicate-tag NDPI IFD should parse");

        assert_eq!(ifds[0].get(256).and_then(IfdValue::as_u32), Some(1));
    }

    #[test]
    fn read_ifd_rejects_value_byte_count_overflow() {
        let mut bytes = bigtiff_le_header(16);
        bytes.extend_from_slice(&1u64.to_le_bytes());
        big_offset_entry(&mut bytes, 270, 12, u64::MAX, 40);
        bytes.extend_from_slice(&0u64.to_le_bytes());

        let mut parser = parse(bytes);
        let err = parser.read_ifds().expect_err("huge value should fail");
        assert!(
            err.to_string().contains("overflows"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bigtiff_le_reads_inline_and_offset_values() {
        let mut parser = parse(minimal_bigtiff_with_inline_and_offset(true));
        assert!(parser.little_endian);
        assert_eq!(parser.variant, TiffVariant::Big);

        let ifds = parser.read_ifds().expect("BigTIFF IFD should parse");
        assert_eq!(ifds.len(), 1);
        assert_eq!(ifds[0].get(256).and_then(IfdValue::as_u16), Some(0x1234));
        assert!(matches!(
            ifds[0].get(270),
            Some(IfdValue::Ascii(value)) if value == "offset tag"
        ));
    }

    #[test]
    fn bigtiff_be_reads_inline_and_offset_values() {
        let mut parser = parse(minimal_bigtiff_with_inline_and_offset(false));
        assert!(!parser.little_endian);
        assert_eq!(parser.variant, TiffVariant::Big);

        let ifds = parser.read_ifds().expect("BigTIFF IFD should parse");
        assert_eq!(ifds.len(), 1);
        assert_eq!(ifds[0].get(256).and_then(IfdValue::as_u16), Some(0x1234));
        assert!(matches!(
            ifds[0].get(270),
            Some(IfdValue::Ascii(value)) if value == "offset tag"
        ));
    }

    #[test]
    fn bigtiff_rejects_invalid_offset_byte_size() {
        let err = match TiffParser::new(Cursor::new(bigtiff_header(true, 4, 0, 16))) {
            Ok(_) => panic!("invalid BigTIFF byte-size should fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("byte-size"),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn bigtiff_rejects_nonzero_reserved_field() {
        let err = match TiffParser::new(Cursor::new(bigtiff_header(false, 8, 1, 16))) {
            Ok(_) => panic!("nonzero BigTIFF reserved field should fail"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("reserved"),
            "unexpected error: {err}"
        );
    }
}
