use std::path::Path;

// NOTE: OirReader, VolocityClippingReader, CellWorxReader, ObfReader, I2iReader,
// JdceReader, and PciReader were formerly stubs but are now real readers.
// BrukerOpusReader, IssFlimReader, LambertFlimReader, VolocityLibraryReader, and
// SedatReader/WoolzReader were fabricated readers for formats Bio-Formats has no
// reader for (or duplicates of real readers) and have been DELETED. The only
// remaining hand-written placeholder is FilePatternReaderStub.
use bioformats::formats::misc4::FilePatternReaderStub;
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
fn unsupported_hand_written_placeholders_stay_uninitialized() {
    assert_uninitialized_placeholder(FilePatternReaderStub::new(), "sample.pattern");
}
