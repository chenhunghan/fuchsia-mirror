# Macro / Rule Modification Handoff: examples-structured-config-cpp-pilot

- **Task Title**: [bazel_migration] //examples/components/config_from_parent/cpp
- **Target Directories**: `//examples/components/config_from_parent/cpp`, `//examples/components/config_from_parent/integration_test`

## Coder Summary

Leave examples/components/config_from_parent/integration_test unmigrated because its test packages are blocked by unmigrated external dependencies //examples/components/config_from_parent/cpp:component and //examples/components/config_from_parent/rust:component. In //examples/components/config_from_parent/cpp, using fuchsia_structured_config_cpp_elf_lib from @rules_fuchsia//fuchsia:defs.bzl with in-tree fx_component_manifest and fx_cc_binary fails Bazel analysis under --config=fuchsia_platform with 'in fuchsia_api_level rule @@rules_fuchsia++fuchsia_sdk_ext+fuchsia_sdk//flags:fuchsia_api_level: ERROR: An API level has not been specified for this target or the repository', mixes @fuchsia_sdk C++/FIDL bindings with in-tree //sdk/lib/fidl/cpp, and is rejected by shac bazel_repository_paths. Defining an in-tree fuchsia_structured_config_cpp_elf_lib macro in //build/bazel/rules/components/structured_config.bzl is required before migrating //examples/components/config_from_parent/cpp and //examples/components/config_from_parent/integration_test.

