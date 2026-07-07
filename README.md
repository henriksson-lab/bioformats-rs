# bioformats-rs

A pure-Rust translation of [Bio-Formats](https://www.openmicroscopy.org/bio-formats/)
— a library for reading (and writing) scientific image formats used in microscopy, medical imaging, and astronomy.

The internal Metakit table reader used for Volocity is translated from
`ome.metakit.MetakitReader` in
[`ome/ome-metakit`](https://github.com/ome/ome-metakit) at commit
`b8b3a629a6dd9bf422949f6b175b9e310ba6e252`.

Current status: the tracked Java-to-Rust translation audit is complete; every
translated reader/writer row has passed two clean audits. Real-file and Java
parity coverage continue to expand as public fixtures become available.

## This is an LLM-mediated faithful (hopefully) translation, not the original code! 

Most users should probably first see if the existing original code works for them, unless they have reason otherwise. The original source
may have newer features and it has had more love in terms of fixing bugs. In fact, we aim to replicate bugs if they are present, for the
sake of reproducibility! (but then we might have added a few more in the process)

There are however cases when you might prefer this Rust version. We generally agree with [this manifesto](https://rewrites.bio/) but more specifically:
* We have had many issues with ensuring that our software works using existing containers (Docker, PodMan, Singularity). One size does not fit all and it eats our resources trying to keep up with every way of delivering software
* Common package managers do not work well. It was great when we had a few Linux distributions with stable procedures, but now there are just too many ecosystems (Homebrew, Conda). Conda has an NP-complete resolver which does not scale. Homebrew is only so-stable. And our dependencies in Python still break. These can no longer be considered professional serious options. Meanwhile, Cargo enables multiple versions of packages to be available, even within the same program(!)
* The future is the web. We deploy software in the web browser, and until now that has meant Javascript. This is a language where even the == operator is broken. Typescript is one step up, but a game changer is the ability to compile Rust code into webassembly, enabling performance and sharing of code with the backend. Translating code to Rust enables new ways of deployment and running code in the browser has especial benefits for science - researchers do not have deep pockets to run servers, so pushing compute to the user enables deployment that otherwise would be impossible
* Old CLI-based utilities are bad for the environment(!). A large amount of compute resources are spent creating and communicating via small files, which we can bypass by using code as libraries. Even better, we can avoid frequent reloading of databases by hoisting this stage, with up to 100x speedups in some cases. Less compute means faster compute and less electricity wasted
* LLM-mediated translations may actually be safer to use than the original code. This article shows that [running the same code on different operating systems can give somewhat different answers](https://doi.org/10.1038/nbt.3820). This is a gap that Rust+Cargo can reduce. Typesafe interfaces also reduce coding mistakes and error handling, as opposed to typical command-line scripting

But:

* **This approach should still be considered experimental**. The LLM technology is immature and has sharp corners. But there are opportunities to reap, and the genie is not going back into the bottle. This translation is as much aimed to learn how to improve the technology and get feedback on the results.
* Translations are not endorsed by the original authors unless otherwise noted. **Do not send bug reports to the original developers**. Use our Github issues page instead.
* **Do not overinterpret the benchmark reports**. They are used to help evaluate the translation. If you want improved performance, you generally have to use this code as a library, and use the additional tricks it offers. We generally accept performance losses in order to reduce our dependency issues
* **Check the original Github pages for information about the package**. This README is kept sparse on purpose. It is not meant to be the primary source of information
* **If you are the author of the original code and wish to move to Rust, you can obtain ownership of this repository and crate**. Until then, our commitment is to offer an as-faithful-as-possible translation of a snapshot of your code. If we find serious bugs, we will report them to you. Otherwise we will just replicate them, to ensure comparability across studies that claim to use package XYZ v.666. Think of this like a fancy Ubuntu .deb-package of your software - that is how we treat it

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

For canonical Bio-Formats format descriptions, extensions, and support ratings,
see the upstream
[Bio-Formats 8.5.0 supported formats table](https://bio-formats.readthedocs.io/en/v8.5.0/supported-formats.html).
The notes below only call out Rust-specific behavior, optional features, or
known deviations from the translated Bio-Formats behavior.

### Read + Write

| Format | Extensions | Rust-specific notes |
|--------|-----------|-------|
| TIFF / OME-TIFF | `.tif` `.tiff` `.tf2` `.tf8` `.btf` `.ome.tif` `.ome.tiff` `.ome.tf2` `.ome.tf8` `.ome.btf` | Auto-dispatched `.ome.*` writes ordinary OME-TIFF; use `PyramidOmeTiffWriter` directly for pyramid output. |
| PNG | `.png` | |
| JPEG | `.jpg` `.jpeg` | |
| BMP | `.bmp` | |
| TGA | `.tga` | |
| ICS / ICS2 | `.ics` | |
| MRC / CCP4 | `.mrc` `.mrcs` `.map` `.ccp4` | |
| FITS | `.fits` `.fit` `.fts` | |
| NRRD | `.nrrd` `.nhdr` | |
| MetaImage | `.mha` `.mhd` | Rust-only writer. |
| JPEG 2000 | `.jp2` `.j2k` `.j2c` `.jpc` | Write support is behind the default-on `jpeg2000-write` feature and is limited to single 2D lossless planes with 1 or 3 integer components. |
| DICOM | `.dcm` | Writer emits unencapsulated planes. |
| PNM / PGM / PPM | `.pnm` `.pgm` `.ppm` `.pbm` `.pfm` | Rust-only writer. |

Additional write-capable formats (read support listed elsewhere): OME-XML
(`.ome`, `.ome.xml`), APNG (`.apng`), AVI (`.avi`), EPS (`.eps`), QuickTime RAW
(`.mov`), CellH5 (`.ch5`), Java source image dumps (`.java`), and Vaa3D V3DRAW
(`.v3draw`).

### Read only

| Format | Extensions | Rust-specific notes |
|--------|-----------|-------|
| GIF | `.gif` | |
| WebP | `.webp` | |
| OpenEXR | `.exr` | |
| HDR / RGBE | `.hdr` `.rgbe` | |
| DDS | `.dds` | |
| Farbfeld | `.ff` | |
| Leica LIF | `.lif` | |
| Nikon ND2 | `.nd2` | |
| Zeiss CZI | `.czi` | JPEG-XR uses the default-on `jpegxr` feature. |
| NIfTI-1 / Analyze 7.5 | `.nii` `.nii.gz` `.hdr` `.img` | |
| Zeiss LSM | `.lsm` | |
| Applied Precision DeltaVision | `.dv` `.r3d` | |
| Andor SIF | `.sif` | |
| Olympus FV1000 OIF | `.oif` | |
| Gatan DM3 / DM4 | `.dm3` `.dm4` | |
| Bio-Rad PIC | `.pic` | |
| Princeton SPE | `.spe` | |
| Norpix StreamPix | `.seq` | Rust-only reader; Bio-Formats' `.seq` reader is Image-Pro Sequence. |
| Hamamatsu DCIMG | `.dcimg` | |

## Translation status (all readers)

A per-reader audit of the Java-to-Rust translation is tracked in
[`TOAUDIT.md`](https://github.com/henriksson-lab/bioformats-rs/blob/main/TOAUDIT.md).
Rows marked complete there have passed two clean audits against the corresponding
Bio-Formats Java reader/writer or are documented Rust-only additions.

The table below is a user-facing capability summary. It should not be read as the
translation audit state; codec gaps, optional features, or Rust-only support may
still be called out even when the Java translation audit is complete. The notes
column is intentionally limited to Rust-specific deviations from the upstream
Bio-Formats documentation.

- ✅ **Complete** — tracked audit is clean for translated behavior and the common
  read path is implemented.
- 🟡 **Partial** — user-visible codec, payload, metadata, or optional-feature gap
  remains.
- ⛔ **Stub** — detection only; `set_id` returns `UnsupportedFormat` (the format
  is proprietary/undocumented or needs a decoder/container parser not yet ported).

Readers with **no Bio-Formats counterpart** are *added code*, not translations,
so they are not rated here — see
[Added (non-upstream) readers](#added-non-upstream-readers) for that list.

### Standard image formats

| Format | Extensions | Status | Rust-specific notes |
|--------|-----------|:------:|-------|
| TIFF / BigTIFF / OME-TIFF | `.tif` `.tiff` `.btf` `.tf8` | ✅ | Zstd and JP2 tile paths are implemented. |
| PNG | `.png` | ✅ | APNG is routed separately. |
| JPEG | `.jpg` `.jpeg` | ✅ | |
| BMP | `.bmp` | ✅ | |
| PCX | `.pcx` | ✅ | |
| Imagic-5 | `.hed` `.img` | ✅ | |
| TGA | `.tga` `.tpic` | ✅ | |
| GIF | `.gif` | ✅ | |
| WebP / OpenEXR / HDR / DDS / Farbfeld / PNM | `.webp` `.exr` `.hdr` `.dds` `.ff` `.pnm` `.pgm` `.ppm` `.pbm` `.pfm` | ✅ | Some entries are Rust-only additions via `image`. |
| JPEG 2000 / JPX | `.jp2` `.j2k` `.j2c` `.jpc` `.jpx` | ✅ | |
| EPS / PostScript | `.eps` `.epsi` `.ps` | ✅ | |
| Photoshop PSD/PSB | `.psd` `.psb` | ✅ | |
| Khoros VIFF | `.xv` `.viff` | ✅ | |
| Apple PICT | `.pict` `.pct` | ✅ | |
| ZIP container | `.zip` | ✅ | |
| FilePattern dataset | `.pattern` | ✅ | |
| APNG | `.apng` | ✅ | |
| MNG | `.mng` | ✅ | |
| AVI (video) | `.avi` | ✅ | |
| QuickTime MOV/QT | `.mov` `.qt` | ✅ | Modern platform codecs are listed under missing external codecs. |
| Fake (test format) | `.fake` | ✅ | Test-only format. |

### Microscopy acquisition containers

| Format | Extensions | Status | Rust-specific notes |
|--------|-----------|:------:|-------|
| Zeiss ZVI | `.zvi` | ✅ | |
| Zeiss XRM/TXRM | `.xrm` `.txrm` `.txm` | ✅ | |
| OME-XML | `.ome` | ✅ | |
| Zeiss LSM | `.lsm` | ✅ | |
| Olympus FV1000 OIF/OIB | `.oif` `.oib` | ✅ | |
| Leica LEI | `.lei` | ✅ | |
| Leica TCS | `.xml` | ✅ | |
| MicroManager | `metadata.txt` `.json` | ✅ | |
| Visitech | `.xys` `.html` | ✅ | |
| Zeiss CZI | `.czi` | ✅ | JPEG-XR via default-on `jpegxr` feature. |
| Nikon ND2 | `.nd2` | ✅ | |
| Prairie View | `.xml` `.cfg` `.env` `.tif` | ✅ | |
| MetaMorph STK | `.stk` `.nd` | ✅ | |
| Leica XLEF | `.xlef` `.xlif` `.lms` | ✅ | Compressed/internal LMS Memory blocks return `UnsupportedFormat`. |
| Imaris IMS | `.ims` | ✅ | |
| Leica LIF | `.lif` | ✅ | |
| Volocity | `.mvd2` `.aisf` `.aiix` `.dat` `.atsf` | ✅ | Proprietary fixture-complete validation remains dataset-limited. |

### High-content screening (HCS)

| Format | Extensions | Status | Rust-specific notes |
|--------|-----------|:------:|-------|
| PerkinElmer FLEX | `.flex` `.mea` `.res` | ✅ | |
| InCell (GE) | `.xdce` `.xml` | ✅ | |
| PerkinElmer UltraVIEW | `.htm` `.tim` `.csv` `.zpo` | ✅ | |
| MIAS (Maia Scientific) | `.tif` | ✅ | |
| Operetta / Columbus / InCell3000 / RCPNL | `.xml` `.rcpnl` `.frm` | ✅ | |
| ScanR / CellVoyager / BD Pathway | `.xml` `.mlf` `.exp` | ✅ | |
| Tecan plate ASCII | `.asc` | ✅ | |
| Yokogawa CV7000/8000 | `.wpi` `.mlf` `.mrf` | ✅ | |
| MetaXpress | `.htd` `.tif` | ✅ | |
| SimplePCI / Trestle / Mikroscan / Ionpath MIBI / MIAS TIFFs | `.tif` | ✅ | |
| TissueFAXS | `.aqproj` `.tfcyto` | ✅ | Optional `tissuefaxs` feature, enabled by default. |
| Cellomics | `.c01` `.dib` | ✅ | |
| CellWorX | `.htd` `.pnl` | ✅ | |

### Whole-slide / pyramidal TIFF

| Format | Extensions | Status | Rust-specific notes |
|--------|-----------|:------:|-------|
| Aperio SVS (+ generic WSI) | `.svs` `.ndpi` `.scn` `.vsi` `.afi` | ✅ | |
| Hamamatsu NDPI | `.ndpi` | ✅ | |
| Nikon NIS / FEI / Olympus SIS / Improvision / Zeiss ApoTome / Fluoview / Molecular Devices TIFFs | `.tif` `.tiff` | ✅ | |
| Leica SCN | `.scn` | ✅ | |
| Ventana/Roche BIF | `.bif` | ✅ | |
| Hamamatsu NDPIS | `.ndpis` | ✅ | |
| Olympus cellSens VSI | `.vsi` | ✅ | |

### Vendor microscopy & cameras

| Format | Extensions | Status | Rust-specific notes |
|--------|-----------|:------:|-------|
| Applied Precision DeltaVision | `.dv` `.r3d` | ✅ | |
| Gatan DM3/DM4 | `.dm3` `.dm4` | ✅ | |
| Bio-Rad PIC | `.pic` | ✅ | |
| IPLab | `.ipl` `.ipm` | ✅ | |
| Bio-Rad GEL | `.1sc` | ✅ | |
| Li-Cor L2D | `.l2d` `.scn` | ✅ | |
| PCO-RAW | `.pcoraw` `.rec` | ✅ | `.rec` companion metadata is optional. |
| Openlab Raw | `.raw` | ✅ | |
| SM-Camera | `.smc` | ✅ | |
| Andor SIF | `.sif` | ✅ | |
| Princeton SPE | `.spe` | ✅ | |
| Gatan DM2 | `.dm2` | ✅ | |
| Lab Imaging LIM | `.lim` | ✅ | |
| Hasselblad Imacon / Image-Pro IPW | `.fff` `.ipw` | ✅ | |
| Hamamatsu DCIMG | `.dcimg` | ✅ | v0 legacy fallback is Rust-only extra. |
| TillVision | `.vws` `.pst` | ✅ | |
| Canon RAW / Minolta MRW / DNG (CFA) | `.cr2` `.crw` `.mrw` `.dng` | ✅ | |
| Photoshop / QPTIFF / NIS TIFF wrappers | `.tif` `.qptiff` `.nif` | ✅ | |
| Hamamatsu VMS/VMU | `.vms` `.vmu` | ✅ | |

### Medical, volumetric & astronomy

| Format | Extensions | Status | Rust-specific notes |
|--------|-----------|:------:|-------|
| MRC / CCP4 | `.mrc` `.mrcs` `.ccp4` `.map` `.rec` | ✅ | |
| NIfTI-1 / Analyze 7.5 | `.nii` `.nii.gz` `.hdr` `.img` | ✅ | |
| ICS / ICS2 | `.ics` | ✅ | |
| Siemens Inveon | `.hdr` (+`.img`) | ✅ | |
| POV-Ray DF3 | `.df3` | ✅ | |
| SBIG astronomy | `.fts` | ✅ | |
| FITS | `.fits` `.fit` `.fts` | ✅ | |
| NRRD | `.nrrd` `.nhdr` | ✅ | |
| DICOM | `.dcm` `.dicom` `.dic` | ✅ | |
| ECAT7 PET | `.v` | ✅ | |
| Varian FDF | `.fdf` | ✅ | |
| Molecular Dynamics GEL | `.gel` | ✅ | |
| Kodak BIP | `.bip` | ✅ | |
| MINC | `.mnc` | ✅ | Classic MINC-1 uses a pure-Rust NetCDF-3 parser. |
| PDS (planetary) | `.pds` | ✅ | |
| Photon Dynamics | `.hdr` `.img` `.pds` | ✅ | |

### Electron / scanning-probe / AFM microscopy

| Format | Extensions | Status | Rust-specific notes |
|--------|-----------|:------:|-------|
| FEI/Philips XL SEM | `.img` | ✅ | |
| INRIMAGE-4 | `.inr` | ✅ | |
| Veeco/Nanoscope AFM | `.afm` | ✅ | |
| Seiko / UBM / VG SAM / WA-Top SPM | `.xqd` `.pr3` `.dti` `.wat` | ✅ | |
| Unisoku STM/AFM | `.hdr` `.dat` | ✅ | |
| JPK AFM | `.jpk` | ✅ | |
| Scanco AIM/ISQ micro-CT | `.aim` `.isq` | ✅ | |
| Zeiss TIFF SEM | `.tif` | ✅ | |
| Hitachi SEM | `.txt` | ✅ | |
| LEO/Zeiss SEM | `.tif` | ✅ | |
| RHK SPM | `.sm2` `.sm3` `.sm4` | ✅ | |
| TopoMetrix AFM | `.tfr` `.zfr` | ✅ | Byte probe is extra Rust support. |
| IMOD mesh | `.mod` | ✅ | |
| JEOL | `.dat` `.img` `.par` | ✅ | |
| Zeiss LMS (LMSFile) | `.lms` | ✅ | |
| Quesant AFM | `.afm` | ✅ | |

### FLIM / lifetime / flow / HDF5

| Format | Extensions | Status | Rust-specific notes |
|--------|-----------|:------:|-------|
| Lambert LI-FLIM | `.fli` | ✅ | |
| Becker & Hickl SDT/SPC | `.sdt` `.spc` | ✅ | |
| Amira/Avizo Mesh | `.am` `.amiramesh` | ✅ | |
| Spider EM | `.spi` `.xmp` | ✅ | |
| Amnis FlowSight CIF | `.cif` | ✅ | |
| CellH5 | `.ch5` | ✅ | |
| Aperio AFI / Bio-Rad SCN | `.afi` `.scn` | ✅ | |
| Imaris TIFF / SlideBook TIFF | `.ims` `.tif` | ✅ | |
| BigDataViewer | `.h5` | ✅ | |
| Olympus OIR / Volocity clipping | `.oir` `.acff` | ✅ | |
| Imspector OBF/MSR | `.obf` `.msr` | ✅ | Bio-Formats-style OBF is handled separately by `ObfReader`. |
| Amnis IM3 | `.im3` | ✅ | |
| SlideBook 7 | `.sld` `.sldy` `.sldyz` | ✅ | |
| iVision IPM | `.ipm` | ✅ | |

Various no-Java-reference camera/SPM readers remain best-effort extensions; when
native layout is unknown they return `UnsupportedFormat` instead of guessed
metadata.

### Added (non-upstream) readers

These readers have **no counterpart in Bio-Formats** — they are *added code*,
written for this crate (or ported from a separate project), **not translations**
of a Bio-Formats reader. They are listed separately because there is no Java
reader to be faithful to; confidence comes from local synthetic tests, optional
real fixtures, or the wrapped upstream library.

| Reader | Extensions | Validation | Note |
|--------|-----------|------------|------|
| MetaImage (ITK) | `.mha` `.mhd` | Synthetic round-trip plus optional real fixture. | ITK/MetaIO volume. |
| OME-Zarr / NGFF | `.zarr` | Synthetic NGFF/plain-Zarr tests. | Ported from the separate `ome/ZarrReader`. |
| OpenSlide | `.mrxs` `.vms` `.bif` | Covered by OpenSlide and optional local feature tests. | Wraps the OpenSlide library. |
| SimFCS | `.b64` `.r64` `.i64` | Synthetic raw-frame tests. | Globals SimFCS raw FLIM frames. |
| Norpix StreamPix | `.seq` | Synthetic header, timestamp, and pixel tests. | Bio-Formats' `SEQReader` is unrelated Image-Pro Sequence. |
| Bruker MicroCT | `.ctf` | Needs a real fixture before claiming support. | Bio-Formats' translated MicroCT reader is the separate `.vff` reader. |
| PCO B16 | `.b16` | Synthetic header and pixel tests. | Raw uint16 PCO camera files. |
| PicoQuant PTU/PHU | `.ptu` `.phu` `.pqres` | Synthetic metadata/reconstruction tests plus optional real fixture. | Set `BIOFORMATS_RS_PICOQUANT_FIXTURE` for local PTU/PQRes coverage. |

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
use bioformats::{FormatWriter, TiffWriter, WriteCompression};

let mut writer = TiffWriter::new().with_compression(WriteCompression::Deflate);
writer.set_metadata(&meta)?;
writer.set_id(Path::new("compressed.tif"))?;
writer.save_bytes(0, &plane_data)?;
writer.close()?;
```

### `FormatReader` trait

Implement this to add a new format:

Only non-default methods are shown below; resolution, LUT, metadata-options,
thumbnail-series, and `ome_metadata()` hooks have default implementations.

```rust
use bioformats::{FormatReader, ImageMetadata, Result};
use std::path::Path;

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

## Remaining gaps

- **ND2**: full coverage of modern structured `ImageDataSeq` variants and richer per-plane metadata (raw/zlib/JPEG2000 frames already decode; see status table)
- **Write support** for LIF, ND2, CZI
- **OME metadata**: `reader.ome_metadata()` returns baseline OME metadata for all readers and enriches it with structured physical sizes, channel names, channel colors, wavelengths, and plane positions where supported; richer parsing (instrument, experimenter) is partial
- **Pyramid writing UX**: direct `bioformats::tiff::PyramidOmeTiffWriter` exists; automatic `ImageWriter` dispatch intentionally writes ordinary OME-TIFF for plain `.ome.*` suffixes unless callers use the pyramid writer directly.

### Missing external codecs

A few pixel formats are **not stubs by choice** — they require a codec for
which there is no pure-Rust decoder in this tree, and which upstream Java
Bio-Formats itself decodes by delegating to native/platform libraries
(QuickTime/Java ImageIO). Metadata for these files is still read; only the pixel
decode is unavailable. Wiring a Rust codec crate for each would close the gap:

| Codec | Used by | Status |
| --- | --- | --- |
| **H.264 / AVC** (`avc1`, `h264`, …) | QuickTime `.mov` | no Rust decoder; metadata-only |
| **H.265 / HEVC** (`hvc1`, `hev1`) | QuickTime `.mov` | no Rust decoder; metadata-only |
| **Apple ProRes** (`apch`, `apcn`, …) | QuickTime `.mov` | no Rust decoder; metadata-only |
| **Motion JPEG 2000** (`mjp2`, `mj2k`) | QuickTime `.mov` | no Rust decoder; metadata-only |
| **DV** (`dvcp`, `dv25`, …) | QuickTime `.mov` | no Rust decoder; metadata-only |
Codecs that **are** implemented without native/platform video decoders include:
LZW, PackBits, Deflate/zlib, Zstd, LZ4, JPEG (baseline), JPEG 2000 (JP2/J2K),
JPEG-XR (default-on `jpegxr` feature), Cinepak, Apple RLE/Animation, PNG, and
the standard TIFF compressions.

On the **write** side, **JPEG 2000 writing** (`.jp2`/`.j2k`) is now supported via
the pure-Rust `openjp2` encoder, behind the default-on `jpeg2000-write` feature
(disable with `--no-default-features`). The `Jpeg2000Writer` emits lossless JP2
(matching Java `JPEG2000Writer`'s `irreversible = 0` semantics) for single 2D
planes with 1 (grayscale) or 3 (RGB) integer components. The QuickTime writer
(`.mov`) still writes the uncompressed/RAW codec only; the lossy QuickTime codecs
(Cinepak/Animation/Motion-JPEG/etc.) need encoders.

### Performance

For broad translation audits, use the subset comparison harness. It runs Java
Bio-Formats and bioformats-rs against the same centered crop from the first N
planes of each series, records average open+subset-read time, and captures peak
RSS with `/usr/bin/time`:

```bash
bench/compare_subset.sh --warmup 2 --measure 5 --planes 1 --region 256x256 \
  --find-testdata
```

Outputs, including the Markdown result table, are written under `bench/target/`:

- `bench/target/subset-comparison.csv`
- `bench/target/subset-comparison.md`

You can pass explicit files or a newline-delimited manifest instead of
`--find-testdata`:

```bash
bench/compare_subset.sh --manifest fixtures-to-benchmark.txt
bench/compare_subset.sh test/tubhiswt_C0.ome.tif testdata/nd2/BF007.nd2
```

The reported timing excludes process startup inside each harness. RSS is measured
around the whole harness process, so Java RSS includes JVM overhead.

#### Open Microscopy Corpus Screening

Latest sampled real-data speed/RSS screening command:

```bash
BIOFORMATS_RS_OME_IMAGES_WARMUP=0 BIOFORMATS_RS_OME_IMAGES_MEASURE=1 BIOFORMATS_RS_OME_IMAGES_TIMEOUT=30 scripts/run_ome_images_conformance.sh --bench-only
```

The full sweep output is `bench/target/ome-images-subset.csv`. The ICS rows
below use the post-fix focused rerun in `bench/target/ics-after-region.csv`.
TIFF-region-fast-path follow-up rows use focused reruns under
`bench/target/*-region-fastpath.{csv,md}`.
The HDF5-backed BDV, CellH5, and Imaris-IMS rows use the focused benchmark
output in `bench/target/hdf5-readers-0310.md`; the HDF5 dependency is crates.io
`hdf5-pure-rust` 0.3.10 with its `lz4` feature enabled.
`Worst speedup J/R` and `Worst RSS J/R` are Java divided by Rust, so values
below `1.0x` mean Rust was slower or used more RSS for that comparable row.

| Device / folder | Files | Comparable | Java ms max | Rust ms max | Worst speedup J/R | Java RSS max KiB | Rust RSS max KiB | Worst RSS J/R | Notes |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---|
| AmiraMesh | 2 | 2 | 511.4 | 10.5 | 33.66x | 120516 | 16640 | 5.29x | - |
| BDV | 2 | 2 | 1304.3 | 202.6 | 5.76x | 218228 | 14720 | 9.61x | - |
| CV7000 | 2 | 1 | 683.5 | 109.1 | 6.26x | 180664 | 10560 | 17.11x | One XML sidecar rejected by both. |
| CellH5 | 2 | 2 | 1540.5 | 214.3 | 7.19x | 196192 | 9600 | 19.19x | - |
| CellSens | 2 | 2 | 2209.9 | 1517.0 | 1.42x | 327452 | 123864 | 1.94x | - |
| DCIMG | 2 | 2 | 473.7 | 5.8 | 71.02x | 116152 | 13520 | 8.57x | - |
| DICOM | 2 | 2 | 726.1 | 12.3 | 44.78x | 146424 | 9920 | 14.36x | - |
| DNG | 1 | 1 | 770.0 | 183.3 | 4.20x | 185608 | 52080 | 3.56x | Focused region rerun using raw.pixls Canon PowerShot A410 DNG; Java routes it through NikonReader. |
| DV | 2 | 2 | 407.8 | 6.9 | 58.38x | 98224 | 12160 | 8.03x | - |
| Flex | 2 | 2 | 172124.4 | 90828.5 | 1.90x | 230432 | 12160 | 18.95x | Focused TIFF-region fast-path rerun; first file reads 96 planes. |
| Gatan | 2 | 2 | 555.4 | 14.6 | 36.61x | 114572 | 11200 | 10.18x | - |
| HCS | 2 | 2 | 705.4 | 420.0 | 1.68x | 183028 | 25280 | 7.16x | - |
| Hamamatsu-NDPI | 2 | 2 | 7059.3 | 4672.4 | 1.51x | 624488 | 150256 | 4.16x | - |
| Hamamatsu-VMS | 2 | 2 | 10689.9 | 1257.8 | 4.25x | 710084 | 636776 | 1.10x | - |
| ICS | 2 | 2 | 434.8 | 2.5 | 165.73x | 122840 | 8320 | 12.31x | Direct uncompressed row-window reads. |
| Imaris-IMS | 2 | 2 | 583.3 | 104.3 | 5.40x | 145264 | 12480 | 11.64x | Java reports shorter byte counts on the LZ4 fixtures. |
| InCell2000 | 2 | 2 | 1247.6 | 21.2 | 42.55x | 180012 | 9600 | 18.75x | Focused TIFF-region fast-path rerun. |
| InCell3000 | 2 | 2 | 565.0 | 5.7 | 99.97x | 88564 | 10560 | 8.37x | Focused BMP control rerun; not a TIFF-fast-path fixture. |
| KLB | 2 | 2 | 440.7 | 311.8 | 1.24x | 123712 | 19364 | 5.34x | - |
| LEO | 2 | 2 | 2489.3 | 131.7 | 12.35x | 126044 | 11984 | 10.11x | Focused TIFF-region fast-path rerun. |
| Leica-LIF | 2 | 2 | 1421.1 | 128.1 | 11.09x | 259828 | 49024 | 5.30x | - |
| Leica-SCN | 2 | 2 | 5015.1 | 115.4 | 14.48x | 237488 | 12356 | 17.25x | - |
| Leica-XLEF | 3 | 3 | 3909.0 | 1321.0 | 2.72x | 569516 | 144960 | 3.93x | - |
| MetaXpress | 1 | 1 | 93378.3 | 27581.2 | 3.39x | 458476 | 16640 | 27.55x | Focused TIFF-region fast-path rerun for idr0005; second HTD did not complete cleanly. |
| Metamorph | 2 | 2 | 8400.3 | 340.3 | 13.09x | 193684 | 13444 | 13.34x | Focused TIFF-region fast-path rerun. |
| Micro-Manager | 1 | 1 | 102265.1 | 2407.2 | 42.48x | 198000 | 12064 | 16.41x | Focused OME-TIFF region fast-path rerun. |
| Molecular-Devices-TIFF | 1 | 1 | 955.2 | 15.9 | 60.18x | 175808 | 9600 | 18.31x | Focused TIFF-region fast-path rerun using public JDCE TIFF payload. |
| ND2 | 2 | 2 | 9279.1 | 4596.8 | 2.02x | 1045252 | 784492 | 1.17x | - |
| NIfTI | 2 | 1 | 375.2 | 3.1 | 119.61x | 107852 | 12480 | 8.64x | One XML sidecar rejected by both. |
| OME-TIFF | 2 | 2 | 804.2 | 13.0 | 61.93x | 99860 | 11200 | 8.84x | Focused OME companion TIFF-region fast-path rerun. |
| OME-XML | 2 | 2 | 1026.7 | 2.4 | 425.37x | 155684 | 11200 | 13.26x | Sampled files are tiny inline BinData planes. |
| Olympus-FluoView | 1 | 1 | 4940.8 | 335.0 | 14.75x | 176368 | 36160 | 4.88x | Focused TIFF-region fast-path rerun. |
| Olympus-OIR | 2 | 2 | 4988.0 | 845.5 | 2.55x | 200940 | 46792 | 3.92x | Focused TIFF-region fast-path rerun; worst speedup is the companion OME-TIFF row. |
| PNG | 2 | 2 | 394.2 | 4.4 | 85.99x | 86436 | 11520 | 7.47x | Complete-plane decodes; APNG is routed separately. |
| PerkinElmer-Columbus | 1 | 1 | 40318.5 | 17335.4 | 2.33x | 1509724 | 34080 | 44.30x | XML-index benchmark with 3696 planes. |
| PerkinElmer-Operetta | 2 | 2 | 616.1 | 28.9 | 16.80x | 94724 | 17172 | 5.49x | - |
| SDT | 1 | 1 | 8751.9 | 465.3 | 18.81x | 670068 | 40336 | 16.61x | - |
| SPC-FIFO | 1 | 1 | 940.0 | 132.4 | 7.10x | 144976 | 43200 | 3.36x | - |
| SVS | 2 | 2 | 2088.2 | 276.0 | 5.45x | 211200 | 13196 | 16.01x | - |
| ScanR | 1 | 1 | 1229.9 | 503.9 | 2.44x | 188808 | 14720 | 12.83x | Focused smoke run in `bench/target/scanr-benchmark-smoke.md`; Java and Rust both read 357 planes. |
| TIFF | 2 | 2 | 5920.4 | 29.3 | 201.81x | 193596 | 9280 | 19.93x | Focused TIFF-region fast-path rerun. |
| Trestle | 2 | 2 | 1261.7 | 71.0 | 16.64x | 216484 | 11520 | 18.10x | - |
| Vectra-QPTIFF | 2 | 2 | 1047.8 | 63.0 | 15.73x | 177832 | 12000 | 14.78x | - |
| Zeiss-CZI | 2 | 2 | 793.8 | 14.2 | 54.51x | 134988 | 13784 | 9.77x | - |
| Zeiss-LSM | 1 | 1 | 519.2 | 34.4 | 15.11x | 116740 | 9920 | 11.77x | Focused TIFF-region fast-path rerun on `testdata/lsm/colocsample1b.lsm`. |
| gateway_tests | 2 | 2 | 697.8 | 17.2 | 40.48x | 196864 | 11840 | 8.00x | - |



## Java parity & known divergences

Readers are checked against the reference Java Bio-Formats
(`bioformats_package.jar`) by `tests/java_parity_test.rs` with
`parity/BfParityOracle.java`. The reader harness is opt-in:
`BIOFORMATS_RS_JAVA_PARITY=1 cargo test --test java_parity_test -- --nocapture`
(skipped otherwise, so a plain `cargo test` needs no JVM).

Default reader parity runs gate **core metadata** and **OME metadata**. Core
coverage includes dimensions, pixel type, bit depth, image count, dimension
order, RGB/interleaved/indexed/little-endian flags, `rgbChannelCount`, and
`resolutionCount`. OME coverage includes image name, physical sizes X/Y/Z, time
increment, channel count, channel name, `samplesPerPixel`, and
emission/excitation wavelengths. The oracle also compares OME object-graph
summary counts: instruments, objectives, detectors, light sources, filters,
dichroics, plane metadata entries, ROIs/shapes, plates, wells, well samples, and
structured annotation counts for annotation types represented by Rust.

Pixel parity is evidence by default and becomes a gate with
`BIOFORMATS_RS_JAVA_PARITY_STRICT=1`. Pixel sampling compares top-left 256x256
crops for up to 64 planes per series (per-file caps may reduce this for very
slow fixtures), whole planes only when Java reports the plane is small enough
(<= 4 MiB), and one centered off-origin crop from plane 0. Very large Rust plane
reads are skipped above the harness memory budget. Pixel results are classified
**bitwise** / **tolerant** (<=5 levels, JPEG IDCT rounding) / **Java-bug** /
**mismatch**.

Release/CI strict pixel coverage can run the explicit ignored gate:
`BIOFORMATS_RS_JAVA_PARITY=1 BIOFORMATS_RS_JAVA_PARITY_STRICT=1 cargo test --test java_parity_test java_parity_strict_pixel_gate -- --ignored --nocapture`.
Optional fixture hooks are `BIOFORMATS_RS_QT_EXTERNAL_CODEC_FIXTURE` for a local
QuickTime H.264/HEVC/ProRes/DV-style sample and `BIOFORMATS_RS_PICOQUANT_FIXTURE`
for local PTU/PQRes coverage; both skip when unset.

Writer parity is separate: `tests/java_writer_parity_test.rs` synthesizes small
Rust outputs, asks Java to read them, and fails on unannotated metadata or pixel
divergence. It runs by default when the Java jar/toolchain are present, and can
be disabled with `BIOFORMATS_RS_JAVA_PARITY=0`.

Core and OME metadata are treated as hard parity gates. The remaining documented
known divergences are pixel-only:

- **JPEG-compressed pixels** (whole-slide SVS/SCN/NDPI, VSI overview, baseline
  JPEG). These match Java to within ±1–3 levels per sample but not bitwise: the
  pure-Rust JPEG decoder uses a different integer IDCT than libjpeg-turbo. Counted
  as *tolerant*, not failures.

- **BigDataViewer HDF5 (`bdv`), setup-8 levels.** Our decode is correct; Java
  diverges because the libhdf5 (1.10.5) bundled in Bio-Formats has an off-by-one
  in `H5Zscaleoffset.c` that mis-reads full-precision scale-offset chunks (fixed
  upstream only in HDF5 2.0.0). Classified **⚠ Java-bug**, not a failure. Full
  write-up in `bioformats_bug.txt`.


## How to cite

If you use this package, cite the original Bio-Formats paper:

```text
Melissa Linkert, Curtis T. Rueden, Chris Allan, Jean-Marie Burel, Will Moore,
Andrew Patterson, Brian Loranger, Josh Moore, Carlos Neves, Donald MacDonald,
Aleksandra Tarkowska, Caitlin Sticco, Emma Hill, Mike Rossner, Kevin W.
Eliceiri, and Jason R. Swedlow (2010) Metadata matters: access to image data
in the real world. The Journal of Cell Biology 189(5), 777-782.
doi: 10.1083/jcb.201004104
```

## License

GPL v2 or later (`GPL-2.0-or-later`), matching the upstream Bio-Formats
`formats-gpl` grant ("either version 2 of the License, or (at your option) any
later version"). Bio-Formats' `formats-bsd`/`formats-api` components are BSD-2-Clause;
because this crate also translates GPL readers, the combined work is GPL-2.0-or-later.
