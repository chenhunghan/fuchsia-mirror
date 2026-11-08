# Playbook: Case 2 Dual-Build (bazel2gn)

Use when the package retains `BUILD.gn`.
- **Sentinel & `fx bazel2gn`**: `BUILD.bazel` is the source of truth for all `bazel2gn`-convertible
  targets (libraries, binaries, FIDL, structured configs, `fx_component`, `fx_component_manifest`,
  `fx_compiled_manifest`, `ffx_plugin`, and tests). Delete their manual definitions above
  `## BAZEL2GN SENTINEL - DO NOT EDIT BELOW THIS LINE ##` in `BUILD.gn`, never hand-edit below the
  sentinel, and run `fx bazel2gn -d <dir>` after every `BUILD.bazel` edit.
- **GN-only attributes**: Preserve `exclude_toolchain_tags`, `concurrent_jobs`, toolchain-qualified
  `assert_no_deps`/`deps`, `rustenv`/`inputs`, and `configs -=` below the sentinel via dedicated
  attributes/tags or `# @bazel2gn:raw_attribute:` / `raw_overwrite` / `value_overwrite` annotations
  (see `prompts/coder.md`).
- **Packages & components**: Migrate non-test `fuchsia_package` / `fuchsia_package_with_single_component`
  targets to `fx_package` / `fx_component` / `fx_component_manifest` / `fx_packaged_binary` in
  `BUILD.bazel` and delete them from `BUILD.gn` (unless the coder prompt's assembly/size-creep
  fallback keeps `fuchsia_package` above the sentinel). Standalone `fx_component` and `fx_component_manifest`
  targets with GN consumers sync below the sentinel via `bazel2gn`.
- **Dual-build tests & `# @bazel2gn:skip` rules**: Define tests in `BUILD.bazel` without
  `# @bazel2gn:skip` so `bazel2gn` generates the GN copies (`<package_name>-gn`) below the sentinel.
  Never add `# @bazel2gn:skip` to a test rule or attribute unless one of the 5 unified test skip
  conditions in `prompts/coder.md` holds (Condition 5 `# Needs reviewer attention (bazel2gn skip):`
  may be proposed directly for Gerrit review with `Skipped-Dual-Tests: //<dir>:<target>` and a
  numeric `Bug: <id>` footer for human `Test-Drop-Verified +1` sign-off, including when dual-running
  device test copies triggers `testsharder` `duplicatedShardNames` under `use_test_pilot`).
- **Wiring tests**: Define `test_suite(name = "tests" | "host_tests",
  tests = [":<test_target>", ...], visibility = ["//<aggregation_dir>:__pkg__"])` always listing
  explicit test targets in `tests = [...]` (never `tests = []` or omitting `tests`, and keeping
  individual test targets private; if modifying an existing `test_suite()` that had an empty `tests`
  attribute, add all existing package test targets to `tests = [...]` or start from the existing
  list and remove entries, and report `test_suite_layout/expanded_empty_test_suite` in
  `CoderReport.disputed_findings` and `summary` requesting that a human ensure all relevant tests
  have been included without allowing agents to resolve the comment) as the first
  target in `BUILD.bazel`, wire `"//<dir>:tests"` or `"//<dir>:host_tests"` into the area's aggregation
  `test_suite()` (referencing `"tests"` and `"host_tests"` from the same package in the same
  aggregation package / `BUILD.bazel` file, adding the companion aggregation `test_suite()` there
  and in its `BUILD.gn` `bazel_test_suite()` if only one type exists yet; never declare a
  per-package `bazel_test_suite("bazel_tests")` in `BUILD.gn`; if
  no existing aggregation `test_suite()` can be found, define the package `test_suite()` (omitting
  `visibility` until the aggregation package is known) without guessing an aggregation directory,
  and report the unresolved finding(s) in `CoderReport.disputed_findings` and `summary` asking the
  reviewer which aggregation `test_suite()` (or where a new one, with a matching
  `bazel_test_suite()` in `BUILD.gn`, should be added if none exists) the package's
  `test_suite()`(s) should be added to, suggesting asking the Build team if they do not know), and list generated GN
  test copies in `group("tests")` above the sentinel (host copies as `:<name>($host_toolchain)`).
  Never set `Build-Only: true` on any CL that adds, removes, or modifies test targets or `:tests`
  edges.
- **Starnix unit tests** (`test_type = "starnix"` or `.cml` including `starnix/unit_test.shard.cml`):
  Migrate to `fx_packaged_binary` + `fx_component_manifest` (`component_name` = old GN package name)
  + `fx_test_component(test_type = "starnix", deps = [...])` + `fx_package` + `fx_test` (adding
  `"//src/starnix/kernel/vdso"` to `fx_test_component(deps)` only when the test binary transitively
  links `//src/starnix/kernel/core`). In `BUILD.gn`, `group("tests")` gets `deps = [ ":<pkg>-package" ]`
  and `if (!is_asan) { deps += [ ":bazel_tests" ] }`.
