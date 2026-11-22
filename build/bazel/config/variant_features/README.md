# Variant Features (`//build/bazel/config/variant_features/`)

This directory exposes active Fuchsia build variants and instrumentation modes
(ASAN, HWASan, UBSan, TSan, Coverage, Profile, ThinLTO, Fuzzer, Heapdump, etc.)
to `select()` expressions and Starlark rules as cycle-free Bazel
`feature_flag()` and `config_setting()` targets.

## Why `feature()` Targets Instead of `build_setting()` Targets?

In GN, build variants work by mutating the toolchain definition (`toolchain_variant.tags`
and `toolchain_variant.configs` in `//build/config/BUILDCONFIG.gn`), and individual
targets can check tag membership:

```gn
# Holds true if "instrumented" appears in toolchain_variant.tags
_is_instrumented =
    toolchain_variant.tags + [ "instrumented" ] - [ "instrumented" ] !=
    toolchain_variant.tags

config("foo") {
  if (_is_instrumented) {
    rustflags = [ "..." ]
  }
}
```

In Bazel, toolchain variants are intentionally modeled using Bazel toolchain
`feature()`s (enabled globally, per-target via `features = [...]`, or across
subtrees via Starlark feature transitions) rather than independent Skylib
`build_setting()` (`bool_flag()`) targets, for several reasons:

1. **Per-target and transitive disabling**: Toolchain `feature()`s support two
   distinct mechanisms depending on whether flags come from `cc_toolchain` or
   from `select()` over `variant_features/`:
   - **Target-local `features = [...]`**: Disables `cc_toolchain` features for
     a single target via `cc_common.configure_features()` (for example,
     `features = ["-clang_sanitizer", "-instrumented"]` in
     `sanitizer_default_options` or the legacy non-transition
     `disable_instrumentation` attribute on `fx_cc_*()`). Because
     `features = [...]` does not transition the build configuration or populate
     `ctx.disabled_features` on dependencies, it does **not** flip
     `select()` conditions on `variant_features/`.
   - **Command-line `--features` and Starlark transitions**: Transitions that
     rewrite `//command_line_option:features` (such as
     `transitive_features` in `@fuchsia_rules_common//:transition_utils.bzl`,
     `//src/starnix/containers/linux_resource.bzl`, or future transitions for
     GN's `exclude_toolchain_tags`) update both `cc_toolchain` and
     `feature_flag()` / `select()` evaluation across the entire dependency
     subtree.
2. **Single source of truth between toolchain and flags**: Both the Clang
   toolchain (`sanitizer.bzl`, `toolchain_utils.bzl`) and `build_flags()`
   `select()` predicates read from the same active feature set, guaranteeing
   compiler/linker behavior (such as dynamic-linker `ld.so.1` paths) stays
   synchronized with build flag selections.
3. **Cycle-free evaluation**: Standard `cc_toolchain` feature queries cannot be
   used inside `select()` expressions on default `build_flags()` targets without
   creating a dependency cycle (`default_build_flags` -> `cc_toolchain` ->
   `default_build_flags`). `feature_flag()` inspects `ctx.features`,
   `ctx.disabled_features`, and
   `@fuchsia_rules_common//build_flags:enabled_features` directly without
   depending on `cc_toolchain`.

## Scope: `variant_features/` vs. General `cc_toolchain` Features

This package **only** contains the 20 build-variant and instrumentation features
listed in `_SUPPORTED_FEATURES` in
`//build/bazel_sdk/fuchsia_rules_common/build_flags/feature_flag.bzl`.

General `cc_toolchain` features in
`//build/bazel_sdk/bazel_rules_fuchsia/common/toolchains/clang/toolchain_utils.bzl`
(such as `ml_inliner`, `no_dotd_file`, `pic`, `opt`, `dbg`,
`clang_driver_mode_cxx`, `runtime_library_search_directories`, or
`generate_linkmap`) are intentionally excluded here--even if forwarded through
`enabled_features` (like `ml_inliner` in `bazel_args.gni`)--because they are
not toolchain build variants.

## Special Naming: `"thinlto"` vs. `"thin_lto"`

Upstream Bazel, `@rules_cc`, `@rules_rust`, and `@rules_go` reserve the feature
name `"thin_lto"` (**with an underscore**) for Bazel's built-in distributed
ThinLTO pipeline, which triggers hardcoded `lto_indexing` and `lto_backend`
actions and altered bitcode outputs in `@rules_cc`.

Fuchsia intentionally names its feature `"thinlto"` (**without an underscore**)
so enabling ThinLTO applies Fuchsia's own `build_flags()` and linker-driven
ThinLTO options without triggering upstream Bazel's ThinLTO-indexing paths.

(Note also that upstream `@rules_cc` treats `"coverage"` as a known feature name
that sets `BUILD_COVERAGE_ENABLED=1` in stamped C++ linkstamps, while Fuchsia's
Clang toolchain defines `"coverage"` explicitly in `toolchain_utils.bzl` to
provide `-fprofile-instr-generate`, `-fcoverage-mapping`, and
`coverage/ld.so.1`.)

## GN Flag Propagation (`bazel_args.gni` Workaround for `bazel#22453`)

Because passing multiple `--features=<name>` flags on the Bazel command line
triggers https://github.com/bazelbuild/bazel/issues/22453,
`//build/bazel/config/bazel_args.gni` does not emit `--features=...`. Instead,
it joins active feature names with commas into a single string list flag:

```text
--@fuchsia_rules_common//build_flags:enabled_features=<comma-separated-list>
```

Both `cc_toolchain` (when constructing its default-enabled `feature()`s in
`toolchain_utils.bzl` and `sanitizer.bzl`) and `feature_flag()` read this build
setting alongside `ctx.features` and `ctx.disabled_features`.

## Mapping from GN `toolchain_variant.tags` & `configs` to Bazel Features

The 20 Bazel variant features in `//build/bazel/config/variant_features/` are
**not** a 1:1 copy of `all_variant_tags` in
`//build/toolchain/variant_tags.gni`.

In GN, some variants are only distinguishable via `toolchain_variant.configs`
or tag combinations rather than a unique tag name (for instance, `lto` and
`thinlto` share `"lto"`, `profile` and `profile-rust` share `"profile"`, and
`profile-use` / `profile-use-rust` carry only `"instrumented"`), while some GN
variant tags apply conflicting semantics in Clang (for example, GN's `hwasan`
variant sets `"asan"` and `"lsan"` alongside `"hwasan"`).

### Supported Features (`_SUPPORTED_FEATURES`)

| Bazel Feature | GN Source (`//build/bazel/config/bazel_args.gni`) | GN Variant(s) (`BUILDCONFIG.gn`) | Bazel Toolchain `feature()` |
| :--- | :--- | :--- | :--- |
| `asan` | `"asan"` in `tags` and `"hwasan"` not in `tags` | `asan`, `asan-ubsan`, `asan-sancov`, `asan-fuzzer`, `kasan`, `kasan-sancov` | `sanitizer.bzl` |
| `cfi` | `"cfi"` in `tags` | `lto-cfi`, `thinlto-cfi` | `toolchain_utils.bzl` (marker) |
| `coverage` | `"coverage_clang"` in `tags` and `//build/config/profile:coverage-cts` not in `configs` | `coverage` (and Rust when `"coverage_rust"` also set via `--config`) | `toolchain_utils.bzl` |
| `coverage-cts` | `//build/config/profile:coverage-cts` in `configs` | `coverage-cts` (has `"coverage_clang"` tag in GN) | `toolchain_utils.bzl` (marker) |
| `coverage-rust` | `"coverage_rust"` in `tags` and `"coverage_clang"` not in `tags` | `coverage-rust` (unified `coverage` uses `"coverage"` + `--config`) | `toolchain_utils.bzl` |
| `fuzzer` | `"fuzzer"` in `tags` | `asan-fuzzer`, `ubsan-fuzzer` | `toolchain_utils.bzl` (marker) |
| `heapdump` | `"heapdump"` in `tags` | `heapdump` | `toolchain_utils.bzl` (marker) |
| `hwasan` | `"hwasan"` in `tags` (suppresses `asan`) | `hwasan`, `hwasan-ubsan` (have `"asan"` + `"lsan"` tags in GN) | `sanitizer.bzl` |
| `instrumented` | `"instrumented"` in `tags` | All sanitizer, coverage, profile, profile-use, and heapdump variants | `toolchain_utils.bzl` (marker) |
| `lsan` | `"lsan"` in `tags` | `asan*`, `hwasan*` (or standalone `"lsan"`) | `sanitizer.bzl` |
| `lto` | `"lto"` in `tags` | `lto`, `thinlto`, `lto-cfi`, `thinlto-cfi` | `toolchain_utils.bzl` (marker) |
| `msan` | `"msan"` in `tags` | Reserved in Bazel; not currently in `variant_tags.gni` | `sanitizer.bzl` |
| `profile` | `//build/config/profile` in `configs` | `profile` (disambiguated from `profile-rust`, which shares `"profile"` tag) | `toolchain_utils.bzl` |
| `profile-rust` | `//build/config/profile:profile-rust` in `configs` and `//build/config/profile` not in `configs` | `profile-rust` (also emits `--config={fuchsia,host}_rust_profile`) | `toolchain_utils.bzl` |
| `profile-use` | `//build/config/profile:profile-use` in `configs` | `profile-use` (GN variant only sets `"instrumented"` tag) | `toolchain_utils.bzl` (marker) |
| `profile-use-rust` | `//build/config/profile:profile-use-rust` in `configs` | `profile-use-rust` (GN variant only sets `"instrumented"` tag) | `toolchain_utils.bzl` (marker) |
| `sancov` | `"sancov"` in `tags` | `ubsan-sancov`, `asan-sancov`, `kasan-sancov` | `sanitizer.bzl` |
| `thinlto` | `//build/config/lto:thinlto` in `configs` | `thinlto`, `thinlto-cfi` (GN sets `"lto"` tag alongside this config) | `toolchain_utils.bzl` (marker) |
| `tsan` | `"tsan"` in `tags` | `tsan` | `sanitizer.bzl` |
| `ubsan` | `"ubsan"` in `tags` | `ubsan`, `ubsan-sancov`, `ubsan-fuzzer`, `asan-ubsan`, `hwasan-ubsan`, `kubsan` | `sanitizer.bzl` |

Note on composite GN variants: Variants such as `asan-ubsan`, `asan-sancov`,
`asan-fuzzer`, `ubsan-sancov`, `ubsan-fuzzer`, `hwasan-ubsan`, `lto-cfi`, and
`thinlto-cfi` are automatically represented by enabling the union of their
component Bazel features (e.g. `asan-ubsan` enables `asan`, `lsan`, `ubsan`, and
`instrumented`; `thinlto-cfi` enables `lto`, `thinlto`, and `cfi`).

### GN Tags Not Forwarded as Separate Bazel Features

Several tags in `//build/toolchain/variant_tags.gni` are intentionally omitted
from `variant_features/`:

- **GN variant selection / linker property tags**: `instrumentation-runtime`,
  `custom-runtime`, `needs-compiler-abi`, `needs-writable-globals`,
  `replaces-allocator`, `uses-shadow`, `uses-shlibs`, `debugdata`,
  `llvm-profdata`, `kernel-only`, `kernel-excluded`, `fuchsia-only`,
  `fuchsia-excluded`. These express GN toolchain-selection constraints or
  low-level properties rather than user-facing Bazel features.
- **`gcc`**: Zircon kernel GCC toolchains are GN-only; Bazel builds Clang only.
- **Disaggregated `coverage` and `profile` tags**: Rather than forwarding the
  generic `"coverage"` and `"profile"` GN tags raw, `bazel_args.gni` routes
  them to `"coverage"`, `"coverage-cts"`, `"coverage-rust"`, `"profile"`, and
  `"profile-rust"` based on `coverage_clang`, `coverage_rust`, and
  `toolchain_variant.configs`. Use `//build/bazel/config/select/is_coverage` and
  `//build/bazel/config/select/is_profile` when selecting on any coverage or
  any PGO collection mode.

## Package Layout

- **`//build/bazel/config/variant_features/BUILD.bazel`**:
  Declares one `feature_flag()` target per supported feature with
  `visibility = ["//:__subpackages__"]`, kept in sync with
  `_SUPPORTED_FEATURES` via `LINT.IfChange(supported_variant_features)`.
- **`//build/bazel/config/variant_features/<feature>/BUILD.bazel`**:
  Defines `config_setting()` predicates `:true`, `:false`, and
  `alias(name = "<feature>", actual = ":true")` (all with
  `visibility = ["//:__subpackages__"]`) matching against `flag_values` on
  `//build/bazel/config/variant_features:<feature>`.

## Checklist: Adding a New Variant or Tag

When adding a new GN variant or variant tag that Bazel targets or `build_flags`
need to query:

1. **GN definition**: Add any new tag string to `all_variant_tags` in
   `//build/toolchain/variant_tags.gni` and define the variant in
   `known_variants` (`//build/config/BUILDCONFIG.gn`). Avoid choosing a Bazel
   feature name reserved by upstream Bazel (such as `"thin_lto"`).
2. **GN -> Bazel forwarding**: Update `//build/bazel/config/bazel_args.gni` to
   detect the tag in `toolchain_variant.tags` and/or config label in
   `toolchain_variant.configs` and append the Bazel feature name to
   `_enabled_features`.
3. **`feature_flag()` allowlist**: Add the feature name (sorted alphabetically)
   to `_SUPPORTED_FEATURES` in
   `//build/bazel_sdk/fuchsia_rules_common/build_flags/feature_flag.bzl`.
4. **Target definitions**: Add the `feature_flag()` target to
   `//build/bazel/config/variant_features/BUILD.bazel` and create
   `//build/bazel/config/variant_features/<feature>/BUILD.bazel` (`:true`,
   `:false`, and `:<feature>`, all with `visibility = ["//:__subpackages__"]`).
5. **Toolchain registration**: Ensure a `feature(name = "<feature>", ...)` is
   created in
   `//build/bazel_sdk/bazel_rules_fuchsia/common/toolchains/clang/sanitizer.bzl`
   or in the marker list in `toolchain_utils.bzl` (`known_feature_names`
   validation fails analysis if `enabled_features` contains an unregistered
   feature name). If the feature represents compiler instrumentation, also
   update `//build/bazel/rules/cc/instrumentation.bzl`
   (`_INSTRUMENTATION_FEATURES`) and
   `//src/starnix/containers/linux_resource.bzl` (`_INSTRUMENTATION_FEATURES`),
   plus any `variant_config_settings` if it introduces a new runtime sysroot
   directory.

## Usage Example

```starlark
load("@fuchsia_rules_common//build_flags:build_flags.bzl", "build_flags")

build_flags(
    name = "foo",
    rustflags = select({
        "//build/bazel/config/variant_features/instrumented:true": ["-Cpasses=..."],
        "//conditions:default": [],
    }),
    visibility = ["//:__subpackages__"],
)
```
