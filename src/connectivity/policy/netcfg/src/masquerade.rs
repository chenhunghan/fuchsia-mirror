// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::collections::HashMap;

use derivative::Derivative;
use fidl_fuchsia_net as fnet;
use fidl_fuchsia_net_filter_ext::{CommitError, Matchers, PushChangesError, RuleId};
use fidl_fuchsia_net_masquerade as fnet_masquerade;
use fidl_fuchsia_net_matchers_ext as fnet_matchers_ext;
use fnet_masquerade::Error;
use futures::stream::LocalBoxStream;
use futures::{StreamExt as _, TryStreamExt as _, future};
use log::{debug, error, warn};

use crate::InterfaceId;
use crate::filter::{FilterControl, FilterError};

// The minimum allowed prefix length for IPv4 masquerading subnets.
//
// While the OpenThread API only requires a non-zero prefix, in practice we do
// not need to support subnets larger than a Class A private network (/8, ~16M
// addresses). This covers the default /24 pool used by lowpan-ot-driver while
// blocking overly broad /1 to /7 subnets.
const MIN_V4_MASQUERADE_PREFIX_LEN: u8 = 8;

// The minimum allowed prefix length for IPv6 masquerading subnets.
//
// Enforced to prevent overly broad masquerade rules. Aligns with RFC 6052
// section 2.2 and OpenThread's NAT64 translator API
// (https://openthread.io/reference/group/api-nat64#otip4extractfromip6address),
// which both specify a minimum prefix length of 32.
const MIN_V6_MASQUERADE_PREFIX_LEN: u8 = 32;

#[derive(Derivative)]
#[derivative(Debug)]
pub(super) enum Event {
    FactoryRequestStream(#[derivative(Debug = "ignore")] fnet_masquerade::FactoryRequestStream),
    FactoryRequest(fnet_masquerade::FactoryRequest),
    ControlRequest(ValidatedConfig, fnet_masquerade::ControlRequest),
    Disconnect(ValidatedConfig),
}

pub(super) type EventStream = LocalBoxStream<'static, Result<Event, fidl::Error>>;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(super) struct ValidatedConfig {
    /// The network to be masqueraded.
    pub src_subnet: fnet_matchers_ext::Subnet,
    /// The interface through which to masquerade.
    pub output_interface: InterfaceId,
}

impl TryFrom<fnet_masquerade::ControlConfig> for ValidatedConfig {
    type Error = fnet_masquerade::Error;

    fn try_from(
        fnet_masquerade::ControlConfig {
            src_subnet,
            output_interface
        }: fnet_masquerade::ControlConfig,
    ) -> Result<Self, Self::Error> {
        match src_subnet.addr {
            fnet::IpAddress::Ipv4(_) => {
                if src_subnet.prefix_len < MIN_V4_MASQUERADE_PREFIX_LEN {
                    return Err(Error::Unsupported);
                }
            }
            fnet::IpAddress::Ipv6(_) => {
                if src_subnet.prefix_len < MIN_V6_MASQUERADE_PREFIX_LEN {
                    return Err(Error::Unsupported);
                }
            }
        }
        Ok(Self {
            src_subnet: src_subnet.try_into().map_err(|_| Error::InvalidArguments)?,
            output_interface: InterfaceId::new(output_interface).ok_or(Error::InvalidArguments)?,
        })
    }
}

/// State of a masquerade configuration.
#[derive(Clone, Debug)]
enum MasqueradeFilterState {
    /// The masquerade config is inactive.
    Inactive,
    /// The masquerade config is active in `fuchsia.net.filter`.
    Active { rule: RuleId },
}

impl MasqueradeFilterState {
    fn is_active(&self) -> bool {
        match self {
            MasqueradeFilterState::Inactive => false,
            MasqueradeFilterState::Active { rule: _ } => true,
        }
    }
}

#[derive(Debug, Clone)]
struct MasqueradeState {
    filter_state: MasqueradeFilterState,
    control: fnet_masquerade::ControlControlHandle,
}

impl MasqueradeState {
    fn new(control: fnet_masquerade::ControlControlHandle) -> Self {
        Self { filter_state: MasqueradeFilterState::Inactive, control }
    }
}

// Convert errors observed on `fuchsia.net.filter` to errors on the Masquerade
// API.
impl From<FilterError> for Error {
    fn from(error: FilterError) -> Error {
        match error {
            FilterError::Push(e) => {
                error!("failed to push filtering changes: {e}");
                match e {
                    PushChangesError::CallMethod(e) => crate::exit_with_fidl_error(e),
                    PushChangesError::TooManyChanges
                    | PushChangesError::FidlConversion(_)
                    | PushChangesError::ErrorOnChange(_) => {
                        panic!("failed to push: generated filtering state was invalid.")
                    }
                }
            }
            FilterError::Commit(e) => {
                error!("failed to commit filtering changes: {e}");
                match e {
                    CommitError::CallMethod(e) => crate::exit_with_fidl_error(e),
                    CommitError::CyclicalRoutineGraph(_)
                    | CommitError::MasqueradeWithInvalidMatcher(_)
                    | CommitError::TransparentProxyWithInvalidMatcher(_)
                    | CommitError::RedirectWithInvalidMatcher(_)
                    | CommitError::RuleWithInvalidAction(_)
                    | CommitError::RuleWithInvalidMatcher(_)
                    | CommitError::RejectWithInvalidMatcher(_)
                    | CommitError::ErrorOnChange(_)
                    | CommitError::FidlConversion(_) => {
                        panic!("failed to commit: generated filtering state was invalid.")
                    }
                }
            }
        }
    }
}

/// Adds or removes a masquerade rule.
///
/// If the existing state is inactive, a rule will be added. Otherwise, the
/// existing rule is removed.
async fn add_or_remove_masquerade_rule(
    filter: &mut FilterControl,
    config: ValidatedConfig,
    existing_state: &MasqueradeFilterState,
) -> Result<MasqueradeFilterState, Error> {
    let ValidatedConfig { src_subnet, output_interface } = config;
    match existing_state {
        MasqueradeFilterState::Inactive => {
            let rule = crate::filter::add_masquerade_rule(
                filter,
                Matchers {
                    out_interface: Some(fnet_matchers_ext::Interface::Id(output_interface.into())),
                    src_addr: Some(fnet_matchers_ext::Address {
                        matcher: fnet_matchers_ext::AddressMatcherType::Subnet(src_subnet),
                        invert: false,
                    }),
                    ..Default::default()
                },
            )
            .await
            .map_err(Error::from)?;
            Ok(MasqueradeFilterState::Active { rule })
        }
        MasqueradeFilterState::Active { rule } => {
            crate::filter::remove_masquerade_rule(filter, rule).await.map_err(Error::from)?;
            Ok(MasqueradeFilterState::Inactive)
        }
    }
}

#[derive(Debug, Default)]
pub(super) struct MasqueradeHandler {
    active_controllers: HashMap<ValidatedConfig, MasqueradeState>,
}

impl MasqueradeHandler {
    async fn set_enabled(
        &mut self,
        filter: &mut FilterControl,
        config: ValidatedConfig,
        enabled: bool,
    ) -> Result<bool, Error> {
        let state = self.active_controllers.get_mut(&config).ok_or(Error::InvalidArguments)?;

        let original_state = state.filter_state.is_active();
        if original_state == enabled {
            // The current state is already the desired state; short circuit.
            return Ok(original_state);
        }
        let new_state = add_or_remove_masquerade_rule(filter, config, &state.filter_state).await?;

        state.filter_state = new_state;
        Ok(original_state)
    }

    /// Attempts to create a new fuchsia_net_masquerade/Control connection.
    ///
    /// On error, returns the original control handle back so that the caller
    /// may terminate the connection.
    fn create_control(
        &mut self,
        config: ValidatedConfig,
        control: fnet_masquerade::ControlControlHandle,
    ) -> Result<(), (Error, fnet_masquerade::ControlControlHandle)> {
        match self.active_controllers.entry(config) {
            std::collections::hash_map::Entry::Vacant(e) => {
                // No need to modify the just-added state.
                let _: &mut MasqueradeState = e.insert(MasqueradeState::new(control));
                Ok(())
            }
            // TODO(https://fxbug.dev/374287551): At the moment, new controllers
            // are rejected if their configuration exactly matches an existing
            // controller. However, it would be preferable to also reject
            // controllers that specify an overlapping configuration. E.g. a
            // subnet that overlaps with an existing subnet on the same
            // interface.
            std::collections::hash_map::Entry::Occupied(_) => Err((Error::AlreadyExists, control)),
        }
    }

    /// Locally cleans up masquerade controllers and shuts down client channels
    /// for a removed interface.
    ///
    /// Netstack automatically removes the associated rules, so no Netstack calls
    /// are needed. Concurrent client disconnects will safely no-op when their
    /// queued `Disconnect` events find the controller already removed.
    pub(super) fn handle_interface_removed(&mut self, interface_id: InterfaceId) {
        self.active_controllers.retain(|config, state| {
            if config.output_interface == interface_id {
                state.control.shutdown_with_epitaph(fidl::Status::NOT_FOUND);
                false
            } else {
                true
            }
        });
    }

    pub(super) async fn handle_event(
        &mut self,
        event: Event,
        events: &mut futures::stream::SelectAll<EventStream>,
        filter: &mut FilterControl,
    ) {
        match event {
            Event::FactoryRequestStream(stream) => events.push(
                stream.try_filter_map(|r| future::ok(Some(Event::FactoryRequest(r)))).boxed(),
            ),
            Event::FactoryRequest(fnet_masquerade::FactoryRequest::Create {
                config,
                control,
                responder,
            }) => {
                let (stream, control) = control.into_stream_and_control_handle();
                let config = match ValidatedConfig::try_from(config) {
                    Ok(config) => config,
                    Err(e) => {
                        control.respond_and_maybe_shutdown(Err(e), |r| {
                            let _: Result<(), fidl::Error> = responder.send(r);
                            // N.B. we always return Ok here because we don't
                            // want to shut down the Control handle if replying
                            // to the Factory request fails.
                            Ok(())
                        });
                        return;
                    }
                };
                match self.create_control(config, control) {
                    Ok(()) => {
                        if let Err(e) = responder.send(Ok(())) {
                            error!("failed to notify control of successful creation: {e:?}");
                        }
                        events.push(
                            stream
                                .try_filter_map(move |r| {
                                    future::ok(Some(Event::ControlRequest(config, r)))
                                })
                                // Note: chaining a disconnect event onto the back of
                                // the stream allows us to cleanup `active_controllers`
                                // when the client hangs up.
                                .chain(futures::stream::once(future::ok(Event::Disconnect(config))))
                                .boxed(),
                        );
                    }
                    Err((e, control)) => {
                        warn!("failed to create control: {e:?}");
                        control.respond_and_maybe_shutdown(Err(e), |r| responder.send(r));
                    }
                }
            }
            Event::ControlRequest(
                config,
                fnet_masquerade::ControlRequest::SetEnabled { enabled, responder },
            ) => {
                let response = self.set_enabled(filter, config.clone(), enabled).await;
                if let Some(state) = self.active_controllers.get_mut(&config) {
                    state.respond_and_maybe_shutdown(response, |r| responder.send(r));
                } else {
                    // Controller removed by interface teardown while request was in-flight.
                    debug!(
                        "masquerade controller for {config:?} was removed while processing \
                         SetEnabled"
                    );
                }
            }
            Event::Disconnect(config) => {
                match self.set_enabled(filter, config, false).await {
                    Ok(_prev_enabled) => {
                        // Disable succeeded; remove controller from tracking.
                        if self.active_controllers.remove(&config).is_none() {
                            error!(
                                "masquerade controller for {config:?} was unexpectedly \
                                 missing on disconnect"
                            );
                        }
                    }
                    // The controller was already cleaned up by
                    // `handle_interface_removed` due to a race between the
                    // interface removal and the client disconnect.
                    Err(Error::InvalidArguments) => {}
                    // Interface is gone (Netstack failed to disable), but
                    // controller is still tracked. Clean it up.
                    Err(Error::NotFound) | Err(Error::BadRule) => {
                        let _removed_controller: Option<MasqueradeState> =
                            self.active_controllers.remove(&config);
                    }
                    Err(Error::RetryExceeded) => error!(
                        "Failed to removed masquerade configuration for disconnected client \
                            (RetryExceeded): {config:?}"
                    ),
                    Err(Error::AlreadyExists) | Err(Error::Unsupported) => {
                        panic!("removing existing configuration cannot fail")
                    }
                    Err(Error::__SourceBreaking { unknown_ordinal: _ }) => {}
                }
            }
        }
    }
}

trait RespondAndMaybeShutdown {
    fn respond_and_maybe_shutdown<T: Clone, Sender>(
        &self,
        response: Result<T, fnet_masquerade::Error>,
        sender: Sender,
    ) where
        Sender: FnOnce(Result<T, fnet_masquerade::Error>) -> Result<(), fidl::Error>;
}

fn to_epitaph(e: Error) -> fidl::Status {
    match e {
        Error::Unsupported => fidl::Status::NOT_SUPPORTED,
        Error::InvalidArguments => fidl::Status::INVALID_ARGS,
        Error::NotFound => fidl::Status::NOT_FOUND,
        Error::AlreadyExists => fidl::Status::ALREADY_BOUND,
        Error::BadRule => fidl::Status::BAD_PATH,
        Error::RetryExceeded => fidl::Status::TIMED_OUT,
        e => panic!("Unhandled error {e:?}"),
    }
}

impl RespondAndMaybeShutdown for fnet_masquerade::ControlControlHandle {
    fn respond_and_maybe_shutdown<T: Clone, Sender>(
        &self,
        response: Result<T, fnet_masquerade::Error>,
        sender: Sender,
    ) where
        Sender: FnOnce(Result<T, fnet_masquerade::Error>) -> Result<(), fidl::Error>,
    {
        // This is not a permanent error, and should not cause a shutdown.
        if let Err(err) = sender(response.clone()) {
            error!("Shutting down due to fidl error: {err:?}");
            self.shutdown_with_epitaph(fidl::Status::INTERNAL);
            return;
        }
        if let Err(e) = response {
            match e {
                Error::RetryExceeded => {
                    // This is not a permanent error, and should not cause a shutdown.
                }
                e => {
                    warn!("Shutting down due to permanent error: {e:?}");
                    self.shutdown_with_epitaph(to_epitaph(e));
                }
            }
        }
    }
}

impl RespondAndMaybeShutdown for MasqueradeState {
    fn respond_and_maybe_shutdown<T: Clone, Sender>(
        &self,
        response: Result<T, fnet_masquerade::Error>,
        sender: Sender,
    ) where
        Sender: FnOnce(Result<T, fnet_masquerade::Error>) -> Result<(), fidl::Error>,
    {
        self.control.respond_and_maybe_shutdown(response, sender)
    }
}

#[cfg(test)]
pub mod test {
    use fuchsia_sync::Mutex;
    use net_declare::fidl_subnet;
    use std::sync::Arc;

    use assert_matches::assert_matches;
    use fidl_fuchsia_net_filter::{ControlRequest, NamespaceControllerRequest};
    use fidl_fuchsia_net_filter_ext::{Action, Change, Resource, ResourceId};
    use futures::FutureExt;
    use futures::future::FusedFuture;
    use test_case::test_case;

    use super::*;

    const VALID_OUTPUT_INTERFACE: u64 = 11;

    const V4_UNSPECIFIED_SUBNET: fnet::Subnet = fidl_subnet!("0.0.0.0/0");
    const V6_UNSPECIFIED_SUBNET: fnet::Subnet = fidl_subnet!("::/0");
    const V4_SLASH_7_SUBNET: fnet::Subnet = fidl_subnet!("0.0.0.0/7");
    const V4_SLASH_8_SUBNET: fnet::Subnet = fidl_subnet!("0.0.0.0/8");
    const V6_SLASH_31_SUBNET: fnet::Subnet = fidl_subnet!("::/31");
    const V6_SLASH_32_SUBNET: fnet::Subnet = fidl_subnet!("::/32");

    const VALID_SUBNET: fnet::Subnet = fidl_subnet!("192.0.2.0/24");
    // Note: Invalid because the host-bits are set.
    const INVALID_SUBNET: fnet::Subnet = fidl_subnet!("192.0.2.1/24");

    const DEFAULT_CONFIG: fnet_masquerade::ControlConfig = fnet_masquerade::ControlConfig {
        src_subnet: VALID_SUBNET,
        output_interface: VALID_OUTPUT_INTERFACE,
    };

    /// A mock implementation of `fuchsia.net.filter`.
    #[derive(Default)]
    struct MockFilterState {
        pending_changes: Vec<Change>,
        resources: HashMap<ResourceId, Resource>,
    }

    impl MockFilterState {
        fn handle_request(&mut self, req: NamespaceControllerRequest) {
            match req {
                NamespaceControllerRequest::PushChanges { changes, responder } => {
                    let changes = changes
                        .into_iter()
                        .map(|change| Change::try_from(change).expect("invalid change"));
                    self.pending_changes.extend(changes);
                    responder
                        .send(fidl_fuchsia_net_filter::ChangeValidationResult::Ok(
                            fidl_fuchsia_net_filter::Empty,
                        ))
                        .expect("failed to respond");
                }
                NamespaceControllerRequest::Commit { payload: _, responder } => {
                    for change in self.pending_changes.drain(..) {
                        match change {
                            Change::Create(resource) => {
                                let id = resource.id();
                                assert_matches!(
                                    self.resources.insert(id.clone(), resource),
                                    None,
                                    "resource {id:?} already exists"
                                );
                            }
                            Change::Remove(resource) => {
                                assert_matches!(
                                    self.resources.remove(&resource),
                                    Some(_),
                                    "resource {resource:?} does not exist"
                                );
                            }
                        }
                    }
                    responder
                        .send(fidl_fuchsia_net_filter::CommitResult::Ok(
                            fidl_fuchsia_net_filter::Empty,
                        ))
                        .expect("failed to respond");
                }
                _ => unimplemented!("fuchsia.net.filter mock called with unsupported request"),
            }
        }
    }

    #[derive(Clone, Default)]
    pub(crate) struct MockFilter(Arc<Mutex<MockFilterState>>);

    impl MockFilter {
        // Lists the masquerade configurations that are currently installed.
        fn list_configurations(&self) -> Vec<fnet_masquerade::ControlConfig> {
            self.0
                .lock()
                .resources
                .values()
                .filter_map(|resource| match resource {
                    Resource::Rule(rule) => match rule.action {
                        Action::Masquerade { src_port: _ } => {
                            let output_interface = rule
                                .matchers
                                .out_interface
                                .clone()
                                .expect("out_interface should be Some");
                            let output_interface = match output_interface {
                                fnet_matchers_ext::Interface::Id(value) => value.get(),
                                matcher => panic!("unexpected interface matcher: {matcher:?}"),
                            };
                            let src_subnet =
                                rule.matchers.src_addr.clone().expect("src_addr should be Some");
                            assert!(!src_subnet.invert);
                            let src_subnet = match src_subnet.matcher {
                                fnet_matchers_ext::AddressMatcherType::Subnet(value) => {
                                    value.into()
                                }
                                matcher => panic!("unexpected address matcher: {matcher:?}"),
                            };
                            Some(fnet_masquerade::ControlConfig { output_interface, src_subnet })
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .collect()
        }

        // Returns true if the provided interface is active.
        fn is_interface_active(&self, interface_id: u64) -> bool {
            self.list_configurations().iter().any(|config| config.output_interface == interface_id)
        }

        /// Create a client (`FilterControl`), and server (future) from a mock.
        ///
        /// The server future must be polled in order for operations against the
        /// client to make progress.
        pub(crate) async fn into_client_and_server(
            self,
        ) -> (FilterControl, impl FusedFuture<Output = ()>) {
            // Note: we have to go through `fuchsia.net.filter/Control` to
            // get a connection to `fuchsia.net.filter/NamespaceController`.
            let (control_client, control_server) =
                fidl::endpoints::create_endpoints::<fidl_fuchsia_net_filter::ControlMarker>();
            let client_fut = FilterControl::new(control_client.into_proxy())
                .map(|result| result.expect("error creating controller"));
            let mut client_fut = std::pin::pin!(client_fut);
            assert!(client_fut.as_mut().now_or_never().is_none());
            let mut control_stream = control_server.into_stream();
            let control_server_fut = control_stream.next().map(|req| {
                match req.expect("stream shouldn't close").expect("stream shouldn't have an error")
                {
                    ControlRequest::OpenController { id, request, control_handle: _ } => {
                        let (request_stream, control_handle) =
                            request.into_stream_and_control_handle();
                        control_handle.send_on_id_assigned(id.as_str()).expect("failed to respond");
                        request_stream
                    }
                    ControlRequest::ReopenDetachedController {
                        key: _,
                        request: _,
                        control_handle: _,
                    } => unimplemented!("fuchsia.net.filter mock called with unsupported request"),
                }
            });
            let (server_request_stream, client) = futures::join!(control_server_fut, client_fut);

            let server_fut = server_request_stream
                .fold(self.0, |state, req| {
                    state.lock().handle_request(req.expect("failed to receive request"));
                    futures::future::ready(state)
                })
                .map(|_state| ())
                .fuse();
            (client, server_fut)
        }
    }

    #[fuchsia::test]
    async fn enable_disable_masquerade() {
        let config = ValidatedConfig::try_from(DEFAULT_CONFIG).unwrap();

        let mock = MockFilter::default();
        let (mut filter_control, mut server_fut) = mock.clone().into_client_and_server().await;

        let mut masq = MasqueradeHandler::default();
        let (_client, server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_net_masquerade::ControlMarker>();
        let (_request_stream, control) = server.into_stream_and_control_handle();

        assert_matches!(masq.create_control(config, control), Ok(()));

        for (enable, expected_configs) in [(true, vec![DEFAULT_CONFIG]), (false, vec![])] {
            let set_enabled_fut = masq.set_enabled(&mut filter_control, config, enable).fuse();
            futures::pin_mut!(set_enabled_fut);
            let response = futures::select!(
                r = set_enabled_fut => r,
                () = server_fut => panic!("mock filter server should never exit"),
            );
            assert_eq!(response, Ok(!enable));
            assert_eq!(mock.list_configurations(), expected_configs);
            assert_eq!(mock.is_interface_active(DEFAULT_CONFIG.output_interface), enable);
        }
    }

    #[fuchsia::test]
    async fn interface_removed() {
        let config = ValidatedConfig::try_from(DEFAULT_CONFIG).unwrap();

        let mock = MockFilter::default();
        let (mut filter_control, mut server_fut) = mock.clone().into_client_and_server().await;

        let mut masq = MasqueradeHandler::default();
        let (client, server) =
            fidl::endpoints::create_endpoints::<fidl_fuchsia_net_masquerade::ControlMarker>();
        let (_request_stream, control) = server.into_stream_and_control_handle();

        assert_matches!(masq.create_control(config, control), Ok(()));

        // Enable masquerading for this config first.
        {
            let set_enabled_fut = masq.set_enabled(&mut filter_control, config, true).fuse();
            futures::pin_mut!(set_enabled_fut);
            let response = futures::select!(
                r = set_enabled_fut => r,
                () = server_fut => panic!("mock filter server should never exit"),
            );
            assert_eq!(response, Ok(false));
        }
        assert_eq!(mock.list_configurations(), vec![DEFAULT_CONFIG]);
        assert_eq!(mock.is_interface_active(DEFAULT_CONFIG.output_interface), true);

        // Now trigger interface removal.
        masq.handle_interface_removed(InterfaceId::new(DEFAULT_CONFIG.output_interface).unwrap());

        // Verify the controller was removed.
        assert!(masq.active_controllers.is_empty());

        // Verify the FIDL control handle channel was closed with NOT_FOUND.
        let client_proxy = client.into_proxy();
        let event = client_proxy.take_event_stream().next().await;
        assert_matches!(
            event,
            Some(Err(fidl::Error::ClientChannelClosed { epitaph, .. }))
                if epitaph == fidl::Status::NOT_FOUND
        );
    }

    #[test_case(
        DEFAULT_CONFIG => Ok(());
        "valid_config"
    )]
    #[test_case(
        fnet_masquerade::ControlConfig {
            src_subnet: V4_UNSPECIFIED_SUBNET,
            .. DEFAULT_CONFIG
        } => Err(Error::Unsupported);
        "v4_unspecified_subnet"
    )]
    #[test_case(
        fnet_masquerade::ControlConfig {
            src_subnet: V6_UNSPECIFIED_SUBNET,
            .. DEFAULT_CONFIG
        } => Err(Error::Unsupported);
        "v6_unspecified_subnet"
    )]
    #[test_case(
        fnet_masquerade::ControlConfig {
            src_subnet: V4_SLASH_7_SUBNET,
            .. DEFAULT_CONFIG
        } => Err(Error::Unsupported);
        "v4_slash_7_subnet"
    )]
    #[test_case(
        fnet_masquerade::ControlConfig {
            src_subnet: V4_SLASH_8_SUBNET,
            .. DEFAULT_CONFIG
        } => Ok(());
        "v4_slash_8_subnet"
    )]
    #[test_case(
        fnet_masquerade::ControlConfig {
            src_subnet: V6_SLASH_31_SUBNET,
            .. DEFAULT_CONFIG
        } => Err(Error::Unsupported);
        "v6_slash_31_subnet"
    )]
    #[test_case(
        fnet_masquerade::ControlConfig {
            src_subnet: V6_SLASH_32_SUBNET,
            .. DEFAULT_CONFIG
        } => Ok(());
        "v6_slash_32_subnet"
    )]
    #[test_case(
        fnet_masquerade::ControlConfig {
            src_subnet: INVALID_SUBNET,
            .. DEFAULT_CONFIG
        } => Err(Error::InvalidArguments);
        "invalid_subnet"
    )]
    #[test_case(
        fnet_masquerade::ControlConfig {
            output_interface: 0,
            .. DEFAULT_CONFIG
        } => Err(Error::InvalidArguments);
        "invalid_output_interface"
    )]
    #[fuchsia::test]
    fn validate_config(config: fnet_masquerade::ControlConfig) -> Result<(), Error> {
        ValidatedConfig::try_from(config).map(|_| ())
    }
}
