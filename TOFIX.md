# Current parity status

Updated: 2026-10-26. Reference: Bio-Formats `d54bf462e3`.

The identified non-video implementation gaps have been fixed. Completed work
and its verification scope are recorded in the
[gap-closure report](audit/non-video-gap-closure-2026-10-26.md) and
[audit index](AUDITLIST2.md). This page does not repeat closed tasks as a backlog.
The previous issue ledger is a clearly marked
[historical snapshot](audit/archive/2026-10-26/TOFIX.md).

## Validation limits

Real native SlideBook projects, LEI/raw companion datasets, LSM MDB/overlay
projects, and some MIAS/CellWorx companion variants remain scarce. Source-backed
synthetic fixtures and available real datasets verify implementation paths;
coverage of every proprietary variant is not established. ND2 has bounded
comparisons across 62 files / 202 core series; one oversized pixel series and
deeper whole-corpus coverage remain unverified. See [ND2 status](ND2.md).

## Excluded work

QuickTime H.264, HEVC, ProRes, Motion JPEG 2000 and DV decoders were excluded by
request. Additional JPEG 2000 region-decode optimization is optional performance
work. New writers without Java counterparts are separate feature work.
