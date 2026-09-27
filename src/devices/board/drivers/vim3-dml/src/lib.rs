// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use dml_config::BoardConfig;
use fdf_component::{Driver, DriverContext, DriverError, Node, driver_register};
use fdf_fidl::DriverChannel;
use fidl_fuchsia_io as fio;
use fidl_next_fuchsia_driver_framework as fdf_framework;
use fidl_next_fuchsia_hardware_platform_bus as fpbus;
use log::info;

use anyhow::Context;

mod driver_specific_data;
use dml_config::parser::{DEFAULT_DML_PARSER_CONFIG, publish_dml_devices};

/// The VIM3 DML board driver.
/// This driver parses the compiled board configuration (from DML) and publishes
/// devices using the default parser configuration (`DEFAULT_DML_PARSER_CONFIG`) to map services to
/// bind properties and rules, and using `VIM3_DRIVER_METADATA` to publish metadata.
struct Vim3DmlDriver {
    _node: Node,
    _pbus: fidl_next::Client<fpbus::PlatformBus, DriverChannel>,
}

driver_register!(Vim3DmlDriver);

impl Driver for Vim3DmlDriver {
    const NAME: &str = "vim3-dml";

    async fn start(mut context: DriverContext) -> Result<Self, DriverError> {
        info!("Starting VIM3 DML driver");
        let node = context.take_node()?;

        let file = fuchsia_component::directory::open_file_async(
            &context.incoming,
            "pkg/config/board-config.fidl",
            fio::Rights::READ_BYTES,
        )
        .context("Failed to open board config file")?;
        let board_config_bytes =
            fuchsia_fs::file::read(&file).await.context("Failed to read board config file")?;
        let board_config = fidl::unpersist::<BoardConfig>(&board_config_bytes)
            .context("Failed to deserialize board config FIDL")?;

        // Connect to PlatformBus service.
        let service = context
            .incoming
            .service::<fdf_component::ServiceInstance<fpbus::Service>>()
            .connect_next()
            .context("Failed to connect to PlatformBus service")?;

        let (client_end, server_end) = fdf_fidl::create_channel::<fpbus::PlatformBus>();
        service.platform_bus(server_end).context("Failed to connect to platform_bus member")?;

        let pbus = client_end.spawn();

        let composite_manager_client = context
            .incoming
            .connect_protocol_next::<fdf_framework::CompositeNodeManager>()
            .context("Failed to connect to CompositeNodeManager")?;
        let composite_manager = composite_manager_client.spawn();

        let board_info = pbus
            .get_board_info()
            .await
            .context("Failed to call GetBoardInfo")?
            .map_err(|e| e.err().unwrap_or(zx::Status::INTERNAL))
            .context("GetBoardInfo returned error")?;
        info!("Board info: {board_info:?}");

        let enabled_nodes = context
            .take_config::<dml_config::StructuredConfig>()
            .map(|c| c.enabled_nodes)
            .unwrap_or_default();

        publish_dml_devices(
            &pbus,
            &composite_manager,
            &board_config,
            &DEFAULT_DML_PARSER_CONFIG,
            Some(&driver_specific_data::VIM3_DRIVER_METADATA),
            None,
            &enabled_nodes,
        )
        .await
        .context("Failed to publish DML devices")?;

        info!("VIM3 DML driver started successfully");
        Ok(Self { _node: node, _pbus: pbus })
    }

    async fn stop(&self) {}
}
