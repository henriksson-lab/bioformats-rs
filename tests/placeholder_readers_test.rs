use std::path::Path;

use bioformats::formats::flim2::{OirReader, VolocityClippingReader};
use bioformats::formats::mias::CellWorxReader;
use bioformats::formats::misc::VolocityLibraryReader;
use bioformats::formats::misc4::{
    FilePatternReaderStub, I2iReader, JdceReader, ObfReader, PciReader,
};
use bioformats::formats::opus::{BrukerOpusReader, IssFlimReader};
use bioformats::formats::simfcs::LambertFlimReader;
use bioformats::{BioFormatsError, FormatReader};

fn assert_uninitialized_placeholder<R: FormatReader>(mut reader: R, path: &str) {
    assert_eq!(reader.series_count(), 0, "{path}");
    assert_eq!(reader.series(), 0, "{path}");

    assert!(
        matches!(reader.set_series(0), Err(BioFormatsError::NotInitialized)),
        "{path}"
    );
    assert!(
        matches!(reader.set_series(1), Err(BioFormatsError::NotInitialized)),
        "{path}"
    );

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
    assert_uninitialized_placeholder(OirReader::new(), "sample.oir");
    assert_uninitialized_placeholder(VolocityClippingReader::new(), "sample.acff");
    assert_uninitialized_placeholder(VolocityLibraryReader::new(), "sample.acff");
    assert_uninitialized_placeholder(BrukerOpusReader::new(), "sample.0");
    assert_uninitialized_placeholder(IssFlimReader::new(), "sample.iss");
    assert_uninitialized_placeholder(LambertFlimReader::new(), "sample.asc");
}

#[test]
fn unsupported_hand_written_placeholders_stay_uninitialized() {
    assert_uninitialized_placeholder(CellWorxReader::new(), "sample.htd");

    assert_uninitialized_placeholder(I2iReader::new(), "sample.i2i");
    assert_uninitialized_placeholder(JdceReader::new(), "sample.jdce");
    assert_uninitialized_placeholder(PciReader::new(), "sample.pci");
    assert_uninitialized_placeholder(FilePatternReaderStub::new(), "sample.pattern");
    assert_uninitialized_placeholder(ObfReader::new(), "sample.obf");
}
