# BEADS: Build Evolution with AI Directed Synthesis

## Introduction

BEADS is a backronym for Build Evolution with AI Directed Synthesis. This is the
home directory to collect and organize tools and documentation related to
AI-assisted GN to Bazel migration.

Before more information is available, please refer to the following:

- [Fuchsia AI-Assisted GN-to-Bazel Migration](https://docs.google.com/document/d/1gs24goUKSoA_TzMFDFF_WsJk_7qY2fg_Ue4DkjQAuX8)

## Bazel migration skills

Migration skills are available under [.agent/skills](.agent/skills):

- [`migrating-host-tool-to-bazel`](.agent/skills/migrating_host_tool_to_bazel/SKILL.md): Migrates host tools from GN to Bazel.
- [`migrating-fidl-to-bazel`](.agent/skills/migrating_fidl_to_bazel/SKILL.md): Migrates FIDL libraries under `//sdk/fidl` from GN to Bazel.
- [`syncing-bazel-to-gn`](.agent/skills/syncing_bazel_to_gn/SKILL.md): For targets whose GN definitions cannot yet be deleted, syncs migrated Bazel targets to GN with `bazel2gn`.
- [`determining-bazel-visibility`](.agent/skills/determining_bazel_visibility/SKILL.md): Scopes proper Bazel visibility for migrated targets based on GN reverse dependencies and intelligent grouping.

### Shared Migration References

Common references and templates used across multiple migration skills are maintained under [`references/`](references/):

- [`common_attribute_mappings.md`](references/common_attribute_mappings.md): Universal GN-to-Bazel attribute name mappings and comment preservation standards.
- [`target_compatible_with.md`](references/target_compatible_with.md): Platform and host constraint guidelines for host tools, host tests, and platform-specific targets.

For repository-wide Bazel file conventions (such as copyright headers, load ordering, and package licensing), see [`//build/bazel/references/build_bazel_header_template.md`](../bazel/references/build_bazel_header_template.md).

To make these skills discoverable by Gemini, follow [Skill discovery and
configuration](http://go/fuchsia-skills-guide#skill-discovery-and-configuration)
from Fuchsia skills guide. One common approach is to have the following entry
in your `~/.gemini/config/skills.json`:

```json
{
  "entries": [
    {
      "path": "<FUCHSIA>/build/beads/.agent/skills/"
    }
  ]
}
```

where `<FUCHSIA>` is the root of your Fuchsia checkout directory. Note
that this path should be an absolute path.

## Planter machinery

[planter](planter) holds the shared rules and lessons for planter, an agent
harness that runs GN-to-Bazel migrations and learns from code review. See
[planter/README.md](planter/README.md).
