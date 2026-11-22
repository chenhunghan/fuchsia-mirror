// Copyright 2019 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tools for starting or connecting to existing Fuchsia applications and services.

#![deny(missing_docs)]

pub mod fidl_next;

use anyhow::{Context as _, Error, format_err};
use fidl::endpoints::{
    DiscoverableProtocolMarker, FromClient, MemberOpener, ProtocolMarker, ServiceMarker,
    ServiceProxy,
};
use fidl_fuchsia_component::{RealmMarker, RealmProxy};
use fidl_fuchsia_component_decl::ChildRef;
use fidl_fuchsia_io as fio;
use fuchsia_component_directory::{AsRefDirectory, open_directory_async};
use fuchsia_fs::directory::{WatchEvent, Watcher};
use futures::channel::oneshot;
use futures::stream::FusedStream;
use futures::{Stream, StreamExt};
use pin_project::pin_project;
use std::borrow::Borrow;
use std::marker::PhantomData;
use std::task::Poll;

/// Trait that for types that can be returned when you connect to protocols.
pub trait Connect: Sized + FromClient<Protocol: DiscoverableProtocolMarker> {
    /// Connect to a FIDL protocol in the `/svc` directory of the application's root namespace.
    fn connect() -> Result<Self, Error> {
        Self::connect_at(SVC_DIR)
    }

    /// Connect to a FIDL protocol using the provided namespace prefix.
    fn connect_at(service_prefix: impl AsRef<str>) -> Result<Self, Error> {
        let (client, server_end) = fidl::endpoints::create_endpoints::<Self::Protocol>();
        let () = connect_channel_to_protocol_at::<Self::Protocol>(
            server_end.into_channel(),
            service_prefix.as_ref(),
        )?;
        Ok(Self::from_client(client))
    }

    /// Connect to an instance of a FIDL protocol hosted in `directory`, in the `/svc/` subdir.
    fn connect_at_dir_svc(directory: &impl AsRefDirectory) -> Result<Self, Error> {
        let protocol_path = format!("{}/{}", SVC_DIR, Self::Protocol::PROTOCOL_NAME);
        Self::connect_at_dir_root_with_name(directory, &protocol_path)
    }

    /// Connect to an instance of a FIDL protocol hosted in `directory`.
    fn connect_at_dir_root(directory: &impl AsRefDirectory) -> Result<Self, Error> {
        Self::connect_at_dir_root_with_name(directory, Self::Protocol::PROTOCOL_NAME)
    }

    /// Connect to an instance of a FIDL protocol hosted in `directory` using the given `filename`.
    fn connect_at_dir_root_with_name(
        directory: &impl AsRefDirectory,
        filename: &str,
    ) -> Result<Self, Error> {
        let (client, server) = fidl::endpoints::create_endpoints::<Self::Protocol>();
        directory.as_ref_directory().open(
            filename,
            fio::Flags::PROTOCOL_SERVICE,
            server.into_channel().into(),
        )?;
        Ok(Self::from_client(client))
    }
}

impl<T: FromClient<Protocol: DiscoverableProtocolMarker>> Connect for T {}

/// The connect module provides connect methods where the generic argument is the return type rather
/// than the marker type. This allows them to work for different return types; the same method can
/// be used for asynchronous proxies, synchronous proxies, client ends, etc.  This also makes it
/// possible to elide the type where the compiler can infer the type, which is not the case when
/// using the marker types.
pub mod connect {
    use super::*;

    /// Connect to a FIDL protocol in the `/svc` directory of the application's root namespace.
    pub fn connect_to_protocol<T: Connect>() -> Result<T, Error> {
        T::connect()
    }

    /// Connect to a FIDL protocol using the provided namespace prefix.
    pub fn connect_to_protocol_at<T: Connect>(service_prefix: impl AsRef<str>) -> Result<T, Error> {
        T::connect_at(service_prefix)
    }

    /// Connect to an instance of a FIDL protocol hosted in `directory`, in the `/svc/` subdir.
    pub fn connect_to_protocol_at_dir_svc<T: Connect>(
        directory: &impl AsRefDirectory,
    ) -> Result<T, Error> {
        T::connect_at_dir_svc(directory)
    }

    /// Connect to an instance of a FIDL protocol hosted in `directory`.
    pub fn connect_to_protocol_at_dir_root<T: Connect>(
        directory: &impl AsRefDirectory,
    ) -> Result<T, Error> {
        T::connect_at_dir_root(directory)
    }

    /// Connect to an instance of a FIDL protocol hosted in `directory` using the given `filename`.
    pub fn connect_to_named_protocol_at_dir_root<T: Connect>(
        directory: &impl AsRefDirectory,
        filename: &str,
    ) -> Result<T, Error> {
        T::connect_at_dir_root_with_name(directory, filename)
    }
}

/// Path to the service directory in an application's root namespace.
pub const SVC_DIR: &'static str = "/svc";

/// A protocol connection request that allows checking if the protocol exists.
pub struct ProtocolConnector<D: Borrow<fio::DirectoryProxy>, P: DiscoverableProtocolMarker> {
    svc_dir: D,
    _svc_marker: PhantomData<P>,
}

impl<D: Borrow<fio::DirectoryProxy>, P: DiscoverableProtocolMarker> ProtocolConnector<D, P> {
    /// Returns a new `ProtocolConnector` to `P` in the specified service directory.
    fn new(svc_dir: D) -> ProtocolConnector<D, P> {
        ProtocolConnector { svc_dir, _svc_marker: PhantomData }
    }

    /// Returns `true` if the protocol exists in the service directory.
    ///
    /// This method requires a round trip to the service directory to check for
    /// existence.
    pub async fn exists(&self) -> Result<bool, Error> {
        match fuchsia_fs::directory::dir_contains(self.svc_dir.borrow(), P::PROTOCOL_NAME).await {
            Ok(v) => Ok(v),
            // If the service directory is unavailable, then mask the error as if
            // the protocol does not exist.
            Err(fuchsia_fs::directory::EnumerateError::Fidl(
                _,
                fidl::Error::ClientChannelClosed { epitaph, .. },
            )) if epitaph == zx::Status::PEER_CLOSED => Ok(false),
            Err(e) => Err(Error::new(e).context("error checking for service entry in directory")),
        }
    }

    /// Connect to the FIDL protocol using the provided server-end.
    ///
    /// Note, this method does not check if the protocol exists. It is up to the
    /// caller to call `exists` to check for existence.
    pub fn connect_with(self, server_end: zx::Channel) -> Result<(), Error> {
        #[cfg(fuchsia_api_level_at_least = "27")]
        return self
            .svc_dir
            .borrow()
            .open(
                P::PROTOCOL_NAME,
                fio::Flags::PROTOCOL_SERVICE,
                &fio::Options::default(),
                server_end.into(),
            )
            .context("error connecting to protocol");
        #[cfg(not(fuchsia_api_level_at_least = "27"))]
        return self
            .svc_dir
            .borrow()
            .open3(
                P::PROTOCOL_NAME,
                fio::Flags::PROTOCOL_SERVICE,
                &fio::Options::default(),
                server_end.into(),
            )
            .context("error connecting to protocol");
    }

    /// Connect to the FIDL protocol.
    ///
    /// Note, this method does not check if the protocol exists. It is up to the
    /// caller to call `exists` to check for existence.
    pub fn connect(self) -> Result<P::Proxy, Error> {
        let (proxy, server_end) = fidl::endpoints::create_proxy::<P>();
        let () = self
            .connect_with(server_end.into_channel())
            .context("error connecting with server channel")?;
        Ok(proxy)
    }
}

/// Clone the handle to the service directory in the application's root namespace.
pub fn clone_namespace_svc() -> Result<fio::DirectoryProxy, Error> {
    fuchsia_fs::directory::open_in_namespace(SVC_DIR, fio::PERM_READABLE)
        .context("error opening svc directory")
}

/// Return a FIDL protocol connector at the default service directory in the
/// application's root namespace.
pub fn new_protocol_connector<P: DiscoverableProtocolMarker>()
-> Result<ProtocolConnector<fio::DirectoryProxy, P>, Error> {
    new_protocol_connector_at::<P>(SVC_DIR)
}

/// Return a FIDL protocol connector at the specified service directory in the
/// application's root namespace.
///
/// The service directory path must be an absolute path.
pub fn new_protocol_connector_at<P: DiscoverableProtocolMarker>(
    service_directory_path: &str,
) -> Result<ProtocolConnector<fio::DirectoryProxy, P>, Error> {
    let dir = fuchsia_fs::directory::open_in_namespace(service_directory_path, fio::PERM_READABLE)
        .context("error opening service directory")?;

    Ok(ProtocolConnector::new(dir))
}

/// Return a FIDL protocol connector at the specified service directory.
pub fn new_protocol_connector_in_dir<P: DiscoverableProtocolMarker>(
    dir: &fio::DirectoryProxy,
) -> ProtocolConnector<&fio::DirectoryProxy, P> {
    ProtocolConnector::new(dir)
}

/// Connect to a FIDL protocol using the provided channel.
pub fn connect_channel_to_protocol<P: DiscoverableProtocolMarker>(
    server_end: zx::Channel,
) -> Result<(), Error> {
    connect_channel_to_protocol_at::<P>(server_end, SVC_DIR)
}

/// Connect to a FIDL protocol using the provided channel and namespace prefix.
pub fn connect_channel_to_protocol_at<P: DiscoverableProtocolMarker>(
    server_end: zx::Channel,
    service_directory_path: &str,
) -> Result<(), Error> {
    let protocol_path = format!("{}/{}", service_directory_path, P::PROTOCOL_NAME);
    connect_channel_to_protocol_at_path(server_end, &protocol_path)
}

/// Connect to a FIDL protocol using the provided channel and namespace path.
pub fn connect_channel_to_protocol_at_path(
    server_end: zx::Channel,
    protocol_path: &str,
) -> Result<(), Error> {
    fdio::service_connect(&protocol_path, server_end)
        .with_context(|| format!("Error connecting to protocol path: {}", protocol_path))
}

/// Connect to a FIDL protocol in the `/svc` directory of the application's root namespace.
pub fn connect_to_protocol<P: DiscoverableProtocolMarker>() -> Result<P::Proxy, Error> {
    connect_to_protocol_at::<P>(SVC_DIR)
}

/// Connect to a FIDL protocol using the application root namespace, returning a synchronous proxy.
///
/// Note: while this function returns a synchronous thread-blocking proxy it does not block until
/// the connection is complete. The proxy must be used to discover whether the connection was
/// successful.
pub fn connect_to_protocol_sync<P: DiscoverableProtocolMarker>()
-> Result<P::SynchronousProxy, Error> {
    connect_to_protocol_sync_at::<P>(SVC_DIR)
}

/// Connect to a FIDL protocol using the provided namespace prefix.
pub fn connect_to_protocol_at<P: DiscoverableProtocolMarker>(
    service_prefix: impl AsRef<str>,
) -> Result<P::Proxy, Error> {
    let (proxy, server_end) = fidl::endpoints::create_proxy::<P>();
    let () =
        connect_channel_to_protocol_at::<P>(server_end.into_channel(), service_prefix.as_ref())?;
    Ok(proxy)
}

/// Connect to a FIDL protocol using the provided namespace prefix, returning a synchronous proxy.
///
/// Note: while this function returns a synchronous thread-blocking proxy it does not block until
/// the connection is complete. The proxy must be used to discover whether the connection was
/// successful.
pub fn connect_to_protocol_sync_at<P: DiscoverableProtocolMarker>(
    service_prefix: impl AsRef<str>,
) -> Result<P::SynchronousProxy, Error> {
    let (proxy, server_end) = fidl::endpoints::create_sync_proxy::<P>();
    let () =
        connect_channel_to_protocol_at::<P>(server_end.into_channel(), service_prefix.as_ref())?;
    Ok(proxy)
}

/// Connect to a FIDL protocol using the provided path.
pub fn connect_to_protocol_at_path<P: ProtocolMarker>(
    protocol_path: impl AsRef<str>,
) -> Result<P::Proxy, Error> {
    let (proxy, server_end) = fidl::endpoints::create_proxy::<P>();
    let () =
        connect_channel_to_protocol_at_path(server_end.into_channel(), protocol_path.as_ref())?;
    Ok(proxy)
}

/// Connect to an instance of a FIDL protocol hosted in `directory`.
pub fn connect_to_protocol_at_dir_root<P: DiscoverableProtocolMarker>(
    directory: &impl AsRefDirectory,
) -> Result<P::Proxy, Error> {
    connect_to_named_protocol_at_dir_root::<P>(directory, P::PROTOCOL_NAME)
}

/// Connect to an instance of a FIDL protocol hosted in `directory` using the given `filename`.
pub fn connect_to_named_protocol_at_dir_root<P: ProtocolMarker>(
    directory: &impl AsRefDirectory,
    filename: &str,
) -> Result<P::Proxy, Error> {
    let (proxy, server_end) = fidl::endpoints::create_proxy::<P>();
    directory.as_ref_directory().open(
        filename,
        fio::Flags::PROTOCOL_SERVICE,
        server_end.into_channel().into(),
    )?;
    Ok(proxy)
}

/// Connect to an instance of a FIDL protocol hosted in `directory`, in the `/svc/` subdir.
pub fn connect_to_protocol_at_dir_svc<P: DiscoverableProtocolMarker>(
    directory: &impl AsRefDirectory,
) -> Result<P::Proxy, Error> {
    let protocol_path = format!("{}/{}", SVC_DIR, P::PROTOCOL_NAME);
    connect_to_named_protocol_at_dir_root::<P>(directory, &protocol_path)
}

/// This wraps an instance directory for a service capability and provides the MemberOpener trait
/// for it. This can be boxed and used with a |ServiceProxy::from_member_opener|.
pub struct ServiceInstanceDirectory(pub fio::DirectoryProxy, pub String);

impl MemberOpener for ServiceInstanceDirectory {
    fn open_member(&self, member: &str, server_end: zx::Channel) -> Result<(), fidl::Error> {
        let Self(directory, _) = self;
        #[cfg(fuchsia_api_level_at_least = "27")]
        return directory.open(
            member,
            fio::Flags::PROTOCOL_SERVICE,
            &fio::Options::default(),
            server_end,
        );
        #[cfg(not(fuchsia_api_level_at_least = "27"))]
        return directory.open3(
            member,
            fio::Flags::PROTOCOL_SERVICE,
            &fio::Options::default(),
            server_end,
        );
    }
    fn instance_name(&self) -> &str {
        let Self(_, instance_name) = self;
        return &instance_name;
    }
}

/// An instance of an aggregated fidl service that has been enumerated by [`Service::watch`],
/// [`Service::watch_for_any`], or [`Service::enumerate`].
struct ServiceInstance {
    /// The name of the service instance within the service directory.
    name: String,
    dir: fio::DirectoryProxy,
}

impl MemberOpener for ServiceInstance {
    fn open_member(&self, member: &str, server_end: zx::Channel) -> Result<(), fidl::Error> {
        connect_to_instance_member_with_channel(&self.dir, &self.name, member, server_end)
    }
    fn instance_name(&self) -> &str {
        &self.name
    }
}

fn connect_to_instance_member_with_channel(
    dir: &fio::DirectoryProxy,
    instance: &str,
    member: &str,
    server_end: zx::Channel,
) -> Result<(), fidl::Error> {
    let path = format!("{instance}/{member}");
    #[cfg(fuchsia_api_level_at_least = "27")]
    return dir.open(&path, fio::Flags::PROTOCOL_SERVICE, &fio::Options::default(), server_end);
    #[cfg(not(fuchsia_api_level_at_least = "27"))]
    return dir.open3(&path, fio::Flags::PROTOCOL_SERVICE, &fio::Options::default(), server_end);
}

/// A service from an incoming namespace's `/svc` directory.
pub struct Service<S> {
    dir: fio::DirectoryProxy,
    _marker: S,
}

impl<S: Clone> Clone for Service<S> {
    fn clone(&self) -> Self {
        Self { dir: Clone::clone(&self.dir), _marker: self._marker.clone() }
    }
}

/// Returns a new [`Service`] that waits for instances to appear in
/// the given service directory, probably opened by [`open_service`]
impl<S> From<fio::DirectoryProxy> for Service<S>
where
    S: Default,
{
    fn from(dir: fio::DirectoryProxy) -> Self {
        Self { dir, _marker: S::default() }
    }
}

impl<S: ServiceMarker> Service<S> {
    /// Returns a new [`Service`] that waits for instances to appear in
    /// the given service directory, probably opened by [`open_service`]
    pub fn from_service_dir_proxy(dir: fio::DirectoryProxy, _marker: S) -> Self {
        Self { dir, _marker }
    }

    /// Returns a new [`Service`] from the process's incoming service namespace.
    pub fn open(marker: S) -> Result<Self, Error> {
        Ok(Self::from_service_dir_proxy(open_service::<S>()?, marker))
    }

    /// Returns a new [`Service`] from the process's incoming service namespace.
    pub fn open_at(service_name: impl AsRef<str>, marker: S) -> Result<Self, Error> {
        Ok(Self::from_service_dir_proxy(open_service_at(service_name)?, marker))
    }

    /// Returns a new [`Service`] that is in the given directory.
    pub fn open_from_dir(svc_dir: impl AsRefDirectory, marker: S) -> Result<Self, Error> {
        let dir = open_directory_async(&svc_dir, S::SERVICE_NAME, fio::R_STAR_DIR)?;
        Ok(Self::from_service_dir_proxy(dir, marker))
    }

    /// Returns a new [`Service`] that is in the given directory under the given prefix
    /// (as "{prefix}/ServiceName"). A common case would be passing [`SVC_DIR`] as the prefix.
    pub fn open_from_dir_prefix(
        dir: impl AsRefDirectory,
        prefix: impl AsRef<str>,
        marker: S,
    ) -> Result<Self, Error> {
        let prefix = prefix.as_ref();
        let service_path = format!("{prefix}/{}", S::SERVICE_NAME);
        // TODO(https://fxbug.dev/42068248): Some Directory implementations require relative paths,
        // even though they aren't technically supposed to, so strip the leading slash until that's
        // resolved one way or the other.
        let service_path = service_path.strip_prefix('/').unwrap_or_else(|| service_path.as_ref());
        let dir = open_directory_async(&dir, &service_path, fio::R_STAR_DIR)?;
        Ok(Self::from_service_dir_proxy(dir, marker))
    }

    /// Connects to the named instance without waiting for it to appear. You should only use this
    /// after the instance name has been returned by the [`Self::watch`] stream, or if the
    /// instance is statically routed so component manager will lazily load it.
    pub fn connect_to_instance(&self, name: impl AsRef<str>) -> Result<S::Proxy, Error> {
        let directory_proxy = fuchsia_fs::directory::open_directory_async(
            &self.dir,
            name.as_ref(),
            fio::PERM_READABLE,
        )?;
        Ok(S::Proxy::from_member_opener(Box::new(ServiceInstanceDirectory(
            directory_proxy,
            name.as_ref().to_string(),
        ))))
    }

    /// Returns an async stream of service instances that are enumerated within this service
    /// directory.
    pub async fn watch(self) -> Result<ServiceInstanceStream<S>, Error> {
        let state = watch_service_dir(self.dir).await?;
        Ok(ServiceInstanceStream {
            lifecycle: StreamLifecycle::Active(state),
            _marker: self._marker,
        })
    }

    /// Asynchronously returns the first service instance available within this service directory.
    pub async fn watch_for_any(self) -> Result<S::Proxy, Error> {
        self.watch()
            .await?
            .next()
            .await
            .context("No instances found before service directory was removed")?
    }

    /// Returns an list of all instances that are currently available.
    pub async fn enumerate(self) -> Result<Vec<S::Proxy>, Error> {
        let instances: Vec<S::Proxy> = enumerate_service_dir(self.dir)
            .await?
            .into_iter()
            .map(S::Proxy::from_member_opener)
            .collect();
        Ok(instances)
    }
}

enum IdleSignal {
    Unset,
    SeenIdle,
    Pending(oneshot::Sender<()>),
    Signaled,
}

impl IdleSignal {
    fn was_set(&self) -> bool {
        matches!(self, Self::Pending(_) | Self::Signaled)
    }
}

struct ServiceInstanceStreamState {
    dir: fio::DirectoryProxy,
    watcher: Watcher,
    idle_signal: IdleSignal,
}

enum StreamLifecycle {
    Active(ServiceInstanceStreamState),
    Terminated { had_idle_signal: bool },
}

async fn watch_service_dir(dir: fio::DirectoryProxy) -> Result<ServiceInstanceStreamState, Error> {
    let watcher = Watcher::new(&dir).await?;
    Ok(ServiceInstanceStreamState { dir, watcher, idle_signal: IdleSignal::Unset })
}

async fn enumerate_service_dir(
    dir: fio::DirectoryProxy,
) -> Result<Vec<Box<dyn MemberOpener>>, Error> {
    let entries = fuchsia_fs::directory::readdir(&dir).await?;
    Ok(entries
        .into_iter()
        .map(|dirent| -> Box<dyn MemberOpener> {
            Box::new(ServiceInstance { dir: Clone::clone(&dir), name: dirent.name })
        })
        .collect())
}

#[cold]
#[inline(never)]
#[track_caller]
fn panic_idle_sender_already_set() -> ! {
    panic!("ServiceInstanceStream::with_idle_signal called more than once");
}

#[track_caller]
fn register_idle_signal(lifecycle: &mut StreamLifecycle, idle_sender: oneshot::Sender<()>) {
    match lifecycle {
        StreamLifecycle::Active(state) => match &state.idle_signal {
            IdleSignal::Unset => {
                state.idle_signal = IdleSignal::Pending(idle_sender);
            }
            IdleSignal::SeenIdle => {
                state.idle_signal = IdleSignal::Signaled;
                let _ = idle_sender.send(());
            }
            IdleSignal::Pending(_) | IdleSignal::Signaled => {
                panic_idle_sender_already_set();
            }
        },
        StreamLifecycle::Terminated { had_idle_signal } => {
            if *had_idle_signal {
                panic_idle_sender_already_set();
            }
            *had_idle_signal = true;
        }
    }
}

fn poll_next_service_instance(
    lifecycle: &mut StreamLifecycle,
    cx: &mut std::task::Context<'_>,
) -> Poll<Option<Result<Box<dyn MemberOpener>, Error>>> {
    use Poll::*;
    let StreamLifecycle::Active(state) = lifecycle else {
        return Ready(None);
    };
    // poll the inner watcher until we either find something worth returning or it
    // returns Pending.
    while let Ready(next) = state.watcher.poll_next_unpin(cx) {
        match next {
            Some(Ok(msg)) => match msg.event {
                WatchEvent::DELETED => {
                    let had_idle_signal = state.idle_signal.was_set();
                    *lifecycle = StreamLifecycle::Terminated { had_idle_signal };
                    return Ready(None);
                }
                WatchEvent::ADD_FILE | WatchEvent::EXISTING => {
                    let filename = msg.filename.to_str().unwrap();
                    if filename != "." {
                        let opener: Box<dyn MemberOpener> = Box::new(ServiceInstance {
                            dir: Clone::clone(&state.dir),
                            name: filename.to_owned(),
                        });
                        return Ready(Some(Ok(opener)));
                    }
                }
                WatchEvent::IDLE => {
                    match std::mem::replace(&mut state.idle_signal, IdleSignal::SeenIdle) {
                        IdleSignal::Pending(sender) => {
                            state.idle_signal = IdleSignal::Signaled;
                            let _ = sender.send(());
                        }
                        IdleSignal::Signaled => {
                            state.idle_signal = IdleSignal::Signaled;
                        }
                        IdleSignal::Unset | IdleSignal::SeenIdle => {}
                    }
                }
                WatchEvent::REMOVE_FILE => {}
            },
            Some(Err(err)) => {
                let had_idle_signal = state.idle_signal.was_set();
                *lifecycle = StreamLifecycle::Terminated { had_idle_signal };
                return Ready(Some(Err(err.into())));
            }
            None => {
                let had_idle_signal = state.idle_signal.was_set();
                *lifecycle = StreamLifecycle::Terminated { had_idle_signal };
                return Ready(None);
            }
        }
    }
    Pending
}

/// A stream iterator for a service directory that produces one item for every service instance
/// that is added to it as they are added. Returned from [`Service::watch`]
///
/// Normally, this stream will only terminate if the service directory being watched is removed, so
/// the client must decide when it has found all the instances it's looking for.
#[pin_project]
pub struct ServiceInstanceStream<S: Clone> {
    lifecycle: StreamLifecycle,
    _marker: S,
}

impl<S: Clone> ServiceInstanceStream<S> {
    /// Registers a oneshot sender that is signaled once all pre-existing service instances have
    /// been yielded by this stream (when the underlying directory watcher emits
    /// `WatchEvent::IDLE`).
    ///
    /// Because `poll_next` yields each `EXISTING` instance before polling the directory watcher
    /// further, `idle_sender` is only signaled on the poll after the last pre-existing instance
    /// has been yielded (or on the first poll if the service directory was initially empty). If
    /// `WatchEvent::IDLE` has already been observed on this stream, `idle_sender` is signaled
    /// immediately. If the stream terminates or errors before `WatchEvent::IDLE` is reached,
    /// `idle_sender` is dropped immediately so any receiver resolves with
    /// `Err(oneshot::Canceled)` even if this `ServiceInstanceStream` value remains alive.
    ///
    /// # Panics
    ///
    /// Panics if `with_idle_signal` has already been called on this stream.
    #[track_caller]
    pub fn with_idle_signal(mut self, idle_sender: oneshot::Sender<()>) -> Self {
        register_idle_signal(&mut self.lifecycle, idle_sender);
        self
    }
}

impl<S: ServiceMarker> Stream for ServiceInstanceStream<S> {
    type Item = Result<S::Proxy, Error>;

    fn poll_next(
        self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> Poll<Option<Self::Item>> {
        let this = self.project();
        poll_next_service_instance(this.lifecycle, cx).map_ok(S::Proxy::from_member_opener)
    }
}

impl<S: ServiceMarker> FusedStream for ServiceInstanceStream<S> {
    fn is_terminated(&self) -> bool {
        matches!(self.lifecycle, StreamLifecycle::Terminated { .. })
    }
}

/// Connect to an instance of a FIDL service in the `/svc` directory of
/// the application's root namespace.
/// `instance` is a path of one or more components.
// NOTE: We would like to use impl AsRef<T> to accept a wide variety of string-like
// inputs but Rust limits specifying explicit generic parameters when `impl-traits`
// are present.
pub fn connect_to_service_instance<S: ServiceMarker>(instance: &str) -> Result<S::Proxy, Error> {
    let service_path = format!("{}/{}/{}", SVC_DIR, S::SERVICE_NAME, instance);
    let directory_proxy =
        fuchsia_fs::directory::open_in_namespace(&service_path, fio::PERM_READABLE)?;
    Ok(S::Proxy::from_member_opener(Box::new(ServiceInstanceDirectory(
        directory_proxy,
        instance.to_string(),
    ))))
}

/// Connect to a named instance of a FIDL service hosted in the service subdirectory under the
/// directory protocol channel `directory`
// NOTE: We would like to use impl AsRef<T> to accept a wide variety of string-like
// inputs but Rust limits specifying explicit generic parameters when `impl-traits`
// are present.
pub fn connect_to_service_instance_at_dir<S: ServiceMarker>(
    directory: &fio::DirectoryProxy,
    instance: &str,
) -> Result<S::Proxy, Error> {
    let service_path = format!("{}/{}", S::SERVICE_NAME, instance);
    let directory_proxy =
        fuchsia_fs::directory::open_directory_async(directory, &service_path, fio::PERM_READABLE)?;
    Ok(S::Proxy::from_member_opener(Box::new(ServiceInstanceDirectory(
        directory_proxy,
        instance.to_string(),
    ))))
}

/// Connect to an instance of a FIDL service hosted in `directory`, in the `svc/` subdir.
pub fn connect_to_service_instance_at_dir_svc<S: ServiceMarker>(
    directory: &impl AsRefDirectory,
    instance: impl AsRef<str>,
) -> Result<S::Proxy, Error> {
    let service_path = format!("{SVC_DIR}/{}/{}", S::SERVICE_NAME, instance.as_ref());
    // TODO(https://fxbug.dev/42068248): Some Directory implementations require relative paths,
    // even though they aren't technically supposed to, so strip the leading slash until that's
    // resolved one way or the other.
    let service_path = service_path.strip_prefix('/').unwrap();
    let directory_proxy = open_directory_async(directory, service_path, fio::R_STAR_DIR)?;
    Ok(S::Proxy::from_member_opener(Box::new(ServiceInstanceDirectory(
        directory_proxy,
        instance.as_ref().to_string(),
    ))))
}

/// Opens a FIDL service as a directory, which holds instances of the service.
pub fn open_service<S: ServiceMarker>() -> Result<fio::DirectoryProxy, Error> {
    let service_path = format!("{}/{}", SVC_DIR, S::SERVICE_NAME);
    fuchsia_fs::directory::open_in_namespace(&service_path, fio::PERM_READABLE)
        .context("namespace open failed")
}

/// Opens a FIDL service with a custom name as a directory, which holds instances of the service.
pub fn open_service_at(service_name: impl AsRef<str>) -> Result<fio::DirectoryProxy, Error> {
    let service_path = format!("{SVC_DIR}/{}", service_name.as_ref());
    fuchsia_fs::directory::open_in_namespace(&service_path, fio::PERM_READABLE)
        .context("namespace open failed")
}

/// Opens the exposed directory from a child. Only works in CFv2, and only works if this component
/// uses `fuchsia.component.Realm`.
pub async fn open_childs_exposed_directory(
    child_name: impl Into<String>,
    collection_name: Option<String>,
) -> Result<fio::DirectoryProxy, Error> {
    let realm_proxy = connect_to_protocol::<RealmMarker>()?;
    let (directory_proxy, server_end) = fidl::endpoints::create_proxy::<fio::DirectoryMarker>();
    let child_ref = ChildRef { name: child_name.into(), collection: collection_name };
    realm_proxy.open_exposed_dir(&child_ref, server_end).await?.map_err(|e| {
        let ChildRef { name, collection } = child_ref;
        format_err!("failed to bind to child {} in collection {:?}: {:?}", name, collection, e)
    })?;
    Ok(directory_proxy)
}

/// Connects to a FIDL protocol exposed by a child that's within the `/svc` directory. Only works in
/// CFv2, and only works if this component uses `fuchsia.component.Realm`.
pub async fn connect_to_childs_protocol<P: DiscoverableProtocolMarker>(
    child_name: String,
    collection_name: Option<String>,
) -> Result<P::Proxy, Error> {
    let child_exposed_directory =
        open_childs_exposed_directory(child_name, collection_name).await?;
    connect_to_protocol_at_dir_root::<P>(&child_exposed_directory)
}

/// Returns a connection to the Realm protocol. Components v2 only.
pub fn realm() -> Result<RealmProxy, Error> {
    connect_to_protocol::<RealmMarker>()
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;
    use std::sync::Arc;

    use super::*;
    use fidl::endpoints::ServiceMarker as _;
    use fidl_fuchsia_component_client_test::{
        ProtocolAMarker, ProtocolAProxy, ProtocolBMarker, ProtocolBProxy, ServiceMarker,
    };
    use fuchsia_async::{self as fasync};
    use futures::{TryStreamExt, future};
    use vfs::directory::simple::Simple;
    use vfs::file::vmo::read_only;
    use vfs::pseudo_directory;

    #[fasync::run_singlethreaded(test)]
    async fn test_svc_connector_svc_does_not_exist() -> Result<(), Error> {
        let req = new_protocol_connector::<ProtocolAMarker>().context("error probing service")?;
        assert_matches::assert_matches!(
            req.exists().await.context("error checking service"),
            Ok(false)
        );
        let _: ProtocolAProxy = req.connect().context("error connecting to service")?;

        let req = new_protocol_connector_at::<ProtocolAMarker>(SVC_DIR)
            .context("error probing service at svc dir")?;
        assert_matches::assert_matches!(
            req.exists().await.context("error checking service at svc dir"),
            Ok(false)
        );
        let _: ProtocolAProxy = req.connect().context("error connecting to service at svc dir")?;

        Ok(())
    }

    #[fasync::run_singlethreaded(test)]
    async fn test_svc_connector_connect_with_dir() -> Result<(), Error> {
        let dir = pseudo_directory! {
            ProtocolBMarker::PROTOCOL_NAME => read_only("read_only"),
        };
        let dir_proxy =
            vfs::directory::serve_read_only(dir, vfs::execution_scope::ExecutionScope::new());
        let req = new_protocol_connector_in_dir::<ProtocolAMarker>(&dir_proxy);
        assert_matches::assert_matches!(
            req.exists().await.context("error probing invalid service"),
            Ok(false)
        );
        let _: ProtocolAProxy = req.connect().context("error connecting to invalid service")?;

        let req = new_protocol_connector_in_dir::<ProtocolBMarker>(&dir_proxy);
        assert_matches::assert_matches!(
            req.exists().await.context("error probing service"),
            Ok(true)
        );
        let _: ProtocolBProxy = req.connect().context("error connecting to service")?;

        Ok(())
    }

    fn make_inner_service_instance_tree() -> Arc<Simple> {
        pseudo_directory! {
            ServiceMarker::SERVICE_NAME => pseudo_directory! {
                "default" => read_only("read_only"),
                "another_instance" => read_only("read_only"),
            },
        }
    }

    fn make_service_instance_tree() -> Arc<Simple> {
        pseudo_directory! {
            "svc" => make_inner_service_instance_tree(),
        }
    }

    #[fasync::run_until_stalled(test)]
    async fn test_service_instance_watcher_from_root() -> Result<(), Error> {
        let dir_proxy = vfs::directory::serve_read_only(
            make_service_instance_tree(),
            vfs::execution_scope::ExecutionScope::new(),
        );
        let watcher = Service::open_from_dir_prefix(&dir_proxy, SVC_DIR, ServiceMarker)?;
        let found_names: HashSet<_> = watcher
            .watch()
            .await?
            .take(2)
            .and_then(|proxy| future::ready(Ok(proxy.instance_name().to_owned())))
            .try_collect()
            .await?;

        assert_eq!(
            found_names,
            HashSet::from_iter(["default".to_owned(), "another_instance".to_owned()])
        );

        Ok(())
    }

    #[fasync::run_until_stalled(test)]
    async fn test_service_instance_watcher_from_svc() -> Result<(), Error> {
        let dir_proxy = vfs::directory::serve_read_only(
            make_inner_service_instance_tree(),
            vfs::execution_scope::ExecutionScope::new(),
        );
        let watcher = Service::open_from_dir(&dir_proxy, ServiceMarker)?;
        let found_names: HashSet<_> = watcher
            .watch()
            .await?
            .take(2)
            .and_then(|proxy| future::ready(Ok(proxy.instance_name().to_owned())))
            .try_collect()
            .await?;

        assert_eq!(
            found_names,
            HashSet::from_iter(["default".to_owned(), "another_instance".to_owned()])
        );

        Ok(())
    }

    #[fasync::run_until_stalled(test)]
    async fn test_connect_to_all_services() -> Result<(), Error> {
        let dir_proxy = vfs::directory::serve_read_only(
            make_service_instance_tree(),
            vfs::execution_scope::ExecutionScope::new(),
        );
        let watcher = Service::open_from_dir_prefix(&dir_proxy, SVC_DIR, ServiceMarker)?;
        let _: Vec<_> = watcher.watch().await?.take(2).try_collect().await?;

        Ok(())
    }

    #[fasync::run_until_stalled(test)]
    async fn test_connect_to_any() -> Result<(), Error> {
        let dir_proxy = vfs::directory::serve_read_only(
            make_service_instance_tree(),
            vfs::execution_scope::ExecutionScope::new(),
        );
        let watcher = Service::open_from_dir_prefix(&dir_proxy, SVC_DIR, ServiceMarker)?;
        let found = watcher.watch_for_any().await?;
        assert!(["default", "another_instance"].contains(&found.instance_name()));

        Ok(())
    }

    #[test]
    fn test_service_instance_watcher_idle_signal_with_existing() -> Result<(), Error> {
        let mut exec = fasync::TestExecutor::new();
        let dir_proxy = vfs::directory::serve_read_only(
            make_service_instance_tree(),
            vfs::execution_scope::ExecutionScope::new(),
        );
        let service = Service::open_from_dir_prefix(&dir_proxy, SVC_DIR, ServiceMarker)?;
        let (idle_tx, mut idle_rx) = oneshot::channel();
        let mut stream = exec.run_singlethreaded(service.watch())?.with_idle_signal(idle_tx);

        assert_matches::assert_matches!(exec.run_until_stalled(&mut idle_rx), Poll::Pending);

        let first = match exec.run_until_stalled(&mut stream.next()) {
            Poll::Ready(Some(res)) => res?,
            _ => panic!("expected first instance"),
        };
        assert_matches::assert_matches!(exec.run_until_stalled(&mut idle_rx), Poll::Pending);

        let second = match exec.run_until_stalled(&mut stream.next()) {
            Poll::Ready(Some(res)) => res?,
            _ => panic!("expected second instance"),
        };
        assert_matches::assert_matches!(exec.run_until_stalled(&mut idle_rx), Poll::Pending);

        assert_eq!(
            HashSet::<_>::from_iter([
                first.instance_name().to_owned(),
                second.instance_name().to_owned()
            ]),
            HashSet::from_iter(["default".to_owned(), "another_instance".to_owned()])
        );

        assert!(exec.run_until_stalled(&mut stream.next()).is_pending());
        assert_matches::assert_matches!(exec.run_until_stalled(&mut idle_rx), Poll::Ready(Ok(())));

        Ok(())
    }

    #[test]
    fn test_service_instance_watcher_idle_signal_when_empty() -> Result<(), Error> {
        let mut exec = fasync::TestExecutor::new();
        let empty_tree = pseudo_directory! {
            "svc" => pseudo_directory! {
                ServiceMarker::SERVICE_NAME => pseudo_directory! {},
            },
        };
        let dir_proxy = vfs::directory::serve_read_only(
            empty_tree,
            vfs::execution_scope::ExecutionScope::new(),
        );
        let service = Service::open_from_dir_prefix(&dir_proxy, SVC_DIR, ServiceMarker)?;
        let (idle_tx, mut idle_rx) = oneshot::channel();
        let mut stream = exec.run_singlethreaded(service.watch())?.with_idle_signal(idle_tx);

        assert!(exec.run_until_stalled(&mut stream.next()).is_pending());
        assert_matches::assert_matches!(exec.run_until_stalled(&mut idle_rx), Poll::Ready(Ok(())));

        Ok(())
    }

    #[test]
    fn test_service_instance_watcher_idle_signal_canceled_on_early_termination() -> Result<(), Error>
    {
        let mut exec = fasync::TestExecutor::new();
        let (dir_proxy, mut dir_stream) =
            fidl::endpoints::create_proxy_and_stream::<fio::DirectoryMarker>();
        let server_task = fasync::Task::local(async move {
            if let Some(Ok(fio::DirectoryRequest::Watch { watcher, responder, .. })) =
                dir_stream.next().await
            {
                // Acknowledge Watch, then immediately drop `watcher` without emitting IDLE.
                drop(watcher);
                responder.send(zx::sys::ZX_OK).unwrap();
            }
        });
        exec.run_singlethreaded(&mut Box::pin(async {
            let service = Service::from_service_dir_proxy(dir_proxy, ServiceMarker);
            let (idle_tx, idle_rx) = oneshot::channel();
            let mut stream = service.watch().await.unwrap().with_idle_signal(idle_tx);
            server_task.await;

            // Stream errors/terminates before IDLE while `stream` is still retained in scope.
            assert!(matches!(stream.next().await, Some(Err(_)) | None));
            assert!(stream.is_terminated());
            assert_matches::assert_matches!(idle_rx.await, Err(oneshot::Canceled));
        }));

        Ok(())
    }

    #[test]
    fn test_service_instance_watcher_idle_signal_registered_after_idle_fires_immediately()
    -> Result<(), Error> {
        let mut exec = fasync::TestExecutor::new();
        let empty_tree = pseudo_directory! {
            "svc" => pseudo_directory! {
                ServiceMarker::SERVICE_NAME => pseudo_directory! {},
            },
        };
        let dir_proxy = vfs::directory::serve_read_only(
            empty_tree,
            vfs::execution_scope::ExecutionScope::new(),
        );
        let service = Service::open_from_dir_prefix(&dir_proxy, SVC_DIR, ServiceMarker)?;
        let mut stream = exec.run_singlethreaded(service.watch())?;

        // Poll past WatchEvent::IDLE before registering an idle signal.
        assert!(exec.run_until_stalled(&mut stream.next()).is_pending());

        let (idle_tx, mut idle_rx) = oneshot::channel();
        let _stream = stream.with_idle_signal(idle_tx);
        assert_matches::assert_matches!(exec.run_until_stalled(&mut idle_rx), Poll::Ready(Ok(())));

        Ok(())
    }

    #[test]
    #[should_panic(expected = "ServiceInstanceStream::with_idle_signal called more than once")]
    fn test_service_instance_watcher_idle_signal_panics_if_set_twice() {
        let mut exec = fasync::TestExecutor::new();
        let dir_proxy = vfs::directory::serve_read_only(
            make_service_instance_tree(),
            vfs::execution_scope::ExecutionScope::new(),
        );
        let service = Service::open_from_dir_prefix(&dir_proxy, SVC_DIR, ServiceMarker).unwrap();
        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();
        let _stream = exec
            .run_singlethreaded(service.watch())
            .unwrap()
            .with_idle_signal(tx1)
            .with_idle_signal(tx2);
    }

    #[test]
    #[should_panic(expected = "ServiceInstanceStream::with_idle_signal called more than once")]
    fn test_service_instance_watcher_idle_signal_panics_if_set_twice_after_idle() {
        let mut exec = fasync::TestExecutor::new();
        let empty_tree = pseudo_directory! {
            "svc" => pseudo_directory! {
                ServiceMarker::SERVICE_NAME => pseudo_directory! {},
            },
        };
        let dir_proxy = vfs::directory::serve_read_only(
            empty_tree,
            vfs::execution_scope::ExecutionScope::new(),
        );
        let service = Service::open_from_dir_prefix(&dir_proxy, SVC_DIR, ServiceMarker).unwrap();
        let (tx1, mut rx1) = oneshot::channel();
        let mut stream = exec.run_singlethreaded(service.watch()).unwrap().with_idle_signal(tx1);

        assert!(exec.run_until_stalled(&mut stream.next()).is_pending());
        assert_matches::assert_matches!(exec.run_until_stalled(&mut rx1), Poll::Ready(Ok(())));

        let (tx2, _rx2) = oneshot::channel();
        let _stream = stream.with_idle_signal(tx2);
    }
}
