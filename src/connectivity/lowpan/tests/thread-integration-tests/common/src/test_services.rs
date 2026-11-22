// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! The `test-services` mock child of every node realm: capabilities the test process serves into
//! the realm, since a netemul realm can only reach the test through a mock child.
//!
//! * `test.lowpan.rfsim.Medium`: forwarded as is to the test's `rf-medium-dispatcher`.
//! * `fuchsia.net.masquerade.Factory`, `fuchsia.net.dhcpv6.PrefixProvider`: fakes standing in for
//!   netcfg.
//! * `fuchsia.device.NameProvider`: the node's host name, which mdns needs before it starts.

use crate::medium::MediumConnector;
use fidl::endpoints::{ClientEnd, DiscoverableProtocolMarker as _, Responder as _, ServerEnd};
use fidl_fuchsia_device as fdevice;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_net as fnet;
use fidl_fuchsia_net_dhcpv6 as fnet_dhcpv6;
use fidl_fuchsia_net_masquerade as fnet_masquerade;
use fidl_test_lowpan_rfsim as rfsim;
use fuchsia_async as fasync;
use fuchsia_component::server::{ServiceFs, ServiceFsDir};
use futures::{FutureExt as _, StreamExt as _, future};
use log::{error, warn};
use std::cell::RefCell;
use std::collections::HashMap;
use std::collections::hash_map::Entry;
use std::rc::Rc;

/// Name of the mock child in every node realm.
pub(crate) const COMPONENT_NAME: &str = "test-services";

/// The protocols `test-services` serves.
pub(crate) const PROTOCOLS: &[&str] = &[
    rfsim::MediumMarker::PROTOCOL_NAME,
    fnet_masquerade::FactoryMarker::PROTOCOL_NAME,
    fnet_dhcpv6::PrefixProviderMarker::PROTOCOL_NAME,
    fdevice::NameProviderMarker::PROTOCOL_NAME,
];

enum IncomingService {
    Medium(ServerEnd<rfsim::MediumMarker>),
    Masquerade(fnet_masquerade::FactoryRequestStream),
    PrefixProvider(fnet_dhcpv6::PrefixProviderRequestStream),
    NameProvider(fdevice::NameProviderRequestStream),
}

/// One active `fuchsia.net.masquerade.Control` instance held on a node's fake masquerade service.
#[derive(Clone, Debug, PartialEq)]
pub struct MasqueradeInstance {
    /// Configuration passed to `fuchsia.net.masquerade/Factory.Create`.
    pub config: fnet_masquerade::ControlConfig,
    /// Current state set via `fuchsia.net.masquerade/Control.SetEnabled` (`false` until enabled).
    pub enabled: bool,
}

/// A `ControlConfig` usable as a map key, since the FIDL type is not `Eq`/`Hash`. netcfg keys its
/// masquerade instances the same way.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
struct ConfigKey {
    src_subnet: fnet::Subnet,
    output_interface: u64,
}

impl From<&fnet_masquerade::ControlConfig> for ConfigKey {
    fn from(config: &fnet_masquerade::ControlConfig) -> Self {
        let fnet_masquerade::ControlConfig { src_subnet, output_interface } = *config;
        Self { src_subnet, output_interface }
    }
}

type MasqueradeInstances = Rc<RefCell<HashMap<ConfigKey, MasqueradeInstance>>>;

/// The capabilities the test serves into one node's realm. Tests reach it through
/// [`crate::Node::test_services`] to observe what the node asked of the fake services.
pub struct TestServices {
    masquerade_instances: MasqueradeInstances,
    _task: fasync::Task<()>,
}

impl TestServices {
    /// Starts serving on the local executor. Returns the server and the directory to hand to
    /// netemul as the mock child's outgoing directory.
    pub(crate) fn serve(
        host_name: String,
        medium: MediumConnector,
    ) -> Result<(Self, ClientEnd<fio::DirectoryMarker>), anyhow::Error> {
        let (dir, server_end) = fidl::endpoints::create_endpoints::<fio::DirectoryMarker>();
        let mut fs = ServiceFs::new_local();
        let _: &mut ServiceFsDir<'_, _> = fs
            .dir("svc")
            .add_service_connector(IncomingService::Medium)
            .add_fidl_service(IncomingService::Masquerade)
            .add_fidl_service(IncomingService::PrefixProvider)
            .add_fidl_service(IncomingService::NameProvider);
        let _: &mut ServiceFs<_> = fs.serve_connection(server_end)?;
        let masquerade_instances = MasqueradeInstances::default();
        let masquerade_for_task = Rc::clone(&masquerade_instances);
        let task =
            fasync::Task::local(fs.for_each_concurrent(None, move |service| match service {
                // Without the medium the RCP cannot start, and the test would only time out.
                IncomingService::Medium(server_end) => {
                    medium.connect_medium(server_end).expect("forwarding test.lowpan.rfsim.Medium");
                    future::ready(()).boxed_local()
                }
                IncomingService::Masquerade(stream) => {
                    serve_masquerade(stream, Rc::clone(&masquerade_for_task)).boxed_local()
                }
                IncomingService::PrefixProvider(stream) => {
                    serve_prefix_provider(stream).boxed_local()
                }
                IncomingService::NameProvider(stream) => {
                    serve_name_provider(stream, host_name.clone()).boxed_local()
                }
            }));
        Ok((Self { masquerade_instances, _task: task }, dir))
    }

    /// Snapshot of the active masquerade instances on this node, in no particular order.
    pub fn masquerade_instances(&self) -> Vec<MasqueradeInstance> {
        self.masquerade_instances.borrow().values().cloned().collect()
    }
}

/// Logs a failed reply or event: a closed peer is expected at teardown, anything else is a bug.
/// The log record carries the caller's location.
#[track_caller]
fn log_send_error(what: &str, e: fidl::Error) {
    if e.is_closed() {
        warn!("Failed to send {what}: {e:?}");
    } else {
        error!("Failed to send {what}: {e:?}");
    }
}

async fn serve_masquerade(
    mut stream: fnet_masquerade::FactoryRequestStream,
    instances: MasqueradeInstances,
) {
    while let Some(request) = stream.next().await {
        match request {
            Ok(fnet_masquerade::FactoryRequest::Create { config, control, responder }) => {
                let key = ConfigKey::from(&config);
                let response = match instances.borrow_mut().entry(key) {
                    // Matches netcfg: duplicate active ControlConfig is rejected and `control` is
                    // dropped closed.
                    Entry::Occupied(_) => Err(fnet_masquerade::Error::AlreadyExists),
                    Entry::Vacant(entry) => {
                        let _: &mut MasqueradeInstance =
                            entry.insert(MasqueradeInstance { config, enabled: false });
                        fasync::Task::local(serve_masquerade_control(
                            control.into_stream(),
                            key,
                            Rc::clone(&instances),
                        ))
                        .detach();
                        Ok(())
                    }
                };
                if let Err(e) = responder.send(response) {
                    log_send_error("masquerade.Factory.Create reply", e);
                }
            }
            Err(e) => {
                error!("masquerade.Factory request error: {e:?}");
                break;
            }
        }
    }
}

/// Serves one `fuchsia.net.masquerade.Control` instance until either end closes, updating its entry
/// in `instances` and removing it on teardown (matching `netcfg`).
async fn serve_masquerade_control(
    mut stream: fnet_masquerade::ControlRequestStream,
    key: ConfigKey,
    instances: MasqueradeInstances,
) {
    while let Some(request) = stream.next().await {
        match request {
            Ok(fnet_masquerade::ControlRequest::SetEnabled { enabled, responder }) => {
                let was_enabled = std::mem::replace(
                    &mut instances
                        .borrow_mut()
                        .get_mut(&key)
                        .expect("masquerade instance registered while Control is served")
                        .enabled,
                    enabled,
                );
                if let Err(e) = responder.send(Ok(was_enabled)) {
                    log_send_error("masquerade.Control.SetEnabled reply", e);
                }
            }
            Err(e) => {
                error!("masquerade.Control request error: {e:?}");
                break;
            }
        }
    }
    let _: MasqueradeInstance = instances
        .borrow_mut()
        .remove(&key)
        .expect("masquerade instance registered while Control is served");
}

async fn serve_prefix_provider(mut stream: fnet_dhcpv6::PrefixProviderRequestStream) {
    while let Some(request) = stream.next().await {
        match request {
            Ok(fnet_dhcpv6::PrefixProviderRequest::AcquirePrefix { config: _, prefix, .. }) => {
                fasync::Task::local(serve_prefix_control(prefix.into_stream())).detach();
            }
            Err(e) => {
                error!("dhcpv6.PrefixProvider request error: {e:?}");
                break;
            }
        }
    }
}

/// Never assigns a prefix, but keeps the control open: `PrefixControl.WatchPrefix` guarantees its
/// first reply is `Assigned`, and `lowpan-ot-driver` treats the end of the prefix stream while a
/// watch is pending as a failure and files a crash report.
async fn serve_prefix_control(mut stream: fnet_dhcpv6::PrefixControlRequestStream) {
    // Held unanswered: dropping a responder would close the channel.
    let mut pending_watch: Option<fnet_dhcpv6::PrefixControlWatchPrefixResponder> = None;
    while let Some(request) = stream.next().await {
        match request {
            Ok(fnet_dhcpv6::PrefixControlRequest::WatchPrefix { responder }) => {
                if pending_watch.is_some() {
                    error!(
                        "dhcpv6.PrefixControl.WatchPrefix called while a call is already pending"
                    );
                    if let Err(e) = responder
                        .control_handle()
                        .send_on_exit(fnet_dhcpv6::PrefixControlExitReason::DoubleWatch)
                    {
                        log_send_error("dhcpv6.PrefixControl.OnExit(DoubleWatch)", e);
                    }
                    break;
                }
                pending_watch = Some(responder);
            }
            Ok(fnet_dhcpv6::PrefixControlRequest::Stop { control_handle }) => {
                if let Err(e) =
                    control_handle.send_on_exit(fnet_dhcpv6::PrefixControlExitReason::Stopped)
                {
                    log_send_error("dhcpv6.PrefixControl.OnExit(Stopped)", e);
                }
                break;
            }
            Err(e) => {
                error!("dhcpv6.PrefixControl request error: {e:?}");
                break;
            }
        }
    }
}

async fn serve_name_provider(mut stream: fdevice::NameProviderRequestStream, host_name: String) {
    while let Some(request) = stream.next().await {
        match request {
            Ok(fdevice::NameProviderRequest::GetDeviceName { responder }) => {
                if let Err(e) = responder.send(Ok(&host_name)) {
                    log_send_error("NameProvider.GetDeviceName reply", e);
                }
            }
            Err(e) => {
                error!("NameProvider request error: {e:?}");
                break;
            }
        }
    }
}
