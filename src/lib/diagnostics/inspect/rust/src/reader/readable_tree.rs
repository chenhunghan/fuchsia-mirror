// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Provides implementations for common structs that can be read in its entirety. These are structs
//! that can be interpreted using the `fuchsia.inspect.Tree` protocol.

use crate::Inspector;
use crate::reader::ReaderError;
use async_trait::async_trait;

#[cfg(target_os = "fuchsia")]
pub type SnapshotSource = zx::Vmo;

#[cfg(not(target_os = "fuchsia"))]
pub type SnapshotSource = Vec<u8>;

/// Trait implemented by structs that can provide inspect data and their lazy links.
#[async_trait]
pub trait ReadableTree: Sized {
    /// Returns the lazy links names.
    async fn tree_names(&self) -> Result<Vec<String>, ReaderError>;

    /// Returns the blob links names.
    async fn blob_names(&self) -> Result<Vec<String>, ReaderError>;

    /// Returns the vmo of the current root node.
    async fn vmo(&self) -> Result<SnapshotSource, ReaderError>;

    /// Loads the lazy link of the given `name`.
    async fn read_tree(&self, name: &str) -> Result<Self, ReaderError>;

    /// Loads the blob VMO of the given `name`.
    async fn read_blob(&self, name: &str) -> Result<SnapshotSource, ReaderError>;
}

#[async_trait]
impl ReadableTree for Inspector {
    async fn vmo(&self) -> Result<SnapshotSource, ReaderError> {
        self.duplicate_vmo().ok_or(ReaderError::DuplicateVmo)
    }

    async fn tree_names(&self) -> Result<Vec<String>, ReaderError> {
        match self.state() {
            // A no-op inspector.
            None => Ok(vec![]),
            Some(state) => {
                let state = state.try_lock().map_err(ReaderError::FailedToLockState)?;
                let names =
                    state.callbacks().keys().map(|k| k.to_string()).collect::<Vec<String>>();
                Ok(names)
            }
        }
    }

    async fn blob_names(&self) -> Result<Vec<String>, ReaderError> {
        match self.state() {
            // A no-op inspector.
            None => Ok(vec![]),
            Some(state) => {
                let state = state.try_lock().map_err(ReaderError::FailedToLockState)?;
                let names = state.blobs().keys().map(|k| k.to_string()).collect::<Vec<String>>();
                Ok(names)
            }
        }
    }

    async fn read_tree(&self, name: &str) -> Result<Self, ReaderError> {
        let cloned_callbacks = self.state().and_then(|state| match state.try_lock() {
            Err(_) => None,
            Ok(state) => state.callbacks().get(name).cloned(),
        });
        let result = cloned_callbacks.map(|value| value());
        match result {
            Some(cb_result) => cb_result.await.map_err(ReaderError::LazyCallback),
            None => return Err(ReaderError::FailedToLoadTree(name.to_string())),
        }
    }

    #[cfg(target_os = "fuchsia")]
    async fn read_blob(&self, name: &str) -> Result<SnapshotSource, ReaderError> {
        self.duplicate_blob_vmo(name).map_err(|err| match err {
            crate::Error::VmoMissing => ReaderError::FailedToLoadBlob(name.to_string()),
            crate::Error::DuplicateVmoFailed(status) => ReaderError::Vmo(status),
            _ => ReaderError::Vmo(zx::Status::UNAVAILABLE),
        })
    }

    #[cfg(not(target_os = "fuchsia"))]
    async fn read_blob(&self, name: &str) -> Result<SnapshotSource, ReaderError> {
        self.copy_blob_vmo_data(name).map(Vec::from).map_err(|err| match err {
            crate::Error::VmoMissing => ReaderError::FailedToLoadBlob(name.to_string()),
            err => ReaderError::FailedToLockState(err),
        })
    }
}

#[cfg(target_os = "fuchsia")]
#[async_trait]
impl ReadableTree for fidl_fuchsia_inspect::TreeProxy {
    async fn vmo(&self) -> Result<zx::Vmo, ReaderError> {
        let tree_content = self.get_content().await.map_err(|e| ReaderError::Fidl(e.into()))?;
        tree_content.buffer.map(|b| b.vmo).ok_or(ReaderError::FetchVmo)
    }

    async fn tree_names(&self) -> Result<Vec<String>, ReaderError> {
        let (name_iterator, server_end) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_inspect::TreeNameIteratorMarker>();
        self.list_child_names(server_end).map_err(|e| ReaderError::Fidl(e.into()))?;
        let mut names = vec![];
        loop {
            let subset_names =
                name_iterator.get_next().await.map_err(|e| ReaderError::Fidl(e.into()))?;
            if subset_names.is_empty() {
                return Ok(names);
            }
            names.extend(subset_names);
        }
    }

    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    async fn blob_names(&self) -> Result<Vec<String>, ReaderError> {
        let (name_iterator, server_end) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_inspect::TreeNameIteratorMarker>();
        self.list_blob_child_names(server_end).map_err(|e| ReaderError::Fidl(e.into()))?;
        let mut names = vec![];
        loop {
            let subset_names =
                name_iterator.get_next().await.map_err(|e| ReaderError::Fidl(e.into()))?;
            if subset_names.is_empty() {
                return Ok(names);
            }
            names.extend(subset_names);
        }
    }

    #[cfg(fuchsia_api_level_less_than = "HEAD")]
    async fn blob_names(&self) -> Result<Vec<String>, ReaderError> {
        Ok(vec![])
    }

    async fn read_tree(&self, name: &str) -> Result<Self, ReaderError> {
        let (child_tree, server_end) =
            fidl::endpoints::create_proxy::<fidl_fuchsia_inspect::TreeMarker>();
        self.open_child(name, server_end).map_err(|e| ReaderError::Fidl(e.into()))?;
        Ok(child_tree)
    }

    #[cfg(fuchsia_api_level_at_least = "HEAD")]
    async fn read_blob(&self, name: &str) -> Result<zx::Vmo, ReaderError> {
        self.open_blob_child(name).await.map_err(|e| ReaderError::Fidl(e.into()))?.map_err(
            |status| match zx::Status::err_from_raw(status) {
                zx::Status::NOT_FOUND => ReaderError::FailedToLoadBlob(name.to_string()),
                status => ReaderError::Vmo(status),
            },
        )
    }

    #[cfg(fuchsia_api_level_less_than = "HEAD")]
    async fn read_blob(&self, name: &str) -> Result<zx::Vmo, ReaderError> {
        Err(ReaderError::FailedToLoadBlob(name.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::reader::Snapshot;
    use assert_matches::assert_matches;
    use inspect_format::CopyBytes;

    #[fuchsia::test]
    async fn inspector_blob_names_and_read_blob() {
        let inspector = Inspector::default();
        let blob = inspector.root().create_blob_property("ring", 16);
        blob.write_at(0, b"hello").unwrap();

        let names = inspector.blob_names().await.unwrap();
        assert_eq!(names, vec!["ring-0".to_string()]);
        assert!(inspector.tree_names().await.unwrap().is_empty());

        let source = inspector.read_blob("ring-0").await.unwrap();
        assert!(Snapshot::try_from_blob_vmo(&source).is_ok());
        let mut buf = [0u8; 37];
        source.copy_bytes(&mut buf);
        assert_eq!(&buf[32..37], b"hello");

        assert_matches!(
            inspector.read_blob("missing-0").await,
            Err(ReaderError::FailedToLoadBlob(name)) if name == "missing-0"
        );

        let (locked_tx, locked_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let writer_thread = std::thread::spawn(move || {
            blob.atomic_write(|_| {
                locked_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            })
            .unwrap();
            blob
        });
        locked_rx.recv().unwrap();

        #[cfg(target_os = "fuchsia")]
        assert_matches!(
            inspector.read_blob("ring-0").await,
            Err(ReaderError::Vmo(zx::Status::UNAVAILABLE))
        );
        #[cfg(not(target_os = "fuchsia"))]
        assert_matches!(
            inspector.read_blob("ring-0").await,
            Err(ReaderError::FailedToLockState(_))
        );

        release_tx.send(()).unwrap();
        let _blob = writer_thread.join().unwrap();
    }
}
