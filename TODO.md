# Remaining Translation Work

This file tracks Bio-Formats reader/writer behavior that still needs translation
or parity work. The goal is faithful translation of original Bio-Formats code
where such code exists; no-Java-reference extensions are called out separately.

## True Stubs, Metadata-Only Leaves, and Partial Native Variants

- `VolocityReader` (`.mvd2`, plus `.aisf`, `.aiix`, `.dat`, `.atsf`):
  - Java `initFile` named-child metadata streams are now translated: per-stack
    `Timepoint times stream` ATSF id, `um/pixel (X/Y/Z)` physical sizes,
    `Microscope Objective` magnification, `Camera/Detector` and
    `Experiment Description` strings, and `X/Y/Z Location` scalars are parsed
    from the signature-prefixed child sample streams and projected into
    candidate metadata plus the native semantic summary.
  - Full Volocity stack/sample hierarchy semantics beyond bounded Java-style
    stack-candidate/channel-child discovery, channel child name/AISF-link and
    stack `sampleFileLink` to `filesViewR` resolution, bounded Data sibling
    provenance diagnostics, and inline stream header validation remain
    partially translated.
  - Native pixel decoding beyond diagnostic inline stream header clues is not
    translated.
  - Current code probes bounded footer/TOC/table/column/subview row diagnostics,
    reports safe single-row fixed scalar values from native Metakit tables, and
    reports Java table-role provenance, variables-table counters, and bounded
    sample/string/file hierarchy column provenance, including bounded first-row
    variable string/byte previews, first `samplesViewR` row scalar clues,
    `sampleData` byte previews, first `stringsViewR` string-link provenance,
    and bounded Java-style stack-candidate discovery from `samplesViewR` plus
    `stringsViewR`, including channel-child counts, bounded channel sample
    names and `.aisf` link IDs, stack `sampleFileLink` to `filesViewR`
    filename/spec previews, and inline native stream dimension/endian clues for
    candidates without `Channels`, plus bounded expected `.aisf`, `.aiix`,
    `.dat`, and `.atsf` sibling provenance under the Java-style `Data` tree;
    `set_id` succeeds for explicit `BFVOLOCITYMVD2` blind raw fixtures.
- `ImspectorReader` (`.obf`, `.msr`):
  - Native Imspector stack decoding remains partial: v1 uncompressed and
    zlib-compressed contiguous single-stack payloads are supported, bounded
    v1-v6 footer parsing is supported, and uncompressed or zlib-compressed v6
    chunk-position tables are assembled into logical payloads. Multi-stack
    files are now exposed as one series per stack (faithful translation of the
    OBF/Imspector `initFile` linked-list `next`-pointer loop, with bounds- and
    monotonicity-checked offsets). Bounded native dimension labels, step
    tables, step labels, flush-point counts/previews, selected v4/v6 footer
    scalars, and v4+ tag-dictionary length plus bounded raw ASCII/hex previews
    are preserved as metadata (Java itself only skips the tag-dictionary
    bytes). The `initStack` description metadata is now translated:
    `Stack version`, per-dimension `Lengths`/`Offsets`, the stack `Name`, derived
    `PhysicalSizeX/Y/Z`, and the `Description`/`<Time Lapse>` XML grandchild
    scalars. The incremental flush-point-seeking inflate read path is superseded
    here by up-front per-stack payload assembly/decompression; FLIM `SPCM`
    label dimension remapping remains unported.
  - Explicit `BFIMSPECTOR_RAW_STACK_V1` synthetic payloads remain supported.
  - Uncompressed and zlib-compressed synthetic stack payloads are supported.
- `XlefReader` (`.xlef`, `.xlif` projects):
  - TIFF/LOF/JPEG/PNG/BMP delegate leaves are readable; XLIF tilescans are
    expanded one series per tile (translation of `XlifDocument.getTileCount`
    `DimID="10"` plus the `XLEFReader` reader-index/series-per-tile mapping),
    with the tile index exposed as `xlef.project.tile_index`.
  - LMS leaves are metadata-only when no delegate supports pixels; pixel reads
    return `UnsupportedFormat`.
  - LMS metadata projects instrument, objective, detector, laser, filter,
    dichroic, ROI, channel, physical-size, and description fields into OME plus
    original-metadata annotations. Channel LUT colour (hex `#RRGGBB` and
    `R,G,B`, packed to the OME signed-i32 RGBA word and projected onto
    `OmeChannel.color`), the full instrument graph (microscope/objective/
    detector/laser/filter/dichroic into one `OmeInstrument` with
    instrument/objective refs), and Rectangle/Line/Ellipse/Point ROI shapes
    (with `TheZ`/`TheC`/`TheT`/`IndexC` plane-index aliases) are translated.
    Per-channel/per-dimension `BytesInc` strides and `Memory`/`Storage` block
    nodes are surfaced as `xlef.lms.pixel_layout.*` diagnostics; a declared raw
    layout marks the series `declared_unsupported` and fails `open_bytes` with
    an enriched diagnostic. Raw LMS pixel decoding and full
    `LMSMetadataExtractor` object-graph parity remain partial.
- `LifReader` (`.lif`):
  - Additional Leica compression/storage variants remain unported; declared
    compressed payloads record `lif.compression.status` provenance for
    supported zlib/raw-deflate/gzip-wrapped deflate, generic boolean
    compression hints routed by bounded zlib/gzip payload signatures, and
    unsupported compression hints, include bounded payload
    signature/leading-byte diagnostics, memory block resolution records
    requested/resolved IDs, file offset/length, and ID-match versus file-order
    fallback diagnostics, and Java-compatible blank planes are returned for
    missing/truncated uncompressed pixel memory blocks.
  - XML-described contiguous interleaved/planar RGB, four-sample color groups,
    and ordered non-overlapping padded planar color groups are supported, with
    three-sample color groups reordered from Leica's stored BGR to RGB
    (translation of `ImageTools.bgrToRgb`, gated on `getRGBChannelCount() == 3`
    so four-sample RGBA is left untouched); other irregular/non-contiguous
    color layouts — which the Java reader also does not specially decode — are
    preserved as original layout metadata and rejected explicitly rather than
    decoded.
  - Other unverified layouts still return explicit `UnsupportedFormat` errors.

## High-Priority Unported Java Branches

1. PicoQuant `.ptu` / `.pqres`
   - PicoHarp T2/T3 records are recognized for metadata but remain
     pixel-reconstruction blocked until a local Bio-Formats Java branch, oracle
     fixture, or PicoHarp marker-raster bit-packing reference is available; the
     Rust reader explicitly rejects HydraHarp-compatible inference.
   - TTTR record layouts beyond HydraHarp, HydraHarp 2, TimeHarp 260N/260P,
     MultiHarp, and metadata-only PicoHarp identification still need
     translation and Java-oracle parity tests; current metadata records the
     explicit layout source/provenance and keeps unknown or inferred layouts
     marked as undecoded.
   - Histogram variants with non-contiguous, mixed-bin-count, or otherwise
     structured payloads beyond exact contiguous uint8/uint16/uint32
     `HistResDscr_*` and `HistoResult_*` bins remain unsupported; exact
     contiguous equal-width indexed `HistResDscr_*` descriptors and contiguous
     equal-width indexed per-curve `HistResDscr_DataOffset` payloads with
     bounded padding are decoded. Explicit histogram payload-offset headers are
     skipped for otherwise exact contiguous payloads, and explicit
     zlib/gzip/raw-deflate compressed histogram payload tags are decoded with
     focused synthetic coverage when the inflated payload matches one of those
     exact contiguous bin layouts.
     Mismatched payload sizes now report the supported byte counts explicitly,
     and non-contiguous, mixed-bin-count, ambiguous compression-flag, or
     non-exact inflated layouts are preserved as structured metadata while
     still rejecting image-plane decoding. Remaining PicoQuant histogram work is
     Java-oracle parity for the covered exact-layout branches and original-code
     translation for the still-unsupported structured payload layouts.

2. QuickTime `.mov` / `.qt`
   - External-codec families are recognized with explicit Java-delegation
     diagnostics but not decoded: H.264/AVC, HEVC, ProRes, MJ2K, DV, and
     related platform-decoder codecs.
   - Edit-list handling supports complete normal-speed, sample-aligned
     multi-segment presentation-to-source sample reordering in `open_bytes`,
     including sample-aligned clipped normal-speed media segments, internal and
     trailing empty edits that do not require synthetic pixel planes. Clipped
     edit lists also record retained source sample range, omitted before/after
     sample counts, and source media tick boundaries. Chunk offsets are read
     from both 32-bit `stco` and 64-bit `co64` tables.
   - Non-sample-aligned segments, media sample gaps, overlaps, and non-unit
     rates remain diagnostic-only; all of these unsupported edit-list branches
     record bounded reason plus first problem segment/sample metadata where
     applicable but still do not apply pixel-plane clipping/reordering.

3. Leica LIF `.lif`
   - Three-sample color groups are now reordered from Leica's stored BGR to RGB
     (`ImageTools.bgrToRgb`, both interleaved and planar paths, at the
     uncompressed and compressed return points).
   - Additional Java LIF compression/storage branches beyond confirmed scalar,
     interleaved/planar XML-described color sample groups, ordered
     non-overlapping padded planar color groups, zlib, raw deflate,
     gzip-wrapped deflate paths, generic boolean compression hints routed by
     bounded zlib/gzip payload signatures, and Java-compatible
     missing/truncated uncompressed memory-block blank-plane fallback;
     unsupported compressed payload hints and bounded payload signatures are
     preserved in metadata and rejected explicitly rather than decoded.
   - Irregular/non-contiguous Leica color layouts beyond ordered
     non-overlapping padded planar groups still need Java-layout translation
     before decoding; current Rust behavior records the declared offsets/status
     metadata and rejects these layouts explicitly.
   - More fixture-backed coverage is needed for unverified Leica layouts.

4. Nikon ND2 `.nd2`
   - Modern `ImageDataSeq` coverage is bounded to Java-style raw payloads with
     8-byte prefixes, small structured trailers, 4096-byte Nikon prefixes,
     zlib, and JPEG2000 signature routing.
   - Multi-position loops are split for one-plane positions, interleaved
     full-series layouts, contiguous full-series layouts when per-plane Z
     metadata disambiguates the order, and unambiguous outermost/innermost
     modern XML `XYPosLoop` orders whose loop counts exactly match the plane
     grid. Modern XML loop order/count evidence is preserved as diagnostics;
     remaining ambiguous loop-order mappings are not used for pixel remapping
     until richer Java mapping is translated.
   - Additional Java frame-encoding branches beyond current raw, zlib, and
     JPEG2000 coverage remain to be translated where Java does not delegate to
     unavailable external codecs.
   - Modern chunked variants have bounded little-endian chunk-table diagnostics
     including table offset, entry width, payload byte count, and payload
     ranges, plus exact raw-plane assembly for validated u32/u64 offset/length
     tables, including 8-byte timestamp-prefixed and 4096-byte Nikon-prefixed
     tables. Split single-stream zlib and JPEG2000 payloads inside those
     tables are recognized and routed through the existing zlib/JPEG2000 paths,
     and all-zlib per-chunk tables are decoded by inflating each table entry in
     order and requiring the exact plane size. Ambiguous per-chunk JPEG2000 and
     mixed-compression chunk-table layouts are diagnosed explicitly and remain
     unsupported pending Java-oracle parity tests.
     Structurally richer/non-offset-length chunk-table variants still need
     Java-oracle parity tests after translation.

5. Leica XLEF / LMS
   - XLIF tilescans are expanded one series per tile
     (`XlifDocument.getTileCount` + `XLEFReader` reader-index mapping).
   - Unsupported LMS pixel-layout decoding remains incomplete; metadata-only
     LMS leaves capture per-channel/per-dimension `BytesInc`, `Memory`,
     `Storage`, and compression diagnostics under `xlef.lms.pixel_layout.*`,
     mark a declared raw layout `declared_unsupported`, and include the
     enumerated declaration in pixel-read errors.
   - Full LMS/OME semantic object graph parity is still incomplete, but channel
     LUT colour (hex/`R,G,B` packed to the OME signed-i32 RGBA word), the full
     microscope/objective/detector/laser/filter/dichroic instrument graph, and
     `ChannelDescription` filter/dichroic light-path references are resolved
     from bounded LMS graph nodes and projected into OME.
   - Current support is bounded to scalar/description projection, selected
     graph node diagnostics, OME instrument/channel/light-path/colour
     projection, Rectangle/Line/Ellipse/Point ROI projection with
     plane/channel/time indices, and OME original-metadata annotations.

6. SlideBook 7
   - Unknown `.npyz` container variants remain unsupported, but now report
     payload signature, leading bytes, and gzip/zlib/deflate probe diagnostics
     plus ZIP/gzip container detection; `CompressionDictionary.yaml` entries are
     parsed/counted and a `.npyz` payload declaring a non-compressed algorithm
     is rejected as inconsistent.
   - The upstream typed record layer is now translated faithfully: the
     `ClassDecoder.Decode`/`FindNextClass` `StartClass`/`ClassName`/`EndClass`
     walk (replacing Java's reflection assignment), `RestoreSpecialCharacters`,
     the count-prefixed numeric-array convention, `CImageRecord70.Decode` with
     its chained `CLensDef70`/`COptovarDef70`/`CMainViewRecord70`, and the
     `CImageGroup.LoadChannelRecord` loop with `CChannelRecord70.Decode`
     (chaining `CExposureRecord70` and `CChannelDef70`->`CFluorDef70`) including
     skipping interleaved remap/manip/histogram classes. A small
     `slidebook7_yaml_compose` stands in for the snakeyaml node parse (the
     project takes no YAML dependency). The reader uses the typed
     `CImageRecord70` for dimensions, projects its name/info/lens/optovar/
     main-view fields as `slidebook7.image_record.*`, and projects per-channel
     name/camera/exposure/excitation/emission from `ChannelRecord.yaml` as
     `slidebook7.channel.*`, falling back to the bounded line scanner for record
     files without these typed classes.
   - `CMaskRecord70`/`LoadMaks` (mask records + per-timepoint position tables)
     and the full annotation graph — `CDataTableHeaderRecord70`,
     `CAnnotation70`, `CCubeAnnotation70`, `CFRAPRegionAnnotation70` (with its
     `theNumRegions` sub-region loop), and `CUnknownAnnotation70`, driven by the
     `LoadAnnotations` per-timepoint count-prefixed lists and the `GetIntegerValue`/
     `GetStringValue` helpers — are translated and projected as
     `slidebook7.mask.*` / `slidebook7.annotation.*` from `MaskRecord.yaml` and
     `AnnotationRecord.yaml`.
   - The multi-table `LoadAuxData` loader (float/double/sint32/sint64/serialized
     `CDataTableHeaderRecord70` + `theXMLDescriptor` + count-prefixed `theAuxData`
     arrays), `LoadElapsedTimes`/`LoadSAPositions`/`LoadStagePosition` (the
     `firstIsSize` true/false array convention included), and
     `CHistogramRecord70` capture in the channel loop are translated and
     projected as `slidebook7.aux.*` / `slidebook7.elapsed_times.*` /
     `slidebook7.sa_positions.*` / `slidebook7.stage_positions.*` /
     `slidebook7.histogram.*` from the corresponding `*.yaml` files.
   - The channel-loop manip/LUT records are now decoded and projected rather than
     skipped: `CRemapChannelLUT70` (remap type, low/high desired/given, built-table,
     equation), `CAlignManipRecord70` (manip id + XYZ offsets), `CRatioManipRecord70`
     (Kd/Rmin/Rmax/Beta), `CFRETManipRecord70` (paradigm + Fd/Dd, Fa/Aa), and
     `CRemapManipRecord70` (remap type, calib points) as `slidebook7.remap_lut.*` /
     `slidebook7.align_manip.*` / `slidebook7.ratio_manip.*` / `slidebook7.fret_manip.*` /
     `slidebook7.remap_manip.*`. The whole typed-record decode layer is now ported.
   - The decoded elapsed-times/stage-position/exposure/interplane-spacing data is
     now promoted into structured OME planes (Java `initFile` plane loop):
     per-plane `DeltaT` from the elapsed-times array (per timepoint),
     `ExposureTime` from the channel exposure record (per channel), `PositionX/Y`
     from the stage point, and `PositionZ` offset by the interplane spacing per Z
     plane; times are converted from Java milliseconds to OME seconds. Montage
     (multi-position) captures are not modelled by this reader, so the single
     stage point is used (Java non-montage path, position index 0).
   - Remaining SlideBook 7 work is montage/multi-position support and pixel
     decompression for non-trivial `CCompressionBase` block dictionaries. Note
     the `slidebook7.record.*` flatten-everything original-metadata behaviour
     (block seqs of flow maps, nested flow collections, YAML tag normalization)
     is a port extension, not upstream behaviour — Java's `DecodeUnknownString`
     is empty and discards unrecognised attributes.
   - Mixed byte order/type pixel layouts remain unsupported, but diagnostics
     now report the first and mismatched ImageData files, NPY descriptors,
     pixel types, and byte orders.

7. Amnis IM3
   - Pixel interpretations beyond the current interleaved `Uint16` XYC path
     remain unported (Java itself hard-codes UINT16 and never decodes the
     REC_IMAGE pixel-type field, so this matches upstream).
   - The `SpectralLibrary` object graph is now ported with bounded fidelity:
     the `SpectralLibrary` -> unnamed-container -> `Spectra` -> `Values` walk
     (`IM3Reader.initFile`), per-`Spectrum` `Name` (`Spectrum` constructor),
     and the nested `Wavelengths`/`Magnitudes` float records
     (`parseSpectrumRecord`) are surfaced as `im3.spectral_library.*`
     metadata. Spectrum sub-fields Java declares but never reads
     (Color/Selected/calibration) remain unported.
   - Acquisition metadata now includes REC_BOOLEAN scalar/array aliases
     (`BooleanIM3Record`) alongside the existing Java-style scalar/string/float
     aliases and OME original-metadata annotations. Spectra-only files with no
     Shape/Data image dataset are still explicitly rejected.

8. TillVision `.vws`
   - The non-embedded `.vws` "Root Entry/Contents" image-name loop and its
     `findNextOffset` marker dispatcher (4-marker plus byte-scan variants) are
     translated, so on-disk `.pst`-backed series recover their OLE-stream image
     names with the faithful big-endian-name-length / little-endian-skip-length
     endianness switch.
   - Native compressed CImage algorithms beyond zlib-wrapped, gzip-wrapped,
     and raw-deflate payloads remain missing (upstream reads only uncompressed
     planes, so this is an explicit rejection rather than a missing decoder);
     common zlib/gzip/raw-deflate alias spellings and compressed-without-
     algorithm diagnostics have bounded coverage.
   - Binary-only native fragment-table structures beyond the bounded
     little-endian count plus u32/u64 offset/length or offset/end tables,
     metadata-described offset/length or offset/end tables, and padding-based
     inference remain incomplete; the non-embedded description-block
     `nImages`-driven `.pst` selection/count-mismatch logic is still replaced by
     directory scan + INF discovery.
   - The native class-name fixed-offset CImage layout has bounded coverage;
     other exotic/native CImage layouts still need original-branch translation
     and fixture-backed tests.

9. Hamamatsu VMS / VMU
   - `initFile` store population is now translated: image names
     (full/macro/map), OME `PhysicalSizeX/Y` (full + macro series), and the
     `SourceLens` Instrument/Objective/ObjectiveSettings on series 0, all gated
     behind `MetadataLevel.MINIMUM` exactly as Java does.
   - JPEG decoding remains bounded to RGB24, L8 expanded to RGB, and CMYK32
     converted to RGB; other decoder pixel formats, including decoder L16, are
     explicit unsupported paths that report the requested format, supported
     formats, source path, and metadata-only ICC/profile policy. The Turbo-JPEG
     `JPEGTurboService` restart-marker tile-streaming path is replaced by the
     port's own bounded restart-aware band decoder.
   - ICC/profile color transforms are preserved as metadata and explicitly
     reported as unapplied; invalid, missing, and duplicate ICC chunk
     sequences are reported as incomplete rather than complete profiles.
   - Bounded tile/associated-image key aliases are supported, including
     `ImageFileName`, `ImageName`, `ImagePath`, `MacroImageFile`,
     `MacroImageName`, `MapImage`, and `MapImageFile`; single-row
     zero-based numeric suffix tile keys such as `ImageFile0`/`ImageFile1`
     and label-based `Row`/`Column`, `Col`, or `X`/`Y` coordinate tile keys
     are also supported. Supported alias families and unknown-alias handling
     are reported in metadata, and remaining unknown tile-key variants stay
     unsupported rather than guessed.

10. FilePattern `.pattern`
    - Recursive `**` behavior has bounded support, including zero-or-more
      directory matching and terminal `**` filtering to registered-reader
      leaves, adjacent `**/**` collapse, plus an explicit diagnostic when a
      terminal recursive tree has no supported reader files; remaining edge
      cases need oracle-backed Java parity coverage.
    - Bracket class glob matching and overlapping explicit labels have bounded
      support, including negated shell classes and nested brace/class
      alternations covered by local tests.
    - Java-style aliases are emitted for local FilePattern pattern/root/block/
      axis/channel metadata, including block-count/per-block element-count
      aliases, generic axis, and channel-name variants; any remaining naming
      gaps need oracle comparison against Java original-metadata output.

## Other Partial Readers / Feature Gaps

- Imaris IMS `.ims`:
  - HDF5 hyperslab reads, common metadata, scalar instrument/objective/detector/
    light-source projection, bounded Surpass/object-graph scalar metadata and
    group-provenance capture, named Surpass statistics-table capture for
    bounded stat-row/stat-column layouts including singleton-axis table shapes,
    bounded diagnostics for oversized or unsupported Surpass statistics-table layouts,
    mesh/large-geometry diagnostics, OME original-metadata annotations, and
    bounded OME ROI projection from small Surpass object center/radius plus
    explicit zero-based `IndexT`/`IndexC`-style statistics are supported.
  - Richer Imaris Surpass object semantics, unbounded statistics-table value
    translation, value translation for complex statistics-table layouts, and
    full object translation still need translation.
- MetaXpress / SimplePCI / MIAS / Trestle / TissueFAXS / Mikroscan /
  Ionpath MIBI TIFF wrappers:
  - Pixels are delegated through TIFF.
  - SimplePCI/HCImage TIFF ImageDescription signatures, bounded key/value
    vendor annotations including section-qualified INI-style scalar metadata,
    bounded XML scalar metadata aliases, and bounded SimplePCI/HCImage nested
    XML hierarchy scalar leaves with path/depth metadata are supported in the
    Molecular Devices TIFF wrapper and the HCS SimplePCI TIFF wrapper. Bounded
    MetaXpress XML plate/well/site/channel/acquisition/objective scalar
    metadata plus bounded nested MetaXpress XML hierarchy scalar leaves with
    path/depth metadata are supported.
  - Format-specific assembly and deeper vendor metadata remain incomplete for
    MIAS, Trestle, TissueFAXS, Mikroscan, Ionpath MIBI, typed/non-scalar
    SimplePCI object semantics beyond bounded scalar hierarchy preservation,
    and typed/non-scalar MetaXpress object semantics.
- Cellomics `.c01`, `.dib`:
  - zlib + DIB pixels, DIB header metadata, filename plate/well/field/channel
    hints including the Java `oN` channel filename variant, same-field sibling
    file channel assembly, same-plate filename-sorted well/field series
    assembly, and sibling `.mdb` channel metadata plus bounded protocol/plate/
    experiment/instrument MDB scalar metadata are supported; unhandled MDB table
    names and row/column shapes are reported as diagnostics.
  - Remaining gaps are semantic mappings for vendor-specific MDB tables beyond
    the bounded scalar protocol/plate/experiment/instrument/channel mappings and
    richer plate metadata-store semantics that need concrete original-code/table
    coverage.
- Photoshop / QPTIFF TIFF wrappers:
  - Plain TIFF delegation, PSD/PSB header/image-resource summaries, PSD/PSB
    pixel-aspect-ratio, resolution-info, copyright-flag, global-angle, and
    version-info image-resource payload decoding, bounded AlphaChannelNames
    resource 1006 decoding with malformed-payload provenance, bounded
    DisplayInfo resource 1007 decoding, bounded PrintFlags resource 1011
    decoding, bounded PrintFlagsInformation resource 10000 decoding, bounded
    XMP resource 1060 semantic scalar expansion, and bounded QPTIFF
    ImageDescription/tag metadata annotations plus JSON vendor-object scalar
    metadata flattening, bounded JSON object/array graph summary metadata, and
    safe semantic aliases for common vendor JSON acquisition, channel
    wavelength/name, and instrument/objective scalars are supported.
  - Deeper Photoshop image-resource payload decoding beyond the current bounded
    branches, such as larger structured resources, paths, layer/channel blocks,
    and richer XMP object semantics, plus richer QPTIFF vendor object semantics
    beyond bounded JSON graph summaries, scalar flattening, and common scalar
    semantic aliases remain incomplete.
- NIS TIFF wrapper:
  - Plain TIFF delegation and bounded NIS-Elements ImageDescription XML
    metadata for variants, objective/camera acquisition fields, and channel
    names/wavelengths plus explicit exposure/gain/readout acquisition scalars
    are supported. Recognized shallow XML object elements such as experiment,
    microscope/camera/device, channel/channel-description, stage/ROI,
    illumination, and plane metadata are preserved as bounded scalar original
    metadata; bounded nested XML hierarchy scalar leaves are preserved with path
    metadata; numeric stage positions and rectangular/point/line/ellipse ROI
    XML objects are also projected into OME plane/ROI metadata when explicit
    safe scalar fields are present.
  - Full NIS XML object graph semantics remain incomplete; unsupported and
    partially supported `<variant>` records are reported with bounded
    per-record unsupported-attribute diagnostics.
- Becker & Hickl SDT/SPC `.sdt`, `.spc`:
  - Multi-block support exists.
  - Binary setup MCS-TA point counts are parsed for series sizing; padded-width
    rows are cropped to the exposed image width; unmatched raw data block
    lengths are reported with explicit unsupported-layout metadata/diagnostics.
  - Non-image FCS/FIDA/FILDA/MCS curve measurement modes are preserved as
    descriptor metadata, and parsed descriptor scalars for ADC resolution,
    stop time, increment, scan dimensions, and routing channels are preserved
    as original metadata; simple raw little-endian `u16` curve payloads with
    exact even byte counts and simple ZIP-local-header deflated raw `u16`
    payloads are exposed as one-plane curve series, odd-sized raw/ZIP curve
    payloads are preserved as byte-valued one-plane curve series with explicit
    diagnostics, while empty raw/ZIP `u16` curve payloads are exposed as
    metadata-only `empty_curve` series. ZIP-local-header curve payloads preserve
    bounded method, flag, declared-size, filename, payload-offset, and
    leading-byte diagnostics for unsupported variants. Structurally richer curve
    payload decoding remains partial.
- Bruker MicroCT / Imaris TIFF / SlideBook TIFF:
  - TIFF delegation exists.
  - Bruker MicroCT reads bounded same/related-stem `.log`/`.protocol`
    key-value companions plus global or related-stem `Parameters.txt` and
    `Description.txt`, including image description and exposure-time OME
    projection.
  - Imaris TIFF has bounded strip-to-Z support for exact uncompressed
    full-plane strips, with bounded diagnostics for compressed, tiled, and
    irregular unsupported strip layouts.
  - SlideBook TIFF has bounded multi-file channel grouping for sibling
    SlideBook TIFFs with matching TIFF `DateTime` timestamps and compatible
    one-series scalar geometry.
    Remaining gaps are broader MicroCT directory grouping, decoding compressed,
    tiled, or otherwise irregular Imaris TIFF strip variants, and broader/ambiguous
    SlideBook TIFF grouping layouts.
- Leica LOF:
  - Core reader, direct channel/wavelength/LUT-name diagnostics, and bounded
    scalar XML instrument/detector/ROI/stage/acquisition metadata are supported.
    BGR/RGB channel-order diagnostics are recorded from explicit
    `ChannelDescription BytesInc` offsets when the simple Leica XML layout is
    unambiguous.
  - True LUT table decoding and deeper Leica object graph semantics remain
    incomplete.
- Openlab LIFF:
  - Bounded native scalars, calibration, per-plane tag headers/names/offsets,
    non-image tag headers, Z/C/T/name inference, and OME original-metadata
    annotations are supported, including explicit combined and separate
    stage/detector projection provenance plus reason strings for why structured
    OME stage/detector objects are not projected from the currently safe LIFF
    fields.
  - Richer structured OME stage/detector metadata remains incomplete unless the
    LIFF parser is extended with explicit safe semantic fields for those
    objects.
- CellSens ETS/VSI:
  - Pixel reading, many tag/scalar metadata paths, OME channel names/wavelengths,
    calibration-function VALUE metadata, instrument/objective/detector
    projection, Z/T plane metadata projection, Java-style device type and
    `DEVICE_SUBTYPE` device-class metadata, detector-type projection from the
    mapped device class, Java-style stack type labels, HAS_EXTERNAL_FILE scalar
    preservation, and bounded original-metadata preservation for recognized
    scalar/display/document/slide tags, with stable semantic aliases for
    document/slide identity fields, are supported.
  - Remaining Java metadata/tag branches beyond current tag-name preservation
    coverage still need translation where they require richer semantics than
    safe scalar original-metadata capture.

## Writers and Cross-Cutting Metadata

- OME metadata:
  - Baseline/enriched metadata exists for many readers.
  - Generic `objective.magnification` series metadata is projected into an OME
    Instrument/Objective reference.
  - Generic dotted series metadata for objective model/NA/immersion, detector
    model/manufacturer/type/gain/offset, light-source
    model/type/power/manufacturer, filter model/manufacturer/type/cut-in/cut-out,
    dichroic model/manufacturer, channel light-path filter/dichroic
    references, experimenter first/last name/email/institution, plane
    timing/exposure/position, bounded rectangle/point/ellipse/line ROI
    coordinates, image name/description, and channel names/wavelengths is
    projected into OME where readers expose those scalar conventions.
  - Richer instrument, experimenter references, detector, objective,
    multi-channel light-path, ROI object graphs beyond
    rectangle/point/ellipse/line scalar coordinates, and acquisition metadata
    beyond scalar acquisition-date projection is still partial across several
    formats.

## Non-Upstream / No-Java-Reference Extensions

These are not missing original Bio-Formats branches, but they are incomplete
best-effort readers or wrappers:

- MetaImage `.mha`, `.mhd`: deliberate ITK/MetaIO extension, not core
  Bio-Formats.
- OME-Zarr / NGFF `.zarr`: translated from separate `ome/ZarrReader`, not core
  Bio-Formats.
- OpenSlide formats `.mrxs`, `.vms`, `.bif`, etc.: optional feature-gated
  multi-resolution OpenSlide wrapper.
- Hamamatsu DCIMG `.dcimg`: v0/v1 and four-corner correction are supported; no
  Java reader reference, so remaining unknown variants are best-effort gaps.
- Norpix StreamPix `.seq`: raw/JPEG frames and timestamps are supported; no
  Java reader reference, so additional compressed-frame behavior is best effort.
- TopoMetrix AFM `.tfr`, `.zfr`: best-effort header parsing only; no Java
  reader reference.
- SimFCS `.b64`, `.r64`, `.i64`: fixed 256x256 frame support exists; no core
  Java reader reference, so richer variants remain best effort.
- BigDataViewer `.h5`, `.xml`: HDF5 core reads, XML/core metadata, ViewSetup
  names, voxel-size unit/value metadata, and bounded ViewSetup scalar
  `<attributes>` metadata are supported; no core Bio-Formats Java reader
  reference, so richer metadata remains best effort.
- Photon Dynamics `.img`: header + raw pixel sidecar extension.
- PDS planetary `.pds`: NASA/planetary label/image variants are outside the
  translated Bio-Formats PerkinElmer/Photon Dynamics PDSReader branch.

## Missing external codecs (documented in README.md)

Pixel decoding that upstream Java delegates to native/platform decoders, with no
pure-Rust decoder in this tree (metadata is still read; only pixel decode is
unavailable): H.264/AVC, H.265/HEVC, Apple ProRes, Motion JPEG 2000, and DV
(all QuickTime `.mov`). JPEG-XR is implemented but feature-gated
(`--features jpegxr`). All other codecs used by the readers are implemented
in-tree (LZW, PackBits, Deflate, Zstd, LZ4, JPEG, JPEG 2000, Cinepak, RLE, PNG).

## Previously Resolved / Removed From Stub List

- GE MicroCT VFF (`.vff`, magic `ncaa`) — full `MicroCTReader.java` port
  (`bruker::MicroCtVffReader`), registered as a magic-byte detector.
- Photoshop PSD-TIFF layer/channel blocks (`PhotoshopTiffReader` Layr block:
  layer names/count) — was a plain-TIFF delegate stub.
- Trestle TIFF (`TrestleReader` comment key/values + copyright detection) —
  was a bare TIFF-wrapper stub.
- Becker & Hickl SDT descriptor sub-blocks (MeasStopInfo / MeasFCSInfo /
  extended MeasureInfo / MeasHISTInfo — the FCS/FIDA/FILDA/MCS curve descriptors).
- Openlab LIFF `USER`/`CVariableList` stage/detector variables (gain/offset,
  X/Y/Z stage positions) projected to OME Detector + plane positions.
- Leica LOF channel-LUT object graph (`translateLut`/`getChannelPriority`/
  inverse-RGB) projected to `OmeChannel.color`.
- CellSens VSI `STACK_TYPE`/`DEVICE_SUBTYPE` label translation + `RWC_FRAME_ORIGIN`.
- Imaris HDF channel `Gain`/`Pinhole`/`Min`/`Max`/`MicroscopyMode` attributes.
- SimplePCI/HCImage TIFF typed INI metadata (objective/binning/camera/exposure/
  calibration); Columbus/Operetta plate-scalar global metadata.
- OBF
- Olympus OIR
- CellWorX / MetaXpress
- I2I
- JDCE
- SimplePCI
- Volocity clipping
- KLB
- Olympus APL
- HRD-GDF
- Hamamatsu NAF
- Burleigh
- Leica LOF bounded channel and structured scalar metadata projection
- MNG
- 3i SlideBook
- Openlab LIFF bounded plane/name metadata and OME original-metadata annotations
- IMOD
- Bruker MRI / ParaVision
- iVision IPM packed 16-bit color and square-root transfer Java-parity
  unsupported branches
- Fabricated/non-upstream duplicate readers removed from registration:
  Bruker OPUS, ISS Vista FLIM, duplicate Lambert FLIM / Volocity variants, and
  the Sedat/Woolz inventions.
- APNG explicit animation rejection with regression coverage.
- Native LIF/ND2/CZI writer requests explicitly reject with no-Java-writer
  rationale.
