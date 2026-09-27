# Complete target migration and bazel2gn copt raw overwrite without skip

- **Learned from:** cq failure on CL Ife928bc7d881ec995bccdd165dc37adad70f86b1, patchset 1 (task `starnix-usercopy`)
- **Date:** 2026-09-25
- **Changed:** `checks/manifest.json`, `checks/migration_sanity.sh`, `prompts/coder.md`, `prompts/playbooks/case2_dual_build.md`, `prompts/reviewers/dual_build_sentinel.md`, `prompts/reviewers/target_parity.md`

## Root cause

The existing machinery lacked a deterministic check and explicit coder/reviewer
rules forbidding target-level `# @bazel2gn:skip` on convertible library targets
(`fx_cc_library`, `cc_library`, `rustc_library`, etc.) or forbidding duplicate
manual `static_library` / `source_set` definitions left above `## BAZEL2GN
SENTINEL` in `BUILD.gn`. When a C/C++ reference/bitrot-prevention library used
compiler flags in `copts` (`-O3`, `-fno-omit-frame-pointer`,
`-mno-omit-leaf-frame-pointer`) not present in `bazel2gn`'s built-in
`coptToConfig` table, the coder worked around `bazel2gn`'s `unexpected copt`
error by annotating the `fx_cc_library` with `# @bazel2gn:skip` and keeping a
duplicate `static_library` above the sentinel in `BUILD.gn` instead of using `#
@bazel2gn:raw_overwrite:[ ... ]` on `copts`.

## Why not a one-off fix

Fixing only a single package's `BUILD.bazel` and `BUILD.gn` leaves the coder
agent free to apply `# @bazel2gn:skip` to any convertible C/C++ or Rust library
whenever `bazel2gn` rejects an unmapped `copts` flag or when a standalone
reference/bitrot-prevention library is only referenced by `group("tests")` in
`BUILD.gn`. Only a systemic check in `migration_sanity.sh` combined with
explicit `# @bazel2gn:raw_overwrite` rules in `coder.md`, `case2_dual_build.md`,
and reviewer seats prevents skipped convertible targets, duplicate manual GN
definitions above the sentinel, and missing `verify_bazel2gn` registrations
across all future migrations.
