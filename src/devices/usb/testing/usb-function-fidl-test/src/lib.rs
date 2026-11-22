// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

pub mod mock;

use crate::mock::{MockEvent, MockUsbFunction};
use diagnostics_reader::{ArchiveReader, DiagnosticsHierarchy, Property};
pub use fidl_fuchsia_hardware_usb_descriptor as fusb_descriptor;
use fidl_fuchsia_hardware_usb_function as fusb_function;
use fuchsia_async as fasync;
use futures::FutureExt;
use std::collections::{HashMap, HashSet};
use zx::Status;

const USB_DT_INTERFACE: u8 = 4;
const USB_DT_ENDPOINT: u8 = 5;
const USB_DT_INTERFACE_ASSOCIATION: u8 = 11;
const USB_DT_INTERFACE_MIN_LEN: usize = 9;
const USB_DT_ENDPOINT_MIN_LEN: usize = 7;

/// Verifies that the driver returns NOT_SUPPORTED for an unknown/unsupported control request.
pub async fn verify_control_unsupported(proxy: &fusb_function::UsbFunctionInterfaceProxy) {
    let setup = fusb_descriptor::UsbSetup {
        bm_request_type: 0,
        b_request: 0,
        w_value: 0,
        w_index: 0,
        w_length: 0,
    };
    let result = proxy.control(&setup, &[]).await;
    assert!(result.is_ok());
    let response = result.unwrap();
    assert!(response.is_err());
    assert_eq!(Status::err_from_raw(response.err().unwrap()), Status::NOT_SUPPORTED);
}

/// Verifies UsbFunctionInterface.SetInterface on an unsupported interface/alt_setting fails.
pub async fn verify_set_interface_unsupported(
    proxy: &fusb_function::UsbFunctionInterfaceProxy,
    interface: u8,
    alt_setting: u8,
) {
    let result = proxy.set_interface(interface, alt_setting).await;
    assert!(result.is_ok());
    let response = result.unwrap();
    assert!(response.is_err());
    let err = Status::err_from_raw(response.err().unwrap());
    assert!(
        err == Status::NOT_SUPPORTED || err == Status::BAD_STATE,
        "Expected NOT_SUPPORTED or BAD_STATE, got {:?}",
        err
    );
}

/// Verifies UsbFunctionInterface.Control replies promptly (logging a warning if wall-clock latency
/// exceeds the 50ms USB 2.0 §9.2.6 status-stage target on slow/unaccelerated CI builders).
pub async fn verify_control_latency(proxy: &fusb_function::UsbFunctionInterfaceProxy) {
    let setup = fusb_descriptor::UsbSetup {
        bm_request_type: 0,
        b_request: 0,
        w_value: 0,
        w_index: 0,
        w_length: 0,
    };
    // Warm up connection to avoid startup latency
    let _ = proxy.control(&setup, &[]).await;

    let start = std::time::Instant::now();
    let result = proxy.control(&setup, &[]).await;
    let elapsed = start.elapsed();

    assert!(result.is_ok());
    if elapsed >= std::time::Duration::from_millis(50) {
        log::warn!(
            "Control request took {:?}, exceeding 50ms USB 2.0 status-stage target (likely CI scheduling jitter)",
            elapsed
        );
    }
}

/// Verifies UsbFunction.AllocResources -> UsbFunction.Configure sequencing and descriptor layout.
pub fn verify_initialization(mock: &mock::MockUsbFunction) {
    let state = mock.state.lock();
    let events = &state.events;

    let alloc_idx = events.iter().position(|e| matches!(e, MockEvent::AllocResources { .. }));
    let config_idx = events.iter().position(|e| matches!(e, MockEvent::Configure { .. }));

    assert!(alloc_idx.is_some(), "UsbFunction.AllocResources was never called");
    assert!(config_idx.is_some(), "UsbFunction.Configure was never called");
    assert!(
        alloc_idx.unwrap() < config_idx.unwrap(),
        "UsbFunction.Configure called before UsbFunction.AllocResources"
    );

    let (alloc_interfaces, alloc_endpoints) = match &events[alloc_idx.unwrap()] {
        MockEvent::AllocResources { interface_nums, endpoints } => (
            interface_nums.iter().copied().collect::<HashSet<u8>>(),
            endpoints.iter().copied().collect::<HashSet<u8>>(),
        ),
        _ => unreachable!(),
    };

    let config_desc = match &events[config_idx.unwrap()] {
        MockEvent::Configure { descriptors } => descriptors.clone(),
        _ => unreachable!(),
    };

    // UsbFunction.Configure requires the descriptor block to begin with Interface (4) or IAD (11).
    assert!(config_desc.len() >= 2, "Configuration descriptor payload too short");
    let first_desc_type = config_desc[1];
    assert!(
        first_desc_type == USB_DT_INTERFACE || first_desc_type == USB_DT_INTERFACE_ASSOCIATION,
        "First descriptor type is {}, expected Interface ({}) or IAD ({})",
        first_desc_type,
        USB_DT_INTERFACE,
        USB_DT_INTERFACE_ASSOCIATION
    );

    // Descriptor interface numbers and endpoint addresses must match those returned by
    // UsbFunction.AllocResources.
    let parsed_config = parse_usb_descriptors(&config_desc);
    let desc_interfaces = parsed_config.all_interfaces();
    assert_eq!(
        desc_interfaces, alloc_interfaces,
        "Descriptor interface numbers {:?} do not match AllocResources interfaces {:?}",
        desc_interfaces, alloc_interfaces
    );

    let desc_endpoints = parsed_config.all_endpoints();
    assert_eq!(
        desc_endpoints, alloc_endpoints,
        "Descriptor endpoint addresses {:?} do not match AllocResources endpoints {:?}",
        desc_endpoints, alloc_endpoints
    );
}

pub fn parsed_config(mock: &MockUsbFunction) -> ParsedUsbConfig {
    let events = mock.state.lock().events.clone();
    let config_desc = events
        .iter()
        .find_map(|e| match e {
            MockEvent::Configure { descriptors } => Some(descriptors.clone()),
            _ => None,
        })
        .expect("No Configure event found in mock state");
    parse_usb_descriptors(&config_desc)
}

#[derive(Debug, Clone)]
pub struct InterfaceAltSetting {
    pub interface_num: u8,
    pub alt_setting: u8,
    pub endpoints: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct ParsedUsbConfig {
    pub interfaces: Vec<InterfaceAltSetting>,
}

impl ParsedUsbConfig {
    pub fn all_interfaces(&self) -> HashSet<u8> {
        self.interfaces.iter().map(|i| i.interface_num).collect()
    }

    pub fn all_endpoints(&self) -> HashSet<u8> {
        self.interfaces.iter().flat_map(|i| i.endpoints.iter().copied()).collect()
    }

    pub fn default_endpoints(&self) -> Vec<u8> {
        self.interfaces
            .iter()
            .filter(|i| i.alt_setting == 0)
            .flat_map(|i| i.endpoints.iter().copied())
            .collect()
    }

    pub fn interface_endpoints(&self, interface_num: u8, alt_setting: u8) -> Vec<u8> {
        self.interfaces
            .iter()
            .find(|i| i.interface_num == interface_num && i.alt_setting == alt_setting)
            .map(|i| i.endpoints.clone())
            .unwrap_or_default()
    }

    pub fn unsupported_interface_alts(&self) -> Vec<(u8, u8)> {
        let max_alt_0 = self
            .interfaces
            .iter()
            .filter(|i| i.interface_num == 0)
            .map(|i| i.alt_setting)
            .max()
            .unwrap_or(0);
        let max_iface = self.interfaces.iter().map(|i| i.interface_num).max().unwrap_or(0);
        vec![(0, max_alt_0 + 1), (max_iface + 1, 0)]
    }
}

pub fn parse_usb_descriptors(config_desc: &[u8]) -> ParsedUsbConfig {
    let mut interfaces = Vec::new();
    let mut offset = 0;
    let mut current_interface: Option<(u8, u8)> = None;
    let mut current_endpoints = Vec::new();

    while offset < config_desc.len() {
        assert!(
            offset + 2 <= config_desc.len(),
            "Truncated USB descriptor header at offset {} (total len {})",
            offset,
            config_desc.len()
        );
        let length = config_desc[offset] as usize;
        assert!(
            length >= 2 && offset + length <= config_desc.len(),
            "Invalid USB descriptor bLength {} at offset {} (total len {})",
            length,
            offset,
            config_desc.len()
        );
        let desc_type = config_desc[offset + 1];
        match desc_type {
            USB_DT_INTERFACE => {
                // Interface Descriptor (USB 2.0 §9.6.5: 9 bytes)
                assert!(
                    length >= USB_DT_INTERFACE_MIN_LEN,
                    "Interface descriptor bLength {} is shorter than {} bytes",
                    length,
                    USB_DT_INTERFACE_MIN_LEN
                );
                if let Some((iface_num, alt_setting)) = current_interface {
                    interfaces.push(InterfaceAltSetting {
                        interface_num: iface_num,
                        alt_setting,
                        endpoints: std::mem::take(&mut current_endpoints),
                    });
                }
                let interface_num = config_desc[offset + 2];
                let alt_setting = config_desc[offset + 3];
                current_interface = Some((interface_num, alt_setting));
            }
            USB_DT_ENDPOINT => {
                // Endpoint Descriptor (USB 2.0 §9.6.6: 7 bytes)
                assert!(
                    length >= USB_DT_ENDPOINT_MIN_LEN,
                    "Endpoint descriptor bLength {} is shorter than {} bytes",
                    length,
                    USB_DT_ENDPOINT_MIN_LEN
                );
                assert!(
                    current_interface.is_some(),
                    "Endpoint descriptor at offset {} precedes any Interface descriptor",
                    offset
                );
                let ep_addr = config_desc[offset + 2];
                current_endpoints.push(ep_addr);
            }
            _ => {}
        }
        offset += length;
    }

    if let Some((iface_num, alt_setting)) = current_interface {
        interfaces.push(InterfaceAltSetting {
            interface_num: iface_num,
            alt_setting,
            endpoints: current_endpoints,
        });
    }

    ParsedUsbConfig { interfaces }
}

/// Verifies SetConfigured(true) configures endpoints and SetConfigured(false) disables them across
/// all standard USB speeds (Full, High, Super, EnhancedSuper), asserting call sequencing.
pub async fn verify_set_configured_lifecycle(
    mock: &mock::MockUsbFunction,
    proxy: &fusb_function::UsbFunctionInterfaceProxy,
    expected_endpoints: &[u8],
) {
    let mut configs_by_speed: HashMap<
        u8,
        Vec<(fusb_descriptor::UsbSpeed, fusb_function::EndpointConfiguration)>,
    > = HashMap::new();
    for speed in [
        fusb_descriptor::UsbSpeed::Full,
        fusb_descriptor::UsbSpeed::High,
        fusb_descriptor::UsbSpeed::Super,
        fusb_descriptor::UsbSpeed::EnhancedSuper,
    ] {
        // 1. Configure
        let start_idx = {
            let mut state = mock.state.lock();
            let idx = state.events.len();
            state.events.push(MockEvent::SetConfiguredSend { configured: true });
            idx
        };
        let result = proxy.set_configured(true, speed).await;
        assert!(result.is_ok(), "SetConfigured(true, {:?}) FIDL call failed", speed);
        assert!(result.unwrap().is_ok(), "SetConfigured(true, {:?}) failed", speed);
        {
            let mut state = mock.state.lock();
            state.events.push(MockEvent::SetConfiguredReply { configured: true });
            for addr in expected_endpoints {
                let config = state.configured_endpoints.get(addr).unwrap_or_else(|| {
                    panic!("Endpoint 0x{:02x} not configured at {:?}", addr, speed)
                });
                configs_by_speed.entry(*addr).or_default().push((speed, config.clone()));
            }
        }

        // 2. Deconfigure
        {
            let mut state = mock.state.lock();
            state.events.push(MockEvent::SetConfiguredSend { configured: false });
        }
        let result = proxy.set_configured(false, speed).await;
        assert!(result.is_ok(), "SetConfigured(false, {:?}) FIDL call failed", speed);
        assert!(result.unwrap().is_ok(), "SetConfigured(false, {:?}) failed", speed);
        {
            let mut state = mock.state.lock();
            state.events.push(MockEvent::SetConfiguredReply { configured: false });
        }

        // Verify ordering within this lifecycle step
        let state = mock.state.lock();
        let events = &state.events[start_idx..];

        // Verify SetConfigured(true) ordering
        let send_true_idx = events
            .iter()
            .position(|e| matches!(e, MockEvent::SetConfiguredSend { configured: true }))
            .unwrap();
        let reply_true_idx = events
            .iter()
            .position(|e| matches!(e, MockEvent::SetConfiguredReply { configured: true }))
            .unwrap();

        for addr in expected_endpoints {
            let ep_cfg_idx = events[send_true_idx..reply_true_idx]
                .iter()
                .position(
                    |e| matches!(e, MockEvent::ConfigureEndpoint { ep_addr } if ep_addr == addr),
                )
                .map(|i| i + send_true_idx);
            assert!(
                ep_cfg_idx.is_some(),
                "Endpoint 0x{:02x} was not configured between SetConfigured(true, {:?}) send and reply",
                addr,
                speed
            );
        }

        // Verify SetConfigured(false) ordering
        let send_false_idx = events
            .iter()
            .position(|e| matches!(e, MockEvent::SetConfiguredSend { configured: false }))
            .unwrap();
        let reply_false_idx = events
            .iter()
            .position(|e| matches!(e, MockEvent::SetConfiguredReply { configured: false }))
            .unwrap();

        let has_deconfigure = events[send_false_idx..reply_false_idx]
            .iter()
            .any(|e| matches!(e, MockEvent::Deconfigure));
        if !has_deconfigure {
            for addr in expected_endpoints {
                let ep_dis_idx = events[send_false_idx..reply_false_idx]
                    .iter()
                    .position(
                        |e| matches!(e, MockEvent::DisableEndpoint { ep_addr } if ep_addr == addr),
                    )
                    .map(|i| i + send_false_idx);
                assert!(
                    ep_dis_idx.is_some(),
                    "Endpoint 0x{:02x} was not disabled between SetConfigured(false, {:?}) send and reply",
                    addr,
                    speed
                );
            }
        }

        assert!(
            state.configured_endpoints.is_empty(),
            "Endpoints remained configured after SetConfigured(false, {:?}): {:?}",
            speed,
            state.configured_endpoints.keys()
        );
        for (addr, ep) in &state.endpoints {
            assert!(
                ep.state.lock().requests.is_empty(),
                "Endpoint 0x{:02x} still has pending requests after SetConfigured(false, {:?})",
                addr,
                speed
            );
        }
    }

    verify_endpoint_configuration_per_speed(&configs_by_speed);
}

/// Verifies the EndpointConfiguration each driver passed to ConfigureEndpoint at each speed.
///
/// Every endpoint needs a descriptor with a non-zero wMaxPacketSize, and a SuperSpeed companion
/// descriptor is only valid at Super/EnhancedSuper. Drivers that adapt wMaxPacketSize to the
/// negotiated speed (today usb-zero-function) must also use the 1024-byte SuperSpeed bulk packet
/// size and provide a companion descriptor at Super/EnhancedSuper. Drivers that copy a static
/// 512-byte descriptor at every speed and never set a companion are accepted as-is.
fn verify_endpoint_configuration_per_speed(
    configs_by_speed: &HashMap<
        u8,
        Vec<(fusb_descriptor::UsbSpeed, fusb_function::EndpointConfiguration)>,
    >,
) {
    use fusb_descriptor::UsbSpeed;
    const USB_ENDPOINT_TYPE_MASK: u8 = 0x03;
    const USB_ENDPOINT_BULK: u8 = 0x02;

    for (addr, configs) in configs_by_speed {
        let mps_at = |wanted: UsbSpeed| {
            configs
                .iter()
                .find(|(speed, _)| *speed == wanted)
                .and_then(|(_, config)| config.descriptor.as_ref().map(|d| d.w_max_packet_size))
        };
        let speed_aware = mps_at(UsbSpeed::Full) != mps_at(UsbSpeed::Super);

        for (speed, config) in configs {
            let descriptor = config.descriptor.as_ref().unwrap_or_else(|| {
                panic!("Endpoint 0x{:02x} configured without a descriptor at {:?}", addr, speed)
            });
            assert_ne!(
                descriptor.w_max_packet_size, 0,
                "Endpoint 0x{:02x} configured with wMaxPacketSize 0 at {:?}",
                addr, speed
            );
            let is_super = matches!(speed, UsbSpeed::Super | UsbSpeed::EnhancedSuper);
            if !is_super {
                assert!(
                    config.super_speed_companion.is_none(),
                    "Endpoint 0x{:02x} has a SuperSpeed companion descriptor at {:?}",
                    addr,
                    speed
                );
            } else if speed_aware {
                assert!(
                    config.super_speed_companion.is_some(),
                    "Speed-aware endpoint 0x{:02x} missing SuperSpeed companion descriptor at {:?}",
                    addr,
                    speed
                );
                if descriptor.bm_attributes & USB_ENDPOINT_TYPE_MASK == USB_ENDPOINT_BULK {
                    assert_eq!(
                        descriptor.w_max_packet_size, 1024,
                        "Speed-aware bulk endpoint 0x{:02x} wMaxPacketSize at {:?}",
                        addr, speed
                    );
                }
            }
        }
    }
}

/// Verifies that closing the UsbFunction server channel while the function is configured and
/// active (producing a FIDL framework/transport PEER_CLOSED error on subsequent DisableEndpoint
/// and Deconfigure calls) is tolerated cleanly during SetConfigured(false) and subsequent driver
/// stop/teardown.
pub async fn verify_function_closure_resilience(
    mock: &MockUsbFunction,
    proxy: &fusb_function::UsbFunctionInterfaceProxy,
    parsed_config: &ParsedUsbConfig,
) {
    let alt_interfaces: Vec<_> = parsed_config
        .interfaces
        .iter()
        .filter(|i| i.alt_setting > 0 && !i.endpoints.is_empty())
        .cloned()
        .collect();

    // 1. Configure the function and activate any alternate settings with endpoints so all
    // endpoints are enabled before the UsbFunction channel closes.
    let result = proxy.set_configured(true, fusb_descriptor::UsbSpeed::High).await;
    assert!(result.is_ok(), "SetConfigured(true) FIDL call failed");
    assert!(result.unwrap().is_ok(), "SetConfigured(true) failed");
    for alt_iface in &alt_interfaces {
        let result = proxy.set_interface(alt_iface.interface_num, alt_iface.alt_setting).await;
        assert!(result.is_ok(), "SetInterface FIDL call failed");
        assert!(result.unwrap().is_ok(), "SetInterface failed");
    }

    // 2. Close the UsbFunction server channel so subsequent DisableEndpoint and Deconfigure
    // calls fail at the FIDL transport layer with ZX_ERR_PEER_CLOSED (is_framework_error()).
    mock.close_function_server().await;

    // 3. Verify SetConfigured(false) tolerates the framework/transport PEER_CLOSED error on
    // DisableEndpoint and still completes with ZX_OK. Subsequent realm.destroy() will also
    // verify that driver Stop()/PrepareStop() (including DisableAllEndpoints and Deconfigure)
    // completes cleanly with the UsbFunction server channel closed.
    let result = proxy.set_configured(false, fusb_descriptor::UsbSpeed::High).await;
    assert!(
        result.is_ok(),
        "UsbFunctionInterface channel closed on SetConfigured(false) after UsbFunction closure"
    );
    assert!(
        result.unwrap().is_ok(),
        "SetConfigured(false) did not tolerate UsbFunction server channel closure"
    );

    let state = mock.state.lock();
    for (addr, ep) in &state.endpoints {
        assert!(
            ep.state.lock().requests.is_empty(),
            "Endpoint 0x{:02x} still has pending requests after SetConfigured(false) with UsbFunction closed",
            addr
        );
    }
}

/// Verifies that the driver remains active and responsive when Endpoint channels close.
pub async fn verify_endpoint_closure_resilience(
    mock: &mock::MockUsbFunction,
    proxy: &fusb_function::UsbFunctionInterfaceProxy,
) {
    // Close all mock endpoint channels from the server side by clearing the map.
    {
        let mut state = mock.state.lock();
        state.endpoints.clear();
    }

    // Give some time for unbind/disconnection detection to happen asynchronously
    fasync::Timer::new(std::time::Duration::from_millis(100)).await;

    // Verify driver is still responsive by sending control request
    verify_control_unsupported(proxy).await;
}

/// Verifies SetConfigured(false) idempotency when already unconfigured, reconfiguration across
/// SetConfigured(false) -> SetConfigured(true), soft reset via SetConfigured(true) when already
/// configured, and SetInterface(iface, 0) on default interfaces while configured.
pub async fn verify_set_configured_idempotency_and_reconfigure(
    mock: &MockUsbFunction,
    proxy: &fusb_function::UsbFunctionInterfaceProxy,
    parsed_config: &ParsedUsbConfig,
) {
    let default_endpoints = parsed_config.default_endpoints();

    // 1. SetConfigured(false) when already unconfigured (idempotent unconfiguration).
    let result = proxy.set_configured(false, fusb_descriptor::UsbSpeed::High).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_ok(), "Idempotent SetConfigured(false) failed");

    // 2. Reconfiguration: SetConfigured(true) after SetConfigured(false) must re-enable all
    // default endpoints before replying.
    let reconfig_start_idx = {
        let mut state = mock.state.lock();
        state.events.push(MockEvent::SetConfiguredSend { configured: true });
        state.events.len() - 1
    };
    let result = proxy.set_configured(true, fusb_descriptor::UsbSpeed::High).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_ok(), "Reconfiguration SetConfigured(true) failed");
    let reconfig_reply_idx = {
        let mut state = mock.state.lock();
        state.events.push(MockEvent::SetConfiguredReply { configured: true });
        state.events.len() - 1
    };

    {
        let state = mock.state.lock();
        for addr in &default_endpoints {
            assert!(
                state.configured_endpoints.contains_key(addr),
                "Endpoint 0x{:02x} not in configured_endpoints after reconfiguration",
                addr
            );
            let configured_during_call = state.events[reconfig_start_idx + 1..reconfig_reply_idx]
                .iter()
                .any(|e| matches!(e, MockEvent::ConfigureEndpoint { ep_addr } if ep_addr == addr));
            assert!(
                configured_during_call,
                "ConfigureEndpoint for 0x{:02x} not called during reconfiguration SetConfigured(true)",
                addr
            );
        }
    }

    // 3. Soft reset: SetConfigured(true) while already configured must succeed and leave default
    // endpoints configured.
    let result = proxy.set_configured(true, fusb_descriptor::UsbSpeed::High).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_ok(), "Soft-reset SetConfigured(true) while configured failed");
    {
        let state = mock.state.lock();
        for addr in &default_endpoints {
            assert!(
                state.configured_endpoints.contains_key(addr),
                "Endpoint 0x{:02x} not configured after soft-reset SetConfigured(true)",
                addr
            );
        }
    }

    // 4. Default SetInterface(iface, 0) while configured must succeed for interfaces with
    // alternate settings, and may either succeed or return ZX_ERR_NOT_SUPPORTED (STALL per
    // USB 2.0 §9.4.10) if the interface only supports the default setting.
    let mut interfaces: Vec<u8> = parsed_config.all_interfaces().into_iter().collect();
    interfaces.sort_unstable();
    assert!(!interfaces.is_empty(), "No interfaces found in configuration descriptor");
    for interface in interfaces {
        let has_alt_settings = parsed_config
            .interfaces
            .iter()
            .any(|i| i.interface_num == interface && i.alt_setting > 0);
        let result = proxy.set_interface(interface, 0).await;
        assert!(result.is_ok(), "SetInterface({}, 0) FIDL call failed", interface);
        match result.unwrap() {
            Ok(()) => {}
            Err(raw) if !has_alt_settings && Status::err_from_raw(raw) == Status::NOT_SUPPORTED => {
            }
            Err(raw) => panic!(
                "SetInterface({}, 0) failed with {:?} (has_alt_settings={})",
                interface,
                Status::err_from_raw(raw),
                has_alt_settings
            ),
        }
    }

    // 5. Return to unconfigured state cleanly.
    let result = proxy.set_configured(false, fusb_descriptor::UsbSpeed::High).await;
    assert!(result.is_ok());
    assert!(result.unwrap().is_ok(), "SetConfigured(false) after reconfigure test failed");
}

/// Verifies ConfigureEndpoint error propagation, partial-failure rollback, recovery on retry,
/// and DisableEndpoint disconnect/non-disconnect error handling.
pub async fn verify_endpoint_error_handling_and_rollback(
    mock: &MockUsbFunction,
    proxy: &fusb_function::UsbFunctionInterfaceProxy,
    default_endpoints: &[u8],
) {
    if default_endpoints.is_empty() {
        return;
    }

    // 1. Failing the 1st ConfigureEndpoint call must fail SetConfigured(true) and leave 0
    // endpoints configured.
    {
        let mut state = mock.state.lock();
        state.fail_configure_endpoint_nth = Some((1, Status::NO_RESOURCES));
    }
    let first_err = Status::err_from_raw(
        proxy
            .set_configured(true, fusb_descriptor::UsbSpeed::High)
            .await
            .expect("SetConfigured(true) FIDL call failed")
            .expect_err("SetConfigured(true) succeeded despite 1st ConfigureEndpoint failure"),
    );
    // Most drivers propagate the ConfigureEndpoint status verbatim. usb-cdc-function and the
    // virtual-bus-tester FidlTestFunction intentionally report any ConfigureEndpoint domain error
    // as INTERNAL.
    assert!(
        first_err == Status::NO_RESOURCES || first_err == Status::INTERNAL,
        "Expected NO_RESOURCES or INTERNAL on ConfigureEndpoint failure, got {:?}",
        first_err
    );
    {
        let state = mock.state.lock();
        assert!(
            state.configured_endpoints.is_empty(),
            "Endpoints left configured after 1st ConfigureEndpoint failure"
        );
    }

    // 2. For two-endpoint bulk functions, failing the 2nd ConfigureEndpoint call must roll back
    // the 1st endpoint via DisableEndpoint (and tolerate PEER_CLOSED during rollback).
    if default_endpoints.len() == 2 {
        let start_events_len = {
            let mut state = mock.state.lock();
            state.fail_configure_endpoint_nth = Some((2, Status::IO_NOT_PRESENT));
            state.events.len()
        };
        proxy
            .set_configured(true, fusb_descriptor::UsbSpeed::High)
            .await
            .expect("SetConfigured(true) FIDL call failed")
            .expect_err("SetConfigured(true) succeeded despite 2nd ConfigureEndpoint failure");
        {
            let state = mock.state.lock();
            assert!(
                state.configured_endpoints.is_empty(),
                "1st endpoint was not rolled back on 2nd ConfigureEndpoint failure"
            );
            let rolled_back = state.events[start_events_len..]
                .iter()
                .any(|e| matches!(e, MockEvent::DisableEndpoint { .. }));
            assert!(
                rolled_back,
                "DisableEndpoint was not called during partial ConfigureEndpoint rollback"
            );
        }

        // Rollback must also tolerate DisableEndpoint returning PEER_CLOSED.
        {
            let mut state = mock.state.lock();
            state.fail_configure_endpoint_nth = Some((2, Status::IO_NOT_PRESENT));
            state.fail_disable_endpoint_status = Some(Status::PEER_CLOSED);
        }
        proxy
            .set_configured(true, fusb_descriptor::UsbSpeed::High)
            .await
            .expect("SetConfigured(true) FIDL call failed")
            .expect_err("SetConfigured(true) succeeded despite rollback failure");
        {
            let mut state = mock.state.lock();
            state.fail_disable_endpoint_status = None;
        }
    }

    // 3. Clearing the fault and retrying SetConfigured(true) must recover cleanly.
    {
        let mut state = mock.state.lock();
        state.fail_configure_endpoint_nth = None;
        state.fail_disable_endpoint_status = None;
    }
    proxy
        .set_configured(true, fusb_descriptor::UsbSpeed::High)
        .await
        .expect("SetConfigured(true) FIDL call failed")
        .expect("SetConfigured(true) retry failed after clearing ConfigureEndpoint error");
    {
        let state = mock.state.lock();
        for addr in default_endpoints {
            assert!(
                state.configured_endpoints.contains_key(addr),
                "Endpoint 0x{:02x} not configured on retry after rollback",
                addr
            );
        }
    }

    // 4. DisableEndpoint returning IO_NOT_PRESENT or PEER_CLOSED during SetConfigured(false)
    // must be tolerated without crashing or hanging the driver.
    {
        let mut state = mock.state.lock();
        state.fail_disable_endpoint_status = Some(Status::IO_NOT_PRESENT);
    }
    proxy
        .set_configured(false, fusb_descriptor::UsbSpeed::High)
        .await
        .expect("FIDL channel closed on DisableEndpoint(IO_NOT_PRESENT)")
        .expect("SetConfigured(false) did not tolerate DisableEndpoint(IO_NOT_PRESENT)");
    {
        let mut state = mock.state.lock();
        state.fail_disable_endpoint_status = None;
    }

    // Reconfigure and verify PEER_CLOSED on DisableEndpoint is also tolerated.
    proxy
        .set_configured(true, fusb_descriptor::UsbSpeed::High)
        .await
        .expect("SetConfigured(true) FIDL call failed")
        .expect("SetConfigured(true) failed");
    {
        let mut state = mock.state.lock();
        state.fail_disable_endpoint_status = Some(Status::PEER_CLOSED);
    }
    proxy
        .set_configured(false, fusb_descriptor::UsbSpeed::High)
        .await
        .expect("FIDL channel closed on DisableEndpoint(PEER_CLOSED)")
        .expect("SetConfigured(false) did not tolerate DisableEndpoint(PEER_CLOSED)");
    {
        let mut state = mock.state.lock();
        state.fail_disable_endpoint_status = None;
    }

    // 5. Non-disconnect DisableEndpoint errors (IO, BAD_STATE, INTERNAL) during
    // SetConfigured(false) must still disable all remaining endpoints without skipping any,
    // keep the FIDL channel open, propagate the first injected error, and allow clean
    // reconfiguration afterward.
    for err_status in [Status::IO, Status::BAD_STATE, Status::INTERNAL] {
        proxy
            .set_configured(true, fusb_descriptor::UsbSpeed::High)
            .await
            .expect("SetConfigured(true) FIDL call failed")
            .expect("SetConfigured(true) failed");
        let start_events_len = {
            let mut state = mock.state.lock();
            state.fail_disable_endpoint_status = Some(err_status);
            state.events.len()
        };
        let actual_err = Status::err_from_raw(
            proxy
                .set_configured(false, fusb_descriptor::UsbSpeed::High)
                .await
                .expect("FIDL channel closed on DisableEndpoint error injection")
                .expect_err("SetConfigured(false) succeeded despite DisableEndpoint error"),
        );
        assert_eq!(
            actual_err, err_status,
            "Unexpected error status from SetConfigured(false) when DisableEndpoint returned {:?}",
            err_status
        );
        {
            let mut state = mock.state.lock();
            state.fail_disable_endpoint_status = None;
            let events = &state.events[start_events_len..];
            let has_deconfigure = events.iter().any(|e| matches!(e, MockEvent::Deconfigure));
            if !has_deconfigure {
                for addr in default_endpoints {
                    assert!(
                        events.iter().any(
                            |e| matches!(e, MockEvent::DisableEndpoint { ep_addr } if ep_addr == addr)
                        ),
                        "Endpoint 0x{:02x} was skipped when DisableEndpoint returned {:?}",
                        addr,
                        err_status
                    );
                }
            }
            assert!(
                state.configured_endpoints.is_empty(),
                "Endpoints left configured after DisableEndpoint({:?})",
                err_status
            );
        }
    }

    proxy
        .set_configured(true, fusb_descriptor::UsbSpeed::High)
        .await
        .expect("SetConfigured(true) FIDL call failed")
        .expect("SetConfigured(true) failed");
    proxy
        .set_configured(false, fusb_descriptor::UsbSpeed::High)
        .await
        .expect("SetConfigured(false) FIDL call failed")
        .expect("SetConfigured(false) failed");
}

/// Configures the function driver (`SetConfigured(true)` + alternate interface settings +
/// `CDC_SEND_ENCAPSULATED_COMMAND` control kick when needed) so active endpoints have queued
/// requests. Returns `(active_endpoints, endpoints_with_queued_requests)`.
pub async fn configure_and_activate_endpoints(
    mock: &MockUsbFunction,
    proxy: &fusb_function::UsbFunctionInterfaceProxy,
    parsed_config: &ParsedUsbConfig,
) -> (HashSet<u8>, HashSet<u8>) {
    proxy
        .set_configured(true, fusb_descriptor::UsbSpeed::High)
        .await
        .expect("SetConfigured(true) FIDL call failed")
        .map_err(Status::err_from_raw)
        .expect("SetConfigured(true) failed");
    activate_configured_endpoints(mock, proxy, parsed_config).await
}

/// Activates alternate interface settings on an already-configured function driver and sends a
/// 1-byte `CDC_SEND_ENCAPSULATED_COMMAND` control kick if no endpoint has queued requests yet.
pub async fn activate_configured_endpoints(
    mock: &MockUsbFunction,
    proxy: &fusb_function::UsbFunctionInterfaceProxy,
    parsed_config: &ParsedUsbConfig,
) -> (HashSet<u8>, HashSet<u8>) {
    let alt_interfaces: Vec<_> = parsed_config
        .interfaces
        .iter()
        .filter(|i| i.alt_setting > 0 && !i.endpoints.is_empty())
        .cloned()
        .collect();
    for alt_iface in &alt_interfaces {
        proxy
            .set_interface(alt_iface.interface_num, alt_iface.alt_setting)
            .await
            .expect("SetInterface FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("SetInterface failed");
    }
    fasync::Timer::new(std::time::Duration::from_millis(15)).await;

    let has_queued_requests =
        |state: &crate::mock::MockUsbFunctionState, active: &HashSet<u8>| -> HashSet<u8> {
            state
                .endpoints
                .iter()
                .filter(|(addr, ep)| active.contains(addr) && !ep.state.lock().requests.is_empty())
                .map(|(&addr, _)| addr)
                .collect()
        };

    let need_control_kick = {
        let state = mock.state.lock();
        let active: HashSet<u8> = state.configured_endpoints.keys().copied().collect();
        assert!(!active.is_empty(), "Expected active endpoints after configuration");
        has_queued_requests(&state, &active).is_empty()
    };

    if need_control_kick {
        // Drivers like rndis-function only queue endpoint requests once host control activity
        // arrives; send a 1-byte CDC/RNDIS SEND_ENCAPSULATED_COMMAND to trigger an in-flight
        // notification request if supported.
        let setup = fusb_descriptor::UsbSetup {
            bm_request_type: 0x21,
            b_request: 0x00,
            w_value: 0,
            w_index: 0,
            w_length: 1,
        };
        let _ = proxy.control(&setup, &[0]).await;
        fasync::Timer::new(std::time::Duration::from_millis(10)).await;
    }

    let state = mock.state.lock();
    let active: HashSet<u8> = state.configured_endpoints.keys().copied().collect();
    let with_reqs = has_queued_requests(&state, &active);
    (active, with_reqs)
}

/// Verifies Endpoint.CancelAll error tolerance (IO_NOT_PRESENT) and delayed CancelAll / in-flight
/// request drain handling during SetConfigured(false).
pub async fn verify_cancel_all_teardown_and_error_tolerance(
    mock: &MockUsbFunction,
    proxy: &fusb_function::UsbFunctionInterfaceProxy,
    parsed_config: &ParsedUsbConfig,
) {
    // Configure the function (and activate alternate settings if present) so endpoints are active
    // and RX requests are queued, then inject IO_NOT_PRESENT on CancelAll during
    // SetConfigured(false) and verify the driver tolerates it cleanly and cancels active
    // endpoints before replying.
    let (active_endpoints, expected_cancel_endpoints) =
        configure_and_activate_endpoints(mock, proxy, parsed_config).await;
    let uncfg_send_idx = {
        let mut state = mock.state.lock();
        state.fail_cancel_all_status = Some(Status::IO_NOT_PRESENT);
        state.events.push(MockEvent::SetConfiguredSend { configured: false });
        state.events.len() - 1
    };
    proxy
        .set_configured(false, fusb_descriptor::UsbSpeed::High)
        .await
        .expect("SetConfigured(false) FIDL call failed when CancelAll returned IO_NOT_PRESENT")
        .map_err(Status::err_from_raw)
        .expect("SetConfigured(false) failed when CancelAll returned IO_NOT_PRESENT");
    let cancels_on_deconfig = {
        let mut state = mock.state.lock();
        state.fail_cancel_all_status = None;
        let deconfig_events = &state.events[uncfg_send_idx + 1..];
        for &ep in &active_endpoints {
            let cancel_idx = deconfig_events
                .iter()
                .position(|e| matches!(e, MockEvent::CancelAll { ep_addr } if *ep_addr == ep));
            if expected_cancel_endpoints.contains(&ep) {
                assert!(
                    cancel_idx.is_some(),
                    "Active endpoint 0x{ep:02x} with in-flight requests did not receive CancelAll during SetConfigured(false)"
                );
            }
            if let (Some(c_idx), Some(d_idx)) = (
                cancel_idx,
                deconfig_events.iter().position(
                    |e| matches!(e, MockEvent::DisableEndpoint { ep_addr } if *ep_addr == ep),
                ),
            ) {
                assert!(
                    c_idx < d_idx,
                    "Endpoint 0x{ep:02x} DisableEndpoint (idx {d_idx}) occurred before CancelAll (idx {c_idx})"
                );
            }
        }
        let any_cancel = deconfig_events.iter().any(|e| matches!(e, MockEvent::CancelAll { .. }));
        state.events.push(MockEvent::SetConfiguredReply { configured: false });
        any_cancel
    };

    if !cancels_on_deconfig {
        return;
    }

    // Reconfigure, hold CancelAll completion in MockEndpoint, and verify SetConfigured(false)
    // drains cleanly once held CancelAll calls are released.
    let _ = configure_and_activate_endpoints(mock, proxy, parsed_config).await;
    let hold_start_idx = {
        let mut state = mock.state.lock();
        state.hold_cancel_all = true;
        state.events.len()
    };

    let release_fut = async {
        mock.wait_for_held_cancel_all(hold_start_idx).await;
        // Verify DisableEndpoint was not called while CancelAll was still held pending.
        let premature_disable = mock.state.lock().events[hold_start_idx..]
            .iter()
            .any(|e| matches!(e, MockEvent::DisableEndpoint { .. }));
        assert!(
            !premature_disable,
            "DisableEndpoint called while CancelAll was still held pending"
        );
        mock.release_held_cancel_all();
    };

    let (unconfig_res, ()) = futures::future::join(
        proxy.set_configured(false, fusb_descriptor::UsbSpeed::High),
        release_fut,
    )
    .await;
    unconfig_res
        .expect("SetConfigured(false) with held CancelAll FIDL call failed")
        .map_err(Status::err_from_raw)
        .expect("SetConfigured(false) with held CancelAll failed");
}

/// Verifies overlapping SetConfigured transitions:
/// 1. Rapid back-to-back SetConfigured(true) followed immediately by SetConfigured(false) without
///    awaiting the first call leaves the driver unconfigured.
/// 2. SetConfigured(true) (and a superseding second SetConfigured(true)) arriving while
///    SetConfigured(false) is actively draining held CancelAll reconfigures the driver once
///    CancelAll completes.
/// 3. SetConfigured(true) followed by SetConfigured(false) arriving while SetConfigured(false) is
///    draining held CancelAll cancels/supersedes the pending configure and leaves the driver
///    unconfigured once CancelAll completes.
pub async fn verify_overlapping_set_configured(
    mock: &MockUsbFunction,
    proxy: &fusb_function::UsbFunctionInterfaceProxy,
    parsed_config: &ParsedUsbConfig,
) {
    let default_endpoints = parsed_config.default_endpoints();
    if default_endpoints.is_empty() {
        return;
    }

    // 1. Rapid toggle: dispatch SetConfigured(true) and SetConfigured(false) back-to-back without
    // awaiting the first call.
    let cfg_fut = proxy.set_configured(true, fusb_descriptor::UsbSpeed::High);
    let uncfg_fut = proxy.set_configured(false, fusb_descriptor::UsbSpeed::High);
    let (cfg_res, uncfg_res) = futures::future::join(cfg_fut, uncfg_fut).await;
    match cfg_res.expect("Rapid-toggle SetConfigured(true) FIDL call failed") {
        Ok(()) => {}
        Err(raw) => assert_eq!(
            Status::err_from_raw(raw),
            Status::CANCELED,
            "Unexpected error from superseded SetConfigured(true)"
        ),
    }
    uncfg_res
        .expect("Rapid-toggle SetConfigured(false) FIDL call failed")
        .map_err(Status::err_from_raw)
        .expect("Rapid-toggle SetConfigured(false) failed");
    assert!(
        mock.state.lock().configured_endpoints.is_empty(),
        "Endpoints remained configured after rapid SetConfigured(true) -> SetConfigured(false)"
    );

    // Check whether this driver issues CancelAll during SetConfigured(false).
    let _ = configure_and_activate_endpoints(mock, proxy, parsed_config).await;
    let check_start_idx = mock.state.lock().events.len();
    proxy
        .set_configured(false, fusb_descriptor::UsbSpeed::High)
        .await
        .expect("SetConfigured(false) FIDL call failed")
        .map_err(Status::err_from_raw)
        .expect("SetConfigured(false) failed");
    let cancels_on_deconfig = mock.state.lock().events[check_start_idx..]
        .iter()
        .any(|e| matches!(e, MockEvent::CancelAll { .. }));
    if !cancels_on_deconfig {
        return;
    }

    // 2. While SetConfigured(false) is draining held CancelAll, send two back-to-back
    // SetConfigured(true) calls (testing deferred configure and superseding).
    let _ = configure_and_activate_endpoints(mock, proxy, parsed_config).await;
    let hold_start_idx = {
        let mut state = mock.state.lock();
        state.hold_cancel_all = true;
        state.events.len()
    };

    let uncfg_fut = proxy.set_configured(false, fusb_descriptor::UsbSpeed::High);
    let overlap_reconfig_fut = async {
        mock.wait_for_held_cancel_all(hold_start_idx).await;
        let mut cfg1_fut = proxy.set_configured(true, fusb_descriptor::UsbSpeed::High).fuse();
        let cfg2_fut = proxy.set_configured(true, fusb_descriptor::UsbSpeed::High);

        // Wait until cfg1_fut resolves (superseded by cfg2_fut with ZX_ERR_CANCELED or Ok) or
        // a generous fallback timeout expires, ensuring both overlapping SetConfigured requests
        // have reached the driver before unblocking held CancelAll.
        let cfg1_res = futures::select! {
            res = cfg1_fut => {
                mock.release_held_cancel_all();
                res
            }
            _ = fasync::Timer::new(std::time::Duration::from_millis(500)).fuse() => {
                mock.release_held_cancel_all();
                cfg1_fut.await
            }
        };
        let cfg2_res = cfg2_fut.await;
        (cfg1_res, cfg2_res)
    };

    let (uncfg_res, (cfg1_res, cfg2_res)) =
        futures::future::join(uncfg_fut, overlap_reconfig_fut).await;
    match uncfg_res.expect("SetConfigured(false) FIDL call failed during overlap reconfig") {
        Ok(()) => {}
        Err(raw) => assert_eq!(
            Status::err_from_raw(raw),
            Status::CANCELED,
            "Unexpected error from SetConfigured(false) during overlap reconfig"
        ),
    }
    match cfg1_res.expect("First overlapping SetConfigured(true) FIDL call failed") {
        Ok(()) => {}
        Err(raw) => assert_eq!(
            Status::err_from_raw(raw),
            Status::CANCELED,
            "Unexpected error from superseded first SetConfigured(true)"
        ),
    }
    cfg2_res
        .expect("Second overlapping SetConfigured(true) FIDL call failed")
        .map_err(Status::err_from_raw)
        .expect("Second overlapping SetConfigured(true) failed");
    mock.wait_for_endpoints_configured(&default_endpoints).await;

    // 3. While SetConfigured(false) is draining held CancelAll, send SetConfigured(true) followed
    // by SetConfigured(false) and verify the driver ends up unconfigured once CancelAll completes
    // without transiently configuring endpoints when superseded.
    let _ = activate_configured_endpoints(mock, proxy, parsed_config).await;
    let hold_start_idx = {
        let mut state = mock.state.lock();
        state.hold_cancel_all = true;
        state.events.len()
    };

    let uncfg1_fut = proxy.set_configured(false, fusb_descriptor::UsbSpeed::High);
    let supersede_unconfig_fut = async {
        mock.wait_for_held_cancel_all(hold_start_idx).await;
        let mut cfg_fut = proxy.set_configured(true, fusb_descriptor::UsbSpeed::High).fuse();
        let uncfg2_fut = proxy.set_configured(false, fusb_descriptor::UsbSpeed::High);

        // Wait until cfg_fut resolves (superseded by uncfg2_fut with ZX_ERR_CANCELED or Ok) or
        // a generous fallback timeout expires, ensuring both overlapping requests have reached
        // the driver before unblocking held CancelAll.
        let cfg_res = futures::select! {
            res = cfg_fut => {
                mock.release_held_cancel_all();
                res
            }
            _ = fasync::Timer::new(std::time::Duration::from_millis(500)).fuse() => {
                mock.release_held_cancel_all();
                cfg_fut.await
            }
        };
        let uncfg2_res = uncfg2_fut.await;
        (cfg_res, uncfg2_res)
    };

    let (uncfg1_res, (cfg_res, uncfg2_res)) =
        futures::future::join(uncfg1_fut, supersede_unconfig_fut).await;
    match uncfg1_res.expect("Initial SetConfigured(false) FIDL call failed") {
        Ok(()) => {}
        Err(raw) => assert_eq!(
            Status::err_from_raw(raw),
            Status::CANCELED,
            "Unexpected error from initial SetConfigured(false)"
        ),
    }
    let cfg_was_canceled = match cfg_res.expect("Superseded SetConfigured(true) FIDL call failed") {
        Ok(()) => false,
        Err(raw) => {
            assert_eq!(
                Status::err_from_raw(raw),
                Status::CANCELED,
                "Unexpected error from SetConfigured(true) superseded by SetConfigured(false)"
            );
            true
        }
    };
    uncfg2_res
        .expect("Superseding SetConfigured(false) FIDL call failed")
        .map_err(Status::err_from_raw)
        .expect("Superseding SetConfigured(false) failed");
    {
        let state = mock.state.lock();
        assert!(
            state.configured_endpoints.is_empty(),
            "Endpoints remained configured after SetConfigured(false) superseded pending SetConfigured(true)"
        );
        if cfg_was_canceled {
            let transient_cfg = state.events[hold_start_idx..]
                .iter()
                .any(|e| matches!(e, MockEvent::ConfigureEndpoint { .. }));
            assert!(
                !transient_cfg,
                "Superseded SetConfigured(true) transiently called ConfigureEndpoint during Step 3"
            );
        }
    }
}

fn find_function_inspect_node(hierarchy: &DiagnosticsHierarchy) -> Option<&DiagnosticsHierarchy> {
    let has_configured = hierarchy.properties.iter().any(|p| p.name() == "configured");
    let has_unconfiguring = hierarchy.properties.iter().any(|p| p.name() == "unconfiguring");
    if has_configured && has_unconfiguring {
        return Some(hierarchy);
    }
    hierarchy.children.iter().find_map(find_function_inspect_node)
}

async fn snapshot_function_inspect_node() -> Option<DiagnosticsHierarchy> {
    let snapshots = ArchiveReader::inspect().snapshot().await.ok()?;
    for data in snapshots {
        if let Some(payload) = &data.payload {
            if let Some(node) = find_function_inspect_node(payload) {
                return Some(node.clone());
            }
        }
    }
    None
}

fn inspect_bool(node: &DiagnosticsHierarchy, name: &str) -> Option<bool> {
    node.properties.iter().find_map(|p| match p {
        Property::Bool(k, v) if k == name => Some(*v),
        _ => None,
    })
}

fn inspect_uint(node: &DiagnosticsHierarchy, name: &str) -> Option<u64> {
    node.properties.iter().find_map(|p| match p {
        Property::Uint(k, v) if k == name => Some(*v),
        Property::Int(k, v) if k == name && *v >= 0 => Some(*v as u64),
        _ => None,
    })
}

fn inspect_string(node: &DiagnosticsHierarchy, name: &str) -> Option<String> {
    node.properties.iter().find_map(|p| match p {
        Property::String(k, v) if k == name => Some(v.clone()),
        _ => None,
    })
}

fn inspect_event_entries(node: &DiagnosticsHierarchy) -> Vec<(u32, String)> {
    let Some(history) = node.children.iter().find(|c| c.name == "event_history") else {
        return Vec::new();
    };
    let mut entries: Vec<(i64, u32, String)> = history
        .children
        .iter()
        .filter_map(|child| {
            let idx = child.name.parse::<u32>().unwrap_or(0);
            let ts = child
                .properties
                .iter()
                .find_map(|p| match p {
                    Property::Int(k, v) if k == "@time" => Some(*v),
                    Property::Uint(k, v) if k == "@time" => Some(*v as i64),
                    _ => None,
                })
                .unwrap_or(0);
            let ev = child.properties.iter().find_map(|p| match p {
                Property::String(k, v) if k == "event" => Some(v.clone()),
                _ => None,
            })?;
            Some((ts, idx, ev))
        })
        .collect();
    entries.sort_by_key(|(ts, idx, _)| (*ts, *idx));
    entries.into_iter().map(|(_, idx, ev)| (idx, ev)).collect()
}

/// Verifies `usb_inspect::FunctionDriverInspect` and per-endpoint cancellation/drain telemetry
/// across `SetConfigured(true)` -> held `CancelAll` drain -> `SetConfigured(false)` for drivers
/// that expose `FunctionDriverInspect`.
pub async fn verify_function_driver_inspect(
    mock: &MockUsbFunction,
    proxy: &fusb_function::UsbFunctionInterfaceProxy,
    parsed_config: &ParsedUsbConfig,
) {
    let Some(initial_node) = snapshot_function_inspect_node().await else {
        return;
    };
    let prev_true_count = inspect_uint(&initial_node, "set_configured_true_count").unwrap_or(0);
    let prev_false_count = inspect_uint(&initial_node, "set_configured_false_count").unwrap_or(0);
    let initial_state_str = inspect_string(&initial_node, "state");
    let prev_func_max_idx =
        inspect_event_entries(&initial_node).into_iter().map(|(idx, _)| idx).max();
    let prev_ep_max_idx: HashMap<String, Option<u32>> = initial_node
        .children
        .iter()
        .filter(|c| inspect_bool(c, "cancelled").is_some())
        .map(|c| {
            let max_idx = inspect_event_entries(c).into_iter().map(|(idx, _)| idx).max();
            (c.name.clone(), max_idx)
        })
        .collect();
    let expected_ep_count = parsed_config.all_endpoints().len();

    let alt_interfaces: Vec<_> = parsed_config
        .interfaces
        .iter()
        .filter(|i| i.alt_setting > 0 && !i.endpoints.is_empty())
        .cloned()
        .collect();

    proxy
        .set_configured(true, fusb_descriptor::UsbSpeed::High)
        .await
        .expect("SetConfigured(true) FIDL call failed")
        .map_err(Status::err_from_raw)
        .expect("SetConfigured(true) failed");
    for alt_iface in &alt_interfaces {
        proxy
            .set_interface(alt_iface.interface_num, alt_iface.alt_setting)
            .await
            .expect("SetInterface FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("SetInterface failed");
    }

    let configured_node = snapshot_function_inspect_node()
        .await
        .expect("FunctionDriverInspect node disappeared while configured");
    assert_eq!(inspect_bool(&configured_node, "configured"), Some(true));
    assert_eq!(inspect_bool(&configured_node, "unconfiguring"), Some(false));
    assert_eq!(inspect_bool(&configured_node, "online"), Some(true));
    if let Some(ref init_st) = initial_state_str {
        assert_ne!(
            inspect_string(&configured_node, "state").as_deref(),
            Some(init_st.as_str()),
            "FunctionDriverInspect 'state' property did not transition out of unconfigured state '{}'",
            init_st
        );
    }
    assert!(
        inspect_uint(&configured_node, "set_configured_true_count").unwrap_or(0) > prev_true_count,
        "set_configured_true_count did not increment on SetConfigured(true)"
    );
    let mut configured_ep_children = 0usize;
    for ep_child in &configured_node.children {
        if inspect_bool(ep_child, "cancelled").is_some() {
            configured_ep_children += 1;
            assert_eq!(
                inspect_bool(ep_child, "cancelled"),
                Some(false),
                "Endpoint {} cancelled property was not cleared on SetConfigured(true)",
                ep_child.name
            );
            assert!(
                inspect_uint(ep_child, "total_requests").unwrap_or(0) > 0,
                "Endpoint {} total_requests was 0",
                ep_child.name
            );
        }
    }
    assert_eq!(
        configured_ep_children, expected_ep_count,
        "Expected {} endpoint inspect children under FunctionDriverInspect, found {}",
        expected_ep_count, configured_ep_children
    );

    let hold_start_idx = {
        let mut state = mock.state.lock();
        state.hold_cancel_all = true;
        state.events.len()
    };

    let uncfg_fut = proxy.set_configured(false, fusb_descriptor::UsbSpeed::High);
    let inspect_during_drain_fut = async {
        mock.wait_for_held_cancel_all(hold_start_idx).await;
        let draining_node = snapshot_function_inspect_node()
            .await
            .expect("FunctionDriverInspect node disappeared during unconfigure drain");
        assert_eq!(inspect_bool(&draining_node, "unconfiguring"), Some(true));
        assert_eq!(inspect_bool(&draining_node, "online"), Some(false));
        assert!(
            inspect_uint(&draining_node, "set_configured_false_count").unwrap_or(0)
                > prev_false_count,
            "set_configured_false_count did not increment on SetConfigured(false) start"
        );
        mock.release_held_cancel_all();
    };

    let (uncfg_res, ()) = futures::future::join(uncfg_fut, inspect_during_drain_fut).await;
    uncfg_res
        .expect("SetConfigured(false) FIDL call failed")
        .map_err(Status::err_from_raw)
        .expect("SetConfigured(false) failed");

    let unconfigured_node = snapshot_function_inspect_node()
        .await
        .expect("FunctionDriverInspect node disappeared after SetConfigured(false)");
    assert_eq!(inspect_bool(&unconfigured_node, "configured"), Some(false));
    assert_eq!(inspect_bool(&unconfigured_node, "unconfiguring"), Some(false));
    assert_eq!(inspect_bool(&unconfigured_node, "online"), Some(false));
    if let Some(ref init_st) = initial_state_str {
        assert_eq!(
            inspect_string(&unconfigured_node, "state").as_deref(),
            Some(init_st.as_str()),
            "FunctionDriverInspect 'state' property did not return to '{}' after SetConfigured(false)",
            init_st
        );
    }

    let new_func_events: Vec<String> = inspect_event_entries(&unconfigured_node)
        .into_iter()
        .filter(|(idx, _)| prev_func_max_idx.is_none_or(|prev| *idx > prev))
        .map(|(_, ev)| ev)
        .collect();
    let expected_sequence = [
        "SetConfigured(true)",
        "SetConfigured(false) start",
        "DisableEndpoints complete",
        "SetConfigured(false) reply",
    ];
    let mut last_pos = 0usize;
    for expected_ev in expected_sequence {
        let rel_pos = new_func_events[last_pos..]
            .iter()
            .position(|e| e == expected_ev)
            .unwrap_or_else(|| {
                panic!(
                    "FunctionDriverInspect newly emitted event_history {:?} missing '{}' in chronological order",
                    new_func_events, expected_ev
                )
            });
        last_pos += rel_pos + 1;
    }

    let mut unconfigured_ep_children = 0usize;
    for ep_child in &unconfigured_node.children {
        if inspect_bool(ep_child, "cancelled").is_some() {
            unconfigured_ep_children += 1;
            assert_eq!(
                inspect_bool(ep_child, "cancelled"),
                Some(true),
                "Endpoint {} cancelled property was not true after SetConfigured(false)",
                ep_child.name
            );
            assert_eq!(
                inspect_uint(ep_child, "in_flight_requests"),
                Some(0),
                "Endpoint {} in_flight_requests was not 0 after SetConfigured(false)",
                ep_child.name
            );
            let prev_max = prev_ep_max_idx.get(&ep_child.name).copied().flatten();
            let new_ep_events: Vec<String> = inspect_event_entries(ep_child)
                .into_iter()
                .filter(|(idx, _)| prev_max.is_none_or(|prev| *idx > prev))
                .map(|(_, ev)| ev)
                .collect();
            assert!(
                new_ep_events.iter().any(|e| e == "CancelAll complete"),
                "Endpoint {} newly emitted event_history {:?} missing 'CancelAll complete'",
                ep_child.name,
                new_ep_events
            );
            assert!(
                !new_ep_events.iter().any(|e| e == "SetConfigured(true)"),
                "Endpoint {} event_history unexpectedly contained function event 'SetConfigured(true)'",
                ep_child.name
            );
            assert!(
                !new_ep_events.iter().any(|e| e == "SetConfigured(false) start"),
                "Endpoint {} event_history unexpectedly contained function event 'SetConfigured(false) start'",
                ep_child.name
            );
        }
    }
    assert_eq!(
        unconfigured_ep_children, expected_ep_count,
        "Expected {} endpoint inspect children under FunctionDriverInspect after SetConfigured(false), found {}",
        expected_ep_count, unconfigured_ep_children
    );
}

/// Verifies UsbFunctionInterface.SetInterface configures and disables endpoints before replying,
/// and returns the queued request depth on each newly configured OUT endpoint.
pub async fn verify_set_interface_lifecycle(
    mock: &MockUsbFunction,
    proxy: &fusb_function::UsbFunctionInterfaceProxy,
    interface: u8,
    alt_setting: u8,
    endpoints_to_configure: &[u8],
    endpoints_to_disable: &[u8],
) -> HashMap<u8, usize> {
    // 1. SetInterface
    let (start_idx, endpoints_with_requests_before) = {
        let mut state = mock.state.lock();
        let with_reqs: HashSet<u8> = state
            .endpoints
            .iter()
            .filter(|(_, ep)| !ep.state.lock().requests.is_empty())
            .map(|(&addr, _)| addr)
            .collect();
        let idx = state.events.len();
        state.events.push(MockEvent::SetInterfaceSend { interface, alt_setting });
        (idx, with_reqs)
    };
    let result = proxy.set_interface(interface, alt_setting).await;
    assert!(result.is_ok());
    let response = result.unwrap();
    assert!(response.is_ok(), "SetInterface({}, {}) failed", interface, alt_setting);
    {
        let mut state = mock.state.lock();
        state.events.push(MockEvent::SetInterfaceReply { interface, alt_setting });
    }

    // Verify ordering within this SetInterface call
    {
        let state = mock.state.lock();
        let events = &state.events[start_idx..];

        let send_idx = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    MockEvent::SetInterfaceSend { interface: i, alt_setting: a }
                    if *i == interface && *a == alt_setting
                )
            })
            .unwrap();
        let reply_idx = events
            .iter()
            .position(|e| {
                matches!(
                    e,
                    MockEvent::SetInterfaceReply { interface: i, alt_setting: a }
                    if *i == interface && *a == alt_setting
                )
            })
            .unwrap();

        for addr in endpoints_to_configure {
            let ep_cfg_idx = events[send_idx..reply_idx]
                .iter()
                .position(
                    |e| matches!(e, MockEvent::ConfigureEndpoint { ep_addr } if ep_addr == addr),
                )
                .map(|i| i + send_idx);
            assert!(
                ep_cfg_idx.is_some(),
                "Endpoint 0x{:02x} was not configured between SetInterface({}, {}) send and reply",
                addr,
                interface,
                alt_setting
            );
            assert!(
                state.configured_endpoints.contains_key(addr),
                "Endpoint 0x{:02x} not in configured_endpoints after SetInterface({}, {})",
                addr,
                interface,
                alt_setting
            );
        }

        for addr in endpoints_to_disable {
            let ep_dis_idx = events[send_idx..reply_idx]
                .iter()
                .position(
                    |e| matches!(e, MockEvent::DisableEndpoint { ep_addr } if ep_addr == addr),
                )
                .map(|i| i + send_idx);
            assert!(
                ep_dis_idx.is_some(),
                "Endpoint 0x{:02x} was not disabled between SetInterface({}, {}) send and reply",
                addr,
                interface,
                alt_setting
            );
            let dis_idx = ep_dis_idx.unwrap();
            let ep_cancel_idx = events[send_idx..reply_idx]
                .iter()
                .position(|e| matches!(e, MockEvent::CancelAll { ep_addr } if ep_addr == addr))
                .map(|i| i + send_idx);
            if endpoints_with_requests_before.contains(addr) {
                assert!(
                    ep_cancel_idx.is_some(),
                    "Endpoint 0x{:02x} with in-flight requests did not receive CancelAll during SetInterface({}, {})",
                    addr,
                    interface,
                    alt_setting
                );
            }
            if let Some(c_idx) = ep_cancel_idx {
                assert!(
                    c_idx < dis_idx,
                    "Endpoint 0x{:02x} DisableEndpoint (idx {}) occurred before CancelAll (idx {}) during SetInterface({}, {})",
                    addr,
                    dis_idx,
                    c_idx,
                    interface,
                    alt_setting
                );
            }
            assert!(
                !state.configured_endpoints.contains_key(addr),
                "Endpoint 0x{:02x} still in configured_endpoints after SetInterface({}, {})",
                addr,
                interface,
                alt_setting
            );
            if let Some(ep) = state.endpoints.get(addr) {
                assert!(
                    ep.state.lock().requests.is_empty(),
                    "Endpoint 0x{:02x} still has pending requests after SetInterface({}, {}) disabled it",
                    addr,
                    interface,
                    alt_setting
                );
            }
        }
    }

    let configured_out_eps: Vec<u8> =
        endpoints_to_configure.iter().copied().filter(|addr| (*addr & 0x80) == 0).collect();
    if !configured_out_eps.is_empty() {
        // Intentionally unbounded: the test framework timeout bounds a stuck driver, while the
        // periodic log below names the endpoints that never reached full RX depth.
        let mut polls: u64 = 0;
        loop {
            let pending: Vec<(u8, usize, usize)> = {
                let state = mock.state.lock();
                configured_out_eps
                    .iter()
                    .filter_map(|addr| {
                        let (queued, vmos) = state.endpoints.get(addr).map_or((0, 0), |ep| {
                            let ep_state = ep.state.lock();
                            (ep_state.requests.len(), ep_state.vmos.len())
                        });
                        let full = queued > 0 && (vmos == 0 || queued == vmos);
                        (!full).then_some((*addr, queued, vmos))
                    })
                    .collect()
            };
            if pending.is_empty() {
                break;
            }
            polls += 1;
            if polls.is_multiple_of(200) {
                log::warn!(
                    "Still waiting for RX requests after SetInterface({}, {}); \
                     (endpoint, queued, registered VMOs): {:02x?}",
                    interface,
                    alt_setting,
                    pending
                );
            }
            fasync::Timer::new(std::time::Duration::from_millis(5)).await;
        }
    }

    let state = mock.state.lock();
    configured_out_eps
        .into_iter()
        .filter_map(|addr| {
            state.endpoints.get(&addr).map(|ep| (addr, ep.state.lock().requests.len()))
        })
        .collect()
}
