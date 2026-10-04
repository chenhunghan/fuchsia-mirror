# Migration commit message conventions: subsystem tag selection (including no-parent-subsystem case) and Bazel Test: footer command form/ordering

- **Learned from:** human review on [CL 1838761](https://fuchsia-review.googlesource.com/c/fuchsia/+/1838761), patchset 3 (task `cl-1838761`)
- **Date:** 2026-10-02
- **Changed:** `checks/build_verification.sh`, `checks/commit_message_format.sh`, `checks/manifest.json`, `prompts/coder.md`

## Root cause

commit_message_format only accepted '[bazel_migration][<tag>] //dir' and never
considered the no-subsystem form; nothing linted Test: footers for fx bazel
without --config, labels without @, redundant //a/b:b labels, :all builds
instead of test targets, or bazel2gn verification form/ordering, and coder.md
told the coder to copy dry-run commands that use the discouraged forms.

## Why not a one-off fix

Every migration CL writes a subject and Test: footers; fixing one commit message
leaves future coders producing the same invalid fx bazel commands and wrong
tags, so the rule must live in the deterministic commit-message check that runs
on all tasks.
