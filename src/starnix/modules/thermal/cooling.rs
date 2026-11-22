// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use anyhow::{Context, Error, anyhow, format_err};
use fidl::endpoints::Proxy as _;
use fidl_fuchsia_power_battery as fbattery;
use fuchsia_async as fasync;
use futures::channel::mpsc;
use futures::{FutureExt as _, StreamExt as _};
use starnix_core::device::kobject::Device;
use starnix_core::device::service_watcher::ServiceWatcher;
use starnix_core::fs::sysfs::{CpuFreqDomain, build_device_directory, get_cpu_freq_domains};
use starnix_core::task::{CurrentTask, Kernel, ThreadLockupDetector};
use starnix_core::vfs::FsNodeOps;
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectoryMutator;
use starnix_core::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use starnix_logging::{log_error, log_warn};
use starnix_uapi::errors::{Errno, errno};
use starnix_uapi::file_mode::mode;
use std::borrow::Cow;
use std::future::Future;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

trait CoolingOps: Send + Sync + 'static {
    fn get_max_state(&self) -> u32;
    fn get_state(&self) -> Result<u32, Errno>;
    fn set_state(&self, state: u32) -> Result<(), Errno>;
}

impl<T: CoolingOps> CoolingOps for Arc<T> {
    fn get_max_state(&self) -> u32 {
        self.as_ref().get_max_state()
    }

    fn get_state(&self) -> Result<u32, Errno> {
        self.as_ref().get_state()
    }

    fn set_state(&self, state: u32) -> Result<(), Errno> {
        self.as_ref().set_state(state)
    }
}

struct CoolingDevice<T: CoolingOps> {
    device_id: u32,
    device_type: String,
    ops: T,
}

impl<T: CoolingOps> CoolingDevice<T> {
    fn get_device_name(&self) -> String {
        format!("cooling_device{}", self.device_id)
    }

    fn build_device_dir(self: Arc<Self>, device: &Device, dir: &SimpleDirectoryMutator) {
        build_device_directory(device, dir);
        dir.entry(
            "max_state",
            BytesFile::new_node(format!("{}\n", self.ops.get_max_state()).into_bytes()),
            mode!(IFREG, 0o444),
        );
        dir.entry(
            "type",
            BytesFile::new_node(format!("{}\n", self.device_type).into_bytes()),
            mode!(IFREG, 0o444),
        );
        dir.entry("cur_state", CurStateFile::new_node(self), mode!(IFREG, 0o644));
    }
}

/// Current state file, which proxies integral reads and writes to [`CoolingOps`].
struct CurStateFile<T: CoolingOps> {
    cooling_device: Arc<CoolingDevice<T>>,
}

impl<T: CoolingOps> CurStateFile<T> {
    fn new_node(cooling_device: Arc<CoolingDevice<T>>) -> impl FsNodeOps {
        BytesFile::new_node(Self { cooling_device })
    }
}

impl<T: CoolingOps> BytesFileOps for CurStateFile<T> {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let state = self.cooling_device.ops.get_state()?;
        Ok(format!("{}\n", state).into_bytes().into())
    }

    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let input = str::from_utf8(&data).map_err(|_| errno!(EINVAL))?;
        let input_num = input.trim().parse().map_err(|_| errno!(EINVAL))?;
        self.cooling_device.ops.set_state(input_num)
    }
}

/// Registrar for cooling devices, which are sequentially numbered.
struct CoolingDeviceRegistrar {
    next_id: u32,
}

impl CoolingDeviceRegistrar {
    fn new() -> Self {
        Self { next_id: 0 }
    }

    fn get_next_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    /// Register a device in the thermal class.
    fn register<T: CoolingOps>(
        &mut self,
        kernel: &Kernel,
        device_type: String,
        ops: T,
    ) -> Result<Device, Errno> {
        let device_registry = &kernel.device_registry;
        let device_class = device_registry.objects.thermal_class();

        let cooling_device =
            Arc::new(CoolingDevice::<T> { device_id: self.get_next_id(), device_type, ops });

        device_registry.add_numberless_device(
            cooling_device.get_device_name().as_str().into(),
            /* parent = */ None,
            device_class,
            |device, dir| cooling_device.build_device_dir(device, dir),
        )
    }
}

const COOLING_DEVICE_VOTER: &str = "cooling_device";

struct CpuCoolingOps {
    domain: Arc<CpuFreqDomain>,
}

impl CpuCoolingOps {
    fn new(domain: Arc<CpuFreqDomain>) -> Self {
        Self { domain }
    }
}

impl CoolingOps for CpuCoolingOps {
    fn get_max_state(&self) -> u32 {
        (self.domain.opp_count() - 1) as u32
    }
    fn get_state(&self) -> Result<u32, Errno> {
        Ok(self.domain.get_vote(COOLING_DEVICE_VOTER).unwrap_or(0) as u32)
    }
    fn set_state(&self, state: u32) -> Result<(), Errno> {
        let opp_index = if state == 0 { None } else { Some(state.into()) };
        self.domain.update_vote(COOLING_DEVICE_VOTER, opp_index)
    }
}

fn register_cpu_domains(
    kernel: &Kernel,
    registrar: &mut CoolingDeviceRegistrar,
) -> Result<(), Error> {
    let domains = get_cpu_freq_domains(kernel);
    if !domains.iter().any(|d| d.is_tunable()) {
        return Err(anyhow!("No tunable CPU domains available from DomainController"));
    }

    // Expose each tunable domain as a separate cooling device.
    for domain in domains.iter().filter(|d| d.is_tunable()) {
        let device_type = domain.name().context("name not provided")?.to_string();
        let ops = CpuCoolingOps::new(domain.clone());
        registrar.register(kernel, device_type, ops)?;
    }
    Ok(())
}

/// Initializes the cooling devices specified in the device list.
///
/// Device strings are of the form `type[=param]`. Not all device types support a parameter.
///
/// Supported devices:
/// * `fcc=N`: Fast charge current, where N is the maximum charge level.
pub fn cooling_device_init(kernel: &Kernel, devices: Vec<String>) -> Result<(), Error> {
    let mut registrar = CoolingDeviceRegistrar::new();
    for device_spec in devices.into_iter() {
        let (device_type, device_param) =
            device_spec.split_once('=').map_or((device_spec.as_str(), None), |(t, p)| (t, Some(p)));
        match device_type {
            "fcc" => {
                // TODO(b/460321934): Return errors rather than logging them.
                if let Err(e) = register_fcc_device(
                    kernel,
                    &mut registrar,
                    device_param
                        .ok_or_else(|| format_err!("Missing parameter for 'fcc' cooling device"))?,
                ) {
                    log_error!("Failed to register 'fcc' cooling device: {e:?}");
                }
            }
            "cpu" => {
                // TODO(b/460321934): Return errors rather than logging them.
                if let Err(e) = register_cpu_domains(kernel, &mut registrar) {
                    log_error!("Failed to register 'cpu' cooling device: {e:?}");
                }
            }
            t => {
                return Err(format_err!("Unknown cooling device: {t:?}"));
            }
        };
    }

    Ok(())
}

const CHARGER_SYNC_TIMEOUT: zx::MonotonicDuration = zx::MonotonicDuration::from_seconds(5);

struct PendingSetState {
    charge_level: u32,
    should_enable: bool,
    waiters: Vec<(std::sync::mpsc::SyncSender<Result<(), Errno>>, fasync::MonotonicInstant)>,
}

impl PendingSetState {
    fn reply(self, result: Result<(), Errno>) {
        for (waiter, _) in self.waiters {
            let _ = waiter.send(result.clone());
        }
    }

    fn expire_waiters(&mut self, now: fasync::MonotonicInstant) {
        self.waiters.retain(|(waiter, deadline)| {
            if *deadline <= now {
                let _ = waiter.send(Err(errno!(ENODEV)));
                false
            } else {
                true
            }
        });
    }

    fn next_deadline(&self) -> Option<fasync::MonotonicInstant> {
        self.waiters.first().map(|(_, deadline)| *deadline)
    }
}

enum FccCommand {
    Connect {
        id: u64,
        proxy: fbattery::ChargerProxy,
    },
    Disconnect {
        id: u64,
    },
    SetState {
        charge_level: u32,
        should_enable: bool,
        reply: std::sync::mpsc::SyncSender<Result<(), Errno>>,
    },
}

/// [`CoolingOps`] implementation for the fast-charge current (`fcc`) cooling device.
///
/// The `fcc` cooling device is registered in `sysfs` synchronously during container initialization,
/// while the backing `fuchsia.power.battery.ChargerService` capability is connected asynchronously
/// by [`ServiceWatcher`]. Hardware transitions and charger reconnections are serialized on a
/// background async worker task with the following behavioral guarantees:
///
/// * **Disabling charging (`state == max_charge_level`)**: Always requires a connected
///   `ChargerService` proxy to confirm `Enable(false)`, even if `charge_level` was already at
///   `max_charge_level` prior to a disconnect. If no proxy is connected, the caller waits up to
///   [`CHARGER_SYNC_TIMEOUT`] for one to attach (failing with `ENODEV` if none attaches in time),
///   and the `Enable(false)` RPC itself is bounded by [`CHARGER_SYNC_TIMEOUT`].
/// * **Enabling charging (`state < max_charge_level`)**: Succeeds immediately without blocking if
///   no charger proxy is connected; if a proxy is connected and charging is not known to be
///   enabled, waits up to [`CHARGER_SYNC_TIMEOUT`] for `Enable(true)` to complete.
/// * **Coalescing**: Concurrent or queued `set_state` requests that arrive while waiting for a
///   proxy or an in-flight RPC are coalesced into the latest target state, with all waiting
///   callers notified once that transition completes (or when their individual wait-for-connect
///   deadline expires).
/// * **Reconnection sync**: When a charger proxy connects after charging was disabled or left in an
///   unknown state (e.g. after a disconnect or RPC timeout/error), the worker synchronizes the new
///   proxy with the current `charge_level`.
struct FccCoolingOps {
    charge_level: Arc<AtomicU32>,
    next_conn_id: AtomicU64,
    commands: mpsc::UnboundedSender<FccCommand>,
    max_charge_level: u32,
}

impl FccCoolingOps {
    fn new(max_charge_level: u32, charge_level: u32) -> (Arc<Self>, impl Future<Output = ()>) {
        let (commands, rx) = mpsc::unbounded();
        let charge_level = Arc::new(AtomicU32::new(charge_level));
        let worker = Self::run_worker(charge_level.clone(), max_charge_level, rx);
        let ops = Arc::new(Self {
            charge_level,
            next_conn_id: AtomicU64::new(0),
            commands,
            max_charge_level,
        });
        (ops, worker)
    }

    fn connect(&self, proxy: fbattery::ChargerProxy) -> u64 {
        let id = self.next_conn_id.fetch_add(1, Ordering::Relaxed);
        let _ = self.commands.unbounded_send(FccCommand::Connect { id, proxy });
        id
    }

    fn disconnect(&self, id: u64) {
        let _ = self.commands.unbounded_send(FccCommand::Disconnect { id });
    }

    fn enqueue_set_state(
        &self,
        state: u32,
    ) -> Result<std::sync::mpsc::Receiver<Result<(), Errno>>, Errno> {
        // Attempting to set a charge level greater than the maximum results in 0 being set.
        // This is based on observations of how this node behaves on Linux.
        // See b/446016549#comment4 for details.
        let charge_level = if state > self.max_charge_level {
            log_warn!(
                "FCC charge_level of {} exceeds {}; setting to 0",
                state,
                self.max_charge_level
            );
            0
        } else {
            state
        };

        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        self.commands
            .unbounded_send(FccCommand::SetState {
                charge_level,
                should_enable: charge_level < self.max_charge_level,
                reply,
            })
            .map_err(|_| errno!(ENODEV))?;
        Ok(rx)
    }

    /// Calls `Charger.Enable(enable)` with `deadline`, returning `Err(None)` if the peer closed its
    /// channel or `Err(Some(errno))` on FIDL error or timeout.
    async fn set_charger_enabled(
        proxy: &fbattery::ChargerProxy,
        enable: bool,
        deadline: fasync::MonotonicInstant,
    ) -> Result<(), Option<Errno>> {
        futures::select_biased! {
            res = proxy.enable(enable).fuse() => match res {
                Ok(Ok(())) => Ok(()),
                Ok(Err(e)) => Err(Some(errno!(EIO, e))),
                Err(e) if e.is_closed() => Err(None),
                Err(e) => Err(Some(errno!(EIO, e))),
            },
            () = fasync::Timer::new(deadline).fuse() => Err(Some(errno!(ETIMEDOUT))),
        }
    }

    fn handle_command(
        cmd: FccCommand,
        proxy: &mut Option<(u64, fbattery::ChargerProxy)>,
        charger_enabled: &mut Option<bool>,
        sync_on_connect: &mut bool,
        pending: &mut Option<PendingSetState>,
    ) {
        match cmd {
            FccCommand::Connect { id, proxy: new_proxy } => {
                *proxy = Some((id, new_proxy));
                if *charger_enabled != Some(true) {
                    *charger_enabled = None;
                }
                *sync_on_connect = true;
            }
            FccCommand::Disconnect { id } => {
                if proxy.as_ref().is_some_and(|(cur_id, _)| *cur_id == id) {
                    *proxy = None;
                    if *charger_enabled != Some(true) {
                        *charger_enabled = None;
                    }
                    *sync_on_connect = false;
                }
            }
            FccCommand::SetState { charge_level, should_enable, reply } => {
                let mut waiters = pending.take().map(|prev| prev.waiters).unwrap_or_default();
                waiters.push((reply, fasync::MonotonicInstant::after(CHARGER_SYNC_TIMEOUT)));
                *pending = Some(PendingSetState { charge_level, should_enable, waiters });
            }
        }
    }

    async fn run_worker(
        charge_level: Arc<AtomicU32>,
        max_charge_level: u32,
        mut commands: mpsc::UnboundedReceiver<FccCommand>,
    ) {
        let mut proxy: Option<(u64, fbattery::ChargerProxy)> = None;
        let mut charger_enabled: Option<bool> = Some(true);
        let mut sync_on_connect = false;
        let mut pending: Option<PendingSetState> = None;

        loop {
            loop {
                match commands.try_recv() {
                    Ok(cmd) => Self::handle_command(
                        cmd,
                        &mut proxy,
                        &mut charger_enabled,
                        &mut sync_on_connect,
                        &mut pending,
                    ),
                    Err(e) if e.is_closed() => return,
                    Err(_) => break,
                }
            }

            if proxy.as_ref().is_some_and(|(_, p)| p.is_closed()) {
                proxy = None;
                if charger_enabled != Some(true) {
                    charger_enabled = None;
                }
                sync_on_connect = false;
            }

            if let Some(mut req) = pending.take() {
                req.expire_waiters(fasync::MonotonicInstant::now());
                if !req.waiters.is_empty() {
                    // Setting a state below `max_charge_level` (`should_enable == true`) does not
                    // require a connected charger; disabling charging (`should_enable == false`)
                    // always requires a connected charger to confirm `Enable(false)`, even if
                    // `charge_level` was already `max_charge_level` prior to a disconnect.
                    if (req.should_enable && proxy.is_none())
                        || charger_enabled == Some(req.should_enable)
                    {
                        charge_level.store(req.charge_level, Ordering::Relaxed);
                        req.reply(Ok(()));
                    } else if let Some((_, active_proxy)) = proxy.as_ref() {
                        sync_on_connect = false;
                        charger_enabled = None;
                        let deadline = fasync::MonotonicInstant::after(CHARGER_SYNC_TIMEOUT);
                        match Self::set_charger_enabled(active_proxy, req.should_enable, deadline)
                            .await
                        {
                            Ok(()) => {
                                charger_enabled = Some(req.should_enable);
                                charge_level.store(req.charge_level, Ordering::Relaxed);
                                req.reply(Ok(()));
                            }
                            Err(None) => {
                                proxy = None;
                                if req.should_enable {
                                    charge_level.store(req.charge_level, Ordering::Relaxed);
                                    req.reply(Ok(()));
                                } else {
                                    pending = Some(req);
                                }
                            }
                            Err(Some(err)) => req.reply(Err(err)),
                        }
                        continue;
                    } else {
                        pending = Some(req);
                    }
                }
            }

            if sync_on_connect {
                sync_on_connect = false;
                let should_enable = charge_level.load(Ordering::Relaxed) < max_charge_level;
                if charger_enabled != Some(should_enable)
                    && let Some((_, active_proxy)) = proxy.as_ref()
                {
                    charger_enabled = None;
                    let deadline = fasync::MonotonicInstant::after(CHARGER_SYNC_TIMEOUT);
                    match Self::set_charger_enabled(active_proxy, should_enable, deadline).await {
                        Ok(()) => charger_enabled = Some(should_enable),
                        Err(None) => {
                            proxy = None;
                            log_warn!("Charger peer closed while syncing on connect");
                        }
                        Err(Some(err)) => {
                            log_warn!("Failed to sync charger on connect: {err:?}");
                        }
                    }
                    continue;
                }
            }

            let active_proxy = proxy.as_ref().map(|(_, p)| p.clone());
            let mut on_closed = std::pin::pin!(match active_proxy.as_ref() {
                Some(p) => p.on_closed().fuse(),
                None => futures::future::Fuse::terminated(),
            });
            let mut timer =
                std::pin::pin!(match pending.as_ref().and_then(|r| r.next_deadline()) {
                    Some(deadline) => fasync::Timer::new(deadline).fuse(),
                    None => futures::future::Fuse::terminated(),
                });

            futures::select_biased! {
                cmd = commands.next() => match cmd {
                    None => break,
                    Some(cmd) => Self::handle_command(
                        cmd,
                        &mut proxy,
                        &mut charger_enabled,
                        &mut sync_on_connect,
                        &mut pending,
                    ),
                },
                // Both `on_closed` and `timer` only need to wake the loop; the top of the next
                // iteration checks `is_closed()` and expires any elapsed waiter deadlines.
                _ = on_closed => {}
                () = timer => {}
            }
        }
    }
}

impl CoolingOps for FccCoolingOps {
    fn get_max_state(&self) -> u32 {
        self.max_charge_level
    }

    fn get_state(&self) -> Result<u32, Errno> {
        Ok(self.charge_level.load(Ordering::Relaxed))
    }

    fn set_state(&self, state: u32) -> Result<(), Errno> {
        let rx = self.enqueue_set_state(state)?;
        let _waiting_guard = ThreadLockupDetector::pause_tracking();
        rx.recv().map_err(|_| errno!(ENODEV))?
    }
}

fn register_fcc_device(
    kernel: &Kernel,
    registrar: &mut CoolingDeviceRegistrar,
    param: &str,
) -> Result<(), Error> {
    let max_charge_level: u32 = param.parse().context("Invalid max_charge_level")?;
    let (ops, worker) = FccCoolingOps::new(max_charge_level, /* charge_level = */ 0);

    // Register the cooling device synchronously so sysfs scanners (such as Android's thermal
    // init scripts and Thermal HAL) find `/sys/class/thermal/cooling_device*` at startup before
    // asynchronous ChargerService discovery completes.
    registrar.register(kernel, "fcc".to_string(), ops.clone())?;
    kernel.kthreads.spawn_future(move || worker, "fcc_charger_worker");

    ServiceWatcher::register_singleton(
        kernel,
        fbattery::ChargerServiceMarker,
        move |current_task, instance, scope| {
            let proxy = match instance.connect_to_device() {
                Ok(proxy) => proxy,
                Err(e) => {
                    log_error!("Failed to connect to battery Charger: {e:?}");
                    scope.teardown(current_task);
                    return;
                }
            };
            scope.monitor_async_channel(proxy.clone());
            let id = ops.connect(proxy);
            let ops_for_disconnect = ops.clone();
            scope.on_disconnect(move |_current_task| {
                ops_for_disconnect.disconnect(id);
            });
        },
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_uapi::error;

    async fn collect_enable_requests(
        stream: &mut fbattery::ChargerRequestStream,
        count: usize,
    ) -> Vec<bool> {
        let mut requests = Vec::with_capacity(count);
        while requests.len() < count {
            match stream.next().await.expect("stream item").expect("charger request") {
                fbattery::ChargerRequest::Enable { enable, responder } => {
                    requests.push(enable);
                    let _ = responder.send(Ok(()));
                }
                other => panic!("unexpected request: {other:?}"),
            }
        }
        requests
    }

    #[::fuchsia::test]
    async fn fcc_cooling_device_registered_before_charger_service_attaches() {
        spawn_kernel_and_run(async |current_task| {
            let kernel = current_task.kernel();
            cooling_device_init(kernel, vec!["fcc=8".to_string()]).expect("cooling_device_init");

            let root = &kernel.device_registry.objects.root;
            assert!(root.lookup("class/thermal/cooling_device0".into()).is_some());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn fcc_set_state_waits_for_delayed_proxy_and_skips_duplicate_transitions() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::ChargerMarker>();
        let (ops, worker) = FccCoolingOps::new(8, /* charge_level = */ 0);
        let _worker = fasync::Task::local(worker);

        let ops_for_writer = ops.clone();
        let writer_task = fasync::unblock(move || {
            // Transitioning to `max_charge_level` blocks until `connect` runs.
            ops_for_writer.set_state(8).expect("set_state(8)");
            assert_eq!(ops_for_writer.get_state().expect("get_state"), 8);

            // Setting `max_charge_level` again when already at `max_charge_level` must not issue a
            // duplicate `Enable(false)` FIDL call.
            ops_for_writer.set_state(8).expect("duplicate set_state(8)");
            assert_eq!(ops_for_writer.get_state().expect("get_state"), 8);

            // Dropping below `max_charge_level` re-enables charging.
            ops_for_writer.set_state(4).expect("set_state(4)");
            assert_eq!(ops_for_writer.get_state().expect("get_state"), 4);
        });

        ops.connect(proxy);

        assert_eq!(collect_enable_requests(&mut stream, 2).await, vec![false, true]);
        writer_task.await;
    }

    fn next_enable_request(
        exec: &mut fasync::TestExecutor,
        stream: &mut fbattery::ChargerRequestStream,
    ) -> (bool, fbattery::ChargerEnableResponder) {
        let mut next = stream.next();
        match exec.run_until_stalled(&mut next) {
            std::task::Poll::Ready(Some(Ok(fbattery::ChargerRequest::Enable {
                enable,
                responder,
            }))) => (enable, responder),
            other => panic!("unexpected charger request poll result: {other:?}"),
        }
    }

    #[::fuchsia::test]
    fn fcc_set_state_returns_enodev_on_proxy_timeout_and_after_disconnect() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        let (ops, worker) = FccCoolingOps::new(8, /* charge_level = */ 0);
        let mut worker = std::pin::pin!(worker);

        // State changes below `max_charge_level` do not toggle charging and succeed without a
        // connected `ChargerService`.
        let rx = ops.enqueue_set_state(3).expect("enqueue_set_state(3)");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(rx.try_recv(), Ok(Ok(())));
        assert_eq!(ops.get_state().expect("get_state"), 3);

        // Transitioning to `max_charge_level` without an attached `ChargerService` times out
        // with `ENODEV` and preserves the previous `charge_level`.
        let rx = ops.enqueue_set_state(8).expect("enqueue_set_state(8)");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
        exec.set_fake_time(exec.now() + CHARGER_SYNC_TIMEOUT);
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(rx.try_recv(), Ok(error!(ENODEV)));
        assert_eq!(ops.get_state().expect("get_state"), 3);

        // Attaching a proxy, disabling charging at `max_charge_level`, and then disconnecting the
        // proxy resets readiness so even a redundant `set_state(max_charge_level)` times out with
        // `ENODEV`.
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::ChargerMarker>();
        let id = ops.connect(proxy);
        let rx = ops.enqueue_set_state(8).expect("enqueue_set_state(8) while connected");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        let (enable, responder) = next_enable_request(&mut exec, &mut stream);
        assert!(!enable);
        responder.send(Ok(())).expect("send Enable response");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(rx.try_recv(), Ok(Ok(())));
        assert_eq!(ops.get_state().expect("get_state"), 8);

        ops.disconnect(id);

        let rx = ops.enqueue_set_state(8).expect("enqueue_set_state(8) after disconnect");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(rx.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
        exec.set_fake_time(exec.now() + CHARGER_SYNC_TIMEOUT);
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(rx.try_recv(), Ok(error!(ENODEV)));
        assert_eq!(ops.get_state().expect("get_state"), 8);

        // Dropping the last `FccCoolingOps` reference while `proxy` and `pending` are `None` must
        // cleanly terminate `run_worker`.
        drop(ops);
        assert!(exec.run_until_stalled(&mut worker).is_ready());
    }

    #[::fuchsia::test]
    async fn fcc_reconnect_at_max_charge_level_disables_new_charger() {
        let (proxy1, mut stream1) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::ChargerMarker>();
        let (ops, worker) = FccCoolingOps::new(8, /* charge_level = */ 0);
        let _worker = fasync::Task::local(worker);
        let id1 = ops.connect(proxy1);

        let ops_for_writer = ops.clone();
        let writer_task = fasync::unblock(move || {
            ops_for_writer.set_state(8).expect("set_state(8)");
        });
        assert_eq!(collect_enable_requests(&mut stream1, 1).await, vec![false]);
        writer_task.await;
        assert_eq!(ops.get_state().expect("get_state"), 8);

        // Simulate `ChargerService` closing its channel and a new instance connecting before the
        // old instance's `disconnect(id1)` arrives. `connect` must disable charging on `proxy2`
        // immediately, `disconnect(id1)` must not clear `proxy2`, and a subsequent `set_state(8)`
        // must not send a duplicate `Enable(false)` call.
        drop(stream1);

        let (proxy2, mut stream2) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::ChargerMarker>();
        ops.connect(proxy2);
        ops.disconnect(id1);
        assert_eq!(collect_enable_requests(&mut stream2, 1).await, vec![false]);

        let ops_for_reconnect = ops.clone();
        let reconnect_task = fasync::unblock(move || {
            ops_for_reconnect.set_state(8).expect("set_state(8) after reconnect");
            ops_for_reconnect.set_state(4).expect("set_state(4) after reconnect");
            ops_for_reconnect.set_state(8).expect("set_state(8) before disconnect");
        });

        assert_eq!(collect_enable_requests(&mut stream2, 2).await, vec![true, false]);
        reconnect_task.await;
        assert_eq!(ops.get_state().expect("get_state"), 8);
    }

    #[::fuchsia::test]
    async fn fcc_set_state_below_max_while_disconnected_does_not_block() {
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::ChargerMarker>();
        let (ops, worker) = FccCoolingOps::new(8, /* charge_level = */ 0);
        let _worker = fasync::Task::local(worker);
        let id = ops.connect(proxy);

        let ops_for_writer = ops.clone();
        let writer_task = fasync::unblock(move || {
            ops_for_writer.set_state(8).expect("set_state(8)");
        });
        assert_eq!(collect_enable_requests(&mut stream, 1).await, vec![false]);
        writer_task.await;
        assert_eq!(ops.get_state().expect("get_state"), 8);

        // If `ChargerService` disconnects while at `max_charge_level`, dropping below
        // `max_charge_level` while still disconnected must succeed immediately without waiting for
        // a charger proxy to reconnect, and a subsequent reconnect must sync `Enable(true)`.
        ops.disconnect(id);
        let ops_for_disconnected = ops.clone();
        fasync::unblock(move || {
            ops_for_disconnected.set_state(2).expect("set_state(2) while disconnected");
            assert_eq!(ops_for_disconnected.get_state().expect("get_state"), 2);
        })
        .await;

        let (proxy2, mut stream2) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::ChargerMarker>();
        ops.connect(proxy2);
        assert_eq!(collect_enable_requests(&mut stream2, 1).await, vec![true]);
    }

    #[::fuchsia::test]
    fn fcc_set_state_handles_rpc_errors_and_mid_rpc_peer_closure() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        let (proxy1, mut stream1) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::ChargerMarker>();
        let (ops, worker) = FccCoolingOps::new(8, /* charge_level = */ 0);
        let mut worker = std::pin::pin!(worker);
        ops.connect(proxy1);

        // 1. FIDL driver error (`Err(ZX_ERR_IO)`) maps to `EIO` and leaves `charger_enabled`
        // unknown (`None`).
        let rx_eio = ops.enqueue_set_state(8).expect("enqueue_set_state(8)");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        let (enable, responder) = next_enable_request(&mut exec, &mut stream1);
        assert!(!enable);
        responder.send(Err(zx::Status::IO.into_raw())).expect("send EIO response");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(rx_eio.try_recv(), Ok(error!(EIO)));
        assert_eq!(ops.get_state().expect("get_state"), 0);

        // 2. RPC timeout maps to `ETIMEDOUT` and leaves `charger_enabled` unknown (`None`), so a
        // subsequent `set_state(4)` (`< max_charge_level`) re-syncs hardware with `Enable(true)`.
        let rx_timeout = ops.enqueue_set_state(8).expect("enqueue_set_state(8)");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        let (enable, responder) = next_enable_request(&mut exec, &mut stream1);
        assert!(!enable);
        exec.set_fake_time(exec.now() + CHARGER_SYNC_TIMEOUT);
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(rx_timeout.try_recv(), Ok(error!(ETIMEDOUT)));
        let _ = responder.send(Ok(()));
        assert_eq!(ops.get_state().expect("get_state"), 0);

        let rx_resync = ops.enqueue_set_state(4).expect("enqueue_set_state(4)");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        let (enable, responder) = next_enable_request(&mut exec, &mut stream1);
        assert!(enable);
        responder.send(Ok(())).expect("send Enable(true) response");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(rx_resync.try_recv(), Ok(Ok(())));
        assert_eq!(ops.get_state().expect("get_state"), 4);

        // 3. Peer closure mid-RPC (`Err(None)`) when disabling charging re-queues the request until
        // a replacement proxy connects.
        let rx_disable = ops.enqueue_set_state(8).expect("enqueue_set_state(8)");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        let (enable, responder) = next_enable_request(&mut exec, &mut stream1);
        assert!(!enable);
        drop(responder);
        drop(stream1);
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(rx_disable.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));

        let (proxy2, mut stream2) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::ChargerMarker>();
        ops.connect(proxy2);
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        let (enable, responder) = next_enable_request(&mut exec, &mut stream2);
        assert!(!enable);
        responder.send(Ok(())).expect("send Enable(false) response");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(rx_disable.try_recv(), Ok(Ok(())));
        assert_eq!(ops.get_state().expect("get_state"), 8);

        // 4. Peer closure mid-RPC when enabling charging (`< max_charge_level`) succeeds
        // immediately without waiting for another proxy.
        let rx_enable = ops.enqueue_set_state(2).expect("enqueue_set_state(2)");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        let (enable, responder) = next_enable_request(&mut exec, &mut stream2);
        assert!(enable);
        drop(responder);
        drop(stream2);
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(rx_enable.try_recv(), Ok(Ok(())));
        assert_eq!(ops.get_state().expect("get_state"), 2);
    }

    #[::fuchsia::test]
    fn fcc_concurrent_set_state_coalesces_waiters_with_per_waiter_deadlines() {
        let mut exec = fasync::TestExecutor::new_with_fake_time();
        let (ops, worker) = FccCoolingOps::new(8, /* charge_level = */ 0);
        let mut worker = std::pin::pin!(worker);

        // 1. Superseding a pending `set_state(8)` with `set_state(4)` while disconnected resolves
        // both waiters immediately with `Ok(())` and updates `charge_level` to `4`.
        let caller_disable = ops.enqueue_set_state(8).expect("enqueue_set_state(8)");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(caller_disable.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));

        let caller_enable = ops.enqueue_set_state(4).expect("enqueue_set_state(4)");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(caller_disable.try_recv(), Ok(Ok(())));
        assert_eq!(caller_enable.try_recv(), Ok(Ok(())));
        assert_eq!(ops.get_state().expect("get_state"), 4);

        // 2. Start `caller1` at t=0s and `caller2` + `caller3` at t=2s while disconnected so all
        // three coalesce into `PendingSetState::waiters` with per-waiter deadlines.
        let caller1 = ops.enqueue_set_state(8).expect("caller1 enqueue_set_state(8)");
        assert!(exec.run_until_stalled(&mut worker).is_pending());

        exec.set_fake_time(exec.now() + zx::MonotonicDuration::from_seconds(2));
        let caller2 = ops.enqueue_set_state(8).expect("caller2 enqueue_set_state(8)");
        let caller3 = ops.enqueue_set_state(8).expect("caller3 enqueue_set_state(8)");
        assert!(exec.run_until_stalled(&mut worker).is_pending());

        // Advance to t=5s (`CHARGER_SYNC_TIMEOUT`): `caller1`'s deadline expires with `ENODEV`,
        // while `caller2` and `caller3` remain alive until t=7s.
        exec.set_fake_time(exec.now() + zx::MonotonicDuration::from_seconds(3));
        assert!(exec.wake_expired_timers());
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(caller1.try_recv(), Ok(error!(ENODEV)));
        assert_eq!(caller2.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));
        assert_eq!(caller3.try_recv(), Err(std::sync::mpsc::TryRecvError::Empty));

        // Connecting a charger before t=7s sends a single `Enable(false)` call and wakes both
        // remaining live waiters (`caller2` and `caller3`).
        let (proxy, mut stream) =
            fidl::endpoints::create_proxy_and_stream::<fbattery::ChargerMarker>();
        ops.connect(proxy);
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        let (enable, responder) = next_enable_request(&mut exec, &mut stream);
        assert!(!enable);
        responder.send(Ok(())).expect("send Enable(false) response");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(caller2.try_recv(), Ok(Ok(())));
        assert_eq!(caller3.try_recv(), Ok(Ok(())));
        assert_eq!(ops.get_state().expect("get_state"), 8);

        // 3. Queuing `set_state(6)` followed by `set_state(2)` before the worker runs coalesces
        // both into a single `Enable(true)` transition that records the latest target state (`2`).
        let caller_6 = ops.enqueue_set_state(6).expect("enqueue_set_state(6)");
        let caller_2 = ops.enqueue_set_state(2).expect("enqueue_set_state(2)");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        let (enable, responder) = next_enable_request(&mut exec, &mut stream);
        assert!(enable);
        responder.send(Ok(())).expect("send Enable(true) response");
        assert!(exec.run_until_stalled(&mut worker).is_pending());
        assert_eq!(caller_6.try_recv(), Ok(Ok(())));
        assert_eq!(caller_2.try_recv(), Ok(Ok(())));
        assert_eq!(ops.get_state().expect("get_state"), 2);
    }
}
