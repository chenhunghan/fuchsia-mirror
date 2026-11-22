---
name: fuchsia-bazel-guidelines
description: >
  Fuchsia platform Bazel formatting, target visibility, platform-SDK
  dependency boundaries, language wrapper rules, host constraints, and three-
  layer test_suite conventions. Use when authoring, modifying, or code-
  reviewing BUILD.bazel, .bazel, and .bzl files in the Fuchsia platform build.
  Do NOT use for vendored or upstream third-party Bazel files, or as a
  standalone review orchestrator (use /review). For //build/bazel_sdk/, see
  build/bazel_sdk/.agents/skills/fuchsia-bazel-sdk-guidelines/SKILL.md.
---

# Fuchsia Bazel guidelines

Apply these rules when authoring, modifying, or reviewing `BUILD.bazel`,
`.bazel`, and `.bzl` files in the Fuchsia platform build. These conventions
distill the [Fuchsia Bazel style guide][bazel-style] ([common][bazel-common],
[BUILD.bazel files][bazel-build], and [tests and test suites][bazel-tests]).

- When authoring or reviewing `.bzl` files, also apply
  [fuchsia-bzl-file-guidelines](/build/bazel/.agents/skills/fuchsia-bzl-file-guidelines/SKILL.md),
  and apply
  [fuchsia-bazel-rules-and-macros-guidelines](/build/bazel/rules/.agents/skills/fuchsia-bazel-rules-and-macros-guidelines/SKILL.md)
  when defining or modifying Starlark rules, macros, providers, or functions.
- Files under `//build/bazel_sdk/` follow separate SDK conventions covered by
  [fuchsia-bazel-sdk-guidelines](/build/bazel_sdk/.agents/skills/fuchsia-bazel-sdk-guidelines/SKILL.md).

## File structure, source lists, and globs

- **Standard `BUILD.bazel` layout:** Order sections as:
  1.  Copyright header (and optional package overview comment)
  2.  `load()` statements
  3.  `package(default_applicable_licenses = ["//:license"])` (required in
      `fuchsia.git` per [build_bazel_header_template.md][header-template])
  4.  Package `test_suite()` targets (`"tests"` and/or `"host_tests"`)
  5.  Production and package-private `*_test` target declarations
- **Keep `BUILD.bazel` DAMP and avoid top-level comprehensions:** Declare
  targets explicitly rather than generating targets with top-level `[... for
  ...]` comprehensions so engineers, `bazel query`, and `bazel2gn` can inspect
  targets directly. Never modify shared `.bzl` rules or macros inside a target
  migration CL; land `.bzl` rule/macro changes in a separate `[bazel][<area>]`
  CL reviewed by `//build/bazel/BAZEL_OWNERS`.
- **Never use `glob()` or recursive globs (`**`):** List source and data files
  explicitly (SHAC error). Because `fx build` invokes Bazel from Ninja actions,
  Ninja cannot detect newly added files that match an existing `glob()`, causing
  stale incremental builds and `fx test` runs. Do not edit
  `build/bazel/shac/*glob_allowlist.json` without Build team approval.
- **Shared list variables:** Local variables are permitted in `BUILD.bazel` for
  large, substantially similar `srcs`, `deps`, or `visibility` lists. Never
  remove or subtract items from a list variable.
- **String quoting:** Use double quotes (`"..."`) by default; use single quotes
  only when doing so avoids escaping embedded double quotes (`\"`).

## Target visibility in BUILD.bazel

- **Never use public visibility:** Do not use `"//visibility:public"` outside
  `//build/bazel_sdk/*rules*/...` (SHAC error).
- **Describe the target's audience, not a caller allowlist:** Follow
  [common.md][bazel-visibility] when scoping `visibility`:
  - If 5+ packages within a subsystem consume a target, group to that
    subsystem's lowest common ancestor (such as
    `["//src/storage:__subpackages__"]`) rather than enumerating individual
    `:__pkg__` callers -- without collapsing all the way to
    `//src:__subpackages__` or `//:__subpackages__` unless genuinely tree-wide.
  - Define a `package_group()` when several packages share the same multi-tree
    audience (such as driver or IDK history packages). Note that negative
    package specs work in target `visibility` but not in `.bzl` load
    `visibility()`.
  - **Deprecated targets** are the exception: list exact current `:__pkg__`
    consumers so new usages are blocked.
  - Use `"//:__pkg__"` (never `"//:__subpackages__"`) when granting access to
    targets defined in the workspace root package (`//`).
  - Preserve architectural boundaries enforced in GN via `assert_no_deps` (for
    example, do not expose internal runtime `bin/` targets broadly when callers
    must depend on a public `//sdk/lib/...` wrapper).
- **Avoid package `default_visibility`:** Specify `visibility` per exported
  target instead of setting `package(default_visibility = [...])`. Broad
  `default_visibility` (`"//visibility:public"`, `"//:__subpackages__"`, or
  `"//src:__subpackages__"`) is a SHAC error.
- **Omit `visibility` on package-private targets:** Targets in `BUILD.bazel` are
  package-private (`:__pkg__`) by default, unlike GN where targets default to
  public. Exceptions:
  - Targets instantiated inside legacy `def` macros must explicitly set
    `visibility = ["//visibility:private"]`.
  - Dual-built `bazel2gn` files with intra-package dependencies may include
    `":__pkg__"` in `visibility` when needed by GN, accompanied by a comment
    referencing `bazel2gn`.
- **Mandatory `visibility` on public-by-default rules:** Always specify explicit
  `visibility` on `config_setting()` and `exports_files()` (SHAC error), which
  Bazel otherwise defaults to public.
- **Vendor visibility restrictions:** Public `fuchsia.git` files must never name
  vendor subpaths or partner names in visibility lists (SHAC error). Only
  `//vendor:__pkg__`, `//vendor:__subpackages__`, `//vendor/google:__pkg__`, and
  `//vendor/google:__subpackages__` are allowed in `BUILD.bazel`.

## Platform boundaries, language wrappers, constraints, and labels

- **Do not mix platform and Bazel SDK symbols or repositories:** Platform
  targets (including AIBs and IDK inputs) must never reference `bazel_sdk` paths
  or load/depend on `@fuchsia_sdk//`, `@internal_sdk//`, or
  `@rules_fuchsia//fuchsia` (SHAC error). `@fuchsia_rules_common//` is reserved
  for Build-team rules that share implementation with the SDK.
- **Avoid the `fuchsia_` prefix:** Do not name platform `.bzl` files, rules, or
  macros `fuchsia_*`; that prefix belongs to the Fuchsia Bazel SDK. Platform
  wrappers around common rules use `fx_*` (`fx_cc_library()`, `fx_package()`).
- **Use Fuchsia language wrappers (SHAC error):**
  - **C/C++:** Load `fx_cc_*()` from `//build/bazel/rules/cc/...` (and
    `fx_cc_*_library_zx()` for Zircon libraries), never `cc_*` from
    `@rules_cc//`.
  - **Rust:** Load `rustc_*()` from `//build/bazel/rules/rust/...`, never
    `rust_*` from `@rules_rust//`.
  - **Go and Python:** Fuchsia has no custom wrappers; load directly from
    `@io_bazel_rules_go//go...` and `@rules_python//python...`.
- **Platform and CPU constraints (`target_compatible_with`):** See [BUILD.bazel
  files][bazel-target-compat] for full details:
  - **Host targets:** Host tools, host tests, and host-only libraries must set
    `target_compatible_with = HOST_OS_CONSTRAINTS` from
    `//build/bazel/platforms:constraints.bzl`. Never use `HOST_CONSTRAINTS` from
    `@platforms//host:constraints.bzl` (SHAC error; pins the host CPU and breaks
    cross-CPU host builds) and never substitute `["@platforms//os:linux"]`.
  - **Fuchsia-only and CPU-specific targets:** Use `["@platforms//os:fuchsia"]`
    for Fuchsia-only targets and `@platforms//cpu:<arch>` (`x86_64`, `arm64`,
    `riscv64`) for architecture restrictions rather than `select()` + `#
    @bazel2gn:raw_overwrite:`.
  - **Constraint values, `alias()`, and `select()`:** Pass `constraint_value`
    labels (not `config_setting` labels) to `target_compatible_with`, repeat the
    same `target_compatible_with` on any `alias()` to a constrained target, and
    ensure every label in every branch of a `select()` exists in the build graph
    (because `bazel query` traverses unselected branches).
- **Permitted and prohibited label forms:**
  - Use `:target` only for targets/files in the same package, and `//path:name`
    (or `@platforms//...`, `@bazel_skylib//...`) across packages.
  - `//:` labels are valid for targets and files defined directly in the
    workspace root package (`//`), such as `package(default_applicable_licenses
    = ["//:license"])`. Never use workspace-root-relative package or
    subdirectory labels (`//:dir/foo`) -- except `"//:prebuilt/..."` for CIPD
    prebuilts -- and never place slashes after the colon (`//pkg:subdir/name`)
    (SHAC error).

## Tests and test_suite organization

Fuchsia connects Bazel tests to the build in three strict layers:

1.  **Individual packages (`BUILD.bazel`):**
    - Name all test targets ending in `_test` and keep them **package-private**
      (omit `visibility`; never reference `*_test` targets outside their own
      `BUILD.bazel` or place tests/`test_suite()` targets in `filegroup()`).
    - Expose package `test_suite()` targets named `"tests"` and/or
      `"host_tests"` as the **first targets** in `BUILD.bazel` (immediately
      after `load()` and `package()`), with `visibility` scoped only to the
      aggregation package(s) using `:__pkg__` (never `:__subpackages__`).
    - When every test in the package belongs to the same platform suite (and as
      a default in packages without tests yet), use an empty `tests` list with
      the standard comment:

      ```bazel
      test_suite(
          name = "tests",
          tests = [],  # Include all tests in the package.
          visibility = ["//src/connectivity/bluetooth:__pkg__"],
      )
      ```

    - **Never use `tests = []` when a package defines both Fuchsia and host
      tests:** `tests = []` captures every test in the package regardless of
      platform, and Bazel's `tests()` `cquery` silently skips incompatible
      members without failing `fx gen`. Define `"tests"` and `"host_tests"` with
      explicit `tests = [":..."]` lists instead, ensuring every test in the
      package (including macro-generated tests) belongs to a suite.
    - **Never depend on subpackages:** Individual packages must not reference
      tests or `test_suite()` targets in subdirectories (do not copy GN's
      recursive `group("tests")` pattern). Avoid per-CPU test targets.
2.  **Area aggregation packages (`BUILD.bazel`):**
    - Group child `:tests` / `:host_tests` targets into descriptive aggregation
      `test_suite()` targets ending in `tests` or `host_tests` (such as
      `bluetooth_tests` and `bluetooth_host_tests` in the same aggregation
      `BUILD.bazel`). Aggregation `BUILD.bazel` files should not define
      production or `*_test` targets.
3.  **GN build graph bridge (`BUILD.gn`):**
    - Reference aggregation `test_suite()` targets from a `bazel_test_suite()`
      in the **same directory's** `BUILD.gn` using full `"//..."` labels
      (`host_tests` for `*_host_tests`, `target_tests` for all other `*_tests`),
      **and add that `bazel_test_suite()` target (such as `":bazel_tests"`) to
      `deps` of a reachable `group("tests")` in the same `BUILD.gn`**. Defining
      `bazel_test_suite()` without wiring it into a reachable GN test group
      silently omits the tests from `tests.json`.

## Verification and review workflow

After authoring or modifying Bazel files, run the Build -> Lint -> Test loop
before committing:

```posix-terminal
fx format-code
fx host-tool shac check
fx build
```

- If the directory is dual-built with `bazel2gn` (contains the `BAZEL2GN
  SENTINEL` in `BUILD.gn`), re-sync and verify parity with `fx bazel2gn -d
  <dir>` and `fx lint --only=verify_bazel2gn`.
- Run affected test targets via `fx test` (and run `fx bazel test
  //path/to/tests:...` directly for Starlark rule/analysis tests, which do not
  fail `fx build` or run under `fx test`).

## Quick review checklist

- [ ] `package(default_applicable_licenses = ["//:license"])` follows `load()`;
  no `glob()`, recursive `**` globs, top-level comprehensions,
  workspace-root-relative `//:dir/foo` labels (except `"//:prebuilt/..."`), or
  `//pkg:a/b` colon-slash labels.
- [ ] No `"//visibility:public"`, broad `default_visibility`, or vendor
  subpaths; visibility describes an audience (subsystem LCA or `package_group`
  rather than long caller lists); `config_setting()` and `exports_files()` set
  explicit visibility.
- [ ] No `@fuchsia_sdk//`, `@internal_sdk//`, `@rules_fuchsia//`, or `fuchsia_*`
  definitions in non-SDK platform packages.
- [ ] C/C++ and Rust targets use `fx_cc_*` and `rustc_*` wrappers.
- [ ] Host tools and host-only libraries use `HOST_OS_CONSTRAINTS` (not
  `HOST_CONSTRAINTS` or `@platforms//os:linux`), and `target_compatible_with`
  does not accidentally narrow multi-platform libraries.
- [ ] `*_test` targets are package-private; `test_suite()` (`tests` /
  `host_tests`) appears first, does not mix platforms with `tests = []`, does
  not recurse into subpackages, and connects via `bazel_test_suite()` wired into
  a reachable `group("tests")` in `BUILD.gn`.

[bazel-style]: /docs/development/languages/bazel/style/README.md
[bazel-common]: /docs/development/languages/bazel/style/common.md
[bazel-visibility]: /docs/development/languages/bazel/style/common.md#visibility
[bazel-build]: /docs/development/languages/bazel/style/build_files.md
[bazel-target-compat]: /docs/development/languages/bazel/style/build_files.md#target-compatible-with
[bazel-tests]: /docs/development/languages/bazel/style/tests.md
[header-template]: /build/bazel/references/build_bazel_header_template.md
