<!--* freshness: { reviewed: '2026-10-20' } *-->

# bazel2gn

## System Overview

`bazel2gn` syncs build targets from `BUILD.bazel` files to `BUILD.gn` files in
`fuchsia.git`. During the GN-to-Bazel migration, many targets must build in both
GN and Bazel while downstream GN targets still depend on them. `bazel2gn` lets
developers maintain the single source of truth in `BUILD.bazel` and
automatically generates the corresponding GN targets below a sentinel line in
`BUILD.gn`.

The root Go package (`package bazel2gn`) parses Starlark syntax trees and
converts rules, attributes, conditions, and comment annotations into GN
fragments. The command-line entry point in [./cmd/main.go](./cmd/main.go)
orchestrates `buildifier` formatting, batch directory discovery, sentinel
preservation, and `gn format` execution. For contributor guidelines and package
invariants, see [AGENTS.md](./AGENTS.md).

## GN to Bazel migration instructions

### To migrate new targets from foo/bar/BUILD.gn to Bazel

1.  Create `foo/bar/BUILD.bazel` if it doesn't exist already.
2.  Implement the target you want to migrate in `foo/bar/BUILD.bazel`.
3.  Run `fx bazel2gn -d foo/bar` to sync the targets defined in
    `foo/bar/BUILD.bazel` to `foo/bar/BUILD.gn`.
4.  Remove old GN targets you've migrated from `foo/bar/BUILD.gn`.

`BUILD.gn` files containing the `bazel2gn` sentinel are automatically verified
to be in sync with their `BUILD.bazel` files by `shac` (`fx lint` and CQ).

### To re-sync targets managed by `bazel2gn`

*   Run `fx bazel2gn`, which automatically discovers and syncs all `BUILD.gn`
    files containing the `bazel2gn` sentinel.
*   Or, run `fx bazel2gn -d path/to/dir`, if you only want to sync targets in a
    specific directory.

### To maintain visibility allowlists

When a target with a `visibility` allowlist is migrated, the allowlist moves to
`BUILD.bazel` and uses Bazel syntax (`//foo:__pkg__`, `//foo:__subpackages__`).
bazel2gn derives the GN list from it, so the Bazel list must cover the
consumers in both graphs. To find stale entries, run:

```posix-terminal
build/bazel/scripts/trim_visibility.py --target=//path/to:target
```

The script finds direct consumers with `fx bazel query 'rdeps(...)'` and
`fx gn refs`, then prints the entries still needed, the unused entries, and a
trimmed list ready to paste. (`scripts/gn/trim_visibility.py` is deprecated
and forwards to this script.)

If the Bazel and GN lists differ on purpose, keep the GN list in a
`# @bazel2gn:raw_overwrite:` annotation on `visibility`. The script then trims
the Bazel list against Bazel consumers and the GN list against GN consumers.

## Design ideas

There are two main sources of differences between `BUILD.gn` and `BUILD.bazel`:

*   Differences between GN templates and Bazel rules for the same target;
*   Differences between Starlark and GN languages.

Examples of these differences and ideas for tackling them during conversion are
discussed in the sections below. For implementation details, refer to the Go
sources in this directory.

### GN templates vs Bazel rules

It is common for GN templates and Bazel rules to have different attributes for
the same target type. Some are simple naming differences, for example `srcs` in
Bazel rules is usually spelled `sources` in GN templates. This is converted with
hardcoded mappings in [constants.go](./constants.go). Others are more
complicated; for example, `go_library` from `rules_go` in Bazel requires sources
for [embed][goembed] to be specified in [`embedsrcs`][embedsrcs], whereas
`go_library` in GN mixes embed sources with other sources in `sources`.

#### C++ header dependencies (`deps`, `implementation_deps`) and `check_includes_strict`

GN's `check_includes_strict = true` enforces direct header dependencies:

*   `deps` in Bazel C++ library rules maps to `public_deps` in GN (headers
    `#include`d by the target's own public `hdrs`), while `implementation_deps`
    maps to `deps` in GN (headers `#include`d only by `srcs`).
*   With `check_includes_strict = true` in GN, `public_deps` does not allow
    downstream consumers to `#include` transitive headers without declaring a
    direct dependency on the target providing the header.
*   When unmigrated GN consumers still rely on transitive `#include`s through an
    umbrella or transitional library, annotate the target in `BUILD.bazel` with
    `# @bazel2gn:raw_attribute:check_includes_strict = false` (accompanied by a
    `TODO` referencing the cleanup bug) until those GN consumers declare direct
    dependencies or migrate.

To bridge this gap, modifying the template implementation in GN while
maintaining backward compatibility is preferred because:

*   The build team owns most of the GN templates, making them easier to change;
*   GN targets will eventually be removed, so keeping Bazel targets idiomatic is
    preferred.

For example, `rules_rust` only allows Rust targets in `deps`, and native (e.g.
C/C++) libraries linked into Rust targets go in `link_deps` instead. The GN
`rustc_*` templates accept `link_deps` too (with the same semantics as `deps`),
so `bazel2gn` passes the attribute through unchanged, applying the same label
translations it applies to `deps`.

Conversely, C++ library rules (`cc_library()`, `fx_cc_library()`, and
`fx_cc_library_headers()`) map Bazel `deps` to GN `public_deps` and
`implementation_deps` to GN `deps`, because Bazel propagates the headers of
`deps` (but not `implementation_deps`) to dependents.

`fx_cc_library()` also has a `friend` attribute that Bazel ignores. It passes
through unchanged to GN `friend` (GN label patterns such as `":*"` or
`"//foo/*"`), which lets the listed targets include the library's private
headers in GN. Bazel has no such check (the Fuchsia Bazel C++ toolchain does
not enable `layering_check`).

`fx_cc_shared_library()` (from `//build/bazel/rules/cc:fx_cc_shared_library.bzl`)
maps to GN `shared_library()`. Both `deps` and `dynamic_deps` map to GN `deps`,
`user_link_flags` to `ldflags`, and `build_flags` / `disable_build_flags` to
`configs +=` / `configs -=`. The GN-only `gn_public_deps` and
`gn_public_configs` attributes, which Bazel ignores, map to GN `public_deps`
and `public_configs`. Headers don't propagate the same way in both builds: GN
dependents of a `shared_library()` get the headers and `public_configs` of its
`public_deps`, while Bazel dependents of a `cc_shared_library()` get no headers
from it (it only provides `CcSharedLibraryInfo` for linking, not `CcInfo`) and
depend on the library's headers themselves. Use `gn_public_deps` to keep forwarding a
headers target to existing GN dependents. `bazel2gn` sets `output_name`,
`output_extension`, and `output_prefix_override` so the GN output file name
matches the Bazel one (`shared_lib_name`, or `lib<name>.so` by default), and
fails on Bazel-only attributes such as `exports_filter`, `roots`, and
`transitive_features`. Note that Bazel links the direct `deps` of a
`cc_shared_library()` whole, while a GN `shared_library()` only links the
objects it references from a `static_library()`, so direct `deps` need
`alwayslink = True` (which converts to a GN `source_set()`) for GN to link the
same code. `bazel2gn` doesn't select the GN shared library toolchain: a
`BUILD.gn` file that must only be evaluated in `${default_toolchain}-shared`
keeps its own `assert()` above the sentinel.

#### `compiled_action()`

GN `compiled_action()` targets, which run a tool built in the same build, map to
`compiled_action()` from `//build/bazel/rules:compiled_action.bzl`:

```bzl
compiled_action(
    name = "generate",
    srcs = ["input.json"],
    outs = ["out.rs"],
    args = [
        "--input",
        "$(location :input.json)",
        "--output",
        "$(location :out.rs)",
        "--out-dir",
        "$(RULEDIR)",
    ],
    tool = ":generator",
)
```

is synced to:

```gn
compiled_action("generate") {
  sources = [ "input.json" ]
  outputs = [ "$target_gen_dir/out.rs" ]
  args = [
    "--input",
    rebase_path("input.json", root_build_dir),
    "--output",
    rebase_path("$target_gen_dir/out.rs", root_build_dir),
    "--out-dir",
    rebase_path(target_gen_dir, root_build_dir),
  ]
  tool = ":generator"
}
```

`outs` and `depfile` are placed in `$target_gen_dir` unless they have a
`path_overwrite` annotation. `args` only support `$(location <label>)` and
`$(execpath <label>)` (for `outs`, `srcs`, or `path_overwrite`-annotated
`deps`), and `$(RULEDIR)`; any other substitution is an error. `deps`,
`mnemonic`, `testonly`, `visibility`, and `tool_output_name` are forwarded as
well.

#### Large RBE workers

Rust targets that need a lot of memory to compile run on large RBE workers with
`exec_properties = RBE_USE_LARGE_WORKER` (from `//build/bazel/rbe:rbe.bzl`) in
Bazel, and use `concurrent_jobs.rust_highmem` (a large RBE worker, or the local
high memory pool without RBE) in GN:

```bzl
load("//build/bazel/rbe:rbe.bzl", "RBE_USE_LARGE_WORKER")

rustc_binary(
    name = "bin",
    srcs = ["src/main.rs"],
    exec_properties = RBE_USE_LARGE_WORKER,
)
```

is synced to:

```gn
rustc_binary("bin") {
  sources = [ "src/main.rs" ]
  _concurrent_jobs = concurrent_jobs.rust_highmem
  configs += _concurrent_jobs.configs
  forward_variables_from(_concurrent_jobs.vars, "*")
}
```

`exec_properties` is never emitted verbatim. Other values, other rules (only
`rustc_binary()`, `rustc_cdylib()`, `fuchsia_rust_dso()`, `rustc_library()`,
`rustc_staticlib()`, `rustc_test()` and their aliases are supported), and
overwrite annotations on `exec_properties` are errors; use `# @bazel2gn:skip`
and `# @bazel2gn:raw_attribute:` instead.

#### `rustc_cdylib()`, `fuchsia_cc_dso()`, and `fuchsia_rust_dso()`

Rust `cdylib` shared libraries (`rustc_cdylib()` from
`//build/bazel/rules/rust:rustc_cdylib.bzl`) and Fuchsia DSO component shared
libraries (`fuchsia_cc_dso()` and `fuchsia_rust_dso()` from
`//src/lib/dso:fuchsia_dso.bzl`) sync directly to their GN counterparts in
`//build/rust/rustc_cdylib.gni` and `//src/lib/dso/fuchsia_dso.gni`:

*   On `fuchsia_cc_dso()`, `is_async` (named `is_async` because `async` is a
    reserved keyword in Starlark) maps to GN `async`, and `linkopts` maps to GN
    `ldflags`.
*   On `rustc_cdylib()`, `hdrs` maps to GN `public`, and `public_deps` passes
    through.
*   To include a `rustc_cdylib()`, `fuchsia_cc_dso()`, or `fuchsia_rust_dso()`
    in an `fx_component()` (or converted `fx_test_component()`), wrap it with
    `fx_packaged_binary(binary = ":<dso>", binary_name = "lib<output_name>.so",
    install_root = "lib/")`. Because `"lib/lib<output_name>.so"` matches the GN
    shared library's default `distribution_entries` destination, `bazel2gn`
    unwraps `:<packaged_dso>` in component `deps` directly to `:<dso>`.
    (`fuchsia_cc_dso()` builds in GN's `$shlib_toolchain`, so non-default
    `fx_packaged_binary` destinations on `fuchsia_cc_dso()` are rejected.)

#### `cc_link_stub_from_ifs_file()`

`cc_link_stub_from_ifs_file()` from `//build/bazel/rules/cc:shared_library.bzl`
syncs to `ifs_shared_library()` from `//build/toolchain/ifs_shared_library.gni`.
Both only build a link stub from an `.ifs` file, for a shared library that is
provided at runtime by something else:

*   `ifs_file` maps to GN `abi`, and `deps` to GN `public_deps`, because they
    propagate to dependents like the `deps` of `cc_import()`.
*   `system_provided = True` is required, because GN never ships the stub as the
    runtime library.
*   The stub is named `<stub_name>.so`, or after the `.ifs` file if `stub_name`
    is not set, in Bazel, and `lib<output_name>.so` in GN. The name must start
    with `lib`, and `bazel2gn` sets GN `output_name` to the rest of it when that
    is not the target name.

#### Components

`cmc_merge()`, `fx_component_manifest()`, `fx_compiled_manifest()`, and
`fx_component()` sync to GN `cmc_merge()`, `fuchsia_component_manifest()`,
`cm()`, and `fuchsia_component()`:

*   `cmc_merge()` maps `srcs` (`.cml`, `.shard.cml`, or `.json` files,
    `cml_shard()` targets ending in `.shard`, or `:<merge>` targets defined in
    the same `BUILD.bazel`) to `sources` and defaults `output_name` to
    `<name>.cml` (matching the Bazel rule, whereas GN `cmc_merge()` defaults to
    `<name>`). When an `fx_component_manifest()`,
    `fx_component_manifest_with_expectations()`, `fx_compiled_manifest()`, or
    `cmc_merge()` consumes a `:<merge>` target defined in the same
    `BUILD.bazel`, `bazel2gn` rewrites `:<merge>` to
    `"$target_out_dir/<output_name>"` and adds `:<merge>` to `deps`.
*   `fx_component_manifest()` maps `manifest` to a GN path, passes
    `restricted_features` through, and omits `includes`
    (which lists `.cml` shard inputs for the Bazel action sandbox; GN runs `cmc`
    with global include paths and a depfile instead). An
    `fx_component_manifest()` referenced via `compiled_manifest` only by
    Bazel-only `fx_test_component()`s (see [Tests](#tests)) or skipped
    `fx_component()`s in the same `BUILD.bazel` (and not by any converted
    component, `fuchsia_structured_config_rust_lib()`, or
    `fuchsia_structured_config_cpp_elf_lib()`) is skipped
    automatically.
*   `fx_component()` maps `compiled_manifest` (which must refer to a
    `:<manifest>` target in the same `BUILD` file) to `cm_label`. If the
    `fx_component_manifest()` or `fx_compiled_manifest()` appears after a
    consumer (including `fuchsia_structured_config_rust_lib()` and
    `fuchsia_structured_config_cpp_elf_lib()`) in
    `BUILD.bazel`, `bazel2gn` hoists it earlier in `BUILD.gn` so GN's
    `get_target_outputs()` succeeds.
*   `fx_compiled_manifest()` maps to the low-level GN `cm()`, which compiles
    the manifest without adding it to a package as a resource (for a `.cm`
    file that is embedded in a binary or only read at build time). GN `cm()`
    has no `component_name`, so `bazel2gn` emits the `output_name` (default
    `cml/<name>/<component_name>.cm`) and `config_values_package_path`
    (`meta/<component_name>.cvf`) that the Bazel rule uses instead.
    `restricted_features` is passed through and `includes` is omitted. An
    `fx_component()` can't use an `fx_compiled_manifest()` as its
    `compiled_manifest`.
*   `fx_packaged_binary()` has no standalone GN equivalent, but when an
    `fx_component()` (or a converted `fx_test_component()`) depends on a
    `:<packaged_bin>` defined in the same
    `BUILD.bazel` wrapping an un-skipped binary or shared-object target in the
    same file:
    *   If `install_root + binary_name` matches the GN target's default
        distribution destination (`"bin/<output_name>"` for executables,
        including `fx_cc_test_binary()`, or
        `"lib/lib<output_name>.so"` for `rustc_cdylib()`, `fuchsia_cc_dso()`,
        and `fuchsia_rust_dso()`), `:<packaged_bin>` in `deps` unwraps directly
        to the underlying `:<binary>` target.
    *   Otherwise, `bazel2gn` emits a GN `renamed_binary("<packaged_bin>")`
        target and keeps `:<packaged_bin>` in `deps` (except for
        `fuchsia_cc_dso()`, which builds in `$shlib_toolchain` and only supports
        its default `"lib/lib<output_name>.so"` destination).

Put `# @bazel2gn:skip` (with a brief explanatory comment) on an `fx_component()`
when its package is Bazel-only and has no GN consumer (its
`fx_component_manifest()` will then be skipped automatically unless used by
another un-skipped target).

#### Package resources

`resource()` (and `fuchsia_package_resource()`), `resource_group()`,
`fuchsia_package_resource_group()`, `resource_tree()`, and
`fuchsia_package_resource_collection()` (from
`//build/bazel/rules/packages:resources.bzl` (and SDK rules from
`@fuchsia_rules_common//packages:resources.bzl`)) sync to GN's `resource()`,
`resource_group()`, `resource_tree()`, and `group()` (from
`//build/dist/resource.gni`):

*   `resource()` supports both `src` + `dest` (converted to single-element
    `sources` and `outputs` lists in GN) and `sources` +
    `outputs = [".../{{source_file_part}}"]`. If a `src` or `sources` entry
    refers to an output of a `compiled_action()` in the same `BUILD.bazel`,
    `bazel2gn` rewrites it to `"$target_gen_dir/<out>"` and adds the
    `compiled_action()` target to `deps`.
*   `resource_group()` with `files = [{"source": ..., "dest": ...}]` converts
    directly to GN `resource_group()`. `fuchsia_package_resource_group()` (or
    `resource_group()` with `srcs` and `dest`) converts to GN `resource()` with
    `outputs = ["<dest>/{{source_file_part}}"]` when `basename_only = True`, or
    expands `srcs`, `dest`, and `strip_prefix` into `files` on GN
    `resource_group()` otherwise.
*   `resource_tree()` syncs directly to GN `resource_tree()`.
*   `fuchsia_package_resource_collection()` syncs to GN `group()` with
    `resources` mapped to `deps`.
*   On C++ rules (`cc_library()`, `fx_cc_library()`, `cc_binary()`,
    `fx_cc_binary()`, and `fx_cc_test_binary()`), `data` maps to GN
    `data_deps`.

#### ICU and internationalization rules

ICU flavor-aware rules (`icu_source_set()`, `icu_executable()`,
`icu_fuchsia_component()`, `icu_fuchsia_package()`, `icu_group()`, and
`third_party_icu_headers()` from `//build/bazel/rules/icu:*`) and
`intl_strings()` (from `//build/bazel/rules/intl:intl_strings.bzl`) sync to
their GN counterparts in `//build/icu/*.gni` and `//build/intl/intl_strings.gni`:

*   `icu_source_set()` uses the same attribute mappings as `fx_cc_library()`
    (`srcs` to `sources`, `hdrs` to `public`, `deps` to `public_deps`,
    `implementation_deps` to `deps`), and passes `icu_deps` and
    `icu_public_deps` through with dependency label translation.
*   `icu_executable()` uses the same attribute mappings as `fx_cc_binary()`,
    and passes `output_name`, `icu_deps`, and `icu_public_deps` through.
*   `icu_fuchsia_component()` and `icu_group()` pass `icu_deps` and
    `icu_public_deps` through with dependency label translation
    (`icu_fuchsia_component()` also translates `manifest` / `compiled_manifest`
    and omits `includes`).
*   `icu_fuchsia_package()` merges `components` and `resources` into `deps`,
    `icu_components` and `icu_resources` into `icu_deps`, and translates
    `validate_elf_binaries` into `disable_elf_binaries_checks`.
*   `third_party_icu_headers()` passes `headers` and `icu_assembly` through.
*   `intl_strings()` converts `source` to a GN file path and passes
    `source_locale`, `output_locales`, `library`, and `same_dir_locales`
    through.

#### Starnix Linux binaries

Starnix Linux binaries and shared objects packaged into Fuchsia components use
standard binary rules (`fx_cc_binary()`, `fx_cc_test_binary()`, `cc_binary()`,
`rustc_binary()`) paired with `linux_resource()` (from
`//src/starnix/containers:linux_resource.bzl`), which syncs to GN's
`linux_resource()` template (from `//src/starnix/build/linux_resource.gni`):

*   On C++ binary rules (`fx_cc_binary()`, `cc_binary()`,
    `cc_binary_host_tool()`, and `idk_cc_binary_host_tool()`), `linkopts` maps
    to GN `ldflags`, `includes` maps to GN `include_dirs` (unlike C++ library
    rules, binaries have no downstream compilation dependents), and
    `linkshared = True` on `fx_cc_binary()` / `cc_binary()` maps to GN
    `loadable_module()`. `gn_output_name` on `fx_cc_binary()`
    / `fx_cc_test_binary()` is only used by GN: it maps to the GN `output_name` (the
    Bazel binary is always named after the target).
*   When a `linux_resource()` references a binary target in the same
    `BUILD.bazel` file that has no non-`linux_resource()` GN consumers,
    `bazel2gn` automatically wraps the binary in
    `if (is_linux && !is_host) { ... }` in `BUILD.gn` (unless an OS constraint
    such as `"//build/bazel/platforms:linux_flavor_starnix"` is already
    specified in `target_compatible_with`), and passes `bin_output_name` /
    `arch32_bin_output_name` to `linux_resource()` whenever the GN binary's
    output filename differs from its target name.
*   `arch32_src = select({"@platforms//cpu:arm64": ":<bin32>",
    "//conditions:default": None})` with `arch32_dest` syncs to an
    `if (target_cpu == "arm64") { ... }` block inside `linux_resource()`.

```bzl
fx_cc_binary(
    name = "hello_starnix_bin_bin",
    srcs = select({
        "@platforms//cpu:x86_64": ["hello_x64.S"],
        "@platforms//cpu:arm64": ["hello_aarch64.S"],
        "@platforms//cpu:riscv64": ["hello_riscv64.S"],
    }),
    disable_instrumentation = True,
    disable_syslog_backend = True,
    linkopts = [
        "-nostdlib",
        "-nostartfiles",
        "-static",
        "-Wno-unused-command-line-argument",
    ],
    tags = ["manual"],
)

linux_resource(
    name = "hello_starnix_bin",
    src = ":hello_starnix_bin_bin",
    dest = "data/tests/hello_starnix",
    visibility = ["//src/starnix/kernel/core:__pkg__"],
)
```

is synced to:

```gn
if (is_linux && !is_host) {
  executable("hello_starnix_bin_bin") {
    if (current_cpu == "x64") {
      sources = [ "hello_x64.S" ]
    } else if (current_cpu == "arm64") {
      sources = [ "hello_aarch64.S" ]
    } else if (current_cpu == "riscv64") {
      sources = [ "hello_riscv64.S" ]
    }
    exclude_toolchain_tags = [ "instrumented" ]
    disable_syslog_backend = true
    ldflags = [
      "-nostdlib",
      "-nostartfiles",
      "-static",
      "-Wno-unused-command-line-argument",
    ]
  }
}
linux_resource("hello_starnix_bin") {
  src = ":hello_starnix_bin_bin"
  dest = "data/tests/hello_starnix"
  visibility = [
    ":*",
    "//src/starnix/kernel/core:*",
  ]
}
```

#### Tests

While a library is used in both build systems, its tests run in both (see
[Testing libraries in both build systems](/docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems)),
so `bazel2gn` translates test rules too:

*   `fx_cc_test_binary()` (from
    `//build/bazel/rules/cc:fx_cc_test_binary.bzl`) maps to GN
    `cc_test_executable()` (from `//build/testing/cc_test_executable.gni`) with
    the same attribute mappings as `fx_cc_binary()` (except `linkshared`, which
    is not supported on test binaries). The GN target always gets
    `testonly = true` (`testonly = False` is an error) and
    `emit_tool_path = false`, and `disable_lto` passes through when explicitly
    set. Like the Bazel macro, it doesn't register a test. `bazel2gn` doesn't
    translate `host_cc_test()` or `host_test()` yet, so the GN copy of a C++
    host test is a hand-written GN `host_test()` that runs the generated
    binary, above the sentinel.
*   `rustc_test()` maps to GN `rustc_test()`. With `crate = ":<lib>"`, the
    crate must be defined in the same `BUILD.bazel`, and its `srcs`,
    `crate_root`, `edition`, `rustc_env`, `deps`, `proc_macro_deps`,
    `link_deps`, and `compile_data` are inlined into the GN test, like
    `rules_rust` does (including `fuchsia_rust_dso()`'s implicit DSO runtime
    dependencies and `public_deps` -> `link_deps` on `rustc_cdylib()`,
    `rustc_dylib()`, and `fuchsia_rust_dso()`). Generic undisambiguated test
    binary names (such as `lib_test` or `bin_test`) are rejected so test
    binaries from different packages do not collide in `${root_out_dir}`; name
    the test after the crate (or set `crate_name`, which maps to GN's
    `output_name`).
*   `host_rustc_test()` and `host_go_test()` map to GN `rustc_test()` and
    `go_test()` inside `if (is_host)`. `data`, `test_data`, and `test_label`
    are errors, because the GN templates have no equivalent.
*   `fx_unittest_package()` maps to `fuchsia_unittest_package()`, or to
    `fuchsia_unittest_component()`s in a `fuchsia_test_package()` when it has
    several `unit_tests`. The GN package name gets a `-gn` suffix so it doesn't
    collide with the Bazel package in the package repository. Only `name`,
    `package_name`, `unit_tests`, `resources`, `max_log_severity`,
    `min_log_severity`, `parallel`, `tags`, `testonly`, and `visibility` are
    supported (`parallel > 0` maps to `test_specs.parallel`, `0` omits it); any
    other attribute (such as `environments`) is an error.
*   An `fx_package()` with un-skipped `test_components` maps to
    `fuchsia_test_package()`, with the same `-gn` suffix on the package name.
    `test_components` (which must be a list literal) and `subpackages` are
    passed through, and `components` and `resources` map to `deps`. For package
    validations, `validate_elf_binaries` (`True` or `False`) maps to GN
    `disable_elf_binaries_checks` with the value inverted, and
    `validate_structured_config` (`True` or `False`) is passed through. Any
    other attribute (such as `tools`) is an error. The
    `fx_test()` in the same `BUILD.bazel` whose `package` is the `fx_package()`
    (at most one) sets the GN `test_specs`: `max_log_severity` and `min_log_severity` map to
    `log_settings.max_severity` and `log_settings.min_severity`, `parallel > 0`
    maps to `parallel` (`0` omits it), `build_only` is
    passed through, and `environments` must be a list of
    `//build/testing/environments:<env>` labels, which map to the `<env>`
    variables of `//build/testing/environments.gni`. GN only supports
    `test_type` on components, so `fx_test(test_type = ...)` is set on each test
    component instead.
*   An `fx_test_component()` listed in the `test_components` of such an
    `fx_package()` in the same `BUILD.bazel` maps to `fuchsia_test_component()`
    like `fx_component()` maps to `fuchsia_component()`, and also passes
    `test_type` through. A listed `fx_unittest_component()` maps to
    `fuchsia_unittest_component()` with `binary` in `deps` (only `name`,
    `binary`, `component_name`, `test_type`, `tags`, `testonly`, and
    `visibility` are supported). Any other same-package test component (such as
    a skipped one) is an error. Other `fx_test_component()`s and
    `fx_unittest_component()`s, `fx_package()`s without `test_components`, and
    `fx_test()` are ignored.
*   `ctf_fx_package()` (from `//sdk/ctf/build/ctf_fx_package.bzl`) maps to
    `ctf_fuchsia_package()`, which also defines the `<name>_archive` target
    that GN `ctf-tests` groups depend on. The GN package name does **not** get
    a `-gn` suffix, because CTF releases and the tests that subpackage a CTF
    package identify it by name; only the GN copy is published or released
    until the CTF release is built with Bazel. `package_name` defaults to
    `name`, `components` and `test_components` map to `deps`, and
    `subpackages`, `test_component_names`, and `visibility` are passed
    through. Same-package test components listed in `test_components` are
    converted like those of an `fx_package()` with `test_components` (see
    below), except that no `test_type` is added from an `fx_test()`. Any other attribute (such as
    `resources`) is an error.
*   `fx_component_manifest_with_expectations()` and `fx_expectations()` map to
    `fuchsia_component_manifest_with_expectations()` and
    `fuchsia_expectations()` in
    `//src/lib/testing/expectation/fuchsia_test_component_with_expectations.gni`
    (omitting `includes`), and `fx_generated_expectations()` maps to
    `generated_expectations_file()` in
    `//src/lib/testing/expectation/generated_expectations_file.gni`. An
    `fx_test_component_with_expectations()` listed in the `test_components` of a
    converted `fx_package()` in the same `BUILD.bazel` maps to
    `fuchsia_test_component_with_expectations()`: `compiled_manifest` maps to
    `cm_label`, `.json5` labels in `expectations` convert to GN paths while
    target labels (such as `:expectations` or `//pkg:expectations`) convert as
    GN labels (including inside `select()`), and
    `treatment_of_cases_with_error_logs` and `test_type` pass through. Unlisted
    `fx_test_component_with_expectations()` targets (and any
    `fx_component_manifest_with_expectations()` referenced only by them) are
    ignored.
*   The `with_unit_tests`, `with_host_unit_tests`, and `test_deps` attributes
    of Rust rules are passed through to the GN templates.
*   `test_suite()`, `wrap_host_rust_test()`, and the other `host_*_test()`
    rules have no GN equivalent and are ignored.

Put `# @bazel2gn:skip` (with a brief explanatory comment) on an `fx_package()`
with `test_components` when the test is Bazel-only (for example, because its
test binary is skipped) or when its GN copy must be written by hand; its
`fx_test_component()`s / `fx_test_component_with_expectations()`s and their
manifests are then ignored too.

#### Tags

Most Bazel `tags` have no GN equivalent and are ignored, except:

*   `assert_no_deps=<label>` is converted to GN `assert_no_deps`, see
    [`assert_no_deps.bzl`](/build/bazel/aspects/assert_no_deps.bzl).
    Comment annotations such as `# @bazel2gn:path_overwrite:` and
    `# @bazel2gn:raw_overwrite:` are forwarded onto the generated GN
    dependencies, allowing toolchain qualifiers (e.g. `($current_toolchain)`).
*   `exclude_toolchain_tags=<tag>` on Rust binary, cdylib/dylib/DSO, and test
    rules (and on library rules with `with_unit_tests`, where it only affects
    the unit test) is ignored by Bazel, and converted to GN
    `exclude_toolchain_tags`, so the GN target isn't built in toolchains
    (variants) with `<tag>`, such as `heapdump`. This is the Rust counterpart of
    `disable_instrumentation = True` on `fx_cc_binary()` / `fuchsia_cc_dso()`,
    which becomes `exclude_toolchain_tags = [ "instrumented" ]`.
    TODO(https://fxbug.dev/578022348): Add canonical Bazel support for excluding
    Rust targets from toolchain variants (e.g. heapdump).

```bzl
rustc_binary(
    name = "bin",
    srcs = ["src/main.rs"],
    tags = [
        "assert_no_deps=//third_party/rust_crates/vendor:regex",
        "assert_no_deps=//third_party/rust_crates/vendor:serde_json5",  # @bazel2gn:path_overwrite://third_party/rust_crates:serde_json5($current_toolchain)
        "exclude_toolchain_tags=heapdump",
    ],
)
```

is synced to:

```gn
rustc_binary("bin") {
  sources = [ "src/main.rs" ]
  assert_no_deps = [
    "//third_party/rust_crates:regex",
    "//third_party/rust_crates:serde_json5($current_toolchain)",
  ]
  exclude_toolchain_tags = [ "heapdump" ]
}
```

#### Python mypy

Bazel type checks `py_library()` and `py_binary()` targets with
[Pyrefly][pyrefly] through `pyrefly_aspect`, so `bazel2gn` adds
`enable_mypy = false` to their GN copies instead of type checking the same
sources again with [mypy][mypy]. Pyrefly skips targets under `third_party/` and
`prebuilt/`, and targets tagged `no-pyrefly`, so such targets are not type
checked by either build unless they override the default.
Without mypy, a GN `python_library()` also stops providing type information to
GN consumers that still run mypy, so they treat it as an untyped import. To keep
running mypy in GN, override the default with a `# @bazel2gn:raw_attribute:`
annotation:

```bzl
# @bazel2gn:raw_attribute:enable_mypy = true
py_library(
    name = "lib",
    srcs = ["lib.py"],
)
```

is synced to:

```gn
python_library("lib") {
  sources = [ "lib.py" ]
  enable_mypy = true
}
```

#### Python `imports`

`imports` of `py_library()` passes through unchanged to GN `python_library()`,
which stages sources the same way Bazel adds them to the import path: each
source is importable by its path relative to the closest directory in `imports`
that contains it (or relative to the root of the source tree if none does).
`srcs` therefore don't need `# @bazel2gn:path_overwrite:` annotations, and
`source_root` and `library_name` (which can't be used together with `imports`)
don't need `# @bazel2gn:raw_attribute:` annotations:

```bzl
py_library(
    name = "yaml",
    srcs = [
        "src/lib/yaml/__init__.py",
        "src/lib/yaml/composer.py",
    ],
    imports = ["src/lib"],
)
```

is synced to:

```gn
python_library("yaml") {
  sources = [
    "src/lib/yaml/__init__.py",
    "src/lib/yaml/composer.py",
  ]
  imports = [ "src/lib" ]
  enable_mypy = false
}
```

and both builds import it with `import yaml`. GN `python_binary()` doesn't
support `imports`, so `imports` on `py_binary()` is an error.

`py_data_package()` (from `//build/bazel/rules/python:py_data_package.bzl`)
stages `importlib.resources` data packages for Bazel Python targets and is
ignored by `bazel2gn`; while a `py_library()` still has GN consumers, skip its
`py_data_package()` `deps` entry and set `data_package_name` and `data_sources`
with `# @bazel2gn:raw_attribute:`.

#### Python C extensions

`python_c_extension()` (from `//build/bazel/rules/python:python_c_extension.bzl`)
maps to GN `python_c_extension()` inside `if (is_host && is_linux)`, converting
`srcs` to `sources` and passing `defines`, `deps`, `stubs`, `stubs_root`,
`testonly`, and `visibility` through.

### Starlark vs GN

[Starlark][starlark] is the language of Bazel for defining build targets, while
GN has its [own language][gnreference] for the same purpose. Starlark is more
declarative than GN. For example, to conditionally build a list in GN, you use
`if` statements to imperatively add elements to the list, while in Bazel this is
done by concatenating results returned by the `select()` function.

#### Conditional targets

Targets in the build graph are often conditionally defined. For example, host
tools should only be built for the host. In GN, this is expressed with `if`
clauses. In Bazel, because `if` clauses are not supported in `BUILD.bazel`
files, this is done by setting the `target_compatible_with` attribute when
defining targets. `bazel2gn` recognizes `target_compatible_with` attributes in
Bazel and converts their values into GN `if` conditions. For example,
`target_compatible_with = HOST_CONSTRAINTS` in Bazel maps to `if (is_host) { ...
}` in GN. Because `target_compatible_with` is a list of constraints that must
all hold, multiple constraints are converted into GN conditions joined with
`&&`.

#### Conditional values

Bazel attributes can be conditionally assigned across platforms using
`select()`.

NOTE: Not all attributes support `select()` values. Consult the rule's
documentation for details.

```bzl
rust_library(
    name = "list_concatenation",
    srcs = [
        "foo.rs",
    ] + select({
        "@platforms//os:fuchsia": [ "fuchsia.rs" ],
        "@platforms//os:linux": [ "linux.rs" ],
    }),
)
```

In GN, this is synced to `if-else-if` blocks:

```gn
rustc_library("list_concatenation") {
    sources = []
    sources += [
        "foo.rs",
    ]
    if (is_fuchsia) {
        sources += [
            "fuchsia.rs",
        ]
    } else if (is_linux) {
        sources += [
            "linux.rs",
        ]
    }
}
```

### Comment annotations

`bazel2gn` supports several comment annotations in `BUILD.bazel` files to
control the conversion process.

#### `@bazel2gn:skip`

This annotation tells `bazel2gn` to skip the annotated top-level statement,
target attribute assignment, or list element. It is useful for ignoring targets,
attributes, or list items that are not needed in GN. This annotation must be
placed either on the line immediately above the node it annotates or as a
trailing comment on the same line.

**Example:**

```bzl
rustc_library(
    name = "foo",
    srcs = ["foo.rs"],
)

# @bazel2gn:skip
rustc_library(
    name = "bar",
    srcs = ["bar.rs"],
)
```

**Converted GN:**

```gn
rustc_library("foo") {
  sources = [ "foo.rs" ]
}
```

Tests of a library are translated too, so they run in both build systems while
the library is used in both. Put `# @bazel2gn:skip` on a test (with a brief
comment explaining why) when `bazel2gn` shouldn't or can't generate a GN copy
(for example, when the target under test is itself skipped in `BUILD.bazel`,
`bazel2gn` doesn't support that rule or attribute, or the cost of running the
test in both build systems outweighs the benefit); see
[Testing libraries in both build systems](/docs/development/build/bazel_concepts/tests.md#tests-in-both-build-systems).

#### `@bazel2gn:clear`

This annotation tells `bazel2gn` an assignment is an explicit clear. It is
useful when you want to explicitly set an attribute to empty in GN, which would
otherwise result in list concatenation (`+=`) in GN. This annotation must be
placed on the same line as the assignment it is annotating to take effect.

NOTE: This currently only applies to the `configs` attribute in GN.

**Example:**

```bzl
cc_library(
    name = "foo",
    srcs = ["foo.cc"],
    copts = [],  # @bazel2gn:clear
)
```

**Converted GN:**

```gn
static_library("foo") {
  sources = [ "foo.cc" ]
  configs = []
}
```

#### `@bazel2gn:path_overwrite:<path>`

This annotation overwrites a specific path in a list. It is useful when a file
path or dependency label in Bazel differs from its path in GN. This annotation
must be placed on the same line as the path it is overwriting to take effect.
The specified value is wrapped in double quotes.

**Example:**

```bzl
cc_library(
    name = "foo",
    srcs = [
        "foo.cc",
        "bar.cc",  # @bazel2gn:path_overwrite:bar_replaced.cc
    ],
)
```

**Converted GN:**

```gn
static_library("foo") {
  sources = [
    "foo.cc",
    "bar_replaced.cc",
  ]
}
```

#### `@bazel2gn:raw_overwrite:<value>`

<!-- TODO(https://fxbug.dev/543568916) Specify where this and other annotations
     are supported once a decision is made about which scenarios to support
     and that is reflected in the implementation. -->

Where supported, this annotation overwrites an attribute assignment or list item
with the exact GN expression that follows the annotation. It is useful when a
non-path value differs in GN, including cases where GN features like
`rebase_path()` must be used. This annotation must be placed on the same line as
the value it is overwriting to take effect.

**Example:**

```bzl
cc_library(
    name = "foo",
    ...
    ldflags = [
        "-Wl,--version-script=sdk/lib/fdio/fdio.ld",  # @bazel2gn:raw_overwrite:"-Wl,--version-script=" + rebase_path("fdio.ld", root_build_dir)
    ],
)
```

**Converted GN:**

```gn
static_library("foo") {
    ...
  ldflags =
      [ "-Wl,--version-script=" + rebase_path("fdio.ld", root_build_dir) ]
}
```

#### `@bazel2gn:raw_attribute:<value>`

This annotation emits a raw GN attribute or statement inside the converted GN
target. It is useful when a GN target needs an extra attribute or statement that
has no counterpart in the Bazel target. This annotation must be placed right
above the target definition it is annotating to take effect. Multiple
`@bazel2gn:raw_attribute:` annotations may be stacked right above a target.
Prefer a Bazel attribute when the rule has one: for example, set
`gn_output_name` on `fx_cc_binary()` and `fx_cc_test_binary()` instead of using this
annotation.

**Example:**

```bzl
# @bazel2gn:raw_attribute:output_name = "far"
cc_binary(
    name = "bin",
    srcs = ["main.cc"],
)
```

**Converted GN:**

```gn
executable("bin") {
  sources = [ "main.cc" ]
  output_name = "far"
}
```

#### `@bazel2gn:value_overwrite:<value>`

This annotation overwrites the value of a dictionary entry when converting
dictionary attributes such as `rustc_env`. It must be placed as a trailing
comment on the same line as the dictionary entry it overwrites.

**Example:**

```bzl
rustc_library(
    name = "lib",
    srcs = ["src/lib.rs"],
    rustc_env = {
        "FOO": "bar",  # @bazel2gn:value_overwrite:overwritten_bar
        "BAZ": "qux",
    },
)
```

**Converted GN:**

```gn
rustc_library("lib") {
  sources = [ "src/lib.rs" ]
  rustenv = [
    "FOO=overwritten_bar",
    "BAZ=qux",
  ]
}
```

#### `@bazel2gn:transformer=<transformer>`

This annotation tells `bazel2gn` to apply a transformer to the annotated
statement.

`bazel2gn` knows which attributes of a target hold labels or file paths, but it
cannot tell what a top-level variable holds, so it copies unannotated variables
verbatim. Annotate shared lists of deps or sources with a transformer so that
`bazel2gn` converts them the same way it converts target attributes directly.

**Example:**

```bzl
# @bazel2gn:transformer=deps
COMMON_DEPS = ["//third_party/rust_crates/vendor:lock_api"]
```

**Converted GN:**

```gn
COMMON_DEPS = [ "//third_party/rust_crates:lock_api" ]
```

The currently supported transformers are:

*   `visibility`: converts Bazel visibility to GN visibility.
*   `deps`: converts Bazel dependencies to GN dependencies.
*   `configs`: converts Bazel `copts` to GN `configs`.
*   `file_paths`: converts Bazel file paths to GN file paths.
*   `expectations`: converts `.json5` labels to GN file paths and other labels
    to GN dependencies.

### Third-party dependency translation

`bazel2gn` automatically translates Bazel external repository target labels
(e.g., `@com_google_googletest//:gtest`, `@com_googletest_gtest//:gtest`,
`@zlib//:zlib`) and root-package third-party target labels (e.g., `//:glm`) to
their equivalent GN labels (e.g., `//third_party/googletest:gtest`,
`//third_party/zlib`, `//third_party/glm`).

The mapping between Bazel target labels and GN labels is defined in
[third_party_target_map.json](./third_party_target_map.json).

If `bazel2gn` encounters an external repository dependency (starting with `@`)
that is not defined in `third_party_target_map.json` and has no manual overwrite
directive:

1.  It prints a warning to `stderr`: `WARNING: unable to translate Bazel label
    <label> to GN`
2.  It appends an inline comment in the generated GN file: `# BAZEL2GN_WARNING:
    Unknown Bazel repository name`

### Repository-local rule extensions (`.bazel2gn.json`)

Subtrees or nested repositories can extend `bazel2gn` with custom Bazel-to-GN
rule mappings and extra GN `.gni` imports by placing a `.bazel2gn.json` file in
an ancestor directory (loaded and merged from `--fuchsia_dir` down to each
synced directory):

```json
{
  "gn_imports": ["//path/to/migration.gni"],
  "rules": {
    "custom_rule": {
      "gn_template": "custom_gn_template",
      "attrs_to_omit": ["bazel_only_attr"],
      "attr_map": {"bazel_attr": "gn_attr"},
      "attr_transformers": {"manifest": "file_paths", "extra_deps": "deps"},
      "attr_assignment_ops": {"configs": "+="},
      "extra_gn_expressions": ["from_config = true"],
      "default_gn_expressions": ["opt_in = false"],
      "unwrap_packaged_binary_attrs": ["deps"]
    }
  }
}
```

*   `gn_imports`: Additional `.gni` files imported below the `bazel2gn` sentinel
    after `//build/tools/bazel2gn/bazel_migration.gni`.
*   `rules.<name>.gn_template`: Target GN template name, or
    `"__NO_GN_EQUIVALENT__"` to skip targets of that rule.
*   `rules.<name>.attr_transformers`: Maps Bazel or converted GN attribute names
    to one of the transformers listed above (`visibility`, `deps`, `configs`,
    `file_paths`, `expectations`).
*   `rules.<name>.unwrap_packaged_binary_attrs`: Lists attributes whose
    same-file `fx_packaged_binary()` labels should be unwrapped to the
    underlying GN binary (or rewritten to `renamed_binary()`).

[embedsrcs]: https://github.com/bazel-contrib/rules_go/blob/master/docs/go/core/rules.md#go_library-embedsrcs
[goembed]: https://pkg.go.dev/embed
[starlark]: https://bazel.build/rules/language
[gnreference]: https://gn.googlesource.com/gn/+/main/docs/reference.md
[mypy]: https://mypy-lang.org/
[pyrefly]: https://pyrefly.org/
