# TODO - broad translation audit

This backlog was rebuilt from a broad code audit on 2026-05-24, with parallel
review slices for TIFF, vendor formats, and API/wrapper/writer behavior.
It was triaged again on 2026-05-25 to deprecate or merge stale older backlog
items after recent TIFF, OME, CZI, AVI, DICOM, ND2, and LSM fixes.

Priorities:

- P0: likely wrong pixels, panics, infinite loops, or misleading public API.
- P1: important format parity gaps or malformed output risks.
- P2: lower-risk compatibility, coverage, documentation, and long-term parity.

## P1 - format parity and output integrity

- [ ] **Blocked: discover real modern ND2 ImageDataSeq metadata/JPEG2000 fixtures.**
  `src/formats/nd2.rs` no longer strips `data.len() - expected` blindly: it
  now decodes exact raw payloads, explicit 8-byte/4096-byte framed payloads,
  zlib streams, JPEG2000 signatures, XML `value="..."` attributes, cropped
  sensor rectangles from grabber-camera metadata, non-contiguous chunk maps,
  and gapped chunk scans; it rejects unknown oversized frame encodings. The
  current external ND2 regression set opens those files, asserts parsed
  dimensions/channel count/bit depth/chunk counts, and reads every reported
  plane with dimensions/byte counts matching Java Bio-Formats for `BF007.nd2`,
  `Exception_2.nd2`, `MeOh_high_fluo_003.nd2`, and `header_test2.nd2`.
  The remaining open scope is now only real modern chunked `ImageDataSeq`
  coverage. Local fixture audit on 2026-05-26, after downloading only the
  bounded `nd2-modern-uicomp-feature` candidate in addition to the previously
  tried `nd2-zenodo-vpa002-smoke` candidate, closes the modern chunked
  multi-component subcase: `100217_OD122_001.nd2` is a modern
  `ImageDataSeq` file with `uiComp=2`, 725 raw image chunks, asserted
  two-channel metadata, and sampled decoded plane hashes. The remaining
  open subcases are now only per-plane metadata/attributes varying by
  `ImageDataSeq|N` with asserted OME/original metadata values, and a valid
  JPEG2000-compressed modern ND2 frame whose decoded bytes are known.
  This is blocked on external fixture discovery, not on known local
  implementation work: the workspace contains no stable direct fixture that
  satisfies either remaining gate.
  The local no-download set contains `BF007.nd2`,
  `Exception_2.nd2`, `MeOh_high_fluo_003.nd2`, `MeOh_high_fluo_007.nd2`,
  `MeOh_high_fluo_011.nd2`, `header_test2.nd2`, `b16_14_12.nd2`, and
  `but3_cont200-1.nd2`; the downloaded VPA002 candidate is also a modern
  single-image raw `ImageDataSeq` file with no detected `uiComp`, metadata
  sequence index beyond `0`, attribute sequence indexes, or JPEG2000
  `ImageDataSeq` payloads. `Exception_2.nd2` covers zlib frames; `BF007.nd2`,
  `MeOh_high_fluo_003.nd2`, `header_test2.nd2`, VPA002, and
  `100217_OD122_001.nd2` cover raw frames.

  Re-run the candidate audit with
  `python3 external-fixtures/scripts/audit_nd2_fixtures.py`; the helper follows
  ND2 chunk maps, falls back to gapped sequential scanning, and also scans
  old ND box text metadata for per-frame/component evidence while reporting
  file-header, old-ND-box footer classification, explicit candidate reasons,
  and separate `chunked_candidate_reasons` for the remaining modern
  `ImageDataSeq` gaps. It also has separate executable gates for each modern
  subcase:
  `--require-chunked-metadata-candidate`,
  `--require-chunked-uicomp-candidate`, and
  `--require-chunked-jpeg2000-candidate`. The `--require-chunked-uicomp-candidate`
  gate now passes with `100217_OD122_001.nd2`; the metadata and JPEG2000 gates
  still exit non-zero for the local fixture set including VPA002 and
  `100217_OD122_001.nd2`, which is the expected blocked status until a new
  external fixture is found.
  Rejected fixture evidence already checked on 2026-05-26:
  - Old JP2/old-ND-box fixtures `b16_14_12.nd2` and `but3_cont200-1.nd2`
    close only the old-format JP2 subcase; do not reuse them for modern
    `ImageDataSeq` metadata/JPEG2000 gates.
  - The discovered OME ND2 set, including `100217_OD122_001.nd2`,
    `control003.nd2`, `rfp_h2a_cells_02.nd2`,
    `cag_p5_simgc_2511_70ms22s_crop.nd2`, and other listed OME samples, has
    no closable remaining modern metadata/JPEG2000 candidate: probed modern
    files reported only metadata sequence `0`, no `ImageAttributesSeq|N`, and
    raw/other sampled image payloads.
  - Public Zenodo/Figshare probes rejected records `10.5281/zenodo.15493140`,
    `10.5281/zenodo.5277605`, `10.5281/zenodo.16921650`,
    `10.5281/zenodo.15077205`, `10.5281/zenodo.14719388`,
    `10.5281/zenodo.7573480`, `10.6084/m9.figshare.23798451.v1`,
    `10.5281/zenodo.7738355`, `10.5281/zenodo.19591867`,
    `10.5281/zenodo.10277961`, `10.5281/zenodo.14590332`,
    `10.5281/zenodo.5772393`, `10.5061/dryad.x95x69pmq`,
    `10.5281/zenodo.14231228`, `10.5281/zenodo.7572182`, and
    `10.5281/zenodo.7572656` for the same reason.
  - The tlambert Dropbox ND2 archive remains unsuitable for manifest promotion
    because it lacks stable per-file URLs. Range/targeted extraction found only
    raw or zlib modern `ImageDataSeq` files with metadata sequence `0` and no
    `ImageAttributesSeq|N`; `karl_sample_image.nd2` has useful future
    `CustomData` timing/stage evidence, but that is outside this TODO unless
    the scope is broadened.

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
