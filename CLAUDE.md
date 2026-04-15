# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Pure-Rust reimplementation of [Bio-Formats](https://www.openmicroscopy.org/bio-formats/) — a library for reading/writing scientific image formats used in microscopy, medical imaging, and astronomy. No JVM, no native dependencies (except optional features like `jpeg2k`/`openslide`).

The `java-bioformats/` directory is the upstream Java reference implementation — **read-only, do not modify**.

This is experimental software. The code need not be correct. The authoritative implementation is the original Java Bio-Formats.

## Commands

All commands run from the repo root:

```bash
cargo build                          # Build entire workspace
cargo test                           # Run all tests
cargo test -- format_tests           # Run format integration tests
cargo test -- write_test             # Run write/round-trip tests
cargo test -- <test_name>            # Run a specific test by name
```

Optional features:
```bash
cargo build --features jpegxr        # Enable JPEG-XR codec
cargo build --features openslide     # Enable OpenSlide-based whole-slide readers
```

Benchmarks: `./bench/run.sh` (requires `java` and `bioformats_package.jar` in repo root).

## Architecture

This is a **single crate** (not a Cargo workspace). All code lives under `src/`.

### Module Layout

```
src/
├── lib.rs              # Public API re-exports
├── common/             # Shared types used by all format modules
│   ├── reader.rs       # FormatReader trait (16 methods)
│   ├── writer.rs       # FormatWriter trait
│   ├── metadata.rs     # ImageMetadata, MetadataLevel, ModuloAnnotation
│   ├── ome_metadata.rs # 21 OME types (Image, Channel, Instrument, ROI, HCS plate...)
│   ├── codec.rs        # Compression/decompression (LZW, Deflate, PackBits, JPEG, Zstd, etc.)
│   ├── pixel_type.rs   # PixelType (9 variants)
│   ├── endian.rs       # Byte-order utilities
│   ├── io.rs           # File I/O helpers (peek_header, etc.)
│   └── error.rs        # BioFormatsError
├── tiff/               # TIFF/BigTIFF/OME-TIFF (from scratch, not the `tiff` crate)
│   ├── reader.rs       # TiffReader + pyramid SubIFD support
│   ├── writer.rs       # TiffWriter + PyramidOmeTiffWriter
│   ├── ifd.rs          # IFD parsing, 20 compression types
│   └── compression.rs  # Decompression dispatch
├── formats/            # ~67 modules implementing ~182 readers, organized by category:
│   ├── mod.rs          # Module declarations
│   ├── czi.rs, nd2.rs, lif.rs, ...  # Major scientific formats
│   ├── misc.rs, misc4.rs            # Miscellaneous/stub readers
│   ├── extended.rs                  # Extended format set
│   ├── hcs2.rs                      # High-content screening
│   ├── sem.rs                       # Electron microscopy
│   ├── spm.rs                       # Scanning probe microscopy
│   ├── camera2.rs                   # Camera/RAW formats
│   ├── flim2.rs                     # FLIM/flow cytometry
│   └── tiff_wrappers.rs            # TIFF-based whole-slide formats
├── registry.rs         # ImageReader: format auto-detection (magic bytes → extension fallback)
├── writer_registry.rs  # ImageWriter: 14 format writers (extension-based)
├── wrappers.rs         # 5 reader wrappers (ChannelSep/Merge/Fill, DimSwap, MinMax)
├── cache.rs            # CachedReader (LRU/Rectangle/Crosshair strategies)
├── memoizer.rs         # Metadata memoization (.bfmemo files)
├── stitcher.rs         # FileStitcher + FilePattern + AxisGuesser
└── bin/bioformats_convert.rs  # CLI tool
```

### Key Types

- **`ImageReader`** (`registry.rs`) — Auto-detecting reader. Tries magic bytes first, then extension fallback. Delegates to the matching `FormatReader`.
- **`ImageWriter`** (`writer_registry.rs`) — Auto-detecting writer. Selects format by extension.
- **`FormatReader`** trait (`common/reader.rs`) — Implement to add a new read format.
- **`FormatWriter`** trait (`common/writer.rs`) — Implement to add a new write format.
- **`ImageMetadata`** — Strongly typed metadata (dimensions, pixel type, channel info, etc.).
- **`OmeMetadata`** — Structured OME metadata for CZI, OME-TIFF, OME-XML.

### Adding a Format

1. Create a new module in `src/formats/` (or add to an existing category module)
2. Implement `FormatReader` and/or `FormatWriter` from `common/`
3. Register in `src/registry.rs` (`all_readers()` list) and/or `src/writer_registry.rs`
4. Reader ordering in `all_readers()` matters: magic-byte detectors first, extension-only last

### Key Design Decisions

- **No JVM, no native deps** — pure Rust only (some optional: hdf5, zstd, jpeg2k, openslide)
- **Metadata is strongly typed** — `ImageMetadata` structs, not OME-XML strings
- **Pixel data is raw `Vec<u8>`** — callers interpret bytes according to `PixelType`; little-endian except FITS (big-endian per spec)
- **Multi-series support** — `set_series()` switches context for container formats like LIF and ND2
- **TIFF is central** — many microscopy formats are TIFF variants; `src/tiff/` is designed for reuse by `tiff_wrappers.rs`, `lsm.rs`, `svs.rs`, `flex.rs`, etc.
- **~25 readers are stubs** — return `UnsupportedFormat` errors for proprietary/undocumented formats (see `FEATURES.md` for the full list)

### Tests

- `tests/format_tests.rs` — Format-specific integration tests
- `tests/write_test.rs` — Round-trip tests (write → read → verify)
- `tests/integration_test.rs` — Cross-format integration tests
- `tests/fixtures/` — Small test images
