#!/usr/bin/env python3
"""Generate deterministic OME-NGFF v0.4 (Zarr v2) samples for the Rust reader tests.

Writes:
  testdata/zarr/sample.ome.zarr  -- single multiscales image (t=1,c=2,z=3,y=32,x=32),
                                    2 resolution levels, uint16, omero channels + scale.
  testdata/zarr/multi.ome.zarr   -- bioformats2raw-style group with two sub-image
                                    multiscales groups "0" and "1" (multi-series).

The pixel value pattern is deterministic so the Rust test can assert bitwise:
    val(t,c,z,y,x,level) = (level*10000 + t*7 + c*1000 + z*100 + y*10 + x) & 0xFFFF
"""
import os
import shutil

import numpy as np
import zarr

HERE = os.path.dirname(os.path.abspath(__file__))
OUT = os.path.join(HERE, "..", "testdata", "zarr")


def pattern(shape, level):
    t, c, z, y, x = shape
    arr = np.zeros(shape, dtype=np.uint16)
    for tt in range(t):
        for cc in range(c):
            for zz in range(z):
                for yy in range(y):
                    for xx in range(x):
                        v = (level * 10000 + tt * 7 + cc * 1000 + zz * 100 + yy * 10 + xx) & 0xFFFF
                        arr[tt, cc, zz, yy, xx] = v
    return arr


def write_image_group(group, shapes, axes, datasets_paths, scales, omero=None):
    """Write a multiscales image into an open zarr group."""
    for level, (path, shape) in enumerate(zip(datasets_paths, shapes)):
        a = group.create_array(
            path, shape=shape, dtype="uint16", chunks=shape
        )
        a[:] = pattern(shape, level)
    multiscales = [
        {
            "version": "0.4",
            "name": "sample",
            "axes": axes,
            "datasets": [
                {
                    "path": p,
                    "coordinateTransformations": [
                        {"type": "scale", "scale": s}
                    ],
                }
                for p, s in zip(datasets_paths, scales)
            ],
        }
    ]
    group.attrs["multiscales"] = multiscales
    if omero is not None:
        group.attrs["omero"] = omero


def axes_tczyx():
    return [
        {"name": "t", "type": "time", "unit": "second"},
        {"name": "c", "type": "channel"},
        {"name": "z", "type": "space", "unit": "micrometer"},
        {"name": "y", "type": "space", "unit": "micrometer"},
        {"name": "x", "type": "space", "unit": "micrometer"},
    ]


def gen_single():
    path = os.path.join(OUT, "sample.ome.zarr")
    if os.path.exists(path):
        shutil.rmtree(path)
    g = zarr.open_group(path, mode="w", zarr_format=2)
    shapes = [(1, 2, 3, 32, 32), (1, 2, 3, 16, 16)]
    scales = [[1.0, 1.0, 2.0, 0.5, 0.5], [1.0, 1.0, 2.0, 1.0, 1.0]]
    omero = {
        "id": 1,
        "name": "sample",
        "version": "0.4",
        "channels": [
            {
                "active": True,
                "color": "0000FF",
                "label": "DAPI",
                "window": {"start": 0.0, "end": 4000.0, "min": 0.0, "max": 65535.0},
            },
            {
                "active": True,
                "color": "00FF00",
                "label": "GFP",
                "window": {"start": 0.0, "end": 3000.0, "min": 0.0, "max": 65535.0},
            },
        ],
        "rdefs": {"defaultT": 0, "defaultZ": 0, "model": "color"},
    }
    write_image_group(g, shapes, axes_tczyx(), ["0", "1"], scales, omero=omero)
    print("wrote", path)


def gen_multi():
    path = os.path.join(OUT, "multi.ome.zarr")
    if os.path.exists(path):
        shutil.rmtree(path)
    g = zarr.open_group(path, mode="w", zarr_format=2)
    g.attrs["bioformats2raw.layout"] = 3
    # Image 0
    g0 = g.create_group("0")
    write_image_group(
        g0,
        [(1, 1, 1, 8, 8)],
        axes_tczyx(),
        ["0"],
        [[1.0, 1.0, 1.0, 0.25, 0.25]],
    )
    # Image 1 (different size + 2 channels)
    g1 = g.create_group("1")
    write_image_group(
        g1,
        [(2, 2, 1, 4, 6)],
        axes_tczyx(),
        ["0"],
        [[1.0, 1.0, 1.0, 0.1, 0.1]],
    )
    print("wrote", path)


if __name__ == "__main__":
    os.makedirs(OUT, exist_ok=True)
    gen_single()
    gen_multi()
    print("done")
