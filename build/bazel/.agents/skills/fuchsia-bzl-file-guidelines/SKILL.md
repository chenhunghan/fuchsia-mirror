---
name: fuchsia-bzl-file-guidelines
description: >
  Fuchsia Starlark .bzl file load visibility, private symbol naming, vendor
  visibility restrictions, and file naming conventions. Use when authoring,
  modifying, or code-reviewing .bzl files in the Fuchsia platform build. Do
  NOT use for BUILD.bazel-only changes without .bzl files, vendored or
  upstream third-party Bazel files, or as a standalone review orchestrator
  (use /review). For //build/bazel_sdk/, see
  build/bazel_sdk/.agents/skills/fuchsia-bazel-sdk-guidelines/SKILL.md.
---

# Fuchsia .bzl file guidelines

Apply these rules when authoring, modifying, or reviewing `.bzl` files in the
Fuchsia platform build. This skill distills the [.bzl files style
guide][bazel-bzl] and complements
[fuchsia-bazel-guidelines](/build/bazel/.agents/skills/fuchsia-bazel-guidelines/SKILL.md)
(common Bazel conventions) and
[fuchsia-bazel-rules-and-macros-guidelines](/build/bazel/rules/.agents/skills/fuchsia-bazel-rules-and-macros-guidelines/SKILL.md)
(defining rules, macros, providers, and functions).

- Files under `//build/bazel_sdk/` follow separate SDK conventions covered by
  [fuchsia-bazel-sdk-guidelines](/build/bazel_sdk/.agents/skills/fuchsia-bazel-sdk-guidelines/SKILL.md).

## Load visibility

- **Always call `visibility(...)` in every `.bzl` file:** Unlike GN `.gni`
  files, Bazel `.bzl` files have unrestricted load visibility unless
  `visibility(...)` is called. Every `.bzl` file must explicitly declare load
  visibility at the top level (SHAC error):
  - Use `visibility("private")` for `.bzl` files not intended to be loaded
    outside their own package (or place internal `.bzl` files in a `private/`
    subdirectory when a package mixes public and private `.bzl` files).
  - Otherwise, pass an explicit package list `visibility([...])` scoped as
    tightly as practical (for example, `["//src/...", "//sdk/lib/..."]`). Use
    `"//..."` only for genuinely codebase-wide helpers.
- **Never use public load visibility:** Do not call `visibility("public")`
  outside `//build/bazel_sdk/*rules*/...` (SHAC error).
- **Load visibility path syntax differs from target visibility:**
  - `"//path/to/pkg"` grants load access only to that package (the `.bzl`
    equivalent of target visibility `"//path/to/pkg:__pkg__"`).
  - `"//path/to/pkg/..."` grants load access to that package and all of its
    subpackages (the equivalent of `"//path/to/pkg:__subpackages__"`).
  - Negative visibility specs are **not** supported in `.bzl` load
    `visibility(...)` (even via `package_group`).
- **Vendor load visibility restrictions:** Public `fuchsia.git` `.bzl` files
  must never name vendor subpaths or partner names in `visibility([...])` (SHAC
  error). Only `"//vendor"`, `"//vendor/..."`, `"//vendor/google"`, and
  `"//vendor/google/..."` may be referenced.

## Symbol visibility and file naming

- **Prefix file-private symbols with `_`:** Prefix helper functions, constants,
  providers, and internal rules with a leading underscore (`_`) so other files
  (even in the same directory) cannot `load()` them.
- **Never create `defs.bzl` grab-bag files:** Do not define or re-export
  unrelated symbols from a generic `defs.bzl` file. Limit exported symbols per
  `.bzl` file to cohesive definitions that belong together, and name each `.bzl`
  file descriptively after the rule, macro, or concept it exports (such as
  `fx_cc_library.bzl`).

## Verification workflow

Format and run SHAC static checks (including `bazel_bzl_require_load_visibility`
and `bazel_load_visibility`) before committing:

```posix-terminal
fx format-code
fx host-tool shac check
fx build
```

[bazel-bzl]: /docs/development/languages/bazel/style/bzl_files.md
