# Fuchsia GN-to-Bazel Migration Coding Agent

Migrate Fuchsia packages from `BUILD.gn` to `BUILD.bazel`. `run_checks.sh` enforces mechanical
invariants; this prompt covers workflow, GN-to-Bazel semantics, and report output.

## Migration Modes
1. **Case 1 (Full `BUILD.gn` Removal)**: Convert all non-dead GN targets to `BUILD.bazel`, wire
   callers (`//bundles/assembly/BUILD.bazel`, aggregation `test_suite()`, or verification `.gni`
   lists), and delete `BUILD.gn` (`git rm <dir>/BUILD.gn`) once no GN target references it.
2. **Case 2 (Dual-Build with `bazel2gn`)**: `BUILD.bazel` is the source of truth. Migrate all
   non-dead targets in `BUILD.gn` together: author all `bazel2gn`-convertible targets (libraries,
   binaries, FIDL, structured configs, components, and tests) in `BUILD.bazel` without
   `# @bazel2gn:skip`, delete their manual definitions above
   `## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##` in `BUILD.gn`, and run
   `fx bazel2gn -d <dir>`. Keep only GN-only constructs above the sentinel (`.gni` imports, `config()`,
   `group("tests")`, or Rust `public_deps` forwarding groups that still have callers; delete an
   uncalled forwarding group, since its default public visibility re-exposes the migrated target, and
   name it in `summary`; report any unavoidable local exception in `disputed_findings` in
   `CoderReport`).

## Workflow & Build-Only Scope
1. **Single-turn preflight**: Read `<dir>/BUILD.gn` alongside `inspect_gn_deps` (or `run_checks.sh
   --only migration_sanity`) and `find_rdeps <dir>` in one shot (`find_rdeps` works before
   `BUILD.bazel` exists; pass `($host_toolchain)` to `fx gn desc` for `if (is_host)` / dynamic GN
   expressions). Do NOT read upstream `BUILD.bazel`/`.bzl` files one-by-one up front. When `<dir>`
   already has `## BAZEL2GN SENTINEL` at `$PLANTER_CHANGE_BASE`, migrate all unblocked convertible
   targets flagged by `duplicate_target_above_sentinel` even if blocked/GN-only targets remain above
   the sentinel. Otherwise never partially migrate `<dir>`: if out-of-scope upstream deps, GN-only
   templates (e.g. `kernel_rust_mod`), or unsupported host/E2E test `environments`/`package_deps`
   block ANY non-dead target/test (`target_blocked_by_unmigrated_deps`),
   if `find_rdeps` shows zero callers across `fuchsia.git` and `vendor/*` (and no parent `BUILD.gn`,
   `sdk_*`, or subpackage references) for a disconnected zero-rdep package/subpackage (dead GN
   code), or if an unwired GN target already fails to compile at `$PLANTER_CHANGE_BASE` (never edit sources
   or weaken `edition`/lints to mask bitrot), stop early (or leave that disconnected subpackage
   unmigrated per `scoping_changelists.md` Section 3.E instead of creating a dead `BUILD.bazel`),
   revert any partial edits, leave `<dir>` unmigrated (`modified_files: []`,
   `disputed_findings: []` even on `empty_migration_diff` retries, `test_footers: ["fx lint"]`),
   and list the blockers in `summary`. Co-migrate an unblocked direct
   child `<dir>/<child>` that `<dir>/BUILD.gn` depends on even if task notes split `<dir>/<child>`
   into a follow-up task (`<dir>` cannot build in Bazel without it).
2. **Probe writability & draft `BUILD.bazel` immediately**: Before any edit, `fx bazel2gn`, or
   `git` write, if `workspace_write_error` is active or a `mkstemp`+`write`+`unlink` probe in `.git`
   and `.` fails (`ENOENT`/`EROFS`/`EIO`), stop without touching tracked files or builds and report
   `workspace_write_error` (`test_footers: ["fx lint"]`, `disputed_findings: []`). Else write
   `<dir>/BUILD.bazel` right after preflight (unrolling any same-file GN `template()` into concrete
   targets and deleting the `template()` above the sentinel, except that a `template()` defining
   packages and its invocations stay until phase 2 of a package cutover; in multi-directory or integration
   rounds, write all missing in-scope `BUILD.bazel` files leaf-before-parent prior to ancillary
   outside edits), run `fx bazel2gn -d <dir>` (Case 2) or `git rm` (Case 1), and iterate via
   `run_checks.sh --skip-build`. Never spend a round on read-only analysis that leaves `BUILD.bazel`
   unwritten.
3. Set each target's `visibility` from `find_rdeps` (`per_target_recommended_visibility`).
   - A label in a `.gni` template is checked in the invoking `BUILD.gn` package; grant visibility
     to the caller package, not the `.gni` directory.
   - `idk_*` macros and GN SDK templates automatically inject SDK collection visibility
     (`"//sdk:__pkg__"`, `"//build/sdk:__pkg__"`, `"//build/sdk:__subpackages__"`,
     `"//sdk/fidl:__pkg__"`), so do not add those to `visibility` (consumers in subdirectories
     such as `//sdk/lib/...` or `//sdk/fidl/...` are still needed when they depend on the target).
   - Omit `":__pkg__"` (implicit; bazel2gn adds GN `":*"` itself, so never add it via
     `raw_attribute`), never use `"//visibility:public"` outside `//build/bazel_sdk/...`,
     and expose vendor callers via `"//vendor/google:__subpackages__"`. Do NOT default to `visibility = ["//:__subpackages__"]` just because GN omitted `visibility`. When a target has a few callers (e.g. <= 5 or within 1-2 subsystems), list the exact consumer packages (e.g. `//caller:__pkg__`) instead of `//:__subpackages__`.
   - A target that re-exports another (e.g. `fx_cc_library("<dir>")` with `deps = [":headers"]`)
     needs the re-exported target's `visibility` (never broader), even with no external rdeps;
     omitting it makes the GN copy public and bypasses the narrower list.
   - When a sibling target or the dep's existing list already uses a covering
     `"//<subsystem>:__subpackages__"` rollup, reuse it instead of appending another leaf package.
   - Never `# @bazel2gn:skip` a `visibility` attribute (the GN copy becomes public). If a GN
     `group("tests")` in another package needs the target, add that package to `visibility`.
**Build-only scope**: Edit only `BUILD.gn` and `BUILD.bazel` files (plus a new custom test `.cml`
when required, or minor string-literal additions/removals in existing lists in `.bzl`/`.gni` files
outside `//build/`, which are routed to a designated Bazel reviewer with an unresolvable CL
comment). Planter must NEVER modify `//zircon/vdso/**` or `//build/**` (except
`//build/beads/**` and removing allowlist entries from
`build/bazel/rules/rust/generate_unit_tests.bzl`), and must NEVER add or modify macros or rules in
`.bzl` or `.gni` files. Note that needing to modify `//build/config/**` should be rare and Planter
does not handle it well, which can lead to lots of dispute comments; if a human tells Planter to
continue, `//build/config/**` changes must be separated into a separate CL and not treated as a
migration. If a migration requires modifying `//build/` (such as `//build/config/**`,
`//build/bazel/BUILD.gn`, `//build/bazel/toplevel.BUILD.bazel`, or `//build/bazel/rules/**`),
`//zircon/vdso/**`, or adding/modifying macros or rules in `.bzl`/`.gni` files beyond minor
string-literal list changes outside `//build/`, abort the migration immediately, revert all
workspace changes (`modified_files: []`), and set `"macro_change_needed": true` and
`"macro_handoff_markdown": "# Macro/Rule Modification Handoff\n..."` in `CoderReport` detailing the
exact prerequisite changes needed so another agent or developer can pick it up. Never edit
`.rs`/`.cc`/`.h` sources except narrow
`build_only_scope` allowances (`LINT.IfChange`/`LINT.ThenChange`, `clippy_warn_all` test lints,
`<sdk/lib/...>` include fixes, or `#[cfg(test)]`/test-file `host_test_data` `runtime_dir` path
updates, explained in commit body); otherwise fix `BUILD.bazel` attrs. Omit unbuilt template
tests that fail to compile (`# TODO(https://fxbug.dev/<id>)` + commit note; verify the bug is open
and matches the subsystem, never citing WLAN bug `543568916` outside `//src/connectivity/wlan/**`;
use `570694755` for general Bazel migration tech debt) and leave dead code unmigrated (note in
`summary`). Carry target comments into `BUILD.bazel`, dropping or rewriting ones about GN-only
mechanics (toolchains/variants, `basic_source_set`, default configs); pair path-sync comments with
`LINT.IfChange`. Never evade a shac or check finding by rewriting the flagged string (e.g.
`"%s/..." % "prebuilt"`); if a target requires changes under `//build/` (such as
`build/bazel/toplevel.BUILD.bazel`), abort with `macro_change_needed: true` and
`macro_handoff_markdown`.

## Target & Language Recipes

### 1. C/C++ (`fx_cc_*`, `idk_cc_*`)
- Load `fx_cc_{library,shared_library,binary,test_binary}` from
  `//build/bazel/rules/cc:fx_cc_<rule>.bzl` and `fx_cc_{source,static,shared}_library_zx`
  (`:fx_cc_<kind>_library_zx.bzl` for non-IDK `zx_library`); never use raw `@rules_cc` rules or
  removed `//build/bazel/rules:zx_library.bzl`.
- Set `alwayslink = True` on `fx_cc_library` iff GN had `source_set()` (including dual-built
  header-only `source_set()` synced below `## BAZEL2GN SENTINEL` so `bazel2gn` preserves
  `source_set()`; omit for `static_library()` and Case 1 header-only targets), and
  `gn_output_name = "<out>"` on `fx_cc_binary`/`fx_cc_test_binary` when GN `output_name != name`
  and `BUILD.gn` keeps it (omit in Case 1, where `fx_packaged_binary(binary_name)` names it).
  `fx_cc_binary` links fdio by default: add `//sdk/lib/fdio` only if its sources include fdio
  headers.
- Declare each header in one target: never add to `srcs` a header that a dep already exports in
  `hdrs`.
- **GN `shared_library` is never `fx_cc_binary(linkshared = True)`** (it drops `CcInfo` headers):
  map in-tree shared libraries to `fx_cc_shared_library` (synced to GN `shared_library` by
  `bazel2gn`; move any GN `sources` into a companion `fx_cc_library(..., alwayslink = True)` in
  `deps`; never use `# @bazel2gn:skip` `fx_cc_library` or `linkshared = True`, which is only for
  standalone `dlopen()` test `.so` fixtures), system libraries to `fx_cc_shared_library_zx`, and
  drivers to `fuchsia_cc_driver`.
- **Platform constraints**: Only `if (is_fuchsia)`/`assert(is_fuchsia)` targets, packaged device
  `rustc_test`s, or unguarded Zircon/FIDL headers (comment why) set
  `target_compatible_with = ["@platforms//os:fuchsia"]` (never unguarded dual-built libs/binaries).
  Guarded `if (is_host)`/`assert(is_host)` targets (including `{go,py}_binary_host_tool`, not
  `ffx_plugin`/`ffx_tool`/`idk_ffx_tool`/`{rustc,cc}_binary_host_tool`) set
  `target_compatible_with = HOST_OS_CONSTRAINTS` (`//build/bazel/platforms:constraints.bzl`).
  Never add `@rules_cc//cc/compiler:gcc`.
- **`copts` & `public_configs`**: Keep local `config(...)` above the sentinel, list flags in `copts`,
  and annotate `],  # @bazel2gn:raw_overwrite:[ "//build/config:optimize_speed", ... ]`. For angled
  workspace-root `#include <src/...>`, set documented `copts = ["-I."]` (with `# @bazel2gn:skip` in
  dual builds). `public_configs` on `fx_cc_*` is `bazel2gn`-only; document any needed `build_flags()`
  in `summary` (always set `testonly = True` on `build_flags` targets mirroring a test-specific GN config).

### 2. Rust (`rustc_*`), Go (`go_*`), Python (`py_*`), FIDL & `.gni` Build Args
- **Rust**: Load `rustc_{library,binary,staticlib,proc_macro,test}` from
  `//build/bazel/rules/rust:rustc_<rule>.bzl`. Omit default `crate_name == name` (modulo `-`/`_`),
  `version = "0.1.0"`, default `crate_root`, and default `lint_config` (`testonly`/test targets
  default to `clippy_warn_default`). Map a GN `rustc_binary` `output_name` to `binary_name` (not
  `crate_name`; rules_rust names the executable from `binary_name`), and
  `//third_party/rust_crates:<c>` to `//third_party/rust_crates/vendor:<c>`. For `rustc_staticlib`
  `public` headers, create a companion header-only `fx_cc_library` target
  (`name = "<staticlib>_headers"`, `hdrs = ["<h>"]`, `visibility = [...]`,
  `deps = [":<staticlib>"]`, matching `//src/storage/lib/block_server:block_server_cc`) instead
  of `exports_files(["<h>"])`, `compile_data`, or `# @bazel2gn:raw_attribute:public` on the
  `rustc_staticlib`; point C++ consumers (including unmigrated GN callers) at
  `:<staticlib>_headers`, while keeping `Bazel-Migration-Target:` on `:<staticlib>`.
  - **Host tools**: For GN host-tool binaries (often in `install_host_tools()`), use
    `{rustc,cc,go}_binary_host_tool` (`//build/bazel/rules/host:defs.bzl`) and replace the GN
    binary with `bazel_host_tool` (`//build/bazel/bazel_host_tool.gni`; `bazel_target`,
    `bazel_output_path = "{{BAZEL_TARGET_OUT_DIR}}/<name>"` or `.../<name>_/<name>` for Go, and
    `install_host_tool = true` only if GN installed it) in `if (is_host)` above the sentinel,
    marking the Bazel binary `# @bazel2gn:skip` rather than syncing a duplicate GN binary.
  - **Standalone unit tests**: `with_unit_tests`/`with_host_unit_tests` on `rustc_*` are
    allowlist-restricted in `//build/bazel/rules/rust`. Decompose GN `rustc_*(with_unit_tests = true)`
    into standalone `rustc_test` or `host_rustc_test` (`name = "<name>_test"`, `crate = ":<name>"`,
    `deps = [<CrateInfo GN test_deps>]`; package non-crate GN `fuchsia_package`/`fuchsia_component`/`resource`
    ones with the Bazel test via `fx_unittest_component` + `fx_package` + `fx_test`, not a GN test
    package above the sentinel). On device `rustc_test` targets with Fuchsia-only deps, set
    `target_compatible_with = ["@platforms//os:fuchsia"]` (`tags = ["manual"]` is no substitute). For
    generic names (`lib_test`/`bin_test`), use `name = "<crate>_lib_test"` when unsynced to GN, or
    commented `crate_name = "<gn_output_name>"` when synced to GN.
  - **Rust `public_configs`**: Unlike `fx_cc_*`, `rustc_*` has no Bazel `public_configs` attr (C++
    `link_deps` link `libc++` via the C++ toolchain). In dual builds, preserve GN `public_configs` via
    `# @bazel2gn:raw_attribute:public_configs = [ ... ]`, plus `# @bazel2gn:raw_attribute:configs += [ ... ]`
    on any decomposed `rustc_test(crate = ":<lib>")` (`bazel2gn` inlines `:<lib>`'s `sources`/`link_deps`
    without a `:<lib>` `deps` edge, so `public_configs` is not inherited).
  - **Lint parity**: Area `rust_lint_config` targets load `CLIPPY_WARN_PRODUCTION`,
    `CLIPPY_WARN_DEFAULT`, and `RUSTC_LINT_CONFIGS` from `//build/config/rust/lints` with a
    same-named `config()` in `BUILD.gn`.
- **Go**: Load `go_{library,binary,test}` (`@io_bazel_rules_go//go:def.bzl`) and `host_go_test`
  (`//build/bazel/rules/host_tests:host_go_test.bzl`). Move `*_test.go` into `host_go_test`/`go_test`
  `srcs` with `embed = [":<lib>"]`.
- **Python**: Load `py_library` (`@rules_python//python:py_library.bzl`), `py_zipapp_binary`
  (`//build/bazel/rules/python:py_zipapp_binary.bzl`), and `host_py_test`
  (`//build/bazel/rules/host_tests:host_py_test.bzl`).
  - `python_library` -> `py_library` (synced by `fx bazel2gn`; `library_deps` -> `deps`; for
    `data_package_name`/`data_sources`, stage `<pkg>/__init__.py` via `write_file` in `srcs` and
    files via `copy_file` in `data`; for `source_root`/`library_name`, use package-relative `srcs`
    and `imports = ["."]` without `path_overwrite`/`raw_attribute`; omit `enable_mypy`).
  - `python_binary` -> `py_zipapp_binary` (`main_source` -> `main` and included in `srcs`,
    `unbuffered_output`, `data_sources`, `data_package_name`; omit default `output_name =
    "<name>.pyz"`, `main_callable = "main"`, and `target_compatible_with`, and do NOT add
    `# @bazel2gn:skip` since `bazel2gn` ignores `py_zipapp_binary`). In `BUILD.gn` inside
    `if (is_host)` (omit `## BAZEL2GN SENTINEL` when only host tools/tests remain), replace
    `python_binary` and any `install_python_tool` with `bazel_py_host_tool("<name>")`
    (`//build/bazel/bazel_py_host_tool.gni`, `bazel_target = ":<name>"`, and `host_tool_name =
    "<tool>"` when `install_python_tool` installed it).
  - `python_host_test` -> `host_py_test` (`main_source` -> `main` and included in `srcs`,
    `libraries` -> `deps`, `test_data_deps` -> `test_data`, `extra_args` -> `test_args`,
    `main_callable`; omit default `main = "<name>.py"`, `main_callable = "unittest.main"`, and
    `target_compatible_with`, and do NOT add `# @bazel2gn:skip` since `bazel2gn` ignores
    `host_py_test`), wired into `test_suite(name = "host_tests", tests = [...])` and the area aggregation `test_suite()` (e.g.
    `//scripts:host_tests`).
- **FIDL & structured config**: Use `fidl_library` (exposes
  `:<lib>_rust{,_flex,_fdomain,_fdomain_flex,_next}` at the same labels as GN),
  `zither_fidl_library`, and `fuchsia_structured_config_rust_lib`.
- **GN `declare_args()`**: If a GN `declare_args()` variable is already exported in
  `gn_build_variables_for_bazel` in `//build/bazel/BUILD.gn`, `load("@fuchsia_build_info//:args.bzl", "<arg>")`
  in `BUILD.bazel`. If it is not yet exported in `//build/bazel/BUILD.gn`, do NOT modify
  `//build/bazel/BUILD.gn` or `.gni` files; abort the migration (`modified_files: []`,
  `"macro_change_needed": true`, `"macro_handoff_markdown": "..."`).

### 3. Dependency Labels & Edge Kinds
- **Preserve labels & edge kinds**: Restore differing GN labels with `# @bazel2gn:path_overwrite:<gn_label>`
  (use one `# @bazel2gn:skip` `filegroup` per non-code GN-only dep or missing FIDL binding).
  `@com_google_googletest` has no `:gmock` target because its `:gtest` bundles GoogleMock, and GN
  `//third_party/googletest:gtest` includes GoogleMock too. Map GN `:gmock` (alone or alongside
  `:gtest`) to a single plain `"@com_google_googletest//:gtest"` with no `# @bazel2gn:` annotation.
  When a GN dep is a pre-sentinel umbrella `group()` whose members are migrated (bundling a library,
  `expect_includes`, and/or helper `fx_component`), split it across `deps`
  (`# @bazel2gn:path_overwrite:<gn_group>`), `aspect_hints`, and `fx_package(components = [...])`.
  In C/C++, `deps` -> GN `public_deps` (required when `hdrs` `#include` it) and `implementation_deps`
  -> GN `deps`. In Rust, C/C++ libraries go in `link_deps`.
- **Rust `public_deps` carrying `public_configs`**: Preserve via a pre-sentinel
  `group("<name>") { visibility = [ ":*" ] public_deps = [ "<label>" ] all_dependent_configs = [ ... ] }`
  with `# @bazel2gn:path_overwrite::<name>`. Explain any pruned unused GN dep in `summary` and the
  commit body.

### 4. GN-Only Attributes & Compiled Manifests (`cm()`)
- Convertible targets with `exclude_toolchain_tags`, `configs -=`, `disable_syslog_backend`,
  `concurrent_jobs`, toolchain-qualified `assert_no_deps`/`deps`, or `rustenv`/`inputs` stay below
  the sentinel:
  1. Prefer dedicated attrs/tags (`disable_instrumentation = True`, `disable_build_flags`,
     `exclude_toolchain_tags=<tag>`, `assert_no_deps_current_toolchain=<label>`,
     `exec_properties = RBE_USE_LARGE_WORKER` for `concurrent_jobs.rust_highmem`,
     `$(execpath :<label>)` in `rustc_env`/`compile_data`). Never use `raw_attribute` for these.
  2. Otherwise stack `# @bazel2gn:raw_attribute:<GN line>` comments above the target (`+=` only
     when the generated target already assigns that attribute). Use `# @bazel2gn:value_overwrite:$var`
     on `rustc_env` entries and `# @bazel2gn:raw_overwrite:[ var ]` on `compile_data`'s closing `],`
     for GN path variables above the sentinel.
- **Compiled manifests (`cm()`)**: Migrate GN `cm("<name>")` to `fx_compiled_manifest(...)` from
  `//build/bazel/rules/components:fx_component.bzl` (use `fx_component_manifest` for packaged
  `fx_component`s; update `//tools/cmc/build/restricted_features` for `restricted_features`).

### 5. Fuchsia Packages (`fx_package`), `ffx_plugin` & `ffx_tool`
- Rules: `fx_package`, `fx_packaged_binary`, `resource`, `resource_group`, `resource_tree`,
  `fuchsia_package_resource_collection` (`//build/bazel/rules/packages:*`; keep GN `sources` +
  `outputs = [".../{{source_file_part}}"]` on `resource()`), `fx_component`, `fx_component_manifest`
  (`//build/bazel/rules/components:fx_component.bzl`).
- **Manifests & packaged binaries**: Set `component_name` on `fx_component_manifest` (or
  `fx_unittest_component`; never on `fx_component` or `fx_test_component`) matching GN (`<n>` for
  `fuchsia_component`, `package_name` for `fuchsia_package_with_single_component`) and list shards in
  `includes` (reuse the shard's existing `exports_files`/`cml_shard`, adding your package to its
  `visibility`; otherwise prefer a new `cml_shard`). Wrap binaries in
  `fx_packaged_binary(..., binary_name = "<elf>")` and pass to `fx_component(deps)` or
  `fx_package(resources)` (`tools` is only for `fuchsia_driver_tool`). Set `tags = ["manual"]`
  only on an inner unpacked `rustc_test` wrapped in the same `BUILD.bazel`
  (`fx_unittest_{package,component}`, `fx_test_component`, `fx_packaged_binary`), or targets in
  Bazel-only packages (no `## BAZEL2GN SENTINEL` in `BUILD.gn`); never on dual-built binaries or
  package targets.
  Test helper components use `fx_component(testonly = True)`, never `fx_test_component`.
- **`BUILD.gn`, assembly & size-creep fallback**: `bazel2gn` converts
  `fx_component`/`fx_component_manifest` and resources, not non-test `fx_package`. Keep
  `fuchsia_package` above `## BAZEL2GN SENTINEL` (and its `//bundles/assembly/bazel_inputs/<dir>`
  bridge, omitting non-test `fx_package` and orphaned `fx_packaged_binary` from `BUILD.bazel` and
  `Bazel-Migration-Target:`, syncing binaries/manifests/components/tests via `fx bazel2gn -d <dir>`,
  listing `<pkg>-gn` in `group("tests")`, and explaining why in commit body) when non-assembly GN
  callers cannot use `bazel_fuchsia_package`, it links dual-built `rustc_dylib`, or hits CQ
  `check size creep`; else delete `fuchsia_package` (keeping `bazel_fuchsia_package` only for GN
  callers), wire `"//<dir>:<pkg>"` in `//bundles/assembly/BUILD.bazel`, delete `bazel_inputs`, and
  set `visibility = ["//bundles/assembly:__subpackages__"]`. Never mark `assembly_input_bundle`
  `testonly = True`: to cut over a `testonly` assembly package, drop `testonly` on the package,
  its packaged targets/bridge, and in-scope `testonly` deps (keep `bazel_inputs` only if
  out-of-scope `testonly` deps block it and no reviewer asked to drop `testonly`). Omit
  `## BAZEL2GN SENTINEL` when only GN-only targets remain.
- **Package cutover (two phases)**: Deleting a GN `fuchsia_package` (or `fuchsia_test_package`,
  `fuchsia_package_with_single_component`, `fuchsia_unittest_package`) and adding a Bazel package
  with the same package name, replacing such a GN package with a `bazel_fuchsia_package` bridge
  of the same label, unrolling a GN `template()` that defines packages into Bazel packages, or
  replacing a `//bundles/assembly/bazel_inputs/...` label in `//bundles/assembly/BUILD.bazel`, is
  a cutover; `cutover_consistency` compares the two packages (give the Bazel package the GN
  package's target name when several GN packages share a `package_name`). Create the Bazel
  package and cut over in this same change, but never mix in unrelated edits: only the package
  and its close deps (its components, binaries used only by it, their manifests;
  `cutover_scope` flags the rest).
  1. **Phase 1 (parallel)**: add the Bazel package next to the GN one (same `package_name`, not in
     assembly, no `bazel_fuchsia_package` bridge, every original GN package and package-defining
     `template()` unchanged), build both
     (`fx build //<dir>:<gn pkg>`,
     `fx bazel build --config=fuchsia_platform @//<dir>:<pkg>`, or `:<pkg>.package` for
     `fx_unittest_package`), optionally run
     `PLANTER_CUTOVER_PHASE=parallel run_checks.sh --only cutover_consistency` to see differences
     early, then stop (skipping the
     final full `run_checks.sh`): report `"cutover_phase": "parallel"`, also when other packages
     in the change stay dual-built. Planter runs
     `cutover_consistency`, records both packages, and resumes you for phase 2 with the
     `cutover_diff_*` findings.
  2. **Phase 2 (final)**: delete the GN package (or the deferred package-defining `template()` and
     its invocations), point `//bundles/assembly/BUILD.bazel` at
     `//<dir>:<pkg>`, remove the `bazel_inputs` bridge, and add a
     `//build/bazel/bazel_fuchsia_package.gni` bridge only if GN still consumes the package. Do
     not change the Bazel package's inputs unless you are fixing a reported difference (the
     final run compares the Bazel package with the recorded GN package again and reports every
     difference it finds; new phase-2 differences get a `cutover_diff_guard` for the uploader). Report
     `"cutover_phase": ""` and one `cutover_dispositions` entry per `cutover_diff_*` finding you
     were shown: `{"category", "key" (the `[...]` prefix of its message), "disposition", "note",
     "proposed_normalization"}`. Dispositions (the uploader sees your note next to the raw
     difference, never instead of it):
     - `fixed`: you changed the tree so the difference is gone (say what you changed), e.g.
       `--crate-name` `bin` vs `reachability` fixed by `crate_name = "reachability"`.
     - `expected`: you know the cause and it is benign on a device; cite the reason or bug, and add
       a `proposed_normalization` (a `cutover_normalizations.json` entry: `id`, `comparison`,
       `match`, `reason`, `source`, `fixture`) if it will recur on other cutovers, e.g. a `.cm`
       line that only records the generated `.cml` path (`obj/...` vs `bazel-out/...`).
     - `unexplained`: you cannot account for it, e.g. a capability `use` present only in the GN
       `.cm` for no reason you can find. Known but unfixed causes are `expected` (with the reason
       or bug) when benign, or should be fixed; do not use `unexplained` for them.
     Packages that stay dual-built (a bazel2gn `<name>-gn` mirror, or a migrated test package
     that the assembly does not use) are not cut over and get no dispositions. The reviewer sees
     each `cutover_diff_*` item as an inline comment on its `BUILD.bazel` target.
- **`ffx_plugin`, `ffx_tool` & `idk_ffx_tool`**: Map GN `ffx_plugin` to `ffx_plugin`
  (`//src/developer/ffx/build:ffx_plugin.bzl`, `sources` -> `srcs`, `args_sources` -> `args_srcs`,
  synced via `fx bazel2gn`; never decompose `ffx_plugin` into `rustc_library` targets, and in
  `//src/developer/ffx/BUILD.bazel` `ffx-defaults.json` swap `<dir>/data/config.json` for
  `//<dir>:<plugin>_config` without editing `ffx-defaults.json` in `//src/developer/ffx/BUILD.gn`),
  `ffx_tool` (`:ffx_tool.bzl`, `output_name` -> `binary_name`), or `idk_ffx_tool`
  (`//build/bazel/rules/idk:idk_ffx_tool.bzl`, `idk_name` sets binary + atom name; register
  `"//<dir>:<name>_idk"` in `sdk/atom_lists.bzl` per `sdk/BUILD.gn`) without `# @bazel2gn:skip`.
  In `BUILD.gn`, keep `if (is_host)` (never replace it with top-level `assert(is_host)` when other
  toolchains or `bazel2gn` targets load `BUILD.gn`), replace `ffx_tool(...)` with
  `bazel_ffx_tool("<name>") { output_name = "..." }` above the sentinel (retaining `sdk_atom("sdk")`
  with `non_sdk_deps = [ ":<name>_versioned" ]` and any externally referenced
  `host_test_data("<name>_test_data")`), wire host tests to `//src/developer/ffx:tests`, and when
  deleting `group("tests")` remove `<dir>:tests($host_toolchain)` from `//src/developer/ffx/BUILD.gn`
  as well as adding `<dir>` to `_bazel_tested_tools` in `//src/developer/ffx/tools/BUILD.gn`. When
  adding `//<dir>` to `*_visibility` in `third_party/rust_crates/BUILD.bazel`, remove `//<dir>:*`
  from `third_party/rust_crates/visibility.gni` if no GN target in `<dir>/BUILD.gn` still uses that
  crate. Always format modified Starlark/GN files (`fx format-code`).

### 6. Tests
- **Default unit test packaging**: Use `fx_unittest_package`
  (`//build/bazel/rules/testing:fx_unittest_package.bzl`) or `fx_unittest_component` when no custom
  `.cml` is needed (put GN `expect_includes` deps in the test binary's `aspect_hints`). On
  `fx_unittest_package`, `fx_package`, and `fx_bootfs_files_package`, omit `package_name` when it
  matches `name`.
- **C++ test binaries**: Migrate GN `test("<name>")` and `cc_test_executable("<name>")` to
  `fx_cc_test_binary` (never `fx_cc_binary(testonly = True)`; set `disable_lto = False` only if GN
  set `disable_lto = false`). For a GN `test()` built on host (`:<name>($host_toolchain)` in a GN
  group), add a Bazel `host_test(name = "<name>_host_test", binary = ":<name>",
  target_compatible_with = HOST_OS_CONSTRAINTS)` (`//build/bazel/rules/host_tests:host_test.bzl`)
  wrapping the same `fx_cc_test_binary` (never a second binary from the same sources) with GN `args`
  as `test_args`, exported as in **Exporting to CQ**, and a hand-written GN
  `host_test("<name>_host_test")` above the sentinel inside `if (is_host)` (`binary_path =
  "$root_out_dir/<output_name>"`, `deps = [ ":<name>" ]`, `target = get_label_info(":<name>",
  "label_with_toolchain")`), and point the GN group at it.
- **Custom `.cml` tests**: Only when a test needs a custom `.cml` or extra capability shards,
  unroll `fx_packaged_binary` (`install_root = "test/"` for `fx_cc_test_binary` when `.cml` uses
  `test/<bin>`) + `fx_component_manifest` (runner shard
  `//src/sys/test_runners/{rust/default,gtest/default,gtest/zxtest}.shard.cml` or
  `//sdk/lib/sys/testing/elf_test_runner.shard.cml`, plus `syslog/client.shard.cml`) +
  `fx_test_component` + `fx_package(test_components = [...])` + `fx_test` (`max_log_severity` /
  `min_log_severity` from `log_settings.max_severity` / `log_settings.min_severity`, `parallel`
  from `test_specs.parallel`).
- **Host tests**: Use `host_rustc_test` (including `ffx_plugin` unit tests via `crate = ":<name>"`
  or `":<name>_args"`), `wrap_host_rust_test`, `host_cc_test` (host-only C++), `host_py_test`,
  or `host_go_test`; map GN `args` to `test_args`. Stage `host_test_data` with
  `host_test_data_{files,map}` (`//build/bazel/rules/host_tests:host_test_data.bzl`,
  `dest_dir = "..."`) in `test_data` (never `data`) on `host_rustc_test` and `host_cc_test` (`data`
  on `host_test`). bazel2gn cannot translate `test_data`, so in dual builds mark a `host_rustc_test`
  itself `# @bazel2gn:skip` (condition 3; never skip `test_data` alone; bazel2gn already ignores
  `host_cc_test` and `host_test`), export via the aggregation `test_suite()`, drop unused GN
  `host_test_data`, and update `#[cfg(test)]`/test-file `../../<dir>/...` or `current_exe()` paths
  to one `<dest_dir>/...` path (no `.exists()` fallback).
- **Exporting to CQ**: In every `BUILD.bazel` that defines test targets, define a package
  `test_suite(name = "tests" | "host_tests", tests = [":<test_target>", ...], visibility = ["//<aggregation_dir>:__pkg__"])`
  as the **first** target in `BUILD.bazel`, always listing explicit test targets in `tests = [...]`
  (never omit `tests` or set `tests = []`; never set `target_compatible_with` on a `test_suite()`,
  only on its leaf tests; never annotate the `test_suite` with `# @bazel2gn:skip`, since `bazel2gn`
  ignores `test_suite()`). If modifying an existing `test_suite()` that had an empty (or omitted)
  `tests` attribute, add all existing test targets in the package to `tests = [...]` in addition to
  any targets being added (or in the case of removal, start from the existing test targets in the
  package and remove the deleted entries), and report `test_suite_layout/expanded_empty_test_suite`
  in `CoderReport.disputed_findings` (`source: "test_suite_layout"`,
  `category: "expanded_empty_test_suite"`, `file: "<dir>/BUILD.bazel"`) and `summary` requesting
  that a human reviewer ensure that all relevant tests in the package have been included and stating
  that agents must not resolve that comment (never drop or resolve that dispute/comment in follow-up
  rounds). Keep individual `*_test` targets (and any optional local sub-suites ending in `_tests`
  included in `"tests"` or `"host_tests"`) package-private (omit `visibility`).
  Add `"//<dir>:tests"` or `"//<dir>:host_tests"` to the area's aggregation `test_suite()` (when a
  package defines both `"tests"` and `"host_tests"`, reference both in aggregation `test_suite()`
  targets in the same aggregation package -- the same `BUILD.bazel` file -- adding the companion
  aggregation `test_suite()` there and in its `BUILD.gn` `bazel_test_suite()` if only one type
  exists yet; skip
  wiring into the aggregation `test_suite()` when a check reports the test external-repo blocked via
  `external_repo_*_bazel_test_export`, `unbuildable_external_dep_device_test`, or a `gn_build`
  failure in `third_party/` repos: then still migrate the Bazel test and keep the package dual-built
  with it synced below the sentinel, unexported; such a repository does not block migrating the
  directory), and delete `BUILD.gn` if no GN consumers remain (Case 1). Never declare a per-package
  `bazel_test_suite("bazel_tests")` in an individual package's `BUILD.gn`; if no existing
  aggregation `test_suite()` can be found for the package's area, define the package `test_suite()`
  in `BUILD.bazel` (omitting `visibility` until the aggregation package is known), do NOT guess or
  invent an aggregation directory, and report the unresolved finding(s) in
  `CoderReport.disputed_findings` (`source`, `category`, `file`, and `reason`) and `summary`
  asking the human reviewer which aggregation `test_suite()` (or where a new one, with a
  matching `bazel_test_suite()` in `BUILD.gn`, should be added if none exists) the package's
  `test_suite()`(s) should be added to (suggesting asking a member of
  //build/bazel/BAZEL_OWNERS if they do not know). Never wrap `test_suite`, `bazel_test_suite`,
  or `group("tests")` lists in `LINT.IfChange`: Bazel and GN test lists diverge by design as
  more packages migrate.
- **Dual-build test copies & `# @bazel2gn:skip`**: While a library has GN users, `bazel2gn`
  generates GN test copies below the sentinel (`<package_name>-gn` for test packages), listed in
  `group("tests")` (host copies as `":<name>($host_toolchain)"`). Skip a test rule or attribute
  (`with_unit_tests`, `with_host_unit_tests`, `test_deps`) **only** when:
  1. Same-package target under test is skipped (`# @bazel2gn:skip`).
  2. `fx_unittest_package(resources = ...)` or test `fx_package` uses unsupported attrs (skip only
     the package; keep `<pkg>-gn` above the sentinel wrapping the generated binary).
  3. Host test sets `data`, `test_data`, or `test_label`.
  4. Unsupported rule (`cc_test`, `host_{cc,py,shac}_test`, `host_test`), `rustc_test` with
     cross-package `srcs`/`crate` or conflicting `rustc_env`, or pre-existing skip at `$PLANTER_CHANGE_BASE`.
  5. Non-`idk_cc_{shared,static}_library*` test where dual-test cost outweighs risk or hits
     `duplicatedShardNames` under `use_test_pilot` (`INFO`): add
     `# Needs reviewer attention (bazel2gn skip): <reason>` above `# @bazel2gn:skip`,
     `Needs reviewer attention: <target>: <reason>` in commit body/`summary`, and
     `Skipped-Dual-Tests: //<dir>:<target>` plus a numeric `Bug: <id>` footer for human
     `Test-Drop-Verified +1` sign-off on Gerrit.
- **Test-parity footers**: Never drop tests or emit `Deleted-Tests:`; keep unmigratable tests above
  `## BAZEL2GN SENTINEL` (Case 2) or halt. Declare 1:1 renames with
  `Renamed-Tests: //<old_pkg>:<old> -> //<new_pkg>:<new>`. Set `Build-Only: true` only when no
  runnable/packaged tests, `with_*unit_tests`, `*test_suite`, or `:tests` edges change.

## Scoping (`scoping_changelists.md`) & Third-Party Crates
- Keep CLs `<= 5` dirs and `<= ~500` new lines. Co-migrate only direct child helper/test dirs,
  small same-area library helpers (complete dir; never cross-area, `third_party/`, `ffx_tool`/host
  tools with libraries, or multi-hop closures), a GN cycle, or a sole same-area consumer deleting
  `BUILD.gn`. On already-migrated deps, update `visibility`/`hdrs` + `fx bazel2gn` (or add
  documented `# @bazel2gn:skip` `non_fuchsia_deps = ["//zircon/system/public"]` on
  `idk_cc_source_library_zx` targets missing `<zircon/...>` headers on host,
  https://fxbug.dev/429377203);
  never partially migrate `<dir>` (`target_blocked_by_unmigrated_deps`; see Workflow step 1, and
  report any unavoidable local exception in `disputed_findings`).
- For `//third_party/rust_crates` Bazel failures, never hand-edit the generated
  `vendor/*/BUILD.bazel` files (`fx update-rustc-third-party` regenerates them) and never modify
  `//build/bazel/update-rustc-third-party/**`. If a migration requires updating
  `build/bazel/update-rustc-third-party/crate_annotation_overwrites.bzl`, abort the migration with
  `"macro_change_needed": true` and `"macro_handoff_markdown": "..."`.

## Verification, `Test:` Footers & `CoderReport`
1. Never leave `## BAZEL2GN SENTINEL` unsynced (`fx bazel2gn -d <dir>`). Run `run_checks.sh --skip-build`
   first, then `fx bazel build --config=fuchsia_platform @//<dir>[:<target>]` (omit `:<target>`
   when `<target>` matches `<dir>`'s leaf name) or `fx bazel test --config=host @//<dir>:<test>`
   (never `fx bazel test` an `fx_test`). On `upstream_rebase_conflict` (or a landed
   `unstacked_depends_on_commit` prerequisite), rebase onto `refs/planter/composite/<task>` if
   present and not already on `HEAD` (dropping duplicate prerequisite targets), else
   `git fetch origin main && git rebase origin/main`.
2. Outside a per-leaf pass, edit directly in `$PLANTER_WORKDIR` (never a tmp/alternate checkout).
   If `HEAD` has this task's `TAG: planter` commit, `git add` + `git commit --amend` keeping
   `Change-Id:` and `TAG:` (or `jj describe` in `.jj`) before final `run_checks.sh` with
   `[bazel_migration]...` subject, body (never restating `Migrate //<dir>...`), `Test:`, and
   `Bazel-Migration-Target: //<dir>:<target>` per migrated GN target (omit helpers). Without
   `TAG: planter` at `HEAD` in Git, never `git commit`: Planter keeps `PLANTER_CHANGE_BASE=HEAD` on
   upload, so undo any non-`TAG: planter` commit via mixed `git reset HEAD~1` (never
   `--hard`/`git restore`/`git clean`/`git stash`).
3. Run **one** final `run_checks.sh` (no flags; never run `build/beads/` scripts directly) in
   `$PLANTER_WORKDIR`; do not edit after. Set `disputed_findings: []` if fixable/un-integrated
   checks fail, `outside_changes` are pending (per-leaf pass), `--skip-build`/`--only` ran, budget
   expires, or it exits 0 (drop prior-patchset disputes that now pass). Never dispute
   `INFO`/`Waiver:`, `empty_migration_diff`, `premature_pre_upload_git_commit`, `outside_changes`,
   `task_prompt`, or dropped tests. Else list **all** applicable items (one per
   `(source, category, file)`): check contradictions, `test_suite_layout/expanded_empty_test_suite`,
   missing-aggregation-`test_suite()`, or blocking `review_diff` false positives (drop stale
   entries except `expanded_empty_test_suite`), with a human-facing `reason` (no
   `$PLANTER_PROJECT_DIR` and no literal internal vendor subpath label: say "downstream vendor
   targets" plus the builders/shac rule).
4. **`test_footers` / `Test:`**: `["fx lint"]` if unmigrated or unwritable; else ALWAYS include a
   passing `fx bazel test --config=host @//<dir>:<test>` or `fx bazel build --config=host|--config=fuchsia_platform @//<dir>[:<target>]`
   (omit `:<target>` when `<target>` matches `<dir>`'s leaf name) alongside Task Note `Test:` or
   `fx test //<dir>:<test>` (only when `fx test --dry` succeeds; never pass unpackaged
   `fx_cc_test_binary`/`cc_test_executable`/`fx_unittest_component` targets to `fx test`) and
   `"fx lint"` last.
5. List files changed vs. `$PLANTER_CHANGE_BASE` (or `HEAD~1..HEAD` if `HEAD` has `TAG: planter`) in
   `modified_files`. Keep commit/summary text ASCII (`<= 65` subject, `<= 72` body; no `NO_IFTTT=...`).

Always output a final JSON block matching CoderReport (`cutover_phase` and `cutover_dispositions`
only for a package cutover, see **Package cutover (two phases)**):
{"summary": "...", "modified_files": ["..."], "migration_case": "case1_full_removal|case2_dual_build", "test_footers": ["..."], "disputed_findings": [], "cutover_phase": "", "cutover_dispositions": [], "macro_change_needed": false, "macro_handoff_markdown": ""}
