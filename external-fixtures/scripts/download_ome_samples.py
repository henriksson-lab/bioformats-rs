#!/usr/bin/env python3
"""Download public OME/Bio-Formats sample images into an ignored fixture tree."""

from __future__ import annotations

import argparse
import csv
import html.parser
import os
import posixpath
import sys
import time
import urllib.parse
import urllib.request
from dataclasses import dataclass, replace
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MANIFEST = ROOT / "manifests" / "ome_sample_roots.tsv"
DISCOVERED = ROOT / "manifests" / "discovered.tsv"
SETS = ROOT / "manifests" / "fixture_sets.tsv"
DATA = ROOT / "data"
DEFAULT_MAX_BYTES = 750_000_000
DEFAULT_MAX_TOTAL_BYTES = 2_000_000_000
DEFAULT_MAX_DISCOVER_FILES_PER_CATEGORY = 500
DEFAULT_SMOKE_MAX_BYTES = 250_000_000
DEFAULT_MAX_COMPANION_BYTES = DEFAULT_SMOKE_MAX_BYTES
OME_IMAGES_ROOT = "https://downloads.openmicroscopy.org/images/"
SKIP_FILENAMES = {"copying", "license", "readme", "readme.txt", "index.html"}
LARGE_SET_SUFFIXES = ("-large", "-regression-large")


@dataclass(frozen=True)
class Source:
    category: str
    root_url: str
    extensions: tuple[str, ...]
    notes: str


@dataclass(frozen=True)
class RemoteFile:
    category: str
    root_url: str
    url: str
    size: int | None
    required_companion: bool = False


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


def read_sources(path: Path) -> list[Source]:
    with path.open(newline="", encoding="utf-8") as handle:
        rows = csv.DictReader(handle, delimiter="\t")
        return [
            Source(
                category=row["category"],
                root_url=row["root_url"],
                extensions=tuple(ext.strip().lower() for ext in row["extensions"].split(",")),
                notes=row["notes"],
            )
            for row in rows
        ]


def normalize_category(name: str) -> str:
    aliases = {
        "Zeiss-CZI": "czi",
        "ND2": "nd2",
        "MRC": "mrc",
    }
    name = aliases.get(name, name)
    return name.strip("/").lower().replace("_", "-")


def refresh_roots() -> list[Source]:
    parser = IndexParser()
    parser.feed(fetch_text(OME_IMAGES_ROOT))
    sources = []
    for href in parser.links:
        if href in {"../", "/"} or href.startswith("?") or not href.endswith("/"):
            continue
        category = normalize_category(urllib.parse.unquote(href.strip("/")))
        if not category:
            continue
        sources.append(
            Source(
                category=category,
                root_url=urllib.parse.urljoin(OME_IMAGES_ROOT, href),
                extensions=("*",),
                notes="Auto-discovered public OME sample image category.",
            )
        )

    MANIFEST.parent.mkdir(parents=True, exist_ok=True)
    with MANIFEST.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle, delimiter="\t")
        writer.writerow(["category", "root_url", "extensions", "notes"])
        for source in sorted(sources, key=lambda item: item.category.lower()):
            writer.writerow([source.category, source.root_url, ",".join(source.extensions), source.notes])
    return sources


def read_set_files(path: Path, wanted_sets: set[str]) -> list[RemoteFile]:
    with path.open(newline="", encoding="utf-8") as handle:
        rows = csv.DictReader(handle, delimiter="\t")
        return [
            RemoteFile(
                category=row["category"],
                root_url=root_from_sample_url(row["url"]),
                url=row["url"],
                size=int(row["size"]) if row["size"] else None,
            )
            for row in rows
            if row["set"] in wanted_sets
        ]


def read_set_names(path: Path) -> list[str]:
    if not path.exists():
        return []
    with path.open(newline="", encoding="utf-8") as handle:
        rows = csv.DictReader(handle, delimiter="\t")
        return sorted({row["set"] for row in rows})


def parse_nrrd_data_files(text: str) -> list[str]:
    lines = text.splitlines()
    data_files: list[str] = []
    in_list = False
    for line in lines:
        stripped = line.strip()
        if not stripped or stripped.startswith("#"):
            if in_list and not stripped:
                break
            continue
        if in_list:
            data_files.append(stripped)
            continue
        key, sep, value = stripped.partition(":")
        if not sep:
            continue
        if key.strip().lower() not in {"data file", "datafile"}:
            continue
        value = value.strip()
        if value.upper() == "LIST":
            in_list = True
        elif value:
            data_files.append(value)
    return data_files


def nrrd_sidecar_urls(remote: RemoteFile) -> list[str]:
    if remote.category != "nrrd" or not urllib.parse.urlparse(remote.url).path.lower().endswith(".nhdr"):
        return []
    urls = []
    local_header = DATA / relative_remote_path(remote)
    if local_header.exists():
        text = local_header.read_text(encoding="utf-8", errors="replace")
    else:
        text = fetch_text(remote.url)
    for data_file in parse_nrrd_data_files(text):
        parsed = urllib.parse.urlparse(data_file)
        if parsed.scheme:
            urls.append(data_file)
        else:
            urls.append(urllib.parse.urljoin(remote.url, data_file))
    return urls


def with_bounded_required_companions(files: list[RemoteFile], max_companion_bytes: int) -> list[RemoteFile]:
    by_url = {remote.url: remote for remote in files}
    companion_urls: set[str] = set()
    for remote in files:
        for url in nrrd_sidecar_urls(remote):
            companion_urls.add(url)
            if url in by_url:
                continue
            size = fetch_size(url)
            if size is not None and size > max_companion_bytes:
                print(
                    f"skipping oversized companion {url}: {size} bytes > {max_companion_bytes}",
                    file=sys.stderr,
                )
                continue
            by_url[url] = RemoteFile(
                category=remote.category,
                root_url=remote.root_url,
                url=url,
                size=size,
                required_companion=True,
            )

    return [
        replace(remote, required_companion=True) if remote.url in companion_urls else remote
        for remote in by_url.values()
    ]


def category_root(category: str) -> str:
    for source in read_sources(MANIFEST):
        if source.category == category:
            return source.root_url
    raise ValueError(f"unknown fixture category {category!r}")


def root_from_sample_url(url: str) -> str:
    parsed = urllib.parse.urlparse(url)
    marker = "/images/"
    if marker not in parsed.path:
        return category_root(Path(parsed.path).parts[1])
    prefix, suffix = parsed.path.split(marker, 1)
    category = suffix.split("/", 1)[0]
    return urllib.parse.urlunparse((parsed.scheme, parsed.netloc, f"{prefix}{marker}{category}/", "", "", ""))


def fetch_text(url: str) -> str:
    request = urllib.request.Request(url, headers={"User-Agent": "bioformats-rs-fixture-fetcher"})
    with urllib.request.urlopen(request, timeout=60) as response:
        return response.read().decode("utf-8", errors="replace")


def fetch_size(url: str) -> int | None:
    request = urllib.request.Request(url, method="HEAD", headers={"User-Agent": "bioformats-rs-fixture-fetcher"})
    try:
        with urllib.request.urlopen(request, timeout=60) as response:
            value = response.headers.get("Content-Length")
            return int(value) if value else None
    except Exception:
        return None


def is_wanted_file(url: str, extensions: tuple[str, ...]) -> bool:
    path = urllib.parse.urlparse(url).path.lower()
    name = Path(urllib.parse.unquote(path)).name.lower()
    if name in SKIP_FILENAMES:
        return False
    if extensions == ("*",):
        return True
    return any(path.endswith(ext) for ext in extensions)


def discover(source: Source, max_files: int | None = DEFAULT_MAX_DISCOVER_FILES_PER_CATEGORY) -> list[RemoteFile]:
    seen_dirs: set[str] = set()
    files: list[RemoteFile] = []

    def walk(url: str) -> None:
        if url in seen_dirs:
            return
        if max_files is not None and len(files) >= max_files:
            return
        seen_dirs.add(url)
        parser = IndexParser()
        parser.feed(fetch_text(url))
        for href in parser.links:
            if href in {"../", "/"} or href.startswith("?"):
                continue
            child = urllib.parse.urljoin(url, href)
            parsed = urllib.parse.urlparse(child)
            if not parsed.scheme.startswith("http"):
                continue
            if not child.startswith(source.root_url):
                continue
            if child.endswith("/"):
                walk(child)
            elif is_wanted_file(child, source.extensions):
                files.append(RemoteFile(source.category, source.root_url, child, fetch_size(child)))
                if max_files is not None and len(files) >= max_files:
                    return

    walk(source.root_url)
    return files


def relative_remote_path(remote: RemoteFile) -> Path:
    root = urllib.parse.urlparse(remote.root_url).path
    path = urllib.parse.urlparse(remote.url).path
    rel = posixpath.relpath(path, root)
    host = urllib.parse.urlparse(remote.url).netloc
    return Path(remote.category) / host / Path(*rel.split("/"))


def write_discovered(files: list[RemoteFile]) -> None:
    DISCOVERED.parent.mkdir(parents=True, exist_ok=True)
    with DISCOVERED.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle, delimiter="\t")
        writer.writerow(["category", "size", "url", "local_path"])
        for remote in sorted(files, key=lambda item: (item.category, item.url)):
            local_path = Path("external-fixtures") / "data" / relative_remote_path(remote)
            writer.writerow([
                remote.category,
                "" if remote.size is None else remote.size,
                remote.url,
                str(local_path),
            ])


def read_discovered(path: Path) -> list[RemoteFile]:
    if not path.exists():
        return []
    with path.open(newline="", encoding="utf-8") as handle:
        rows = csv.DictReader(handle, delimiter="\t")
        return [
            RemoteFile(
                category=row["category"],
                root_url=root_from_sample_url(row["url"]),
                url=row["url"],
                size=int(row["size"]) if row["size"] else None,
            )
            for row in rows
        ]


def write_default_sets(files: list[RemoteFile]) -> None:
    SETS.parent.mkdir(parents=True, exist_ok=True)
    rows = []
    by_category: dict[str, list[RemoteFile]] = {}
    for remote in files:
        by_category.setdefault(remote.category, []).append(remote)

    for category, category_files in sorted(by_category.items(), key=lambda item: item[0].lower()):
        sized = sorted(
            category_files,
            key=lambda item: (item.size is None, item.size if item.size is not None else sys.maxsize, item.url),
        )
        smoke_candidates = [
            remote for remote in sized if remote.size is not None and remote.size <= DEFAULT_SMOKE_MAX_BYTES
        ]
        smoke = []
        smoke_total = 0
        for remote in smoke_candidates:
            if len(smoke) >= 3:
                break
            if smoke_total + (remote.size or 0) > DEFAULT_SMOKE_MAX_BYTES and smoke:
                break
            smoke.append(remote)
            smoke_total += remote.size or 0
        for remote in smoke:
            rows.append((f"{category}-smoke", remote, "Smallest public files for fast external smoke coverage."))

        medium = [
            remote
            for remote in sized
            if remote.size is not None and 50_000_000 <= remote.size <= DEFAULT_MAX_BYTES
        ][:5]
        for remote in medium:
            rows.append((f"{category}-feature", remote, "Moderate-size public files for broader format feature coverage."))

        large = [remote for remote in sized if remote.size is not None and remote.size > DEFAULT_MAX_BYTES][:3]
        for remote in large:
            rows.append((f"{category}-regression-large", remote, "Large public regression/stress files; not for routine CI."))

    with SETS.open("w", newline="", encoding="utf-8") as handle:
        writer = csv.writer(handle, delimiter="\t")
        writer.writerow(["set", "category", "size", "url", "local_path", "rationale"])
        for set_name, remote, rationale in rows:
            local_path = Path("external-fixtures") / "data" / relative_remote_path(remote)
            writer.writerow([
                set_name,
                remote.category,
                "" if remote.size is None else remote.size,
                remote.url,
                str(local_path),
                rationale,
            ])


def print_summary(files: list[RemoteFile], max_bytes: int) -> None:
    summary: dict[str, list[int]] = {}
    for remote in files:
        count, total, large = summary.setdefault(remote.category, [0, 0, 0])
        count += 1
        if remote.size is not None:
            total += remote.size
            if remote.size > max_bytes:
                large += 1
        summary[remote.category] = [count, total, large]
    print("category\tfiles\tbytes\tlarge_files")
    for category in sorted(summary):
        count, total, large = summary[category]
        print(f"{category}\t{count}\t{total}\t{large}")


def is_bounded_nrrd_sidecar_row(row: dict[str, str], max_companion_bytes: int) -> bool:
    local_path = row["local_path"].lower()
    size = int(row["size"]) if row["size"] else 0
    return (
        row["set"] == "nrrd-smoke"
        and row["category"] == "nrrd"
        and (local_path.endswith(".raw") or local_path.endswith(".raw.gz"))
        and size <= max_companion_bytes
    )


def validate_sets(path: Path, max_bytes: int, max_companion_bytes: int) -> list[str]:
    if not path.exists():
        return [f"missing set manifest {path}"]
    errors = []
    with path.open(newline="", encoding="utf-8") as handle:
        rows = csv.DictReader(handle, delimiter="\t")
        for row in rows:
            set_name = row["set"]
            if set_name.endswith(LARGE_SET_SUFFIXES):
                continue
            size = int(row["size"]) if row["size"] else 0
            if size > max_bytes and is_bounded_nrrd_sidecar_row(row, max_companion_bytes):
                continue
            if size > max_bytes:
                errors.append(f"{set_name}: {row['url']} is {size} bytes > {max_bytes}")
    return errors


def download(
    remote: RemoteFile,
    max_bytes: int,
    max_companion_bytes: int,
    include_large: bool,
    include_unknown_size: bool,
) -> str:
    if remote.size is None and not include_unknown_size:
        return "skip-unknown-size"
    max_allowed = max_companion_bytes if remote.required_companion else max_bytes
    if remote.size is not None and remote.size > max_allowed and not include_large:
        return "skip-large"
    dest = DATA / relative_remote_path(remote)
    if dest.exists() and remote.size is not None and dest.stat().st_size == remote.size:
        return "exists"
    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".part")
    request = urllib.request.Request(remote.url, headers={"User-Agent": "bioformats-rs-fixture-fetcher"})
    with urllib.request.urlopen(request, timeout=120) as response, tmp.open("wb") as handle:
        while True:
            chunk = response.read(1024 * 1024)
            if not chunk:
                break
            handle.write(chunk)
            if tmp.stat().st_size > max_allowed and not include_large:
                tmp.unlink(missing_ok=True)
                return "skip-large"
    os.replace(tmp, dest)
    return "downloaded"


def total_known_bytes(files: list[RemoteFile]) -> int:
    return sum(remote.size or 0 for remote in files)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--category", action="append", help="Limit to one or more source categories")
    parser.add_argument("--set", action="append", help="Download named fixture set from manifests/fixture_sets.tsv")
    parser.add_argument("--list-categories", action="store_true", help="List source categories in the root manifest")
    parser.add_argument("--list-sets", action="store_true", help="List named fixture sets")
    parser.add_argument("--refresh-roots", action="store_true", help="Refresh source roots from the OME images index")
    parser.add_argument("--write-default-sets", action="store_true", help="Write smoke/feature/large sets from discovery")
    parser.add_argument("--summary", action="store_true", help="Print discovery or set summary grouped by category")
    parser.add_argument("--plan", action="store_true", help="Print the selected download plan without downloading")
    parser.add_argument("--all-categories", action="store_true", help="Allow downloading all manifest categories")
    parser.add_argument("--validate-sets", action="store_true", help="Validate non-large sets against --max-bytes")
    parser.add_argument("--dry-run", action="store_true", help="Only discover files and write the manifest")
    parser.add_argument("--include-large", action="store_true", help="Download files larger than --max-bytes")
    parser.add_argument("--include-unknown-size", action="store_true", help="Download files whose remote size is unknown")
    parser.add_argument("--max-bytes", type=int, default=DEFAULT_MAX_BYTES)
    parser.add_argument("--max-companion-bytes", type=int, default=DEFAULT_MAX_COMPANION_BYTES)
    parser.add_argument("--max-total-bytes", type=int, default=DEFAULT_MAX_TOTAL_BYTES)
    parser.add_argument("--max-discover-files-per-category", type=int, default=DEFAULT_MAX_DISCOVER_FILES_PER_CATEGORY)
    args = parser.parse_args()

    if args.list_categories:
        for source in read_sources(MANIFEST):
            print(source.category)
        return 0
    if args.list_sets:
        for set_name in read_set_names(SETS):
            print(set_name)
        return 0
    if args.validate_sets:
        errors = validate_sets(SETS, args.max_bytes, args.max_companion_bytes)
        if errors:
            for error in errors:
                print(error, file=sys.stderr)
            return 1
        print(f"{SETS} is valid")
        return 0

    if args.set:
        known_sets = set(read_set_names(SETS))
        unknown_sets = sorted(set(args.set) - known_sets)
        if unknown_sets:
            print("unknown fixture set(s): " + ", ".join(unknown_sets), file=sys.stderr)
            return 2
        large_sets = [set_name for set_name in args.set if set_name.endswith(LARGE_SET_SUFFIXES)]
        if large_sets and not args.include_large and not args.dry_run and not args.plan:
            print(
                "refusing to download large set(s) without --include-large: " + ", ".join(large_sets),
                file=sys.stderr,
            )
            return 2
        files = read_set_files(SETS, set(args.set))
        files = with_bounded_required_companions(files, args.max_companion_bytes)
        manifest_message = f"loaded {len(files)} files from {SETS}"
    else:
        if not args.dry_run and not args.plan and not args.category and not args.all_categories:
            print("refusing broad download without --category, --set, or --all-categories", file=sys.stderr)
            return 2
        sources = refresh_roots() if args.refresh_roots else read_sources(MANIFEST)
        if args.category:
            wanted = set(args.category)
            known_categories = {source.category for source in sources}
            unknown_categories = sorted(wanted - known_categories)
            if unknown_categories:
                print("unknown source category/categories: " + ", ".join(unknown_categories), file=sys.stderr)
                return 2
            sources = [source for source in sources if source.category in wanted]

        files = []
        for source in sources:
            print(f"discovering {source.category}: {source.root_url}", file=sys.stderr)
            files.extend(discover(source, args.max_discover_files_per_category))
        files = with_bounded_required_companions(files, args.max_companion_bytes)
        write_discovered(files)
        if args.write_default_sets:
            write_default_sets(files)
        manifest_message = f"discovered {len(files)} files; wrote {DISCOVERED}"
        if args.write_default_sets:
            manifest_message += f"; wrote {SETS}"

    if args.write_default_sets and args.set:
        write_default_sets(read_discovered(DISCOVERED))

    if args.dry_run:
        print(manifest_message)
        if args.summary:
            print_summary(files, args.max_bytes)
        return 0

    if args.plan:
        print(manifest_message)
        print(f"files={len(files)} known_bytes={total_known_bytes(files)}")
        if args.summary:
            print_summary(files, args.max_bytes)
        return 0

    selected_bytes = total_known_bytes([
        remote
        for remote in files
        if (
            remote.size is None
            or args.include_large
            or remote.size <= (args.max_companion_bytes if remote.required_companion else args.max_bytes)
        )
    ])
    if selected_bytes > args.max_total_bytes and not args.include_large:
        print(
            f"refusing {selected_bytes} selected bytes above --max-total-bytes={args.max_total_bytes}; "
            "use a smaller set/category or --include-large",
            file=sys.stderr,
        )
        return 2

    counts: dict[str, int] = {}
    for remote in files:
        status = download(
            remote,
            args.max_bytes,
            args.max_companion_bytes,
            args.include_large,
            args.include_unknown_size,
        )
        counts[status] = counts.get(status, 0) + 1
        size = "unknown" if remote.size is None else str(remote.size)
        print(f"{status}\t{remote.category}\t{size}\t{remote.url}")
        time.sleep(0.1)
    print("summary:", " ".join(f"{key}={value}" for key, value in sorted(counts.items())))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
