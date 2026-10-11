// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

mod config;
mod hwinfo_server;

use anyhow::Error;
use config::{BoardInfo, DeviceInfo, ProductInfo};
use fidl_fuchsia_factory::MiscFactoryStoreProviderMarker;
use fidl_fuchsia_hwinfo::{BoardRequestStream, DeviceRequestStream, ProductRequestStream};
use fuchsia_async as fasync;
use fuchsia_component::client::connect_to_protocol;
use fuchsia_component::escrow::EscrowOperation;
use fuchsia_component::server::{Item, ServiceFs};
use fuchsia_sync::RwLock;
use futures::prelude::*;
use hwinfo_server::{BoardInfoServer, DeviceInfoServer, ProductInfoServer};
use std::sync::Arc;

const IDLE_TIMEOUT: fasync::MonotonicDuration = fasync::MonotonicDuration::from_seconds(5);

enum IncomingServices {
    ProductInfo(ProductRequestStream),
    DeviceInfo(DeviceRequestStream),
    BoardInfo(BoardRequestStream),
}

#[fuchsia::main(logging_tags = ["hwinfo"])]
async fn main() -> Result<(), Error> {
    log::info!("Initiating Hwinfo Server...");
    let escrow_operation = EscrowOperation::new();
    escrow_operation.watch_for_stop().expect("Failed to watch for stop on lifecycle channel");

    let proxy = connect_to_protocol::<MiscFactoryStoreProviderMarker>()
        .expect("Failed to connect to MiscFactoryStoreProvider service");
    // Loading Device Info
    let device_info = DeviceInfo::load(&proxy).await;
    let locked_device_info = Arc::new(RwLock::new(device_info));
    // Loading Product Info
    let product_info = ProductInfo::load(&proxy).await;
    let locked_product_info = Arc::new(RwLock::new(product_info));
    // Loading Board Info
    let board_info = BoardInfo::load();
    let locked_board_info = Arc::new(RwLock::new(board_info));
    let mut fs = ServiceFs::new();
    fs.dir("svc")
        .add_fidl_service(IncomingServices::ProductInfo)
        .add_fidl_service(IncomingServices::DeviceInfo)
        .add_fidl_service(IncomingServices::BoardInfo);
    fs.take_and_serve_directory_handle()?;
    const CONCURRENT_LIMIT: usize = 100;
    fs.until_stalled(IDLE_TIMEOUT)
        .for_each_concurrent(CONCURRENT_LIMIT, move |item| {
            let device_info_clone = Arc::clone(&locked_device_info);
            let product_info_clone = Arc::clone(&locked_product_info);
            let board_info_clone = Arc::clone(&locked_board_info);
            let escrow_operation = escrow_operation.clone();
            async move {
                match item {
                    Item::Request(incoming_service, _active_guard) => match incoming_service {
                        IncomingServices::ProductInfo(stream) => {
                            let server = ProductInfoServer::new(Arc::clone(&product_info_clone));
                            server
                                .handle_requests_from_stream(stream, IDLE_TIMEOUT)
                                .await
                                .unwrap_or_else(|e| {
                                    log::error!("Failed to run product_info service: {:?}", e)
                                });
                        }
                        IncomingServices::DeviceInfo(stream) => {
                            let server = DeviceInfoServer::new(Arc::clone(&device_info_clone));
                            server
                                .handle_requests_from_stream(stream, IDLE_TIMEOUT)
                                .await
                                .unwrap_or_else(|e| {
                                    log::error!("Failed to run device_info service: {:?}", e)
                                });
                        }
                        IncomingServices::BoardInfo(stream) => {
                            let server = BoardInfoServer::new(Arc::clone(&board_info_clone));
                            server
                                .handle_requests_from_stream(stream, IDLE_TIMEOUT)
                                .await
                                .unwrap_or_else(|e| {
                                    log::error!("Failed to run board_info service: {:?}", e)
                                });
                        }
                    },
                    Item::Stalled(outgoing_directory) => {
                        escrow_operation
                            .run(outgoing_directory.into())
                            .expect("Failed to run escrow operation");
                    }
                }
            }
        })
        .await;
    Ok(())
}
