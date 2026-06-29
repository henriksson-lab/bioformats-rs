#!/usr/bin/env bash
# Refresh /big Open Microscopy manifests, then run parity and speed/RSS checks.

set -euo pipefail
cd "$(dirname "$0")/.."

ROOT="${BIOFORMATS_RS_OME_IMAGES_ROOT:-/big/henriksson/ome_images}"
TSV="${BIOFORMATS_RS_OME_IMAGES_TSV:-external-fixtures/manifests/ome_images_pending.tsv}"
PATHS="${BIOFORMATS_RS_OME_IMAGES_PATHS:-bench/target/ome-images-pending.paths}"
PER_FORMAT="${BIOFORMATS_RS_OME_IMAGES_PER_FORMAT:-2}"
WARMUP="${BIOFORMATS_RS_OME_IMAGES_WARMUP:-1}"
MEASURE="${BIOFORMATS_RS_OME_IMAGES_MEASURE:-3}"
PLANES="${BIOFORMATS_RS_OME_IMAGES_PLANES:-1}"
REGION="${BIOFORMATS_RS_OME_IMAGES_REGION:-256x256}"
MODE="all"
ALL_FLAG=()

usage() {
  sed -n '2,24p' "$0" >&2
  cat >&2 <<'EOF'

Options:
  --prepare-only   refresh manifests only
  --parity-only    refresh manifests and run Java/Rust metadata parity only
  --bench-only     refresh manifests and run subset speed/RSS only
  --all-files      include every recognized image file instead of sampling

Environment:
  BIOFORMATS_RS_OME_IMAGES_ROOT=/big/henriksson/ome_images
  BIOFORMATS_RS_OME_IMAGES_PER_FORMAT=2
  BIOFORMATS_RS_OME_IMAGES_WARMUP=1
  BIOFORMATS_RS_OME_IMAGES_MEASURE=3
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --prepare-only)
      MODE="prepare"; shift ;;
    --parity-only)
      MODE="parity"; shift ;;
    --bench-only)
      MODE="bench"; shift ;;
    --all-files)
      ALL_FLAG=(--all); shift ;;
    -h|--help)
      usage; exit 0 ;;
    *)
      echo "ERROR: unknown option: $1" >&2
      usage
      exit 2 ;;
  esac
done

scripts/prepare_ome_images_conformance.py \
  --root "$ROOT" \
  --tsv "$TSV" \
  --paths "$PATHS" \
  --per-format "$PER_FORMAT" \
  "${ALL_FLAG[@]}"

if [[ "$MODE" == "prepare" ]]; then
  exit 0
fi

if [[ "$MODE" == "all" || "$MODE" == "parity" ]]; then
  BIOFORMATS_RS_JAVA_PARITY=1 \
  BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS="${BIOFORMATS_RS_JAVA_PARITY_NO_PIXELS:-1}" \
  BIOFORMATS_RS_JAVA_PARITY_FILES="__manifest_only__" \
  BIOFORMATS_RS_JAVA_PARITY_MANIFEST="$PATHS" \
    cargo test --test java_parity_test java_parity -- --nocapture
fi

if [[ "$MODE" == "all" || "$MODE" == "bench" ]]; then
  bench/compare_subset.sh \
    --manifest "$PATHS" \
    --warmup "$WARMUP" \
    --measure "$MEASURE" \
    --planes "$PLANES" \
    --region "$REGION" \
    --out bench/target/ome-images-subset.csv \
    --markdown bench/target/ome-images-subset.md
fi
