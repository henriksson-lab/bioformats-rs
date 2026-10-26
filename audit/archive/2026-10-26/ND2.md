> Historical snapshot retained for provenance. This is not the active backlog.
> See [current implementation status](../../../TOFIX.md).

# ND2 parity audit

Goal: bring `src/formats/nd2.rs` back toward a 1:1 translation of Java Bio-Formats `ND2Reader`, using real ND2 data and recording every remaining mismatch.

## Current status

Open for full-corpus pixel and performance parity. Full-corpus no-pixel Java
metadata parity now passes for all 62 discovered ND2 files:

- core metadata: 202 series OK / 0 bad
- OME metadata: 62 files OK / 0 bad
- pixels: skipped by `BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1`
- runtime: about 274 seconds with warm Java oracle cache

Bounded pixel parity now passes on all 62 discovered ND2 files with
`BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`:

- core metadata: 202 series OK / 0 bad
- OME metadata: 62 files OK / 0 bad
- pixels: 201 bitwise / 0 bad; 1 huge series skipped by the 512 MiB full-plane
  budget
- runtime: about 816 seconds with warm Java oracle cache

Full-depth pixel parity remains proven on the representative/focused subset and
batches below; the remaining corpus has bounded pixel proof but not default-depth
proof.

Latest cleanup toward 1:1 auditability:

- Inlined the one-use Rust-only loop-descriptor dimension-order helper into the
  ND2 metadata initialization path. This keeps the ZStackLoop/TimeLoop fallback
  visible where the Java `initFile` dimension-order correction is being modeled.
- Inlined the one-use `block_offset + 16` helper used by the row-size correction
  path. The code now shows the Java-equivalent consecutive image-offset
  arithmetic at the correction site instead of hiding it behind a Rust-only
  wrapper.
- Removed the modern-ND2 raw 1 MiB prefix scan for `pPosName` fallback. Java
  populates image names from `handler.getPositionNames()` after parsing metadata
  blocks; it does not scan the file prefix as a general fallback. The known
  multi-position naming case, `b16_pdtB+y50__crop.nd2`, still matches Java
  through the normal parsed metadata path.
- Kept the split-channel byte extraction helper because it is the adapter for
  Java's `ImageTools.splitChannels` library call to Rust byte-slice code; this
  is the allowed helper exception under the parity audit rule.
- Verification after this cleanup: `cargo test nd2_ --lib` passes 27/27, the
  focused issue-regression manifest passes 6 core series and 6 OME files with
  pixels skipped, and the NDControl regression manifest passes 4 core series and
  4 OME files with pixels skipped. The row-size helper inline was rechecked with
  `cargo test nd2_ --lib` and the focused issue-regression manifest.
- Verification after removing the prefix scan: `cargo test nd2_ --lib` passes
  27/27; `b16_pdtB+y50__crop.nd2` passes 12 core series and OME metadata with
  pixels skipped; the focused issue-regression manifest passes 6 core series and
  6 OME files with pixels skipped; the full `.tmp/nd2-all.paths` no-pixel pass
  compares 62 files, 202 core series, and 62 OME files with 0 mismatches
  (runtime about 290 seconds with warm Java oracle cache).
- Perf profiling of `ns_blobtest2.0_20x_A2_far.nd2` showed open time dominated
  by allocation-heavy iterator/string collection rather than disk I/O. Rewrote
  `nd2_xml_ui_count_for_runtype` and NDControl `LoopSize` extraction to stream
  the first needed values directly, matching Java's SAX-style event consumption
  instead of materializing temporary vectors.
- Verification after the streaming XML cleanup: `cargo test nd2_ --lib` passes
  27/27; the focused issue-regression manifest passes 6 core series and 6 OME
  files with pixels skipped; the NDControl regression manifest passes 4 core
  series and 4 OME files with pixels skipped; the full `.tmp/nd2-all.paths`
  no-pixel pass compares 62 files, 202 core series, and 62 OME files with 0
  mismatches (runtime about 274 seconds with warm Java oracle cache).
- Current `ns_blobtest2.0_20x_A2_far.nd2` release open-only profile after these
  cleanups is about 191 ms (`open_ns=190996112`, 132 image chunks, 155 total ND2
  chunks). The performance gap is smaller but still not fully closed against the
  best Java subset timing previously observed.
- Added the PR #9-compatible raw region fast path for modern ND2 frames:
  `open_bytes_region` now reads only the requested window directly from raw
  uncompressed ImageDataSeq payloads, including split-channel logical planes,
  and falls back to the full Java-shaped decode/crop path for compressed,
  chunk-table, legacy JPEG2000, lossless, or ambiguous frames. This is
  compatible with the Bio-Formats translation philosophy because it does not
  change public series/OME/pixel semantics; it is only a narrower I/O path under
  the same raw-frame layout rules. The chunk-map reader was also tightened to
  scan map entries sequentially before validating referenced chunk headers,
  avoiding repeated stream-position queries during map parsing while preserving
  header-derived offsets and the existing coarse Java-style sanity checks.
  Focused real-data parity after this optimization passes for
  `.tmp/nd2-remaining-failures.path` with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=2`:
  6 files compared, 6 core series OK, 6 OME files OK, and 6/6 pixel series
  bitwise.
- Fixed the Java parity oracle output-size blocker that made `control003.nd2`
  look untestable at the default 64-plane depth. The oracle still emits
  full-plane CRC/length for exact whole-plane parity, but now omits raw
  full-plane base64 for planes larger than 256 KiB. Direct Java failure before
  this change was `OutOfMemoryError` while appending the JSON line after about
  5.4 GiB RSS; this was a verifier limitation, not an ND2 reader mismatch.
- Fixed two batch-04 pixel parity regressions found only by default-depth
  checks. Chunk-map ImageDataSeq offsets now use each chunk's actual name length
  instead of a Rust-only cached first-image name length, matching Java's
  per-entry `realLength` behavior when names grow from `ImageDataSeq|9!` to
  `ImageDataSeq|10!`. Raw uncompressed frames whose 8-byte timestamp prefix is
  zero now still skip 8 bytes like Java, instead of taking the Rust-only
  7-byte legacy fallback.
- Fixed the batch-07 `3.nd2` full-plane-only pixel divergence. Chunk-map
  ImageDataSeq entries now use the actual chunk header's payload length rather
  than the chunk-map block length. This matches Java's chunk-map path, where the
  final pixel offset is `entry.position + 16 + realLength + 8` and the readable
  payload length is derived from the block length after header/name bytes.
- Remaining gaps: full-depth pixel parity is not yet proven across all 62
  discovered files, and the `ns_blobtest2.0_20x_A2_far.nd2` open-time
  performance gap remains an audit item.

## Real-data manifests

- All discovered ND2 data: `.tmp/nd2-all.paths` (62 files from `/big/henriksson/ome_images/ND2`)
- Representative subset: `.tmp/nd2-representative.paths`
- Broad batches: `.tmp/nd2-batches/batch-00.paths` through `.tmp/nd2-batches/batch-12.paths`
- Focused issue-regression manifest: `.tmp/nd2-remaining-failures.path`
- Focused NDControl regression manifest: `.tmp/nd2-ndcontrol-regression.path`

Representative subset:

```text
/big/henriksson/ome_images/ND2/aryeh/MeOh_high_fluo_003.nd2
/big/henriksson/ome_images/ND2/aryeh/b16_14_12.nd2
/big/henriksson/ome_images/ND2/aryeh/but3_cont200-1.nd2
/big/henriksson/ome_images/ND2/aryeh/multipoint.nd2
/big/henriksson/ome_images/ND2/jonas/100217_OD122_001.nd2
```

Metadata command:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-representative.paths \
cargo test --test java_parity_test java_parity -- --nocapture
```

Current representative result: 5 files compared, 12 core series OK / 0 bad, 5 OME files OK / 0 bad, pixels skipped.

Current focused issue-regression result for `.tmp/nd2-remaining-failures.path`:
6 files compared, 6 core series OK / 0 bad, 6 OME files OK / 0 bad, pixels
skipped. This covers:

- `/big/henriksson/ome_images/ND2/jonas/100217_OD122_001.nd2`
- `/big/henriksson/ome_images/ND2/jonas/512c/nikonTest_two.nd2`
- `/big/henriksson/ome_images/ND2/jonas/header_test1.nd2`
- `/big/henriksson/ome_images/ND2/jonas/jonas_nd2Test/Exception61.nd2`
- `/big/henriksson/ome_images/ND2/jonas/multicolorTest.nd2`
- `/big/henriksson/ome_images/ND2/zenodo-7439387/ns_blobtest2.0_20x_A2_far.nd2`

Current NDControl regression result for `.tmp/nd2-ndcontrol-regression.path`:
4 files compared, 4 core series OK / 0 bad, 4 OME files OK / 0 bad, pixels
skipped. This covers the cropped NDControl collapse cases and
`ns_blobtest2.0_20x_A2_far.nd2`, which must keep `Z=3`, `T=44`.

Latest full no-pixel run over `.tmp/nd2-all.paths` passes with cached Java
oracle output: 62 files compared, 202 core series OK / 0 bad, 62 OME files OK /
0 bad, pixels skipped; runtime about 274 seconds.

Fast metadata iteration command:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-time-sequence-24.path \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

With `BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1`, the harness now passes `maxPlanes=0` and `fullPlane=false` to the Java oracle. That keeps both Java and Rust in metadata-only mode; earlier runs skipped Rust pixel comparison but still made Java read pixel crops.

Use `BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR` to reuse Java oracle JSON between Rust iterations. Use `BIOFORMATS_RS_JAVA_PARITY_REFRESH_CACHE=1` only when deliberately refreshing the Java reference output after changing the oracle, `bioformats_package.jar`, or fixture data. The cache key includes the file length/mtime, oracle arguments, Bio-Formats jar fingerprint, and `parity/BfParityOracle.java` fingerprint.

For quick pixel iteration, set `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=N`. The default remains 64 planes per series for final confirmation, but a smaller value such as 4 checks the same code path without paying the full deep-stack cost on every run.

Fast debugging loop:

1. Use `cargo test nd2_ --lib` for Rust-only unit coverage while editing.
2. Use one-file manifests such as `.tmp/nd2-batches/batch-09-c3-reconstructed.path` with `BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1` and `BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache` for focused Java parity.
3. For pixel smoke checks, add `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4` rather than using the default 64-plane depth.
4. Run the full five-file batch only after the focused file passes.
5. Save full-depth pixel parity and all-batch/all-corpus runs for confirmation, not for every metadata edit.

Verified metadata-only/cache behavior on `Time sequence 24.nd2`:

- forced cache refresh/miss after the metadata-only harness fix: no-pixel parity OK, runtime about 12 seconds, cache file written under `.tmp/java-oracle-cache/`.
- cache hit: no-pixel parity OK, runtime about 11 seconds.
- previous no-pixel behavior, before passing `maxPlanes=0` to Java, took about 108 seconds because Java still read pixel regions and emitted CRC/base64 data.

## Pixel parity spot checks

Full-depth pixel parity, using the default 64-plane cap, has been checked on six representative/focused ND2 files. All six passed bitwise against Java Bio-Formats:

| File | Coverage | Runtime |
| --- | ---: | ---: |
| `BF007.nd2` | 2/2 checks | about 2.6s |
| `HE_002.nd2` | 4/4 checks | about 4.9s |
| `ns_blobtest2.0_20x_A2_far.nd2` | 65/65 checks | about 36.0s |
| `MeOh_high_fluo_003.nd2` | 27/27 checks | about 8.6s |
| `b16_14_12.nd2` | 129/129 checks | about 134.8s |
| `100217_OD122_001.nd2` | 129/129 checks | about 15.5s |

This covers modern ND2, high Z/T, old-JP2, split-channel old-JP2, and
Nikon-style grouped metadata cases. Full-depth pixel parity is not yet proven
across all 62 discovered ND2 files.

Full-depth batch pixel parity, using the default 64-plane cap:

| Batch | Files | Series | Pixel result | Runtime |
| --- | ---: | ---: | --- | ---: |
| `batch-00.paths` | 5 | 5 | 260 bitwise checks / 0 bad | about 80.2s |
| `batch-01.paths` | 5 | 17 | 17 bitwise series / 0 bad; 1471 detailed checks | about 1226.2s |
| `batch-02.paths` | 5 | 24 | 24 bitwise series / 0 bad; 2662 detailed checks, combining the earlier batch run with the focused `control003.nd2` proof | about 1849.9s + 395.4s |
| `batch-04.paths` | 5 | 5 | 5 bitwise series / 0 bad; 578 detailed checks | about 21.7s |
| `batch-05.paths` | 5 | 7 | 7 bitwise series / 0 bad; 855 detailed checks | about 198.3s |
| `batch-06.paths` | 5 | 6 | 6 bitwise series / 0 bad; 766 detailed checks | about 160.2s |
| `batch-07.paths` | 5 | 12 | 12 bitwise series / 0 bad; 1547 detailed checks | about 68.3s |
| `batch-08.paths` | 5 | 5 | 5 bitwise series / 0 bad; 429 detailed checks | about 157.9s |
| `batch-09.paths` | 5 | 15 | 14 bitwise series / 0 bad; 468 detailed checks; 1 huge series skipped by memory budget | about 261.9s |
| `batch-10.paths` | 5 | 36 | 36 bitwise series / 0 bad; 133 detailed checks | about 224.7s |
| `batch-11.paths` | 5 | 44 | 44 bitwise series / 0 bad; 88 detailed checks | about 12.5s |
| `batch-12.paths` | 2 | 2 | 2 bitwise series / 0 bad; 67 detailed checks | about 55.2s |

`batch-00.paths` covers `Experiment_0001.nd2`, `MRAP1 KO DN_10X03.nd2`,
`823_Clarkia pulchella_09-1_DIC_SG.nd2`, `sample_image.nd2`, and
`MeOh_high_fluo_003.nd2`. This adds full-depth proof for modern raw
single-channel, split-channel RGB/3-channel, and 5-channel Z-stack cases.

`batch-01.paths` covers `4_2_1_cont_NoMR001.nd2`, `weekend002.nd2`,
`por003.nd2`, `MeOh_high_fluo_007.nd2`, and `but3_cont200-1.nd2`. This adds
full-depth proof for the old-JP2 multi-series/channel grouping path, including
big-endian JP2 decode and old-JP2 RGB/logical-channel splitting cases.

`batch-02.paths` now has default-depth pixel proof for all five files. The
earlier batch run proved `4_con_2_1_cot002.nd2`, `MeOh_high_fluo_011.nd2`,
`multipoint.nd2`, and `old_cells_02.nd2`; the missing `control003.nd2` case now
passes a focused default-depth run after fixing the Java oracle JSON-size
blocker:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-control003.path \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Result: 1 file compared, 10 core series OK / 0 bad, OME metadata OK, and 10
bitwise pixel series / 0 bad. Each series reported 103 default-depth checks:
51 top-left crops, 51 whole-plane CRC checks, and 1 non-zero-origin offset crop.

`batch-12.paths` also passes default-depth pixel parity after the oracle
output-size change:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-12.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Result: 2 files compared, 2 core series OK / 0 bad, 2 OME files OK / 0 bad,
and 2 bitwise pixel series / 0 bad. This covers `BF007.nd2` and
`ns_blobtest2.0_20x_A2_far.nd2` as a batch-level default-depth check.

`batch-07.paths` initially found one full-plane-only pixel divergence:

- `3.nd2`: crop and non-zero-origin region checks matched, but full-plane
  checks failed. The file has unusually large ImageDataSeq chunk names
  (`nameLength=4072` on the first image chunk). Rust used the chunk-map entry
  length as the readable payload length for image chunks, so full-plane decoding
  could retain the 8-byte frame timestamp as pixels. Java's chunk-map code uses
  `entry.position + 16 + realLength + 8` for the final pixel offset and derives
  the payload span after header/name bytes. Rust now reads each ImageDataSeq
  chunk header's actual data length, preserving the Java offset and payload
  semantics.

After the fix:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-07.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Result: 5 files compared, 12 core series OK / 0 bad, 5 OME files OK / 0 bad,
and 12 bitwise pixel series / 0 bad. This covers `2112-2265.nd2`, `3.nd2`,
`JJ1473_conrol_24h_02.nd2`, `JJ_RNAi_hmr-1_001.nd2`, and
`JJ_RNAi_hmr-1_001_CropCyan.nd2` at the default 64-plane cap.

`batch-08.paths` also passes default-depth pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-08.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Result: 5 files compared, 5 core series OK / 0 bad, 5 OME files OK / 0 bad,
and 5 bitwise pixel series / 0 bad. This covers `2.nd2`, `header_test2.nd2`,
`169_Cropped.nd2`, `169_Original.nd2`, and `19825_Original.nd2` at the default
64-plane cap. The large original-resolution files use crop and non-zero-origin
offset checks only because their full planes exceed the Java oracle full-plane
memory budget.

`batch-09.paths` passes default-depth pixel parity within the harness memory
budget:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-09.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Result: 5 files compared, 15 core series OK / 0 bad, 5 OME files OK / 0 bad,
14 bitwise pixel series / 0 bad, and 1 huge series skipped by the 512 MiB
full-plane budget. This covers the OMERO `WellA02` file, the `C3` reconstructed
file, `19825_Cropped.nd2`, `Slide4-4_Region0001_Channel20xDAPI,20xCy3_Seq0027.nd2`,
and `B08.nd2` at the default 64-plane cap where memory budgets allow pixel
checks.

`batch-10.paths` passes default-depth pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-10.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Result: 5 files compared, 36 core series OK / 0 bad, 5 OME files OK / 0 bad,
and 36 bitwise pixel series / 0 bad. This covers the OMERO `WellA03` file,
`HE_002.nd2`, `N2_file_bugged_reconstructed-version.nd2`,
`N3_file_nobug_raw-version.nd2`, and `N4_file_nobug_raw-version.nd2` at the
default 64-plane cap. Large single-plane series use crop and offset checks only
when their full planes exceed the Java oracle full-plane memory budget.

`batch-11.paths` passes default-depth pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-11.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Result: 5 files compared, 44 core series OK / 0 bad, 5 OME files OK / 0 bad,
and 44 bitwise pixel series / 0 bad. This covers the paired `N1`, `N2`, `N3`,
and `N4` raw/reconstructed files at the default 64-plane cap.

`batch-04.paths` initially found two pixel-only Rust divergences despite clean
core/OME metadata:

- `Exception_2.nd2`: planes 0-9 matched, then plane 10 diverged. Root cause was
  the chunk-map parser caching the first `ImageDataSeq` name length and applying
  it to all image chunks. That shifted two-digit image names by one byte, so
  `ImageDataSeq|10!` was read from `!` plus compressed bytes instead of the true
  frame timestamp plus zlib stream. Java computes a per-entry real length.
- `Exception9_e3.nd2`: plane 0 region reads diverged while the full-plane path
  was corrected. Root cause was the same Rust-only 7-byte raw fallback in the
  direct region path. Java's chunk-map ImageDataSeq path skips 8 bytes before
  pixel reads even when the timestamp double is zero.

After fixing both paths:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-04.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Result: 5 files compared, 5 core series OK / 0 bad, 5 OME files OK / 0 bad,
and 5 bitwise pixel series / 0 bad. The focused ND2 unit suite also passes
29/29, including new tests for variable-length ImageDataSeq chunk-map names and
zero-timestamp raw frames.

`batch-05.paths` passes default-depth pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-05.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Result: 5 files compared, 7 core series OK / 0 bad, 5 OME files OK / 0 bad,
and 7 bitwise pixel series / 0 bad. This covers `header_test1.nd2`,
`cag_p5_simgc_2511_70ms22s_crop.nd2`, `nikonTest_two.nd2`, `wtf_DIC.nd2`, and
`nikonTest_1.nd2` at the default 64-plane cap.

`batch-06.paths` passes default-depth pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-06.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Result: 5 files compared, 6 core series OK / 0 bad, 5 OME files OK / 0 bad,
and 6 bitwise pixel series / 0 bad. This covers `wtf_alignment.nd2`,
`100217_OD122_001.nd2`, `290110.tranMgc005cr.nd2`, `multicolorTest.nd2`, and
`JJ1473_control_24h_03.nd2` at the default 64-plane cap.

Bounded batch pixel parity, using `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`,
currently passes for all discovered files:

| Batch | Files | Series | Pixel result |
| --- | ---: | ---: | --- |
| `batch-00.paths` | 5 | 5 | 5 bitwise / 0 bad |
| `batch-01.paths` | 5 | 17 | 17 bitwise / 0 bad |
| `batch-02.paths` | 5 | 24 | 24 bitwise / 0 bad |
| `batch-03.paths` | 5 | 24 | 24 bitwise / 0 bad |
| `batch-04.paths` | 5 | 5 | 5 bitwise / 0 bad |
| `batch-05.paths` | 5 | 7 | 7 bitwise / 0 bad |
| `batch-06.paths` | 5 | 6 | 6 bitwise / 0 bad |
| `batch-07.paths` | 5 | 12 | 12 bitwise / 0 bad |
| `batch-08.paths` | 5 | 5 | 5 bitwise / 0 bad |
| `batch-09.paths` | 5 | 15 | 14 bitwise / 0 bad; 1 huge series skipped by memory budget |
| `batch-10.paths` | 5 | 36 | 36 bitwise / 0 bad |
| `batch-11.paths` | 5 | 44 | 44 bitwise / 0 bad |
| `batch-12.paths` | 2 | 2 | 2 bitwise / 0 bad |

## Confirmed mismatches

| File | Java | Rust | Parity issue |
| --- | --- | --- | --- |
| `MeOh_high_fluo_003.nd2` | series=1, dimension order `XYCZT`, channel 0 name `pdt-405`, emission=460.0 | fixed: same core metadata and OME as Java | Fixed |
| `b16_14_12.nd2` | series=1, sizeC=2, sizeT=50, imageCount=100, interleaved=false, resolutionCount=1 | fixed: same core metadata and OME as Java | Fixed |
| `but3_cont200-1.nd2` | series=5, sizeC=2, imageCount=2 per series, channel 0 name `20phase`, emission=525.0 | fixed: same core metadata and 5-image OME as Java | Fixed |
| `multipoint.nd2` | series=4, sizeZ=5, sizeT=11, imageCount=55 per series, channel 0 name `beeds4`, emission=585.0 | fixed: same core metadata and 4-image OME as Java | Fixed |
| `100217_OD122_001.nd2` | sizeZ=29, sizeT=25, bitsPerPixel=14, emission=520, excitation=488 | fixed: sizeZ=29, sizeT=25, bitsPerPixel=14, matching OME wavelengths | Fixed |
| `b16_pdtB+y50__crop.nd2` | 12 series, each `sizeZ=1`, `sizeT=56`, `imageCount=56`, `bitsPerPixel=16`, first OME image names use position names `#3` through `#10` | fixed: same core metadata and OME as Java | Fixed |
| `control003.nd2` | 10 series, each `sizeT=1`, `imageCount=51`, OME channel 2 emission=570.0 | fixed: same core metadata and OME as Java | Fixed |
| `Exception_2.nd2` | plane 10 reads zlib payload after the 8-byte ImageDataSeq prefix despite the two-digit chunk name | fixed: chunk-map offsets use each image chunk's actual name length; default-depth pixels match Java | Fixed |
| `Exception9_e3.nd2` | raw plane 0 region reads skip the 8-byte ImageDataSeq prefix even when the timestamp value is zero | fixed: raw full-plane and region reads both use Java's 8-byte prefix skip | Fixed |
| `3.nd2` | full-plane raw reads start at `ImageDataSeq` data offset + 8 even for huge chunk names; chunk-map payload length excludes header/name bytes | fixed: image chunks use the actual chunk header data length, so full-plane and region reads share Java's payload offset/length semantics | Fixed |
| `290110.tranMgc005cr.nd2` | `sizeY=117`, `dimensionOrder=XYZTC`, indexed=false, OME channel 0 name unset | fixed: same core metadata and OME as Java | Fixed |
| `169_Original.nd2` | OME channels 0-2 named `Brightfield` | fixed: same core and OME as Java | Fixed |
| `19825_Original.nd2` | OME channels 0-2 named `Brightfield` | fixed: same core and OME as Java | Fixed |

Broad batch 00 is fixed and now passes:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-00.paths \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified batch-00 result:

- files compared: 5
- core metadata: 5 series OK / 0 bad
- OME metadata: 5 files OK / 0 bad
- pixels: 5 bitwise / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`

Fixed batch-00 pixel mismatches:

- `823_Clarkia pulchella_09-1_DIC_SG.nd2`: Rust applied Java's
  `getScanlinePad()` rule as `sizeX * sizeC + pad`; Java's split raw branch
  allocates `(sizeX + scanlinePad) * sizeY * pixel`, where `pixel` already
  includes all channels, then calls `ImageTools.splitChannels` before
  `readPlane`. Rust now follows that split-then-depad sequence.
- `Experiment_0001.nd2`: Java's non-split raw branch resets `scanlinePad` to a
  4-byte row-boundary value before `readPlane`. Rust now uses the same row
  stride and zero-filling behavior instead of the ND2 `getScanlinePad()` helper
  rule.

Broad batch 01 is fixed and now passes:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-01.paths \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified batch-01 result:

- files compared: 5
- core metadata: 17 series OK / 0 bad
- OME metadata: 5 files OK / 0 bad
- pixels: 17 bitwise / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`
- runtime: about 184 seconds with Java oracle cache hits

Fixed batch-01 mismatches:

- `4_2_1_cont_NoMR001.nd2` is old JP2-backed ND2. Java exposes 5 series, each `sizeC=2`, `sizeT=81`, `imageCount=162`; Rust previously exposed 405 two-plane series. The old-JP2 fallback now infers repeated XY positions from the `dTimeMSec` cadence when explicit `XYPosLoop` / `TimeLoop` containers are absent.
- `weekend002.nd2` is old JP2-backed ND2. Java reports `PhysicalSizeX/Y=100.0` for all 3 images; Rust previously filtered it out. The old-JP2 calibration filter now keeps this single-channel `100.0` case while still matching `por003.nd2` and `but3_cont200-1.nd2`, where Java leaves OME physical X/Y unset.
- A later all-file metadata run showed `4_2_1_cont_NoMR001.nd2` regressed to
  11 series with `sizeT=36` because Rust chose an exact `XYPosLoop x TimeLoop`
  product before Java's first-count/cap flow. The old-JP2 path now uses
  Java-style first `uiCount` evidence (`TimeLoop=81`, `XYPosLoop=11`) only when
  Java's final series cap consumes the available planes exactly; otherwise it
  falls back to XML-derived XY position count plus timestamp count. The previous
  Rust-only timing-gap helper was removed because it inferred alternate
  position counts and made the old-JP2 path drift from Java. This also preserves
  `but3_cont200-1.nd2`, `por003.nd2`, and `weekend002.nd2`.
- Old JP2 pixel decode now requests big-endian samples, matching Java's
  `littleEndian=false` metadata and `JPEG2000Codec` output on this path.
  Previously Rust decoded the same UINT16 samples little-endian, causing every
  old-JP2 pixel crop in batch 01 to diverge.

Broad batch 02 is fixed and now passes:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-02.paths \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified batch-02 result:

- files compared: 5
- core metadata: 24 series OK / 0 bad
- OME metadata: 5 files OK / 0 bad
- pixels: 24 bitwise / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`
- runtime: about 200 seconds with Java oracle cache hits

Fixed batch-02 mismatches:

- `old_cells_02.nd2` now matches Java's `XYPosLoop` split: 4 series, each `sizeZ=7`, `sizeT=42`, `imageCount=294`, `dimensionOrder=XYCZT`.
- `old_cells_02.nd2` and `control003.nd2` now use Java's non-ImageMetadataLV
  offset mapping: build the four `lengths` entries from dimension order and
  `fieldIndex=2`, map each `ImageDataSeq` index through
  `FormatTools.rasterToPosition`, collapse the field axis, and place offsets by
  `FormatTools.positionToRaster`. Rust previously used a contiguous divisibility
  shortcut, which preserved metadata counts but selected the wrong source chunks
  for nonzero series.
- The non-ImageMetadataLV mapping is guarded so it must fill the Java-sized
  source-plane arrays. This preserves `B08.nd2`, where Java's offset-table
  compaction needs 405 stored frames per series before split-channel expansion;
  an unconstrained raster mapping produced only 135 and regressed `sizeT` from
  81 to 27.
- Rust now models the Java SAX handler's `prevRuntype` behavior for child `uiCount` elements. This lets `<no_name runtype="...XYPosLoop"><uiCount runtype="lx_uint32" value="4"/>` set the series count like Java.
- Line-based TextInfo `Dimensions`, `Time Loop`, and `Z Stack Loop` annotations are now parsed like `ND2Handler.parseKeyAndValue`. For `old_cells_02.nd2`, this lowers the XML time-loop count from `73` to text `41`; the Java-style image-count correction then increments it to `42`.
- OME channel wavelengths no longer come from generic optical filter spectra or channel XML `ExWavelength`. Java leaves `old_cells_02.nd2` emission unset and `MeOh_high_fluo_011.nd2` excitation unset, while still accepting channel XML `EmWavelength` where Java does.

Focused batch-03 regressions are fixed and now pass no-pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-time-sequence-24.path \
cargo test --test java_parity_test java_parity -- --nocapture

BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-rfp-h2a-cells-02.path \
cargo test --test java_parity_test java_parity -- --nocapture

BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-03.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified focused results:

- `Time sequence 24.nd2`: 1 core series OK, OME metadata OK, pixels skipped.
- `rfp_h2a_cells_02.nd2`: 5 core series OK, OME metadata OK with 5 images, pixels skipped.
- Full batch 03 now passes: 5 files compared, 24 core series OK / 0 bad, 5 OME files OK / 0 bad, pixels skipped; runtime about 113 seconds with Java oracle cache hits.

Fixed focused batch-03 mismatches:

- `Time sequence 24.nd2` is old JP2-backed ND2 with RGB JP2 bands. Java treats the JP2 component count as RGB samples, not logical channels, so it exposes `sizeC=3`, `sizeT=60`, and `imageCount=60`. Rust previously multiplied the old-JP2 plane count by three logical channels. The old-JP2 path now separates RGB sample count from logical non-RGB channel grouping and decodes/crops RGB planes with three samples per pixel.
- Pixel parity for `Time sequence 24.nd2` required matching Java's
  `rgb=true`, `interleaved=false` old-JP2 read path. The Rust JPEG2000 decoder
  returns component-interleaved RGB, but Java `readPlane` consumes old-JP2 RGB
  as planar channel blocks. Rust now planarizes old-JP2 RGB full planes and
  crops planar RGB regions channel-by-channel like Java.
- `rfp_h2a_cells_02.nd2` is modern chunk-map ND2 with `XYPosLoop=5`, `Z=5`, `T=116`, two split channels, and one missing `ImageDataSeq` index. Java still exposes five full logical series. Rust previously kept it flat because `2899` stored frames is not divisible by five positions. The modern XY splitter now accepts sparse interleaved sequence maps, maps declared sequence indexes back to compact stored chunks, and keeps Java's declared per-position logical plane count.
- `but3_cont2002.nd2` is old JP2-backed ND2 with 1400 timestamp entries, `XYPosLoop=5`, and a misleading first `NETimeLoop` count of `321`. Java exposes 5 series, each `sizeC=2`, `sizeT=280`, `imageCount=560`. Rust previously rejected the Java-style loop/cap path and then used a Rust-only timing-gap heuristic that inferred 10 series with `sizeT=140`. Rust now rejects the invalid `321` loop product like before, but the fallback keeps Java's parsed XY count and derives `T=1400/5=280` from the timestamp entries.
- Java creates an ND2 Objective/ObjectiveSettings reference even when objective fields are present but empty or `-1`. Rust now emits the default Objective object and references it for every ND2 series like Java.

Broad batch 04 is fixed and now passes no-pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-04.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified batch-04 result:

- files compared: 5
- core metadata: 5 series OK / 0 bad
- OME metadata: 5 files OK / 0 bad
- pixels: 5 bitwise / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`
- runtime: about 10 seconds with warm Java oracle cache

Fixed batch-04 mismatches:

- `control002.nd2`: line-based TextInfo `Line:...; Excitation wavelength:488; Emission wavelength:520` is now recursively parsed like `ND2Handler.parseKeyAndValue`, so OME excitation/emission match Java.
- `Exception61.nd2`: same TextInfo `Line` parsing fixes excitation metadata. Dimension order now ignores bare Z/T loop descriptors when Java would skip the `uiCount` handler because `ImageMetadataLV` exists, so the final order is Java's `XYZCT`.
- `Exception61.nd2` has a legacy `ImageDataSeq` compressed payload where zlib
  begins at byte 7 relative to Rust's chunk data slice; Rust recognizes that
  legacy compressed prefix when the usual 8-byte prefix is not a codec
  signature. `Exception9_e3.nd2` is different: its raw UINT16 planes use Java's
  normal 8-byte ImageDataSeq skip even when the timestamp value is zero, and the
  default-depth pixel audit now covers that separately.
- `Exception_2.nd2`: Z-stack-only loop/spectrum evidence no longer forces C-first order; it now matches Java's `XYZCT`.
- `130606_JCC719_003.nd2`: the Java `parseText` phase now feeds text `Dimensions` / `Time Loop` / `Z Stack Loop` evidence back into core loop counts before ImageMetadataLV Z-candidate divisibility is tested. This lets the final `T=20, Z=8` text block replace stale `T=22, Z=11` evidence, and the declared-loop image-count reconciliation trims overcomplete mapped chunks to Java's logical 160 planes. Rust now matches Java's `304x232`, `sizeZ=8`, `sizeT=20`, `imageCount=160`, `dimensionOrder=XYCZT`, and 160 OME planes.
- Regression check: `MeOh_high_fluo_003.nd2` still passes focused no-pixel Java parity after narrowing spectral-loop dimension-order evidence.

Focused `290110.tranMgc005cr.nd2` mismatch is fixed and now passes no-pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-290110.path \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified result: 1 file compared, 1 core series OK / 0 bad, OME metadata OK, pixels skipped.

Fixed `290110.tranMgc005cr.nd2` mismatch:

- Java reports `1392x117`, `sizeZ=1`, `sizeC=1`, `sizeT=195`, `imageCount=195`, `uint8`, `dimensionOrder=XYZTC`, indexed=false, and leaves OME channel 0 name unset. Rust now matches this by applying Java-style row correction for the `too_small` first-frame layout, clearing stale LV color/name state on that path, and preserving Java's time-loop-first dimension order when no NDControl order overrides it.

Broad batch 05 is fixed and now passes no-pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-05.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified batch-05 result:

- files compared: 5
- core metadata: 7 series OK / 0 bad
- OME metadata: 5 files OK / 0 bad
- pixels: 7 bitwise / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`
- runtime: about 41 seconds with Java oracle cache writes

Fixed batch-05 mismatches:

- `cag_p5_simgc_2511_70ms22s_crop.nd2`: Java exposes two XY-position series, each `sizeZ=31`, `sizeC=2`, `sizeT=8`, `imageCount=496`; Rust previously stopped at early loop evidence `sizeZ=2`, `imageCount=32`. For split-channel multi-position files, Rust now uses stored-frame count evidence like Java's offset-array normalization: `stored ImageDataSeq / positions / sizeT` can raise `sizeZ` before logical channel expansion.
- `cag_p5_simgc_2511_70ms22s_crop.nd2` and `wtf_DIC.nd2`: Java reports `bitsPerPixel=16`, not 14. Rust now separates `uiBpcSignificant` from `uiBpc`/storage fields; `uiBpc` alone no longer forces significant bits to 14, and Java-style split-channel multi-series rebuilds report storage bits from the `uint16` pixel type.
- Regression checks after this bit-depth change: batch-04 and focused `MeOh_high_fluo_003.nd2` still pass cached no-pixel Java parity.

Broad batch 06 is fixed and now passes no-pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-06.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified batch-06 result:

- files compared: 5
- core metadata: 6 series OK / 0 bad
- OME metadata: 5 files OK / 0 bad
- pixels: 6 bitwise / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`
- runtime: about 15 seconds with warm Java oracle cache

Fixed batch-06 mismatches:

- `divisionByZero/290110.tranMgc005cr.nd2`: Java keeps the camera XML `uiCamPxlCountX/Y` dimensions, then reduces `sizeY` using the available bytes between the first two image offsets. Rust previously fell back to a square `81x81` inference. Rust now reads `uiCamPxlCountX/Y` from the XML camera block and applies the Java row-count correction, yielding Java's `1392x117`.
- The same `divisionByZero` file has corrupt `ImageAttributes` metadata with `uiBpcSignificant=186`, `uiBpcInMemory=72`, and invalid component/dimension values. Java preserves `bitsPerPixel=186` as metadata but still reports `pixelType=uint8`. `ImageMetadata.bits_per_pixel` is now `u16` so the odd Java value can be represented, and ND2 pixel-type inference ignores unsupported storage-bit values instead of treating them as `uint16`.
- `divisionByZero` also now matches Java's `XYZTC` dimension order. XML-only `TimeLoop`/`ZStackLoop` evidence is allowed to seed Java's non-C-first order when no binary `ImageMetadataLV` block is present.
- Pixel follow-up for `divisionByZero/290110.tranMgc005cr.nd2`: Java's
  uncompressed non-split path seeks past the 8-byte frame timestamp and calls
  `FormatReader.readPlane`, whose raw stream read is not capped to the
  `ImageDataSeq` chunk-map length and leaves only truly unread suffix bytes as
  zero. Rust now mirrors that raw-short-frame behavior instead of rejecting
  `162592 < 162864` or padding only the mapped chunk bytes.
- `multicolorTest.nd2`: the experiment XML/TextInfo declare `TimeLoop=13` and `ZStackLoop=15`, but a later `CustomDataVar|NDControl` block carries stale `LoopSize=14,14`. Java keeps the explicit experiment loop counts. Rust now treats NDControl loop sizes as fill-in evidence only when Z/T loop counts are missing, yielding Java's `sizeZ=15`, `sizeT=13`, `sizeC=2`, `imageCount=390`.
- Regression checks after the batch-06 changes: batch-05 cached no-pixel Java parity still passes, `cargo test nd2_ --lib` passes, and `cargo test --no-run` passes for the full test suite.

Broad batch 07 also passes no-pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-07.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified batch-07 result:

- files compared: 5
- core metadata: 12 series OK / 0 bad
- OME metadata: 5 files OK / 0 bad
- pixels: 12 bitwise / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`
- runtime: about 42 seconds with Java oracle cache writes

Broad batch 08 is fixed and now passes no-pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-08.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified batch-08 result:

- files compared: 5
- core metadata: 5 series OK / 0 bad
- OME metadata: 5 files OK / 0 bad
- pixels: 5 bitwise / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`
- runtime: about 76 seconds with Java oracle cache writes

Fixed batch-08 mismatch:

- `zenodo-15493140/169_Cropped.nd2`: Java exposes 57 stored `ImageDataSeq` chunks as `sizeZ=57`, `sizeC=3`, `sizeT=1`, `imageCount=171`. Rust previously accepted a lone NDControl `LoopSize=57` as a time count and then expanded split channels, producing `sizeT=57` and `imageCount=9747`. Rust now collapses this NDControl-only split-channel case back to Java's `T=1` before logical channel expansion. The focused one-file manifest `.tmp/nd2-batches/batch-08-169-cropped.path` passes cached no-pixel parity.
- Regression checks after the batch-08 change: `cargo test nd2_ --lib` passes and `cargo test --no-run` passes.

Broad batch 09 is fixed and now passes bounded pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-09.paths \
BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4 \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified batch-09 result:

- files compared: 5
- core metadata: 15 series OK / 0 bad
- OME metadata: 5 files OK / 0 bad
- pixels: 14 bitwise / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`;
  1 huge series skipped by the 512 MiB full-plane budget
- runtime: about 33 seconds with warm Java oracle cache after compile

Fixed batch-09 mismatches:

- `WellA02_ChannelBrightfield,Cy3 SD_Seq0001.nd2`: Java has no explicit Z/T loop evidence, so the stored `ImageDataSeq` offset count becomes `sizeT=30`; split-channel expansion then yields `sizeC=2`, `imageCount=60`. Rust previously capped the stored image count to the default one Z/T plane before the Java fallback could run. The cap now requires explicit plane-count evidence.
- `C3_N01Ato5_nucleo-trans_div3_mSG_b2s_200ms_30%_Reconstructed.nd2`: Java reports 10 one-plane series with `pixelType=float`, `bitsPerPixel=32`. Rust previously treated `LoopSize=10` as `sizeT=10` in one series and mapped 32-bit ND2 storage to `uint8`. Rust now maps 32-bit ND2 storage to `Float32` like Java's binary `uiBpcInMemory` path and handles reconstructed float `NDControl` loop-size expansion as separate one-plane series.
- `B08.nd2`: Java exposes two series, each `sizeZ=5`, `sizeC=3`, `sizeT=81`, `imageCount=1215`. Rust previously kept one series. Rust now mirrors the Java offset-table compaction case where a large `positionCount` expands the core list but only two contiguous offset groups survive compaction.
- Pixel follow-up for `B08.nd2`: the surviving Java series are not contiguous
  405-frame groups. Java's non-ImageMetadataLV offset placement uses
  `fieldIndex=2` and `FormatTools.rasterToPosition`/`positionToRaster`, so
  series 1 starts at stored frame 5 (`Z` runs before the field axis), not stored
  frame 405. Rust now uses the same raster placement for this compaction path.
- ND2 OME image names now use Java's width-padded series suffix. This matters for ten or more series: Java names `series 01` through `series 10`, not `series 1` through `series 10`.
- Regression checks after the batch-09 changes: `cargo test nd2_ --lib` passes, cached batch-08 no-pixel Java parity still passes, and cached batch-05 no-pixel Java parity still passes.

Broad batch 10 is fixed and now passes no-pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-10.paths \
BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4 \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified batch-10 result:

- files compared: 5
- core metadata: 36 series OK / 0 bad
- OME metadata: 5 files OK / 0 bad
- pixels: 36 bitwise / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`
- runtime: about 44 seconds with Java oracle cache writes

Fixed batch-10 mismatches:

- `WellA03_ChannelBrightfield,Cy3 SD_Seq0002.nd2`: covered by the same no-explicit-Z/T split-channel fallback fixed for `WellA02`; it now matches Java's `sizeT=30`, `sizeC=2`, `imageCount=60`.
- `N2_file_bugged_reconstructed-version.nd2`: covered by the Nikon SIM reconstructed-frame split fixed in batch 09; it matches Java as six one-plane float series.
- `N3_file_nobug_raw-version.nd2` and `N4_file_nobug_raw-version.nd2`: Java exposes raw Nikon SIM frames as one series per stored frame, not as a single time series. Rust now treats `CustomData|NikonSimAcqData` plus NDControl loop evidence as the same Java `LoopSize` core-list expansion path, while preserving the raw `uint16` pixel type.
- Regression checks after the batch-10 changes: `cargo test nd2_ --lib` passes and cached batch-09 no-pixel Java parity still passes.

Broad batch 11 now passes bounded pixel parity with the same Nikon SIM handling:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-11.paths \
BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4 \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified batch-11 result:

- files compared: 5
- core metadata: 44 series OK / 0 bad
- OME metadata: 5 files OK / 0 bad
- pixels: 44 bitwise / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`
- runtime: about 34 seconds with Java oracle cache writes

Broad batch 12 now passes bounded pixel parity:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-batches/batch-12.paths \
BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4 \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified batch-12 result:

- files compared: 2
- core metadata: 2 series OK / 0 bad
- OME metadata: 2 files OK / 0 bad
- pixels: 2 bitwise / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`
- runtime: about 18 seconds with Java oracle cache writes

## Performance parity

Subset timing command used one centered `256x256` plane per exposed series:

```sh
java -cp bioformats_package.jar:bench/target/java BfSubsetBench "$path" 0 1 1 256 256
bench/target/release/bench_subset_rust "$path" 0 1 1 256 256
```

Notable result:

- Before the chunk-map optimization, `100217_OD122_001.nd2` was a real performance-parity problem: Java `156.505ms`, Rust `7136.181ms` for the same one-plane subset benchmark, with matching bytes and series/plane counts. Focused Rust profiling showed the cost was `ImageReader::open` (~7-9s), not pixel read (<1ms).
- Rust now follows Java's chunk-map strategy more closely: trust image chunk-map entries, read the padded image-name header once, keep exact header-derived offsets for non-image metadata chunks, and only do Java-style coarse image-data sanity checks. `100217_OD122_001.nd2` now benchmarks Java `150.577ms`, Rust `64.880ms`; focused Rust open is about `61ms`.
- The raw-row height correction is now gated to raw frame layouts. This preserves the `130606_JCC719_003.nd2` correction while keeping compressed `Exception61.nd2` and `Exception_2.nd2` at Java's declared `sizeY=520`.
- Old-JP2 metadata assembly now follows Java's legacy branch: scan from the
  last `jp2c` codestream to the JPEG EOI marker, then read length-prefixed XML
  metadata blocks. Rust previously decoded and searched the last 64 MiB of the
  file as lossy UTF-8, which made old-JP2 open time much slower and less
  auditable than Java. Focused profiling after the fix: `multipoint.nd2` open
  about `108ms` instead of about `2.5s`; `b16_14_12.nd2` open about `42ms`
  instead of about `960ms`. A guarded tail-text fallback remains for legacy
  files where the Java-shaped block assembly lacks channel/XY-loop evidence;
  this is required for `but3_cont200-1.nd2` and `por003.nd2`.

Current focused subset benchmark after the old-JP2 metadata fix
(`bench/compare_subset.sh --warmup 1 --measure 3 --planes 1 --region 256x256`):

| File | Java ms | Rust ms | Rust speedup | Notes |
| --- | ---: | ---: | ---: | --- |
| `b16_14_12.nd2` | 299.325 | 174.499 | 1.715 | old-JP2 open fixed; remaining cost is JPEG2000 decode |
| `multipoint.nd2` | 3229.389 | 2281.775 | 1.415 | old-JP2 open fixed; 4 series / 4 crops |
| `ns_blobtest2.0_20x_A2_far.nd2` | 94.393 | 354.162 | 0.267 | still slower; modern split-channel full-buffer path remains a performance risk |

Current focused `ns_blobtest2.0_20x_A2_far.nd2` status after gating bulky
Rust-only diagnostics and optimizing the Rust replacement for Java
`ImageTools.splitChannels`:

- Java parity: 1 file compared, 1 core series OK / 0 bad, OME metadata OK, 1
  bitwise pixel comparison / 0 bad with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`.
- Subset benchmark (`--warmup 1 --measure 5 --planes 1 --region 256x256`):
  Java `95.681ms`, Rust `304.883ms`; matching bytes/series/plane counts.
- Focused Rust profile: open about `231ms`, one crop read about `98ms`.
  The read phase improved from about `119ms`, but open time remains the dominant
  performance-parity gap for this modern split-channel file.
- After folding the ImageMetadataLV eType/uiCount scan into the existing
  metadata pass, focused Java parity still passes and Rust profiling is roughly
  the same: open about `226ms`, one crop read about `92ms`. This removes one
  duplicate metadata read/scan and moves the implementation closer to Java's
  single streaming metadata pass, but does not eliminate the remaining open-time
  gap.

Earlier six-file subset benchmark after the chunk-map fix:

| File | Java ms | Rust ms | Rust speedup | Notes |
| --- | ---: | ---: | ---: | --- |
| `BF007.nd2` | 138.140 | 14.230 | 9.708 | Rust faster |
| `HE_002.nd2` | 695.140 | 91.961 | 7.559 | Rust faster |
| `ns_blobtest2.0_20x_A2_far.nd2` | 96.970 | 151.890 | 0.638 | Rust about 1.6x slower |
| `MeOh_high_fluo_003.nd2` | 282.581 | 193.901 | 1.457 | Rust faster |
| `b16_14_12.nd2` | 302.212 | 1022.327 | 0.296 | Open old-JP2 performance risk; focused profile shows open ~719ms, one crop read ~149ms |
| `100217_OD122_001.nd2` | 150.577 | 64.880 | 2.321 | fixed |

## Fixed in this pass

- Modern ND2 chunk-map handling now matches Java more closely. Rust previously sought to every mapped chunk header and then rechecked every image-data chunk, which made `100217_OD122_001.nd2` open in ~7-9 seconds. Java trusts the chunk map, performs only coarse image-data sanity checks, and skips past mapped image data. Rust now keeps exact header-derived offsets for metadata chunks but avoids per-image header seeks by reading the padded image-data name length once.
- Old-JP2 metadata assembly now matches Java's EOI/length-prefixed block flow
  on the fast path instead of always using a Rust-only 64 MiB tail scan. This
  removes the largest old-JP2 performance drift while keeping focused Java
  parity bitwise on `b16_14_12.nd2` and `multipoint.nd2`. The old tail scan is
  now a guarded compatibility fallback only when the Java-shaped text lacks the
  channel or XY-loop evidence needed to match Java on legacy files.
- Bulky Rust-only ND2 diagnostic metadata (`nd2_image_data_sequence_indices`
  and `nd2_image_data_chunk_lengths`) is now emitted only for small/eager
  metadata cases. This preserves synthetic audit tests while avoiding large
  non-Java comma-joined metadata strings for real files such as
  `ns_blobtest2.0_20x_A2_far.nd2`.
- The Rust replacement for Java `ImageTools.splitChannels` now avoids
  per-pixel multiplication and generic slice copies for common 1/2/4-byte
  samples. This keeps the Java-visible full-buffer split branch intact while
  reducing the modern split-channel crop cost.
- The ImageMetadataLV eType/uiCount scan now happens while reading metadata
  chunks for LV/XML parsing. Rust previously made a later extra pass over
  `ImageMetadat*` chunks to derive the same Java loop-order/count state.
- Chunk-map mode now preserves metadata chunks before the first mapped `ImageDataSeq` block, matching Java's scan-until-first-image-data flow. This restores early `ImageMetadataLV` channel descriptions and fixes the `Brightfield` OME channel names in `169_Original.nd2` and `19825_Original.nd2`.
- ND2 chunks now store their true block offset explicitly. This avoids reconstructing block positions from logical names, which is wrong for chunk-map image data with padded 4072-byte name areas.
- Java-style row-height correction now applies only to raw frame layouts. Compressed frames keep their declared dimensions, restoring batch-04 parity for `Exception61.nd2` and `Exception_2.nd2`.
- The single-channel loop-order fallback no longer overrides Java's default order when both Z and T counts are already established. This restores `Exception61.nd2` to Java's `XYZCT`.
- `uiBpcSignificant` now drives `bits_per_pixel`, while `uiBpcInMemory` drives `pixel_type`, matching Java's separation of reported significant bits and storage type.
- `dZHigh` / `dZLow` / `dZStep` are now used to infer `sizeZ` with Java's `ceil(abs(high-low)/step)+1` formula.
- Channel XML `EmWavelength` values are collected for OME emission wavelengths, while generic optical-filter `m_EmissionSpectrum` / `m_ExcitationSpectrum` and channel XML `ExWavelength` are ignored because Java `ND2Handler` does not populate OME wavelengths from those paths.
- `.nd2` suffix dispatch now gives `Nd2Reader` first chance before generic JPEG2000 magic dispatch, matching Java's explicit old-JP2 ND2 branch. This prevents old ND2 files from being exposed as plain JP2 pyramids.
- Old-JP2 loop metadata now reads counts from enclosing `XYPosLoop` / `ZStackLoop` / `TimeLoop` containers, not from `uiCount` child runtype attributes. This fixes `multipoint.nd2` from 220 one-plane series to 4 series with `Z=5`, `T=11`.
- Old-JP2 TextInfo annotations are parsed for Java-style channel names and wavelengths, while broad `sDescription` / spectrum XML promotion is suppressed on the legacy path where Java does not use it.
- ND2 OME metadata now emits all series/images, not only `current_series`, and old-JP2 plane metadata arrays are indexed by global source plane so multi-series OME plane counts match Java.
- The old-JP2 path filters the bogus `dCalibration=50.0` value in `but3_cont200-1.nd2`, matching Java OME output where physical X/Y are unset for that fixture.
- Modern `ImageText*` chunks are now parsed like Java's `ImageText` branch, so TextInfo channel names can populate the primary handler-equivalent channel list.
- Modern `CustomDataVar|NDControl*` chunks are now parsed for Java-style `LoopState` / `LoopSize`; this fixes `sample_image.nd2` from `sizeZ=20`, `imageCount=100` to Java's `sizeZ=21`, `imageCount=105`.
- Modern `CustomDataVar|GrabberCameraSettings*` chunks are now parsed for channel metadata, so RGB camera files such as `MRAP1 KO DN_10X03.nd2` populate Java's `Blue` / `Green` / `Red` OME channel names.
- Modern ND2 `isIndexed` now matches the tested Java behavior: fluorescent channel-color files remain indexed, while RGB uint8 color-camera files do not become indexed just because channel colors are present.
- When the first binary ImageMetadataLV block is used for raster mapping but a later ImageMetadataLV block declares the Z count, Rust now preserves that declared count before falling back to the floating `dZHigh` / `dZLow` range. This fixes `Experiment_0001.nd2` from `sizeZ=16` to Java's `sizeZ=17`.
- Old-JP2 fallback dimensions now infer multi-position time series from repeated `dTimeMSec` cadence when Java loop containers are absent. This fixes `4_2_1_cont_NoMR001.nd2` from 405 series of 2 planes to 5 series with `T=81`, `C=2`.
- Old-JP2 calibration filtering now matches the batch-01 cases: `weekend002.nd2` keeps `PhysicalSizeX/Y=100.0`, while two-channel `por003.nd2` and `but3_cont200-1.nd2` leave physical X/Y unset like Java.
- Old-JP2 loop-count handling now follows Java's first `uiCount` plus final
  `numSeries` cap more closely. Rust no longer accepts exact alternative
  products such as `XYPosLoop=11, T=36` for `4_2_1_cont_NoMR001.nd2`, and it
  rejects cap results that would leave unused planes before falling back to
  timestamp/metadata layout.
- Binary/XML `sDescription` no longer populates primary channel names; it only feeds `textChannelNames` when paired with `uiColor`, matching Java `ND2Reader.iterateIn`.
- Spectral-loop/time-loop evidence preserves Java's `CZT`/`XYCZT` dimension order for the tested single-effective-channel time-series case, while Z-stack-only loop descriptors with `ImageMetadataLV` no longer force C-first order.
- TextInfo `Dimensions`, `Time Loop`, and `Z Stack Loop` annotations now feed dimension inference, and the Java image-count correction increments `sizeT` when the per-series plane count is exactly one Z stack larger than `sizeZ * sizeT`.
- Split-channel multi-position modern ND2 files now use stored-frame count evidence to raise `sizeZ` when early loop metadata undercounts Z; this matches Java's offset-array normalization for `cag_p5_simgc_2511_70ms22s_crop.nd2`.
- XML `uiBpc` is treated as storage/camera bit depth, not OME significant bits. Only `uiBpcSignificant` drives significant-bit reporting, and Java-style split-channel multi-series rebuilds fall back to storage bits from the pixel type.
- Modern multi-position ND2 state now retains Java `pPosName` values through OME population. Rust previously captured the names but cleared the reader field before `ome_metadata()`, so `b16_pdtB+y50__crop.nd2` used synthetic `(series NN)` names instead of Java's `#3` through `#10`.
- The NDControl split-channel collapse is now limited to Java's mismatch case:
  NDControl loop dimensions with no explicit TimeLoop/ZStackLoop descriptors and
  a declared `sizeZ * sizeT` product that does not match the stored plane count.
  This restores cropped files such as `169_Cropped.nd2`, `19825_Cropped.nd2`,
  and `823_Clarkia pulchella_09-1_DIC_SG.nd2` while preserving
  `ns_blobtest2.0_20x_A2_far.nd2` as `Z=3`, `T=44`.
- Single-channel TimeLoop-first dimension order is now applied only when Java is
  in the stale-TimeLoop fallback case where the declared TimeLoop count differs
  from the final inferred `sizeT`. This keeps `3.nd2` at Java's `XYZTC` while
  restoring `Exception61.nd2` to Java's `XYZCT`.
- Normal split-channel Z/T files with explicit TimeLoop/ZStackLoop descriptors
  are no longer overwritten by the NDControl collapse. This restores
  `100217_OD122_001.nd2`, `nikonTest_two.nd2`, `header_test1.nd2`, and
  `multicolorTest.nd2`.
- No-pixel parity now passes for the representative manifest:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/nd2-representative.paths \
cargo test --test java_parity_test java_parity -- --nocapture
```

Verified representative-manifest result:

- files compared: 5
- core metadata: 12 series OK / 0 bad
- OME metadata: 5 files OK / 0 bad
- pixels: skipped by `BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1`
- runtime: about 290 seconds, dominated by `multipoint.nd2`

Also verified individually during this pass:

- `100217_OD122_001.nd2`: core metadata 1/1 OK, OME metadata 1/1 OK, pixels skipped.
- `MeOh_high_fluo_003.nd2`: core metadata 1/1 OK, OME metadata 1/1 OK, pixels skipped.
- `b16_14_12.nd2`: core metadata 1/1 OK, OME metadata 1/1 OK, pixels skipped.
- `but3_cont200-1.nd2`: core metadata 5/5 OK, OME metadata 1/1 OK with 5 images, pixels skipped.
- `multipoint.nd2`: core metadata 4/4 OK, OME metadata 1/1 OK with 4 images, pixels skipped. Runtime was about 252 seconds individually and about 290 seconds in the full representative manifest, so this remains a metadata-performance fixture worth investigating.

Full-corpus no-pixel parity was also verified after these fixes:

- files compared: 62
- core metadata: 202 series OK / 0 bad
- OME metadata: 62 files OK / 0 bad
- pixels: skipped
- runtime: about 256 seconds with warm Java oracle cache after the metadata-pass
  consolidation

Post old-JP2 metadata rewrite verification:

- Focused risk manifest (`b16_14_12.nd2`, `multipoint.nd2`,
  `ns_blobtest2.0_20x_A2_far.nd2`) with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`:
  3 files compared, 6 core series OK / 0 bad, 3 OME files OK / 0 bad, 6
  bitwise pixel comparisons / 0 bad.
- Batch 01 with `BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4`: 5 files compared, 17
  core series OK / 0 bad, 5 OME files OK / 0 bad, 17 bitwise pixel comparisons
  / 0 bad. This specifically rechecks `but3_cont200-1.nd2` and `por003.nd2`,
  which require the guarded tail-text fallback.

## PRs to compare

- PR #5: should be treated as context only for ND2 unless its exact diff is rechecked against Java. Earlier review found it did not by itself provide a 1:1 Java-aligned old-JP2 fix.
- PR #7: claims to fix issue #6. The real-data parity result above shows the required behavior is old-JP2 suffix dispatch, old loop splitting, TextInfo channel extraction, all-series OME emission, and legacy calibration filtering. Any PR #7 change should be compared against those Java anchors before accepting.
- PR #9: most relevant to issue #4 / modern ND2. Its claimed areas overlap the fixed binary `ImageAttributesLV`, significant-bit-depth, Z-range, and wavelength work. Keep only pieces that move Rust closer to Java `ND2Reader` rather than broad rewrites.

## Java source anchors

Upstream Java file: `java-bioformats/components/formats-gpl/src/loci/formats/in/ND2Reader.java`

Important Java regions:

- `initFile`, around lines 409-1180: chunk scanning, binary `ImageAttributes`, binary `ImageMetadataLV`, `setDimensions` guard.
- Around lines 1185-1535: XML handler merge, series heuristics, pixel type/valid bits correction, image count correction, dimension order construction.
- Around lines 1624-1763: `FormatTools.rasterToPosition` mapping from `imageMetadataLVOrder`, compaction of empty offset arrays, and per-series metadata rebuild.
- Around lines 1775-1812: `split = getSizeC() > 1` and timestamp count per split channel model.

## First fix candidates

- Parse binary LV `ImageAttributesLV` fields through the LV tree: `uiWidth`, `uiHeight`, `uiComp`/`uiVirtualComponents`, `uiBpcInMemory`, `uiBpcSignificant`. Initial support added, but it did not change the representative failing files except for the bit-depth separation.
- Preserve Java's distinction between storage pixel type (`uiBpcInMemory`) and reported significant bits (`uiBpcSignificant`). Done for the tested modern ND2 case.
- Verify `parse_image_metadata_lv` starts at the same byte position as Java after `blockType = readString(12)`, `skipBytes(6)`, and zero-sentinel consumption.
- Apply Java's `currentCountSetted && order && timeCount*zCount*XYCount == imageOffsets.size()` guard before trusting LV-derived dimensions.
- Audit old JPEG2000 ND2 handling separately: current Rust reports `resolutionCount=6` and interleaved RGB-like metadata where Java reports normal non-flattened logical planes with `resolutionCount=1`.
