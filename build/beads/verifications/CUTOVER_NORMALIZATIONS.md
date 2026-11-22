# Package cutover normalizations and categories

<!-- Generated from cutover_normalizations.json and cutover_categories.json
     by `verify_package_cutover.py --list-normalizations --markdown`.
     Do not edit by hand. -->

## Normalizations

Differences between a GN package and its Bazel replacement that the
package cutover check (`verify_package_cutover.py`) ignores. The package
content comparison compares the built packages; the build command
comparison compares the compile and link commands. Every report lists
the normalizations it applied.

| ID | Comparison | Match | Reason | Source |
|---|---|---|---|---|
| `cmd_clang_code_rules` | build command comparison | `code=build/beads/scripts/normalize_clang_args.py:_ARGS_TO_IGNORE,_ARG_PREFIXES_TO_IGNORE` | Output, depfile, map-file, driver-mode, environment, RBE, crash-diagnostic and ThinLTO-cache flags differ by build system, not by what is built. Applied in code before comparing; never counted here. | normalize_clang_args.py |
| `cmd_clang_paths` | build command comparison | `code=build/beads/scripts/path_normalizer.py` | GN build-dir and Bazel execroot/output paths are rewritten to common placeholders before comparing. Applied in code. | path_normalizer.py |
| `cmd_rustc_code_rules` | build command comparison | `code=build/beads/scripts/normalize_rustc_args.py:_ARGS_TO_IGNORE` | Extern/-L, --emit, output, sysroot, RBE, API-level cfg, debuginfo, embed-bitcode, strip, metadata, link-arg, linker and extra-filename flags are ignored in code (see the TODOs there). The cutover check adds -Copt-level back, see cmd_rustc_opt_level_readded. | normalize_rustc_args.py |
| `cmd_rustc_opt_level_readded` | build command comparison | `code=build/beads/verifications/verify_package_cutover.py:_OPT_LEVEL_RE` | normalize_rustc_args.py drops -Copt-level; the cutover check re-adds the effective value so that an opt-level change in a cutover is reported. | verify_package_cutover.py |
| `cmd_include_paths` | build command comparison | `category=cutover_diff_cmd_flags`, `item=-(I\|isystem\|iquote).*` | Header search paths differ in layout (GN gen/ and fidling/ dirs vs Bazel _virtual_includes, -isystem for deps). A wrong or missing header fails the build instead of changing the binary. | replay of 0c6fb156878 (snapshot) |
| `cmd_sysroot_path` | build command comparison | `category=cutover_diff_cmd_flags`, `item=--sysroot=.*` | GN uses gen/zircon/public/sysroot and Bazel the fuchsia_platform_sysroot repository; both are the same Fuchsia sysroot. | replay of 0c6fb156878 (snapshot) |
| `cmd_driver_name` | build command comparison | `category=cutover_diff_cmd_flags`, `item=clang(\+\+)?` | GN invokes clang++ and Bazel clang with --driver-mode=g++; the driver mode, not the binary name, decides C++ behavior. | replay of 0c6fb156878 (snapshot) |
| `cmd_compilation_dir` | build command comparison | `category=cutover_diff_cmd_flags`, `item=-ffile-compilation-dir=.*` | Debug-info compilation directory placeholder; GN uses the build dir and Bazel the execroot. No runtime effect. | replay of 0c6fb156878 (snapshot) |
| `cmd_random_seed` | build command comparison | `category=cutover_diff_cmd_flags`, `item=-frandom-seed=.*` | Bazel sets a per-output random seed for reproducible builds. No runtime effect. | replay of 0c6fb156878 (snapshot) |
| `cmd_toolchain_version_define` | build command comparison | `category=cutover_diff_cmd_flags`, `item=-DTOOLCHAIN_VERSION=.*` | GN defines the toolchain version only to invalidate caches when the toolchain changes. | replay of 0c6fb156878 (snapshot) |
| `cmd_crash_diagnostics` | build command comparison | `category=cutover_diff_cmd_flags`, `item=-fcrash-diagnostics=.*` | Only controls compiler crash reproducers. | replay of 0c6fb156878 (snapshot) |
| `cmd_warnings` | build command comparison | `category=cutover_diff_cmd_flags`, `item=-W(?!l,\|a,\|p,).*` | Warning flags do not change generated code. Differences in warning sets are a lint concern, not a device one. | replay of 0c6fb156878 (snapshot) |
| `cmd_link_library_spelling` | build command comparison | `category=cutover_diff_cmd_flags`, `item=-L.*\|-l.*\|.*\.so\|@.*\.rsp\|-Wl,--(start\|end)-group\|-Wl,-(no-)?whole-archive`, `key=.* cpp_link( \w+)?` | On C++ link lines GN passes link stubs and response files by path; Bazel passes -L/-l and whole-archive groups. The set of linked shared libraries is compared by the package content comparison (DT_NEEDED). | replay of 0c6fb156878 (snapshot) |
| `cmd_rust_sysroot_search_path` | build command comparison | `category=cutover_diff_cmd_flags`, `item=(-L(native=)? ?)?\S*zircon/public/sysroot/lib` | GN passes the Zircon sysroot library directory as a rustc -L search path; Bazel's toolchain provides the same sysroot. Search paths do not change the linked libraries, which the package content comparison compares (DT_NEEDED). | live pilot of src/connectivity/policy/reachability |
| `cmd_bazel_zx_all_source_define` | build command comparison | `category=cutover_diff_cmd_flags`, `item=-D_ALL_SOURCE` | Bazel zx_library macros (build/bazel/rules/cc/zx_library_utils.bzl) put _ALL_SOURCE in defines so it propagates to dependents, while GN zx_library applies it only to the library itself (build/config/zircon/BUILD.gn). Only exposes extra libc header declarations. | fxr/1980795 (src/recovery/factory_reset) |
| `cmd_fuchsia_link_pthread` | build command comparison | `category=cutover_diff_cmd_flags`, `item=-pthread`, `key=.* cpp_link( \w+)?` | Upstream Bazel targets such as @com_google_googletest//:gtest propagate linkopts = ["-pthread"]. On Fuchsia link lines Clang translates -pthread to -lpthread, which resolves to the sysroot's empty libpthread.so placeholder (pthreads is built into libc.so) and does not change DT_NEEDED or the output binary. | fxr/1989115 (src/devices/bin/driver_playground) |
| `pkg_cm_generated_manifest_path` | package content comparison | `category=cutover_diff_cm`, `item=\"(obj\|bazel-out)/.*generated_manifest\.cml\",?` | GN fuchsia_unittest_component and Bazel fx_unittest_component write generated .cml manifests under obj/ vs bazel-out/; cmc records the input path without affecting runtime component declarations. | live pilot of src/connectivity/policy/reachability (coder proposal) |

## Categories

Each finding category, its escalation stage and what it can do on a
device. Stage 0 and 1 findings are INFO (Planter reads stage 1
itself); stage 2 findings are WARNING. `cutover_info_phase1_missing`
and `cutover_info_not_compared` mark a cutover that was not verified.

| Category | Stage | Device impact |
|---|---|---|
| `cutover_diff_abi_revision` | 0 | functional: the package targets a different API level and may be rejected |
| `cutover_diff_blob_content` | 0 | functional: data or configuration read at runtime differs |
| `cutover_diff_cm` | 0 | functional: capability routing, program arguments or the runner differ; the component may fail to start or lose access |
| `cutover_diff_cmd_flags` | 0 | performance or functional: see the flag category below |
| `cutover_diff_config` | 0 | functional: runtime configuration defaults differ |
| `cutover_diff_dylib_renamed` | 0 | functional or size: a second copy of the library can end up in blobfs next to the GN one, and users that look it up by the old name fail |
| `cutover_diff_elf_arch` | 0 | functional: the binary does not run on the device |
| `cutover_diff_elf_dynsym` | 0 | functional: code that looks symbols up at runtime may fail |
| `cutover_diff_elf_interp` | 0 | functional: the binary loads with a different dynamic linker |
| `cutover_diff_elf_needed` | 0 | functional: the binary links different shared libraries and may fail to load |
| `cutover_diff_elf_relro` | 0 | functional: a security hardening of the binary differs |
| `cutover_diff_elf_sections` | 0 | functional or performance: for example a missing `.eh_frame` breaks unwinding and crash reports |
| `cutover_diff_elf_soname` | 0 | functional: dependents look the library up by a different name |
| `cutover_diff_elf_stack` | 0 | functional: an executable stack weakens security hardening |
| `cutover_diff_elf_stripped` | 0 | performance: an unstripped binary is larger, so it takes more storage and page-in time |
| `cutover_diff_elf_text_size` | 0 | performance: a large code size change suggests a different optimization level, LTO or allocator |
| `cutover_diff_extra_blob` | 0 | functional or size: usually benign, but it grows the package and can shadow an expected path |
| `cutover_diff_guard` | 1 | functional: the Bazel package changed after phase 1 and now differs from the GN package in ways phase 1 did not report; for records from before version 2, it changed and was not compared again |
| `cutover_diff_missing_blob` | 0 | functional: a component may fail to start or fail to read a file it needs |
| `cutover_diff_package_name` | 0 | functional: the package URL changes, so components and assembly entries that name the old package no longer resolve |
| `cutover_diff_subpackages` | 0 | functional: a subpackaged component may be missing |
| `cutover_info_ambiguous` | 0 | none: several GN packages share the package name, so the cutover was not compared (unverified) |
| `cutover_info_dual_build_only` | 0 | none: the packages stay dual-built and are not cut over |
| `cutover_info_guard_rebased` | 0 | none: the change was rebased since a version 1 phase-1 record, so the guard was skipped |
| `cutover_info_not_compared` | 0 | none: the packages were not compared, so the cutover is unverified |
| `cutover_info_not_covered` | 0 | none: part of a comparison did not run, so differences there are not detected |
| `cutover_info_phase1_missing` | 0 | none: there is no phase-1 record, so the cutover is unverified |
| `cutover_info_tool_unavailable` | 0 | none: a tool or the check itself failed, so part of the comparison is missing |
| `cutover_summary` | 0 | none: the per-cutover summary |
