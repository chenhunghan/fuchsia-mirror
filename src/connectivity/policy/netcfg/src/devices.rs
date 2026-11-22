// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::fmt::Display;

use fidl_fuchsia_hardware_network as fhwnet;

use anyhow::Context as _;

use crate::errors::{self, ContextExt as _};
use crate::exit_with_fidl_error;

/// An error when adding a device.
pub(super) enum AddDeviceError {
    AlreadyExists(String),
    Other(errors::Error),
}

impl From<errors::Error> for AddDeviceError {
    fn from(e: errors::Error) -> AddDeviceError {
        AddDeviceError::Other(e)
    }
}

impl errors::ContextExt for AddDeviceError {
    fn context<C>(self, context: C) -> AddDeviceError
    where
        C: Display + Send + Sync + 'static,
    {
        match self {
            AddDeviceError::AlreadyExists(name) => AddDeviceError::AlreadyExists(name),
            AddDeviceError::Other(e) => AddDeviceError::Other(e.context(context)),
        }
    }

    fn with_context<C, F>(self, f: F) -> AddDeviceError
    where
        C: Display + Send + Sync + 'static,
        F: FnOnce() -> C,
    {
        match self {
            AddDeviceError::AlreadyExists(name) => AddDeviceError::AlreadyExists(name),
            AddDeviceError::Other(e) => AddDeviceError::Other(e.with_context(f)),
        }
    }
}

// Cannot be completely replaced by `DeviceInfoRef` due to `get_device_info`
// not being able to return a struct of references.
#[derive(Debug, Clone)]
pub(super) struct DeviceInfo {
    pub(super) port_class: fhwnet::PortClass,
    pub(super) mac: Option<fidl_fuchsia_net_ext::MacAddress>,
    pub(super) bus_info: Vec<fidl_fuchsia_driver_framework::BusInfo>,
}

/// An instance of a network device.
pub(super) struct NetworkDeviceInstance {
    port: fhwnet::PortProxy,
    port_id: fhwnet::PortId,
    device_control: fidl_fuchsia_net_interfaces_admin::DeviceControlProxy,
    bus_info: Vec<fidl_fuchsia_driver_framework::BusInfo>,
}

impl std::fmt::Debug for NetworkDeviceInstance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let NetworkDeviceInstance { port: _, port_id, device_control: _, bus_info } = self;
        write!(f, "NetworkDeviceInstance{{bus_info={:?}, port={:?}}}", bus_info, port_id)
    }
}

impl NetworkDeviceInstance {
    pub async fn get_instance_stream(
        installer: &fidl_fuchsia_net_interfaces_admin::InstallerProxy,
        service: fhwnet::ServiceProxy,
    ) -> Result<impl futures::Stream<Item = Result<Self, errors::Error>> + use<>, errors::Error>
    {
        let device = service
            .connect_to_device()
            .context("cannot connect to device")
            .map_err(errors::Error::NonFatal)?;
        let node_token = service
            .connect_to_token()
            .context("cannot connect to token")
            .map_err(errors::Error::NonFatal)?
            .get()
            .await
            .context("error sending get token request")
            .map_err(errors::Error::NonFatal)?
            .map_err(zx::Status::err_from_raw)
            .context("error getting node token")
            .map_err(errors::Error::NonFatal)?;
        let bus_topology = fuchsia_component::client::connect_to_protocol::<
            fidl_fuchsia_driver_token::NodeBusTopologyMarker,
        >()
        .context("cannot connect to fuchsia.driver.token.NodeBusTopology")
        .map_err(errors::Error::NonFatal)?;
        let bus_info = bus_topology
            .get(node_token)
            .await
            .context("error sending NodeBusTopology.Get request")
            .map_err(errors::Error::NonFatal)?
            .map_err(zx::Status::err_from_raw)
            .context("error getting bus topology")
            .map_err(errors::Error::NonFatal)?;

        let (port_watcher, port_watcher_server_end) =
            fidl::endpoints::create_proxy::<fhwnet::PortWatcherMarker>();
        device
            .get_port_watcher(port_watcher_server_end)
            .context("calling Device get_port_watcher")
            .map_err(errors::Error::NonFatal)?;

        let (device_control, device_control_server_end) = fidl::endpoints::create_proxy::<
            fidl_fuchsia_net_interfaces_admin::DeviceControlMarker,
        >();

        let (device_for_netstack, device_server_end) =
            fidl::endpoints::create_endpoints::<fhwnet::DeviceMarker>();
        device
            .clone(device_server_end)
            .context("calling Device clone")
            .map_err(errors::Error::NonFatal)?;
        installer
            .install_device(device_for_netstack, device_control_server_end)
            // NB: Failing to communicate with installer is a fatal error, that
            // means the Netstack is gone, which we don't tolerate.
            .unwrap_or_else(|err| exit_with_fidl_error(err));

        Ok(futures::stream::try_unfold(
            (port_watcher, device_control, device, bus_info),
            |(port_watcher, device_control, device, bus_info)| async move {
                loop {
                    let port_event = match port_watcher.watch().await {
                        Ok(port_event) => port_event,
                        Err(err) => {
                            break if err.is_closed() {
                                Ok(None)
                            } else {
                                Err(errors::Error::Fatal(err.into()))
                                    .context("calling PortWatcher watch")
                            };
                        }
                    };
                    match port_event {
                        fhwnet::DevicePortEvent::Idle(fhwnet::Empty {}) => {}
                        fhwnet::DevicePortEvent::Removed(port_id) => {
                            let _: fhwnet::PortId = port_id;
                        }
                        fhwnet::DevicePortEvent::Added(port_id)
                        | fhwnet::DevicePortEvent::Existing(port_id) => {
                            let (port, port_server_end) =
                                fidl::endpoints::create_proxy::<fhwnet::PortMarker>();
                            device
                                .get_port(&port_id, port_server_end)
                                .context("calling Device get_port")
                                .map_err(errors::Error::NonFatal)?;
                            break Ok(Some((
                                NetworkDeviceInstance {
                                    port,
                                    port_id,
                                    device_control: device_control.clone(),
                                    bus_info: bus_info.clone(),
                                },
                                (port_watcher, device_control, device, bus_info),
                            )));
                        }
                    }
                }
            },
        ))
    }

    pub async fn get_device_info(&self) -> Result<DeviceInfo, errors::Error> {
        let NetworkDeviceInstance { port, port_id: _, device_control: _, bus_info } = self;
        let fhwnet::PortInfo { id: _, base_info, .. } = port
            .get_info()
            .await
            .context("error getting port info")
            .map_err(errors::Error::NonFatal)?;
        let port_class = base_info
            .ok_or_else(|| errors::Error::Fatal(anyhow::anyhow!("missing base info in port info")))?
            .port_class
            .ok_or_else(|| {
                errors::Error::Fatal(anyhow::anyhow!("missing port class in port base info"))
            })?;

        let (mac_addressing, mac_addressing_server_end) =
            fidl::endpoints::create_proxy::<fhwnet::MacAddressingMarker>();
        port.get_mac(mac_addressing_server_end)
            .context("calling Port get_mac")
            .map_err(errors::Error::NonFatal)?;

        let mac = mac_addressing
            .get_unicast_address()
            .await
            .map(Some)
            .or_else(|fidl_err| {
                if fidl_err.is_closed() { Ok(None) } else { Err(anyhow::Error::from(fidl_err)) }
            })
            .map_err(errors::Error::NonFatal)?;
        Ok(DeviceInfo { port_class, mac: mac.map(Into::into), bus_info: bus_info.clone() })
    }

    pub async fn add_to_stack(
        &self,
        _netcfg: &super::NetCfg<'_>,
        config: crate::InterfaceConfig,
    ) -> Result<(u64, fidl_fuchsia_net_interfaces_ext::admin::Control), AddDeviceError> {
        let NetworkDeviceInstance { port: _, port_id, device_control, bus_info: _ } = self;
        let crate::InterfaceConfig { name, metric, netstack_managed_routes_designation } = config;

        let (control, control_server_end) =
            fidl_fuchsia_net_interfaces_ext::admin::Control::create_endpoints()
                .context("create Control proxy")
                .map_err(errors::Error::NonFatal)?;

        device_control
            .create_interface(
                &port_id,
                control_server_end,
                fidl_fuchsia_net_interfaces_admin::Options {
                    name: Some(name.clone()),
                    metric: Some(metric),
                    netstack_managed_routes_designation: netstack_managed_routes_designation
                        .map(Into::into),
                    ..Default::default()
                },
            )
            .context("calling DeviceControl create_interface")
            .map_err(errors::Error::NonFatal)?;

        let interface_id = control.get_id().await.map_err(|err| {
            let other = match err {
                fidl_fuchsia_net_interfaces_ext::admin::TerminalError::Fidl(err) => err.into(),
                fidl_fuchsia_net_interfaces_ext::admin::TerminalError::Terminal(terminal_error) => {
                    match terminal_error {
                        fidl_fuchsia_net_interfaces_admin::InterfaceRemovedReason::DuplicateName => {
                            return AddDeviceError::AlreadyExists(name);
                        }
                        reason => {
                            anyhow::anyhow!("received terminal event {:?}", reason)
                        }
                    }
                }
            };
            AddDeviceError::Other(
                errors::Error::NonFatal(other).context("calling Control get_id"),
            )
        })?;
        Ok((interface_id, control))
    }
}
