//! Hamamatsu DCIMG time-lapse format reader.
//!
//! DCIMG is a proprietary Hamamatsu format for sCMOS camera time-lapse data.
//! The file starts with the 8-byte magic "DCIMG\0\0\0".
//!
//! Faithful port of Bio-Formats' `loci.formats.in.DCIMGReader`. The on-disk
//! header is versioned: version `0x7` (DCIMG_VERSION_0) and `0x1000000`
//! (DCIMG_VERSION_1) are parsed from the offsets used by Bio-Formats; an older
//! synthetic fixture layout is kept on the previous simplified layout for
//! compatibility.
//!
//! Like the Java reader, a DCIMG dataset can span several `.dcimg` files in the
//! same directory: each grouped file contributes one Z slice, while frames
//! within a file are the T axis (dimension order `XYZCT`, `sizeC = 1`).

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

const DCIMG_VERSION_0: u32 = 0x7;
const DCIMG_VERSION_1: u32 = 0x1000000;

const DCIMG_PIXELTYPE_MONO8: u32 = 0x00000001;
const DCIMG_PIXELTYPE_MONO16: u32 = 0x00000002;

fn r_u32_le(b: &[u8], off: usize) -> u32 {
    u32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn r_u64_le(b: &[u8], off: usize) -> u64 {
    u64::from_le_bytes([
        b[off],
        b[off + 1],
        b[off + 2],
        b[off + 3],
        b[off + 4],
        b[off + 5],
        b[off + 6],
        b[off + 7],
    ])
}

fn positive_u32_dim(value: u32, label: &str) -> Result<u32> {
    if value == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "DCIMG {label} must be positive"
        )));
    }
    Ok(value)
}

/// Values a version header parser produces that Java's `parseDCAMVersionXHeader`
/// writes into `CoreMetadata` (sizeT/sizeX/sizeY) and the local `pixelType`
/// field. The remaining parsed fields (dataOffset, bytesPerRow, bytesPerImage,
/// frameFooterSize, offsetToFooter) are stored on `DcimgReader` directly,
/// mirroring how Java's parsers mutate instance fields.
struct DcamHeader {
    header_size: u64,
    n_frames: u32,
    width: u32,
    height: u32,
    pixel_type_code: u32,
}

pub struct DcimgReader {
    /// The currently-open id (Java `currentId`); also the file used to derive the
    /// group directory.
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    version: u32,
    header_size: u64,
    data_offset: u64,
    bytes_per_row: usize,
    bytes_per_image: u64,
    frame_footer_size: u64,
    offset_to_footer: u64,
    /// Whether a four-pixel correction is present (Java
    /// `fourPixelCorrectionInFooter`).
    four_pixel_correction_in_footer: bool,
    /// Absolute file offset of the four-pixel block, only meaningful for
    /// version 0 (Java `offsetToFourPixels`).
    offset_to_four_pixels: u64,
    /// Byte offset within a frame of the four-pixel block (version 0 only, Java
    /// `fourPixelOffsetInFrame`).
    four_pixel_offset_in_frame: u64,
    /// Output row whose first four pixels must be restored (Java
    /// `fourPixelCorrectionLine`).
    four_pixel_correction_line: usize,
    /// Absolute file offset of the four original pixels (Java
    /// `fourPixelCorrectionOffset`).
    four_pixel_correction_offset: u64,
    /// Grouped files, one per Z slice (Java `uniqueFiles`).
    unique_files: Vec<PathBuf>,
}

impl DcimgReader {
    pub fn new() -> Self {
        DcimgReader {
            path: None,
            meta: None,
            version: 0,
            header_size: 0,
            data_offset: 0,
            bytes_per_row: 0,
            bytes_per_image: 0,
            frame_footer_size: 0,
            offset_to_footer: 0,
            four_pixel_correction_in_footer: false,
            offset_to_four_pixels: 0,
            four_pixel_offset_in_frame: 0,
            four_pixel_correction_line: 0,
            four_pixel_correction_offset: 0,
            unique_files: Vec::new(),
        }
    }
}

impl Default for DcimgReader {
    fn default() -> Self {
        Self::new()
    }
}

impl DcimgReader {
    /// Java `DCIMGReader.isThisType(RandomAccessInputStream)` (112-118): the
    /// first five bytes spell "DCIMG".
    fn is_this_type_stream(header: &[u8]) -> bool {
        header.len() >= 5 && &header[..5] == b"DCIMG"
    }

    /// Java `DCIMGReader.parseDCAMVersion1Header` (DCIMGReader.java:322-342).
    ///
    /// Seeks to `headerSize` and reads the version-1 frame-format fields,
    /// storing `dataOffset`/`bytesPerImage`/`frameFooterSize` on `self` and
    /// returning the dimensions / pixel-type that `set_id` copies into the
    /// core metadata. Java reads from `headerSize` via the stream; we already
    /// hold the leading bytes in `hdr`, so the version-1 fields live at
    /// `headerSize + {60, 64, 72, ...}`.
    fn parse_dcam_version1_header(&mut self, hdr: &[u8]) -> Result<DcamHeader> {
        let header_size = self.header_size;
        let header_start = header_size as usize;
        if hdr.len() < header_start + 124 {
            return Err(BioFormatsError::Format(
                "DCIMG version 1 header is truncated".into(),
            ));
        }

        let n_frames = positive_u32_dim(r_u32_le(hdr, header_start + 60), "frame count")?;
        let pixel_type_code = r_u32_le(hdr, header_start + 64);
        let width = positive_u32_dim(r_u32_le(hdr, header_start + 72), "width")?;
        let height = positive_u32_dim(r_u32_le(hdr, header_start + 76), "height")?;
        // Java skips bytesPerRow for version 1 (336); we keep it for stride use.
        let bytes_per_row = r_u32_le(hdr, header_start + 80) as usize;
        let bytes_per_image = r_u32_le(hdr, header_start + 84) as u64;
        let data_offset = header_size + r_u64_le(hdr, header_start + 96);
        let frame_footer_size = r_u32_le(hdr, header_start + 120) as u64;

        self.bytes_per_row = bytes_per_row;
        self.bytes_per_image = bytes_per_image;
        self.data_offset = data_offset;
        self.frame_footer_size = frame_footer_size;
        Ok(DcamHeader {
            header_size,
            n_frames,
            width,
            height,
            pixel_type_code,
        })
    }

    /// Java `DCIMGReader.parseDCAMVersion0Header` (DCIMGReader.java:301-320).
    ///
    /// `headerSize` comes from offset 40 (initFile:196-197); the version-0
    /// fields then live at `headerSize + {32, 36, ...}`:
    ///
    /// ```text
    ///   seek(headerSize); skip 32
    ///   sizeT   = readInt()        @ headerSize + 32
    ///   pixelType = readInt()      @ headerSize + 36
    ///   skip 4
    ///   sizeX   = readInt()        @ headerSize + 44 (num columns)
    ///   bytesPerRow = readUInt()   @ headerSize + 48
    ///   sizeY   = readInt()        @ headerSize + 52 (num rows)
    ///   bytesPerImage = readUInt() @ headerSize + 56
    ///   skip 8
    ///   dataOffset = readInt()     @ headerSize + 68
    ///   offsetToFooter = readLong()@ headerSize + 72
    /// ```
    fn parse_dcam_version0_header(&mut self, f: &mut File, _hdr: &[u8]) -> Result<DcamHeader> {
        let header_size = self.header_size;
        let mut v0 = [0u8; 80];
        f.seek(SeekFrom::Start(header_size))
            .map_err(BioFormatsError::Io)?;
        f.read_exact(&mut v0)
            .map_err(|_| BioFormatsError::Format("DCIMG version 0 header is truncated".into()))?;
        // n_frames is sizeT for version 0 (single-file => size_t = frames).
        let n_frames = positive_u32_dim(r_u32_le(&v0, 32), "frame count")?;
        let pixel_type_code = r_u32_le(&v0, 36);
        let width = positive_u32_dim(r_u32_le(&v0, 44), "width")?;
        let bytes_per_row = r_u32_le(&v0, 48) as usize;
        let height = positive_u32_dim(r_u32_le(&v0, 52), "height")?;
        let bytes_per_image = r_u32_le(&v0, 56) as u64;
        let data_offset = header_size + r_u32_le(&v0, 68) as u64;
        // offsetToFooter = readLong() @ headerSize + 72.
        let offset_to_footer = r_u64_le(&v0, 72);

        self.bytes_per_row = bytes_per_row;
        self.bytes_per_image = bytes_per_image;
        self.data_offset = data_offset;
        self.frame_footer_size = 0;
        self.offset_to_footer = offset_to_footer;
        Ok(DcamHeader {
            header_size,
            n_frames,
            width,
            height,
            pixel_type_code,
        })
    }

    /// Java `DCIMGReader.parseDCAMVersion0Footer` (DCIMGReader.java:344-388).
    ///
    /// Walks the two version-0 footers to locate the separately-stored four
    /// original pixels: validates the footer version, follows the second footer
    /// offset, then reads `offsetToFourPixels`, `fourPixelOffsetInFrame`, and a
    /// `fourPixelSize` whose positivity enables the correction.
    fn parse_dcam_version0_footer(&mut self, f: &mut File) -> Result<()> {
        // Go to the first footer and find out where the second footer is.
        let footer_offset = self.header_size + self.offset_to_footer;
        f.seek(SeekFrom::Start(footer_offset))
            .map_err(BioFormatsError::Io)?;

        let mut buf12 = [0u8; 16];
        f.read_exact(&mut buf12)
            .map_err(|_| BioFormatsError::Format("DCIMG version 0 footer is truncated".into()))?;
        let footer_version = r_u32_le(&buf12, 0);
        if self.version != footer_version {
            return Err(BioFormatsError::Format(format!(
                "Header DCIMG version {} does not match footer version {}.",
                self.version, footer_version
            )));
        }
        // skip 4, then secondFooterOffset = readLong().
        let second_footer_offset = r_u64_le(&buf12, 8);

        // Go to the second footer and get information about the 4px offset.
        f.seek(SeekFrom::Start(footer_offset + second_footer_offset))
            .map_err(BioFormatsError::Io)?;
        // skip 88, offsetToFourPixels = readLong(), skip 4,
        // fourPixelOffsetInFrame = readUInt(), fourPixelSize = readLong().
        let mut tail = [0u8; 88 + 8 + 4 + 4 + 8];
        f.read_exact(&mut tail).map_err(|_| {
            BioFormatsError::Format("DCIMG version 0 second footer is truncated".into())
        })?;
        let offset_to_four_pixels = r_u64_le(&tail, 88);
        let four_pixel_offset_in_frame = r_u32_le(&tail, 88 + 8 + 4) as u64;
        let four_pixel_size = r_u64_le(&tail, 88 + 8 + 4 + 4);
        if four_pixel_size > 0 {
            self.four_pixel_correction_in_footer = true;
        }
        self.offset_to_four_pixels = offset_to_four_pixels;
        self.four_pixel_offset_in_frame = four_pixel_offset_in_frame;
        Ok(())
    }

    /// Java `DCIMGReader.getFourPixelCorrectionLine` (391-415).
    ///
    /// Returns the line whose first four pixels are corrected, and (for
    /// version 1) may flip `four_pixel_correction_in_footer` on.
    fn get_four_pixel_correction_line(&mut self, size_y: usize) -> usize {
        if self.version == DCIMG_VERSION_0 {
            if self.four_pixel_correction_in_footer {
                // TODO (Java): Why do we need the +1?
                return (self.four_pixel_offset_in_frame / self.bytes_per_row.max(1) as u64) as usize
                    + 1;
            } else {
                return size_y.saturating_sub(1);
            }
        }
        if self.version == DCIMG_VERSION_1 {
            if self.frame_footer_size >= 512 || self.frame_footer_size == 32 {
                self.four_pixel_correction_in_footer = true;
            }

            // TODO (Java): This is a guess because the upstream spec resulted in
            // a div-by-zero on the example file.
            if size_y % 2 == 0 {
                return size_y / 2;
            } else {
                return size_y / 2 + 1;
            }
        }
        0
    }

    /// Java `DCIMGReader.getFourPixelCorrectionOffset` (417-423).
    fn get_four_pixel_correction_offset(&self) -> u64 {
        if self.version == DCIMG_VERSION_0 {
            return self.header_size + self.offset_to_footer + self.offset_to_four_pixels;
        }
        // Java: headerSize + dataOffset + bytesPerImage + 12. Our data_offset
        // already includes header_size (added in the header parsers), so we
        // omit the extra header_size add here.
        self.data_offset + self.bytes_per_image + 12
    }

    /// Java `DCIMGReader.scanDirectory` (268-279). Lists the directory, sorts
    /// the names, and offers each to `add_file_to_list`.
    fn scan_directory(&mut self, dir: &Path) -> Result<()> {
        let mut files: Vec<PathBuf> = match std::fs::read_dir(dir) {
            Ok(rd) => rd.filter_map(|e| e.ok().map(|e| e.path())).collect(),
            Err(_) => return Ok(()),
        };
        files.sort();
        for file in files {
            self.add_file_to_list(&file)?;
        }
        Ok(())
    }

    /// Java `DCIMGReader.addFileToList` (282-298). Skips non-`.dcimg` files and
    /// files that are not DCIMG, otherwise appends them to the companion list.
    fn add_file_to_list(&mut self, file: &Path) -> Result<()> {
        let is_dcimg_ext = file
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("dcimg"))
            .unwrap_or(false);
        if !is_dcimg_ext {
            return Ok(());
        }
        let mut stream = match File::open(file) {
            Ok(s) => s,
            Err(_) => return Ok(()),
        };
        let mut magic = [0u8; 5];
        if stream.read_exact(&mut magic).is_err() || !Self::is_this_type_stream(&magic) {
            return Ok(());
        }
        self.unique_files.push(file.to_path_buf());
        Ok(())
    }

    /// Rust-only fallback for older synthetic fixtures (no Java counterpart).
    /// Kept separate from the version-0/1 parsers so they faithfully mirror the
    /// Java helpers; this legacy layout predates the Bio-Formats offsets.
    fn parse_legacy_header(&mut self, hdr: &[u8]) -> Result<DcamHeader> {
        let header_size = r_u32_le(hdr, 16) as u64;
        let n_frames = positive_u32_dim(r_u32_le(hdr, 20), "frame count")?;
        let width = positive_u32_dim(r_u32_le(hdr, 32), "width")?;
        let height = positive_u32_dim(r_u32_le(hdr, 36), "height")?;
        let bit_depth = r_u32_le(hdr, 40);
        let bytes_per_row = r_u32_le(hdr, 48) as usize;

        self.header_size = header_size;
        self.bytes_per_row = bytes_per_row;
        self.bytes_per_image = 0;
        self.data_offset = if header_size > 64 { header_size } else { 64 };
        self.frame_footer_size = 0;
        Ok(DcamHeader {
            header_size,
            n_frames,
            width,
            height,
            pixel_type_code: bit_depth,
        })
    }

    /// Java `getZCTCoords(no)` for the fixed `XYZCT` order with `sizeC == 1`.
    /// Z is the fastest-varying index, so Z selects the grouped file and T
    /// selects the frame within that file.
    fn get_zct_coords(&self, no: u32) -> (usize, usize) {
        let meta = self.meta.as_ref();
        let size_z = meta.map(|m| m.size_z).unwrap_or(1).max(1);
        let zp = no % size_z;
        let tp = no / size_z; // sizeC == 1
        (zp as usize, tp as usize)
    }
}

impl FormatReader for DcimgReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("dcimg"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        Self::is_this_type_stream(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; 512];
        let n = f.read(&mut hdr).map_err(BioFormatsError::Io)?;
        hdr.truncate(n);
        if hdr.len() < 64 {
            return Err(BioFormatsError::Format(
                "DCIMG header is shorter than 64 bytes".into(),
            ));
        }
        if !Self::is_this_type_stream(&hdr) {
            return Err(BioFormatsError::Format("Not a valid DCIMG file.".into()));
        }

        // Java initFile (180-203): version @ 8, headerSize @ 40, fileSize @ 48
        // and a second copy @ 64 must match.
        let version = r_u32_le(&hdr, 8);
        self.version = version;
        if version != DCIMG_VERSION_0 && version < DCIMG_VERSION_1 {
            return Err(BioFormatsError::Format(format!(
                "Unknown DCIMG version number {version}."
            )));
        }
        // Java warns (188-190) when version > DCIMG_VERSION_1; we proceed.

        let header_size = r_u32_le(&hdr, 40) as u64;
        self.header_size = header_size;
        // Java validates fileSize == fileSize2 (199-202). These live at offsets
        // 48 and 64 once the leading bytes (8 magic + 4 version + 28 skip) are
        // accounted for. Only check when the bytes are present.
        if hdr.len() >= 68 {
            let file_size = r_u32_le(&hdr, 48);
            let file_size2 = r_u32_le(&hdr, 64);
            if file_size != file_size2 {
                return Err(BioFormatsError::Format(
                    "Improper header. File sizes do not match.".into(),
                ));
            }
        }

        // Mirror Java initFile (221-226): dispatch to the version-specific
        // header parser, which (like Java's helpers) writes dataOffset /
        // bytesPerRow / bytesPerImage / frameFooterSize onto `self` and returns
        // the dimensions / pixel-type for the core metadata. Version 0 also runs
        // its footer parser to set up the four-pixel correction.
        let DcamHeader {
            header_size,
            n_frames,
            width,
            height,
            pixel_type_code,
        } = if version >= DCIMG_VERSION_1 {
            self.parse_dcam_version1_header(&hdr)?
        } else if version == DCIMG_VERSION_0 {
            let h = self.parse_dcam_version0_header(&mut f, &hdr)?;
            self.parse_dcam_version0_footer(&mut f)?;
            h
        } else {
            self.parse_legacy_header(&hdr)?
        };
        let bytes_per_row = self.bytes_per_row;
        let data_offset = self.data_offset;
        let bytes_per_image = self.bytes_per_image;
        let frame_footer_size = self.frame_footer_size;

        let (pixel_type, bpp): (PixelType, u8) =
            if version >= DCIMG_VERSION_1 || version == DCIMG_VERSION_0 {
                // Java initFile (228-234): MONO8 = 1, MONO16 = 2 for both versions.
                match pixel_type_code {
                    DCIMG_PIXELTYPE_MONO8 => (PixelType::Uint8, 8),
                    DCIMG_PIXELTYPE_MONO16 => (PixelType::Uint16, 16),
                    other => {
                        return Err(BioFormatsError::Format(format!(
                            "Unknown pixel type {other}."
                        )));
                    }
                }
            } else {
                match pixel_type_code {
                    8 => (PixelType::Uint8, 8),
                    16 => (PixelType::Uint16, 16),
                    32 => (PixelType::Float32, 32),
                    other => {
                        return Err(BioFormatsError::UnsupportedFormat(format!(
                            "DCIMG unsupported legacy bit depth {other}"
                        )));
                    }
                }
            };

        let bps = pixel_type.bytes_per_sample();
        let bpr = if bytes_per_row > 0 {
            bytes_per_row
        } else {
            width as usize * bps
        };
        let min_row = (width as usize)
            .checked_mul(bps)
            .ok_or_else(|| BioFormatsError::Format("DCIMG row byte count overflows".into()))?;
        if bpr < min_row {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "DCIMG row stride {bpr} is shorter than declared image row {min_row}"
            )));
        }
        let plane_bytes = (bpr as u64)
            .checked_mul(height as u64)
            .ok_or_else(|| BioFormatsError::Format("DCIMG plane byte count overflows".into()))?;
        let frame_stride = if bytes_per_image > 0 {
            if bytes_per_image < plane_bytes {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "DCIMG frame stride {bytes_per_image} is shorter than declared plane {plane_bytes}"
                )));
            }
            bytes_per_image
                .checked_add(frame_footer_size)
                .ok_or_else(|| BioFormatsError::Format("DCIMG frame stride overflows".into()))?
        } else {
            plane_bytes
        };
        let payload_bytes = frame_stride
            .checked_mul(n_frames as u64)
            .ok_or_else(|| BioFormatsError::Format("DCIMG pixel payload size overflows".into()))?;
        let required_len = data_offset.checked_add(payload_bytes).ok_or_else(|| {
            BioFormatsError::Format("DCIMG pixel payload offset overflows".into())
        })?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        if file_len < required_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "DCIMG pixel payload is shorter than declared: need {required_len} bytes, found {file_len}"
            )));
        }

        // Four-pixel correction setup (Java initFile 238-239 ->
        // getFourPixelCorrectionLine / getFourPixelCorrectionOffset). Must run
        // before the offset getter because the line getter can flip
        // `four_pixel_correction_in_footer` on for version 1.
        self.four_pixel_correction_line = self.get_four_pixel_correction_line(height as usize);
        self.four_pixel_correction_offset = self.get_four_pixel_correction_offset();

        // Java initFile (242-251): if grouping is enabled (the Bio-Formats
        // default), scan the parent directory and gather all DCIMG companions,
        // one per Z slice. Each grouped file is assumed to share this header.
        self.unique_files.clear();
        let parent = path.parent().filter(|p| !p.as_os_str().is_empty());
        match parent {
            Some(dir) => self.scan_directory(dir)?,
            None => self.unique_files.push(path.to_path_buf()),
        }
        if self.unique_files.is_empty() {
            // Fall back to the current file so reads still work even if the
            // directory scan turned up nothing (e.g. permissions).
            self.unique_files.push(path.to_path_buf());
        }

        // Java (252-253): sizeZ = number of grouped files, sizeT = frame count.
        let size_z = self.unique_files.len() as u32;
        let image_count = size_z * n_frames; // sizeZ * sizeT * sizeC (sizeC == 1)

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert(
            "format".into(),
            MetadataValue::String("Hamamatsu DCIMG".into()),
        );
        meta_map.insert("version".into(), MetadataValue::Int(version as i64));
        meta_map.insert("bit_depth".into(), MetadataValue::Int(bpp as i64));
        meta_map.insert("header_size".into(), MetadataValue::Int(header_size as i64));
        meta_map.insert("Version".into(), MetadataValue::Int(version as i64));

        self.meta = Some(ImageMetadata {
            size_x: width,
            size_y: height,
            size_z,
            size_c: 1,
            size_t: n_frames,
            pixel_type,
            bits_per_pixel: bpp,
            image_count,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });

        self.data_offset = data_offset;
        self.bytes_per_row = bpr;
        self.bytes_per_image = bytes_per_image;
        self.frame_footer_size = frame_footer_size;

        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.version = 0;
        self.header_size = 0;
        self.data_offset = 0;
        self.bytes_per_row = 0;
        self.bytes_per_image = 0;
        self.frame_footer_size = 0;
        self.offset_to_footer = 0;
        self.four_pixel_correction_in_footer = false;
        self.offset_to_four_pixels = 0;
        self.four_pixel_offset_in_frame = 0;
        self.four_pixel_correction_line = 0;
        self.four_pixel_correction_offset = 0;
        self.unique_files.clear();
        Ok(())
    }
    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if s == 0 && self.meta.is_some() {
            Ok(())
        } else {
            Err(BioFormatsError::SeriesOutOfRange(s))
        }
    }
    fn series(&self) -> usize {
        0
    }
    fn metadata(&self) -> &ImageMetadata {
        self.meta
            .as_ref()
            .unwrap_or(crate::common::reader::uninitialized_metadata())
    }

    fn open_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        if plane_index >= meta.image_count {
            return Err(BioFormatsError::PlaneOutOfRange(plane_index));
        }
        let bps = meta.pixel_type.bytes_per_sample();
        let size_x = meta.size_x as usize;
        let size_y = meta.size_y as usize;

        // Java openBytes (126-141): Z selects the grouped file, T selects the
        // frame within that file. The per-frame stride includes the per-frame
        // footer for version 1.
        let (zp, tp) = self.get_zct_coords(plane_index);

        let row_bytes = if self.bytes_per_row > 0 {
            self.bytes_per_row
        } else {
            size_x * bps
        };
        let plane_bytes = row_bytes * size_y;
        let frame_stride = if self.bytes_per_image > 0 {
            if self.version >= DCIMG_VERSION_1 {
                self.bytes_per_image + self.frame_footer_size
            } else {
                self.bytes_per_image
            }
        } else {
            plane_bytes as u64
        };
        let offset = self.data_offset + tp as u64 * frame_stride;

        let path = self
            .unique_files
            .get(zp)
            .ok_or(BioFormatsError::PlaneOutOfRange(plane_index))?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut raw = vec![0u8; plane_bytes];
        f.read_exact(&mut raw).map_err(BioFormatsError::Io)?;

        // DCIMG planes are stored bottom-to-top: the first row in the file is
        // the bottom row of the image. Java's DCIMGReader.openBytes (lines
        // 142-162) reflects this by reading with `for (int row=h-1; row>=0;
        // row--)`, writing the first file row into the last buffer row. We
        // mirror that here by emitting source row `r` into destination row
        // `(size_y - 1 - r)`, combining the flip with per-row stride handling.
        let out_row = size_x * bps;
        let mut out = vec![0u8; size_y * out_row];
        for r in 0..size_y {
            let dst = (size_y - 1 - r) * out_row;
            out[dst..dst + out_row].copy_from_slice(&raw[r * row_bytes..r * row_bytes + out_row]);
        }

        // Four-pixel correction (Java openBytes 143-156). DCIMG stashes the
        // original first four pixels of one interior line elsewhere in the file;
        // the in-frame copy of those pixels holds bookkeeping data and must be
        // overwritten. For a full-plane read (x = 0) Java replaces the first
        // four pixels of the buffer row `fourPixelCorrectionLine` with four
        // samples read little-endian from `fourPixelCorrectionOffset`, then
        // reads the remainder of the row from the frame skipping four pixels.
        // Since we always read the full row from the frame above (the in-frame
        // four bookkeeping pixels included), overwriting the first four pixels
        // afterward yields the identical full-plane result. Java compares the
        // *output* (flipped) row index, so we patch `out` after the flip.
        if self.four_pixel_correction_in_footer {
            let line = self.four_pixel_correction_line;
            let n = (4 * bps).min(out_row);
            if line < size_y && n > 0 {
                let mut corner = vec![0u8; n];
                if f.seek(SeekFrom::Start(self.four_pixel_correction_offset))
                    .is_ok()
                    && f.read_exact(&mut corner).is_ok()
                {
                    let dst = line * out_row;
                    out[dst..dst + n].copy_from_slice(&corner);
                }
            }
        }

        Ok(out)
    }

    fn open_bytes_region(
        &mut self,
        plane_index: u32,
        x: u32,
        y: u32,
        w: u32,
        h: u32,
    ) -> Result<Vec<u8>> {
        let full = self.open_bytes(plane_index)?;
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        crop_full_plane("DCIMG", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        let meta = self.meta.as_ref()?;
        let mut ome = crate::common::ome_metadata::OmeMetadata::from_image_metadata(meta);
        let _ = ome.add_original_metadata_annotations(meta, 0);
        Some(ome)
    }
}
