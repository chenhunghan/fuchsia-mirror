<!-- Copyright 2026 The Fuchsia Authors. All rights reserved.
Use of this source code is governed by a BSD-style license that can be
found in the LICENSE file. -->

# driver-lab host tooling specification

This is the normative specification for the driver-lab host tooling at
//tools/driver-lab. The companion proxy driver specification lives at
//src/devices/driver-lab/SPEC.md.

Phase 1 -- the unclaimed-node proxy workflow, which requires no Driver Manager
changes -- is delivered first. Phase 2 -- in-situ driver debugging via an
embedded library -- extends host tooling to active, bound drivers without
managed takeover or Driver Manager changes, requiring no major contract change.
Phase 3 -- software state, runtime knobs, and adaptive bug verification --
extends host plans, the Python API, and agent skills to drive single-build
software state and concurrency experiments over `StateBank` resources.

## Phase 1: unclaimed-node proxy workflow

### Implementation status

Changeset numbers (CS) are global across this specification and
`//src/devices/driver-lab/SPEC.md`, whose changesets interleave with
these; gaps in the numbering below are proxy driver changesets. Every
changeset marks itself complete in this list when it lands, so at any
commit this list shows which changes precede and which follow that
commit.

- [x] CS2 `[driver-lab] Host tooling specification` -- this
      document.
- [x] CS4 `[driver-lab] Transport-neutral host tooling core` --
      frozen data models (18), grant resolution with the TOML store
      (10.3-10.5), and plan validation, canonicalization, and digests
      carrying the reserved takeover keys (11).
- [x] CS5 `[driver-lab] Evidence recording + proxy transport
      abstraction` -- hashed manifest-last evidence bundles (16) and
      the wire-contract-shaped transport seam with the fake proxy
      target (12, 20.3).
- [x] CS6 `[driver-lab] Consent resolution and the plan-run API` --
      interactive consent with exact-rule persistence and fail-closed
      unattended behavior (10.2), and `DriverLab.run_plan` composing
      prepare/execute/finalize with mode and guarantee resolution
      before any connection (6.2, 7.3, 14).
- [x] CS7 `[driver-lab] fuchsia-controller FIDL transport adapter`
      -- `ProxyTransport` over the generated bindings, exercised
      through real FIDL encoding on the build host (12, 20.3).
- [x] CS8 `[driver-lab] Add host CLI tools` -- `driver-lab.pyz`
      with run, permissions list/explain/add/revoke, plan
      digest/validate, and spec exit categories (10.5, 12, 13).
- [x] CS10 `[driver-lab] Add live-target conformance runbook` --
      ephemeral registration and test-node activation exercising the
      real driver end to end (14, 20.4), verified on the emulator.
- [x] CS11 `[driver-lab] Host node discovery via fuchsia.driver.development`
      -- typed node enumeration, bound driver inspection, unclaimed-node
      detection over FIDL, and expected_unclaimed plan verification (7.1,
      8.3, 14.2, 15; milestones H0, H1).
- [x] CS12 `[driver-lab] Direct mode client and published-protocol workflow`
      -- direct-mode capability validation, typed protocol discovery,
      protocol transactions, and target audit exclusion (7.1, 8.2, 15;
      milestone H1).
- [x] CS13 `[driver-lab] Target ceiling policy manifests and verification`
      -- immutable target ceiling manifests, canonical JSON and deterministic
      SHA-256 digests, non-widening narrowing validation, and one-shot vs poll
      enforcement (9.2, 9.6, 22, 23.1; milestone P1 remainder).

Remaining phase 1 work:

- [x] CS14 `[driver-lab] Bounded sequences and mutation in proxy driver`
      (11.4, 11.5, 12.2-12.4, 13; milestone P3).
- [x] CS15 `[driver-lab] Host tooling support for mutation and sequences`
      (11, 14; milestone P3 / H4).
- [x] CS16 `[driver-lab] Driver-shaped public Python API` -- `connect`/`attach`,
      `HardwareSession`, `MmioRegion`, and representative protocol resources
      (6.1, 9; milestone H4).
- [x] CS17 `[driver-lab] Stop-path hardening and cancellation` (19; milestone P2).
- [x] CS18 `[driver-lab] Host CLI subtool expansion and verified teardown`
      (8.3, 12, 14.2; milestone H3 remainder).
- [x] CS19 `[driver-lab] Protocol resources, typed endpoints, and heterogeneous sequence`
      (8.1, 8.4, 9.5, 11.5, 11.6, 12, 14.2; milestone P4).
- [ ] Engineering assembly inclusion and production-absence verification
      (7.1, 25).
- [x] CS20 `[driver-lab] Interrupt observation and local acknowledgement`
      (11.7, 17; milestone P5).
- [x] CS21 `[driver-lab] Structured configuration and per-class rate limits`
      (in //src/devices/driver-lab/SPEC.md).
- [x] CS22 `[driver-lab] Serial capture and independent recovery integration`
      (3, 14.1; milestone H5).
- [ ] Register-metadata-backed consent expansion (10.2; open decision 8).

Phase: 1 -- new-driver development. Covers direct mode and proxy access on
unclaimed nodes. In-situ debugging of existing drivers via an embedded library
is specified in the Phase 2 section of this document and in
//src/devices/driver-lab/SPEC.md, and requires no changes to this contract.

Scope: host Python API, `ffx` integration, operator authorization, orchestration,
and evidence

Companion specifications: the proxy driver specification at
//src/devices/driver-lab/SPEC.md, and the Phase 2 section of this document

### 1. Purpose

The host tooling provides a single, driver-shaped Python programming model for
interactive hardware exploration and agent-assisted Fuchsia driver
development. The same entry point supports two target access modes:

1. direct use of protocols already published by a running driver; and
2. private-resource access through an engineering-only proxy driver.

In phase 1 the proxy binds directly to an otherwise unclaimed development
node. In-situ debugging of existing, active drivers through an embedded
library is specified in phase 2; the wire and transport contracts here
support both deployment paths without breaking changes.

The tooling also exposes an `ffx` interface, coordinates out-of-band recovery,
enforces operator-consent policy, freezes executable plans, and preserves the
raw evidence needed to reproduce and audit an experiment.

This specification deliberately does not define how the proxy maps MMIO,
handles interrupts, or enforces its target-side policy. Those requirements are
owned by the proxy driver specification at //src/devices/driver-lab/SPEC.md.

### 2. Normative language

The terms **must**, **must not**, **should**, and **may** describe requirements,
recommendations, and optional behavior respectively.

The following terms have specific meanings:

- **Direct mode:** The host connects to a FIDL protocol already published by
  the running driver. The driver remains bound and active.
- **Proxy mode:** The host connects to the isolated proxy driver for private,
  policy-controlled target resources. In phase 1 the node is an unclaimed
  development node.
- **In-situ debugging:** Connecting directly to an active driver's embedded
  driver-lab endpoint for live hardware exploration without unbinding or
  quiescing device state. Defined in the Phase 2 section of this document and
  in //src/devices/driver-lab/SPEC.md.
- **Target ceiling:** Immutable target-side limits on resources and operations.
  Host approval can narrow this ceiling but cannot widen it.
- **Read grant:** Operator authorization for a particular class of register
  reads.
- **Plan:** Canonical, bounded experiment intent whose digest changes whenever
  any executable field changes.
- **Evidence:** Raw requests, responses, target identity, audit records,
  transport events, recovery events, and serial output saved before
  interpretation.

### 3. Goals

The host tooling must:

1. Reach a target control plane served by Driver Manager in the bootstrap
   realm, with no dependency on a component in the `core` realm for the
   target-side endpoint.
2. Be reachable through supported `ffx` target transports without precluding
   a serial-capable transport; serial support itself is separate
   `ffx`-team-owned work and does not gate phase 1.
3. Present one typed asynchronous Python API across direct and proxy modes.
4. Preserve the production-driver abstractions used for MMIO, GPIO, SPI, I2C,
   serial, clock, reset, and interrupt access.
5. Discover target nodes, bound drivers, and published protocols through the
   existing `fuchsia.driver.development` surface rather than a parallel
   discovery protocol.
6. Make backend capabilities and safety guarantees visible to callers.
7. Never silently replace a policy-controlled operation with a weaker direct
   operation.
8. Require operator authorization for previously unapproved register reads.
9. Persist an exact read grant when the operator chooses "always allow."
10. Fail closed in unattended operation when a required grant is absent.
11. Require explicit approval for mutating plans.
12. Remain forward-compatible with phase 2 managed takeover: plan, session,
    and evidence schemas reserve the takeover fields so phase 2 requires no
    major contract change.
13. Produce complete, hashed evidence for every attempted hardware run.
14. Integrate with Driver Agent without combining proposal, approval,
    execution, and interpretation into one opaque step.
15. Keep serial capture, reset, and power recovery independent of the in-band
    experiment transport.

### 4. Non-goals

The host tooling must not:

- expose arbitrary physical addresses;
- claim that devfs or a published service reveals a driver's private MMIO,
  interrupt handles, or incoming namespace;
- infer that every offset in an MMIO region is safe to read;
- interpret a persistent host grant as a target security capability;
- silently authorize an offset because a nearby offset was approved;
- silently downgrade a requested access mode or guarantee;
- automatically retry a mutating plan after a disconnect, reboot, or uncertain
  partial execution;
- promise that stopping a production driver preserves its live hardware state;
- execute arbitrary Python or shell expressions from a plan;
- coordinate multiple device nodes in one plan or session (single-node only
  in V1);
- guarantee sub-millisecond target-local timing precision;
- provide a universal translation from every Python convenience to Rust or
  C++; or
- require the embedded proxy library discussed in earlier designs.

### 5. Architecture

```text
Driver Agent / Python script / human CLI
                    |
                    v
Driver-shaped Python API and plan/evidence layer
                    |
                    v
FFX transport adapter
  network / USB / serial-capable target transport
                    |
                    v
Driver Manager control plane
  discovery: fuchsia.driver.development
  (takeover protocol arrives in phase 2)
          |                           |
          | direct                    | proxy
          v                           v
Published driver protocol       Isolated proxy driver
running driver remains bound    bound to an unclaimed node

Paniolo or equivalent --- serial / liveness / reset / power recovery
```

The Driver Manager integration is a control plane. Hardware operations run in
the production driver in direct mode or in the isolated proxy driver in proxy
mode. Driver Manager must not become a generic MMIO executor.

#### 5.1 Why there is no core broker

The target endpoint must remain available on engineering systems whose normal
component topology or networking is unavailable. Discovery and channel routing
therefore belong at the Driver Framework/Driver Manager layer rather than at a
stable moniker under `core`. This placement was suggested by driver-framework
engineering during early design review.

Two requirements are distinct here. The target-side control plane must not
live under `core`; serving it from Driver Manager in the bootstrap realm
satisfies that. End-to-end operation without `core` additionally requires a
transport that terminates outside `core`; no such `ffx` transport exists
today, so this design must not preclude one, and serial transport is tracked
as separate work with the `ffx` team. The public Python API must not expose
the transport choice.

#### 5.2 Standalone proxy vs embedded library

The standalone proxy (`lab_proxy`) is the Phase 1 mechanism for private hardware
access on unclaimed nodes. For nodes with an active, bound driver, Phase 2
introduces the embedded library (`driver_lab_rust` / `driver_lab_cpp`) for
in-situ debugging. Rather than unbinding the driver, the embedded library preserves
live hardware state and solves clock/power gating by running inside the active
driver component, using cooperative quiesce locks and interrupt event taps to
prevent concurrent interference.

### 6. Design invariants

#### 6.1 Programming-model fidelity

The public Python API must preserve, as closely as practical:

- resource types;
- production FIDL method boundaries;
- MMIO access width and ordering;
- asynchronous waits;
- error propagation;
- resource acquisition and lifetime;
- explicit resets and state transitions; and
- the distinction between semantic device protocols and raw registers.

Discovery, transport, consent, target policy, batching, and evidence may be
implemented underneath or alongside this API. They must not flatten every
hardware resource into an unrelated generic remote-control interface.

For protocol-backed resources, use generated Python FIDL types directly or a
thin shape-preserving adapter. For MMIO, expose a remote `MmioRegion` object
whose scalar operations resemble the MMIO operations used by an on-device
driver. Target-local batches are an explicit timing and safety facility, not
the only programming model.

#### 6.2 Explicit capability degradation

The host must report:

- selected access mode;
- resource-level capabilities;
- target-policy enforcement;
- target audit availability;
- target-local timing availability;
- fault-isolation level;
- whether a production driver remains active; and
- whether restoration will be required (always false until phase 2).

Unsupported operations fail before hardware access. A direct-mode connection
must not pretend that private MMIO, private interrupts, target audit, or
target-local batch timing is available.

#### 6.3 Evidence before interpretation

Raw evidence is finalized before an agent may label an observation verified,
contradicted, or inconclusive. Interpretation is a derived artifact and must
refer back to exact operation results.

#### 6.4 Independent recovery

The experiment channel is not the recovery channel. Serial capture, target
liveness, reset, and power control remain usable after loss of the FIDL or
`ffx` path.

### 7. Access modes

#### 7.1 Direct published-protocol mode

Direct mode connects to a protocol already published by the active driver.
Discovery may use an aggregated service, devfs compatibility path, or another
Driver Manager-provided route, but those details are hidden below the public
API.

Direct mode is appropriate for:

- black-box driver testing;
- normal device configuration;
- querying state exposed by the driver;
- standard GPIO, SPI, I2C, serial, clock, or reset protocols;
- an existing `fuchsia.hardware.registers` endpoint; and
- low-risk ad-hoc exploration that does not require private resources.

Direct mode does not provide private-register reflection. It provides only
what the running driver publishes.

#### 7.2 Proxy mode

Proxy mode is appropriate when the caller needs:

- MMIO owned privately by the production driver;
- target-local polling or timing;
- proxy target policy and audit;
- proxy interrupt observation;
- exclusive experimental ownership; or
- an initialization sequence that precedes the production driver.

In phase 1 the node must be unclaimed: the proxy binds through engineering-only
node metadata and existing registration and bind mechanisms. If a normal
driver is bound, the node requires managed takeover, which is phase 2 and is
reported as unsupported. Either way the proxy backend must be compatible with
the resources offered by the parent.

#### 7.3 Selection rules

The caller may request `direct`, `proxy`, or `auto`. A proxy request may also
require a specific activation policy: `bind-unclaimed` (phase 1) or `takeover`
(reserved; rejected as unsupported until phase 2).

`auto` may select direct mode only when every plan requirement is satisfied by
direct mode. It must never:

- unbind a driver (takeover authorization does not exist until phase 2);
- fall back from a required target policy to host-only validation;
- fall back from target-local timing to host round trips;
- convert private-resource intent into a semantically different public
  protocol call; or
- downgrade a mutating plan.

The selected mode and its guarantees are included in the canonical plan and
evidence.

### 8. Target control-plane contract

Discovery reuses the existing `fuchsia.driver.development` and
`fuchsia.driver.registrar` protocols; this specification adds no parallel
discovery surface and phase 1 requires no Driver Manager changes. The
engineering-only takeover protocol is defined in the Phase 2 section of this
document and in //src/devices/driver-lab/SPEC.md.
The exact FIDL syntax is revision-dependent, but the semantics below are
normative.

#### 8.1 Node discovery

Discovery uses `fuchsia.driver.development` (`GetNodeInfo`, `GetDriverInfo`,
and related methods). Work package 1 produces a gap matrix mapping each
required field to that surface in the selected revision. Discovery must
provide:

- stable node identity criteria (moniker and node properties, not per-boot
  numeric IDs);
- bound driver URL and driver-host koid, if any; and
- published protocol/service descriptors, composed from devfs or
  component-framework queries where `fuchsia.driver.development` does not
  report them directly.

Boot identity, proxy generation, resource descriptions, and their digests are
reported by the bound proxy's `Describe`, not by Driver Manager. Takeover
eligibility, active proxy-access state, and pending restoration failure are
reported by the phase 2 takeover protocol, not by node discovery.

Physical addresses and raw handles are never returned.

#### 8.2 Direct connection

Direct mode connects only to a protocol that discovery reported. Phase 1 uses
existing devfs or aggregated-service routing; the host tooling validates the
typed protocol/service selector itself and never accepts an arbitrary path or
component moniker from plan JSON. A typed Driver Manager route may be added
later if selector validation proves insufficient.

#### 8.3 Proxy activation on unclaimed nodes

Phase 1 activates the proxy only on nodes with no bound driver:

- the proxy is loaded through engineering assembly inclusion or ephemeral
  registration (`fuchsia.driver.registrar`);
- eligibility comes from explicit engineering-only node metadata or a
  developer-created test node, never from broad bind rules;
- before connecting, the host verifies the node identity criteria and that
  the bound driver is the expected proxy at the expected generation;
- the host connects to the proxy's published service through existing devfs
  or aggregated-service routing, then correlates the endpoint back to stable
  node identity; and
- ending access stops or unbinds the proxy and verifies the node is again
  unclaimed.

The caller cannot select an arbitrary driver package. The managed-takeover
activation path (`BeginProxyAccess`, `EndProxyAccess`, exclusive leases, and
restoration) is specified in phase 2; phase 1 plans, sessions, and evidence
reserve the fields those flows require.

### 9. Public Python API

The illustrative API below fixes responsibilities, not final spelling:

```python
class DriverLab:
    @classmethod
    async def connect(
        cls,
        target: str,
        *,
        transport: str = "auto",
        timeout_s: float = 10.0,
    ) -> "DriverLab": ...

    async def list_nodes(self) -> list[NodeSummary]: ...
    async def describe_node(self, node_id: str) -> NodeDescription: ...

    async def attach(
        self,
        node_id: str,
        *,
        mode: Literal["auto", "direct", "proxy"],
        requirements: AccessRequirements,
        context: RunContext,
    ) -> "HardwareSession": ...

class HardwareSession:
    @property
    def capabilities(self) -> SessionCapabilities: ...

    async def mmio(self, resource: str) -> "MmioRegion": ...
    async def gpio(self, resource: str) -> "Gpio": ...
    async def spi(self, resource: str) -> "Spi": ...
    async def i2c(self, resource: str) -> "I2c": ...
    async def serial(self, resource: str) -> "Serial": ...
    async def clock(self, resource: str) -> "Clock": ...
    async def reset(self, resource: str) -> "Reset": ...
    async def interrupt(self, resource: str) -> "Interrupt": ...
    async def sequence(self, operations: list[Operation]) -> ExecutionReport: ...

class MmioRegion:
    async def read32(self, offset: int) -> int: ...
    async def write32(
        self,
        offset: int,
        value: int,
        *,
        mask: int,
        expected_before: int | None = None,
        expected_mask: int | None = None,
        require_readback: bool = True,
    ) -> WriteResult: ...

    async def poll32(
        self,
        offset: int,
        *,
        expected: int,
        mask: int,
        interval_s: float,
        timeout_s: float,
    ) -> PollResult: ...
```

`sequence` is available only when the selected backend can preserve the
requested ordering and timing. Resource-specific methods remain the canonical
programming model.

#### 9.1 Translation metadata

Each public hardware method should document:

- its closest production C++ and Rust analogue;
- whether it is directly translatable;
- differences caused by remote execution;
- whether it depends on target-local timing; and
- whether it is an experiment-only convenience.

A future translation checker may reject or flag scripts that use
experiment-only facilities. Automatic translation is not required in the
initial implementation.

#### 9.2 Async behavior

Use one asyncio event loop per process. All waits have host deadlines. Target
deadlines must expire slightly before host deadlines so a normal target timeout
can be distinguished from transport loss.

Do not call blocking sleep from asynchronous code. Hold strong references to
serial, health, evidence, and execution tasks until each has completed or been
explicitly canceled and joined.

### 10. Operator authorization

#### 10.1 Two independent layers

Authorization has two layers:

1. The target ceiling determines what the target can ever access.
2. Host consent determines what the current operator or agent may request
   within that ceiling.

A host grant never widens the target ceiling. A target accepting a
session-specific allowlist does not prove who approved it; approval provenance
is evidence, not cryptographic authorization.

#### 10.2 Default read behavior

An MMIO read that matches neither an active plan approval nor a persistent
grant must pause for operator consent. In unattended operation it fails closed.
For an unknown register, the active plan approval must itself contain an
explicit human read decision; classifying a plan as read-only is not sufficient
authorization.

The interactive choices are:

- **Allow once:** authorize the exact access rule for the current canonical
  plan or interactive session.
- **Always allow:** persist the exact read rule.
- **Deny once:** reject the request without changing persistent policy.
- **Always deny:** persist a matching denial.

The UI must warn that reads may clear status, consume FIFO data, acknowledge
events, or fault when hardware dependencies are disabled.

"Read all registers" is not a primitive permission:

- when register metadata exists, the host expands the request into the exact
  declared readable offsets and resolves consent for each rule;
- when only an MMIO byte range is known, the host must describe the action as a
  raw range sweep rather than pretending every aligned word is a register;
- a raw range sweep requires an explicit human-approved plan enumerating every
  offset before execution; and
- the initial implementation must not persist an "always allow this entire
  MMIO region" grant.

The UI may approve an exact displayed set in one interaction, but the stored
and target-enforced result remains a set of exact access rules.

Consent gates register-class access regardless of transport. A direct-mode
read through a published raw-register endpoint such as
`fuchsia.hardware.registers` resolves against the same plan approvals and
persistent grants as a proxy MMIO read. Semantic protocol operations (GPIO,
SPI, I2C, serial, clock, reset) are governed by plan approval and target
policy, not per-offset read consent.

Register metadata is a first-class deliverable, not an ambient assumption:
its schema, provenance, and review process must be specified before consent
expansion depends on it (see open decisions).

In V1 the proxy session allowlist is fixed when the session opens. Consent
granted mid-session takes effect by closing the session and reopening it with
the expanded allowlist; a narrowing-only `ExtendAllowlist` operation is a
deferred open decision.

Permission-resolution evidence records the source of every decision --
persistent rule, allow-once, always-allow, deny-once, always-deny, or
fail-closed -- so a run's authorization basis is auditable. A persistent
deny short-circuits prompting for the rest of the plan.

#### 10.3 Grant identity

A persistent read grant must include:

- grant schema version;
- stable target/product scope;
- stable node identity criteria;
- resource-description digest;
- logical resource name or ID;
- byte offset;
- width;
- access class;
- optional poll limits;
- decision;
- approval timestamp;
- approval source or identity when available; and
- optional rationale/reference.

It must not match solely by nodename, boot ID, devfs path, or dynamic service
instance.

The resource-description digest covers the resource kind, logical size,
provider identity, and any stable metadata needed to prevent an old grant from
matching a changed mapping. A boot change alone need not invalidate a
persistent read grant; a resource-description change does.

#### 10.4 Access classes

At minimum distinguish:

- one-shot read;
- bounded snapshot;
- bounded poll;
- write;
- protocol transaction; and
- interrupt wait.

A one-shot read grant does not implicitly authorize polling. Poll authorization
includes maximum interval frequency and timeout. All target-local execution
remains subject to proxy limits.

#### 10.5 Persistent configuration

The persistent store is a checked, human-readable TOML file. For example:

```toml
schema_version = 1

[[read_grants]]
target_scope = "example-engineering-target"
node_id = "example-device"
resource_digest = "sha256:..."
resource = "control"
offset = 0x3c
width = 32
access = "read_once"
decision = "allow"
approved_at = "2026-07-29T23:10:00Z"
reason = "Reviewed against the device register specification"
```

Interactive persistence must:

1. lock against concurrent writers;
2. parse and validate the existing file;
3. reject ambiguous or overlapping entries;
4. write a temporary file;
5. flush and atomically replace the original;
6. preserve a revision/audit record; and
7. print the exact rule that was added.

The CLI provides list, explain, add, and revoke operations. Manual config edits
are allowed but undergo the same validation on load.

#### 10.6 Writes

Persistent read permission never authorizes a write. A mutating plan requires:

- an explicit target write policy;
- exact value and mask;
- target/device expectations;
- a precondition when semantics permit;
- readback semantics;
- bounded cleanup or recovery;
- canonical plan approval; and
- active recovery monitoring.

The first implementation should not offer "always allow arbitrary write."

### 11. Probe plans

Plans are data, not executable code. A representative plan is:

```json
{
  "schema_version": 1,
  "run_id": "2026-07-29T23-10-00Z-status-read",
  "case_id": "device-status-observation",
  "target": {
    "selector": "lab-target",
    "expected_boot_id": "..."
  },
  "node": {
    "id": "example-device",
    "expected_unclaimed": true,
    "expected_resource_digest": "..."
  },
  "access": {
    "mode": "proxy",
    "activation": "bind-unclaimed",
    "requires_target_policy": true,
    "requires_target_audit": true,
    "requires_target_local_timing": false
  },
  "operations": [
    {
      "kind": "mmio_read32",
      "resource": "control",
      "offset": "0x3c"
    }
  ]
}
```

The plan schema reserves the managed-takeover fields (expected bound driver,
expected topology generation, expected proxy identity, restoration policy)
for phase 2. For mutation the canonical plan records target policy digest,
proxy generation, exact operation fields, and recovery intent.

Plans must not support:

- Python expressions;
- shell interpolation;
- arbitrary driver URLs;
- arbitrary component monikers or protocol paths;
- includes outside approved workspace roots; or
- unvalidated plugins.

Canonicalization normalizes numeric representations, key order, and operation
order before hashing.

### 12. FFX tool and transport adapter

The supported command surface should be available as an `ffx` subtool so that
target discovery and transport selection remain in the Fuchsia tooling layer.
Proposed commands:

```text
ffx driver-lab list
ffx driver-lab describe --node <id>
ffx driver-lab direct --node <id> --protocol <typed-selector>
ffx driver-lab bind-proxy --node <id>
ffx driver-lab end-proxy --node <id>
ffx driver-lab run --plan <json> --evidence-dir <dir>
ffx driver-lab permissions list
ffx driver-lab permissions explain --plan <json>
ffx driver-lab permissions revoke --grant <id>
```

`ffx driver-lab takeover` arrives in phase 2.

The Python package links fuchsia-controller in process and uses the
generated `fuchsia.driver.lab` bindings, behind a transport-neutral
`ProxyTransport` interface so tests run against a wire-contract-faithful
fake and future transports slot in without public API changes. Transport
implementation must not leak into hardware scripts.

Until the `ffx` subtool exists, the same operations ship as a standalone
`driver-lab.pyz` host tool with JSON on stdout, diagnostics on stderr, and
the exit categories below.

A serial-capable path, once the separately tracked transport work lands, must
be tested without the normal `core` topology before it is considered
supported. It does not gate phase 1.

### 13. CLI behavior

Structured results go to stdout and diagnostics/prompts go to stderr or the
dedicated interactive UI channel.

Recommended exit categories:

```text
0  success
2  local argument, plan, or permission error
3  stale target/node/resource expectation
4  operation failure or partial execution
5  transport/channel failure
6  target reboot or liveness failure
7  evidence persistence failure
8  proxy activation failure
9  restoration failure (reserved; used by phase 2)
10 unsupported capability or unsafe downgrade
```

Ctrl-C cancels bounded execution, finalizes available evidence, and ends
proxy access when one is active. A second interrupt may request out-of-band
escalation but must not abandon proxy-access state silently.

### 14. Proxy-mode workflow

The managed-takeover workflow is specified in phase 2. The phase 1
unclaimed-node workflow is:

#### 14.1 Prepare

1. Discover the node and verify it is unclaimed.
2. Capture boot, node, and resource identities.
3. Confirm a compatible proxy is available.
4. Resolve plan requirements.
5. Resolve persistent read grants.
6. Prompt for missing read consent.
7. Require explicit mutation approval when applicable.
8. Freeze and hash the canonical plan.
9. Verify independent serial/recovery availability.

Preparation performs no hardware operations.

#### 14.2 Execute

1. Create a non-reused evidence directory.
2. Start serial and liveness capture.
3. Activate the proxy on the unclaimed node with frozen expectations.
4. Describe the bound proxy and verify its policy/resource digest.
5. Open a session with an exact session allowlist.
6. Execute the bounded plan, draining proxy audit after every mutating
   operation so the host record never runs more than one write ahead of
   drained target audit.
7. Drain remaining proxy audit.
8. Close the proxy session.
9. End proxy access and verify the node is again unclaimed.
10. Stop capture and finalize evidence.

#### 14.3 Failure behavior

- A stale expectation fails before activation.
- Channel loss never causes automatic replay.
- If a write may have completed, the result remains partial or unknown.
- A reboot invalidates all boot-scoped expectations.
- Recovery creates a new run context; it does not resume the old mutation.

### 15. Direct-mode workflow

1. Discover a typed published protocol.
2. Record the running driver identity and generation.
3. Confirm the plan requires no private resources or takeover guarantees.
4. Resolve any applicable host consent.
5. Connect through Driver Manager's typed route.
6. Execute production FIDL methods through generated or shape-preserving
   Python bindings.
7. Save requests, responses, timing, and transport events.

Target proxy policy and audit are absent unless the published protocol itself
provides equivalent guarantees.

### 16. Evidence

Each run gets a new directory:

```text
evidence/<run_id>/
    manifest.json
    plan.requested.json
    plan.canonical.json
    approval.json
    permission-resolution.json
    target.description.json
    node.before.json
    access.capabilities.json
    proxy-access.jsonl
    execution.response.json
    operations.jsonl
    target-audit.jsonl
    node.after.json
    restoration.json
    serial.log
    host-events.jsonl
    interpretation.json
```

Files that do not apply to a run are marked not applicable in the manifest
rather than fabricated. `restoration.json` and the takeover entries of
`proxy-access.jsonl` are reserved for phase 2 and marked not applicable in
phase 1 runs.

`manifest.json` contains:

- evidence schema version;
- run and case IDs;
- canonical plan digest;
- selected access mode and guarantees;
- target boot/build/product identity;
- node, driver, resource, proxy, and policy identities;
- resolved grant IDs and approval source;
- start and end wall-clock timestamps;
- target monotonic range when available;
- proxy-access outcome (takeover/restoration fields reserved for phase 2);
- serial capture metadata;
- terminal exit category;
- audit-gap status; and
- hash and byte length of every evidence file.

Write each artifact to a unique temporary file, flush, close, atomically rename,
and hash the final bytes. Write the manifest last. A run whose evidence cannot
be finalized is not successful.

### 17. Driver Agent integration

The workflow remains:

```text
proposed
    -> permissions-resolved
    -> approved
    -> running
    -> observed
    -> interpreted
    -> verified | contradicted | inconclusive
```

Driver Agent:

1. proposes the smallest plan needed to resolve a claim;
2. requests missing consent rather than broadening the plan;
3. freezes approved intent;
4. invokes the host API;
5. watches execution and recovery;
6. preserves raw evidence;
7. interprets only finalized evidence; and
8. never converts a transport success into semantic verification.

Agent interpretation cites the operation, resource, offset or protocol method,
raw values, target timestamp, driver/proxy generation, and relevant source.

### 18. Host data models

Use frozen dataclasses or equivalent validated models for:

- node summaries and descriptions;
- access requirements and capabilities;
- target and resource identity;
- run context;
- read grants and permission decisions;
- plans and approvals;
- operation requests/results;
- proxy-access transitions (takeover/restoration variants reserved for
  phase 2);
- audit pages; and
- evidence manifests.

Conversion among JSON, internal models, generated FIDL, and subprocess messages
occurs only in explicit functions. Do not pass unvalidated dictionaries through
the system.

### 19. Security and trust boundaries

The host consent file protects the workflow from accidental or unapproved
agent behavior. It is not a target security boundary: a malicious host with
engineering access may bypass it.

The target ceiling must remain safe enough for the intended engineering
environment even if host approval is bypassed. The plan digest proves stable
intent within the workflow; it is not bearer authorization.

Permission files:

- contain no secrets;
- use least-specific filesystem permissions appropriate to the environment;
- are never read from unapproved workspace paths;
- have explicit schema and revision history;
- reject duplicate or ambiguous matches; and
- are included by digest in evidence.

### 20. Testing

#### 20.1 Permission tests

Test:

- exact grant match;
- offset, width, resource, node, and digest mismatch;
- boot change with stable resource description;
- resource-description change;
- one-shot grant rejected for poll;
- poll limit enforcement;
- persistent deny precedence;
- unattended fail-closed behavior;
- concurrent config writers;
- malformed/ambiguous config;
- atomic persistence; and
- revoke/explain behavior.

#### 20.2 Plan and evidence tests

Test:

- deterministic canonicalization and hashing;
- every executable change invalidates approval;
- selected access mode is frozen;
- unsupported guarantee fails before connection;
- arbitrary path/moniker/driver injection is rejected;
- partial execution classification;
- atomic artifact writes;
- manifest hashes;
- Ctrl-C finalization;
- disk-full failure; and
- no evidence-directory reuse.

#### 20.3 Transport tests

Use fake and physical transports to test:

- normal network connection;
- supported USB connection;
- serial-capable connection without `core` (applicable once the serial
  transport lands);
- target discovery;
- target reboot;
- peer closure;
- transport migration without public API change; and
- identical structured output across transports.

#### 20.4 Proxy-activation tests

Test:

- unclaimed-node verification before bind;
- a node with a bound driver is rejected (takeover is phase 2);
- incompatible proxy;
- proxy bind failure;
- host disconnect during activation and execution;
- execution failure;
- proxy stop failure; and
- the node is verified unclaimed after end of access.

Managed-takeover state-transition tests are specified in phase 2.

#### 20.5 Direct-mode tests

Test:

- typed protocol discovery;
- direct production FIDL call;
- no private MMIO capability;
- no target-audit claim;
- direct-mode evidence;
- refusal of a takeover-only plan; and
- no fallback after direct connection failure.

#### 20.6 Driver Agent tests

Test:

- prepare performs no access;
- missing read grant pauses or fails closed;
- "always allow" persists the exact rule;
- plan mutation invalidates approval;
- target drift fails before proxy activation;
- interpretation cannot precede finalized evidence;
- target loss causes no automatic mutation retry; and
- a failed proxy teardown prevents a verified outcome.

### 21. Build and repository placement

The `ffx` subtool, target bindings, and transport integration belong in the
Fuchsia tree. The Python package is built there, at `tools/driver-lab`,
because it links fuchsia-controller and the generated bindings.

Driver Agent adapters, workflow approval objects, and repository-specific
orchestration remain in the external Driver Agent repository.

Every run records exact revisions and dirty-state artifacts for both
repositories. Directory layout is not part of the evidence identity.

### 22. Milestones

#### Milestone H0: transport and control-plane feasibility

- prove the target control plane is reachable through a supported transport;
- produce the serial-transport discovery report (FDomain byte-stream
  connector status, bootstrap-realm host candidates, `ffx` team roadmap);
- enumerate nodes and bound drivers via `fuchsia.driver.development` and
  produce the discovery gap matrix;
- validate unclaimed-node proxy binding via existing registration and bind
  mechanisms; and
- decide the stable Python-to-`ffx` adapter.

Exit: no architecture-critical dependency is assumed rather than demonstrated.

#### Milestone H1: direct mode

- typed node/protocol discovery;
- one direct published-protocol interaction;
- capability reporting;
- structured CLI output; and
- evidence bundle.

Exit: direct mode makes no claim to private resources or target audit.

#### Milestone H2: permissions

- read-grant model;
- interactive allow-once/always-deny flows;
- atomic persistent config;
- unattended fail-closed behavior; and
- plan permission resolution.

Exit: an unapproved register read cannot reach the target.

#### Milestone H3: proxy activation on unclaimed nodes

- direct proxy bind for an unclaimed synthetic node;
- prepare, activate, execute, and end-access workflow;
- stale-state validation;
- disconnect handling;
- evidence of every transition; and
- failure recovery.

Exit: the proxy activates, executes, and deactivates on a synthetic unclaimed
node without manual target intervention.

#### Milestone H4: driver-shaped Python API

- MMIO and representative protocol-backed resource objects;
- capability-aware operation dispatch;
- translation metadata;
- target-local sequence support; and
- async cancellation tests.

Exit: the same overlapping Python operations work through direct and proxy
backends without hiding semantic differences.

#### Milestone H5: Driver Agent and recovery

- proposal/permission/approval/run/interpret separation;
- paniolo integration;
- panic/reboot handling;
- no automatic mutating retry; and
- evidence-linked conclusions.

Exit: an agent can complete an approved read-only investigation and safely
stop for missing authority.

### 23. Definition of done

The phase 1 host tooling is ready when:

- its target control plane has no `core` component dependency;
- supported target transports pass end-to-end tests without precluding a
  serial-capable path;
- direct and proxy modes share one typed public API;
- programming-model differences remain visible;
- private resources are never claimed in direct mode;
- unknown reads require exact operator consent;
- persistent grants are stable, auditable, and revocable;
- mutations require target policy and explicit plan approval;
- proxy access on an unclaimed node activates, executes, and tears down
  verifiably;
- transport loss never triggers automatic mutation replay;
- every run produces a complete hashed evidence bundle;
- Driver Agent separates observation from interpretation;
- no plan can inject arbitrary target paths, monikers, or driver URLs; and
- plan, session, and evidence schemas carry the reserved takeover fields so
  phase 2 requires no major contract change.

### 24. Open decisions

Before implementation, resolve:

1. (deferred, `ffx`-team-owned) The serial-capable target transport; phase 1
   must not preclude it.
2. Resolved: the Python API links fuchsia-controller in process, with the
   generated `fuchsia.driver.lab` bindings behind the transport-neutral
   `ProxyTransport` seam.
3. Which node/resource descriptors are stable enough for grant matching.
4. The user-level versus workspace-level permission-file location and merge
   precedence.
5. Whether persistent read grants may be shared across equivalent physical
   targets.
6. The first standard FIDL resources used to validate programming-model
   fidelity.
7. Whether a narrowing-only `ExtendAllowlist` session operation replaces V1's
   reopen-per-grant behavior.
8. The register-metadata schema, provenance, and review process backing
   consent expansion.

Takeover-related decisions (force-bind, node persistence, restoration policy,
takeover-protocol packaging) live in the Phase 2 section of this document and
in //src/devices/driver-lab/SPEC.md.

### 25. Primary references

- [Fuchsia Controller remote scripting](https://fuchsia.dev/fuchsia-src/development/tools/fuchsia-controller/scripting-remote-actions)
- [Driver communication and services](https://fuchsia.dev/fuchsia-src/concepts/drivers/driver_communication)
- [Fuchsia registers driver](https://fuchsia.dev/fuchsia-src/development/drivers/driver_guides/registers/overview)
- [Driver runner and driver hosts](https://fuchsia.dev/fuchsia-src/concepts/components/v2/driver_runner)
- [FFX overview and target connections](https://fuchsia.dev/fuchsia-src/development/tools/ffx/getting-started)
- [FDomain RFC](https://fuchsia.dev/fuchsia-src/contribute/governance/rfcs/0228_fdomain)

## Phase 2: in-situ driver debugging workflow

### Implementation status

Changesets continue the global numbering:

- [x] CS24 `[driver-lab] Core library extraction & pre-mapped MMIO adapter`
      (in //src/devices/driver-lab/SPEC.md).
- [x] CS25 `[driver-lab] Embedded Rust driver library with ServiceFs integration`
      (in //src/devices/driver-lab/SPEC.md).
- [x] CS26 `[driver-lab] Cooperative locking, quiesce hooks, and interrupt tap support`
      (in //src/devices/driver-lab/SPEC.md).
- [x] CS27 `[driver-lab] Assembly gating, CML debug shard, and production-absence verification`
- [x] CS28 `[driver-lab] Reference integration: synthetic platform driver in testing/`
- [x] CS29 `[driver-lab] Host tooling discovery of embedded debug endpoints and in-situ session workflow`
- [x] CS30 `[driver-lab] In-situ live-target conformance test suite`
- [x] CS34 `[driver-lab] Expose FIDL and debug shard to Bazel`
      (in //src/devices/driver-lab/SPEC.md).
- [x] CS35 `[driver-lab] Native Bazel build for driver_lab_rust`
      (in //src/devices/driver-lab/SPEC.md).
- [x] CS36 `[driver-lab] Add C FFI staticlib shim (driver_lab_c)`
      (in //src/devices/driver-lab/SPEC.md).
- [x] CS37 `[driver-lab] Add C/C++ wrapper and StateVmoBank`
      (in //src/devices/driver-lab/SPEC.md).
- [x] CS38 `[driver-lab] Add C++ embedded server unit tests`
      (in //src/devices/driver-lab/SPEC.md).

Phase: 2 -- in-situ driver debugging workflow.
Extends: the Phase 1 section of this document. The wire client, transport adapters,
CLI parsing, and evidence models apply unchanged.

Companion specification: the proxy driver specification at
`//src/devices/driver-lab/SPEC.md`.

### 1. Purpose

Phase 2 enables host tooling (`ffx driver-lab`, the public Python API, and Driver
Agent) to connect directly to active, bound drivers equipped with the embedded
driver-lab library. This eliminates the need for managed takeover, avoids
disturbing live hardware state, and provides real-time in-situ inspection of
wedged or misbehaving devices.

### 2. Workflow differences: standalone proxy vs in-situ library

| Stage | Phase 1 (Standalone Proxy) | Phase 2 (Embedded In-Situ Library) |
| :--- | :--- | :--- |
| **Node State** | Unclaimed dev-node (no driver bound) | Active driver bound and running |
| **Target Moniker** | `lab_proxy.cm` on synthetic or test node | Bound driver moniker (e.g. `/bootstrap/boot-drivers:sample-driver`) |
| **Transport Connection** | `connect_device_proxy(proxy_moniker, ...)` | `connect_device_proxy(driver_moniker, ...)` |
| **Hardware State** | Freshly initialized / unclaimed | Live in-flight state (preserves bugs) |
| **Teardown / Restore** | Stop `lab_proxy`; discard node | Close session; release mutation lease; driver resumes normal work |

### 3. Design invariants

#### 3.1 No unbind or hardware reset

In-situ debugging connects to the live driver component. It does not unbind the
driver, does not drop power domains or clock votes, and does not reset hardware
registers.

#### 3.2 Safe reads by default

Scalar reads and snapshots default to non-destructive inspection. Known
clear-on-read or FIFO data registers must be marked `hard_denied` in the target
ceiling manifest unless explicitly authorized in an exact session allowlist.

#### 3.3 Cooperative quiesce during mutation

Opening a mutating session acquires an exclusive mutation lease. If registered
by the driver, the library invokes an optional driver-registered synchronous
`quiesce_hook(bool paused)` callback to pause driver-side background polling
loops while host mutations and sequences execute. If complex driver dispatchers
require asynchronous coordination in the future, this interface may be extended
to support an async stream/channel notification.

### 4. Host discovery additions

Host discovery via `fuchsia.driver.development.Manager` remains the discovery
mechanism:
1. `ffx driver-lab list --debug-capable`: Queries active drivers on the target and
   filters for components exposing `fuchsia.driver.lab.Service`.
2. `ffx driver-lab describe --moniker <driver-moniker>`: Fetches `ProxyDescription`
   directly from the live driver's embedded endpoint, inspecting its registered
   MMIO banks, protocol resources, and ceiling policy digest.

### 5. In-situ experiment workflow

#### 5.1 Prepare

1. Query target driver monikers and confirm that `fuchsia.driver.lab.Service`
   is exposed.
2. Run `describe` to fetch registered resources, logical sizes, and the target
   policy digest.
3. Operator approves exact read allowlists and probe plans.

#### 5.2 Execute

1. Create a non-reused evidence directory.
2. Start serial and liveness capture.
3. Open session with exact allowlist and expectations matching live driver state.
4. Execute probe plan (scalar reads, snapshots, timing sequences).
5. If mutating, the driver's internal background loop is paused via the quiesce
   interlock while operations execute.
6. Drain audit ring directly from the driver's embedded buffer.
7. Close session channel.

#### 5.3 Teardown

1. Closing the session releases the mutation lease and unquiesces the driver.
2. No Driver Manager rebind, node restoration, or reboot is required.
3. Stop capture and finalize evidence with driver moniker and hash verification.

### 6. Failure behavior

- **Target Disconnect**: Dropping the FIDL connection closes the session on the
  target; the driver's embedded server immediately clears the mutation lease and
  resumes normal operations.
- **Hardware Wedging**: If a mutating register write locks the hardware bus,
  out-of-band serial capture and target reboot recovery run as specified in
  Phase 1 Milestone H5.

### 7. Host surface additions

- `attach` and `connect` accept a driver component moniker directly without
  requiring an unclaimed node.
- The CLI adds `ffx driver-lab inspect --moniker <id>` as the primary entry point
  for live driver diagnostics.
- Evidence records the driver component moniker and URL in `manifest.json`.

## Phase 3: software state, runtime knobs, and adaptive verification workflow

### Implementation status

Changesets continue the global numbering:

- [x] CS31 `[driver-lab] Software state banks, runtime knobs, and diagnostic triggers`
      (in //src/devices/driver-lab/SPEC.md).
- [x] CS32 `[driver-lab] Host plan aliases and StateHandle API for state and knob experiments`
- [x] CS33 `[driver-lab] Update driver-lab and autoda-fix skills for Mode A/B/C verification`

Phase: 3 -- software state, runtime knobs, and adaptive verification workflow.
Extends: Phases 1 and 2 of this document.

Companion specification: the proxy driver specification at
`//src/devices/driver-lab/SPEC.md`.

### 1. Purpose

Phase 3 enables host probe plans, the asynchronous Python API, and the
`autoda-fix` skill to interact ergonomically with target-side
`StateBank` resources (CS31) for single-build software state and concurrency
experimentation, while selecting the lightest-weight verification mode for any
driver bug.

### 2. Host plan operation aliases and `StateHandle` API (CS32)

#### 2.1 Plan operation aliases (`driver_lab/plans.py`, `api.py`, `cli.py`)

Probe plans support semantic operation kinds for `StateBank` resources both at
top-level `operations` and inside `sequence.items`:

1. **`state_read32`**:
   - Reads a 32-bit state slot or read probe at `offset`.
   - `resource` defaults to `"state0"` when omitted.
   - Maps to `AccessClass.READ_ONCE` (or `AccessClass.SEQUENCE` inside a
     sequence) and executes via `session.read32(resource_id, offset)`.
2. **`state_poll32`**:
   - Polls a 32-bit state slot until `(value & mask) == (expected & mask)` or
     timeout (`timeout_ns` defaults to `100_000_000` when omitted).
   - `resource` defaults to `"state0"` when omitted.
   - Maps to `AccessClass.POLL` and executes via `session.poll32(...)`.
3. **`knob_write32`**:
   - Writes a 32-bit runtime tuning knob or fix-toggle flag at `offset`.
   - `resource` defaults to `"state0"` when omitted; `readback` defaults to
     `True`.
   - Marks the plan as mutating (`is_mutating_plan() == True`), maps to
     `AccessClass.WRITE` (requiring one-time operator consent), and executes via
     `session.write32(...)`.
4. **`trigger_write32`**:
   - Invokes a deterministic diagnostic trigger callback at `offset`.
   - `resource` defaults to `"state0"` when omitted; accepts `arg` or `value`
     (defaulting to `1`); `readback` defaults to `False`.
   - Marks the plan as mutating (`is_mutating_plan() == True`), maps to
     `AccessClass.WRITE` (requiring one-time operator consent), and executes via
     `session.write32(...)`.

Evidence bundles (`operations.jsonl`) preserve the exact semantic operation
`kind` (`"state_read32"`, `"state_poll32"`, `"knob_write32"`,
`"trigger_write32"`) for audit clarity.

#### 2.2 Asynchronous Python `StateHandle` (`driver_lab/session.py`)

`HardwareSession.state(name: str = "state0") -> StateHandle` wraps the named
`StateBank` resource with a driver-shaped asynchronous API:
- `await state.read32(offset) -> int`
- `await state.poll32(offset, expected=0, mask=0xFFFF_FFFF, interval_s=0.001, timeout_s=1.0) -> PollOutcome`
- `await state.write_knob(offset, value, ..., require_readback=True) -> WriteOutcome`
- `await state.trigger(offset, arg=1, ..., require_readback=False) -> WriteOutcome`
- `await state.snapshot32(offsets) -> list[int]`

### 3. Adaptive Mode A/B/C verification workflow (`autoda-fix`, CS33)

The `autoda-fix` skill (`//src/devices/skills/autoda-fix`)
and companion `driver-lab` skill (`//src/devices/skills/driver-lab`) classify
driver bugs during Phase 0 reconnaissance into one of three verification modes:

1. **Mode A: Hardware Register / Interrupt Loop (`in-situ` MMIO/IRQ)**:
   - Used for bugs involving hardware register configuration, bitfields, clocks,
     FIFOs, or interrupts.
   - Verifies live register values and interrupt delivery via `mmio_read32`,
     `mmio_write32`, `mmio_poll32`, and `wait_for_interrupt` with hashed
     evidence bundles.
2. **Mode B: In-Situ Software State & Knob Loop (`in-situ` `StateBank`)**:
   - Used for concurrency/race bugs, state-machine bugs, and retry/timeout
     tuning on live hardware.
   - **Phase 1 (Single Instrumentation Build)**: Instruments suspect code paths
     with a `"state0"` `StateBank` (`define_state_slot`, `define_knob` for fault
     and fix toggles, `define_trigger`) and deploys once (preferring
     `ffx driver restart <driver_url>` for reloadable non-bootfs drivers).
   - **Phase 2 (Zero-Compile Experimentation Loop)**: Executes a **Repro Plan**
     (`knob_write32` with fix knob = `0`, `trigger_write32`, `state_read32`)
     followed in the same boot by a **Fix Proof Plan** (`knob_write32` with fix
     knob = `1`, `trigger_write32`, `state_read32` / `state_poll32`).
   - **Phase 3 (Clean Final Patch)**: Replaces temporary knobs with the clean,
     unconditional production fix and adds a permanent unit/realm regression
     test.
3. **Mode C: Hermetic Test & Log Verification Loop (No `driver-lab` Required)**:
   - Used for pure logic, protocol/message parsing, or deterministic lifecycle
     bugs reproducible via `fx test` (unit/realm tests) or `ffx log`.
   - Explicitly forbids shoe-horning `driver-lab` into Mode C bugs when a unit
     test or log assertion is faster and more direct.
