// Copyright 2024 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Kernel classes and permissions are added here when the relevant hook and enforcement is added.
use crate::policy::AccessVector;
use fuchsia_rcu::RcuDroppable;
use selinux_policy_derive::{class_permission_enum, declare_kernel_classes, subset_enum};
use std::ops::{BitOr, BitOrAssign};
use zerocopy::{Immutable, IntoBytes};

declare_kernel_classes! {
    /// Well-known class in SELinux policy that has a particular meaning in policy enforcement
    /// hooks.
    #[derive(IntoBytes, Immutable, RcuDroppable)]
    #[repr(u8)]
    KernelClass {
        // keep-sorted start
        /// SELinux "anon_inode" object class.
        AnonFsNode("anon_inode"),
        /// SELinux "binder" object class.
        Binder,
        /// SELinux "blk_file" object class.
        BlkFile,
        /// SELinux "bpf" object class.
        Bpf,
        /// SELinux "capability" object class.
        Capability,
        /// SELinux "capability2" object class.
        Capability2,
        /// SELinux "chr_file" object class.
        ChrFile,
        /// SELinux "dir" object class.
        Dir,
        /// SELinux "fd" object class.
        Fd,
        /// SELinux "fifo_file" object class.
        FifoFile,
        /// SELinux "file" object class.
        File,
        /// SELinux "filesystem" object class.
        FileSystem("filesystem"),
        /// SELinux "icmp_socket" class enabled via the "extended_socket_class" policy capability.
        IcmpSocket,
        /// SELinux "key" object class.
        Key,
        /// SELinux "key_socket" object class.
        KeySocket,
        /// SELinux "lnk_file" object class.
        LnkFile,
        /// SELinux "memfd_file" object class.
        MemFdFile("memfd_file"),
        /// SELinux "memprotect" object class.
        MemProtect("memprotect"),
        /// SELinux "netlink_audit_socket" object class.
        NetlinkAuditSocket,
        /// SELinux "netlink_connector_socket" object class.
        NetlinkConnectorSocket,
        /// SELinux "netlink_crypto_socket" object class.
        NetlinkCryptoSocket,
        /// SELinux "netlink_dnrt_socket" object class.
        NetlinkDnrtSocket,
        /// SELinux "netlink_fib_lookup_socket" object class.
        NetlinkFibLookupSocket,
        /// SELinux "netlink_firewall_socket" object class.
        NetlinkFirewallSocket,
        /// SELinux "netlink_generic_socket" object class.
        NetlinkGenericSocket,
        /// SELinux "netlink_ip6fw_socket" object class.
        NetlinkIp6FwSocket("netlink_ip6fw_socket"),
        /// SELinux "netlink_iscsi_socket" object class.
        NetlinkIscsiSocket,
        /// SELinux "netlink_kobject_uevent_socket" object class.
        NetlinkKobjectUeventSocket,
        /// SELinux "netlink_netfilter_socket" object class.
        NetlinkNetfilterSocket,
        /// SELinux "netlink_nflog_socket" object class.
        NetlinkNflogSocket,
        /// SELinux "netlink_rdma_socket" object class.
        NetlinkRdmaSocket,
        /// SELinux "netlink_route_socket" object class.
        NetlinkRouteSocket,
        /// SELinux "netlink_scsitransport_socket" object class.
        NetlinkScsitransportSocket,
        /// SELinux "netlink_selinux_socket" object class.
        NetlinkSelinuxSocket,
        /// SELinux "netlink_socket" object class.
        NetlinkSocket,
        /// SELinux "netlink_tcpdiag_socket" object class.
        NetlinkTcpDiagSocket("netlink_tcpdiag_socket"),
        /// SELinux "netlink_xfrm_socket" object class.
        NetlinkXfrmSocket,
        /// SELinux "packet_socket" object class.
        PacketSocket,
        /// SELinux "perf_event" object class.
        PerfEvent,
        /// SELinux "process" object class.
        Process,
        /// SELinux "process2" object class.
        Process2,
        /// SELinux "qipcrtr_socket" object class.
        QipcrtrSocket,
        /// SELinux "rawip_socket" object class.
        RawIpSocket("rawip_socket"),
        /// SELinux "sctp_socket" class enabled via the "extended_socket_class" policy capability.
        SctpSocket,
        /// SELinux "security" object class.
        Security,
        /// SELinux "sock_file" object class.
        SockFile,
        /// SELinux "socket" object class.
        Socket,
        /// SELinux "system" object class.
        System,
        /// SELinux "tcp_socket" object class.
        TcpSocket,
        /// SELinux "tun_socket" object class.
        TunSocket,
        /// SELinux "udp_socket" object class.
        UdpSocket,
        /// SELinux "unix_dgram_socket" object class.
        UnixDgramSocket,
        /// SELinux "unix_stream_socket" object class.
        UnixStreamSocket,
        /// SELinux "vsock_socket" class enabled via the "extended_socket_class" policy capability.
        VsockSocket,
        // keep-sorted end
    }
}

pub trait ForClass<T> {
    type Output;

    /// Returns the `class`-affine [`KernelPermission`] or [`KernelPermissionSet`] value
    /// corresponding to this common permission or permission set. This is used to allow hooks to
    /// resolve e.g. common "sys_nice" permission access based on the "allow" rules for the correct
    /// target object class.
    fn for_class(&self, class: T) -> Self::Output;
}

subset_enum! {
    /// Covers the set of classes that inherit from the common "cap" symbol (e.g. "capability" for
    /// now and "cap_userns" after Starnix gains user namespacing support).
    CapClass {
        // keep-sorted start
        /// SELinux "capability" object class.
        Capability,
        // keep-sorted end
    }
}

subset_enum! {
    /// Covers the set of classes that inherit from the common "cap2" symbol (e.g. "capability2" for
    /// now and "cap2_userns" after Starnix gains user namespacing support).
    Cap2Class {
        // keep-sorted start
        /// SELinux "capability2" object class.
        Capability2,
        // keep-sorted end
    }
}

subset_enum! {
    /// Well-known file-like class in SELinux policy that has a particular meaning in policy
    /// enforcement hooks.
    #[derive(IntoBytes, Immutable)]
    #[repr(transparent)]
    FileClass {
        // keep-sorted start
        /// SELinux "anon_inode" object class.
        AnonFsNode,
        /// SELinux "blk_file" object class.
        BlkFile,
        /// SELinux "chr_file" object class.
        ChrFile,
        /// SELinux "dir" object class.
        Dir,
        /// SELinux "fifo_file" object class.
        FifoFile,
        /// SELinux "file" object class.
        File,
        /// SELinux "lnk_file" object class.
        LnkFile,
        /// SELinux "memfd_file" object class.
        MemFdFile,
        /// SELinux "sock_file" object class.
        SockFile,
        // keep-sorted end
    }
}

subset_enum! {
    /// Distinguishes socket-like kernel object classes defined in SELinux policy.
    #[derive(IntoBytes, Immutable)]
    #[repr(transparent)]
    SocketClass {
        // keep-sorted start
        IcmpSocket,
        KeySocket,
        NetlinkAuditSocket,
        NetlinkConnectorSocket,
        NetlinkCryptoSocket,
        NetlinkDnrtSocket,
        NetlinkFibLookupSocket,
        NetlinkFirewallSocket,
        NetlinkGenericSocket,
        NetlinkIp6FwSocket,
        NetlinkIscsiSocket,
        NetlinkKobjectUeventSocket,
        NetlinkNetfilterSocket,
        NetlinkNflogSocket,
        NetlinkRdmaSocket,
        NetlinkRouteSocket,
        NetlinkScsitransportSocket,
        NetlinkSelinuxSocket,
        NetlinkSocket,
        NetlinkTcpDiagSocket,
        NetlinkXfrmSocket,
        PacketSocket,
        QipcrtrSocket,
        RawIpSocket,
        SctpSocket,
        /// Generic socket class applied to all socket-like objects for which no more specific
        /// class is defined.
        Socket,
        TcpSocket,
        TunSocket,
        UdpSocket,
        UnixDgramSocket,
        UnixStreamSocket,
        VsockSocket,
        // keep-sorted end
    }
}

subset_enum! {
    /// Container for a security class that could be associated with an `FsNode`, to allow
    /// permissions common to both file-like and socket-like classes to be generated easily by
    /// hooks.
    #[derive(IntoBytes, Immutable)]
    #[repr(u8)]
    FsNodeClass {
        File(FileClass),
        Socket(SocketClass),
    }
}

/// Kernel object class and class-scoped permission identifier.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq, IntoBytes, Immutable)]
#[repr(C)]
pub struct KernelPermission {
    class: KernelClass,
    id: u8,
}

impl KernelPermission {
    /// Constructs a [`KernelPermission`] for `class` and kernel permission `id`.
    ///
    /// `id` must be less than `class.permission_count()`.
    #[inline]
    const fn new(class: KernelClass, id: u8) -> Self {
        debug_assert!(id < class.permission_count());
        Self { class, id }
    }

    /// Returns the [`KernelClass`] to which this permission belongs.
    #[inline]
    pub const fn class(&self) -> KernelClass {
        self.class
    }

    /// Returns the kernel permission ID (`0..32`) within `self.class()`.
    #[inline]
    pub(crate) const fn id(&self) -> u8 {
        self.id
    }

    /// Returns this permission as a single-bit kernel [`AccessVector`].
    #[inline]
    pub fn as_access_vector(&self) -> AccessVector {
        AccessVector::from(1u32 << self.id)
    }

    /// Returns the policy string name of this permission.
    pub const fn name(&self) -> &'static str {
        self.class.permission_name(self.id)
    }
}

impl<T: Into<KernelClass>> ForClass<T> for KernelPermission {
    type Output = KernelPermission;

    fn for_class(&self, class: T) -> KernelPermission {
        assert_eq!(self.class(), class.into());
        *self
    }
}

/// Set of permissions of a single permission type `P` (such as [`DirPermission`] or
/// [`CommonFsNodePermission`]).
///
/// Sets are created from a single permission via `into()`, or by combining permissions of the
/// same type with `|`, e.g. `DirPermission::Search | DirPermission::AddName`, and converted to
/// [`KernelPermissionSet`] via `into()` (for class permissions) or `for_class()` (for common
/// permissions).
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct PermissionSet<P> {
    access_vector: AccessVector,
    _marker: std::marker::PhantomData<P>,
}

impl<P> PermissionSet<P> {
    /// Returns an empty set of permissions.
    #[inline]
    pub const fn empty() -> Self {
        Self { access_vector: AccessVector::NONE, _marker: std::marker::PhantomData }
    }

    /// Returns the permissions in this set, as permission-Id bits.
    #[cfg(test)]
    pub(crate) fn access_vector(&self) -> AccessVector {
        self.access_vector
    }

    /// Returns true if this set contains no permissions.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.access_vector == AccessVector::NONE
    }
}

impl<P> Default for PermissionSet<P> {
    #[inline]
    fn default() -> Self {
        Self::empty()
    }
}

impl<P, T: Into<Self>> BitOr<T> for PermissionSet<P> {
    type Output = Self;

    #[inline]
    fn bitor(mut self, other: T) -> Self {
        self |= other;
        self
    }
}

impl<P, T: Into<Self>> BitOrAssign<T> for PermissionSet<P> {
    #[inline]
    fn bitor_assign(&mut self, other: T) {
        self.access_vector |= other.into().access_vector;
    }
}

/// Set of permissions of a single [`KernelClass`], to be checked together.
///
/// Sets are created from a single class permission, [`KernelPermission`], or typed
/// [`PermissionSet`] via `into()`, or from a common [`PermissionSet`] via `for_class()`.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct KernelPermissionSet {
    class: KernelClass,
    /// Permissions in the set, with bits at the kernel permission-Id positions for `class`
    /// (see [`KernelPermission::id()`]).
    access_vector: AccessVector,
}

impl KernelPermissionSet {
    /// Returns the set of `class` permissions whose kernel permission-Id bits are set in
    /// `access_vector`.
    ///
    /// `access_vector` must only have bits set at the kernel permission-Id positions of `class`'s
    /// permissions.
    #[inline]
    pub(crate) fn new(class: KernelClass, access_vector: AccessVector) -> Self {
        debug_assert_eq!(
            access_vector.value().checked_shr(class.permission_count() as u32).unwrap_or(0),
            0,
            "access_vector contains bits outside class permissions",
        );
        Self { class, access_vector }
    }

    /// Returns the class of the permissions in this set.
    #[inline]
    pub fn class(&self) -> KernelClass {
        self.class
    }

    /// Returns the permissions in this set, as kernel permission-Id bits.
    #[inline]
    pub(crate) fn access_vector(&self) -> AccessVector {
        self.access_vector
    }

    /// Returns true if this set contains no permissions.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.access_vector == AccessVector::NONE
    }

    /// Returns the permissions in this set, in kernel permission-Id order.
    pub fn iter(&self) -> impl Iterator<Item = KernelPermission> + use<> {
        let class = self.class;
        let mut bits = self.access_vector.value();
        std::iter::from_fn(move || {
            if bits == 0 {
                return None;
            }
            let id = bits.trailing_zeros() as u8;
            bits &= bits - 1;
            Some(KernelPermission::new(class, id))
        })
    }
}

impl From<KernelPermission> for KernelPermissionSet {
    #[inline]
    fn from(permission: KernelPermission) -> Self {
        Self { class: permission.class(), access_vector: permission.as_access_vector() }
    }
}

class_permission_enum! {
    /// Permissions common to all cap-like object classes (e.g. "capability" for now and
    /// "cap_userns" after Starnix gains user namespacing support). These are combined with a
    /// specific [`CapClass`] by policy enforcement hooks, to obtain class-affine permission
    /// values to check.
    CommonCapPermission {
        // keep-sorted start
        AuditControl,
        AuditWrite,
        Chown,
        DacOverride,
        DacReadSearch,
        Fowner,
        Fsetid,
        IpcLock,
        IpcOwner,
        Kill,
        Lease,
        LinuxImmutable,
        Mknod,
        NetAdmin,
        NetBindService,
        NetBroadcast,
        NetRaw,
        Setfcap,
        Setgid,
        Setpcap,
        Setuid,
        SysAdmin,
        SysBoot,
        SysChroot,
        SysModule,
        SysNice,
        SysPacct,
        SysPtrace,
        SysRawio,
        SysResource,
        SysTime,
        SysTtyConfig,
        // keep-sorted end
    }
}

class_permission_enum! {
    /// Common symbol inherited by "capability2" and "cap2_userns" classes.
    CommonCap2Permission {
        // keep-sorted start
        AuditRead,
        BlockSuspend,
        Bpf,
        MacAdmin,
        MacOverride,
        Perfmon,
        Syslog,
        WakeAlarm,
        // keep-sorted end
    }
}

class_permission_enum! {
    /// Permissions meaningful for all `FsNode`s, whether file- or socket-like.
    ///
    /// This extra layer of common permissions is not reflected in the hierarchy defined by the
    /// SELinux Reference Policy. Because even common permissions are mapped per-class, by name, to
    /// the policy equivalents, the implementation and policy notions of common permissions need not
    /// be identical.
    CommonFsNodePermission {
        // keep-sorted start
        /// Permission to append to a file or socket.
        Append,
        /// Pseudo-permission used in `dontaudit` access-rules to allow access checks to be made
        /// between specific sources & targets without generating audit logs.
        AuditAccess,
        /// Permission to create a file or socket.
        Create,
        /// Permission to query attributes, including uid, gid and extended attributes.
        GetAttr("getattr"),
        /// Permission to execute ioctls on the file or socket.
        Ioctl,
        /// Permission to set and unset file or socket locks.
        Lock,
        /// Permission to map a file.
        Map,
        /// Permission to read content from a file or socket, as well as reading or following links.
        Read,
        /// Permission checked against the existing label when updating a node's security label.
        RelabelFrom("relabelfrom"),
        /// Permission checked against the new label when updating a node's security label.
        RelabelTo("relabelto"),
        /// Permission to modify attributes, including uid, gid and extended attributes.
        SetAttr("setattr"),
        /// Permission to write contents to the file or socket.
        Write,
        // keep-sorted end
    }
}

class_permission_enum! {
    /// Permissions common to all socket-like object classes. These are combined with a specific
    /// [`SocketClass`] by policy enforcement hooks, to obtain class-affine permission values.
    CommonSocketPermission extends CommonFsNodePermission {
        // keep-sorted start
        /// Permission to accept a connection.
        Accept,
        /// Permission to bind to a name.
        Bind,
        /// Permission to initiate a connection.
        Connect,
        /// Permission to get socket options.
        GetOpt("getopt"),
        /// Permission to listen for connections.
        Listen,
        /// Permission to send datagrams to the socket.
        SendTo("sendto"),
        /// Permission to set socket options.
        SetOpt("setopt"),
        /// Permission to terminate connection.
        Shutdown,
        // keep-sorted end
    }
}

class_permission_enum! {
    NetlinkRouteSocketPermission for NetlinkRouteSocket extends CommonSocketPermission {
        // keep-sorted start
        /// Permission for nlmsg xperms.
        Nlmsg,
        /// Permission to read the kernel neighbor table.
        NlmsgGetNeigh("nlmsg_getneigh"),
        /// Permission to read the kernel routing table.
        NlmsgRead,
        /// Permission to read privileged netlink messages.
        NlmsgReadPriv("nlmsg_readpriv"),
        /// Permission to write to the kernel routing table.
        NlmsgWrite,
        // keep-sorted end
    }
}

class_permission_enum! {
    NetlinkTcpDiagSocketPermission for NetlinkTcpDiagSocket extends CommonSocketPermission {
        // keep-sorted start
        /// Permission for nlmsg xperms.
        Nlmsg,
        /// Permission to request information about a protocol.
        NlmsgRead,
        /// Permission to write netlink message.
        NlmsgWrite,
        // keep-sorted end
    }
}

class_permission_enum! {
    NetlinkXfrmSocketPermission for NetlinkXfrmSocket extends CommonSocketPermission {
        // keep-sorted start
        /// Permission for nlmsg xperms.
        Nlmsg,
        /// Permission to get IPSec configuration information.
        NlmsgRead,
        /// Permission to set IPSec configuration information.
        NlmsgWrite,
        // keep-sorted end
    }
}

class_permission_enum! {
    NetlinkAuditSocketPermission for NetlinkAuditSocket extends CommonSocketPermission {
        // keep-sorted start
        /// Permission for nlmsg xperms.
        Nlmsg,
        /// Permission to query status of audit service.
        NlmsgRead,
        /// Permission to list auditing configuration rules.
        NlmsgReadPriv("nlmsg_readpriv"),
        /// Permission to send userspace audit messages to the audit service.
        NlmsgRelay,
        /// Permission to control TTY auditing.
        NlmsgTtyAudit,
        /// Permission to update the audit service configuration.
        NlmsgWrite,
        // keep-sorted end
    }
}

class_permission_enum! {
    UnixStreamSocketPermission for UnixStreamSocket extends CommonSocketPermission {
        // keep-sorted start
        /// Permission to connect a streaming Unix-domain socket.
        ConnectTo("connectto"),
        // keep-sorted end
    }
}

class_permission_enum! {
    /// Permissions common to all file-like object classes (e.g. "lnk_file", "dir"). These are
    /// combined with a specific [`FileClass`] by policy enforcement hooks, to obtain class-affine
    /// permission values to check.
    CommonFilePermission extends CommonFsNodePermission {
        // keep-sorted start
        /// Permission to make executable a file mapping that has been modified by copy-on-write.
        ExecMod("execmod"),
        /// Permission to execute a file with domain transition.
        Execute,
        /// Permissions to create hard link.
        Link,
        /// Permission to use as mount point; only useful for directories and files.
        MountOn("mounton"),
        /// Permission to open a file.
        Open,
        /// Permission to rename a file.
        Rename,
        /// Permission to delete a file or remove a hard link.
        Unlink,
        /// Permission to set a watch for changes to a file (e.g. via inotify).
        Watch,
        /// Permission to set a watch that will receive notifications of read-like events.
        WatchReads,
        // keep-sorted end
    }
}

class_permission_enum! {
    BinderPermission for Binder {
        // keep-sorted start
        /// Permission to perform a binder IPC to a given target process.
        Call,
        /// Permission to use a Binder connection created with a different security context.
        Impersonate,
        /// Permission to set oneself as a context manager.
        SetContextMgr,
        /// Permission to transfer Binder objects as part of a Binder transaction.
        Transfer,
        // keep-sorted end
    }
}

class_permission_enum! {
    DirPermission for Dir extends CommonFilePermission {
        // keep-sorted start
        /// Permission to add a file to the directory.
        AddName,
        /// Permission to remove a directory.
        RemoveDir("rmdir"),
        /// Permission to remove an entry from a directory.
        RemoveName,
        /// Permission to change parent directory.
        Reparent,
        /// Search access to the directory.
        Search,
        // keep-sorted end
    }
}

class_permission_enum! {
    FdPermission for Fd {
        // keep-sorted start
        /// Permission to use file descriptors copied/retained/inherited from another security
        /// context. This permission is generally used to control whether an `exec*()` call from a
        /// cloned process that retained a copy of the file descriptor table should succeed.
        Use,
        // keep-sorted end
    }
}

class_permission_enum! {
    BpfPermission for Bpf {
        // keep-sorted start
        /// Permission to create a map.
        MapCreate,
        /// Permission to read from a map.
        MapRead,
        /// Permission to write on a map.
        MapWrite,
        /// Permission to load a program.
        ProgLoad,
        /// Permission to run a program.
        ProgRun,
        // keep-sorted end
    }
}

class_permission_enum! {
    PerfEventPermission for PerfEvent {
        // keep-sorted start
        /// Permission to monitor the cpu.
        Cpu,
        /// Permission to monitor the kernel.
        Kernel,
        /// Permission to open a perf event.
        Open,
        /// Permission to read a perf event.
        Read,
        /// Permission to monitor tracepoints.
        Tracepoint,
        /// Permission to write a perf event.
        Write,
        // keep-sorted end
    }
}

class_permission_enum! {
    FilePermission for File extends CommonFilePermission {
        // keep-sorted start
        /// Permission to use a file as an entry point into the new domain on transition.
        Entrypoint,
        /// Permission to use a file as an entry point to the calling domain without performing a
        /// transition.
        ExecuteNoTrans,
        // keep-sorted end
    }
}

class_permission_enum! {
    FileSystemPermission for FileSystem {
        // keep-sorted start
        /// Permission to associate a file to the filesystem.
        Associate,
        /// Permission to get filesystem attributes.
        GetAttr("getattr"),
        /// Permission mount a filesystem.
        Mount,
        /// Permission to relabel from this filesystem SID.
        RelabelFrom("relabelfrom"),
        /// Permission to relabel to this filesystem SID.
        RelabelTo("relabelto"),
        /// Permission to remount a filesystem with different flags.
        Remount,
        /// Permission to unmount a filesystem.
        Unmount,
        // keep-sorted end
    }
}

class_permission_enum! {
    KeyPermission for Key {
        // keep-sorted start
        /// Permission to create a key.
        Create,
        // keep-sorted end
    }
}

class_permission_enum! {
    MemProtectPermission for MemProtect {
        // keep-sorted start
        /// Permission to map memory at low addresses below `mmap_min_addr`.
        MmapZero,
        // keep-sorted end
    }
}

class_permission_enum! {
    ProcessPermission for Process {
        // keep-sorted start
        /// Permission to dynamically transition a process to a different security domain.
        DynTransition("dyntransition"),
        /// Permission to execute arbitrary code from the heap.
        ExecHeap("execheap"),
        /// Permission to execute arbitrary code from memory.
        ExecMem("execmem"),
        /// Permission to execute arbitrary code from the stack.
        ExecStack("execstack"),
        /// Permission to fork the current running process.
        Fork,
        /// Permission to get Linux capabilities of a process.
        GetCap("getcap"),
        /// Permission to get the process group ID.
        GetPgid("getpgid"),
        /// Permission to get the resource limits on a process.
        GetRlimit("getrlimit"),
        /// Permission to get scheduling policy currently applied to a process.
        GetSched("getsched"),
        /// Permission to get the session ID.
        GetSession("getsession"),
        /// Permission to exec into a new security domain without setting the AT_SECURE entry in the
        /// executable's auxiliary vector.
        NoAtSecure("noatsecure"),
        /// Permission to trace a process.
        Ptrace,
        /// Permission to inherit the parent process's resource limits on exec.
        RlimitInh("rlimitinh"),
        /// Permission to set Linux capabilities of a process.
        SetCap("setcap"),
        /// Permission to set the calling task's current Security Context.
        /// The "dyntransition" permission separately limits which Contexts "setcurrent" may be used to transition to.
        SetCurrent("setcurrent"),
        /// Permission to set the Security Context used by `exec()`.
        SetExec("setexec"),
        /// Permission to set the Security Context used when creating filesystem objects.
        SetFsCreate("setfscreate"),
        /// Permission to set the Security Context used when creating kernel keyrings.
        SetKeyCreate("setkeycreate"),
        /// Permission to set the process group ID.
        SetPgid("setpgid"),
        /// Permission to set the resource limits on a process.
        SetRlimit("setrlimit"),
        /// Permission to set scheduling policy for a process.
        SetSched("setsched"),
        /// Permission to set the Security Context used when creating new labeled sockets.
        SetSockCreate("setsockcreate"),
        /// Permission to share resources (e.g. FD table, address-space, etc) with a process.
        Share,
        /// Permission to send SIGCHLD to a process.
        SigChld("sigchld"),
        /// Permission to inherit the parent process's signal state.
        SigInh("siginh"),
        /// Permission to send SIGKILL to a process.
        SigKill("sigkill"),
        /// Permission to send SIGSTOP to a process.
        SigStop("sigstop"),
        /// Permission to send a signal other than SIGKILL, SIGSTOP, or SIGCHLD to a process.
        Signal,
        /// Permission to transition to a different security domain.
        Transition,
        // keep-sorted end
    }
}

class_permission_enum! {
    Process2Permission for Process2 {
        // keep-sorted start
        /// Permission to transition to an unbounded domain when no-new-privileges is set.
        NnpTransition,
        /// Permission to transition domain when executing from a no-SUID mounted filesystem.
        NosuidTransition,
        // keep-sorted end
    }
}

class_permission_enum! {
    SecurityPermission for Security {
        // keep-sorted start
        /// Permission to validate Security Context using the "context" API.
        CheckContext,
        /// Permission to compute access vectors via the "access" API.
        ComputeAv,
        /// Permission to compute security contexts based on `type_transition` rules via "create".
        ComputeCreate,
        /// Permission to compute security contexts based on `type_member` rules via "member".
        ComputeMember,
        /// Permission to compute security contexts based on `type_change` rules via "relabel".
        ComputeRelabel,
        /// Permission to compute user decisions via "user".
        ComputeUser,
        /// Permission to load a new binary policy into the kernel via the "load" API.
        LoadPolicy,
        /// Permission to read the loaded binary policy via the "policy" file.
        ReadPolicy,
        /// Permission to commit booleans to control conditional elements of the policy.
        SetBool("setbool"),
        /// Permission to change the way permissions are validated for `mmap()` operations.
        SetCheckReqProt("setcheckreqprot"),
        /// Permission to switch the system between permissive and enforcing modes, via "enforce".
        SetEnforce("setenforce"),
        // keep-sorted end
    }
}

class_permission_enum! {
    SystemPermission for System {
        // keep-sorted start
        /// Permission to use the syslog(2) CONSOLE action types.
        SyslogConsole,
        /// Permission to use other syslog(2) action types.
        SyslogMod,
        /// Permission to use the syslog(2) READ_ALL related action types.
        SyslogRead,
        // keep-sorted end
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_set_from_permission() {
        let typed = ProcessPermissionSet::from(ProcessPermission::Fork);
        assert_eq!(typed.access_vector(), ProcessPermission::Fork.as_access_vector());
        assert!(!typed.is_empty());

        let permissions = KernelPermissionSet::from(typed);
        assert_eq!(permissions, ProcessPermission::Fork.into());
        assert_eq!(permissions.class(), KernelClass::Process);
        assert_eq!(permissions.access_vector(), ProcessPermission::Fork.as_access_vector());
        assert!(!permissions.is_empty());
        assert_eq!(permissions.iter().collect::<Vec<_>>(), [ProcessPermission::Fork.into()]);
    }

    #[test]
    fn empty_permission_set() {
        let typed = DirPermissionSet::empty();
        assert_eq!(typed.access_vector(), AccessVector::NONE);
        assert!(typed.is_empty());

        let permissions = KernelPermissionSet::from(typed);
        assert_eq!(permissions.class(), KernelClass::Dir);
        assert_eq!(permissions.access_vector(), AccessVector::NONE);
        assert!(permissions.is_empty());
        assert_eq!(permissions.iter().count(), 0);
    }

    #[test]
    fn permission_set_combines_permissions_of_class() {
        let typed: DirPermissionSet = DirPermission::Search | DirPermission::AddName;
        let permissions = KernelPermissionSet::from(typed);
        assert_eq!(permissions.class(), KernelClass::Dir);
        assert_eq!(
            permissions.access_vector(),
            DirPermission::Search.as_access_vector() | DirPermission::AddName.as_access_vector()
        );

        // Permissions are listed in kernel permission-Id order, regardless of insertion order.
        assert_eq!(
            permissions.iter().collect::<Vec<_>>(),
            [DirPermission::AddName.into(), DirPermission::Search.into()]
        );

        // Common permissions and sets convert into derived permission sets, and sets can be
        // combined with permissions or other sets in either order.
        let read_and_open: CommonFilePermissionSet =
            CommonFilePermission::Open | CommonFsNodePermission::Read;
        let combined: DirPermissionSet = DirPermission::RemoveName | (typed | read_and_open);
        let permissions = KernelPermissionSet::from(combined);
        assert_eq!(
            permissions.iter().collect::<Vec<_>>(),
            [
                DirPermission::Read.into(),
                DirPermission::Open.into(),
                DirPermission::AddName.into(),
                DirPermission::RemoveName.into(),
                DirPermission::Search.into(),
            ]
        );
    }

    #[test]
    fn common_permission_set_for_class() {
        let fs_node_permissions: CommonFsNodePermissionSet =
            CommonFsNodePermission::Read | CommonFsNodePermission::Write;
        assert_eq!(
            fs_node_permissions.for_class(FileClass::File),
            (FilePermission::Read | FilePermission::Write).into()
        );
        assert_eq!(
            fs_node_permissions.for_class(FsNodeClass::File(FileClass::File)),
            (FilePermission::Read | FilePermission::Write).into()
        );
        assert_eq!(
            fs_node_permissions.for_class(SocketClass::UnixStreamSocket),
            (UnixStreamSocketPermission::Read | UnixStreamSocketPermission::Write).into()
        );
        // Verify a subset class that relies on the fallback `__default_*_permissions` alias.
        assert_eq!(
            fs_node_permissions.for_class(FsNodeClass::Socket(SocketClass::TcpSocket)),
            (CommonSocketPermission::Read | CommonSocketPermission::Write)
                .for_class(SocketClass::TcpSocket)
        );

        let file_permissions: CommonFilePermissionSet =
            CommonFilePermissionSet::from(fs_node_permissions) | CommonFilePermission::Open;
        assert_eq!(
            file_permissions.for_class(FileClass::Dir),
            (DirPermission::Read | DirPermission::Write | DirPermission::Open).into()
        );
    }

    #[test]
    fn kernel_class_and_permission_names() {
        for class in KernelClass::VARIANTS {
            assert_eq!(KernelClass::from_name(class.name()), Some(*class));
            let perms: Vec<_> = class.permissions().collect();
            assert!((1..=32).contains(&perms.len()));
            for perm in &perms {
                assert!(!perm.name().is_empty());
            }
        }

        // Verify that permission names across the 3-level inheritance hierarchy
        // (`CommonFsNodePermission` -> `CommonFilePermission` -> `DirPermission`) match their IDs.
        assert_eq!(KernelPermission::from(DirPermission::Read).name(), "read");
        assert_eq!(KernelPermission::from(DirPermission::GetAttr).name(), "getattr");
        assert_eq!(KernelPermission::from(DirPermission::Open).name(), "open");
        assert_eq!(KernelPermission::from(DirPermission::MountOn).name(), "mounton");
        assert_eq!(KernelPermission::from(DirPermission::Search).name(), "search");
        assert_eq!(KernelPermission::from(DirPermission::RemoveDir).name(), "rmdir");
    }

    #[test]
    fn permission_set_new_from_access_vector() {
        let permissions = KernelPermissionSet::new(
            KernelClass::Dir,
            DirPermission::Search.as_access_vector() | DirPermission::AddName.as_access_vector(),
        );
        assert_eq!(permissions, (DirPermission::Search | DirPermission::AddName).into());
    }
}
