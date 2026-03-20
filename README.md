# bioformats-rs

A pure-Rust reimplementation of [Bio-Formats](https://www.openmicroscopy.org/bio-formats/) — a library for reading (and writing) scientific image formats used in microscopy, medical imaging, and astronomy. No JVM, no native dependencies.

## Quick start

```rust
use bioformats::{ImageReader, ImageWriter, ImageMetadata, PixelType};
use std::path::Path;

// --- Reading ---
let mut reader = ImageReader::open(Path::new("image.tif"))?;

let meta = reader.metadata();
println!("{}x{} px, {} planes, {:?}", meta.size_x, meta.size_y, meta.image_count, meta.pixel_type);

for i in 0..meta.image_count {
    let plane: Vec<u8> = reader.open_bytes(i)?;
    // plane is raw little-endian pixel data
}

// --- Writing ---
let mut meta = ImageMetadata::default();
meta.size_x = 512;
meta.size_y = 512;
meta.size_z = 10;
meta.image_count = 10;
meta.pixel_type = PixelType::Uint16;

let planes: Vec<Vec<u8>> = (0..10).map(|_| vec![0u8; 512 * 512 * 2]).collect();
ImageWriter::save(Path::new("output.tif"), &meta, &planes)?;
```

## Supported formats

### Read + Write

| Format | Extensions | Notes |
|--------|-----------|-------|
| TIFF / OME-TIFF / BigTIFF | `.tif` `.tiff` `.btf` | Full IFD parser; strip and tile layout; LZW, Deflate, PackBits, JPEG, Zstd |
| PNG | `.png` | 8-bit and 16-bit; grayscale and RGB |
| JPEG | `.jpg` `.jpeg` | 8-bit RGB |
| BMP | `.bmp` | 8-bit RGB |
| TGA | `.tga` | 8-bit |
| PNM / PGM / PPM | `.pnm` `.pgm` `.ppm` `.pbm` `.pfm` | |
| ICS / ICS2 | `.ics` | Image Cytometry Standard; gzip optional |
| MRC / CCP4 | `.mrc` `.mrcs` `.map` `.ccp4` | Cryo-EM; uint8/16, int16, float32/64 |
| FITS | `.fits` `.fit` `.fts` | 2880-byte blocks; big-endian; multi-plane |
| NRRD | `.nrrd` `.nhdr` | Raw and gzip encodings |
| MetaImage | `.mha` `.mhd` | ITK/VTK; inline and detached data file |

### Read only

| Format | Extensions | Notes |
|--------|-----------|-------|
| GIF | `.gif` | Via `image` crate |
| WebP | `.webp` | Via `image` crate |
| OpenEXR | `.exr` | Via `image` crate |
| HDR / RGBE | `.hdr` `.rgbe` | Radiance HDR |
| DDS | `.dds` | DirectDraw Surface |
| Farbfeld | `.ff` | |
| Leica LIF | `.lif` | Binary container with UTF-16 XML metadata |
| Nikon ND2 | `.nd2` | Chunk-based; uncompressed and zlib |
| Zeiss CZI | `.czi` | ZISRAWFILE segments; uncompressed, JPEG, LZW, Zstd |

## API overview

### `ImageReader` — auto-detecting reader

Format is detected automatically from magic bytes first, then file extension.

```rust
use bioformats::ImageReader;

let mut reader = ImageReader::open(path)?;

// Multi-series files (e.g. LIF containers with multiple acquisitions)
for s in 0..reader.series_count() {
    reader.set_series(s)?;
    let meta = reader.metadata();
    println!("Series {}: {}x{}", s, meta.size_x, meta.size_y);
}

// Read a sub-region (avoids loading the entire plane)
let tile = reader.open_bytes_region(
    /*plane*/ 0,
    /*x*/ 128, /*y*/ 128, /*w*/ 64, /*h*/ 64,
)?;

// Pyramid levels (where supported, e.g. tiled TIFF)
println!("{} resolution levels", reader.resolution_count());
reader.set_resolution(1)?; // switch to half-resolution
```

### `ImageMetadata`

```rust
pub struct ImageMetadata {
    pub size_x: u32,            // width in pixels
    pub size_y: u32,            // height in pixels
    pub size_z: u32,            // number of Z planes
    pub size_c: u32,            // number of channels
    pub size_t: u32,            // number of time points
    pub pixel_type: PixelType,  // Int8/Uint8/Int16/Uint16/Int32/Uint32/Float32/Float64/Bit
    pub bits_per_pixel: u8,
    pub image_count: u32,       // total planes = size_z * size_c * size_t
    pub dimension_order: DimensionOrder,
    pub is_rgb: bool,
    pub is_interleaved: bool,   // RGBRGB… vs RRR…GGG…BBB…
    pub is_indexed: bool,       // palette image
    pub is_little_endian: bool,
    pub resolution_count: u32,
    pub series_metadata: HashMap<String, MetadataValue>, // format-specific key/values
    pub lookup_table: Option<LookupTable>,
}
```

### `ImageWriter` — auto-detecting writer

```rust
use bioformats::{ImageWriter, ImageMetadata, PixelType};

// One-shot convenience
ImageWriter::save(path, &meta, &planes)?;

// Streaming (for large Z-stacks)
let mut w = ImageWriter::open(path, &meta)?;
for (i, plane) in planes.iter().enumerate() {
    w.save_bytes(i as u32, plane)?;
}
w.close()?;
```

### Format-specific writers

Access compression options through the crate-level types:

```rust
use bioformats_tiff::{TiffWriter, WriteCompression};
use bioformats_common::writer::FormatWriter;

let mut writer = TiffWriter::new().with_compression(WriteCompression::Deflate);
writer.set_metadata(&meta)?;
writer.set_id(Path::new("compressed.tif"))?;
writer.save_bytes(0, &plane_data)?;
writer.close()?;
```

### `FormatReader` trait

Implement this to add a new format:

```rust
use bioformats_common::{reader::FormatReader, metadata::ImageMetadata, error::Result};

struct MyReader { /* ... */ }

impl FormatReader for MyReader {
    fn is_this_type_by_bytes(&self, header: &[u8]) -> bool { /* magic check */ }
    fn is_this_type_by_name(&self, path: &Path) -> bool { /* extension check */ }
    fn set_id(&mut self, path: &Path) -> Result<()> { /* parse header */ }
    fn close(&mut self) -> Result<()> { Ok(()) }
    fn series_count(&self) -> usize { 1 }
    fn set_series(&mut self, _: usize) -> Result<()> { Ok(()) }
    fn series(&self) -> usize { 0 }
    fn metadata(&self) -> &ImageMetadata { &self.meta }
    fn open_bytes(&mut self, plane: u32) -> Result<Vec<u8>> { /* read pixels */ }
    fn open_bytes_region(&mut self, plane: u32, x: u32, y: u32, w: u32, h: u32) -> Result<Vec<u8>> { /* crop */ }
    fn open_thumb_bytes(&mut self, plane: u32) -> Result<Vec<u8>> { /* small preview */ }
}
```

## Workspace structure

```
bioformats-rs/
├── crates/
│   ├── bioformats/             # Public facade: ImageReader, ImageWriter, re-exports
│   ├── bioformats-common/      # Shared: FormatReader/Writer traits, ImageMetadata,
│   │                           #   PixelType, error types, codecs, endian utils
│   ├── bioformats-tiff/        # TIFF / OME-TIFF / BigTIFF (from scratch)
│   ├── bioformats-png/         # PNG
│   ├── bioformats-jpeg/        # JPEG
│   ├── bioformats-bmp/         # BMP
│   ├── bioformats-raster/      # GIF, TGA, WebP, PNM, HDR, EXR, DDS, Farbfeld
│   ├── bioformats-ics/         # ICS / ICS2 (fluorescence microscopy)
│   ├── bioformats-mrc/         # MRC / CCP4 (cryo-EM)
│   ├── bioformats-fits/        # FITS (astronomy)
│   ├── bioformats-nrrd/        # NRRD (medical imaging)
│   ├── bioformats-metaimage/   # MHA / MHD MetaImage (ITK/VTK)
│   ├── bioformats-lif/         # Leica LIF
│   ├── bioformats-nd2/         # Nikon ND2
│   └── bioformats-czi/         # Zeiss CZI (ZISRAWFILE)
```

Core traits (`FormatReader`, `FormatWriter`, `ImageMetadata`, `PixelType`) live in `bioformats-common` so format crates can implement them without circular dependencies.

## Pixel data layout

`open_bytes` returns a flat `Vec<u8>` containing raw pixel samples, row-major (top-left origin), with the following conventions:

- **Multi-byte samples** (16-bit, 32-bit, float): little-endian byte order (except FITS, which is big-endian as per the standard)
- **Interleaved RGB** (`is_interleaved = true`): `R G B R G B …`
- **Planar multi-channel** (`is_interleaved = false`): all of channel 0, then all of channel 1, …
- **Palette images** (`is_indexed = true`): each byte is a colour-map index; the table is in `meta.lookup_table`

```rust
let meta = reader.metadata();
let bps = meta.pixel_type.bytes_per_sample(); // bytes per sample
let row_bytes = meta.size_x as usize * meta.size_c as usize * bps;
let plane = reader.open_bytes(0)?;
assert_eq!(plane.len(), meta.size_y as usize * row_bytes);
```

## TIFF details

The TIFF reader is implemented from scratch (no dependency on the `tiff` crate) to support the full range of bioimaging TIFF variants:

- Classic TIFF (32-bit offsets) and BigTIFF (64-bit offsets)
- Strip-based and tile-based storage
- `open_bytes_region` reads only the required strips/tiles
- Compression: None, LZW, Deflate/Zlib, PackBits, JPEG (old and new style), Zstd
- Predictor: horizontal differencing for 8-bit and 16-bit data
- Photometric: MinIsWhite, MinIsBlack, RGB, YCbCr, Palette (with LUT)
- All pixel types: Uint8/16/32, Int8/16/32, Float32/64, Bit

The TIFF writer supports None, Deflate, and LZW compression, and writes valid multi-IFD files for Z-stacks/time series.

## Planned (not yet implemented)

- **ND2**: JPEG2000-compressed frames (requires an external J2K decoder)
- **CZI**: JPEG-XR compression
- **Phase 3+**: HDF5/N5/Zarr, DICOM, NetCDF, Olympus OIF/OIB, Andor SIF, and the remainder of the Bio-Formats format list
- **Write support** for LIF, ND2, CZI
- **OME-XML** structured metadata output (basic support added for CZI, OME-TIFF, OME-XML via `reader.ome_metadata()`)
- **Pyramid writing** for tiled multi-resolution TIFF

## Comparison with Java Bio-Formats

| Feature | Java Bio-Formats | bioformats-rs |
|---------|-----------------|---------------|
| Formats | 200+ | ~20 (Phase 1–2 complete) |
| JVM dependency | Required | None |
| Python bindings | Via scyjava | None (pure Rust) |
| Metadata output | OME-XML string | Structured Rust types |
| Write support | Most formats | TIFF, PNG, JPEG, BMP, TGA, ICS, MRC, FITS, NRRD, MetaImage |
| Pyramid / tiled read | ✓ | ✓ (TIFF) |

### Performance

Reading all pixel data from a 512×512 2-channel OME-TIFF (`tubhiswt_C0.ome.tif`, Bio-Formats 8.6.0 vs Rust release build, macOS, 3 warmup + 10 measured iterations):

| | Time | Throughput |
|--|------|-----------|
| Java Bio-Formats | 22.1 ms | 11.6 MiB/s |
| bioformats-rs | 1.6 ms | 171.3 MiB/s |
| **Speedup** | **13.9×** | |

Reproduce with `./bench/run.sh` from the repo root (requires `java` and `bioformats_package.jar` in the repo root).

## License

Licensed under either of [Apache License 2.0](LICENSE-APACHE) or [MIT License](LICENSE-MIT) at your option.
