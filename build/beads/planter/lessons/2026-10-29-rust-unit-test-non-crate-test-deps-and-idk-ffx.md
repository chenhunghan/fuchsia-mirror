# Rust unit test non crate test deps and idk ffx tool atom list mapping

- **Learned from:** inner loop friction on a local task (task `ffx-assembly`)
- **Date:** 2026-10-29
- **Changed:** `checks/gn_attr_parity.sh`, `checks/manifest.json`, `prompts/coder.md`, `tools/run_checks.sh`

## Root cause

Three systemic gaps converged: (1) gn_attr_parity's cross-package
missing_sources coverage only excluded srcs/hdrs/embedsrcs when computing the
unmatched file list and emitted only GN declaration order, whereas diffengine
already matched same-package compile_data entries and emitted the remaining
cross-package //<pkg>/<file> inputs sorted alphabetically; (2) diffengine's
unit_test_parity check required every GN test_deps entry to appear in Rust test
deps/proc_macro_deps, even when the GN target is a non-crate
executable/host-tool (which provides no CrateInfo in Bazel) that is unused by
any unit test in the package; and (3) prompts/coder.md hardcoded
_NON_BUILD_FFX_TOOLS_ATOMS_REQUIRED_BY_BAZEL_SDK for all idk_ffx_tool targets
instead of matching the sdk_molecule in //sdk/BUILD.gn (such as build_host_tools
-> BUILD_FFX_TOOLS_ATOMS or non_build_host_tools -> NON_BUILD_FFX_TOOLS_ATOMS).

## Why not a one-off fix

Any Fuchsia Rust crate or ffx plugin/subtool migration can mix same-package and
cross-package compile_data inputs, carry legacy non-crate executable targets in
GN test_deps that cannot be linked as Rust crates in Bazel, or publish an
idk_ffx_tool atom belonging to build_host_tools or non_build_host_tools in
//sdk/BUILD.gn; handling these patterns via general AST/template resolution in
gn_attr_parity.sh, run_checks.sh, and coder.md prevents false-positive
inner-loop blocks and atom-list misplacements across all future migrations.
