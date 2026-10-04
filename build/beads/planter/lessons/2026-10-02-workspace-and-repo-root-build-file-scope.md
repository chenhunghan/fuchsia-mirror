# Workspace and repo root build file scope recognition

- **Learned from:** inner loop friction on a local task (task `sparse-rust`)
- **Date:** 2026-10-02
- **Changed:** `checks/build_only_scope.sh`, `checks/manifest.json`

## Root cause

checks/build_only_scope.sh only matched exact basenames BUILD.gn, BUILD.bazel,
BUILD, and MODULE.bazel plus .gni/.bzl/.bazelrc suffixes, so it misclassified
workspace-root and external-repository Bazel build definition files named
*.BUILD.bazel or *.BUILD (such as build/bazel/toplevel.BUILD.bazel, where
prebuilt inputs without a package BUILD.bazel are exported via exports_files) as
non-build source files.

## Why not a one-off fix

Any migration whose targets or host_test_data_files reference prebuilt or
external-repository paths that lack an in-directory BUILD.bazel must export
those files in build/bazel/toplevel.BUILD.bazel or a *.BUILD.bazel template
file; recognizing the .BUILD.bazel and .BUILD suffixes in build_only_scope.sh
fixes this across all future migrations.
