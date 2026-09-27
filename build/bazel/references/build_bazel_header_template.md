# Standard BUILD.bazel File Header Template

This template defines the standard file header and package-level declaration for any newly created `BUILD.bazel` file in the Fuchsia repository.

## Template

```bazel
# Copyright {current_year} The Fuchsia Authors. All rights reserved.
# Use of this source code is governed by a BSD-style license that can be
# found in the LICENSE file.

# load(...) statements go here.

package(default_applicable_licenses = ["//:license"])
```

## Rules and Conventions

1. **Copyright Header:**
   - Use the current calendar year for newly created files.
   - Do not modify or remove existing copyright headers when updating existing `BUILD.gn` files.

2. **Statement Ordering:**
   - In Bazel syntax, all `load(...)` statements must appear before any rule invocations or `package(...)` calls.
   - Place `package(default_applicable_licenses = ["//:license"])` immediately following the `load(...)` block.

3. **Package Visibility:**
   - Do not specify `default_visibility` (with any value) in `package(...)`. Target visibility should be set explicitly on individual targets.
