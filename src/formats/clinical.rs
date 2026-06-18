//! Clinical scanner format readers: ECAT7 PET, Inveon PET/CT, Varian FDF MRI.

use std::collections::HashMap;
use std::fs::File;
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::common::error::{BioFormatsError, Result};
use crate::common::metadata::{DimensionOrder, ImageMetadata, MetadataValue};
use crate::common::pixel_type::PixelType;
use crate::common::reader::FormatReader;
use crate::common::region::crop_full_plane;

// ─── ECAT7 PET ────────────────────────────────────────────────────────────────
//
// ECAT7 is a format used by CTI/Siemens PET scanners.
// Main header (512 bytes):
//   Offset 0:  magic_number[14] — "MATRIX72v\0" or similar (null-terminated)
//   Offset 14: original_file_name[32]
//   Offset 46: sw_version (i16)
//   Offset 48: system_type (i16)
//   Offset 50: file_type (i16)
//   Offset 52: serial_number[10]
//   Offset 62: scan_start_time (i32)
//   Offset 66: isotope_code[8]
//   ...
//   Offset 80: num_planes (i16)
//   Offset 82: num_frames (i16)
//   Offset 84: num_gates (i16)
//   Offset 86: num_bed_pos (i16)
//
// After the main header, a directory block (512 bytes) maps matrix codes to
// subheader+data blocks. For simplicity we read only the main header for dims.
// Pixel data type is always int16 for emission data (file_type=1) and
// float32 for sinogram data (file_type=2).

fn r_i16_be(b: &[u8], off: usize) -> i16 {
    i16::from_be_bytes([b[off], b[off + 1]])
}

fn r_i32_be(b: &[u8], off: usize) -> i32 {
    i32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

fn r_f32_be(b: &[u8], off: usize) -> f32 {
    f32::from_be_bytes([b[off], b[off + 1], b[off + 2], b[off + 3]])
}

/// Read a fixed-length, null-padded ASCII string from the header, mirroring
/// Java RandomAccessInputStream.readString (trailing NUL bytes dropped).
fn r_string(b: &[u8], off: usize, len: usize) -> String {
    let raw = &b[off..off + len];
    let end = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
    String::from_utf8_lossy(&raw[..end]).to_string()
}

fn is_ecat7_magic(header: &[u8]) -> bool {
    header.len() >= 9
        && &header[..7] == b"MATRIX7"
        && matches!(header[7], b'0' | b'1' | b'2')
        && header[8] == b'v'
}

pub struct Ecat7Reader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
}

impl Ecat7Reader {
    pub fn new() -> Self {
        Ecat7Reader {
            path: None,
            meta: None,
            data_offset: 1024,
        }
    }
}
impl Default for Ecat7Reader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for Ecat7Reader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("v"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        is_ecat7_magic(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        // Per Ecat7Reader.java: main header is 512 bytes, then a 512-byte
        // directory block, then a per-matrix subheader. The first matrix
        // subheader begins at offset 1024; sizeZ/sizeT come from the main
        // header, while sizeX/sizeY/dataType come from the subheader after
        // skipping 512 bytes (i.e. at offset 1024).
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        // Read main header (512) + directory (512) + the full image subheader
        // scalars Java parses. The image subheader tail extends through
        // userFill[48] which ends at offset 1534; read up to HEADER_SIZE
        // (1536, where plane data begins) but no further than the file.
        let file_len_for_hdr = f.metadata().map_err(BioFormatsError::Io)?.len();
        let hdr_len = 1536usize.min(file_len_for_hdr as usize);
        if hdr_len < 1078 {
            return Err(BioFormatsError::UnsupportedFormat(
                "ECAT7 header truncated".into(),
            ));
        }
        let mut hdr = vec![0u8; hdr_len];
        f.read_exact(&mut hdr).map_err(BioFormatsError::Io)?;
        if !is_ecat7_magic(&hdr) {
            return Err(BioFormatsError::UnsupportedFormat(
                "ECAT7 missing MATRIX7[012]v magic".into(),
            ));
        }

        // Main header values (big-endian).
        let file_type = r_i16_be(&hdr, 50);
        // Following the Java field-by-field reads, facilityName ends at
        // offset 352; sizeZ (short) is at 352 and sizeT (short) at 354.
        let size_z_i = r_i16_be(&hdr, 352);
        let size_t_i = r_i16_be(&hdr, 354);
        if size_z_i <= 0 || size_t_i <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "ECAT7 has zero image dimensions".into(),
            ));
        }
        let size_z = size_z_i as u32;
        let size_t = size_t_i as u32;

        // Subheader begins at offset 1024 (after main header + 512 skip).
        // Java: dataType (short), numDimensions (short), sizeX (short),
        // sizeY (short).
        let data_type = r_i16_be(&hdr, 1024);
        // numDimensions at 1026
        let size_x_i = r_i16_be(&hdr, 1026 + 2);
        let size_y_i = r_i16_be(&hdr, 1026 + 4);
        if size_x_i <= 0 || size_y_i <= 0 {
            return Err(BioFormatsError::UnsupportedFormat(
                "ECAT7 has zero image dimensions".into(),
            ));
        }
        let size_x = size_x_i as u32;
        let size_y = size_y_i as u32;

        let (pixel_type, bpp): (PixelType, u8) = match data_type {
            6 => (PixelType::Uint16, 16),
            _ => {
                return Err(BioFormatsError::UnsupportedFormat(format!(
                    "ECAT7 unsupported data type: {}",
                    data_type
                )))
            }
        };

        let size_c = 1u32;
        let image_count = size_z * size_t * size_c;
        let plane_bytes = (size_x as u64)
            .checked_mul(size_y as u64)
            .and_then(|px| px.checked_mul(pixel_type.bytes_per_sample() as u64))
            .ok_or_else(|| BioFormatsError::Format("ECAT7 plane size overflows".into()))?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        let mut required_len = self.data_offset;
        for plane in 0..image_count {
            let z = plane % size_z.max(1);
            let mut t_skip: u64 = 0;
            for i in 0..z {
                t_skip += 512;
                if i > 0 && (i % 30) == 0 {
                    t_skip += 512;
                }
            }
            let end = 1536u64
                .checked_add((plane as u64).checked_mul(plane_bytes).ok_or_else(|| {
                    BioFormatsError::Format("ECAT7 pixel offset overflows".into())
                })?)
                .and_then(|v| v.checked_add(t_skip))
                .and_then(|v| v.checked_add(plane_bytes))
                .ok_or_else(|| BioFormatsError::Format("ECAT7 pixel offset overflows".into()))?;
            required_len = required_len.max(end);
        }
        if file_len < required_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "ECAT7 pixel payload is shorter than declared ({file_len} < {required_len})"
            )));
        }

        let mut meta_map: HashMap<String, MetadataValue> = HashMap::new();
        meta_map.insert("format".into(), MetadataValue::String("ECAT7 PET".into()));
        meta_map.insert("file_type".into(), MetadataValue::Int(file_type as i64));
        meta_map.insert("Data type".into(), MetadataValue::Int(data_type as i64));

        // Named main-header scalars (Java Ecat7Reader.initFile addGlobalMeta).
        // Field offsets follow Java's field-by-field reads from offset 14:
        //   originalPath[32]@14, version@46, systemType@48, fileType@50,
        //   serialNumber[10]@52, scanStart@62, isotopeName[8]@66,
        //   isotopeHalflife@74.
        let original_path = r_string(&hdr, 14, 32);
        let version = r_i16_be(&hdr, 46);
        let system_type = r_i16_be(&hdr, 48);
        let serial_number = r_string(&hdr, 52, 10);
        let scan_start = r_i32_be(&hdr, 62);
        let isotope_name = r_string(&hdr, 66, 8);
        let isotope_halflife = r_f32_be(&hdr, 74);
        meta_map.insert("Original path".into(), MetadataValue::String(original_path));
        meta_map.insert("Version".into(), MetadataValue::Int(version as i64));
        meta_map.insert("System type".into(), MetadataValue::Int(system_type as i64));
        meta_map.insert("File type".into(), MetadataValue::Int(file_type as i64));
        meta_map.insert("Serial number".into(), MetadataValue::String(serial_number));
        meta_map.insert("Scan start".into(), MetadataValue::Int(scan_start as i64));
        meta_map.insert("Isotope Name".into(), MetadataValue::String(isotope_name));
        meta_map.insert(
            "Isotope half-life".into(),
            MetadataValue::Float(isotope_halflife as f64),
        );

        // Remaining named main-header scalars (Java Ecat7Reader.initFile
        // continues field-by-field from isotopeHalflife @74):
        //   radioPharmaceutical[32]@78, gantryTilt@110, gantryRotation@114,
        //   bedElevation@118, intrinsicTilt@122, wobbleSpeed@126,
        //   sourceType@128, distanceScanned@130, transaxialFOV@134,
        //   angularCompression@138, coinSampleMode@140, axialSampleMode@142,
        //   calibrationFactor@144, calibrationUnits@148, calibrationLabel@150,
        //   compression@152.
        let radio_pharmaceutical = r_string(&hdr, 78, 32);
        let gantry_tilt = r_f32_be(&hdr, 110);
        let gantry_rotation = r_f32_be(&hdr, 114);
        let bed_elevation = r_f32_be(&hdr, 118);
        let intrinsic_tilt = r_f32_be(&hdr, 122);
        let wobble_speed = r_i16_be(&hdr, 126);
        let source_type = r_i16_be(&hdr, 128);
        let distance_scanned = r_f32_be(&hdr, 130);
        let transaxial_fov = r_f32_be(&hdr, 134);
        let angular_compression = r_i16_be(&hdr, 138);
        let coin_sample_mode = r_i16_be(&hdr, 140);
        let axial_sample_mode = r_i16_be(&hdr, 142);
        let calibration_factor = r_f32_be(&hdr, 144);
        let calibration_units = r_i16_be(&hdr, 148);
        let calibration_label = r_i16_be(&hdr, 150);
        let compression = r_i16_be(&hdr, 152);
        meta_map.insert(
            "Radiopharmaceutical".into(),
            MetadataValue::String(radio_pharmaceutical),
        );
        meta_map.insert(
            "Gantry tilt".into(),
            MetadataValue::Float(gantry_tilt as f64),
        );
        meta_map.insert(
            "Gantry rotation".into(),
            MetadataValue::Float(gantry_rotation as f64),
        );
        meta_map.insert(
            "Bed elevation".into(),
            MetadataValue::Float(bed_elevation as f64),
        );
        meta_map.insert(
            "Intrinsic tilt".into(),
            MetadataValue::Float(intrinsic_tilt as f64),
        );
        meta_map.insert(
            "Wobble speed".into(),
            MetadataValue::Int(wobble_speed as i64),
        );
        meta_map.insert("Source type".into(), MetadataValue::Int(source_type as i64));
        meta_map.insert(
            "Distance scanned".into(),
            MetadataValue::Float(distance_scanned as f64),
        );
        meta_map.insert(
            "Transaxial FOV".into(),
            MetadataValue::Float(transaxial_fov as f64),
        );
        meta_map.insert(
            "Angular compression".into(),
            MetadataValue::Int(angular_compression as i64),
        );
        meta_map.insert(
            "Coin. sample mode".into(),
            MetadataValue::Int(coin_sample_mode as i64),
        );
        meta_map.insert(
            "Axial sample mode".into(),
            MetadataValue::Int(axial_sample_mode as i64),
        );
        meta_map.insert(
            "Calibration factor".into(),
            MetadataValue::Float(calibration_factor as f64),
        );
        meta_map.insert(
            "Calibration units".into(),
            MetadataValue::Int(calibration_units as i64),
        );
        meta_map.insert(
            "Calibration units label".into(),
            MetadataValue::Int(calibration_label as i64),
        );
        meta_map.insert("Compression".into(), MetadataValue::Int(compression as i64));

        // Patient/study header block (Java Ecat7Reader.initFile continues
        // field-by-field from compression @152):
        //   studyType[12]@154, patientID[16]@166, patientName[32]@182,
        //   patientSex[1]@214, patientDexterity[1]@215, patientAge@216,
        //   patientHeight@220, patientWeight@224, patientBirthDate@228,
        //   physicianName[32]@232, operatorName[32]@264, description[32]@296,
        //   acquisitionType@328, patientOrientation@330, facilityName[20]@332.
        let study_type = r_string(&hdr, 154, 12);
        let patient_id = r_string(&hdr, 166, 16);
        let patient_name = r_string(&hdr, 182, 32);
        let patient_sex = r_string(&hdr, 214, 1);
        let patient_dexterity = r_string(&hdr, 215, 1);
        let patient_age = r_f32_be(&hdr, 216);
        let patient_height = r_f32_be(&hdr, 220);
        let patient_weight = r_f32_be(&hdr, 224);
        let patient_birth_date = r_i32_be(&hdr, 228);
        let physician_name = r_string(&hdr, 232, 32);
        let operator_name = r_string(&hdr, 264, 32);
        let description = r_string(&hdr, 296, 32);
        let acquisition_type = r_i16_be(&hdr, 328);
        let patient_orientation = r_i16_be(&hdr, 330);
        let facility_name = r_string(&hdr, 332, 20);
        meta_map.insert("Study type".into(), MetadataValue::String(study_type));
        meta_map.insert("Patient ID".into(), MetadataValue::String(patient_id));
        meta_map.insert("Patient name".into(), MetadataValue::String(patient_name));
        meta_map.insert("Patient sex".into(), MetadataValue::String(patient_sex));
        meta_map.insert(
            "Patient dexterity".into(),
            MetadataValue::String(patient_dexterity),
        );
        meta_map.insert(
            "Patient age".into(),
            MetadataValue::Float(patient_age as f64),
        );
        meta_map.insert(
            "Patient height".into(),
            MetadataValue::Float(patient_height as f64),
        );
        meta_map.insert(
            "Patient weight".into(),
            MetadataValue::Float(patient_weight as f64),
        );
        meta_map.insert(
            "Patient birth date".into(),
            MetadataValue::Int(patient_birth_date as i64),
        );
        meta_map.insert(
            "Physician name".into(),
            MetadataValue::String(physician_name),
        );
        meta_map.insert("Operator name".into(), MetadataValue::String(operator_name));
        meta_map.insert("Description".into(), MetadataValue::String(description));
        meta_map.insert(
            "Acquisition type".into(),
            MetadataValue::Int(acquisition_type as i64),
        );
        meta_map.insert(
            "Patient orientation".into(),
            MetadataValue::Int(patient_orientation as i64),
        );
        meta_map.insert("Facility name".into(), MetadataValue::String(facility_name));

        // Acquisition/scan-parameter block (Java Ecat7Reader.initFile continues
        // field-by-field from facilityName @332 ending at 352):
        //   sizeZ@352, sizeT@354 (consumed as dimensions above), numGates@356,
        //   numBedPositions@358, initBedPosition@360, bedPositions[15]@364
        //   (15 floats, ending at 424), planeSeparation@424, lowerThreshold@428,
        //   trueLowerThreshold@430, trueUpperThreshold@432, processCode[10]@434,
        //   acquisitionMode@444, binSize@446, branchingFraction@450,
        //   doseStartTime@454, dosage@458, wellCounterCorrectionFactor@462,
        //   dataUnits[32]@466, septaState@498, fillCTI[6]@500 (6 shorts,
        //   ending at 512). All offsets fall within the main-header buffer.
        let num_gates = r_i16_be(&hdr, 356);
        let num_bed_positions = r_i16_be(&hdr, 358);
        meta_map.insert(
            "Number of gates".into(),
            MetadataValue::Int(num_gates as i64),
        );
        meta_map.insert(
            "Number of bed positions".into(),
            MetadataValue::Int(num_bed_positions as i64),
        );
        // Java reads initBedPosition @360 (not emitted), then 15 bed positions
        // @364, and emits each via addGlobalMetaList("Bed position", ...). With
        // 15 entries the flattened keys are "Bed position #01".."Bed position
        // #15" (2-digit, leading-zero padded; see FormatReader.updateMetadataLists).
        for i in 0..15usize {
            let bed_pos = r_f32_be(&hdr, 364 + i * 4);
            meta_map.insert(
                format!("Bed position #{:02}", i + 1),
                MetadataValue::Float(bed_pos as f64),
            );
        }
        let plane_separation = r_f32_be(&hdr, 424);
        let lower_threshold = r_i16_be(&hdr, 428);
        let true_lower_threshold = r_i16_be(&hdr, 430);
        let true_upper_threshold = r_i16_be(&hdr, 432);
        let process_code = r_string(&hdr, 434, 10);
        let acquisition_mode = r_i16_be(&hdr, 444);
        let bin_size = r_f32_be(&hdr, 446);
        let branching_fraction = r_f32_be(&hdr, 450);
        let dose_start_time = r_i32_be(&hdr, 454);
        let dosage = r_f32_be(&hdr, 458);
        let well_counter_correction_factor = r_f32_be(&hdr, 462);
        let data_units = r_string(&hdr, 466, 32);
        let septa_state = r_i16_be(&hdr, 498);
        meta_map.insert(
            "Plane separation".into(),
            MetadataValue::Float(plane_separation as f64),
        );
        meta_map.insert(
            "Lower threshold".into(),
            MetadataValue::Int(lower_threshold as i64),
        );
        meta_map.insert(
            "True lower threshold".into(),
            MetadataValue::Int(true_lower_threshold as i64),
        );
        meta_map.insert(
            "True upper threshold".into(),
            MetadataValue::Int(true_upper_threshold as i64),
        );
        meta_map.insert("Process code".into(), MetadataValue::String(process_code));
        // Java's key is misspelled "Acquistion mode"; mirror it exactly.
        meta_map.insert(
            "Acquistion mode".into(),
            MetadataValue::Int(acquisition_mode as i64),
        );
        meta_map.insert("Bin size".into(), MetadataValue::Float(bin_size as f64));
        meta_map.insert(
            "Branching fraction".into(),
            MetadataValue::Float(branching_fraction as f64),
        );
        meta_map.insert(
            "Dose start time".into(),
            MetadataValue::Int(dose_start_time as i64),
        );
        meta_map.insert("Dosage".into(), MetadataValue::Float(dosage as f64));
        meta_map.insert(
            "Well counter correction factor".into(),
            MetadataValue::Float(well_counter_correction_factor as f64),
        );
        meta_map.insert("Data units".into(), MetadataValue::String(data_units));
        meta_map.insert("Septa state".into(), MetadataValue::Int(septa_state as i64));
        // fillCTI[6] @500 emitted via addGlobalMetaList("Fill CTI", ...); 6
        // entries flatten to 1-digit keys "Fill CTI #1".."Fill CTI #6".
        for i in 0..6usize {
            let fill = r_i16_be(&hdr, 500 + i * 2);
            meta_map.insert(
                format!("Fill CTI #{}", i + 1),
                MetadataValue::Int(fill as i64),
            );
        }

        // Named image-subheader scalars. Java skips the 512-byte directory
        // block (in.skipBytes(512)) at offset 512, then reads the subheader
        // starting at 1024: dataType@1024, numDimensions@1026, sizeX@1028,
        // sizeY@1030, skipBytes(2)@1032, then the floating/int scalars below.
        let num_dimensions = r_i16_be(&hdr, 1026);
        meta_map.insert(
            "Number of dimensions".into(),
            MetadataValue::Int(num_dimensions as i64),
        );
        let x_offset = r_f32_be(&hdr, 1034);
        let y_offset = r_f32_be(&hdr, 1038);
        let z_offset = r_f32_be(&hdr, 1042);
        let recon_zoom = r_f32_be(&hdr, 1046);
        let scale_factor = r_f32_be(&hdr, 1050);
        let image_min = r_i16_be(&hdr, 1054);
        let image_max = r_i16_be(&hdr, 1056);
        let x_pixel_size = r_f32_be(&hdr, 1058);
        let y_pixel_size = r_f32_be(&hdr, 1062);
        let z_pixel_size = r_f32_be(&hdr, 1066);
        let frame_duration = r_i32_be(&hdr, 1070);
        let frame_start_time = r_i32_be(&hdr, 1074);
        meta_map.insert("X offset".into(), MetadataValue::Float(x_offset as f64));
        meta_map.insert("Y offset".into(), MetadataValue::Float(y_offset as f64));
        meta_map.insert("Z offset".into(), MetadataValue::Float(z_offset as f64));
        meta_map.insert(
            "Recon. zoom".into(),
            MetadataValue::Float(recon_zoom as f64),
        );
        meta_map.insert(
            "Scale factor".into(),
            MetadataValue::Float(scale_factor as f64),
        );
        meta_map.insert("Image minimum".into(), MetadataValue::Int(image_min as i64));
        meta_map.insert("Image maximum".into(), MetadataValue::Int(image_max as i64));
        meta_map.insert(
            "X pixel size".into(),
            MetadataValue::Float(x_pixel_size as f64),
        );
        meta_map.insert(
            "Y pixel size".into(),
            MetadataValue::Float(y_pixel_size as f64),
        );
        meta_map.insert(
            "Z pixel size".into(),
            MetadataValue::Float(z_pixel_size as f64),
        );
        meta_map.insert(
            "Frame duration".into(),
            MetadataValue::Int(frame_duration as i64),
        );
        meta_map.insert(
            "Frame start time".into(),
            MetadataValue::Int(frame_start_time as i64),
        );

        // Image-subheader tail (Java Ecat7Reader.initFile continues
        // field-by-field from frameStartTime @1074, which ends at 1078):
        //   filterCode short@1078, xResolution float@1080,
        //   yResolution float@1084, zResolution float@1088,
        //   numRElements float@1092, numAngles float@1096,
        //   zRotationAngle float@1100, decayCorrectionFactor float@1104,
        //   processingCode int@1108, gateDuration int@1112,
        //   rWaveOffset int@1116, numAcceptedBeats int@1120,
        //   filterCutoffFrequency float@1124, filterResolution float@1128,
        //   filterRampSlope float@1132, filterOrder short@1136,
        //   filterScatterFraction float@1138, filterScatterSlope float@1142,
        //   annotation String(40)@1146 (ends @1186), matrix[3][3] floats
        //   (cols 0..2) @1186 (9 floats, ends @1222),
        //   rFilterCutoff float@1222, rFilterResolution float@1226,
        //   rFilterCode short@1230, rFilterOrder short@1232,
        //   zFilterCutoff float@1234, zFilterResolution float@1238,
        //   zFilterCode short@1242, zFilterOrder short@1244,
        //   matrix[0][3] float@1246, matrix[1][3] float@1250,
        //   matrix[2][3] float@1254, scatterType short@1258,
        //   reconType short@1260, reconViews short@1262,
        //   ctiFill[87] shorts@1264 (ends @1438),
        //   userFill[48] shorts@1438 (ends @1534).
        // These offsets all lie past 1078; emit only if the subheader is
        // present (matches Java, which reads the entire subheader).
        if hdr.len() >= 1534 {
            let filter_code = r_i16_be(&hdr, 1078);
            let x_resolution = r_f32_be(&hdr, 1080);
            let y_resolution = r_f32_be(&hdr, 1084);
            let z_resolution = r_f32_be(&hdr, 1088);
            let num_r_elements = r_f32_be(&hdr, 1092);
            let num_angles = r_f32_be(&hdr, 1096);
            let z_rotation_angle = r_f32_be(&hdr, 1100);
            let decay_correction_factor = r_f32_be(&hdr, 1104);
            let processing_code = r_i32_be(&hdr, 1108);
            let gate_duration = r_i32_be(&hdr, 1112);
            let r_wave_offset = r_i32_be(&hdr, 1116);
            let num_accepted_beats = r_i32_be(&hdr, 1120);
            let filter_cutoff_frequency = r_f32_be(&hdr, 1124);
            let filter_resolution = r_f32_be(&hdr, 1128);
            let filter_ramp_slope = r_f32_be(&hdr, 1132);
            let filter_order = r_i16_be(&hdr, 1136);
            let filter_scatter_fraction = r_f32_be(&hdr, 1138);
            let filter_scatter_slope = r_f32_be(&hdr, 1142);
            let annotation = r_string(&hdr, 1146, 40);

            // matrix is float[3][4]; first loop fills cols 0..2 (9 floats)
            // sequentially at 1186, then later col 3 is filled per-row below.
            let mut matrix = [[0f32; 4]; 3];
            for (i, row) in matrix.iter_mut().enumerate() {
                for (j, cell) in row.iter_mut().take(3).enumerate() {
                    *cell = r_f32_be(&hdr, 1186 + (i * 3 + j) * 4);
                }
            }

            let r_filter_cutoff = r_f32_be(&hdr, 1222);
            let r_filter_resolution = r_f32_be(&hdr, 1226);
            let r_filter_code = r_i16_be(&hdr, 1230);
            let r_filter_order = r_i16_be(&hdr, 1232);
            let z_filter_cutoff = r_f32_be(&hdr, 1234);
            let z_filter_resolution = r_f32_be(&hdr, 1238);
            let z_filter_code = r_i16_be(&hdr, 1242);
            let z_filter_order = r_i16_be(&hdr, 1244);
            matrix[0][3] = r_f32_be(&hdr, 1246);
            matrix[1][3] = r_f32_be(&hdr, 1250);
            matrix[2][3] = r_f32_be(&hdr, 1254);
            let scatter_type = r_i16_be(&hdr, 1258);
            let recon_type = r_i16_be(&hdr, 1260);
            let recon_views = r_i16_be(&hdr, 1262);

            meta_map.insert("Filter code".into(), MetadataValue::Int(filter_code as i64));
            meta_map.insert(
                "X resolution".into(),
                MetadataValue::Float(x_resolution as f64),
            );
            meta_map.insert(
                "Y resolution".into(),
                MetadataValue::Float(y_resolution as f64),
            );
            meta_map.insert(
                "Z resolution".into(),
                MetadataValue::Float(z_resolution as f64),
            );
            meta_map.insert(
                "Number of R elements".into(),
                MetadataValue::Float(num_r_elements as f64),
            );
            meta_map.insert(
                "Number of angles".into(),
                MetadataValue::Float(num_angles as f64),
            );
            meta_map.insert(
                "Z rotation angle".into(),
                MetadataValue::Float(z_rotation_angle as f64),
            );
            meta_map.insert(
                "Decay correction factor".into(),
                MetadataValue::Float(decay_correction_factor as f64),
            );
            meta_map.insert(
                "Processing code".into(),
                MetadataValue::Int(processing_code as i64),
            );
            meta_map.insert(
                "Gate duration".into(),
                MetadataValue::Int(gate_duration as i64),
            );
            meta_map.insert(
                "R wave offset".into(),
                MetadataValue::Int(r_wave_offset as i64),
            );
            meta_map.insert(
                "Number of accepted beats".into(),
                MetadataValue::Int(num_accepted_beats as i64),
            );
            meta_map.insert(
                "Filter cutoff frequency".into(),
                MetadataValue::Float(filter_cutoff_frequency as f64),
            );
            meta_map.insert(
                "Filter resolution".into(),
                MetadataValue::Float(filter_resolution as f64),
            );
            meta_map.insert(
                "Filter ramp slope".into(),
                MetadataValue::Float(filter_ramp_slope as f64),
            );
            meta_map.insert(
                "Filter order".into(),
                MetadataValue::Int(filter_order as i64),
            );
            meta_map.insert(
                "Filter scatter fraction".into(),
                MetadataValue::Float(filter_scatter_fraction as f64),
            );
            meta_map.insert(
                "Filter scatter slope".into(),
                MetadataValue::Float(filter_scatter_slope as f64),
            );
            meta_map.insert("Annotation".into(), MetadataValue::String(annotation));

            // MT (i, j) keys: 1-based row/col, all 12 cells.
            for (i, row) in matrix.iter().enumerate() {
                for (j, cell) in row.iter().enumerate() {
                    meta_map.insert(
                        format!("MT ({}, {})", i + 1, j + 1),
                        MetadataValue::Float(*cell as f64),
                    );
                }
            }

            meta_map.insert(
                "R filter cutoff".into(),
                MetadataValue::Float(r_filter_cutoff as f64),
            );
            meta_map.insert(
                "R filter resolution".into(),
                MetadataValue::Float(r_filter_resolution as f64),
            );
            meta_map.insert(
                "R filter code".into(),
                MetadataValue::Int(r_filter_code as i64),
            );
            meta_map.insert(
                "R filter order".into(),
                MetadataValue::Int(r_filter_order as i64),
            );
            meta_map.insert(
                "Z filter cutoff".into(),
                MetadataValue::Float(z_filter_cutoff as f64),
            );
            meta_map.insert(
                "Z filter resolution".into(),
                MetadataValue::Float(z_filter_resolution as f64),
            );
            meta_map.insert(
                "Z filter code".into(),
                MetadataValue::Int(z_filter_code as i64),
            );
            meta_map.insert(
                "Z filter order".into(),
                MetadataValue::Int(z_filter_order as i64),
            );
            meta_map.insert(
                "Scatter type".into(),
                MetadataValue::Int(scatter_type as i64),
            );
            meta_map.insert("Recon. type".into(), MetadataValue::Int(recon_type as i64));
            meta_map.insert(
                "Recon. views".into(),
                MetadataValue::Int(recon_views as i64),
            );

            // ctiFill[87]@1264 and userFill[48]@1438 are emitted by Java with a
            // single repeated key each (plain addGlobalMeta in a loop, so only
            // the last value survives the overwrite). Both fit within the
            // 1534-byte subheader guarded above.
            let last_cti = r_i16_be(&hdr, 1264 + 86 * 2);
            meta_map.insert("CTI fill".into(), MetadataValue::Int(last_cti as i64));
            let last_user = r_i16_be(&hdr, 1438 + 47 * 2);
            meta_map.insert("User fill".into(), MetadataValue::Int(last_user as i64));
        }

        self.meta = Some(ImageMetadata {
            size_x,
            size_y,
            size_z,
            size_c,
            size_t,
            pixel_type,
            bits_per_pixel: bpp,
            image_count,
            dimension_order: DimensionOrder::XYZTC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: false, // ECAT7 is big-endian
            resolution_count: 1,
            thumbnail: false,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        // HEADER_SIZE in Java is 1536: main header (512) + directory (512) +
        // first subheader (512). Plane data starts after the first subheader.
        self.data_offset = 1536;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        1
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
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

        // Java Ecat7Reader.openBytes: there is an interleaved 512-byte
        // subheader before every Z-plane, plus an extra 512 bytes every 30
        // planes. tSkip = sum over i in 0..z { 512; +512 if i>0 && i%30==0 }.
        // The Z-coordinate is derived from the plane index via getZCTCoords;
        // for dimensionOrder XYZTC with sizeC=1 the Z-coordinate is
        // plane_index % sizeZ.
        let size_z = meta.size_z.max(1);
        let z = plane_index % size_z;
        let mut t_skip: u64 = 0;
        for i in 0..z {
            t_skip += 512;
            if i > 0 && (i % 30) == 0 {
                t_skip += 512;
            }
        }
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64 + t_skip;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
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
        crop_full_plane("ECAT7", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ─── Inveon PET/CT ────────────────────────────────────────────────────────────
//
// Siemens Inveon preclinical PET/CT stores data as:
//   <stem>.hdr — ASCII text header with key=value lines
//   <stem>.img — raw binary pixel data (default little-endian, float32 or int16)
//
// Key header fields (lower-case):
//   x_dimension <n>
//   y_dimension <n>
//   z_dimension <n>
//   data_type <n>    — 1=uint8, 2=int16, 4=int32, 5=float32, 6=float64
//   scale_factor <f>

// -- Inveon header value transforms (mirror InveonReader.java helpers) --
// Enumeration data is taken from the comments of the .hdr files.

fn inveon_transform_model(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(2000) => "Primate",
        Ok(2001) => "Rodent",
        Ok(2002) => "microPET2",
        Ok(2500) => "Focus_220",
        Ok(2501) => "Focus_120",
        Ok(3000) => "mCAT",
        Ok(3500) => "mCATII",
        Ok(4000) => "mSPECT",
        Ok(5000) => "Inveon_Dedicated_PET",
        Ok(5001) => "Inveon_MM_Platform",
        Ok(6000) => "MR_PET_Head_Insert",
        Ok(8000) => "Tuebingen_PET_MR",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_modality(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(0) => "PET acquisition",
        Ok(1) => "CT acquisition",
        Ok(2) => "SPECT acquisition",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_modality_configuration(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(3000) => "mCAT",
        Ok(3500) => "mCATII",
        Ok(3600) => "Inveon_MM_Std_CT",
        Ok(3601) => "Inveon_MM_HiRes_Std_CT",
        Ok(3602) => "Inveon_MM_Std_LFOV_CT",
        Ok(3603) => "Inveon_MM_HiRes_LFOV_CT",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_file_type(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "List mode",
        Ok(2) => "Sinogram",
        Ok(3) => "Normalization",
        Ok(4) => "Attenuation correction",
        Ok(5) => "Image data",
        Ok(6) => "Blank data",
        // 7 omitted intentionally
        Ok(8) => "Mu map",
        Ok(9) => "Scatter correction",
        Ok(10) => "Crystal efficiency",
        Ok(11) => "Crystal interference correction",
        Ok(12) => "Transaxial geometric correction",
        Ok(13) => "Axial geometric correction",
        Ok(14) => "CT projection",
        Ok(15) => "SPECT raw projection",
        Ok(16) => "SPECT energy data from projections",
        Ok(17) => "SPECT normalization",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_acquisition_mode(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "Blank",
        Ok(2) => "Emission",
        Ok(3) => "Dynamic",
        Ok(4) => "Gated",
        Ok(5) => "Continuous bed motion",
        Ok(6) => "Singles transmission",
        Ok(7) => "Windowed coincidence transmission",
        Ok(8) => "Non-windowed coincidence transmission",
        Ok(9) => "CT projection",
        Ok(10) => "CT calibration",
        Ok(11) => "SPECT planar projection",
        Ok(12) => "SPECT multi-projection",
        Ok(13) => "SPECT calibration",
        Ok(14) => "SPECT normalization",
        Ok(15) => "SPECT detector setup",
        Ok(16) => "SPECT scout view",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_bed_control(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "Dedicated PET",
        Ok(2) => "microCAT II",
        Ok(3) => "Multimodality bed control",
        Ok(4) => "microPET bed control",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_bed_motion(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "Continuous",
        Ok(2) => "Multiple bed positions",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_registration_available(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "CT",
        Ok(2) => "PET",
        _ => "None",
    }
    .to_string()
}

fn inveon_transform_normalization_applied(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "Point source inversion",
        Ok(2) => "Point source component based",
        Ok(3) => "Cylinder source inversion",
        Ok(4) => "Cylinder source component based",
        Ok(5) => "Dark/bright field log normalization (CT)",
        Ok(6) => "SPECT flood inversion based",
        _ => "None",
    }
    .to_string()
}

fn inveon_transform_recon_algorithm(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "Filtered Backprojection",
        Ok(2) => "OSEM2d",
        Ok(3) => "OSEM3d",
        // 4 and 5 omitted intentionally
        Ok(6) => "OSEM3D followed by MAP or FastMAP",
        Ok(7) => "MAPTR for transmission image",
        Ok(8) => "MAP 3D reconstruction",
        Ok(9) => "Feldkamp cone beam",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_filter(value: &str) -> String {
    let space = match value.find(' ') {
        Some(s) => s,
        None => return "Unknown".to_string(),
    };
    let filter = value[..space].trim().parse::<i32>().unwrap_or(-1);
    let cutoff = format!(" (cutoff = {})", &value[space + 1..]);
    match filter {
        0 => "None".to_string(),
        1 => "Ramp filter (backprojection)".to_string(),
        2 => "First-order Butterworth window".to_string(),
        3 => "Hanning window".to_string(),
        4 => "Hamming window".to_string(),
        5 => "Parzen window".to_string(),
        6 => "Shepp filter".to_string(),
        7 => "Second-order Butterworth window".to_string(),
        _ => format!("Unknown{}", cutoff),
    }
}

fn inveon_transform_subject_orientation(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "Feet first, prone",
        Ok(2) => "Head first, prone",
        Ok(3) => "Feet first, supine",
        Ok(4) => "Head first, supine",
        Ok(5) => "Feet first, right",
        Ok(6) => "Head first, right",
        Ok(7) => "Feet first, left",
        Ok(8) => "Head first, left",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_subject_length_units(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "millimeters",
        Ok(2) => "centimeters",
        Ok(3) => "inches",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_subject_weight_units(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "grams",
        Ok(2) => "ounces",
        Ok(3) => "kilograms",
        Ok(4) => "pounds",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_gantry_rotation(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(0) => "No gantry rotation",
        Ok(1) => "Rotation with discrete steps",
        Ok(2) => "Continuous rotation",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_rotation_direction(value: &str) -> String {
    if value == "0" {
        "Clockwise".to_string()
    } else {
        "Counterclockwise".to_string()
    }
}

fn inveon_transform_ct_warping(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "None",
        Ok(2) => "Bilinear",
        Ok(3) => "Nearest neighbor",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_ct_projection_interpolation(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "Bilinear",
        Ok(2) => "Nearest neighbor",
        _ => "Unknown",
    }
    .to_string()
}

fn inveon_transform_event_type(value: &str) -> String {
    match value.trim().parse::<i32>() {
        Ok(1) => "Singles",
        Ok(2) => "Prompt events (coincidences)",
        Ok(3) => "Delay events",
        Ok(4) => "Trues (prompts - delays)",
        Ok(5) => "Energy spectrum data",
        _ => "Unknown",
    }
    .to_string()
}

/// Parse one non-comment Inveon header line into its captured (key, value)
/// pair, applying the same per-key transform as Java InveonReader.initFile.
/// Returns None for blank/keyless lines (Java's `space < 0` continue).
fn inveon_parse_header_line(line: &str) -> Option<(String, String)> {
    let space = line.find(' ')?;
    let mut key = line[..space].to_string();
    let mut value = line[space + 1..].to_string();

    match key.as_str() {
        "model" => value = inveon_transform_model(&value),
        "modality" => value = inveon_transform_modality(&value),
        "modality_configuration" => value = inveon_transform_modality_configuration(&value),
        "file_type" => value = inveon_transform_file_type(&value),
        "acquisition_mode" => value = inveon_transform_acquisition_mode(&value),
        "bed_control" => value = inveon_transform_bed_control(&value),
        "bed_motion" => value = inveon_transform_bed_motion(&value),
        "registration_available" => value = inveon_transform_registration_available(&value),
        "normalization_applied" => value = inveon_transform_normalization_applied(&value),
        "recon_algorithm" => value = inveon_transform_recon_algorithm(&value),
        "x_filter" | "y_filter" | "z_filter" => value = inveon_transform_filter(&value),
        "subject_orientation" => value = inveon_transform_subject_orientation(&value),
        "subject_length_units" => value = inveon_transform_subject_length_units(&value),
        "subject_weight_units" => value = inveon_transform_subject_weight_units(&value),
        "gantry_rotation" => value = inveon_transform_gantry_rotation(&value),
        "rotation_direction" => value = inveon_transform_rotation_direction(&value),
        "ct_warping" => value = inveon_transform_ct_warping(&value),
        "ct_projection_interpolation" => {
            value = inveon_transform_ct_projection_interpolation(&value)
        }
        "event_type" => value = inveon_transform_event_type(&value),
        "projection" | "ct_projection_center_offset" | "ct_projection_horizontal_bed_offset" => {
            if let Some(s) = value.find(' ') {
                let index = &value[..s];
                let rest = value[s + 1..].to_string();
                key = format!("{} {}", key, index);
                value = rest;
            }
        }
        "user" => {
            if let Some(s) = value.find(' ') {
                key = value[..s].to_string();
                value = value[s + 1..].to_string();
            }
        }
        _ => {}
    }

    Some((key, value))
}

#[allow(clippy::type_complexity)]
fn parse_inveon_header(
    path: &Path,
) -> Result<(
    u32,
    u32,
    u32,
    PixelType,
    u8,
    bool,
    HashMap<String, MetadataValue>,
)> {
    let f = File::open(path).map_err(BioFormatsError::Io)?;
    let reader = BufReader::new(f);

    let mut nx: Option<u32> = None;
    let mut ny: Option<u32> = None;
    let mut nz: Option<u32> = None;
    let mut data_type: Option<i32> = None;
    let mut meta: HashMap<String, MetadataValue> = HashMap::new();

    for line in reader.lines() {
        let line = line.map_err(BioFormatsError::Io)?;
        let t = line.trim();
        if t.starts_with('#') {
            continue;
        }
        let lo = t.to_ascii_lowercase();
        let parts: Vec<&str> = t.split_ascii_whitespace().collect();
        if lo.starts_with("x_dimension") {
            nx = parts.get(1).and_then(|s| s.parse::<u32>().ok());
        } else if lo.starts_with("y_dimension") {
            ny = parts.get(1).and_then(|s| s.parse::<u32>().ok());
        } else if lo.starts_with("z_dimension") {
            nz = parts.get(1).and_then(|s| s.parse::<u32>().ok());
        } else if lo.starts_with("data_type") {
            data_type = parts.get(1).and_then(|s| s.parse::<i32>().ok());
        }

        // Capture the named acquisition scalar (Java addGlobalMeta(key, value)),
        // applying the same per-key transform.
        if let Some((key, value)) = inveon_parse_header_line(t) {
            meta.insert(key, MetadataValue::String(value));
        }
    }
    let nx = nx.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Inveon header missing x_dimension".into())
    })?;
    let ny = ny.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Inveon header missing y_dimension".into())
    })?;
    let nz = nz.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Inveon header missing z_dimension".into())
    })?;
    if nx == 0 || ny == 0 || nz == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "Inveon header has zero image dimensions".into(),
        ));
    }
    let data_type = data_type.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("Inveon header missing data_type".into())
    })?;

    // Per InveonReader.setDataType:
    //   default → INT8, little-endian
    //   2 → INT16  LE
    //   3 → INT32  LE
    //   4 → FLOAT  LE
    //   5 → FLOAT  BE
    //   6 → INT16  BE
    //   7 → INT32  BE
    // (case 1 is not listed, so it falls through to the INT8/LE default.)
    let (pixel_type, bpp, little_endian): (PixelType, u8, bool) = match data_type {
        2 => (PixelType::Int16, 16, true),
        3 => (PixelType::Int32, 32, true),
        4 => (PixelType::Float32, 32, true),
        5 => (PixelType::Float32, 32, false),
        6 => (PixelType::Int16, 16, false),
        7 => (PixelType::Int32, 32, false),
        _ => (PixelType::Int8, 8, true),
    };

    Ok((nx, ny, nz, pixel_type, bpp, little_endian, meta))
}

pub struct InveonReader {
    hdr_path: Option<PathBuf>,
    img_path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
}

impl InveonReader {
    pub fn new() -> Self {
        InveonReader {
            hdr_path: None,
            img_path: None,
            meta: None,
        }
    }
}
impl Default for InveonReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for InveonReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        // Inveon .hdr files could conflict with Analyze; check for .img companion
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase());
        if !matches!(ext.as_deref(), Some("hdr")) {
            return false;
        }
        // Check if a .img companion exists
        let stem = path.file_stem().unwrap_or_default();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));
        parent
            .join(format!("{}.img", stem.to_string_lossy()))
            .exists()
    }

    fn is_this_type_by_bytes(&self, _header: &[u8]) -> bool {
        false
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let stem = path.file_stem().unwrap_or_default();
        let parent = path.parent().unwrap_or_else(|| Path::new("."));

        let hdr_path = if path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("hdr"))
            .unwrap_or(false)
        {
            path.to_path_buf()
        } else {
            parent.join(format!("{}.hdr", stem.to_string_lossy()))
        };
        let img_path = parent.join(format!("{}.img", stem.to_string_lossy()));

        let (nx, ny, nz, pixel_type, bpp, little_endian, hdr_meta) =
            parse_inveon_header(&hdr_path)?;
        let bps = pixel_type.bytes_per_sample() as u64;
        let required_len = (nx as u64)
            .checked_mul(ny as u64)
            .and_then(|px| px.checked_mul(nz as u64))
            .and_then(|px| px.checked_mul(bps))
            .ok_or_else(|| BioFormatsError::Format("Inveon payload size overflows".into()))?;
        let img_len = std::fs::metadata(&img_path)
            .map_err(BioFormatsError::Io)?
            .len();
        if img_len < required_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "Inveon pixel payload is shorter than declared ({img_len} < {required_len})"
            )));
        }

        let mut meta_map: HashMap<String, MetadataValue> = hdr_meta;
        meta_map.insert(
            "format".into(),
            MetadataValue::String("Siemens Inveon".into()),
        );

        self.meta = Some(ImageMetadata {
            size_x: nx,
            size_y: ny,
            size_z: nz,
            size_c: 1,
            size_t: 1,
            pixel_type,
            bits_per_pixel: bpp,
            image_count: nz,
            dimension_order: DimensionOrder::XYZCT,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: little_endian,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        self.hdr_path = Some(hdr_path);
        self.img_path = Some(img_path);
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.hdr_path = None;
        self.img_path = None;
        self.meta = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        1
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
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
        let offset = plane_index as u64 * plane_bytes as u64;
        let img_path = self
            .img_path
            .as_ref()
            .ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(img_path).map_err(BioFormatsError::Io)?;
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
        crop_full_plane("Inveon", &full, meta, 1, x, y, w, h)
    }

    fn open_thumb_bytes(&mut self, plane_index: u32) -> Result<Vec<u8>> {
        let meta = self.meta.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let (tw, th) = (meta.size_x.min(256), meta.size_y.min(256));
        let (tx, ty) = ((meta.size_x - tw) / 2, (meta.size_y - th) / 2);
        self.open_bytes_region(plane_index, tx, ty, tw, th)
    }
}

// ─── Varian FDF MRI ───────────────────────────────────────────────────────────
//
// Varian FDF (Flexible Data Format) stores MRI data.
// The file is a text header followed by binary pixel data.
// The header is a series of C-style declarations:
//   int    ro_size = 256;
//   int    pe_size = 256;
//   int    slices = 16;
//   char   *storage = "float";
//   int    bits = 32;
// The header ends with a 0x0C (form-feed) byte immediately before the pixel data.

/// Split a "{a, b, c}" style array value into trimmed, unquoted elements.
fn parse_fdf_array(value: &str) -> Vec<String> {
    value
        .replace(['{', '}'], "")
        .split(',')
        .map(|s| s.replace('"', "").trim().to_string())
        .collect()
}

fn is_fdf_header(header: &[u8]) -> bool {
    let s = std::str::from_utf8(&header[..header.len().min(32)]).unwrap_or("");
    s.starts_with("#!/usr/local/fdf") || s.starts_with("# FDF")
}

struct FdfHeader {
    size_x: u32,
    size_y: u32,
    size_z: u32,
    size_t: u32,
    pixel_type: PixelType,
    bits_per_pixel: u8,
    little_endian: bool,
    data_offset: u64,
    metadata: HashMap<String, MetadataValue>,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    physical_size_z: Option<f64>,
}

fn fdf_physical_size(length: u32, physical_length: &str, unit: Option<&str>) -> Option<f64> {
    if length == 0 {
        return None;
    }
    let mut size = physical_length.trim().parse::<f64>().ok()? / f64::from(length);
    if unit == Some("cm") {
        size *= 1000.0;
    }
    Some(size)
}

fn parse_fdf_header(path: &Path) -> Result<FdfHeader> {
    let mut f = File::open(path).map_err(BioFormatsError::Io)?;
    // Read up to 8 KiB looking for the 0x0C terminator
    let max = 8192usize;
    let mut buf = vec![0u8; max];
    let n = f.read(&mut buf).map_err(BioFormatsError::Io)?;
    buf.truncate(n);

    let ff_pos = buf.iter().position(|&b| b == 0x0C);
    let (header_bytes, data_offset) = if let Some(pos) = ff_pos {
        (&buf[..pos], (pos + 1) as u64)
    } else {
        (&buf[..n], n as u64)
    };

    let text = String::from_utf8_lossy(header_bytes);

    // Per VarianFDFReader.parseFDF: dimensions come from matrix[]={x,y,z},
    // pixel type from bits + *storage, and endianness from the bigendian key.
    let mut size_x: Option<u32> = None;
    let mut size_y: Option<u32> = None;
    let mut size_z = 1u32;
    let mut size_t = 1u32;
    let mut stored_floats = false;
    let mut bits: Option<u32> = None;
    let mut pixel_type: Option<PixelType> = None;
    // FDF default is big-endian unless "bigendian = 1" sets little-endian.
    // Java only sets littleEndian when a bigendian key is present; the
    // RandomAccessInputStream default is big-endian.
    let mut little_endian = false;
    let mut units: Vec<String> = Vec::new();
    let mut physical_size_x = None;
    let mut physical_size_y = None;
    let mut physical_size_z = None;
    let mut metadata = HashMap::new();

    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if line.starts_with('#') {
            continue;
        }
        // Java: type = line[0..firstSpace]; var = line[firstSpace..'='];
        //       value = line['='+1 .. ';']
        let space = match line.find(' ') {
            Some(s) => s,
            None => continue,
        };
        let eq = match line.find('=') {
            Some(e) => e,
            None => continue,
        };
        if space >= eq {
            continue;
        }
        let var = line[space..eq].trim();
        let value_end = line.find(';').unwrap_or(line.len());
        if eq + 1 > value_end {
            continue;
        }
        let value = line[eq + 1..value_end].trim();

        if var == "*storage" {
            stored_floats = value == "\"float\"";
        }
        if var == "bits" {
            let parsed_bits = value.parse::<u32>().unwrap_or(0);
            bits = Some(parsed_bits);
            pixel_type = Some(match value {
                "8" => PixelType::Uint8,
                "16" => PixelType::Uint16,
                "32" => {
                    if stored_floats {
                        PixelType::Float32
                    } else {
                        PixelType::Uint32
                    }
                }
                _ => {
                    return Err(BioFormatsError::UnsupportedFormat(format!(
                        "Unsupported FDF bits: {}",
                        value
                    )))
                }
            });
        } else if var == "matrix[]" {
            let values = parse_fdf_array(value);
            if let Some(v) = values.first() {
                if let Ok(p) = v.trim().parse::<f64>() {
                    size_x = u32::try_from(p as i64).ok();
                }
            }
            if let Some(v) = values.get(1) {
                if let Ok(p) = v.trim().parse::<f64>() {
                    size_y = u32::try_from(p as i64).ok();
                }
            }
            if let Some(v) = values.get(2) {
                if let Ok(p) = v.trim().parse::<f64>() {
                    size_z = (p as i64).max(1) as u32;
                }
            }
        } else if var == "slices" {
            size_z = value.parse::<u32>().unwrap_or(1).max(1);
        } else if var == "echoes" {
            // Java VarianFDFReader.parseFDF: m.sizeT = echoes.
            size_t = value.parse::<u32>().unwrap_or(1).max(1);
        } else if var == "*abscissa[]" {
            units = parse_fdf_array(value);
        } else if var == "span[]" {
            let values = parse_fdf_array(value);
            if let Some(v) = values.first() {
                physical_size_x =
                    fdf_physical_size(size_x.unwrap_or(0), v, units.first().map(String::as_str));
            }
            if let Some(v) = values.get(1) {
                physical_size_y =
                    fdf_physical_size(size_y.unwrap_or(0), v, units.get(1).map(String::as_str));
            }
            if let Some(v) = values.get(2) {
                physical_size_z = fdf_physical_size(size_z, v, units.get(2).map(String::as_str));
            }
        } else if var == "bigendian" {
            little_endian = value == "0";
        }
        metadata.insert(var.to_string(), MetadataValue::String(value.to_string()));
    }

    let size_x = size_x.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("FDF header missing matrix width".into())
    })?;
    let size_y = size_y.ok_or_else(|| {
        BioFormatsError::UnsupportedFormat("FDF header missing matrix height".into())
    })?;
    if size_x == 0 || size_y == 0 {
        return Err(BioFormatsError::UnsupportedFormat(
            "FDF header has zero image dimensions".into(),
        ));
    }
    let pixel_type = pixel_type
        .ok_or_else(|| BioFormatsError::UnsupportedFormat("FDF header missing bits".into()))?;
    let bits = bits.unwrap_or((pixel_type.bytes_per_sample() * 8) as u32);
    let bpp = bits as u8;

    Ok(FdfHeader {
        size_x,
        size_y,
        size_z,
        size_t,
        pixel_type,
        bits_per_pixel: bpp,
        little_endian,
        data_offset,
        metadata,
        physical_size_x,
        physical_size_y,
        physical_size_z,
    })
}

pub struct VarianFdfReader {
    path: Option<PathBuf>,
    meta: Option<ImageMetadata>,
    data_offset: u64,
    physical_size_x: Option<f64>,
    physical_size_y: Option<f64>,
    physical_size_z: Option<f64>,
}

impl VarianFdfReader {
    pub fn new() -> Self {
        VarianFdfReader {
            path: None,
            meta: None,
            data_offset: 0,
            physical_size_x: None,
            physical_size_y: None,
            physical_size_z: None,
        }
    }
}
impl Default for VarianFdfReader {
    fn default() -> Self {
        Self::new()
    }
}

impl FormatReader for VarianFdfReader {
    fn is_this_type_by_name(&self, path: &Path) -> bool {
        path.extension()
            .and_then(|e| e.to_str())
            .map(|e| e.eq_ignore_ascii_case("fdf"))
            .unwrap_or(false)
    }

    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool {
        is_fdf_header(header)
    }

    fn set_id(&mut self, path: &Path) -> Result<()> {
        let mut sniff = [0u8; 32];
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        let n = f.read(&mut sniff).map_err(BioFormatsError::Io)?;
        if !is_fdf_header(&sniff[..n]) {
            return Err(BioFormatsError::UnsupportedFormat(
                "FDF missing Varian FDF header".into(),
            ));
        }
        let header = parse_fdf_header(path)?;
        let plane_bytes = (header.size_x as u64)
            .checked_mul(header.size_y as u64)
            .and_then(|px| px.checked_mul(header.pixel_type.bytes_per_sample() as u64))
            .ok_or_else(|| BioFormatsError::Format("FDF plane size overflows".into()))?;
        let image_count = header
            .size_z
            .max(1)
            .checked_mul(header.size_t.max(1))
            .ok_or_else(|| BioFormatsError::Format("FDF image count overflows".into()))?;
        let pixel_bytes = (image_count as u64)
            .checked_mul(plane_bytes)
            .ok_or_else(|| BioFormatsError::Format("FDF payload size overflows".into()))?;
        let required_len = header
            .data_offset
            .checked_add(pixel_bytes)
            .ok_or_else(|| BioFormatsError::Format("FDF payload size overflows".into()))?;
        let file_len = f.metadata().map_err(BioFormatsError::Io)?.len();
        if file_len < required_len {
            return Err(BioFormatsError::UnsupportedFormat(format!(
                "FDF pixel payload is shorter than declared ({file_len} < {required_len})"
            )));
        }
        let first_pixel_offset = file_len
            .checked_sub(pixel_bytes)
            .ok_or_else(|| BioFormatsError::Format("FDF payload offset underflows".into()))?;

        let nx = header.size_x;
        let ny = header.size_y;
        let mut meta_map = header.metadata;
        meta_map.insert(
            "format".into(),
            MetadataValue::String("Varian FDF MRI".into()),
        );

        // Java VarianFDFReader: imageCount = sizeZ * sizeC * sizeT.
        self.meta = Some(ImageMetadata {
            size_x: nx,
            size_y: ny,
            size_z: header.size_z,
            size_c: 1,
            size_t: header.size_t,
            pixel_type: header.pixel_type,
            bits_per_pixel: header.bits_per_pixel,
            image_count,
            // Java VarianFDFReader uses dimensionOrder "XYTZC".
            dimension_order: DimensionOrder::XYTZC,
            is_rgb: false,
            is_interleaved: false,
            is_indexed: false,
            is_little_endian: header.little_endian,
            resolution_count: 1,
            thumbnail: false,
            series_metadata: meta_map,
            lookup_table: None,
            modulo_z: None,
            modulo_c: None,
            modulo_t: None,
        });
        // Java VarianFDFReader builds pixelOffsets from the file tail:
        // in.length() - planeSize * (imageCount - i). This preserves files that
        // carry padding or extra header bytes between the form-feed terminator
        // and the pixel payload.
        self.data_offset = first_pixel_offset;
        self.physical_size_x = header.physical_size_x;
        self.physical_size_y = header.physical_size_y;
        self.physical_size_z = header.physical_size_z;
        self.path = Some(path.to_path_buf());
        Ok(())
    }

    fn close(&mut self) -> Result<()> {
        self.path = None;
        self.meta = None;
        self.physical_size_x = None;
        self.physical_size_y = None;
        self.physical_size_z = None;
        Ok(())
    }
    fn series_count(&self) -> usize {
        1
    }
    fn set_series(&mut self, s: usize) -> Result<()> {
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
        let offset = self.data_offset + plane_index as u64 * plane_bytes as u64;
        let path = self.path.as_ref().ok_or(BioFormatsError::NotInitialized)?;
        let mut f = File::open(path).map_err(BioFormatsError::Io)?;
        f.seek(SeekFrom::Start(offset))
            .map_err(BioFormatsError::Io)?;
        let mut buf = vec![0u8; plane_bytes];
        f.read_exact(&mut buf).map_err(BioFormatsError::Io)?;

        // Java VarianFDFReader.openBytes flips the rows vertically
        // (lower-left origin → top-left origin).
        let row = meta.size_x as usize * bps;
        let h = meta.size_y as usize;
        let mut row_buf = vec![0u8; row];
        for r in 0..h / 2 {
            let src = r * row;
            let dest = (h - r - 1) * row;
            row_buf.copy_from_slice(&buf[src..src + row]);
            buf.copy_within(dest..dest + row, src);
            buf[dest..dest + row].copy_from_slice(&row_buf);
        }
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
        crop_full_plane("FDF", &full, meta, 1, x, y, w, h)
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
        let image = &mut ome.images[0];
        image.physical_size_x = self.physical_size_x;
        image.physical_size_y = self.physical_size_y;
        image.physical_size_z = self.physical_size_z;
        Some(ome)
    }
}

#[cfg(test)]
mod clinical_metadata_tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    // MetadataValue does not derive PartialEq, so compare via its Display form.
    fn meta_str(map: &HashMap<String, MetadataValue>, key: &str) -> Option<String> {
        map.get(key).map(|v| v.to_string())
    }

    fn tmp_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bioformats_clinical_{}_{}_{}",
            std::process::id(),
            nanos,
            name
        ))
    }

    #[test]
    fn ecat7_rejects_non_java_magic_versions() {
        let reader = Ecat7Reader::new();
        assert!(reader.is_this_type_by_bytes(b"MATRIX72v\0\0\0\0\0"));
        assert!(reader.is_this_type_by_bytes(b"MATRIX70v\0\0\0\0\0"));
        assert!(!reader.is_this_type_by_bytes(b"MATRIX99v\0\0\0\0\0"));

        let mut buf = vec![0u8; 1536 + 2];
        buf[..9].copy_from_slice(b"MATRIX99v");
        buf[352..354].copy_from_slice(&1i16.to_be_bytes());
        buf[354..356].copy_from_slice(&1i16.to_be_bytes());
        buf[1024..1026].copy_from_slice(&6i16.to_be_bytes());
        buf[1028..1030].copy_from_slice(&1i16.to_be_bytes());
        buf[1030..1032].copy_from_slice(&1i16.to_be_bytes());

        let path = tmp_path("bad_ecat_magic.v");
        std::fs::write(&path, &buf).unwrap();
        let mut reader = Ecat7Reader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(err.to_string().contains("MATRIX7[012]v"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ecat7_captures_named_header_keys() {
        // Build a minimal valid ECAT7 file (main hdr 512 + dir 512 + subhdr).
        let mut buf = vec![0u8; 1536 + (2 * 2)];
        buf[..14].copy_from_slice(b"MATRIX72v\0\0\0\0\0");
        // originalPath[32] @14
        buf[14..14 + 5].copy_from_slice(b"/scan");
        // version @46, systemType @48, fileType @50
        buf[46..48].copy_from_slice(&3i16.to_be_bytes());
        buf[48..50].copy_from_slice(&962i16.to_be_bytes());
        buf[50..52].copy_from_slice(&1i16.to_be_bytes());
        // serialNumber[10] @52
        buf[52..52 + 4].copy_from_slice(b"SN42");
        // scanStart @62 (i32)
        buf[62..66].copy_from_slice(&123456i32.to_be_bytes());
        // isotopeName[8] @66
        buf[66..66 + 4].copy_from_slice(b"F-18");
        // isotopeHalflife @74 (f32)
        buf[74..78].copy_from_slice(&6586.2f32.to_be_bytes());
        // radioPharmaceutical[32] @78
        buf[78..78 + 3].copy_from_slice(b"FDG");
        // gantryTilt @110 (f32)
        buf[110..114].copy_from_slice(&12.5f32.to_be_bytes());
        // sourceType @128 (i16)
        buf[128..130].copy_from_slice(&3i16.to_be_bytes());
        // Patient/study block: studyType[12]@154, patientID[16]@166,
        // patientName[32]@182, patientSex[1]@214, facilityName[20]@332.
        buf[154..154 + 5].copy_from_slice(b"brain");
        buf[166..166 + 4].copy_from_slice(b"P007");
        buf[182..182 + 7].copy_from_slice(b"Jane Q.");
        buf[214..215].copy_from_slice(b"F");
        buf[332..332 + 4].copy_from_slice(b"UMEA");
        // sizeZ @352, sizeT @354
        buf[352..354].copy_from_slice(&1i16.to_be_bytes());
        buf[354..356].copy_from_slice(&1i16.to_be_bytes());
        // Acquisition/scan block: numGates@356, numBedPositions@358,
        // bedPositions[15]@364 (first entry), planeSeparation@424.
        buf[356..358].copy_from_slice(&4i16.to_be_bytes());
        buf[358..360].copy_from_slice(&2i16.to_be_bytes());
        buf[364..368].copy_from_slice(&17.5f32.to_be_bytes());
        buf[424..428].copy_from_slice(&3.25f32.to_be_bytes());
        // subheader @1024: dataType, numDimensions, sizeX, sizeY
        buf[1024..1026].copy_from_slice(&6i16.to_be_bytes());
        buf[1026..1028].copy_from_slice(&3i16.to_be_bytes());
        buf[1028..1030].copy_from_slice(&1i16.to_be_bytes());
        buf[1030..1032].copy_from_slice(&1i16.to_be_bytes());
        // subheader scaleFactor @1050 (f32), frameDuration @1070 (i32)
        buf[1050..1054].copy_from_slice(&2.5f32.to_be_bytes());
        buf[1070..1074].copy_from_slice(&3000i32.to_be_bytes());
        // image-subheader tail: filterCode @1078 (i16), zResolution @1088
        // (f32), reconType @1260 (i16).
        buf[1078..1080].copy_from_slice(&7i16.to_be_bytes());
        buf[1088..1092].copy_from_slice(&4.5f32.to_be_bytes());
        buf[1260..1262].copy_from_slice(&5i16.to_be_bytes());

        let path = tmp_path("ecat.v");
        std::fs::write(&path, &buf).unwrap();
        let mut reader = Ecat7Reader::new();
        reader.set_id(&path).unwrap();
        let meta = reader.metadata();

        assert_eq!(
            meta_str(&meta.series_metadata, "Isotope Name").as_deref(),
            Some("F-18")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "System type").as_deref(),
            Some("962")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Serial number").as_deref(),
            Some("SN42")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Scan start").as_deref(),
            Some("123456")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Original path").as_deref(),
            Some("/scan")
        );
        assert!(meta.series_metadata.contains_key("Isotope half-life"));

        // Patient/study header block (Java addGlobalMeta).
        assert_eq!(
            meta_str(&meta.series_metadata, "Study type").as_deref(),
            Some("brain")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Patient ID").as_deref(),
            Some("P007")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Patient name").as_deref(),
            Some("Jane Q.")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Patient sex").as_deref(),
            Some("F")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Facility name").as_deref(),
            Some("UMEA")
        );
        assert!(meta.series_metadata.contains_key("Patient dexterity"));

        // Image-subheader and adjacent named scalars (Java addGlobalMeta).
        assert_eq!(
            meta_str(&meta.series_metadata, "Radiopharmaceutical").as_deref(),
            Some("FDG")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Gantry tilt").as_deref(),
            Some("12.5")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Source type").as_deref(),
            Some("3")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Scale factor").as_deref(),
            Some("2.5")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Frame duration").as_deref(),
            Some("3000")
        );

        // Acquisition/scan-subheader block (Java addGlobalMeta /
        // addGlobalMetaList).
        assert_eq!(
            meta_str(&meta.series_metadata, "Number of gates").as_deref(),
            Some("4")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Number of bed positions").as_deref(),
            Some("2")
        );
        // 15 bed positions flatten to 2-digit "Bed position #01".."#15".
        assert_eq!(
            meta_str(&meta.series_metadata, "Bed position #01").as_deref(),
            Some("17.5")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Plane separation").as_deref(),
            Some("3.25")
        );
        assert!(meta.series_metadata.contains_key("Lower threshold"));
        assert!(meta.series_metadata.contains_key("Acquistion mode"));
        assert!(meta.series_metadata.contains_key("Fill CTI #1"));

        // Image-subheader tail block (Java addGlobalMeta).
        assert_eq!(
            meta_str(&meta.series_metadata, "Filter code").as_deref(),
            Some("7")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Z resolution").as_deref(),
            Some("4.5")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "Recon. type").as_deref(),
            Some("5")
        );
        assert!(meta.series_metadata.contains_key("Annotation"));
        assert!(meta.series_metadata.contains_key("MT (1, 1)"));
        assert!(meta.series_metadata.contains_key("MT (3, 4)"));
        assert!(meta.series_metadata.contains_key("CTI fill"));
        assert!(meta.series_metadata.contains_key("User fill"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn inveon_captures_named_header_keys() {
        let base = tmp_path("scan");
        let hdr = base.with_extension("hdr");
        let img = base.with_extension("img");
        std::fs::write(
            &hdr,
            b"x_dimension 2\ny_dimension 2\nz_dimension 1\ndata_type 2\n\
              modality_configuration 3600\nbed_control 1\nacquisition_mode 2\n\
              institution Acme Labs\ninvestigator Dr Smith\nstudy My Study\n\
              model 5000\nmodality 0\nfile_type 5\n",
        )
        .unwrap();
        std::fs::write(&img, [1u8, 0, 2, 0, 3, 0, 4, 0]).unwrap();

        let mut reader = InveonReader::new();
        reader.set_id(&hdr).expect("inveon set_id");
        let meta = reader.metadata();

        assert_eq!(
            meta_str(&meta.series_metadata, "modality_configuration").as_deref(),
            Some("Inveon_MM_Std_CT")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "bed_control").as_deref(),
            Some("Dedicated PET")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "acquisition_mode").as_deref(),
            Some("Emission")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "institution").as_deref(),
            Some("Acme Labs")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "investigator").as_deref(),
            Some("Dr Smith")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "study").as_deref(),
            Some("My Study")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "model").as_deref(),
            Some("Inveon_Dedicated_PET")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "modality").as_deref(),
            Some("PET acquisition")
        );
        assert_eq!(
            meta_str(&meta.series_metadata, "file_type").as_deref(),
            Some("Image data")
        );
        let _ = std::fs::remove_file(&hdr);
        let _ = std::fs::remove_file(&img);
    }
}
