// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fdf_component::{DriverContext, DriverError, ServiceInstance};
use fidl_next_fuchsia_hardware_serial as serial;
use fidl_next_fuchsia_hardware_serialimpl as serialimpl;
use fuchsia_async as fasync;
use log::error;

/// Encapsulates the connection to the parent `fuchsia.hardware.serialimpl.Service` device.
#[derive(Clone)]
pub struct SerialConnection {
    client: fidl_next::Client<serialimpl::Device, fdf_fidl::DriverChannel>,
    serial_pid: u32,
}

impl SerialConnection {
    /// Connects to the parent serial device in `context.incoming`, performs handshake validation,
    /// and enables the device.
    pub async fn connect_and_validate(
        context: &DriverContext,
        scope: &fasync::ScopeHandle,
    ) -> Result<Self, DriverError> {
        let service_proxy: ServiceInstance<serialimpl::Service> =
            context.incoming.service().connect_next().map_err(|status| {
                error!("Failed to connect to incoming serialimpl::Service: {status}");
                DriverError::from(status)
            })?;

        let (client_end, server_end) = fdf_fidl::create_channel();
        service_proxy.device(server_end).map_err(|status| {
            error!("Failed to connect to serialimpl device member: {status}");
            DriverError::from(status)
        })?;

        let client = client_end.spawn_on(scope);

        let info_response = client.get_info().await?;
        let info = match info_response.as_ref() {
            Ok(resp) => resp.info,
            Err(status_res) => {
                let status = status_res.err().unwrap_or(zx::Status::INTERNAL);
                error!("Serial device GetInfo failed with status: {status}");
                return Err(DriverError::from(status));
            }
        };

        if info.serial_class != serial::Class::BluetoothHci {
            error!("Serial device class ({:?}) is not BLUETOOTH_HCI", info.serial_class);
            return Err(DriverError::from(zx::Status::INTERNAL));
        }

        let serial_pid = info.serial_pid;

        let enable_response = client.enable(true).await?;
        if let Err(status_res) = enable_response.as_ref() {
            let status = status_res.err().unwrap_or(zx::Status::INTERNAL);
            error!("Serial device Enable failed with status: {status}");
            return Err(DriverError::from(status));
        }

        Ok(Self { client, serial_pid })
    }

    /// Returns the PID reported by the parent serial device.
    pub fn serial_pid(&self) -> u32 {
        self.serial_pid
    }

    /// Cancels all pending operations on the serial device.
    pub async fn cancel_all(&self) {
        if let Err(e) = self.client.cancel_all().await {
            error!("Failed to cancel all pending serial operations: {e:?}");
        }
    }

    /// Closes the underlying FIDL client connection.
    pub fn close(&self) {
        self.client.close();
    }
}

impl std::fmt::Debug for SerialConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SerialConnection")
            .field("serial_pid", &self.serial_pid)
            .finish_non_exhaustive()
    }
}

impl serialimpl::DeviceServerHandler<fdf_fidl::DriverChannel> for SerialConnection {
    async fn get_info(
        &mut self,
        responder: fidl_next::Responder<serialimpl::device::GetInfo, fdf_fidl::DriverChannel>,
    ) {
        let info = serial::SerialPortInfo {
            serial_class: serial::Class::BluetoothHci,
            serial_vid: 0,
            serial_pid: self.serial_pid,
        };
        let _ = responder.respond(&info).await;
    }

    async fn config(
        &mut self,
        request: fidl_next::Request<serialimpl::device::Config, fdf_fidl::DriverChannel>,
        responder: fidl_next::Responder<serialimpl::device::Config, fdf_fidl::DriverChannel>,
    ) {
        let payload = request.payload();
        let res = self.client.config(payload.baud_rate, payload.flags).await;
        let Ok(response) = res else {
            let err = res.unwrap_err();
            error!("Config request failed with FIDL error: {err:?}");
            let status = DriverError::from(err).log_to_status();
            let _ = responder.respond_err(status).await;
            return;
        };
        if let Err(status_res) = response.as_ref() {
            let status = status_res.err().unwrap_or(zx::Status::INTERNAL);
            error!("Config request failed with status: {status}");
            let _ = responder.respond_err(status).await;
        } else {
            let _ = responder.respond(()).await;
        }
    }

    async fn enable(
        &mut self,
        _request: fidl_next::Request<serialimpl::device::Enable, fdf_fidl::DriverChannel>,
        responder: fidl_next::Responder<serialimpl::device::Enable, fdf_fidl::DriverChannel>,
    ) {
        let _ = responder.respond_err(zx::Status::NOT_SUPPORTED).await;
    }

    async fn read(
        &mut self,
        responder: fidl_next::Responder<serialimpl::device::Read, fdf_fidl::DriverChannel>,
    ) {
        let _ = responder.respond_err(zx::Status::NOT_SUPPORTED).await;
    }

    async fn write(
        &mut self,
        _request: fidl_next::Request<serialimpl::device::Write, fdf_fidl::DriverChannel>,
        responder: fidl_next::Responder<serialimpl::device::Write, fdf_fidl::DriverChannel>,
    ) {
        let _ = responder.respond_err(zx::Status::NOT_SUPPORTED).await;
    }

    async fn cancel_all(
        &mut self,
        responder: fidl_next::Responder<serialimpl::device::CancelAll, fdf_fidl::DriverChannel>,
    ) {
        let _ = responder.respond(()).await;
    }
}

/// Service handler that exposes `fuchsia.hardware.serialimpl.Service` to child drivers.
#[derive(Debug)]
pub struct SerialService {
    serial: SerialConnection,
    scope: fasync::ScopeHandle,
}

impl SerialService {
    /// Creates a new `SerialService` backed by the given `SerialConnection` and async `scope`.
    pub fn new(serial: SerialConnection, scope: fasync::ScopeHandle) -> Self {
        Self { serial, scope }
    }
}

impl serialimpl::ServiceHandler for SerialService {
    fn device(
        &self,
        server_end: fidl_next::ServerEnd<serialimpl::Device, fdf_fidl::DriverChannel>,
    ) {
        server_end.spawn_on(self.serial.clone(), &self.scope);
    }
}
