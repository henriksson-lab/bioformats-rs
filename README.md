# bioformats-rs

A pure-Rust translation of [Bio-Formats](https://www.openmicroscopy.org/bio-formats/) 
— a library for reading (and writing) scientific image formats used in microscopy, medical imaging, and astronomy.

**This package has limited real data testing, not all features are yet included**

* 2026-05-27: Further progress but incomplete. See status of translation below. However, more test data is needed for audit
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
| TIFF / OME-TIFF | `.tif` `.tiff` | Full IFD parser; strip and tile layout; LZW, Deflate, PackBits, JPEG, Zstd. BigTIFF is read-only. |
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
| Leica LIF | `.lif` | Binary container with UTF-16 XML metadata; bounded simple uncompressed pixels |
| Nikon ND2 | `.nd2` | Chunk-based; raw, zlib, and JPEG2000 frames |
| Zeiss CZI | `.czi` | ZISRAWFILE segments; scene/mosaic/pyramid support; uncompressed, JPEG, LZW, Zstd |
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

Most registered readers are now complete; the remaining partial/stub rows below
call out the specific native payload, metadata, or multi-file behavior that is
still missing.

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
| FilePattern dataset | `.pattern` | 🟡 | Numeric/list/label/range blocks, shell-style bracket character classes, channel labels projected into OME channel names, recursive globs, confined parent-directory traversal after wildcard components, and explicit pattern/root/block metadata |
| APNG | `.apng` | ⛔ | Animated PNG is explicitly rejected |
| MNG | `.mng` | ✅ | Java-style MNG/JNG container parse with embedded PNG/JPEG frames |
| AVI (video) | `.avi` | ✅ | Uncompressed/16-bit/Y8 + MSRLE, MS Video 1, Cinepak, JPEG/MJPEG |
| QuickTime MOV/QT | `.mov` `.qt` | 🟡 | Uncompressed raw/gray/RGB plus JPEG/MJPEG and PNG layouts including gray, gray+alpha, RGB, and RGBA where decoder-supported; RPZA; Cinepak with prior-frame delta replay; 24-bit, 16-bit RGB555, and 32-bit ARGB Animation RLE including supported delta/partial-frame replay; sample/chunk table metadata, sample timing, codec-family diagnostics, simple edit-list timestamps, compatible multi-video-track series, OME plane DeltaT, and OME original-metadata annotations |
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
| Nikon ND2 | `.nd2` | 🟡 | Raw/zlib/JPEG2000 frames; ImageDataSeq chunks ordered by sequence index with XML calibration/channel fallback metadata, per-plane metadata sequence diagnostics including bounded sequence timestamps, payload prefix/timestamp diagnostics, and chunk/encoding diagnostics; remaining structured modern chunked variants need broader fixtures |
| Prairie View | `.xml` `.cfg` `.env` `.tif` | ✅ | Channels/metadata + stage-position multi-series |
| MetaMorph STK | `.stk` `.nd` | ✅ | Per-plane UIC metadata + multi-STK `.nd` file-group series |
| Leica XLEF | `.xlef` | 🟡 | Local XLEF/XLIF traversal; TIFF/LOF/JPEG/PNG/BMP leaves plus LMS leaves via pixel delegate when supported, otherwise bounded metadata-only LMS/OME scalars, descriptions, and original-metadata annotations; project grouping metadata is exposed per series |
| Imaris IMS | `.ims` | 🟡 | HDF5 hyperslab reads; RecordingEntry*Spacing physical-size metadata plus common dataset/image/Imaris/log/time/channel metadata, image description, channel RGBA colors, and OME original-metadata annotations |
| Leica LIF | `.lif` | 🟡 | Native container/header validation, UTF-16 XML metadata series discovery, simple confirmed uncompressed payloads, zlib/deflate compressed memory blocks, directly described interleaved RGB triples and uncompressed or zlib/deflate planar RGB triples including repeated logical RGB groups, declared tile byte strides, layout/compression metadata, and precise compression/layout errors; unknown compression and unverified layouts unsupported |

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
| Cellomics | `.c01` `.dib` | 🟡 | zlib + DIB decoded; sibling `.mdb` channel metadata via `mdbtools-rs` |
| CellWorX | `.htd` `.pnl` | ✅ | HTD index + TIFF delegation with plate/well metadata |

### Whole-slide / pyramidal TIFF

| Format | Extensions | Status | Notes |
|--------|-----------|:------:|-------|
| Aperio SVS (+ generic WSI) | `.svs` `.ndpi` `.scn` `.vsi` `.afi` | ✅ | SVS pyramid regroup + Aperio metadata |
| Hamamatsu NDPI | `.ndpi` | ✅ | TIFF-based; vendor tags |
| Nikon NIS / FEI / Olympus SIS / Improvision / Zeiss ApoTome / Fluoview / Molecular Devices TIFFs | `.tif` `.tiff` | ✅ | TIFF-based metadata scrape; pixels via TIFF engine |
| Leica SCN | `.scn` | ✅ | XML series split + per-resolution pyramid mapping |
| Ventana/Roche BIF | `.bif` | ✅ | BIF tile reassembly (overlap-averaged stitching) |
| Hamamatsu NDPIS | `.ndpis` | ✅ | `.ndpis` multi-file channel index |
| Olympus cellSens VSI | `.vsi` | ✅ | `.ets` pyramid + RAW/JPEG/J2K/PNG/BMP tiles, tag-tree dims/crop, orphan-ETS matching, dim collision-shift, prefix-gated value metadata, ETS-level acquisition metadata, and OME original-metadata annotations |
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
| Hamamatsu DCIMG | `.dcimg` | 🟡 | v0/v1 + four-corner correction and OME original-metadata annotations (no Java reference) |
| Norpix StreamPix | `.seq` | 🟡 | Raw/JPEG frames, timestamps, bounded compressed-frame diagnostics, and OME original-metadata annotations (no Java reference) |
| TillVision | `.vws` `.pst` | 🟡 | PST+INF sidecar plus bounded native VWS OLE/CImage uncompressed, zlib-wrapped deflate, or raw-deflate planes; shifted CImage layout discovery, offset/fragments, explicit fragment offset+size metadata, inferred padded fragments, description metadata, normalized exposure/acquisition metadata, OME physical/channel/plane enrichment, and ISO-like acquisition date-time metadata with two-digit-year pivoting |
| Canon RAW / Minolta MRW / DNG (CFA) | `.cr2` `.crw` `.mrw` `.dng` | ✅ | CFA Bayer interpolation + bit unpacking + DNG EXIF/maker-note white-balance |
| Photoshop / QPTIFF / NIS TIFF wrappers | `.tif` `.qptiff` `.nif` | 🟡 | Plain TIFF delegate; vendor metadata not parsed |
| Hamamatsu VMS/VMU | `.vms` `.vmu` | 🟡 | JPEG tile grids including RGB/L8/CMYK32 conversion, normalized text/`.opt` keys, macro/map series, broader layer/tile-key variants, explicit/inferred pyramid levels, validated header-only JPEG marker/ICC metadata without applying color transforms, and OME original-metadata annotations |

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
| IMOD mesh | `.mod` | ✅ | Java-style model metadata and blank RGB plane |
| JEOL | `.dat` `.img` `.par` | ✅ | Native MG/IM/DAT paths plus `.par` companion resolution |
| Zeiss LMS (LMSFile) | `.lms` | ✅ | Marker/LUT parse with main indexed stack and RGB thumbnail series |
| Quesant AFM | `.afm` | ✅ | Native variable-table parse plus strict raw fallback |
| PicoQuant | `.ptu` `.pqres` | 🟡 | Header/tag metadata plus bounded marker-raster T2/T3 reconstruction for HydraHarp, TimeHarp 260, and MultiHarp synthetic records, metadata labels with safe T2/T3 inference, lifetime calibration metadata, exact `Uint8`/`Uint16`/`Uint32` histogram payloads including `HistoResult_*` variants, and explicit histogram payload ambiguity metadata; PicoHarp reconstruction is fixture/spec-blocked because its bit packing and marker encoding are not confirmed locally, and broader TTTR reconstruction needs fixtures/layout work |

### FLIM / lifetime / flow / HDF5

| Format | Extensions | Status | Notes |
|--------|-----------|:------:|-------|
| Lambert LI-FLIM | `.fli` | ✅ | INI header, gzip, UINT12 packing |
| Becker & Hickl SDT | `.sdt` `.spc` | 🟡 | Multi-block; MCS-TA remains partial |
| Amira/Avizo Mesh | `.am` `.amiramesh` | ✅ | Binary + ASCII streams |
| Spider EM | `.spi` `.xmp` | ✅ | Float32 header + planar |
| Amnis FlowSight CIF | `.cif` | ✅ | TIFF + greyscale/bitmask codecs |
| CellH5 | `.ch5` | ✅ | HDF5; multi-position/well series (two-pass structure) |
| Aperio AFI / Bio-Rad SCN | `.afi` `.scn` | ✅ | AFI channel XML; SCN MIME-multipart parse |
| Bruker MicroCT / Imaris TIFF / SlideBook TIFF | `.ctf` `.ims` `.tif` | 🟡 | TIFF delegate; some companion metadata skipped |
| SimFCS | `.b64` `.r64` `.i64` | 🟡 | Fixed 256×256 frames with captured payload metadata and OME original-metadata annotations (no Java reference) |
| BigDataViewer | `.h5` | 🟡 | HDF5; single series with companion XML/core metadata and OME original-metadata annotations (no Java reference) |
| Olympus OIR / Volocity clipping | `.oir` `.acff` | ✅ | Native payload readers with explicit clipping/plane bounds |
| Amnis IM3 | `.im3` | 🟡 | Bounded native Uint16 XYC dataset decode with multi-dataset series, safe scalar native metadata, interpreted channel/wavelength/instrument/acquisition metadata, modulo-C wavelength annotations, OME original-metadata annotations, and bounded unsupported-record diagnostics including nested non-pixel containers; complex spectral/object records unsupported |
| SlideBook 7 | `.sld` `.sldy` `.sldyz` | 🟡 | Bounded native `.sldy` directory and `.sldyz` ZIP layouts, including nested archive roots, with uncompressed NPY and compressed NPYZ byte-stream planes, multi-digit channels, scalar channel/time/position YAML metadata, typed top-level scalar keys, safe flattened nested YAML scalars, shallow scalar flow maps/lists, and OME original-metadata annotations; rich YAML object graph semantics unsupported |
| iVision IPM | `.ipm` | 🟡 | Native header/plane decode for common data types including 16-bit RGB storage, plus bounded scalar native metadata and OME-style XML tail/sidecar metadata; square-root and packed 16-bit color metadata initialize with explicit read-time unsupported decode because their transfer/mask layouts are unresolved |

### True stubs and metadata-only leaves

Detection works; `set_id` returns a descriptive `UnsupportedFormat` (or only a
synthetic subset is read). Partial readers with bounded native pixel support are
listed in the status tables above instead of here.

| Format | Extensions | Reason |
|--------|-----------|--------|
| Volocity | `.mvd2` plus `.aisf` `.aiix` `.dat` `.atsf` companions | Bounded Metakit stream probe validates native headers/TOC/table shape and reports footer/TOC/structure/table diagnostics; full Metakit metadata and pixel decoding remain unsupported except explicit `BFVOLOCITYMVD2` blind raw fixtures with original-metadata provenance |
| Imspector synthetic OBF/MSR subset | `.obf` `.msr` | Native Imspector stack decoding is unsupported except explicit `BFIMSPECTOR_RAW_STACK_V1` data; uncompressed and zlib-compressed synthetic stack payloads are supported. Bio-Formats-style OBF is handled separately by `ObfReader`. |
| Leica XLEF LMS leaves | `.xlef` / `.xlif` projects containing `.lms` leaves | LMS metadata leaves expose bounded metadata/OME scalars and original-metadata annotations when no pixel delegate supports them; pixel reads return `UnsupportedFormat` |

Various no-Java-reference camera/SPM readers remain best-effort extensions; when
native layout is unknown they return `UnsupportedFormat` instead of guessed
metadata.

> **Note.** Several formats previously listed here are now implemented as faithful
> translations of their Java readers: OBF, Olympus OIR, CellWorX/MetaXpress, I2I,
> JDCE, SimplePCI, Volocity clipping, KLB, Olympus APL, HRD-GDF, Hamamatsu NAF,
> Burleigh, Leica LOF with bounded channel metadata projection, MNG, 3i SlideBook, Openlab LIFF with bounded OME original-metadata annotations, IMOD, and Bruker MRI
> (ParaVision). Conversely, four fabricated readers for formats Bio-Formats has no
> reader for (Bruker OPUS, ISS Vista FLIM) or that duplicated real readers
> (Lambert FLIM → LI-FLIM, Volocity Library → Volocity; plus a `Sedat`/`Woolz`
> invention) were **removed** — this project is a translation of Bio-Formats, not
> a superset.

### Non-upstream extensions (deliberate extras)

A few readers have **no counterpart in Bio-Formats** but read a real format and
are kept as documented extensions rather than removed:

| Reader | Extensions | Note |
|--------|-----------|------|
| MetaImage (ITK) | `.mha` `.mhd` | ITK/MetaIO volume; not a Bio-Formats format |
| OME-Zarr / NGFF | `.zarr` | Translated from the separate `ome/ZarrReader`, not core Bio-Formats |
| OpenSlide | `.mrxs` `.vms` … | Optional `openslide` feature; wraps the OpenSlide library |
| SimFCS | `.r64` `.ref` | Globals SimFCS raw 256×256 FLIM frames |
| Photon Dynamics | `.img` | Header + raw pixel sidecar |

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

- **ND2**: full coverage of modern structured `ImageDataSeq` variants and richer per-plane metadata (raw/zlib/JPEG2000 frames already decode; see status table)
- **CZI**: broader vendor metadata enrichment beyond the implemented scene/acquisition/angle series, pyramid handling, and feature-gated JPEG-XR path
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



## Java parity & known divergences

Readers and writers are checked against the reference Java Bio-Formats
(`bioformats_package.jar`) by a parity harness (`tests/java_parity_test.rs` for
reads, `tests/java_writer_parity_test.rs` for writes; oracle in
`parity/BfParityOracle.java`). For each file it compares **core metadata**, **OME
metadata** (image name, physical sizes, channels/wavelengths) and **pixels**
(CRC of the top-left crop of every plane, the whole plane for small images, and a
centered off-origin region). Pixel results are classified **bitwise** /
**tolerant** (≤5 levels, JPEG IDCT rounding) / **⚠ Java-bug** / **✗**. Run with
`BIOFORMATS_RS_JAVA_PARITY=1` (skipped otherwise, so a plain `cargo test` needs
no JVM).

Core metadata reaches full parity; the remaining divergences are documented and
intentional:

- **OME image name — IMAGIC (`.hed`/`.img`).** The OME *image name* convention is
  the file basename, which we populate for every reader to match Java. IMAGIC is
  the one exception: Java's `IMAGICReader` takes the name from a header field
  rather than the filename, and in real files that field often contains numeric
  header junk (e.g. `"!--0.507 41.5 10.9 …"`). We deliberately leave the name
  unset (`None`) rather than replicate the junk — matching Java here would be
  *less* useful, not more. This is the only OME-metadata divergence in the suite.

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

GPL v2
