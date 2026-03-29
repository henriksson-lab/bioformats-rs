# Feature Gap: bioformats-rs vs Java Bio-Formats

Ground-truth audit performed 2026-03-29 by reading both codebases.

## Current Stats (updated 2026-03-29)

| Metric | Java | Rust | Gap |
|--------|------|------|-----|
| Reader classes | 184 | 178 (120 real, 18 TIFF wrapper, 40 stub) | 40 stubs + 6 missing |
| Writers | 17 | 14 | 3 (V3Draw, JPEG2000Writer, PyramidOME-TIFF full impl) |
| Codecs | 21 | 20 (9 working + 11 stubs) | 0 registered, 11 awaiting full impl |
| TIFF compression types | 20 | 17 | 3 (JP2K lossy variants) |
| Wrappers | 8 | 5 | 3 (BufferedImageReader N/A) |
| Tests | Extensive | 33 | Need more |

## Remaining Gaps — Implementation Checklist

### CRITICAL

- [ ] C1. Convert ~40 stub readers to real implementations (most impactful: SlideBook, Volocity Library, MINC, QuickTime, MNG, OpenlabLIFF, Sedat, SmCamera, APL, ARF, I2I, JDCE, JPX, PCI, PDS, HIS, HRDGDF, TextImage, MRW, Yokogawa, LeicaLOF, APNG, PovRay, NAF, Burleigh, BD, Columbus, Operetta, ScanR, CellVoyager, Tecan, InCell3000, RCPNL, JEOL, Hitachi, Leo, ZeissLMS, IMOD, RHK, Quesant, JPK, WaTop, VgSam, UBM, Seiko, CanonRAW, Imacon, SBIG, IPW, FlowSight, IM3, SlideBook7, NDPIS, iVision, AFI-fluorescence, ImarisTIFF, XLEF, OIR, CellSens, VolocityClipping, MicroCT, BioRadSCN, SlidebookTIFF)
- [x] C2. PyramidOME-TIFF writer (multi-resolution SubIFD output)

### HIGH

- [x] H1. LosslessJPEG codec — already supported via jpeg-decoder SOF3
- [x] H2. ChannelFiller wrapper (fills missing channel data)
- [x] H3. AxisGuesser (heuristic dimension detection from filenames)
- [x] H4. FilePattern (glob pattern parser for file sequences)
- [x] H5. HCS plate model types (Plate, Well, WellSample, Screen)
- [x] H6. Plane cache framework (LRU/Rectangle/Crosshair strategies via CachedReader)
- [x] H7. Parse ROI elements from OME-XML into OmeROI structs
- [x] H8. Parse Annotation elements from OME-XML into OmeAnnotation structs
- [x] H9. Parse Experimenter elements from OME-XML into OmeExperimenter structs

### MEDIUM

- [x] M1. CCITT Group 3/4 fax TIFF compression (stub — returns error, compression type registered)
- [x] M2. Video codecs: MSRLE, MJPB, QTRLE, RPZA (stubs — return errors)
- [x] M3. EPS writer
- [x] M4. ImageConverter CLI tool (`bioformats-convert` binary)
- [ ] M5. Enrich remaining 14 thin TIFF wrappers with vendor metadata (Ventana, NikonElements, FEI, OlympusSIS, Improvision, ZeissApotome, MolecularDevices, Metaxpress, SimplePCI, IonpathMIBI, MIAS, Trestle, TissueFAXS, Mikroscan)
- [ ] M6. Add 6 missing reader classes (FilePatternReader, ScreenReader, KLBReader, OBFReader, TileJPEGReader, TextReader)
- [ ] M7. JPEG2000 lossy (33004), ALT_JPEG2000 (33005), ALT_JPEG (33007) TIFF compression variants
- [x] M8. Nikon TIFF compression (34713) — stub registered in compression enum

### LOW

- [x] L1. NikonCodec (NEF RAW decompression) — stub
- [x] L2. LZO codec — stub
- [x] L3. Base64 codec — fully implemented
- [x] L4. HuffmanCodec (standalone, not JPEG-internal) — stub
- [x] L5. Thunderscan TIFF compression — registered in compression enum
- [ ] L6. V3Draw writer
- [ ] L7. Experiment/Dataset OME types
- [x] L8. MapAnnotation parsing from OME-XML
- [ ] L9. Oracle testing harness (compare Rust output vs Java bioformats_package.jar)
- [ ] L10. Fuzz testing (cargo-fuzz on TIFF, CZI, ND2 parsers)

## What Already Works

Implemented during the 2026-03-28/29 sessions:

- Phase 1: 60 stub readers error instead of returning fake data
- Phase 2: JPEG2000 codec (jpeg2k), JPEG-XR codec (optional jpegxr feature), pyramid SubIFD parsing, OME-TIFF writer
- Phase 3: 15 OME metadata types (Instrument, Objective, Detector, LightSource, Filter, Dichroic, LightPath, ROI, Shape, Experimenter, Annotation, Plate, Well, WellSample, Screen) + instrument/ROI/annotation/experimenter parsing from OME-XML + MetadataLevel + ModuloAnnotation
- Phase 4: ChannelSeparator, ChannelMerger, ChannelFiller, DimensionSwapper, MinMaxCalculator wrappers + Memoizer (bincode) + FileStitcher + FilePattern + AxisGuesser + CachedReader (LRU/Rectangle/Crosshair)
- Phase 5: NDPI, SVS/Aperio, Leica SCN, FluoView enriched with vendor metadata
- Phase 6: 14 writers (TIFF, PNG, JPEG, BMP, TGA, ICS, MRC, FITS, NRRD, MetaImage, OME-XML, DICOM, AVI, EPS) + PyramidOME-TIFF writer + bioformats-convert CLI
- Codecs: LZW, Deflate, PackBits, JPEG (lossy+lossless), Zstd, JPEG2000, JPEG-XR + stubs for CCITT G3/G4, MSRLE, MJPB, QTRLE, RPZA, Nikon, LZO, Huffman, Base64, Thunderscan
