# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Project Overview

Pure-Rust reimplementation of [Bio-Formats](https://www.openmicroscopy.org/bio-formats/) — a library for reading/writing scientific image formats used in microscopy, medical imaging, and astronomy. No JVM, no native dependencies (except optional features like `jpeg2k`/`openslide`).

The `java-bioformats/` directory is the upstream Java reference implementation — **read-only, do not modify**.

This is experimental software. The code need not be correct. The authoritative implementation is the original Java Bio-Formats.

## Translation Parity Rule

Prefer line-by-line auditability against the Java Bio-Formats source. Rust-only
helper functions should be removed or inlined by default because they prevent
direct audit against the original code and can drift from it. The only accepted
helper exception is an adapter for a Java library call that has been replaced by
a Rust library or API with a different shape; those helpers must preserve Java
behavior rather than introduce a new abstraction. Do not introduce new helper
layers during parity work unless they meet that adapter exception.

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

Licensing split (mirrors upstream Bio-Formats' `formats-bsd`/`formats-gpl` modules):
```bash
cargo build --no-default-features --features "jpegxr,zarr,tissuefaxs"   # BSD-2-Clause only: excludes all GPL-derived readers
```
The `gpl` feature is on by default (full format coverage, matching this crate's own
`GPL-2.0-or-later` license). Disabling it excludes every reader ported from upstream's
GPL `formats-gpl` module from compilation — not just from the registry — so a
`--no-default-features` build without `gpl` is a genuinely BSD-2-Clause-only binary.
See `src/formats/gpl/` vs `src/formats/bsd/` below.

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
│   ├── gpl/             # Readers ported from upstream's GPL-2.0-or-later `formats-gpl`
│   │                     # module (czi.rs, nd2.rs, lif.rs, tiff_wrappers.rs, hcs2.rs,
│   │                     # sem.rs, spm.rs, camera2.rs, ...). Gated behind the `gpl`
│   │                     # cargo feature (on by default) — see Commands above.
│   ├── bsd/              # Readers ported from upstream's BSD-2-Clause `formats-bsd`
│   │                     # module (avi.rs, dicom.rs, ics.rs, jpeg.rs, ...). Always built.
│   ├── misc.rs, misc4.rs            # Miscellaneous/stub readers (mixed BSD/GPL, gated per-item)
│   ├── extended.rs                  # Extended format set (mixed BSD/GPL, gated per-item)
│   └── flim2.rs                     # FLIM/flow cytometry (mixed BSD/GPL, gated per-item)
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

1. Check the reader's license in upstream Bio-Formats (`components/formats-bsd/` vs
   `components/formats-gpl/` in `java-bioformats/`) and create the new module in
   `src/formats/bsd/` or `src/formats/gpl/` accordingly (or add to an existing mixed
   category module in `src/formats/` directly, gating the new GPL item with
   `#[cfg(feature = "gpl")]` if the file also holds BSD readers).
2. Implement `FormatReader` and/or `FormatWriter` from `common/`
3. Register in `src/reader_order.rs`/`src/registry.rs` and/or `src/writer_order.rs`; GPL
   dispatch arms/blocks need `#[cfg(feature = "gpl")]` (see existing arms for the pattern)
4. Reader ordering matters: magic-byte detectors first, extension-only last

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
