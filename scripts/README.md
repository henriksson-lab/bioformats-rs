# Sample test data download scripts

Scripts to fetch **real, publicly-downloadable** sample files for the scientific
image formats this pure-Rust Bio-Formats port reads, so you can run real-file
integration tests.

All URLs below were verified to resolve (directory listing / HTTP 200) at the
time of writing. Sizes come from the source directory listings.

## Scripts

| Script | Purpose |
|---|---|
| `download_ndpi.sh` | **Hamamatsu NDPI focus**, including the >4 GB 64-bit-offset test. |
| `download_test_data.sh` | One section per format, with a `--format <name>` selector. |
| `download_biostudies_data.sh` | Formats **not** covered above, sourced from the EMBL-EBI [BioImage Archive](https://www.ebi.ac.uk/bioimage-archive/) (+ EMPIAR): `oib oir zvi avi psd dm3 imagic vsi oif`. `--list` also documents which supported formats are **absent** from the archive. |

### Usage

```bash
# NDPI (small + medium). Add the >4 GB slides with DOWNLOAD_LARGE=1.
scripts/download_ndpi.sh                       # -> ./testdata
scripts/download_ndpi.sh /data/samples         # custom dest
DOWNLOAD_LARGE=1 scripts/download_ndpi.sh       # also fetch >4 GB Hamamatsu-1.ndpi

# Broader: list selectors, fetch one or several formats
scripts/download_test_data.sh --list
scripts/download_test_data.sh --format czi --format lif
DEST=/data/samples scripts/download_test_data.sh --format dicom
scripts/download_test_data.sh                  # download everything (large!)

# BioImage Archive formats not in download_test_data.sh
scripts/download_biostudies_data.sh --list
scripts/download_biostudies_data.sh --format oib --format zvi
scripts/download_biostudies_data.sh --format psd    # smallest (~1 MB)
```

All three scripts share the `./testdata/<fmt>/` layout that
`tests/real_data_test.rs` expects, so downloaded files are picked up by the
real-data integration tests automatically.

Both scripts use resumable downloads (`curl -L -C -`); re-run to continue an
interrupted transfer.

## Disk-space warnings

- Small/baseline set (excluding the large NDPI and large per-format extras):
  roughly **1 GB total**.
- The **>4 GB NDPI** path (`DOWNLOAD_LARGE=1`) adds ~**11.4 GB**
  (Hamamatsu-1.ndpi 6.43 GB + two DM0014 slides ~4.9 GB).
- `download_test_data.sh` with no `--format` downloads **everything**, which
  pulls in multi-hundred-MB items (SCN ~278 MB, FLEX ~16 MB, BDV H5 ~357 MB,
  DV ~86 MB, SDT ~30 MB, CZI ~44 MB, NDPI ~189 MB). Prefer `--format`.

## Hamamatsu NDPI (priority — 64-bit offset reconstruction)

NDPI stores TIFF-style 32-bit offsets that **wrap** past 4 GB; files over 4 GB
require reconstructing 64-bit offsets. The large rows below exercise that path.

| File | URL | Size | Exercises |
|---|---|---|---|
| CMU-1.ndpi | `https://downloads.openmicroscopy.org/images/Hamamatsu-NDPI/openslide/CMU-1/CMU-1.ndpi` | ~189 MB | Baseline <4 GB NDPI (normal 32-bit offsets) |
| CMU-2.ndpi | `https://openslide.cs.cmu.edu/download/openslide-testdata/Hamamatsu/CMU-2.ndpi` | 382 MB | Medium <4 GB NDPI |
| **Hamamatsu-1.ndpi** | `https://openslide.cs.cmu.edu/download/openslide-testdata/Hamamatsu/Hamamatsu-1.ndpi` | **6.43 GB (6,901,145,600 B)** | **>4 GB 64-bit-offset RECONSTRUCTION — primary target** |
| DM0014 ...10.25.21.ndpi | `https://downloads.openmicroscopy.org/images/Hamamatsu-NDPI/hamamatsu/DM0014%20-%202020-04-02%2010.25.21.ndpi` | 2.82 GB (2,817,876,014 B) | Near-4 GB boundary |
| DM0014 ...11.10.47.ndpi | `https://downloads.openmicroscopy.org/images/Hamamatsu-NDPI/hamamatsu/DM0014%20-%202020-04-02%2011.10.47.ndpi` | 2.11 GB (2,106,109,404 B) | Near-4 GB boundary |
| OS-1.ndpi | `https://openslide.cs.cmu.edu/download/openslide-testdata/Hamamatsu/OS-1.ndpi` | 1.86 GB | Optional large (<4 GB) |
| OS-3.ndpi | `https://openslide.cs.cmu.edu/download/openslide-testdata/Hamamatsu/OS-3.ndpi` | 1.37 GB | Optional large (<4 GB) |

Other OpenSlide NDPI slides also available: CMU-3.ndpi (270 MB),
Hamamatsu-2.ndpi (194 MB), OS-2.ndpi (931 MB).

## Other formats (verified samples)

| Format | Reader | File | URL | Size | Exercises |
|---|---|---|---|---|---|
| Aperio SVS | `svs` (`tiff_wrappers`) | CMU-1-Small-Region.svs | `https://openslide.cs.cmu.edu/download/openslide-testdata/Aperio/CMU-1-Small-Region.svs` | 1.85 MB | Single-level Aperio/TIFF whole-slide |
| Leica SCN | `scn` | Leica-1.scn | `https://downloads.openmicroscopy.org/images/Leica-SCN/openslide/Leica-1/Leica-1.scn` | 278 MB | Leica SCN pyramidal TIFF |
| Zeiss CZI | `czi` | Plate1-Blue-A-25-Scene-1-P1-F5-01.czi | `https://downloads.openmicroscopy.org/images/Zeiss-CZI/idr0011/Plate1-Blue-A_TS-Stinger/Plate1-Blue-A-25-Scene-1-P1-F5-01.czi` | 43.6 MB | CZI parsing + OME metadata |
| Nikon ND2 | `nd2` | BF007.nd2 | `https://downloads.openmicroscopy.org/images/ND2/maxime/BF007.nd2` | 270 KB | ND2 multi-series |
| Leica LIF | `lif` | PR2729_frameOrderCombinedScanTypes.lif | `https://downloads.openmicroscopy.org/images/Leica-LIF/michael/PR2729_frameOrderCombinedScanTypes.lif` | 227 KB | LIF container, multi-series |
| OME-TIFF | `tiff` | tubhiswt_C0.ome.tif (+ _C1) | `https://downloads.openmicroscopy.org/images/OME-TIFF/2016-06/tubhiswt-2D/tubhiswt_C0.ome.tif` | 270 KB each | OME-TIFF + companion files |
| Zeiss LSM | `lsm` | colocsample1b.lsm | `https://samples.fiji.sc/colocsample1b.lsm` | 2.0 MB | LSM (TIFF variant) |
| DICOM | `dicom` | MR-MONO2-12-angio-an1.dcm; CT-MONO2-16-chest.dcm | `https://downloads.openmicroscopy.org/images/DICOM/samples/MR-MONO2-12-angio-an1.dcm` | 99 KB / 145 KB | DICOM MR/CT |
| PerkinElmer FLEX | `flex` | 001001000.flex | `https://downloads.openmicroscopy.org/images/Flex/idr0007/Plate1/001001000.flex` | 16.2 MB | Flex (TIFF-based HCS) |
| Imaris IMS | `ims` | Convallaria_3C_1T_confocal.ims | `https://downloads.openmicroscopy.org/images/Imaris-IMS/davemason/Convallaria_3C_1T_confocal.ims` | 7.4 MB | Imaris HDF5 |
| ICS/IDS | `ics`/`ids` | benchmark_v1...ics + .ids | `https://downloads.openmicroscopy.org/images/ICS/jan/` (both files) | 4.4 KB + 80 KB | ICS header + IDS data pair |
| FITS | `fits` | WFPC2u5780205r_c0fx.fits | `https://fits.gsfc.nasa.gov/samples/WFPC2u5780205r_c0fx.fits` | small | FITS big-endian pixels |
| MRC | `mrc` | EMD-2225.map | `https://downloads.openmicroscopy.org/images/MRC/EMDB/EMD-2225/EMD-2225.map` | 8.4 MB | MRC/CCP4 map |
| NIfTI | `nifti` | zstat1.nii (+ .nii.gz) | `https://downloads.openmicroscopy.org/images/NIfTI/NIH/zstat1.nii` | 344 KB / 73 KB | NIfTI volume (plain + gzip) |
| Amira | `amira` | test.am | `https://downloads.openmicroscopy.org/images/AmiraMesh/ignacio/test.am` | 18 KB | AmiraMesh |
| DeltaVision | `dv` | P-TRE_12_R3D_D3D.dv | `https://downloads.openmicroscopy.org/images/DV/will/P-TRE_12_R3D_D3D.dv` | 86.5 MB | DeltaVision .dv |
| Metamorph ND | `metamorph` | test_timelapse_20240816.nd + _sN_tM.TIF | `https://downloads.openmicroscopy.org/images/Metamorph/zenodo-13642395/` | 358 B + ~2.1 MB/plane | .nd index + TIFF stack (see note below) |
| Gatan DM4 | `gatan` | SmallMontage0000.dm4 | `https://downloads.openmicroscopy.org/images/Gatan/imagesc-36590/SmallMontage0000.dm4` | 4.8 MB | Gatan DigitalMicrograph DM4 |
| Becker&Hickl SDT | `sdt` | FocalCheck_A1_20x_8xzoom_800nm.sdt | `https://downloads.openmicroscopy.org/images/SDT/gh-4198/FocalCheck_A1_20x_8xzoom_800nm.sdt` | 30 MB | SDT FLIM |
| BDV / BDV-HDF5 | `bdv` | HisYFP-SPIM.xml + .h5 | `https://downloads.openmicroscopy.org/images/BDV/samples/HisYFP-SPIM.xml` | 10 KB + 357 MB | BigDataViewer HDF5 (xml+h5 pair) |

### Format-specific notes

- **ICS/IDS** and **BDV** require **both** files of the pair (`.ics`+`.ids`,
  `.xml`+`.h5`); the script fetches both.
- **Metamorph**: no public single-file `.stk` was found in the OME directories.
  The verified public Metamorph dataset is a `.nd` index plus a set of
  `_sN_tM.TIF` planes (6 sites x 13 timepoints, ~2.1 MB each). The script fetches
  the `.nd` and one plane (`_s1_t1.TIF`); fetch additional planes from the
  directory if your reader needs the full series.
- **Gatan**: only `.dm4` samples were found publicly (no `.dm3`).
- **OME-TIFF**: `tubhiswt` is split into per-channel companion files; fetch both
  `_C0` and `_C1` (the script does).
- **DeltaVision**: 86.5 MB is the smallest `.dv` in the public OME directory.

## NOT FOUND / needs manual sourcing

No verified, public, direct-download sample could be confirmed for these. Source
manually (vendor portals, request from a lab, or generate):

| Format | Reader | Notes / where to look |
|---|---|---|
| **FlowSight / ImageStream CIF** | `flim2`/CIF | No public direct `.cif` download verified. The R package `IFCdata` (https://gitdemont.github.io/IFCdata/, a `drat` repo) ships toy `.cif`/`.rif`/`.daf` data — extract from the installed package rather than a single URL. |
| **Metamorph STK** (single-file `.stk`) | `metamorph` | Only `.nd`+`.TIF` series found publicly (see note above); a standalone multi-plane `.stk` was not located on a public mirror. |
| **Gatan DM3** | `gatan` | Only `.dm4` found publicly. DM3 samples typically come from older DigitalMicrograph exports; source from a TEM/EM lab if specifically needed. |

## Sources

- OpenSlide test data: https://openslide.cs.cmu.edu/download/openslide-testdata/
- OME public sample images: https://downloads.openmicroscopy.org/images/
- Fiji sample data (LSM): https://samples.fiji.sc/
- NASA FITS Support Office samples: https://fits.gsfc.nasa.gov/fits_samples.html
