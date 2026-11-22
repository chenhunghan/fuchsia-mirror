<!-- mdformat off(templates not supported) -->
{% set rfcid = "RFC-0289" %}
{% include "docs/contribute/governance/rfcs/_common/_rfc_header.md" %}
# {{ rfc.name }}: {{ rfc.title }}
{# Fuchsia RFCs use templates to display various fields from _rfcs.yaml. View the #}
{# fully rendered RFCs at https://fuchsia.dev/fuchsia-src/contribute/governance/rfcs #}
<!-- SET the `rfcid` VAR ABOVE. DO NOT EDIT ANYTHING ELSE ABOVE THIS LINE. -->

<!-- mdformat on -->

## Problem Statement

The Fuchsia project is able to produce high quality code faster than we are able
to review that code and incorporate it into the source tree. We want to
integrate code into the source tree faster while maintaining high quality
standards.

## Summary

We propose a new code review policy for Fuchsia which allows owners of
directories to specify the review policy for changes to those directories.
Owners may set reviewer policy based on the risk assessment of a given change.
Namely, if a change is considered to be low risk, then it should be reviewed by
the author and an automated code review system. If a change is higher risk, it
may require one or two additional human reviewers. We propose an automatic
classifier for changes to Fuchsia which assigns one of those categories for each
changelist (CL) and is tuned by feedback from code owners.

## Stakeholders

All Fuchsia developers are stakeholders, particularly people listed in OWNERS
files.

*Facilitator:*

Will Drewry - drewry@google.com

*Reviewers:*

*   Will Drewry - drewry@google.com

*Consulted:*

*   FEC members
*   Fuchsia subsystem OWNERS
*   Security team

*Socialization:*

This RFC went through socialization with various subsystem owners around the
Fuchsia organization.

## Requirements

*   Increase speed of reviewing CLs across the project
*   Lower cost of reviewing CLs across the project
*   Minimize decreases in landed code quality
*   Give code owners more control over review policy in their areas

## Design

### Risk

We will classify code reviews into three risk tiers:

*   **Low (estimated at \~4% of CLs).** Changes unlikely to break anything, and
    where human review would likely not improve the quality of the code
    substantially. Examples include typos in documentation, removing unused
    variables, and clean reverts.
*   **Medium (estimated at \~71% of CLs)**. Changes which require human
    judgement but where adding a second code reviewer would likely not improve
    the quality of the code.
*   **High (estimated at \~25% of CLs).** Changes which are substantially risky,
    or where a second human code reviewer would likely improve the code under
    review. Changes can also be opted into this tier by policies defined
    globally or individually by directory. Examples include changes spanning
    OWNERS boundaries, security-impacting changes, or changes of types that have
    broken frequently in the past.

Classification will be done by a risk assessment agent, using both deterministic
and LLM-based tools.

Humans can set a risk assessment at any time by setting a flag on the Gerrit CL.
Changes will be evaluated on the higher of the two risk assessments (agentic or
human).

This proposal does not change any of the other voting rules required for Gerrit
submission, xReview, or API voting, which are out of
scope for this proposal and will continue to be done by humans.

#### Risk assessment configuration

The risk assessment agent will be configured with project-wide instructions in a
RISK.md file in the root directory. Each directory in the source tree can
include a more specific RISK.md file controlled by the OWNERS of that directory
that the risk assessment agent will consider for changes to files within that
directory and its subdirectories.

RISK.md files will be able to set the minimum risk for any change in a
deterministic manner with a metadata flag at the top of the file. For example,
if the OWNERS of a particular directory want to mandate that all changes in that
directory require two human votes, they can set the minimum risk assessment for
that directory to High.

Changes that create, modify, or delete governance configuration files (such as
`RISK.md`, `REVIEW.md`, or `OWNERS`) will be deterministically classified as
High risk and evaluated against the base revision.

The agent will record its assessment using a flag on the Gerrit CL, which will
affect the voting requirements for submitting that CL. If there are multiple
RISK.md files which apply to a given change, then the agent will apply the
highest of the various risk assessments it could generate for the directories in
that change.

### Agentic Review

All changes, regardless of risk, will be reviewed by a code review agent. The
code review agent will be configured with project-wide instructions in a
REVIEW.md file in the root directory. It will use deterministic and model-based
checks to review code, extending what the project already does with tools like
`shac` and Gerrit's code review agent.

Each directory in the source tree can include a more specific REVIEW.md file
controlled by the OWNERS of that directory (and its ancestor directories,
unless `noparent` is set) that the code review agent will use to review changes
to files within that directory and its subdirectories.

The agent will record its review by posting comments and flags on the Gerrit CL.
When the agent no longer has comments that need to be addressed before landing,
the agent signals its approval for the CL by setting an Agent-Code-Review flag
to \+1, potentially with additional optional comments.

New patchsets to existing changes will also be reviewed by the agent, which may
change its approval flag based on the new patchset. Gerrit automatically resets
the value of `Agent-Code-Review` to `?` when a code-changing patchset is
uploaded.

Changes classified as low risk by the classifier described in the previous
section can land after receiving Agent-Code-Review=+1 without human review.

### Human Review

Humans review changes by posting comments on the Gerrit CL. When a human is
satisfied with the CL, the human approves the CL by setting the Code-Review flag
to \+2.

Medium and high risk changes will require one or two human votes respectively to
be eligible to merge.

For medium risk changes, humans can provide Code-Review=+2 on CLs authored by
themselves or their agents (self-approval). As a result, if a single human is an
OWNER of all the files changed by a CL, the human can land the change without
approval from a second human. For new code-changing patchsets, Gerrit policy
already clears the Code-Review flag, so a new human vote is required.

### Overriding the Agentic Reviewer

The agentic reviewer's decision can be overridden by human votes. Any human \-1
or \-2 vote on a Low risk change will prevent the code from landing with only
agentic review, and if an agent votes \-1 on a change, two human votes
(including at least one OWNER) will be required to override it.

We will track rates of and reasons for Agentic Reviewer overrides to tune the
agentic review process.

### Summary of proposed Gerrit submission rules

We propose to replace our current human voting rules with:

As today, any human Code-Review=-2 vote blocks submission across all risk tiers,
whereas a human Code-Review=-1 vote only blocks zero-human submission on
low-risk changes (remaining non-blocking once human Code-Review=+2 requirements
are met and comment threads are resolved). Assuming no Code-Review=-2 votes, we
propose to replace our current human voting rules with:

*   Changes voted by humans and classifier as **low risk** can submit after
    either:
    *   Agent-Code-Review=+1 and no human Code-Review=-1 votes, if and only if
        the uploader is also a Committer (has CQ+2 privileges). If the uploader
        is not a Committer, then the change is considered Medium risk
    *   satisfying the **medium risk** requirements below (for example, if the
        uploader is not a Committer or a human votes Code-Review=-1)
*   Changes voted by humans and classifier as **medium risk** can submit after
    either:
    *   Agent-Code-Review=+1, and \+2 votes from **at least one** human OWNER on
        all changed files (including the human uploading the change)
    *   Agent-Code-Review of any value, and \+2 votes from **at least two**
        human Committers (possibly including the human uploading the change). In
        addition, OWNERS voting requirements must be satisfied for all changed
        files.
*   Changes voted by humans and classifier as **high risk**
    *   \+2 votes from at least **two** humans including at least one OWNER on
        all changed files

### Judgement

The OWNERS of a given directory are responsible for the quality of the code in
that directory. This policy increases the autonomy of OWNERS relative to the
previous code review policy. We expect OWNERS to demonstrate *good judgement* in
deciding when their medium-risk changes would benefit from review by another
human. Given that the current OWNERS files were populated based on the previous
code review policy, we should not be surprised if teams decide to adjust their
OWNERS files given this new policy.

## Implementation

We have already trained a prototype risk classifier on six months of Fuchsia
changes, and tested it on a separate six months of Fuchsia changes.

### Advisory mode

We propose to run the classifier in an *advisory* mode for a period of weeks. In
this mode, the classifier will propose a classification for each Gerrit change
but no voting rules will be different from today's. We will allow humans to vote
on risk classifications for each change, and use human votes to further tune our
classifier. Teams should use this period to configure RISK.md files for their
directories if desired. RISK.md files will have the same format as existing
SKILL.md files: deterministic key/value frontmatter at the top followed by
model-readable prose.

We will also start running our review agent in advisory mode, with suggested
comments on each Gerrit change. Responses to suggested comments will be used to
tune the agentic reviewer and suggest changes to REVIEW.md files in relevant
directories to improve review performance. Teams should use this advisory period
to configure REVIEW.md files in their directories if desired. REVIEW.md files
have the same format as existing SKILL.md files: deterministic key/value
frontmatter at the top followed by model-readable prose.

We'll work with the security team to identify any directories that should be
excluded from these policies and require human review, and to identify changes
which should be flagged as high risk based on security concerns.

Before we start changing any voting rules, we will report on the risk
distribution of all Fuchsia changes and publish those findings. We will also get
explicit approval from the Fuchsia Engineering Council.

### Medium Risk rollout, in slices

Once we are happy with both the risk classifier and our agentic reviewer (by a
sufficiently low rate of risk classification overrides and a sufficiently high
rate of acceptance of agentic review comments), we will determine the set of
changes for which the risk classifier and agentic reviewer perform best, and
start opting those changes into the rules for Medium risk changes and expand the
set of eligible changes as we are satisfied with the results. We will hold back
a small percentage of changes randomly from Medium risk rules as a control
group.

We will measure the rate of human-caught bugs in Medium risk changes and compare
it to the rate of human-caught bugs in the control group and the rate of
human-caught bugs before this proposal was implemented.

### Low Risk rollout, in slices

Once we are satisfied with the performance of the classifier and agent on low
risk changes, we will start changing submission rules for those changes. We will
gradually expand the set of Low risk changes eligible for the new rules, holding
the rest back as a control group.

We will measure the rate of human-caught bugs in Low risk changes and compare
it to the rate of human-caught bugs in the control group and the rate of
human-caught bugs before this proposal was implemented.

### Ongoing retraining and improvement

We will use comments on CLs made by humans to periodically suggest changes to
REVIEW.md files in relevant directories to improve the performance of the
agentic reviewer.

We will use human votes on risk classifications to retrain our risk classifier
on an ongoing basis. We will additionally suggest changes to RISK.md files based
on human votes.

We will use issues caught by the agent to periodically suggest changes to tests
to reduce the rate of issues posted in CLs.

We will track rates of human-caught bugs if this proposal is implemented and
compare them to before this proposal was implemented. In our testing, we've
defined human-caught bugs as:

*   Human review comments anchored to lines subsequently modified in a new
    patchset, filtered to logic changes instead of style
*   Post-submit regressions, the union of
    *   Reverts of a change, where the revert reason indicates the original
        change was incorrect.
    *   Commits identified as culprits in high priority bugs and postmortems
    *   Fix-forwards of a change, where the fix-forward indicates the original
        change was incorrect.

## Performance

This change does not change system performance.

## Security considerations

Changing voting rules in Gerrit should not significantly change the rate of
security issues submitted compared to today, assuming good faith by code
authors.

We must consider that a malicious code author who is also an OWNER could more
easily submit code with a security hole under these rules. We propose to
mitigate this by holding a random subset of changes back from these rules and
requiring today's human review standards. We will propose changes to these rules
if we find a higher than expected rate of security or code quality issues
introduced with lower human review requirements.

To manage resource costs and avoid prompt injection, we will only run agentic
reviewers automatically on changes uploaded by committers to the project.
Committers will be able to request agentic review for non-committers. We may
change this policy in future if we find that costs are lower and we have
additional mitigations against prompt injection.

We will tighten our guardrails against agents casting votes on behalf of humans,
to the best of our ability. Humans are still responsible for the code they
upload and submit under their own identities.

We will continue our ongoing human and agent-assisted security audits of the
entire codebase and our products.

## Privacy considerations

This change has no direct impact on privacy.

## Testing

Described in the Implementation section of this document.

## Documentation

We will update fuchsia.dev's sections on how to submit changes with the new
voting rules and processes.

## Drawbacks, alternatives, and unknowns

This proposal could lower code quality (including introduction of security or
privacy issues) by allowing OWNERS to effectively self-approve code submissions.

The main alternative to this proposal is to continue requiring two humans to
approve every change. This proposal will let humans focus their attention on the
changes which benefit most from that attention.

This proposal accepts the current state of code review on the project and
introduces standards by which we can improve velocity and mitigate reduction in
code quality. In order for this change to be successful, humans added to a code
review must actually read and comprehend the code being reviewed.

If humans become less effective at code reviews, we will propose additional
changes to code review policy.

## Prior art and references

The software industry as a whole is grappling with an overwhelming code review
volume created by agent-assisted software developers.

This proposal aligns with the direction recently taken by
[Anthropic](https://claude.dev/blog/solving-code-review-at-anthropic/) and
others to introduce risk-based code review.
