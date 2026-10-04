# Third-party code built through GN's secondary source tree (build/secondary overlay) migrated in place instead of to the //third party/<name>/src layout

- **Learned from:** human review on [CL 1841583](https://fuchsia-review.googlesource.com/c/fuchsia/+/1841583), patchset 6 (task `cl-1841583`)
- **Date:** 2026-10-02
- **Changed:** `checks/gn_secondary_tree.sh`, `checks/manifest.json`

## Root cause

No check or prompt rule covered build/secondary/: the coder treated a GN-only
overlay directory like any other package, added a BUILD.bazel there and invented
bazel2gn target-map entries to bridge the labels. No existing check looks at
where a BUILD.bazel lives or what target-map entries it points to.

## Why not a one-off fix

Every third-party library that GN provides through build/secondary/ hits the
same trap when it is migrated. Fixing one directory leaves the coder free to put
Bazel builds in that GN-only tree again, so a deterministic, path-based check
with the third-party layout migration as its remediation is needed.
