# Buganizer Ticket Template For Bazel Migration

-   Title: `[bazel_migration][<area>] <target-or-directory-reference>`
    -   `<area>`: The tag normally used for changes to that part of the codebase (e.g. `starnix`, `storage`, `developer`).
    -   `<target-or-directory-reference>`: The `//` label of the migrated GN package directory (e.g. `//src/starnix/lib/selinux`), or of the target (e.g. `//src/storage/lib/vfs:vfs`) if only one target is migrated. Do not begin it with "Migrate ".
    -   Example: `[bazel_migration][starnix] //src/starnix/lib/selinux`
-   Description: `<follow the "Ticket Description Template" section>`
-   Component ID: 1379267
-   Hotlist ID: 8619159
-   Parent ID: 571374004
-   Type: Bug
-   Status: Assigned
-   Priority: P2
-   Severity: S2
-   Assignee: `<current user>`
-   Verifier: `<current user>`

## Ticket Description Template

```
Package name: <target-or-directory-reference>

Parent: 571374004

```
