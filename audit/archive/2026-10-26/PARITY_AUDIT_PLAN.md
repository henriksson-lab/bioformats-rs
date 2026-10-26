> Historical snapshot retained for provenance. This is not the active backlog.
> See [current implementation status](../../../TOFIX.md).

# Bio-Formats Parity Audit Plan

Created: 2026-08-13

Purpose: restore and maintain close one-to-one behavior with upstream Java
Bio-Formats for every reader, while keeping Rust-specific optimizations when
they are behavior-preserving and explicitly verified.

This plan is a process checklist, not the issue list itself. Concrete defects
found during audits should continue to be recorded in `TOFIX.md` until fixed.

## Audit Principles

- Standing helper rule: Rust-only helpers should be removed or inlined by
  default because they prevent direct audit against the original Java code and
  can drift from it. The only allowed exception is an adapter for a Java library
  call that has been replaced by a Rust library/API with a different shape, and
  then it must preserve Java behavior instead of introducing a new abstraction.
  Do not introduce new helper layers during parity work unless they meet that
  adapter exception.
- Treat Java Bio-Formats as the behavioral reference unless Rust intentionally
  documents a difference.
- Compare against the matching upstream Java reader, helper readers, metadata
  store population, and shared utilities. Many bugs live in helper code, not in
  the top-level reader.
- Prefer 1-to-1 translation structure when practical: same parsing phases, same
  counters, same plane mapping, same validation/fallback order, same metadata
  population order.
- Apply the standing helper rule during parity work before adding new
  abstractions; do not keep helper layers merely for local style preferences.
- Keep Rust optimizations only when parity is proven. An optimized path must
  have either a direct equivalence argument or tests showing it matches the
  Java-shaped path for non-trivial regions/planes.
- Never audit metadata alone when the issue is pixel layout, and never audit
  full-plane pixels alone when region reads exist. Region, channel, resolution,
  and tile-origin behavior can diverge while full planes appear plausible.
- When a Java behavior looks odd, copy it first, then isolate any deliberate
  Rust improvement behind a clearly named helper with parity tests.

## Per-Format Checklist

Use this checklist for every reader implementation and every wrapper/delegating
reader. Mark each item with one of:

- `[x]` matches Java
- `[ ]` not checked
- `[~]` differs but appears intentional/optimized and has tests
- `[!]` differs and needs a `TOFIX.md` entry
- `[n/a]` not applicable to this format

### 1. Source Mapping

- [ ] Identify the exact Java reader class and any helper classes it delegates
      to.
- [ ] Identify all Rust readers/wrappers involved for the same extension or
      container path.
- [ ] Confirm registry routing and extension priority match Java reader
      selection expectations.
- [ ] Confirm companion-file discovery, directory handling, and missing-file
      behavior match Java.
- [ ] Record source files and line anchors used for the audit.

### 2. File Identification And Open Semantics

- [ ] `is_this_type_by_name` matches Java extension/name rules.
- [ ] `is_this_type_by_bytes` does not overclaim unrelated files.
- [ ] `set_id` accepts/rejects the same representative valid and invalid files
      as Java.
- [ ] Errors are clean `Result::Err` values, not panics or misleading fallback
      reader errors.
- [ ] Metadata-only and full metadata options behave like Java when the Java
      reader has a partial metadata path.

### 3. Parsing Structure

- [ ] Parsing phases happen in Java order, especially where later fallbacks
      depend on earlier state.
- [ ] Binary/text/XML branches are all represented when Java supports multiple
      encodings for the same logical metadata.
- [ ] Offsets are relative to the same stream position as Java, including
      header/name bytes and consumed sentinel bytes.
- [ ] Endianness, signedness, string encoding, null stripping, alignment, and
      padding match Java.
- [ ] Java fallback heuristics are preserved in the same order.
- [ ] Rust-only helpers have been removed or inlined when they hide Java's
      original control flow. Remaining helpers are limited to adapters for Java
      library calls that must map to different Rust library APIs; no other
      helper layer is accepted as parity-preserving.
- [ ] Optimized parsers are covered by tests against Java-shaped edge cases.

### 4. Core Metadata

- [ ] `series_count` and series ordering match Java.
- [ ] `size_x`, `size_y`, `size_z`, `size_c`, `size_t`, and `image_count`
      match Java after all fallbacks.
- [ ] `dimension_order`, `is_rgb`, `is_interleaved`, `is_indexed`, and
      `is_little_endian` match Java.
- [ ] Bits-per-pixel and pixel type follow Java's significant-bit and storage-bit
      rules.
- [ ] RGB/effective channel semantics match Java: stored samples, logical
      channels, and OME channels are not conflated.
- [ ] Series-specific metadata overrides match Java when a reader rebuilds or
      splits series after parsing.

### 5. Plane Mapping

- [ ] Logical plane index to stored offset/chunk/IFD mapping matches Java.
- [ ] Z/C/T rasterization order matches Java, including special reader-specific
      field indexes.
- [ ] Multi-position or multi-field files split/merge series the same way Java
      does.
- [ ] Multi-channel stored planes are split, interleaved, or returned RGB exactly
      as Java does.
- [ ] Missing, extra, duplicate, or sparse planes follow Java behavior.
- [ ] Per-plane timestamps, exposure times, positions, and annotations use the
      same logical plane index as Java.

### 6. Pixel Reads

- [ ] `open_bytes` returns the same full-plane byte layout as Java for each
      representative pixel type.
- [ ] `open_bytes_region` matches Java for top-left, non-zero-origin, boundary,
      and cross-tile/cross-strip regions.
- [ ] Row stride, scanline padding, tile padding, planar/chunky conversion,
      channel order, BGR/RGB conversion, and endian swaps match Java.
- [ ] Cropping and tile-origin math use the same coordinate system as Java.
- [ ] Decompression dispatch and codec options match Java.
- [ ] Optimized direct-region reads are proven equivalent to the Java-shaped
      full/intersection algorithm.
- [ ] Thumbnail reads match Java's selected thumbnail source and scaling rules.

### 7. Resolutions And Pyramids

- [ ] `resolution_count`, `set_resolution`, and `resolution` match the chosen
      Rust policy: expose Java non-flattened behavior by default.
- [ ] Base series and sub-resolution grouping match Java's non-flattened model.
- [ ] Flattened Java behavior is considered when comparing historical Java
      defaults, but Rust must document any deliberate API difference.
- [ ] Per-resolution metadata and pixel reads use the active resolution, not
      stale base-level dimensions.
- [ ] Whole-slide label, macro, overview, thumbnail, and pyramid levels appear
      in the same logical places as Java.

### 8. OME Metadata

- [ ] OME image count equals Rust/Java logical series count.
- [ ] OME channel list length is at least the declared OME channel count.
- [ ] Channel names are populated when Java calls `setChannelName`.
- [ ] Emission and excitation wavelengths are populated when Java calls
      `setChannelEmissionWavelength` or `setChannelExcitationWavelength`.
- [ ] Instrument, objective, detector, filter, light-source, and image
      references match Java where the reader populates them.
- [ ] Plane records exist when Java creates them, even if most fields are empty.
- [ ] Physical sizes, positions, times, colors, acquisition modes, and
      annotations use Java units and indexes.
- [ ] Rust-only annotations are intentional and do not hide missing Java fields.

### 9. Compressed Extraction And Optimized Paths

- [ ] `compressed_level_info` reports support only when a logical request maps
      cleanly to stored compressed bytes or a documented lossless repack.
- [ ] `read_compressed_tile` uses the same plane/resolution/tile mapping as
      pixel decode.
- [ ] Codec container, tile dimensions, offsets, byte counts, and edge-tile
      semantics are verified against Java decode behavior.
- [ ] Optimized compressed extraction does not bypass required crop, origin,
      channel split, or color-order transformations unless documented as an
      extraction-mode constraint.

### 10. Test Evidence

- [ ] At least one valid fixture is checked against Java for core metadata.
- [ ] OME metadata is checked for channel count, names, wavelengths, planes, and
      instrument graph fields relevant to the reader.
- [ ] Pixel CRC/hash checks cover full plane or bounded region where practical.
- [ ] Region checks include non-zero-origin and edge/cross-tile cases when the
      reader supports regions.
- [ ] Negative fixtures cover Java rejection or clean unsupported behavior.
- [ ] Tests are named after the Java behavior being preserved.

## Systematic Audit Workflow

### Phase 0: Freeze The Baseline

- [ ] Choose the upstream Java Bio-Formats version for the audit and record it
      in `TOFIX.md` and this file if it changes.
- [ ] Ensure Java source is available under `java-bioformats/` or another
      stable local path.
- [ ] Confirm the Rust tree builds before broad changes.
- [ ] Record the current dirty worktree state before editing so unrelated user
      changes are not reverted.

### Phase 1: Build The Format Inventory

- [ ] Generate the reader inventory from `src/formats/mod.rs`, all
      `impl FormatReader` blocks, wrapper readers, `src/tiff`, and
      `src/stitcher.rs`.
- [ ] Map each Rust reader to the Java reader(s), including delegated helpers.
- [ ] Mark readers without Java equivalents separately.
- [ ] Group readers into audit batches with low overlap:
      TIFF/wrappers, whole-slide/pyramids, microscopy containers, simple raster,
      medical/scientific volume, HCS/plate, movie/sequence, and synthetic/test
      readers.

### Phase 2: Checklist-Only Pass

- [ ] For each reader, complete the per-format checklist without editing code.
- [ ] Use parallel workers for independent batches; each worker must report
      checked readers, Java files consulted, Rust files consulted, `[!]` gaps,
      `[~]` optimized paths, and unverified areas.
- [ ] Add every confirmed gap to `TOFIX.md` with:
      format, Rust file, Java reader, failure class, parity expectation,
      suspected fix area, and test evidence needed.
- [ ] Do not mark a reader fully audited unless all checklist sections have a
      status.

### Phase 3: Prioritize Fixes

- [ ] Fix shared API or helper-layer divergences before format-specific copies.
- [ ] Prioritize bugs that corrupt dimensions, logical plane count, channel
      mapping, OME channel graph, tile/region reads, and resolution topology.
- [ ] Batch fixes by disjoint files where possible so parallel workers can edit
      safely.
- [ ] For each fix, decide whether to restore Java-shaped structure or keep an
      optimized Rust path with explicit equivalence tests.
- [ ] Remove or inline helper functions introduced during previous translations
      by default so the Rust reader remains line-by-line auditable against Java
      and does not drift. Keep helpers only for adapting Java library calls to
      Rust library APIs with a different shape; remove or inline all other
      helpers before considering the reader audited.

### Phase 4: Implement Format Fixes

- [ ] Start each fix by copying Java's control flow into comments or local
      helper boundaries only where it clarifies non-obvious behavior.
- [ ] Keep edits scoped to the reader/helper being fixed.
- [ ] Preserve existing optimizations only after proving these invariants:
      same logical metadata, same OME graph, same plane mapping, same region
      bytes, same error behavior for malformed inputs.
- [ ] If an optimization cannot be proven quickly, replace it with the
      Java-shaped path first and reintroduce the optimization later behind
      tests.
- [ ] Update `TOFIX.md` immediately when a finding is fixed, partially fixed, or
      disproven.

### Phase 5: Verify

- [ ] Run targeted Rust tests for the changed reader.
- [ ] Run parity harness checks against Java for available fixtures.
- [ ] Run broader `cargo test --lib` after shared behavior changes.
- [ ] For pixel changes, compare at least:
      full small plane, top-left region, non-zero-origin region, edge region,
      and cross-tile/cross-strip region where applicable.
- [ ] For OME changes, compare image/channel/plane counts and key OME fields
      before inspecting minor annotation differences.

### Phase 6: Close The Audit Loop

- [ ] Every reader has a checklist result in the audit records.
- [ ] Every `[!]` item has a `TOFIX.md` entry or a linked fix commit.
- [ ] Every `[~]` optimized path has a documented equivalence test.
- [ ] `TOFIX.md` contains no stale issues whose current code has already been
      fixed or disproven.
- [ ] Add a short final audit summary: readers checked, readers fixed, remaining
      gaps, tests run, and fixtures still missing.

## Reader Inventory To Audit

Audit every `FormatReader` implementation and every wrapper/helper that changes
metadata, plane mapping, resolution selection, or pixel layout. This is the
trackable top-level inventory; each checked item must have either a completed
audit record, a `TOFIX.md` finding, or both.

- [x] `afm.rs`
- [x] `aim.rs`
- [x] `amira.rs`
- [x] `avi.rs`
- [x] `bdv.rs`
- [x] `biorad.rs`
- [x] `bmp.rs`
- [x] `bruker.rs`
- [x] `camera2.rs`
- [x] `cellh5.rs`
- [x] `clinical.rs`
- [x] `dcimg.rs`
- [x] `deltavision.rs`
- [x] `dicom.rs`
- [x] `eps.rs`
- [x] `extended.rs`
- [x] `fake.rs`
- [x] `fits.rs`
- [x] `flex.rs`
- [x] `flim.rs`
- [!] `flim2.rs`
- [x] `gatan.rs`
- [x] `hcs2.rs`
- [x] `ics.rs`
- [x] `imagic.rs`
- [x] `imaris_hdf.rs`
- [x] `incell.rs`
- [x] `jpeg.rs`
- [x] `khoros.rs`
- [x] `legacy.rs`
- [!] `leica.rs`
- [!] `leica_lms.rs`
- [x] `lif.rs`
- [x] `lim.rs`
- [x] `metaimage.rs`
- [x] `metamorph.rs`
- [!] `mias.rs`
- [x] `micromanager.rs`
- [x] `misc.rs`
- [x] `misc4.rs`
- [x] `mrc.rs`
- [x] `nd2.rs`
- [x] `nifti.rs`
- [x] `norpix.rs`
- [x] `nrrd.rs`
- [x] `olympus.rs`
- [x] `ome_xml.rs`
- [x] `openslide_reader.rs`
- [x] `pcx.rs`
- [x] `perkinelmer.rs`
- [x] `png.rs`
- [x] `prairie.rs`
- [x] `psd.rs`
- [x] `raster.rs`
- [x] `sem.rs`
- [x] `sif.rs`
- [x] `simfcs.rs`
- [x] `spe.rs`
- [x] `spm.rs`
- [x] `svs.rs`
- [x] `tiff_wrappers.rs`
- [x] `v3draw.rs`
- [x] `visitech.rs`
- [x] `volocity.rs`
- [x] `zarr.rs`
- [x] `zeiss_czi.rs`
- [!] `zeiss_lsm.rs`
- [x] `zeiss_xrm.rs`
- [x] `zeiss_zvi.rs`
- [x] `zip.rs`

Also audit shared code that can create cross-format parity drift:

- `src/common/reader.rs`
- `src/common/metadata.rs`
- `src/common/ome_metadata.rs`
- `src/common/codec.rs`
- `src/common/compressed.rs`
- `src/tiff/`
- `src/stitcher.rs`
- `src/wrappers.rs`
- registry and reader-order code

## Completed Audit Records

### 2026-08-13 clinical/DCIMG/legacy/raster/SEM/SPM/ZIP scoped pass

- **Scoped Rust files**: `src/formats/clinical.rs`,
  `src/formats/dcimg.rs`, `src/formats/legacy.rs`,
  `src/formats/raster.rs`, `src/formats/sem.rs`, `src/formats/spm.rs`,
  and `src/formats/zip.rs`.
- **Java sources**:
  `Ecat7Reader.java` was not present in the local `java-bioformats` checkout;
  clinical parity was checked against available `InveonReader.java` and
  `VarianFDFReader.java`. Other mapped sources were `DCIMGReader.java`,
  `KodakReader.java`, `FujiReader.java`, `PictReader.java`,
  `TargaReader.java`, `TargaRLECodec.java`, `GIFReader.java`,
  `PGMReader.java`, `INRReader.java`, `FEIReader.java`,
  `HitachiReader.java`, `ZeissTIFFReader.java`, `LEOReader.java`,
  `IMODReader.java`, `QuesantReader.java`, `SeikoReader.java`,
  `PQBinReader.java`, `WATOPReader.java`, `VGSAMReader.java`,
  `UBMReader.java`, and delegated TIFF/ImageIO readers where applicable.
- **Status**: source-audited for suffix/byte detection, companion-file
  discovery, core metadata, plane mapping, region reads, OME metadata fields
  represented by the Rust metadata model, resolution API behavior, and
  ZIP/delegating-reader forwarding. No new source-backed code gap was found in
  this bounded pass.
- **Fixes already present and verified in this pass**: Targa RLE `0x80` is a
  Java no-op; ZIP forwards child OME/LUT/resolution/compressed APIs; Quesant
  byte probing no longer overclaims stray `IMAG` headers; Quesant and Seiko
  project Java-populated image description/acquisition/physical-size OME fields.
- **Checklist summary**: source mapping/open semantics/parsing/core metadata/
  plane mapping/pixel reads/OME metadata `[x]` for the implemented Java-backed
  paths in the scoped files; resolutions `[n/a]` except delegated ZIP where
  forwarding is `[x]`; compressed extraction `[n/a]` except delegated ZIP where
  forwarding is `[x]`; Rust-only strict raw fallbacks and local non-Java
  extensions are treated as `[~]` and kept isolated from Java byte/name probes.
- **Test evidence**: `cargo test rle_decompress_0x80_packet_is_java_noop --lib`,
  `cargo test native_spm_parity_tests --lib`, and
  `cargo test zip_reader_opens_inner_image_and_rejects_unrecognized_entries --lib`.
- **Remaining fixture gaps**: no real Java-generated OME-XML/pixel CRC fixtures
  for representative DCIMG v0/v1 grouped data and four-pixel correction,
  clinical Inveon/FDF/ECAT7 datasets, Kodak/Fuji/PICT variants, Hitachi helper
  files, FEI/Philips interlaced SEM data, or native Quesant/Seiko/SPM files.
  These are evidence gaps, not confirmed source-backed behavioral defects in
  the audited code paths.

### 2026-08-13 misc/OME/Volocity/Zeiss scoped pass

- **Scoped Rust files**: `src/formats/misc.rs`, `src/formats/misc4.rs`,
  `src/formats/ome_xml.rs`, `src/formats/volocity.rs`,
  `src/formats/zeiss_czi.rs`, `src/formats/zeiss_lsm.rs`,
  `src/formats/zeiss_xrm.rs`, and `src/formats/zeiss_zvi.rs`.
- **Java sources**: `QTReader.java`, `MNGReader.java`, `SlidebookReader.java`,
  `MINCReader.java`, `OpenlabReader.java`, `OpenlabRawReader.java`,
  `JPEG2000Reader.java`, `JPEG2000Writer.java`, `SMCameraReader.java`,
  `TextReader.java`, `APLReader.java`, `AliconaReader.java`,
  `CellWorxReader.java`, `HISReader.java`, `HRDGDFReader.java`,
  `FilePatternReader.java`, `KLBReader.java`, `OBFReader.java`,
  `JPKReader.java`, `OMEXMLReader.java`, `OMEXMLWriter.java`,
  `VolocityReader.java`, `VolocityClippingReader.java`, `ZeissCZIReader.java`,
  `ZeissLSMReader.java`, `ZeissXRMReader.java`, and `ZeissZVIReader.java` from
  the local `java-bioformats` checkout.
- **Status**: source-backed pass complete for dispatch/suffix/byte detection,
  core metadata, OME channel-count padding/name/wavelength projection where
  implemented, companion-file routing, plane and region reads, non-flattened
  resolution APIs, and compressed/optimized paths covered by focused tests.
- **Fixed before/covered by this pass**: standalone JPEG-2000 COD resolution
  levels and lazy reduced-resolution decode; FilePattern/FileStitcher
  resolution forwarding; OME-XML sparse channel padding, BinData endian/default
  behavior, external companion TIFF mapping, and direct companion region reads;
  Volocity Metakit stack OME projection and companion detection; CZI channel
  padding/name/emission/excitation overlay, pyramid resolution selection,
  mosaic/PALM/rotation plane mapping, sparse tile fill, and compressed subblock
  fallbacks; XRM/TXM/TXRM suffix parity and OLE metadata/plane projection; LSM
  suffix/MDB byte-sniff parity, fixed-slot CZ-LSMINFO rich-offset retention, and
  bounded scan-info OME projection; ZVI bounded tag/ROI OME projection,
  including Java `Orca Analog Gain/Offset` tag IDs `65633/65634`,
  experimenter institution, objective ID/name correction parsing, and camera
  acquisition timestamp-derived plane `DeltaT`.
- **Checklist summary**: `misc.rs`, `misc4.rs`, `ome_xml.rs`, `volocity.rs`,
  `zeiss_czi.rs`, `zeiss_xrm.rs`, and `zeiss_zvi.rs` are `[x]` for the audited
  Java-parity surface in the current Rust model; `zeiss_lsm.rs` is `[!]`
  because source-backed full MDB/rich-subblock metadata gaps remain tracked in
  `TOFIX.md`.
- **Test evidence**: `cargo test jpeg2000 --lib`, `cargo test ome_xml --lib`,
  `cargo test volocity --lib`, `cargo test zeiss --lib`,
  `cargo test misc4 --lib`, `cargo test filepattern --lib`,
  `cargo test zeiss_lsm --lib`, `cargo test lsm_ --lib`, and
  `cargo test zeiss_zvi --lib`.
- **Remaining fixture gaps**: no checked-in Java/Rust OME-XML and pixel CRC
  matrix for real QuickTime/MNG/SlideBook SLD/MINC/Openlab/SM Camera/Text,
  misc4 vendor formats, standalone OME-XML companion layouts, native Volocity
  libraries, representative CZI pyramids/mosaics/attachments/ROIs, TXM/TXRM
  acquisitions, LSM MDB multi-file datasets, or ZVI rich overlay/style/tag
  datasets.

### 2026-08-13 AFM/SPM local pass

- **Scoped Rust file**: `src/formats/afm.rs`.
- **Java sources**:
  `java-bioformats/components/formats-gpl/src/loci/formats/in/TopometrixReader.java`,
  `UnisokuReader.java`, and `QuesantReader.java` (Quesant implementation lives
  in Rust `src/formats/spm.rs` and is tracked separately in `TOFIX.md`).
- **Status**: source-backed pass complete for TopoMetrix and Unisoku dispatch,
  header parsing, core metadata, single-plane layout, region reads, and
  representable OME image fields.
- **Fixed in this pass**: TopoMetrix byte detection now matches Java's
  `isThisType(RandomAccessInputStream)` fallthrough: even `#R...` headers
  return false, so reader selection relies on the registered suffixes.
- **Checklist summary**: source mapping/open semantics `[x]`; core metadata and
  single-plane pixel layout `[x]`; OME image description/acquisition date/
  physical size `[x]` for TopoMetrix and Unisoku; channel names/wavelengths and
  non-flattened resolutions `[n/a]`; direct region cropping `[~]` with focused
  tests.
- **Test evidence**: `cargo test afm --lib`.
- **Remaining fixture gaps**: real TopoMetrix/Unisoku/Quesant Java OME-XML and
  pixel CRC fixtures across all supported header versions and data types.

### 2026-08-13 FLIM scoped pass

- **Becker & Hickl SDT / Lambert Instruments LI-FLIM** (`src/formats/flim.rs`)
  - Java sources:
    `java-bioformats/components/formats-gpl/src/loci/formats/in/SDTReader.java`,
    `java-bioformats/components/formats-gpl/src/loci/formats/in/SDTInfo.java`,
    and
    `java-bioformats/components/formats-gpl/src/loci/formats/in/LiFlimReader.java`.
  - Status: source-audited against Java suffix/byte detection, SDT header/setup
    parsing, SDT block/measurement layout, padded raw row handling, compressed
    and raw curve fallback behavior, LI-FLIM INI parsing, version 1/2
    dimensions, raw/gzip/UINT12 plane mapping, background/dark-image series,
    and representable OME fields.
  - Fixed in this pass: LI-FLIM now has a reader-specific OME projection for
    Java-style primary/background image names, `MetadataTools.populatePixels`
    channel/modulo baseline, timestamp-derived plane `DeltaT`, exposure time,
    and sorted ROI polygon points.
  - Checklist summary: source mapping/open semantics/parsing/core metadata/plane
    mapping/pixel reads/OME metadata `[x]` for the implemented SDT and LI-FLIM
    paths; resolutions and compressed tile extraction `[n/a]`; direct SDT
    region-cache optimization `[~]` with focused region tests.
  - Test evidence: `cargo test liflim_ --lib`.
  - Remaining fixture gaps: no real SDT/SPC or LI-FLIM Java-generated OME-XML
    and pixel CRC matrix covering all measurement modes, gzip streams,
    UINT12 dark images, and vendor ROI variants.

### 2026-08-13 slide/TIFF scoped batch

- **Scoped Rust files**: `src/formats/bdv.rs`, `src/formats/olympus.rs`,
  `src/formats/openslide_reader.rs`, `src/formats/svs.rs`,
  `src/formats/tiff_wrappers.rs`, and shared `src/tiff/reader.rs`.
- **Java sources**: `BDVReader.java`, `FV1000Reader.java`,
  `OlympusTileReader.java`, `SVSReader.java`, `MinimalTiffReader.java`,
  `TiffReader.java`, `NDPIReader.java`, `NDPISReader.java`,
  `LeicaSCNReader.java`, and `VentanaReader.java` from the local
  `java-bioformats` checkout.
- **Status**: bounded source-backed pass complete for the implemented BDV
  setup/mipmap model, FV1000/OIF/OIB channel metadata and TIFF delegation,
  Olympus `.omp2info` tile stitching, OpenSlide fallback dispatch and compressed
  tile bridge, SVS label/macro/pyramid topology, wrapper NDPI/SCN/BIF
  non-flattened topology, compressed TIFF tile extraction, and generic TIFF
  SubIFD/JPEG2000/Nikon raw SubIFD handling.
- **Fixed in this pass**: `TiffReader::single_ifd_series` no longer tags every
  helper-built one-IFD series as `NikonRawSubIFD`. The Nikon marker is now added
  only by `add_nikon_raw_sub_ifd_series`, preventing SVS label/macro and other
  wrapper-regrouped series from inheriting Nikon-only metadata that Java does
  not emit.
- **Checklist summary**: source mapping `[x]`; detection/open semantics `[x]`
  for the implemented native/wrapper readers; non-flattened pyramid/resolution
  topology `[x]` for BDV, SVS, NDPI, SCN, BIF, TIFF SubIFDs, and JPEG2000
  synthetic levels; plane and region reads `[x]` for synthetic focused
  fixtures; OME channel name/wavelength projection `[x]` for FV1000, SVS, NDPI,
  and the wrapper paths represented by current OME structs; label/macro/overview
  series `[x]` for SVS/NDPI/SCN/BIF; compressed/optimized paths `[~]` for TIFF
  compressed tile extraction, OpenSlide compressed tile delegation, BDV block
  addressing, and tile-stitching paths covered by focused tests.
- **Test evidence**: `cargo test svs_ --lib`, `cargo test bdv_ --lib`,
  `cargo test fv1000 --lib`, `cargo test ndpi_ --lib`,
  `cargo test scn_ --lib`, `cargo test ventana --lib`, and
  `cargo test nikon_34713_sub_ifd_is_exposed_as_raw_series_before_decoder --lib`.
  Attempted filters `cargo test olympus_tile --lib`,
  `cargo test openslide --lib`, and `cargo test tiff_subifd_pyramid --lib`
  matched no tests and are not counted as coverage.
- **Remaining fixture gaps**: no checked-in Java/Rust CRC and OME-XML comparison
  matrix for real BDV HDF5/XML data, OIF/OIB acquisitions with previews and
  multiple embedded TIFF planes, MRXS/OpenSlide compressed tiles, SVS Z-stack
  pyramids with real label/macro ordering, large NDPI restart-marker windows,
  SCN multi-C/Z/R pyramids, or Ventana BIF overlap XML with sub-resolution
  stitched regions.

### 2026-08-13 camera/extended/metamorph scoped batch

- **Scoped Rust files**: `src/formats/camera2.rs`,
  `src/formats/extended.rs`, `src/formats/metamorph.rs`.
- **Java sources**:
  `CanonRawReader.java`, `DNGReader.java`, `NikonReader.java`,
  `CellomicsReader.java`, `MetamorphReader.java`; `FV1000Reader.java` was
  consulted for the user-requested comparison scope, but the Rust FV1000 reader
  is in `src/formats/olympus.rs` and was outside this write scope.
- **Status**: bounded source-backed pass complete for the implemented Canon
  legacy CRW/TIFF delegation, DNG CFA/white-balance paths, Nikon NEF
  maker-note/CFA paths, Cellomics C01/DIB grouping and OME projection, and
  MetaMorph STK/ND UIC parsing and plane mapping.
- **Fixed in this pass**: Cellomics MDB `CompositeColor` values now match Java's
  BGR interpretation before being packed as OME RGBA colors. Previously Rust
  treated the 24-bit value as RGB.
- **Checklist summary**: source mapping `[x]`; detection/open semantics `[x]`
  for the focused readers and registry-gated generic TIFF/DNG/Nikon/MetaMorph
  paths; companion discovery `[x]` for Cellomics sibling channel files and
  MetaMorph `.nd` grids in the implemented scope; core metadata/plane mapping
  `[x]` for Canon CRW, DNG CFA, Nikon NEF, Cellomics grouped channels/fields,
  and MetaMorph UIC/ND dimensions; pixel and region layout `[x]` for existing
  synthetic full-plane/nonzero-region paths; OME channel names/wavelengths
  `[x]` for Cellomics and MetaMorph fields represented in Rust's OME model;
  resolutions `[n/a]`; compressed/optimized paths `[~]` for CFA decode caches
  and in-memory crop helpers with focused tests.
- **Test evidence**: `cargo test cellomics --lib` after the color fix; focused
  camera/MetaMorph tests were also run in this pass.
- **Remaining fixture gaps**: no checked-in Java/Rust CRC and OME-XML comparison
  fixtures for real Canon CRW/CR2/DNG/NEF RAW files, Cellomics plate datasets
  with MDB channel colors/exposures, or MetaMorph STK/ND acquisitions with
  varied UIC1/UIC2/UIC3/UIC4 tables. FV1000/OIF/OIB parity remains recorded
  outside this scoped file batch because its Rust implementation lives in
  `src/formats/olympus.rs`.

### 2026-08-13 Norpix/IPLab/MIAS/SimFCS local batch

- **Scoped Rust files**: `src/formats/norpix.rs`, `src/formats/mias.rs`,
  `src/formats/simfcs.rs`.
- **Java sources**:
  `java-bioformats/components/formats-gpl/src/loci/formats/in/SEQReader.java`,
  `IPLabReader.java`, `CellWorxReader.java`, `AliconaReader.java`,
  `OxfordInstrumentsReader.java`, `FEIReader.java`, and `MIASReader.java`.
  SimFCS has no matching Java Bio-Formats reader in the local source tree and
  is classified as intentional Rust-only support.
- **Status**: source-backed dispatch/core pass complete for Norpix StreamPix,
  Image-Pro SEQ/IPS, IPLab, Alicona AL3D, Oxford TOP, FEI/SER-style Rust
  support, MIAS TIFF hierarchy routing, CellWorx well-log scalar metadata, MIAS
  result/template/overlay companion indexing, and SimFCS raw 256x256 frames.
- **Fixed in this pass**: `IplabReader` no longer claims `.ipm` by filename;
  Java registers only `.ipl` and relies on the `iiii`/`mmmm` header probe for
  extensionless or nonstandard names. `MiasReader` now mirrors Java byte
  detection by reading the first TIFF IFD and accepting Software values starting
  with `eaZYX`, `SCIL_Image`, or `IDL`. CellWorx now derives Java
  `parseWellLogFile` fields for channel names, acquisition date, scan origin,
  scan area, gain, and EX/EM wavelengths; MIAS now accepts Java-style `.txt`
  result/template entry points and records template/results/ROI/overlay/mask
  companions. MIAS detail companions now project Java `populateROI` ellipse
  fields (`Label`, `Col`, `Row`, `Cell Diam.`, filename T/Z) through the local
  OME ROI model. MIAS `AllModesOverlay.tif` channel colors now follow Java
  `getChannelColorFromFile` ColorMap max/tie behavior and populate
  `OmeChannel.color` for every series on the matching channel.
- **Checklist summary**: Norpix/SEQ/IPLab source mapping, detection, core
  metadata, IPS channel names, and TIFF-backed plane reads `[x]`; SimFCS
  Java-parity `[n/a]` with Rust-only raw-frame behavior documented and tested;
  MIAS/Alicona/Oxford core byte/name detection `[x]` after the MIAS fix; MIAS
  structured HCS/analysis metadata and broader CellWorx/MetaXpress OME
  projection remain `[!]` only for fixture-backed Java/Rust OME comparisons
  across richer real companion datasets. MIAS and
  CellWorx/MetaXpress now build Java-style OME
  Plate/Well/WellSample graph links for all parsed series, including Java's
  MIAS Plate.Name overwrite from the plate directory suffix. The MetaXpress
  fixture-backed test in `tests/stub_cellworx_test.rs` now asserts HTD wave
  names into Rust series metadata and OME channels and reads one present TIFF
  while zero-filling one missing companion; it is an explicit Cargo test target
  and skips when the external `testdata/metaxpress/` sample is absent. The
  available fixture still has no scan logs or MIAS
  template/results/ROI/overlay/mask companions.
- **Test evidence**:
  `cargo test iplab_name_detection_matches_java_registered_suffix --lib`,
  `cargo test mias_byte_probe_matches_java_software_prefixes --lib`,
  `cargo test mias --lib`, `cargo test cellworx --lib`, and
  `cargo test --test stub_cellworx_test`.
- **Remaining fixture gaps**: no checked-in Java/Rust CRC and OME-XML comparison
  matrix for IPLab metadata blocks, Image-Pro IPS multi-position datasets,
  CellWorx HTD/PNL/log acquisitions with wavelengths/gain/exposure, MIAS
  result/template/ROI/mask companion files, Alicona padded-row fixtures, Oxford
  TOP fixtures, or FEI/Philips electron-microscopy fixtures.

### 2026-08-13 HCS/container scoped batch

- **Scoped readers** (`src/formats/cellh5.rs`, `src/formats/flex.rs`,
  `src/formats/hcs2.rs`, `src/formats/imaris_hdf.rs`,
  `src/formats/incell.rs`, `src/formats/perkinelmer.rs`,
  `src/formats/visitech.rs`)
  - Java sources:
    `java-bioformats/components/formats-bsd/src/loci/formats/in/CellH5Reader.java`,
    `java-bioformats/components/formats-gpl/src/loci/formats/in/FlexReader.java`,
    `InCellReader.java`, `InCell3000Reader.java`, `ImarisHDFReader.java`,
    `PerkinElmerReader.java`, `VisitechReader.java`, plus HCS readers mirrored
    inside `hcs2.rs` (`ColumbusReader`, `OperettaReader`, `ScanrReader`,
    `TrestleReader`, `CellVoyagerReader`, and related TIFF wrappers).
  - Status: bounded source-backed pass complete for detection/open semantics,
    companion discovery, HCS/plate metadata, core dimensions, series/field and
    plane mapping, region/pixel layout, non-flattened Trestle resolutions,
    Java-populated OME channel names/wavelengths, CellH5 ROIs, InCell
    instrument/detector fields, and Imaris HDF channel/surpass metadata paths.
  - Fixed in this pass: `InCell3000Reader` no longer claims `.xdce` by name;
    Java registers only `.frm`. The XML `.xdce` path remains owned by
    `InCellReader`, with a missing-companion diagnostic that preserves the
    existing HCS rejection contract.
  - Existing fixed items confirmed in this scope: CellH5 Java-style ROI
    rectangles; Operetta/Columbus/ScanR/CellVoyager OME channel projection;
    Trestle non-flattened pyramid levels; PerkinElmer HTML exposure
    emission/excitation wavelengths.
  - Checklist summary: source mapping/open semantics/parsing/core metadata/plane
    mapping/pixel reads/OME metadata `[x]` for the focused implemented paths;
    resolutions `[x]` where relevant for Trestle and Imaris HDF,
    `[n/a]` elsewhere; compressed extraction `[n/a]` for these readers in the
    audited paths; optimized direct region paths `[~]` where covered by focused
    crop/region tests.
  - Test evidence: `cargo test hcs2 --lib`, `cargo test incell --lib`,
    `cargo test flex_ --lib`, `cargo test imaris --lib`,
    `cargo test cellh5 --lib`, `cargo test perkinelmer --lib`,
    `cargo test visitech --lib`, and
    `cargo test visitech_registry_opens_metadata_only_xys_but_rejects_hcs_without_tiffs --lib`.
  - Remaining fixture gaps: no checked-in Java/Rust OME-XML and pixel CRC matrix
    for real CellH5 segmentation plates, grouped FLEX `.mea/.res` datasets,
    full InCell XDCE TIFF/IM layouts, Imaris HDF multi-resolution/channel color
    volumes, PerkinElmer Columbus/Opera Phenix directory sets, or Visitech
    multi-position `.xys` reports. CellH5 rectangle text/stroke color still
    awaits Rust OME shape fields.

### 2026-08-13 microscopy/scientific batch

- **Bio-Rad PIC** (`src/formats/biorad.rs`)
  - Java sources: `java-bioformats/components/formats-gpl/src/loci/formats/in/BioRadReader.java`.
  - Rust sources: `src/formats/biorad.rs`.
  - Status: source-audited against Java `.pic`/`lse.xml`/`data.raw` detection,
    PIC companion resolution, grouped sibling PIC handling, header/note/XML/LUT
    parse order, `XYCTZ` core metadata, multi-file `no % picFiles.length` plane
    mapping, packed raw region reads, LUT/indexed metadata, and representable
    OME objective/detector/physical-size fields. Existing focused tests cover
    byte/name detection, notes, LUTs, lse.xml grouping, plane mapping, and OME
    detector settings.
  - Remaining fixture gap: no real PIC/lse.xml Java OME-XML and pixel CRC
    comparison fixture.

- **Bruker MRI** (`src/formats/bruker.rs`)
  - Java sources: `java-bioformats/components/formats-gpl/src/loci/formats/in/BrukerReader.java`.
  - Rust sources: `src/formats/bruker.rs`.
  - Status: source-audited against Java exact `fid`/`acqp` name detection,
    false byte probe, acquisition directory discovery, `acqp`/`reco`/optional
    `d3proc` parsing, core dimension fallback, raw `2dseq` plane offsets,
    direct row-region reads, and OME image name/acquisition date/experimenter
    fields. Existing focused tests cover a synthetic ParaVision dataset,
    metadata, full plane, region, and OME fields.
  - Remaining fixture gap: no real Bruker MRI Java OME-XML and pixel CRC
    comparison fixture.

- **Gatan DM3/DM4/DM2** (`src/formats/gatan.rs`)
  - Java sources: `java-bioformats/components/formats-gpl/src/loci/formats/in/GatanReader.java`.
  - Rust sources: `src/formats/gatan.rs`.
  - Status: source-audited against Java first-int byte detection, invalid-header
    rejection, DM3/DM4 tag tree parsing, padding and endian adjustment, payload
    byte-count pixel-type reconciliation, `XYZTC` core metadata, montage series
    splitting, absolute plane mapping, direct raw region reads, physical-size
    selection, acquisition mode, plane exposure/stage positions, and
    representable ROI/instrument OME fields. Existing tests cover invalid
    headers, montage mapping, OME fields, ROI conversion, and optional real
    DM3/DM4 fixtures when present.
  - Remaining fixture gap: no checked-in Java-generated OME-XML/CRC fixture for
    real DM3/DM4 montage and annotation files.

- **MRC/IMOD** (`src/formats/mrc.rs`)
  - Java sources: `java-bioformats/components/formats-gpl/src/loci/formats/in/MRCReader.java`.
  - Rust sources: `src/formats/mrc.rs`.
  - Status: source-audited against Java suffixes, byte probe, header offsets,
    endian fallback, IMOD signed flag, EMAN2 unsigned correction, RGB/core
    metadata, lower-left row flip, truncated payload zero-fill, OME image name,
    and physical-size projection. Fixed source-backed suffix overclaim: Rust no
    longer claims `.ccp4` by name, matching Java's `MRC_SUFFIXES`; header byte
    probing still accepts valid MRC/CCP4 data.
  - Remaining fixture gap: no real MRC/IMOD Java OME-XML and pixel CRC
    comparison fixture, and AGAR unit distinction is retained as original
    metadata rather than a structured Rust OME unit.

### 2026-08-13 simple raster batch

- **BMP** (`src/formats/bmp.rs`)
  - Java sources: `java-bioformats/components/formats-bsd/src/loci/formats/in/BMPReader.java`.
  - Rust sources: `src/formats/bmp.rs`.
  - Status: source-audited against Java header parsing, `BM` byte detection,
    `XYCTZ` core metadata, BGR-to-RGB conversion, indexed palette handling,
    RLE4/RLE8 decode shape, region cropping, basename OME image name, and
    pixels-per-metre physical-size projection. Existing focused tests cover
    raw/RLE/BITFIELDS layout, region cropping, OME basics, clean failed
    `set_id`, and byte/name detection.
  - Remaining fixture gap: no real Java-generated pixel CRC/OME-XML comparison
    for external BMP fixtures.

- **JPEG** (`src/formats/jpeg.rs`)
  - Java sources: `java-bioformats/components/formats-bsd/src/loci/formats/in/JPEGReader.java`,
    `java-bioformats/components/formats-bsd/src/loci/formats/in/ImageIOReader.java`.
  - Rust sources: `src/formats/jpeg.rs`.
  - Status: source-audited against Java delegate/ImageIO semantics for suffixes
    (`jpg`, `jpeg`, `jpe`), SOI byte detection, one image plane, `XYCZT`
    metadata, non-interleaved RGB channel-separated output, false
    little-endian flag, planar region cropping, and basename OME image name.
  - Remaining fixture gap: no Java parity fixture for large JPEG restart-marker
    tiled path or CMMException APP-marker fallback; Rust currently decodes via
    the `image` crate and crops from decoded planar pixels.

- **PNG/APNG** (`src/formats/png.rs`)
  - Java sources: `java-bioformats/components/formats-bsd/src/loci/formats/in/APNGReader.java`.
  - Rust sources: `src/formats/png.rs`.
  - Status: source-audited against Java PNG signature detection, `.png` suffix,
    IHDR color-type core metadata, `XYCTZ` order, indexed LUT parsing, APNG
    frame count as `SizeT`/`imageCount`, interleaved RGB/alpha layout,
    big-endian 16-bit sample bytes, still-PNG fallback, and full decoded region
    cropping. Rust's still-PNG path deliberately rejects APNG control chunks so
    animated files use the APNG parser.
  - Remaining fixture gap: no Java CRC comparison for APNG disposal/blend
    variants; current parser models the Java source path for default frame
    compositing cases covered by code review.

- **PCX** (`src/formats/pcx.rs`)
  - Java sources: `java-bioformats/components/formats-bsd/src/loci/formats/in/PCXReader.java`.
  - Rust sources: `src/formats/pcx.rs`.
  - Status: source-audited against Java magic byte detection, `.pcx` suffix,
    little-endian header fields, Java's `xMax - xMin` / `yMax - yMin`
    dimensions, version-specific `nColorPlanes`/`bytesPerLine` offsets,
    version-5 single-plane LUT, planar RLE decode, non-interleaved region
    layout, and `XYCZT` core metadata.
  - Remaining fixture gap: no real PCX fixture compared against Java for
    palette marker variations or malformed RLE truncation behavior.

- **Khoros XV** (`src/formats/khoros.rs`)
  - Java sources: `java-bioformats/components/formats-gpl/src/loci/formats/in/KhorosReader.java`.
  - Rust sources: `src/formats/khoros.rs`.
  - Status: source-audited against Java `0xab01` byte detection, `.xv` suffix,
    dependency-controlled endianness, 1024-byte header parsing, LUT offset,
    indexed sizeC collapse, `SizeZ = imageCount`, planar pixel layout, and
    region cropping. Fixed during this audit: Rust no longer claims `.viff` by
    name because the Java reader registers only `.xv`.
  - Remaining fixture gap: no real XV fixture compared against Java for
    multi-band LUT and non-`UINT8` pixel payload CRCs.

## Optimization Preservation Gate

Before replacing an optimized Rust path with a more literal Java-shaped path,
check this gate:

- [ ] Does the optimized path preserve Java-visible core metadata, OME metadata,
      series/resolution topology, plane mapping, pixel bytes, region reads, and
      error behavior for the audited cases?
- [ ] Is there a focused test proving the optimized path against a Java-shaped
      edge case or fixture?
- [ ] Does the optimization preserve Java-shaped control flow without adding a
      helper layer that can drift from Java, unless it is adapting a Java
      library call to a Rust library API with a different shape?
- [ ] If any answer is no, prefer the Java-shaped implementation first and keep
      a `TOFIX.md` item for reintroducing the optimization with evidence.

## Audit Record Template

## Focused Audit Records 2026-08-13: Simple Scientific/Volume Readers

### Format: AIM

- Rust reader(s): `src/formats/aim.rs` (`AimReader`; includes Rust-only ISQ support).
- Java reader(s): `java-bioformats/components/formats-gpl/src/loci/formats/in/AIMReader.java`.
- Audit status: checklist pass complete for Java-backed `.aim`; no source-backed open/core/plane/region/OME channel bug found.
- Checklist summary: source mapping `[x]`; open semantics `[~]` because Rust also accepts `.isq`/`CTDATA-HEADER_V1` with no local Java reader; parsing/core/plane mapping/pixel reads `[x]` for AIM V020/V030 dimensions, little-endian INT16, processing-log pixel offset, zero-filled missing AIM planes, and full-plane-cropped regions; resolutions `[n/a]`; OME channel names/emission/list length `[n/a]`.
- Test evidence: added `aim_v020_core_metadata_and_region_match_java_layout`; existing focused tests in unregistered `tests/format_tests.rs` cover invalid/missing AIM magic, zero dimensions, short ISQ rejection, Java-style short AIM zero-fill, processing-log acquisition date, and physical sizes. Remaining fixture gap: no real Java-compared AIM V030 fixture with processing log and pixel CRCs.

### Format: Amira

- Rust reader(s): `src/formats/amira.rs` (`AmiraReader`; `SpiderReader` lives in the same file but was outside this four-reader checklist except as shared file context).
- Java reader(s): `java-bioformats/components/formats-gpl/src/loci/formats/in/AmiraReader.java`; helper `java-bioformats/components/formats-bsd/src/loci/formats/tools/AmiraParameters.java`.
- Audit status: checklist pass complete for Amira Mesh; no source-backed open/core/plane/region/OME channel bug found.
- Checklist summary: source mapping/open semantics/parsing/core metadata/plane mapping/pixel reads `[x]` for Java suffixes, header byte probe, ASCII/binary endianness, `define Lattice`, stream-count-to-SizeC, non-RGB channels, HxZip/HxByteRLE stack decode, and full-plane-cropped regions; resolutions `[n/a]`; OME channel names/emission/list length `[n/a]`.
- Test evidence: existing tests cover Java suffixes, malformed/short ASCII, invalid dimensions/types, short binary payload, multi-stream non-RGB channel mapping, and compressed/ASCII paths in this file. Remaining fixture gap: no real Java-compared Amira file with `Materials` LUT, units, and pixel CRCs across compressed and ASCII streams.

### Format: AVI

- Rust reader(s): `src/formats/avi.rs` (`AviReader`).
- Java reader(s): `java-bioformats/components/formats-bsd/src/loci/formats/in/AVIReader.java`.
- Audit status: checklist pass complete for the implemented AVI subset; no new source-backed bug found in this pass.
- Checklist summary: source mapping/open semantics `[x]` for `.avi` and `RIFF`/`AVI ` byte probing; parsing/core metadata/plane mapping/pixel reads `[x]` for `avih`/`strh`/`strf` metadata, Java-style frame width reconciliation, T-axis frame mapping, raw bottom-up BGR/RGB handling, indexed 4/8-bit paths, Y8 top-down rows, MJPEG compressed extraction, and full-plane-cropped regions; resolutions `[n/a]`; OME channel names/emission/list length `[n/a]`.
- Test evidence: existing `avi_` unit tests cover chunk metadata, writer bounds, indexed/RLE/MS_VIDEO/Cinepak/MJPEG branches, and compressed extraction. Remaining fixture gap: no real Java-compared AVI corpus covering palette variants, malformed chunk truncation, and codec CRCs.

### Format: DICOM

- Rust reader(s): `src/formats/dicom.rs` (`DicomReader`).
- Java reader(s): `java-bioformats/components/formats-bsd/src/loci/formats/in/DicomReader.java`.
- Audit status: checklist pass complete with source-backed suffix and encapsulated tiled-WSI fixes recorded.
- Checklist summary: source mapping `[x]`; open semantics `[x]` after restricting name-only suffix detection to Java's sufficient suffixes (`dic`, `dcm`, `dicom`, `j2ki`, `j2kr`); parsing/core metadata/plane mapping/pixel reads `[x]` for preamble/preambleless detection, explicit/implicit VR, grouped files, WSI tile stitching, native/RLE/JPEG/JPEG2000 decoding, MONOCHROME1 inversion, pixel spacing, content timestamp, channel names, and planar RGB conversion; resolutions `[~]` Rust exposes grouped smaller-dimension images through its current series model but does not yet expose Java's full non-flattened resolution graph; OME channel names/list length `[x]` for optical path descriptions.
- Test evidence: added `dicom_name_detection_matches_java_sufficient_suffixes`; existing focused tests cover RLE segment layout, JPEG trim, WSI encapsulated JPEG tile stitching, pixel spacing/timestamp, tag capture, grouping, and transfer-syntax classification. Remaining fixture gap: no real Java-compared DICOM set covering multi-resolution WSI pyramids, optical-path channel grouping, and compressed tile CRCs.

### Format: FITS

- Rust reader(s): `src/formats/fits.rs` (`FitsReader`).
- Java reader(s): `java-bioformats/components/formats-bsd/src/loci/formats/in/FitsReader.java`.
- Audit status: checklist pass complete; one source-backed suffix mismatch fixed.
- Checklist summary: source mapping `[x]`; open semantics `[x]` after restricting reader suffixes to Java's `fits`/`fts`; parsing/core metadata/plane mapping/pixel reads `[x]` for `SIMPLE` rejection, extended-header scan until dimensions, raw big-endian `BITPIX` mapping, BZERO/BSCALE metadata-only behavior, Java-style sizeZ clamp, primary/first-image-extension behavior, and full-plane-cropped regions; resolutions `[n/a]`; OME channel names/emission/list length `[n/a]`.
- Test evidence: added `fits_reader_name_suffixes_match_java`; existing tests cover primary-only behavior, empty-primary image extension, unsupported `BITPIX`, negative `BITPIX`, BZERO/BSCALE raw bytes, and stack reads. Remaining fixture gap: no real Java-compared FITS fixture covering long/multi-block headers and truncated stack CRCs.

### Format: NIfTI

- Rust reader(s): `src/formats/nifti.rs` (`NiftiReader`).
- Java reader(s): `java-bioformats/components/formats-gpl/src/loci/formats/in/NiftiReader.java`.
- Audit status: checklist pass complete; one source-backed suffix/open mismatch fixed.
- Checklist summary: source mapping `[x]`; open semantics `[x]` after matching Java name-only detection for `.nii`/`.nii.gz` while `.hdr`/`.img` require byte/open probing; parsing/core metadata/plane mapping/pixel reads `[x]` for dimension-endianness probing, paired `.img` to `.hdr` redirect, header `vox_offset`, extra dimensions folded into SizeC, Java RGB24/unsupported RGBA fallthrough, `XYCZT`, voxel/time metadata, gzip reset semantics via full inflate, and full-plane-cropped regions; resolutions `[n/a]`; OME channel names/emission/list length `[n/a]`.
- Test evidence: added `name_detection_matches_java_nii_suffix_only`; existing tests cover magic byte probing, header endianness, paired header/data offset, zero `vox_offset`, units, RGB24, and unsupported RGBA. Remaining fixture gap: no real Java-compared NIfTI/Analyze fixtures for `.nii.gz` streaming, big-endian headers, and multi-extra-dimension CRCs.

### Format: NRRD

- Rust reader(s): `src/formats/nrrd.rs` (`NrrdReader`).
- Java reader(s): `java-bioformats/components/formats-bsd/src/loci/formats/in/NRRDReader.java`.
- Audit status: checklist pass complete with existing gzip-region fix recorded.
- Checklist summary: source mapping/open semantics `[x]` for `.nrrd`/`.nhdr` suffixes and `NRRD` magic; parsing/core metadata/plane mapping/pixel reads `[x]` for scalar/RGB axis mapping, detached data files, raw/gzip payloads, endian handling, physical sizes, and full-plane-cropped regions; compressed path `[x]` after non-zero-`y` gzip regions return a clean Java-divergence error instead of silently returning different bytes; resolutions `[n/a]`; OME channel names/emission/list length `[n/a]`.
- Test evidence: existing tests cover sidecar path confinement, unsupported type rejection, raw/gzip read paths, and `gzip_region_nonzero_y_rejects_like_java`. Remaining fixture gap: no real Java-compared NRRD/NHDR fixtures covering detached gzip payloads, vector axis kinds beyond RGB, and nontrivial space directions.

### Format: MetaImage

- Rust reader(s): `src/formats/metaimage.rs` (`MetaImageReader`).
- Java reader(s): no corresponding Bio-Formats Java reader found under local `java-bioformats/components/*/src/loci/formats/in`.
- Audit status: inventory marked complete as Rust-only/non-upstream for this local source tree; Java parity checklist is not applicable beyond registry isolation and existing Rust behavior tests.
- Checklist summary: source mapping `[x]` for absence; open semantics/core metadata/plane mapping/pixel reads `[~]` as Rust-only support for `.mha`/`.mhd`, detached/local/per-slice data, element channels, endian swap, and region cropping; resolutions `[n/a]`; OME channel names/emission/list length `[n/a]`.
- Test evidence: existing tests cover MHA/MHD round trips, interleaved element channels with nonzero-origin region crop, malformed dimensions, unsupported ASCII/type, and confined detached data paths. Remaining fixture gap: no upstream Java fixture exists locally; broader Rust fixture gaps include compressed `.zraw`, `LIST`/printf multi-slice layouts, and `HeaderSize=-1`.

Copy this template into batch notes or issue-specific sections as each reader is
audited.

```markdown
## Format: <name>

- Rust reader(s):
- Java reader(s):
- Fixtures:
- Java version:
- Audit status:

### Checklist Summary

- Source mapping:
- Open semantics:
- Parsing structure:
- Core metadata:
- Plane mapping:
- Pixel reads:
- Resolutions/pyramids:
- OME metadata:
- Compressed/optimized paths:
- Test evidence:

### Findings

- `[x]` Matches Java:
- `[~]` Optimized but parity-proven:
- `[!]` Needs fixing:
- `[?]` Blocked/unverified:

### Required Tests

- Core metadata:
- OME metadata:
- Pixels:
- Regions:
- Negative cases:
```

## Audit Records

## Format: Requested microscopy reader batch

- Rust reader(s): `src/formats/deltavision.rs`, `src/formats/ics.rs`,
  `src/formats/lif.rs`, `src/formats/lim.rs`,
  `src/formats/micromanager.rs`, `src/formats/prairie.rs`,
  `src/formats/spe.rs`, `src/formats/sif.rs`.
- Java reader(s):
  `java-bioformats/components/formats-gpl/src/loci/formats/in/DeltavisionReader.java`,
  `java-bioformats/components/formats-bsd/src/loci/formats/in/ICSReader.java`,
  `java-bioformats/components/formats-gpl/src/loci/formats/in/LIFReader.java`,
  `java-bioformats/components/formats-gpl/src/loci/formats/in/LIMReader.java`,
  `java-bioformats/components/formats-bsd/src/loci/formats/in/MicromanagerReader.java`,
  `java-bioformats/components/formats-gpl/src/loci/formats/in/PrairieReader.java`,
  `java-bioformats/components/formats-gpl/src/loci/formats/in/SPEReader.java`,
  and
  `java-bioformats/components/formats-gpl/src/loci/formats/in/SIFReader.java`.
- Fixtures: existing synthetic/unit fixtures in the target Rust files.
- Java version: local `java-bioformats` checkout.
- Audit status: `[x]` bounded source-backed pass over detection/open
  semantics, companion discovery where applicable, dimensions, plane mapping,
  region layout, and Java-populated OME channel/instrument/plane metadata.

### Checklist Summary

- Source mapping: `[x]` all eight Rust readers map directly to the listed Java
  readers; Micro-Manager and Prairie additionally delegate pixel reads through
  TIFF readers like Java.
- Open semantics: `[x]` suffix/name routing and byte probes match the audited
  Java paths after fixing Micro-Manager's byte probe to delegate to TIFF header
  detection. Rust still has documented additive Micro-Manager `metadata.json`
  name support.
- Parsing structure: `[x]` existing source comments and tests cover Java-shaped
  DeltaVision headers/logs, ICS axes/history/lifetime handling, LIF memory
  blocks/XML, LIM fixed header, Micro-Manager metadata/private TIFF JSON,
  Prairie XML/CFG layouts, SPE binary headers/ROIs, and SIF `Pixel number`
  parsing.
- Core metadata: `[x]` audited paths preserve Java dimensions, pixel type,
  endianness, RGB/interleaving, dimension order, and image-count behavior.
- Plane mapping: `[x]` existing tests cover DeltaVision truncated planes,
  ICS lifetime contiguous planes, LIF block/stride layouts, Micro-Manager
  `Coords-*` mapping, Prairie missing channels, SPE frame fallback, and SIF
  linear plane offsets.
- Pixel reads: `[x]` full-plane and region paths are Java-shaped for the covered
  uncompressed/raw cases; Micro-Manager/Prairie delegate real TIFF bytes through
  the TIFF reader and return zero-filled missing planes like Java.
- Resolutions/pyramids: `[n/a]` no Java nested-pyramid behavior found for this
  batch in the audited reader paths.
- OME metadata: `[x]` covered Java calls for channel names/wavelengths,
  detector/objective/instrument refs, and per-plane time/position/exposure in
  the existing target-reader tests where those readers populate such fields.
- Compressed/optimized paths: `[~]` LIF/DeltaVision/ICS optimized layout paths
  are kept with focused synthetic parity tests; broader compressed real-data
  coverage remains fixture-limited.
- Test evidence: focused target-reader tests listed in each file, including
  the LIM fixed-offset/RGB-swap and compressed-rejection tests added during
  this pass, plus `cargo test micromanager --lib` for the new detection fix.

### Findings

- `[x]` Matches Java: LIM fixed-offset uncompressed read and RGB byte swap; SPE
  weak stream probe, frame fallback, incomplete XML flag, missing-payload zero
  plane, and ROI metadata; SIF `Pixel number` dimensions and footer-relative
  payload offset; the previously fixed DeltaVision, ICS, LIF, Prairie, and
  Micro-Manager metadata/plane behaviors recorded in `TOFIX.md`.
- `[~]` Optimized but parity-proven: Rust region helpers crop in memory for
  several simple raw readers instead of streaming row intersections directly;
  existing nonzero-origin tests cover the byte layout for those paths.
- `[!]` Needs fixing: none newly confirmed in the allowed files after the
  Micro-Manager byte-probe correction.
- `[?]` Blocked/unverified: real Java/Rust CRC and OME-XML comparisons are still
  needed for representative real DeltaVision log variants, ICS gzip/IDS pairs,
  complex LIF LAS AF detector/filter graphs, multi-position Micro-Manager
  acquisitions with malformed private JSON, Prairie single-TIFF mode, and
  vendor SPE/SIF files with nontrivial timestamps/ROIs.

## Format: Encapsulated PostScript / EPS

- Rust reader(s): `src/formats/eps.rs`
- Java reader(s): `java-bioformats/components/formats-bsd/src/loci/formats/in/EPSReader.java`
- Fixtures: synthetic inline hex raster and vector-only EPS tests.
- Java version: local `java-bioformats` checkout.
- Audit status: `[x]` source-backed bounded parity pass.

### Checklist Summary

- Source mapping: `[x]` `EPSReader` suffixes `eps`, `epsi`, `ps` map to
  `EpsReader`.
- Open semantics: `[x]` PostScript files must start with `%!PS`; DOS EPS TIFF
  preview handling is implemented; vector-only PostScript is rejected like Java
  when no image operator is found.
- Parsing structure: `[x]` BoundingBox, ImageData, BeginBinary, `image`, and
  `colorimage` branches follow Java's raster-reader control flow.
- Core metadata: `[x]` single `XYCZT` UINT8 series for inline rasters, RGB when
  `sizeC == 3`.
- Plane mapping: `[x]` one logical plane.
- Pixel reads: `[x]` full-plane and region reads over inline raster payload are
  covered.
- Resolutions/pyramids: `[n/a]` no pyramid support in Java or Rust reader.
- OME metadata: `[x]` generic single-image/channel population; Java does not
  set channel names or wavelengths for EPS.
- Compressed/optimized paths: `[n/a]`.
- Test evidence: `cargo test eps_reader --lib`.

### Findings

- `[x]` Matches Java: suffixes, inline raster subset, vector-only rejection,
  single-plane metadata, and region cropping.
- `[~]` Optimized but parity-proven: Rust uses direct in-memory region cropping;
  the covered region test proves the byte layout for the Java inline-raster
  subset.
- `[!]` Needs fixing: none found in this source-backed pass.
- `[?]` Blocked/unverified: real-world DOS EPS TIFF-preview fixture comparing
  Java/Rust pixel CRCs would strengthen coverage.

## Format: Fake

- Rust reader(s): `src/formats/fake.rs`
- Java reader(s): `java-bioformats/components/formats-bsd/src/loci/formats/in/FakeReader.java`
- Fixtures: synthetic filename and `.fake.ini` parameter tests.
- Java version: local `java-bioformats` checkout.
- Audit status: `[x]` bounded core/pixel parity with representable structured
  OME graph fields projected.

### Checklist Summary

- Source mapping: `[x]` `FakeReader` filename token and `.fake.ini` parsing map
  to Rust `FakeReader`.
- Open semantics: `[x]` suffix/name routing, missing `.fake` lookup through
  `.fake.ini`, and fake-series discovery are represented by current tests.
- Parsing structure: `[x]` Java filename/INI token parsing, validation, RGB
  dimension-order correction, resolution scale, and SPW series-count override
  are translated.
- Core metadata: `[x]` size, pixel type, bits, RGB/effective C, dimension order,
  endian, indexed/false-color, thumbnails, and non-flattened resolutions are
  tested.
- Plane mapping: `[x]` Java ZCT/special-box plane index encoding is tested.
- Pixel reads: `[x]` full-plane and RGB/region generation match Java's gradient
  and special-pixel rules in focused tests.
- Resolutions/pyramids: `[x]` Rust exposes Java non-flattened fake resolutions
  through `resolution_count()` / `set_resolution()`.
- OME metadata: `[x]` channel names, wavelengths, colors, physical sizes,
  acquisition date, SPW plate/well/sample metadata, representable ROI shapes,
  supported annotations, and instrument/filter/light-source/dichroic refs are
  projected. Java label/mask/microbeam details that the current Rust OME model
  cannot represent are emitted as explicit map annotations.
- Compressed/optimized paths: `[n/a]`.
- Test evidence: `cargo test fake --lib`.

### Findings

- `[x]` Matches Java: core metadata, non-flattened resolutions, pixel gradient,
  RGB/interleaving behavior, channel names/wavelengths/colors, SPW
  series-to-well-sample mapping, representable ROI geometry, annotations, and
  instrument/light-path references.
- `[~]` Optimized but parity-proven: direct synthetic region generation avoids
  allocating full planes and is covered by region/layout tests.
- `[!]` Needs fixing: none remaining in the representable Rust OME model.
- `[?]` Blocked/unverified: exact Java OME-XML comparison for complex fake
  parameter combinations, especially Java XMLMockObjects microbeam details and
  mask binary payloads that are intentionally summarized as map annotations.

## Format: IMAGIC

- Rust reader(s): `src/formats/imagic.rs`
- Java reader(s): `java-bioformats/components/formats-gpl/src/loci/formats/in/ImagicReader.java`
- Fixtures: synthetic `.hed`/`.img` single-record PACK fixture.
- Java version: local `java-bioformats` checkout.
- Audit status: `[x]` source-backed bounded parity pass after metadata-level fix.

### Checklist Summary

- Source mapping: `[x]` `.hed`/`.img` pair maps to Java `ImagicReader`.
- Open semantics: `[x]` `.hed` name is sufficient; `.img` requires sibling
  `.hed`, matching Java `isThisType`.
- Parsing structure: `[x]` 1024-byte header records, dimensions at Java offsets,
  type string handling, image name, and physical-size fields are translated.
- Core metadata: `[x]` `XYZCT`, one channel, `SizeZ == imageCount`, little
  endian, and Java pixel-type mapping for `REAL`/`INTG`/`PACK`.
- Plane mapping: `[x]` plane index maps to sequential `.img` payload records.
- Pixel reads: `[x]` full-plane and region reads are covered by the synthetic
  fixture.
- Resolutions/pyramids: `[n/a]`.
- OME metadata: `[x]` image name and physical sizes match Java metadata-level
  behavior; `MINIMUM` skips physical sizes.
- Compressed/optimized paths: `[n/a]`.
- Test evidence:
  `cargo test imagic_minimal_metadata_skips_physical_sizes_like_java --lib` and
  `cargo test imagic --lib`.

### Findings

- `[x]` Matches Java: paired-file detection, core metadata, plane layout, image
  name, physical-size scaling, and `MINIMUM` metadata gating.
- `[~]` Optimized but parity-proven: Rust region reads crop a full plane; the
  nonzero-origin synthetic region test covers the byte layout.
- `[!]` Needs fixing: none remaining from source audit.
- `[?]` Blocked/unverified: real IMAGIC fixtures for all pixel types and
  Java/Rust OME-XML plus pixel CRC comparison.

## Format: Adobe Photoshop / PSD

- Rust reader(s): `src/formats/psd.rs`
- Java reader(s): `java-bioformats/components/formats-gpl/src/loci/formats/in/PSDReader.java`
- Fixtures: synthetic helper/resource tests, generated PSD-style RGB group
  fixture.
- Java version: local `java-bioformats` checkout.
- Audit status: `[x]` source-backed bounded parity pass after suffix fix.

### Checklist Summary

- Source mapping: `[x]` Java `PSDReader` maps to Rust `PsdReader`.
- Open semantics: `[x]` name-only detection now matches Java's registered
  `.psd` suffix; byte detection accepts `8BPS` like Java's stream probe.
- Parsing structure: `[x]` header, color mode data, image resources, layer/mask
  section heuristics, compression field, and PackBits row lengths follow the
  Java reader's structure for supported paths.
- Core metadata: `[x]` dimensions, channel count, bit depth, pixel type,
  big-endian flag, RGB/interleaving, indexed LUT, and single-plane behavior are
  source-audited and covered by focused tests.
- Plane mapping: `[x]` one logical merged-image plane.
- Pixel reads: `[x]` uncompressed and PackBits helper paths covered; region
  output is planar by channel like Java `openBytes`.
- Resolutions/pyramids: `[n/a]`.
- OME metadata: `[x]` generic single-image/channel OME plus Java-style basename
  image name; Java does not set channel names or wavelengths for PSD.
- Compressed/optimized paths: `[~]` Rust decodes/caches the merged plane and
  crops regions from it; focused region/layout tests cover the exposed bytes.
- Test evidence: `cargo test psd --lib`.

### Findings

- `[x]` Matches Java: PSD suffix/magic routing, core metadata, first RGB group
  plane semantics, PackBits validation, image-resource retention, and image
  basename.
- `[~]` Optimized but parity-proven: cached full-plane region cropping is
  covered for the decoded layout.
- `[!]` Needs fixing: none remaining from source audit.
- `[?]` Blocked/unverified: real PSD fixtures for layer/mask edge cases, palette
  LUTs, PackBits rows, and Java/Rust CRC comparison.

## Format: Vaa3D V3Draw Writer

- Rust reader(s): `src/formats/v3draw.rs` (writer only)
- Java reader(s): `java-bioformats/components/formats-bsd/src/loci/formats/out/V3DrawWriter.java`
- Fixtures: synthetic writer output tests.
- Java version: local `java-bioformats` checkout.
- Audit status: `[x]` writer parity pass.

### Checklist Summary

- Source mapping: `[x]` Rust `V3DrawWriter` maps to Java `V3DrawWriter`; there
  is no corresponding Java reader to audit.
- Open/write semantics: `[x]` `.v3draw` suffix, 24-byte magic, endian flag,
  datatype-size code, XYZC dimensions, and pixel offset match Java writer
  output.
- Plane mapping: `[x]` input planes are reordered to the writer's XYZCT storage
  order using Java-style ZCT/index helpers.
- Pixel writes: `[x]` UINT8/UINT16/endian and reordered multi-plane output are
  covered.
- Resolutions/pyramids: `[n/a]`.
- OME metadata: `[n/a]` writer only.
- Compressed/optimized paths: `[n/a]`.
- Test evidence: `cargo test v3draw`.

### Findings

- `[x]` Matches Java: writer header, datatype/endian encoding, output
  dimensions, and plane reordering.
- `[!]` Needs fixing: none found.
- `[?]` Blocked/unverified: Java writer binary-output comparison fixture would
  strengthen evidence.

## Format: OME-Zarr / OME-NGFF

- Rust reader(s): `src/formats/zarr.rs`
- Java reader(s): none in local `java-bioformats/components/*/src/loci/formats/in`.
- Fixtures: generated local root-array tests and optional OME-Zarr fixtures.
- Java version: local `java-bioformats` checkout.
- Audit status: `[x]` classified as intentional Rust-only support.

### Checklist Summary

- Source mapping: `[x]` no Java Bio-Formats reader exists in the local source;
  this is documented Rust-only functionality.
- Open semantics: `[x]` `.zarr`/`.ome.zarr` directory detection and group marker
  handling are covered by tests.
- Parsing structure: `[x]` OME-NGFF `multiscales`, `omero`, `plate`, and plain
  root-array fallback paths are represented in Rust tests.
- Core metadata: `[x]` levels map to Rust non-flattened `resolution_count()` /
  `set_resolution()` semantics.
- Plane mapping: `[x]` 5D `t,c,z,y,x` normalization maps planes to Z/C/T.
- Pixel reads: `[x]` local root-array read test covers core bytes; optional
  fixtures cover broader NGFF data when present.
- Resolutions/pyramids: `[x]` multiscales datasets are nested resolutions.
- OME metadata: `[x]` OME-NGFF channel rendering and HCS plate/well metadata are
  Rust-owned mappings, not Java parity claims.
- Compressed/optimized paths: `[~]` native zarrs chunk access is intentionally
  optimized and tested through public pixel reads.
- Test evidence: `cargo test zarr`.

### Findings

- `[x]` Matches Java: not applicable; no Java reader exists.
- `[~]` Optimized but parity-proven: Rust-only reader uses native zarrs access
  with tests for observable FormatReader behavior.
- `[!]` Needs fixing: none for Java parity.
- `[?]` Blocked/unverified: broader OME-NGFF fixture coverage for v3 stores,
  sharding, labels, and all compression/filter combinations.

## Format: Scoped FLIM2 / Leica / LMS / ND2 Parity Audit

- Rust reader(s): `src/formats/flim2.rs`, `src/formats/leica.rs`,
  `src/formats/leica_lms.rs`, `src/formats/nd2.rs`.
- Java reader(s): local `java-bioformats` checkout:
  `LeicaReader.java`, `ND2Reader.java`, `XLEFReader.java`, and
  `LeicaMicrosystemsMetadata/*.java`; plus the matching Java readers mirrored
  by `flim2.rs` reader implementations.
- Fixtures: synthetic focused unit tests plus existing optional external fixture
  hooks.
- Java version: local `java-bioformats` checkout.
- Audit status: `[!]` complete for the requested files, with remaining
  fixture-backed gaps recorded in `TOFIX.md`.

### Checklist Summary

- Dispatch/suffix/byte detection: `[x]` scoped dispatch gaps checked; Leica LEI
  `.raw` companions, Java XLEF/LMS suffixes, SlideBook7 native suffixes, OIR
  native magic, and ND2 byte detection are covered by focused tests.
- Metadata population: `[!]` current code projects the audited small/safe
  channel name, emission/excitation, physical size, instrument, detector,
  filter, plane, and OriginalMetadata fields where represented by the Rust OME
  model. Shared OME StageLabel support now covers Java-backed Leica LEI
  `CXYZStage` Z-position projection. Remaining source-backed Leica LEI and LMS graph/layout gaps are in
  `TOFIX.md`.
- OME channel count shorter than declared C: `[x]` ND2 fallback and LMS/XLEF
  channel-array guards were reviewed; focused tests cover shorter/fallback
  channel metadata paths.
- Non-flattened resolution semantics: `[x]` CellSens/VSI and NDPIS now expose
  pyramid levels through `resolution_count` / `set_resolution` instead of
  flattening Java subresolutions into unrelated series.
- Plane mapping and region reads: `[x]` ND2 split-channel logical planes,
  Leica raw companions, SlideBook TIFF grouped channel/time planes, XLEF JPEG
  mono conversion, and CellSens direct tile-region assembly are covered.
- Companion files/raw layouts: `[!]` bounded Leica raw companions are fixed;
  bounded LMS raw-storage leaves now cover explicit Z/C/T strides plus Java-style
  inferred channel stride from `ChannelDescription BytesInc`, dimension order
  derived from declared `BytesInc`, and one uncompressed memory node paired with
  one storage node; compressed memory, multi-storage, and colour/interleaved LMS
  layouts remain source-backed `TOFIX.md` work.
- Compression/optimized paths: `[~]` existing optimized Rust extraction paths
  were retained only where observable Java plane/region behavior is preserved;
  ND2 split-channel compressed direct extraction is explicitly disabled.
- Test evidence: `cargo test leica --lib`, `cargo test nd2 --lib`,
  `cargo test xlef --lib`, `cargo test cellsens --lib`,
  `cargo test slidebook --lib`, `cargo test imaris --lib`,
  `cargo test ndpis --lib`, `cargo test oir --lib`.

### Findings

- `[x]` `src/formats/nd2.rs`: audited ND2Reader parity gaps for binary
  ImageAttributes dimensions/bit depth, ImageMetadataLV scan offset, split
  channel plane mapping, per-plane OME mapping, and compressed-path gating are
  fixed and tested.
- `[!]` `src/formats/flim2.rs`: audited small/safe fixes are present for
  CellSens, NDPIS, XLEF JPEG mono/upscaling, Imaris TIFF, SlideBook TIFF,
  SlideBook7 bounded metadata/plane projection, and OIR paths. Current
  SlideBook7 synthetic coverage also includes typed channel/exposure,
  optovar/x-factor physical-size and objective-magnification projection,
  Java-style objective correction/immersion defaults, mask/annotation,
  elapsed-time, stage-position, aux-data, `.sldyz` extraction, gzip `.npyz`,
  Z/T-varying OME planes, Java's single-Z `PhysicalSizeZ` guard, and Java's
  single `Instrument:0` with one objective per native capture. Native
  image-data file-count handling now mirrors Java
  `CImageGroup.CountImageDataFiles()`: declared C/T is kept only when all
  declared files exist, TP0-as-multiple-T is used only for Java's
  one-file-per-channel single-Z case, and sparse mixed file sets derive C/T from
  observed filename indices instead of exposing phantom declared planes. Complete
  native-layout coverage and broader structured ROI/instrument parity still
  require real fixture evidence. Worker J rechecked the local Java
  `SlideBook7Reader.java` native discovery path and fixed one bounded
  source-backed divergence: Rust now mirrors
  `CSBFile70.GetListOfImageGroupTitles()` by skipping immediate `.imgdir`
  directories that have `ImageRecord.yaml` but no `.npy`/`.npyz` payload before
  parsing captures. Verified with
  `cargo test slidebook7_skips_imgdirs_without_image_data_like_java --lib`,
  `cargo test slidebook7 --lib`, `cargo test --test slidebook7_sldyz_test`, and
  `cargo test --test slidebook7_yaml_metadata_test`. Worker I/J rechecked
  registered Cargo targets,
  `tests/java_parity_test.rs`, checked-in fixture/test/parity trees, `.tmp`,
  `userdata`, `external-fixtures`, and mounted `/big/henriksson/ome_images`;
  no real `.sldy`/`.sldyz` project fixture or default Java parity input is
  present. Existing SlideBook 7 tests are generated Java-shaped synthetic
  layouts; the standalone `tests/slidebook7_sldyz_test.rs` and
  `tests/slidebook7_yaml_metadata_test.rs` files are explicit Cargo targets.
  The native bucket stays open until a real project can be compared against
  Java OME-XML and pixel CRCs.
- `[!]` `src/formats/leica.rs`: bounded LEI raw companion, TIFF companion
  `Series Name`/sibling `.lei` lookup, and structured OME projection paths are
  fixed, including objective serial number and objective settings refractive
  index after extending the shared OME model. This pass also adds Java
  `TIMEINFO` acquisition date and plane `DeltaT` projection, Java
  `nDelayTime`/`nDelayTime_ms` plane exposure projection, and OME image
  name/description projection from the series tag. The remaining source-backed
  `CScanActuator|Z Scan Actuator|Position` StageLabel Z projection is now
  covered with Java's meter-to-micrometer conversion, and Java metadata-only
  `EXPERIMENT`/`CHANDESC` tags are now retained as original metadata. A
  Leica-only follow-up source audit also added Java-style `Block N <contentID>`
  original metadata retention for non-empty instrument SAFEARRAY entries.
  Worker I rechecked registered Cargo targets, `tests/java_parity_test.rs`, checked-in
  fixtures, `.tmp`, `userdata`, `external-fixtures`, parity/bench inputs, and mounted
  `/big/henriksson/ome_images`; no real LEI dataset or Leica `.raw` companion
  dataset was present. The only `.raw` hits are unrelated NRRD/MHA payloads, so
  richer real LEI instrument graph parity remains to be proven against Java
  OME-XML/CRC fixtures.
- `[!]` `src/formats/leica_lms.rs`: channel resolution/bytes-inc retention for
  XLEF JPEG parity is fixed. `src/formats/flim2.rs` now also supports bounded
  LMS raw-storage channel strides inferred from `ChannelDescription BytesInc`
  plus one-storage-file raw leaves, one uncompressed memory node paired with one
  storage node, multi-frame delegate series, and Java `URLDecoder`-style `+` to
  space handling in Leica XML file references. LMS collection traversal now
  also mirrors Java's case-sensitive `.xlif`/`.xlcf` child suffix checks after
  `fileExists` path correction, and `src/formats/flim2.rs` now treats `.xlcf`
  as a recursive collection reference while preserving supported sibling
  leaves when some collection leaves are unsupported. LMS RGB metadata now also
  records Java-style `rgb_channel_count=3` when the X stride is divisible by 3,
  so logical `SizeC` stays separate from stored RGB samples in OME and parity
  comparisons. XLEF OME metadata now comes from the whole project metadata
  vector, so mounted multi-series LOF projects expose one OME image per Java
  series; LMS projection creates Java-style structural Instrument/Objective IDs,
  implicit RGB filter IDs, and no Rust-only OriginalMetadata map annotations.
  XLEF XLIF metadata now also overlays Java's LMS little-endian core flag onto
  PNG/JPEG raster delegates, records `rgb_channel_count=3` in the XLIF-backed
  metadata path, and creates Java-style implicit emission filter IDs for every
  effective RGB channel carrying a Leica channel colour. Worker F found mounted
  real XLEF/LOF fixtures and ran metadata-only Java parity against collection,
  format-test, and RGB channel LOF projects. A nine-project mounted XLEF smoke
  covering BMP, PNG, JPEG, TIFF, compressed TIFF, LOF, mixed JPEG/TIFF,
  collection, and RGB-channel LOF projects now passes core and OME parity for
  all 32 Java series with pixel reads disabled. Pixel-enabled Java parity now
  also passes for the mounted format-test BMP, PNG, JPEG, TIFF, compressed
  TIFF, and mixed JPEG/TIFF raster delegate projects; non-JPEG delegates are
  bitwise and JPEG delegates are within the harness' Java/Rust IDCT tolerance.
  A broader read-only pixel feasibility pass also found bitwise pixel parity
  for the mounted format-test LOF project, both TIF/LOF collection projects,
  XYCT 16-bit 2-channel time-series projects, `falcon_sample_data_small`, and
  Paramecium fixtures where pixel checks reached byte comparison. Thunder
  Organoid Tilescan grouping now follows Java `XlifDocument.getTileCount` /
  `MultipleImagesReader.swapDimensions`: TIF frame references are grouped into
  9 tile series in stored `Z,S,T,C` order, LOF delegates that already expose
  the 9 tile series are not multiplied again, and non-RGB channel-colour
  emission filters are created. Java parity now passes for both mounted
  Thunder TIF and LOF projects: 9 series each, all 18 core series matching,
  2/2 OME documents matching, and all 18 tile series passing bitwise pixel
  checks. The mounted XYCT LOF, falcon LOF, and Paramecium LOF OME divergences
  are now fixed: Rust preserves Java-visible explicit empty LMS channel names,
  creates Java-style active-detector placeholder IDs when detector index/model
  data is empty, and suppresses per-channel laser-derived excitation
  wavelengths for later XLEF project series where Java emits no OME excitation
  wavelength. Strict metadata-only Java parity now passes for those three
  mounted projects: 3 files, 6/6 core series matching, and 3/3 OME documents
  matching, with pixels intentionally skipped for the focused OME check.
  Broader LMS compressed-memory, multi-storage, colour/interleaved, and
  ordering layouts remain source-backed work, and larger Lysosomes, 2020,
  Snail, tau-selection, annotated-XYCZT, and annotated-stage XLEF projects
  still need bounded fixture-backed closure.
- `[!]` `src/formats/mias.rs`: CellWorx HTD wave names, well-log OME fields
  including WellSample scan-origin positions, MIAS template/result/detail ROI
  metadata, MIAS overlay channel colors, and MIAS/CellWorx SPW
  Plate/Well/WellSample graph projection are fixed and covered by focused
  tests. The shared OME model now represents Mask shapes and
  `MaskPixels.BinData`, and MIAS opt-in mask parsing covers Java-style indexed
  palette and uncompressed RGB overlays. The shared OME model also now
  represents image-level ROIRef links, and MIAS detail/mask ROIs populate those
  Java-style references. MIAS Plate.Name now follows Java's final
  directory-suffix overwrite after `parseTemplateFile`, while retaining
  template scalars in series metadata. CellWorx OME now emits populated Plane
  entries when no per-plane metadata already exists, matching Java
  `MetadataTools.populatePixels(..., true)`; mounted MetaXpress `idr0081`
  metadata-only parity now matches series/core metadata for 384 series and no
  longer has the OME plane-count gap. CellWorx now also templates Java-visible
  physical sizes and instrument metadata from the first real MetaMorph TIFF
  companion via the translated `MetamorphReader`, so UIC
  `XCalibration`/`YCalibration` values win over generic TIFF `72 dpi`
  resolution metadata; mounted MetaXpress `idr0081` metadata-only parity now
  also matches OME `PhysicalSizeX/Y=1.72` and the single instrument/detector
  graph. Remaining work is fixture-backed Java/Rust OME/pixel matrices for the
  MIAS template/results/ROI/overlay/mask and richer CellWorx scan-log cases.

## Current Audit Status

- Inventory status: every file-format inventory entry has been checked. There
  are no remaining `[ ]` file-format rows in the top-level inventory.
- Remaining `[!]` entries with open `TOFIX.md` work: `flim2.rs`, `leica.rs`,
  `leica_lms.rs`, `mias.rs`, and `zeiss_lsm.rs`;
  the checked-in LSM and ZVI fixtures now pass Java core, OME, and pixel
  parity. Zeiss LSM now also has source-backed MDB byte-sniff parity,
  channel-colours LUT/color
  parsing and Java-style OME plane DeltaT/PositionX/Y/Z projection from
  CZ_LSMINFO timestamp/position tables plus rotation/illumination/phase Modulo
  annotations. Recent bounded LSM passes add single-file DimensionM/DimensionP
  expansion with Java-style logical series, per-series IFD offsets, image-name
  suffixes, series-local OME plane timing/positions, Java scan-info
  acquired-before-non-acquired OME graph population, and application-tag
  `SimOut*`/`SimPar*` handling for Java's SIM channel-colour fallback. LSM
  remains `[!]` only for unproven MDB/multi-LSM datasets and richer
  ROI/full-subblock fixtures. Worker
  E checked `testdata`, `tests`, `userdata`, `external-fixtures`, `.tmp`, and
  Java parity harness inputs; the sole local LSM fixture is
  `testdata/lsm/colocsample1b.lsm`, which has inert Java InputLut/OutputLut
  overlay offsets but no ROI overlay, application-tag, event-list, channel-
  wavelength, `.mdb`, or multi-`.lsm` fixture evidence. The current common OME
  model also lacks Java's `ExperimenterUserName` field, so scan-info `User`
  can be retained by the LSM parser but not projected 1:1 to OME yet;
  ZVI residual ROI text/styling/tag-display fields are tracked as current Rust
  OME model limitations rather than open reader fixes.
- MIAS mask ROI projection now covers both indexed-palette overlays and the
  Java non-indexed RGB channel-splitting path for uncompressed 8-bit overlay
  TIFFs. CellWorx scan-origin WellSample PositionX/PositionY projection is now
  source-backed against Java `parseWellLogFile`; shared OME image ROIRef
  support plus MIAS detail/mask ROIRef projection now covers Java
  `setImageROIRef`. The remaining MIAS `[!]` status is for fixture-backed
  HCS/results/overlay comparison coverage.
- 2026-08-13 MIAS/CellWorx focused follow-up checked current Rust against
  vendored Java `MIASReader.java` and `CellWorxReader.java`. No extra bounded
  source-backed fix was provable with the local fixture set
  (`testdata/metaxpress/BSF018292-1A.HTD` plus one `A01_w1` TIFF only). The
  residual MIAS/CellWorx bucket now specifically needs companion-rich MIAS
  template/results/detail/overlay/mask fixtures and richer CellWorx
  multi-wavelength/scan-log datasets to prove or close Java
  `convertChannels`, plate-acquisition timing/field-count, and overlay/mask
  edge cases.
- The aggregate scoped FLIM2/Leica/LMS TOFIX entry is closed; the individual
  SlideBook7, Leica LEI, and Leica XLEF/LMS entries remain open for the
  fixture-backed gaps listed above.
- Completed/fixed during this wave: TopoMetrix byte-probe parity, IPLab suffix
  parity, MIAS Software-tag byte detection, Fake structured OME projection,
  Cellomics MDB BGR color projection, TIFF/SVS Nikon marker scoping, LI-FLIM
  OME image/plane/ROI projection, and bounded LMS raw-storage channel-stride
  inference.
  TIFF/SVS Nikon marker scoping, and LI-FLIM OME image/plane/ROI projection.
- Verification baseline after integration: run `cargo test --lib` after any
  additional edits to shared TIFF, OME, registry, or wrapper behavior.

### Format: Tecan Spark Cyto

- Rust reader(s): `src/formats/gpl/tecan.rs` (`TecanReader`; replaces the former non-Java `.asc` reader in `hcs2.rs`).
- Java reader(s): `java-bioformats/components/formats-gpl/src/loci/formats/in/TecanReader.java`.
- Audit status: line-by-line port; Java parity verified on a real Spark Cyto kinetic plate (12 wells, 3 raw channels, 10 cycles, 2456x2052 uint16).
- Checklist summary: source mapping `[x]` (method order mirrors Java; adapters only for JDBC, MinimalTiffReader, XMLTools/DOM, Zip, DateTools, FormatTools, addMeta/flatten); open semantics `[x]` (`.db` suffix plus SQLite/PlateDefinition probe; reached in Java order once SPE/AFI byte probes are gated by `suffix_necessary`); parsing/core metadata/plane mapping/pixel reads `[x]`; resolutions `[n/a]`; OME `[~]` (Plate/Well/WellSample, image names, channel names, physical sizes, acquisition date, plane exposure match Java; PlateAcquisition not modelled in Rust OME).
- Test evidence: Java parity harness via `BIOFORMATS_RS_JAVA_PARITY_EXTRA_FILES` (core/OME/pixels all pass); ad hoc Java-vs-Rust metadata dump identical; synthetic SQLite+TIFF unit tests `cargo test --release --lib tecan`. Remaining fixture gap: no multi-field or ObjectList (SVG result) dataset checked against Java.


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
