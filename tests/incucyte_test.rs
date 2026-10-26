use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use bioformats::formats::bsd::incucyte::IncucyteReader;
use bioformats::{
    BioFormatsError, FormatReader, ImageReader, ImageReaderOptions, MetadataValue, PixelType,
};

struct Fixture(PathBuf);
impl Fixture {
    fn new() -> Self {
        static N: AtomicU64 = AtomicU64::new(0);
        let p = std::env::temp_dir().join(format!(
            "bioformats_incucyte_{}_{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir_all(&p).unwrap();
        Self(p)
    }
    fn marker(&self) -> PathBuf {
        let p = self.0.join("experiment.icarch");
        fs::write(
            &p,
            "\u{feff}<?xml version=\"1.0\" encoding=\"utf-8\"?><incuCyteArchiveHeaderFile />",
        )
        .unwrap();
        p
    }
    fn plane(&self, time: usize, well: &str, site: u32, kind: &str, value: u16) -> PathBuf {
        let p = self.0.join(format!(
            "EssenFiles/ScanData/2605/19/{:02}44/3983/{well}-{site}-{kind}.tif",
            11 + time * 2
        ));
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        let ty = match kind {
            "C1" => "FLR (Color 1)",
            "C2" => "FLR (Color 2)",
            "Ph" => "Phase",
            "Seed" => "Cell-by-Cell Seed",
            _ => kind,
        };
        let xml = xml(time, well, site, ty);
        write_tiff(
            &p,
            &xml,
            if kind == "Ph" { 8 } else { 16 },
            if kind == "Seed" { 1 } else { 2 },
            value,
            true,
        );
        p
    }
}
impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn xml(time: usize, well: &str, site: u32, kind: &str) -> String {
    format!("<ImageInfo><IncuCyteMetadataVersion>1.0</IncuCyteMetadataVersion>
      <AcqInfo><ObjectiveMagnification>10</ObjectiveMagnification><FullResPixelSpacing>1.244 µm</FullResPixelSpacing>
      <ImageType>{kind}</ImageType><Location><WellorSector>{well}</WellorSector><UnityBasedImageIndex>{site}</UnityBasedImageIndex>
      <ImagesPerWellorSector>9</ImagesPerWellorSector><GantryPosition>(218.93,58.25) mm</GantryPosition><FocusPosition>2325.6 µm</FocusPosition></Location>
      <TimeStamp>05/19/2026 {:02}:44:49</TimeStamp><CameraSettings><ExpTime>0.35 msec</ExpTime><ExpGain>0.00</ExpGain></CameraSettings>
      <ColorSettings><AcqTime>300 msec</AcqTime><ConversionScale>46.8</ConversionScale><ConversionBias>0</ConversionBias></ColorSettings>
      <DrawerTemperature>37.63 °C</DrawerTemperature><Unknown>Green &amp; Red</Unknown></AcqInfo>
      <ScanInfo><VesselID>3983</VesselID><ScanDateTime>05/19/2026 {:02}:44 {}</ScanDateTime><Labware><Name>24-well Corning Falcon</Name></Labware></ScanInfo>
      <DeviceInfo><IncuCyteID>instrument-a</IncuCyteID><ControllerSerialNumber>60013</ControllerSerialNumber><HardwareName>Incucyte S3</HardwareName>
      <Camera><Model>acA1920</Model></Camera><OpticalModule><ColorChannel1><Name>Green</Name><EmissionWavelength>524 nm</EmissionWavelength><ExcitationWavelength>460 nm</ExcitationWavelength></ColorChannel1>
      <ColorChannel2><Name>Red</Name><EmissionWavelength>635 nm</EmissionWavelength><ExcitationWavelength>585 nm</ExcitationWavelength></ColorChannel2></OpticalModule></DeviceInfo></ImageInfo>",
      11+time*2, (11+time*2-1)%12+1, if time == 0 {"AM"} else {"PM"})
}

/// Small independent classic TIFF fixture, with source levels in the main
/// chain and a separately pointed-to metadata directory. Legacy CP1252 XML.
fn write_tiff(path: &Path, xml: &str, bits: u16, levels: usize, value: u16, linked_metadata: bool) {
    fn entry(b: &mut Vec<u8>, tag: u16, typ: u16, count: u32, value: u32) -> usize {
        b.extend(tag.to_le_bytes());
        b.extend(typ.to_le_bytes());
        b.extend(count.to_le_bytes());
        let at = b.len();
        b.extend(value.to_le_bytes());
        at
    }
    fn ascii(b: &mut Vec<u8>, s: &[u8]) -> (u32, u32) {
        let at = b.len() as u32;
        b.extend(s);
        b.push(0);
        (at, s.len() as u32 + 1)
    }
    let mut b = b"II\x2a\0\0\0\0\0".to_vec();
    let mut data = Vec::new();
    for l in 0..levels {
        let (w, h) = if l == 0 { (5, 3) } else { (3, 2) };
        let at = b.len() as u32;
        for p in 0..w * h {
            let v = value + l as u16 * 10 + p as u16;
            if bits == 8 {
                b.push(v as u8);
            } else {
                b.extend(v.to_le_bytes());
            }
        }
        data.push((w, h, at, w * h * u32::from(bits / 8)));
    }
    let (sw, swlen) = ascii(&mut b, b"SnTiffDll.dll v20222.4.1.0");
    let encoded = encoding_rs::WINDOWS_1252.encode(xml).0;
    let (xo, xlen) = ascii(&mut b, &encoded);
    let (sp, splen) = ascii(&mut b, b"60013\\D:\\EssenFiles\\moved-source.tif");
    let (ver, verlen) = ascii(&mut b, b"1.2.0.0");
    let resolution = b.len() as u32;
    b.extend(72u32.to_le_bytes());
    b.extend(1u32.to_le_bytes());
    let mut previous_next = 4;
    let mut pointer_patch = 0;
    for (l, (w, h, at, len)) in data.iter().enumerate() {
        let off = b.len() as u32;
        b[previous_next..previous_next + 4].copy_from_slice(&off.to_le_bytes());
        b.extend((if l == 0 { 15u16 } else { 11 }).to_le_bytes());
        entry(&mut b, 254, 4, 1, 0);
        entry(&mut b, 256, 4, 1, *w);
        entry(&mut b, 257, 4, 1, *h);
        entry(&mut b, 258, 3, 1, bits.into());
        entry(&mut b, 259, 3, 1, 1);
        entry(&mut b, 262, 3, 1, 1);
        entry(&mut b, 273, 4, 1, *at);
        entry(&mut b, 277, 3, 1, 1);
        entry(&mut b, 278, 4, 1, *h);
        entry(&mut b, 279, 4, 1, *len);
        entry(&mut b, 305, 2, swlen, sw);
        if l == 0 {
            entry(&mut b, 282, 5, 1, resolution);
            entry(&mut b, 283, 5, 1, resolution);
            entry(&mut b, 296, 3, 1, 2);
            pointer_patch = entry(&mut b, 60000, 13, 1, 0);
        }
        previous_next = b.len();
        b.extend(0u32.to_le_bytes());
    }
    let mo = b.len() as u32;
    b[pointer_patch..pointer_patch + 4].copy_from_slice(&mo.to_le_bytes());
    if linked_metadata {
        b[previous_next..previous_next + 4].copy_from_slice(&mo.to_le_bytes());
    }
    b.extend(14u16.to_le_bytes());
    entry(&mut b, 254, 4, 1, 1);
    entry(&mut b, 256, 4, 1, 1);
    entry(&mut b, 257, 4, 1, 1);
    entry(&mut b, 258, 3, 1, bits.into());
    entry(&mut b, 259, 3, 1, 1);
    entry(&mut b, 262, 3, 1, 1);
    entry(&mut b, 273, 4, 1, 8);
    entry(&mut b, 277, 3, 1, 1);
    entry(&mut b, 278, 4, 1, 1);
    entry(&mut b, 279, 4, 1, u32::from(bits / 8));
    entry(&mut b, 305, 2, swlen, sw);
    entry(&mut b, 60010, 2, verlen, ver);
    entry(&mut b, 60011, 2, splen, sp);
    entry(&mut b, 60020, 2, xlen, xo);
    b.extend(0u32.to_le_bytes());
    fs::write(path, b).unwrap();
}

fn open(path: &Path) -> ImageReader {
    ImageReader::open_with_options(path, ImageReaderOptions::new().flattened_resolutions(false))
        .unwrap()
}
fn family(reader: &ImageReader) -> &str {
    match reader
        .metadata()
        .series_metadata
        .get("incucyte.family")
        .unwrap()
    {
        MetadataValue::String(s) => s,
        _ => panic!(),
    }
}

#[test]
fn standalone_legacy_xml_calibration_positions_and_pointer_only_metadata() {
    let f = Fixture::new();
    let p = f.0.join("renamed.tif");
    write_tiff(&p, &xml(0, "B2", 1, "Phase"), 8, 2, 100, false);
    let mut r = open(&p);
    assert_eq!(r.series_count(), 1);
    assert_eq!(r.resolution_count(), 2);
    assert_eq!(r.metadata().pixel_type, PixelType::Uint8);
    assert_eq!(r.metadata().image_count, 1);
    assert_eq!(
        r.open_bytes_region(0, 1, 1, 3, 2).unwrap(),
        [106, 107, 108, 111, 112, 113]
    );
    let o = r.ome_metadata().unwrap();
    assert_eq!(o.images[0].physical_size_x, Some(1.244));
    assert_eq!(o.images[0].planes[0].position_x, Some(218930.0));
    assert_eq!(o.images[0].planes[0].position_y, Some(58250.0));
    assert_eq!(o.images[0].planes[0].position_x_unit.as_deref(), Some("µm"));
    assert_eq!(o.images[0].planes[0].exposure_time, Some(0.00035));
    assert!(o.images[0].acquisition_date.is_none());
    assert!(
        matches!(r.metadata().series_metadata.get("incucyte.ImageInfo/AcqInfo/Unknown"),Some(MetadataValue::String(s)) if s=="Green & Red")
    );
    assert!(
        matches!(r.metadata().series_metadata.get("incucyte.plane.0.xml"),Some(MetadataValue::String(s)) if s.contains("µm") && s.contains("°C") && !s.contains('\u{fffd}'))
    );
    r.set_resolution(1).unwrap();
    assert_eq!((r.metadata().size_x, r.metadata().size_y), (3, 2));
    assert_eq!(r.open_bytes(0).unwrap(), [110, 111, 112, 113, 114, 115]);
    assert!((r.ome_metadata().unwrap().images[0].physical_size_y.unwrap() - 1.244).abs() < 1e-10); // OME images describe series at initial resolution.
    assert!(r.set_resolution(2).is_err());
    assert!(r.open_bytes(1).is_err());
    assert!(r.open_bytes_region(0, 3, 1, 1, 1).is_err());
}

#[test]
fn archive_groups_native_streams_times_channels_and_seed_without_truncation() {
    let f = Fixture::new();
    let marker = f.marker();
    let mut anchor = PathBuf::new();
    for t in 0..2 {
        for (kind, value) in [("C1", 100), ("C2", 200), ("Ph", 30), ("Seed", 8425)] {
            anchor = f.plane(t, "B2", 1, kind, value + t as u16);
        }
    }
    let mut r = open(&marker);
    assert_eq!(r.series_count(), 3);
    assert_eq!(family(&r), "Fluorescence");
    assert_eq!((r.metadata().size_c, r.metadata().size_t), (2, 2));
    assert_eq!(r.open_bytes(0).unwrap()[..2], 100u16.to_le_bytes());
    assert_eq!(r.open_bytes(1).unwrap()[..2], 200u16.to_le_bytes());
    assert_eq!(r.open_bytes(2).unwrap()[..2], 101u16.to_le_bytes());
    assert_eq!(r.open_bytes(3).unwrap()[..2], 201u16.to_le_bytes());
    r.set_resolution(1).unwrap();
    assert_eq!(r.open_bytes(3).unwrap()[..2], 211u16.to_le_bytes());
    let o = r.ome_metadata().unwrap();
    assert_eq!(o.images.len(), 3);
    assert_eq!(o.images[0].channels[0].name.as_deref(), Some("Green"));
    assert_eq!(o.images[0].channels[1].emission_wavelength, Some(635.0));
    assert_eq!(o.images[0].time_increment, Some(7200.0));
    assert_eq!(o.images[0].planes[2].delta_t, Some(7200.0));
    assert_eq!(o.plates.len(), 1);
    assert_eq!((o.plates[0].rows, o.plates[0].columns), (4, 6));
    assert_eq!(o.plates[0].wells[0].well_samples.len(), 3);
    r.set_series(1).unwrap();
    assert_eq!(family(&r), "Phase");
    assert_eq!(r.metadata().pixel_type, PixelType::Uint8);
    assert_eq!(r.resolution(), 0);
    r.set_series(2).unwrap();
    assert_eq!(family(&r), "Seed");
    assert_eq!(r.resolution_count(), 1);
    assert_eq!(r.open_bytes(0).unwrap()[..2], 8425u16.to_le_bytes());
    assert_eq!(open(&anchor).series_count(), 3);
    let mut single = IncucyteReader::new();
    single.set_group_files(false).unwrap();
    single.set_flattened_resolutions(false).unwrap();
    single.set_id(&anchor).unwrap();
    assert_eq!(single.series_count(), 1);
}

#[test]
fn flattening_exposes_pyramids_but_never_metadata_page_and_resets_on_reopen() {
    let f = Fixture::new();
    let p = f.plane(0, "B2", 1, "C1", 100);
    let mut r = ImageReader::open(&p).unwrap();
    assert_eq!(r.series_count(), 2);
    assert_eq!(r.resolution_count(), 1);
    r.set_series(1).unwrap();
    assert_eq!(r.open_bytes(0).unwrap()[..2], 110u16.to_le_bytes());
    assert!(r.metadata().thumbnail);
    assert_eq!(
        r.ome_metadata().unwrap().images[1].physical_size_y,
        Some(1.244 * 1.5)
    );
    let mut reader = IncucyteReader::new();
    reader.set_id(&p).unwrap();
    reader.close().unwrap();
    assert_eq!(reader.series_count(), 0);
    assert!(reader.ome_metadata().is_none());
    assert!(reader.open_bytes(0).is_err());
    reader.set_id(&p).unwrap();
    assert_eq!(reader.series_count(), 2);
}

#[test]
fn rejects_missing_duplicate_and_inconsistent_planes() {
    let f = Fixture::new();
    let m = f.marker();
    f.plane(0, "B2", 1, "C1", 100);
    f.plane(0, "B2", 1, "C2", 200);
    f.plane(1, "B2", 1, "C1", 101);
    assert!(ImageReader::open(&m)
        .unwrap_err_text()
        .contains("missing plane"));
    let p = f.plane(1, "B2", 1, "C2", 201);
    let duplicate = p.with_file_name("duplicate.tif");
    fs::copy(&p, &duplicate).unwrap();
    assert!(ImageReader::open(&m)
        .unwrap_err_text()
        .contains("duplicate"));
    fs::remove_file(duplicate).unwrap();
    let xml = xml(1, "B2", 1, "FLR (Color 2)").replace("1.244 µm", "2.488 µm");
    write_tiff(&p, &xml, 16, 2, 201, true);
    assert!(ImageReader::open(&m)
        .unwrap_err_text()
        .contains("inconsistent"));
}

trait ErrorText {
    fn unwrap_err_text(self) -> String;
}
impl ErrorText for Result<ImageReader, BioFormatsError> {
    fn unwrap_err_text(self) -> String {
        match self {
            Ok(_) => panic!("expected error"),
            Err(e) => e.to_string(),
        }
    }
}

#[test]
fn malformed_vendor_pointer_does_not_fall_back_to_generic_tiff() {
    let f = Fixture::new();
    let p = f.plane(0, "B2", 1, "C1", 100);
    let mut b = fs::read(&p).unwrap();
    let off = u32::from_le_bytes(b[4..8].try_into().unwrap()) as usize;
    let n = u16::from_le_bytes(b[off..off + 2].try_into().unwrap()) as usize;
    for i in 0..n {
        let e = off + 2 + i * 12;
        if u16::from_le_bytes(b[e..e + 2].try_into().unwrap()) == 60000 {
            b[e + 8..e + 12].copy_from_slice(&u32::MAX.to_le_bytes());
        }
    }
    fs::write(&p, b).unwrap();
    assert!(ImageReader::open(&p).is_err());
}

#[test]
fn ordinary_tiff_is_not_claimed() {
    let f = Fixture::new();
    let p = f.0.join("ordinary.tif");
    bioformats::ImageWriter::save(
        &p,
        &bioformats::ImageMetadata {
            size_x: 2,
            size_y: 2,
            ..Default::default()
        },
        &[vec![1, 2, 3, 4]],
    )
    .unwrap();
    let mut r = open(&p);
    assert_eq!(r.open_bytes(0).unwrap(), [1, 2, 3, 4]);
    assert!(!r.metadata().series_metadata.contains_key("incucyte.family"));
}

#[test]
fn numeric_sites_are_not_lexically_sorted_and_marker_errors_are_clear() {
    let f = Fixture::new();
    let m = f.marker();
    f.plane(0, "B2", 10, "Ph", 100);
    f.plane(0, "B2", 2, "Ph", 50);
    let mut r = open(&m);
    assert_eq!(r.open_bytes(0).unwrap()[0], 50);
    r.set_series(1).unwrap();
    assert_eq!(r.open_bytes(0).unwrap()[0], 100);
    fs::write(&m, "<wrong />").unwrap();
    assert!(ImageReader::open(&m)
        .unwrap_err_text()
        .contains("invalid archive marker"));
}

#[test]
fn multiple_vessels_have_independent_time_origins_and_tiff_entry_selects_vessel() {
    let f = Fixture::new();
    let marker = f.marker();
    let first = f.plane(0, "B2", 1, "Ph", 20);
    let second = f.plane(1, "B2", 1, "Ph", 40);
    write_tiff(
        &second,
        &xml(1, "B2", 1, "Phase")
            .replace("<VesselID>3983", "<VesselID>9000")
            .replace("<ObjectiveMagnification>10", "<ObjectiveMagnification>20"),
        8,
        2,
        40,
        true,
    );
    let r = open(&marker);
    let ome = r.ome_metadata().unwrap();
    assert_eq!(ome.plates.len(), 2);
    assert_eq!(ome.images.len(), 2);
    assert_eq!(ome.images[0].planes[0].delta_t, Some(0.0));
    assert_eq!(ome.images[1].planes[0].delta_t, Some(0.0));
    assert_eq!(
        ome.instruments[0].objectives[ome.images[1].objective_ref.unwrap()].nominal_magnification,
        Some(20.0)
    );
    assert_eq!(open(&first).series_count(), 1);
}

#[test]
fn fluorescence_channels_use_numeric_color_order() {
    let f = Fixture::new();
    let marker = f.marker();
    f.plane(0, "B2", 1, "C2", 200);
    let p = f.plane(0, "B2", 1, "C10", 1000);
    write_tiff(&p, &xml(0, "B2", 1, "FLR (Color 10)"), 16, 2, 1000, true);
    let mut r = open(&marker);
    assert_eq!(r.metadata().size_c, 2);
    assert_eq!(r.open_bytes(0).unwrap()[..2], 200u16.to_le_bytes());
    assert_eq!(r.open_bytes(1).unwrap()[..2], 1000u16.to_le_bytes());
}

#[test]
fn xml_calendar_orders_scans_across_year_boundary_without_host_timezone() {
    let f = Fixture::new();
    let marker = f.marker();
    let first = f.plane(0, "B2", 1, "Ph", 20);
    let second = f.plane(1, "B2", 1, "Ph", 40);
    let a = xml(0, "B2", 1, "Phase")
        .replace("05/19/2026 11:44:49", "12/31/2025 23:59:49")
        .replace("05/19/2026 11:44 AM", "12/31/2025 11:59 PM");
    let b = xml(1, "B2", 1, "Phase")
        .replace("05/19/2026 13:44:49", "01/01/2026 00:01:49")
        .replace("05/19/2026 01:44 PM", "01/01/2026 12:01 AM");
    write_tiff(&first, &a, 8, 2, 20, true);
    write_tiff(&second, &b, 8, 2, 40, true);
    let mut r = open(&marker);
    let ome = r.ome_metadata().unwrap();
    assert_eq!(ome.images[0].time_increment, Some(120.0));
    assert_eq!(ome.images[0].planes[1].delta_t, Some(120.0));
    assert!(ome.images[0].acquisition_date.is_none());
    assert_eq!(r.open_bytes(1).unwrap()[0], 40);
}

#[test]
fn rejects_xml_entities_and_marker_spoofing_and_ignores_unrelated_tiffs() {
    let f = Fixture::new();
    let marker = f.marker();
    let p = f.plane(0, "B2", 1, "Ph", 20);
    let ordinary = p.with_file_name("unrelated.tif");
    bioformats::ImageWriter::save(
        &ordinary,
        &bioformats::ImageMetadata {
            size_x: 2,
            size_y: 2,
            ..Default::default()
        },
        &[vec![1, 2, 3, 4]],
    )
    .unwrap();
    assert_eq!(open(&marker).series_count(), 1);
    fs::write(
        &marker,
        "<incuCyteArchiveHeaderFile></incuCyteArchiveHeaderFile>",
    )
    .unwrap();
    assert_eq!(open(&marker).series_count(), 1);
    fs::write(
        &marker,
        "<wrong><!-- <incuCyteArchiveHeaderFile /> --></wrong>",
    )
    .unwrap();
    assert!(ImageReader::open(&marker).is_err());
    let malicious = format!(
        "<!DOCTYPE ImageInfo [<!ENTITY x SYSTEM 'file:///nonexistent'>]>{}",
        xml(0, "B2", 1, "Phase")
    );
    write_tiff(&p, &malicious, 8, 2, 20, true);
    assert!(ImageReader::open(&p)
        .unwrap_err_text()
        .contains("document types"));
}

/// Optional real-data smoke check. Run with INCUCYTE_ARCHIVE=/path/file.icarch
/// cargo test --test incucyte_test real_incucyte_archive -- --ignored --nocapture
#[test]
#[ignore = "requires private Incucyte archive supplied through INCUCYTE_ARCHIVE"]
fn real_incucyte_archive() {
    let path = std::env::var_os("INCUCYTE_ARCHIVE").expect("set INCUCYTE_ARCHIVE");
    let mut r = open(Path::new(&path));
    println!("native stream count {}", r.series_count());
    assert!(r.series_count() > 0);
    let o = r.ome_metadata().unwrap();
    assert_eq!(o.images.len(), r.series_count());
    for s in 0..r.series_count() {
        r.set_series(s).unwrap();
        assert_eq!(o.images[s].physical_size_x, Some(1.244));
        for p in [
            0,
            r.metadata().image_count / 2,
            r.metadata().image_count - 1,
        ] {
            let data = r.open_bytes_region(p, 250, 250, 32, 32).unwrap();
            assert_eq!(
                data.len(),
                32 * 32 * r.metadata().pixel_type.bytes_per_sample()
            );
        }
        for l in 0..r.resolution_count() {
            r.set_resolution(l).unwrap();
            let b = r.open_bytes_region(0, 0, 0, 1, 1).unwrap();
            assert_eq!(b.len(), r.metadata().pixel_type.bytes_per_sample());
        }
    }
}

/// Independently decoded TIFF oracle manifest; private image bytes stay outside
/// the repository. Each case supplies source, level, expected raw pixels and a
/// tile-crossing region. See README's Incucyte validation instructions.
#[test]
#[ignore = "requires independent tifffile references in INCUCYTE_REFERENCE_MANIFEST"]
fn real_incucyte_pixels_match_independent_references() {
    let manifest =
        std::env::var_os("INCUCYTE_REFERENCE_MANIFEST").expect("set INCUCYTE_REFERENCE_MANIFEST");
    let cases: serde_json::Value = serde_json::from_slice(&fs::read(manifest).unwrap()).unwrap();
    let mut count = 0;
    for case in cases.as_array().unwrap() {
        let mut r = IncucyteReader::new();
        r.set_group_files(false).unwrap();
        r.set_flattened_resolutions(false).unwrap();
        let source = Path::new(case["source"].as_str().unwrap());
        r.set_id(source).unwrap();
        r.set_resolution(case["level"].as_u64().unwrap() as usize)
            .unwrap();
        assert_eq!(r.metadata().size_x as u64, case["width"].as_u64().unwrap());
        assert_eq!(r.metadata().size_y as u64, case["height"].as_u64().unwrap());
        assert_eq!(
            r.open_bytes(0).unwrap(),
            fs::read(case["expected"].as_str().unwrap()).unwrap(),
            "full {:?} level {}",
            source,
            r.resolution()
        );
        let region: Vec<u32> = case["region"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_u64().unwrap() as u32)
            .collect();
        assert_eq!(
            r.open_bytes_region(0, region[0], region[1], region[2], region[3])
                .unwrap(),
            fs::read(case["region_expected"].as_str().unwrap()).unwrap(),
            "region {:?} level {}",
            source,
            r.resolution()
        );
        count += 1;
    }
    println!("Matched {count} independent full planes and regions");
}
