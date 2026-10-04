# Keep GN target types and C/C++ link semantics (alwayslink)

- **Learned from:** review request on CL 1842589 (not a planter task)
- **Date:** 2026-09-28
- **Changed:** `checks/gn_target_type_parity.sh`, `checks/manifest.json`, `prompts/coder.md`

## Root cause

A GN `source_set()` links all of its objects into every dependent; a
`static_library()`, like a Bazel `cc_library` without `alwayslink = True`, only
the objects that resolve undefined symbols. bazel2gn emits a
`cc_library`/`fx_cc_library` as `static_library()` unless it sets
`alwayslink = True`. Nothing required the coder to set `alwayslink`, and no
check compared a converted target with what it replaced, so a source_set could
silently become a static_library (or a source_set-less cc_library after a full
removal) and drop static initializers and other unreferenced objects. Other
template changes (e.g. a GN-only wrapper replaced by a plain rule) were not
checked either.

## Why not a one-off fix

Every migration is exposed, with or without bazel2gn. The coder prompt now maps
GN `source_set()` to `cc_library(alwayslink = True)` and `static_library()` to
`cc_library` without it (`sdk_source_set()` has its own mapping), and requires
bazel2gn migrations to keep every GN template. The new blocking
`gn_target_type_parity` check fails any bazel2gn migration that changes a GN
target's type, and any removal whose replacing cc_library's `alwayslink` does
not match the former GN type.
