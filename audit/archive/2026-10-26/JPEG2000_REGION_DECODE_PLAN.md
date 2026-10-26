> Historical snapshot retained for provenance. This is not the active backlog.
> See [current implementation status](../../../TOFIX.md).

# JPEG2000 Region Decode Plan

Created: 2026-08-15

Status update: 2026-08-15

- Crates.io `openjpeg2-pure-rs` `0.1.0` was rejected for this work because it
  does not expose a safe region decode API.
- The current git `main` revision is the same code as the previous pinned
  revision, so `bioformats-rs` now vendors that source at
  `vendor/openjpeg2-pure-rs` and adds the safe API locally.
- `Decoder::decode_region` is exposed in the vendored crate. It converts
  partial-decode panics into errors.
- `src/common/codec.rs` has a shared region helper. It attempts true region
  decode first and falls back to full decode plus crop when the library cannot
  decode the requested area.
- Standalone JPEG2000 and CellSens/ETS region paths are wired through the
  shared helper.
- TIFF JPEG2000 region decode was trialed at the design/API level and backed
  out for now. The current pure OpenJPEG partial-DWT path fails on ordinary
  generated wavelet JP2 data, so TIFF would add several branch-specific call
  paths without a proven faster decode path. Revisit TIFF after
  `openjpeg2-pure-rs` can pass the common region equivalence tests without
  fallback.

Purpose: use `openjpeg2-pure-rs` region/tile decode support where it can reduce
work for `open_bytes_region` without changing Bio-Formats-visible pixel
semantics.

This is an optimization plan. The behavioral contract stays the current reader
contract: a region read must return the same bytes as full decode followed by
the existing crop/placement logic.

## Scope

Target the first three useful areas:

1. CellSens/ETS JPEG2000 tile reads in `src/formats/flim2.rs`.
2. Standalone JPEG2000 region reads in `src/formats/misc.rs`.
3. TIFF JPEG2000 tile/strip reads in `src/tiff/reader.rs` and
   `src/tiff/compression.rs`.

Do not change compressed tile APIs such as `read_compressed_tile`. Those APIs
must continue to expose original compressed tile bytes, not decoded regions.

## Upstream Crate Requirement

Before changing the readers, expose a safe region API from
`openjpeg2-pure-rs`. The low-level implementation already has primitives such
as decode area, decoded tile, and decoded resolution handling, but
`bioformats-rs` should consume a stable safe API rather than depending on
unexported internals.

Desired API shape:

```rust
Decoder::decode_region(x: u32, y: u32, width: u32, height: u32) -> Result<Image>
Decoder::decode_region_with_reduce(
    x: u32,
    y: u32,
    width: u32,
    height: u32,
    reduce: u32,
) -> Result<Image>
```

Acceptable first step:

- `decode_region(...)` only.
- Keep current full-decode fallback when reduced decode, unusual component
  geometry, or unsupported codestream features are encountered.

## Invariants

- Region decode must be compared against the current full-decode-then-crop path.
- Output packing, sample width, endianness, RGB/interleaving, and component
  order must stay unchanged.
- Requested Bio-Formats coordinates are authoritative; codec coordinates are an
  implementation detail.
- Partial tiles and out-of-range areas must preserve the reader's current fill
  and clipping behavior.
- Lossy JPEG2000 needs tolerance analysis. If subset decode differs from full
  decode plus crop for lossy data, keep full decode for that path unless the
  reader already permits equivalent lossy variation.
- Fall back to full decode on any unsupported region path instead of changing
  public behavior.

## Stage 1: Shared Codec Adapter

Add a common helper beside the existing JPEG2000 helpers in
`src/common/codec.rs`.

Planned helper:

```rust
decompress_jpeg2000_region_with_endianness(
    data: &[u8],
    little_endian: bool,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
) -> Result<Vec<u8>>
```

Implementation notes:

- Reuse the same sample packing logic as full JPEG2000 decode.
- Validate that returned component dimensions match the requested region.
- Return an explicit unsupported error only inside the helper; callers should
  catch that and fall back to full decode when preserving behavior requires it.
- Add focused unit tests with a small lossless JP2/J2K fixture:
  - full image decode equals current path;
  - centered crop;
  - edge crop;
  - single-pixel crop;
  - big-endian and little-endian output where applicable.

## Stage 2: Standalone JPEG2000 Reader

File: `src/formats/misc.rs`

Target:

- `Jpeg2000Reader::open_bytes_region`.

Current behavior:

- Decodes the whole plane through `decode_pixels`, then crops in Rust.

New behavior:

- For non-full-region requests, call the shared JPEG2000 region helper.
- Preserve full-plane decode path for full-plane reads.
- Fall back to the current full-decode-then-crop path if region decode is
  unsupported or validation fails.

Why first:

- One codestream maps directly to one plane, so the coordinate mapping is
  simple and semantic risk is low.

Tests:

- Extend existing JPEG2000 reader tests to assert region reads match full decode
  crops for several offsets and sizes.
- Include a full-plane read to ensure the old fast path remains valid.

Acceptance gate:

- `cargo test jpeg2000 --lib`
- `cargo test jpeg2000_writer --test write_test`
- `cargo fmt --check`

## Stage 3: CellSens/ETS JPEG2000 Tiles

File: `src/formats/flim2.rs`

Targets:

- ETS tile JPEG2000 decode around `decode_tile`.
- Region assembly around `assemble_region`.
- CellSens reader `open_bytes_region` dispatch.

Current behavior:

- The reader determines which ETS tiles intersect the requested region.
- Each intersecting compressed JPEG2000 ETS tile is read and decoded in full.
- The decoded tile is then copied into the output region with existing
  clipping/fill behavior.

New behavior:

- Keep the existing ETS tile intersection logic.
- For each intersecting ETS tile, compute the sub-region needed inside that
  tile.
- Decode only that sub-region from the JPEG2000 tile payload when possible.
- Copy the smaller decoded tile fragment through the same placement rules.
- Fall back per tile to full tile decode when region decode is unsupported.

Important details:

- Preserve CellSens Java-compatible tile origin and pyramid behavior.
- Preserve missing/partial tile fill semantics.
- Preserve big-endian output currently used for JPEG2000 ETS tile payloads.
- Do not change `compressed_level_info` or `read_compressed_tile`.
- This may reduce decode CPU and memory more than disk I/O, because the ETS
  chunk table still identifies whole compressed tile payload ranges.

Tests:

- Existing synthetic ETS tests must keep passing.
- Add tests where a requested region:
  - touches one tile partially;
  - crosses tile boundaries;
  - starts at a nonzero x/y offset;
  - reaches the right/bottom edge of a tile;
  - includes missing tile/background fill if covered by existing fixtures.
- Run Java parity against the Joda VSI with metadata and bounded pixel reads.

Acceptance gate:

- `cargo test cellsens_ --lib`
- Java parity check for
  `/big/henriksson/ome_images/from_joda/haut/Image_045_M5_MEV_Sk3_Zstack_40x_decon.vsi`
  with representative nonzero-offset region reads.

## Stage 4: TIFF JPEG2000 Trial

Files:

- `src/tiff/reader.rs`
- `src/tiff/compression.rs`

Targets:

- JPEG2000 branch in TIFF strip/tile decompression.
- `TiffReader::open_bytes_region` when a requested region only needs part of a
  JPEG2000-compressed strip/tile.

Current behavior:

- TIFF reads the compressed strip/tile payload.
- JPEG2000 decompression produces a full decoded strip/tile.
- Existing TIFF placement/crop code copies the requested bytes into the output
  region.

Trial behavior:

- Thread an optional requested sub-region through the TIFF JPEG2000
  decompression path.
- Decode only the region needed inside the compressed TIFF tile/strip.
- Keep all existing TIFF placement, planar configuration, photometric, endian,
  and crop semantics.
- Full decode remains the fallback for unsupported layouts.

Risk areas:

- Strips versus tiles have different coordinate shapes.
- Planar configuration and RGB/component packing can obscure codec component
  coordinates.
- TIFF photometric conversion or downstream transforms may expect a full tile.
- JPEG2000 reduced resolution and TIFF sub-resolution handling must not be
  conflated.
- Some TIFF JPEG2000 codestreams may not benefit if the decoder still has to
  process most of the codestream.

Back-out criteria:

- Back out the TIFF region-decode trial if any of these are true:
  - code needs broad restructuring outside TIFF decompression and region
    assembly;
  - region decode cannot be isolated behind a small optional path;
  - full-decode parity fails for representative tiled and stripped TIFF cases;
  - performance improvement is negligible on realistic small-region reads;
  - lossy TIFF JPEG2000 subset decode differs materially from full decode plus
    crop;
  - handling planar/RGB/photometric cases requires format-specific exceptions
    that make the path hard to audit.

Rollback plan:

- Keep the common codec helper and stages 2-3 intact.
- Revert only the TIFF call-site changes.
- Leave a short note in this file describing the failing layout or benchmark.

Tests:

- Synthetic TIFF JPEG2000 tiled image:
  - crop inside one tile;
  - crop crossing tiles;
  - crop at right/bottom image edge.
- Synthetic TIFF JPEG2000 stripped image:
  - crop inside one strip;
  - crop crossing strips.
- Existing compressed tile tests must still pass.

Acceptance gate:

- `cargo test tiff --lib`
- `cargo test jpeg2000 --lib`
- Focused real-data smoke test if a JPEG2000 TIFF fixture exists locally.

## Benchmark Plan

Measure before and after each stage:

- Full-plane read.
- Small centered region.
- Small edge region.
- Region crossing tile boundaries.
- Repeated reads from the same plane, if reader caching is present.

Record:

- elapsed wall time;
- peak allocation or maximum resident set size when practical;
- decoded bytes requested versus returned;
- fallback count, if instrumented temporarily.

Useful real-data target:

- CellSens Joda VSI:
  `/big/henriksson/ome_images/from_joda/haut/Image_045_M5_MEV_Sk3_Zstack_40x_decon.vsi`

## Suggested Work Order

1. Add or request `openjpeg2-pure-rs` safe region API.
2. Add shared JPEG2000 region helper in `src/common/codec.rs`.
3. Implement standalone `Jpeg2000Reader::open_bytes_region` optimization.
4. Implement CellSens ETS per-tile region decode.
5. Benchmark and verify stages 2-3.
6. Trial TIFF JPEG2000 region decode behind an easily removable call path.
7. Decide whether to keep or back out TIFF based on parity and benchmark gates.

## Done Criteria

- Region reads match full-decode-then-crop bytes for supported lossless cases.
- Unsupported cases fall back to full decode.
- CellSens ETS and standalone JPEG2000 use region decode without a user-visible
  option.
- TIFF JPEG2000 is either kept with tests and benchmark evidence, or explicitly
  backed out with the reason recorded.
- Existing compressed-tile APIs continue to return original compressed bytes.
