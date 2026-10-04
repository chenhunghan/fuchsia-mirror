# Commit Message Guidelines for Bazel Migration CLs

This document defines conventions for formatting Git commit messages (subject line, body, and footers) for GN-to-Bazel migration changelists (CLs) in Fuchsia.

---

## 1. Title / Subject Line Format

The subject line of a migration CL must follow a structured format:

```none
[bazel_migration][<area>] <target-or-directory-reference>
```

### Components

1. **`[bazel_migration]` Prefix:**
   - Every migration CL title **must** begin with `[bazel_migration]`.
   - This tag identifies the change as migrating targets from GN to Bazel.

2. **Area Tag (`[<area>]`):**
   - Follow immediately with the tag that would normally be used for changes to that part of the codebase (e.g. `[drivers]`, `[storage]`, `[starnix]`, `[media]`, `[developer]`).
   - Do **not** place a space between `[bazel_migration]` and the area tag: `[bazel_migration][<area>]`.

3. **Space and Target/Directory Reference:**
   - Follow the area tag with a single space and the canonical label of the directory or target being migrated.
   - The label **must** begin with `//`.
   - **Do NOT begin this part of the title with "Migrate "** (e.g., use `//src/...`, never `Migrate //src/...`). The `[bazel_migration]` tag already communicates that the change is a migration.

---

## 2. Referencing What Was Migrated

Depending on the scope of the migration, use the following conventions for the reference following `[bazel_migration][<area>] `:

### A. Entire Directory Migrated

When migrating all targets defined in a directory's `BUILD.gn`:
- Use the directory label starting with `//`.
- **Format:** `[bazel_migration][<area>] //<dir_path>`
- **Example:**
  ```none
  [bazel_migration][starnix] //src/starnix/lib/selinux
  ```

### B. Single Target in a Directory

When only one target in a `BUILD.gn` file is migrated:
- Specify the full target label including the target name.
- **Format:** `[bazel_migration][<area>] //<dir_path>:<target_name>`
- **Example:**
  ```none
  [bazel_migration][storage] //src/storage/lib/vfs:vfs
  ```

### C. Multiple but Not All Targets in a Directory

When multiple targets (but not all targets) within a single `BUILD.gn` file are migrated:
- Use the directory label and append `" targets"` to the end of the line.
- **Format:** `[bazel_migration][<area>] //<dir_path> targets`
- **Example:**
  ```none
  [bazel_migration][media] //src/media/audio/lib targets
  ```

### D. Multiple Directories Migrated at Once

When a migration spans multiple directories, determine an appropriate way to reference all of them concisely:

1. **Reference the dependent / root target:**
   Use the label of the top-level directory or target that depends on the other migrated directories.
   - **Example:**
     ```none
     [bazel_migration][developer] //src/developer/ffx/lib/isolate
     ```
     *(when migrating `isolate` alongside helper libraries it depends on)*

2. **Group by subsystem or library family:**
   If a dependent target doesn't naturally represent the change, group the directories by conceptual name or subsystem:
   - **Example:**
     ```none
     [bazel_migration][power] power_broker libraries
     ```

3. **List directories under a shared area:**
   If all migrated directories share the area tag (e.g. all under `//src/starnix`), list the specific directory names:
   - **Examples:**
     ```none
     [bazel_migration][starnix] selinux and ebpf libraries
     ```
     ```none
     [bazel_migration][starnix] //src/starnix/lib: selinux, ebpf
     ```

If a good summary cannot be determined or the list of directory names is long, that may be a sign that the scope of this single change is too large and should be broken up, ideally along boundaries that match the order of suggestions above (e.g. by dependent targets/subtrees, by logical groupings, or by directory hierarchies).

---

## 3. Title Length Considerations

- In accordance with the [Fuchsia commit message style guide](../../../docs/contribute/commit-message-style-guide.md), aim to keep the subject line concise (ideally <= 65 characters, and no longer than 72 characters).
- Omitting redundant verbs like "Migrate " helps ensure the title stays within line length constraints even with full `//...` labels.

---

## 4. Commit Message Body & Footers

### Body

The commit body should explain the details of the migration:
- List the specific directories or targets (as appropriate) converted to Bazel (`BUILD.bazel`).
- Note whether GN fallback synchronization was configured via `bazel2gn` (and why GN consumers remain).
- Note any non-trivial conversions (e.g. visibility scoping, `target_compatible_with` constraints, or IDK registration).
- Wrap body lines at **72 characters or fewer**.

### Footers

Include standard Fuchsia issue and verification footers:
- **`Bug:`** Link to the tracking Buganizer issue (e.g., `Bug: 123456789`).
- **`Test:`** List the exact commands used to verify the build and tests (e.g., `Test: fx build`, `Test: fx bazel test //...`). Consolidate entries to at most 3 lines. For detailed formatting, quoting, and consolidation rules, see [`migration_commit_message_test_guidelines.md`](migration_commit_message_test_guidelines.md).
