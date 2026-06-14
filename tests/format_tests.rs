use bioformats::{
    BioFormatsError, DimensionOrder, FormatReader, FormatWriter, ImageMetadata, ImageReader,
    ImageWriter, MetadataValue, OmeAnnotation, OmeShape, PixelType,
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

fn write_i32_be(buf: &mut [u8], offset: usize, value: i32) {
    buf[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

fn write_i16_le(buf: &mut [u8], offset: usize, value: i16) {
    buf[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
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

fn pack_his_12(values: &[u16]) -> Vec<u8> {
    let mut out = vec![0u8; (values.len() * 12).div_ceil(8)];
    for (sample, value) in values.iter().enumerate() {
        let value = value & 0x0fff;
        let bit_base = sample * 12;
        for bit_offset in 0..12 {
            let bit_value = ((value >> (11 - bit_offset)) & 1) as u8;
            let bit = bit_base + bit_offset;
            out[bit / 8] |= bit_value << (7 - (bit % 8));
        }
    }
    out
}

fn append_ptu_int_tag(out: &mut Vec<u8>, ident: &str, value: i64) {
    append_ptu_tag(out, ident, 0x1000_0008, value);
}

fn append_ptu_float_tag(out: &mut Vec<u8>, ident: &str, value: f64) {
    append_ptu_tag(out, ident, 0x2000_0008, value.to_bits() as i64);
}

fn append_ptu_indexed_int_tag(out: &mut Vec<u8>, ident: &str, index: i32, value: i64) {
    append_ptu_indexed_tag(out, ident, index, 0x1000_0008, value);
}

fn append_ptu_tag(out: &mut Vec<u8>, ident: &str, tag_type: u32, value: i64) {
    append_ptu_indexed_tag(out, ident, -1, tag_type, value);
}

fn append_ptu_indexed_tag(out: &mut Vec<u8>, ident: &str, index: i32, tag_type: u32, value: i64) {
    let mut tag = [0u8; 48];
    let ident_bytes = ident.as_bytes();
    tag[..ident_bytes.len().min(32)].copy_from_slice(&ident_bytes[..ident_bytes.len().min(32)]);
    tag[32..36].copy_from_slice(&index.to_le_bytes());
    tag[36..40].copy_from_slice(&tag_type.to_le_bytes());
    tag[40..48].copy_from_slice(&value.to_le_bytes());
    out.extend_from_slice(&tag);
}

fn append_ptu_ansi_tag(out: &mut Vec<u8>, ident: &str, value: &str) {
    let mut payload = value.as_bytes().to_vec();
    payload.push(0);
    append_ptu_tag(out, ident, 0x4001_ffff, payload.len() as i64);
    out.extend_from_slice(&payload);
}

fn minimal_ptu_header(tags: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"PQTTTR\0\0");
    data.extend_from_slice(b"1.0\0\0\0\0\0");
    tags(&mut data);
    append_ptu_tag(&mut data, "Header_End", 0xffff_0008, 0);
    data
}

fn append_ptu_t3_marker(out: &mut Vec<u8>, marker: u8, nsync: u16) {
    let record = 0x8000_0000u32 | ((marker as u32) << 25) | u32::from(nsync & 0x03ff);
    out.extend_from_slice(&record.to_le_bytes());
}

fn append_ptu_t3_photon(out: &mut Vec<u8>, channel: u8, nsync: u16) {
    append_ptu_t3_photon_with_dtime(out, channel, 0, nsync);
}

fn append_ptu_t3_photon_with_dtime(out: &mut Vec<u8>, channel: u8, dtime: u16, nsync: u16) {
    let record =
        ((channel as u32) << 25) | (u32::from(dtime & 0x7fff) << 10) | u32::from(nsync & 0x03ff);
    out.extend_from_slice(&record.to_le_bytes());
}

fn append_ptu_t2_marker(out: &mut Vec<u8>, marker: u8, timetag: u32) {
    let record = 0x8000_0000u32 | ((marker as u32) << 25) | (timetag & 0x01ff_ffff);
    out.extend_from_slice(&record.to_le_bytes());
}

fn append_ptu_t2_photon(out: &mut Vec<u8>, channel: u8, timetag: u32) {
    let record = ((channel as u32) << 25) | (timetag & 0x01ff_ffff);
    out.extend_from_slice(&record.to_le_bytes());
}

fn sm_camera_bytes(width: u16, height: u16, pixels: &[u8]) -> Vec<u8> {
    let mut data = vec![0u8; 548];
    data[..16].copy_from_slice(&[0, 0, 0, 0, 2, 0, 0, 5, 0xc9, 0x88, 0, 5, 0xcb, 0x88, 0, 0]);
    data[524..526].copy_from_slice(&height.to_be_bytes());
    data[532..534].copy_from_slice(&width.to_be_bytes());
    data.extend_from_slice(pixels);
    data
}

fn strict_misc_raw_bytes(
    magic: &[u8; 16],
    width: u32,
    height: u32,
    planes: u32,
    pixel_type_code: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(magic);
    data.extend_from_slice(&width.to_le_bytes());
    data.extend_from_slice(&height.to_le_bytes());
    data.extend_from_slice(&planes.to_le_bytes());
    data.extend_from_slice(&pixel_type_code.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&40u64.to_le_bytes());
    data.extend_from_slice(payload);
    data
}

fn strict_misc4_raw_bytes(
    magic: &[u8; 8],
    width: u32,
    height: u32,
    planes: u32,
    pixel_type_code: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(magic);
    data.extend_from_slice(&width.to_le_bytes());
    data.extend_from_slice(&height.to_le_bytes());
    data.extend_from_slice(&planes.to_le_bytes());
    data.extend_from_slice(&pixel_type_code.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&32u64.to_le_bytes());
    data.extend_from_slice(payload);
    data
}

fn tillvision_native_cimage_contents() -> Vec<u8> {
    let mut contents = vec![0u8; 125];
    contents[0..4].copy_from_slice(b"\xf0\x3f\xff\x00");
    contents[12..16].copy_from_slice(b"\x00\x00\xff\x00");
    contents[22..26].copy_from_slice(b"\x08\x00\x04\x00");
    contents[26] = 11;
    contents[27..38].copy_from_slice(b"NativeImage");
    contents[48..50].copy_from_slice(b"sB");
    let dims = 70;
    contents[dims..dims + 4].copy_from_slice(&2u32.to_le_bytes());
    contents[dims + 4..dims + 8].copy_from_slice(&2u32.to_le_bytes());
    contents[dims + 8..dims + 12].copy_from_slice(&1u32.to_le_bytes());
    contents[dims + 12..dims + 16].copy_from_slice(&2u32.to_le_bytes());
    contents[dims + 16..dims + 20].copy_from_slice(&1u32.to_le_bytes());
    contents[dims + 20..dims + 24].copy_from_slice(&2u32.to_le_bytes());
    contents.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8]);
    contents
}

fn tillvision_native_cimage_contents_with_shifted_object_marker() -> Vec<u8> {
    let mut contents = tillvision_native_cimage_contents();
    contents.splice(22..22, [0x55; 8]);
    contents
}

fn tillvision_native_cimage_contents_with_description() -> Vec<u8> {
    let mut contents = tillvision_native_cimage_contents();
    let description = b"Date: 05/26/26\r\nStart time of experiment: 09:10:11 AM\r\nExposure time [ms]: 25.5\r\nImage type: fluorescence\r\n; ignored comment\r\n";
    contents.extend_from_slice(b"\0\0\0\0\0\xff");
    contents.extend_from_slice(&(description.len() as u16).to_le_bytes());
    contents.extend_from_slice(description);
    contents
}

fn tillvision_native_cimage_contents_with_payload_and_description(
    payload: &[u8],
    description: &[u8],
) -> Vec<u8> {
    let mut contents = tillvision_native_cimage_contents();
    contents.truncate(125);
    contents.extend_from_slice(payload);
    contents.extend_from_slice(b"\0\0\0\0\0\xff");
    contents.extend_from_slice(&(description.len() as u16).to_le_bytes());
    contents.extend_from_slice(description);
    contents
}

fn tillvision_native_cimage_contents_with_payload_at_offset(
    payload: &[u8],
    payload_offset: usize,
    description: &[u8],
) -> Vec<u8> {
    let mut contents = tillvision_native_cimage_contents();
    contents.truncate(125);
    assert!(payload_offset >= contents.len());
    contents.resize(payload_offset, 0xaa);
    contents.extend_from_slice(payload);
    contents.extend_from_slice(b"\0\0\0\0\0\xff");
    contents.extend_from_slice(&(description.len() as u16).to_le_bytes());
    contents.extend_from_slice(description);
    contents
}

fn tillvision_native_cimage_contents_with_payload_fragments(
    fragments: &[(usize, &[u8])],
    description: &[u8],
) -> Vec<u8> {
    let mut contents = tillvision_native_cimage_contents();
    contents.truncate(125);
    for &(offset, payload) in fragments {
        assert!(offset >= contents.len());
        contents.resize(offset, 0xaa);
        contents.extend_from_slice(payload);
    }
    contents.extend_from_slice(b"\0\0\0\0\0\xff");
    contents.extend_from_slice(&(description.len() as u16).to_le_bytes());
    contents.extend_from_slice(description);
    contents
}

fn write_tillvision_vws_with_contents(path: &Path, contents: &[u8]) {
    use std::io::Write;

    let mut comp = cfb::create(path).unwrap();
    comp.create_stream("/Contents")
        .unwrap()
        .write_all(contents)
        .unwrap();
}

fn strict_spm_raw_bytes(
    magic: &[u8; 16],
    width: u32,
    height: u32,
    planes: u32,
    pixel_type_code: u16,
    payload: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(magic);
    data.extend_from_slice(&width.to_le_bytes());
    data.extend_from_slice(&height.to_le_bytes());
    data.extend_from_slice(&planes.to_le_bytes());
    data.extend_from_slice(&pixel_type_code.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&40u64.to_le_bytes());
    data.extend_from_slice(payload);
    data
}

fn append_fei_ser_2d_element(
    out: &mut Vec<u8>,
    dtype: u16,
    width: u32,
    height: u32,
    pixels: &[u8],
) {
    let mut header = vec![0u8; 50];
    header[40..42].copy_from_slice(&dtype.to_le_bytes());
    header[42..46].copy_from_slice(&width.to_le_bytes());
    header[46..50].copy_from_slice(&height.to_le_bytes());
    out.extend_from_slice(&header);
    out.extend_from_slice(pixels);
}

fn synthetic_fei_ser_u8(width: u32, height: u32, frames: &[Vec<u8>]) -> Vec<u8> {
    let offset_array_offset = 28u32;
    let first_element_offset = offset_array_offset as usize + frames.len() * 4;
    let element_stride = 50 + (width * height) as usize;
    let mut data = Vec::new();
    data.extend_from_slice(&0x0197u16.to_le_bytes());
    data.extend_from_slice(&0x0210u16.to_le_bytes());
    data.extend_from_slice(&0x4122u32.to_le_bytes());
    data.extend_from_slice(&0x4152u32.to_le_bytes());
    data.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    data.extend_from_slice(&(frames.len() as u32).to_le_bytes());
    data.extend_from_slice(&offset_array_offset.to_le_bytes());
    data.extend_from_slice(&0u32.to_le_bytes());
    for i in 0..frames.len() {
        data.extend_from_slice(&((first_element_offset + i * element_stride) as u32).to_le_bytes());
    }
    for frame in frames {
        append_fei_ser_2d_element(&mut data, 1, width, height, frame);
    }
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

#[test]
fn eps_reader_rejects_invalid_raster_dimensions_instead_of_clamping() {
    let image_data = tmp("eps_bad_imagedata.eps");
    std::fs::write(
        &image_data,
        b"%!PS-Adobe-3.0 EPSF-3.0\n%ImageData: 0 2 8 1 0 1 1 \"image\"\nimage\n00\n",
    )
    .unwrap();
    let err = bioformats::formats::eps::EpsReader::new()
        .set_id(&image_data)
        .unwrap_err();
    assert!(
        err.to_string().contains("ImageData width"),
        "unexpected EPS ImageData error: {err}"
    );

    let bbox = tmp("eps_bad_bbox.eps");
    std::fs::write(
        &bbox,
        b"%!PS-Adobe-3.0 EPSF-3.0\n%%BoundingBox: 5 0 5 2\nimage\n00\n",
    )
    .unwrap();
    let err = bioformats::formats::eps::EpsReader::new()
        .set_id(&bbox)
        .unwrap_err();
    assert!(
        err.to_string().contains("BoundingBox"),
        "unexpected EPS BoundingBox error: {err}"
    );
}

#[test]
fn micromanager_rejects_invalid_dimensions_and_unknown_pixel_type() {
    let dir = isolated_tmp_dir("micromanager_validation");
    let path = dir.join("metadata.txt");

    std::fs::write(
        &path,
        r#"{"Summary":{"Width":-2,"Height":1,"Channels":1,"Slices":1,"Frames":1,"PixelType":"GRAY16"}}"#,
    )
    .unwrap();
    let err = bioformats::formats::micromanager::MicromanagerReader::new()
        .set_id(&path)
        .unwrap_err();
    assert!(
        err.to_string().contains("invalid Width -2"),
        "unexpected MicroManager width error: {err}"
    );

    std::fs::write(
        &path,
        r#"{"Summary":{"Width":1,"Height":1,"Channels":1,"Slices":1,"Frames":1,"PixelType":"GRAY12"}}"#,
    )
    .unwrap();
    let err = bioformats::formats::micromanager::MicromanagerReader::new()
        .set_id(&path)
        .unwrap_err();
    assert!(
        err.to_string().contains("unsupported PixelType GRAY12"),
        "unexpected MicroManager PixelType error: {err}"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn andor_sif_rejects_short_declared_payload_before_metadata() {
    let path = tmp("andor_short_payload.sif");
    let mut uninit = bioformats::formats::andor::AndorSifReader::new();
    assert_eq!(uninit.series_count(), 0);
    assert!(uninit.set_series(0).is_err());

    std::fs::write(
        &path,
        b"Andor Technology Multi-Channel File\nXdet 2\nYdet 2\n4\n",
    )
    .unwrap();
    let err = uninit.set_id(&path).unwrap_err();
    assert!(
        err.to_string().contains("declared data block"),
        "unexpected Andor SIF error: {err}"
    );
    assert_eq!(uninit.series_count(), 0);
    assert!(uninit.set_series(0).is_err());
}

fn synthetic_biorad_gel_with_bpp(bpp: i16) -> Vec<u8> {
    let mut data = vec![0u8; 420];
    data[0..2].copy_from_slice(&0xafafu16.to_be_bytes());
    data[160..162].copy_from_slice(&0x81i16.to_be_bytes());
    data[162..164].copy_from_slice(&0i16.to_be_bytes());
    data[166..170].copy_from_slice(&80i32.to_be_bytes());
    data[400..402].copy_from_slice(&1i16.to_be_bytes());
    data[402..404].copy_from_slice(&1i16.to_be_bytes());
    data[406..408].copy_from_slice(&bpp.to_be_bytes());
    data
}

#[test]
fn biorad_gel_rejects_unknown_bytes_per_pixel() {
    let path = tmp("biorad_gel_bad_bpp.1sc");
    std::fs::write(&path, synthetic_biorad_gel_with_bpp(3)).unwrap();
    let err = bioformats::formats::camera2::BioRadGelReader::new()
        .set_id(&path)
        .unwrap_err();
    assert!(
        err.to_string().contains("unsupported bytes per pixel 3"),
        "unexpected Bio-Rad GEL error: {err}"
    );
}

#[test]
fn camera2_stateful_readers_clear_failed_reopen_and_require_initialization() {
    let pco_good = tmp("good_pco.b16");
    let mut pco_bytes = vec![0u8; 216];
    pco_bytes[4..6].copy_from_slice(&1u16.to_le_bytes());
    pco_bytes[6..8].copy_from_slice(&1u16.to_le_bytes());
    pco_bytes.extend_from_slice(&[7, 0]);
    std::fs::write(&pco_good, pco_bytes).unwrap();
    let pco_bad = tmp("bad_pco.b16");
    std::fs::write(&pco_bad, [0u8; 4]).unwrap();

    let mut pco = bioformats::formats::camera2::PcoRawReader::new();
    assert_eq!(pco.series_count(), 0);
    assert!(matches!(
        pco.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    pco.set_id(&pco_good).unwrap();
    assert_eq!(pco.series_count(), 1);
    assert!(pco.set_id(&pco_bad).is_err());
    assert_eq!(pco.series_count(), 0);
    assert_eq!(pco.metadata().size_x, 0);

    let gel_good = tmp("good_biorad_gel.1sc");
    std::fs::write(&gel_good, synthetic_biorad_gel_with_bpp(2)).unwrap();
    let gel_bad = tmp("bad_biorad_gel.1sc");
    std::fs::write(&gel_bad, synthetic_biorad_gel_with_bpp(3)).unwrap();
    let mut gel = bioformats::formats::camera2::BioRadGelReader::new();
    assert_eq!(gel.series_count(), 0);
    assert!(matches!(
        gel.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    gel.set_id(&gel_good).unwrap();
    assert_eq!(gel.series_count(), 1);
    assert!(gel.set_id(&gel_bad).is_err());
    assert_eq!(gel.series_count(), 0);
    assert_eq!(gel.metadata().size_x, 0);

    let _ = std::fs::remove_file(pco_good);
    let _ = std::fs::remove_file(pco_bad);
    let _ = std::fs::remove_file(gel_good);
    let _ = std::fs::remove_file(gel_bad);
}

fn write_tiny_tiff_bytes(path: &Path) -> Vec<u8> {
    let mut meta = ImageMetadata::default();
    meta.size_x = 1;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.bits_per_pixel = 8;
    meta.image_count = 1;
    let mut writer = bioformats::tiff::TiffWriter::new();
    writer.set_metadata(&meta).unwrap();
    writer.set_id(path).unwrap();
    writer.save_bytes(0, &[7]).unwrap();
    writer.close().unwrap();
    std::fs::read(path).unwrap()
}

fn write_tiny_flex_tiff(path: &Path, xml: &str, pixel: u8) {
    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&8u32.to_le_bytes());

    let entry_count = 10u16;
    let ifd_len = 2usize + entry_count as usize * 12 + 4;
    let xml_offset = 8 + ifd_len;
    let mut xml_bytes = xml.as_bytes().to_vec();
    xml_bytes.push(0);
    let pixel_offset = xml_offset + xml_bytes.len();

    let short_entry = |out: &mut Vec<u8>, tag: u16, value: u16| {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&3u16.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
    };
    let long_entry = |out: &mut Vec<u8>, tag: u16, value: u32| {
        out.extend_from_slice(&tag.to_le_bytes());
        out.extend_from_slice(&4u16.to_le_bytes());
        out.extend_from_slice(&1u32.to_le_bytes());
        out.extend_from_slice(&value.to_le_bytes());
    };

    bytes.extend_from_slice(&entry_count.to_le_bytes());
    long_entry(&mut bytes, 256, 1);
    long_entry(&mut bytes, 257, 1);
    short_entry(&mut bytes, 258, 8);
    short_entry(&mut bytes, 259, 1);
    short_entry(&mut bytes, 262, 1);
    long_entry(&mut bytes, 273, pixel_offset as u32);
    short_entry(&mut bytes, 277, 1);
    long_entry(&mut bytes, 278, 1);
    long_entry(&mut bytes, 279, 1);
    bytes.extend_from_slice(&65200u16.to_le_bytes());
    bytes.extend_from_slice(&2u16.to_le_bytes());
    bytes.extend_from_slice(&(xml_bytes.len() as u32).to_le_bytes());
    bytes.extend_from_slice(&(xml_offset as u32).to_le_bytes());
    bytes.extend_from_slice(&0u32.to_le_bytes());
    bytes.extend_from_slice(&xml_bytes);
    bytes.push(pixel);

    std::fs::write(path, bytes).unwrap();
}

#[test]
fn flex_rejects_bad_xml_factor_counts_and_clears_failed_state() {
    let dir = isolated_tmp_dir("flex_validation");
    let good = dir.join("good.flex");
    write_tiny_flex_tiff(
        &good,
        r#"<Arrays><Array Name="p0" Factor="1"/></Arrays>"#,
        7,
    );

    let mut reader = bioformats::formats::flex::FlexReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    reader.set_id(&good).unwrap();
    assert_eq!(reader.series_count(), 1);

    let bad_count = dir.join("bad_count.flex");
    write_tiny_flex_tiff(
        &bad_count,
        r#"<Arrays><Array Name="p0" Factor="1"/><Array Name="p1" Factor="1"/></Arrays>"#,
        9,
    );
    let err = reader.set_id(&bad_count).unwrap_err();
    assert!(
        err.to_string().contains("XML Array count"),
        "unexpected Flex count error: {err}"
    );
    assert_eq!(reader.series_count(), 0);

    let bad_factor = dir.join("bad_factor.flex");
    write_tiny_flex_tiff(
        &bad_factor,
        r#"<Arrays><Array Name="p0" Factor="NaN"/></Arrays>"#,
        9,
    );
    let err = reader.set_id(&bad_factor).unwrap_err();
    assert!(
        err.to_string().contains("invalid Array Factor"),
        "unexpected Flex factor error: {err}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn ipw_rejects_zero_imageinfo_axes_instead_of_clamping() {
    use std::io::Write;

    let tiff_path = tmp("ipw_embedded.tif");
    let tiff = write_tiny_tiff_bytes(&tiff_path);
    let path = tmp("zero_axis.ipw");
    let mut comp = cfb::create(&path).unwrap();
    comp.create_storage_all("/0").unwrap();
    comp.create_stream("/0/ImageTIFF")
        .unwrap()
        .write_all(&tiff)
        .unwrap();
    comp.create_stream("/ImageInfo")
        .unwrap()
        .write_all(b"channels=0\nslices=1\nframes=1\n")
        .unwrap();
    drop(comp);

    let err = bioformats::formats::camera2::IpwReader::new()
        .set_id(&path)
        .unwrap_err();
    assert!(
        err.to_string().contains("channels must be positive"),
        "unexpected IPW error: {err}"
    );

    let _ = std::fs::remove_file(tiff_path);
    let _ = std::fs::remove_file(path);
}

#[test]
fn aim_rejects_missing_magic_zero_dimensions_and_short_payload() {
    let mut uninit = bioformats::formats::aim::AimReader::new();
    assert_eq!(uninit.series_count(), 0);
    assert!(matches!(
        uninit.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));

    let random = tmp("random.aim");
    let mut bytes = vec![0u8; 512];
    bytes[56..60].copy_from_slice(&1i32.to_le_bytes());
    bytes[60..64].copy_from_slice(&1i32.to_le_bytes());
    bytes[64..68].copy_from_slice(&1i32.to_le_bytes());
    bytes.extend_from_slice(&[1, 2]);
    std::fs::write(&random, bytes).unwrap();
    let mut reader = bioformats::formats::aim::AimReader::new();
    let err = reader.set_id(&random).unwrap_err();
    assert!(err.to_string().contains("AIMDATA"));
    let _ = std::fs::remove_file(&random);

    let zero = tmp("zero.aim");
    let mut bytes = vec![0u8; 512];
    bytes[..12].copy_from_slice(b"AIMDATA_V020");
    bytes[60..64].copy_from_slice(&1i32.to_le_bytes());
    bytes[64..68].copy_from_slice(&1i32.to_le_bytes());
    bytes.extend_from_slice(&[1, 2]);
    std::fs::write(&zero, bytes).unwrap();
    let mut reader = bioformats::formats::aim::AimReader::new();
    let err = reader.set_id(&zero).unwrap_err();
    assert!(err.to_string().contains("non-positive AIM width"));
    let _ = std::fs::remove_file(&zero);

    let short = tmp("short.aim");
    let mut bytes = vec![0u8; 512];
    bytes[..12].copy_from_slice(b"AIMDATA_V020");
    bytes[56..60].copy_from_slice(&2i32.to_le_bytes());
    bytes[60..64].copy_from_slice(&2i32.to_le_bytes());
    bytes[64..68].copy_from_slice(&1i32.to_le_bytes());
    bytes[160..512].fill(b'x');
    bytes.extend_from_slice(&[1, 2]);
    std::fs::write(&short, bytes).unwrap();
    let mut reader = bioformats::formats::aim::AimReader::new();
    let err = reader.set_id(&short).unwrap_err();
    assert!(err.to_string().contains("shorter than declared"));
    let _ = std::fs::remove_file(&short);

    let short_isq = tmp("short.isq");
    let mut bytes = vec![0u8; 512];
    bytes[..16].copy_from_slice(b"CTDATA-HEADER_V1");
    bytes[28..32].copy_from_slice(&1i32.to_le_bytes());
    bytes[32..36].copy_from_slice(&1i32.to_le_bytes());
    bytes[36..40].copy_from_slice(&1i32.to_le_bytes());
    std::fs::write(&short_isq, bytes).unwrap();
    let mut reader = bioformats::formats::aim::AimReader::new();
    let err = reader.set_id(&short_isq).unwrap_err();
    assert!(err.to_string().contains("shorter than declared"));
    let _ = std::fs::remove_file(&short_isq);
}

#[test]
fn gatan_rejects_weak_dm_magic_and_short_dm2_payload() {
    let mut reader = bioformats::formats::gatan::GatanReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    assert!(!reader.is_this_type_by_bytes(&[0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 2, 0, 0, 0, 0,]));
    assert!(!reader.is_this_type_by_bytes(&[0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2,]));
    assert!(reader.is_this_type_by_bytes(&[0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 1, 0, 0, 0, 0,]));
    assert!(reader.is_this_type_by_bytes(&[0, 0, 0, 4, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1,]));

    let dm2 = tmp("short.dm2");
    let mut bytes = vec![0u8; 24];
    bytes[..4].copy_from_slice(&0x003d_0000i32.to_be_bytes());
    bytes[16..18].copy_from_slice(&2i16.to_be_bytes());
    bytes[18..20].copy_from_slice(&2i16.to_be_bytes());
    bytes[20..22].copy_from_slice(&1i16.to_be_bytes());
    bytes[22..24].copy_from_slice(&0i16.to_be_bytes());
    std::fs::write(&dm2, bytes).unwrap();
    let mut dm2_reader = bioformats::formats::gatan::Dm2Reader::new();
    assert_eq!(dm2_reader.series_count(), 0);
    assert!(matches!(
        dm2_reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    let err = dm2_reader.set_id(&dm2).unwrap_err();
    assert!(err.to_string().contains("shorter than declared"));
    let _ = std::fs::remove_file(dm2);

    let zero = tmp("zero.dm2");
    let mut bytes = vec![0u8; 24];
    bytes[..4].copy_from_slice(&0x003d_0000i32.to_be_bytes());
    bytes[18..20].copy_from_slice(&1i16.to_be_bytes());
    bytes[20..22].copy_from_slice(&1i16.to_be_bytes());
    std::fs::write(&zero, bytes).unwrap();
    let mut dm2_reader = bioformats::formats::gatan::Dm2Reader::new();
    let err = dm2_reader.set_id(&zero).unwrap_err();
    assert!(err.to_string().contains("non-positive"));
    assert_eq!(dm2_reader.series_count(), 0);
    let _ = std::fs::remove_file(zero);
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
fn iplab_rejects_non_positive_dimensions_and_truncated_payload() {
    let zero = tmp("zero_dim.ipl");
    let mut data = vec![0u8; 96];
    data[..8].copy_from_slice(b"ipl bina");
    write_i32_le(&mut data, 12, 0);
    write_i32_le(&mut data, 16, 1);
    write_i32_le(&mut data, 20, 1);
    write_i32_le(&mut data, 24, 1);
    write_i32_le(&mut data, 28, 1);
    write_i32_le(&mut data, 32, 4);
    std::fs::write(&zero, &data).unwrap();

    let mut reader = bioformats::formats::norpix::IplabReader::new();
    let err = reader.set_id(&zero).unwrap_err();
    assert!(err.to_string().contains("non-positive"));
    let _ = std::fs::remove_file(&zero);

    let truncated = tmp("truncated.ipl");
    write_i32_le(&mut data, 12, 2);
    std::fs::write(&truncated, data).unwrap();

    let mut reader = bioformats::formats::norpix::IplabReader::new();
    let err = reader.set_id(&truncated).unwrap_err();
    assert!(err.to_string().contains("truncated"));
    let _ = std::fs::remove_file(truncated);
}

#[test]
fn iplab_rejects_unknown_data_type_and_requires_initialization_for_series() {
    let path = tmp("unknown_dtype.ipl");
    let mut data = vec![0u8; 96];
    data[..8].copy_from_slice(b"ipl bina");
    write_i32_le(&mut data, 12, 1);
    write_i32_le(&mut data, 16, 1);
    write_i32_le(&mut data, 20, 1);
    write_i32_le(&mut data, 24, 1);
    write_i32_le(&mut data, 28, 1);
    write_i32_le(&mut data, 32, 99);
    std::fs::write(&path, &data).unwrap();

    let mut reader = bioformats::formats::norpix::IplabReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    let err = reader.set_id(&path).unwrap_err();
    assert!(err.to_string().contains("unsupported data type 99"));
    assert_eq!(reader.series_count(), 0);
    let _ = std::fs::remove_file(path);
}

#[test]
fn norpix_seq_rejects_clamped_dimensions_unknown_format_and_short_payload() {
    fn seq_header(frames: u32, width: u32, height: u32, desc_fmt: u32, true_size: u32) -> Vec<u8> {
        let mut data = vec![0u8; 1024];
        data[..10].copy_from_slice(b"Norpix seq");
        data[548..552].copy_from_slice(&frames.to_le_bytes());
        data[572..576].copy_from_slice(&true_size.to_le_bytes());
        data[592..596].copy_from_slice(&desc_fmt.to_le_bytes());
        data[596..600].copy_from_slice(&width.to_le_bytes());
        data[600..604].copy_from_slice(&height.to_le_bytes());
        data
    }

    let zero = tmp("zero_dim.seq");
    std::fs::write(&zero, seq_header(1, 0, 1, 0, 1)).unwrap();
    let mut reader = bioformats::formats::norpix::NorpixReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    let err = reader.set_id(&zero).unwrap_err();
    assert!(err.to_string().contains("non-positive"));
    let _ = std::fs::remove_file(&zero);

    let unknown = tmp("unknown.seq");
    std::fs::write(&unknown, seq_header(1, 1, 1, 77, 1)).unwrap();
    let err = bioformats::formats::norpix::NorpixReader::new()
        .set_id(&unknown)
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("unsupported description format 77"));
    let _ = std::fs::remove_file(&unknown);

    let short = tmp("short.seq");
    std::fs::write(&short, seq_header(1, 2, 2, 0, 4)).unwrap();
    let err = bioformats::formats::norpix::NorpixReader::new()
        .set_id(&short)
        .unwrap_err();
    assert!(err.to_string().contains("shorter than declared"));
    let _ = std::fs::remove_file(short);

    let compressed_short = tmp("compressed_short.seq");
    let mut data = seq_header(1, 1, 1, 100, 1);
    data.extend_from_slice(&4u32.to_le_bytes());
    data.extend_from_slice(&[0xff, 0xd8]);
    std::fs::write(&compressed_short, data).unwrap();
    let err = bioformats::formats::norpix::NorpixReader::new()
        .set_id(&compressed_short)
        .unwrap_err();
    assert!(err
        .to_string()
        .contains("compressed frame 0 payload is shorter than declared"));
    let _ = std::fs::remove_file(compressed_short);

    let unsupported_compression = tmp("unsupported_compression.seq");
    let mut data = seq_header(1, 1, 1, 0, 1);
    data[612..616].copy_from_slice(&7u32.to_le_bytes());
    std::fs::write(&unsupported_compression, data).unwrap();
    let err = bioformats::formats::norpix::NorpixReader::new()
        .set_id(&unsupported_compression)
        .unwrap_err();
    assert!(err.to_string().contains("unsupported compression code 7"));
    let _ = std::fs::remove_file(unsupported_compression);
}

#[test]
fn norpix_seq_preserves_header_metadata_and_timestamps_in_ome() {
    let path = tmp("metadata.seq");
    let mut data = vec![0u8; 1024];
    data[..10].copy_from_slice(b"Norpix seq");
    data[24..32].copy_from_slice(&3i64.to_le_bytes());
    data[32..36].copy_from_slice(&1024i32.to_le_bytes());
    data[548..552].copy_from_slice(&2u32.to_le_bytes());
    data[572..576].copy_from_slice(&10u32.to_le_bytes());
    data[592..596].copy_from_slice(&0u32.to_le_bytes());
    data[596..600].copy_from_slice(&2u32.to_le_bytes());
    data[600..604].copy_from_slice(&1u32.to_le_bytes());
    data.extend_from_slice(&[1, 2]);
    data.extend_from_slice(&1000u32.to_le_bytes());
    data.extend_from_slice(&250u16.to_le_bytes());
    data.extend_from_slice(&500u16.to_le_bytes());
    data.extend_from_slice(&[3, 4]);
    data.extend_from_slice(&1002u32.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::norpix::NorpixReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_z, 2);
    assert!(matches!(
        meta.series_metadata.get("norpix.version"),
        Some(MetadataValue::Int(3))
    ));
    assert!(matches!(
        meta.series_metadata.get("norpix.description_format"),
        Some(MetadataValue::Int(0))
    ));
    assert_eq!(reader.open_bytes(1).unwrap(), vec![3, 4]);

    let ome = reader.ome_metadata().expect("Norpix OME metadata");
    assert_eq!(ome.images[0].planes.len(), 2);
    assert_eq!(ome.images[0].planes[0].delta_t, Some(0.0));
    assert!((ome.images[0].planes[1].delta_t.unwrap() - 1.7495).abs() < 1.0e-12);
    let original = ome
        .annotations
        .iter()
        .find_map(|annotation| match annotation {
            OmeAnnotation::MapAnnotation {
                id,
                namespace,
                values,
            } if id.as_deref() == Some("Annotation:OriginalMetadata:0")
                && namespace.as_deref()
                    == Some("openmicroscopy.org/bioformats/original-metadata") =>
            {
                Some(values)
            }
            _ => None,
        })
        .expect("Norpix original metadata annotation");
    assert!(original
        .iter()
        .any(|(key, value)| key == "norpix.true_image_size" && value == "10"));
    assert!(original
        .iter()
        .any(|(key, _)| key == "norpix.timestamps_unix_seconds"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn biorad_rejects_non_positive_dimensions_and_truncated_payload() {
    let zero = tmp("zero_dim.pic");
    let mut data = vec![0u8; 76];
    write_i16_le(&mut data, 0, 0);
    write_i16_le(&mut data, 2, 1);
    write_i16_le(&mut data, 4, 1);
    write_i16_le(&mut data, 14, 1);
    write_i16_le(&mut data, 54, 12345);
    std::fs::write(&zero, &data).unwrap();

    let mut reader = bioformats::formats::biorad::BioRadReader::new();
    let err = reader.set_id(&zero).unwrap_err();
    assert!(err.to_string().contains("non-positive"));
    let _ = std::fs::remove_file(&zero);

    let truncated = tmp("truncated.pic");
    write_i16_le(&mut data, 0, 2);
    write_i16_le(&mut data, 2, 2);
    std::fs::write(&truncated, data).unwrap();

    let mut reader = bioformats::formats::biorad::BioRadReader::new();
    let err = reader.set_id(&truncated).unwrap_err();
    assert!(err.to_string().contains("shorter than declared"));
    let _ = std::fs::remove_file(truncated);
}

#[test]
fn biorad_requires_initialization_for_series_and_clears_after_failed_reopen() {
    let valid = tmp("valid_biorad.pic");
    let mut data = vec![0u8; 76];
    write_i16_le(&mut data, 0, 1);
    write_i16_le(&mut data, 2, 1);
    write_i16_le(&mut data, 4, 1);
    write_i16_le(&mut data, 14, 1);
    write_i16_le(&mut data, 54, 12345);
    data.push(7);
    std::fs::write(&valid, data).unwrap();

    let invalid = tmp("invalid_biorad.pic");
    std::fs::write(&invalid, [0u8; 76]).unwrap();

    let mut reader = bioformats::formats::biorad::BioRadReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    reader.set_id(&valid).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert!(reader.set_id(&invalid).is_err());
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_file(valid);
    let _ = std::fs::remove_file(invalid);
}

#[test]
fn imagic_does_not_claim_magicless_headers_by_bytes() {
    let mut header = vec![0u8; 1024];
    header[56..60].copy_from_slice(b"REAL");
    let reader = bioformats::formats::imagic::ImagicReader::new();
    assert!(!reader.is_this_type_by_bytes(&header));
}

#[test]
fn imagic_rejects_non_positive_dimensions() {
    let dir = isolated_tmp_dir("imagic_zero_dim");
    let hed = dir.join("sample.hed");
    let img = dir.join("sample.img");
    let mut header = vec![0u8; 1024];
    write_i32_le(&mut header, 48, 1);
    write_i32_le(&mut header, 52, 0);
    header[56..60].copy_from_slice(b"REAL");
    std::fs::write(&hed, header).unwrap();
    std::fs::write(&img, [0u8; 4]).unwrap();

    let mut reader = bioformats::formats::imagic::ImagicReader::new();
    let err = reader.set_id(&hed).unwrap_err();
    assert!(err.to_string().contains("non-positive"));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn imagic_rejects_unknown_pixel_type_and_requires_initialization_for_series() {
    let dir = isolated_tmp_dir("imagic_unknown_type");
    let hed = dir.join("sample.hed");
    let img = dir.join("sample.img");
    let mut header = vec![0u8; 1024];
    write_i32_le(&mut header, 48, 1);
    write_i32_le(&mut header, 52, 1);
    header[56..60].copy_from_slice(b"????");
    std::fs::write(&hed, header).unwrap();
    std::fs::write(&img, [0u8; 4]).unwrap();

    let mut reader = bioformats::formats::imagic::ImagicReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    let err = reader.set_id(&hed).unwrap_err();
    assert!(err.to_string().contains("unsupported pixel type"));
    assert_eq!(reader.series_count(), 0);
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn imagic_ome_image_name_uses_last_header_record_like_java() {
    let dir = isolated_tmp_dir("imagic_image_name");
    let hed = dir.join("sample.hed");
    let img = dir.join("sample.img");
    let mut headers = vec![0u8; 2048];
    for record in 0..2 {
        let off = record * 1024;
        write_i32_le(&mut headers, off + 48, 1);
        write_i32_le(&mut headers, off + 52, 1);
        headers[off + 56..off + 60].copy_from_slice(b"PACK");
    }
    headers[116..116 + 5].copy_from_slice(b"first");
    headers[1024 + 116..1024 + 116 + 12].copy_from_slice(b"  last name ");
    std::fs::write(&hed, headers).unwrap();
    std::fs::write(&img, [7u8, 9]).unwrap();

    let mut reader = bioformats::formats::imagic::ImagicReader::new();
    reader.set_id(&hed).unwrap();
    assert_eq!(reader.metadata().image_count, 2);
    let ome = reader.ome_metadata().unwrap();
    assert_eq!(ome.images[0].name.as_deref(), Some("last name"));

    let _ = std::fs::remove_dir_all(dir);
}

// Build a binary TopoMetrix fixture matching the Java TopometrixReader layout:
// "#R" magic in the 2-byte pad, version ASCII at [2..6), pixelOffset ASCII at
// [8..12), an empty date line at offset 14 (so the comment region ends at 254),
// then sizeX@406 / sizeY@410, with UINT16 LE pixels at the declared pixelOffset
// (412 = the fixed header size used here).
fn topometrix_fixture(version: &[u8; 4], size_x: i16, size_y: i16, pixels: &[u16]) -> Vec<u8> {
    let mut data = vec![0u8; 412];
    data[0..2].copy_from_slice(b"#R");
    data[2..6].copy_from_slice(version);
    data[8..12].copy_from_slice(b"412 ");
    data[14] = b'\n';
    data[406..408].copy_from_slice(&size_x.to_le_bytes());
    data[410..412].copy_from_slice(&size_y.to_le_bytes());
    for &p in pixels {
        data.extend_from_slice(&p.to_le_bytes());
    }
    data
}

#[test]
fn topometrix_requires_declared_dimensions() {
    // A binary TopoMetrix header with a non-positive declared size cannot
    // describe a real plane and is rejected.
    let path = tmp("missing_dims.tfr");
    std::fs::write(&path, topometrix_fixture(b"1.0 ", 0, 2, &[])).unwrap();

    let mut reader = bioformats::formats::afm::TopoMetrixReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("invalid dimensions")),
        "{err:?}"
    );
}

#[test]
fn topometrix_rejects_malformed_version_field() {
    // The version is a 4-byte ASCII numeric field (Java parses it as a double);
    // a non-numeric value is rejected.
    let path = tmp("bad_version.tfr");
    std::fs::write(&path, topometrix_fixture(b"abcd", 1, 1, &[])).unwrap();

    let mut reader = bioformats::formats::afm::TopoMetrixReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("invalid version field")),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn topometrix_region_crops_real_pixels() {
    let path = tmp("real_crop.tfr");
    std::fs::write(
        &path,
        topometrix_fixture(b"1.0 ", 3, 2, &[1, 2, 3, 4, 5, 6]),
    )
    .unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let crop = reader.open_bytes_region(0, 1, 0, 2, 2).unwrap();
    assert_eq!(crop, vec![2, 0, 3, 0, 5, 0, 6, 0]);
}

#[test]
fn picoquant_ptu_reconstructs_hydraharp_t3_marker_raster() {
    let path = tmp("minimal.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 2);
        append_ptu_int_tag(out, "ImgHdr_PixY", 2);
        append_ptu_int_tag(out, "ImgHdr_Frame", 1);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 9);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0304);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t3_marker(&mut data, 1, 0);
    append_ptu_t3_photon(&mut data, 0, 1);
    append_ptu_t3_photon(&mut data, 1, 3);
    append_ptu_t3_marker(&mut data, 2, 4);
    append_ptu_t3_marker(&mut data, 1, 4);
    append_ptu_t3_photon(&mut data, 0, 5);
    append_ptu_t3_photon(&mut data, 0, 5);
    append_ptu_t3_photon(&mut data, 0, 7);
    append_ptu_t3_marker(&mut data, 2, 8);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.size_t, 1);
    assert_eq!(meta.image_count, 1);
    assert_eq!(meta.pixel_type, PixelType::Uint32);
    assert!(matches!(
        meta.series_metadata.get("ptu.ImgHdr_PixX"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.TTResult_NumberOfRecords"),
        Some(MetadataValue::Int(9))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction"),
        Some(MetadataValue::String(value)) if value.contains("HydraHarp T3")
    ));

    let plane = reader.open_bytes(0).unwrap();
    let counts: Vec<u32> = plane
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(counts, vec![1, 1, 2, 1]);
    let crop = reader.open_bytes_region(0, 1, 0, 1, 2).unwrap();
    let crop_counts: Vec<u32> = crop
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(crop_counts, vec![1, 1]);
    assert!(matches!(
        reader.open_bytes_region(1, 0, 0, 1, 1),
        Err(BioFormatsError::PlaneOutOfRange(1))
    ));
    assert!(matches!(
        reader.open_bytes(1),
        Err(BioFormatsError::PlaneOutOfRange(1))
    ));
    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_reconstructs_timeharp_t3_marker_raster() {
    let path = tmp("timeharp_t3_marker_raster.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 2);
        append_ptu_int_tag(out, "ImgHdr_PixY", 1);
        append_ptu_int_tag(out, "ImgHdr_Frame", 1);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 4);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0305);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t3_marker(&mut data, 1, 0);
    append_ptu_t3_photon(&mut data, 0, 1);
    append_ptu_t3_photon(&mut data, 0, 3);
    append_ptu_t3_marker(&mut data, 2, 4);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.image_count, 1);
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_type"),
        Some(MetadataValue::String(value)) if value == "TimeHarp 260N T3"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_hydraharp_layout"),
        Some(MetadataValue::Bool(false))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction"),
        Some(MetadataValue::String(value)) if value.contains("TimeHarp 260N T3")
    ));

    let counts: Vec<u32> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(counts, vec![1, 1]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_reconstructs_timeharp_t2_marker_raster() {
    let path = tmp("timeharp_t2_marker_raster.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 2);
        append_ptu_int_tag(out, "ImgHdr_PixY", 1);
        append_ptu_int_tag(out, "ImgHdr_Frame", 1);
        append_ptu_int_tag(out, "ImgHdr_DetectorChannels", 2);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 5);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0206);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t2_marker(&mut data, 1, 0);
    append_ptu_t2_photon(&mut data, 0, 1);
    append_ptu_t2_photon(&mut data, 1, 2);
    append_ptu_t2_photon(&mut data, 1, 3);
    append_ptu_t2_marker(&mut data, 2, 4);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.image_count, 2);
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_type"),
        Some(MetadataValue::String(value)) if value == "TimeHarp 260P T2"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction"),
        Some(MetadataValue::String(value)) if value.contains("TimeHarp 260P T2")
    ));

    let channel_0: Vec<u32> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    let channel_1: Vec<u32> = reader
        .open_bytes(1)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(channel_0, vec![1, 0]);
    assert_eq!(channel_1, vec![0, 2]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_reconstructs_multiharp_t3_marker_raster() {
    let path = tmp("multiharp_t3_marker_raster.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 2);
        append_ptu_int_tag(out, "ImgHdr_PixY", 1);
        append_ptu_int_tag(out, "ImgHdr_Frame", 1);
        append_ptu_int_tag(out, "ImgHdr_DetectorChannels", 2);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 5);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0307);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t3_marker(&mut data, 1, 0);
    append_ptu_t3_photon(&mut data, 0, 1);
    append_ptu_t3_photon(&mut data, 1, 3);
    append_ptu_t3_photon(&mut data, 1, 3);
    append_ptu_t3_marker(&mut data, 2, 4);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.image_count, 2);
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_type"),
        Some(MetadataValue::String(value)) if value == "MultiHarp T3"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_layout"),
        Some(MetadataValue::String(value)) if value == "hydraharp-compatible"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_marker_raster_layout"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_hydraharp_layout"),
        Some(MetadataValue::Bool(false))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction"),
        Some(MetadataValue::String(value)) if value.contains("MultiHarp T3")
    ));

    let channel_0: Vec<u32> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    let channel_1: Vec<u32> = reader
        .open_bytes(1)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(channel_0, vec![1, 0]);
    assert_eq!(channel_1, vec![0, 2]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_reconstructs_multiharp_t2_marker_raster() {
    let path = tmp("multiharp_t2_marker_raster.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 2);
        append_ptu_int_tag(out, "ImgHdr_PixY", 1);
        append_ptu_int_tag(out, "ImgHdr_Frame", 1);
        append_ptu_int_tag(out, "ImgHdr_DetectorChannels", 2);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 5);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0207);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t2_marker(&mut data, 1, 0);
    append_ptu_t2_photon(&mut data, 0, 1);
    append_ptu_t2_photon(&mut data, 1, 2);
    append_ptu_t2_photon(&mut data, 1, 3);
    append_ptu_t2_marker(&mut data, 2, 4);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.image_count, 2);
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_type"),
        Some(MetadataValue::String(value)) if value == "MultiHarp T2"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_layout"),
        Some(MetadataValue::String(value)) if value == "hydraharp-compatible"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction"),
        Some(MetadataValue::String(value)) if value.contains("MultiHarp T2")
    ));

    let channel_0: Vec<u32> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    let channel_1: Vec<u32> = reader
        .open_bytes(1)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(channel_0, vec![1, 0]);
    assert_eq!(channel_1, vec![0, 2]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_splits_hydraharp_t3_detector_channels() {
    let path = tmp("minimal_detector_channels.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 2);
        append_ptu_int_tag(out, "ImgHdr_PixY", 1);
        append_ptu_int_tag(out, "ImgHdr_Frame", 1);
        append_ptu_int_tag(out, "ImgHdr_DetectorChannels", 2);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 5);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0304);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t3_marker(&mut data, 1, 0);
    append_ptu_t3_photon(&mut data, 0, 1);
    append_ptu_t3_photon(&mut data, 1, 3);
    append_ptu_t3_photon(&mut data, 1, 3);
    append_ptu_t3_marker(&mut data, 2, 4);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 1);
    assert_eq!(meta.image_count, 2);
    assert!(matches!(
        meta.series_metadata.get("ptu.detector_channels"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction"),
        Some(MetadataValue::String(value)) if value.contains("split into 2 detector channels")
    ));

    let channel_0: Vec<u32> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    let channel_1: Vec<u32> = reader
        .open_bytes(1)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(channel_0, vec![1, 0]);
    assert_eq!(channel_1, vec![0, 2]);

    let crop = reader.open_bytes_region(1, 1, 0, 1, 1).unwrap();
    assert_eq!(u32::from_le_bytes(crop.try_into().unwrap()), 2);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_splits_hydraharp_t3_lifetime_bins() {
    let path = tmp("minimal_lifetime_bins.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 2);
        append_ptu_int_tag(out, "ImgHdr_PixY", 1);
        append_ptu_int_tag(out, "ImgHdr_Frame", 1);
        append_ptu_int_tag(out, "ImgHdr_LifetimeBins", 3);
        append_ptu_int_tag(out, "ImgHdr_LifetimeBinWidth", 8);
        append_ptu_float_tag(out, "MeasDesc_Resolution", 2.5e-11);
        append_ptu_float_tag(out, "MeasDesc_GlobalResolution", 1.0e-8);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 6);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0304);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t3_marker(&mut data, 1, 0);
    append_ptu_t3_photon_with_dtime(&mut data, 0, 0, 1);
    append_ptu_t3_photon_with_dtime(&mut data, 0, 9, 1);
    append_ptu_t3_photon_with_dtime(&mut data, 0, 16, 3);
    append_ptu_t3_photon_with_dtime(&mut data, 0, 23, 3);
    append_ptu_t3_marker(&mut data, 2, 4);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_c, 3);
    assert_eq!(meta.image_count, 3);
    assert!(matches!(
        meta.series_metadata.get("ptu.lifetime_bins"),
        Some(MetadataValue::Int(3))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.acquisition_mode"),
        Some(MetadataValue::String(value)) if value == "tttr_t3"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.tttr_record_type"),
        Some(MetadataValue::String(value)) if value == "HydraHarp T3"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.lifetime_dtime_resolution_seconds"),
        Some(MetadataValue::Float(value)) if (*value - 2.5e-11).abs() < 1.0e-18
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.lifetime_bin_width_dtime"),
        Some(MetadataValue::Int(8))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.lifetime_bin_width_seconds"),
        Some(MetadataValue::Float(value)) if (*value - 2.0e-10).abs() < 1.0e-18
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.lifetime_range_seconds"),
        Some(MetadataValue::Float(value)) if (*value - 6.0e-10).abs() < 1.0e-18
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.sync_resolution_seconds"),
        Some(MetadataValue::Float(value)) if (*value - 1.0e-8).abs() < 1.0e-18
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction"),
        Some(MetadataValue::String(value)) if value.contains("3 lifetime bins")
    ));

    let bin_0: Vec<u32> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    let bin_1: Vec<u32> = reader
        .open_bytes(1)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    let bin_2: Vec<u32> = reader
        .open_bytes(2)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(bin_0, vec![1, 0]);
    assert_eq!(bin_1, vec![1, 0]);
    assert_eq!(bin_2, vec![0, 2]);

    let crop = reader.open_bytes_region(2, 1, 0, 1, 1).unwrap();
    assert_eq!(u32::from_le_bytes(crop.try_into().unwrap()), 2);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_reconstructs_hydraharp_t2_marker_raster() {
    let path = tmp("minimal_t2.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 2);
        append_ptu_int_tag(out, "ImgHdr_PixY", 1);
        append_ptu_int_tag(out, "ImgHdr_Frame", 1);
        append_ptu_int_tag(out, "ImgHdr_DetectorChannels", 2);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 6);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0204);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t2_marker(&mut data, 1, 0);
    append_ptu_t2_photon(&mut data, 0, 1);
    append_ptu_t2_photon(&mut data, 1, 2);
    append_ptu_t2_photon(&mut data, 1, 3);
    append_ptu_t2_photon(&mut data, 0, 3);
    append_ptu_t2_marker(&mut data, 2, 4);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.size_t, 1);
    assert_eq!(meta.image_count, 2);
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction"),
        Some(MetadataValue::String(value)) if value.contains("HydraHarp T2")
    ));

    let channel_0: Vec<u32> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    let channel_1: Vec<u32> = reader
        .open_bytes(1)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(channel_0, vec![1, 1]);
    assert_eq!(channel_1, vec![0, 2]);

    let crop = reader.open_bytes_region(1, 1, 0, 1, 1).unwrap();
    assert_eq!(u32::from_le_bytes(crop.try_into().unwrap()), 2);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_applies_bidirectional_scan_correction() {
    let path = tmp("minimal_bidirectional.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 4);
        append_ptu_int_tag(out, "ImgHdr_PixY", 2);
        append_ptu_int_tag(out, "ImgHdr_Frame", 1);
        append_ptu_int_tag(out, "ImgHdr_BiDirectional", 1);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 6);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0304);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t3_marker(&mut data, 1, 0);
    append_ptu_t3_photon(&mut data, 0, 1);
    append_ptu_t3_marker(&mut data, 2, 4);
    append_ptu_t3_marker(&mut data, 1, 4);
    append_ptu_t3_photon(&mut data, 0, 5);
    append_ptu_t3_marker(&mut data, 2, 8);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata.get("ptu.bidirectional"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction"),
        Some(MetadataValue::String(value)) if value.contains("bidirectional scan correction")
    ));

    let counts: Vec<u32> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(counts, vec![0, 1, 0, 0, 0, 0, 1, 0]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_rejects_invalid_bidirectional_tag() {
    let path = tmp("bad_bidirectional.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 1);
        append_ptu_int_tag(out, "ImgHdr_PixY", 1);
        append_ptu_int_tag(out, "ImgHdr_BiDirectional", 2);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 3);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0304);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t3_marker(&mut data, 1, 0);
    append_ptu_t3_photon(&mut data, 0, 0);
    append_ptu_t3_marker(&mut data, 2, 1);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value)) if value.contains("bidirectional scan tag must be 0 or 1")
    ));
    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("bidirectional scan tag must be 0 or 1")),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_rejects_t2_lifetime_binning() {
    let path = tmp("bad_t2_lifetime.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 1);
        append_ptu_int_tag(out, "ImgHdr_PixY", 1);
        append_ptu_int_tag(out, "ImgHdr_LifetimeBins", 2);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 3);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0204);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t2_marker(&mut data, 1, 0);
    append_ptu_t2_photon(&mut data, 0, 1);
    append_ptu_t2_marker(&mut data, 2, 2);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value)) if value.contains("T2 records do not carry lifetime dtime values")
    ));
    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("T2 records do not carry lifetime dtime values")),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_rejects_lifetime_bin_outside_declared_split() {
    let path = tmp("bad_lifetime_bin.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 1);
        append_ptu_int_tag(out, "ImgHdr_PixY", 1);
        append_ptu_int_tag(out, "ImgHdr_LifetimeBins", 2);
        append_ptu_int_tag(out, "ImgHdr_LifetimeBinWidth", 4);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 3);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0304);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t3_marker(&mut data, 1, 0);
    append_ptu_t3_photon_with_dtime(&mut data, 0, 8, 1);
    append_ptu_t3_marker(&mut data, 2, 2);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value)) if value.contains("photon lifetime bin 2 exceeds declared lifetime bin count 2")
    ));
    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("photon lifetime bin 2 exceeds declared lifetime bin count 2")),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_rejects_detector_channel_outside_declared_split() {
    let path = tmp("bad_detector_channel.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 1);
        append_ptu_int_tag(out, "ImgHdr_PixY", 1);
        append_ptu_int_tag(out, "ImgHdr_DetectorChannels", 1);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 3);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0304);
        append_ptu_int_tag(out, "ImgHdr_LineStart", 1);
        append_ptu_int_tag(out, "ImgHdr_LineStop", 2);
    });
    append_ptu_t3_marker(&mut data, 1, 0);
    append_ptu_t3_photon(&mut data, 1, 1);
    append_ptu_t3_marker(&mut data, 2, 2);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value)) if value.contains("photon detector channel 1 exceeds declared detector channel count 1")
    ));
    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("photon detector channel 1 exceeds declared detector channel count 1")),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_reports_missing_timing_for_unmapped_tttr_stream() {
    let path = tmp("minimal_metadata_only.ptu");
    let data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 7);
        append_ptu_int_tag(out, "ImgHdr_PixY", 5);
        append_ptu_int_tag(out, "ImgHdr_Frame", 3);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 0);
        append_ptu_int_tag(out, "TTResultFormat_TTTRRecType", 0x0001_0304);
    });
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 7);
    assert_eq!(meta.size_y, 5);
    assert_eq!(meta.image_count, 3);
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value)) if value.contains("missing line-start marker")
    ));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("missing line-start marker"))
    );
    let err = reader.open_bytes_region(1, 0, 0, 1, 1).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("missing line-start marker"))
    );
    assert!(matches!(
        reader.open_bytes_region(3, 0, 0, 1, 1),
        Err(BioFormatsError::PlaneOutOfRange(3))
    ));
    assert!(matches!(
        reader.open_bytes(3),
        Err(BioFormatsError::PlaneOutOfRange(3))
    ));
    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_histogram_acquisition_opens_metadata_only() {
    let path = tmp("histogram_metadata_only.ptu");
    let data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "HistResDscr_HistogramBins", 4);
        append_ptu_int_tag(out, "HistResDscr_CurveIndex", 0);
        append_ptu_ansi_tag(out, "CreatorSW_Name", "SymPhoTime");
    });
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 4);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.size_t, 1);
    assert_eq!(meta.image_count, 1);
    assert_eq!(meta.pixel_type, PixelType::Uint32);
    assert!(matches!(
        meta.series_metadata.get("ptu.acquisition_mode"),
        Some(MetadataValue::String(value)) if value == "histogram"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_bins"),
        Some(MetadataValue::Int(4))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_curves"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_expected_bytes"),
        Some(MetadataValue::Int(16))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_actual_bytes"),
        Some(MetadataValue::Int(0))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_ambiguous"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.CreatorSW_Name"),
        Some(MetadataValue::String(value)) if value == "SymPhoTime"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value)) if value.contains("histogram acquisition")
    ));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("histogram acquisition image-plane decoding is unsupported")),
        "{err:?}"
    );
    assert!(matches!(
        reader.open_bytes_region(0, 4, 0, 1, 1),
        Err(BioFormatsError::Format(_))
    ));

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_decodes_exact_uint16_histogram_payload() {
    let path = tmp("histogram_uint16_payload.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "HistResDscr_HistogramBins", 4);
        append_ptu_int_tag(out, "HistResDscr_CurveIndex", 0);
    });
    for value in [1u16, 2, 3, 4] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.pixel_type, PixelType::Uint16);
    assert_eq!(meta.bits_per_pixel, 16);
    assert_eq!(meta.size_x, 4);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 1);
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_expected_bytes"),
        Some(MetadataValue::Int(8))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_actual_bytes"),
        Some(MetadataValue::Int(8))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_ambiguous"),
        Some(MetadataValue::Bool(false))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_layout"),
        Some(MetadataValue::String(value)) if value == "little-endian uint16 bins"
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_sample_bytes"),
        Some(MetadataValue::Int(2))
    ));

    let counts: Vec<u16> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(2)
        .map(|px| u16::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(counts, vec![1, 2, 3, 4]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_decodes_exact_uint8_histogram_payload() {
    let path = tmp("histogram_uint8_payload.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "HistResDscr_HistogramBins", 4);
        append_ptu_int_tag(out, "HistResDscr_CurveIndex", 0);
    });
    data.extend_from_slice(&[1, 2, 3, 4]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.pixel_type, PixelType::Uint8);
    assert_eq!(meta.bits_per_pixel, 8);
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_expected_bytes"),
        Some(MetadataValue::Int(4))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_layout"),
        Some(MetadataValue::String(value)) if value == "uint8 bins"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_reports_ambiguous_histogram_payload_size() {
    let path = tmp("histogram_ambiguous_payload.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "HistResDscr_HistogramBins", 4);
        append_ptu_int_tag(out, "HistResDscr_CurveIndex", 0);
    });
    data.extend_from_slice(&[1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_expected_bytes"),
        Some(MetadataValue::Int(16))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("ptu.histogram_payload_actual_bytes"),
        Some(MetadataValue::Int(12))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_ambiguous"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction_unsupported"),
        Some(MetadataValue::String(value)) if value.contains("12 payload bytes found")
    ));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("12 payload bytes found")),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_decodes_bounded_histogram_payload() {
    let path = tmp("histogram_payload.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "HistResDscr_HistogramBins", 4);
        append_ptu_int_tag(out, "HistResDscr_CurveIndex", 0);
    });
    for value in [3u32, 0, 7, 11] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 4);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.image_count, 1);
    assert!(matches!(
        meta.series_metadata.get("ptu.reconstruction"),
        Some(MetadataValue::String(value)) if value.contains("histogram payload decoded")
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_curves"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_ambiguous"),
        Some(MetadataValue::Bool(false))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_payload_layout"),
        Some(MetadataValue::String(value)) if value == "little-endian uint32 bins"
    ));

    let counts: Vec<u32> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(counts, vec![3, 0, 7, 11]);
    let crop = reader.open_bytes_region(0, 2, 0, 2, 1).unwrap();
    let crop_counts: Vec<u32> = crop
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(crop_counts, vec![7, 11]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_decodes_indexed_histogram_curves() {
    let path = tmp("histogram_indexed_curves.ptu");
    let mut data = minimal_ptu_header(|out| {
        append_ptu_indexed_int_tag(out, "HistResDscr_HistogramBins", 0, 3);
        append_ptu_indexed_int_tag(out, "HistResDscr_CurveIndex", 0, 0);
        append_ptu_indexed_int_tag(out, "HistResDscr_HistogramBins", 1, 3);
        append_ptu_indexed_int_tag(out, "HistResDscr_CurveIndex", 1, 1);
    });
    for value in [1u32, 2, 3, 4, 5, 6] {
        data.extend_from_slice(&value.to_le_bytes());
    }
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 3);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 2);
    assert_eq!(meta.image_count, 2);
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_bins"),
        Some(MetadataValue::Int(3))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.histogram_curves"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.HistResDscr_HistogramBins[1]"),
        Some(MetadataValue::Int(3))
    ));

    let curve_0: Vec<u32> = reader
        .open_bytes(0)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    let curve_1: Vec<u32> = reader
        .open_bytes(1)
        .unwrap()
        .chunks_exact(4)
        .map(|px| u32::from_le_bytes(px.try_into().unwrap()))
        .collect();
    assert_eq!(curve_0, vec![1, 2, 3]);
    assert_eq!(curve_1, vec![4, 5, 6]);

    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_rejects_missing_explicit_dimensions() {
    let path = tmp("no_dims.ptu");
    std::fs::write(&path, minimal_ptu_header(|_| {})).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("missing explicit image width"))
    );
    assert_eq!(reader.metadata().size_x, 0);
    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_rejects_histogram_without_bins_or_dimensions() {
    let path = tmp("histogram_no_bins.ptu");
    std::fs::write(
        &path,
        minimal_ptu_header(|out| {
            append_ptu_int_tag(out, "HistResDscr_CurveIndex", 0);
        }),
    )
    .unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("histogram acquisition missing bounded histogram bin descriptor")),
        "{err:?}"
    );
    assert_eq!(reader.metadata().size_x, 0);
    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_rejects_truncated_tag_table_before_metadata() {
    let path = tmp("truncated.ptu");
    let mut data = Vec::new();
    data.extend_from_slice(b"PQTTTR\0\0");
    data.extend_from_slice(b"1.0\0\0\0\0\0");
    data.extend_from_slice(&[0u8; 12]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("tag table is truncated"))
    );
    assert_eq!(reader.metadata().size_x, 0);
    let _ = std::fs::remove_file(path);
}

#[test]
fn picoquant_ptu_validates_regions_before_event_stream_boundary() {
    let path = tmp("picoquant_bad_region.ptu");
    let data = minimal_ptu_header(|data| {
        append_ptu_int_tag(data, "ImgHdr_PixX", 2);
        append_ptu_int_tag(data, "ImgHdr_PixY", 2);
    });
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let err = reader.open_bytes_region(0, 2, 0, 1, 1).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("outside image bounds")),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
}

/// Build a minimal little-endian, single-strip, 16-bit grayscale TIFF with a
/// list of extra (tag, type, value) entries appended to the IFD. Used to forge
/// Molecular Dynamics GEL files for the GelReader tests. `value` for SHORT/LONG
/// is the inline value; for RATIONAL it is an out-of-line numerator/denominator
/// pair written after the IFD.
fn build_gel_tiff(w: u16, h: u16, pixels_le: &[u8], extra: &[(u16, u16, u32)]) -> Vec<u8> {
    // type codes: 3 = SHORT, 4 = LONG, 5 = RATIONAL
    let mut entries: Vec<(u16, u16, u32, u32)> = vec![
        (256, 3, 1, w as u32), // ImageWidth
        (257, 3, 1, h as u32), // ImageLength
        (258, 3, 1, 16),       // BitsPerSample
        (259, 3, 1, 1),        // Compression = none
        (262, 3, 1, 1),        // PhotometricInterpretation = BlackIsZero
        (277, 3, 1, 1),        // SamplesPerPixel
        (278, 3, 1, h as u32), // RowsPerStrip
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
    let mut rational_offsets: std::collections::HashMap<u16, u32> =
        std::collections::HashMap::new();
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
fn qptiff_preserves_description_key_value_metadata_and_pixels() {
    let path = tmp("metadata_slice.qptiff");
    write_tiny_flex_tiff(
        &path,
        "ImageType=FullResolution\nChannelName=DAPI\nExposureTime=12.5",
        7,
    );

    let mut reader = bioformats::formats::extended::QptiffReader::new();
    reader.set_id(&path).unwrap();

    assert_eq!(reader.open_bytes(0).unwrap(), vec![7]);
    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("qptiff.ifd_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("qptiff.series_ifds"),
        Some(MetadataValue::String(value)) if value == "0"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.description.0.ImageType"),
        Some(MetadataValue::String(value)) if value == "FullResolution"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.description.1.ChannelName"),
        Some(MetadataValue::String(value)) if value == "DAPI"
    ));
    assert!(metadata.contains_key("qptiff.ifd.0.tag.65200.Private"));

    let ome = reader.ome_metadata().expect("QPTIFF OME metadata");
    let original_metadata = ome
        .annotations
        .iter()
        .find_map(|annotation| match annotation {
            OmeAnnotation::MapAnnotation {
                namespace: Some(ns),
                values,
                ..
            } if ns == "openmicroscopy.org/bioformats/original-metadata" => Some(values),
            _ => None,
        })
        .expect("QPTIFF original metadata annotation");
    assert!(original_metadata
        .iter()
        .any(|(key, value)| key == "qptiff.ifd.0.description.1.ChannelName" && value == "DAPI"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn qptiff_flattens_bounded_vendor_json_object_metadata() {
    let path = tmp("metadata_json_object.qptiff");
    write_tiny_flex_tiff(
        &path,
        r#"{"ImageType":"FullResolution","ScanProfile":{"Name":"PKI","ExposureTime":12.5},"Channels":[{"Name":"DAPI","Enabled":true,"ExcitationWavelength":405,"EmissionWavelength":460}],"ObjectiveName":"20x Plan Apo"}"#,
        11,
    );

    let mut reader = bioformats::formats::extended::QptiffReader::new();
    reader.set_id(&path).unwrap();

    assert_eq!(reader.open_bytes(0).unwrap(), vec![11]);
    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.format"),
        Some(MetadataValue::String(value)) if value == "json"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.ImageType"),
        Some(MetadataValue::String(value)) if value == "FullResolution"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.ScanProfile.Name"),
        Some(MetadataValue::String(value)) if value == "PKI"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.ScanProfile.ExposureTime"),
        Some(MetadataValue::Float(value)) if (*value - 12.5).abs() < f64::EPSILON
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.Channels.0.Name"),
        Some(MetadataValue::String(value)) if value == "DAPI"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.Channels.0.Enabled"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.semantic.acquisition.exposure_time"),
        Some(MetadataValue::Float(value)) if (*value - 12.5).abs() < f64::EPSILON
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.semantic.channel.0.name"),
        Some(MetadataValue::String(value)) if value == "DAPI"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.semantic.channel.0.excitation_wavelength"),
        Some(MetadataValue::Int(405))
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.semantic.channel.0.emission_wavelength"),
        Some(MetadataValue::Int(460))
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.semantic.instrument.objective"),
        Some(MetadataValue::String(value)) if value == "20x Plan Apo"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.graph.node_count"),
        Some(MetadataValue::Int(4))
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.graph.0.path"),
        Some(MetadataValue::String(value)) if value == "$"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.graph.0.type"),
        Some(MetadataValue::String(value)) if value == "object"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.graph.0.child_count"),
        Some(MetadataValue::Int(4))
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.graph.0.container_child_count"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.graph.1.path"),
        Some(MetadataValue::String(value)) if value == "ScanProfile"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.graph.1.scalar_child_count"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.graph.2.path"),
        Some(MetadataValue::String(value)) if value == "Channels"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.graph.2.type"),
        Some(MetadataValue::String(value)) if value == "array"
    ));
    assert!(matches!(
        metadata.get("qptiff.ifd.0.vendor_object.graph.3.path"),
        Some(MetadataValue::String(value)) if value == "Channels.0"
    ));

    let ome = reader.ome_metadata().expect("QPTIFF OME metadata");
    let original_metadata = ome
        .annotations
        .iter()
        .find_map(|annotation| match annotation {
            OmeAnnotation::MapAnnotation {
                namespace: Some(ns),
                values,
                ..
            } if ns == "openmicroscopy.org/bioformats/original-metadata" => Some(values),
            _ => None,
        })
        .expect("QPTIFF original metadata annotation");
    assert!(original_metadata.iter().any(|(key, value)| {
        key == "qptiff.ifd.0.vendor_object.Channels.0.Name" && value == "DAPI"
    }));

    let _ = std::fs::remove_file(path);
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
    let visitech = dir.join("scan Report.html");
    std::fs::write(
        &visitech,
        b"Image dimensions: (2, 2)\nNumber of steps: 1\nMicroscope XY: 0\nImage bit depth: 16\nChannel Selection: 1\nTime Series; 1\n",
    )
    .unwrap();
    let mut reader = bioformats::formats::visitech::VisitechReader::new();
    let err = reader.set_id(&visitech).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Visitech XYS does not have")),
        "{err:?}"
    );
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn olympus_prairie_leica_readers_require_initialization_for_series() {
    let mut oif = bioformats::formats::olympus::OifReader::new();
    assert_eq!(oif.series_count(), 0);
    assert!(matches!(
        oif.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    let mut prairie = bioformats::formats::prairie::PrairieReader::new();
    assert_eq!(prairie.series_count(), 0);
    assert!(matches!(
        prairie.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    let mut tcs = bioformats::formats::prairie::LeicaTcsReader::new();
    assert_eq!(tcs.series_count(), 0);
    assert!(matches!(
        tcs.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));

    let mut lei = bioformats::formats::lei::LeiReader::new();
    assert_eq!(lei.series_count(), 0);
    assert!(matches!(
        lei.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));

    let mut lif = bioformats::formats::lif::LifReader::new();
    assert_eq!(lif.series_count(), 0);
    assert!(matches!(
        lif.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));
}

#[test]
fn olympus_oif_rejects_missing_planes_and_bad_pixel_depth() {
    let empty = tmp("empty_planes.oif");
    std::fs::write(&empty, "[FileInformation]\n").unwrap();
    let mut reader = bioformats::formats::olympus::OifReader::new();
    let err = reader.set_id(&empty).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("does not reference any PTY")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);
    let _ = std::fs::remove_file(&empty);

    let root = tmp("bad_depth.oif");
    let companion = root.with_file_name(format!(
        "{}.files",
        root.file_stem().unwrap().to_string_lossy()
    ));
    std::fs::create_dir_all(&companion).unwrap();
    let pty = companion.join("plane0.pty");
    std::fs::write(&pty, "[File Info]\nDataName=plane0.tif\n").unwrap();
    std::fs::write(
        &root,
        "[ProfileSaveInfo]\nIniFileName0=plane0.pty\n[Axis 0 Parameters Common]\nAxisCode=X\nMaxSize=1\n[Axis 1 Parameters Common]\nAxisCode=Y\nMaxSize=1\n[Reference Image Parameter]\nImageDepth=3\n",
    )
    .unwrap();
    let err = reader.set_id(&root).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("unsupported ImageDepth 3")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_file(root);
    let _ = std::fs::remove_file(pty);
    let _ = std::fs::remove_dir(companion);
}

#[test]
fn prairie_and_leica_tcs_reject_fake_metadata_without_readable_tiff_dimensions() {
    let prairie = tmp("pvscan_missing_tiff_dims.xml");
    std::fs::write(
        &prairie,
        r#"<PVScan><Sequence><Frame index="0"><File filename="missing.tif" channel="1"/></Frame></Sequence></PVScan>"#,
    )
    .unwrap();
    let mut reader = bioformats::formats::prairie::PrairieReader::new();
    let err = reader.set_id(&prairie).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("companion TIFF")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);
    assert_eq!(reader.metadata().size_x, 0);
    let _ = std::fs::remove_file(&prairie);

    let leica = tmp("leica_missing_tiff_dims.xml");
    std::fs::write(&leica, r#"<LEICA><Attachment Name="missing.tif"/></LEICA>"#).unwrap();
    let mut reader = bioformats::formats::prairie::LeicaTcsReader::new();
    let err = reader.set_id(&leica).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("companion TIFF")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);
    assert_eq!(reader.metadata().size_x, 0);
    let _ = std::fs::remove_file(&leica);
}

#[test]
fn incell_rejects_missing_dimensions_for_im_and_clears_failed_reopen() {
    let dir = isolated_tmp_dir("incell_im_validation");
    let im = dir.join("plane.im");
    let xdce = dir.join("plate.xdce");
    std::fs::write(&im, [0u8; 130]).unwrap();
    std::fs::write(
        &xdce,
        r#"<InCell><Image filename="plane.im"><Identifier field_index="0" z_index="0" wave_index="0" time_index="0"/></Image></InCell>"#,
    )
    .unwrap();

    let mut reader = bioformats::formats::incell::InCellReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    let err = reader.set_id(&xdce).unwrap_err();
    assert!(
        err.to_string().contains("positive image dimensions"),
        "unexpected InCell .im dimension error: {err}"
    );

    let tiff = dir.join("plane.tif");
    write_tiny_tiff_bytes(&tiff);
    std::fs::write(
        &xdce,
        r#"<InCell><Image filename="plane.tif"><Identifier field_index="0" z_index="0" wave_index="0" time_index="0"/></Image></InCell>"#,
    )
    .unwrap();
    reader.set_id(&xdce).unwrap();
    assert_eq!(reader.series_count(), 1);

    std::fs::write(
        &xdce,
        r#"<InCell><Image filename="missing.tif"><Identifier field_index="0" z_index="0" wave_index="0" time_index="0"/></Image></InCell>"#,
    )
    .unwrap();
    let err = reader.set_id(&xdce).unwrap_err();
    assert!(
        err.to_string().contains("existing companion"),
        "unexpected InCell missing companion error: {err}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn incell_rejects_bad_indices_and_unreadable_tiff_before_metadata() {
    let dir = isolated_tmp_dir("incell_bad_indices");
    let xdce = dir.join("plate.xdce");
    let tiff = dir.join("bad.tif");
    std::fs::write(&tiff, b"not a tiff").unwrap();

    std::fs::write(
        &xdce,
        r#"<InCell><Plate rows="1" columns="1"/><Image filename="bad.tif"><Identifier field_index="0" z_index="-1" wave_index="0" time_index="0"/></Image></InCell>"#,
    )
    .unwrap();
    let mut reader = bioformats::formats::incell::InCellReader::new();
    let err = reader.set_id(&xdce).unwrap_err();
    assert!(
        err.to_string().contains("z_index must be non-negative"),
        "unexpected InCell negative-index error: {err}"
    );
    assert_eq!(reader.series_count(), 0);

    std::fs::write(
        &xdce,
        r#"<InCell><Plate rows="1" columns="1"/><Image filename="bad.tif"><Identifier field_index="0" z_index="0" wave_index="0" time_index="0"/></Image></InCell>"#,
    )
    .unwrap();
    let err = reader.set_id(&xdce).unwrap_err();
    assert!(
        err.to_string().contains("companion TIFF"),
        "unexpected InCell bad-TIFF error: {err}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn hcs_index_readers_reject_fake_payloads_before_metadata() {
    let mut wrapper = bioformats::formats::hcs2::MetaxpressTiffReader::new();
    assert_eq!(wrapper.series_count(), 0);
    assert!(matches!(
        wrapper.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    let dir = isolated_tmp_dir("columbus_missing_payload");
    let index = dir.join("MeasurementIndex.ColumbusIDX.xml");
    let image_index = dir.join("Images.ColumbusIDX.xml");
    std::fs::write(
        &index,
        r#"<ColumbusMeasurementIndex><PlateRows>1</PlateRows><PlateColumns>1</PlateColumns><Reference>Images.ColumbusIDX.xml</Reference></ColumbusMeasurementIndex>"#,
    )
    .unwrap();
    std::fs::write(
        &image_index,
        r#"<Images><Image><URL BufferNo="0">missing.tif</URL><Row>1</Row><Col>1</Col><FieldID>1</FieldID><PlaneID>1</PlaneID><TimepointID>1</TimepointID><ChannelID>1</ChannelID></Image></Images>"#,
    )
    .unwrap();
    let mut reader = bioformats::formats::hcs2::ColumbusReader::new();
    let err = reader.set_id(&index).unwrap_err();
    assert!(
        err.to_string().contains("companion TIFF"),
        "unexpected Columbus missing-payload error: {err}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn yokogawa_requires_initialization_and_clears_failed_reopen() {
    let dir = isolated_tmp_dir("yokogawa_validation");
    let wpi = dir.join("plate.wpi");
    let mlf = dir.join("MeasurementData.mlf");
    let tiff = dir.join("plane.tif");
    write_tiny_tiff_bytes(&tiff);
    std::fs::write(
        &wpi,
        r#"<bts:WellPlate bts:Name="Plate" bts:Rows="1" bts:Columns="1"/>"#,
    )
    .unwrap();
    std::fs::write(
        &mlf,
        r#"<root><bts:MeasurementRecord bts:Type="IMG" bts:Row="1" bts:Column="1" bts:FieldIndex="1" bts:ZIndex="1" bts:Ch="1" bts:TimePoint="1" bts:ActionIndex="1" bts:TimelineIndex="1">plane.tif</bts:MeasurementRecord></root>"#,
    )
    .unwrap();

    let mut reader = bioformats::formats::extended::YokogawaReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    reader.set_id(&wpi).unwrap();
    assert_eq!(reader.series_count(), 1);

    std::fs::write(
        &mlf,
        r#"<root><bts:MeasurementRecord bts:Type="IMG" bts:Row="1" bts:Column="1" bts:FieldIndex="1" bts:ZIndex="1" bts:Ch="1" bts:TimePoint="1" bts:ActionIndex="1" bts:TimelineIndex="1">missing.tif</bts:MeasurementRecord></root>"#,
    )
    .unwrap();
    let err = reader.set_id(&wpi).unwrap_err();
    assert!(
        err.to_string().contains("missing image file"),
        "unexpected Yokogawa missing-payload error: {err}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn visitech_rejects_invented_metadata_and_short_payload() {
    let dir = isolated_tmp_dir("visitech_validation");
    let report = dir.join("scan Report.html");
    let pixels = dir.join("scan 1.xys");

    std::fs::write(
        &report,
        "Image bit depth: 8\nNumber of steps: 1\nMicroscope XY: 0\nChannel Selection 1: Ch\nTime Series; 1\n",
    )
    .unwrap();
    std::fs::write(&pixels, b"[USE SAME FILE]\x01\x02").unwrap();
    let mut reader = bioformats::formats::visitech::VisitechReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    let err = reader.set_id(&report).unwrap_err();
    assert!(
        err.to_string().contains("image dimensions"),
        "unexpected Visitech missing-dimension error: {err}"
    );

    std::fs::write(
        &report,
        "Image dimensions: (2, 1)\nImage bit depth: 8\nNumber of steps: 2\nMicroscope XY: 0\nChannel Selection 1: Ch\nTime Series; 1\n",
    )
    .unwrap();
    let err = reader.set_id(&report).unwrap_err();
    assert!(
        err.to_string().contains("does not have any companion")
            || err.to_string().contains("shorter than declared"),
        "unexpected Visitech short-payload error: {err}"
    );
    assert_eq!(reader.series_count(), 0);

    std::fs::write(
        &report,
        "Image dimensions: (2, 1)\nImage bit depth: 8\nNumber of steps: 1\nMicroscope XY: 0\nChannel Selection 1: Ch\nTime Series; 1\n",
    )
    .unwrap();
    reader.set_id(&report).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);
    assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 1).unwrap(), vec![2]);

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
fn cellomics_dib_records_safe_header_and_filename_metadata() {
    let dir = isolated_tmp_dir("cellomics_metadata");
    let path = dir.join("AS_09125_050118150001_A03f00d1.DIB");
    let mut data = vec![0u8; 52];
    data[0..4].copy_from_slice(&40u32.to_le_bytes());
    data[4..8].copy_from_slice(&2i32.to_le_bytes());
    data[8..12].copy_from_slice(&(-2i32).to_le_bytes());
    data[12..14].copy_from_slice(&1u16.to_le_bytes());
    data[14..16].copy_from_slice(&8u16.to_le_bytes());
    data[16..20].copy_from_slice(&0u32.to_le_bytes());
    data.extend_from_slice(&[1, 2, 3, 4]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::extended::CellomicsReader::new();
    reader.set_id(&path).unwrap();
    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("cellomics.file_name"),
        Some(MetadataValue::String(value)) if value == "AS_09125_050118150001_A03f00d1.DIB"
    ));
    assert!(matches!(
        metadata.get("cellomics.plate"),
        Some(MetadataValue::String(value)) if value == "AS_09125_050118150001"
    ));
    assert!(matches!(
        metadata.get("cellomics.well"),
        Some(MetadataValue::String(value)) if value == "A03"
    ));
    assert!(matches!(
        metadata.get("cellomics.field_index"),
        Some(MetadataValue::Int(0))
    ));
    assert!(matches!(
        metadata.get("cellomics.filename_channel_index"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("cellomics.dib.header_size"),
        Some(MetadataValue::Int(40))
    ));
    assert!(matches!(
        metadata.get("cellomics.dib.top_down"),
        Some(MetadataValue::String(value)) if value == "true"
    ));

    let ome = reader.ome_metadata().unwrap();
    assert_eq!(
        ome.images[0].name.as_deref(),
        Some("AS_09125_050118150001 A03")
    );
    assert!(ome.annotations.iter().any(|annotation| {
        matches!(
            annotation,
            OmeAnnotation::MapAnnotation { values, .. }
                if values.iter().any(|(key, value)| key == "cellomics.well" && value == "A03")
        )
    }));
    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cellomics_dib_assembles_matching_sibling_channel_files() {
    let dir = isolated_tmp_dir("cellomics_assembly");
    let path_d0 = dir.join("AS_09125_050118150001_A03f00d0.DIB");
    let path_d1 = dir.join("AS_09125_050118150001_A03f00d1.DIB");
    let path_other_field = dir.join("AS_09125_050118150001_A03f01d2.DIB");

    for (path, pixels) in [
        (&path_d0, [1u8, 2, 3, 4]),
        (&path_d1, [5u8, 6, 7, 8]),
        (&path_other_field, [9u8, 10, 11, 12]),
    ] {
        let mut data = vec![0u8; 52];
        data[0..4].copy_from_slice(&40u32.to_le_bytes());
        data[4..8].copy_from_slice(&2i32.to_le_bytes());
        data[8..12].copy_from_slice(&2i32.to_le_bytes());
        data[12..14].copy_from_slice(&1u16.to_le_bytes());
        data[14..16].copy_from_slice(&8u16.to_le_bytes());
        data[16..20].copy_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&pixels);
        std::fs::write(path, data).unwrap();
    }

    let mut reader = bioformats::formats::extended::CellomicsReader::new();
    reader.set_id(&path_d1).unwrap();
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);

    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("cellomics.assembly"),
        Some(MetadataValue::String(value)) if value == "sibling_filename_channels"
    ));
    assert!(matches!(
        metadata.get("cellomics.assembled_channel_indices"),
        Some(MetadataValue::String(value)) if value == "0,1"
    ));
    assert!(matches!(
        metadata.get("cellomics.assembled_files"),
        Some(MetadataValue::String(value))
            if value == "AS_09125_050118150001_A03f00d0.DIB,AS_09125_050118150001_A03f00d1.DIB"
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cellomics_dib_assembles_java_o_channel_sibling_files() {
    let dir = isolated_tmp_dir("cellomics_o_assembly");
    let path_o1 = dir.join("WHICA-VTI1_090915160001_A01f00o1.DIB");
    let path_o2 = dir.join("WHICA-VTI1_090915160001_A01f00o2.DIB");
    let path_other_field = dir.join("WHICA-VTI1_090915160001_A01f01o3.DIB");

    for (path, pixels) in [
        (&path_o1, [11u8, 12, 13, 14]),
        (&path_o2, [21u8, 22, 23, 24]),
        (&path_other_field, [31u8, 32, 33, 34]),
    ] {
        let mut data = vec![0u8; 52];
        data[0..4].copy_from_slice(&40u32.to_le_bytes());
        data[4..8].copy_from_slice(&2i32.to_le_bytes());
        data[8..12].copy_from_slice(&2i32.to_le_bytes());
        data[12..14].copy_from_slice(&1u16.to_le_bytes());
        data[14..16].copy_from_slice(&8u16.to_le_bytes());
        data[16..20].copy_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&pixels);
        std::fs::write(path, data).unwrap();
    }

    let mut reader = bioformats::formats::extended::CellomicsReader::new();
    reader.set_id(&path_o2).unwrap();
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![11, 12, 13, 14]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![21, 22, 23, 24]);

    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("cellomics.assembly"),
        Some(MetadataValue::String(value)) if value == "sibling_filename_channels"
    ));
    assert!(matches!(
        metadata.get("cellomics.assembled_channel_indices"),
        Some(MetadataValue::String(value)) if value == "1,2"
    ));
    assert!(matches!(
        metadata.get("cellomics.assembled_files"),
        Some(MetadataValue::String(value))
            if value == "WHICA-VTI1_090915160001_A01f00o1.DIB,WHICA-VTI1_090915160001_A01f00o2.DIB"
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cellomics_dib_groups_same_plate_well_fields_as_series() {
    let dir = isolated_tmp_dir("cellomics_plate_series");
    let path_f00_d0 = dir.join("AS_09125_050118150001_A03f00d0.DIB");
    let path_f00_d1 = dir.join("AS_09125_050118150001_A03f00d1.DIB");
    let path_f01_d0 = dir.join("AS_09125_050118150001_A03f01d0.DIB");
    let path_f01_d1 = dir.join("AS_09125_050118150001_A03f01d1.DIB");

    for (path, pixels) in [
        (&path_f00_d0, [1u8, 2, 3, 4]),
        (&path_f00_d1, [5u8, 6, 7, 8]),
        (&path_f01_d0, [11u8, 12, 13, 14]),
        (&path_f01_d1, [15u8, 16, 17, 18]),
    ] {
        let mut data = vec![0u8; 52];
        data[0..4].copy_from_slice(&40u32.to_le_bytes());
        data[4..8].copy_from_slice(&2i32.to_le_bytes());
        data[8..12].copy_from_slice(&2i32.to_le_bytes());
        data[12..14].copy_from_slice(&1u16.to_le_bytes());
        data[14..16].copy_from_slice(&8u16.to_le_bytes());
        data[16..20].copy_from_slice(&0u32.to_le_bytes());
        data.extend_from_slice(&pixels);
        std::fs::write(path, data).unwrap();
    }

    let mut reader = bioformats::formats::extended::CellomicsReader::new();
    reader.set_id(&path_f01_d1).unwrap();
    assert_eq!(reader.series_count(), 2);
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("cellomics.plate_assembly"),
        Some(MetadataValue::String(value)) if value == "plate_well_field_filename_series"
    ));
    assert!(matches!(
        metadata.get("cellomics.assembled_series_count"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        metadata.get("cellomics.field_index"),
        Some(MetadataValue::Int(0))
    ));
    assert!(matches!(
        metadata.get("cellomics.assembled_channel_indices"),
        Some(MetadataValue::String(value)) if value == "0,1"
    ));

    reader.set_series(1).unwrap();
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![11, 12, 13, 14]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![15, 16, 17, 18]);
    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("cellomics.series_index"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("cellomics.field_index"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("cellomics.assembled_files"),
        Some(MetadataValue::String(value))
            if value == "AS_09125_050118150001_A03f01d0.DIB,AS_09125_050118150001_A03f01d1.DIB"
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn perkinelmer_tolerates_truncated_trailing_tiff_metadata() {
    // PerkinElmer .htm layout: an .htm header plus a matching TIFF pixel file.
    // The TiffWriter lays out pixel strips first and IFD metadata last, so
    // chopping the tail damages only an out-of-line metadata value, leaving the
    // pixel strips intact. Java Bio-Formats parses such trailing-truncated TIFFs
    // leniently (truncating the over-long value rather than erroring); after the
    // Tier 2 robustness fix the Rust reader matches that — and the pixels must
    // still read back exactly. (Genuine *pixel* shortfall is still rejected; see
    // `openlab_rejects_short_payloads_instead_of_padding`.)
    let dir = isolated_tmp_dir("perkin_trunc_meta");
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
    // Chop the trailing metadata bytes; the leading pixel strips are untouched.
    let full = std::fs::read(&tif).unwrap();
    std::fs::write(&tif, &full[..full.len() - 3]).unwrap();
    let mut pe = bioformats::formats::perkinelmer::PerkinElmerReader::new();
    pe.set_id(&htm)
        .expect("trailing-metadata truncation should be tolerated, not rejected");
    assert_eq!(pe.open_bytes(0).unwrap(), vec![1u8, 2, 3, 4, 5, 6]);
    let _ = std::fs::remove_dir_all(dir);
}

fn append_openlab_liff_v2_tag_header(
    out: &mut Vec<u8>,
    tag: i16,
    sub_tag: i16,
    next_offset: i32,
    format: &str,
) {
    out.extend_from_slice(&tag.to_be_bytes());
    out.extend_from_slice(&sub_tag.to_be_bytes());
    out.extend_from_slice(&next_offset.to_be_bytes());
    let mut fmt = [0u8; 4];
    let bytes = format.as_bytes();
    fmt[..bytes.len().min(4)].copy_from_slice(&bytes[..bytes.len().min(4)]);
    out.extend_from_slice(&fmt);
    out.extend_from_slice(&0u32.to_be_bytes());
}

fn minimal_openlab_liff_with_non_image_tag() -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(&[0, 0, 0xff, 0xff]);
    data.extend_from_slice(b"impr");
    data.extend_from_slice(&2i32.to_be_bytes());
    data.extend_from_slice(&1i16.to_be_bytes());
    data.extend_from_slice(&0i16.to_be_bytes());
    data.extend_from_slice(&20i32.to_be_bytes());

    append_openlab_liff_v2_tag_header(&mut data, 70, 9, 36, "META");
    append_openlab_liff_v2_tag_header(&mut data, 67, 3, 0, "RAW ");
    data.extend_from_slice(&[0u8; 24]);
    data.extend_from_slice(&1i16.to_be_bytes());
    data.extend_from_slice(&[0u8; 16]);
    let name_start = data.len();
    data.extend_from_slice(b"Well A1 Z1 C1 T1\0");
    data.resize(name_start + 256, 0);
    data.extend_from_slice(&0i16.to_be_bytes());
    data.extend_from_slice(&0i16.to_be_bytes());
    data.extend_from_slice(&0i16.to_be_bytes());
    data.extend_from_slice(&1i16.to_be_bytes());
    data.extend_from_slice(&1i16.to_be_bytes());
    data.push(7);
    data
}

#[test]
fn openlab_liff_preserves_non_image_tag_headers_as_original_metadata() {
    let path = tmp("tag_headers.liff");
    std::fs::write(&path, minimal_openlab_liff_with_non_image_tag()).unwrap();

    let mut reader = bioformats::formats::misc::OpenlabLiffReader::new();
    reader.set_id(&path).unwrap();
    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("openlab.tag_header.count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("openlab.tag_header.0.tag"),
        Some(MetadataValue::Int(70))
    ));
    assert!(matches!(
        metadata.get("openlab.tag_header.0.sub_tag"),
        Some(MetadataValue::Int(9))
    ));
    assert!(matches!(
        metadata.get("openlab.tag_header.0.format"),
        Some(MetadataValue::String(value)) if value == "META"
    ));
    assert!(matches!(
        metadata.get("openlab.tag_header.0.offset"),
        Some(MetadataValue::Int(20))
    ));
    assert!(matches!(
        metadata.get("openlab.tag_header.0.next_offset"),
        Some(MetadataValue::Int(36))
    ));

    let ome = reader.ome_metadata().unwrap();
    let original_metadata = ome
        .annotations
        .iter()
        .find_map(|annotation| match annotation {
            OmeAnnotation::MapAnnotation { id, values, .. }
                if id.as_deref() == Some("Annotation:OriginalMetadata:0") =>
            {
                Some(values)
            }
            _ => None,
        });
    assert!(
        original_metadata.is_some_and(|values| values
            .iter()
            .any(|(key, value)| { key == "openlab.tag_header.0.format" && value == "META" })),
        "Openlab LIFF tag header should be preserved in OME original metadata"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn openlab_rejects_short_payloads_instead_of_padding() {
    // Openlab .raw declares its dimensions in a fixed header; when the actual
    // payload is shorter than declared, the reader must reject it rather than
    // zero-pad to the declared size.
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

    let raw = tmp("bad_magic_openlab.raw");
    let mut data = vec![0u8; 288];
    data[8..12].copy_from_slice(&1i32.to_be_bytes());
    data[12..16].copy_from_slice(&1i32.to_be_bytes());
    data[16..20].copy_from_slice(&8i32.to_be_bytes());
    data.extend_from_slice(&[1]);
    std::fs::write(&raw, data).unwrap();
    let mut openlab = bioformats::formats::perkinelmer::OpenlabRawReader::new();
    let err = openlab.set_id(&raw).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("LBLB magic")),
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
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    let err = reader.set_id(&missing).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("LIM header is missing")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&missing);

    let negative_height = tmp("negative_height.lim");
    let mut header = vec![0u8; 32];
    header[0..2].copy_from_slice(&1u16.to_le_bytes());
    header[2..4].copy_from_slice(&(-1i16).to_le_bytes());
    header[4..6].copy_from_slice(&8u16.to_le_bytes());
    std::fs::write(&negative_height, header).unwrap();
    let mut reader = bioformats::formats::lim::LimReader::new();
    let err = reader.set_id(&negative_height).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("LIM header is missing")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&negative_height);

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
fn tillvision_pst_entrypoint_reads_sidecar_inf_pixels() {
    let pst = tmp("direct_tillvision.pst");
    let inf = tmp("direct_tillvision.inf");
    std::fs::write(
        &inf,
        "Width=3\nHeight=2\nBands=1\nSlices=1\nFrames=1\nDatatype=2\nImageName=Till scalar metadata\nExposureTime=12.5\nImageType=brightfield\nDate=05/27/26\nStart Time=10:11:12 AM\nPixelSizeX [um]=0.25\nPixelSizeY [um]=0.5\nZStep [um]=1.5\nFrameInterval [ms]=2500\nChannel Name 1=DAPI\nChannel Excitation Wavelength 1 [nm]=405\nChannel Emission Wavelength 1 [nm]=460\n",
    )
    .unwrap();
    std::fs::write(&pst, [1, 2, 3, 4, 5, 6]).unwrap();

    let mut direct = bioformats::formats::lim::TillVisionReader::new();
    assert_eq!(direct.series_count(), 0);
    assert!(matches!(
        direct.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    let mut reader = ImageReader::open(&pst).unwrap();
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("tillvision.exposure_time_seconds"),
        Some(MetadataValue::Float(value)) if (*value - 0.0125).abs() < 1.0e-12
    ));
    assert!(matches!(
        reader.metadata().series_metadata.get("tillvision.image_type"),
        Some(MetadataValue::String(value)) if value == "brightfield"
    ));
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("tillvision.acquisition_datetime"),
        Some(MetadataValue::String(value)) if value == "05/27/26 10:11:12 AM"
    ));
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("tillvision.acquisition_datetime_iso8601"),
        Some(MetadataValue::String(value)) if value == "2026-05-27T10:11:12"
    ));
    let ome = reader.ome_metadata().expect("TillVision OME metadata");
    let image = &ome.images[0];
    assert_eq!(image.name.as_deref(), Some("Till scalar metadata"));
    assert_eq!(image.physical_size_x, Some(0.25));
    assert_eq!(image.physical_size_y, Some(0.5));
    assert_eq!(image.physical_size_z, Some(1.5));
    assert_eq!(image.time_increment, Some(2.5));
    assert_eq!(image.channels[0].name.as_deref(), Some("DAPI"));
    assert_eq!(image.channels[0].excitation_wavelength, Some(405.0));
    assert_eq!(image.channels[0].emission_wavelength, Some(460.0));
    assert!(image
        .planes
        .iter()
        .all(|plane| plane.exposure_time == Some(0.0125)));
    assert!(ome.annotations.iter().any(|annotation| matches!(
        annotation,
        bioformats::OmeAnnotation::MapAnnotation { values, .. }
            if values.iter().any(|(key, value)| key == "AcquisitionDate" && value == "2026-05-27T10:11:12")
    )));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 3, 5, 6]
    );

    let _ = std::fs::remove_file(pst);
    let _ = std::fs::remove_file(inf);
}

#[test]
fn tillvision_rejects_zero_inf_dimensions_before_payload_math() {
    let pst = tmp("zero_tillvision.pst");
    let inf = tmp("zero_tillvision.inf");
    std::fs::write(
        &inf,
        "Width=0\nHeight=2\nBands=1\nSlices=1\nFrames=1\nDatatype=2\n",
    )
    .unwrap();
    std::fs::write(&pst, []).unwrap();

    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&pst).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("INF dimensions and counts must be positive")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_file(pst);
    let _ = std::fs::remove_file(inf);
}

#[test]
fn tillvision_vws_discovers_pst_sidecar_pixels() {
    let dir = isolated_tmp_dir("tillvision_vws");
    let vws = dir.join("experiment.vws");
    let pst = dir.join("experiment_001.pst");
    let inf = dir.join("experiment_001.inf");
    std::fs::write(&vws, b"TillVision workspace placeholder").unwrap();
    std::fs::write(
        &inf,
        "Width=2\nHeight=2\nBands=1\nSlices=1\nFrames=1\nDatatype=2\n",
    )
    .unwrap();
    std::fs::write(&pst, [9, 8, 7, 6]).unwrap();

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![9, 8, 7, 6]);
    assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(), vec![8, 6]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_embedded_strict_raw_subset() {
    let dir = isolated_tmp_dir("tillvision_vws_embedded");
    let vws = dir.join("embedded.vws");
    let magic = *b"BFTILLVISIONVWS1";
    let payload = vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12];
    std::fs::write(&vws, strict_misc_raw_bytes(&magic, 3, 2, 2, 1, &payload)).unwrap();

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_t, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![7, 8, 9, 10, 11, 12]);
    assert_eq!(
        reader.open_bytes_region(1, 1, 0, 2, 2).unwrap(),
        vec![8, 9, 11, 12]
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_native_ole_cimage_contents() {
    let dir = isolated_tmp_dir("tillvision_vws_native_cimage");
    let vws = dir.join("native.vws");
    write_tillvision_vws_with_contents(&vws, &tillvision_native_cimage_contents());

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
    assert!(matches!(
        reader.metadata().series_metadata.get("Info image_name"),
        Some(MetadataValue::String(name)) if name == "NativeImage"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
    assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(), vec![6, 8]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_zlib_compressed_native_cimage_payload() {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let dir = isolated_tmp_dir("tillvision_vws_native_cimage_zlib");
    let vws = dir.join("native_zlib.vws");
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    let compressed = encoder.finish().unwrap();
    let description = b"Compression: zlib\r\nImage type: compressed native\r\n";
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_payload_and_description(&compressed, description),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 2);
    assert!(matches!(
        reader.metadata().series_metadata.get("Info Compression"),
        Some(MetadataValue::String(value)) if value == "zlib"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
    assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(), vec![2, 4]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_raw_deflate_compressed_native_cimage_payload() {
    use flate2::write::DeflateEncoder;
    use flate2::Compression;
    use std::io::Write;

    let dir = isolated_tmp_dir("tillvision_vws_native_cimage_raw_deflate");
    let vws = dir.join("native_raw_deflate.vws");
    let mut encoder = DeflateEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    let compressed = encoder.finish().unwrap();
    let description = b"Compression: deflate\r\nImage type: raw deflate native\r\n";
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_payload_and_description(&compressed, description),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 2);
    assert!(matches!(
        reader.metadata().series_metadata.get("Info Compression"),
        Some(MetadataValue::String(value)) if value == "deflate"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
    assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(), vec![6, 8]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_native_cimage_with_shifted_object_marker() {
    let dir = isolated_tmp_dir("tillvision_vws_native_cimage_shifted_marker");
    let vws = dir.join("native_shifted_marker.vws");
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_shifted_object_marker(),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_compressed_flag_does_not_mask_declared_zlib_algorithm() {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let dir = isolated_tmp_dir("tillvision_vws_native_cimage_compressed_flag_zlib");
    let vws = dir.join("native_compressed_flag_zlib.vws");
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[1, 2, 3, 4, 5, 6, 7, 8]).unwrap();
    let compressed = encoder.finish().unwrap();
    let description = b"Compressed: 1\r\nCompression: zlib\r\n";
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_payload_and_description(&compressed, description),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_rejects_compressed_flag_without_algorithm() {
    let dir = isolated_tmp_dir("tillvision_vws_native_cimage_compressed_flag");
    let vws = dir.join("native_compressed_flag.vws");
    let description = b"Compressed: true\r\n";
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_payload_and_description(
            &[1, 2, 3, 4, 5, 6, 7, 8],
            description,
        ),
    );

    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&vws).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message)
            if message.contains("compressed payload without a supported algorithm")),
        "{err:?}"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_noncontiguous_native_cimage_payload_offset() {
    let dir = isolated_tmp_dir("tillvision_vws_native_cimage_offset");
    let vws = dir.join("native_offset.vws");
    let description = b"Payload offset: 160\r\nImage type: offset native\r\n";
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_payload_at_offset(
            &[1, 2, 3, 4, 5, 6, 7, 8],
            160,
            description,
        ),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 2);
    assert!(matches!(
        reader.metadata().series_metadata.get("Info Payload offset"),
        Some(MetadataValue::String(value)) if value == "160"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
    assert_eq!(reader.open_bytes_region(1, 0, 1, 2, 1).unwrap(), vec![7, 8]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_fragmented_native_cimage_payload_table() {
    let dir = isolated_tmp_dir("tillvision_vws_native_cimage_fragments");
    let vws = dir.join("native_fragments.vws");
    let description = b"Payload fragments: 160:3, 180:5\r\nImage type: fragmented native\r\n";
    write_tillvision_vws_with_contents(
        &vws,
        &tillvision_native_cimage_contents_with_payload_fragments(
            &[(160, &[1, 2, 3]), (180, &[4, 5, 6, 7, 8])],
            description,
        ),
    );

    let mut reader = ImageReader::open(&vws).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_c, 2);
    assert!(matches!(
        reader.metadata().series_metadata.get("Info Payload fragments"),
        Some(MetadataValue::String(value)) if value == "160:3, 180:5"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
    assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(), vec![2, 4]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_reads_native_description_metadata() {
    let dir = isolated_tmp_dir("tillvision_vws_native_description");
    let vws = dir.join("native_description.vws");
    write_tillvision_vws_with_contents(&vws, &tillvision_native_cimage_contents_with_description());

    let mut reader = ImageReader::open(&vws).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata.get("Info Date"),
        Some(MetadataValue::String(value)) if value == "05/26/26"
    ));
    assert!(matches!(
        meta.series_metadata.get("Info Start time of experiment"),
        Some(MetadataValue::String(value)) if value == "09:10:11 AM"
    ));
    assert!(matches!(
        meta.series_metadata.get("Info Exposure time [ms]"),
        Some(MetadataValue::String(value)) if value == "25.5"
    ));
    assert!(matches!(
        meta.series_metadata.get("Info Exposure time [s]"),
        Some(MetadataValue::String(value)) if value == "0.0255"
    ));
    assert!(matches!(
        meta.series_metadata.get("tillvision.exposure_time_seconds"),
        Some(MetadataValue::Float(value)) if (*value - 0.0255).abs() < 1.0e-12
    ));
    assert!(matches!(
        meta.series_metadata.get("Info Image type"),
        Some(MetadataValue::String(value)) if value == "fluorescence"
    ));
    assert!(matches!(
        meta.series_metadata.get("tillvision.image_type"),
        Some(MetadataValue::String(value)) if value == "fluorescence"
    ));
    assert!(matches!(
        meta.series_metadata.get("Info Acquisition date/time"),
        Some(MetadataValue::String(value)) if value == "05/26/26 09:10:11 AM"
    ));
    assert!(matches!(
        meta.series_metadata.get("tillvision.acquisition_datetime"),
        Some(MetadataValue::String(value)) if value == "05/26/26 09:10:11 AM"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("tillvision.acquisition_datetime_iso8601"),
        Some(MetadataValue::String(value)) if value == "2026-05-26T09:10:11"
    ));
    let ome = reader.ome_metadata().unwrap();
    assert_eq!(ome.images[0].name.as_deref(), Some("NativeImage"));
    assert_eq!(ome.images[0].planes.len(), 2);
    assert!(ome.images[0]
        .planes
        .iter()
        .all(|plane| plane.exposure_time == Some(0.0255)));
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_two_digit_year_metadata_uses_fixed_century_pivot() {
    let pst = tmp("two_digit_year_tillvision.pst");
    let inf = tmp("two_digit_year_tillvision.inf");
    std::fs::write(
        &inf,
        "Width=1\nHeight=1\nBands=1\nSlices=1\nFrames=1\nDatatype=2\nDate=12/31/99\nStart Time=11:59:58 PM\n",
    )
    .unwrap();
    std::fs::write(&pst, [42]).unwrap();

    let mut reader = ImageReader::open(&pst).unwrap();
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("tillvision.acquisition_datetime"),
        Some(MetadataValue::String(value)) if value == "12/31/99 11:59:58 PM"
    ));
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("tillvision.acquisition_datetime_iso8601"),
        Some(MetadataValue::String(value)) if value == "1999-12-31T23:59:58"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![42]);

    let _ = std::fs::remove_file(pst);
    let _ = std::fs::remove_file(inf);
}

#[test]
fn tillvision_vws_native_ole_reports_precise_cimage_blockers() {
    let dir = isolated_tmp_dir("tillvision_vws_native_errors");

    let empty_contents = dir.join("empty_contents.vws");
    write_tillvision_vws_with_contents(&empty_contents, b"not a cimage stream");
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&empty_contents).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("contains no supported CImage records")),
        "{err:?}"
    );

    let missing_contents = dir.join("missing_contents.vws");
    {
        let mut comp = cfb::create(&missing_contents).unwrap();
        comp.create_stream("/Other").unwrap();
    }
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&missing_contents).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("lacks Root Entry/Contents")),
        "{err:?}"
    );

    let short_payload = dir.join("short_payload_native.vws");
    let mut contents = tillvision_native_cimage_contents();
    contents.truncate(contents.len() - 1);
    write_tillvision_vws_with_contents(&short_payload, &contents);
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&short_payload).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("payload is shorter than declared")),
        "{err:?}"
    );

    let invalid_exposure = dir.join("invalid_exposure_native.vws");
    let mut contents = tillvision_native_cimage_contents();
    let description = b"Exposure time [ms]: not-a-number\r\n";
    contents.extend_from_slice(b"\0\0\0\0\0\xff");
    contents.extend_from_slice(&(description.len() as u16).to_le_bytes());
    contents.extend_from_slice(description);
    write_tillvision_vws_with_contents(&invalid_exposure, &contents);
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&invalid_exposure).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("invalid Exposure time [ms]")),
        "{err:?}"
    );

    let unsupported_compression = dir.join("unsupported_compression_native.vws");
    let description = b"Compression: lzw\r\n";
    write_tillvision_vws_with_contents(
        &unsupported_compression,
        &tillvision_native_cimage_contents_with_payload_and_description(
            &[1, 2, 3, 4, 5, 6, 7, 8],
            description,
        ),
    );
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&unsupported_compression).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("unsupported compression Compression: lzw")),
        "{err:?}"
    );

    let invalid_payload_offset = dir.join("invalid_payload_offset_native.vws");
    let description = b"Payload offset: 64\r\n";
    write_tillvision_vws_with_contents(
        &invalid_payload_offset,
        &tillvision_native_cimage_contents_with_payload_at_offset(
            &[1, 2, 3, 4, 5, 6, 7, 8],
            160,
            description,
        ),
    );
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&invalid_payload_offset).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("before parsed payload start")),
        "{err:?}"
    );

    let short_fragments = dir.join("short_fragment_table_native.vws");
    let description = b"Payload fragments: 160:2, 180:5\r\n";
    write_tillvision_vws_with_contents(
        &short_fragments,
        &tillvision_native_cimage_contents_with_payload_fragments(
            &[(160, &[1, 2]), (180, &[3, 4, 5, 6, 7])],
            description,
        ),
    );
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&short_fragments).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("fragments assemble to 7 bytes, expected 8")),
        "{err:?}"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_rejects_malformed_embedded_or_nonmatching_payloads() {
    let dir = isolated_tmp_dir("tillvision_vws_embedded_errors");
    let magic = *b"BFTILLVISIONVWS1";

    let truncated = dir.join("truncated.vws");
    let mut truncated_header = magic.to_vec();
    truncated_header.extend_from_slice(&[0, 0, 0, 0]);
    std::fs::write(&truncated, truncated_header).unwrap();
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&truncated).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("header is truncated")),
        "{err:?}"
    );

    let bad_dims = dir.join("bad_dims.vws");
    std::fs::write(
        &bad_dims,
        strict_misc_raw_bytes(&magic, 0, 2, 1, 1, &[1, 2]),
    )
    .unwrap();
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&bad_dims).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("dimensions must be non-zero")),
        "{err:?}"
    );

    let short_payload = dir.join("short_payload.vws");
    std::fs::write(
        &short_payload,
        strict_misc_raw_bytes(&magic, 2, 2, 1, 1, &[1, 2, 3]),
    )
    .unwrap();
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&short_payload).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("payload length mismatch")),
        "{err:?}"
    );

    let native = dir.join("native_placeholder.vws");
    std::fs::write(&native, b"TillVision workspace placeholder").unwrap();
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&native).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("no supported companion PST/INF pixels")),
        "{err:?}"
    );

    let fake = dir.join("fake.vws");
    std::fs::write(&fake, b"fake").unwrap();
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&fake).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("no supported companion PST/INF pixels")),
        "{err:?}"
    );

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn tillvision_vws_without_pst_sidecar_stays_unsupported() {
    let dir = isolated_tmp_dir("tillvision_vws_no_sidecar");
    let vws = dir.join("experiment.vws");
    std::fs::write(&vws, b"TillVision workspace placeholder").unwrap();

    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&vws).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("no supported companion PST/INF pixels")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.open_bytes(0),
        Err(BioFormatsError::NotInitialized)
    ));

    let _ = std::fs::remove_dir_all(dir);
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
    assert_eq!(reader.series_count(), 1);
    reader.set_series(0).unwrap();
    assert!(matches!(
        reader.metadata().series_metadata.get("simfcs.extension"),
        Some(MetadataValue::String(value)) if value == "b64"
    ));
    assert!(matches!(
        reader.metadata().series_metadata.get("simfcs.frame_bytes"),
        Some(MetadataValue::Int(65536))
    ));
    let ome = reader.ome_metadata().expect("SimFCS OME metadata");
    let original = ome
        .annotations
        .iter()
        .find_map(|annotation| match annotation {
            OmeAnnotation::MapAnnotation {
                id,
                namespace,
                values,
            } if id.as_deref() == Some("Annotation:OriginalMetadata:0")
                && namespace.as_deref()
                    == Some("openmicroscopy.org/bioformats/original-metadata") =>
            {
                Some(values)
            }
            _ => None,
        })
        .expect("SimFCS original metadata annotation");
    assert!(original
        .iter()
        .any(|(key, value)| key == "simfcs.extension" && value == "b64"));
    assert!(original
        .iter()
        .any(|(key, value)| key == "simfcs.payload_bytes" && value == "65536"));
    assert!(matches!(
        reader.set_series(1),
        Err(BioFormatsError::SeriesOutOfRange(1))
    ));
    assert_eq!(
        reader.open_bytes_region(0, 1, 1, 2, 1).unwrap(),
        vec![99, 2]
    );
    assert!(reader.open_bytes_region(0, 255, 0, 2, 1).is_err());
    assert!(reader.open_bytes_region(0, 0, 0, 0, 1).is_err());
    let _ = std::fs::remove_file(path);
}

#[test]
fn dcimg_rejects_out_of_bounds_regions() {
    let path = tmp("region_bounds.dcimg");
    let mut data = vec![0u8; 64];
    data[0..5].copy_from_slice(b"DCIMG");
    data[16..20].copy_from_slice(&64u32.to_le_bytes());
    data[20..24].copy_from_slice(&1u32.to_le_bytes());
    data[32..36].copy_from_slice(&2u32.to_le_bytes());
    data[36..40].copy_from_slice(&2u32.to_le_bytes());
    data[40..44].copy_from_slice(&8u32.to_le_bytes());
    data[48..52].copy_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&[1, 2, 3, 4]);
    std::fs::write(&path, &data).unwrap();

    let mut reader = bioformats::formats::hamamatsu::DcimgReader::new();
    reader.set_id(&path).unwrap();
    // DCIMG stores rows bottom-to-top; Java DCIMGReader.openBytes flips them
    // (row h-1-i). For the 2x2 plane [[1,2],[3,4]] the flipped plane is
    // [[3,4],[1,2]], so column 1 over both rows reads [4, 2].
    assert_eq!(reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(), vec![4, 2]);
    assert!(reader.open_bytes_region(0, 1, 0, 2, 1).is_err());
    assert!(reader.open_bytes_region(0, 0, 0, 0, 1).is_err());

    let _ = std::fs::remove_file(path);
}

#[test]
fn dcimg_ome_metadata_preserves_original_header_metadata() {
    let path = tmp("original_metadata.dcimg");
    let mut data = vec![0u8; 64];
    data[0..5].copy_from_slice(b"DCIMG");
    data[16..20].copy_from_slice(&64u32.to_le_bytes());
    data[20..24].copy_from_slice(&1u32.to_le_bytes());
    data[32..36].copy_from_slice(&2u32.to_le_bytes());
    data[36..40].copy_from_slice(&2u32.to_le_bytes());
    data[40..44].copy_from_slice(&8u32.to_le_bytes());
    data[48..52].copy_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&[1, 2, 3, 4]);
    std::fs::write(&path, &data).unwrap();

    let mut reader = bioformats::formats::hamamatsu::DcimgReader::new();
    reader.set_id(&path).unwrap();
    let ome = reader.ome_metadata().expect("DCIMG OME metadata");
    let values = ome
        .annotations
        .iter()
        .find_map(|annotation| match annotation {
            OmeAnnotation::MapAnnotation {
                id,
                namespace,
                values,
            } if id.as_deref() == Some("Annotation:OriginalMetadata:0")
                && namespace.as_deref()
                    == Some("openmicroscopy.org/bioformats/original-metadata") =>
            {
                Some(values)
            }
            _ => None,
        })
        .expect("DCIMG original metadata annotation");

    assert!(values
        .iter()
        .any(|(key, value)| key == "format" && value == "Hamamatsu DCIMG"));
    assert!(values
        .iter()
        .any(|(key, value)| key == "version" && value == "0"));
    assert!(values
        .iter()
        .any(|(key, value)| key == "header_size" && value == "64"));
    assert!(values
        .iter()
        .any(|(key, value)| key == "bit_depth" && value == "8"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn dcimg_rejects_clamped_dimensions_unknown_depth_and_short_payload() {
    let path = tmp("bad.dcimg");
    let mut data = vec![0u8; 64];
    data[0..5].copy_from_slice(b"DCIMG");
    data[16..20].copy_from_slice(&64u32.to_le_bytes());
    data[20..24].copy_from_slice(&0u32.to_le_bytes());
    data[32..36].copy_from_slice(&2u32.to_le_bytes());
    data[36..40].copy_from_slice(&2u32.to_le_bytes());
    data[40..44].copy_from_slice(&8u32.to_le_bytes());
    data[48..52].copy_from_slice(&2u32.to_le_bytes());
    std::fs::write(&path, &data).unwrap();

    let mut reader = bioformats::formats::hamamatsu::DcimgReader::new();
    assert_eq!(reader.series_count(), 0);
    let err = reader.set_id(&path).unwrap_err();
    assert!(err.to_string().contains("frame count"));
    assert_eq!(reader.series_count(), 0);

    data[20..24].copy_from_slice(&1u32.to_le_bytes());
    data[40..44].copy_from_slice(&12u32.to_le_bytes());
    std::fs::write(&path, &data).unwrap();
    let err = reader.set_id(&path).unwrap_err();
    assert!(err.to_string().contains("bit depth"));

    data[40..44].copy_from_slice(&8u32.to_le_bytes());
    std::fs::write(&path, &data).unwrap();
    let err = reader.set_id(&path).unwrap_err();
    assert!(err.to_string().contains("shorter than declared"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn clinical_raw_readers_reject_out_of_bounds_regions() {
    let dir = isolated_tmp_dir("clinical_regions");

    let inveon_hdr = dir.join("scan.hdr");
    let inveon_img = dir.join("scan.img");
    std::fs::write(
        &inveon_hdr,
        b"x_dimension 2\ny_dimension 2\nz_dimension 1\ndata_type 1\n",
    )
    .unwrap();
    std::fs::write(&inveon_img, [1, 2, 3, 4]).unwrap();
    let mut inveon = bioformats::formats::clinical::InveonReader::new();
    inveon.set_id(&inveon_hdr).unwrap();
    assert_eq!(inveon.open_bytes_region(0, 1, 0, 1, 2).unwrap(), vec![2, 4]);
    assert!(inveon.open_bytes_region(0, 1, 0, 2, 1).is_err());
    assert!(inveon.open_bytes_region(0, 0, 0, 0, 1).is_err());

    let fdf = dir.join("scan.fdf");
    let mut fdf_bytes =
        b"#!/usr/local/fdf/startup\nint matrix[] = {2, 2};\nint bits = 8;\n\x0c".to_vec();
    fdf_bytes.extend_from_slice(&[1, 2, 3, 4]);
    std::fs::write(&fdf, fdf_bytes).unwrap();
    let mut fdf_reader = bioformats::formats::clinical::FdfReader::new();
    fdf_reader.set_id(&fdf).unwrap();
    assert_eq!(
        fdf_reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
        vec![4, 2]
    );
    assert!(fdf_reader.open_bytes_region(0, 1, 0, 2, 1).is_err());
    assert!(fdf_reader.open_bytes_region(0, 0, 0, 0, 1).is_err());

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn clinical_readers_reject_fake_default_metadata() {
    let dir = isolated_tmp_dir("clinical_defaults");

    let bad_hdr = dir.join("fake.hdr");
    let bad_img = dir.join("fake.img");
    std::fs::write(&bad_hdr, b"not an inveon header\n").unwrap();
    std::fs::write(&bad_img, [1]).unwrap();
    let mut inveon = bioformats::formats::clinical::InveonReader::new();
    let err = inveon.set_id(&bad_hdr).unwrap_err();
    assert!(err.to_string().contains("x_dimension"));

    let short_img_hdr = dir.join("short.hdr");
    let short_img = dir.join("short.img");
    std::fs::write(
        &short_img_hdr,
        b"x_dimension 2\ny_dimension 2\nz_dimension 1\ndata_type 1\n",
    )
    .unwrap();
    std::fs::write(&short_img, [1, 2, 3]).unwrap();
    let mut inveon = bioformats::formats::clinical::InveonReader::new();
    let err = inveon.set_id(&short_img_hdr).unwrap_err();
    assert!(err.to_string().contains("shorter than declared"));

    let random_fdf = dir.join("random.fdf");
    std::fs::write(&random_fdf, [1]).unwrap();
    let mut fdf = bioformats::formats::clinical::FdfReader::new();
    let err = fdf.set_id(&random_fdf).unwrap_err();
    assert!(err.to_string().contains("FDF"));

    let missing_bits_fdf = dir.join("missing_bits.fdf");
    std::fs::write(
        &missing_bits_fdf,
        b"#!/usr/local/fdf/startup\nint matrix[] = {1, 1};\n\x0c\x01",
    )
    .unwrap();
    let mut fdf = bioformats::formats::clinical::FdfReader::new();
    let err = fdf.set_id(&missing_bits_fdf).unwrap_err();
    assert!(err.to_string().contains("bits"));

    let bad_ecat = dir.join("bad.v");
    let mut ecat = vec![0u8; 1538];
    ecat[1024..1026].copy_from_slice(&6i16.to_be_bytes());
    ecat[1028..1030].copy_from_slice(&1i16.to_be_bytes());
    ecat[1030..1032].copy_from_slice(&1i16.to_be_bytes());
    std::fs::write(&bad_ecat, ecat).unwrap();
    let mut reader = bioformats::formats::clinical::Ecat7Reader::new();
    let err = reader.set_id(&bad_ecat).unwrap_err();
    assert!(err.to_string().contains("MATRIX"));

    let zero_ecat = dir.join("zero.v");
    let mut ecat = vec![0u8; 1538];
    ecat[..6].copy_from_slice(b"MATRIX");
    ecat[1024..1026].copy_from_slice(&6i16.to_be_bytes());
    ecat[1028..1030].copy_from_slice(&1i16.to_be_bytes());
    ecat[1030..1032].copy_from_slice(&1i16.to_be_bytes());
    std::fs::write(&zero_ecat, ecat).unwrap();
    let mut reader = bioformats::formats::clinical::Ecat7Reader::new();
    let err = reader.set_id(&zero_ecat).unwrap_err();
    assert!(err.to_string().contains("zero image dimensions"));

    let _ = std::fs::remove_dir_all(dir);
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
        matches!(err, BioFormatsError::NotInitialized)
            || matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("CellWorX")),
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
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("2D image data elements")),
        "{err:?}"
    );
    assert!(matches!(
        fei.open_bytes(0),
        Err(BioFormatsError::NotInitialized)
    ));
    let _ = std::fs::remove_file(&ser);

    let short_al3d = tmp("short_payload.al3d");
    let mut al3d = vec![0u8; 512];
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

    let bad_al3d = tmp("bad_magic.al3d");
    let mut al3d = vec![0u8; 512];
    al3d[8..12].copy_from_slice(&1u32.to_le_bytes());
    al3d[12..16].copy_from_slice(&1u32.to_le_bytes());
    al3d[16..20].copy_from_slice(&1u32.to_le_bytes());
    al3d.extend_from_slice(&[1]);
    std::fs::write(&bad_al3d, al3d).unwrap();
    let mut reader = bioformats::formats::mias::Al3dReader::new();
    let err = reader.set_id(&bad_al3d).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("AL3D magic")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&bad_al3d);

    let zero_al3d = tmp("zero_dim.al3d");
    let mut al3d = vec![0u8; 512];
    al3d[..4].copy_from_slice(b"AL3D");
    al3d[12..16].copy_from_slice(&1u32.to_le_bytes());
    al3d[16..20].copy_from_slice(&1u32.to_le_bytes());
    al3d.extend_from_slice(&[1]);
    std::fs::write(&zero_al3d, al3d).unwrap();
    let mut reader = bioformats::formats::mias::Al3dReader::new();
    let err = reader.set_id(&zero_al3d).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("zero image dimensions")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&zero_al3d);

    let unsupported_al3d = tmp("unsupported_dtype.al3d");
    let mut al3d = vec![0u8; 512];
    al3d[..4].copy_from_slice(b"AL3D");
    al3d[8..12].copy_from_slice(&1u32.to_le_bytes());
    al3d[12..16].copy_from_slice(&1u32.to_le_bytes());
    al3d[16..20].copy_from_slice(&1u32.to_le_bytes());
    al3d[20..22].copy_from_slice(&99u16.to_le_bytes());
    al3d.extend_from_slice(&[1, 2]);
    std::fs::write(&unsupported_al3d, al3d).unwrap();
    let mut reader = bioformats::formats::mias::Al3dReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    let err = reader.set_id(&unsupported_al3d).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("AL3D data type 99")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&unsupported_al3d);

    let path = tmp("real_payload.top");
    let mut top = vec![0u8; 128];
    top[4..6].copy_from_slice(&2u16.to_le_bytes());
    top[6..8].copy_from_slice(&2u16.to_le_bytes());
    top[8..10].copy_from_slice(&0u16.to_le_bytes());
    top.extend_from_slice(&[1, 2, 3, 4]);
    std::fs::write(&path, top).unwrap();
    let mut oxford = bioformats::formats::mias::OxfordInstrumentsReader::new();
    assert_eq!(oxford.series_count(), 0);
    oxford.set_id(&path).unwrap();
    assert_eq!(oxford.series_count(), 1);
    oxford.set_series(0).unwrap();
    assert_eq!(oxford.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(oxford.open_bytes_region(0, 1, 0, 1, 2).unwrap(), vec![2, 4]);
    let _ = std::fs::remove_file(path);

    let unsupported_top = tmp("unsupported_dtype.top");
    let mut top = vec![0u8; 128];
    top[4..6].copy_from_slice(&1u16.to_le_bytes());
    top[6..8].copy_from_slice(&1u16.to_le_bytes());
    top[8..10].copy_from_slice(&99u16.to_le_bytes());
    top.extend_from_slice(&[1, 2]);
    std::fs::write(&unsupported_top, top).unwrap();
    let mut oxford = bioformats::formats::mias::OxfordInstrumentsReader::new();
    let err = oxford.set_id(&unsupported_top).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Oxford TOP data type 99")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(unsupported_top);
}

// NOTE: cellworx_strict_raw_* tests were removed — CellWorxReader is now a real
// MetaXpress/CellWorX HCS reader (HTD index + TIFF delegation), not the former
// synthetic BF_CELLWORX_RAW_V1 stub. See tests/stub_cellworx_test.rs.

#[test]
fn fei_ser_reads_synthetic_2d_image_elements() {
    let path = tmp("synthetic_2d.ser");
    let frame0 = vec![1, 2, 3, 4, 5, 6];
    let frame1 = vec![11, 12, 13, 14, 15, 16];
    std::fs::write(
        &path,
        synthetic_fei_ser_u8(3, 2, &[frame0.clone(), frame1.clone()]),
    )
    .unwrap();

    let mut reader = bioformats::formats::mias::FeiSerReader::new();
    assert!(reader.is_this_type_by_bytes(&[0x97, 0x01, 0, 0]));
    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
    assert_eq!(reader.open_bytes(0).unwrap(), frame0);
    assert_eq!(reader.open_bytes(1).unwrap(), frame1);
    assert_eq!(
        reader.open_bytes_region(1, 1, 0, 2, 2).unwrap(),
        vec![12, 13, 15, 16]
    );
    assert!(matches!(
        reader.open_bytes(2),
        Err(BioFormatsError::PlaneOutOfRange(2))
    ));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn fei_ser_rejects_truncated_offset_array_and_payloads() {
    let short_offsets = tmp("short_offsets.ser");
    let mut data = synthetic_fei_ser_u8(1, 1, &[vec![7]]);
    data.truncate(29);
    std::fs::write(&short_offsets, data).unwrap();
    let mut reader = bioformats::formats::mias::FeiSerReader::new();
    let err = reader.set_id(&short_offsets).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("offset array")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&short_offsets);

    let short_payload = tmp("short_payload.ser");
    let mut data = synthetic_fei_ser_u8(2, 2, &[vec![1, 2, 3, 4]]);
    data.pop();
    std::fs::write(&short_payload, data).unwrap();
    let mut reader = bioformats::formats::mias::FeiSerReader::new();
    let err = reader.set_id(&short_payload).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("payload")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&short_payload);
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

    let uninit = bioformats::formats::zip::ZipReader::new();
    assert_eq!(uninit.series_count(), 0);

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
fn zip_preserves_companion_file_relative_paths() {
    use std::io::Write;

    let dir = isolated_tmp_dir("zip_companion_paths");
    let zip_path = dir.join("sample.zip");
    let file = std::fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();

    let header = "ics_version\t1.0\nlayout\torder\tbits x y\nlayout\tsizes\t8 2 2\nlayout\tsignificant_bits\t8\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t1 2 3 4\nrepresentation\tcompression\tuncompressed\n";
    zip.start_file("README.txt", options).unwrap();
    zip.write_all(b"not an image").unwrap();
    zip.start_file("sample.ics", options).unwrap();
    zip.write_all(header.as_bytes()).unwrap();
    zip.start_file("sample.ids", options).unwrap();
    zip.write_all(&[1, 2, 3, 4]).unwrap();
    zip.finish().unwrap();

    let mut reader = ImageReader::open(&zip_path).unwrap();

    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
}

#[test]
fn zip_primary_entry_requires_base_name_boundary() {
    use std::io::Write;

    let dir = isolated_tmp_dir("zip_primary_boundary");
    let mut meta = ImageMetadata::default();
    meta.size_x = 1;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let wrong_tiff = dir.join("wrong.tif");
    ImageWriter::save(&wrong_tiff, &meta, &[vec![99]]).unwrap();
    let wrong_bytes = std::fs::read(&wrong_tiff).unwrap();

    let right_tiff = dir.join("right.tif");
    ImageWriter::save(&right_tiff, &meta, &[vec![7]]).unwrap();
    let right_bytes = std::fs::read(&right_tiff).unwrap();

    let zip_path = dir.join("sample.zip");
    let file = std::fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("sample2.tif", options).unwrap();
    zip.write_all(&wrong_bytes).unwrap();
    zip.start_file("sample.tif", options).unwrap();
    zip.write_all(&right_bytes).unwrap();
    zip.finish().unwrap();

    let mut reader = ImageReader::open(&zip_path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![7]);
}

#[test]
fn zip_skips_non_numeric_text_entries_before_images() {
    use std::io::Write;

    let dir = isolated_tmp_dir("zip_skips_text");
    let tiff_src = dir.join("source.tif");
    let mut meta = ImageMetadata::default();
    meta.size_x = 2;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;
    let pixels = vec![7u8, 9];
    ImageWriter::save(&tiff_src, &meta, &[pixels.clone()]).unwrap();
    let tiff_bytes = std::fs::read(&tiff_src).unwrap();

    let zip_path = dir.join("bundle.zip");
    let file = std::fs::File::create(&zip_path).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    let options = zip::write::SimpleFileOptions::default();
    zip.start_file("notes.csv", options).unwrap();
    zip.write_all(b"not,image,data\n").unwrap();
    zip.start_file("frame.tif", options).unwrap();
    zip.write_all(&tiff_bytes).unwrap();
    zip.finish().unwrap();

    let mut reader = ImageReader::open(&zip_path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 1);
    assert_eq!(reader.open_bytes(0).unwrap(), pixels);
}

#[test]
fn metamorph_requires_initialization_for_series() {
    let mut reader = bioformats::formats::metamorph::MetamorphReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
}

#[test]
fn zip_failed_reopen_clears_previous_inner_reader() {
    use std::io::Write;

    let dir = isolated_tmp_dir("zip_failed_reopen");
    let mut meta = ImageMetadata::default();
    meta.size_x = 1;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.image_count = 1;

    let tiff_src = dir.join("source.tif");
    ImageWriter::save(&tiff_src, &meta, &[vec![42]]).unwrap();
    let tiff_bytes = std::fs::read(&tiff_src).unwrap();

    let good_zip = dir.join("good.zip");
    let file = std::fs::File::create(&good_zip).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("good.tif", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(&tiff_bytes).unwrap();
    zip.finish().unwrap();

    let bad_zip = dir.join("bad.zip");
    let file = std::fs::File::create(&bad_zip).unwrap();
    let mut zip = zip::ZipWriter::new(file);
    zip.start_file("bad.txt", zip::write::SimpleFileOptions::default())
        .unwrap();
    zip.write_all(b"not image data").unwrap();
    zip.finish().unwrap();

    let mut reader = bioformats::formats::zip::ZipReader::new();
    reader.set_id(&good_zip).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![42]);

    let err = reader.set_id(&bad_zip).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(_)),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.open_bytes(0),
        Err(BioFormatsError::NotInitialized)
    ));
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
fn unisoku_rejects_zero_bit_depth() {
    let path = tmp("zero_bit_unisoku.hdr");
    let dat = tmp("zero_bit_unisoku.dat");
    std::fs::write(&path, b"XSIZE=1\nYSIZE=1\nBIT=0\n").unwrap();
    std::fs::write(&dat, [1, 2]).unwrap();

    let mut reader = bioformats::formats::afm::UnisokuReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("invalid BIT depth")),
        "{err:?}"
    );

    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(dat);
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
fn unisoku_ascii_data_type_float32_is_supported() {
    let dir = isolated_tmp_dir("unisoku_float32");
    let hdr = dir.join("float.HDR");
    let dat = dir.join("float.DAT");
    std::fs::write(
        &hdr,
        b":STM data\r:data volume(x*y)\r2 1\r:ascii flag; data type\r0 8\r",
    )
    .unwrap();
    std::fs::write(&dat, [0, 0, 0x80, 0x3f, 0, 0, 0, 0x40]).unwrap();

    let mut reader = ImageReader::open(&hdr).unwrap();
    assert_eq!(reader.metadata().pixel_type, PixelType::Float32);
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        vec![0, 0, 0x80, 0x3f, 0, 0, 0, 0x40]
    );
}

#[test]
fn unisoku_unsupported_ascii_data_type_names_boundary() {
    let dir = isolated_tmp_dir("unisoku_unsupported_ascii_type");
    let hdr = dir.join("unsupported.HDR");
    let dat = dir.join("unsupported.DAT");
    std::fs::write(
        &hdr,
        b":STM data\r:data volume(x*y)\r1 1\r:ascii flag; data type\r0 6\r",
    )
    .unwrap();
    std::fs::write(&dat, [0, 0, 0]).unwrap();

    let mut reader = bioformats::formats::afm::UnisokuReader::new();
    let err = reader.set_id(&hdr).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("unsupported ASCII data type 6")),
        "{err:?}"
    );
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
fn spm_remaining_placeholders_read_strict_raw_subsets() {
    let path = tmp("strict.afm");
    let plane0 = vec![1u8, 2, 3, 4, 5, 6];
    let plane1 = vec![11u8, 12, 13, 14, 15, 16];
    let mut payload = plane0.clone();
    payload.extend_from_slice(&plane1);
    let magic = *b"BFQUESANTAFMRAW!";
    std::fs::write(&path, strict_spm_raw_bytes(&magic, 3, 2, 2, 1, &payload)).unwrap();

    let mut reader = bioformats::formats::spm::QuesantReader::new();
    assert!(reader.is_this_type_by_bytes(&magic));
    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_t, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
    assert_eq!(reader.open_bytes(0).unwrap(), plane0);
    assert_eq!(reader.open_bytes(1).unwrap(), plane1);
    assert_eq!(
        reader.open_bytes_region(1, 1, 0, 2, 2).unwrap(),
        vec![12, 13, 15, 16]
    );
    assert!(matches!(
        reader.open_bytes(2),
        Err(BioFormatsError::PlaneOutOfRange(2))
    ));

    let _ = std::fs::remove_file(path);
}

#[test]
fn quesant_reads_java_variable_table_native_pixels() {
    let path = tmp("native_quesant.afm");
    let pixel_offset = 1024usize;
    let mut data = vec![0u8; pixel_offset];
    data[0..4].copy_from_slice(b"IMAG");
    data[4..8].copy_from_slice(&(pixel_offset as u32).to_le_bytes());
    data[8..12].copy_from_slice(b"SDES");
    data[12..16].copy_from_slice(&900u32.to_le_bytes());
    data[900..915].copy_from_slice(b"native quesant\0");
    data.extend_from_slice(&3u16.to_le_bytes());
    data.extend_from_slice(&[1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0, 7, 0, 8, 0, 9, 0]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::QuesantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!((meta.size_x, meta.size_y), (3, 3));
    assert_eq!(meta.pixel_type, PixelType::Uint16);
    assert_eq!(meta.dimension_order, DimensionOrder::XYZCT);
    assert!(matches!(
        meta.series_metadata.get("Quesant description"),
        Some(MetadataValue::String(s)) if s == "native quesant"
    ));
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 0, 3, 0, 5, 0, 6, 0]
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn spm_strict_raw_rejects_malformed_or_nonmatching_inputs() {
    let magic = *b"BFQUESANTAFMRAW!";
    let cases: Vec<(&str, Vec<u8>, &str)> = vec![
        (
            "short_strict.afm",
            b"BFQUESANTAFMRAW!".to_vec(),
            "header missing width",
        ),
        (
            "zero_strict.afm",
            strict_spm_raw_bytes(&magic, 0, 2, 1, 1, &[1, 2]),
            "dimensions must be non-zero",
        ),
        (
            "bad_type_strict.afm",
            strict_spm_raw_bytes(&magic, 2, 2, 1, 99, &[1, 2, 3, 4]),
            "unsupported pixel type code",
        ),
        (
            "short_payload_strict.afm",
            strict_spm_raw_bytes(&magic, 2, 2, 1, 1, &[1, 2, 3]),
            "payload length mismatch",
        ),
        (
            "bad_offset_strict.afm",
            {
                let mut data = strict_spm_raw_bytes(&magic, 2, 2, 1, 1, &[1, 2, 3, 4]);
                data[32..40].copy_from_slice(&39u64.to_le_bytes());
                data
            },
            "data offset points into header",
        ),
    ];

    for (name, bytes, expected) in cases {
        let path = tmp(name);
        std::fs::write(&path, bytes).unwrap();
        let mut reader = bioformats::formats::spm::QuesantReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains(expected)),
            "{name}: {err:?}"
        );
        assert_eq!(reader.series_count(), 0, "{name}");
        assert_eq!(reader.metadata().size_x, 0, "{name}");
        let _ = std::fs::remove_file(path);
    }

    let path = tmp("heuristic_fake.afm");
    std::fs::write(&path, [0u8; 32]).unwrap();
    let mut reader = bioformats::formats::spm::QuesantReader::new();
    assert!(!reader.is_this_type_by_bytes(&[0u8; 32]));
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("refusing heuristic dimensions")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);
    let _ = std::fs::remove_file(path);
}

#[test]
fn spm_stateful_readers_clear_failed_reopen_and_require_initialization() {
    let magic = *b"BFQUESANTAFMRAW!";
    let good = tmp("good_strict_state.afm");
    std::fs::write(&good, strict_spm_raw_bytes(&magic, 1, 1, 1, 1, &[7])).unwrap();
    let bad = tmp("bad_strict_state.afm");
    std::fs::write(&bad, [0u8; 16]).unwrap();

    let mut quesant = bioformats::formats::spm::QuesantReader::new();
    assert_eq!(quesant.series_count(), 0);
    assert!(matches!(
        quesant.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    quesant.set_id(&good).unwrap();
    assert_eq!(quesant.series_count(), 1);
    assert!(quesant.set_id(&bad).is_err());
    assert_eq!(quesant.series_count(), 0);
    assert_eq!(quesant.metadata().size_x, 0);

    let mut vgsam = bioformats::formats::spm::VgSamReader::new();
    assert_eq!(vgsam.series_count(), 0);
    assert!(matches!(
        vgsam.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    let _ = std::fs::remove_file(good);
    let _ = std::fs::remove_file(bad);
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
    // Java WATOPReader layout (seek 211, 5 ints, skip 8 -> 239): xSize@239,
    // ySize@243, zSize@247, then sizeX@251, sizeY@255 (WATOPReader.java:104-120).
    data[239..243].copy_from_slice(&300i32.to_le_bytes());
    data[243..247].copy_from_slice(&200i32.to_le_bytes());
    data[247..251].copy_from_slice(&100i32.to_le_bytes());
    data[251..255].copy_from_slice(&3i32.to_le_bytes());
    data[255..259].copy_from_slice(&2i32.to_le_bytes());
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
fn sem_stateful_readers_clear_failed_reopen_and_require_initialization() {
    let inr_good = tmp("good_state.inr");
    let mut header = b"#INRIMAGE-4#{\nXDIM=1\nYDIM=1\nZDIM=1\nVDIM=1\nPIXSIZE=8 bits\nTYPE=unsigned fixed\nCPU=pc\n".to_vec();
    header.resize(256, b'\n');
    header.push(9);
    std::fs::write(&inr_good, header).unwrap();
    let inr_bad = tmp("bad_state.inr");
    std::fs::write(&inr_bad, b"#INRIMAGE-4#{").unwrap();

    let mut inr = bioformats::formats::sem::InrReader::new();
    assert_eq!(inr.series_count(), 0);
    assert!(matches!(
        inr.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    inr.set_id(&inr_good).unwrap();
    assert_eq!(inr.series_count(), 1);
    assert!(inr.set_id(&inr_bad).is_err());
    assert_eq!(inr.series_count(), 0);
    assert_eq!(inr.metadata().size_x, 0);

    let magic = b"BIOFORMATS-RS-JEOL-SEM-STRICT-RAW-V1\n";
    let jeol_good = tmp("good_state.dat");
    let mut jeol_bytes = magic.to_vec();
    jeol_bytes.extend_from_slice(&1u32.to_le_bytes());
    jeol_bytes.extend_from_slice(&1u32.to_le_bytes());
    jeol_bytes.extend_from_slice(&1u16.to_le_bytes());
    jeol_bytes.extend_from_slice(&0u16.to_le_bytes());
    jeol_bytes.push(5);
    std::fs::write(&jeol_good, jeol_bytes).unwrap();
    let jeol_bad = tmp("bad_state.dat");
    std::fs::write(&jeol_bad, [0u8; 16]).unwrap();
    let mut jeol = bioformats::formats::sem::JeolReader::new();
    assert_eq!(jeol.series_count(), 0);
    assert!(matches!(
        jeol.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    jeol.set_id(&jeol_good).unwrap();
    assert_eq!(jeol.series_count(), 1);
    assert!(jeol.set_id(&jeol_bad).is_err());
    assert_eq!(jeol.series_count(), 0);
    assert_eq!(jeol.metadata().size_x, 0);

    let _ = std::fs::remove_file(inr_good);
    let _ = std::fs::remove_file(inr_bad);
    let _ = std::fs::remove_file(jeol_good);
    let _ = std::fs::remove_file(jeol_bad);
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

fn strict_sem_raw(
    width: u32,
    height: u32,
    pixel_type: u16,
    payload: &[u8],
    magic: &[u8],
) -> Vec<u8> {
    let mut data = magic.to_vec();
    data.extend_from_slice(&width.to_le_bytes());
    data.extend_from_slice(&height.to_le_bytes());
    data.extend_from_slice(&pixel_type.to_le_bytes());
    data.extend_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(payload);
    data
}

#[test]
fn sem_remaining_readers_open_strict_raw_subsets() {
    let pixels = vec![1, 0, 2, 0, 3, 0, 4, 0, 5, 0, 6, 0];
    let cases: Vec<(&str, &[u8], Box<dyn FormatReader>)> = vec![
        (
            "strict.dat",
            b"BIOFORMATS-RS-JEOL-SEM-STRICT-RAW-V1\n",
            Box::new(bioformats::formats::sem::JeolReader::new()),
        ),
        (
            "strict.lms",
            b"BIOFORMATS-RS-ZEISS-LMS-STRICT-RAW-V1\n",
            Box::new(bioformats::formats::sem::ZeissLmsReader::new()),
        ),
    ];

    for (name, magic, mut reader) in cases {
        let path = tmp(name);
        let data = strict_sem_raw(3, 2, 2, &pixels, magic);
        assert!(reader.is_this_type_by_bytes(&data));
        std::fs::write(&path, data).unwrap();

        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 1, "{name}");
        assert_eq!(reader.metadata().size_x, 3, "{name}");
        assert_eq!(reader.metadata().size_y, 2, "{name}");
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint16, "{name}");
        assert_eq!(reader.open_bytes(0).unwrap(), pixels, "{name}");
        assert_eq!(
            reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
            vec![2, 0, 3, 0, 5, 0, 6, 0],
            "{name}"
        );
        assert!(matches!(
            reader.open_bytes_region(0, 2, 0, 2, 1),
            Err(BioFormatsError::Format(_))
        ));
    }
}

#[test]
fn jeol_reads_java_mg_native_uint8_pixels() {
    let path = tmp("native_mg.dat");
    let pixel_offset = 0x644usize + 540;
    let mut data = vec![0u8; pixel_offset];
    data[0..2].copy_from_slice(b"MG");
    data[0x63c..0x640].copy_from_slice(&3u32.to_le_bytes());
    data[0x640..0x644].copy_from_slice(&2u32.to_le_bytes());
    data.extend_from_slice(&[1, 2, 3, 4, 5, 6]);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::sem::JeolReader::new();
    assert!(reader.is_this_type_by_bytes(b"MG"));
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!((meta.size_x, meta.size_y), (3, 2));
    assert_eq!(meta.pixel_type, PixelType::Uint8);
    assert_eq!(meta.dimension_order, DimensionOrder::XYZCT);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 2).unwrap(),
        vec![2, 3, 5, 6]
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn jeol_par_resolves_companion_image_like_java() {
    let par = tmp("native_pair.PAR");
    let img = tmp("native_pair.IMG");
    let pixel_offset = 4usize + 56;
    let mut data = vec![0u8; pixel_offset];
    data[0..2].copy_from_slice(b"IM");
    data[2..4].copy_from_slice(&0u16.to_le_bytes());
    data.extend_from_slice(&[1, 2, 3, 4]);
    data.resize(pixel_offset + 1024, 0);
    std::fs::write(&img, data).unwrap();
    std::fs::write(&par, b"parameters").unwrap();

    let mut reader = bioformats::formats::sem::JeolReader::new();
    reader.set_id(&par).unwrap();
    assert_eq!(
        (reader.metadata().size_x, reader.metadata().size_y),
        (1024, 1)
    );
    assert_eq!(&reader.open_bytes(0).unwrap()[..4], &[1, 2, 3, 4]);

    let _ = std::fs::remove_file(par);
    let _ = std::fs::remove_file(img);
}

#[test]
fn zeiss_lms_reads_java_markers_lut_and_thumbnail_series() {
    let path = tmp("native.lms");
    let width = 1280usize;
    let height = 1024usize;
    let thumb_bytes = width * height * 3;
    let main_bytes = width * height * 2;

    let mut data = vec![0u8; 64];
    data[0..6].copy_from_slice(b"LMSFLE");
    data[18..22].copy_from_slice(&40u32.to_le_bytes());
    data[32..36].copy_from_slice(b"BM6!");
    let thumb_offset = 32 + 4 + 50;
    data.resize(thumb_offset, 0);
    data.extend_from_slice(&[10, 20, 30, 40, 50, 60]);
    data.resize(thumb_offset + thumb_bytes, 0);
    data.extend_from_slice(b"BM6!");
    data.extend_from_slice(&[0u8; 50]);
    for i in 0..256u16 {
        data.push(i as u8);
        data.push(255u8.wrapping_sub(i as u8));
        data.push((i / 2) as u8);
        data.push(0);
    }
    data.extend_from_slice(&[1, 0, 2, 0, 3, 0]);
    data.resize(data.len() + main_bytes - 6, 0);
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::sem::ZeissLmsReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 2);
    let main = reader.metadata();
    assert_eq!((main.size_x, main.size_y, main.size_z), (1280, 1024, 1));
    assert_eq!(main.pixel_type, PixelType::Uint16);
    assert!(main.is_indexed);
    assert_eq!(main.lookup_table.as_ref().unwrap().red[2], 2);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 1).unwrap(),
        vec![2, 0, 3, 0]
    );

    reader.set_series(1).unwrap();
    let thumb = reader.metadata();
    assert_eq!((thumb.size_x, thumb.size_y, thumb.size_c), (1280, 1024, 3));
    assert!(thumb.is_rgb);
    assert_eq!(
        reader.open_bytes_region(0, 0, 0, 2, 1).unwrap(),
        vec![10, 20, 30, 40, 50, 60]
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn sem_strict_raw_rejects_invalid_headers() {
    let bad_payload = strict_sem_raw(
        3,
        2,
        2,
        &[1, 0, 2, 0],
        b"BIOFORMATS-RS-JEOL-SEM-STRICT-RAW-V1\n",
    );
    let path = tmp("strict_bad_payload.dat");
    std::fs::write(&path, bad_payload).unwrap();
    let mut reader = bioformats::formats::sem::JeolReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("payload length mismatch")),
        "{err:?}"
    );

    let bad_pixel_type = strict_sem_raw(1, 1, 99, &[0], b"BIOFORMATS-RS-ZEISS-LMS-STRICT-RAW-V1\n");
    let path = tmp("strict_bad_pixel_type.lms");
    std::fs::write(&path, bad_pixel_type).unwrap();
    let mut reader = bioformats::formats::sem::ZeissLmsReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("unsupported pixel type code")),
        "{err:?}"
    );
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

/// Write a PerkinElmer-densitometer PDS header (`.hdr`, magic " IDENTIFICATION",
/// CRLF `KEY = value` lines) matching the reimplemented PdsReader. Returns the
/// header path; no companion pixel file is written.
fn write_pds_header(
    stem: &str,
    sign_x: char,
    sign_y: char,
    color: u32,
    size_x: usize,
    size_y: usize,
    record_width: usize,
) -> std::path::PathBuf {
    let hdr_path = tmp(stem);
    let mut hdr = Vec::new();
    hdr.extend_from_slice(b" IDENTIFICATION\r\n");
    hdr.extend_from_slice(format!("NXP = {size_x}\r\n").as_bytes());
    hdr.extend_from_slice(format!("NYP = {size_y}\r\n").as_bytes());
    hdr.extend_from_slice(format!("SIGNX = '{sign_x}'\r\n").as_bytes());
    hdr.extend_from_slice(format!("SIGNY = '{sign_y}'\r\n").as_bytes());
    hdr.extend_from_slice(format!("COLOR = {color}\r\n").as_bytes());
    // The reader divides FILE REC LEN by 2 to get the record (row-pad) width.
    hdr.extend_from_slice(format!("FILE REC LEN = {}\r\n", record_width * 2).as_bytes());
    hdr.extend_from_slice(b"END\r\n");
    std::fs::write(&hdr_path, hdr).unwrap();
    hdr_path
}

/// Write a full PDS dataset: a `.hdr` plus its companion `.IMG` (UINT16 LE, each
/// on-disk row padded to `record_width` with 0xFFFF sentinels). `pixels` is
/// row-major across `size_c` planar planes of `size_x * size_y` samples.
fn write_pds_fixture(
    stem: &str,
    sign_x: char,
    sign_y: char,
    color: u32,
    size_x: usize,
    size_y: usize,
    record_width: usize,
    pixels: &[u16],
) -> std::path::PathBuf {
    let hdr_path = write_pds_header(stem, sign_x, sign_y, color, size_x, size_y, record_width);
    let img_path = hdr_path.with_extension("IMG");
    let pad = record_width - (size_x % record_width);
    let planes = pixels.len() / (size_x * size_y);
    let mut img = Vec::new();
    let mut idx = 0;
    for _ in 0..planes {
        for _ in 0..size_y {
            for _ in 0..size_x {
                img.extend_from_slice(&pixels[idx].to_le_bytes());
                idx += 1;
            }
            for _ in 0..pad {
                img.extend_from_slice(&0xFFFFu16.to_le_bytes());
            }
        }
    }
    std::fs::write(&img_path, img).unwrap();
    hdr_path
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
    assert_eq!(arf.series_count(), 1);
    arf.set_series(0).unwrap();
    assert_eq!(
        arf.open_bytes_region(0, 1, 1, 2, 2).unwrap(),
        vec![5, 0, 6, 0, 8, 0, 9, 0]
    );

    // PDS (Perkin Elmer densitometer): a .hdr + companion .IMG. 3x2 UINT16
    // grayscale, record width 4 (one pad sample per row).
    let pds_path = write_pds_fixture(
        "pds_crop.hdr",
        '+',
        '+',
        1,
        3,
        2,
        4,
        &[10, 20, 30, 40, 50, 60],
    );
    let mut pds = bioformats::formats::misc4::PdsReader::new();
    pds.set_id(&pds_path).unwrap();
    assert_eq!(pds.series_count(), 1);
    pds.set_series(0).unwrap();
    let pds_expected: Vec<u8> = [20u16, 30, 50, 60]
        .iter()
        .flat_map(|s| s.to_le_bytes())
        .collect();
    assert_eq!(pds.open_bytes_region(0, 1, 0, 2, 2).unwrap(), pds_expected);

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
}

#[test]
fn tecan_reader_rejects_nonnumeric_rows() {
    let asc_path = tmp("bad_tecan.asc");
    std::fs::write(&asc_path, b"1\t2\n3\tbad\n").unwrap();
    let mut asc = bioformats::formats::hcs2::TecanReader::new();
    assert_eq!(asc.series_count(), 0);
    assert!(matches!(
        asc.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    let err = asc.set_id(&asc_path).unwrap_err();
    assert!(
        err.to_string().contains("non-numeric cell"),
        "unexpected Tecan error: {err}"
    );
    assert_eq!(asc.series_count(), 0);
}

#[test]
fn hcs2_binary_and_text_readers_clear_failed_reopen() {
    let valid = tmp("valid.frm");
    let mut data = vec![0u8; 6];
    write_i16_le(&mut data, 0, 6);
    write_i16_le(&mut data, 2, 1);
    write_i16_le(&mut data, 4, 33);
    std::fs::write(&valid, data).unwrap();

    let invalid = tmp("invalid.frm");
    std::fs::write(&invalid, [0u8; 6]).unwrap();

    let mut frm = bioformats::formats::hcs2::InCell3000Reader::new();
    assert_eq!(frm.series_count(), 0);
    assert!(matches!(
        frm.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    frm.set_id(&valid).unwrap();
    assert_eq!(frm.series_count(), 1);
    let err = frm.set_id(&invalid).unwrap_err();
    assert!(
        err.to_string().contains("invalid dimensions"),
        "unexpected InCell3000 error: {err}"
    );
    assert_eq!(frm.series_count(), 0);

    let _ = std::fs::remove_file(valid);
    let _ = std::fs::remove_file(invalid);
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
fn hamamatsu_his_unpacks_packed_12_bit_grayscale_planes() {
    let path = tmp("packed12_gray.his");
    let values = [0x0abcu16, 0x0123, 0x0fff];
    let mut data = Vec::new();
    append_his_series(
        &mut data,
        1,
        3,
        1,
        6,
        b"vDate=2026/05/29;",
        &pack_his_12(&values),
    );
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::misc4::HisReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
    assert_eq!(reader.metadata().bits_per_pixel, 12);
    assert_eq!(reader.metadata().size_c, 1);
    assert!(!reader.metadata().is_rgb);

    let mut expected = Vec::new();
    for value in values {
        expected.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(reader.open_bytes(0).unwrap(), expected);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 2, 1).unwrap(),
        expected[2..].to_vec()
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn hamamatsu_his_unpacks_packed_12_bit_rgb_regions() {
    let path = tmp("packed12_rgb.his");
    let values = [
        0x001u16, 0x002, 0x003, // pixel 0 RGB
        0x0a0, 0x0b0, 0x0c0, // pixel 1 RGB
    ];
    let mut data = Vec::new();
    append_his_series(&mut data, 1, 2, 1, 14, b"", &pack_his_12(&values));
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::misc4::HisReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
    assert_eq!(reader.metadata().bits_per_pixel, 12);
    assert_eq!(reader.metadata().size_c, 3);
    assert!(reader.metadata().is_rgb);
    assert!(reader.metadata().is_interleaved);

    let mut expected_pixel = Vec::new();
    for value in &values[3..] {
        expected_pixel.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 1, 1).unwrap(),
        expected_pixel
    );

    let _ = std::fs::remove_file(path);
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

    let arf_zero_path = tmp("zero_dim.arf");
    let mut bad = vec![1u8, 0];
    bad.extend_from_slice(b"AR");
    bad.extend_from_slice(&1u16.to_le_bytes());
    bad.extend_from_slice(&0u16.to_le_bytes());
    bad.extend_from_slice(&2u16.to_le_bytes());
    bad.extend_from_slice(&8u16.to_le_bytes());
    bad.resize(524, 0);
    std::fs::write(&arf_zero_path, bad).unwrap();
    let mut arf = bioformats::formats::misc4::ArfReader::new();
    let err = arf.set_id(&arf_zero_path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("zero image dimensions")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&arf_zero_path);

    let arf_zero_count_path = tmp("zero_count.arf");
    let mut bad = vec![1u8, 0];
    bad.extend_from_slice(b"AR");
    bad.extend_from_slice(&2u16.to_le_bytes());
    bad.extend_from_slice(&2u16.to_le_bytes());
    bad.extend_from_slice(&2u16.to_le_bytes());
    bad.extend_from_slice(&8u16.to_le_bytes());
    bad.extend_from_slice(&0u16.to_le_bytes());
    bad.resize(524, 0);
    std::fs::write(&arf_zero_count_path, bad).unwrap();
    let mut arf = bioformats::formats::misc4::ArfReader::new();
    let err = arf.set_id(&arf_zero_count_path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("zero image count")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&arf_zero_count_path);

    let arf_short_path = tmp("short_payload.arf");
    let mut bad = vec![1u8, 0];
    bad.extend_from_slice(b"AR");
    bad.extend_from_slice(&1u16.to_le_bytes());
    bad.extend_from_slice(&2u16.to_le_bytes());
    bad.extend_from_slice(&2u16.to_le_bytes());
    bad.extend_from_slice(&8u16.to_le_bytes());
    bad.resize(524, 0);
    bad.extend_from_slice(&[1, 2, 3]);
    std::fs::write(&arf_short_path, bad).unwrap();
    let mut arf = bioformats::formats::misc4::ArfReader::new();
    let err = arf.set_id(&arf_short_path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("ARF payload")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&arf_short_path);

    // PDS: a companion shorter than the declared (padded) plane is rejected.
    let pds_short_path = write_pds_header("pds_short.hdr", '+', '+', 1, 3, 2, 4);
    std::fs::write(pds_short_path.with_extension("IMG"), [0u8; 4]).unwrap();
    let mut pds = bioformats::formats::misc4::PdsReader::new();
    let err = pds.set_id(&pds_short_path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("shorter than declared")),
        "{err:?}"
    );

    // PDS: a missing companion pixel file is rejected at set_id.
    let pds_no_companion = write_pds_header("pds_nocomp.hdr", '+', '+', 1, 3, 2, 4);
    let mut pds = bioformats::formats::misc4::PdsReader::new();
    let err = pds.set_id(&pds_no_companion).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("companion .IMG/.img pixel file not found")),
        "{err:?}"
    );

    // PDS: a header missing the required NXP keyword is rejected.
    let pds_no_nxp_path = tmp("pds_no_nxp.hdr");
    std::fs::write(
        &pds_no_nxp_path,
        b" IDENTIFICATION\r\nNYP = 2\r\nFILE REC LEN = 8\r\nEND\r\n".as_slice(),
    )
    .unwrap();
    let mut pds = bioformats::formats::misc4::PdsReader::new();
    let err = pds.set_id(&pds_no_nxp_path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("missing NXP keyword")),
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

    let _ = std::fs::remove_file(&arf_path);
    let _ = std::fs::remove_file(&pds_short_path);
    let _ = std::fs::remove_file(&pds_no_companion);
    let _ = std::fs::remove_file(&pds_no_nxp_path);
    let _ = std::fs::remove_file(&his_path);
}

#[test]
fn misc4_readers_clear_state_after_failed_reopen() {
    let arf_valid = tmp("valid_then_bad.arf");
    let mut arf_data = vec![1u8, 0];
    arf_data.extend_from_slice(b"AR");
    arf_data.extend_from_slice(&1u16.to_le_bytes());
    arf_data.extend_from_slice(&2u16.to_le_bytes());
    arf_data.extend_from_slice(&2u16.to_le_bytes());
    arf_data.extend_from_slice(&8u16.to_le_bytes());
    arf_data.resize(524, 0);
    arf_data.extend_from_slice(&[1, 2, 3, 4]);
    std::fs::write(&arf_valid, arf_data).unwrap();

    let arf_invalid = tmp("bad_reopen.arf");
    std::fs::write(&arf_invalid, [1u8, 0, b'X', b'Y', 0, 0, 0, 0, 0, 0, 8, 0]).unwrap();

    let mut arf = bioformats::formats::misc4::ArfReader::new();
    arf.set_id(&arf_valid).unwrap();
    assert_eq!(arf.series_count(), 1);
    let _ = arf.set_id(&arf_invalid).unwrap_err();
    assert_eq!(arf.series_count(), 0);
    assert_eq!(arf.metadata().size_x, 0);
    assert!(matches!(
        arf.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    // PDS: a valid .hdr+.IMG dataset, then a reopen on a header whose companion
    // is missing must clear state.
    let pds_valid = write_pds_fixture("pds_reopen_ok.hdr", '+', '+', 1, 1, 2, 2, &[10, 20]);
    let pds_invalid = write_pds_header("pds_reopen_bad.hdr", '+', '+', 1, 1, 2, 2);
    let mut pds = bioformats::formats::misc4::PdsReader::new();
    pds.set_id(&pds_valid).unwrap();
    assert_eq!(pds.series_count(), 1);
    let _ = pds.set_id(&pds_invalid).unwrap_err();
    assert_eq!(pds.series_count(), 0);
    assert_eq!(pds.metadata().size_x, 0);

    let his_valid = tmp("valid_then_bad.his");
    let mut his_data = Vec::new();
    append_his_series(&mut his_data, 1, 1, 1, 1, b"", &[7]);
    std::fs::write(&his_valid, his_data).unwrap();
    let his_invalid = tmp("bad_reopen.his");
    std::fs::write(&his_invalid, b"not his").unwrap();
    let mut his = bioformats::formats::misc4::HisReader::new();
    his.set_id(&his_valid).unwrap();
    assert_eq!(his.series_count(), 1);
    let _ = his.set_id(&his_invalid).unwrap_err();
    assert_eq!(his.series_count(), 0);
    assert_eq!(his.metadata().size_x, 0);

    for path in [
        arf_valid,
        arf_invalid,
        pds_valid,
        pds_invalid,
        his_valid,
        his_invalid,
    ] {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn misc4_readers_report_not_initialized_for_preinit_set_series() {
    let mut readers: Vec<Box<dyn FormatReader>> = vec![
        Box::new(bioformats::formats::misc4::ArfReader::new()),
        Box::new(bioformats::formats::misc4::PdsReader::new()),
        Box::new(bioformats::formats::misc4::HisReader::new()),
        Box::new(bioformats::formats::misc4::FilePatternReaderStub::new()),
        // NOTE: I2i/Jdce/Pci/Obf/Apl/Hrdgdf/Klb removed — they are now real
        // readers (see tests/stub_misc4*_test.rs), no longer placeholders.
    ];

    for reader in &mut readers {
        assert_eq!(reader.series_count(), 0);
        assert!(matches!(
            reader.set_series(0),
            Err(BioFormatsError::NotInitialized)
        ));
        assert_eq!(reader.metadata().size_x, 0);
    }
}

#[test]
fn misc4_remaining_placeholders_read_strict_raw_subsets() {
    // I2I/JDCE/PCI/OBF are no longer placeholders (real readers now); only the
    // remaining strict-raw stubs are exercised here.
    let cases: Vec<(&str, Box<dyn FormatReader>, [u8; 8])> = vec![(
        "strict.pattern",
        Box::new(bioformats::formats::misc4::FilePatternReaderStub::new()),
        *b"BFPATT\0\0",
    )];

    for (name, mut reader, magic) in cases {
        let path = tmp(name);
        let plane0 = vec![1u8, 2, 3, 4, 5, 6];
        let plane1 = vec![11u8, 12, 13, 14, 15, 16];
        let mut payload = plane0.clone();
        payload.extend_from_slice(&plane1);
        std::fs::write(&path, strict_misc4_raw_bytes(&magic, 3, 2, 2, 1, &payload)).unwrap();

        assert!(reader.is_this_type_by_bytes(&magic));
        reader.set_id(&path).unwrap();
        assert_eq!(reader.series_count(), 1, "{name}");
        assert_eq!(reader.metadata().size_x, 3, "{name}");
        assert_eq!(reader.metadata().size_y, 2, "{name}");
        assert_eq!(reader.metadata().size_t, 2, "{name}");
        assert_eq!(reader.metadata().image_count, 2, "{name}");
        assert_eq!(reader.metadata().pixel_type, PixelType::Uint8, "{name}");
        assert_eq!(reader.open_bytes(0).unwrap(), plane0, "{name}");
        assert_eq!(reader.open_bytes(1).unwrap(), plane1, "{name}");
        assert_eq!(
            reader.open_bytes_region(1, 1, 0, 2, 2).unwrap(),
            vec![12, 13, 15, 16],
            "{name}"
        );
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));

        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn misc4_strict_raw_subsets_reject_malformed_inputs_before_metadata() {
    // I2I/JDCE/PCI/OBF are no longer placeholders (real readers now); only the
    // remaining strict-raw stubs are exercised here.
    let cases: Vec<(&str, Box<dyn FormatReader>, Vec<u8>, &str)> = vec![(
        "truncated.pattern",
        Box::new(bioformats::formats::misc4::FilePatternReaderStub::new()),
        strict_misc4_raw_bytes(b"BFPATT\0\0", 2, 2, 1, 1, &[1, 2, 3]),
        "payload is truncated",
    )];

    for (name, mut reader, bytes, expected) in cases {
        let path = tmp(name);
        std::fs::write(&path, bytes).unwrap();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains(expected)),
            "{name}: {err:?}"
        );
        assert_eq!(reader.series_count(), 0, "{name}");
        assert_eq!(reader.metadata().size_x, 0, "{name}");
        let _ = std::fs::remove_file(path);
    }
}

// NOTE: misc4_obf_fallback_rejects_imspector_magic was removed — ObfReader is now
// a real OBF reader that DOES claim the OMAS_BF magic (see tests/stub_misc4_test.rs).

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
    // Truncated payload (3 bytes for 8 voxels) is rejected, not zero-padded.
    // The reader now derives bytes-per-voxel as payload/voxels (Java
    // PovrayReader), so a payload too small for even 1 byte/voxel reports a
    // bytes-per-voxel of 0 rather than the old fixed-1-byte payload mismatch.
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("DF3")),
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
fn extended_hamamatsu_vms_validates_index_then_requires_tile_grid() {
    let path = tmp("native.vms");
    std::fs::write(
        &path,
        b"NoLayers=1\nImageFile=tile.jpg\nPhysicalWidth=2\nPhysicalHeight=2\n",
    )
    .unwrap();

    let mut reader = bioformats::formats::extended::HamamatsuVmsReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Hamamatsu VMS missing nojpegcolumns")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);
    assert_eq!(reader.metadata().size_x, 0);

    let _ = std::fs::remove_file(path);
}

fn synthetic_biorad_scn(xml: &str, pixels: &[u8], declared_pixel_len: usize) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"Generated by Image Lab\n");
    data.extend_from_slice(b"Content-Type: multipart/mixed; boundary=\"bf\"\n\n");
    data.extend_from_slice(b"--bf\n");
    data.extend_from_slice(b"Content-Type: text/xml\n");
    data.extend_from_slice(format!("Content-Length: {}\n\n", xml.len()).as_bytes());
    data.extend_from_slice(xml.as_bytes());
    data.extend_from_slice(b"\n--bf\n");
    data.extend_from_slice(b"Content-Type: application/octet-stream\n");
    data.extend_from_slice(format!("Content-Length: {declared_pixel_len}\n\n").as_bytes());
    data.extend_from_slice(pixels);
    data
}

#[test]
fn biorad_scn_validates_dimensions_channels_and_payload() {
    let path = tmp("synthetic.scn");
    let xml = r#"<root><size_pix width="2" height="1"/><scanner max_value="255"/><channel_count>2</channel_count><endian>little</endian></root>"#;
    std::fs::write(&path, synthetic_biorad_scn(xml, &[1, 2, 3, 4], 4)).unwrap();

    let mut reader = bioformats::formats::flim2::BioRadScnReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 1);
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![3, 4]);
    assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(), vec![4]);
    assert!(matches!(
        reader.open_bytes(2),
        Err(BioFormatsError::PlaneOutOfRange(2))
    ));

    let _ = std::fs::remove_file(path);

    let path = tmp("short.scn");
    std::fs::write(&path, synthetic_biorad_scn(xml, &[1, 2, 3], 3)).unwrap();
    let mut reader = bioformats::formats::flim2::BioRadScnReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("pixel payload")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);
    assert_eq!(reader.metadata().size_x, 0);
    let _ = std::fs::remove_file(path);

    let path = tmp("bad_dims.scn");
    let bad_xml =
        r#"<root><size_pix width="0" height="1"/><channel_count>1</channel_count></root>"#;
    std::fs::write(&path, synthetic_biorad_scn(bad_xml, &[1], 1)).unwrap();
    let err = bioformats::formats::flim2::BioRadScnReader::new()
        .set_id(&path)
        .unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("dimensions")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(path);
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
fn ics1_rejects_companion_filename_that_escapes_directory() {
    let ics = tmp("escaped_companion.ics");
    std::fs::write(
        &ics,
        "ics_version\t1.0\nfilename\t../escaped.ids\nlayout\torder\tbits x y\nlayout\tsizes\t8 1 1\nlayout\tsignificant_bits\t8\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t1 2 3 4\nrepresentation\tcompression\tuncompressed\n",
    )
    .unwrap();

    let mut reader = ImageReader::open(&ics).unwrap();
    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        err.to_string().contains("escapes image directory"),
        "unexpected error: {err}"
    );
}

#[test]
fn ics_rejects_malformed_numeric_header_values() {
    for (name, line) in [
        ("bad_size", "layout\tsizes\t8 2 BAD\n"),
        ("bad_bits", "layout\tsignificant_bits\tBAD\n"),
        ("bad_byte_order", "representation\tbyte_order\t1 BAD\n"),
        ("bad_version", "ics_version\tBAD\n"),
    ] {
        let ics = tmp(&format!("ics_{name}.ics"));
        let header = if name == "bad_version" {
            format!(
                "{line}layout\torder\tbits x y\nlayout\tsizes\t8 1 1\nlayout\tsignificant_bits\t8\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t1 2 3 4\nrepresentation\tcompression\tuncompressed\nend\n"
            )
        } else {
            format!(
                "ics_version\t2.0\nlayout\torder\tbits x y\n{line}representation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tcompression\tuncompressed\nend\n"
            )
        };
        std::fs::write(&ics, header).unwrap();

        let err = match ImageReader::open(&ics) {
            Ok(_) => panic!("{name}: malformed ICS header unexpectedly opened"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("ICS invalid numeric value"),
            "{name}: unexpected error: {err}"
        );
    }
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

#[test]
fn ics_non_rgb_channels_are_separate_planes() {
    let ics = tmp("non_rgb_channels.ics");
    let companion = tmp("non_rgb_channels.ids");

    let header = format!(
        "ics_version\t1.0\nfilename\t{}\nlayout\torder\tbits x y ch\nlayout\tsizes\t8 2 1 2\nlayout\tsignificant_bits\t8\nrepresentation\tformat\tinteger\nrepresentation\tsign\tunsigned\nrepresentation\tbyte_order\t1 2 3 4\nrepresentation\tcompression\tuncompressed\n",
        companion.file_name().unwrap().to_string_lossy()
    );
    std::fs::write(&ics, header).unwrap();
    std::fs::write(&companion, [1, 2, 11, 12]).unwrap();

    let mut reader = ImageReader::open(&ics).unwrap();
    assert!(!reader.metadata().is_rgb);
    assert_eq!(reader.metadata().size_c, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![11, 12]);
    assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(), vec![12]);
}

#[test]
fn ics_writer_counts_channel_axis_in_layout_parameters() {
    let path = tmp("ics_rgb_parameters.ics");
    let mut meta = ImageMetadata::default();
    meta.size_x = 1;
    meta.size_y = 1;
    meta.pixel_type = PixelType::Uint8;
    meta.size_c = 3;
    meta.is_rgb = true;
    meta.is_interleaved = true;
    meta.image_count = 1;

    ImageWriter::save(&path, &meta, &[vec![1, 2, 3]]).unwrap();
    let contents = std::fs::read_to_string(&path)
        .unwrap_or_else(|_| String::from_utf8_lossy(&std::fs::read(&path).unwrap()).to_string());

    assert!(contents.contains("layout\tparameters\t4"));
    assert!(contents.contains("layout\torder\tbits x y ch"));
}

#[test]
fn nifti_extra_dimension_channels_are_separate_planes_not_samples() {
    let path = tmp("extra_dim_channels.nii");
    let mut bytes = vec![0u8; 352];
    bytes[0..4].copy_from_slice(&348i32.to_le_bytes());
    bytes[40..42].copy_from_slice(&5u16.to_le_bytes()); // ndim
    bytes[42..44].copy_from_slice(&2u16.to_le_bytes()); // X
    bytes[44..46].copy_from_slice(&1u16.to_le_bytes()); // Y
    bytes[46..48].copy_from_slice(&1u16.to_le_bytes()); // Z
    bytes[48..50].copy_from_slice(&1u16.to_le_bytes()); // T
    bytes[50..52].copy_from_slice(&2u16.to_le_bytes()); // extra dim -> C planes
    bytes[70..72].copy_from_slice(&2i16.to_le_bytes()); // uint8
    bytes[72..74].copy_from_slice(&8i16.to_le_bytes());
    bytes[108..112].copy_from_slice(&352f32.to_le_bytes());
    bytes[344..348].copy_from_slice(b"n+1\0");
    bytes.extend_from_slice(&[1, 2, 3, 4]);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();

    assert_eq!(reader.metadata().size_c, 2);
    assert!(!reader.metadata().is_rgb);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![3, 4]);
}

#[test]
fn nifti_rejects_invalid_or_non_positive_dimensions() {
    for (name, dims, expected) in [
        (
            "bad_ndim",
            [8i16, 1, 1, 1, 1, 1, 1, 1],
            "invalid dimension count",
        ),
        ("zero_x", [2i16, 0, 1, 1, 1, 1, 1, 1], "non-positive SizeX"),
        (
            "negative_y",
            [2i16, 1, -1, 1, 1, 1, 1, 1],
            "non-positive SizeY",
        ),
    ] {
        let path = tmp(&format!("nifti_{name}.nii"));
        let mut bytes = vec![0u8; 352];
        bytes[0..4].copy_from_slice(&348i32.to_le_bytes());
        for (i, dim) in dims.iter().enumerate() {
            bytes[40 + i * 2..42 + i * 2].copy_from_slice(&dim.to_le_bytes());
        }
        bytes[70..72].copy_from_slice(&2i16.to_le_bytes());
        bytes[72..74].copy_from_slice(&8i16.to_le_bytes());
        bytes[108..112].copy_from_slice(&352f32.to_le_bytes());
        bytes[344..348].copy_from_slice(b"n+1\0");
        std::fs::write(&path, bytes).unwrap();

        let err = match ImageReader::open(&path) {
            Ok(_) => panic!("{name}: malformed NIfTI unexpectedly opened"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains(expected),
            "{name}: unexpected error: {err}"
        );
    }
}

#[test]
fn viff_rejects_non_positive_counts_and_short_payload() {
    let mut uninit = bioformats::formats::viff::ViffReader::new();
    assert_eq!(uninit.series_count(), 0);
    assert!(matches!(
        uninit.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));

    let zero_channels = tmp("zero_channels.viff");
    let mut bytes = vec![0u8; 1024];
    bytes[..2].copy_from_slice(&[0xab, 0x01]);
    write_i32_be(&mut bytes, 520, 1);
    write_i32_be(&mut bytes, 524, 1);
    write_i32_be(&mut bytes, 556, 1);
    write_i32_be(&mut bytes, 560, 0);
    write_i32_be(&mut bytes, 564, 1);
    bytes.push(7);
    std::fs::write(&zero_channels, bytes).unwrap();
    let err = match ImageReader::open(&zero_channels) {
        Ok(_) => panic!("VIFF with zero channels unexpectedly opened"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("non-positive channel count"));
    let _ = std::fs::remove_file(zero_channels);

    let short = tmp("short.viff");
    let mut bytes = vec![0u8; 1024];
    bytes[..2].copy_from_slice(&[0xab, 0x01]);
    write_i32_be(&mut bytes, 520, 2);
    write_i32_be(&mut bytes, 524, 2);
    write_i32_be(&mut bytes, 556, 1);
    write_i32_be(&mut bytes, 560, 1);
    write_i32_be(&mut bytes, 564, 1);
    bytes.extend_from_slice(&[1, 2, 3]);
    std::fs::write(&short, bytes).unwrap();
    let err = match ImageReader::open(&short) {
        Ok(_) => panic!("short VIFF unexpectedly opened"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("shorter than declared"));
    let _ = std::fs::remove_file(short);
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

#[test]
fn mrc_rejects_non_positive_z_and_short_payload_before_metadata() {
    let zero_z = tmp("zero_z.mrc");
    let mut bytes = vec![0u8; 1024];
    write_i32_le(&mut bytes, 0, 2);
    write_i32_le(&mut bytes, 4, 2);
    write_i32_le(&mut bytes, 8, 0);
    write_i32_le(&mut bytes, 12, 0);
    write_i32_le(&mut bytes, 28, 2);
    write_i32_le(&mut bytes, 32, 2);
    write_i32_le(&mut bytes, 36, 1);
    write_i32_le(&mut bytes, 64, 1);
    write_i32_le(&mut bytes, 68, 2);
    write_i32_le(&mut bytes, 72, 3);
    bytes[208..212].copy_from_slice(b"MAP ");
    std::fs::write(&zero_z, &bytes).unwrap();
    let err = match ImageReader::open(&zero_z) {
        Ok(_) => panic!("zero-Z MRC unexpectedly opened"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("dimensions must be positive"));
    let _ = std::fs::remove_file(&zero_z);

    let short = tmp("short_payload.mrc");
    write_i32_le(&mut bytes, 8, 1);
    bytes.truncate(1024);
    bytes.extend_from_slice(&[1, 2, 3]);
    std::fs::write(&short, bytes).unwrap();
    let err = match ImageReader::open(&short) {
        Ok(_) => panic!("short MRC unexpectedly opened"),
        Err(err) => err,
    };
    assert!(err.to_string().contains("shorter than declared"));
    let _ = std::fs::remove_file(short);
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

#[test]
fn metaimage_rejects_malformed_dimensions() {
    for (name, header) in [
        (
            "bad_ndims",
            "ObjectType = Image\nNDims = bad\nDimSize = 1 1 1\nElementType = MET_UCHAR\nElementDataFile = LOCAL\n",
        ),
        (
            "bad_dimsize",
            "ObjectType = Image\nNDims = 3\nDimSize = 1 nope 1\nElementType = MET_UCHAR\nElementDataFile = LOCAL\n",
        ),
        (
            "short_dimsize",
            "ObjectType = Image\nNDims = 3\nDimSize = 1 1\nElementType = MET_UCHAR\nElementDataFile = LOCAL\n",
        ),
        (
            "zero_dimsize",
            "ObjectType = Image\nNDims = 3\nDimSize = 1 0 1\nElementType = MET_UCHAR\nElementDataFile = LOCAL\n",
        ),
    ] {
        let path = tmp(&format!("metaimage_{name}.mha"));
        std::fs::write(&path, header).unwrap();

        let err = match ImageReader::open(&path) {
            Ok(_) => panic!("{name}: malformed MetaImage header unexpectedly opened"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("MetaImage:"),
            "{name}: unexpected error: {err}"
        );
    }
}

#[test]
fn metaimage_rejects_detached_data_file_that_escapes_directory() {
    for (name, data_file) in [
        ("relative_escape", "../outside.raw".to_string()),
        (
            "absolute_escape",
            std::env::temp_dir()
                .join("outside.raw")
                .display()
                .to_string(),
        ),
    ] {
        let path = tmp(&format!("metaimage_{name}.mhd"));
        let header = format!(
            "ObjectType = Image\nNDims = 2\nDimSize = 1 1\nElementType = MET_UCHAR\nElementDataFile = {data_file}\n"
        );
        std::fs::write(&path, header).unwrap();

        let mut reader = ImageReader::open(&path).unwrap();
        let err = reader.open_bytes(0).unwrap_err();
        assert!(
            err.to_string().contains("ElementDataFile escapes"),
            "{name}: unexpected error: {err}"
        );
    }
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
fn ome_xml_name_detection_accepts_ome_xml_suffix() {
    let path = tmp("suffix.ome.xml");
    let reader = bioformats::formats::ome::OmeXmlReader::new();
    assert!(reader.is_this_type_by_name(&path));
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
fn ome_xml_rejects_multiple_logical_rgb_channels() {
    let path = tmp("multi_logical_rgb.ome");
    let xml = r#"<?xml version="1.0" encoding="UTF-8"?>
<OME><Image ID="Image:0"><Pixels ID="Pixels:0" DimensionOrder="XYZCT" Type="uint8" SizeX="1" SizeY="1" SizeZ="1" SizeC="6" SizeT="1"><Channel ID="Channel:0:0" SamplesPerPixel="3"/><Channel ID="Channel:0:1" SamplesPerPixel="3"/></Pixels></Image></OME>"#;
    std::fs::write(&path, xml).unwrap();

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("multi-logical-channel RGB OME-XML unexpectedly opened"),
        Err(err) => err,
    };
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("multiple logical RGB channels")),
        "{err:?}"
    );
}

#[test]
fn ome_xml_attribute_matching_does_not_confuse_physical_size_with_size() {
    let path = tmp("physical_size_before_size.ome");
    std::fs::write(
        &path,
        r#"<OME><Image><Pixels PhysicalSizeX="0.25" PhysicalSizeY="0.5" SizeX="512" SizeY="128" SizeZ="1" SizeC="1" SizeT="1" Type="uint8" DimensionOrder="XYZCT"/></Image></OME>"#,
    )
    .unwrap();

    let mut reader = bioformats::formats::ome::OmeXmlReader::new();
    reader.set_id(&path).unwrap();

    assert_eq!(reader.metadata().size_x, 512);
    assert_eq!(reader.metadata().size_y, 128);
}

#[test]
fn ome_xml_rejects_fake_dimensions_unknown_metadata_and_short_bindata() {
    let cases = [
        (
            "zero_size_x.ome",
            r#"<OME><Image><Pixels DimensionOrder="XYZCT" Type="uint8" SizeX="0" SizeY="1" SizeZ="1" SizeC="1" SizeT="1"><BinData Length="1">AA==</BinData></Pixels></Image></OME>"#,
            "SizeX must be positive",
        ),
        (
            "unknown_type.ome",
            r#"<OME><Image><Pixels DimensionOrder="XYZCT" Type="mystery" SizeX="1" SizeY="1" SizeZ="1" SizeC="1" SizeT="1"><BinData Length="1">AA==</BinData></Pixels></Image></OME>"#,
            "unsupported Type mystery",
        ),
        (
            "unknown_order.ome",
            r#"<OME><Image><Pixels DimensionOrder="XYBAD" Type="uint8" SizeX="1" SizeY="1" SizeZ="1" SizeC="1" SizeT="1"><BinData Length="1">AA==</BinData></Pixels></Image></OME>"#,
            "unsupported DimensionOrder XYBAD",
        ),
        (
            "short_bindata.ome",
            r#"<OME><Image><Pixels DimensionOrder="XYZCT" Type="uint8" SizeX="2" SizeY="1" SizeZ="1" SizeC="1" SizeT="1"><BinData Length="2">AA==</BinData></Pixels></Image></OME>"#,
            "pixel payload is shorter",
        ),
    ];

    for (name, xml, expected) in cases {
        let path = tmp(name);
        std::fs::write(&path, xml).unwrap();
        let mut reader = bioformats::formats::ome::OmeXmlReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            err.to_string().contains(expected),
            "{name}: unexpected error: {err}"
        );
        assert_eq!(reader.series_count(), 0);
    }
}

#[test]
fn ome_xml_rejects_missing_explicit_companion_before_metadata() {
    let path = tmp("missing_companion.ome");
    let xml = r#"<OME><Image><Pixels DimensionOrder="XYZCT" Type="uint8" SizeX="1" SizeY="1" SizeZ="1" SizeC="1" SizeT="1"><TiffData IFD="0"><UUID FileName="missing_pixels.tif">urn:uuid:test</UUID></TiffData></Pixels></Image></OME>"#;
    std::fs::write(&path, xml).unwrap();

    let mut reader = bioformats::formats::ome::OmeXmlReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        err.to_string().contains("companion TIFF not found"),
        "unexpected OME-XML companion error: {err}"
    );
    assert_eq!(reader.series_count(), 0);
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
fn dicom_series_requires_successful_initialization() {
    let mut reader = bioformats::formats::dicom::DicomReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));
}

#[test]
fn dicom_rejects_missing_required_pixel_attributes() {
    // Note: missing SamplesPerPixel (0028,0002) is NOT rejected — it defaults to
    // 1 per the DICOM standard (old ACR-NEMA / implicit-VR files omit it). Only
    // genuinely-required-and-invalid attributes (bits) are rejected here.
    for (name, omit_samples, omit_bits) in [
        ("bits_allocated", false, true),
        ("bits_stored_too_large", false, false),
    ] {
        let path = tmp(&format!("dicom_missing_required_{name}.dcm"));
        let mut bytes = Vec::new();
        if !omit_samples {
            dicom_elem_explicit(&mut bytes, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
        }
        dicom_elem_explicit(&mut bytes, 0x0028, 0x0010, b"US", &1u16.to_le_bytes());
        dicom_elem_explicit(&mut bytes, 0x0028, 0x0011, b"US", &1u16.to_le_bytes());
        if !omit_bits {
            dicom_elem_explicit(&mut bytes, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
        }
        let bits_stored = if name == "bits_stored_too_large" {
            16u16
        } else {
            8u16
        };
        dicom_elem_explicit(
            &mut bytes,
            0x0028,
            0x0101,
            b"US",
            &bits_stored.to_le_bytes(),
        );
        dicom_elem_explicit(&mut bytes, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
        dicom_elem_explicit(&mut bytes, 0x7FE0, 0x0010, b"OB", &[1]);
        std::fs::write(&path, bytes).unwrap();

        let err = match ImageReader::open(&path) {
            Ok(_) => panic!("{name}: DICOM with invalid pixel metadata should be rejected"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("SamplesPerPixel")
                || err.to_string().contains("BitsAllocated")
                || err.to_string().contains("BitsStored"),
            "{name}: unexpected DICOM error: {err}"
        );
    }
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
fn dicom_rejects_invalid_or_zero_number_of_frames() {
    for (name, value) in [("invalid", b"abc".as_slice()), ("zero", b"0".as_slice())] {
        let path = tmp(&format!("dicom_bad_number_of_frames_{name}.dcm"));
        let mut bytes = Vec::new();

        dicom_elem_explicit(&mut bytes, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
        dicom_elem_explicit(&mut bytes, 0x0028, 0x0008, b"IS", value);
        dicom_elem_explicit(&mut bytes, 0x0028, 0x0010, b"US", &2u16.to_le_bytes());
        dicom_elem_explicit(&mut bytes, 0x0028, 0x0011, b"US", &2u16.to_le_bytes());
        dicom_elem_explicit(&mut bytes, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
        dicom_elem_explicit(&mut bytes, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
        dicom_elem_explicit(&mut bytes, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
        dicom_elem_explicit(&mut bytes, 0x7FE0, 0x0010, b"OB", &[1, 2, 3, 4]);
        std::fs::write(&path, bytes).unwrap();

        let err = match ImageReader::open(&path) {
            Ok(_) => panic!("{name}: malformed NumberOfFrames unexpectedly opened"),
            Err(err) => err,
        };
        assert!(
            err.to_string().contains("NumberOfFrames"),
            "{name}: unexpected error: {err}"
        );
    }
}

#[test]
fn dicom_accepts_valid_number_of_frames() {
    let path = tmp("dicom_two_number_of_frames.dcm");
    let mut bytes = Vec::new();

    dicom_elem_explicit(&mut bytes, 0x0028, 0x0002, b"US", &1u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0008, b"IS", b"2");
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0010, b"US", &2u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0011, b"US", &2u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0100, b"US", &8u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0101, b"US", &8u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x0028, 0x0103, b"US", &0u16.to_le_bytes());
    dicom_elem_explicit(&mut bytes, 0x7FE0, 0x0010, b"OB", &[1, 2, 3, 4, 5, 6, 7, 8]);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.metadata().size_z, 2);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![5, 6, 7, 8]);
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
    // SliceThickness (0018,0050) is NOT used by Java for the physical Z size;
    // SpacingBetweenSlices (0018,0088) drives DicomReader.pixelSizeZ instead.
    dicom_elem_implicit(&mut bytes, 0x0018, 0x0050, b"0.75");
    dicom_elem_implicit(&mut bytes, 0x0018, 0x0088, b"0.75");
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
    // Java DicomReader derives the image name from (0008,0008) ImageType, falling
    // back to the file name when ImageType is absent (it does NOT use PatientName).
    assert_eq!(
        image.name.as_deref(),
        Some("bioformats_fmt_metadata_dictionary.dcm")
    );
    // PixelSpacing/SpacingBetweenSlices are stored as raw millimetre OME Length
    // values to match Java (FormatTools.getPhysicalSizeX(value, UNITS.MILLIMETER)),
    // not rescaled to micrometres. PixelSpacing is "row\col" so X=col=0.25,
    // Y=row=0.5; physical Z comes from SpacingBetweenSlices (0018,0088) = 0.75.
    assert_eq!(image.physical_size_x, Some(0.25));
    assert_eq!(image.physical_size_y, Some(0.5));
    assert_eq!(image.physical_size_z, Some(0.75));
}

/// Build a CellH5 file whose canonical experiment layout
/// `/sample/0/plate/Plate0/experiment/A01/position/1/image/channel` ends in an
/// `image/channel` dataset configured by `build_channel`. The builder must
/// consume the `DatasetBuilder` (call `.write::<T>(..)`).
fn build_cellh5_channel<F>(path: &Path, build_channel: F)
where
    F: for<'b> FnOnce(hdf5_pure_rust::DatasetBuilder<'b>),
{
    let mut file = hdf5_pure_rust::WritableFile::create(path).unwrap();
    {
        let mut sample = file.create_group("sample").unwrap();
        let mut zero = sample.create_group("0").unwrap();
        let mut plate = zero.create_group("plate").unwrap();
        let mut plate0 = plate.create_group("Plate0").unwrap();
        let mut experiment = plate0.create_group("experiment").unwrap();
        let mut well = experiment.create_group("A01").unwrap();
        let mut positions = well.create_group("position").unwrap();
        let mut site = positions.create_group("1").unwrap();
        let mut image = site.create_group("image").unwrap();
        build_channel(image.new_dataset_builder("channel"));
    }
    file.flush().unwrap();
}

#[test]
fn bdv_preserves_companion_xml_original_metadata() {
    let path = tmp("metadata_parity_bdv.h5");
    let xml_path = path.with_extension("xml");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&xml_path);

    // <first>2</first> means the first timepoint group is named per Java's
    // t%05d(firstTimepoint + increment*t) = t00002 (BDVReader.java:431), so the
    // fixture's pixel group must be t00002 to be consistent with its own XML.
    let mut file = hdf5_pure_rust::WritableFile::create(&path).unwrap();
    {
        let mut t0 = file.create_group("t00002").unwrap();
        let mut s0 = t0.create_group("s00").unwrap();
        let mut level0 = s0.create_group("0").unwrap();
        level0
            .new_dataset_builder("cells")
            .shape(&[1, 2, 3])
            .write::<u16>(&[1u16, 2, 3, 4, 5, 6])
            .unwrap();
    }
    {
        let mut setup0 = file.create_group("s00").unwrap();
        setup0
            .new_dataset_builder("resolutions")
            .shape(&[1, 3])
            .write::<f64>(&[1.0f64, 1.0, 1.0])
            .unwrap();
    }
    file.flush().unwrap();

    let xml = r#"<SpimData>
  <SequenceDescription>
    <ViewSetups>
      <ViewSetup>
        <id>0</id>
        <name>sample angle A</name>
        <size>3 2 1</size>
        <voxelSize><unit>micrometer</unit><size>0.5 0.6 1.25</size></voxelSize>
      </ViewSetup>
      <ViewSetup><id>1</id><size>3 2 1</size></ViewSetup>
    </ViewSetups>
    <Timepoints type="range"><first>2</first><last>4</last></Timepoints>
  </SequenceDescription>
</SpimData>"#;
    std::fs::write(&xml_path, xml).unwrap();

    let mut reader = bioformats::formats::bdv::BdvReader::new();
    reader.set_id(&path).unwrap();

    // Java parity: each (setup × timepoint × level) becomes its own series.
    // The XML declares 2 setups and timepoints 2..=4 (3 timepoints), but the
    // HDF5 fixture only contains pixels for setup 0 at t00002 level 0, so only
    // one series is actually readable. Each series is single-channel,
    // single-timepoint, with dims taken from the cells dataset shape.
    assert_eq!(reader.series_count(), 1);
    let metadata = &reader.metadata().series_metadata;
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_z, 1);
    assert_eq!(reader.metadata().size_c, 1);
    assert_eq!(reader.metadata().size_t, 1);
    assert_eq!(reader.metadata().image_count, 1);
    assert!(matches!(
        metadata.get("bdv_setup"),
        Some(MetadataValue::Int(0))
    ));
    assert!(matches!(
        metadata.get("bdv_timepoint"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        metadata.get("bdv_level"),
        Some(MetadataValue::Int(0))
    ));
    assert!(matches!(
        metadata.get("bdv_view_setup_name"),
        Some(MetadataValue::String(value)) if value == "sample angle A"
    ));
    assert!(matches!(
        metadata.get("bdv_voxel_unit"),
        Some(MetadataValue::String(value)) if value == "micrometer"
    ));
    assert!(matches!(
        metadata.get("bdv_voxel_size_x"),
        Some(MetadataValue::Float(value)) if *value == 0.5
    ));
    assert!(matches!(
        metadata.get("bdv_voxel_size_y"),
        Some(MetadataValue::Float(value)) if *value == 0.6
    ));
    assert!(matches!(
        metadata.get("bdv_voxel_size_z"),
        Some(MetadataValue::Float(value)) if *value == 1.25
    ));
    let ome = reader.ome_metadata().expect("OME metadata");
    assert_eq!(ome.images[0].physical_size_x, Some(0.5));
    assert_eq!(ome.images[0].physical_size_y, Some(0.6));
    assert_eq!(ome.images[0].physical_size_z, Some(1.25));
    let original = ome
        .annotations
        .iter()
        .find_map(|annotation| match annotation {
            OmeAnnotation::MapAnnotation {
                id: Some(id),
                values,
                ..
            } if id == "Annotation:OriginalMetadata:0" => Some(values),
            _ => None,
        })
        .expect("BDV original metadata annotation");
    assert!(original
        .iter()
        .any(|(key, value)| key == "bdv_timepoint" && value == "2"));
    assert!(original
        .iter()
        .any(|(key, value)| key == "bdv_view_setup_name" && value == "sample angle A"));
    assert!(original
        .iter()
        .any(|(key, value)| key == "bdv_voxel_size_z" && value == "1.25"));
    let err = reader.open_bytes_region(0, 2, 0, 2, 1).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("outside image bounds")),
        "{err:?}"
    );
}

#[test]
fn bdv_derives_dimensions_from_cells_dataset_shape() {
    // Java parity: core dimensions come from the {level}/cells dataset shape
    // [z, y, x], not from the companion XML <size>. Here the XML claims a
    // 2x1x1 setup but the actual cells dataset is 1x1x1, and the reader must
    // expose the dataset's real shape with a single readable plane.
    let path = tmp("short_bdv.h5");
    let xml_path = path.with_extension("xml");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&xml_path);

    let mut file = hdf5_pure_rust::WritableFile::create(&path).unwrap();
    {
        let mut t0 = file.create_group("t00000").unwrap();
        let mut s0 = t0.create_group("s00").unwrap();
        let mut level0 = s0.create_group("0").unwrap();
        level0
            .new_dataset_builder("cells")
            .shape(&[1, 1, 1])
            .write::<u16>(&[7u16])
            .unwrap();
    }
    {
        let mut setup0 = file.create_group("s00").unwrap();
        setup0
            .new_dataset_builder("resolutions")
            .shape(&[1, 3])
            .write::<f64>(&[1.0f64, 1.0, 1.0])
            .unwrap();
    }
    file.flush().unwrap();

    std::fs::write(
        &xml_path,
        r#"<SpimData><SequenceDescription><ViewSetups><ViewSetup><id>0</id><size>2 1 1</size></ViewSetup></ViewSetups></SequenceDescription></SpimData>"#,
    )
    .unwrap();

    let mut reader = bioformats::formats::bdv::BdvReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(
        (
            reader.metadata().size_x,
            reader.metadata().size_y,
            reader.metadata().size_z
        ),
        (1, 1, 1)
    );
    assert_eq!(reader.open_bytes(0).unwrap(), 7u16.to_le_bytes().to_vec());
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&xml_path);
}

#[test]
fn bdv_requires_real_dimensions_and_initialized_series() {
    let path = tmp("weak_bdv.h5");
    let xml_path = path.with_extension("xml");
    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&xml_path);

    let mut uninit = bioformats::formats::bdv::BdvReader::new();
    assert_eq!(uninit.series_count(), 0);
    assert_eq!(uninit.resolution_count(), 0);
    assert!(matches!(
        uninit.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    assert!(matches!(
        uninit.set_resolution(0),
        Err(BioFormatsError::NotInitialized)
    ));

    let mut wf = hdf5_pure_rust::WritableFile::create(&path).unwrap();
    wf.flush().unwrap();
    drop(wf);
    // No companion XML and no sNN groups in the HDF5 → no setups to enumerate.
    let err = uninit.set_id(&path).unwrap_err();
    assert!(
        err.to_string()
            .contains("no ViewSetups / setup groups found"),
        "unexpected BDV error: {err}"
    );
    assert_eq!(uninit.series_count(), 0);

    // A ViewSetup without an <id> cannot be mapped to an sNN group, so it is
    // ignored; with no usable setups, set_id still fails (no series).
    std::fs::write(
        &xml_path,
        r#"<SpimData><SequenceDescription><ViewSetups><ViewSetup><size>0 2 1</size></ViewSetup></ViewSetups></SequenceDescription></SpimData>"#,
    )
    .unwrap();
    let err = uninit.set_id(&path).unwrap_err();
    assert!(
        err.to_string()
            .contains("no ViewSetups / setup groups found"),
        "unexpected BDV error: {err}"
    );

    let _ = std::fs::remove_file(&path);
    let _ = std::fs::remove_file(&xml_path);
}

#[test]
fn imaris_derives_dimensions_from_dataset_shape_not_attributes() {
    // Real Imaris files can carry bogus DataSetInfo/Image X/Y/Z attributes
    // (e.g. 1/1/1, observed in public samples); the authoritative pixel
    // dimensions are the Data dataset shape [z, y, x]. Here the attributes say
    // 1x1x1 but the Data is 4x3, and the reader must use the shape.
    let path = tmp("shape_dims_ims.ims");
    let _ = std::fs::remove_file(&path);

    let mut file = hdf5_pure_rust::WritableFile::create(&path).unwrap();
    {
        let mut info = file.create_group("DataSetInfo").unwrap();
        let mut image = info.create_group("Image").unwrap();
        image.add_fixed_ascii_attr("X", "1", 1).unwrap();
        image.add_fixed_ascii_attr("Y", "1", 1).unwrap();
        image.add_fixed_ascii_attr("Z", "1", 1).unwrap();
    }
    {
        let mut dataset = file.create_group("DataSet").unwrap();
        let mut res = dataset.create_group("ResolutionLevel 0").unwrap();
        let mut time = res.create_group("TimePoint 0").unwrap();
        let mut channel = time.create_group("Channel 0").unwrap();
        channel
            .new_dataset_builder("Data")
            .shape(&[1, 3, 4]) // z=1, y=3, x=4
            .write::<u8>(&[1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
            .unwrap();
    }
    file.flush().unwrap();

    let mut reader = bioformats::formats::imaris::ImarisReader::new();
    reader.set_id(&path).unwrap();
    let m = reader.metadata();
    assert_eq!((m.size_x, m.size_y, m.size_z, m.size_c), (4, 3, 1, 1));
    assert_eq!(reader.open_bytes(0).unwrap().len(), 12);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn imaris_reads_java_style_underscore_layout() {
    let path = tmp("underscore_layout_ims.ims");
    let _ = std::fs::remove_file(&path);

    let mut file = hdf5_pure_rust::WritableFile::create(&path).unwrap();
    {
        let mut info = file.create_group("DataSetInfo").unwrap();
        let mut image = info.create_group("Image").unwrap();
        image.add_fixed_ascii_attr("X", "99", 8).unwrap();
        image.add_fixed_ascii_attr("Y", "99", 8).unwrap();
        image.add_fixed_ascii_attr("Z", "99", 8).unwrap();
        let mut channel_info = info.create_group("Channel_0").unwrap();
        channel_info
            .add_fixed_ascii_attr("Name", "underscore channel", 32)
            .unwrap();
    }
    {
        let mut dataset = file.create_group("DataSet").unwrap();
        let mut res = dataset.create_group("ResolutionLevel_0").unwrap();
        let mut time = res.create_group("TimePoint_0").unwrap();
        let mut channel = time.create_group("Channel_0").unwrap();
        channel
            .new_dataset_builder("Data")
            .shape(&[1, 2, 3])
            .write::<u8>(&[10u8, 11, 12, 13, 14, 15])
            .unwrap();
    }
    file.flush().unwrap();

    let mut reader = bioformats::formats::imaris::ImarisReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(
        (meta.size_x, meta.size_y, meta.size_z, meta.size_c),
        (3, 2, 1, 1)
    );
    assert!(matches!(
        meta.series_metadata.get("imaris.channel.0.Name"),
        Some(MetadataValue::String(v)) if v == "underscore channel"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10, 11, 12, 13, 14, 15]);

    let ome = reader.ome_metadata().unwrap();
    assert_eq!(
        ome.images[0].channels[0].name.as_deref(),
        Some("underscore channel")
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn imaris_preserves_recording_spacing_and_dataset_metadata() {
    let path = tmp("rich_metadata_ims.ims");
    let _ = std::fs::remove_file(&path);

    let mut file = hdf5_pure_rust::WritableFile::create(&path).unwrap();
    {
        let mut info = file.create_group("DataSetInfo").unwrap();
        let mut image = info.create_group("Image").unwrap();
        image.add_fixed_ascii_attr("X", "1", 8).unwrap();
        image.add_fixed_ascii_attr("Y", "1", 8).unwrap();
        image.add_fixed_ascii_attr("Z", "1", 8).unwrap();
        image
            .add_fixed_ascii_attr("RecordingEntrySampleSpacing", "0.25", 16)
            .unwrap();
        image
            .add_fixed_ascii_attr("RecordingEntryLineSpacing", "0.5", 16)
            .unwrap();
        image
            .add_fixed_ascii_attr("RecordingEntryPlaneSpacing", "1", 16)
            .unwrap();
        image.add_fixed_ascii_attr("ExtMin0", "0", 16).unwrap();
        image.add_fixed_ascii_attr("ExtMin1", "0", 16).unwrap();
        image.add_fixed_ascii_attr("ExtMin2", "0", 16).unwrap();
        image.add_fixed_ascii_attr("ExtMax0", "10", 16).unwrap();
        image.add_fixed_ascii_attr("ExtMax1", "20", 16).unwrap();
        image.add_fixed_ascii_attr("ExtMax2", "6", 16).unwrap();
        image
            .add_fixed_ascii_attr("Description", "synthetic Imaris notes", 32)
            .unwrap();

        let mut imaris = info.create_group("Imaris").unwrap();
        imaris
            .add_fixed_ascii_attr("Version", "9.9.0-test", 32)
            .unwrap();
        let mut microscope = info.create_group("Microscope").unwrap();
        microscope
            .add_fixed_ascii_attr("Model", "Imaris Scope", 32)
            .unwrap();
        microscope
            .add_fixed_ascii_attr("Manufacturer", "Bitplane", 32)
            .unwrap();
        let mut objective = info.create_group("Objective_0").unwrap();
        objective
            .add_fixed_ascii_attr("Name", "Plan-Apochromat 63x", 32)
            .unwrap();
        objective
            .add_fixed_ascii_attr("Manufacturer", "Zeiss", 32)
            .unwrap();
        objective
            .add_fixed_ascii_attr("Magnification", "63", 16)
            .unwrap();
        objective
            .add_fixed_ascii_attr("NumericalAperture", "1.4", 16)
            .unwrap();
        objective
            .add_fixed_ascii_attr("Immersion", "Oil", 16)
            .unwrap();
        objective
            .add_fixed_ascii_attr("WorkingDistance", "190", 16)
            .unwrap();
        let mut detector = info.create_group("Detector").unwrap();
        detector.add_fixed_ascii_attr("Name", "HyD 1", 16).unwrap();
        detector.add_fixed_ascii_attr("Type", "PMT", 16).unwrap();
        detector.add_fixed_ascii_attr("Gain", "2.5", 16).unwrap();
        detector.add_fixed_ascii_attr("Offset", "1.5", 16).unwrap();
        let mut laser = info.create_group("Laser_0").unwrap();
        laser.add_fixed_ascii_attr("Name", "Argon 488", 16).unwrap();
        laser.add_fixed_ascii_attr("Power", "12.5", 16).unwrap();
        let mut time = info.create_group("TimeInfo").unwrap();
        time.add_attr("FileTimePoints", 1i64).unwrap();
        let mut channel_info = info.create_group("Channel 0").unwrap();
        channel_info
            .add_fixed_ascii_attr("Name", "DAPI", 16)
            .unwrap();
        channel_info
            .add_fixed_ascii_attr("Color", "0.1 0.2 0.3", 32)
            .unwrap();
        channel_info
            .add_fixed_ascii_attr("LSMEmissionWavelength", "520", 16)
            .unwrap();
        channel_info
            .add_fixed_ascii_attr("LSMExcitationWavelength", "405", 16)
            .unwrap();
        channel_info.add_attr("Gain", 7i64).unwrap();
    }
    {
        let mut dataset = file.create_group("DataSet").unwrap();
        let mut res = dataset.create_group("ResolutionLevel 0").unwrap();
        let mut time = res.create_group("TimePoint 0").unwrap();
        let mut channel = time.create_group("Channel 0").unwrap();
        channel
            .new_dataset_builder("Data")
            .shape(&[2, 2, 2])
            .write::<u8>(&[1u8, 2, 3, 4, 5, 6, 7, 8])
            .unwrap();
    }
    file.flush().unwrap();

    let mut reader = bioformats::formats::imaris::ImarisReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_z, 2);
    assert!(matches!(
        meta.series_metadata.get("imaris.recording_spacing_x"),
        Some(MetadataValue::Float(v)) if *v == 0.25
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.recording_spacing_y"),
        Some(MetadataValue::Float(v)) if *v == 0.5
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.channel.0.Gain"),
        Some(MetadataValue::Int(7))
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.channel.0.color.red"),
        Some(MetadataValue::Int(26))
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.channel.0.color.green"),
        Some(MetadataValue::Int(51))
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.channel.0.color.blue"),
        Some(MetadataValue::Int(77))
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.channel.0.color.alpha"),
        Some(MetadataValue::Int(255))
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.info.Version"),
        Some(MetadataValue::String(v)) if v == "9.9.0-test"
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.microscope.Model"),
        Some(MetadataValue::String(v)) if v == "Imaris Scope"
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.objective.NumericalAperture"),
        Some(MetadataValue::String(v)) if v == "1.4"
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.detector.Gain"),
        Some(MetadataValue::String(v)) if v == "2.5"
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.light_source.Power"),
        Some(MetadataValue::String(v)) if v == "12.5"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.channel.0.emission_wavelength"),
        Some(MetadataValue::Float(v)) if *v == 520.0
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.channel.0.excitation_wavelength"),
        Some(MetadataValue::Float(v)) if *v == 405.0
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.dataset.0.shape"),
        Some(MetadataValue::String(v)) if v == "2 2 2"
    ));
    assert!(meta
        .series_metadata
        .contains_key("imaris.dataset.0.layout_class"));

    let ome = reader.ome_metadata().unwrap();
    let image = &ome.images[0];
    assert_eq!(image.physical_size_x, Some(0.25));
    assert_eq!(image.physical_size_y, Some(0.5));
    assert_eq!(image.physical_size_z, Some(3.0));
    assert_eq!(image.description.as_deref(), Some("synthetic Imaris notes"));
    assert_eq!(image.channels[0].name.as_deref(), Some("DAPI"));
    assert_eq!(
        image.channels[0].color,
        Some(u32::from_be_bytes([26, 51, 77, 255]) as i32)
    );
    assert_eq!(image.channels[0].emission_wavelength, Some(520.0));
    assert_eq!(image.channels[0].excitation_wavelength, Some(405.0));
    assert_eq!(image.instrument_ref, Some(0));
    assert_eq!(image.objective_ref, Some(0));
    assert_eq!(ome.instruments.len(), 1);
    let instrument = &ome.instruments[0];
    assert_eq!(instrument.microscope_model.as_deref(), Some("Imaris Scope"));
    assert_eq!(
        instrument.microscope_manufacturer.as_deref(),
        Some("Bitplane")
    );
    assert_eq!(instrument.objectives.len(), 1);
    assert_eq!(
        instrument.objectives[0].model.as_deref(),
        Some("Plan-Apochromat 63x")
    );
    assert_eq!(
        instrument.objectives[0].manufacturer.as_deref(),
        Some("Zeiss")
    );
    assert_eq!(instrument.objectives[0].nominal_magnification, Some(63.0));
    assert_eq!(instrument.objectives[0].lens_na, Some(1.4));
    assert_eq!(instrument.objectives[0].immersion.as_deref(), Some("Oil"));
    assert_eq!(instrument.objectives[0].working_distance, Some(190.0));
    assert_eq!(instrument.detectors.len(), 1);
    assert_eq!(instrument.detectors[0].model.as_deref(), Some("HyD 1"));
    assert_eq!(
        instrument.detectors[0].detector_type.as_deref(),
        Some("PMT")
    );
    assert_eq!(instrument.detectors[0].gain, Some(2.5));
    assert_eq!(instrument.detectors[0].offset, Some(1.5));
    assert_eq!(instrument.light_sources.len(), 1);
    assert_eq!(
        instrument.light_sources[0].model.as_deref(),
        Some("Argon 488")
    );
    assert_eq!(
        instrument.light_sources[0].light_source_type.as_deref(),
        Some("Laser")
    );
    assert_eq!(instrument.light_sources[0].power, Some(12.5));
    let original = ome
        .annotations
        .iter()
        .find_map(|annotation| match annotation {
            bioformats::OmeAnnotation::MapAnnotation {
                id: Some(id),
                values,
                ..
            } if id == "Annotation:OriginalMetadata:0" => Some(values),
            _ => None,
        })
        .expect("Imaris original metadata annotation");
    assert!(original
        .iter()
        .any(|(key, value)| key == "imaris.info.Version" && value == "9.9.0-test"));
    assert!(original.iter().any(|(key, value)| {
        key == "imaris.channel.0.color.rgba"
            && value == &(u32::from_be_bytes([26, 51, 77, 255]) as i32).to_string()
    }));
    assert!(original
        .iter()
        .any(|(key, value)| key == "imaris.objective.Name" && value == "Plan-Apochromat 63x"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn imaris_preserves_surpass_object_graph_metadata_without_reading_large_payloads() {
    let path = tmp("surpass_metadata_ims.ims");
    let _ = std::fs::remove_file(&path);

    let mut file = hdf5_pure_rust::WritableFile::create(&path).unwrap();
    {
        let mut info = file.create_group("DataSetInfo").unwrap();
        let mut image = info.create_group("Image").unwrap();
        image.add_fixed_ascii_attr("X", "1", 8).unwrap();
        image.add_fixed_ascii_attr("Y", "1", 8).unwrap();
        image.add_fixed_ascii_attr("Z", "1", 8).unwrap();
    }
    {
        let mut scene = file.create_group("Scene").unwrap();
        scene
            .add_fixed_ascii_attr("Name", "Root Surpass Scene", 32)
            .unwrap();
        let mut surface = scene.create_group("Surfaces 0").unwrap();
        surface.add_fixed_ascii_attr("Name", "Nuclei", 32).unwrap();
        surface
            .add_fixed_ascii_attr("Type", "Surfaces", 32)
            .unwrap();
        surface.add_attr("Visible", 1i64).unwrap();
        let mut statistics = surface.create_group("Statistics").unwrap();
        statistics
            .new_dataset_builder("NumberOfSurfaces")
            .shape(&[1])
            .write::<i64>(&[3])
            .unwrap();
        statistics
            .new_dataset_builder("Center")
            .shape(&[3])
            .write::<f64>(&[1.0, 2.0, 3.0])
            .unwrap();
        statistics
            .new_dataset_builder("RadiusXYZ")
            .shape(&[3])
            .write::<f64>(&[4.0, 5.0, 6.0])
            .unwrap();
        statistics
            .new_dataset_builder("IndexT")
            .shape(&[1])
            .write::<i64>(&[1])
            .unwrap();
        statistics
            .new_dataset_builder("IndexC")
            .shape(&[1])
            .write::<i64>(&[2])
            .unwrap();
        statistics
            .new_dataset_builder("Names")
            .shape(&[2])
            .write_fixed_ascii_strings(&["Mean Intensity", "Volume"], 16)
            .unwrap();
        statistics
            .new_dataset_builder("Values")
            .shape(&[2, 2])
            .write::<f64>(&[12.5, 13.5, 44.0, 45.0])
            .unwrap();
        statistics
            .new_dataset_builder("LargeMask")
            .shape(&[33])
            .write::<u8>(&[7u8; 33])
            .unwrap();
    }
    {
        let mut dataset = file.create_group("DataSet").unwrap();
        let mut res = dataset.create_group("ResolutionLevel 0").unwrap();
        let mut time = res.create_group("TimePoint 0").unwrap();
        let mut channel = time.create_group("Channel 0").unwrap();
        channel
            .new_dataset_builder("Data")
            .shape(&[1, 1, 1])
            .write::<u8>(&[42])
            .unwrap();
    }
    file.flush().unwrap();

    let mut reader = bioformats::formats::imaris::ImarisReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata.get("imaris.surpass.roots"),
        Some(MetadataValue::String(value)) if value == "Scene"
    ));
    assert!(matches!(
        meta.series_metadata.get("imaris.surpass.Scene.Name"),
        Some(MetadataValue::String(value)) if value == "Root Surpass Scene"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Name"),
        Some(MetadataValue::String(value)) if value == "Nuclei"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Visible"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.NumberOfSurfaces.value"),
        Some(MetadataValue::Int(3))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.Center.value"),
        Some(MetadataValue::String(value)) if value == "1 2 3"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.RadiusXYZ.value"),
        Some(MetadataValue::String(value)) if value == "4 5 6"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.IndexT.value"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.IndexC.value"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.LargeMask.value_status"),
        Some(MetadataValue::String(value)) if value == "not_read_large_dataset"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.table.stat_count"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.table.value_shape"),
        Some(MetadataValue::String(value)) if value == "2 2"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.table.layout"),
        Some(MetadataValue::String(value)) if value == "stat_rows"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.table.Mean_Intensity"),
        Some(MetadataValue::String(value)) if value == "12.5 13.5"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.table.Volume"),
        Some(MetadataValue::String(value)) if value == "44 45"
    ));

    let ome = reader.ome_metadata().unwrap();
    assert_eq!(ome.rois.len(), 1);
    assert_eq!(ome.rois[0].name.as_deref(), Some("Nuclei"));
    assert!(matches!(
        ome.rois[0].shapes.first(),
        Some(OmeShape::Ellipse {
            x,
            y,
            radius_x,
            radius_y,
            the_z: Some(3),
            the_t: Some(1),
            the_c: Some(2),
        }) if *x == 1.0 && *y == 2.0 && *radius_x == 4.0 && *radius_y == 5.0
    ));

    let original = ome
        .annotations
        .into_iter()
        .find_map(|annotation| match annotation {
            bioformats::OmeAnnotation::MapAnnotation {
                id: Some(id),
                values,
                ..
            } if id == "Annotation:OriginalMetadata:0" => Some(values),
            _ => None,
        })
        .expect("Imaris original metadata annotation");
    assert!(original.iter().any(|(key, value)| {
        key == "imaris.surpass.Scene.Surfaces_0.Statistics.NumberOfSurfaces.value" && value == "3"
    }));
    assert!(original.iter().any(|(key, value)| {
        key == "imaris.surpass.Scene.Surfaces_0.Statistics.LargeMask.value_status"
            && value == "not_read_large_dataset"
    }));
    assert!(original.iter().any(|(key, value)| {
        key == "imaris.surpass.Scene.Surfaces_0.Statistics.table.Mean_Intensity"
            && value == "12.5 13.5"
    }));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![42]);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn imaris_preserves_column_oriented_surpass_statistics_table() {
    let path = tmp("surpass_column_statistics_ims.ims");
    let _ = std::fs::remove_file(&path);

    let mut file = hdf5_pure_rust::WritableFile::create(&path).unwrap();
    {
        let mut info = file.create_group("DataSetInfo").unwrap();
        let mut image = info.create_group("Image").unwrap();
        image.add_fixed_ascii_attr("X", "1", 8).unwrap();
        image.add_fixed_ascii_attr("Y", "1", 8).unwrap();
        image.add_fixed_ascii_attr("Z", "1", 8).unwrap();
    }
    {
        let mut scene = file.create_group("Scene").unwrap();
        let mut surface = scene.create_group("Surfaces 0").unwrap();
        surface
            .add_fixed_ascii_attr("Name", "Membranes", 32)
            .unwrap();
        let mut statistics = surface.create_group("Statistics").unwrap();
        statistics
            .new_dataset_builder("Names")
            .shape(&[3])
            .write_fixed_ascii_strings(&["Area", "Volume", "Intensity Mean"], 16)
            .unwrap();
        statistics
            .new_dataset_builder("Values")
            .shape(&[2, 3])
            .write::<f64>(&[1.0, 10.0, 100.0, 2.0, 20.0, 200.0])
            .unwrap();
    }
    {
        let mut dataset = file.create_group("DataSet").unwrap();
        let mut res = dataset.create_group("ResolutionLevel 0").unwrap();
        let mut time = res.create_group("TimePoint 0").unwrap();
        let mut channel = time.create_group("Channel 0").unwrap();
        channel
            .new_dataset_builder("Data")
            .shape(&[1, 1, 1])
            .write::<u8>(&[9])
            .unwrap();
    }
    file.flush().unwrap();

    let mut reader = bioformats::formats::imaris::ImarisReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.table.stat_count"),
        Some(MetadataValue::Int(3))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.table.value_shape"),
        Some(MetadataValue::String(value)) if value == "2 3"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.table.layout"),
        Some(MetadataValue::String(value)) if value == "stat_columns"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.table.Area"),
        Some(MetadataValue::String(value)) if value == "1 2"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.table.Volume"),
        Some(MetadataValue::String(value)) if value == "10 20"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Statistics.table.Intensity_Mean"),
        Some(MetadataValue::String(value)) if value == "100 200"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![9]);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn imaris_reports_large_surpass_geometry_without_reading_payloads() {
    let path = tmp("surpass_large_geometry_ims.ims");
    let _ = std::fs::remove_file(&path);

    let mut file = hdf5_pure_rust::WritableFile::create(&path).unwrap();
    {
        let mut info = file.create_group("DataSetInfo").unwrap();
        let mut image = info.create_group("Image").unwrap();
        image.add_fixed_ascii_attr("X", "1", 8).unwrap();
        image.add_fixed_ascii_attr("Y", "1", 8).unwrap();
        image.add_fixed_ascii_attr("Z", "1", 8).unwrap();
    }
    {
        let mut scene = file.create_group("Scene").unwrap();
        let mut surface = scene.create_group("Surfaces 0").unwrap();
        surface.add_fixed_ascii_attr("Name", "Mesh", 16).unwrap();
        surface
            .new_dataset_builder("Vertices")
            .shape(&[12, 3])
            .write::<f64>(&[1.0; 36])
            .unwrap();
        surface
            .new_dataset_builder("Triangles")
            .shape(&[12, 3])
            .write::<i64>(&[0; 36])
            .unwrap();
    }
    {
        let mut dataset = file.create_group("DataSet").unwrap();
        let mut res = dataset.create_group("ResolutionLevel 0").unwrap();
        let mut time = res.create_group("TimePoint 0").unwrap();
        let mut channel = time.create_group("Channel 0").unwrap();
        channel
            .new_dataset_builder("Data")
            .shape(&[1, 1, 1])
            .write::<u8>(&[5])
            .unwrap();
    }
    file.flush().unwrap();

    let mut reader = bioformats::formats::imaris::ImarisReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Vertices.geometry_role"),
        Some(MetadataValue::String(value)) if value == "vertices"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Vertices.geometry_value_count"),
        Some(MetadataValue::Int(36))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Vertices.geometry_element_count"),
        Some(MetadataValue::Int(12))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Vertices.geometry_component_count"),
        Some(MetadataValue::Int(3))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Vertices.geometry_status"),
        Some(MetadataValue::String(value)) if value == "not_read_large_geometry"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Vertices.value_status"),
        Some(MetadataValue::String(value)) if value == "not_read_large_dataset"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Triangles.geometry_role"),
        Some(MetadataValue::String(value)) if value == "triangles"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("imaris.surpass.Scene.Surfaces_0.Triangles.geometry_element_count"),
        Some(MetadataValue::Int(12))
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![5]);

    let _ = std::fs::remove_file(&path);
}

#[test]
fn imaris_requires_pixel_dataset_and_initialized_series() {
    let path = tmp("weak_ims.ims");
    let _ = std::fs::remove_file(&path);

    let mut reader = bioformats::formats::imaris::ImarisReader::new();
    assert_eq!(reader.series_count(), 0);
    assert_eq!(reader.resolution_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    // DataSetInfo present but no DataSet/.../Data dataset. Since dimensions are
    // derived from the Data shape, a file lacking it is rejected and the reader
    // stays uninitialized.
    let mut file = hdf5_pure_rust::WritableFile::create(&path).unwrap();
    {
        let mut info = file.create_group("DataSetInfo").unwrap();
        let mut image = info.create_group("Image").unwrap();
        image.add_fixed_ascii_attr("X", "2", 1).unwrap();
        image.add_fixed_ascii_attr("Y", "1", 1).unwrap();
        image.add_fixed_ascii_attr("Z", "1", 1).unwrap();
    }
    file.flush().unwrap();

    let err = reader.set_id(&path).unwrap_err();
    assert!(
        err.to_string().contains("Data"),
        "unexpected Imaris error: {err}"
    );
    assert_eq!(reader.series_count(), 0);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn imaris_rejects_out_of_bounds_region() {
    let path = tmp("region_bounds_ims.ims");
    let _ = std::fs::remove_file(&path);

    let mut file = hdf5_pure_rust::WritableFile::create(&path).unwrap();
    {
        let mut info = file.create_group("DataSetInfo").unwrap();
        let mut image = info.create_group("Image").unwrap();
        image.add_fixed_ascii_attr("X", "3", 1).unwrap();
        image.add_fixed_ascii_attr("Y", "2", 1).unwrap();
        image.add_fixed_ascii_attr("Z", "1", 1).unwrap();
    }
    {
        let mut dataset = file.create_group("DataSet").unwrap();
        let mut res = dataset.create_group("ResolutionLevel 0").unwrap();
        let mut time = res.create_group("TimePoint 0").unwrap();
        let mut channel = time.create_group("Channel 0").unwrap();
        channel
            .new_dataset_builder("Data")
            .shape(&[1, 2, 3])
            .write::<u8>(&[1u8, 2, 3, 4, 5, 6])
            .unwrap();
    }
    file.flush().unwrap();

    let mut reader = bioformats::formats::imaris::ImarisReader::new();
    reader.set_id(&path).unwrap();
    let err = reader.open_bytes_region(0, 2, 0, 2, 1).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("outside image bounds")),
        "{err:?}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cellh5_preserves_hdf5_attributes_and_dataset_metadata() {
    let path = tmp("metadata_parity_cellh5.ch5");
    let _ = std::fs::remove_file(&path);

    // CellH5Reader.java#parseStructure() walks the canonical experiment layout
    //   /sample/0/plate/{plate}/experiment/{well}/position/{site}/image/channel
    // (CellH5Constants: PREFIX_PATH "/sample/0/", PLATE "plate/", WELL
    // "/experiment/", SITE "/position/", IMAGE_PATH "image/channel/"). The
    // `image/channel` dataset is itself the 5D [channel, time, zslice, y, x]
    // image stack. Here c=1,t=2,z=1,y=2,x=3, keeping x=3, y=2, t=2.
    let mut file = hdf5_pure_rust::WritableFile::create(&path).unwrap();
    file.add_fixed_ascii_attr(
        "experiment_name",
        "synthetic assay",
        "synthetic assay".len(),
    )
    .unwrap();
    {
        let mut sample = file.create_group("sample").unwrap();
        let mut zero = sample.create_group("0").unwrap();
        let mut plate = zero.create_group("plate").unwrap();
        let mut plate0 = plate.create_group("Plate0").unwrap();
        let mut experiment = plate0.create_group("experiment").unwrap();
        let mut well = experiment.create_group("A01").unwrap();
        let mut positions = well.create_group("position").unwrap();
        let mut site = positions.create_group("1").unwrap();
        let mut image = site.create_group("image").unwrap();
        image
            .new_dataset_builder("channel")
            .shape(&[1, 2, 1, 2, 3])
            .attr("wavelength_nm", 488u32)
            .unwrap()
            .write::<u16>(&[1u16, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12])
            .unwrap();
    }
    file.flush().unwrap();

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
fn cellh5_rejects_zero_dataset_axes() {
    let path = tmp("zero_axis_cellh5.ch5");
    let _ = std::fs::remove_file(&path);

    build_cellh5_channel(&path, |b| {
        b.shape(&[1, 1, 0, 1, 1]).write::<u16>(&[]).unwrap();
    });

    let mut reader = bioformats::formats::cellh5::CellH5Reader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        err.to_string().contains("invalid Z dimension 0"),
        "unexpected CellH5 error: {err}"
    );
    let _ = std::fs::remove_file(&path);
}

#[test]
fn cellh5_rejects_unsupported_dataset_dtype_and_clears_failed_reopen() {
    let good = tmp("good_cellh5.ch5");
    let bad = tmp("bad_dtype_cellh5.ch5");
    let _ = std::fs::remove_file(&good);
    let _ = std::fs::remove_file(&bad);

    build_cellh5_channel(&good, |b| {
        b.shape(&[1, 1, 1, 1, 1]).write::<u16>(&[1u16]).unwrap();
    });

    build_cellh5_channel(&bad, |b| {
        b.shape(&[1, 1, 1, 1, 1]).write::<f64>(&[1.0f64]).unwrap();
    });

    let mut reader = bioformats::formats::cellh5::CellH5Reader::new();
    reader.set_id(&good).unwrap();
    assert_eq!(reader.series_count(), 1);
    let err = reader.set_id(&bad).unwrap_err();
    assert!(
        err.to_string().contains("unsupported dtype size 8"),
        "unexpected CellH5 error: {err}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_file(&good);
    let _ = std::fs::remove_file(&bad);
}

#[test]
fn spe_rejects_zero_dimensions_and_short_payload() {
    let zero = tmp("zero.spe");
    let mut bytes = vec![0u8; 4100];
    bytes[108..110].copy_from_slice(&3i16.to_le_bytes());
    bytes[656..658].copy_from_slice(&1u16.to_le_bytes());
    bytes[1446..1450].copy_from_slice(&1i32.to_le_bytes());
    std::fs::write(&zero, bytes).unwrap();
    let mut reader = bioformats::formats::spe::SpeReader::new();
    let err = reader.set_id(&zero).unwrap_err();
    assert!(err.to_string().contains("non-positive width"));
    let _ = std::fs::remove_file(&zero);

    let short = tmp("short.spe");
    let mut bytes = vec![0u8; 4100];
    bytes[42..44].copy_from_slice(&2u16.to_le_bytes());
    bytes[656..658].copy_from_slice(&2u16.to_le_bytes());
    bytes[108..110].copy_from_slice(&3i16.to_le_bytes());
    bytes[1446..1450].copy_from_slice(&1i32.to_le_bytes());
    bytes.extend_from_slice(&[1, 2]);
    std::fs::write(&short, bytes).unwrap();
    let mut reader = bioformats::formats::spe::SpeReader::new();
    let err = reader.set_id(&short).unwrap_err();
    assert!(err.to_string().contains("shorter than declared"));
    let _ = std::fs::remove_file(&short);
}

#[test]
fn spe_rejects_unknown_pixel_type_and_requires_initialization_for_series() {
    let path = tmp("unknown_pixel_type.spe");
    let mut bytes = vec![0u8; 4100];
    bytes[42..44].copy_from_slice(&1u16.to_le_bytes());
    bytes[656..658].copy_from_slice(&1u16.to_le_bytes());
    bytes[108..110].copy_from_slice(&99i16.to_le_bytes());
    bytes[1446..1450].copy_from_slice(&1i32.to_le_bytes());
    std::fs::write(&path, bytes).unwrap();

    let mut reader = bioformats::formats::spe::SpeReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    let err = reader.set_id(&path).unwrap_err();
    assert!(err.to_string().contains("invalid pixel type 99"));
    assert_eq!(reader.series_count(), 0);
    let _ = std::fs::remove_file(path);
}

fn minimal_psd(version: u16, width: u32, height: u32, channels: u16, depth: u16) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"8BPS");
    data.extend_from_slice(&version.to_be_bytes());
    data.extend_from_slice(&[0; 6]);
    data.extend_from_slice(&channels.to_be_bytes());
    data.extend_from_slice(&height.to_be_bytes());
    data.extend_from_slice(&width.to_be_bytes());
    data.extend_from_slice(&depth.to_be_bytes());
    data.extend_from_slice(&3u16.to_be_bytes());
    data.extend_from_slice(&0u32.to_be_bytes());
    data.extend_from_slice(&0u32.to_be_bytes());
    data.extend_from_slice(&0u32.to_be_bytes());
    data.extend_from_slice(&0u16.to_be_bytes());
    if width > 0 && height > 0 && channels > 0 && matches!(depth, 8 | 16 | 32) {
        let bytes_per_sample = match depth {
            8 => 1usize,
            16 => 2usize,
            32 => 4usize,
            _ => unreachable!(),
        };
        data.resize(
            data.len() + width as usize * height as usize * channels as usize * bytes_per_sample,
            0,
        );
    }
    data
}

fn psd_with_text_resource(tag: u16, payload: &[u8]) -> Vec<u8> {
    let mut resource = Vec::new();
    resource.extend_from_slice(b"8BIM");
    resource.extend_from_slice(&tag.to_be_bytes());
    resource.push(0); // empty Pascal string name
    resource.push(0); // pad Pascal name to even length
    resource.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    resource.extend_from_slice(payload);
    if payload.len() % 2 == 1 {
        resource.push(0);
    }

    let mut data = Vec::new();
    data.extend_from_slice(b"8BPS");
    data.extend_from_slice(&1u16.to_be_bytes());
    data.extend_from_slice(&[0; 6]);
    data.extend_from_slice(&3u16.to_be_bytes());
    data.extend_from_slice(&1u32.to_be_bytes());
    data.extend_from_slice(&1u32.to_be_bytes());
    data.extend_from_slice(&8u16.to_be_bytes());
    data.extend_from_slice(&3u16.to_be_bytes());
    data.extend_from_slice(&0u32.to_be_bytes()); // color mode data
    data.extend_from_slice(&(resource.len() as u32).to_be_bytes());
    data.extend_from_slice(&resource);
    data.extend_from_slice(&0u32.to_be_bytes()); // layer/mask block
    data.extend_from_slice(&0u16.to_be_bytes()); // raw composite pixels
    data.extend_from_slice(&[1, 2, 3]);
    data
}

fn psd_utf16be_string(text: &str) -> Vec<u8> {
    let units = text.encode_utf16().collect::<Vec<_>>();
    let mut data = Vec::new();
    data.extend_from_slice(&(units.len() as u32).to_be_bytes());
    for unit in units {
        data.extend_from_slice(&unit.to_be_bytes());
    }
    data
}

#[test]
fn photoshop_rejects_unknown_header_values_and_short_payload() {
    let version = tmp("bad_version.psd");
    std::fs::write(&version, minimal_psd(3, 1, 1, 1, 8)).unwrap();
    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
    let err = reader.set_id(&version).unwrap_err();
    assert!(err.to_string().contains("unsupported version 3"));
    let _ = std::fs::remove_file(&version);

    let depth = tmp("bad_depth.psd");
    std::fs::write(&depth, minimal_psd(1, 1, 1, 3, 12)).unwrap();
    let err = bioformats::formats::photoshop::PsdReader::new()
        .set_id(&depth)
        .unwrap_err();
    assert!(err.to_string().contains("unsupported bit depth 12"));
    let _ = std::fs::remove_file(&depth);

    let zero = tmp("zero_dim.psd");
    std::fs::write(&zero, minimal_psd(1, 0, 1, 3, 8)).unwrap();
    let err = bioformats::formats::photoshop::PsdReader::new()
        .set_id(&zero)
        .unwrap_err();
    assert!(err.to_string().contains("non-positive"));
    let _ = std::fs::remove_file(&zero);

    let bad_channels = tmp("bad_channels.psd");
    std::fs::write(&bad_channels, minimal_psd(1, 1, 1, 1, 8)).unwrap();
    let err = bioformats::formats::photoshop::PsdReader::new()
        .set_id(&bad_channels)
        .unwrap_err();
    assert!(err.to_string().contains("channel count is too small"));
    let _ = std::fs::remove_file(&bad_channels);

    let short = tmp("short.psd");
    let mut data = minimal_psd(1, 2, 2, 3, 8);
    data.pop();
    std::fs::write(&short, data).unwrap();
    let err = bioformats::formats::photoshop::PsdReader::new()
        .set_id(&short)
        .unwrap_err();
    assert!(err.to_string().contains("failed to fill whole buffer"));
    let _ = std::fs::remove_file(short);
}

#[test]
fn photoshop_preserves_header_and_image_resource_metadata() {
    let path = tmp("metadata_resource.psd");
    std::fs::write(
        &path,
        psd_with_text_resource(1035, b"https://example.invalid/image"),
    )
    .unwrap();

    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3]);

    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("psd.version"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("psd.color_mode"),
        Some(MetadataValue::String(value)) if value == "RGB"
    ));
    assert!(matches!(
        metadata.get("psd.compression"),
        Some(MetadataValue::String(value)) if value == "Raw"
    ));
    assert!(matches!(
        metadata.get("psd.image_resources.count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1035.type"),
        Some(MetadataValue::String(value)) if value == "Url"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1035.text"),
        Some(MetadataValue::String(value)) if value == "https://example.invalid/image"
    ));

    let _ = std::fs::remove_file(path);
}

#[test]
fn photoshop_decodes_pixel_aspect_ratio_image_resource() {
    let path = tmp("pixel_aspect_ratio_resource.psd");
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_be_bytes());
    payload.extend_from_slice(&1.25f64.to_bits().to_be_bytes());
    std::fs::write(&path, psd_with_text_resource(1064, &payload)).unwrap();

    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    reader.set_id(&path).unwrap();

    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("psd.image_resource.1064.type"),
        Some(MetadataValue::String(value)) if value == "PixelAspectRatio"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1064.bytes"),
        Some(MetadataValue::Int(12))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1064.version"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1064.aspect_ratio"),
        Some(MetadataValue::Float(value)) if (*value - 1.25).abs() < 1.0e-12
    ));

    let _ = std::fs::remove_file(path);
}

#[test]
fn photoshop_decodes_resolution_info_image_resource() {
    let path = tmp("resolution_info_resource.psd");
    let mut payload = Vec::new();
    payload.extend_from_slice(&(300u32 << 16).to_be_bytes());
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.extend_from_slice(&(150u32 << 16).to_be_bytes());
    payload.extend_from_slice(&2u16.to_be_bytes());
    payload.extend_from_slice(&3u16.to_be_bytes());
    std::fs::write(&path, psd_with_text_resource(1005, &payload)).unwrap();

    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    reader.set_id(&path).unwrap();

    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("psd.image_resource.1005.type"),
        Some(MetadataValue::String(value)) if value == "ResolutionInfo"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1005.bytes"),
        Some(MetadataValue::Int(16))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1005.horizontal_resolution"),
        Some(MetadataValue::Float(value)) if (*value - 300.0).abs() < 1.0e-12
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1005.horizontal_resolution_unit"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1005.width_unit"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1005.vertical_resolution"),
        Some(MetadataValue::Float(value)) if (*value - 150.0).abs() < 1.0e-12
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1005.vertical_resolution_unit"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1005.height_unit"),
        Some(MetadataValue::Int(3))
    ));

    let _ = std::fs::remove_file(path);
}

#[test]
fn photoshop_decodes_display_info_image_resource() {
    let path = tmp("display_info_resource.psd");
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.extend_from_slice(&65535u16.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload.extend_from_slice(&80u16.to_be_bytes());
    payload.push(2);
    payload.push(0);
    std::fs::write(&path, psd_with_text_resource(1007, &payload)).unwrap();

    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    reader.set_id(&path).unwrap();

    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("psd.image_resource.1007.type"),
        Some(MetadataValue::String(value)) if value == "DisplayInfo"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1007.display_info_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1007.display_info.0.color_space"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1007.display_info.0.color_components"),
        Some(MetadataValue::String(value)) if value == "65535,0,0,0"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1007.display_info.0.opacity"),
        Some(MetadataValue::Int(80))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1007.display_info.0.kind"),
        Some(MetadataValue::Int(2))
    ));
    assert!(!metadata.contains_key("psd.image_resource.1007.parse_status"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn photoshop_decodes_print_flags_image_resource() {
    let path = tmp("print_flags_resource.psd");
    std::fs::write(
        &path,
        psd_with_text_resource(1011, &[1, 0, 1, 1, 0, 0, 1, 0, 1]),
    )
    .unwrap();

    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    reader.set_id(&path).unwrap();

    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("psd.image_resource.1011.type"),
        Some(MetadataValue::String(value)) if value == "PrintFlags"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1011.labels"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1011.crop_marks"),
        Some(MetadataValue::Bool(false))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1011.color_bars"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1011.registration_marks"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1011.interpolate"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1011.print_flags"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(!metadata.contains_key("psd.image_resource.1011.parse_status"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn photoshop_decodes_print_flags_information_image_resource() {
    let path = tmp("print_flags_information_resource.psd");
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.push(1);
    payload.push(0);
    payload.extend_from_slice(&144u32.to_be_bytes());
    payload.extend_from_slice(&72u16.to_be_bytes());
    std::fs::write(&path, psd_with_text_resource(10000, &payload)).unwrap();

    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    reader.set_id(&path).unwrap();

    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("psd.image_resource.10000.type"),
        Some(MetadataValue::String(value)) if value == "PrintFlagsInformation"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.10000.version"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.10000.center_crop_marks"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.10000.reserved"),
        Some(MetadataValue::Int(0))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.10000.bleed_width"),
        Some(MetadataValue::Int(144))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.10000.bleed_width_scale"),
        Some(MetadataValue::Int(72))
    ));
    assert!(!metadata.contains_key("psd.image_resource.10000.parse_status"));

    let _ = std::fs::remove_file(path);
}

#[test]
fn photoshop_print_flags_information_records_malformed_payloads() {
    let short = tmp("short_print_flags_information_resource.psd");
    std::fs::write(&short, psd_with_text_resource(10000, &[0, 1, 1])).unwrap();
    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    reader.set_id(&short).unwrap();
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("psd.image_resource.10000.parse_status"),
        Some(MetadataValue::String(value)) if value == "truncated"
    ));
    let _ = std::fs::remove_file(short);

    let trailing = tmp("trailing_print_flags_information_resource.psd");
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u16.to_be_bytes());
    payload.push(0);
    payload.push(0);
    payload.extend_from_slice(&144u32.to_be_bytes());
    payload.extend_from_slice(&72u16.to_be_bytes());
    payload.extend_from_slice(&[9, 10]);
    std::fs::write(&trailing, psd_with_text_resource(10000, &payload)).unwrap();
    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    reader.set_id(&trailing).unwrap();
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("psd.image_resource.10000.parse_status"),
        Some(MetadataValue::String(value)) if value == "trailing_2_bytes"
    ));
    let _ = std::fs::remove_file(trailing);
}

#[test]
fn photoshop_decodes_version_info_image_resource() {
    let path = tmp("version_info_resource.psd");
    let mut payload = Vec::new();
    payload.extend_from_slice(&1u32.to_be_bytes());
    payload.push(1);
    payload.extend_from_slice(&psd_utf16be_string("Writer"));
    payload.extend_from_slice(&psd_utf16be_string("Reader"));
    payload.extend_from_slice(&2u32.to_be_bytes());
    std::fs::write(&path, psd_with_text_resource(1057, &payload)).unwrap();

    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    reader.set_id(&path).unwrap();

    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("psd.image_resource.1057.type"),
        Some(MetadataValue::String(value)) if value == "VersionInfo"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1057.version"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1057.has_real_merged_data"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1057.writer_name"),
        Some(MetadataValue::String(value)) if value == "Writer"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1057.reader_name"),
        Some(MetadataValue::String(value)) if value == "Reader"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1057.file_version"),
        Some(MetadataValue::Int(2))
    ));

    let _ = std::fs::remove_file(path);
}

#[test]
fn photoshop_decodes_copyright_flag_image_resource() {
    let path = tmp("copyright_flag_resource.psd");
    std::fs::write(&path, psd_with_text_resource(1034, &[1])).unwrap();

    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    reader.set_id(&path).unwrap();

    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("psd.image_resource.1034.type"),
        Some(MetadataValue::String(value)) if value == "CopyrightFlag"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1034.bytes"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1034.copyrighted"),
        Some(MetadataValue::Bool(true))
    ));

    let _ = std::fs::remove_file(path);
}

#[test]
fn photoshop_decodes_global_angle_image_resource() {
    let path = tmp("global_angle_resource.psd");
    std::fs::write(&path, psd_with_text_resource(1037, &120i32.to_be_bytes())).unwrap();

    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    reader.set_id(&path).unwrap();

    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("psd.image_resource.1037.type"),
        Some(MetadataValue::String(value)) if value == "GlobalAngle"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1037.bytes"),
        Some(MetadataValue::Int(4))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1037.angle"),
        Some(MetadataValue::Int(120))
    ));

    let _ = std::fs::remove_file(path);
}

#[test]
fn photoshop_decodes_icc_profile_image_resource_metadata() {
    let path = tmp("icc_profile_resource.psd");
    let mut payload = vec![0u8; 128];
    payload[0..4].copy_from_slice(&128u32.to_be_bytes());
    payload[8] = 4;
    payload[9] = 0x30;
    payload[12..16].copy_from_slice(b"mntr");
    payload[16..20].copy_from_slice(b"RGB ");
    payload[20..24].copy_from_slice(b"XYZ ");
    payload[36..40].copy_from_slice(b"acsp");
    std::fs::write(&path, psd_with_text_resource(1039, &payload)).unwrap();

    let mut reader = bioformats::formats::photoshop::PsdReader::new();
    reader.set_id(&path).unwrap();

    let metadata = &reader.metadata().series_metadata;
    assert!(matches!(
        metadata.get("psd.image_resource.1039.type"),
        Some(MetadataValue::String(value)) if value == "ICCProfile"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1039.profile_bytes"),
        Some(MetadataValue::Int(128))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1039.profile_applied"),
        Some(MetadataValue::Bool(false))
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1039.version"),
        Some(MetadataValue::String(value)) if value == "4.3"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1039.profile_class"),
        Some(MetadataValue::String(value)) if value == "mntr"
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1039.color_space"),
        Some(MetadataValue::String(value)) if value == "RGB "
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1039.pcs"),
        Some(MetadataValue::String(value)) if value == "XYZ "
    ));
    assert!(matches!(
        metadata.get("psd.image_resource.1039.signature"),
        Some(MetadataValue::String(value)) if value == "acsp"
    ));
    assert!(!metadata.contains_key("psd.image_resource.1039.parse_status"));

    let _ = std::fs::remove_file(path);
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
fn dicom_bit_packed_pixels_are_read_raw_like_java() {
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
    // Java's DicomReader does NOT bit-unpack sub-byte BitsAllocated: it rounds up
    // to a byte boundary (uint8 here) and reads the raw packed bytes straight in,
    // zero-padding the tail. So 5 one-bit pixels packed in one byte (0b00010101)
    // become [0x15, 0, 0, 0, 0], matching the Java reference byte-for-byte.
    assert_eq!(reader.open_bytes(0).unwrap(), vec![0b0001_0101, 0, 0, 0, 0]);
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
    // 1x1 single-channel: sizeX and sizeC are both odd, so ND2 stores one
    // scanline-pad sample per row (Java getScanlinePad). Each frame is therefore
    // 2 bytes (pixel + pad); the reader strips the pad on read.
    let image0_pos = push_chunk(&mut bytes, "ImageDataSeq|0!", &[11, 0]);
    bytes.extend_from_slice(b"more-junk");
    let image1_pos = push_chunk(&mut bytes, "ImageDataSeq|1!", &[22, 0]);

    let mut entries = Vec::new();
    for (name, position, data_len) in [
        ("ImageAttributesLV", attr_pos, attr_xml.len() as u64),
        ("ImageDataSeq|0", image0_pos, 2u64),
        ("ImageDataSeq|1", image1_pos, 2u64),
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
fn nd2_orders_image_data_chunks_by_sequence_index() {
    let path = tmp("sequence_order.nd2");
    let mut bytes = Vec::new();
    let attr_xml = b"<uiWidth>1</uiWidth><uiHeight>1</uiHeight><uiComp>1</uiComp><uiBpc>8</uiBpc>";
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    push_nd2_chunk(&mut bytes, "ImageDataSeq|1!", &[22, 0]);
    push_nd2_chunk(&mut bytes, "ImageDataSeq|0!", &[11, 0]);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().image_count, 2);
    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("nd2_image_data_chunks"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        md.get("nd2_image_data_sequence_indices"),
        Some(MetadataValue::String(value)) if value == "0,1"
    ));
    assert!(matches!(
        md.get("nd2_image_data_chunk_lengths"),
        Some(MetadataValue::String(value)) if value == "2,2"
    ));
    assert!(matches!(
        md.get("nd2_first_image_data_encoding"),
        Some(MetadataValue::String(value)) if value == "raw"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![11]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![22]);
}

#[test]
fn nd2_records_bounded_image_data_and_metadata_sequence_diagnostics() {
    let path = tmp("sequence_diagnostics.nd2");
    let mut bytes = Vec::new();
    let attr_xml = b"<uiWidth>2</uiWidth><uiHeight>1</uiHeight><uiComp>1</uiComp><uiBpc>8</uiBpc>";
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);

    let mut frame1 = 0.50f64.to_le_bytes().to_vec();
    frame1.extend_from_slice(&[22, 23]);
    push_nd2_chunk(&mut bytes, "ImageDataSeq|1!", &frame1);
    push_nd2_chunk(
        &mut bytes,
        "ImageMetadataSeqLV|1!",
        br#"<sDescription value="later"/><dTimeMSec value="500"/><dZPos value="2.5"/>"#,
    );

    let mut frame0 = 0.25f64.to_le_bytes().to_vec();
    frame0.extend_from_slice(&[11, 12]);
    push_nd2_chunk(&mut bytes, "ImageDataSeq|0!", &frame0);
    push_nd2_chunk(
        &mut bytes,
        "ImageMetadataSeq|0!",
        br#"<sDescription value="first"/><dTimeMSec value="0"/><dZPos value="1.5"/>"#,
    );
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![11, 12]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![22, 23]);

    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("nd2_image_data_encodings"),
        Some(MetadataValue::String(value)) if value == "raw_with_8_byte_prefix,raw_with_8_byte_prefix"
    ));
    assert!(matches!(
        md.get("nd2_image_data_payload_offsets"),
        Some(MetadataValue::String(value)) if value == "8,8"
    ));
    assert!(matches!(
        md.get("nd2_image_data_timestamps"),
        Some(MetadataValue::String(value)) if value == "0.25,0.5"
    ));
    assert!(matches!(
        md.get("nd2_image_metadata_seq_chunks"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        md.get("nd2_image_metadata_seq_indices"),
        Some(MetadataValue::String(value)) if value == "0,1"
    ));
    assert!(matches!(
        md.get("nd2_image_metadata_seq_matches_images"),
        Some(MetadataValue::Bool(true))
    ));
    assert!(matches!(
        md.get("nd2_image_metadata_seq_timestamps"),
        Some(MetadataValue::String(value)) if value == "0,0.5"
    ));

    let ome = reader.ome_metadata().unwrap();
    assert_eq!(ome.images[0].planes.len(), 2);
    assert_eq!(ome.images[0].planes[0].delta_t, Some(0.25));
    assert_eq!(ome.images[0].planes[1].delta_t, Some(0.5));
    assert_eq!(ome.images[0].planes[0].position_z, Some(1.5));
    assert_eq!(ome.images[0].planes[1].position_z, Some(2.5));
}

#[test]
fn nd2_uses_modern_loop_counts_for_z_and_t_dimensions() {
    let path = tmp("loop_counts.nd2");
    let mut bytes = Vec::new();
    let attr_xml = br#"<?xml version="1.0"?>
<variant>
  <uiWidth value="1"/>
  <uiHeight value="1"/>
  <uiComp value="1"/>
  <uiBpc value="8"/>
  <uiSequenceCount value="6"/>
  <uiCount runtype="CLxZStackLoop" value="2"/>
  <uiCount runtype="CLxTimeLoop" value="3"/>
</variant>"#;
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    for i in 0..6u8 {
        push_nd2_chunk(&mut bytes, &format!("ImageDataSeq|{}!", i), &[10 + i, 0]);
    }
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_z, 2);
    assert_eq!(reader.metadata().size_t, 3);
    assert_eq!(reader.metadata().image_count, 6);
    assert!(matches!(
        reader.metadata().series_metadata.get("nd2_loop_size_z"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        reader.metadata().series_metadata.get("nd2_loop_size_t"),
        Some(MetadataValue::Int(3))
    ));
    assert_eq!(reader.open_bytes(5).unwrap(), vec![15]);
}

#[test]
fn nd2_records_modern_loop_order_evidence_from_xml() {
    let path = tmp("loop_order_evidence.nd2");
    let mut bytes = Vec::new();
    let attr_xml = br#"<?xml version="1.0"?>
<variant>
  <uiWidth value="1"/>
  <uiHeight value="1"/>
  <uiComp value="1"/>
  <uiBpc value="8"/>
  <uiCount runtype="CLxXYPosLoop" value="1"/>
  <uiCount runtype="CLxTimeLoop" value="3"/>
  <uiCount runtype="CLxZStackLoop" value="2"/>
</variant>"#;
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    for i in 0..6u8 {
        push_nd2_chunk(&mut bytes, &format!("ImageDataSeq|{}!", i), &[30 + i, 0]);
    }
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_z, 2);
    assert_eq!(reader.metadata().size_t, 3);
    assert_eq!(reader.metadata().image_count, 6);
    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("nd2_loop_order"),
        Some(MetadataValue::String(value)) if value == "XYPosLoop,TimeLoop,ZStackLoop"
    ));
    assert!(matches!(
        md.get("nd2_loop_count_evidence"),
        Some(MetadataValue::String(value)) if value == "XYPosLoop=1,TimeLoop=3,ZStackLoop=2"
    ));
    assert_eq!(reader.open_bytes(5).unwrap(), vec![35]);
}

#[test]
fn nd2_uses_unambiguous_xml_loop_order_for_contiguous_xy_position_series() {
    let path = tmp("xml_loop_order_contiguous_xy_positions.nd2");
    let mut bytes = Vec::new();
    let attr_xml = br#"<?xml version="1.0"?>
<variant>
  <uiWidth value="1"/>
  <uiHeight value="1"/>
  <uiComp value="1"/>
  <uiBpc value="8"/>
  <uiCount runtype="CLxXYPosLoop" value="2"/>
  <uiCount runtype="CLxZStackLoop" value="2"/>
</variant>"#;
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    for (index, value) in [10u8, 11, 20, 21].into_iter().enumerate() {
        push_nd2_chunk(&mut bytes, &format!("ImageDataSeq|{}!", index), &[value, 0]);
    }
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.series_count(), 2);
    assert_eq!(reader.metadata().size_z, 2);
    assert_eq!(reader.metadata().image_count, 2);
    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("nd2_loop_series_handling"),
        Some(MetadataValue::String(value)) if value == "split_xy_positions_contiguous_full_series"
    ));
    assert!(matches!(
        md.get("nd2_loop_series_assumed_layout"),
        Some(MetadataValue::String(value)) if value == "contiguous"
    ));
    assert!(matches!(
        md.get("nd2_loop_series_layout_source"),
        Some(MetadataValue::String(value)) if value == "xml_loop_order_outer_to_inner"
    ));
    assert!(matches!(
        md.get("nd2_series_source_planes"),
        Some(MetadataValue::String(value)) if value == "0,1"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![11]);

    reader.set_series(1).unwrap();
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_series_source_planes"),
        Some(MetadataValue::String(value)) if value == "2,3"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![20]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![21]);
}

#[test]
fn nd2_splits_simple_xy_position_loop_into_series() {
    let path = tmp("xy_position_series.nd2");
    let mut bytes = Vec::new();
    let attr_xml = br#"<?xml version="1.0"?>
<variant>
  <uiWidth value="1"/>
  <uiHeight value="1"/>
  <uiComp value="1"/>
  <uiBpc value="8"/>
  <uiCount runtype="CLxXYPosLoop" value="2"/>
</variant>"#;
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    push_nd2_chunk(&mut bytes, "ImageDataSeq|0!", &[21, 0]);
    push_nd2_chunk(&mut bytes, "ImageDataSeq|1!", &[37, 0]);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.series_count(), 2);
    assert_eq!(reader.metadata().image_count, 1);
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_loop_series_handling"),
        Some(MetadataValue::String(value)) if value == "split_xy_positions_one_plane_each"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![21]);

    reader.set_series(1).unwrap();
    assert_eq!(reader.metadata().image_count, 1);
    assert!(matches!(
        reader.metadata().series_metadata.get("nd2_series_index"),
        Some(MetadataValue::Int(1))
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![37]);
}

#[test]
fn nd2_splits_interleaved_xy_position_full_series() {
    let path = tmp("xy_position_full_series.nd2");
    let mut bytes = Vec::new();
    let attr_xml = br#"<?xml version="1.0"?>
<variant>
  <uiWidth value="1"/>
  <uiHeight value="1"/>
  <uiComp value="1"/>
  <uiBpc value="8"/>
  <uiCount runtype="CLxXYPosLoop" value="2"/>
  <uiCount runtype="CLxZStackLoop" value="2"/>
</variant>"#;
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    for (index, value) in [10u8, 20, 11, 21].into_iter().enumerate() {
        push_nd2_chunk(&mut bytes, &format!("ImageDataSeq|{}!", index), &[value, 0]);
        push_nd2_chunk(
            &mut bytes,
            &format!("ImageMetadataSeqLV|{}!", index),
            format!(
                r#"<dTimeMSec value="{}"/><dZPos value="{}"/>"#,
                index * 100,
                index
            )
            .as_bytes(),
        );
    }
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.series_count(), 2);
    assert_eq!(reader.metadata().size_z, 2);
    assert_eq!(reader.metadata().size_t, 1);
    assert_eq!(reader.metadata().image_count, 2);
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_loop_series_handling"),
        Some(MetadataValue::String(value)) if value == "split_xy_positions_interleaved_full_series"
    ));
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_loop_series_candidate_layouts"),
        Some(MetadataValue::String(value)) if value == "interleaved,contiguous"
    ));
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_loop_series_assumed_layout"),
        Some(MetadataValue::String(value)) if value == "interleaved"
    ));
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_series_source_planes"),
        Some(MetadataValue::String(value)) if value == "0,2"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![11]);
    let ome0 = reader.ome_metadata().unwrap();
    assert_eq!(ome0.images[0].planes[0].position_z, Some(0.0));
    assert_eq!(ome0.images[0].planes[1].position_z, Some(2.0));
    assert_eq!(ome0.images[0].planes[1].delta_t, Some(0.2));

    reader.set_series(1).unwrap();
    assert_eq!(reader.metadata().size_z, 2);
    assert_eq!(reader.metadata().size_t, 1);
    assert_eq!(reader.metadata().image_count, 2);
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_series_source_planes"),
        Some(MetadataValue::String(value)) if value == "1,3"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![20]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![21]);
    let ome1 = reader.ome_metadata().unwrap();
    assert_eq!(ome1.images[0].planes[0].position_z, Some(1.0));
    assert_eq!(ome1.images[0].planes[1].position_z, Some(3.0));
    assert_eq!(ome1.images[0].planes[1].delta_t, Some(0.3));
}

#[test]
fn nd2_splits_contiguous_xy_position_full_series_when_z_metadata_disambiguates() {
    let path = tmp("xy_position_contiguous_full_series.nd2");
    let mut bytes = Vec::new();
    let attr_xml = br#"<?xml version="1.0"?>
<variant>
  <uiWidth value="1"/>
  <uiHeight value="1"/>
  <uiComp value="1"/>
  <uiBpc value="8"/>
  <uiCount runtype="CLxXYPosLoop" value="2"/>
  <uiCount runtype="CLxZStackLoop" value="2"/>
</variant>"#;
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    for (index, (value, z)) in [(10u8, 0.0), (11, 1.0), (20, 0.0), (21, 1.0)]
        .into_iter()
        .enumerate()
    {
        push_nd2_chunk(&mut bytes, &format!("ImageDataSeq|{}!", index), &[value, 0]);
        push_nd2_chunk(
            &mut bytes,
            &format!("ImageMetadataSeqLV|{}!", index),
            format!(
                r#"<dTimeMSec value="{}"/><dZPos value="{}"/>"#,
                index * 100,
                z
            )
            .as_bytes(),
        );
    }
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.series_count(), 2);
    assert_eq!(reader.metadata().size_z, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_loop_series_handling"),
        Some(MetadataValue::String(value)) if value == "split_xy_positions_contiguous_full_series"
    ));
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_loop_series_assumed_layout"),
        Some(MetadataValue::String(value)) if value == "contiguous"
    ));
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_series_source_planes"),
        Some(MetadataValue::String(value)) if value == "0,1"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![10]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![11]);
    let ome0 = reader.ome_metadata().unwrap();
    assert_eq!(ome0.images[0].planes[0].position_z, Some(0.0));
    assert_eq!(ome0.images[0].planes[1].position_z, Some(1.0));

    reader.set_series(1).unwrap();
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_series_source_planes"),
        Some(MetadataValue::String(value)) if value == "2,3"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![20]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![21]);
    let ome1 = reader.ome_metadata().unwrap();
    assert_eq!(ome1.images[0].planes[0].position_z, Some(0.0));
    assert_eq!(ome1.images[0].planes[1].position_z, Some(1.0));
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
fn nd2_decodes_java_style_raw_frame_with_trailing_structured_bytes() {
    let path = tmp("raw_frame_prefix_trailer.nd2");
    let mut frame = 1.25f64.to_le_bytes().to_vec();
    frame.extend_from_slice(&[17, 23]);
    frame.extend_from_slice(b"TAIL");
    write_synthetic_nd2(&path, &frame);

    let mut reader = ImageReader::open(&path).unwrap();
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_first_image_data_encoding"),
        Some(MetadataValue::String(value)) if value == "raw_with_8_byte_prefix_and_trailer"
    ));
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_image_data_payload_offsets"),
        Some(MetadataValue::String(value)) if value == "8"
    ));
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_image_data_timestamps"),
        Some(MetadataValue::String(value)) if value == "1.25"
    ));
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
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_first_image_data_encoding"),
        Some(MetadataValue::String(value)) if value == "unknown_oversized"
    ));
    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(msg) if msg.contains("ImageDataSeq|0!") && msg.contains("length 5") && msg.contains("unsupported structured frame encoding"))
    );
}

#[test]
fn nd2_decodes_raw_frame_from_little_endian_chunk_table() {
    let path = tmp("chunk_table_frame.nd2");
    let mut frame = Vec::new();
    frame.extend_from_slice(&2u32.to_le_bytes());
    frame.extend_from_slice(&24u32.to_le_bytes());
    frame.extend_from_slice(&1u32.to_le_bytes());
    frame.extend_from_slice(&26u32.to_le_bytes());
    frame.extend_from_slice(&1u32.to_le_bytes());
    frame.extend_from_slice(b"gap!");
    frame.push(17);
    frame.push(0);
    frame.push(23);
    write_synthetic_nd2(&path, &frame);

    let mut reader = ImageReader::open(&path).unwrap();
    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("nd2_first_image_data_encoding"),
        Some(MetadataValue::String(value)) if value == "chunk_table_le32"
    ));
    assert!(matches!(
        md.get("nd2_image_data_encodings"),
        Some(MetadataValue::String(value)) if value == "chunk_table_le32"
    ));
    assert!(matches!(
        md.get("nd2_image_data_chunk_tables"),
        Some(MetadataValue::String(value))
            if value == "plane=0:offset=0,entry_width=4,count=2,first_payload=24,payload_bytes=2"
    ));
    assert!(matches!(
        md.get("nd2_image_data_chunk_table_ranges"),
        Some(MetadataValue::String(value)) if value == "plane=0:24..25,26..27"
    ));

    assert_eq!(reader.open_bytes(0).unwrap(), vec![17, 23]);
}

#[test]
fn nd2_decodes_timestamp_prefixed_raw_frame_from_little_endian_chunk_table() {
    let path = tmp("timestamp_prefixed_chunk_table_frame.nd2");
    let mut frame = 2.5f64.to_le_bytes().to_vec();
    frame.extend_from_slice(&2u32.to_le_bytes());
    frame.extend_from_slice(&32u32.to_le_bytes());
    frame.extend_from_slice(&1u32.to_le_bytes());
    frame.extend_from_slice(&35u32.to_le_bytes());
    frame.extend_from_slice(&1u32.to_le_bytes());
    frame.resize(32, 0);
    frame.push(17);
    frame.resize(35, 0);
    frame.push(23);
    write_synthetic_nd2(&path, &frame);

    let mut reader = ImageReader::open(&path).unwrap();
    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("nd2_first_image_data_encoding"),
        Some(MetadataValue::String(value)) if value == "chunk_table_le32"
    ));
    assert!(matches!(
        md.get("nd2_image_data_payload_offsets"),
        Some(MetadataValue::String(value)) if value == "8"
    ));
    assert!(matches!(
        md.get("nd2_image_data_timestamps"),
        Some(MetadataValue::String(value)) if value == "2.5"
    ));
    assert!(matches!(
        md.get("nd2_image_data_chunk_tables"),
        Some(MetadataValue::String(value))
            if value == "plane=0:offset=8,entry_width=4,count=2,first_payload=32,payload_bytes=2"
    ));
    assert!(matches!(
        md.get("nd2_image_data_chunk_table_ranges"),
        Some(MetadataValue::String(value)) if value == "plane=0:32..33,35..36"
    ));

    assert_eq!(reader.open_bytes(0).unwrap(), vec![17, 23]);
}

#[test]
fn nd2_decodes_raw_frame_from_little_endian_u64_chunk_table() {
    let path = tmp("chunk_table_u64_frame.nd2");
    let mut frame = Vec::new();
    frame.extend_from_slice(&2u32.to_le_bytes());
    frame.extend_from_slice(&40u64.to_le_bytes());
    frame.extend_from_slice(&1u64.to_le_bytes());
    frame.extend_from_slice(&42u64.to_le_bytes());
    frame.extend_from_slice(&1u64.to_le_bytes());
    frame.extend_from_slice(b"gap!");
    frame.push(17);
    frame.push(0);
    frame.push(23);
    write_synthetic_nd2(&path, &frame);

    let mut reader = ImageReader::open(&path).unwrap();
    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("nd2_first_image_data_encoding"),
        Some(MetadataValue::String(value)) if value == "chunk_table_le64"
    ));
    assert!(matches!(
        md.get("nd2_image_data_encodings"),
        Some(MetadataValue::String(value)) if value == "chunk_table_le64"
    ));
    assert!(matches!(
        md.get("nd2_image_data_chunk_tables"),
        Some(MetadataValue::String(value))
            if value == "plane=0:offset=0,entry_width=8,count=2,first_payload=40,payload_bytes=2"
    ));
    assert!(matches!(
        md.get("nd2_image_data_chunk_table_ranges"),
        Some(MetadataValue::String(value)) if value == "plane=0:40..41,42..43"
    ));

    assert_eq!(reader.open_bytes(0).unwrap(), vec![17, 23]);
}

#[test]
fn nd2_decodes_raw_frame_from_4096_prefixed_little_endian_chunk_table() {
    let path = tmp("chunk_table_after_4096_prefix_frame.nd2");
    let mut frame = vec![0u8; 4096];
    frame.extend_from_slice(&2u32.to_le_bytes());
    frame.extend_from_slice(&4120u32.to_le_bytes());
    frame.extend_from_slice(&1u32.to_le_bytes());
    frame.extend_from_slice(&4123u32.to_le_bytes());
    frame.extend_from_slice(&1u32.to_le_bytes());
    frame.resize(4120, 0);
    frame.push(17);
    frame.resize(4123, 0);
    frame.push(23);
    write_synthetic_nd2(&path, &frame);

    let mut reader = ImageReader::open(&path).unwrap();
    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("nd2_first_image_data_encoding"),
        Some(MetadataValue::String(value)) if value == "chunk_table_le32"
    ));
    assert!(matches!(
        md.get("nd2_image_data_payload_offsets"),
        Some(MetadataValue::String(value)) if value == "4096"
    ));
    assert!(matches!(
        md.get("nd2_image_data_chunk_tables"),
        Some(MetadataValue::String(value))
            if value == "plane=0:offset=4096,entry_width=4,count=2,first_payload=4120,payload_bytes=2"
    ));
    assert!(matches!(
        md.get("nd2_image_data_chunk_table_ranges"),
        Some(MetadataValue::String(value)) if value == "plane=0:4120..4121,4123..4124"
    ));

    assert_eq!(reader.open_bytes(0).unwrap(), vec![17, 23]);
}

#[test]
fn nd2_routes_zlib_frame_from_little_endian_u64_chunk_table() {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let path = tmp("chunk_table_u64_zlib_frame.nd2");
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[31, 47]).unwrap();
    let compressed = encoder.finish().unwrap();
    let split = compressed.len() / 2;

    let mut frame = Vec::new();
    frame.extend_from_slice(&2u32.to_le_bytes());
    let first_offset = 40u64;
    let second_offset = first_offset + split as u64 + 3;
    frame.extend_from_slice(&first_offset.to_le_bytes());
    frame.extend_from_slice(&(split as u64).to_le_bytes());
    frame.extend_from_slice(&second_offset.to_le_bytes());
    frame.extend_from_slice(&((compressed.len() - split) as u64).to_le_bytes());
    frame.extend_from_slice(b"gap!");
    frame.extend_from_slice(&compressed[..split]);
    frame.extend_from_slice(b"pad");
    frame.extend_from_slice(&compressed[split..]);
    write_synthetic_nd2(&path, &frame);

    let mut reader = ImageReader::open(&path).unwrap();
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("nd2_first_image_data_encoding"),
        Some(MetadataValue::String(value)) if value == "chunk_table_le64_zlib"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![31, 47]);
}

#[test]
fn nd2_routes_zlib_frame_from_4096_prefixed_little_endian_chunk_table() {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let path = tmp("chunk_table_after_4096_prefix_zlib_frame.nd2");
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(&[31, 47]).unwrap();
    let compressed = encoder.finish().unwrap();
    let split = compressed.len() / 2;

    let mut frame = vec![0u8; 4096];
    frame.extend_from_slice(&2u32.to_le_bytes());
    let first_offset = 4120u32;
    let second_offset = first_offset + split as u32 + 3;
    frame.extend_from_slice(&first_offset.to_le_bytes());
    frame.extend_from_slice(&(split as u32).to_le_bytes());
    frame.extend_from_slice(&second_offset.to_le_bytes());
    frame.extend_from_slice(&((compressed.len() - split) as u32).to_le_bytes());
    frame.resize(first_offset as usize, 0);
    frame.extend_from_slice(&compressed[..split]);
    frame.extend_from_slice(b"pad");
    frame.extend_from_slice(&compressed[split..]);
    write_synthetic_nd2(&path, &frame);

    let mut reader = ImageReader::open(&path).unwrap();
    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("nd2_first_image_data_encoding"),
        Some(MetadataValue::String(value)) if value == "chunk_table_le32_zlib"
    ));
    assert!(matches!(
        md.get("nd2_image_data_payload_offsets"),
        Some(MetadataValue::String(value)) if value == "4096"
    ));
    assert!(matches!(
        md.get("nd2_image_data_chunk_tables"),
        Some(MetadataValue::String(value))
            if value.contains("plane=0:offset=4096,entry_width=4,count=2,first_payload=4120,payload_bytes=")
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![31, 47]);
}

#[test]
fn nd2_decodes_per_chunk_zlib_chunk_table() {
    use flate2::write::ZlibEncoder;
    use flate2::Compression;
    use std::io::Write;

    let path = tmp("chunk_table_per_chunk_zlib.nd2");
    let compress = |value: u8| {
        let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
        encoder.write_all(&[value]).unwrap();
        encoder.finish().unwrap()
    };
    let first = compress(17);
    let second = compress(23);

    let mut frame = Vec::new();
    frame.extend_from_slice(&2u32.to_le_bytes());
    let first_offset = 20u32;
    let second_offset = first_offset + first.len() as u32 + 3;
    frame.extend_from_slice(&first_offset.to_le_bytes());
    frame.extend_from_slice(&(first.len() as u32).to_le_bytes());
    frame.extend_from_slice(&second_offset.to_le_bytes());
    frame.extend_from_slice(&(second.len() as u32).to_le_bytes());
    frame.extend_from_slice(&first);
    frame.extend_from_slice(b"pad");
    frame.extend_from_slice(&second);
    write_synthetic_nd2(&path, &frame);

    let mut reader = ImageReader::open(&path).unwrap();
    let md = &reader.metadata().series_metadata;
    assert!(matches!(
        md.get("nd2_first_image_data_encoding"),
        Some(MetadataValue::String(value)) if value == "chunk_table_le32_per_chunk_zlib"
    ));
    assert!(matches!(
        md.get("nd2_image_data_encodings"),
        Some(MetadataValue::String(value)) if value == "chunk_table_le32_per_chunk_zlib"
    ));
    assert!(matches!(
        md.get("nd2_image_data_chunk_tables"),
        Some(MetadataValue::String(value))
            if value.contains("plane=0:offset=0,entry_width=4,count=2,first_payload=20,payload_bytes=")
    ));

    assert_eq!(reader.open_bytes(0).unwrap(), vec![17, 23]);
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

#[test]
fn nd2_extracts_xml_calibration_and_channel_metadata() {
    let path = tmp("xml_calibration_channels.nd2");
    let mut bytes = Vec::new();
    let attr_xml = br#"<?xml version="1.0"?>
<variant>
  <uiWidth value="1"/>
  <uiHeight value="1"/>
  <uiComp value="2"/>
  <uiBpc value="8"/>
</variant>"#;
    let calibration_xml = br#"<?xml version="1.0"?>
<variant>
  <dCalibration value="0.25"/>
  <dZStep value="1.5"/>
</variant>"#;
    let metadata_xml = br#"<?xml version="1.0"?>
<variant>
  <sDescription value="DAPI"/>
  <sDescription value="FITC"/>
  <EmWavelength value="460"/>
  <EmWavelength value="525"/>
</variant>"#;
    push_nd2_chunk(&mut bytes, "ImageAttributesLV!", attr_xml);
    push_nd2_chunk(&mut bytes, "ImageCalibrationLV!", calibration_xml);
    push_nd2_chunk(&mut bytes, "ImageMetadataSeqLV!", metadata_xml);
    push_nd2_chunk(&mut bytes, "ImageDataSeq|0!", &[3, 7]);
    std::fs::write(&path, bytes).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.open_bytes(0).unwrap(), vec![3, 7]);

    let ome = reader.ome_metadata().unwrap();
    let image = &ome.images[0];
    assert_eq!(image.physical_size_x, Some(0.25));
    assert_eq!(image.physical_size_y, Some(0.25));
    assert_eq!(image.physical_size_z, Some(1.5));
    assert_eq!(image.channels[0].name.as_deref(), Some("DAPI"));
    assert_eq!(image.channels[1].name.as_deref(), Some("FITC"));
    assert_eq!(image.channels[0].emission_wavelength, Some(460.0));
    assert_eq!(image.channels[1].emission_wavelength, Some(525.0));
}

fn push_jp2_box(bytes: &mut Vec<u8>, box_type: &[u8; 4], payload: &[u8]) {
    bytes.extend_from_slice(&((payload.len() as u32) + 8).to_be_bytes());
    bytes.extend_from_slice(box_type);
    bytes.extend_from_slice(payload);
}

fn push_tiff_entry(out: &mut Vec<u8>, tag: u16, ty: u16, count: u32, value: u32) {
    out.extend_from_slice(&tag.to_le_bytes());
    out.extend_from_slice(&ty.to_le_bytes());
    out.extend_from_slice(&count.to_le_bytes());
    out.extend_from_slice(&value.to_le_bytes());
}

fn write_minimal_lsm(path: &Path, dim_z: i32, dim_c: i32, dim_t: i32, data_type: i32) {
    let entry_count = 10u16;
    let ifd_start = 8u32;
    let ifd_end = ifd_start + 2 + entry_count as u32 * 12 + 4;
    let lsm_offset = ifd_end;
    let pixel_offset = lsm_offset + 64;

    let mut bytes = Vec::new();
    bytes.extend_from_slice(b"II");
    bytes.extend_from_slice(&42u16.to_le_bytes());
    bytes.extend_from_slice(&ifd_start.to_le_bytes());
    bytes.extend_from_slice(&entry_count.to_le_bytes());
    push_tiff_entry(&mut bytes, 256, 4, 1, 1);
    push_tiff_entry(&mut bytes, 257, 4, 1, 1);
    push_tiff_entry(&mut bytes, 258, 3, 1, 8);
    push_tiff_entry(&mut bytes, 259, 3, 1, 1);
    push_tiff_entry(&mut bytes, 262, 3, 1, 1);
    push_tiff_entry(&mut bytes, 273, 4, 1, pixel_offset);
    push_tiff_entry(&mut bytes, 277, 3, 1, 1);
    push_tiff_entry(&mut bytes, 278, 4, 1, 1);
    push_tiff_entry(&mut bytes, 279, 4, 1, 1);
    push_tiff_entry(&mut bytes, 34412, 7, 64, lsm_offset);
    bytes.extend_from_slice(&0u32.to_le_bytes());
    let mut lsm = [0u8; 64];
    lsm[0..4].copy_from_slice(&0x0030_0494i32.to_le_bytes());
    lsm[4..8].copy_from_slice(&64i32.to_le_bytes());
    lsm[8..12].copy_from_slice(&1i32.to_le_bytes());
    lsm[12..16].copy_from_slice(&1i32.to_le_bytes());
    lsm[16..20].copy_from_slice(&dim_z.to_le_bytes());
    lsm[20..24].copy_from_slice(&dim_c.to_le_bytes());
    lsm[24..28].copy_from_slice(&dim_t.to_le_bytes());
    lsm[28..32].copy_from_slice(&data_type.to_le_bytes());
    bytes.extend_from_slice(&lsm);
    bytes.push(7);
    std::fs::write(path, bytes).unwrap();
}

fn write_minimal_czi_directory(path: &Path, pixel_type: i32, include_entry: bool) {
    let dir_pos = 112u64;
    let entry_count = u32::from(include_entry);
    let entry_bytes = if include_entry { 256u64 } else { 0 };
    let used_size = 128 + entry_bytes;

    let mut bytes = vec![0u8; 32];
    bytes[..10].copy_from_slice(b"ZISRAWFILE");
    bytes.extend_from_slice(&[0u8; 80]);
    bytes[32 + 36..32 + 44].copy_from_slice(&dir_pos.to_le_bytes());
    let dir_start = bytes.len();
    bytes.resize(dir_start + 32, 0);
    bytes[dir_start..dir_start + 12].copy_from_slice(b"ZISRAWDIRECT");
    bytes[dir_start + 16..dir_start + 24].copy_from_slice(&used_size.to_le_bytes());
    bytes[dir_start + 24..dir_start + 32].copy_from_slice(&used_size.to_le_bytes());
    let hdr_start = bytes.len();
    bytes.resize(hdr_start + 128, 0);
    bytes[hdr_start..hdr_start + 4].copy_from_slice(&entry_count.to_le_bytes());
    if include_entry {
        let entry_start = bytes.len();
        bytes.resize(entry_start + 256, 0);
        bytes[entry_start + 2..entry_start + 6].copy_from_slice(&pixel_type.to_le_bytes());
        bytes[entry_start + 6..entry_start + 14].copy_from_slice(&0i64.to_le_bytes());
        bytes[entry_start + 28..entry_start + 32].copy_from_slice(&3i32.to_le_bytes());
        for (i, (name, size)) in [("X", 1i32), ("Y", 1i32), ("C", 1i32)].iter().enumerate() {
            let off = entry_start + 32 + i * 20;
            bytes[off..off + name.len()].copy_from_slice(name.as_bytes());
            bytes[off + 8..off + 12].copy_from_slice(&size.to_le_bytes());
            bytes[off + 16..off + 20].copy_from_slice(&size.to_le_bytes());
        }
    }
    std::fs::write(path, bytes).unwrap();
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
        // skipBytes(4) — single skip between bpp and `valid`, per
        // ZeissZVIReader.fillMetadataPass1.
        item.extend_from_slice(&[0u8; 4]);
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

#[test]
fn czi_lsm_xrm_zvi_reject_fake_metadata_before_initialization() {
    use std::io::Write;

    let mut czi = bioformats::formats::czi::CziReader::new();
    assert_eq!(czi.series_count(), 0);
    assert!(matches!(
        czi.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));
    let empty_czi = tmp("empty_directory.czi");
    write_minimal_czi_directory(&empty_czi, 0, false);
    let err = czi.set_id(&empty_czi).unwrap_err();
    assert!(
        err.to_string().contains("no subblocks"),
        "unexpected CZI empty-directory error: {err}"
    );
    let unknown_czi = tmp("unknown_pixel_type.czi");
    write_minimal_czi_directory(&unknown_czi, 99, true);
    let err = czi.set_id(&unknown_czi).unwrap_err();
    assert!(
        err.to_string().contains("unsupported pixel type code 99"),
        "unexpected CZI pixel-type error: {err}"
    );
    assert_eq!(czi.series_count(), 0);

    let mut lsm = bioformats::formats::lsm::LsmReader::new();
    assert_eq!(lsm.series_count(), 0);
    assert!(matches!(
        lsm.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));
    let zero_lsm = tmp("zero_z.lsm");
    write_minimal_lsm(&zero_lsm, 0, 1, 1, 1);
    let err = lsm.set_id(&zero_lsm).unwrap_err();
    assert!(
        err.to_string().contains("non-positive dimensions"),
        "unexpected LSM zero-dimension error: {err}"
    );
    let bad_dtype_lsm = tmp("unknown_dtype.lsm");
    write_minimal_lsm(&bad_dtype_lsm, 1, 1, 1, 9);
    let err = lsm.set_id(&bad_dtype_lsm).unwrap_err();
    assert!(
        err.to_string()
            .contains("unsupported CZ_LSMInfo DataType 9"),
        "unexpected LSM dtype error: {err}"
    );
    assert_eq!(lsm.series_count(), 0);

    let mut xrm = bioformats::formats::xrm::XrmReader::new();
    assert_eq!(xrm.series_count(), 0);
    assert!(matches!(
        xrm.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));
    let short_xrm = tmp("short_plane.txrm");
    {
        let mut comp = cfb::create(&short_xrm).unwrap();
        comp.create_storage_all("/ImageInfo").unwrap();
        comp.create_stream("/ImageInfo/ImageWidth")
            .unwrap()
            .write_all(&2i32.to_le_bytes())
            .unwrap();
        comp.create_stream("/ImageInfo/ImageHeight")
            .unwrap()
            .write_all(&2i32.to_le_bytes())
            .unwrap();
        comp.create_stream("/ImageInfo/DataType")
            .unwrap()
            .write_all(&3i32.to_le_bytes())
            .unwrap();
        comp.create_storage_all("/ImageData").unwrap();
        comp.create_stream("/ImageData/Image1")
            .unwrap()
            .write_all(&[1, 2, 3])
            .unwrap();
    }
    let err = xrm.set_id(&short_xrm).unwrap_err();
    assert!(
        err.to_string().contains("shorter than declared"),
        "unexpected XRM short-payload error: {err}"
    );
    assert_eq!(xrm.series_count(), 0);

    let mut zvi = bioformats::formats::zvi::ZviReader::new();
    assert_eq!(zvi.series_count(), 0);
    assert!(matches!(
        zvi.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));
    let bad_bpp_zvi = tmp("bad_bpp.zvi");
    {
        let mut comp = cfb::create(&bad_bpp_zvi).unwrap();
        comp.create_storage_all("/Image/Item(1)").unwrap();
        let mut item: Vec<u8> = Vec::new();
        item.extend_from_slice(&[0u8; 22]);
        item.extend_from_slice(&[0u8; 2]);
        let pad: i32 = 1100;
        let len_raw: i32 = pad + 28;
        item.extend_from_slice(&len_raw.to_le_bytes());
        item.extend_from_slice(&[0u8; 8]);
        item.extend_from_slice(&0i32.to_le_bytes());
        item.extend_from_slice(&0i32.to_le_bytes());
        item.extend_from_slice(&0i32.to_le_bytes());
        item.extend_from_slice(&[0u8; 4]);
        item.extend_from_slice(&0i32.to_le_bytes());
        item.extend_from_slice(&vec![0u8; pad as usize]);
        item.extend_from_slice(&[0u8; 10]);
        item.extend_from_slice(&[0u8; 4]);
        item.extend_from_slice(&1i32.to_le_bytes());
        item.extend_from_slice(&1i32.to_le_bytes());
        item.extend_from_slice(&[0u8; 4]);
        item.extend_from_slice(&4i32.to_le_bytes());
        item.extend_from_slice(&[0u8; 8]);
        item.extend_from_slice(&2i32.to_le_bytes());
        item.extend_from_slice(&[77u8, 0, 0, 0]);
        comp.create_stream("/Image/Item(1)/CONTENTS")
            .unwrap()
            .write_all(&item)
            .unwrap();
    }
    let err = zvi.set_id(&bad_bpp_zvi).unwrap_err();
    assert!(
        err.to_string()
            .contains("unsupported bytes-per-pixel value 4"),
        "unexpected ZVI bpp error: {err}"
    );
    assert_eq!(zvi.series_count(), 0);
}

#[test]
fn mias_metamorph_prairie_olympus_require_initialization_for_series() {
    let mut mias = bioformats::formats::mias::MiasReader::new();
    assert_eq!(mias.series_count(), 0);
    assert!(matches!(
        mias.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    let mut metamorph = bioformats::formats::metamorph::MetamorphReader::new();
    assert_eq!(metamorph.series_count(), 0);
    assert!(matches!(
        metamorph.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    let mut prairie = bioformats::formats::prairie::PrairieReader::new();
    assert_eq!(prairie.series_count(), 0);
    assert!(matches!(
        prairie.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    let mut olympus = bioformats::formats::olympus::OifReader::new();
    assert_eq!(olympus.series_count(), 0);
    assert!(matches!(
        olympus.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));
}

#[test]
fn mias_failed_reopen_clears_prior_series() {
    let dir = isolated_tmp_dir("mias_failed_reopen");
    let well = dir.join("Plate").join("Well0001");
    std::fs::create_dir_all(&well).unwrap();
    let good = well.join("mode1_z001_t001.tif");
    write_tiny_tiff_bytes(&good);
    let bad = dir.join("plain.tif");
    write_tiny_tiff_bytes(&bad);

    let mut reader = bioformats::formats::mias::MiasReader::new();
    reader.set_id(&good).unwrap();
    assert_eq!(reader.series_count(), 1);

    let err = reader.set_id(&bad).unwrap_err();
    assert!(
        err.to_string().contains("not a Well"),
        "unexpected MIAS error: {err}"
    );
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn metamorph_failed_reopen_clears_prior_series() {
    let dir = isolated_tmp_dir("metamorph_failed_reopen");
    let good = dir.join("good.stk");
    write_tiny_tiff_bytes(&good);
    let bad = dir.join("bad.stk");
    std::fs::write(&bad, b"not a tiff").unwrap();

    let mut reader = bioformats::formats::metamorph::MetamorphReader::new();
    reader.set_id(&good).unwrap();
    assert_eq!(reader.series_count(), 1);

    let err = reader.set_id(&bad).unwrap_err();
    assert!(
        err.to_string().contains("TIFF") || err.to_string().contains("tiff"),
        "unexpected MetaMorph error: {err}"
    );
    assert_eq!(reader.series_count(), 0);
    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::NotInitialized)
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn prairie_rejects_unreadable_companion_before_fake_metadata() {
    let dir = isolated_tmp_dir("prairie_bad_companion");
    let xml = dir.join("scan.xml");
    let bad_tiff = dir.join("bad.tif");
    std::fs::write(&bad_tiff, b"not a tiff").unwrap();
    std::fs::write(
        &xml,
        r#"<PVScan version="5.2">
<PVStateValue key="pixelsPerLine" value="2"/>
<PVStateValue key="linesPerFrame" value="2"/>
<PVStateValue key="bitDepth" value="8"/>
<Sequence type="ZSeries">
<Frame index="0">
<File channel="1" filename="bad.tif"/>
</Frame>
</Sequence>
</PVScan>"#,
    )
    .unwrap();

    let mut reader = bioformats::formats::prairie::PrairieReader::new();
    let err = reader.set_id(&xml).unwrap_err();
    assert!(
        err.to_string().contains("companion TIFF") && err.to_string().contains("could not be read"),
        "unexpected Prairie error: {err}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn olympus_rejects_missing_pty_and_clears_prior_series() {
    let dir = isolated_tmp_dir("olympus_missing_pty");
    let good = dir.join("good.oif");
    let good_companion = dir.join("good.files");
    std::fs::create_dir_all(&good_companion).unwrap();
    let tiff = good_companion.join("plane0.tif");
    write_tiny_tiff_bytes(&tiff);
    std::fs::write(
        good_companion.join("plane0.pty"),
        "[File Info]\nDataName=plane0.tif\n",
    )
    .unwrap();
    std::fs::write(
        &good,
        "[ProfileSaveInfo]\nIniFileName0=plane0.pty\n[Axis 0 Parameters Common]\nAxisCode=X\nMaxSize=1\n[Axis 1 Parameters Common]\nAxisCode=Y\nMaxSize=1\n[Reference Image Parameter]\nImageDepth=1\nValidBitCounts=8\n",
    )
    .unwrap();

    let bad = dir.join("bad.oif");
    std::fs::create_dir_all(dir.join("bad.files")).unwrap();
    std::fs::write(
        &bad,
        "[ProfileSaveInfo]\nIniFileName0=missing.pty\n[Axis 0 Parameters Common]\nAxisCode=X\nMaxSize=1\n[Axis 1 Parameters Common]\nAxisCode=Y\nMaxSize=1\n[Reference Image Parameter]\nImageDepth=1\nValidBitCounts=8\n",
    )
    .unwrap();

    let mut reader = bioformats::formats::olympus::OifReader::new();
    reader.set_id(&good).unwrap();
    assert_eq!(reader.series_count(), 1);
    let err = reader.set_id(&bad).unwrap_err();
    assert!(
        err.to_string().contains("referenced PTY file"),
        "unexpected Olympus error: {err}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_dir_all(dir);
}

// ---- raster formats --------------------------------------------------------

#[test]
fn simple_raster_readers_do_not_report_series_before_set_id() {
    let mut avi = bioformats::formats::avi::AviReader::new();
    assert_eq!(avi.series_count(), 0);
    assert!(matches!(
        avi.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));

    let mut png = bioformats::formats::png::PngReader::new();
    assert_eq!(png.series_count(), 0);
    assert!(matches!(
        png.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));

    let mut jpeg = bioformats::formats::jpeg::JpegReader::new();
    assert_eq!(jpeg.series_count(), 0);
    assert!(matches!(
        jpeg.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));

    let mut pcx = bioformats::formats::pcx::PcxReader::new();
    assert_eq!(pcx.series_count(), 0);
    assert!(matches!(
        pcx.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));

    let mut pnm = bioformats::formats::raster::pnm_reader();
    assert_eq!(pnm.series_count(), 0);
    assert!(matches!(
        pnm.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));

    let mut gif = bioformats::formats::raster::GifReader::new();
    assert_eq!(gif.series_count(), 0);
    assert!(matches!(
        gif.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));
}

fn riff_avi(chunks: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((chunks.len() + 4) as u32).to_le_bytes());
    out.extend_from_slice(b"AVI ");
    out.extend_from_slice(chunks);
    out
}

fn avi_chunk(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(kind);
    out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
    out.extend_from_slice(payload);
    if payload.len() % 2 != 0 {
        out.push(0);
    }
    out
}

#[test]
fn avi_rejects_missing_dimensions_and_bit_depth_instead_of_defaults() {
    let path = tmp("avi_missing_headers.avi");
    let frame = avi_chunk(b"00db", &[1, 2, 3, 4]);
    std::fs::write(&path, riff_avi(&frame)).unwrap();
    let mut reader = bioformats::formats::avi::AviReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        err.to_string().contains("dimensions"),
        "unexpected AVI missing dimension error: {err}"
    );
    assert_eq!(reader.series_count(), 0);

    let path = tmp("avi_missing_strf_bit_depth.avi");
    let mut avih = vec![0u8; 40];
    avih[16..20].copy_from_slice(&1u32.to_le_bytes());
    avih[32..36].copy_from_slice(&1u32.to_le_bytes());
    avih[36..40].copy_from_slice(&1u32.to_le_bytes());
    let mut chunks = avi_chunk(b"avih", &avih);
    chunks.extend_from_slice(&avi_chunk(b"00db", &[1, 2, 3, 4]));
    std::fs::write(&path, riff_avi(&chunks)).unwrap();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        err.to_string().contains("bit depth"),
        "unexpected AVI missing bit depth error: {err}"
    );
    assert_eq!(reader.series_count(), 0);
}

#[test]
fn pcx_rejects_short_bytes_per_line_before_decoding() {
    let path = tmp("pcx_short_bytes_per_line.pcx");
    let mut header = [0u8; 128];
    header[0] = 0x0a;
    header[1] = 5;
    header[3] = 8;
    header[8..10].copy_from_slice(&2i16.to_le_bytes());
    header[10..12].copy_from_slice(&1i16.to_le_bytes());
    header[65] = 1;
    header[66..68].copy_from_slice(&1u16.to_le_bytes());
    std::fs::write(&path, header).unwrap();
    let mut reader = bioformats::formats::pcx::PcxReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        err.to_string().contains("bytes-per-line"),
        "unexpected PCX bytes-per-line error: {err}"
    );
    assert_eq!(reader.series_count(), 0);
}

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

/// Build a synthetic 2-frame RGBA APNG by hand: a 2x2 default image (frame 0)
/// that is all red, plus a 2x2 second frame that is all green, composited at
/// (0,0). Chunk CRCs are computed with the same CRC32 the readers expect.
fn write_two_frame_apng(path: &Path) {
    fn crc(type_: &[u8], data: &[u8]) -> u32 {
        let mut c = flate2::Crc::new();
        c.update(type_);
        c.update(data);
        c.sum()
    }
    fn chunk(out: &mut Vec<u8>, type_: &[u8; 4], data: &[u8]) {
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(type_);
        out.extend_from_slice(data);
        out.extend_from_slice(&crc(type_, data).to_be_bytes());
    }
    // Zlib-deflate a 2x2 RGBA image (filter byte 0 per row).
    fn deflate_image(rgba: &[[u8; 4]; 4]) -> Vec<u8> {
        use flate2::write::ZlibEncoder;
        use flate2::Compression;
        use std::io::Write;
        let mut enc = ZlibEncoder::new(Vec::new(), Compression::default());
        // row 0: pixels 0,1
        enc.write_all(&[0]).unwrap();
        enc.write_all(&rgba[0]).unwrap();
        enc.write_all(&rgba[1]).unwrap();
        // row 1: pixels 2,3
        enc.write_all(&[0]).unwrap();
        enc.write_all(&rgba[2]).unwrap();
        enc.write_all(&rgba[3]).unwrap();
        enc.finish().unwrap()
    }

    let red = [255u8, 0, 0, 255];
    let green = [0u8, 255, 0, 255];
    let frame0 = [red, red, red, red];
    let frame1 = [green, green, green, green];

    let mut bytes = Vec::new();
    bytes.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]);

    // IHDR: 2x2, 8-bit, color type 6 (RGBA).
    let mut ihdr = Vec::new();
    ihdr.extend_from_slice(&2u32.to_be_bytes());
    ihdr.extend_from_slice(&2u32.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]);
    chunk(&mut bytes, b"IHDR", &ihdr);

    // acTL: 2 frames, 0 plays.
    let mut actl = Vec::new();
    actl.extend_from_slice(&2u32.to_be_bytes());
    actl.extend_from_slice(&0u32.to_be_bytes());
    chunk(&mut bytes, b"acTL", &actl);

    // fcTL frame 0 (seq 0): full 2x2 at (0,0).
    let mut fctl0 = Vec::new();
    fctl0.extend_from_slice(&0u32.to_be_bytes()); // sequence
    fctl0.extend_from_slice(&2u32.to_be_bytes()); // width
    fctl0.extend_from_slice(&2u32.to_be_bytes()); // height
    fctl0.extend_from_slice(&0u32.to_be_bytes()); // x
    fctl0.extend_from_slice(&0u32.to_be_bytes()); // y
    fctl0.extend_from_slice(&1u16.to_be_bytes()); // delay_num
    fctl0.extend_from_slice(&0u16.to_be_bytes()); // delay_den
    fctl0.push(0); // dispose
    fctl0.push(0); // blend
    chunk(&mut bytes, b"fcTL", &fctl0);

    // IDAT (default image = frame 0, red).
    chunk(&mut bytes, b"IDAT", &deflate_image(&frame0));

    // fcTL frame 1 (seq 1): full 2x2 at (0,0).
    let mut fctl1 = Vec::new();
    fctl1.extend_from_slice(&1u32.to_be_bytes());
    fctl1.extend_from_slice(&2u32.to_be_bytes());
    fctl1.extend_from_slice(&2u32.to_be_bytes());
    fctl1.extend_from_slice(&0u32.to_be_bytes());
    fctl1.extend_from_slice(&0u32.to_be_bytes());
    fctl1.extend_from_slice(&1u16.to_be_bytes());
    fctl1.extend_from_slice(&0u16.to_be_bytes());
    fctl1.push(0);
    fctl1.push(0);
    chunk(&mut bytes, b"fcTL", &fctl1);

    // fdAT (frame 1, green): 4-byte sequence number then the frame's zlib data.
    let mut fdat = Vec::new();
    fdat.extend_from_slice(&2u32.to_be_bytes()); // sequence
    fdat.extend_from_slice(&deflate_image(&frame1));
    chunk(&mut bytes, b"fdAT", &fdat);

    chunk(&mut bytes, b"IEND", &[]);
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
fn animated_png_reads_all_frames_as_image_stack() {
    // The APNGReader port reads every APNG frame as a separate timepoint
    // (sizeT == numFrames), compositing each frame onto the default image.
    let path = tmp("animated.apng");
    write_two_frame_apng(&path);

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_c, 4); // RGBA
    assert_eq!(meta.size_t, 2);
    assert_eq!(meta.image_count, 2);
    assert_eq!(meta.dimension_order, DimensionOrder::XYCTZ);
    assert_eq!(reader.series_count(), 1);

    // Frame 0 is the default image (all red), frame 1 is composited green.
    let frame0 = reader.open_bytes(0).unwrap();
    let frame1 = reader.open_bytes(1).unwrap();
    assert_eq!(frame0.len(), 2 * 2 * 4);
    assert_eq!(frame1.len(), 2 * 2 * 4);
    assert_eq!(&frame0[..4], &[255, 0, 0, 255]);
    assert_eq!(&frame1[..4], &[0, 255, 0, 255]);
    // Every pixel of each frame matches.
    for px in frame0.chunks_exact(4) {
        assert_eq!(px, &[255, 0, 0, 255]);
    }
    for px in frame1.chunks_exact(4) {
        assert_eq!(px, &[0, 255, 0, 255]);
    }
}

#[test]
fn animated_png_round_trip_two_frames() {
    // Write a 2-frame RGBA stack with ApngWriter, then read it back with
    // ApngReader and verify both frames survive.
    let path = tmp("roundtrip.apng");

    let meta = ImageMetadata {
        size_x: 2,
        size_y: 2,
        size_z: 1,
        size_c: 4,
        size_t: 2,
        pixel_type: PixelType::Uint8,
        bits_per_pixel: 8,
        image_count: 2,
        dimension_order: DimensionOrder::XYCTZ,
        is_rgb: true,
        is_interleaved: true,
        is_indexed: false,
        is_little_endian: false,
        resolution_count: 1,
        ..Default::default()
    };

    // Frame 0: blue; frame 1: white.
    let blue: Vec<u8> = std::iter::repeat([0u8, 0, 255, 255]).take(4).flatten().collect();
    let white: Vec<u8> = std::iter::repeat([255u8, 255, 255, 255]).take(4).flatten().collect();

    ImageWriter::save(&path, &meta, &[blue.clone(), white.clone()]).expect("apng write failed");

    let mut reader = ImageReader::open(&path).unwrap();
    let rmeta = reader.metadata();
    assert_eq!(rmeta.size_t, 2);
    assert_eq!(rmeta.image_count, 2);
    assert_eq!(rmeta.size_c, 4);

    let f0 = reader.open_bytes(0).unwrap();
    let f1 = reader.open_bytes(1).unwrap();
    assert_eq!(f0, blue);
    assert_eq!(f1, white);
}

#[test]
fn amira_ascii_rejects_malformed_or_short_planes() {
    let malformed = tmp("malformed_ascii.am");
    std::fs::write(
        &malformed,
        b"# AmiraMesh 3D ASCII 2.0\ndefine Lattice 2 1 1\nLattice { byte Data } @1\n@1\n1 bad\n",
    )
    .unwrap();
    let mut reader = bioformats::formats::amira::AmiraReader::new();
    reader.set_id(&malformed).unwrap();
    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        err.to_string().contains("non-integer sample"),
        "unexpected malformed Amira error: {err}"
    );

    let short = tmp("short_ascii.am");
    std::fs::write(
        &short,
        b"# AmiraMesh 3D ASCII 2.0\ndefine Lattice 2 1 1\nLattice { byte Data } @1\n@1\n1\n",
    )
    .unwrap();
    let mut reader = bioformats::formats::amira::AmiraReader::new();
    reader.set_id(&short).unwrap();
    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        err.to_string().contains("has 1 samples, expected 2"),
        "unexpected short Amira error: {err}"
    );
}

#[test]
fn amira_rejects_invalid_lattice_dimensions() {
    let mut uninit = bioformats::formats::amira::AmiraReader::new();
    assert_eq!(uninit.series_count(), 0);
    assert!(matches!(
        uninit.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));

    let path = tmp("bad_lattice.am");
    std::fs::write(
        &path,
        b"# AmiraMesh 3D ASCII 2.0\ndefine Lattice nope 1 1\nLattice { byte Data } @1\n@1\n1\n",
    )
    .unwrap();

    let mut reader = bioformats::formats::amira::AmiraReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        err.to_string().contains("invalid lattice width"),
        "unexpected Amira lattice error: {err}"
    );

    let zero = tmp("zero_lattice.am");
    std::fs::write(
        &zero,
        b"# AmiraMesh 3D ASCII 2.0\ndefine Lattice 0 1 1\nLattice { byte Data } @1\n@1\n1\n",
    )
    .unwrap();
    let err = reader.set_id(&zero).unwrap_err();
    assert!(
        err.to_string().contains("non-positive dimensions"),
        "unexpected Amira zero dimension error: {err}"
    );

    let unknown_type = tmp("unknown_lattice_type.am");
    std::fs::write(
        &unknown_type,
        b"# AmiraMesh 3D ASCII 2.0\ndefine Lattice 1 1 1\nLattice { complex Data } @1\n@1\n1\n",
    )
    .unwrap();
    let err = reader.set_id(&unknown_type).unwrap_err();
    assert!(
        err.to_string().contains("unsupported lattice data type"),
        "unexpected Amira type error: {err}"
    );

    let short_binary = tmp("short_binary.am");
    std::fs::write(
        &short_binary,
        b"# AmiraMesh 3D BINARY-LITTLE-ENDIAN 2.0\ndefine Lattice 2 1 1\nLattice { byte Data } @1\n@1\n7",
    )
    .unwrap();
    let err = reader.set_id(&short_binary).unwrap_err();
    assert!(
        err.to_string().contains("pixel payload is shorter"),
        "unexpected Amira short payload error: {err}"
    );
    assert_eq!(reader.series_count(), 0);
}

fn write_spider_header(path: &Path, nslice: f32, nrow: f32, iform: f32, nsam: f32, payload: &[u8]) {
    let mut data = vec![0u8; 256];
    data[0..4].copy_from_slice(&nslice.to_le_bytes());
    data[4..8].copy_from_slice(&nrow.to_le_bytes());
    data[16..20].copy_from_slice(&iform.to_le_bytes());
    data[44..48].copy_from_slice(&nsam.to_le_bytes());
    data[84..88].copy_from_slice(&256f32.to_le_bytes());
    data.extend_from_slice(payload);
    std::fs::write(path, data).unwrap();
}

#[test]
fn spider_rejects_invalid_dimensions_iform_and_short_payload() {
    let mut uninit = bioformats::formats::amira::SpiderReader::new();
    assert_eq!(uninit.series_count(), 0);
    assert!(matches!(
        uninit.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));

    let zero = tmp("zero.spi");
    write_spider_header(&zero, 1.0, 0.0, 1.0, 1.0, &[0; 4]);
    let mut reader = bioformats::formats::amira::SpiderReader::new();
    let err = reader.set_id(&zero).unwrap_err();
    assert!(
        err.to_string().contains("invalid NROW"),
        "unexpected Spider zero dimension error: {err}"
    );

    let bad_iform = tmp("bad_iform.spi");
    write_spider_header(&bad_iform, 1.0, 1.0, 99.0, 1.0, &[0; 4]);
    let err = reader.set_id(&bad_iform).unwrap_err();
    assert!(
        err.to_string().contains("unsupported IFORM"),
        "unexpected Spider IFORM error: {err}"
    );

    let short = tmp("short_payload.spi");
    write_spider_header(&short, 1.0, 1.0, 1.0, 2.0, &[0; 4]);
    let err = reader.set_id(&short).unwrap_err();
    assert!(
        err.to_string().contains("pixel payload is shorter"),
        "unexpected Spider short payload error: {err}"
    );
    assert_eq!(reader.series_count(), 0);
}

fn quicktime_atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&((payload.len() as u32) + 8).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

fn quicktime_full_atom(kind: &[u8; 4], version: u8, payload: &[u8]) -> Vec<u8> {
    let mut full_payload = Vec::new();
    full_payload.push(version);
    full_payload.extend_from_slice(&[0, 0, 0]);
    full_payload.extend_from_slice(payload);
    quicktime_atom(kind, &full_payload)
}

fn quicktime_mdhd_atom(timescale: u32, duration: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&timescale.to_be_bytes());
    payload.extend_from_slice(&duration.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes());
    payload.extend_from_slice(&0u16.to_be_bytes());
    quicktime_full_atom(b"mdhd", 0, &payload)
}

fn quicktime_stts_atom(entries: &[(u32, u32)]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for &(sample_count, sample_delta) in entries {
        payload.extend_from_slice(&sample_count.to_be_bytes());
        payload.extend_from_slice(&sample_delta.to_be_bytes());
    }
    quicktime_full_atom(b"stts", 0, &payload)
}

fn quicktime_ctts_atom(entries: &[(u32, u32)]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for &(sample_count, sample_offset) in entries {
        payload.extend_from_slice(&sample_count.to_be_bytes());
        payload.extend_from_slice(&sample_offset.to_be_bytes());
    }
    quicktime_full_atom(b"ctts", 0, &payload)
}

fn quicktime_elst_atom(entries: &[(u32, i32, i32)]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for &(segment_duration, media_time, media_rate_fixed) in entries {
        payload.extend_from_slice(&segment_duration.to_be_bytes());
        payload.extend_from_slice(&media_time.to_be_bytes());
        payload.extend_from_slice(&media_rate_fixed.to_be_bytes());
    }
    quicktime_full_atom(b"elst", 0, &payload)
}

struct QuickTimeTimingAtoms {
    mdhd: Option<Vec<u8>>,
    stts: Option<Vec<u8>>,
    ctts: Option<Vec<u8>>,
    elst: Option<Vec<u8>>,
}

fn quicktime_video_track_atom(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    samples: &[&[u8]],
    offsets: &[u32],
    use_stsc: bool,
) -> Vec<u8> {
    quicktime_video_track_atom_with_depth(codec, width, height, samples, offsets, use_stsc, 0)
}

fn quicktime_video_track_atom_with_depth(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    samples: &[&[u8]],
    offsets: &[u32],
    use_stsc: bool,
    depth: u16,
) -> Vec<u8> {
    quicktime_video_track_atom_with_timing(
        codec, width, height, samples, offsets, use_stsc, depth, None,
    )
}

fn quicktime_video_track_atom_with_timing(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    samples: &[&[u8]],
    offsets: &[u32],
    use_stsc: bool,
    depth: u16,
    timing: Option<&QuickTimeTimingAtoms>,
) -> Vec<u8> {
    let mut sample_entry = vec![0u8; 86];
    sample_entry[..4].copy_from_slice(&86u32.to_be_bytes());
    sample_entry[4..8].copy_from_slice(codec);
    sample_entry[14..16].copy_from_slice(&1u16.to_be_bytes());
    sample_entry[32..34].copy_from_slice(&width.to_be_bytes());
    sample_entry[34..36].copy_from_slice(&height.to_be_bytes());
    sample_entry[82..84].copy_from_slice(&depth.to_be_bytes());
    let mut stsd_payload = Vec::new();
    stsd_payload.extend_from_slice(&0u32.to_be_bytes());
    stsd_payload.extend_from_slice(&1u32.to_be_bytes());
    stsd_payload.extend_from_slice(&sample_entry);
    let stsd = quicktime_atom(b"stsd", &stsd_payload);

    let mut stsz_payload = Vec::new();
    stsz_payload.extend_from_slice(&0u32.to_be_bytes());
    stsz_payload.extend_from_slice(&0u32.to_be_bytes());
    stsz_payload.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for sample in samples {
        stsz_payload.extend_from_slice(&(sample.len() as u32).to_be_bytes());
    }
    let stsz = quicktime_atom(b"stsz", &stsz_payload);

    let mut stco_payload = Vec::new();
    stco_payload.extend_from_slice(&0u32.to_be_bytes());
    stco_payload.extend_from_slice(&(offsets.len() as u32).to_be_bytes());
    for offset in offsets {
        stco_payload.extend_from_slice(&offset.to_be_bytes());
    }
    let stco = quicktime_atom(b"stco", &stco_payload);

    let mut stbl_payload = Vec::new();
    stbl_payload.extend_from_slice(&stsd);
    stbl_payload.extend_from_slice(&stsz);
    stbl_payload.extend_from_slice(&stco);
    if use_stsc {
        let mut stsc_payload = Vec::new();
        stsc_payload.extend_from_slice(&0u32.to_be_bytes());
        stsc_payload.extend_from_slice(&1u32.to_be_bytes());
        stsc_payload.extend_from_slice(&1u32.to_be_bytes());
        stsc_payload.extend_from_slice(&1u32.to_be_bytes());
        stsc_payload.extend_from_slice(&1u32.to_be_bytes());
        stbl_payload.extend_from_slice(&quicktime_atom(b"stsc", &stsc_payload));
    }
    if let Some(timing) = timing {
        if let Some(stts) = &timing.stts {
            stbl_payload.extend_from_slice(stts);
        }
        if let Some(ctts) = &timing.ctts {
            stbl_payload.extend_from_slice(ctts);
        }
    }

    let mut mdia_payload = Vec::new();
    if let Some(timing) = timing {
        if let Some(mdhd) = &timing.mdhd {
            mdia_payload.extend_from_slice(mdhd);
        }
    }
    mdia_payload.extend_from_slice(&quicktime_atom(
        b"minf",
        &quicktime_atom(b"stbl", &stbl_payload),
    ));

    let mut trak_payload = Vec::new();
    if let Some(timing) = timing {
        if let Some(elst) = &timing.elst {
            trak_payload.extend_from_slice(&quicktime_atom(b"edts", elst));
        }
    }
    trak_payload.extend_from_slice(&quicktime_atom(b"mdia", &mdia_payload));
    quicktime_atom(b"trak", &trak_payload)
}

fn quicktime_test_movie(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    samples: &[&[u8]],
    use_stsc: bool,
) -> Vec<u8> {
    quicktime_test_movie_with_depth(codec, width, height, samples, use_stsc, 0)
}

fn quicktime_test_movie_with_depth(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    samples: &[&[u8]],
    use_stsc: bool,
    depth: u16,
) -> Vec<u8> {
    quicktime_test_movie_with_timing(codec, width, height, samples, use_stsc, depth, None)
}

fn quicktime_test_movie_with_timing(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    samples: &[&[u8]],
    use_stsc: bool,
    depth: u16,
    timing: Option<&QuickTimeTimingAtoms>,
) -> Vec<u8> {
    let mut ftyp = Vec::new();
    ftyp.extend_from_slice(b"qt  ");
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(b"qt  ");
    let ftyp = quicktime_atom(b"ftyp", &ftyp);

    let mut mdat_payload = Vec::new();
    let mut offsets = Vec::new();
    let mut next_offset = (ftyp.len() + 8) as u32;
    for sample in samples {
        offsets.push(next_offset);
        mdat_payload.extend_from_slice(sample);
        next_offset += sample.len() as u32;
    }
    let mdat = quicktime_atom(b"mdat", &mdat_payload);

    let moov = quicktime_atom(
        b"moov",
        &quicktime_video_track_atom_with_timing(
            codec, width, height, samples, &offsets, use_stsc, depth, timing,
        ),
    );

    let mut mov = Vec::new();
    mov.extend_from_slice(&ftyp);
    mov.extend_from_slice(&mdat);
    mov.extend_from_slice(&moov);
    mov
}

fn quicktime_test_movie_with_two_video_tracks(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    sample0: &[u8],
    sample1: &[u8],
) -> Vec<u8> {
    quicktime_test_movie_with_two_custom_video_tracks(
        codec, width, height, sample0, codec, width, height, sample1,
    )
}

fn quicktime_test_movie_with_two_custom_video_tracks(
    codec0: &[u8; 4],
    width0: u16,
    height0: u16,
    sample0: &[u8],
    codec1: &[u8; 4],
    width1: u16,
    height1: u16,
    sample1: &[u8],
) -> Vec<u8> {
    let mut ftyp = Vec::new();
    ftyp.extend_from_slice(b"qt  ");
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(b"qt  ");
    let ftyp = quicktime_atom(b"ftyp", &ftyp);

    let offset0 = (ftyp.len() + 8) as u32;
    let offset1 = offset0 + sample0.len() as u32;
    let mut mdat_payload = Vec::new();
    mdat_payload.extend_from_slice(sample0);
    mdat_payload.extend_from_slice(sample1);
    let mdat = quicktime_atom(b"mdat", &mdat_payload);

    let track0 = quicktime_video_track_atom(codec0, width0, height0, &[sample0], &[offset0], false);
    let track1 = quicktime_video_track_atom(codec1, width1, height1, &[sample1], &[offset1], false);
    let mut moov_payload = Vec::new();
    moov_payload.extend_from_slice(&track0);
    moov_payload.extend_from_slice(&track1);
    let moov = quicktime_atom(b"moov", &moov_payload);

    let mut mov = Vec::new();
    mov.extend_from_slice(&ftyp);
    mov.extend_from_slice(&mdat);
    mov.extend_from_slice(&moov);
    mov
}

#[test]
fn quicktime_reads_blind_uncompressed_rgb_samples() {
    let path = tmp("blind_raw.mov");
    let sample0 = [1u8, 2, 3, 4, 5, 6];
    let sample1 = [7u8, 8, 9, 10, 11, 12];
    let mov = quicktime_test_movie(b"raw ", 2, 1, &[&sample0, &sample1], false);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 1);
    assert!(reader.metadata().is_rgb);
    assert_eq!(reader.metadata().image_count, 2);
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("quicktime.sample_count"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        reader.metadata().series_metadata.get("quicktime.sample_sizes"),
        Some(MetadataValue::String(value)) if value == "6,6"
    ));
    assert!(matches!(
        reader.metadata().series_metadata.get("quicktime.chunk_offsets"),
        Some(MetadataValue::String(value)) if value == "28,34"
    ));
    assert!(matches!(
        reader.metadata().series_metadata.get("quicktime.sample_offsets"),
        Some(MetadataValue::String(value)) if value == "28,34"
    ));
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("quicktime.chunk_offset_table_type"),
        Some(MetadataValue::String(value)) if value == "stco"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), sample0);
    assert_eq!(reader.open_bytes(1).unwrap(), sample1);
    assert_eq!(
        reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(),
        vec![10, 11, 12]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_records_media_timescale_and_sample_durations() {
    let path = tmp("timed_raw.mov");
    let sample0 = [1u8, 2, 3];
    let sample1 = [4u8, 5, 6];
    let sample2 = [7u8, 8, 9];
    let timing = QuickTimeTimingAtoms {
        mdhd: Some(quicktime_mdhd_atom(600, 160)),
        stts: Some(quicktime_stts_atom(&[(2, 40), (1, 80)])),
        ctts: None,
        elst: None,
    };
    let mov = quicktime_test_movie_with_timing(
        b"raw ",
        1,
        1,
        &[&sample0, &sample1, &sample2],
        false,
        0,
        Some(&timing),
    );
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_t, 3);
    assert!(matches!(
        meta.series_metadata.get("quicktime.timescale"),
        Some(MetadataValue::Int(600))
    ));
    assert!(matches!(
        meta.series_metadata.get("quicktime.duration_ticks"),
        Some(MetadataValue::Int(160))
    ));
    assert!(matches!(
        meta.series_metadata.get("quicktime.duration_seconds"),
        Some(MetadataValue::Float(value)) if (*value - (160.0 / 600.0)).abs() < 1.0e-12
    ));
    assert!(matches!(
        meta.series_metadata.get("quicktime.stts.entries"),
        Some(MetadataValue::String(value)) if value == "2x40,1x80"
    ));
    assert!(matches!(
        meta.series_metadata.get("quicktime.stts.duration_ticks"),
        Some(MetadataValue::Int(160))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_media_time_ticks"),
        Some(MetadataValue::String(value)) if value == "0,40,80"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_presentation_time_ticks"),
        Some(MetadataValue::String(value)) if value == "0,40,80"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_presentation_time_seconds"),
        Some(MetadataValue::String(value)) if value == "0,0.06666666666666667,0.13333333333333333"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.average_frame_duration_seconds"),
        Some(MetadataValue::Float(value)) if (*value - (160.0 / 600.0 / 3.0)).abs() < 1.0e-12
    ));
    let ome = reader.ome_metadata().unwrap();
    assert_eq!(ome.images[0].planes.len(), 3);
    assert_eq!(ome.images[0].planes[0].the_t, 0);
    assert_eq!(ome.images[0].planes[1].the_t, 1);
    assert_eq!(ome.images[0].planes[2].the_t, 2);
    assert_eq!(ome.images[0].planes[0].delta_t, Some(0.0));
    assert!((ome.images[0].planes[1].delta_t.unwrap() - (40.0 / 600.0)).abs() < 1.0e-12);
    assert!((ome.images[0].planes[2].delta_t.unwrap() - (80.0 / 600.0)).abs() < 1.0e-12);
    let annotation = ome
        .annotations
        .iter()
        .find_map(|annotation| match annotation {
            OmeAnnotation::MapAnnotation {
                id,
                namespace,
                values,
            } if id.as_deref() == Some("Annotation:OriginalMetadata:0")
                && namespace.as_deref()
                    == Some("openmicroscopy.org/bioformats/original-metadata") =>
            {
                Some(values)
            }
            _ => None,
        })
        .expect("QuickTime original metadata annotation");
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "quicktime.codec" && value == "raw "));
    assert!(annotation
        .iter()
        .any(|(key, value)| key == "quicktime.sample_media_time_ticks" && value == "0,40,80"));
    assert_eq!(reader.open_bytes(2).unwrap(), sample2);
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_records_edit_list_metadata() {
    let path = tmp("edit_list.mov");
    let sample0 = [1u8, 2, 3];
    let sample1 = [4u8, 5, 6];
    let timing = QuickTimeTimingAtoms {
        mdhd: Some(quicktime_mdhd_atom(1000, 2000)),
        stts: Some(quicktime_stts_atom(&[(2, 1000)])),
        ctts: None,
        elst: Some(quicktime_elst_atom(&[
            (500, -1, 1 << 16),
            (1500, 1000, 1 << 16),
        ])),
    };
    let mov = quicktime_test_movie_with_timing(
        b"raw ",
        1,
        1,
        &[&sample0, &sample1],
        false,
        0,
        Some(&timing),
    );
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata.get("quicktime.edit_list.count"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata.get("quicktime.edit_list.entries"),
        Some(MetadataValue::String(value))
            if value == "duration=500,media_time=-1,rate=1;duration=1500,media_time=1000,rate=1"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(value)) if value == "applied_leading_empty_edits_single_normal_speed_media_segment"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_media_time_ticks"),
        Some(MetadataValue::String(value)) if value == "0,1000"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_presentation_time_ticks"),
        Some(MetadataValue::String(value)) if value == "-1000,0"
    ));
    let ome = reader.ome_metadata().unwrap();
    assert_eq!(ome.images[0].planes.len(), 2);
    assert_eq!(ome.images[0].planes[0].delta_t, Some(-1.0));
    assert_eq!(ome.images[0].planes[1].delta_t, Some(0.0));
    let mut reader = reader;
    assert_eq!(reader.open_bytes(0).unwrap(), sample0);
    assert_eq!(reader.open_bytes(1).unwrap(), sample1);
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_applies_composition_time_offsets() {
    let path = tmp("composition_offsets.mov");
    let sample0 = [1u8, 2, 3];
    let sample1 = [4u8, 5, 6];
    let sample2 = [7u8, 8, 9];
    let timing = QuickTimeTimingAtoms {
        mdhd: Some(quicktime_mdhd_atom(1000, 3000)),
        stts: Some(quicktime_stts_atom(&[(3, 1000)])),
        ctts: Some(quicktime_ctts_atom(&[(1, 500), (2, 0)])),
        elst: None,
    };
    let mov = quicktime_test_movie_with_timing(
        b"raw ",
        1,
        1,
        &[&sample0, &sample1, &sample2],
        false,
        0,
        Some(&timing),
    );
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata.get("quicktime.ctts.entries"),
        Some(MetadataValue::String(value)) if value == "1x500,2x0"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_composition_offset_ticks"),
        Some(MetadataValue::String(value)) if value == "500,0,0"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.ctts.presentation_status"),
        Some(MetadataValue::String(value)) if value == "applied"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_presentation_time_ticks"),
        Some(MetadataValue::String(value)) if value == "500,1000,2000"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_presentation_time_seconds"),
        Some(MetadataValue::String(value)) if value == "0.5,1,2"
    ));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_rejects_stts_sample_count_mismatch() {
    let path = tmp("bad_stts_count.mov");
    let sample = [1u8, 2, 3];
    let timing = QuickTimeTimingAtoms {
        mdhd: Some(quicktime_mdhd_atom(1000, 1000)),
        stts: Some(quicktime_stts_atom(&[(2, 1000)])),
        ctts: None,
        elst: None,
    };
    let mov = quicktime_test_movie_with_timing(b"raw ", 1, 1, &[&sample], false, 0, Some(&timing));
    std::fs::write(&path, mov).unwrap();

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("QuickTime with mismatched stts sample count unexpectedly opened"),
        Err(err) => err,
    };
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("stts sample count 2 does not match stsz sample count 1")),
        "unexpected QuickTime stts mismatch error: {err}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_rejects_ctts_sample_count_mismatch() {
    let path = tmp("bad_ctts_count.mov");
    let sample = [1u8, 2, 3];
    let timing = QuickTimeTimingAtoms {
        mdhd: Some(quicktime_mdhd_atom(1000, 1000)),
        stts: Some(quicktime_stts_atom(&[(1, 1000)])),
        ctts: Some(quicktime_ctts_atom(&[(2, 0)])),
        elst: None,
    };
    let mov = quicktime_test_movie_with_timing(b"raw ", 1, 1, &[&sample], false, 0, Some(&timing));
    std::fs::write(&path, mov).unwrap();

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("QuickTime with mismatched ctts sample count unexpectedly opened"),
        Err(err) => err,
    };
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("ctts sample count 2 does not match stsz sample count 1")),
        "unexpected QuickTime ctts mismatch error: {err}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_decodes_mjpeg_samples_with_stsc_chunk_table() {
    let path = tmp("mjpeg_stsc.mov");
    let rgb0 = [255u8, 0, 0, 0, 255, 0];
    let rgb1 = [0u8, 0, 255, 255, 255, 0];
    let mut jpeg0 = Vec::new();
    let mut jpeg1 = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg0, 100)
        .encode(&rgb0, 2, 1, image::ColorType::Rgb8.into())
        .unwrap();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg1, 100)
        .encode(&rgb1, 2, 1, image::ColorType::Rgb8.into())
        .unwrap();
    let mut dec0 = jpeg_decoder::Decoder::new(jpeg0.as_slice());
    let expected0 = dec0.decode().unwrap();
    let mut dec1 = jpeg_decoder::Decoder::new(jpeg1.as_slice());
    let expected1 = dec1.decode().unwrap();
    let mov = quicktime_test_movie(b"mjpg", 2, 1, &[&jpeg0, &jpeg1], true);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 1);
    assert!(reader.metadata().is_rgb);
    assert!(matches!(
        reader.metadata().series_metadata.get("quicktime.codec"),
        Some(MetadataValue::String(codec)) if codec == "mjpg"
    ));
    assert!(matches!(
        reader
            .metadata()
            .series_metadata
            .get("quicktime.stsc.entry_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        reader.metadata().series_metadata.get("quicktime.stsc.entries"),
        Some(MetadataValue::String(value))
            if value == "first_chunk=1,samples_per_chunk=1,sample_description_index=1"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), expected0);
    assert_eq!(reader.open_bytes(1).unwrap(), expected1);
    assert_eq!(
        reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(),
        expected1[3..6].to_vec()
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_decodes_png_samples() {
    use image::ImageEncoder;

    let path = tmp("png_codec.mov");
    let rgb = [10u8, 20, 30, 40, 50, 60];
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(&rgb, 2, 1, image::ColorType::Rgb8.into())
        .unwrap();
    let mov = quicktime_test_movie(b"png ", 2, 1, &[&png], false);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 1);
    assert!(reader.metadata().is_rgb);
    assert!(matches!(
        reader.metadata().series_metadata.get("quicktime.codec"),
        Some(MetadataValue::String(codec)) if codec == "png "
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), rgb);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 1, 1).unwrap(),
        vec![40, 50, 60]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_decodes_rpza_samples() {
    let path = tmp("rpza_codec.mov");
    let rpza = [
        0xe1, 0x00, 0x00, 0x07, // chunk marker and length
        0xa0, 0x7c, 0x00, // one RGB555 red 4x4 block
    ];
    let mov = quicktime_test_movie(b"rpza", 4, 4, &[&rpza], false);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 4);
    assert_eq!(reader.metadata().size_y, 4);
    assert!(reader.metadata().is_rgb);
    assert!(matches!(
        reader.metadata().series_metadata.get("quicktime.codec"),
        Some(MetadataValue::String(codec)) if codec == "rpza"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), vec![255, 0, 0].repeat(16));
    assert_eq!(
        reader.open_bytes_region(0, 1, 1, 2, 2).unwrap(),
        vec![255, 0, 0].repeat(4)
    );
    let _ = std::fs::remove_file(path);
}

fn quicktime_animation_rle_sample(width: usize, rows: &[&[[u8; 3]]]) -> Vec<u8> {
    let mut sample = Vec::new();
    sample.extend_from_slice(&0u32.to_be_bytes());
    sample.extend_from_slice(&0u16.to_be_bytes());
    for row in rows {
        assert_eq!(row.len(), width);
        sample.push(1);
        sample.push(row.len() as u8);
        for pixel in *row {
            sample.extend_from_slice(pixel);
        }
        sample.push(0xff);
    }
    let size = sample.len() as u32;
    sample[0..4].copy_from_slice(&size.to_be_bytes());
    sample
}

fn quicktime_animation_rle16_sample(width: usize, rows: &[&[u16]]) -> Vec<u8> {
    let mut sample = Vec::new();
    sample.extend_from_slice(&0u32.to_be_bytes());
    sample.extend_from_slice(&0u16.to_be_bytes());
    for row in rows {
        assert_eq!(row.len(), width);
        sample.push(1);
        sample.push(row.len() as u8);
        for pixel in *row {
            sample.extend_from_slice(&pixel.to_be_bytes());
        }
        sample.push(0xff);
    }
    let size = sample.len() as u32;
    sample[0..4].copy_from_slice(&size.to_be_bytes());
    sample
}

fn quicktime_animation_rle32_sample(width: usize, rows: &[&[[u8; 4]]]) -> Vec<u8> {
    let mut sample = Vec::new();
    sample.extend_from_slice(&0u32.to_be_bytes());
    sample.extend_from_slice(&0u16.to_be_bytes());
    for row in rows {
        assert_eq!(row.len(), width);
        sample.push(1);
        sample.push(row.len() as u8);
        for pixel in *row {
            sample.extend_from_slice(pixel);
        }
        sample.push(0xff);
    }
    let size = sample.len() as u32;
    sample[0..4].copy_from_slice(&size.to_be_bytes());
    sample
}

fn quicktime_animation_rle24_delta_sample(
    start_line: u16,
    line_count: u16,
    line_data: &[u8],
) -> Vec<u8> {
    let mut sample = Vec::new();
    sample.extend_from_slice(&0u32.to_be_bytes());
    sample.extend_from_slice(&0x0008u16.to_be_bytes());
    sample.extend_from_slice(&start_line.to_be_bytes());
    sample.extend_from_slice(&0u16.to_be_bytes());
    sample.extend_from_slice(&line_count.to_be_bytes());
    sample.extend_from_slice(&0u16.to_be_bytes());
    sample.extend_from_slice(line_data);
    let size = sample.len() as u32;
    sample[0..4].copy_from_slice(&size.to_be_bytes());
    sample
}

#[test]
fn quicktime_decodes_animation_rle_samples() {
    let path = tmp("animation_rle.mov");
    let row0 = [[1u8, 2, 3], [4, 5, 6], [7, 8, 9]];
    let row1 = [[10u8, 11, 12], [13, 14, 15], [16, 17, 18]];
    let rle = quicktime_animation_rle_sample(3, &[&row0, &row1]);
    let mov = quicktime_test_movie(b"rle ", 3, 2, &[&rle], false);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert!(reader.metadata().is_rgb);
    assert!(matches!(
        reader.metadata().series_metadata.get("quicktime.codec"),
        Some(MetadataValue::String(codec)) if codec == "rle "
    ));
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16, 17, 18]
    );
    assert_eq!(
        reader.open_bytes_region(0, 1, 1, 2, 1).unwrap(),
        vec![13, 14, 15, 16, 17, 18]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_decodes_animation_rle_delta_frames() {
    let path = tmp("animation_rle_delta_decode.mov");
    let row0 = [[1u8, 2, 3], [4, 5, 6], [7, 8, 9]];
    let row1 = [[10u8, 11, 12], [13, 14, 15], [16, 17, 18]];
    let keyframe = quicktime_animation_rle_sample(3, &[&row0, &row1]);

    let mut line = Vec::new();
    line.push(1);
    line.push(1);
    line.extend_from_slice(&[20, 21, 22]);
    line.push(0);
    line.push(2);
    line.push(1);
    line.extend_from_slice(&[30, 31, 32]);
    line.push(0xff);
    let delta = quicktime_animation_rle24_delta_sample(1, 1, &line);
    let mov = quicktime_test_movie(b"rle ", 3, 2, &[&keyframe, &delta], false);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(
        reader.open_bytes(1).unwrap(),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 20, 21, 22, 13, 14, 15, 30, 31, 32]
    );
    assert_eq!(
        reader.open_bytes_region(1, 1, 1, 1, 1).unwrap(),
        vec![13, 14, 15]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_decodes_animation_rle_argb32_samples() {
    let path = tmp("animation_rle_argb32.mov");
    let row0 = [[0u8, 1, 2, 3], [128, 4, 5, 6]];
    let row1 = [[255u8, 7, 8, 9], [64, 10, 11, 12]];
    let rle = quicktime_animation_rle32_sample(2, &[&row0, &row1]);
    let mov = quicktime_test_movie_with_depth(b"rle ", 2, 2, &[&rle], false, 32);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert!(reader.metadata().is_rgb);
    assert!(matches!(
        reader.metadata().series_metadata.get("quicktime.rle.depth"),
        Some(MetadataValue::Int(32))
    ));
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        vec![1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12]
    );
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
        vec![4, 5, 6, 10, 11, 12]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_decodes_animation_rle_rgb555_samples() {
    let path = tmp("animation_rle_rgb555.mov");
    let row0 = [0x7c00u16, 0x03e0u16];
    let row1 = [0x001fu16, 0x7fffu16];
    let rle = quicktime_animation_rle16_sample(2, &[&row0, &row1]);
    let mov = quicktime_test_movie_with_depth(b"rle ", 2, 2, &[&rle], false, 16);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert!(reader.metadata().is_rgb);
    assert!(matches!(
        reader.metadata().series_metadata.get("quicktime.rle.depth"),
        Some(MetadataValue::Int(16))
    ));
    assert_eq!(
        reader.open_bytes(0).unwrap(),
        vec![255, 0, 0, 0, 255, 0, 0, 0, 255, 255, 255, 255]
    );
    assert_eq!(
        reader.open_bytes_region(0, 1, 1, 1, 1).unwrap(),
        vec![255, 255, 255]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_animation_rle_rejects_partial_delta_frames() {
    let path = tmp("animation_rle_delta.mov");
    let mut sample = Vec::new();
    sample.extend_from_slice(&14u32.to_be_bytes());
    sample.extend_from_slice(&0x0008u16.to_be_bytes());
    sample.extend_from_slice(&1u16.to_be_bytes());
    sample.extend_from_slice(&0u16.to_be_bytes());
    sample.extend_from_slice(&1u16.to_be_bytes());
    sample.extend_from_slice(&0u16.to_be_bytes());
    let mov = quicktime_test_movie(b"rle ", 2, 2, &[&sample], false);
    std::fs::write(&path, mov).unwrap();

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("partial QuickTime Animation RLE frame unexpectedly opened"),
        Err(err) => err,
    };
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("partial/delta frame")),
        "unexpected QuickTime Animation RLE error: {err}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_animation_rle_rejects_unsupported_depth() {
    let path = tmp("animation_rle_depth8.mov");
    let sample = quicktime_animation_rle_sample(1, &[&[[1u8, 2, 3]]]);
    let mov = quicktime_test_movie_with_depth(b"rle ", 1, 1, &[&sample], false, 8);
    std::fs::write(&path, mov).unwrap();

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("unsupported QuickTime Animation RLE depth unexpectedly opened"),
        Err(err) => err,
    };
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Animation RLE depth 8 is unsupported")),
        "unexpected QuickTime Animation RLE depth error: {err}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_maps_compatible_video_tracks_to_series() {
    let path = tmp("multiple_video_tracks.mov");
    let sample0 = [1u8, 2, 3, 4, 5, 6];
    let sample1 = [7u8, 8, 9, 10, 11, 12];
    let mov = quicktime_test_movie_with_two_video_tracks(b"raw ", 2, 1, &sample0, &sample1);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.series_count(), 2);
    assert_eq!(reader.series(), 0);
    assert_eq!(reader.metadata().image_count, 1);
    assert!(
        matches!(
            reader
                .metadata()
                .series_metadata
                .get("quicktime.video_track_count"),
            Some(MetadataValue::Int(2))
        ),
        "missing QuickTime video track count"
    );
    assert!(
        matches!(
            reader
                .metadata()
                .series_metadata
                .get("quicktime.video_track_index"),
            Some(MetadataValue::Int(0))
        ),
        "missing QuickTime video track index for series 0"
    );
    assert_eq!(reader.open_bytes(0).unwrap(), sample0);

    reader.set_series(1).unwrap();
    assert_eq!(reader.series(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 1);
    assert_eq!(reader.metadata().image_count, 1);
    assert!(
        matches!(
            reader
                .metadata()
                .series_metadata
                .get("quicktime.video_track_index"),
            Some(MetadataValue::Int(1))
        ),
        "missing QuickTime video track index for series 1"
    );
    assert_eq!(reader.open_bytes(0).unwrap(), sample1);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 1, 1).unwrap(),
        vec![10, 11, 12]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_rejects_incompatible_multiple_video_tracks() {
    let path = tmp("incompatible_video_tracks.mov");
    let sample0 = [1u8, 2, 3, 4, 5, 6];
    let sample1 = [7u8, 8, 9];
    let mov = quicktime_test_movie_with_two_custom_video_tracks(
        b"raw ", 2, 1, &sample0, b"raw ", 1, 1, &sample1,
    );
    std::fs::write(&path, mov).unwrap();

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("incompatible multi-video-track QuickTime unexpectedly opened"),
        Err(err) => err,
    };
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("multiple incompatible video tracks")),
        "unexpected QuickTime multi-track incompatibility error: {err}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_reports_unsupported_codec_fourcc() {
    let path = tmp("unsupported_codec.mov");
    let sample = [0u8; 6];
    let mov = quicktime_test_movie(b"avc1", 2, 1, &[&sample], false);
    std::fs::write(&path, mov).unwrap();

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("unsupported QuickTime codec unexpectedly opened"),
        Err(err) => err,
    };
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("QuickTime codec avc1 is unsupported")),
        "unexpected QuickTime unsupported codec error: {err}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn paletted_tga_keeps_indices_with_lookup_table() {
    // The faithful TargaReader port (matching Java) reports color-mapped images
    // as 8-bit indexed data with a separate color map, rather than expanding
    // the palette to RGB samples.
    let path = tmp("palette.tga");
    write_paletted_tga(&path);

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata().clone();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 1);
    assert_eq!(meta.size_c, 1);
    assert!(meta.is_indexed);
    assert!(!meta.is_rgb);
    // raw palette indices for the two pixels
    assert_eq!(reader.open_bytes(0).unwrap(), vec![0, 1]);
    // color map reconstructs RGB (red, green)
    let lut = meta.lookup_table.expect("indexed image exposes a lookup table");
    assert_eq!(lut.red, vec![255, 0]);
    assert_eq!(lut.green, vec![0, 255]);
    assert_eq!(lut.blue, vec![0, 0]);
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
