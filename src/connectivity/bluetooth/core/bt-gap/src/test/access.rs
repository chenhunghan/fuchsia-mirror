// Copyright 2020 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use assert_matches::assert_matches;
use fidl::endpoints;
use fidl_fuchsia_bluetooth_host::HostRequest;
use fidl_fuchsia_bluetooth_sys::{
    self as sys, AccessMarker, AccessSetConnectionPolicyRequest, ProcedureTokenMarker,
};
use fuchsia_bluetooth::types::pairing_options::{BondableMode, PairingOptions, SecurityLevel};
use fuchsia_bluetooth::types::{HostId, PeerId, Technology};
use fuchsia_sync::RwLock;
use futures::stream::{StreamExt, TryStreamExt};
use futures::{FutureExt, future};
use std::sync::Arc;

use crate::services::access;
use crate::{host_device, host_dispatcher};

#[fuchsia::test]
async fn initiate_pairing() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(42), &dispatcher)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access = access::run(dispatcher, server);

    // The parameters to send to access.Pair()
    let req_id = PeerId(128);
    let req_opts = PairingOptions {
        transport: Technology::LE,
        le_security_level: SecurityLevel::Authenticated,
        bondable: BondableMode::NonBondable,
    };

    let req_opts_ = req_opts.clone();

    let make_request = async move {
        let response = client.pair(&req_id.into(), &req_opts_.into()).await;
        assert_matches!(response, Ok(Ok(())));
        // This terminating will drop the access client, which causes the access stream to
        // terminate. This will cause run_access to terminate which drops the host dispatcher, which
        // closes the host channel and will cause run_host to terminate
        Ok(())
    };

    let run_host = async move {
        host_server.try_for_each(move |req| {
            assert_matches!(&req, HostRequest::Pair { id, options, responder: _ } if *id == req_id.into() && PairingOptions::from(options) == req_opts);
            if let HostRequest::Pair { id: _, options: _, responder } = req {
                assert_matches!(responder.send(Ok(())), Ok(()));
            }
            future::ok(())
        }).await.map_err(|e| e.into())
    };

    future::try_join3(make_request, run_host, run_access)
        .await
        .map(|_: ((), (), ())| ())
        .expect("successful execution")
}

// Test that we can start discovery on a host then migrate that discovery session onto a different
// host when the original host is deactivated
#[fuchsia::test]
async fn discovery_over_adapter_change() {
    // Create mock host dispatcher
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    // Add Host #1 to dispatcher and make active
    let (host_server_1, host_1, _gatt_server_1, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();
    let host_info_1 = Arc::new(RwLock::new(host_1.info()));

    hd.set_active_host(HostId(1)).await.expect("can set active host");

    // Add Host #2 to dispatcher
    let (host_server_2, host_2, _gatt_server_2, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();
    let host_info_2 = Arc::new(RwLock::new(host_2.info()));

    // Create access server future
    let (access_client, access_server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access = access::run(hd.clone(), access_server);

    // Create access client future
    let (discovery_session, discovery_session_server) = endpoints::create_proxy();
    let run_client = async move {
        // Request discovery on active Host #1
        let response = access_client.start_discovery(discovery_session_server).await;
        assert_matches!(response, Ok(Ok(())));

        // Assert that Host #1 is now marked as discovering
        let _ = host_1
            .clone()
            .refresh_test_host_info()
            .await
            .expect("did not receive Host #1 info update");
        let is_discovering = host_1.info().discovering.clone();
        assert!(is_discovering);

        // Deactivate Host #1
        hd.rm_device(host_1.path()).await;

        // Assert that Host #2 is now marked as discovering
        let _ = host_2
            .clone()
            .refresh_test_host_info()
            .await
            .expect("did not receive Host #2 info update");
        let is_discovering = host_2.info().discovering.clone();
        assert!(is_discovering);

        // Drop discovery session, which contains an internal reference to the dispatcher state,
        // so that the other futures may terminate. Then, assert Host #2 stops discovering.
        drop(discovery_session);

        let _ = host_2
            .clone()
            .refresh_test_host_info()
            .await
            .expect("did not receive Host #2 info update");
        let is_discovering = host_2.info().discovering.clone();
        assert!(!is_discovering);

        Ok(())
    };

    future::try_join4(
        run_client,
        run_access,
        host_device::test::run_discovery_host_server(host_server_1, host_info_1),
        host_device::test::run_discovery_host_server(host_server_2, host_info_2),
    )
    .await
    .map(|_: ((), (), (), ())| ())
    .expect("successful execution")
}

#[fuchsia::test]
async fn make_active_host_discoverable() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(49), &dispatcher)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access = access::run(dispatcher, server);

    let make_request = async move {
        let (token, server) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
        let response = client.make_discoverable(server).await;
        assert_matches!(response, Ok(Ok(())));
        // Immediately dropping the token should trigger a SetDiscoverable request to turn it off.
        drop(token);
        Ok(())
    };

    let mut expected_enabled = true;
    let run_host = async move {
        host_server
            .try_for_each(move |req| {
                let HostRequest::SetDiscoverable { enabled, responder } = req else {
                    panic!("Expected HostRequest::SetDiscoverable");
                };
                assert_matches!(responder.send(Ok(())), Ok(()));
                assert_eq!(enabled, expected_enabled);
                // We first expect the Discoverable request with `enabled=true`. Then when the token
                // is dropped, we expect it to be false.
                expected_enabled = !expected_enabled;
                future::ok(())
            })
            .await
            .map_err(|e| e.into())
    };

    future::try_join3(make_request, run_host, run_access)
        .await
        .map(|_: ((), (), ())| ())
        .expect("successful execution");
}

#[fuchsia::test]
async fn toggle_connectable_of_active_host() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(49), &dispatcher)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let _run_access_task = fuchsia_async::Task::spawn(access::run(dispatcher, server));

    let (token, server) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server),
        ..Default::default()
    };
    let connection_policy_fut = client.set_connection_policy(policy);
    // Expect the Host server to receive a request to disable `connectable'.
    match host_server.next().await.expect("valid_request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable, got: {x:?}"),
    }

    // Expect the connection policy request to resolve successfully.
    let result = connection_policy_fut.await;
    assert_matches!(result, Ok(Ok(_)));

    // Immediately dropping the token should result in a request to re-enable connectable.
    drop(token);
    match host_server.next().await.expect("valid_request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable, got: {x:?}"),
    }
}

#[fuchsia::test]
async fn multiple_connection_policy_clients() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(49), &dispatcher)
            .await
            .unwrap();

    let (client1, server1) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task1 = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server1));
    let (client2, server2) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let mut run_access_fut2 = Box::pin(access::run(dispatcher.clone(), server2));
    // Poll once to enqueue watch_peers() registration before host_server.next().await runs the
    // broker task.
    assert!(futures::poll!(&mut run_access_fut2).is_pending());
    let (client3, server3) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task3 = fuchsia_async::Task::spawn(access::run(dispatcher, server3));

    let (token1, server_token1) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy1 = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token1),
        ..Default::default()
    };
    let fut1 = client1.set_connection_policy(policy1);

    // Host receives request to disable connectable; hold responder so state stays Pending.
    let responder = match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => responder,
        x => panic!("Expected SetConnectable, got: {x:?}"),
    };

    // Second client requests suppression while the first request is still Pending.
    let (token2, server_token2) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy2 = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token2),
        ..Default::default()
    };
    let mut fut2 = std::pin::pin!(client2.set_connection_policy(policy2));
    assert!(futures::poll!(&mut fut2).is_pending());
    assert!(futures::poll!(&mut run_access_fut2).is_pending());
    assert!(futures::poll!(&mut fut2).is_pending());
    let run_access_task2 = fuchsia_async::Task::spawn(run_access_fut2);
    assert!(
        host_server.next().now_or_never().is_none(),
        "Unexpected message received on host_server while first suppression request is Pending"
    );

    // Responding on host resolves both Pending clients.
    responder.send(Ok(())).expect("valid response");
    assert_matches!(fut1.await, Ok(Ok(_)));
    assert_matches!(fut2.await, Ok(Ok(_)));

    // Third client requests suppression while already in Suppressed state.
    let (token3, server_token3) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy3 = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token3),
        ..Default::default()
    };
    let fut3 = client3.set_connection_policy(policy3);
    // Third request resolves successfully without issuing another SetConnectable to host.
    assert_matches!(fut3.await, Ok(Ok(_)));
    assert!(
        host_server.next().now_or_never().is_none(),
        "Unexpected message received on host_server after third client requested suppression"
    );

    // Drop first and second clients' tokens and ensure their access tasks finish processing.
    drop(token1);
    drop(client1);
    assert_matches!(run_access_task1.await, Ok(()));
    drop(token2);
    drop(client2);
    assert_matches!(run_access_task2.await, Ok(()));

    // Verify that dropping the first two tokens did NOT re-enable connectable because third client
    // is still active.
    assert!(
        host_server.next().now_or_never().is_none(),
        "Unexpected message received on host_server while third client still holds token"
    );

    // Dropping third token should now cause connectable to be re-enabled.
    drop(token3);
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable, got: {x:?}"),
    }

    drop(client3);
    assert_matches!(run_access_task3.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_over_adapter_change() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    hd.set_active_host(HostId(1)).await.expect("can set active host");

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Host 1 should receive SetConnectable(false)
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 1, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Spawn task to remove Host 1 so Host 2 becomes active
    let host_path_1 = host_1.path().to_string();
    let hd_clone = hd.clone();
    let rm_task = fuchsia_async::Task::spawn(async move {
        hd_clone.rm_device(&host_path_1).await;
    });

    // Host 2 should receive SetConnectable(false) during configure_newly_active_adapter
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    rm_task.await;

    // Verify removed host 1 was NOT re-enabled when its suppression session was migrated.
    assert!(
        host_server_1.next().now_or_never().is_none(),
        "Unexpected message received on host 1 after adapter change"
    );

    // Dropping token should cause Host 2 to receive SetConnectable(true)
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    // Host 1 should still have received no messages.
    assert!(
        host_server_1.next().now_or_never().is_none(),
        "Unexpected message received on host 1 after token drop"
    );

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_over_adapter_removal_and_late_addition() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Host 1 should receive SetConnectable(false)
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 1, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Remove Host 1 so 0 hosts remain.
    let host_path_1 = host_1.path().to_string();
    hd.rm_device(&host_path_1).await;

    // Add Host 2 (0 -> 1 host transition).
    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    // Host 2 should receive SetConnectable(false) during configure_newly_active_adapter
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    // Dropping token should cause Host 2 to receive SetConnectable(true)
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_host_error_does_not_deadlock_and_recovers() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &dispatcher)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server));

    let (_token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Host receives SetConnectable(false) and replies with an error.
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Err(sys::Error::Failed)).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false), got: {x:?}"),
    }

    // The client request must fail without deadlocking the dispatcher.
    assert_matches!(fut.await, Ok(Err(sys::Error::Failed)));

    // A subsequent connection policy request should be able to try again and succeed.
    let (token2, server_token2) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy2 = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token2),
        ..Default::default()
    };
    let fut2 = client.set_connection_policy(policy2);

    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false), got: {x:?}"),
    }
    assert_matches!(fut2.await, Ok(Ok(_)));

    drop(token2);
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true), got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_immediate_re_suppression_maintains_ordering() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &dispatcher)
            .await
            .unwrap();

    let (client1, server1) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task1 = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server1));
    let (client2, server2) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task2 = fuchsia_async::Task::spawn(access::run(dispatcher, server2));

    let (token1, server_token1) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy1 = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token1),
        ..Default::default()
    };
    let fut1 = client1.set_connection_policy(policy1);

    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false), got: {x:?}"),
    }
    assert_matches!(fut1.await, Ok(Ok(_)));

    // Client 1 drops token and finishes processing before Client 2 immediately requests
    // suppression.
    drop(token1);
    drop(client1);
    assert_matches!(run_access_task1.await, Ok(()));

    let (token2, server_token2) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy2 = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token2),
        ..Default::default()
    };
    let fut2 = client2.set_connection_policy(policy2);

    // Host should first receive SetConnectable(true) from the drop, followed by
    // SetConnectable(false) from the new request, in strict FIFO order.
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true), got: {x:?}"),
    }

    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false), got: {x:?}"),
    }
    assert_matches!(fut2.await, Ok(Ok(_)));

    drop(token2);
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true), got: {x:?}"),
    }

    drop(client2);
    assert_matches!(run_access_task2.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_migrates_pending_suppression() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    hd.set_active_host(HostId(1)).await.expect("can set active host");

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Host 1 receives SetConnectable(false), but does NOT respond yet (keeping state Pending).
    let responder_1 = match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => responder,
        x => panic!("Expected SetConnectable on host 1, got: {x:?}"),
    };

    // Remove Host 1 while suppression is pending so Host 2 becomes active.
    let host_path_1 = host_1.path().to_string();
    let hd_clone = hd.clone();
    let rm_task = fuchsia_async::Task::spawn(async move {
        hd_clone.rm_device(&host_path_1).await;
    });

    // Host 2 should receive SetConnectable(false) as the pending task is migrated.
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    rm_task.await;

    // Dropping or replying on the abandoned Host 1 responder should be harmless.
    drop(responder_1);

    // The client's original future should resolve successfully on Host 2!
    assert_matches!(fut.await, Ok(Ok(_)));

    // Dropping the token should cause Host 2 to receive SetConnectable(true).
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_migrates_suppressed_across_multiple_adapter_changes() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    let (mut host_server_2, host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Establish Suppressed state on Host 1.
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 1, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Remove Host 1 so Host 2 becomes active and transitions Suppressed -> Pending.
    let host_path_1 = host_1.path().to_string();
    hd.rm_device(&host_path_1).await;

    // Host 2 receives SetConnectable(false), but does NOT respond yet (keeping state Pending).
    let responder_2 = match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => responder,
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    };

    // Add Host 3 after Host 1 is removed so that removing Host 1 deterministically selected
    // Host 2 (the only remaining adapter in the HashMap) and removing Host 2 next will
    // deterministically select Host 3.
    let (mut host_server_3, _host_3, _gatt_server_3, _bonding_3) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(3), &hd)
            .await
            .unwrap();

    // Remove Host 2 while suppression is still Pending on Host 2 so Host 3 becomes active and
    // transitions Pending -> Pending.
    let host_path_2 = host_2.path().to_string();
    hd.rm_device(&host_path_2).await;

    // Host 3 receives SetConnectable(false) and completes the Pending -> Suppressed transition.
    match host_server_3.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 3, got: {x:?}"),
    }
    drop(responder_2);

    // Allow the suppression task on Host 3 to finish transitioning to Suppressed. Because the
    // original SuppressConnectionsSession was preserved across Pending -> Pending, suppression must
    // remain active while the client still holds `token`.
    fuchsia_async::yield_now().await;
    assert!(
        host_server_3.next().now_or_never().is_none(),
        "Unexpected message received on host 3 while client still holds suppression token"
    );

    // Dropping the client's original token must now re-enable connectability on Host 3.
    drop(token);
    match host_server_3.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 3, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn multiple_discoverable_clients() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(49), &dispatcher)
            .await
            .unwrap();

    let (client1, server1) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task1 = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server1));
    let (client2, server2) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task2 = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server2));
    let (client3, server3) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task3 = fuchsia_async::Task::spawn(access::run(dispatcher, server3));

    let (token1, server_token1) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut1 = client1.make_discoverable(server_token1);

    // Host receives request to enable discoverable; hold responder so state stays Pending.
    let responder = match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => responder,
        x => panic!("Expected SetDiscoverable, got: {x:?}"),
    };

    // Second client requests discoverable while the first request is still Pending.
    let (token2, server_token2) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let mut fut2 = std::pin::pin!(client2.make_discoverable(server_token2));
    for _ in 0..5 {
        fuchsia_async::yield_now().await;
    }
    assert!(futures::poll!(&mut fut2).is_pending());
    assert!(
        host_server.next().now_or_never().is_none(),
        "Unexpected message received on host_server while first discoverable request is Pending"
    );

    // Responding on host resolves both Pending clients.
    responder.send(Ok(())).expect("valid response");
    assert_matches!(fut1.await, Ok(Ok(_)));
    assert_matches!(fut2.await, Ok(Ok(_)));

    // Third client requests discoverable while already in Discoverable state.
    let (token3, server_token3) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut3 = client3.make_discoverable(server_token3);
    // Third request resolves successfully without issuing another SetDiscoverable to host.
    assert_matches!(fut3.await, Ok(Ok(_)));
    assert!(
        host_server.next().now_or_never().is_none(),
        "Unexpected message received on host_server after third client requested discoverable"
    );

    // Drop first and second clients' tokens and ensure their access tasks finish processing.
    drop(token1);
    drop(client1);
    assert_matches!(run_access_task1.await, Ok(()));
    drop(token2);
    drop(client2);
    assert_matches!(run_access_task2.await, Ok(()));

    // Verify that dropping the first two tokens did NOT disable discoverable because third client
    // is still active.
    assert!(
        host_server.next().now_or_never().is_none(),
        "Unexpected message received on host_server while third client still holds token"
    );

    // Dropping third token should now cause discoverable to be disabled.
    drop(token3);
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable, got: {x:?}"),
    }

    drop(client3);
    assert_matches!(run_access_task3.await, Ok(()));
}

#[fuchsia::test]
async fn discoverable_over_adapter_change() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    hd.set_active_host(HostId(1)).await.expect("can set active host");

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut = client.make_discoverable(server_token);

    // Host 1 should receive SetDiscoverable(true)
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 1, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Spawn task to remove Host 1 so Host 2 becomes active
    let host_path_1 = host_1.path().to_string();
    let hd_clone = hd.clone();
    let rm_task = fuchsia_async::Task::spawn(async move {
        hd_clone.rm_device(&host_path_1).await;
    });

    // Host 2 should receive SetDiscoverable(true) during configure_newly_active_adapter
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    rm_task.await;

    // Verify removed host 1 was NOT disabled when its discoverable session was migrated.
    assert!(
        host_server_1.next().now_or_never().is_none(),
        "Unexpected message received on host 1 after adapter change"
    );

    // Dropping token should cause Host 2 to receive SetDiscoverable(false)
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    // Host 1 should still have received no messages.
    assert!(
        host_server_1.next().now_or_never().is_none(),
        "Unexpected message received on host 1 after token drop"
    );

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn discoverable_over_adapter_removal_and_late_addition() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut = client.make_discoverable(server_token);

    // Host 1 should receive SetDiscoverable(true)
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 1, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Remove Host 1 so 0 hosts remain.
    let host_path_1 = host_1.path().to_string();
    hd.rm_device(&host_path_1).await;

    // Add Host 2 (0 -> 1 host transition).
    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    // Host 2 should receive SetDiscoverable(true) during configure_newly_active_adapter
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    // Dropping token should cause Host 2 to receive SetDiscoverable(false)
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn discoverable_host_error_does_not_deadlock_and_recovers() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &dispatcher)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server));

    let (_token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut = client.make_discoverable(server_token);

    // Host receives SetDiscoverable(true) and replies with an error.
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Err(sys::Error::Failed)).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(true), got: {x:?}"),
    }

    // The client request must fail without deadlocking the dispatcher.
    assert_matches!(fut.await, Ok(Err(sys::Error::Failed)));

    // A subsequent make_discoverable request should be able to try again and succeed.
    let (token2, server_token2) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut2 = client.make_discoverable(server_token2);

    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(true), got: {x:?}"),
    }
    assert_matches!(fut2.await, Ok(Ok(_)));

    drop(token2);
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(false), got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn discoverable_immediate_re_discoverable_maintains_ordering() {
    let dispatcher = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server, _, _gatt_server, _bonding) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &dispatcher)
            .await
            .unwrap();

    let (client1, server1) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task1 = fuchsia_async::Task::spawn(access::run(dispatcher.clone(), server1));
    let (client2, server2) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task2 = fuchsia_async::Task::spawn(access::run(dispatcher, server2));

    let (token1, server_token1) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut1 = client1.make_discoverable(server_token1);

    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(true), got: {x:?}"),
    }
    assert_matches!(fut1.await, Ok(Ok(_)));

    // Client 1 drops token and finishes processing before Client 2 immediately requests
    // discoverable.
    drop(token1);
    drop(client1);
    assert_matches!(run_access_task1.await, Ok(()));

    let (token2, server_token2) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut2 = client2.make_discoverable(server_token2);

    // Host should first receive SetDiscoverable(false) from the drop, followed by
    // SetDiscoverable(true) from the new request, in strict FIFO order.
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(false), got: {x:?}"),
    }

    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(true), got: {x:?}"),
    }
    assert_matches!(fut2.await, Ok(Ok(_)));

    drop(token2);
    match host_server.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable(false), got: {x:?}"),
    }

    drop(client2);
    assert_matches!(run_access_task2.await, Ok(()));
}

#[fuchsia::test]
async fn discoverable_migrates_pending_discoverable() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    hd.set_active_host(HostId(1)).await.expect("can set active host");

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut = client.make_discoverable(server_token);

    // Host 1 receives SetDiscoverable(true), but does NOT respond yet (keeping state Pending).
    let responder_1 = match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => responder,
        x => panic!("Expected SetDiscoverable on host 1, got: {x:?}"),
    };

    // Remove Host 1 while discoverable is pending so Host 2 becomes active.
    let host_path_1 = host_1.path().to_string();
    let hd_clone = hd.clone();
    let rm_task = fuchsia_async::Task::spawn(async move {
        hd_clone.rm_device(&host_path_1).await;
    });

    // Host 2 should receive SetDiscoverable(true) as the pending task is migrated.
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    rm_task.await;

    // Dropping or replying on the abandoned Host 1 responder should be harmless.
    drop(responder_1);

    // The client's original future should resolve successfully on Host 2!
    assert_matches!(fut.await, Ok(Ok(_)));

    // Dropping the token should cause Host 2 to receive SetDiscoverable(false).
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_over_set_active_host() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, _host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Host 1 should receive SetConnectable(false)
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 1, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Explicitly designate Host 2 as active via set_active_host
    let mut set_active_fut = std::pin::pin!(hd.set_active_host(HostId(2)));
    assert!(futures::poll!(&mut set_active_fut).is_pending());

    // The previously active host receives Shutdown and has connectable restored to true
    // when a new active host is designated.
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::Shutdown { .. }) => {}
        x => panic!("Expected Shutdown on host 1, got: {x:?}"),
    }
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true) on host 1, got: {x:?}"),
    }
    set_active_fut.await.expect("set_active_host succeeds");

    // Host 2 should receive SetConnectable(false) during configure_newly_active_adapter
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    // Dropping token should cause Host 2 to receive SetConnectable(true)
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable on host 2, got: {x:?}"),
    }

    // Host 1 should receive no further messages after token drop.
    assert!(
        host_server_1.next().now_or_never().is_none(),
        "Unexpected message received on host 1 after token drop"
    );

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn connection_policy_pending_over_set_active_host() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, _host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let mut fut = client.set_connection_policy(policy);
    assert!(futures::poll!(&mut fut).is_pending());

    // Host 1 receives SetConnectable(false), but we hold the responder so suppression stays Pending.
    let _responder_1 = match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => responder,
        x => panic!("Expected SetConnectable(false) on host 1, got: {x:?}"),
    };

    // Switch active host to Host 2 while suppression is still Pending on Host 1.
    let mut set_active_fut = std::pin::pin!(hd.set_active_host(HostId(2)));
    assert!(futures::poll!(&mut set_active_fut).is_pending());

    // Host 1 must receive Shutdown followed by SetConnectable(true) to undo the pipelined
    // SetConnectable(false) request.
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::Shutdown { .. }) => {}
        x => panic!("Expected Shutdown on host 1, got: {x:?}"),
    }
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true) on host 1, got: {x:?}"),
    }
    set_active_fut.await.expect("set_active_host succeeds");

    // Host 2 receives the migrated SetConnectable(false) request and completes the pending session.
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false) on host 2, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Dropping token restores connectability on Host 2.
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true) on host 2, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn discoverable_over_set_active_host() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, _host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let fut = client.make_discoverable(server_token);

    // Host 1 should receive SetDiscoverable(true)
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 1, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Explicitly designate Host 2 as active via set_active_host
    hd.set_active_host(HostId(2)).await.expect("set_active_host succeeds");

    // Host 2 should receive SetDiscoverable(true) during configure_newly_active_adapter
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    // The previously active host receives Shutdown when a new active host is designated.
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::Shutdown { .. }) => {}
        x => panic!("Expected Shutdown on host 1, got: {x:?}"),
    }

    // Dropping token should cause Host 2 to receive SetDiscoverable(false)
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetDiscoverable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetDiscoverable on host 2, got: {x:?}"),
    }

    // Host 1 should receive no further messages after token drop.
    assert!(
        host_server_1.next().now_or_never().is_none(),
        "Unexpected message received on host 1 after token drop"
    );

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn discovery_over_set_active_host() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();
    let host_info_1 = Arc::new(RwLock::new(host_1.info()));

    let (host_server_2, host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();
    let host_info_2 = Arc::new(RwLock::new(host_2.info()));

    let (access_client, access_server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access = access::run(hd.clone(), access_server);

    let (discovery_session, discovery_session_server) = endpoints::create_proxy();
    let run_client = async move {
        // Request discovery on active Host #1
        let response = access_client.start_discovery(discovery_session_server).await;
        assert_matches!(response, Ok(Ok(())));

        // Assert that Host #1 is now marked as discovering
        let _ = host_1
            .clone()
            .refresh_test_host_info()
            .await
            .expect("did not receive Host #1 info update");
        assert!(host_1.info().discovering);

        // Explicitly designate Host #2 as active via set_active_host
        hd.set_active_host(HostId(2)).await.expect("set_active_host succeeds");

        // Assert that Host #1 stopped discovering when shut down
        let _ = host_1
            .clone()
            .refresh_test_host_info()
            .await
            .expect("did not receive Host #1 info update");
        assert!(!host_1.info().discovering);

        // Assert that Host #2 is now marked as discovering
        let _ = host_2
            .clone()
            .refresh_test_host_info()
            .await
            .expect("did not receive Host #2 info update");
        assert!(host_2.info().discovering);

        // Drop discovery session so that the other futures may terminate, then assert Host #2
        // stops discovering.
        drop(discovery_session);

        let _ = host_2
            .clone()
            .refresh_test_host_info()
            .await
            .expect("did not receive Host #2 info update");
        assert!(!host_2.info().discovering);

        Ok(())
    };

    future::try_join4(
        run_client,
        run_access,
        host_device::test::run_discovery_host_server(host_server_1, host_info_1),
        host_device::test::run_discovery_host_server(host_server_2, host_info_2),
    )
    .await
    .map(|_: ((), (), (), ())| ())
    .expect("successful execution")
}

#[fuchsia::test]
async fn suppression_inhibits_config_update_connectable_true_until_token_drop() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    // Start with connectable configuration set to false.
    let initial_config = hd
        .apply_sys_settings(sys::Settings {
            bredr_connectable_mode: Some(false),
            ..Default::default()
        })
        .await;
    assert!(!initial_config.bredr.connectable);

    let (mut host_server_1, _host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    // Add a secondary inactive host (Host 2) to verify its connectability is updated directly
    // even while Host 1 is suppressed.
    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Host 1 receives SetConnectable(false) when suppression session is established
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false), got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Update configuration policy to connectable: true while suppression is active on Host 1.
    let hd_clone = hd.clone();
    let update_task = fuchsia_async::Task::spawn(async move {
        hd_clone
            .apply_sys_settings(sys::Settings {
                bredr_connectable_mode: Some(true),
                ..Default::default()
            })
            .await
    });

    // Inactive Host 2 immediately receives SetConnectable(true).
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true) on inactive host 2, got: {x:?}"),
    }

    let new_config = update_task.await;

    // The baseline configuration was updated to true...
    assert!(new_config.bredr.connectable);

    // ...but SetConnectable(true) was inhibited on active Host 1 because suppression is active.
    assert!(
        host_server_1.next().now_or_never().is_none(),
        "Unexpected message received on host_server_1 while suppressed"
    );

    // Dropping the suppression token should now restore connectability to Host 1.
    drop(token);
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true) upon token drop, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn config_update_connectable_false_persists_after_suppression_token_drop() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (mut host_server_1, _host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Host 1 receives SetConnectable(false) when suppression session is established
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false), got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Update configuration policy to connectable: false while suppression is active.
    let hd_clone = hd.clone();
    let update_task = fuchsia_async::Task::spawn(async move {
        hd_clone
            .apply_sys_settings(sys::Settings {
                bredr_connectable_mode: Some(false),
                ..Default::default()
            })
            .await
    });
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false), got: {x:?}"),
    }
    let new_config = update_task.await;
    assert!(!new_config.bredr.connectable);

    // Add Host 2 while suppression is active and switch active host to Host 2.
    // Host 1 must receive Shutdown and must NOT have connectability re-enabled during migration
    // because config_settings.bredr.connectable is false.
    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    hd.set_active_host(HostId(2)).await.expect("set_active_host succeeds");

    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::Shutdown { .. }) => {}
        x => panic!("Expected Shutdown on host 1, got: {x:?}"),
    }
    assert!(
        host_server_1.next().now_or_never().is_none(),
        "Unexpected message received on host 1 after set_active_host when connectable config is false"
    );

    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false) on host 2 after migration, got: {x:?}"),
    }

    // Dropping the suppression token should NOT restore connectability to Host 2,
    // because the configured policy is connectable: false.
    drop(token);
    drop(client);
    assert_matches!(run_access_task.await, Ok(()));

    // Verify that dropping the token did NOT re-enable connectable because the configured
    // policy is false.
    assert!(
        host_server_2.next().now_or_never().is_none(),
        "Unexpected message received on host 2 after token drop"
    );
}

#[fuchsia::test]
async fn pending_suppression_failure_restores_deferred_connectable_true() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    // Start with connectable configuration set to false.
    let initial_config = hd
        .apply_sys_settings(sys::Settings {
            bredr_connectable_mode: Some(false),
            ..Default::default()
        })
        .await;
    assert!(!initial_config.bredr.connectable);

    let (mut host_server_1, _host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), server));

    let (_token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let fut = client.set_connection_policy(policy);

    // Hold the SetConnectable(false) responder so suppression remains Pending on Host 1.
    let responder_1 = match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => responder,
        x => panic!("Expected SetConnectable(false), got: {x:?}"),
    };

    // Update configuration policy to connectable: true while suppression is Pending on Host 1.
    let new_config = hd
        .apply_sys_settings(sys::Settings {
            bredr_connectable_mode: Some(true),
            ..Default::default()
        })
        .await;
    assert!(new_config.bredr.connectable);
    assert!(
        host_server_1.next().now_or_never().is_none(),
        "Unexpected SetConnectable(true) on host 1 while suppression is Pending"
    );

    // Fail the pending SetConnectable(false) request; deferred connectable: true must now be
    // restored on Host 1.
    responder_1.send(Err(sys::Error::Failed)).expect("valid response");
    assert_matches!(fut.await, Ok(Err(sys::Error::Failed)));

    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true) after Pending suppression failure, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn pending_suppression_migration_restores_deferred_connectable_true() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    // Start with connectable configuration set to false.
    let initial_config = hd
        .apply_sys_settings(sys::Settings {
            bredr_connectable_mode: Some(false),
            ..Default::default()
        })
        .await;
    assert!(!initial_config.bredr.connectable);

    let (client, server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let mut run_access_fut = Box::pin(access::run(hd.clone(), server));
    // Poll once to enqueue watch_peers() registration before host creation runs the broker task.
    assert!(futures::poll!(&mut run_access_fut).is_pending());

    let (mut host_server_1, _host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (token, server_token) = fidl::endpoints::create_sync_proxy::<ProcedureTokenMarker>();
    let policy = AccessSetConnectionPolicyRequest {
        suppress_bredr_connections: Some(server_token),
        ..Default::default()
    };
    let mut fut = std::pin::pin!(client.set_connection_policy(policy));
    assert!(futures::poll!(&mut fut).is_pending());
    // Drive access::run once so suppress_connections() enters Pending { host_id: None, .. }
    // before _start_suppression_task runs.
    assert!(futures::poll!(&mut run_access_fut).is_pending());
    let run_access_task = fuchsia_async::Task::spawn(run_access_fut);

    // Update configuration policy to connectable: true while suppression is Pending on Host 1
    // (before _start_suppression_task has run).
    let mut update_fut = std::pin::pin!(hd.apply_sys_settings(sys::Settings {
        bredr_connectable_mode: Some(true),
        ..Default::default()
    }));
    assert!(futures::poll!(&mut update_fut).is_pending());

    // Inactive Host 2 immediately receives SetConnectable(true).
    match host_server_2.next().now_or_never().expect("request ready").expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true) on inactive host 2, got: {x:?}"),
    }
    assert!(
        host_server_1.next().now_or_never().is_none(),
        "Unexpected message received on host 1 while suppression is Pending"
    );

    // Migrate active adapter to Host 2 while suppression is still Pending on Host 1
    // (before _start_suppression_task has run). Host 1 must receive Shutdown and have its
    // inhibited SetConnectable(true) restored.
    let mut set_active_fut = std::pin::pin!(hd.set_active_host(HostId(2)));
    assert!(futures::poll!(&mut set_active_fut).is_pending());

    let new_config = update_fut.await;
    assert!(new_config.bredr.connectable);

    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::Shutdown { .. }) => {}
        x => panic!("Expected Shutdown on host 1, got: {x:?}"),
    }
    match host_server_1.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true) on host 1 after migration, got: {x:?}"),
    }
    set_active_fut.await.expect("set_active_host succeeds");

    // Host 2 receives SetConnectable(false) for the migrated Pending suppression session.
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: false, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(false) on host 2 after migration, got: {x:?}"),
    }
    assert_matches!(fut.await, Ok(Ok(_)));

    // Dropping the token restores connectability on Host 2.
    drop(token);
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::SetConnectable { enabled: true, responder }) => {
            responder.send(Ok(())).expect("valid response");
        }
        x => panic!("Expected SetConnectable(true) on host 2 after token drop, got: {x:?}"),
    }

    drop(client);
    assert_matches!(run_access_task.await, Ok(()));
}

#[fuchsia::test]
async fn discovery_migrates_pending_across_multiple_adapter_changes() {
    let hd = host_dispatcher::test::make_simple_test_dispatcher();

    let (host_server_1, host_1, _gatt_server_1, _bonding_1) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(1), &hd)
            .await
            .unwrap();
    let host_info_1 = Arc::new(RwLock::new(host_1.info()));

    let (mut host_server_2, _host_2, _gatt_server_2, _bonding_2) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(2), &hd)
            .await
            .unwrap();

    let (host_server_3, host_3, _gatt_server_3, _bonding_3) =
        host_dispatcher::test::create_and_add_test_host_to_dispatcher(HostId(3), &hd)
            .await
            .unwrap();
    let host_info_3 = Arc::new(RwLock::new(host_3.info()));

    let _host_1_task = fuchsia_async::Task::spawn(host_device::test::run_discovery_host_server(
        host_server_1,
        host_info_1,
    ));
    let _host_3_task = fuchsia_async::Task::spawn(host_device::test::run_discovery_host_server(
        host_server_3,
        host_info_3.clone(),
    ));

    let (access_client, access_server) = endpoints::create_proxy_and_stream::<AccessMarker>();
    let run_access_task = fuchsia_async::Task::spawn(access::run(hd.clone(), access_server));

    let (discovery_session, discovery_session_server) = endpoints::create_proxy();
    let response = access_client.start_discovery(discovery_session_server).await;
    assert_matches!(response, Ok(Ok(())));

    let _ =
        host_1.clone().refresh_test_host_info().await.expect("did not receive Host #1 info update");
    assert!(host_1.info().discovering);

    // Poll set_active_host(HostId(2)) once so it transitions DiscoveryState::Discovering to
    // DiscoveryState::Pending and suspends at session_receiver.await before start_discovery_task
    // runs.
    let mut set_active_2_fut = std::pin::pin!(hd.set_active_host(HostId(2)));
    assert!(futures::poll!(&mut set_active_2_fut).is_pending());

    // Migrate again from Host #2 (Pending) to Host #3 while migration to Host #2 is still Pending.
    hd.set_active_host(HostId(3)).await.expect("set_active_host(3) succeeds");
    set_active_2_fut.await.expect("set_active_host(2) completes");

    // Host #2 must only receive Shutdown (its pending start_discovery_task was cancelled before
    // sending StartDiscovery).
    match host_server_2.next().await.expect("valid request") {
        Ok(HostRequest::Shutdown { .. }) => {}
        x => panic!("Expected Shutdown on host 2, got: {x:?}"),
    }
    assert!(
        host_server_2.next().now_or_never().is_none(),
        "Unexpected message received on host 2 after Pending discovery migration"
    );

    // Host #3 must start discovering and remain discovering while the client holds
    // discovery_session (verifying that the Pending -> Pending migration did not drop the session).
    let _ =
        host_3.clone().refresh_test_host_info().await.expect("did not receive Host #3 info update");
    assert!(host_3.info().discovering);
    assert!(host_info_3.read().discovering);

    // Dropping the client's discovery_session token must now stop discovery on Host #3.
    drop(discovery_session);
    let _ =
        host_3.clone().refresh_test_host_info().await.expect("did not receive Host #3 info update");
    assert!(!host_3.info().discovering);
    assert!(!host_info_3.read().discovering);

    drop(access_client);
    assert_matches!(run_access_task.await, Ok(()));
}
