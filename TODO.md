# Remaining Translation Work

This file is the current audit of Bio-Formats reader/writer behavior that still needs translation or parity work.

## True Stubs, Metadata-Only Leaves, and Partial Native Variants

- `VolocityReader` (`.mvd2`, plus companion suffixes `.aisf`, `.aiix`, `.dat`, `.atsf`): full Metakit metadata and pixel decoding are unsupported. Native Metakit probing reports bounded footer/TOC/table-shape diagnostics; `set_id` only succeeds for explicit `BFVOLOCITYMVD2` blind raw fixtures with original-metadata provenance.
- `ImspectorReader` (`.obf`, `.msr`): native stack decoding is unsupported unless explicit `BFIMSPECTOR_RAW_STACK_V1` data is present. Uncompressed and zlib-compressed synthetic stack payloads are supported.
- `XlefReader` (`.xlef`): TIFF/LOF/JPEG/PNG/BMP delegate leaves are readable, but LMS metadata leaves are metadata-only and pixel reads return `UnsupportedFormat`.
- `LifReader` (`.lif`): Leica LIF container/XML metadata is parsed and bounded uncompressed/zlib/deflate payloads are readable, including directly described interleaved RGB triples and uncompressed or zlib/deflate planar RGB triples with repeated logical RGB groups; unknown compression, alpha/non-RGB-sample color models, and other unverified layouts still return precise `UnsupportedFormat` errors.

Old placeholder macros in `extended.rs`, `flim2.rs`, `misc.rs`, and `misc4.rs` are not currently instantiated by registered readers, so they are not active registered stubs.

## Highest Priority Remaining Translation Work

1. PicoQuant `.ptu` / `.pqres`
   - Marker-rasterized TTTR reconstruction covers HydraHarp, TimeHarp 260, and MultiHarp T2/T3 layouts from synthetic records.
   - PicoHarp T2/T3 pixel reconstruction remains an unported Java branch: translate the original record bit packing and marker-raster path instead of treating it as unknown format behavior.
   - Non-HydraHarp reconstruction and unusual histogram variants need Java-oracle parity coverage once their branches are ported.

2. QuickTime `.mov` / `.qt`
   - H.264/AVC, HEVC, ProRes, MJ2K, DV, and other external-codec families are recognized with explicit diagnostics, but actual decoding remains unsupported without real codec implementations/dependencies.
   - Complex edit-list presentation metadata is bounded to finite, unit-rate, sample-aligned media segments that cover every sample exactly once; non-sample-aligned segments, gaps, overlaps, non-unit rates, and actual pixel-plane reordering remain unsupported with diagnostics.

3. Leica LIF `.lif`
   - Missing Java LIF branches for additional Leica compression/storage variants beyond confirmed scalar/interleaved-RGB-triple/planar-RGB-triple paths.
   - Alpha/non-RGB-sample color models need translation of the corresponding Java layout handling or explicit Java-parity rejection if Java does not decode them.

4. Nikon ND2
   - Missing full Java `ImageDataSeq` structured frame handling beyond the bounded chunk ordering path.
   - Missing full multi-position/full-series logic; bounded TimeLoop/ZStackLoop counts and OME per-plane DeltaT/Z-position metadata are covered when present in parsed XML.
   - Missing Java frame encoding branches beyond raw, zlib, and JPEG2000 where Java does not delegate to unavailable codecs.
   - Modern chunked variants are partially covered by bounded raw/zlib/JPEG2000 and UIComp paths; broader structured frame variants need Java-oracle parity tests after translation.

5. Leica XLEF / LMS
   - Unsupported LMS layouts still need coverage.
   - Missing full LMS/OME semantic object graph translation; current support is bounded to scalar/description projection, common graph-node diagnostics (detector/objective/laser/ROI/position/timestamp), and OME original-metadata annotations.

6. SlideBook 7
   - Missing unknown `.npyz` container variants.
   - Missing rich YAML object graph semantics beyond captured scalars and shallow scalar flow maps/lists with OME original-metadata annotations.
   - Missing validated semantics for compression dictionaries and mixed byte order/type pixel layouts beyond the current explicit diagnostics.

7. Amnis IM3
   - Missing pixel interpretations beyond the current interleaved `Uint16` XYC path.
   - Missing full Java spectral library/object semantics and richer acquisition metadata beyond Java-style string/float/int scalar aliases, OME original-metadata annotations, and unsupported nested-record diagnostics.

8. TillVision `.vws`
   - Missing compressed CImage algorithms beyond zlib-wrapped and raw deflate payloads.
   - Missing broader native binary fragment table discovery beyond explicit metadata tables and bounded padding-based inference.

9. Hamamatsu VMS / VMU
   - Missing JPEG color models beyond RGB/L8/CMYK; current metadata reports marker-derived SOF/color-conversion diagnostics for unsupported or profile-sensitive encodings.
   - Missing actual ICC/profile color transforms and still-unknown tile-key variants; ICC APP2 marker sequences are preserved and reported but not applied.

10. Other obvious partials
    - iVision IPM: packed 16-bit color and square-root type metadata initialize, but pixel decode remains unsupported because the RGB555/RGB565 mask/order and square-root transfer curve are unresolved.
    - Leica LOF: optional enrichment remains for broader instrument, detector, ROI, true LUT tables, and BGR channel-order metadata beyond the faithful core reader; direct channel names, wavelengths, and LUT-name diagnostics are projected.
    - Openlab LIFF: bounded native scalars, calibration, Z/C/T/name inference, and OME original-metadata annotations are preserved; richer structured OME stage/detector metadata remains unsupported.
    - CellSens ETS/VSI: Java metadata/tag branches beyond current tag coverage remain to be translated; captured ETS/VSI scalars are preserved in OME original-metadata annotations.
    - No-Java-reference partials such as DCIMG, Norpix SEQ, SimFCS, BigDataViewer, and several TIFF wrappers remain best-effort extensions without an original Java reader to translate; DCIMG, Norpix SEQ, SimFCS, and BigDataViewer now preserve captured series metadata in OME original-metadata annotations.
