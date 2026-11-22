// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::security;
use crate::task::CurrentTask;
use crate::vfs::{
    CheckAccessReason, DirectoryMode, FileHandle, FileObject, FileSystemHandle,
    FileSystemRenameToken, FsLockDepType, FsNodeHandle, FsNodeInfo, FsNodeLinkBehavior, FsStr,
    FsString, FscryptNodePolicy, LookupVec, MountInfo, Mounts, NamespaceNode, PathBuilder,
    UnlinkKind, inotify_hook, path,
};
use atomic_bitflags::atomic_bitflags;
use bitflags::bitflags;
use bstr::ByteSlice;
use fuchsia_rcu::{RcuArc, RcuReadScope};
use fuchsia_sync::ResetDependencies;
use fxfs_unicode::{CasefoldStr, utf8_bytes};
use smallvec::SmallVec;
use starnix_rcu::RcuString;
use starnix_sync::{
    DirEntryChildrenLevel, DirEntryChildrenRecursiveLevel, DynamicLockDepRwLock,
    FuseDirEntryChildrenLevel, LockDepGuard, LockDepWriteGuard, allow_subclass,
};
use starnix_uapi::auth::CAP_FOWNER;
use starnix_uapi::errors::{ENOENT, Errno};
use starnix_uapi::file_mode::{Access, FileMode};
use starnix_uapi::inotify_mask::InotifyMask;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{NAME_MAX, RENAME_EXCHANGE, RENAME_NOREPLACE, RENAME_WHITEOUT, error};
use std::collections::BTreeMap;
use std::fmt;
use std::ops::Deref;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Weak};

bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct RenameFlags: u32 {
        // Exchange the entries.
        const EXCHANGE = RENAME_EXCHANGE;

        // Don't overwrite an existing DirEntry.
        const NOREPLACE = RENAME_NOREPLACE;

        // Create a "whiteout" object to replace the file.
        const WHITEOUT = RENAME_WHITEOUT;

        // Allow replacing any file with a directory. This is an internal flag used only
        // internally inside Starnix for OverlayFS.
        const REPLACE_ANY = 1 << 31;

        // Internal flags that cannot be passed to `sys_rename()`
        const INTERNAL = Self::REPLACE_ANY.bits();
    }
}

pub trait DirEntryOps: Send + Sync + 'static {
    /// Revalidate the [`DirEntry`], if needed.
    ///
    /// Most filesystems don't need to do any revalidations because they are "local"
    /// and all changes to nodes go through the kernel. However some filesystems
    /// allow changes to happen through other means (e.g. NFS, FUSE) and these
    /// filesystems need a way to let the kernel know it may need to refresh its
    /// cached metadata. This method provides that hook for such filesystems.
    ///
    /// For more details, see:
    ///  - https://www.halolinux.us/kernel-reference/the-dentry-cache.html
    ///  - https://www.kernel.org/doc/html/latest/filesystems/path-lookup.html#revalidation-and-automounts
    ///  - https://lwn.net/Articles/649115/
    ///  - https://www.infradead.org/~mchehab/kernel_docs/filesystems/path-walking.html
    ///
    /// Returns `Ok(valid)` where `valid` indicates if the `DirEntry` is still valid,
    /// or an error.
    fn revalidate(&self, _: &CurrentTask, _: &DirEntry) -> Result<bool, Errno> {
        Ok(true)
    }
}

atomic_bitflags! {
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct DirEntryFlags: u8 {
        /// Whether this directory entry has been removed from the tree.
        const IS_DEAD = 1 << 0;

        /// Whether the entry has filesystems mounted on top of it.
        const HAS_MOUNTS = 1 << 1;

        /// Whether the entry has been used since it was inserted into the dentry cache of its
        /// file system, or since that cache last gave it a second chance. Set by
        /// `DirEntry::record_dcache_use` and cleared by `FileSystem::purge_old_entries` and
        /// `FileSystem::forget_dir_entry_uses`.
        const DCACHE_REFERENCED = 1 << 2;

        /// Whether this entry was looked up or created while its parent directory was locked
        /// (i.e. using an fscrypt no-key name) rather than a plaintext name. Such entries are
        /// never inserted into their parent's `children` cache, cannot be renamed or unlinked
        /// once the parent's encryption key is added, and do not cache child entries until
        /// spliced onto a plaintext name.
        const NO_KEY = 1 << 3;
    }
}

pub struct DefaultDirEntryOps;

impl DirEntryOps for DefaultDirEntryOps {}

/// An entry in a directory.
///
/// This structure assigns a name to an FsNode in a given file system. An
/// FsNode might have multiple directory entries, for example if there are more
/// than one hard link to the same FsNode. In those cases, each hard link will
/// have a different parent and a different local_name because each hard link
/// has its own DirEntry object.
///
/// A directory cannot have more than one hard link, which means there is a
/// single DirEntry for each Directory FsNode. That invariant lets us store the
/// children for a directory in the DirEntry rather than in the FsNode.
pub struct DirEntry {
    /// The FsNode referenced by this DirEntry.
    ///
    /// A given FsNode can be referenced by multiple DirEntry objects, for
    /// example if there are multiple hard links to a given FsNode.
    pub node: FsNodeHandle,

    /// The [`DirEntryOps`] for this `DirEntry`.
    ///
    /// The `DirEntryOps` are implemented by the individual file systems to provide
    /// specific behaviours for this `DirEntry`.
    ops: Box<dyn DirEntryOps>,

    /// The parent DirEntry.
    ///
    /// The DirEntry tree has strong references from child-to-parent and weak
    /// references from parent-to-child. This design ensures that the parent
    /// chain is always populated in the cache, but some children might be
    /// missing from the cache.
    parent: RcuArc<DirEntry>,

    /// The [`DirEntryFlags`] for this `DirEntry`.
    flags: AtomicDirEntryFlags,

    /// The name that this parent calls this child.
    ///
    /// This name might not be reflected in the full path in the namespace that
    /// contains this DirEntry. For example, this DirEntry might be the root of
    /// a chroot.
    ///
    /// Most callers that want to work with names for DirEntries should use the
    /// NamespaceNodes.
    local_name: RcuString,

    /// A partial cache of the children of this DirEntry.
    ///
    /// DirEntries are added to this cache when they are looked up and removed
    /// when they are no longer referenced.
    ///
    // FIXME(b/379929394): The lock ordering here assumes parent-to-child lock acquisition, which
    // a number of algorithms in the DirEntry operations also assume. This assumption can be broken
    // by the rename operation, which can move nodes around the hierarchy. See the referenced bug
    // for more details, the current mitigations, and potentials for long-term solutions.
    children: DynamicLockDepRwLock<DirEntryChildren>,
}

/// Canonical live directory entry for a directory [`FsNode`], along with its parent.
#[derive(Default)]
pub(crate) struct CanonicalDirEntry {
    pub parent: Weak<DirEntry>,
    pub entry: Weak<DirEntry>,
}

pub type DirEntryHandle = Arc<DirEntry>;

impl DirEntry {
    #[allow(clippy::let_and_return)]
    pub fn new_uncached(
        node: FsNodeHandle,
        parent: Option<DirEntryHandle>,
        local_name: FsString,
    ) -> DirEntryHandle {
        let ops = parent
            .as_ref()
            .and_then(|parent| parent.node.create_child_dir_entry_ops())
            .unwrap_or_else(|| node.create_dir_entry_ops());
        let fs_lockdep_type = node.fs().fs_lockdep_type();
        let casefold = node.info().casefold;
        let initial_children = DirEntryChildren::new(casefold);
        let result = Arc::new(DirEntry {
            node,
            ops,
            parent: RcuArc::new(parent),
            flags: Default::default(),
            local_name: local_name.into(),
            children: match fs_lockdep_type {
                FsLockDepType::Normal => {
                    DynamicLockDepRwLock::new::<DirEntryChildrenLevel>(initial_children)
                }
                FsLockDepType::Recursive => {
                    DynamicLockDepRwLock::new::<DirEntryChildrenRecursiveLevel>(initial_children)
                }
                FsLockDepType::Fuse => {
                    DynamicLockDepRwLock::new::<FuseDirEntryChildrenLevel>(initial_children)
                }
            },
        });
        #[cfg(any(test, debug_assertions))]
        {
            // Taking this lock tells the lock tracing system about the parent/child ordering
            // relation.
            let _token = allow_subclass();
            let _l1 = result.children.read();
        }
        result
    }

    pub fn new(
        node: FsNodeHandle,
        parent: Option<DirEntryHandle>,
        local_name: FsString,
    ) -> DirEntryHandle {
        let result = Self::new_uncached(node, parent, local_name);
        result.node.fs().did_create_dir_entry(&result);
        result
    }

    /// Returns a new DirEntry for the given `node` without parent. The entry has no local name and
    /// is not cached.
    pub fn new_unrooted(node: FsNodeHandle) -> DirEntryHandle {
        Self::new_uncached(node, None, FsString::default())
    }

    /// Returns a new `DirEntry` that is ready marked as having been deleted.
    pub fn new_deleted(
        node: FsNodeHandle,
        parent: Option<DirEntryHandle>,
        local_name: FsString,
    ) -> DirEntryHandle {
        let entry = DirEntry::new_uncached(node, parent, local_name);
        entry.raise_flags(DirEntryFlags::IS_DEAD);
        entry
    }

    /// Returns a file handle to this entry, associated with an anonymous namespace.
    pub fn open_anonymous(
        self: &DirEntryHandle,
        current_task: &CurrentTask,
        flags: OpenFlags,
    ) -> Result<FileHandle, Errno> {
        let ops = self.node.create_file_ops(current_task, flags)?;
        FileObject::new(current_task, ops, NamespaceNode::new_anonymous(self.clone()), flags)
    }

    fn lock_children<'a>(self: &'a DirEntryHandle) -> DirEntryLockedChildren<'a> {
        DirEntryLockedChildren { entry: self, children: self.children.write() }
    }

    /// Sets whether this directory is casefolded.
    ///
    /// # Errors
    ///
    /// * [`EPERM`]: If the caller neither owns the inode nor has `CAP_FOWNER`.
    /// * [`ENOTDIR`]: If this entry is not a directory.
    /// * [`ENOTSUP`]: If enabling casefolding on a filesystem without casefold support.
    /// * [`ENOTEMPTY`]: If the directory is not empty.
    /// * Propagates errors from [`FsNode::update_attributes`].
    pub fn set_casefold(&self, current_task: &CurrentTask, casefold: bool) -> Result<(), Errno> {
        // From `ioctl_iflags(2)`:
        //   In order to change the inode flags of a file using the FS_IOC_SETFLAGS
        //   operation, the effective user ID of the caller must match the owner of
        //   the file, or the caller must have the CAP_FOWNER capability.
        let (uid, current_casefold) = {
            let info = self.node.info();
            (info.uid, info.casefold)
        };
        if uid != current_task.current_creds().fsuid {
            security::check_task_capable(current_task, CAP_FOWNER)?;
        }
        if current_casefold == casefold {
            return Ok(());
        }
        if !self.node.is_dir() {
            return error!(ENOTDIR);
        }
        if casefold && !self.node.fs().has_casefold_support() {
            return error!(ENOTSUP);
        }

        // It is important that the `children` write lock is held across the call to
        // `update_attributes` below because it acts as the guard preventing the directory's
        // case-sensitivity from changing concurrently. Elsewhere, we make decisions based on the
        // state of `children` to determine whether a directory is case-folded or not.
        let mut children = self.children.write();
        if children.is_casefold() == casefold {
            return Ok(());
        }

        self.node.update_attributes(current_task, /* force = */ false, |info| {
            if info.uid != current_task.current_creds().fsuid {
                security::check_task_capable(current_task, CAP_FOWNER)?;
            }
            info.casefold = casefold;
            Ok(())
        })?;

        children.set_casefold(casefold);
        Ok(())
    }

    /// The parent DirEntry.
    pub fn parent(&self) -> Option<DirEntryHandle> {
        self.parent.upgrade()
    }

    /// Set the parent of this DirEntry.
    fn set_parent(self: &DirEntryHandle, parent: DirEntryHandle, is_dir: bool) {
        if is_dir {
            *self.node.canonical_dir_entry.lock() =
                CanonicalDirEntry { parent: Arc::downgrade(&parent), entry: Arc::downgrade(self) };
        }
        self.parent.update(Some(parent));
    }

    fn clear_canonical_dir_entry(&self) {
        let mut canonical = self.node.canonical_dir_entry.lock();
        if std::ptr::eq(canonical.entry.as_ptr(), self) {
            *canonical = Default::default();
        }
    }

    /// The parent DirEntry object or this DirEntry if this entry is the root.
    ///
    /// Useful when traversing up the tree if you always want to find a parent
    /// (e.g., for "..").
    ///
    /// Be aware that the root of one file system might be mounted as a child
    /// in another file system. For that reason, consider walking the
    /// NamespaceNode tree (which understands mounts) rather than the DirEntry
    /// tree.
    pub fn parent_or_self(self: &DirEntryHandle) -> DirEntryHandle {
        self.parent().unwrap_or_else(|| self.clone())
    }

    /// The name that this parent calls this child.
    ///
    /// The reference is only valid for the duration of the RCU read scope.
    pub fn local_name<'a>(&self, scope: &'a RcuReadScope) -> &'a FsStr {
        self.local_name.read(scope)
    }

    /// Whether the given name has special semantics as a directory entry.
    ///
    /// Specifically, whether the name is empty (which means "self"), dot
    /// (which also means "self"), or dot dot (which means "parent").
    pub fn is_reserved_name(name: &FsStr) -> bool {
        name.is_empty() || name == "." || name == ".."
    }

    /// Returns the flags of this DirEntry.
    pub fn flags(&self) -> DirEntryFlags {
        self.flags.load(Ordering::Acquire)
    }

    /// Raises the flags of this DirEntry.
    ///
    /// Returns the flags of this DirEntry before the flags were raised.
    pub fn raise_flags(&self, flags: DirEntryFlags) -> DirEntryFlags {
        if flags.contains(DirEntryFlags::IS_DEAD) {
            self.clear_canonical_dir_entry();
        }
        self.flags.fetch_or(flags, Ordering::AcqRel)
    }

    /// Lowers the flags of this DirEntry.
    ///
    /// Returns the flags of this DirEntry before the flags were lowered.
    pub fn lower_flags(&self, flags: DirEntryFlags) -> DirEntryFlags {
        self.flags.fetch_and(!flags, Ordering::AcqRel)
    }

    /// Returns true if this DirEntry is dead.
    pub fn is_dead(&self) -> bool {
        self.flags().contains(DirEntryFlags::IS_DEAD)
    }

    /// Returns true if this DirEntry was resolved via a ciphertext no-key name while its parent
    /// directory lacked its encryption key.
    pub fn has_nokey_name(&self) -> bool {
        self.flags().contains(DirEntryFlags::NO_KEY)
    }

    /// Returns true if this directory is allowed to cache children (neither a no-key entry nor a
    /// locked encrypted directory).
    fn can_cache_children(&self, current_task: &CurrentTask) -> bool {
        !self.has_nokey_name() && self.node.fail_if_locked(current_task, &self.node.info()).is_ok()
    }

    /// Returns true if this directory is allowed to cache children using a pre-acquired
    /// `FsNodeInfo` guard.
    fn can_cache_children_with_info(&self, current_task: &CurrentTask, info: &FsNodeInfo) -> bool {
        !self.has_nokey_name() && self.node.fail_if_locked(current_task, info).is_ok()
    }

    /// Look up a directory entry with the given name as direct child of this
    /// entry.
    pub fn component_lookup(
        self: &DirEntryHandle,
        current_task: &CurrentTask,
        mount: &MountInfo,
        name: &FsStr,
    ) -> Result<DirEntryHandle, Errno> {
        let (node, _) =
            self.get_or_create_child(current_task, mount, name, |d, _mount, name| {
                Ok((d.node.ops().lookup(d, current_task, name)?, CreationStatus::Existed))
            })?;
        Ok(node)
    }

    /// Looks up multiple path components in sequence starting from `self`.
    ///
    /// Traversal stops at the first lookup error, or after resolving a component that has a
    /// submount in `mount` or is not a directory (so callers may receive fewer than `names.len()`
    /// `Ok` entries, unlike [`FsNodeOps::lookup_pipelined`]).
    pub fn get_children_pipelined(
        self: &DirEntryHandle,
        current_task: &CurrentTask,
        mount: &MountInfo,
        names: &[&FsStr],
    ) -> LookupVec<Result<DirEntryHandle, Errno>> {
        let mut nodes = LookupVec::new();
        let mut results = LookupVec::new();
        let mut current_parent = self.clone();
        let mut unlocked_policy: Option<FscryptNodePolicy> = None;
        for i in 0..names.len() {
            let mut next_node = nodes.pop();
            if next_node.is_some() {
                match current_parent.node.info().encryption_policy {
                    Some(policy) if unlocked_policy != Some(policy) => {
                        // Do not consume prefetched child nodes across an encryption policy
                        // boundary unless that policy was already unlocked before pipelining
                        // started; re-run lookup from `current_parent` so `get_or_create_child`
                        // observes its lock state directly.
                        nodes.clear();
                        next_node = None;
                    }
                    None => {
                        unlocked_policy = None;
                    }
                    Some(_) => {}
                }
            }
            match current_parent.get_or_create_child(
                current_task,
                mount,
                names[i],
                |d, _mount, _name| {
                    let node = if let Some(node) = next_node {
                        node?
                    } else {
                        unlocked_policy = {
                            let info = d.node.info();
                            info.encryption_policy
                                .filter(|_| d.node.fail_if_locked(current_task, &info).is_ok())
                        };
                        nodes = d.node.ops().lookup_pipelined(d, current_task, &names[i..]);
                        nodes.reverse();
                        nodes.pop().expect("lookup_pipelined must return at least one result")?
                    };
                    Ok((node, CreationStatus::Existed))
                },
            ) {
                Ok((entry, _)) => {
                    results.push(Ok(entry.clone()));
                    // Stop pipelined traversal at mountpoints and non-directories (such as
                    // symlinks) so `NamespaceNode::lookup_children` can resolve the mount or
                    // symlink before looking up remaining path components.
                    if entry.has_submount(mount) || !entry.node.is_dir() {
                        break;
                    }
                    current_parent = entry;
                }
                Err(e) => {
                    results.push(Err(e));
                    break;
                }
            }
        }
        results
    }

    /// Creates a new DirEntry
    ///
    /// The create_node_fn function is called to create the underlying FsNode
    /// for the DirEntry.
    ///
    /// If the entry already exists, create_node_fn is not called, and EEXIST is
    /// returned.
    pub fn create_entry(
        self: &DirEntryHandle,
        current_task: &CurrentTask,
        mount: &MountInfo,
        name: &FsStr,
        create_node_fn: impl FnOnce(&FsNodeHandle, &MountInfo, &FsStr) -> Result<FsNodeHandle, Errno>,
    ) -> Result<DirEntryHandle, Errno> {
        let (entry, status) =
            self.create_entry_internal(current_task, mount, name, create_node_fn)?;
        if status == CreationStatus::Existed {
            return error!(EEXIST);
        }
        Ok(entry)
    }

    /// Creates a new DirEntry. Works just like create_entry, except if the entry already exists,
    /// it is returned. Also returns whether the entry was created.
    pub fn get_or_create_entry(
        self: &DirEntryHandle,
        current_task: &CurrentTask,
        mount: &MountInfo,
        name: &FsStr,
        create_node_fn: impl FnOnce(&FsNodeHandle, &MountInfo, &FsStr) -> Result<FsNodeHandle, Errno>,
    ) -> Result<(DirEntryHandle, CreationStatus), Errno> {
        self.create_entry_internal(current_task, mount, name, create_node_fn)
    }

    fn create_entry_internal(
        self: &DirEntryHandle,
        current_task: &CurrentTask,
        mount: &MountInfo,
        name: &FsStr,
        create_node_fn: impl FnOnce(&FsNodeHandle, &MountInfo, &FsStr) -> Result<FsNodeHandle, Errno>,
    ) -> Result<(DirEntryHandle, CreationStatus), Errno> {
        if DirEntry::is_reserved_name(name) {
            return error!(EEXIST);
        }
        // TODO: Do we need to check name for embedded NUL characters?
        if name.contains(&path::SEPARATOR) {
            return error!(EINVAL);
        }
        let (entry, status) =
            self.get_or_create_child(current_task, mount, name, |d, mount, name| {
                match d.node.ops().lookup(d, current_task, name) {
                    Ok(node) => Ok((node, CreationStatus::Existed)),
                    Err(e) if e == ENOENT => {
                        Ok((create_node_fn(&d.node, mount, name)?, CreationStatus::Created))
                    }
                    Err(e) => Err(e),
                }
            })?;
        if status == CreationStatus::Created {
            // An entry was created. Update the ctime and mtime of this directory.
            self.node.update_ctime_mtime();
            entry.notify_creation();
        }
        Ok((entry, status))
    }

    // Creates a directory with mode 0o777 on a detached mount for testing.
    #[cfg(test)]
    pub fn create_dir(
        self: &DirEntryHandle,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<DirEntryHandle, Errno> {
        self.create_dir_for_testing(current_task, name)
    }

    // Creates a directory with mode 0o777 on a detached mount for testing.
    pub fn create_dir_for_testing(
        self: &DirEntryHandle,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<DirEntryHandle, Errno> {
        // TODO: apply_umask
        self.create_entry(current_task, &MountInfo::detached(), name, |dir, mount, name| {
            dir.create_node(
                current_task,
                mount,
                name,
                starnix_uapi::file_mode::mode!(IFDIR, 0o777),
                starnix_uapi::device_id::DeviceId::NONE,
            )
        })
    }

    /// Creates an anonymous file.
    ///
    /// The FileMode::IFMT of the FileMode is always FileMode::IFREG.
    ///
    /// Used by O_TMPFILE.
    pub fn create_tmpfile(
        self: &DirEntryHandle,
        current_task: &CurrentTask,
        mount: &MountInfo,
        mode: FileMode,
        flags: OpenFlags,
    ) -> Result<DirEntryHandle, Errno> {
        // Only directories can have children.
        if !self.node.is_dir() {
            return error!(ENOTDIR);
        }
        assert!(mode.is_reg());

        // From <https://man7.org/linux/man-pages/man2/open.2.html>:
        //
        //   Specifying O_EXCL in conjunction with O_TMPFILE prevents a
        //   temporary file from being linked into the filesystem in
        //   the above manner.  (Note that the meaning of O_EXCL in
        //   this case is different from the meaning of O_EXCL
        //   otherwise.)
        let link_behavior = if flags.contains(OpenFlags::EXCL) {
            FsNodeLinkBehavior::Disallowed
        } else {
            FsNodeLinkBehavior::Allowed
        };

        let node = self.node.create_tmpfile(current_task, mount, mode, link_behavior)?;
        let local_name = format!("#{}", node.ino).into();
        Ok(DirEntry::new_deleted(node, Some(self.clone()), local_name))
    }

    pub fn unlink(
        self: &DirEntryHandle,
        current_task: &CurrentTask,
        mount: &MountInfo,
        name: &FsStr,
        kind: UnlinkKind,
        directory_mode: DirectoryMode,
    ) -> Result<(), Errno> {
        assert!(!DirEntry::is_reserved_name(name));

        // child_to_unlink *must* be dropped after self_children (even in the error paths).
        let child_to_unlink;

        let mut self_children = self.lock_children();
        child_to_unlink = self_children.component_lookup(current_task, mount, name)?;
        child_to_unlink.require_no_mounts(mount)?;

        // Check that this filesystem entry must be a directory. This can
        // happen if the path terminates with a trailing slash.
        //
        // Example: If we're unlinking a symlink `/foo/bar/`, this would
        // result in `ENOTDIR` because of the trailing slash, even if
        // `UnlinkKind::NonDirectory` was used.
        if directory_mode == DirectoryMode::MustBeDirectory && !child_to_unlink.node.is_dir() {
            return error!(ENOTDIR);
        }

        match kind {
            UnlinkKind::Directory => {
                if !child_to_unlink.node.is_dir() {
                    return error!(ENOTDIR);
                }
            }
            UnlinkKind::NonDirectory => {
                if child_to_unlink.node.is_dir() {
                    return error!(EISDIR);
                }
            }
        }

        if child_to_unlink.has_nokey_name()
            && self.node.fail_if_locked(current_task, &self.node.info()).is_ok()
        {
            return error!(ENOENT);
        }

        self.node.unlink(current_task, mount, name, &child_to_unlink.node)?;
        self_children.children.remove(name);
        child_to_unlink.clear_canonical_dir_entry();

        std::mem::drop(self_children);
        child_to_unlink.destroy(&current_task.kernel().mounts);

        Ok(())
    }

    /// Destroy this directory entry.
    ///
    /// Notice that this method takes `self` by value to destroy this reference.
    fn destroy(self: DirEntryHandle, mounts: &Mounts) {
        let was_already_dead =
            self.raise_flags(DirEntryFlags::IS_DEAD).contains(DirEntryFlags::IS_DEAD);
        if was_already_dead {
            return;
        }
        let unmount =
            self.lower_flags(DirEntryFlags::HAS_MOUNTS).contains(DirEntryFlags::HAS_MOUNTS);
        self.node.fs().will_destroy_dir_entry(&self);
        if unmount {
            mounts.unmount(&self);
        }
        self.notify_deletion();
    }

    /// Returns whether this entry is a descendant of |other|, comparing `DirEntry` identity.
    ///
    /// Use this for namespace and mount checks, where mounts are keyed by `DirEntry`. Use
    /// [`Self::is_descendant_node_of`] for checks on the directory structure itself.
    pub fn is_descendant_of(self: &DirEntryHandle, other: &DirEntryHandle) -> bool {
        if Arc::ptr_eq(self, other) {
            return true;
        }
        let mut current = self.parent();
        while let Some(entry) = current {
            if Arc::ptr_eq(&entry, other) {
                // We found |other|.
                return true;
            }
            current = entry.parent();
        }
        // We reached the root of the file system.
        false
    }

    /// Returns whether this entry is a descendant of |other|, comparing `FsNode` identity.
    ///
    /// Two distinct `DirEntry` handles can wrap the same directory `FsNode` (e.g. a no-key
    /// handle and a canonical plaintext handle for an encrypted directory). Rename lock ordering
    /// and cycle detection must treat such handles as the same directory, so they use this
    /// rather than [`Self::is_descendant_of`].
    fn is_descendant_node_of(self: &DirEntryHandle, other: &DirEntryHandle) -> bool {
        if Arc::ptr_eq(&self.node, &other.node) {
            return true;
        }
        let mut current = self.parent();
        while let Some(entry) = current {
            if Arc::ptr_eq(&entry.node, &other.node) {
                return true;
            }
            current = entry.parent();
        }
        false
    }

    /// Rename the file with old_basename in old_parent to new_basename in
    /// new_parent.
    ///
    /// old_parent and new_parent must belong to the same file system.
    pub fn rename(
        current_task: &CurrentTask,
        old_parent: &DirEntryHandle,
        old_mount: &MountInfo,
        old_basename: &FsStr,
        new_parent: &DirEntryHandle,
        new_mount: &MountInfo,
        new_basename: &FsStr,
        flags: RenameFlags,
    ) -> Result<(), Errno> {
        // The nodes we are touching must be part of the same mount.
        if old_mount != new_mount {
            return error!(EXDEV);
        }

        // The mounts are equals, choose one.
        let mount = old_mount;

        // If either the old_basename or the new_basename is a reserved name
        // (e.g., "." or ".."), then we cannot do the rename.
        if DirEntry::is_reserved_name(old_basename) || DirEntry::is_reserved_name(new_basename) {
            if flags.contains(RenameFlags::NOREPLACE) {
                return error!(EEXIST);
            }
            return error!(EBUSY);
        }

        // Destination entry names must fit within `NAME_MAX` (for `RENAME_EXCHANGE`, `old_basename`
        // also becomes a destination entry name).
        if new_basename.len() > NAME_MAX as usize
            || (flags.contains(RenameFlags::EXCHANGE) && old_basename.len() > NAME_MAX as usize)
        {
            return error!(ENAMETOOLONG);
        }

        // If the names and parents are the same, then there's nothing to do
        // and we can report success.
        if Arc::ptr_eq(&old_parent.node, &new_parent.node) && old_basename == new_basename {
            if flags.contains(RenameFlags::NOREPLACE) {
                return error!(EEXIST);
            }
            return Ok(());
        }

        // This task must have write access to the old and new parent nodes.
        old_parent.node.check_access(
            current_task,
            mount,
            Access::WRITE,
            CheckAccessReason::InternalPermissionChecks,
            old_parent,
        )?;
        new_parent.node.check_access(
            current_task,
            mount,
            Access::WRITE,
            CheckAccessReason::InternalPermissionChecks,
            new_parent,
        )?;

        // The mount check ensures that the nodes we're touching are part of the
        // same file system. It doesn't matter where we grab the FileSystem reference from.
        let fs = old_parent.node.fs();

        // We need to hold these DirEntryHandles until after we drop all the
        // locks so that we do not deadlock when we drop them.
        let renamed;
        let lookup_replaced;
        let mut maybe_replaced = None;
        let mut peer_handles = SmallVec::<[DirEntryHandle; 2]>::new();
        let mut evicted_peer_entries = SmallVec::<[DirEntryHandle; 2]>::new();
        let is_cross_handle_casefold_self_rename;
        let mut register_replaced_in_dcache = false;
        let mut evict_replaced_from_dcache = false;
        let mut register_renamed_in_dcache = false;
        let mut evict_renamed_from_dcache = false;

        {
            // Before we take any locks, we need to take the rename mutex on
            // the file system. This lock ensures that no other rename
            // operations are happening in this file system while we're
            // analyzing this rename operation.
            //
            // For example, we grab writer locks on both old_parent and
            // new_parent. If there was another rename operation in flight with
            // old_parent and new_parent reversed, then we could deadlock while
            // trying to acquire these locks.
            let _lock = RenameLock::new(&fs, old_parent, new_parent);

            let same_parent_entry = Arc::ptr_eq(old_parent, new_parent);
            let same_parent_node = Arc::ptr_eq(&old_parent.node, &new_parent.node);

            // We cannot simply grab the locks on old_parent and new_parent
            // independently because old_parent and new_parent might be the
            // same directory entry (or distinct entries sharing an FsNode).
            // Instead, we use the RenameGuard helper to grab the appropriate locks.
            let mut state = RenameGuard::lock(old_parent, new_parent);

            // Now that we know the old_parent child list cannot change, we
            // establish the DirEntry that we are going to try to rename.
            renamed =
                state.old_parent_children().component_lookup(current_task, mount, old_basename)?;

            // We need to check if there is already a DirEntry with
            // new_basename in new_parent. If so, there are additional checks
            // we need to perform.
            // This must be done BEFORE locking info to avoid self-deadlock.
            let new_parent_was_locked =
                new_parent.node.fail_if_locked(current_task, &new_parent.node.info()).is_err();
            lookup_replaced =
                state.new_parent_children().component_lookup(current_task, mount, new_basename);
            let new_lookup_used_nokey = new_parent_was_locked
                || new_parent.node.fail_if_locked(current_task, &new_parent.node.info()).is_err();

            // If the target entry is an ancestor of the source parent, the
            // rename would create a cycle (for EXCHANGE) or attempt to
            // overwrite a non-empty directory.
            // We check this before acquiring child locks to avoid
            // deadlocks from bottom-up locking (locking ancestor after
            // descendant).
            // This is done early (before parent locks) as a fail-fast
            // optimization since we already have the lookup result.
            if let Ok(replaced) = &lookup_replaced {
                if old_parent.is_descendant_node_of(replaced) {
                    if flags.contains(RenameFlags::EXCHANGE) {
                        return error!(EINVAL);
                    } else {
                        return error!(ENOTEMPTY);
                    }
                }
            }

            // Lock the info for the parents to ensure that subsequent checks on their
            // state (e.g., sticky bit checks) and the actual rename operation are not racy.
            let mut state =
                state.lock_info(old_parent, new_parent, &renamed, lookup_replaced.as_ref().ok());

            // If new_parent is a descendant of renamed, the operation would
            // create a cycle. That's disallowed.
            if new_parent.is_descendant_node_of(&renamed) {
                return error!(EINVAL);
            }

            is_cross_handle_casefold_self_rename = !same_parent_entry
                && same_parent_node
                && state.old_parent_info().casefold
                && canonicalize_name(old_basename) == canonicalize_name(new_basename);

            // Check whether the sticky bit on the old parent prevents us from
            // removing this child.
            {
                // Safe because the parent is locked first, and then we check the
                // sticky bit of the child. This parent -> child acquisition follows
                // the hierarchical lock ordering.
                let _token = allow_subclass();
                old_parent.node.check_sticky_bit(
                    current_task,
                    &renamed.node,
                    state.old_parent_info(),
                )?;
            }

            // Check whether the renamed entry (or a cached peer entry under another DirEntry
            // handle for the same parent FsNode) is a mountpoint.
            // TODO: We should hold a read lock on the mount points for this
            //       namespace to prevent the child from becoming a mount point
            //       while this function is executing.
            renamed.require_no_mounts(mount)?;
            if !same_parent_entry && same_parent_node {
                if let Some(peer_renamed) =
                    state.new_parent_children().children.get(old_basename).and_then(Weak::upgrade)
                {
                    peer_handles.push(peer_renamed);
                    peer_handles.last().unwrap().require_no_mounts(mount)?;
                }
            }

            // We lookup the replaced entry before locking info to avoid deadlock, but we match
            // on the result here under the parent info locks. This ensures that checks on the
            // replaced entry (e.g., existence, directory status, identity) are consistent and
            // do not race with concurrent operations.
            match &lookup_replaced {
                Ok(replaced) => {
                    // Set `maybe_replaced` now to ensure it gets dropped in the right order.
                    let replaced = maybe_replaced.insert(replaced.clone());

                    if flags.contains(RenameFlags::NOREPLACE) {
                        // From <https://man7.org/linux/man-pages/man2/rename.2.html>:
                        //
                        //   RENAME_NOREPLACE
                        //          Don't overwrite newpath of the rename. Return an error
                        //          if newpath already exists.
                        return error!(EEXIST);
                    }

                    if Arc::ptr_eq(&renamed.node, &replaced.node) {
                        if !Arc::ptr_eq(&renamed, &replaced)
                            && !is_cross_handle_casefold_self_rename
                        {
                            // From <https://man7.org/linux/man-pages/man2/rename.2.html>:
                            //
                            // "If oldpath and newpath are existing hard links referring to the
                            // same file, then rename() does nothing, and returns a success
                            // status."
                            return Ok(());
                        }

                        // Same directory entry (e.g. case-only rename in a casefolded directory).
                        // With EXCHANGE, swapping an entry with itself is a no-op that succeeds.
                        // Without EXCHANGE, we must proceed to rename on the underlying filesystem
                        // to update the on-disk casing.
                        if flags.contains(RenameFlags::EXCHANGE) {
                            return Ok(());
                        }
                    }

                    // Sayeth https://man7.org/linux/man-pages/man2/rename.2.html:
                    //
                    // "oldpath can specify a directory.  In this case, newpath must"
                    // either not exist, or it must specify an empty directory."
                    if state.replaced_is_dir() {
                        // Check whether the replaced entry (or a cached peer entry under another
                        // DirEntry handle for the same parent FsNode) is a mountpoint.
                        // TODO: We should hold a read lock on the mount points for this
                        //       namespace to prevent the child from becoming a mount point
                        //       while this function is executing.
                        replaced.require_no_mounts(mount)?;
                        if !same_parent_entry && same_parent_node {
                            if let Some(peer_replaced) = state
                                .old_parent_children()
                                .children
                                .get(new_basename)
                                .and_then(Weak::upgrade)
                            {
                                peer_handles.push(peer_replaced);
                                peer_handles.last().unwrap().require_no_mounts(mount)?;
                            }
                        }
                    }

                    if !flags.intersects(RenameFlags::EXCHANGE | RenameFlags::REPLACE_ANY) {
                        let renamed_is_dir = state.renamed_is_dir();
                        let replaced_is_dir = state.replaced_is_dir();
                        if renamed_is_dir && !replaced_is_dir {
                            return error!(ENOTDIR);
                        } else if !renamed_is_dir && replaced_is_dir {
                            return error!(EISDIR);
                        }
                    }
                }
                // It's fine for the lookup to fail to find a child.
                Err(errno) if *errno == ENOENT => {
                    if flags.contains(RenameFlags::EXCHANGE) {
                        return error!(ENOENT);
                    }
                }
                // However, other errors are fatal.
                Err(e) => return Err(e.clone()),
            }

            security::check_fs_node_rename_access(
                current_task,
                &old_parent.node,
                &renamed.node,
                &new_parent.node,
                maybe_replaced.as_ref().map(|dir_entry| dir_entry.node.deref().as_ref()),
                old_basename,
                new_basename,
            )?;

            if let Some(replaced) = maybe_replaced.as_ref() {
                // Safe because the parent is locked first, and then we check the
                // sticky bit of the child. This parent -> child acquisition
                // follows the hierarchical lock ordering.
                let _token = allow_subclass();
                new_parent.node.check_sticky_bit(
                    current_task,
                    &replaced.node,
                    state.new_parent_info().unwrap_or_else(|| state.old_parent_info()),
                )?;
            }

            let renamed_is_dir = state.renamed_is_dir();
            let replaced_is_dir = state.replaced_is_dir();

            // In a casefolded directory, renaming an entry to a case variant of itself resolves
            // `lookup_replaced` to the same directory entry (`renamed` or its peer handle on the
            // same parent `FsNode`). At the filesystem level, no distinct directory entry is being
            // overwritten or replaced, so clear `state.replaced` to `None` before invoking the
            // filesystem operation.
            if state
                .replaced
                .is_some_and(|r| Arc::ptr_eq(&renamed, r) || is_cross_handle_casefold_self_rename)
            {
                state.replaced = None;
            }

            // Match Linux `__fscrypt_prepare_rename`: renaming into or out of a locked encrypted
            // directory, or using a source/target lookup that ran while its parent lacked its
            // encryption key before a concurrent unlock, fails with `ENOKEY`.
            old_parent.node.fail_if_locked(current_task, state.old_parent_info())?;
            if let Some(info) = state.new_parent_info() {
                new_parent.node.fail_if_locked(current_task, info)?;
            }
            if renamed.has_nokey_name()
                || maybe_replaced.as_ref().is_some_and(|r| r.has_nokey_name())
                || new_lookup_used_nokey
            {
                return error!(ENOKEY);
            }

            if flags.contains(RenameFlags::EXCHANGE) {
                fs.exchange(current_task, &mut state, old_basename, new_basename)?;
            } else {
                fs.rename(current_task, &mut state, old_basename, new_basename)?;
            }

            let renamed_was_cached = state
                .old_parent_children()
                .children
                .get(old_basename)
                .is_some_and(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(&renamed)));
            let replaced_was_cached = maybe_replaced.as_ref().is_some_and(|r| {
                state
                    .new_parent_children()
                    .children
                    .get(new_basename)
                    .is_some_and(|w| std::ptr::eq(w.as_ptr(), Arc::as_ptr(r)))
            });
            let new_parent_info_ref =
                state.new_parent_info().unwrap_or_else(|| state.old_parent_info());
            let new_parent_can_cache =
                new_parent.can_cache_children_with_info(current_task, new_parent_info_ref);
            let old_parent_can_cache =
                old_parent.can_cache_children_with_info(current_task, state.old_parent_info());

            let mut record_evicted_peer = |weak: Option<Weak<DirEntry>>| {
                if let Some(peer) = weak.and_then(|w| w.upgrade()) {
                    if !Arc::ptr_eq(&peer, &renamed)
                        && !maybe_replaced.as_ref().is_some_and(|r| Arc::ptr_eq(&peer, r))
                    {
                        peer.clear_canonical_dir_entry();
                        evicted_peer_entries.push(peer);
                    }
                }
            };

            if flags.contains(RenameFlags::EXCHANGE) {
                if !same_parent_entry && same_parent_node {
                    record_evicted_peer(state.new_parent_children().children.remove(old_basename));
                    record_evicted_peer(state.old_parent_children().children.remove(new_basename));
                }
                // Reparent `replaced` when exchanging.
                let replaced =
                    maybe_replaced.as_ref().expect("replaced expected with RENAME_EXCHANGE");
                replaced.set_parent(old_parent.clone(), replaced_is_dir);
                replaced.local_name.update(old_basename.to_owned());
                if old_parent_can_cache {
                    state
                        .old_parent_children()
                        .children
                        .insert(old_basename, Arc::downgrade(replaced));
                    register_replaced_in_dcache = !replaced_was_cached;
                } else {
                    state.old_parent_children().children.remove(old_basename);
                    evict_replaced_from_dcache = replaced_was_cached;
                }
            } else {
                if let Some(replaced) = maybe_replaced.as_ref() {
                    if !Arc::ptr_eq(&renamed, replaced) {
                        replaced.clear_canonical_dir_entry();
                    }
                }
                // Remove the renamed child from the old_parent's child list.
                // In casefolded directories, removing old_basename before inserting new_basename
                // prevents removal from clobbering the freshly inserted entry when old and new
                // names canonicalize to the same key.
                state.old_parent_children().children.remove(old_basename);
                if !same_parent_entry && same_parent_node {
                    record_evicted_peer(state.new_parent_children().children.remove(old_basename));
                    record_evicted_peer(state.old_parent_children().children.remove(new_basename));
                }
            }

            // We need to update the parent and local name for the DirEntry
            // we are renaming to reflect its new parent and its new name.
            renamed.set_parent(new_parent.clone(), renamed_is_dir);
            renamed.local_name.update(new_basename.to_owned());

            // Add the renamed child to `new_parent`'s child cache unless `new_parent` cannot
            // cache children (e.g. an unspliced no-key entry).
            if new_parent_can_cache {
                state.new_parent_children().children.insert(new_basename, Arc::downgrade(&renamed));
                register_renamed_in_dcache = !renamed_was_cached;
            } else {
                state.new_parent_children().children.remove(new_basename);
                evict_renamed_from_dcache = renamed_was_cached;
            }

            // Lock ordering is enforced from parent-to-child, and therefore we need to
            // reset the lock ordering constraints when we reorder the tree nodes. `_lock` does
            // the same for the parents when it is dropped.
            // SAFETY: We manually clear the dependency graph for these locks.
            // This is safe because `fs.rename_mutex` is held during this operation, which
            // prevents the tree topology from changing concurrently. This allows us to safely
            // dynamically enforce a sound locking order (e.g. by memory address in `RenameGuard`)
            // to avoid deadlocks. Clearing the graph prevents false-positive cycle panics from
            // `tracing-mutex` after the node is reparented.
            unsafe {
                if let Some(replaced) =
                    maybe_replaced.as_ref().filter(|_| flags.contains(RenameFlags::EXCHANGE))
                {
                    replaced.children.reset_dependencies();
                    replaced.node.info_lock().reset_dependencies();
                }
                renamed.children.reset_dependencies();
                renamed.node.info_lock().reset_dependencies();
            }
        };

        for peer in &evicted_peer_entries {
            fs.will_destroy_dir_entry(peer);
        }
        if let Some(replaced) = maybe_replaced.as_ref() {
            if register_replaced_in_dcache {
                fs.did_create_dir_entry(replaced);
            } else if evict_replaced_from_dcache {
                fs.will_destroy_dir_entry(replaced);
            }
        }
        if register_renamed_in_dcache {
            fs.did_create_dir_entry(&renamed);
        } else if evict_renamed_from_dcache {
            fs.will_destroy_dir_entry(&renamed);
        }

        fs.purge_old_entries();

        if let Some(replaced) = maybe_replaced {
            if !flags.contains(RenameFlags::EXCHANGE) && !Arc::ptr_eq(&renamed, &replaced) {
                if is_cross_handle_casefold_self_rename {
                    fs.will_destroy_dir_entry(&replaced);
                } else {
                    replaced.destroy(&current_task.kernel().mounts);
                }
            }
        }

        // Renaming a file updates its ctime.
        renamed.node.update_ctime();

        let mode = renamed.node.info().mode;
        if let Some(hook) =
            current_task.kernel().expando.peek::<Arc<dyn inotify_hook::NotifyHook>>()
        {
            let cookie = hook.get_next_cookie();
            old_parent.node.notify(InotifyMask::MOVE_FROM, cookie, old_basename, mode, false);
            new_parent.node.notify(InotifyMask::MOVE_TO, cookie, new_basename, mode, false);
            renamed.node.notify(InotifyMask::MOVE_SELF, 0, Default::default(), mode, false);
        }

        Ok(())
    }

    pub(crate) fn get_children<F, T>(&self, callback: F) -> T
    where
        F: FnOnce(&DirEntryChildren) -> T,
    {
        let children = self.children.read();
        callback(&children)
    }

    /// Remove the child with the given name from the children cache.  The child must not have any
    /// mounts.
    pub fn remove_child(&self, name: &FsStr, mounts: &Mounts) {
        self.remove_child_if(name, mounts, |_| true);
    }

    /// Removes the child with the given name from the children cache if `predicate` returns `true`
    /// while holding the children write lock. The child must not have any mounts.
    pub fn remove_child_if(
        &self,
        name: &FsStr,
        mounts: &Mounts,
        predicate: impl FnOnce(&DirEntryHandle) -> bool,
    ) {
        // Declare `child` before `children` so any last reference drops after releasing the lock.
        let child;
        let mut children = self.children.write();
        child = children.get(name).and_then(|weak| weak.upgrade());
        if child.as_ref().is_none_or(predicate) {
            children.remove(name);
            if let Some(child) = &child {
                child.clear_canonical_dir_entry();
            }
            std::mem::drop(children);
            if let Some(child) = child {
                child.destroy(mounts);
            }
        }
    }

    /// Verifies that `name` is not a reserved `.` or `..` entry, that this entry is a directory,
    /// and that `current_task` has search (`EXEC`) permission on it.
    fn check_search_access(
        &self,
        current_task: &CurrentTask,
        mount: &MountInfo,
        name: &FsStr,
    ) -> Result<(), Errno> {
        assert!(!DirEntry::is_reserved_name(name));
        // Only directories can have children.
        if !self.node.is_dir() {
            return error!(ENOTDIR);
        }
        // The user must be able to search the directory (requires the EXEC permission).
        self.node.check_access(
            current_task,
            mount,
            Access::EXEC,
            CheckAccessReason::InternalPermissionChecks,
            self,
        )
    }

    /// Records a use of this cached entry for the dentry cache of its file system, if `can_cache`
    /// returns true.
    ///
    /// The use is recorded with `DirEntryFlags::DCACHE_REFERENCED`: an LRU dentry cache does not
    /// move entries when they are used, but gives entries with the flag set a second chance in
    /// `FileSystem::purge_old_entries`, which clears it again. Other dentry caches ignore the flag.
    /// A use only needs to be recorded once until the flag is cleared, so for an entry that is used
    /// often this is a single atomic load and `can_cache` is not called.
    fn record_dcache_use(&self, can_cache: impl FnOnce() -> bool) {
        if !self.flags.load(Ordering::Relaxed).contains(DirEntryFlags::DCACHE_REFERENCED)
            && can_cache()
        {
            self.flags.fetch_or(DirEntryFlags::DCACHE_REFERENCED, Ordering::Relaxed);
        }
    }

    /// Clears `DirEntryFlags::DCACHE_REFERENCED` and returns whether it was set.
    pub(super) fn clear_dcache_referenced(&self) -> bool {
        self.flags
            .fetch_and(!DirEntryFlags::DCACHE_REFERENCED, Ordering::Relaxed)
            .contains(DirEntryFlags::DCACHE_REFERENCED)
    }

    /// Retrieves a child `DirEntry` from the cache or obtains one via `create_fn`.
    ///
    /// Checks the in-memory child cache. A cached child is validated using `revalidate()` and
    /// evicted if revalidation fails. On a cache miss, or after evicting a stale child, calls
    /// `create_fn` to acquire the underlying `FsNode` and its `CreationStatus`. Nodes returned by
    /// `create_fn` are not revalidated, and `create_fn` is called at most once.
    fn get_or_create_child(
        self: &DirEntryHandle,
        current_task: &CurrentTask,
        mount: &MountInfo,
        name: &FsStr,
        create_fn: impl FnOnce(
            &DirEntryHandle,
            &MountInfo,
            &FsStr,
        ) -> Result<(FsNodeHandle, CreationStatus), Errno>,
    ) -> Result<(DirEntryHandle, CreationStatus), Errno> {
        self.check_search_access(current_task, mount, name)?;

        // Check if the child is already in children. In that case, we can simply return the
        // child if it is still valid, and we do not need to call create_fn.
        //
        // The cache is queried in its own statement so that the children lock is released before
        // the child is revalidated or evicted.
        let child = self.children.read().get(name).and_then(Weak::upgrade);
        if let Some(child) = child {
            if !child.has_nokey_name() {
                if child.ops.revalidate(current_task, &child)? {
                    // Do not cache a child in a locked or no-key directory.
                    child.record_dcache_use(|| self.can_cache_children(current_task));
                    return Ok((child, CreationStatus::Existed));
                }
                // The cached child is stale. Evict it so that create_fn provides a fresh node.
                self.internal_remove_child(&child);
                child.destroy(&current_task.kernel().mounts);
            }
        }

        // If another thread added the child since the check above, it is returned without being
        // revalidated because it was just added. Otherwise, create_fn provides the node.
        let (child, status) =
            self.lock_children().get_or_create_child(current_task, mount, name, create_fn)?;
        child.node.fs().purge_old_entries();
        Ok((child, status))
    }

    // This function is only useful for tests and has some oddities.
    //
    // For example, not all the children might have been looked up yet, which
    // means the returned vector could be missing some names.
    //
    // Also, the vector might have "extra" names that are in the process of
    // being looked up. If the lookup fails, they'll be removed.
    #[cfg(test)]
    pub fn copy_child_names(&self) -> Vec<FsString> {
        let scope = RcuReadScope::new();
        self.children.read().copy_child_names(&scope)
    }

    fn internal_remove_child(&self, child: &DirEntry) {
        let mut children = self.children.write();
        let scope = RcuReadScope::new();
        let local_name = child.local_name.read(&scope);
        children.remove_if_child_matches(local_name, child);
        child.clear_canonical_dir_entry();
    }

    /// Notifies watchers on the current node and its parent about an event.
    pub fn notify(&self, event_mask: InotifyMask) {
        self.notify_watchers(event_mask, self.is_dead());
    }

    /// Notifies watchers on the current node and its parent about an event.
    ///
    /// Used for FSNOTIFY_EVENT_INODE events, which ignore IN_EXCL_UNLINK.
    pub fn notify_ignoring_excl_unlink(&self, event_mask: InotifyMask) {
        // We pretend that this directory entry is not dead to ignore IN_EXCL_UNLINK.
        self.notify_watchers(event_mask, false);
    }

    fn notify_watchers(&self, event_mask: InotifyMask, is_dead: bool) {
        let mode = self.node.info().mode;
        {
            let scope = RcuReadScope::new();
            if let Some(parent) = self.parent() {
                let local_name = self.local_name.read(&scope);
                parent.node.notify(event_mask, 0, local_name, mode, is_dead);
            }
        }
        self.node.notify(event_mask, 0, Default::default(), mode, is_dead);
    }

    /// Notifies parents about creation, and notifies current node about link_count change.
    fn notify_creation(&self) {
        let mode = self.node.info().mode;
        if Arc::strong_count(&self.node) > 1 {
            // Notify about link change only if there is already a hardlink.
            self.node.notify(InotifyMask::ATTRIB, 0, Default::default(), mode, false);
        }
        let scope = RcuReadScope::new();
        if let Some(parent) = self.parent() {
            let local_name = self.local_name.read(&scope);
            parent.node.notify(InotifyMask::CREATE, 0, local_name, mode, false);
        }
    }

    /// Notifies watchers on the current node about deletion if this is the
    /// last hardlink, and drops the DirEntryHandle kept by Inotify.
    /// Parent is also notified about deletion.
    fn notify_deletion(&self) {
        let mode = self.node.info().mode;
        if !mode.is_dir() {
            // Linux notifies link count change for non-directories.
            self.node.notify(InotifyMask::ATTRIB, 0, Default::default(), mode, false);
        }

        // This check is incorrect if there's another hard link to this FsNode that isn't in
        // memory at the moment.
        if Arc::strong_count(&self.node) == 1 {
            self.node.notify(InotifyMask::DELETE_SELF, 0, Default::default(), mode, false);
        }

        let scope = RcuReadScope::new();
        if let Some(parent) = self.parent() {
            let local_name = self.local_name.read(&scope);
            parent.node.notify(InotifyMask::DELETE, 0, local_name, mode, false);
        }
    }

    /// Returns true if this entry has mounts.
    pub fn has_mounts(&self) -> bool {
        self.flags().contains(DirEntryFlags::HAS_MOUNTS)
    }

    /// Records whether or not the entry has mounts.
    pub fn set_has_mounts(&self, v: bool) {
        if v {
            self.raise_flags(DirEntryFlags::HAS_MOUNTS);
        } else {
            self.lower_flags(DirEntryFlags::HAS_MOUNTS);
        }
    }

    /// Returns true if this entry has a submount in `parent_mount`.
    fn has_submount(self: &Arc<Self>, parent_mount: &MountInfo) -> bool {
        self.has_mounts() && parent_mount.as_ref().is_some_and(|mount| mount.has_submount(self))
    }

    /// Verifies this directory has nothing mounted on it.
    fn require_no_mounts(self: &Arc<Self>, parent_mount: &MountInfo) -> Result<(), Errno> {
        if self.has_submount(parent_mount) {
            return error!(EBUSY);
        }
        Ok(())
    }
}

/// Canonicalizes a filename into its casefolded, NFD-normalized byte representation on the stack.
/// Non-UTF-8 filenames are preserved as raw bytes.
#[inline]
fn canonicalize_name(name: impl AsRef<[u8]>) -> SmallVec<[u8; NAME_MAX as usize]> {
    let name = name.as_ref();
    match std::str::from_utf8(name) {
        Ok(valid) => {
            CasefoldStr::new(valid).casefold_normalized_chars().flat_map(utf8_bytes).collect()
        }
        Err(_) => SmallVec::from_slice(name),
    }
}

/// A canonicalized key stored in the directory entry cache.
///
/// Stores pre-casefolded, NFD-normalized UTF-8 bytes (for casefolded directories) or raw bytes
/// (for case-sensitive directories and non-UTF-8 filenames).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) struct DirEntryChildKey(FsString);

impl DirEntryChildKey {
    pub fn new(name: &FsStr, casefold: bool) -> Self {
        if casefold {
            Self(canonicalize_name(name).into_vec().into())
        } else {
            Self(name.to_owned())
        }
    }
}

impl std::borrow::Borrow<FsStr> for DirEntryChildKey {
    fn borrow(&self) -> &FsStr {
        self.0.as_bstr()
    }
}

/// The cached child directory entries of a directory.
///
/// Keys stored in this map are strictly internal to the VFS cache and are never exposed back to
/// userspace. Directory listings (`readdir`) query the underlying filesystem directly for canonical
/// on-disk names, while individual child entries track their associated lookup/creation name in
/// [`DirEntry::local_name`].
#[derive(Default, Debug)]
pub(crate) struct DirEntryChildren {
    entries: BTreeMap<DirEntryChildKey, Weak<DirEntry>>,
    casefold: bool,
}

impl std::ops::Deref for DirEntryChildren {
    type Target = BTreeMap<DirEntryChildKey, Weak<DirEntry>>;

    fn deref(&self) -> &Self::Target {
        &self.entries
    }
}

impl DirEntryChildren {
    pub fn new(casefold: bool) -> Self {
        Self { entries: BTreeMap::new(), casefold }
    }

    pub fn is_casefold(&self) -> bool {
        self.casefold
    }

    pub fn set_casefold(&mut self, casefold: bool) {
        self.entries.clear();
        self.casefold = casefold;
    }

    /// Looks up a child entry by name.
    pub fn get(&self, name: &FsStr) -> Option<&Weak<DirEntry>> {
        if self.casefold {
            let key = canonicalize_name(name);
            self.entries.get(FsStr::new(&key))
        } else {
            self.entries.get(name)
        }
    }

    /// Inserts a child into the directory cache under `name`, returning the weak reference
    /// previously stored under this name, if any.
    pub fn insert(&mut self, name: &FsStr, child: Weak<DirEntry>) -> Option<Weak<DirEntry>> {
        self.entries.insert(DirEntryChildKey::new(name, self.casefold), child)
    }

    /// Removes a child from the directory cache by name, returning the weak reference
    /// previously stored under this name, if any.
    pub fn remove(&mut self, name: &FsStr) -> Option<Weak<DirEntry>> {
        if self.casefold {
            let key = canonicalize_name(name);
            self.entries.remove(FsStr::new(&key))
        } else {
            self.entries.remove(name)
        }
    }

    /// Removes a child entry if the cached weak reference matches `expected_child`.
    /// Returns `true` if the entry was found and removed.
    pub fn remove_if_child_matches(&mut self, name: &FsStr, expected_child: &DirEntry) -> bool {
        if let Some(weak_child) = self.get(name) {
            if std::ptr::eq(weak_child.as_ptr(), expected_child) {
                let _ = self.remove(name);
                return true;
            }
        }
        false
    }

    #[cfg(test)]
    /// Extracts a snapshot of all active child names, retaining the exact casing currently
    /// stored on each child node and filtering out dropped entries.
    fn copy_child_names(&self, scope: &RcuReadScope) -> Vec<FsString> {
        self.entries
            .values()
            .filter_map(|child| Weak::upgrade(child).map(|c| c.local_name.read(scope).to_owned()))
            .collect()
    }
}

struct DirEntryLockedChildren<'a> {
    entry: &'a DirEntryHandle,
    children: LockDepWriteGuard<'a, DirEntryChildren>,
}

/// Represents whether a node was freshly created or already existed on the underlying
/// filesystem.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CreationStatus {
    /// The node was newly created. Updates directory ctime/mtime.
    Created,
    /// The node already existed (e.g., found in the cache, or resolved via lookup or pipelined
    /// lookup).
    Existed,
}

impl<'a> DirEntryLockedChildren<'a> {
    fn component_lookup(
        &mut self,
        current_task: &CurrentTask,
        mount: &MountInfo,
        name: &FsStr,
    ) -> Result<DirEntryHandle, Errno> {
        self.entry.check_search_access(current_task, mount, name)?;
        let (node, _) =
            self.get_or_create_child(current_task, mount, name, |d, _mount, name| {
                Ok((d.node.ops().lookup(d, current_task, name)?, CreationStatus::Existed))
            })?;
        Ok(node)
    }

    fn get_or_create_child(
        &mut self,
        current_task: &CurrentTask,
        mount: &MountInfo,
        name: &FsStr,
        create_fn: impl FnOnce(
            &DirEntryHandle,
            &MountInfo,
            &FsStr,
        ) -> Result<(FsNodeHandle, CreationStatus), Errno>,
    ) -> Result<(DirEntryHandle, CreationStatus), Errno> {
        if let Some(child) = self.children.get(name).and_then(Weak::upgrade) {
            debug_assert!(!child.has_nokey_name());
            // Do not cache a child in a locked or no-key directory.
            child.record_dcache_use(|| self.entry.can_cache_children(current_task));
            return Ok((child, CreationStatus::Existed));
        }
        if name.len() > NAME_MAX as usize {
            return error!(ENAMETOOLONG);
        }

        let parent_unlocked_before =
            self.entry.node.fail_if_locked(current_task, &self.entry.node.info()).is_ok();

        let (node, status) = create_fn(self.entry, mount, name)?;

        let node_mode = node.info().mode;
        assert!(
            node_mode & FileMode::IFMT != FileMode::EMPTY,
            "FsNode initialization did not populate the FileMode in FsNodeInfo."
        );

        let parent_unlocked = parent_unlocked_before
            && self.entry.node.fail_if_locked(current_task, &self.entry.node.info()).is_ok();

        let reusable_dir = if node_mode.is_dir() {
            let canonical = node.canonical_dir_entry.lock();
            if std::ptr::eq(canonical.parent.as_ptr(), Arc::as_ptr(self.entry)) {
                canonical.entry.upgrade()
            } else {
                None
            }
        } else {
            None
        };

        let child = if let Some(entry) = reusable_dir {
            let old_name = {
                let scope = RcuReadScope::new();
                let current_name = entry.local_name(&scope);
                (current_name != name).then(|| current_name.to_owned())
            };
            if let Some(old_name) = old_name {
                self.children.remove_if_child_matches(old_name.as_ref(), &entry);
                entry.local_name.update(name.to_owned());
            }
            entry
        } else {
            let child = DirEntry::new_uncached(node, Some(self.entry.clone()), name.to_owned());
            if let Err(err) = security::fs_node_init_with_dentry(current_task, &child) {
                // Null out the `parent` reference from `child` otherwise dropping `child` will
                // attempt to remove itself from `parent`, triggering a deadlock with `self`.
                child.parent.update(None);
                return Err(err);
            }
            if node_mode.is_dir() {
                *child.node.canonical_dir_entry.lock() = CanonicalDirEntry {
                    parent: Arc::downgrade(self.entry),
                    entry: Arc::downgrade(&child),
                };
            }
            child
        };

        if !parent_unlocked_before || (status == CreationStatus::Existed && !parent_unlocked) {
            child.raise_flags(DirEntryFlags::NO_KEY);
        } else {
            child.lower_flags(DirEntryFlags::NO_KEY);
        }

        // Do not cache a child in a locked or no-key directory.
        if parent_unlocked && !self.entry.has_nokey_name() {
            self.children.insert(name, Arc::downgrade(&child));
            child.node.fs().did_create_dir_entry(&child);
        } else {
            child.node.fs().will_destroy_dir_entry(&child);
        }

        Ok((child, status))
    }
}

impl fmt::Debug for DirEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let scope = RcuReadScope::new();
        let mut path = PathBuilder::new();
        let mut parent_holder;
        let mut current = self;
        loop {
            // Root and unnamed unrooted entries have an empty `local_name`.
            let name = current.local_name(&scope);
            if !name.is_empty() {
                path.prepend_element(name);
            }
            let Some(parent) = current.parent() else {
                break;
            };
            parent_holder = parent;
            current = &parent_holder;
        }
        f.debug_struct("DirEntry")
            .field("fs", &self.node.fs().name())
            .field("path", &path.build_absolute())
            .finish()
    }
}

/// Holds a file system's `rename_mutex` for the duration of a rename.
///
/// `RenameGuard` locks unrelated parents, or two entries for the same node, in address order,
/// which can contradict the parent-to-child lock order recorded elsewhere. When dropped, this
/// clears the `tracing-mutex` dependencies of the parents' locks before releasing
/// `rename_mutex`, so the address-order edge does not outlive the rename on any exit path,
/// including errors.
struct RenameLock<'a> {
    old_parent: &'a DirEntryHandle,
    new_parent: &'a DirEntryHandle,
    _guard: LockDepGuard<'a, FileSystemRenameToken>,
}

impl<'a> RenameLock<'a> {
    fn new(
        fs: &'a FileSystemHandle,
        old_parent: &'a DirEntryHandle,
        new_parent: &'a DirEntryHandle,
    ) -> Self {
        Self { old_parent, new_parent, _guard: fs.rename_mutex.lock() }
    }
}

impl Drop for RenameLock<'_> {
    fn drop(&mut self) {
        // SAFETY: Address-order acquisitions of these locks only happen in `RenameGuard` while
        // `rename_mutex` is held, and it is still held here (`_guard` is dropped after this
        // body runs), so clearing their recorded order cannot mask a real deadlock.
        unsafe {
            for parent in [self.old_parent, self.new_parent] {
                parent.children.reset_dependencies();
                parent.node.info_lock().reset_dependencies();
            }
        }
    }
}

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
enum RenameRelationship {
    SameEntry,
    SameNode,
    NewIsDescendant,
    OldIsDescendant,
    Independent,
}

struct RenameGuard<'a> {
    old_parent_guard: DirEntryLockedChildren<'a>,
    new_parent_guard: Option<DirEntryLockedChildren<'a>>,
    relationship: RenameRelationship,
}

impl<'a> RenameGuard<'a> {
    fn lock(old_parent: &'a DirEntryHandle, new_parent: &'a DirEntryHandle) -> Self {
        if Arc::ptr_eq(old_parent, new_parent) {
            let old_parent_guard = old_parent.lock_children();
            Self {
                old_parent_guard,
                new_parent_guard: None,
                relationship: RenameRelationship::SameEntry,
            }
        } else if Arc::ptr_eq(&old_parent.node, &new_parent.node) {
            let (g1, g2) =
                starnix_sync::ordered_write_lock(&old_parent.children, &new_parent.children);
            let old_parent_guard = DirEntryLockedChildren { entry: old_parent, children: g1 };
            let new_parent_guard = DirEntryLockedChildren { entry: new_parent, children: g2 };
            Self {
                old_parent_guard,
                new_parent_guard: Some(new_parent_guard),
                relationship: RenameRelationship::SameNode,
            }
        } else if new_parent.is_descendant_node_of(old_parent) {
            let old_parent_guard = old_parent.lock_children();
            let _token = allow_subclass();
            let new_parent_guard = new_parent.lock_children();
            Self {
                old_parent_guard,
                new_parent_guard: Some(new_parent_guard),
                relationship: RenameRelationship::NewIsDescendant,
            }
        } else if old_parent.is_descendant_node_of(new_parent) {
            let new_parent_guard = new_parent.lock_children();
            let _token = allow_subclass();
            let old_parent_guard = old_parent.lock_children();
            Self {
                old_parent_guard,
                new_parent_guard: Some(new_parent_guard),
                relationship: RenameRelationship::OldIsDescendant,
            }
        } else {
            // Independent directories can be locked in address order.
            let (g1, g2) =
                starnix_sync::ordered_write_lock(&old_parent.children, &new_parent.children);
            let old_parent_guard = DirEntryLockedChildren { entry: old_parent, children: g1 };
            let new_parent_guard = DirEntryLockedChildren { entry: new_parent, children: g2 };
            Self {
                old_parent_guard,
                new_parent_guard: Some(new_parent_guard),
                relationship: RenameRelationship::Independent,
            }
        }
    }

    /// Consumes the `RenameGuard` (which only holds children locks) and locks the `info`
    /// of the parent directories in a safe order to prevent deadlocks. Returns a
    /// `RenameContext` which encapsulates all acquired locks (both children and info).
    fn lock_info(
        self,
        old_parent: &'a DirEntryHandle,
        new_parent: &'a DirEntryHandle,
        renamed: &'a DirEntryHandle,
        replaced: Option<&'a DirEntryHandle>,
    ) -> RenameContext<'a> {
        let (g1, g2) = match self.relationship {
            RenameRelationship::SameEntry | RenameRelationship::SameNode => {
                (old_parent.node.info_lock().write(), None)
            }
            RenameRelationship::NewIsDescendant => {
                let g1 = old_parent.node.info_lock().write();
                let _token = allow_subclass();
                let g2 = new_parent.node.info_lock().write();
                (g1, Some(g2))
            }
            RenameRelationship::OldIsDescendant => {
                let g2 = new_parent.node.info_lock().write();
                let _token = allow_subclass();
                let g1 = old_parent.node.info_lock().write();
                (g1, Some(g2))
            }
            RenameRelationship::Independent => {
                let (g1, g2) = starnix_sync::ordered_write_lock(
                    old_parent.node.info_lock(),
                    new_parent.node.info_lock(),
                );
                (g1, Some(g2))
            }
        };

        RenameContext {
            renamed,
            replaced,
            old_parent_guard: self.old_parent_guard,
            new_parent_guard: self.new_parent_guard,
            old_parent_info_guard: g1,
            new_parent_info_guard: g2,
        }
    }

    fn old_parent_children(&mut self) -> &mut DirEntryLockedChildren<'a> {
        &mut self.old_parent_guard
    }

    fn new_parent_children(&mut self) -> &mut DirEntryLockedChildren<'a> {
        if let Some(new_guard) = self.new_parent_guard.as_mut() {
            new_guard
        } else {
            &mut self.old_parent_guard
        }
    }
}

/// A context that holds the locked children and info of the parents during a
/// rename operation.
///
/// The context is constructed by locking the parent directories
/// (`old_parent`, `new_parent`) for write. These parent locks are held for
/// the duration of the context's lifetime, preventing concurrent
/// modifications to the parents.
///
/// Child nodes (`renamed` and `replaced`) are *not* locked upon
/// construction. To query properties of the children (like whether they are
/// directories) without risking deadlocks, use the provided helper methods
/// (`renamed_is_dir`, `replaced_is_dir`) instead of locking them directly.
pub struct RenameContext<'a> {
    pub renamed: &'a DirEntryHandle,
    pub replaced: Option<&'a DirEntryHandle>,
    old_parent_guard: DirEntryLockedChildren<'a>,
    new_parent_guard: Option<DirEntryLockedChildren<'a>>,
    old_parent_info_guard: LockDepWriteGuard<'a, crate::vfs::FsNodeInfo>,
    new_parent_info_guard: Option<LockDepWriteGuard<'a, crate::vfs::FsNodeInfo>>,
}

impl<'a> RenameContext<'a> {
    /// Returns whether the renamed child node is a directory.
    ///
    /// This method safely handles child info locking under the parent locks.
    /// If the renamed node is same as a parent (which is already locked),
    /// it uses the parent's guard to avoid self-deadlock. Otherwise, it
    /// locks the child info using `allow_subclass`.
    pub fn renamed_is_dir(&self) -> bool {
        self.is_dir(&self.renamed.node)
    }

    /// Returns whether the replaced child node is a directory.
    ///
    /// Returns `false` if `replaced` is `None`.
    ///
    /// This method safely handles child info locking under the parent locks.
    /// If the replaced node is same as a parent (which is already locked),
    /// it uses the parent's guard to avoid self-deadlock. Otherwise, it
    /// locks the child info using `allow_subclass`.
    pub fn replaced_is_dir(&self) -> bool {
        self.replaced.map(|r| self.is_dir(&r.node)).unwrap_or(false)
    }

    /// Returns the old parent directory entry handle.
    ///
    /// The old parent's child list is write-locked for the lifetime of the
    /// context.
    pub fn old_parent(&self) -> &DirEntryHandle {
        self.old_parent_guard.entry
    }

    /// Returns the new parent directory entry handle.
    ///
    /// The new parent's child list is write-locked for the lifetime of the
    /// context.
    pub fn new_parent(&self) -> &DirEntryHandle {
        self.new_parent_guard.as_ref().map(|g| g.entry).unwrap_or(self.old_parent_guard.entry)
    }

    /// Returns mutable references to the `FsNodeInfo` of both parent
    /// directories.
    ///
    /// The references are returned as a tuple to allow them to be borrowed
    /// mutably at the same time (e.g., to update link counts in both).
    ///
    /// If `new_parent` shares the same `FsNode` as `old_parent`, the second
    /// element of the tuple will be `None` to prevent mutable aliasing of the
    /// same guard.
    pub fn parent_infos_mut(
        &mut self,
    ) -> (&mut crate::vfs::FsNodeInfo, Option<&mut crate::vfs::FsNodeInfo>) {
        let old = &mut *self.old_parent_info_guard;
        let new = self.new_parent_info_guard.as_mut().map(|g| &mut **g);
        (old, new)
    }

    /// Returns a shared reference to the `FsNodeInfo` of the old parent
    /// directory.
    pub fn old_parent_info(&self) -> &crate::vfs::FsNodeInfo {
        &self.old_parent_info_guard
    }
    /// Returns a shared reference to the `FsNodeInfo` of the new parent
    /// directory.
    ///
    /// Returns `None` if `new_parent` shares the same `FsNode` as `old_parent`.
    pub fn new_parent_info(&self) -> Option<&crate::vfs::FsNodeInfo> {
        self.new_parent_info_guard.as_deref()
    }

    fn new_parent_children(&mut self) -> &mut DirEntryLockedChildren<'a> {
        self.new_parent_guard.as_mut().unwrap_or(&mut self.old_parent_guard)
    }

    fn old_parent_children(&mut self) -> &mut DirEntryLockedChildren<'a> {
        &mut self.old_parent_guard
    }

    fn is_dir(&self, node: &FsNodeHandle) -> bool {
        if Arc::ptr_eq(node, &self.old_parent().node) {
            self.old_parent_info_guard.mode.is_dir()
        } else if Arc::ptr_eq(node, &self.new_parent().node) {
            self.new_parent_info_guard
                .as_ref()
                .map(|g| g.mode.is_dir())
                .unwrap_or_else(|| self.old_parent_info_guard.mode.is_dir())
        } else {
            let _token = allow_subclass();
            node.is_dir()
        }
    }
}

/// The Drop trait for DirEntry removes the entry from the child list of the
/// parent entry, which means we cannot drop DirEntry objects while holding a
/// lock on the parent's child list.
impl Drop for DirEntry {
    fn drop(&mut self) {
        let maybe_parent = self.parent();
        self.parent.update(None);
        if let Some(parent) = maybe_parent {
            parent.internal_remove_child(self);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{PanickingFile, spawn_kernel_and_run};
    use crate::vfs::pseudo::simple_directory::SimpleDirectory;
    use crate::vfs::{
        CacheConfig, CacheMode, FileOps, FileSystem, FileSystemHandle, FileSystemOps,
        FileSystemOptions, FsNode, FsNodeOps, FscryptPolicyFlags, LookupContext,
        MemoryDirectoryFile, Namespace, SymlinkNode, WhatToMount, fs_node_impl_not_dir,
    };
    use starnix_crypt::CryptService;
    use starnix_sync::Mutex;
    use starnix_types::vfs::default_statfs;
    use starnix_uapi::auth::FsCred;
    use starnix_uapi::device_id::DeviceId;
    use starnix_uapi::file_mode::mode;
    use starnix_uapi::mount_flags::MountpointFlags;
    use starnix_uapi::vfs::ResolveFlags;
    use starnix_uapi::{errno, statfs};
    use std::sync::atomic::AtomicUsize;

    #[fuchsia::test]
    async fn test_dir_entry_debug() {
        spawn_kernel_and_run(async |current_task| {
            let fs = current_task.fs().root().entry.node.fs();
            let check = |parent: Option<&DirEntryHandle>, name: &str, expected_path: &str| {
                let node = SimpleDirectory::new().into_node(&fs, 0o755);
                let entry = DirEntry::new(node, parent.cloned(), name.into());
                assert_eq!(
                    format!("{entry:?}"),
                    format!("DirEntry {{ fs: {:?}, path: {expected_path:?} }}", fs.name())
                );
                entry
            };

            let root = check(None, "", "/");
            let bus = check(Some(&root), "bus", "/bus");
            check(Some(&bus), "pci", "/bus/pci");
            check(None, "[eventfd]", "/[eventfd]");
        })
        .await;
    }

    #[test]
    fn test_canonicalize_name() {
        let upper = canonicalize_name("FooBar.TXT");
        let lower = canonicalize_name("foobar.txt");
        let mixed = canonicalize_name("FOOBAR.TXT");
        let diff = canonicalize_name("other.txt");

        assert_eq!(upper.as_slice(), lower.as_slice());
        assert_eq!(upper.as_slice(), mixed.as_slice());
        assert_ne!(upper.as_slice(), diff.as_slice());

        // Unicode decomposed vs composed
        let composed = canonicalize_name("\u{03AA}");
        let decomposed = canonicalize_name("\u{0399}\u{0308}");
        assert_eq!(composed.as_slice(), decomposed.as_slice());

        // Unicode accent folding (e\u{0301} vs \u{00c9})
        let e_accent_lower = canonicalize_name("e\u{0301}");
        let e_accent_upper = canonicalize_name("\u{00c9}");
        assert_eq!(e_accent_lower.as_slice(), e_accent_upper.as_slice());

        // Unicode German sharp S (straße vs STRASSE)
        let strasse_lower = canonicalize_name("straße");
        let strasse_upper = canonicalize_name("STRASSE");
        assert_eq!(strasse_lower.as_slice(), strasse_upper.as_slice());

        // Non-UTF-8 preserved as-is
        let non_utf8 = canonicalize_name(b"foo\x80bar");
        assert_eq!(non_utf8.as_slice(), b"foo\x80bar");
    }

    #[test]
    fn test_dir_entry_children_casefold() {
        let mut children = DirEntryChildren::new(true);
        assert!(children.is_casefold());

        // 1. Insert and lookup casefolded UTF-8
        children.insert("FooBar.TXT".into(), Weak::new());
        assert!(children.get("foobar.txt".into()).is_some());
        assert!(children.get("FOOBAR.TXT".into()).is_some());
        assert!(children.get("FooBar.TXT".into()).is_some());
        assert!(children.get("other.txt".into()).is_none());

        // 2. Insert valid Unicode with decomposed/composed characters
        children.insert("\u{03AA}".into(), Weak::new());
        assert!(children.get("\u{0399}\u{0308}".into()).is_some());

        // 3. Insert non-UTF-8
        children.insert(b"foo\x80bar".into(), Weak::new());
        assert!(children.get(b"foo\x80bar".into()).is_some());
        assert!(children.get(b"FOO\x80bar".into()).is_none());

        // 4. Removal
        assert!(children.remove("FOOBAR.TXT".into()).is_some());
        assert!(children.get("foobar.txt".into()).is_none());
        assert!(children.remove("\u{0399}\u{0308}".into()).is_some());
        assert!(children.get("\u{03AA}".into()).is_none());
        assert!(children.remove(b"foo\x80bar".into()).is_some());
        assert!(children.get(b"foo\x80bar".into()).is_none());
        assert!(children.is_empty());

        // 5. Dynamic casefold toggling clears the cache
        children.insert("Foo.TXT".into(), Weak::new());
        assert!(!children.is_empty());
        children.set_casefold(false);
        assert!(!children.is_casefold());
        assert!(children.is_empty());
        children.set_casefold(true);
        assert!(children.is_casefold());
    }

    #[derive(Default)]
    struct CountingTestFs {
        /// The nodes that lookups find. Tests add nodes here to simulate files that exist in the
        /// filesystem but are not in the dcache.
        existing_nodes: Mutex<BTreeMap<FsString, FsNodeHandle>>,

        /// Optional node whose encryption policy is cleared mid-lookup to simulate a concurrent
        /// `FS_IOC_ADD_ENCRYPTION_KEY` racing with VFS lookup/rename/unlink.
        unlock_node_on_lookup: Mutex<Option<FsNodeHandle>>,

        /// The number of times that the entry of a `CountingFileNode` was revalidated.
        revalidate_count: AtomicUsize,

        /// The number of times `FileSystemOps::rename` was invoked.
        rename_count: AtomicUsize,

        /// Optional crypt service for simulating locked/unlocked fscrypt directories.
        crypt_service: Option<Arc<CryptService>>,
    }

    impl FileSystemOps for CountingTestFs {
        fn statfs(&self, _fs: &FileSystem, _current_task: &CurrentTask) -> Result<statfs, Errno> {
            Ok(default_statfs(0))
        }

        fn name(&self) -> &'static FsStr {
            "counting_test_fs".into()
        }

        fn crypt_service(&self) -> Option<Arc<CryptService>> {
            self.crypt_service.clone()
        }

        fn has_casefold_support(&self) -> bool {
            true
        }

        fn rename(
            &self,
            _fs: &FileSystem,
            _current_task: &CurrentTask,
            context: &mut RenameContext<'_>,
            old_name: &FsStr,
            new_name: &FsStr,
        ) -> Result<(), Errno> {
            self.rename_count.fetch_add(1, Ordering::SeqCst);
            if let Some(replaced) = context.replaced {
                if context.replaced_is_dir() && !dir_entries(&replaced.node).lock().is_empty() {
                    return error!(ENOTEMPTY);
                }
            }
            let old_parent = context.old_parent().node.clone();
            let new_parent = context.new_parent().node.clone();
            let renamed = context.renamed.node.clone();
            let (old_info, _) = context.parent_infos_mut();
            let casefold = old_info.casefold;
            {
                let mut old_entries = dir_entries(&old_parent).lock();
                if old_entries.remove(old_name).is_none() && casefold {
                    if let Some(key) =
                        old_entries.keys().find(|k| k.eq_ignore_ascii_case(old_name)).cloned()
                    {
                        old_entries.remove(&key);
                    }
                }
            }
            dir_entries(&new_parent).lock().insert(new_name.to_owned(), renamed);
            Ok(())
        }

        fn exchange(
            &self,
            _fs: &FileSystem,
            _current_task: &CurrentTask,
            context: &mut RenameContext<'_>,
            name1: &FsStr,
            name2: &FsStr,
        ) -> Result<(), Errno> {
            let replaced = context.replaced.expect("exchange requires replaced entry");
            dir_entries(&context.old_parent().node)
                .lock()
                .insert(name1.to_owned(), replaced.node.clone());
            dir_entries(&context.new_parent().node)
                .lock()
                .insert(name2.to_owned(), context.renamed.node.clone());
            Ok(())
        }
    }

    /// Returns the `CountingTestFs` of `fs`.
    fn counting_fs(fs: &FileSystem) -> &CountingTestFs {
        fs.downcast_ops::<CountingTestFs>().expect("not a CountingTestFs")
    }

    fn dir_entries(node: &FsNode) -> &Mutex<BTreeMap<FsString, FsNodeHandle>> {
        &node.downcast_ops::<CountingDirNode>().expect("not a CountingDirNode").entries
    }

    struct CountingDirNode {
        lookup_count: Arc<AtomicUsize>,
        entries: Mutex<BTreeMap<FsString, FsNodeHandle>>,
    }

    impl CountingDirNode {
        fn lookup_in_dir(&self, dir_node: &FsNode, name: &FsStr) -> Result<FsNodeHandle, Errno> {
            self.lookup_count.fetch_add(1, Ordering::SeqCst);
            let fs = dir_node.fs();
            let casefold = dir_node.info().casefold;
            let in_dir = {
                let entries = self.entries.lock();
                entries.get(name).cloned().or_else(|| {
                    casefold.then(|| {
                        entries
                            .iter()
                            .find(|(k, _)| k.eq_ignore_ascii_case(name))
                            .map(|(_, v)| v.clone())
                    })?
                })
            };
            let node = in_dir.or_else(|| counting_fs(&fs).existing_nodes.lock().get(name).cloned());
            if let Some(node_to_unlock) = counting_fs(&fs).unlock_node_on_lookup.lock().take() {
                node_to_unlock.update_info(|info| {
                    info.encryption_policy = None;
                });
            }
            node.ok_or_else(|| errno!(ENOENT))
        }
    }

    impl FsNodeOps for CountingDirNode {
        fn create_file_ops(
            &self,
            _node: &FsNode,
            _current_task: &CurrentTask,
            _flags: OpenFlags,
        ) -> Result<Box<dyn FileOps>, Errno> {
            Ok(Box::new(MemoryDirectoryFile::new()))
        }

        fn lookup(
            &self,
            entry: &DirEntry,
            _current_task: &CurrentTask,
            name: &FsStr,
        ) -> Result<FsNodeHandle, Errno> {
            self.lookup_in_dir(&entry.node, name)
        }

        fn lookup_pipelined(
            &self,
            entry: &DirEntry,
            _current_task: &CurrentTask,
            names: &[&FsStr],
        ) -> LookupVec<Result<FsNodeHandle, Errno>> {
            let mut out = LookupVec::new();
            let mut current_dir = entry.node.clone();
            for name in names {
                let Some(dir_ops) = current_dir.downcast_ops::<CountingDirNode>() else {
                    out.push(error!(ENOTDIR));
                    break;
                };
                match dir_ops.lookup_in_dir(&current_dir, name) {
                    Ok(node) => {
                        current_dir = node;
                        out.push(Ok(current_dir.clone()));
                    }
                    Err(e) => {
                        out.push(Err(e));
                        break;
                    }
                }
            }
            out
        }

        fn mknod(
            &self,
            node: &FsNode,
            _current_task: &CurrentTask,
            name: &FsStr,
            mode: FileMode,
            dev: DeviceId,
            owner: FsCred,
        ) -> Result<FsNodeHandle, Errno> {
            let mut info = FsNodeInfo::new(mode, owner);
            info.rdev = dev;
            let created = node.fs().create_node_and_allocate_node_id(CountingFileNode, info);
            self.entries.lock().insert(name.to_owned(), created.clone());
            Ok(created)
        }

        fn mkdir(
            &self,
            node: &FsNode,
            _current_task: &CurrentTask,
            name: &FsStr,
            mode: FileMode,
            owner: FsCred,
        ) -> Result<FsNodeHandle, Errno> {
            let created = node.fs().create_node_and_allocate_node_id(
                CountingDirNode {
                    lookup_count: self.lookup_count.clone(),
                    entries: Default::default(),
                },
                FsNodeInfo::new(mode, owner),
            );
            self.entries.lock().insert(name.to_owned(), created.clone());
            Ok(created)
        }

        fn link(
            &self,
            _node: &FsNode,
            _current_task: &CurrentTask,
            name: &FsStr,
            child: &FsNodeHandle,
        ) -> Result<(), Errno> {
            self.entries.lock().insert(name.to_owned(), child.clone());
            Ok(())
        }

        fn unlink(
            &self,
            _node: &FsNode,
            _current_task: &CurrentTask,
            name: &FsStr,
            _child: &FsNodeHandle,
        ) -> Result<(), Errno> {
            self.entries.lock().remove(name);
            Ok(())
        }

        fn create_symlink(
            &self,
            _node: &FsNode,
            _current_task: &CurrentTask,
            _name: &FsStr,
            _target: &FsStr,
            _owner: FsCred,
        ) -> Result<FsNodeHandle, Errno> {
            error!(ENOTSUP)
        }
    }

    struct CountingFileNode;
    impl FsNodeOps for CountingFileNode {
        fs_node_impl_not_dir!();

        fn create_file_ops(
            &self,
            _node: &FsNode,
            _current_task: &CurrentTask,
            _flags: OpenFlags,
        ) -> Result<Box<dyn FileOps>, Errno> {
            Ok(Box::new(PanickingFile))
        }

        fn create_dir_entry_ops(&self) -> Box<dyn DirEntryOps> {
            Box::new(CountingDirEntryOps)
        }
    }

    struct CountingDirEntryOps;
    impl DirEntryOps for CountingDirEntryOps {
        fn revalidate(&self, _current_task: &CurrentTask, entry: &DirEntry) -> Result<bool, Errno> {
            counting_fs(&entry.node.fs()).revalidate_count.fetch_add(1, Ordering::SeqCst);
            Ok(true)
        }
    }

    fn create_counting_fs(current_task: &CurrentTask) -> (FileSystemHandle, Arc<AtomicUsize>) {
        create_counting_fs_with_crypt(current_task, None)
    }

    fn create_counting_fs_with_crypt(
        current_task: &CurrentTask,
        crypt_service: Option<Arc<CryptService>>,
    ) -> (FileSystemHandle, Arc<AtomicUsize>) {
        let lookup_count = Arc::new(AtomicUsize::new(0));
        let fs = FileSystem::new(
            current_task.kernel(),
            CacheMode::Cached(CacheConfig { capacity: 16 }),
            CountingTestFs { crypt_service, ..Default::default() },
            FileSystemOptions::default(),
        )
        .expect("create filesystem");
        let root_ino = fs.allocate_ino();
        fs.create_root(
            root_ino,
            CountingDirNode { lookup_count: lookup_count.clone(), entries: Default::default() },
        );
        (fs, lookup_count)
    }

    #[fuchsia::test]
    async fn test_open_create_looks_up_once() {
        spawn_kernel_and_run(async |current_task| {
            let (fs, lookup_count) = create_counting_fs(&current_task);
            let dir = Namespace::new(fs.clone()).root();
            let open = |name: &str, flags: OpenFlags| {
                current_task.open_namespace_node_at(
                    dir.clone(),
                    name.into(),
                    flags,
                    mode!(IFREG, 0o644),
                    ResolveFlags::empty(),
                )
            };

            // Creating a new file looks up the name only once before creating it.
            let created = open("file", OpenFlags::CREAT | OpenFlags::RDWR).expect("create file");
            assert_eq!(lookup_count.load(Ordering::SeqCst), 1);

            // Opening the existing file with O_CREAT is served from the cache, and revalidates the
            // cached entry only once.
            let opened = open("file", OpenFlags::CREAT | OpenFlags::RDWR).expect("open file");
            assert!(Arc::ptr_eq(&created.name.entry, &opened.name.entry));
            assert_eq!(lookup_count.load(Ordering::SeqCst), 1);
            assert_eq!(counting_fs(&fs).revalidate_count.load(Ordering::SeqCst), 1);

            // O_EXCL still fails for an existing file.
            assert_eq!(
                open("file", OpenFlags::CREAT | OpenFlags::EXCL | OpenFlags::RDWR).unwrap_err(),
                errno!(EEXIST)
            );
            assert_eq!(lookup_count.load(Ordering::SeqCst), 1);

            // Creating a new file with O_EXCL also looks up the name only once.
            open("excl", OpenFlags::CREAT | OpenFlags::EXCL | OpenFlags::RDWR)
                .expect("create excl");
            assert_eq!(lookup_count.load(Ordering::SeqCst), 2);
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_open_create_existing() {
        spawn_kernel_and_run(async |current_task| {
            let (fs, lookup_count) = create_counting_fs(&current_task);
            let ns = Namespace::new(fs.clone());
            let dir = ns.root();
            let open = |name: &str, resolve_flags: ResolveFlags| {
                current_task.open_namespace_node_at(
                    dir.clone(),
                    name.into(),
                    OpenFlags::CREAT | OpenFlags::RDWR,
                    mode!(IFREG, 0o644),
                    resolve_flags,
                )
            };
            let add_existing_node = |name: &str, node: FsNodeHandle| {
                counting_fs(&fs).existing_nodes.lock().insert(name.into(), node);
            };

            // Opening a file that exists but is not in the dcache with O_CREAT looks it up only
            // once, and does not revalidate the entry that was just looked up.
            let file = fs.create_node_and_allocate_node_id(
                CountingFileNode,
                FsNodeInfo::new(mode!(IFREG, 0o644), FsCred::root()),
            );
            add_existing_node("file", file.clone());
            let opened = open("file", ResolveFlags::empty()).expect("open file");
            assert!(Arc::ptr_eq(&opened.name.entry.node, &file));
            assert_eq!(lookup_count.load(Ordering::SeqCst), 1);
            assert_eq!(counting_fs(&fs).revalidate_count.load(Ordering::SeqCst), 0);

            // Opening a symlink with O_CREAT opens its target.
            let (link, info) = SymlinkNode::new("file".into(), FsCred::root());
            add_existing_node("link", fs.create_node_and_allocate_node_id(link, info));
            let via_link = open("link", ResolveFlags::empty()).expect("open link");
            assert!(Arc::ptr_eq(&via_link.name.entry, &opened.name.entry));
            assert_eq!(lookup_count.load(Ordering::SeqCst), 2);

            // Opening a mount point with O_CREAT opens the root of the mount, unless the path must
            // not cross mount points.
            let mounted = open("mounted", ResolveFlags::empty()).expect("create mounted");
            opened
                .name
                .mount(WhatToMount::Bind(mounted.name.to_passive()), MountpointFlags::empty())
                .expect("bind mount");
            let over_mount = open("file", ResolveFlags::empty()).expect("open mount point");
            assert!(Arc::ptr_eq(&over_mount.name.entry, &mounted.name.entry));
            assert_eq!(open("file", ResolveFlags::NO_XDEV).unwrap_err(), errno!(EXDEV));

            // With O_EXCL, opening a mount point fails with EEXIST, but only after the mount has
            // been entered, so a path that must not cross mount points fails with EXDEV instead,
            // as in Linux.
            let open_excl = |name: &str, resolve_flags: ResolveFlags| {
                current_task.open_namespace_node_at(
                    dir.clone(),
                    name.into(),
                    OpenFlags::CREAT | OpenFlags::EXCL | OpenFlags::RDWR,
                    mode!(IFREG, 0o644),
                    resolve_flags,
                )
            };
            assert_eq!(open_excl("file", ResolveFlags::empty()).unwrap_err(), errno!(EEXIST));
            assert_eq!(open_excl("file", ResolveFlags::NO_XDEV).unwrap_err(), errno!(EXDEV));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_dcache_keeps_recently_used_entries() {
        spawn_kernel_and_run(async |current_task| {
            let (fs, lookup_count) = create_counting_fs(&current_task);
            let dir = Namespace::new(fs.clone()).root();
            // The entries are only kept alive by the cache, so a weak reference to an entry tells
            // whether the entry is still cached without recording a use.
            let create = |name: &str| -> Weak<DirEntry> {
                let node = dir
                    .create_node(&current_task, name.into(), mode!(IFREG, 0o644), DeviceId::NONE)
                    .expect("create_node failed");
                Arc::downgrade(&node.entry)
            };
            let is_cached = |entry: &Weak<DirEntry>| entry.upgrade().is_some();

            // Fill the cache, which holds 16 entries.
            let mut entries: Vec<_> = (0..16).map(|i| create(&format!("file{i}"))).collect();

            // Use the oldest entry, then add another entry, which evicts one entry.
            let lookups = lookup_count.load(Ordering::SeqCst);
            dir.lookup_child(&current_task, &mut LookupContext::default(), "file0".into())
                .expect("lookup_child failed");
            assert_eq!(lookup_count.load(Ordering::SeqCst), lookups);
            entries.push(create("file16"));

            // The entry that was used got a second chance, and the next oldest one was evicted.
            assert!(is_cached(&entries[0]));
            assert!(!is_cached(&entries[1]));

            // The second chance cleared the use: without another use, the entry is evicted once
            // the 15 entries that are now ahead of it have been evicted.
            for i in 17..32 {
                entries.push(create(&format!("file{i}")));
            }
            assert!(is_cached(&entries[0]));
            assert!((2..17).all(|i| !is_cached(&entries[i])));
            entries.push(create("file32"));
            assert!(!is_cached(&entries[0]));
            assert!((17..33).all(|i| is_cached(&entries[i])));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_rename_across_distinct_entries_sharing_fs_node() {
        spawn_kernel_and_run(async |current_task| {
            let (fs, _lookup_count) = create_counting_fs(&current_task);
            let root = Namespace::new(fs.clone()).root();

            let create_file = |dir: &NamespaceNode, name: &str| {
                dir.create_node(&current_task, name.into(), mode!(IFREG, 0o644), DeviceId::NONE)
                    .expect("create file")
            };
            let create_dir = |dir: &NamespaceNode, name: &str| {
                dir.create_node(&current_task, name.into(), mode!(IFDIR, 0o755), DeviceId::NONE)
                    .expect("create dir")
            };
            let lookup = |dir: &NamespaceNode, name: &str| {
                dir.lookup_child(&current_task, &mut LookupContext::default(), name.into())
            };
            let alias = |dir: &NamespaceNode, alias_name: &str| NamespaceNode {
                mount: root.mount.clone(),
                entry: DirEntry::new_uncached(
                    dir.entry.node.clone(),
                    Some(root.entry.clone()),
                    alias_name.into(),
                ),
            };
            let rename =
                |old_dir: &NamespaceNode, old: &str, new_dir: &NamespaceNode, new: &str| {
                    NamespaceNode::rename(
                        &current_task,
                        old_dir,
                        old.into(),
                        new_dir,
                        new.into(),
                        RenameFlags::empty(),
                    )
                };

            let dir1 = create_dir(&root, "dir");
            let dir2 = alias(&dir1, "dir_alias");
            let src = create_file(&dir1, "src");
            let _dst = create_file(&dir1, "dst");
            let _child_dir = create_dir(&dir1, "child_dir");

            // Rename across `dir2` -> `dir1` succeeds without deadlocking and evicts `"src"` from
            // `dir1`'s child cache.
            rename(&dir2, "src", &dir1, "dst").expect("rename across alias handles");
            assert_eq!(lookup(&dir1, "src").unwrap_err(), errno!(ENOENT));
            assert!(Arc::ptr_eq(&lookup(&dir1, "dst").unwrap().entry.node, &src.entry.node));

            // `RENAME_EXCHANGE` across `dir1` and `dir2` keeps both handles' caches consistent.
            let other = create_file(&dir1, "other");
            NamespaceNode::rename(
                &current_task,
                &dir1,
                "dst".into(),
                &dir2,
                "other".into(),
                RenameFlags::EXCHANGE,
            )
            .expect("exchange across alias handles");
            assert!(Arc::ptr_eq(&lookup(&dir1, "dst").unwrap().entry.node, &other.entry.node));
            assert!(Arc::ptr_eq(&lookup(&dir1, "other").unwrap().entry.node, &src.entry.node));

            // Cycle and ancestor checks compare `FsNode` identity across distinct handles.
            assert_eq!(rename(&root, "dir", &dir2, "into_self").unwrap_err(), errno!(EINVAL));
            assert_eq!(rename(&dir2, "child_dir", &root, "dir").unwrap_err(), errno!(ENOTEMPTY));

            // In a casefolded directory, renaming across hard links is a no-op, whereas a
            // case-only rename proceeds to `fs.rename`.
            let cf1 = create_dir(&root, "cf_dir");
            cf1.entry.set_casefold(&current_task, true).expect("enable casefold");
            let cf2 = alias(&cf1, "cf_dir_alias");
            let shared = create_file(&cf1, "link1");
            cf1.link(&current_task, "link2".into(), &shared.entry.node).expect("hard link");
            let _foo = create_file(&cf1, "Foo");

            let renames_before = counting_fs(&fs).rename_count.load(Ordering::SeqCst);
            rename(&cf2, "link1", &cf1, "link2").expect("hard-link rename in casefold dir");
            assert_eq!(counting_fs(&fs).rename_count.load(Ordering::SeqCst), renames_before);

            rename(&cf2, "Foo", &cf1, "foo").expect("case-only rename across alias handles");
            assert_eq!(counting_fs(&fs).rename_count.load(Ordering::SeqCst), renames_before + 1);
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_splice_no_key_dir_entry_on_unlock() {
        spawn_kernel_and_run(async |current_task| {
            let crypt = Arc::new(CryptService::new(&[1; 32], &[2; 32], None));
            let key = [3u8; 64];
            let key_id = crypt.add_wrapping_key(&key, 0).expect("add wrapping key");
            let (fs, lookup_count) =
                create_counting_fs_with_crypt(&current_task, Some(crypt.clone()));
            let ns = Namespace::new(fs.clone());
            let root = ns.root();

            let locked_dir = root
                .create_node(&current_task, "locked".into(), mode!(IFDIR, 0o755), DeviceId::NONE)
                .expect("create locked dir");
            let bind_mountpoint = root
                .create_node(&current_task, "mnt".into(), mode!(IFDIR, 0o755), DeviceId::NONE)
                .expect("create bind mountpoint");
            locked_dir.entry.node.update_info(|info| {
                info.encryption_policy = Some(FscryptNodePolicy {
                    key_identifier: key_id,
                    flags: FscryptPolicyFlags::empty(),
                });
            });

            let app_node = fs.create_node_and_allocate_node_id(
                CountingDirNode { lookup_count: lookup_count.clone(), entries: Default::default() },
                FsNodeInfo::new(mode!(IFDIR, 0o755), FsCred::root()),
            );
            {
                let mut existing = counting_fs(&fs).existing_nodes.lock();
                existing.insert("_nokey_app".into(), app_node.clone());
                existing.insert("com.example.app".into(), app_node);
            }

            // Lock the directory and look up the child via its no-key name.
            crypt.forget_wrapping_key(key_id, 0).expect("forget wrapping key");
            let no_key_1 = locked_dir
                .lookup_child(&current_task, &mut LookupContext::default(), "_nokey_app".into())
                .expect("no-key lookup 1");
            assert!(no_key_1.entry.flags().contains(DirEntryFlags::NO_KEY));
            assert!(locked_dir.entry.copy_child_names().is_empty());

            // A second locked lookup while the NO_KEY entry is live reuses the canonical entry.
            let no_key_2 = locked_dir
                .lookup_child(&current_task, &mut LookupContext::default(), "_nokey_app".into())
                .expect("no-key lookup 2");
            assert!(Arc::ptr_eq(&no_key_1.entry, &no_key_2.entry));
            drop(no_key_2);

            // Bind-mount the NO_KEY entry onto /mnt.
            bind_mountpoint
                .mount(WhatToMount::Bind(no_key_1.clone()), MountpointFlags::empty())
                .expect("bind mount no_key_1");

            // Unlock and look up by the plaintext name: the live NO_KEY entry is spliced in place.
            crypt.add_wrapping_key(&key, 0).expect("re-add wrapping key");
            let plaintext = locked_dir
                .lookup_child(
                    &current_task,
                    &mut LookupContext::default(),
                    "com.example.app".into(),
                )
                .expect("plaintext lookup");
            assert!(Arc::ptr_eq(&plaintext.entry, &no_key_1.entry));
            assert!(!plaintext.entry.flags().contains(DirEntryFlags::NO_KEY));
            {
                let scope = RcuReadScope::new();
                assert_eq!(plaintext.entry.local_name(&scope), "com.example.app");
            }
            assert_eq!(
                locked_dir.entry.copy_child_names(),
                vec![FsString::from("com.example.app")]
            );

            // Verify that the bind mount at /mnt resolves to the spliced DirEntry and that `..`
            // from the bind mount root traverses to / (the mountpoint's parent), not /locked.
            let mnt_resolved = root
                .lookup_child(&current_task, &mut LookupContext::default(), "mnt".into())
                .expect("lookup mnt");
            assert!(Arc::ptr_eq(&mnt_resolved.entry, &plaintext.entry));
            let mnt_parent = mnt_resolved
                .lookup_child(&current_task, &mut LookupContext::default(), "..".into())
                .expect("lookup .. from mnt");
            assert!(Arc::ptr_eq(&mnt_parent.entry, &root.entry));

            // Subsequent plaintext lookup hits the dcache without calling lookup.
            let lookups_before = lookup_count.load(Ordering::SeqCst);
            let cached = locked_dir
                .lookup_child(
                    &current_task,
                    &mut LookupContext::default(),
                    "com.example.app".into(),
                )
                .expect("cached lookup");
            assert!(Arc::ptr_eq(&cached.entry, &plaintext.entry));
            assert_eq!(lookup_count.load(Ordering::SeqCst), lookups_before);

            // Re-locking the directory and looking up via the no-key name splices the canonical
            // DirEntry back to NO_KEY and evicts the plaintext name from `children`.
            crypt.forget_wrapping_key(key_id, 0).expect("forget wrapping key again");
            let relocked = locked_dir
                .lookup_child(&current_task, &mut LookupContext::default(), "_nokey_app".into())
                .expect("relocked no-key lookup");
            assert!(Arc::ptr_eq(&relocked.entry, &plaintext.entry));
            assert!(relocked.entry.flags().contains(DirEntryFlags::NO_KEY));
            assert!(locked_dir.entry.copy_child_names().is_empty());

            // Dead NO_KEY canonical entries must not be spliced on unlock, and canonical_dir_entry
            // must be updated to point to the fresh entry.
            let locked_dir2 = root
                .create_node(&current_task, "locked2".into(), mode!(IFDIR, 0o755), DeviceId::NONE)
                .expect("create locked2");
            locked_dir2.entry.node.update_info(|info| {
                info.encryption_policy = Some(FscryptNodePolicy {
                    key_identifier: key_id,
                    flags: FscryptPolicyFlags::empty(),
                });
            });
            // Looking up `app_node` under a different parent (`locked_dir2`) must not splice
            // `locked_dir`'s canonical entry across parents, and updates `canonical_dir_entry`.
            let cross_parent = locked_dir2
                .lookup_child(&current_task, &mut LookupContext::default(), "_nokey_app".into())
                .expect("lookup app_node under locked_dir2");
            assert!(!Arc::ptr_eq(&cross_parent.entry, &relocked.entry));
            assert!(Arc::ptr_eq(
                &cross_parent.entry.node.canonical_dir_entry.lock().entry.upgrade().unwrap(),
                &cross_parent.entry
            ));

            let dead_target = fs.create_node_and_allocate_node_id(
                CountingDirNode { lookup_count, entries: Default::default() },
                FsNodeInfo::new(mode!(IFDIR, 0o755), FsCred::root()),
            );
            {
                let mut existing = counting_fs(&fs).existing_nodes.lock();
                existing.insert("_nokey_dead".into(), dead_target.clone());
                existing.insert("dead.app".into(), dead_target);
            }
            let dead_entry = locked_dir2
                .lookup_child(&current_task, &mut LookupContext::default(), "_nokey_dead".into())
                .expect("lookup dead no-key");
            dead_entry.entry.raise_flags(DirEntryFlags::IS_DEAD);
            crypt.add_wrapping_key(&key, 0).expect("re-add key");
            let fresh = locked_dir2
                .lookup_child(&current_task, &mut LookupContext::default(), "dead.app".into())
                .expect("lookup plaintext after dead no-key");
            assert!(!Arc::ptr_eq(&fresh.entry, &dead_entry.entry));
            assert!(!fresh.entry.is_dead());
            assert!(Arc::ptr_eq(
                &fresh.entry.node.canonical_dir_entry.lock().entry.upgrade().unwrap(),
                &fresh.entry
            ));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_nokey_name_dir_entry() {
        spawn_kernel_and_run(async |current_task| {
            let (fs, lookup_count) = create_counting_fs(&current_task);
            let root = Namespace::new(fs.clone()).root();

            let create_file = |dir: &NamespaceNode, name: &str| {
                dir.create_node(&current_task, name.into(), mode!(IFREG, 0o644), DeviceId::NONE)
                    .expect("create file")
            };
            let create_dir = |dir: &NamespaceNode, name: &str| {
                dir.create_node(&current_task, name.into(), mode!(IFDIR, 0o755), DeviceId::NONE)
                    .expect("create dir")
            };
            let lookup = |dir: &NamespaceNode, name: &str| {
                dir.lookup_child(&current_task, &mut LookupContext::default(), name.into())
            };
            let rename =
                |old_dir: &NamespaceNode, old: &str, new_dir: &NamespaceNode, new: &str| {
                    NamespaceNode::rename(
                        &current_task,
                        old_dir,
                        old.into(),
                        new_dir,
                        new.into(),
                        RenameFlags::empty(),
                    )
                };
            let set_locked = |dir: &NamespaceNode, is_locked: bool| {
                dir.entry.node.update_info(|info| {
                    info.encryption_policy = is_locked.then(FscryptNodePolicy::default);
                });
            };

            let locked = create_dir(&root, "locked");
            let setup_sub = create_dir(&locked, "nokey_sub");
            create_file(&setup_sub, "leaf");
            create_file(&locked, "locked_file");
            drop(setup_sub);
            locked.entry.remove_child("nokey_sub".into(), &current_task.kernel().mounts);
            locked.entry.remove_child("locked_file".into(), &current_task.kernel().mounts);
            set_locked(&locked, true);

            // 1. A child resolved under a locked directory is marked `NO_KEY` and not cached in
            // `locked`.
            let before = lookup_count.load(Ordering::SeqCst);
            let nokey_sub = lookup(&locked, "nokey_sub").expect("lookup nokey_sub");
            assert_eq!(lookup_count.load(Ordering::SeqCst), before + 1);
            assert!(nokey_sub.entry.has_nokey_name());
            let _ = lookup(&locked, "nokey_sub").expect("second lookup while locked");
            assert_eq!(lookup_count.load(Ordering::SeqCst), before + 2);
            assert_eq!(
                rename(&locked, "nokey_sub", &locked, "renamed_while_locked").unwrap_err(),
                errno!(ENOKEY),
            );

            // 2. Unlock `locked`: until spliced by a plaintext lookup, `nokey_sub` refrains from
            // caching direct children, while plaintext children resolved under it do not inherit
            // `NO_KEY`.
            set_locked(&locked, false);
            let before_leaf = lookup_count.load(Ordering::SeqCst);
            let leaf1 = lookup(&nokey_sub, "leaf").expect("lookup leaf 1");
            assert!(!leaf1.entry.has_nokey_name());
            let leaf2 = lookup(&nokey_sub, "leaf").expect("lookup leaf 2");
            assert_eq!(lookup_count.load(Ordering::SeqCst), before_leaf + 2);
            assert!(!Arc::ptr_eq(&leaf1.entry, &leaf2.entry));

            // 3. Concurrent unlock mid-lookup rejects `NO_KEY` entries with `ENOKEY` on `rename`
            // and `ENOENT` on `unlink`.
            set_locked(&locked, true);
            *counting_fs(&fs).unlock_node_on_lookup.lock() = Some(locked.entry.node.clone());
            assert_eq!(
                rename(&locked, "nokey_sub", &locked, "new_name").unwrap_err(),
                errno!(ENOKEY),
            );

            set_locked(&locked, true);
            *counting_fs(&fs).unlock_node_on_lookup.lock() = Some(locked.entry.node.clone());
            assert_eq!(
                locked
                    .unlink(
                        &current_task,
                        "locked_file".into(),
                        UnlinkKind::NonDirectory,
                        DirectoryMode::AllowAny,
                    )
                    .unwrap_err(),
                errno!(ENOENT),
            );

            // 3b. Pipelined lookup across an unencrypted -> locked encrypted directory boundary
            // discards prefetched nodes so the child under the locked directory is stamped
            // `NO_KEY` and not cached.
            let pipelined_enc = create_dir(&root, "pipelined_enc");
            create_dir(&pipelined_enc, "nokey_sub");
            set_locked(&pipelined_enc, true);
            drop(pipelined_enc);
            root.entry.remove_child("pipelined_enc".into(), &current_task.kernel().mounts);

            let pipelined = root.entry.get_children_pipelined(
                &current_task,
                &root.mount,
                &["pipelined_enc".into(), "nokey_sub".into()],
            );
            let pipelined_enc_entry = pipelined[0].as_ref().expect("pipelined_enc");
            let pipelined_sub_entry = pipelined[1].as_ref().expect("pipelined nokey_sub");
            assert!(!pipelined_enc_entry.has_nokey_name());
            assert!(pipelined_sub_entry.has_nokey_name());

            // 4. Destination unlock mid-lookup (`lookup_replaced` TOCTOU) returns `ENOKEY` for
            // both missing and existing destinations, and moving a directory with a submount into
            // an unspliced `NO_KEY` parent preserves the submount without caching under
            // `nokey_sub`.
            let cached_dir = create_dir(&root, "cached_dir");
            create_file(&cached_dir, "toctou_src");
            set_locked(&locked, true);
            *counting_fs(&fs).unlock_node_on_lookup.lock() = Some(locked.entry.node.clone());
            assert_eq!(
                rename(&cached_dir, "toctou_src", &locked, "missing_while_locked").unwrap_err(),
                errno!(ENOKEY),
            );
            set_locked(&locked, true);
            *counting_fs(&fs).unlock_node_on_lookup.lock() = Some(locked.entry.node.clone());
            assert_eq!(
                rename(&cached_dir, "toctou_src", &locked, "locked_file").unwrap_err(),
                errno!(ENOKEY),
            );

            let moved_dir = create_dir(&cached_dir, "moved");
            let submount_point = create_dir(&moved_dir, "mnt");
            let (sub_fs, _) = create_counting_fs(&current_task);
            submount_point
                .mount(WhatToMount::Fs(sub_fs.clone()), MountpointFlags::empty())
                .expect("mount sub_fs");

            rename(&cached_dir, "moved", &nokey_sub, "moved_in_nokey")
                .expect("rename plaintext dir into unlocked nokey parent");
            assert!(!moved_dir.entry.has_nokey_name());
            assert!(!submount_point.entry.has_nokey_name());
            let lookups_before_nokey = lookup_count.load(Ordering::SeqCst);
            let _ = lookup(&nokey_sub, "moved_in_nokey").expect("lookup moved_in_nokey");
            assert_eq!(lookup_count.load(Ordering::SeqCst), lookups_before_nokey + 1);
            let resolved_mnt = lookup(&moved_dir, "mnt").expect("lookup mnt after rename");
            assert!(Arc::ptr_eq(&resolved_mnt.entry.node.fs(), &sub_fs));

            // 5. Renaming an uncached plaintext entry out of `nokey_sub` into `cached_dir` caches
            // it under `cached_dir`.
            create_dir(&nokey_sub, "from_nokey");
            rename(&nokey_sub, "from_nokey", &cached_dir, "into_cached")
                .expect("rename from nokey parent into cached_dir");
            let lookups_before = lookup_count.load(Ordering::SeqCst);
            let moved_into_cached = lookup(&cached_dir, "into_cached").expect("lookup into_cached");
            assert_eq!(lookup_count.load(Ordering::SeqCst), lookups_before);
            assert!(!moved_into_cached.entry.has_nokey_name());

            // 6. Looking up `nokey_sub` under the unlocked `locked` directory splices it in place,
            // clears `NO_KEY`, and enables child caching.
            let canonical_sub = lookup(&locked, "nokey_sub").expect("lookup canonical_sub");
            assert!(Arc::ptr_eq(&canonical_sub.entry, &nokey_sub.entry));
            assert!(!canonical_sub.entry.has_nokey_name());

            let cached_leaf1 = lookup(&canonical_sub, "leaf").expect("lookup cached_leaf1");
            let after_first = lookup_count.load(Ordering::SeqCst);
            let cached_leaf2 = lookup(&canonical_sub, "leaf").expect("lookup cached_leaf2");
            assert_eq!(lookup_count.load(Ordering::SeqCst), after_first);
            assert!(Arc::ptr_eq(&cached_leaf1.entry, &cached_leaf2.entry));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_failed_rename_does_not_leave_lock_order_behind() {
        spawn_kernel_and_run(async |current_task| {
            let (fs, _lookup_count) = create_counting_fs(&current_task);
            let root = Namespace::new(fs.clone()).root();

            let create_dir = |dir: &NamespaceNode, name: &str| {
                dir.create_node(&current_task, name.into(), mode!(IFDIR, 0o755), DeviceId::NONE)
                    .expect("create dir")
            };
            let create_file = |dir: &NamespaceNode, name: &str| {
                dir.create_node(&current_task, name.into(), mode!(IFREG, 0o644), DeviceId::NONE)
                    .expect("create file")
            };
            let rename = |old_dir: &NamespaceNode,
                          old: &str,
                          new_dir: &NamespaceNode,
                          new: &str,
                          flags: RenameFlags| {
                NamespaceNode::rename(
                    &current_task,
                    old_dir,
                    old.into(),
                    new_dir,
                    new.into(),
                    flags,
                )
            };

            // Renames lock unrelated parents in address order. Sort three directories by the
            // address of one of their locks into `a < b < c`. Then move `a` under `c`, so that
            // renames from `c` into `a` lock `c` before `a`.
            let setup = |prefix: &str, lock_addr: fn(&DirEntry) -> usize| {
                let mut dirs = [0, 1, 2].map(|i| {
                    let name = format!("{prefix}{i}");
                    let dir = create_dir(&root, &name);
                    create_file(&dir, "file");
                    (name, dir)
                });
                dirs.sort_by_key(|(_, dir)| lock_addr(&dir.entry));
                let [(a_name, a), (_, b), (_, c)] = dirs;
                rename(&root, &a_name, &c, &a_name, RenameFlags::empty()).expect("move a under c");
                (a, b, c)
            };
            // If a failed rename left its lock order behind, this closes `a -> b -> c -> a`.
            let rename_between = |a: &NamespaceNode, b: &NamespaceNode| {
                rename(a, "file", b, "moved", RenameFlags::empty())
                    .expect("rename between a and b");
            };

            // Renames that fail while holding the `children` locks.
            let (a, b, c) = setup("children", |entry| std::ptr::from_ref(&entry.children).addr());
            let flags = RenameFlags::empty();
            assert_eq!(rename(&c, "missing", &a, "x", flags).unwrap_err(), errno!(ENOENT));
            assert_eq!(rename(&b, "missing", &c, "x", flags).unwrap_err(), errno!(ENOENT));
            rename_between(&a, &b);

            // Renames that fail while also holding the `info` locks.
            let (a, b, c) =
                setup("info", |entry| std::ptr::from_ref(entry.node.info_lock()).addr());
            let flags = RenameFlags::NOREPLACE;
            assert_eq!(rename(&c, "file", &a, "file", flags).unwrap_err(), errno!(EEXIST));
            assert_eq!(rename(&b, "file", &c, "file", flags).unwrap_err(), errno!(EEXIST));
            rename_between(&a, &b);
        })
        .await;
    }
}
