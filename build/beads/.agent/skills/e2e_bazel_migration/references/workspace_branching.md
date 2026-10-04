# Workspace Branching

1. Run `git --no-optional-locks status --porcelain` to check if there are uncommitted changes.
    * If there are uncommitted changes, fail the Workspace Branching step with the reason: "uncommitted changes detected in the branch".

2. Run `git branch --show-current` to retrieve the current branch name and check if it is the **migration branch**.
    * If current branch is not the **migration branch**:
        * If the **migration branch** exists, switch to it.
        * If the **migration branch** does not exist, create the branch and switch to it.

3.  Run `jiri update` to update repository and `git rebase origin/main` to rebase. Resolve any conflicts if they arise.