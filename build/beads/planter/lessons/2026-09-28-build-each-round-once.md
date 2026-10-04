# Build each coder round once, not three times

- **Learned from:** slow remediation rounds of the starnix-line-discipline-pstate task
- **Date:** 2026-09-28
- **Changed:** `prompts/coder.md`, `checks/build_verification.sh`

## Root cause

Every coder round ran the same slow builds three times. The coder prompt told
the coder to run `fx build`, the GN dependents, the bazel2gn verifications and
`fx bazel build` by hand ("Build & Test Verification"), then to finish with a
full `run_checks.sh`, whose `build_verification` check runs those same builds.
Planter then ran every check again after the coder reported, rebuilding the
unchanged tree once more. With a full product `fx build` taking minutes, most
of a round was spent rebuilding identical inputs.

## Why not a one-off fix

The prompt now says that `build_verification` is the build: the coder iterates
with `run_checks.sh --skip-build` and targeted `fx bazel build`s of the
directories it touched, runs host tests, and runs ONE full `run_checks.sh` as
its last step. `build_verification` now records a fingerprint of what a passing
run built (this script, the git tree of the whole working tree including
untracked files, the build directories and steps, `args.gn`, the last jiri
update, and its environment) in `<git dir>/planter-build-verification.json`,
and a later run with the same fingerprint reuses that result instead of
rebuilding, reporting a `build_reused` INFO finding. So planter's own check run
after the coder reports costs seconds when nothing changed. The check stays
blocking and deterministic: failures are never reused, a result is only stored
when the tree did not change during the build, any edit, commit-content change,
machinery change or `fx set` invalidates it, it expires after 6 hours
(`PLANTER_BUILD_REUSE_TTL`), and `PLANTER_BUILD_NO_REUSE=1` forces a rebuild.
