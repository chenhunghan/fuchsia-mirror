// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The network's simulated IEEE 802.15.4 medium: an `rf-medium-dispatcher` component in a netemul
//! realm of its own.
//!
//! Node realms cannot route capabilities from another realm, so each node's `test-services` mock
//! child forwards the channels its RCP opens to this realm (see [`MediumConnector`]).

use anyhow::Context as _;
use fidl::endpoints::{DiscoverableProtocolMarker as _, ServerEnd};
use fidl_fuchsia_netemul as fnetemul;
use fidl_test_lowpan_rfsim as rfsim;

/// Name of the medium's realm.
const REALM_NAME: &str = "rf-medium";

/// Name of the `rf-medium-dispatcher` child in the medium's realm.
const COMPONENT_NAME: &str = "rf-medium-dispatcher";

/// `rf-medium-dispatcher`'s subpackage URL, relative to the test package.
const COMPONENT_URL: &str = "rf-medium-dispatcher#meta/rf-medium-dispatcher.cm";

/// Owns the medium's realm. Dropping it shuts the realm down, and panics if `rf-medium-dispatcher`
/// did not exit cleanly.
pub(crate) struct MediumRealm {
    realm: fnetemul::ManagedRealmProxy,
    // The realm lives as long as the sandbox connection that created it.
    _sandbox: fnetemul::SandboxProxy,
}

impl MediumRealm {
    /// Spawns a new `rf-medium-dispatcher`.
    ///
    /// The realm is created from a sandbox connection of its own rather than from the network's
    /// `netemul::TestSandbox`: forwarding channels from the `test-services` mock children needs a
    /// `'static` handle to the realm, which a `netemul::TestRealm` borrowing the sandbox is not.
    /// The medium uses no netemul network, so being in another sandbox context makes no difference.
    pub(crate) fn new() -> Result<Self, anyhow::Error> {
        let sandbox = fuchsia_component::client::connect_to_protocol::<fnetemul::SandboxMarker>()
            .context("connecting to fuchsia.netemul.Sandbox")?;
        let (realm, server_end) = fidl::endpoints::create_proxy::<fnetemul::ManagedRealmMarker>();
        sandbox
            .create_realm(
                server_end,
                fnetemul::RealmOptions {
                    name: Some(REALM_NAME.to_string()),
                    children: Some(vec![fnetemul::ChildDef {
                        source: Some(fnetemul::ChildSource::Component(COMPONENT_URL.to_string())),
                        name: Some(COMPONENT_NAME.to_string()),
                        exposes: Some(vec![
                            rfsim::MediumMarker::PROTOCOL_NAME.to_string(),
                            rfsim::MonitorMarker::PROTOCOL_NAME.to_string(),
                        ]),
                        uses: Some(fnetemul::ChildUses::Capabilities(vec![
                            fnetemul::Capability::LogSink(fnetemul::Empty {}),
                        ])),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
            )
            .context("Sandbox.CreateRealm")?;
        Ok(Self { realm, _sandbox: sandbox })
    }

    /// Returns a handle that connects to the medium.
    pub(crate) fn connector(&self) -> MediumConnector {
        MediumConnector { realm: self.realm.clone() }
    }

    /// Connects to the medium's `test.lowpan.rfsim.Monitor`.
    pub(crate) fn monitor(&self) -> Result<rfsim::MonitorProxy, anyhow::Error> {
        let (proxy, server_end) = fidl::endpoints::create_proxy::<rfsim::MonitorMarker>();
        self.realm
            .connect_to_protocol(
                rfsim::MonitorMarker::PROTOCOL_NAME,
                Some(COMPONENT_NAME),
                server_end.into_channel(),
            )
            .context("ManagedRealm.ConnectToProtocol(Monitor)")?;
        Ok(proxy)
    }
}

impl Drop for MediumRealm {
    fn drop(&mut self) {
        let (listener, server_end) =
            fidl::endpoints::create_sync_proxy::<fnetemul::CrashListenerMarker>();
        let result = self
            .realm
            .get_crash_listener(server_end)
            .and_then(|()| self.realm.shutdown())
            .and_then(|()| {
                // Yields the monikers of the components that exited uncleanly, then an empty
                // vector once the realm has terminated.
                let mut unclean = Vec::new();
                loop {
                    let monikers = listener.next(zx::MonotonicInstant::INFINITE)?;
                    if monikers.is_empty() {
                        break Ok(unclean);
                    }
                    unclean.extend(monikers);
                }
            });
        match result {
            Ok(unclean) => {
                if !unclean.is_empty() {
                    panic!("realm {REALM_NAME} found unclean component stops: {unclean:?}");
                }
            }
            // The realm is already gone, e.g. it failed to build: nothing to check.
            Err(e) if e.is_closed() => {}
            Err(e) => panic!("error verifying clean shutdown of realm {REALM_NAME}: {e:?}"),
        }
    }
}

/// Connects channels to the medium. Cheap to clone; does not keep the medium alive.
#[derive(Clone)]
pub(crate) struct MediumConnector {
    realm: fnetemul::ManagedRealmProxy,
}

impl MediumConnector {
    /// Hands `server_end` to `rf-medium-dispatcher` as is.
    pub(crate) fn connect_medium(
        &self,
        server_end: ServerEnd<rfsim::MediumMarker>,
    ) -> Result<(), anyhow::Error> {
        self.realm
            .connect_to_protocol(
                rfsim::MediumMarker::PROTOCOL_NAME,
                Some(COMPONENT_NAME),
                server_end.into_channel(),
            )
            .context("ManagedRealm.ConnectToProtocol(Medium)")
    }
}
