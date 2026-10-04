# Third-party layout move: concrete recipe, and Bazel deps on //build/secondary labels

- **Learned from:** human review on [CL 1841583](https://fuchsia-review.googlesource.com/c/fuchsia/+/1841583) (follow-up implemented as [CL 1859317](https://fuchsia-review.googlesource.com/c/fuchsia/+/1859317), which unblocked [CL 1841584](https://fuchsia-review.googlesource.com/c/fuchsia/+/1841584))
- **Date:** 2026-10-02
- **Changed:** `checks/gn_secondary_tree.sh`, `checks/manifest.json`

## Root cause

`gn_secondary_tree` only looked at files under build/secondary/ and at the
bazel2gn target map. The zxdb migration (1841584) added
`"//build/secondary/third_party/double-conversion"` to `expr/BUILD.bazel`, a
label whose Bazel target an earlier CL in the stack had removed. No check
caught the dangling dependency. The remediation text also suggested reusing an
upstream BUILD file in src/. For double-conversion that file was stale: it
listed sources that no longer exist. Together with upstream's WORKSPACE it
also made src/ a separate Bazel package, so file labels like
`//third_party/<name>/src:<file>` fail with "target not declared in package".

## What worked

A separate CL below the migration CL. It moves the manifest path to
`third_party/<name>/src` and pins pristine upstream; the fork only added
BUILD.gn and OWNERS. It also moves `.gitignore`, the gitlink and
`.gitmodules`, and adds BUILD.gn, BUILD.bazel, OWNERS and README.fuchsia
(with Security Critical) in fuchsia.git. The check-licenses asset README and
its policy exceptions are deleted. Bazel gets the sources through
`new_local_repository(build_file = //third_party/<name>:<name>.BUILD.bazel)`
in toplevel.MODULE.bazel, plus an alias at `//third_party/<name>`, so GN and
Bazel share one label and no target-map entry is needed.

## Why not a one-off fix

Every legacy third-party library a migration touches has the same shape: an
old-layout fork, maybe a build/secondary overlay, and upstream Bazel files of
unknown freshness. The check now flags Bazel deps on //build/secondary from
anywhere, and the remediation carries the full recipe. The coder can then do
the move or report it as a blocker, instead of inventing labels.
