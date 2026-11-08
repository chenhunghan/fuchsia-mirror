---
canonical_root: src/power/broker
tier: subsystem
parent_inherit: true
cluster_id: src_power_broker_pfc_1
topic: "Daemon Resilience, Lifecycle Terminology & Test Determinism"
total_clusters: 1
scope:
  - "sdk/fidl/fuchsia.power.broker/**"
  - "src/power/broker/**"
---

# `src/power/broker` Review Guidance

Power Broker enforces orderly power-up and orderly power-down across a directed
dependency graph (`dependent ==> requires`, i.e., child requires parent). In
`Catalog`, `claims` maps each `Dependency` to a `Claims` record holding
`pending`, `active`, and `deactivating` lease sets. Both `active` and
`deactivating` claims enforce `requires.level`. A `deactivating` claim belongs
to a dropped lease and stays enforced until dependent elements finish powering
down. Lease targets use root dependencies (`Dependency::new_root`,
`dependent: None`) rather than synthetic vertices in `Topology`.

## Review Rules

### 1.1 Never Panic or Stall the Daemon on Client Inputs or Post-`.await` State

**Rule** (daemon, `src/power/broker/src`): Power Broker runs on a
single-threaded executor (`Rc<RefCell<Broker>>`) as critical system
infrastructure and must never `.unwrap()`, `.expect()`, or `todo!()` on
client-reachable inputs, FIDL streams, or post-`.await` state. The
`_UnknownMethod` arms in `main.rs` still call `todo!()`, and `main()` still
`.expect()`s the result of `run_topology`; treat both as known debt and fix them
when a change touches them:
- **FIDL errors**: Return typed FIDL domain errors (`AddElementError`,
  `fpb::ModifyDependencyError`, `LeaseError::InvalidArgument` / `InvalidLevel`)
  instead of collapsing `LeaseError` failures to `LeaseError::Internal` or
  panicking.
- **Removed parents**: Removing an element redirects each non-removable
  dependency on it to `unsatisfiable_element_id`. A dependent with several
  removed parents hits that same redirected edge again, so treat internal
  `topology::ModifyDependencyError::AlreadyExists` as expected there.
- **Across `.await` points**: Never hold `RefCell::borrow_mut()` across
  `.await`, and tolerate concurrent lease or element removal after `.await` by
  logging expected `drop_lease` errors at `debug` rather than `.expect()`ing.
- **`ElementRunnerHandler`**: While `initial_lease_token` is open, the runner
  suppresses `SetLevel` calls below the initial level. If `initial_lease_token`
  closes early (`EVENTPAIR_PEER_CLOSED`), send the most recently suppressed
  level. On element removal, let any in-flight `SetLevel` call complete before
  exiting.
- **Inspect & `StuckDetector`**: Keep Inspect writes infallible so a full
  Inspect VMO never aborts broker operations, and ensure
  `StuckDetector::spawn_watchdog` never computes a zero-second `next_interval_s`
  (`Timer` deadline of `after(0)`).

**Rationale**: Clients can pass invalid levels, send unknown flexible requests,
close channels, or remove elements concurrently across `.await` points.
Panicking or spinning on a zero-duration timer takes down system-wide power
management, and dropping an initial or in-flight `SetLevel` leaves hardware out
of sync with broker state.

```rust
// Bad: Panics if the required element was concurrently removed while waiting
// for provisional lease satisfaction.
while let Some(status) = rx.next().await {
    if status == Some(LeaseStatus::Satisfied) { break; }
}
self.broker.borrow_mut().drop_lease(lease_id).expect("drop_lease failed");

// Good: Tolerate and log concurrent lease cleanup across .await.
while let Some(status) = rx.next().await {
    if status == Some(LeaseStatus::Satisfied) { break; }
}
if let Err(err) = self.broker.borrow_mut().drop_lease(lease_id) {
    log::debug!("drop_lease {lease_id} failed: {err:?}");
}
```

*Provenance:
[fxr/1089556](https://fuchsia-review.googlesource.com/c/fuchsia/+/1089556),
[fxr/1127933](https://fuchsia-review.googlesource.com/c/fuchsia/+/1127933),
[fxr/1214364](https://fuchsia-review.googlesource.com/c/fuchsia/+/1214364),
[fxr/1483078](https://fuchsia-review.googlesource.com/c/fuchsia/+/1483078),
[fxr/1527856](https://fuchsia-review.googlesource.com/c/fuchsia/+/1527856),
[fxr/1604336](https://fuchsia-review.googlesource.com/c/fuchsia/+/1604336),
[fxr/1821469](https://fuchsia-review.googlesource.com/c/fuchsia/+/1821469),
[fxr/1884100](https://fuchsia-review.googlesource.com/c/fuchsia/+/1884100)*

---

### 1.2 Use Specific Lifecycle Verbs Instead of Ambiguous `"release"`

**Rule**: Never use `"release"` in new or modified identifiers, comments, or
commit messages to mean ending a lease or claim. Use the verb matching the exact
lifecycle transition:
- A client *drops* a lease (`Broker::drop_lease`, moving it to
  `LeaseStatus::PoweringDown`), and the broker *drops* a claim
  (`Catalog::drop_claim`, removing it).
- The broker marks a dropped lease's claims *to deactivate*
  (`Catalog::mark_to_deactivate`, moving them from `active` to `deactivating`),
  and *deactivates* an open lease's broken claim (`Catalog::deactivate_claim`,
  moving it from `active` back to `pending`).
- A lease is *vacated* (`vacate_lease_if_all_claims_dropped`,
  `LeaseStatus::Vacated`) once its last claim is dropped.

**Rationale**: `"release"` is ambiguous with `"re-lease"` (acquiring a lease
again) and blurs distinct states (`PoweringDown`, `deactivating`, `pending`, and
`Vacated`) that have different enforcement semantics.

*Provenance:
[fxr/1819109](https://fuchsia-review.googlesource.com/c/fuchsia/+/1819109),
[fxr/1878590](https://fuchsia-review.googlesource.com/c/fuchsia/+/1878590)*

---

### 1.3 Assert State-Machine and Catalog Invariants and Keep Tests Deterministic

**Rule**:
- In `broker.rs` unit tests, call `Catalog::assert_claims_consistent()` after
  claim transitions, call `assert_element_cleaned_up()` or
  `assert_lease_cleaned_up()` after removing elements or dropping leases, and
  enforce transition invariants via `#[cfg(test)]` assertions and unit tests
  (e.g., verifying that no droppable `deactivating` claims remain after level or
  lease updates, satisfied dependent levels keep their required parent levels
  enforced, and disorderly power-downs scope teardown to unsatisfied reverse
  branches).
- In `tests/integration`, before asserting that an element did *not* receive a
  `SetLevel` (`executor.run_until_stalled(&mut runner_next).is_pending()`),
  reply with `assert_send_response_updates_level_to` so `Status.WatchPowerLevel`
  proves the broker processed the reply (after consuming the initial immediate
  `WatchPowerLevel` value). Hanging-get coalescing tests intentionally skip
  intermediate `WatchPowerLevel` calls.
- Use `>= 3` levels (`[0, 1, 2]`) when testing hanging-get coalescing, and use
  `TestExecutor::new_with_fake_time()` in `main.rs` `StuckDetector` unit tests
  rather than real-time sleeps.

**Rationale**: The `Catalog` and transition assertions verify that secondary
indexes retain no orphaned entries and that refactors preserve orderly and
disorderly transition invariants. Because Power Broker runs as a child component
in integration tests, sending a `SetLevel` reply only queues the FIDL message;
a negative `run_until_stalled(&mut runner_next).is_pending()` check passes
vacuously unless `WatchPowerLevel` confirms the broker processed the reply
first. Two-state elements (`[0, 1]`) hide intermediate coalescing bugs, and fake
time keeps multi-second watchdog unit tests fast and deterministic.

```rust
// Bad: Checking that dependent_runner has no pending SetLevel right after
// responder.send() passes vacuously before the broker processes the reply.
let responder =
    assert_set_level_required_eq_and_return_responder(parent_runner.try_next(), 0).await;
responder.send().expect("set_level resp failed");
let mut dep_next = dependent_runner.try_next();
assert!(executor.run_until_stalled(&mut dep_next).is_pending());

// Good: After consuming `parent_status`'s initial level during setup, await
// WatchPowerLevel via assert_send_response_updates_level_to before the negative
// run_until_stalled check.
let responder =
    assert_set_level_required_eq_and_return_responder(parent_runner.try_next(), 0).await;
assert_send_response_updates_level_to(responder, &parent_status, 0).await;
let mut dep_next = dependent_runner.try_next();
assert!(executor.run_until_stalled(&mut dep_next).is_pending());
```

*Provenance:
[fxr/1049893](https://fuchsia-review.googlesource.com/c/fuchsia/+/1049893),
[fxr/1062918](https://fuchsia-review.googlesource.com/c/fuchsia/+/1062918),
[fxr/1143272](https://fuchsia-review.googlesource.com/c/fuchsia/+/1143272),
[fxr/1387934](https://fuchsia-review.googlesource.com/c/fuchsia/+/1387934),
[fxr/1604336](https://fuchsia-review.googlesource.com/c/fuchsia/+/1604336),
[fxr/1819109](https://fuchsia-review.googlesource.com/c/fuchsia/+/1819109)*

---

## Removed and Deprecated Mechanisms

`Lessor` (`ElementSchema.lessor_channel`, `Lessor.Lease`) is deprecated
([fxr/1603730](https://fuchsia-review.googlesource.com/c/fuchsia/+/1603730)) and
`LeaseControl.WatchStatus` is slated for removal
([fxr/1056073](https://fuchsia-review.googlesource.com/c/fuchsia/+/1056073)):
Power Broker still serves them for existing callers, so do not flag maintenance
of the server implementation, but require new client code to use
`Topology.Lease` with `LeaseToken` (`zx::EventPair`).

The following Power Broker mechanisms have been removed completely. Do **not**
ask authors to handle them, and flag any change that reintroduces them:
- **Opportunistic / passive dependencies & lease contingency**: Removed
  ([fxr/1425756](https://fuchsia-review.googlesource.com/c/fuchsia/+/1425756),
  [fxr/1469140](https://fuchsia-review.googlesource.com/c/fuchsia/+/1469140),
  [fxr/1475123](https://fuchsia-review.googlesource.com/c/fuchsia/+/1475123)).
  All dependencies are assertive.
- **`LevelControl` (`RequiredLevel.Watch` / `CurrentLevel.Update`)**: Removed
  ([fxr/1361731](https://fuchsia-review.googlesource.com/c/fuchsia/+/1361731),
  [fxr/1382421](https://fuchsia-review.googlesource.com/c/fuchsia/+/1382421)).
  Elements use `ElementRunner.SetLevel`.
- **Synthetic per-lease elements in `Topology`**: Removed
  ([fxr/1821469](https://fuchsia-review.googlesource.com/c/fuchsia/+/1821469)).
  Leases hold `targets: Vec<ElementLevel>` with root `Dependency` entries in
  `Catalog`.
- **Per-claim `Claim` / `ClaimID` structs**: Replaced by dependency-keyed
  `Claims` sets
  ([fxr/1819109](https://fuchsia-review.googlesource.com/c/fuchsia/+/1819109)).
- **Client `LeaseHelper`**: Removed
  ([fxr/1511617](https://fuchsia-review.googlesource.com/c/fuchsia/+/1511617)).
