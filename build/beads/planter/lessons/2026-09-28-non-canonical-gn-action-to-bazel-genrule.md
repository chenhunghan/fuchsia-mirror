# Reject shell-computed paths and subshells in Bazel genrule cmd

- **Learned from:** cq failure on CL Ib2e9fab824df6ce0bbd2ff226ef41940c510fff5, patchset 2 (task `starnix-line-discipline-pstate`)
- **Date:** 2026-09-28
- **Changed:** `checks/bazel_minimality.sh`, `checks/manifest.json`

## Root cause

Nothing in the machinery covered how to express a GN action() as a genrule.
bazel2gn only converts a string cmd of the form '$(location <tool>)' plus
literal words, $@ and $<, so the coder improvised. It took the source directory
from '$$(dirname $(location <one arbitrary input>))' and read the GN read_file()
list with a '$$(python3 -c ...)' subshell. No check looked inside genrule
commands, the reviewer prompts only cover target, dependency, visibility and
sentinel parity, and the build steps passed. So the hack reached human review,
and the working tree still has the subshell.

## Why not a one-off fix

Rewriting only this one genrule leaves every future GN action() migration free
to repeat the same shell tricks: dirname of an input, interpreter subshells,
backticks or hard-coded bazel-out paths. A deterministic rule in the shared
BUILD.bazel audit catches any genrule command added by a change in any migrated
package, and its remediation gives the canonical replacements, so the pattern is
fixed before upload instead of being found one review at a time.
