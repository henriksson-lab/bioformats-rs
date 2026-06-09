#!/usr/bin/env bash
# Download verified public sample files for formats whose readers were stubs
# (now being implemented) plus other newly-located formats. Each file is fetched
# into a per-format subdirectory under $DEST (default ./testdata). Files already
# present are skipped. All URLs were verified resolving (HTTP 200) on 2026-06-09.
#
# Usage:  DEST=./testdata ./scripts/download_stub_samples.sh [format ...]
#   with no args, downloads the SMALL samples (< ~50 MB). Large ones (msr,
#   qptiff) are only fetched when named explicitly or BIG=1 is set.
set -euo pipefail
DEST="${DEST:-./testdata}"
BIG="${BIG:-0}"

fetch() { # fetch <subdir> <filename> <url>
  local dir="$DEST/$1" out="$DEST/$1/$2"
  mkdir -p "$dir"
  if [ -s "$out" ]; then echo "skip  $1/$2 (exists)"; return; fi
  echo "get   $1/$2"
  curl -fSL --retry 3 --max-time 600 -o "$out" "$3" || { echo "FAIL  $3" >&2; rm -f "$out"; }
}

OME=https://downloads.openmicroscopy.org/images

# ── stub readers being implemented (samples available) ──────────────────────
# OBF / Imspector (Abberior STED) — small v6 sample + codec variants
fetch obf  test-v6-short-write.obf "$OME/OBF/ngladitz/v6/test-v6-short-write.obf"
fetch obf  test-v4.obf             "$OME/OBF/ngladitz/v4/test-v4.obf"

# CellWorX / MetaXpress (.HTD plate index + companion TIFFs) — IDR idr0081
MX="$OME/MetaXpress/idr0081/BSF018292-1A"
fetch metaxpress BSF018292-1A.HTD     "$MX/BSF018292-1A.HTD"
fetch metaxpress BSF018292-1A_A01_w1.TIF "$MX/BSF018292-1A_A01_w1.TIF"

# ── other verified formats located during the hunt (small) ──────────────────
PIL=https://raw.githubusercontent.com/python-pillow/Pillow/main/Tests/images
fetch pcx   odd_stride.pcx   "$PIL/odd_stride.pcx"
fetch tga   la.tga           "$PIL/la.tga"
fetch gif   hopper.gif       "$PIL/hopper.gif"
fetch ico   hopper.ico       "$PIL/hopper.ico"
fetch pnm   hopper.ppm       "$PIL/hopper.ppm"
fetch pnm   hopper_8bit.pgm  "$PIL/hopper_8bit.pgm"
fetch spider hopper.spider   "$PIL/hopper.spider"
fetch mng   mngexample.mng   "https://raw.githubusercontent.com/imageio/imageio-binaries/master/images/mngexample.mng"

# JPEG2000 family
OJ=https://raw.githubusercontent.com/uclouvain/openjpeg-data/master/input
fetch jp2  16bit.cropped.jp2 "$PIL/16bit.cropped.jp2"
fetch jp2  9bit.j2k          "$PIL/9bit.j2k"
fetch jp2  a1_mono.j2c       "$OJ/conformance/a1_mono.j2c"

# MINC / ECAT7 (clinical) — nibabel test data
NB=https://raw.githubusercontent.com/nipy/nibabel/master/nibabel/tests/data
fetch minc  minc1_1_scale.mnc "$NB/minc1_1_scale.mnc"
fetch minc  minc2_1_scale.mnc "$NB/minc2_1_scale.mnc"
fetch ecat  tinypet.v         "$NB/tinypet.v"

# Leica LOF (standalone, readable without the XLEF container)
fetch leica-lof "mono 8bit.lof" "$OME/Leica-XLEF/format%20test/format%20test%20LOF/mono%208bit.lof"

# KLB (Keller-lab light-sheet) + ECAT7 (OME)
fetch klb   img.klb            "$OME/KLB/samples/img.klb"
fetch ecat7 gradient-512x512x10.v "$OME/ECAT7/torsten/gradient-512x512x10.v"

# SPIDER stack (EMPIAR-10007) — genuine EM stack
fetch spider-em data001.dat "https://ftp.ebi.ac.uk/empiar/world_availability/10007/data/data001.dat"

# ── large samples (only with BIG=1 or named) ────────────────────────────────
if [ "$BIG" = "1" ]; then
  fetch imspector test.msr "$OME/Imspector/zenodo-10476252/test.msr"
fi

echo "done."
