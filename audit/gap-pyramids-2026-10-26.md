# Pyramid and CZI gap closure (2026-10-26)

Reference: read-only Java Bio-Formats checkout `d54bf462e3`. This report covers
FakeReader, TissueFAXS, PyramidTiffReader and ZeissCZIReader changes in this pass.

## Implementation

- All four readers now expose resolutions as independent public series by
  default, matching Java. Calling `set_flattened_resolutions(false)` before
  initialization retains nested resolution selection. Pixel reads, region reads,
  public series indices, metadata and compressed extraction follow the selected
  level. Changing this option after initialization reports an error.
- OME images retain their own Pixels dimensions, pixel type and bit depth.
  Flattened images carry level-specific calibration; nested images represent
  their logical base series. TissueFAXS retains channel/objective associations.
- CZI packed RGB reports total SizeC while preserving logical plane counts and
  channel SamplesPerPixel. Selecting scenes, pyramid levels or attachments
  refreshes the appropriate pixel type and core dimensions.
- CZI translates source-backed objective settings, detectors, light sources,
  filter sets and channel references, including component manufacturer fields,
  detector gain/binning, light intensity, correction collar and medium.
- CZI reads per-subblock stage positions, acquisition timestamps and exposure
  times into OME planes. Without a global acquisition date the timestamp origin
  is initialized per image from its first timestamped plane.
- CZI scans local subblock segments in numbered parts, including parts without
  their own directory. Opening a numbered part resolves its master. Reads and
  compressed byte ranges retain the actual source file. Bare relative paths
  resolve their directory as `.`.
- Embedded Label and SlidePreview CZI attachments are exposed as thumbnail
  series using their own core/bit depth and embedded byte source. Like Java,
  attachments with Z/T greater than one are skipped and valid attachment plane
  requests return the cached first child plane. TimeStamps attachment arrays
  supply per-plane/per-timepoint OME DeltaT.
- JPEG-XR decoder failures retain Java's raw-length/black-fill fallback; this is
  deliberate source parity, not an unimplemented codec path.

## Validation

Focused offline library runs: `fake` (36 passed), `tissuefaxs` (1 passed),
`pyramid` (30 passed, including registry dispatch). CZI focused checks include
the final scene timestamp and attachment regressions; the final count is recorded
in `.tmp/gap-czi-tests.log` (53 tests). `rustfmt` was applied to owned files and
`git diff --check` passed.

Synthetic fixtures exercise SQLite TissueFAXS pixel routing at two levels,
multi-IFD pyramid reads, default/nested APIs, independent OME pixels/calibration,
multipart source routing, directory-less parts, embedded RGB labels and grayscale
previews, repeated attachment plane zero, timestamp arrays, rich optical XML,
and per-subblock tags. These tests verify implementation paths; they do not claim
coverage of every vendor-produced combination or production fixture parity.

FakeReader and CZI now call the shared Java FormatTools/AWT thumbnail adapter.
Default dimensions preserve aspect ratio with maximum 128; FakeReader respects
independent explicit thumbSizeX/thumbSizeY settings. Exact Java-produced golden
bytes cover FakeReader 64x32 to 4x2 and CZI grayscale 256x64 to 128x32. Tests also
check small images, explicit upscaling, lower-resolution selection and unchanged
core dimensions after reading thumbnails. The corresponding Java oracle tools
and output are under `.tmp/thumbnail-oracle`.

PyramidTiffReader preserves MinimalTiffReader's specialized path: reduced main
IFDs (NewSubfileType exactly 1, with multiple IFDs) are excluded from image
resolution cores and read as thumbnail IFDs when sample count/storage depth
match. Other requests use the shared adapter, with thumbnail-IFD dimensions when
present. A synthetic flagged/unflagged IFD regression verifies thumbnail pixels,
resolution counts and the inherited fallback. The SVS reader is unchanged.

Shared thumbnail conversion and TissueFAXS integration are complete; see the
[thumbnail verification report](gap-thumbnails-2026-10-26.md) for the model-specific
Java goldens, source-rejected combinations and validation limits.

## Final real CZI parity correction

The available `testdata/czi/Plate1-Blue-A-25.czi` fixture exposed two instrument
mapping differences. These are fixed against the Java translation order:
HardwareSetting ObjectiveChanger entries overlay Information objectives by index
and update the selected objective from nosepiece Position. Empty or unsupported
LightSourceType elements are skipped; Name-based Laser/LED inference applies only
when both the type element and manufacturer Model are absent. Unsupported source
IDs are excluded from channel LightSourceSettings references. A synthetic test
covers ordered objective overlays, retained manufacturer fields, selected
objective, typed/text/inferred lights and empty/unsupported light types.

The requested strict Java parity run using the updated reference jar passed for
this real CZI fixture: 1 series core metadata, OME metadata and all 5 sampled pixel
checks matched (including full-plane, crop and offset reads). Log:
`.tmp/gap-czi-java-parity.log`. The run used `BIOFORMATS_RS_JAVA_PARITY_FILES=czi/`
and `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=2`; it does not imply wider vendor
fixture coverage. The final focused CZI library run passed 53 tests.

## Fake indexed palette closure

FakeReader now generates the source deterministic per-channel shuffled index
maps for Uint8/Uint16 indexed images, using java.util.Random's 48-bit LCG and
bounded nextInt rejection behavior. The source unsuffixed hexadecimal seed
`0xcafebabe` is sign-extended from Java int to long before adding the channel.
Pixels use the selected logical channel's inverse map before scaleFactor;
lookup_table exposes its forward palette. LUT lengths one and two leave absent
RGB components zero; extra palette components do not alter the first three.
Other pixel types retain Java's absent LUT behavior. Closing/reopening clears
maps, and resolution views retain the palette.

Exact Java-generated goldens verify Uint8/Uint16 inverse pixel bytes and palette
values for channels zero and one. The shared adapter's eight complete Fake
indexed thumbnail hashes pass for Uint8/Uint16 with LUT lengths one through four.
The final focused `cargo test --offline --lib fake` run passed 36 tests, including
those full thumbnail goldens. Log: `.tmp/gap-fake-tests.log`.
