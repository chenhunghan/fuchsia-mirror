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

`HOST_CONSTRAINTS` pins the host CPU as well as the OS, so it breaks building host tools for the other host CPU (for example, linux-arm64 tools on an x64 builder).

Other common mistakes:

- Don't substitute `["@platforms//os:linux"]` for `HOST_OS_CONSTRAINTS`.
- Put the same `target_compatible_with` on an `alias` as on its target, so the GN group bazel2gn generates lands in the same `if` block.

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

*Note: Omit `target_compatible_with` only if the library really builds for both host and Fuchsia. A missing `is_host` guard in GN doesn't prove that: if only host tools depend on the library, it still needs `HOST_OS_CONSTRAINTS`.*

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

Prefer a CPU constraint like this over a `select()` plus a `# @bazel2gn:raw_overwrite:` comment. bazel2gn already turns the constraint into a GN `if` block.

---

## 4. `select()` and Constraint Pitfalls

- `target_compatible_with` takes `constraint_value` labels, not `config_setting` labels.
- Every label in every branch of a `select()` must exist, even branches your build never picks. `bazel query` and `genquery` follow all branches.
- A `select()` can't be a single element inside a list. Concatenate it with the rest of the list instead: `deps = [...] + select({...}) + [...]`.
