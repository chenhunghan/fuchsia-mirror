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

    async fn stop(self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl_fuchsia_driver_metadata as fdr;
    use fidl_fuchsia_hardware_sdmmc as fsdmmc;

    fn load_prepared_devices() -> Vec<dml_config::parser::PreparedDevice> {
        let board_config_bytes =
            std::fs::read("/pkg/config/board-config.fidl").expect("board-config.fidl must exist");
        let board_config: BoardConfig =
            fidl::unpersist(&board_config_bytes).expect("board-config.fidl must deserialize");
        dml_config::parser::prepare_dml_devices(
            &board_config,
            &DEFAULT_DML_PARSER_CONFIG,
            Some(&driver_specific_data::VIM3_DRIVER_METADATA),
            None,
            &[],
        )
        .expect("prepare_dml_devices must succeed")
    }

    fn find_spec<'a>(
        prepared: &'a [dml_config::parser::PreparedDevice],
        name: &str,
    ) -> &'a fdf_framework::CompositeNodeSpec {
        prepared
            .iter()
            .find(|(dev_name, _, _)| dev_name == name)
            .and_then(|(_, spec, _)| spec.as_ref())
            .unwrap_or_else(|| panic!("missing composite spec {name}"))
    }

    fn find_pbus_node<'a>(
        prepared: &'a [dml_config::parser::PreparedDevice],
        name: &str,
    ) -> &'a fpbus::Node {
        prepared
            .iter()
            .find(|(dev_name, _, _)| dev_name == name)
            .and_then(|(_, _, node)| node.as_ref())
            .unwrap_or_else(|| panic!("missing platform bus node {name}"))
    }

    fn spec_has_parent_rule_string(
        spec: &fdf_framework::CompositeNodeSpec,
        key: &str,
        expected: &str,
    ) -> bool {
        spec.parents2.as_deref().unwrap_or(&[]).iter().any(|parent| {
            parent.bind_rules.iter().any(|rule| {
                rule.key == key
                    && rule.values.iter().any(|v| {
                        matches!(v, fdf_framework::NodePropertyValue::StringValue(s) if s == expected)
                    })
            })
        })
    }

    fn spec_has_parent_rule_int(
        spec: &fdf_framework::CompositeNodeSpec,
        key: &str,
        expected: u32,
    ) -> bool {
        spec.parents2.as_deref().unwrap_or(&[]).iter().any(|parent| {
            parent.bind_rules.iter().any(|rule| {
                rule.key == key
                    && rule.values.contains(&fdf_framework::NodePropertyValue::IntValue(expected))
            })
        })
    }

    #[test]
    fn test_vim3_clock_init_and_pwm_pin_states_parents() {
        let prepared = load_prepared_devices();

        const CLOCK_INIT_STEP: u32 = 0x494B4C43;
        let usb_phy = find_spec(&prepared, "usb-phy-ffe09000");
        assert!(
            spec_has_parent_rule_int(usb_phy, "fuchsia.BIND_INIT_STEP", CLOCK_INIT_STEP),
            "usb-phy-ffe09000 must depend on fuchsia.clock.Init: {:?}",
            usb_phy.parents2
        );

        let audio = find_spec(&prepared, "audio-controller-ff642000");
        assert!(
            spec_has_parent_rule_int(audio, "fuchsia.BIND_INIT_STEP", CLOCK_INIT_STEP),
            "audio-controller-ff642000 must depend on fuchsia.clock.Init: {:?}",
            audio.parents2
        );

        let pwm_ctrl = find_spec(&prepared, "pwm-ffd1b000");
        assert_eq!(
            pwm_ctrl.parents2.as_deref().map_or(0, |p| p.len()),
            1,
            "pwm-ffd1b000 must only have its pdev parent"
        );

        for reg_name in ["pwm_a-regulator", "pwm_a0_d-regulator"] {
            let reg = find_spec(&prepared, reg_name);
            assert_eq!(
                reg.parents2.as_deref().map_or(0, |p| p.len()),
                3,
                "{reg_name} must have 3 parents (pdev, pwm, pin-states-0): {:?}",
                reg.parents2
            );
            assert!(
                spec_has_parent_rule_string(
                    reg,
                    "fuchsia.Service",
                    "fuchsia.hardware.pin.PinStatesService"
                ),
                "{reg_name} missing pin-states-0 parent: {:?}",
                reg.parents2
            );
        }
    }

    #[test]
    fn test_vim3_i2c_and_sdmmc_metadata() {
        let prepared = load_prepared_devices();

        let i2c_node = find_pbus_node(&prepared, "i2c-1c000");
        let i2c_meta = i2c_node
            .metadata
            .as_deref()
            .unwrap_or(&[])
            .iter()
            .find(|m| m.id.as_deref() == Some("fuchsia.hardware.i2c.businfo.I2CBusMetadata"))
            .expect("i2c-1c000 missing I2CBusMetadata");
        let dict: fdr::Dictionary = fidl::unpersist(i2c_meta.data.as_deref().unwrap())
            .expect("I2CBusMetadata dictionary must deserialize");
        let entries = dict.entries.expect("I2CBusMetadata entries must exist");
        assert!(
            entries
                .iter()
                .any(|e| e.key == "provider" && e.value == fdr::DictionaryValue::Int64(1)),
            "i2c-1c000 must set provider = 1: {entries:?}"
        );
        assert!(
            entries
                .iter()
                .any(|e| e.key == "channels._count" && e.value == fdr::DictionaryValue::Int64(0)),
            "i2c-1c000 must set channels._count = 0: {entries:?}"
        );

        for (node_name, expected_id, expected_removable) in [
            ("mmc-ffe07000", "mmc@ffe07000", false),
            ("mmc-ffe05000", "mmc@ffe05000", true),
            ("mmc-ffe03000", "mmc@ffe03000", true),
        ] {
            let mmc_node = find_pbus_node(&prepared, node_name);
            let mmc_meta = mmc_node
                .metadata
                .as_deref()
                .unwrap_or(&[])
                .iter()
                .find(|m| m.id.as_deref() == Some("fuchsia.hardware.sdmmc.SdmmcMetadata"))
                .unwrap_or_else(|| panic!("{node_name} missing SdmmcMetadata"));
            let parsed: fsdmmc::SdmmcMetadata =
                fidl::unpersist(mmc_meta.data.as_deref().unwrap()).unwrap();
            assert_eq!(parsed.instance_identifier.as_deref(), Some(expected_id));
            assert_eq!(parsed.removable, Some(expected_removable));
        }
    }
}
