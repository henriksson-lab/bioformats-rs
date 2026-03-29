# Feature Gap: bioformats-rs vs Java Bio-Formats

Ground-truth audit performed 2026-03-29 by reading both codebases.

## Current Stats (final update 2026-03-29)

| Metric | Java | Rust | Notes |
|--------|------|------|-------|
| Registered readers | 184 | 182 | 2 Java-only (ScreenReader, TileJPEGReader = AWT-specific) |
| Real readers | ~184 | ~128 | Up from ~120 — converted TextImage, HIS, CanonRAW, SBIG |
| TIFF wrappers (enriched) | ~30 | 11 enriched | NDPI, SVS, LeicaSCN, FluoView, Ventana, NikonElements, FEI, OlympusSIS, Improvision, ZeissApotome, MolecularDevices |
| TIFF wrappers (thin) | — | ~11 | HCS2 plate readers + extended (DNG, QPTIFF) + camera2 (PhotoshopTiff) |
| Stub readers | 0 | ~36 | Down from ~60. Formats recognized but reading not implemented |
| Writers | 17 | 14 | Missing: V3Draw, JPEG2000Writer, CellH5Writer |
| Codecs (working) | 21 | 9 | LZW, Deflate, PackBits, JPEG+Lossless, Zstd, JPEG2000, JPEG-XR, Base64 |
| Codecs (stubs) | — | 12 | CCITT G3/G4, MSRLE, MJPB, QTRLE, RPZA, Nikon, LZO, Huffman, Thunderscan |
| TIFF compression types | 20 | 20 | All recognized (including JPEG2000 variants 33003-33007) |
| Reader wrappers | 8 | 5 | ChannelSeparator/Merger/Filler, DimensionSwapper, MinMaxCalculator |
| OME metadata types | Many | 21 | Full hierarchy: Instrument, ROI, Annotation, HCS Plate, Experiment, Dataset |
| Tests | Extensive | 33 | Round-trip tests for all writers + wrapper tests |

## Remaining Gaps

### Still TODO

- [ ] C1. ~36 stub readers need real format implementations (proprietary formats requiring reverse engineering)
- [ ] M5 partial. ~11 thin TIFF wrappers in HCS2/extended still use macro delegation
- [ ] L6. V3Draw writer
- [ ] L9. Oracle testing harness (Java jar comparison)
- [ ] L10. Fuzz testing

### Completed (all sessions combined)

- [x] C2. PyramidOME-TIFF writer
- [x] H1-H9. All HIGH items: LosslessJPEG, ChannelFiller, AxisGuesser, FilePattern, HCS plate model, CachedReader, ROI/Annotation/Experimenter parsing
- [x] M1-M4, M7-M8. CCITT stubs, video codec stubs, EPS writer, ImageConverter CLI, JPEG2000 variants, Nikon compression
- [x] L1-L5, L7-L8. All niche codecs, Experiment/Dataset types, MapAnnotation parsing

## Architecture Summary

```
bioformats (facade crate)
├── common/
│   ├── reader.rs      — FormatReader trait (16 methods)
│   ├── writer.rs      — FormatWriter trait
│   ├── metadata.rs    — ImageMetadata (19 fields), MetadataLevel, ModuloAnnotation
│   ├── ome_metadata.rs — 21 types: OmeMetadata, OmeImage, OmeChannel, OmePlane,
│   │                     OmeInstrument, OmeObjective, OmeDetector, OmeLightSource,
│   │                     OmeFilter, OmeDichroic, OmeLightPath, OmeROI, OmeShape,
│   │                     OmeExperimenter, OmeAnnotation, OmePlate, OmeWell,
│   │                     OmeWellSample, OmeScreen, OmeExperiment, OmeDataset
│   ├── codec.rs       — 21 codec functions (9 working + 12 stubs)
│   ├── pixel_type.rs  — PixelType enum (9 variants)
│   ├── error.rs       — BioFormatsError enum
│   └── io.rs          — I/O utilities
├── tiff/
│   ├── reader.rs      — TiffReader with pyramid SubIFD support
│   ├── writer.rs      — TiffWriter + PyramidOmeTiffWriter
│   ├── ifd.rs         — IFD parsing, 20 compression types
│   ├── parser.rs      — TIFF/BigTIFF parser
│   └── compression.rs — Compression dispatch
├── formats/           — 67 format modules, 182 registered readers
├── wrappers.rs        — ChannelSeparator, ChannelMerger, ChannelFiller,
│                        DimensionSwapper, MinMaxCalculator
├── cache.rs           — CachedReader (LRU/Rectangle/Crosshair strategies)
├── memoizer.rs        — Metadata memoization (.bfmemo files)
├── stitcher.rs        — FileStitcher, FilePattern, AxisGuesser
├── registry.rs        — ImageReader (auto-detecting facade)
├── writer_registry.rs — ImageWriter (auto-detecting writer, 14 formats)
└── bin/
    └── bioformats_convert.rs — CLI format conversion tool
```
