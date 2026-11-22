---
name: wlan-review-commit-message
description: >
  Reviews WLAN commit messages for clear intent, supporting context links, and
  alignment with the diff. Use when reviewing committed changes or Gerrit CLs
  in WLAN code, drivers, APIs, or tests. Do NOT use as a standalone review
  orchestrator (use /review instead) or for uncommitted working copies.
---

# WLAN commit message review guidelines

Read the full commit message (from `/COMMIT_MSG`, or via `git show -s
--format=%B` when a shell is available) alongside the diff. Skip this check and
report nothing when reviewing uncommitted working-copy edits. Attach findings to
`/COMMIT_MSG` when supported; otherwise report a general change-level finding.

## Intent over mechanics

- **Explain why, not how:** Flag non-trivial commit messages that only restate
  the diff (such as file-by-file or function-by-function lists) without stating
  the problem being solved, the reason for the approach, or relevant tradeoffs.
- **Stand on its own:** Flag messages that defer the entire explanation to a
  bare issue link (for example, "Fix b/123456") without summarizing the problem
  and approach in prose.

## Links to supporting context

- **Missing context links:** When a commit relies on background context, bug
  reproduction, a spec requirement, an RFC, or a broader migration that is not
  sufficiently explained in the message itself, ask for the relevant reference
  (such as `Bug:`, `Fixed:`, or a doc link). Do not require a `Bug:` line when
  the message already provides complete, self-contained context.
- **Multi-step and companion changes:** When a commit lands only part of a
  feature or relies on another change (same-repo or cross-repo) for its stated
  effect, flag it if that dependency is not already visible in the commit stack
  (local ancestors or Gerrit relation chain, when available) and the message
  neither states its partial scope nor links the tracking bug or companion
  change (`Depends-on:` or `Change-Id`).

## Alignment with the diff

- **Undisclosed changes:** Flag functional changes, altered defaults, timeouts,
  or error handling, dropped checks, or unrelated edits in the diff that the
  commit message omits. Pay close attention to commits claiming "no behavior
  change" or "pure refactor" that change runtime behavior.
- **Unbacked or inaccurate claims:** Flag claims in the commit message (fixed
  edge cases, updated callers, deferred work, resource savings, preserved
  invariants, or newly added tests) that are missing from or contradicted by the
  diff or by direct callers/consumers in the repository. When a message claims a
  runtime effect that depends on how callers invoke or configure the changed
  code, check immediate callers in the tree before accepting the claim.

## Do not flag

- Subject-only messages on genuinely self-explanatory changes (typo fixes,
  `OWNERS` updates, formatting, target reordering, or simple renames).
- Automated mechanical migrations (such as `bazel2gn` syncs) whose subject and
  footers already state the full intent.
- Line length, wrapping, or footer syntax already checked by SHAC and Gerrit
  commit validators (see
  [commit-message-style-guide.md](/docs/contribute/commit-message-style-guide.md)).
