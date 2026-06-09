#!/usr/bin/env bash
#
# download_zarr_sample.sh — Fetch a small REAL OME-Zarr (OME-NGFF v0.4) sample
# from the IDR public S3 store for the `real_idr0062a_multiscales` test in
# tests/zarr_test.rs.
#
# Sample: IDR study idr0062A (Blin et al., nuclear segmentation), image 6001240.
#   - 4D (c,z,y,x) uint16 multiscales image, 3 pyramid levels:
#       level 0: [c=2, z=236, y=275, x=271]
#       level 1: [c=2, z=236, y=137, x=135]
#       level 2: [c=2, z=236, y=68,  x=67]
#   - blosc/lz4 compressed chunks, dimension_separator "/"
#   - two omero channels: LaminB1 (0000FF) + Dapi (FFFF00)
#   - ~43 MiB once the (reader-skipped) `labels/` subtree is excluded.
#
# Source bucket (no credentials required):
#   https://uk1s3.embassy.ebi.ac.uk/idr/zarr/v0.4/idr0062A/6001240.zarr/
#
# The dataset is a DIRECTORY tree, downloaded into:
#   testdata/zarr/idr0062A_6001240.ome.zarr/
#
# Usage:
#   ./scripts/download_zarr_sample.sh            # uses aws CLI (recommended)
#   DEST=/data/samples ./scripts/download_zarr_sample.sh
#
set -euo pipefail

DEST="${DEST:-./testdata}"
OUT="${DEST}/zarr/idr0062A_6001240.ome.zarr"
ENDPOINT="https://uk1s3.embassy.ebi.ac.uk"
SRC="s3://idr/zarr/v0.4/idr0062A/6001240.zarr"

mkdir -p "$OUT"

if command -v aws >/dev/null 2>&1; then
  echo "==> Downloading OME-Zarr sample via aws s3 (no-sign-request)"
  echo "    ${ENDPOINT}/idr/zarr/v0.4/idr0062A/6001240.zarr/  ->  ${OUT}/"
  # Skip the labels/ subtree: our reader ignores it and it just adds weight.
  aws --endpoint-url "$ENDPOINT" s3 cp --no-sign-request --recursive \
    --exclude "labels/*" \
    "${SRC}/" "${OUT}/"
else
  echo "ERROR: this script needs the 'aws' CLI to recursively fetch the" >&2
  echo "       Zarr directory tree. Install awscli, or use s5cmd / rclone" >&2
  echo "       against endpoint ${ENDPOINT} bucket idr, key prefix" >&2
  echo "       zarr/v0.4/idr0062A/6001240.zarr/ (exclude labels/)." >&2
  exit 1
fi

echo
echo "==> Done. Sample at: ${OUT}"
du -sh "$OUT" 2>/dev/null || true
echo "    Verify with: cargo test --test zarr_test real_idr0062a_multiscales -- --nocapture"
