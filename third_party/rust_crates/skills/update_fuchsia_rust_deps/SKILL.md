---
name: update-fuchsia-rust-deps
description: >
  Comprehensive workflow for auditing and updating third-party Rust crate
  dependencies in //third_party/rust_crates using fx rust-3p-outdated and fx
  update-rustc-third-party. Covers filtering outdated crates, handling direct
  versus transitive-only and empty/ or forks/ crates, filing tracking bugs in
  Buganizer, updating gn.package, Bazel annotations, and license exceptions,
  checking for new OSRB dependencies, verifying with clippy and fx build,
  managing multi-repository graceful migrations, and preparing or rebasing
  review-friendly Gerrit CLs.
---

# Updating Fuchsia Rust Dependencies

This top-level skill describes the end-to-end workflow for upgrading third-party
Rust crate dependencies under `third_party/rust_crates/`, verifying that the
tree compiles and lints cleanly, gracefully handling out-of-tree or
multi-repository dependents, and structuring commits for Gerrit code review.

**Note**: Do **not** spawn parallel subagents to update multiple crates or
switch branches concurrently in a single Fuchsia checkout, and never run more
than one `fx update-rustc-third-party` or `fx build` at a time. Process crate
updates strictly one by one. For splitting large diffs or reviewing dependency
update CLs, see
[`split-fuchsia-rust-dep-updates`](../split_fuchsia_rust_dep_updates/SKILL.md)
and
[`review-fuchsia-rust-dep-updates`](../review_fuchsia_rust_dep_updates/SKILL.md).

---

## 1. Planning & Performing the Update

### Discover and Filter Outdated Crates (`fx rust-3p-outdated`)

1.  **Run `fx rust-3p-outdated`:**

    ```posix-terminal
    .jiri_root/bin/fx rust-3p-outdated
    ```

2.  **Cross-reference with `//third_party/rust_crates/Cargo.toml`:** Classify
    each outdated crate along two axes:

    - **Dependency type:**

      - **Direct:** Listed in `[dependencies]` or `[target.'...'.dependencies]`
        in `third_party/rust_crates/Cargo.toml`.
      - **Transitive-only:** Present in `Cargo.lock` and `vendor/`, but only
        pulled in by another third-party crate.

    - **Patch or location status (`[patch.crates-io]`):**

      - **Standard vendored (`vendor/`):** No `[patch.crates-io]` override.
      - **Empty stub (`empty/<crate>`):** Stub crate used to satisfy Cargo
        resolution for unused platform or build dependencies (such as
        `windows-*`, `cc`, `pkg-config`, `wasi`, or `portable-atomic`).
      - **Fork (`forks/<crate>`):** In-tree patched copy whose code diverges
        from upstream, usually because of Fuchsia-specific patches or pull
        requests that have not been included in an upstream release yet.
      - **Ask-to-patch (`ask2patch/<crate>`):** Crates requiring a policy
        exception to modify.

3.  **Apply standard filtering rules:**

    - **Skip recent releases (less than 14 days old):** `Cargo.toml` enforces
      `min_publish_age = 14` unless overridden for security fixes.
      * Critical security updates that might qualify can be identified by
        searching for the crate at RUSTSEC (https://rustsec.org/).
    - **Prioritize non-`empty/` and non-`forks/` crates first:** Keep the list
      of outdated `empty/` crates handy. Updating a normal crate frequently
      requires bumping the version inside a corresponding
      `empty/<crate>/Cargo.toml` stub (especially for `windows-*`, `cc`,
      `rustversion`, or `wasi`).
    - **Group coupled crates:** Update tightly coupled workspace families
      together in a single CL (for example, `pest` + `pest_derive` +
      `pest_generator` + `pest_meta`, `schemars` + `schemars_derive`, `askama` +
      `askama_derive` + `askama_macros` + `askama_parser`, or `toml`
      + `toml_parser` + `toml_writer` + `toml_datetime` + `serde_spanned`).

### Separate into Independent Stacks
Before starting a large dependency upgrade, check if the updates can be broken
into **smaller, independent stacks** that can be developed, compiled, and landed
in parallel.

For example, when updating a suite of cryptographic crates:

* **Block Cipher Stack** (`cipher`, `aes`, `chacha20`, `cbc`) is independent of
  digests.
* **Digest Stack** (`digest`, `sha2`, `hmac`, `hkdf`) depends on block ciphers
  but is independent of signatures.
* **ECC Stack** (`p256`, `elliptic-curve`, `ecdsa`) might depend on digest
  versions already in tree.

Upgrading separate stacks on distinct branches reduces risk and keeps Change
Lists (CLs) manageable.

### Per-Crate Update Workflow (One by One)

Perform the following steps sequentially for each crate, coupled crate group, or
stack.

#### Step 1: Create a clean branch from `JIRI_HEAD`

Avoid long CL chains by branching directly from `JIRI_HEAD` for each independent
update:

```posix-terminal
git checkout -b update-<crate> JIRI_HEAD
```

#### Step 2: File a tracking bug in Buganizer

File a `TASK` issue in component `1488711` (`Public Trackers > Fuchsia >
Language Platforms > Rust > Third Party`) assigned to the user (template:
`b/573552760`):

```posix-terminal
/google/bin/releases/issues-cli/issues mutate create \
  --component_id 1488711 \
  --type TASK \
  --priority P2 \
  --severity S2 \
  --status ASSIGNED \
  --assignee <user_ldap>@google.com \
  --title "Outdated rust-3p crate: <crate>" \
  --description "Update <crate> from <old_version> to <new_version>."
```

Record the returned `<bug_id>` for the commit message `Fixed: <bug_id>` footer.

#### Step 3: Bump the crate version & update metadata

- **For direct dependencies:**

  1.  Update crate version requirements or add legacy package aliases in
      `third_party/rust_crates/Cargo.toml` under `[dependencies]` and/or
      `[target.'...'.dependencies]`.
  2.  Check if `[gn.package.<crate>."<old_version>"]` exists in
      `third_party/rust_crates/Cargo.toml`:

      - Update the version key to `"<new_version>"`.
      - If `env_vars` hardcodes `CARGO_PKG_VERSION*` (such as
        `CARGO_PKG_VERSION`, `CARGO_PKG_VERSION_MINOR`, or
        `CARGO_PKG_VERSION_PATCH`) or paths with the version number, update
        those values to match `<new_version>`.
      - Check if any other `[gn.package.*]` entries list `:<crate>-v<old_ver>`
        in their `deps` or `visibility` lists and update them if needed.

  3.  Check
      `build/bazel/update-rustc-third-party/crate_annotation_overwrites.bzl` for
      any version-pinned `crate.annotation(version = "<old_version>", ...)` or
      versioned dependency paths
      (`//third_party/rust_crates/vendor/<crate>-<ver>`).
  4.  Check
      `tools/check-licenses/assets/configs/policy_exceptions/AllLicenseTextsMustBeRecognized/rust_crates.json`
      for any hardcoded path referencing
      `third_party/rust_crates/vendor/<crate>-<old_version>/...` (such as
      `hyper-rustls` or `rustls-native-certs`) and update it to `<new_version>`
      so `check_licenses` (`static-checks-internal`) passes in CQ.

- **Lock the target version in `Cargo.lock` first:**

  Whether updating a direct or transitive dependency, lock the exact target
  version in `Cargo.lock` with the prebuilt Cargo binary *before* running `fx
  update-rustc-third-party --no-min-publish-age`. This prevents `:crates_vendor`
  from jumping to an even newer release published within the last 14 days when
  `--no-min-publish-age` is passed. If multiple versions of `<crate>` exist in
  `Cargo.lock`, disambiguate with `<crate>@<old_version>`:

  ```posix-terminal
  CARGO_REGISTRY_GLOBAL_MIN_PUBLISH_AGE="14 days" \
    prebuilt/third_party/rust/linux-x64/bin/cargo update \
    --manifest-path third_party/rust_crates/Cargo.toml \
    -p <crate>@<old_version> --precise <new_version>
  ```

  Watch the `cargo update` output for any `Downgrading <other_crate> ...` lines
  (for example, an upstream point release that pins an older version of a
  transitive dependency). If an update downgrades another crate that was already
  updated in tree, abort the update, comment on the tracking bug, and reset the
  branch.

- **If an `empty/` stub crate needs updating:**

  If Cargo resolution fails because a new version requires a newer version of an
  `empty/<stub>` crate, update `version = "<new_stub_version>"` in
  `third_party/rust_crates/empty/<stub>/Cargo.toml` (or add a new stub if
  multiple major versions coexist).

#### Step 4: Regenerate vendored crates and build files

Run `fx update-rustc-third-party` (if an existing crate already on `main` in
`Cargo.toml` was published within the last 14 days, pass `--no-min-publish-age`
so `cargo-gnaw` does not reject the existing crate):

```posix-terminal
.jiri_root/bin/fx update-rustc-third-party --no-min-publish-age
```

What this script does:

1.  Preserves `OWNERS` and custom `LICENSE` files from existing `vendor/`
    directories.
2.  Runs `generate_crate_annotations.py` to sync
    `build/bazel/update-rustc-third-party/crate_annotations.bzl`.
3.  Runs Bazel `:crates_vendor` and `postprocess_crates_vendor.py` to update
    `third_party/rust_crates/Cargo.lock`, `third_party/rust_crates/vendor/`,
    `third_party/rust_crates/vendor/BUILD.bazel`, and `crates.bzl`.
4.  Propagates `OWNERS` and `LICENSE` files from the previous version directory
    to the new version directory and runs `check_rust_licenses.py`.
5.  Runs `cargo-gnaw` to regenerate `third_party/rust_crates/BUILD.gn`.

**Handling `cargo-gnaw` build script errors:** If `cargo-gnaw` fails because the
new version added or modified a `build.rs` script, inspect the new
`third_party/rust_crates/vendor/<crate>-<new_version>/build.rs` and add or
update the appropriate `[gn.package.<crate>."<new_version>"]` section in
`third_party/rust_crates/Cargo.toml` (and, if needed,
`crate_annotation_overwrites.bzl`), then re-run `fx update-rustc-third-party`.

#### Step 5: Check for new dependencies or license changes requiring OSRB

Inspect `git status --short`:

```posix-terminal
git status --short
```

- **Upgraded crate (`D` old version + `??` new version):**
  `third_party/rust_crates/vendor/<crate>-<old>/` is deleted and
  `third_party/rust_crates/vendor/<crate>-<new>/` is untracked, with its
  `OWNERS` file automatically copied from `<crate>-<old>/OWNERS`.
- **Brand-new transitive dependency (`??` new crate with no prior version):** If
  a completely new crate `<new_crate>-<ver>` appears in `vendor/` (which had no
  prior `<new_crate>-*` directory in `vendor/` and therefore has no `OWNERS`
  file in `vendor/<new_crate>-<ver>/`), or if a crate changes its license to an
  unapproved license, **stop before uploading to Gerrit**. New third-party
  crates require Open-Source Review Board (OSRB) approval and an `OWNERS` file.
  Comment on the tracking bug, reset the working tree, and skip uploading to
  Gerrit unless explicitly instructed after OSRB approval.

---

## 2. Verifying the Update

### 1. Fast Compilation & Lint Check (`fx clippy --all`)
Always run `fx clippy --all` first before running a full build. Clippy
type-checks and lints across all Rust targets significantly faster than a full
build:

```bash
./scripts/fx clippy --all
```

If compilation or lint errors occur, inspect the diagnostics and update affected
in-tree Fuchsia targets to match the new crate APIs.

### 2. Full Build (`fx build`)
After `fx clippy --all` passes cleanly, run a full build to verify non-Rust
targets, linking, and C/C++ FFI bindings:

```bash
./scripts/fx build
```

If updating a schema or codegen crate (such as `schemars`) changes generated
host outputs tested by golden tests (for example,
`//src/developer/ffx/tests/mw-goldens:ffx-golden-mw-list`), run the
corresponding host test or generator script to update the goldens.

### 3. Verifying Host-Only Targets
Some host-only components (such as `partitions_config`) may not be part of the
default target-toolchain build graph:

1.  **Build with Host Toolchain**: Explicitly specify the host toolchain label:

    ```bash
    ./scripts/fx build "//src/lib/assembly/partitions_config:partitions_config(//build/toolchain:host_x64)"
    ```

---

## 3. Multi-Repository Migration Workflow

When a dependency upgrade affects code in another repository (such as
`//vendor/google` or other `jiri`-managed sub-repositories), we can update
`fuchsia.git` and `vendor/google` at the same time using cross-repository
`Depends-On` footers in Gerrit.

### 1. Simultaneous Cross-Repository Updates (`Depends-On`)

You can update `fuchsia.git` and `vendor/google` simultaneously without needing
sequential staged releases:

1.  **Create `fuchsia.git` Commit**: Apply the crate upgrade,
    `Cargo.toml`/`Cargo.lock` changes, and in-tree `fuchsia.git` migrations.
2.  **Create `vendor/google` Commit**: Update code and `BUILD.gn` dependencies
    in `vendor/google` to match the new crate APIs.
3.  **Link Commits with `Depends-On` Footers**:
    * In the **`fuchsia.git`** commit message, add a `Depends-On` footer
      pointing to the `vendor/google` (`turquoise-internal`) Change-Id:

        ```
        Depends-On: turquoise-internal:$Commit-Id
        ```

    * In the **`vendor/google`** commit message, add a `Depends-On` footer
      pointing to the `fuchsia.git` Change-Id:

        ```
        Depends-On: fuchsia:$Commit-Id
        ```

4.  **Land via Fuchsia CQ**: Gerrit and Fuchsia CQ will test both CLs together
    in a single integration run and land them atomically across repositories.

---

### 2. Sequential Staged Rollout Workflow (Alternative)

If a simultaneous update is not possible and out-of-tree references must be
temporarily pinned:

1.  **Check Sibling Dependencies & Transitive Stacks**: First verify whether
    updating sibling dependencies together allows `fuchsia.git` targets to
    compile cleanly.
    * **Shared Foundational Crates (e.g., `der`, `spki`)**: When a target in a
      sub-repository depends on both an upgraded crate (e.g., `pkcs8 0.11.0`)
      and sibling crates (`sec1`, `pkcs1`) that share foundational types (`der`,
      `spki`), mixing versions across those crates will cause trait bound errors
      (`Decode`, `Encode`). Ensure either:
        * The sub-repository remains on the legacy versioned target
          (`//third_party/rust_crates:pkcs8-0.10.2`) until all sibling
          cryptographic crates in that component can be migrated to the new
          foundational stack together, OR
        * When migrating off the legacy pin, update all top-level
          foundational/sibling crates in `Cargo.toml` (`der`, `spki`) so the
          entire stack shares consistent types.
2.  **Define Structured GN Groups Before Pinning Sub-Repositories**: Before
    updating sub-repository build files (`//vendor/...`) to pin to a legacy
    crate version, first land a preparatory patch in `fuchsia.git` that sets up
    structured `groups` on the existing crate version under
    `[gn.package.<PackageName>.<Version>]` in
    `third_party/rust_crates/Cargo.toml`. Because defining explicit `groups` on
    an active dependency overrides `cargo-gnaw`'s default group generation,
    explicitly list the default unversioned group (`{ name = "foo" }`), the
    major-version group (`{ name = "foo-0.3" }`), and the exact patch version
    group (`{ name = "foo-0.3.2" }`):

    ```toml
    [gn.package.foo."0.3.2"]
    groups = [
      { name = "foo" },
      { name = "foo-0.3" },
      { name = "foo-0.3.2" }
    ]
    ```

    Always depend on the major-version group
    (`//third_party/rust_crates:foo-0.3` or
    `//third_party/rust_crates:pkcs8-0.10`) in sub-repositories (`//vendor/...`)
    rather than patch-specific targets (`:foo-0.3.2`). Note: Do **not** depend
    directly on internal `rustc_library` targets (`:foo-v0_3_2`).

3.  **5-CL Sequential Rollout Workflow**:
    * **Reference**: For guidance on managing these CLs, see the
      [split_fuchsia_rust_dep_updates](../split_fuchsia_rust_dep_updates/SKILL.md)
      skill.
    * **CL 1 (`fuchsia.git`) - Define Legacy GN Groups**:
        * Add the structured `groups` block above to
          `third_party/rust_crates/Cargo.toml` for the current active legacy
          version (`0.3.2`).
        * Run `fx update-rustc-third-party` to emit
          `//third_party/rust_crates:foo-0.3` in `BUILD.gn`.
        * Run `./scripts/fx clippy --all` and land CL 1 in `fuchsia.git`.
    * **CL 2 (Sub-Repository e.g., `//vendor/...`) - Pin to Legacy Major-Version
      Group**:
        * Now that `//third_party/rust_crates:foo-0.3`
          (`//third_party/rust_crates:pkcs8-0.10`) exists in tree, update the
          sub-repository's `BUILD.gn` to depend on it instead of
          `//third_party/rust_crates:foo`.
        * Run `./scripts/fx clippy --all` to verify it passes cleanly.
        * Check in the code within the sub-repository context (`git -C
          vendor/... commit`) using a commit message like:

            ```
            [rust-3p] Pin keymint to legacy pkcs8-0.10

            Pin libkmr_common to //third_party/rust_crates:pkcs8-0.10 prior to upgrading
            pkcs8 in fuchsia.git.

            Test: ./scripts/fx clippy --all
            ```

        * Land CL 2 in the sub-repository.
    * **CL 3 (`fuchsia.git`) - Upgrade Crate & Retain Legacy Alias**:
        * Update `foo = "0.4.0"` in `third_party/rust_crates/Cargo.toml`.
        * Add legacy alias `foo-0_3_2 = { package = "foo", version = "0.3.2" }`.
        * Update `[gn.package.foo."0.3.2"]` to remove `{ name = "foo" }` so the
          upgraded `foo 0.4.0` owns `//third_party/rust_crates:foo`, while
          keeping `{ name = "foo-0.3" }` and `{ name = "foo-0.3.2" }`:

            ```toml
            [gn.package.foo."0.3.2"]
            groups = [
              { name = "foo-0.3" },
              { name = "foo-0.3.2" }
            ]
            ```

        * Run `fx update-rustc-third-party` and `./scripts/fx clippy --all`
          across the tree, and commit CL 3 in `fuchsia.git`.
    * **CL 4 (Sub-Repository e.g., `//vendor/...`) - Migrate to Upgraded
      Crate**:
        * Update code in the sub-repository to use `foo 0.4.0` and switch its
          `BUILD.gn` dependency back to `//third_party/rust_crates:foo`.
        * Run `./scripts/fx clippy --all` to verify compilation and linting.
        * Check in the code within the sub-repository context (`git -C
          vendor/... commit`) using a commit message like:

            ```
            [rust-3p] Unpin keymint to use upgraded pkcs8

            Switch libkmr_common back to //third_party/rust_crates:pkcs8 now that
            all sibling dependencies have been upgraded.

            Test: ./scripts/fx clippy --all
            ```

        * Land CL 4 in the sub-repository.
    * **CL 5 (`fuchsia.git`) - Remove Legacy Crate**:
        * Remove `foo-0_3_2` and its `[gn.package.foo."0.3.2"]` block from
          `third_party/rust_crates/Cargo.toml`.
        * Run `fx update-rustc-third-party` and `./scripts/fx clippy --all`
          across the tree to confirm all targets compile cleanly without the
          legacy pin, and commit CL 5 in `fuchsia.git`.

---

## 4. Breaking Up Changes for Code Review

Dependency upgrades often introduce massive diffs due to added, removed, or
updated vendored files under `third_party/rust_crates/vendor/`. To make CLs easy
to review in Gerrit:

### Commit Message Formatting
When checking in the code, format the commit message clearly with a `[rust-3p]`
prefix and bullet points listing every added, updated, or renamed crate and its
version transition. For example:

```
[rust-3p] Update ecc crates and dependencies

This patch:

* Updates p256 from 0.11.1 to 0.13.2
* Renames p256 0.11.1 to p256-0_11
* Adds primeorder 0.13.6
* Updates rfc6979 from 0.3.1 to 0.4.0
* Updates serdect from 0.1.0 to 0.2.0
```

For single-crate updates (following `fxr/1909814`), include the `Fixed:
<bug_id>` and `Test: fx build` footers and validate with
`commit_msg_checker.py`:

```posix-terminal
./scripts/fuchsia-vendored-python scripts/shac/commit_msg_checker.py \
  --strict --message-file <scratch_file>
git commit -F <scratch_file> && rm -f <scratch_file>
```

### Splitting Commits

* **Delegate to `split-fuchsia-rust-dep-updates` Skill**: Refer to
  [`third_party/rust_crates/skills/split_fuchsia_rust_dep_updates/SKILL.md`](../split_fuchsia_rust_dep_updates/SKILL.md)
* That skill automates splitting your update commit into:
    1.  **Commit 1 (Pure copies & moves)**: Git recognizes these as 100%
        similar, making the Gerrit diff clean.
    1.  **Commit 2 (Actual updates & API migrations)**: Contains
        `Cargo.toml`/`Cargo.lock` changes, vendored code modifications, and
        in-tree API migrations. Include the detailed `[rust-3p]` commit message
        listing the version updates here.

### Uploading & Rebasing When Multiple CLs Are in Flight

1.  **Upload to Gerrit (only if no new OSRB dependency was pulled in):**

    ```posix-terminal
    git push origin HEAD:refs/for/main%l=Commit-Queue+1
    ```

2.  **Rebasing and Avoiding Concurrent Auto-Submit Breakages:**
    - Every `[rust-3p]` CL modifies `Cargo.lock`, `BUILD.gn`, `crates.bzl`, and
      `vendor/<crate>-<ver>/BUILD.bazel`. When one `[rust-3p]` CL lands on
      `main`, other open `[rust-3p]` CLs frequently become non-mergeable
      (`mergeable: false`).
    - Worse, if two `[rust-3p]` CLs where one depends on the other (for example,
      `askama` and `syn`) are in `Commit-Queue+2` / `Fuchsia-Auto-Submit+1`
      simultaneously, Gerrit's text-based `REBASE_ALWAYS` strategy can submit
      the second CL without regenerating `BUILD.bazel` and `BUILD.gn` for the
      newly landed crate, breaking `main`.
    - To rebase an open `[rust-3p]` CL cleanly after another `[rust-3p]` CL
      lands: fetch `origin/main`, reset the branch to `origin/main` (or
      `JIRI_HEAD`), re-apply the `Cargo.toml` and in-tree caller changes, re-run
      `cargo update` and `fx update-rustc-third-party`, verify with `fx build`,
      and push a new patchset preserving the original `Change-Id`.
