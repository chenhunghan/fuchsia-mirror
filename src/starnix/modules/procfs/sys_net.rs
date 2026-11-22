// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use std::sync::atomic::Ordering;

use crate::sysctl_directory::SysctlDirectory;
use fidl::endpoints::DiscoverableProtocolMarker as _;
use fidl_fuchsia_net_interfaces_admin as fnet_interfaces_admin;
use fidl_fuchsia_net_root as fnet_root;
use fidl_fuchsia_net_settings as fnet_settings;
use fuchsia_component::client::connect_to_protocol_sync;
use fuchsia_rcu::RcuReadScope;
use net_types::ip::{Ip, IpVersion, Ipv4, Ipv6};
use netlink::{SysctlError, SysctlInterfaceSelector};
use starnix_core::task::CurrentTask;
use starnix_core::vfs::pseudo::simple_directory::SimpleDirectory;
use starnix_core::vfs::pseudo::simple_file::{
    BytesFile, BytesFileOps, SimpleFileNode, parse_i32_file, serialize_for_file,
};
use starnix_core::vfs::pseudo::stub_bytes_file::StubBytesFile;
use starnix_core::vfs::{
    DirEntry, DirEntryOps, DirectoryEntryType, DirentSink, FileObject, FileOps, FileSystemHandle,
    FsNode, FsNodeHandle, FsNodeInfo, FsNodeOps, FsStr, emit_dotdot, fileops_impl_directory,
    fileops_impl_noop_sync, fileops_impl_unbounded_seek, fs_node_impl_dir_readonly,
};
use starnix_logging::{bug_ref, log_error, log_warn, track_stub};

use starnix_uapi::auth::FsCred;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::{FileMode, mode};
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::vfs::FdEvents;
use starnix_uapi::{errno, error, uapi};
use std::borrow::Cow;
use std::num::{NonZeroU32, NonZeroU64};
use std::sync::Arc;

const FILE_MODE: FileMode = mode!(IFREG, 0o644);

fn netstack_devices_readdir(
    file: &FileObject,
    current_task: &CurrentTask,
    sink: &mut dyn DirentSink,
) -> Result<(), Errno> {
    file.blocking_op(current_task, FdEvents::empty(), None, || {
        let (initialized, _) = &current_task.kernel().netstack_devices.initialized_and_wq;
        if !initialized.load(Ordering::SeqCst) {
            // Kick off the initialization of the netlink worker if not yet.
            let _ = current_task.kernel().network_netlink();
            return error!(EAGAIN);
        }
        emit_dotdot(file, sink)?;

        if sink.offset() == 2 {
            sink.add(
                file.fs.allocate_ino(),
                sink.offset() + 1,
                DirectoryEntryType::from_mode(FILE_MODE),
                "all".into(),
            )?;
        }

        if sink.offset() == 3 {
            sink.add(
                file.fs.allocate_ino(),
                sink.offset() + 1,
                DirectoryEntryType::from_mode(FILE_MODE),
                "default".into(),
            )?;
        }

        let devices = current_task.kernel().netstack_devices.snapshot_devices();
        for (name, _) in devices.iter().skip(sink.offset() as usize - 4) {
            let inode_num = file.fs.allocate_ino();
            sink.add(
                inode_num,
                sink.offset() + 1,
                DirectoryEntryType::from_mode(FILE_MODE),
                name.as_ref(),
            )?;
        }
        Ok(())
    })
}

macro_rules! fileops_impl_netstack_devices {
    () => {
        fn readdir(
            &self,
            file: &FileObject,
            current_task: &CurrentTask,
            sink: &mut dyn DirentSink,
        ) -> Result<(), Errno> {
            netstack_devices_readdir(file, current_task, sink)
        }

        fn wait_async(
            &self,
            _file: &FileObject,
            current_task: &CurrentTask,
            waiter: &starnix_core::task::Waiter,
            _events: FdEvents,
            _handler: starnix_core::task::EventHandler,
        ) -> Option<starnix_core::task::WaitCanceler> {
            let (_initialized, wq) = &current_task.kernel().netstack_devices.initialized_and_wq;
            Some(wq.wait_async(waiter))
        }
    };
}

fn get_netstack_device(
    current_task: &CurrentTask,
    name: &FsStr,
) -> Option<SysctlInterfaceSelector> {
    let (initialized, _) = &current_task.kernel().netstack_devices.initialized_and_wq;
    if !initialized.load(Ordering::SeqCst) {
        // Kick off lazy initialization of the netlink worker (matching `netstack_devices_readdir`),
        // while allowing unit tests that mark `initialized_and_wq` initialized to exercise lookup
        // and revalidation without spawning a live netlink worker.
        let _ = current_task.kernel().network_netlink();
    }
    // Per https://www.kernel.org/doc/Documentation/networking/ip-sysctl.txt,
    //
    //   conf/default/*:
    //	   Change the interface-specific default settings.
    //
    //   conf/all/*:
    //	   Change all the interface-specific settings.
    //
    // Note that the all/default directories don't exist in `/sys/class/net`.
    if name == "all" {
        return Some(SysctlInterfaceSelector::All);
    }
    if name == "default" {
        return Some(SysctlInterfaceSelector::Default);
    }
    if let Some(dev) = current_task.kernel().netstack_devices.get_device(name) {
        return Some(SysctlInterfaceSelector::Id(dev.interface_id));
    }
    None
}

fn is_netstack_device_valid(
    current_task: &CurrentTask,
    iface_entry: &DirEntry,
    interface_id: NonZeroU64,
) -> bool {
    let scope = RcuReadScope::new();
    current_task
        .kernel()
        .netstack_devices
        .get_device(iface_entry.local_name(&scope))
        .is_some_and(|dev| dev.interface_id == interface_id)
}

/// Revalidates a `/proc/sys/net/ipv{4,6}/{conf,neigh}/<iface>` entry against the netstack
/// interface ID it was created for, so removed or recreated interfaces are evicted.
struct NetstackDeviceDirEntryOps {
    interface_id: NonZeroU64,
}

impl DirEntryOps for NetstackDeviceDirEntryOps {
    fn revalidate(&self, current_task: &CurrentTask, dir_entry: &DirEntry) -> Result<bool, Errno> {
        Ok(is_netstack_device_valid(current_task, dir_entry, self.interface_id))
    }
}

/// Revalidates children of a `/proc/sys/net/ipv{4,6}/{conf,neigh}/<iface>` directory against both
/// the parent `<iface>` directory's interface ID and the underlying [`SimpleDirectory`].
struct NetstackDeviceChildDirEntryOps {
    dir: Arc<SimpleDirectory>,
    interface_id: NonZeroU64,
}

impl DirEntryOps for NetstackDeviceChildDirEntryOps {
    fn revalidate(&self, current_task: &CurrentTask, dir_entry: &DirEntry) -> Result<bool, Errno> {
        let Some(parent) = dir_entry.parent() else {
            return Ok(false);
        };
        if !is_netstack_device_valid(current_task, &parent, self.interface_id) {
            return Ok(false);
        }
        <Arc<SimpleDirectory> as DirEntryOps>::revalidate(&self.dir, current_task, dir_entry)
    }
}

/// Wraps a per-interface [`SimpleDirectory`] so revalidation, child lookups, and `readdir` all
/// fail once `<iface>` is removed or recreated with a different interface ID.
#[derive(Clone)]
struct NetstackDeviceDirectory {
    dir: Arc<SimpleDirectory>,
    interface_id: NonZeroU64,
}

impl FsNodeOps for NetstackDeviceDirectory {
    fs_node_impl_dir_readonly!();

    // `DirEntry::new` prefers the parent's `create_child_dir_entry_ops()` over these ops. This hook
    // only takes effect because the `ProcSysNet*` parent directories don't implement
    // `create_child_dir_entry_ops`; if they ever do, they must provide equivalent revalidation.
    fn create_dir_entry_ops(&self) -> Box<dyn DirEntryOps> {
        Box::new(NetstackDeviceDirEntryOps { interface_id: self.interface_id })
    }

    fn create_child_dir_entry_ops(&self) -> Option<Box<dyn DirEntryOps>> {
        Some(Box::new(NetstackDeviceChildDirEntryOps {
            dir: self.dir.clone(),
            interface_id: self.interface_id,
        }))
    }

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(self.clone()))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        if !is_netstack_device_valid(current_task, entry, self.interface_id) {
            return error!(ENOENT);
        }
        FsNodeOps::lookup(&self.dir, entry, current_task, name)
    }
}

impl FileOps for NetstackDeviceDirectory {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();

    fn readdir(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        if !is_netstack_device_valid(current_task, &file.name.entry, self.interface_id) {
            return error!(ENOENT);
        }
        self.dir.readdir(file, current_task, sink)
    }
}

/// Converts `dir` into an [`FsNodeHandle`], wrapping per-interface directories in
/// [`NetstackDeviceDirectory`] while leaving `"all"` and `"default"` as plain [`SimpleDirectory`]
/// nodes.
fn into_netstack_device_dir_node(
    fs: &FileSystemHandle,
    dir: SysctlDirectory<{ uapi::CAP_NET_ADMIN }>,
    interface: SysctlInterfaceSelector,
) -> FsNodeHandle {
    match interface {
        SysctlInterfaceSelector::All | SysctlInterfaceSelector::Default => dir.into_node(fs, 0o555),
        SysctlInterfaceSelector::Id(interface_id) => fs.create_node_and_allocate_node_id(
            NetstackDeviceDirectory { dir: dir.into_inner(), interface_id },
            FsNodeInfo::new(mode!(IFDIR, 0o555), FsCred::root()),
        ),
    }
}

#[derive(Clone)]
pub struct ProcSysNetIpv4Conf;

impl FsNodeOps for ProcSysNetIpv4Conf {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(self.clone()))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        if let Some(interface) = get_netstack_device(current_task, name) {
            let fs = entry.node.fs();
            let dir = SysctlDirectory::<{ uapi::CAP_NET_ADMIN }>::new();
            dir.edit(&fs, |dir| {
                dir.entry(
                    "accept_redirects",
                    StubBytesFile::new_node(bug_ref!("https://fxbug.dev/423646442")),
                    FILE_MODE,
                );
            });
            return Ok(into_netstack_device_dir_node(&fs, dir, interface));
        }
        error!(ENOENT, "looking for {name}")
    }
}

impl FileOps for ProcSysNetIpv4Conf {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();
    fileops_impl_netstack_devices!();
}

#[derive(Clone)]
pub struct ProcSysNetIpv4Neigh;

impl FsNodeOps for ProcSysNetIpv4Neigh {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(self.clone()))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        if let Some(interface) = get_netstack_device(current_task, name) {
            let fs = entry.node.fs();
            let dir = SysctlDirectory::<{ uapi::CAP_NET_ADMIN }>::new();
            dir.edit(&fs, |dir| {
                dir.entry(
                    "ucast_solicit",
                    new_interface_config_file_node::<UcastSolicit<Ipv4>>(interface),
                    FILE_MODE,
                );
                dir.entry(
                    "retrans_time_ms",
                    new_interface_config_file_node::<RetransTimeMs<Ipv4>>(interface),
                    FILE_MODE,
                );
                dir.entry(
                    "mcast_resolicit",
                    new_interface_config_file_node::<McastResolicit<Ipv4>>(interface),
                    FILE_MODE,
                );
                dir.entry(
                    "base_reachable_time_ms",
                    new_interface_config_file_node::<BaseReachableTimeMs<Ipv4>>(interface),
                    FILE_MODE,
                );
            });
            return Ok(into_netstack_device_dir_node(&fs, dir, interface));
        }
        error!(ENOENT, "looking for {name}")
    }
}

impl FileOps for ProcSysNetIpv4Neigh {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();
    fileops_impl_netstack_devices!();
}

#[derive(Clone)]
pub struct ProcSysNetIpv6Conf;

impl FsNodeOps for ProcSysNetIpv6Conf {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(self.clone()))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        if let Some(interface) = get_netstack_device(current_task, name) {
            let fs = entry.node.fs();
            let dir = SysctlDirectory::<{ uapi::CAP_NET_ADMIN }>::new();
            dir.edit(&fs, |dir| {
                dir.entry(
                    "accept_ra",
                    StubBytesFile::new_node(bug_ref!("https://fxbug.dev/423646365")),
                    FILE_MODE,
                );
                dir.entry(
                    "accept_ra_defrtr",
                    new_interface_config_file_node::<AcceptRaDefrtr>(interface),
                    FILE_MODE,
                );
                dir.entry(
                    "accept_ra_info_min_plen",
                    StubBytesFile::new_node(bug_ref!("https://fxbug.dev/423645816")),
                    FILE_MODE,
                );
                dir.entry(
                    "accept_ra_rt_info_min_plen",
                    StubBytesFile::new_node(bug_ref!("https://fxbug.dev/322908046")),
                    FILE_MODE,
                );
                dir.entry(
                    "accept_ra_rt_table",
                    NetworkNetlinkSysctlFile::new_node(interface),
                    FILE_MODE,
                );
                dir.entry(
                    "accept_redirects",
                    StubBytesFile::new_node(bug_ref!("https://fxbug.dev/423646442")),
                    FILE_MODE,
                );
                dir.entry(
                    "dad_transmits",
                    new_interface_config_file_node::<Ipv6DadTransmits>(interface),
                    FILE_MODE,
                );
                dir.entry(
                    "use_tempaddr",
                    new_interface_config_file_node::<UseTempAddr>(interface),
                    FILE_MODE,
                );
                dir.entry(
                    "addr_gen_mode",
                    StubBytesFile::new_node(bug_ref!("https://fxbug.dev/423645864")),
                    FILE_MODE,
                );
                dir.entry(
                    "stable_secret",
                    StubBytesFile::new_node(bug_ref!("https://fxbug.dev/423646722")),
                    FILE_MODE,
                );
                dir.entry(
                    "disable_ipv6",
                    new_interface_config_file_node::<DisableIpv6>(interface),
                    FILE_MODE,
                );
                dir.entry(
                    "optimistic_dad",
                    StubBytesFile::new_node(bug_ref!("https://fxbug.dev/423646584")),
                    FILE_MODE,
                );
                dir.entry(
                    "use_oif_addrs_only",
                    StubBytesFile::new_node(bug_ref!("https://fxbug.dev/423645421")),
                    FILE_MODE,
                );
                dir.entry(
                    "use_optimistic",
                    StubBytesFile::new_node(bug_ref!("https://fxbug.dev/423645883")),
                    FILE_MODE,
                );
                dir.entry(
                    "forwarding",
                    StubBytesFile::new_node(bug_ref!("https://fxbug.dev/322907925")),
                    FILE_MODE,
                );
            });
            return Ok(into_netstack_device_dir_node(&fs, dir, interface));
        }
        error!(ENOENT, "looking for {name}")
    }
}

impl FileOps for ProcSysNetIpv6Conf {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();
    fileops_impl_netstack_devices!();
}

#[derive(Clone)]
pub struct ProcSysNetIpv6Neigh;

impl FsNodeOps for ProcSysNetIpv6Neigh {
    fs_node_impl_dir_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        Ok(Box::new(self.clone()))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        if let Some(interface) = get_netstack_device(current_task, name) {
            let fs = entry.node.fs();
            let dir = SysctlDirectory::<{ uapi::CAP_NET_ADMIN }>::new();
            dir.edit(&fs, |dir| {
                dir.entry(
                    "ucast_solicit",
                    new_interface_config_file_node::<UcastSolicit<Ipv6>>(interface),
                    FILE_MODE,
                );
                dir.entry(
                    "retrans_time_ms",
                    new_interface_config_file_node::<RetransTimeMs<Ipv6>>(interface),
                    FILE_MODE,
                );
                dir.entry(
                    "mcast_resolicit",
                    new_interface_config_file_node::<McastResolicit<Ipv6>>(interface),
                    FILE_MODE,
                );
                dir.entry(
                    "base_reachable_time_ms",
                    new_interface_config_file_node::<BaseReachableTimeMs<Ipv6>>(interface),
                    FILE_MODE,
                );
            });
            return Ok(into_netstack_device_dir_node(&fs, dir, interface));
        }
        error!(ENOENT, "looking for {name}")
    }
}

impl FileOps for ProcSysNetIpv6Neigh {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();
    fileops_impl_netstack_devices!();
}

struct NetworkNetlinkSysctlFile {
    interface: SysctlInterfaceSelector,
}

impl NetworkNetlinkSysctlFile {
    fn new_node(interface: SysctlInterfaceSelector) -> impl FsNodeOps {
        SimpleFileNode::new(move |_| Ok(BytesFile::new(Self { interface })))
    }
}

fn to_errno(error: SysctlError) -> Errno {
    match error {
        SysctlError::Disconnected => errno!(EIO),
        SysctlError::NoInterface => errno!(ENODEV),
        SysctlError::Unsupported => errno!(ENOTSUP),
    }
}

impl BytesFileOps for NetworkNetlinkSysctlFile {
    fn write(&self, current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let value = parse_i32_file(&data)?;
        current_task
            .kernel()
            .network_netlink()
            .write_accept_ra_rt_table(self.interface, value)
            .map_err(|err| {
                log_error!("failed to write to {:?}: {:?}", self.interface, err);
                to_errno(err)
            })
    }

    fn read(&self, current_task: &CurrentTask) -> Result<std::borrow::Cow<'_, [u8]>, Errno> {
        let value = current_task
            .kernel()
            .network_netlink()
            .read_accept_ra_rt_table(self.interface)
            .map_err(|err| {
                log_error!("failed to read from {:?}: {:?}", self.interface, err);
                to_errno(err)
            })?;
        Ok(serialize_for_file(value).into())
    }
}

pub struct PingGroupRangeFile;

impl PingGroupRangeFile {
    const MAX_GID: u32 = 4294967294;

    pub fn new_node() -> impl FsNodeOps {
        BytesFile::new_node(Self)
    }
}

impl BytesFileOps for PingGroupRangeFile {
    fn write(&self, current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let mut params = std::str::from_utf8(&data)
            .map_err(|_| errno!(EINVAL))?
            .trim_ascii()
            .split_ascii_whitespace();
        let min = params
            .next()
            .ok_or_else(|| errno!(EINVAL))?
            .parse::<u32>()
            .map_err(|_| errno!(EINVAL))?;
        if min > Self::MAX_GID {
            return error!(EINVAL);
        }

        // Max value is optional.
        let max = match params.next() {
            Some(v) => {
                let v = v.parse::<u32>().map_err(|_| errno!(EINVAL))?;
                if v > Self::MAX_GID {
                    return error!(EINVAL);
                }
                Some(v + 1)
            }
            None => None,
        };

        let mut range = current_task.kernel().system_limits.socket.icmp_ping_gids.lock();
        range.start = min;
        if let Some(max) = max {
            range.end = max;
        }
        if range.is_empty() {
            // Default to "[1, 0]" range (equivalent to "[1, 1)") to match
            // Linux behavior.
            *range = 1..1;
        }

        Ok(())
    }
    fn read(&self, current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let range = current_task.kernel().system_limits.socket.icmp_ping_gids.lock().clone();
        Ok(format!("{}\t{}\n", range.start, range.end - 1).into_bytes().into())
    }
}

trait InterfaceConfig: Sync + Send + 'static {
    fn try_from_i32(value: i32) -> Result<fidl_fuchsia_net_interfaces_admin::Configuration, Errno>;
    fn try_into_i32(config: fidl_fuchsia_net_interfaces_admin::Configuration)
    -> Result<i32, Errno>;
}

fn new_interface_config_file_node<Config>(selector: SysctlInterfaceSelector) -> impl FsNodeOps
where
    Config: InterfaceConfig,
{
    SimpleFileNode::new(move |_| {
        Ok(BytesFile::new(InterfaceConfigFile {
            selector,
            _marker: std::marker::PhantomData::<Config>,
        }))
    })
}

struct InterfaceConfigFile<Config> {
    selector: SysctlInterfaceSelector,
    _marker: std::marker::PhantomData<Config>,
}

impl<Config> BytesFileOps for InterfaceConfigFile<Config>
where
    Config: InterfaceConfig,
{
    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let config = Config::try_from_i32(parse_i32_file(&data)?)?;
        set_interface_config(self.selector, &config)?;
        Ok(())
    }
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let config = Config::try_into_i32(get_interface_config(self.selector)?)?;
        Ok(serialize_for_file::<i32>(config).into())
    }
}

fn set_interface_config(
    selector: SysctlInterfaceSelector,
    config: &fidl_fuchsia_net_interfaces_admin::Configuration,
) -> Result<(), Errno> {
    match selector {
        SysctlInterfaceSelector::All => {
            log_warn!("setting config for all network interfaces is ignored");
            Ok(())
        }
        SysctlInterfaceSelector::Default => {
            let control =
                connect_to_protocol_sync::<fnet_settings::ControlMarker>().map_err(|err| {
                    log_error!(
                        "failed to connect to {}: {:?}",
                        fnet_settings::ControlMarker::PROTOCOL_NAME,
                        err
                    );
                    errno!(EIO)
                })?;
            control
                .update_interface_defaults(config, zx::MonotonicInstant::INFINITE)
                .map_err(|err| {
                    log_error!("failed to set network interface config: {:?}", err);
                    errno!(EIO)
                })?
                .map_err(map_update_error)?;
            Ok(())
        }
        SysctlInterfaceSelector::Id(id) => {
            let root =
                connect_to_protocol_sync::<fnet_root::InterfacesMarker>().map_err(|err| {
                    log_error!(
                        "failed to connect to {}: {:?}",
                        fnet_root::InterfacesMarker::PROTOCOL_NAME,
                        err
                    );
                    errno!(EIO)
                })?;
            let (control, server) = fidl::endpoints::create_sync_proxy();
            root.get_admin(id.get(), server).map_err(|err| {
                log_error!("failed to get network interface: {:?}", err);
                errno!(EIO)
            })?;
            let _prev = control
                .set_configuration(config, zx::MonotonicInstant::INFINITE)
                .map_err(|err| {
                    if err.is_closed() {
                        log_error!("network interface {} went away", id);
                        errno!(ENODEV)
                    } else {
                        log_error!("failed to set network interface config: {:?}", err);
                        errno!(EIO)
                    }
                })?
                .map_err(|err| {
                    use fnet_interfaces_admin::ControlSetConfigurationError;
                    match err {
                        ControlSetConfigurationError::Ipv4ForwardingUnsupported
                        | ControlSetConfigurationError::Ipv4MulticastForwardingUnsupported
                        | ControlSetConfigurationError::Ipv4IgmpVersionUnsupported
                        | ControlSetConfigurationError::Ipv6ForwardingUnsupported
                        | ControlSetConfigurationError::Ipv6MulticastForwardingUnsupported
                        | ControlSetConfigurationError::Ipv6MldVersionUnsupported
                        | ControlSetConfigurationError::ArpNotSupported
                        | ControlSetConfigurationError::NdpNotSupported => errno!(ENOTSUP),
                        ControlSetConfigurationError::IllegalZeroValue
                        | ControlSetConfigurationError::IllegalNegativeValue => errno!(EINVAL),
                        ControlSetConfigurationError::__SourceBreaking { unknown_ordinal } => {
                            log_error!("unknown error with ordinal: {unknown_ordinal}");
                            errno!(EIO)
                        }
                    }
                });
            Ok(())
        }
    }
}

fn get_interface_defaults() -> Result<fidl_fuchsia_net_interfaces_admin::Configuration, Errno> {
    let state = connect_to_protocol_sync::<fnet_settings::StateMarker>().map_err(|err| {
        log_error!("failed to connect to {}: {:?}", fnet_settings::StateMarker::PROTOCOL_NAME, err);
        errno!(EIO)
    })?;
    let config = state.get_interface_defaults(zx::MonotonicInstant::INFINITE).map_err(|err| {
        log_error!("failed to get network interface defaults: {:?}", err);
        if err.is_closed() { errno!(ENODEV) } else { errno!(EIO) }
    })?;
    Ok(config)
}

fn get_interface_config(
    selector: SysctlInterfaceSelector,
) -> Result<fidl_fuchsia_net_interfaces_admin::Configuration, Errno> {
    match selector {
        SysctlInterfaceSelector::All => {
            track_stub!(
                TODO("https://fxbug.dev/521344735"),
                "procfs sys net 'all' interface config"
            );
            get_interface_defaults()
        }
        SysctlInterfaceSelector::Default => get_interface_defaults(),
        SysctlInterfaceSelector::Id(id) => {
            let root =
                connect_to_protocol_sync::<fnet_root::InterfacesMarker>().map_err(|err| {
                    log_error!(
                        "failed to connect to {}: {:?}",
                        fnet_root::InterfacesMarker::PROTOCOL_NAME,
                        err
                    );
                    errno!(EIO)
                })?;
            let (control, server) = fidl::endpoints::create_sync_proxy();
            root.get_admin(id.get(), server).map_err(|err| {
                log_error!("failed to get network interface: {:?}", err);
                errno!(EIO)
            })?;
            let config = control
                .get_configuration(zx::MonotonicInstant::INFINITE)
                .map_err(|err| {
                    log_error!("failed to get network interface config: {:?}", err);
                    if err.is_closed() { errno!(ENODEV) } else { errno!(EIO) }
                })?
                .map_err(|err| match err {
                    fnet_interfaces_admin::ControlGetConfigurationError::__SourceBreaking {
                        unknown_ordinal,
                    } => {
                        log_error!("unknown error with ordinal: {unknown_ordinal}");
                        errno!(EIO)
                    }
                })?;
            Ok(config)
        }
    }
}

struct DisableIpv6;

impl InterfaceConfig for DisableIpv6 {
    fn try_from_i32(value: i32) -> Result<fidl_fuchsia_net_interfaces_admin::Configuration, Errno> {
        Ok(fidl_fuchsia_net_interfaces_admin::Configuration {
            ipv6: Some(fidl_fuchsia_net_interfaces_admin::Ipv6Configuration {
                enabled: Some(value == 0),
                ..Default::default()
            }),
            ..Default::default()
        })
    }
    fn try_into_i32(
        config: fidl_fuchsia_net_interfaces_admin::Configuration,
    ) -> Result<i32, Errno> {
        let ipv6 = config.ipv6.ok_or_else(|| {
            log_error!("network interface config missing ipv6");
            errno!(EIO)
        })?;
        let enabled = ipv6.enabled.ok_or_else(|| {
            log_error!("network interface config missing ipv6 enabled");
            errno!(EIO)
        })?;
        Ok(i32::from(!enabled))
    }
}

struct UcastSolicit<I: Ip> {
    _marker: core::marker::PhantomData<I>,
}

impl<I: Ip> InterfaceConfig for UcastSolicit<I> {
    fn try_from_i32(value: i32) -> Result<fnet_interfaces_admin::Configuration, Errno> {
        let max_unicast_solicitations = u16::try_from(value).map_err(|_| errno!(EINVAL))?;
        let nud_config = fnet_interfaces_admin::NudConfiguration {
            max_unicast_solicitations: Some(max_unicast_solicitations),
            ..Default::default()
        };
        let mut config = fnet_interfaces_admin::Configuration::default();
        match I::VERSION {
            IpVersion::V4 => {
                config.ipv4 = Some(fidl_fuchsia_net_interfaces_admin::Ipv4Configuration {
                    arp: Some(fnet_interfaces_admin::ArpConfiguration {
                        nud: Some(nud_config),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            }
            IpVersion::V6 => {
                config.ipv6 = Some(fidl_fuchsia_net_interfaces_admin::Ipv6Configuration {
                    ndp: Some(fnet_interfaces_admin::NdpConfiguration {
                        nud: Some(nud_config),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            }
        }
        Ok(config)
    }

    fn try_into_i32(config: fnet_interfaces_admin::Configuration) -> Result<i32, Errno> {
        let max_unicast_solicitations = match I::VERSION {
            IpVersion::V4 => config
                .ipv4
                .and_then(|ipv4| ipv4.arp)
                .and_then(|arp| arp.nud)
                .and_then(|nud| nud.max_unicast_solicitations)
                .ok_or_else(|| {
                    log_error!(
                        "network interface config missing ipv4 arp max_unicast_solicitations"
                    );
                    errno!(EIO)
                })?,
            IpVersion::V6 => config
                .ipv6
                .and_then(|ipv6| ipv6.ndp)
                .and_then(|ndp| ndp.nud)
                .and_then(|nud| nud.max_unicast_solicitations)
                .ok_or_else(|| {
                    log_error!(
                        "network interface config missing ipv6 ndp max_unicast_solicitations"
                    );
                    errno!(EIO)
                })?,
        };
        Ok(i32::from(max_unicast_solicitations))
    }
}

struct McastResolicit<I: Ip> {
    _marker: core::marker::PhantomData<I>,
}

impl<I: Ip> InterfaceConfig for McastResolicit<I> {
    fn try_from_i32(value: i32) -> Result<fnet_interfaces_admin::Configuration, Errno> {
        let max_multicast_solicitations = u16::try_from(value).map_err(|_| errno!(EINVAL))?;
        let nud_config = fnet_interfaces_admin::NudConfiguration {
            max_multicast_solicitations: Some(max_multicast_solicitations),
            ..Default::default()
        };
        let mut config = fnet_interfaces_admin::Configuration::default();
        match I::VERSION {
            IpVersion::V4 => {
                config.ipv4 = Some(fidl_fuchsia_net_interfaces_admin::Ipv4Configuration {
                    arp: Some(fnet_interfaces_admin::ArpConfiguration {
                        nud: Some(nud_config),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            }
            IpVersion::V6 => {
                config.ipv6 = Some(fidl_fuchsia_net_interfaces_admin::Ipv6Configuration {
                    ndp: Some(fnet_interfaces_admin::NdpConfiguration {
                        nud: Some(nud_config),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            }
        }
        Ok(config)
    }

    fn try_into_i32(config: fnet_interfaces_admin::Configuration) -> Result<i32, Errno> {
        let max_multicast_solicitations = match I::VERSION {
            IpVersion::V4 => config
                .ipv4
                .and_then(|ipv4| ipv4.arp)
                .and_then(|arp| arp.nud)
                .and_then(|nud| nud.max_multicast_solicitations)
                .ok_or_else(|| {
                    log_error!(
                        "network interface config missing ipv4 arp max_multicast_solicitations"
                    );
                    errno!(EIO)
                })?,
            IpVersion::V6 => config
                .ipv6
                .and_then(|ipv6| ipv6.ndp)
                .and_then(|ndp| ndp.nud)
                .and_then(|nud| nud.max_multicast_solicitations)
                .ok_or_else(|| {
                    log_error!(
                        "network interface config missing ipv6 ndp max_multicast_solicitations"
                    );
                    errno!(EIO)
                })?,
        };
        Ok(i32::from(max_multicast_solicitations))
    }
}

struct BaseReachableTimeMs<I: Ip> {
    _marker: core::marker::PhantomData<I>,
}

impl<I: Ip> InterfaceConfig for BaseReachableTimeMs<I> {
    fn try_from_i32(value: i32) -> Result<fnet_interfaces_admin::Configuration, Errno> {
        let base_reachable_time = zx::Duration::<zx::BootTimeline>::from_millis(i64::from(value));
        let nud_config = fnet_interfaces_admin::NudConfiguration {
            base_reachable_time: Some(base_reachable_time.into_nanos()),
            ..Default::default()
        };
        let mut config = fnet_interfaces_admin::Configuration::default();
        match I::VERSION {
            IpVersion::V4 => {
                config.ipv4 = Some(fnet_interfaces_admin::Ipv4Configuration {
                    arp: Some(fnet_interfaces_admin::ArpConfiguration {
                        nud: Some(nud_config),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            }
            IpVersion::V6 => {
                config.ipv6 = Some(fnet_interfaces_admin::Ipv6Configuration {
                    ndp: Some(fnet_interfaces_admin::NdpConfiguration {
                        nud: Some(nud_config),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            }
        }
        Ok(config)
    }

    fn try_into_i32(config: fnet_interfaces_admin::Configuration) -> Result<i32, Errno> {
        let base_reachable_time_ns = match I::VERSION {
            IpVersion::V4 => config
                .ipv4
                .and_then(|ipv4| ipv4.arp)
                .and_then(|arp| arp.nud)
                .and_then(|nud| nud.base_reachable_time)
                .ok_or_else(|| {
                    log_error!("network interface config missing ipv4 arp base_reachable_time");
                    errno!(EIO)
                })?,
            IpVersion::V6 => config
                .ipv6
                .and_then(|ipv6| ipv6.ndp)
                .and_then(|ndp| ndp.nud)
                .and_then(|nud| nud.base_reachable_time)
                .ok_or_else(|| {
                    log_error!("network interface config missing ipv6 ndp base_reachable_time");
                    errno!(EIO)
                })?,
        };
        Ok(i32::try_from(
            zx::Duration::<zx::BootTimeline>::from_nanos(base_reachable_time_ns).into_millis(),
        )
        .map_err(|_| errno!(EIO))?)
    }
}

struct Ipv6DadTransmits;

impl InterfaceConfig for Ipv6DadTransmits {
    fn try_from_i32(value: i32) -> Result<fidl_fuchsia_net_interfaces_admin::Configuration, Errno> {
        let transmits = u16::try_from(value).map_err(|_| errno!(EINVAL))?;
        Ok(fnet_interfaces_admin::Configuration {
            ipv6: Some(fnet_interfaces_admin::Ipv6Configuration {
                ndp: Some(fnet_interfaces_admin::NdpConfiguration {
                    dad: Some(fnet_interfaces_admin::DadConfiguration {
                        transmits: Some(transmits),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn try_into_i32(
        config: fidl_fuchsia_net_interfaces_admin::Configuration,
    ) -> Result<i32, Errno> {
        config
            .ipv6
            .and_then(|ipv6| ipv6.ndp)
            .and_then(|ndp| ndp.dad)
            .and_then(|dad| dad.transmits)
            .map(i32::from)
            .ok_or_else(|| {
                log_error!("network interface config missing ipv6 ndp dad transmits");
                errno!(EIO)
            })
    }
}

// Note that this has a different behavior than Linux, linux does not tell
// whether a neighbor host variable is set by user or is learned from network.
// The Fuchsia behavior is the same if the value is only set once during
// initialization.
struct RetransTimeMs<I: Ip> {
    _marker: core::marker::PhantomData<I>,
}

impl<I: Ip> InterfaceConfig for RetransTimeMs<I> {
    fn try_from_i32(value: i32) -> Result<fnet_interfaces_admin::Configuration, Errno> {
        let retrans_timer = zx::Duration::<zx::BootTimeline>::from_millis(i64::from(value));
        let nud_config = fnet_interfaces_admin::NudConfiguration {
            retrans_timer: Some(retrans_timer.into_nanos()),
            ..Default::default()
        };
        let mut config = fnet_interfaces_admin::Configuration::default();
        match I::VERSION {
            IpVersion::V4 => {
                config.ipv4 = Some(fnet_interfaces_admin::Ipv4Configuration {
                    arp: Some(fnet_interfaces_admin::ArpConfiguration {
                        nud: Some(nud_config),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            }
            IpVersion::V6 => {
                config.ipv6 = Some(fnet_interfaces_admin::Ipv6Configuration {
                    ndp: Some(fnet_interfaces_admin::NdpConfiguration {
                        nud: Some(nud_config),
                        ..Default::default()
                    }),
                    ..Default::default()
                })
            }
        }
        Ok(config)
    }

    fn try_into_i32(config: fnet_interfaces_admin::Configuration) -> Result<i32, Errno> {
        let retrans_timer_ns = match I::VERSION {
            IpVersion::V4 => config
                .ipv4
                .and_then(|ipv4| ipv4.arp)
                .and_then(|arp| arp.nud)
                .and_then(|nud| nud.retrans_timer)
                .ok_or_else(|| {
                    log_error!("network interface config missing ipv4 arp retrans_timer");
                    errno!(EIO)
                })?,
            IpVersion::V6 => config
                .ipv6
                .and_then(|ipv6| ipv6.ndp)
                .and_then(|ndp| ndp.nud)
                .and_then(|nud| nud.retrans_timer)
                .ok_or_else(|| {
                    log_error!("network interface config missing ipv6 ndp retrans_timer");
                    errno!(EIO)
                })?,
        };
        Ok(i32::try_from(
            zx::Duration::<zx::BootTimeline>::from_nanos(retrans_timer_ns).into_millis(),
        )
        .map_err(|_| errno!(EIO))?)
    }
}

struct UseTempAddr;

impl InterfaceConfig for UseTempAddr {
    fn try_from_i32(value: i32) -> Result<fidl_fuchsia_net_interfaces_admin::Configuration, Errno> {
        // use_tempaddr - INTEGER
        // Preference for Privacy Extensions (RFC3041).
        // <= 0 : disable Privacy Extensions
        // == 1 : enable Privacy Extensions, but prefer public
        //      addresses over temporary addresses.
        // >  1 : enable Privacy Extensions and prefer temporary
        //      addresses over public addresses.
        //
        // Netstack only supports disable (<=0) or enable (>1). 1 is not a
        // sensible option. We will make it more strict by interpreting 1 as
        // >1.
        if value == 1 {
            log_warn!(
                "use_tempaddr=1 is not supported, treating it as enabled and we will prefer temporary addresses over public addresses"
            );
        }
        let use_tempaddr = value >= 1;
        Ok(fnet_interfaces_admin::Configuration {
            ipv6: Some(fnet_interfaces_admin::Ipv6Configuration {
                ndp: Some(fnet_interfaces_admin::NdpConfiguration {
                    slaac: Some(fnet_interfaces_admin::SlaacConfiguration {
                        temporary_address: Some(use_tempaddr),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn try_into_i32(
        config: fidl_fuchsia_net_interfaces_admin::Configuration,
    ) -> Result<i32, Errno> {
        config
            .ipv6
            .and_then(|ipv6| ipv6.ndp)
            .and_then(|ndp| ndp.slaac)
            .and_then(|slacc| slacc.temporary_address)
            // We deviate from Linux here by not remembering the original
            // value, this is acceptable for now and we should revisit if it
            // causes issues.
            .map(|use_tempaddr| if use_tempaddr { 2 } else { 0 })
            .ok_or_else(|| {
                log_error!("network interface config missing ipv6 ndp slacc temporary_address");
                errno!(EIO)
            })
    }
}

struct AcceptRaDefrtr;

impl InterfaceConfig for AcceptRaDefrtr {
    fn try_from_i32(value: i32) -> Result<fidl_fuchsia_net_interfaces_admin::Configuration, Errno> {
        Ok(fnet_interfaces_admin::Configuration {
            ipv6: Some(fnet_interfaces_admin::Ipv6Configuration {
                ndp: Some(fnet_interfaces_admin::NdpConfiguration {
                    route_discovery: Some(fnet_interfaces_admin::RouteDiscoveryConfiguration {
                        allow_default_route: Some(value != 0),
                        ..Default::default()
                    }),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        })
    }

    fn try_into_i32(
        config: fidl_fuchsia_net_interfaces_admin::Configuration,
    ) -> Result<i32, Errno> {
        config
            .ipv6
            .and_then(|ipv6| ipv6.ndp)
            .and_then(|ndp| ndp.route_discovery)
            .and_then(|route_discovery| route_discovery.allow_default_route)
            .map(|allow_default_route| i32::from(allow_default_route))
            .ok_or_else(|| {
                log_error!(
                    "network interface config missing ipv6 ndp route_discovery allow_default_route"
                );
                errno!(EIO)
            })
    }
}

pub struct TcpRmemFile;

impl TcpRmemFile {
    pub fn new_node() -> impl FsNodeOps {
        BytesFile::new_node(Self)
    }
}

impl BytesFileOps for TcpRmemFile {
    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let mut params = std::str::from_utf8(&data)
            .map_err(|_| errno!(EINVAL))?
            .trim_ascii()
            .split_ascii_whitespace();

        let mut next_u32 = || {
            params.next().ok_or_else(|| errno!(EINVAL))?.parse::<u32>().map_err(|_| errno!(EINVAL))
        };

        let min = next_u32()?;
        let default = next_u32()?;
        let max = next_u32()?;

        if params.next().is_some() {
            return error!(EINVAL);
        }

        let control =
            connect_to_protocol_sync::<fnet_settings::ControlMarker>().map_err(|err| {
                log_error!(
                    "failed to connect to {}: {:?}",
                    fnet_settings::ControlMarker::PROTOCOL_NAME,
                    err
                );
                errno!(EIO)
            })?;

        let tcp_settings = fnet_settings::Tcp {
            buffer_sizes: Some(fnet_settings::SocketBufferSizes {
                receive: Some(fnet_settings::SocketBufferSizeRange {
                    min: Some(min),
                    default: Some(default),
                    max: Some(max),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        control
            .update_tcp(&tcp_settings, zx::MonotonicInstant::INFINITE)
            .map_err(|err| {
                log_error!("failed to update tcp settings: {:?}", err);
                errno!(EIO)
            })?
            .map_err(map_update_error)?;

        Ok(())
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let state = connect_to_protocol_sync::<fnet_settings::StateMarker>().map_err(|err| {
            log_error!(
                "failed to connect to {}: {:?}",
                fnet_settings::StateMarker::PROTOCOL_NAME,
                err
            );
            errno!(EIO)
        })?;

        let tcp_settings = state.get_tcp(zx::MonotonicInstant::INFINITE).map_err(|err| {
            log_error!("failed to get tcp settings: {:?}", err);
            errno!(EIO)
        })?;

        let receive_sizes =
            tcp_settings.buffer_sizes.and_then(|sizes| sizes.receive).ok_or_else(|| {
                log_error!("tcp settings missing receive buffer sizes");
                errno!(EIO)
            })?;

        let min = receive_sizes.min.unwrap_or(0);
        let default = receive_sizes.default.unwrap_or(0);
        let max = receive_sizes.max.unwrap_or(0);

        Ok(format!("{}\t{}\t{}\n", min, default, max).into_bytes().into())
    }
}

pub struct TcpWmemFile;

impl TcpWmemFile {
    pub fn new_node() -> impl FsNodeOps {
        BytesFile::new_node(Self)
    }
}

impl BytesFileOps for TcpWmemFile {
    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let mut params = std::str::from_utf8(&data)
            .map_err(|_| errno!(EINVAL))?
            .trim_ascii()
            .split_ascii_whitespace();

        let mut next_u32 = || {
            params.next().ok_or_else(|| errno!(EINVAL))?.parse::<u32>().map_err(|_| errno!(EINVAL))
        };

        let min = next_u32()?;
        let default = next_u32()?;
        let max = next_u32()?;

        if params.next().is_some() {
            return error!(EINVAL);
        }

        let control =
            connect_to_protocol_sync::<fnet_settings::ControlMarker>().map_err(|err| {
                log_error!(
                    "failed to connect to {}: {:?}",
                    fnet_settings::ControlMarker::PROTOCOL_NAME,
                    err
                );
                errno!(EIO)
            })?;

        let tcp_settings = fnet_settings::Tcp {
            buffer_sizes: Some(fnet_settings::SocketBufferSizes {
                send: Some(fnet_settings::SocketBufferSizeRange {
                    min: Some(min),
                    default: Some(default),
                    max: Some(max),
                    ..Default::default()
                }),
                ..Default::default()
            }),
            ..Default::default()
        };

        control
            .update_tcp(&tcp_settings, zx::MonotonicInstant::INFINITE)
            .map_err(|err| {
                log_error!("failed to update tcp settings: {:?}", err);
                errno!(EIO)
            })?
            .map_err(map_update_error)?;

        Ok(())
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let state = connect_to_protocol_sync::<fnet_settings::StateMarker>().map_err(|err| {
            log_error!(
                "failed to connect to {}: {:?}",
                fnet_settings::StateMarker::PROTOCOL_NAME,
                err
            );
            errno!(EIO)
        })?;

        let tcp_settings = state.get_tcp(zx::MonotonicInstant::INFINITE).map_err(|err| {
            log_error!("failed to get tcp settings: {:?}", err);
            errno!(EIO)
        })?;

        let send_sizes =
            tcp_settings.buffer_sizes.and_then(|sizes| sizes.send).ok_or_else(|| {
                log_error!("tcp settings missing send buffer sizes");
                errno!(EIO)
            })?;

        let min = send_sizes.min.unwrap_or(0);
        let default = send_sizes.default.unwrap_or(0);
        let max = send_sizes.max.unwrap_or(0);

        Ok(format!("{}\t{}\t{}\n", min, default, max).into_bytes().into())
    }
}

pub struct RmemMaxFile;

impl RmemMaxFile {
    pub fn new_node() -> impl FsNodeOps {
        BytesFile::new_node(Self)
    }
}

fn map_update_error(err: fnet_settings::UpdateError) -> Errno {
    match err {
        fnet_settings::UpdateError::IllegalZeroValue
        | fnet_settings::UpdateError::IllegalNegativeValue => errno!(EINVAL),
        fnet_settings::UpdateError::OutOfRange => errno!(ERANGE),
        fnet_settings::UpdateError::NotSupported => errno!(ENOTSUP),
        fnet_settings::UpdateError::__SourceBreaking { unknown_ordinal } => {
            log_error!("unknown error with ordinal: {unknown_ordinal}");
            errno!(EIO)
        }
    }
}

#[derive(Clone, Copy)]
enum BufferDirection {
    Receive,
    Send,
}

impl BufferDirection {
    fn name(self) -> &'static str {
        match self {
            Self::Receive => "receive",
            Self::Send => "send",
        }
    }

    fn extract(
        self,
        sizes: Option<fnet_settings::SocketBufferSizes>,
    ) -> Option<fnet_settings::SocketBufferSizeRange> {
        let sizes = sizes?;
        match self {
            Self::Receive => sizes.receive,
            Self::Send => sizes.send,
        }
    }

    fn into_sizes(
        self,
        range: fnet_settings::SocketBufferSizeRange,
    ) -> fnet_settings::SocketBufferSizes {
        match self {
            Self::Receive => {
                fnet_settings::SocketBufferSizes { receive: Some(range), ..Default::default() }
            }
            Self::Send => {
                fnet_settings::SocketBufferSizes { send: Some(range), ..Default::default() }
            }
        }
    }
}

fn check_fidl<T, E: std::fmt::Debug>(
    res: Result<T, E>,
    context: impl std::fmt::Display,
) -> Result<T, Errno> {
    res.map_err(|err| {
        log_error!("failed to {context}: {err:?}");
        errno!(EIO)
    })
}

fn connect_to_protocol<P: fidl::endpoints::DiscoverableProtocolMarker>()
-> Result<P::SynchronousProxy, Errno> {
    check_fidl(connect_to_protocol_sync::<P>(), format_args!("connect to {}", P::PROTOCOL_NAME))
}

fn parse_max_buffer_size(data: &[u8]) -> Result<NonZeroU32, Errno> {
    std::str::from_utf8(data)
        .map_err(|_| errno!(EINVAL))?
        .trim_ascii()
        .parse::<NonZeroU32>()
        .map_err(|_| errno!(EINVAL))
}

fn max_buffer_size_range(
    current: Option<fnet_settings::SocketBufferSizeRange>,
    max: NonZeroU32,
) -> Result<fnet_settings::SocketBufferSizeRange, Errno> {
    let max = max.get();
    if let Some(min) = current.as_ref().and_then(|r| r.min) {
        if max < min {
            return error!(EINVAL);
        }
    }
    // Netstack3 requires `min <= default <= max` on every `SocketBufferSizeRange` update and
    // initializes TCP `default` to `ZIRCON_SOCKET_BUFFER_SIZE` (264,096 bytes). When userspace
    // lowers `rmem_max` or `wmem_max` below the current `default` (for example, Android init
    // writing 262,144), clamp `default` down to `max` so the update succeeds.
    let default = current.and_then(|r| r.default).and_then(|d| (d > max).then_some(max));
    Ok(fnet_settings::SocketBufferSizeRange { max: Some(max), default, ..Default::default() })
}

fn update_max_buffer_sizes(
    state: &fnet_settings::StateSynchronousProxy,
    control: &fnet_settings::ControlSynchronousProxy,
    dir: BufferDirection,
    max: NonZeroU32,
) -> Result<(), Errno> {
    let tcp = check_fidl(state.get_tcp(zx::MonotonicInstant::INFINITE), "get tcp settings")?;
    let udp = check_fidl(state.get_udp(zx::MonotonicInstant::INFINITE), "get udp settings")?;
    let icmp = check_fidl(state.get_icmp(zx::MonotonicInstant::INFINITE), "get icmp settings")?;

    // Validate `max` against all protocols' minimum buffer sizes before applying any updates so a
    // value between TCP's min (2048) and UDP/ICMP's min (4096) does not partially mutate TCP.
    let tcp_range = max_buffer_size_range(dir.extract(tcp.buffer_sizes), max)?;
    let udp_range = max_buffer_size_range(dir.extract(udp.buffer_sizes), max)?;
    let icmp_range = max_buffer_size_range(dir.extract(icmp.echo_buffer_sizes), max)?;

    let tcp_settings =
        fnet_settings::Tcp { buffer_sizes: Some(dir.into_sizes(tcp_range)), ..Default::default() };
    check_fidl(
        control.update_tcp(&tcp_settings, zx::MonotonicInstant::INFINITE),
        "update tcp settings",
    )?
    .map_err(map_update_error)?;

    let udp_settings =
        fnet_settings::Udp { buffer_sizes: Some(dir.into_sizes(udp_range)), ..Default::default() };
    check_fidl(
        control.update_udp(&udp_settings, zx::MonotonicInstant::INFINITE),
        "update udp settings",
    )?
    .map_err(map_update_error)?;

    let icmp_settings = fnet_settings::Icmp {
        echo_buffer_sizes: Some(dir.into_sizes(icmp_range)),
        ..Default::default()
    };
    check_fidl(
        control.update_icmp(&icmp_settings, zx::MonotonicInstant::INFINITE),
        "update icmp settings",
    )?
    .map_err(map_update_error)?;

    Ok(())
}

fn write_max_buffer_sizes(data: &[u8], dir: BufferDirection) -> Result<(), Errno> {
    let max = parse_max_buffer_size(data)?;
    let state = connect_to_protocol::<fnet_settings::StateMarker>()?;
    let control = connect_to_protocol::<fnet_settings::ControlMarker>()?;
    update_max_buffer_sizes(&state, &control, dir, max)
}

fn read_max_buffer_size(dir: BufferDirection) -> Result<Cow<'static, [u8]>, Errno> {
    let state = connect_to_protocol::<fnet_settings::StateMarker>()?;
    // Note: The three max values may have changed and not agree with each other.
    // Currently this is good enough to only get one of the max values.
    let tcp = check_fidl(state.get_tcp(zx::MonotonicInstant::INFINITE), "get tcp settings")?;
    let max = dir.extract(tcp.buffer_sizes).and_then(|sizes| sizes.max).ok_or_else(|| {
        log_error!("tcp settings missing {} buffer sizes", dir.name());
        errno!(EIO)
    })?;
    Ok(format!("{max}\n").into_bytes().into())
}

impl BytesFileOps for RmemMaxFile {
    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        write_max_buffer_sizes(&data, BufferDirection::Receive)
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        read_max_buffer_size(BufferDirection::Receive)
    }
}

pub struct WmemMaxFile;

impl WmemMaxFile {
    pub fn new_node() -> impl FsNodeOps {
        BytesFile::new_node(Self)
    }
}

impl BytesFileOps for WmemMaxFile {
    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        write_max_buffer_sizes(&data, BufferDirection::Send)
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        read_max_buffer_size(BufferDirection::Send)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{
        cached_entry_survives, lookup_no_follow, spawn_kernel_with_cached_procfs,
        spawn_kernel_with_procfs,
    };
    use assert_matches::assert_matches;
    use futures::StreamExt as _;
    use starnix_core::task::KernelFeatures;
    use starnix_core::vfs::{LookupContext, NamespaceNode};
    use starnix_uapi::off_t;

    const ETH0_CONF_PATH: &str = "/proc/sys/net/ipv4/conf/eth0";
    const ETH0_NEIGH_PATH: &str = "/proc/sys/net/ipv4/neigh/eth0";
    const ETH0_IPV6_CONF_PATH: &str = "/proc/sys/net/ipv6/conf/eth0";
    const ETH0_IPV6_NEIGH_PATH: &str = "/proc/sys/net/ipv6/neigh/eth0";
    const ETH0_IFACE_PATHS: [&str; 4] =
        [ETH0_CONF_PATH, ETH0_NEIGH_PATH, ETH0_IPV6_CONF_PATH, ETH0_IPV6_NEIGH_PATH];
    const ALL_CONF_PATH: &str = "/proc/sys/net/ipv4/conf/all";
    const DEFAULT_CONF_PATH: &str = "/proc/sys/net/ipv6/conf/default";
    const PERMANENT_IFACE_PATHS: [&str; 2] = [ALL_CONF_PATH, DEFAULT_CONF_PATH];

    #[::fuchsia::test]
    fn test_max_buffer_size_range() {
        assert_eq!(parse_max_buffer_size(b"262144\n"), Ok(NonZeroU32::new(262_144).unwrap()));
        assert_eq!(parse_max_buffer_size(b"0\n"), error!(EINVAL));
        assert_eq!(parse_max_buffer_size(b"invalid"), error!(EINVAL));
        assert_eq!(parse_max_buffer_size(b"\xff"), error!(EINVAL));
        assert_eq!(BufferDirection::Receive.name(), "receive");
        assert_eq!(BufferDirection::Send.name(), "send");
        assert_eq!(BufferDirection::Receive.extract(None), None);
        assert_eq!(BufferDirection::Send.extract(None), None);

        // Missing current settings still sets `max` while leaving `default` unset.
        assert_eq!(
            max_buffer_size_range(None, NonZeroU32::new(262_144).unwrap()),
            Ok(fnet_settings::SocketBufferSizeRange {
                max: Some(262_144),
                default: None,
                min: None,
                ..Default::default()
            })
        );

        let initial = Some(fnet_settings::SocketBufferSizeRange {
            min: Some(2048),
            default: Some(264_096),
            max: Some(4 * 1024 * 1024),
            ..Default::default()
        });

        // Values below `min` are rejected before sending FIDL updates.
        assert_eq!(
            max_buffer_size_range(initial.clone(), NonZeroU32::new(1024).unwrap()),
            error!(EINVAL)
        );

        // Lowering `max` below `default` clamps `default` down to `max`.
        assert_eq!(
            max_buffer_size_range(initial.clone(), NonZeroU32::new(262_144).unwrap()),
            Ok(fnet_settings::SocketBufferSizeRange {
                max: Some(262_144),
                default: Some(262_144),
                min: None,
                ..Default::default()
            })
        );

        // Setting `max` at or above `default` leaves `default` untouched (`None`).
        assert_eq!(
            max_buffer_size_range(initial.clone(), NonZeroU32::new(264_096).unwrap()),
            Ok(fnet_settings::SocketBufferSizeRange {
                max: Some(264_096),
                default: None,
                min: None,
                ..Default::default()
            })
        );
        assert_eq!(
            max_buffer_size_range(initial, NonZeroU32::new(6_291_456).unwrap()),
            Ok(fnet_settings::SocketBufferSizeRange {
                max: Some(6_291_456),
                default: None,
                min: None,
                ..Default::default()
            })
        );
    }

    #[::fuchsia::test]
    fn test_update_max_buffer_sizes() {
        let range = |min, default, max| {
            Some(fnet_settings::SocketBufferSizeRange {
                min: Some(min),
                default: Some(default),
                max: Some(max),
                ..Default::default()
            })
        };
        let tcp = fnet_settings::Tcp {
            buffer_sizes: Some(fnet_settings::SocketBufferSizes {
                receive: range(256, 264_096, 4_194_304),
                send: range(2048, 264_096, 4_194_304),
                ..Default::default()
            }),
            ..Default::default()
        };

        let dgram_sizes = Some(fnet_settings::SocketBufferSizes {
            receive: range(256, 212_992, 4_194_304),
            send: range(4096, 212_992, 4_194_304),
            ..Default::default()
        });
        let udp = fnet_settings::Udp { buffer_sizes: dgram_sizes.clone(), ..Default::default() };
        let icmp = fnet_settings::Icmp { echo_buffer_sizes: dgram_sizes, ..Default::default() };

        let (state, state_server) =
            fidl::endpoints::create_sync_proxy::<fnet_settings::StateMarker>();
        let (control, control_server) =
            fidl::endpoints::create_sync_proxy::<fnet_settings::ControlMarker>();

        let server = std::thread::spawn(move || {
            let mut exec = fuchsia_async::LocalExecutor::default();
            exec.run_singlethreaded(async move {
                let mut state_stream = state_server.into_stream();
                let mut control_stream = control_server.into_stream();
                let mut tcp_updates = Vec::new();
                let mut udp_updates = Vec::new();
                let mut icmp_updates = Vec::new();

                let state_fut = async {
                    while let Some(Ok(req)) = state_stream.next().await {
                        match req {
                            fnet_settings::StateRequest::GetTcp { responder } => {
                                responder.send(&tcp).unwrap();
                            }
                            fnet_settings::StateRequest::GetUdp { responder } => {
                                responder.send(&udp).unwrap();
                            }
                            fnet_settings::StateRequest::GetIcmp { responder } => {
                                responder.send(&icmp).unwrap();
                            }
                            _ => unreachable!(),
                        }
                    }
                };
                let control_fut = async {
                    while let Some(Ok(req)) = control_stream.next().await {
                        match req {
                            fnet_settings::ControlRequest::UpdateTcp { payload, responder } => {
                                tcp_updates.push(payload);
                                responder.send(Ok(&fnet_settings::Tcp::default())).unwrap();
                            }
                            fnet_settings::ControlRequest::UpdateUdp { payload, responder } => {
                                udp_updates.push(payload);
                                responder.send(Ok(&fnet_settings::Udp::default())).unwrap();
                            }
                            fnet_settings::ControlRequest::UpdateIcmp { payload, responder } => {
                                icmp_updates.push(payload);
                                responder.send(Ok(&fnet_settings::Icmp::default())).unwrap();
                            }
                            _ => unreachable!(),
                        }
                    }
                };
                futures::future::join(state_fut, control_fut).await;
                (tcp_updates, udp_updates, icmp_updates)
            })
        });

        assert_eq!(
            update_max_buffer_sizes(
                &state,
                &control,
                BufferDirection::Receive,
                NonZeroU32::new(262_144).unwrap(),
            ),
            Ok(())
        );
        assert_eq!(
            update_max_buffer_sizes(
                &state,
                &control,
                BufferDirection::Send,
                NonZeroU32::new(262_144).unwrap(),
            ),
            Ok(())
        );
        // 3000 is >= TCP SNDBUF_MIN (2048) but < UDP/ICMP DEFAULT_DATAGRAM_MIN_SNDBUF (4096);
        // pre-validation must reject it with EINVAL without sending UpdateTcp.
        assert_eq!(
            update_max_buffer_sizes(
                &state,
                &control,
                BufferDirection::Send,
                NonZeroU32::new(3000).unwrap(),
            ),
            error!(EINVAL)
        );

        drop(state);
        drop(control);
        let (tcp_updates, udp_updates, icmp_updates) = server.join().unwrap();
        assert_eq!(tcp_updates.len(), 2);
        assert_eq!(udp_updates.len(), 2);
        assert_eq!(icmp_updates.len(), 2);

        // TCP default (264,096) was > 262,144 so it was clamped to 262,144; UDP/ICMP default
        // (212,992) was <= 262,144 so it was left as None. Opposite directions remain None.
        let clamped_range = Some(fnet_settings::SocketBufferSizeRange {
            max: Some(262_144),
            default: Some(262_144),
            ..Default::default()
        });
        let unclamped_range = Some(fnet_settings::SocketBufferSizeRange {
            max: Some(262_144),
            default: None,
            ..Default::default()
        });
        assert_eq!(
            tcp_updates[0].buffer_sizes,
            Some(fnet_settings::SocketBufferSizes {
                receive: clamped_range.clone(),
                send: None,
                ..Default::default()
            })
        );
        assert_eq!(
            tcp_updates[1].buffer_sizes,
            Some(fnet_settings::SocketBufferSizes {
                receive: None,
                send: clamped_range,
                ..Default::default()
            })
        );
        let expected_dgram_receive = Some(fnet_settings::SocketBufferSizes {
            receive: unclamped_range.clone(),
            send: None,
            ..Default::default()
        });
        let expected_dgram_send = Some(fnet_settings::SocketBufferSizes {
            receive: None,
            send: unclamped_range,
            ..Default::default()
        });
        assert_eq!(udp_updates[0].buffer_sizes, expected_dgram_receive);
        assert_eq!(udp_updates[1].buffer_sizes, expected_dgram_send);
        assert_eq!(icmp_updates[0].echo_buffer_sizes, expected_dgram_receive);
        assert_eq!(icmp_updates[1].echo_buffer_sizes, expected_dgram_send);
    }

    fn add_test_device(current_task: &CurrentTask, name: &str, id: u64) {
        // Mark initialized to exercise lookup and revalidation without spawning a netlink worker.
        let netstack_devices = &current_task.kernel().netstack_devices;
        netstack_devices.initialized_and_wq.0.store(true, Ordering::SeqCst);
        netstack_devices.add_device(
            current_task.kernel(),
            name.into(),
            NonZeroU64::new(id).expect("non-zero"),
        );
    }

    fn remove_test_device(current_task: &CurrentTask, name: &str) {
        current_task.kernel().netstack_devices.remove_device(current_task, name.into());
    }

    /// Looks up `path` twice while holding the first result and asserts that the second lookup
    /// reused the first `DirEntry`.
    fn assert_relookup_reuses_entry(current_task: &CurrentTask, path: &str) {
        let entry1 = lookup_no_follow(current_task, path).expect("lookup");
        let entry2 = lookup_no_follow(current_task, path).expect("re-lookup");
        assert!(
            Arc::ptr_eq(&entry1.entry, &entry2.entry),
            "re-lookup of {path} created a new entry"
        );
    }

    /// Looks up `path` with `eth0` present, removes `eth0` while holding the result, and looks up
    /// `path` again. Returns the held entry and the result of the second lookup.
    fn lookup_across_eth0_removal(
        current_task: &CurrentTask,
        path: &str,
    ) -> (NamespaceNode, Result<NamespaceNode, Errno>) {
        add_test_device(current_task, "eth0", 1);
        let held = lookup_no_follow(current_task, path).expect("lookup");
        remove_test_device(current_task, "eth0");
        (held, lookup_no_follow(current_task, path))
    }

    #[derive(Default)]
    struct NoopDirentSink(off_t);

    impl DirentSink for NoopDirentSink {
        fn add(
            &mut self,
            _inode_num: uapi::ino_t,
            offset: off_t,
            _entry_type: DirectoryEntryType,
            _name: &FsStr,
        ) -> Result<(), Errno> {
            self.0 = offset;
            Ok(())
        }

        fn offset(&self) -> off_t {
            self.0
        }
    }

    /// Holds `/proc/sys/net/ipv4/neigh/eth0`, one of its children (`ucast_solicit`), and an open
    /// directory `FileHandle`, runs `invalidate`, and asserts that relative lookups of both an
    /// uncached child (`retrans_time_ms`) and the held child (`ucast_solicit`) as well as `readdir`
    /// on the open directory all fail with `ENOENT`.
    fn assert_held_iface_dir_rejects_relative_ops_after(
        current_task: &CurrentTask,
        invalidate: impl FnOnce(&CurrentTask),
    ) {
        add_test_device(current_task, "eth0", 1);
        let held_dir = lookup_no_follow(current_task, ETH0_NEIGH_PATH).expect("lookup eth0 neigh");
        let _held_child = held_dir
            .lookup_child(current_task, &mut LookupContext::default(), "ucast_solicit".into())
            .expect("lookup held child");
        let open_dir = current_task
            .open_file(ETH0_NEIGH_PATH.into(), OpenFlags::RDONLY | OpenFlags::DIRECTORY)
            .expect("open eth0 neigh");

        invalidate(current_task);

        assert_matches!(
            held_dir.lookup_child(
                current_task,
                &mut LookupContext::default(),
                "retrans_time_ms".into(),
            ),
            Err(e) if e == errno!(ENOENT)
        );
        assert_matches!(
            held_dir.lookup_child(
                current_task,
                &mut LookupContext::default(),
                "ucast_solicit".into(),
            ),
            Err(e) if e == errno!(ENOENT)
        );
        assert_matches!(
            open_dir.readdir(current_task, &mut NoopDirentSink::default()),
            Err(e) if e == errno!(ENOENT)
        );
    }

    #[::fuchsia::test]
    async fn test_netstack_device_revalidate_same_interface() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            add_test_device(current_task, "eth0", 1);
            for path in ETH0_IFACE_PATHS {
                assert_relookup_reuses_entry(current_task, path);
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_netstack_device_revalidate_recreated_interface() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            add_test_device(current_task, "eth0", 1);
            let held: Vec<_> = ETH0_IFACE_PATHS
                .into_iter()
                .map(|path| (path, lookup_no_follow(current_task, path).expect("lookup eth0")))
                .collect();

            remove_test_device(current_task, "eth0");
            add_test_device(current_task, "eth0", 2);
            for (path, entry1) in held {
                let entry2 = lookup_no_follow(current_task, path).expect("lookup recreated");
                assert!(!Arc::ptr_eq(&entry1.entry, &entry2.entry), "{path}");
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_netstack_device_revalidate_removed_interface() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            for path in ETH0_IFACE_PATHS {
                assert_matches!(
                    lookup_across_eth0_removal(current_task, path).1,
                    Err(e) if e == errno!(ENOENT),
                    "{path}"
                );
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_netstack_device_revalidate_keeps_permanent_entries() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            for path in PERMANENT_IFACE_PATHS {
                let (held, relookup) = lookup_across_eth0_removal(current_task, path);
                assert!(
                    Arc::ptr_eq(&held.entry, &relookup.expect("re-lookup permanent").entry),
                    "{path}"
                );
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_netstack_device_revalidate_recreated_neigh_grandchild() {
        const ETH0_UCAST_SOLICIT_PATH: &str = "/proc/sys/net/ipv4/neigh/eth0/ucast_solicit";
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            add_test_device(current_task, "eth0", 1);
            let file1 = lookup_no_follow(current_task, ETH0_UCAST_SOLICIT_PATH)
                .expect("lookup ucast_solicit");

            remove_test_device(current_task, "eth0");
            add_test_device(current_task, "eth0", 2);
            let file2 = lookup_no_follow(current_task, ETH0_UCAST_SOLICIT_PATH)
                .expect("re-lookup ucast_solicit");
            assert!(!Arc::ptr_eq(&file1.entry.node, &file2.entry.node));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_held_netstack_device_dir_removed_interface_fails_relative_ops() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            assert_held_iface_dir_rejects_relative_ops_after(current_task, |task| {
                remove_test_device(task, "eth0");
            });
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_held_netstack_device_dir_recreated_interface_fails_relative_ops() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            assert_held_iface_dir_rejects_relative_ops_after(current_task, |task| {
                remove_test_device(task, "eth0");
                add_test_device(task, "eth0", 2);
            });
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_procfs_dcache_netstack_recreated_interface_evicts_entry() {
        spawn_kernel_with_cached_procfs(async move |current_task| {
            add_test_device(current_task, "eth0", 1);
            let recreate = || {
                remove_test_device(current_task, "eth0");
                add_test_device(current_task, "eth0", 2);
            };
            assert!(!cached_entry_survives(current_task, ETH0_CONF_PATH, recreate));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_procfs_dcache_netstack_removed_interface_evicts_entry() {
        spawn_kernel_with_cached_procfs(async move |current_task| {
            add_test_device(current_task, "eth0", 1);
            let remove = || remove_test_device(current_task, "eth0");
            assert!(!cached_entry_survives(current_task, ETH0_CONF_PATH, remove));
        })
        .await;
    }
}
