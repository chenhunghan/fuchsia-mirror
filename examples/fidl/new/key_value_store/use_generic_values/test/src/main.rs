// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use example_tester::{Client, Server, TestKind, assert_logs_eq_to_golden, run_test};
use fidl::prelude::*;
use fidl_examples_keyvaluestore_usegenericvalues::StoreMarker;

use fuchsia_component_test::{Capability, ChildRef, RealmBuilder, Ref, Route};

const CONFIG_CAPABILITIES: [&str; 7] = [
    "fuchsia.examples.keyvaluestore.usegenericvalues.SetOverwriteOption",
    "fuchsia.examples.keyvaluestore.usegenericvalues.SetConcatOption",
    "fuchsia.examples.keyvaluestore.usegenericvalues.WriteBytes",
    "fuchsia.examples.keyvaluestore.usegenericvalues.WriteStrings",
    "fuchsia.examples.keyvaluestore.usegenericvalues.WriteUint64s",
    "fuchsia.examples.keyvaluestore.usegenericvalues.WriteInt64s",
    "fuchsia.examples.keyvaluestore.usegenericvalues.WriteUint128s",
];

async fn test_write_items_twice(
    test_name: &str,
    set_overwrite: bool,
    set_concat: bool,
) -> Result<(), Error> {
    let client = Client::new(test_name, "#meta/keyvaluestore_usegenericvalues_client.cm");
    let server = Server::new(test_name, "#meta/keyvaluestore_usegenericvalues_server.cm");

    run_test(
        StoreMarker::PROTOCOL_NAME,
        TestKind::ClientAndServer { client: &client, server: &server },
        |builder: RealmBuilder, client: ChildRef| async move {
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.SetOverwriteOption"
                        .parse()?,
                    value: set_overwrite.into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.SetConcatOption"
                        .parse()?,
                    value: set_concat.into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.WriteBytes".parse()?,
                    value: vec!["foo", "bar"].into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.WriteStrings".parse()?,
                    value: vec!["baz", "qux"].into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.WriteUint64s".parse()?,
                    value: vec![1u64, 101u64].into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.WriteInt64s".parse()?,
                    value: vec![-2 as i64, -102 as i64].into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.WriteUint128s"
                        .parse()?,
                    value: vec![3u64, 103u64].into(),
                }))
                .await?;
            builder
                .add_route(
                    CONFIG_CAPABILITIES
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
async fn test_write_items_overwrite_success() -> Result<(), Error> {
    test_write_items_twice("test_write_items_overwrite_success", true, false).await
}

#[fuchsia::test]
async fn test_write_items_concat_success() -> Result<(), Error> {
    test_write_items_twice("test_write_items_concat_success", false, true).await
}

#[fuchsia::test]
async fn test_write_items_default_error() -> Result<(), Error> {
    test_write_items_twice("test_write_items_default_error", false, false).await
}

#[fuchsia::test]
async fn test_write_items_default_success() -> Result<(), Error> {
    let test_name = "test_write_items_default_success";
    let client = Client::new(test_name, "#meta/keyvaluestore_usegenericvalues_client.cm");
    let server = Server::new(test_name, "#meta/keyvaluestore_usegenericvalues_server.cm");

    run_test(
        StoreMarker::PROTOCOL_NAME,
        TestKind::ClientAndServer { client: &client, server: &server },
        |builder: RealmBuilder, client: ChildRef| async move {
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.SetOverwriteOption"
                        .parse()?,
                    value: false.into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.SetConcatOption"
                        .parse()?,
                    value: false.into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.WriteBytes".parse()?,
                    value: vec!["foo"].into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.WriteStrings".parse()?,
                    value: vec!["baz"].into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.WriteUint64s".parse()?,
                    value: vec![1u64].into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.WriteInt64s".parse()?,
                    value: vec![-2 as i64].into(),
                }))
                .await?;
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.keyvaluestore.usegenericvalues.WriteUint128s"
                        .parse()?,
                    value: vec![3u64].into(),
                }))
                .await?;
            builder
                .add_route(
                    CONFIG_CAPABILITIES
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
