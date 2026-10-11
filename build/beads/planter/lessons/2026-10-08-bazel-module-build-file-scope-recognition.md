# Bazel module build file scope recognition

- **Learned from:** inner loop friction on CL I957e390393a5a53cd48ea8186323fe8fc4192f62, patchset 2 (task `starnix-perfetto-trace-decoder`)
- **Date:** 2026-10-08
- **Changed:** `checks/build_only_scope.sh`, `checks/manifest.json`

## Root cause

checks/build_only_scope.sh recognized exact MODULE.bazel basenames and
*.BUILD.bazel suffixes as build-definition files, but omitted the .MODULE.bazel
suffix used by Fuchsia's root module definition file
(build/bazel/toplevel.MODULE.bazel) when declaring repository overlays such as
new_local_repository.

## Why not a one-off fix

Exempting a single third-party overlay or package directory would still leave
build_only_scope rejecting build/bazel/toplevel.MODULE.bazel (or any other
*.MODULE.bazel module segment) whenever a future migration needs to register a
repository overlay or module extension.
