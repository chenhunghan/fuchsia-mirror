// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use super::*;

use anyhow::Error;
use fuchsia_component::client::connect_to_protocol;
use openthread::ot::{Ip4Cidr, Nat64State};

const NAT64_IP4CIDR_DEFAULT_SUBNET: [u8; 4] = [240, 16, 0, 0];
const NAT64_IP4CIDR_DEFAULT_PREFIX_LEN: u8 = 24;

/// A failed `fuchsia.net.masquerade/Control.SetEnabled` call.
#[derive(thiserror::Error, Debug)]
enum SetEnabledError {
    /// netcfg rejected the request, after which it closes the Control channel.
    #[error("masquerade SetEnabled failed with {0:?}")]
    Rejected(fidl_fuchsia_net_masquerade::Error),
    #[error("masquerade SetEnabled FIDL error")]
    Fidl(#[source] fidl::Error),
}

#[derive(Debug)]
struct MasqueradeControl {
    proxy: fidl_fuchsia_net_masquerade::ControlProxy,
    config: fidl_fuchsia_net_masquerade::ControlConfig,
}

impl MasqueradeControl {
    async fn set_enabled(&self, enabled: bool) -> Result<(), SetEnabledError> {
        match self.proxy.set_enabled(enabled).await {
            Ok(Ok(_was_enabled)) => Ok(()),
            Ok(Err(err)) => Err(SetEnabledError::Rejected(err)),
            Err(err) => Err(SetEnabledError::Fidl(err)),
        }
    }
}

/// OpenThread NAT64 translator state, copied out from under the driver state
/// lock so that masquerade FIDL calls can be awaited without holding it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Nat64TranslatorSnapshot {
    cidr: Option<Ip4Cidr>,
    state: Nat64State,
}

impl Nat64TranslatorSnapshot {
    fn read<OT: ot::InstanceInterface>(ot_instance: &OT) -> Self {
        Self {
            cidr: ot_instance.nat64_get_cidr().ok(),
            state: ot_instance.nat64_get_translator_state(),
        }
    }
}

#[derive(Debug)]
pub struct Nat64 {
    lowpan_nat_ctrl: Option<MasqueradeControl>,
    active_cidr_addr: Option<Ip4Cidr>,
}

pub fn get_first_usable_addr_in_subnet(subnet: [u8; 4], prefix_len: u8) -> [u8; 4] {
    // Stop the lowpan-ot-driver for earier debugging
    assert!(prefix_len <= 32, "IP subnet prefix is too long");

    if prefix_len == 32 {
        return subnet;
    }

    let mask: u32 = !((1u64 << (32 - prefix_len)) - 1) as u32;

    // Mask out the subnet bits and then add one to represent the first usable address
    ((u32::from_be_bytes(subnet) & mask) + 1).to_be_bytes()
}

impl<OT, NI, BI> OtDriver<OT, NI, BI>
where
    OT: Send + ot::InstanceInterface,
    NI: NetworkInterface,
    BI: BackboneInterface,
{
    pub async fn on_nat64_translator_state_changed(&self) {
        let Some(wlan_client_nicid) = self.backbone_if.get_nicid() else {
            return;
        };

        // Serializes NAT64 reconciliation across the masquerade FIDL calls
        // below. Lock order: `nat64` before `driver_state`, and `driver_state`
        // is never held across an await.
        let mut nat64 = self.nat64.lock().await;

        let snapshot = {
            let driver_state = self.driver_state.lock();
            Nat64TranslatorSnapshot::read(&driver_state.ot_instance)
        };

        let reconcile_result = nat64
            .reconcile(snapshot, &self.net_if, wlan_client_nicid.into(), || {
                connect_to_protocol::<fidl_fuchsia_net_masquerade::FactoryMarker>()
                    .context("error connecting to masquerade factory")
            })
            .await;
        match reconcile_result {
            Ok(()) => {}
            Err(err) => {
                let driver_state = self.driver_state.lock();
                if Nat64TranslatorSnapshot::read(&driver_state.ot_instance) != snapshot {
                    // OpenThread changed while masquerade was being configured,
                    // which signals another NAT64 translator state change that
                    // will reconcile again.
                    warn!(err:?; "NAT64: failed to process stale NAT64 changes, ignoring");
                    return;
                }
                warn!(
                    err:?;
                    "NAT64: failed to process NAT64 changes, disabling NAT64 in OpenThread"
                );
                driver_state.ot_instance.nat64_set_enabled(false);
            }
        }
    }
}

impl Nat64 {
    pub fn new() -> Self {
        Nat64 { lowpan_nat_ctrl: None, active_cidr_addr: None }
    }

    async fn reconcile<NI, F>(
        &mut self,
        snapshot: Nat64TranslatorSnapshot,
        net_if: &NI,
        wlan_client_nicid: u64,
        connect_factory: F,
    ) -> Result<(), Error>
    where
        NI: NetworkInterface,
        F: FnOnce() -> Result<fidl_fuchsia_net_masquerade::FactoryProxy, Error>,
    {
        let Some(translator_cidr) = snapshot.cidr else {
            // `init_ot` configures a CIDR before any state change is processed,
            // so this only happens if that failed or the CIDR was cleared.
            warn!(state:? = snapshot.state; "NAT64: translator has no CIDR configured, ignoring");
            return Ok(());
        };

        if self.active_cidr_addr != Some(translator_cidr) {
            // Someone sets a new CIDR for NAT64. Drop the old route and close
            // the previous masquerade Control channel since src_subnet is
            // immutable per Control channel.
            if let Some(addr) = self.active_cidr_addr {
                self.delete_route(net_if, addr.get_address_bytes(), addr.get_length());
                // TODO(https://fxbug.dev/521638425): Close the previous Control
                // channel with a two-way call so netcfg has removed its
                // configuration before a later Factory.Create.
                drop(self.lowpan_nat_ctrl.take());
            }
            self.active_cidr_addr = Some(translator_cidr);
            info!("NAT64: CIDR updated to {:?}", translator_cidr);
        }

        let cidr_addr = translator_cidr.get_address_bytes();
        let cidr_prefix_len = translator_cidr.get_length();
        if snapshot.state == Nat64State::Active {
            self.add_route(net_if, cidr_addr, cidr_prefix_len);
            self.enable_masquerade(wlan_client_nicid, cidr_addr, cidr_prefix_len, connect_factory)
                .await?;
            info!("NAT64: adding route for NAT64");
        } else {
            self.delete_route(net_if, cidr_addr, cidr_prefix_len);
            self.disable_masquerade().await;
            info!("NAT64: deleting route for NAT64");
        }

        Ok(())
    }

    async fn enable_masquerade<F>(
        &mut self,
        wlan_client_nicid: u64,
        cidr_addr: [u8; 4],
        cidr_addr_prefix_len: u8,
        connect_factory: F,
    ) -> Result<(), Error>
    where
        F: FnOnce() -> Result<fidl_fuchsia_net_masquerade::FactoryProxy, Error>,
    {
        let config = fidl_fuchsia_net_masquerade::ControlConfig {
            // The network to be masqueraded.
            src_subnet: fidl_fuchsia_net::Subnet {
                addr: fidl_fuchsia_net::IpAddress::Ipv4(fidl_fuchsia_net::Ipv4Address {
                    addr: cidr_addr,
                }),
                prefix_len: cidr_addr_prefix_len,
            },
            // The interface through which to masquerade.[Wlan Client]
            output_interface: wlan_client_nicid,
        };

        // Reuse the existing Control channel when the masquerade configuration
        // is unchanged. Sending SetEnabled requests over a single channel
        // serializes enable/disable transitions in FIFO order and avoids racing
        // a new Factory.Create call against the asynchronous teardown of a
        // previous Control channel in netcfg.
        if let Some(ctrl) = self.lowpan_nat_ctrl.as_ref().filter(|ctrl| ctrl.config == config) {
            match ctrl.set_enabled(true).await {
                Ok(()) => {
                    info!("NAT64: masquerade enabled with {:?}", config);
                    return Ok(());
                }
                // A closed channel means netcfg has torn down this masquerade
                // configuration, so create a new one below.
                Err(SetEnabledError::Fidl(err)) if err.is_closed() => {
                    warn!(err:?; "NAT64: masquerade control channel closed, recreating it");
                }
                Err(err) => {
                    self.lowpan_nat_ctrl = None;
                    return Err(err.into());
                }
            }
        }

        // Drop any previous Control channel before creating a replacement so
        // netcfg can observe peer-closed on the old channel as early as
        // possible.
        // TODO(https://fxbug.dev/521638425): Close the previous Control channel
        // with a two-way call so netcfg has removed its configuration before
        // the Factory.Create below.
        drop(self.lowpan_nat_ctrl.take());

        let masquerade_proxy = connect_factory()?;
        let (proxy, server_end) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_net_masquerade::ControlMarker>();
        masquerade_proxy
            .create(&config, server_end)
            .await
            .context("masquerade Factory.Create FIDL error")?
            .map_err(|err| format_err!("masquerade Factory.Create failed with {err:?}"))?;

        let ctrl = MasqueradeControl { proxy, config };
        ctrl.set_enabled(true).await?;

        info!("NAT64: masquerade configured with {:?}", ctrl.config);

        let prev_ctrl = self.lowpan_nat_ctrl.replace(ctrl);
        assert!(prev_ctrl.is_none());

        Ok(())
    }

    // Errors are logged rather than returned: the NAT64 translator is already
    // inactive, so there is nothing to disable in OpenThread.
    async fn disable_masquerade(&mut self) {
        // Keep the Control channel open and toggle SetEnabled(false) so
        // subsequent enable/disable calls remain serialized on the same
        // channel instead of racing a channel drop against a future
        // Factory.Create call.
        let Some(ctrl) = self.lowpan_nat_ctrl.as_ref() else {
            return;
        };
        match ctrl.set_enabled(false).await {
            Ok(()) => info!("NAT64: masquerade config disabled"),
            Err(err) => {
                warn!(err:?; "NAT64: failed to disable masquerade, clearing control channel");
                self.lowpan_nat_ctrl = None;
            }
        }
    }

    fn add_route<NI: NetworkInterface>(
        &self,
        net_if: &NI,
        cidr_addr: [u8; 4],
        cidr_addr_prefix_len: u8,
    ) {
        let addr = fidl_fuchsia_net::Subnet {
            addr: fidl_fuchsia_net::IpAddress::Ipv4(fidl_fuchsia_net::Ipv4Address {
                addr: get_first_usable_addr_in_subnet(cidr_addr, cidr_addr_prefix_len),
            }),
            prefix_len: cidr_addr_prefix_len,
        };
        if let Err(err) = net_if.add_forwarding_entry(addr).ignore_already_exists() {
            warn!("Unable to add NAT64 route `{:?}` to lowpan interface: {:?}", addr, err);
        }
    }

    fn delete_route<NI: NetworkInterface>(
        &self,
        net_if: &NI,
        cidr_addr: [u8; 4],
        cidr_addr_prefix_len: u8,
    ) {
        let addr = fidl_fuchsia_net::Subnet {
            addr: fidl_fuchsia_net::IpAddress::Ipv4(fidl_fuchsia_net::Ipv4Address {
                addr: get_first_usable_addr_in_subnet(cidr_addr, cidr_addr_prefix_len),
            }),
            prefix_len: cidr_addr_prefix_len,
        };
        if let Err(err) = net_if.remove_forwarding_entry(addr).ignore_not_found() {
            warn!("Unable to delete NAT64 route `{:?}` to lowpan interface: {:?}", addr, err);
        }
    }

    pub fn get_default_nat64_cidr() -> Ip4Cidr {
        Ip4Cidr::new(NAT64_IP4CIDR_DEFAULT_SUBNET, NAT64_IP4CIDR_DEFAULT_PREFIX_LEN)
    }
}

#[cfg(test)]
mod test {
    use super::*;
    use assert_matches::assert_matches;
    use fidl::endpoints::create_proxy_and_stream;
    use fidl_fuchsia_net_masquerade::{
        ControlConfig, ControlRequest, ControlRequestStream, Error as MasqueradeError,
        FactoryMarker, FactoryRequest, FactoryRequestStream,
    };

    /// Responds successfully to the next `Factory.Create` request and returns
    /// the stream of requests on the created Control channel.
    async fn respond_to_create(factory_stream: &mut FactoryRequestStream) -> ControlRequestStream {
        assert_matches!(
            factory_stream.next().await,
            Some(Ok(FactoryRequest::Create { control, responder, .. })) => {
                responder.send(Ok(())).expect("respond to create");
                control.into_stream()
            }
        )
    }

    /// Expects the next Control request to be `SetEnabled(expected_enabled)`
    /// and responds with `response`.
    async fn respond_to_set_enabled(
        control_stream: &mut ControlRequestStream,
        expected_enabled: bool,
        response: Result<bool, MasqueradeError>,
    ) {
        assert_matches!(
            control_stream.next().await,
            Some(Ok(ControlRequest::SetEnabled { enabled, responder }))
                if enabled == expected_enabled =>
            {
                responder.send(response).expect("respond to set_enabled");
            }
        );
    }

    /// Enables masquerade with the default CIDR through a fresh Factory and
    /// returns the stream of requests on the created Control channel.
    async fn enable_default_masquerade(nat64: &mut Nat64) -> ControlRequestStream {
        let (factory_proxy, mut factory_stream) = create_proxy_and_stream::<FactoryMarker>();
        let (enable_res, control_stream) = futures::join!(
            nat64.enable_masquerade(
                1,
                NAT64_IP4CIDR_DEFAULT_SUBNET,
                NAT64_IP4CIDR_DEFAULT_PREFIX_LEN,
                || Ok(factory_proxy),
            ),
            async {
                let mut control_stream = respond_to_create(&mut factory_stream).await;
                respond_to_set_enabled(&mut control_stream, true, Ok(false)).await;
                control_stream
            },
        );
        enable_res.expect("initial enable_masquerade should succeed");
        control_stream
    }

    #[test]
    fn test_get_first_usable_addr_in_subnet() {
        assert_eq!(
            get_first_usable_addr_in_subnet([0xff, 0xff, 0xff, 0xff], 32),
            [0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(
            get_first_usable_addr_in_subnet([0xff, 0xff, 0xff, 0xff], 31),
            [0xff, 0xff, 0xff, 0xff]
        );
        assert_eq!(
            get_first_usable_addr_in_subnet([0xff, 0xff, 0xff, 0xff], 28),
            [0xff, 0xff, 0xff, 0xf1]
        );
        assert_eq!(
            get_first_usable_addr_in_subnet([0xff, 0xff, 0xff, 0xff], 24),
            [0xff, 0xff, 0xff, 0x1]
        );
        assert_eq!(
            get_first_usable_addr_in_subnet([0xff, 0xff, 0xff, 0xff], 8),
            [0xff, 0x0, 0x0, 0x1]
        );
        assert_eq!(
            get_first_usable_addr_in_subnet([0xff, 0xff, 0xff, 0xff], 0),
            [0x0, 0x0, 0x0, 0x1]
        );
        assert_eq!(
            get_first_usable_addr_in_subnet(
                NAT64_IP4CIDR_DEFAULT_SUBNET,
                NAT64_IP4CIDR_DEFAULT_PREFIX_LEN
            ),
            [240, 16, 0, 1]
        );
    }

    #[fuchsia::test]
    async fn test_enable_masquerade_reuses_existing_control_proxy_for_same_config() {
        let mut nat64 = Nat64::new();
        let mut control_stream = enable_default_masquerade(&mut nat64).await;

        let (enable_res, ()) = futures::join!(
            nat64.enable_masquerade(
                1,
                NAT64_IP4CIDR_DEFAULT_SUBNET,
                NAT64_IP4CIDR_DEFAULT_PREFIX_LEN,
                || panic!("factory should not be connected when config is unchanged"),
            ),
            respond_to_set_enabled(&mut control_stream, true, Ok(true)),
        );
        enable_res.expect("second enable_masquerade should succeed");
    }

    #[fuchsia::test]
    async fn test_disable_and_reenable_masquerade_reuses_same_control_channel() {
        let mut nat64 = Nat64::new();
        let mut control_stream = enable_default_masquerade(&mut nat64).await;

        let ((), ()) = futures::join!(
            nat64.disable_masquerade(),
            respond_to_set_enabled(&mut control_stream, false, Ok(true)),
        );

        let (enable_res, ()) = futures::join!(
            nat64.enable_masquerade(
                1,
                NAT64_IP4CIDR_DEFAULT_SUBNET,
                NAT64_IP4CIDR_DEFAULT_PREFIX_LEN,
                || panic!("factory should not be connected when re-enabling same config"),
            ),
            respond_to_set_enabled(&mut control_stream, true, Ok(false)),
        );
        enable_res.expect("re-enabling masquerade should succeed");
    }

    #[fuchsia::test]
    async fn test_enable_masquerade_replaces_control_proxy_when_config_changes() {
        let mut nat64 = Nat64::new();
        let mut control_stream_1 = enable_default_masquerade(&mut nat64).await;
        let (factory_proxy_2, mut factory_stream_2) = create_proxy_and_stream::<FactoryMarker>();

        let updated_subnet = [240, 16, 1, 0];
        let (enable_res, ()) = futures::join!(
            nat64.enable_masquerade(1, updated_subnet, NAT64_IP4CIDR_DEFAULT_PREFIX_LEN, || {
                Ok(factory_proxy_2)
            }),
            async {
                assert_matches!(control_stream_1.next().await, None);
                let mut control_stream_2 = assert_matches!(
                    factory_stream_2.next().await,
                    Some(Ok(FactoryRequest::Create { config, control, responder })) => {
                        assert_eq!(
                            config,
                            ControlConfig {
                                src_subnet: fidl_fuchsia_net::Subnet {
                                    addr: fidl_fuchsia_net::IpAddress::Ipv4(
                                        fidl_fuchsia_net::Ipv4Address { addr: updated_subnet },
                                    ),
                                    prefix_len: NAT64_IP4CIDR_DEFAULT_PREFIX_LEN,
                                },
                                output_interface: 1,
                            }
                        );
                        responder.send(Ok(())).expect("respond to second create");
                        control.into_stream()
                    }
                );
                respond_to_set_enabled(&mut control_stream_2, true, Ok(false)).await;
            },
        );
        enable_res.expect("enable_masquerade with new subnet should succeed");
    }

    #[fuchsia::test]
    async fn test_enable_masquerade_recreates_control_proxy_when_channel_closed() {
        let mut nat64 = Nat64::new();
        let control_stream_1 = enable_default_masquerade(&mut nat64).await;
        drop(control_stream_1);
        let (factory_proxy_2, mut factory_stream_2) = create_proxy_and_stream::<FactoryMarker>();

        let (enable_res, ()) = futures::join!(
            nat64.enable_masquerade(
                1,
                NAT64_IP4CIDR_DEFAULT_SUBNET,
                NAT64_IP4CIDR_DEFAULT_PREFIX_LEN,
                || Ok(factory_proxy_2),
            ),
            async {
                let mut control_stream_2 = respond_to_create(&mut factory_stream_2).await;
                respond_to_set_enabled(&mut control_stream_2, true, Ok(false)).await;
            },
        );
        enable_res.expect("enable_masquerade after peer closed should succeed");
    }

    #[fuchsia::test]
    async fn test_enable_masquerade_returns_error_when_create_fails() {
        let mut nat64 = Nat64::new();
        let (factory_proxy, mut factory_stream) = create_proxy_and_stream::<FactoryMarker>();

        let (enable_res, ()) = futures::join!(
            nat64.enable_masquerade(
                1,
                NAT64_IP4CIDR_DEFAULT_SUBNET,
                NAT64_IP4CIDR_DEFAULT_PREFIX_LEN,
                || Ok(factory_proxy),
            ),
            async {
                assert_matches!(
                    factory_stream.next().await,
                    Some(Ok(FactoryRequest::Create { responder, .. })) => {
                        responder
                            .send(Err(MasqueradeError::AlreadyExists))
                            .expect("respond to create");
                    }
                );
            },
        );
        assert_matches!(enable_res, Err(_));
    }

    #[fuchsia::test]
    async fn test_enable_masquerade_drops_control_proxy_when_set_enabled_rejected() {
        let mut nat64 = Nat64::new();
        let (factory_proxy, mut factory_stream) = create_proxy_and_stream::<FactoryMarker>();

        let (enable_res, ()) = futures::join!(
            nat64.enable_masquerade(
                1,
                NAT64_IP4CIDR_DEFAULT_SUBNET,
                NAT64_IP4CIDR_DEFAULT_PREFIX_LEN,
                || Ok(factory_proxy),
            ),
            async {
                let mut control_stream = respond_to_create(&mut factory_stream).await;
                respond_to_set_enabled(
                    &mut control_stream,
                    true,
                    Err(MasqueradeError::InvalidArguments),
                )
                .await;
            },
        );
        assert_matches!(enable_res, Err(_));
        assert_matches!(nat64.lowpan_nat_ctrl, None);
    }
}
