// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::filesystems::{BlobFilesystem, FsManagementFilesystemInstance};
use async_trait::async_trait;
use blob_pager_and_verifier::BlobPagerAndVerifier;
use fidl_fuchsia_fxfs::{BlobCreatorMarker, BlobCreatorProxy};
use fidl_fuchsia_io as fio;
use fidl_fuchsia_storage_block as fblock;
use fidl_fuchsia_storage_mapping as fmapping;
use fuchsia_component::client::{connect_to_protocol_at_dir_root, connect_to_protocol_at_dir_svc};
use fuchsia_merkle::Hash;
use std::path::Path;
use std::sync::Arc;
use storage_benchmarks::{
    BlockDeviceConfig, BlockDeviceFactory, CacheClearableFilesystem, Filesystem, FilesystemConfig,
};

/// Config object for starting Fxblob instances.
#[derive(Clone, Default)]
pub struct Fxblob;

impl Fxblob {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl FilesystemConfig for Fxblob {
    type Filesystem = FxblobInstance;

    async fn start_filesystem(
        &self,
        block_device_factory: &dyn BlockDeviceFactory,
    ) -> FxblobInstance {
        let block_device = block_device_factory
            .create_block_device(&BlockDeviceConfig {
                requires_fvm: false,
                use_zxcrypt: false,
                volume_size: Some(104 * 1024 * 1024),
            })
            .await;
        let fxblob = FsManagementFilesystemInstance::new(
            || fs_management::Fxfs { allow_type3_blobs: true, ..Default::default() },
            block_device,
            None,
            /*as_blob=*/ true,
        )
        .await;
        let blob_creator =
            connect_to_protocol_at_dir_svc::<BlobCreatorMarker>(fxblob.exposed_dir())
                .expect("failed to connect to the BlobCreator protocol");
        let pager_and_verifier =
            Some(Arc::new(FxblobInstance::create_pager_and_verifier(&fxblob).await));
        FxblobInstance { blob_creator, pager_and_verifier, fxblob }
    }

    fn name(&self) -> String {
        "fxblob".to_owned()
    }
}

pub struct FxblobInstance {
    blob_creator: BlobCreatorProxy,
    pager_and_verifier: Option<Arc<BlobPagerAndVerifier>>,
    fxblob: FsManagementFilesystemInstance,
}

impl FxblobInstance {
    async fn create_pager_and_verifier(
        fxblob: &FsManagementFilesystemInstance,
    ) -> BlobPagerAndVerifier {
        let mapping_provider =
            connect_to_protocol_at_dir_svc::<fmapping::MappingProviderMarker>(fxblob.exposed_dir())
                .expect("failed to connect to the MappingProvider protocol");
        let mapper = connect_to_protocol_at_dir_svc::<fblock::MapperMarker>(fxblob.exposed_dir())
            .expect("failed to connect to the Mapper protocol");
        BlobPagerAndVerifier::new(&mapping_provider, &mapper)
            .await
            .expect("failed to initialize BlobPagerAndVerifier")
    }

    pub fn connect_to_debug(&self) -> fidl_fuchsia_fxfs::DebugProxy {
        connect_to_protocol_at_dir_root::<fidl_fuchsia_fxfs::DebugMarker>(
            self.fxblob.exposed_services_dir(),
        )
        .expect("failed to connect to Debug protocol")
    }

    pub fn clear_unreferenced_blobs(&self) {
        if let Some(pager) = &self.pager_and_verifier {
            pager.clear_cache();
        }
    }

    pub fn get_file_mapper_handler(
        &self,
        scope: vfs::execution_scope::ExecutionScope,
    ) -> Arc<vfs::service::Service> {
        self.pager_and_verifier
            .as_ref()
            .expect("driver blob paging is not enabled")
            .get_file_mapper_handler(scope)
    }
}

#[async_trait]
impl Filesystem for FxblobInstance {
    async fn shutdown(self) {
        drop(self.pager_and_verifier);
        self.fxblob.shutdown().await
    }

    fn benchmark_dir(&self) -> &Path {
        self.fxblob.benchmark_dir()
    }
}

#[async_trait]
impl CacheClearableFilesystem for FxblobInstance {
    async fn clear_cache(&mut self) {
        self.pager_and_verifier.take();
        let () = self.fxblob.clear_cache().await;
        self.blob_creator =
            connect_to_protocol_at_dir_svc::<BlobCreatorMarker>(self.fxblob.exposed_dir())
                .expect("failed to connect to the BlobCreator protocol");
        self.pager_and_verifier =
            Some(Arc::new(Self::create_pager_and_verifier(&self.fxblob).await));
    }
}

#[async_trait]
impl BlobFilesystem for FxblobInstance {
    async fn get_vmo(&self, name: &Hash) -> zx::Vmo {
        self.pager_and_verifier
            .as_ref()
            .unwrap()
            .create_vmo(&**name)
            .await
            .expect("failed to get vmo from BlobPagerAndVerifier")
    }

    fn blob_creator(&self) -> &BlobCreatorProxy {
        &self.blob_creator
    }

    fn exposed_dir(&self) -> &fio::DirectoryProxy {
        self.fxblob.exposed_dir()
    }
}

#[cfg(test)]
mod tests {
    use super::Fxblob;
    use crate::filesystems::testing::check_blob_filesystem;

    #[fuchsia::test]
    async fn start_fxblob_new() {
        check_blob_filesystem(Fxblob::new()).await;
    }
}
