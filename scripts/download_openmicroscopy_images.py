#!/usr/bin/env python3
"""Download selected Open Microscopy sample-image folders.

The manifest keeps upstream folder names intact under userdata/openmicroscopy-images
so companion-file formats can be opened exactly as Bio-Formats expects.
"""

from __future__ import annotations

import argparse
import csv
import html.parser
import os
from pathlib import Path
import sys
import urllib.parse
import urllib.request
from collections import defaultdict


DEFAULT_MANIFEST = Path("external-fixtures/manifests/openmicroscopy_images.tsv")


class IndexParser(html.parser.HTMLParser):
    def __init__(self) -> None:
        super().__init__()
        self.links: list[str] = []

    def handle_starttag(self, tag: str, attrs: list[tuple[str, str | None]]) -> None:
        if tag.lower() != "a":
            return
        for key, value in attrs:
            if key.lower() == "href" and value:
                self.links.append(value)


def read_manifest(path: Path) -> list[dict[str, str]]:
    rows: list[dict[str, str]] = []
    with path.open(newline="") as f:
        reader = csv.DictReader(
            (line for line in f if line.strip() and not line.startswith("#")),
            delimiter="\t",
        )
        for row in reader:
            rows.append(row)
    return rows


def fetch_bytes(url: str) -> bytes:
    with urllib.request.urlopen(url) as response:
        return response.read()


def index_files(url: str) -> list[tuple[str, str]]:
    body = fetch_bytes(url).decode("utf-8", errors="replace")
    parser = IndexParser()
    parser.feed(body)
    files: list[tuple[str, str]] = []
    for href in parser.links:
        if href.startswith("../") or href.endswith("/"):
            continue
        file_url = urllib.parse.urljoin(url, href)
        name = Path(urllib.parse.unquote(urllib.parse.urlparse(href).path)).name
        if name:
            files.append((file_url, name))
    return files


def remote_size(url: str) -> int | None:
    request = urllib.request.Request(url, method="HEAD")
    try:
        with urllib.request.urlopen(request) as response:
            value = response.headers.get("Content-Length")
    except Exception:
        return None
    return int(value) if value and value.isdigit() else None


def plan(rows: list[dict[str, str]], groups: set[str] | None) -> list[tuple[str, str, Path]]:
    downloads: list[tuple[str, str, Path]] = []
    for row in rows:
        group = row["group"]
        if groups and group not in groups:
            continue
        url = row["url"]
        local_path = Path(row["local_path"])
        mode = row["mode"]
        if mode == "file":
            downloads.append((group, url, local_path))
        elif mode == "index":
            for file_url, name in index_files(url):
                downloads.append((group, file_url, local_path / name))
        else:
            raise ValueError(f"unknown mode for {group}: {mode}")
    return downloads


def download(url: str, dst: Path) -> None:
    dst.parent.mkdir(parents=True, exist_ok=True)
    tmp = dst.with_suffix(dst.suffix + ".part")
    headers = {}
    existing = tmp.stat().st_size if tmp.exists() else 0
    if existing:
        headers["Range"] = f"bytes={existing}-"
    request = urllib.request.Request(url, headers=headers)
    with urllib.request.urlopen(request) as response:
        mode = "ab" if existing and response.status == 206 else "wb"
        with tmp.open(mode) as out:
            while True:
                chunk = response.read(1024 * 1024)
                if not chunk:
                    break
                out.write(chunk)
    tmp.replace(dst)


def human_size(value: int | None) -> str:
    if value is None:
        return "unknown"
    size = float(value)
    for unit in ["B", "KiB", "MiB", "GiB", "TiB"]:
        if size < 1024.0:
            return f"{size:.1f} {unit}"
        size /= 1024.0
    return f"{size:.1f} PiB"


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--manifest", type=Path, default=DEFAULT_MANIFEST)
    parser.add_argument("--group", action="append", help="download only this manifest group")
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--verbose", action="store_true")
    parser.add_argument("--head-sizes", action="store_true", help="HEAD every file during planning")
    parser.add_argument("--skip-existing", action="store_true", default=True)
    args = parser.parse_args()

    groups = set(args.group) if args.group else None
    rows = read_manifest(args.manifest)
    downloads = plan(rows, groups)
    total = 0
    known = True
    counts: dict[str, int] = defaultdict(int)
    bytes_by_group: dict[str, int] = defaultdict(int)
    unknown_by_group: set[str] = set()
    for group, url, dst in downloads:
        counts[group] += 1
        size = remote_size(url) if args.head_sizes else None
        if size is None:
            known = False
            unknown_by_group.add(group)
        else:
            total += size
            bytes_by_group[group] += size
        if args.verbose:
            status = "exists" if dst.exists() else "download"
            print(f"{status}\t{group}\t{human_size(size)}\t{dst}\t{url}")
    for group in sorted(counts):
        suffix = "+" if group in unknown_by_group else ""
        print(f"group={group}\tfiles={counts[group]}\tbytes={human_size(bytes_by_group[group])}{suffix}")
    print(f"planned_files={len(downloads)}")
    print(f"planned_bytes={human_size(total) if known else human_size(total) + '+'}")
    if args.dry_run:
        return 0

    for group, url, dst in downloads:
        if args.skip_existing and dst.exists():
            continue
        print(f"downloading\t{group}\t{dst}", flush=True)
        download(url, dst)
    return 0


if __name__ == "__main__":
    sys.exit(main())
