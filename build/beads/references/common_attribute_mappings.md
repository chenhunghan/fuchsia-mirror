# Common GN to Bazel Attribute Mappings

This document defines universal attribute conversion conventions and code parity standards when migrating build targets from GN (`BUILD.gn`) to Bazel (`BUILD.bazel`) in Fuchsia.

## Universal Field Mappings

The following attribute renamings apply across languages:

| GN Attribute | Bazel Attribute | Description |
| :--- | :--- | :--- |
| `sources` | `srcs` | Source files for compilation. |
| `headers` | `hdrs` | Public/interface header files (C++). |
| `public_deps` | `deps` | Public/interface dependencies re-exported to consumers. |
| `deps` | `implementation_deps` / `deps` | Internal direct target dependencies. Mapped to `implementation_deps` where supported (e.g. C++ rules), or `deps` otherwise. Strict dependency checking applies: every directly imported or included dependency must be listed. |
| `sdk_area` | `api_area` | API governance area (e.g. for IDK atoms and FIDL libraries). |
| `sdk_category` | `category` | IDK publication category (`partner`, `internal`, `experimental`, etc.). |
| `output_name` | `crate_name` / `name` | Output artifact or crate name. |
| `true` / `false` | `True` / `False` | Boolean values must be capitalized in Starlark. |

---

## Comment Preservation Standards

Except for the file-level copyright header, **you must copy all comments from the `BUILD.gn` file to the `BUILD.bazel` file**:

- **Relative Placement:** If a comment is above or on the same line as an attribute in `BUILD.gn`, place it in the identical relative position with respect to the mapped attribute in `BUILD.bazel`.
  - *Example:* If a comment is directly above `sources = [`, place it directly above `srcs = [` in `BUILD.bazel`.
- **Inline TODOs & Bug Links:** Preserve all inline `TODO(bug)` trackers, design rationale comments, and explanatory notes intact.
- **Do Not Modernize / Reword:** Do not rephrase or drop comments during migration unless the comment refers to GN-specific mechanisms that are completely eliminated or to match Bazel style label syntax.

---

## Target Parity & Target Naming

- **Exact Naming Parity:** Target names defined in `BUILD.bazel` must match the corresponding legacy target names in `BUILD.gn`.
- **No Unmapped Attributes:** Every attribute declared on a GN target should either be mapped to an equivalent Bazel attribute or intentionally handled (e.g. GN-specific configurations replaced by Bazel toolchains or features).

---

## Related Skills and Scoping

- **Target Visibility:** For determining appropriate visibility and translating visibility declarations from GN to Bazel, follow the [`determining-bazel-visibility`](../.agent/skills/determining_bazel_visibility/SKILL.md) skill.
- **Host Tool Platform Constraints:** For setting `target_compatible_with`, refer to [`target_compatible_with.md`](target_compatible_with.md).
- **Syncing to GN:** For syncing library targets back to GN when unmigrated dependers remain, follow [`syncing-bazel-to-gn`](../.agent/skills/syncing_bazel_to_gn/SKILL.md).
