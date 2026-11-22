// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

//! Tracks the `CATEGORY_STARNIX_SYSCALLS` or `CATEGORY_STARNIX_SYSCALLS_FS` duration of the syscall
//! running on the current thread, so that code inside the syscall can end that duration early.
//!
//! `TraceMarkerFile::write` uses this to end the syscall's slice before it records an
//! `ftrace/print` (`atrace`) event. Otherwise the syscall slice ends after the `atrace` event on
//! the same thread track, and Perfetto's strict slice nesting truncates the `atrace` slice. Linux
//! `ftrace` traces avoid this the same way, in Perfetto's
//! `SyscallTracker::MaybeTruncateOngoingWriteSlice`.

use fuchsia_trace::{TraceCategoryContext, trace_site_t};
use starnix_logging::{CATEGORY_STARNIX_SYSCALLS, CATEGORY_STARNIX_SYSCALLS_FS};
use starnix_syscalls::decls::SyscallDecl;
use std::cell::Cell;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SyscallTraceKind {
    /// Emitted under `CATEGORY_STARNIX_SYSCALLS`.
    Syscalls,
    /// Emitted under `CATEGORY_STARNIX_SYSCALLS_FS` when `confirmed` is `true` (if enabled), or
    /// under `CATEGORY_STARNIX_SYSCALLS` if `fallback_to_syscalls` is `true`.
    Fs { confirmed: bool, fallback_to_syscalls: bool },
}

#[derive(Clone, Copy)]
struct ActiveSyscallTrace {
    name: &'static str,
    start_time: zx::BootTicks,
    kind: SyscallTraceKind,
}

thread_local! {
    /// The active syscall duration that is open on the current thread, if any.
    ///
    /// This is a `const`-initialized thread-local without a destructor, so accessing it is cheap.
    /// A `CurrentTask` runs syscalls only on its own thread, so this holds the state of that task's
    /// current syscall.
    static SYSCALL_TRACE: Cell<Option<ActiveSyscallTrace>> = const { Cell::new(None) };
}

/// Ends the syscall duration started by [`begin_syscall_trace`] when dropped, unless
/// [`complete_syscall_trace`] already ended it.
#[must_use = "the syscall duration ends when the scope is dropped"]
pub struct SyscallTraceScope(());

impl Drop for SyscallTraceScope {
    fn drop(&mut self) {
        complete_syscall_trace();
    }
}

/// Starts a duration for `syscall_decl` on the current thread under its trace category
/// (`CATEGORY_STARNIX_SYSCALLS_FS` or `CATEGORY_STARNIX_SYSCALLS`).
///
/// Returns `None`, without calling `syscall_decl.trace_name()`, if neither applicable category is
/// enabled. Otherwise the duration ends when the returned scope is dropped, or earlier at a call to
/// [`complete_syscall_trace`].
#[inline]
pub fn begin_syscall_trace(syscall_decl: SyscallDecl) -> Option<SyscallTraceScope> {
    // The context is dropped right away. `complete_syscall_trace` acquires it again, so the trace
    // buffers can be flushed while the syscall runs, as with `fuchsia_trace::duration!`.
    static SYSCALLS_CACHE: trace_site_t = trace_site_t::new(0);
    static FS_CACHE: trace_site_t = trace_site_t::new(0);
    let syscalls_enabled =
        TraceCategoryContext::acquire_cached(CATEGORY_STARNIX_SYSCALLS, &SYSCALLS_CACHE).is_some();

    let kind = if syscall_decl.is_fs()
        && TraceCategoryContext::acquire_cached(CATEGORY_STARNIX_SYSCALLS_FS, &FS_CACHE).is_some()
    {
        // Filesystem syscalls start unconfirmed under `CATEGORY_STARNIX_SYSCALLS_FS` and are
        // confirmed when operating on a storage-backed filesystem (such as `ext4`). Unconfirmed
        // filesystem syscalls on in-memory or pseudo filesystems (such as `tmpfs` or `procfs`, or
        // pipes/sockets) fall back to `CATEGORY_STARNIX_SYSCALLS` if enabled.
        SyscallTraceKind::Fs { confirmed: false, fallback_to_syscalls: syscalls_enabled }
    } else if syscalls_enabled {
        SyscallTraceKind::Syscalls
    } else {
        return None;
    };

    SYSCALL_TRACE.set(Some(ActiveSyscallTrace {
        name: syscall_decl.trace_name(),
        start_time: zx::BootTicks::get(),
        kind,
    }));
    Some(SyscallTraceScope(()))
}

/// Confirms that the current syscall operates on a storage-backed filesystem, so a
/// `CATEGORY_STARNIX_SYSCALLS_FS` syscall duration will be emitted when completed.
#[inline]
pub fn confirm_fs_syscall_trace() {
    confirm_fs_syscall_trace_if(|| true);
}

/// Confirms an unconfirmed `CATEGORY_STARNIX_SYSCALLS_FS` syscall duration on the current thread if
/// `is_storage_backed()` returns `true`.
///
/// Checks whether an unconfirmed trace is active on the current thread before invoking
/// `is_storage_backed()`, avoiding virtual dispatch on `FileSystemOps::is_storage_backed` when
/// tracing is disabled or the current syscall trace is already confirmed.
#[inline]
pub fn confirm_fs_syscall_trace_if(is_storage_backed: impl FnOnce() -> bool) {
    SYSCALL_TRACE.with(|cell| {
        if let Some(mut trace) = cell.get() {
            if let SyscallTraceKind::Fs { ref mut confirmed, .. } = trace.kind {
                if !*confirmed && is_storage_backed() {
                    *confirmed = true;
                    cell.set(Some(trace));
                }
            }
        }
    });
}

/// Ends the syscall duration that is open on the current thread, if any, and writes it to the
/// trace.
///
/// Called early by `TraceMarkerFile::write`, right before it records an `ftrace/print` event, so
/// that the syscall slice ends at or before the event's timestamp.
#[inline]
pub fn complete_syscall_trace() {
    if let Some(trace) = SYSCALL_TRACE.take() {
        static SYSCALLS_CACHE: trace_site_t = trace_site_t::new(0);
        static FS_CACHE: trace_site_t = trace_site_t::new(0);
        let category = match trace.kind {
            SyscallTraceKind::Fs { confirmed: true, .. }
                if TraceCategoryContext::acquire_cached(
                    CATEGORY_STARNIX_SYSCALLS_FS,
                    &FS_CACHE,
                )
                .is_some() =>
            {
                CATEGORY_STARNIX_SYSCALLS_FS
            }
            SyscallTraceKind::Syscalls
            | SyscallTraceKind::Fs { fallback_to_syscalls: true, .. }
                if TraceCategoryContext::acquire_cached(
                    CATEGORY_STARNIX_SYSCALLS,
                    &SYSCALLS_CACHE,
                )
                .is_some() =>
            {
                CATEGORY_STARNIX_SYSCALLS
            }
            _ => return,
        };
        fuchsia_trace::complete_duration(category, trace.name, trace.start_time, &[]);
    }
}

/// Opens a syscall duration on the current thread even if `CATEGORY_STARNIX_SYSCALLS` is
/// disabled, so tests can check that code ends it. Drop the returned scope to clear it.
pub fn begin_syscall_trace_for_testing(name: &'static str) -> SyscallTraceScope {
    SYSCALL_TRACE.set(Some(ActiveSyscallTrace {
        name,
        start_time: zx::BootTicks::get(),
        kind: SyscallTraceKind::Syscalls,
    }));
    SyscallTraceScope(())
}

/// Returns whether a syscall duration is open on the current thread.
pub fn has_active_syscall_trace_for_testing() -> bool {
    SYSCALL_TRACE.with(|trace| trace.get().is_some())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::tmpfs::TmpFs;
    use crate::testing::{spawn_kernel_and_run, spawn_kernel_and_run_with_pkgfs};
    use crate::vfs::pipe::new_pipe;
    use crate::vfs::{FdFlags, FdNumber, LookupContext, WhatToMount};
    use starnix_syscalls::SyscallArg;
    use starnix_types::arch::ArchWidth;
    use starnix_uapi::file_mode::FileMode;
    use starnix_uapi::mount_flags::MountpointFlags;
    use starnix_uapi::open_flags::OpenFlags;
    use starnix_uapi::user_address::UserAddress;
    use starnix_uapi::vfs::ResolveFlags;

    fn begin_unconfirmed_fs_trace_for_testing(name: &'static str) -> SyscallTraceScope {
        SYSCALL_TRACE.set(Some(ActiveSyscallTrace {
            name,
            start_time: zx::BootTicks::get(),
            kind: SyscallTraceKind::Fs { confirmed: false, fallback_to_syscalls: false },
        }));
        SyscallTraceScope(())
    }

    fn active_syscall_trace_kind() -> Option<SyscallTraceKind> {
        SYSCALL_TRACE.with(|trace| trace.get().map(|t| t.kind))
    }

    fn is_active_fs_trace_confirmed() -> bool {
        matches!(active_syscall_trace_kind(), Some(SyscallTraceKind::Fs { confirmed: true, .. }))
    }

    #[::fuchsia::test]
    fn test_confirm_fs_syscall_trace() {
        assert_eq!(active_syscall_trace_kind(), None);

        // No-op when no trace is active; closure is not evaluated.
        let mut called = false;
        confirm_fs_syscall_trace_if(|| {
            called = true;
            true
        });
        assert!(!called);

        let _scope = begin_unconfirmed_fs_trace_for_testing("read");
        assert_eq!(
            active_syscall_trace_kind(),
            Some(SyscallTraceKind::Fs { confirmed: false, fallback_to_syscalls: false })
        );

        // Non-storage-backed check leaves trace unconfirmed.
        confirm_fs_syscall_trace_if(|| false);
        assert_eq!(
            active_syscall_trace_kind(),
            Some(SyscallTraceKind::Fs { confirmed: false, fallback_to_syscalls: false })
        );

        // Storage-backed check confirms the trace.
        confirm_fs_syscall_trace_if(|| true);
        assert_eq!(
            active_syscall_trace_kind(),
            Some(SyscallTraceKind::Fs { confirmed: true, fallback_to_syscalls: false })
        );

        // Once confirmed, subsequent checks short-circuit without calling the predicate.
        let mut called_after_confirm = false;
        confirm_fs_syscall_trace_if(|| {
            called_after_confirm = true;
            true
        });
        assert!(!called_after_confirm);
    }

    #[::fuchsia::test]
    fn test_syscall_decl_is_fs() {
        let read_decl = SyscallDecl::from_number(starnix_uapi::__NR_read as u64, ArchWidth::Arch64);
        assert!(read_decl.is_fs());

        let openat_decl =
            SyscallDecl::from_number(starnix_uapi::__NR_openat as u64, ArchWidth::Arch64);
        assert!(openat_decl.is_fs());

        let execve_decl =
            SyscallDecl::from_number(starnix_uapi::__NR_execve as u64, ArchWidth::Arch64);
        assert!(execve_decl.is_fs());

        let execveat_decl =
            SyscallDecl::from_number(starnix_uapi::__NR_execveat as u64, ArchWidth::Arch64);
        assert!(execveat_decl.is_fs());

        let inotify_add_watch_decl = SyscallDecl::from_number(
            starnix_uapi::__NR_inotify_add_watch as u64,
            ArchWidth::Arch64,
        );
        assert!(inotify_add_watch_decl.is_fs());

        let io_uring_enter_decl =
            SyscallDecl::from_number(starnix_uapi::__NR_io_uring_enter as u64, ArchWidth::Arch64);
        assert!(io_uring_enter_decl.is_fs());

        let ioctl_decl =
            SyscallDecl::from_number(starnix_uapi::__NR_ioctl as u64, ArchWidth::Arch64);
        assert!(ioctl_decl.is_fs());

        let mmap_decl = SyscallDecl::from_number(starnix_uapi::__NR_mmap as u64, ArchWidth::Arch64);
        assert!(mmap_decl.is_fs());

        let getpid_decl =
            SyscallDecl::from_number(starnix_uapi::__NR_getpid as u64, ArchWidth::Arch64);
        assert!(!getpid_decl.is_fs());

        let umask_decl =
            SyscallDecl::from_number(starnix_uapi::__NR_umask as u64, ArchWidth::Arch64);
        assert!(!umask_decl.is_fs());

        for num in 0..512 {
            let decl = SyscallDecl::from_number(num, ArchWidth::Arch64);
            if decl.is_fs() {
                assert_ne!(
                    decl.trace_name(),
                    "<unknown syscall>",
                    "Arch64 FS syscall {num} is missing a trace_name"
                );
            }
        }

        #[cfg(target_arch = "aarch64")]
        {
            for nr in [
                starnix_uapi::arch32::__NR__llseek,
                starnix_uapi::arch32::__NR_fstatat64,
                starnix_uapi::arch32::__NR_chmod,
                starnix_uapi::arch32::__NR_creat,
                starnix_uapi::arch32::__NR_execve,
                starnix_uapi::arch32::__NR_execveat,
                starnix_uapi::arch32::__NR_inotify_add_watch,
                starnix_uapi::arch32::__NR_io_uring_enter,
                starnix_uapi::arch32::__NR_ioctl,
                starnix_uapi::arch32::__NR_link,
                starnix_uapi::arch32::__NR_mmap2,
                starnix_uapi::arch32::__NR_readlink,
                starnix_uapi::arch32::__NR_rename,
                starnix_uapi::arch32::__NR_rmdir,
                starnix_uapi::arch32::__NR_symlink,
            ] {
                let decl = SyscallDecl::from_number(nr as u64, ArchWidth::Arch32);
                assert!(decl.is_fs(), "expected arch32 syscall {nr} to be is_fs()");
                assert_ne!(decl.trace_name(), "<unknown syscall>");
            }

            for nr in [starnix_uapi::arch32::__NR_getpid, starnix_uapi::arch32::__NR_umask] {
                let decl = SyscallDecl::from_number(nr as u64, ArchWidth::Arch32);
                assert!(!decl.is_fs(), "expected arch32 syscall {nr} not to be is_fs()");
            }

            for num in 0..512 {
                let decl = SyscallDecl::from_number(num, ArchWidth::Arch32);
                if decl.is_fs() {
                    assert_ne!(
                        decl.trace_name(),
                        "<unknown syscall>",
                        "Arch32 FS syscall {num} is missing a trace_name"
                    );
                }
            }
        }
    }

    #[::fuchsia::test]
    async fn test_vfs_confirmation_tmpfs_and_pipe() {
        spawn_kernel_and_run(async |current_task| {
            // Creating and looking up files on TmpFs should not confirm `starnix:syscalls:fs`.
            let tmp_fd = {
                let _scope = begin_unconfirmed_fs_trace_for_testing("openat");
                let file = current_task
                    .open_file_at(
                        FdNumber::AT_FDCWD,
                        "tmpfile.txt".into(),
                        OpenFlags::RDWR | OpenFlags::CREAT,
                        FileMode::ALLOW_ALL,
                        ResolveFlags::empty(),
                    )
                    .expect("create tmpfs file");
                assert!(!is_active_fs_trace_confirmed());
                current_task.add_file(file, FdFlags::empty()).expect("add tmpfs fd")
            };

            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("lseek");
                crate::vfs::syscalls::sys_lseek(current_task, tmp_fd, 0, starnix_uapi::SEEK_CUR)
                    .expect("lseek tmpfs fd");
                assert!(!is_active_fs_trace_confirmed());
            }

            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("ioctl");
                let _ =
                    crate::vfs::syscalls::sys_ioctl(current_task, tmp_fd, 0, SyscallArg::default());
                assert!(!is_active_fs_trace_confirmed());
            }

            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("mmap");
                crate::mm::syscalls::do_mmap(
                    current_task,
                    UserAddress::NULL,
                    *crate::mm::PAGE_SIZE as usize,
                    starnix_uapi::PROT_READ,
                    starnix_uapi::MAP_PRIVATE | starnix_uapi::MAP_ANONYMOUS,
                    FdNumber::from_raw(-1),
                    0,
                )
                .expect("anonymous mmap");
                assert!(!is_active_fs_trace_confirmed());
            }

            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("mmap");
                crate::mm::syscalls::do_mmap(
                    current_task,
                    UserAddress::NULL,
                    *crate::mm::PAGE_SIZE as usize,
                    starnix_uapi::PROT_READ,
                    starnix_uapi::MAP_PRIVATE,
                    tmp_fd,
                    0,
                )
                .expect("tmpfs mmap");
                assert!(!is_active_fs_trace_confirmed());
            }

            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("openat");
                assert!(current_task.open_file("missing.txt".into(), OpenFlags::RDONLY).is_err());
                assert!(!is_active_fs_trace_confirmed());
            }

            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("execve");
                let _ = current_task.open_file_for_exec(
                    FdNumber::AT_FDCWD,
                    "tmpfile.txt".into(),
                    OpenFlags::RDONLY,
                );
                assert!(!is_active_fs_trace_confirmed());
            }

            // Pipe file descriptors should not confirm `starnix:syscalls:fs`.
            let (read_pipe, _write_pipe) = new_pipe(current_task).expect("new_pipe");
            let pipe_fd = current_task.add_file(read_pipe, FdFlags::empty()).expect("add pipe fd");
            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("lseek");
                let _ = crate::vfs::syscalls::sys_lseek(
                    current_task,
                    pipe_fd,
                    0,
                    starnix_uapi::SEEK_CUR,
                );
                assert!(!is_active_fs_trace_confirmed());
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_vfs_confirmation_pkgfs_and_submount() {
        spawn_kernel_and_run_with_pkgfs(async |current_task| {
            // Opening a file on storage-backed `pkgfs` (`RemoteFs`) confirms `starnix:syscalls:fs`.
            let pkg_fd = {
                let _scope = begin_unconfirmed_fs_trace_for_testing("openat");
                let file = current_task
                    .open_file("data/testfile.txt".into(), OpenFlags::RDONLY)
                    .expect("open pkgfs file");
                assert!(is_active_fs_trace_confirmed());
                current_task.add_file(file, FdFlags::empty()).expect("add pkgfs fd")
            };

            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("execve");
                let _ = current_task.open_file_for_exec(
                    FdNumber::AT_FDCWD,
                    "data/testfile.txt".into(),
                    OpenFlags::RDONLY,
                );
                assert!(is_active_fs_trace_confirmed());
            }

            // Operating on a storage-backed fd confirms `starnix:syscalls:fs`.
            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("lseek");
                crate::vfs::syscalls::sys_lseek(current_task, pkg_fd, 0, starnix_uapi::SEEK_CUR)
                    .expect("lseek pkgfs fd");
                assert!(is_active_fs_trace_confirmed());
            }
            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("ioctl");
                let _ =
                    crate::vfs::syscalls::sys_ioctl(current_task, pkg_fd, 0, SyscallArg::default());
                assert!(is_active_fs_trace_confirmed());
            }
            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("mmap");
                crate::mm::syscalls::do_mmap(
                    current_task,
                    UserAddress::NULL,
                    *crate::mm::PAGE_SIZE as usize,
                    starnix_uapi::PROT_READ,
                    starnix_uapi::MAP_PRIVATE,
                    pkg_fd,
                    0,
                )
                .expect("pkgfs mmap");
                assert!(is_active_fs_trace_confirmed());
            }
            {
                // `FIOCLEX` only updates `FdTable` flags and should not confirm.
                let _scope = begin_unconfirmed_fs_trace_for_testing("ioctl");
                crate::vfs::syscalls::sys_ioctl(
                    current_task,
                    pkg_fd,
                    starnix_uapi::FIOCLEX,
                    SyscallArg::default(),
                )
                .expect("FIOCLEX");
                assert!(!is_active_fs_trace_confirmed());
            }

            // Failed lookups on a storage-backed parent or intermediate directory confirm.
            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("openat");
                assert!(
                    current_task.open_file("data/no_such_file".into(), OpenFlags::RDONLY).is_err()
                );
                assert!(is_active_fs_trace_confirmed());
            }
            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("openat");
                assert!(
                    current_task.open_file("no_such_dir/file".into(), OpenFlags::RDONLY).is_err()
                );
                assert!(is_active_fs_trace_confirmed());
            }

            // `ENOTDIR` on a storage-backed `dir_fd` in `resolve_dir_fd` confirms.
            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("openat");
                assert!(
                    current_task
                        .open_file_at(
                            pkg_fd,
                            "child".into(),
                            OpenFlags::RDONLY,
                            FileMode::default(),
                            ResolveFlags::empty(),
                        )
                        .is_err()
                );
                assert!(is_active_fs_trace_confirmed());
            }

            // Traversing from a storage-backed `dir_fd` into a `TmpFs` submount leaves the trace
            // unconfirmed.
            let root_file =
                current_task.open_file("/".into(), OpenFlags::RDONLY | OpenFlags::PATH).unwrap();
            let root_fd = current_task.add_file(root_file, FdFlags::empty()).unwrap();
            let mut context = LookupContext::default();
            let bin_dir = current_task
                .fs()
                .root()
                .lookup_child(current_task, &mut context, "bin".into())
                .expect("lookup bin");
            bin_dir
                .mount(
                    WhatToMount::Fs(TmpFs::new_fs(current_task.kernel())),
                    MountpointFlags::empty(),
                )
                .expect("mount tmpfs on bin");

            {
                let _scope = begin_unconfirmed_fs_trace_for_testing("openat");
                let _tmp_in_submount = current_task
                    .open_file_at(
                        root_fd,
                        "bin/submount_tmp.txt".into(),
                        OpenFlags::RDWR | OpenFlags::CREAT,
                        FileMode::ALLOW_ALL,
                        ResolveFlags::empty(),
                    )
                    .expect("create file in tmpfs submount");
                assert!(!is_active_fs_trace_confirmed());
            }
        })
        .await;
    }
}
