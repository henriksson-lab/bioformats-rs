#!/usr/bin/env python3
"""Audit local ND2 fixtures for coverage-relevant chunk features."""

from __future__ import annotations

import argparse
import re
from dataclasses import dataclass
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
DEFAULT_DATA = ROOT / "data" / "nd2"
ND2_MAGIC = b"\xda\xce\xbe\x0a"
CHUNK_MAP_SIGNATURE = b"ND2 CHUNK MAP SIGNATURE 0000001"


@dataclass(frozen=True)
class Chunk:
    name: str
    data_offset: int
    data_length: int


@dataclass(frozen=True)
class GapFlags:
    candidate: bool
    chunked_candidate: bool
    chunked_metadata_candidate: bool
    chunked_uicomp_candidate: bool
    chunked_jpeg2000_candidate: bool


def scan_chunks(path: Path) -> list[Chunk]:
    data = path.read_bytes()
    chunks: list[Chunk] = []
    pos = 0
    while pos + 16 <= len(data):
        found = data.find(ND2_MAGIC, pos)
        if found < 0 or found + 16 > len(data):
            break
        name_len = int.from_bytes(data[found + 4 : found + 8], "little")
        data_len = int.from_bytes(data[found + 8 : found + 16], "little")
        data_offset = found + 16 + name_len
        data_end = data_offset + data_len
        if name_len == 0 or name_len > 4096 or data_end > len(data):
            pos = found + 1
            continue
        name_bytes = data[found + 16 : data_offset]
        name = name_bytes.rstrip(b"\0").decode("utf-8", errors="replace")
        if not name.endswith("!"):
            pos = found + 1
            continue
        chunks.append(Chunk(name=name, data_offset=data_offset, data_length=data_len))
        pos = data_end
    return chunks


def read_chunk_map(path: Path) -> list[Chunk] | None:
    data = path.read_bytes()
    file_len = len(data)
    if file_len < 40:
        return None

    footer = data[file_len - 40 :]
    if not footer.startswith(CHUNK_MAP_SIGNATURE):
        return None
    map_offset = int.from_bytes(footer[len(CHUNK_MAP_SIGNATURE) + 1 :], "little")
    if map_offset + 16 > file_len or data[map_offset : map_offset + 4] != ND2_MAGIC:
        return None

    name_len = int.from_bytes(data[map_offset + 4 : map_offset + 8], "little")
    map_len = int.from_bytes(data[map_offset + 8 : map_offset + 16], "little")
    entries_offset = map_offset + 16 + name_len
    entries_end = entries_offset + map_len
    if entries_offset > file_len or entries_end > file_len:
        return None

    chunks: list[Chunk] = []
    image_indexes: list[int] = []
    cursor = entries_offset
    while cursor + 17 <= entries_end:
        bang = data.find(b"!", cursor, entries_end)
        if bang < 0 or bang + 17 > entries_end:
            return None
        name_bytes = data[cursor:bang]
        if name_bytes == CHUNK_MAP_SIGNATURE:
            break
        position = int.from_bytes(data[bang + 1 : bang + 9], "little")
        if position + 16 > file_len or data[position : position + 4] != ND2_MAGIC:
            return None
        actual_name_len = int.from_bytes(data[position + 4 : position + 8], "little")
        actual_data_len = int.from_bytes(data[position + 8 : position + 16], "little")
        data_offset = position + 16 + actual_name_len
        if data_offset > file_len or data_offset + actual_data_len > file_len:
            return None
        name = name_bytes.decode("utf-8", errors="replace") + "!"
        if (index := image_index(name, "ImageDataSeq|")) is not None:
            image_indexes.append(index)
        chunks.append(Chunk(name=name, data_offset=data_offset, data_length=actual_data_len))
        cursor = bang + 17

    if image_indexes and len(image_indexes) != max(image_indexes) + 1:
        return None

    return sorted(chunks, key=lambda chunk: chunk.data_offset)


def chunk_payload(path: Path, chunk: Chunk, limit: int | None = None) -> bytes:
    with path.open("rb") as handle:
        handle.seek(chunk.data_offset)
        return handle.read(chunk.data_length if limit is None else min(limit, chunk.data_length))


def xml_values(text: str, tag: str) -> list[str]:
    values: list[str] = []
    for match in re.finditer(rf"<{re.escape(tag)}\b([^>]*)>(.*?)</{re.escape(tag)}>", text, re.S):
        attrs, body = match.groups()
        attr = re.search(r'value="([^"]*)"', attrs)
        values.append((attr.group(1) if attr else body.strip()))
    for match in re.finditer(rf"<{re.escape(tag)}\b([^>]*)/>", text, re.S):
        attr = re.search(r'value="([^"]*)"', match.group(1))
        if attr:
            values.append(attr.group(1))
    return values


def image_index(name: str, prefix: str) -> int | None:
    if not name.startswith(prefix):
        return None
    suffix = name[len(prefix) :].rstrip("!")
    try:
        return int(suffix)
    except ValueError:
        return None


def sequence_index(name: str, stem: str) -> int | None:
    match = re.match(rf"^{re.escape(stem)}(?:LV)?\|(\d+)!$", name)
    if not match:
        return None
    return int(match.group(1))


def looks_like_zlib(data: bytes) -> bool:
    if len(data) < 2:
        return False
    cmf, flg = data[0], data[1]
    return (cmf & 0x0F) == 8 and ((cmf << 8) | flg) % 31 == 0


def looks_like_jpeg2000(data: bytes) -> bool:
    return data.startswith(b"\xff\x4f\xff\x51") or data.startswith(b"\0\0\0\x0cjP  ")


def file_header_kind(path: Path) -> str:
    header = path.read_bytes()[:8]
    if header.startswith(ND2_MAGIC):
        return "modern-nd2"
    if looks_like_jpeg2000(header):
        return "jpeg2000"
    return "unknown"


def has_old_nd_box_footer(path: Path) -> bool:
    marker = b"LABORATORY IMAGING ND BOX MAP 00"
    with path.open("rb") as handle:
        handle.seek(0, 2)
        file_len = handle.tell()
        handle.seek(max(0, file_len - 4096))
        return marker in handle.read()


def payload_kind(data: bytes) -> str:
    for offset in (0, 8, 4096):
        payload = data[offset:]
        if looks_like_jpeg2000(payload):
            return f"jpeg2000@{offset}"
        if looks_like_zlib(payload):
            return f"zlib@{offset}"
    return "raw-or-other"


def scan_old_jp2_boxes(path: Path) -> tuple[int, str]:
    data = path.read_bytes()
    pos = 0
    codestreams = 0
    dimensions: list[str] = []
    while pos + 8 <= len(data):
        box_len = int.from_bytes(data[pos : pos + 4], "big")
        box_type = data[pos + 4 : pos + 8]
        next_pos = pos + box_len
        if box_len < 8 or next_pos > len(data):
            break
        if box_type == b"jp2c":
            codestreams += 1
        elif box_type == b"jp2h":
            sub = pos + 8
            while sub + 8 <= next_pos:
                sub_len = int.from_bytes(data[sub : sub + 4], "big")
                sub_type = data[sub + 4 : sub + 8]
                sub_next = sub + sub_len
                if sub_len < 8 or sub_next > next_pos:
                    break
                if sub_type == b"ihdr" and sub_len >= 22:
                    size_y = int.from_bytes(data[sub + 8 : sub + 12], "big")
                    size_x = int.from_bytes(data[sub + 12 : sub + 16], "big")
                    bands = int.from_bytes(data[sub + 16 : sub + 18], "big")
                    pixel_type = int.from_bytes(data[sub + 18 : sub + 22], "big")
                    dimensions.append(f"{size_x}x{size_y}x{bands}/0x{pixel_type:08x}")
                sub = sub_next
        pos = next_pos
    return codestreams, ",".join(dimensions) or "none"


def scan_text_metadata(path: Path) -> tuple[list[int], set[str], set[str]]:
    text = path.read_bytes().decode("utf-8", errors="ignore")
    metadata_indexes = sorted(
        {
            int(match.group(1))
            for match in re.finditer(r"<MetadataSeq\b[^>]*\b_SEQUENCE_INDEX=\"(\d+)\"", text)
        }
    )
    return metadata_indexes, set(xml_values(text, "uiComp")), set(xml_values(text, "uiCompCount"))


def audit_file(path: Path) -> tuple[str, GapFlags]:
    header_kind = file_header_kind(path)
    old_nd_box_footer = has_old_nd_box_footer(path)
    old_jp2_codestreams, old_jp2_ihdr = scan_old_jp2_boxes(path)
    chunks = read_chunk_map(path) or scan_chunks(path)
    image_chunks = [chunk for chunk in chunks if chunk.name.startswith("ImageDataSeq")]
    metadata_indexes = sorted(
        {
            idx
            for chunk in chunks
            if (idx := sequence_index(chunk.name, "ImageMetadataSeq")) is not None
        }
    )
    attributes_indexes = sorted(
        {
            idx
            for chunk in chunks
            if (idx := sequence_index(chunk.name, "ImageAttributesSeq")) is not None
        }
    )

    ui_comp: set[str] = set()
    ui_comp_count: set[str] = set()
    for chunk in chunks:
        if "Attributes" not in chunk.name and "Metadata" not in chunk.name:
            continue
        text = chunk_payload(path, chunk, limit=512 * 1024).decode("utf-8", errors="ignore")
        ui_comp.update(xml_values(text, "uiComp"))
        ui_comp_count.update(xml_values(text, "uiCompCount"))

    if not image_chunks:
        text_metadata_indexes, text_ui_comp, text_ui_comp_count = scan_text_metadata(path)
        metadata_indexes = sorted(set(metadata_indexes).union(text_metadata_indexes))
        ui_comp.update(text_ui_comp)
        ui_comp_count.update(text_ui_comp_count)

    kinds: dict[str, int] = {}
    for chunk in image_chunks:
        kind = payload_kind(chunk_payload(path, chunk, limit=4104))
        kinds[kind] = kinds.get(kind, 0) + 1

    has_component_candidate = any(value not in {"", "1", "4294967295"} for value in ui_comp)
    has_metadata_candidate = any(index > 0 for index in metadata_indexes)
    has_attributes_candidate = any(index > 0 for index in attributes_indexes)
    has_jpeg2000_candidate = any(kind.startswith("jpeg2000") for kind in kinds)
    has_component_count_candidate = any(
        value not in {"", "1", "4294967295"} for value in ui_comp_count
    ) and (old_nd_box_footer or has_metadata_candidate)
    chunked_uicomp_candidate = bool(image_chunks and has_component_candidate)
    chunked_metadata_candidate = bool(
        image_chunks and (has_metadata_candidate or has_attributes_candidate)
    )
    chunked_jpeg2000_candidate = bool(image_chunks and has_jpeg2000_candidate)

    chunked_candidate_reasons: list[str] = []
    if chunked_uicomp_candidate:
        chunked_candidate_reasons.append("uiComp")
    if image_chunks and has_metadata_candidate:
        chunked_candidate_reasons.append("metadata_seq")
    if image_chunks and has_attributes_candidate:
        chunked_candidate_reasons.append("attributes_seq")
    if chunked_jpeg2000_candidate:
        chunked_candidate_reasons.append("jpeg2000_payload")
    candidate_reasons: list[str] = []
    if has_component_candidate:
        candidate_reasons.append("uiComp")
    if has_component_count_candidate:
        candidate_reasons.append("uiCompCount")
    if has_metadata_candidate:
        candidate_reasons.append("metadata_seq")
    if has_attributes_candidate:
        candidate_reasons.append("attributes_seq")
    if has_jpeg2000_candidate:
        candidate_reasons.append("jpeg2000_payload")
    if old_nd_box_footer and old_jp2_codestreams:
        candidate_reasons.append("old_jp2_nd2")
    candidate = bool(candidate_reasons)

    status = "candidate" if candidate else "no-gap-match"
    details = [
        f"header={header_kind}",
        f"old_nd_box_footer={str(old_nd_box_footer).lower()}",
        f"old_jp2_codestreams={old_jp2_codestreams}",
        f"old_jp2_ihdr={old_jp2_ihdr}",
        f"images={len(image_chunks)}",
        f"metadata_seq={','.join(map(str, metadata_indexes)) or 'none'}",
        f"attributes_seq={','.join(map(str, attributes_indexes)) or 'none'}",
        f"uiComp={','.join(sorted(ui_comp)) or 'none'}",
        f"uiCompCount={','.join(sorted(ui_comp_count)) or 'none'}",
        f"candidate_reasons={','.join(candidate_reasons) or 'none'}",
        f"chunked_candidate_reasons={','.join(chunked_candidate_reasons) or 'none'}",
        f"chunked_metadata_candidate={str(chunked_metadata_candidate).lower()}",
        f"chunked_uicomp_candidate={str(chunked_uicomp_candidate).lower()}",
        f"chunked_jpeg2000_candidate={str(chunked_jpeg2000_candidate).lower()}",
        "payloads=" + ",".join(f"{key}:{value}" for key, value in sorted(kinds.items())),
    ]
    flags = GapFlags(
        candidate=candidate,
        chunked_candidate=bool(chunked_candidate_reasons),
        chunked_metadata_candidate=chunked_metadata_candidate,
        chunked_uicomp_candidate=chunked_uicomp_candidate,
        chunked_jpeg2000_candidate=chunked_jpeg2000_candidate,
    )
    return f"{path}\t{status}\t" + "\t".join(details), flags


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("paths", nargs="*", type=Path, help="ND2 files or directories to audit")
    parser.add_argument("--require-candidate", action="store_true")
    parser.add_argument("--require-chunked-candidate", action="store_true")
    parser.add_argument("--require-chunked-metadata-candidate", action="store_true")
    parser.add_argument("--require-chunked-uicomp-candidate", action="store_true")
    parser.add_argument("--require-chunked-jpeg2000-candidate", action="store_true")
    args = parser.parse_args()

    inputs = args.paths or [DEFAULT_DATA]
    files: list[Path] = []
    for item in inputs:
        if item.is_dir():
            files.extend(sorted(item.rglob("*.nd2")))
        elif item.suffix.lower() == ".nd2":
            files.append(item)

    found_candidate = False
    found_chunked_candidate = False
    found_chunked_metadata_candidate = False
    found_chunked_uicomp_candidate = False
    found_chunked_jpeg2000_candidate = False
    for path in sorted(set(files)):
        line, flags = audit_file(path)
        found_candidate = found_candidate or flags.candidate
        found_chunked_candidate = found_chunked_candidate or flags.chunked_candidate
        found_chunked_metadata_candidate = (
            found_chunked_metadata_candidate or flags.chunked_metadata_candidate
        )
        found_chunked_uicomp_candidate = (
            found_chunked_uicomp_candidate or flags.chunked_uicomp_candidate
        )
        found_chunked_jpeg2000_candidate = (
            found_chunked_jpeg2000_candidate or flags.chunked_jpeg2000_candidate
        )
        print(line)

    if args.require_candidate and not found_candidate:
        return 1
    if args.require_chunked_candidate and not found_chunked_candidate:
        return 1
    if args.require_chunked_metadata_candidate and not found_chunked_metadata_candidate:
        return 1
    if args.require_chunked_uicomp_candidate and not found_chunked_uicomp_candidate:
        return 1
    if args.require_chunked_jpeg2000_candidate and not found_chunked_jpeg2000_candidate:
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
