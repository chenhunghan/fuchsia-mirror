# Workspace Branching

The steps depend on the **execution mode** decided in the [Pre-Migration Checks](pre_migration_checks.md#execution-mode):
* In host and in-container modes, follow [Host and In-Container Modes](#host-and-in-container-modes).
* In workbox mode, follow [Workbox Mode: Host Checkout Readiness](#workbox-mode-host-checkout-readiness) instead.

## Host and In-Container Modes

In in-container mode, run these steps in `/workspace`, the checkout of the workbox the skill runs in.

1. Run `git --no-optional-locks status --porcelain` to check if there are uncommitted changes.
    * If there are uncommitted changes, fail the Workspace Branching step with the reason: "uncommitted changes detected in the branch".

2. Run `git branch --show-current` to retrieve the current branch name and check if it is the **migration branch**.
    * If current branch is not the **migration branch**:
        * If the **migration branch** exists, switch to it.
        * If the **migration branch** does not exist, create the branch and switch to it.

3.  Run `jiri update` to update repository and then `git reset --hard origin/main` to start the **migration branch** from the latest `origin/main`. Planter uploads every migration to Gerrit, so the **migration branch** never holds local commits worth keeping; the reset drops Planter commits left behind by earlier runs, so they are not uploaded again under the next CL.

## Workbox Mode: Host Checkout Readiness

In workbox mode, Planter migrates each **migration group** in a workbox, in the workbox's own checkout. Each workbox moves its checkout to a freshly fetched `origin/main` before it starts a new task. The host checkout (the Fuchsia checkout this skill runs from) is used only for package analysis (reading the `BUILD.gn` files and `fx gn refs`) and as the working directory for `fx gh` and `scripts/planter_task_add_run.sh`.

These checks are read-only. Do not switch branches, run `jiri update`, or run `git reset` or `git clean` in the host checkout, and do not fail because it has uncommitted changes.

1. From the root of the Fuchsia checkout, run `git fetch -q origin main`. It updates only the remote-tracking refs; the working tree and the current branch are untouched.
    * If it fails, fail the Workspace Branching step with the reason: "Failed to fetch origin/main into the host checkout".

2. Compare the commit times of `HEAD` and `origin/main` (`git log -1 --format=%ct HEAD` and `git log -1 --format=%ct origin/main`).
    * If `HEAD` is more than 24 hours older than `origin/main`, tell the user that the host checkout is out of date and that package analysis may not match `origin/main`. Do not fail the step: the [Upstream Change Check](#upstream-change-check) protects each selected package.

3. Run `fx get-build-dir` and check that the directory it prints contains `args.gn`. `fx gn refs` needs a host build directory for package analysis.
    * If it does not, fail the Workspace Branching step with the reason: "Host checkout has no build directory for package analysis".

### Upstream Change Check

In workbox mode, [Migration Package Selection](migration_package_selection.md#3-check-for-an-existing-migration-cl) runs this check for each candidate package, because the host checkout may be behind `origin/main`. For the candidate package directory `<dir>`:

1. Run `git --no-optional-locks status --porcelain -- <dir>`.
    * If it prints anything, tell the user that the host checkout has uncommitted changes in `<dir>`, which package analysis may have read. Do not drop the package for this reason.

2. Run `git log --oneline HEAD..origin/main -- <dir>`.
    * If it prints nothing, the package has not changed upstream since the host checkout; the check passes.
    * Otherwise, re-read the build files of `<dir>` from `origin/main`: `git show origin/main:<dir>/BUILD.bazel` and `git diff HEAD origin/main -- <dir>/BUILD.gn`. If `origin/main` already has a `BUILD.bazel` in `<dir>`, or its `BUILD.gn` changed, the package changed upstream since the host checkout. Otherwise, the check passes.
