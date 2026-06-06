#!/usr/bin/env bash
#
# download_biostudies_data.sh — Download real sample files from the EMBL-EBI
# BioImage Archive (and sibling EMPIAR) for formats this pure-Rust Bio-Formats
# port reads but that are NOT yet covered by scripts/download_test_data.sh.
#
# Source: https://www.ebi.ac.uk/bioimage-archive/  (beta.bioimagearchive.org)
# Every URL below was resolved to HTTP 200 with the stated Content-Length via
# the BioStudies file-list JSON API at the time of writing. Studies are cited so
# you can re-derive a file if one is withdrawn:
#   curl -s "https://www.ebi.ac.uk/biostudies/api/v1/studies/<ACC>/info" | jq .httpLink
#   curl -s "<httpLink>/Files/<FileList>.json" | jq '.[]|{path,size}'
#
# Usage:
#   ./download_biostudies_data.sh                 # download ALL (some are large)
#   ./download_biostudies_data.sh --list          # list selectors
#   ./download_biostudies_data.sh --format oib     # download just one
#   ./download_biostudies_data.sh --format oib --format zvi
#   DEST=/data/samples ./download_biostudies_data.sh --format avi
#
# Downloads are resumable (curl -C -). Layout mirrors ./testdata/<fmt>/ so
# tests/real_data_test.rs can find them (e.g. testdata/oib/..., testdata/zvi/...).
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

# BioStudies FTP roots. Files live under <study-root>/Files/<path>.
BIA="https://ftp.ebi.ac.uk/biostudies/fire"           # most studies
BIA_PUB="https://ftp.ebi.ac.uk/pub/databases/biostudies"  # some newer studies
EMPIAR="https://ftp.ebi.ac.uk/empiar/world_availability"  # EM archive (sibling)

############################################################
# Per-format functions — formats NOT in download_test_data.sh
############################################################

f_oib() {             # Olympus FluoView OIB — single self-contained OLE file
  # Study S-BIAD1612 (Cry11Aa toxin colocalization), 272 files, no companions.
  dl oib "${BIA}/S-BIAD/612/S-BIAD1612/Files/CryTox_Colocalization_Image_Data1/Colocalization_Manders_Endosomes/6_Cry11_1_mg_FM4_3h_60X_Z3.5_ant_bis_3.oib" \
    "cry11_colocalization.oib" "~6.35 MB, Olympus FluoView OIB (S-BIAD1612)"
}

f_oir() {             # Olympus OIR (newer) — single self-contained container
  # Study S-BIAD1557 (GFP/RFP-ATG8 confocal). NOTE: repo OirReader is a stub.
  dl oir "${BIA}/S-BIAD/557/S-BIAD1557/Files/Subimission.v2/Fig.3_data/raw_data/FIg.3a_Nb%20intact_MIP.oir" \
    "atg8_fig3a_mip.oir" "~6.29 MB, Olympus OIR (S-BIAD1557)"
}

f_zvi() {             # Zeiss AxioVision ZVI — single OLE compound file
  # Study S-BSST1202 (tumor endothelial autophagy), no companions.
  dl zvi "${BIA}/S-BSST/202/S-BSST1202/Files/Fig%203d%20raw%20image%20wt%20sting%20cd31.zvi" \
    "fig3d_wt_sting_cd31.zvi" "~8.81 MB, Zeiss ZVI (S-BSST1202)"
}

f_avi() {             # AVI movie
  # Study S-BSST212 (SCN AVP/VIP). NOTE: httpLink uses padded S-BSSTxxx212 form.
  dl avi "${BIA}/S-BSST/S-BSSTxxx212/S-BSST212/Files/20120928_CryPER2_newborn_SNE10.avi" \
    "cryper2_newborn.avi" "~6.46 MB, AVI timelapse (S-BSST212)"
}

f_psd() {             # Adobe Photoshop PSD
  # Study S-BIAD2364 (human embryonic limb atlas).
  dl psd "${BIA_PUB}/S-BIAD/364/S-BIAD2364/Files/Shuaiyu/FGF8%20FGFR2%20FGF10/FGF8_pcW5.6.psd" \
    "fgf8_pcw5.psd" "~1.0 MB, Photoshop PSD (S-BIAD2364)"
}

f_dm3() {             # Gatan DigitalMicrograph DM3 (download_test_data.sh has DM4)
  # Study S-BIAD1712 (autophagy CLEM). All DM3 here are large (~67 MB).
  dl dm3 "${BIA}/S-BIAD/712/S-BIAD1712/Files/CLEM%20Fig%203b/Fig%203b_top%20panel.dm3" \
    "clem_fig3b.dm3" "~67 MB, Gatan DM3 EM (S-BIAD1712) — large, smallest available"
}

f_imagic() {          # IMAGIC — .hed header + .img data, MUST keep the pair
  # EMPIAR-10083 (bacteriophage P22). Both files share the basename.
  dl imagic "${EMPIAR}/10083/data/donghua/12409.stpm.hed" \
    "12409.stpm.hed" "3 KB, IMAGIC header (EMPIAR-10083)"
  dl imagic "${EMPIAR}/10083/data/donghua/12409.stpm.img" \
    "12409.stpm.img" "~8.5 MB, IMAGIC data (pairs with .hed)"
}

f_vsi() {             # Olympus VSI — tiny header + large .ets pixel data
  # Study S-BIAD1328. The .vsi header alone is useless without the .ets stack,
  # which is ~147 MB. Keep both, mirroring the original folder structure so the
  # reader can resolve the companion: <name>.vsi  +  _<name>_/stack1/frame_t.ets
  dl vsi "${BIA}/S-BIAD/328/S-BIAD1328/Files/Figure%205D/HN%20485%20HNSCC%20APOBEC3A-1.1000.vsi" \
    "HN 485 HNSCC APOBEC3A-1.1000.vsi" "~278 KB, Olympus VSI header (S-BIAD1328)"
  dl "vsi/_HN 485 HNSCC APOBEC3A-1.1000_/stack1" \
    "${BIA}/S-BIAD/328/S-BIAD1328/Files/Figure%205D/_HN%20485%20HNSCC%20APOBEC3A-1.1000_/stack1/frame_t.ets" \
    "frame_t.ets" "~147 MB, VSI pixel data (.ets) — required companion"
}

f_oif() {             # Olympus OIF — multi-file bundle, only zip-packaged in BIA
  # Study S-BSST374. After download, unzip to get <name>.oif + <name>.oif.files/.
  dl oif "${BIA}/S-BSST/S-BSSTxxx374/S-BSST374/Files/Source%20Data%20Figure%20S5.zip" \
    "oif_figureS5.zip" "~16 MB zip, contains 2 OIF bundles (S-BSST374)"
  if command -v unzip >/dev/null 2>&1; then
    echo "    unzipping OIF bundle (master .oif + .oif.files/ dir must stay together)..."
    unzip -o -q "${DEST}/oif/oif_figureS5.zip" -d "${DEST}/oif" || \
      echo "    (unzip failed — extract ${DEST}/oif/oif_figureS5.zip manually)"
  else
    echo "    NOTE: 'unzip' not found. Extract ${DEST}/oif/oif_figureS5.zip manually."
    echo "          Keep each <name>.oif together with its sibling <name>.oif.files/ dir."
  fi
}

############################################################
# Dispatch
############################################################

# Only formats with a verified BioImage Archive / EMPIAR source. Formats the
# repo supports but that are ABSENT from the archive (depositors convert them to
# TIFF/OME-TIFF/MRC first) are listed in NOT_IN_ARCHIVE below.
FORMATS=(oib oir zvi avi psd dm3 imagic vsi oif)

NOT_IN_ARCHIVE=(
  "ser  (FEI/TIA)       — EMPIAR deposits are MRC/TIFF; EMPIAR-10533 cites .ser but folders hold converted TIFFs"
  "spe  (Princeton)     — not present; use downloads.openmicroscopy.org/images/SPE/"
  "pic  (Bio-Rad)       — not present; use downloads.openmicroscopy.org/images/Bio-Rad-PIC/"
  "nrrd                  — only inside supplementary .zip archives, no loose file"
  "seq  (Norpix)        — not present (all 'seq' hits are 'sequencing' text)"
  "mvd2 (Volocity)      — only as 307 MB+ library .zip; too large for a fixture"
  "ch5  (CellH5)        — not present; archive stores converted TIFF/ND2"
  "xdce (GE InCell)     — not present"
  "lei  (Leica LCS)     — not present; modern Leica deposits are .lif/OME-TIFF"
  "lim  (Nikon)         — not present; Nikon deposits are .nd2"
  "xrm/txrm (Zeiss Xradia) — not present; X-ray data stored as TIFF/.rec"
  "mias / Olympus ScanR — not present; stored as plain .tif or .zip"
)

list_formats() {
  echo "Available --format selectors (verified BioImage Archive / EMPIAR sources):"
  for f in "${FORMATS[@]}"; do echo "  - $f"; done
  echo
  echo "Supported by the repo but ABSENT from the BioImage Archive (get elsewhere):"
  for n in "${NOT_IN_ARCHIVE[@]}"; do echo "  * $n"; done
  echo
  echo "Already covered by scripts/download_test_data.sh:"
  echo "  amira bdv czi dicom dv fits flex gatan(dm4) ics ims lif lsm metamorph"
  echo "  mrc nd2 ndpi nifti ometiff scn sdt svs"
}

run_format() {
  case "$1" in
    oib) f_oib ;; oir) f_oir ;; zvi) f_zvi ;; avi) f_avi ;; psd) f_psd ;;
    dm3) f_dm3 ;; imagic|hed|img) f_imagic ;; vsi|ets) f_vsi ;; oif) f_oif ;;
    *) echo "Unknown format: $1" >&2; list_formats; exit 1 ;;
  esac
}

main() {
  if [ "$#" -eq 0 ]; then
    echo "No --format given; downloading ALL formats into ${DEST}."
    echo "Some files are large (dm3 ~67 MB, vsi ~147 MB). Use '--list' or Ctrl-C to abort."
    for f in "${FORMATS[@]}"; do run_format "$f"; done
    echo; echo "Done. Files in: ${DEST}"
    return
  fi

  local selected=()
  while [ "$#" -gt 0 ]; do
    case "$1" in
      --list) list_formats; exit 0 ;;
      --format) shift; [ -n "${1:-}" ] || { echo "--format needs a value" >&2; exit 1; }; selected+=("$1") ;;
      -h|--help) sed -n '2,30p' "$0"; exit 0 ;;
      *) echo "Unknown arg: $1" >&2; exit 1 ;;
    esac
    shift
  done

  for f in "${selected[@]}"; do run_format "$f"; done
  echo; echo "Done. Files in: ${DEST}"
}

main "$@"
