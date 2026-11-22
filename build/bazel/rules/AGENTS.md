# Fuchsia Platform Bazel Rules (`//build/bazel/rules`)

Before authoring, refactoring, or reviewing `.bzl` or `BUILD.bazel` files in
this directory tree, read:

- [`fuchsia-bazel-rules-and-macros-guidelines`](.agents/skills/fuchsia-bazel-rules-and-macros-guidelines/SKILL.md),
  [`fuchsia-bzl-file-guidelines`](../.agents/skills/fuchsia-bzl-file-guidelines/SKILL.md),
  and the Fuchsia Bazel style docs
  ([`common.md`](../../../docs/development/languages/bazel/style/common.md),
  [`bzl_files.md`](../../../docs/development/languages/bazel/style/bzl_files.md),
  and
  [`rules_macros.md`](../../../docs/development/languages/bazel/style/rules_macros.md))
  for Fuchsia rule, symbolic `macro()`, and Starlark conventions.
- [`extract-common-bazel-sdk-functionality`](../../bazel_sdk/fuchsia_rules_common/.agents/skills/extract-common-bazel-sdk-functionality/SKILL.md)
  when extracting common functionality from a Fuchsia Bazel SDK rule, macro,
  and/or provider under `//build/bazel_sdk/bazel_rules_fuchsia/` into
  `//build/bazel_sdk/fuchsia_rules_common/` for shared use in platform Bazel
  rules.

## Critical Invariants

- **Directory layout and CL scoping**: Place new `.bzl` files in domain
  subdirectories (`rules/testing/`, `rules/bind/`, `rules/cc/`, etc., not
  directly under `rules/`), keep rules and macros together without separate
  `macros/` directories, put rule tests in `rules/<area>/tests/`, and never mix
  shared `.bzl` rule/macro edits into `[bazel_migration]` target CLs.
- **Layering vs. `@fuchsia_rules_common`**: Rules under `//build/bazel/rules/`
  cannot `load()` or depend on `@rules_fuchsia` or `@fuchsia_sdk`. Only put
  providers/helpers in `//build/bazel_sdk/fuchsia_rules_common/` when genuinely
  shared with `@rules_fuchsia`; keep in-tree-only providers under
  `rules/<area>/` in `UpperCamelCase` ending in `Info` without a `Fuchsia` or
  `Fx` prefix (unless avoiding a collision or referring to the `fx` tool, such
  as `FxHostToolInfo`).
- **Symbolic `macro()`, `inherit_attrs`, and `SelectorList` discipline**:
  Prefer symbolic `macro()` over legacy `def` macros, avoid `native.<lang>_*`
  (use ruleset symbols such as `@rules_cc//cc:cc_rules_extension_support.bzl`
  for `inherit_attrs`), name subtargets `name + ".<suffix>"` or
  `name + "_<suffix>"` (never leading `"_" + name`), forward `**kwargs` only to
  the target matching `inherit_attrs`, and override any attribute inspected in
  `_<name>_impl` with `configurable = False` in `attrs`.
- **Naming, visibility, and performance**: Declare explicit load
  `visibility(...)` in every `.bzl` file (never `visibility("public")` or
  `defs.bzl`), prefix platform wrappers of common rules with `fx_` (never
  `fuchsia_`; `rustc_*` is the Rust exception), avoid flattening `depset`s in
  analysis, build command lines with `ctx.actions.args()`, and pass tools via
  `attr.label(executable = True, cfg = "exec")`.
- **Verification**: Run `fx format-code`, `fx host-tool shac check`,
  `fx build //build/bazel/rules/tests:quick_tests`, and
  `fx bazel test --config=host //build/bazel/rules/tests:host_tests`.
