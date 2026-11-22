// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Error, Result};
use assert_matches::assert_matches;
use fidl::endpoints::{Proxy, create_endpoints, create_proxy};
use fidl_fuchsia_feedback as ffeedback;
use fidl_fuchsia_hardware_power_statecontrol as fstatecontrol;
use fidl_fuchsia_power_broker::{
    self as fpb, BinaryPowerLevel, ElementSchema, LevelDependency, StatusMarker, TopologyMarker,
    TopologyProxy,
};
use fuchsia_async as fasync;
use fuchsia_component_test::{
    Capability, ChildOptions, LocalComponentHandles, RealmBuilder, RealmInstance, Ref, Route,
};
use futures_util::{FutureExt, StreamExt, TryStreamExt};
use power_broker_client::BINARY_POWER_LEVELS;
use std::thread;
use std::time::Duration;

async fn build_power_broker_realm() -> Result<RealmInstance, Error> {
    let builder = RealmBuilder::new().await?;
    let power_broker =
        builder.add_child("power_broker", "#meta/power-broker.cm", ChildOptions::new()).await?;

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol::<TopologyMarker>())
                .from(&power_broker)
                .to(Ref::parent()),
        )
        .await?;
    let realm = builder.build().await?;
    Ok(realm)
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::{Context as _, anyhow, ensure};
    use fidl_fuchsia_power_broker::StatusProxy;
    use fpb::{
        ElementControlMarker, ElementControlProxy, ElementRunnerMarker, ElementRunnerRequest,
        ElementRunnerRequestStream, LeaseDependency, LeaseError, LeaseSchema,
    };
    use fuchsia_async::TimeoutExt;
    use std::sync::mpsc;
    use test_case::test_case;

    const CRASH_REPORT_TIMEOUT_SECONDS: i64 = 20;
    const LEASE_PENDING_POLL_INTERVAL_MILLIS: i64 = 10;
    const LEASE_SATISFIED_SIGNAL: zx::Signals =
        zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED);
    const REBOOT_TIMEOUT_SECONDS: i64 = 20;
    const WATCH_LEVEL_TIMEOUT_SECONDS: i64 = 20;

    #[derive(Debug)]
    struct TestLease {
        name: String,
        token: zx::EventPair,
    }

    impl TestLease {
        #[track_caller]
        fn is_satisfied(&self) -> bool {
            match self.token.wait_one(
                LEASE_SATISFIED_SIGNAL | zx::Signals::EVENTPAIR_PEER_CLOSED,
                zx::MonotonicInstant::INFINITE_PAST,
            ) {
                zx::WaitResult::Ok(signals) | zx::WaitResult::TimedOut(signals) => {
                    assert!(
                        !signals.contains(zx::Signals::EVENTPAIR_PEER_CLOSED),
                        "{}: Power Broker closed LeaseToken",
                        self.name
                    );
                    signals.contains(LEASE_SATISFIED_SIGNAL)
                }
                zx::WaitResult::Canceled(_) => panic!("{}: LeaseToken wait canceled", self.name),
                zx::WaitResult::Err(status) => {
                    panic!("{}: LeaseToken wait failed: {status}", self.name)
                }
            }
        }

        #[track_caller]
        fn assert_pending(&self) {
            assert!(!self.is_satisfied(), "{}: expected lease to be pending", self.name);
        }

        #[track_caller]
        fn assert_satisfied(&self) {
            assert!(self.is_satisfied(), "{}: expected lease to be satisfied", self.name);
        }

        async fn await_satisfied(&self) {
            let observed = fasync::OnSignals::new(
                &self.token,
                LEASE_SATISFIED_SIGNAL | zx::Signals::EVENTPAIR_PEER_CLOSED,
            )
            .on_timeout(
                fasync::MonotonicDuration::from_seconds(WATCH_LEVEL_TIMEOUT_SECONDS),
                || panic!("{}: timed out waiting for LEASE_SIGNAL_SATISFIED", self.name),
            )
            .await
            .expect("wait for LEASE_SIGNAL_SATISFIED failed");
            assert!(
                !observed.contains(zx::Signals::EVENTPAIR_PEER_CLOSED),
                "{}: Power Broker closed LeaseToken before satisfaction ({observed:?})",
                self.name
            );
            self.assert_satisfied();
        }

        /// Waits for `LEASE_SIGNAL_SATISFIED` to clear after an event that transitions an active
        /// lease back to pending. Zircon cannot wait asynchronously for a signal bit to de-assert,
        /// so poll with a short timer delay.
        async fn await_pending(&self) {
            let deadline = fasync::MonotonicInstant::after(
                fasync::MonotonicDuration::from_seconds(WATCH_LEVEL_TIMEOUT_SECONDS),
            );
            while self.is_satisfied() {
                assert!(
                    fasync::MonotonicInstant::now() < deadline,
                    "{}: timed out waiting for LEASE_SIGNAL_SATISFIED to clear",
                    self.name
                );
                fasync::Timer::new(fasync::MonotonicDuration::from_millis(
                    LEASE_PENDING_POLL_INTERVAL_MILLIS,
                ))
                .await;
            }
        }
    }

    struct TestElement {
        name: String,
        topology: TopologyProxy,
        control: ElementControlProxy,
        status: StatusProxy,
        runner: ElementRunnerRequestStream,
        token: fpb::DependencyToken,
    }

    impl TestElement {
        /// Returns a builder that registers a test element on `topology` starting at
        /// `valid_levels[0]`.
        fn builder<'a>(
            topology: &'a TopologyProxy,
            name: &'a str,
            valid_levels: &'a [u8],
        ) -> TestElementBuilder<'a> {
            TestElementBuilder { topology, name, valid_levels, dependencies: Vec::new() }
        }

        fn dep_token(&self) -> fpb::DependencyToken {
            self.token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed")
        }

        /// Requests a direct lease on this element at `level` with `should_return_pending_lease`
        /// enabled so callers can drive element runners before awaiting satisfaction.
        async fn try_lease(&self, level: u8) -> Result<TestLease, LeaseError> {
            let name = format!("{}_lease_{level}", self.name);
            let (token, server) = zx::EventPair::create();
            self.topology
                .lease(LeaseSchema {
                    lease_token: Some(server),
                    lease_name: Some(name.clone()),
                    dependencies: Some(vec![LeaseDependency {
                        requires_token: Some(self.dep_token()),
                        requires_level: Some(level),
                        ..Default::default()
                    }]),
                    should_return_pending_lease: Some(true),
                    ..Default::default()
                })
                .await
                .expect("Topology.Lease FIDL failed")?;
            // Drain Power Broker's executor via a fresh Status channel so `monitor_lease`'s initial
            // `LeaseToken` signal update has run before any `assert_satisfied()` or
            // `assert_pending()` check.
            let _ = self.current_power_level().await;
            Ok(TestLease { name, token })
        }

        async fn lease(&self, level: u8) -> TestLease {
            self.try_lease(level).await.expect("Topology.Lease error")
        }

        /// Queries the element's current power level via a fresh `Status` channel, giving queued
        /// Power Broker executor tasks (such as `LeaseToken` signal updates) a round-trip to run.
        /// Blocks until timeout if this element's `ElementControl` is currently awaiting
        /// `AddDependency`; query a different element in that case.
        async fn current_power_level(&self) -> u8 {
            let (status, status_server) = create_proxy::<StatusMarker>();
            self.control.open_status_channel(status_server).expect("open_status_channel failed");
            status
                .watch_power_level()
                .on_timeout(
                    fasync::MonotonicDuration::from_seconds(WATCH_LEVEL_TIMEOUT_SECONDS),
                    || panic!("{}: timed out waiting for current_power_level", self.name),
                )
                .await
                .expect("watch_power_level FIDL failed")
                .expect("watch_power_level error")
        }

        /// Awaits the next `ElementRunner::SetLevel` request, asserts it matches `expected`,
        /// and returns the responder so callers can inspect intermediate state before replying.
        async fn expect_set_level(&mut self, expected: u8) -> fpb::ElementRunnerSetLevelResponder {
            match self.runner.try_next().await.expect("ElementRunner FIDL failed") {
                Some(fpb::ElementRunnerRequest::SetLevel { level, responder }) => {
                    assert_eq!(level, expected, "{}: SetLevel level", self.name);
                    responder
                }
                other => panic!("{}: expected SetLevel({expected}), got {other:?}", self.name),
            }
        }

        /// Replies to a pending `SetLevel` request and verifies `Status::WatchPowerLevel` reports
        /// `expected`.
        async fn complete_set_level(
            &mut self,
            responder: fpb::ElementRunnerSetLevelResponder,
            expected: u8,
        ) {
            responder.send().expect("set_level resp failed");
            let observed = self
                .status
                .watch_power_level()
                .await
                .expect("watch_power_level FIDL failed")
                .expect("watch_power_level error");
            assert_eq!(observed, expected, "{}: WatchPowerLevel level", self.name);
        }

        /// Awaits `SetLevel(expected)`, replies immediately, and verifies `Status::WatchPowerLevel`.
        async fn accept_set_level(&mut self, expected: u8) {
            let responder = self.expect_set_level(expected).await;
            self.complete_set_level(responder, expected).await;
        }

        /// Asserts that no `ElementRunner` request is currently queued for this element.
        #[track_caller]
        fn assert_no_set_level(&mut self, executor: &mut fasync::TestExecutor) {
            assert!(
                executor.run_until_stalled(&mut self.runner.try_next()).is_pending(),
                "{}: expected no pending ElementRunner request",
                self.name
            );
        }
    }

    struct TestElementBuilder<'a> {
        topology: &'a TopologyProxy,
        name: &'a str,
        valid_levels: &'a [u8],
        dependencies: Vec<LevelDependency>,
    }

    impl<'a> TestElementBuilder<'a> {
        /// Adds a dependency: at `dependent_level`, this element requires `required` to be at
        /// `required_level`.
        fn dep(self, dependent_level: u8, required: &TestElement, required_level: u8) -> Self {
            self.dep_with_removable(dependent_level, required, required_level, None)
        }

        /// Like [`Self::dep`], but sets `LevelDependency.remove_with_required_element` (`None` uses
        /// the broker default, which is not removable).
        fn dep_with_removable(
            mut self,
            dependent_level: u8,
            required: &TestElement,
            required_level: u8,
            remove_with_required_element: Option<bool>,
        ) -> Self {
            self.dependencies.push(LevelDependency {
                dependent_level: Some(dependent_level),
                requires_token: Some(required.dep_token()),
                requires_level_by_preference: Some(vec![required_level]),
                remove_with_required_element,
                ..Default::default()
            });
            self
        }

        /// Adds the element starting at `valid_levels[0]` with its initial `SetLevel` request still
        /// queued on `runner` and no `Status.WatchPowerLevel` call made yet.
        async fn build_unsettled(self) -> TestElement {
            let initial_level = *self.valid_levels.first().expect("valid_levels must not be empty");
            let name = self.name.to_string();
            let token = zx::Event::create();
            let (runner_client, runner_server) = create_endpoints::<ElementRunnerMarker>();
            let runner = runner_server.into_stream();
            let (control, control_server) = create_proxy::<ElementControlMarker>();
            self.topology
                .add_element(ElementSchema {
                    element_name: Some(name.clone()),
                    initial_current_level: Some(initial_level),
                    valid_levels: Some(self.valid_levels.to_vec()),
                    dependencies: (!self.dependencies.is_empty()).then_some(self.dependencies),
                    element_control: Some(control_server),
                    element_runner: Some(runner_client),
                    ..Default::default()
                })
                .await
                .expect("add_element FIDL failed")
                .expect("add_element error");
            control
                .register_dependency_token(
                    token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
                )
                .await
                .expect("register_dependency_token FIDL failed")
                .expect("register_dependency_token error");
            let (status, status_server) = create_proxy::<StatusMarker>();
            control.open_status_channel(status_server).expect("open_status_channel failed");
            TestElement { name, topology: self.topology.clone(), control, status, runner, token }
        }

        /// Like [`Self::build_unsettled`], but also acknowledges the initial `SetLevel` and
        /// consumes the initial `Status.WatchPowerLevel` observation.
        async fn build(self) -> TestElement {
            let initial_level = *self.valid_levels.first().expect("valid_levels must not be empty");
            let mut element = self.build_unsettled().await;
            element.accept_set_level(initial_level).await;
            element
        }
    }

    /// Verifies that attempting to add an element without providing an `element_runner` channel
    /// returns an `AddElementError::Invalid` error.
    #[fuchsia::test]
    async fn test_element_runner_invalid() -> Result<()> {
        let realm = build_power_broker_realm().await?;

        // Create a topology with only two elements and a single dependency:
        // P <- C
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;
        let (_, element_control_server) = create_proxy::<ElementControlMarker>();
        // Not supplying element_runner is invalid.
        let invalid_resp = topology
            .add_element(ElementSchema {
                element_name: Some("P".into()),
                initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                element_control: Some(element_control_server),
                ..Default::default()
            })
            .await;
        assert_matches!(invalid_resp, Ok(Err(fpb::AddElementError::Invalid)));
        Ok(())
    }

    /// Verifies direct parent-child dependency handling: leasing a child powers up its parent
    /// first, then powers up the child to satisfy the lease, and dropping the lease steps both down
    /// in reverse order.
    #[fuchsia::test]
    fn test_direct() -> Result<()> {
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;

        // Create a topology with only two elements and a single dependency:
        // P <- C
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;
        let on = BinaryPowerLevel::On.into_primitive();
        let off = BinaryPowerLevel::Off.into_primitive();
        let (mut parent, mut child) = executor.run_singlethreaded(async {
            let mut parent =
                TestElement::builder(&topology, "P", &BINARY_POWER_LEVELS).build_unsettled().await;
            let mut child = TestElement::builder(&topology, "C", &BINARY_POWER_LEVELS)
                .dep(on, &parent, on)
                .build_unsettled()
                .await;
            parent.accept_set_level(off).await;
            child.accept_set_level(off).await;
            (parent, child)
        });

        // Attempt to lease an invalid level, this should fail.
        executor.run_singlethreaded(async {
            assert_matches!(child.try_lease(100).await, Err(LeaseError::InvalidLevel));
        });

        // Acquire lease for C.
        // P's required level should become ON.
        // C's required level should remain OFF until P turns ON.
        // Lease should be pending.
        let lease = executor.run_singlethreaded(child.lease(on));
        let parent_current = executor.run_singlethreaded(parent.expect_set_level(on));
        child.assert_no_set_level(&mut executor);
        assert_eq!(executor.run_singlethreaded(child.current_power_level()), off);
        lease.assert_pending();

        // Update P's current level to ON.
        // P's required level should remain ON.
        // C's required level should become ON.
        executor.run_singlethreaded(parent.complete_set_level(parent_current, on));
        parent.assert_no_set_level(&mut executor);
        let child_current = executor.run_singlethreaded(child.expect_set_level(on));

        // Update C's current level to ON.
        // Lease should become satisfied.
        executor.run_singlethreaded(async {
            child.complete_set_level(child_current, on).await;
            lease.await_satisfied().await;
        });

        // Drop lease.
        // C's required level should become OFF.
        drop(lease);
        let child_current = executor.run_singlethreaded(child.expect_set_level(off));

        // Update C's current level to OFF.
        // P's required level should become OFF.
        let parent_current = executor.run_singlethreaded(async {
            child.complete_set_level(child_current, off).await;
            parent.expect_set_level(off).await
        });

        // Update P's current level to OFF.
        executor.run_singlethreaded(parent.complete_set_level(parent_current, off));

        // Remove P's element. Status channel should be closed.
        executor.run_singlethreaded(async {
            drop(parent.control);
            let status_after_remove = parent.status.watch_power_level().await;
            assert_matches!(status_after_remove, Err(fidl::Error::ClientChannelClosed { .. }));
        });
        // Remove C's element. Status channel should be closed.
        executor.run_singlethreaded(async {
            drop(child.control);
            let status_after_remove = child.status.watch_power_level().await;
            assert_matches!(status_after_remove, Err(fidl::Error::ClientChannelClosed { .. }));
        });

        Ok(())
    }

    /// Verifies dynamically adding dependencies to an element: inactive dependencies take no
    /// immediate effect, while active dependencies power up the required element before completing.
    #[fuchsia::test]
    fn test_add_dependency() -> Result<()> {
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;

        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        let (mut required, mut dependent) = executor.run_singlethreaded(async {
            let required = TestElement::builder(&topology, "Required", &[0, 1, 2]).build().await;
            let dependent = TestElement::builder(&topology, "Dependent", &[0, 1, 2]).build().await;
            (required, dependent)
        });

        // Lease Dependent at 1.
        let lease = executor.run_singlethreaded(async {
            let lease = dependent.lease(1).await;
            dependent.accept_set_level(1).await;
            lease.await_satisfied().await;
            lease
        });

        // Case 1: Unused Dependency.
        // Add dependency that WILL NOT be used: Dependent at 2 requires Required at 2.
        // Since Dependent is only at 1, this should not raise Required's level from 0.
        executor.run_singlethreaded(async {
            dependent
                .control
                .add_dependency(fpb::LevelDependency {
                    dependent_level: Some(2),
                    requires_token: Some(required.dep_token()),
                    requires_level_by_preference: Some(vec![2]),
                    ..Default::default()
                })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(dependent.current_power_level().await, 1);
        });

        // Verify Required level is still 0 and Dependent's lease is still Satisfied (`LeaseToken`
        // signals are sampled at checkpoints and paired with `assert_no_set_level`).
        required.assert_no_set_level(&mut executor);
        dependent.assert_no_set_level(&mut executor);
        lease.assert_satisfied();

        // Case 2: Used Dependency.
        // Now, add dependency that WILL be used: Dependent at 1 requires Required at 1.
        // Since Dependent is already at level 1, this dependency becomes ACTIVE immediately!
        let mut add_dep_fut = Box::pin(dependent.control.add_dependency(fpb::LevelDependency {
            dependent_level: Some(1),
            requires_token: Some(required.dep_token()),
            requires_level_by_preference: Some(vec![1]),
            ..Default::default()
        }));

        // add_dependency call should be blocked because it is waiting for Required to go to 1.
        assert!(executor.run_until_stalled(&mut add_dep_fut).is_pending());

        // Verify Required increases to 1 while Dependent's existing lease is still Satisfied.
        // Query `required` rather than `dependent` while `dependent.control` is blocked in
        // `add_dependency`.
        let required_current = executor.run_singlethreaded(required.expect_set_level(1));
        assert_eq!(executor.run_singlethreaded(required.current_power_level()), 0);
        dependent.assert_no_set_level(&mut executor);
        lease.assert_satisfied();
        executor.run_singlethreaded(required.complete_set_level(required_current, 1));

        // Now add_dep_fut should complete!
        executor.run_singlethreaded(async {
            add_dep_fut.await.unwrap().unwrap();
            assert_eq!(dependent.current_power_level().await, 1);
        });

        // Verify Dependent stayed at 1 and the lease is still Satisfied.
        dependent.assert_no_set_level(&mut executor);
        lease.assert_satisfied();

        Ok(())
    }

    /// Verifies removing a dependency of an element: only that dependency is removed, the required
    /// element is lowered right away without affecting the lease on the dependent element, and
    /// removing a dependency that is not valid, or that the element does not have, fails.
    #[fuchsia::test]
    fn test_remove_dependency() -> Result<()> {
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;

        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        let (mut required, mut dependent) = executor.run_singlethreaded(async {
            let required = TestElement::builder(&topology, "Required", &[0, 1, 2]).build().await;
            let dependent = TestElement::builder(&topology, "Dependent", &[0, 1, 2]).build().await;
            (required, dependent)
        });

        // Lease Dependent at 1.
        let lease = executor.run_singlethreaded(async {
            let lease = dependent.lease(1).await;
            dependent.accept_set_level(1).await;
            lease.await_satisfied().await;
            lease
        });

        // Add a dependency that is used: Dependent at 1 requires Required at 1. Since Dependent is
        // already at level 1, the call waits for Required to go to 1.
        let mut add_dep_fut = Box::pin(dependent.control.add_dependency(fpb::LevelDependency {
            dependent_level: Some(1),
            requires_token: Some(required.dep_token()),
            requires_level_by_preference: Some(vec![1]),
            ..Default::default()
        }));
        assert!(executor.run_until_stalled(&mut add_dep_fut).is_pending());
        executor.run_singlethreaded(async {
            required.accept_set_level(1).await;
            add_dep_fut.await.unwrap().unwrap();
            assert_eq!(dependent.current_power_level().await, 1);
        });
        dependent.assert_no_set_level(&mut executor);
        lease.assert_satisfied();

        // Add another dependency on Required that the lease does not use: Dependent at 2 requires
        // Required at 2.
        executor.run_singlethreaded(async {
            dependent
                .control
                .add_dependency(fpb::LevelDependency {
                    dependent_level: Some(2),
                    requires_token: Some(required.dep_token()),
                    requires_level_by_preference: Some(vec![2]),
                    ..Default::default()
                })
                .await
                .unwrap()
                .unwrap();
            assert_eq!(dependent.current_power_level().await, 1);
        });
        dependent.assert_no_set_level(&mut executor);
        lease.assert_satisfied();

        // The dependency of Dependent at `dependent_level` upon Required at `requires_level`.
        let required_token = required.dep_token();
        let dependency = |dependent_level, requires_level| fpb::LevelDependency {
            dependent_level: Some(dependent_level),
            requires_token: Some(
                required_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
            ),
            requires_level_by_preference: Some(vec![requires_level]),
            ..Default::default()
        };

        // Removing a dependency fails if it is not valid, if the token is not registered, or if
        // the element does not have the dependency.
        executor.run_singlethreaded(async {
            // The dependent level is missing.
            let invalid_resp = dependent
                .control
                .remove_dependency(fpb::LevelDependency {
                    dependent_level: None,
                    ..dependency(1, 1)
                })
                .await;
            assert_matches!(invalid_resp, Ok(Err(fpb::ModifyDependencyError::Invalid)));

            // The token is not registered.
            let not_authorized_resp = dependent
                .control
                .remove_dependency(fpb::LevelDependency {
                    requires_token: Some(zx::Event::create()),
                    ..dependency(1, 1)
                })
                .await;
            assert_matches!(
                not_authorized_resp,
                Ok(Err(fpb::ModifyDependencyError::NotAuthorized))
            );

            // Required does not have a dependency upon Dependent, only the other way around.
            let not_found_resp = required
                .control
                .remove_dependency(fpb::LevelDependency {
                    requires_token: Some(dependent.dep_token()),
                    ..dependency(1, 1)
                })
                .await;
            assert_matches!(not_found_resp, Ok(Err(fpb::ModifyDependencyError::NotFound)));

            // Dependent has dependencies upon Required, but neither at level 1 upon level 2, nor
            // at level 2 upon level 1.
            for (dependent_level, requires_level) in [(1, 2), (2, 1)] {
                let not_found_resp = dependent
                    .control
                    .remove_dependency(dependency(dependent_level, requires_level))
                    .await;
                assert_matches!(not_found_resp, Ok(Err(fpb::ModifyDependencyError::NotFound)));
            }
            assert_eq!(dependent.current_power_level().await, 1);
        });

        // None of the failures changed anything.
        required.assert_no_set_level(&mut executor);
        dependent.assert_no_set_level(&mut executor);
        lease.assert_satisfied();

        // Remove the dependency that the lease does not use. The other one, which the lease does
        // use, is not affected, so Required is not lowered.
        executor.run_singlethreaded(async {
            dependent.control.remove_dependency(dependency(2, 2)).await.unwrap().unwrap();
            assert_eq!(dependent.current_power_level().await, 1);
        });
        required.assert_no_set_level(&mut executor);
        dependent.assert_no_set_level(&mut executor);
        lease.assert_satisfied();

        // Remove the dependency that the lease uses. It does not wait for Required to be lowered,
        // which happens regardless of the lease on Dependent.
        executor.run_singlethreaded(async {
            dependent.control.remove_dependency(dependency(1, 1)).await.unwrap().unwrap();
        });
        let required_current = executor.run_singlethreaded(required.expect_set_level(0));
        assert_eq!(executor.run_singlethreaded(required.current_power_level()), 1);
        dependent.assert_no_set_level(&mut executor);
        lease.assert_satisfied();
        executor.run_singlethreaded(required.complete_set_level(required_current, 0));

        // Dependent stays at 1, and the lease is still satisfied.
        assert_eq!(executor.run_singlethreaded(dependent.current_power_level()), 1);
        dependent.assert_no_set_level(&mut executor);
        lease.assert_satisfied();

        // Both of the dependencies were removed, so there is none left to remove.
        executor.run_singlethreaded(async {
            let not_found_resp = dependent.control.remove_dependency(dependency(1, 1)).await;
            assert_matches!(not_found_resp, Ok(Err(fpb::ModifyDependencyError::NotFound)));
        });

        Ok(())
    }

    /// Verifies that `Topology.Lease` blocks until the requested lease dependencies are satisfied
    /// when `should_return_pending_lease` is false.
    #[fuchsia::test]
    fn test_topology_lease_wait_for_satisfied() -> Result<()> {
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        let mut element = executor.run_singlethreaded(async {
            TestElement::builder(&topology, "E", &[0, 1]).build().await
        });

        let (lease_token_client, lease_token_server) = zx::EventPair::create();

        // Call Topology.Lease with should_return_pending_lease = false (default)
        // It should block until we update current level to 1.
        let mut lease_fut = Box::pin(topology.lease(fpb::LeaseSchema {
            lease_token: Some(lease_token_server),
            lease_name: Some("test_lease".into()),
            dependencies: Some(vec![fpb::LeaseDependency {
                requires_token: Some(element.dep_token()),
                requires_level: Some(1),
                ..Default::default()
            }]),
            ..Default::default()
        }));

        assert!(executor.run_until_stalled(&mut lease_fut).is_pending());

        // Now update current level to 1
        executor.run_singlethreaded(element.accept_set_level(1));

        // Now the lease future should complete
        executor.run_singlethreaded(async {
            lease_fut.await.unwrap().unwrap();
        });

        executor.run_singlethreaded(async {
            fasync::OnSignals::new(
                &lease_token_client,
                zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED),
            )
            .await
            .unwrap();
        });

        Ok(())
    }

    /// Verifies that `Topology.Lease` returns immediately with a pending lease when
    /// `should_return_pending_lease` is true, and signals satisfaction once dependencies are met.
    #[fuchsia::test]
    async fn test_topology_lease_return_pending() -> Result<()> {
        let realm = build_power_broker_realm().await?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        let mut element = TestElement::builder(&topology, "E", &[0, 1]).build().await;

        let (lease_token_client, lease_token_server) = zx::EventPair::create();

        // Call Topology.Lease with should_return_pending_lease = true
        // It should return immediately.
        topology
            .lease(fpb::LeaseSchema {
                lease_token: Some(lease_token_server),
                lease_name: Some("test_lease".into()),
                dependencies: Some(vec![fpb::LeaseDependency {
                    requires_token: Some(element.dep_token()),
                    requires_level: Some(1),
                    ..Default::default()
                }]),
                should_return_pending_lease: Some(true),
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap();

        assert_matches!(
            lease_token_client.wait_one(
                zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED),
                zx::MonotonicInstant::INFINITE_PAST,
            ),
            zx::WaitResult::TimedOut(_)
        );

        // Now update current level to 1
        element.accept_set_level(1).await;

        fasync::OnSignals::new(
            &lease_token_client,
            zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED),
        )
        .await
        .unwrap();

        Ok(())
    }

    /// Verifies transitive dependency propagation across a multi-element chain (A <- B <- C):
    /// elements power up sequentially from root to leaf and step down in reverse order when the
    /// lease is dropped.
    #[fuchsia::test]
    fn test_transitive() -> Result<()> {
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;

        // Create a four element topology with the following dependencies:
        // C depends on B, which in turn depends on A.
        // D has no dependencies or dependents.
        // A <- B <- C   D
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;
        let on = BinaryPowerLevel::On.into_primitive();
        let off = BinaryPowerLevel::Off.into_primitive();
        let (mut element_a, mut element_b, mut element_c, mut element_d) = executor
            .run_singlethreaded(async {
                let a = TestElement::builder(&topology, "A", &BINARY_POWER_LEVELS).build().await;
                let b = TestElement::builder(&topology, "B", &BINARY_POWER_LEVELS)
                    .dep(on, &a, on)
                    .build()
                    .await;
                let c = TestElement::builder(&topology, "C", &BINARY_POWER_LEVELS)
                    .dep(on, &b, on)
                    .build()
                    .await;
                let d = TestElement::builder(&topology, "D", &BINARY_POWER_LEVELS).build().await;
                (a, b, c, d)
            });

        // Acquire lease for C.
        // A's required level should become ON.
        // B's required level should remain OFF because A is not yet ON.
        // C's required level should remain OFF because B is not yet ON.
        // D's required level should remain OFF.
        let lease = executor.run_singlethreaded(element_c.lease(on));
        let element_a_current = executor.run_singlethreaded(element_a.expect_set_level(on));
        element_b.assert_no_set_level(&mut executor);
        element_c.assert_no_set_level(&mut executor);
        element_d.assert_no_set_level(&mut executor);
        assert_eq!(executor.run_singlethreaded(element_c.current_power_level()), off);
        lease.assert_pending();

        // Update A's current level to ON.
        // A's required level should remain ON.
        // B's required level should become ON because A is now ON.
        // C's required level should remain OFF because B is not yet ON.
        // D's required level should remain OFF.
        executor.run_singlethreaded(element_a.complete_set_level(element_a_current, on));
        element_a.assert_no_set_level(&mut executor);
        let element_b_current = executor.run_singlethreaded(element_b.expect_set_level(on));
        element_c.assert_no_set_level(&mut executor);
        element_d.assert_no_set_level(&mut executor);

        // Update B's current level to ON.
        // A's required level should remain ON.
        // B's required level should remain ON.
        // C's required level should become ON because B is now ON.
        // D's required level should remain OFF.
        executor.run_singlethreaded(element_b.complete_set_level(element_b_current, on));
        element_a.assert_no_set_level(&mut executor);
        element_b.assert_no_set_level(&mut executor);
        let element_c_current = executor.run_singlethreaded(element_c.expect_set_level(on));
        element_d.assert_no_set_level(&mut executor);

        // Update C's current level to ON.
        // A's required level should remain ON.
        // B's required level should remain ON.
        // C's required level should remain ON.
        // D's required level should remain OFF.
        // Lease should become satisfied.
        executor.run_singlethreaded(element_c.complete_set_level(element_c_current, on));
        element_a.assert_no_set_level(&mut executor);
        element_b.assert_no_set_level(&mut executor);
        element_c.assert_no_set_level(&mut executor);
        element_d.assert_no_set_level(&mut executor);
        executor.run_singlethreaded(lease.await_satisfied());

        // Drop lease for C.
        // A's required level should remain ON.
        // B's required level should remain ON.
        // C's required level should become OFF because the lease was dropped.
        // D's required level should remain OFF.
        drop(lease);
        let element_c_current = executor.run_singlethreaded(element_c.expect_set_level(off));
        element_a.assert_no_set_level(&mut executor);
        element_b.assert_no_set_level(&mut executor);
        element_d.assert_no_set_level(&mut executor);

        // Lower C's current level to OFF.
        // A's required level should remain ON.
        // B's required level should become OFF.
        // C's required level should remain OFF.
        // D's required level should remain OFF.
        executor.run_singlethreaded(element_c.complete_set_level(element_c_current, off));
        element_a.assert_no_set_level(&mut executor);
        let element_b_current = executor.run_singlethreaded(element_b.expect_set_level(off));
        element_c.assert_no_set_level(&mut executor);
        element_d.assert_no_set_level(&mut executor);

        // Lower B's current level to OFF.
        // A's required level should become OFF because B is no longer dependent.
        // B's required level should remain OFF.
        // C's required level should remain OFF.
        // D's required level should remain OFF.
        executor.run_singlethreaded(element_b.complete_set_level(element_b_current, off));
        executor.run_singlethreaded(element_a.accept_set_level(off));
        element_b.assert_no_set_level(&mut executor);
        element_c.assert_no_set_level(&mut executor);
        element_d.assert_no_set_level(&mut executor);

        Ok(())
    }

    /// Verifies a shared parent/grandparent dependency hierarchy with multiple child elements
    /// requiring different levels, ensuring intermediate and root elements scale to the maximum
    /// required level and step down appropriately as leases are dropped.
    #[fuchsia::test]
    fn test_shared() -> Result<()> {
        // Create a topology of two child elements (C1 & C2) with a shared
        // parent (P) and grandparent (GP)
        // C1 \
        //     > P -> GP
        // C2 /
        // Child 1 requires Parent at 50 to support its own level of 5.
        // Parent requires Grandparent at 200 to support its own level of 50.
        // C1 -> P -> GP
        //  5 -> 50 -> 200
        // Child 2 requires Parent at 30 to support its own level of 3.
        // Parent requires Grandparent at 90 to support its own level of 30.
        // C2 -> P -> GP
        //  3 -> 30 -> 90
        // Grandparent has a default minimum level of 10.
        // All other elements have a default of 0.
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;
        let (mut grandparent, mut parent, mut child1, mut child2) =
            executor.run_singlethreaded(async {
                let gp = TestElement::builder(&topology, "GP", &[10, 90, 200]).build().await;
                let p = TestElement::builder(&topology, "P", &[0, 30, 50])
                    .dep(50, &gp, 200)
                    .dep(30, &gp, 90)
                    .build()
                    .await;
                let c1 =
                    TestElement::builder(&topology, "C1", &[0, 5]).dep(5, &p, 50).build().await;
                let c2 =
                    TestElement::builder(&topology, "C2", &[0, 3]).dep(3, &p, 30).build().await;
                (gp, p, c1, c2)
            });

        // Acquire lease for C1 @ 5.
        // GP's required level should become 200 because C1 @ 5 has a
        // dependency on P @ 50 and P @ 50 has a dependency on GP @ 200.
        // GP @ 200 has no dependencies so its level should be raised first.
        // P's required level should remain 0 because GP is not yet at 200.
        // C1's required level should remain 0 because P is not yet at 50.
        // C2's required level should remain 0.
        let lease_child_1 = executor.run_singlethreaded(child1.lease(5));
        let grandparent_current = executor.run_singlethreaded(grandparent.expect_set_level(200));
        parent.assert_no_set_level(&mut executor);
        child1.assert_no_set_level(&mut executor);
        child2.assert_no_set_level(&mut executor);
        assert_eq!(executor.run_singlethreaded(child1.current_power_level()), 0);
        lease_child_1.assert_pending();

        // Raise GP's current level to 200.
        // GP's required level should remain 200.
        // P's required level should become 50 because GP is now at 200.
        // C1's required level should remain 0 because P is not yet at 50.
        // C2's required level should remain 0.
        executor.run_singlethreaded(grandparent.complete_set_level(grandparent_current, 200));
        let parent_current = executor.run_singlethreaded(parent.expect_set_level(50));
        child1.assert_no_set_level(&mut executor);
        child2.assert_no_set_level(&mut executor);
        assert_eq!(executor.run_singlethreaded(child1.current_power_level()), 0);
        lease_child_1.assert_pending();

        // Update P's current level to 50.
        // GP's required level should remain 200.
        // P's required level should remain 50.
        // C1's required level should become 5 because P is now at 50.
        // C2's required level should remain 0.
        executor.run_singlethreaded(parent.complete_set_level(parent_current, 50));
        let child1_current = executor.run_singlethreaded(child1.expect_set_level(5));
        child2.assert_no_set_level(&mut executor);

        // Update C1's current level to 5.
        // GP's required level should remain 200.
        // P's required level should remain 50.
        // C1's required level should remain 5.
        // C2's required level should remain 0.
        // C1's lease @ 5 is now satisfied.
        executor.run_singlethreaded(child1.complete_set_level(child1_current, 5));
        grandparent.assert_no_set_level(&mut executor);
        parent.assert_no_set_level(&mut executor);
        child1.assert_no_set_level(&mut executor);
        child2.assert_no_set_level(&mut executor);
        executor.run_singlethreaded(lease_child_1.await_satisfied());

        // Acquire lease for C2 @ 3.
        // Though C2 @ 3 has nominal requirements of P @ 30 and GP @ 90,
        // they are superseded by C1's requirements of 50 and 200.
        // GP's required level should remain 200.
        // P's required level should remain 50.
        // C1's required level should remain 5.
        // C2's required level should become 3 because its dependencies are already satisfied.
        // C1's lease @ 5 is still satisfied.
        let lease_child_2 = executor.run_singlethreaded(child2.lease(3));
        grandparent.assert_no_set_level(&mut executor);
        parent.assert_no_set_level(&mut executor);
        child1.assert_no_set_level(&mut executor);
        let child2_current = executor.run_singlethreaded(child2.expect_set_level(3));
        assert_eq!(executor.run_singlethreaded(child1.current_power_level()), 5);
        lease_child_1.assert_satisfied();
        lease_child_2.assert_pending();

        // Update C2's current level to 3.
        // GP's required level should remain 200.
        // P's required level should remain 50.
        // C1's required level should remain 5.
        // C2's required level should remain 0.
        // C1's lease @ 5 is still satisfied.
        // C2's lease @ 3 is now satisfied.
        executor.run_singlethreaded(child2.complete_set_level(child2_current, 3));
        grandparent.assert_no_set_level(&mut executor);
        parent.assert_no_set_level(&mut executor);
        child1.assert_no_set_level(&mut executor);
        child2.assert_no_set_level(&mut executor);
        executor.run_singlethreaded(lease_child_2.await_satisfied());
        assert_eq!(executor.run_singlethreaded(child1.current_power_level()), 5);
        lease_child_1.assert_satisfied();
        // Drop lease for C1.
        // GP's required level should remain 200.
        // P's required level should remain 50.
        // C1's required level should become 0 because its lease has been dropped.
        // C2's required level should remain 3.
        // C2's lease @ 3 is still satisfied.
        drop(lease_child_1);
        let child1_current = executor.run_singlethreaded(child1.expect_set_level(0));
        grandparent.assert_no_set_level(&mut executor);
        parent.assert_no_set_level(&mut executor);
        child2.assert_no_set_level(&mut executor);
        assert_eq!(executor.run_singlethreaded(child2.current_power_level()), 3);
        lease_child_2.assert_satisfied();

        // Lower C1's current level to 0.
        // GP's required level should remain 200.
        // P's required level should become 30.
        // C1's required level should remain 0.
        // C2's required level should remain 3.
        // C2's lease @ 3 is still satisfied.
        let parent_current = executor.run_singlethreaded(async {
            child1.complete_set_level(child1_current, 0).await;
            parent.expect_set_level(30).await
        });
        grandparent.assert_no_set_level(&mut executor);
        child1.assert_no_set_level(&mut executor);
        child2.assert_no_set_level(&mut executor);
        assert_eq!(executor.run_singlethreaded(child2.current_power_level()), 3);
        lease_child_2.assert_satisfied();

        // Lower Parent's current level to 30.
        // GP's required level should become 90 because P has dropped to 30.
        // P's required level should remain 30.
        // C1's required level should remain 0.
        // C2's required level should remain 3.
        // C2's lease @ 3 is still satisfied.
        let grandparent_current = executor.run_singlethreaded(async {
            parent.complete_set_level(parent_current, 30).await;
            grandparent.expect_set_level(90).await
        });
        parent.assert_no_set_level(&mut executor);
        child1.assert_no_set_level(&mut executor);
        child2.assert_no_set_level(&mut executor);
        assert_eq!(executor.run_singlethreaded(child2.current_power_level()), 3);
        lease_child_2.assert_satisfied();

        // Drop lease for Child 2.
        // GP's required level should remain 90 because P is still at 30.
        // P's required level should remain 30.
        // C1's required level should remain 0.
        // C2's required level should become 0 because its lease has been dropped.
        drop(lease_child_2);
        let child2_current = executor.run_singlethreaded(child2.expect_set_level(0));
        grandparent.assert_no_set_level(&mut executor);
        parent.assert_no_set_level(&mut executor);
        child1.assert_no_set_level(&mut executor);

        // Lower GP's current level to 90.
        // GP's required level should remain 90 because P is still at 30.
        // P's required level should remain 30.
        // C1's required level should remain 0.
        // C2's required level should remain 0.
        executor.run_singlethreaded(grandparent.complete_set_level(grandparent_current, 90));
        grandparent.assert_no_set_level(&mut executor);
        parent.assert_no_set_level(&mut executor);
        child1.assert_no_set_level(&mut executor);
        child2.assert_no_set_level(&mut executor);

        // Lower C2's current level to 0.
        // GP's required level should remain 90 because P is still at 30.
        // P's required level should become 0.
        // C1's required level should remain 0.
        // C2's required level should remain 0.
        let parent_current = executor.run_singlethreaded(async {
            child2.complete_set_level(child2_current, 0).await;
            parent.expect_set_level(0).await
        });
        grandparent.assert_no_set_level(&mut executor);
        child1.assert_no_set_level(&mut executor);
        child2.assert_no_set_level(&mut executor);

        // Lower Parent's current level to 0.
        // GP's required level should become its minimum level of 10 because P is now at 0.
        // P's required level should remain 0.
        // C1's required level should remain 0.
        // C2's required level should remain 0.
        let grandparent_current = executor.run_singlethreaded(async {
            parent.complete_set_level(parent_current, 0).await;
            grandparent.expect_set_level(10).await
        });
        parent.assert_no_set_level(&mut executor);
        child1.assert_no_set_level(&mut executor);
        child2.assert_no_set_level(&mut executor);

        // Lower GP's current level to 10.
        // GP's required level should remain 10.
        // P's required level should remain 0.
        // C1's required level should remain 0.
        // C2's required level should remain 0.
        executor.run_singlethreaded(grandparent.complete_set_level(grandparent_current, 10));
        grandparent.assert_no_set_level(&mut executor);
        parent.assert_no_set_level(&mut executor);
        child1.assert_no_set_level(&mut executor);
        child2.assert_no_set_level(&mut executor);

        Ok(())
    }

    /// Verifies validation and error handling when adding elements, including unauthorized
    /// dependency tokens and invalid required power levels.
    #[fuchsia::test]
    async fn test_add_element_errors() -> Result<(), Error> {
        let realm = build_power_broker_realm().await?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        // Create a root element
        let earth =
            TestElement::builder(&topology, "Earth", &BINARY_POWER_LEVELS).build_unsettled().await;

        // Using an unauthorized dependency token yields AddElementError::NotAuthorized.
        let (element_runner_client, _) = create_endpoints::<ElementRunnerMarker>();
        assert_matches!(
            topology
                .add_element(ElementSchema {
                    element_name: Some("Water".into()),
                    initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                    valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                    dependencies: Some(vec![LevelDependency {
                        dependent_level: Some(BinaryPowerLevel::On.into_primitive()),
                        requires_token: Some(zx::Event::create()),
                        requires_level_by_preference: Some(vec![
                            BinaryPowerLevel::On.into_primitive()
                        ]),
                        ..Default::default()
                    }]),
                    element_runner: Some(element_runner_client),
                    ..Default::default()
                })
                .await,
            Ok(Err(fpb::AddElementError::NotAuthorized))
        );

        // Using an invalid level in a dependency yields AddElementError::Invalid.
        let (element_runner_client, _) = create_endpoints::<ElementRunnerMarker>();
        assert_matches!(
            topology
                .add_element(ElementSchema {
                    element_name: Some("Air".into()),
                    initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                    valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                    dependencies: Some(vec![LevelDependency {
                        dependent_level: Some(BinaryPowerLevel::On.into_primitive()),
                        requires_token: Some(earth.dep_token()),
                        requires_level_by_preference: Some(vec![2]),
                        ..Default::default()
                    }]),
                    element_runner: Some(element_runner_client),
                    ..Default::default()
                })
                .await,
            Ok(Err(fpb::AddElementError::Invalid))
        );

        // Using the correct dependency token succeeds.
        let (element_runner_client, _) = create_endpoints::<ElementRunnerMarker>();
        assert!(
            topology
                .add_element(ElementSchema {
                    element_name: Some("Fire".into()),
                    initial_current_level: Some(BinaryPowerLevel::Off.into_primitive()),
                    valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                    dependencies: Some(vec![LevelDependency {
                        dependent_level: Some(BinaryPowerLevel::On.into_primitive()),
                        requires_token: Some(earth.dep_token()),
                        requires_level_by_preference: Some(vec![
                            BinaryPowerLevel::On.into_primitive()
                        ]),
                        ..Default::default()
                    }]),
                    element_runner: Some(element_runner_client),
                    ..Default::default()
                })
                .await
                .is_ok()
        );

        Ok(())
    }

    /// When `Topology.Lease` is given an invalid `LeaseSchema`, it should report the specific
    /// error for the field at fault rather than a generic failure. Covers every field the
    /// request is rejected on.
    #[fuchsia::test]
    async fn test_lease_errors() -> Result<(), Error> {
        let realm = build_power_broker_realm().await?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        // Create a root element to depend on.
        let earth =
            TestElement::builder(&topology, "Earth", &BINARY_POWER_LEVELS).build_unsettled().await;

        let earth_dependency = || fpb::LeaseDependency {
            requires_token: Some(earth.dep_token()),
            requires_level: Some(BinaryPowerLevel::On.into_primitive()),
            ..Default::default()
        };

        // Omitting lease_token yields LeaseError::INVALID_ARGUMENT.
        assert_matches!(
            topology
                .lease(fpb::LeaseSchema {
                    lease_name: Some("no_token".into()),
                    dependencies: Some(vec![earth_dependency()]),
                    ..Default::default()
                })
                .await,
            Ok(Err(fpb::LeaseError::InvalidArgument))
        );

        // Omitting lease_name yields LeaseError::INVALID_ARGUMENT.
        let (_lease_token_client, lease_token_server) = zx::EventPair::create();
        assert_matches!(
            topology
                .lease(fpb::LeaseSchema {
                    lease_token: Some(lease_token_server),
                    dependencies: Some(vec![earth_dependency()]),
                    ..Default::default()
                })
                .await,
            Ok(Err(fpb::LeaseError::InvalidArgument))
        );

        // Omitting dependencies yields LeaseError::INVALID_ARGUMENT.
        let (_lease_token_client, lease_token_server) = zx::EventPair::create();
        assert_matches!(
            topology
                .lease(fpb::LeaseSchema {
                    lease_token: Some(lease_token_server),
                    lease_name: Some("no_dependencies".into()),
                    ..Default::default()
                })
                .await,
            Ok(Err(fpb::LeaseError::InvalidArgument))
        );

        // An empty dependencies vector also yields LeaseError::INVALID_ARGUMENT.
        let (_lease_token_client, lease_token_server) = zx::EventPair::create();
        assert_matches!(
            topology
                .lease(fpb::LeaseSchema {
                    lease_token: Some(lease_token_server),
                    lease_name: Some("empty_dependencies".into()),
                    dependencies: Some(vec![]),
                    ..Default::default()
                })
                .await,
            Ok(Err(fpb::LeaseError::InvalidArgument))
        );

        // A dependency that specifies neither requires_level nor
        // requires_level_by_preference yields LeaseError::INVALID_LEVEL.
        let (_lease_token_client, lease_token_server) = zx::EventPair::create();
        assert_matches!(
            topology
                .lease(fpb::LeaseSchema {
                    lease_token: Some(lease_token_server),
                    lease_name: Some("no_level".into()),
                    dependencies: Some(vec![fpb::LeaseDependency {
                        requires_token: Some(earth.dep_token()),
                        ..Default::default()
                    }]),
                    ..Default::default()
                })
                .await,
            Ok(Err(fpb::LeaseError::InvalidLevel))
        );

        // A fully specified lease is accepted.
        let (_lease_token_client, lease_token_server) = zx::EventPair::create();
        topology
            .lease(fpb::LeaseSchema {
                lease_token: Some(lease_token_server),
                lease_name: Some("valid".into()),
                dependencies: Some(vec![earth_dependency()]),
                should_return_pending_lease: Some(true),
                ..Default::default()
            })
            .await?
            .expect("lease failed");

        Ok(())
    }

    /// Verifies that dropping an element's `ElementControl` channel removes the element and closes
    /// any associated `Status` channels.
    #[fuchsia::test]
    async fn test_closing_element_control_closes_status() -> Result<(), Error> {
        let realm = build_power_broker_realm().await?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        let fire =
            TestElement::builder(&topology, "Fire", &BINARY_POWER_LEVELS).build_unsettled().await;

        // Confirm the element has been removed and Status channels have been
        // closed.
        drop(fire.control);
        fire.status.as_channel().on_closed().await?;

        Ok(())
    }

    /// Verifies that `Status.WatchPowerLevel` accurately observes power level changes as leases
    /// are acquired and dropped.
    #[fuchsia::test]
    fn test_status_watch_power_level() -> Result<(), Error> {
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;

        // Create a topology with only one element:
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;
        let mut element = executor.run_singlethreaded(async {
            TestElement::builder(&topology, "E", &[0, 1, 2]).build().await
        });

        executor.run_singlethreaded(async {
            // Acquire a lease to change the required level to 1.
            let lease1 = element.lease(1).await;
            element.accept_set_level(1).await;
            lease1.await_satisfied().await;

            // Acquire another lease to change the required level to 2.
            let lease2 = element.lease(2).await;
            element.expect_set_level(2).await.send().unwrap();
            // DON'T call watch_power_level here--we want to make sure it skips to the most recent
            lease2.await_satisfied().await;
            // Drop lease2, level should go back to 1.
            drop(lease2);
            element.expect_set_level(1).await.send().unwrap();
            // Sleep here because otherwise we might call this before PB has updated from 2 to 1
            thread::sleep(Duration::from_millis(100));
            assert_eq!(element.status.watch_power_level().await.unwrap(), Ok(1));

            // Drop lease1, level should go back to 0.
            drop(lease1);
            element.accept_set_level(0).await;
        });

        // Ensure there are no more current levels in the queue.
        assert!(executor.run_until_stalled(&mut element.status.watch_power_level()).is_pending());

        Ok(())
    }

    /// Verifies that an element added with an `initial_lease_token` acquires a pending initial
    /// lease that becomes satisfied once its dependencies and own power level are satisfied.
    #[fuchsia::test]
    async fn test_add_element_initial_lease() -> Result<()> {
        let realm = build_power_broker_realm().await?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        // Create a topology with only two elements and a single dependency:
        // A -> B
        let mut element_b =
            TestElement::builder(&topology, "B", &BINARY_POWER_LEVELS).build().await;

        // Setup Dependent "A" with initial_lease_token
        let (element_a_runner_client, element_a_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let (_element_a_control, element_a_control_server) = create_proxy::<ElementControlMarker>();

        let (lease_token_client, lease_token_server) = zx::EventPair::create();

        // We run AddElement for A. B is currently OFF, so it will need to be turned ON.
        // AddElement should return immediately with the lease granted (but not yet satisfied).
        topology
            .add_element(ElementSchema {
                element_name: Some("A".into()),
                initial_current_level: Some(BinaryPowerLevel::On.into_primitive()), // Initialized to ON
                valid_levels: Some(BINARY_POWER_LEVELS.to_vec()),
                dependencies: Some(vec![LevelDependency {
                    dependent_level: Some(BinaryPowerLevel::On.into_primitive()),
                    requires_token: Some(element_b.dep_token()),
                    requires_level_by_preference: Some(vec![BinaryPowerLevel::On.into_primitive()]),
                    ..Default::default()
                }]),
                element_control: Some(element_a_control_server),
                element_runner: Some(element_a_runner_client),
                initial_lease_token: Some(lease_token_server), // Request Lease at Initial Level (ON)
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap();

        // B should receive SetLevel(ON).
        let on = BinaryPowerLevel::On.into_primitive();
        let element_b_current = element_b.expect_set_level(on).await;

        // Lease is not yet satisfied, because B is not ON.
        assert_matches!(
            lease_token_client.wait_one(
                zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED),
                zx::MonotonicInstant::INFINITE_PAST,
            ),
            zx::WaitResult::TimedOut(_)
        );

        // Turn B ON.
        element_b.complete_set_level(element_b_current, on).await;

        // Lease is still not satisfied, because A is not ON.
        assert_matches!(
            lease_token_client.wait_one(
                zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED),
                zx::MonotonicInstant::INFINITE_PAST,
            ),
            zx::WaitResult::TimedOut(_)
        );

        // A should receive SetLevel(ON).
        let mut stream = element_a_runner_server.into_stream();
        match stream.try_next().await.expect("ElementRunner FIDL failed") {
            Some(fpb::ElementRunnerRequest::SetLevel { level, responder }) => {
                assert_eq!(level, on, "First SetLevel must be ON");
                responder.send().expect("responder failed");
            }
            other => panic!("expected SetLevel({on}), got {other:?}"),
        }

        // The lease should now be SATISFIED.
        fasync::OnSignals::new(
            &lease_token_client,
            zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED),
        )
        .await
        .unwrap();

        Ok(())
    }

    /// Verifies that `initial_lease_token` suppression compares `IndexedPowerLevel` order rather
    /// than raw `u8` values when an element's `valid_levels` are non-monotonic (`[10, 5, 2]`).
    #[fuchsia::test]
    async fn test_add_element_initial_lease_non_monotonic_levels() -> Result<()> {
        let realm = build_power_broker_realm().await?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        let on = BinaryPowerLevel::On.into_primitive();
        let off = BinaryPowerLevel::Off.into_primitive();
        let mut element_b =
            TestElement::builder(&topology, "B", &BINARY_POWER_LEVELS).build().await;

        // Element A has non-monotonic valid_levels [10, 5, 2]:
        //   index 0 -> 10 (minimum)
        //   index 1 ->  5 (initial_current_level / initial lease target)
        //   index 2 ->  2 (maximum)
        let (element_a_runner_client, element_a_runner_server) =
            create_endpoints::<ElementRunnerMarker>();
        let element_a_runner = element_a_runner_server.into_stream();
        let (element_a_control, element_a_control_server) = create_proxy::<ElementControlMarker>();
        let (element_a_status, element_a_status_server) = create_proxy::<StatusMarker>();
        let element_a_token = zx::Event::create();
        let (lease_token_client, lease_token_server) = zx::EventPair::create();

        topology
            .add_element(ElementSchema {
                element_name: Some("A".into()),
                initial_current_level: Some(5),
                valid_levels: Some(vec![10, 5, 2]),
                dependencies: Some(vec![LevelDependency {
                    dependent_level: Some(5),
                    requires_token: Some(element_b.dep_token()),
                    requires_level_by_preference: Some(vec![on]),
                    ..Default::default()
                }]),
                element_control: Some(element_a_control_server),
                element_runner: Some(element_a_runner_client),
                initial_lease_token: Some(lease_token_server),
                ..Default::default()
            })
            .await
            .expect("AddElement A failed")
            .expect("AddElement A error");

        element_a_control
            .register_dependency_token(
                element_a_token.duplicate_handle(zx::Rights::SAME_RIGHTS).expect("dup failed"),
            )
            .await
            .expect("register_dependency_token A failed")
            .expect("register_dependency_token A error");

        element_a_control
            .open_status_channel(element_a_status_server)
            .expect("open_status_channel A failed");

        let mut element_a = TestElement {
            name: "A".into(),
            topology: topology.clone(),
            control: element_a_control,
            status: element_a_status,
            runner: element_a_runner,
            token: element_a_token,
        };

        // B must be requested ON while A's initial minimum level 10 (index 0 < index 1) is
        // suppressed on element_a.runner despite 10 > 5 in raw u8 value.
        let element_b_on_responder = element_b.expect_set_level(on).await;

        // Race element_a.runner against Status.WatchPowerLevel so that if SetLevel(10) is wrongly
        // dispatched instead of suppressed, the test panics immediately rather than hanging.
        loop {
            let mut runner_fut = std::pin::pin!(element_a.runner.try_next());
            let mut status_fut = std::pin::pin!(element_a.status.watch_power_level());
            match futures_util::future::select(runner_fut.as_mut(), status_fut.as_mut()).await {
                futures_util::future::Either::Left((req, _)) => {
                    panic!("unexpected SetLevel on A while waiting for initial lease: {req:?}");
                }
                futures_util::future::Either::Right((res, _)) => {
                    let observed = res.expect("watch A fidl").expect("watch A");
                    if observed == 10 {
                        break;
                    }
                    assert_eq!(observed, 5);
                }
            }
        }

        // Lease A at 2 (index 2 > wait index 1, even though 2 < 5 as raw u8) before turning B ON
        // so that once B ON is satisfied, A's first dispatched SetLevel must be 2 (verifying the
        // higher-index/smaller-u8 direction end-to-end as well).
        let (high_lease_client, high_lease_server) = zx::EventPair::create();
        topology
            .lease(fpb::LeaseSchema {
                lease_token: Some(high_lease_server),
                lease_name: Some("high_lease_a_2".into()),
                dependencies: Some(vec![fpb::LeaseDependency {
                    requires_token: Some(element_a.dep_token()),
                    requires_level_by_preference: Some(vec![2]),
                    ..Default::default()
                }]),
                should_return_pending_lease: Some(true),
                ..Default::default()
            })
            .await
            .expect("lease A(2) fidl")
            .expect("lease A(2) error");

        element_b.complete_set_level(element_b_on_responder, on).await;
        element_a.accept_set_level(2).await;

        fasync::OnSignals::new(
            &lease_token_client,
            zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED),
        )
        .await
        .expect("initial lease satisfied");

        // Dropping the high lease steps A down to initial lease level 5 (index 1).
        drop(high_lease_client);
        element_a.accept_set_level(5).await;

        // Dropping the initial lease steps A down to its minimum level 10 (index 0) and then B OFF.
        drop(lease_token_client);
        element_a.accept_set_level(10).await;
        element_b.accept_set_level(off).await;

        Ok(())
    }

    /// Verifies `remove_with_required_element` dependency behavior: when a parent element is
    /// removed, removable dependent leases remain satisfied while unremovable dependent leases
    /// transition to pending.
    #[fuchsia::test]
    fn test_dependency_removal() -> Result<()> {
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;

        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        let on = BinaryPowerLevel::On.into_primitive();
        let (mut parent, mut child_removable, mut child_unremovable, mut child_default_unremovable) =
            executor.run_singlethreaded(async {
                let parent =
                    TestElement::builder(&topology, "P", &BINARY_POWER_LEVELS).build().await;
                let child_removable =
                    TestElement::builder(&topology, "child_removable", &BINARY_POWER_LEVELS)
                        .dep_with_removable(on, &parent, on, Some(true))
                        .build()
                        .await;
                let child_unremovable =
                    TestElement::builder(&topology, "child_unremovable", &BINARY_POWER_LEVELS)
                        .dep_with_removable(on, &parent, on, Some(false))
                        .build()
                        .await;
                let child_default_unremovable = TestElement::builder(
                    &topology,
                    "child_default_unremovable",
                    &BINARY_POWER_LEVELS,
                )
                .dep_with_removable(on, &parent, on, None)
                .build()
                .await;
                (parent, child_removable, child_unremovable, child_default_unremovable)
            });

        // Lease children. Leases should be pending because P is OFF.
        let (lease_removable, lease_unremovable, lease_default_unremovable) = executor
            .run_singlethreaded(async {
                let removable = child_removable.lease(on).await;
                let unremovable = child_unremovable.lease(on).await;
                let default_unremovable = child_default_unremovable.lease(on).await;
                removable.assert_pending();
                unremovable.assert_pending();
                default_unremovable.assert_pending();
                (removable, unremovable, default_unremovable)
            });

        // P should receive SetLevel(ON), while children remain stalled until P turns ON.
        let parent_current = executor.run_singlethreaded(parent.expect_set_level(on));
        child_removable.assert_no_set_level(&mut executor);
        child_unremovable.assert_no_set_level(&mut executor);
        child_default_unremovable.assert_no_set_level(&mut executor);

        // Transition P and children to ON; leases should become Satisfied.
        executor.run_singlethreaded(async {
            parent.complete_set_level(parent_current, on).await;
            child_removable.accept_set_level(on).await;
            child_unremovable.accept_set_level(on).await;
            child_default_unremovable.accept_set_level(on).await;
            lease_removable.await_satisfied().await;
            lease_unremovable.await_satisfied().await;
            lease_default_unremovable.await_satisfied().await;
        });

        // Drop parent element control to remove P.
        drop(parent.control);

        // Verify parent status channel is closed.
        executor.run_singlethreaded(async {
            parent.status.as_channel().on_closed().await.unwrap();
        });

        // Since child_removable has removable: true, its lease stays Satisfied.
        // child_unremovable (removable: false) and child_default_unremovable (removable: default/false) should transition to Pending.
        executor.run_singlethreaded(async {
            lease_unremovable.await_pending().await;
            lease_default_unremovable.await_pending().await;
            assert_eq!(child_removable.current_power_level().await, on);
            lease_removable.assert_satisfied();
        });

        Ok(())
    }

    /// Verifies that higher levels implicitly requiring multiple lower-level dependencies
    /// correctly activate all of them.
    #[fuchsia::test]
    fn test_cumulative_implicit_dependencies() -> Result<()> {
        let mut executor = fasync::TestExecutor::new();
        let realm = executor.run_singlethreaded(async { build_power_broker_realm().await })?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        // Element B: valid levels [0, 1]
        // Element C: valid levels [0, 1]
        // Element A: valid levels [0, 1, 2], A(1) -> B(1), A(2) -> C(1)
        let (mut b, mut c, mut a) = executor.run_singlethreaded(async {
            let b = TestElement::builder(&topology, "B", &[0, 1]).build().await;
            let c = TestElement::builder(&topology, "C", &[0, 1]).build().await;
            let a = TestElement::builder(&topology, "A", &[0, 1, 2])
                .dep(1, &b, 1)
                .dep(2, &c, 1)
                .build()
                .await;
            (b, c, a)
        });

        // Acquire lease for A at level 2.
        // A@2 implies A@1, so BOTH B@1 and C@1 must be requested before A changes level.
        let lease_a2 = executor.run_singlethreaded(a.lease(2));
        lease_a2.assert_pending();

        let (b_resp, c_resp) = executor.run_singlethreaded(async {
            (b.expect_set_level(1).await, c.expect_set_level(1).await)
        });
        a.assert_no_set_level(&mut executor);

        // Update B and C to 1, then A should receive SetLevel(2) and satisfy lease_a2.
        executor.run_singlethreaded(async {
            b.complete_set_level(b_resp, 1).await;
            c.complete_set_level(c_resp, 1).await;
            a.accept_set_level(2).await;
            lease_a2.await_satisfied().await;
        });

        // Acquire lease for A at level 1, and drop lease for level 2.
        let lease_a1 = executor.run_singlethreaded(a.lease(1));
        lease_a1.assert_satisfied();
        drop(lease_a2);

        // A steps down to 1 and C steps down to 0, while B must remain at 1.
        executor.run_singlethreaded(async {
            a.accept_set_level(1).await;
            c.accept_set_level(0).await;
            assert_eq!(a.current_power_level().await, 1);
            lease_a1.assert_satisfied();
        });
        b.assert_no_set_level(&mut executor);

        // Finally, drop lease_a1; A and B step down to 0.
        drop(lease_a1);
        executor.run_singlethreaded(async {
            a.accept_set_level(0).await;
            b.accept_set_level(0).await;
        });

        Ok(())
    }

    /// Verifies that dropping an intermediate element in a dependency chain correctly transitions
    /// dependent leases to pending and steps down active elements.
    #[fuchsia::test]
    async fn test_transitive_intermediate_element_removal() -> Result<()> {
        let realm = build_power_broker_realm().await?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        // A <- B <- C
        let mut a = TestElement::builder(&topology, "A", &[0, 1]).build().await;
        let mut b = TestElement::builder(&topology, "B", &[0, 1]).dep(1, &a, 1).build().await;
        let mut c = TestElement::builder(&topology, "C", &[0, 1]).dep(1, &b, 1).build().await;

        // Lease C at 1 and power up A -> B -> C.
        let lease = c.lease(1).await;
        lease.assert_pending();
        a.accept_set_level(1).await;
        b.accept_set_level(1).await;
        c.accept_set_level(1).await;
        lease.await_satisfied().await;

        // Remove intermediate element B by dropping b.control.
        drop(b.control);
        b.status.as_channel().on_closed().await.unwrap();

        // C's lease should become Pending because B is gone, and C and A step down to 0.
        lease.await_pending().await;
        c.accept_set_level(0).await;
        a.accept_set_level(0).await;

        Ok(())
    }

    /// Verifies that a lease requiring multiple elements is only satisfied when all requirements
    /// are met.
    #[fuchsia::test]
    async fn test_direct_multi_element_lease() -> Result<()> {
        let realm = build_power_broker_realm().await?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        let mut a = TestElement::builder(&topology, "A", &[0, 1]).build().await;
        let mut b = TestElement::builder(&topology, "B", &[0, 1]).build().await;

        let (lease_token_client, lease_token_server) = zx::EventPair::create();

        // Acquire direct lease with multiple requirements: A@1 and B@1.
        topology
            .lease(fpb::LeaseSchema {
                lease_token: Some(lease_token_server),
                lease_name: Some("multi_lease".into()),
                dependencies: Some(vec![
                    fpb::LeaseDependency {
                        requires_token: Some(a.dep_token()),
                        requires_level: Some(1),
                        ..Default::default()
                    },
                    fpb::LeaseDependency {
                        requires_token: Some(b.dep_token()),
                        requires_level: Some(1),
                        ..Default::default()
                    },
                ]),
                should_return_pending_lease: Some(true),
                ..Default::default()
            })
            .await
            .unwrap()
            .unwrap();

        // Lease is pending (not yet satisfied).
        assert_matches!(
            lease_token_client.wait_one(
                zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED),
                zx::MonotonicInstant::INFINITE_PAST,
            ),
            zx::WaitResult::TimedOut(_)
        );

        // A receives SetLevel(1). Update A to 1.
        a.accept_set_level(1).await;

        // Lease is still NOT satisfied because B is not yet 1.
        assert_matches!(
            lease_token_client.wait_one(
                zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED),
                zx::MonotonicInstant::INFINITE_PAST,
            ),
            zx::WaitResult::TimedOut(_)
        );

        // B receives SetLevel(1). Update B to 1.
        b.accept_set_level(1).await;

        // Now the lease should be satisfied!
        fasync::OnSignals::new(
            &lease_token_client,
            zx::Signals::from_bits_truncate(fpb::LEASE_SIGNAL_SATISFIED),
        )
        .await
        .unwrap();

        // Close the lease token to drop the lease; both A and B step down to 0.
        drop(lease_token_client);
        a.accept_set_level(0).await;
        b.accept_set_level(0).await;

        Ok(())
    }

    /// Verifies that a shared dependency scales to the maximum required level of its active
    /// dependents and scales down correctly.
    #[fuchsia::test]
    async fn test_shared_element_different_required_levels() -> Result<()> {
        let realm = build_power_broker_realm().await?;
        let topology: TopologyProxy = realm.root.connect_to_protocol_at_exposed_dir()?;

        // Shared element S with levels [0, 1, 2]
        // Child C1 with levels [0, 1], C1(1) requires S(1)
        // Child C2 with levels [0, 1], C2(1) requires S(2)
        let mut s = TestElement::builder(&topology, "S", &[0, 1, 2]).build().await;
        let mut c1 = TestElement::builder(&topology, "C1", &[0, 1]).dep(1, &s, 1).build().await;
        let mut c2 = TestElement::builder(&topology, "C2", &[0, 1]).dep(1, &s, 2).build().await;

        // Lease C1 at 1; S scales to 1 and C1 scales to 1.
        let lease_c1 = c1.lease(1).await;
        lease_c1.assert_pending();
        s.accept_set_level(1).await;
        c1.accept_set_level(1).await;
        lease_c1.await_satisfied().await;

        // Lease C2 at 1; C2(1) requires S(2), so S scales to 2 and C2 scales to 1.
        let lease_c2 = c2.lease(1).await;
        lease_c2.assert_pending();
        s.accept_set_level(2).await;
        c2.accept_set_level(1).await;
        lease_c2.await_satisfied().await;

        // Drop Lease C2; C2 steps down to 0 and S steps down from 2 to 1 (not 0) while C1 remains
        // leased.
        drop(lease_c2);
        c2.accept_set_level(0).await;
        s.accept_set_level(1).await;
        assert_eq!(c1.current_power_level().await, 1);
        lease_c1.assert_satisfied();

        // Drop Lease C1; C1 and S both step down to 0.
        drop(lease_c1);
        c1.accept_set_level(0).await;
        s.accept_set_level(0).await;

        Ok(())
    }

    #[test_case(1, true; "reboot")]
    #[test_case(1, false; "no_reboot")]
    #[test_case(0, false; "disabled")]
    #[fuchsia::test]
    async fn test_set_level_timeout_integration(
        timeout_seconds: u32,
        reboot_enabled: bool,
    ) -> Result<(), Error> {
        let builder = RealmBuilder::new().await?;

        // Add Power Broker component.
        let power_broker =
            builder.add_child("power_broker", "#meta/power-broker.cm", ChildOptions::new()).await?;

        // Override configuration capabilities for testing.
        let mut decl = builder.get_component_decl(&power_broker).await?;
        for cap in &mut decl.capabilities {
            if let cm_rust::CapabilityDecl::Config(config_decl) = cap {
                if config_decl.name.as_str() == "fuchsia.power.broker.SetLevelTimeoutSeconds" {
                    config_decl.value = cm_rust::ConfigValue::Single(
                        cm_rust::ConfigSingleValue::Uint32(timeout_seconds),
                    );
                } else if config_decl.name.as_str()
                    == "fuchsia.power.broker.RebootOnSetLevelTimeout"
                {
                    config_decl.value = cm_rust::ConfigValue::Single(
                        cm_rust::ConfigSingleValue::Bool(reboot_enabled),
                    );
                }
            }
        }
        builder.replace_component_decl(&power_broker, decl).await?;

        // Add mock CrashReporter.
        let (crash_tx, mut rx_crash) = futures::channel::mpsc::unbounded();
        let crash_reporter_mock = builder
            .add_local_child(
                "crash_reporter",
                move |handles: LocalComponentHandles| {
                    let crash_tx = crash_tx.clone();
                    Box::pin(async move {
                        let mut fs = fuchsia_component::server::ServiceFs::new();
                        fs.dir("svc").add_fidl_service(
                            |stream: ffeedback::CrashReporterRequestStream| stream,
                        );
                        fs.serve_connection(handles.outgoing_dir).unwrap();
                        fs.for_each_concurrent(None, move |mut stream| {
                            let crash_tx = crash_tx.clone();
                            async move {
                                while let Ok(Some(request)) = stream.try_next().await {
                                    match request {
                                        ffeedback::CrashReporterRequest::FileReport {
                                            report,
                                            responder,
                                        } => {
                                            log::info!("Mock CrashReporter received report");
                                            let _ = crash_tx.unbounded_send(report);
                                            let _ = responder
                                                .send(Ok(&ffeedback::FileReportResults::default()));
                                        }
                                    }
                                }
                            }
                        })
                        .await;
                        Ok(())
                    })
                },
                ChildOptions::new(),
            )
            .await?;

        // Add mock Admin.
        let (reboot_tx, mut rx_reboot) = futures::channel::mpsc::unbounded();
        let admin_mock = builder
            .add_local_child(
                "admin",
                move |handles: LocalComponentHandles| {
                    let reboot_tx = reboot_tx.clone();
                    Box::pin(async move {
                        let mut fs = fuchsia_component::server::ServiceFs::new();
                        fs.dir("svc")
                            .add_fidl_service(|stream: fstatecontrol::AdminRequestStream| stream);
                        fs.serve_connection(handles.outgoing_dir).unwrap();
                        fs.for_each_concurrent(None, move |mut stream| {
                            let reboot_tx = reboot_tx.clone();
                            async move {
                                while let Ok(Some(request)) = stream.try_next().await {
                                    match request {
                                        fstatecontrol::AdminRequest::Shutdown {
                                            options,
                                            responder,
                                        } => {
                                            log::info!("Mock Admin received shutdown");
                                            let _ = reboot_tx.unbounded_send(options);
                                            let _ = responder.send(Ok(()));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        })
                        .await;
                        Ok(())
                    })
                },
                ChildOptions::new(),
            )
            .await?;

        // Route protocols.
        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<ffeedback::CrashReporterMarker>())
                    .from(&crash_reporter_mock)
                    .to(&power_broker),
            )
            .await?;

        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<fstatecontrol::AdminMarker>())
                    .from(&admin_mock)
                    .to(&power_broker),
            )
            .await?;

        // Route Topology from Power Broker to test.
        builder
            .add_route(
                Route::new()
                    .capability(Capability::protocol::<fpb::TopologyMarker>())
                    .from(&power_broker)
                    .to(Ref::parent()),
            )
            .await?;

        let instance = builder.build().await?;

        let topology: fpb::TopologyProxy = instance.root.connect_to_protocol_at_exposed_dir()?;

        let mut stuck_element =
            TestElement::builder(&topology, "stuck_element", &[0, 1]).build_unsettled().await;

        // Wait for SetLevel request and hold the responder without replying to simulate a stuck
        // transition.
        let _responder = stuck_element.expect_set_level(0).await;
        log::info!("Received SetLevel, ignoring to simulate stuck state.");

        if timeout_seconds > 0 {
            // Wait for crash report.
            let report = rx_crash
                .next()
                .on_timeout(
                    fasync::MonotonicDuration::from_seconds(CRASH_REPORT_TIMEOUT_SECONDS),
                    || None,
                )
                .await
                .ok_or_else(|| anyhow::anyhow!("Did not receive crash report"))?;
            assert_eq!(report.program_name, Some("power-broker".to_string()));
            assert_eq!(
                report.crash_signature,
                Some("fuchsia-power-broker-set-level-stuck-stuck_element".to_string())
            );
            assert_eq!(report.is_fatal, Some(false));

            if reboot_enabled {
                let options = rx_reboot
                    .next()
                    .on_timeout(
                        fasync::MonotonicDuration::from_seconds(REBOOT_TIMEOUT_SECONDS),
                        || None,
                    )
                    .await
                    .ok_or_else(|| anyhow::anyhow!("Did not receive reboot request"))?;
                assert_eq!(options.action, Some(fstatecontrol::ShutdownAction::Reboot));
                assert_eq!(
                    options.reasons,
                    Some(vec![fstatecontrol::ShutdownReason::PowerLevelTransitionStuck])
                );
            } else {
                // Verify that NO reboot request was received.
                fasync::Timer::new(fasync::MonotonicInstant::after(
                    fasync::MonotonicDuration::from_millis(500),
                ))
                .await;
                assert!(rx_reboot.next().now_or_never().is_none());
            }
        } else {
            // Since timeout is disabled (0), wait to confirm no crash report or reboot request
            // is dispatched (unlike the 1s timeout tests).
            fasync::Timer::new(fasync::MonotonicInstant::after(
                fasync::MonotonicDuration::from_seconds(2),
            ))
            .await;
            assert!(rx_crash.next().now_or_never().is_none());
            assert!(rx_reboot.next().now_or_never().is_none());

            // Verify Power Broker is still responsive.
            let (second_runner_client, _second_runner_server) =
                create_endpoints::<fpb::ElementRunnerMarker>();
            let add_res = topology
                .add_element(fpb::ElementSchema {
                    element_name: Some("second_element".into()),
                    initial_current_level: Some(0),
                    valid_levels: Some(vec![0, 1]),
                    element_runner: Some(second_runner_client),
                    ..Default::default()
                })
                .await?;
            assert!(add_res.is_ok());
        }

        Ok(())
    }

    #[fuchsia::test]
    async fn test_drop_lease_non_monotonic_dependencies() -> Result<()> {
        async fn spawn_immediate_runner(
            mut runner: ElementRunnerRequestStream,
            status: &StatusProxy,
            name: &str,
        ) -> Result<(mpsc::Receiver<u8>, fasync::Task<()>)> {
            let (level_tx, level_rx) = mpsc::channel();
            let runner_task = fasync::Task::spawn(async move {
                while let Ok(Some(request)) = runner.try_next().await {
                    if let ElementRunnerRequest::SetLevel { level, responder } = request {
                        let _ = level_tx.send(level);
                        let _ = responder.send();
                    }
                }
            });
            let initial_level = status
                .watch_power_level()
                .await
                .with_context(|| format!("initial WatchPowerLevel({name}) FIDL failed"))?
                .map_err(|err| anyhow!("initial WatchPowerLevel({name}) failed: {err:?}"))?;
            ensure!(initial_level == 0, "{name} initial level = {initial_level}, want 0");

            Ok((level_rx, runner_task))
        }

        async fn acquire_lease(
            topology: &TopologyProxy,
            token: &zx::Event,
            level: u8,
            name: &str,
        ) -> Result<zx::EventPair> {
            let (lease_client, lease_server) = zx::EventPair::create();
            topology
                .lease(LeaseSchema {
                    lease_token: Some(lease_server),
                    lease_name: Some(name.into()),
                    dependencies: Some(vec![LeaseDependency {
                        requires_token: Some(
                            token
                                .duplicate_handle(zx::Rights::SAME_RIGHTS)
                                .context("duplicate token")?,
                        ),
                        requires_level: Some(level),
                        ..Default::default()
                    }]),
                    ..Default::default()
                })
                .await
                .with_context(|| format!("Topology.Lease({name}) FIDL failed"))?
                .map_err(|err| anyhow!("Topology.Lease({name}) failed: {err:?}"))?;
            Ok(lease_client)
        }

        async fn watch_power_level_bounded(status: &StatusProxy, label: &str) -> Result<u8> {
            status
                .watch_power_level()
                .map(Some)
                .on_timeout(
                    fasync::MonotonicDuration::from_seconds(WATCH_LEVEL_TIMEOUT_SECONDS),
                    || None,
                )
                .await
                .with_context(|| {
                    format!(
                        "WatchPowerLevel({label}) timed out after {WATCH_LEVEL_TIMEOUT_SECONDS}s"
                    )
                })?
                .with_context(|| format!("WatchPowerLevel({label}) FIDL failed"))?
                .map_err(|err| anyhow!("WatchPowerLevel({label}) failed: {err:?}"))
        }

        let realm = build_power_broker_realm().await?;
        let topology: TopologyProxy =
            realm.root.connect_to_protocol_at_exposed_dir().context("connect Topology")?;

        let c = TestElement::builder(&topology, "C", &[0, 1, 2]).build_unsettled().await;
        let b = TestElement::builder(&topology, "B", &[0, 1, 2, 3])
            .dep(1, &c, 2)
            .dep(2, &c, 1)
            .dep(3, &c, 2)
            .build_unsettled()
            .await;
        let a = TestElement::builder(&topology, "A", &[0, 1]).dep(1, &b, 2).build_unsettled().await;

        let (c_levels, _c_runner) = spawn_immediate_runner(c.runner, &c.status, "C").await?;
        let (b_levels, _b_runner) = spawn_immediate_runner(b.runner, &b.status, "B").await?;
        let (_a_levels, _a_runner) = spawn_immediate_runner(a.runner, &a.status, "A").await?;

        // Acquire `lease_2` (`B@3`) first and wait for `C=2, B=3` before acquiring `lease_1`
        // (`A@1`) so `B` transitions `0 -> 3` in a single step without an intermediate `B=2`
        // (following each element's initial `SetLevel(0)` on registration).
        let lease_2 = acquire_lease(&topology, &b.token, 3, "lease_b").await?;
        assert_eq!(watch_power_level_bounded(&c.status, "C(2)").await?, 2);
        assert_eq!(watch_power_level_bounded(&b.status, "B(3)").await?, 3);
        assert_eq!(c_levels.try_iter().collect::<Vec<_>>(), vec![0, 2]);
        assert_eq!(b_levels.try_iter().collect::<Vec<_>>(), vec![0, 3]);

        let lease_1 = acquire_lease(&topology, &a.token, 1, "lease_a").await?;
        assert_eq!(watch_power_level_bounded(&a.status, "A(1)").await?, 1);

        // Dropping `lease_1` while `lease_2` holds `B` at `3` must lower `A` to `0` without
        // hanging Power Broker or sending any `SetLevel` requests to `B` or `C`.
        drop(lease_1);
        assert_eq!(watch_power_level_bounded(&a.status, "A(0)").await?, 0);
        assert_eq!(b_levels.try_iter().collect::<Vec<_>>(), Vec::<u8>::new());
        assert_eq!(c_levels.try_iter().collect::<Vec<_>>(), Vec::<u8>::new());

        // Dropping `lease_2` then powers `B` and `C` cleanly down to `0` (also confirming no
        // late `SetLevel` arrived between `A(0)` and `B(0)`/`C(0)`).
        drop(lease_2);
        assert_eq!(watch_power_level_bounded(&b.status, "B(0)").await?, 0);
        assert_eq!(watch_power_level_bounded(&c.status, "C(0)").await?, 0);
        assert_eq!(b_levels.try_iter().collect::<Vec<_>>(), vec![0]);
        assert_eq!(c_levels.try_iter().collect::<Vec<_>>(), vec![0]);

        Ok(())
    }
}
