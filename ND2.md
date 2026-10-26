# ND2 validation status

The prior detailed audit is preserved in [the historical record](audit/archive/2026-10-26/ND2.md).

Recorded corpus evidence: 62 ND2 files, 202 core series, all core and OME
metadata comparisons passed. Bounded pixel sampling across that corpus passed
for 201 series; one oversized series was skipped by the 512 MiB full-plane
budget. Deeper pixel checks passed on the representative subsets documented
in the historical record.

The remaining issue is broader validation depth and performance measurement,
not a presently identified missing reader implementation. Do not treat the
old completed fix list as a current backlog. Current work is [TOFIX.md](TOFIX.md).
