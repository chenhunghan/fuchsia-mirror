// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Netstack3 bindings system power handling.

use assert_matches::assert_matches;
use fidl_fuchsia_power_broker as fbroker;
use fuchsia_async as fasync;
use futures::{FutureExt as _, StreamExt as _};
use log::{debug, error, warn};
use netstack3_core::device::WeakDeviceId;
use thiserror::Error;

use crate::bindings::devices::{DeviceSpecificInfo, EthernetInfo, PureIpDeviceInfo};
use crate::bindings::power::element::InternalPowerElement;
use crate::bindings::power::suspension_block::MaybeSuspensionGuard;
use crate::bindings::power::{BinaryPowerLevel, LessorRegistration};
use crate::bindings::{BindingsCtx, Ctx, DeviceIdAndName, DeviceIdExt as _};

const SUSPENSION_WARN_INTERVAL: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(10);
const LEASE_SIGNAL_SATISFIED: zx::Signals =
    zx::Signals::from_bits(fbroker::LEASE_SIGNAL_SATISFIED).unwrap();

/// A [`fbroker::DependencyToken`] verified to have [`zx::Rights::DUPLICATE`].
pub(crate) struct PowerDependencyToken(fbroker::DependencyToken);

impl PowerDependencyToken {
    /// Wraps `token` if it has [`zx::Rights::DUPLICATE`], which is required to
    /// duplicate the token for each lease taken on it.
    pub(crate) fn try_new(token: fbroker::DependencyToken) -> Option<Self> {
        let rights = token.basic_info().expect("get basic info of open handle").rights;
        rights.contains(zx::Rights::DUPLICATE).then_some(Self(token))
    }

    fn duplicate(&self) -> fbroker::DependencyToken {
        let Self(token) = self;
        token
            .duplicate_handle(zx::Rights::SAME_RIGHTS)
            .expect("verified DUPLICATE right on construction")
    }
}

#[derive(Error, Debug)]
enum DriverLeaseError {
    #[error("Topology.Lease FIDL error: {0}")]
    Fidl(#[from] fidl::Error),
    #[error("Topology.Lease failed: {0:?}")]
    Lease(fbroker::LeaseError),
    #[error("lease token peer closed")]
    PeerClosed,
}

#[derive(Debug)]
enum LeaseState {
    Released,
    Acquired(fbroker::LeaseToken),
    FailedOpen,
}

struct DriverLease {
    topology: fbroker::TopologyProxy,
    dependency_token: PowerDependencyToken,
    state: LeaseState,
}

impl DriverLease {
    fn new(topology: fbroker::TopologyProxy, dependency_token: PowerDependencyToken) -> Self {
        Self { topology, dependency_token, state: LeaseState::Released }
    }

    async fn try_acquire(&mut self, name: &DeviceIdAndName) {
        let Self { topology, dependency_token, state } = self;
        assert_matches!(state, LeaseState::Released);
        let result = async {
            let dependency_token = dependency_token.duplicate();
            let (lease_token, server_token) = zx::EventPair::create();
            let schema = fbroker::LeaseSchema {
                lease_token: Some(server_token),
                lease_name: Some(format!("netstack-tx-{name}")),
                dependencies: Some(vec![fbroker::LeaseDependency {
                    requires_token: Some(dependency_token),
                    requires_level: Some(fbroker::BinaryPowerLevel::On.into_primitive()),
                    ..Default::default()
                }]),
                // All devices share the same connection. We'll wait for the lease
                // to be resolved ourselves to avoid unintended serialization.
                should_return_pending_lease: Some(true),
                ..Default::default()
            };
            topology.lease(schema).await?.map_err(DriverLeaseError::Lease)?;
            debug!("waiting for driver power lease satisfaction for {name}");
            let signals = fasync::OnSignals::new(
                &lease_token,
                LEASE_SIGNAL_SATISFIED | zx::Signals::EVENTPAIR_PEER_CLOSED,
            )
            .await
            .expect("waiting on a locally created eventpair never fails");
            if signals.contains(zx::Signals::EVENTPAIR_PEER_CLOSED) {
                return Err(DriverLeaseError::PeerClosed);
            }
            debug!("driver power lease satisfied for {name}");
            Ok(lease_token)
        }
        .await;
        *state = match result {
            Ok(lease_token) => LeaseState::Acquired(lease_token),
            Err(e) => {
                error!(
                    "failed to acquire driver power lease for {name}: {e}; \
                    proceeding without lease"
                );
                LeaseState::FailedOpen
            }
        };
    }

    fn release(&mut self) {
        let Self { topology: _, dependency_token: _, state } = self;
        match std::mem::replace(state, LeaseState::Released) {
            LeaseState::Acquired(lease_token) => {
                let _: fbroker::LeaseToken = lease_token;
            }
            LeaseState::FailedOpen => {}
            LeaseState::Released => panic!("attempted to release an unacquired driver lease"),
        }
    }
}

/// Handles tx path suspension for devices.
pub(crate) struct TransmitSuspensionHandler {
    internal_power_element: InternalPowerElement,
    // None if suspension is disabled, connecting to
    // fuchsia.power.broker.Topology failed, or the port does not provide a
    // power dependency token.
    //
    // Dropped before suspension_guard so the lease closes before unblocking
    // suspension.
    driver_lease: Option<DriverLease>,
    suspension_guard: MaybeSuspensionGuard,
    device: WeakDeviceId<BindingsCtx>,
    _lessor_registration: LessorRegistration,
}

impl TransmitSuspensionHandler {
    /// Creates a new `TransmitSuspensionHandler` that communicates with the
    /// `PowerWorker` in `ctx`.
    ///
    /// If `power_dependency_token` is provided, a Power Broker lease on it is
    /// acquired before enabling TX and held while TX is active.
    pub(crate) async fn new(
        ctx: &Ctx,
        device: WeakDeviceId<BindingsCtx>,
        power_dependency_token: Option<PowerDependencyToken>,
    ) -> Self {
        let internal_power_element = InternalPowerElement::new();
        let registration =
            ctx.bindings_ctx().power.register_internal_lessor(internal_power_element.lessor());
        let driver_lease = power_dependency_token
            .zip(ctx.bindings_ctx().power.topology.clone())
            .map(|(dependency_token, topology)| DriverLease::new(topology, dependency_token));

        let mut this = Self {
            internal_power_element,
            driver_lease,
            suspension_guard: MaybeSuspensionGuard::new(
                ctx.bindings_ctx().power.suspension_block.clone(),
            ),
            device,
            _lessor_registration: registration,
        };

        // Power elements start in off state. Wait until our lease registration
        // has told us to be active (and the driver lease, if any, is
        // satisfied) before returning from `new`.
        this.wait_for_level_on().await;

        this
    }

    /// Waits for a suspension request.
    pub(crate) async fn wait(&mut self) -> TransmitSuspensionRequest<'_> {
        let Self {
            internal_power_element,
            driver_lease: _,
            suspension_guard: _,
            device: _,
            _lessor_registration,
        } = self;
        // May only be called if `internal_power_element` is on.
        assert_eq!(internal_power_element.current_level(), BinaryPowerLevel::On);
        assert_eq!(internal_power_element.wait_level_change().await, BinaryPowerLevel::Off);
        TransmitSuspensionRequest(self)
    }

    async fn wait_for_level_on(&mut self) {
        let Self {
            internal_power_element,
            driver_lease,
            suspension_guard,
            device,
            _lessor_registration,
        } = self;
        wait_for_level_on(
            internal_power_element,
            suspension_guard,
            driver_lease.as_mut(),
            device.bindings_id(),
        )
        .await
    }
}

async fn wait_for_level_on(
    internal_power_element: &mut InternalPowerElement,
    suspension_guard: &mut MaybeSuspensionGuard,
    mut driver_lease: Option<&mut DriverLease>,
    name: &DeviceIdAndName,
) {
    loop {
        assert_eq!(internal_power_element.wait_level_change().await, BinaryPowerLevel::On);

        futures::select! {
            nl = internal_power_element.wait_level_change().fuse() => {
                // Power level changed back to off while we were waiting
                // for a suspension guard, loop and wait for it to be on
                // again.
                assert_eq!(nl, BinaryPowerLevel::Off);
                continue;
            }
            acquired = suspension_guard.acquire().fuse() => assert!(acquired),
        }

        let Some(driver_lease) = driver_lease.as_mut() else {
            break;
        };
        let warn_interval = fasync::Interval::new(SUSPENSION_WARN_INTERVAL).map(|()| {
            warn!("device {name} still waiting for driver power lease");
        });
        futures::select! {
            nl = internal_power_element.wait_level_change().fuse() => {
                // Power level changed back to off while we were acquiring
                // the driver lease. Release the suspension guard and loop
                // to try again.
                assert_eq!(nl, BinaryPowerLevel::Off);
                assert!(suspension_guard.release());
                continue;
            }
            () = driver_lease.try_acquire(name).fuse() => break,
            () = warn_interval.collect::<()>() => unreachable!(),
        }
    }
}

pub(crate) struct TransmitSuspensionRequest<'a>(&'a mut TransmitSuspensionHandler);

impl TransmitSuspensionRequest<'_> {
    /// Handles a suspension request, blocking until the system resumes.
    ///
    /// Waits for the transmitting device to finish its work, drops the driver
    /// power lease (if any), and then signals back to power framework that our
    /// power element's level is lowered. Then it waits for the level of our
    /// power element to rise and re-acquires the driver power lease (if any)
    /// before returning.
    /// The rise in power level indicates that it is okay for us to resume
    /// normal operation.
    pub(crate) async fn handle_suspension(self) {
        let Self(handler) = self;
        let TransmitSuspensionHandler {
            internal_power_element,
            driver_lease,
            suspension_guard,
            device,
            _lessor_registration,
        } = handler;
        assert_eq!(internal_power_element.current_level(), BinaryPowerLevel::Off);
        let name = device.bindings_id();

        // Wait for the device to be ready for suspension. Meanwhile, PF is
        // waiting for us to notify it that we've transitioned to the disabled
        // state.
        debug!("handling suspension for {name}. Waiting for tx done.");
        let warn_interval = fasync::Interval::new(SUSPENSION_WARN_INTERVAL).map(|()| {
            warn!("device {name} still pending tx suspension");
        });
        futures::select! {
            () = wait_device_tx_suspension_ready(device).fuse() => (),
            () = warn_interval.collect::<()>() => unreachable!(),
        }
        debug!("suspension handling for {name} complete.");

        if let Some(driver_lease) = driver_lease.as_mut() {
            driver_lease.release();
        }
        // Drop our guard on suspension.
        assert!(suspension_guard.release());
        // Wait for the power level to go back to on.
        handler.wait_for_level_on().await;

        debug!("resumed device {} from suspension", handler.device.bindings_id());
    }
}

async fn wait_device_tx_suspension_ready(device: &WeakDeviceId<BindingsCtx>) {
    let Some(device) = device.upgrade() else {
        return;
    };
    match device.external_state() {
        DeviceSpecificInfo::Loopback(_) => {
            // No suspension handling for loopback.
        }
        DeviceSpecificInfo::Blackhole(_) => {
            // No suspension handling for blackhole devices.
        }
        DeviceSpecificInfo::Ethernet(EthernetInfo { netdevice, .. })
        | DeviceSpecificInfo::PureIp(PureIpDeviceInfo { netdevice, .. }) => {
            // For netdevice backed devices, wait for all tx descriptors to be
            // done before continuing with suspension.
            netdevice.handler.wait_tx_done().await;
        }
    };
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::pin::pin;
    use std::task::Poll;

    use test_case::test_case;
    use zx::Peered as _;

    use crate::bindings::devices::BindingId;
    use crate::bindings::power::suspension_block::SuspensionBlockControl;

    fn expect_lease_request(
        exec: &mut fasync::TestExecutor,
        topology_stream: &mut fbroker::TopologyRequestStream,
        expected_device: &DeviceIdAndName,
        expected_token_koid: zx::Koid,
    ) -> (fbroker::LeaseToken, fbroker::TopologyLeaseResponder) {
        let (payload, responder) = match exec.run_until_stalled(&mut topology_stream.next()) {
            Poll::Ready(Some(Ok(fbroker::TopologyRequest::Lease { payload, responder }))) => {
                (payload, responder)
            }
            other => panic!("unexpected topology request: {other:?}"),
        };
        let fbroker::LeaseSchema {
            lease_token,
            lease_name,
            dependencies,
            should_return_pending_lease,
            ..
        } = payload;
        assert_eq!(lease_name, Some(format!("netstack-tx-{expected_device}")));
        assert_eq!(should_return_pending_lease, Some(true));
        let [dependency]: [fbroker::LeaseDependency; 1] =
            dependencies.expect("dependencies").try_into().expect("single dependency");
        let fbroker::LeaseDependency {
            requires_token,
            requires_level,
            requires_level_by_preference,
            ..
        } = dependency;
        assert_eq!(requires_token.expect("requires_token").koid(), Ok(expected_token_koid));
        assert_eq!(requires_level, Some(fbroker::BinaryPowerLevel::On.into_primitive()));
        assert_eq!(requires_level_by_preference, None);
        (lease_token.expect("lease_token"), responder)
    }

    #[test]
    fn wait_for_level_on_cancel_and_acquire() {
        let mut exec = fasync::TestExecutor::new();

        let mut internal_power_element = InternalPowerElement::new();
        let lessor = internal_power_element.lessor();
        let mut suspension_control = SuspensionBlockControl::new();
        let mut suspension_guard = MaybeSuspensionGuard::new(suspension_control.issuer());
        let (topology, mut topology_stream) =
            fidl::endpoints::create_proxy_and_stream::<fbroker::TopologyMarker>();
        let dependency_token = zx::Event::create();
        let expected_koid = dependency_token.koid().expect("koid");
        let dependency_token =
            PowerDependencyToken::try_new(dependency_token).expect("duplicable token");
        let mut driver_lease = DriverLease::new(topology, dependency_token);
        let name = DeviceIdAndName { id: BindingId::MIN, name: "eth".to_string() };

        let server_token = {
            let mut wait_fut = pin!(wait_for_level_on(
                &mut internal_power_element,
                &mut suspension_guard,
                Some(&mut driver_lease),
                &name,
            ));
            assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Pending);

            // No driver lease is requested before the suspension guard is
            // acquired.
            assert_eq!(suspension_control.suspend().now_or_never(), Some(()));
            let internal_lease = lessor.lease();
            assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Pending);
            assert_matches!(exec.run_until_stalled(&mut topology_stream.next()), Poll::Pending);
            drop(internal_lease);
            assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Pending);

            // No suspension guard is acquired when guards are unblocked while
            // power is off.
            suspension_control.resume();
            assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Pending);
            assert_eq!(suspension_control.suspend().now_or_never(), Some(()));
            suspension_control.resume();

            // The driver lease and suspension guard are released when power
            // goes off while waiting for the driver lease.
            let internal_lease = lessor.lease();
            assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Pending);
            let (server_token, responder) =
                expect_lease_request(&mut exec, &mut topology_stream, &name, expected_koid);
            responder.send(Ok(())).expect("send Lease response");
            assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Pending);
            assert_eq!(server_token.is_closed(), Ok(false));
            drop(internal_lease);
            assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Pending);
            assert_eq!(server_token.is_closed(), Ok(true));
            assert_eq!(suspension_control.suspend().now_or_never(), Some(()));
            suspension_control.resume();

            // The wait completes when power is on and the driver lease is
            // satisfied.
            let _internal_lease = lessor.lease();
            assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Pending);
            let (server_token, responder) =
                expect_lease_request(&mut exec, &mut topology_stream, &name, expected_koid);
            responder.send(Ok(())).expect("send Lease response");
            assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Pending);

            server_token
                .signal_peer(zx::Signals::NONE, LEASE_SIGNAL_SATISFIED)
                .expect("signal LEASE_SIGNAL_SATISFIED");
            assert_matches!(exec.run_until_stalled(&mut wait_fut), Poll::Ready(()));
            server_token
        };

        assert_matches!(suspension_guard, MaybeSuspensionGuard::Guarded(_));
        assert_matches!(driver_lease.state, LeaseState::Acquired(_));
        assert_eq!(server_token.is_closed(), Ok(false));
        driver_lease.release();
        assert_eq!(server_token.is_closed(), Ok(true));
        assert_matches!(driver_lease.state, LeaseState::Released);
    }

    enum LeaseFailure {
        LeaseError,
        FidlError,
        PeerClosed,
        SatisfiedAndPeerClosed,
    }

    #[test_case(LeaseFailure::LeaseError)]
    #[test_case(LeaseFailure::FidlError)]
    #[test_case(LeaseFailure::PeerClosed)]
    #[test_case(LeaseFailure::SatisfiedAndPeerClosed)]
    fn driver_lease_try_acquire_error(failure: LeaseFailure) {
        let mut exec = fasync::TestExecutor::new();

        let (topology, mut topology_stream) =
            fidl::endpoints::create_proxy_and_stream::<fbroker::TopologyMarker>();
        let dependency_token = zx::Event::create();
        let expected_koid = dependency_token.koid().expect("koid");
        let dependency_token =
            PowerDependencyToken::try_new(dependency_token).expect("duplicable token");
        let name = DeviceIdAndName { id: BindingId::MIN, name: "eth".to_string() };

        let mut driver_lease = DriverLease::new(topology, dependency_token);
        let server_token = {
            let mut try_acquire = pin!(driver_lease.try_acquire(&name));
            assert_matches!(exec.run_until_stalled(&mut try_acquire), Poll::Pending);

            let (server_token, responder) =
                expect_lease_request(&mut exec, &mut topology_stream, &name, expected_koid);
            let server_token = match failure {
                LeaseFailure::LeaseError => {
                    responder.send(Err(fbroker::LeaseError::Internal)).expect("send Lease error");
                    Some(server_token)
                }
                LeaseFailure::FidlError => {
                    drop((responder, topology_stream));
                    Some(server_token)
                }
                LeaseFailure::PeerClosed => {
                    responder.send(Ok(())).expect("send Lease response");
                    drop(server_token);
                    None
                }
                LeaseFailure::SatisfiedAndPeerClosed => {
                    responder.send(Ok(())).expect("send Lease response");
                    server_token
                        .signal_peer(zx::Signals::NONE, LEASE_SIGNAL_SATISFIED)
                        .expect("signal");
                    drop(server_token);
                    None
                }
            };

            assert_matches!(exec.run_until_stalled(&mut try_acquire), Poll::Ready(()));
            server_token
        };

        assert_matches!(driver_lease.state, LeaseState::FailedOpen);
        if let Some(server_token) = server_token {
            assert_eq!(server_token.is_closed(), Ok(true));
        }
        driver_lease.release();
        assert_matches!(driver_lease.state, LeaseState::Released);
    }
}
