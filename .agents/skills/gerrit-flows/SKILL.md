---
name: gerrit-flows
description: >
  Create, list, and delete automated Gerrit Flows on Fuchsia and Turquoise
  changes (CLs), such as adding reviewers or voting Fuchsia-Auto-Submit+1 once
  Presubmit-Verified+1 passes. Use when configuring conditional post-upload
  actions or multi-stage pipelines on a Gerrit CL (via gob-curl or fx gh pr
  flow). Do NOT use for standard code reviews, inline comments, or checking CI
  status (use fuchsia-gerrit-cli or review instead).
---

# Gerrit Flows for Fuchsia & Turquoise CLs

Gerrit Flows automate follow-up actions on a Gerrit change (CL) when specified
Gerrit search conditions are met (evaluated periodically by Gerrit-on-Borg).

## Critical Rules

1.  **Do NOT start `Commit-Queue+1` (`CQ+1`) automatically on upload:** When
    uploading a CL or attaching a Gerrit Flow, do **not** vote `Commit-Queue+1`
    unless the user explicitly asks to trigger presubmit.
2.  **Never use the `submit` flow action:** Direct `submit` is disabled in
    Gerrit configuration (`disabledActions = submit`). Always use `vote
    Fuchsia-Auto-Submit+1` or `vote Commit-Queue+2` instead.
3.  **One action parameter per stage:** Each `stage_expressions` entry executes
    a single action (`add-reviewer` with one email, or `vote` with one
    `<Label>+<Score>`). To perform multiple actions when a condition is met (for
    example, adding a reviewer **and** voting `Fuchsia-Auto-Submit+1`), include
    multiple stages with the same `condition`.
4.  **Canonical condition URL prefix (for raw REST API calls):** Every stage
    `condition` in the Gerrit REST API **must** begin with:
    `https://<host>-review.googlesource.com/c/<project>/+/<change_number> is
    <query>`
    - Use `fuchsia-review.googlesource.com` or
      `turquoise-internal-review.googlesource.com` (never
      `.git.corp.google.com`).
    - Include `/c/<project>/+/<change_number>` (e.g. `/c/fuchsia/+/1936169`) so
      the Gerrit Flows evaluator queries the change by primary key
      (`<project>~<change_number>`) rather than a cross-project change number
      scan.
    - `fx gh` resolves the project and prepends this prefix automatically.
5.  **At most 3 pending flows per change:** Creating a fourth flow fails with
    `409 Conflict` (`Too many pending flows`). Put actions that share a
    condition in one flow, and delete stale flows (`fx gh pr flow delete`)
    before adding new ones.
6.  **Pass `gob-curl` JSON bodies on stdin (`-d @-`):** `gob-curl` drops every
    argument that contains the Gerrit host name, which includes any inline `-d`
    body with a condition URL. Always use a heredoc as in the examples below.

---

## Choosing Between `fx gh` and `gob-curl`

- **Use `fx gh` (Preferred)** when working inside a Fuchsia checkout for
  standard single-condition flows (`pr create`, `pr edit`, `pr flow`). It
  automatically resolves the Gerrit project, prepends the canonical
  `.googlesource.com` condition prefix, splits multiple reviewers/votes into
  separate stages, and formats `--dryrun-at` timestamps.
- **Use `gob-curl` (Gerrit REST API)** when:
  - Running outside a Fuchsia checkout (e.g., `chrome_infra`, `google3`, a
    sub-repo, or a triage environment without `fx gh`), or targeting a change on
    a different Gerrit host/project than your local git remote.
  - Creating a **sequential multi-stage pipeline** in a single flow where
    different stages use different conditions (for example, Stage 1 waits until
    `20:00` to vote `Commit-Queue+1`, and Stage 2 waits for
    `label:Presubmit-Verified+1` to add a reviewer).

---

## Common Conditions & Actions in Fuchsia / Turquoise

### Conditions (`--when` on `pr flow create`, `--flow-when` on `pr create` / `pr edit`)

- `label:Presubmit-Verified+1`: True once LUCI CV presubmit succeeds on the
  current patchset.
- `label:Code-Review+2`: True once the change has a `+2` code review approval.
- `is:submittable`: True once all submit requirements (review, verification,
  resolvable comments) are satisfied.
- `is:open, wait until <today|tomorrow|Weekday> at <HH:MM> in
  America/Los_Angeles`: Time-delayed trigger in US Pacific time.

### Actions (Comma-Separated or Repeatable)
All reviewer and vote flags accept **both comma-separated values**
(`--add-reviewer alice@google.com,bob@google.com`) and **repeated flags**
(`--add-reviewer alice@google.com --add-reviewer bob@google.com`):

- **Add Reviewer (`add-reviewer`):** Adds one or more users (emails) as
  reviewers on the CL.
- **Vote Label (`vote`):** Casts one or more label votes configured on the
  project (`<Label>+<Score>` or `<Label>=<Score>`), such as:
  - `Fuchsia-Auto-Submit+1`
  - `Commit-Queue+1`
  - `Commit-Queue+2`

---

## 1. Using `fx gh` (Preferred)

### Attach Flows During `pr create` or `pr edit`

Use the built-in convenience flags when creating or editing a CL (without
starting `Commit-Queue+1` on upload):

```bash
# Upload a new CL and add a flow that adds reviewers and sets
# Fuchsia-Auto-Submit+1 once Presubmit-Verified+1 is met:
fx gh pr create -t "[component] Fix issue" \
  --reviewer-on-presubmit mohrr@google.com,chaselatta@google.com \
  --autosubmit-on-presubmit

# Equivalent on 'pr create' using explicit --flow-when and --flow-add-reviewer:
fx gh pr create -t "[component] Fix issue" \
  --flow-when "label:Presubmit-Verified+1" \
  --flow-add-reviewer mohrr@google.com,chaselatta@google.com \
  --flow-vote Fuchsia-Auto-Submit+1

# Attach the same flow to an existing CL (note --add-reviewer-on-presubmit on 'pr edit'):
fx gh pr edit 1936169 \
  --add-reviewer-on-presubmit mohrr@google.com,chaselatta@google.com \
  --autosubmit-on-presubmit

# Set Fuchsia-Auto-Submit+1 once the change is submittable (is:submittable):
fx gh pr create -t "[component] Fix issue" \
  --autosubmit-on-submittable
fx gh pr edit 1936169 \
  --autosubmit-on-submittable

# Schedule a Commit-Queue+1 dry run at a specific time (defaults to America/Los_Angeles):
fx gh pr create -t "[component] Fix issue" \
  --dryrun-at "Today at 20:00"
fx gh pr edit 1936169 \
  --dryrun-at "Today at 20:00"

# Attach a flow with a custom condition on pr create or pr edit:
fx gh pr edit 1936169 \
  --flow-when "is:submittable" \
  --flow-vote Commit-Queue+2
```

### Manage Flows Directly with `fx gh pr flow`

```bash
# Create a flow on a change (canonical host + project prefix is prepended to --when automatically):
fx gh pr flow create 1936169 \
  --when "label:Presubmit-Verified+1" \
  --add-reviewer mohrr@google.com,chaselatta@google.com \
  --vote Fuchsia-Auto-Submit+1

# Set Fuchsia-Auto-Submit+1 once the change is submittable:
fx gh pr flow create 1936169 \
  --autosubmit-on-submittable

# Schedule a Commit-Queue+1 dry run at a specific time:
fx gh pr flow create 1936169 \
  --dryrun-at "Today at 20:00"

# List all flows and stage states (PENDING, DONE, FAILED, TERMINATED):
fx gh pr flow list 1936169
fx gh pr flow list 1936169 --json

# Delete a specific flow by UUID, or delete all flows on the change:
fx gh pr flow delete 1936169 <flow-uuid>
fx gh pr flow delete 1936169 --all
```

---

## 2. Gerrit REST API via `gob-curl` (Outside Checkout or Multi-Stage Pipelines)

Use `gob-curl` when `fx gh` is unavailable, when targeting a change in a
different repository/host than your current checkout, or when building a
sequential multi-stage flow where stages have different conditions. Pass the
JSON body on stdin (`-d @-`), never inline (see Critical Rule 6).

### Resolve Project Name (if unknown)

```bash
HOST="https://fuchsia-review.googlesource.com"
CHANGE_NUM="1936169"
PROJECT="$(gob-curl -s "${HOST}/a/changes/${CHANGE_NUM}" | sed 1d | jq -r .project)"
```

### Create a Flow (Same Condition, Multiple Actions)

```bash
HOST="https://fuchsia-review.googlesource.com"
PROJECT="fuchsia"
CHANGE_NUM="1936169"
CONDITION="${HOST}/c/${PROJECT}/+/${CHANGE_NUM} is label:Presubmit-Verified+1"

gob-curl -s -X POST \
  -H "Content-Type: application/json" \
  -d @- \
  "${HOST}/a/changes/${CHANGE_NUM}/flows" <<EOF | sed 1d
{
  "stage_expressions": [
    {
      "condition": "${CONDITION}",
      "action": {
        "name": "add-reviewer",
        "parameters": ["mohrr@google.com"]
      }
    },
    {
      "condition": "${CONDITION}",
      "action": {
        "name": "vote",
        "parameters": ["Fuchsia-Auto-Submit+1"]
      }
    }
  ]
}
EOF
```

### Create a Sequential Multi-Stage Flow (`Stage 1 -> Stage 2`)

Stages in `stage_expressions` are evaluated in order. Use separate conditions
per stage to chain sequential steps (for example, schedule a `Commit-Queue+1`
dry run at `20:00`, then add a reviewer once `Presubmit-Verified+1` passes):

```bash
HOST="https://fuchsia-review.googlesource.com"
PROJECT="fuchsia"
CHANGE_NUM="1936169"
PREFIX="${HOST}/c/${PROJECT}/+/${CHANGE_NUM} is"

gob-curl -s -X POST \
  -H "Content-Type: application/json" \
  -d @- \
  "${HOST}/a/changes/${CHANGE_NUM}/flows" <<EOF | sed 1d
{
  "stage_expressions": [
    {
      "condition": "${PREFIX} is:open, wait until today at 20:00 in America/Los_Angeles",
      "action": {
        "name": "vote",
        "parameters": ["Commit-Queue+1"]
      }
    },
    {
      "condition": "${PREFIX} label:Presubmit-Verified+1",
      "action": {
        "name": "add-reviewer",
        "parameters": ["mohrr@google.com"]
      }
    }
  ]
}
EOF
```

### List Flows

```bash
gob-curl -s "${HOST}/a/changes/${CHANGE_NUM}/flows" | sed 1d
```

### Delete a Flow

```bash
gob-curl -s -X DELETE "${HOST}/a/changes/${CHANGE_NUM}/flows/${FLOW_UUID}"
```
