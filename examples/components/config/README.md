# Config Example

This directory contains a simple example using structured configuration in the
[Component Framework](/docs/concepts/components/v2/introduction.md).


## Building

If these components are not present in your build, they can be added by
appending `--with //examples` to your `fx set` command. For example:

```bash
$ fx set core.x64 --with //examples --with //examples:tests
$ fx build
```

## Running

Use `ffx component run` to launch the config_example component into a restricted realm
for development purposes:

-  **C++**

```bash
$ ffx component run /core/ffx-laboratory:config_example fuchsia-pkg://fuchsia.com/cpp_config_example#meta/config_example.cm
```

-  **Rust**

`--recreate` will delete the existing C++ component and replace it with the Rust component
implementation.

```bash
$ ffx component run /core/ffx-laboratory:config_example fuchsia-pkg://fuchsia.com/rust_config_example#meta/config_example.cm --recreate
```

When the above commands are run, you can see the following output with `fx log`:

```bsh
[04821.191151][508383][508385][ffx-laboratory:config_example] INFO: [main.cc(29)] Hello, World! (from C++)
[04883.083137][514250][514252][ffx-laboratory:config_example] INFO: Hello, World! (from Rust)
```


Use `ffx component show` to see the configuration of this component:

```bash
$ ffx component show /core/ffx-laboratory:config_example
               Moniker:  /core/ffx-laboratory:config_example
                   URL:  fuchsia-pkg://fuchsia.com/rust_config_example#meta/config_example.cm
           Instance ID:  None
                  Type:  CML Component
       Component State:  Resolved
 Incoming Capabilities:  /svc/fuchsia.logger.LogSink
  Exposed Capabilities:  diagnostics
           Merkle root:  20db03957098e49df4119ed9b96c7a7b181787868e830ac54e79ea538d220dfb
         Configuration:  delay_ms -> Uint64(100)
                         greeting -> String("World")
       Execution State:  Running
          Start reason:  Instance was started from debugging workflow
         Running since:  2023-01-31 22:16:14.599032884 UTC
                Job ID:  514225
            Process ID:  514250
 Outgoing Capabilities:  diagnostics
```

### Configuration capabilities

These examples declare the following configuration capabilities in their component manifests:

- `fuchsia.example.config.Greeting` (`string`, `max_size: 512`, default `"World"`): The greeting to log.
- `fuchsia.example.config.DelayMs` (`uint64`, default `100`): Delay in milliseconds before logging greeting.

## Inspect

These examples also publish their configuration to an Inspect VMO.

```bash
$ ffx inspect show core/ffx-laboratory\*config_example
core/ffx-laboratory\:config_example:
  metadata:
    filename = fuchsia.inspect.Tree
    component_url = fuchsia-pkg://fuchsia.com/rust_config_example#meta/config_example.cm
    timestamp = 6758159784100
  payload:
    root:
      config:
        delay_ms = 100
        greeting = World
```

## Testing

Integration tests for structured config are available in the `integration_test` directory.
Use the `ffx test run` command to run the tests on a target device:

```bash
$ ffx test run fuchsia-pkg://fuchsia.com/cpp_config_integration_test#meta/config_integration_test_cpp.cm
$ ffx test run fuchsia-pkg://fuchsia.com/rust_config_integration_test#meta/config_integration_test_rust.cm
```

You should see an integration test for each language execute and pass:

```bash
Running test 'fuchsia-pkg://fuchsia.com/cpp_config_integration_test#meta/config_integration_test_cpp.cm'
[RUNNING]       IntegrationTest.ConfigCpp
[PASSED]        IntegrationTest.ConfigCpp
[RUNNING]       IntegrationTest.ConfigCppReplaceSome
[PASSED]        IntegrationTest.ConfigCppReplaceSome
[RUNNING]       IntegrationTest.ConfigCppReplaceAll
[PASSED]        IntegrationTest.ConfigCppReplaceAll

3 out of 3 tests passed...
fuchsia-pkg://fuchsia.com/cpp_config_integration_test#meta/config_integration_test_cpp.cm completed with result: PASSED

...
```
