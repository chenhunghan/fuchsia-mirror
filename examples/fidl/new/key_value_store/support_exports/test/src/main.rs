// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use example_tester::{Client, Server, TestKind, assert_logs_eq_to_golden, run_test};
use fidl::prelude::*;
use fidl_examples_keyvaluestore_supportexports::StoreMarker;

use fuchsia_component_test::{Capability, ChildRef, RealmBuilder, Ref, Route};

async fn test_export(
    test_name: &str,
    write_items: impl IntoIterator<Item = impl ToString>,
    max_export_size: u64,
) -> Result<(), Error> {
    let client = Client::new(test_name, "#meta/keyvaluestore_supportexports_client.cm");
    let server = Server::new(test_name, "#meta/keyvaluestore_supportexports_server.cm");
    let write_items: Vec<_> = write_items.into_iter().map(|s| s.to_string()).collect();

    run_test(
        StoreMarker::PROTOCOL_NAME,
        TestKind::ClientAndServer { client: &client, server: &server },
        |builder: RealmBuilder, client: ChildRef| async move {
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.supportexports.WriteItems".parse()?,
                    value: write_items.into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.supportexports.MaxExportSize".parse()?,
                    value: max_export_size.into(),
                }))
                .await?;
            let config_capabilities = [
                "fuchsia.examples.keyvaluestore.supportexports.WriteItems",
                "fuchsia.examples.keyvaluestore.supportexports.MaxExportSize",
            ];
            builder
                .add_route(
                    config_capabilities
                        .into_iter()
                        .fold(Route::new(), |route, name| {
                            route.capability(Capability::configuration(name))
                        })
                        .from(Ref::self_())
                        .to(&client),
                )
                .await?;
            Ok::<(RealmBuilder, ChildRef), Error>((builder, client))
        },
        |log_reader| {
            let client_clone = client.clone();
            let server_clone = server.clone();
            async move {
                assert_logs_eq_to_golden(&log_reader, &client_clone).await;
                assert_logs_eq_to_golden(&log_reader, &server_clone).await;
            }
        },
    )
    .await
}

#[fuchsia::test]
async fn test_export_success() -> Result<(), Error> {
    test_export("test_export_success", vec!["verse_1", "verse_2", "verse_3", "verse_4"], 10_000)
        .await
}

#[fuchsia::test]
async fn test_export_empty_error() -> Result<(), Error> {
    test_export("test_export_empty_error", Vec::<&str>::new(), 10_000).await
}

#[fuchsia::test]
async fn test_export_storage_too_small_error() -> Result<(), Error> {
    test_export(
        "test_export_storage_too_small_error",
        vec!["verse_1", "verse_2", "verse_3", "verse_4"],
        50,
    )
    .await
}
