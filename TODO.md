# TODO — toward read/write parity with Java Bio-Formats

Goal: bioformats-rs should read and write every data stream the upstream Java
implementation handles. Items below are grouped from "probably incorrect pixel
output today" down to "feature gap, errors cleanly".

Most entries come from a cross-language audit against the Java tree using
`ccc-rs compare/missing/constants-diff` (with `ccc_mapping.toml`) plus targeted
reading of both implementations. See FEATURES.md for the inventory of declared
stubs.

## 1. Translation bugs (wrong or missing pixel logic)

- [ ] **LsmReader::open_bytes — missing channel-split path**
  - `src/formats/lsm.rs:238` delegates to `inner.open_bytes` only.
  - Java `ZeissLSMReader.openBytes` (components/formats-gpl/…/ZeissLSMReader.java:393–435) takes a second path when `splitPlanes && getSizeC() > 1 && ifds.size() == getSizeZ() * getSizeT()`: loads one IFD buffer per (Z,T) and emits per-channel bytes via `ImageTools.splitChannels`.
  - Result today: multi-channel LSMs where one IFD holds all channels return interleaved/incorrect bytes.

- [ ] **CziReader::open_bytes — missing mosaic/tile assembly and pyramid selection**
  - `src/formats/czi.rs:314` reads a single subblock and trims/pads to the plane size.
  - Java `ZeissCZIReader.openBytes` (~105 LOC, cognitive 92) assembles mosaic tiles, honours pyramid resolution level, and supports X/Y subregions.
  - Result today: multi-tile or multi-resolution CZI datasets render incorrectly (missing tiles, no crop honoured).

- [ ] **TIFF decompress — real strip dimensions not threaded through**
  - `src/tiff/compression.rs:46–64` calls `decompress_ccitt_group3/4`, `decompress_nikon` with width/height = `0, 0` (and bpp = `0` for Nikon).
  - Even after the stub bodies in `src/common/codec.rs:167,174,213` are implemented, these codecs need the actual strip/tile dimensions from the IFD (`image_width`, `image_length`, `bits_per_sample`). Plumb them through `decompress()`'s signature.

- [ ] **Reader `close` methods that don't null out format-specific state**
  - `missing --stub-loc-ratio 0.2` flagged ~30 one-line `close` implementations.
  - Safe where `close` just forwards to an `inner` reader. **Not safe** on readers that hold their own owned state (path, meta, parsed caches) — e.g. `LsmReader`, `CziReader`, `ZviReader`, `XrmReader`, `VolocityReader`, `MicromanagerReader`. Java's equivalents (e.g. `SVSReader.close` at SVSReader.java:228–249) null per-file fields so `setId` on a second file starts clean.
  - Action: audit each non-forwarding reader's `close` — anything held in `self` that is file-specific must be reset.

- [ ] **Verify wrapping-reader method delegation is complete**
  - `WholeSlideTiffReader` (`src/formats/svs.rs`) and similar TIFF-wrapper readers forward many methods to `inner`. Java SVS mirrors this pattern, so delegation is fine *if complete*. Confirm each of: `is_this_type_by_bytes`, `set_series`, `resolution_count`, `set_resolution`, `open_thumb_bytes` matches the Java counterpart's behaviour (not just compiles).

## 2. Incomplete parsers (reader opens file but drops metadata)

Signal: `constants-diff` showed 100+ magic strings/tag names present only on the Java side per function. Rust reads pixels but skips optional metadata the Java reader stores in `originalMetadata`.

- [ ] **IPLab (`src/formats/norpix.rs::IplabReader`)** — Java `IPLabReader.parseTags` emits tag names (`fini`, `clut`, `norm`, `roi`, `head`, `mmrc`, `user`, LUT labels, ROI shape codes). Rust parses the pixel payload but not the tag dictionary.
- [ ] **ZVI (`src/formats/zvi.rs`)** — `parse_zvi` misses Java `ZeissZVIReader.parseImageNames` / tag-code table (constants drift score 73).
- [ ] **DICOM (`src/formats/dicom.rs::parse_dicom`)** — 94 cognitive points on the Rust side but constants diverge from `DicomReader.parseMDB` (score 107); confirm DICOM tag dictionary coverage.
- [ ] **CellH5 (`src/formats/cellh5.rs::parse_cellh5`)** — HDF5 attribute/dataset names not in the Rust constants.
- [ ] **BigDataViewer (`src/formats/bdv.rs::parse_bdv`)** — XML element/attribute names diverge from Java.
- [ ] **Metamorph MM metadata (`src/formats/metamorph.rs::parse_mm_metadata`)** — key set smaller than `MetamorphReader.getOriginalMetadata`.

Re-run after each fix:
```
ccc-rs constants-diff rust.json java.json \
  --mapping ccc_mapping.toml | grep '^== <reader>'
```

## 3. Codec stubs (explicit `UnsupportedFormat`)

In `src/common/codec.rs`, these return "not yet implemented" and block the readers that reach them:

- [ ] `decompress_ccitt_group3` / `decompress_ccitt_group4` — blocks some scanner TIFFs.
- [ ] `decompress_nikon` — blocks Nikon NEF inside TIFF wrappers.
- [ ] `decompress_msrle` — blocks AVI MSRLE.
- [ ] `decompress_mjpb` (Motion JPEG-B) — blocks QuickTime / AVI.
- [ ] `decompress_rpza`, `decompress_qtrle` — blocks legacy QuickTime.
- [ ] `decompress_lzo` — used by a couple of proprietary readers.
- [ ] Thunderscan (inline error in `src/tiff/compression.rs:57`) — rare; safe to keep last.

When implementing any of these, also fix the "real strip dimensions" plumbing (section 1).

## 4. Reader stubs (already listed in FEATURES.md)

25 readers return `UnsupportedFormat` on `set_id`. Filling them closes read-side parity directly. Priority order below picks formats that are (a) documented or (b) common in real datasets:

- [ ] `QuickTimeReader` (.mov) — QuickTime atom parser (benefits MJPEG-B too)
- [ ] `SlideBookReader` (.sld) — reverse-engineered format docs exist
- [ ] `OirReader` (.oir) — Olympus OIR; needs OLE2 compound-doc reader
- [ ] `VolocityLibraryReader` / `VolocityClippingReader` (.acff) — OLE2 as above
- [ ] `XrmReader` (.xrm/.txrm) — OLE2 Zeiss X-ray microscope (shared code with above)
- [ ] `FlowSightReader` (.cif) — Amnis flow cytometry
- [ ] `LeicaLofReader` (.lof) — binary format; LIF parser already exists, reuse
- [ ] Remaining proprietary readers on best-effort basis (see FEATURES.md table)

A single shared OLE2/Compound-Document reader unblocks four of the above.

## 5. Writer gaps

Current writers (14): AVI, BMP, DICOM, EPS, FITS, ICS, JPEG, MetaImage, MRC, NRRD, OMEXML, PNG, Targa, TIFF (+ PyramidOMETiff). Java also ships:

- [ ] `APNGWriter` — animated PNG; Rust has `PngReader` but no animated output.
- [ ] `CellH5Writer` — HDF5-backed output (needs `hdf5` dep already in features).
- [ ] `JPEG2000Writer` — needs jpeg2k feature, already optional on reader side.
- [ ] `OMETiffWriter` — a non-pyramid OME-TIFF writer (current `PyramidOmeTiffWriter` is pyramid-only).
- [ ] `QTWriter` — paired with QuickTime reader.
- [ ] `V3DrawWriter` — Vaa3D raw; small and self-contained.
- [ ] `ImageIOWriter` — Java's generic fallback; Rust has `image` crate, decide whether to add or skip.

## 6. Reader wrappers missing in Rust

Java has 8, Rust has 5 (`ChannelSeparator`, `ChannelMerger`, `ChannelFiller`, `DimensionSwapper`, `MinMaxCalculator`). Missing:

- [ ] `FileStitcher` (Rust has `stitcher.rs` but it's not wired through `ImageReader`).
- [ ] `Memoizer` as a *wrapper* (Rust has `memoizer.rs` but as standalone `.bfmemo` helper).
- [ ] `BufferedImageReader` / `ReaderWrapper` base equivalent — decide whether the trait composition in Rust already covers this or whether a shared base wrapper struct is needed.

## 7. Metadata-store gap

`missing` shows Java has extensive `MetadataStore` / `MetadataConverter` /
`MetadataTools` helpers (populatePixels, verifyMinimumPopulated, createLSID,
setChannelGlobalMinMax, ModuloAnnotation factories…). Rust's `OmeMetadata`
covers 21 types but no equivalent populate/verify/merge helpers.

- [ ] Port `MetadataTools.populatePixels` / `populateMetadata` equivalents so readers don't all re-implement OME-XML construction inline.
- [ ] Port `verifyMinimumPopulated` as a debug-only check.
- [ ] Port `MetadataConverter.convertMetadata` for writer input.
- [ ] Wire `ModuloAnnotation` / `OriginalMetadataAnnotation` into OME-XML serialization in `OmeXmlWriter`.

## 8. Continuous audit

Re-run after each change:

```
# 1. rebuild reports
ccc-rs analyze src                        -l rust --recurse -o rust.json
ccc-rs analyze java-bioformats/components -l java --recurse -o java.json

# 2. Top deviations — look for new surprises near the top
ccc-rs compare rust.json java.json --mapping ccc_mapping.toml --top 30

# 3. Stub-likelihood partial matches
ccc-rs missing rust.json java.json --mapping ccc_mapping.toml --stub-loc-ratio 0.2

# 4. Magic-number / string drift per function
ccc-rs constants-diff rust.json java.json --mapping ccc_mapping.toml
```

When a new reader is added, append its mapping entries to `ccc_mapping.toml`
(class rename + `open_bytes`/`set_id`/`close`/`is_this_type` pins) to keep the
diff meaningful.
