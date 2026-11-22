---
name: wlan-review-e2e
description: >
  Performs code review of Fuchsia WLAN Lacewing end-to-end tests, Mobly AP
  controllers, and Honeydew WLAN affordances for fail-loudly fidelity and RF
  determinism. Use when code-reviewing diffs or CLs touching WLAN E2E test
  suites, Mobly controllers, or Honeydew WLAN affordances. Do NOT use for
  reviewing E2E test results, logs, flakes, or CI failures (use wlan-e2e-test-
  luci-triage or wlan-e2e-console-triage), as a standalone review orchestrator
  (use /review instead), or for running or promoting WLAN E2E tests.
---

# WLAN end-to-end and Honeydew code review guidelines

Apply these rules when reviewing Lacewing WLAN end-to-end tests, WLAN Mobly base
tests and controllers (`openwrt_access_point`, `legacy_access_point`,
`attenuator`, `iperf`), and Honeydew WLAN affordances under
`src/testing/end_to_end/honeydew/honeydew/affordances/connectivity/wlan/`.

## Fail-loudly test fidelity

- **No runtime skips for missing hardware or capabilities:** Missing testbed
  fixtures, offline APs, or unreachable attenuators and switches must fail the
  test, never call `asserts.skip`, `asserts.skip_if`, or `signals.TestSkip`.
  Runtime skips report green in CI and mask broken labs; gate hardware
  requirements through GN test groups instead.
- **No `setup_class` abort signals:** Raise `signals.TestError` on `setup_class`
  failures; never call `signals.TestAbortClass`, `signals.TestAbortAll`,
  `asserts.abort_class*`, or `asserts.abort_all*` inside `setup_class`, which
  breaks test failure attribution.
- **No speculative fallbacks or suppressed errors:** WLAN E2E test environments
  are tightly controlled and deterministic. Do not introduce speculative
  fallbacks or suppress unexpected errors (including ignoring non-zero exit
  codes in chained shell commands); if anything unexpected happens during a test
  run, let it fail loudly instead of masking the problem.
- **Verify outcomes, not just command dispatch:** Core test steps must assert a
  completion result or downstream state change, not rely on a `0` exit code from
  an async or fire-and-forget CLI, shell, or RPC call. Check whether wrappers
  discard output or status, and search sibling tests and helpers
  (`tests/utils/`, `utils/`, `*_testing/` via `git grep` or `rg`) for
  non-failing commands; if an out-of-tree command's failure behavior is
  unverifiable in-tree, ask (`suggestion`, category `question`) the author to
  confirm it fails on error. Enforce this (and readiness polling below) on new
  tests even when older siblings omit them.
- **Fresh post-fault assertions:** When a test repeats a query or operation
  after a fault (such as a restart or suspend/resume), verify the assertion
  observes fresh post-fault state rather than passing on cached or stale
  pre-fault state.

## Test tiering, suite isolation, and scheduling

- Prefer crate unit tests, `drivers-only` integration tests, or `hw-sim`
  (`wlantap`) tests when those lower tiers can actually exercise the behavior
  under test (see [tests/README.md](/src/connectivity/wlan/tests/README.md),
  [tests/drivers-only/README.md](/src/connectivity/wlan/tests/drivers-only/README.md),
  and
  [testing/hw-sim/README.md](/src/connectivity/wlan/testing/hw-sim/README.md)).
  When testing behavior that requires an end-to-end environment, write an E2E
  test.
- Suites under `src/connectivity/wlan/tests/core/` drive SME directly and must
  only target core testing product bundles without an active policy component
  (`wlancfg` or `wlanix`), whose background scans and interface management race
  direct SME calls.
- Land new hardware E2E targets in `tests_for_fyi*` groups (via `test_lists.gni`
  where used) before promoting to postsubmit or presubmit.

## AP controller lifecycle and device readiness

- When a test supports both `self.openwrt_ap` and legacy `self.access_point`,
  verify both branches configure matching band, channel, bandwidth, and security
  parameters. Only legacy `self.access_point` requires explicit AP teardown;
  `openwrt_access_point` cleans up Wi-Fi, DHCP, and iPerf state in its
  controller fixture.
- Avoid defensive or preemptive cleanup scattered across individual teardown
  methods (and do not ask authors to add it): it spreads lifecycle logic across
  suites and adds maintenance burden. Keep tests self-contained by establishing
  required conditions in test setup, or in the base test class
  (`FuchsiaWlanBaseTest`) and controller fixtures when shared across tests.
- After device boot, component restart, or Wi-Fi power/radio toggles (including
  ADB-driven Starnix/Android tests invoking `cmd wifi`), wait for boot
  completion and poll device status until the expected state is observed before
  scanning or connecting, rather than relying on fixed sleeps or assuming
  asynchronous commands have completed.
- **Fault-recovery isolation:** In tests verifying recovery or state persistence
  across a fault or lifecycle transition (component restart, crash,
  suspend/resume, link loss, or reboot), assert that the expected post-recovery
  behavior succeeds before performing any manual disable/enable, reset, or
  reconfiguration step that could re-initialize the subsystem and mask a broken
  automatic recovery path (unless manual recovery is explicitly what is under
  test).

## Test documentation

- In multi-step tests (such as recovery flows, suspend/resume, or state
  transitions), verify that non-obvious pre-fault preconditions (for example, an
  action run before a restart without an assertion of its own to prime internal
  state, handles, or subscriptions) and distinct test phases have a brief inline
  comment explaining why they are there, and that milestone logs accurately
  describe what is being verified. Do not ask for comments that merely restate
  obvious lines.
