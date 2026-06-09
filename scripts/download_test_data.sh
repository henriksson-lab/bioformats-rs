#!/usr/bin/env bash
#
# download_test_data.sh — Download public sample files for the formats this
# pure-Rust Bio-Formats port reads, for real-file integration testing.
#
# Each format has its own function so you can fetch just one. URLs were verified
# to resolve (directory listings / HTTP 200) at the time of writing; sizes are
# from the source directory listings.
#
# Usage:
#   ./download_test_data.sh                 # download ALL formats (large!)
#   ./download_test_data.sh --list          # list available format selectors
#   ./download_test_data.sh --format ndpi   # download just one format
#   ./download_test_data.sh --format czi --format lif   # several
#   DEST=/data/samples ./download_test_data.sh --format dicom
#
# Downloads are resumable (curl -C -). Re-run to continue interrupted files.
#
# NOTE: NDPI has a dedicated, more detailed script: ./download_ndpi.sh
#       (including the >4 GB 64-bit-offset test). This script's `ndpi` selector
#       only fetches the small baseline slide.
#
set -euo pipefail

DEST="${DEST:-./testdata}"

dl() {
  # dl <subdir> <url> <output-filename> <description>
  local sub="$1" url="$2" out="$3" desc="$4"
  local dir="${DEST}/${sub}"
  mkdir -p "$dir"
  echo
  echo "==> [${sub}] ${out}  (${desc})"
  echo "    ${url}"
  curl -L -C - --fail --retry 3 -o "${dir}/${out}" "${url}"
}

dlzip() {
  # dlzip <subdir> <url> <member-in-zip> <output-filename> <description>
  # Downloads a .zip and extracts a single member as <output-filename>.
  local sub="$1" url="$2" member="$3" out="$4" desc="$5"
  local dir="${DEST}/${sub}"
  mkdir -p "$dir"
  local tmp; tmp="$(mktemp --suffix=.zip)"
  echo
  echo "==> [${sub}] ${out}  (${desc})"
  echo "    ${url}  ::${member}"
  curl -L --fail --retry 3 -o "${tmp}" "${url}"
  unzip -p "${tmp}" "${member}" > "${dir}/${out}"
  rm -f "${tmp}"
}

OME="https://downloads.openmicroscopy.org/images"
OPENSLIDE="https://openslide.cs.cmu.edu/download/openslide-testdata"
FIJI="https://samples.fiji.sc"
NASA_FITS="https://fits.gsfc.nasa.gov/samples"
SCIF="https://samples.scif.io"
GH="https://github.com"
VTKEX="${GH}/lorensen/VTKExamples/raw/master/src/Testing/Data"

############################################################
# Per-format functions
############################################################

f_ndpi() {            # Hamamatsu NDPI — see download_ndpi.sh for >4GB test
  dl ndpi "${OME}/Hamamatsu-NDPI/openslide/CMU-1/CMU-1.ndpi" "CMU-1.ndpi" "~189 MB, baseline <4GB NDPI (use download_ndpi.sh for >4GB)"
}

f_svs() {             # Aperio SVS (TIFF-based whole slide)
  dl svs "${OPENSLIDE}/Aperio/CMU-1-Small-Region.svs" "CMU-1-Small-Region.svs" "1.85 MB, single-level Aperio SVS"
}

f_scn() {             # Leica SCN (TIFF-based whole slide)
  dl scn "${OME}/Leica-SCN/openslide/Leica-1/Leica-1.scn" "Leica-1.scn" "~278 MB, Leica SCN slide"
}

f_czi() {             # Zeiss CZI
  dl czi "${OME}/Zeiss-CZI/idr0011/Plate1-Blue-A_TS-Stinger/Plate1-Blue-A-25-Scene-1-P1-F5-01.czi" "Plate1-Blue-A-25.czi" "~43.6 MB, Zeiss CZI (idr0011 HCS)"
}

f_nd2() {             # Nikon ND2
  dl nd2 "${OME}/ND2/maxime/BF007.nd2" "BF007.nd2" "~270 KB, small Nikon ND2"
}

f_lif() {             # Leica LIF container
  dl lif "${OME}/Leica-LIF/michael/PR2729_frameOrderCombinedScanTypes.lif" "PR2729.lif" "~227 KB, small Leica LIF (multi-series)"
}

f_ometiff() {         # OME-TIFF
  dl ome-tiff "${OME}/OME-TIFF/2016-06/tubhiswt-2D/tubhiswt_C0.ome.tif" "tubhiswt_C0.ome.tif" "~270 KB, OME-TIFF (companion: tubhiswt_C1.ome.tif)"
  dl ome-tiff "${OME}/OME-TIFF/2016-06/tubhiswt-2D/tubhiswt_C1.ome.tif" "tubhiswt_C1.ome.tif" "~270 KB, OME-TIFF second channel"
}

f_lsm() {             # Zeiss LSM (510/710)
  dl lsm "${FIJI}/colocsample1b.lsm" "colocsample1b.lsm" "~2.0 MB, Zeiss LSM (Fiji samples)"
}

f_dicom() {           # DICOM medical imaging
  dl dicom "${OME}/DICOM/samples/MR-MONO2-12-angio-an1.dcm" "MR-MONO2-12-angio-an1.dcm" "~99 KB, smallest DICOM (MR)"
  dl dicom "${OME}/DICOM/samples/CT-MONO2-16-chest.dcm" "CT-MONO2-16-chest.dcm" "~145 KB, DICOM CT chest"
}

f_flex() {            # PerkinElmer FLEX
  dl flex "${OME}/Flex/idr0007/Plate1/001001000.flex" "001001000.flex" "~16.2 MB, PerkinElmer Flex (idr0007)"
}

f_ims() {             # Imaris IMS (HDF5-based)
  dl ims "${OME}/Imaris-IMS/davemason/Convallaria_3C_1T_confocal.ims" "Convallaria_3C_1T_confocal.ims" "~7.4 MB, Imaris IMS (HDF5)"
}

f_ics() {             # ICS/IDS pair (header + data, must fetch both)
  dl ics "${OME}/ICS/jan/benchmark_v1_2018_x64y64z5c2s1t11_w1Laser4054BD4BP_5c8bc101d6559_hrm.ics" "benchmark_v1.ics" "~4.4 KB, ICS header"
  dl ics "${OME}/ICS/jan/benchmark_v1_2018_x64y64z5c2s1t11_w1Laser4054BD4BP_5c8bc101d6559_hrm.ids" "benchmark_v1.ids" "~80 KB, IDS pixel data (pairs with .ics)"
}

f_fits() {            # FITS astronomy (big-endian per spec)
  dl fits "${NASA_FITS}/WFPC2u5780205r_c0fx.fits" "WFPC2u5780205r_c0fx.fits" "small, HST WFPC2 200x200x4 cube (NASA)"
}

f_mrc() {             # MRC / CCP4 map (cryo-EM)
  dl mrc "${OME}/MRC/EMDB/EMD-2225/EMD-2225.map" "EMD-2225.map" "~8.4 MB, MRC/CCP4 map (EMDB)"
}

f_nifti() {           # NIfTI neuroimaging
  dl nifti "${OME}/NIfTI/NIH/zstat1.nii" "zstat1.nii" "~344 KB, NIfTI volume"
  dl nifti "${OME}/NIfTI/NIH/zstat1.nii.gz" "zstat1.nii.gz" "~73 KB, gzipped NIfTI"
}

f_amira() {           # Amira Mesh
  dl amira "${OME}/AmiraMesh/ignacio/test.am" "test.am" "~18 KB, AmiraMesh"
}

f_dv() {              # DeltaVision .dv (moved from DV/will/ to top-level DV/)
  dl dv "${OME}/DV/P-TRE_12_R3D_D3D.dv" "P-TRE_12_R3D_D3D.dv" "~86.5 MB, DeltaVision (smallest in dir)"
}

f_metamorph() {       # Metamorph .nd + TIFF stack (no public .stk found; .nd dataset)
  local base="${OME}/Metamorph/zenodo-13642395"
  dl metamorph "${base}/test_timelapse_20240816.nd" "test_timelapse_20240816.nd" "~358 B, Metamorph .nd index file"
  dl metamorph "${base}/test_timelapse_20240816_s1_t1.TIF" "test_timelapse_20240816_s1_t1.TIF" "~2.1 MB, one referenced TIFF plane"
  echo "    NOTE: full Metamorph .nd dataset references many _sN_tM.TIF planes; fetch the rest from ${base}/ if needed."
}

f_gatan() {           # Gatan DigitalMicrograph DM4 (no public DM3 found)
  dl gatan "${OME}/Gatan/imagesc-36590/SmallMontage0000.dm4" "SmallMontage0000.dm4" "~4.8 MB, Gatan DM4 (no public DM3 sample found)"
}

f_sdt() {             # Becker & Hickl SDT (FLIM)
  dl sdt "${OME}/SDT/gh-4198/FocalCheck_A1_20x_8xzoom_800nm.sdt" "FocalCheck.sdt" "~30 MB, B&H SDT FLIM"
}

f_bdv() {             # BigDataViewer / BDV-HDF5 (.h5 + .xml, fetch both)
  dl bdv "${OME}/BDV/samples/HisYFP-SPIM.xml" "HisYFP-SPIM.xml" "~10 KB, BDV XML (pairs with .h5)"
  dl bdv "${OME}/BDV/samples/HisYFP-SPIM.h5" "HisYFP-SPIM.h5" "~357 MB, BDV HDF5 SPIM dataset"
}

f_pic() {             # Bio-Rad PIC (BioRadReader)
  # sdub.zip contains 12 single-plane-stack .pic files (sdub1..sdub12); take one.
  dlzip pic "${SCIF}/sdub.zip" "sdub1.pic" "sdub1.pic" "~216 KB, Bio-Rad PIC 192x128x9 uint8 (Java oracle: ok)"
}

f_nrrd() {            # NRRD (NrrdReader) — self-contained, attached gzip-encoded data.
  # NOTE: server name has a .gz suffix but the file is a complete .nrrd
  # (magic NRRD0001, internal 'encoding: gzip'); store it as plain .nrrd.
  dl nrrd "${OME}/NRRD/glencoe/gzip/dt-helix.nrrd.gz" "dt-helix.nrrd" "~1.0 MB, self-contained NRRD 38x39x40, 7ch float (Java oracle: ok)"
}

f_spe() {             # Princeton Instruments SPE (SpeReader)
  dl spe "${GH}/imageio/imageio-binaries/raw/master/images/test_000_.SPE" "test_000_.spe" "~8 KB, SPE 32x32 uint16, 2 frames (Java oracle: ok)"
}

f_andor() {           # Andor SIF (AndorSifReader)
  dl sif "${GH}/fujiisoup/sif_reader/raw/master/testings/public_testdata/image.sif" "image.sif" "~2.1 MB, Andor SIF 512x512 float (Java oracle: ok)"
}

f_klb() {             # KLB (KlbReader)
  dl klb "${OME}/KLB/samples/img.klb" "img.klb" "~373 KB, KLB 101x151x29 uint16 (Java oracle: ok)"
}

f_metaimage() {       # MetaImage (MetaImageReader) — detached .mhd + .raw pair.
  # NOTE: Java Bio-Formats has NO MetaImage reader, so the oracle cannot verify
  # this; it is a Rust-only reader. A LOCAL self-contained .mha is avoided because
  # the current Rust parser reads the header line-by-line as UTF-8 and chokes on
  # the trailing binary; a detached .mhd (pure-text header) + .raw works.
  dl mha "${VTKEX}/HeadMRVolume.mhd" "HeadMRVolume.mhd" "231 B, MetaImage header 48x62x42 uint8 (Java: no reader; Rust: ok)"
  dl mha "${VTKEX}/HeadMRVolume.raw" "HeadMRVolume.raw" "~122 KB, raw pixel data paired with HeadMRVolume.mhd"
}

f_stk() {             # Metamorph STK (MetamorphReader) — standalone .stk stack.
  # f_metamorph fetches the .nd + .TIF dataset; this is a true single-file .stk.
  dl stk "${GH}/CellProfiler/CellProfiler/raw/main/tests/core/data/modules/loadimages/C0.stk" "C0.stk" "~13 MB, Metamorph STK 800x800, 7 planes, 3ch uint8 (Java oracle: ok)"
}

f_jpg() {             # plain JPEG (JpegReader)
  dlzip jpg "${SCIF}/test-jpg.zip" "scifio-test.jpg" "scifio-test.jpg" "~102 KB, JPEG 500x500 RGB (Java oracle: ok)"
}

f_png() {             # plain PNG (PngReader)
  dlzip png "${SCIF}/test-png.zip" "scifio-test.png" "scifio-test.png" "~71 KB, PNG 500x500 RGB (Java oracle: ok)"
}

f_bmp() {             # plain BMP (BmpReader)
  dl bmp "${GH}/imageio/imageio-binaries/raw/master/images/scribble_P_RGB.bmp" "scribble_P_RGB.bmp" "~470 KB, BMP 800x600 indexed (Java oracle: ok)"
}

############################################################
# Dispatch
############################################################

FORMATS=(ndpi svs scn czi nd2 lif ometiff lsm dicom flex ims ics fits mrc nifti amira dv metamorph gatan sdt bdv pic nrrd spe andor klb metaimage stk jpg png bmp)

list_formats() {
  echo "Available --format selectors:"
  for f in "${FORMATS[@]}"; do echo "  - $f"; done
  echo
  echo "FlowSight CIF: no verified public direct download (see scripts/README.md 'NOT FOUND')."
}

run_format() {
  case "$1" in
    ndpi) f_ndpi ;; svs) f_svs ;; scn) f_scn ;; czi) f_czi ;; nd2) f_nd2 ;;
    lif) f_lif ;; ometiff|ome-tiff) f_ometiff ;; lsm) f_lsm ;; dicom) f_dicom ;;
    flex) f_flex ;; ims|imaris) f_ims ;; ics|ids) f_ics ;; fits) f_fits ;;
    mrc) f_mrc ;; nifti) f_nifti ;; amira) f_amira ;; dv|deltavision) f_dv ;;
    metamorph|nd) f_metamorph ;; gatan|dm4|dm3) f_gatan ;; sdt) f_sdt ;;
    bdv|h5) f_bdv ;;
    pic|biorad) f_pic ;; nrrd) f_nrrd ;; spe) f_spe ;;
    andor|sif) f_andor ;; klb) f_klb ;; metaimage|mha|mhd) f_metaimage ;;
    stk) f_stk ;; jpg|jpeg) f_jpg ;; png) f_png ;; bmp) f_bmp ;;
    *) echo "Unknown format: $1" >&2; list_formats; exit 1 ;;
  esac
}

main() {
  if [ "$#" -eq 0 ]; then
    echo "No --format given; downloading ALL formats into ${DEST} (this is large)."
    echo "Use './download_test_data.sh --list' to see selectors, or Ctrl-C to abort."
    for f in "${FORMATS[@]}"; do run_format "$f"; done
    echo; echo "Done. Files in: ${DEST}"
    return
  fi

  local selected=()
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --list) list_formats; exit 0 ;;
      --format) shift; [ -n "${1:-}" ] || { echo "--format needs a value" >&2; exit 1; }; selected+=("$1") ;;
      -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
      *) echo "Unknown arg: $1" >&2; exit 1 ;;
    esac
    shift
  done

  for f in "${selected[@]}"; do run_format "$f"; done
  echo; echo "Done. Files in: ${DEST}"
}

main "$@"
