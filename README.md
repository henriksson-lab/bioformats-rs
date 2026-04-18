# bioformats-rs

A pure-Rust reimplementation of [Bio-Formats](https://www.openmicroscopy.org/bio-formats/) 
— a library for reading (and writing) scientific image formats used in microscopy, medical imaging, and astronomy.
No JVM, no native dependencies.

**This package is still under development**


## This is an LLM-mediated faithful (hopefully) translation, not the original code!

Most users should probably first see if the existing original code works for them, unless they have reason otherwise. The original source
may have newer features and it has had more love in terms of fixing bugs. In fact, we aim to replicate bugs if they are present, for the
sake of reproducibility! (but then we might have added a few more in the process)

There are however cases when you might prefer this Rust version. We generally agree with [this page](https://rewrites.bio/)
but more specifically:
* We have had many issues with ensuring that our software works using existing containers (Docker, PodMan, Singularity). One size does not fit all and it eats our resources trying to keep up with every way of delivering software
* Common package managers do not work well. It was great when we had a few Linux distributions with stable procedures, but now there are just too many ecosystems (Homebrew, Conda). Conda has an NP-complete resolver which does not scale. Homebrew is only so-stable. And our dependencies in Python still break. These can no longer be considered professional serious options. Meanwhile, Cargo enables multiple versions of packages to be available, even within the same program(!)
* The future is the web. We deploy software in the web browser, and until now that has meant Javascript. This is a language where even the == operator is broken. Typescript is one step up, but a game changer is the ability to compile Rust code into webassembly, enabling performance and sharing of code with the backend. Translating code to Rust enables new ways of deployment and running code in the browser has especial benefits for science - researchers do not have deep pockets to run servers, so pushing compute to the user enables deployment that otherwise would be impossible
* Old CLI-based utilities are bad for the environment(!). A large amount of compute resources are spent creating and communicating via small files, which we can bypass by using code as libraries. Even better, we can avoid frequent reloading of databases by hoisting this stage, with up to 100x speedups in some cases. Less compute means faster compute and less electricity wasted
* LLM-mediated translations may actually be safer to use than the original code. This article shows that [running the same code on different operating systems can give somewhat different answers](https://doi.org/10.1038/nbt.3820). This is a gap that Rust+Cargo can reduce. Typesafe interfaces also reduce coding mistakes and error handling, as opposed to typical command-line scripting

But:

* **This approach should still be considered experimental**. The LLM technology is immature and has sharp corners. But there are opportunities to reap, and the genie is not going back to the bottle. This translation is as much aimed to learn how to improve the technology and get feedback on the results.
* Translations are not endorsed by the original authors unless otherwise noted. **Do not send bug reports to the original developers**. Use our Github issues page instead.
* **Do not trust the benchmarks on this page**. They are used to help evaluate the translation. If you want improved performance, you generally have to use this code as a library, and use the additional tricks it offers. We generally accept performance losses in order to reduce our dependency issues
* **Check the original Github pages for information about the package**. This README is kept sparse on purpose. It is not meant to be the primary source of information



## License

The license is derived from Bio-formats, as code is derived from this source. License will be reanalyzed after
full refactoring

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
| PNM / PGM / PPM | `.pnm` `.pgm` `.ppm` `.pbm` `.pfm` | Via `image` crate |
| Leica LIF | `.lif` | Binary container with UTF-16 XML metadata |
| Nikon ND2 | `.nd2` | Chunk-based; uncompressed and zlib |
| Zeiss CZI | `.czi` | ZISRAWFILE segments; uncompressed, JPEG, LZW, Zstd |
| DICOM | `.dcm` | Unencapsulated pixel data; uint8/16, int16 |
| NIfTI-1 / Analyze 7.5 | `.nii` `.nii.gz` `.hdr` `.img` | gzip supported |
| Zeiss LSM | `.lsm` | TIFF-based with CZ_LSMInfo metadata |
| Applied Precision DeltaVision | `.dv` `.r3d` | Binary header + raw planes |
| Andor SIF | `.sif` | ASCII header + float32 pixel data |
| Olympus FV1000 OIF | `.oif` | INI-style header + companion TIFFs |
| Gatan DM3 / DM4 | `.dm3` `.dm4` | Tag-tree structure; EM format |
| Bio-Rad PIC | `.pic` | Confocal microscopy |
| Princeton SPE | `.spe` | Spectroscopy / CCD cameras |
| Norpix StreamPix | `.seq` | Video sequence; raw frames |
| Hamamatsu DCIMG | `.dcimg` | Scientific CMOS camera format |

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
- **Write support** for LIF, ND2, CZI, PNM
- **OME metadata**: `reader.ome_metadata()` returns structured physical sizes, channel names, and plane positions for CZI, OME-TIFF, and OME-XML files; richer parsing (instrument, experimenter) not yet implemented
- **Pyramid writing** for tiled multi-resolution TIFF

## Comparison with Java Bio-Formats

| Feature | Java Bio-Formats | bioformats-rs |
|---------|-----------------|---------------|
| Formats | 200+ | ~30 with full pixel read support; 150+ registered |
| JVM dependency | Required | None |
| Python bindings | Via scyjava | None (pure Rust) |
| Metadata output | OME-XML / `IMetadata` | `ImageMetadata` (always) + `OmeMetadata` for CZI/OME-TIFF/OME-XML |
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

