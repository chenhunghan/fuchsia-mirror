// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

#![recursion_limit = "512"]

mod cgroups;
mod config_gz;
mod cpuinfo;
mod devices;
mod filesystems;
mod fs;
mod kmsg;
mod loadavg;
mod meminfo;
mod misc;
mod mounts_symlink;
mod net_dev;
pub mod pid_directory;
mod pressure_directory;
mod proc_directory;
mod self_symlink;
mod stat;
mod swaps;
pub mod sys_net;
mod sysctl;
mod sysctl_directory;
mod sysrq;
mod thread_self;
mod uid_cputime;
mod uid_io;
mod uid_procstat;
mod uptime;
mod vmstat;
mod zoneinfo;

pub use fs::proc_fs;

/// Test helpers shared by the unit tests of several procfs modules.
#[cfg(test)]
pub(crate) mod testing {
    use starnix_core::task::{CurrentTask, KernelFeatures};
    use starnix_core::testing::spawn_kernel_with_features_and_run;
    use starnix_core::vfs::{
        FileSystemOptions, LookupContext, NamespaceNode, SymlinkMode, WhatToMount,
    };
    use starnix_uapi::errors::Errno;
    use starnix_uapi::mount_flags::MountpointFlags;
    use std::sync::Arc;

    const TEST_DIRENT_CACHE_SIZE: u32 = 2048;

    /// Spawns a kernel with `features`, mounts a fresh procfs at `/proc` with `options`, and runs
    /// `f`.
    pub(crate) async fn spawn_kernel_with_procfs_and_options<F>(
        features: KernelFeatures,
        options: FileSystemOptions,
        f: F,
    ) where
        F: AsyncFnOnce(&mut CurrentTask) + Send + Sync + 'static,
    {
        spawn_kernel_with_features_and_run(
            async move |current_task| {
                let proc_fs = crate::proc_fs(current_task, options).expect("create procfs");
                let root = current_task.lookup_path_from_root("/".into()).expect("root");
                let _ = root
                    .entry
                    .create_dir_for_testing(current_task, "proc".into())
                    .expect("mkdir /proc");
                let proc_mount_point = root
                    .lookup_child(current_task, &mut LookupContext::default(), "proc".into())
                    .expect("lookup /proc");
                proc_mount_point
                    .mount(WhatToMount::Fs(proc_fs), MountpointFlags::empty())
                    .expect("mount proc");
                f(current_task).await;
            },
            features,
        )
        .await;
    }

    /// Spawns a kernel with `features`, mounts a fresh procfs at `/proc`, and runs `f`.
    pub(crate) async fn spawn_kernel_with_procfs<F>(features: KernelFeatures, f: F)
    where
        F: AsyncFnOnce(&mut CurrentTask) + Send + Sync + 'static,
    {
        spawn_kernel_with_procfs_and_options(features, FileSystemOptions::default(), f).await;
    }

    /// Spawns a kernel with a `TEST_DIRENT_CACHE_SIZE`-entry dcache, mounts a fresh procfs at
    /// `/proc` with `options`, and runs `f`.
    pub(crate) async fn spawn_kernel_with_cached_procfs_and_options<F>(
        options: FileSystemOptions,
        f: F,
    ) where
        F: AsyncFnOnce(&mut CurrentTask) + Send + Sync + 'static,
    {
        let features =
            KernelFeatures { dirent_cache_size: TEST_DIRENT_CACHE_SIZE, ..Default::default() };
        spawn_kernel_with_procfs_and_options(features, options, f).await;
    }

    /// Spawns a kernel with a `TEST_DIRENT_CACHE_SIZE`-entry dcache, mounts a fresh procfs at
    /// `/proc`, and runs `f`.
    pub(crate) async fn spawn_kernel_with_cached_procfs<F>(f: F)
    where
        F: AsyncFnOnce(&mut CurrentTask) + Send + Sync + 'static,
    {
        spawn_kernel_with_cached_procfs_and_options(FileSystemOptions::default(), f).await;
    }

    /// Looks up `path` from the root without following a trailing symlink.
    pub(crate) fn lookup_no_follow(
        current_task: &CurrentTask,
        path: &str,
    ) -> Result<NamespaceNode, Errno> {
        let (parent, basename) = current_task.lookup_parent(
            &mut LookupContext::default(),
            &current_task.fs().root(),
            path.into(),
        )?;
        parent.lookup_child(current_task, &mut LookupContext::new(SymlinkMode::NoFollow), basename)
    }

    /// Looks up `path` twice and asserts that the dcache retained the first `DirEntry` (only held
    /// weakly in between) and returned it again. Returns the second lookup.
    pub(crate) fn assert_cached_lookup(current_task: &CurrentTask, path: &str) -> NamespaceNode {
        let weak = Arc::downgrade(&lookup_no_follow(current_task, path).expect(path).entry);
        let second = lookup_no_follow(current_task, path).expect(path);
        assert!(Arc::ptr_eq(&weak.upgrade().expect("retained by dcache"), &second.entry), "{path}");
        second
    }

    /// Caches `path` with `assert_cached_lookup` without holding it, runs `change`, and looks up
    /// `path` again. Returns whether the cached `DirEntry` is still alive afterwards, i.e. whether
    /// the second lookup did not evict it.
    pub(crate) fn cached_entry_survives(
        current_task: &CurrentTask,
        path: &str,
        change: impl FnOnce(),
    ) -> bool {
        let weak = Arc::downgrade(&assert_cached_lookup(current_task, path).entry);
        change();
        let _ = lookup_no_follow(current_task, path);
        weak.upgrade().is_some()
    }
}
