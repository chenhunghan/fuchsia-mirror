// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use diagnostics_data::{Data, Logs};
use example_tester::{
    Client, Proxy, Server, TestKind, assert_filtered_logs_eq_to_golden, logs_to_str, run_test,
};
use fidl::prelude::*;
use fuchsia_component_test::{Capability, ChildRef, RealmBuilder, Ref, Route};

async fn set_client_config(
    builder: &RealmBuilder,
    client: &ChildRef,
    do_in_process: bool,
    augend: u8,
    addend: u8,
) -> Result<(), Error> {
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.exampletester.DoInProcess".parse()?,
            value: do_in_process.into(),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.exampletester.Augend".parse()?,
            value: augend.into(),
        }))
        .await?;
    builder
        .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
            name: "fuchsia.exampletester.Addend".parse()?,
            value: addend.into(),
        }))
        .await?;
    let config_capabilities = [
        "fuchsia.exampletester.DoInProcess",
        "fuchsia.exampletester.Augend",
        "fuchsia.exampletester.Addend",
    ];
    builder
        .add_route(
            config_capabilities
                .into_iter()
                .fold(Route::new(), |route, name| route.capability(Capability::configuration(name)))
                .from(Ref::self_())
                .to(client),
        )
        .await?;
    Ok(())
}

// Tests the framework for a single component running locally. This is useful for testing things
// like local logging and persistent FIDL.
#[fuchsia::test]
async fn test_one_component_log_by_log() -> Result<(), Error> {
    let client = Client::new("test_one_component", "#meta/example_tester_example_client.cm");

    run_test(
        fidl_test_exampletester::SimpleMarker::PROTOCOL_NAME,
        TestKind::StandaloneComponent { client: &client },
        |builder: RealmBuilder, client: ChildRef| async move {
            set_client_config(&builder, &client, true, 1, 3).await?;
            Ok::<(RealmBuilder, ChildRef), Error>((builder, client))
        },
        |log_reader| {
            let client_clone = client.clone();
            async move {
                let raw_logs = log_reader.snapshot().await.expect("snapshot succeeds");
                let all_logs = logs_to_str(&raw_logs, None);
                assert_eq!(all_logs.filter(|log| *log == "Started").count(), 1);

                let client_logs = logs_to_str(&raw_logs, Some(vec![&client_clone]));
                assert_eq!(client_logs.last().expect("no response"), format!("Response: {}", 4));
            }
        },
    )
    .await
}

// Same as above, but does the test using a log golden, rather than asserting against specific logs.
#[fuchsia::test]
async fn test_one_component() -> Result<(), Error> {
    let client = Client::new("test_one_component", "#meta/example_tester_example_client.cm");
    let filter = |raw_log: &&Data<Logs>| {
        let msg = raw_log.payload_message().expect("payload not found").properties[0]
            .string()
            .expect("message is not string");
        if msg.contains("trim me") {
            return false;
        }
        true
    };

    run_test(
        fidl_test_exampletester::SimpleMarker::PROTOCOL_NAME,
        TestKind::StandaloneComponent { client: &client },
        |builder: RealmBuilder, client: ChildRef| async move {
            set_client_config(&builder, &client, true, 10, 30).await?;
            Ok::<(RealmBuilder, ChildRef), Error>((builder, client))
        },
        |log_reader| {
            let client_clone = client.clone();
            async move {
                assert_filtered_logs_eq_to_golden(&log_reader, &client_clone, filter).await;
            }
        },
    )
    .await
}

// Tests the standard FIDL IPC scenario, with a client talking to a server.
#[fuchsia::test]
async fn test_two_component_log_by_log() -> Result<(), Error> {
    let augend = 1u8;
    let addend = 2u8;
    let want_response = 3;
    let test_name = "test_two_component";
    let client = Client::new(test_name, "#meta/example_tester_example_client.cm");
    let server = Server::new(test_name, "#meta/example_tester_example_server.cm");

    run_test(
        fidl_test_exampletester::SimpleMarker::PROTOCOL_NAME,
        TestKind::ClientAndServer { client: &client, server: &server },
        |builder: RealmBuilder, client: ChildRef| async move {
            set_client_config(&builder, &client, false, augend, addend).await?;
            Ok::<(RealmBuilder, ChildRef), Error>((builder, client))
        },
        |log_reader| {
            let server_clone = server.clone();
            let client_clone = client.clone();
            async move {
                let raw_logs = log_reader.snapshot().await.expect("snapshot succeeds");
                let all_logs = logs_to_str(&raw_logs, None);
                assert_eq!(all_logs.filter(|log| *log == "Started").count(), 2);

                let non_client_logs = logs_to_str(&raw_logs, Some(vec![&server_clone]));
                assert_eq!(
                    non_client_logs
                        .filter(|log| *log == "Request received" || *log == "Response sent")
                        .count(),
                    2
                );

                let client_logs = logs_to_str(&raw_logs, Some(vec![&client_clone]));
                assert_eq!(
                    client_logs.last().expect("no response"),
                    format!("Response: {}", want_response)
                );
            }
        },
    )
    .await
}

// Same as above, but does the test using log goldens, rather than asserting against specific logs.
#[fuchsia::test]
async fn test_two_component() -> Result<(), Error> {
    let test_name = "test_two_component";
    let client = Client::new(test_name, "#meta/example_tester_example_client.cm");
    let server = Server::new(test_name, "#meta/example_tester_example_server.cm");
    let filter = |raw_log: &&Data<Logs>| {
        let msg = raw_log.payload_message().expect("payload not found").properties[0]
            .string()
            .expect("message is not string");
        if msg.contains("trim me") {
            return false;
        }
        true
    };

    run_test(
        fidl_test_exampletester::SimpleMarker::PROTOCOL_NAME,
        TestKind::ClientAndServer { client: &client, server: &server },
        |builder: RealmBuilder, client: ChildRef| async move {
            set_client_config(&builder, &client, false, 10, 20).await?;
            Ok::<(RealmBuilder, ChildRef), Error>((builder, client))
        },
        |log_reader| {
            let client_clone = client.clone();
            let server_clone = server.clone();
            async move {
                assert_filtered_logs_eq_to_golden(&log_reader, &client_clone, filter).await;
                assert_filtered_logs_eq_to_golden(&log_reader, &server_clone, filter).await;
            }
        },
    )
    .await
}

// Tests a client-server IPC interaction mediated by a proxy in the middle.
#[fuchsia::test]
async fn test_three_component_log_by_log() -> Result<(), Error> {
    let augend = 4u8;
    let addend = 5u8;
    let want_response = 9;
    let test_name = "test_three_component";
    let client = Client::new(test_name, "#meta/example_tester_example_client.cm");
    let proxy = Proxy::new(test_name, "#meta/example_tester_example_proxy.cm");
    let server = Server::new(test_name, "#meta/example_tester_example_server.cm");

    run_test(
        fidl_test_exampletester::SimpleMarker::PROTOCOL_NAME,
        TestKind::ClientProxyAndServer { client: &client, proxy: &proxy, server: &server },
        |builder: RealmBuilder, client: ChildRef| async move {
            set_client_config(&builder, &client, false, augend, addend).await?;
            Ok::<(RealmBuilder, ChildRef), Error>((builder, client))
        },
        |log_reader| {
            let proxy_fut = proxy.clone();
            let client_clone = client.clone();
            let server_clone = server.clone();
            async move {
                let raw_logs = log_reader.snapshot().await.expect("snapshot succeeds");
                let all_logs = logs_to_str(&raw_logs, None);
                assert_eq!(all_logs.filter(|log| *log == "Started").count(), 3);

                let non_client_logs = logs_to_str(&raw_logs, Some(vec![&proxy_fut, &server_clone]));
                assert_eq!(
                    non_client_logs
                        .filter(|log| *log == "Request received" || *log == "Response sent")
                        .count(),
                    4
                );

                let client_logs = logs_to_str(&raw_logs, Some(vec![&client_clone]));
                assert_eq!(
                    client_logs.last().expect("no response"),
                    format!("Response: {}", want_response)
                );
            }
        },
    )
    .await
}

// Same as above, but does the test using log goldens, rather than asserting against specific logs.
#[fuchsia::test]
async fn test_three_component() -> Result<(), Error> {
    let test_name = "test_three_component";
    let client = Client::new(test_name, "#meta/example_tester_example_client.cm");
    let proxy = Proxy::new(test_name, "#meta/example_tester_example_proxy.cm");
    let server = Server::new(test_name, "#meta/example_tester_example_server.cm");
    let filter = |raw_log: &&Data<Logs>| {
        let msg = raw_log.payload_message().expect("payload not found").properties[0]
            .string()
            .expect("message is not string");
        if msg.contains("trim me") {
            return false;
        }
        true
    };

    run_test(
        fidl_test_exampletester::SimpleMarker::PROTOCOL_NAME,
        TestKind::ClientProxyAndServer { client: &client, proxy: &proxy, server: &server },
        |builder: RealmBuilder, client: ChildRef| async move {
            set_client_config(&builder, &client, false, 40, 50).await?;
            Ok::<(RealmBuilder, ChildRef), Error>((builder, client))
        },
        |log_reader| {
            let client_clone = client.clone();
            let server_clone = server.clone();
            let proxy_fut = proxy.clone();
            async move {
                assert_filtered_logs_eq_to_golden(&log_reader, &client_clone, filter).await;
                assert_filtered_logs_eq_to_golden(&log_reader, &proxy_fut, filter).await;
                assert_filtered_logs_eq_to_golden(&log_reader, &server_clone, filter).await;
            }
        },
    )
    .await
}
