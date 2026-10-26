"""Reproduce Rust thumbnail golden hashes using the reference Bio-Formats JAR.

Run from the repository root: python3 tests/fixtures/thumbnail/generate.py
The Java helper also accepts --gray-table to reproduce the ICC conversion LUTs.
Generated binary inputs/outputs and compiled Java classes stay in .tmp.
"""
import pathlib
import struct
import subprocess

output = pathlib.Path(".tmp/thumbnail-oracle")
output.mkdir(parents=True, exist_ok=True)
source = pathlib.Path("tests/fixtures/thumbnail/ThumbnailOracle.java")
subprocess.run(["javac", "-cp", "bioformats_package.jar", "-d", str(output), str(source)], check=True)
cases = [
    ("u8", 256, 4, "uint8", 1, True),
    ("u16", 256, 4, "uint16", 1, True),
    ("f32", 256, 4, "float", 1, True),
    ("rgb8", 257, 129, "uint8", 3, True),
    ("rgb16", 257, 129, "uint16", 3, False),
    ("smallf", 4, 2, "float", 1, True),
    ("small16", 4, 2, "uint16", 1, True),
    ("rgba8", 256, 4, "uint8", 4, True),
    ("rgba16", 256, 4, "uint16", 4, True),
    ("rgb_i16", 256, 4, "int16", 3, False),
    ("f64", 256, 4, "double", 1, True),
    ("two8", 256, 4, "uint8", 2, True),
    ("two16", 256, 4, "uint16", 2, True),
    ("i8", 256, 4, "int8", 1, True),
    ("i16", 256, 4, "int16", 1, True),
    ("u32", 256, 4, "uint32", 1, True),
    ("i32", 256, 4, "int32", 1, True),
]
for name, width, height, pixel_type, channels, interleaved in cases:
    count = width * height * channels
    fmt = {"uint8": "B", "int8": "B", "uint16": "H", "int16": "H", "uint32": "I", "int32": "I", "float": "f", "double": "d"}[pixel_type]
    bps = struct.calcsize(fmt)
    if fmt in ("f", "d"):
        values = [(i % 257 - 12) * .3 for i in range(count)]
    else:
        factor = (47 if bps == 1 else 1003) if name in {"u8", "u16", "rgb8", "rgb16", "small16"} else 1003 if name in {"rgba8", "rgba16", "rgb_i16"} else 100003
        values = [(i * factor + 17) % 2 ** (bps * 8) for i in range(count)]
    raw = output / (name + ".raw")
    raw.write_bytes(struct.pack("<" + fmt * count, *values))
    thumb = output / (name + ".thumb")
    subprocess.run(["java", "-Djava.awt.headless=true", "-cp", "bioformats_package.jar:" + str(output), "ThumbnailOracle", str(thumb), str(width), str(height), pixel_type, str(channels), str(raw), str(interleaved).lower()], check=True)
    hash_value = 0xcbf29ce484222325
    for byte in thumb.read_bytes():
        hash_value = ((hash_value ^ byte) * 0x100000001b3) & 0xffffffffffffffff
    print(name, f"0x{hash_value:016x}")
