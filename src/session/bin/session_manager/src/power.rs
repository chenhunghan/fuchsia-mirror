// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, anyhow};
use fidl_fuchsia_power_broker as fbroker;
use fidl_fuchsia_power_system as fsystem;
use rand::RngExt as _;
use rand::distr::Alphanumeric;

const LEASE_NAME_RANDOM_SUFFIX_LEN: usize = 8;

/// A lease on the system's Application Activity power element held on behalf of the session.
///
/// After `session_manager` starts, a direct lease on the Application Activity element owned by
/// SAG is created and retained. The session component may fetch the lease from `session_manager`
/// and decide when to drop it.
///
/// When stopping or restarting the session, the lease will be recreated if it was previously
/// taken, returning `session_manager` to the initial started state.
#[derive(Debug)]
pub struct PowerLease {
    lease: Option<fbroker::LeaseToken>,
}

impl PowerLease {
    /// Acquires a lease on SAG's Application Activity element and signals boot completion.
    ///
    /// Blocks until the lease is satisfied before calling `BootControl.SetBootComplete`.
    ///
    /// # Errors
    ///
    /// Returns an error if a required protocol cannot be reached, SAG does not provide an
    /// Application Activity assertive dependency token, or Power Broker rejects the lease.
    pub async fn new() -> Result<Self, anyhow::Error> {
        let topology = fuchsia_component::client::connect_to_protocol::<fbroker::TopologyMarker>()?;
        let activity_governor =
            fuchsia_component::client::connect_to_protocol::<fsystem::ActivityGovernorMarker>()?;
        let boot_control =
            fuchsia_component::client::connect_to_protocol::<fsystem::BootControlMarker>()?;

        Self::acquire(&topology, &activity_governor, &boot_control).await
    }

    async fn acquire(
        topology: &fbroker::TopologyProxy,
        activity_governor: &fsystem::ActivityGovernorProxy,
        boot_control: &fsystem::BootControlProxy,
    ) -> Result<Self, anyhow::Error> {
        let power_elements = activity_governor
            .get_power_elements()
            .await
            .context("cannot get power elements from SAG")?;
        let Some(Some(application_activity_token)) = power_elements
            .application_activity
            .map(|application_activity| application_activity.assertive_dependency_token)
        else {
            return Err(anyhow!("Did not find application activity assertive dependency token"));
        };

        // TODO(https://fxbug.dev/316023943): also depend on execution_resume_latency after implemented.
        let (lease, server_token) = fbroker::LeaseToken::create();
        let random_string: String = rand::rng()
            .sample_iter(&Alphanumeric)
            .take(LEASE_NAME_RANDOM_SUFFIX_LEN)
            .map(char::from)
            .collect();
        topology
            .lease(fbroker::LeaseSchema {
                lease_token: Some(server_token),
                lease_name: Some(format!("session-manager-lease-{random_string}")),
                dependencies: Some(vec![fbroker::LeaseDependency {
                    requires_token: Some(application_activity_token),
                    requires_level: Some(
                        fsystem::ApplicationActivityLevel::Active.into_primitive(),
                    ),
                    ..Default::default()
                }]),
                ..Default::default()
            })
            .await?
            .map_err(|e| anyhow!("PowerBroker::LeaseError({e:?})"))?;

        let () = boot_control.set_boot_complete().await?;

        Ok(Self { lease: Some(lease) })
    }

    pub fn take_lease(&mut self) -> Option<fbroker::LeaseToken> {
        self.lease.take()
    }

    pub fn has_lease(&self) -> bool {
        self.lease.is_some()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use fidl::endpoints::create_proxy_and_stream;
    use fuchsia_async as fasync;
    use futures::StreamExt as _;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[fuchsia::test]
    async fn acquire_leases_application_activity_before_completing_boot() {
        let (topology, mut topology_stream) = create_proxy_and_stream::<fbroker::TopologyMarker>();
        let (activity_governor, mut activity_governor_stream) =
            create_proxy_and_stream::<fsystem::ActivityGovernorMarker>();
        let (boot_control, mut boot_control_stream) =
            create_proxy_and_stream::<fsystem::BootControlMarker>();

        let application_activity_token = fbroker::DependencyToken::create();
        let application_activity_token_koid = application_activity_token.koid().unwrap();
        let lease_satisfied = Arc::new(AtomicBool::new(false));
        let boot_completed = Arc::new(AtomicBool::new(false));

        let _activity_governor_task = fasync::Task::local(async move {
            let Some(Ok(fsystem::ActivityGovernorRequest::GetPowerElements { responder })) =
                activity_governor_stream.next().await
            else {
                panic!("Expected GetPowerElements request");
            };
            let _ = responder.send(fsystem::PowerElements {
                application_activity: Some(fsystem::ApplicationActivity {
                    assertive_dependency_token: Some(application_activity_token),
                    ..Default::default()
                }),
                ..Default::default()
            });
        });

        let lease_satisfied_clone = Arc::clone(&lease_satisfied);
        let _topology_task = fasync::Task::local(async move {
            let Some(Ok(fbroker::TopologyRequest::Lease { payload, responder })) =
                topology_stream.next().await
            else {
                panic!("Expected Topology.Lease request");
            };
            assert!(payload.lease_name.unwrap().starts_with("session-manager-lease-"));
            assert_eq!(payload.should_return_pending_lease, None);
            let deps = payload.dependencies.unwrap();
            assert_eq!(deps.len(), 1);
            assert_eq!(
                deps[0].requires_token.as_ref().unwrap().koid().unwrap(),
                application_activity_token_koid
            );
            assert_eq!(
                deps[0].requires_level,
                Some(fsystem::ApplicationActivityLevel::Active.into_primitive())
            );
            let _server_token = payload.lease_token.unwrap();
            lease_satisfied_clone.store(true, Ordering::SeqCst);
            let _ = responder.send(Ok(()));
        });

        let lease_satisfied_for_boot = Arc::clone(&lease_satisfied);
        let boot_completed_clone = Arc::clone(&boot_completed);
        let _boot_task = fasync::Task::local(async move {
            let Some(Ok(fsystem::BootControlRequest::SetBootComplete { responder })) =
                boot_control_stream.next().await
            else {
                panic!("Expected SetBootComplete request");
            };
            assert!(
                lease_satisfied_for_boot.load(Ordering::SeqCst),
                "SetBootComplete called before Topology.Lease completed"
            );
            boot_completed_clone.store(true, Ordering::SeqCst);
            let _ = responder.send();
        });

        let power_lease =
            PowerLease::acquire(&topology, &activity_governor, &boot_control).await.unwrap();
        assert!(boot_completed.load(Ordering::SeqCst));
        assert!(power_lease.has_lease());
    }

    #[fuchsia::test]
    async fn take_lease_returns_token_once_and_drop_closes_peer() {
        let (topology, mut topology_stream) = create_proxy_and_stream::<fbroker::TopologyMarker>();
        let (activity_governor, mut activity_governor_stream) =
            create_proxy_and_stream::<fsystem::ActivityGovernorMarker>();
        let (boot_control, mut boot_control_stream) =
            create_proxy_and_stream::<fsystem::BootControlMarker>();

        let _activity_governor_task = fasync::Task::local(async move {
            let Some(Ok(fsystem::ActivityGovernorRequest::GetPowerElements { responder })) =
                activity_governor_stream.next().await
            else {
                panic!("Expected GetPowerElements request");
            };
            let _ = responder.send(fsystem::PowerElements {
                application_activity: Some(fsystem::ApplicationActivity {
                    assertive_dependency_token: Some(fbroker::DependencyToken::create()),
                    ..Default::default()
                }),
                ..Default::default()
            });
        });

        let topology_task = fasync::Task::local(async move {
            let Some(Ok(fbroker::TopologyRequest::Lease { payload, responder })) =
                topology_stream.next().await
            else {
                panic!("Expected Topology.Lease request");
            };
            let server_token = payload.lease_token.unwrap();
            let _ = responder.send(Ok(()));
            server_token
        });

        let _boot_task = fasync::Task::local(async move {
            if let Some(Ok(fsystem::BootControlRequest::SetBootComplete { responder })) =
                boot_control_stream.next().await
            {
                let _ = responder.send();
            }
        });

        let mut power_lease =
            PowerLease::acquire(&topology, &activity_governor, &boot_control).await.unwrap();
        let server_token = topology_task.await;

        let client_token = power_lease.take_lease().unwrap();
        assert!(!power_lease.has_lease());
        assert!(power_lease.take_lease().is_none());
        assert_matches!(
            server_token
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
                .to_result(),
            Err(zx::Status::TIMED_OUT)
        );

        drop(client_token);
        assert_matches!(
            server_token
                .wait_one(zx::Signals::EVENTPAIR_PEER_CLOSED, zx::MonotonicInstant::INFINITE_PAST)
                .to_result(),
            Ok(_)
        );
    }

    #[fuchsia::test]
    async fn acquire_returns_error_without_completing_boot_when_lease_fails() {
        let (topology, mut topology_stream) = create_proxy_and_stream::<fbroker::TopologyMarker>();
        let (activity_governor, mut activity_governor_stream) =
            create_proxy_and_stream::<fsystem::ActivityGovernorMarker>();
        let (boot_control, mut boot_control_stream) =
            create_proxy_and_stream::<fsystem::BootControlMarker>();

        let application_activity_token = fbroker::DependencyToken::create();
        let _activity_governor_task = fasync::Task::local(async move {
            if let Some(Ok(fsystem::ActivityGovernorRequest::GetPowerElements { responder })) =
                activity_governor_stream.next().await
            {
                let _ = responder.send(fsystem::PowerElements {
                    application_activity: Some(fsystem::ApplicationActivity {
                        assertive_dependency_token: Some(application_activity_token),
                        ..Default::default()
                    }),
                    ..Default::default()
                });
            }
        });

        let _topology_task = fasync::Task::local(async move {
            if let Some(Ok(fbroker::TopologyRequest::Lease { responder, .. })) =
                topology_stream.next().await
            {
                let _ = responder.send(Err(fbroker::LeaseError::Internal));
            }
        });

        let err = PowerLease::acquire(&topology, &activity_governor, &boot_control)
            .await
            .expect_err("acquire should fail when Topology.Lease returns LeaseError");
        assert!(format!("{err:#}").contains("LeaseError(Internal)"), "unexpected error: {err:#}");
        drop(boot_control);
        assert!(boot_control_stream.next().await.is_none());
    }

    #[fuchsia::test]
    async fn acquire_returns_error_when_application_activity_token_missing() {
        for elements in [
            fsystem::PowerElements::default(),
            fsystem::PowerElements {
                application_activity: Some(fsystem::ApplicationActivity::default()),
                ..Default::default()
            },
        ] {
            let (topology, mut topology_stream) =
                create_proxy_and_stream::<fbroker::TopologyMarker>();
            let (activity_governor, mut activity_governor_stream) =
                create_proxy_and_stream::<fsystem::ActivityGovernorMarker>();
            let (boot_control, mut boot_control_stream) =
                create_proxy_and_stream::<fsystem::BootControlMarker>();

            let _activity_governor_task = fasync::Task::local(async move {
                if let Some(Ok(fsystem::ActivityGovernorRequest::GetPowerElements { responder })) =
                    activity_governor_stream.next().await
                {
                    let _ = responder.send(elements);
                }
            });

            let err = PowerLease::acquire(&topology, &activity_governor, &boot_control)
                .await
                .expect_err("acquire should fail when application_activity token is missing");
            assert!(
                format!("{err:#}")
                    .contains("Did not find application activity assertive dependency token"),
                "unexpected error: {err:#}"
            );
            drop(topology);
            drop(boot_control);
            assert!(topology_stream.next().await.is_none());
            assert!(boot_control_stream.next().await.is_none());
        }
    }
}
