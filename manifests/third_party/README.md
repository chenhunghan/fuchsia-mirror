# Third-party source manifests

This directory declares third-party source repositories used by Fuchsia in two
ways:

- **Jiri manifests** (`all`, `dart`, `icu`, etc.): XML manifests listing
  external Git repositories that `jiri update` checks out as separate Git
  repositories under `//third_party/`.
- **`MODULE.bazel`** (with `extensions.bzl`, `BUILD.bazel`, and `.bazelrc`):
  Declares external Git repositories whose sources are copied directly into
  `fuchsia.git` under `//third_party/bazel_vendor/no_workspace/` instead of
  being checked out as separate Git repositories by `jiri update`.

## Copied third-party sources (`MODULE.bazel` and `no_workspace/`)

`MODULE.bazel` defines a standalone, vendoring-only Bazel module that is never
loaded during `fx build`. Using `bazel vendor` to materialize repositories into
`//third_party/bazel_vendor/no_workspace/+third_party+<name>/` is an
implementation detail:

- Typical `//third_party/<name>/BUILD.gn` and `//third_party/<name>/BUILD.bazel`
  files do not need to know about `bazel vendor`, apart from referencing the
  copied source tree (for example through a `//third_party/<name>/src` or
  `//third_party/<name>/vendor` symlink pointing to
  `../bazel_vendor/no_workspace/+third_party+<name>`).
- Bazel vendor-mode bookkeeping files (`VENDOR.bazel`, `bazel-external`,
  `_registries/`, and `@*.marker`) are stripped after `bazel vendor` runs so
  each `+third_party+<name>/` directory is a plain source tree committed into
  `fuchsia.git`.

See [//third_party/bazel_vendor/README.md](../../third_party/bazel_vendor/README.md)
for how `no_workspace/` compares to the `--vendor_dir` directories under
`//third_party/bazel_vendor/`.

### Adding or updating a copied third-party repository

1. Add or update the `third_party.git_repository(...)` (and `use_repo(...)`)
   entry in `//manifests/third_party/MODULE.bazel`.
2. Materialize the updated sources into
   `//third_party/bazel_vendor/no_workspace/` by running:

   ```sh
   ./scripts/fuchsia-vendored-python build/bazel/scripts/update_no_workspace_bazel_vendor_dir.py
   ```

   (Or run `./build/bazel/scripts/update_no_workspace_bazel_vendor_dir.py`
   directly if `//scripts` is in your `PATH`.)

   For `vendor/*` repositories that define their own
   `manifests/third_party/MODULE.bazel`, pass `--repo-root` (for example,
   `--repo-root vendor/google`).
