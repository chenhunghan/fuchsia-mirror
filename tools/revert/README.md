# `fx revert`

`fx revert` is a host tool that orchestrates pure reverts of submitted Gerrit
changes across Fuchsia repositories (e.g. `fuchsia.git` and `vendor/google`).

It generates authentic Gerrit server-side reverts via Gerrit's REST API,
ensuring that **Rubber Stamper** (`rubber-stamper@appspot.gserviceaccount.com`)
automatically recognizes the reverts and approves them with `Bot-Commit +1` /
`Code-Review +2`.

---

## Features

- **Mechanical Single-CL Reverts**: Reverts a single merged Gerrit change via
  Gerrit's `/revert` REST endpoint. The change must be `MERGED`; its
  `Depends-on` footers are reported but related changes are not reverted
  automatically.
- **Local Jiri Workspace Mapping**: Maps each Gerrit project and host to its
  local repository in the Jiri checkout (including `fx-worktree` checkouts),
  rejecting projects from mismatched hosts and paths outside the checkout.
- **State-Invisible Remote Fetching**: Queries the current tip of the change's
  branch on `origin` and fetches any missing objects by SHA without a refmap,
  so no refs, reflogs, `FETCH_HEAD`, or other user-visible repository state is
  modified. Falls back to the local copy of the change's branch when the
  remote is unreachable.
- **In-Memory Conflict Simulation**: Simulates the planned revert in-memory
  using `git merge-tree` (no working-tree, index, or ref modifications) on top
  of the up-to-date branch tip to verify conflict-free execution before making
  any API calls. Reverts that conflict with later changes are rejected.
- **Topological Wave Graphs**: Builds a dependency graph of the planned reverts
  from intra-repo git ancestry and `Depends-on` footers, and partitions it into
  topological waves of submission clusters that are printed with the plan.
- **Rubber Stamper & Auto-Submit Integration**: Adds Rubber Stamper as a
  reviewer and optionally applies `Fuchsia-Auto-Submit +1` for hands-off tree
  unblocking.
- **Structured Commit Messages**: Properly formats standard Gerrit revert
  metadata including `Reason for revert:`, `Failure Link:`, and `Bug:`
  references.

---

## Installation & Setup

`fx revert` is a Bazel-only `fx` host tool: it is registered in
`//tools/devshell/fx_host_tools.json` and built on demand the first time it is
run (and whenever its sources change), so no `fx set` or `fx build` step is
required.

To build the tool explicitly:

```bash
fx bazel build --config=host //tools/revert:fx_revert
```

---

## Repository Impact & Requirements

`fx revert` reads local Git repositories to plan and simulate reverts, while
the reverts themselves are created through Gerrit's REST API. It never
modifies any user-visible repository state:

- **Running from a Fuchsia checkout**: `fx revert` must be run from within a
  Fuchsia checkout or `fx-worktree`. It locates the checkout via
  `$FUCHSIA_DIR` (or by searching upward from the current directory for
  `.jiri_manifest` / `.jiri_root`) and uses `jiri project` to locate the local
  Git repository for each Gerrit project.
- **Branch, working tree, and local commits**: You do **not** need to be on
  `origin/main`, and uncommitted edits, staged changes, local branches, a
  detached `HEAD`, or local commits have no effect on the tool. All
  simulations run in memory against the target branch tip using
  `git merge-tree --write-tree`.
- **Up-to-date vs. lagging checkouts**: You do **not** need to run
  `jiri update` or `git fetch` beforehand. `fx revert` queries `origin`
  (`git ls-remote`) for the current tip of the change's target branch and
  fetches any missing commit objects directly by SHA with an empty refmap
  (`--refmap=` and `--no-write-fetch-head`). If `origin` cannot be reached
  (e.g. offline), it prints a warning and falls back to the local copy of the
  change's branch: `origin/main`, `jiri/head`, or `HEAD` for `main`, and
  `origin/<branch>` or the local `<branch>` for any other branch (e.g.
  `releases/f30`). If the branch is not available locally, the tool fails
  instead of simulating against `main`. In that case, `jiri update` is only
  required if the reverted commit itself is missing from local history (or
  is newer than the local branch). When offline, the conflict simulation only
  sees commits present in the local fallback ref.
- **Root commits and shallow clones**: Reverting an initial (root) commit, or
  a commit whose parent is missing from a shallow clone, is rejected. Run
  `git fetch --unshallow` or `jiri update` to fetch the missing history.
- **Gerrit credentials**: Gerrit requests are authenticated with `gob-curl`
  when it is installed. Otherwise `fx revert` prints a warning and falls back
  to plain `curl` with `~/.netrc` and `~/.gitcookies`; requests to internal
  hosts or authenticated endpoints may fail without valid cookies.
- **Repository state after running**: User-visible repository state is
  completely untouched. Fetching by SHA with `--refmap=`,
  `--no-write-fetch-head`, and automatic maintenance disabled never modifies
  `HEAD`, `FETCH_HEAD`, local or remote-tracking refs (`refs/heads/*`,
  `refs/remotes/*`, `refs/tags/*`), reflogs, `packed-refs`, the Git index, or
  the working tree, and never triggers `jj` colocation imports. Only
  content-addressed Git objects are added under `.git/objects/`.

---

## Usage

### 1. Dry Run (Preview Plan)
Always preview the revert plan before executing. Running without `--upload`
only prints the plan (dry-run mode is the default):

```bash
fx revert <gerrit_url> --dry-run
```

Example:
```bash
fx revert https://fuchsia-review.googlesource.com/c/fuchsia/+/1783117 \
  -m "Reverting suspect CL due to builder failure of core.arm64-debug" \
  -f "go/bbid/8672361213915190673" \
  -b "12345" \
  --dry-run
```

### 2. Execute a Pure Revert via REST API
Upload the revert to Gerrit with Rubber Stamper reviewer:

```bash
fx revert <gerrit_url> \
  -m "<reason>" \
  -f "<failure_link>" \
  -b "<bug_id>" \
  --upload
```

The full plan summary is always printed before any Gerrit API calls are made.

### 3. Auto-Submit (+1)
To automatically set `Fuchsia-Auto-Submit +1` on the created revert CL:

```bash
fx revert <gerrit_url> \
  -m "Reverting broken build" \
  -f "go/bbid/..." \
  --upload \
  --auto-submit
```

---

## Options & Arguments

- `<gerrit_url>`: **Positional (Required)**. Full Gerrit URL to the change
  (e.g. `https://fuchsia-review.googlesource.com/c/fuchsia/+/123456`).
- `-m, --reason`: Custom reason for the revert in the commit message.
- `-f, --failure-link`: Link to builder failure, log, or test failure (e.g.
  `go/bbid/...` or `https://ci.chromium.org/...`).
- `-b, --bug`: Associated Buganizer issue tracker ID (e.g. `12345` or
  `b/12345`).
- `--upload`: Create and upload the revert CL via Gerrit REST API with Rubber
  Stamper reviewer. Without this flag, the tool runs in dry-run mode.
- `--dry-run`: Preview the planned revert without executing API calls.
- `--auto-submit`: Apply `Fuchsia-Auto-Submit +1` vote to the created revert
  CL (default: off).
- `-h, --help`: Display usage and help information.

---

## Running Unit Tests

Run unit tests via `fx bazel test`:

```bash
fx bazel test --config=host //tools/revert:host_tests
```
