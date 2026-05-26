#!/usr/bin/env python3
"""Audit local CZI fixture directory entries for mosaic/pyramid coverage."""

from __future__ import annotations

import argparse
import csv
import struct
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
REPO_ROOT = ROOT.parent
MANIFESTS = ROOT / "manifests"
LOCAL_CZI_ROOT = ROOT / "data" / "czi"
SEG_HEADER = 32
TARGETED_CZI_SETS = {
    "czi-openslide-zeiss5-smoke": "mosaic-jpegxr-smoke",
    "czi-openslide-zeiss5-feature": "mosaic-jpegxr",
    "czi-openslide-zeiss5-pyramid": "pyramid-jpegxr",
    "czi-synthetic-tile-smoke": "mosaic",
    "czi-synthetic-tile-feature": "mosaic",
    "czi-synthetic-tile-regression-large": "mosaic-regression-large",
}


@dataclass(frozen=True)
class Entry:
    pixel_type: int
    file_position: int
    compression: int
    dims: dict[str, tuple[int, int, int]]

    def start(self, dim: str) -> int:
        return self.dims.get(dim, (0, 1, 1))[0]

    def size(self, dim: str) -> int:
        return self.dims.get(dim, (0, 1, 1))[1]

    def stored_size(self, dim: str) -> int:
        return self.dims.get(dim, (0, 1, 1))[2]

    def plane_key(self) -> tuple[int, int, int, int, int]:
        return (
            self.start("R"),
            self.start("S"),
            self.start("Z"),
            self.start("C"),
            self.start("T"),
        )


def u64(data: bytes, offset: int) -> int:
    return struct.unpack_from("<Q", data, offset)[0]


def i64(data: bytes, offset: int) -> int:
    return struct.unpack_from("<q", data, offset)[0]


def i32(data: bytes, offset: int) -> int:
    return struct.unpack_from("<i", data, offset)[0]


def segment_type(header: bytes) -> str:
    return header[:16].split(b"\0", 1)[0].decode("ascii", errors="replace")


def valid_segment_position(position: int, file_len: int) -> bool:
    return position > 0 and position + SEG_HEADER <= file_len


def choose_segment_position(primary: int, fallback: int, file_len: int) -> int:
    if valid_segment_position(primary, file_len):
        return primary
    if valid_segment_position(fallback, file_len):
        return fallback
    return 0


def parse_entry(data: bytes) -> Entry:
    pixel_type = i32(data, 2)
    file_position = i64(data, 6)
    compression = i32(data, 18)
    dim_count = max(i32(data, 28), 0)
    dims: dict[str, tuple[int, int, int]] = {}
    for index in range(dim_count):
        offset = 32 + index * 20
        if offset + 20 > len(data):
            break
        name = (
            data[offset : offset + 4]
            .split(b"\0", 1)[0]
            .decode("ascii", errors="replace")
            .strip()
        )
        if name:
            dims[name] = (i32(data, offset + 4), i32(data, offset + 8), i32(data, offset + 16))
    return Entry(pixel_type, file_position, compression, dims)


def parse_entries(data: bytes, entry_count: int) -> list[Entry]:
    if entry_count <= 0:
        return []
    fixed_stride = 256 if len(data) >= entry_count * 256 else None
    entries: list[Entry] = []
    offset = 0
    for _ in range(entry_count):
        if offset + 32 > len(data):
            break
        dim_count = max(i32(data, offset + 28), 0)
        compact_len = 32 + dim_count * 20
        stride = fixed_stride or compact_len
        if offset + compact_len > len(data):
            break
        entry_len = min(stride, len(data) - offset)
        entries.append(parse_entry(data[offset : offset + entry_len]))
        offset += stride
    return entries


def read_entries(path: Path) -> list[Entry]:
    with path.open("rb") as handle:
        file_len = path.stat().st_size
        header = handle.read(SEG_HEADER)
        if len(header) != SEG_HEADER or not segment_type(header).startswith("ZISRAWFILE"):
            raise ValueError("not a CZI file")
        file_header = handle.read(80)
        real_dir_position = u64(file_header, 52)
        legacy_dir_position = u64(file_header, 36)
        dir_position = choose_segment_position(real_dir_position, legacy_dir_position, file_len)
        if dir_position == 0:
            return []

        handle.seek(dir_position)
        dir_header = handle.read(SEG_HEADER)
        allocated = u64(dir_header, 16)
        used = u64(dir_header, 24) or allocated
        body_header = handle.read(128)
        entry_count = i32(body_header, 0)
        body_size = max(allocated, used) - 128
        body_size = min(body_size, file_len - handle.tell())
        return parse_entries(handle.read(body_size), entry_count)


def manifest_czi_paths(manifest: Path) -> list[Path]:
    paths: list[Path] = []
    with manifest.open(newline="", encoding="utf-8") as handle:
        for row in csv.DictReader(handle, delimiter="\t"):
            if row.get("category") == "czi":
                paths.append(Path(row["local_path"]))
    return paths


def manifest_targeted_czi_rows(manifest: Path) -> list[dict[str, str]]:
    if not manifest.exists():
        return []
    rows: list[dict[str, str]] = []
    with manifest.open(newline="", encoding="utf-8") as handle:
        for row in csv.DictReader(handle, delimiter="\t"):
            fixture_set = row.get("set", "")
            if row.get("category") == "czi" and fixture_set in TARGETED_CZI_SETS:
                rows.append(row)
    return rows


def local_czi_paths() -> list[Path]:
    if not LOCAL_CZI_ROOT.exists():
        return []
    return sorted(path for path in LOCAL_CZI_ROOT.rglob("*.czi") if path.is_file())


def default_paths(include_missing: bool) -> list[Path]:
    paths: list[Path] = []
    for manifest_name in ("fixture_sets.tsv", "discovered.tsv"):
        manifest = MANIFESTS / manifest_name
        if manifest.exists():
            paths.extend(manifest_czi_paths(manifest))
    paths.extend(local_czi_paths())
    resolved = sorted({REPO_ROOT / path if not path.is_absolute() else path for path in paths})
    if include_missing:
        return resolved
    return [path for path in resolved if path.exists()]


def print_targeted_manifest_summary() -> bool:
    rows = manifest_targeted_czi_rows(MANIFESTS / "fixture_sets.tsv")
    seen_sets = {row["set"] for row in rows}
    has_mosaic = any(TARGETED_CZI_SETS[fixture_set].startswith("mosaic") for fixture_set in seen_sets)
    for row in rows:
        local_path = REPO_ROOT / row["local_path"]
        target_kind = TARGETED_CZI_SETS[row["set"]]
        status = "present" if local_path.exists() else "missing"
        print(
            f"targeted_manifest\tset={row['set']}\tkind={target_kind}"
            f"\tbytes={row.get('size', '')}\tstatus={status}"
            f"\tpath={local_path}"
        )
    print(
        "targeted_manifest_summary"
        f"\tsets={','.join(sorted(seen_sets)) or 'none'}"
        f"\thas_mosaic={str(has_mosaic).lower()}"
    )
    return has_mosaic


def summarize(path: Path) -> tuple[str, bool, bool, bool, bool]:
    if not path.exists():
        return f"{path}\tmissing", False, False, False, False

    entries = read_entries(path)
    resolutions = Counter(entry.start("R") for entry in entries)
    pixel_types = Counter(entry.pixel_type for entry in entries)
    compressions = Counter(entry.compression for entry in entries)
    plane_entries: dict[tuple[int, int, int, int, int], list[Entry]] = defaultdict(list)
    nonzero_xy = 0
    bounds = defaultdict(lambda: [None, None, None, None])
    for entry in entries:
        plane_entries[entry.plane_key()].append(entry)
        if entry.start("X") != 0 or entry.start("Y") != 0:
            nonzero_xy += 1
        min_x, min_y, max_x, max_y = bounds[entry.start("R")]
        x0 = entry.start("X")
        y0 = entry.start("Y")
        x1 = x0 + entry.size("X")
        y1 = y0 + entry.size("Y")
        bounds[entry.start("R")] = [
            x0 if min_x is None else min(min_x, x0),
            y0 if min_y is None else min(min_y, y0),
            x1 if max_x is None else max(max_x, x1),
            y1 if max_y is None else max(max_y, y1),
        ]

    mosaic_planes = sum(1 for grouped in plane_entries.values() if len(grouped) > 1)
    single_subblock_planes = sum(1 for grouped in plane_entries.values() if len(grouped) == 1)
    plane_entry_counts = Counter(len(grouped) for grouped in plane_entries.values())
    reduced_xy_entries = sum(
        1
        for entry in entries
        if (entry.stored_size("X") > 0 and entry.stored_size("X") != entry.size("X"))
        or (entry.stored_size("Y") > 0 and entry.stored_size("Y") != entry.size("Y"))
    )
    r_dimension_pyramid = len(resolutions) > 1
    stored_size_pyramid = reduced_xy_entries > 0
    pyramid = r_dimension_pyramid or stored_size_pyramid
    mosaic = mosaic_planes > 0 and nonzero_xy > 0
    candidate_reasons = []
    if mosaic:
        candidate_reasons.append("mosaic")
    if pyramid:
        candidate_reasons.append("pyramid")
    pyramid_reasons = []
    if r_dimension_pyramid:
        pyramid_reasons.append("R-dimension")
    if stored_size_pyramid:
        pyramid_reasons.append("stored-size")
    smoke = (
        bool(entries)
        and not mosaic
        and not pyramid
        and nonzero_xy == 0
        and single_subblock_planes == len(plane_entries)
    )
    extents = ",".join(
        f"R{r}:{(max_x or 0) - (min_x or 0)}x{(max_y or 0) - (min_y or 0)}"
        for r, (min_x, min_y, max_x, max_y) in sorted(bounds.items())
    )
    origins = ",".join(
        f"R{r}:{min_x or 0},{min_y or 0}"
        for r, (min_x, min_y, _, _) in sorted(bounds.items())
    )
    status = "candidate" if mosaic or pyramid else "smoke-only"
    line = (
        f"{path}\t{status}\tentries={len(entries)}\tresolutions={dict(sorted(resolutions.items()))}"
        f"\tmosaic_planes={mosaic_planes}\tsingle_subblock_planes={single_subblock_planes}"
        f"\tplane_entry_counts={dict(sorted(plane_entry_counts.items()))}"
        f"\treduced_xy_entries={reduced_xy_entries}"
        f"\tnonzero_xy_entries={nonzero_xy}\tpixel_types={dict(sorted(pixel_types.items()))}"
        f"\tcompressions={dict(sorted(compressions.items()))}\textents={extents}\torigins={origins}"
        f"\tpyramid_reasons={','.join(pyramid_reasons) or 'none'}"
        f"\tcandidate_reasons={','.join(candidate_reasons) or 'none'}"
    )
    return line, mosaic or pyramid, smoke, mosaic, pyramid


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "paths",
        nargs="*",
        type=Path,
        help="CZI files to audit; defaults to manifested CZI local paths",
    )
    parser.add_argument(
        "--include-missing",
        action="store_true",
        help="also print manifested CZI paths that are not present locally",
    )
    parser.add_argument(
        "--require-candidate",
        action="store_true",
        help="exit non-zero unless a mosaic or pyramid candidate is found",
    )
    parser.add_argument(
        "--require-mosaic",
        action="store_true",
        help="exit non-zero unless a real multi-subblock mosaic candidate is found",
    )
    parser.add_argument(
        "--require-pyramid",
        action="store_true",
        help="exit non-zero unless a real multi-resolution pyramid candidate is found",
    )
    parser.add_argument(
        "--require-smoke",
        action="store_true",
        help="exit non-zero unless a real single-subblock smoke fixture is found",
    )
    parser.add_argument(
        "--targeted-manifest",
        action="store_true",
        help="print manifested targeted CZI mosaic candidate sets without requiring local files",
    )
    parser.add_argument(
        "--require-targeted-manifest",
        action="store_true",
        help="exit non-zero unless targeted CZI mosaic candidate sets are manifested",
    )
    args = parser.parse_args()

    paths = args.paths or default_paths(args.include_missing)
    any_candidate = False
    any_smoke = False
    any_mosaic = False
    any_pyramid = False
    manifest_has_mosaic = False
    if args.targeted_manifest or args.require_targeted_manifest:
        manifest_has_mosaic = print_targeted_manifest_summary()
    for path in paths:
        line, candidate, smoke, mosaic, pyramid = summarize(path)
        print(line)
        any_candidate = any_candidate or candidate
        any_smoke = any_smoke or smoke
        any_mosaic = any_mosaic or mosaic
        any_pyramid = any_pyramid or pyramid
    if args.require_candidate and not any_candidate:
        return 1
    if args.require_mosaic and not any_mosaic:
        return 1
    if args.require_pyramid and not any_pyramid:
        return 1
    if args.require_smoke and not any_smoke:
        return 1
    if args.require_targeted_manifest and not manifest_has_mosaic:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
