# Macro / Rule Modification Handoff: examples-components-routing-failed-routing_failed

- **Task Title**: [bazel] Migrate //examples/components/routing_failed
- **Target Directories**: `//examples/components/routing_failed`

## Coder Summary

Aborted migration of //examples/components/routing_failed because //examples/components/routing_failed:echo_server_bad sets check_references = false on an intentionally broken manifest (meta/echo_server_bad.cml with nonexistent program.binary bin/routing_failed_echo_server_oops), which fails CmcValidate (cmc validate-references) in Bazel because fx_component and fx_package under //build/bazel/rules/ and //build/bazel_sdk/fuchsia_rules_common/ do not support check_references = False. In addition, components-routing-failed-example-rust and components-routing-failed-example-cpp depend on unmigrated subpackages //examples/components/routing_failed/rust:echo_client, //examples/components/routing_failed/cpp:echo_client, and //examples/components/routing/rust/echo_server:echo_server (and //examples/components/routing/cpp/echo_server:echo_server needs visibility for //examples/components/routing_failed:__pkg__).

