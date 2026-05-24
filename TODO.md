# TODO - broad translation audit

This backlog was rebuilt from a broad code audit on 2026-05-24, with parallel
review slices for TIFF, vendor formats, and API/wrapper/writer behavior.
It was triaged again on 2026-05-24 to deprecate or merge stale older backlog
items after recent TIFF, OME, CZI, AVI, DICOM, ND2, and LSM fixes.

Priorities:

- P0: likely wrong pixels, panics, infinite loops, or misleading public API.
- P1: important format parity gaps or malformed output risks.
- P2: lower-risk compatibility, coverage, documentation, and long-term parity.

## P1 - format parity and output integrity

- [ ] **Fix fixture-backed reader failures from the 2026-05-24 smoke audit.**
  A bounded external smoke set was downloaded with
  `python3 external-fixtures/scripts/download_ome_samples.py --set ome-tiff-smoke --set png-smoke --set amiramesh-smoke --set gatan-smoke --set ecat7-smoke --set mrc-smoke --set czi-smoke --set dicom-smoke --set nrrd-smoke --set cellomics-smoke --set sdt-smoke --max-bytes 60000000 --max-total-bytes 350000000`.
  Passing formats are covered by
  `tests/external_fixtures_test.rs::external_multi_format_smoke_set_opens_and_reads_first_plane`.
  Remaining failures to translate/fix are:
  the NRRD sidecar manifest/download changes need to be exercised by
  downloading the detached raw sidecars before NRRD can be promoted to passing
  external smoke coverage.
  Completed from this item: CZI smoke files open/read all 63 reported planes
  as `672x512`; DICOM/J2KI smoke files open/read JPEG 2000 first planes;
  SDT opens/reads the first `512x512` FLIM plane from the 8192-plane smoke
  file; Cellomics `.DIB` opens/reads as `512x512`, one 16-bit plane; Gatan
  `.dm4` opens/reads as `1024x1024`, one 32-bit plane; and ECAT7 now matches
  Java for the small fixture at `512x512`, 10 planes.

- [ ] **Add real CZI mosaic/pyramid fixture coverage.**
  `src/formats/czi.rs` now assembles synthetic multi-subblock mosaic planes,
  supports cropped regions via assembled planes, and selects synthetic pyramid
  levels keyed by the CZI `R` dimension. Remaining work is validating those
  paths against real Zeiss CZI fixtures, including any pyramid files that do
  not expose resolution levels through `R`. A fixture-backed audit on
  2026-05-24 downloaded `czi-smoke`; all three real smoke files now open and
  read every reported plane with Java-matching dimensions and byte counts.
  Add real fixtures with known expected pixels/metadata before replacing the
  synthetic-only coverage:
  `czi_mosaic_tiles.czi` should contain multiple subblocks for one logical
  plane with non-zero X/Y tile offsets and expected full-plane plus cropped
  region bytes, and `czi_pyramid_r_levels.czi`/`czi_pyramid_non_r_levels.czi`
  should expose at least two resolution levels with asserted dimensions and
  pixels, including one pyramid encoding that does not advertise levels solely
  through the `R` dimension.

- [ ] **Add real ND2 frame metadata/component fixtures.**
  `src/formats/nd2.rs` no longer strips `data.len() - expected` blindly: it
  now decodes exact raw payloads, explicit 8-byte/4096-byte framed payloads,
  zlib streams, JPEG2000 signatures, XML `value="..."` attributes, cropped
  sensor rectangles from grabber-camera metadata, non-contiguous chunk maps,
  and gapped chunk scans; it rejects unknown oversized frame encodings. The
  current external ND2 regression set opens and reads the first plane with
  dimensions/byte counts matching Java Bio-Formats for `BF007.nd2`,
  `Exception_2.nd2`, `MeOh_high_fluo_003.nd2`, and `header_test2.nd2`.
  To complete this item, add real ND2 fixtures covering all of:
  per-plane metadata/attributes varying by `ImageDataSeq|N` with asserted OME
  metadata/original metadata values, multi-component layouts where `uiComp > 1`
  and channel/interleaving can be asserted from pixels, and a valid
  JPEG2000-compressed ND2 frame whose decoded bytes are known.

- [ ] **Add real MRC/CCP4/IMOD orientation fixture coverage.**
  `src/formats/mrc.rs` now has synthetic unit coverage for the row-order
  heuristics: legacy lower-left-origin row flipping, no flip when `MAPR` is not
  the Y axis, and no flip when Y origin/start metadata indicates the first
  stored row is already the top edge. A broad local fixture audit on 2026-05-24
  found no real `.mrc`, `.mrcs`, `.ccp4`, `.map`, or `.rec` fixtures in this
  checkout outside build outputs, and no matching members/files in
  `bioformats_package.jar` or the checked-out
  `java-bioformats/{artifacts,components,jar}` tree. Public OME samples are now
  tracked by `external-fixtures/manifests/ome_sample_roots.tsv` and can be
  fetched into ignored local storage with
  `python3 external-fixtures/scripts/download_ome_samples.py --category mrc`.
  To complete this item, add real fixtures with known expected pixels/metadata
  for all of:
  a legacy lower-left-origin MRC requiring a row flip, a top-origin or top-start
  MRC that must not flip, a CCP4/MRC2014 file with non-default `MAPC/MAPR/MAPS`
  to assert logical X/Y/Z dimensions and physical-size axis remapping, and an
  IMOD `.rec` fixture that exercises IMOD mode-0 signedness plus row-order
  metadata.

## P2 - compatibility, coverage, and long-term parity

- [ ] **Implement remaining codec stubs.**
  JPEG2000, optional JPEG-XR, and MSRLE RLE8 support exist. A local-only audit
  on 2026-05-24 found no existing Rust dependency or local fixture that can
  safely validate the remaining stubs in `src/common/codec.rs`: CCITT Group 3,
  CCITT Group 4, Nikon NEF, Motion JPEG-B, QuickTime RLE, Apple RPZA, LZO, and
  standalone Huffman. `Cargo.toml`/`Cargo.lock` currently provide JPEG, JPEG
  2000, optional JPEG-XR, zstd, deflate, and LZW/PackBits support, but no fax,
  Nikon, LZO, QuickTime RLE, RPZA, or standalone Huffman decoder. The only
  remaining stubs reached through registered TIFF decompression dispatch
  (`src/tiff/compression.rs`) are CCITT Group 3/4 and Nikon compression code
  34713; `tests/`, `test/`, and the checked-out `java-bioformats/` tree contain
  no CCITT fax TIFF, Nikon NEF/NIS compressed TIFF, or known-pixel codec
  conformance fixture. The checked-out Java `formats-bsd` codec classes for
  MJPB, QTRLE, RPZA, LZO, Huffman, and MSRLE are adapters around `ome.codecs.*`;
  the referenced implementation sources are not present locally, so they cannot
  be ported or validated from this checkout alone. To complete this item, add
  one of: a CCITT Group 3/4 decoder dependency plus small TIFF or strip-level
  fixtures with known decoded 1-bit pixels and FillOrder/T4/T6 option coverage;
  a Nikon NEF compressed TIFF fixture with known raw pixels and enough metadata
  to validate bpp/predictor behavior; the missing `ome-codecs` implementation
  sources plus independent known-output fixtures; or dedicated spec-backed
  fixtures for the non-TIFF video/LZO/Huffman stubs before wiring them into
  readers.

## Deprecated or merged audit items

These items came from older audit phrasing and should not be tracked as
separate active tasks anymore.

- [x] **Improve metadata parity for partially parsed vendor formats.**
  Completed as a standalone item. IPLab now preserves parsed header fields plus
  common post-pixel original metadata tags (`note`, `head`, and indexed `clut`);
  ZVI now preserves per-plane tag IDs plus known tag names/values; CellH5
  preserves HDF5 attributes/dataset summaries; DICOM has targeted
  dictionary-name/value decoding for common public tags; BDV preserves
  companion XML original metadata; and Metamorph preserves UIC4 raw string
  metadata plus simple key/value entries.

- [x] **Generic "broaden wrapper and writer tests beyond happy paths".**
  Deprecated as a standalone item. Its concrete coverage requirements are now
  folded into active tasks for wrapper OME metadata consistency,
  `ChannelMerger` indexing, `DimensionSwapper` bounds, min/max endianness,
  writer plane validation, and region validation.

- [x] **Standalone "surface structural TIFF tag decode failures".**
  Merged into active TIFF parser hardening and required strip/tile tag
  validation. Structural tag failures should be handled while adding checked
  IFD arithmetic, visited-offset detection, and explicit required-tag errors.

- [x] **Broad "OME-TIFF `TiffData` mapping is missing".**
  Deprecated because embedded OME-TIFF `TiffData` logical-plane mapping now
  exists. Remaining active work is namespace-aware XML parsing and stronger
  tokenizer compatibility.

- [x] **Broad "YCbCr returns raw bytes".**
  Deprecated because 8-bit chunky non-JPEG TIFF YCbCr now decodes to planar RGB.
  Remaining active work is unsupported tiled/JPEG/subsample edge coverage and
  compression/predictor test expansion.

- [x] **Broad "CZI ZSTD_1 unsupported".**
  Deprecated because CZI ZSTD_1 wrapper decoding is implemented. Remaining CZI
  active work is logical-channel accounting, mosaic/tile assembly, pyramids,
  and richer metadata.

- [x] **Broad "AVI row padding/BGR conversion missing".**
  Deprecated because uncompressed DIB row padding, bottom-up order, and BGR/RGB
  conversion have targeted coverage. Remaining AVI active work is RIFF/index
  parsing and compressed-stream rejection.

- [x] **Broad "DICOM files without preamble are unsupported".**
  Deprecated because raw no-preamble explicit VR little-endian datasets are now
  supported. Remaining DICOM active work is implicit VR fallback, transfer
  syntax coverage, photometric handling, and pixel-data validation.

- [x] **Broad "ND2 chunk map ignored".**
  Deprecated because the EOF chunk map is now parsed. Remaining ND2 active work
  is structured per-frame decoding instead of prefix stripping.

- [x] **Broad "audit synthetic-pixel registered readers".**
  Completed for the remaining AFM/SPM/SEM readers on 2026-05-24. Header-backed
  readers now require declared dimensions and sufficient payload bytes before
  exposing metadata, region reads crop real decoded planes, and heuristic-only
  raw binary extensions return descriptive `UnsupportedFormat` errors instead
  of fake metadata or zero-filled pixels.

## Continuous audit commands

Use these after substantive translation changes:

```bash
HDF5_DIR=/tmp/bioformats-hdf5-root cargo test
cargo clippy --all-targets --all-features
git diff --check
```

For Java parity checks when `java-bioformats/` and `ccc-rs` are available:

```bash
ccc-rs analyze src                        -l rust --recurse -o rust.json
ccc-rs analyze java-bioformats/components -l java --recurse -o java.json
ccc-rs compare rust.json java.json --mapping ccc_mapping.toml --top 30
ccc-rs missing rust.json java.json --mapping ccc_mapping.toml --stub-loc-ratio 0.2
ccc-rs constants-diff rust.json java.json --mapping ccc_mapping.toml
```
