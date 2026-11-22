# `@fuchsia_rules_common`

This directory defines the `@fuchsia_rules_common` Bazel module, which holds
providers, rules, macros, implementation helpers, aspects, and scripts shared
between:

* **Fuchsia Bazel SDK rules** (`//build/bazel_sdk/bazel_rules_fuchsia/`,
  `@rules_fuchsia`)
* **In-tree platform rules** (`//build/bazel/rules/`)

Files in this directory should follow the Fuchsia
[Bazel style guide and best practices][bazel-style-guide], although some
existing files predate parts of that guidance and may still have gaps.

For step-by-step instructions on extracting common functionality from
`@rules_fuchsia` into `@fuchsia_rules_common` so platform targets can use
locally built host tools instead of SDK prebuilts, see the
[`extract-common-bazel-sdk-functionality` skill][extract-common-skill].

## What belongs here

Only place definitions in `fuchsia_rules_common/` when other definitions in
this directory load and use them, or when **both** `@rules_fuchsia` and
`//build/bazel/rules/` load and use them:

* Do **not** add platform-only providers, rules, macros, or helpers here, even
  when related symbols already live in `fuchsia_rules_common/`. Define
  platform-only symbols under `//build/bazel/rules/` instead.
* Do **not** load from `@rules_fuchsia` or `//build/bazel/rules/` inside
  `fuchsia_rules_common/`.

When adding a new `<target_type>/` subdirectory, expose its `.bzl` files in a
`bzl_srcs` `filegroup()` in `<target_type>/BUILD.bazel` and add
`"@fuchsia_rules_common//<target_type>:bzl_srcs"` to `bzl_srcs` in
`BUILD.bazel`.

## Implementation patterns and naming

Use consistent `_common` naming across shared rules and macros
(`<name>_common`), implementation helpers (`<name>_common_impl()`), and their
paired attribute dictionaries (`<NAME>_COMMON_ATTRS`).

### Rule implementation helpers (taking `ctx`)

When `@rules_fuchsia` and `//build/bazel/rules/` need separate `rule()`
definitions (typically because the SDK rule resolves tools from the SDK
toolchain while the platform rule uses private `_tool` executable attributes),
share the implementation through a helper function that takes `ctx`:

* Name the helper `<target_or_action>_common_impl(ctx, *, ...)`.
  * For example, see `build_fuchsia_package_common_impl()` in
    [`packages/package.bzl`][package-bzl] for the structural pattern.
* Parameterize tool executables and environment differences rather than reading
  SDK toolchains directly.
* When attributes are also shared between the two `rule()` definitions for that
  implementation, export a `<TARGET_OR_ACTION>_COMMON_ATTRS` dictionary in the
  same `.bzl` file so each rule can merge
  `attrs = <TARGET_OR_ACTION>_COMMON_ATTRS | { ... }`.
* Cross-reference the dictionary and helper in both docstrings: document on
  `<TARGET_OR_ACTION>_COMMON_ATTRS` that these are the attributes required by
  `<target_or_action>_common_impl()`, and document on
  `<target_or_action>_common_impl()` that calling rules must include
  `<TARGET_OR_ACTION>_COMMON_ATTRS`.

### Shared helper attributes

When a shared helper function (such as an action helper or provider builder,
rather than a full `<name>_common_impl()` rule wrapper) requires specific
attributes on the calling rule's `ctx`, name the attribute dictionary after the
helper or capability (`<HELPER>_ATTRS` or `<CAPABILITY>_ATTRS`, without
`COMMON_` or `FUCHSIA_`) and cross-reference the dictionary and the helper
function(s) that require it in both docstrings (for example,
`BUILD_FLAGS_ATTRS` and `create_build_flags_info()` in
[`build_flags/build_flags.bzl`][build-flags-bzl]).

### Shared rules and macros

When both `@rules_fuchsia` and `//build/bazel/rules/` can instantiate (and
inherit attributes from via `inherit_attrs`) the exact same underlying rule or
symbolic macro, define it directly in `fuchsia_rules_common/`:

* Name shared rules and macros `<name>_common` rather than starting names with
  `fuchsia_`, because `fuchsia_*` in platform `BUILD.bazel` and `.bzl` files
  signals a Fuchsia Bazel SDK-only symbol (see
  [`fuchsia_...` and `Fuchsia...` files and symbols are in the Fuchsia Bazel SDK][fuchsia-prefix-sdk]).
  If `@rules_fuchsia` exposes a shared rule as part of the public Bazel SDK
  API, rename it with a `fuchsia_` prefix when loading and re-exporting it in
  `@rules_fuchsia`.
  * For example, see `component_common()` in
    [`components/component.bzl`][component-bzl] for the structural pattern.

### Providers

* Shared providers that `@rules_fuchsia` re-exports as part of the Fuchsia
  Bazel SDK use the `Fuchsia*Info` naming convention (for example,
  `FuchsiaPackageInfo`, `FuchsiaBindLibraryInfo`) and live in
  `<target_type>/providers.bzl`.
* Platform-only providers do **not** belong in `fuchsia_rules_common/`.
* Internal (not re-exported by `@rules_fuchsia`) and platform-only providers
  should not use `Fuchsia` (which implies the Bazel SDK) or `Fx` prefixes
  unless needed to avoid colliding with a built-in or upstream Bazel symbol or
  literally referring to the `fx` tool (such as `FxHostToolInfo`).

### `.bzl` file names

* Put shared providers in `<target_type>/providers.bzl`.
* Name other `.bzl` files after the target type or concept they implement
  without a `fuchsia_` prefix or redundant `_common` suffix (for example,
  `components/component.bzl`, `packages/package.bzl`,
  `packages/prebuilt_package.bzl`, and `assembly/product_configuration.bzl`).

<!-- Reference links -->

[bazel-style-guide]: /docs/development/languages/bazel/style/README.md
[build-flags-bzl]: build_flags/build_flags.bzl
[component-bzl]: components/component.bzl
[extract-common-skill]: .agents/skills/extract-common-bazel-sdk-functionality/SKILL.md
[fuchsia-prefix-sdk]: /docs/development/languages/bazel/style/common.md#fuchsia-files-and-symbols-are-in-the-fuchsia-bazel-sdk
[package-bzl]: packages/package.bzl
