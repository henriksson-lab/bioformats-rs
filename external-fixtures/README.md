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
python3 external-fixtures/scripts/download_ome_samples.py --set czi-smoke
python3 external-fixtures/scripts/download_ome_samples.py --set czi-plate-scenes
python3 external-fixtures/scripts/download_ome_samples.py --set nd2-smoke
python3 external-fixtures/scripts/download_ome_samples.py --set nd2-feature
```

Large stress sets are intentionally separate:

```bash
python3 external-fixtures/scripts/download_ome_samples.py --set czi-zenodo-large --include-large
python3 external-fixtures/scripts/download_ome_samples.py --set nd2-regression-large --include-large
```

Download every discovered file, including multi-GB samples:

```bash
python3 external-fixtures/scripts/download_ome_samples.py --include-large
```

Discovery mode writes `external-fixtures/manifests/discovered.tsv`. Set mode
uses `external-fixtures/manifests/fixture_sets.tsv` and does not refresh the
full remote index.
