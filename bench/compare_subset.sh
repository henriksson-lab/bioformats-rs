#!/usr/bin/env bash
# Compare Java Bio-Formats and bioformats-rs subset-read speed/RSS.
#
# Usage:
#   bench/compare_subset.sh [options] <file-or-dir>...
#
# Options:
#   --warmup N        warmup iterations per engine (default: 2)
#   --measure N       measured iterations per engine (default: 5)
#   --planes N        planes read per series (default: 1)
#   --region WxH      centered crop size, clamped to image bounds (default: 256x256)
#   --manifest FILE   newline-delimited paths to benchmark
#   --out FILE        CSV output path (default: bench/target/subset-comparison.csv)
#   --markdown FILE   Markdown output path (default: bench/target/subset-comparison.md)
#   --find-testdata   benchmark one discovered file per extension under testdata/ and test/
#
# Timing excludes process startup inside each harness and includes reader open +
# subset reads per measured iteration. RSS is measured around the whole harness
# process with /usr/bin/time, so Java RSS includes JVM overhead.

set -euo pipefail
cd "$(dirname "$0")/.."

WARMUP=2
MEASURE=5
PLANES=1
REGION_W=256
REGION_H=256
OUT="bench/target/subset-comparison.csv"
MARKDOWN="bench/target/subset-comparison.md"
MANIFEST=""
FIND_TESTDATA=0
INPUTS=()

usage() {
  sed -n '2,24p' "$0" >&2
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --warmup)
      WARMUP="$2"; shift 2 ;;
    --measure)
      MEASURE="$2"; shift 2 ;;
    --planes)
      PLANES="$2"; shift 2 ;;
    --region)
      if [[ "$2" != *x* ]]; then
        echo "ERROR: --region must be WxH, e.g. 256x256" >&2
        exit 2
      fi
      REGION_W="${2%x*}"
      REGION_H="${2#*x}"
      shift 2 ;;
    --manifest)
      MANIFEST="$2"; shift 2 ;;
    --out)
      OUT="$2"; shift 2 ;;
    --markdown)
      MARKDOWN="$2"; shift 2 ;;
    --find-testdata)
      FIND_TESTDATA=1; shift ;;
    -h|--help)
      usage; exit 0 ;;
    --)
      shift
      while [[ $# -gt 0 ]]; do INPUTS+=("$1"); shift; done ;;
    -*)
      echo "ERROR: unknown option: $1" >&2
      usage
      exit 2 ;;
    *)
      INPUTS+=("$1"); shift ;;
  esac
done

if [[ -n "$MANIFEST" ]]; then
  if [[ ! -f "$MANIFEST" ]]; then
    echo "ERROR: manifest not found: $MANIFEST" >&2
    exit 2
  fi
  while IFS= read -r line; do
    [[ -z "$line" || "$line" == \#* ]] && continue
    INPUTS+=("$line")
  done < "$MANIFEST"
fi

if [[ "$FIND_TESTDATA" -eq 1 ]]; then
  while IFS= read -r path; do
    INPUTS+=("$path")
  done < <(
    find test testdata \
      -type d -name '*.zarr' -print -prune \
      -o -type f \
        ! -path '*/.git/*' \
        ! -name '*.xlsx' \
        ! -name '*.log' \
        ! -name '*.ids' \
        ! -name '*.raw' \
        ! -name '*.pty' \
        ! -name '*.lut' \
        ! -name '*.roi' \
        -print \
      | awk '
          {
            name=$0
            sub(/^.*\//, "", name)
            lower=tolower(name)
            if (lower ~ /\.ome\.tiff?$/) ext="ome-tiff"
            else if (lower ~ /\.ome\.zarr$/) ext="ome-zarr"
            else {
              ext=name
              sub(/^.*\./, "", ext)
              if (ext == name) ext="__noext"
            }
            ext=tolower(ext)
            if (!(ext in seen)) { seen[ext]=1; print $0 }
          }'
  )
fi

if [[ "${#INPUTS[@]}" -eq 0 ]]; then
  INPUTS=(
    "test/tubhiswt_C0.ome.tif"
    "testdata/nd2/BF007.nd2"
    "testdata/czi/Plate1-Blue-A-25.czi"
    "testdata/lsm/colocsample1b.lsm"
    "testdata/svs/CMU-1-Small-Region.svs"
    "testdata/avi/cryper2_newborn.avi"
    "testdata/dicom/CT-MONO2-16-chest.dcm"
    "testdata/ics/benchmark_v1.ics"
  )
fi

if [[ ! -f bioformats_package.jar ]]; then
  echo "ERROR: bioformats_package.jar not found in repo root" >&2
  exit 2
fi
if ! command -v /usr/bin/time >/dev/null 2>&1; then
  echo "ERROR: /usr/bin/time is required for peak RSS measurement" >&2
  exit 2
fi

mkdir -p bench/target

echo "Building Rust subset bench..."
cargo build --release --manifest-path bench/Cargo.toml --bin bench_subset_rust -q
RUST_BIN="bench/target/release/bench_subset_rust"

CLASS_DIR="bench/target/java"
mkdir -p "$CLASS_DIR"
echo "Compiling Java subset bench..."
javac -cp bioformats_package.jar bench/BfSubsetBench.java -d "$CLASS_DIR"

csv_quote() {
  local s="${1:-}"
  s="${s//\"/\"\"}"
  printf '"%s"' "$s"
}

key_value() {
  local file="$1"
  local key="$2"
  awk -F= -v key="$key" '$1 == key { sub(/^[^=]*=/, ""); print; exit }' "$file"
}

format_label() {
  local path="$1"
  local base ext
  base="$(basename "$path")"
  if [[ "${base,,}" == *.ome.tif || "${base,,}" == *.ome.tiff ]]; then
    printf 'ome-tiff'
  elif [[ "${base,,}" == *.ome.zarr ]]; then
    printf 'ome-zarr'
  elif [[ "$base" == *.* ]]; then
    ext="${base##*.}"
    printf '%s' "${ext,,}"
  else
    printf 'noext'
  fi
}

to_ms() {
  awk -v ns="${1:-0}" 'BEGIN { if (ns == "" || ns == 0) print ""; else printf "%.3f", ns / 1000000.0 }'
}

ratio() {
  awk -v a="${1:-0}" -v b="${2:-0}" 'BEGIN { if (a == "" || b == "" || a == 0 || b == 0) print ""; else printf "%.3f", a / b }'
}

run_engine() {
  local engine="$1"
  local path="$2"
  local out_file="$3"
  local err_file="$4"

  if [[ "$engine" == "java" ]]; then
    /usr/bin/time -f "__rss_kb=%M" \
      java -cp "bioformats_package.jar:$CLASS_DIR" BfSubsetBench \
      "$path" "$WARMUP" "$MEASURE" "$PLANES" "$REGION_W" "$REGION_H" \
      > "$out_file" 2> "$err_file" || true
  else
    /usr/bin/time -f "__rss_kb=%M" \
      "$RUST_BIN" "$path" "$WARMUP" "$MEASURE" "$PLANES" "$REGION_W" "$REGION_H" \
      > "$out_file" 2> "$err_file" || true
  fi
}

rss_value() {
  local err_file="$1"
  awk -F= '$1 == "__rss_kb" { value=$2 } END { print value }' "$err_file"
}

error_text() {
  local out_file="$1"
  local err_file="$2"
  local err
  err="$(key_value "$out_file" error)"
  if [[ -z "$err" ]]; then
    err="$(grep -v '^__rss_kb=' "$err_file" | tr '\n\r' ' ' | sed 's/[[:space:]]\+/ /g')"
  fi
  printf '%s' "$err"
}

{
  printf 'format,path,java_status,rust_status,java_ms,rust_ms,rust_speedup_vs_java,java_peak_rss_kb,rust_peak_rss_kb,java_rss_over_rust,java_bytes,rust_bytes,java_series,rust_series,java_planes,rust_planes,max_width,max_height,java_error,rust_error\n'
} > "$OUT"

{
  printf '| Format | Path | Java status | Rust status | Java ms | Rust ms | Rust speedup | Java RSS KiB | Rust RSS KiB | RSS ratio | Bytes J/R | Planes J/R |\n'
  printf '|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|\n'
} > "$MARKDOWN"

for path in "${INPUTS[@]}"; do
  if [[ ! -e "$path" ]]; then
    echo "Skipping missing path: $path" >&2
    continue
  fi

  echo "Benchmarking $path"
  tmp_java_out="$(mktemp)"
  tmp_java_err="$(mktemp)"
  tmp_rust_out="$(mktemp)"
  tmp_rust_err="$(mktemp)"

  run_engine java "$path" "$tmp_java_out" "$tmp_java_err"
  run_engine rust "$path" "$tmp_rust_out" "$tmp_rust_err"

  java_status="$(key_value "$tmp_java_out" status)"
  rust_status="$(key_value "$tmp_rust_out" status)"
  java_status="${java_status:-error}"
  rust_status="${rust_status:-error}"
  java_ns="$(key_value "$tmp_java_out" avg_ns)"
  rust_ns="$(key_value "$tmp_rust_out" avg_ns)"
  java_ms="$(to_ms "$java_ns")"
  rust_ms="$(to_ms "$rust_ns")"
  speedup="$(ratio "$java_ns" "$rust_ns")"
  java_rss="$(rss_value "$tmp_java_err")"
  rust_rss="$(rss_value "$tmp_rust_err")"
  rss_ratio="$(ratio "$java_rss" "$rust_rss")"
  java_bytes="$(key_value "$tmp_java_out" bytes)"
  rust_bytes="$(key_value "$tmp_rust_out" bytes)"
  java_series="$(key_value "$tmp_java_out" series)"
  rust_series="$(key_value "$tmp_rust_out" series)"
  java_planes="$(key_value "$tmp_java_out" planes)"
  rust_planes="$(key_value "$tmp_rust_out" planes)"
  max_width="$(key_value "$tmp_rust_out" max_width)"
  max_height="$(key_value "$tmp_rust_out" max_height)"
  if [[ -z "$max_width" ]]; then max_width="$(key_value "$tmp_java_out" max_width)"; fi
  if [[ -z "$max_height" ]]; then max_height="$(key_value "$tmp_java_out" max_height)"; fi
  java_error="$(error_text "$tmp_java_out" "$tmp_java_err")"
  rust_error="$(error_text "$tmp_rust_out" "$tmp_rust_err")"
  format="$(format_label "$path")"

  {
    csv_quote "$format"; printf ','
    csv_quote "$path"; printf ','
    csv_quote "$java_status"; printf ','
    csv_quote "$rust_status"; printf ','
    csv_quote "$java_ms"; printf ','
    csv_quote "$rust_ms"; printf ','
    csv_quote "$speedup"; printf ','
    csv_quote "$java_rss"; printf ','
    csv_quote "$rust_rss"; printf ','
    csv_quote "$rss_ratio"; printf ','
    csv_quote "$java_bytes"; printf ','
    csv_quote "$rust_bytes"; printf ','
    csv_quote "$java_series"; printf ','
    csv_quote "$rust_series"; printf ','
    csv_quote "$java_planes"; printf ','
    csv_quote "$rust_planes"; printf ','
    csv_quote "$max_width"; printf ','
    csv_quote "$max_height"; printf ','
    csv_quote "$java_error"; printf ','
    csv_quote "$rust_error"; printf '\n'
  } >> "$OUT"

  printf '| %s | `%s` | %s | %s | %s | %s | %s | %s | %s | %s | %s/%s | %s/%s |\n' \
    "$format" "$path" "$java_status" "$rust_status" "${java_ms:-}" "${rust_ms:-}" \
    "${speedup:-}" "${java_rss:-}" "${rust_rss:-}" "${rss_ratio:-}" \
    "${java_bytes:-}" "${rust_bytes:-}" "${java_planes:-}" "${rust_planes:-}" >> "$MARKDOWN"

  rm -f "$tmp_java_out" "$tmp_java_err" "$tmp_rust_out" "$tmp_rust_err"
done

echo
echo "CSV:      $OUT"
echo "Markdown: $MARKDOWN"
