# Bazel vendor directories

This directory contains vendored external repositories. Avoid manually editing
files under any of its subdirectories, as they are overwritten when re-vendored.

## Bazel vendored modules

These subdirectories are passed via `--vendor_dir` to different Bazel
workspaces, exposing their contents as Bazel modules (referenced with
`@<module_name>` labels) and maintained by `fx update-bazel-vendor-dir`:

- `fuchsia_in_tree/`: Vendored modules for the Fuchsia in-tree platform
  workspace (`//build/bazel/toplevel.MODULE.bazel`).
- `bazel_sdk_in_tree/`: Vendored modules for running the Fuchsia Bazel SDK test
  suite workspace (`//build/bazel_sdk/tests/MODULE.bazel`) against the in-tree
  Bazel binary (`//prebuilt/third_party/bazel`).
- `bazel_sdk_oot/`: Vendored modules for running the Fuchsia Bazel SDK test
  suite workspace (`//build/bazel_sdk/tests/MODULE.bazel`) against the pinned
  out-of-tree Bazel binary (`//prebuilt/third_party/bazel_oot`).

### Adding a Bazel module dependency

1. Add the dependency to `//build/bazel/toplevel.MODULE.bazel` or
   `//build/bazel_sdk/tests/MODULE.bazel`.
2. Add a corresponding `--repo` argument to the appropriate invocation in
   `//tools/devshell/contrib/update-bazel-vendor-dir`.
3. Run `fx update-bazel-vendor-dir`.

### Upgrading a Bazel module dependency

1. Update the version of the dependency in
   `//build/bazel/toplevel.MODULE.bazel` or
   `//build/bazel_sdk/tests/MODULE.bazel`.
2. Run `fx update-bazel-vendor-dir`.

## Vendored third-party code

`no_workspace/` holds third-party source trees copied into `fuchsia.git` from
declarations in `//manifests/third_party/MODULE.bazel`:

- The contents of `//third_party/bazel_vendor/no_workspace/` can be used by both
  GN and Bazel builds.
- When used by Bazel, these directories are not exposed as Bazel modules;
  instead, they are exposed as regular source directories through a symlink such
  as `//third_party/<name>/src` or `//third_party/<name>/vendor`, and their
  contents are referenced via `//third_party/<name>/{src,vendor}` labels rather
  than `@<module_name>` labels.
- The contents of `no_workspace/` are updated using
  `//build/bazel/scripts/update_no_workspace_bazel_vendor_dir.py`, which uses
  `bazel vendor` as an implementation detail of no relevance to the GN and Bazel
  parts of the build that consume these sources.
- As part of the Bazel migration, some vendored directories in `no_workspace/`
  will move to the Bazel vendored modules section above once no GN targets
  depending on them remain.

See [//manifests/third_party/README.md](../../manifests/third_party/README.md)
for details on adding or updating `no_workspace/` repositories.
