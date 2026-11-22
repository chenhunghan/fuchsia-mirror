// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Fake `fuchsia.boot.Items` provider serving a precompiled devicetree blob for Starnix syscall
//! tests.

use anyhow::Error;
use fidl_fuchsia_boot as fboot;
use fuchsia_component::server::ServiceFs;
use futures::{StreamExt as _, TryStreamExt as _};

const TEST_DTB_PATH: &str = "/pkg/data/test_devicetree.dtb";

fn create_devicetree_vmo(bytes: &[u8]) -> (zx::Vmo, u32) {
    let len = bytes.len() as u32;
    let vmo = zx::Vmo::create(bytes.len() as u64).expect("failed to create devicetree VMO");
    vmo.write(bytes, 0).expect("failed to write devicetree VMO");
    (vmo, len)
}

#[fuchsia::main]
async fn main() -> Result<(), Error> {
    let dtb_bytes = std::fs::read(TEST_DTB_PATH)?;

    let mut service_fs = ServiceFs::new_local();
    service_fs.dir("svc").add_fidl_service(|stream: fboot::ItemsRequestStream| stream);
    service_fs.take_and_serve_directory_handle()?;

    let devicetree_type: u32 = zbi::Type::DEVICETREE.into();
    let dtb_bytes = dtb_bytes.as_slice();
    service_fs
        .for_each_concurrent(None, |stream: fboot::ItemsRequestStream| async move {
            let _ = stream
                .try_for_each(|request| async move {
                    match request {
                        fboot::ItemsRequest::Get { type_, extra: _, responder } => {
                            if type_ == devicetree_type {
                                let (vmo, len) = create_devicetree_vmo(dtb_bytes);
                                responder.send(Some(vmo), len)?;
                            } else {
                                responder.send(None, 0)?;
                            }
                        }
                        fboot::ItemsRequest::Get2 { type_, extra: _, responder } => {
                            if type_ == devicetree_type {
                                let (vmo, length) = create_devicetree_vmo(dtb_bytes);
                                responder.send(Ok(vec![fboot::RetrievedItems {
                                    payload: vmo,
                                    length,
                                    extra: 0,
                                }]))?;
                            } else {
                                responder.send(Ok(vec![]))?;
                            }
                        }
                        fboot::ItemsRequest::GetBootloaderFile { responder, .. } => {
                            responder.send(None)?;
                        }
                    }
                    Ok(())
                })
                .await;
        })
        .await;

    Ok(())
}
