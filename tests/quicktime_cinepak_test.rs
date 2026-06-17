use bioformats::{BioFormatsError, ImageReader, MetadataValue};
use image::ImageEncoder;

fn tmp(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "bioformats_quicktime_cinepak_{}_{}",
        std::process::id(),
        name
    ))
}

fn atom(kind: &[u8; 4], payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.extend_from_slice(&((payload.len() as u32) + 8).to_be_bytes());
    out.extend_from_slice(kind);
    out.extend_from_slice(payload);
    out
}

fn quicktime_movie(codec: &[u8; 4], width: u16, height: u16, depth: u16, sample: &[u8]) -> Vec<u8> {
    quicktime_movie_samples(codec, width, height, depth, &[sample])
}

fn quicktime_movie_samples(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    depth: u16,
    samples: &[&[u8]],
) -> Vec<u8> {
    quicktime_movie_samples_with_timing(codec, width, height, depth, samples, None, None)
}

fn quicktime_movie_samples_with_timing(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    depth: u16,
    samples: &[&[u8]],
    timing: Option<(u32, u32)>,
    edit_list: Option<&[(u32, i32, i32)]>,
) -> Vec<u8> {
    let mut ftyp = Vec::new();
    ftyp.extend_from_slice(b"qt  ");
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(b"qt  ");
    let ftyp = atom(b"ftyp", &ftyp);

    let mut mdat_payload = Vec::new();
    for sample in samples {
        mdat_payload.extend_from_slice(sample);
    }
    let mdat = atom(b"mdat", &mdat_payload);

    let mut sample_offsets = Vec::with_capacity(samples.len());
    let mut sample_offset = (ftyp.len() + 8) as u32;
    for sample in samples {
        sample_offsets.push(sample_offset);
        sample_offset += sample.len() as u32;
    }
    let trak = quicktime_track_with_timing(
        codec,
        width,
        height,
        depth,
        samples,
        &sample_offsets,
        timing,
        edit_list,
    );
    let mut moov_payload = Vec::new();
    if let Some((timescale, sample_delta)) = timing {
        moov_payload.extend_from_slice(&quicktime_mvhd(
            timescale,
            sample_delta * samples.len() as u32,
        ));
    }
    moov_payload.extend_from_slice(&trak);
    let moov = atom(b"moov", &moov_payload);

    let mut mov = Vec::new();
    mov.extend_from_slice(&ftyp);
    mov.extend_from_slice(&mdat);
    mov.extend_from_slice(&moov);
    mov
}

fn quicktime_movie_samples_with_co64(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    depth: u16,
    samples: &[&[u8]],
) -> Vec<u8> {
    let mut ftyp = Vec::new();
    ftyp.extend_from_slice(b"qt  ");
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(b"qt  ");
    let ftyp = atom(b"ftyp", &ftyp);

    let mut mdat_payload = Vec::new();
    for sample in samples {
        mdat_payload.extend_from_slice(sample);
    }
    let mdat = atom(b"mdat", &mdat_payload);

    let mut sample_offsets = Vec::with_capacity(samples.len());
    let mut sample_offset = (ftyp.len() + 8) as u64;
    for sample in samples {
        sample_offsets.push(sample_offset);
        sample_offset += sample.len() as u64;
    }
    let trak = quicktime_track_with_timing_u64_offsets(
        codec,
        width,
        height,
        depth,
        samples,
        &sample_offsets,
        true,
        None,
        None,
    );
    let moov = atom(b"moov", &trak);

    let mut mov = Vec::new();
    mov.extend_from_slice(&ftyp);
    mov.extend_from_slice(&mdat);
    mov.extend_from_slice(&moov);
    mov
}

fn quicktime_track(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    depth: u16,
    samples: &[&[u8]],
    sample_offsets: &[u32],
) -> Vec<u8> {
    quicktime_track_with_timing(
        codec,
        width,
        height,
        depth,
        samples,
        sample_offsets,
        None,
        None,
    )
}

fn quicktime_track_with_timing(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    depth: u16,
    samples: &[&[u8]],
    sample_offsets: &[u32],
    timing: Option<(u32, u32)>,
    edit_list: Option<&[(u32, i32, i32)]>,
) -> Vec<u8> {
    let sample_offsets = sample_offsets
        .iter()
        .copied()
        .map(u64::from)
        .collect::<Vec<_>>();
    quicktime_track_with_timing_u64_offsets(
        codec,
        width,
        height,
        depth,
        samples,
        &sample_offsets,
        false,
        timing,
        edit_list,
    )
}

fn quicktime_track_with_timing_u64_offsets(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    depth: u16,
    samples: &[&[u8]],
    sample_offsets: &[u64],
    use_co64: bool,
    timing: Option<(u32, u32)>,
    edit_list: Option<&[(u32, i32, i32)]>,
) -> Vec<u8> {
    assert_eq!(samples.len(), sample_offsets.len());
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
    let stsd = atom(b"stsd", &stsd_payload);

    let mut stsz_payload = Vec::new();
    stsz_payload.extend_from_slice(&0u32.to_be_bytes());
    stsz_payload.extend_from_slice(&0u32.to_be_bytes());
    stsz_payload.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for sample in samples {
        stsz_payload.extend_from_slice(&(sample.len() as u32).to_be_bytes());
    }
    let stsz = atom(b"stsz", &stsz_payload);

    let mut offset_payload = Vec::new();
    offset_payload.extend_from_slice(&0u32.to_be_bytes());
    offset_payload.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for sample_offset in sample_offsets {
        if use_co64 {
            offset_payload.extend_from_slice(&sample_offset.to_be_bytes());
        } else {
            offset_payload.extend_from_slice(&(*sample_offset as u32).to_be_bytes());
        }
    }
    let offset_table = atom(if use_co64 { b"co64" } else { b"stco" }, &offset_payload);

    let mut stbl_payload = Vec::new();
    stbl_payload.extend_from_slice(&stsd);
    stbl_payload.extend_from_slice(&stsz);
    stbl_payload.extend_from_slice(&offset_table);
    if let Some((_, sample_delta)) = timing {
        stbl_payload.extend_from_slice(&quicktime_stts(samples.len() as u32, sample_delta));
    }
    let minf = atom(b"minf", &atom(b"stbl", &stbl_payload));
    let mut mdia_payload = Vec::new();
    if let Some((timescale, sample_delta)) = timing {
        mdia_payload.extend_from_slice(&quicktime_mdhd(
            timescale,
            sample_delta * samples.len() as u32,
        ));
    }
    mdia_payload.extend_from_slice(&minf);
    let mut trak_payload = Vec::new();
    if let Some(entries) = edit_list {
        trak_payload.extend_from_slice(&atom(b"edts", &quicktime_elst(entries)));
    }
    trak_payload.extend_from_slice(&atom(b"mdia", &mdia_payload));
    atom(b"trak", &trak_payload)
}

fn quicktime_time_header(kind: &[u8; 4], timescale: u32, duration: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&timescale.to_be_bytes());
    payload.extend_from_slice(&duration.to_be_bytes());
    payload.extend_from_slice(&0u32.to_be_bytes());
    atom(kind, &payload)
}

fn quicktime_mdhd(timescale: u32, duration: u32) -> Vec<u8> {
    quicktime_time_header(b"mdhd", timescale, duration)
}

fn quicktime_mvhd(timescale: u32, duration: u32) -> Vec<u8> {
    quicktime_time_header(b"mvhd", timescale, duration)
}

fn quicktime_stts(sample_count: u32, sample_delta: u32) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&1u32.to_be_bytes());
    payload.extend_from_slice(&sample_count.to_be_bytes());
    payload.extend_from_slice(&sample_delta.to_be_bytes());
    atom(b"stts", &payload)
}

fn quicktime_elst(entries: &[(u32, i32, i32)]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (duration, media_time, rate) in entries {
        payload.extend_from_slice(&duration.to_be_bytes());
        payload.extend_from_slice(&media_time.to_be_bytes());
        payload.extend_from_slice(&rate.to_be_bytes());
    }
    atom(b"elst", &payload)
}

fn quicktime_stsc(entries: &[(u32, u32, u32)]) -> Vec<u8> {
    let mut payload = Vec::new();
    payload.extend_from_slice(&0u32.to_be_bytes());
    payload.extend_from_slice(&(entries.len() as u32).to_be_bytes());
    for (first_chunk, samples_per_chunk, sample_description_index) in entries {
        payload.extend_from_slice(&first_chunk.to_be_bytes());
        payload.extend_from_slice(&samples_per_chunk.to_be_bytes());
        payload.extend_from_slice(&sample_description_index.to_be_bytes());
    }
    atom(b"stsc", &payload)
}

fn quicktime_movie_samples_in_one_chunk(
    codec: &[u8; 4],
    width: u16,
    height: u16,
    depth: u16,
    samples: &[&[u8]],
) -> Vec<u8> {
    let mut ftyp = Vec::new();
    ftyp.extend_from_slice(b"qt  ");
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(b"qt  ");
    let ftyp = atom(b"ftyp", &ftyp);

    let mut mdat_payload = Vec::new();
    for sample in samples {
        mdat_payload.extend_from_slice(sample);
    }
    let mdat = atom(b"mdat", &mdat_payload);

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

    let mut stsz_payload = Vec::new();
    stsz_payload.extend_from_slice(&0u32.to_be_bytes());
    stsz_payload.extend_from_slice(&0u32.to_be_bytes());
    stsz_payload.extend_from_slice(&(samples.len() as u32).to_be_bytes());
    for sample in samples {
        stsz_payload.extend_from_slice(&(sample.len() as u32).to_be_bytes());
    }

    let mut stco_payload = Vec::new();
    stco_payload.extend_from_slice(&0u32.to_be_bytes());
    stco_payload.extend_from_slice(&1u32.to_be_bytes());
    stco_payload.extend_from_slice(&((ftyp.len() + 8) as u32).to_be_bytes());

    let mut stbl_payload = Vec::new();
    stbl_payload.extend_from_slice(&atom(b"stsd", &stsd_payload));
    stbl_payload.extend_from_slice(&atom(b"stsz", &stsz_payload));
    stbl_payload.extend_from_slice(&atom(b"stco", &stco_payload));
    stbl_payload.extend_from_slice(&quicktime_stsc(&[(1, samples.len() as u32, 1)]));

    let minf = atom(b"minf", &atom(b"stbl", &stbl_payload));
    let mdia = atom(b"mdia", &minf);
    let moov = atom(b"moov", &atom(b"trak", &mdia));

    let mut mov = Vec::new();
    mov.extend_from_slice(&ftyp);
    mov.extend_from_slice(&mdat);
    mov.extend_from_slice(&moov);
    mov
}

fn quicktime_movie_two_tracks(
    first: (&[u8; 4], u16, u16, u16, &[u8]),
    second: (&[u8; 4], u16, u16, u16, &[u8]),
) -> Vec<u8> {
    let mut ftyp = Vec::new();
    ftyp.extend_from_slice(b"qt  ");
    ftyp.extend_from_slice(&0u32.to_be_bytes());
    ftyp.extend_from_slice(b"qt  ");
    let ftyp = atom(b"ftyp", &ftyp);

    let mut mdat_payload = Vec::new();
    mdat_payload.extend_from_slice(first.4);
    mdat_payload.extend_from_slice(second.4);
    let mdat = atom(b"mdat", &mdat_payload);

    let first_offset = (ftyp.len() + 8) as u32;
    let second_offset = first_offset + first.4.len() as u32;
    let first_track = quicktime_track(
        first.0,
        first.1,
        first.2,
        first.3,
        &[first.4],
        &[first_offset],
    );
    let second_track = quicktime_track(
        second.0,
        second.1,
        second.2,
        second.3,
        &[second.4],
        &[second_offset],
    );
    let mut moov_payload = Vec::new();
    moov_payload.extend_from_slice(&first_track);
    moov_payload.extend_from_slice(&second_track);
    let moov = atom(b"moov", &moov_payload);

    let mut mov = Vec::new();
    mov.extend_from_slice(&ftyp);
    mov.extend_from_slice(&mdat);
    mov.extend_from_slice(&moov);
    mov
}

fn be16(v: u16) -> [u8; 2] {
    v.to_be_bytes()
}

fn png_sample(width: u32, height: u32, pixels: &[u8], color_type: image::ColorType) -> Vec<u8> {
    let mut png = Vec::new();
    image::codecs::png::PngEncoder::new(&mut png)
        .write_image(pixels, width, height, color_type.into())
        .unwrap();
    png
}

fn jpeg_sample(width: u32, height: u32, pixels: &[u8], color_type: image::ColorType) -> Vec<u8> {
    let mut jpeg = Vec::new();
    image::codecs::jpeg::JpegEncoder::new_with_quality(&mut jpeg, 100)
        .encode(pixels, width, height, color_type.into())
        .unwrap();
    jpeg
}

#[test]
fn quicktime_decodes_grayscale_jpeg_sample() {
    let path = tmp("jpeg_gray.mov");
    let pixels = [0u8, 64, 128, 255];
    let jpeg = jpeg_sample(2, 2, &pixels, image::ColorType::L8);
    let mov = quicktime_movie(b"jpeg", 2, 2, 8, &jpeg);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_c, 1);
    assert!(!meta.is_rgb);
    assert!(!meta.is_interleaved);

    let plane = reader.open_bytes(0).unwrap();
    assert_eq!(plane.len(), 4);
    assert_eq!(plane[0], 0);
    assert!((60..=68).contains(&plane[1]));
    assert!((124..=132).contains(&plane[2]));
    assert_eq!(plane[3], 255);
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_decodes_grayscale_alpha_png_sample() {
    let path = tmp("png_la.mov");
    let pixels = [10u8, 255, 20, 128, 30, 64, 40, 0];
    let png = png_sample(2, 2, &pixels, image::ColorType::La8);
    let mov = quicktime_movie(b"png ", 2, 2, 32, &png);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_c, 2);
    assert!(!meta.is_rgb);
    assert!(meta.is_interleaved);
    assert!(matches!(
        meta.series_metadata.get("quicktime.codec"),
        Some(MetadataValue::String(codec)) if codec == "png "
    ));

    assert_eq!(reader.open_bytes(0).unwrap(), pixels);
    assert_eq!(
        reader.open_bytes_region(0, 1, 0, 1, 2).unwrap(),
        vec![20, 128, 40, 0]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_decodes_rgba_png_sample() {
    let path = tmp("png_rgba.mov");
    let pixels = [1u8, 2, 3, 255, 4, 5, 6, 128, 7, 8, 9, 64, 10, 11, 12, 0];
    let png = png_sample(2, 2, &pixels, image::ColorType::Rgba8);
    let mov = quicktime_movie(b"png ", 2, 2, 32, &png);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 2);
    assert_eq!(meta.size_y, 2);
    assert_eq!(meta.size_c, 4);
    assert!(meta.is_rgb);
    assert!(meta.is_interleaved);

    assert_eq!(reader.open_bytes(0).unwrap(), pixels);
    assert_eq!(
        reader.open_bytes_region(0, 0, 1, 2, 1).unwrap(),
        vec![7, 8, 9, 64, 10, 11, 12, 0]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_stsc_maps_multiple_samples_in_one_chunk() {
    let path = tmp("stsc_two_samples_one_chunk.mov");
    let first = [1u8, 2, 3, 4, 5, 6];
    let second = [11u8, 12, 13, 14, 15, 16];
    let mov = quicktime_movie_samples_in_one_chunk(b"raw ", 2, 1, 24, &[&first, &second]);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.image_count, 2);
    assert_eq!(meta.size_t, 2);
    assert!(matches!(
        meta.series_metadata.get("quicktime.chunk_offsets"),
        Some(MetadataValue::String(offsets)) if !offsets.contains(',')
    ));
    assert!(matches!(
        meta.series_metadata.get("quicktime.sample_offsets"),
        Some(MetadataValue::String(offsets)) if offsets.contains(',')
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), first);
    assert_eq!(reader.open_bytes(1).unwrap(), second);
    let _ = std::fs::remove_file(path);
}

fn cinepak_frame(width: u16, height: u16, flags: u8, chunks: &[(u16, Vec<u8>)]) -> Vec<u8> {
    let mut strip_body = Vec::new();
    for (id, body) in chunks {
        strip_body.extend_from_slice(&be16(*id));
        strip_body.extend_from_slice(&be16((body.len() + 4) as u16));
        strip_body.extend_from_slice(body);
    }
    let strip_size = strip_body.len() + 12;
    let mut frame = Vec::new();
    frame.push(flags);
    frame.extend_from_slice(&[0, 0, 0]);
    frame.extend_from_slice(&be16(width));
    frame.extend_from_slice(&be16(height));
    frame.extend_from_slice(&be16(1));
    frame.extend_from_slice(&be16(0x1000));
    frame.extend_from_slice(&be16(strip_size as u16));
    frame.extend_from_slice(&be16(0));
    frame.extend_from_slice(&be16(0));
    frame.extend_from_slice(&be16(height));
    frame.extend_from_slice(&be16(width));
    frame.extend_from_slice(&strip_body);
    frame
}

fn cinepak_keyframe_4x4() -> Vec<u8> {
    let mut v4 = Vec::new();
    for i in 0..4u8 {
        let y = (i + 1) * 30;
        v4.extend_from_slice(&[y, y, y, y, 0, 0]);
    }
    let mut vectors = Vec::new();
    vectors.extend_from_slice(&[0x80, 0x00, 0x00, 0x00]);
    vectors.extend_from_slice(&[0, 1, 2, 3]);
    cinepak_frame(4, 4, 0, &[(0x2000, v4), (0x3000, vectors)])
}

#[test]
fn quicktime_decodes_keyframe_cinepak_rgb_sample() {
    let path = tmp("rgb.mov");
    let mut v4 = Vec::new();
    for i in 0..4u8 {
        let y = (i + 1) * 30;
        v4.extend_from_slice(&[y, y, y, y, 0, 0]);
    }
    let mut vectors = Vec::new();
    vectors.extend_from_slice(&[0x80, 0x00, 0x00, 0x00]);
    vectors.extend_from_slice(&[0, 1, 2, 3]);
    let frame = cinepak_frame(4, 4, 0, &[(0x2000, v4), (0x3000, vectors)]);
    let mov = quicktime_movie(b"cvid", 4, 4, 24, &frame);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.size_x, 4);
    assert_eq!(meta.size_y, 4);
    assert_eq!(meta.size_c, 3);
    assert!(meta.is_rgb);
    assert!(matches!(
        meta.series_metadata.get("quicktime.codec"),
        Some(MetadataValue::String(codec)) if codec == "cvid"
    ));
    assert!(matches!(
        meta.series_metadata.get("quicktime.cinepak.depth"),
        Some(MetadataValue::Int(24))
    ));

    let plane = reader.open_bytes(0).unwrap();
    assert_eq!(plane.len(), 4 * 4 * 3);
    let red = |x: usize, y: usize| plane[(y * 4 + x) * 3];
    assert_eq!(red(0, 0), 30);
    assert_eq!(red(2, 0), 60);
    assert_eq!(red(0, 2), 90);
    assert_eq!(red(2, 2), 120);
    assert_eq!(
        reader.open_bytes_region(0, 2, 2, 2, 2).unwrap(),
        vec![120, 120, 120, 120, 120, 120, 120, 120, 120, 120, 120, 120]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_rejects_non_keyframe_cinepak_sample() {
    let path = tmp("delta.mov");
    let v1 = vec![10u8, 20, 30, 40, 0, 0];
    let vectors = vec![0u8];
    let frame = cinepak_frame(4, 4, 1, &[(0x2100, v1), (0x3200, vectors)]);
    let mov = quicktime_movie(b"cvid", 4, 4, 24, &frame);
    std::fs::write(&path, mov).unwrap();

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("non-keyframe Cinepak QuickTime unexpectedly opened"),
        Err(err) => err,
    };
    assert!(
        matches!(err, BioFormatsError::UnsupportedFormat(ref message) if message.contains("delta frame without a previous frame")),
        "unexpected Cinepak non-keyframe error: {err}"
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_decodes_cinepak_delta_sample_from_previous_frame() {
    let path = tmp("delta_after_key.mov");
    let mut v4 = Vec::new();
    for i in 0..4u8 {
        let y = (i + 1) * 30;
        v4.extend_from_slice(&[y, y, y, y, 0, 0]);
    }
    let mut key_vectors = Vec::new();
    key_vectors.extend_from_slice(&[0x80, 0x00, 0x00, 0x00]);
    key_vectors.extend_from_slice(&[0, 1, 2, 3]);
    let keyframe = cinepak_frame(4, 4, 0, &[(0x2000, v4), (0x3000, key_vectors)]);

    let mut delta_vectors = Vec::new();
    delta_vectors.extend_from_slice(&[0x00, 0x00, 0x00, 0x00]);
    let delta = cinepak_frame(4, 4, 1, &[(0x3100, delta_vectors)]);
    let mov = quicktime_movie_samples(b"cvid", 4, 4, 24, &[&keyframe, &delta]);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let first = reader.open_bytes(0).unwrap();
    let second = reader.open_bytes(1).unwrap();
    assert_eq!(second, first);

    let region = reader.open_bytes_region(1, 2, 2, 2, 2).unwrap();
    assert_eq!(
        region,
        vec![120, 120, 120, 120, 120, 120, 120, 120, 120, 120, 120, 120]
    );
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_applies_nonzero_media_time_metadata() {
    let path = tmp("edit_media_time.mov");
    let keyframe = cinepak_keyframe_4x4();
    let delta = cinepak_frame(4, 4, 1, &[(0x3100, vec![0, 0, 0, 0])]);
    let mov = quicktime_movie_samples_with_timing(
        b"cvid",
        4,
        4,
        24,
        &[&keyframe, &delta],
        Some((10, 10)),
        Some(&[(20, 10, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status))
            if status == "applied_single_normal_speed_media_segment"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.media_time_ticks"),
        Some(MetadataValue::Int(10))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_offset_ticks"),
        Some(MetadataValue::Int(-10))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_presentation_time_ticks"),
        Some(MetadataValue::String(times)) if times == "-10,0"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.sample_source_media_time_ticks"),
        Some(MetadataValue::String(times)) if times == "0,10"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.sample_media_segment_index"),
        Some(MetadataValue::String(indices)) if indices == "0,0"
    ));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_applies_leading_empty_edit_metadata() {
    let path = tmp("edit_empty.mov");
    let keyframe = cinepak_keyframe_4x4();
    let delta = cinepak_frame(4, 4, 1, &[(0x3100, vec![0, 0, 0, 0])]);
    let mov = quicktime_movie_samples_with_timing(
        b"cvid",
        4,
        4,
        24,
        &[&keyframe, &delta],
        Some((10, 10)),
        Some(&[(5, -1, 65536), (20, 10, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status))
            if status == "applied_leading_empty_edits_single_normal_speed_media_segment"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.empty_edit_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.empty_duration_movie_ticks"),
        Some(MetadataValue::Int(5))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_offset_ticks"),
        Some(MetadataValue::Int(-5))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_presentation_time_ticks"),
        Some(MetadataValue::String(times)) if times == "-5,5"
    ));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_reports_non_unit_rate_without_presentation_times() {
    let path = tmp("edit_rate.mov");
    let keyframe = cinepak_keyframe_4x4();
    let mov = quicktime_movie_samples_with_timing(
        b"cvid",
        4,
        4,
        24,
        &[&keyframe],
        Some((10, 10)),
        Some(&[(10, 0, 32768)]),
    );
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status)) if status == "not_applied_non_unit_rate"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_diagnostic"),
        Some(MetadataValue::String(diagnostic)) if diagnostic.contains("media_rate 0.5")
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.pixel_order_status"),
        Some(MetadataValue::String(status)) if status == "not_reordered_non_unit_rate"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.unsupported_reason"),
        Some(MetadataValue::String(reason)) if reason == "non_unit_rate"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.first_problem_segment_index"),
        Some(MetadataValue::Int(0))
    ));
    assert!(!meta
        .series_metadata
        .contains_key("quicktime.sample_presentation_time_ticks"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_reports_empty_edit_after_media_metadata() {
    let path = tmp("edit_empty_after_media.mov");
    let keyframe = cinepak_keyframe_4x4();
    let delta = cinepak_frame(4, 4, 1, &[(0x3100, vec![0, 0, 0, 0])]);
    let mov = quicktime_movie_samples_with_timing(
        b"cvid",
        4,
        4,
        24,
        &[&keyframe, &delta],
        Some((10, 10)),
        Some(&[(20, 0, 65536), (5, -1, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status))
            if status == "applied_trailing_empty_edits_single_normal_speed_media_segment"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.empty_edit_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.empty_duration_movie_ticks"),
        Some(MetadataValue::Int(5))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.empty_after_media_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.empty_after_media_duration_movie_ticks"),
        Some(MetadataValue::Int(5))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.first_empty_after_media_segment_index"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_diagnostic"),
        Some(MetadataValue::String(diagnostic)) if diagnostic.contains("applied at normal speed")
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.pixel_order_status"),
        Some(MetadataValue::String(status)) if status == "metadata_only_sample_table_order"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_presentation_time_ticks"),
        Some(MetadataValue::String(times)) if times == "0,10"
    ));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_applies_internal_empty_edit_between_media_segments() {
    let path = tmp("edit_internal_empty.mov");
    let first = [1u8, 2, 3, 4, 5, 6];
    let second = [11u8, 12, 13, 14, 15, 16];
    let mov = quicktime_movie_samples_with_timing(
        b"raw ",
        2,
        1,
        24,
        &[&first, &second],
        Some((10, 10)),
        Some(&[(10, 10, 65536), (5, -1, 65536), (10, 0, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.image_count, 2);
    assert_eq!(meta.size_t, 2);
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status))
            if status == "applied_internal_empty_edits_multiple_normal_speed_media_segments"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.empty_after_media_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.first_empty_after_media_segment_index"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.pixel_order_status"),
        Some(MetadataValue::String(status)) if status == "reordered_sample_aligned_normal_speed"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.pixel_order_diagnostic"),
        Some(MetadataValue::String(diagnostic))
            if diagnostic.contains("skips empty edits")
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_presentation_time_ticks"),
        Some(MetadataValue::String(times)) if times == "15,0"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.sample_source_media_time_ticks"),
        Some(MetadataValue::String(times)) if times == "0,10"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.sample_media_segment_index"),
        Some(MetadataValue::String(indices)) if indices == "1,0"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.sample_read_order"),
        Some(MetadataValue::String(order)) if order == "1,0"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), second);
    assert_eq!(reader.open_bytes(1).unwrap(), first);
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_reports_complex_multiple_media_segments() {
    let path = tmp("edit_complex.mov");
    let keyframe = cinepak_keyframe_4x4();
    let mov = quicktime_movie_samples_with_timing(
        b"cvid",
        4,
        4,
        24,
        &[&keyframe],
        Some((10, 10)),
        Some(&[(5, 0, 65536), (5, 5, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status)) if status == "not_applied_complex_edit_list"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_diagnostic"),
        Some(MetadataValue::String(diagnostic))
            if diagnostic.contains("multiple media segments")
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.pixel_order_status"),
        Some(MetadataValue::String(status)) if status == "not_reordered_complex_edit_list"
    ));
    assert!(!meta
        .series_metadata
        .contains_key("quicktime.sample_presentation_time_ticks"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_applies_sample_aligned_multiple_media_segments() {
    let path = tmp("edit_segments.mov");
    let keyframe = cinepak_keyframe_4x4();
    let delta = cinepak_frame(4, 4, 1, &[(0x3100, vec![0, 0, 0, 0])]);
    let mov = quicktime_movie_samples_with_timing(
        b"cvid",
        4,
        4,
        24,
        &[&keyframe, &delta],
        Some((10, 10)),
        Some(&[(10, 10, 65536), (10, 0, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status)) if status == "applied_multiple_normal_speed_media_segments"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_presentation_time_ticks"),
        Some(MetadataValue::String(times)) if times == "10,0"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.sample_source_media_time_ticks"),
        Some(MetadataValue::String(times)) if times == "0,10"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.sample_media_segment_index"),
        Some(MetadataValue::String(indices)) if indices == "1,0"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.pixel_order_status"),
        Some(MetadataValue::String(status)) if status == "reordered_sample_aligned_normal_speed"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.pixel_order_diagnostic"),
        Some(MetadataValue::String(diagnostic))
            if diagnostic.contains("open_bytes uses edit-list presentation order")
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.sample_read_order"),
        Some(MetadataValue::String(order)) if order == "1,0"
    ));
    assert!(!meta
        .series_metadata
        .contains_key("quicktime.edit_list.presentation_offset_ticks"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_reorders_sample_aligned_raw_reads() {
    let path = tmp("edit_segments_raw_reordered.mov");
    let first = [1u8, 2, 3, 4, 5, 6];
    let second = [11u8, 12, 13, 14, 15, 16];
    let mov = quicktime_movie_samples_with_timing(
        b"raw ",
        2,
        1,
        24,
        &[&first, &second],
        Some((10, 10)),
        Some(&[(10, 10, 65536), (10, 0, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.sample_read_order"),
        Some(MetadataValue::String(order)) if order == "1,0"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), second);
    assert_eq!(reader.open_bytes(1).unwrap(), first);
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_applies_trailing_empty_edit_raw_reads() {
    let path = tmp("edit_trailing_empty_raw.mov");
    let first = [1u8, 2, 3, 4, 5, 6];
    let second = [11u8, 12, 13, 14, 15, 16];
    let mov = quicktime_movie_samples_with_timing(
        b"raw ",
        2,
        1,
        24,
        &[&first, &second],
        Some((10, 10)),
        Some(&[(20, 0, 65536), (10, -1, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.image_count, 2);
    assert_eq!(meta.size_t, 2);
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status))
            if status == "applied_trailing_empty_edits_single_normal_speed_media_segment"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.empty_after_media_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.sample_presentation_time_ticks"),
        Some(MetadataValue::String(times)) if times == "0,10"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), first);
    assert_eq!(reader.open_bytes(1).unwrap(), second);
    assert!(reader.open_bytes(2).is_err());
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_clips_sample_aligned_raw_reads() {
    let path = tmp("edit_segments_raw_clipped.mov");
    let first = [1u8, 2, 3, 4, 5, 6];
    let second = [11u8, 12, 13, 14, 15, 16];
    let third = [21u8, 22, 23, 24, 25, 26];
    let mov = quicktime_movie_samples_with_timing(
        b"raw ",
        2,
        1,
        24,
        &[&first, &second, &third],
        Some((10, 10)),
        Some(&[(10, 10, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.image_count, 1);
    assert_eq!(meta.size_t, 1);
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status)) if status == "applied_clipped_normal_speed_media_segments"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.pixel_order_status"),
        Some(MetadataValue::String(status)) if status == "clipped_sample_aligned_normal_speed"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.sample_read_order"),
        Some(MetadataValue::String(order)) if order == "1"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_sample_indices"),
        Some(MetadataValue::String(order)) if order == "1"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_sample_range_start_index"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_sample_range_end_index_exclusive"),
        Some(MetadataValue::Int(2))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_sample_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_before_sample_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_after_sample_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_source_start_media_time_ticks"),
        Some(MetadataValue::Int(10))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_source_end_media_time_ticks"),
        Some(MetadataValue::Int(20))
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), second);
    assert!(reader.open_bytes(1).is_err());
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_records_clipped_range_for_reordered_segments() {
    let path = tmp("edit_segments_raw_reordered_clipped.mov");
    let first = [1u8, 2, 3, 4, 5, 6];
    let second = [11u8, 12, 13, 14, 15, 16];
    let third = [21u8, 22, 23, 24, 25, 26];
    let fourth = [31u8, 32, 33, 34, 35, 36];
    let mov = quicktime_movie_samples_with_timing(
        b"raw ",
        2,
        1,
        24,
        &[&first, &second, &third, &fourth],
        Some((10, 10)),
        Some(&[(10, 20, 65536), (10, 10, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert_eq!(meta.image_count, 2);
    assert_eq!(meta.size_t, 2);
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.pixel_order_status"),
        Some(MetadataValue::String(status)) if status == "clipped_sample_aligned_normal_speed"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.sample_read_order"),
        Some(MetadataValue::String(order)) if order == "2,1"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_sample_indices"),
        Some(MetadataValue::String(order)) if order == "1,2"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_sample_range_start_index"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_sample_range_end_index_exclusive"),
        Some(MetadataValue::Int(3))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_before_sample_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_after_sample_count"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_source_start_media_time_ticks"),
        Some(MetadataValue::Int(10))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.clipped_source_end_media_time_ticks"),
        Some(MetadataValue::Int(30))
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), third);
    assert_eq!(reader.open_bytes(1).unwrap(), second);
    assert!(reader.open_bytes(2).is_err());
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_reports_non_sample_aligned_start() {
    let path = tmp("edit_non_sample_start.mov");
    let keyframe = cinepak_keyframe_4x4();
    let delta = cinepak_frame(4, 4, 1, &[(0x3100, vec![0, 0, 0, 0])]);
    let mov = quicktime_movie_samples_with_timing(
        b"cvid",
        4,
        4,
        24,
        &[&keyframe, &delta],
        Some((10, 10)),
        Some(&[(10, 5, 65536), (10, 0, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status)) if status == "not_applied_complex_edit_list"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_diagnostic"),
        Some(MetadataValue::String(diagnostic))
            if diagnostic.contains("media segment start is not sample-aligned")
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.unsupported_reason"),
        Some(MetadataValue::String(reason)) if reason == "non_sample_aligned_start"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.first_problem_segment_index"),
        Some(MetadataValue::Int(0))
    ));
    assert!(!meta
        .series_metadata
        .contains_key("quicktime.edit_list.sample_media_segment_index"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_reports_single_segment_non_sample_aligned_end() {
    let path = tmp("edit_single_non_sample_end.mov");
    let first = [1u8, 2, 3, 4, 5, 6];
    let second = [11u8, 12, 13, 14, 15, 16];
    let mov = quicktime_movie_samples_with_timing(
        b"raw ",
        2,
        1,
        24,
        &[&first, &second],
        Some((10, 10)),
        Some(&[(5, 0, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status)) if status == "not_applied_complex_edit_list"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_diagnostic"),
        Some(MetadataValue::String(diagnostic))
            if diagnostic.contains("media segment end is not sample-aligned")
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.unsupported_reason"),
        Some(MetadataValue::String(reason)) if reason == "non_sample_aligned_end"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.first_problem_segment_index"),
        Some(MetadataValue::Int(0))
    ));
    assert!(!meta
        .series_metadata
        .contains_key("quicktime.edit_list.sample_media_segment_index"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_reports_sample_gaps() {
    let path = tmp("edit_gap.mov");
    let keyframe = cinepak_keyframe_4x4();
    let delta = cinepak_frame(4, 4, 1, &[(0x3100, vec![0, 0, 0, 0])]);
    let delta2 = cinepak_frame(4, 4, 1, &[(0x3100, vec![0, 0, 0, 0])]);
    let mov = quicktime_movie_samples_with_timing(
        b"cvid",
        4,
        4,
        24,
        &[&keyframe, &delta, &delta2],
        Some((10, 10)),
        Some(&[(10, 0, 65536), (10, 20, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status)) if status == "not_applied_complex_edit_list"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_diagnostic"),
        Some(MetadataValue::String(diagnostic))
            if diagnostic.contains("edit list media segments do not cover every sample")
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.unsupported_reason"),
        Some(MetadataValue::String(reason)) if reason == "gapped_media_segments"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.first_problem_sample_index"),
        Some(MetadataValue::Int(1))
    ));
    assert!(!meta
        .series_metadata
        .contains_key("quicktime.edit_list.sample_media_segment_index"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_edit_list_reports_overlapping_segments() {
    let path = tmp("edit_overlap.mov");
    let keyframe = cinepak_keyframe_4x4();
    let delta = cinepak_frame(4, 4, 1, &[(0x3100, vec![0, 0, 0, 0])]);
    let mov = quicktime_movie_samples_with_timing(
        b"cvid",
        4,
        4,
        24,
        &[&keyframe, &delta],
        Some((10, 10)),
        Some(&[(20, 0, 65536), (10, 10, 65536)]),
    );
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_status"),
        Some(MetadataValue::String(status)) if status == "not_applied_complex_edit_list"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.presentation_diagnostic"),
        Some(MetadataValue::String(diagnostic))
            if diagnostic.contains("media segments overlap in sample space")
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.unsupported_reason"),
        Some(MetadataValue::String(reason)) if reason == "overlapping_media_segments"
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.first_problem_segment_index"),
        Some(MetadataValue::Int(1))
    ));
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.edit_list.first_problem_sample_index"),
        Some(MetadataValue::Int(1))
    ));
    assert!(!meta
        .series_metadata
        .contains_key("quicktime.edit_list.sample_media_segment_index"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_multi_track_rejection_reports_track_diagnostics() {
    let path = tmp("multi_track.mov");
    let first = vec![0u8; 4 * 4 * 3];
    let second = vec![0u8; 2 * 2 * 3];
    let mov = quicktime_movie_two_tracks((b"raw ", 4, 4, 24, &first), (b"raw ", 2, 2, 24, &second));
    std::fs::write(&path, mov).unwrap();

    let err = match ImageReader::open(&path) {
        Ok(_) => panic!("multi-track QuickTime unexpectedly opened"),
        Err(err) => err,
    };
    let BioFormatsError::UnsupportedFormat(message) = err else {
        panic!("unexpected multi-track error: {err}");
    };
    assert!(message.contains("multiple incompatible video tracks"));
    assert!(message.contains("track 1: codec=raw  4x4 samples=1"));
    assert!(message.contains("track 2: codec=raw  2x2 samples=1"));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_reads_co64_chunk_offsets() {
    let path = tmp("co64_offsets.mov");
    let first = [1u8, 2, 3, 4, 5, 6];
    let second = [11u8, 12, 13, 14, 15, 16];
    let mov = quicktime_movie_samples_with_co64(b"raw ", 2, 1, 24, &[&first, &second]);
    std::fs::write(&path, mov).unwrap();

    let mut reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata
            .get("quicktime.chunk_offset_table_type"),
        Some(MetadataValue::String(table_type)) if table_type == "co64"
    ));
    assert!(matches!(
        meta.series_metadata.get("quicktime.sample_offsets"),
        Some(MetadataValue::String(offsets)) if offsets == "28,34"
    ));
    assert_eq!(reader.open_bytes(0).unwrap(), first);
    assert_eq!(reader.open_bytes(1).unwrap(), second);
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_records_supported_codec_family_metadata() {
    let path = tmp("codec_family_raw.mov");
    let sample = [1u8, 2, 3, 4, 5, 6];
    let mov = quicktime_movie(b"raw ", 2, 1, 24, &sample);
    std::fs::write(&path, mov).unwrap();

    let reader = ImageReader::open(&path).unwrap();
    let meta = reader.metadata();
    assert!(matches!(
        meta.series_metadata.get("quicktime.codec_family"),
        Some(MetadataValue::String(family)) if family == "uncompressed RGB"
    ));
    let _ = std::fs::remove_file(path);
}

#[test]
fn quicktime_reports_known_unsupported_codec_families() {
    for (fourcc, family) in [
        (b"avc1", "H.264/AVC"),
        (b"hvc1", "H.265/HEVC"),
        (b"apch", "Apple ProRes"),
        (b"mjp2", "Motion JPEG 2000"),
        (b"dv50", "DV"),
    ] {
        let path = tmp(&format!(
            "unsupported_{}.mov",
            String::from_utf8_lossy(fourcc)
        ));
        let sample = [0u8; 6];
        let mov = quicktime_movie(fourcc, 2, 1, 24, &sample);
        std::fs::write(&path, mov).unwrap();

        let err = match ImageReader::open(&path) {
            Ok(_) => panic!("unsupported QuickTime codec unexpectedly opened"),
            Err(err) => err,
        };
        let BioFormatsError::UnsupportedFormat(message) = err else {
            panic!("unexpected unsupported codec error: {err}");
        };
        assert!(message.contains(&format!(
            "QuickTime codec {} is unsupported (family: {family})",
            String::from_utf8_lossy(fourcc)
        )));
        assert!(message.contains("Bio-Formats Java delegates this codec family"));
        assert!(message.contains("no external video decoder backend"));
        let _ = std::fs::remove_file(path);
    }
}
