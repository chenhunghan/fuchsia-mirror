// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context;
use blob_pager_and_verifier::BlobPagerAndVerifier;
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_mapping as fmapping;
use fuchsia_component::client::connect_to_protocol;
use futures::{FutureExt as _, StreamExt, TryFutureExt, TryStreamExt};
use log::error;
use std::sync::Arc;

// `pkgdir-component` is a lightweight stand-in for `pkg-cache` used by storage benchmarks to host
// `package_directory::serve` in a separate component process. Like `pkg-cache` in production, it
// uses `BlobPagerAndVerifier` in-process to back `blobfs::Client` when connected to Fxblob.
struct DriverBlobReader(BlobPagerAndVerifier);

impl std::fmt::Debug for DriverBlobReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("DriverBlobReader").finish_non_exhaustive()
    }
}

impl blobfs::BlobReader for DriverBlobReader {
    fn get_vmo<'a>(
        &'a self,
        blob: &'a fuchsia_hash::Hash,
    ) -> futures::future::BoxFuture<'a, Result<zx::Vmo, blobfs::GetBlobVmoError>> {
        async move {
            self.0.create_vmo(blob).await.map_err(|e| {
                if let Some(&status) = e.root_cause().downcast_ref::<zx::Status>() {
                    blobfs::GetBlobVmoError::GetVmo(status)
                } else {
                    blobfs::GetBlobVmoError::GetVmo(zx::Status::INTERNAL)
                }
            })
        }
        .boxed()
    }
}

impl blobfs::IntoBlobReader for DriverBlobReader {
    fn into_blob_reader(self) -> Arc<dyn blobfs::BlobReader> {
        Arc::new(self)
    }
}

enum IncomingService {
    PkgDir(fidl_test_pkgdir::PkgDirRequestStream),
}

#[fuchsia::main]
async fn main() -> anyhow::Result<()> {
    fuchsia_trace_provider::trace_provider_create_with_fdio();
    let mut builder = blobfs::Client::builder().readable();
    if let (Ok(mapping_provider), Ok(mapper)) = (
        connect_to_protocol::<fmapping::MappingProviderMarker>(),
        connect_to_protocol::<fblock::MapperMarker>(),
    ) && let Ok(pager) = BlobPagerAndVerifier::new(&mapping_provider, &mapper).await
    {
        builder = builder.reader(DriverBlobReader(pager));
    }
    let blobfs = builder.build().await.context("creating blobfs client")?;
    let mut service_fs = fuchsia_component::server::ServiceFs::new_local();
    service_fs.dir("svc").add_fidl_service(IncomingService::PkgDir);
    service_fs.take_and_serve_directory_handle().context("failed to serve outgoing namespace")?;
    let () = service_fs
        .for_each_concurrent(None, |request| async {
            match request {
                IncomingService::PkgDir(stream) => {
                    serve_request_stream(stream, blobfs.clone())
                        .unwrap_or_else(|e| error!("failed to serve PkgDirRequest: {:#}", e))
                        .await
                }
            }
        })
        .await;
    Ok(())
}

async fn serve_request_stream(
    mut stream: fidl_test_pkgdir::PkgDirRequestStream,
    blobfs: blobfs::Client,
) -> anyhow::Result<()> {
    while let Some(request) =
        stream.try_next().await.context("failed to read request from FIDL stream")?
    {
        match request {
            fidl_test_pkgdir::PkgDirRequest::OpenPackageDirectory { meta_far, responder } => {
                responder
                    .send(open_package_directory(fuchsia_hash::Hash::from(meta_far), &blobfs).await)
                    .context("failed to send OpenPackageDirectory response")?;
            }
        }
    }
    Ok(())
}

async fn open_package_directory(
    meta_far: fuchsia_hash::Hash,
    blobfs: &blobfs::Client,
) -> Result<ClientEnd<fio::DirectoryMarker>, i32> {
    let (client, server) = fidl::endpoints::create_endpoints();
    let () = package_directory::serve(
        package_directory::ExecutionScope::new(),
        blobfs.clone(),
        meta_far,
        fio::PERM_READABLE | fio::PERM_EXECUTABLE,
        server,
    )
    .await
    .map_err(|_| zx::Status::INTERNAL.into_raw())?;
    Ok(client)
}
