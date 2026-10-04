# Bazel visibility scoping of exports files() and migration commit-message conventions

- **Learned from:** human review on [CL 1838762](https://fuchsia-review.googlesource.com/c/fuchsia/+/1838762), patchset 3 (task `cl-1838762`)
- **Date:** 2026-10-02
- **Changed:** `checks/commit_message_format.sh`, `checks/manifest.json`, `checks/visibility_audit.sh`

## Root cause

visibility_audit only inspected rule calls with a name attribute, so
exports_files() (which has no name and defaults to public visibility) was never
audited; and no check validated the migration commit subject convention
'[bazel_migration][<subsystem>] //<dir>', leaving the tag choice to the coder.

## Why not a one-off fix

Every migration that exports shard or data files (e.g. test runner shards, test
data) and every migration commit hits these same conventions; fixing one
BUILD.bazel or one commit message would let the next migration reintroduce
public exports_files() and ad-hoc subject tags.
