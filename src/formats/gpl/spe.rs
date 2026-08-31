//! Princeton Instruments SPE format reader.
//!
//! The SPE file has a 4100-byte binary header followed by raw pixel data.
//! Key header fields (offsets from SPEReader.java SpeHeaderEntry, all
//! little-endian): DATATYPE at 108 (short), WIDTH at 42 (short),
//! HEIGHT at 656 (short), NUM_FRAMES at 1446 (int), XML_OFFSET at 678 (long),
//! HEADER_VER at 1992 (int).
//!
//! SPE 3.0 introduced a trailing XML footer at `XML_OFFSET`. Matching the Java
//! reference (SPEReader.initFile), the pixel dimensions are still taken from the
//! binary header for both 2.x and 3.x; the v3 XML footer is detected (via
//! HEADER_VER >= 3 or XML_OFFSET > 0) and exposed in metadata, but Java marks
//! such files as `metadataComplete = false` rather than parsing the XML, so we
//! do the same and additionally surface the raw footer XML string.

use std::collections::HashMap;
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

const HEADER_SIZE: u64 = 4100;

#[derive(Clone, Copy, PartialEq, Eq)]
enum SpeHeaderType {
    Long,
    Int,
    Short,
    Byte,
    String,
    LongArray,
    IntArray,
    ShortArray,
    RoiArray,
}

#[derive(Clone, Copy)]
struct SpeHeaderEntry {
    name: &'static str,
    offset: usize,
    kind: SpeHeaderType,
}

const SPE_HEADER_ENTRIES: &[SpeHeaderEntry] = &[
    SpeHeaderEntry {
        name: "CONTROLLER_VER",
        offset: 0,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "LOGIC_OUTPUT",
        offset: 2,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "AMP_MODE",
        offset: 4,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "X_DIMENSION",
        offset: 6,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "MODE",
        offset: 8,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "EXPOSURE",
        offset: 10,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "VIRTUAL_XDIM",
        offset: 14,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "VIRTUAL_YDIM",
        offset: 16,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "Y_DIMENSION",
        offset: 18,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "DATE",
        offset: 20,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "VIRTUAL_CHIP",
        offset: 30,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "NOSCAN",
        offset: 34,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "DETECTOR_TEMP",
        offset: 36,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "DETECTOR_TYPE",
        offset: 40,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "WIDTH",
        offset: 42,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "TRIGGER_DIODE",
        offset: 44,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "DELAY_TIME",
        offset: 46,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "SHUTTER_CTRL",
        offset: 50,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "ABSORB_LIVE",
        offset: 52,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "ABSORB_MODE",
        offset: 54,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "CAN_VRTL_CHIP",
        offset: 56,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "THRESHOLD_MIN_LIVE",
        offset: 58,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "THRESHOLD_MIN_VAL",
        offset: 60,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "THRESHOLD_MAX_LIVE",
        offset: 64,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "THRESHOLD_MAX_VAL",
        offset: 66,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "AUTO_SPECTRO",
        offset: 70,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "SPEC_CENTER_WAVELEN",
        offset: 72,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "SPEC_GLUE_FLAG",
        offset: 76,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "SPEC_GLUE_START",
        offset: 78,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "SPEC_GLUE_END",
        offset: 82,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "SPEC_GLUE_MIN_OVRLP",
        offset: 86,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "SPEC_GLUE_FINAL_RES",
        offset: 90,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "PULSAR_TYPE",
        offset: 94,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "CHIP_FLAG",
        offset: 96,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "X_PRE_PIXELS",
        offset: 98,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "X_POST_PIXELS",
        offset: 100,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "Y_PRE_PIXELS",
        offset: 102,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "Y_POST_PIXELS",
        offset: 104,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "ASYNCH",
        offset: 106,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "DATATYPE",
        offset: 108,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "PULSER_MODE",
        offset: 110,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "PULSER_CHIP_ACCUMS",
        offset: 112,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "PULSE_REP_EXP",
        offset: 114,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "PULSE_REP_WIDTH",
        offset: 118,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "PULSE_REP_DELAY",
        offset: 122,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "PULSE_START_WIDTH",
        offset: 126,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "PULSE_END_WIDTH",
        offset: 130,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "PULSE_START_DELAY",
        offset: 134,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "PULSE_END_DELAY",
        offset: 138,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "PULSE_INC_MODE",
        offset: 142,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "PI_MAX_USED",
        offset: 144,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "PI_MAX_MODE",
        offset: 146,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "PI_MAX_GAIN",
        offset: 148,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "BCKGRND_SUB",
        offset: 150,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "PI_MAX_2NS_BRD",
        offset: 152,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "MIN_BLK",
        offset: 154,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "NUM_IN_BLK",
        offset: 156,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "SPEC_MIRR_LOC",
        offset: 158,
        kind: SpeHeaderType::ShortArray,
    },
    SpeHeaderEntry {
        name: "SPEC_SLIT_LOC",
        offset: 162,
        kind: SpeHeaderType::ShortArray,
    },
    SpeHeaderEntry {
        name: "CUS_TIMING_FLAG",
        offset: 170,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "EXP_TIME_LOCAL",
        offset: 172,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "EXP_TIME_UTC",
        offset: 179,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "EXPOSURE_UNITS",
        offset: 186,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "ADC_OFFSET",
        offset: 188,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "ADC_RATE",
        offset: 190,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "ADC_TYPE",
        offset: 192,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "ADC_RESOLUTION",
        offset: 194,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "ADC_BIT_ADJUST",
        offset: 196,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "GAIN",
        offset: 198,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "COMMENTS",
        offset: 200,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "GEOMETRIC",
        offset: 600,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "X_LABEL",
        offset: 602,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "CLEANS",
        offset: 618,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "LFLOAT",
        offset: 620,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "SPEC_MIRROR_POS",
        offset: 622,
        kind: SpeHeaderType::ShortArray,
    },
    SpeHeaderEntry {
        name: "SPEC_SLIT_POS",
        offset: 626,
        kind: SpeHeaderType::IntArray,
    },
    SpeHeaderEntry {
        name: "AUTO_CLEAN",
        offset: 642,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "CONT_CLEAN",
        offset: 644,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "ABSORB_STRIP_NUM",
        offset: 646,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "SPEC_SLIT_POS_UNITS",
        offset: 648,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "SPEC_GROOVES",
        offset: 650,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "SOURCE_COMP",
        offset: 654,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "HEIGHT",
        offset: 656,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "SCRAMBLE",
        offset: 658,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "LEXPOS",
        offset: 660,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "EXT_TRIGGER",
        offset: 662,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "LNOSCAN",
        offset: 664,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "ACCUMULATIONS",
        offset: 668,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "READOUT_TIME",
        offset: 672,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "TRIGGER_MODE",
        offset: 676,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "XML_OFFSET",
        offset: 678,
        kind: SpeHeaderType::Long,
    },
    SpeHeaderEntry {
        name: "VERSION",
        offset: 688,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "TYPE",
        offset: 704,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "FLAT_FIELD",
        offset: 706,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "KINETIC_TRIGGER",
        offset: 724,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "DATA_LABEL",
        offset: 726,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "SPARE4",
        offset: 742,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "PULSE_FILENAME",
        offset: 1178,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "ABSORB_FILENAME",
        offset: 1298,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "EXP_REPEATS",
        offset: 1418,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "EXP_ACCUMS",
        offset: 1422,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "YT_FLAG",
        offset: 1426,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "VERT_CLOCK_SPEED",
        offset: 1428,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "HW_ACCUM",
        offset: 1432,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "STORE_SYNC",
        offset: 1434,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "BLEMISH_APPLIED",
        offset: 1436,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "COSMIC_APPLIED",
        offset: 1438,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "COSMIC_TYPE",
        offset: 1440,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "COSMIC_THRESHOLD",
        offset: 1442,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "NUM_FRAMES",
        offset: 1446,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "MAX_INTENSITY",
        offset: 1450,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "MIN_INTENSITY",
        offset: 1454,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "Y_LABEL",
        offset: 1458,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "SHUTTER_TYPE",
        offset: 1474,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "SHUTTER_COMP",
        offset: 1476,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "READOUT_MODE",
        offset: 1480,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "WINDOW_SIZE",
        offset: 1482,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "CLOCK_SPEED",
        offset: 1484,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "INTERFACE_TYPE",
        offset: 1486,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "NUM_EXP_ROIS",
        offset: 1488,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "CONTROLLER_NUM",
        offset: 1506,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "SOFTWARE",
        offset: 1508,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "NUM_ROIS",
        offset: 1510,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "ROI_BEGIN",
        offset: 1512,
        kind: SpeHeaderType::RoiArray,
    },
    SpeHeaderEntry {
        name: "FLAT_FIELD_FILE",
        offset: 1632,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "BACKGROUND_FILE",
        offset: 1752,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "BLEMISH_FILE",
        offset: 1872,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "HEADER_VER",
        offset: 1992,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "YT_INFO",
        offset: 1996,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "WINVIEW_ID",
        offset: 2996,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "X_SCALING_OFFSET",
        offset: 3000,
        kind: SpeHeaderType::Long,
    },
    SpeHeaderEntry {
        name: "X_SCALING_FACTOR",
        offset: 3008,
        kind: SpeHeaderType::Long,
    },
    SpeHeaderEntry {
        name: "X_SCALING_UNIT",
        offset: 3016,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "X_RESERVED",
        offset: 3017,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "X_SPECIAL_STRING",
        offset: 3018,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "X_RESERVED2",
        offset: 3058,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "X_CALIB_VALID",
        offset: 3098,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "X_INPUT_UNIT",
        offset: 3099,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "X_POLYNUM_UNIT",
        offset: 3100,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "X_POLYNUM_ORDER",
        offset: 3101,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "X_CALIB_COUNT",
        offset: 3102,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "X_PIXEL_POSITION",
        offset: 3103,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "X_CALIB_VALUE",
        offset: 3183,
        kind: SpeHeaderType::LongArray,
    },
    SpeHeaderEntry {
        name: "X_POLYNUM_COEFF",
        offset: 3263,
        kind: SpeHeaderType::LongArray,
    },
    SpeHeaderEntry {
        name: "X_LASER_POS",
        offset: 3311,
        kind: SpeHeaderType::LongArray,
    },
    SpeHeaderEntry {
        name: "X_RESERVED3",
        offset: 3319,
        kind: SpeHeaderType::Long,
    },
    SpeHeaderEntry {
        name: "X_CALIB_FLAG",
        offset: 3320,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "X_CALIB_LABEL",
        offset: 3321,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "X_EXPANSION",
        offset: 3402,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "Y_SCALING_OFFSET",
        offset: 3489,
        kind: SpeHeaderType::Long,
    },
    SpeHeaderEntry {
        name: "Y_SCALING_FACTOR",
        offset: 3497,
        kind: SpeHeaderType::Long,
    },
    SpeHeaderEntry {
        name: "Y_SCALING_UNIT",
        offset: 3505,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "Y_RESERVED",
        offset: 3506,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "Y_SPECIAL_STRING",
        offset: 3507,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "Y_RESERVED2",
        offset: 3547,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "Y_CALIB_VALID",
        offset: 3587,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "Y_INPUT_UNIT",
        offset: 3588,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "Y_POLYNUM_UNIT",
        offset: 3589,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "Y_POLYNUM_ORDER",
        offset: 3590,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "Y_CALIB_COUNT",
        offset: 3591,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "Y_PIXEL_POSITION",
        offset: 3592,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "Y_CALIB_VALUE",
        offset: 3672,
        kind: SpeHeaderType::LongArray,
    },
    SpeHeaderEntry {
        name: "Y_POLYNUM_COEFF",
        offset: 3752,
        kind: SpeHeaderType::LongArray,
    },
    SpeHeaderEntry {
        name: "Y_LASER_POS",
        offset: 3800,
        kind: SpeHeaderType::LongArray,
    },
    SpeHeaderEntry {
        name: "Y_RESERVED3",
        offset: 3808,
        kind: SpeHeaderType::Long,
    },
    SpeHeaderEntry {
        name: "Y_CALIB_FLAG",
        offset: 3809,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "Y_CALIB_LABEL",
        offset: 3810,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "Y_EXPANSION",
        offset: 3891,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "INTENSITY_STRING",
        offset: 3978,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "SPARE6",
        offset: 4018,
        kind: SpeHeaderType::String,
    },
    SpeHeaderEntry {
        name: "SPEC_TYPE",
        offset: 4043,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "SPEC_MODEL",
        offset: 4044,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "PULSE_BURST_USED",
        offset: 4045,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "PULSE_BURST_COUNT",
        offset: 4046,
        kind: SpeHeaderType::Int,
    },
    SpeHeaderEntry {
        name: "PULSE_BURST_PERIOD",
        offset: 4050,
        kind: SpeHeaderType::Long,
    },
    SpeHeaderEntry {
        name: "PULSE_BRACKET_USED",
        offset: 4058,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "PULSE_BRACKET_TYPE",
        offset: 4059,
        kind: SpeHeaderType::Byte,
    },
    SpeHeaderEntry {
        name: "PULSE_TIMECONST_FAST",
        offset: 4060,
        kind: SpeHeaderType::Long,
    },
    SpeHeaderEntry {
        name: "PULSE_AMP_FAST",
        offset: 4068,
        kind: SpeHeaderType::Long,
    },
    SpeHeaderEntry {
        name: "PULSE_TIMECONST_SLOW",
        offset: 4076,
        kind: SpeHeaderType::Long,
    },
    SpeHeaderEntry {
        name: "PULSE_AMP_SLOW",
        offset: 4084,
        kind: SpeHeaderType::Long,
    },
    SpeHeaderEntry {
        name: "ANALOG_GAIN",
        offset: 4092,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "AV_GAIN_USED",
        offset: 4094,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "AV_GAIN",
        offset: 4096,
        kind: SpeHeaderType::Short,
    },
    SpeHeaderEntry {
        name: "LAST_VALUE",
        offset: 4098,
        kind: SpeHeaderType::Short,
    },
];

fn r_i16_le(b: &[u8], off: usize) -> i16 {
    i16::from_le_bytes([b[off], b[off + 1]])
}
fn r_u16_le(b: &[u8], off: usize) -> u16 {
    u16::from_le_bytes([b[off], b[off + 1]])
}
fn r_i32_le(b: &[u8], off: usize) -> i32 {
    i32::from_le_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}
fn r_i64_le(b: &[u8], off: usize) -> i64 {
    i64::from_le_bytes([
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
fn r_u8(b: &[u8], off: usize) -> u8 {
    b[off]
}

fn spe_string(hdr: &[u8], off: usize, len: usize) -> String {
    String::from_utf8_lossy(&hdr[off..off + len])
        .trim()
        .to_string()
}

fn spe_array_string<T: std::fmt::Display>(values: &[T]) -> String {
    let body = values
        .iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join(", ");
    format!("[{body}]")
}

fn spe_header_entry_len(index: usize) -> usize {
    SPE_HEADER_ENTRIES
        .get(index + 1)
        .map(|next| next.offset - SPE_HEADER_ENTRIES[index].offset)
        .unwrap_or(HEADER_SIZE as usize - SPE_HEADER_ENTRIES[index].offset)
}

fn populate_spe_header_metadata(hdr: &[u8], meta: &mut HashMap<String, MetadataValue>) {
    for (index, entry) in SPE_HEADER_ENTRIES.iter().enumerate() {
        if entry.kind == SpeHeaderType::RoiArray {
            continue;
        }
        let len = spe_header_entry_len(index);
        match entry.kind {
            SpeHeaderType::Int => {
                let value = r_i32_le(hdr, entry.offset);
                if value > 0 {
                    meta.insert(entry.name.into(), MetadataValue::Int(value as i64));
                }
            }
            SpeHeaderType::Short => {
                let value = r_u16_le(hdr, entry.offset);
                if value > 0 {
                    meta.insert(entry.name.into(), MetadataValue::Int(value as i64));
                }
            }
            SpeHeaderType::Long => {
                let value = r_i64_le(hdr, entry.offset);
                if value > 0 {
                    meta.insert(entry.name.into(), MetadataValue::Int(value));
                }
            }
            SpeHeaderType::Byte => {
                let value = r_u8(hdr, entry.offset);
                if value > 0 {
                    meta.insert(entry.name.into(), MetadataValue::Int(value as i64));
                }
            }
            SpeHeaderType::String => {
                let value = spe_string(hdr, entry.offset, len);
                if !value.is_empty() {
                    meta.insert(entry.name.into(), MetadataValue::String(value));
                }
            }
            SpeHeaderType::IntArray => {
                let values: Vec<i32> = (0..len / 4)
                    .map(|i| r_i32_le(hdr, entry.offset + i * 4))
                    .collect();
                if values.iter().any(|v| *v > 0) {
                    meta.insert(
                        entry.name.into(),
                        MetadataValue::String(spe_array_string(&values)),
                    );
                }
            }
            SpeHeaderType::ShortArray => {
                let values: Vec<u16> = (0..len / 2)
                    .map(|i| r_u16_le(hdr, entry.offset + i * 2))
                    .collect();
                if values.iter().any(|v| *v > 0) {
                    meta.insert(
                        entry.name.into(),
                        MetadataValue::String(spe_array_string(&values)),
                    );
                }
            }
            SpeHeaderType::LongArray => {
                let values: Vec<i64> = (0..len / 8)
                    .map(|i| r_i64_le(hdr, entry.offset + i * 8))
                    .collect();
                if values.iter().any(|v| *v > 0) {
                    meta.insert(
                        entry.name.into(),
                        MetadataValue::String(spe_array_string(&values)),
                    );
                }
            }
            SpeHeaderType::RoiArray => {}
        }
    }
}

fn spe_rois(hdr: &[u8]) -> Vec<SpeRoi> {
    let num_rois = r_u16_le(hdr, 1510) as usize;
    let max_rois = ((1632usize - 1512usize) / 12).min(num_rois);
    (0..max_rois)
        .map(|i| {
            let off = 1512 + i * 12;
            SpeRoi {
                start_x: r_u16_le(hdr, off),
                end_x: r_u16_le(hdr, off + 2),
                group_x: r_u16_le(hdr, off + 4),
                start_y: r_u16_le(hdr, off + 6),
                end_y: r_u16_le(hdr, off + 8),
                group_y: r_u16_le(hdr, off + 10),
            }
        })
        .collect()
}

#[derive(Clone, Copy)]
struct SpeRoi {
    start_x: u16,
    end_x: u16,
    group_x: u16,
    start_y: u16,
    end_y: u16,
    group_y: u16,
}

fn populate_spe_roi_metadata(hdr: &[u8], meta: &mut HashMap<String, MetadataValue>) {
    for (index, roi) in spe_rois(hdr).iter().enumerate() {
        let prefix = format!("ROI {}", index + 1);
        meta.insert(
            format!("{prefix} Start X"),
            MetadataValue::Int(roi.start_x as i64),
        );
        meta.insert(
            format!("{prefix} End X"),
            MetadataValue::Int(roi.end_x as i64),
        );
        meta.insert(
            format!("{prefix} Group X"),
            MetadataValue::Int(roi.group_x as i64),
        );
        meta.insert(
            format!("{prefix} Start Y"),
            MetadataValue::Int(roi.start_y as i64),
        );
        meta.insert(
            format!("{prefix} End Y"),
            MetadataValue::Int(roi.end_y as i64),
        );
        meta.insert(
            format!("{prefix} Group Y"),
            MetadataValue::Int(roi.group_y as i64),
        );
    }
}

/// SPE datatype codes
fn spe_pixel_type(datatype: i16) -> (PixelType, u8) {
    // Per SPEReader.java: FLOAT=0, INT32=1, INT16=2, UNINT16=3, UNINT32=4.
    match datatype {
        0 => (PixelType::Float32, 32),
        1 => (PixelType::Int32, 32),
        2 => (PixelType::Int16, 16),
        3 => (PixelType::Uint16, 16),
        4 => (PixelType::Uint32, 32),
        _ => (PixelType::Uint16, 16),
    }
}

/// Replicate Java SPEReader.SpeHeader.getStackSize (904-919): used as a
/// fallback to derive the frame count when NUM_FRAMES < 1.
///
/// Offsets (all little-endian, matching SpeHeaderEntry):
///   HEIGHT     = 656 (short, Y dim of raw data / "stripe")
///   NOSCAN     =  34 (short, old num scans; usually -1, i.e. 65535 unsigned)
///   LNOSCAN    = 664 (int, number of scans for early WinX)
///   NUM_FRAMES =1446 (int)
///
/// Note: Java's getShort reads an UNSIGNED 16-bit value (no sign extension),
/// so the `noscan == 65535` check is performed against the unsigned reading
/// (r_u16_le), matching Java exactly.
fn spe_stack_size(hdr: &[u8]) -> i32 {
    let stripe = r_u16_le(hdr, 656) as i32; // HEIGHT
    let noscan = r_u16_le(hdr, 34) as i32; // NOSCAN
    let num_frames = r_i32_le(hdr, 1446); // NUM_FRAMES
    if stripe == 0 || noscan == 0 {
        return num_frames;
    }
    if noscan == 65535 {
        let lnoscan = r_i32_le(hdr, 664); // LNOSCAN
        if lnoscan == -1 || lnoscan == 0 {
            num_frames
        } else {
            lnoscan / stripe
        }
    } else {
        noscan / stripe
    }
}

/// Read the SPE 3.0 trailing XML footer starting at `offset` to EOF.
fn read_xml_footer(f: &mut File, offset: u64) -> Result<String> {
    let len = f.metadata().map_err(BioFormatsError::Io)?.len();
    if offset >= len {
        return Ok(String::new());
    }
    f.seek(SeekFrom::Start(offset))
        .map_err(BioFormatsError::Io)?;
    let mut buf = Vec::with_capacity((len - offset) as usize);
    f.read_to_end(&mut buf).map_err(BioFormatsError::Io)?;
    Ok(String::from_utf8_lossy(&buf)
        .trim_matches(|c: char| c == '\0' || c.is_whitespace())
        .to_string())
}

pub struct SpeReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl SpeReader {
    pub fn new() -> Self {
        SpeReader {
            path: None,
            meta: None,
        }
    }
}

impl Default for SpeReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for SpeReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("spe"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        // Java SPEReader.isThisType only checks FormatTools.validStream(stream, 4, false).
        header.len() >= 4
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        self.close()?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let mut hdr = vec![0u8; HEADER_SIZE as usize];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;

        // Offsets from SPEReader.java SpeHeaderEntry (all little-endian):
        //  DATATYPE   = 108 (short)
        //  WIDTH      =  42 (short)
        //  HEIGHT     = 656 (short)
        //  NUM_FRAMES =1446 (int)
        //  EXPOSURE   =  10 (int)
        //  DATE       =  20 (10 bytes, byte string)
        //  XML_OFFSET = 678 (long)
        //  HEADER_VER =1992 (int)
        let datatype = r_i16_le(&hdr, 108);
        let xdim = positive_u16_dim(r_u16_le(&hdr, 42), "width")?;
        let ydim = positive_u16_dim(r_u16_le(&hdr, 656), "height")?;
        // NUM_FRAMES (offset 1446, int). When < 1, Java SPEReader.java:152-155
        // falls back to header.getStackSize() before erroring.
        let raw_numframes = r_i32_le(&hdr, 1446);
        let numframes = if raw_numframes < 1 {
            let stack_size = spe_stack_size(&hdr);
            if stack_size >= 1 {
                stack_size as u32
            } else {
                // Still non-positive after the fallback: reject as Java would
                // produce an invalid (<1) frame count.
                positive_i32_dim(raw_numframes, "frame count")?
            }
        } else {
            positive_i32_dim(raw_numframes, "frame count")?
        };
        let header_ver = r_i32_le(&hdr, 1992);
        let xml_offset = r_i64_le(&hdr, 678);

        // Java throws "Invalid pixel type" for unknown datatypes (FLOAT=0,
        // INT32=1, INT16=2, UNINT16=3, UNINT32=4).
        if !matches!(datatype, 0..=4) {
            return Err(BioFormatsError::Format(format!(
                "SPE: invalid pixel type {datatype}"
            )));
        }
        let (pixel_type, bpp) = spe_pixel_type(datatype);
        validate_spe_layout(xdim, ydim, numframes, pixel_type)?;

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        populate_spe_header_metadata(&hdr, &mut meta_map);
        populate_spe_roi_metadata(&hdr, &mut meta_map);

        // SPE 3.0 XML footer: detected when HEADER_VER >= 3 or XML_OFFSET > 0.
        // Matching SPEReader.java, the binary-header dimensions are authoritative
        // and the file is flagged metadata-incomplete; we additionally expose the
        // raw footer XML text so downstream callers can inspect it.
        if header_ver >= 3 || xml_offset > 0 {
            meta_map.insert("XML_OFFSET".into(), MetadataValue::Int(xml_offset));
            meta_map.insert("metadataComplete".into(), MetadataValue::Bool(false));
            if xml_offset > 0 {
                if let Ok(xml) = read_xml_footer(&mut f, xml_offset as u64) {
                    if !xml.is_empty() {
                        meta_map.insert("XMLFooter".into(), MetadataValue::String(xml));
                    }
                }
            }
        } else {
            meta_map.insert("metadataComplete".into(), MetadataValue::Bool(true));
        }

        self.meta = Some(ImageMetadata {
            size_x: xdim,
            size_y: ydim,
            // Java: sizeZ=1, sizeC=1, sizeT=numFrames, order "XYZTC".
            size_z: 1,
            size_c: 1,
            size_t: numframes,
            pixel_type,
            bits_per_pixel: (bpp).into(),
            image_count: numframes,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: true,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        usize::from(self.meta.is_some())
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
        if self.meta.is_none() {
            return Err(BioFormatsError::NotInitialized);
        }
        if s != 0 {
            Err(BioFormatsError::SeriesOutOfRange(s))
        } else {
            Ok(())
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
        let plane_bytes = (meta.size_x * meta.size_y) as usize * bps;
        let offset = HEADER_SIZE + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        if offset
            .checked_add(plane_bytes as u64)
            .is_none_or(|end| end > file_len)
        {
            return Ok(vec![0u8; plane_bytes]);
        }
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;
        Ok(buf)
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
        crop_full_plane("SPE", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }

    fn ome_metadata(&self) -> Option<crate::common::ome_metadata::OmeMetadata> {
        use crate::common::ome_metadata::{create_lsid, OmeMetadata, OmeROI, OmeShape};
        let meta = self.meta.as_ref()?;
        // SPEReader.java populates pixels only; exposure time is stored as a
        // global metadata int (microseconds, per the SPE spec) and is not mapped
        // to per-plane OME PlaneDeltaT, so we mirror the pixel-only mapping.
        let mut ome = OmeMetadata::from_image_metadata(meta);
        // MetadataTools.populatePixels sets the image name to the file's basename.
        if let (Some(path), Some(img)) = (self.path.as_ref(), ome.images.get_mut(0)) {
            img.name = path
                .file_name()
                .and_then(|n| n.to_str())
                .map(|s| s.to_string());
        }
        let roi_count = spe_metadata_roi_count(meta);
        for index in 0..roi_count {
            let prefix = format!("ROI {}", index + 1);
            let start_x = spe_metadata_i64(meta, &format!("{prefix} Start X"))?;
            let end_x = spe_metadata_i64(meta, &format!("{prefix} End X"))?;
            let start_y = spe_metadata_i64(meta, &format!("{prefix} Start Y"))?;
            let end_y = spe_metadata_i64(meta, &format!("{prefix} End Y"))?;
            ome.rois.push(OmeROI {
                id: Some(create_lsid("ROI", &[index])),
                name: Some(prefix.clone()),
                shapes: vec![OmeShape::Rectangle {
                    x: start_x as f64,
                    y: start_y as f64,
                    width: (end_x - start_x) as f64,
                    height: (end_y - start_y) as f64,
                    the_z: None,
                    the_t: None,
                    the_c: None,
                }],
            });
        }
        Some(ome)
    }
}

fn spe_metadata_i64(meta: &ImageMetadata, key: &str) -> Option<i64> {
    match meta.series_metadata.get(key) {
        Some(MetadataValue::Int(value)) => Some(*value),
        _ => None,
    }
}

fn spe_metadata_roi_count(meta: &ImageMetadata) -> usize {
    let mut count = 0usize;
    while meta
        .series_metadata
        .contains_key(&format!("ROI {} Start X", count + 1))
    {
        count += 1;
    }
    count
}

fn positive_u16_dim(value: u16, label: &str) -> Result<u32> {
    if value == 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "SPE header has non-positive {label}"
        )));
    }
    Ok(value as u32)
}

fn positive_i32_dim(value: i32, label: &str) -> Result<u32> {
    if value <= 0 {
        return Err(BioFormatsError::UnsupportedFormat(format!(
            "SPE header has non-positive {label}"
        )));
    }
    Ok(value as u32)
}

fn validate_spe_layout(size_x: u32, size_y: u32, frames: u32, pixel_type: PixelType) -> Result<()> {
    let plane_bytes = (size_x as u64)
        .checked_mul(size_y as u64)
        .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample() as u64))
        .ok_or_else(|| BioFormatsError::Format("SPE plane size overflows".into()))?;
    HEADER_SIZE
        .checked_add(
            plane_bytes
                .checked_mul(frames as u64)
                .ok_or_else(|| BioFormatsError::Format("SPE payload size overflows".into()))?,
        )
        .ok_or_else(|| BioFormatsError::Format("SPE payload size overflows".into()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::SpeReader;
    use crate::common::metadata::MetadataValue;
    use crate::common::ome_metadata::OmeShape;
    use crate::common::reader::FormatReader;

    #[test]
    fn spe_byte_identification_matches_java_valid_stream_check() {
        let reader = SpeReader::new();
        assert!(!reader.is_this_type_by_bytes(&[0, 1, 2]));
        assert!(reader.is_this_type_by_bytes(&[0, 1, 2, 3]));
        assert!(reader.is_this_type_by_bytes(b"not actually spe"));
    }

    #[test]
    fn spe_projects_java_header_metadata_and_roi_rectangles() {
        let path = std::env::temp_dir().join("bioformats_spe_header_roi.spe");
        let mut bytes = vec![0u8; 4100];
        bytes[42..44].copy_from_slice(&10u16.to_le_bytes());
        bytes[656..658].copy_from_slice(&8u16.to_le_bytes());
        bytes[108..110].copy_from_slice(&3i16.to_le_bytes());
        bytes[1446..1450].copy_from_slice(&1i32.to_le_bytes());
        bytes[10..14].copy_from_slice(&25i32.to_le_bytes());
        bytes[158..160].copy_from_slice(&4u16.to_le_bytes());
        bytes[160..162].copy_from_slice(&5u16.to_le_bytes());
        bytes[172..179].copy_from_slice(b"12:3456");
        bytes[1510..1512].copy_from_slice(&1u16.to_le_bytes());
        bytes[1512..1514].copy_from_slice(&2u16.to_le_bytes());
        bytes[1514..1516].copy_from_slice(&7u16.to_le_bytes());
        bytes[1516..1518].copy_from_slice(&2u16.to_le_bytes());
        bytes[1518..1520].copy_from_slice(&3u16.to_le_bytes());
        bytes[1520..1522].copy_from_slice(&6u16.to_le_bytes());
        bytes[1522..1524].copy_from_slice(&3u16.to_le_bytes());
        bytes.extend(std::iter::repeat_n(0, 10 * 8));
        std::fs::write(&path, bytes).unwrap();

        let mut reader = SpeReader::new();
        reader.set_id(&path).unwrap();
        let metadata = &reader.metadata().series_metadata;
        assert!(matches!(
            metadata.get("EXPOSURE"),
            Some(MetadataValue::Int(25))
        ));
        assert!(matches!(
            metadata.get("SPEC_MIRR_LOC"),
            Some(MetadataValue::String(value)) if value == "[4, 5]"
        ));
        assert!(matches!(
            metadata.get("EXP_TIME_LOCAL"),
            Some(MetadataValue::String(value)) if value == "12:3456"
        ));
        assert!(matches!(
            metadata.get("ROI 1 Start X"),
            Some(MetadataValue::Int(2))
        ));
        assert!(matches!(
            metadata.get("ROI 1 End Y"),
            Some(MetadataValue::Int(6))
        ));
        assert!(matches!(
            metadata.get("ROI 1 Group Y"),
            Some(MetadataValue::Int(3))
        ));

        let ome = reader.ome_metadata().unwrap();
        assert_eq!(ome.rois.len(), 1);
        assert_eq!(ome.rois[0].id.as_deref(), Some("ROI:0"));
        assert_eq!(ome.rois[0].name.as_deref(), Some("ROI 1"));
        assert!(matches!(
            ome.rois[0].shapes.as_slice(),
            [OmeShape::Rectangle {
                x: 2.0,
                y: 3.0,
                width: 5.0,
                height: 3.0,
                ..
            }]
        ));
        let _ = std::fs::remove_file(path);
    }
}
