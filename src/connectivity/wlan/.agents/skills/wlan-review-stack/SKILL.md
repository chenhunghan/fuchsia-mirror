---
name: wlan-review-stack
description: >
  Reviews Fuchsia WLAN stack and in-tree driver code for layer boundaries,
  FullMAC and SoftMAC parity, PII redaction, wake-lease lifetimes, and 802.11
  and firmware input validation. Use when reviewing Rust, C/C++, CML, or build
  changes under src/connectivity/wlan/. Do NOT use as a standalone review
  orchestrator (use /review instead) or for FIDL API or Lacewing E2E test
  reviews.
---

# WLAN stack and in-tree driver review guidelines

Apply these rules when reviewing changes to the Fuchsia WLAN stack (`wlancfg`,
`wlanix`, `wlandevicemonitor`, `lib`), in-tree platform, vendor, and virtual
drivers (`wlanif`, `wlansoftmac`, `brcmfmac`, `iwlwifi`, `wlan-virtio-hwsim`),
`testing`, `tools`, and on-target integration tests. Defer general language and
style to other reviewers.

## Layer boundaries and FullMAC/SoftMAC parity

- Follow the layer structure in
  [src/connectivity/wlan/AGENTS.md](/src/connectivity/wlan/AGENTS.md) and
  [src/connectivity/wlan/README.md](/src/connectivity/wlan/README.md). `wlancfg`
  and `wlanix` are mutually exclusive policy layers above `wlandevicemonitor`
  and `lib/sme`; keep shared 802.11 protocol, frame, and security logic in
  `lib/` (`sme`, `mlme`, `rsn`, `common`, `ieee80211`) rather than duplicating
  it across policy components, and implement driver-agnostic behavior meant to
  work across all drivers in the policy layer or shared `lib/` crates rather
  than inside individual drivers.
- When changing `MlmeRequest`, MLME-to-SME indications (`MlmeEvent`), or shared
  driver capabilities, verify both FullMAC (`lib/mlme/fullmac`, in-tree FullMAC
  drivers like `brcmfmac`, and out-of-tree FullMAC drivers in `vendor/google`)
  and SoftMAC (`lib/mlme/rust`, `wlansoftmac`, and `iwlwifi`) handle the change
  or explicitly handle unsupported modes. Coordinate cross-repo `vendor/google`
  FullMAC driver updates in parallel commits linked with `Depends-on:` footers.

## PII redaction in logs and Inspect

- In Rust policy, device-monitor, SME, and MLME code, wrap SSIDs, BSSIDs, and
  MAC addresses in `ieee80211::{Ssid, Bssid, MacAddr}` and format via `Display`
  or `Debug` (which dispatch through the process `redactor::Redactor`) rather
  than logging raw byte arrays or slices (`to_array()`, `as_array()`,
  `as_slice()`, or `{:?}` on FIDL/RSNA structs carrying raw identifiers),
  `redactor::Unredacted`, `Ssid::to_string_not_redactable()`, or
  `BssDescription::to_non_obfuscated_string()` in production logs or Inspect.
- In C and C++ drivers, never log raw SSIDs, BSSIDs, or MAC addresses in
  production log statements.

## Power management and wake leases

- Retain `wlan_power_manager::WakeLease` (and driver activity or wake leases)
  across the full handling of wake-sensitive events, including fallible state
  transitions and error returns, until outgoing teardown frames or FIDL
  notifications (such as `DeauthenticateReq` or disconnect events) are queued.
  Use `WakeLease::duplicate_token` when forwarding a wake baton over a one-way
  FIDL call so the sender does not release the lease early.
- Pass an explicit bounded deadline (`zx::MonotonicInstant::after(...)`), never
  `zx::MonotonicInstant::INFINITE`, to synchronous FIDL calls such as
  `ActivityGovernorSynchronousProxy::acquire_wake_lease` (async
  `ActivityGovernorProxy::acquire_wake_lease` takes no deadline argument).

## 802.11 frames and untrusted firmware inputs

- Treat over-the-air 802.11 frames/IEs and firmware iovar or event buffers as
  untrusted peer input: validate struct/API version fields and buffer byte
  length (using slices or `std::span`) before reading fields, value-initialize C
  and firmware out-parameter structs (`= {}`) so uninitialized stack bytes
  cannot leak, clamp firmware values before narrowing integer casts, and ensure
  C `goto` error labels return a non-`ZX_OK` status.

## Test documentation

- In multi-step tests (such as handshakes, state transitions, recovery flows, or
  `hw-sim` scenarios), verify the test states the scenario under test and
  comments non-obvious steps or ordering requirements. Do not ask for comments
  that merely restate obvious lines in short unit tests.

## Do not flag

- Integer arithmetic without `checked_add` or `checked_sub` when an explicit
  range check immediately guards the operands, or symmetric `checked_*` bounds
  kept for clarity in frequency/channel tables.
- Standard 802.11 terminology and capability bounds: `Rsne` (the wire RSN
  Element IE) vs. `Rsna` (the negotiated RSN Association), synchronous
  `fuchsia_sync::Mutex` held across non-awaiting critical sections, or omitted
  handling for PHY bandwidths or features a driver never advertises (verify
  driver capability declarations before reporting).
