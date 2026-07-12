use std::path::PathBuf;

/// Whether a level can expose source lossy-compressed blocks.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressedExtractionSupport {
    Supported(CompressedLevelInfo),
    NotSupported { reason: String },
}

/// Metadata for a level that can expose compressed blocks for one logical plane.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedLevelInfo {
    pub plane_index: u32,
    pub level: u32,
    pub width: u64,
    pub height: u64,
    pub tile_width: u32,
    pub tile_height: u32,
    pub tiles_across: u64,
    pub tiles_down: u64,
    pub codec: LossyCodec,
    pub modes: Vec<CompressedTileMode>,
    pub constraints: Vec<CompressedExtractionConstraint>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LossyCodec {
    Jpeg {
        color_space: JpegColorSpace,
        subsampling: Option<JpegSubsampling>,
    },
    Jpeg2000 {
        container: Jpeg2000Container,
    },
    JpegXr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegColorSpace {
    Rgb,
    YCbCr,
    Gray,
    Unknown,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JpegSubsampling {
    Cs444,
    Cs422,
    Cs420,
    Other { horizontal: u16, vertical: u16 },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Jpeg2000Container {
    Codestream,
    Jp2,
    Unknown,
}

/// How the returned compressed bytes relate to the source data.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompressedTileMode {
    /// Exact source bytes already stored by the source format.
    OriginalBytes,
    /// A new JPEG stream produced without pixel-domain decode/recompress.
    DerivedLosslessJpeg,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressedExtractionConstraint {
    RequiresCustomZarrCodec,
    EdgeTilesMayBePartial,
    FragmentedSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CompressedBytes {
    Owned(Vec<u8>),
    FileRange {
        path: PathBuf,
        offset: u64,
        length: u64,
    },
    FileRanges {
        ranges: Vec<CompressedFileRange>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedFileRange {
    pub path: PathBuf,
    pub offset: u64,
    pub length: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CompressedTile {
    pub plane_index: u32,
    pub level: u32,
    pub col: u64,
    pub row: u64,
    pub origin_x: u64,
    pub origin_y: u64,
    pub width: u32,
    pub height: u32,
    pub nominal_tile_width: u32,
    pub nominal_tile_height: u32,
    pub codec: LossyCodec,
    pub mode: CompressedTileMode,
    pub bytes: CompressedBytes,
}

pub(crate) fn mode_allowed(
    preferred_modes: &[CompressedTileMode],
    mode: CompressedTileMode,
) -> bool {
    preferred_modes.is_empty() || preferred_modes.contains(&mode)
}
