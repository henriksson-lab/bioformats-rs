use bioformats::{
    BioFormatsError, FormatReader, ImageMetadata, ImageReader, ImageWriter, MetadataValue,
    PixelType,
};
use std::path::Path;

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!("bioformats_fmt_{}", name))
}

fn round_trip(path: &Path, meta: &ImageMetadata, plane: Vec<u8>) -> Vec<u8> {
    ImageWriter::save(path, meta, &[plane]).expect("write failed");
    let mut r = ImageReader::open(path).expect("read failed");
    r.open_bytes(0).expect("open_bytes failed")
}

fn fixed_offset_rhk_header() -> String {
    let mut header = String::new();
    loop {
        let next = format!("x_size 3\ny_size 2\nheader size {}\n", header.len());
        if next.len() == header.len() {
            return next;
        }
        header = next;
    }
}

fn write_i32_le(buf: &mut [u8], offset: usize, value: i32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn fixed_ascii<const N: usize>(text: &str) -> [u8; N] {
    let mut out = [0u8; N];
    let bytes = text.as_bytes();
    out[..bytes.len().min(N)].copy_from_slice(&bytes[..bytes.len().min(N)]);
    out
}

// ---- AFM/SPM/SEM synthetic-pixel audit ------------------------------------

#[test]
fn iplab_preserves_header_and_common_original_metadata_tags() {
    let path = tmp("metadata_tags_common.ipl");
    let mut data = vec![0u8; 96];
    data[..8].copy_from_slice(b"ipl bina");
    write_i32_le(&mut data, 8, 0x100e);
    write_i32_le(&mut data, 12, 2);
    write_i32_le(&mut data, 16, 1);
    write_i32_le(&mut data, 20, 1);
    write_i32_le(&mut data, 24, 1);
    write_i32_le(&mut data, 28, 1);
    write_i32_le(&mut data, 32, 4);
    write_i32_le(&mut data, 36, 7);
    data.extend_from_slice(&[9, 11]);

    let mut note_payload = vec![0u8; 576];
    note_payload[..64].copy_from_slice(&fixed_ascii::<64>("Synthetic descriptor"));
    note_payload[64..576].copy_from_slice(&fixed_ascii::<512>("Synthetic acquisition notes"));
    data.extend_from_slice(b"note");
    data.extend_from_slice(&(note_payload.len() as u32).to_le_bytes());
    data.extend_from_slice(&note_payload);

    let mut head_payload = Vec::new();
    head_payload.extend_from_slice(&3i16.to_le_bytes());
    head_payload.extend_from_slice(&fixed_ascii::<20>("Exposure"));
    data.extend_from_slice(b"head");
    data.extend_from_slice(&(head_payload.len() as u32).to_le_bytes());
    data.extend_from_slice(&head_payload);
    data.extend_from_slice(b"fini");
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::norpix::IplabReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![9, 11]);
    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("iplab.version"),
        Some(MetadataValue::Int(4110))
    ));
    assert!(matches!(
        metadata.get("iplab.data_type"),
        Some(MetadataValue::Int(4))
    ));
    assert!(matches!(
        metadata.get("iplab.color_mode"),
        Some(MetadataValue::Int(7))
    ));
    assert!(
        matches!(metadata.get("Descriptor"), Some(MetadataValue::String(value)) if value == "Synthetic descriptor")
    );
    assert!(
        matches!(metadata.get("Notes"), Some(MetadataValue::String(value)) if value == "Synthetic acquisition notes")
    );
    assert!(
        matches!(metadata.get("Header3"), Some(MetadataValue::String(value)) if value == "Exposure")
    );
    assert!(matches!(
        metadata.get("iplab.tag.note.size"),
        Some(MetadataValue::Int(576))
    ));
}

#[test]
fn topometrix_requires_declared_dimensions() {
    let path = tmp("missing_dims.tfr");
    std::fs::write(&path, b"[Data]\n\x01\x02\x03\x04").unwrap();

    let mut reader = bioformats::formats::afm::TopoMetrixReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("XPoints")),
        "{err:?}"
    );
}

#[test]
fn topometrix_region_crops_real_pixels() {
    let path = tmp("real_crop.tfr");
    let mut data = b"XPoints=3\nYPoints=2\nDataType=uint16\n[Data]\n".to_vec();
    data.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]);
    std::fs::write(&path, data).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let crop = reader.open_bytes_region(0, 1, 0, 2, 2).unwrap();
    assert_eq!(crop, vec![2, 0, 3, 0, 5, 0, 6, 0]);
}

#[test]
fn unisoku_rejects_short_companion_pixels() {
    let path = tmp("short_unisoku.hdr");
    let dat = tmp("short_unisoku.dat");
    std::fs::write(&path, b"XSIZE=2\nYSIZE=2\nBIT=16\n").unwrap();
    std::fs::write(&dat, [1, 2]).unwrap();

    let mut reader = bioformats::formats::afm::UnisokuReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("shorter than declared")),
        "{err:?}"
    );
}

#[test]
fn spm_heuristic_only_readers_reject_raw_files() {
    let cases: Vec<(&str, Box<dyn FormatReader>)> = vec![
        (
            "raw.afm",
            Box::new(bioformats::formats::spm::QuesantReader::new()),
        ),
        (
            "raw.wap",
            Box::new(bioformats::formats::spm::WatopReader::new()),
        ),
        (
            "raw.vgsam",
            Box::new(bioformats::formats::spm::VgSamReader::new()),
        ),
        (
            "raw.ubm",
            Box::new(bioformats::formats::spm::UbmReader::new()),
        ),
        (
            "raw.xqd",
            Box::new(bioformats::formats::spm::SeikoReader::new()),
        ),
    ];

    for (name, mut reader) in cases {
        let path = tmp(name);
        std::fs::write(&path, [0u8; 32]).unwrap();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("refusing heuristic dimensions")),
            "{name}: {err:?}"
        );
    }
}

#[test]
fn rhk_requires_header_dimensions_and_crops_real_pixels() {
    let missing = tmp("missing_dims.sm3");
    std::fs::write(&missing, b"header size 0\n\x01\x00\x02\x00").unwrap();
    let mut reader = bioformats::formats::spm::RhkReader::new();
    let err = reader.set_id(&missing).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(message) if message.contains("missing image dimensions"))
    );

    let path = tmp("rhktest.sm3");
    let mut data = fixed_offset_rhk_header().into_bytes();
    data.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]);
    std::fs::write(&path, data).unwrap();
    let mut reader = bioformats::formats::spm::RhkReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 0, 3, 0, 5, 0, 6, 0]
    );
}

#[test]
fn inr_rejects_missing_required_header_fields() {
    let path = tmp("missing_fields.inr");
    let mut data = vec![0u8; 256];
    data[..13].copy_from_slice(b"#INRIMAGE-4#{");
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::sem::InrReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(matches!(err, BioFormatsError::UnsupportedFormat(message) if message.contains("XDIM")));
}

#[test]
fn inr_region_crops_real_pixels() {
    let path = tmp("crop.inr");
    let mut header = b"#INRIMAGE-4#{\nXDIM=3\nYDIM=2\nZDIM=1\nVDIM=1\nPIXSIZE=8 bits\nTYPE=unsigned fixed\nCPU=pc\n".to_vec();
    header.resize(256, b'\n');
    header.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
    std::fs::write(&path, header).unwrap();

    let mut reader = bioformats::formats::sem::InrReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 3, 5, 6]
    );
}

#[test]
fn sem_heuristic_only_readers_reject_raw_files() {
    let cases: Vec<(&str, Box<dyn FormatReader>)> = vec![
        (
            "raw.dat",
            Box::new(bioformats::formats::sem::JeolReader::new()),
        ),
        (
            "raw.lms",
            Box::new(bioformats::formats::sem::ZeissLmsReader::new()),
        ),
    ];

    for (name, mut reader) in cases {
        let path = tmp(name);
        std::fs::write(&path, [0u8; 32]).unwrap();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("refusing heuristic dimensions")),
            "{name}: {err:?}"
        );
    }
}

#[test]
fn hitachi_region_crops_real_pixels_from_declared_header() {
    let path = tmp("hitachi.hiv");
    let mut data = vec![0u8; 512];
    data[4..8].copy_from_slice(&3u32.to_le_bytes());
    data[8..12].copy_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]);
    std::fs::write(&path, data).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 0, 3, 0, 5, 0, 6, 0]
    );
}

// ---- ICS -------------------------------------------------------------------

#[test]
fn ics_round_trip_gray8() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 8;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let data: Vec<u8> = (0..64).collect();
    let rb = round_trip(&tmp("gray8.ics"), &meta, data.clone());
    assert_eq!(rb, data);
}

#[test]
fn ics_round_trip_gray16() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint16;
    meta.bits_per_pixel = 16;
    meta.image_count = 1;

    let data: Vec<u8> = (0u16..16).flat_map(|v| v.to_le_bytes()).collect();
    let rb = round_trip(&tmp("gray16.ics"), &meta, data.clone());
    assert_eq!(rb, data);
}

#[test]
fn ics1_uses_explicit_companion_filename() {
    let ics = tmp("explicit_companion.ics");
    let companion = tmp("explicit_companion_pixels.ids");
    let derived = tmp("explicit_companion.ids");
    let _ = std::fs::remove_file(&derived);

    let header = format!(
        "ics_version\t1.0\nfilename\t{}\nlayout\torder\tbits x y\nlayout\tsizes\t8 2 2\nlayout\tsignificant_bits\t8\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t1 2 3 4\nrepresentation\tcompression\tuncompressed\n",
        companion.file_name().unwrap().to_string_lossy()
    );
    std::fs::write(&ics, header).unwrap();
    std::fs::write(&companion, [1, 2, 3, 4]).unwrap();

    let mut reader = ImageReader::open(&ics).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
}

#[test]
fn ics_big_endian_pixels_are_byte_swapped() {
    let ics = tmp("big_endian_ics1.ics");
    let companion = tmp("big_endian_ics1.ids");

    let header = format!(
        "ics_version\t1.0\nfilename\t{}\nlayout\torder\tbits x y\nlayout\tsizes\t16 2 1\nlayout\tsignificant_bits\t16\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t2 1\nrepresentation\tcompression\tuncompressed\n",
        companion.file_name().unwrap().to_string_lossy()
    );
    std::fs::write(&ics, header).unwrap();
    std::fs::write(&companion, [0x12, 0x34, 0xab, 0xcd]).unwrap();

    let mut reader = ImageReader::open(&ics).unwrap();
    assert!(!reader.metadata().is_little_endian);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![0x34, 0x12, 0xcd, 0xab]);
}

// ---- MRC -------------------------------------------------------------------

#[test]
fn mrc_round_trip_gray8() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 8;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let data: Vec<u8> = (0..64).collect();
    ImageWriter::save(&tmp("test.mrc"), &meta, &[data.clone()]).unwrap();
    let mut r = ImageReader::open(&tmp("test.mrc")).unwrap();
    let rb = r.open_bytes(0).unwrap();
    // MRC flips rows; after double-flip (write+read) data should be identical
    assert_eq!(rb, data);
}

#[test]
fn mrc_round_trip_float32() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Float32;
    meta.bits_per_pixel = 32;
    meta.image_count = 1;

    let data: Vec<u8> = (0u32..16).flat_map(|v| (v as f32).to_le_bytes()).collect();
    ImageWriter::save(&tmp("float.mrc"), &meta, &[data.clone()]).unwrap();
    let mut r = ImageReader::open(&tmp("float.mrc")).unwrap();
    let rb = r.open_bytes(0).unwrap();
    assert_eq!(rb, data);
}

// ---- FITS ------------------------------------------------------------------

fn fits_header_record(key: &str, value: Option<&str>) -> [u8; 80] {
    let mut rec = [b' '; 80];
    let key_bytes = key.as_bytes();
    rec[..key_bytes.len().min(8)].copy_from_slice(&key_bytes[..key_bytes.len().min(8)]);
    if let Some(value) = value {
        rec[8] = b'=';
        let value_bytes = value.as_bytes();
        rec[10..10 + value_bytes.len().min(70)]
            .copy_from_slice(&value_bytes[..value_bytes.len().min(70)]);
    }
    rec
}

fn write_fits(path: &Path, hdus: Vec<(Vec<[u8; 80]>, Vec<u8>)>) {
    let mut bytes = Vec::new();
    for (mut records, data) in hdus {
        records.push(fits_header_record("END", None));
        for rec in records {
            bytes.extend_from_slice(&rec);
        }
        let header_pad = (2880 - (bytes.len() % 2880)) % 2880;
        bytes.extend(std::iter::repeat(b' ').take(header_pad));
        bytes.extend_from_slice(&data);
        let data_pad = (2880 - (data.len() % 2880)) % 2880;
        bytes.extend(std::iter::repeat(0).take(data_pad));
    }
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn fits_round_trip_gray8() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 8;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let data: Vec<u8> = (0..64).collect();
    ImageWriter::save(&tmp("test.fits"), &meta, &[data.clone()]).unwrap();
    let mut r = ImageReader::open(&tmp("test.fits")).unwrap();
    let rb = r.open_bytes(0).unwrap();
    assert_eq!(rb, data);
}

#[test]
fn fits_multi_plane() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Uint8;
    meta.size_z = 3;
    meta.image_count = 3;

    let planes: Vec<Vec<u8>> = (0u8..3).map(|p| vec![p * 50; 16]).collect();
    ImageWriter::save(&tmp("stack.fits"), &meta, &planes).unwrap();
    let mut r = ImageReader::open(&tmp("stack.fits")).unwrap();
    assert_eq!(r.metadata().image_count, 3);
    for p in 0u8..3 {
        let plane = r.open_bytes(p as u32).unwrap();
        assert!(plane.iter().all(|&b| b == p * 50));
    }
}

#[test]
fn fits_applies_bzero_unsigned_16_scaling() {
    let path = tmp("bzero_u16.fits");
    let raw: Vec<u8> = [-32768i16, -1, 0, 32767]
        .into_iter()
        .flat_map(i16::to_be_bytes)
        .collect();
    write_fits(
        &path,
        vec![(
            vec![
                fits_header_record("SIMPLE", Some("                   T")),
                fits_header_record("BITPIX", Some("                  16")),
                fits_header_record("NAXIS", Some("                   2")),
                fits_header_record("NAXIS1", Some("                   4")),
                fits_header_record("NAXIS2", Some("                   1")),
                fits_header_record("BZERO", Some("             32768.0")),
                fits_header_record("BSCALE", Some("                 1.0")),
            ],
            raw,
        )],
    );

    let mut r = ImageReader::open(&path).unwrap();
    assert_eq!(r.metadata().pixel_type, PixelType::Uint16);
    assert!(r.metadata().is_little_endian);
    let values: Vec<u16> = r
        .open_bytes(0)
        .unwrap()
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect();
    assert_eq!(values, vec![0, 32767, 32768, 65535]);
}

#[test]
fn fits_applies_nontrivial_bscale_as_float32() {
    let path = tmp("bscale_float.fits");
    let raw: Vec<u8> = [-2i16, 0, 4]
        .into_iter()
        .flat_map(i16::to_be_bytes)
        .collect();
    write_fits(
        &path,
        vec![(
            vec![
                fits_header_record("SIMPLE", Some("                   T")),
                fits_header_record("BITPIX", Some("                  16")),
                fits_header_record("NAXIS", Some("                   2")),
                fits_header_record("NAXIS1", Some("                   3")),
                fits_header_record("NAXIS2", Some("                   1")),
                fits_header_record("BZERO", Some("                10.0")),
                fits_header_record("BSCALE", Some("                 0.5")),
            ],
            raw,
        )],
    );

    let mut r = ImageReader::open(&path).unwrap();
    assert_eq!(r.metadata().pixel_type, PixelType::Float32);
    let values: Vec<f32> = r
        .open_bytes(0)
        .unwrap()
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect();
    assert_eq!(values, vec![9.0, 10.0, 12.0]);
}

#[test]
fn fits_reads_first_image_extension_when_primary_is_empty() {
    let path = tmp("image_extension.fits");
    write_fits(
        &path,
        vec![
            (
                vec![
                    fits_header_record("SIMPLE", Some("                   T")),
                    fits_header_record("BITPIX", Some("                   8")),
                    fits_header_record("NAXIS", Some("                   0")),
                ],
                Vec::new(),
            ),
            (
                vec![
                    fits_header_record("XTENSION", Some("'IMAGE   '")),
                    fits_header_record("BITPIX", Some("                   8")),
                    fits_header_record("NAXIS", Some("                   2")),
                    fits_header_record("NAXIS1", Some("                   2")),
                    fits_header_record("NAXIS2", Some("                   2")),
                    fits_header_record("PCOUNT", Some("                   0")),
                    fits_header_record("GCOUNT", Some("                   1")),
                ],
                vec![5, 6, 7, 8],
            ),
        ],
    );

    let mut r = ImageReader::open(&path).unwrap();
    assert_eq!(r.metadata().size_x, 2);
    assert_eq!(r.metadata().size_y, 2);
    assert_eq!(r.open_bytes(0).unwrap(), vec![5, 6, 7, 8]);
}

// ---- NRRD ------------------------------------------------------------------

#[test]
fn nrrd_round_trip_gray8() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 8;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let data: Vec<u8> = (0..64).collect();
    let rb = round_trip(&tmp("test.nrrd"), &meta, data.clone());
    assert_eq!(rb, data);
}

#[test]
fn nrrd_round_trip_float32() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 4;
    meta.size_y = 4;
    meta.pixel_type = PixelType::Float32;
    meta.bits_per_pixel = 32;
    meta.image_count = 1;

    let data: Vec<u8> = (0u32..16).flat_map(|v| (v as f32).to_le_bytes()).collect();
    let rb = round_trip(&tmp("float.nrrd"), &meta, data.clone());
    assert_eq!(rb, data);
}

#[test]
fn nrrd_rgb_kind_uses_leading_vector_axis_as_channels() {
    let path = tmp("rgb_kind.nrrd");
    let data = vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 9, 8, 7];
    let mut bytes = b"NRRD0004
type: uint8
dimension: 3
sizes: 3 2 2
kinds: RGB-color domain domain
encoding: raw

"
    .to_vec();
    bytes.extend_from_slice(&data);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 3);
    assert!(reader.metadata().is_rgb);
    assert_eq!(reader.open_bytes(0).unwrap(), data);
}

#[test]
fn nrrd_space_directions_none_axis_becomes_channels() {
    let path = tmp("space_directions_rgb.nrrd");
    let data = vec![1, 2, 3, 4, 5, 6];
    let mut bytes = b"NRRD0004
type: uint8
dimension: 3
sizes: 3 2 1
space directions: none (1,0) (0,1)
encoding: raw

"
    .to_vec();
    bytes.extend_from_slice(&data);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 1);
    assert_eq!(reader.metadata().size_c, 3);
    assert_eq!(reader.open_bytes(0).unwrap(), data);
}

#[test]
fn nrrd_time_kind_expands_image_count() {
    let path = tmp("time_kind.nrrd");
    let data: Vec<u8> = (0..12).collect();
    let mut bytes = b"NRRD0004
type: uint8
dimension: 4
sizes: 2 1 2 3
kinds: domain domain domain time
encoding: raw

"
    .to_vec();
    bytes.extend_from_slice(&data);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 1);
    assert_eq!(reader.metadata().size_z, 2);
    assert_eq!(reader.metadata().size_t, 3);
    assert_eq!(reader.metadata().image_count, 6);
    assert_eq!(reader.open_bytes(4).unwrap(), vec![8, 9]);
}

#[test]
fn nrrd_detached_list_reads_one_file_per_plane() {
    let path = tmp("detached_list.nhdr");
    let plane0 = tmp("detached_list_0.raw");
    let plane1 = tmp("detached_list_1.raw");
    std::fs::write(&plane0, [1, 2]).unwrap();
    std::fs::write(&plane1, [3, 4]).unwrap();
    let header = format!(
        "NRRD0004
type: uint8
dimension: 3
sizes: 2 1 2
kinds: domain domain domain
encoding: raw
data file: LIST
{}
{}

",
        plane0.file_name().unwrap().to_string_lossy(),
        plane1.file_name().unwrap().to_string_lossy()
    );
    std::fs::write(&path, header).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![3, 4]);
}

#[test]
fn nrrd_detached_data_honors_line_and_byte_skip() {
    let path = tmp("skip.nhdr");
    let raw = tmp("skip.raw");
    std::fs::write(&raw, b"skip this\nand this\nX\x05\x06").unwrap();
    let header = format!(
        "NRRD0004
type: uint8
dimension: 2
sizes: 2 1
kinds: domain domain
encoding: raw
data file: {}
line skip: 2
byte skip: 1

",
        raw.file_name().unwrap().to_string_lossy()
    );
    std::fs::write(&path, header).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![5, 6]);
}

// ---- MetaImage (MHA) -------------------------------------------------------

#[test]
fn metaimage_mha_round_trip() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 8;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let data: Vec<u8> = (0..64).collect();
    let rb = round_trip(&tmp("test.mha"), &meta, data.clone());
    assert_eq!(rb, data);
}

#[test]
fn metaimage_mhd_round_trip() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 8;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let data: Vec<u8> = (0..64).collect();
    let rb = round_trip(&tmp("test.mhd"), &meta, data.clone());
    assert_eq!(rb, data);
}

// ---- OME-XML ---------------------------------------------------------------

#[test]
fn ome_xml_int8_preserves_signed_pixel_type() {
    let path = tmp("signed_int8.ome");
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME><Image ID="Image:0"><Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="int8" SizeX="2" SizeY="2" SizeZ="1" SizeC="1" SizeT="1"><BinData Length="4">/wCAAQ==</BinData></Pixels></Image></OME>"#;
    std::fs::write(&path, xml).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().pixel_type, PixelType::Int8);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![255, 0, 128, 1]);
}

#[test]
fn ome_xml_uses_bindata_big_endian_when_pixels_omits_it() {
    let path = tmp("bindata_big_endian.ome");
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME><Image ID="Image:0"><Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint16" SizeX="1" SizeY="1" SizeZ="1" SizeC="1" SizeT="1"><BinData Length="2" BigEndian="true">EjQ=</BinData></Pixels></Image></OME>"#;
    std::fs::write(&path, xml).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert!(!reader.metadata().is_little_endian);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![0x12, 0x34]);
}

#[test]
fn ome_xml_slices_multichannel_bindata_with_samples_per_pixel() {
    let path = tmp("rgb_bindata.ome");
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME><Image ID="Image:0"><Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint8" SizeX="1" SizeY="1" SizeZ="2" SizeC="1" SizeT="1"><Channel ID="Channel:0:0" SamplesPerPixel="3"/><BinData Length="6">AQIDBAUG</BinData></Pixels></Image></OME>"#;
    std::fs::write(&path, xml).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_c, 3);
    assert!(reader.metadata().is_rgb);
    assert!(reader.metadata().is_interleaved);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![4, 5, 6]);
}

#[test]
fn ome_xml_exposes_multiple_images_as_series() {
    let path = tmp("two_images.ome");
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME>
  <ome:Image ID="Image:0" xmlns:ome="http://www.openmicroscopy.org/Schemas/OME/2016-06"><ome:Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint8" SizeX="1" SizeY="1" SizeZ="1" SizeC="1" SizeT="1"><ome:BinData Length="1">Cw==</ome:BinData></ome:Pixels></ome:Image>
  <ome:Image ID="Image:1" xmlns:ome="http://www.openmicroscopy.org/Schemas/OME/2016-06"><ome:Pixels ID="Pixels:1" DimensionOrder="XYZCT" Type="uint8" SizeX="2" SizeY="1" SizeZ="1" SizeC="1" SizeT="1"><ome:BinData Length="2">Fhc=</ome:BinData></ome:Pixels></ome:Image>
</OME>"#;
    std::fs::write(&path, xml).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.series_count(), 2);
    assert_eq!(reader.metadata().size_x, 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![11]);

    reader.set_series(1).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![22, 23]);
}

// ---- DICOM -----------------------------------------------------------------

#[test]
fn dicom_without_preamble_reads_explicit_vr_little_endian() {
    let path = tmp("no_preamble.dcm");
    let mut bytes = Vec::new();

    fn elem(bytes: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
        bytes.extend_from_slice(&group.to_le_bytes());
        bytes.extend_from_slice(&element.to_le_bytes());
        bytes.extend_from_slice(vr);
        if matches!(
            vr,
            b"OB" | b"OD" | b"OF" | b"OL" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
        ) {
            bytes.extend_from_slice(&0u16.to_le_bytes());
            bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
        } else {
            bytes.extend_from_slice(&(value.len() as u16).to_le_bytes());
        }
        bytes.extend_from_slice(value);
    }

    elem(&mut bytes, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    elem(&mut bytes, 0x0028, 0x0010, b"US", &2u16.to_le_bytes());
    elem(&mut bytes, 0x0028, 0x0011, b"US", &2u16.to_le_bytes());
    elem(&mut bytes, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    elem(&mut bytes, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    elem(&mut bytes, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    elem(&mut bytes, 0x7FE0, 0x0010, b"OB", &[1, 2, 3, 4]);

    std::fs::write(&path, bytes).unwrap();
    let mut reader = ImageReader::open(&path).unwrap();

    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
}

fn dicom_elem_explicit(bytes: &mut Vec<u8>, group: u16, element: u16, vr: &[u8; 2], value: &[u8]) {
    bytes.extend_from_slice(&group.to_le_bytes());
    bytes.extend_from_slice(&element.to_le_bytes());
    bytes.extend_from_slice(vr);
    if matches!(
        vr,
        b"OB" | b"OD" | b"OF" | b"OL" | b"OW" | b"SQ" | b"UC" | b"UN" | b"UR" | b"UT"
    ) {
        bytes.extend_from_slice(&0u16.to_le_bytes());
        bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
    } else {
        bytes.extend_from_slice(&(value.len() as u16).to_le_bytes());
    }
    bytes.extend_from_slice(value);
}

fn dicom_sq_undefined_with_undefined_item(bytes: &mut Vec<u8>) {
    bytes.extend_from_slice(&0x0008u16.to_le_bytes());
    bytes.extend_from_slice(&0x1115u16.to_le_bytes());
    bytes.extend_from_slice(b"SQ");
    bytes.extend_from_slice(&0u16.to_le_bytes());
    bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());

    bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
    bytes.extend_from_slice(&0xE000u16.to_le_bytes());
    bytes.extend_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
    dicom_elem_explicit(bytes, 0x0010, 0x0010, b"PN", b"Doe^Jane");

    bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
    bytes.extend_from_slice(&0xE00Du16.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&0xFFFEu16.to_le_bytes());
    bytes.extend_from_slice(&0xE0DDu16.to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
}

fn dicom_elem_implicit(bytes: &mut Vec<u8>, group: u16, element: u16, value: &[u8]) {
    bytes.extend_from_slice(&group.to_le_bytes());
    bytes.extend_from_slice(&element.to_le_bytes());
    bytes.extend_from_slice(&(value.len() as u32).to_le_bytes());
    bytes.extend_from_slice(value);
}

#[test]
fn dicom_skips_undefined_length_explicit_sequence_before_image_tags() {
    let path = tmp("undefined_sequence_before_dimensions.dcm");
    let mut bytes = Vec::new();

    dicom_sq_undefined_with_undefined_item(&mut bytes);
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0010, b"US", &2u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0011, b"US", &2u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x7FE0, 0x0010, b"OB", &[1, 2, 3, 4]);

    std::fs::write(&path, bytes).unwrap();
    let mut reader = ImageReader::open(&path).unwrap();

    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
}

#[test]
fn dicom_without_preamble_falls_back_to_implicit_vr_little_endian() {
    let path = tmp("no_preamble_implicit.dcm");
    let mut bytes = Vec::new();

    dicom_elem_implicit(&mut bytes, 0x0028, 0x0002, &1u16.to_le_bytes());
    dicom_elem_implicit(&mut bytes, 0x0028, 0x0010, &2u16.to_le_bytes());
    dicom_elem_implicit(&mut bytes, 0x0028, 0x0011, &2u16.to_le_bytes());
    dicom_elem_implicit(&mut bytes, 0x0028, 0x0100, &8u16.to_le_bytes());
    dicom_elem_implicit(&mut bytes, 0x0028, 0x0101, &8u16.to_le_bytes());
    dicom_elem_implicit(&mut bytes, 0x0028, 0x0103, &0u16.to_le_bytes());
    dicom_elem_implicit(&mut bytes, 0x7FE0, 0x0010, &[5, 6, 7, 8]);

    std::fs::write(&path, bytes).unwrap();
    let mut reader = ImageReader::open(&path).unwrap();

    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![5, 6, 7, 8]);
}

#[test]
fn dicom_metadata_uses_dictionary_names_and_decodes_value_representations() {
    let path = tmp("metadata_dictionary.dcm");
    let mut bytes = Vec::new();

    dicom_elem_implicit(&mut bytes, 0x0010, 0x0010, b"Doe^Jane");
    dicom_elem_implicit(&mut bytes, 0x0008, 0x103E, b"Metadata parity");
    dicom_elem_implicit(&mut bytes, 0x0018, 0x0050, b"0.75");
    dicom_elem_implicit(&mut bytes, 0x0028, 0x0030, b"0.5\\0.25");
    dicom_elem_implicit(&mut bytes, 0x0028, 0x0002, &1u16.to_le_bytes());
    dicom_elem_implicit(&mut bytes, 0x0028, 0x0010, &2u16.to_le_bytes());
    dicom_elem_implicit(&mut bytes, 0x0028, 0x0011, &2u16.to_le_bytes());
    dicom_elem_implicit(&mut bytes, 0x0028, 0x0100, &8u16.to_le_bytes());
    dicom_elem_implicit(&mut bytes, 0x0028, 0x0101, &8u16.to_le_bytes());
    dicom_elem_implicit(&mut bytes, 0x0028, 0x0103, &0u16.to_le_bytes());
    dicom_elem_implicit(&mut bytes, 0x7FE0, 0x0010, &[1, 2, 3, 4]);

    std::fs::write(&path, bytes).unwrap();
    let reader = ImageReader::open(&path).unwrap();
    let metadata = &reader.metadata().series_metadata;

    assert!(matches!(
        metadata.get("PatientName"),
        Some(MetadataValue::String(value)) if value == "Doe^Jane"
    ));
    assert!(matches!(
        metadata.get("(0010,0010)"),
        Some(MetadataValue::String(value)) if value == "Doe^Jane"
    ));
    assert!(matches!(
        metadata.get("Rows"),
        Some(MetadataValue::String(value)) if value == "2"
    ));
    assert!(matches!(
        metadata.get("PixelSpacing"),
        Some(MetadataValue::String(value)) if value == "0.5\\0.25"
    ));

    let ome = reader.ome_metadata().unwrap();
    let image = &ome.images[0];
    assert_eq!(image.name.as_deref(), Some("Doe^Jane"));
    assert_eq!(image.physical_size_x, Some(250.0));
    assert_eq!(image.physical_size_y, Some(500.0));
    assert_eq!(image.physical_size_z, Some(750.0));
}

#[test]
fn bdv_preserves_companion_xml_original_metadata() {
    let path = tmp("metadata_parity_bdv.h5");
    let xml_path = path.with_extension("xml");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&xml_path);

    let file = hdf5::File::create(&path).unwrap();
    let t0 = file.create_group("t00000").unwrap();
    let s0 = t0.create_group("s00").unwrap();
    let level0 = s0.create_group("0").unwrap();
    level0
        .new_dataset::<u16>()
        .shape([1, 2, 3])
        .create("cells")
        .unwrap()
        .write_raw(&[1u16, 2, 3, 4, 5, 6])
        .unwrap();
    file.create_group("s00")
        .unwrap()
        .new_dataset::<f64>()
        .shape([1, 3])
        .create("resolutions")
        .unwrap()
        .write_raw(&[1.0f64, 1.0, 1.0])
        .unwrap();
    drop(file);

    let xml = r#"<SpimData>
  <SequenceDescription>
    <ViewSetups>
      <ViewSetup><id>0</id><size>3 2 1</size></ViewSetup>
      <ViewSetup><id>1</id><size>3 2 1</size></ViewSetup>
    </ViewSetups>
    <Timepoints type="range"><first>2</first><last>4</last></Timepoints>
  </SequenceDescription>
</SpimData>"#;
    std::fs::write(&xml_path, xml).unwrap();

    let mut reader = bioformats::formats::bdv::BdvReader::new();
    reader.set_id(&path).unwrap();
    let metadata = &reader.metadata().series_metadata;

    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_z, 1);
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.metadata().size_t, 3);
    assert!(matches!(
        metadata.get("bdv_size"),
        Some(MetadataValue::String(value)) if value == "3 2 1"
    ));
    assert!(matches!(
        metadata.get("bdv_timepoint_first"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        metadata.get("bdv_timepoint_last"),
        Some(MetadataValue::Int(4))
    ));
    assert!(matches!(
        metadata.get("bdv_view_setup_count"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        metadata.get("bdv_xml"),
        Some(MetadataValue::String(value)) if value.contains("<SpimData>")
    ));
}

#[test]
fn cellh5_preserves_hdf5_attributes_and_dataset_metadata() {
    let path = tmp("metadata_parity_cellh5.ch5");
    let _ = std::fs::remove_file(&path);

    let file = hdf5::File::create(&path).unwrap();
    file.new_attr_builder()
        .with_data("synthetic assay")
        .create("experiment_name")
        .unwrap();
    let sample = file.create_group("sample").unwrap();
    let plate = sample.create_group("0").unwrap();
    let position = plate.create_group("position").unwrap();
    let well = position.create_group("A01").unwrap();
    let image = well.create_group("image").unwrap();
    let channel = image.create_group("channel").unwrap();
    let ds = channel
        .new_dataset::<u16>()
        .shape([2, 2, 3])
        .create("0")
        .unwrap();
    ds.write_raw(&[1u16, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
        .unwrap();
    ds.new_attr::<u32>()
        .shape(())
        .create("wavelength_nm")
        .unwrap()
        .write_scalar(&488u32)
        .unwrap();
    drop(file);

    let mut reader = bioformats::formats::cellh5::CellH5Reader::new();
    reader.set_id(&path).unwrap();
    let metadata = &reader.metadata().series_metadata;

    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_t, 2);
    assert!(
        matches!(
            metadata.get("cellh5_attr:/@experiment_name"),
            Some(MetadataValue::String(value)) if value == "synthetic assay"
        ),
        "{:?}",
        metadata.get("cellh5_attr:/@experiment_name")
    );
    assert!(matches!(
        metadata.get("cellh5_attr:/sample/0/position/A01/image/channel/0@wavelength_nm"),
        Some(MetadataValue::Int(488))
    ));
    assert!(matches!(
        metadata.get("cellh5_dataset:/sample/0/position/A01/image/channel/0"),
        Some(MetadataValue::String(value))
            if value == "shape=[2, 2, 3]; dtype_size=2"
    ));
}

#[test]
fn dicom_monochrome1_pixels_are_inverted() {
    let path = tmp("monochrome1.dcm");
    let mut bytes = Vec::new();

    dicom_elem_explicit(&mut bytes, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0004, b"CS", b"MONOCHROME1 ");
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0010, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0011, b"US", &3u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x7FE0, 0x0010, b"OB", &[0, 127, 255, 0]);

    std::fs::write(&path, bytes).unwrap();
    let mut reader = ImageReader::open(&path).unwrap();

    assert_eq!(reader.open_bytes(0).unwrap(), vec![255, 128, 0]);
}

#[test]
fn dicom_planar_rgb_pixels_are_returned_interleaved() {
    let path = tmp("planar_rgb.dcm");
    let mut bytes = Vec::new();

    dicom_elem_explicit(&mut bytes, 0x0028, 0x0002, b"US", &3u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0004, b"CS", b"RGB ");
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0006, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0010, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0011, b"US", &2u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x7FE0, 0x0010, b"OB", &[10, 20, 30, 40, 50, 60]);

    std::fs::write(&path, bytes).unwrap();
    let mut reader = ImageReader::open(&path).unwrap();

    assert!(reader.metadata().is_rgb);
    assert!(reader.metadata().is_interleaved);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 30, 50, 20, 40, 60]);
}

#[test]
fn dicom_rejects_mismatched_pixel_data_length() {
    let path = tmp("bad_pixel_length.dcm");
    let mut bytes = Vec::new();

    dicom_elem_explicit(&mut bytes, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0010, b"US", &2u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0011, b"US", &2u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x7FE0, 0x0010, b"OB", &[1, 2, 3]);

    std::fs::write(&path, bytes).unwrap();
    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("mismatched DICOM pixel data length should be rejected"),
        Err(err) => err,
    };

    assert!(matches!(err, BioFormatsError::Format(msg) if msg.contains("shorter than expected")));
}

#[test]
fn dicom_palette_color_pixels_are_expanded_to_rgb() {
    let path = tmp("palette_color.dcm");
    let mut bytes = Vec::new();
    let descriptor = [2u16, 0, 8]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();

    dicom_elem_explicit(&mut bytes, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0004, b"CS", b"PALETTE COLOR ");
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0010, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0011, b"US", &2u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x1101, b"US", &descriptor);
    dicom_elem_explicit(&mut bytes, 0x0028, 0x1102, b"US", &descriptor);
    dicom_elem_explicit(&mut bytes, 0x0028, 0x1103, b"US", &descriptor);
    dicom_elem_explicit(&mut bytes, 0x0028, 0x1201, b"OW", &[10, 20]);
    dicom_elem_explicit(&mut bytes, 0x0028, 0x1202, b"OW", &[30, 40]);
    dicom_elem_explicit(&mut bytes, 0x0028, 0x1203, b"OW", &[50, 60]);
    dicom_elem_explicit(&mut bytes, 0x7FE0, 0x0010, b"OB", &[0, 1]);

    std::fs::write(&path, bytes).unwrap();
    let mut reader = ImageReader::open(&path).unwrap();

    assert_eq!(reader.metadata().size_c, 3);
    assert!(reader.metadata().is_rgb);
    assert!(!reader.metadata().is_indexed);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 30, 50, 20, 40, 60]);
}

#[test]
fn dicom_one_bit_pixels_are_unpacked() {
    let path = tmp("packed_bit.dcm");
    let mut bytes = Vec::new();

    dicom_elem_explicit(&mut bytes, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0010, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0011, b"US", &5u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0100, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0101, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x7FE0, 0x0010, b"OB", &[0b0001_0101]);

    std::fs::write(&path, bytes).unwrap();
    let mut reader = ImageReader::open(&path).unwrap();

    assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
    assert_eq!(reader.metadata().bits_per_pixel, 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 0, 1, 0, 1]);
}

#[test]
fn dicom_twelve_bit_stored_pixels_are_masked() {
    let path = tmp("stored_12bit.dcm");
    let mut bytes = Vec::new();
    let pixels = [0x0000u16, 0x0abcu16, 0xf123u16]
        .into_iter()
        .flat_map(u16::to_le_bytes)
        .collect::<Vec<_>>();

    dicom_elem_explicit(&mut bytes, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0010, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0011, b"US", &3u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0100, b"US", &16u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0101, b"US", &12u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x7FE0, 0x0010, b"OW", &pixels);

    std::fs::write(&path, bytes).unwrap();
    let mut reader = ImageReader::open(&path).unwrap();
    let values = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(2)
        .map(|chunk| u16::from_le_bytes([chunk[0], chunk[1]]))
        .collect::<Vec<_>>();

    assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
    assert_eq!(reader.metadata().bits_per_pixel, 12);
    assert_eq!(values, vec![0x0000, 0x0abc, 0x0123]);
}

// ---- ND2 -------------------------------------------------------------------

fn push_nd2_chunk(bytes: &mut Vec<u8>, name: &str, data: &[u8]) -> u64 {
    let position = bytes.len() as u64;
    bytes.extend_from_slice(&[0xDA, 0xCE, 0xBE, 0x0A]);
    bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(data.len() as u64).to_le_bytes());
    bytes.extend_from_slice(name.as_bytes());
    bytes.extend_from_slice(data);
    position
}

fn write_synthetic_nd2(path: &Path, image_payload: &[u8]) {
    let mut bytes = Vec::new();
    let attr_xml = b"<uiWidth>2</uiWidth><uiHeight>1</uiHeight><uiComp>1</uiComp><uiBpc>8</uiBpc>";
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    push_nd2_chunk(&mut bytes, "ImageDataSeq|0!", image_payload);
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn nd2_chunk_map_finds_non_contiguous_image_chunks() {
    let path = tmp("chunk_map.nd2");
    let mut bytes = Vec::new();

    fn push_chunk(bytes: &mut Vec<u8>, name: &str, data: &[u8]) -> u64 {
        let position = bytes.len() as u64;
        bytes.extend_from_slice(&[0xDA, 0xCE, 0xBE, 0x0A]);
        bytes.extend_from_slice(&(name.len() as u32).to_le_bytes());
        bytes.extend_from_slice(&(data.len() as u64).to_le_bytes());
        bytes.extend_from_slice(name.as_bytes());
        bytes.extend_from_slice(data);
        position
    }

    bytes.extend_from_slice(b"not-a-leading-chunk");
    let attr_xml = b"<uiWidth>1</uiWidth><uiHeight>1</uiHeight><uiComp>1</uiComp><uiBpc>8</uiBpc>";
    let attr_pos = push_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    bytes.extend_from_slice(b"junk-between");
    let image0_pos = push_chunk(&mut bytes, "ImageDataSeq|0!", &[11]);
    bytes.extend_from_slice(b"more-junk");
    let image1_pos = push_chunk(&mut bytes, "ImageDataSeq|1!", &[22]);

    let mut entries = Vec::new();
    for (name, position, data_len) in [
        ("ImageAttributesLV", attr_pos, attr_xml.len() as u64),
        ("ImageDataSeq|0", image0_pos, 1u64),
        ("ImageDataSeq|1", image1_pos, 1u64),
    ] {
        entries.extend_from_slice(name.as_bytes());
        entries.push(b'!');
        entries.extend_from_slice(&position.to_le_bytes());
        let total_len = 16 + name.len() as u64 + 1 + data_len;
        entries.extend_from_slice(&total_len.to_le_bytes());
    }

    let map_pos = push_chunk(&mut bytes, "ChunkMap!", &entries);
    bytes.extend_from_slice(b"ND2 CHUNK MAP SIGNATURE 0000001");
    bytes.push(0);
    bytes.extend_from_slice(&map_pos.to_le_bytes());
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 1);
    assert_eq!(reader.metadata().size_y, 1);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![11]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![22]);
}

#[test]
fn nd2_decodes_raw_frame_with_eight_byte_prefix() {
    let path = tmp("raw_frame_prefix.nd2");
    let mut frame = b"ND2FRAME".to_vec();
    frame.extend_from_slice(&[17, 23]);
    write_synthetic_nd2(&path, &frame);

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![17, 23]);
}

#[test]
fn nd2_decodes_zlib_frame_after_eight_byte_prefix() {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let path = tmp("zlib_frame_prefix.nd2");
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[31, 47]).unwrap();
    let compressed = encoder.finish().unwrap();

    let mut frame = b"ND2FRAME".to_vec();
    frame.extend_from_slice(&compressed);
    write_synthetic_nd2(&path, &frame);

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![31, 47]);
}

#[test]
fn nd2_routes_jpeg2000_frame_by_signature() {
    let path = tmp("jpeg2000_frame_prefix.nd2");
    let mut frame = b"ND2FRAME".to_vec();
    frame.extend_from_slice(&[0xff, 0x4f, 0xff, 0x51, 0, 0, 0, 0]);
    write_synthetic_nd2(&path, &frame);

    let mut reader = ImageReader::open(&path).unwrap();
    let err = reader.open_bytes(0).unwrap_err();
    assert!(matches!(err, BioFormatsError::Codec(msg) if msg.contains("JPEG 2000")));
}

#[test]
fn nd2_rejects_unrecognized_oversized_frame_prefix() {
    let path = tmp("unknown_frame_prefix.nd2");
    write_synthetic_nd2(&path, &[1, 2, 3, 19, 29]);

    let mut reader = ImageReader::open(&path).unwrap();
    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(msg) if msg.contains("unsupported structured frame encoding"))
    );
}

#[test]
fn nd2_accepts_small_raw_frame_trailer_on_large_planes() {
    let path = tmp("raw_frame_trailer.nd2");
    let mut bytes = Vec::new();
    let attr_xml =
        b"<uiWidth>64</uiWidth><uiHeight>16</uiHeight><uiComp>1</uiComp><uiBpc>8</uiBpc>";
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    let mut frame = vec![7u8; 1024];
    frame.extend_from_slice(&[0x55; 69]);
    push_nd2_chunk(&mut bytes, "ImageDataSeq|0!", &frame);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![7u8; 1024]);
}

#[test]
fn nd2_parses_xml_value_attributes() {
    let path = tmp("value_attributes.nd2");
    let mut bytes = Vec::new();
    let attr_xml = br#"<?xml version="1.0"?>
<variant>
  <uiWidth runtype="lx_uint32" value="696"/>
  <uiHeight runtype="lx_uint32" value="520"/>
  <uiComp runtype="lx_uint32" value="1"/>
  <uiBpcInMemory runtype="lx_uint32" value="16"/>
  <uiBpcSignificant runtype="lx_uint32" value="14"/>
  <uiSequenceCount runtype="lx_uint32" value="1"/>
</variant>"#;
    push_nd2_chunk(&mut bytes, "ImageAttributes!", attr_xml);
    push_nd2_chunk(&mut bytes, "ImageDataSeq|0!", &vec![0x34u8; 696 * 520 * 2]);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 696);
    assert_eq!(reader.metadata().size_y, 520);
    assert_eq!(reader.metadata().bits_per_pixel, 16);
    assert_eq!(reader.open_bytes(0).unwrap().len(), 696 * 520 * 2);
}

#[test]
fn nd2_prefers_sensor_user_rectangle_dimensions() {
    let path = tmp("sensor_user_rect.nd2");
    let mut bytes = Vec::new();
    let attr_xml = br#"<?xml version="1.0"?>
<variant>
  <uiWidth runtype="lx_uint32" value="226"/>
  <uiHeight runtype="lx_uint32" value="226"/>
  <uiComp runtype="lx_uint32" value="1"/>
  <uiBpc runtype="lx_uint32" value="16"/>
  <rectSensorUser>
    <left runtype="lx_int32" value="184"/>
    <top runtype="lx_int32" value="174"/>
    <right runtype="lx_int32" value="348"/>
    <bottom runtype="lx_int32" value="330"/>
  </rectSensorUser>
</variant>"#;
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    push_nd2_chunk(&mut bytes, "ImageDataSeq|0!", &vec![0x12u8; 164 * 156 * 2]);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 164);
    assert_eq!(reader.metadata().size_y, 156);
    assert_eq!(reader.open_bytes(0).unwrap().len(), 164 * 156 * 2);
}

#[test]
fn nd2_uses_sensor_user_rectangle_from_metadata_chunk() {
    let path = tmp("sensor_user_rect_metadata.nd2");
    let mut bytes = Vec::new();
    let attr_xml = br#"<?xml version="1.0"?>
<variant>
  <uiWidth runtype="lx_uint32" value="226"/>
  <uiHeight runtype="lx_uint32" value="226"/>
  <uiComp runtype="lx_uint32" value="1"/>
  <uiBpc runtype="lx_uint32" value="16"/>
</variant>"#;
    let metadata_xml = br#"<?xml version="1.0"?>
<variant>
  <rectSensorUser>
    <left runtype="lx_int32" value="184"/>
    <top runtype="lx_int32" value="174"/>
    <right runtype="lx_int32" value="348"/>
    <bottom runtype="lx_int32" value="330"/>
  </rectSensorUser>
</variant>"#;
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    push_nd2_chunk(
        &mut bytes,
        "CustomDataVar|GrabberCameraSettingsV1_0!",
        metadata_xml,
    );
    push_nd2_chunk(&mut bytes, "ImageDataSeq|0!", &vec![0x12u8; 164 * 156 * 2]);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 164);
    assert_eq!(reader.metadata().size_y, 156);
    assert_eq!(reader.open_bytes(0).unwrap().len(), 164 * 156 * 2);
}

#[test]
fn iplab_preserves_post_pixel_tags_as_metadata() {
    let path = tmp("metadata_tags.ipl");
    let mut bytes = vec![0u8; 96];
    bytes[..8].copy_from_slice(b"ipl bina");
    bytes[8..12].copy_from_slice(&1i32.to_le_bytes());
    bytes[12..16].copy_from_slice(&1i32.to_le_bytes());
    bytes[16..20].copy_from_slice(&1i32.to_le_bytes());
    bytes[20..24].copy_from_slice(&1i32.to_le_bytes());
    bytes[24..28].copy_from_slice(&1i32.to_le_bytes());
    bytes[28..32].copy_from_slice(&1i32.to_le_bytes());
    bytes[32..36].copy_from_slice(&4i32.to_le_bytes());
    bytes.push(9);
    bytes.extend_from_slice(b"note");
    bytes.extend_from_slice(&576i32.to_le_bytes());
    let mut note = vec![0u8; 576];
    note[..10].copy_from_slice(b"Descriptor");
    note[64..77].copy_from_slice(b"Acquired note");
    bytes.extend_from_slice(&note);
    bytes.extend_from_slice(b"head");
    bytes.extend_from_slice(&22i32.to_le_bytes());
    bytes.extend_from_slice(&7i16.to_le_bytes());
    let mut label = [0u8; 20];
    label[..10].copy_from_slice(b"HeaderName");
    bytes.extend_from_slice(&label);
    bytes.extend_from_slice(b"fini");
    std::fs::write(&path, bytes).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = &reader.metadata().series_metadata;
    assert_eq!(
        meta.get("Descriptor").map(ToString::to_string),
        Some("Descriptor".to_string())
    );
    assert_eq!(
        meta.get("Notes").map(ToString::to_string),
        Some("Acquired note".to_string())
    );
    assert_eq!(
        meta.get("Header7").map(ToString::to_string),
        Some("HeaderName".to_string())
    );
}

#[test]
fn zvi_preserves_tag_stream_ids_names_and_values() {
    use std::io::Write;

    let path = tmp("metadata_tags.zvi");
    let mut comp = cfb::create(&path).unwrap();
    comp.create_storage_all("/Image/Item(1)/Tags").unwrap();
    {
        let mut stream = comp.create_stream("/Image/CONTENTS").unwrap();
        stream.write_all(&1u32.to_le_bytes()).unwrap();
        stream.write_all(&1u32.to_le_bytes()).unwrap();
        stream.write_all(&1u32.to_le_bytes()).unwrap();
    }
    {
        let mut stream = comp.create_stream("/Image/Item(1)/CONTENTS").unwrap();
        stream.write_all(&0u32.to_le_bytes()).unwrap();
        stream.write_all(&0u32.to_le_bytes()).unwrap();
        stream.write_all(&0u32.to_le_bytes()).unwrap();
        stream.write_all(&0u32.to_le_bytes()).unwrap();
        stream.write_all(&[77]).unwrap();
    }
    {
        let mut tags = Vec::new();
        tags.extend_from_slice(&[0u8; 8]);
        tags.extend_from_slice(&2u32.to_le_bytes());
        for (tag_id, value) in [(1537u32, "Scene title"), (1284u32, "DAPI")] {
            tags.extend_from_slice(&8u16.to_le_bytes());
            tags.extend_from_slice(&(value.len() as u32).to_le_bytes());
            tags.extend_from_slice(value.as_bytes());
            tags.extend_from_slice(&0u16.to_le_bytes());
            tags.extend_from_slice(&tag_id.to_le_bytes());
            tags.extend_from_slice(&[0u8; 6]);
        }
        let mut stream = comp.create_stream("/Image/Item(1)/Tags/CONTENTS").unwrap();
        stream.write_all(&tags).unwrap();
    }
    drop(comp);

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![77]);
    let meta = &reader.metadata().series_metadata;
    assert_eq!(
        meta.get("zvi.image.1.Title").map(ToString::to_string),
        Some("Scene title".to_string())
    );
    assert_eq!(
        meta.get("zvi.image.1.Channel Name")
            .map(ToString::to_string),
        Some("DAPI".to_string())
    );
    assert_eq!(
        meta.get("zvi.image.1.tag.1537").map(ToString::to_string),
        Some("Scene title".to_string())
    );
}

// ---- raster formats --------------------------------------------------------

fn write_animated_gif(path: &Path) {
    use image::codecs::gif::GifEncoder;
    use image::{Frame, Rgba, RgbaImage};

    let file = std::fs::File::create(path).unwrap();
    let mut encoder = GifEncoder::new(file);
    encoder
        .encode_frame(Frame::new(RgbaImage::from_pixel(
            1,
            1,
            Rgba([255, 0, 0, 255]),
        )))
        .unwrap();
    encoder
        .encode_frame(Frame::new(RgbaImage::from_pixel(
            1,
            1,
            Rgba([0, 255, 0, 255]),
        )))
        .unwrap();
}

fn write_apng_header(path: &Path) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);
    bytes.extend_from_slice(&13u32.to_be_bytes());
    bytes.extend_from_slice(b"IHDR");
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(&1u32.to_be_bytes());
    bytes.extend_from_slice(&[8, 6, 0, 0, 0]);
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&8u32.to_be_bytes());
    bytes.extend_from_slice(b"acTL");
    bytes.extend_from_slice(&2u32.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    std::fs::write(path, bytes).unwrap();
}

fn write_paletted_tga(path: &Path) {
    let mut bytes = vec![
        0, 1, 1, // no image id, color map present, uncompressed color-mapped image
        0, 0, 2, 0, 24, // first palette index, palette length, BGR24 entries
        0, 0, 0, 0, 2, 0, 1, 0, 8, 0, // origin, 2x1 image, 8-bit indices
    ];
    bytes.extend_from_slice(&[0, 0, 255, 0, 255, 0]); // red, green in TGA BGR order
    bytes.extend_from_slice(&[0, 1]);
    std::fs::write(path, bytes).unwrap();
}

#[test]
fn gif_palette_is_expanded_to_samples() {
    let path = tmp("palette.gif");
    std::fs::write(
        &path,
        b"GIF89a\x01\x00\x01\x00\x80\x00\x00\xff\x00\x00\x00\x00\x00,\x00\x00\x00\x00\x01\x00\x01\x00\x00\x02\x02D\x01\x00;",
    )
    .unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 1);
    assert_eq!(meta.size_y, 1);
    assert!(!meta.is_indexed);
    assert!(meta.size_c >= 3);
    let size_c = meta.size_c as usize;
    assert_eq!(reader.open_bytes(0).unwrap().len(), size_c);
}

#[test]
fn animated_gif_is_rejected_instead_of_flattened() {
    let path = tmp("animated.gif");
    write_animated_gif(&path);

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("animated GIF should be rejected"),
        Err(err) => err,
    };
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(message) if message.contains("animated GIF"))
    );
}

#[test]
fn animated_png_is_rejected_instead_of_first_frame_flattened() {
    let path = tmp("animated.apng");
    write_apng_header(&path);

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("animated PNG should be rejected"),
        Err(err) => err,
    };
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(message) if message.contains("animated PNG"))
    );
}

#[test]
fn paletted_tga_is_expanded_to_rgb_samples() {
    let path = tmp("palette.tga");
    write_paletted_tga(&path);

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 3);
    assert!(!meta.is_indexed);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![255, 0, 0, 0, 255, 0]);
}

#[test]
fn tga_round_trip() {
    let mut meta = ImageMetadata::default();
    meta.size_x = 8;
    meta.size_y = 8;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 3;
    meta.is_rgb = true;
    meta.image_count = 1;

    let data: Vec<u8> = (0u8..192).collect(); // 8*8*3
    let rb = round_trip(&tmp("test.tga"), &meta, data.clone());
    assert_eq!(rb, data);
}
