use bioformats::{
    BioFormatsError, FormatReader, FormatWriter, ImageMetadata, ImageReader, ImageWriter,
    MetadataValue, PixelType,
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

fn append_ptu_tag(out: &mut Vec<u8>, ident: &str, tag_type: u32, value: i64) {
    let mut tag = [0u8; 48];
    let ident_bytes = ident.as_bytes();
    tag[..ident_bytes.len().min(32)].copy_from_slice(&ident_bytes[..ident_bytes.len().min(32)]);
    tag[32..36].copy_from_slice(&(-1i32).to_le_bytes());
    tag[36..40].copy_from_slice(&tag_type.to_le_bytes());
    tag[40..48].copy_from_slice(&value.to_le_bytes());
    out.extend_from_slice(&tag);
}

fn minimal_ptu_header(tags: impl FnOnce(&mut Vec<u8>)) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"PQTTTR\0\0");
    data.extend_from_slice(b"1.0\0\0\0\0\0");
    tags(&mut data);
    append_ptu_tag(&mut data, "Header_End", 0xffff_0008, 0);
    data
}

fn sm_camera_bytes(width: u16, height: u16, pixels: &[u8]) -> Vec<u8> {
    let mut data = vec![0u8; 548];
    data[..16].copy_from_slice(&[0, 0, 0, 0, 2, 0, 0, 5, 0xc9, 0x88, 0, 5, 0xcb, 0x88, 0, 0]);
    data[524..526].copy_from_slice(&height.to_be_bytes());
    data[532..534].copy_from_slice(&width.to_be_bytes());
    data.extend_from_slice(pixels);
    data
}

fn blind_opus_iss_bytes(
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

fn strict_extended_raw_bytes(
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
fn picoquant_ptu_parses_unified_header_dimensions_without_decoding_events() {
    let path = tmp("minimal.ptu");
    let data = minimal_ptu_header(|out| {
        append_ptu_int_tag(out, "ImgHdr_PixX", 7);
        append_ptu_int_tag(out, "ImgHdr_PixY", 5);
        append_ptu_int_tag(out, "ImgHdr_Frame", 3);
        append_ptu_int_tag(out, "TTResult_NumberOfRecords", 0);
    });
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::spm::PicoQuantReader::new();
    reader.set_id(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 7);
    assert_eq!(meta.size_y, 5);
    assert_eq!(meta.size_z, 1);
    assert_eq!(meta.size_c, 1);
    assert_eq!(meta.size_t, 3);
    assert_eq!(meta.image_count, 3);
    assert_eq!(meta.pixel_type, PixelType::Uint32);
    assert!(matches!(
        meta.series_metadata.get("ptu.ImgHdr_PixX"),
        Some(MetadataValue::Int(7))
    ));
    assert!(matches!(
        meta.series_metadata.get("ptu.TTResult_NumberOfRecords"),
        Some(MetadataValue::Int(0))
    ));

    let err = reader.open_bytes(0).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("event-stream image reconstruction is unsupported") && message.contains("explicit image dimensions"))
    );
    let err = reader.open_bytes_region(1, 0, 0, 1, 1).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("not decoded to image planes"))
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
        "Width=3\nHeight=2\nBands=1\nSlices=1\nFrames=1\nDatatype=2\n",
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
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("embedded VWS native payload decoding is unsupported")),
        "{err:?}"
    );

    let fake = dir.join("fake.vws");
    std::fs::write(&fake, b"fake").unwrap();
    let mut reader = bioformats::formats::lim::TillVisionReader::new();
    let err = reader.set_id(&fake).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("embedded VWS native payload decoding is unsupported")),
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
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("embedded VWS native payload decoding is unsupported")),
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
fn lambert_flim_reads_strict_blind_ascii_subset() {
    let path = tmp("blind_lambert.asc");
    std::fs::write(
        &path,
        "BFLAMBERT_ASCII_V1\nWidth=3\nHeight=2\nFrames=2\nPixelType=uint8\nDataHex=0102030405060b0c0d0e0f10\n",
    )
    .unwrap();

    let mut reader = bioformats::formats::simfcs::LambertFlimReader::new();
    assert!(reader.is_this_type_by_bytes(b"Lambert GlobalImages"));
    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_t, 2);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
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
fn lambert_flim_rejects_malformed_blind_ascii_before_metadata() {
    let path = tmp("bad_lambert.asc");
    std::fs::write(
        &path,
        "BFLAMBERT_ASCII_V1\nWidth=2\nHeight=2\nFrames=1\nPixelType=uint8\nDataHex=010203\n",
    )
    .unwrap();

    let mut reader = bioformats::formats::simfcs::LambertFlimReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("payload length")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_file(path);
}

#[test]
fn lambert_flim_accepts_obvious_blind_ascii_aliases() {
    let path = tmp("blind_lambert_aliases.asc");
    std::fs::write(
        &path,
        "BFLAMBERT_ASCII_V1\nSizeX=2\nSizeY=2\nFrameCount=1\nType=uint16le\nDATA_HEX\n0201040306050807\n",
    )
    .unwrap();

    let mut reader = bioformats::formats::simfcs::LambertFlimReader::new();
    reader.set_id(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_t, 1);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
        vec![0x04, 0x03, 0x08, 0x07]
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn lambert_flim_rejects_nonmatching_fake_native_payload() {
    let path = tmp("fake_native_lambert.asc");
    std::fs::write(&path, b"Lambert GlobalImages fake native payload").unwrap();

    let mut reader = bioformats::formats::simfcs::LambertFlimReader::new();
    assert!(reader.is_this_type_by_bytes(b"Lambert GlobalImages"));
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("native/fallback decoding is not supported")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_file(path);
}

fn woolz_blind_raw_bytes(
    width: u32,
    height: u32,
    planes: u32,
    pixel_type: u16,
    pixels: &[u8],
) -> Vec<u8> {
    let mut data = Vec::new();
    data.extend_from_slice(b"BFWOOLZRAW0001\0\0");
    data.extend_from_slice(&width.to_le_bytes());
    data.extend_from_slice(&height.to_le_bytes());
    data.extend_from_slice(&planes.to_le_bytes());
    data.extend_from_slice(&pixel_type.to_le_bytes());
    data.extend_from_slice(&1u16.to_le_bytes());
    data.extend_from_slice(&48u32.to_le_bytes());
    data.extend_from_slice(&[0u8; 12]);
    data.extend_from_slice(pixels);
    data
}

#[test]
fn woolz_reads_strict_blind_raw_subset() {
    let path = tmp("blind.wlz");
    let pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
    std::fs::write(&path, woolz_blind_raw_bytes(2, 2, 2, 1, &pixels)).unwrap();

    let mut reader = bioformats::formats::legacy::WoolzReader::new();
    assert!(reader.is_this_type_by_bytes(b"BFWOOLZRAW0001\0\0extra"));
    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().size_z, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4]);
    assert_eq!(reader.open_bytes_region(1, 1, 0, 1, 2).unwrap(), vec![6, 8]);
    assert!(matches!(
        reader.open_bytes(2),
        Err(BioFormatsError::PlaneOutOfRange(2))
    ));

    let _ = std::fs::remove_file(path);
}

#[test]
fn woolz_rejects_malformed_blind_raw_before_metadata() {
    let path = tmp("bad.wlz");
    std::fs::write(&path, woolz_blind_raw_bytes(2, 2, 1, 1, &[1, 2, 3])).unwrap();

    let mut reader = bioformats::formats::legacy::WoolzReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("payload length")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_file(path);
}

#[test]
fn woolz_preserves_unsupported_for_nonmatching_data() {
    let path = tmp("unsupported.wlz");
    std::fs::write(&path, b"Woolz proprietary object payload").unwrap();

    let mut reader = bioformats::formats::legacy::WoolzReader::new();
    assert!(!reader.is_this_type_by_bytes(b"Woolz proprietary object payload"));
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("native object decoding")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);

    let _ = std::fs::remove_file(path);
}

#[test]
fn woolz_rejects_non_fixed_blind_raw_payload_offset() {
    let path = tmp("bad_offset.wlz");
    let mut data = woolz_blind_raw_bytes(2, 2, 1, 1, &[1, 2, 3, 4]);
    data[32..36].copy_from_slice(&52u32.to_le_bytes());
    std::fs::write(&path, data).unwrap();

    let mut reader = bioformats::formats::legacy::WoolzReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("fixed header length")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);

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

#[test]
fn cellworx_strict_raw_sidecar_opens_planes_and_regions() {
    let dir = isolated_tmp_dir("cellworx_strict_raw");
    let htd = dir.join("strict.htd");
    let raw = dir.join("strict.raw");
    std::fs::write(&raw, [1u8, 2, 3, 4, 5, 6, 11, 12, 13, 14, 15, 16]).unwrap();
    std::fs::write(
        &htd,
        b"BF_CELLWORX_RAW_V1\nXSites,2\nYSites,1\nRawWidth,3\nRawHeight,2\nRawPixelType,uint8\nRawFile,strict.raw\n",
    )
    .unwrap();

    let mut reader = bioformats::formats::mias::CellWorxReader::new();
    reader.set_id(&htd).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 3);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint8);
    assert_eq!(reader.open_bytes(0).unwrap(), vec![1, 2, 3, 4, 5, 6]);
    assert_eq!(reader.open_bytes(1).unwrap(), vec![11, 12, 13, 14, 15, 16]);
    assert_eq!(
        reader.open_bytes_region(1, 1, 0, 2, 2).unwrap(),
        vec![12, 13, 15, 16]
    );
    assert!(matches!(
        reader.open_bytes(2),
        Err(BioFormatsError::PlaneOutOfRange(2))
    ));

    let _ = std::fs::remove_dir_all(dir);
}

#[test]
fn cellworx_strict_raw_rejects_unsafe_or_short_sidecars() {
    let dir = isolated_tmp_dir("cellworx_strict_raw_errors");
    let escaped = dir.join("escaped.htd");
    std::fs::write(
        &escaped,
        b"BF_CELLWORX_RAW_V1\nXSites,1\nYSites,1\nRawWidth,1\nRawHeight,1\nRawPixelType,uint8\nRawFile,../escape.raw\n",
    )
    .unwrap();
    let mut reader = bioformats::formats::mias::CellWorxReader::new();
    let err = reader.set_id(&escaped).unwrap_err();
    assert!(err.to_string().contains("must stay"));

    let short = dir.join("short.htd");
    std::fs::write(dir.join("short.raw"), [1u8, 2, 3]).unwrap();
    std::fs::write(
        &short,
        b"BF_CELLWORX_RAW_V1\nXSites,1\nYSites,1\nRawWidth,2\nRawHeight,2\nRawPixelType,uint8\nRawFile,short.raw\n",
    )
    .unwrap();
    let err = reader.set_id(&short).unwrap_err();
    assert!(err.to_string().contains("payload length"));

    let _ = std::fs::remove_dir_all(dir);
}

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
            "raw.mod",
            Box::new(bioformats::formats::sem::ImrodReader::new()),
        ),
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
            "strict.mod",
            b"BIOFORMATS-RS-IMROD-STRICT-RAW-V1\n",
            Box::new(bioformats::formats::sem::ImrodReader::new()),
        ),
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

#[test]
fn opus_iss_guessed_header_readers_reject_raw_files() {
    let cases: Vec<(&str, Box<dyn FormatReader>, &str)> = vec![
        (
            "raw.abs",
            Box::new(bioformats::formats::opus::BrukerOpusReader::new()),
            "Bruker OPUS native spectral image decoding is unsupported",
        ),
        (
            "raw.iss",
            Box::new(bioformats::formats::opus::IssFlimReader::new()),
            "ISS Vista FLIM native decoding is unsupported",
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
            "Bruker OPUS native spectral image decoding is unsupported",
        ),
        (
            "registry_raw.0",
            b"\x0a\x01\0\0\x04\0\0\0not real".as_slice(),
            "Bruker OPUS native spectral image decoding is unsupported",
        ),
        (
            "registry_raw.iss",
            b"not real".as_slice(),
            "ISS Vista FLIM native decoding is unsupported",
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
fn bruker_opus_reads_strict_blind_raw_subset() {
    let path = tmp("blind_opus.abs");
    let plane0 = vec![1u8, 2, 3, 4, 5, 6];
    let plane1 = vec![11u8, 12, 13, 14, 15, 16];
    let mut payload = plane0.clone();
    payload.extend_from_slice(&plane1);
    std::fs::write(
        &path,
        blind_opus_iss_bytes(b"BFOPUS1\0", 3, 2, 2, 1, &payload),
    )
    .unwrap();

    let mut reader = bioformats::formats::opus::BrukerOpusReader::new();
    assert!(reader.is_this_type_by_bytes(b"BFOPUS1\0extra"));
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
fn iss_flim_reads_strict_blind_u16_subset() {
    let path = tmp("blind_iss.iss");
    let pixels: Vec<u16> = vec![0x0102, 0x0304, 0x0506, 0x0708];
    let payload: Vec<u8> = pixels.iter().flat_map(|v| v.to_le_bytes()).collect();
    std::fs::write(
        &path,
        blind_opus_iss_bytes(b"BFISSFL1", 2, 2, 1, 2, &payload),
    )
    .unwrap();

    let mut reader = bioformats::formats::opus::IssFlimReader::new();
    assert!(reader.is_this_type_by_bytes(b"BFISSFL1extra"));
    reader.set_id(&path).unwrap();
    assert_eq!(reader.series_count(), 1);
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 2);
    assert_eq!(reader.metadata().pixel_type, PixelType::Uint16);
    assert_eq!(reader.open_bytes(0).unwrap(), payload);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
        vec![0x04, 0x03, 0x08, 0x07]
    );

    let _ = std::fs::remove_file(path);
}

#[test]
fn opus_iss_blind_subsets_reject_truncated_and_malformed_inputs_before_metadata() {
    let cases: Vec<(&str, Box<dyn FormatReader>, Vec<u8>, &str)> = vec![
        (
            "truncated_opus.abs",
            Box::new(bioformats::formats::opus::BrukerOpusReader::new()),
            b"BFOPUS1\0short".to_vec(),
            "header is truncated",
        ),
        (
            "zero_iss.iss",
            Box::new(bioformats::formats::opus::IssFlimReader::new()),
            blind_opus_iss_bytes(b"BFISSFL1", 0, 2, 1, 1, &[1, 2]),
            "dimensions must be non-zero",
        ),
        (
            "short_payload.iss",
            Box::new(bioformats::formats::opus::IssFlimReader::new()),
            blind_opus_iss_bytes(b"BFISSFL1", 2, 2, 1, 1, &[1, 2, 3]),
            "payload is truncated",
        ),
    ];

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
    let pds_path =
        write_pds_fixture("pds_crop.hdr", '+', '+', 1, 3, 2, 4, &[10, 20, 30, 40, 50, 60]);
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

    let csv_path = tmp("crop.csv");
    std::fs::write(&csv_path, b"1 2 3\n4 5 6\n").unwrap();
    let mut csv = bioformats::formats::misc4::TextImageReader::new();
    csv.set_id(&csv_path).unwrap();
    assert_eq!(csv.series_count(), 1);
    csv.set_series(0).unwrap();
    let mut expected = Vec::new();
    for value in [2.0f32, 3.0, 5.0, 6.0] {
        expected.extend_from_slice(&value.to_le_bytes());
    }
    assert_eq!(csv.open_bytes_region(0, 1, 0, 2, 2).unwrap(), expected);
}

#[test]
fn text_table_readers_reject_ragged_or_nonnumeric_rows() {
    let csv_path = tmp("ragged.csv");
    std::fs::write(&csv_path, b"1 2 3\n4 5\n").unwrap();
    let mut csv = bioformats::formats::misc4::TextImageReader::new();
    let err = csv.set_id(&csv_path).unwrap_err();
    assert!(
        err.to_string().contains("inconsistent column counts"),
        "unexpected CSV error: {err}"
    );

    let txt_path = tmp("ragged.txt");
    std::fs::write(&txt_path, b"1,2\n3,4,5\n").unwrap();
    let mut txt = bioformats::formats::misc::TextReader::new();
    let err = txt.set_id(&txt_path).unwrap_err();
    assert!(
        err.to_string().contains("inconsistent column counts"),
        "unexpected text error: {err}"
    );

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

    let csv_valid = tmp("valid_then_bad.csv");
    std::fs::write(&csv_valid, b"1 2\n").unwrap();
    let csv_invalid = tmp("bad_reopen.csv");
    std::fs::write(&csv_invalid, b"1 nope\n").unwrap();
    let mut csv = bioformats::formats::misc4::TextImageReader::new();
    csv.set_id(&csv_valid).unwrap();
    assert_eq!(csv.series_count(), 1);
    let _ = csv.set_id(&csv_invalid).unwrap_err();
    assert_eq!(csv.series_count(), 0);
    assert_eq!(csv.metadata().size_x, 0);

    for path in [
        arf_valid,
        arf_invalid,
        pds_valid,
        pds_invalid,
        his_valid,
        his_invalid,
        csv_valid,
        csv_invalid,
    ] {
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn misc4_readers_report_not_initialized_for_preinit_set_series() {
    let mut readers: Vec<Box<dyn FormatReader>> = vec![
        Box::new(bioformats::formats::misc4::AplReader::new()),
        Box::new(bioformats::formats::misc4::ArfReader::new()),
        Box::new(bioformats::formats::misc4::I2iReader::new()),
        Box::new(bioformats::formats::misc4::JdceReader::new()),
        Box::new(bioformats::formats::misc4::PciReader::new()),
        Box::new(bioformats::formats::misc4::PdsReader::new()),
        Box::new(bioformats::formats::misc4::HisReader::new()),
        Box::new(bioformats::formats::misc4::HrdgdfReader::new()),
        Box::new(bioformats::formats::misc4::TextImageReader::new()),
        Box::new(bioformats::formats::misc4::FilePatternReaderStub::new()),
        Box::new(bioformats::formats::misc4::KlbReader::new()),
        Box::new(bioformats::formats::misc4::ObfReader::new()),
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
    let cases: Vec<(&str, Box<dyn FormatReader>, [u8; 8])> = vec![
        (
            "strict.i2i",
            Box::new(bioformats::formats::misc4::I2iReader::new()),
            *b"BFI2I\0\0\0",
        ),
        (
            "strict.jdce",
            Box::new(bioformats::formats::misc4::JdceReader::new()),
            *b"BFJDCE\0\0",
        ),
        (
            "strict.pci",
            Box::new(bioformats::formats::misc4::PciReader::new()),
            *b"BFPCI\0\0\0",
        ),
        (
            "strict.apl",
            Box::new(bioformats::formats::misc4::AplReader::new()),
            *b"BFAPL\0\0\0",
        ),
        (
            "strict.gdf",
            Box::new(bioformats::formats::misc4::HrdgdfReader::new()),
            *b"BFGDF\0\0\0",
        ),
        (
            "strict.klb",
            Box::new(bioformats::formats::misc4::KlbReader::new()),
            *b"BFKLB\0\0\0",
        ),
        (
            "strict.pattern",
            Box::new(bioformats::formats::misc4::FilePatternReaderStub::new()),
            *b"BFPATT\0\0",
        ),
        (
            "strict.obf",
            Box::new(bioformats::formats::misc4::ObfReader::new()),
            *b"BFOBF\0\0\0",
        ),
    ];

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
    let cases: Vec<(&str, Box<dyn FormatReader>, Vec<u8>, &str)> = vec![
        (
            "short.i2i",
            Box::new(bioformats::formats::misc4::I2iReader::new()),
            b"BFI2I\0\0\0short".to_vec(),
            "header is truncated",
        ),
        (
            "zero.jdce",
            Box::new(bioformats::formats::misc4::JdceReader::new()),
            strict_misc4_raw_bytes(b"BFJDCE\0\0", 0, 2, 1, 1, &[1, 2]),
            "dimensions must be non-zero",
        ),
        (
            "truncated.pci",
            Box::new(bioformats::formats::misc4::PciReader::new()),
            strict_misc4_raw_bytes(b"BFPCI\0\0\0", 2, 2, 1, 1, &[1, 2, 3]),
            "payload is truncated",
        ),
        (
            "reserved.apl",
            Box::new(bioformats::formats::misc4::AplReader::new()),
            {
                let mut data = strict_misc4_raw_bytes(b"BFAPL\0\0\0", 2, 2, 1, 1, &[1, 2, 3, 4]);
                data[22] = 1;
                data
            },
            "reserved header bytes must be zero",
        ),
        (
            "offset.gdf",
            Box::new(bioformats::formats::misc4::HrdgdfReader::new()),
            {
                let mut data = strict_misc4_raw_bytes(b"BFGDF\0\0\0", 2, 2, 1, 1, &[1, 2, 3, 4]);
                data[24..32].copy_from_slice(&31u64.to_le_bytes());
                data
            },
            "data offset points into header",
        ),
        (
            "pixel_type.klb",
            Box::new(bioformats::formats::misc4::KlbReader::new()),
            strict_misc4_raw_bytes(b"BFKLB\0\0\0", 2, 2, 1, 99, &[1, 2, 3, 4]),
            "unsupported pixel type code 99",
        ),
        (
            "truncated.pattern",
            Box::new(bioformats::formats::misc4::FilePatternReaderStub::new()),
            strict_misc4_raw_bytes(b"BFPATT\0\0", 2, 2, 1, 1, &[1, 2, 3]),
            "payload is truncated",
        ),
        (
            "zero.obf",
            Box::new(bioformats::formats::misc4::ObfReader::new()),
            strict_misc4_raw_bytes(b"BFOBF\0\0\0", 2, 0, 1, 1, &[1, 2]),
            "dimensions must be non-zero",
        ),
    ];

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

#[test]
fn misc4_obf_fallback_rejects_imspector_magic() {
    let path = tmp("imspector_magic_for_misc4.obf");
    std::fs::write(&path, b"OMAS_BF\n").unwrap();

    let mut reader = bioformats::formats::misc4::ObfReader::new();
    assert!(!reader.is_this_type_by_bytes(b"OMAS_BF\n"));
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("OBF fallback synthetic raw")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);
    assert_eq!(reader.metadata().size_x, 0);

    let _ = std::fs::remove_file(path);
}

#[test]
fn misc_remaining_placeholders_read_strict_raw_subsets() {
    let cases: Vec<(&str, Box<dyn FormatReader>, [u8; 16])> = vec![
        (
            "strict.acff",
            Box::new(bioformats::formats::misc::VolocityLibraryReader::new()),
            *b"BFVOLOCITYACFF01",
        ),
        (
            "strict.mng",
            Box::new(bioformats::formats::misc::MngReader::new()),
            *b"BFMNGSTRICTRAW01",
        ),
        (
            "strict.sld",
            Box::new(bioformats::formats::misc::SlideBookReader::new()),
            *b"BFSLIDEBOOKRAW1!",
        ),
        (
            "strict.liff",
            Box::new(bioformats::formats::misc::OpenlabLiffReader::new()),
            *b"BFOPENLABLIFFRAW",
        ),
        (
            "strict.sedat",
            Box::new(bioformats::formats::misc::SedatReader::new()),
            *b"BFSEDATLABRAW01!",
        ),
    ];

    for (name, mut reader, magic) in cases {
        let path = tmp(name);
        let plane0 = vec![1u8, 2, 3, 4, 5, 6];
        let plane1 = vec![11u8, 12, 13, 14, 15, 16];
        let mut payload = plane0.clone();
        payload.extend_from_slice(&plane1);
        std::fs::write(&path, strict_misc_raw_bytes(&magic, 3, 2, 2, 1, &payload)).unwrap();

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
        let err = reader.open_bytes_region(0, 2, 1, 2, 1).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains("outside image bounds")),
            "{name}: {err:?}"
        );
        assert!(matches!(
            reader.open_bytes(2),
            Err(BioFormatsError::PlaneOutOfRange(2))
        ));

        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn misc_strict_raw_subsets_reject_malformed_or_nonmatching_inputs() {
    let cases: Vec<(&str, Box<dyn FormatReader>, Vec<u8>, &str)> = vec![
        (
            "bad_pixel.acff",
            Box::new(bioformats::formats::misc::VolocityLibraryReader::new()),
            strict_misc_raw_bytes(b"BFVOLOCITYACFF01", 2, 2, 1, 99, &[1, 2, 3, 4]),
            "unsupported pixel type",
        ),
        (
            "reserved.mng",
            Box::new(bioformats::formats::misc::MngReader::new()),
            {
                let mut data =
                    strict_misc_raw_bytes(b"BFMNGSTRICTRAW01", 2, 2, 1, 1, &[1, 2, 3, 4]);
                data[30] = 1;
                data
            },
            "reserved header bytes must be zero",
        ),
        (
            "short.sld",
            Box::new(bioformats::formats::misc::SlideBookReader::new()),
            b"BFSLIDEBOOKRAW1!short".to_vec(),
            "header is truncated",
        ),
        (
            "zero.liff",
            Box::new(bioformats::formats::misc::OpenlabLiffReader::new()),
            strict_misc_raw_bytes(b"BFOPENLABLIFFRAW", 0, 2, 1, 1, &[1, 2]),
            "dimensions must be non-zero",
        ),
        (
            "extra.sedat",
            Box::new(bioformats::formats::misc::SedatReader::new()),
            strict_misc_raw_bytes(b"BFSEDATLABRAW01!", 2, 2, 1, 1, &[1, 2, 3, 4, 5]),
            "payload length mismatch",
        ),
    ];

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

    let path = tmp("realish.sld");
    std::fs::write(&path, b"SlideBook 6 proprietary binary").unwrap();
    let mut reader = bioformats::formats::misc::SlideBookReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(matches!(err, BioFormatsError::UnsupportedFormat(_)));
    assert_eq!(reader.series_count(), 0);
    let _ = std::fs::remove_file(path);

    let path = tmp("realish.mng");
    std::fs::write(&path, b"\x8aMNG\r\n\x1a\n").unwrap();
    let mut reader = bioformats::formats::misc::MngReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("MNG strict raw native decoding is unsupported")),
        "{err:?}"
    );
    assert_eq!(reader.series_count(), 0);
    assert_eq!(reader.metadata().size_x, 0);
    let _ = std::fs::remove_file(path);

    for (name, bytes) in [
        (
            "ole.acff",
            vec![
                0xd0, 0xcf, 0x11, 0xe0, 0xa1, 0xb1, 0x1a, 0xe1, 0, 0, 0, 0, 0, 0, 0, 0,
            ],
        ),
        (
            "proprietary.acff",
            b"Volocity Library proprietary OLE payload".to_vec(),
        ),
        ("short_nonmatching.acff", b"acff".to_vec()),
    ] {
        let path = tmp(name);
        std::fs::write(&path, bytes).unwrap();
        let mut reader = bioformats::formats::misc::VolocityLibraryReader::new();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Volocity Library native decoding is unsupported")),
            "{name}: {err:?}"
        );
        assert_eq!(reader.series_count(), 0, "{name}");
        assert_eq!(reader.metadata().size_x, 0, "{name}");
        let _ = std::fs::remove_file(path);
    }
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
fn extended_naf_burleigh_and_leica_lof_read_strict_raw_subsets() {
    let cases: Vec<(&str, Box<dyn FormatReader>, [u8; 16])> = vec![
        (
            "strict.naf",
            Box::new(bioformats::formats::extended::NafReader::new()),
            *b"BFNAFSTRICTRAW01",
        ),
        (
            "strict.img",
            Box::new(bioformats::formats::extended::BurleighReader::new()),
            *b"BFBURLEIGHRAW001",
        ),
        (
            "strict.lof",
            Box::new(bioformats::formats::extended::LeicaLofReader::new()),
            *b"BFLEICALOFRAW001",
        ),
    ];

    for (name, mut reader, magic) in cases {
        let path = tmp(name);
        let plane0 = vec![1u8, 2, 3, 4, 5, 6];
        let plane1 = vec![11u8, 12, 13, 14, 15, 16];
        let mut payload = plane0.clone();
        payload.extend_from_slice(&plane1);
        std::fs::write(
            &path,
            strict_extended_raw_bytes(&magic, 3, 2, 2, 1, &payload),
        )
        .unwrap();

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
fn extended_strict_raw_subsets_reject_malformed_inputs_before_metadata() {
    let cases: Vec<(&str, Box<dyn FormatReader>, Vec<u8>, &str)> = vec![
        (
            "short.naf",
            Box::new(bioformats::formats::extended::NafReader::new()),
            b"BFNAFSTRICTRAW01short".to_vec(),
            "header is truncated",
        ),
        (
            "reserved.img",
            Box::new(bioformats::formats::extended::BurleighReader::new()),
            {
                let mut data =
                    strict_extended_raw_bytes(b"BFBURLEIGHRAW001", 2, 2, 1, 1, &[1, 2, 3, 4]);
                data[30] = 1;
                data
            },
            "reserved header bytes must be zero",
        ),
        (
            "zero.naf",
            Box::new(bioformats::formats::extended::NafReader::new()),
            strict_extended_raw_bytes(b"BFNAFSTRICTRAW01", 0, 2, 1, 1, &[1, 2]),
            "dimensions must be non-zero",
        ),
        (
            "short_payload.img",
            Box::new(bioformats::formats::extended::BurleighReader::new()),
            strict_extended_raw_bytes(b"BFBURLEIGHRAW001", 2, 2, 1, 1, &[1, 2, 3]),
            "payload length mismatch",
        ),
        (
            "bad_pixel.naf",
            Box::new(bioformats::formats::extended::NafReader::new()),
            strict_extended_raw_bytes(b"BFNAFSTRICTRAW01", 2, 2, 1, 99, &[1, 2, 3, 4]),
            "unsupported pixel type code 99",
        ),
        (
            "offset.lof",
            Box::new(bioformats::formats::extended::LeicaLofReader::new()),
            {
                let mut data =
                    strict_extended_raw_bytes(b"BFLEICALOFRAW001", 2, 2, 1, 1, &[1, 2, 3, 4]);
                data[32..40].copy_from_slice(&39u64.to_le_bytes());
                data
            },
            "data offset points into header",
        ),
    ];

    for (name, mut reader, bytes, expected) in cases {
        let path = tmp(name);
        std::fs::write(&path, bytes).unwrap();
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::Format(ref message) if message.contains(expected))
                || matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains(expected)),
            "{name}: {err:?}"
        );
        assert_eq!(reader.series_count(), 0, "{name}");
        assert_eq!(reader.metadata().size_x, 0, "{name}");
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn extended_naf_burleigh_and_leica_preserve_unsupported_for_nonmatching_native_data() {
    let cases: Vec<(&str, Box<dyn FormatReader>, Vec<u8>, &str)> = vec![
        (
            "native.naf",
            Box::new(bioformats::formats::extended::NafReader::new()),
            b"NAF proprietary payload".to_vec(),
            "NAF native payload decoding is unsupported",
        ),
        (
            "native.img",
            Box::new(bioformats::formats::extended::BurleighReader::new()),
            b"Burleigh SPM proprietary payload".to_vec(),
            "Burleigh SPM native payload decoding is unsupported",
        ),
        (
            "native.lof",
            Box::new(bioformats::formats::extended::LeicaLofReader::new()),
            b"\0\0LMS_Object_File\0payload".to_vec(),
            "Leica LOF native payload decoding is unsupported",
        ),
    ];

    for (name, mut reader, bytes, expected) in cases {
        let path = tmp(name);
        std::fs::write(&path, bytes).unwrap();
        assert!(!reader.is_this_type_by_bytes(b"not-strict"));
        let err = reader.set_id(&path).unwrap_err();
        assert!(
            matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains(expected)),
            "{name}: {err:?}"
        );
        assert_eq!(reader.series_count(), 0, "{name}");
        assert_eq!(reader.metadata().size_x, 0, "{name}");
        let _ = std::fs::remove_file(path);
    }
}

#[test]
fn extended_hamamatsu_vms_validates_index_then_reports_native_payload_unsupported() {
    let path = tmp("native.vms");
    std::fs::write(
        &path,
        b"NoLayers=1\nImageFile=tile.jpg\nPhysicalWidth=2\nPhysicalHeight=2\n",
    )
    .unwrap();

    let mut reader = bioformats::formats::extended::HamamatsuVmsReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("Hamamatsu VMS/VMU native JPEG tile payload decoding is unsupported")),
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
    let err = reader.open_bytes_region(0, 2, 0, 2, 1).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::Format(ref message) if message.contains("outside image bounds")),
        "{err:?}"
    );
}

#[test]
fn bdv_rejects_short_dataset_instead_of_zero_filling_plane() {
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
        r#"<SpimData><SequenceDescription><ViewSetups><ViewSetup><size>2 1 1</size></ViewSetup></ViewSetups></SequenceDescription></SpimData>"#,
    )
    .unwrap();

    let mut reader = bioformats::formats::bdv::BdvReader::new();
    let err = reader.set_id(&path).unwrap_err();
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("does not match declared")),
        "{err:?}"
    );
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
    let err = uninit.set_id(&path).unwrap_err();
    assert!(
        err.to_string().contains("no timepoint groups found"),
        "unexpected BDV error: {err}"
    );
    assert_eq!(uninit.series_count(), 0);

    std::fs::write(
        &xml_path,
        r#"<SpimData><ViewSetup><size>0 2 1</size></ViewSetup></SpimData>"#,
    )
    .unwrap();
    let err = uninit.set_id(&path).unwrap_err();
    assert!(
        err.to_string().contains("non-positive size axis"),
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
    file.add_fixed_ascii_attr("experiment_name", "synthetic assay", "synthetic assay".len())
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

#[test]
fn mng_is_explicit_unsupported_instead_of_delegating_to_png() {
    let path = tmp("unsupported.mng");
    std::fs::write(&path, b"\x8aMNG\r\n\x1a\n").unwrap();

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("MNG should be explicitly unsupported"),
        Err(err) => err,
    };

    assert!(
        matches!(&err, BioFormatsError::UnsupportedFormat(message) if message.contains("MNG strict raw native decoding is unsupported")),
        "unexpected error: {err}"
    );
}

#[test]
fn quicktime_reads_blind_uncompressed_rgb_samples() {
    fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&((payload.len() as u32) + 8).to_be_bytes());
        out.extend_from_slice(kind);
        out.extend_from_slice(payload);
        out
    }

    let path = tmp("blind_raw.mov");
    let sample0 = [1u8, 2, 3, 4, 5, 6];
    let sample1 = [7u8, 8, 9, 10, 11, 12];

    let mut ftyp = Vec::new();
    ftyp.extend_from_slice(b"qt  ");
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(b"qt  ");
    let ftyp = atom(b"ftyp", &ftyp);

    let mut mdat_payload = Vec::new();
    mdat_payload.extend_from_slice(&sample0);
    mdat_payload.extend_from_slice(&sample1);
    let first_sample_offset = (ftyp.len() + 8) as u32;
    let second_sample_offset = first_sample_offset + sample0.len() as u32;
    let mdat = atom(b"mdat", &mdat_payload);

    let mut sample_entry = vec![0u8; 86];
    sample_entry[..4].copy_from_slice(&86u32.to_be_bytes());
    sample_entry[4..8].copy_from_slice(b"raw ");
    sample_entry[14..16].copy_from_slice(&1u16.to_be_bytes());
    sample_entry[32..34].copy_from_slice(&2u16.to_be_bytes());
    sample_entry[34..36].copy_from_slice(&1u16.to_be_bytes());
    let mut stsd_payload = Vec::new();
    stsd_payload.extend_from_slice(&0u32.to_be_bytes());
    stsd_payload.extend_from_slice(&1u32.to_be_bytes());
    stsd_payload.extend_from_slice(&sample_entry);
    let stsd = atom(b"stsd", &stsd_payload);

    let mut stsz_payload = Vec::new();
    stsz_payload.extend_from_slice(&0u32.to_be_bytes());
    stsz_payload.extend_from_slice(&0u32.to_be_bytes());
    stsz_payload.extend_from_slice(&2u32.to_be_bytes());
    stsz_payload.extend_from_slice(&(sample0.len() as u32).to_be_bytes());
    stsz_payload.extend_from_slice(&(sample1.len() as u32).to_be_bytes());
    let stsz = atom(b"stsz", &stsz_payload);

    let mut stco_payload = Vec::new();
    stco_payload.extend_from_slice(&0u32.to_be_bytes());
    stco_payload.extend_from_slice(&2u32.to_be_bytes());
    stco_payload.extend_from_slice(&first_sample_offset.to_be_bytes());
    stco_payload.extend_from_slice(&second_sample_offset.to_be_bytes());
    let stco = atom(b"stco", &stco_payload);

    let mut stbl_payload = Vec::new();
    stbl_payload.extend_from_slice(&stsd);
    stbl_payload.extend_from_slice(&stsz);
    stbl_payload.extend_from_slice(&stco);
    let moov = atom(
        b"moov",
        &atom(
            b"trak",
            &atom(b"mdia", &atom(b"minf", &atom(b"stbl", &stbl_payload))),
        ),
    );

    let mut mov = Vec::new();
    mov.extend_from_slice(&ftyp);
    mov.extend_from_slice(&mdat);
    mov.extend_from_slice(&moov);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    assert_eq!(reader.metadata().size_x, 2);
    assert_eq!(reader.metadata().size_y, 1);
    assert!(reader.metadata().is_rgb);
    assert_eq!(reader.metadata().image_count, 2);
    assert_eq!(reader.open_bytes(0).unwrap(), sample0);
    assert_eq!(reader.open_bytes(1).unwrap(), sample1);
    assert_eq!(
        reader.open_bytes_region(1, 1, 0, 1, 1).unwrap(),
        vec![10, 11, 12]
    );
    let _ = std::fs::remove_file(path);
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
