// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "512"]

use fuchsia_rcu::RcuReadScope;
use once_cell::sync::OnceCell;
use rand::RngExt as _;
use starnix_core::fs::tmpfs::TmpFs;
use starnix_core::mm::memory::MemoryObject;
use starnix_core::security::{self, PermissionFlags};
use starnix_core::task::{CurrentTask, Kernel};
use starnix_core::vfs::fs_args::MountParams;
use starnix_core::vfs::pipe::Pipe;
use starnix_core::vfs::rw_queue::{RwQueueReadGuard, RwQueueWriteGuard};
use starnix_core::vfs::{
    AppendLockWriteGuard, CacheMode, CheckAccessReason, DirEntry, DirEntryHandle,
    DirectoryEntryType, DirectoryMode, DirentSink, FILESYSTEM_MAX_STACK_DEPTH, FallocMode,
    FileHandle, FileObject, FileOps, FileSystem, FileSystemHandle, FileSystemOps,
    FileSystemOptions, FsLockDepType, FsNode, FsNodeAttrFlags, FsNodeFlags, FsNodeHandle,
    FsNodeInfo, FsNodeOps, FsStr, FsString, InputBuffer, MountInfo, OutputBuffer, RenameContext,
    RenameFlags, SeekTarget, StatxFlags, SymlinkTarget, UnlinkKind, ValueOrSize, VecInputBuffer,
    VecOutputBuffer, XattrOp, default_seek, emit_dotdot, fileops_impl_directory,
    fileops_impl_seekable,
};
use starnix_logging::{log_error, log_warn, track_stub};
use starnix_sync::{
    DynamicLockDepRwLock, LockDepReadGuard, LockDepRwLock, LockDepWriteGuard,
    OverlayFsDirEntriesLock, OverlayFsStateLock,
};
use starnix_uapi::auth::{Credentials, FsCred};
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errors::{EEXIST, ENODATA, ENOENT, ENOSYS, ENOTSUP, Errno};
use starnix_uapi::file_mode::{FileMode, mode};
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{XATTR_NAME_CAPS, errno, error, ino_t, off_t, statfs};
use std::collections::BTreeSet;
use std::sync::Arc;

// Name and value for the xattr used to mark opaque directories in the upper FS.
// See https://docs.kernel.org/filesystems/overlayfs.html#whiteouts-and-opaque-directories
const OPAQUE_DIR_XATTR: &str = "trusted.overlay.opaque";
const OPAQUE_DIR_XATTR_VALUE: &[u8] = b"y";

#[derive(Clone)]
struct DirEntryInfo {
    name: FsString,
    inode_num: ino_t,
    entry_type: DirectoryEntryType,
}

type DirEntries = Vec<DirEntryInfo>;

#[derive(Default)]
struct DirentSinkAdapter {
    items: Vec<DirEntryInfo>,
    offset: off_t,
    entry_count: usize,
}

impl DirentSink for DirentSinkAdapter {
    fn add(
        &mut self,
        inode_num: ino_t,
        offset: off_t,
        entry_type: DirectoryEntryType,
        name: &FsStr,
    ) -> Result<(), Errno> {
        if !DirEntry::is_reserved_name(name) {
            self.items.push(DirEntryInfo { name: name.to_owned(), inode_num, entry_type });
        }
        self.offset = offset;
        self.entry_count += 1;
        Ok(())
    }

    fn offset(&self) -> off_t {
        self.offset
    }
}

#[derive(Copy, Clone, Eq, PartialEq)]
enum UpperCopyMode {
    MetadataOnly,
    CopyAll,
}

/// An `DirEntry` associated with the mount options. This is required because OverlayFs mostly
/// works at the `DirEntry` level (mounts on the lower, upper and work directories are ignored),
/// but operation must still depend on mount options.
#[derive(Clone)]
struct ActiveEntry {
    entry: DirEntryHandle,
    mount: MountInfo,
}

impl ActiveEntry {
    fn mapper<'a>(entry: &'a ActiveEntry) -> impl Fn(DirEntryHandle) -> ActiveEntry + 'a {
        |dir_entry| ActiveEntry { entry: dir_entry, mount: entry.mount.clone() }
    }

    fn entry(&self) -> &DirEntryHandle {
        &self.entry
    }

    fn mount(&self) -> &MountInfo {
        &self.mount
    }

    fn component_lookup(&self, current_task: &CurrentTask, name: &FsStr) -> Result<Self, Errno> {
        self.entry()
            .component_lookup(current_task, self.mount(), name)
            .map(ActiveEntry::mapper(self))
    }

    fn create_entry(
        &self,
        current_task: &CurrentTask,
        name: &FsStr,
        create_node_fn: impl FnOnce(&FsNodeHandle, &MountInfo, &FsStr) -> Result<FsNodeHandle, Errno>,
    ) -> Result<Self, Errno> {
        self.entry()
            .create_entry(current_task, self.mount(), name, create_node_fn)
            .map(ActiveEntry::mapper(self))
    }

    /// Sets an xattr to mark the directory referenced by `entry` as opaque. Directories that are
    /// marked as opaque in the upper FS are not merged with the corresponding directories in the
    /// lower FS.
    fn set_opaque_xattr(&self, current_task: &CurrentTask) -> Result<(), Errno> {
        self.entry().node.set_xattr(
            current_task,
            self.mount(),
            OPAQUE_DIR_XATTR.into(),
            OPAQUE_DIR_XATTR_VALUE,
            XattrOp::Set,
        )
    }

    /// Checks if the `entry` is marked as opaque.
    fn is_opaque_node(&self, current_task: &CurrentTask) -> bool {
        match self.entry().node.get_xattr(
            current_task,
            self.mount(),
            OPAQUE_DIR_XATTR.into(),
            OPAQUE_DIR_XATTR_VALUE.len(),
        ) {
            Ok(ValueOrSize::Value(v)) if &*v == OPAQUE_DIR_XATTR_VALUE => true,
            _ => false,
        }
    }

    /// Creates a "whiteout" entry in the directory called `name`. Whiteouts are created by
    /// overlayfs to denote files and directories that were removed and should not be listed in the
    /// directory. This is necessary because we cannot remove entries from the lower FS.
    fn create_whiteout(
        &self,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<ActiveEntry, Errno> {
        self.create_entry(current_task, name, |dir, mount, name| {
            dir.create_node(current_task, mount, name, FileMode::IFCHR, DeviceId::NONE)
        })
    }

    /// Returns `true` if this is a "whiteout".
    fn is_whiteout(&self) -> bool {
        let info = self.entry().node.info();
        info.mode.is_chr() && info.rdev == DeviceId::NONE
    }

    /// Checks whether the child of this entry represented by `info` is a "whiteout".
    ///
    /// Only looks up the corresponding `DirEntry` when necessary.
    fn is_whiteout_child(
        &self,
        current_task: &CurrentTask,
        info: &DirEntryInfo,
    ) -> Result<bool, Errno> {
        // We need to lookup the node only if the file is a char device.
        if info.entry_type != DirectoryEntryType::CHR {
            return Ok(false);
        }
        let entry = self.component_lookup(current_task, info.name.as_ref())?;
        Ok(entry.is_whiteout())
    }

    fn require_no_mounts(&self) -> Result<(), Errno> {
        if self.entry().has_mounts()
            && self.mount().as_ref().is_some_and(|mount| mount.has_submount(self.entry()))
        {
            return error!(EBUSY);
        }
        Ok(())
    }

    fn read_dir_entries(&self, current_task: &CurrentTask) -> Result<Vec<DirEntryInfo>, Errno> {
        let mut sink = DirentSinkAdapter::default();
        let file = self.entry().open_anonymous(current_task, OpenFlags::DIRECTORY)?;
        loop {
            let prev_count = sink.entry_count;
            file.readdir(current_task, &mut sink)?;
            if sink.entry_count == prev_count {
                break;
            }
        }
        Ok(sink.items)
    }
}

struct OverlayNode {
    stack: Arc<OverlayStack>,

    // Corresponding `DirEntries` in the lower and the upper filesystems. At least one must be
    // set. Note that we don't care about `NamespaceNode`: overlayfs overlays filesystems
    // (i.e. not namespace subtrees). These directories may not be mounted anywhere.
    // `upper` may be created dynamically whenever write access is required.
    upper: OnceCell<ActiveEntry>,
    lower: Option<ActiveEntry>,

    // `prepare_to_unlink()` may mark `upper` as opaque. In that case we want to skip merging
    // with `lower` in `readdir()`.
    upper_is_opaque: OnceCell<()>,

    parent: Option<Arc<OverlayNode>>,
}

impl OverlayNode {
    fn new(
        stack: Arc<OverlayStack>,
        lower: Option<ActiveEntry>,
        upper: Option<ActiveEntry>,
        parent: Option<Arc<OverlayNode>>,
    ) -> Arc<Self> {
        assert!(upper.is_some() || parent.is_some());

        let upper = match upper {
            Some(entry) => OnceCell::with_value(entry),
            None => OnceCell::new(),
        };

        Arc::new(OverlayNode { stack, upper, lower, upper_is_opaque: OnceCell::new(), parent })
    }

    fn from_fs_node(node: &FsNodeHandle) -> Result<&Arc<Self>, Errno> {
        Ok(&node.downcast_ops::<OverlayNodeOps>().ok_or_else(|| errno!(EIO))?.node)
    }

    fn main_entry(&self) -> &ActiveEntry {
        self.upper.get().or(self.lower.as_ref()).expect("Expected either upper or lower node")
    }

    fn init_fs_node_for_child(
        self: &Arc<OverlayNode>,
        node: &FsNode,
        lower: Option<ActiveEntry>,
        upper: Option<ActiveEntry>,
    ) -> FsNodeHandle {
        let entry = upper.as_ref().or(lower.as_ref()).expect("expect either lower or upper node");
        let ino = entry.entry().node.ino;
        let info = entry.entry().node.info().clone();
        let flags = if entry.entry().node.has_no_privileges() {
            FsNodeFlags::HAS_NO_PRIVILEGES
        } else {
            FsNodeFlags::empty()
        };

        // Parent may be needed to initialize `upper`. We don't need to pass it if we have `upper`.
        let parent = if upper.is_some() { None } else { Some(self.clone()) };

        let overlay_node =
            OverlayNodeOps { node: OverlayNode::new(self.stack.clone(), lower, upper, parent) };
        FsNode::new_uncached(ino, overlay_node, &node.fs(), info, flags)
    }

    /// If the file is currently in the lower FS, then promote it to the upper FS. No-op if the
    /// file is already in the upper FS.
    fn ensure_upper(
        &self,
        current_task: &CurrentTask,
        fs: &FileSystem,
    ) -> Result<&ActiveEntry, Errno> {
        self.ensure_upper_maybe_copy(current_task, UpperCopyMode::CopyAll, fs)
    }

    /// Same as `ensure_upper()`, but allows to skip copying of the file content.
    fn ensure_upper_maybe_copy(
        &self,
        current_task: &CurrentTask,
        copy_mode: UpperCopyMode,
        fs: &FileSystem,
    ) -> Result<&ActiveEntry, Errno> {
        self.upper.get_or_try_init(|| {
            let lower = self.lower.as_ref().expect("lower is expected when upper is missing");
            let parent = self.parent.as_ref().expect("Parent is expected when upper is missing");
            let parent_upper = parent.ensure_upper(current_task, fs)?;
            let name = lower.entry.local_name(&RcuReadScope::new()).to_owned();
            let info = {
                let info = lower.entry.node.info();
                info.clone()
            };
            let cred = info.cred();

            let link_path = if info.mode.is_lnk() {
                let link_target =
                    self.as_mounter(current_task, || lower.entry.node.readlink(current_task))?;
                match link_target {
                    SymlinkTarget::Node(_) => return error!(EIO),
                    SymlinkTarget::Path(path) => Some(path),
                }
            } else {
                None
            };

            let mut copy_up_creds = self.mounter_creds_with_fscred(cred);
            security::fs_node_copy_up(current_task, &lower.entry.node, fs, &mut copy_up_creds);
            let copy_up_creds = Arc::new(copy_up_creds);
            let res = self.as_mounter(current_task, || {
                self.stack.create_upper_entry(
                    current_task,
                    parent_upper,
                    name.as_ref(),
                    |dir, temp_name| {
                        current_task.override_creds(copy_up_creds.clone(), || {
                            dir.create_entry(current_task, temp_name, |dir_node, mount, name| {
                                match &link_path {
                                    Some(target) => dir_node.create_symlink(
                                        current_task,
                                        mount,
                                        name,
                                        target.as_ref(),
                                    ),
                                    None => dir_node.create_node(
                                        current_task,
                                        mount,
                                        name,
                                        info.mode,
                                        info.rdev,
                                    ),
                                }
                            })
                        })
                    },
                    |entry| {
                        if link_path.is_some() {
                            return Ok(());
                        }
                        if info.mode.is_reg() && copy_mode == UpperCopyMode::CopyAll {
                            copy_file_content(current_task, lower, entry)?;
                            if info.mode.intersects(FileMode::ISUID | FileMode::ISGID) {
                                entry.entry().node.fetch_and_refresh_info(current_task)?;
                            }
                        }
                        entry.entry().node.update_attributes(
                            current_task,
                            /* force = */ false,
                            |node_info| {
                                node_info.uid = cred.uid;
                                node_info.gid = cred.gid;
                                node_info.mode = info.mode;
                                Ok(())
                            },
                        )?;
                        if info.mode.is_reg() {
                            match lower.entry.node.get_xattr(
                                current_task,
                                &lower.mount,
                                XATTR_NAME_CAPS.to_bytes().into(),
                                usize::MAX,
                            ) {
                                Ok(ValueOrSize::Value(caps)) => entry.entry().node.set_xattr(
                                    current_task,
                                    entry.mount(),
                                    XATTR_NAME_CAPS.to_bytes().into(),
                                    &caps,
                                    XattrOp::Set,
                                )?,
                                Ok(ValueOrSize::Size(_)) => return error!(EINVAL),
                                // Nothing to copy if the attribute is not set on the lower
                                // node, or if the lower filesystem does not support extended
                                // attributes.
                                Err(err) if matches!(err.code, ENODATA | ENOTSUP | ENOSYS) => {}
                                Err(err) => return Err(err),
                            }
                        }
                        Ok(())
                    },
                )
            });

            track_stub!(TODO("https://fxbug.dev/322874151"), "overlayfs copy xattrs");
            res
        })
    }

    /// Checks if this node exists in the lower FS.
    fn has_lower(&self) -> bool {
        self.lower.is_some()
    }

    /// Check that an item isn't present in the lower FS.
    fn lower_entry_exists(&self, current_task: &CurrentTask, name: &FsStr) -> Result<bool, Errno> {
        match &self.lower {
            Some(lower) => match lower.component_lookup(current_task, name) {
                Ok(entry) => Ok(!entry.is_whiteout()),
                Err(err) if err.code == ENOENT => Ok(false),
                Err(err) => Err(err),
            },
            None => Ok(false),
        }
    }

    /// Helper used to create a new entry in the directory. It first checks that the target node
    /// doesn't exist. Then `do_create` is called to create the new node in the work dir, which
    /// is then moved to the target dir in the upper file system.
    ///
    /// It's assumed that the calling `DirEntry` has the current directory locked, so it is not
    /// supposed to change while this method is executed. Note that OveralayFS doesn't handle
    /// the case when the underlying file systems are changed directly, but that restriction
    /// is not enforced.
    fn create_entry<FCreate, FInit>(
        self: &Arc<OverlayNode>,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        do_create: FCreate,
        do_init: FInit,
    ) -> Result<ActiveEntry, Errno>
    where
        FCreate: Fn(&ActiveEntry, &FsStr) -> Result<ActiveEntry, Errno>,
        FInit: FnOnce(&ActiveEntry) -> Result<(), Errno>,
    {
        let upper = self.ensure_upper(current_task, &node.fs())?;

        match upper.component_lookup(current_task, name) {
            Ok(existing) => {
                // If there is an entry in the upper dir, then it must be a whiteout.
                if !existing.is_whiteout() {
                    return error!(EEXIST);
                }
            }

            Err(e) if e.code == ENOENT => {
                // If we don't have the entry in the upper fs, then check lower.
                if self.lower_entry_exists(current_task, name)? {
                    return error!(EEXIST);
                }
            }
            Err(e) => return Err(e),
        };

        self.stack.create_upper_entry(current_task, upper, name, do_create, do_init)
    }

    /// An overlay directory may appear empty when the corresponding upper dir isn't empty:
    /// it may contain a number of whiteout entries. In that case the whiteouts need to be
    /// unlinked before the upper directory can be unlinked as well.
    /// `prepare_to_unlink()` checks that the directory doesn't contain anything other
    /// than whiteouts and if that is the case then it unlinks all of them.
    fn prepare_to_unlink(self: &Arc<OverlayNode>, current_task: &CurrentTask) -> Result<(), Errno> {
        if self.main_entry().entry().node.is_dir() {
            let mut lower_entries = BTreeSet::new();
            if let Some(dir) = &self.lower {
                for item in dir.read_dir_entries(current_task)?.drain(..) {
                    if !dir.is_whiteout_child(current_task, &item)? {
                        lower_entries.insert(item.name);
                    }
                }
            }

            if let Some(dir) = self.upper.get() {
                let mut to_remove = Vec::<FsString>::new();
                for item in dir.read_dir_entries(current_task)?.drain(..) {
                    if !dir.is_whiteout_child(current_task, &item)? {
                        return error!(ENOTEMPTY);
                    }
                    lower_entries.remove(&item.name);
                    to_remove.push(item.name);
                }

                if !lower_entries.is_empty() {
                    return error!(ENOTEMPTY);
                }

                // Mark the directory as opaque. Children can be removed after this.
                dir.set_opaque_xattr(current_task)?;
                let _ = self.upper_is_opaque.set(());

                // Finally, remove the children.
                for name in to_remove.iter() {
                    dir.entry().unlink(
                        current_task,
                        dir.mount(),
                        name.as_ref(),
                        UnlinkKind::NonDirectory,
                        DirectoryMode::AllowAny,
                    )?;
                }
            }
        }

        Ok(())
    }

    fn as_mounter<R, F: FnOnce() -> R>(&self, current_task: &CurrentTask, do_work: F) -> R {
        current_task.override_creds(self.stack.mounter.clone(), do_work)
    }

    fn mounter_creds_with_fscred(&self, fscred: FsCred) -> Credentials {
        let mut creds = Credentials::clone(&self.stack.mounter);
        creds.fsuid = fscred.uid;
        creds.fsgid = fscred.gid;
        creds
    }
}

struct OverlayNodeOps {
    node: Arc<OverlayNode>,
}

impl OverlayNodeOps {
    /// Creates and initializes a child entry staged through `work` using `owner` as the creator
    /// filesystem identity while keeping `mounter` permissions for upper lookup and rename.
    fn create_child_node<FCreate, FInit>(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        mode: FileMode,
        owner: FsCred,
        do_create: FCreate,
        do_init: FInit,
    ) -> Result<FsNodeHandle, Errno>
    where
        FCreate: Fn(&FsNodeHandle, &MountInfo, &FsStr) -> Result<FsNodeHandle, Errno>,
        FInit: FnOnce(&ActiveEntry) -> Result<(), Errno>,
    {
        let mut creds = self.node.mounter_creds_with_fscred(owner);
        security::dentry_create_files_as(current_task, node, mode, name, &mut creds)?;
        let creds = Arc::new(creds);
        self.node.as_mounter(current_task, || {
            let new_upper_node = self.node.create_entry(
                node,
                current_task,
                name,
                |dir, temp_name| {
                    current_task.override_creds(creds.clone(), || {
                        dir.create_entry(current_task, temp_name, |dir_node, mount, name| {
                            do_create(dir_node, mount, name)
                        })
                    })
                },
                do_init,
            )?;
            Ok(self.node.init_fs_node_for_child(node, None, Some(new_upper_node)))
        })
    }
}

impl FsNodeOps for OverlayNodeOps {
    fn check_access(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        access: security::PermissionFlags,
        info: &DynamicLockDepRwLock<FsNodeInfo>,
        reason: CheckAccessReason,
        audit_context: security::Auditable<'_>,
    ) -> Result<(), Errno> {
        node.default_check_access_impl(current_task, access, reason, info.read(), audit_context)?;

        self.node.as_mounter(current_task, || {
            if let Some(entry) = self.node.upper.get() {
                entry.entry.node.check_access(
                    current_task,
                    entry.mount(),
                    access,
                    reason,
                    audit_context,
                )
            } else {
                let entry = self.node.lower.as_ref().expect("Either upper or lower node is set");
                let lower_node = &entry.entry.node;

                // If the lower node is a regular file, directory or symlink then opening it for
                // write access will cause it to be copied-up, so the mounter only requires read
                // access to the underlying node.
                //
                // If the lower node is "special" (i.e. a device, FIFO or socket) then writes will
                // affect the underlying resource, so to avoid privilege escalation via overlays,
                // the mounter is still required to have write access to the node. This works
                // even if the lower filesystem is readonly because special nodes remain writable
                // in that case (though they may not be modified or unlinked, which would require
                // actually writing to the filesystem).
                let mut access = access;
                if access.contains(PermissionFlags::WRITE) && !lower_node.info().mode.is_special() {
                    // Verify that the mounter will be able to write to copy-up the node.
                    // TODO: https://fxbug.dev/403260093 - Fix this to also verify discretionary
                    // write access to the mounter, while correctly taking into account the
                    // `context=` mount option (if any) for the mandatory write access check.
                    security::fs_node_permission(
                        current_task,
                        node,
                        PermissionFlags::WRITE,
                        audit_context,
                    )?;

                    access |= PermissionFlags::READ;
                    access &= !(PermissionFlags::WRITE | PermissionFlags::APPEND);
                }

                lower_node.check_access(current_task, &entry.mount, access, reason, audit_context)
            }
        })
    }

    fn create_file_ops(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        if node.is_fifo() {
            return Pipe::open(current_task, node.fifo(current_task), flags);
        }
        self.node.as_mounter(current_task, || {
            if flags.can_write() {
                // Only upper FS can be writable.
                let copy_mode = if flags.contains(OpenFlags::TRUNC) {
                    UpperCopyMode::MetadataOnly
                } else {
                    UpperCopyMode::CopyAll
                };
                self.node.ensure_upper_maybe_copy(current_task, copy_mode, &node.fs())?;
            }

            let ops: Box<dyn FileOps> = if node.is_dir() {
                Box::new(OverlayDirectory {
                    node: self.node.clone(),
                    dir_entries: Default::default(),
                })
            } else {
                let state = match (self.node.upper.get(), &self.node.lower) {
                    (Some(upper), _) => {
                        OverlayFileState::Upper(upper.entry().open_anonymous(current_task, flags)?)
                    }
                    (None, Some(lower)) => {
                        OverlayFileState::Lower(lower.entry().open_anonymous(current_task, flags)?)
                    }
                    _ => panic!("Expected either upper or lower node"),
                };

                Box::new(OverlayFile {
                    node: self.node.clone(),
                    flags,
                    state: LockDepRwLock::new(state),
                })
            };

            Ok(ops)
        })
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        self.node.as_mounter(current_task, || {
            let resolve_child = |dir_opt: Option<&ActiveEntry>| {
                // TODO(sergeyu): lookup() checks access, but we don't need that here.
                dir_opt
                    .as_ref()
                    .map(|dir| match dir.component_lookup(current_task, name) {
                        Ok(entry) => Some(Ok(entry)),
                        Err(e) if e.code == ENOENT => None,
                        Err(e) => Some(Err(e)),
                    })
                    .flatten()
                    .transpose()
            };

            let upper: Option<ActiveEntry> = resolve_child(self.node.upper.get())?;

            let (upper_is_dir, upper_is_opaque) = match &upper {
                Some(upper) if upper.is_whiteout() => return error!(ENOENT),
                Some(upper) => {
                    let is_dir = upper.entry().node.is_dir();
                    let is_opaque = !is_dir || upper.is_opaque_node(current_task);
                    (is_dir, is_opaque)
                }
                None => (false, false),
            };

            let parent_upper_is_opaque = self.node.upper_is_opaque.get().is_some();

            // We don't need to resolve the lower node if we have an opaque node in the upper dir.
            let lookup_lower = !parent_upper_is_opaque && !upper_is_opaque;
            let lower: Option<ActiveEntry> = if lookup_lower {
                match resolve_child(self.node.lower.as_ref())? {
                    // If the upper node is a directory and the lower isn't then ignore the lower node.
                    Some(lower) if upper_is_dir && !lower.entry().node.is_dir() => None,
                    Some(lower) if lower.is_whiteout() => None,
                    result => result,
                }
            } else {
                None
            };

            if upper.is_none() && lower.is_none() {
                return error!(ENOENT);
            }

            Ok(self.node.init_fs_node_for_child(&entry.node, lower, upper))
        })
    }

    fn mknod(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        mode: FileMode,
        dev: DeviceId,
        owner: FsCred,
    ) -> Result<FsNodeHandle, Errno> {
        if mode.fmt() == FileMode::IFCHR && dev == DeviceId::NONE {
            // Callers are blocked from creating character device nodes with Id zero, which would
            // be indistuinguishable from those created to represent whiteouts.
            return error!(EPERM);
        }
        self.create_child_node(
            node,
            current_task,
            name,
            mode,
            owner,
            |dir_node, mount, name| dir_node.create_node(current_task, mount, name, mode, dev),
            |_entry| Ok(()),
        )
    }

    fn mkdir(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        mode: FileMode,
        owner: FsCred,
    ) -> Result<FsNodeHandle, Errno> {
        self.create_child_node(
            node,
            current_task,
            name,
            mode,
            owner,
            |dir_node, mount, name| {
                dir_node.create_node(current_task, mount, name, mode, DeviceId::NONE)
            },
            |entry| {
                // Set opaque attribute to ensure the new directory is not merged with lower.
                entry.set_opaque_xattr(current_task)
            },
        )
    }

    fn create_symlink(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        target: &FsStr,
        owner: FsCred,
    ) -> Result<FsNodeHandle, Errno> {
        self.create_child_node(
            node,
            current_task,
            name,
            FileMode::IFLNK,
            owner,
            |dir_node, mount, name| dir_node.create_symlink(current_task, mount, name, target),
            |_entry| Ok(()),
        )
    }

    fn readlink(&self, _node: &FsNode, current_task: &CurrentTask) -> Result<SymlinkTarget, Errno> {
        self.node
            .as_mounter(current_task, || self.node.main_entry().entry().node.readlink(current_task))
    }

    fn link(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        child: &FsNodeHandle,
    ) -> Result<(), Errno> {
        self.node.as_mounter(current_task, || {
            let child_overlay = OverlayNode::from_fs_node(child)?;
            let upper_child = child_overlay.ensure_upper(current_task, &node.fs())?;
            self.node.create_entry(
                node,
                current_task,
                name,
                |dir, temp_name| {
                    dir.create_entry(current_task, temp_name, |dir_node, mount, name| {
                        dir_node.link(current_task, mount, name, &upper_child.entry().node)
                    })
                },
                |_entry| Ok(()),
            )?;
            Ok(())
        })
    }

    fn unlink(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        child: &FsNodeHandle,
    ) -> Result<(), Errno> {
        self.node.as_mounter(current_task, || {
            let upper = self.node.ensure_upper(current_task, &node.fs())?;
            let child_overlay = OverlayNode::from_fs_node(child)?;
            child_overlay.prepare_to_unlink(current_task)?;

            let need_whiteout = self.node.lower_entry_exists(current_task, name)?;
            if need_whiteout {
                self.node.stack.create_upper_entry(
                    current_task,
                    &upper,
                    &name,
                    |work, name| work.create_whiteout(current_task, name),
                    |_entry| Ok(()),
                )?;
            } else if let Some(child_upper) = child_overlay.upper.get() {
                let kind = if child_upper.entry().node.is_dir() {
                    UnlinkKind::Directory
                } else {
                    UnlinkKind::NonDirectory
                };
                upper.entry().unlink(
                    current_task,
                    upper.mount(),
                    name,
                    kind,
                    DirectoryMode::AllowAny,
                )?;
            }

            Ok(())
        })
    }

    fn fetch_and_refresh_info<'a>(
        &self,
        _node: &FsNode,
        current_task: &CurrentTask,
        info: &'a DynamicLockDepRwLock<FsNodeInfo>,
        flags: StatxFlags,
    ) -> Result<LockDepReadGuard<'a, FsNodeInfo>, Errno> {
        self.node.as_mounter(current_task, || {
            let underlying_node = &self.node.main_entry().entry().node;
            // Work-around to ensure that mounter `getattr` access is required when a caller tries
            // to `stat()` a file.
            security::check_fs_node_getattr_access(current_task, underlying_node)?;
            let real_info =
                underlying_node.fetch_and_refresh_info_with_flags(current_task, flags)?.clone();
            let mut lock = info.write();
            *lock = real_info;
            Ok(LockDepWriteGuard::downgrade(lock))
        })
    }

    // Work-around to allow the append-only writes to proceed without `getattr` access checks,
    // which `fetch_and_refresh_info()`, above, would otherwise introduce.
    fn get_size(&self, _node: &FsNode, current_task: &CurrentTask) -> Result<usize, Errno> {
        self.node
            .as_mounter(current_task, || self.node.main_entry().entry().node.get_size(current_task))
    }

    fn update_attributes(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        new_info: &FsNodeInfo,
        has: FsNodeAttrFlags,
    ) -> Result<(), Errno> {
        self.node.as_mounter(current_task, || {
            let upper = self.node.ensure_upper(current_task, &node.fs())?.entry();
            upper.node.update_attributes(
                current_task,
                has.contains(FsNodeAttrFlags::FORCE),
                |info| {
                    if has.contains(FsNodeAttrFlags::TIME_MODIFY) {
                        info.time_modify = new_info.time_modify;
                    }
                    if has.contains(FsNodeAttrFlags::TIME_ACCESS) {
                        info.time_access = new_info.time_access;
                    }
                    if has.contains(FsNodeAttrFlags::MODE) {
                        info.mode = new_info.mode;
                    }
                    if has.contains(FsNodeAttrFlags::UID) {
                        info.uid = new_info.uid;
                    }
                    if has.contains(FsNodeAttrFlags::GID) {
                        info.gid = new_info.gid;
                    }
                    if has.contains(FsNodeAttrFlags::RDEV) {
                        info.rdev = new_info.rdev;
                    }
                    Ok(())
                },
            )
        })
    }

    fn append_lock_read<'a>(
        &'a self,
        node: &'a FsNode,
        current_task: &CurrentTask,
    ) -> Result<RwQueueReadGuard<'a>, Errno> {
        self.node.as_mounter(current_task, || {
            let upper_node = self.node.ensure_upper(current_task, &node.fs())?.entry.node.as_ref();
            upper_node.ops().append_lock_read(upper_node, current_task)
        })
    }

    fn append_lock_write<'a>(
        &'a self,
        node: &'a FsNode,
        current_task: &CurrentTask,
    ) -> Result<RwQueueWriteGuard<'a>, Errno> {
        self.node.as_mounter(current_task, || {
            let upper_node = self.node.ensure_upper(current_task, &node.fs())?.entry.node.as_ref();
            upper_node.ops().append_lock_write(upper_node, current_task)
        })
    }

    fn truncate(
        &self,
        guard: &AppendLockWriteGuard<'_>,
        node: &FsNode,
        current_task: &CurrentTask,
        file: Option<&FileObject>,
        length: u64,
    ) -> Result<(), Errno> {
        self.node.as_mounter(current_task, || {
            let upper = self.node.ensure_upper(current_task, &node.fs())?;
            let upper_file = file
                .and_then(|f| f.downcast_file::<OverlayFile>())
                .and_then(|f| f.upper_file().ok());
            let upper_file = upper_file.as_ref().map(|f| f as &FileObject);
            let upper_node = &upper.entry().node;
            upper_node.ops().truncate(guard, upper_node, current_task, upper_file, length)?;
            upper_node.update_ctime_mtime();
            Ok(())
        })
    }

    fn sync(&self, _node: &FsNode, current_task: &CurrentTask) -> Result<(), Errno> {
        self.node.as_mounter(current_task, || match self.node.upper.get() {
            Some(upper) => upper.entry().node.ops().sync(&upper.entry().node, current_task),
            None => Ok(()),
        })
    }

    fn get_xattr(
        &self,
        _node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        max_size: usize,
    ) -> Result<ValueOrSize<Box<[u8]>>, Errno> {
        let entry = self
            .node
            .upper
            .get()
            .or(self.node.lower.as_ref())
            .expect("expect either lower or upper node");
        self.node.as_mounter(current_task, || {
            entry.entry().node.get_xattr(current_task, &entry.mount, name, max_size)
        })
    }

    fn set_xattr(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
        value: &[u8],
        op: XattrOp,
    ) -> Result<(), Errno> {
        self.node.as_mounter(current_task, || {
            let upper = self.node.ensure_upper(current_task, &node.fs())?;
            upper.entry().node.set_xattr(current_task, &upper.mount, name, value, op)
        })
    }

    fn remove_xattr(
        &self,
        node: &FsNode,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<(), Errno> {
        self.node.as_mounter(current_task, || {
            let upper = self.node.ensure_upper(current_task, &node.fs())?;
            upper.entry().node.remove_xattr(current_task, &upper.mount, name)
        })
    }

    fn list_xattrs(
        &self,
        _node: &FsNode,
        current_task: &CurrentTask,
        max_size: usize,
    ) -> Result<ValueOrSize<Vec<FsString>>, Errno> {
        self.node.as_mounter(current_task, || {
            let entry = self
                .node
                .upper
                .get()
                .or(self.node.lower.as_ref())
                .expect("expect either lower or upper node");
            entry.entry().node.list_xattrs(current_task, max_size)
        })
    }
}
struct OverlayDirectory {
    node: Arc<OverlayNode>,
    dir_entries: LockDepRwLock<DirEntries, OverlayFsDirEntriesLock>,
}

impl OverlayDirectory {
    fn refresh_dir_entries(&self, current_task: &CurrentTask) -> Result<(), Errno> {
        let mut entries = DirEntries::new();

        let upper_is_opaque = self.node.upper_is_opaque.get().is_some();
        let merge_with_lower = self.node.lower.is_some() && !upper_is_opaque;

        // First enumerate entries in the upper dir. Then enumerate the lower dir and add only
        // items that are not present in the upper.
        let mut upper_set = BTreeSet::new();
        if let Some(dir) = self.node.upper.get() {
            for item in dir.read_dir_entries(current_task)?.drain(..) {
                // Fill `upper_set` only if we will need it later.
                if merge_with_lower {
                    upper_set.insert(item.name.clone());
                }
                if !dir.is_whiteout_child(current_task, &item)? {
                    entries.push(item);
                }
            }
        }

        if merge_with_lower {
            if let Some(dir) = &self.node.lower {
                for item in dir.read_dir_entries(current_task)?.drain(..) {
                    if !upper_set.contains(&item.name)
                        && !dir.is_whiteout_child(current_task, &item)?
                    {
                        entries.push(item);
                    }
                }
            }
        }

        *self.dir_entries.write() = entries;

        Ok(())
    }
}

impl FileOps for OverlayDirectory {
    fileops_impl_directory!();

    fn seek(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        current_offset: off_t,
        target: SeekTarget,
    ) -> Result<off_t, Errno> {
        self.node
            .as_mounter(current_task, || default_seek(current_offset, target, || error!(EINVAL)))
    }

    fn readdir(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        self.node.as_mounter(current_task, || {
            if sink.offset() == 0 {
                self.refresh_dir_entries(current_task)?;
            }

            emit_dotdot(file, sink)?;

            for item in self.dir_entries.read().iter().skip(sink.offset() as usize - 2) {
                sink.add(item.inode_num, sink.offset() + 1, item.entry_type, item.name.as_ref())?;
            }

            Ok(())
        })
    }
}

enum OverlayFileState {
    Lower(FileHandle),
    Upper(FileHandle),
}

impl OverlayFileState {
    fn file(&self) -> &FileHandle {
        match self {
            Self::Lower(f) | Self::Upper(f) => f,
        }
    }
}

struct OverlayFile {
    node: Arc<OverlayNode>,
    flags: OpenFlags,
    state: LockDepRwLock<OverlayFileState, OverlayFsStateLock>,
}

impl OverlayFile {
    /// Returns the upper file, or `EBADF` if this file was not opened for writing and promoted.
    fn upper_file(&self) -> Result<FileHandle, Errno> {
        let state = self.state.read();
        match &*state {
            OverlayFileState::Upper(f) => Ok(f.clone()),
            OverlayFileState::Lower(_) => error!(EBADF),
        }
    }
}

impl FileOps for OverlayFile {
    fileops_impl_seekable!();

    fn read(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        self.node.as_mounter(current_task, || {
            let mut state = self.state.read();

            // Check if the file was promoted to the upper FS. In that case we need to reopen it
            // from there.
            if let Some(upper) = self.node.upper.get() {
                if matches!(*state, OverlayFileState::Lower(_)) {
                    std::mem::drop(state);

                    {
                        let mut write_state = self.state.write();

                        // TODO(mariagl): don't hold write_state while calling open_anonymous.
                        // It may call back into read(), causing lock order inversion.
                        *write_state = OverlayFileState::Upper(
                            upper.entry().open_anonymous(current_task, self.flags)?,
                        );
                    }
                    state = self.state.read();
                }
            }

            // TODO(mariagl): Drop state here
            let file = state.file();
            security::file_permission(current_task, &file, security::PermissionFlags::READ)?;
            file.ops().read(file, current_task, offset, data)
        })
    }

    fn write(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        self.node.as_mounter(current_task, || {
            // `write()` should be called only for files that were opened for write, and that
            // required the file to be promoted to the upper FS.
            let file = self.upper_file()?;
            security::file_permission(current_task, &file, security::PermissionFlags::WRITE)?;
            file.ops().write(&file, current_task, offset, data)
        })
    }

    fn fallocate(
        &self,
        guard: &AppendLockWriteGuard<'_>,
        _file: &FileObject,
        current_task: &CurrentTask,
        mode: FallocMode,
        offset: u64,
        length: u64,
    ) -> Result<(), Errno> {
        // `guard` already holds the upper node's append lock (via
        // `OverlayNodeOps::append_lock_write`). Call the upper file's `FileOps` directly to avoid
        // deadlocking in `FileObject::fallocate`, and update upper timestamps manually so
        // `fetch_and_refresh_info` observes them.
        self.node.as_mounter(current_task, || {
            let file = self.upper_file()?;
            security::file_permission(current_task, &file, security::PermissionFlags::WRITE)?;
            file.ops().fallocate(guard, &file, current_task, mode, offset, length)?;
            file.node().update_ctime_mtime();
            Ok(())
        })
    }

    fn sync(&self, _file: &FileObject, current_task: &CurrentTask) -> Result<(), Errno> {
        self.node.as_mounter(current_task, || {
            let state = self.state.read();
            let file = state.file();
            file.ops().sync(file, current_task)
        })
    }

    fn flush(&self, _file: &FileObject, current_task: &CurrentTask) {
        self.node.as_mounter(current_task, || {
            let state = self.state.read();
            let file = state.file();
            file.ops().flush(file, current_task)
        })
    }

    fn get_memory(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        length: Option<usize>,
        prot: starnix_core::mm::ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        self.node.as_mounter(current_task, || {
            let state = self.state.read();
            let file = state.file();
            // Not that the VMO returned here will not updated if the file is promoted to upper FS
            // later. This is consistent with OverlayFS behavior on Linux, see
            // https://docs.kernel.org/filesystems/overlayfs.html#non-standard-behavior .
            file.ops().get_memory(file, current_task, length, prot)
        })
    }
}

pub fn new_overlay_fs(
    current_task: &CurrentTask,
    options: FileSystemOptions,
) -> Result<FileSystemHandle, Errno> {
    OverlayStack::new_fs(current_task, options)
}

pub struct OverlayStack {
    // Keep references to the underlying file systems to ensure they outlive `overlayfs` since
    // they may be unmounted before overlayfs.
    #[allow(unused)]
    lower_fs: FileSystemHandle,
    upper_fs: FileSystemHandle,

    work: ActiveEntry,

    // Used when interacting with the `upper_fs`, `lower_fs` or `work` directories.
    mounter: Arc<Credentials>,
}

impl OverlayStack {
    fn compute_stack_depth(lower_fs: &FileSystemHandle, upper_fs: &FileSystemHandle) -> u32 {
        std::cmp::max(lower_fs.stack_depth(), upper_fs.stack_depth()).saturating_add(1)
    }

    fn stack_depth(&self) -> u32 {
        Self::compute_stack_depth(&self.lower_fs, &self.upper_fs)
    }

    fn build_fs(
        kernel: &Kernel,
        lower: ActiveEntry,
        upper: ActiveEntry,
        prepare_work: impl FnOnce() -> Result<ActiveEntry, Errno>,
        mounter: Arc<Credentials>,
        options: FileSystemOptions,
    ) -> Result<FileSystemHandle, Errno> {
        let lower_fs = lower.entry().node.fs();
        let upper_fs = upper.entry().node.fs();

        if upper_fs.fs_lockdep_type() == FsLockDepType::Recursive {
            // Recursive filesystems (like OverlayFS itself) are not supported as upper filesystems.
            return error!(EINVAL);
        }

        let stack_depth = Self::compute_stack_depth(&lower_fs, &upper_fs);
        if stack_depth > FILESYSTEM_MAX_STACK_DEPTH {
            log_warn!(
                "overlayfs: maximum fs stacking depth exceeded ({} > {})",
                stack_depth,
                FILESYSTEM_MAX_STACK_DEPTH
            );
            return error!(EINVAL);
        }

        let work = prepare_work()?;
        let stack = Arc::new(OverlayStack { lower_fs, upper_fs, work, mounter });

        let root_node = OverlayNode::new(stack.clone(), Some(lower), Some(upper), None);
        let root_info = root_node.main_entry().entry().node.info().clone();
        let fs = FileSystem::new(kernel, CacheMode::Uncached, OverlayFs { stack }, options)?;
        let root_ino = fs.allocate_ino();
        fs.create_root_with_info(root_ino, OverlayNodeOps { node: root_node }, root_info);
        Ok(fs)
    }

    fn new_fs(
        current_task: &CurrentTask,
        options: FileSystemOptions,
    ) -> Result<FileSystemHandle, Errno> {
        match options.params.get("redirect_dir".as_bytes()) {
            None => (),
            Some(o) if o == "off" => (),
            Some(_) => {
                track_stub!(TODO("https://fxbug.dev/322874205"), "overlayfs redirect_dir");
                return error!(ENOTSUP);
            }
        }

        let lower = resolve_dir_param(current_task, &options.params, "lowerdir".into())?;
        let upper = resolve_dir_param(current_task, &options.params, "upperdir".into())?;
        let work_base = resolve_dir_param(current_task, &options.params, "workdir".into())?;

        if !Arc::ptr_eq(&upper.entry().node.fs(), &work_base.entry().node.fs())
            || upper.mount() != work_base.mount()
        {
            log_warn!("overlayfs: upperdir and workdir must be on the same mount");
            return error!(EINVAL);
        }
        if upper.entry().is_descendant_of(work_base.entry())
            || work_base.entry().is_descendant_of(upper.entry())
        {
            log_warn!("overlayfs: upperdir and workdir must not overlap");
            return error!(EINVAL);
        }
        if Arc::ptr_eq(&lower.entry().node.fs(), &work_base.entry().node.fs())
            && (lower.entry().is_descendant_of(work_base.entry())
                || work_base.entry().is_descendant_of(lower.entry()))
        {
            log_warn!("overlayfs: lowerdir and workdir must not overlap");
            return error!(EINVAL);
        }

        Self::build_fs(
            current_task.kernel(),
            lower,
            upper,
            || Self::prepare_work_dir(current_task, &work_base),
            current_task.current_creds().clone(),
            options,
        )
    }

    /// Given a filesystem, wraps it in a tmpfs-backed writable overlayfs.
    pub fn wrap_fs_in_writable_layer(
        current_task: &CurrentTask,
        rootfs: FileSystemHandle,
    ) -> Result<FileSystemHandle, Errno> {
        let kernel = current_task.kernel();
        let lower = ActiveEntry { entry: rootfs.root().clone(), mount: MountInfo::detached() };
        let lower_info = lower.entry().node.fetch_and_refresh_info(current_task)?.clone();

        // Create upper and work directories in an invisible tmpfs.
        let invisible_tmp = TmpFs::new_fs(kernel);
        let create_dir =
            |name: &FsStr, mode: FileMode, cred: FsCred| -> Result<ActiveEntry, Errno> {
                let mut creds = Credentials::clone(&Credentials::root());
                creds.fsuid = cred.uid;
                creds.fsgid = cred.gid;
                current_task.override_creds(Arc::new(creds), || {
                    let create_dir_node = |dir: &FsNodeHandle, mount: &MountInfo, name: &FsStr| {
                        dir.create_node(current_task, mount, name, mode, DeviceId::NONE)
                    };
                    let mount = MountInfo::detached();
                    let root = invisible_tmp.root();
                    let entry = root.create_entry(current_task, &mount, name, create_dir_node)?;
                    Ok(ActiveEntry { entry, mount })
                })
            };
        let upper = create_dir("upper".into(), lower_info.mode, lower_info.cred())?;

        Self::build_fs(
            kernel,
            lower,
            upper,
            || create_dir("work".into(), mode!(IFDIR, 0o700), FsCred::root()),
            Credentials::root(),
            FileSystemOptions::default(),
        )
    }

    // Helper used to create new entry called `name` in `target_dir` in the upper FS.
    // 1. Calls `try_create` to create a new entry in `work`. It is called repeateadly with a
    //    new name until it returns any result other than `EEXIST`.
    // 2. `do_init` is called to initilize the contents and the attributes of the new entry, etc.
    // 3. The new entry is moved to `target_dir`. If there is an existing entry called `name` in
    //    `target_dir` then it's replaced with the new entry.
    // The temp file is cleared from the work dir if either of the last two steps fails.
    fn create_upper_entry<FCreate, FInit>(
        &self,
        current_task: &CurrentTask,
        target_dir: &ActiveEntry,
        name: &FsStr,
        try_create: FCreate,
        do_init: FInit,
    ) -> Result<ActiveEntry, Errno>
    where
        FCreate: Fn(&ActiveEntry, &FsStr) -> Result<ActiveEntry, Errno>,
        FInit: FnOnce(&ActiveEntry) -> Result<(), Errno>,
    {
        let mut rng = rand::rng();
        let (temp_name, entry) = loop {
            let x: u64 = rng.random();
            let temp_name = FsString::from(format!("tmp{:x}", x));
            match try_create(&self.work, temp_name.as_ref()) {
                Err(err) if err.code == EEXIST => continue,
                Err(err) => return Err(err),
                Ok(entry) => break (temp_name, entry),
            }
        };

        do_init(&entry)
            .and_then(|()| {
                DirEntry::rename(
                    current_task,
                    self.work.entry(),
                    self.work.mount(),
                    temp_name.as_ref(),
                    target_dir.entry(),
                    target_dir.mount(),
                    name,
                    RenameFlags::REPLACE_ANY,
                )
            })
            .map_err(|e| {
                // Remove the temp entry in case of a failure.
                let unlink_kind = if entry.entry().node.is_dir() {
                    UnlinkKind::Directory
                } else {
                    UnlinkKind::NonDirectory
                };
                self.work
                    .entry()
                    .unlink(
                        current_task,
                        self.work.mount(),
                        temp_name.as_ref(),
                        unlink_kind,
                        DirectoryMode::AllowAny,
                    )
                    .unwrap_or_else(|e| {
                        log_error!("Failed to cleanup work dir after an error: {}", e)
                    });
                e
            })?;

        Ok(entry)
    }

    fn scan_dir_for_remove(
        current_task: &CurrentTask,
        dir: &ActiveEntry,
    ) -> Result<(Vec<(FsString, ActiveEntry)>, Vec<FsString>), Errno> {
        dir.require_no_mounts()?;
        let mut subdirs = Vec::new();
        let mut non_dirs = Vec::new();
        for item in dir.read_dir_entries(current_task)? {
            let child = dir.component_lookup(current_task, item.name.as_ref())?;
            child.require_no_mounts()?;
            if child.entry().node.is_dir() {
                subdirs.push((item.name, child));
            } else {
                non_dirs.push(item.name);
            }
        }
        Ok((subdirs, non_dirs))
    }

    /// Recursively unlinks `dir` (named `name` under `parent`) and all of its contents.
    fn remove_dir_all(
        current_task: &CurrentTask,
        parent: &ActiveEntry,
        name: &FsStr,
        dir: ActiveEntry,
    ) -> Result<(), Errno> {
        // Walk the subtree before modifying anything so any nested submount aborts cleanly with
        // `EBUSY` before partial unlinking, recording directories in pre-order (unlinked bottom-up
        // via `.rev()`).
        let mut to_remove = Vec::new();
        let mut pending = vec![(parent.clone(), name.to_owned(), dir)];
        while let Some((parent, name, dir)) = pending.pop() {
            let (subdirs, non_dirs) = Self::scan_dir_for_remove(current_task, &dir)?;
            for (child_name, child) in subdirs {
                pending.push((dir.clone(), child_name, child));
            }
            to_remove.push((parent, name, dir, non_dirs));
        }

        for (parent, name, dir, non_dirs) in to_remove.into_iter().rev() {
            for child_name in non_dirs {
                dir.entry().unlink(
                    current_task,
                    dir.mount(),
                    child_name.as_ref(),
                    UnlinkKind::NonDirectory,
                    DirectoryMode::AllowAny,
                )?;
            }
            parent.entry().unlink(
                current_task,
                parent.mount(),
                name.as_ref(),
                UnlinkKind::Directory,
                DirectoryMode::AllowAny,
            )?;
        }
        Ok(())
    }

    /// Prepares a private `work` subdirectory inside `work_base`, clearing any stale contents or
    /// inherited `S_ISGID` mode bits so staged nodes do not inherit `workdir` metadata.
    fn prepare_work_dir(
        current_task: &CurrentTask,
        work_base: &ActiveEntry,
    ) -> Result<ActiveEntry, Errno> {
        match work_base.component_lookup(current_task, "work".into()) {
            Ok(existing) => {
                if !existing.entry().node.is_dir() {
                    log_warn!("overlayfs: workdir/work is not a directory");
                    return error!(EINVAL);
                }
                Self::remove_dir_all(current_task, work_base, "work".into(), existing)?;
            }
            Err(e) if e.code == ENOENT => {}
            Err(e) => return Err(e),
        }

        let work = work_base.create_entry(current_task, "work".into(), |dir, mount, name| {
            dir.create_node(current_task, mount, name, mode!(IFDIR, 0o000), DeviceId::NONE)
        })?;
        // Strip any inherited `S_ISGID`/mode bits so `work` stays `0o000` and does not alter
        // staged children, then verify the mounter can still search and write `work` (e.g. holds
        // `CAP_DAC_OVERRIDE` / SELinux `write` + `search`).
        let init_result = (|| -> Result<(), Errno> {
            if work.entry().node.fetch_and_refresh_info(current_task)?.mode != mode!(IFDIR, 0o000) {
                work.entry().node.update_attributes(
                    current_task,
                    /* force = */ false,
                    |node_info| {
                        node_info.mode = mode!(IFDIR, 0o000);
                        Ok(())
                    },
                )?;
            }
            work.entry().node.check_access(
                current_task,
                work.mount(),
                PermissionFlags::WRITE | PermissionFlags::EXEC,
                CheckAccessReason::InternalPermissionChecks,
                work.entry(),
            )
        })();
        if let Err(err) = init_result {
            work_base
                .entry()
                .unlink(
                    current_task,
                    work_base.mount(),
                    "work".into(),
                    UnlinkKind::Directory,
                    DirectoryMode::AllowAny,
                )
                .unwrap_or_else(|e| log_error!("Failed to cleanup work dir after an error: {}", e));
            return Err(err);
        }
        Ok(work)
    }
}

struct OverlayFs {
    stack: Arc<OverlayStack>,
}

impl FileSystemOps for OverlayFs {
    fn fs_lockdep_type(&self) -> FsLockDepType {
        FsLockDepType::Recursive
    }

    fn stack_depth(&self) -> u32 {
        self.stack.stack_depth()
    }

    fn statfs(&self, _fs: &FileSystem, current_task: &CurrentTask) -> Result<statfs, Errno> {
        current_task
            .override_creds(self.stack.mounter.clone(), || self.stack.upper_fs.statfs(current_task))
    }

    fn sync(&self, _fs: &FileSystem, current_task: &CurrentTask) -> Result<(), Errno> {
        current_task
            .override_creds(self.stack.mounter.clone(), || self.stack.upper_fs.sync(current_task))
    }

    fn name(&self) -> &'static FsStr {
        "overlay".into()
    }

    fn is_storage_backed(&self) -> bool {
        self.stack.lower_fs.is_storage_backed() || self.stack.upper_fs.is_storage_backed()
    }

    fn rename(
        &self,
        _fs: &FileSystem,
        current_task: &CurrentTask,
        context: &mut RenameContext<'_>,
        old_name: &FsStr,
        new_name: &FsStr,
    ) -> Result<(), Errno> {
        let old_parent = &context.old_parent().node;
        let new_parent = &context.new_parent().node;
        let renamed = &context.renamed.node;
        current_task.override_creds(self.stack.mounter.clone(), || {
            let renamed_overlay = OverlayNode::from_fs_node(renamed)?;
            if renamed_overlay.has_lower() && renamed_overlay.main_entry().entry().node.is_dir() {
                // Return EXDEV for directory renames. Potentially they may be handled with the
                // `redirect_dir` feature, but it's not implemented here yet.
                // See https://docs.kernel.org/filesystems/overlayfs.html#renaming-directories
                return error!(EXDEV);
            }
            renamed_overlay.ensure_upper(current_task, &renamed.fs())?;

            let old_parent_overlay = OverlayNode::from_fs_node(old_parent)?;
            let old_parent_upper = old_parent_overlay.ensure_upper(current_task, &renamed.fs())?;

            let new_parent_overlay = OverlayNode::from_fs_node(new_parent)?;
            let new_parent_upper = new_parent_overlay.ensure_upper(current_task, &renamed.fs())?;

            let need_whiteout = old_parent_overlay.lower_entry_exists(current_task, old_name)?;

            DirEntry::rename(
                current_task,
                old_parent_upper.entry(),
                old_parent_upper.mount(),
                old_name,
                new_parent_upper.entry(),
                new_parent_upper.mount(),
                new_name,
                RenameFlags::REPLACE_ANY,
            )?;

            // If the old node existed in lower FS, then override it in the upper FS with a
            // whiteout.
            if need_whiteout {
                match old_parent_upper.create_whiteout(current_task, old_name) {
                    Err(e) => log_warn!("overlayfs: failed to create whiteout for {old_name}: {e}"),
                    Ok(_) => (),
                }
            }

            Ok(())
        })
    }

    fn unmount(&self) {}

    fn sub_filesystems(&self) -> Vec<Arc<FileSystem>> {
        vec![self.stack.lower_fs.clone(), self.stack.upper_fs.clone()]
    }
}

/// Helper used to resolve directories passed in mount options. The directory is resolved in the
/// namespace of the calling process, but only `DirEntry` is returned (detached from the
/// namespace). The corresponding file systems may be unmounted before overlayfs that uses them.
fn resolve_dir_param(
    current_task: &CurrentTask,
    params: &MountParams,
    name: &FsStr,
) -> Result<ActiveEntry, Errno> {
    let path = params.get(&**name).ok_or_else(|| {
        log_error!("overlayfs: {name} was not specified");
        errno!(EINVAL)
    })?;

    current_task
        .open_file(path.as_ref(), OpenFlags::RDONLY | OpenFlags::DIRECTORY)
        .map(|f| ActiveEntry { entry: f.name.entry.clone(), mount: f.name.mount.clone() })
        .map_err(|e| {
            log_error!("overlayfs: Failed to lookup {path}: {}", e);
            e
        })
}

/// Copies file content from one file to another.
fn copy_file_content(
    current_task: &CurrentTask,
    from: &ActiveEntry,
    to: &ActiveEntry,
) -> Result<(), Errno> {
    let from_file = from.entry().open_anonymous(current_task, OpenFlags::RDONLY)?;
    let to_file = to.entry().open_anonymous(current_task, OpenFlags::WRONLY)?;

    security::fs_node_permission(
        current_task,
        from_file.node().as_ref(),
        security::PermissionFlags::READ,
        (&**from_file).into(),
    )?;
    security::fs_node_permission(
        current_task,
        to_file.node().as_ref(),
        security::PermissionFlags::WRITE,
        (&**to_file).into(),
    )?;

    const BUFFER_SIZE: usize = 4096;

    let mut read_offset = 0;
    let mut write_offset = 0;
    loop {
        // TODO(sergeyu): Reuse buffer between iterations.

        let mut output_buffer = VecOutputBuffer::new(BUFFER_SIZE);
        let bytes_read =
            from_file.ops().read(&from_file, current_task, read_offset, &mut output_buffer)?;
        if bytes_read == 0 {
            break;
        }
        read_offset += bytes_read;

        let buffer: Vec<u8> = output_buffer.into();
        let mut input_buffer = VecInputBuffer::from(buffer);
        while input_buffer.available() > 0 {
            write_offset +=
                to_file.ops().write(&to_file, current_task, write_offset, &mut input_buffer)?;
        }
    }

    to_file.ops().data_sync(&to_file, current_task)?;

    Ok(())
}

#[cfg(test)]
mod test {
    use super::*;
    use fidl_fuchsia_io as fio;
    use fxfs_testing::{Hooks, TestFixture, TestFixtureOptions};
    use starnix_core::fs::fuchsia::RemoteFs;
    use starnix_core::testing::spawn_kernel_and_run;
    use starnix_core::vfs::{
        FdFlags, LookupContext, Namespace, NamespaceNode, WhatToMount, fileops_impl_noop_sync,
        fileops_impl_unbounded_seek, fs_node_impl_dir_readonly,
    };
    use starnix_uapi::mount_flags::MountpointFlags;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;

    /// Returns an Fxfs fixture, a channel to its root and a count of its transaction commits.
    async fn new_fxfs() -> (TestFixture, zx::Channel, Arc<AtomicUsize>, Hooks<'static>) {
        let commits = Arc::new(AtomicUsize::new(0));
        let commits_clone = commits.clone();
        let (mut hooks, fs_hooks) = Hooks::new();
        hooks.set_pre_commit(move |_transaction| {
            commits_clone.fetch_add(1, Ordering::SeqCst);
            Ok(())
        });
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(16384, 512)),
            TestFixtureOptions { hooks: Some(fs_hooks), ..Default::default() },
        )
        .await;
        let (server, client) = zx::Channel::create();
        fixture.root().clone(server.into()).expect("clone failed");
        (fixture, client, commits, hooks)
    }

    /// Mounts `remote` at /remote and returns an overlay whose upper layer is on it.
    fn new_overlay(current_task: &CurrentTask, remote: zx::Channel) -> FileSystemHandle {
        let rights = fio::PERM_READABLE | fio::PERM_WRITABLE;
        let remote = RemoteFs::new_fs(current_task.kernel(), remote, Default::default(), rights)
            .expect("new_fs failed");
        remote.root().create_dir_for_testing(current_task, "upper".into()).expect("mkdir failed");
        remote.root().create_dir_for_testing(current_task, "work".into()).expect("mkdir failed");
        let root = current_task.fs().root();
        root.entry.create_dir_for_testing(current_task, "lower".into()).expect("mkdir failed");
        root.entry.create_dir_for_testing(current_task, "remote".into()).expect("mkdir failed");
        root.lookup_child(current_task, &mut LookupContext::default(), "remote".into())
            .expect("lookup failed")
            .mount(WhatToMount::Fs(remote), MountpointFlags::empty())
            .expect("mount failed");
        let params = "lowerdir=/lower,upperdir=/remote/upper,workdir=/remote/work";
        let params = MountParams::parse(params.into()).expect("parse failed");
        new_overlay_fs(current_task, FileSystemOptions { params, ..Default::default() })
            .expect("new_overlay_fs failed")
    }

    /// Creates and writes a file in `dir`, leaving unflushed data in Fxfs.
    fn write_new_file(current_task: &CurrentTask, dir: &NamespaceNode) -> FileHandle {
        let file = dir
            .create_node(current_task, "file".into(), mode!(IFREG, 0o666), DeviceId::NONE)
            .expect("create_node failed")
            .open(current_task, OpenFlags::RDWR)
            .expect("open failed");
        file.write(current_task, &mut VecInputBuffer::new(b"hello")).expect("write failed");
        file
    }

    #[::fuchsia::test]
    async fn test_close_flushes_upper_file() {
        let (fixture, remote, commits, _hooks) = new_fxfs().await;
        spawn_kernel_and_run(async move |current_task| {
            let root = Namespace::new(new_overlay(current_task, remote)).root();
            let file = write_new_file(current_task, &root);
            let fd = current_task.add_file(file, FdFlags::empty()).expect("add_file failed");

            let commits_before = commits.load(Ordering::SeqCst);
            current_task.files().close(fd).expect("close failed");
            current_task.trigger_delayed_releaser();
            assert!(commits.load(Ordering::SeqCst) > commits_before);
        })
        .await;
        fixture.close().await;
    }

    // These rely on Fxfs flushing all file data on directory sync.
    #[::fuchsia::test]
    async fn test_dir_fsync_syncs_upper_dir() {
        let (fixture, remote, commits, _hooks) = new_fxfs().await;
        spawn_kernel_and_run(async move |current_task| {
            let root = Namespace::new(new_overlay(current_task, remote)).root();
            let _file = write_new_file(current_task, &root);
            let dir = root.open(current_task, OpenFlags::RDONLY).expect("open failed");

            let commits_before = commits.load(Ordering::SeqCst);
            dir.sync(current_task).expect("fsync failed");
            assert!(commits.load(Ordering::SeqCst) > commits_before);
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_syncfs_syncs_upper_fs() {
        let (fixture, remote, commits, _hooks) = new_fxfs().await;
        spawn_kernel_and_run(async move |current_task| {
            let overlay = new_overlay(current_task, remote);
            let _file = write_new_file(current_task, &Namespace::new(overlay.clone()).root());

            let commits_before = commits.load(Ordering::SeqCst);
            overlay.sync(current_task).expect("syncfs failed");
            assert!(commits.load(Ordering::SeqCst) > commits_before);
        })
        .await;
        fixture.close().await;
    }

    #[::fuchsia::test]
    async fn test_wrap_fs_in_writable_layer_preserves_root_mode() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let lower = TmpFs::new_fs_with_options(
                kernel,
                FileSystemOptions {
                    params: MountParams::parse(b"mode=0755,uid=42,gid=84".into())
                        .expect("parse params"),
                    ..Default::default()
                },
            )
            .expect("new_fs_with_options");
            let overlay = OverlayStack::wrap_fs_in_writable_layer(current_task, lower)
                .expect("wrap_fs_in_writable_layer");

            let info = overlay.root().node.info();
            assert_eq!(info.mode, mode!(IFDIR, 0o755));
            assert_eq!(info.uid, 42);
            assert_eq!(info.gid, 84);
            drop(info);

            let refreshed = overlay
                .root()
                .node
                .fetch_and_refresh_info(current_task)
                .expect("fetch_and_refresh_info");
            assert_eq!(refreshed.mode, mode!(IFDIR, 0o755));
            assert_eq!(refreshed.uid, 42);
            assert_eq!(refreshed.gid, 84);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_overlay_stack_depth() {
        spawn_kernel_and_run(async move |current_task| {
            let kernel = current_task.kernel();
            let base = TmpFs::new_fs(kernel);
            assert_eq!(base.stack_depth(), 0);

            let overlay1 = OverlayStack::wrap_fs_in_writable_layer(current_task, base)
                .expect("wrap first layer");
            assert_eq!(overlay1.stack_depth(), 1);

            let overlay2 = OverlayStack::wrap_fs_in_writable_layer(current_task, overlay1)
                .expect("wrap second layer");
            assert_eq!(overlay2.stack_depth(), 2);

            assert_eq!(
                OverlayStack::wrap_fs_in_writable_layer(current_task, overlay2).err(),
                Some(errno!(EINVAL))
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_read_dir_entries_loops_multiple_pages() {
        struct PagedDirFile {
            calls: Arc<AtomicUsize>,
        }
        impl FileOps for PagedDirFile {
            fileops_impl_directory!();
            fileops_impl_noop_sync!();
            fileops_impl_unbounded_seek!();

            fn readdir(
                &self,
                file: &FileObject,
                _current_task: &CurrentTask,
                sink: &mut dyn DirentSink,
            ) -> Result<(), Errno> {
                self.calls.fetch_add(1, Ordering::SeqCst);
                match sink.offset() {
                    0 => emit_dotdot(file, sink),
                    2 => sink.add(10, 3, DirectoryEntryType::REG, "a".into()),
                    3 => sink.add(11, 4, DirectoryEntryType::REG, "b".into()),
                    4 => sink.add(12, 5, DirectoryEntryType::REG, "c".into()),
                    _ => Ok(()),
                }
            }
        }

        struct PagedDirNode {
            calls: Arc<AtomicUsize>,
        }
        impl FsNodeOps for PagedDirNode {
            fs_node_impl_dir_readonly!();

            fn create_file_ops(
                &self,
                _node: &FsNode,
                _current_task: &CurrentTask,
                _flags: OpenFlags,
            ) -> Result<Box<dyn FileOps>, Errno> {
                Ok(Box::new(PagedDirFile { calls: self.calls.clone() }))
            }

            fn lookup(
                &self,
                _entry: &DirEntry,
                _current_task: &CurrentTask,
                _name: &FsStr,
            ) -> Result<FsNodeHandle, Errno> {
                error!(ENOENT)
            }
        }

        spawn_kernel_and_run(async move |current_task| {
            let calls = Arc::new(AtomicUsize::new(0));
            let fs = TmpFs::new_fs(current_task.kernel());
            let node = fs.create_node(
                fs.allocate_ino(),
                PagedDirNode { calls: calls.clone() },
                FsNodeInfo::new(mode!(IFDIR, 0o755), FsCred::root()),
            );
            let dir = ActiveEntry {
                entry: DirEntry::new(node, None, "paged".into()),
                mount: MountInfo::detached(),
            };
            let names: Vec<FsString> = dir
                .read_dir_entries(current_task)
                .expect("read_dir_entries")
                .into_iter()
                .map(|entry| entry.name)
                .collect();
            assert_eq!(names, vec![FsString::from("a"), FsString::from("b"), FsString::from("c")]);
            assert_eq!(calls.load(Ordering::SeqCst), 5);
        })
        .await;
    }
}
