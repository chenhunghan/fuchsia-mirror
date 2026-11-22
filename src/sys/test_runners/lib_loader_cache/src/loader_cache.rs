// Copyright 2022 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::Context;
use fidl::endpoints::{ControlHandle, ServerEnd};
use fidl_fuchsia_io as fio;
use fidl_fuchsia_ldsvc::{LoaderMarker, LoaderRequest};
use fidl_fuchsia_test_runner as ftestrunner;
use ftestrunner::LibraryLoaderCacheMarker;
use fuchsia_async as fasync;
use futures::lock::Mutex as FutMutex;
use futures::prelude::*;
use log::warn;
use std::collections::HashMap;
use std::sync::{Arc, Weak};

/// Cached status and VMO for a single library name.
type CachedObject = (i32, Option<Arc<zx::Vmo>>);

/// Maps library name to a per-object coalescing mutex so concurrent misses for the same
/// library issue only one filesystem open while distinct libraries load in parallel.
type VmoKeyMap = HashMap<String, Arc<FutMutex<Option<CachedObject>>>>;

#[derive(Debug)]
pub struct LibraryLoaderCache {
    /// Proxy to /pkg/lib
    lib_proxy: fio::DirectoryProxy,

    /// Mapping of config key with loaded VMOs map.
    load_response_map: FutMutex<HashMap<String, Arc<FutMutex<VmoKeyMap>>>>,
}

impl LibraryLoaderCache {
    pub fn new(lib_proxy: fio::DirectoryProxy) -> Arc<Self> {
        return Arc::new(Self { lib_proxy, load_response_map: FutMutex::new(HashMap::new()) });
    }
}

pub async fn serve_cache(
    cache: Arc<LibraryLoaderCache>,
    server_end: ServerEnd<LibraryLoaderCacheMarker>,
) -> Result<(), anyhow::Error> {
    let mut stream = server_end.into_stream();
    while let Some(event) = stream.try_next().await? {
        match event {
            ftestrunner::LibraryLoaderCacheRequest::Serve { loader, .. } => {
                let cache = Arc::downgrade(&cache);
                serve_lib_loader(loader, cache);
            }
        }
    }
    Ok(())
}

fn vmo_create_child(vmo: &zx::Vmo) -> Result<zx::Vmo, anyhow::Error> {
    let size = vmo.get_size().context("Cannot get vmo size.")?;
    vmo.create_child(
        zx::VmoChildOptions::SNAPSHOT_AT_LEAST_ON_WRITE | zx::VmoChildOptions::NO_WRITE,
        0,
        size,
    )
    .context("cannot create child vmo")
}

fn duplicate_vmo(vmo: &Option<Arc<zx::Vmo>>) -> Result<Option<zx::Vmo>, anyhow::Error> {
    Ok(match vmo {
        // create child instead of duplicating so that our debugger tools don't break.
        // Also vmo created using create child is non-writable, but debugger is able to write to it
        // as it has special permissions.
        Some(vmo) => vmo_create_child(vmo)?.into(),
        None => None,
    })
}

/// Serve a custom lib loader which caches request to load VMOs.
fn serve_lib_loader(loader: ServerEnd<LoaderMarker>, lib_loader_cache: Weak<LibraryLoaderCache>) {
    fasync::Scope::global().spawn(async move {
        let (mut search_dirs, mut current_response_map) = match lib_loader_cache.upgrade() {
            Some(obj) => (
                Arc::new(vec![Clone::clone(&obj.lib_proxy)]),
                obj.load_response_map.lock().await.entry("".to_string()).or_default().clone(),
            ),
            None => return,
        };

        let scope = fasync::Scope::new();
        let res: Result<(), anyhow::Error> = async {
            let mut stream = loader.into_stream();
            while let Some(req) = stream.try_next().await? {
                let lib_loader_cache = match lib_loader_cache.upgrade() {
                    Some(obj) => obj,
                    None => break,
                };
                match req {
                    LoaderRequest::Done { control_handle } => {
                        control_handle.shutdown();
                    }
                    LoaderRequest::LoadObject { object_name, responder } => {
                        let search_dirs = Arc::clone(&search_dirs);
                        let current_response_map = Arc::clone(&current_response_map);
                        scope.spawn(async move {
                            let entry_mutex = {
                                let mut map = current_response_map.lock().await;
                                map.entry(object_name.clone())
                                    .or_insert_with(|| Arc::new(FutMutex::new(None)))
                                    .clone()
                            };

                            let (rv, vmo_arc) = {
                                let mut entry = entry_mutex.lock().await;
                                if entry.is_none() {
                                    let loaded = match library_loader::load_object(
                                        &search_dirs,
                                        &object_name,
                                    )
                                    .await
                                    {
                                        Ok(b) => (zx::sys::ZX_OK, Some(Arc::new(b))),
                                        Err(e) => {
                                            warn!("failed to load object: {e:?}");
                                            (zx::sys::ZX_ERR_NOT_FOUND, None)
                                        }
                                    };
                                    *entry = Some(loaded);
                                }
                                let (rv, vmo) = entry.as_ref().unwrap();
                                (*rv, vmo.clone())
                            };

                            let send_res = match duplicate_vmo(&vmo_arc) {
                                Ok(vmo_clone) => responder.send(rv, vmo_clone),
                                Err(e) => {
                                    warn!("failed to duplicate vmo: {e:?}");
                                    responder.send(zx::sys::ZX_ERR_INTERNAL, None)
                                }
                            };
                            if let Err(e) = send_res
                                && !e.is_closed()
                            {
                                warn!("failed to send LoadObject response: {e}");
                            }
                        });
                    }
                    LoaderRequest::Config { config, responder } => {
                        match library_loader::parse_config_string(
                            &vec![Clone::clone(&lib_loader_cache.lib_proxy)],
                            &config,
                        ) {
                            Ok(new_search_path) => {
                                search_dirs = Arc::new(new_search_path);
                                current_response_map = lib_loader_cache
                                    .load_response_map
                                    .lock()
                                    .await
                                    .entry(config)
                                    .or_default()
                                    .clone();
                                responder.send(zx::sys::ZX_OK)?;
                            }
                            Err(e) => {
                                warn!("failed to parse config: {e}");
                                responder.send(zx::sys::ZX_ERR_INVALID_ARGS)?;
                            }
                        }
                    }
                    LoaderRequest::Clone { loader, responder } => {
                        serve_lib_loader(loader, Arc::downgrade(&lib_loader_cache));
                        responder.send(zx::sys::ZX_OK)?;
                    }
                }
            }
            Ok(())
        }
        .await;
        scope.join().await;
        if let Err(e) = res {
            warn!("couldn't run library loader service: {e:?}");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Error;
    use assert_matches::assert_matches;

    async fn serve_vmo(object: zx::Channel, content: &[u8]) {
        let mut stream = ServerEnd::<fio::FileMarker>::new(object).into_stream();
        let Some(Ok(fio::FileRequest::GetBackingMemory { responder, .. })) = stream.next().await
        else {
            panic!("expected GetBackingMemory");
        };
        let vmo = zx::Vmo::create(content.len() as u64).unwrap();
        vmo.write(content, 0).unwrap();
        responder.send(Ok(vmo)).unwrap();
    }

    fn assert_loaded((rv, vmo): (i32, Option<zx::Vmo>), expected: &[u8]) {
        assert_eq!(rv, zx::sys::ZX_OK);
        let bytes = vmo.expect("missing vmo").read_to_vec::<u8>(0, expected.len() as u64).unwrap();
        assert_eq!(bytes, expected);
    }

    #[fuchsia::test]
    async fn load_objects_test() -> Result<(), Error> {
        // Open this test's real /pkg/lib directory to use for this test, and if ld.so.1 is in a
        // variant subdirectory (such as /pkg/lib/asan or /pkg/lib/ubsan), use that subdirectory
        // instead.
        // TODO(https://fxbug.dev/42061196): Replace conditional logic with a pseudo-directory using Rust VFS.
        // LINT.IfChange
        let rights = fio::PERM_READABLE | fio::PERM_EXECUTABLE;
        let mut pkg_lib = fuchsia_fs::directory::open_in_namespace("/pkg/lib", rights)?;
        let entries = fuchsia_fs::directory::readdir(&pkg_lib).await?;
        if !entries.iter().any(|e| e.name == "ld.so.1") {
            for entry in entries
                .into_iter()
                .filter(|e| e.kind == fuchsia_fs::directory::DirentKind::Directory)
            {
                let subdir =
                    fuchsia_fs::directory::open_directory_async(&pkg_lib, &entry.name, rights)?;
                if fuchsia_fs::directory::readdir(&subdir)
                    .await?
                    .iter()
                    .any(|e| e.name == "ld.so.1")
                {
                    pkg_lib = subdir;
                    break;
                }
            }
        }
        // LINT.ThenChange(//src/sys/lib/library_loader/src/lib.rs)

        let (loader_proxy, loader_service) = fidl::endpoints::create_proxy::<LoaderMarker>();
        let cache = Arc::new(LibraryLoaderCache {
            lib_proxy: pkg_lib.into(),
            load_response_map: FutMutex::new(HashMap::new()),
        });
        serve_lib_loader(loader_service, Arc::downgrade(&cache));
        let tests = vec![
            // Should be able to access lib/ld.so.1
            ("ld.so.1", true),
            // Should be able to access lib/libfdio.so
            ("libfdio.so", true),
            // Should not be able to access lib/lib/ld.so.1
            ("lib/ld.so.1", false),
            // Should not be able to access lib/../lib/ld.so.1
            ("../lib/ld.so.1", false),
            // Should not be able to access lib/bin/test-runner-unit-tests
            ("bin/test-runner-unit-tests", false),
            // Should not be able to access bin/test-runner-unit-tests
            ("../bin/test-runner-unit-tests", false),
            // Should not be able to access meta/test-runner-unit-tests.cm
            ("../meta/test-runner-unit-tests.cm", false),
        ];
        for &(obj_name, should_succeed) in &tests {
            let (res, o_vmo) = loader_proxy.load_object(obj_name).await?;
            let map = cache.load_response_map.lock().await.get("").unwrap().clone();
            assert!(map.lock().await.contains_key(obj_name));
            let entry = map.lock().await.get(obj_name).unwrap().clone();
            if should_succeed {
                assert_eq!(zx::sys::ZX_OK, res, "loading {} did not succeed", obj_name);
                assert!(o_vmo.is_some());
                assert_matches!(entry.lock().await.as_ref().unwrap().1, Some(_));
            } else {
                assert_ne!(zx::sys::ZX_OK, res, "loading {} did not fail", obj_name);
                assert!(o_vmo.is_none());
                assert!(entry.lock().await.as_ref().unwrap().1.is_none());
            }
        }

        // also test clone
        let (loader_proxy2, loader_service) = fidl::endpoints::create_proxy::<LoaderMarker>();
        assert_eq!(zx::sys::ZX_OK, loader_proxy.clone(loader_service).await?);
        for (obj_name, should_succeed) in tests {
            let (res, o_vmo) = loader_proxy2.load_object(obj_name).await?;
            if should_succeed {
                assert_eq!(zx::sys::ZX_OK, res, "loading {} did not succeed", obj_name);
                assert!(o_vmo.is_some());
            } else {
                assert_ne!(zx::sys::ZX_OK, res, "loading {} did not fail", obj_name);
                assert!(o_vmo.is_none());
            }
        }

        // test done
        loader_proxy2.done().expect("done should not fail");
        let err = loader_proxy2
            .load_object("some_name")
            .await
            .expect_err("Should fail with PEER_CLOSED.");

        let epitaph = assert_matches!(
            err,
            fidl::Error::ClientChannelClosed { epitaph, .. } => epitaph
        );
        assert_eq!(epitaph, zx::Status::PEER_CLOSED);
        Ok(())
    }

    #[fuchsia::test]
    async fn config_test() -> Result<(), Error> {
        // This /pkg/lib/config_test/ directory is added by the build rules for this test package,
        // since we need a directory that supports OPEN_RIGHT_EXECUTABLE. It contains a file 'foo'
        // which contains 'hippos' and a file 'bar/baz' (that is, baz in a subdirectory bar) which
        // contains 'rule'.
        // TODO(https://fxbug.dev/42061196): Replace conditional logic with a pseudo-directory using Rust VFS.
        let pkg_lib = fuchsia_fs::directory::open_in_namespace(
            "/pkg/lib/config_test/",
            fio::PERM_READABLE | fio::PERM_EXECUTABLE,
        )?;
        let (loader_proxy, loader_service) = fidl::endpoints::create_proxy::<LoaderMarker>();
        let cache = Arc::new(LibraryLoaderCache {
            lib_proxy: pkg_lib.into(),
            load_response_map: FutMutex::new(HashMap::new()),
        });
        serve_lib_loader(loader_service, Arc::downgrade(&cache));

        // Attempt to access things with different configurations
        for (obj_name, config, expected_result) in vec![
            // Should be able to load foo
            ("foo", None, Some("hippos")),
            // Should not be able to load bar (it's a directory)
            ("bar", None, None),
            // Should not be able to load baz (it's in a sub directory)
            ("baz", None, None),
            // Should be able to load baz with config "bar!" (only look in sub directory bar)
            ("baz", Some("bar!"), Some("rule")),
            // Should not be able to load foo with config "bar!" (only look in sub directory bar)
            ("foo", Some("bar!"), None),
            // Should be able to load foo with config "bar" (also look in sub directory bar)
            ("foo", Some("bar"), Some("hippos")),
            // Should be able to load baz with config "bar" (also look in sub directory bar)
            ("baz", Some("bar"), Some("rule")),
        ] {
            if let Some(config) = config {
                assert_eq!(zx::sys::ZX_OK, loader_proxy.config(config).await?);
            }

            let (res, o_vmo) = loader_proxy.load_object(obj_name).await?;
            let map = cache
                .load_response_map
                .lock()
                .await
                .get(config.unwrap_or_default())
                .unwrap()
                .clone();
            let entry = map.lock().await.get(obj_name).unwrap().clone();
            if let Some(expected_result) = expected_result {
                assert_eq!(zx::sys::ZX_OK, res);
                let mut buf = vec![0; expected_result.len()];
                o_vmo.expect("missing vmo").read(&mut buf, 0)?;
                assert_eq!(expected_result.as_bytes(), buf.as_slice());
                assert_matches!(entry.lock().await.as_ref().unwrap().1, Some(_));
            } else {
                assert_ne!(zx::sys::ZX_OK, res);
                assert!(o_vmo.is_none());
                assert!(entry.lock().await.as_ref().unwrap().1.is_none());
            }
        }
        Ok(())
    }

    #[fuchsia::test]
    async fn concurrent_load_objects_and_coalescing_test() -> Result<(), Error> {
        let (dir_proxy, mut dir_stream) =
            fidl::endpoints::create_proxy_and_stream::<fio::DirectoryMarker>();
        let (loader_proxy, loader_service) = fidl::endpoints::create_proxy::<LoaderMarker>();
        let cache = LibraryLoaderCache::new(dir_proxy);
        serve_lib_loader(loader_service, Arc::downgrade(&cache));

        let mut slow_fut = loader_proxy.load_object("slow.so");
        let mut shared_futs =
            std::array::from_fn::<_, 4, _>(|_| loader_proxy.load_object("shared.so"));
        let fast_fut = loader_proxy.load_object("fast.so");

        let mut slow_obj = None;
        let mut shared_obj = None;
        let mut fast_obj = None;
        for _ in 0..3 {
            let Some(Ok(fio::DirectoryRequest::Open { path, object, .. })) =
                dir_stream.next().await
            else {
                panic!("unexpected end of directory request stream");
            };
            match path.as_str() {
                "slow.so" => assert!(slow_obj.replace(object).is_none()),
                "shared.so" => assert!(shared_obj.replace(object).is_none()),
                "fast.so" => assert!(fast_obj.replace(object).is_none()),
                other => panic!("unexpected Open path: {other}"),
            }
        }

        // fast.so must complete out-of-order while slow.so and shared.so are still stalled.
        serve_vmo(fast_obj.unwrap(), b"fast").await;
        assert_loaded(fast_fut.await?, b"fast");
        assert!((&mut slow_fut).now_or_never().is_none());
        for fut in &mut shared_futs {
            assert!(fut.now_or_never().is_none());
        }

        // Answer shared.so and verify all 4 concurrent cold-cache requests succeed from 1 open.
        serve_vmo(shared_obj.unwrap(), b"shared").await;
        for result in futures::future::try_join_all(shared_futs).await? {
            assert_loaded(result, b"shared");
        }
        assert!(dir_stream.next().now_or_never().is_none());
        assert!((&mut slow_fut).now_or_never().is_none());

        // Finally answer slow.so.
        serve_vmo(slow_obj.unwrap(), b"slow").await;
        assert_loaded(slow_fut.await?, b"slow");

        Ok(())
    }
}
