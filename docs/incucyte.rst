Incucyte S3 raw archives
========================

This reader was independently implemented from Incucyte S3 2022B Rev3 raw
files (firmware 20222.4.1.0), with no corresponding Java Bio-Formats reader.
It is in formats::bsd::incucyte and is available without the gpl feature.

Entry points and discovery
--------------------------

ImageReader automatically recognizes both a .icarch archive marker and an
Incucyte TIFF. The marker is a small XML file; pixels are found recursively
under its sibling EssenFiles/ScanData directory. The instrument's Windows
path in TIFF tag 60011 is retained as metadata and never followed.

A TIFF inside EssenFiles/ScanData discovers its companion scans for the same
instrument and vessel. A moved/copied TIFF outside that structure is read
on its own. Directory symlinks are not traversed. To suppress companion
discovery explicitly for a TIFF, use IncucyteReader::set_group_files(false)
before set_id. Configure set_flattened_resolutions(false) before opening,
or use ImageReaderOptions::flattened_resolutions(false), to retain source
pyramids as resolutions rather than independent public series.

Recognition uses the metadata pointer/XML tags, not a Software string
starting Incucyte: observed files report SnTiffDll.dll. TIFF tag 60000
points to a 1 x 1 metadata IFD with legacy-encoded XML in tag 60020.
The metadata IFD is never returned as an image or resolution, including
when it is reachable only through the private pointer. Generic TIFF ASCII
decoding is unchanged; vendor XML is decoded as UTF-8 if valid, otherwise
Windows-1252. XML document types are rejected.

Stream and plate mapping
-------------------------

One native series represents (instrument, vessel, well, physical site,
stream family). Fluorescence channels are grouped together; phase and
Cell-by-Cell Seed remain separate series. Native uint8 phase and uint16
fluorescence/Seed values are preserved without conversion or bit masking.
Unknown image types remain separate families. Stored main-chain half-size
images are exposed as source resolutions; Seed has no source pyramid.

The investigated archive contains 10 wells, nine sites, 27 scans and four
types: C1=Green, C2=Red, Ph=Phase, Seed=Cell-by-Cell Seed. With resolution
flattening disabled, this becomes 270 native series:

* 90 fluorescence series: C=2, T=27, uint16, six stored resolutions.
* 90 phase series: C=1, T=27, uint8, six stored resolutions.
* 90 Seed series: C=1, T=27, uint16, one resolution.

The dimension order is XYCZT; Z=1 and plane_index=t*SizeC+c. Ordering is
numeric well row/column, numeric physical site, then family; fluorescence
channels and scan timestamps are sorted deterministically. Opening with
the default flattening instead exposes 1,170 series for this archive.

OME metadata contains a plate per instrument/vessel and a WellSample for
each native stream image. Thus this archive has 27 image samples per well,
representing three modalities at each of nine physical sites. These are
not 27 distinct stage positions. Physical site identity is retained in
incucyte.site, family in incucyte.family, and sample positions identify
coincident streams. Consumers that count WellSamples as spatial fields
will count modality streams separately. Stream families are not falsely
labelled separate acquisition sessions. The OME model currently has no
plate-acquisition/modality grouping field.

Duplicate plane identities and incomplete channel/time grids within a
stream are errors; missing data is never silently replaced by zero pixels.
Different streams may have different observed scan sets. Their original
ScanDateTime values and plane timestamps remain available in XML. Inconsistent
geometry, sample type, objective or pixel calibration within a stream is an
error. Multi-vessel markers produce separate OME plates. Unsupported sectors
that cannot be interpreted as letter/number wells are reported explicitly.

Metadata and interpretation limits
-----------------------------------

The level-0 pixel spacing in the investigated files is 1.244 micrometres,
from FullResPixelSpacing. The generic TIFF 72-DPI calibration is ignored.
Reduced-level spacing uses the ratio of actual full/reduced dimensions,
including the odd final height (1040 -> 33). Resampling/centre alignment is
inferred from comparison to stored pixels; no new pyramid pixels are generated.

OME channels include XML-selected fluorescence names and excitation/emission
wavelengths. Instrument, objective magnification, detector, per-plane gain,
local timing, exposure and native stage coordinates are retained. Gantry
millimetres are converted to micrometres with explicit position units.
FocusPosition is retained as a focus coordinate, not a Z step. Camera-to-stage
handedness and centre/corner registration remain unknown; stage metadata
does not establish a registered mosaic or image top-left position.

AcqInfo/TimeStamp supplies per-plane DeltaT relative to the earliest acquired
plane of its instrument/vessel. Nominal ScanDateTime orders T. Regular nominal
scan intervals populate TimeIncrement, while actual DeltaT can vary by plane.
No UTC offset is invented and AcquisitionDate is omitted because acquisition
timezone is absent. The original local timestamps are retained. TIFF DateTime
is not used as acquisition time. CameraSettings/ExpTime in msec is converted
to seconds; fluorescence ColorSettings/AcqTime is a combined acquisition
budget, so it is retained in XML rather than falsely reported as one exposure.

Each series retains first-plane flattened XML fields and a per-plane XML/source
file mapping in series_metadata (and OME original metadata map annotations).
Unknown XML fields, sensor temperatures, effective bits, source instrument
paths, acquisition strategies and intensity-conversion coefficients survive.
Full source XML is kept rather than discarding fields that lack an OME mapping.

ConversionScale/ConversionBias and FLRSignalCal are not applied: their precise
calibrated-intensity formula is not established. Seed is auxiliary image data,
not an object-ID segmentation label map. Its values can exceed the reported
12 effective bits and must not be truncated. .sdf databases, .ehf histograms,
vendor analysis results and sample/treatment annotations are not decoded.

Validation
-----------

Synthetic regression coverage is in tests/incucyte_test.rs and runs with::

  cargo test --no-default-features --test incucyte_test

Private data is never required for ordinary tests. The opt-in archive check
opens all native streams, reads first/middle/last tile-crossing regions, and
accesses every stored resolution::

  INCUCYTE_ARCHIVE=/path/experiment.icarch cargo test --test incucyte_test real_incucyte_archive -- --ignored --nocapture

An additional opt-in pixel oracle test accepts INCUCYTE_REFERENCE_MANIFEST,
a JSON array of cases. Each case has source (TIFF path), level (integer),
width, height, expected (raw native pixel bytes), region ([x,y,w,h]), and
region_expected (raw region bytes). Generate these independently with
tifffile, excluding the XML-bearing metadata IFD. The investigated run uses
57 full planes and regions: all native levels of C1/C2/Ph/Seed at the first,
middle and last scans, site B2-1::

  INCUCYTE_REFERENCE_MANIFEST=/tmp/reference/manifest.json cargo test --test incucyte_test real_incucyte_pixels_match_independent_references -- --ignored --nocapture

Other hardware generations, codecs, labware and presentation exports are
not established by this corpus. Private images are not included as fixtures.
