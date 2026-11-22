# Structured Configuration

Structured configuration allows C++/Rust components to declare configuration schemas directly in
their manifest. Benefits of using structured configuration include:

* Errors in configuration are detected at build and assembly time.
* Multiple packages can be created using the same component and different configuration values.
* Components read their configuration with statically-typed libraries.
* Component Framework only starts components with valid configuration.
* Configuration can be viewed at runtime with `ffx` tooling.
* Values can be set at runtime.

For more details on the implementation and behavior of structured configuration, see its
[reference](/docs/reference/components/structured_config.md).

To use structured configuration in your component, you must update build rules, declare a schema,
define values, and generate a client library.

## Update build rules

To prevent cyclic dependencies when generating client libraries, define a rule that compiles the
component manifest (`fx_component_manifest` in Bazel or `fuchsia_component_manifest` in GN). Pass
the label of this compiled manifest to the component rule (`fx_component` or `fuchsia_component`).

```bazel
{% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/cpp/BUILD.bazel" region_tag="component" adjust_indentation="auto" %}
```

## Declare configuration schema

You must declare a configuration schema in a component's manifest by adding
[configuration capabilities][config-capabilities] to the `use` block:

```json5
{
    ...
{% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/rust/meta/config_example.cml" region_tag="config" %}
}
```

Each configuration capability entry in `use` specifies:

* `config`: The capability name used when routing configuration from a parent or product assembly.
* `key`: The field identifier generated in the component's structured configuration client library.
* `type`: The value's data type and constraints.
* `availability` (optional): Set to `"optional"` or `"transitional"` when the field has a `default`
  value in the manifest.
* `default` (optional): The fallback value used when the capability is routed from `void` (or
  unrouted with `"transitional"` availability).

Structured config supports the following types:

* `bool`
* `uint8`, `int8`
* `uint16`, `int16`
* `uint32`, `int32`
* `uint64`, `int64`
* `string` (requires `max_size` property)
* `vector` (requires `element` and `max_count` properties)

See the [CML reference doc][cml-ref-doc] and
[Configuration capabilities][config-capabilities] for the complete syntax.

Once your component declares its configuration capabilities, values can be provided directly via
`default` in the component manifest, routed from a parent component, or configured using Software
Assembly.

**Googlers only**: if your component is configured differently for eng and non-eng
[build types][build-types], you must
[add verification of your structured configuration][sc-verification] before you check it in.

## Define configuration values using Software Assembly

If your component's configuration varies between products, see the documentation
for [Assembling Structured Configuration][sa-sc-docs].

## Package the component {#define-package-configuration-values-using-gn}

To package the component, add it to an `fx_package` (Bazel) or `fuchsia_package` (GN) rule:

* {C++}

  ```bazel
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/cpp/BUILD.bazel" region_tag="package" adjust_indentation="auto" %}
  ```

* {Rust}

  ```gn
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/rust/BUILD.gn" region_tag="package" adjust_indentation="auto" %}
  ```

The build system verifies your component's configuration schema. A component with a faulty
configuration (for example: invalid type constraints or mismatched default values) will fail to
build.

## Checking the configuration

Component manager validates a component's configuration when the component is started.

Use `ffx component show` to print out a component's configuration key-value pairs. The component
does not have to be running for this to work.

```
$ ffx component show config_example
                Moniker: /core/ffx-laboratory:config_example
        Component State: Resolved
                      ...
          Configuration: delay_ms -> Uint64(100)
                         greeting -> String("World")
                      ...
```

Components with unresolved or invalid required configuration capabilities will fail to start.

## Reading the configuration

Components read their resolved configuration values with a generated library. Generate a library
using the following build rules:

* {C++}

  ```bazel
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/cpp/BUILD.bazel" region_tag="binary" adjust_indentation="auto" %}
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/cpp/BUILD.bazel" region_tag="library" adjust_indentation="auto" %}
  ```

* {Rust}

  ```gn
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/rust/BUILD.gn" region_tag="binary" adjust_indentation="auto" %}
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/rust/BUILD.gn" region_tag="library" adjust_indentation="auto" %}
  ```

Use the following functions from the library to read configuration values:

* {C++}

  ```cpp
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/cpp/main.cc" region_tag="imports" adjust_indentation="auto" %}

  ...

  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/cpp/main.cc" region_tag="get_config" adjust_indentation="auto" %}
  ```

* {Rust}

  ```rust
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/rust/src/main.rs" region_tag="imports" adjust_indentation="auto" %}

  ...

  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/rust/src/main.rs" region_tag="get_config" adjust_indentation="auto" %}
  ```

## Export configuration to Inspect

You can export a component's configuration to Inspect so that it is available in
crash reports. The client libraries have functions to export a component's configuration to an
Inspect tree:

* {C++}

  ```cpp
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/cpp/main.cc" region_tag="inspect" adjust_indentation="auto" %}
  ```

* {Rust}

  ```rust
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/rust/src/main.rs" region_tag="inspect" adjust_indentation="auto" %}
  ```

Use `ffx inspect show` to print out the component's exported configuration:

```
$ ffx inspect show core/ffx-laboratory\*config_example
core/ffx-laboratory\:config_example:
  ...
  payload:
    root:
      config:
        greeting = World
```

## Runtime values from outside the component

Components receive configuration values routed as
[configuration capabilities][config-capabilities] from their parent component, or fall back to
their manifest `default` value when `availability` is `"optional"` (and routed from `void`) or
`"transitional"` (and unrouted).

Note: When a component receives configuration capabilities from external parent components, it
must take care to [evolve][sc-evolution] its schema in coordination with any
external providers of that configuration.

[sc-evolution]: /docs/development/components/configuration/evolving_structured_config.md

### Providing values from parent components

Parent components can define configuration capabilities in their `capabilities` block and offer
them to static children via `offer` declarations, or offer them dynamically to children launched
in a collection with `allowed_offers: "static_and_dynamic"`.

First, the child declares the configuration capability in its `use` block:

```json5
{
    ...
{% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config_from_parent/rust/meta/config_example.cml" region_tag="config" %}
}
```

When launching the child in a collection with the `Realm` protocol, pass a dynamic configuration
capability offer in `CreateChildArgs`:

* {C++}

  ```cpp
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config_from_parent/integration_test/cpp/test.cc" region_tag="create_child" adjust_indentation="auto" %}
  ```

* {Rust}

  ```rust
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config_from_parent/integration_test/rust/lib.rs" region_tag="create_child" adjust_indentation="auto" %}
  ```

See the [full example](/examples/components/config_from_parent) for details.

#### Providing values with `ffx component`

You can override configuration capability values for a component from the command line using
`ffx component config set`.

If you run the above configuration example without any overrides you can see it
print the default configuration to its logs:

```
$ ffx component run /core/ffx-laboratory:hello-default-value fuchsia-pkg://fuchsia.com/rust_config_from_parent_example#meta/config_example.cm --follow-logs
...
[02655.273631][ffx-laboratory:hello-default-value] INFO: Hello, World! (from Rust)
```

Using `ffx component config set --reload` allows you to override that greeting and reload the
component instance with the override in effect:

```
$ ffx component config set --reload /core/ffx-laboratory:hello-default-value "greeting=parent component"
...
[02622.752978][ffx-laboratory:hello-default-value] INFO: Hello, parent component! (from Rust)
```

Each configuration field is specified as `KEY=VALUE` where `KEY` is the `key` of a configuration
capability declared in the component's manifest and `VALUE` is a string matching the type of the
field.

### Testing with Realm Builder {#testing-with-realm-builder}

You can use [Realm Builder][rb-feature-matrix] to define and route configuration capabilities to a
component in integration tests.

TODO(https://fxbug.dev/42053123) Add a section covering use with subpackaged components.

* {C++}

  ```cpp
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/integration_test/cpp/test.cc" region_tag="config_replace" adjust_indentation="auto" %}
  ```

* {Rust}

  ```rust
  {% includecode gerrit_repo="fuchsia/fuchsia" gerrit_path="examples/components/config/integration_test/rust/lib.rs" region_tag="config_replace" adjust_indentation="auto" %}
  ```

Realm Builder validates the capability value against the component's configuration schema. Any
optional or transitional configuration capabilities that are not routed in the test realm (or are
routed from `void`) resolve to the `default` values declared in the component's manifest.

<!-- TODO(https://fxbug.dev/42055979): Link to fxbug.dev page when better documentation is available.  -->
[build-types]: /docs/contribute/governance/rfcs/0115_build_types.md
[cml-ref-doc]: https://fuchsia.dev/reference/cml#use
[config-capabilities]: /docs/concepts/components/v2/capabilities/configuration.md
[sa-sc-docs]: assembling_structured_config.md
[rb-feature-matrix]: /docs/development/testing/components/realm_builder.md#language-feature-matrix
[sc-verification]: /docs/development/verification/build_integration.md#verifying-structured-configuration-files
