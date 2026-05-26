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

/// Build an RHK SPM text-layout page header (the 512-byte block parsed by
/// RHKReader.java's non-XPM branch) for a `width`x`height` int16 image.
///
/// Mirrors the fixed offsets in RHKReader.initFile: the first short is not
/// 0xaa, the type/dimension record is a space-separated ASCII string at
/// offset 32 (fields: imageType dataType lineType sizeX sizeY _ pageType),
/// the X/Y axis records (whose field [1] is the scale) are at 64/96, and the
/// description sits at 352. Pixels begin at offset 512 (HEADER_SIZE).
fn rhk_text_header(width: u32, height: u32, x_scale: &str, y_scale: &str) -> [u8; 512] {
    let mut hdr = [0u8; 512];
    // first short stays 0 (!= 0xaa) → text layout.
    let put = |hdr: &mut [u8; 512], off: usize, s: &str| {
        let b = s.as_bytes();
        let n = b.len().min(32);
        hdr[off..off + n].copy_from_slice(&b[..n]);
    };
    // imageType=0 dataType=1(int16) lineType=0 sizeX sizeY _ pageType=0
    put(&mut hdr, 32, &format!("0 1 0 {width} {height} 0 0"));
    put(&mut hdr, 64, &format!("x {x_scale}"));
    put(&mut hdr, 96, &format!("y {y_scale}"));
    put(&mut hdr, 352, "test description");
    hdr
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

fn isolated_tmp_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("bioformats_fmt_{}_{}", name, std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

fn append_his_series(
    out: &mut Vec<u8>,
    series_count: u16,
    width: u16,
    height: u16,
    data_type: u16,
    comment: &[u8],
    pixels: &[u8],
) {
    let mut header = vec![0u8; 64];
    header[0..2].copy_from_slice(b"IM");
    header[2..4].copy_from_slice(&(comment.len() as u16).to_le_bytes());
    header[4..6].copy_from_slice(&width.to_le_bytes());
    header[6..8].copy_from_slice(&height.to_le_bytes());
    header[12..14].copy_from_slice(&data_type.to_le_bytes());
    header[14..16].copy_from_slice(&series_count.to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(comment);
    out.extend_from_slice(pixels);
}

fn sm_camera_bytes(width: u16, height: u16, pixels: &[u8]) -> Vec<u8> {
    let mut data = vec![0u8; 548];
    data[..16].copy_from_slice(&[0, 0, 0, 0, 2, 0, 0, 5, 0xc9, 0x88, 0, 5, 0xcb, 0x88, 0, 0]);
    data[524..526].copy_from_slice(&height.to_be_bytes());
    data[532..534].copy_from_slice(&width.to_be_bytes());
    data.extend_from_slice(pixels);
    data
}

#[test]
fn eps_writer_raster_round_trip_reads_hex_image_data() {
    let path = tmp("raster_roundtrip.eps");
    let mut meta = ImageMetadata::default();
    meta.size_x = 3;
    meta.size_y = 2;
    meta.size_c = 3;
    meta.is_rgb = true;
    meta.is_interleaved = true;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 8;

    let pixels = vec![
        1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18,
    ];
    ImageWriter::save(&path, &meta, std::slice::from_ref(&pixels)).expect("EPS write failed");

    let mut reader = ImageReader::open(&path).expect("EPS read failed");
    let read_meta = reader.metadata();
    assert_eq!(read_meta.size_x, 3);
    assert_eq!(read_meta.size_y, 2);
    assert_eq!(read_meta.size_c, 3);
    assert!(read_meta.is_rgb);
    assert_eq!(reader.open_bytes(0).unwrap(), pixels);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![4, 5, 6, 7, 8, 9, 13, 14, 15, 16, 17, 18]
    );
}

#[test]
fn eps_reader_accepts_bioformats_binary_image_subset() {
    let path = tmp("binary_subset.eps");
    let mut data = Vec::new();
    data.extend_from_slice(b"%!PS-Adobe-3.0 EPSF-3.0\n");
    data.extend_from_slice(b"%%BoundingBox: 0 0 2 2\n");
    data.extend_from_slice(b"%%BeginBinary: 4\n");
    data.extend_from_slice(b"2 2 8 [2 0 0 -2 0 2]\n");
    data.extend_from_slice(b"{currentfile 2 string readstring pop}\n");
    data.extend_from_slice(b"image\n");
    data.extend_from_slice(&[9, 8, 7, 6]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::eps::EpsReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![9, 8, 7, 6]);
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

/// Build a minimal little-endian, single-strip, 16-bit grayscale TIFF with a
/// list of extra (tag, type, value) entries appended to the IFD. Used to forge
/// Molecular Dynamics GEL files for the GelReader tests. `value` for SHORT/LONG
/// is the inline value; for RATIONAL it is an out-of-line numerator/denominator
/// pair written after the IFD.
fn build_gel_tiff(w: u16, h: u16, pixels_le: &[u8], extra: &[(u16, u16, u32)]) -> Vec<u8> {
    // type codes: 3 = SHORT, 4 = LONG, 5 = RATIONAL
    let mut entries: Vec<(u16, u16, u32, u32)> = vec![
        (256, 3, 1, w as u32),  // ImageWidth
        (257, 3, 1, h as u32),  // ImageLength
        (258, 3, 1, 16),        // BitsPerSample
        (259, 3, 1, 1),         // Compression = none
        (262, 3, 1, 1),         // PhotometricInterpretation = BlackIsZero
        (277, 3, 1, 1),         // SamplesPerPixel
        (278, 3, 1, h as u32),  // RowsPerStrip
    ];
    for &(tag, ty, val) in extra {
        entries.push((tag, ty, 1, val));
    }
    // We'll patch StripOffsets (273) and StripByteCounts (279) after layout.
    entries.push((273, 4, 1, 0)); // StripOffsets (patched)
    entries.push((279, 4, 1, pixels_le.len() as u32)); // StripByteCounts
    entries.sort_by_key(|e| e.0);

    let n = entries.len();
    // Header (8) + IFD count (2) + 12*n entries + next-IFD offset (4)
    let ifd_start = 8u32;
    let ifd_size = 2 + 12 * n as u32 + 4;
    let mut rational_area = Vec::new();
    let rational_start = ifd_start + ifd_size;

    // Resolve out-of-line RATIONAL values, recording their offsets.
    let mut rational_offsets: std::collections::HashMap<u16, u32> = std::collections::HashMap::new();
    for &(tag, ty, _cnt, val) in &entries {
        if ty == 5 {
            let off = rational_start + rational_area.len() as u32;
            rational_offsets.insert(tag, off);
            // val encodes numerator in high 16 bits, denominator in low 16 bits.
            let num = (val >> 16) as u32;
            let den = (val & 0xffff) as u32;
            rational_area.extend_from_slice(&num.to_le_bytes());
            rational_area.extend_from_slice(&den.to_le_bytes());
        }
    }

    let pixel_start = rational_start + rational_area.len() as u32;

    let mut out = Vec::new();
    out.extend_from_slice(b"II"); // little-endian
    out.extend_from_slice(&42u16.to_le_bytes());
    out.extend_from_slice(&ifd_start.to_le_bytes());
    out.extend_from_slice(&(n as u16).to_le_bytes());
    for &(tag, ty, cnt, val) in &entries {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&ty.to_le_bytes());
        out.extend_from_slice(&cnt.to_le_bytes());
        let field_val = match tag {
            273 => pixel_start,
            _ if ty == 5 => *rational_offsets.get(&tag).unwrap(),
            _ => val,
        };
        out.extend_from_slice(&field_val.to_le_bytes());
    }
    out.extend_from_slice(&0u32.to_le_bytes()); // next IFD = 0
    out.extend_from_slice(&rational_area);
    out.extend_from_slice(pixels_le);
    out
}

#[test]
fn gel_linear_reads_tiff_pixels() {
    // A GEL is a TIFF carrying the MD_FILETAG (33445). With LINEAR format (128),
    // pixels pass through as the underlying 16-bit TIFF samples.
    let pixels: Vec<u8> = vec![0, 1, 0, 2, 0, 3, 0, 4];
    let tiff = build_gel_tiff(2, 2, &pixels, &[(33445, 3, 128)]);
    let path = tmp("gel_linear.gel");
    std::fs::write(&path, &tiff).unwrap();

    let mut reader = bioformats::formats::extended::GelReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!((meta.size_x, meta.size_y), (2, 2));
    assert_eq!(meta.pixel_type, PixelType::Uint16);
    assert_eq!(reader.open_bytes(0).unwrap(), pixels);
    let _ = std::fs::remove_file(&path);

    // A plain TIFF without MD_FILETAG must be rejected by GelReader.
    let plain = build_gel_tiff(2, 2, &pixels, &[]);
    let path2 = tmp("gel_plain.gel");
    std::fs::write(&path2, &plain).unwrap();
    let mut reader2 = bioformats::formats::extended::GelReader::new();
    let err = reader2.set_id(&path2).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref m) if m.contains("MD_FILETAG")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&path2);
}

#[test]
fn gel_square_root_squares_and_scales_to_float() {
    // SQUARE_ROOT format (2): each unsigned-short sample is squared and
    // multiplied by the MD_SCALE_PIXEL (33446) rational, output as 32-bit float.
    // Use a scale of 2/1 and samples [1, 2, 3, 4].
    let pixels: Vec<u8> = vec![1, 0, 2, 0, 3, 0, 4, 0]; // 1,2,3,4 LE u16
    let tiff = build_gel_tiff(
        2,
        2,
        &pixels,
        &[(33445, 3, 2), (33446, 5, (2u32 << 16) | 1u32)],
    );
    let path = tmp("gel_sqrt.gel");
    std::fs::write(&path, &tiff).unwrap();

    let mut reader = bioformats::formats::extended::GelReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.metadata().pixel_type, PixelType::Float32);
    let bytes = reader.open_bytes(0).unwrap();
    let floats: Vec<f32> = bytes
        .chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect();
    // value*value*scale: 1*1*2=2, 4*2=8, 9*2=18, 16*2=32
    assert_eq!(floats, vec![2.0, 8.0, 18.0, 32.0]);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn xml_and_index_readers_reject_missing_companion_images() {
    let prairie = tmp("no_file_pvscan.xml");
    std::fs::write(
        &prairie,
        r#"<PVScan><PVStateValue key="pixelsPerLine" value="2"/><PVStateValue key="linesPerFrame" value="2"/></PVScan>"#,
    )
    .unwrap();
    let mut reader = bioformats::formats::prairie::PrairieReader::new();
    let err = reader.set_id(&prairie).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("PrairieView XML does not reference")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&prairie);

    let leica = tmp("no_file_leica.xml");
    std::fs::write(&leica, r#"<LEICA><Image Width="2" Height="2"/></LEICA>"#).unwrap();
    let mut reader = bioformats::formats::prairie::LeicaTcsReader::new();
    let err = reader.set_id(&leica).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Leica TCS XML does not reference")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&leica);

    let incell = tmp("no_file.xdce");
    std::fs::write(&incell, r#"<InCell Width="2" Height="2"/>"#).unwrap();
    let mut reader = bioformats::formats::incell::InCellReader::new();
    let err = reader.set_id(&incell).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("InCell XML/XDCE does not reference")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&incell);

    let dir = isolated_tmp_dir("visitech_no_tiff");
    let visitech = dir.join("scan.xys");
    std::fs::write(&visitech, b"Width=2\nHeight=2\n").unwrap();
    let mut reader = bioformats::formats::visitech::VisitechReader::new();
    let err = reader.set_id(&visitech).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Visitech XYS does not have")),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn photon_dynamics_pds_is_explicit_unsupported() {
    let path = tmp("unsupported.pds");
    std::fs::write(&path, b"not decoded").unwrap();
    let mut reader = bioformats::formats::perkinelmer::PhotonDynamicsReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Photon Dynamics PDS")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(path);
}

// NB: The upstream Java CellomicsReader reads `.c01` files as zlib-compressed
// (skip the 4-byte magic, then inflate) and `.dib` files as uncompressed. These
// synthetic tests therefore use the `.dib` (uncompressed) layout so the same
// DIB header/cropping path is exercised without needing a zlib encoder in the
// test crate. (CellomicsReader::set_id, src/formats/extended.rs.)
#[test]
fn cellomics_rejects_fake_dimensions_and_truncated_payloads() {
    let missing = tmp("missing_dims.dib");
    std::fs::write(&missing, [0u8; 10]).unwrap();
    let mut reader = bioformats::formats::extended::CellomicsReader::new();
    let err = reader.set_id(&missing).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("missing or invalid image dimensions")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&missing);

    let short = tmp("short_payload.dib");
    let mut data = vec![0u8; 52];
    data[4..6].copy_from_slice(&2u16.to_le_bytes());
    data[6..8].copy_from_slice(&2u16.to_le_bytes());
    data[8..10].copy_from_slice(&16u16.to_le_bytes());
    data.extend_from_slice(&[1, 0, 2, 0]);
    std::fs::write(&short, data).unwrap();
    let mut reader = bioformats::formats::extended::CellomicsReader::new();
    let err = reader.set_id(&short).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("shorter than declared")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&short);
}

#[test]
fn cellomics_legacy_payload_crops_real_pixels() {
    let path = tmp("real_payload.dib");
    let mut data = vec![0u8; 52];
    data[4..6].copy_from_slice(&3u16.to_le_bytes());
    data[6..8].copy_from_slice(&2u16.to_le_bytes());
    data[8..10].copy_from_slice(&8u16.to_le_bytes());
    data.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::extended::CellomicsReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 3, 5, 6]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn perkinelmer_and_openlab_reject_short_payloads_instead_of_padding() {
    // PerkinElmer .htm layout: an .htm header plus a numbered/TIFF pixel file
    // whose stem matches the header stem. Here the TIFF pixel file is truncated
    // so its declared dimensions require more bytes than the file actually
    // contains; the reader must reject it rather than zero-pad.
    let dir = isolated_tmp_dir("perkin_short_payload");
    let htm = dir.join("scan.htm");
    let tif = dir.join("scan.tif");
    std::fs::write(&htm, b"<html><body></body></html>").unwrap();
    let mut meta = ImageMetadata::default();
    meta.size_x = 3;
    meta.size_y = 2;
    meta.size_z = 1;
    meta.size_c = 1;
    meta.size_t = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 8;
    meta.image_count = 1;
    meta.is_little_endian = true;
    meta.resolution_count = 1;
    ImageWriter::save(&tif, &meta, &[vec![1u8, 2, 3, 4, 5, 6]]).unwrap();
    // Truncate the pixel file so it no longer covers the declared image.
    let full = std::fs::read(&tif).unwrap();
    std::fs::write(&tif, &full[..full.len() - 3]).unwrap();
    let mut pe = bioformats::formats::perkinelmer::PerkinElmerReader::new();
    let err = pe.set_id(&htm).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("exceeds file length")),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(dir);

    let raw = tmp("short_openlab.raw");
    let mut data = vec![0u8; 288];
    data[..4].copy_from_slice(b"LBLB");
    data[8..12].copy_from_slice(&2i32.to_be_bytes());
    data[12..16].copy_from_slice(&2i32.to_be_bytes());
    data[16..20].copy_from_slice(&16i32.to_be_bytes());
    data.extend_from_slice(&[1, 0, 2, 0]);
    std::fs::write(&raw, data).unwrap();
    let mut openlab = bioformats::formats::perkinelmer::OpenlabRawReader::new();
    let err = openlab.set_id(&raw).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("shorter than declared")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(raw);
}

#[test]
fn perkinelmer_and_openlab_crop_real_pixels() {
    // PerkinElmer .htm layout: an .htm header plus a matching single-plane TIFF
    // pixel file holding a 3x2 8-bit ramp. Cropping returns real pixels.
    let dir = isolated_tmp_dir("perkin_real_payload");
    let htm = dir.join("scan.htm");
    let tif = dir.join("scan.tif");
    std::fs::write(&htm, b"<html><body></body></html>").unwrap();
    let mut meta = ImageMetadata::default();
    meta.size_x = 3;
    meta.size_y = 2;
    meta.size_z = 1;
    meta.size_c = 1;
    meta.size_t = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 8;
    meta.image_count = 1;
    meta.is_little_endian = true;
    meta.resolution_count = 1;
    ImageWriter::save(&tif, &meta, &[vec![1u8, 2, 3, 4, 5, 6]]).unwrap();
    let mut pe = bioformats::formats::perkinelmer::PerkinElmerReader::new();
    pe.set_id(&htm).unwrap();
    let m = pe.metadata();
    assert_eq!((m.size_x, m.size_y, m.image_count), (3, 2, 1));
    assert_eq!(m.pixel_type, PixelType::Uint8);
    assert_eq!(pe.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(
        pe.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 3, 5, 6]
    );
    let _ = std::fs::remove_dir_all(dir);

    let raw = tmp("real_openlab.raw");
    let mut data = vec![0u8; 288];
    data[..4].copy_from_slice(b"LBLB");
    data[8..12].copy_from_slice(&3i32.to_be_bytes());
    data[12..16].copy_from_slice(&2i32.to_be_bytes());
    data[16..20].copy_from_slice(&8i32.to_be_bytes());
    data.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
    std::fs::write(&raw, data).unwrap();
    let mut openlab = bioformats::formats::perkinelmer::OpenlabRawReader::new();
    openlab.set_id(&raw).unwrap();
    assert_eq!(
        openlab.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 3, 5, 6]
    );
    let _ = std::fs::remove_file(raw);
}

#[test]
fn sm_camera_reads_magic_dimensions_and_pixels() {
    let path = tmp("real_payload.smc");
    std::fs::write(&path, sm_camera_bytes(3, 2, &[1, 2, 3, 4, 5, 6])).unwrap();

    let mut reader = bioformats::formats::misc::SmCameraReader::new();
    assert!(reader.is_this_type_by_bytes(&std::fs::read(&path).unwrap()));
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!((meta.size_x, meta.size_y, meta.image_count), (3, 2, 1));
    assert_eq!(meta.pixel_type, PixelType::Uint8);
    assert!(!meta.is_little_endian);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 3, 5, 6]
    );
    let mut registry_reader = ImageReader::open(&path).unwrap();
    assert_eq!(registry_reader.metadata().size_x, 3);
    assert_eq!(
        registry_reader.open_bytes(0).unwrap(),
        vec![1, 2, 3, 4, 5, 6]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn sm_camera_rejects_bad_magic_and_truncated_payload() {
    let bad_magic = tmp("bad_magic.smc");
    let mut data = sm_camera_bytes(1, 1, &[7]);
    data[0] = 1;
    std::fs::write(&bad_magic, data).unwrap();
    let mut reader = bioformats::formats::misc::SmCameraReader::new();
    let err = reader.set_id(&bad_magic).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("SMC magic")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&bad_magic);

    let short = tmp("short_payload.smc");
    std::fs::write(&short, sm_camera_bytes(2, 2, &[1, 2, 3])).unwrap();
    let mut reader = bioformats::formats::misc::SmCameraReader::new();
    let err = reader.set_id(&short).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("shorter than declared")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(short);
}

#[test]
fn lim_requires_declared_header_and_crops_real_pixels() {
    let missing = tmp("missing_dims.lim");
    std::fs::write(&missing, [0u8; 32]).unwrap();
    let mut reader = bioformats::formats::lim::LimReader::new();
    let err = reader.set_id(&missing).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("LIM header is missing")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&missing);

    // Java-correct LIM header layout (LIMReader.java):
    //   sizeX @0, sizeY @2, bits @4, isCompressed @6; pixels at PIXELS_OFFSET=0x94b.
    let path = tmp("real_payload.lim");
    let pixels_offset = 0x94b;
    let mut data = vec![0u8; pixels_offset];
    data[0..2].copy_from_slice(&3u16.to_le_bytes()); // sizeX
    data[2..4].copy_from_slice(&2u16.to_le_bytes()); // sizeY
    data[4..6].copy_from_slice(&8u16.to_le_bytes()); // bits
    data[6..8].copy_from_slice(&0u16.to_le_bytes()); // isCompressed
    data.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::lim::LimReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 3, 5, 6]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn simfcs_requires_whole_frames_and_crops_real_pixels() {
    let short = tmp("short_frame.b64");
    std::fs::write(&short, [1, 2, 3]).unwrap();
    let mut reader = bioformats::formats::simfcs::SimfcsReader::new();
    let err = reader.set_id(&short).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("whole number of 256x256 frames")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&short);

    let path = tmp("one_frame.b64");
    let mut data: Vec<u8> = (0..=255).cycle().take(256 * 256).collect();
    data[257] = 99;
    std::fs::write(&path, &data).unwrap();

    let mut reader = bioformats::formats::simfcs::SimfcsReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(
        reader.open_bytes_region(0, 1, 1, 2, 1).unwrap(),
        vec![99, 2]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn mias_placeholder_readers_reject_or_require_real_payloads() {
    let htd = tmp("cellworx.htd");
    std::fs::write(&htd, b"XSites,1\nYSites,1\n").unwrap();
    let mut cell = bioformats::formats::mias::CellWorxReader::new();
    let err = cell.set_id(&htd).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("CellWorX")),
        "{err:?}"
    );
    let err = cell.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("CellWorX")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&htd);

    let ser = tmp("minimal.ser");
    std::fs::write(
        &ser,
        [
            0x97, 0x01, 0, 0, 2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0,
            0, 1, 0, 0, 0,
        ],
    )
    .unwrap();
    let mut fei = bioformats::formats::mias::FeiSerReader::new();
    let err = fei.set_id(&ser).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("FEI SER payload decoding")),
        "{err:?}"
    );
    let err = fei.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("FEI SER payload decoding")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&ser);

    let short_al3d = tmp("short_payload.al3d");
    let mut al3d = vec![0u8; 24];
    al3d[..4].copy_from_slice(b"AL3D");
    al3d[8..12].copy_from_slice(&2u32.to_le_bytes());
    al3d[12..16].copy_from_slice(&2u32.to_le_bytes());
    al3d[16..20].copy_from_slice(&1u32.to_le_bytes());
    std::fs::write(&short_al3d, al3d).unwrap();
    let mut reader = bioformats::formats::mias::Al3dReader::new();
    let err = reader.set_id(&short_al3d).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("AL3D pixel payload")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&short_al3d);

    let path = tmp("real_payload.top");
    let mut top = vec![0u8; 128];
    top[4..6].copy_from_slice(&2u16.to_le_bytes());
    top[6..8].copy_from_slice(&2u16.to_le_bytes());
    top[8..10].copy_from_slice(&0u16.to_le_bytes());
    top.extend_from_slice(&[1, 2, 3, 4]);
    std::fs::write(&path, top).unwrap();
    let mut oxford = bioformats::formats::mias::OxfordInstrumentsReader::new();
    oxford.set_id(&path).unwrap();
    assert_eq!(oxford.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(oxford.open_bytes_region(0, 1, 0, 1, 2).unwrap(), vec![2, 4]);
    let _ = std::fs::remove_file(path);
}

#[test]
fn zip_delegates_inner_image_and_has_no_placeholder_pixels() {
    use std::io::Write;
    fn write_zip_entry(path: &Path, name: &str, bytes: &[u8]) {
        let file = std::fs::File::create(path).unwrap();
        let mut zip = zip::ZipWriter::new(file);
        zip.start_file(name, zip::write::SimpleFileOptions::default())
            .unwrap();
        zip.write_all(bytes).unwrap();
        zip.finish().unwrap();
    }

    // Matching the Java ZipReader, ZipReader delegates the primary archive entry
    // to the auto-detecting ImageReader (any inner format). A ZIP wrapping a real
    // TIFF reads that TIFF's real pixels (no fabricated placeholder data).
    let dir = isolated_tmp_dir("zip_inner_image");
    let tiff_src = dir.join("source.tif");
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 2;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;
    let pixels = vec![11u8, 22, 33, 44];
    ImageWriter::save(&tiff_src, &meta, &[pixels.clone()]).unwrap();
    let tiff_bytes = std::fs::read(&tiff_src).unwrap();

    let zip_path = dir.join("inner.zip");
    write_zip_entry(&zip_path, "frame.tif", &tiff_bytes);

    let mut reader = ImageReader::open(&zip_path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), pixels);

    // A ZIP whose entry matches no registered reader is rejected outright,
    // never producing placeholder pixels.
    let bad_zip = dir.join("bad.zip");
    write_zip_entry(&bad_zip, "data.unknownfmt", b"not image data at all");
    let err = match ImageReader::open(&bad_zip) {
        Ok(_) => panic!("ZIP with no recognized image entry should be rejected"),
        Err(err) => err,
    };
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(_)),
        "{err:?}"
    );
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
fn unisoku_reads_java_style_hdr_dat_dataset() {
    let dir = isolated_tmp_dir("unisoku_java_style");
    let hdr = dir.join("sample.HDR");
    let dat = dir.join("sample.DAT");
    std::fs::write(
        &hdr,
        b":STM data\r:data volume(x*y)\r3 2\r:ascii flag; data type\r0 4\r:sample name\rsynthetic\r",
    )
    .unwrap();
    std::fs::write(&dat, [1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]).unwrap();

    let mut reader = ImageReader::open(&hdr).unwrap();
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
    assert!(matches!(
        reader.metadata().series_metadata.get(":sample name"),
        Some(MetadataValue::String(value)) if value == "synthetic"
    ));
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 0, 3, 0, 5, 0, 6, 0]
    );
}

#[test]
fn unisoku_dat_entrypoint_uses_companion_hdr() {
    let dir = isolated_tmp_dir("unisoku_dat_entrypoint");
    let hdr = dir.join("entry.HDR");
    let dat = dir.join("entry.DAT");
    std::fs::write(
        &hdr,
        b":STM data\r:data volume(x*y)\r2 1\r:ascii flag; data type\r0 2\r",
    )
    .unwrap();
    std::fs::write(&dat, [7, 8]).unwrap();

    let mut reader = ImageReader::open(&dat).unwrap();
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![7, 8]);
}

#[test]
fn spm_heuristic_only_readers_reject_raw_files() {
    let cases: Vec<(&str, Box<dyn FormatReader>)> = vec![(
        "raw.afm",
        Box::new(bioformats::formats::spm::QuesantReader::new()),
    )];

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
fn vgsam_reads_java_header_and_big_endian_pixels() {
    let path = tmp("vgsam.dti");
    let mut data = vec![0u8; 368];
    data[..3].copy_from_slice(b"VGS");
    data[348..352].copy_from_slice(&3i32.to_be_bytes());
    data[352..356].copy_from_slice(&2i32.to_be_bytes());
    data[360..364].copy_from_slice(&2i32.to_be_bytes());
    data.extend_from_slice(&[0, 1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::VgSamReader::new();
    assert!(reader.is_this_type_by_bytes(b"VGS synthetic"));
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!((meta.size_x, meta.size_y, meta.bits_per_pixel), (3, 2, 16));
    assert_eq!(meta.pixel_type, PixelType::Uint16);
    assert!(!meta.is_little_endian);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![0, 2, 0, 3, 0, 5, 0, 6]
    );
}

#[test]
fn vgsam_rejects_bad_magic_and_short_payloads() {
    let bad = tmp("bad_vgsam.dti");
    std::fs::write(&bad, [0u8; 368]).unwrap();
    let err = bioformats::formats::spm::VgSamReader::new()
        .set_id(&bad)
        .unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("VGS magic")),
        "{err:?}"
    );

    let short = tmp("short_vgsam.dti");
    let mut data = vec![0u8; 368];
    data[..3].copy_from_slice(b"VGS");
    data[348..352].copy_from_slice(&2i32.to_be_bytes());
    data[352..356].copy_from_slice(&2i32.to_be_bytes());
    data[360..364].copy_from_slice(&2i32.to_be_bytes());
    data.extend_from_slice(&[0, 1]);
    std::fs::write(&short, data).unwrap();
    let err = bioformats::formats::spm::VgSamReader::new()
        .set_id(&short)
        .unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("shorter than declared")),
        "{err:?}"
    );
}

#[test]
fn seiko_reads_java_header_and_raw_uint16_pixels() {
    let path = tmp("seiko.xqd");
    let mut data = vec![0u8; 2944];
    data[40..49].copy_from_slice(b"synthetic");
    data[156..160].copy_from_slice(&1.5f32.to_le_bytes());
    data[164..168].copy_from_slice(&2.5f32.to_le_bytes());
    data[1402..1404].copy_from_slice(&3u16.to_le_bytes());
    data[1404..1406].copy_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::SeikoReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!((meta.size_x, meta.size_y, meta.bits_per_pixel), (3, 2, 16));
    assert_eq!(meta.pixel_type, PixelType::Uint16);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 0, 3, 0, 5, 0, 6, 0]
    );
}

#[test]
fn seiko_rejects_short_payloads() {
    let path = tmp("short_seiko.xqd");
    let mut data = vec![0u8; 2944];
    data[1402..1404].copy_from_slice(&2u16.to_le_bytes());
    data[1404..1406].copy_from_slice(&2u16.to_le_bytes());
    data.extend_from_slice(&[1, 0]);
    std::fs::write(&path, data).unwrap();

    let err = bioformats::formats::spm::SeikoReader::new()
        .set_id(&path)
        .unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("shorter than declared")),
        "{err:?}"
    );
}

#[test]
fn watop_reads_java_header_and_raw_int16_pixels() {
    let path = tmp("watop.wat");
    let mut data = vec![0u8; 4864];
    data[..25].copy_from_slice(b"0TOPSystem W.A.Technology");
    data[49..58].copy_from_slice(b"synthetic");
    data[247..251].copy_from_slice(&300i32.to_le_bytes());
    data[251..255].copy_from_slice(&200i32.to_le_bytes());
    data[255..259].copy_from_slice(&100i32.to_le_bytes());
    data[259..263].copy_from_slice(&3i32.to_le_bytes());
    data[263..267].copy_from_slice(&2i32.to_le_bytes());
    data.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::WatopReader::new();
    assert!(reader.is_this_type_by_bytes(b"0TOPSystem W.A.Technology"));
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!((meta.size_x, meta.size_y, meta.bits_per_pixel), (3, 2, 16));
    assert_eq!(meta.pixel_type, PixelType::Int16);
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        vec![1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]
    );
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 0, 3, 0, 5, 0, 6, 0]
    );
}

#[test]
fn watop_rejects_short_or_wrong_magic_files() {
    let path = tmp("bad_watop.wat");
    std::fs::write(&path, [0u8; 128]).unwrap();
    let err = bioformats::formats::spm::WatopReader::new()
        .set_id(&path)
        .unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("4864-byte header")),
        "{err:?}"
    );
}

#[test]
fn ubm_reads_java_header_uint32_pixels_and_row_padding() {
    let path = tmp("ubm.pr3");
    let mut data = vec![0u8; 128];
    data[44..48].copy_from_slice(&3i32.to_le_bytes());
    data[48..52].copy_from_slice(&2i32.to_le_bytes());
    for value in [1u32, 2, 3, 99, 4, 5, 6, 100] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::UbmReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!((meta.size_x, meta.size_y, meta.bits_per_pixel), (3, 2, 32));
    assert_eq!(meta.pixel_type, PixelType::Uint32);

    let mut full = Vec::new();
    for value in [1u32, 2, 3, 4, 5, 6] {
        full.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(reader.open_bytes(0).unwrap(), full);

    let mut crop = Vec::new();
    for value in [2u32, 3, 5, 6] {
        crop.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(), crop);
}

#[test]
fn ubm_rejects_short_payloads() {
    let path = tmp("short_ubm.pr3");
    let mut data = vec![0u8; 128];
    data[44..48].copy_from_slice(&2i32.to_le_bytes());
    data[48..52].copy_from_slice(&2i32.to_le_bytes());
    data.extend_from_slice(&[1, 0, 0, 0]);
    std::fs::write(&path, data).unwrap();

    let err = bioformats::formats::spm::UbmReader::new()
        .set_id(&path)
        .unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("shorter than declared")),
        "{err:?}"
    );
}

#[test]
fn rhk_requires_header_dimensions_and_crops_real_pixels() {
    // A file shorter than the 512-byte page header must be rejected
    // (RHKReader.java reads fixed offsets up to 512).
    let missing = tmp("missing_dims.sm3");
    std::fs::write(&missing, b"\x00\x00header too short").unwrap();
    let mut reader = bioformats::formats::spm::RhkReader::new();
    let err = reader.set_id(&missing).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(message) if message.contains("shorter than the 512-byte"))
    );

    // Text-layout 3x2 int16 image, no axis inversion (xScale > 0 → invertX
    // false; yScale < 0 → invertY false since invertY = yScale > 0). Pixels
    // start at offset 512: rows (1,2,3) and (4,5,6).
    let path = tmp("rhktest.sm3");
    let mut data = rhk_text_header(3, 2, "1.0", "-1.0").to_vec();
    data.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]);
    std::fs::write(&path, data).unwrap();
    let mut reader = bioformats::formats::spm::RhkReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 0, 3, 0, 5, 0, 6, 0]
    );

    // With invertX (xScale < 0) the stored plane is mirrored horizontally
    // before cropping: row0 becomes (3,2,1), row1 (6,5,4). Region (1,0,2,2)
    // then yields cols 1,2 of each mirrored row → (2,1) and (5,4).
    let path_inv = tmp("rhktest_invx.sm3");
    let mut data_inv = rhk_text_header(3, 2, "-1.0", "-1.0").to_vec();
    data_inv.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0]);
    std::fs::write(&path_inv, data_inv).unwrap();
    let mut reader = bioformats::formats::spm::RhkReader::new();
    reader.set_id(&path_inv).unwrap();
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 0, 1, 0, 5, 0, 4, 0]
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

fn write_fei_philips_img(path: &Path, width: usize, height: usize, pixels: &[u8]) {
    assert_eq!(pixels.len(), width * height);
    assert_eq!(width % 2, 0);

    let header_size = 524usize;
    let mut data = vec![0u8; header_size];
    data[..2].copy_from_slice(b"XL");
    data[44..48].copy_from_slice(&1500.0f32.to_le_bytes());
    data[48..52].copy_from_slice(&12000.0f32.to_le_bytes());
    data[52..56].copy_from_slice(&3.5f32.to_le_bytes());
    data[68..72].copy_from_slice(&4.0f32.to_le_bytes());
    let stored_width = (width as u16 + 112).to_le_bytes();
    data[514..516].copy_from_slice(&stored_width);
    data[516..518].copy_from_slice(&(height as u16).to_le_bytes());
    data[522..524].copy_from_slice(&(header_size as u16).to_le_bytes());

    let invalid = [0u8; 56];
    for row_pass in 0..4usize {
        let mut row = row_pass;
        while row < height {
            for col_pass in 0..2usize {
                for col in (col_pass..width).step_by(2) {
                    data.push(pixels[row * width + col]);
                }
                data.extend_from_slice(&invalid);
            }
            row += 4;
        }
    }

    std::fs::write(path, data).unwrap();
}

#[test]
fn fei_philips_img_decodes_java_interlaced_pixels() {
    let path = tmp("fei_philips.img");
    let pixels: Vec<u8> = (0..16).collect();
    write_fei_philips_img(&path, 4, 4, &pixels);

    let mut reader = bioformats::formats::sem::FeiPhilipsReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!((meta.size_x, meta.size_y), (4, 4));
    assert_eq!(meta.pixel_type, PixelType::Uint8);
    assert!(matches!(
        meta.series_metadata.get("kV"),
        Some(MetadataValue::Float(v)) if (*v - 12.0).abs() < 0.0001
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), pixels);
    assert_eq!(
        reader.open_bytes_region(0, 1, 1, 2, 2).unwrap(),
        vec![5, 6, 9, 10]
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn fei_philips_img_registry_opens_magic_detected_file() {
    let path = tmp("fei_philips_registry.img");
    let pixels: Vec<u8> = (20..36).collect();
    write_fei_philips_img(&path, 4, 4, &pixels);

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 4);
    assert_eq!(reader.open_bytes(0).unwrap(), pixels);

    let _ = std::fs::remove_file(path);
}

#[test]
fn opus_iss_guessed_header_readers_reject_raw_files() {
    let cases: Vec<(&str, Box<dyn FormatReader>, &str)> = vec![
        (
            "raw.abs",
            Box::new(bioformats::formats::opus::BrukerOpusReader::new()),
            "Bruker OPUS spectral image decoding is not implemented",
        ),
        (
            "raw.iss",
            Box::new(bioformats::formats::opus::IssFlimReader::new()),
            "ISS Vista FLIM decoding is not implemented",
        ),
    ];

    for (name, mut reader, expected) in cases {
        let path = tmp(name);
        std::fs::write(&path, [0x0a, 0x01, 0, 0, 4, 0, 0, 0]).unwrap();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains(expected)),
            "{name}: {err:?}"
        );
    }
}

#[test]
fn opus_iss_registry_paths_reject_guessed_headers() {
    for (name, bytes, expected) in [
        (
            "registry_raw.abs",
            b"\x0a\x01\0\0\x04\0\0\0not real".as_slice(),
            "Bruker OPUS spectral image decoding is not implemented",
        ),
        (
            "registry_raw.0",
            b"\x0a\x01\0\0\x04\0\0\0not real".as_slice(),
            "Bruker OPUS spectral image decoding is not implemented",
        ),
        (
            "registry_raw.iss",
            b"not real".as_slice(),
            "ISS Vista FLIM decoding is not implemented",
        ),
    ] {
        let path = tmp(name);
        std::fs::write(&path, bytes).unwrap();
        let err = match ImageReader::open(&path) {
            Ok(_) => panic!("{name}: guessed OPUS/ISS header opened"),
            Err(err) => err,
        };
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains(expected)),
            "{name}: {err:?}"
        );
    }
}

#[test]
fn misc4_raw_payload_readers_crop_real_pixels() {
    // A valid Axon Raw Format (ARF) file per the Java ARFReader: 2 endianness
    // bytes, "AR" signature, then version/width/height/bitsPerPixel as unsigned
    // shorts; raw pixel data begins at PIXELS_OFFSET (524).
    let arf_path = tmp("crop.arf");
    let mut arf_data = vec![1u8, 0]; // little-endian
    arf_data.extend_from_slice(b"AR");
    arf_data.extend_from_slice(&1u16.to_le_bytes()); // version
    arf_data.extend_from_slice(&3u16.to_le_bytes()); // width
    arf_data.extend_from_slice(&3u16.to_le_bytes()); // height
    arf_data.extend_from_slice(&16u16.to_le_bytes()); // bits per pixel
    arf_data.resize(524, 0); // pad to PIXELS_OFFSET
    for value in 1u16..=9 {
        arf_data.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(&arf_path, arf_data).unwrap();
    let mut arf = bioformats::formats::misc4::ArfReader::new();
    arf.set_id(&arf_path).unwrap();
    assert_eq!(
        arf.open_bytes_region(0, 1, 1, 2, 2).unwrap(),
        vec![5, 0, 6, 0, 8, 0, 9, 0]
    );

    let pds_path = tmp("crop.pds");
    let mut pds_data = b"LINES = 2\nLINE_SAMPLES = 3\nSAMPLE_BITS = 8\nEND\n".to_vec();
    pds_data.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
    std::fs::write(&pds_path, pds_data).unwrap();
    let mut pds = bioformats::formats::misc4::PdsReader::new();
    pds.set_id(&pds_path).unwrap();
    assert_eq!(
        pds.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 3, 5, 6]
    );

    let his_path = tmp("crop.his");
    let mut his_data = Vec::new();
    let mut his_pixels = Vec::new();
    for value in 1u16..=6 {
        his_pixels.extend_from_slice(&value.to_le_bytes());
    }
    append_his_series(&mut his_data, 1, 3, 2, 2, b"vDate=2026/05/26;", &his_pixels);
    std::fs::write(&his_path, his_data).unwrap();
    let mut his = bioformats::formats::misc4::HisReader::new();
    his.set_id(&his_path).unwrap();
    assert_eq!(
        his.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 0, 3, 0, 5, 0, 6, 0]
    );

    let csv_path = tmp("crop.csv");
    std::fs::write(&csv_path, b"1 2 3\n4 5 6\n").unwrap();
    let mut csv = bioformats::formats::misc4::TextImageReader::new();
    csv.set_id(&csv_path).unwrap();
    let mut expected = Vec::new();
    for value in [2.0f32, 3.0, 5.0, 6.0] {
        expected.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(csv.open_bytes_region(0, 1, 0, 2, 2).unwrap(), expected);
}

#[test]
fn hamamatsu_his_reads_java_style_multiseries_and_rgb_regions() {
    let path = tmp("java_style_multi.his");
    let mut data = Vec::new();
    append_his_series(&mut data, 2, 2, 1, 1, b"vOffset=1.5;", &[10, 20]);
    append_his_series(
        &mut data,
        2,
        2,
        1,
        11,
        b"vBinX=2;vBinY=3;",
        &[1, 2, 3, 4, 5, 6],
    );
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::misc4::HisReader::new();
    assert!(reader.is_this_type_by_bytes(b"IM"));
    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 2);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
    assert!(matches!(
        reader.metadata().series_metadata.get("vOffset"),
        Some(MetadataValue::String(value)) if value == "1.5"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 20]);

    reader.set_series(1).unwrap();
    assert_eq!(reader.metadata().size_c, 3);
    assert!(reader.metadata().is_rgb);
    assert!(reader.metadata().is_interleaved);
    assert!(matches!(
        reader.metadata().series_metadata.get("vBinY"),
        Some(MetadataValue::String(value)) if value == "3"
    ));
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 1, 1).unwrap(),
        vec![4, 5, 6]
    );
}

#[test]
fn misc4_raw_payload_readers_reject_truncated_or_fake_dimensions() {
    // A file with valid endianness bytes but a missing "AR" signature must be
    // rejected (per the Java ARFReader header validation).
    let arf_path = tmp("odd.arf");
    let mut bad = vec![1u8, 0, b'X', b'Y'];
    bad.resize(12, 0);
    std::fs::write(&arf_path, bad).unwrap();
    let mut arf = bioformats::formats::misc4::ArfReader::new();
    let err = arf.set_id(&arf_path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::InvalidData(ref message) if message.contains("AR")),
        "{err:?}"
    );

    let pds_path = tmp("truncated.pds");
    let mut pds_data = b"LINES = 2\nLINE_SAMPLES = 3\nSAMPLE_BITS = 8\nEND\n".to_vec();
    pds_data.extend_from_slice(&[1, 2, 3, 4, 5]);
    std::fs::write(&pds_path, pds_data).unwrap();
    let mut pds = bioformats::formats::misc4::PdsReader::new();
    let err = pds.set_id(&pds_path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("shorter than declared")),
        "{err:?}"
    );

    let his_path = tmp("missing_dims.his");
    let mut his_data = Vec::new();
    append_his_series(&mut his_data, 1, 0, 2, 2, b"", &[]);
    std::fs::write(&his_path, his_data).unwrap();
    let mut his = bioformats::formats::misc4::HisReader::new();
    let err = his.set_id(&his_path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("missing image dimensions")),
        "{err:?}"
    );
}

#[test]
fn povray_df3_rejects_truncated_payload_instead_of_padding() {
    let path = tmp("truncated.df3");
    let mut data = Vec::new();
    data.extend_from_slice(&2u16.to_be_bytes());
    data.extend_from_slice(&2u16.to_be_bytes());
    data.extend_from_slice(&2u16.to_be_bytes());
    data.extend_from_slice(&[1, 2, 3]);
    std::fs::write(&path, data).unwrap();

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("truncated DF3 unexpectedly opened"),
        Err(err) => err,
    };
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("DF3 pixel payload")),
        "{err:?}"
    );
}

#[test]
fn povray_df3_regions_crop_real_voxel_data() {
    let path = tmp("valid.df3");
    let mut data = Vec::new();
    data.extend_from_slice(&3u16.to_be_bytes());
    data.extend_from_slice(&2u16.to_be_bytes());
    data.extend_from_slice(&2u16.to_be_bytes());
    data.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
    data.extend_from_slice(&[7, 8, 9, 10, 11, 12]);
    std::fs::write(&path, data).unwrap();

    let mut reader = ImageReader::open(&path).expect("valid DF3 should open");
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_z, 2);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![7, 8, 9, 10, 11, 12]);
    assert_eq!(
        reader.open_bytes_region(1, 1, 0, 2, 2).unwrap(),
        vec![8, 9, 11, 12]
    );
}

#[test]
fn hitachi_region_crops_real_pixels_from_declared_header() {
    // HitachiReader.java reads a `.txt` INI whose `[SemImageFile]` table carries
    // an `ImageName=` pointing at a companion pixels file; pixel access is fully
    // delegated to a helper ImageReader on that companion image (openBytes and
    // the x/y/w/h crop). Build a real INI + a small TIFF companion (3x2 uint8)
    // and verify a cropped region returns the companion's real pixels.
    let dir = isolated_tmp_dir("hitachi_sem");
    let txt = dir.join("scan.txt");
    let companion = dir.join("scan.tif");

    let mut meta = ImageMetadata::default();
    meta.size_x = 3;
    meta.size_y = 2;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;
    let pixels: Vec<u8> = vec![1, 2, 3, 4, 5, 6];
    ImageWriter::save(&companion, &meta, &[pixels]).unwrap();

    // The `[SemImageFile]` magic and `ImageName` key match HitachiReader.java.
    let ini = format!(
        "[SemImageFile]\r\nImageName={}\r\nPixelSize=1.0\r\nDataSize=3x2\r\n",
        companion.file_name().unwrap().to_string_lossy()
    );
    std::fs::write(&txt, ini).unwrap();

    let mut reader = ImageReader::open(&txt).unwrap();
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    // Crop the right-most 2 columns, both rows: companion row-major pixels are
    // [1,2,3 / 4,5,6], so columns 1..3 -> [2,3,5,6].
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 3, 5, 6]
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
fn fits_returns_raw_int16_big_endian_without_bzero_scaling() {
    // Java FitsReader keeps littleEndian=false and returns raw samples; it does
    // not apply BZERO/BSCALE. BITPIX 16 maps to signed INT16.
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
            raw.clone(),
        )],
    );

    let mut r = ImageReader::open(&path).unwrap();
    assert_eq!(r.metadata().pixel_type, PixelType::Int16);
    assert!(!r.metadata().is_little_endian);
    // Raw, unscaled, big-endian bytes exactly as stored.
    assert_eq!(r.open_bytes(0).unwrap(), raw);
}

#[test]
fn fits_ignores_bscale_and_returns_raw_int16() {
    // BSCALE/BZERO are ignored by Java FitsReader; the type stays INT16 (not
    // promoted to float) and the bytes are returned unscaled, big-endian.
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
            raw.clone(),
        )],
    );

    let mut r = ImageReader::open(&path).unwrap();
    assert_eq!(r.metadata().pixel_type, PixelType::Int16);
    assert!(!r.metadata().is_little_endian);
    assert_eq!(r.open_bytes(0).unwrap(), raw);
}

#[test]
fn fits_reads_only_primary_hdu_ignoring_image_extensions() {
    // Java FitsReader reads only the primary HDU. An empty primary (NAXIS 0)
    // followed by an IMAGE extension yields no readable image, so opening fails
    // rather than silently reading the extension's pixels.
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

    assert!(ImageReader::open(&path).is_err());
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
fn nrrd_omitted_leading_space_direction_becomes_channels() {
    let path = tmp("omitted_leading_space_direction.nrrd");
    let data: Vec<u8> = (0..24).collect();
    let mut bytes = b"NRRD0004
type: uint8
dimension: 4
sizes: 3 2 2 2
space dimension: 3
space directions: (1,0,0) (0,1,0) (0,0,1)
encoding: raw

"
    .to_vec();
    bytes.extend_from_slice(&data);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_z, 2);
    assert_eq!(reader.metadata().size_c, 3);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(
        reader.open_bytes(1).unwrap(),
        vec![12, 13, 14, 15, 16, 17, 18, 19, 20, 21, 22, 23]
    );
}

#[test]
fn nrrd_legacy_nan_leading_axis_becomes_channels() {
    let path = tmp("legacy_nan_leading_axis.nhdr");
    let raw = tmp("legacy_nan_leading_axis.raw");
    let data: Vec<u8> = (0..24).collect();
    std::fs::write(&raw, &data).unwrap();
    let header = format!(
        "NRRD0001
type: uint8
dimension: 4
sizes: 3 2 2 2
axis mins: NaN -1 -1 -1
encoding: raw
data file: {}

",
        raw.file_name().unwrap().to_string_lossy()
    );
    std::fs::write(&path, header).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_z, 2);
    assert_eq!(reader.metadata().size_c, 3);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        vec![0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11]
    );
}

#[test]
fn nrrd_leading_channel_then_xyz_axes() {
    // Java NRRDReader (initFile, lines ~308-328) assigns axes positionally and
    // ignores `kinds`. For `sizes: 2 1 2 3` with dimension >= 3: axis 0 has size
    // 2 (1 < 2 <= 16) so it becomes sizeC; the remaining axes fill X, Y, Z. T
    // stays 1, so imageCount = sizeZ * sizeT = 3. Data is interleaved C-first.
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
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.metadata().size_x, 1);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_z, 3);
    assert_eq!(reader.metadata().size_t, 1);
    assert_eq!(reader.metadata().image_count, 3);
    // Plane size = X*Y*C = 1*2*2 = 4 bytes; z=0 is the first 4 interleaved samples.
    assert_eq!(reader.open_bytes(0).unwrap(), vec![0, 1, 2, 3]);
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
sizes: 1 2 2
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
fn nrrd_detached_data_honors_byte_skip_as_absolute_offset() {
    // Java NRRDReader treats `byte skip` as an absolute file offset
    // (offset = parseLong(v); safeSkip(fis, offset)) and has no `line skip`
    // handling for detached raw data. The pixels \x05\x06 begin at byte 20.
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
byte skip: 20

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

    let mut file = hdf5_pure::FileBuilder::new();
    let mut t0 = file.create_group("t00000");
    let mut s0 = t0.create_group("s00");
    let mut level0 = s0.create_group("0");
    level0
        .create_dataset("cells")
        .with_u16_data(&[1u16, 2, 3, 4, 5, 6])
        .with_shape(&[1, 2, 3]);
    s0.add_group(level0.finish());
    t0.add_group(s0.finish());
    file.add_group(t0.finish());

    let mut setup0 = file.create_group("s00");
    setup0
        .create_dataset("resolutions")
        .with_f64_data(&[1.0f64, 1.0, 1.0])
        .with_shape(&[1, 3]);
    file.add_group(setup0.finish());
    file.write(&path).unwrap();

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
fn bdv_rejects_short_dataset_instead_of_zero_filling_plane() {
    let path = tmp("short_bdv.h5");
    let xml_path = path.with_extension("xml");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&xml_path);

    let mut file = hdf5_pure::FileBuilder::new();
    let mut t0 = file.create_group("t00000");
    let mut s0 = t0.create_group("s00");
    let mut level0 = s0.create_group("0");
    level0
        .create_dataset("cells")
        .with_u16_data(&[7u16])
        .with_shape(&[1, 1, 1]);
    s0.add_group(level0.finish());
    t0.add_group(s0.finish());
    file.add_group(t0.finish());

    let mut setup0 = file.create_group("s00");
    setup0
        .create_dataset("resolutions")
        .with_f64_data(&[1.0f64, 1.0, 1.0])
        .with_shape(&[1, 3]);
    file.add_group(setup0.finish());
    file.write(&path).unwrap();

    std::fs::write(
        &xml_path,
        r#"<SpimData><SequenceDescription><ViewSetups><ViewSetup><size>2 1 1</size></ViewSetup></ViewSetups></SequenceDescription></SpimData>"#,
    )
    .unwrap();

    let mut reader = bioformats::formats::bdv::BdvReader::new();
    reader.set_id(&path).unwrap();
    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("shorter than declared plane")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&xml_path);
}

#[test]
fn imaris_rejects_short_dataset_instead_of_zero_filling_plane() {
    let path = tmp("short_ims.ims");
    let _ = std::fs::remove_file(&path);

    let mut file = hdf5_pure::FileBuilder::new();
    let mut info = file.create_group("DataSetInfo");
    let mut image = info.create_group("Image");
    image.set_attr("X", hdf5_pure::AttrValue::String("2".to_string()));
    image.set_attr("Y", hdf5_pure::AttrValue::String("1".to_string()));
    image.set_attr("Z", hdf5_pure::AttrValue::String("1".to_string()));
    info.add_group(image.finish());
    file.add_group(info.finish());

    let mut dataset = file.create_group("DataSet");
    let mut res = dataset.create_group("ResolutionLevel 0");
    let mut time = res.create_group("TimePoint 0");
    let mut channel = time.create_group("Channel 0");
    channel
        .create_dataset("Data")
        .with_u8_data(&[5u8])
        .with_shape(&[1, 1, 1]);
    time.add_group(channel.finish());
    res.add_group(time.finish());
    dataset.add_group(res.finish());
    file.add_group(dataset.finish());
    file.write(&path).unwrap();

    let mut reader = bioformats::formats::imaris::ImarisReader::new();
    reader.set_id(&path).unwrap();
    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("shorter than declared plane")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cellh5_preserves_hdf5_attributes_and_dataset_metadata() {
    let path = tmp("metadata_parity_cellh5.ch5");
    let _ = std::fs::remove_file(&path);

    let mut file = hdf5_pure::FileBuilder::new();
    file.set_attr(
        "experiment_name",
        hdf5_pure::AttrValue::String("synthetic assay".to_string()),
    );
    // CellH5Reader.java#parseStructure() walks the canonical experiment layout
    //   /sample/0/plate/{plate}/experiment/{well}/position/{site}/image/channel
    // (CellH5Constants: PREFIX_PATH "/sample/0/", PLATE "plate/", WELL
    // "/experiment/", SITE "/position/", IMAGE_PATH "image/channel/"). The
    // `image/channel` dataset is itself the 5D [channel, time, zslice, y, x]
    // image stack. Here c=1,t=2,z=1,y=2,x=3, keeping x=3, y=2, t=2.
    let mut sample = file.create_group("sample");
    let mut zero = sample.create_group("0");
    let mut plate = zero.create_group("plate");
    let mut plate0 = plate.create_group("Plate0");
    let mut experiment = plate0.create_group("experiment");
    let mut well = experiment.create_group("A01");
    let mut positions = well.create_group("position");
    let mut site = positions.create_group("1");
    let mut image = site.create_group("image");
    image
        .create_dataset("channel")
        .with_u16_data(&[1u16, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
        .with_shape(&[1, 2, 1, 2, 3])
        .set_attr("wavelength_nm", hdf5_pure::AttrValue::U32(488));
    site.add_group(image.finish());
    positions.add_group(site.finish());
    well.add_group(positions.finish());
    experiment.add_group(well.finish());
    plate0.add_group(experiment.finish());
    plate.add_group(plate0.finish());
    zero.add_group(plate.finish());
    sample.add_group(zero.finish());
    file.add_group(sample.finish());
    file.write(&path).unwrap();

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
    let ds_path = "/sample/0/plate/Plate0/experiment/A01/position/1/image/channel";
    assert!(matches!(
        metadata.get(&format!("cellh5_attr:{ds_path}@wavelength_nm")),
        Some(MetadataValue::Int(488))
    ));
    assert!(matches!(
        metadata.get(&format!("cellh5_dataset:{ds_path}")),
        Some(MetadataValue::String(value))
            if value == "shape=[1, 2, 1, 2, 3]; dtype_size=2"
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
fn nd2_rejects_out_of_bounds_region() {
    let path = tmp("region_bounds.nd2");
    write_synthetic_nd2(&path, &[1, 2]);

    let mut reader = ImageReader::open(&path).unwrap();
    let err = reader.open_bytes_region(0, 1, 0, 2, 1).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("outside image bounds")),
        "{err:?}"
    );
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

fn push_jp2_box(bytes: &mut Vec<u8>, box_type: &[u8; 4], payload: &[u8]) {
    bytes.extend_from_slice(&((payload.len() as u32) + 8).to_be_bytes());
    bytes.extend_from_slice(box_type);
    bytes.extend_from_slice(payload);
}

#[test]
fn nd2_detects_old_jp2_backed_metadata_and_series() {
    let path = tmp("old_jp2_backed.nd2");
    let mut bytes = Vec::new();

    push_jp2_box(&mut bytes, b"jP  ", &[0x0d, 0x0a, 0x87, 0x0a]);
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&2u32.to_be_bytes());
    ihdr.extend_from_slice(&3u32.to_be_bytes());
    ihdr.extend_from_slice(&1u16.to_be_bytes());
    ihdr.extend_from_slice(&0x0f07_0100u32.to_be_bytes());
    ihdr.extend_from_slice(&[0, 0, 0]);
    let mut jp2h = Vec::new();
    push_jp2_box(&mut jp2h, b"ihdr", &ihdr);
    push_jp2_box(&mut bytes, b"jp2h", &jp2h);
    for marker in [1u8, 2, 3, 4] {
        push_jp2_box(&mut bytes, b"jp2c", &[0xff, 0x4f, 0xff, 0x51, marker]);
    }
    bytes.extend_from_slice(
        br#"<MetadataSeq _SEQUENCE_INDEX="0"><uiCompCount value="2"/></MetadataSeq>
<MetadataSeq _SEQUENCE_INDEX="1"><uiCompCount value="2"/></MetadataSeq>"#,
    );
    bytes.extend_from_slice(b"LABORATORY IMAGING ND BOX MAP 00");
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.series_count(), 2);
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.metadata().size_t, 1);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
    assert!(matches!(
        reader.metadata().series_metadata.get("nd2_old_jp2"),
        Some(MetadataValue::Bool(true))
    ));
    reader.set_series(1).unwrap();
    assert_eq!(reader.metadata().image_count, 2);
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
        // Java-correct ZVI item ("/Image/Item(N)/CONTENTS") layout. The reader
        // derives all image dimensions and the pixel-data offset from this
        // stream (see ZeissZVIReader.fillMetadataPass1). Streams of <=1024 bytes
        // are skipped, so the stream must exceed that.
        let mut item: Vec<u8> = Vec::new();
        // 11 leading VT_EMPTY tags (type 0, 2 bytes each).
        item.extend_from_slice(&[0u8; 22]);
        // skipBytes(2)
        item.extend_from_slice(&[0u8; 2]);
        // len = readInt() - 20. We pad the skip(len-8) region to push the
        // stream past 1024 bytes. Choose len_raw so that skip(len-8) = 1100.
        let pad: i32 = 1100;
        let len_raw: i32 = pad + 28; // skip = (len_raw - 20) - 8 = len_raw - 28
        item.extend_from_slice(&len_raw.to_le_bytes());
        // skipBytes(8)
        item.extend_from_slice(&[0u8; 8]);
        // zidx, cidx, tidx, skip(4), tileIndex
        item.extend_from_slice(&0i32.to_le_bytes()); // zidx
        item.extend_from_slice(&0i32.to_le_bytes()); // cidx
        item.extend_from_slice(&0i32.to_le_bytes()); // tidx
        item.extend_from_slice(&[0u8; 4]); // skip
        item.extend_from_slice(&0i32.to_le_bytes()); // tileIndex
        // skipBytes(len - 8) == pad
        item.extend_from_slice(&vec![0u8; pad as usize]);
        // 5 more VT_EMPTY tags.
        item.extend_from_slice(&[0u8; 10]);
        // skipBytes(4)
        item.extend_from_slice(&[0u8; 4]);
        // sizeX, sizeY
        item.extend_from_slice(&1i32.to_le_bytes()); // sizeX
        item.extend_from_slice(&1i32.to_le_bytes()); // sizeY
        // skipBytes(4)
        item.extend_from_slice(&[0u8; 4]);
        // bpp (1 => UINT8, grayscale)
        item.extend_from_slice(&1i32.to_le_bytes());
        // skipBytes(4); skipBytes(4)
        item.extend_from_slice(&[0u8; 8]);
        // valid (use 2 so the data is treated as uncompressed)
        item.extend_from_slice(&2i32.to_le_bytes());
        // check / first pixel bytes: pixel data offset = filePointer - 4, i.e.
        // it points at this 4-byte region. First pixel value is 77.
        item.extend_from_slice(&[77u8, 0, 0, 0]);
        let mut stream = comp.create_stream("/Image/Item(1)/CONTENTS").unwrap();
        stream.write_all(&item).unwrap();
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
fn animated_gif_reads_all_frames_as_image_stack() {
    // The Java GIFReader reads every frame of an animated GIF as a separate
    // plane (sizeT == imageCount), rather than rejecting or flattening it.
    // The synthetic GIF has two 1x1 frames (red then green).
    let path = tmp("animated.gif");
    write_animated_gif(&path);

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 1);
    assert_eq!(meta.size_y, 1);
    // Two frames -> two planes.
    assert_eq!(meta.image_count, 2);
    assert_eq!(reader.series_count(), 1);

    let size_c = reader.metadata().size_c as usize;
    // Both frames decode to real (composited RGBA) pixels.
    let frame0 = reader.open_bytes(0).unwrap();
    let frame1 = reader.open_bytes(1).unwrap();
    assert_eq!(frame0.len(), size_c);
    assert_eq!(frame1.len(), size_c);
    // First frame is red, second is green (RGBA, opaque).
    assert_eq!(&frame0[..4], &[255, 0, 0, 255]);
    assert_eq!(&frame1[..4], &[0, 255, 0, 255]);
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
