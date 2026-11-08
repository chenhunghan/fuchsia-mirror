// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use example_tester::{Client, Server, TestKind, assert_logs_eq_to_golden, run_test};
use fidl::prelude::*;
use fidl_examples_keyvaluestore_supporttrees::StoreMarker;

use fuchsia_component_test::{Capability, ChildRef, RealmBuilder, Ref, Route};

#[fuchsia::test]
async fn test_write_success() -> Result<(), Error> {
    let test_name = "test_write_success";
    let client = Client::new(test_name, "#meta/keyvaluestore_supporttrees_client.cm");
    let server = Server::new(test_name, "#meta/keyvaluestore_supporttrees_server.cm");

    run_test(
        StoreMarker::PROTOCOL_NAME,
        TestKind::ClientAndServer { client: &client, server: &server },
        |builder: RealmBuilder, client: ChildRef| async move {
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.supporttrees.WriteItems".parse()?,
                    value: vec!["verse_1", "verse_2"].into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.supporttrees.WriteNested".parse()?,
                    value: vec!["rest_of_poem\nverse_3\nverse_4"].into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.supporttrees.WriteNull".parse()?,
                    value: vec!["null_verse"].into(),
                }))
                .await?;
            let config_capabilities = [
                "fuchsia.examples.keyvaluestore.supporttrees.WriteItems",
                "fuchsia.examples.keyvaluestore.supporttrees.WriteNested",
                "fuchsia.examples.keyvaluestore.supporttrees.WriteNull",
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
async fn test_empty_nested() -> Result<(), Error> {
    let test_name = "test_empty_nested";
    let client = Client::new(test_name, "#meta/keyvaluestore_supporttrees_client.cm");
    let server = Server::new(test_name, "#meta/keyvaluestore_supporttrees_server.cm");

    run_test(
        StoreMarker::PROTOCOL_NAME,
        TestKind::ClientAndServer { client: &client, server: &server },
        |builder: RealmBuilder, client: ChildRef| async move {
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.supporttrees.WriteItems".parse()?,
                    value: Vec::<&str>::new().into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.supporttrees.WriteNested".parse()?,
                    value: vec!["invalid_empty"].into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.supporttrees.WriteNull".parse()?,
                    value: Vec::<&str>::new().into(),
                }))
                .await?;
            let config_capabilities = [
                "fuchsia.examples.keyvaluestore.supporttrees.WriteItems",
                "fuchsia.examples.keyvaluestore.supporttrees.WriteNested",
                "fuchsia.examples.keyvaluestore.supporttrees.WriteNull",
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
