> Historical snapshot retained for provenance. This is not the active backlog.
> See [current implementation status](../../../TOFIX.md).

# Cross-Format Parity Drift Audit List

Purpose: use the ND2 parity drift we found as a checklist for every other file
format represented in the real-world data lists. For each issue below, audit
each format against the matching Java Bio-Formats reader and record defects in
`TOFIX.md` or a format-specific audit note.

This is not an ND2-only list. ND2 is the source of the failure classes; the
audit target is every format for which we have real data.

## Required Audit Protocol

Every file format represented in the real-world data lists must get one
explicit check for each issue in this file. The check can be a source-parity
audit, a focused Java/Rust real-data parity run, or `[n/a]` with a source-based
reason why the issue does not apply to that reader.

For each format:

- identify the Java Bio-Formats reader and the Rust reader/wrapper path
- choose at least one real sample from the fixture manifests or real-data lists
- run metadata parity against Java for the sample set
- run pixel parity for representative files unless the issue is metadata-only
  or the file is too large for the current parity budget
- for lossy JPEG-family decode paths, relax byte-parity checks:
  Java and Rust may use different IDCT/color-conversion and floating-point
  paths, so exact byte equality is not required for JPEG-backed pixel bytes.
  This applies to standalone JPEG/JPEG-2000 readers and container readers that
  delegate JPEG-backed planes such as WSI TIFF wrappers. Metadata, dimensions,
  plane mapping, channel layout, and decoded image shape must still match Java.
  Prefer raw-byte tolerant comparison when Java bytes are emitted.
  If a whole-plane check is CRC-only because Java raw bytes were intentionally
  omitted for output/memory limits, matching length plus passing crop/offset
  tolerant checks is JPEG-relaxed evidence rather than a hard format-parity
  failure. Raise `BIOFORMATS_RS_JAVA_PARITY_FULL_B64_MAX` for a focused
  raw-byte tolerance proof when needed. Record JPEG-family pixel results as
  tolerant or relaxed evidence, not byte-parity proof.
- mark every issue column in the work-record table
- add any `[!] drift found` item to `TOFIX.md` with Java/Rust source anchors
- keep `[~] optimized but parity-proven` only when the optimization is either a
  direct Rust adaptation of a Java library call or proven by real-data parity

Status meanings:

- `[x] matches`: source audit and/or real-data parity show Java-visible parity
- `[!] drift found`: Rust behavior differs from Java and needs a fix
- `[~] optimized but parity-proven`: Rust implementation differs internally but
  Java-visible behavior is proven equivalent for the audited data
- `[n/a] not applicable`: Java reader does not have the feature/failure mode
  for this format

## Scope

- Real-world manifests and fixture lists:
  - `external-fixtures/manifests/fixture_sets.tsv`
  - `external-fixtures/manifests/discovered.tsv`
  - `external-fixtures/manifests/openmicroscopy_images.tsv`
  - `external-fixtures/manifests/ome_images_pending.tsv`
  - `external-fixtures/manifests/ome_sample_roots.tsv`
  - `bench/manifests/*.paths`
  - manifests generated under `.tmp/`
  - any dataset paths referenced from `tests/real_data_test.rs`
  - any format-specific fixture inventories under `external-fixtures/`
- For each file format present in those lists:
  - identify the Java reader and Rust reader/wrapper path
  - run one focused source audit for each issue below
  - run at least one real-data Java/Rust parity check when data exists
  - mark the issue as `[x] matches`, `[!] drift found`, `[~] optimized but
    parity-proven`, or `[n/a] not applicable`

## Real-Data Format Inventory

This inventory is the audit universe. Each category present in the real-world
lists must have one recorded check for each AUDITLIST2 issue 1-10. A category
can map to a native reader, a wrapper reader, a delegated TIFF/XML/HDF5 reader,
or `[n/a]` if the Rust crate intentionally has no corresponding reader.

Initial normalized categories observed across the real-world manifest files
listed above:

```text
amiramesh
bdv
bdv-samples
cellh5
cellomics
cellsens
cellworx
cv7000
cv7000-cpg0016
czi
dcimg
dicom
dv
ecat7
flex
gatan
gateway-tests
gateway_tests
hamamatsu-ndpi
hamamatsu-vms
hcs
ics
ics-jan
imaris-ims
imspector
incell2000
incell3000
jdce
klb
leica-lif
leica-scn
leica-scn-1
leica-xlef
leo
metamorph
metaxpress
micro-manager
mrc
nd2
nifti
nikon-nef
nikon-nrw
nrrd
obf
olympus-fluoview
olympus-oir
ome-tiff
ome-xml
perkinelmer-columbus
perkinelmer-operetta
png
scanr
sdt
sdt-gh-4198
spc-fifo
svs
tiff
trestle
u-track
vectra-qptiff
ventana
zeiss-czi
```

Alias/duplicate inventory names covered by existing rows:

- `bdv-samples` -> `BDV / BigDataViewer`
- `cv7000-cpg0016` -> `CV7000 / Yokogawa`
- `gateway_tests` -> `Gateway tests TIFF/DV`
- `hamamatsu-ndpi` -> `NDPI / Hamamatsu`
- `ics-jan` -> `ICS / IDS`
- `leica-scn-1` -> `Leica SCN`
- `olympus-fluoview` -> `Olympus OIB/OIF`
- `sdt-gh-4198` -> `SDT`
- `zeiss-czi` -> `CZI`

The broad `hcs` inventory bucket is represented by concrete HCS subfamilies in
the table (`Cellomics`, `ScanR`, `Columbus`, `Operetta`, `CellH5`, `InCell`,
`MetaXpress`/`CellWorx`, `CV7000`, and related wrappers) rather than one
separate reader row.

Inventory refresh command:

```sh
awk 'FNR == 1 {next} /^#/ {next} \
  FILENAME ~ /fixture_sets.tsv$/ {v = $2} \
  FILENAME ~ /openmicroscopy_images.tsv$/ {v = $1} \
  FILENAME !~ /fixture_sets.tsv$/ && FILENAME !~ /openmicroscopy_images.tsv$/ {v = $1} \
  v != "" {print tolower(v)}' \
  external-fixtures/manifests/fixture_sets.tsv \
  external-fixtures/manifests/discovered.tsv \
  external-fixtures/manifests/openmicroscopy_images.tsv \
  external-fixtures/manifests/ome_images_pending.tsv \
  external-fixtures/manifests/ome_sample_roots.tsv | sort -u
```

When the inventory changes, add a row to the work record or a tracked
`[n/a]` reason before considering the audit complete.

## ND2 Drift Causes To Check Everywhere

The ND2 fixes exposed these recurring causes of parity drift. These are the
concrete failure modes behind the issue buckets below:

- Chunk-map offsets used the wrong base when Java used the actual chunk name
  length and chunk-header payload length.
- Raw frame readers treated zero timestamp/prefix values as absent, while Java
  still skipped the fixed prefix.
- Old JPEG2000 ND2 confused codec RGB components with logical OME channels.
- Multi-position and split-channel files were flattened or regrouped before
  applying Java's stored-frame and declared-position logic.
- Loop/control metadata was trusted too early when Java later corrected
  dimensions from stronger evidence such as offsets, timestamps, or TextInfo.
- Storage pixel type and significant bit depth were conflated.
- OME channel names, emission/excitation wavelengths, image names, default
  objectives, and all-series OME population used Rust fallbacks instead of
  Java's metadata precedence.
- Row-size correction, scanline padding, short-frame reads, and zero-fill
  behavior were hidden in helpers or implemented differently from Java.
- Rust-only scans and helper layers made source auditing harder and sometimes
  created performance drift.

## Audit Issues

### 1. Chunk/Block Offset And Payload Length Semantics

- Check whether Rust uses the same offset base as Java: block start, header end,
  name end, payload start, or codec stream start.
- Check variable-length block names, two-digit indexes, padded names, sentinel
  bytes, and alignment.
- Check that a container/map entry length is not mistaken for a payload length
  when Java subtracts header/name bytes or reads the actual chunk header.
- Real-data check: compare full-plane and non-zero-origin region CRCs; crops can
  pass while full planes fail.

### 2. Raw Frame Prefix, Timestamp, And Sentinel Handling

- Check whether Java skips fixed per-frame prefixes even when prefix values are
  zero or look invalid.
- Check for Rust-only alternate prefix fallbacks that are not in Java.
- Check full-plane and region paths separately; they often duplicate prefix
  logic and can drift independently.
- Real-data check: include plane 0 and at least one later plane for raw,
  compressed, and mixed-layout files when available.

### 3. Codec Container Layout Versus Logical Channel Layout

- Check whether codec components/bands are RGB samples or logical OME channels.
- Check planar/chunky conversion, RGB/BGR order, split-channel extraction, row
  padding, and depadding order against Java.
- Check old/container-specific codec paths separately from modern raw paths.
- Real-data check: compare RGB metadata as a group: `sizeC`, `rgb`,
  `interleaved`, `rgbChannelCount`, `imageCount`, and pixel CRCs.

### 4. Multi-Position, Multi-Field, And Split-Channel Plane Mapping

- Check Java's logical plane index to stored frame/chunk/IFD mapping.
- Check sparse, missing, duplicate, or non-divisible stored frame counts.
- Check whether Java preserves declared position counts and later compacts empty
  offset arrays rather than rejecting the layout early.
- Check split-channel expansion after, not before, Java's stored-frame grouping.
- Real-data check: include files with multiple positions, fields, wells, scenes,
  tiles, channels, Z, and T.

### 5. Dimension Inference And Metadata Precedence

- Check the exact Java precedence order among binary metadata, XML metadata,
  text annotations, companion files, loop descriptors, offset counts, and final
  image-count correction.
- Check stale early metadata versus later Java-preferred evidence.
- Check dimension-order construction (`XYZCT`, `XYCZT`, `XYZTC`, etc.) after all
  Java fallbacks and corrections.
- Real-data check: compare `seriesCount`, `sizeZ`, `sizeC`, `sizeT`,
  `imageCount`, and `dimensionOrder` for every series.

### 6. Bit Depth, Pixel Type, And Significant Bits

- Check Java's distinction between storage pixel type and significant bits.
- Check whether camera/storage fields should set pixel type only, significant
  bits only, or neither.
- Check 32-bit integer/float paths and reconstructed/processed-data paths.
- Real-data check: compare `pixelType`, bits-per-pixel/significant bits, and
  first-plane CRCs.

### 7. OME Channel And Instrument Metadata Population

- Check channel names, colors, emission wavelengths, excitation wavelengths,
  detector settings, objective settings, and image names.
- Check Java's fallback order among primary metadata, backup/secondary metadata,
  text metadata, and companion metadata.
- Check that Rust creates Java's minimal/default OME graph objects even when
  values are empty or placeholder-like.
- Real-data check: compare OME-XML, not only core metadata.

### 8. Row Size, Padding, Short Frames, And Zero-Fill Behavior

- Check Java row-size correction from available bytes between offsets/chunks.
- Check scanline padding and depadding order.
- Check whether Java accepts short reads and leaves caller buffer bytes as zero
  instead of rejecting.
- Real-data check: include non-standard widths, odd row strides, truncated/short
  frames, and non-zero-origin regions.

### 9. Control/Loop Metadata Should Be Conditional, Not Over-Authoritative

- Check whether loop/control metadata is authoritative, fill-in evidence, or
  ignored when stronger Java evidence exists.
- Check files where loop counts disagree with stored frame counts or explicit
  Z/T descriptors.
- Check whether Java collapses or expands split-channel layouts before exposing
  final logical planes.
- Real-data check: compare files with explicit loops, missing loops, stale loop
  blocks, and processed/reconstructed loop blocks.

### 10. Performance Drift Caused By Non-Java Scans Or Helper Layers

- Check for Rust-only whole-file scans, tail scans, prefix scans, eager decoding,
  per-image header seeks, and large temporary allocations where Java streams or
  trusts indexes.
- Remove or inline Rust-only helpers unless they adapt a Java library call to a
  differently shaped Rust library/API while preserving Java behavior.
- Keep optimized paths only when the source-level equivalence is clear or
  real-data parity proves them.
- Real-data check: compare Java and Rust open time for representative files,
  especially large containers and deep stacks.

## Per-Format Work Record Template

Use one table per format or reader family.

| Format | Java reader | Rust reader | Data source | 1 | 2 | 3 | 4 | 5 | 6 | 7 | 8 | 9 | 10 | Notes |
| --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- | --- |
| ND2 | `ND2Reader` | `src/formats/nd2.rs` | `/big/henriksson/ome_images/ND2` | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | Correctness drift fixed in current checked scopes; one huge-series pixel coverage gap and one performance gap remain noted in `ND2.md`. |
| Native SlideBook7 | `SlideBook7Reader` | `src/formats/flim2.rs` | no real `.sldy`/`.sldyz` fixture found; synthetic native layouts only | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [~] | [~] | Source-backed native subset is now closer to Java: bounded `.dir/*.imgdir` discovery, NPY/NPYZ data-file counting, objective/plane/ROI OME projection, and Java's one-digit sparse `GetChannelIndexOfPath` fallback are covered by `cargo test slidebook7 --lib` (22/22). Remaining bucket is fixture-blocked for full native project coverage until real `.sldy`/`.sldyz` data exists. |
| MRC/IMOD | `MRCReader` | `src/formats/mrc.rs` | `testdata/mrc/EMD-2225.map`; `external-fixtures/data/mrc/*` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Source audit found this reader already mirrors Java's fixed 1024-byte header plus extended-header offset, unconditional lower-left row flip, Java suffix set, EMAN2 unsigned correction, IMOD signed mode-0 flag, RGB mode-16 dimensions, and Java short-read zero-fill behavior. Real-data Java parity passed for `EMD-2225.map`: 1 file, 1 series, OME OK, 128/128 pixel checks bitwise. `cargo test mrc_reader --lib` passed 13/13 targeted tests. `tests/external_fixtures_test.rs` has additional MRC real-data checks, but it is not currently registered as a Cargo test target. |
| NRRD/NHDR | `NRRDReader` | `src/formats/nrrd.rs` | `testdata/nrrd/dt-helix.nrrd`; `external-fixtures/data/nrrd/.../dt-helix.nhdr` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Source audit found raw/detached payload offset, Java-style detached path stripping, scalar/RGB axis mapping, `XYCZT` order, unsigned type mapping, and physical-size projection already aligned. Prior drift for gzip non-zero-origin region reads is fixed by rejecting that request like Java rather than returning silently different bytes. Real-data Java parity passed for inline and detached raw `dt-helix`: 2 files, 2 series, OME OK, 160/160 detailed pixel checks bitwise. `cargo test nrrd --lib` passed 7/7 targeted tests. |
| DICOM | `DicomReader` | `src/formats/dicom.rs` | `testdata/dicom/MR-MONO2-12-angio-an1.dcm`; `testdata/dicom/CT-MONO2-16-chest.dcm` | [x] | [n/a] | [x] | [~] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed AUDITLIST2 issue 3/7 drift: Rust reported monochrome DICOM as interleaved and omitted Java's empty OME plane records. Java leaves `interleaved=false` unless PlanarConfiguration is present and 0, and calls `MetadataTools.populatePixels(..., true)`. Focused Java parity now passes for both local mono DICOM files: 2 files, 2 series, OME OK, 5/5 detailed pixel checks bitwise. `cargo test dicom --lib` passed 18/18. |
| AmiraMesh | `AmiraReader` | `src/formats/amira.rs` | `testdata/amira/test.am` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Upgraded from metadata-only evidence. Focused Java parity now passes for the sampled AmiraMesh fixture: 1 file, 1 core series OK, OME OK, and 8/8 pixel checks bitwise. |
| FITS | `FitsReader` | `src/formats/fits.rs` | `testdata/fits/WFPC2u5780205r_c0fx.fits` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Added explicit sampled row for the FITS category. Focused Java parity passes: 1 file, 1 core series OK, OME OK, and 8/8 pixel checks bitwise. |
| ICS / IDS | `ICSReader` | `src/formats/ics.rs` | `testdata/ics/benchmark_v1.ics` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [~] | [~] | Added explicit sampled row for the ICS category. Focused Java parity passes: 1 file, 1 core series OK, OME OK, and 8/8 pixel checks bitwise. Separate channel-name fixes for richer ICS metadata remain closed in `TOFIX.md`. |
| Andor SIF | `SIFReader` | `src/formats/misc3.rs` | `testdata/sif/image.sif` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Added explicit sampled row for the SIF category. Focused Java parity passes: 1 file, 1 core series OK, OME OK, and 3/3 pixel checks bitwise. |
| Princeton SPE | `SPEReader` | `src/formats/misc3.rs` | `testdata/spe/test_000_.spe` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [~] | [~] | Added explicit sampled row for SPE. Focused Java parity passes: 1 file, 1 core series OK, OME OK, and 4/4 pixel checks bitwise. The richer SPE metadata/ROI projection fix remains closed in `TOFIX.md`. |
| Photoshop PSD | `PSDReader` | `src/formats/psd.rs` | `testdata/psd/sample_rgb.psd` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Added explicit sampled row for PSD. Focused Java parity passes: 1 file, 1 core series OK, OME OK, and 3/3 pixel checks bitwise. The `.psb` suffix overclaim is already closed in `TOFIX.md`. |
| JPEG | `JPEGReader` | `src/formats/jpeg.rs` | `testdata/jpg/scifio-test.jpg` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Added explicit sampled row for standalone JPEG. Focused Java parity passes for metadata and decoded shape: 1 file, 1 core series OK, OME OK, with 2 bounded pixel checks within +/-2 and 1 full-plane CRC-only JPEG-relaxed check. This is tolerant evidence, not byte-parity proof, per the JPEG-family rule above. |
| DCIMG | `DCIMGReader` | `src/formats/dcimg.rs` | `external-fixtures/data/dcimg/.../Cell09_642_000_000.dcimg` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Fixed sampled pixel drift: Rust read DCIMG v1 `frameFooterSize` from `headerSize + 120`, while Java reads it from `headerSize + 124`; this disabled Java's four-pixel footer correction for the real Cell09 sample. Rust now reads the Java offset and has a focused synthetic regression for the correction path. Focused Java parity now passes: 1 file, 1 core series OK, OME OK, and 7/7 pixel checks bitwise. |
| ECAT7 | `Ecat7Reader` | `src/formats/clinical.rs` | `external-fixtures/data/ecat7/.../gradient-512x512x10.v` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Fixed issue 7 drift: Rust omitted the Java OME image name for the sampled `.v` file. `Ecat7Reader::ome_metadata` now fills the image name from the file name when no stronger name exists. Focused Java parity now passes: 1 file, 1 core series OK, OME OK. `cargo test ecat --lib` passed in the focused fix run. |
| KLB | `KLBReader` | `src/formats/misc4.rs` | `testdata/klb/img.klb` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Focused Java parity now passes for the sampled KLB fixture: 1 file, 1 core series OK, OME OK, and 8/8 pixel checks bitwise. |
| NIfTI | `NiftiReader` | `src/formats/nifti.rs` | `testdata/nifti/zstat1.nii` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Upgraded from metadata-only evidence. Focused Java parity now passes for the sampled NIfTI fixture: 1 file, 1 core series OK, OME OK, and 8/8 pixel checks bitwise. |
| OBF/MSR | `OBFReader` | `src/formats/misc4.rs` | `testdata/obf/test-v6-short-write.obf` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed issue 5/7 drift: Rust parsed embedded OME XML with a raw substring helper that matched `SizeX` inside `PhysicalSizeX`, dropped all `<Pixels>` blocks, fell back to raw stack axes, and emitted OME for only the active series. OBF now uses boundary-aware XML event parsing, uses embedded OME Pixels for core size/order, exposes all embedded OME images, and preserves Java-visible raw OME physical-size numeric values for this format. Focused Java parity now passes: 1 file, 7/7 core series OK, OME OK, and 7 series with 9/9 pixel checks bitwise. |
| PNG | `PNGReader` | `src/formats/png.rs` | `testdata/png/scifio-test.png` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Focused Java parity now passes for the sampled PNG fixture: 1 file, 1 core series OK, OME OK, and 3/3 pixel checks bitwise. PNG is not a lossy JPEG-family path, so future pixel runs should require exact byte parity unless a specific codec-library caveat is found. |
| SDT/FLIM | `SDTReader` | `src/formats/flim.rs` | `testdata/sdt/FocalCheck.sdt`; `external-fixtures/data/sdt/.../FocalCheck_A1_20x_8xzoom_800nm.sdt` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [~] | [~] | Focused Java parity now passes for the two sampled SDT fixtures: 2 files, 4/4 core series OK, 2/2 OME OK, and each of the 4 series has 9/9 pixel checks bitwise. |
| SPC-FIFO | `SPCReader` | `src/formats/flim2.rs` | `/big/henriksson/ome_images/SPC-FIFO/biofisika/conv-256x256.spc`; `.set` companion | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [~] | Added mounted real-data row for the previously uncovered SPC-FIFO category. Focused Java parity passes for `conv-256x256.spc`: 1/1 core series OK, OME OK, and 129/129 detailed pixel checks bitwise. This covers FIFO frame/line parsing, companion `.set` metadata, plane count, zero-fill/region behavior, and OME projection for the sampled file. |
| BMP | `BMPReader` | `src/formats/bmp.rs` | `testdata/bmp/scribble_P_RGB.bmp` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Added explicit generic BMP row. Focused Java parity passes for the sampled paletted RGB BMP: 1 file, 1 core series OK, OME OK, and 3/3 pixel checks bitwise. |
| IMAGIC | `ImagicReader` | `src/formats/imagic.rs` | `testdata/imagic/12409.stpm.hed` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Added explicit IMAGIC row. Focused Java parity passes for the sampled `.hed`/`.img` pair: 1 file, 1 core series OK, OME OK, and 7/7 pixel checks bitwise. The minimal-metadata physical-size rule remains closed in `TOFIX.md`. |
| FilePattern / generic TIFF | `FilePatternReader`; `TiffReader` | `src/formats/misc4.rs`; `src/stitcher.rs`; `src/tiff/reader.rs` | `/big/henriksson/ome_images/TIFF/cellprofiler/fruit-fly-cells/POS002.pattern/POS002.pattern` | [x] | [n/a] | [x] | [~] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed issue 7 drift exposed by this sampled pattern: FileStitcher did not project inherited TIFF physical sizes into OME, and generic TIFF physical-size conversion did not match Java's `ResolutionUnit` conversion to microns per pixel. Focused Java parity now passes: 1 file, 1 core series OK, OME OK, and 7/7 pixel checks bitwise. |
| OME-TIFF / OME-XML | `OMETiffReader` / `OMEXMLReader` | `src/formats/ome_xml.rs`; TIFF wrappers | `testdata/ome-tiff/tubhiswt_C0.ome.tif`; `external-fixtures/data/ome-tiff/.../{single-image,multi-pixel-default,multi-pixel-aquired}.ome.tiff` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Upgraded from metadata-only evidence. Focused Java parity now passes for one local and three external small OME-TIFF fixtures: all core metadata and OME OK, local sample 5/5 pixel checks bitwise, and each external sample 2/2 pixel checks bitwise. Shared parsed OME channel-list padding is already tracked in `TOFIX.md` and verified through this reader family. |
| Standalone OME-XML | `OMEXMLReader` | `src/formats/ome_xml.rs`; `src/common/ome_metadata.rs` | `/big/henriksson/ome_images/OME-XML/2009-09/metadata-only.ome.xml`; `single-image.ome.xml`; `multi-channel.ome.xml` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Upgraded the weak OME-XML evidence with standalone `.ome.xml` real-data checks. Metadata-only Java parity passes for 3 mounted OME-XML files: 3/3 core series OK and 3/3 OME OK. Pixel-enabled parity passes for `single-image.ome.xml` and `multi-channel.ome.xml`: 8/8 detailed checks bitwise. `metadata-only.ome.xml` was intentionally metadata-only because it has no external pixel payload in this audit slice. |
| Imaris IMS | `ImarisHDFReader` | `src/formats/imaris_hdf.rs` | `testdata/ims/Convallaria_3C_1T_confocal.ims` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [n/a] | [~] | Fixed AUDITLIST2 issue 7 drift: Rust projected `LSMEmissionWavelength`/`LSMExcitationWavelength` and Rust original-metadata map annotations into OME, while Java parses those wavelength lists but does not set OME Channel emission/excitation fields or map annotations for the sampled IMS. Focused Java parity now passes: 1 file, 1 series, OME OK, 7/7 detailed pixel checks bitwise. `cargo test imaris --lib` passed 12/12. |
| AVI | `AVIReader` | `src/formats/avi.rs` | `testdata/avi/cryper2_newborn.avi` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Fixed AUDITLIST2 issue 3 drift: Rust tied `is_interleaved` to RGB output, while Java sets `interleaved = bmpBitsPerPixel != 16` before deriving RGB/channel count. Focused Java parity now passes: 1 file, 1 series, OME OK, 129/129 detailed pixel checks bitwise. `cargo test avi --lib` passed its matching unit subset. |
| DeltaVision | `DeltavisionReader` | `src/formats/deltavision.rs` | `testdata/dv/P-TRE_12_R3D_D3D.dv` | [x] | [n/a] | [x] | [~] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed AUDITLIST2 issue 7 drift: Java creates `Instrument:0`, links the image, reads header `lensID`, and calls `populateObjective` even without a `.log` companion. Rust only created instrument/objective metadata from `.log` data and emitted Rust-only map annotations. Focused Java parity now passes: 1 file, 1 series, OME OK, 129/129 detailed pixel checks bitwise. `cargo test deltavision --lib` passed 18/18. |
| PIC / Bio-Rad | `BioRadReader` | `src/formats/biorad.rs` | `testdata/pic/sdub1.pic` | [x] | [n/a] | [x] | [~] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed AUDITLIST2 issue 7 drift: Java always creates `Instrument:0` and `Objective:0:0` at full metadata level from header lens/magFactor, then enriches them with note-derived values. Rust only created the graph when note objective metadata or detectors existed. Focused Java parity now passes: 1 file, 1 series, OME OK, 18/18 detailed pixel checks bitwise. `cargo test biorad --lib` passed 17/17. |
| STK / MetaMorph | `MetamorphReader` | `src/formats/metamorph.rs` | `testdata/stk/C0.stk` | [x] | [n/a] | [x] | [~] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed AUDITLIST2 issue 7 drift for the sampled standalone STK: Java creates `Instrument:0` and `Detector:0:0` with type `Other` before per-channel settings, while Rust only projected parsed image/physical/channel fields. Focused Java parity now passes: 1 file, 1 series, OME OK, 15/15 detailed pixel checks bitwise. `cargo test metamorph --lib` passed 20/20. |
| Gatan DM3/DM4 | `GatanReader` | `src/formats/gatan.rs` | `testdata/dm3/clem_fig3b.dm3`; `testdata/gatan/SmallMontage0000.dm4` | [x] | [n/a] | [n/a] | [~] | [x] | [x] | [x] | [x] | [n/a] | [~] | Fixed AUDITLIST2 issue 7 drift for DM3: Java calls `MetadataTools.populatePixels(..., true)` and therefore emits empty OME Plane records even when no position/exposure values are present. Rust only emitted planes when optional per-plane values existed. Focused Java parity now passes for `clem_fig3b.dm3`: 1 file, 1 series, OME OK, 2/2 pixel checks bitwise. `cargo test gatan --lib` passed 7/7. |
| LEO SEM TIFF | `LEOReader` | `src/formats/sem.rs`; TIFF wrappers | `/big/henriksson/ome_images/LEO/dmankus/2160grating 512.tif` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Added mounted real-data row for the previously uncovered LEO category. Focused Java parity passes for the sampled SEM TIFF: 1/1 core series OK, OME OK, and 3/3 detailed pixel checks bitwise. Source risk remains for richer LEO private-tag variants, but no sampled AUDITLIST2 issue drift is present. |
| CZI | `ZeissCZIReader` | `src/formats/zeiss_czi.rs`; `src/common/ome_metadata.rs` | `testdata/czi/Plate1-Blue-A-25.czi` | [x] | [n/a] | [x] | [~] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed AUDITLIST2 issue 7 drift for the sampled CZI: Java creates an OME instrument graph from CZI XML and calls `MetadataTools.populatePixels(..., true)`, while Rust previously projected only image/channel/physical-size metadata and emitted no instrument children or planes. Rust now creates the sampled Java graph counts from scoped CZI XML plus default ZCT planes. Focused Java parity now passes: 1 file, 1 series, OME OK, 127/127 detailed pixel checks bitwise. |
| Nikon NEF | `NikonReader` | `src/formats/camera2.rs`; `src/tiff/nikon.rs`; `src/registry.rs` | `external-fixtures/data/nikon-nef/raw.pixls.us/data/Nikon/D70/20170902_0047.NEF` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Fixed sampled issue 7 drift found with the downloaded raw.pixls D70 NEF: Rust switched to the Java merged data SubIFD for core dimensions and pixels but kept generic TIFF physical-size metadata from the wrong/default series, so OME omitted `PhysicalSizeX/Y` while Java emitted `84.66666666666667` um/pixel from 300 dpi TIFF resolution tags. Rust now derives X/Y physical sizes from the active Nikon data IFD using the TIFF resolution unit conversion and projects them into OME. Focused Java parity now passes for the D70 sample: 1/1 core series OK, OME OK, and 2/2 pixel checks bitwise. |
| Nikon NRW | no tracked Bio-Formats Java reader suffix found for `.nrw` | `src/formats/camera2.rs` if TIFF/Nikon byte-detected; registry has no `.nrw` suffix route | `external-fixtures/data/nikon-nrw/raw.pixls.us/data/Nikon/Coolpix P7000/RAW_NIKON_P7000.NRW` | [n/a] | [n/a] | [n/a] | [n/a] | [n/a] | [n/a] | [n/a] | [n/a] | [n/a] | [n/a] | Downloaded the manifest raw.pixls P7000 NRW sample and confirmed it is valid little-endian TIFF/Nikon data with the expected 16,058,749-byte size. The tracked Java checkout has no `.nrw` suffix reference, and the focused Java oracle rejects the file before comparison with `FormatException: No IFDs found`. Do not add Rust-only `.nrw` suffix support for parity; treat this category as unsupported by the tracked Java reader unless upstream Java adds NRW handling. |
| CV7000 / Yokogawa | `CV7000Reader` | `src/formats/extended.rs`; `src/registry.rs`; `src/reader_order.rs` | `userdata/openmicroscopy-images/CV7000/cpg0016/Dest21053D1-15214/Dest210531-152149.wpi`; `.tmp/auditlist2-cv7000.paths` | [x] | [n/a] | [x] | [x] | [x] | [~] | [x] | [x] | [x] | [x] | Source audit and Rust unit coverage confirm the sampled issue 7 fix: Rust now emits one OME channel per acquired channel name using Java's `Action #..., Channel #..., Camera #...` string and `SamplesPerPixel=1`. Rust now also registers/routes `.wpi` to `YokogawaReader`, matching Java's suffix-based `CV7000Reader` dispatch. The focused real-data Java oracle still cannot compare this WPI because upstream Java `CV7000Reader.initFile` leaves `channels` null when `MeasurementDetail.mrf` is absent and then calls `channels.sort(...)`, throwing `NullPointerException` before metadata is emitted. `cargo test cv7000 --lib` passes the channel-name and `.wpi` dispatch tests; the manifest rerun records zero compared files with the same Java NPE. |
| Flex | `FlexReader` | `src/formats/flex.rs` | `testdata/flex/001001000.flex`; `.tmp/auditlist2-channel-ome-local.paths` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Added fixture-backed issue 7 proof for the sampled multi-series Flex file after the channel-list/name/wavelength audit flagged this as source-checked but not row-proven. Focused Java parity now passes: 1 file, 3 core series OK, OME OK with 3 images, and each of the 3 series has 5/5 pixel checks bitwise. |
| Zeiss LSM/ZVI | `ZeissLSMReader` / `ZeissZVIReader` | `src/formats/zeiss_lsm.rs`; `src/formats/zeiss_zvi.rs` | `testdata/lsm/colocsample1b.lsm`; `testdata/zvi/fig3d_wt_sting_cd31.zvi`; `.tmp/auditlist2-channel-ome-local.paths` | [x] | [n/a] | [x] | [~] | [x] | [x] | [x] | [x] | [~] | [~] | Added fixture-backed issue 7 proof for the sampled LSM and ZVI channel-list/name/wavelength paths after the channel audit flagged them as source-checked but not row-proven. Focused Java parity now passes for the available LSM/ZVI samples: 2 Zeiss files, 2 core series OK, 2 OME documents OK, LSM 8/8 pixel checks bitwise, and ZVI 7/7 pixel checks bitwise. A refreshed fixture scan found no additional `.lsm`, `.zvi`, or Zeiss `.mdb` candidates locally or under `/big/henriksson/ome_images`; the remaining source-backed LSM gap is MDB companion grouping/parsing after Java-compatible `.mdb` detection. |
| Leica LIF | `LeicaReader` | `src/formats/lif.rs` | `testdata/lif/PR2729.lif`; `.tmp/auditlist2-channel-ome-local.paths`; `.tmp/auditlist2-lif-broader.paths` | [x] | [n/a] | [x] | [~] | [x] | [x] | [x] | [x] | [~] | [~] | Sampled local issue 7 proof still passes for the two-channel LIF path. Broader real-data LIF drift is fixed for the checked metadata surface: RGB sample count is Java-visible via `lif.rgb_channel_count`, Java null-vs-empty channel names are preserved, laser-derived channel excitation is projected, Java label ROI shapes are represented, and Java's two filter passes are mirrored. Metadata-only Java parity now passes for `.tmp/auditlist2-lif-broader.paths`: 7 files, 34/34 core series, and 7/7 OME documents; pixels were intentionally skipped for this metadata regression check. |
| Leica LEI/raw | `LeicaReader` | `src/formats/leica.rs`; `src/registry.rs` | no real `.lei`/Leica `.raw` fixture found; synthetic LEI/raw and LEI/TIFF layouts only | [x] | [n/a] | [x] | [~] | [x] | [x] | [x] | [x] | [~] | [~] | Source-backed LEI/raw parity improved: top-level `.raw` with a sibling `.lei` now pre-dispatches to `LeicaReader` like Java's `.raw` suffix entry path, while existing tests cover flat raw planes, TIFF companion lookup, channel names, pinhole/color/filter/objective/detector/stage/time OME projection. Remaining bucket is fixture-blocked for real LEI graph/pixel proof. |
| Olympus OIB/OIF | `FV1000Reader` | `src/formats/olympus.rs` | `/big/henriksson/ome_images/Olympus-FluoView/imagesc-71616/20220824_4492_cord_dapi__iba568_60x.oib`; `testdata/oib/cry11_colocalization.oib`; `testdata/oif/Source Data Figure S5c-d CTRL.oif`; `.tmp/auditlist2-fv1000-broader.paths` | [x] | [n/a] | [x] | [~] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed sampled issue 3/7 drift plus a broader mounted-OIB issue 7 drift: Java reads `LutFileName*` LUTs, reports indexed core metadata, creates default FV1000 OME graph/plane records, sets a default 1 s time increment, parses legacy numeric text such as `0,207.0`, and derives physical Z spacing from Axis Common start/end spans. Rust now loads channel LUTs, selects LUT by logical plane channel, creates Java-style FV1000 graph/plane scaffolding from active channels, parses FV1000 doubles like Java's tolerant path, and computes Z/T spacing from axis spans. Broader focused parity now passes for 3 files: 3 core series OK, 3 OME OK, and bounded pixel parity has 3 bitwise series with 0 failures. |
| MetaXpress / CellWorx HTD | `MetaxpressTiffReader` / `CellWorxReader` | `src/formats/hcs2.rs`; `src/formats/mias.rs`; `src/formats/metamorph.rs` | `.tmp/auditlist2-metaxpress-mounted.paths`; `/big/henriksson/ome_images/MetaXpress` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed sampled issue 3/7/8 drift in mounted MetaXpress HTDs: registered MetaXpress now mirrors Java's flat sorted-directory fallback for UUID-suffixed TIFF companions and nested TimePoint/ZStep fallback, HTD parsing tolerates invalid UTF-8 like Java's permissive text read, OME delegates through the inherited CellWorx assembly for every image, and template physical sizes now come from MetaMorph UIC `XCalibration`/`YCalibration` instead of generic TIFF 72 dpi values. Refreshed metadata-only Java parity on 2026-08-14 still passes for 5 mounted HTDs: 4,808 core series OK and 5 OME files OK. Pixels were intentionally skipped for this metadata/wrapper regression check. Focused real-data tests assert the UIC `1.6125` calibration on the underlying TIFF and the HTD OME wrapper. |
| SVS / Aperio | `SVSReader` | `src/formats/svs.rs`; `src/tiff/reader.rs` | `testdata/svs/CMU-1-Small-Region.svs` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | Fixed sampled issue 3/4/5/7 drift: Rust now exposes Java default flattened public series for SVS (`seriesCount=3`, all `resolutionCount=1`) while preserving the internal pyramid for decoder/tile paths; label/macro classification now happens before Java-style stripped-thumbnail removal; OME emits Java-style flattened image ordering, one plane per image, and the sampled instrument/objective graph. Focused Java parity now passes for `CMU-1-Small-Region.svs`: 3 core series OK, OME OK, 1 bitwise pixel series and 2 JPEG-tolerant series with 0 pixel failures. |
| NDPI / Hamamatsu | `NDPIReader` | `src/formats/tiff_wrappers.rs`; `src/tiff/reader.rs` | `testdata/ndpi/CMU-1.ndpi` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | Fixed sampled issue 3/4/5/7 drift: Rust now exposes Java default flattened public series for NDPI (`seriesCount=5`, all `resolutionCount=1`) while preserving the internal pyramid and 64-bit offset/tile optimizations. Active flattened metadata derives `is_interleaved` from the mapped IFD, matching Java's NDPI `MAX_SIZE` custom-decoder threshold per public series. Focused Java parity now passes for `CMU-1.ndpi`: 5 core series OK, OME OK, the huge full-resolution plane skipped by budget, and 4 JPEG-tolerant pixel series with 0 failures. |
| Hamamatsu VMS | `HamamatsuVMSReader` | `src/formats/extended.rs` | `/big/henriksson/ome_images/Hamamatsu-VMS/openslide/CMU-1/CMU-1-40x - 2010-01-12 13.24.05.vms` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed sampled issue 7 drift found in this audit: Rust emitted three original-metadata map annotations from VMS series metadata, while Java emitted none. Rust no longer adds those annotations in the VMS OME builder. Refreshed metadata-only Java parity passes: 3/3 core series OK and OME OK. Pixels were intentionally skipped because the full-resolution and map series are large JPEG-backed whole-slide images; future VMS pixel checks should use JPEG-tolerant region comparisons. |
| Leica SCN | `LeicaSCNReader` | `src/formats/tiff_wrappers.rs`; `src/tiff/reader.rs` | `testdata/scn/Leica-1.scn` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | Fixed sampled issue 3/4/5/7 drift: Rust now exposes Java default flattened public series for SCN (`seriesCount=8`, all `resolutionCount=1`) while preserving internal image/resolution grouping for pixel dispatch. OME images are emitted per flattened resolution with Java-style `name (Rr)` naming. Focused Java parity now passes for `Leica-1.scn`: 8 core series OK, OME OK, one huge series skipped by budget, and 7 JPEG-tolerant pixel series with 0 failures. |
| Leica XLEF/LMS | `XLEFReader` / LMS readers | `src/formats/flim2.rs`; `src/formats/leica_lms.rs` | `external-fixtures/data/leica-xlef/.../XLEF-LOF 2020_06_23_sample 3 U2OS.xlef`; `bench/manifests/leica-xlef-benchmark.paths`; `.tmp/leica-xlef-closure.paths`; `.tmp/leica-xlef-tau.paths`; `.tmp/xlef-extra-candidates.paths` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed sampled issue 7 drift in mounted XLEF/LMS OME projection: Rust now emits Java `initFilterModels` filter objects beyond filter 0, counts channel light-path filters like Java when no explicit filter list exists, creates Java baseline OME planes for every image plane, and matches Java's projected/raster physical-size handling for oversized `Unit=m` XLIF dimensions. Rust now also preserves raw LMS laser/exWave/active-detector arrays and replays Java's XLEF project-store excitation projection, including persistent `nextChannel` behavior and exWave override. Java parity passes for the 3-project benchmark: 17 core series OK, 3 OME OK, and 17/17 series bitwise; focused tau-selection parity passes for `.tmp/leica-xlef-tau.paths`: 2 files, 10/10 core series OK, 2/2 OME OK. Public OME fixture parity now also passes for the downloaded `2020_06_23_sample 3 U2OS` XLEF-LOF project: 1 file, 1 core series `1024x1024 z8 c6 t1 uint8 ic=48`, OME OK, and 9/9 bounded pixel checks bitwise. The current closure manifest has no Java-comparable drift; an extra 2026-08-14 bounded pixel probe of four 2020/Lysosomes projects also passes with 16/16 core series, 4/4 OME documents, and 16/16 checked pixel series bitwise. Remaining closure is fixture/oracle-limited for Java-skipped annotated-stage/MaF entries and broader pixel-heavy storage layouts such as compressed/multiple LMS `Memory`/`Storage` raw layouts. |
| Olympus cellSens VSI/ETS | `CellSensReader` | `src/formats/flim2.rs` | `testdata/vsi/HN 485 HNSCC APOBEC3A-1.1000.vsi` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | Fixed sampled issue 3/4/5/7 drift: Rust now exposes Java default flattened public series for VSI/ETS (`seriesCount=9`, all `resolutionCount=1`) while preserving internal ETS `(volume, resolution)` tile dispatch. Rust also stopped emitting CellSens Rust-only OME original-metadata map annotations and now emits Java-style default plane records for every public image. Focused Java parity now passes for `HN 485 HNSCC APOBEC3A-1.1000.vsi`: 9 core series OK, OME OK, the huge full-resolution plane skipped by budget, and 8 JPEG-tolerant pixel series with 0 failures when `BIOFORMATS_RS_JAVA_PARITY_FULL_B64_MAX=2097152` allows tolerant full-plane comparison. |
| Ventana BIF | `VentanaReader` | `src/formats/tiff_wrappers.rs`; `src/tiff/reader.rs` | `userdata/openmicroscopy-images/Ventana/openslide/Ventana-1.bif` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | Fixed sampled issue 3/4/5/7 drift: Rust now exposes Java default flattened public series for BIF (`seriesCount=10`, all public `resolutionCount=1`) while preserving internal stitched pyramid dispatch by mapping public pyramid series back to inner series 0 with the selected inner resolution. Focused metadata-only Java parity now passes for `Ventana-1.bif`: 10 core series OK and OME OK. The preceding pixel-enabled run, before only the OME-name correction, had 10 core series OK and 0 pixel failures (2 bitwise, 7 JPEG-tolerant, one huge series skipped by budget). |
| BDV / BigDataViewer | `BDVReader` | `src/formats/bdv.rs` | `testdata/bdv/HisYFP-SPIM.h5`; `userdata/openmicroscopy-images/BDV/samples/drosophila.h5` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | Fixed sampled issue 3/4/5/7 drift: Rust now exposes Java default flattened public setup-resolution series for BDV (`HisYFP-SPIM.h5` `seriesCount=34`, `drosophila.h5` `seriesCount=8`, all `resolutionCount=1`) while preserving internal logical setup/resolution grouping for HDF5 reads. OME image order and names now use Java's `P_tNNNNN, W_sSS_L` pattern. Focused metadata-only Java parity now passes for both files: 42 core series OK and 2 OME files OK. Bounded pixel smoke with `MAX_PLANES=2` passed the harness; remaining small downsampled-HDF5 byte differences are tracked as the existing BDV/libhdf5 scaleoffset decode caveat, not public-series drift. |
| Trestle TIFF | `TrestleReader` | `src/formats/hcs2.rs` | `/big/henriksson/ome_images/Trestle/openslide/CMU-1/CMU-1.tif` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | Fixed sampled issue 3/4/5/7 drift: Rust now matches Java's default flattened public-series view for Trestle (`seriesCount=7`, all `resolutionCount=1`) while preserving internal overlap-aware level mapping for pixel reads. OME now emits one image per public flattened series and clears non-base physical sizes to match the Java oracle. Metadata-only Java parity passes for CMU-1: 7/7 core series OK, OME OK; pixels were intentionally skipped for this HCS/wrapper metadata pass. |
| Olympus ScanR | `ScanrReader` | `src/formats/hcs2.rs` | `bench/manifests/scanr-benchmark-smoke.paths` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | Fixed sampled issue 7 drift: assembled HCS readers now override OME metadata instead of falling back to the current series only. ScanR now emits Java-style all-series OME images, baseline planes, image names, channel names, physical sizes, and Plate/Well/WellSample graph. Metadata-only Java parity now passes for `experiment_descriptor.xml`: 357 core series OK and OME OK. Pixels were intentionally skipped for this OME/SPW regression check. |
| PerkinElmer Columbus | `ColumbusReader` | `src/formats/hcs2.rs` | `bench/manifests/perkinelmer-columbus-benchmark.paths` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | Fixed sampled issue 7 drift: assembled HCS readers now build Java-style all-series OME images, baseline planes, image names, channel names/wavelengths, physical sizes, and Plate/Well/WellSample graph. Metadata-only Java parity now passes for `MeasurementIndex.ColumbusIDX.xml`: 3696 core series OK and OME OK. Pixels were intentionally skipped for this OME/SPW regression check. |
| PerkinElmer Operetta | `OperettaReader` | `src/formats/hcs2.rs`; `src/registry.rs` | `external-fixtures/data/perkinelmer-operetta/.../Images/Index.ref.xml` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | Fixed sampled issue 3/7 drift: Operetta XML entry points now dispatch before broad XML suffix readers, HTTP/missing payload URLs remain metadata-comparable like Java, fallback core defaults match Java when no valid TIFF IFD is available, and OME now includes Java-style image names, physical sizes, objective graph, channel names/wavelengths, default planes, and Plate/Well/WellSample graph. Metadata-only Java parity passes for `Index.ref.xml`: 384/384 core series OK, OME OK. Pixels were skipped because this fixture references HTTP/missing image payloads. |
| Cellomics C01/DIB | `CellomicsReader` | `src/formats/extended.rs` | `external-fixtures/data/cellomics/.../AS_09125_050118150001_A03f00d0.DIB` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed sampled issue 7 drift: Cellomics now emits Java-style all-series OME images, default planes, channel names/wavelengths/colors, and Plate/Well/WellSample graph, and no longer emits a Rust-only original-metadata map annotation for the sampled DIB. Focused Java parity passes: 3/3 core series OK, OME OK, and each of the 3 series has 3/3 pixel checks bitwise. |
| CellH5 | `CellH5Reader` | `src/formats/cellh5.rs` | `/big/henriksson/ome_images/CellH5/idr0013/00001_01.ch5`; `/big/henriksson/ome_images/CellH5/samples/full/{0013,0041,0042}.ch5` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [~] | [~] | Upgraded from metadata-only evidence. Focused Java parity passes for the sampled IDR CH5: 1/1 core series OK (`1344x1024 z1 c1 t93 uint8 ic=93`), OME OK, and bounded pixel parity reports 5/5 checks bitwise. Broader mounted metadata-only parity also passes for `0013.ch5`, `0041.ch5`, and `0042.ch5`: 3/3 core series OK (`1392x1040 z1 c2 t206 uint8 ic=412`) and 3/3 OME OK. `_all_positions.ch5` is not Java-comparable in this mount because Java cannot resolve its Windows external HDF5 links. |
| Micro-Manager | `MicromanagerReader` | `src/formats/micromanager.rs` | `/big/henriksson/ome_images/Micro-Manager/1.4.16/serge/Pos0/metadata.txt` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | Fixed sampled issue 5 drift: Java accepts unquoted JSON booleans such as `"SlicesFirst": false` when deriving dimension order, while Rust only handled string/integer forms. Focused metadata-only Java parity now passes for the sampled `Pos0` metadata: 1/1 core series OK and OME OK; pixels were skipped in this companion-metadata check. |
| InCell3000 public BMP sample | `BMPReader` on the public BBBC image; `InCell3000Reader` for native `.frm` | `src/formats/bmp.rs`; `src/formats/hcs2.rs` | `external-fixtures/data/incell3000/.../Channel1-01-A-01.BMP` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | The available InCell3000 manifest sample is a standalone BMP and compares through the generic BMP path: metadata-only Java parity passes for 1/1 core series and OME OK. Native `.frm` source parity is separately covered by the InCell3000 suffix fix in `TOFIX.md`; real native `.frm` data is still needed for a full pixel/layout audit. |
| InCell2000 / native InCell XDCE | `InCellReader` | `src/formats/incell.rs`; TIFF/BMP delegates | `/big/henriksson/ome_images/InCell2000/zenodo-14777242/Training_Demo_1.xdce` | [x] | [n/a] | [x] | [~] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed sampled issue 5/7 drift: native XDCE XML now honors Java-compatible XML-declared encodings such as ISO-8859-1, and OME now emits Java-style default Plane records for every image plane. Focused Java parity passes for the mounted XDCE: 8/8 core series OK (`1024x1024 z16 c4 t1 uint16 ic=64`), OME OK with 8 images and 512 planes, and bounded pixel parity reports 8 bitwise series with 3/3 checks each. |
| Native InCell3000 FRM | `InCell3000Reader` | `src/formats/hcs2.rs` | `/big/henriksson/ome_images/InCell3000/BBBC013/BBBC013_v1_images_frm/20041103 1049_01_REF-1049-03 - EvoTec_0_A10_0.frm` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [n/a] | [~] | Native FRM is no longer fixture-blocked for the sampled BBBC013 frame. Focused Java parity passes: 1/1 core series OK (`640x640 z1 c1 t1 uint16 ic=1`), OME OK with Java's minimal single-image graph, and bounded/full/offset pixel checks are bitwise. |
| MetaImage | no tracked Bio-Formats Java reader | `src/formats/metaimage.rs` | `testdata/mha/HeadMRVolume.mhd` | [n/a] | [n/a] | [n/a] | [n/a] | [n/a] | [n/a] | [n/a] | [n/a] | [n/a] | [n/a] | Java parity is not applicable in the tracked Bio-Formats checkout: no MetaImage reader was found, and `tests/java_parity_test.rs` excludes `testdata/mha/HeadMRVolume.mhd` because there is no oracle reader. Treat this as a Rust-only supported format unless upstream Java adds a reader. |
| Imspector OBF/MSR | `ImspectorReader` | `src/formats/extended.rs` (`ImspectorReader`) | `/big/henriksson/ome_images/Imspector/zenodo-10476252/test.msr` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed sampled issue 5/7 drift: Java reads later MSR metadata record `xyz-Table Z Resolution=1` and collapses raw `Z=200,T=1` to `Z=1,T=200`; Rust now mirrors that branch and populates Java-style OME image name from the file basename. Focused Java parity passes on the mounted MSR: 1/1 core series OK, OME OK, and 9/9 bounded pixel checks bitwise. |
| Olympus OIR | `OIRReader` | `src/formats/flim2.rs` | `/big/henriksson/ome_images/Olympus-OIR/gh-4205/zenodo-12773657/DAPI_mCherry_22Lambda-420-630-w10nm-s10nm.oir` | [x] | [n/a] | [x] | [~] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed sampled issue 7 drift: native OIR XML now feeds Java-style instrument graph counts instead of hardcoded Rust detector/light-source defaults. Focused Java parity passes on the sampled native OIR: metadata-only run covers core/OME parity, and bounded pixel run reports `512x512 z1 c22 t1 uint16 ic=22`, OME OK, and 9/9 pixel checks bitwise. |
| Vectra QPTIFF | `VectraReader` | `src/formats/extended.rs` | `/big/henriksson/ome_images/Vectra-QPTIFF/perkinelmer/PKI_fields/HnE_3_1x1component_data.tif` | [x] | [n/a] | [x] | [x] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed sampled issue 3/4/5/7 drift earlier: Rust now matches Java's QPTIFF pyramid/thumbnail topology and OME image ordering for the sampled file. Focused Java parity now passes with pixels enabled: 2/2 core series OK, OME OK, and both series pass bounded pixel checks bitwise (`s0` float 3/3 checks, `s1` uint8 3/3 checks). |
| Gateway tests TIFF/DV | `TiffReader`; `DeltavisionReader` | `src/tiff/reader.rs`; `src/formats/deltavision.rs` | `/big/henriksson/ome_images/gateway_tests/big.tiff`; `tinyTest.d3d.dv` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [~] | [~] | Added explicit gateway/gateway_tests row. Metadata-only Java parity passes for `big.tiff` and `tinyTest.d3d.dv`: 2 files, 2/2 core series OK, 2/2 OME OK. Pixel-enabled parity passes for `tinyTest.d3d.dv`: 60/60 detailed checks bitwise. `big.tiff` was kept metadata-only in this pass because it is 8192x8192 and the current gateway issue slice was dispatch/metadata; run bounded regions separately for TIFF pixel-performance closure. |
| JDCE Molecular Devices | `JDCEReader` | `src/formats/misc4.rs` | `/big/henriksson/ome_images/JDCE/molecular-devices/Converted/*/*.JDCE` | [x] | [n/a] | [x] | [n/a] | [x] | [x] | [x] | [x] | [~] | [~] | Fixed source-audited issue 7 drift. Java writes Plate/Well/WellSample graph, image names, physical sizes, channel names/emission/excitation wavelengths, and per-plane positions/timing from the JDCE JSON/CSV metadata. Rust now carries the parsed JDCE plate/channel/plane fields through the assembled series and emits an all-series OME override. A focused unit test covers the OME projection. 2026-08-14 refresh: `cargo test jdce --lib -- --nocapture` passes the OME projection test, but both mounted JDCE samples still fail in the Java oracle before comparison with `IllegalArgumentException: 0 must be non-null and strictly positive`; JDCE therefore remains source/unit-backed rather than Java-oracle-proven. |
| u-track ZIP | `ZipReader` delegating to `OMETiffReader` | `src/formats/zip.rs`; `src/formats/ome_xml.rs`; TIFF wrappers | `/big/henriksson/ome_images/u-track/integrins.zip` | [~] | [n/a] | [~] | [n/a] | [~] | [~] | [~] | [~] | [n/a] | [~] | Java oracle is blocked on this mounted archive: Java `ZipReader` maps inner names and then errors resolving `case1_higherSNR.ome.tiff` as a working-directory file before metadata is emitted; a 2026-08-14 metadata-only oracle refresh reproduced the same `FileNotFoundException`. Source audit shows Rust extracts archive entries to a temp directory and delegates the primary entry to `ImageReader`, which is an intentional Rust-library adaptation of Java's mapped-file ZIP delegation. Rust smoke via `bioformats-convert` opens and reads the archive as 101x101, 20 planes, uint16. This row is not Java-parity-proven; keep it as delegation/source-backed until a Java oracle with mapped ZIP entries can compare it. |

## Metadata-Only Sweep Backlog

Command:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS=1 \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Current focused HCS/wrapper metadata batch after the Operetta, Cellomics, and
Trestle fixes: 6 files compared, 398 core series OK / 0 bad, 6 OME files OK /
0 bad, pixels skipped, and no deeper-check findings. The batch covers
Cellomics DIB, the public InCell3000 BMP sample, Operetta `Index.ref.xml`,
CellH5, Trestle CMU-1, and Vectra QPTIFF.

Prior cached metadata-only sweep after the XLEF/LMS OME fixes: 45 files
compared, 184 core series OK / 0 bad, 45 OME files OK / 0 bad, pixels skipped,
and no deeper-check findings. The CV7000 WPI sample remains skipped because the
Java reader throws `NullPointerException` before producing oracle metadata; the
focused CV7000 rerun against `.tmp/auditlist2-cv7000.paths` reproduces the same
upstream-Java null `channels.sort(...)` failure. A 2026-08-14 unit refresh
passed `cargo test cv7000 --lib -- --nocapture` for the channel-name, OME
projection, and `.wpi` dispatch coverage.

Result before the DICOM fix: 45 files compared, 38 core series OK / 42 bad,
25 OME files OK / 20 bad, pixels skipped. The sweep failed by design on
metadata drift and exposed these main cross-format targets:

- CZI: core metadata matched for `Plate1-Blue-A-25.czi`, but OME instrument,
  objective, detector, filter, dichroic, and plane metadata were missing. Fixed
  for the sampled file in `src/formats/zeiss_czi.rs` and
  `src/common/ome_metadata.rs`: focused Java parity now reports OME OK and
  127/127 detailed pixel checks bitwise. Remaining CZI work is deeper
  field-level enrichment that the current comparator does not serialize, such
  as detector refs, light paths, objective settings, and per-subblock plane
  position/time/exposure values.
- DICOM: monochrome files reported `interleaved=true`; MR also lacked Java's
  empty OME plane record. Fixed in `src/formats/dicom.rs`.
- Imaris IMS: Rust populated channel emission/excitation wavelengths where Java
  left them unset for `Convallaria_3C_1T_confocal.ims`. Fixed in
  `src/formats/imaris_hdf.rs`.
- Whole-slide TIFF wrappers and BDV: SVS, NDPI, SCN, VSI, Ventana BIF, and BDV
  are fixed for the sampled real files and now pass focused Java metadata/OME
  parity in Java's default flattened public-view mode.
- Olympus OIB/OIF sampled core indexed/LUT and OME graph/plane count gaps are
  fixed for `cry11_colocalization.oib` and
  `Source Data Figure S5c-d CTRL.oif`. Broader mounted FV1000 coverage also
  exposed and fixed physical-size drift for legacy decimal text and Z spacing.
  Remaining Olympus risk is detailed field-level enrichment beyond what the
  current comparator serializes. The sampled DeltaVision default objective
  graph gap, PIC default instrument/objective graph gap, STK default detector
  graph gap, and Gatan DM3 plane-count gap are fixed; remaining Gatan risk is
  montage/ROI fidelity.
- AVI: RGB/interleaved metadata differs for `cryper2_newborn.avi`. Fixed in
  `src/formats/avi.rs`.
- BDV: Java flattened resolution behavior is fixed for local BDV samples
  (`HisYFP-SPIM.h5`, `drosophila.h5`), including series count, dimensions,
  image count, resolution count, OME image count, and image names. Remaining
  sampled pixel differences are the existing BDV/libhdf5 scaleoffset decode
  caveat.
- Flattened-resolution bucket: SVS, SCN, NDPI, VSI, Ventana BIF, and BDV were
  mostly Java default flattened-view drift rather than pixel-decoder drift.
  Java `FormatReader` defaults `flattenedResolutions=true`, so public
  `seriesCount` is the flattened `core` length and each public series has
  `resolutionCount=1`. Rust now exposes that public view for the sampled
  readers by mapping public series to `(logical_series, resolution)` while
  retaining the optimized internal pyramid/BDV structures for dispatch.
- Olympus OIB/OIF sampled indexed/LUT and graph/plane count drift is fixed.
  A broader mounted OIB physical-size drift is also fixed. Continue auditing
  detailed FV1000 fields such as objective settings, detector settings,
  filter/dichroic models, light paths, and per-plane position/time overrides
  against additional real-world files.
- CV7000 WPI path: Java oracle currently throws a `NullPointerException`; treat
  this as an oracle/upstream-Java comparability issue until separately audited.

## Next Audit Priorities

After the mounted MetaXpress/CellWorx pass, the next highest-risk unfinished
audit slices are:

1. Leica XLEF/LMS broader fixture closure: the current
   `bench/manifests/leica-xlef-benchmark.paths` bounded-pixel pass is green,
   the focused `.tmp/leica-xlef-tau.paths` metadata-only pass is green after
   the Java-style project-store excitation replay, and the broader
   `.tmp/leica-xlef-closure.paths` metadata-only pass is green for all 4
   Java-comparable Snail/tau-selection entries. The remaining closure caveat is
   fixture/oracle comparability: 24 annotated-stage/MaF entries are skipped by
   the Java oracle as invalid image-reference projects rather than compared.
2. Channel-list/name/wavelength proof for richer custom OME-builder variants:
   the sampled Zeiss LSM/ZVI row is now Java-parity-proven, but no additional
   `.lsm`, `.zvi`, or Zeiss `.mdb` candidate was found locally or under
   `/big/henriksson/ome_images`. The remaining LSM work is MDB companion
   grouping plus rich ROI/event/application/channel-wavelength fixtures.
   Broader Leica LIF is now row-checked and fixed for the checked filter/name
   metadata drift in `TOFIX.md`. Nikon Elements TIFF is source-checked and covered by synthetic
   unit tests for channel list/name/emission/excitation projection, but the
   current mounted manifests do not contain a real NIS-Elements TIFF fixture to
   compare against Java. CV7000/WPI is source-audited and unit-checked, but the
   available real WPI remains blocked by upstream Java's missing
   `MeasurementDetail.mrf` NPE.
3. Rich companion/layout gaps without fixture closure: Zeiss LSM `.mdb` or
   multi-`.lsm` datasets, native SlideBook7 real projects, Leica LEI/raw
   companions, and the BDV downsampled HDF5 pixel caveat. The latest fixture
   sweep found only a generic Java MDB service test file, not Zeiss LSM MDB
   data, and no native SlideBook7 `.sldy`/`.sldyz` projects.

## Per-Format Evidence Log

### MRC/IMOD

- Source anchors:
  - Java: `java-bioformats/components/formats-gpl/src/loci/formats/in/MRCReader.java`
    `isThisType`, `initFile`, and `openBytes`.
  - Rust: `src/formats/mrc.rs` `is_this_type_by_name`,
    `is_this_type_by_bytes`, `set_id`, `open_bytes`, and
    `open_bytes_region`.
- AUDITLIST2 issues covered:
  - 1: MRC uses a fixed 1024-byte header plus Java's extended-header offset;
    Rust uses the same `HEADER_SIZE + extHeaderSize + no * planeSize` model.
  - 3: RGB mode 16 uses RGB samples, not logical non-RGB channels, matching
    Java's `rgb=true`, `sizeC=3`, `imageCount=sizeZ` behavior.
  - 5: Dimensions and metadata come from the same MRC header fields Java reads;
    Rust also records the Java-style named header metadata keys.
  - 6: Rust matches Java mode-to-pixel-type behavior, including EMAN2 unsigned
    correction and IMOD signed mode-0 handling.
  - 7: MRC has minimal OME metadata; the real-data Java parity run proves the
    current OME projection for the sampled file.
  - 8: Rust mirrors Java's unconditional lower-left row flip and Java's
    short-read behavior where unavailable bytes remain zero-filled.
  - 10: No Rust-only helper layer was found that changes Java-visible behavior;
    existing helper code is local parsing/byte-order code or a Rust adaptation
    of Java header reads.
- Commands:

```sh
printf '%s\n' 'testdata/mrc/EMD-2225.map' > .tmp/auditlist2-mrc.paths
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/auditlist2-mrc.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
cargo test mrc_reader --lib
```

- Result:
  - Java parity: 1 file compared, 1 core series OK / 0 bad, 1 OME file OK,
    pixels 1 bitwise / 0 bad, 128/128 detailed pixel checks.
  - Unit tests: 13/13 `mrc_reader` tests passed.
- Unrun evidence:
  - `BIOFORMATS_RS_EXTERNAL_FIXTURES=1 cargo test external_mrc_ --test
    external_fixtures_test -- --nocapture` could not run because
    `external_fixtures_test` is not a registered Cargo test target.

### NRRD/NHDR

- Source anchors:
  - Java: `java-bioformats/components/formats-bsd/src/loci/formats/in/NRRDReader.java`
    `initFile`, `openBytes`, and `readGZIPPlane`.
  - Rust: `src/formats/nrrd.rs` `parse_nrrd_header`,
    `nrrd_axes_from_header`, `read_nrrd_payload`, `open_bytes`, and
    `open_bytes_region`.
- AUDITLIST2 issues covered:
  - 1: Rust uses the same inline header-end data offset and detached data-file
    byte-skip behavior as Java; detached path resolution also mirrors Java's
    leading-directory stripping for single `data file` values.
  - 3: Scalar/RGB axis handling and `sizeC`/RGB metadata match Java for the
    sampled vector NRRD fixture.
  - 5: Rust follows Java's fixed `XYCZT` dimension order and axis inference for
    spatial, color, list, and time axes.
  - 6: Rust maps NRRD signed/unsigned 8/16/32-bit type names to Java's unsigned
    Bio-Formats pixel types.
  - 7: OME metadata for the sampled inline and detached fixtures matches Java.
  - 8: Raw full-plane and region reads match Java on real data. The known gzip
    non-zero-origin region drift is fixed by returning a clean error for the
    Java-incompatible request; this prevents silently returning different
    region bytes.
  - 10: No Java-visible helper drift found; decompression/full-inflate helpers
    are Rust-library adapters with explicit Java-behavior guards.
- Commands:

```sh
printf '%s\n' \
  'testdata/nrrd/dt-helix.nrrd' \
  'external-fixtures/data/nrrd/downloads.openmicroscopy.org/gordon/dt-helix.nhdr' \
  > .tmp/auditlist2-nrrd.paths
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/auditlist2-nrrd.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
cargo test nrrd --lib
```

- Result:
  - Java parity: 2 files compared, 2 core series OK / 0 bad, 2 OME files OK,
    pixels 2 bitwise / 0 bad, 160/160 detailed pixel checks.
  - Unit tests: 7/7 `nrrd` tests passed.

### DICOM

- Source anchors:
  - Java: `java-bioformats/components/formats-bsd/src/loci/formats/in/DicomReader.java`
    `initFile` handling of `PLANAR_CONFIGURATION`, `MetadataTools.populatePixels`,
    and OME plane population.
  - Rust: `src/formats/dicom.rs` `parse_dicom_attrs`, `build_metadata`, and
    `ome_metadata`.
- AUDITLIST2 issues covered:
  - 1: Raw pixel offsets and frame strides were already Java-aligned for the
    two local mono fixtures; focused full-plane/region CRCs pass.
  - 3: Fixed codec/logical-layout metadata drift. Rust now leaves
    `is_interleaved=false` unless the DICOM Planar Configuration tag is present
    and equals 0, matching Java.
  - 5/6: Core dimensions, pixel type, and bits-per-pixel match Java for local
    MR and CT fixtures after the interleaving fix.
  - 7: Fixed OME plane-list shape. Rust now creates one DICOM OME Plane per
    image plane like Java's `MetadataTools.populatePixels(..., true)`, then
    overlays parsed position values when present.
  - 8: Full-plane and region pixel checks pass bitwise for both fixtures.
  - 9: Grouping/loop logic is covered by existing DICOM unit tests, but broader
    multi-file real-data parity remains a separate audit target.
- Commands:

```sh
printf '%s\n' \
  'testdata/dicom/MR-MONO2-12-angio-an1.dcm' \
  'testdata/dicom/CT-MONO2-16-chest.dcm' \
  > .tmp/auditlist2-dicom.paths
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/auditlist2-dicom.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
cargo test dicom --lib
```

- Result:
  - Java parity: 2 files compared, 2 core series OK / 0 bad, 2 OME files OK,
    pixels 2 bitwise / 0 bad, 5/5 detailed pixel checks.
  - Unit tests: 18/18 `dicom` tests passed.

### Imaris IMS

- Source anchors:
  - Java: `java-bioformats/components/formats-gpl/src/loci/formats/in/ImarisHDFReader.java`
    `parseAttributes` and OME population around channel color/name handling.
  - Rust: `src/formats/imaris_hdf.rs` `parse_ims_file` and `ome_metadata`.
- AUDITLIST2 issues covered:
  - 1/3/4/5/6/8: The sampled IMS has matching core metadata and bitwise pixel
    checks for full-plane, crop, and offset reads.
  - 7: Fixed OME metadata drift. Java parses `LSMEmissionWavelength` and
    `LSMExcitationWavelength` into internal lists but does not project them to
    OME Channel emission/excitation fields. Rust now keeps the Java-visible OME
    fields unset and no longer adds Rust-only original-metadata map annotations
    for this reader.
  - 10: Removing the Rust-only OME projection and annotation layer improves
    source auditability; remaining HDF5 helper logic adapts Java HDF5 library
    calls to the Rust HDF5 reader.
- Commands:

```sh
printf '%s\n' 'testdata/ims/Convallaria_3C_1T_confocal.ims' \
  > .tmp/auditlist2-imaris.paths
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/auditlist2-imaris.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
cargo test imaris --lib
```

- Result:
  - Java parity: 1 file compared, 1 core series OK / 0 bad, 1 OME file OK,
    pixels 1 bitwise / 0 bad, 7/7 detailed pixel checks.
  - Unit tests: 12/12 `imaris` tests passed.

### AVI

- Source anchors:
  - Java: `java-bioformats/components/formats-bsd/src/loci/formats/in/AVIReader.java`
    `initFile` metadata derivation and `openBytes` row/padding logic.
  - Rust: `src/formats/avi.rs` `parse_avi`, `set_id`, `open_bytes`, and
    `open_bytes_region`.
- AUDITLIST2 issues covered:
  - 1/8: RIFF chunk offsets, BMP row padding, bottom-up row order, and region
    reads pass bitwise against Java for the local AVI fixture.
  - 3: Fixed interleaving metadata drift. Java sets `interleaved` from
    `bmpBitsPerPixel != 16` independently of whether the final image is RGB.
    Rust now uses the same rule.
  - 5/6/7: Core dimensions, frame count, pixel type, and OME metadata match
    Java for the sampled indexed/grayscale AVI.
  - 10: Existing helper functions correspond to RIFF parsing and codec
    adapters; no Java-visible helper drift remains for the sampled path.
- Commands:

```sh
printf '%s\n' 'testdata/avi/cryper2_newborn.avi' \
  > .tmp/auditlist2-avi.paths
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/auditlist2-avi.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
cargo test avi --lib
```

- Result:
  - Java parity: 1 file compared, 1 core series OK / 0 bad, 1 OME file OK,
    pixels 1 bitwise / 0 bad, 129/129 detailed pixel checks.
  - Unit tests: `cargo test avi --lib` passed 32/32 matching tests.

### Gatan DM3/DM4

- Source anchors:
  - Java: `java-bioformats/components/formats-gpl/src/loci/formats/in/GatanReader.java`
    `initFile`, especially `MetadataTools.populatePixels(store, this, true)`
    and the subsequent instrument/objective/detector/plane metadata projection.
  - Rust: `src/formats/gatan.rs` `set_id`, `ome_metadata`,
    `select_physical_sizes`, `acquisition_mode_from_info`, and
    `roi_shapes_to_ome_rois`.
- AUDITLIST2 issues covered:
  - 1/8: Pixel offsets, payload length, and bounded reads pass Java parity for
    the sampled DM3 file.
  - 4: Montage splitting logic remains source-aligned; existing unit coverage
    exercises split-montage series mapping against Java behavior.
  - 5/6: Core dimensions, image count, pixel type, physical sizes, and signed
    correction pass Java parity for `clem_fig3b.dm3`.
  - 7: Fixed default OME plane-count drift. Java's
    `MetadataTools.populatePixels(..., true)` creates one Plane per logical
    image even when no position or exposure fields are populated. Rust now
    always emits `meta.image_count` planes and overlays optional Gatan
    position/exposure values when present.
  - 10: Existing helper functions are parsing/projection adapters with Java
    source anchors; no Java-visible helper drift remains for the sampled DM3
    path.
- Commands:

```sh
printf '%s\n' 'testdata/dm3/clem_fig3b.dm3' > .tmp/auditlist2-dm3.paths
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/auditlist2-dm3.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
cargo test gatan --lib
```

- Result:
  - Java parity: 1 file compared, 1 core series OK / 0 bad, 1 OME file OK,
    pixels 1 bitwise / 0 bad, 2/2 detailed pixel checks.
  - Unit tests: 7/7 `gatan` tests passed.

### PIC / Bio-Rad

- Source anchors:
  - Java: `java-bioformats/components/formats-gpl/src/loci/formats/in/BioRadReader.java`
    `initFile` and `parseNotes`.
  - Rust: `src/formats/biorad.rs` `set_id`, `parse_notes`, and
    `ome_metadata`.
- AUDITLIST2 issues covered:
  - 1/8: PIC header, note/LUT boundary handling, grouped PIC file mapping, and
    pixel reads pass Java parity for the sampled file.
  - 3/4/5/6: Core dimensions, image count, channel grouping, pixel type, and
    indexed LUT behavior pass Java parity for `sdub1.pic`.
  - 7: Fixed default OME graph drift. Java always creates `Instrument:0` and
    `Objective:0:0` in full metadata mode, with objective lens NA from header
    `lens` and nominal magnification from header `magFactor`, then note parsing
    may overwrite/enrich objective and detector values. Rust now creates that
    default graph and still overlays note-derived objective/detector settings.
  - 10: Existing helper functions are local ports of Bio-Rad note parsing,
    companion grouping, and LUT handling; no Java-visible helper drift remains
    for the sampled PIC path.
- Commands:

```sh
printf '%s\n' 'testdata/pic/sdub1.pic' > .tmp/auditlist2-pic.paths
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/auditlist2-pic.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
cargo test biorad --lib
```

- Result:
  - Java parity: 1 file compared, 1 core series OK / 0 bad, 1 OME file OK,
    pixels 1 bitwise / 0 bad, 18/18 detailed pixel checks.
  - Unit tests: 17/17 `biorad` matching tests passed.

### STK / MetaMorph

- Source anchors:
  - Java: `java-bioformats/components/formats-gpl/src/loci/formats/in/MetamorphReader.java`
    `initFile`, `initStandardMetadata`, `parseUIC1Tags`, `parseUIC2Tags`,
    `parseUIC4Tags`, and the per-plane metadata loop.
  - Rust: `src/formats/metamorph.rs` `read_metamorph_data`,
    `emit_metamorph_data_metadata`, `init_single_stk`, and `ome_metadata`.
- AUDITLIST2 issues covered:
  - 1/8: TIFF-backed pixel offsets and bounded reads pass Java parity for the
    sampled standalone STK file.
  - 4/5/6: Core Z/C/T dimensions, image count, dimension order, pixel type, and
    UIC-derived metadata pass Java parity for `C0.stk`.
  - 7: Fixed sampled OME graph drift. Java creates `Instrument:0` and
    `Detector:0:0` with detector type `Other` before applying per-channel
    detector settings. Rust standalone STK now creates that scaffold, links the
    image to the instrument, and links channels to `Detector:0:0`, while keeping
    existing parsed binning/channel/plane metadata.
  - 10: Existing UIC parsing helpers are source-anchored ports. Larger
    MetaMorph risks remain for light-source/objective enrichment in other
    fixtures, but no Java-visible drift remains for sampled `C0.stk`.
- Commands:

```sh
printf '%s\n' 'testdata/stk/C0.stk' > .tmp/auditlist2-stk.paths
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/auditlist2-stk.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
cargo test metamorph --lib
```

- Result:
  - Java parity: 1 file compared, 1 core series OK / 0 bad, 1 OME file OK,
    pixels 1 bitwise / 0 bad, 15/15 detailed pixel checks.
  - Unit tests: 20/20 `metamorph` matching tests passed.

### CZI

- Source anchors:
  - Java: `java-bioformats/components/formats-gpl/src/loci/formats/in/ZeissCZIReader.java`
    `initFile`, `translateMetadata`, `translateExperiment`,
    `translateInformation`, `translateHardwareSettings`, and
    `SubBlock.parseMetadata`.
  - Rust: `src/formats/zeiss_czi.rs` `parse_czi_file`,
    `build_czi_channels`, and `ome_metadata`.
- AUDITLIST2 issues covered:
  - 7: Fixed sampled Java OME graph population drift. Java calls
    `MetadataTools.populatePixels(store, this, true)` and then populates
    instrument/objective/detector/filter/dichroic metadata from scoped CZI XML.
    Rust now creates Java-style default Plane records for each logical ZCT plane
    and populates the sampled CZI graph counts from `Information/Instrument` and
    `HardwareSetting/ObjectiveChanger`.
  - 4/5: Per-plane position/time/exposure enrichment depends on Java's
    `indexIntoPlanes` mapping from logical ZCT planes to `SubBlock` metadata.
    Rust `DirEntry` does not yet retain Java-equivalent `stageX`, `stageY`,
    `stageZ`, `timestamp`, or `exposureTime` fields.
  - 10: Keep existing CZI block/pixel optimizations. The fix should add
    source-aligned metadata projection and subblock metadata retention, not
    replace the decoded-pixel path.
- Fix strategy:
  - Done for sampled comparator parity: create baseline OME planes for each
    logical ZCT plane and scoped XML graph counts for instrument, objectives,
    detectors, filters, and dichroics.
  - Remaining broader parity work: preserve Java-equivalent per-subblock metadata alongside each directory
    entry: stage X/Y/Z, acquisition timestamp, exposure time, and resolved
    series/plane/resolution mapping.
  - Port the remaining CZI XML graph field population: detector settings,
    light paths, channel detector refs, objective settings refs, and full
    objective/filter/dichroic attributes.
- Commands:

```sh
printf '%s\n' 'testdata/czi/Plate1-Blue-A-25.czi' > .tmp/auditlist2-czi.paths
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/auditlist2-czi.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

- Result:
  - Java parity: 1 file compared, 1 core series OK / 0 bad, 1 OME file OK,
    pixels 1 bitwise / 0 bad, 127/127 detailed pixel checks.

### DeltaVision

- Source anchors:
  - Java: `java-bioformats/components/formats-gpl/src/loci/formats/in/DeltavisionReader.java`
    `initFile`, extended-header plane metadata loop, log parsing, and
    `populateObjective`.
  - Rust: `src/formats/deltavision.rs` `set_id`, extended-header parsing, and
    `ome_metadata`.
- AUDITLIST2 issues covered:
  - 7: Fixed Java default OME graph/objective drift. Java calls
    `MetadataTools.populatePixels(store, this, true)`, creates `Instrument:0`,
    links every image to it, reads `lensID` from header offset 162, and always
    calls `populateObjective(store, lensID)`. Rust now reads `lensID`, creates
    the default instrument/objective graph, links the image, applies the sampled
    Java `lensID=10002` objective row, merges `.log` objective metadata into the
    same objective slot, and suppresses the Rust-only map annotation.
  - 4/8: Rust already reads extended headers and projects plane
    delta/exposure/stage data for sampled paths; the fix should preserve that
    code and only add missing default graph/objective scaffolding plus baseline
    planes where extended-header lookup is sparse.
  - 10: Keep the existing extended-header plane parsing and row/plane optimized
    reads. A full port of the Java `populateObjective` switch remains useful
    for broader lens coverage; the sampled fixture is now parity-proven.
- Commands:

```sh
printf '%s\n' 'testdata/dv/P-TRE_12_R3D_D3D.dv' > .tmp/auditlist2-dv.paths
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/auditlist2-dv.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
cargo test deltavision --lib
```

- Result:
  - Java parity: 1 file compared, 1 core series OK / 0 bad, 1 OME file OK,
    pixels 1 bitwise / 0 bad, 129/129 detailed pixel checks.
  - Unit tests: 18/18 `deltavision` matching tests passed.

### Olympus OIB/OIF

- Source anchors:
  - Java: `java-bioformats/components/formats-gpl/src/loci/formats/in/FV1000Reader.java`
    `initFile`, `populateMetadataStore`, `ChannelData.getFilter`,
    `mapOIBFiles`, `getIniFile`, and `parseROIFile`.
  - Rust: `src/formats/olympus.rs` `OifChannel`, OIB/OIF metadata parsing, and
    `ome_metadata`.
- AUDITLIST2 issues covered:
  - 3/7: Fixed sampled core indexed/LUT drift. Java collects `LutFileName*`,
    reads one 16-bit RGB LUT per channel from the tail of each `.lut` file, and
    sets `indexed = lut != null`; Rust previously hard-coded `indexed=false`.
    Both sampled OIB/OIF files now have core metadata parity.
  - 7: Fixed sampled OME graph/plane count drift. Java creates `Instrument:0`,
    links the image, sets default `Pixels TimeIncrement=1 s`, creates baseline
    planes, and builds detector/light-source/filter/two-dichroic graph entries
    from active `ChannelData`. Rust now mirrors that scaffold from active
    channels while preserving existing channel name/wavelength projection.
  - 7: Fixed broader mounted-OIB physical-size drift. Java's FV1000 path accepts
    legacy numeric text such as `0,207.0` for X/Y convert values and computes
    physical Z spacing from `Axis N Parameters Common` `EndPosition -
    StartPosition` divided by `SizeZ - 1` and converted from nm to µm. Rust now
    applies equivalent tolerant FV1000 double parsing and Axis Common Z/T
    spacing.
  - 4/5: Java per-plane metadata comes from PTY axis metadata and acquisition
    fields; Rust currently only projects image name, physical sizes, and
    channel name/emission/excitation.
  - 10: Keep OIB stream mapping and INI parsing; fix graph projection and LUT
    parity in stages.
- Fix strategy:
  - Extend `OifChannel` with Java `ChannelData` fields: active state, gain,
    voltage, dye, barrier filter, emission/excitation filter names, and
    illumination metadata.
  - Parse `Laser N Parameters`, `Channel N Parameters`, `GUI Channel N
    Parameters`, and per-plane PTY axis fields used by Java.
  - Create Java FV1000 OME graph: `Instrument:0`, PMT detectors, lasers,
    filters, dichroics, objective, objective settings, light paths, baseline
    planes, and time increment.
  - Done: add Java-style LUT loading so OIB/OIF indexed core metadata matches
    Java, including BGR-byte to 16-bit RGB conversion and channel-specific
    `lookup_table(plane_index)` selection.
  - Done: build sampled Java FV1000 OME graph and plane metadata scaffold.
  - Done: parse legacy FV1000 double text and derive physical Z/T spacing from
    Axis Common spans.
  - Remaining broader parity work: enrich detailed objective settings, detector
    settings, filter/dichroic models, light paths, and per-plane position/time
    overrides from PTY/channel metadata where Java serializes those fields.
- Commands:

```sh
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES='oib/cry11_colocalization.oib,oif/Source Data Figure S5c-d CTRL.oif' \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/auditlist2-fv1000-broader.paths \
BIOFORMATS_RS_JAVA_PARITY_MAX_PLANES=4 \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
cargo test fv1000 --lib
```

- Result:
  - Original sampled Java parity: 2 files compared, 2 core series OK / 0 bad,
    2 OME files OK, pixels 2 bitwise / 0 bad, 138/138 detailed pixel checks.
  - Broader mounted FV1000 Java parity: 3 files compared, 3 core series OK / 0
    bad, 3 OME files OK, pixels 3 bitwise / 0 bad with bounded
    `MAX_PLANES=4`.
  - Unit tests: `cargo test olympus --lib` passes 18/18, including FV1000
    LUT/indexed and legacy-double parsing tests.

### Whole-Slide TIFF Wrappers And BDV Flattened Resolutions

- Source anchors:
  - Java: `java-bioformats/components/formats-api/src/loci/formats/FormatReader.java`
    default `flattenedResolutions = true`, plus wrapper readers for
    `SVSReader`, `NDPIReader`, `LeicaSCNReader`, `CellSensReader`,
    `VentanaReader`, and `BDVReader`.
  - Rust: `src/formats/svs.rs` `SvsReader`, `src/formats/tiff_wrappers.rs`
    `NdpiReader`, `LeicaScnReader`, and `VentanaReader`,
    `src/formats/flim2.rs` `CellSensReader`, and `src/formats/bdv.rs`
    `BdvReader`.
- AUDITLIST2 issues covered:
  - 3/4/5: The main drift is public series/resolution mapping, not decoded pixel
    bytes. Java default mode flattens each pyramid/BDV resolution into a public
    series with `resolutionCount=1`; Rust now exposes this public view for the
    sampled SVS, NDPI, SCN, VSI, Ventana BIF, and BDV files.
  - 7: OME image ordering and image names drift as a consequence of public
    flattened ordering. Associated images such as label/macro/overview are
    ordered after flattened pyramid resolutions in Java default mode. The
    sampled readers now emit Java-style flattened OME image counts/order.
  - 10: Keep the existing optimized tile/offset/HDF5 paths. The fix should be a
    public-view mapping layer that resolves `(series, resolution)` before pixel
    dispatch rather than rewriting decoder internals; the current fixes follow
    that pattern.
- Java default observations:
  - SVS `CMU-1-Small-Region.svs`: 3 public series, all `resolutionCount=1`.
  - SCN `Leica-1.scn`: 8 public series, all `resolutionCount=1`.
  - NDPI `CMU-1.ndpi`: 5 public series, all `resolutionCount=1`.
  - VSI sample: 9 public series, all `resolutionCount=1`.
  - Ventana `Ventana-1.bif`: 10 public series, all `resolutionCount=1`.
  - BDV `HisYFP-SPIM.h5`: 34 public series, all `resolutionCount=1`.
  - BDV `drosophila.h5`: 8 public series, all `resolutionCount=1`.
- Fix strategy used:
  - Added default-flattened public view layers for the sampled readers that
    support resolution pyramids. Public series map to `(logical_series,
    resolution)` or the reader-specific equivalent.
  - Preserved the current internal pyramid structures for optimized tile,
    offset, ETS, and HDF5 dispatch.
  - Flattened metadata and OME image ordering per Java default mode:
    SVS pyramid resolutions then label/macro; NDPI pyramid resolutions then
    macro/mask/map; SCN XML image resolutions in Java order; Ventana pyramid
    resolutions then overview/label extras; VSI ETS resolutions then
    macro/overview; BDV setup-resolution pairs in Java `core` order.
  - BDV bounded pixel parity still shows the known small HDF5 scaleoffset decode
    differences on some downsampled levels; this is distinct from the fixed
    public-series mapping drift.
- Remaining follow-up:
  - Add dual-mode parity checks: the current Java default flattened oracle and
    a non-flattened oracle path that calls `setFlattenedResolutions(false)`
    before `setId`.

### Current Compact Verification

This manifest covers the format rows completed in this file after the DICOM,
Imaris IMS, AVI, DeltaVision, PIC, STK, Gatan DM3, CZI, and Olympus FV1000
fixes:

```sh
printf '%s\n' \
  'testdata/mrc/EMD-2225.map' \
  'testdata/nrrd/dt-helix.nrrd' \
  'external-fixtures/data/nrrd/downloads.openmicroscopy.org/gordon/dt-helix.nhdr' \
  'testdata/dicom/MR-MONO2-12-angio-an1.dcm' \
  'testdata/dicom/CT-MONO2-16-chest.dcm' \
  'testdata/ims/Convallaria_3C_1T_confocal.ims' \
  'testdata/avi/cryper2_newborn.avi' \
  'testdata/dv/P-TRE_12_R3D_D3D.dv' \
  'testdata/pic/sdub1.pic' \
  'testdata/stk/C0.stk' \
  'testdata/dm3/clem_fig3b.dm3' \
  'testdata/czi/Plate1-Blue-A-25.czi' \
  'testdata/oib/cry11_colocalization.oib' \
  'testdata/oif/Source Data Figure S5c-d CTRL.oif' \
  > .tmp/auditlist2-verified.paths
BIOFORMATS_RS_JAVA_PARITY=1 \
BIOFORMATS_RS_JAVA_PARITY_FILES=__manifest_only__ \
BIOFORMATS_RS_JAVA_PARITY_MANIFEST=.tmp/auditlist2-verified.paths \
BIOFORMATS_RS_JAVA_PARITY_CACHE_DIR=.tmp/java-oracle-cache \
cargo test --test java_parity_test java_parity -- --nocapture
```

Result: 14 files compared, 14 core series OK / 0 bad, 14 OME files OK / 0 bad,
14 bitwise pixel series / 0 bad, and 858/858 detailed pixel checks.

## Required Evidence Per Issue

- Source anchors: Java method/class and Rust function/module.
- Real-data fixture path(s), preferably from an existing manifest.
- Exact parity command used.
- Result scope: files, series, OME files, pixel checks, skipped checks, runtime.
- If drift is found: root cause, Java behavior, Rust behavior, fix location, and
  regression test or parity rerun.

## Run Order

1. Build or refresh real-data manifests by format.
2. For each format, do the source audit for issues 1-10 before broad editing.
3. Run metadata-only Java parity first.
4. Run bounded pixel parity (`MAX_PLANES=4` or equivalent).
5. Run default-depth pixel parity on representative batches.
6. Escalate any mismatch to `TOFIX.md` and a format-specific note.
7. Keep optimizations only after documenting why they preserve Java-visible
   behavior.
