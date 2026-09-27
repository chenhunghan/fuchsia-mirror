# Target Platform Constraints (target_compatible_with)

For target(s) in the `BUILD.gn` file guarded by `is_host` (e.g. `if (is_host)` or `assert(is_host)`), the equivalent target(s) in the `BUILD.bazel` file must specify the `target_compatible_with` attribute appropriately.
This is especially relevant when migrating host tools, host tests, and the libraries they use.

There may also be targets that are not guarded in the `BUILD.gn` file that nonetheless should have `target_compatible_with` set according to the guidance below.

---

## 1. Host Tools and Related Tests and Libraries only supported on Host Platforms

These are often but not always guarded by `is_host` conditions or asserts in the `BUILD.gn` file.

1. Add the load statement to `BUILD.bazel`:
   ```bazel
   load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
   ```
2. Set `target_compatible_with` on the target:
   ```bazel
   target_compatible_with = HOST_OS_CONSTRAINTS,
   ```

Do NOT use the following when migrating targets.
In general, these should very rarely be used and only by members of the Build team.
```bazel
load("@platforms//host:constraints.bzl", "HOST_CONSTRAINTS")
```
```bazel
target_compatible_with = HOST_CONSTRAINTS,
```

### Examples

#### Host Binary Tool
```bazel
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules/host:defs.bzl", "go_binary_host_tool")

package(default_applicable_licenses = ["//:license"])

go_binary_host_tool(
    name = "my_tool",
    srcs = ["main.go"],
    target_compatible_with = HOST_OS_CONSTRAINTS,
)
```

#### IDK Host Tool
```bazel
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("//build/bazel/rules/idk:idk_host_tool.bzl", "idk_go_binary_host_tool")

package(default_applicable_licenses = ["//:license"])

idk_go_binary_host_tool(
    name = "idk_tool_target",
    api_area = "Developer",
    category = "partner",
    target_compatible_with = HOST_OS_CONSTRAINTS,
)
```

#### Host-Only Library
```bazel
load("//build/bazel/platforms:constraints.bzl", "HOST_OS_CONSTRAINTS")
load("@io_bazel_rules_go//go:def.bzl", "go_library")

package(default_applicable_licenses = ["//:license"])

go_library(
    name = "my_host_lib",
    srcs = ["lib.go"],
    target_compatible_with = HOST_OS_CONSTRAINTS,
)
```

*Note: If the library is not guarded by `is_host` in GN (i.e. it is platform-agnostic), omit `target_compatible_with`.*

---

## 2. Fuchsia-Only Targets

For targets that should only build on Fuchsia (relevant for `!is_host`, `is_fuchsia`, or targets containing Fuchsia-specific dependencies):

Set `target_compatible_with = ["@platforms//os:fuchsia"]`:

```bazel
package(default_applicable_licenses = ["//:license"])

cc_library(
    name = "my_fuchsia_lib",
    srcs = ["fuchsia_lib.cc"],
    hdrs = ["fuchsia_lib.h"],
    target_compatible_with = ["@platforms//os:fuchsia"],
)
```

---

## 3. Architecture-Specific Targets

For targets that are restricted to specific CPU architectures (e.g. `current_cpu == "x64"` or `current_cpu == "arm64"` in GN):

Use `@platforms//cpu:<arch>` constraints (such as `@platforms//cpu:x86_64`, `@platforms//cpu:arm64`, `@platforms//cpu:riscv64`):

```bazel
package(default_applicable_licenses = ["//:license"])

# Fuchsia target restricted to x86_64
cc_library(
    name = "my_x64_lib",
    srcs = ["x64.cc"],
    target_compatible_with = [
        "@platforms//os:fuchsia",
        "@platforms//cpu:x86_64",
    ],
)

# Host target restricted to specific CPU architecture
go_binary_host_tool(
    name = "my_x64_host_tool",
    srcs = ["main.go"],
    target_compatible_with = [
        "@platforms//cpu:x86_64",
    ] + HOST_OS_CONSTRAINTS,
)
```
