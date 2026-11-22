// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Error;
use example_tester::{Client, Server, TestKind, assert_logs_eq_to_golden, run_test};
use fidl::prelude::*;
use fidl_examples_canvas_addlinemetered::InstanceMarker;

use fuchsia_component_test::{Capability, ChildRef, RealmBuilder, Ref, Route};

#[fuchsia::test]
async fn test_draw_success() -> Result<(), Error> {
    let test_name = "test_draw_success";
    let client = Client::new(test_name, "#meta/canvas_addlinemetered_client.cm");
    let server = Server::new(test_name, "#meta/canvas_addlinemetered_server.cm");

    run_test(
        InstanceMarker::PROTOCOL_NAME,
        TestKind::ClientAndServer { client: &client, server: &server },
        |builder: RealmBuilder, client: ChildRef| async move {
            builder
                .add_capability(cm_rust::CapabilityDecl::Config(cm_rust::ConfigurationDecl {
                    name: "fuchsia.examples.canvas.addlinemetered.Script".parse()?,
                    value: vec![
                        "-5,0:0,0",
                        "-2,2:5,6",
                        "4,-3:-2,-2",
                        "WAIT",
                        "6,-1:7,0",
                        "7,-8:-4,3",
                        "WAIT",
                    ]
                    .into(),
                }))
                .await?;
            builder
                .add_route(
                    Route::new()
                        .capability(Capability::configuration(
                            "fuchsia.examples.canvas.addlinemetered.Script",
                        ))
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
