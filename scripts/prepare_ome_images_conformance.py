#!/usr/bin/env python3
"""Build parity/perf manifests from a local Open Microscopy image tree."""

from __future__ import annotations

import argparse
import os
from pathlib import Path


PRIMARY_SUFFIXES = {
    ".am",
    ".avi",
    ".bmp",
    ".ch5",
    ".czi",
    ".dcimg",
    ".dcm",
    ".dm3",
    ".dm4",
    ".dv",
    ".flex",
    ".frm",
    ".h5",
    ".hed",
    ".htd",
    ".ics",
    ".ids",
    ".ims",
    ".jpg",
    ".jpeg",
    ".klb",
    ".lif",
    ".mrc",
    ".nd",
    ".nd2",
    ".ndpi",
    ".nii",
    ".nrrd",
    ".oib",
    ".oir",
    ".ome.tif",
    ".ome.tiff",
    ".pic",
    ".png",
    ".qptiff",
    ".r3d",
    ".scn",
    ".sdt",
    ".sif",
    ".spc",
    ".spe",
    ".stk",
    ".svs",
    ".tif",
    ".tiff",
    ".vms",
    ".vmu",
    ".vsi",
    ".xlef",
    ".xlif",
    ".xml",
    ".zvi",
}

SKIP_NAMES = {
    "copying",
    "license",
    "readme",
    "readme.txt",
}

SKIP_SUFFIXES = {
    ".doc",
    ".docx",
    ".gz",
    ".log",
    ".mat",
    ".pdf",
    ".raw",
    ".set",
    ".txt",
    ".zip",
}

SKIP_PATH_CONTAINS = {
    "Micro-Manager/1.4.22/sebastien/",
}


def image_suffix(path: Path) -> str:
    name = path.name.lower()
    if name.endswith(".ome.tiff"):
        return ".ome.tiff"
    if name.endswith(".ome.tif"):
        return ".ome.tif"
    if name.endswith(".nii.gz"):
        return ".nii.gz"
    return path.suffix.lower()


def is_primary_image(path: Path) -> bool:
    name = path.name.lower()
    stem = path.stem.lower()
    suffix = image_suffix(path)
    if name in SKIP_NAMES or stem in SKIP_NAMES:
        return False
    if suffix in SKIP_SUFFIXES:
        return False
    return suffix in PRIMARY_SUFFIXES


def format_name(root: Path, path: Path) -> str:
    try:
        return path.relative_to(root).parts[0]
    except (IndexError, ValueError):
        return "__root__"


FORMAT_SUFFIX_PRIORITY = {
    "Hamamatsu-VMS": [".vms", ".vmu"],
    "InCell3000": [".frm"],
    "Leica-XLEF": [".xlef", ".xlif"],
    "MetaXpress": [".htd"],
    "Metamorph": [".nd"],
    "Micro-Manager": [".ome.tif", ".ome.tiff"],
    "ScanR": [".xml"],
}

FORMAT_PATH_PRIORITY = {
    "Micro-Manager": ["1.4.23/jens"],
}


def candidate_sort_key(root: Path, path: Path):
    fmt = format_name(root, path)
    suffix = image_suffix(path)
    priority = FORMAT_SUFFIX_PRIORITY.get(fmt, [])
    try:
        suffix_rank = priority.index(suffix)
    except ValueError:
        suffix_rank = len(priority)
    rel = path.relative_to(root).as_posix()
    path_priority = FORMAT_PATH_PRIORITY.get(fmt, [])
    path_rank = next(
        (rank for rank, needle in enumerate(path_priority) if needle in rel),
        len(path_priority),
    )
    return (fmt, path_rank, suffix_rank, str(path).lower())


def iter_candidates(root: Path):
    candidates = []
    for dirpath, dirnames, filenames in os.walk(root):
        dirnames[:] = sorted(
            dirname
            for dirname in dirnames
            if not dirname.startswith(".") and dirname not in {"__MACOSX"}
        )
        for filename in sorted(filenames):
            path = Path(dirpath) / filename
            rel = path.relative_to(root).as_posix()
            if any(needle in rel for needle in SKIP_PATH_CONTAINS):
                continue
            if is_primary_image(path):
                candidates.append(path)
    yield from sorted(candidates, key=lambda path: candidate_sort_key(root, path))



def main() -> int:
    parser = argparse.ArgumentParser(
        description="Create manifests for parity and speed/RSS tests over /big/henriksson/ome_images."
    )
    parser.add_argument("--root", default="/big/henriksson/ome_images")
    parser.add_argument(
        "--tsv",
        default="external-fixtures/manifests/ome_images_pending.tsv",
        help="human-readable TSV manifest with format, path, status, size_bytes, suffix",
    )
    parser.add_argument(
        "--paths",
        default="bench/target/ome-images-pending.paths",
        help="newline path manifest consumed by java parity and compare_subset.sh",
    )
    parser.add_argument(
        "--per-format",
        type=int,
        default=2,
        help="number of files to select per top-level format folder",
    )
    parser.add_argument(
        "--all",
        action="store_true",
        help="include every recognized image file instead of sampling per format",
    )
    args = parser.parse_args()

    root = Path(args.root)
    if not root.exists():
        raise SystemExit(f"root not found: {root}")

    chosen: list[Path] = []
    counts: dict[str, int] = {}
    for path in iter_candidates(root):
        fmt = format_name(root, path)
        if not args.all:
            count = counts.get(fmt, 0)
            if count >= args.per_format:
                continue
            counts[fmt] = count + 1
        chosen.append(path)

    tsv_path = Path(args.tsv)
    paths_path = Path(args.paths)
    tsv_path.parent.mkdir(parents=True, exist_ok=True)
    paths_path.parent.mkdir(parents=True, exist_ok=True)

    with tsv_path.open("w", encoding="utf-8") as handle:
        handle.write("format\tpath\tstatus\tsize_bytes\tsuffix\n")
        for path in chosen:
            try:
                size = path.stat().st_size
            except OSError:
                size = 0
            handle.write(
                f"{format_name(root, path)}\t{path}\tpresent\t{size}\t{image_suffix(path)}\n"
            )

    with paths_path.open("w", encoding="utf-8") as handle:
        for path in chosen:
            handle.write(f"{path}\n")

    print(f"selected {len(chosen)} files from {root}")
    print(f"tsv:   {tsv_path}")
    print(f"paths: {paths_path}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
