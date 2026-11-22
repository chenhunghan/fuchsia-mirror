---
name: wlan-review-simplification
description: >
  Reviews Fuchsia WLAN Rust and Python changes for crate and test helper
  reuse, layer boundaries, tighter state types, and concise tests. Use when
  reviewing WLAN Rust or Python E2E diffs for non-blocking simplifications. Do
  NOT use as a standalone review orchestrator (use /review instead), or for
  C/C++ drivers, FIDL APIs, or blocking correctness and test-fidelity rules.
---

# WLAN code simplification guidelines

Apply these rules to WLAN Rust and Python changes. They add WLAN-specific checks
on top of [code-simplification](/.agents/skills/code-simplification/SKILL.md).

## Scope and guardrails

- **Suggestions only:** Report every finding as a non-blocking suggestion that
  keeps behavior, error handling, FIDL contracts, and test coverage unchanged.
  Report nothing when no WLAN-specific simplification applies.
- **Changed code only:** Review only code the change adds or modifies. Do not
  ask for rewrites of untouched or verbatim-moved code.
- **Verify before citing:** Name the exact crate path and symbol to reuse, and
  confirm it exists, is visible to the changed target, and has compatible
  `#[cfg(target_os = "fuchsia")]` gating. Read the source directly, or search
  with `git grep` or `rg` (excluding `out/`) when a shell is available. Adding
  an in-tree WLAN crate to a GN target is fine; a new third-party crate is not.
- **Focus on simplification:** Leave correctness, safety, test-fidelity, and
  general language style to other review guidance.

## Rust crate and helper reuse

Flag new hand-rolled code when an existing crate already does the job:

- **802.11 primitives:** raw `[u8; 6]` addresses, `Vec<u8>` SSIDs, or manual MAC
  formatting instead of `ieee80211::{MacAddr, Bssid, Ssid}`.
- **Frames and IEs:** manual byte offsets, length bookkeeping, or IE loops
  instead of `wlan_common` (`buffer_reader::BufferReader`, `append::Append`,
  `ie` readers and writers, `mac`, `bss::BssDescription`), the
  `wlan_frame_writer` macros, or `wlan_bitfield` for packed fields.
- **Channels:** new frequency or bandwidth math instead of
  `wlan_common::channel` (`Channel::get_center_freq`, `derive_channel`,
  `primary_channel_from_freq`).
- **FIDL tables and replies:** repeated per-field `ok_or` or `match` ladders
  instead of `wlan_fidl_ext` (`TryUnpack`, `WithName`, `SendResultExt`, and
  `ResponderExt` where that responder already implements it).
- **Test fixtures:** hand-built `BssDescription`s, IEs, or RSNEs instead of
  `fake_bss_description!`, `fake_fidl_bss_description!`, or `FakeProtectionCfg`
  from `wlan_common::test_utils` (Fuchsia target only), or the host-safe
  `wlan_common::ie::fake_ies` and `wlan_common::ie::rsn::fake_rsnes`.

## Rust layer and state shape

- **Replaced abstractions:** When a diff removes or bypasses a call to an
  in-tree wrapper, helper, or test harness and replaces it with lower-level
  code, read the replaced implementation. If the new code duplicates
  boilerplate, filtering, or validation the replaced abstraction already owns
  just to observe or customize one step, suggest extending or parameterizing the
  in-tree abstraction instead of re-implementing it inline (do not suggest
  modifying third-party crates or stable SDK boundaries).
- **Shared logic in lib:** When new 802.11, channel, security, or credential
  logic is copied into both `wlancfg` and `wlanix`, or added to one while an
  equivalent exists in `src/connectivity/wlan/lib/`, suggest one shared helper
  in the matching `lib/` crate (see
  [AGENTS.md](/src/connectivity/wlan/AGENTS.md)).
- **Enums over coupled fields:** Suggest one enum when new code adds coupled
  `bool` flags, parallel `Option` fields, or raw integer constants whose
  combinations callers must re-check (for example, a `Credential` variant that
  holds both PSK and SAE secrets, or `PowerLevel` instead of `u8` constants).
  Keep `Option` fields that mirror FIDL tables.
- **State machines:** In crates already built on `wlan_statemachine` (`lib/sme`,
  `lib/mlme/rust`, `lib/rsn`, `lib/fcg-crypto`), suggest a state or transition
  instead of side flags tracking the same phase.
- **Single match per event:** When the same event is classified in two places
  (such as a filter step and a later `match`), suggest handling it in one
  `match`.

## Rust unit and hw-sim tests

- **Table-driven cases:** Suggest `#[test_case]` when two or more new tests
  differ only in inputs and expected output and the case list stays readable.
  Keep separate tests when setup or assertions differ in shape.
- **Shared harnesses:** Suggest `WlanixTestHelper` in `wlanix`, or `wlan_hw_sim`
  helpers (`test_utils::TestHelper`, `connect_or_timeout`,
  `save_network_and_wait_until_connected`, `Beacon`, `ProbeResponse`, and the
  `event` handler combinators) instead of new ad hoc setup or `wlantap` loops.
- **Focused tests:** Flag setup that creates interfaces, connections, or scan
  results the test never uses, new assertions on fields unrelated to the test
  name, and assertions that only echo a value a fake was told to return.

## Python E2E tests

- **Use existing layers:** Flag raw `ffx`, SSH, `iw`, or `uci` commands and
  hand-written FIDL plumbing in tests when `FuchsiaWlanBaseTest` (such as
  `wait_for_interface`), Honeydew `wlan_core`, `wlan_policy`, or
  `wlan_policy_ap`, `OpenWrtAP` methods, `wlan_test_utils`, or `vendor/google`
  `utils/adb_utils.py` (`adb_poll_until`) and `adb_wifi_utils.py` already
  provide the operation. Suites deriving from `suspend_resume.BaseTest` or
  `WoofBaseTest` cannot use `FuchsiaWlanBaseTest` helpers.
- **AP options:** Suggest `custom_uci_options` or `custom_hostapd_options` on
  `openwrt_access_point` configs instead of new one-off config fields.
- **Repeated waits:** When a new test repeats the same poll, timeout, and
  failure block, suggest one local helper that takes the expected state.
- **Base-test teardown:** Flag per-test cleanup that repeats what
  `FuchsiaWlanBaseTest` already does in `teardown_test` or `teardown_class`
  (`reset_attenuators()`, OpenWrt, AP, and iPerf log downloads).
- **Unneeded setup:** Trace every new device setting, flag, or fixture to the
  subsystem layer and assertion in that test that depends on it (for example, do
  not copy data-path or connectivity-policy settings into a test that only
  checks radio state or scanning). Do not excuse unneeded setup merely because a
  sibling test in the same file includes it -- compare across sibling tests in
  the suite or directory to confirm whether the setting is actually required for
  the new test's scope. Move setup shared by every test in a class into
  `setup_test` or `setup_class`.
- **Parameterize repeated cases:** When new `test_*` methods differ only in
  arguments, suggest Mobly `generate_tests` from `pre_run` with a readable
  `name_func` (as in `ConnectToApTest`). If `BUILD.gn` lists individual
  `test_cases` per target, suggest a shared private helper behind thin `test_*`
  methods instead.

## Do not flag

- RAII bindings held for drop timing (`let _lease = ...`, `WakeLease`,
  `InitSuspendBlocker`).
- Length and version checks on over-the-air frames, firmware buffers, or FIDL
  input, and look-alike FullMAC and SoftMAC or `wlancfg` and `wlanix` code that
  reflects different layers or semantics.
- Catching `TimeoutError` to raise `signals.TestFailure`, which keeps Mobly
  failure attribution; only the repeated block is a helper candidate.
- Removing an over-engineered or unnecessary wrapper when the replacement is
  strictly simpler and does not duplicate the wrapper's internal logic.
