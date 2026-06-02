#!/usr/bin/env bash
#
# download_ndpi.sh — Download Hamamatsu NDPI sample slides for integration testing.
#
# NDPI uses TIFF-style 32-bit IFD/strip offsets that WRAP past 4 GB. Files larger
# than 4 GB therefore require 64-bit offset reconstruction (the reader must add
# 0x100000000 each time an offset appears to go "backwards"). The large samples
# below are specifically here to exercise that code path.
#
# Usage:
#   ./download_ndpi.sh                # download into ./testdata
#   ./download_ndpi.sh /path/to/dest  # download into a custom directory
#
# All downloads are resumable: re-run the script to continue an interrupted file
# (curl -C -). Files already fully present are skipped by curl.
#
# DISK SPACE: the small set is ~190 MB. Enabling the >4 GB sample needs ~7 GB free.
#
set -euo pipefail

DEST="${1:-./testdata}"
mkdir -p "$DEST"

# OpenSlide test data (https://openslide.org/) and OME public sample images.
OPENSLIDE_BASE="https://openslide.cs.cmu.edu/download/openslide-testdata/Hamamatsu"
OME_NDPI_BASE="https://downloads.openmicroscopy.org/images/Hamamatsu-NDPI"

# Set DOWNLOAD_LARGE=1 in the environment to also fetch the multi-GB samples.
DOWNLOAD_LARGE="${DOWNLOAD_LARGE:-0}"

dl() {
  # dl <url> <output-filename> <description>
  local url="$1" out="$2" desc="$3"
  echo
  echo "==> ${out}  (${desc})"
  echo "    ${url}"
  curl -L -C - --fail --retry 3 -o "${DEST}/${out}" "${url}"
}

echo "Destination: ${DEST}"
echo "Large (>4 GB) downloads: $([ "$DOWNLOAD_LARGE" = 1 ] && echo ENABLED || echo disabled '(set DOWNLOAD_LARGE=1 to enable)')"

#############################################
# (a) SMALL / MEDIUM NDPI — always downloaded
#############################################

# ~189 MB. Classic OpenSlide reference slide (CMU-1). Single-file NDPI well under
# 4 GB: exercises the normal 32-bit-offset TIFF path + NDPI tags. Good first test.
dl "${OME_NDPI_BASE}/openslide/CMU-1/CMU-1.ndpi" "CMU-1.ndpi" "~189 MB, baseline <4GB NDPI"

# Same CMU-1 slide is also mirrored on OpenSlide directly (188.86 MB) if the OME
# mirror is slow; uncomment to use instead:
# dl "${OPENSLIDE_BASE}/CMU-1.ndpi" "CMU-1.ndpi" "~189 MB, OpenSlide mirror"

# ~382 MB. Larger-but-still-<4GB slide. Useful as an intermediate-size sanity test.
dl "${OPENSLIDE_BASE}/CMU-2.ndpi" "CMU-2.ndpi" "~382 MB, medium <4GB NDPI"

#############################################
# (b) LARGE (>4 GB) NDPI — 64-bit offset path
#############################################
# These are gated behind DOWNLOAD_LARGE=1 because of their size.

if [ "$DOWNLOAD_LARGE" = 1 ]; then
  # ~6.43 GB (6,901,145,600 bytes). THE key >4 GB test file. Human bone-marrow
  # aspirate smear, Hamamatsu NanoZoomer S360. Crosses the 4 GB boundary, so the
  # reader MUST reconstruct 64-bit offsets from wrapped 32-bit TIFF offsets.
  # *** PRIMARY TARGET for the offset-reconstruction code. ***
  dl "${OPENSLIDE_BASE}/Hamamatsu-1.ndpi" "Hamamatsu-1.ndpi" ">>> 6.43 GB, >4GB 64-bit-offset reconstruction test <<<"

  # ~2.82 GB and ~2.11 GB. Under 4 GB individually but near the boundary; good
  # for verifying offsets just below the wrap point behave correctly.
  dl "${OME_NDPI_BASE}/hamamatsu/DM0014 - 2020-04-02 10.25.21.ndpi" "DM0014-10.25.21.ndpi" "~2.82 GB, near-4GB boundary"
  dl "${OME_NDPI_BASE}/hamamatsu/DM0014 - 2020-04-02 11.10.47.ndpi" "DM0014-11.10.47.ndpi" "~2.11 GB, near-4GB boundary"

  # Optional additional OpenSlide >1 GB slides (uncomment as desired):
  # ~1.86 GB
  # dl "${OPENSLIDE_BASE}/OS-1.ndpi" "OS-1.ndpi" "~1.86 GB"
  # ~1.37 GB
  # dl "${OPENSLIDE_BASE}/OS-3.ndpi" "OS-3.ndpi" "~1.37 GB"
else
  echo
  echo "Skipping large (>4 GB) NDPI samples. Re-run with DOWNLOAD_LARGE=1 to fetch:"
  echo "  - Hamamatsu-1.ndpi  (~6.43 GB)  *** PRIMARY >4GB offset-reconstruction test ***"
  echo "  - DM0014 *.ndpi      (~2.82 GB + ~2.11 GB, near-boundary)"
  echo "  Total for the large set: ~11.4 GB"
fi

echo
echo "Done. Files in: ${DEST}"
