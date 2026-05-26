# bioformats-rs

A pure-Rust reimplementation of [Bio-Formats](https://www.openmicroscopy.org/bio-formats/) 
— a library for reading (and writing) scientific image formats used in microscopy, medical imaging, and astronomy.

**This package is still under development**

* 2026-05-26: 60-70% there. see list of libraries below. translation of mdbtools underway to support key file formats
* 2026-05-24: started proper audit; plenty left to do on this crate

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
* **Do not trust the benchmarks on this page**. They are used to help evaluate the translation. If you want improved performance, you generally have to use this code as a library, and use the additional tricks it offers. We generally accept performance losses in order to reduce our dependency issues
* **Check the original Github pages for information about the package**. This README is kept sparse on purpose. It is not meant to be the primary source of information
* **If you are the author of the original code and wish to move to Rust, you can obtain ownership of this repository and crate**. Until then, our commitment is to offer an as-faithful-as-possible translation of a snapshot of your code. If we find serious bugs, we will report them to you. Otherwise we will just replicate them, to ensure comparability across studies that claim to use package XYZ v.666. Think of this like a fancy Ubuntu .deb-package of your software - that is how we treat it

This blurb might be out of date. Go to [this page](https://github.com/henriksson-lab/rustification) for the latest information and further information about how we approach translation


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

## Translation status (all readers)

A per-reader audit of the Java→Rust translation (2026-05-26, updated after a
parity pass). Status reflects how complete the translation is *in theory*
(faithful to the Java reader for the common cases), not how thoroughly it has
been tested against real-world files.

- ✅ **Complete** — faithful core read path; metadata + pixels work for the
  format's common cases with no major known gap vs the Java reader.
- 🟡 **Partial** — opens and reads the common case, but a specific feature is
  missing (noted). Often a vendor TIFF wrapper that reads pixels via the TIFF
  engine but skips format-specific metadata/companion-file assembly, or a format
  with no Java counterpart to be faithful to.
- ⛔ **Stub** — detection only; `set_id` returns `UnsupportedFormat` (the format
  is proprietary/undocumented or needs a decoder/container parser not yet ported).

Roughly **108 complete, 42 partial, 36 stub** out of ~185 registered readers
(up from 66/83/36 at the first audit).

### Standard image formats

| Format | Extensions | Status | Notes |
|--------|-----------|:------:|-------|
| TIFF / BigTIFF / OME-TIFF | `.tif` `.tiff` `.btf` `.tf8` | ✅ | Strips/tiles, planar, palette, YCbCr, LZW/Deflate/PackBits/JPEG/Zstd/JP2/fax, SubIFD pyramids |
| PNG | `.png` | ✅ | 8/16-bit gray/RGB(A); animated APNG rejected |
| JPEG | `.jpg` `.jpeg` | ✅ | Decodes to 8-bit RGB |
| BMP | `.bmp` | ✅ | RAW, RLE4/8, BITFIELDS, palette |
| PCX | `.pcx` | ✅ | RLE, planar channels, v5 palette |
| Imagic-5 | `.hed` `.img` | ✅ | REAL/INTG/PACK types |
| TGA | `.tga` `.tpic` | ✅ | via `image` crate |
| GIF | `.gif` | ✅ | All frames read as an image stack |
| WebP / OpenEXR / HDR / DDS / Farbfeld / PNM | `.webp` `.exr` `.hdr` `.dds` `.ff` `.pnm` `.pgm` `.ppm` `.pbm` `.pfm` | ✅ | via `image` crate |
| JPEG 2000 / JPX | `.jp2` `.j2k` `.j2c` `.jpc` `.jpx` | ✅ | via `jpeg2k`; single plane |
| EPS / PostScript | `.eps` `.epsi` `.ps` | ✅ | Inline raster + DOS-EPS TIFF preview (matches Java; no vector interpreter) |
| Photoshop PSD/PSB | `.psd` `.psb` | ✅ | Composite image (matches Java; no per-layer extraction) |
| Khoros VIFF | `.xv` `.viff` | ✅ | KhorosReader parity (byte-order, pixel types, LUT) |
| Apple PICT | `.pict` `.pct` | ✅ | Bitmap/pixmap/packbits + JPEG-in-PICT |
| ZIP container | `.zip` | ✅ | Delegates primary entry to any auto-detected reader |
| MNG / APNG | `.mng` `.apng` | 🟡 | First frame only |
| Text / CSV image | `.txt` `.csv` | 🟡 | Parsed as Float32; no distinct Java counterpart |
| AVI (video) | `.avi` | ✅ | Uncompressed/16-bit/Y8 + MSRLE, MS Video 1, Cinepak, JPEG/MJPEG |
| Fake (test format) | `.fake` | ✅ | Synthetic gradient generator |

### Microscopy acquisition containers

| Format | Extensions | Status | Notes |
|--------|-----------|:------:|-------|
| Zeiss ZVI | `.zvi` | ✅ | OLE2/CFB; each mosaic tile a separate series |
| Zeiss XRM/TXRM | `.xrm` `.txrm` `.txm` | ✅ | CFB X-ray tomography (Java reads uncompressed only too) |
| OME-XML | `.ome` | ✅ | Inline BinData + external `<TiffData>`/`<UUID>` companion files |
| Zeiss LSM | `.lsm` | ✅ | TIFF + CZ_LSMInfo |
| Olympus FV1000 OIF/OIB | `.oif` `.oib` | ✅ | INI + companion TIFF; `.oib` via OLE2/CFB |
| Leica LEI | `.lei` | ✅ | DIMDESCR dimensions/physical sizes + companion TIFF |
| Leica TCS | `.xml` | ✅ | Full C/Z/T from Leica handler |
| MicroManager | `metadata.txt` `.json` | ✅ | Per-plane file map, multi-position, channel/calibration metadata |
| Visitech | `.xys` `.html` | ✅ | `.html`/`.xys` parse + multi-position series |
| Zeiss CZI | `.czi` | ✅ | Scene/acquisition/angle series, mosaic stitching + fusion rebalancing, per-pixel-type split, rotation→moduloZ, PALM split, pyramids; JPEG-XR needs `jpegxr` feature |
| Nikon ND2 | `.nd2` | 🟡 | Raw/zlib/JPEG2000 frames; modern chunked ImageDataSeq blocked on fixtures |
| Prairie View | `.xml` `.cfg` `.env` `.tif` | ✅ | Channels/metadata + stage-position multi-series |
| MetaMorph STK | `.stk` `.nd` | ✅ | Per-plane UIC metadata + multi-STK `.nd` file-group series |
| Leica XLEF | `.xlef` | 🟡 | XLEF/XLIF container resolved; LOF-backed references undecoded |
| Imaris IMS | `.ims` | 🟡 | HDF5; whole-volume read cached (no hyperslab slicing) |
| Leica LIF | `.lif` | ⛔ | Detection only; container parser not ported |

### High-content screening (HCS)

| Format | Extensions | Status | Notes |
|--------|-----------|:------:|-------|
| PerkinElmer FLEX | `.flex` `.mea` `.res` | ✅ | Factor scaling + well/field grouping + OME plate |
| InCell (GE) | `.xdce` `.xml` | ✅ | Well/field Z/C/T series + OME plate/well/wellsample |
| PerkinElmer UltraVIEW | `.htm` `.tim` `.csv` `.zpo` | ✅ | Full dataset parse, TIFF + numbered raw planes |
| MIAS (Maia Scientific) | `.tif` | ✅ | Per-well series + tiled-mosaic stitching |
| Operetta / Columbus / InCell3000 / RCPNL | `.xml` `.rcpnl` `.frm` | ✅ | Index → per-well/field series + plane→file mapping |
| ScanR / CellVoyager / BD Pathway | `.xml` `.mlf` `.exp` | ✅ | Sparse-well compaction, CellVoyager tile stitching, BD montage field split |
| Tecan plate ASCII | `.asc` | ✅ | Tab-separated plate → Float32 |
| Yokogawa CV7000/8000 | `.wpi` `.mlf` `.mrf` | ✅ | `.wpi`/`.mlf`/`.mrf` XML index → well/field series + OME plate |
| MetaXpress / SimplePCI / MIAS / Trestle / TissueFAXS / Mikroscan / Ionpath MIBI TIFFs | `.tif` | 🟡 | Extension-only TIFF delegate; no format-specific assembly |
| Cellomics | `.c01` `.dib` | 🟡 | zlib + DIB decoded; `.mdb` metadata needs MS-Access lib |
| CellWorX | `.htd` `.pnl` | ⛔ | Parses HTD dims but `set_id` unsupported |

### Whole-slide / pyramidal TIFF

| Format | Extensions | Status | Notes |
|--------|-----------|:------:|-------|
| Aperio SVS (+ generic WSI) | `.svs` `.ndpi` `.scn` `.vsi` `.afi` | ✅ | SVS pyramid regroup + Aperio metadata |
| Hamamatsu NDPI | `.ndpi` | ✅ | TIFF-based; vendor tags |
| Nikon NIS / FEI / Olympus SIS / Improvision / Zeiss ApoTome / Fluoview / Molecular Devices TIFFs | `.tif` `.tiff` | ✅ | TIFF-based metadata scrape; pixels via TIFF engine |
| Leica SCN | `.scn` | ✅ | XML series split + per-resolution pyramid mapping |
| Ventana/Roche BIF | `.bif` | ✅ | BIF tile reassembly (overlap-averaged stitching) |
| Hamamatsu NDPIS | `.ndpis` | ✅ | `.ndpis` multi-file channel index |
| Olympus cellSens VSI | `.vsi` | ✅ | `.ets` pyramid + RAW/JPEG/J2K/PNG/BMP tiles, tag-tree dims/crop, orphan-ETS matching, dim collision-shift, prefix-gated value metadata |
| OpenSlide (MRXS/VMS/BIF) | `.mrxs` `.vms` `.bif` | 🟡 | Feature-gated; multi-resolution |

### Vendor microscopy & cameras

| Format | Extensions | Status | Notes |
|--------|-----------|:------:|-------|
| Applied Precision DeltaVision | `.dv` `.r3d` | ✅ | Extended headers, panels, stage positions |
| Gatan DM3/DM4 | `.dm3` `.dm4` | ✅ | Tag-tree parse, endianness |
| Bio-Rad PIC | `.pic` | ✅ | AXIS notes, multi-file grouping, RGB swap |
| IPLab | `.ipl` `.ipm` | ✅ | Header + tag block |
| Bio-Rad GEL | `.1sc` | ✅ | Chunk-walk, dynamic pixel offset |
| Li-Cor L2D | `.l2d` `.scn` | ✅ | Manifest + companion TIFF |
| PCO B16 | `.b16` | ✅ | Raw uint16 |
| Openlab Raw | `.raw` | ✅ | LBLB header + raw plane |
| Photon Dynamics | `.hdr` `.img` `.pds` | ✅ | Header + companion IMG |
| SM-Camera | `.smc` | ✅ | 548-byte header |
| Andor SIF | `.sif` | ✅ | SIFReader parity (Java has no v3 XML-footer path) |
| Princeton SPE | `.spe` | ✅ | SPE 2.x + 3.x detection (Java keeps binary dims; matches Java) |
| Gatan DM2 | `.dm2` | ✅ | GatanDM2Reader parity (header + tag metadata) |
| Lab Imaging LIM | `.lim` | ✅ | Matches Java (Java also rejects compressed LIM) |
| Hasselblad Imacon / Image-Pro IPW | `.fff` `.ipw` | ✅ | Imacon XML tag; IPW OLE2 multi-TIFF |
| Hamamatsu DCIMG | `.dcimg` | 🟡 | v0/v1 + four-corner correction (no Java reference) |
| Norpix StreamPix | `.seq` | 🟡 | JPEG frames + timestamps (no Java reference) |
| TillVision | `.vws` | 🟡 | PST+INF sidecar; embedded VWS unsupported |
| Canon RAW / Minolta MRW / DNG (CFA) | `.cr2` `.crw` `.mrw` `.dng` | ✅ | CFA Bayer interpolation + bit unpacking + DNG EXIF/maker-note white-balance |
| Photoshop / QPTIFF / NIS TIFF wrappers | `.tif` `.qptiff` `.nif` | 🟡 | Plain TIFF delegate; vendor metadata not parsed |
| Hamamatsu VMS/VMU | `.vms` `.vmu` | ⛔ | JPEG tile decoding not ported |

### Medical, volumetric & astronomy

| Format | Extensions | Status | Notes |
|--------|-----------|:------:|-------|
| MRC / CCP4 | `.mrc` `.mrcs` `.ccp4` `.map` `.rec` | ✅ | Endian detect, EMAN2/IMOD fixes, Y-flip |
| MetaImage (ITK/VTK) | `.mha` `.mhd` | ✅ | Inline/detached, zlib, endian swap |
| NIfTI-1 / Analyze 7.5 | `.nii` `.nii.gz` `.hdr` `.img` | ✅ | Single/paired/gz, color datatypes |
| ICS / ICS2 | `.ics` | ✅ | gzip, endianness rules, dim ordering |
| Siemens Inveon | `.hdr` (+`.img`) | ✅ | All data-type codes + endianness |
| POV-Ray DF3 | `.pov` `.df3` | ✅ | Raw voxel grid |
| SBIG astronomy | `.fts` | ✅ | FITS-based |
| FITS | `.fits` `.fit` `.fts` | ✅ | Primary HDU, big-endian, no BZERO/BSCALE (matches Java) |
| NRRD | `.nrrd` `.nhdr` | ✅ | raw/gzip/ascii (Java has no bzip2 either) |
| DICOM | `.dcm` `.dicom` `.dic` | ✅ | RLE/JPEG/JP2 encapsulation + multi-series companions (Deflate matches Java) |
| ECAT7 PET | `.v` | ✅ | data_type 6 (matches Java, which supports only that) |
| Varian FDF | `.fdf` | ✅ | matrix[] dims, XYTZC, bigendian honored |
| Molecular Dynamics GEL | `.gel` | ✅ | TIFF-based; MD_FILETAG, square-root/linear scaling |
| Kodak BIP | `.bip` | ✅ | KodakReader parity (GBiH/BSfD markers, float32 BE) |
| MINC | `.mnc` | ✅ | MINC2/HDF5 + classic MINC-1 (pure-Rust NetCDF-3 parser) |
| PDS (planetary) | `.pds` | 🟡 | Single-band raw; PE two-file `.hdr`/`.IMG` dialect not split |

### Electron / scanning-probe / AFM microscopy

| Format | Extensions | Status | Notes |
|--------|-----------|:------:|-------|
| FEI/Philips XL SEM | `.img` | ✅ | Interlaced decode + metadata |
| INRIMAGE-4 | `.inr` | ✅ | Header + planar read |
| Veeco/Nanoscope AFM | `.afm` | ✅ | Text header + raw plane |
| Seiko / UBM / VG SAM / WA-Top SPM | `.xqd` `.pr3` `.dti` `.wat` | ✅ | Faithful binary headers |
| Unisoku STM/AFM | `.hdr` `.dat` | ✅ | Companion hdr/dat |
| JPK AFM | `.jpk` | ✅ | TIFF-based two-series |
| Scanco AIM/ISQ micro-CT | `.aim` `.isq` | ✅ | ISQ + AIM v020/v030 headers |
| Zeiss TIFF SEM | `.tif` | ✅ | TIFF delegate |
| Hitachi SEM | `.txt` | ✅ | `[SemImageFile]` INI + companion image (HitachiReader parity) |
| LEO/Zeiss SEM | `.tif` | ✅ | TIFF tag 34118 + AP_/DP_/SV_ metadata |
| RHK SPM | `.sm2` `.sm3` `.sm4` | ✅ | Real binary page header (XPM/text), scales, invertX/Y |
| TopoMetrix AFM | `.tfr` `.zfr` | 🟡 | No Java reference; best-effort header parse |
| IMOD mesh / JEOL / Zeiss LMS / Quesant / PicoQuant / Bruker OPUS / ISS Vista | `.mod` `.dat` `.lms` `.afm` `.ptu` `.abs` `.iss` | ⛔ | Stubs — undocumented or decoder not ported |

### FLIM / lifetime / flow / HDF5

| Format | Extensions | Status | Notes |
|--------|-----------|:------:|-------|
| Lambert LI-FLIM | `.fli` | ✅ | INI header, gzip, UINT12 packing |
| Becker & Hickl SDT | `.sdt` `.spc` | ✅ | Multi-block; MCS-TA partial |
| Amira/Avizo Mesh | `.am` `.amiramesh` | ✅ | Binary + ASCII streams |
| Spider EM | `.spi` `.xmp` | ✅ | Float32 header + planar |
| Amnis FlowSight CIF | `.cif` | ✅ | TIFF + greyscale/bitmask codecs |
| CellH5 | `.ch5` | ✅ | HDF5; multi-position/well series (two-pass structure) |
| Aperio AFI / Bio-Rad SCN | `.afi` `.scn` | ✅ | AFI channel XML; SCN MIME-multipart parse |
| Bruker MicroCT / Imaris TIFF / SlideBook TIFF | `.ctf` `.ims` `.tif` | 🟡 | TIFF delegate; some companion metadata skipped |
| SimFCS | `.b64` `.r64` `.i64` | 🟡 | Fixed 256×256 frames (no Java reference) |
| BigDataViewer | `.h5` | 🟡 | HDF5; single series (no Java reference) |
| Lambert ASCII FLIM / Amnis IM3 / iVision / Olympus OIR / SlideBook 7 / Volocity clipping | `.asc` `.im3` `.ipm` `.oir` `.sld` `.acff` | ⛔ | Stubs |

### Not yet implemented (stubs)

Detection works; `set_id` returns a descriptive `UnsupportedFormat`. Mostly
proprietary/undocumented formats or ones needing an unported container parser or
codec.

| Format | Extensions | Reason |
|--------|-----------|--------|
| QuickTime | `.mov` `.qt` | MOV atom container not parsed |
| Volocity library / MVD2 / clipping | `.acff` `.mvd2` | OLE2 / Metakit container |
| SlideBook / SlideBook 7 | `.sld` | Proprietary undocumented |
| Openlab LIFF | `.liff` | Proprietary undocumented |
| Sedat / APL / I2I / JDCE / PCI / HRD-GDF / NAF | `.sedat` `.apl` `.i2i` `.jdce` `.pci` `.gdf` `.naf` | Proprietary undocumented |
| KLB | `.klb` | No pure-Rust KLB decoder |
| Imspector OBF/MSR | `.obf` `.msr` | Header parses; stack payload not decoded |
| Leica LOF | `.lof` | Leica LAS proprietary binary |
| Burleigh | `.img` | `.img` too generic to detect |
| Woolz | `.wlz` | Graph-based format |
| File-pattern dataset | `.pattern` | Needs glob/regex multi-file expansion |
| FEI SER | `.ser` | Header parses; `set_id` unsupported |

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

```rust
use bioformats::{FormatReader, ImageMetadata, Result};

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

## Planned (not yet implemented)

- **ND2**: full coverage of modern chunked `ImageDataSeq` per-plane metadata (raw/zlib/JPEG2000 frames already decode; see status table)
- **CZI**: JPEG-XR compression is available behind the `jpegxr` feature; multi-scene series are not yet split
- **Write support** for LIF, ND2, CZI, PNM
- **OME metadata**: `reader.ome_metadata()` returns baseline OME metadata for all readers and enriches it with structured physical sizes, channel names, and plane positions where supported; richer parsing (instrument, experimenter) is partial
- **Pyramid writing** for tiled multi-resolution TIFF

### Performance

Reading all planes from each fixture, using one long-lived process per
implementation with 5 warmup iterations and 20 measured iterations:

| Fixture | Java Bio-Formats | bioformats-rs | Ratio |
|---------|-----------------:|--------------:|------:|
| CZI, `672x512`, 63 planes, 43,352,064 pixel bytes | 177.342 ms | 51.438 ms | 3.45x |
| OME-TIFF, `test/tubhiswt_C0.ome.tif` | 13.463 ms | 2.432 ms | 5.54x |

Reproduce by building the warmup harnesses and running `bench/BfBench.java` and
`bench/bench_rust.rs` against the same fixture. This measures open + read all
planes + close per iteration after warmup, not cold JVM startup.



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

GPL v2
