// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use fidl_fuchsia_io as fio;
use fuchsia_inspect as finspect;
use futures::FutureExt as _;
use futures::future::BoxFuture;
use std::collections::HashMap;
use std::collections::hash_map::Entry::{Occupied, Vacant};
use std::sync::{Arc, Weak};
use vfs::ObjectRequestRef;
use vfs::directory::entry::{EntryInfo, OpenRequest};
use vfs::directory::traversal_position::TraversalPosition;
use vfs::execution_scope::ExecutionScope;

/// `RootDirCache` is a cache of `Arc<impl Deref<Target = RootDir>>`s indexed by their hash.
///
/// The cache internally stores `Weak`s and drops the corresponding entry when the last `Arc` is
/// dropped, so it is a cache of `Arc`s that are actively in use by its clients. This is useful for
/// deduplicating the `RootDir`s used by VFS to serve package directory connections while also
/// keeping track of which connections are open.
///
/// Due to `RootDir`'s VFS implementation, a package will have alive `Arc`s if there are
/// fuchsia.io.Directory connections to the package's root directory or any sub directory.
///
/// This does not keep track of fuchsia.io.File connections made to a `RootDir`s files (files under
/// meta/, files not under meta/, and the meta file) but both Blobfs and Fxblob will wait to delete
/// blobs that have open connections or VMOs until the last one closes, so it is safe to delete
/// packages that are not in the `RootDirCache`.
///
/// Clients close connections to packages by closing their end of the Zircon channel over which the
/// fuchsia.io.Directory messages were being sent. Some time after the client end of the channel is
/// closed, the server (usually in a different process) will be notified by the kernel, and the VFS
/// task serving the connection will finish, dropping its `Arc`. When the last `Arc` is dropped, the
/// strong count of the corresponding `std::sync::Weak` in the `RootDirCache` will decrement to
/// zero. At this point the `RootDirCache` will no longer report the package as open.
/// All this is to say that there will be some delay between a package no longer being in use and
/// clients of `RootDirCache` finding out about that.
#[derive(Debug, Clone)]
pub struct RootDirCache<S> {
    non_meta_storage: S,
    dirs: Arc<std::sync::Mutex<HashMap<fuchsia_hash::Hash, Weak<CachedRootDir<S>>>>>,
}

impl<S: crate::NonMetaStorage + Clone> RootDirCache<S> {
    /// Creates a `RootDirCache` that uses `non_meta_storage` as the backing for the
    /// internally managed `crate::RootDir`s.
    pub fn new(non_meta_storage: S) -> Self {
        let dirs = Arc::new(std::sync::Mutex::new(HashMap::new()));
        Self { non_meta_storage, dirs }
    }

    /// Returns an `Arc<CachedRootDir>` corresponding to `hash`.
    /// If there is not already one in the cache, `root_dir` will be used if provided, otherwise
    /// a new one will be created using the `non_meta_storage` provided to `Self::new`.
    ///
    /// If provided, `root_dir` must be backed by the same `NonMetaStorage` that `Self::new` was
    /// called with.
    pub async fn get_or_insert(
        &self,
        hash: fuchsia_hash::Hash,
        root_dir: Option<crate::RootDir<S>>,
    ) -> Result<Arc<CachedRootDir<S>>, crate::Error> {
        if let Some(root_dir) = self.get(&hash) {
            return Ok(root_dir);
        }

        let root_dir = match root_dir {
            Some(root_dir) => root_dir,
            None => crate::RootDir::new_raw(self.non_meta_storage.clone(), hash).await?,
        };
        Ok(match self.dirs.lock().expect("poisoned mutex").entry(hash) {
            // Raced with another call to get_or_insert.
            Occupied(mut o) => {
                let old_root_dir = o.get_mut();
                if let Some(old_root_dir) = old_root_dir.upgrade() {
                    old_root_dir
                } else {
                    let new_root_dir = CachedRootDir::new(root_dir, &self.dirs);
                    *old_root_dir = Arc::downgrade(&new_root_dir);
                    new_root_dir
                }
            }
            Vacant(v) => {
                let new_root_dir = CachedRootDir::new(root_dir, &self.dirs);
                v.insert(Arc::downgrade(&new_root_dir));
                new_root_dir
            }
        })
    }

    /// Returns the `Arc<CachedRootDir>` with the given `hash`, if one exists in the cache.
    /// Otherwise returns `None`.
    /// Holding on to the returned `Arc` will keep the package open (as reported by
    /// `Self::list`).
    pub fn get(&self, hash: &fuchsia_hash::Hash) -> Option<Arc<CachedRootDir<S>>> {
        self.dirs.lock().expect("poisoned mutex").get(hash)?.upgrade()
    }

    /// Packages with live `Arc<CachedRootDir>`s.
    /// Holding on to the returned `Arc`s will keep the packages open.
    pub fn list(&self) -> Vec<Arc<CachedRootDir<S>>> {
        self.dirs.lock().expect("poisoned mutex").values().filter_map(|v| v.upgrade()).collect()
    }

    /// Returns a callback to be given to `fuchsia_inspect::Node::record_lazy_child`.
    /// Records the package hashes and their corresponding `Arc<CachedRootDir>` strong counts.
    pub fn record_lazy_inspect(
        &self,
    ) -> impl Fn() -> BoxFuture<'static, Result<finspect::Inspector, anyhow::Error>>
    + Send
    + Sync
    + 'static {
        let dirs = Arc::downgrade(&self.dirs);
        move || {
            let dirs = dirs.clone();
            async move {
                let inspector = finspect::Inspector::default();
                if let Some(dirs) = dirs.upgrade() {
                    let package_counts: HashMap<_, _> = {
                        let dirs = dirs.lock().expect("poisoned mutex");
                        dirs.iter().map(|(k, v)| (*k, v.strong_count() as u64)).collect()
                    };
                    let root = inspector.root();
                    let () = package_counts.into_iter().for_each(|(pkg, count)| {
                        root.record_child(pkg.to_string(), |n| n.record_uint("instances", count))
                    });
                }
                Ok(inspector)
            }
            .boxed()
        }
    }
}

/// A wrapper around `RootDir` that notifies `RootDirCache` when dropped.
#[derive(Debug)]
pub struct CachedRootDir<S> {
    root_dir: crate::RootDir<S>,
    dirs: Weak<std::sync::Mutex<HashMap<fuchsia_hash::Hash, Weak<Self>>>>,
}

impl<S> CachedRootDir<S> {
    #[allow(clippy::type_complexity)]
    fn new(
        root_dir: crate::RootDir<S>,
        dirs: &Arc<std::sync::Mutex<HashMap<fuchsia_hash::Hash, Weak<Self>>>>,
    ) -> Arc<Self> {
        Arc::new(Self { root_dir, dirs: Arc::downgrade(dirs) })
    }
}

impl<S> std::ops::Deref for CachedRootDir<S> {
    type Target = crate::RootDir<S>;

    fn deref(&self) -> &Self::Target {
        &self.root_dir
    }
}

impl<S: crate::NonMetaStorage> crate::root_dir::AsRootDir for CachedRootDir<S> {
    type Storage = S;

    fn as_root_dir(&self) -> &crate::RootDir<S> {
        &self.root_dir
    }
}

impl<S: crate::NonMetaStorage> vfs::directory::entry::DirectoryEntry for CachedRootDir<S> {
    fn open_entry(self: Arc<Self>, request: OpenRequest<'_>) -> Result<(), zx::Status> {
        request.open_dir(self)
    }
}

impl<S: crate::NonMetaStorage> vfs::directory::entry::GetEntryInfo for CachedRootDir<S> {
    fn entry_info(&self) -> EntryInfo {
        self.root_dir.entry_info()
    }
}

impl<S: crate::NonMetaStorage> vfs::node::Node for CachedRootDir<S> {
    async fn get_attributes(
        &self,
        requested_attributes: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, zx::Status> {
        self.root_dir.get_attributes(requested_attributes).await
    }
}

impl<S: crate::NonMetaStorage> vfs::directory::entry_container::Directory for CachedRootDir<S> {
    fn open(
        self: Arc<Self>,
        scope: ExecutionScope,
        path: vfs::Path,
        flags: fio::Flags,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), zx::Status> {
        crate::root_dir::open_impl(self, scope, path, flags, object_request)
    }

    async fn read_dirents(
        &self,
        pos: &TraversalPosition,
        sink: Box<dyn vfs::directory::dirents_sink::Sink + 'static>,
    ) -> Result<
        (TraversalPosition, Box<dyn vfs::directory::dirents_sink::Sealed + 'static>),
        zx::Status,
    > {
        self.root_dir.read_dirents(pos, sink).await
    }

    fn register_watcher(
        self: Arc<Self>,
        _: ExecutionScope,
        _: fio::WatchMask,
        _: vfs::directory::entry_container::DirectoryWatcher,
    ) -> Result<(), zx::Status> {
        Err(zx::Status::NOT_SUPPORTED)
    }

    // `register_watcher` is unsupported so no need to do anything here.
    fn unregister_watcher(self: Arc<Self>, _: usize) {}
}

impl<S> Drop for CachedRootDir<S> {
    fn drop(&mut self) {
        let Some(dirs) = self.dirs.upgrade() else {
            return;
        };
        match dirs.lock().expect("poisoned mutex").entry(self.root_dir.hash) {
            Occupied(o) => {
                // In case this raced with a call to get_or_insert that added a new one.
                if o.get().strong_count() == 0 {
                    o.remove_entry();
                }
            }
            // Raced with another call to get_or_insert that already removed the entry.
            Vacant(_) => (),
        };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use assert_matches::assert_matches;
    use diagnostics_assertions::assert_data_tree;
    use fuchsia_async as fasync;
    use fuchsia_pkg_testing::PackageBuilder;
    use fuchsia_pkg_testing::blobfs::Fake as FakeBlobfs;

    #[fuchsia::test]
    async fn get_or_insert_new_entry() {
        let pkg = PackageBuilder::new("pkg-name").build().await.unwrap();
        let (metafar_blob, _) = pkg.contents();
        let (blobfs_fake, blobfs_client) = FakeBlobfs::new();
        blobfs_fake.add_blob(metafar_blob.merkle, metafar_blob.contents);
        let server = RootDirCache::new(blobfs_client);

        let dir = server.get_or_insert(metafar_blob.merkle, None).await.unwrap();

        assert_eq!(server.list().len(), 1);

        drop(dir);
        assert_eq!(server.list().len(), 0);
        assert!(server.dirs.lock().expect("poisoned mutex").is_empty());
    }

    #[fuchsia::test]
    async fn closing_package_connection_closes_package() {
        let pkg = PackageBuilder::new("pkg-name").build().await.unwrap();
        let (metafar_blob, _) = pkg.contents();
        let (blobfs_fake, blobfs_client) = FakeBlobfs::new();
        blobfs_fake.add_blob(metafar_blob.merkle, metafar_blob.contents);
        let server = RootDirCache::new(blobfs_client);

        let dir = server.get_or_insert(metafar_blob.merkle, None).await.unwrap();
        let (proxy, server_end) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
        let scope = vfs::execution_scope::ExecutionScope::new();
        vfs::directory::serve_on(dir, fio::PERM_READABLE, scope.clone(), server_end);
        let _ = proxy
            .get_attributes(Default::default())
            .await
            .expect("directory succesfully handling requests");
        assert_eq!(server.list().len(), 1);

        drop(proxy);
        let () = scope.wait().await;
        assert_eq!(server.list().len(), 0);
        assert!(server.dirs.lock().expect("poisoned mutex").is_empty());
    }

    #[fuchsia::test]
    async fn open_subdirectory_keeps_package_open() {
        let pkg = PackageBuilder::new("pkg-name")
            .add_resource_at("dir/file", "bloblob".as_bytes())
            .add_resource_at("meta/dir/file", "meta-contents".as_bytes())
            .build()
            .await
            .unwrap();
        let (metafar_blob, content_blobs) = pkg.contents();
        let (blobfs_fake, blobfs_client) = FakeBlobfs::new();
        blobfs_fake.add_blob(metafar_blob.merkle, metafar_blob.contents);
        for (hash, bytes) in content_blobs {
            blobfs_fake.add_blob(hash, bytes);
        }
        let server = RootDirCache::new(blobfs_client);

        for subdir_path in ["meta", "meta/dir", "dir"] {
            let dir = server.get_or_insert(metafar_blob.merkle, None).await.unwrap();
            let (proxy, server_end) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
            let scope = vfs::execution_scope::ExecutionScope::new();
            vfs::directory::serve_on(dir, fio::PERM_READABLE, scope.clone(), server_end);
            let subdir =
                fuchsia_fs::directory::open_directory(&proxy, subdir_path, fio::PERM_READABLE)
                    .await
                    .unwrap();
            assert_eq!(Arc::strong_count(&server.list()[0]), 3);

            drop(proxy);
            while Arc::strong_count(&server.list()[0]) != 2 {
                let () = fasync::Timer::new(std::time::Duration::from_millis(10)).await;
            }

            drop(subdir);
            let () = scope.wait().await;
            assert_eq!(server.list().len(), 0);
            assert!(server.dirs.lock().expect("poisoned mutex").is_empty());
        }
    }

    #[fuchsia::test]
    async fn get_or_insert_existing_entry() {
        let pkg = PackageBuilder::new("pkg-name").build().await.unwrap();
        let (metafar_blob, _) = pkg.contents();
        let (blobfs_fake, blobfs_client) = FakeBlobfs::new();
        blobfs_fake.add_blob(metafar_blob.merkle, metafar_blob.contents);
        let server = RootDirCache::new(blobfs_client);

        let dir0 = server.get_or_insert(metafar_blob.merkle, None).await.unwrap();

        let dir1 = server.get_or_insert(metafar_blob.merkle, None).await.unwrap();
        assert_eq!(server.list().len(), 1);
        assert_eq!(Arc::strong_count(&server.list()[0]), 3);

        drop(dir0);
        drop(dir1);
        assert_eq!(server.list().len(), 0);
        assert!(server.dirs.lock().expect("poisoned mutex").is_empty());
    }

    #[fuchsia::test]
    async fn get_or_insert_provided_root_dir() {
        let pkg = PackageBuilder::new("pkg-name").build().await.unwrap();
        let (metafar_blob, _) = pkg.contents();
        let (blobfs_fake, blobfs_client) = FakeBlobfs::new();
        blobfs_fake.add_blob(metafar_blob.merkle, metafar_blob.contents);
        let root_dir =
            crate::RootDir::new_raw(blobfs_client.clone(), metafar_blob.merkle).await.unwrap();
        blobfs_fake.delete_blob(metafar_blob.merkle);
        let server = RootDirCache::new(blobfs_client);

        let dir = server.get_or_insert(metafar_blob.merkle, Some(root_dir)).await.unwrap();
        assert_eq!(server.list().len(), 1);

        drop(dir);
        assert_eq!(server.list().len(), 0);
        assert!(server.dirs.lock().expect("poisoned mutex").is_empty());
    }

    #[fuchsia::test]
    async fn get_or_insert_fails_if_root_dir_creation_fails() {
        let (_blobfs_fake, blobfs_client) = FakeBlobfs::new();
        let server = RootDirCache::new(blobfs_client);

        assert_matches!(
            server.get_or_insert([0; 32].into(), None).await,
            Err(crate::Error::MissingMetaFar)
        );
        assert!(server.dirs.lock().expect("poisoned mutex").is_empty());
    }

    #[fuchsia::test]
    async fn get_or_insert_concurrent_race_to_insert_new_root_dir() {
        let pkg = PackageBuilder::new("pkg-name").build().await.unwrap();
        let (metafar_blob, _) = pkg.contents();
        let (blobfs_fake, blobfs_client) = FakeBlobfs::new();
        blobfs_fake.add_blob(metafar_blob.merkle, metafar_blob.contents);
        let server = RootDirCache::new(blobfs_client);

        let fut0 = server.get_or_insert(metafar_blob.merkle, None);

        let fut1 = server.get_or_insert(metafar_blob.merkle, None);

        let (res0, res1) = futures::future::join(fut0, fut1).await;
        let (dir0, dir1) = (res0.unwrap(), res1.unwrap());

        assert_eq!(server.list().len(), 1);
        assert_eq!(Arc::strong_count(&server.list()[0]), 3);

        drop(dir0);
        drop(dir1);
        assert_eq!(server.list().len(), 0);
        assert!(server.dirs.lock().expect("poisoned mutex").is_empty());
    }

    #[fuchsia::test]
    async fn inspect() {
        let pkg = PackageBuilder::new("pkg-name").build().await.unwrap();
        let (metafar_blob, _) = pkg.contents();
        let (blobfs_fake, blobfs_client) = FakeBlobfs::new();
        blobfs_fake.add_blob(metafar_blob.merkle, metafar_blob.contents);
        let server = RootDirCache::new(blobfs_client);
        let _dir = server.get_or_insert(metafar_blob.merkle, None).await.unwrap();

        let inspector = finspect::Inspector::default();
        inspector.root().record_lazy_child("open-packages", server.record_lazy_inspect());

        assert_data_tree!(inspector, root: {
            "open-packages": {
                pkg.hash().to_string() => {
                    "instances": 1u64,
                },
            }
        });
    }
}
