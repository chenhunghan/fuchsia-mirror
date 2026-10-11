# Transitive dependency scope traversal across existing bazel packages

- **Learned from:** inner loop friction on a local task (task `starnix-wave-next`)
- **Date:** 2026-10-05
- **Changed:** `checks/bazel_minimality.sh`

## Root cause

migrated_dependency_packages() in checks/bazel_minimality.sh only expanded
frontier packages that were themselves in candidates (packages whose BUILD.bazel
was added or modified in the current change). When a target package depended on
an already-migrated intermediate BUILD.bazel committed at the change base that
in turn referenced an unmigrated dependency package (or a transitive .shard.cml
exports_files package), traversal stopped at the unmodified intermediate package
and falsely flagged the transitive dependency packages as
out_of_scope_package_modified.

## Why not a one-off fix

Allowlisting individual library or component shard paths in
checks/bazel_minimality.sh would fail whenever any future migration encounters
another already-checked-in BUILD.bazel that references an unmigrated transitive
dependency or unexported component manifest shard.
