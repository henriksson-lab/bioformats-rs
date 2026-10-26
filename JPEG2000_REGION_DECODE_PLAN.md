# JPEG 2000 region-decode optimization

This is optional performance work, not a missing pixel-decoding capability.
The old design and experiments are [archived](audit/archive/2026-10-26/JPEG2000_REGION_DECODE_PLAN.md).
Their vendored-crate paths and version assumptions are historical; the current
Cargo dependency is authoritative.

Any future optimization must preserve the bytes produced by full decode plus
crop and must be demonstrated on generated and real wavelet-compressed images.
Keep fallback behavior where the codec cannot safely decode a region. Current
reader implementation work is tracked in [TOFIX.md](TOFIX.md).
