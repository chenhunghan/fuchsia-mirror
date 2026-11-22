---
name: fuchsia-bazel-sdk-guidelines
description: >
  Conventions, repository boundaries, and intentional platform-style
  exceptions for the Fuchsia Bazel SDK under //build/bazel_sdk/ (including
  bazel_rules_fuchsia, fuchsia_rules_common, and tests). Use when authoring,
  modifying, or code-reviewing Bazel SDK files under //build/bazel_sdk/. Do
  NOT use for platform packages outside //build/bazel_sdk/ or as a standalone
  review orchestrator (use /review).
---

# Fuchsia Bazel SDK guidelines

Apply these rules when authoring, modifying, or reviewing files under
`//build/bazel_sdk/`. As noted in the [Fuchsia Bazel style guide][bazel-style],
the Fuchsia Bazel SDK ships to out-of-tree (OOT) Bazel workspaces and diverges
from some in-tree platform Bazel style and best practices for intentional and/or
historical reasons.

When sharing functionality between SDK rules and non-SDK platform rules
(`//build/bazel/rules/...`) via `@fuchsia_rules_common`, follow
[extract-common-bazel-sdk-functionality](/build/bazel_sdk/fuchsia_rules_common/.agents/skills/extract-common-bazel-sdk-functionality/SKILL.md)
and
[fuchsia-bazel-rules-and-macros-guidelines](/build/bazel/rules/.agents/skills/fuchsia-bazel-rules-and-macros-guidelines/SKILL.md).

## SDK repository layout and strict dependency boundaries

The Bazel SDK comprises distinct repositories with strict one-way layering (see
[build/bazel_sdk/README.md][sdk-readme],
[fuchsia_rules_common/README.md][rules-common-readme], and
[bazel_rules_fuchsia/README.md][rules-fuchsia-readme]):

1.  **`@fuchsia_rules_common` (`//build/bazel_sdk/fuchsia_rules_common/`):**
    - Follow [fuchsia_rules_common/README.md][rules-common-readme] for naming
      conventions, documentation requirements, and layering rules.
    - Holds shared providers, parameterized action implementations
      (`*_common_impl`), shared attribute dictionaries
      (`<TARGET>_COMMON_ATTRS`), and helper scripts used by **both**
      `@rules_fuchsia` and in-tree platform rules (`//build/bazel/rules/...`).
    - **Only place code here when genuinely shared with the SDK:** If a rule,
      helper, or provider is only used in-tree (not by `@rules_fuchsia`), place
      it under `//build/bazel/rules/<area>/` instead without a `Fuchsia` prefix
      (`Fuchsia*` implies SDK membership).
    - Must **never** load from `@rules_fuchsia`, `@fuchsia_sdk`, or in-tree
      platform paths (`//build/bazel/rules/...`, `//src/...`, `@gn_targets//`).
    - Any new subdirectory under `fuchsia_rules_common/` must export a
      `bzl_srcs` `filegroup` and register it in
      `//build/bazel_sdk/fuchsia_rules_common/BUILD.bazel`.
2.  **`@rules_fuchsia` (`//build/bazel_sdk/bazel_rules_fuchsia/`):**
    - `fuchsia/`: Public Starlark API loaded by SDK consumers.
    - `fuchsia/private/`: Private rule/macro implementations, providers, and
      transitions.
    - `fuchsia/tools/`, `fuchsia/workspace/`, `fuchsia/constraints/`,
      `fuchsia/flags/`: Supporting Python tools, repository rules/templates,
      constraints, and build flags.
    - Must **never** depend on platform targets (`//src/...`,
      `//build/bazel/rules/...`, `@gn_targets//`); tools must come from the SDK
      toolchain/providers (`sdk.ffx*`) or `@fuchsia_rules_common`.
3.  **Generated SDK repositories (`@fuchsia_sdk`, `@fuchsia_clang`):**
    - Generated from the GN-built Core IDK (`//sdk:final_fuchsia_sdk`) via
      templates under `fuchsia/workspace/sdk_templates/`.

## Intentional exceptions to platform Bazel rules

Do **not** flag the following patterns as errors inside `//build/bazel_sdk/`,
even though SHAC and platform style forbid them in non-SDK platform packages:

- **`fuchsia_*` file, rule, macro, and `Fuchsia*Info` provider names:** Public
  SDK rules, files, and SDK providers intentionally use the `fuchsia_` /
  `Fuchsia` prefix (such as `fuchsia_package`, `fuchsia_cc_binary`,
  `FuchsiaPackageInfo`). Reserve `fx_*` for non-SDK platform wrappers under
  `//build/bazel/rules/...`.
- **Public target and load visibility in rule packages:**
  `"//visibility:public"` and `visibility("public")` are permitted under
  `//build/bazel_sdk/*rules*/...` because external OOT repositories consume
  these targets and `.bzl` files. Keep `fuchsia/private/...` internals scoped to
  SDK implementation packages rather than exposing them as public API.
- **SDK repository labels:** Referencing `@fuchsia_sdk//`, `@rules_fuchsia//`,
  `@fuchsia_clang//`, and `@fuchsia_rules_common//` is expected inside
  `//build/bazel_sdk/...` and its test suites.
- **Legacy `def` macros and external Bazel compatibility:** Public SDK
  entrypoints in `bazel_rules_fuchsia/` may use legacy `def` macros or
  `defs.bzl` re-export surfaces for backward compatibility with external SDK
  consumers (prefer symbolic `macro()` for new non-public/shared macros in
  `fuchsia_rules_common/` where compatible). When maintaining legacy macros:
  - Explicitly set `visibility = ["//visibility:private"]` on all internal
    helper targets so they do not inherit a caller's `default_visibility`.
  - Declare used arguments explicitly in the macro signature instead of reading
    them via `kwargs.get(...)`, and document all public rules, macros, and
    attributes with clear `doc` strings.
  - When relocating shared helpers from `fuchsia/private/...` into
    `@fuchsia_rules_common`, re-export them from their original SDK `.bzl`
    location if existing internal or external callers still load them.

## Starlark quality rules that still apply in bazel_sdk

- **No `glob()` or recursive `**` globs** unless explicitly allowlisted, and no
  committed `print()` statements.
- **Fail loudly and preserve providers:** Call `fail(...)` on invalid or
  deprecated attribute combinations rather than silently ignoring inputs (and
  include `//build/bazel_sdk/OWNERS` on breaking SDK API changes). Forward
  complete provider sets (including `OutputGroupInfo` and
  `InstrumentedFilesInfo`) in wrapper/transition rules without duplicating
  `_solib_` shared libraries in `default_runfiles`.
- **Performant, hermetic actions:** Use `depset(direct = ..., transitive = ...)`
  without analysis-phase `to_list()` flattening, build command lines with
  `ctx.actions.args()`, and pass tools via executable label attributes or SDK
  toolchains -- never via `PATH` lookups, `bazel-bin` symlinks, or labels
  embedded inside JSON/dict strings.
- **Explicit visibility and synchronized test names:** Declare load
  `visibility(...)` in `.bzl` files where enforced by SHAC, explicit
  `visibility` on `config_setting()` and `exports_files()`, and keep test target
  names under `//build/bazel_sdk/tests/...` aligned with updated assertions.

## Verification and testing workflow

Always format and run SHAC static checks before committing:

```posix-terminal
fx format-code
fx host-tool shac check
```

Choose build/test verification matching what changed (note that
`//build/bazel_sdk/tests/` is a separate Bazel workspace excluded from the
platform workspace via `toplevel.REPO.bazel`, so `fx bazel test
//build/bazel_sdk/tests/...` will fail; see
[build/bazel_sdk/tests/README.md][sdk-tests-readme]):

- **Static Starlark or unit test changes (`bazel_rules_fuchsia`,
  `fuchsia_rules_common`, `build/bazel_sdk/tests`):** Static `.bzl` changes do
  not require regenerating the full IDK; run `fx build bazel_sdk_tests` or
  `build/bazel_sdk/tests/scripts/run_tests.py` (plus `fx bazel test
  //build/bazel/rules/...` when touching `fuchsia_rules_common/`).
- **IDK generator or `sdk_templates` changes:** Build and validate the full
  Bazel SDK via GN:

  ```posix-terminal
  fx build //sdk:final_fuchsia_sdk_tests
  fx build //sdk:final_fuchsia_sdk
  ```

- For OOT consumer validation on a Gerrit CL, use `fx lsc <cl_url> bazel_sdk`
  per [build/bazel_sdk/README.md][sdk-readme].

[bazel-style]: /docs/development/languages/bazel/style/README.md
[sdk-readme]: /build/bazel_sdk/README.md
[sdk-tests-readme]: /build/bazel_sdk/tests/README.md
[rules-common-readme]: /build/bazel_sdk/fuchsia_rules_common/README.md
[rules-fuchsia-readme]: /build/bazel_sdk/bazel_rules_fuchsia/README.md
