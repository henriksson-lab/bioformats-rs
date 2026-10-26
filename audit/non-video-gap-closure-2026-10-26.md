# Non-video translation gap closure — 2026-10-26

Reference: Bio-Formats `d54bf462e3cbe07715c6002eb614e2ced2278189`.
The upstream update and identified non-video gaps were handled together. This
record describes the scope of the work; the active ledger is [TOFIX](../TOFIX.md).

## Changes and evidence

- [Upstream translation](upstream-2026-10-26.md): all 18 first-parent updates
  classified and translated; orientation/calibration, CellSens, CV7000,
  Operetta and medical reader changes are covered by focused regressions.
  Final `updaterust status --only bioformats --no-fetch --all` reports both
  Bio-Formats and ZarrReader at zero untranslated commits / zero LOC.
- [Pyramids and CZI](gap-pyramids-2026-10-26.md): default flattened series and
  explicit nested resolutions; per-image OME pixels/calibration; multipart CZI
  routing, embedded images/timestamps and detailed optical metadata.
- [LSM](gap-lsm-2026-10-26.md): MDB grouping, source-file routing, scan metadata,
  overlays and all-series instrument associations.
- [Leica](gap-leica-2026-10-26.md): LEI block ordering, detector/channel mapping,
  objective properties and LMS/XLEF optical metadata.
- [SlideBook 7](gap-slidebook-2026-10-26.md): native C/Z/T plane order and metadata
  projection, plus a reproducible Java-readable synthetic project generator.
- [Shared OME XML](gap-ome-2026-10-26.md): heterogeneous image cores, schema child
  ordering, scoped child parsing, optical/HCS graph roundtrips, modulo XML
  annotations and explicit metadata-only versus TIFF storage mapping.
- FilePattern and FileStitcher retain requested resolution mode through probes
  and lazy child reopens. Two FilePattern and 37 stitcher tests pass; the latter
  also cover resolution-specific stitched plane counts.
- [Thumbnail conversion](gap-thumbnails-2026-10-26.md) replaces missing/cropping paths with Java-compatible
  area averaging and sample/color-model conversion. Java oracle sources and
  golden regeneration instructions are retained in
  `tests/fixtures/thumbnail/`, including indexed-palette dithering checks.

## Final combined verification

- `BIOFORMATS_RS_OME_SCHEMA_JAR=.tmp/upstream-bioformats-2026-10-26/bioformats-updated.jar cargo test --offline`:
  **1,512 passed**, including **1,339 library tests**, across 14 test targets;
  two opt-in gates ignored. Log: `.tmp/non-video-final-tests.log`.
- `cargo check --offline --no-default-features`: passed.
  Log: `.tmp/non-video-final-bsd-check.log`.
- Strict registered Java parity with the updated reference jar, selecting
  CZI, SVS, VSI, LSM, NIfTI, DICOM and OME-TIFF with a two-plane sampling cap:
  **8 files**, **18 core series**, and all **8 OME comparisons** passed.
  Pixel series: **7 bitwise**, **10 documented tolerant/relaxed JPEG**, zero
  mismatches; the oversized VSI base plane was skipped by the 512 MiB budget.
  Log: `.tmp/non-video-final-java-parity.log`.
- Additional source-reference parity records cover three XLEF projects / 17
  series and a generated native SlideBook project / eight planes, bitwise;
  see the individual reader reports for their exact sampling scope.
- Changed Rust files pass `rustfmt --check` with `skip_children=true`;
  `git diff --check` passes. The Java reference tree is clean.

## Evidence limits and exclusions

Real native SlideBook, LEI/raw, LSM MDB/overlay and some HCS companion datasets
remain scarce. Synthetic fixtures and Java-generated expected results verify
those implementation paths; they do not establish coverage of every proprietary
variant. ND2's recorded bounded corpus evidence is described in [ND2](../ND2.md).

Video codecs were explicitly excluded: QuickTime H.264, HEVC, ProRes, Motion
JPEG 2000 and DV. Existing JPEG decoder rounding tolerance remains documented
in the README. Additional JPEG 2000 region-decode optimization is optional
performance work. New LIF/ND2/CZI writers have no corresponding Java writer to
translate and are outside this gap-closure scope.

## Audit maintenance

Completed issue lists were replaced by concise current status pages. Original
ledgers remain under [the historical archive](archive/2026-10-26/TOFIX.md), clearly
marked as snapshots. Stale LMS compressed-Memory and LSM channel-wavelength
claims were removed after checking the actual Java source behavior.
