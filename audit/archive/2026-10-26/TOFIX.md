> Historical snapshot retained for provenance. This is not the active backlog.
> See [current implementation status](../../../TOFIX.md).

# Formats needing non-flattened pyramid-resolution fixes

Audit date: 2026-08-12

Scope: all Rust format files under `src/formats`, plus shared `src/tiff`,
`src/stitcher`, `src/common/reader`, and mapped Java readers under
`java-bioformats/components/*/src/loci/formats/in`.

Java Bio-Formats has a reader-level `setFlattenedResolutions(boolean)` switch.
The default is flattened, but callers can disable flattening so one logical
series exposes multiple pyramid levels through `getResolutionCount()` and
`setResolution()`. Rust has `resolution_count()` and `set_resolution()`, but no
equivalent mode switch in `FormatReader`.

Supersession note: later Java-parity work in `AUDITLIST2.md` restored Java's
default flattened public view for sampled WSI/BDV readers where the old
non-flattened choice caused drift. Treat the format-specific entries below as
historical unless they are explicitly rechecked against Java's current default
`flattenedResolutions=true` behavior.

JPEG-family parity note: for standalone JPEG/JPEG-2000 and container readers
with JPEG-backed planes, byte-parity checks may need relaxed tolerance because
Java and Rust can use different IDCT, color-conversion, or floating-point paths.
Do not require exact byte equality for JPEG-backed pixel bytes when floating
point decoder paths differ. Do not treat small tolerant pixel deltas as drift
when metadata, dimensions, plane mapping, channel layout, decoded shape, and
bounded tolerant pixel checks match Java.

## Shared API gap

- [x] Add a crate-level equivalent of Java `setFlattenedResolutions(false)` /
      `hasFlattenedResolutions()`, or formally choose Rust's default behavior
      and make every pyramid-capable reader expose nested resolutions through
      `resolution_count()` / `set_resolution()`.
  - Java: `java-bioformats/components/formats-api/src/loci/formats/FormatReader.java`
    has `flattenedResolutions`, `getResolutionCount()`, `setResolution()`, and
    `setFlattenedResolutions(boolean)`.
  - Rust: `src/common/reader.rs` exposes `resolution_count()` /
    `set_resolution()` but no flattening-mode switch.
  - Formal Rust choice for this audit: do not add Java's configurable flattened
    mode. Pyramid-capable readers expose Bio-Formats' non-flattened behavior by
    default through the existing `resolution_count()` / `set_resolution()` /
    `resolution()` API. All format-specific gaps listed below have been updated
    to follow that convention.

## Formats to fix

- [x] **BDV / BigDataViewer** (`.h5`, `.xml`)
  - Java `BDVReader` records per-setup `resolutionCount` and switches indexing
    based on `hasFlattenedResolutions()`.
  - Rust `src/formats/bdv.rs` explicitly builds a flattened setup x level series
    list and sets each visible level to `resolution_count: 1`.
  - Fixed in `src/formats/bdv.rs`; logical setup series now own nested
    `ResolutionInfo` levels, `metadata()`/pixel reads/OME use the active/base
    resolution as Java's non-flattened path does, and `set_resolution()` switches
    the HDF5 level path. Verified with `cargo test bdv_ --lib` and full
    `cargo test --lib` (1135 tests).

- [x] **Olympus cellSens / VSI / ETS** (`.vsi`, `.ets`)
  - Java `CellSensReader` creates per-pyramid resolution cores and supports both
    flattened and non-flattened access.
  - Rust `src/formats/flim2.rs` builds one `CellSensTarget::Ets { volume,
    resolution }` per top-level series, returns `resolution_count() == 1`, and
    rejects nonzero `set_resolution()`.
  - Fixed in `src/formats/flim2.rs`; ETS volumes are now exposed as one logical
    series with pyramid levels selected through `resolution_count()` /
    `set_resolution()`, active-resolution metadata is rebuilt from
    `EtsVolume::level_metadata`, and compressed tile APIs route by requested
    level. Verified with
    `cellsens_vsi_pyramid_exposes_resolution_levels_not_flattened_series`,
    `cargo test cellsens_ --lib`, and full `cargo test --lib` (1134 tests).

- [x] **Aperio SVS** (`.svs`)
  - Java `SVSReader` groups pyramid levels under the base image via
    `resolutionCount`, while still supporting flattened default mode.
  - Rust `src/formats/svs.rs` first regroups the pyramid, then unconditionally
    calls `flatten_resolutions_into_series()`.
  - Fixed in `src/formats/svs.rs`; SVS now keeps the regrouped pyramid nested,
    so the main image exposes sub-resolution levels through `resolution_count()`
    / `set_resolution()`, with label/macro images remaining as separate series.

- [x] **Hamamatsu NDPI** (`.ndpi`)
  - Java `NDPIReader` sets `resolutionCount = pyramidHeight` and branches
    between `setSeries` and `setResolution`.
  - Rust `src/formats/tiff_wrappers.rs` builds a nested NDPI series, then
    unconditionally flattens it.
  - Fixed in `src/formats/tiff_wrappers.rs`; the already-built nested pyramid
    is no longer flattened, so series 0 exposes `pyramidHeight` through
    `resolution_count()` / `set_resolution()` and macro/map images remain
    separate logical series. Verified with the updated synthetic
    `ndpi_capture_mode_overrides_pyramid_rgb_metadata`, `cargo test ndpi_ --lib`,
    and full `cargo test --lib` (1134 tests).

- [x] **Hamamatsu NDPIS** (`.ndpis`)
  - Java `NDPISReader` forwards `hasFlattenedResolutions()` to child
    `NDPIReader` instances.
  - Rust `src/formats/flim2.rs` constructs fixed `NdpiReader` children with no
    mode to forward, so it inherits the NDPI flattened-only behavior.
  - Fixed in `src/formats/flim2.rs`; NDPIS now builds merged metadata from the
    nested child NDPI logical series, treats series 0 as the multi-channel
    pyramid image, forwards `set_resolution()` to child NDPI readers only for
    that pyramid series, and rebuilds active-resolution metadata with merged
    channel counts. Verified with `cargo test ndpis --lib` and full
    `cargo test --lib` (1134 tests).

- [x] **Leica SCN** (`.scn`)
  - Java `LeicaSCNReader` uses `sizeR` / `resolutionCount` and skips nonzero
    resolution images in metadata when not flattened.
  - Rust `src/formats/tiff_wrappers.rs` builds `sub_resolutions`, then
    unconditionally calls `flatten_resolutions_into_series()`.
  - Fixed in `src/formats/tiff_wrappers.rs`; SCN no longer flattens the
    constructed sub-resolution series, OME metadata is emitted once per logical
    image using resolution-0 metadata like Java's non-flattened path, and the
    reader exposes pyramid levels via the delegated TIFF `resolution_count()` /
    `set_resolution()` API. Verified with
    `scn_pyramid_exposes_resolution_levels_not_flattened_series`,
    `cargo test scn_ --lib`, and full `cargo test --lib` (1135 tests).

- [x] **Ventana / Roche BIF** (`.bif`)
  - Java `VentanaReader` groups pyramid levels and has non-flattened metadata
    logic.
  - Rust `src/formats/tiff_wrappers.rs` reorders BIF pyramid IFDs into
    independent top-level series instead of nesting them as resolutions.
  - Fixed in `src/formats/tiff_wrappers.rs`; BIF pyramid IFDs are now grouped as
    series 0 sub-resolutions, overview/mask IFDs remain separate logical series,
    stitched-size overrides and assembly scaling use `resolution()` for the
    active pyramid level, and non-pyramid series bypass stitching. Verified with
    `ventana_bif_pyramid_ifds_are_nested_resolutions_like_java_non_flattened`
    and `cargo test ventana --lib`.

- [x] **Trestle TIFF** (`.tif`, `.tiff`)
  - Java default `TrestleReader` exposes flattened pyramid IFDs as public
    series (`seriesCount=7`, each `resolutionCount=1` for the sampled CMU-1
    file), while retaining overlap-aware tile math internally.
  - Rust had drifted toward the configurable non-flattened mental model for this
    reader.
  - Fixed in `src/formats/hcs2.rs`; Trestle now exposes Java's default flattened
    public series, keeps overlap-adjusted level metadata for pixel reads, rejects
    nonzero public `set_resolution()`, and emits one OME image per public series.
    Verified with `trestle_pyramid_ifds_are_flattened_series_like_java_default`,
    `cargo test trestle --lib`, and metadata-only Java parity on
    `/big/henriksson/ome_images/Trestle/openslide/CMU-1/CMU-1.tif`
    (7/7 core series OK, OME OK).

- [x] **InCell 3000 suffix overclaim** (`.frm`)
  - Java `InCell3000Reader` registers only `.frm`; `.xdce` is the XML InCell
    index handled by `InCellReader`.
  - Rust `src/formats/hcs2.rs` claimed `.xdce` by name in `InCell3000Reader`,
    which could route XML InCell files through the binary frame reader.
  - Fixed in `src/formats/hcs2.rs`; `InCell3000Reader` now claims only `.frm`.
    `src/formats/incell.rs` keeps a missing-companion diagnostic containing the
    existing HCS no-TIFF wording for `.xdce` registry behavior. Verified with
    `incell3000_name_suffix_matches_java_frm_only`,
    `cargo test incell --lib`, and
    `cargo test visitech_registry_opens_metadata_only_xys_but_rejects_hcs_without_tiffs --lib`.

- [x] **Vectra / QPTIFF** (`.qptiff`, `.tif`, `.tiff`)
  - Java `VectraReader` detects `pyramidDepth`, sets the first core's
    `resolutionCount`, and changes OME metadata behavior based on
    `hasFlattenedResolutions()`.
  - Rust `src/formats/extended.rs` emits flattened one-resolution series and
    clears `sub_resolutions`.
  - Fixed in `src/formats/extended.rs`; QPTIFF now builds Java's non-flattened
    topology with series 0 containing nested pyramid levels and thumbnail/extra
    images as separate series, active metadata enrichment uses the selected
    resolution's physical IFDs, and high-resolution thumbnail reads redirect to
    the smallest pyramid level like Java. Verified with
    `qptiff_pyramid_levels_are_nested_resolutions_like_java_non_flattened` and
    `cargo test qptiff --lib`.

- [x] **JPEG-2000** (`.jp2`, `.j2k`)
  - Java `JPEG2000Reader` parses codestream resolution levels and sets
    `resolutionCount = resolutionLevels + 1`.
  - Rust `src/formats/misc.rs` decodes one image and hard-codes
    `resolution_count: 1`.
  - Fixed in `src/formats/misc.rs`; standalone JPEG-2000 now parses COD
    decomposition levels, builds Java-style synthetic resolution metadata,
    implements active `resolution_count()` / `set_resolution()`, and decodes
    lazily with OpenJPEG reduction for the selected level. Verified with
    `jpeg2000_reader_exposes_codestream_resolutions_like_java_non_flattened`,
    `jpeg2000_cod_parser_reads_resolution_levels_from_raw_and_jp2_codestreams`,
    and `cargo test jpeg2000 --lib`.

- [x] **Fake** (`.fake`)
  - Java `FakeReader` accepts `resolutions`, appends scaled sub-resolution cores,
    and exposes them through the standard resolution API.
  - Rust `src/formats/fake.rs` validates and records `resolution_count` in
    metadata, but does not implement active `resolution_count()` /
    `set_resolution()` behavior.
  - Fixed in `src/formats/fake.rs`; the reader now tracks the active resolution,
    reports parsed `resolution_count()`, rejects out-of-range levels, and scales
    `SizeX`/`SizeY` by `resolutionScale^level` like Java's sub-resolution cores.

- [x] **FilePattern** (`.pattern`)
  - Java `FilePatternReader` forwards `hasFlattenedResolutions()` and
    `setFlattenedResolutions()` to its helper reader and preserves helper
    `resolutionCount`.
  - Rust `src/formats/misc4.rs` delegates to `FileStitcher`, but neither the
    wrapper nor `src/stitcher.rs` forwards/overrides the resolution API.
  - Fixed in `src/stitcher.rs` and `src/formats/misc4.rs`; FileStitcher now
    tracks active resolution, forwards `resolution_count()` / `set_resolution()`
    / `resolution()` to wrapped readers, updates active-resolution metadata,
    and FilePatternReader exposes the forwarded API. Verified with
    `forwards_wrapped_reader_resolution_api_like_java` and full
    `cargo test --lib` (1133 tests).

- [x] **JPEG-2000-compressed TIFF internal resolutions** (`.tif`, `.tiff`)
  - Java `MinimalTiffReader` can synthesize sub-resolution metadata from
    JPEG-2000 codestream levels for TIFF planes.
  - Rust `src/tiff/reader.rs` supports TIFF SubIFDs, but this audit did not find
    equivalent synthetic JPEG-2000 internal-resolution handling.
  - Fixed in `src/tiff/reader.rs` and `src/tiff/compression.rs`; TIFF
    initialization now parses JPEG-2000 COD levels from the first compressed
    strip/tile when explicit SubIFDs are absent, synthesizes sub-resolution IFD
    metadata, tracks the corresponding OpenJPEG reduction factor, and passes it
    through strip/tile decode. Verified with
    `jpeg2000_compressed_tiff_synthesizes_internal_resolutions_like_java` and
    `cargo test jpeg2000 --lib`.

## Checked and not currently listed for fixing

- **Generic TIFF / OME-TIFF SubIFDs**: Rust `TiffReader` has `sub_resolutions`
  and implements `resolution_count()` / `set_resolution()`.
- **Faas Pyramid TIFF**: Rust `PyramidTiffReader` regroups trailing IFDs into
  `sub_resolutions` and delegates resolution switching.
- **Zeiss CZI**: Rust stores per-series `CziResolution` levels and implements
  `resolution_count()` / `set_resolution()`.
- **TissueFAXS**: Rust exposes logical series with `series_resolutions` and
  functional `set_resolution()`.
- **Olympus OIR**, **Zeiss LMS/LSM/ZVI/XRM**, **Leica LIF/LEI/LMS/XLEF**:
  no matching Java nested-resolution behavior found in the audited paths.
- **CV7000, CellVoyager, Columbus, Operetta, InCell, CellH5, Micro-Manager**:
  no Java pyramid/sub-resolution behavior found beyond ordinary series
  assembly.
- Remaining Rust format files were scanned for `resolution_count`,
  `set_resolution`, `resolution_count:` metadata, `sub_resolutions`, and mapped
  Java `resolutionCount` / `hasFlattenedResolutions` usage. No additional
  format-specific non-flattened pyramid gaps were identified.

# Formats needing OME channel metadata fixes

Audit date: 2026-08-12

Scope: all Rust format files under `src/formats`, shared OME projection in
`src/common/ome_metadata.rs`, shared wrappers/stitching, and mapped Java readers
that call `setChannelName`, `setChannelEmissionWavelength`, or
`setChannelExcitationWavelength`.

The two audited bug classes were:

- OME channel list shorter than the declared non-RGB channel count.
- Java populates channel `name` and/or `emission_wavelength`, but Rust leaves
  the corresponding `OmeChannel` field unset.

## Shared channel-metadata behavior

- [x] Make parsed/imported OME metadata robust when an image declares non-RGB
      `SizeC > ChannelCount`.
  - Rust `OmeMetadata::from_image_metadata` / `populate_pixels` sizes channels
    from effective `SizeC`, so most plain readers are safe.
  - Rust `OmeMetadata::from_ome_xml` only pads when the parsed `<Channel>` list
    is empty, not when it is shorter than `Pixels@SizeC`.
  - Java readers typically call `MetadataTools.populatePixels` first and then
    overlay channel fields, so partially populated channel lists still have the
    declared channel count.
  - Fixed in `src/common/ome_metadata.rs`; sparse parsed channels are padded
    against `Pixels@SizeC` while packed RGB `SamplesPerPixel > 1` remains a
    single OME channel.

## Formats to fix

- [x] **JDCE Molecular Devices plate metadata parity** (`.jdce`)
  - Java `JDCEReader.populateMetadataStore` calls
    `MetadataTools.populatePixels(store, this, true)`, then writes
    Plate/Well/WellSample graph, image names (`WellName, Field #N`), physical
    sizes, channel names/emission/excitation wavelengths, per-plane positions,
    exposure time, and delta-T values.
  - Fixed in `src/formats/misc4.rs`: `JdceReader` now carries parsed plate
    name/shape, Java `FormatTools.getWellName`-style image names, physical
    pixel sizes, channel names/emission/excitation wavelengths, and per-plane
    timestamp/position/exposure metadata through the assembled series and emits
    an all-series OME metadata override with Plate/Well/WellSample objects.
  - Both mounted JDCE samples under `/big/henriksson/ome_images/JDCE` currently
    fail in the Java oracle before metadata with
    `IllegalArgumentException: 0 must be non-null and strictly positive`, so this
    item is source/unit-backed rather than Java-oracle-proven. Verification:
    `cargo test jdce_ome_metadata_projects_java_plate_channel_and_plane_fields
    --lib -- --nocapture` passed, `cargo check` passed, and the focused
    metadata-only Java parity run remained green for 45 comparable files while
    skipping both JDCE samples for the upstream Java exception.
  - 2026-08-14 refresh against the two mounted JDCE files reproduced the same
    upstream Java exception for both files before comparison; `cargo test jdce
    --lib -- --nocapture` still passes the focused Rust OME projection test.

- [x] **Nikon NEF physical-size OME metadata** (`.nef`)
  - Java `NikonReader` uses `mergeSubIFDs=true`, exposes the data SubIFD as the
    single logical image, and projects TIFF X/Y resolution tags through the
    standard Bio-Formats physical-size conversion.
  - Rust already selected the data SubIFD for core dimensions and pixels, but
    kept generic TIFF physical-size metadata from the wrong/default series. The
    downloaded raw.pixls D70 sample therefore matched Java core metadata while
    omitting OME `PhysicalSizeX/Y`; Java emitted `84.66666666666667` um/pixel
    from 300 dpi inch-resolution tags.
  - Fixed in `src/formats/camera2.rs`: `NikonReader` now derives physical sizes
    from the active data IFD with TIFF resolution-unit conversion and projects
    them into OME. Verification: `cargo test nikon --lib -- --nocapture` passed,
    and focused Java parity for
    `external-fixtures/data/nikon-nef/raw.pixls.us/data/Nikon/D70/20170902_0047.NEF`
    passed with 1/1 core series OK, OME OK, and 2/2 pixel checks bitwise.

- [x] **ECAT7 OME image name** (`.v`)
  - Java `Ecat7Reader` populates pixels through the standard metadata path and
    exposes the file name as the OME image name for the sampled ECAT7 file.
  - Rust matched core dimensions and pixel type, but emitted `None` for
    `OME/Image@Name`, causing metadata parity drift.
  - Fixed in `src/formats/clinical.rs`; `Ecat7Reader::ome_metadata` now fills
    the image name from the current file name when no stronger name is already
    present.
  - Verified with focused Java metadata parity for
    `external-fixtures/data/ecat7/downloads.openmicroscopy.org/torsten/gradient-512x512x10.v`
    and `cargo test ecat --lib`.

- [x] **Hamamatsu DCIMG v1 four-pixel footer correction** (`.dcimg`)
  - Java `DCIMGReader.parseDCAMVersion1Header` reads `frameFooterSize` at
    `headerSize + 124`; if it is `32` or at least `512`, Java enables the
    four-pixel correction and restores the first four samples of the guessed
    correction row from the frame footer.
  - Rust read `frameFooterSize` from `headerSize + 120`, so the real Cell09
    fixture left the in-frame bookkeeping values in those four pixels.
  - Fixed in `src/formats/dcimg.rs`; Rust now reads the Java offset and applies
    the same footer correction path. Verified with the focused synthetic unit
    test and real Java parity for
    `external-fixtures/data/dcimg/downloads.openmicroscopy.org/zenodo-14287640/Cell09/Cell09_642_000_000.dcimg`:
    1/1 core series OK, OME OK, and 7/7 pixel checks bitwise.

- [x] **OBF embedded OME-XML axis and all-series OME projection** (`.obf`, `.msr`)
  - Java `OBFReader` reads the `ome_xml` metadata block, uses embedded
    `<Pixels>` values for core `SizeX/Y/Z/C/T` and `DimensionOrder`, and
    exposes the full embedded OME image list.
  - Rust parsed embedded OME XML with raw substring matching. On
    `testdata/obf/test-v6-short-write.obf`, that matched `SizeX` inside
    `PhysicalSizeX`, failed to parse the floating-point value as an integer,
    dropped every `<Pixels>` block, fell back to raw stack axes
    (`sizeC=18`, `sizeT=1`, `XYZCT`), and emitted OME for only the active
    series.
  - Fixed in `src/formats/misc4.rs`; OBF now uses boundary-aware XML event
    parsing, applies embedded OME Pixels to core metadata, returns the full
    embedded OME image list, and preserves Java-visible raw OME physical-size
    numeric values for this format-specific path.
  - Verified with focused Java metadata parity for
    `testdata/obf/test-v6-short-write.obf`: 7/7 core series OK and OME OK, plus
    `cargo test obf_real_fixture_uses_embedded_ome_for_all_series --lib`.

- [x] **OME-TIFF / OME-XML in TIFF** (`.ome.tif`, `.ome.tiff`, `.tif`, `.tiff`)
  - Count gap: parsed OME XML can return fewer `OmeChannel` entries than
    non-RGB `SizeC` when only some `<Channel>` elements are present.
  - Name/emission fields are preserved when the XML channel elements exist.
  - Fixed by the shared `OmeMetadata::from_ome_xml` padding behavior.

- [x] **FilePattern / generic TIFF physical-size OME metadata** (`.pattern`, `.tif`, `.tiff`)
  - Java `BaseTiffReader` uses `IFD.getXResolution()` /
    `IFD.getYResolution()`, which convert TIFF `ResolutionUnit` values to
    microns per pixel before OME population, and `FilePatternReader` keeps the
    helper reader's physical-size metadata when stitching planes.
  - Rust previously failed sampled Java parity for
    `/big/henriksson/ome_images/TIFF/cellprofiler/fruit-fly-cells/POS002.pattern/POS002.pattern`:
    the wrapped TIFF metadata contained physical sizes, but `FileStitcher`
    did not project them into OME, and the generic TIFF helper did not apply
    Java's inch/centimeter unit conversion.
  - Fixed in `src/tiff/reader.rs` and `src/stitcher.rs`; TIFF resolution tags
    now convert to microns per pixel for inch/centimeter units, and
    FileStitcher projects inherited `PhysicalSizeX/Y/Z` into OME.
  - Verified with the focused FilePattern Java parity run: 1 file, 1 core
    series OK, OME OK, and 7/7 pixel checks bitwise; the focused
    `file_stitcher_ome_metadata_uses_filepattern_channel_names` unit test also
    covers physical-size projection.

- [x] **Zeiss CZI** (`.czi`)
  - Count gap: Rust replaces the baseline channel list with parsed XML channels
    when any are found, so sparse CZI XML can produce fewer OME channels than
    declared `SizeC`.
  - Java populates channels from pixels first, then overlays parsed channel
    fields only where available.
  - Fixed in `src/formats/zeiss_czi.rs`; CZI OME projection now runs
    `populate_pixels` after XML channel overlay.

- [x] **Olympus cellSens / VSI / ETS** (`.vsi`, `.ets`)
  - Count gap: Rust custom OME construction creates exactly one channel per OME
    image even for non-RGB ETS data with `size_c > 1`.
  - Name/emission gap: Rust captures ETS channel names and channel wavelength
    values in vendor-prefixed metadata, but does not promote them to
    `OmeChannel.name` / `OmeChannel.emission_wavelength`.
  - Fixed for OME channel projection in `src/formats/flim2.rs`, matching Java's
    effective-channel behavior: packed RGB ETS remains one channel with
    `SamplesPerPixel > 1`; non-RGB C-dimension ETS gets one channel per C with
    parsed name/emission fields.

- [x] **Olympus OIR** (`.oir`)
  - Name/emission gap: Rust promotes channel names via generic `channel.N.name`
    metadata, but does not parse/populate Java's optical element start/end
    wavelength fields as OME emission/excitation wavelengths.
  - Fixed in `src/formats/flim2.rs`; native OIR channel XML now promotes
    `opticalelement:startWavelength` and `opticalelement:endWavelength` through
    generic channel excitation/emission metadata consumed by OME projection.

- [x] **Leica LEI** (`.lei`)
  - Name gap: Rust uses baseline OME metadata only; Java `LeicaReader` derives
    channel names from LEI instrument detector/stain metadata before OME
    population.
  - Fixed in `src/formats/leica.rs`; LEI instrument SAFEARRAY entries now parse
    Java's active PMT/stain channel-name path and project resulting names through
    generic OME channel metadata. The audited Java 8.1.0 LEI source checks
    `emWaves`/`exWaves` during OME population but does not populate those lists
    in the LEI reader path, so Rust does not fabricate wavelength fields.

- [x] **Aperio SVS** (`.svs`)
  - Name/emission gap: Java maps Aperio `Dye`, `Emission Wavelength`, and
    `Excitation Wavelength` to OME channels; Rust keeps Aperio key/value pairs
    in `series_metadata` and emits only channel stubs.
  - Fixed in `src/formats/svs.rs`; Aperio `Dye` values are promoted to OME
    channel names and parsed emission/excitation wavelength fields populate the
    first OME channel like Java.

- [x] **Hamamatsu NDPI / NDPIS** (`.ndpi`, `.ndpis`)
  - Name/emission gap: Java reads NDPI filter-set name and wavelength tags and
    sets OME channel name/emission. Rust's NDPI OME builder emits channel stubs,
    and NDPIS inherits that behavior through child NDPI readers.
  - Fixed in `src/formats/tiff_wrappers.rs`; NDPI OME channel projection now
    reads tags 65434 (`FILTER_SET_NAME`) and 65451 (`WAVELENGTH`) like Java.
    NDPIS inherits the behavior through child NDPI readers.

- [x] **CV7000 / Yokogawa** (`.wpi`, `.tif`, `.tiff`, related CV7000 layouts)
  - Count gap: Rust custom OME image construction returns images with no channel
    list, so it can be shorter than non-RGB `SizeC`.
  - Name gap: Java sets channel names; Rust does not project parsed channel
    names to OME channels.
  - Fixed in `src/formats/extended.rs`; the CV7000 OME builder now emits one
    `OmeChannel` per parsed/acquired channel with Java-style
    `Action #..., Channel #..., Camera #...` names.
  - Real-data oracle caveat: the available WPI at
    `userdata/openmicroscopy-images/CV7000/cpg0016/Dest21053D1-15214/Dest210531-152149.wpi`
    still cannot be compared by the Java parity harness because upstream Java
    `CV7000Reader.initFile` leaves `channels` null when `MeasurementDetail.mrf`
    is absent and then calls `channels.sort(...)`, throwing
    `NullPointerException` before OME metadata is emitted. The Rust fix is
    therefore source-backed and unit-checked (`cargo test cv7000 --lib`), not
    Java-oracle-proven on that mounted sample.
  - 2026-08-14 refresh: the focused manifest oracle still compares zero files
    because of the same Java `channels.sort(...)` NPE; `cargo test cv7000
    --lib -- --nocapture` passes the channel-name, OME projection, and `.wpi`
    dispatch tests.
  - Dispatch parity fix: `.wpi` now routes directly to `YokogawaReader` through
    both registry suffix dispatch and Java reader-order mapping, matching
    Java's suffix-based `CV7000Reader` selection. Unit coverage includes an
    incomplete WPI that now fails with the CV7000-specific missing-companion
    error instead of being claimed by another reader.

- [x] **Cellomics C01/DIB** (`.c01`, `.dib`)
  - Color gap: Java interprets MDB `asnProtocolChannel.CompositeColor` as BGR
    before creating an OME `Color(red, green, blue, 255)`. Rust packed the
    24-bit value as RGB, swapping red and blue in OME channel colors.
  - Fixed in `src/formats/extended.rs`; Cellomics OME channel color projection
    now converts Java's BGR integer to opaque RGBA. Rust also now emits
    Java-style all-series OME images, default plane records, and Plate/Well/
    WellSample graph, and stops adding the Rust-only original-metadata map
    annotation for the sampled DIB. Verified with
    `cellomics_ome_color_converts_java_bgr_with_opaque_alpha`,
    `cargo test cellomics --lib`, and metadata-only Java parity on the BBBC001
    DIB sample (3/3 core series OK, OME OK).

- [x] **Hamamatsu VMS Rust-only OME map annotations** (`.vms`, `.vmu`)
  - Java `HamamatsuVMSReader` emits OME image/instrument/objective metadata for
    the sampled VMS file but no original-metadata map annotations.
  - Rust added one original-metadata map annotation per public VMS series from
    `series_metadata`, producing `mapAnnotationCount=3` while Java reported 0.
  - Fixed in `src/formats/extended.rs`; the VMS OME builder no longer calls
    `add_original_metadata_annotations`. Focused metadata-only Java parity now
    passes for mounted CMU-1 VMS: 3/3 core series OK and OME OK. Pixel checks
    remain JPEG-relaxed work because the series are large JPEG-backed
    whole-slide images.

- [x] **Operetta** (`.xml`, `.tif`, `.tiff`, Operetta layouts)
  - Name/emission gap: Rust stores channel name/emission under
    `operetta.Channel{c}.*` metadata keys that the generic OME mapper does not
    consume; Java sets the corresponding OME channel fields.
  - Fixed in `src/formats/hcs2.rs`; Operetta now adds generic
    `channel.N.name`, `channel.N.emission_wavelength`, and
    `channel.N.excitation_wavelength` aliases while preserving original
    metadata. Rust also now dispatches Operetta `Index.idx.xml`,
    `Index.ref.xml`, and `Index.xml` before broad XML suffix readers; keeps Java
    metadata initialization alive for HTTP/missing payload URLs; preserves Java
    `CoreMetadata` fallback defaults when no valid TIFF IFD is available; and
    projects Java-style image names, physical sizes, objective graph, and
    Plate/Well/WellSample metadata through the shared HCS OME builder.
    Metadata-only Java parity now passes for the real `Index.ref.xml` sample:
    384/384 core series OK and OME OK.

- [x] **Columbus** (`.xml`, `.tif`, `.tiff`, Columbus layouts)
  - Name/emission gap: Rust stores `columbus.Channel{c}.Name` and
    `columbus.Channel{c}.EmissionWavelength` metadata, but does not project them
    into `OmeChannel`; Java does.
  - Fixed in `src/formats/hcs2.rs`; Columbus now adds generic channel
    name/emission/excitation aliases consumed by the shared OME mapper.
  - All-series/SPW OME gap: Java `ColumbusReader` calls
    `MetadataTools.populatePixels(store, this, true)` for all series, then
    writes one Plate with Well/WellSample/Image links, image names, physical
    sizes, channel names/wavelengths, and plane records. Rust's assembled-HCS
    macro previously used the default `ome_metadata()` fallback, so OME
    contained only the current series and no plate graph. Fixed in
    `src/formats/hcs2.rs`; focused metadata-only Java parity now passes for
    `/big/henriksson/ome_images/PerkinElmer-Columbus/idr0019/22_lines_HC_EGF_200145913/MeasurementIndex.ColumbusIDX.xml`
    with 3696/3696 core series OK and OME OK.

- [x] **PerkinElmer / Opera Phenix** (`.htm`, `.tif`, `.tiff`, related layouts)
  - Emission gap: Java parses emission wavelengths and sets OME channel emission
    wavelengths; Rust's OME override sets date/physical sizes/planes but does
    not populate channel emission wavelengths.
  - Fixed in `src/formats/perkinelmer.rs`; PerkinElmer HTML `Exposure` entries
    now populate Java-style emission/excitation wavelength vectors, and the OME
    projection writes those values to per-channel metadata.

- [x] **ScanR** (`.xml`, `.tif`, `.tiff`, ScanR layouts)
  - Name gap: Rust parses channel names and stores them as original metadata,
    but does not project them to OME channel names; Java sets OME channel names.
  - Fixed in `src/formats/hcs2.rs`; ScanR channel names are also written as
    generic `channel.N.name` metadata.
  - All-series/SPW OME gap: Java `ScanrReader` populates OME for every assembled
    series and writes Plate/Well/WellSample links, Java image names, channel
    names, physical sizes, and baseline plane records. Rust's assembled-HCS
    macro previously returned OME for only the active series with no SPW graph.
    Fixed in `src/formats/hcs2.rs`; focused metadata-only Java parity now
    passes for
    `/big/henriksson/ome_images/ScanR/idr0009/0307-10--2007-05-30/experiment_descriptor.xml`
    with 357/357 core series OK and OME OK.

- [x] **CellVoyager** (`.xml`, `.tif`, `.tiff`, CellVoyager layouts)
  - Name gap: Rust parses enabled channels and generic channel identifiers, but
    does not set OME channel names from Java's channel info names.
  - Fixed in `src/formats/hcs2.rs`; CellVoyager now builds Java-style channel
    names from excitation/emission/fluorophore XML and projects them into OME.

- [x] **Vectra / QPTIFF** (`.qptiff`, `.tif`, `.tiff`)
  - Name gap: Rust uses XML `<Name>` as `channel.N.name`; Java prefers
    `<Biomarker>` as the OME channel name and uses `<Name>` as the fluor when a
    biomarker exists.
  - Fixed in `src/formats/extended.rs`; QPTIFF channel names now prefer
    `<Biomarker>` and fall back to `<Name>`, matching Java's OME channel-name
    precedence.

- [x] **Prairie** (`.xml`, `.cfg`, `.tif`, `.tiff`, Prairie layouts)
  - Name/emission gap: Rust captures `channelName` only as original metadata and
    does not average wavelength min/max into OME emission wavelength; Java sets
    OME channel names and emission wavelengths.
  - Fixed in `src/formats/prairie.rs`; sorted active channel indices now get
    generic OME channel-name aliases and `<File>` wavelength min/max values are
    averaged into channel emission wavelengths.

- [x] **Fake** (`.fake`, `.fake.ini`)
  - Name/emission gap: Rust validates/stores requested wavelength parameters as
    original metadata, but the default OME mapper does not consume those keys.
  - Java also supports `.fake.ini` channel names/wavelengths; Rust does not
    currently implement that path.
  - Partially fixed for filename `emission_N` / `excitation_N` parameters by
    also projecting them through generic `channel.N.*_wavelength` metadata.
  - Fixed in `src/formats/fake.rs`; `.fake.ini` default parameters now feed the
    same token parser as Java, and `[series_N]` `ChannelName_N`,
    `ChannelEmissionWavelength_N`, and `ChannelExcitationWavelength_N` entries
    are projected to OME channel metadata.

- [x] **ICS / IDS** (`.ics`, `.ids`)
  - Name gap: Rust populates OME emission/excitation wavelengths, but does not
    parse/project Java's channel names from `parameter ch` / history metadata.
  - Fixed in `src/formats/ics.rs`; `parameter ch` is now parsed into per-channel
    names and projected to OME channels alongside the existing
    emission/excitation wavelength projection.

- [x] **Volocity** (`.mvd2`, `.aisf`, related Volocity layouts)
  - Name gap: Rust records channel names as original metadata only; Java sets
    OME channel names.
  - Fixed in `src/formats/volocity.rs`; stack channel names are now also stored
    as generic `channel.N.name` metadata consumed by shared OME projection.

## Checked and not currently listed for channel fixes

- **Generic readers using `OmeMetadata::from_image_metadata` only**: channel
  count is generally OK because the shared conversion sizes channels from
  effective non-RGB `SizeC`.
- **FV1000/OIF/OIB**, **Nikon TIFF**, **Nikon Elements TIFF**,
  **Leica LMS/XLEF**, **ND2**, **SlideBook7**, **FlowSight**, **DICOM**,
  **Imaris HDF**, **TissueFAXS**, **InCell**, **Micro-Manager**, **CellH5**,
  and **BDV** were checked and appear to preserve channel count and/or
  Java-populated channel name/emission fields for the audited behavior. FV1000
  now also has broader mounted-OIB parity evidence after fixing Java-style
  legacy double parsing and Axis Common Z/T spacing in `src/formats/olympus.rs`:
  3 files, 3 core series OK, 3 OME OK, and bounded pixels bitwise. Nikon
  Elements TIFF remains fixture-limited rather than code-drift-listed: source
  parity matches Java's `NikonElementsTiffReader` channel-name/emission/
  excitation loops, `cargo test nikon_elements --lib` passes the synthetic
  channel/list/object-graph coverage, and the current mounted manifests do not
  include a real NIS-Elements TIFF fixture for Java parity.
- **Flex**, **Zeiss LSM**, **Zeiss ZVI**, and **Leica LIF** now also have
  fixture-backed Java parity evidence for the sampled channel-list/name/OME
  paths. Metadata-only Java parity for `.tmp/auditlist2-channel-ome-local.paths`
  passes 4 files, 9 core series, and 4 OME documents with pixels intentionally
  skipped for this channel-count/name evidence slice.
- **NRRD**, **SDT/FLIM**, **LiFlim/IPW/iVision/IM3/SimFCS**, **Leica SCN**, and
  **Ventana BIF** were checked for this specific channel-name/emission behavior;
  no matching Java channel-name/emission projection gap was identified in the
  audited paths.
- Remaining Rust format files were scanned for custom `ome_metadata()` methods,
  manual `OmeChannel` construction, channel-name/wavelength metadata keys, and
  mapped Java `setChannelName` / `setChannelEmissionWavelength` calls. No
  additional format-specific channel-list/name/emission gaps were identified.

- [x] **Leica LIF broader OME filter/name parity** (`.lif`)
  - Broader real-data manifest:
    `.tmp/auditlist2-lif-broader.paths`.
  - Fixed:
    - LIF RGB sample count is now exposed through Java-visible
      `lif.rgb_channel_count`; broader parity now reports 34/34 core series OK.
    - Rust-only eager `MultiBand` dye-name prefill was removed, so channel names
      now come from the Java-style scanner/filter/detector accumulator.
    - Empty LIF channel names are preserved where Java writes `Some("")`.
    - Laser-derived channel excitation is set when Java assigns
      `<LightSourceSettings>`.
    - Shared OME ROI support now includes `<Label>` shapes, and LIF ROI output
      emits Java's label-plus-geometry shape pair.
    - Java's two LIF filter passes are now mirrored: explicit filter objects
      from initialized filter settings, plus the later per-channel light-path
      filter-ID state machine.
    - Scanner/filter/detector accumulator arrays now preserve Java null-vs-empty
      behavior, including sequential-master detector definitions that Java does
      not use to backfill every tau channel name.
  - Current verification:
    `cargo test lif --lib` passes 46/46, and metadata-only Java parity over
    `.tmp/auditlist2-lif-broader.paths` passes for 7 files, 34/34 core series,
    and 7/7 OME documents. Pixels were intentionally skipped for this metadata
    regression check.

# Issue-specific Java parity fixes

Audit date: 2026-08-13

Scope: GitHub issues #6 and #8 plus the Java Bio-Formats 8.1.0 readers they
reference.

## Formats to fix

- [x] **Olympus cellSens / VSI / ETS region assembly** (`.vsi`, `.ets`)
  - Issue: https://github.com/henriksson-lab/bioformats-rs/issues/6
  - Java `CellSensReader.openBytes` intersects each tile directly with the
    requested `Region(x, y, w, h)`, applies `tileOriginX/Y` only when both are
    present, and compacts copied spans with `outputRow` / `outputCol`.
  - Rust `EtsVolume::assemble_region` clipped tiles to the full image first,
    then cropped from a synthetic full-plane placement. That diverged from
    Java for non-zero-origin regions and tile-origin-shifted ETS planes.
  - Fixed in `src/formats/flim2.rs`; `assemble_region` now mirrors Java's
    direct requested-region intersection while preserving the direct-region
    optimization.
  - Verified with `cargo test ets_assemble_plane_applies_tile_origin_crop --lib`.

- [x] **Nikon ND2 binary LV metadata and split-channel planes** (`.nd2`)
  - Issue: https://github.com/henriksson-lab/bioformats-rs/issues/8
  - Java `ND2Reader` parses binary `ImageAttributesLV!` records for
    `uiWidth`, `uiHeight`, `uiComp`, `uiBpcInMemory`, and
    `uiBpcSignificant` before dimension fallbacks.
  - Rust previously derived dimensions from XML-only parsing first and could
    fall back to square-size guesses for binary-only `ImageAttributesLV!` files.
  - Java's special `ImageMetadataLV!` order/count scan starts from the stream
    position after reading the first 12 chunk-name bytes.
  - Rust now preserves that chunk-name suffix context for the special binary
    `ImageMetadataLV!` scan and consumes the zero-skip sentinel byte like Java.
    Verified with
    `cargo test nd2_image_metadata_lv_scan_starts_after_first_twelve_name_bytes_like_java --lib`.
  - Java reports one logical plane per channel when `sizeC > 1` and splits
    interleaved stored chunks in `openBytes`; Rust previously mapped logical
    plane indexes directly to stored `ImageDataSeq` chunks.
  - Fixed in `src/formats/nd2.rs`; Rust now parses binary `uiWidth`,
    `uiHeight`, `uiComp`, `uiBpcInMemory`, and `uiBpcSignificant` during the
    early `ImageAttributes` pass before dimension fallbacks, preserves Java's
    special `ImageMetadataLV!` scan origin, exposes split-channel logical
    planes, maps them back to stored interleaved chunks, splits the requested
    channel on decode, and reports compressed extraction as unsupported for
    split-channel logical planes that require channel extraction.
  - Verified with
    `cargo test nd2_binary_image_attributes_lossless_matches_java_flags --lib`,
    `cargo test nd2_image_metadata_lv_scan_starts_after_first_twelve_name_bytes_like_java --lib`,
    and
    `cargo test nd2_split_channel_planes_map_logical_plane_to_stored_frame_like_java --lib`.

# Systematic parity audit findings

Audit date: 2026-08-13

Scope: checklist-driven audit from `PARITY_AUDIT_PLAN.md`. Findings in this
section come from source-level Java/Rust comparisons and should be converted to
format-specific fixes plus targeted tests.

## Fixed during systematic audit

- [x] **Khoros XV suffix detection** (`.xv`)
  - Java `KhorosReader` registers only the `xv` suffix.
  - Rust `src/formats/khoros.rs` also claimed `.viff` by name, which could
    route non-Java-mapped VIFF files to the Khoros reader.
  - Fixed in `src/formats/khoros.rs`; name detection now matches Java's `.xv`
    registration while byte detection remains the Java `0xab01` magic check.
  - Verified with
    `cargo test name_detection_matches_java_xv_suffix_only --lib`.

- [x] **Targa RLE `0x80` packet** (`.tga`, `.tpic`)
  - Java `TargaRLECodec` treats signed `-128` (`0x80`) as a no-op packet.
  - Rust decoded `0x80` as a one-pixel run packet, preserving the TGA spec but
    diverging from Java Bio-Formats behavior.
  - Fixed in `src/formats/raster.rs`; `targa_rle_decompress` now skips `0x80`
    like Java.
  - Verified with `cargo test rle_decompress_0x80_packet_is_java_noop --lib`.

- [x] **IPLab filename suffix parity** (`.ipl`)
  - Java `IPLabReader` registers only the `ipl` suffix, with
    `suffixNecessary = false` so extensionless/nonstandard names are accepted by
    the `iiii`/`mmmm` header probe rather than by broad filename matching.
  - Rust also claimed `.ipm` by filename, which could route unrelated files to
    IPLab before header validation.
  - Fixed in `src/formats/norpix.rs`; name detection now matches Java's `.ipl`
    registration while byte probing still accepts Java IPLab headers.
  - Verified with
    `cargo test iplab_name_detection_matches_java_registered_suffix --lib`.

- [x] **MIAS TIFF Software byte detection** (`.tif`, `.tiff`)
  - Java `MIASReader.isThisType(RandomAccessInputStream)` reads the first TIFF
    IFD and accepts Software values beginning with `eaZYX`, `SCIL_Image`, or
    `IDL`.
  - Rust returned false for all MIAS byte probes, so valid MIAS TIFFs could be
    missed when registry dispatch reached the stream probe path.
  - Fixed in `src/formats/mias.rs`; the byte probe now parses the first IFD via
    the shared TIFF parser and checks the Java Software prefixes.
  - Verified with
    `cargo test mias_byte_probe_matches_java_software_prefixes --lib`.

- [x] **TopoMetrix byte-probe fallthrough parity** (`.tfr`, `.ffr`, `.zfr`, `.zfp`, `.2fl`)
  - Java `TopometrixReader.isThisType(RandomAccessInputStream)` reads the
    6-byte `#R...` probe but falls through to `return false`, so stream probing
    does not claim TopoMetrix data.
  - Rust honored the apparent `#R` magic and could claim extensionless or
    nonstandard TopoMetrix-like streams that Java would not claim.
  - Fixed in `src/formats/afm.rs`; byte probing now returns false like Java,
    leaving suffix-based dispatch unchanged.
  - Verified with
    `cargo test topometrix_byte_probe_matches_java_false_fallthrough --lib`.

## Formats to fix

- [x] **MIAS/CellWorx structured HCS and analysis metadata parity** (`.tif`, `.tiff`, `.txt`, `.htd`, `.pnl`, `.log`)
  - [x] CellWorx HTD wave names now populate `channel.N.name`; well logs now
    derive acquisition date, scan-origin sample/plane positions, scan-area
    physical sizes, detector-settings gain, and EX/EM wavelengths into stable
    metadata keys consumed by the local OME projection where supported. OME
    WellSample PositionX/PositionY now consumes the parsed scan-origin values,
    matching Java `CellWorxReader.parseWellLogFile`
    `setWellSamplePositionX/Y`. The checked-in MetaXpress fixture now covers HTD
    wave-name projection into both series metadata and OME channels.
  - [x] MIAS `.txt` entry points now resolve to a TIFF dataset like Java, plate
    `Nugenesistemplate.txt` contributes channel names, physical sizes,
    objective, acquisition date, and exposure metadata, and Batchresults/results
    companions are indexed per well with ROI/detail/overlay/mask counts.
  - [x] MIAS detail result files now project Java-style ellipse ROIs into OME:
    Java `MIASReader.populateROI` uses `Col`, `Row`, `Label`, and `Cell Diam.`
    from `Well<nnnn>_mode<n>_z<nnn>_t<nnn>_detail.txt`; Rust now records
    `roi.N` metadata with ellipse center, equal radii, label/name, TheT, and
    TheZ for the existing generic OME ROI projection.
  - [x] MIAS and CellWorx/MetaXpress OME metadata now includes Java-style SPW
    Plate/Well/WellSample graph projection. CellWorx builds all grid wells and
    links selected well x field images with Java's row-major image indexing;
    MIAS builds one well sample per parsed well using Java's well-number to
    row/column mapping. MIAS Plate.Name now follows Java's final store state:
    after `parseTemplateFile`, Java overwrites template `Carrier`/`Barcode`
    values with the plate directory suffix after the first hyphen, so Rust uses
    that directory-derived label for the OME plate graph while retaining parsed
    template scalars in series metadata.
  - [x] CellWorx OME now emits explicit populated `<Plane>` entries when no
    per-plane metadata already created them, matching Java
    `MetadataTools.populatePixels(..., true)`. The mounted MetaXpress `idr0081`
    HTD parity run now matches series/core metadata for 384 series and no
    longer has the OME plane-count gap.
  - [x] CellWorx OME now inherits Java-visible physical sizes and instrument
    metadata from the first real MetaMorph TIFF companion. Rust first templates
    from the translated `MetamorphReader` so UIC `XCalibration`/`YCalibration`
    values win over generic TIFF `72 dpi` resolution metadata, then falls back
    to normal registry dispatch. The mounted MetaXpress `idr0081` HTD parity
    run now matches OME `PhysicalSizeX/Y=1.72` plus the single
    instrument/detector graph.
  - [x] Registered MetaXpress HTD opening now mirrors Java
    `MetaxpressTiffReader.getTiffFiles` for UUID-suffixed flat-directory TIFF
    companions before falling back to nested `TimePoint_<t>/ZStep_<z>` layouts.
    This fixed the mounted `idr0005` case where Java found
    `2011-04-19-plate-1_A01_s1_[...].tif` siblings but Rust's inherited
    CellWorx exact-name probe did not. The same mounted pass also verifies the
    lossy HTD text parsing fix for invalid UTF-8 in `idr0008`, all-series OME
    delegation for the root `idr0006` HTD, and MetaMorph UIC calibration
    precedence (`PhysicalSizeX/Y=1.6125`) over generic TIFF 72 dpi metadata.
  - [x] MIAS `AllModesOverlay.tif` channel colors now mirror Java
    `MIASReader.getChannelColorFromFile`: Rust reads the first TIFF ColorMap
    entry for red/green/blue, maps a unique maximum to opaque red/green/blue,
    maps ties to opaque black, and projects the result to `OmeChannel.color`
    for all series on that channel like Java `setChannelColor`.
  - [x] The shared OME model now supports `Mask` ROI shapes with stroke/fill
    colors and `MaskPixels.BinData`, and `MiasReader` exposes an opt-in
    `set_automatically_parse_masks` flag matching Java's default-off
    `setAutomaticallyParseMasks`. The bounded parser covers indexed palette
    overlay TIFFs, removes grayscale pixels, thresholds the selected RGB plane,
    and writes Java-style packed mask bits.
  - [x] MIAS mask overlay parsing now also covers Java's non-indexed RGB
    channel-splitting path for uncompressed 8-bit RGB overlays, including
    grayscale-pixel suppression, MSB-first bit packing, and valid-mask
    filtering.
  - [x] The shared OME model now supports image-level `<ROIRef>` links, and
    MIAS detail and mask ROIs now populate Java-style image ROI references
    while preserving global ROI IDs across multi-well OME assembly.
  - [ ] Remaining fixture evidence: coverage is still thin: the available checked-in
    MetaXpress sample covers HTD parsing plus one present and one missing TIFF
    companion, but has no scan logs or MIAS template/results/ROI/overlay/mask
    companions, and there is no fixture-backed Java/Rust OME-XML + pixel CRC
    comparison matrix for those cases.
  - 2026-08-14 fixture refresh: local searches under
    `/big/henriksson/ome_images`, `testdata`, `userdata`, `external-fixtures`,
    and `bench` found only the already-audited MetaXpress HTD datasets. No MIAS
    `Nugenesistemplate.txt`, `Batchresults`, `AllModesOverlay.tif`, detail/mask
    companion project, CellWorx `.pnl`, or scan-log-rich fixture was available
    for a stronger Java/Rust parity row.
  - 2026-08-13 MIAS/CellWorx follow-up audit rechecked the local Java sources
    at `java-bioformats/components/formats-gpl/src/loci/formats/in/MIASReader.java`
    and `CellWorxReader.java` against the current Rust implementation. No
    additional bounded fix could be proven from the checked-in fixtures. The
    remaining source-visible but fixture-unproven areas are CellWorx
    multi-wavelength per-companion channel metadata copied through Java
    `MetadataConverter.convertChannels`, plate acquisition start/end and maximum
    field count fields not represented by the current local OME model, and MIAS
    result/template/overlay/mask parity on real companion-rich datasets. Java
    `MIASReader.setAutomaticallyParseMasks` remains default-off and stateful;
    Rust's opt-in mask parsing flag already follows that behavior.
  - Verified with `cargo test mias --lib`, `cargo test metamorph --lib`,
    `cargo test metaxpress --lib`, `cargo test cellworx --lib`, and
    `cargo test --test stub_cellworx_test`. Metadata-only Java parity for
    `.tmp/auditlist2-metaxpress-mounted.paths` now passes 5 mounted
    MetaXpress HTDs, 4,808 core series, and 5 OME files with pixels
    intentionally skipped. `tests/stub_cellworx_test.rs` is an explicit Cargo
    integration target for the external `testdata/metaxpress/` sample; it skips
    cleanly when that sample is absent.

- [x] **Native InCell XDCE XML encoding and OME plane parity** (`.xdce`)
  - Mounted real data
    `/big/henriksson/ome_images/InCell2000/zenodo-14777242/Training_Demo_1.xdce`
    exposed two Java-visible drifts: Rust rejected ISO-8859-1 XML via
    `read_to_string`, while Java `XMLTools.parseXML(byte[], handler)` honors
    the XML declaration; after that, Rust emitted no OME Plane records while
    Java `MetadataTools.populatePixels(..., true)` emitted one default Plane per
    image plane.
  - Fixed in `src/formats/incell.rs`; the native XDCE parser now decodes XML
    using BOM/XML-declared encodings and the OME builder emits Java-style
    default ZCT planes for every series.
  - Verified with focused Java parity on the mounted XDCE: 8/8 core series OK,
    OME OK with 8 images and 512 planes, and bounded pixel parity reports 8
    bitwise series with 3/3 checks each.

- [x] **NIfTI paired-file suffix overclaim** (`.hdr`, `.img`)
  - Java `NiftiReader.isThisType(String, open)` accepts `.nii` (including
    compressed suffix handling for `.nii.gz`) by name, but paired `.hdr`/`.img`
    datasets require opening/probing the sibling header.
  - Rust claimed `.hdr` and `.img` by name alone, which could route unrelated
    files to the NIfTI reader before byte/open probing.
  - Fixed in `src/formats/nifti.rs`; name-only detection now matches Java's
    sufficient suffix behavior while `set_id` still supports valid paired
    `.hdr`/`.img` datasets.
  - Verified with `cargo test name_detection_matches_java_nii_suffix_only --lib`.

- [x] **DICOM name-only suffix overclaim** (`.jp2`, `.raw`, `.ima`)
  - Java `DicomReader` registers several suffixes, but its override only treats
    `dic`, `dcm`, `dicom`, `j2ki`, and `j2kr` as sufficient by name.
  - Rust also claimed `.jp2`, `.raw`, and `.ima` by name, which diverged from
    Java reader-selection behavior.
  - Fixed in `src/formats/dicom.rs`; byte probing still accepts valid DICOM
    streams without relying on those fallback suffixes.
  - Verified with
    `cargo test dicom_name_detection_matches_java_sufficient_suffixes --lib`.

- [x] **NRRD gzip non-zero-origin region reads** (`.nrrd`, `.nhdr`)
  - Java `NRRDReader.readGZIPPlane` writes gzip region rows using offsets based
    on `(row + y) * readLen`.
  - Rust inflates the full gzip payload and crops as a normal packed plane.
    Source-level audit indicates non-zero `y` regions can diverge.
  - Fixed in `src/formats/nrrd.rs`; gzip region reads with non-zero `y` now
    return a clean error instead of silently returning bytes that Java's
    region-sized buffer path cannot produce.
  - Verified with `cargo test gzip_region_nonzero_y_rejects_like_java --lib`.

- [x] **IMAGIC minimal metadata physical-size projection** (`.hed`, `.img`)
  - Java `ImagicReader` always parses core metadata and image name, but only
    writes physical size X/Y/Z to OME when metadata level is not `MINIMUM`.
  - Rust projected those physical sizes regardless of `MetadataLevel`.
  - Fixed in `src/formats/imagic.rs`; `set_metadata_options(Minimal)` now keeps
    core metadata, image name, and pixels while skipping physical-size OME
    projection like Java.
  - Verified with
    `cargo test imagic_minimal_metadata_skips_physical_sizes_like_java --lib`.

- [x] **PSD reader suffix overclaim** (`.psb`)
  - Java `PSDReader` registers only the `psd` suffix, with byte-magic detection
    still available because `suffixNecessary` is false.
  - Rust also claimed `.psb` by name, which diverged from Java reader
    selection.
  - Fixed in `src/formats/psd.rs`; name detection now accepts `.psd` only while
    byte detection still accepts the `8BPS` magic.
  - Verified with
    `cargo test psd_name_detection_matches_java_suffix_registration --lib`.

- [x] **MRC truncated-payload open semantics** (`.mrc`, `.st`, related)
  - Java accepts metadata for short declared payloads and leaves the caller
    buffer unchanged when a requested plane falls outside file length.
  - Fixed in `src/formats/mrc.rs` and `tests/format_tests.rs`; Rust no longer
    rejects the short declared payload during `set_id`, and unavailable or
    partial planes return zero-filled buffers like Java's untouched caller
    buffer.
  - Verified with `cargo test mrc_reader --lib` and `cargo test mrc --lib`.

- [x] **MRC reader suffix overclaim** (`.ccp4`)
  - Java `MRCReader.MRC_SUFFIXES` is `mrc`, `st`, `ali`, `map`, `rec`, `mrcs`.
  - Rust also claimed `.ccp4` by name, which diverged from Java reader
    selection. Byte probing still accepts valid MRC/CCP4 headers.
  - Fixed in `src/formats/mrc.rs`; the reader name check now matches Java's
    suffix list exactly.
  - Verified with `cargo test mrc_reader_accepts_java_suffixes_by_name --lib`.

- [x] **Fake structured OME graph parity** (`.fake`, `.fake.ini`)
  - Java `FakeReader` fabricates structured OME SPW/HCS, ROI shapes,
    annotations, microbeam, filter/light-source, and instrument metadata from
    filename or `.fake.ini` parameters.
  - Rust now projects representable FakeReader graph fields into structured OME:
    SPW plate/well/well-sample metadata, all-series images, channel
    names/wavelengths/colors, physical sizes, acquisition date, instrument/
    objective/detector/light-source/filter/dichroic references, supported ROI
    shapes, and supported annotations.
  - Java label, mask binary payload, and microbeam details that the current Rust
    OME model cannot represent directly are emitted as explicit map annotations
    rather than hidden in generic original metadata.
  - Fixed in `src/formats/fake.rs`. Verified with `cargo test fake --lib`.
  - Remaining fixture gap: exact Java/Rust OME-XML comparison for complex
    FakeReader XMLMockObjects combinations would strengthen evidence.

- [x] **CellH5 ROI metadata fidelity** (`.ch5`)
  - Java parses segmentation ROIs from bounding boxes, names, label IDs, T/C/Z,
    and stroke colors.
  - Rust now populates OME ROI rectangle geometry, ROI names/label IDs, and
    Z/T/C references from the same HDF5 sources Java uses.
  - Remaining non-blocking schema gap: Java also sets rectangle text and stroke
    color, but Rust `OmeShape::Rectangle` has no text/stroke fields yet.
  - Verified with
    `cargo test cellh5_reader_populates_java_style_roi_metadata --lib`.

- [x] **ZIP wrapper metadata/API forwarding** (`.zip`)
  - Java `ZipReader` forwards child-reader metadata store/core behavior.
  - Rust delegates pixels/series/core metadata but does not forward enriched
    `ome_metadata`, `lookup_table`, `resolution_count`, `set_resolution`,
    `resolution`, or compressed extraction APIs.
  - Fixed in `src/registry.rs` and `src/formats/zip.rs`; ZIP now forwards
    enriched OME metadata, LUTs, resolution APIs, and compressed extraction APIs
    to the selected inner reader.
  - Verified with
    `cargo test zip_reader_opens_inner_image_and_rejects_unrecognized_entries --lib`.
  - Real-data oracle caveat: `/big/henriksson/ome_images/u-track/integrins.zip`
    still cannot be compared by the Java parity harness because Java resolves
    the inner `case1_higherSNR.ome.tiff` as a working-directory file and throws
    `FileNotFoundException` before metadata is emitted. A 2026-08-14
    metadata-only oracle refresh reproduced this blocker. Keep u-track ZIP as
    source-backed until the Java oracle can compare mapped ZIP entries.

- [x] **DICOM encapsulated tiled WSI reads** (`.dcm`, `.dicom`)
  - Java supports tiled DICOM region reads, including compressed JPEG,
    JPEG2000, and RLE tile cases.
  - Rust now decodes encapsulated tile frames through the existing JPEG,
    JPEG2000, and RLE paths and reuses that decoder for WSI tile stitching
    instead of rejecting encapsulated WSI tiles.
  - Verified with `cargo test --lib dicom_` and
    `cargo test --test new_features_test dicom`.

- [x] **IM3 native hierarchy/open semantics** (`.im3`)
  - Java `IM3Reader` builds core metadata from `DataSet/Shape` records and reads
    raw dimensions from `Data` records later during pixel access.
  - Rust implements the bounded native subset and verifies interleaved channel
    extraction.
  - Fixed traversal in `src/formats/flim2.rs`; Rust now only collects native
    datasets from child containers under Java's `DataSet` container, not any
    nested `Shape` + `Data` pair. Verified with
    `cargo test im3_only_collects_datasets_under_java_dataset_container --lib`.
  - Fixed mismatch timing in `src/formats/flim2.rs`; Rust now accepts
    `Shape`/`Data` dimension mismatches during metadata parsing like Java,
    preserves core metadata from `Shape`, records a diagnostic, and fails
    cleanly only when a later logical pixel request cannot be satisfied by the
    `Data` payload.
  - Verified with `cargo test im3_ --lib`.

- [ ] **SlideBook 7 native project parity (fixture-blocked broader support)** (`.sldy`, `.sldyz`)
  - Java `SlideBook7Reader` initializes through `DataLoader.LoadMetadata()` and
    supports broader native project metadata than Rust's current
    `.dir/*.imgdir/ImageRecord.yaml` plus `ImageData_*.npy/.npyz` subset.
  - Rust verifies scalar NPY/NPYZ plane reads and `.sldyz` extraction. It now
    mirrors Java `GetNumPositions` for OME plane DeltaT/stage-position mapping:
    logical T is split into acquisition timepoint and montage position.
  - Rust now also projects typed image name/description, Java-style physical
    size X/Y from `CLensDef70.mMicronPerPixel / COptovarDef70.mMagnification *
    CExposureRecord70.mXFactor`, objective model, objective NA, and Java-style
    objective magnification from `CLensDef70.mActualMagnification *
    COptovarDef70.mMagnification` into structured OME. Rust now also mirrors
    Java `initFile` objective defaults: `setObjectiveCorrection(Other)` and
    `setObjectiveImmersion(Other)` for the emitted SlideBook 7 objective.
    Multi-capture native OME now also mirrors Java's instrument topology:
    `initFile` creates one `Instrument:0`, links every capture image to it, and
    appends one `Objective:0:<capture>` per capture.
  - Additional current coverage: synthetic tests exercise typed
    `CChannelRecord70`/`CExposureRecord70` channel names, exposure, emission
    wavelength, mask/annotation/elapsed-time/stage-position/aux-data metadata,
    `.sldyz` nested-root extraction, gzip `.npyz`, compression dictionaries,
    multi-digit channel filenames, and Z/T-varying OME planes.
  - Fixed bounded OME plane position/time, image/lens/optovar/x-factor
    projection, Java-style objective correction/immersion defaults, and
    Java-style cube annotation ROI shape projection in
    `src/formats/flim2.rs`. This pass also fixed image-level
    `PhysicalSizeZ`: Rust now mirrors Java `initFile`, which only calls
    `setPixelsPhysicalSizeZ` when `numZPlanes[capture] > 1`; single-Z
    captures can still emit plane `PositionZ` from stage metadata. Verified
    with `cargo test slidebook7_omits_physical_size_z_for_single_z_like_java --lib`
    and `cargo test slidebook7 --lib`. This pass also fixed multi-capture
    objective/instrument sharing; verified with
    `cargo test slidebook7_ome_uses_one_instrument_with_objective_per_capture_like_java --lib`.
    This pass also fixed native image-data file-count parity: Rust now mirrors
    Java `CImageGroup.CountImageDataFiles()`, preserving declared C/T only when
    all declared `ImageData_Ch*_TP*.npy[z]` files exist, using TP0-as-multiple-T
    only for Java's one-file-per-channel single-Z case, and otherwise replacing
    C/T with the max channel/timepoint indices observed in filenames. Verified
    with `cargo test slidebook7_sparse_timepoints_do_not_reuse_tp0_npy_like_java --lib`
    and `cargo test slidebook7 --lib`.
  - Source audit note: Java's current `CImageGroup.GetBytesPerPixel()` is
    hard-coded to return 2, so the Java `bytes % 3 == 0` RGB/BGR branch is
    unreachable for this reader in the local source.
  - Source audit note: Java `CSBFile70.GetListOfImageGroupTitles()` discovers
    only immediate `<slide>.dir/*.imgdir` directories with `ImageRecord.yaml`
    and at least one `.npy`/`.npyz`; Rust's bounded native discovery now
    matches that local Java path for uncompressed `.sldy`, including skipping
    otherwise valid-looking empty `.imgdir` directories instead of failing the
    whole slide during group parsing. Verified with
    `cargo test slidebook7_skips_imgdirs_without_image_data_like_java --lib`.
  - Worker J source/fixture audit (2026-08-13): rechecked `testdata`,
    `tests`, `userdata`, `external-fixtures`, `.tmp`, and mounted
    `/big/henriksson/ome_images` with `find` for `.sldy`/`.sldyz` and
    SlideBook-named files. No real native SlideBook 7 project fixture was
    found; only generated synthetic test sources are present. Source comparison
    against local Java `SlideBook7Reader.java` found and fixed the empty
    `.imgdir` prefilter divergence above; no further small source-backed
    `src/formats/flim2.rs`-only fix was identified without real fixture
    evidence. Focused tests passed: `cargo test slidebook7 --lib`,
    `cargo test --test slidebook7_sldyz_test`, and
    `cargo test --test slidebook7_yaml_metadata_test` (the YAML metadata test is
    intentionally ignored because it documents non-upstream flattening).
  - Worker fixture refresh (2026-08-14): no real native `.sldy` or `.sldyz`
    projects were found in the repo, external manifests, or mounted
    `/big/henriksson/ome_images`; the `.sld` hits there are Trestle descriptors,
    not SlideBook7 native data. Keep this item fixture-blocked until a real
    native project can be compared against Java. 2026-08-14 follow-up source
    audit classified this as a fixture-blocked broader-support gap, not a
    currently identified source-only Rust defect.
  - 2026-08-14 unit refresh: `cargo test slidebook7 --lib -- --nocapture`
    passes 22/22 focused SlideBook7 tests, so the remaining item is still real
    native fixture coverage rather than a currently failing local regression.
  - 2026-08-14 online fixture search: current public OME image downloads
    (`https://downloads.openmicroscopy.org/images/`) do not list a SlideBook or
    SlideBook7 category, and guessed paths
    `https://downloads.openmicroscopy.org/images/SlideBook7/` and
    `https://downloads.openmicroscopy.org/images/SlideBook/` return 404. The
    Bio-Formats 6.10 docs say OME has "a small number of datasets" for
    SlideBook 7, but no public download URL was found. Treat this as likely
    private OME QA/vendor data unless another repository is located.
  - 2026-08-14 source audit found and fixed one remaining source-only fallback
    drift: Java `CSBFile70.GetChannelIndexOfPath` reads only the single digit
    after `_Ch` when sparse `ImageData` files force `CountImageDataFiles()` to
    derive C/T from filenames. Rust now uses that Java one-digit parser for the
    sparse fallback count while preserving full channel indexes for declared
    all-files-present layouts. Verified with
    `cargo test slidebook7_sparse_fallback_counts_only_one_channel_digit_like_java --lib -- --nocapture`
    and `cargo test slidebook7 --lib -- --nocapture` (22/22).
  - Remaining required fix: add fixture-backed support for native project
    layouts beyond the bounded `.imgdir` NPY/NPYZ subset and complete Java-style
    structured OME projection for any still-uncovered channel/instrument fields,
    while preserving the optimized path where parity is proven. Java only adds
    cube annotations as OME ROIs in the local `addAnnotation` path; base,
    FRAP-region, and unknown annotation lists remain original metadata unless a
    real fixture proves another Java projection path.
  - Required tests/fixtures: real `.sldy`/`.sldyz` project with multiple
    captures, positions x timepoints x Z x C, ROI annotations, objective/lens
    metadata, physical sizes, channel names, elapsed times, exposure, and stage
    positions compared against Java OME-XML and pixel CRCs.
  - Worker I fixture audit (2026-08-13): rechecked registered Cargo targets,
    `tests/java_parity_test.rs`, checked-in fixture/test/parity trees, `.tmp`,
    `userdata`, `external-fixtures`, and mounted `/big/henriksson/ome_images`
    with `find` for `.sldy`/`.sldyz`. No real SlideBook 7 project files are
    currently available. The only SlideBook 7 coverage found is generated
    Java-shaped synthetic data in `tests/slidebook7_sldyz_test.rs`,
    `tests/slidebook7_yaml_metadata_test.rs`, and in-module
    `src/formats/flim2.rs` tests. The standalone files are explicit Cargo test
    targets, but they are still generated Java-shaped layouts, not real native
    project evidence. They are useful for bounded source-backed fixes but
    cannot close the native fixture-backed Java/Rust OME-XML + pixel CRC
    requirement. `tests/java_parity_test.rs` currently has no default
    SlideBook 7 fixture input.

- [x] **iVision metadata options and sidecar XML policy** (`.ipm`)
  - Java `IvisionReader` skips XML parsing at `MetadataLevel.MINIMUM` and
    searches for embedded XML after the pixel payload.
  - Rust previously had no effective iVision metadata-level handling and also
    read adjacent `.xml` sidecars, which Java does not do.
  - Fixed sidecar policy in `src/formats/flim2.rs`; Rust now ignores adjacent
    `.xml` sidecars and only scans the embedded tail like Java. Verified with
    `cargo test ivision_ignores_adjacent_xml_sidecar_like_java --lib`.
  - Fixed metadata-level handling in `src/formats/flim2.rs`;
    `IvisionReader::set_metadata_options(MetadataLevel::Minimal)` now skips
    embedded XML parsing while preserving core metadata and pixel reads.
    Verified with
    `cargo test ivision_minimal_metadata_skips_embedded_xml_like_java --lib`
    and `cargo test ivision --lib`.

- [x] **Quesant AFM OME metadata and byte probe** (`.afm`)
  - Java sets image acquisition date, image description, and physical size X/Y
    from Quesant `DATE`, `SDES`/`DESC`, and `HARD` scan-size metadata; Rust
    previously stored these only as original metadata.
  - Java has suffix-only reader registration for arbitrary streams, while Rust
    previously overclaimed any header containing stray `IMAG` bytes in the first
    1024 bytes.
  - Fixed in `src/formats/spm.rs`; `QuesantReader::ome_metadata` now projects
    Java-style image description, acquisition date, and physical size X/Y
    (`scan size / SizeX/Y`), and byte probing only claims the explicit Rust
    strict-raw fixture magic.
  - Verified with `cargo test native_spm_parity_tests --lib`.

- [x] **Seiko SPM OME metadata** (`.xqd`, `.xqf`)
  - Java sets image description and physical size X/Y from header offsets 40,
    156, and 164; Rust previously stored `Comment`, `X size`, and `Y size` only
    as original metadata.
  - Fixed in `src/formats/spm.rs`; `SeikoReader::ome_metadata` projects those
    fields to the structured OME image.
  - Verified with `cargo test native_spm_parity_tests --lib`.

- [x] **Scoped clinical/DCIMG/legacy/raster/SEM/SPM/ZIP parity audit closure**
  - Audited `src/formats/clinical.rs`, `src/formats/dcimg.rs`,
    `src/formats/legacy.rs`, `src/formats/raster.rs`, `src/formats/sem.rs`,
    `src/formats/spm.rs`, and `src/formats/zip.rs` against the matching local
    Java Bio-Formats readers listed in `PARITY_AUDIT_PLAN.md`.
  - No new source-backed behavioral defect was found in the bounded pass.
    Existing completed entries above cover the scoped fixes for Targa RLE,
    ZIP delegation, Quesant byte probing/OME projection, and Seiko OME
    projection.
  - Remaining work is fixture evidence: real Java-generated OME-XML and pixel
    CRC comparisons for representative clinical, DCIMG, legacy raster, SEM,
    and native SPM datasets.

- [ ] **Zeiss LSM MDB/multi-file and rich OME graph** (`.lsm`, `.mdb`)
  - Java `ZeissLSMReader` registers `lsm,mdb`, handles MDB companion/multi-LSM
    datasets, and parses recording/laser/detector/filter/ROI subblocks into the
    OME graph.
  - Rust now name-detects both `.lsm` and Java-registered `.mdb`, byte-detects
    Java MDB probes (`0x5374` at byte offset 4 plus `ID` in the probe block),
    expands single-file `DimensionM * DimensionP` acquisitions into Java-style
    logical series, and enriches OME with physical
    sizes/time/name/channel names and channel colors/LUTs plus supported
    scan-info recording, objective, laser/light-source, detector, filter,
    light-path, and data-channel fields. It also applies Java's acquired-block
    adjacency rules for illumination and detection scan-info subblocks before
    OME projection.
  - Fixed source-backed suffix parity and bounded scan-info OME projection in
    `src/formats/zeiss_lsm.rs`; Rust now mirrors Java's scan-info
    `SUBBLOCK_END` scanning behavior, preserves detector/light-source/filter/
    dichroic records from later subblocks, and emits Java-style per-plane OME
    records for every logical plane. It also mirrors Java's channel-colours
    table parsing (`colorsOffset`), black-color fallback, per-channel 8/16-bit
    ramp LUT selection, and OME `Channel Color` projection.
  - Fixed source-backed MDB byte-sniff parity in `src/formats/zeiss_lsm.rs`:
    Java `isThisType(RandomAccessInputStream)` accepts non-TIFF MDB probes when
    bytes 4..6 are `0x5374` and the remaining 4096-byte probe contains `ID`;
    Rust now accepts that same bounded MDB header while still leaving TIFF-byte
    ownership to the TIFF reader. This is detection parity only: Java then
    parses MDB tables and groups referenced sibling `.lsm` files, while Rust
    does not yet implement MDB companion grouping/parsing.
  - Fixed Java scan-info population ordering in `src/formats/zeiss_lsm.rs`:
    Rust now projects acquired scan-info subblocks before non-acquired
    instrument-only subblocks, preserving Java detector/filter/light-source
    IDs and channel references when skipped detector or laser blocks precede
    acquired channels.
  - Fixed scan-info channel-name over-projection in `src/formats/zeiss_lsm.rs`:
    Java parses detection `CHANNEL_NAME` but does not call `setChannelName`
    from detection subblocks; Rust now leaves OME channel names to the Java
    channel-colours/name table path.
  - Fixed bounded Java-backed plane timing/position OME projection in
    `src/formats/zeiss_lsm.rs`; Rust now parses CZ_LSMINFO timestamp,
    position, and tile-position tables and emits Java-style OME `Plane DeltaT`
    plus `PositionX/Y/Z` for the single-file LSM series model.
  - Fixed bounded Modulo projection in `src/formats/zeiss_lsm.rs`; Rust now
    parses CZ-LSMINFO rotation/phase/illumination counters and emits Java-style
    ModuloAlongZ/C/T metadata with `angle`, `illumination`, and `phase` types.
  - Fixed bounded Java-backed multi-position expansion in
    `src/formats/zeiss_lsm.rs`; Rust now parses CZ-LSMINFO `DimensionP` and
    `DimensionM`, splits the full-resolution IFD list into per-position logical
    series, maps pixel reads through a per-series IFD offset, appends Java-style
    `#N` image-name suffixes, and uses series-local timestamp/position OME
    plane metadata.
  - Verified with `cargo test zeiss_lsm --lib`, `cargo test lsm_ --lib`, and
    `BIOFORMATS_RS_JAVA_PARITY=1 BIOFORMATS_RS_JAVA_PARITY_FILES=lsm/ cargo test --test java_parity_test -- --nocapture`;
    the checked-in LSM fixture now passes core, OME, and pixel parity.
  - Worker E fixture audit checked `testdata`, `tests`, `userdata`,
    `external-fixtures`, `.tmp`, and Java parity harness inputs for `.mdb`,
    multiple `.lsm` companions, overlay ROI, application/SIM, and richer
    full-subblock examples. Only `testdata/lsm/colocsample1b.lsm` is present
    locally. Its CZ-LSMINFO block has Java `InputLut`/`OutputLut` overlay
    offsets, but both point at size-0 overlay blocks so Java `parseOverlays`
    returns before OME ROI projection; ROI, application-tag, event-list, and
    channel-wavelength offsets are zero.
  - Worker H converted the retained-offset warning into a bounded parser
    parity test: `parse_lsm_info` now has direct coverage that Rust preserves
    Java's fixed CZ-LSMINFO offset slots for all nine overlay keys plus channel
    colors, scan information, application tags, timestamps, event list, and
    channel wavelengths. This closes the synthetic offset-retention concern but
    does not implement or prove the rich subblock projection paths themselves.
  - Fixed source-backed application-tag SIM color fallback parity in
    `src/formats/zeiss_lsm.rs`: Java `parseApplicationTags` marks datasets with
    `SimOut*`/`SimPar*` entries as SIM before reading channel colours, causing
    black colors after the first channel to copy the previous channel instead
    of falling back to white. Rust now parses that bounded application-tag flag
    and applies it to the existing channel-colour parser. The same pass added
    scan-info `User` retention coverage, but OME `ExperimenterUserName`
    projection remains blocked by the current common `OmeExperimenter` model,
    which has first/last/email/institution fields but no username field.
  - 2026-08-14 follow-up source audit: keep this item fixture-blocked for real
    MDB/multi-file parity. Rust has Java-style `.mdb` suffix/header detection,
    but Java `parseMDB` groups referenced sibling `.lsm` files and routes
    pixel reads by source LSM. Rust currently initializes one TIFF/LSM file and
    stores OME enrichment globally, so naive source-only concatenation would
    risk applying one file's channel/scan/timestamp metadata to all series.
    No minimal safe code change is recommended without a real `.mdb` plus
    sibling `.lsm` fixture.
  - Worker fixture refresh (2026-08-14): no additional real Zeiss LSM, ZVI,
    `.mdb` companion, or multi-`.lsm` dataset was found in the repo, external
    manifests, or mounted `/big/henriksson/ome_images`. The only `.mdb` file
    found is
    `java-bioformats/components/formats-gpl/test/loci/formats/utests/test.mdb`,
    a generic Microsoft Access fixture for Java MDB service tests, not Zeiss
    LSM companion evidence. The available LSM/ZVI samples still pass
    metadata-only Java parity through `.tmp/auditlist2-channel-ome-local.paths`.
  - 2026-08-14 online fixture search: current public OME image downloads
    (`https://downloads.openmicroscopy.org/images/`) do not list a Zeiss-LSM
    category, and `https://downloads.openmicroscopy.org/images/Zeiss-LSM/`
    returns 404. Bio-Formats docs still document `.lsm,.mdb`, say OME has many
    LSM datasets, and describe the multi-file structure where an `.mdb`
    accompanies multiple `.lsm` files. No public `.mdb` companion fixture URL
    was found.
  - Remaining required fix: add fixture-backed MDB companion/multi-LSM handling
    and ROI/full-subblock projection beyond the currently retained scan-info
    subset. No local fixture currently exercises MDB grouping, LSM overlay ROI
    records, event-list records, channel-wavelength tables, OME
    `ExperimenterUserName`, or DimensionM/DimensionP multi-position pixel
    ordering; the checked-in `.lsm` fixture is already Java-matched for core,
    OME, and pixels.
  - Required tests/fixtures: `.mdb` companion/multiple `.lsm` dataset plus a
    rich subblock fixture checking Java series order, LUT, objective nominal
    magnification, and OME graph fields.

- [ ] **Leica LEI raw companions and structured OME graph** (`.lei`, `.raw`, `.tif`, `.tiff`)
  - Java `LeicaReader` supports `.raw` companions and projects channel names,
    emission/excitation wavelengths, pinhole, channel colors, and related
    instrument metadata.
  - Rust now detects `.raw` companion entries, resolves `.tif`/`.tiff`
    companions via Java's ImageDescription `Series Name` hint and sibling `.lei`
    fallback, resolves sibling `.lei` files with Java-style stem/underscore
    trimming, reads flat raw planes for non-TIFF companions, and projects parsed
    physical sizes, time increment, and channel names, retained
    emission/excitation wavelengths, pinhole, LUT channel color,
    spectrophotometer cut-in/cut-out filters, and emission light-path refs into
    structured OME metadata. Rust now also projects Leica objective serial
    number to `Objective/@SerialNumber` and objective settings refractive index
    to `ObjectiveSettings/@RefractiveIndex`, plus Java-backed `CXYZStage` X/Y
    positions to per-plane `Plane/@PositionX` and `Plane/@PositionY` and
    `CXYZStage`/`CScanActuator` Z position to image
    `StageLabel Name="Position" Z="..."` using Java's meter-to-micrometer
    conversion for scan-actuator values. Rust now also parses Java `TIMEINFO`
    timestamps into image acquisition date and per-plane `DeltaT`, projects
    series name/description to the OME image, and projects Java
    `nDelayTime`/`nDelayTime_ms` exposure metadata to every plane. Java
    `EXPERIMENT` and `CHANDESC` tags are now retained as original metadata.
  - Fixed bounded raw companion and OME projection parity in
    `src/formats/leica.rs`; TIFF companion lookup now also follows Java
    `LeicaReader.findLEIFile` for `Series Name` hints and directory-level `.lei`
    fallback. A focused Leica unit test now also exercises OME-XML graph
    serialization for Java-backed `InstrumentRef`, `Objective/@SerialNumber`,
    `ObjectiveSettings/@RefractiveIndex`, `DetectorSettings` offset/voltage,
    emission filter light-path refs, per-plane stage X/Y positions, and
    image-level StageLabel Z. This pass added the remaining source-backed
    Java `CScanActuator|Z Scan Actuator|Position` StageLabel Z path and
    Java metadata-only `EXPERIMENT`/`CHANDESC` tag parsing. A follow-up
    Leica-only source audit also added Java-style instrument block original
    metadata retention (`Block N <contentID>`) for every non-empty parsed
    SAFEARRAY entry.
    Verified with `cargo test leica --lib`.
  - 2026-08-14 source audit found and fixed top-level `.raw` dispatch drift:
    Java `LeicaReader` registers `.raw` and accepts raw companion entry points
    when a sibling `.lei` resolves the dataset. Rust `LeicaReader::set_id(&raw)`
    already worked, but `ImageReader::open(&raw)` did not route to Leica because
    generic raw readers claimed the path first. `src/registry.rs` now
    pre-dispatches `.raw` files with a sibling `.lei` to `LeicaReader`.
    Verified with
    `cargo test leica_lei_raw_companion_dispatches_before_generic_raw_like_java --lib -- --nocapture`,
    `cargo test lei_raw_companion_reads_flat_planes_like_java --lib -- --nocapture`,
    `cargo test leica_lei --lib -- --nocapture`, and
    `cargo test leica --lib -- --nocapture`.
  - Remaining required fix: prove richer graph parity against real LEI metadata
    blocks. Fixture-backed evidence is still needed for the full
    instrument/time graph across real LEI/TIFF companion datasets.
  - Worker I fixture audit (2026-08-13): rechecked registered Cargo targets,
    `tests/java_parity_test.rs`, checked-in fixture/test/parity trees, `.tmp`,
    `userdata`, `external-fixtures`, parity/bench inputs, and mounted
    `/big/henriksson/ome_images` with `find` for `.lei` and `.raw`. No real
    `.lei` dataset and no Leica `.raw` companion dataset was present; the only
    `.raw` hits are unrelated NRRD/MHA raw payloads. Existing Leica coverage
    remains bounded unit data plus non-LEI Leica SCN/LIF samples, so this
    bucket cannot close from current fixtures.
  - 2026-08-14 fixture refresh reconfirmed no real `.lei` dataset and no Leica
    `.raw` companion dataset in the current repo, manifests, `userdata`,
    `external-fixtures`, `.tmp`, or mounted `/big/henriksson/ome_images`; the
    `.raw` hits remain unrelated NRRD/MHA payloads.
  - 2026-08-14 online fixture search: current public OME image downloads
    (`https://downloads.openmicroscopy.org/images/`) do not list a Leica-LEI
    category, and `https://downloads.openmicroscopy.org/images/Leica-LEI/`
    returns 404. Bio-Formats docs say OME has many LEI datasets, but no public
    LEI/raw companion download URL was found. The old LOCI sample-data URL
    referenced by historical Bio-Formats docs now redirects to a 404 page.
  - Required tests/fixtures: LEI/TIFF companion metadata containing real channel
    emission/excitation and pinhole/color/filter fields plus instrument/
    objective/detector metadata; compare Java OME-XML and pixel CRCs.

- [ ] **Leica XLEF broader LMS raw-storage/delegate layouts** (`.xlef`, `.xlif`, `.xlcf`, `.lof`, `.lms`)
  - Java `XLEFReader` special-cases LASX mono images stored as RGB JPEG and
    upscales 8-bit JPEG channel data to the declared higher bit depth; Java's
    LMS subsystem also handles broader project/delegate layouts.
  - Rust now converts RGB JPEG delegate output back to mono when the XLIF/LMS
    metadata declares a non-RGB series, and upscales 8-bit channel data to the
    declared LMS channel resolution (9-16 bit), not merely to the storage byte
    width.
  - Fixed JPEG mono/upscaling path in `src/formats/flim2.rs` and retained
    channel resolution in `src/formats/leica_lms.rs`. Rust also reads bounded
    raw-storage leaves with one storage file, no memory node or one
    uncompressed memory node, positive X/Y/Z/T strides, Java-style inferred
    channel stride from `ChannelDescription` `BytesInc` when DimID 5 is absent,
    and Java-style dimension order derived from the declared `BytesInc` values.
    XLEF grouped raster delegates now reorder XLIF frame references from Java's
    stored Z/S/T/C sequence into the LMS core dimension order for non-tile image
    sets. Leica XML file references now also decode raw `+` to a space like
    Java `URLDecoder.decode` before path resolution, and XLEF/XLCF collection
    children now follow Java's case-sensitive `.xlif`/`.xlcf` suffix checks
    after `fileExists` path correction. Rust now also treats `.xlcf` as a
    recursive collection reference in the project collector and opens supported
    leaves when sibling collection leaves are unsupported, matching Java's
    valid-child traversal behavior. Rust now also records Java-style
    `rgb_channel_count=3` for LMS RGB metadata where the X stride is divisible
    by 3, keeping logical `SizeC` separate from stored RGB samples for OME
    channel construction and the Java parity harness. XLEF OME metadata is now
    built from the whole project metadata vector, giving one OME image per Java
    series; LMS OME projection now mirrors Java's structural
    `MetadataStoreInitializer` behavior by creating one Instrument/Objective
    skeleton per LMS series and implicit RGB filter IDs without adding Rust-only
    OriginalMetadata map annotations. XLEF XLIF metadata now also overlays
    Java's LMS little-endian core flag onto PNG/JPEG raster delegates, records
    `rgb_channel_count=3` in the XLIF-backed metadata path, and creates
    Java-style implicit emission filter IDs for every effective RGB channel
    carrying a Leica channel colour. Verified with `cargo test xlef --lib`,
    `cargo test --test xlef_metadata_test`, metadata-only Java parity for the
    mounted `format test/format test LOF/format-test lof.xlef` fixture, and a
    nine-project mounted XLEF smoke covering BMP, PNG, JPEG, TIFF, compressed
    TIFF, LOF, mixed JPEG/TIFF, collection, and RGB-channel LOF projects
    (32 Java series, core and OME all matched; pixels intentionally disabled).
    Pixel-enabled Java parity now also passes for the small mounted format-test
    raster delegate projects: BMP, PNG, JPEG, TIFF, compressed TIFF, and mixed
    JPEG/TIFF. The non-JPEG paths are bitwise for all checked full/crop/offset
    reads; JPEG paths match within the harness' Java/Rust IDCT tolerance.
    A broader read-only pixel feasibility pass also found bitwise pixel parity
    for the mounted format-test LOF project, both TIF/LOF collection projects,
    XYCT 16-bit 2-channel time-series projects, `falcon_sample_data_small`, and
    Paramecium fixtures where pixel comparison reached the byte checks; no
    XLEF/LMS byte-layout mismatch was found in those pixel-enabled runs. Thunder
    Organoid Tilescan grouping is now fixed for both mounted projects: Rust
    follows Java `XlifDocument.getTileCount` / `MultipleImagesReader.swapDimensions`
    by grouping XLIF `Frame` references into tile series in stored `Z,S,T,C`
    order, avoids multiplying LOF delegates that already expose the XLIF tile
    series, and creates non-RGB implicit emission filters from channel colours.
    Java parity now passes for the mounted Thunder TIF and LOF XLEF projects
    with 9 Java/Rust series each, 18/18 core series matching, 2/2 OME files
    matching, and all 18 tile series passing bitwise pixel checks.
  - The mounted XYCT LOF, falcon LOF, and Paramecium LOF OME divergences are
    now fixed. Rust preserves Java-visible explicit empty LMS channel names
    instead of collapsing them to `None`, creates Java-style active-detector
    placeholder IDs even when the detector index/model field is empty, and
    suppresses per-channel laser-derived excitation wavelengths for later XLEF
    project series where Java emits no OME excitation wavelength. Verified with
    strict metadata-only Java parity for mounted XYCT, falcon, and Paramecium:
    3 files, 6/6 core series matching, and 3/3 OME documents matching, with
    pixels intentionally skipped for this focused OME check.
  - The mounted XLEF benchmark OME count drift is now fixed. Rust now emits all
    Java `initFilterModels` filter entries instead of only `filter.0`, counts
    channel light-path filter refs by represented channels when no explicit
    filter list exists, creates Java baseline `OmePlane` records for every LMS
    image plane before overlaying position/time/exposure fields, and handles
    projected/raster XLIF dimensions with oversized `Unit=m` extents like Java's
    observed OME output. Verified with metadata-only Java parity for
    `bench/manifests/leica-xlef-benchmark.paths`: 3 files, 17/17 core series
    matching, and 3/3 OME documents matching. A later bounded pixel run with
    `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4` also passes the same three mounted
    XLEF projects: all 17 series match bitwise for the checked crop/full/offset
    reads.
  - [x] Broader tau-selection OME channel excitation drift fixed. Rust now
    preserves the raw LMS laser/exWave/active-detector arrays and replays
    Java's XLEF project-store projection after project series metadata is
    assembled, including `MetadataStoreInitializer.initLasers`' persistent
    `nextChannel` behavior and `initDetectorModels` exWave override. Verified
    with metadata-only Java parity for `.tmp/leica-xlef-tau.paths`: 2 files,
    10/10 core series matching, and 2/2 OME documents matching.
  - 2026-08-14 broader fixture audit found 53 mounted `.xlef` projects under
    `/big/henriksson/ome_images/Leica-XLEF`. The closure manifest
    `.tmp/leica-xlef-closure.paths` samples Snail, tau-selection, MaF, and
    annotated-stage projects. Current metadata-only Java parity is green for
    every Java-comparable entry in that manifest. Refreshed 2026-08-14
    metadata-only Java parity: 4 files compared, 16/16 core series OK, and 4/4
    OME documents OK. The other 24 MaF and annotated-stage projects are skipped
    by the Java oracle with `Cannot open project: project has no valid image
    references`; they are not usable as parity proof until that Java-side
    comparability issue is understood or a different entry point is selected.
    2026-08-14 unit refresh: `cargo test xlef --lib -- --nocapture` passes
    21/21 focused tests.
    2026-08-14 extra bounded Java parity against four additional mounted
    2020/Lysosomes XLEF projects also passes: 4 files compared, 16/16 core
    series OK, 4/4 OME documents OK, and 16/16 checked pixel series bitwise
    with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=2`.
  - 2026-08-14 follow-up source audit found no additional safe source-only
    XLEF/LMS patch: Java-compatible tile-series mapping, MultipleImagesReader
    `Z,S,T,C` ordering, and OME channel/excitation/filter projection are already
    source-covered and fixture-backed for the comparable projects above. The
    unsupported raw-storage cases remain deliberately narrow because compressed
    `Memory`, multiple `Memory`/`Storage` nodes, and RGB/interleaved raw layouts
    cannot be inferred safely without Java-comparable bytes.
  - 2026-08-14 online fixture search: public OME downloads expose the Leica-XLEF
    category (`https://downloads.openmicroscopy.org/images/Leica-XLEF/`) with
    many real projects, including `format test` subprojects for BMP, JPEG,
    mixed JPEG/TIF, LOF, PNG, TIF, and compressed TIF delegates. The verified
    LOF directory includes `format-test lof.xlef` plus mono/rgb 8/12/16-bit
    `.lof` files, and the TIF/JPEG directories provide comparable raster
    delegates. No public fixture was identified by name for compressed LMS
    `Memory` or multiple `Memory`/`Storage` raw-storage layouts.
  - 2026-08-14 public OME fixture audit: downloaded the public
    `2020_06_23_sample 3 U2OS` XLEF-LOF project into
    `external-fixtures/data/leica-xlef` and added the missing top-level
    `.xlef`, IOManager metadata, and main `Series001.xlif` entries to
    `external-fixtures/manifests/fixture_sets.tsv` so it can be reproduced by
    `leica-xlef-smoke` plus `leica-xlef-feature`. Focused Java parity on the
    downloaded `.xlef` passes: 1 file compared, `seriesCount=1`, core metadata
    `1024x1024 z8 c6 t1 uint8 ic=48`, OME OK, and 9/9 bounded pixel checks
    bitwise with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`. This adds public
    fixture-backed proof for the LOF storage path but does not cover compressed
    LMS `Memory` or multiple `Memory`/`Storage` raw layouts.
  - Remaining required fix: expand LMS raw-storage/delegate support beyond the
    bounded one-storage-file/non-RGB scalar layouts, the single uncompressed
    memory-node case, and the already-covered grouped multi-frame delegate
    ordering path, only with fixture-backed parity evidence. Larger Lysosomes,
    2020, annotated-XYCZT, and currently Java-skipped annotated-stage/MaF
    projects still need bounded fixture-backed pixel and OME closure when a
    Java-comparable entry point is available; 2020 may still need the same
    project-series excitation check against Java on a bounded subset.
  - Worker F fixture audit (2026-08-13): checked repo fixtures and mounted
    `/big/henriksson/ome_images/Leica-XLEF`. Real XLEF/LOF fixtures exist and
    advance this bucket: metadata-only Java parity now passes core and OME for
    `format test/format test LOF/format-test lof.xlef` with six Java series and
    six OME images/instrument/objective skeletons. Worker Ampere source-checked
    the fixture and confirmed its LAS X camera hardware attributes are only
    original metadata in Java; Java's OME graph comes from structural
    `MetadataStoreInitializer` IDs, not populated camera model fields. The mounted
    `collection test/XLEF-LOF collection test/collection test.xlef` exposed the
    `.xlcf` recursive collection gap fixed above. The mounted
    `rgb channel test/XLEF-LOF rgb channel test/XLEF-LOF rgb channel test.xlef`
    exposed the XLIF-backed RGB sample-count and implicit-filter gaps fixed
    above: Java reports `SizeC` as stored RGB samples while
    `getRGBChannelCount()` remains 3 and `MetadataStoreInitializer` creates one
    implicit filter per effective RGB channel with a valid Leica channel colour.
    Mounted PNG/JPEG XLEF format-test fixtures exposed the little-endian
    overlay gap fixed above: Java reports LMS/XLEF core metadata as
    little-endian even when the delegated raster reader reports false.
  - Required tests/fixtures: LMS layouts covering compressed memory/storage
    declarations, multiple memory/storage nodes, colour/interleaved storage
    cases, additional mixed LOF/raster delegate variants, and Java/Rust pixel
    CRC comparisons.

- [x] **Zeiss ZVI ROI and full Zeiss metadata graph** (`.zvi`)
  - Java `ZeissZVIReader` parses Shapes ROI streams and additional Zeiss tags
    into the metadata store.
  - Rust now projects parsed Java tag-stream metadata into OME for image
    description, physical sizes, channel name/emission/excitation/color,
    acquisition date, experimenter first/last name,
    experimenter institution, objective ID/correction/name-derived
    magnification/NA plus explicit objective magnification/NA/immersion/working
    distance, Java `Orca Analog Gain/Offset` detector settings refs, plane
    exposure/stage X/Y and camera timestamp-derived DeltaT, and representable
    Shapes ROI geometry gated by metadata level like Java overlays.
  - Fixed bounded parsed-tag OME projection and Shapes ROI parsing in
    `src/formats/zeiss_zvi.rs`; corrected detector gain/offset tag IDs from
    Meteor color tags `2576/2577` to Java's `65633/65634`, and now creates the
    Java-style per-channel Detector objects even when gain/offset tags are
    absent.
  - Verified with `cargo test zeiss_zvi --lib` and
    `BIOFORMATS_RS_JAVA_PARITY=1 BIOFORMATS_RS_JAVA_PARITY_FILES=zvi/ cargo test --test java_parity_test -- --nocapture`;
    the checked-in ZVI fixture now passes core, OME, and pixel parity.
  - Remaining non-blocking schema gap: Java ROI per-shape text/styling/tag-
    display fields and some broader Zeiss graph fields still have no direct
    Rust OME model slot today.

- [x] **Imaris TIFF OME metadata projection** (`.ims`, `.tif`, `.tiff`)
  - Java projects INI `LSMEmissionWavelength`, `LSMExcitationWavelength`,
    `RecordingDate`, channel names, and description into OME.
  - Rust parses names/description/date and stores wavelengths as
    `imaris.channel.N.emission` / `imaris.channel.N.excitation`, but the shared
    OME mapper expects `emission_wavelength` / `excitation_wavelength`; the
    acquisition date is not projected.
  - Fixed in `src/formats/flim2.rs`; Imaris TIFF INI metadata now also writes
    generic OME projection aliases for image description, acquisition date,
    channel names, and emission/excitation wavelengths.
  - Verified with
    `cargo test imaris_tiff_skips_channel_name_that_is_filename_suffix_like_java --lib`.

- [x] **SlideBook TIFF sibling grouping and OME metadata** (`.tif`, `.tiff`)
  - Java groups sibling TIFFs with matching timestamp into a channel/time stack
    and maps planes with `file = no / sizeT`, `plane = no % sizeT`.
  - Rust exposes only a single TIFF via `TiffReader`.
  - Java fills OME image name, physical size X/Y, objective/instrument metadata,
    and plane positions X/Y/Z; Rust stores `slidebook.*` original metadata but
    has no OME override.
  - Fixed in `src/formats/flim2.rs`; SlideBook TIFF now discovers same-timestamp
    sibling TIFFs, exposes Java's `file = no / sizeT`, `plane = no % sizeT`
    pixel mapping, reports grouped used files, and projects channel names,
    image name, physical XY, objective magnification/instrument reference, and
    plane XYZ positions into structured OME metadata.
  - Verified with `cargo test slidebook_tiff_ --lib`.

- [x] **Bio-Rad SCN structured OME metadata** (`.scn`)
  - Java fills OME physical size X/Y, instrument/microscope serial/model,
    detector/settings, and plane exposure.
  - Rust now projects parsed `biorad.*` XML metadata into the structured OME
    graph, including image name/acquisition date, microscope model/serial,
    detector/settings gain/binning, plane exposure, and physical size X/Y.
  - Verified with `cargo test biorad_scn_projects_java_ome_metadata --lib`.

- [x] **cellSens/VSI extended OME graph** (`.vsi`, `.ets`)
  - Rust now covers non-flattened ETS pyramids, tile-origin region assembly,
    channel names, and emission wavelengths.
  - Java also projects instrument/objective/detector settings, plane exposure,
    X/Y/Z positions, deltaT, physicalSizeZ, refractive index, and acquisition
    date.
  - Rust OME previously emitted image name, physical size X/Y, channels, and
    original metadata annotations only.
  - Fixed in `src/formats/flim2.rs`; `CellSensReader::ome_metadata` now
    projects parsed VSI/ETS instrument, objective, detector settings, per-plane
    exposure, X/Y/Z positions, deltaT, physicalSizeZ, and acquisition date into
    the structured OME graph while keeping the direct ETS region-read
    optimization.
  - Remaining non-blocking schema gap: Java can set detector serial number and
    objective-settings refractive index, but the current Rust OME structs do
    not expose those fields.
  - Verified with `cargo test cellsens_ome_metadata_projects --lib`.

- [x] **SPE full header metadata and ROIs** (`.spe`)
  - Java emits the full `SpeHeaderEntry` metadata set and OME ROI rectangles
    from `NUM_ROIS`.
  - Rust now projects the Java-style SPE header table into original metadata,
    adds ROI start/end/group metadata, and converts SPE ROIs to OME rectangle
    shapes.
  - Verified with `cargo test spe_ --lib`.

- [x] **Zeiss XRM/TXM/TXRM metadata coverage and suffix parity** (`.txm`, `.txrm`)
  - Java suffixes are `txm` / `txrm` and parses TXM/TXRM OLE streams for
    exposure, voltage/current, acquisition dates, angles/moduloZ, per-plane XYZ,
    and deltaT.
  - Rust previously matched only broader name behavior and covered core
    dimensions plus a few scalar fields.
  - Fixed in `src/formats/zeiss_xrm.rs`; suffix detection now matches Java's
    `txm` / `txrm` registration, TXM/TXRM OLE streams are parsed for exposure,
    voltage/current, acquisition dates, angles/moduloZ, per-plane XYZ, and
    deltaT, and the OME metadata projects physical size plus per-plane fields.
  - Verified with `cargo test xrm --lib`.

- [x] **LI-FLIM OME image/plane/ROI projection** (`.fli`)
  - Java `LiFlimReader` calls `MetadataTools.populatePixels`, names the primary
    and background images, derives per-plane `DeltaT` from
    `FLIMIMAGE: TIMESTAMPS`, applies parsed exposure time, and emits sorted ROI
    polygon points.
  - Rust previously relied on the default single-series OME projection for
    LI-FLIM, so multi-series names, timestamp/exposure planes, and ROI polygons
    were not represented.
  - Fixed in `src/formats/flim.rs`; `LiFlimReader::ome_metadata` now builds the
    Java-style all-series OME baseline plus representable plane and ROI fields.
  - Verified with
    `cargo test liflim_ome_metadata_projects_java_image_planes_and_roi_polygon --lib`
    and `cargo test liflim_ --lib`.

- [x] **ICS lifetime dimension/channel handling** (`.ics`, `.ids`)
  - Java has special lifetime handling from `history type` / `history labels`
    that remaps dimensions and channel interpretation.
  - Rust now parses `history type=lifetime` and Java's recognized
    `history labels` values (`x y t`, `t x y`), remaps core dimensions/order,
    sets ModuloC lifetime metadata, and uses Java-style contiguous plane reads
    for these files.
  - Verified with `cargo test lifetime_history_labels --lib`.

- [x] **Micro-Manager private metadata and plane mapping** (`.tif`, `.tiff`, Micro-Manager layouts)
  - Java reads TIFF private JSON tags `50839` and `51123`, supports `Coords-*`
    plane mapping blocks, and parses `Acqusition.xml`.
  - Rust now parses `Coords-*` blocks, `Acqusition.xml` entries, and
    Micro-Manager TIFF private JSON tags `50839` / `51123`, feeding the
    existing per-plane metadata and OME projection path.
  - Follow-up audit fixed byte-level detection: Rust
    `MicromanagerReader.is_this_type_by_bytes` now mirrors Java by delegating to
    TIFF header detection.
  - Verified with `cargo test micromanager --lib`.
  - Remaining risk: malformed/non-JSON `51123` payloads may still differ from
    Java's token-splitting tolerance; valid JSON private metadata is covered.

- [x] **Micro-Manager `SlicesFirst` boolean dimension order** (`.tif`, `.tiff`, Micro-Manager layouts)
  - Java accepts unquoted JSON booleans in Micro-Manager metadata, so
    `"SlicesFirst": false` produces `XYCZT`.
  - Rust only handled string and integer forms, causing the sampled
    `/big/henriksson/ome_images/Micro-Manager/1.4.16/serge/Pos0/metadata.txt`
    metadata to report `XYZCT` instead of Java's `XYCZT`.
  - Fixed in `src/formats/micromanager.rs` by parsing unquoted `true`/`false`
    values in the same lightweight metadata path.
  - Verified with focused metadata-only Java parity for the sampled `Pos0`
    metadata: 1/1 core series OK and OME OK.

- [x] **Olympus OIR native instrument graph parity** (`.oir`)
  - Focused metadata-only Java parity on
    `/big/henriksson/ome_images/Olympus-OIR/gh-4205/zenodo-12773657/DAPI_mCherry_22Lambda-420-630-w10nm-s10nm.oir`
    originally showed core metadata matched, but OME instrument graph counts
    diverged: Java reported 2 detectors and 4 light sources, while Rust
    reported 4 detectors and 1 light source.
  - Fixed in `src/formats/flim2.rs`; native OIR XML now promotes Java-style
    laser, detector, and objective records into the OME instrument graph instead
    of hardcoding Rust defaults.
  - Verified with focused Java parity: metadata-only core/OME parity passes for
    the sampled native OIR, and bounded pixel parity reports
    `512x512 z1 c22 t1 uint16 ic=22`, OME OK, and 9/9 pixel checks bitwise.

- [x] **Imspector MSR native OME/performance parity evidence** (`.msr`)
  - Real fixture found at
    `/big/henriksson/ome_images/Imspector/zenodo-10476252/test.msr`.
  - Fixed in `src/formats/extended.rs`; the Java-style MSR parser now mirrors
    Bio-Formats' `xyz-Table Z Resolution=1` branch, collapsing raw
    `Z=200,T=1` into Java-visible `Z=1,T=200`, and sets the OME image name from
    the file basename.
  - Verified with focused Java parity on the mounted MSR: 1/1 core series OK,
    OME OK, and 9/9 bounded pixel checks bitwise.

- [x] **Volocity structured OME metadata** (`.mvd2`, `.aisf`, related)
  - Java populates instrument/objective/detector settings, channel names, image
    descriptions, per-plane XYZ, and deltaT.
  - Fixed in `src/formats/volocity.rs`; native Metakit-backed stacks now build
    one OME image per stack, share Java-style `Instrument:0`, attach per-series
    objectives/detectors, channel names, descriptions, physical sizes,
    per-plane XYZ positions, and `.atsf` deltaT timestamps.
  - Follow-up scoped audit on 2026-08-13 found no additional source-backed gap
    in dispatch, companion routing, plane/region reads, or representable OME
    projection. Verified with `cargo test volocity --lib`.

- [x] **Scoped misc / OME-XML / Zeiss clean pass**
  - Java sources compared: `QTReader.java`, `MNGReader.java`,
    `SlidebookReader.java`, `MINCReader.java`, `OpenlabReader.java`,
    `OpenlabRawReader.java`, `JPEG2000Reader.java`, `JPEG2000Writer.java`,
    `SMCameraReader.java`, `TextReader.java`, `APLReader.java`,
    `AliconaReader.java`, `CellWorxReader.java`, `HISReader.java`,
    `HRDGDFReader.java`, `FilePatternReader.java`, `KLBReader.java`,
    `OBFReader.java`, `JPKReader.java`, `OMEXMLReader.java`,
    `OMEXMLWriter.java`, `ZeissCZIReader.java`, and `ZeissXRMReader.java`.
  - Rust files covered: `src/formats/misc.rs`, `src/formats/misc4.rs`,
    `src/formats/ome_xml.rs`, `src/formats/zeiss_czi.rs`, and
    `src/formats/zeiss_xrm.rs`.
  - No new small source-backed fix was required in this pass. Existing code
    covers the audited dispatch/suffix/byte checks, OME channel-count padding
    and channel name/emission/excitation projection where Java exposes it,
    non-flattened JPEG-2000/FilePattern/CZI resolution semantics, companion
    routing for OME-XML/FilePattern, bounded plane/region reads, and covered
    compressed/optimized paths.
  - Verified with `cargo test jpeg2000 --lib`, `cargo test ome_xml --lib`,
    `cargo test misc4 --lib`, `cargo test filepattern --lib`,
    `cargo test zeiss --lib`, and `cargo test zeiss_zvi --lib`.
  - Remaining fixture-only risk: real Java/Rust OME-XML and pixel CRC matrices
    are still absent for several vendor formats in `misc.rs`/`misc4.rs`, CZI
    attachment/ROI-heavy datasets, and TXM/TXRM acquisition variants; no
    actionable source-only gap was identified for the current Rust model.

- [x] **DeltaVision truncated-file read semantics** (`.dv`, `.r3d`, related)
  - Java has truncated-file behavior that can return partial/zero-filled planes
    under truncated mode.
  - Rust now zero-fills unavailable plane tails at read time instead of
    rejecting a plane whose computed end exceeds file length, preserving the
    normal lower-left-origin row flip.
  - Verified with
    `cargo test truncated_plane_zero_fills_unavailable_tail_like_java --lib`.

- [x] **Scoped FLIM2 / Leica / LMS remaining Java parity gaps** (`src/formats/flim2.rs`, `src/formats/leica.rs`, `src/formats/leica_lms.rs`)
  - Audit completed against the local Java Bio-Formats sources for
    dispatch/suffix/byte detection, metadata projection, channel
    name/emission/excitation handling, shorter channel metadata arrays,
    non-flattened resolutions, plane/region mapping, companions/raw layouts,
    and compression/optimized paths.
  - Fixed/verified during the scoped pass: bounded LEI `.raw` companion reads
    and structured OME projection in `src/formats/leica.rs`; LMS channel
    resolution retention in `src/formats/leica_lms.rs`; and CellSens/NDPIS,
    XLEF JPEG mono/upscaling, Imaris TIFF, SlideBook TIFF, SlideBook7 bounded
    metadata/plane projection, and OIR audited paths in `src/formats/flim2.rs`.
  - Closed as aggregate bookkeeping on 2026-08-13: the scoped audit itself is
    complete and no additional source-only defect was found in
    `src/formats/flim2.rs`, `src/formats/leica.rs`, or
    `src/formats/leica_lms.rs`. Remaining actionable work stays tracked in the
    individual open SlideBook 7, Leica LEI, and Leica XLEF/LMS entries above:
    real fixture-backed Java/Rust comparisons for full SlideBook7 native
    metadata/pixel coverage, broader Leica LMS raw storage/delegate layouts, and
    rich LEI instrument/objective/detector/filter graph fields that are either
    not yet fixture-proven or not yet representable in the shared Rust OME model.
  - Verified fixed paths with `cargo test leica --lib`, `cargo test xlef --lib`,
    `cargo test cellsens --lib`, `cargo test slidebook --lib`,
    `cargo test imaris --lib`, `cargo test ndpis --lib`, and
    `cargo test oir --lib`.

- [x] **Scoped ND2 Java parity pass** (`src/formats/nd2.rs`)
  - Java `ND2Reader` parses flat binary `ImageAttributes*` records before
    dimension fallbacks, scans binary `ImageMetadata*` from the stream position
    left after the first 12 chunk-name bytes, exposes split logical channel
    planes from interleaved stored frames, and gates optimized compressed reads
    when channel extraction is required.
  - Rust now mirrors those paths for audited behavior and retains optimized
    extraction only where observable plane/region behavior remains Java-parity.
  - Verified with `cargo test nd2 --lib`.

- [x] **Tecan Spark Cyto reader was a non-Java stand-in** (`.db`)
  - Java `TecanReader` (formats-gpl) reads the Spark Cyto SQLite database plus
    sibling `Images/` TIFFs and `Export/` analysis files into an HCS plate
    (series = well x field, C = raw image types, T = `CycleIndex`).
  - Rust registered `loci.formats.in.TecanReader` to an unrelated `.asc`
    tab-separated plate-reader table reader in `hcs2.rs` that has no Java
    counterpart, so Spark Cyto data could not be read.
  - Replaced with a line-by-line port in `src/formats/gpl/tecan.rs` behind the
    default `tecan` cargo feature (`rusqlite`, like `tissuefaxs`). The `.asc`
    reader and its test were removed.
  - Registry: SPE's accept-anything byte probe claimed the database before
    Tecan was reached; fixed by the `suffix_necessary` gate below (no
    Tecan-specific dispatch needed).
  - Verified against Java on a real 12-well x 10-cycle kinetic run
    (`BIOFORMATS_RS_JAVA_PARITY_EXTRA_FILES=<...>/Database/<guid>.db`): core 12/12
    series, OME, and pixels 12/12 bitwise; global/series original metadata,
    acquisition dates, plane exposure times, plate/well graph, and
    `getUsedFiles(true)` identical to a Java dump (518/518 entries).
    Unit tests: `cargo test --release --lib tecan`.
  - Remaining model gaps: `PlateAcquisition` (ID, MaximumFieldCount,
    WellSampleRefs) is not representable in the Rust OME model;
    `getUsedFiles`/`getSeriesUsedFiles` are inherent methods because the
    `FormatReader` trait has no used-files API. Java throws when
    `DateTools.formatDate` returns null for an acquisition timestamp; Rust
    leaves the date unset instead.
  - Parity harness: the oracle reported `Length.value()` in the Java-native
    unit (Tecan stores nm) while the Rust OME model is micrometres. The oracle
    now also emits `physicalSize{X,Y,Z}Um` and the harness accepts a match on
    either value.

- [x] **Accept-anything byte probes claimed unrelated files** (`.spe`, `.afi`)
  - Java `SPEReader` and `AFIReader` `isThisType(stream)` accept any stream of
    at least 4 bytes, but both keep the default `suffixNecessary = true`, so
    Java `FormatReader.isThisType(name, open)` only consults the stream check
    when the suffix matches.
  - The Rust registry ran `is_this_type_by_bytes` for every reader before any
    suffix check, so `SpeReader` could claim and "open" non-SPE files (a Tecan
    SQLite database opened as a float SPE image).
  - Added `FormatReader::suffix_necessary()` (Java's field; default false so
    other readers keep their routing); SPE and AFI return true, and both
    registry byte-probe loops skip suffix-necessary readers whose name does not
    match. A scan of every registered reader against random/zero/SQLite/text
    headers found no other reader whose byte probe accepts all of them.
  - Verified with `registry_does_not_route_non_spe_suffix_to_spe_byte_probe`
    and the Tecan registry test.

- [x] **Panics on unopened/garbage input found by an all-reader scan**
  - `AfiReader::metadata()` and `FlowSightReader::metadata()` indexed an empty
    series list; `AfiReader::open_bytes` indexed `readers[0]`;
    `SlidebookTiffReader::open_bytes` indexed `readers[file]` before `set_id`
    (uninitialized metadata reports one plane). Now return
    `uninitialized_metadata()` / `NotInitialized` like Java `assertId`.
  - `MicroCtVffReader` header parsing sliced bytes for Java
    `substring(eq + 1, length - 1)` and `<...>` stripping, panicking when the
    last character is multi-byte UTF-8; now drops characters.
  - Verified with `afi_flowsight_slidebook_do_not_panic_without_open_file` and
    `microct_header_value_ending_in_multibyte_char_does_not_panic` (the latter
    fails on the old code).
  - Note: `tests/format_tests.rs` is not a registered `[[test]]` target
    (`autotests = false`), so tests there do not run.


## Upstream update, 2026-10-26

See [the upstream update record](audit/upstream-2026-10-26.md) for the
`d87eb853f0` → `d54bf462e3` translation batch. The new cases cover CellSens
embedded stacks/placeholders and per-resolution metadata, unique CV8000 target
names, BDV/SVS/Ventana calibration, anatomical orientation access, MINC negative
steps, split FDF slices/echoes, and Operetta Windows paths.

Remaining resolution-interface work: Fake, TissueFAXS, CZI and PyramidTiff
retain their pre-existing nested public views. Fake/TissueFAXS OME metadata
still represents base images; Java's default flattened per-level physical-size
metadata needs public-series-map support before that gap can be closed.
These existing interface differences were not marked fixed by this update.
