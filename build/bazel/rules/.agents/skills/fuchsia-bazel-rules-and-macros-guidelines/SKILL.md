---
name: fuchsia-bazel-rules-and-macros-guidelines
description: >
  Fuchsia Starlark rules, symbolic and legacy macros, configurable select
  attributes, depset and action performance, macro visibility, and AI
  antipattern rules for .bzl files. Use when authoring, modifying, or code-
  reviewing rules, macros, providers, or helper functions in .bzl files in the
  Fuchsia platform build (including //build/bazel/rules/... and
  //build/bazel_sdk/fuchsia_rules_common/...). Do NOT use for BUILD.bazel-only
  changes without .bzl edits, vendored or upstream third-party Bazel files, or
  as a standalone review orchestrator (use /review). For //build/bazel_sdk/,
  see build/bazel_sdk/.agents/skills/fuchsia-bazel-sdk-guidelines/SKILL.md.
---

# Fuchsia Bazel rules, macros, and Starlark functions

Apply these rules when authoring, modifying, or reviewing Starlark `rule()`,
`macro()`, `provider()`, legacy `def` macros, and helper functions in `.bzl`
files in the Fuchsia platform build (especially under `//build/bazel/rules/...`
and shared implementations in `//build/bazel_sdk/fuchsia_rules_common/...`).
This skill distills [Defining Bazel rules, macros, and functions][rules-macros]
alongside official Bazel `.bzl` style and performance rules, and complements
[fuchsia-bazel-guidelines](/build/bazel/.agents/skills/fuchsia-bazel-guidelines/SKILL.md)
and
[fuchsia-bzl-file-guidelines](/build/bazel/.agents/skills/fuchsia-bzl-file-guidelines/SKILL.md).

- Files under `//build/bazel_sdk/` (outside `fuchsia_rules_common/`) follow
  separate SDK conventions covered by
  [fuchsia-bazel-sdk-guidelines](/build/bazel_sdk/.agents/skills/fuchsia-bazel-sdk-guidelines/SKILL.md).

## Scope, directory layout, and dependency boundaries

- **Keep `BUILD` files DAMP and avoid area-specific macros:** Prefer existing
  rules and macros, shared static `deps`/`srcs` lists, `tags`, Bazel queries, or
  SHAC checks over introducing area-specific macros that hide targets from
  engineers, aspects, and `bazel2gn`. Never modify shared `.bzl` rules or macros
  inside a `[bazel_migration]` target CL; land rule/macro changes in a dedicated
  `[bazel][<area>]` CL reviewed by `//build/bazel/BAZEL_OWNERS`.
- **Subdirectory organization under `//build/bazel/rules/`:**
  - Place new `.bzl` files in domain subdirectories (such as `rules/testing/`,
    `rules/bind/`, `rules/cc/`, `rules/rust/`, `rules/packages/`) rather than
    adding loose files directly at the top level of `//build/bazel/rules/`.
  - Do **not** create separate `rules/` and `macros/` directories; keep rules
    and macros together in `rules/<area>/` so refactoring between a macro and a
    rule preserves `.bzl` load paths, and place rule/macro tests in
    `rules/<area>/tests/`.
- **Non-SDK vs. `@fuchsia_rules_common` placement:**
  - Rules under `//build/bazel/rules/...` must never `load()` or depend on
    `@rules_fuchsia` or `@fuchsia_sdk`.
  - Only place providers, helpers, and scripts in
    `//build/bazel_sdk/fuchsia_rules_common/` when they are genuinely shared
    with `@rules_fuchsia` (the SDK). Keep in-tree-only providers under
    `//build/bazel/rules/<area>/` without a `Fuchsia` or `Fx` prefix unless
    colliding with a common Bazel name or literally referring to the `fx` tool
    (such as `FxHostToolInfo`). (`Fuchsia*` implies SDK membership.)

## Symbolic macros, inherit_attrs, and native.*

- **Prefer symbolic `macro()` over legacy `def` macros:** Symbolic macros
  type-check attributes, evaluate labels relative to the caller, document
  attributes explicitly, and support `inherit_attrs`.
- **Implementation signature and subtarget naming:**
  - Name `rule()` and `macro()` implementation functions `_<name>_impl`. A
    symbolic macro's `_<name>_impl` must accept `(name, visibility, ...)` (plus
    `**kwargs` when using `inherit_attrs`) and must **never** specify default
    parameter values in `def _<name>_impl(...)` (`attrs` supplies defaults).
  - Every target created inside a symbolic `macro()` must be named `name` or
    begin with `name` followed by `_`, `.`, or `-` (such as `name + ".impl"` or
    `name + "_manifest"` -- **never** a leading `"_" + name`, which violates
    symbolic macro namespace scoping and breaks lazy evaluation).
- **Avoid `native.<lang>_*` in `.bzl` files and `inherit_attrs`:** Load Starlark
  symbols from their rulesets instead of deprecated `native.*` wrappers (for
  example, load `@rules_cc//cc:cc_rules_extension_support.bzl` for
  `inherit_attrs` on C++ symbolic macros rather than `native.cc_library` or
  `native.cc_binary`).
- **Forward `**kwargs` only to the matching `inherit_attrs` target:** Never pass
  `**kwargs` to secondary helper rules whose attribute schema differs from
  `inherit_attrs`; otherwise callers passing `testonly`, `size`, `timeout`,
  `flaky`, or `data` fail target loading with duplicate or unknown attribute
  errors.

## Configurable attributes and select() in symbolic macros

Attributes in symbolic `macro()` definitions -- **including attributes inherited
via `inherit_attrs`** -- are configurable (`select()`-aware) by default. Bazel
wraps every non-`None` resolved value (including non-`None` defaults such as
`False`, `""`, or `[]`) into an always-truthy `SelectorList` before calling
`implementation`. Only configurable attributes whose default is `None`
(`attr.label()` with default `None`, or non-mandatory attributes inherited via
`inherit_attrs` without a non-`None` override) arrive as `None` when omitted.

**Never perform concrete-value operations on configurable attributes** (SHAC
error):

- **No truthiness checks:** Never write `if attr:`, `not attr`, `attr or x`, or
  `bool(attr)`. Use `attr == None` or `attr != None` only for `None`-defaulted
  attributes (on `attr.bool()`, `attr.string()`, or `attr.string_list()`, `attr
  == None` is always `False`).
- **No container literals or iteration:** Never write `[attr]`, `{"k": attr}`,
  `for x in attr:`, `len(attr)`, or `list(attr)`. Concatenate lists with `+` and
  merge dicts with `|`.
- **No value comparisons, formatting, methods, or non-configurable forwarding:**
  Never compare `attr == "foo"`, format `"%s" % attr` / `"{}".format(attr)`,
  call methods like `attr.split("/")`, or pass `attr` to non-configurable
  attributes (`name`, `tags`, `testonly`, `visibility`).
- **Override with `configurable = False` when concrete inspection is needed:**
  Declare (or override an `inherit_attrs` attribute such as `linkshared` or
  `shared_lib_name` in `attrs` with) `configurable = False` whenever `_impl`
  must inspect, branch on, iterate over, format, or forward it to a
  non-configurable attribute.

**Good:**

```starlark
def _fx_example_impl(name, visibility, srcs, extra_deps, mode, **kwargs):
    helper_name = name + ".helper"
    _helper_rule(
        name = helper_name,
        srcs = srcs,
    )
    _underlying_rule(
        name = name,
        visibility = visibility,
        deps = [":" + helper_name, "//src/lib/foo"] + extra_deps,
        is_fast = (mode == "fast"),
        **kwargs
    )

fx_example = macro(
    doc = "Defines an example target with standard platform dependencies.",
    implementation = _fx_example_impl,
    inherit_attrs = _underlying_rule,
    attrs = {
        "srcs": attr.label_list(allow_files = True, doc = "Source files."),
        "extra_deps": attr.label_list(doc = "Additional dependencies."),
        "mode": attr.string(
            default = "standard",
            configurable = False,
            doc = "Execution mode ('standard' or 'fast').",
        ),
    },
)
```

## Visibility inside macros

- **Symbolic macros:** Targets instantiated inside a symbolic `macro()` resolve
  default visibility and `deps` visibility against the package containing the
  `.bzl` file, and helper targets default to `["//visibility:private"]`. Forward
  `visibility` to the primary target (`name = name`) and any documented public
  companion targets.
- **Legacy `def` macros:** Legacy macros expand inline in the caller's
  `BUILD.bazel` package. Every internal helper target defined by a legacy macro
  **must** explicitly set `visibility = ["//visibility:private"]` so it never
  inherits the caller package's `default_visibility`.

## Naming, attributes, error handling, and providers

- **Identifier casing:** Name providers in `UpperCamelCase` ending with `Info`
  (such as `BindLibraryInfo`), constants in `UPPER_SNAKE_CASE`, and rules,
  macros, functions, and variables in `lower_snake_case`.
- **Standard attribute names and file-allowing parameters:** Use `deps`, `srcs`
  (or `src`), `data`, and `tools` rather than GNisms like `public_deps` or
  `source`, or custom names like `rust_deps`, `images`, or `scripts`. Use
  `allow_single_file = True` (or `[".ext"]`) on `attr.label()` when reading
  `ctx.file.<attr>`, and `allow_files = True` (or `[".ext"]`) on
  `attr.label_list()` when reading `ctx.files.<attr>`.
- **Keyword-only functions and explicit macro parameters:** Generally place
  arguments on public functions and legacy `def` macros after `*,` (short
  functions with a few unambiguous positional arguments may omit `*`, while all
  boolean or defaulted arguments must always follow `*,`). Declare used macro
  arguments explicitly in the signature rather than calling
  `kwargs.get("testonly", False)`, and forward declared common attributes to
  inner targets when removed from `**kwargs`.
- **Bind sibling target names to variables:** Store intermediate target names in
  a local variable passed to both `name =` and `deps = [":" + var]`.
- **Prefix platform wrappers with `fx_`:** Wrap built-in or upstream symbols
  with `fx_` (`fx_cc_library`, `fx_package`), never `fuchsia_` (`rustc_*` is the
  established Rust exception). Update the `bazel_fuchsia_wrappers` SHAC check
  per `//build/bazel/rules/{cc,rust}/README.md` when adding C++ or Rust
  wrappers.
- **Fail loudly and forward complete providers:** Call `fail(...)` with a clear
  diagnostic on invalid or unsupported attribute combinations rather than
  silently ignoring arguments, and never commit `print()` statements. In wrapper
  rules (such as C++ wrappers), forward auxiliary providers (`OutputGroupInfo`,
  `InstrumentedFilesInfo`, etc.) alongside primary providers (`CcInfo`) rather
  than dropping them, and avoid duplicating `.so` entries already placed in
  `default_runfiles` by `@rules_cc`.

## Starlark rule performance, tools, docstrings, and antipatterns

- **`depset` discipline:** Never call `depset.to_list()` in non-terminal rules,
  merge `depset`s with `+`, or call `depset()` inside a loop ($O(N^2)$ analysis
  memory/CPU). Collect transitive `depset`s in a list and merge once via
  `depset(direct = [...], transitive = [...])`.
- **`ctx.actions.args()` for command lines:** Construct action arguments with
  `ctx.actions.args()` (`args.add()`, `args.add_all()`, `map_each`, and
  `args.use_param_file()` when large), passing `File` objects and `depset`s
  directly instead of building `[f.path for f in ...]` lists during analysis.
- **Pass tools as executable labels and avoid transitions:** Declare tools via
  `attr.label(executable = True, cfg = "exec")` and read `ctx.executable._tool`
  (preferring native Bazel tool targets over `@gn_targets//`). Do not add
  configuration transitions or new platforms without Build team approval.
- **Comments and `doc` strings:** Document public functions with comments, and
  provide `doc` strings on every `rule()`, `macro()`, `provider()`, and public
  attribute (private `_` attributes may omit `doc` when obvious from `default`).
  Keep single-sentence `doc = "..."` on one line; format multiline `doc` strings
  with text starting on the `doc = """` line, continuation lines aligned under
  `d` in `doc`, and closing `"""` on its own aligned line.
- **Reject AI-generated Starlark antipatterns:** Never look up tools via `PATH`,
  pass target labels inside JSON/dict strings instead of `attr.label*` /
  providers, resolve outputs via `bazel-bin` symlinks, rely on upstream private
  paths like `rules_cc`'s `_solib_`, or cache/remote huge artifacts without
  disabling remote cache per [bazel_disk_cache.md][disk-cache].

## Verification workflow

Format, run SHAC checks, and **explicitly run `fx bazel test`** on rule tests:

```posix-terminal
fx format-code
fx host-tool shac check
fx bazel test //build/bazel/rules/<area>/tests:...
fx build
```

- **Critical testing gotcha:** Analysis tests and rule tests included in `fx
  build` targets currently **do not fail `fx build` when assertions fail** and
  are **not executed by `fx test`**. Always invoke `fx bazel test` (adding
  `--config=host` for host-only rule tests) directly to verify rule and macro
  changes.

[rules-macros]: /docs/development/languages/bazel/style/rules_macros.md
[disk-cache]: /docs/development/build/bazel_disk_cache.md#avoiding-caching-large-artifacts
