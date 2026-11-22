// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use crate::vfs::{
    CloseFreeSafe, DirEntry, DirEntryOps, DirectoryEntryType, DirentSink, FileObject, FileOps,
    FileSystemHandle, FsNode, FsNodeHandle, FsNodeInfo, FsNodeOps, FsStr, FsString, SymlinkNode,
    emit_dotdot, fileops_impl_directory, fileops_impl_noop_sync, fileops_impl_unbounded_seek,
    fs_node_impl_dir_readonly,
};
use starnix_rcu::RcuReadScope;
use starnix_sync::{LockDepMutex, SimpleDirectoryEntriesLock, allow_subclass};
use starnix_uapi::auth::FsCred;
use starnix_uapi::device_id::DeviceId;
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::{FileMode, mode};
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::{errno, error};
use std::collections::BTreeMap;
use std::sync::Arc;

/// Helper used to populate a `SimpleDirectory` with nodes for a specific `FileSystem`.
pub struct SimpleDirectoryMutator {
    fs: FileSystemHandle,
    pub directory: Arc<SimpleDirectory>,
}

impl SimpleDirectoryMutator {
    /// Creates a mutator that will allocate nodes in `fs` and insert them into `directory`.
    pub fn new(fs: FileSystemHandle, directory: Arc<SimpleDirectory>) -> Self {
        Self { fs, directory }
    }

    pub fn node(&self, name: FsString, node: FsNodeHandle) {
        let mut state = self.directory.state.lock();
        if state.removed {
            return;
        }
        let child_dir_and_handler = state.not_found_handler.and_then(|handler| {
            node.downcast_ops::<Arc<SimpleDirectory>>().cloned().map(|dir| (dir, handler))
        });
        state.entries.insert(name, node);
        // Propagate the parent's handler after releasing `self.directory.state.lock()`, so that
        // at most one `SimpleDirectoryEntriesLock` is held at a time.
        std::mem::drop(state);
        if let Some((child_dir, handler)) = child_dir_and_handler {
            child_dir.set_not_found_handler(handler);
        }
    }

    pub fn entry(&self, name: &str, ops: impl Into<Box<dyn FsNodeOps>>, mode: FileMode) {
        let name: FsString = name.into();
        let node =
            self.fs.create_node_and_allocate_node_id(ops, FsNodeInfo::new(mode, FsCred::root()));
        self.node(name, node);
    }

    pub fn entry_etc(
        &self,
        name: FsString,
        ops: impl Into<Box<dyn FsNodeOps>>,
        mode: FileMode,
        dev: DeviceId,
        creds: FsCred,
    ) {
        let mut info = FsNodeInfo::new(mode, creds);
        info.rdev = dev;
        let node = self.fs.create_node_and_allocate_node_id(ops, info);
        self.node(name, node);
    }

    pub fn symlink(&self, name: &FsStr, target: &FsStr) {
        let (ops, info) = SymlinkNode::new(target, FsCred::root());
        let node = self.fs.create_node_and_allocate_node_id(ops, info);
        self.node(name.into(), node);
    }

    pub fn subdir(&self, name: &str, mode: u32, build_subdir: impl FnOnce(&Self)) {
        let name: &FsStr = name.into();
        self.subdir2(name, mode, build_subdir);
    }

    // TODO: Figure out a better way to overload this function for &str and &FsStr.
    pub fn subdir2(&self, name: &FsStr, mode: u32, build_subdir: impl FnOnce(&Self)) {
        if let Ok(dir) = self.directory.get_or_create_subdir(&self.fs, name, mode) {
            let mutator = SimpleDirectoryMutator::new(self.fs.clone(), dir);
            build_subdir(&mutator);
        }
    }

    pub fn remove(&self, name: &FsStr) {
        self.directory.remove(name);
    }
}

/// Handler invoked by [`FsNodeOps::lookup`] when the requested child of a [`SimpleDirectory`] is
/// not present.
///
/// The handler receives the [`DirEntry`] of the directory that was searched, and the `name` of
/// the requested child, and returns the [`Errno`] to report to the caller.
///
/// Child [`SimpleDirectory`] nodes created via [`SimpleDirectory::subdir`] or attached via
/// [`SimpleDirectoryMutator`] inherit their parent directory's handler.
pub(crate) type NotFoundHandler = fn(&DirEntry, &FsStr) -> Errno;

struct SimpleDirectoryState {
    entries: BTreeMap<FsString, FsNodeHandle>,
    not_found_handler: Option<NotFoundHandler>,
    removed: bool,
}

impl SimpleDirectoryState {
    fn try_insert(
        &mut self,
        fs: &FileSystemHandle,
        name: &FsStr,
        ops: impl Into<Box<dyn FsNodeOps>>,
        info: FsNodeInfo,
    ) -> Result<FsNodeHandle, Errno> {
        if self.removed {
            return error!(ENOENT);
        }
        if self.entries.contains_key(name) {
            return error!(EEXIST);
        }
        let node = fs.create_node_and_allocate_node_id(ops, info);
        self.entries.insert(name.into(), node.clone());
        Ok(node)
    }

    fn create_subdir(
        &mut self,
        fs: &FileSystemHandle,
        name: &FsStr,
        mode: u32,
    ) -> Result<Arc<SimpleDirectory>, Errno> {
        let dir = Arc::new(SimpleDirectory {
            state: LockDepMutex::new(SimpleDirectoryState {
                entries: Default::default(),
                not_found_handler: self.not_found_handler,
                removed: false,
            }),
        });
        self.try_insert(
            fs,
            name,
            dir.clone(),
            FsNodeInfo::new(mode!(IFDIR, mode), FsCred::root()),
        )?;
        Ok(dir)
    }

    fn get_or_create_subdir(
        &mut self,
        fs: &FileSystemHandle,
        name: &FsStr,
        mode: u32,
    ) -> Result<Arc<SimpleDirectory>, Errno> {
        if self.removed {
            return error!(ENOENT);
        }
        if let Some(node) = self.entries.get(name) {
            let dir = node.downcast_ops::<Arc<SimpleDirectory>>().ok_or_else(|| errno!(EEXIST))?;
            debug_assert!(node.info().mode.is_dir());
            Ok(dir.clone())
        } else {
            self.create_subdir(fs, name, mode)
        }
    }
}

/// Returns `ENOENT`, with context identifying the directory (via its [`DirEntry`] `Debug` form)
/// and the `name` that was looked up in it.
fn default_not_found_handler(entry: &DirEntry, name: &FsStr) -> Errno {
    errno!(ENOENT, format!("Looking for {name} in {entry:?}"))
}

/// Common implementation of a simple read-only directory `FsNodeOps`.
///
/// `SimpleDirectoryMutator` is used to populate the directory with child `FsNode`s allocated
/// in the desired (usually kernel-internal, e.g. "sysfs", "proc", etc) filesystem.
pub struct SimpleDirectory {
    state: LockDepMutex<SimpleDirectoryState, SimpleDirectoryEntriesLock>,
}

impl SimpleDirectory {
    /// Returns a new instance with a default handler that returns `ENOENT` and logs context
    /// when a child is not found.
    pub fn new() -> Arc<Self> {
        Arc::new(SimpleDirectory {
            state: LockDepMutex::new(SimpleDirectoryState {
                entries: Default::default(),
                not_found_handler: None,
                removed: false,
            }),
        })
    }

    /// Installs `not_found_handler` on this directory and recursively across all existing
    /// and future [`SimpleDirectory`] descendants.
    pub(crate) fn set_not_found_handler(&self, not_found_handler: NotFoundHandler) {
        // Collect the child directories and release this directory's lock before recursing, so
        // that at most one `SimpleDirectoryEntriesLock` is held at a time.
        let child_dirs: Vec<Arc<SimpleDirectory>> = {
            let mut state = self.state.lock();
            state.not_found_handler = Some(not_found_handler);
            state
                .entries
                .values()
                .filter_map(|node| node.downcast_ops::<Arc<SimpleDirectory>>().cloned())
                .collect()
        };
        for child_dir in child_dirs {
            child_dir.set_not_found_handler(not_found_handler);
        }
    }

    /// Returns whether this directory has been removed from its parent directory.
    pub fn is_removed(&self) -> bool {
        self.state.lock().removed
    }

    fn mark_removed(node: FsNodeHandle) {
        let mut stack: Vec<Arc<SimpleDirectory>> =
            node.downcast_ops::<Arc<SimpleDirectory>>().cloned().into_iter().collect();
        while let Some(dir) = stack.pop() {
            let entries = {
                let mut state = dir.state.lock();
                state.removed = true;
                std::mem::take(&mut state.entries)
            };
            stack.extend(
                entries
                    .into_values()
                    .filter_map(|child| child.downcast_ops::<Arc<SimpleDirectory>>().cloned()),
            );
        }
    }

    pub fn remove(&self, name: &FsStr) {
        let removed = self.state.lock().entries.remove(name);
        if let Some(removed) = removed {
            Self::mark_removed(removed);
        }
    }

    fn walk<'a>(self: &Arc<Self>, path: &'a FsStr) -> Option<(Arc<Self>, &'a FsStr)> {
        fn check_component(component: &FsStr) {
            assert!(!component.is_empty());

            let dot: &FsStr = b".".into();
            assert_ne!(component, dot);

            let dotdot: &FsStr = b"..".into();
            assert_ne!(component, dotdot);
        }

        let mut components = path.split(|c| *c == b'/');
        let basename = components.next_back()?;
        let basename: &FsStr = basename.into();
        check_component(basename);
        let mut parent = self.clone();
        while let Some(component) = components.next() {
            let component: &FsStr = component.into();
            check_component(component);
            let Some(next) = parent.get_dir(component) else {
                return None;
            };
            parent = next;
        }
        Some((parent, basename))
    }

    pub fn edit(
        self: &Arc<Self>,
        fs: &FileSystemHandle,
        callback: impl FnOnce(&SimpleDirectoryMutator),
    ) {
        if self.is_removed() {
            return;
        }
        let mutator = SimpleDirectoryMutator::new(fs.clone(), self.clone());
        callback(&mutator);
    }

    fn get_or_create_subdir(
        &self,
        fs: &FileSystemHandle,
        name: &FsStr,
        mode: u32,
    ) -> Result<Arc<SimpleDirectory>, Errno> {
        self.state.lock().get_or_create_subdir(fs, name, mode)
    }

    pub fn subdir(&self, fs: &FileSystemHandle, name: &FsStr, mode: u32) -> Arc<SimpleDirectory> {
        self.get_or_create_subdir(fs, name, mode).expect("subdir is a SimpleDirectory")
    }

    /// Creates a new subdirectory named `name`, or returns `EEXIST` if an entry with `name`
    /// already exists or `ENOENT` if this directory has been removed.
    pub fn create_subdir(
        &self,
        fs: &FileSystemHandle,
        name: &FsStr,
        mode: u32,
    ) -> Result<Arc<SimpleDirectory>, Errno> {
        self.state.lock().create_subdir(fs, name, mode)
    }

    /// Creates or looks up `subdir_name`, and creates a new `child_name` subdirectory within it
    /// while holding this directory's lock so the intermediate subdirectory cannot be removed
    /// concurrently by [`Self::remove_from_subdir_if_empty`].
    ///
    /// Returns `EEXIST` if `child_name` already exists in `subdir_name` or if `subdir_name`
    /// exists and is not a [`SimpleDirectory`], or `ENOENT` if this directory has been removed.
    pub fn create_nested_subdir(
        &self,
        fs: &FileSystemHandle,
        subdir_name: &FsStr,
        subdir_mode: u32,
        child_name: &FsStr,
        child_mode: u32,
    ) -> Result<Arc<SimpleDirectory>, Errno> {
        let mut state = self.state.lock();
        let was_created = !state.entries.contains_key(subdir_name);
        let subdir = state.get_or_create_subdir(fs, subdir_name, subdir_mode)?;
        // Safe because locking parent then child strictly follows the directory tree hierarchy.
        let _token = allow_subclass();
        subdir.create_subdir(fs, child_name, child_mode).inspect_err(|_| {
            if was_created && let Some(removed) = state.entries.remove(subdir_name) {
                Self::mark_removed(removed);
            }
        })
    }

    /// Creates a symlink named `name` pointing to `target`.
    ///
    /// Returns `EEXIST` if an entry with `name` already exists, or `ENOENT` if this directory has
    /// been removed.
    pub fn create_symlink(
        self: &Arc<Self>,
        fs: &FileSystemHandle,
        name: &FsStr,
        target: &FsStr,
    ) -> Result<(), Errno> {
        let (ops, info) = SymlinkNode::new(target, FsCred::root());
        self.state.lock().try_insert(fs, name, ops, info)?;
        Ok(())
    }

    /// Removes `child_name` from `subdir_name`, and removes `subdir_name` itself if it becomes
    /// empty, atomically with respect to [`Self::create_nested_subdir`].
    pub fn remove_from_subdir_if_empty(&self, subdir_name: &FsStr, child_name: &FsStr) {
        let mut state = self.state.lock();
        let Some(subdir) = state
            .entries
            .get(subdir_name)
            .and_then(|node| node.downcast_ops::<Arc<SimpleDirectory>>())
            .map(Arc::clone)
        else {
            return;
        };
        let (removed_child, is_empty) = {
            // Safe because locking parent then child strictly follows the directory tree hierarchy.
            let _token = allow_subclass();
            let mut subdir_state = subdir.state.lock();
            let removed_child = subdir_state.entries.remove(child_name);
            let is_empty = subdir_state.entries.is_empty();
            if is_empty {
                subdir_state.removed = true;
            }
            (removed_child, is_empty)
        };
        if is_empty {
            state.entries.remove(subdir_name);
        }
        std::mem::drop(state);
        if let Some(removed_child) = removed_child {
            Self::mark_removed(removed_child);
        }
    }

    fn get(&self, name: &FsStr) -> Option<FsNodeHandle> {
        let state = self.state.lock();
        state.entries.get(name).cloned()
    }

    pub fn get_dir(&self, name: &FsStr) -> Option<Arc<SimpleDirectory>> {
        let state = self.state.lock();
        state
            .entries
            .get(name)
            .and_then(|node| node.downcast_ops::<Arc<SimpleDirectory>>())
            .map(Arc::clone)
    }

    pub fn lookup(self: &Arc<Self>, path: &FsStr) -> Option<FsNodeHandle> {
        let (parent, basename) = self.walk(path)?;
        parent.get(basename)
    }

    pub fn into_node(self: Arc<Self>, fs: &FileSystemHandle, mode: u32) -> FsNodeHandle {
        let info = FsNodeInfo::new(mode!(IFDIR, mode), FsCred::root());
        fs.create_node_and_allocate_node_id(self, info)
    }
}

impl FsNodeOps for Arc<SimpleDirectory> {
    fs_node_impl_dir_readonly!();

    fn create_child_dir_entry_ops(&self) -> Option<Box<dyn DirEntryOps>> {
        Some(Box::new(self.clone()))
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
        _current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        let not_found_handler = {
            let state = self.state.lock();
            if let Some(node) = state.entries.get(name) {
                return Ok(node.clone());
            }
            state.not_found_handler.unwrap_or(default_not_found_handler)
        };
        // Invoke the handler after releasing the directory lock, so that handlers which format
        // paths or log diagnostics do not extend the critical section.
        Err(not_found_handler(entry, name))
    }
}

impl DirEntryOps for Arc<SimpleDirectory> {
    fn revalidate(&self, _: &CurrentTask, child: &DirEntry) -> Result<bool, Errno> {
        // Entries of a `SimpleDirectory` can be removed or replaced without going through the VFS
        // (e.g. when a device is removed from sysfs), which would otherwise leave stale
        // `DirEntry`s in the cache of the file system. Revalidation evicts a cached child
        // `DirEntry` once its node is no longer present under the same name in its parent
        // `SimpleDirectory`.
        let scope = RcuReadScope::new();
        Ok(self
            .state
            .lock()
            .entries
            .get(child.local_name(&scope))
            .is_some_and(|node| Arc::ptr_eq(node, &child.node)))
    }
}

/// `SimpleDirectory` doesn't implement the `close` method.
impl CloseFreeSafe for SimpleDirectory {}
impl FileOps for SimpleDirectory {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();

    fn readdir(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        emit_dotdot(file, sink)?;

        // Skip through the entries until the current offset is reached.
        // Subtract 2 from the offset to account for `.` and `..`.
        let state = self.state.lock();
        for (name, node) in state.entries.iter().skip(sink.offset() as usize - 2) {
            let mode = node.info().mode;
            sink.add(
                node.ino,
                sink.offset() + 1,
                DirectoryEntryType::from_mode(mode),
                name.as_ref(),
            )?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::spawn_kernel_and_run;
    use crate::vfs::{DirEntryHandle, FsNodeOps, MountInfo};
    use starnix_uapi::errno;

    #[fuchsia::test]
    async fn test_default_not_found_handler() {
        spawn_kernel_and_run(async |current_task| {
            let dir = SimpleDirectory::new();
            let node = dir.clone().into_node(&current_task.fs().root().entry.node.fs(), 0o777);
            let entry = DirEntry::new_unrooted(node);
            let result = FsNodeOps::lookup(&dir, &entry, &current_task, "nonexistent".into());
            assert_eq!(result.unwrap_err(), errno!(ENOENT));
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_set_not_found_handler() {
        #[track_caller]
        fn check_lookup(
            current_task: &CurrentTask,
            root: &DirEntryHandle,
            path: &str,
            expected_error: Errno,
        ) {
            let mount = MountInfo::detached();
            let mut dir = root.clone();
            for component in path.split('/').filter(|s| !s.is_empty()) {
                dir = dir.component_lookup(current_task, &mount, component.into()).unwrap();
            }
            let res = dir.component_lookup(current_task, &mount, "expected_error".into());
            assert_eq!(res.unwrap_err(), expected_error);
        }

        spawn_kernel_and_run(async |current_task| {
            let fs = current_task.fs().root().entry.node.fs();
            let root_dir = SimpleDirectory::new();
            root_dir.subdir(&fs, "existing".into(), 0o755).subdir(&fs, "nested".into(), 0o755);

            root_dir.set_not_found_handler(|entry, name| {
                if name == "expected_error" && entry.parent().is_some() {
                    errno!(EACCES)
                } else {
                    errno!(ENOENT)
                }
            });

            root_dir.subdir(&fs, "future".into(), 0o755);

            let attached = SimpleDirectory::new();
            attached.subdir(&fs, "nested".into(), 0o755);
            root_dir.edit(&fs, |dir| {
                dir.entry("attached", attached, mode!(IFDIR, 0o755));
            });

            let root = DirEntry::new_unrooted(root_dir.into_node(&fs, 0o755));
            check_lookup(&current_task, &root, "", errno!(ENOENT));
            for path in ["existing", "existing/nested", "future", "attached", "attached/nested"] {
                check_lookup(&current_task, &root, path, errno!(EACCES));
            }
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_simple_directory_lookups() {
        spawn_kernel_and_run(async |current_task| {
            let fs = current_task.fs().root().entry.node.fs();
            let dir = SimpleDirectory::new();
            let mutator = SimpleDirectoryMutator::new(fs.clone(), dir.clone());

            // Add a symlink
            mutator.symlink("link".into(), "target".into());

            // Add a subdir
            mutator.subdir("subdir", 0o755, |sub_mutator| {
                sub_mutator.symlink("sublink".into(), "subtarget".into());
            });

            let node = dir.clone().into_node(&fs, 0o777);
            let entry = DirEntry::new_unrooted(node);

            // Verify that lookup returns the same FsNodeHandle for multiple calls.
            let node1 =
                FsNodeOps::lookup(&dir, &entry, &current_task, "link".into()).expect("lookup link");
            let node2 = FsNodeOps::lookup(&dir, &entry, &current_task, "link".into())
                .expect("lookup link again");

            assert!(Arc::ptr_eq(&node1, &node2));
            assert!(node1.info().mode.is_lnk());

            // Verify that lookup returns the same FsNodeHandle for subdirectories.
            let subdir1 = FsNodeOps::lookup(&dir, &entry, &current_task, "subdir".into())
                .expect("lookup subdir");
            let subdir2 = FsNodeOps::lookup(&dir, &entry, &current_task, "subdir".into())
                .expect("lookup subdir again");

            assert!(Arc::ptr_eq(&subdir1, &subdir2));
            assert!(subdir1.info().mode.is_dir());

            // Verify that the SimpleDirectory::lookup helper works for nested paths.
            let sublink = dir.lookup("subdir/sublink".into()).expect("lookup subdir/sublink");
            assert!(sublink.info().mode.is_lnk());

            // Verify that removing an entry works and evicts any cached DirEntry.
            let mount = MountInfo::detached();
            let cached_link = entry
                .component_lookup(&current_task, &mount, "link".into())
                .expect("component_lookup link");
            assert!(Arc::ptr_eq(&cached_link.node, &node1));
            let cached_link_again = entry
                .component_lookup(&current_task, &mount, "link".into())
                .expect("component_lookup link again");
            assert!(Arc::ptr_eq(&cached_link, &cached_link_again));

            mutator.remove("link".into());
            let result = FsNodeOps::lookup(&dir, &entry, &current_task, "link".into());
            assert_eq!(result.unwrap_err(), errno!(ENOENT));
            assert_eq!(
                entry.component_lookup(&current_task, &mount, "link".into()).unwrap_err(),
                errno!(ENOENT)
            );
            assert!(cached_link.is_dead());

            let cached_subdir = entry
                .component_lookup(&current_task, &mount, "subdir".into())
                .expect("component_lookup subdir");
            assert!(Arc::ptr_eq(&cached_subdir.node, &subdir1));
            let cached_subdir_again = entry
                .component_lookup(&current_task, &mount, "subdir".into())
                .expect("component_lookup subdir again");
            assert!(Arc::ptr_eq(&cached_subdir, &cached_subdir_again));

            // Replacing the subdirectory without an intervening lookup should evict the old
            // cached DirEntry via the `Arc::ptr_eq` check in
            // `<Arc<SimpleDirectory> as DirEntryOps>::revalidate`.
            mutator.remove("subdir".into());
            mutator.subdir("subdir", 0o755, |_| {});
            let replaced_subdir = entry
                .component_lookup(&current_task, &mount, "subdir".into())
                .expect("component_lookup replaced subdir");
            assert!(cached_subdir.is_dead());
            assert!(!Arc::ptr_eq(&replaced_subdir, &cached_subdir));
            assert!(!Arc::ptr_eq(&replaced_subdir.node, &subdir1));

            // Removing the subdirectory should evict the cached DirEntry and return ENOENT.
            mutator.remove("subdir".into());
            assert_eq!(
                entry.component_lookup(&current_task, &mount, "subdir".into()).unwrap_err(),
                errno!(ENOENT)
            );
            assert!(replaced_subdir.is_dead());
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_simple_directory_create_rejects_duplicates() {
        spawn_kernel_and_run(async |current_task| {
            let fs = current_task.fs().root().entry.node.fs();
            let dir = SimpleDirectory::new();

            dir.create_subdir(&fs, "sub".into(), 0o755).expect("create_subdir");
            assert_eq!(dir.create_subdir(&fs, "sub".into(), 0o755).err(), Some(errno!(EEXIST)));
            assert_eq!(
                dir.create_symlink(&fs, "sub".into(), "target".into()).err(),
                Some(errno!(EEXIST))
            );

            dir.create_symlink(&fs, "link".into(), "target".into()).expect("create_symlink");
            assert_eq!(
                dir.create_symlink(&fs, "link".into(), "target2".into()).err(),
                Some(errno!(EEXIST))
            );
            assert_eq!(dir.create_subdir(&fs, "link".into(), 0o755).err(), Some(errno!(EEXIST)));
            assert_eq!(
                dir.create_nested_subdir(&fs, "link".into(), 0o755, "child".into(), 0o755).err(),
                Some(errno!(EEXIST))
            );

            dir.create_nested_subdir(&fs, "nested".into(), 0o755, "child".into(), 0o755)
                .expect("create_nested_subdir");
            assert_eq!(
                dir.create_nested_subdir(&fs, "nested".into(), 0o755, "child".into(), 0o755).err(),
                Some(errno!(EEXIST))
            );
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_simple_directory_create_under_removed_dir_returns_enoent() {
        spawn_kernel_and_run(async |current_task| {
            let fs = current_task.fs().root().entry.node.fs();
            let dir = SimpleDirectory::new();

            let sub = dir.create_subdir(&fs, "sub".into(), 0o755).expect("create_subdir");
            let child = sub.create_subdir(&fs, "child".into(), 0o755).expect("create child");
            dir.remove("sub".into());
            assert!(sub.is_removed());
            assert!(child.is_removed());
            assert_eq!(
                sub.create_subdir(&fs, "grandchild".into(), 0o755).err(),
                Some(errno!(ENOENT))
            );
            assert_eq!(
                sub.create_nested_subdir(&fs, "glue".into(), 0o755, "grandchild".into(), 0o755)
                    .err(),
                Some(errno!(ENOENT))
            );
            assert_eq!(
                sub.create_symlink(&fs, "link".into(), "target".into()).err(),
                Some(errno!(ENOENT))
            );

            let nested_child = dir
                .create_nested_subdir(&fs, "nested".into(), 0o755, "child".into(), 0o755)
                .expect("create_nested_subdir");
            dir.remove_from_subdir_if_empty("nested".into(), "child".into());
            assert!(dir.lookup("nested".into()).is_none());
            assert!(nested_child.is_removed());
            assert_eq!(
                nested_child.create_subdir(&fs, "grandchild".into(), 0o755).err(),
                Some(errno!(ENOENT))
            );
            dir.create_nested_subdir(&fs, "nested".into(), 0o755, "child".into(), 0o755)
                .expect("create_nested_subdir after removal");
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_simple_directory_edit_on_removed_dir_is_noop() {
        spawn_kernel_and_run(async |current_task| {
            let fs = current_task.fs().root().entry.node.fs();
            let dir = SimpleDirectory::new();

            let sub = dir.create_subdir(&fs, "sub".into(), 0o755).expect("create_subdir");
            dir.remove("sub".into());
            assert!(sub.is_removed());

            sub.edit(&fs, |mutator| {
                mutator.subdir("child", 0o755, |_| {});
                mutator.symlink("link".into(), "target".into());
            });
            assert!(sub.lookup("child".into()).is_none());
            assert!(sub.lookup("link".into()).is_none());

            // Also verify direct mutator operations after removal act as no-ops without panicking.
            let mutator = SimpleDirectoryMutator::new(fs.clone(), sub.clone());
            mutator.subdir("child2", 0o755, |_| {});
            mutator.symlink("link2".into(), "target".into());
            assert!(sub.lookup("child2".into()).is_none());
            assert!(sub.lookup("link2".into()).is_none());
        })
        .await;
    }

    #[fuchsia::test]
    async fn test_simple_directory_create_nested_subdir_preserves_existing_parent() {
        spawn_kernel_and_run(async |current_task| {
            let fs = current_task.fs().root().entry.node.fs();
            let dir = SimpleDirectory::new();

            let existing = dir.create_subdir(&fs, "existing".into(), 0o755).expect("create_subdir");
            existing.create_subdir(&fs, "child".into(), 0o755).expect("create child");

            assert_eq!(
                dir.create_nested_subdir(&fs, "existing".into(), 0o755, "child".into(), 0o755)
                    .err(),
                Some(errno!(EEXIST))
            );
            assert!(dir.lookup("existing".into()).is_some());
            assert!(!existing.is_removed());

            dir.create_symlink(&fs, "link".into(), "target".into()).expect("create_symlink");
            assert_eq!(
                dir.create_nested_subdir(&fs, "link".into(), 0o755, "child".into(), 0o755).err(),
                Some(errno!(EEXIST))
            );
            assert!(dir.lookup("link".into()).is_some());
        })
        .await;
    }
}
