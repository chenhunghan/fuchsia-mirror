// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::{HidePid, ProcFs};
use fuchsia_rcu::RcuReadScope;
use starnix_core::mm::{
    MemoryAccessor, MemoryAccessorExt, MemoryManager, MemoryStats, PAGE_SIZE, ProcMapsFile,
    ProcPagemapFile, ProcSmapsFile, ProcSmapsRollupFile, SelfStatsCacheMode,
};
use starnix_core::security;
use starnix_core::task::{CurrentTask, Pid, Task, TaskContainer, TaskEntryScope, path_from_root};
use starnix_core::vfs::buffers::{InputBuffer, OutputBuffer};
use starnix_core::vfs::pseudo::dynamic_file::{DynamicFile, DynamicFileBuf, DynamicFileSource};
use starnix_core::vfs::pseudo::simple_file::{
    BytesFile, BytesFileOps, SimpleFileNode, parse_i32_file, parse_unsigned_file,
    serialize_for_file,
};
use starnix_core::vfs::pseudo::stub_empty_file::StubEmptyFile;
use starnix_core::vfs::pseudo::vec_directory::{VecDirectory, VecDirectoryEntry};
use starnix_core::vfs::{
    CallbackSymlinkNode, CheckAccessReason, CloseFreeSafe, DirEntry, DirEntryOps,
    DirectoryEntryType, DirentSink, FdFlags, FdNumber, FileHandle, FileObject, FileOps,
    FileSystemHandle, FsNode, FsNodeHandle, FsNodeInfo, FsNodeOps, FsStr, FsString,
    MountNamespaceFile, Namespace, ProcMountinfoFile, ProcMountsFile, SeekTarget, StatxFlags,
    SymlinkTarget, default_seek, emit_dotdot, fileops_impl_directory, fileops_impl_noop_sync,
    fileops_impl_seekable, fileops_impl_unbounded_seek, fs_node_impl_dir_readonly,
    fs_node_impl_dir_readonly_ops, fs_node_impl_not_dir, fs_node_impl_symlink,
};
use starnix_logging::{bug_ref, track_stub};
use starnix_sync::{DynamicLockDepRwLock, LockDepReadGuard};

use starnix_task_command::TaskCommand;
use starnix_types::time::duration_to_scheduler_clock;
use starnix_uapi::auth::{
    CAP_SYS_NICE, CAP_SYS_RESOURCE, FsCred, PTRACE_MODE_ATTACH_FSCREDS, PTRACE_MODE_NOAUDIT,
    PTRACE_MODE_READ_FSCREDS, PtraceAccessMode,
};
use starnix_uapi::errors::Errno;
use starnix_uapi::file_mode::{Access, FileMode, mode};
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::resource_limits::Resource;
use starnix_uapi::user_address::UserAddress;
use starnix_uapi::{
    OOM_ADJUST_MIN, OOM_DISABLE, OOM_SCORE_ADJ_MIN, RLIM_INFINITY, errno, error, ino_t, off_t,
    pid_t, uapi,
};
use std::borrow::Cow;
use std::ops::{Deref, Range};
use std::sync::{Arc, Weak};

/// Static table of entries in `/proc/<pid>` and `/proc/<pid>/task/<tid>`.
const TASK_ENTRIES: &[(&[u8], FileMode)] = &[
    // NOTE: keep entries in sync with `TaskDirectoryNode::lookup()`.
    (b"cgroup", mode!(IFREG, 0o444)),
    (b"cwd", mode!(IFLNK, 0o777)),
    (b"exe", mode!(IFLNK, 0o777)),
    (b"fd", mode!(IFDIR, 0o500)),
    (b"fdinfo", mode!(IFDIR, 0o555)),
    (b"io", mode!(IFREG, 0o400)),
    (b"limits", mode!(IFREG, 0o444)),
    (b"maps", mode!(IFREG, 0o444)),
    (b"mem", mode!(IFREG, 0o600)),
    (b"root", mode!(IFLNK, 0o777)),
    (b"sched", mode!(IFREG, 0o644)),
    (b"schedstat", mode!(IFREG, 0o444)),
    (b"smaps", mode!(IFREG, 0o444)),
    (b"smaps_rollup", mode!(IFREG, 0o444)),
    (b"stat", mode!(IFREG, 0o444)),
    (b"statm", mode!(IFREG, 0o444)),
    (b"status", mode!(IFREG, 0o444)),
    (b"cmdline", mode!(IFREG, 0o444)),
    (b"environ", mode!(IFREG, 0o400)),
    (b"auxv", mode!(IFREG, 0o400)),
    (b"comm", mode!(IFREG, 0o644)),
    (b"attr", mode!(IFDIR, 0o555)),
    (b"ns", mode!(IFDIR, 0o511)),
    (b"mountinfo", mode!(IFREG, 0o444)),
    (b"mounts", mode!(IFREG, 0o444)),
    (b"oom_adj", mode!(IFREG, 0o744)),
    (b"oom_score", mode!(IFREG, 0o444)),
    (b"oom_score_adj", mode!(IFREG, 0o744)),
    (b"timerslack_ns", mode!(IFREG, 0o666)),
    (b"wchan", mode!(IFREG, 0o444)),
    (b"clear_refs", mode!(IFREG, 0o200)),
    (b"pagemap", mode!(IFREG, 0o400)),
    // "task" must be last so we can dynamically include it for ThreadGroups.
    (b"task", mode!(IFDIR, 0o555)),
];

/// Returns entries for the `scope` of a task.
fn task_entries(scope: TaskEntryScope) -> &'static [(&'static [u8], FileMode)] {
    match scope {
        TaskEntryScope::Task => &TASK_ENTRIES[..TASK_ENTRIES.len() - 1],
        TaskEntryScope::ThreadGroup => TASK_ENTRIES,
    }
}

/// Represents a directory node for either `/proc/<pid>` or `/proc/<pid>/task/<tid>`.
///
/// This directory lazily creates its child entries to save memory.
///
/// It pre-allocates a range of inode numbers (`inode_range`) for all its child entries to mark
/// them as unchanged when re-accessed.
///
/// Child entries are owned by the task's effective uid/gid at the time they are looked up. The
/// directory node itself also refreshes its owner on stat, so it reflects credential changes made
/// after it was created.
pub struct TaskDirectory {
    target: TaskContainer,
    inode_range: Range<ino_t>,
    /// Inode numbers for the entries of `attr`, reserved here so that they don't change each time
    /// `attr` is looked up again.
    attr_inode_range: Range<ino_t>,
}

#[derive(Clone)]
struct TaskDirectoryNode {
    task_directory: Arc<TaskDirectory>,
}

impl Deref for TaskDirectoryNode {
    type Target = TaskDirectory;

    fn deref(&self) -> &Self::Target {
        &self.task_directory
    }
}

/// Refreshes `node`'s `uid`/`gid` to `task`'s effective uid and gid.
fn update_task_node_info(node: &FsNode, task: &Task) {
    let need_update = {
        let creds = task.real_creds().euid_as_fscred();
        let info = node.info();
        info.uid != creds.uid || info.gid != creds.gid
    };
    if need_update {
        node.update_info(|info| {
            // Read the creds again under the lock. Otherwise a refresh that read them before a
            // concurrent credential change could overwrite the new owner with the old one.
            let creds = task.real_creds().euid_as_fscred();
            info.uid = creds.uid;
            info.gid = creds.gid;
        });
    }
}

impl TaskDirectory {
    fn new(fs: &FileSystemHandle, task: &Arc<Task>, target: TaskContainer) -> FsNodeHandle {
        let creds = task.real_creds().euid_as_fscred();
        let scope = target.scope;
        fs.create_node_and_allocate_node_id(
            TaskDirectoryNode {
                task_directory: Arc::new(TaskDirectory {
                    target,
                    inode_range: fs.allocate_ino_range(task_entries(scope).len()),
                    attr_inode_range: fs.allocate_ino_range(ATTR_ENTRIES.len()),
                }),
            },
            FsNodeInfo::new(mode!(IFDIR, 0o555), creds),
        )
    }
}

/// Resolves `target` and checks that `hidepid=invisible` on `node`'s procfs mount lets
/// `current_task` see it.
///
/// Returns `Ok(None)` if the task has exited (so that a caller revalidating a `DirEntry` evicts
/// it), or `Err(ENOENT)` if `hidepid=invisible` hides the task from `current_task`.
fn resolve_visible_task(
    target: &TaskContainer,
    current_task: &CurrentTask,
    node: &FsNode,
) -> Result<Option<Arc<Task>>, Errno> {
    let Ok(task) = target.get_task() else {
        return Ok(None);
    };
    if let Some(proc_fs) = node.fs().downcast_ops::<ProcFs>() {
        if !proc_fs.has_pid_permissions(current_task, &task, HidePid::Invisible) {
            return error!(ENOENT);
        }
    }
    Ok(Some(task))
}

/// Revalidates `/proc/<pid>` and `/proc/<pid>/task/<tid>` `DirEntry`s against task liveness and
/// `hidepid=invisible`, and updates `uid`/`gid` on credential changes.
struct TaskDirectoryDirEntryOps {
    target: TaskContainer,
}

impl DirEntryOps for TaskDirectoryDirEntryOps {
    fn revalidate(&self, current_task: &CurrentTask, dir_entry: &DirEntry) -> Result<bool, Errno> {
        let Some(task) = resolve_visible_task(&self.target, current_task, &dir_entry.node)? else {
            return Ok(false);
        };
        update_task_node_info(&dir_entry.node, &task);
        Ok(true)
    }
}

/// Revalidates `/proc/<pid>` child `DirEntry`s against task liveness and updates `uid`/`gid` on
/// credential changes.
///
/// Unlike [`TaskDirectoryDirEntryOps`], this does not check `hidepid`: path walks through
/// `/proc/<pid>` or `task/<tid>` already enforce `hidepid` on the parent directory, while lookups
/// through an already-open subdirectory FD (`attr`, `fd`, `fdinfo`, `ns`) remain allowed.
struct TaskChildDirEntryOps {
    target: TaskContainer,
}

impl DirEntryOps for TaskChildDirEntryOps {
    fn revalidate(&self, _current_task: &CurrentTask, dir_entry: &DirEntry) -> Result<bool, Errno> {
        let Ok(task) = self.target.get_task() else {
            return Ok(false);
        };
        update_task_node_info(&dir_entry.node, &task);
        Ok(true)
    }
}

macro_rules! task_dir_impl_readonly {
    () => {
        // Used for `/proc/<pid>` and `/proc/<pid>/task/<tid>`, whose parents (`ProcDirectory` and
        // `TaskListDirectory`) do not implement `create_child_dir_entry_ops`. For
        // `TaskListDirectory` (`/proc/<pid>/task`), `DirEntry` creation prefers the parent
        // `TaskDirectoryNode::create_child_dir_entry_ops` (`TaskChildDirEntryOps`), which is safe
        // because `/proc/<pid>`'s `check_access` already enforces `hidepid`.
        fn create_dir_entry_ops(&self) -> Box<dyn DirEntryOps> {
            Box::new(TaskDirectoryDirEntryOps { target: self.target.clone() })
        }

        fn check_access(
            &self,
            node: &FsNode,
            current_task: &CurrentTask,
            permission_flags: security::PermissionFlags,
            info: &DynamicLockDepRwLock<FsNodeInfo>,
            reason: CheckAccessReason,
            audit_context: security::Auditable<'_>,
        ) -> Result<(), Errno> {
            if let Some(proc_fs) = node.fs().downcast_ops::<ProcFs>() {
                proc_fs.check_pid_access(current_task, &self.target, HidePid::NoAccess)?;
            }
            if permission_flags.as_access().contains(Access::WRITE) {
                return error!(EROFS, format!("check_access failed: read-only directory"));
            }
            node.default_check_access_impl(
                current_task,
                permission_flags,
                reason,
                info.read(),
                audit_context,
            )
        }

        fn fetch_and_refresh_info<'a>(
            &self,
            node: &FsNode,
            current_task: &CurrentTask,
            info: &'a DynamicLockDepRwLock<FsNodeInfo>,
            _flags: StatxFlags,
        ) -> Result<LockDepReadGuard<'a, FsNodeInfo>, Errno> {
            if let Some(target_task) = resolve_visible_task(&self.target, current_task, node)? {
                update_task_node_info(node, &target_task);
            }
            Ok(info.read())
        }

        fs_node_impl_dir_readonly_ops!();
    };
}

impl FsNodeOps for TaskDirectoryNode {
    task_dir_impl_readonly!();

    // Revalidate direct children (`stat`, `fd`, `attr`, `task`, ...) against task liveness and
    // credential changes. `DirEntry` creation prefers these parent ops over a child node's own
    // `create_dir_entry_ops` (including `TaskListDirectory` for `/proc/<pid>/task`), which is safe
    // because `/proc/<pid>`'s `check_access` already enforces `hidepid`.
    fn create_child_dir_entry_ops(&self) -> Option<Box<dyn DirEntryOps>> {
        Some(Box::new(TaskChildDirEntryOps { target: self.target.clone() }))
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
        let task = self.target.get_task()?;
        let target = self.target.clone();
        let creds = task.real_creds().euid_as_fscred();
        let fs = entry.node.fs();
        let (mode, ino) = task_entries(self.target.scope)
            .iter()
            .enumerate()
            .find_map(|(index, (n, mode))| {
                if name == *n {
                    Some((*mode, self.inode_range.start + index as ino_t))
                } else {
                    None
                }
            })
            .ok_or_else(|| errno!(ENOENT))?;

        // NOTE: keep entries in sync with `task_entries()`.
        let ops: Box<dyn FsNodeOps> = match &**name {
            b"cgroup" => Box::new(CgroupFile::new_node(target)),
            b"cwd" => Box::new(CallbackSymlinkNode::new(move || {
                Ok(SymlinkTarget::Node(target.get_task()?.fs()?.cwd()))
            })),
            b"exe" => Box::new(CallbackSymlinkNode::new(move || {
                let task = target.get_task()?;
                if let Some(node) = task.mm().ok().and_then(|mm| mm.executable_node()) {
                    Ok(SymlinkTarget::Node(node))
                } else {
                    error!(ENOENT)
                }
            })),
            b"fd" => Box::new(FdDirectory::new(target)),
            b"fdinfo" => Box::new(FdInfoDirectory::new(target)),
            b"io" => Box::new(IoFile::new_node()),
            b"limits" => Box::new(LimitsFile::new_node(target)),
            b"maps" => {
                let target_for_file = target.clone();
                Box::new(PtraceCheckedNode::new_node(
                    target,
                    PTRACE_MODE_READ_FSCREDS,
                    move |task| Ok(ProcMapsFile::new(task, target_for_file.clone())),
                ))
            }
            b"mem" => Box::new(MemFile::new_node(target)),
            b"root" => Box::new(CallbackSymlinkNode::new(move || {
                Ok(SymlinkTarget::Node(target.get_task()?.fs()?.root()))
            })),
            b"sched" => Box::new(StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/322893980"))),
            b"schedstat" => {
                Box::new(StubEmptyFile::new_node(bug_ref!("https://fxbug.dev/322894256")))
            }
            b"smaps" => {
                let target_for_file = target.clone();
                Box::new(PtraceCheckedNode::new_node(
                    target,
                    PTRACE_MODE_READ_FSCREDS,
                    move |task| Ok(ProcSmapsFile::new(task, target_for_file.clone())),
                ))
            }
            b"smaps_rollup" => {
                let target_for_file = target.clone();
                Box::new(PtraceCheckedNode::new_node(
                    target,
                    PTRACE_MODE_READ_FSCREDS,
                    move |task| Ok(ProcSmapsRollupFile::new(task, target_for_file.clone())),
                ))
            }
            b"stat" => Box::new(StatFile::new_node(target)),
            b"statm" => Box::new(StatmFile::new_node(target)),
            b"status" => Box::new(StatusFile::new_node(target)),
            b"cmdline" => Box::new(CmdlineFile::new_node(target)),
            b"environ" => Box::new(EnvironFile::new_node(target)),
            b"auxv" => Box::new(AuxvFile::new_node(target)),
            b"comm" => Box::new(CommFile::new_node(target)),
            b"attr" => Box::new(AttrDirectory::new(target, self.attr_inode_range.clone())),
            b"ns" => Box::new(NsDirectory::new(target)),
            b"mountinfo" => Box::new(ProcMountinfoFile::new_node(target)),
            b"mounts" => Box::new(ProcMountsFile::new_node(target)),
            b"oom_adj" => Box::new(OomAdjFile::new_node(target)),
            b"oom_score" => Box::new(OomScoreFile::new_node(target)),
            b"oom_score_adj" => Box::new(OomScoreAdjFile::new_node(target)),
            b"timerslack_ns" => Box::new(TimerslackNsFile::new_node(target)),
            b"wchan" => Box::new(BytesFile::new_node(b"0".to_vec())),
            b"clear_refs" => Box::new(ClearRefsFile::new_node(target)),
            b"pagemap" => Box::new(PtraceCheckedNode::new_node_with_current_task(
                target,
                PTRACE_MODE_READ_FSCREDS,
                |current_task, task| Ok(ProcPagemapFile::new(current_task, &task)),
            )),
            b"task" => Box::new(TaskListDirectory::new_node(target.pid)),
            name => unreachable!(
                "entry \"{:?}\" should be supported to keep in sync with task_entries()",
                name
            ),
        };

        Ok(fs.create_node(ino, ops, FsNodeInfo::new(mode, creds)))
    }
}

/// `TaskDirectory` doesn't implement the `close` method.
impl CloseFreeSafe for TaskDirectory {}
impl FileOps for TaskDirectory {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();

    fn readdir(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        let _task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        emit_dotdot(file, sink)?;

        // Skip through the entries until the current offset is reached.
        // Subtract 2 from the offset to account for `.` and `..`.
        for (index, (name, mode)) in
            task_entries(self.target.scope).iter().enumerate().skip(sink.offset() as usize - 2)
        {
            sink.add(
                self.inode_range.start + index as ino_t,
                sink.offset() + 1,
                DirectoryEntryType::from_mode(*mode),
                (*name).into(),
            )?;
        }
        Ok(())
    }

    fn as_pid(&self, _file: &FileObject) -> Result<Pid, Errno> {
        Ok(self.target.pid.clone())
    }
}

/// Creates an [`FsNode`] that represents the `/proc/<pid>` directory for `task`.
pub fn pid_directory(
    current_task: &CurrentTask,
    fs: &FileSystemHandle,
    task: &Arc<Task>,
    tid: Pid,
) -> FsNodeHandle {
    // proc(5): "The files inside each /proc/pid directory are normally
    // owned by the effective user and effective group ID of the process."
    let fs_node =
        TaskDirectory::new(fs, task, TaskContainer::from_thread_group(task.pid.clone(), tid));

    security::task_to_fs_node(current_task, task, &fs_node);
    fs_node
}

/// Creates an [`FsNode`] that represents the `/proc/<pid>/task/<tid>` directory for `task`.
fn tid_directory(
    current_task: &CurrentTask,
    fs: &FileSystemHandle,
    task: &Arc<Task>,
) -> FsNodeHandle {
    let fs_node = TaskDirectory::new(fs, task, TaskContainer::from_task(task));
    security::task_to_fs_node(current_task, task, &fs_node);
    fs_node
}

/// Derives the symlink's mode from the mode in which the file was opened.
///
/// Readable and/or writable file descriptors also set execute permission on the symlink
/// (0500 for O_RDONLY, 0300 for O_WRONLY, 0700 for O_RDWR, and 0000 for O_PATH).
fn fd_symlink_mode(flags: OpenFlags) -> FileMode {
    let mut access = Access::from_open_flags(flags);
    if access.is_nontrivial() {
        access |= Access::EXEC;
    }
    FileMode::IFLNK | access.user_mode()
}

/// Resolves the task, FD number, and open file for `fd_as_str` in `target`, returning `ENOENT` if
/// `fd_as_str` is not a valid FD number, the task has exited (including unreaped zombies whose
/// `FdTable` has been released), or the FD is no longer open.
fn resolve_open_fd(
    target: &TaskContainer,
    fd_as_str: &FsStr,
) -> Result<(Arc<Task>, FdNumber, FileHandle), Errno> {
    let fd = FdNumber::from_fs_str(fd_as_str).map_err(|_| errno!(ENOENT))?;
    let task = target.get_task().map_err(|_| errno!(ENOENT))?;
    let files = task.files().map_err(|_| errno!(ENOENT))?;
    let file = files.get_allowing_opath(fd).map_err(|_| errno!(ENOENT))?;
    Ok((task, fd, file))
}

/// Resolves the task, FD number, and open file behind a `/proc/<pid>/{fd,fdinfo}/<fd>` `DirEntry`.
///
/// Returns `None` if the task has exited or the FD is no longer open, so that the caller evicts
/// the entry.
fn resolve_open_fd_entry(
    target: &TaskContainer,
    dir_entry: &DirEntry,
) -> Option<(Arc<Task>, FdNumber, FileHandle)> {
    resolve_open_fd(target, dir_entry.local_name(&RcuReadScope::new())).ok()
}

/// Revalidates `/proc/<pid>/fd/<fd>` symlinks against task and FD liveness, refreshing `uid`/`gid`
/// and updating the symlink mode when the FD number now refers to a file opened with a different
/// access mode (e.g. after `dup2()`, or `close()` and reopen).
struct FdDirEntryOps {
    target: TaskContainer,
}

impl DirEntryOps for FdDirEntryOps {
    fn revalidate(&self, _current_task: &CurrentTask, dir_entry: &DirEntry) -> Result<bool, Errno> {
        let Some((task, fd, file)) = resolve_open_fd_entry(&self.target, dir_entry) else {
            return Ok(false);
        };
        let need_update = {
            let creds = task.real_creds().euid_as_fscred();
            let mode = fd_symlink_mode(file.flags());
            let info = dir_entry.node.info();
            info.uid != creds.uid || info.gid != creds.gid || info.mode != mode
        };
        if !need_update {
            return Ok(true);
        }
        let Ok(running_state) = task.running_state() else {
            return Ok(false);
        };
        let current_file = {
            // Hold `running_state.files` across `update_info` so `unshare(CLONE_FILES)` cannot swap
            // out the task's `FdTable` mid-update.
            let files_guard = running_state.files.lock();
            files_guard.as_ref().and_then(|files| {
                dir_entry.node.update_info(|info| {
                    // Re-read the FD and credentials so a stalled refresh cannot overwrite a newer
                    // mode (e.g. after `dup2()`) or owner with a stale one.
                    let current_file = files.table.get_allowing_opath(fd).ok()?;
                    let creds = task.real_creds().euid_as_fscred();
                    info.uid = creds.uid;
                    info.gid = creds.gid;
                    info.mode = fd_symlink_mode(current_file.flags());
                    Some(current_file)
                })
            })
        };
        Ok(current_file.is_some())
    }
}

/// `FdDirectory` implements the directory listing operations for a `proc/<pid>/fd` directory.
///
/// Reading the directory returns a list of all the currently open file descriptors for the
/// associated task.
struct FdDirectory {
    target: TaskContainer,
}

impl FdDirectory {
    fn new(target: TaskContainer) -> Self {
        Self { target }
    }
}

impl FsNodeOps for FdDirectory {
    fs_node_impl_dir_readonly!();

    fn create_child_dir_entry_ops(&self) -> Option<Box<dyn DirEntryOps>> {
        Some(Box::new(FdDirEntryOps { target: self.target.clone() }))
    }

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let _task = self.target.get_task()?;
        Ok(Box::new(FdDirectoryFileOps {
            target: self.target.clone(),
            entry_type: DirectoryEntryType::LNK,
        }))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        _current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        // Make sure that the task is alive and the file descriptor exists before creating the node.
        let (task, fd, file) = resolve_open_fd(&self.target, name)?;
        let mode = fd_symlink_mode(file.flags());
        let target = self.target.clone();
        Ok(entry.node.fs().create_node_and_allocate_node_id(
            CallbackSymlinkNode::new(move || {
                // On Linux, reading a held `fd/<n>` symlink fails with `EACCES` once the task is
                // reaped, and `ENOENT` on an unreaped zombie or closed descriptor.
                let task = target.get_task().map_err(|_| errno!(EACCES))?;
                let files = task.files().map_err(|_| errno!(ENOENT))?;
                let file = files.get_allowing_opath(fd).map_err(|_| errno!(ENOENT))?;
                Ok(SymlinkTarget::Node(file.name.to_passive()))
            }),
            FsNodeInfo::new(mode, task.real_creds().euid_as_fscred()),
        ))
    }
}

/// The entries in `/proc/<pid>/ns`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum NsType {
    Cgroup,
    Ipc,
    Mnt,
    Net,
    Pid,
    PidForChildren,
    Time,
    TimeForChildren,
    User,
    Uts,
}

impl NsType {
    /// All entries, in `/proc/<pid>/ns` listing order.
    const ALL: [NsType; 10] = [
        NsType::Cgroup,
        NsType::Ipc,
        NsType::Mnt,
        NsType::Net,
        NsType::Pid,
        NsType::PidForChildren,
        NsType::Time,
        NsType::TimeForChildren,
        NsType::User,
        NsType::Uts,
    ];

    /// Returns the entry's name in `/proc/<pid>/ns`.
    fn name(self) -> &'static str {
        match self {
            NsType::Cgroup => "cgroup",
            NsType::Ipc => "ipc",
            NsType::Mnt => "mnt",
            NsType::Net => "net",
            NsType::Pid => "pid",
            NsType::PidForChildren => "pid_for_children",
            NsType::Time => "time",
            NsType::TimeForChildren => "time_for_children",
            NsType::User => "user",
            NsType::Uts => "uts",
        }
    }

    fn from_name(name: &str) -> Option<Self> {
        match name {
            "cgroup" => Some(NsType::Cgroup),
            "ipc" => Some(NsType::Ipc),
            "mnt" => Some(NsType::Mnt),
            "net" => Some(NsType::Net),
            "pid" => Some(NsType::Pid),
            "pid_for_children" => Some(NsType::PidForChildren),
            "time" => Some(NsType::Time),
            "time_for_children" => Some(NsType::TimeForChildren),
            "user" => Some(NsType::User),
            "uts" => Some(NsType::Uts),
            _ => None,
        }
    }

    /// Returns the type of namespace the entry's link refers to: `pid_for_children` and
    /// `time_for_children` refer to `pid` and `time` namespaces, so their link targets use the
    /// `pid:` and `time:` prefixes.
    fn link_type(self) -> Self {
        match self {
            NsType::PidForChildren => NsType::Pid,
            NsType::TimeForChildren => NsType::Time,
            ns_type => ns_type,
        }
    }

    /// Returns the ID of `task`'s namespace of this type, or `ENOENT` if `task` has exited
    /// (including zombies).
    ///
    /// Namespace types without per-namespace IDs in Starnix use fixed placeholder IDs from the
    /// `0xEFFF_FFF9..=0xEFFF_FFFF` range that Linux uses for initial namespaces, which does not
    /// collide with `Namespace::id` (counting up from 1).
    /// TODO(https://fxbug.dev/297313673): Report real per-namespace `nsfs` inode IDs.
    fn namespace_id(self, task: &Task) -> Result<u64, Errno> {
        let fs = task.fs().map_err(|_| errno!(ENOENT))?;
        Ok(match self {
            NsType::Mnt => fs.namespace().id,
            NsType::Net => 0xEFFF_FFF9,
            NsType::Time | NsType::TimeForChildren => 0xEFFF_FFFA,
            NsType::Cgroup => 0xEFFF_FFFB,
            NsType::Pid | NsType::PidForChildren => 0xEFFF_FFFC,
            NsType::User => 0xEFFF_FFFD,
            NsType::Uts => 0xEFFF_FFFE,
            NsType::Ipc => 0xEFFF_FFFF,
        })
    }
}

/// Parses a `/proc/<pid>/ns` entry name: either `<ns>` (the link) or `<ns>:[<id>]` (the link's
/// target). Returns `None` if `name` is neither.
fn parse_ns_entry_name(name: &[u8]) -> Option<(NsType, Option<u64>)> {
    let name = std::str::from_utf8(name).ok()?;
    let Some((ns, target)) = name.split_once(':') else {
        return Some((NsType::from_name(name)?, None));
    };
    let ns_type = NsType::from_name(ns)?;
    // Only the form that links actually point to is accepted: the `<ns>` of a target is never
    // `pid_for_children` or `time_for_children`.
    if ns_type.link_type() != ns_type {
        return None;
    }
    let digits = target.strip_prefix('[')?.strip_suffix(']')?;
    // Reject non-canonical forms like `+1` or `01` so they cannot create duplicate cached entries.
    if digits.starts_with(['+', '0']) {
        return None;
    }
    let id = digits.parse().ok()?;
    Some((ns_type, Some(id)))
}

/// Entries exposed in `/proc/<pid>/attr` and `/proc/<pid>/task/<tid>/attr` with their modes, in
/// the order that readdir lists them, which matches Linux.
const ATTR_ENTRIES: &[(&str, security::ProcAttr, FileMode)] = &[
    ("current", security::ProcAttr::Current, mode!(IFREG, 0o666)),
    ("prev", security::ProcAttr::Previous, mode!(IFREG, 0o444)),
    ("exec", security::ProcAttr::Exec, mode!(IFREG, 0o666)),
    ("fscreate", security::ProcAttr::FsCreate, mode!(IFREG, 0o666)),
    ("keycreate", security::ProcAttr::KeyCreate, mode!(IFREG, 0o666)),
    ("sockcreate", security::ProcAttr::SockCreate, mode!(IFREG, 0o666)),
];

/// Represents the `/proc/<pid>/attr` or `/proc/<pid>/task/<tid>/attr` directory.
///
/// This directory lazily creates its child entries on lookup. Each entry is owned by the task's
/// effective uid/gid at the time it is looked up.
///
/// The inode numbers of the entries come from `inode_range`, which is reserved by the parent
/// `TaskDirectory`. They therefore stay the same across instances of this directory for as long as
/// that `TaskDirectory` lives, like the inode numbers of the other `/proc/<pid>` entries.
#[derive(Clone)]
struct AttrDirectory {
    target: TaskContainer,
    inode_range: Range<ino_t>,
}

impl AttrDirectory {
    fn new(target: TaskContainer, inode_range: Range<ino_t>) -> Self {
        Self { target, inode_range }
    }
}

impl FsNodeOps for AttrDirectory {
    fs_node_impl_dir_readonly!();

    fn create_child_dir_entry_ops(&self) -> Option<Box<dyn DirEntryOps>> {
        Some(Box::new(TaskChildDirEntryOps { target: self.target.clone() }))
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
        let (index, &(_, attr, mode)) = ATTR_ENTRIES
            .iter()
            .enumerate()
            .find(|&(_, &(entry_name, _, _))| name == entry_name)
            .ok_or_else(|| errno!(ENOENT))?;
        // Entries of a task that no longer exists don't exist either.
        let task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        let ino = self.inode_range.start + index as ino_t;
        Ok(entry.node.fs().create_node(
            ino,
            AttrNode::new(self.target.clone(), attr),
            FsNodeInfo::new(mode, task.real_creds().euid_as_fscred()),
        ))
    }
}

/// `AttrDirectory` doesn't implement the `close` method.
impl CloseFreeSafe for AttrDirectory {}
impl FileOps for AttrDirectory {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();

    fn readdir(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        // Like Linux, listing the directory of a task that no longer exists fails.
        let _task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        emit_dotdot(file, sink)?;

        // Skip through the entries until the current offset is reached.
        // Subtract 2 from the offset to account for `.` and `..`.
        for (index, &(name, _, mode)) in
            ATTR_ENTRIES.iter().enumerate().skip(sink.offset() as usize - 2)
        {
            sink.add(
                self.inode_range.start + index as ino_t,
                sink.offset() + 1,
                DirectoryEntryType::from_mode(mode),
                name.into(),
            )?;
        }
        Ok(())
    }
}

/// /proc/<pid>/attr directory entry.
struct AttrNode {
    attr: security::ProcAttr,
    target: TaskContainer,
}

impl AttrNode {
    fn new(target: TaskContainer, attr: security::ProcAttr) -> impl FsNodeOps {
        SimpleFileNode::new(move |_| Ok(AttrNode { attr, target: target.clone() }))
    }
}

impl FileOps for AttrNode {
    fileops_impl_seekable!();
    fileops_impl_noop_sync!();

    fn writes_update_seek_offset(&self) -> bool {
        false
    }

    fn read(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        let task = self.target.get_task()?;
        let response = security::get_procattr(current_task, &task, self.attr)?;
        data.write_tail(&response, offset)
    }

    fn write(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let task = self.target.get_task()?;

        // If the current task is not the target then writes are not allowed.
        if current_task.task != task {
            return error!(EPERM);
        }
        if offset != 0 {
            return error!(EINVAL);
        }

        let data = data.read_all()?;
        let data_len = data.len();
        security::set_procattr(current_task, self.attr, data.as_slice())?;
        Ok(data_len)
    }
}

/// Checks `PTRACE_MODE_READ_FSCREDS` access to `task` for `/proc/<pid>/ns` operations.
///
/// Unlike `PtraceCheckedNode` (which exempts kernel threads without an `mm`), `/proc/<pid>/ns`
/// enforces ptrace read access unconditionally so unprivileged callers cannot read a kernel
/// thread's namespace IDs or follow its namespace links.
fn check_ns_ptrace_access(current_task: &CurrentTask, task: &Task) -> Result<(), Errno> {
    current_task
        .check_ptrace_access_mode(PTRACE_MODE_READ_FSCREDS, task)
        .map_err(|_| errno!(EACCES))
}

/// A node representing a mount namespace in `/proc/<pid>/ns/mnt:[<id>]`.
///
/// Holds `Weak<Namespace>` so `FsNode` handles do not retain a `Namespace` or create reference
/// cycles with it (with full `nsfs` bind-mount persistence tracked under
/// `https://fxbug.dev/297313673`). Opening the node fails with `ENOENT` once the namespace is
/// gone.
///
/// Like an `nsfs` file on Linux, opening the node does not check ptrace access again: access is
/// checked when the `/proc/<pid>/ns` link is read or followed, so a node that was already
/// resolved (e.g. held by an `O_PATH` descriptor) can be reopened after the caller drops
/// privileges or the target task exits.
struct MountNamespaceNode {
    mount_namespace: Weak<Namespace>,
}

impl FsNodeOps for MountNamespaceNode {
    fs_node_impl_not_dir!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let mount_namespace = self.mount_namespace.upgrade().ok_or_else(|| errno!(ENOENT))?;
        Ok(Box::new(MountNamespaceFile(mount_namespace)))
    }
}

/// A `/proc/<pid>/ns/<name>` symlink node.
///
/// Lazily resolves `<ns_type>:[<id>]` against `target` on `readlink`, failing with `EACCES`
/// without `PTRACE_MODE_READ_FSCREDS` access and `ENOENT` once the target task has exited.
struct NsSymlinkNode {
    target: TaskContainer,
    ns_type: NsType,
}

impl FsNodeOps for NsSymlinkNode {
    fs_node_impl_symlink!();

    fn readlink(&self, _node: &FsNode, current_task: &CurrentTask) -> Result<SymlinkTarget, Errno> {
        let task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        check_ns_ptrace_access(current_task, &task)?;
        let link_type = self.ns_type.link_type();
        let id = link_type.namespace_id(&task)?;
        Ok(SymlinkTarget::Path(format!("{}:[{id}]", link_type.name()).into()))
    }
}

/// Revalidates `/proc/<pid>/ns` entries against task liveness. Link entries also refresh
/// `uid`/`gid`; `<ns>:[<id>]` entries instead check ptrace access and namespace ID changes on
/// unshare or setns.
struct NsDirEntryOps {
    target: TaskContainer,
}

impl DirEntryOps for NsDirEntryOps {
    fn revalidate(&self, current_task: &CurrentTask, dir_entry: &DirEntry) -> Result<bool, Errno> {
        let Ok(task) = self.target.get_task() else {
            return Ok(false);
        };
        // Bind outside the `match` scrutinee so `RcuReadScope` is dropped before the arms take
        // locks or drop `Arc<Namespace>`.
        let parsed = parse_ns_entry_name(dir_entry.local_name(&RcuReadScope::new()));
        match parsed {
            None => Ok(false),
            Some((_ns_type, None)) => {
                // Only the links follow the task's owner; `<ns>:[<id>]` entries stay root-owned.
                update_task_node_info(&dir_entry.node, &task);
                Ok(true)
            }
            Some((ns_type, Some(id))) => {
                // Linux requires ptrace access to read or follow a `/proc/<pid>/ns` link but not
                // to look up the link itself (e.g. `lstat`). Starnix follows the link by looking
                // up its `<ns>:[<id>]` target in this directory, so such entries require the same
                // access.
                check_ns_ptrace_access(current_task, &task)?;
                Ok(ns_type.namespace_id(&task).is_ok_and(|current_id| current_id == id))
            }
        }
    }
}

/// /proc/[pid]/ns directory
struct NsDirectory {
    target: TaskContainer,
}

impl NsDirectory {
    fn new(target: TaskContainer) -> Self {
        Self { target }
    }
}

impl FsNodeOps for NsDirectory {
    fs_node_impl_dir_readonly!();

    fn create_child_dir_entry_ops(&self) -> Option<Box<dyn DirEntryOps>> {
        Some(Box::new(NsDirEntryOps { target: self.target.clone() }))
    }

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        // For each namespace, this contains a link to the current identifier of the given namespace
        // for the target task.
        Ok(VecDirectory::new_file(
            NsType::ALL
                .into_iter()
                .map(|ns_type| VecDirectoryEntry {
                    entry_type: DirectoryEntryType::LNK,
                    name: FsString::from(ns_type.name()),
                    inode: None,
                })
                .collect(),
        ))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        // If name is a given namespace, link to the current identifier of that namespace for
        // the target task.
        // If name is {namespace}:[id], get a file descriptor for the given namespace.
        let task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        let (ns_type, id) = parse_ns_entry_name(name).ok_or_else(|| errno!(ENOENT))?;
        let Some(id) = id else {
            return Ok(entry.node.fs().create_node_and_allocate_node_id(
                NsSymlinkNode { target: self.target.clone(), ns_type },
                FsNodeInfo::new(mode!(IFLNK, 0o777), task.real_creds().euid_as_fscred()),
            ));
        };

        // See `NsDirEntryOps::revalidate`.
        check_ns_ptrace_access(current_task, &task)?;
        // Unlike the `/proc/<pid>/ns/<name>` link, the namespace file it resolves to is owned by
        // root, whichever task it was reached through, as on Linux.
        let node_info = FsNodeInfo::new(mode!(IFREG, 0o444), FsCred::root());
        // The ID must be the target's current one, so a stale name (e.g. after the target
        // unshares) is not resolved and cached as if it were current.
        match ns_type {
            NsType::Mnt => {
                // TODO(https://fxbug.dev/297313673): Implement `nsfs`.
                let mount_namespace = task.fs().map_err(|_| errno!(ENOENT))?.namespace();
                if mount_namespace.id != id {
                    return error!(ENOENT);
                }
                Ok(entry.node.fs().create_node_and_allocate_node_id(
                    MountNamespaceNode { mount_namespace: Arc::downgrade(&mount_namespace) },
                    node_info,
                ))
            }
            _ => {
                if ns_type.namespace_id(&task)? != id {
                    return error!(ENOENT);
                }
                track_stub!(TODO("https://fxbug.dev/297313673"), ns_type.name());
                Ok(entry
                    .node
                    .fs()
                    .create_node_and_allocate_node_id(BytesFile::new_node(vec![]), node_info))
            }
        }
    }
}

/// Revalidates `/proc/<pid>/fdinfo/<fd>` regular files against task and FD liveness,
/// refreshing `uid`/`gid`.
struct FdInfoDirEntryOps {
    target: TaskContainer,
}

impl DirEntryOps for FdInfoDirEntryOps {
    fn revalidate(&self, _current_task: &CurrentTask, dir_entry: &DirEntry) -> Result<bool, Errno> {
        let Some((task, _fd, _file)) = resolve_open_fd_entry(&self.target, dir_entry) else {
            return Ok(false);
        };
        update_task_node_info(&dir_entry.node, &task);
        Ok(true)
    }
}

/// `FdInfoDirectory` implements the directory listing operations for a `proc/<pid>/fdinfo`
/// directory.
///
/// Reading the directory returns a list of all the currently open file descriptors for the
/// associated task.
struct FdInfoDirectory {
    target: TaskContainer,
}

impl FdInfoDirectory {
    fn new(target: TaskContainer) -> Self {
        Self { target }
    }
}

impl FsNodeOps for FdInfoDirectory {
    fs_node_impl_dir_readonly!();

    fn create_child_dir_entry_ops(&self) -> Option<Box<dyn DirEntryOps>> {
        Some(Box::new(FdInfoDirEntryOps { target: self.target.clone() }))
    }

    fn create_file_ops(
        &self,
        _node: &FsNode,
        current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let task = self.target.get_task()?;
        current_task
            .check_ptrace_access_mode(PTRACE_MODE_READ_FSCREDS, &task)
            .map_err(|_| errno!(EACCES))?;

        Ok(Box::new(FdDirectoryFileOps {
            target: self.target.clone(),
            entry_type: DirectoryEntryType::REG,
        }))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        _current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        // Verify that the FD exists at lookup time. We intentionally do not store the
        // `FileHandle` in `FdInfoFile` to avoid pinning the file in memory after the FD is
        // closed, and to reflect live `FD_CLOEXEC` flags and closed/reassigned FDs on read.
        let (task, fd, _file) = resolve_open_fd(&self.target, name)?;
        Ok(entry.node.fs().create_node_and_allocate_node_id(
            FdInfoFile::new_node(self.target.clone(), fd),
            FsNodeInfo::new(mode!(IFREG, 0o444), task.real_creds().euid_as_fscred()),
        ))
    }
}

#[derive(Clone)]
struct FdInfoFile {
    target: TaskContainer,
    fd: FdNumber,
}

impl FdInfoFile {
    fn new_node(target: TaskContainer, fd: FdNumber) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target, fd })
    }
}

impl DynamicFileSource for FdInfoFile {
    fn generate(&self, current_task: &CurrentTask, sink: &mut DynamicFileBuf) -> Result<(), Errno> {
        let task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        let (file, fd_flags) = task
            .files()
            .map_err(|_| errno!(ENOENT))?
            .get_allowing_opath_with_flags(self.fd)
            .map_err(|_| errno!(ENOENT))?;
        let pos = file.offset.read();
        let mut flags = file.flags();
        flags.set(OpenFlags::CLOEXEC, fd_flags.contains(FdFlags::CLOEXEC));
        writeln!(sink, "pos:\t{}\nflags:\t0{:o}", pos, flags.bits())?;
        if let Some(extra_fdinfo) = file.extra_fdinfo(current_task) {
            sink.write(extra_fdinfo.as_slice());
        }
        Ok(())
    }
}

const FD_READDIR_BATCH_SIZE: usize = 64;

struct FdDirectoryFileOps {
    target: TaskContainer,
    entry_type: DirectoryEntryType,
}

impl FileOps for FdDirectoryFileOps {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();

    fn readdir(
        &self,
        file: &FileObject,
        _current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        let task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        emit_dotdot(file, sink)?;

        let Ok(files) = task.files() else {
            return Ok(());
        };
        let mut next_fd = usize::try_from(sink.offset().saturating_sub(2)).unwrap_or(usize::MAX);
        let mut buf = itoa::Buffer::new();
        let mut batch = [FdNumber::from_raw(0); FD_READDIR_BATCH_SIZE];
        loop {
            // Collect a batch of descriptor numbers while `fill_fd_batch` holds its `RcuReadScope`
            // so `sink.add` (which writes to user memory) runs outside the RCU read section.
            let count = files.fill_fd_batch(next_fd, &mut batch);
            if count == 0 {
                break;
            }
            for &fd in &batch[..count] {
                let raw = fd.raw();
                let name = buf.format(raw);
                sink.add(
                    file.fs.allocate_ino(),
                    raw as off_t + 3,
                    self.entry_type,
                    name.as_bytes().into(),
                )?;
            }
            if count < batch.len() {
                break;
            }
            next_fd = batch[count - 1].raw() as usize + 1;
        }
        Ok(())
    }
}

/// Directory that lists the task IDs (tid) in a process. Located at `/proc/<pid>/task/`.
struct TaskListDirectory {
    target: TaskContainer,
}

struct TaskListFile {
    target: TaskContainer,
    inner: Box<dyn FileOps>,
}

impl FileOps for TaskListFile {
    fileops_impl_directory!();
    fileops_impl_noop_sync!();
    fileops_impl_unbounded_seek!();

    fn readdir(
        &self,
        file: &FileObject,
        current_task: &CurrentTask,
        sink: &mut dyn DirentSink,
    ) -> Result<(), Errno> {
        let _task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        self.inner.readdir(file, current_task, sink)
    }
}

impl TaskListDirectory {
    fn new_node(pid: Pid) -> impl FsNodeOps {
        Self { target: TaskContainer::from_pid(pid) }
    }
}

impl FsNodeOps for TaskListDirectory {
    task_dir_impl_readonly!();

    fn create_file_ops(
        &self,
        _node: &FsNode,
        _current_task: &CurrentTask,
        _flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        let task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        let mut buf = itoa::Buffer::new();
        let inner = VecDirectory::new_file(
            task.thread_group()
                .read()
                .task_ids()
                .map(|tid| VecDirectoryEntry {
                    entry_type: DirectoryEntryType::DIR,
                    name: buf.format(tid).as_bytes().into(),
                    inode: None,
                })
                .collect(),
        );
        Ok(Box::new(TaskListFile { target: self.target.clone(), inner }))
    }

    fn lookup(
        &self,
        entry: &DirEntry,
        current_task: &CurrentTask,
        name: &FsStr,
    ) -> Result<FsNodeHandle, Errno> {
        let target_task = self.target.get_task().map_err(|_| errno!(ENOENT))?;
        let tid = std::str::from_utf8(name)
            .map_err(|_| errno!(ENOENT))?
            .parse::<pid_t>()
            .map_err(|_| errno!(ENOENT))?;

        let task = current_task.get_task(tid).map_err(|_| errno!(ENOENT))?;
        // Make sure the tid belongs to this process.
        if task.pid != target_task.thread_group().leader {
            return error!(ENOENT);
        }
        if let Some(proc_fs) = entry.node.fs().downcast_ops::<ProcFs>() {
            if !proc_fs.has_pid_permissions(current_task, &task, HidePid::Invisible) {
                return error!(ENOENT);
            }
        }

        Ok(tid_directory(current_task, &entry.node.fs(), &task))
    }
}

#[derive(Clone)]
struct CgroupFile {
    target: TaskContainer,
}
impl CgroupFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}
impl DynamicFileSource for CgroupFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let task = self.target.get_task()?;
        let cgroup1 = task.kernel().cgroups.cgroup1.lock();
        for (key, root) in &cgroup1.hierarchies {
            let mut parts: Vec<&str> = key.controllers.iter().map(|c| c.as_str()).collect();
            let name_storage;
            if let Some(name) = &key.name {
                name_storage = format!("name={}", name);
                parts.push(&name_storage);
            }
            let controller_str = parts.join(",");
            let cgroup = root.get_cgroup(&task.pid);
            let path = path_from_root(cgroup)?;
            sink.write(format!("{}:{}:{}\n", root.hierarchy_id, controller_str, path).as_bytes());
        }
        let cgroup = task.kernel().cgroups.cgroup2.get_cgroup(&task.pid);
        let path = path_from_root(cgroup)?;
        sink.write(format!("0::{}\n", path).as_bytes());
        Ok(())
    }
}

fn fill_buf_from_addr_range(
    task: &Task,
    range_start: UserAddress,
    range_end: UserAddress,
    sink: &mut DynamicFileBuf,
) -> Result<(), Errno> {
    #[allow(clippy::manual_saturating_arithmetic)]
    let len = range_end.ptr().checked_sub(range_start.ptr()).unwrap_or(0);
    // NB: If this is exercised in a hot-path, we can plumb the reading task
    // (`CurrentTask`) here to perform a copy without going through the VMO when
    // unified aspaces is enabled.
    let buf = task.read_memory_partial_to_vec(range_start, len)?;
    sink.write(&buf[..]);
    Ok(())
}

/// `CmdlineFile` implements `proc/<pid>/cmdline` file.
#[derive(Clone)]
pub struct CmdlineFile {
    target: TaskContainer,
}
impl CmdlineFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}
impl DynamicFileSource for CmdlineFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        // Opened cmdline file should still be functional once the task is a zombie.
        let Ok(task) = self.target.get_task() else {
            return Ok(());
        };
        // /proc/<pid>/cmdline is empty for kthreads.
        let Ok(mm) = task.mm() else {
            return Ok(());
        };
        let (start, end) = {
            let mm_state = mm.state.read();
            (mm_state.argv_start, mm_state.argv_end)
        };
        fill_buf_from_addr_range(&task, start, end, sink)
    }
}

struct PtraceCheckedNode {}

impl PtraceCheckedNode {
    pub fn new_node<F, O>(
        target: TaskContainer,
        mode: PtraceAccessMode,
        create_ops: F,
    ) -> impl FsNodeOps
    where
        F: Fn(Arc<Task>) -> Result<O, Errno> + Send + Sync + 'static,
        O: FileOps,
    {
        Self::new_node_with_current_task(target, mode, move |_current_task, task| create_ops(task))
    }

    pub fn new_node_with_current_task<F, O>(
        target: TaskContainer,
        mode: PtraceAccessMode,
        create_ops: F,
    ) -> impl FsNodeOps
    where
        F: Fn(&CurrentTask, Arc<Task>) -> Result<O, Errno> + Send + Sync + 'static,
        O: FileOps,
    {
        SimpleFileNode::new(move |current_task: &CurrentTask| {
            let task = target.get_task()?;
            // proc-pid nodes for kthreads do not require ptrace access checks.
            if task.mm().is_ok() || !task.is_running() {
                current_task.check_ptrace_access_mode(mode, &task).map_err(|_| errno!(EACCES))?;
            }
            create_ops(current_task, task)
        })
    }
}

/// `EnvironFile` implements `proc/<pid>/environ` file.
#[derive(Clone)]
pub struct EnvironFile {
    target: TaskContainer,
}
impl EnvironFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        let target_for_file = target.clone();
        PtraceCheckedNode::new_node(target, PTRACE_MODE_READ_FSCREDS, move |_| {
            Ok(DynamicFile::new(Self { target: target_for_file.clone() }))
        })
    }
}
impl DynamicFileSource for EnvironFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let task = self.target.get_task()?;
        // /proc/<pid>/environ is empty for kthreads.
        let Ok(mm) = task.mm() else {
            return Ok(());
        };
        let (start, end) = {
            let mm_state = mm.state.read();
            (mm_state.environ_start, mm_state.environ_end)
        };
        fill_buf_from_addr_range(&task, start, end, sink)
    }
}

/// `AuxvFile` implements `proc/<pid>/auxv` file.
#[derive(Clone)]
pub struct AuxvFile {
    target: TaskContainer,
}
impl AuxvFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        let target_for_file = target.clone();
        PtraceCheckedNode::new_node(target, PTRACE_MODE_READ_FSCREDS, move |_| {
            Ok(DynamicFile::new(Self { target: target_for_file.clone() }))
        })
    }
}
impl DynamicFileSource for AuxvFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let task = self.target.get_task()?;
        // /proc/<pid>/auxv is empty for kthreads.
        let Ok(mm) = task.mm() else {
            return Ok(());
        };
        let (start, end) = {
            let mm_state = mm.state.read();
            (mm_state.auxv_start, mm_state.auxv_end)
        };
        fill_buf_from_addr_range(&task, start, end, sink)
    }
}

/// `CommFile` implements `proc/<pid>/comm` file.
#[derive(Clone)]
pub struct CommFile {
    target: TaskContainer,
}
impl CommFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}

impl DynamicFileSource for CommFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let task = self.target.get_task()?;
        sink.write(task.persistent_info.command_guard().comm_name());
        sink.write(b"\n");
        Ok(())
    }

    fn write(
        &self,
        current_task: &CurrentTask,
        _offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let task = self.target.get_task()?;
        if !Arc::ptr_eq(&task.thread_group(), &current_task.thread_group()) {
            return error!(EINVAL);
        }
        // What happens if userspace writes to this file in multiple syscalls? We need more
        // detailed tests to see when the data is actually committed back to the task.
        let bytes = data.read_all()?;
        task.set_command_name(task.command_from_comm(&bytes));
        Ok(bytes.len())
    }
}

/// `IoFile` implements `proc/<pid>/io` file.
#[derive(Clone)]
pub struct IoFile {}
impl IoFile {
    pub fn new_node() -> impl FsNodeOps {
        DynamicFile::new_node(Self {})
    }
}
impl DynamicFileSource for IoFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        track_stub!(TODO("https://fxbug.dev/322874250"), "/proc/pid/io");
        sink.write(b"rchar: 0\n");
        sink.write(b"wchar: 0\n");
        sink.write(b"syscr: 0\n");
        sink.write(b"syscw: 0\n");
        sink.write(b"read_bytes: 0\n");
        sink.write(b"write_bytes: 0\n");
        sink.write(b"cancelled_write_bytes: 0\n");
        Ok(())
    }
}

/// `LimitsFile` implements `proc/<pid>/limits` file.
#[derive(Clone)]
pub struct LimitsFile {
    target: TaskContainer,
}
impl LimitsFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}
impl DynamicFileSource for LimitsFile {
    fn generate(
        &self,
        _current_task: &CurrentTask,
        sink: &mut DynamicFileBuf,
    ) -> Result<(), Errno> {
        let task = self.target.get_task()?;
        let limits = task.thread_group().limits.lock();

        let write_limit = |sink: &mut DynamicFileBuf, value| {
            if value == RLIM_INFINITY as u64 {
                sink.write(format!("{:<20}", "unlimited").as_bytes());
            } else {
                sink.write(format!("{:<20}", value).as_bytes());
            }
        };
        sink.write(
            format!("{:<25}{:<20}{:<20}{:<10}\n", "Limit", "Soft Limit", "Hard Limit", "Units")
                .as_bytes(),
        );
        for resource in Resource::ALL {
            let desc = resource.desc();
            let limit = limits.get(resource);
            sink.write(format!("{:<25}", desc.name).as_bytes());
            write_limit(sink, limit.rlim_cur);
            write_limit(sink, limit.rlim_max);
            if !desc.unit.is_empty() {
                sink.write(format!("{:<10}", desc.unit).as_bytes());
            }
            sink.write(b"\n");
        }
        Ok(())
    }
}

/// `MemFile` implements `proc/<pid>/mem` file.
pub struct MemFile {
    mm: Weak<MemoryManager>,

    // TODO: https://fxbug.dev/442459337 - Tear-down MemoryManager internals on process exit, to
    // avoid extension of the MM lifetime prolonging access to memory via "/proc/pid/mem", etc
    // beyond that of the actual process/address-space.
    target: TaskContainer,
}

impl MemFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        let target_for_file = target.clone();
        PtraceCheckedNode::new_node(target, PTRACE_MODE_ATTACH_FSCREDS, move |task| {
            let mm = task.mm().ok().as_ref().map(Arc::downgrade).unwrap_or_default();
            Ok(Self { mm, target: target_for_file.clone() })
        })
    }
}

impl FileOps for MemFile {
    fileops_impl_noop_sync!();

    fn is_seekable(&self) -> bool {
        true
    }

    fn seek(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        current_offset: off_t,
        target: SeekTarget,
    ) -> Result<off_t, Errno> {
        default_seek(current_offset, target, || error!(EINVAL))
    }

    fn read(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        let Ok(_task) = self.target.get_task() else {
            return Ok(0);
        };
        let Some(mm) = self.mm.upgrade() else {
            return Ok(0);
        };
        let mut addr = UserAddress::from(offset as u64);
        data.write_each(&mut |bytes| {
            let read_bytes = if current_task.has_same_address_space(Some(&mm)) {
                current_task.read_memory_partial(addr, bytes)
            } else {
                mm.syscall_read_memory_partial(addr, bytes)
            }
            .map_err(|_| errno!(EIO))?;
            let actual = read_bytes.len();
            addr = (addr + actual)?;
            Ok(actual)
        })
    }

    fn write(
        &self,
        _file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let Ok(_task) = self.target.get_task() else {
            return Ok(0);
        };
        let Some(mm) = self.mm.upgrade() else {
            return Ok(0);
        };
        let addr = UserAddress::from(offset as u64);
        let mut written = 0;
        let result = data.peek_each(&mut |bytes| {
            let actual = if current_task.has_same_address_space(Some(&mm)) {
                current_task.write_memory_partial((addr + written)?, bytes)
            } else {
                mm.syscall_write_memory_partial((addr + written)?, bytes)
            }
            .map_err(|_| errno!(EIO))?;
            written += actual;
            Ok(actual)
        });
        data.advance(written)?;
        result
    }
}

const STUBBED_MEM_BYTES: usize = 4096;

// Workaround for b/525059309: Zircon VMAR walks (`ZX_INFO_VMAR_MAPS`) are extremely
// slow and cause Perfetto's `traced` / `traced_probes` watchdog timeouts and `lmkd`
// `cmd_procs_prio` VMAR lock contention when sweeping thread status files. We bypass them
// when the reader is `traced`, `traced_probes`, or `lmkd`.
fn should_skip_memory_stats(current_task: &CurrentTask) -> bool {
    let comm = current_task.persistent_info.command_guard();
    let name = comm.comm_name();
    name == b"traced" || name == b"traced_probes" || name == b"lmkd"
}
fn stub_memory_stats() -> MemoryStats {
    MemoryStats {
        vm_size: STUBBED_MEM_BYTES,
        vm_rss: STUBBED_MEM_BYTES,
        vm_rss_hwm: STUBBED_MEM_BYTES,
        rss_anonymous: STUBBED_MEM_BYTES,
        rss_file: 0,
        rss_shared: 0,
        vm_data: 0,
        vm_stack: STUBBED_MEM_BYTES,
        vm_exe: STUBBED_MEM_BYTES,
        vm_swap: 0,
        vm_lck: 0,
        rss_locked: 0,
    }
}

#[derive(Clone)]
pub struct StatFile {
    target: TaskContainer,
}

impl StatFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}
impl DynamicFileSource for StatFile {
    fn generate(&self, current_task: &CurrentTask, sink: &mut DynamicFileBuf) -> Result<(), Errno> {
        let task = self.target.get_task()?;

        // All fields and their types as specified in the man page.
        // Unimplemented fields are set to 0 here.
        let pid: pid_t; // 1
        let comm: TaskCommand;
        let state: char;
        let ppid: pid_t;
        let pgrp: pid_t; // 5
        let session: pid_t;
        let tty_nr: i32;
        let tpgid: i32 = 0;
        let flags: u32 = 0;
        let minflt: u64 = 0; // 10
        let cminflt: u64 = 0;
        let majflt: u64 = 0;
        let cmajflt: u64 = 0;
        let utime: i64;
        let stime: i64; // 15
        let cutime: i64;
        let cstime: i64;
        let mut priority: i64;
        let nice: i64;
        let num_threads: i64; // 20
        let itrealvalue: i64 = 0;
        let mut starttime: u64 = 0;
        let mut vsize: usize = 0;
        let mut rss: usize = 0;
        let mut rsslim: u64 = 0; // 25
        let mut startcode: u64 = 0;
        let mut endcode: u64 = 0;
        let mut startstack: usize = 0;
        let mut kstkesp: u64 = 0;
        let mut kstkeip: u64 = 0; // 30
        let signal: u64 = 0;
        let blocked: u64 = 0;
        let siginore: u64 = 0;
        let sigcatch: u64 = 0;
        let mut wchan: u64 = 0; // 35
        let nswap: u64 = 0;
        let cnswap: u64 = 0;
        let exit_signal: i32 = 0;
        let processor: i32 = 0;
        let rt_priority: u32 = 0; // 40
        let policy: u32 = 0;
        let delayacct_blkio_ticks: u64 = 0;
        let guest_time: u64 = 0;
        let cguest_time: i64 = 0;
        let mut start_data: u64 = 0; // 45
        let mut end_data: u64 = 0;
        let mut start_brk: u64 = 0;
        let mut arg_start: usize = 0;
        let mut arg_end: usize = 0;
        let mut env_start: usize = 0; // 50
        let mut env_end: usize = 0;
        let mut exit_code: i32 = 0;

        pid = self.target.tid.id;
        comm = task.command();
        state = task.state_code().code_char();
        {
            let scheduler_state = task.scheduler_state.lock();
            priority = scheduler_state.task_priority();
            nice = scheduler_state.normal_priority().as_nice() as i64;
        }
        if let Some(boost) =
            task.kernel().shared_futexes.get_rt_mutex_priority_boost(task.get_tid() as u32)
        {
            priority = std::cmp::min(priority, boost);
        }

        {
            let thread_group = task.thread_group();
            let tg_state = thread_group.read();
            ppid = tg_state.get_ppid();
            pgrp = thread_group.pgid().id;
            session = tg_state.session.id;

            // TTY device ID.
            tty_nr = tg_state.controlling_terminal_device().map(|d| d.bits()).unwrap_or(0) as i32;

            cutime = duration_to_scheduler_clock(tg_state.children_time_stats.user_time);
            cstime = duration_to_scheduler_clock(tg_state.children_time_stats.system_time);

            num_threads = std::cmp::max(1, tg_state.tasks_count()) as i64;
        }

        let time_stats = match self.target.scope {
            TaskEntryScope::Task => task.time_stats(),
            TaskEntryScope::ThreadGroup => task.thread_group().time_stats(),
        };
        utime = duration_to_scheduler_clock(time_stats.user_time);
        stime = duration_to_scheduler_clock(time_stats.system_time);

        if !task.thread_group().process.is_invalid() {
            starttime = duration_to_scheduler_clock(
                task.thread_group().start_time - zx::MonotonicInstant::ZERO,
            ) as u64;
        }

        if let Ok(mm) = task.mm() {
            let page_size = *PAGE_SIZE as usize;
            {
                let mm_state = mm.state.read();
                startstack = mm_state.stack_start.ptr();
                arg_start = mm_state.argv_start.ptr();
                arg_end = mm_state.argv_end.ptr();
                env_start = mm_state.environ_start.ptr();
                env_end = mm_state.environ_end.ptr();
                let (vm_size, vm_rss) = if should_skip_memory_stats(current_task) {
                    mm.get_stat_vsize_and_rss(&mm_state)
                } else {
                    // Releases `mm_state` before walking the Zircon mappings.
                    let mem_stats = mm.get_stats_locked(
                        current_task,
                        mm_state,
                        SelfStatsCacheMode::AllowCached,
                    );
                    (mem_stats.vm_size, mem_stats.vm_rss)
                };
                vsize = vm_size;
                rss = vm_rss / page_size;
            }
            rsslim = task.thread_group().limits.lock().get(Resource::RSS).rlim_max;
        }

        // The man page describes that the following fields have "... values displayed as 0" if the
        // caller does not have ptrace read access to the target.
        // In practice the `startcode` and `endcode` fields appear to be displayed as 1.
        if !current_task
            .check_ptrace_access_mode(PTRACE_MODE_READ_FSCREDS | PTRACE_MODE_NOAUDIT, &task)
            .is_ok()
        {
            startcode = 1;
            endcode = 1;
            startstack = 0;
            kstkesp = 0;
            kstkeip = 0;
            wchan = 0;
            start_data = 0;
            end_data = 0;
            start_brk = 0;
            arg_start = 0;
            arg_end = 0;
            env_start = 0;
            env_end = 0;
            exit_code = 0;
        }

        sink.reserve(512);
        write!(sink, "{pid} (")?;
        sink.write(comm.comm_name());
        writeln!(
            sink,
            ") {state} {ppid} {pgrp} {session} {tty_nr} {tpgid} {flags} {minflt} {cminflt} {majflt} {cmajflt} {utime} {stime} {cutime} {cstime} {priority} {nice} {num_threads} {itrealvalue} {starttime} {vsize} {rss} {rsslim} {startcode} {endcode} {startstack} {kstkesp} {kstkeip} {signal} {blocked} {siginore} {sigcatch} {wchan} {nswap} {cnswap} {exit_signal} {processor} {rt_priority} {policy} {delayacct_blkio_ticks} {guest_time} {cguest_time} {start_data} {end_data} {start_brk} {arg_start} {arg_end} {env_start} {env_end} {exit_code}"
        )?;

        Ok(())
    }
}

#[derive(Clone)]
pub struct StatmFile {
    target: TaskContainer,
}
impl StatmFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}
impl DynamicFileSource for StatmFile {
    fn generate(&self, current_task: &CurrentTask, sink: &mut DynamicFileBuf) -> Result<(), Errno> {
        // /proc/<pid>/statm reports zeroes for kthreads.
        let task = self.target.get_task()?;
        // TODO(b/525059309): Bypassed for traced_probes due to VMAR walk slowness. Re-enable when optimized.
        let mem_stats = if should_skip_memory_stats(current_task) {
            stub_memory_stats()
        } else {
            match task.mm() {
                Ok(mm) => mm.get_stats(current_task, SelfStatsCacheMode::BypassCache),
                Err(_) => Default::default(),
            }
        };
        let page_size = *PAGE_SIZE as usize;

        // 5th and 7th fields are deprecated and should be set to 0.
        writeln!(
            sink,
            "{} {} {} {} 0 {} 0",
            mem_stats.vm_size / page_size,
            mem_stats.vm_rss / page_size,
            mem_stats.rss_shared / page_size,
            mem_stats.vm_exe / page_size,
            (mem_stats.vm_data + mem_stats.vm_stack) / page_size
        )?;
        Ok(())
    }
}

#[derive(Clone)]
pub struct StatusFile {
    target: TaskContainer,
}
impl StatusFile {
    pub fn new_node(target: TaskContainer) -> impl FsNodeOps {
        DynamicFile::new_node(Self { target })
    }
}
impl DynamicFileSource for StatusFile {
    fn generate(&self, current_task: &CurrentTask, sink: &mut DynamicFileBuf) -> Result<(), Errno> {
        let start_monotonic = zx::MonotonicInstant::get();
        let start_boot = zx::BootInstant::get();
        let task = self.target.get_task()?;

        // Typical `/proc/<pid>/status` output fits in 1024 bytes; grow the buffer once.
        sink.reserve(1024);

        sink.write(b"Name:\t");
        sink.write(task.persistent_info.command_guard().comm_name());
        sink.write(b"\n");

        if let Ok(fs) = task.fs() {
            writeln!(sink, "Umask:\t0{:03o}", fs.umask().bits())?;
        }

        // Snapshot everything needed from the task state up front so `task.read()` is acquired
        // only once and nothing is formatted while holding it.
        let (sigblk, sigpnd, no_new_privs, state_code, tracer_tg) = {
            let task_state = task.read();
            (
                task_state.signal_mask().0,
                task_state.task_specific_pending_signals().0,
                task_state.no_new_privs() as u8,
                task_state.unrefined_state_code(),
                task_state.ptrace.as_ref().and_then(|p| p.core_state.thread_group.upgrade()),
            )
        };
        // The tracer's `Arc<ThreadGroup>` is dropped here, outside of the task lock.
        let tracer_pid = tracer_tg.map_or(0, |tg| tg.leader.id);
        let state_code = task.refine_state_code(state_code);

        let thread_group = task.thread_group();
        let shdpnd = thread_group.pending_signals.lock().pending().0;
        let (sigign, sigcgt) = thread_group.signal_actions.ignored_and_caught();
        let (ppid, threads) = {
            let tg_state = thread_group.read();
            (tg_state.get_ppid(), tg_state.tasks_count())
        };

        writeln!(sink, "SigBlk:\t{:016x}", sigblk)?;
        writeln!(sink, "SigPnd:\t{:016x}", sigpnd)?;
        writeln!(sink, "ShdPnd:\t{:016x}", shdpnd)?;
        writeln!(sink, "SigIgn:\t{:016x}", sigign.0)?;
        writeln!(sink, "SigCgt:\t{:016x}", sigcgt.0)?;
        writeln!(sink, "NoNewPrivs:\t{}", no_new_privs)?;

        {
            // Since version 3.8 all nonexistent capabilities are reported as not-enabled.
            let creds = task.real_creds();
            writeln!(sink, "CapInh:\t{:016x}", creds.cap_inheritable)?;
            writeln!(sink, "CapPrm:\t{:016x}", creds.cap_permitted)?;
            writeln!(sink, "CapEff:\t{:016x}", creds.cap_effective)?;
            writeln!(sink, "CapBnd:\t{:016x}", creds.cap_bounding)?;
            writeln!(sink, "CapAmb:\t{:016x}", creds.cap_ambient)?;

            writeln!(sink, "State:\t{} ({})", state_code.code_char(), state_code.name())?;
            writeln!(sink, "Tgid:\t{}", task.pid)?;
            writeln!(sink, "Pid:\t{}", self.target.tid)?;
            writeln!(sink, "PPid:\t{}", ppid)?;
            writeln!(sink, "TracerPid:\t{}", tracer_pid)?;

            writeln!(
                sink,
                "Uid:\t{}\t{}\t{}\t{}",
                creds.uid, creds.euid, creds.saved_uid, creds.fsuid
            )?;
            writeln!(
                sink,
                "Gid:\t{}\t{}\t{}\t{}",
                creds.gid, creds.egid, creds.saved_gid, creds.fsgid
            )?;

            // Stream supplementary groups directly into the sink to avoid intermediate `String`s.
            sink.write(b"Groups:\t");
            let mut itoa_buf = itoa::Buffer::new();
            for (i, &gid) in creds.groups.iter().enumerate() {
                if i > 0 {
                    sink.write(b" ");
                }
                sink.write(itoa_buf.format(gid).as_bytes());
            }
            sink.write(b"\n");
        }

        if let Ok(mm) = task.mm() {
            // TODO(b/525059309): Bypassed for traced_probes due to VMAR walk slowness. Re-enable when optimized.
            let mem_stats = if should_skip_memory_stats(current_task) {
                stub_memory_stats()
            } else {
                mm.get_stats(current_task, SelfStatsCacheMode::AllowCached)
            };
            writeln!(sink, "VmSize:\t{} kB", mem_stats.vm_size / 1024)?;
            writeln!(sink, "VmLck:\t{} kB", mem_stats.vm_lck / 1024)?;
            writeln!(sink, "VmRSS:\t{} kB", mem_stats.vm_rss / 1024)?;
            writeln!(sink, "RssAnon:\t{} kB", mem_stats.rss_anonymous / 1024)?;
            writeln!(sink, "RssFile:\t{} kB", mem_stats.rss_file / 1024)?;
            writeln!(sink, "RssShmem:\t{} kB", mem_stats.rss_shared / 1024)?;
            writeln!(sink, "VmData:\t{} kB", mem_stats.vm_data / 1024)?;
            writeln!(sink, "VmStk:\t{} kB", mem_stats.vm_stack / 1024)?;
            writeln!(sink, "VmExe:\t{} kB", mem_stats.vm_exe / 1024)?;
            writeln!(sink, "VmSwap:\t{} kB", mem_stats.vm_swap / 1024)?;
            writeln!(sink, "VmHWM:\t{} kB", mem_stats.vm_rss_hwm / 1024)?;
        }
        // Report seccomp filter status.
        let seccomp = task.seccomp_filter_state.get() as u8;
        writeln!(sink, "Seccomp:\t{}", seccomp)?;

        // There should be at least one thread in Zombie processes.
        writeln!(sink, "Threads:\t{}", std::cmp::max(1, threads))?;

        let elapsed_monotonic = zx::MonotonicInstant::get() - start_monotonic;
        let elapsed_boot = zx::BootInstant::get() - start_boot;
        if elapsed_monotonic > zx::MonotonicDuration::from_millis(100)
            || elapsed_boot > zx::BootDuration::from_seconds(1)
        {
            let target_pid = task.pid.id;
            let target_comm =
                String::from_utf8_lossy(task.persistent_info.command_guard().comm_name())
                    .into_owned();
            starnix_logging::log_warn!(
                "StatusFile::generate for task {} ({}) took {} ms (monotonic), {} ms (boot)",
                target_pid,
                target_comm,
                elapsed_monotonic.into_millis(),
                elapsed_boot.into_millis()
            );
        }

        Ok(())
    }
}

struct OomScoreFile {
    target: TaskContainer,
}

impl OomScoreFile {
    fn new_node(target: TaskContainer) -> impl FsNodeOps {
        BytesFile::new_node(Self { target })
    }
}

impl BytesFileOps for OomScoreFile {
    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let _task = self.target.get_task()?;
        track_stub!(TODO("https://fxbug.dev/322873459"), "/proc/pid/oom_score");
        Ok(serialize_for_file(0).into())
    }
}

// Redefine these constants as i32 to avoid conversions below.
const OOM_ADJUST_MAX: i32 = uapi::OOM_ADJUST_MAX as i32;
const OOM_SCORE_ADJ_MAX: i32 = uapi::OOM_SCORE_ADJ_MAX as i32;

struct OomAdjFile {
    target: TaskContainer,
}
impl OomAdjFile {
    fn new_node(target: TaskContainer) -> impl FsNodeOps {
        BytesFile::new_node(Self { target })
    }
}

impl BytesFileOps for OomAdjFile {
    fn write(&self, current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let value = parse_i32_file(&data)?;
        let oom_score_adj = if value == OOM_DISABLE {
            OOM_SCORE_ADJ_MIN
        } else {
            if !(OOM_ADJUST_MIN..=OOM_ADJUST_MAX).contains(&value) {
                return error!(EINVAL);
            }
            let fraction = (value - OOM_ADJUST_MIN) / (OOM_ADJUST_MAX - OOM_ADJUST_MIN);
            fraction * (OOM_SCORE_ADJ_MAX - OOM_SCORE_ADJ_MIN) + OOM_SCORE_ADJ_MIN
        };
        security::check_task_capable(current_task, CAP_SYS_RESOURCE)?;
        let task = self.target.get_task()?;
        task.write().oom_score_adj = oom_score_adj;
        Ok(())
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let task = self.target.get_task()?;
        let oom_score_adj = task.read().oom_score_adj;
        let oom_adj = if oom_score_adj == OOM_SCORE_ADJ_MIN {
            OOM_DISABLE
        } else {
            let fraction =
                (oom_score_adj - OOM_SCORE_ADJ_MIN) / (OOM_SCORE_ADJ_MAX - OOM_SCORE_ADJ_MIN);
            fraction * (OOM_ADJUST_MAX - OOM_ADJUST_MIN) + OOM_ADJUST_MIN
        };
        Ok(serialize_for_file(oom_adj).into())
    }
}

struct OomScoreAdjFile {
    target: TaskContainer,
}

impl OomScoreAdjFile {
    fn new_node(target: TaskContainer) -> impl FsNodeOps {
        BytesFile::new_node(Self { target })
    }
}

impl BytesFileOps for OomScoreAdjFile {
    fn write(&self, current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let value = parse_i32_file(&data)?;
        if !(OOM_SCORE_ADJ_MIN..=OOM_SCORE_ADJ_MAX).contains(&value) {
            return error!(EINVAL);
        }
        security::check_task_capable(current_task, CAP_SYS_RESOURCE)?;
        let task = self.target.get_task()?;
        task.write().oom_score_adj = value;
        Ok(())
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let task = self.target.get_task()?;
        let oom_score_adj = task.read().oom_score_adj;
        Ok(serialize_for_file(oom_score_adj).into())
    }
}

struct TimerslackNsFile {
    target: TaskContainer,
}

impl TimerslackNsFile {
    fn new_node(target: TaskContainer) -> impl FsNodeOps {
        BytesFile::new_node(Self { target })
    }
}

impl BytesFileOps for TimerslackNsFile {
    fn write(&self, current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        let target_task = self.target.get_task()?;
        let same_task = current_task.task.pid == target_task.pid;
        if !same_task {
            security::check_task_capable(current_task, CAP_SYS_NICE)?;
            security::check_task_setscheduler_access(current_task, &target_task)?;
        };

        let value = parse_unsigned_file(&data)?;
        target_task.write().set_timerslack_ns(value);
        Ok(())
    }

    fn read(&self, current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        let target_task = self.target.get_task()?;
        let same_task = current_task.task.pid == target_task.pid;
        if !same_task {
            security::check_task_capable(current_task, CAP_SYS_NICE)?;
            security::check_task_getscheduler_access(current_task, &target_task)?;
        };

        let timerslack_ns = target_task.read().timerslack_ns;
        Ok(serialize_for_file(timerslack_ns).into())
    }
}

struct ClearRefsFile {
    target: TaskContainer,
}

impl ClearRefsFile {
    fn new_node(target: TaskContainer) -> impl FsNodeOps {
        BytesFile::new_node(Self { target })
    }
}

impl BytesFileOps for ClearRefsFile {
    fn write(&self, _current_task: &CurrentTask, _data: Vec<u8>) -> Result<(), Errno> {
        let _task = self.target.get_task()?;
        track_stub!(TODO("https://fxbug.dev/396221597"), "/proc/pid/clear_refs");
        Ok(())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{
        assert_cached_lookup, cached_entry_survives, lookup_no_follow,
        spawn_kernel_with_cached_procfs, spawn_kernel_with_cached_procfs_and_options,
        spawn_kernel_with_procfs, spawn_kernel_with_procfs_and_options,
    };
    use assert_matches::assert_matches;
    use starnix_core::device::terminal::{TerminalSide, TtyState};
    use starnix_core::mm::DumpPolicy;
    use starnix_core::task::dynamic_thread_spawner::SpawnRequestBuilder;
    use starnix_core::task::{ExitStatus, KernelFeatures, TaskStateCode};
    use starnix_core::testing::{
        anon_test_file, create_task, map_memory_anywhere, spawn_kernel_and_run,
        spawn_kernel_with_features_and_run,
    };
    use starnix_core::vfs::fs_args::MountParams;
    use starnix_core::vfs::{
        FileSystemOptions, LookupContext, NamespaceNode, SymlinkMode, VecOutputBuffer,
    };
    use starnix_uapi::auth::{CAP_DAC_OVERRIDE, Credentials};
    use starnix_uapi::signals::SIGCHLD;
    use starnix_uapi::{CLONE_SIGHAND, CLONE_THREAD, CLONE_VM, SIG_IGN, gid_t, sigaction_t, uid_t};
    use test_case::test_case;

    #[::fuchsia::test]
    async fn test_fdinfo_file_lazy_and_cloexec() {
        spawn_kernel_and_run(async move |current_task| {
            let target_file = anon_test_file(
                current_task,
                Box::new(BytesFile::new(b"0123456789".to_vec())),
                OpenFlags::RDONLY,
            );
            let fd = current_task
                .files()
                .add(current_task, target_file.clone(), FdFlags::empty())
                .expect("add fd");

            let fdinfo_file = anon_test_file(
                current_task,
                Box::new(DynamicFile::new(FdInfoFile {
                    target: TaskContainer::from_task(&current_task.task),
                    fd,
                })),
                OpenFlags::RDONLY,
            );

            // 1. Initial read reflects offset 0 and no O_CLOEXEC.
            let mut buf = VecOutputBuffer::new(256);
            let n = fdinfo_file.read_at(current_task, 0, &mut buf).expect("read fdinfo");
            let text = String::from_utf8(buf.data()[..n].to_vec()).unwrap();
            let expected_initial = format!("pos:\t0\nflags:\t0{:o}\n", OpenFlags::RDONLY.bits());
            assert_eq!(text, expected_initial);

            // 2. Advancing offset and setting FD_CLOEXEC are reflected lazily on next read from 0.
            target_file.seek(current_task, SeekTarget::Set(5)).expect("seek");
            current_task
                .files()
                .set_fd_flags_allowing_opath(fd, FdFlags::CLOEXEC)
                .expect("set cloexec");

            let mut buf2 = VecOutputBuffer::new(256);
            let n2 = fdinfo_file.read_at(current_task, 0, &mut buf2).expect("read fdinfo after");
            let text2 = String::from_utf8(buf2.data()[..n2].to_vec()).unwrap();
            let expected_updated = format!(
                "pos:\t5\nflags:\t0{:o}\n",
                (OpenFlags::RDONLY | OpenFlags::CLOEXEC).bits()
            );
            assert_eq!(text2, expected_updated);

            // 3. Closing the FD does not keep `target_file` pinned in `FdInfoFile` and subsequent
            // reads return ENOENT.
            current_task.files().close(fd).expect("close fd");
            let mut buf3 = VecOutputBuffer::new(256);
            assert_eq!(fdinfo_file.read_at(current_task, 0, &mut buf3), error!(ENOENT));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_status_file_groups_and_state() {
        spawn_kernel_with_features_and_run(
            async move |current_task| {
                let target_task = create_task(current_task.kernel(), "status_target");
                let status_file = anon_test_file(
                    current_task,
                    Box::new(DynamicFile::new(StatusFile {
                        target: TaskContainer::from_task(&target_task.task),
                    })),
                    OpenFlags::RDONLY,
                );
                let has_line = |line: &str| {
                    let mut buf = VecOutputBuffer::new(2048);
                    let n = status_file.read_at(current_task, 0, &mut buf).expect("read status");
                    std::str::from_utf8(&buf.data()[..n]).unwrap().lines().any(|l| l == line)
                };

                let mut creds = Credentials::with_ids(1001, 2001);
                for groups in [vec![], vec![3001], vec![3001, 3002, 3003]] {
                    creds.groups = groups.clone();
                    target_task.set_creds(creds.clone());
                    let expected = groups.iter().map(u32::to_string).collect::<Vec<_>>().join(" ");
                    assert!(has_line(&format!("Groups:\t{expected}")));
                }

                assert!(has_line("State:\tR (running)"));
                // Like Linux, an exiting task keeps reporting its run state until it has exited.
                target_task.write().set_exiting(ExitStatus::Exit(0));
                assert!(has_line("State:\tR (running)"));
            },
            KernelFeatures { cached_zx_map_info_bytes: 4096, ..Default::default() },
        )
        .await;
    }

    #[::fuchsia::test]
    async fn test_stat_file_avoids_zx_vmar_walk() {
        spawn_kernel_with_features_and_run(
            async move |current_task| {
                let page_size = *PAGE_SIZE as usize;
                let mm = current_task.mm().unwrap();
                current_task.set_command_name(TaskCommand::new(b"traced"));
                let stat_file = anon_test_file(
                    current_task,
                    Box::new(DynamicFile::new(StatFile {
                        target: TaskContainer::from_task(&current_task.task),
                    })),
                    OpenFlags::RDONLY,
                );
                let read_stat_vsize_and_rss = || -> (usize, usize) {
                    let mut buf = VecOutputBuffer::new(512);
                    let n = stat_file.read_at(current_task, 0, &mut buf).expect("read stat");
                    let text = std::str::from_utf8(&buf.data()[..n]).unwrap().trim_end();
                    let comm_end = text.rfind(')').expect("comm end");
                    let after_comm: Vec<&str> = text[comm_end + 1..].split_whitespace().collect();
                    // Fields after `)` start at field 3 (`state`); `vsize` is field 23 (index 20)
                    // and `rss` is field 24 (index 21).
                    let vsize = after_comm[20].parse::<usize>().expect("vsize");
                    let rss = after_comm[21].parse::<usize>().expect("rss");
                    (vsize, rss)
                };

                assert!(mm.cached_stats.lock().is_none());
                let addr = map_memory_anywhere(current_task, 4 * *PAGE_SIZE);
                current_task.write_memory(addr, &[1u8; 8]).unwrap();
                current_task.write_memory((addr + 2 * page_size).unwrap(), &[1u8; 8]).unwrap();

                // 1. Reading `/proc/[pid]/stat` on the `should_skip_memory_stats` fast path when
                // `cached_stats` is `None` must not trigger a `ZX_INFO_VMAR_MAPS` walk (which would
                // populate `cached_stats`), reports exact `vsize`, and reports a 1-page fallback
                // `rss`.
                let (vsize_uncached, rss_uncached) = read_stat_vsize_and_rss();
                assert!(mm.cached_stats.lock().is_none());
                assert_eq!(vsize_uncached, mm.get_total_usage());
                assert_eq!(rss_uncached, 1);

                // 2. Once `cached_stats` is populated (e.g. by `/proc/[pid]/status`), `/proc/[pid]/stat`
                // uses the cached `vm_rss` (including 0) even if the entry is older than
                // `CACHE_TTL`, and clamps `rss * page_size <= vsize` after `munmap`.
                *mm.cached_stats.lock() = Some((
                    zx::MonotonicInstant::INFINITE_PAST,
                    MemoryStats { vm_rss: 0, ..Default::default() },
                ));
                let (_, rss_zero) = read_stat_vsize_and_rss();
                assert_eq!(rss_zero, 0);

                *mm.cached_stats.lock() = Some((
                    zx::MonotonicInstant::INFINITE_PAST,
                    MemoryStats { vm_rss: vsize_uncached - page_size, ..Default::default() },
                ));
                let (vsize_cached, rss_cached) = read_stat_vsize_and_rss();
                assert_eq!(vsize_cached, vsize_uncached);
                assert_eq!(rss_cached, (vsize_uncached - page_size) / page_size);

                mm.unmap(addr, 2 * page_size).unwrap();
                let (vsize_after_unmap, rss_after_unmap) = read_stat_vsize_and_rss();
                assert_eq!(vsize_after_unmap, vsize_uncached - 2 * page_size);
                assert_eq!(rss_after_unmap, vsize_after_unmap / page_size);

                // 3. General callers populate `cached_stats` via `get_stats_locked` under a single
                // `mm.state.read()` acquisition.
                *mm.cached_stats.lock() = None;
                current_task.set_command_name(TaskCommand::new(b"app_reader"));
                let (vsize_populated, rss_populated) = read_stat_vsize_and_rss();
                assert!(mm.cached_stats.lock().is_some());
                assert_eq!(vsize_populated, vsize_after_unmap);
                assert!(rss_populated >= 1);
            },
            KernelFeatures { cached_zx_map_info_bytes: 4096, ..Default::default() },
        )
        .await;
    }

    /// Returns the target of the `/proc/<pid>/ns` link at `path`.
    fn ns_link_target(current_task: &CurrentTask, path: &str) -> FsString {
        let link = lookup_no_follow(current_task, path).expect("lookup ns link");
        match link.readlink(current_task).expect("readlink ns link") {
            SymlinkTarget::Path(target) => target,
            SymlinkTarget::Node(_) => panic!("{path} must link to a path"),
        }
    }

    /// Returns the `<id>` of a `<ns>:[<id>]` link target.
    fn ns_link_id(target: &FsStr) -> u64 {
        let target = std::str::from_utf8(target).expect("UTF-8 link target");
        let (_, id) = target.split_once(":[").expect("<ns>:[<id>] link target");
        id.strip_suffix(']').expect("<ns>:[<id>] link target").parse().expect("numeric id")
    }

    #[::fuchsia::test]
    async fn test_ns_symlink_uses_target_task_namespace() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let target_task = create_task(current_task.kernel(), "ns_target");
            target_task.fs().unshare_namespace();
            let expected = format!("mnt:[{}]", target_task.fs().namespace().id);
            let path = format!("/proc/{}/ns/mnt", target_task.get_pid());
            assert_eq!(ns_link_target(current_task, &path), expected);
            target_task.write().set_exiting(ExitStatus::Exit(0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_mnt_lookup_rejects_caller_namespace_id() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let target_task = create_task(current_task.kernel(), "ns_target");
            target_task.fs().unshare_namespace();
            let target_pid = target_task.get_pid();
            let caller_id = current_task.fs().namespace().id;
            assert_ne!(target_task.fs().namespace().id, caller_id);
            assert_matches!(
                lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns/mnt:[{caller_id}]")),
                Err(e) if e == errno!(ENOENT)
            );
            target_task.write().set_exiting(ExitStatus::Exit(0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_mnt_lookup_accepts_target_namespace_id() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let target_task = create_task(current_task.kernel(), "ns_target");
            target_task.fs().unshare_namespace();
            let target_pid = target_task.get_pid();
            let target_id = target_task.fs().namespace().id;
            assert_matches!(
                lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns/mnt:[{target_id}]")),
                Ok(_)
            );
            target_task.write().set_exiting(ExitStatus::Exit(0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_mnt_open_yields_target_namespace() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let target_task = create_task(current_task.kernel(), "ns_target");
            target_task.fs().unshare_namespace();
            let path = format!("/proc/{}/ns/mnt", target_task.get_pid());
            let file =
                current_task.open_file(path.as_str().into(), OpenFlags::RDONLY).expect("open");
            let ns_file = file.downcast_file::<MountNamespaceFile>().expect("MountNamespaceFile");
            assert!(Arc::ptr_eq(&ns_file.0, &target_task.fs().namespace()));
            target_task.write().set_exiting(ExitStatus::Exit(0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_mnt_node_uses_weak_namespace() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            current_task.fs().unshare_namespace();
            let pid = current_task.get_pid();
            let private_id = current_task.fs().namespace().id;
            let weak_ns = Arc::downgrade(&current_task.fs().namespace());
            let _mnt_node =
                lookup_no_follow(current_task, &format!("/proc/{pid}/ns/mnt:[{private_id}]"))
                    .expect("lookup mnt:[id]")
                    .entry
                    .node
                    .clone();
            current_task.fs().unshare_namespace();
            assert_matches!(weak_ns.upgrade(), None);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_mnt_lookup_rejects_extra_colon_segment() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let pid = current_task.get_pid();
            let id = current_task.fs().namespace().id;
            assert_matches!(
                lookup_no_follow(current_task, &format!("/proc/{pid}/ns/mnt:[{id}]:extra")),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_mnt_node_expired_open_fails() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            current_task.fs().unshare_namespace();
            let pid = current_task.get_pid();
            let private_id = current_task.fs().namespace().id;
            let mnt_node =
                lookup_no_follow(current_task, &format!("/proc/{pid}/ns/mnt:[{private_id}]"))
                    .expect("lookup mnt:[id]");
            current_task.fs().unshare_namespace();
            assert_matches!(
                mnt_node.open(current_task, OpenFlags::RDONLY),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_symlink_unprivileged_readlink_fails() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let target_task = create_task(current_task.kernel(), "ns_target");
            let target_pid = target_task.get_pid();
            let ns_mnt = lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns/mnt"))
                .expect("lookup target ns/mnt");
            current_task.set_creds(Credentials::with_ids(1000, 1000));
            assert_matches!(ns_mnt.readlink(current_task), Err(e) if e == errno!(EACCES));
            target_task.write().set_exiting(ExitStatus::Exit(0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_kthread_unprivileged_readlink_fails() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let kthread_pid = current_task.kernel().kthreads.system_thread_group().leader.clone();
            let ns_mnt = lookup_no_follow(current_task, &format!("/proc/{kthread_pid}/ns/mnt"))
                .expect("lookup kthread ns/mnt");
            current_task.set_creds(Credentials::with_ids(1000, 1000));
            assert_matches!(ns_mnt.readlink(current_task), Err(e) if e == errno!(EACCES));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_symlink_unprivileged_lookup_succeeds() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let target_task = create_task(current_task.kernel(), "ns_target");
            let target_pid = target_task.get_pid();
            current_task.set_creds(Credentials::with_ids(1000, 1000));
            assert_matches!(
                lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns/mnt")),
                Ok(_)
            );
            target_task.write().set_exiting(ExitStatus::Exit(0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_mnt_lookup_unprivileged_fails() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let target_task = create_task(current_task.kernel(), "ns_target");
            let target_pid = target_task.get_pid();
            let target_id = target_task.fs().namespace().id;
            current_task.set_creds(Credentials::with_ids(1000, 1000));
            assert_matches!(
                lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns/mnt:[{target_id}]")),
                Err(e) if e == errno!(EACCES)
            );
            target_task.write().set_exiting(ExitStatus::Exit(0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_placeholder_lookup_unprivileged_fails() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let target_task = create_task(current_task.kernel(), "ns_target");
            let target_pid = target_task.get_pid();
            let net_target = ns_link_target(current_task, &format!("/proc/{target_pid}/ns/net"));
            current_task.set_creds(Credentials::with_ids(1000, 1000));
            assert_matches!(
                lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns/{net_target}")),
                Err(e) if e == errno!(EACCES)
            );
            target_task.write().set_exiting(ExitStatus::Exit(0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_mnt_cached_entry_unprivileged_lookup_fails() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let target_task = create_task(current_task.kernel(), "ns_target");
            let target_pid = target_task.get_pid();
            let target_id = target_task.fs().namespace().id;
            let _held =
                lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns/mnt:[{target_id}]"))
                    .expect("privileged lookup");
            current_task.set_creds(Credentials::with_ids(1000, 1000));
            assert_matches!(
                lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns/mnt:[{target_id}]")),
                Err(e) if e == errno!(EACCES)
            );
            target_task.write().set_exiting(ExitStatus::Exit(0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_mnt_held_node_unprivileged_open_succeeds() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let target_task = create_task(current_task.kernel(), "ns_target");
            let target_pid = target_task.get_pid();
            let target_id = target_task.fs().namespace().id;
            let held =
                lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns/mnt:[{target_id}]"))
                    .expect("privileged lookup");
            current_task.set_creds(Credentials::with_ids(1000, 1000));
            assert_matches!(held.open(current_task, OpenFlags::RDONLY), Ok(_));
            target_task.write().set_exiting(ExitStatus::Exit(0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_mnt_cached_entry_stale_after_unshare() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            current_task.fs().unshare_namespace();
            let pid = current_task.get_pid();
            let old_id = current_task.fs().namespace().id;
            let _held = lookup_no_follow(current_task, &format!("/proc/{pid}/ns/mnt:[{old_id}]"))
                .expect("lookup old id");
            current_task.fs().unshare_namespace();
            assert_matches!(
                lookup_no_follow(current_task, &format!("/proc/{pid}/ns/mnt:[{old_id}]")),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_symlink_exited_task_returns_enoent() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let ns_mnt = {
                let target_task = create_task(current_task.kernel(), "ns_target");
                let target_pid = target_task.get_pid();
                let ns_mnt = lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns/mnt"))
                    .expect("lookup");
                target_task.write().set_exiting(ExitStatus::Exit(0));
                ns_mnt
            };
            assert_matches!(ns_mnt.readlink(current_task), Err(e) if e == errno!(ENOENT));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_placeholder_lookup_rejects_other_id() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            assert_matches!(
                lookup_no_follow(current_task, "/proc/self/ns/net:[1]"),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_placeholder_follow_succeeds() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            assert_matches!(current_task.lookup_path_from_root("/proc/self/ns/net".into()), Ok(_));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_placeholder_lookup_exited_task_fails() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let (ns_dir, net_target) = {
                let target_task = create_task(current_task.kernel(), "ns_target");
                let target_pid = target_task.get_pid();
                let net_target =
                    ns_link_target(current_task, &format!("/proc/{target_pid}/ns/net"));
                let ns_dir = lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns"))
                    .expect("lookup ns dir");
                target_task.write().set_exiting(ExitStatus::Exit(0));
                (ns_dir, net_target)
            };
            assert_matches!(
                ns_dir.lookup_child(
                    current_task,
                    &mut LookupContext::new(SymlinkMode::NoFollow),
                    net_target.as_ref(),
                ),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_placeholder_cached_entry_exited_task_lookup_fails() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let (ns_dir, net_target, _held) = {
                let target_task = create_task(current_task.kernel(), "ns_target");
                let target_pid = target_task.get_pid();
                let net_target =
                    ns_link_target(current_task, &format!("/proc/{target_pid}/ns/net"));
                let ns_dir = lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns"))
                    .expect("lookup ns dir");
                let held = ns_dir
                    .lookup_child(
                        current_task,
                        &mut LookupContext::new(SymlinkMode::NoFollow),
                        net_target.as_ref(),
                    )
                    .expect("lookup net:[id]");
                target_task.write().set_exiting(ExitStatus::Exit(0));
                (ns_dir, net_target, held)
            };
            assert_matches!(
                ns_dir.lookup_child(
                    current_task,
                    &mut LookupContext::new(SymlinkMode::NoFollow),
                    net_target.as_ref(),
                ),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    fn test_ns_type_name_round_trip() {
        for ns_type in NsType::ALL {
            assert_eq!(NsType::from_name(ns_type.name()), Some(ns_type));
        }
    }

    #[::fuchsia::test]
    fn test_parse_ns_entry_name_rejects_leading_plus() {
        assert_eq!(parse_ns_entry_name(b"mnt:[+1]"), None);
    }

    #[::fuchsia::test]
    fn test_parse_ns_entry_name_rejects_leading_zero() {
        assert_eq!(parse_ns_entry_name(b"mnt:[01]"), None);
    }

    #[::fuchsia::test]
    async fn test_ns_link_ids_are_distinct() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let ids: std::collections::HashSet<u64> = NsType::ALL
                .into_iter()
                .filter(|ns_type| ns_type.link_type() == *ns_type)
                .map(|ns_type| {
                    let path = format!("/proc/self/ns/{}", ns_type.name());
                    ns_link_id(ns_link_target(current_task, &path).as_ref())
                })
                .collect();
            assert_eq!(ids.len(), 8);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_pid_for_children_links_to_pid_namespace() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            assert_eq!(
                ns_link_target(current_task, "/proc/self/ns/pid_for_children"),
                ns_link_target(current_task, "/proc/self/ns/pid")
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_pid_for_children_follow_succeeds() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            assert_matches!(
                current_task.lookup_path_from_root("/proc/self/ns/pid_for_children".into()),
                Ok(_)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_pid_for_children_target_lookup_fails() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let id = ns_link_id(ns_link_target(current_task, "/proc/self/ns/pid").as_ref());
            assert_matches!(
                lookup_no_follow(current_task, &format!("/proc/self/ns/pid_for_children:[{id}]")),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_time_for_children_links_to_time_namespace() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            assert_eq!(
                ns_link_target(current_task, "/proc/self/ns/time_for_children"),
                ns_link_target(current_task, "/proc/self/ns/time")
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_time_for_children_follow_succeeds() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            assert_matches!(
                current_task.lookup_path_from_root("/proc/self/ns/time_for_children".into()),
                Ok(_)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_symlink_cached_entry_reaped_task_lookup_fails() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            current_task
                .thread_group()
                .signal_actions
                .set(SIGCHLD, sigaction_t { sa_handler: SIG_IGN, ..Default::default() });
            let (ns_dir, _held) = {
                let target_task = create_task(current_task.kernel(), "ns_target");
                let target_pid = target_task.get_pid();
                let ns_dir = lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns"))
                    .expect("lookup ns dir");
                let held = ns_dir
                    .lookup_child(
                        current_task,
                        &mut LookupContext::new(SymlinkMode::NoFollow),
                        "mnt".into(),
                    )
                    .expect("lookup mnt");
                target_task.write().set_exiting(ExitStatus::Exit(0));
                (ns_dir, held)
            };
            assert_matches!(
                ns_dir.lookup_child(
                    current_task,
                    &mut LookupContext::new(SymlinkMode::NoFollow),
                    "mnt".into(),
                ),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_symlink_cached_entry_unprivileged_lookup_succeeds() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let target_task = create_task(current_task.kernel(), "ns_target");
            let target_pid = target_task.get_pid();
            let _held = lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns/mnt"))
                .expect("privileged lookup");
            current_task.set_creds(Credentials::with_ids(1000, 1000));
            assert_matches!(
                lookup_no_follow(current_task, &format!("/proc/{target_pid}/ns/mnt")),
                Ok(_)
            );
            target_task.write().set_exiting(ExitStatus::Exit(0));
        })
        .await;
    }
    /// Gives `current_task` distinct uid, gid, euid, egid, fsuid, and fsgid values, so that tests
    /// can tell which of them a `/proc/<pid>` node's owner comes from.
    fn set_distinct_creds(current_task: &CurrentTask) {
        let mut creds = Credentials::with_ids(1000, 1001);
        creds.euid = 2000;
        creds.egid = 2001;
        creds.fsuid = 3000;
        creds.fsgid = 3001;
        // `fd` and `fdinfo` are 0500 and owned by euid 2000, but DAC checks use fsuid 3000.
        creds.cap_permitted |= CAP_DAC_OVERRIDE;
        creds.cap_effective |= CAP_DAC_OVERRIDE;
        current_task.set_creds(creds);
        // Changing the euid makes the task non-dumpable. proc(5) makes a non-dumpable process's
        // `/proc/<pid>` files root-owned, so mark the task dumpable again to keep these tests
        // about euid/egid ownership.
        current_task.mm().expect("mm").dumpable.store(DumpPolicy::User);
    }

    fn owner(node: &NamespaceNode) -> (uid_t, gid_t) {
        let info = node.entry.node.info();
        (info.uid, info.gid)
    }

    /// The `/proc/<pid>/attr` entries that are writable, which are all of them except `prev`.
    const WRITABLE_ATTR_NAMES: [&str; 5] =
        ["current", "exec", "fscreate", "keycreate", "sockcreate"];

    fn attr_entry(current_task: &CurrentTask, name: &str) -> NamespaceNode {
        lookup_no_follow(current_task, &format!("/proc/self/attr/{name}"))
            .expect("lookup attr entry")
    }

    fn attr_entry_mode(current_task: &CurrentTask, name: &str) -> FileMode {
        attr_entry(current_task, name).entry.node.info().mode
    }

    /// Records the entries passed to a `DirentSink`.
    #[derive(Default)]
    struct RecordingDirentSink {
        offset: off_t,
        entries: Vec<(ino_t, DirectoryEntryType, FsString)>,
    }

    impl DirentSink for RecordingDirentSink {
        fn add(
            &mut self,
            inode_num: ino_t,
            offset: off_t,
            entry_type: DirectoryEntryType,
            name: &FsStr,
        ) -> Result<(), Errno> {
            self.offset = offset;
            self.entries.push((inode_num, entry_type, name.to_owned()));
            Ok(())
        }

        fn offset(&self) -> off_t {
            self.offset
        }
    }

    /// Returns the `(d_ino, d_type, name)` of each `/proc/self/attr` entry that readdir lists,
    /// without `.` and `..`.
    fn list_attr_dir(current_task: &CurrentTask) -> Vec<(ino_t, DirectoryEntryType, FsString)> {
        let dir = current_task
            .open_file("/proc/self/attr".into(), OpenFlags::RDONLY | OpenFlags::DIRECTORY)
            .expect("open attr dir");
        let mut sink = RecordingDirentSink::default();
        dir.readdir(current_task, &mut sink).expect("readdir attr dir");
        sink.entries.retain(|(_, _, name)| name != "." && name != "..");
        sink.entries
    }

    #[::fuchsia::test]
    async fn test_attr_entry_owned_by_current_euid() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let attr_dir = lookup_no_follow(current_task, "/proc/self/attr").expect("lookup attr");
            set_distinct_creds(current_task);
            // Look up the entry through the `attr` directory instantiated before the change.
            let entry = attr_dir
                .lookup_child(current_task, &mut LookupContext::default(), "current".into())
                .expect("lookup attr/current");
            assert_eq!(owner(&entry), (2000, 2001));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_attr_writable_entries_mode_0666() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            for name in WRITABLE_ATTR_NAMES {
                assert_eq!(attr_entry_mode(current_task, name), mode!(IFREG, 0o666), "{name}");
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_attr_prev_mode_0444() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            assert_eq!(attr_entry_mode(current_task, "prev"), mode!(IFREG, 0o444));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_attr_unknown_entry_lookup_fails() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            assert_matches!(
                lookup_no_follow(current_task, "/proc/self/attr/nonexistent"),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_attr_lookup_reaped_task_returns_enoent() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            current_task
                .thread_group()
                .signal_actions
                .set(SIGCHLD, sigaction_t { sa_handler: SIG_IGN, ..Default::default() });
            // Hold only the `attr` directory, not any of its entries, while the task is reaped.
            let attr_dir = {
                let target_task = create_task(current_task.kernel(), "attr_target");
                let target_pid = target_task.get_pid();
                let attr_dir = lookup_no_follow(current_task, &format!("/proc/{target_pid}/attr"))
                    .expect("lookup attr dir");
                target_task.write().set_exiting(ExitStatus::Exit(0));
                attr_dir
            };
            assert_matches!(
                attr_dir.lookup_child(current_task, &mut LookupContext::default(), "current".into()),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_attr_readdir_reaped_task_returns_enoent() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            current_task
                .thread_group()
                .signal_actions
                .set(SIGCHLD, sigaction_t { sa_handler: SIG_IGN, ..Default::default() });
            // Hold the open `attr` directory while the task is reaped.
            let attr_file = {
                let target_task = create_task(current_task.kernel(), "attr_target");
                let target_pid = target_task.get_pid();
                let attr_file = current_task
                    .open_file(
                        format!("/proc/{target_pid}/attr").as_str().into(),
                        OpenFlags::RDONLY | OpenFlags::DIRECTORY,
                    )
                    .expect("open attr dir");
                target_task.write().set_exiting(ExitStatus::Exit(0));
                attr_file
            };
            assert_matches!(
                attr_file.readdir(current_task, &mut RecordingDirentSink::default()),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_attr_entry_ino_stable_across_attr_lookups() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            // Nothing holds `attr` between the two walks, so each walk instantiates it anew.
            let ino = || attr_entry(current_task, "current").entry.node.ino;
            assert_eq!(ino(), ino());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_attr_entry_inos_distinct() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let inos: std::collections::HashSet<ino_t> =
                list_attr_dir(current_task).into_iter().map(|(ino, _, _)| ino).collect();
            assert_eq!(inos.len(), 6);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_attr_readdir_lists_entries() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let names: Vec<FsString> =
                list_attr_dir(current_task).into_iter().map(|(_, _, name)| name).collect();
            assert_eq!(names, ["current", "prev", "exec", "fscreate", "keycreate", "sockcreate"]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_attr_readdir_entry_types_are_reg() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            for (_, entry_type, name) in list_attr_dir(current_task) {
                assert_eq!(entry_type, DirectoryEntryType::REG, "{name}");
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_attr_readdir_ino_matches_lookup_ino() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            for (ino, _, name) in list_attr_dir(current_task) {
                let name = name.to_string();
                assert_eq!(attr_entry(current_task, &name).entry.node.ino, ino, "{name}");
            }
        })
        .await;
    }

    struct TestDirentSink {
        offset: off_t,
        max_entries: Option<usize>,
        entries: Vec<(FsString, DirectoryEntryType)>,
    }

    impl TestDirentSink {
        fn new(offset: off_t, max_entries: Option<usize>) -> Self {
            Self { offset, max_entries, entries: Vec::new() }
        }
    }

    impl DirentSink for TestDirentSink {
        fn add(
            &mut self,
            _inode_num: ino_t,
            offset: off_t,
            entry_type: DirectoryEntryType,
            name: &FsStr,
        ) -> Result<(), Errno> {
            if let Some(max) = self.max_entries {
                if self.entries.len() >= max {
                    return error!(ENOSPC);
                }
            }
            self.offset = offset;
            self.entries.push((name.into(), entry_type));
            Ok(())
        }

        fn offset(&self) -> off_t {
            self.offset
        }
    }

    #[::fuchsia::test]
    async fn test_fd_directory_file_ops_streaming_and_resume() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let target_file = anon_test_file(
                current_task,
                Box::new(BytesFile::new(b"".to_vec())),
                OpenFlags::RDONLY,
            );
            let _fd0 = current_task
                .files()
                .add(current_task, target_file.clone(), FdFlags::empty())
                .expect("add fd0");
            let fd1 = current_task
                .files()
                .add(current_task, target_file.clone(), FdFlags::empty())
                .expect("add fd1");
            let _fd2 = current_task
                .files()
                .add(current_task, target_file.clone(), FdFlags::empty())
                .expect("add fd2");
            // Create a gap at fd 1.
            current_task.files().close(fd1).expect("close fd1");

            let dir_file = current_task
                .open_file("/proc/self/fd".into(), OpenFlags::RDONLY | OpenFlags::DIRECTORY)
                .expect("open fd dir");

            // First batch stops after ".", "..", and "0".
            let mut sink1 = TestDirentSink::new(0, Some(3));
            assert_eq!(dir_file.readdir(current_task, &mut sink1), error!(ENOSPC));
            assert_eq!(
                sink1.entries,
                vec![
                    (FsString::from("."), DirectoryEntryType::DIR),
                    (FsString::from(".."), DirectoryEntryType::DIR),
                    (FsString::from("0"), DirectoryEntryType::LNK),
                ]
            );
            assert_eq!(sink1.offset(), 3);

            // Second batch resumes from offset 3 and emits "2".
            let mut sink2 = TestDirentSink::new(sink1.offset(), None);
            dir_file.readdir(current_task, &mut sink2).expect("readdir resume");
            assert_eq!(sink2.entries, vec![(FsString::from("2"), DirectoryEntryType::LNK)]);
            assert_eq!(sink2.offset(), 5);

            // Open more descriptors than `FD_READDIR_BATCH_SIZE` to exercise multi-batch iteration
            // and resuming mid-way through a subsequent batch.
            let mut expected_fds = vec![0i32, 2i32];
            for _ in 0..(FD_READDIR_BATCH_SIZE + 5) {
                let fd = current_task
                    .files()
                    .add(current_task, target_file.clone(), FdFlags::empty())
                    .expect("add extra fd");
                expected_fds.push(fd.raw());
            }
            expected_fds.sort_unstable();

            // Stop 2 FDs into the second `FD_READDIR_BATCH_SIZE` chunk (plus "." and "..").
            let first_chunk_len = FD_READDIR_BATCH_SIZE + 4;
            let mut sink3 = TestDirentSink::new(0, Some(first_chunk_len));
            assert_eq!(dir_file.readdir(current_task, &mut sink3), error!(ENOSPC));
            assert_eq!(sink3.entries.len(), first_chunk_len);

            let mut sink4 = TestDirentSink::new(sink3.offset(), None);
            dir_file.readdir(current_task, &mut sink4).expect("readdir multi-batch resume");

            let mut all_entries = sink3.entries;
            all_entries.extend(sink4.entries);
            let mut expected_entries = vec![
                (FsString::from("."), DirectoryEntryType::DIR),
                (FsString::from(".."), DirectoryEntryType::DIR),
            ];
            expected_entries.extend(
                expected_fds
                    .iter()
                    .map(|fd| (FsString::from(fd.to_string()), DirectoryEntryType::LNK)),
            );
            assert_eq!(all_entries, expected_entries);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_fd_readdir_zombie_emits_only_dots() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let fd_file = {
                let target_task = create_task(current_task.kernel(), "fd_zombie_target");
                let target_file = anon_test_file(
                    &target_task,
                    Box::new(BytesFile::new(b"".to_vec())),
                    OpenFlags::RDONLY,
                );
                let _fd = target_task
                    .files()
                    .add(&target_task, target_file, FdFlags::empty())
                    .expect("add fd");
                let target_pid = target_task.get_pid();
                let fd_file = current_task
                    .open_file(
                        format!("/proc/{target_pid}/fd").as_str().into(),
                        OpenFlags::RDONLY | OpenFlags::DIRECTORY,
                    )
                    .expect("open fd dir");
                target_task.write().set_exiting(ExitStatus::Exit(0));
                fd_file
            };
            let mut sink = TestDirentSink::new(0, None);
            fd_file.readdir(current_task, &mut sink).expect("readdir zombie fd dir");
            assert_eq!(
                sink.entries,
                vec![
                    (FsString::from("."), DirectoryEntryType::DIR),
                    (FsString::from(".."), DirectoryEntryType::DIR),
                ]
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_fd_readdir_reaped_task_returns_enoent() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            current_task
                .thread_group()
                .signal_actions
                .set(SIGCHLD, sigaction_t { sa_handler: SIG_IGN, ..Default::default() });
            let fd_file = {
                let target_task = create_task(current_task.kernel(), "fd_reaped_target");
                let target_pid = target_task.get_pid();
                let fd_file = current_task
                    .open_file(
                        format!("/proc/{target_pid}/fd").as_str().into(),
                        OpenFlags::RDONLY | OpenFlags::DIRECTORY,
                    )
                    .expect("open fd dir");
                target_task.write().set_exiting(ExitStatus::Exit(0));
                fd_file
            };
            let mut sink = TestDirentSink::new(0, None);
            assert_matches!(
                fd_file.readdir(current_task, &mut sink),
                Err(e) if e == errno!(ENOENT)
            );
            assert!(sink.entries.is_empty());
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_proc_pid_directory_cache_cleared_on_exit() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let task = create_task(current_task.kernel(), "test-task");
            let _entry = lookup_no_follow(&task, "/proc/self/stat").expect("lookup stat");
            let running_state = Arc::clone(task.running_state());
            assert!(running_state.proc_pid_directory_cache.lock().is_some());
            drop(task);
            assert!(running_state.proc_pid_directory_cache.lock().is_none());
            // Dropping `TaskRunningState` on a non-task thread after the task has exited must not
            // panic in the delayed releaser.
            std::thread::spawn(move || {
                drop(running_state);
            })
            .join()
            .expect("join thread");
        })
        .await;
    }

    /// Returns the path of `name` in `current_task`'s own `/proc/<pid>` directory.
    fn own_proc_path(current_task: &CurrentTask, name: &str) -> String {
        format!("/proc/{}/{name}", current_task.get_pid())
    }

    /// Adds a file opened with `flags` to `current_task`'s FD table and returns its FD.
    fn add_test_fd(current_task: &CurrentTask, flags: OpenFlags) -> FdNumber {
        let file = anon_test_file(current_task, Box::new(BytesFile::new(vec![])), flags);
        current_task.files().add(current_task, file, FdFlags::empty()).expect("add fd")
    }

    /// Makes `fd` refer to a new file opened with `flags`, and returns that file.
    fn replace_test_fd(current_task: &CurrentTask, fd: FdNumber, flags: OpenFlags) -> FileHandle {
        let file = anon_test_file(current_task, Box::new(BytesFile::new(vec![])), flags);
        current_task.files().insert(current_task, fd, file.clone()).expect("insert fd");
        file
    }

    /// Looks up `/proc/<pid>/fd/<fd>` for a new `O_RDONLY` FD, makes the FD refer to a file opened
    /// with `flags` while holding the entry, and returns the mode of the entry that a second
    /// lookup returns.
    fn fd_mode_after_replace(current_task: &CurrentTask, flags: OpenFlags) -> FileMode {
        let fd = add_test_fd(current_task, OpenFlags::RDONLY);
        let path = own_proc_path(current_task, &format!("fd/{}", fd.raw()));
        let _held = lookup_no_follow(current_task, &path).expect("lookup fd");
        replace_test_fd(current_task, fd, flags);
        lookup_no_follow(current_task, &path).expect("re-lookup fd").entry.node.info().mode
    }

    #[::fuchsia::test]
    async fn test_fd_entry_reused_after_fd_replace() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let fd = add_test_fd(current_task, OpenFlags::RDONLY);
            let path = own_proc_path(current_task, &format!("fd/{}", fd.raw()));
            let held = lookup_no_follow(current_task, &path).expect("lookup fd");
            replace_test_fd(current_task, fd, OpenFlags::RDWR);
            let relookup = lookup_no_follow(current_task, &path).expect("re-lookup fd");
            assert!(Arc::ptr_eq(&held.entry, &relookup.entry));
        })
        .await;
    }

    #[test_case(OpenFlags::RDWR, mode!(IFLNK, 0o700); "rdwr")]
    #[test_case(OpenFlags::PATH, mode!(IFLNK, 0o000); "opath")]
    #[::fuchsia::test]
    async fn test_fd_entry_mode_follows_replaced_file(flags: OpenFlags, expected: FileMode) {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            assert_eq!(fd_mode_after_replace(current_task, flags), expected);
        })
        .await;
    }

    #[test_case("fd")]
    #[test_case("fdinfo")]
    #[::fuchsia::test]
    async fn test_fd_dir_entry_relookup_after_close_fails(dir: &'static str) {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let fd = add_test_fd(current_task, OpenFlags::RDONLY);
            let path = own_proc_path(current_task, &format!("{dir}/{}", fd.raw()));
            let _held = lookup_no_follow(current_task, &path).expect("lookup entry");
            current_task.files().close(fd).expect("close fd");
            assert_matches!(lookup_no_follow(current_task, &path), Err(e) if e == errno!(ENOENT));
        })
        .await;
    }

    /// Creates a task with an open FD, looks up `/proc/<pid>/<subpath>` while it is alive, and
    /// exits the task (reaping it when `reap` is true, or leaving it as an unreaped zombie
    /// otherwise), returning the held [`NamespaceNode`] and the FD number.
    fn lookup_before_task_exit(
        current_task: &CurrentTask,
        reap: bool,
        subpath: impl FnOnce(FdNumber) -> String,
    ) -> (NamespaceNode, FdNumber) {
        if reap {
            current_task
                .thread_group()
                .signal_actions
                .set(SIGCHLD, sigaction_t { sa_handler: SIG_IGN, ..Default::default() });
        }
        let target_task = create_task(current_task.kernel(), "exited_target");
        let fd = add_test_fd(&target_task, OpenFlags::RDONLY);
        let path = format!("/proc/{}/{}", target_task.get_pid(), subpath(fd));
        let held = lookup_no_follow(current_task, &path).expect("lookup before exit");
        target_task.write().set_exiting(ExitStatus::Exit(0));
        (held, fd)
    }

    #[test_case("fd", false; "fd_zombie")]
    #[test_case("fdinfo", false; "fdinfo_zombie")]
    #[test_case("fd", true; "fd_reaped")]
    #[test_case("fdinfo", true; "fdinfo_reaped")]
    #[::fuchsia::test]
    async fn test_fd_dir_lookup_exited_task_returns_enoent(dir: &'static str, reap: bool) {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let (held_dir, fd) = lookup_before_task_exit(current_task, reap, |_| dir.to_string());
            let fd_name = fd.raw().to_string();
            assert_matches!(
                held_dir.lookup_child(
                    current_task,
                    &mut LookupContext::default(),
                    fd_name.as_str().into(),
                ),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    #[test_case(false, errno!(ENOENT); "zombie")]
    #[test_case(true, errno!(EACCES); "reaped")]
    #[::fuchsia::test]
    async fn test_fd_symlink_readlink_exited_task(reap: bool, expected: Errno) {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let (held_link, _fd) =
                lookup_before_task_exit(current_task, reap, |fd| format!("fd/{}", fd.raw()));
            assert_matches!(held_link.readlink(current_task), Err(e) if e == expected);
        })
        .await;
    }

    /// Looks up `path`, changes the creds with `set_distinct_creds` while holding the entry, and
    /// returns the owner of the entry that a second lookup returns, without refreshing it on stat.
    fn owner_after_relookup(current_task: &CurrentTask, path: &str) -> (uid_t, gid_t) {
        let _held = lookup_no_follow(current_task, path).expect(path);
        set_distinct_creds(current_task);
        owner(&lookup_no_follow(current_task, path).expect(path))
    }

    #[test_case("stat"; "pid_stat")]
    #[test_case("fd"; "fd_dir")]
    #[test_case("ns/mnt"; "ns_link")]
    #[test_case("attr/current"; "attr_entry")]
    #[::fuchsia::test]
    async fn test_pid_entry_owner_refreshes_on_relookup(subpath: &'static str) {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let path = own_proc_path(current_task, subpath);
            assert_eq!(owner_after_relookup(current_task, &path), (2000, 2001));
        })
        .await;
    }

    #[test_case("fd")]
    #[test_case("fdinfo")]
    #[::fuchsia::test]
    async fn test_fd_dir_entry_owner_refreshes_on_relookup(dir: &'static str) {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let fd = add_test_fd(current_task, OpenFlags::RDONLY);
            let path = own_proc_path(current_task, &format!("{dir}/{}", fd.raw()));
            assert_eq!(owner_after_relookup(current_task, &path), (2000, 2001));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_tid_dir_child_owner_refreshes_on_relookup() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let path =
                own_proc_path(current_task, &format!("task/{}/stat", current_task.get_tid()));
            assert_eq!(owner_after_relookup(current_task, &path), (2000, 2001));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_ns_target_owner_stays_root_on_relookup() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let ns_id = current_task.fs().namespace().id;
            let path = own_proc_path(current_task, &format!("ns/mnt:[{ns_id}]"));
            assert_eq!(owner_after_relookup(current_task, &path), (0, 0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_tid_entry_relookup_after_thread_exit_fails() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let (tid_path, _held) = {
                let thread = current_task
                    .clone_task_for_test((CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64, None);
                let tid_path = own_proc_path(current_task, &format!("task/{}", thread.get_tid()));
                let held = lookup_no_follow(current_task, &tid_path).expect("lookup tid");
                thread.write().set_exiting(ExitStatus::Exit(0));
                (tid_path, held)
            };
            // The process is still alive, so this fails only if the entry has thread-scoped ops.
            assert_matches!(
                lookup_no_follow(current_task, &tid_path),
                Err(e) if e == errno!(ENOENT)
            );
        })
        .await;
    }

    fn hidepid_invisible_options() -> FileSystemOptions {
        let params = MountParams::parse(b"hidepid=invisible".into()).expect("parse mount params");
        FileSystemOptions { params, ..Default::default() }
    }

    #[::fuchsia::test]
    async fn test_hidepid_invisible_hides_held_pid_dir() {
        spawn_kernel_with_procfs_and_options(
            KernelFeatures::default(),
            hidepid_invisible_options(),
            async move |current_task| {
                let target_task = create_task(current_task.kernel(), "hidden_target");
                let path = format!("/proc/{}", target_task.get_pid());
                let _held = lookup_no_follow(current_task, &path).expect("lookup pid dir");
                current_task.set_creds(Credentials::with_ids(1000, 1000));
                assert_matches!(
                    lookup_no_follow(current_task, &path),
                    Err(e) if e == errno!(ENOENT)
                );
                target_task.write().set_exiting(ExitStatus::Exit(0));
            },
        )
        .await;
    }

    #[::fuchsia::test]
    async fn test_procfs_dcache_proc_self() {
        spawn_kernel_with_cached_procfs(async move |current_task| {
            assert_cached_lookup(current_task, "/proc/self");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_procfs_dcache_proc_self_resolves_per_task() {
        spawn_kernel_with_cached_procfs(async move |current_task| {
            // Cache `/proc/self` and this task's `/proc/<pid>` subtree.
            assert_cached_lookup(current_task, "/proc/self");
            current_task.lookup_path_from_root("/proc/self/stat".into()).expect("own stat");
            let child = current_task.clone_task_for_test(0, Some(SIGCHLD));
            let resolved = child.lookup_path_from_root("/proc/self".into()).expect("child self");
            let own_dir = lookup_no_follow(&child, &format!("/proc/{}", child.get_pid()))
                .expect("child pid dir");
            assert!(Arc::ptr_eq(&resolved.entry, &own_dir.entry));
            child.write().set_exiting(ExitStatus::Exit(0));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_procfs_dcache_fd_entry_retained_across_fd_replace() {
        spawn_kernel_with_cached_procfs(async move |current_task| {
            let fd = add_test_fd(current_task, OpenFlags::RDONLY);
            let path = own_proc_path(current_task, &format!("fd/{}", fd.raw()));
            let weak = Arc::downgrade(&assert_cached_lookup(current_task, &path).entry);
            replace_test_fd(current_task, fd, OpenFlags::RDWR);
            let relookup = lookup_no_follow(current_task, &path).expect("re-lookup fd");
            assert!(Arc::ptr_eq(&weak.upgrade().expect("retained"), &relookup.entry));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_procfs_dcache_fd_readlink_follows_replaced_file() {
        spawn_kernel_with_cached_procfs(async move |current_task| {
            let fd = add_test_fd(current_task, OpenFlags::RDONLY);
            let path = own_proc_path(current_task, &format!("fd/{}", fd.raw()));
            assert_cached_lookup(current_task, &path);
            let file = replace_test_fd(current_task, fd, OpenFlags::RDWR);
            let entry = lookup_no_follow(current_task, &path).expect("re-lookup fd");
            assert_matches!(
                entry.readlink(current_task).expect("readlink"),
                SymlinkTarget::Node(target) if Arc::ptr_eq(&target.entry, &file.name.entry)
            );
        })
        .await;
    }

    #[test_case("fd")]
    #[test_case("fdinfo")]
    #[::fuchsia::test]
    async fn test_procfs_dcache_fd_close_evicts_entry(dir: &'static str) {
        spawn_kernel_with_cached_procfs(async move |current_task| {
            let fd = add_test_fd(current_task, OpenFlags::RDONLY);
            let path = own_proc_path(current_task, &format!("{dir}/{}", fd.raw()));
            let close = || current_task.files().close(fd).expect("close fd");
            assert!(!cached_entry_survives(current_task, &path, close));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_procfs_dcache_fd_dir_owner_refreshes_after_setuid() {
        spawn_kernel_with_cached_procfs(async move |current_task| {
            let path = own_proc_path(current_task, "fd");
            assert_cached_lookup(current_task, &path);
            set_distinct_creds(current_task);
            assert_eq!(owner(&lookup_no_follow(current_task, &path).expect("fd")), (2000, 2001));
        })
        .await;
    }

    /// Caches `<task_dir>/fd/<n>` as root, drops all capabilities with `set_creds` while keeping
    /// the task dumpable, and returns the result of walking `<task_dir>/fd/<n>` again through the
    /// cached `0500` `fd` directory.
    fn lookup_cached_fd_after_privilege_drop(
        current_task: &CurrentTask,
        task_dir: &str,
    ) -> Result<NamespaceNode, Errno> {
        let fd = add_test_fd(current_task, OpenFlags::RDONLY);
        let path = format!("{task_dir}/fd/{}", fd.raw());
        assert_cached_lookup(current_task, &path);
        current_task.set_creds(Credentials::with_ids(1000, 1000));
        current_task.mm().expect("mm").dumpable.store(DumpPolicy::User);
        lookup_no_follow(current_task, &path)
    }

    #[test_case("/proc/self"; "proc_self")]
    #[test_case("/proc/thread-self"; "proc_thread_self")]
    #[::fuchsia::test]
    async fn test_procfs_dcache_fd_traversal_after_setuid(task_dir: &'static str) {
        spawn_kernel_with_cached_procfs(async move |current_task| {
            assert_matches!(lookup_cached_fd_after_privilege_drop(current_task, task_dir), Ok(_));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_procfs_dcache_unshare_evicts_stale_ns_target() {
        spawn_kernel_with_cached_procfs(async move |current_task| {
            let old_id = current_task.fs().namespace().id;
            let path = own_proc_path(current_task, &format!("ns/mnt:[{old_id}]"));
            let unshare = || current_task.fs().unshare_namespace();
            assert!(!cached_entry_survives(current_task, &path, unshare));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_procfs_dcache_task_exit_evicts_pid_entry() {
        spawn_kernel_with_cached_procfs(async move |current_task| {
            // Auto-reap child tasks when they exit so dropping AutoReleasableTask reaps them.
            current_task
                .thread_group()
                .signal_actions
                .set(SIGCHLD, sigaction_t { sa_handler: SIG_IGN, ..Default::default() });
            let target_task = create_task(current_task.kernel(), "exit_target");
            let path = format!("/proc/{}", target_task.get_pid());
            // Moving `target_task` into the closure reaps it when the closure returns.
            let exit = move || target_task.write().set_exiting(ExitStatus::Exit(0));
            assert!(!cached_entry_survives(current_task, &path, exit));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_procfs_dcache_thread_exit_evicts_tid_entry() {
        spawn_kernel_with_cached_procfs(async move |current_task| {
            let thread = current_task
                .clone_task_for_test((CLONE_THREAD | CLONE_VM | CLONE_SIGHAND) as u64, None);
            let path = own_proc_path(current_task, &format!("task/{}", thread.get_tid()));
            let exit = move || thread.write().set_exiting(ExitStatus::Exit(0));
            assert!(!cached_entry_survives(current_task, &path, exit));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_procfs_dcache_hidepid_invisible_hides_cached_pid_dir() {
        spawn_kernel_with_cached_procfs_and_options(
            hidepid_invisible_options(),
            async move |current_task| {
                let target_task = create_task(current_task.kernel(), "hidden_target");
                let path = format!("/proc/{}", target_task.get_pid());
                assert_cached_lookup(current_task, &path);
                current_task.set_creds(Credentials::with_ids(1000, 1000));
                assert_matches!(lookup_no_follow(current_task, &path), Err(e) if e == errno!(ENOENT));
                target_task.write().set_exiting(ExitStatus::Exit(0));
            },
        )
        .await;
    }

    #[::fuchsia::test]
    async fn test_kthread_stat_and_status_report_sleeping() {
        spawn_kernel_and_run(async move |current_task| {
            let (task_sender, task_receiver) = std::sync::mpsc::channel();
            let (release_sender, release_receiver) = std::sync::mpsc::channel::<()>();
            let req = SpawnRequestBuilder::new()
                .with_sync_closure(move |kthread| {
                    task_sender.send(kthread.weak_task()).unwrap();
                    let _ = release_receiver.recv();
                })
                .build();
            current_task.kernel().kthreads.spawner().spawn_from_request(req);

            let task = task_receiver.recv().unwrap().upgrade().expect("kthread is alive");
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
            while task.state_code() != TaskStateCode::Sleeping
                && std::time::Instant::now() < deadline
            {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }

            let target = TaskContainer::from_task(&task);
            let stat_file = anon_test_file(
                current_task,
                Box::new(DynamicFile::new(StatFile { target: target.clone() })),
                OpenFlags::RDONLY,
            );
            let status_file = anon_test_file(
                current_task,
                Box::new(DynamicFile::new(StatusFile { target })),
                OpenFlags::RDONLY,
            );

            let mut stat_buf = VecOutputBuffer::new(512);
            let stat_n = stat_file.read_at(current_task, 0, &mut stat_buf).expect("read stat");
            let stat_text = std::str::from_utf8(&stat_buf.data()[..stat_n]).unwrap().to_string();

            let mut status_buf = VecOutputBuffer::new(2048);
            let status_n =
                status_file.read_at(current_task, 0, &mut status_buf).expect("read status");
            let status_text =
                std::str::from_utf8(&status_buf.data()[..status_n]).unwrap().to_string();

            let _ = release_sender.send(());
            assert!(stat_text.contains(") S "), "{stat_text:?}");
            assert!(status_text.lines().any(|l| l == "State:\tS (sleeping)"), "{status_text:?}");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_proc_stat_preserves_tty_nr_for_non_leader_zombie() {
        spawn_kernel_with_procfs(KernelFeatures::default(), async move |current_task| {
            let tty_state = Arc::new(TtyState::default());
            let terminal = tty_state
                .get_next_terminal(current_task.fs().root().entry.clone(), FsCred::root())
                .expect("get_next_terminal");
            let expected_tty_nr = terminal.device().bits() as i32;

            let leader = create_task(current_task.kernel(), "leader");
            leader
                .thread_group()
                .set_controlling_terminal(&leader, &terminal, TerminalSide::Replica, false, true)
                .expect("set_controlling_terminal");

            let child = leader.clone_task_for_test(0, Some(SIGCHLD));
            let child_pid = child.get_pid();
            let child_tg = Arc::clone(child.thread_group());
            // Drop the child so it exits and remains an unreaped zombie under `leader`.
            std::mem::drop(child);
            assert!(child_tg.controlling_terminal().is_none());

            let stat_file = current_task
                .open_file(format!("/proc/{child_pid}/stat").as_str().into(), OpenFlags::RDONLY)
                .expect("open zombie stat");
            let mut buf = VecOutputBuffer::new(1024);
            let n = stat_file.read_at(current_task, 0, &mut buf).expect("read zombie stat");
            let text = String::from_utf8(buf.data()[..n].to_vec()).expect("utf8");
            // Fields after ") " start at field 3 (state), so index 4 is field 7 (tty_nr).
            let after_comm = text.rsplit_once(") ").expect("stat comm").1;
            let fields: Vec<&str> = after_comm.split_whitespace().collect();
            assert_eq!(fields[0], "Z");
            assert_eq!(fields[4].parse::<i32>().expect("tty_nr"), expected_tty_nr);

            std::mem::drop(child_tg);
            std::mem::drop(leader);
        })
        .await;
    }
}
