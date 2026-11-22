# Fuchsia Bazel SDK (`//build/bazel_sdk`)

Files under `//build/bazel_sdk/` ship to out-of-tree (OOT) Bazel workspaces or
provide shared Starlark implementations, and diverge from some in-tree
platform Bazel style and best practices for intentional and/or historical
reasons. Before modifying or reviewing files here, read:

- [`fuchsia-bazel-sdk-guidelines`](.agents/skills/fuchsia-bazel-sdk-guidelines/SKILL.md)
  for SDK style exceptions, repository boundaries, and verification steps.
- [`fuchsia-bazel-rules-and-macros-guidelines`](../bazel/rules/.agents/skills/fuchsia-bazel-rules-and-macros-guidelines/SKILL.md)
  when authoring or reviewing Starlark rules, macros, and providers under
  `fuchsia_rules_common/`.
- [`README.md`](README.md),
  [`bazel_rules_fuchsia/README.md`](bazel_rules_fuchsia/README.md), and
  [`tests/README.md`](tests/README.md) for SDK build, test, and OOT override
  workflows.
- [`extract-common-bazel-sdk-functionality`](fuchsia_rules_common/.agents/skills/extract-common-bazel-sdk-functionality/SKILL.md)
  when extracting shared providers, macros, and rule implementations into
  `fuchsia_rules_common/` for use by both `@rules_fuchsia` and
  `//build/bazel/rules/...`.

## Critical Invariants

- **Strict one-way dependency and sharing boundary**: Neither
  `bazel_rules_fuchsia/` (`@rules_fuchsia`) nor `fuchsia_rules_common/`
  (`@fuchsia_rules_common`) may load from or depend on in-tree platform
  packages (`//src/...`, `//build/bazel/rules/...`, `@gn_targets//`), and
  `fuchsia_rules_common/` must not load from `@rules_fuchsia` or
  `@fuchsia_sdk`. Only place providers/helpers in `fuchsia_rules_common/` when
  actually shared with `@rules_fuchsia` (in-tree-only rules/providers belong in
  `//build/bazel/rules/<area>/` without a `Fuchsia` prefix).
- **Expose new `fuchsia_rules_common` packages**: Any new subdirectory added
  under `fuchsia_rules_common/` must define a `bzl_srcs` `filegroup` and
  register it in `fuchsia_rules_common/BUILD.bazel`.
- **Intentional SDK style exceptions**: `fuchsia_*` rule/macro/file names and
  `Fuchsia*Info` providers, `"//visibility:public"` and `visibility("public")`
  under `*rules*/...`, `@fuchsia_sdk//` and `@rules_fuchsia//` labels, and
  legacy `def` macro entrypoints in `bazel_rules_fuchsia/` for external
  compatibility are expected here. Internal helper targets inside legacy macros
  must still set `visibility = ["//visibility:private"]`, invalid inputs should
  call `fail(...)` rather than being silently ignored, and wrapper rules must
  forward `OutputGroupInfo` and `InstrumentedFilesInfo`.
- **Verification**: Run `fx format-code`, `fx host-tool shac check`, and
  `fx build bazel_sdk_tests` or `build/bazel_sdk/tests/scripts/run_tests.py`
  (or `fx build //sdk:final_fuchsia_sdk_tests` / `//sdk:final_fuchsia_sdk`
  when IDK generators or `sdk_templates` change).
