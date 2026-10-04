// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Implementation of the network socket proxy.
//!
//! Runs proxied versions of fuchsia.posix.socket.Provider and fuchsia.posix.socket.raw.Provider.

use fidl_fuchsia_net as fnet;
use fidl_fuchsia_posix_socket::{self as fposix_socket, OptionalUint32};
use fuchsia_async as fasync;
use fuchsia_component::server::{ServiceFs, ServiceFsDir};
use fuchsia_inspect::health::Reporter;
use fuchsia_inspect_derive::{IValue, Inspect, Unit, WithInspect as _};
use futures::StreamExt as _;
use futures::lock::Mutex;
use log::error;
use std::sync::Arc;

mod mark_watcher;
mod socket_provider;

#[derive(Copy, Clone, Debug, PartialEq, Eq)]
struct SocketMarks {
    mark_1: OptionalUint32,
    mark_2: OptionalUint32,
}

impl From<fnet::Marks> for SocketMarks {
    fn from(fnet::Marks { mark_1, mark_2, __source_breaking }: fnet::Marks) -> Self {
        let into_optional_uint32 = |opt| match opt {
            Some(val) => OptionalUint32::Value(val),
            None => OptionalUint32::Unset(fposix_socket::Empty),
        };
        Self { mark_1: into_optional_uint32(mark_1), mark_2: into_optional_uint32(mark_2) }
    }
}

impl From<SocketMarks> for fnet::Marks {
    fn from(SocketMarks { mark_1, mark_2 }: SocketMarks) -> Self {
        let into_option_u32 = |opt| match opt {
            OptionalUint32::Unset(fposix_socket::Empty) => None,
            OptionalUint32::Value(val) => Some(val),
        };
        Self {
            mark_1: into_option_u32(mark_1),
            mark_2: into_option_u32(mark_2),
            __source_breaking: fidl::marker::SourceBreaking,
        }
    }
}

impl SocketMarks {
    fn has_value(&self) -> bool {
        match (self.mark_1, self.mark_2) {
            (OptionalUint32::Value(_), _) => true,
            (_, OptionalUint32::Value(_)) => true,
            _ => false,
        }
    }
}

impl Default for SocketMarks {
    fn default() -> Self {
        Self {
            mark_1: OptionalUint32::Unset(fposix_socket::Empty),
            mark_2: OptionalUint32::Unset(fposix_socket::Empty),
        }
    }
}

impl Unit for SocketMarks {
    type Data = fuchsia_inspect::Node;

    fn inspect_create(&self, parent: &fuchsia_inspect::Node, name: impl AsRef<str>) -> Self::Data {
        let mut node = parent.create_child(name.as_ref());
        self.inspect_update(&mut node);
        node
    }

    fn inspect_update(&self, data: &mut Self::Data) {
        data.atomic_update(|node| {
            node.clear_recorded();
            let fnet::Marks { mark_1, mark_2, __source_breaking } = fnet::Marks::from(*self);
            if let Some(mark_1) = mark_1 {
                node.record_uint("mark_1", mark_1.into());
            }
            if let Some(mark_2) = mark_2 {
                node.record_uint("mark_2", mark_2.into());
            }
            node.record_bool("has_mark", self.has_value());
        });
    }
}

impl std::fmt::Display for SocketMarks {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.mark_1 {
            fposix_socket::OptionalUint32::Value(v) => write!(f, "{v}"),
            fposix_socket::OptionalUint32::Unset(fposix_socket::Empty) => write!(f, "None"),
        }
    }
}

#[derive(Inspect)]
struct SocketProxy {
    marks: Arc<Mutex<IValue<SocketMarks>>>,
    socket_provider: socket_provider::SocketProvider,
}

impl SocketProxy {
    fn new() -> Self {
        let marks = Arc::new(Mutex::new(IValue::new(SocketMarks::default())));
        Self { marks: marks.clone(), socket_provider: socket_provider::SocketProvider::new(marks) }
    }

    async fn set_marks(&self, marks: SocketMarks) {
        self.marks.lock().await.iset(marks);
    }
}

enum IncomingService {
    PosixSocket(fidl_fuchsia_posix_socket::ProviderRequestStream),
    PosixSocketRaw(fidl_fuchsia_posix_socket_raw::ProviderRequestStream),
}

/// Main entry point for the network socket proxy.
pub async fn run() -> Result<(), anyhow::Error> {
    fuchsia_inspect::component::health().set_starting_up();

    let inspector = fuchsia_inspect::component::inspector();
    let _inspect_server_task =
        inspect_runtime::publish(inspector, inspect_runtime::PublishOptions::default());

    let proxy = Arc::new(SocketProxy::new().with_inspect(inspector.root(), "root")?);

    let mut fs = ServiceFs::new_local();
    let _: &mut ServiceFsDir<'_, _> = fs
        .dir("svc")
        .add_fidl_service(IncomingService::PosixSocket)
        .add_fidl_service(IncomingService::PosixSocketRaw);

    let _: &mut ServiceFs<_> = fs.take_and_serve_directory_handle()?;

    fuchsia_inspect::component::health().set_ok();

    let proxy_for_service = Arc::clone(&proxy);
    let service_fut = fs.for_each_concurrent(100, move |service| {
        let proxy = Arc::clone(&proxy_for_service);
        async move {
            match service {
                IncomingService::PosixSocket(stream) => proxy.socket_provider.run(stream).await,
                IncomingService::PosixSocketRaw(stream) => {
                    proxy.socket_provider.run_raw(stream).await
                }
            }
            .unwrap_or_else(|e| error!("{e:?}"))
        }
    });

    let scope = fasync::Scope::new();

    let proxy_clone = Arc::clone(&proxy);
    let _ = scope.spawn_local(async move {
        mark_watcher::watch_properties(proxy_clone).await;
    });

    let _ = scope.spawn_local(async move {
        service_fut.await;
        error!("The main services future has terminated. It should never terminate");
        // Abort the scope to signal that the main service loop has unexpectedly ended.
        fasync::Scope::current().abort().await;
    });

    scope.join().await;

    Ok(())
}
#[cfg(test)]
mod test {
    use super::*;
    use diagnostics_assertions::assert_data_tree;

    const TEST_MARK_1: u32 = 42;
    const TEST_MARK_2: u32 = 99;

    #[fuchsia::test]
    async fn test_socket_proxy_set_marks() {
        let inspector = fuchsia_inspect::Inspector::default();
        let proxy = SocketProxy::new().with_inspect(inspector.root(), "root").expect("attach");

        assert_data_tree!(inspector, root: contains {
            marks: {
                has_mark: false,
            },
        });

        let marks = SocketMarks {
            mark_1: OptionalUint32::Value(TEST_MARK_1),
            mark_2: OptionalUint32::Unset(fposix_socket::Empty),
        };
        proxy.set_marks(marks).await;
        assert_eq!(**proxy.marks.lock().await, marks);
        assert_data_tree!(inspector, root: contains {
            marks: {
                mark_1: u64::from(TEST_MARK_1),
                has_mark: true,
            },
        });

        let both_marks = SocketMarks {
            mark_1: OptionalUint32::Value(TEST_MARK_1),
            mark_2: OptionalUint32::Value(TEST_MARK_2),
        };
        proxy.set_marks(both_marks).await;
        assert_eq!(**proxy.marks.lock().await, both_marks);
        assert_data_tree!(inspector, root: contains {
            marks: {
                mark_1: u64::from(TEST_MARK_1),
                mark_2: u64::from(TEST_MARK_2),
                has_mark: true,
            },
        });

        proxy.set_marks(SocketMarks::default()).await;
        assert_eq!(**proxy.marks.lock().await, SocketMarks::default());
        assert_data_tree!(inspector, root: contains {
            marks: {
                has_mark: false,
            },
        });
    }
}
