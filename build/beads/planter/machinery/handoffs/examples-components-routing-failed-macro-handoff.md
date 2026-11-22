# Macro / Rule Modification Handoff: examples-components-routing-failed

- **Task Title**: [bazel_migration] //examples/components/routing_failed
- **Target Directories**: `//examples/components/routing_failed`, `//examples/components/routing_failed/rust`

## Coder Summary

Aborted migration of //examples/components/routing_failed because //examples/components/routing_failed:echo_server_bad sets check_references = false on an intentionally broken manifest (meta/echo_server_bad.cml with nonexistent program.binary bin/routing_failed_echo_server_oops), which fails CmcValidate (cmc validate-references) in Bazel because fx_component and fx_package under //build/bazel/rules/ and //build/bazel_sdk/fuchsia_rules_common/ do not support check_references = False. In addition, //examples/components/routing_failed:components-routing-failed-example-cpp depends on unmigrated //examples/components/routing_failed/cpp:echo_client, and //examples/components/routing/cpp/echo_server:echo_server and //examples/components/routing/rust/echo_server:echo_server need visibility granted to //examples/components/routing_failed:__pkg__.

