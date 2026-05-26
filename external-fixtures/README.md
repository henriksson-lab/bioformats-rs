# External Bio-Formats Fixtures

This directory tracks download manifests and scripts for public OME/Bio-Formats
sample images that are too large to check into git.

Downloaded data is written under `external-fixtures/data/`, which is ignored by
git. The directory layout mirrors the fixture category and remote source:

```text
external-fixtures/data/
  czi/
    downloads.openmicroscopy.org/...
  nd2/
    downloads.openmicroscopy.org/...
  mrc/
    downloads.openmicroscopy.org/...
```

Use dry-run mode first to inspect the current remote index:

```bash
python3 external-fixtures/scripts/download_ome_samples.py --dry-run
```

Download a bounded subset:

```bash
python3 external-fixtures/scripts/download_ome_samples.py --category mrc
python3 external-fixtures/scripts/download_ome_samples.py --category czi --max-bytes 60000000
python3 external-fixtures/scripts/download_ome_samples.py --category nd2 --max-bytes 250000000
```

Prefer named sets for tests. They keep routine downloads small while preserving
larger regression data in the manifest:

```bash
python3 external-fixtures/scripts/download_ome_samples.py --set mrc-smoke
python3 external-fixtures/scripts/download_ome_samples.py --set mrc-emdb-small
python3 external-fixtures/scripts/download_ome_samples.py --set czi-smoke
python3 external-fixtures/scripts/download_ome_samples.py --set czi-openslide-zeiss5-smoke
python3 external-fixtures/scripts/download_ome_samples.py --set czi-openslide-zeiss5-pyramid
python3 external-fixtures/scripts/download_ome_samples.py --set czi-synthetic-tile-smoke
python3 external-fixtures/scripts/download_ome_samples.py --set nd2-smoke
python3 external-fixtures/scripts/download_ome_samples.py --set nd2-zenodo-vpa002-smoke
python3 external-fixtures/scripts/download_ome_samples.py --set nd2-modern-uicomp-feature
python3 external-fixtures/scripts/download_ome_samples.py --set nd2-feature
```

Set names are tiered by intended use:

| suffix | use |
| --- | --- |
| `-smoke` | Bounded public samples for quick optional coverage. |
| `-feature` | Larger format coverage that should be downloaded deliberately. |
| `-regression-large` | Stress/regression data; do not use in routine CI. |
| targeted names | Curated fixes for cases where the automatic smallest-file set picked sidecars or unrelated files. |

Prefer targeted readable sets for format smoke tests when they exist, for
example `czi-openslide-zeiss5-smoke`, `czi-synthetic-tile-smoke`,
`mrc-emdb-small`, `dcimg-pixel-smoke`, `cv7000-structured-smoke`,
`hamamatsu-ndpi-image-smoke`, `perkinelmer-operetta-index-smoke`, and
`scanr-image-smoke`.

MRC sets include `mrc-smoke` for the public OME mirror sample,
`mrc-emdb-small` for the bounded `EMD-3197.map` candidate,
`mrc-emdb-feature` for the larger bounded `EMD-3001.map` candidate, and
`mrc-imod-signed-mode0` for the tiny public IMOD fortIOtests `tst0.sbyte`
signed-byte fixture. Keep broad EPU/IMOD archives out of routine sets unless a
focused, bounded payload is identified. The public IMOD sample archive
`http://bio3d.colorado.edu/imod/files/imod_data.tar.gz` has been probed as a
non-routine candidate source: extracted `golgi.mrc` and `dual.rec` provide
legacy mode-0 row-flip byte-regression evidence, but neither has IMOD
signedness flags or documented expected orientation pixels.

CZI sets include `czi-smoke` for the public OME mirror samples,
`czi-openslide-zeiss5-smoke` for the OpenSlide Zeiss-5 Flat JPEG XR mosaic
candidate, and `czi-openslide-zeiss5-feature` for the Zeiss-5 JXR/Cropped
JPEG XR mosaic candidates. `czi-openslide-zeiss5-pyramid` reuses the bounded
CC0 Zeiss-5 JXR/Cropped files for real JPEG XR pyramid coverage where reduced
stored X/Y sizes encode the pyramid rather than explicit CZI `R` levels. Use
`czi-synthetic-tile-smoke` for the small Zenodo
artificial 2x2 tile file, `czi-synthetic-tile-feature` for the W96 5x9 tile
file, and `czi-synthetic-tile-regression-large` only for deliberate tiled
stress coverage.

Nikon RAW candidates from raw.pixls are tracked as targeted sets instead of
part of OME discovery: use `nikon-nef-d70-compression-34713-smoke` for the
primary D70 NEF TIFF Compression 34713 case, with
`nikon-nef-d40-d50-alternates-smoke` and `nikon-nrw-p7000-smoke` kept as
optional alternates.

Large stress sets are intentionally separate:

```bash
python3 external-fixtures/scripts/download_ome_samples.py --set czi-synthetic-tile-regression-large --include-large
python3 external-fixtures/scripts/download_ome_samples.py --set nd2-regression-large --include-large
```

ND2 external candidates:

- `nd2-zenodo-vpa002-smoke` tracks the public Zenodo record
  `10.5281/zenodo.8161776` file `2D 500uM VPA002.nd2`; it has a direct
  per-file download URL and is suitable for bounded targeted coverage.
- `nd2-modern-uicomp-feature` tracks the public OME sample
  `100217_OD122_001.nd2`; range and full-file audits show it is a modern
  chunked `ImageDataSeq` fixture with `uiComp=2`, raw payloads, and no
  per-plane metadata or JPEG2000 payload evidence.
- The tlambert `nd2` sample-data source is intentionally not listed as
  individual fixture rows unless the archive entries are resolved to stable
  per-file URLs; repository/script archive entries are not direct downloader
  inputs.
- Dropbox or other archive-style ND2 candidates should stay as README notes
  until a stable direct file URL, file size, and local path can be recorded in
  `fixture_sets.tsv`.

Download every discovered file, including multi-GB samples:

```bash
python3 external-fixtures/scripts/download_ome_samples.py --include-large
```

Discovery mode writes `external-fixtures/manifests/discovered.tsv`. Set mode
uses `external-fixtures/manifests/fixture_sets.tsv` and does not refresh the
full remote index.

Maintenance rules:

- Add a targeted set when the automatic `-smoke` set is metadata-only, a
  sidecar-only group, or a set of unrelated smallest files.
- Keep companion files with the image payload when the format requires them.
- Do not promote broad `nd2-feature`, `svs`, `leica-scn`, `ventana`, BDV HDF5,
  or multi-GB Imaris/Gatan samples into routine tests without a focused audit.
- Re-run `--validate-sets` after editing `fixture_sets.tsv`.
