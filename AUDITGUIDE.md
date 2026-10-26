# Audit Guide

When auditing reader parity against Java Bio-Formats, check these failure modes explicitly.

- Standing rule: Rust-only helper functions should be removed or inlined by
  default because they prevent direct audit against the original Java code and
  can drift from it. The only allowed exception is an adapter for a Java library
  call that has been replaced by a Rust library/API with a different shape. Such
  helpers must preserve Java behavior instead of introducing a new abstraction.
  Do not introduce new helper layers during parity work unless they meet that
  adapter exception.
- Verify the path is a file or directory before diagnosing a format problem. A user may report a directory as an unreadable file.
- Run `file` or a short header probe, but do not trust extensions alone. Examples seen here: `.pict` was HTML text, `.pov` was scene source rather than DF3 voxel data, and `.mlf/.mrf` were Yokogawa metadata companions rather than standalone images.
- Check companion-file naming rules. ICS 1.0 requires same-stem `.ics`/`.ids`; `ics.ics` plus `ids.ids` fails for both Java and Rust until staged as `ics.ics` plus `ics.ids`.
- Compare Java and Rust on the same opening path. If Java rejects the original path, do not call it a Rust parity failure unless Rust should also reject or diagnose it cleanly.
- For companion metadata/index files, prefer precise unsupported diagnostics over accidental fallback to unrelated readers.
- Ensure byte-signature readers do not claim every unknown stream. A permissive `is_this_type_by_bytes` can misroute unrelated formats and hide the real diagnostic.
- Treat panics as audit blockers. Invalid or degenerate files must return `Result::Err`, especially empty TIFF IFD/series states and malformed whole-slide wrappers.
- Verify registry routing before reader internals. Extension-priority cases such as `.ndpi`, `.scn`, `.bif`, `.mrf`, and `.mlf` can select the wrong reader even when the target reader is implemented.
- Audit BigTIFF/PSB large-length fields separately from classic 32-bit formats. Large-document section lengths can move the image-data offset and produce plausible but wrong pixels.
- Check both `set_id` success and first-plane/region read success. A reader can open metadata but fail when pixels are requested.
- Always compare series count and series order against Java. BIF/SCN whole-slide files can expose thumbnails, macro images, pyramids, and labels in different orders.
- For pyramid formats, compare dimensions per series and per resolution. Stitched full-resolution dimensions can differ from raw IFD dimensions.
- Check RGB metadata as a group: `sizeC`, `rgb`, `interleaved`, `rgbChannelCount`, `dimensionOrder`, and `imageCount`. Fixing only one field often leaves pixel layout wrong.
- Do not assume `is_interleaved=false` for non-RGB data. SDT matched Java only after reporting interleaved lifetime/channel data correctly.
- Verify OME graph counts, not only image dimensions. Java may create minimal instruments/objectives/detectors/planes even when values are mostly empty.
- Populate OME plane records when Java does, even if no timestamp or position value is present.
- For reader-specific OME metadata, check map annotation counts. Extra Rust-only original metadata annotations can break parity even when core metadata is correct.
- Compare bounded pixel regions, not just full-plane reads. Full planes may be huge or slow; 256x256 top-left and offset crops catch different classes of bugs.
- Include non-zero-origin region checks. BIF passed a top-left crop for one level while failing the centered offset crop.
- Apply memory budgets to whole-slide pixel checks. Core/OME parity can still be audited when full-plane or region decode would allocate gigabytes.
- Separate metadata parity from pixel parity. A metadata-only pass is useful for slow SDT/BIF files, but record that pixel parity remains unverified.
- When pixel CRCs diverge, test simple transforms before deeper rewrites: planar/chunky conversion, channel permutation, BGR/RGB order, row stride, endian swap, and crop origin.
- For JPEG-backed whole-slide formats, distinguish codec rounding from layout bugs. Small per-sample deltas can be tolerated; large max differences usually indicate channel, tile, or coordinate errors.
- For stitched whole-slide readers, audit tile AOI mapping, overlap trimming, resolution scale, and macro/label separation.
- For invalid files that Java rejects, align Rust behavior to clean rejection unless Rust intentionally supports more. Avoid panics and misleading unsupported-compression errors.
- Add ad hoc parity inputs through the harness rather than hard-coding user data into the normal test corpus.
- Keep temporary probes out of committed sources. Use `.tmp` for one-off byte dumps and remove debug binaries after the investigation.
- Record exact remaining gaps after each audit: which files pass core, OME, pixels, which files Java rejects, and which Rust failures are clean diagnostics.
