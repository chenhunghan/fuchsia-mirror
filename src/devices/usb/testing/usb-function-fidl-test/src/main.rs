// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Result;
use fidl::endpoints::ServiceMarker;
use fidl_fuchsia_driver_test as fdt;
use fidl_fuchsia_hardware_usb_function as fusb_function;
use fuchsia_async as fasync;
use fuchsia_component::server::ServiceFs;
use fuchsia_component_test::{
    Capability, ChildOptions, LocalComponentHandles, RealmBuilder, RealmInstance, Ref, Route,
};
use fuchsia_driver_test::{DriverTestRealmBuilder2, DriverTestRealmInstance2, Options2};
use futures::{FutureExt, StreamExt};
use log::*;
use std::sync::Arc;
use usb_function_fidl_test::mock::{MockEvent, MockUsbFunction};
use usb_function_fidl_test::{
    configure_and_activate_endpoints, fusb_descriptor, parsed_config,
    verify_cancel_all_teardown_and_error_tolerance, verify_control_latency,
    verify_control_unsupported, verify_endpoint_closure_resilience,
    verify_endpoint_error_handling_and_rollback, verify_function_closure_resilience,
    verify_function_driver_inspect, verify_initialization, verify_overlapping_set_configured,
    verify_set_configured_idempotency_and_reconfigure, verify_set_configured_lifecycle,
    verify_set_interface_lifecycle, verify_set_interface_unsupported,
};

async fn serve_usb_function(
    handles: LocalComponentHandles,
    mock: Arc<MockUsbFunction>,
) -> Result<()> {
    let mut fs = ServiceFs::new();
    let mock_clone = mock.clone();

    fs.dir("svc")
        .dir(fusb_function::UsbFunctionServiceMarker::SERVICE_NAME)
        .dir("default")
        .add_service_at("device", move |channel: zx::Channel| {
            let server_end =
                fidl::endpoints::ServerEnd::<fusb_function::UsbFunctionMarker>::new(channel);
            mock_clone.start(server_end);
            Some(())
        });

    fs.serve_connection(handles.outgoing_dir)?;
    fs.collect::<()>().await;
    Ok(())
}

const PERIPHERAL_CONFIG_CAPABILITIES: [&str; 5] = [
    "fuchsia.usb.test.UsbClass",
    "fuchsia.usb.test.UsbSubclass",
    "fuchsia.usb.test.UsbProtocol",
    "fuchsia.usb.test.UsbVid",
    "fuchsia.usb.test.UsbPid",
];

async fn build_driver_realm(mock: Arc<MockUsbFunction>) -> Result<RealmInstance> {
    let builder = RealmBuilder::new().await?;

    let mock_clone = mock.clone();
    let provider = builder
        .add_local_child(
            "usb_function_mock_provider",
            move |handles| serve_usb_function(handles, mock_clone.clone()).boxed(),
            ChildOptions::new(),
        )
        .await?;

    builder
        .add_route(
            Route::new()
                .capability(Capability::protocol_by_name("fuchsia.logger.LogSink"))
                .from(Ref::parent())
                .to(&provider),
        )
        .await?;

    // Offer UsbFunctionService (and optional per-package peripheral config overrides) to DTR.
    // Packages that do not bundle `fake-usb-peripheral-config.cm` rely on `fake-usb-peripheral`'s
    // inline CML defaults.
    let mut offers = vec![Capability::service::<fusb_function::UsbFunctionServiceMarker>().into()];
    if std::path::Path::new("/pkg/meta/fake-usb-peripheral-config.cm").exists() {
        let peripheral_config = builder
            .add_child(
                "peripheral_config",
                "#meta/fake-usb-peripheral-config.cm",
                ChildOptions::new(),
            )
            .await?;

        let config_decl = builder.get_component_decl(&peripheral_config).await?;
        let mut provider_decl = builder.get_component_decl(&provider).await?;
        provider_decl.capabilities = provider_decl
            .capabilities
            .into_vec()
            .into_iter()
            .chain(config_decl.capabilities)
            .collect();
        provider_decl.exposes =
            provider_decl.exposes.into_vec().into_iter().chain(config_decl.exposes).collect();
        builder.replace_component_decl(&provider, provider_decl).await?;

        offers.extend(
            PERIPHERAL_CONFIG_CAPABILITIES
                .into_iter()
                .map(|cap| Capability::configuration(cap).into()),
        );
    }

    let dtr_args = fdt::RealmArgs {
        root_driver: Some("fuchsia-boot:///dtr#meta/test-parent-sys.cm".to_string()),
        ..Default::default()
    };

    builder
        .driver_test_realm_setup(
            Options2::new().driver_offers((&provider).into(), offers),
            dtr_args,
        )
        .await?;

    let realm = builder.build().await?;
    realm.wait_for_bootup().await?;
    Ok(realm)
}

async fn run_fidl_test() -> Result<()> {
    // Setup mock UsbFunction
    let mock = Arc::new(MockUsbFunction::new());
    let realm = build_driver_realm(mock.clone()).await?;

    // Wait for driver to configure
    info!("Waiting for driver to configure...");
    let interface_proxy = mock.wait_for_interface().await;
    info!("Driver configured! Running function FIDL tests...");

    // NOTE: All verification steps below intentionally share a single booted DriverTestRealm
    // instance to avoid the CI overhead of spinning up a fresh realm for each individual check.
    // Because the realm is stateful, each step must leave the driver and MockUsbFunction in a
    // well-defined state for the next step:
    //   1. Initial state after boot: AllocResources and Configure completed, unconfigured
    //      (SetConfigured(false)), all allocated endpoint channels open.
    //   2. verify_initialization / verify_control_* / verify_set_interface_unsupported:
    //      Read-only or rejected calls; driver remains unconfigured with endpoints open.
    //   3. verify_set_configured_lifecycle:
    //      Transitions unconfigured -> SetConfigured(true) -> SetConfigured(false); leaves
    //      driver unconfigured with endpoints disabled.
    //   4. verify_set_interface_lifecycle (for any interface with alt_setting > 0):
    //      Configures driver, toggles alternate settings (alt -> 0 -> alt -> 0), and returns
    //      driver to SetConfigured(false).
    //   5. verify_cancel_all_teardown_and_error_tolerance, verify_overlapping_set_configured,
    //      and verify_function_driver_inspect:
    //      Exercise CancelAll error tolerance and delayed drain, overlapping SetConfigured calls,
    //      and FunctionDriverInspect telemetry; each leaves driver unconfigured.
    //   6. verify_function_closure_resilience and verify_endpoint_closure_resilience (must run
    //      last before teardown):
    //      Configures driver (and any alternate settings), closes the server-side UsbFunction
    //      channel to verify SetConfigured(false) and subsequent Stop()/PrepareStop() tolerate
    //      framework/transport PEER_CLOSED errors, then drops all server-side MockEndpoint
    //      channels in `mock.state.endpoints` and verifies the driver remains responsive on
    //      UsbFunctionInterface.
    verify_initialization(&mock);

    // Parse configuration dynamically from descriptors
    let parsed_config = parsed_config(&mock);
    let default_endpoints = parsed_config.default_endpoints();

    verify_control_latency(&interface_proxy).await;
    verify_control_unsupported(&interface_proxy).await;
    for (unsupported_intf, unsupported_alt) in parsed_config.unsupported_interface_alts() {
        verify_set_interface_unsupported(&interface_proxy, unsupported_intf, unsupported_alt).await;
    }

    verify_set_configured_lifecycle(&mock, &interface_proxy, &default_endpoints).await;
    verify_set_configured_idempotency_and_reconfigure(&mock, &interface_proxy, &parsed_config)
        .await;
    verify_endpoint_error_handling_and_rollback(&mock, &interface_proxy, &default_endpoints).await;

    let alt_interfaces: Vec<_> = parsed_config
        .interfaces
        .iter()
        .filter(|i| i.alt_setting > 0 && !i.endpoints.is_empty())
        .cloned()
        .collect();
    if !alt_interfaces.is_empty() {
        info!("Running SetInterface alternate-setting lifecycle tests...");
        let result = interface_proxy.set_configured(true, fusb_descriptor::UsbSpeed::High).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_ok());
        for alt_iface in &alt_interfaces {
            let default_iface_endpoints =
                parsed_config.interface_endpoints(alt_iface.interface_num, 0);
            let to_disable_on_activate: Vec<u8> = default_iface_endpoints
                .iter()
                .copied()
                .filter(|ep| !alt_iface.endpoints.contains(ep))
                .collect();
            let to_disable_on_reset: Vec<u8> = alt_iface
                .endpoints
                .iter()
                .copied()
                .filter(|ep| !default_iface_endpoints.contains(ep))
                .collect();

            let initial_rx_depths = verify_set_interface_lifecycle(
                &mock,
                &interface_proxy,
                alt_iface.interface_num,
                alt_iface.alt_setting,
                &alt_iface.endpoints,
                &to_disable_on_activate,
            )
            .await;
            let _ = verify_set_interface_lifecycle(
                &mock,
                &interface_proxy,
                alt_iface.interface_num,
                0,
                &default_iface_endpoints,
                &to_disable_on_reset,
            )
            .await;
            let requeued_rx_depths = verify_set_interface_lifecycle(
                &mock,
                &interface_proxy,
                alt_iface.interface_num,
                alt_iface.alt_setting,
                &alt_iface.endpoints,
                &to_disable_on_activate,
            )
            .await;
            // Re-enabling the alternate setting after a disable must re-queue each OUT endpoint
            // back to its original RX depth.
            assert_eq!(
                requeued_rx_depths, initial_rx_depths,
                "RX queue depth changed after SetInterface({}, 0) -> SetInterface({}, {})",
                alt_iface.interface_num, alt_iface.interface_num, alt_iface.alt_setting
            );
            let _ = verify_set_interface_lifecycle(
                &mock,
                &interface_proxy,
                alt_iface.interface_num,
                0,
                &default_iface_endpoints,
                &to_disable_on_reset,
            )
            .await;
        }
        let result = interface_proxy.set_configured(false, fusb_descriptor::UsbSpeed::High).await;
        assert!(result.is_ok());
        assert!(result.unwrap().is_ok());
    }

    verify_cancel_all_teardown_and_error_tolerance(&mock, &interface_proxy, &parsed_config).await;
    verify_overlapping_set_configured(&mock, &interface_proxy, &parsed_config).await;
    verify_function_driver_inspect(&mock, &interface_proxy, &parsed_config).await;
    verify_function_closure_resilience(&mock, &interface_proxy, &parsed_config).await;
    verify_endpoint_closure_resilience(&mock, &interface_proxy).await;

    realm.destroy().await?;
    Ok(())
}

/// Verifies driver Stop()/PrepareStop() ordering and clean drain when realm teardown occurs while
/// SetConfigured(false) is actively draining held CancelAll calls.
async fn run_teardown_while_draining_test() -> Result<()> {
    let mock = Arc::new(MockUsbFunction::new());
    let realm = build_driver_realm(mock.clone()).await?;
    let interface_proxy = mock.wait_for_interface().await;
    let parsed_config = parsed_config(&mock);

    let _ = configure_and_activate_endpoints(&mock, &interface_proxy, &parsed_config).await;

    let check_start_idx = mock.state.lock().events.len();
    interface_proxy
        .set_configured(false, fusb_descriptor::UsbSpeed::High)
        .await?
        .map_err(zx::Status::err_from_raw)?;
    let cancels_on_deconfig = mock.state.lock().events[check_start_idx..]
        .iter()
        .any(|e| matches!(e, MockEvent::CancelAll { .. }));
    if !cancels_on_deconfig {
        realm.destroy().await?;
        return Ok(());
    }

    // Phase 1: Partial-drain and full-drain barrier check on SetConfigured(false).
    let _ = configure_and_activate_endpoints(&mock, &interface_proxy, &parsed_config).await;
    let hold_start_idx = {
        let mut state = mock.state.lock();
        state.hold_cancel_all = true;
        state.events.len()
    };

    let mut uncfg_fut =
        std::pin::pin!(interface_proxy.set_configured(false, fusb_descriptor::UsbSpeed::High));
    assert!(
        futures::poll!(&mut uncfg_fut).is_pending(),
        "SetConfigured(false) completed synchronously while CancelAll was held"
    );
    mock.wait_for_held_cancel_all(hold_start_idx).await;
    // Allow all parallel endpoint CancelAll calls from SetConfigured(false) to arrive.
    fasync::Timer::new(std::time::Duration::from_millis(10)).await;
    assert!(
        futures::poll!(&mut uncfg_fut).is_pending(),
        "SetConfigured(false) completed while CancelAll was still held pending"
    );

    let had_early_disable = mock.state.lock().events[hold_start_idx..]
        .iter()
        .any(|e| matches!(e, MockEvent::DisableEndpoint { .. }));
    assert!(
        !mock.state.lock().events[hold_start_idx..]
            .iter()
            .any(|e| matches!(e, MockEvent::Deconfigure)),
        "Deconfigure called while CancelAll was still held pending (before partial release)"
    );

    // Partial-drain barrier check: if multiple endpoints have held CancelAll calls, release
    // one endpoint and verify SetConfigured(false) remains pending and neither Deconfigure nor
    // premature DisableEndpoint fires while the remaining endpoint(s) are still draining.
    if mock.state.lock().held_cancel_responders.len() >= 2 {
        let _ = mock.release_one_held_cancel_all();
        fasync::Timer::new(std::time::Duration::from_millis(10)).await;
        assert!(
            futures::poll!(&mut uncfg_fut).is_pending(),
            "SetConfigured(false) completed after releasing only one endpoint while others remained held"
        );
        let state = mock.state.lock();
        let events = &state.events[hold_start_idx..];
        assert!(
            !events.iter().any(|e| matches!(e, MockEvent::Deconfigure)),
            "Deconfigure called after releasing one endpoint while others remained held"
        );
        if !had_early_disable {
            assert!(
                !events.iter().any(|e| matches!(e, MockEvent::DisableEndpoint { .. })),
                "DisableEndpoint called after releasing one endpoint while others remained held"
            );
        }
    }

    mock.release_held_cancel_all();
    uncfg_fut.await?.map_err(zx::Status::err_from_raw)?;
    {
        let state = mock.state.lock();
        assert!(
            state.configured_endpoints.is_empty(),
            "Endpoints remained configured after SetConfigured(false) drain completed"
        );
        for (addr, ep) in &state.endpoints {
            assert!(
                ep.state.lock().requests.is_empty(),
                "Endpoint 0x{:02x} still has pending requests after SetConfigured(false) drain",
                addr
            );
        }
    }

    // Phase 2: Driver Stop()/PrepareStop() colliding with an in-flight SetConfigured(false) drain.
    let _ = configure_and_activate_endpoints(&mock, &interface_proxy, &parsed_config).await;
    let hold_start_idx = {
        let mut state = mock.state.lock();
        state.hold_cancel_all = true;
        state.events.len()
    };

    let uncfg_fut = interface_proxy.set_configured(false, fusb_descriptor::UsbSpeed::High);
    let teardown_fut = async {
        mock.wait_for_held_cancel_all(hold_start_idx).await;
        fasync::Timer::new(std::time::Duration::from_millis(10)).await;

        let check_no_premature_deconfig = |phase: &str| {
            let state = mock.state.lock();
            let events = &state.events[hold_start_idx..];
            let premature_deconfig = events.iter().any(|e| matches!(e, MockEvent::Deconfigure));
            assert!(
                !premature_deconfig,
                "Deconfigure called while CancelAll was still held pending ({})",
                phase
            );
        };

        check_no_premature_deconfig("before Stop() while CancelAll is held");

        let release_fut = async {
            fasync::Timer::new(std::time::Duration::from_millis(20)).await;
            check_no_premature_deconfig("during driver Stop() while CancelAll is held");
            mock.release_held_cancel_all();
        };
        let (destroy_res, ()) = futures::future::join(realm.destroy(), release_fut).await;
        destroy_res
    };

    let (uncfg_res, destroy_res) = futures::future::join(uncfg_fut, teardown_fut).await;
    destroy_res?;
    match uncfg_res {
        Ok(Ok(())) => {}
        Ok(Err(raw)) => assert_eq!(
            zx::Status::err_from_raw(raw),
            zx::Status::CANCELED,
            "Unexpected domain error from SetConfigured(false) interrupted by driver Stop()"
        ),
        Err(e) if e.is_closed() => {}
        Err(e) => panic!("Unexpected FIDL error on SetConfigured(false) during Stop(): {:?}", e),
    }

    let state = mock.state.lock();
    for (addr, ep) in &state.endpoints {
        assert!(
            ep.state.lock().requests.is_empty(),
            "Endpoint 0x{:02x} still has pending requests after teardown during drain",
            addr
        );
    }
    Ok(())
}

/// Verifies that driver Start() fails gracefully without crashing or leaking handles when
/// UsbFunction.Configure returns an error (ZX_ERR_BAD_STATE).
async fn run_configure_failure_test() -> Result<()> {
    let mock = Arc::new(MockUsbFunction::new());
    mock.state.lock().fail_configure_status = Some(zx::Status::BAD_STATE);
    let realm = build_driver_realm(mock.clone()).await?;
    mock.wait_for_configure_attempt().await;
    mock.wait_for_function_closed().await;
    realm.destroy().await?;
    Ok(())
}

/// Returns the total handle count in the test harness process (`usb_function_fidl_test_bin`).
/// Note that the USB function driver under test runs inside `DriverTestRealm`'s `driver_host`
/// process rather than this process; this check verifies that the test harness and in-process
/// `MockUsbFunction` server clean up all VMO, channel, and realm handles after `realm.destroy()`.
fn handle_count() -> usize {
    let self_proc = fuchsia_runtime::process_self();
    let stats = self_proc.handle_stats().expect("failed to get handle stats");
    stats.handle_count.iter().copied().map(|n| n as usize).sum()
}

async fn wait_for_handles_below(max_handles: usize) -> usize {
    let mut attempts = 0usize;
    loop {
        let current = handle_count();
        if current <= max_handles {
            return current;
        }
        if attempts % 100 == 0 {
            warn!(
                "Waiting for test-harness handle count ({}) to drop to <= {}...",
                current, max_handles
            );
        }
        attempts += 1;
        fasync::Timer::new(std::time::Duration::from_millis(10)).await;
    }
}

#[fuchsia::main]
async fn main() -> Result<()> {
    let baseline_handles = handle_count();
    info!("Baseline test-harness handle count: {}", baseline_handles);

    // A small margin of 5 handles is allowed for runtime components / logger
    // connections that may persist dynamically in the test harness process.
    let margin = 5;

    info!("=== Running USB Function FIDL Tests ===");
    run_fidl_test().await?;
    run_teardown_while_draining_test().await?;
    run_configure_failure_test().await?;

    let final_handles = wait_for_handles_below(baseline_handles + margin).await;
    info!("Test-harness handle count after realm teardown: {}", final_handles);

    info!("All function FIDL tests passed (no harness handle leaks detected)!");
    Ok(())
}
