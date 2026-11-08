# Bazel scripts for manual use only

The scripts in this directory are helper tools intended only to be run manually
by developers. They are not used as part of the Fuchsia build or any CI
automation, and as such, they may not be as robust as production tooling.

Do NOT reference any files in this directory from anywhere else in the build.
These scripts must never be added as dependencies or inputs to any GN or Bazel
build targets, build rules, or automation scripts.

## Scripts

### `uprev_bazel_vendor_dep.py`

Updates the version of a vendored Bazel dependency across in-tree `MODULE.bazel`
files, runs `fx update-bazel-vendor-dir` to re-vendor it, and commits the
results.

Usage:

```sh
build/bazel/scripts/manual/uprev_bazel_vendor_dep.py <dep_name> <new_version>
```

Example:

```sh
build/bazel/scripts/manual/uprev_bazel_vendor_dep.py rules_cc 0.2.25
```
