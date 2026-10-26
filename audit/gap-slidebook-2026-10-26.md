# SlideBook 7 implementation closure — 2026-10-26

Reference: `SlideBook7Reader.java` at Bio-Formats `d54bf462e3`.
Implementation: native SlideBook sections of `src/formats/flim2.rs`.

## Corrected behavior

- Native plane selection now matches declared `XYCZT`: channel varies first,
  then Z, then time. The old implementation selected Z first, which swapped
  channels and Z planes when both dimensions were non-trivial. OME Plane
  coordinates and per-channel exposure now use the same C/Z/T mapping.
- Native OME projection follows Java's explicit store calls. Parsed lens NA,
  fluorophore wavelengths and raw YAML values remain in reader metadata but
  are not published as Java-absent objective/wavelength/map-annotation fields.
- Every capture receives Java's Objective entry, including default Other
  correction/immersion, even when descriptive lens fields are absent.
  Channel names use the channel-definition name and Java's trimming behavior.
- Existing typed class decoding, file-count correction, stage/montage/timepoint
  mapping, supported NPY/NPYZ paths and CubeAnnotation ROI projection remain.

The old broad “native format not implemented” label was inaccurate: the current
Java loader itself discovers immediate `.imgdir` directories containing the
record YAML and NPY/NPYZ payloads already handled here. It does not implement a
second unknown binary project decoder that Rust still needs to invent.

## Validation

`cargo test --offline --lib slidebook7`: **23 passed**. The new regression
uses two channels, two Z planes and two timepoints with distinguishable pixels;
existing tests cover typed records, sparse files, compression and annotations.

`scripts/generate_slidebook7_parity_fixture.py` creates a complete Java-readable
native project with all relevant record files. Using the current Java reader
compiled into the reference overlay, strict parity passed **1 project / 1 core
series / 1 OME document / all 8 pixel planes bitwise**, including crop/full and
available offset comparisons. This exercises actual Java initialization and
pixel reading, not only a Rust-shaped synthetic parser test.

Reproduce generation with:

```sh
python3 scripts/generate_slidebook7_parity_fixture.py
```

Then run the registered Java parity harness with `BIOFORMATS_RS_JAVA_PARITY=1`,
`BIOFORMATS_RS_JAVA_PARITY_STRICT=1`, `BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__`,
`BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=8`, and
`BIOFORMATS_RS_JAVA_PARITY_EXTRA_FILES=<absolute generated .sldy path>`.
Select the reference build with `BIOFORMATS_RS_JAVA_PARITY_JAR` when needed.

Logs: `.tmp/gap-slidebook-tests.log`, `.tmp/gap-slidebook-java-parity.log`.
A real proprietary native project remains useful corpus evidence; its absence
is a validation limit, not deferred implementation of an identified source path.
