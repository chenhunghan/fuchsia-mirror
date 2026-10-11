// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::config::{BoardInfo, DeviceInfo, ProductInfo};
use anyhow::{Context as _, Error};
use fidl_fuchsia_hwinfo::{
    BoardMarker, BoardRequest, BoardRequestStream, DeviceMarker, DeviceRequest,
    DeviceRequestStream, ProductMarker, ProductRequest, ProductRequestStream,
};
use fuchsia_async as fasync;
use fuchsia_component::client::connect_channel_to_protocol_at;
use fuchsia_sync::RwLock;
use futures::prelude::*;
use std::sync::Arc;

type DeviceInfoTable = Arc<RwLock<DeviceInfo>>;

type BoardInfoTable = Arc<RwLock<BoardInfo>>;

type ProductInfoTable = Arc<RwLock<ProductInfo>>;

pub struct DeviceInfoServer {
    device_info_table: DeviceInfoTable,
}

impl DeviceInfoServer {
    pub fn new(device_info_table: DeviceInfoTable) -> Self {
        Self { device_info_table }
    }

    pub async fn handle_requests_from_stream(
        &self,
        stream: DeviceRequestStream,
        idle_timeout: fasync::MonotonicDuration,
    ) -> Result<(), Error> {
        let (stream, unbind_if_stalled) = detect_stall::until_stalled(stream, idle_timeout);
        let mut stream = std::pin::pin!(stream);
        while let Some(req) = stream.try_next().await? {
            self.handle_request(req).await?;
        }
        if let Ok(Some(server_end)) = unbind_if_stalled.await {
            connect_channel_to_protocol_at::<DeviceMarker>(server_end, "/escrow")
                .context("Failed to escrow fuchsia.hwinfo.Device")?;
        }
        Ok(())
    }

    async fn handle_request(&self, request: DeviceRequest) -> Result<(), Error> {
        match request {
            DeviceRequest::GetInfo { responder } => {
                responder
                    .send(&self.device_info_table.read().clone().into())
                    .context("error sending response")?;
            }
        };
        Ok(())
    }
}

pub struct BoardInfoServer {
    board_info_table: BoardInfoTable,
}

impl BoardInfoServer {
    pub fn new(board_info_table: BoardInfoTable) -> Self {
        Self { board_info_table }
    }

    pub async fn handle_requests_from_stream(
        &self,
        stream: BoardRequestStream,
        idle_timeout: fasync::MonotonicDuration,
    ) -> Result<(), Error> {
        let (stream, unbind_if_stalled) = detect_stall::until_stalled(stream, idle_timeout);
        let mut stream = std::pin::pin!(stream);
        while let Some(req) = stream.try_next().await? {
            self.handle_request(req).await?;
        }
        if let Ok(Some(server_end)) = unbind_if_stalled.await {
            connect_channel_to_protocol_at::<BoardMarker>(server_end, "/escrow")
                .context("Failed to escrow fuchsia.hwinfo.Board")?;
        }
        Ok(())
    }

    async fn handle_request(&self, request: BoardRequest) -> Result<(), Error> {
        match request {
            BoardRequest::GetInfo { responder } => {
                responder
                    .send(&self.board_info_table.read().clone().into())
                    .context("error sending response")?;
            }
        };
        Ok(())
    }
}

pub struct ProductInfoServer {
    product_info_table: ProductInfoTable,
}

impl ProductInfoServer {
    pub fn new(product_info_table: ProductInfoTable) -> Self {
        Self { product_info_table }
    }

    pub async fn handle_requests_from_stream(
        &self,
        stream: ProductRequestStream,
        idle_timeout: fasync::MonotonicDuration,
    ) -> Result<(), Error> {
        let (stream, unbind_if_stalled) = detect_stall::until_stalled(stream, idle_timeout);
        let mut stream = std::pin::pin!(stream);
        while let Some(req) = stream.try_next().await? {
            self.handle_request(req).await?;
        }
        if let Ok(Some(server_end)) = unbind_if_stalled.await {
            connect_channel_to_protocol_at::<ProductMarker>(server_end, "/escrow")
                .context("Failed to escrow fuchsia.hwinfo.Product")?;
        }
        Ok(())
    }

    async fn handle_request(&self, request: ProductRequest) -> Result<(), Error> {
        match request {
            ProductRequest::GetInfo { responder } => {
                responder
                    .send(&self.product_info_table.read().clone().into())
                    .context("error sending response")?;
            }
        };
        Ok(())
    }
}
