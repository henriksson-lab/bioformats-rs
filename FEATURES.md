# Feature Gap: bioformats-rs vs Java Bio-Formats

Ground-truth audit 2026-03-29.

## Stats

| Metric | Java | Rust |
|--------|------|------|
| Registered readers | 184 | 182 |
| Real readers | ~184 | ~157 |
| Stub readers | 0 | 25 |
| Writers | 17 | 14 + PyramidOME-TIFF |
| Working codecs | 21 | 9 (+LosslessJPEG via jpeg-decoder) |
| Codec stubs | — | 12 |
| TIFF compression types | 20 | 20 |
| Reader wrappers | 8 | 5 |
| OME metadata types | Many | 21 |
| Tests | Extensive | 33 |

## Remaining Stubs (25 readers)

All return descriptive `UnsupportedFormat` errors explaining why.

| # | Reader | Extension | File | Reason |
|---|--------|-----------|------|--------|
| 1 | QuickTimeReader | .mov .qt | misc.rs | MOV atom-based container parsing |
| 2 | VolocityLibraryReader | .acff | misc.rs | OLE2/Compound Document format |
| 3 | SlideBookReader | .sld | misc.rs | Proprietary undocumented binary |
| 4 | OpenlabLiffReader | .liff | misc.rs | Proprietary undocumented binary |
| 5 | SedatReader | .sedat | misc.rs | Proprietary undocumented binary |
| 6 | SmCameraReader | .smc | misc.rs | Proprietary undocumented binary |
| 7 | AplReader | .apl | misc4.rs | Applied Precision proprietary |
| 8 | I2iReader | .i2i | misc4.rs | Proprietary undocumented |
| 9 | JdceReader | .jdce | misc4.rs | Proprietary undocumented |
| 10 | PciReader | .pci | misc4.rs | Media Cybernetics proprietary |
| 11 | HrdgdfReader | .gdf | misc4.rs | Proprietary undocumented binary |
| 12 | FilePatternReaderStub | .pattern | misc4.rs | Needs glob/regex expansion |
| 13 | KlbReader | .klb | misc4.rs | No pure-Rust KLB decoder |
| 14 | ObfReader | .obf | misc4.rs | Fallback; ImspectorReader handles OMAS_BF_ |
| 15 | LeicaLofReader | .lof | extended.rs | Leica proprietary binary |
| 16 | NafReader | .naf | extended.rs | Proprietary undocumented |
| 17 | BurleighReader | .img | extended.rs | .img too generic for reliable detection |
| 18 | FlowSightReader | .cif | flim2.rs | Amnis FlowSight proprietary |
| 19 | IvisionReader | .ipm | flim2.rs | BioVision Technologies proprietary |
| 20 | OirReader | .oir | flim2.rs | Olympus OIR requires OLE2 parsing |
| 21 | VolocityClippingReader | .acff | flim2.rs | OLE2/Compound Document parsing |
| 22 | ImrodReader | .mod | sem.rs | 3D mesh format, not an image |
| 23 | WoolzReader | .wlz | legacy.rs | Woolz graph-based format |
| 24 | PictReader | .pict .pct | legacy.rs | Apple PICT legacy format |
| 25 | XrmReader | .xrm .txrm | xrm.rs | Zeiss XRM OLE2-based |

## Architecture

```
bioformats (facade crate)
├── common/
│   ├── reader.rs       FormatReader trait (16 methods)
│   ├── writer.rs       FormatWriter trait
│   ├── metadata.rs     ImageMetadata, MetadataLevel, ModuloAnnotation
│   ├── ome_metadata.rs 21 types (Image, Channel, Instrument, ROI, HCS plate...)
│   ├── codec.rs        21 codec functions
│   ├── pixel_type.rs   PixelType (9 variants)
│   └── error.rs        BioFormatsError
├── tiff/
│   ├── reader.rs       TiffReader + pyramid SubIFD
│   ├── writer.rs       TiffWriter + PyramidOmeTiffWriter
│   ├── ifd.rs          20 compression types
│   └── compression.rs  Decompression dispatch
├── formats/            67 modules, 182 readers
├── wrappers.rs         5 wrappers (ChannelSep/Merge/Fill, DimSwap, MinMax)
├── cache.rs            CachedReader (LRU/Rectangle/Crosshair)
├── memoizer.rs         Metadata memoization (.bfmemo)
├── stitcher.rs         FileStitcher + FilePattern + AxisGuesser
├── registry.rs         ImageReader auto-detection
├── writer_registry.rs  ImageWriter (14 formats)
└── bin/bioformats_convert.rs  CLI tool
```
