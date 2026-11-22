// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error, anyhow, bail};
use fidl::endpoints::ClientEnd;
use fidl_fuchsia_io as fio;
use fidl_fuchsia_virtualization::{BlockSpec, GuestConfig};
use fidl_fuchsia_virtualization_guest_interaction::{
    CommandListenerEvent, CommandListenerMarker, EnvironmentVariable, GuestType,
    InteractiveGuestMarker, InteractiveGuestProxy,
};
use fuchsia_async::{DurationExt, TimeoutExt};
use fuchsia_component::client::connect_to_protocol;
use futures::TryStreamExt;
use futures::lock::Mutex as AsyncMutex;
use std::cell::OnceCell;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

const EXECUTE_TIMEOUT_SECONDS: i64 = 180;

#[derive(Debug)]
pub struct DataMount {
    /// The block spec for this data mount.
    pub block_spec: BlockSpec,
    /// An executable shell script for mounting this data mount.
    pub mount_script_file: ClientEnd<fio::FileMarker>,
}

impl DataMount {
    pub fn new(block_spec: BlockSpec, mount_script_file: ClientEnd<fio::FileMarker>) -> Self {
        Self { block_spec, mount_script_file }
    }
}

pub struct DebianGuest {
    instance_name: String,
    /// The proxy for interacting with the guest. This should be accessed by the `interactive_guest`
    /// helper function, to aid with locking and ensuring that the guest is ready for interaction.
    guest_proxy: OnceCell<Mutex<InteractiveGuestProxy>>,
    data_mount: Mutex<Option<DataMount>>,
    pushed_files: AsyncMutex<HashMap<PathBuf, String>>,
}

impl DebianGuest {
    /// Creates a new instance of the DebianGuest. The actual bootstrapping of a guest is done
    /// lazily. This is because the lifecycle of the DebianGuest needs to live through the Starnix
    /// test runner framework, but not all tests will actually need the guest. Thus, we construct
    /// the DebianGuest while refraining from bootstrap until the guest is first interacted with.
    ///
    /// # Arguments
    /// * `instance_name` - An instance name, which serves as the tag for log output.
    pub fn new(instance_name: String) -> DebianGuest {
        DebianGuest {
            instance_name,
            guest_proxy: OnceCell::new(),
            data_mount: Mutex::new(None),
            pushed_files: AsyncMutex::new(HashMap::new()),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_test_proxy(guest_proxy: InteractiveGuestProxy) -> Self {
        Self {
            instance_name: "test_guest".to_string(),
            guest_proxy: OnceCell::from(Mutex::new(guest_proxy)),
            data_mount: Mutex::new(None),
            pushed_files: AsyncMutex::new(HashMap::new()),
        }
    }

    pub fn is_bootstrapped(&self) -> bool {
        self.guest_proxy.get().is_some()
    }

    /// Configures an optional data mount to attach to the guest upon initialization. Note that if the
    /// DebianGuest is already running, configuring this has no effect unless the guest is rebooted.
    /// Upon first initiatization, the provided block will be mounted and the executable script run.
    pub fn configure_data_mount(&self, data_mount: DataMount) {
        *self.data_mount.lock().unwrap() = Some(data_mount);
    }

    /// Gets a handle to the proxy, while also lazily bootstrapping the guest if necessary.
    async fn interactive_guest(&self) -> InteractiveGuestProxy {
        match self.guest_proxy.get() {
            Some(proxy_mutex) => proxy_mutex.lock().unwrap().clone(),
            None => {
                log::info!(tag = self.instance_name.as_str();
                    "Interaction requested, lazily starting the guest instance."
                );

                let (mount_block, mount_script) = self
                    .data_mount
                    .lock()
                    .unwrap()
                    .take()
                    .map(|data_mount| (vec![data_mount.block_spec], data_mount.mount_script_file))
                    .unzip();

                let mut cfg = GuestConfig::default();
                cfg.virtio_gpu = Some(false);
                cfg.virtio_sound = Some(false);
                cfg.virtio_sound_input = Some(false);
                cfg.virtio_rng = Some(true);
                cfg.virtio_balloon = Some(false);
                cfg.virtio_mem = Some(false);
                cfg.default_net = Some(false);
                cfg.block_devices = mount_block;

                let guest_proxy = connect_to_protocol::<InteractiveGuestMarker>()
                    .expect("Error connecting to InteractiveGuest");
                guest_proxy
                    .start(GuestType::Debian, &self.instance_name, cfg)
                    .await
                    .expect("Debian guest failed to start!");
                log::info!(tag = self.instance_name.as_str(); "Guest instance started successfully.");

                let return_proxy = guest_proxy.clone();
                let proxy_mutex = Mutex::new(guest_proxy);
                self.guest_proxy
                    .set(proxy_mutex)
                    .expect("Unexpected race condition while bootstrapping the guest proxy.");

                // The initialization of the mount requires interacting with the guest_proxy, and
                // will recursively call interactive_guest() to obtain said proxy. Therefore, the
                // mounting needs to be done only after self.guest_proxy has been set.
                if let Some(mount_script) = mount_script {
                    Box::pin(self.initialize_mount(mount_script))
                        .await
                        .expect("Failed to initialize guest data mount!");
                }

                return_proxy
            }
        }
    }

    /// Uploads and executes the guest data-mount script during lazy guest bootstrap.
    async fn initialize_mount(
        &self,
        mount_script: ClientEnd<fio::FileMarker>,
    ) -> Result<(), Error> {
        const GUEST_MOUNT_SCRIPT_PATH: &str = "/tmp/mount_syscall_deps.sh";

        self.push_data_to_guest_once(Path::new(GUEST_MOUNT_SCRIPT_PATH), None, async || {
            Ok(mount_script)
        })
        .await?;
        self.execute(&format!("/bin/sh {}", GUEST_MOUNT_SCRIPT_PATH), &[], None, None, None)
            .await?;

        log::info!(tag = self.instance_name.as_str(); "Mount initializated successfully.");
        Ok(())
    }

    /// Pushes the open_source content to the given destination on the guest. If a content_hash is
    /// provided, it will be used to validate whether the given content and destination have been
    /// pushed to the guest already. Upon pushing, the internal tracking cache will be updated.
    ///
    /// If a content_hash is not provided, the data will be pushed unconditionally, and no updates
    /// will be made to the internal tracking cache.
    pub async fn push_data_to_guest_once(
        &self,
        destination: &Path,
        content_hash: Option<&str>,
        open_source: impl AsyncFnOnce() -> Result<ClientEnd<fio::FileMarker>, Error>,
    ) -> Result<(), Error> {
        // The interactive guest is lazily initialized, and can attempt to push files itself. To
        // avoid holding the pushed_files lock across the first initialization sequence, which could
        // self-deadlock during initialization data pushes, we'll pre-warm the guest if necessary.
        if !self.is_bootstrapped() {
            let _ = self.interactive_guest().await;
        }

        let mut pushed_files = self.pushed_files.lock().await;
        if let Some(hash) = content_hash
            && pushed_files.get(destination).map(String::as_str) == Some(hash)
        {
            log::info!(
                tag = self.instance_name.as_str();
                "Skipping push to guest; file already present (destination: {}, hash: {})",
                destination.display(),
                hash
            );
            return Ok(());
        }

        let source = open_source().await?;
        pushed_files.remove(destination);

        log::info!(tag = self.instance_name.as_str(); "Pushing data to guest (destination: {})", destination.display());

        let dest_str = destination
            .to_str()
            .ok_or_else(|| anyhow!("Destination path is not valid UTF-8: {:?}", destination))?;
        let guest_proxy = self.interactive_guest().await;
        let response = guest_proxy
            .put_file(source, dest_str)
            .await
            .context("FIDL call to InteractiveGuest::PutFile has failed.")?;

        if let Err(status) = zx::Status::ok(response) {
            bail!("PutFile operation failed with status: {:?}", status);
        }

        log::info!(tag = self.instance_name.as_str();
            "Successfully pushed data to guest (destination: {})",
            destination.display()
        );

        if let Some(hash) = content_hash {
            pushed_files.insert(destination.to_path_buf(), hash.to_string());
        }
        Ok(())
    }

    /// Fetches a file from the guest.
    ///
    /// # Arguments
    /// * `remote_path` - The path to the file in the guest's filesystem.
    /// * `local_file_proxy` - The local file proxy to write the contents to.
    pub async fn get_file(
        &self,
        remote_path: &Path,
        local_file_proxy: ClientEnd<fio::FileMarker>,
    ) -> Result<(), Error> {
        log::info!(tag = self.instance_name.as_str(); "Fetching file from guest (remote_path: {})", remote_path.display());
        let remote_path_str = remote_path
            .to_str()
            .ok_or_else(|| anyhow!("Remote path is not valid UTF-8: {:?}", remote_path))?;
        let guest_proxy = self.interactive_guest().await;

        let response = guest_proxy
            .get_file(remote_path_str, local_file_proxy)
            .await
            .context("FIDL call to GetFile failed")?;

        if let Err(status) = zx::Status::ok(response) {
            bail!("GetFile operation failed with status: {:?}", status);
        }
        Ok(())
    }

    /// Executes a command on the guest, returning the command's exit code upon successful execution
    /// and an Error if the command was unable to be executed on the guest.
    ///
    /// # Arguments
    /// * `command`: The command string to execute (e.g., "/bin/ls -l /tmp").
    /// * `env_vars`: Environment vars to set for the execution context.
    /// * `stdin`: An optional `zx::Socket` for providing standard input to the command.
    /// * `stdout`: An optional client end for receiving stdout from the command.
    /// * `stderr`: An optional client end for receiving stderr from the command.
    pub async fn execute(
        &self,
        command: &str,
        env_vars: &[EnvironmentVariable],
        stdin: Option<zx::Socket>,
        stdout: Option<zx::Socket>,
        stderr: Option<zx::Socket>,
    ) -> Result<i32, Error> {
        log::info!(tag = self.instance_name.as_str(); "Executing command on guest: {}", command);
        let guest_proxy = self.interactive_guest().await;
        let (command_listener_client, command_listener_server) =
            fidl::endpoints::create_proxy::<CommandListenerMarker>();

        guest_proxy
            .execute_command(command, env_vars, stdin, stdout, stderr, command_listener_server)
            .context("FIDL call to ExecuteCommand failed")?;

        let mut event_stream = command_listener_client.take_event_stream();

        let execution_future = async move {
            while let Some(event) = event_stream.try_next().await? {
                match event {
                    CommandListenerEvent::OnStarted { status } => match zx::Status::ok(status) {
                        Ok(()) => {
                            log::info!(tag = self.instance_name.as_str(); "Command '{}'\n...started successfully", command)
                        }
                        Err(status) => {
                            bail!("Command '{}'\n...failed to start: {:?}", command, status)
                        }
                    },
                    CommandListenerEvent::OnTerminated { status, return_code } => {
                        let term_status = zx::Status::err_from_raw(status);
                        log::info!(tag = self.instance_name.as_str();
                            "Command '{}'\n...terminated with status {:?}, return code {}",
                            command,
                            term_status,
                            return_code
                        );
                        if let Err(status) = zx::Status::ok(status) {
                            bail!("Command '{}' failed with status: {:?}", command, status);
                        }
                        return Ok(return_code);
                    }
                }
            }

            panic!("Execution result stream closed before OnTerminated event!");
        };

        let timeout_duration = zx::MonotonicDuration::from_seconds(EXECUTE_TIMEOUT_SECONDS);
        execution_future
            .on_timeout(timeout_duration.after_now(), || {
                Err(anyhow!(
                    "Command execution '{}'\n...timed out after {} seconds",
                    command,
                    EXECUTE_TIMEOUT_SECONDS
                ))
            })
            .await
    }

    /// Shuts down the guest.
    pub async fn shutdown(&self) -> Result<(), Error> {
        match self.guest_proxy.get() {
            Some(proxy) => {
                log::info!(tag = self.instance_name.as_str(); "Shutting down guest instance.");
                let proxy_clone = proxy.lock().unwrap().clone();
                proxy_clone.shutdown().await.context("FIDL call to Shutdown failed")
            }
            None => {
                log::info!(tag = self.instance_name.as_str(); "Guest was never bootstrapped, shutdown is unnecessary.");
                Ok(())
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_helpers {
    use super::*;
    use fidl::endpoints::{ServerEnd, create_proxy_and_stream};
    use fidl_fuchsia_virtualization_guest_interaction::InteractiveGuestRequest;
    use fuchsia_async as fasync;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Simulates a mock guest command starting cleanly and exiting with the given code.
    pub(crate) fn send_command_exit(listener: ServerEnd<CommandListenerMarker>, return_code: i32) {
        let (_stream, control_handle) = listener.into_stream_and_control_handle();
        control_handle.send_on_started(zx::sys::ZX_OK).expect("send OnStarted");
        control_handle.send_on_terminated(zx::sys::ZX_OK, return_code).expect("send OnTerminated");
    }

    /// Mock `InteractiveGuest` server paired with a pre-bootstrapped `DebianGuest` for unit tests.
    pub(crate) struct MockDebianGuest {
        pub(crate) guest: Arc<DebianGuest>,
        put_file_count: Arc<AtomicUsize>,
        scope: fasync::Scope,
    }

    impl MockDebianGuest {
        pub(crate) fn new() -> Self {
            Self::with_put_file_handler(|_remote_path, _attempt| zx::sys::ZX_OK)
        }

        pub(crate) fn with_put_file_handler(
            mut handler: impl FnMut(String, usize) -> zx::sys::zx_status_t + 'static,
        ) -> Self {
            let mut attempt = 0;
            Self::with_request_handler(move |request| match request {
                InteractiveGuestRequest::PutFile { remote_path, responder, .. } => {
                    let status = handler(remote_path, attempt);
                    attempt += 1;
                    responder.send(status).expect("send PutFile response");
                }
                other => panic!("Unexpected InteractiveGuest request: {:?}", other),
            })
        }

        pub(crate) fn with_request_handler(
            mut handler: impl FnMut(InteractiveGuestRequest) + 'static,
        ) -> Self {
            let scope = fasync::Scope::new();
            let put_file_count = Arc::new(AtomicUsize::new(0));
            let put_file_count_server = put_file_count.clone();
            let (guest_proxy, mut guest_stream) =
                create_proxy_and_stream::<InteractiveGuestMarker>();
            let _server_task = scope.spawn_local(async move {
                while let Some(request) = guest_stream.try_next().await.expect("guest stream") {
                    if matches!(request, InteractiveGuestRequest::PutFile { .. }) {
                        put_file_count_server.fetch_add(1, Ordering::SeqCst);
                    }
                    handler(request);
                }
            });

            Self {
                guest: Arc::new(DebianGuest::with_test_proxy(guest_proxy)),
                put_file_count,
                scope,
            }
        }

        pub(crate) fn put_file_count(&self) -> usize {
            self.put_file_count.load(Ordering::SeqCst)
        }

        pub(crate) async fn finish(self) {
            drop(self.guest);
            self.scope.join().await;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_helpers::MockDebianGuest;
    use super::*;
    use fidl::endpoints::{create_endpoints, create_proxy_and_stream};
    use fidl_fuchsia_virtualization_guest_interaction::{
        InteractiveGuestPutFileResponder, InteractiveGuestRequest, InteractiveGuestRequestStream,
    };
    use futures::future::join;
    use futures::poll;
    use std::pin::pin;
    use std::sync::atomic::{AtomicUsize, Ordering};

    async fn open_dummy_file() -> Result<ClientEnd<fio::FileMarker>, Error> {
        let (client, _server) = create_endpoints::<fio::FileMarker>();
        Ok(client)
    }

    async fn next_put_file(
        stream: &mut InteractiveGuestRequestStream,
    ) -> InteractiveGuestPutFileResponder {
        match stream.try_next().await.expect("guest stream") {
            Some(InteractiveGuestRequest::PutFile { responder, .. }) => responder,
            other => panic!("Expected PutFile request, got: {:?}", other),
        }
    }

    #[fuchsia::test]
    async fn push_data_to_guest_once_deduplicates_and_updates() {
        let mock = MockDebianGuest::new();
        let first_path = Path::new("/starnix_linux_foo_bin");
        let second_path = Path::new("/starnix_linux_bar_bin");

        mock.guest
            .push_data_to_guest_once(first_path, Some("hash_v1"), open_dummy_file)
            .await
            .expect("first push succeeds");
        assert_eq!(mock.put_file_count(), 1);

        // Cache hit must skip both `open_source` and `PutFile`.
        mock.guest
            .push_data_to_guest_once(first_path, Some("hash_v1"), async || {
                panic!("open_source must not be called on cache hit")
            })
            .await
            .expect("duplicate push with same hash is skipped");
        assert_eq!(mock.put_file_count(), 1);

        mock.guest
            .push_data_to_guest_once(first_path, Some("hash_v2"), open_dummy_file)
            .await
            .expect("updated hash re-pushes to same path");
        assert_eq!(mock.put_file_count(), 2);

        mock.guest
            .push_data_to_guest_once(first_path, None, open_dummy_file)
            .await
            .expect("push without hash always pushes and clears cached hash");
        assert_eq!(mock.put_file_count(), 3);

        // Because the unhashed push cleared the cache, re-pushing `hash_v2` must push again.
        mock.guest
            .push_data_to_guest_once(first_path, Some("hash_v2"), open_dummy_file)
            .await
            .expect("push after unhashed eviction pushes again");
        assert_eq!(mock.put_file_count(), 4);

        mock.guest
            .push_data_to_guest_once(second_path, Some("hash_v1"), open_dummy_file)
            .await
            .expect("distinct path is pushed");
        assert_eq!(mock.put_file_count(), 5);

        mock.finish().await;
    }

    #[fuchsia::test]
    async fn push_data_to_guest_once_does_not_cache_failed_push() {
        let mock = MockDebianGuest::with_put_file_handler(|_remote_path, attempt| {
            if attempt == 0 || attempt == 2 { zx::Status::IO.into_raw() } else { zx::sys::ZX_OK }
        });
        let destination = Path::new("/starnix_linux_retry_bin");

        assert!(
            mock.guest
                .push_data_to_guest_once(destination, Some("hash_v1"), open_dummy_file)
                .await
                .is_err()
        );

        mock.guest
            .push_data_to_guest_once(destination, Some("hash_v1"), open_dummy_file)
            .await
            .expect("retry push succeeds");
        assert_eq!(mock.put_file_count(), 2);

        // A failed overwrite must evict any previously cached hash so callers never reuse a
        // partially written file on the guest-even if they subsequently request `hash_v1`.
        assert!(
            mock.guest
                .push_data_to_guest_once(destination, Some("hash_v2"), open_dummy_file)
                .await
                .is_err()
        );
        mock.guest
            .push_data_to_guest_once(destination, Some("hash_v1"), open_dummy_file)
            .await
            .expect("retry after failed overwrite pushes again");
        assert_eq!(mock.put_file_count(), 4);

        mock.finish().await;
    }

    #[fuchsia::test]
    async fn push_data_to_guest_once_serializes_concurrent_callers() {
        let (guest_proxy, mut guest_stream) = create_proxy_and_stream::<InteractiveGuestMarker>();
        let guest = DebianGuest::with_test_proxy(guest_proxy);
        let destination = Path::new("/starnix_linux_concurrent_bin");
        let open_count = AtomicUsize::new(0);
        let make_open = || {
            async || {
                open_count.fetch_add(1, Ordering::SeqCst);
                open_dummy_file().await
            }
        };

        // Case 1: Concurrent push for the same (destination, hash) waits on `pushed_files` and
        // skips both `open_source` and `PutFile` once the in-flight push succeeds.
        {
            let mut call1 =
                pin!(guest.push_data_to_guest_once(destination, Some("hash_v1"), make_open()));
            let mut call2 =
                pin!(guest.push_data_to_guest_once(destination, Some("hash_v1"), make_open()));

            assert!(poll!(call1.as_mut()).is_pending());
            let responder = next_put_file(&mut guest_stream).await;
            assert!(poll!(call2.as_mut()).is_pending());

            responder.send(zx::sys::ZX_OK).expect("send PutFile OK");
            let (res1, res2) = join(call1, call2).await;
            res1.expect("first concurrent push succeeds");
            res2.expect("second concurrent push deduplicates");
            assert_eq!(open_count.load(Ordering::SeqCst), 1);
        }

        // Case 2: If the in-flight push fails, the queued waiter retries rather than skipping.
        {
            let mut call3 =
                pin!(guest.push_data_to_guest_once(destination, Some("hash_v2"), make_open()));
            let mut call4 =
                pin!(guest.push_data_to_guest_once(destination, Some("hash_v2"), make_open()));

            assert!(poll!(call3.as_mut()).is_pending());
            let fail_responder = next_put_file(&mut guest_stream).await;
            assert!(poll!(call4.as_mut()).is_pending());

            fail_responder.send(zx::Status::IO.into_raw()).expect("send PutFile IO error");
            assert!(call3.await.is_err());
            assert!(poll!(call4.as_mut()).is_pending());

            let retry_responder = next_put_file(&mut guest_stream).await;
            retry_responder.send(zx::sys::ZX_OK).expect("send retry PutFile OK");
            call4.await.expect("waiting caller retries after failed in-flight push");
            assert_eq!(open_count.load(Ordering::SeqCst), 3);
        }
    }
}
