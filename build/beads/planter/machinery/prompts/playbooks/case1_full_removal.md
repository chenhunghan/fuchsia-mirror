# Playbook: Case 1 Full BUILD.gn Removal

Use when nothing GN-only has to stay in the package (e.g. a pure `fuchsia_package` /
`fuchsia_package_with_single_component` directory with no GN callers).
- Convert every GN target to its Bazel equivalent (`fx_package`, `fx_component`,
  `fx_component_manifest`, `fx_packaged_binary`, `resource`, or library/binary rules; set
  `package_repository_name = "fuchsia.com"` on any `fx_package` in `//bundles/assembly`
  `cache_packages`/`flexible_packages`/`on_demand_packages`/`drivers`) and delete
  `BUILD.gn` (`git rm <dir>/BUILD.gn`).
- With no GN copy left to avoid colliding with, name `fx_package`/`fx_unittest_package` targets
  after the package and omit `package_name` (it defaults to `name`).
- Tests never force Case 2: every test (C++ or Rust, device or host) migrates to Bazel per
  "Tests" in the coder prompt, using `fx_unittest_package` / `fx_unittest_component`
  (for unit tests without custom manifests) or explicit `meta/<component>.cml` and
  `fx_test`. Expose them via a package `test_suite(name = "tests" | "host_tests",
  tests = [":<test_target>", ...], visibility = ["//<aggregation_dir>:__pkg__"])` always listing
  explicit test targets in `tests = [...]` (never `tests = []` or omitting `tests`; if modifying an
  existing `test_suite()` that had an empty `tests` attribute, add all existing package test targets
  to `tests = [...]` or start from the existing list and remove entries, and report
  `test_suite_layout/expanded_empty_test_suite` in `CoderReport.disputed_findings` and `summary`
  requesting that a human ensure all relevant tests have been included without allowing agents to
  resolve the comment) as the first target in `BUILD.bazel` (keeping individual test
  targets private), wire `"//<dir>:tests"` or `"//<dir>:host_tests"` into the
  area's aggregation `test_suite()` (referencing `"tests"` and `"host_tests"` from the same
  package in the same aggregation package / `BUILD.bazel` file, adding the companion aggregation
  `test_suite()` there and in its `BUILD.gn` `bazel_test_suite()` if only one type exists yet),
  and delete `BUILD.gn` (`git rm <dir>/BUILD.gn`). Never keep a
  per-package `BUILD.gn` solely for `bazel_test_suite` and `group("tests")`; if no existing
  aggregation `test_suite()` can be found for `<dir>`, define the package `test_suite()` (omitting
  `visibility` until the aggregation package is known) without guessing an aggregation directory,
  and report the unresolved finding(s) in `CoderReport.disputed_findings` and `summary` asking the
  reviewer which aggregation `test_suite()` (or where a new one, with a matching
  `bazel_test_suite()` in `BUILD.gn`, should be added if none exists) the package's
  `test_suite()`(s) should be added to (suggesting asking the Build team if they do not know).
- For assembly packages bridged via `//bundles/assembly/bazel_inputs/<dir>`, switch
  `//bundles/assembly/BUILD.bazel` to `//<dir>:<pkg>`, remove the entry from
  `//bundles/assembly/bazel_inputs/BUILD.gn`, and delete
  `//bundles/assembly/bazel_inputs/<dir>/BUILD.*`.
