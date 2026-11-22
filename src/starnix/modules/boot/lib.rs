// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "512"]

use anyhow::{Context, Error, bail};
use fidl_fuchsia_power_cpu_manager::BoostMarker;
use fidl_fuchsia_sys2 as fsys;
use fuchsia_inspect::Property;
use starnix_core::device::DeviceOps;
use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
use starnix_core::task::{
    ArgNameAndValue, CurrentTask, Kernel, LockupDetectorReceiver, ThreadLockupDetector,
};
use starnix_core::vfs::buffers::{InputBuffer, OutputBuffer};
use starnix_core::vfs::{
    CloseFreeSafe, FileObject, FileOps, NamespaceNode, fileops_impl_nonseekable,
    fileops_impl_noop_sync,
};
use starnix_logging::{log_error, log_info, log_warn};
use starnix_sync::{BootedLock, LockDepMutex};
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::error;
use starnix_uapi::errors::Errno;
use starnix_uapi::open_flags::OpenFlags;
use std::collections::HashSet;
use std::sync::Arc;
use std::sync::mpsc::Sender;

fn parse_args(args: &[&str]) -> Result<Option<zx::MonotonicDuration>, Error> {
    let mut cpu_boost_secs: Option<u32> = None;
    for &arg in args {
        match ArgNameAndValue::from(arg) {
            ArgNameAndValue { name: "cpu_boost_secs", value: Some(value) } => {
                cpu_boost_secs = Some(value.parse().context("invalid cpu_boost_secs value")?);
            }
            _ => bail!("invalid boot_notifier argument: {arg}"),
        }
    }
    Ok(cpu_boost_secs.map(|secs| zx::MonotonicDuration::from_seconds(secs.into())))
}

/// Validates `boot_notifier` module configuration `args`.
pub fn validate_args(args: &[&str]) -> Result<(), Error> {
    parse_args(args).map(|_| ())
}

/// Initializes the boot notifier device from module configuration `args`.
pub fn module_init(kernel: &Kernel, args: &[&str]) -> Result<(), Error> {
    let cpu_boost_duration = parse_args(args)?;
    let (booted_sender, booted_receiver) = ThreadLockupDetector::tracked_channel::<BootPhase>();
    let node = fuchsia_inspect::component::inspector().root().create_child("boot");
    let device = BootedDevice::new(kernel, booted_sender, node, cpu_boost_duration)?;
    device.clone().register(kernel);
    device.start_relay(kernel, booted_receiver);
    Ok(())
}

/// A guest OS boot phase transition written to `/dev/booted` as `"<tag>; <description>"`.
#[derive(Clone, Debug, PartialEq, Eq)]
struct BootPhase {
    tag: String,
    description: String,
    timestamp: zx::BootInstant,
}

#[derive(Clone)]
struct BootedDevice {
    inner: Arc<Inner>,
}

const CURRENT_PHASE_INSPECT_KEY: &str = "current_phase";
const PRELIMINARY_PHASE_TAG: &str = "waiting_boot";
const SYSTEM_BOOT_TAG: &str = "system_boot";
const MAX_BOOT_PHASES: usize = 16;
const MAX_TAG_LEN: usize = 64;
const MAX_DESCRIPTION_LEN: usize = 256;

trait BootNotifier: Send + Sync {
    fn notify(&self) -> Result<(), Error>;
}

struct SystemBootNotifier;

impl BootNotifier for SystemBootNotifier {
    fn notify(&self) -> Result<(), Error> {
        let client =
            fuchsia_component::client::connect_to_protocol_sync::<fsys::BootControllerMarker>()
                .context("connecting to BootController")?;
        client.notify(zx::MonotonicInstant::INFINITE).context("calling BootController/Notify")?;
        Ok(())
    }
}

impl BootedDevice {
    pub fn new(
        kernel: &Kernel,
        booted_sender: Sender<BootPhase>,
        inspect_node: fuchsia_inspect::Node,
        cpu_boost_duration: Option<zx::MonotonicDuration>,
    ) -> Result<Self, anyhow::Error> {
        if let Some(duration) = cpu_boost_duration {
            match kernel.connect_to_protocol_at_container_svc::<BoostMarker>() {
                Ok(client_end) => {
                    log_info!("Enabling boot-time CPU boost");
                    let booster = client_end.into_proxy();
                    kernel.kthreads.spawn_future(
                        move || async move {
                            let token = match booster.boost().await {
                                Ok(Ok(token)) => token,
                                e => {
                                    log_warn!(e:?; "Failed to enable boot-time CPU boost");
                                    return;
                                }
                            };
                            fuchsia_async::Timer::new(zx::MonotonicInstant::after(duration)).await;
                            log_info!("Disabling boot-time CPU boost");
                            drop(token);
                        },
                        "boot_cpu_boost",
                    );
                }
                Err(e) => {
                    log_warn!(e:?; "Failed to connect to cpu boost protocol");
                }
            }
        }

        Ok(Self { inner: Arc::new(Inner::new(booted_sender, inspect_node, SystemBootNotifier)) })
    }

    pub fn register(self, kernel: &Kernel) {
        let registry = &kernel.device_registry;
        registry
            .add_dyn_device(kernel, "booted".into(), registry.objects.starnix_class(), self)
            .expect("can add booted device");
    }

    pub fn start_relay(&self, kernel: &Kernel, booted_receiver: LockupDetectorReceiver<BootPhase>) {
        let this = self.inner.clone();
        let closure = move |_current_task: &CurrentTask| {
            while let Ok(event) = booted_receiver.recv() {
                this.on_boot_phase(&event);
            }
            log_error!("booted relay was terminated unexpectedly.");
        };
        let req = SpawnRequestBuilder::new()
            .with_debug_name("boot-notifier-relay")
            .with_sync_closure(closure)
            .build();

        kernel.kthreads.spawner().spawn_from_request(req);
    }
}

struct Inner<N: BootNotifier = SystemBootNotifier> {
    file: Arc<File>,
    inspect_node: fuchsia_inspect::Node,
    current_phase: fuchsia_inspect::StringProperty,
    boot_notifier: N,
}

impl<N: BootNotifier> Inner<N> {
    fn new(
        booted_sender: Sender<BootPhase>,
        inspect_node: fuchsia_inspect::Node,
        boot_notifier: N,
    ) -> Self {
        let current_phase =
            inspect_node.create_string(CURRENT_PHASE_INSPECT_KEY, PRELIMINARY_PHASE_TAG);
        Self { file: File::new(booted_sender), inspect_node, current_phase, boot_notifier }
    }

    /// Processes a validated, deduplicated boot phase transition from `File::write_phase`.
    fn on_boot_phase(&self, event: &BootPhase) {
        log_info!("Boot phase '{}' completed: {}", event.tag, event.description);

        self.current_phase.set(&event.tag);

        let timestamp = event.timestamp.into_nanos() as u64;
        self.inspect_node.record_uint(format!("{}_timestamp", event.tag), timestamp);

        if event.tag == SYSTEM_BOOT_TAG {
            match self.boot_notifier.notify() {
                Ok(()) => log_info!(
                    "Notified Fuchsia BootController of boot completion at phase '{}'",
                    event.tag
                ),
                Err(e) => log_error!(
                    e:?;
                    "Failed to notify Fuchsia BootController of boot completion at phase '{}'",
                    event.tag
                ),
            }
        }
    }
}

struct File {
    /// Tracks the set of boot phase tags that have been reported (capped at `MAX_BOOT_PHASES`).
    /// Non-empty once any valid boot phase has been reported.
    seen_tags: LockDepMutex<HashSet<String>, BootedLock>,
    sender: Sender<BootPhase>,
}

impl File {
    fn new(sender: Sender<BootPhase>) -> Arc<Self> {
        Arc::new(Self { seen_tags: HashSet::new().into(), sender })
    }

    fn is_booted(&self) -> bool {
        !self.seen_tags.lock().is_empty()
    }

    fn write_phase(&self, content: &[u8]) -> Result<usize, Errno> {
        let timestamp = zx::BootInstant::get();
        let event = parse_boot_phase(content, timestamp)?;
        let mut seen_tags = self.seen_tags.lock();
        if seen_tags.contains(&event.tag) {
            log_warn!("Duplicate /dev/booted phase tag {:?}; rejecting write", event.tag);
            return error!(EEXIST);
        }
        if seen_tags.len() >= MAX_BOOT_PHASES {
            log_warn!(
                "Maximum /dev/booted phases ({MAX_BOOT_PHASES}) reached; rejecting tag {:?}",
                event.tag
            );
            return error!(ENOSPC);
        }
        let tag = event.tag.clone();
        if let Err(e) = self.sender.send(event) {
            log_error!("unable to send recent booted state to device relay: {:?}", e);
            return error!(EIO);
        }
        seen_tags.insert(tag);
        Ok(content.len())
    }
}

/// `TouchPowerPolicyFile` doesn't implement the `close` method.
impl CloseFreeSafe for File {}
impl FileOps for File {
    fileops_impl_nonseekable!();
    fileops_impl_noop_sync!();

    fn read(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        debug_assert!(offset == 0);
        let booted = self.is_booted();
        data.write_all(&[u8::from(booted)])
    }

    fn write(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let content = data.read_all()?;
        self.write_phase(&content)
    }
}

/// Parses the boot phase that was written to `/dev/booted` from userspace and pairs it with
/// `timestamp`. The expected format is `"<tag>; <description>"`, such as
/// `"system_boot; Android sys.boot_completed=1 (Device Encrypted storage unlocked)"`.
fn parse_boot_phase(content: &[u8], timestamp: zx::BootInstant) -> Result<BootPhase, Errno> {
    let Ok(s) = std::str::from_utf8(content) else {
        log_warn!("Invalid /dev/booted value: not valid UTF-8");
        return error!(EINVAL);
    };
    let trimmed = s.trim();

    let mut parts = trimmed.split(';');
    let (Some(tag), Some(description), None) = (parts.next(), parts.next(), parts.next()) else {
        log_warn!("Invalid /dev/booted value {trimmed:?} - expected '<tag>; <description>'");
        return error!(EINVAL);
    };

    let tag = tag.trim();
    let description = description.trim();

    if tag.is_empty()
        || tag.len() > MAX_TAG_LEN
        || !tag.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        || description.is_empty()
        || description.len() > MAX_DESCRIPTION_LEN
    {
        log_warn!(
            "Invalid /dev/booted tag ({tag:?}) or description ({description:?}) - tag must be a non-empty underscore-delimited identifier (<= {MAX_TAG_LEN} bytes) and description must be non-empty (<= {MAX_DESCRIPTION_LEN} bytes)"
        );
        return error!(EINVAL);
    }

    Ok(BootPhase { tag: tag.to_string(), description: description.to_string(), timestamp })
}

impl DeviceOps for BootedDevice {
    fn open(
        &self,
        _current_task: &CurrentTask,
        _devt: DeviceId,
        _node: &NamespaceNode,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let file = self.inner.file.clone();
        Ok(Box::new(file))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use diagnostics_assertions::{AnyProperty, assert_data_tree};
    use starnix_uapi::errno;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[derive(Default)]
    struct MockBootNotifier {
        notify_count: AtomicUsize,
    }

    impl BootNotifier for Arc<MockBootNotifier> {
        fn notify(&self) -> Result<(), Error> {
            self.notify_count.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
    }

    #[::fuchsia::test]
    fn test_parse_boot_phase_valid() {
        let timestamp = zx::BootInstant::from_nanos(12345);
        assert_eq!(
            parse_boot_phase(
                b"system_boot; Android sys.boot_completed=1 (Device Encrypted storage unlocked)",
                timestamp,
            ),
            Ok(BootPhase {
                tag: "system_boot".to_string(),
                description: "Android sys.boot_completed=1 (Device Encrypted storage unlocked)"
                    .to_string(),
                timestamp,
            })
        );
        assert_eq!(
            parse_boot_phase(
                b"ce_unlocked; Android sys.user.0.ce_available=true (Credential Encrypted storage unlocked)\n",
                timestamp,
            ),
            Ok(BootPhase {
                tag: "ce_unlocked".to_string(),
                description:
                    "Android sys.user.0.ce_available=true (Credential Encrypted storage unlocked)"
                        .to_string(),
                timestamp,
            })
        );
        assert_eq!(
            parse_boot_phase(b"  custom_tag_123 ; Custom phase description \n", timestamp),
            Ok(BootPhase {
                tag: "custom_tag_123".to_string(),
                description: "Custom phase description".to_string(),
                timestamp,
            })
        );
    }

    #[::fuchsia::test]
    fn test_parse_boot_phase_invalid() {
        let oversized_tag = format!("{}; desc", "a".repeat(MAX_TAG_LEN + 1));
        let oversized_desc = format!("system_boot; {}", "a".repeat(MAX_DESCRIPTION_LEN + 1));
        for invalid in [
            &b""[..],
            b"   \n",
            &[0xff, 0xfe],
            b"0",
            b"0\n",
            b"1",
            b"1\n",
            b"  1 \n",
            b"2",
            b"2\n",
            b"system_boot",
            b"; desc",
            b"system_boot; ",
            b"system_boot; desc; extra",
            b"1; system_boot; desc",
            b"bad-tag; desc",
            b"bad.tag; desc",
            b"bad/tag; desc",
            oversized_tag.as_bytes(),
            oversized_desc.as_bytes(),
        ] {
            assert_eq!(
                parse_boot_phase(invalid, zx::BootInstant::ZERO),
                Err(errno!(EINVAL)),
                "expected EINVAL for input {invalid:?}"
            );
        }
    }

    #[::fuchsia::test]
    async fn test_on_boot_phase_and_inspect() {
        let inspector = fuchsia_inspect::Inspector::default();
        let inspect_node = inspector.root().create_child("boot");
        let (sender, receiver) = std::sync::mpsc::channel();
        let boot_notifier = Arc::new(MockBootNotifier::default());
        let inner = Inner::new(sender, inspect_node, Arc::clone(&boot_notifier));

        let write_and_process = |input: &[u8]| -> Result<usize, Errno> {
            let written = inner.file.write_phase(input)?;
            while let Ok(event) = receiver.try_recv() {
                inner.on_boot_phase(&event);
            }
            Ok(written)
        };

        assert!(!inner.file.is_booted());
        assert_eq!(boot_notifier.notify_count.load(Ordering::Relaxed), 0);
        assert_data_tree!(inspector, root: {
            boot: {
                current_phase: "waiting_boot",
            }
        });

        // Phases may arrive in any order (e.g. ce_unlocked before system_boot on uncredentialed
        // devices).
        let ce_unlocked =
            b"ce_unlocked; Android sys.user.0.ce_available=true (Credential Encrypted storage unlocked)";
        assert_eq!(write_and_process(ce_unlocked), Ok(ce_unlocked.len()));
        assert!(inner.file.is_booted());
        assert_eq!(boot_notifier.notify_count.load(Ordering::Relaxed), 0);
        assert_data_tree!(inspector, root: {
            boot: {
                current_phase: "ce_unlocked",
                ce_unlocked_timestamp: AnyProperty,
            }
        });

        let system_boot =
            b"system_boot; Android sys.boot_completed=1 (Device Encrypted storage unlocked)";
        assert_eq!(write_and_process(system_boot), Ok(system_boot.len()));
        assert_eq!(boot_notifier.notify_count.load(Ordering::Relaxed), 1);
        assert_data_tree!(inspector, root: {
            boot: {
                current_phase: "system_boot",
                ce_unlocked_timestamp: AnyProperty,
                system_boot_timestamp: AnyProperty,
            }
        });

        // Duplicate tags are rejected without recording duplicate Inspect properties or
        // re-notifying BootController.
        assert_eq!(write_and_process(b"system_boot; Duplicate system boot"), Err(errno!(EEXIST)));
        assert_eq!(write_and_process(ce_unlocked), Err(errno!(EEXIST)));
        assert!(receiver.try_recv().is_err());
        assert_eq!(boot_notifier.notify_count.load(Ordering::Relaxed), 1);
        assert_data_tree!(inspector, root: {
            boot: {
                current_phase: "system_boot",
                ce_unlocked_timestamp: AnyProperty,
                system_boot_timestamp: AnyProperty,
            }
        });

        // Additional custom tags record their own timestamp and update current_phase.
        // Verify that the Inspect timestamp reflects when `write_phase` captured it rather than
        // when the relay thread later runs `on_boot_phase`.
        let before_write = zx::BootInstant::get();
        let custom_stage = b"custom_stage; Custom stage completed";
        assert_eq!(inner.file.write_phase(custom_stage), Ok(custom_stage.len()));
        let after_write = zx::BootInstant::get();

        let custom_event = receiver.try_recv().expect("expected custom_stage event");
        assert!(custom_event.timestamp >= before_write && custom_event.timestamp <= after_write);
        let expected_custom_timestamp = custom_event.timestamp.into_nanos() as u64;

        inner.on_boot_phase(&custom_event);
        assert_eq!(boot_notifier.notify_count.load(Ordering::Relaxed), 1);
        assert_data_tree!(inspector, root: {
            boot: {
                current_phase: "custom_stage",
                ce_unlocked_timestamp: AnyProperty,
                system_boot_timestamp: AnyProperty,
                custom_stage_timestamp: expected_custom_timestamp,
            }
        });
    }

    #[::fuchsia::test]
    fn test_max_boot_phases_limit() {
        let inspector = fuchsia_inspect::Inspector::default();
        let inspect_node = inspector.root().create_child("boot");
        let (sender, receiver) = std::sync::mpsc::channel();
        let boot_notifier = Arc::new(MockBootNotifier::default());
        let inner = Inner::new(sender, inspect_node, Arc::clone(&boot_notifier));

        assert_eq!(inner.file.write_phase(b"invalid"), Err(errno!(EINVAL)));
        assert!(!inner.file.is_booted());

        for i in 0..MAX_BOOT_PHASES {
            let input = format!("phase_{i}; Phase {i} description");
            assert_eq!(inner.file.write_phase(input.as_bytes()), Ok(input.len()));
            let event = receiver.try_recv().expect("expected phase event");
            inner.on_boot_phase(&event);
        }
        assert!(inner.file.is_booted());

        let overflow = b"overflow_phase; Exceeds MAX_BOOT_PHASES";
        assert_eq!(inner.file.write_phase(overflow), Err(errno!(ENOSPC)));
        assert!(receiver.try_recv().is_err());
    }
}
