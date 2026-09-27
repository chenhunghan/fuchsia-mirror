# FIDL GN to Bazel Attributes Mapping

For general attribute mappings (e.g. `sources` -> `srcs`, `public_deps` -> `deps`) and comment preservation rules, see [`common_attribute_mappings.md`](../../../../references/common_attribute_mappings.md).

For determining proper target visibility, follow the [`determining-bazel-visibility`](../../determining_bazel_visibility/SKILL.md) skill.

## FIDL-Specific Attribute Mappings

Map FIDL-specific GN attributes to Bazel attributes as follows:
  - `sdk_area` -> `api_area`
  - `sdk_category` -> `category`
  - `enable_* = true` -> `enable_* = True`
  - `contains_drivers = true` -> `contains_drivers = True`