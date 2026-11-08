<!-- Generated with `fx rfc` -->
<!-- mdformat off(templates not supported) -->
{% set rfcid = "RFC-0000" %} <!-- TODO: DO NOT SUBMIT, update number -->
{% include "docs/contribute/governance/rfcs/_common/_rfc_header.md" %}
# {{ rfc.name }}: {{ rfc.title }}
{# Fuchsia RFCs use templates to display various fields from _rfcs.yaml. View the #}
{# fully rendered RFCs at https://fuchsia.dev/fuchsia-src/contribute/governance/rfcs #}
<!-- SET the `rfcid` VAR ABOVE. DO NOT EDIT ANYTHING ELSE ABOVE THIS LINE. -->

<!-- mdformat on -->

<!-- This should begin with an H2 element (for example, ## Summary).-->

## Problem Statement

It is self-evident that an engineer will be less effective on their first day
working on a project than on their 100th, but how much less effective is a
function of how idiosyncratic that project is. The more familiar the development
environment, the smaller the gap. Historically, onboarding has been rare, so the
impact of an unusual development environment was limited. However, by the nature
of how coding agents are trained, every session is an agent's "first day on the
project." As agents do more of the day-to-day work interacting with the
codebase, the cost of idiosyncrasy is higher than it has ever been.

Today, setting up and building Fuchsia requires bespoke tools and workflows that
exist nowhere else: bootstrapping `jiri`, fetching a custom manifest of
repositories and CIPD packages, and running `jiri` hooks. The author of this RFC
spent several months on this project without understanding the relationship
between `fuchsia.git` and `integration.git`, and six years later is still not
fully clear on how to make `jiri` download all the right dependencies after
checking out a change not based on `JIRI_HEAD`. Every step that diverges from
industry-standard tools like `git` adds friction for human engineers, slows down
ephemeral workspace provisioning, and forces coding agents off their
well-trained paths.

## Summary

Setting up and building a Fuchsia environment will be as simple as:

```posix-terminal
git clone $FUCHSIA_URL && cd fuchsia && ./scripts/fx set $PRODUCT.$BOARD && ./scripts/fx build
```

Similarly, updating an existing checkout and rebuilding will be as simple as:

```posix-terminal
git pull --rebase origin main && ./scripts/fx build
```

We will remove `jiri` from standard Fuchsia developer workflows and require that
an open-source Fuchsia environment can be fetched, updated, and built using only
`git` and the build tool. A typical developer should not need to use or think
about `jiri`. Internal checkouts may require an additional step to include
internal sources.

## Stakeholders

_Facilitator:_

* leannogasawara@google.com

_Reviewers:_

* abarth@google.com
* digit@google.com
* olivernewman@google.com

_Consulted:_

* chaselatta@google.com

_Socialization:_

A previous version of the document (which was more of a design document and less
of a requirements document) was previously approved by abarth@google.com and
olivernewman@google.com.

## Requirements

* **Standard open-source workflow:** Checking out and building open-source
  Fuchsia MUST only require `git` and the build tool. Specifically, running
  `git clone $FUCHSIA_URL && cd fuchsia && ./scripts/fx set $PRODUCT.$BOARD && ./scripts/fx build`
  MUST succeed without any custom bootstrap script or `jiri` invocation. We are
  working towards migrating to `bazel`, so the `fx` scripts may or may not be
  updated to be `bazel` invocations.
* **No `jiri` in standard developer workflows:** Standard developer workflows
  for fetching and building Fuchsia MUST NOT require `jiri`. A typical developer
  or coding agent should not need to invoke or think about `jiri` to check out
  source code, third-party dependencies, or prebuilt toolchains and binaries.
* **No `git` submodules en masse:** We MUST NOT use `git` submodules at scale to
  replace `jiri` repositories. Direct use of `git` submodules is uncommon enough
  that we consider it a non-standard tool. However, we MAY use a small number of
  submodules (ideally one) for `vendor` repositories that require separate
  repository-level ACLs, for lack of a better alternative.
* **Internal checkouts:** Internal checkouts (which include proprietary sources
  such as `//vendor/*`) MAY require an additional step after `git clone` to
  incorporate internal sources before running the build tool.
* **Implementation flexibility:** This RFC does not mandate a specific technical
  mechanism for how fetching dependencies is divided between `git` and the build
  tool, provided the workflow requirements above are met.

## Design

### Distributing Responsibilities Between `git` and the Build Tool

Historically, setting up a Fuchsia workspace has required bootstrapping `jiri`,
which then assembles the checkout by:

1. Fetching the global `integration` repository into `//integration`, which
   pins the revision of `fuchsia.git` (in `//integration/fuchsia/stem`) and
   imports manifest files from both `//integration` and `//manifests` in
   `fuchsia.git`.
2. Checking out `fuchsia.git` into the workspace root (`.`) and cloning dozens
   of separate external and third-party Git repositories into subdirectories
   such as `//third_party/*` (and `//vendor/*` for internal checkouts).
3. Downloading prebuilt toolchains, host tools, and artifacts from CIPD into
   `//prebuilt/*` based on `jiri.lock` lockfiles.
4. Executing post-sync `<hook>` scripts defined in the manifests to generate
   build metadata and prepare the tree.

(Side note: appropriately enough, the agent that helped me write this RFC wrote
an inaccurate description of the checkout process in an earlier draft.)

Under this proposal, `jiri` is removed from standard developer workflows, and
its responsibilities for assembling a buildable checkout are distributed between
`git` and the build tool:

* **`git` fetches the source tree:** Running `git clone $FUCHSIA_URL` checks out
  the Fuchsia repository containing the build entry points and in-tree sources.
* **`git` and the build tool fetch external dependencies and prebuilts:** All
  additional inputs required for a build (including third-party source
  repositories, prebuilt toolchains such as Clang and Rust, and host tools) are
  either included via `git` (e.g., vendored in-tree or managed via standard
  `git` mechanisms) or fetched lazily on demand by the build tool (e.g., via
  Bzlmod and Bazel repository rules).
* **The build tool drives the build:** Running the build tool inside the cloned
  repository fetches whatever external repositories or toolchains are needed for
  the target being built and executes the build directly, without requiring
  pre-build `jiri` hook execution.

This RFC intentionally leaves open the exact boundary between what is managed in
`git` versus fetched by the build tool. Different categories of dependencies
(for example, small third-party crates, large prebuilt toolchains, or external
repositories) may use different mechanisms, and those choices may evolve over
time as long as the top-level `git clone` and build contract is preserved.

### Internal Checkouts

While an open-source Fuchsia checkout must work with `git clone` alone, internal
development requires proprietary sources and configurations (typically under
`//vendor/*`).

Internal checkouts may require an explicit additional step after cloning
`fuchsia.git` (or an internal superproject/script) to populate internal
repositories and wire them into the workspace before running the build tool.
Even in internal checkouts, standard developer workflows should not require
`jiri`.

## Implementation

Rather than mandating a single mechanism for all external code and prebuilts,
the implementation will draw from a few standard "tools in the toolbox"
depending on what makes the most sense for each dependency:

* **Vendoring into `fuchsia.git`:** Third-party libraries that benefit from
  living directly in the source tree can be vendored into `fuchsia.git`
  (potentially using `git subtree` or similar in-tree vendoring tools). We would
  need to build tooling for automated updates to vendored-in code.
* **Fetching during the build:** Prebuilt CIPD packages, toolchains, and
  external Git repositories can be fetched on demand as part of the build (such
  as by using Bazel's Bzlmod module extensions and/or repository rules).
  Initially, we plan to continue fetching these dependencies using `jiri`, but
  hiding that usage as an implementation detail of the build process.
* **Limited `git` submodules for ACL'd `vendor` repos:** As noted in the
  requirements, we will not use `git` submodules en masse for third-party
  dependencies. However, we will likely manage a small number of internal
  `vendor` repositories (ideally just one) as submodules to support
  repository-level access controls (ACLs).

## Performance

While implementing this RFC may well improve performance (for instance, fetching
dependencies during the build avoids downloading prebuilts and repositories that
aren't needed for the target being built), all this RFC requires is that we do
not significantly regress checkout and build performance.

## Backwards Compatibility

Removing `jiri` from standard developer workflows and transitioning the
top-level workflow to `git` is a major change to developer and infrastructure
workflows. Existing scripts and documentation that assume a `jiri`-managed
checkout will need to be updated during the transition period.

By its very nature, this change will not be seamless. We must take care to not
cause excessive churn and confusion among developers, but some amount of
disruption is unavoidable and expected.

## Security considerations

Dependencies fetched during the build process (such as prebuilt toolchains and
third-party archives) must maintain the same supply-chain security guarantees
currently enforced for `jiri` and CIPD, including cryptographic hash
verification (e.g., via lockfiles and integrity hashes) and sourcing from
trusted mirrors in infrastructure builds.

## Privacy considerations

This proposal does not change how user data is collected, stored, or processed,
and has no privacy impact.

## Testing

The primary contract of this RFC, that
`git clone $FUCHSIA_URL && cd fuchsia && ./scripts/fx set $PRODUCT.$BOARD && ./scripts/fx build`
succeeds in a clean environment, will be continuously tested in our
infrastructure.

## Documentation

Getting started guides (including `//docs/get-started/`), build documentation,
contribution guides, and agent instructions (`AGENTS.md`) will be updated to
replace `jiri` onboarding steps with `git clone`.

## Drawbacks, alternatives, and unknowns

### Drawbacks

* Moving standard workflows off `jiri` requires re-engineering how Fuchsia
  rollers, infrastructure builders, third-party dependency management, and
  internal `//vendor` checkouts operate.

### Alternatives

* **Keep `jiri` in standard workflows and teach agents/users to use it:** We can
  document `jiri` in `AGENTS.md` and onboarding guides (as we do today).
  However, every layer of custom tooling increases cognitive load, breaks
  standard tooling assumptions, and degrades agent reliability across tool
  boundaries.
* **Mandate a single dependency strategy now (e.g., pure monorepo vs. pure
  build-time fetching):** We could specify in this RFC whether third-party code
  is vendored into `fuchsia.git`, included via Git submodules, or fetched via
  the build tool. Instead, this RFC establishes the top-level user-facing
  invariant (`git clone` + build) and allows the implementation to choose the
  most appropriate mechanism for each dependency class.
* **Use `jj` primarily:** Many Fuchsia engineers have had success with `jj`
  (Jujutsu), and we could update our documentation and agent skills to be
  written in terms of `jj`. While we do not want to disrupt engineers using `jj`
  as a frontend to `git`, agents are still far less familiar with it.
  Anecdotally, we have seen agents consult `jj help` in the middle of workflows,
  which is not the case with `git`. As such, `jj` will continue to be supported
  as an opt-in workflow.
* **Submodules en masse:** At first glance, `git` submodules seem like the right
  way to model third-party dependencies, but many projects have struggled to use
  them for that purpose. Open-source projects that _do_ use submodules for
  third-party dependencies often create wrapper scripts to spare developers the
  complexity of understanding them, and [academic research on `git`
  usability](https://dl.acm.org/doi/full/10.1145/3494518) lists `git submodule`
  as among the most difficult `git` commands to learn. Fuchsia previously
  attempted to use submodules for this purpose, but abandoned the effort
  part-way through. Anecdotally, during that migration, engineers on the team
  were often confused about how their submodules got into a certain state and
  how to fix it.

### Unknowns

* What is the best ergonomic mechanism for layering internal `//vendor/*`
  sources onto an open-source `fuchsia.git` checkout?
* How should roller and global integration workflows evolve as dependencies move
  out of `jiri` manifests?

## Prior art and references

* **Standard Bazel projects:** Projects across the open-source Bazel ecosystem
  follow the `git clone <url> && cd <repo> && bazel build //...` pattern, using
  [Bazel Modules (Bzlmod)](https://bazel.build/external/module) to manage
  external dependencies and toolchains automatically.
* **Android (`repo`):** Android manages its multi-repository source tree using
  [`repo`](https://gerrit.googlesource.com/git-repo) and XML manifest files
  hosted in a separate manifest repository, which is the model that directly
  inspired Fuchsia's `jiri` and `integration.git` architecture.
* **Chromium (`gclient` / `depot_tools`):** Chromium uses a single primary
  source repository (`chromium/src.git`) paired with a `DEPS` file and custom
  [`depot_tools`](https://commondatastorage.googleapis.com/chrome-infra-docs/flat/depot_tools/docs/html/depot_tools_tutorial.html)
  (`fetch` and `gclient sync`) to pull third-party Git repositories, CIPD
  packages, and run post-sync hooks. While `chromium/src.git` consolidates
  first-party code, it still requires custom meta-tooling (`gclient`) prior to
  building.
