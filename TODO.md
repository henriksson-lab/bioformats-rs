# TODO - broad translation audit

A broad Java→Rust audit on 2026-05-26 produced ~76 findings across 8 format
slices. Several parallel fix passes the same day (partitioned by file, porting
from the read-only Java reference in `java-bioformats/`) closed essentially all
of them — the original P0/P1/P2 findings, the two long-standing pre-existing
test failures, and then a dedicated **parity pass** that promoted ~30 Partial
readers to Complete (see README "Translation status").

State after the parity pass:

- `cargo build` / `cargo build --bins`: clean.
- `cargo test`: **all green** — lib 185, format_tests 103, write_test 13,
  integration_test 24, new_features_test 24, external_fixtures_test 20.
- `cargo clippy --all-targets`: no errors (warnings only).
- `git diff --check`: clean.

Per-reader completeness is tracked in the README "Translation status" table
(~95 complete / ~54 partial / 36 stub). Synthetic fixtures/tests that encoded
old buggy behavior were updated to Java-correct expectations as part of the fixes.

Priorities: P0 = wrong pixels/panic/loop/misleading API; P1 = parity gap /
malformed output; P2 = lower-risk compatibility/coverage/robustness.

## Completed

All P0/P1/P2 findings from the 2026-05-26 audit are fixed. First-pass items
(TIFF ALT_JPEG, FillOrder; CZI/ZVI/LIM/IMAGIC/ARF/Bio-Rad GEL rewritten
readers; MicroManager/LEI/Olympus/Prairie/Visitech; ECAT7/Inveon/Varian/FITS/
MRC/NIfTI; ChannelMerger/Separator/MinMax/AxisGuesser; DeltaVision/Gatan/ICS;
Imaris/InCell/PerkinElmer; SVS pyramid; PCX/PSD/SPE/Amira/SDT/BDV/CellH5;
OME-XML; DICOM/NRRD/LSM/CZI-104/MetaMorph/EPS robustness) are recorded in git
history.

Second-pass items (the former partials and pre-existing failures), now done:

- [x] **TIFF sub-byte / packed-row partial reads + tiled YCbCr** — ported
  `TiffParser.unpackBytes`/`getTile` (packed planar <8-bit, partial-column
  non-byte-aligned, partial-row + tiled subsampled YCbCr) — `src/tiff/reader.rs`.
- [x] **ThunderScan** — decoder was already correct per libtiff `tif_thunder.c`;
  the test's expected bytes were fabricated and have been corrected with a
  step-by-step trace — `src/tiff/compression.rs`.
- [x] **AVI Microsoft Video 1 (MSVC/CRAM) decode** — 4x4 block codec (skip/
  solid/2-color/8-color, 8-bit palettized + 16-bit RGB555) in
  `src/common/codec.rs`, wired into `src/formats/avi.rs` with unit tests.
- [x] **DICOM encapsulated RLE + JPEG baseline** — segmented RLE decode and
  transfer-syntax dispatch (`classify_transfer_syntax`) + JPEG marker fix-ups —
  `src/formats/dicom.rs` with unit tests.
- [x] **Flex well/field series + `.mea`/`.res` companions** — one OME series per
  (plate, well, field), `lookupFile`/`rasterToPosition` mapping, HCS plate
  metadata — `src/formats/flex.rs` with unit tests.
- [x] **Olympus OIF `.pty` indexing + `.oib` OLE2 variant** — `.pty`-driven TIFF
  resolution and axis order; `.oib` opened via the `cfb` crate
  (`OibInfo.txt`/`mapOIBFiles`) — `src/formats/olympus.rs` with unit tests.
- [x] **Bio-Rad PIC multi-file grouping** — notes-driven sizeC/sizeZ/sizeT,
  `FilePattern` sibling enumeration, Java plane→file mapping
  (`file = no % nFiles`) — `src/formats/biorad.rs` with unit tests.
- [x] **BMP BITFIELDS masks** — per-component shift/scale for 16/32-bit
  BITFIELDS (alpha-aware) — `src/formats/bmp.rs` with unit tests.
- [x] **AxisGuesser Z/T-reorder** — ported the full constructor (segment
  matching + Z/T swap + size-aware back-fill); per-file sizes/order threaded
  from the reader — `src/stitcher.rs` with unit tests.
- [x] **NRRD axis derivation** — replaced the heuristic with Java's positional
  rule (first axis with 1<size≤16 → channel; XYCZT order) — `src/formats/nrrd.rs`.
- [x] **MIAS auto-detection for plain `.tif`** — `MiasReader` is wired into the
  pre-magic `tiff_wrapper_readers_for_extension` hook, gated on a `Well<xxxx>`
  parent dir so ordinary `.tif` files fall through to `TiffReader`; `set_id`
  hardened to reject non-MIAS TIFFs — `src/registry.rs`, `src/formats/mias.rs`.
- [x] **XRM stub test** — XRM is a real CFB reader now (rejects fake input with a
  genuine CFB error), removed from the not-implemented stub list — `src/registry.rs`.
- [x] **EPS stub test** — EPS now rasterizes inline PostScript + reads embedded
  TIFF previews; stub-test expectation updated to its genuine rejection message —
  `src/registry.rs`.
- [x] **Bio-Rad GEL short-file guard** — returns a clean
  `UnsupportedFormat("Bio-Rad GEL file is too short")` instead of leaking an Io
  EOF on truncated input — `src/formats/camera2.rs`.

## Remaining open

### Blocked on external fixtures (not implementable in code)
- [ ] **Modern ND2 ImageDataSeq metadata/JPEG2000 coverage.** `src/formats/nd2.rs`
  decodes raw/zlib/JPEG2000/framed payloads and the regression set passes for the
  available fixtures; the remaining gates (per-plane `ImageDataSeq|N`
  metadata/attributes, and a known-good JPEG2000-compressed modern frame) are
  blocked on discovering a suitable public fixture. Re-run
  `python3 external-fixtures/scripts/audit_nd2_fixtures.py` with
  `--require-chunked-metadata-candidate` / `--require-chunked-jpeg2000-candidate`
  when new candidates appear. (Full prior fixture-rejection log is in git history
  of this file.)

### Done in the latest parity round (now Complete in the README status table)
- [x] **Prairie** stage-position → multi-series split. `src/formats/prairie.rs`.
- [x] **MetaMorph** multi-STK `.nd` file-group series. `src/formats/metamorph.rs`.
- [x] **HCS ScanR** sparse-well compaction, **CellVoyager** tile stitching,
  **BD Pathway** montage field split. `src/formats/hcs2.rs`.
- [x] **RHK SPM** real binary page header (XPM/text) ported. `src/formats/spm.rs`.
- [x] **MINC-1** classic NetCDF-3 via a pure-Rust parser (MINC2/HDF5 already done).
  `src/formats/misc.rs`.
- [x] **AVI Cinepak ("cvid")** decoder implemented + wired in. `src/formats/avi.rs`,
  `src/common/codec.rs`.
- [x] **NRRD bzip2** — confirmed MATCHES-JAVA (Java NRRDReader supports only
  raw/gzip and rejects bzip2); the current rejection is already at parity.
- [x] **CZI** rotation (R) → moduloZ annotation + PALM plane splitting. Scene/
  acquisition/angle series, mosaic stitching, per-pixel-type split already done.
  `src/formats/czi.rs`.
- [x] **cellSens VSI** PNG/BMP `.ets` tile codecs + VSI tag-tree (exact full-res
  dims, dimension order, tile-origin crop). `src/formats/flim2.rs`, `src/common/codec.rs`.
- [x] **Canon RAW / Minolta MRW / DNG (CFA)** Bayer interpolation + sub-byte bit
  unpacking (faithful port of `ImageTools.interpolate`/`DataTools.unpackBytes`).
  `src/formats/camera2.rs`, `src/formats/extended.rs`.
- [x] **CZI** mosaic image-fusion series rebalancing (plane-count-driven
  seriesCount collapse, ZeissCZIReader:941-1003) — ported additively for the
  non-pyramid case without touching the R-as-pyramid model. `src/formats/czi.rs`.
- [x] **cellSens VSI** orphan-ETS file matching + dimension collision-shift
  heuristics + (most) non-geometry metadata. `src/formats/flim2.rs`.
- [x] **DNG EXIF white-balance** — added additive EXIF (34665) + Canon maker-note
  (37500) IFD parsing on `TiffReader` and applied the coefficients.
  `src/tiff/{reader,ifd}.rs`, `src/formats/extended.rs`.
- [x] **cellSens VSI** prefix-gated value metadata (per-channel wavelengths, Z
  start/increment/value, timestamps, exposure-by-prefix) via recursive tag-name
  prefix tracking — VSI is now complete. `src/formats/flim2.rs`.

### Won't implement (decided) / blocked
- [ ] **`.mdb` companion metadata** (Cellomics, APL, ZeissLSM) — Bio-Formats reads
  it via the pure-Java `mdbtools.libmdb`, whose source is NOT in this checkout. A
  from-scratch pure-Rust Jet/MDB reader was declined (no reference to translate,
  untestable without a fixture). Pixels decode without it; metadata stays absent,
  mirroring Java's graceful degradation when MDBService is unavailable.
- [ ] **Imaris HDF5** true single-plane hyperslab reads — `hdf5-pure` exposes only
  whole-dataset reads (currently cached, correct but memory-heavy). Needs hyperslab
  support in `hdf5-pure` or a C-linked HDF5 crate (system dep — against the
  pure-Rust/container-portability stance). `src/formats/imaris.rs`.

### Blocked on a test fixture / has no Java reference
- [ ] **DICOM JPEG-lossless (process 14)** — routed through the shared JPEG
  decoder (depends on `jpeg_decoder` SOF3 support); no bundled fixture to confirm.
- [ ] **Modern ND2 ImageDataSeq** — blocked on discovering a public fixture (see above).
- [ ] **DCIMG / Norpix / SimFCS / BigDataViewer / TopoMetrix** — no Java reference
  to be faithful to; current behavior is best-effort per spec.

### Matches Java by design (no action)
- DICOM **Deflate** transfer syntax returns `UnsupportedFormat` — Java
  `DicomReader` also throws `UnsupportedCompressionException` for it.
- TIFF **floating-point predictor 3** returns `UnsupportedFormat` — Java
  `TiffCompression.undifference` also rejects predictor 3.
- **FITS** (primary HDU, big-endian, no BZERO/BSCALE), **NRRD** (no bzip2),
  **ECAT7** (data_type 6 only), **LIM** (rejects compressed): all match the
  behavior of the corresponding Java reader.
- Flex **fields-stored-within-a-single-file** rare layout is not reconstructed;
  the common one-field-per-file layout (what Java exercises in practice) is.

## Continuous audit commands

Use these after substantive translation changes:

```bash
cargo test
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
