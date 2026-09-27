// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod serial;

use serial::{SerialConnection, SerialService};

use fdf_component::{
    Driver, DriverContext, DriverError, Node, NodeBuilder, ServiceOffer, driver_register,
};
use fidl_next_fuchsia_hardware_serialimpl as serialimpl;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use futures::StreamExt;
use log::info;

pub const CHILD_NODE_NAME: &str = "bt-transport-uart";
pub const HCI_SERVICE_NAME: &str = "fuchsia.hardware.bluetooth.HciService";

/// Bluetooth HCI UART transport driver in Rust.
pub struct BtTransportUart {
    _node: Node,
    scope: fasync::Scope,
    serial: SerialConnection,
}

impl BtTransportUart {
    /// Returns the PID reported by the parent serial device.
    pub fn serial_pid(&self) -> u32 {
        self.serial.serial_pid()
    }
}

impl std::fmt::Debug for BtTransportUart {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BtTransportUart").field("serial", &self.serial).finish_non_exhaustive()
    }
}

driver_register!(BtTransportUart);

impl Driver for BtTransportUart {
    const NAME: &str = "bt-transport-uart-rust";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        info!("BtTransportUart (Rust)::start() invoked");
        let node = context.take_node()?;
        let scope = fasync::Scope::new();

        let serial = SerialConnection::connect_and_validate(&context, scope.as_handle()).await?;

        let mut outgoing = ServiceFs::new();
        let serial_offer = ServiceOffer::<serialimpl::Service>::new_next()
            .add_default_named_next(
                &mut outgoing,
                "default",
                SerialService::new(serial.clone(), scope.to_handle()),
            )
            .build_driver_offer();

        context.serve_outgoing(&mut outgoing)?;
        scope.spawn(outgoing.collect());

        let child_node = NodeBuilder::new(CHILD_NODE_NAME)
            .add_property(bind_fuchsia::SERVICE, HCI_SERVICE_NAME)
            .add_offer(serial_offer)
            .build();
        node.add_child(child_node).await?;

        Ok(Self { _node: node, scope, serial })
    }

    async fn stop(&self) {
        info!("BtTransportUart (Rust)::stop() invoked");
        self.serial.cancel_all().await;
        self.serial.close();
        self.scope.to_handle().cancel().await;
    }
}

#[cfg(test)]
mod tests;
