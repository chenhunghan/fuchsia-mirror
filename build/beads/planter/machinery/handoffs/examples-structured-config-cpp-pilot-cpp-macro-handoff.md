# Macro / Rule Modification Handoff: examples-structured-config-cpp-pilot-cpp

- **Task Title**: [bazel] Migrate //examples/components/config_from_parent/cpp
- **Target Directories**: `//examples/components/config_from_parent/cpp`

## Coder Summary

Stopped and left //examples/components/config_from_parent/cpp unmigrated per PILOT instructions after hitting @rules_fuchsia / @fuchsia_sdk vs. in-tree fx_* rule incompatibilities on fuchsia_structured_config_cpp_elf_lib: (1) under --config=fuchsia_platform, @rules_fuchsia's fuchsia_structured_config_cpp_elf_lib uses fuchsia_fidl_library, fuchsia_fidl_cc_library, and @fuchsia_sdk//pkg/inspect, which fail analysis in @@rules_fuchsia++fuchsia_sdk_ext+fuchsia_sdk//flags:fuchsia_api_level with 'ERROR: An API level has not been specified for this target or the repository.' because in-tree fx_package uses //build/bazel/versioning:api_level instead of transitioning @fuchsia_sdk//flags:fuchsia_api_level; and (2) even with --@fuchsia_sdk//flags:fuchsia_api_level=HEAD, linking fuchsia_structured_config_cpp_elf_lib (@fuchsia_sdk//pkg/inspect -> @fuchsia_sdk//pkg/fdio:fdio) into in-tree fx_cc_binary (//sdk/lib/fdio:fdio) fails in fx_package with 'Multiple files are being installed into the package at lib/libfdio.so' from both @fuchsia_sdk//pkg/fdio and //sdk/lib/fdio. In-tree C++ structured config support in //build/bazel/rules/components/structured_config.bzl (analogous to fuchsia_structured_config_rust_lib) is required first.

