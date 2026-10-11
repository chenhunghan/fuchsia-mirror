# Determining Appropriate `Bug:` Footers for Bazel Migration CLs

> **Note:** All guidance in this document applies to both `Bug:` and `Fixed:` footers.

**TODO:** Populate this document with specific guidelines for determining when and which `Bug:` (or `Fixed:`) footer values to include on GN-to-Bazel migration changelists (CLs) (for example, area-specific migration tracking bugs or issues related to the migrated targets).

For now, AI agents should use whatever method they would normally use to determine whether a `Bug:` or `Fixed:` footer is applicable and which issue ID(s) to specify (such as an issue ID provided in the user prompt, task description, control table, or existing `TODO` comments resolved by the change).

## Core Requirements

- **`Bug:` / `Fixed:` is optional:** Include a `Bug: <issue-id>` or `Fixed: <issue-id>` footer only when a relevant Buganizer issue exists. If no relevant issue applies to the migration, omit the footer entirely.
- **Mandatory verification:** AI agents **MUST** verify that any bug ID(s) included in the commit message are:
  1. **Valid** (the issue actually exists in Buganizer; never fabricate or guess an issue ID),
  2. **In a Fuchsia component** in Buganizer, and
  3. **Relevant** to the migration change being committed.
