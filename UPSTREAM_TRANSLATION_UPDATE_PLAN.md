# Upstream translation update workflow

The latest completed upstream update is recorded in
[audit/upstream-2026-10-26.md](audit/upstream-2026-10-26.md).
The obsolete August work plan is [archived](audit/archive/2026-10-26/UPSTREAM_TRANSLATION_UPDATE_PLAN.md).

Use `/home/mahogny/github/claude/updaterust` to fetch status, scan added source
lines, and export an oldest-first mainline plan. Check every merge against
`git diff <commit>^1 <commit>`: the current tool's exported merge patches can
omit earlier commits in a merged branch even when the manifest counts them.
Translate or explicitly classify each unit before advancing the reference
checkout. Keep Java source read-only and preserve Rust optimizations only when
behavior remains equivalent. Run focused and parity validation, update the
README baseline, and record final tracker status.

For remaining implementation work unrelated to upstream lag, see [TOFIX.md](TOFIX.md).
