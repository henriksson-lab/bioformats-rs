#!/usr/bin/env python3
"""Audit local MRC/CCP4/IMOD fixtures for orientation coverage."""

from __future__ import annotations

import argparse
import csv
import math
import sys
import struct
import zlib
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
REPO_ROOT = ROOT.parent
MANIFESTS = ROOT / "manifests"
HEADER_SIZE = 1024
IMOD_STAMP = 1146047817
MRC_EXTENSIONS = {".mrc", ".mrcs", ".map", ".ccp4", ".rec"}


# Public values asserted by ccpem/mrcfile for tests/test_data/EMD-3197.map.
# These validate stable storage-order sample bytes from a real EMDB map, but do
# not identify which stored Y edge is the logical top edge.
MRCFILE_STORAGE_SAMPLE_EXPECTATIONS = {
    "EMD-3197.map": {
        (0, 0, 0): -1.8013091,
        (9, 6, 13): 4.6207790,
        (9, 6, 14): 5.0373931,
        (19, 19, 19): 1.3078574,
    }
}

# Public IMOD fortIOtests evidence:
# * mrcDirTests/tifTests/tiftest verifies `tif2mrc tst0le.tif` matches the
#   `tst0.mrc` reference, tying the MRC row order to a TIFF top-left image.
# * fortIOtests/iotest verifies converting `tst0.ubyte` to signed bytes matches
#   `tst0.sbyte`, so the signed mode-0 copy inherits that orientation.
KNOWN_ORIENTATION_EXPECTATIONS = {
    "tst0.sbyte": {
        "mode": 0,
        "imod_stamp": IMOD_STAMP,
        "imod_flags_mask": 1,
        "stored_first_row_crc32": 0x518344F2,
        "stored_last_row_crc32": 0xFB1FABCB,
        "evidence": "imod-fortIOtests-tif2mrc-tst0le-and-sbyte-conversion",
    }
}


@dataclass(frozen=True)
class Header:
    nx: int
    ny: int
    nz: int
    mode: int
    nxstart: int
    nystart: int
    nzstart: int
    mx: int
    my: int
    mz: int
    xlen: float
    ylen: float
    zlen: float
    mapc: int
    mapr: int
    maps: int
    nsymbt: int
    origin_x: float
    origin_y: float
    origin_z: float
    map_id: str
    machst: int
    imod_stamp: int
    imod_flags: int
    little_endian: bool


def i32(data: bytes, offset: int, endian: str) -> int:
    return struct.unpack_from(endian + "i", data, offset)[0]


def u32(data: bytes, offset: int, endian: str) -> int:
    return struct.unpack_from(endian + "I", data, offset)[0]


def f32(data: bytes, offset: int, endian: str) -> float:
    return struct.unpack_from(endian + "f", data, offset)[0]


def plausible_mode(mode: int) -> bool:
    return mode in {0, 1, 2, 3, 4, 6, 16}


def plausible_dimension(value: int) -> bool:
    return 0 < value <= 1_000_000


def endian_score(data: bytes, endian: str) -> int:
    nx = i32(data, 0, endian)
    ny = i32(data, 4, endian)
    nz = i32(data, 8, endian)
    mode = i32(data, 12, endian)
    mapc = i32(data, 64, endian)
    mapr = i32(data, 68, endian)
    maps = i32(data, 72, endian)

    score = 0
    for dimension in (nx, ny, nz):
        if plausible_dimension(dimension):
            score += 2
    if plausible_mode(mode):
        score += 3
    if (mapc, mapr, maps) in {
        (1, 2, 3),
        (1, 3, 2),
        (2, 1, 3),
        (2, 3, 1),
        (3, 1, 2),
        (3, 2, 1),
    }:
        score += 2
    return score


def detect_little_endian(data: bytes) -> bool:
    le_score = endian_score(data, "<")
    be_score = endian_score(data, ">")
    if le_score != be_score:
        return le_score > be_score
    if data[212] == 0x11:
        return False
    if data[212] == 0x44:
        return True
    return True


def parse_header(data: bytes) -> Header:
    if len(data) < HEADER_SIZE:
        raise ValueError("MRC header too short")

    little_endian = detect_little_endian(data)
    endian = "<" if little_endian else ">"
    return Header(
        nx=i32(data, 0, endian),
        ny=i32(data, 4, endian),
        nz=i32(data, 8, endian),
        mode=i32(data, 12, endian),
        nxstart=i32(data, 16, endian),
        nystart=i32(data, 20, endian),
        nzstart=i32(data, 24, endian),
        mx=i32(data, 28, endian),
        my=i32(data, 32, endian),
        mz=i32(data, 36, endian),
        xlen=f32(data, 40, endian),
        ylen=f32(data, 44, endian),
        zlen=f32(data, 48, endian),
        mapc=i32(data, 64, endian),
        mapr=i32(data, 68, endian),
        maps=i32(data, 72, endian),
        nsymbt=i32(data, 92, endian),
        origin_x=f32(data, 196, endian),
        origin_y=f32(data, 200, endian),
        origin_z=f32(data, 204, endian),
        map_id=data[208:212].decode("ascii", errors="replace"),
        machst=data[212],
        imod_stamp=u32(data, 152, endian),
        imod_flags=i32(data, 156, endian),
        little_endian=little_endian,
    )


def valid_axis_permutation(hdr: Header) -> bool:
    return (hdr.mapc, hdr.mapr, hdr.maps) in {
        (1, 2, 3),
        (1, 3, 2),
        (2, 1, 3),
        (2, 3, 1),
        (3, 1, 2),
        (3, 2, 1),
    }


def has_explicit_origin(hdr: Header) -> bool:
    return hdr.origin_x != 0.0 or hdr.origin_y != 0.0 or hdr.origin_z != 0.0


def should_flip_y(hdr: Header) -> bool:
    if valid_axis_permutation(hdr) and hdr.mapr != 2:
        return False

    top_y = max(hdr.ny - 1, 0)
    if has_explicit_origin(hdr) and round(hdr.origin_y) >= top_y:
        return False
    if hdr.nystart >= top_y and top_y > 0:
        return False

    return True


def mode_name(hdr: Header) -> str:
    if hdr.mode == 0:
        if hdr.imod_stamp == IMOD_STAMP and (hdr.imod_flags & 1) != 0:
            return "int8"
        return "uint8"
    return {
        1: "int16",
        2: "float32",
        3: "complex-int16",
        4: "complex-float32",
        6: "uint16",
        16: "rgb-uint8",
    }.get(hdr.mode, f"unknown-{hdr.mode}")


def sample_format(hdr: Header) -> tuple[str, int] | None:
    endian = "<" if hdr.little_endian else ">"
    if hdr.mode == 0:
        sample_type = "b" if hdr.imod_stamp == IMOD_STAMP and (hdr.imod_flags & 1) != 0 else "B"
        return (sample_type, 1)
    if hdr.mode == 1:
        return (endian + "h", 2)
    if hdr.mode == 2:
        return (endian + "f", 4)
    if hdr.mode == 6:
        return (endian + "H", 2)
    return None


def finite_text(value: float | int) -> str:
    if isinstance(value, float):
        if math.isfinite(value):
            return f"{value:.6g}"
        return str(value)
    return str(value)


def first_sample(data: bytes, fmt: str) -> str:
    try:
        return finite_text(struct.unpack_from(fmt, data, 0)[0])
    except struct.error:
        return "unavailable"


def storage_sample(path: Path, hdr: Header, z: int, y: int, x: int) -> float | int | None:
    fmt_and_size = sample_format(hdr)
    if fmt_and_size is None:
        return None
    if not (0 <= x < hdr.nx and 0 <= y < hdr.ny and 0 <= z < hdr.nz):
        return None

    fmt, bytes_per_sample = fmt_and_size
    samples_per_pixel = 3 if hdr.mode == 16 else 1
    plane_bytes = hdr.nx * hdr.ny * samples_per_pixel * bytes_per_sample
    row_bytes = hdr.nx * samples_per_pixel * bytes_per_sample
    offset = (
        HEADER_SIZE
        + max(hdr.nsymbt, 0)
        + z * plane_bytes
        + y * row_bytes
        + x * samples_per_pixel * bytes_per_sample
    )
    with path.open("rb") as handle:
        handle.seek(offset)
        data = handle.read(bytes_per_sample)
    if len(data) != bytes_per_sample:
        return None
    try:
        return struct.unpack_from(fmt, data, 0)[0]
    except struct.error:
        return None


def upstream_storage_sample_summary(path: Path, hdr: Header) -> tuple[str, bool]:
    expected = MRCFILE_STORAGE_SAMPLE_EXPECTATIONS.get(path.name)
    if not expected:
        return "upstream_storage_samples=false", False

    parts = []
    all_match = True
    for (z, y, x), expected_value in expected.items():
        actual = storage_sample(path, hdr, z, y, x)
        if actual is None:
            all_match = False
            parts.append(f"mrcfile_sample_z{z}_y{y}_x{x}=unavailable")
            continue
        if isinstance(expected_value, float):
            matches = math.isclose(
                float(actual),
                expected_value,
                rel_tol=1e-6,
                abs_tol=1e-6,
            )
        else:
            matches = actual == expected_value
        all_match = all_match and matches
        parts.append(
            f"mrcfile_sample_z{z}_y{y}_x{x}={finite_text(actual)}"
            f"/expected={finite_text(expected_value)}"
            f"/match={str(matches).lower()}"
        )
    return (
        f"upstream_storage_samples={str(all_match).lower()}\t" + "\t".join(parts),
        all_match,
    )


@dataclass(frozen=True)
class RowEdgeSummary:
    text: str
    row_edges_distinguish: bool


def row_edge_samples(path: Path, hdr: Header) -> RowEdgeSummary:
    fmt_and_size = sample_format(hdr)
    if fmt_and_size is None or hdr.nx <= 0 or hdr.ny <= 0:
        return RowEdgeSummary("edge_samples=unsupported", False)

    fmt, bytes_per_sample = fmt_and_size
    samples_per_pixel = 3 if hdr.mode == 16 else 1
    row_bytes = hdr.nx * samples_per_pixel * bytes_per_sample
    data_offset = HEADER_SIZE + max(hdr.nsymbt, 0)
    bottom_offset = data_offset + max(hdr.ny - 1, 0) * row_bytes

    with path.open("rb") as handle:
        handle.seek(data_offset)
        stored_first_row = handle.read(row_bytes)
        handle.seek(bottom_offset)
        stored_last_row = handle.read(row_bytes)

    stored_first_sample = first_sample(stored_first_row[:bytes_per_sample], fmt)
    stored_last_sample = first_sample(stored_last_row[:bytes_per_sample], fmt)
    sample_edges_distinguish = (
        stored_first_sample != "unavailable"
        and stored_last_sample != "unavailable"
        and stored_first_sample != stored_last_sample
    )
    row_edges_distinguish = (
        len(stored_first_row) == row_bytes
        and len(stored_last_row) == row_bytes
        and stored_first_row != stored_last_row
    )
    stored_first_crc = zlib.crc32(stored_first_row) & 0xffffffff
    stored_last_crc = zlib.crc32(stored_last_row) & 0xffffffff
    return RowEdgeSummary(
        (
            f"stored_first_row_first_sample={stored_first_sample}"
            f"\tstored_last_row_first_sample={stored_last_sample}"
            f"\tedge_samples_distinguish_rows={str(sample_edges_distinguish).lower()}"
            f"\tstored_first_row_crc32=0x{stored_first_crc:08x}"
            f"\tstored_last_row_crc32=0x{stored_last_crc:08x}"
            f"\trow_edges_distinguish={str(row_edges_distinguish).lower()}"
            f"\tflip_expected_first_row_crc32=0x{stored_last_crc:08x}"
            f"\tflip_expected_last_row_crc32=0x{stored_first_crc:08x}"
        ),
        row_edges_distinguish,
    )


def known_orientation_evidence(path: Path, hdr: Header, row_edges: RowEdgeSummary) -> tuple[bool, str]:
    expected = KNOWN_ORIENTATION_EXPECTATIONS.get(path.name)
    if not expected:
        return False, "known_orientation_evidence=none"

    if hdr.mode != expected["mode"]:
        return False, f"known_orientation_evidence=mode-mismatch/expected={expected['mode']}"
    if hdr.imod_stamp != expected["imod_stamp"]:
        return False, (
            "known_orientation_evidence=imod-stamp-mismatch"
            f"/expected={expected['imod_stamp']}"
        )
    if (hdr.imod_flags & expected["imod_flags_mask"]) != expected["imod_flags_mask"]:
        return False, (
            "known_orientation_evidence=imod-flags-mismatch"
            f"/mask={expected['imod_flags_mask']}"
        )
    if not should_flip_y(hdr) or not row_edges.row_edges_distinguish:
        return False, "known_orientation_evidence=row-order-not-distinguishable"

    fmt_and_size = sample_format(hdr)
    if fmt_and_size is None:
        return False, "known_orientation_evidence=unsupported-sample-format"

    _fmt, bytes_per_sample = fmt_and_size
    samples_per_pixel = 3 if hdr.mode == 16 else 1
    row_bytes = hdr.nx * samples_per_pixel * bytes_per_sample
    data_offset = HEADER_SIZE + max(hdr.nsymbt, 0)
    bottom_offset = data_offset + max(hdr.ny - 1, 0) * row_bytes
    with path.open("rb") as handle:
        handle.seek(data_offset)
        stored_first_row = handle.read(row_bytes)
        handle.seek(bottom_offset)
        stored_last_row = handle.read(row_bytes)

    stored_first_crc = zlib.crc32(stored_first_row) & 0xFFFFFFFF
    stored_last_crc = zlib.crc32(stored_last_row) & 0xFFFFFFFF
    matches = (
        stored_first_crc == expected["stored_first_row_crc32"]
        and stored_last_crc == expected["stored_last_row_crc32"]
    )
    return matches, (
        f"known_orientation_evidence={expected['evidence']}"
        f"\tknown_orientation_expected_decoded_first_row_crc32=0x{stored_last_crc:08x}"
        f"\tknown_orientation_expected_decoded_last_row_crc32=0x{stored_first_crc:08x}"
        f"\tknown_orientation_crc_match={str(matches).lower()}"
    )


def manifest_mrc_paths(manifest: Path) -> list[Path]:
    paths: list[Path] = []
    with manifest.open(newline="", encoding="utf-8") as handle:
        for row in csv.DictReader(handle, delimiter="\t"):
            category = row.get("category", "")
            local_path = row.get("local_path", "")
            if category == "mrc" or Path(local_path).suffix.lower() in MRC_EXTENSIONS:
                paths.append(Path(local_path))
    return paths


def default_paths(include_missing: bool) -> list[Path]:
    paths: list[Path] = []
    for manifest_name in ("fixture_sets.tsv", "discovered.tsv"):
        manifest = MANIFESTS / manifest_name
        if manifest.exists():
            paths.extend(manifest_mrc_paths(manifest))
    resolved = sorted({REPO_ROOT / path if not path.is_absolute() else path for path in paths})
    if include_missing:
        return resolved
    return [path for path in resolved if path.exists()]


def coverage_tags(hdr: Header) -> list[str]:
    tags = []
    if should_flip_y(hdr):
        tags.append("flip-heuristic")
    else:
        tags.append("no-flip-heuristic")
    if valid_axis_permutation(hdr) and hdr.mapr != 2:
        tags.append("non-y-row-axis")
    if has_explicit_origin(hdr) and round(hdr.origin_y) >= max(hdr.ny - 1, 0):
        tags.append("top-origin")
    if hdr.nystart >= max(hdr.ny - 1, 0) and hdr.ny > 1:
        tags.append("top-start")
    if (hdr.mapc, hdr.mapr, hdr.maps) != (1, 2, 3):
        tags.append("non-default-axis-order")
    if hdr.imod_stamp == IMOD_STAMP:
        tags.append("imod-stamp")
    if hdr.mode == 0 and hdr.imod_stamp == IMOD_STAMP and (hdr.imod_flags & 1) != 0:
        tags.append("imod-signed-mode0")
    if not tags:
        tags.append("header-only")
    return tags


def summarize(path: Path) -> tuple[str, bool, set[str]]:
    if not path.exists():
        return f"{path}\tmissing", False, set()

    with path.open("rb") as handle:
        hdr = parse_header(handle.read(HEADER_SIZE))

    tags = coverage_tags(hdr)
    row_edges = row_edge_samples(path, hdr)
    known_output, known_orientation_text = known_orientation_evidence(path, hdr, row_edges)
    if known_output:
        tags.append("known-orientation")
    row_flip_byte_regression = should_flip_y(hdr) and row_edges.row_edges_distinguish
    if row_flip_byte_regression:
        tags.append("row-flip-byte-regression")
    coverage = set(tags)
    status = "known-output" if known_output else "header-only"
    storage_sample_text, upstream_storage_samples = upstream_storage_sample_summary(path, hdr)
    line = (
        f"{path}\t{status}\tcoverage={','.join(tags)}"
        f"\tknown_orientation={str(known_output).lower()}"
        f"\tknown_storage_samples={str(upstream_storage_samples).lower()}"
        f"\trow_flip_byte_regression={str(row_flip_byte_regression).lower()}"
        f"\tnon_default_axis={str('non-default-axis-order' in coverage).lower()}"
        f"\timod_signed_mode0={str('imod-signed-mode0' in coverage).lower()}"
        f"\tdims={hdr.nx}x{hdr.ny}x{hdr.nz}\tmode={hdr.mode}({mode_name(hdr)})"
        f"\tmap_axes={hdr.mapc}/{hdr.mapr}/{hdr.maps}"
        f"\tstarts={hdr.nxstart}/{hdr.nystart}/{hdr.nzstart}"
        f"\torigin={finite_text(hdr.origin_x)}/{finite_text(hdr.origin_y)}/{finite_text(hdr.origin_z)}"
        f"\tnsymbt={hdr.nsymbt}\tmap_id={hdr.map_id!r}"
        f"\timod_stamp={hdr.imod_stamp}\timod_flags={hdr.imod_flags}"
        f"\tflip_y={str(should_flip_y(hdr)).lower()}"
        f"\t{row_edges.text}"
        f"\t{known_orientation_text}"
        f"\t{storage_sample_text}"
    )
    return line, known_output, coverage


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "paths",
        nargs="*",
        type=Path,
        help="MRC-family files to audit; defaults to manifested MRC local paths",
    )
    parser.add_argument(
        "--include-missing",
        action="store_true",
        help="also print manifested MRC paths that are not present locally",
    )
    parser.add_argument(
        "--require-known-output",
        action="store_true",
        help="exit non-zero unless at least one known-output orientation fixture is found",
    )
    parser.add_argument(
        "--require-remaining-gaps",
        action="store_true",
        help=(
            "exit non-zero unless local fixtures cover known orientation, "
            "non-default axis order, and IMOD signed mode-0 evidence"
        ),
    )
    args = parser.parse_args()

    paths = args.paths or default_paths(args.include_missing)
    any_known_output = False
    any_non_default_axis = False
    any_imod_signed_mode0 = False
    for path in paths:
        line, known_output, coverage = summarize(path)
        print(line)
        any_known_output = any_known_output or known_output
        any_non_default_axis = any_non_default_axis or "non-default-axis-order" in coverage
        any_imod_signed_mode0 = any_imod_signed_mode0 or "imod-signed-mode0" in coverage
    if args.require_known_output and not any_known_output:
        return 1
    if args.require_remaining_gaps:
        missing = []
        if not any_known_output:
            missing.append("known-orientation")
        if not any_non_default_axis:
            missing.append("non-default-axis")
        if not any_imod_signed_mode0:
            missing.append("imod-signed-mode0")
        if missing:
            print(f"missing_remaining_mrc_gap_evidence={','.join(missing)}", file=sys.stderr)
            return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
