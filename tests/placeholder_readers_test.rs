use std::path::Path;

use bioformats::formats::flim2::{Im3Reader, OirReader, SlideBook7Reader, VolocityClippingReader};
use bioformats::formats::mias::{CellWorxReader, FeiSerReader};
use bioformats::formats::misc::{
    OpenlabLiffReader, QuickTimeReader, SedatReader, SlideBookReader, VolocityLibraryReader,
};
use bioformats::formats::misc4::{
    AplReader, FilePatternReaderStub, HrdgdfReader, I2iReader, JdceReader, KlbReader, ObfReader,
    PciReader,
};
use bioformats::formats::opus::{BrukerOpusReader, IssFlimReader};
use bioformats::formats::sem::{ImrodReader, JeolReader, ZeissLmsReader};
use bioformats::formats::simfcs::LambertFlimReader;
use bioformats::{BioFormatsError, FormatReader};

fn assert_uninitialized_placeholder<R: FormatReader>(mut reader: R, path: &str) {
    assert_eq!(reader.series_count(), 0, "{path}");
    assert_eq!(reader.series(), 0, "{path}");

    assert!(matches!(
        reader.set_series(0),
        Err(BioFormatsError::SeriesOutOfRange(0))
    ));
    assert!(matches!(
        reader.set_series(1),
        Err(BioFormatsError::SeriesOutOfRange(1))
    ));

    let metadata = reader.metadata();
    assert_eq!(metadata.size_x, 0, "{path}");
    assert_eq!(metadata.size_y, 0, "{path}");
    assert_eq!(metadata.image_count, 1, "{path}");
    assert!(reader.ome_metadata().is_none(), "{path}");

    assert!(
        matches!(
            reader.set_id(Path::new(path)),
            Err(BioFormatsError::UnsupportedFormat(_))
        ),
        "{path}"
    );
    assert_eq!(reader.series_count(), 0, "{path}");
    assert_eq!(reader.metadata().size_x, 0, "{path}");
    assert!(reader.ome_metadata().is_none(), "{path}");
}

#[test]
fn flim2_placeholder_readers_stay_uninitialized() {
    assert_uninitialized_placeholder(Im3Reader::new(), "sample.im3");
    assert_uninitialized_placeholder(OirReader::new(), "sample.oir");
    assert_uninitialized_placeholder(SlideBook7Reader::new(), "sample.sld");
    assert_uninitialized_placeholder(VolocityClippingReader::new(), "sample.acff");
    assert_uninitialized_placeholder(VolocityLibraryReader::new(), "sample.acff");
    assert_uninitialized_placeholder(BrukerOpusReader::new(), "sample.0");
    assert_uninitialized_placeholder(IssFlimReader::new(), "sample.iss");
    assert_uninitialized_placeholder(LambertFlimReader::new(), "sample.asc");
}

#[test]
fn sem_placeholder_reader_stays_uninitialized() {
    assert_uninitialized_placeholder(ImrodReader::new(), "sample.mod");
}

#[test]
fn unsupported_hand_written_placeholders_stay_uninitialized() {
    assert_uninitialized_placeholder(QuickTimeReader::new(), "sample.mov");
    assert_uninitialized_placeholder(SlideBookReader::new(), "sample.sld");
    assert_uninitialized_placeholder(OpenlabLiffReader::new(), "sample.liff");
    assert_uninitialized_placeholder(SedatReader::new(), "sample.sedat");

    assert_uninitialized_placeholder(CellWorxReader::new(), "sample.htd");
    let ser = std::env::temp_dir().join(format!(
        "bioformats_placeholder_fei_ser_{}.ser",
        std::process::id()
    ));
    let mut ser_header = vec![0u8; 32];
    ser_header[0..2].copy_from_slice(&[0x97, 0x01]);
    ser_header[4..6].copy_from_slice(&2u16.to_le_bytes());
    ser_header[8..12].copy_from_slice(&1u32.to_le_bytes());
    ser_header[24..28].copy_from_slice(&1u32.to_le_bytes());
    ser_header[28..32].copy_from_slice(&1u32.to_le_bytes());
    std::fs::write(&ser, ser_header).unwrap();
    assert_uninitialized_placeholder(FeiSerReader::new(), ser.to_str().unwrap());
    let _ = std::fs::remove_file(ser);

    assert_uninitialized_placeholder(JeolReader::new(), "sample.dat");
    assert_uninitialized_placeholder(ZeissLmsReader::new(), "sample.lms");

    assert_uninitialized_placeholder(AplReader::new(), "sample.apl");
    assert_uninitialized_placeholder(I2iReader::new(), "sample.i2i");
    assert_uninitialized_placeholder(JdceReader::new(), "sample.jdce");
    assert_uninitialized_placeholder(PciReader::new(), "sample.pci");
    assert_uninitialized_placeholder(HrdgdfReader::new(), "sample.gdf");
    assert_uninitialized_placeholder(FilePatternReaderStub::new(), "sample.pattern");
    assert_uninitialized_placeholder(KlbReader::new(), "sample.klb");
    assert_uninitialized_placeholder(ObfReader::new(), "sample.obf");
}
