// Copyright 2023 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::vfs::FsNode;
use starnix_uapi::errors::Errno;
use starnix_uapi::seal_flags::SealFlags;
use starnix_uapi::{errno, error};

#[derive(Copy, Clone, Debug, Eq, PartialEq)]
pub enum FileWriteGuardMode {
    // File open for write.
    WriteFile,

    // Writable mapping.
    WriteMapping,

    // Mapped for execution.
    ExecMapping,
}

// Tracks FileWriteGuard state for FsNode instances. Used to implement executable write blocking
// (see `ETXTBSY`) and file seals (see `memfd_create`).
// Note that this is not related to the `flock` file state, which is stored in `FlockInfo`.
#[derive(Default)]
pub struct FileWriteGuardState {
    // Positive values indicate number of write locks, negative - execution locks.
    // This implies that write and exec locks cannot be held similtaneously.
    write_exec_locks: isize,

    // Number of WriteMapping guards.
    num_write_mappings: usize,

    // Seals are not allowed by default.
    seals: Option<SealFlags>,
}

impl FileWriteGuardState {
    pub fn acquire(&mut self, mode: FileWriteGuardMode) -> Result<(), Errno> {
        match mode {
            FileWriteGuardMode::WriteFile => {
                if self.write_exec_locks < 0 {
                    return error!(ETXTBSY);
                }

                // Do not check write seals: file with a write seals can be opened,
                // but `write()` will fail.

                self.write_exec_locks += 1;
            }
            FileWriteGuardMode::WriteMapping => {
                self.check_no_seal(SealFlags::WRITE | SealFlags::FUTURE_WRITE)?;

                // File must be open for write in order to be mapped.
                assert!(self.write_exec_locks > 0);

                self.write_exec_locks += 1;
                self.num_write_mappings += 1;
            }
            FileWriteGuardMode::ExecMapping => {
                if self.write_exec_locks > 0 {
                    return error!(ETXTBSY);
                }
                self.write_exec_locks -= 1;
            }
        }
        Ok(())
    }

    pub fn release(&mut self, mode: FileWriteGuardMode) {
        match mode {
            FileWriteGuardMode::WriteFile => {
                assert!(self.write_exec_locks > 0);
                self.write_exec_locks -= 1;
            }
            FileWriteGuardMode::WriteMapping => {
                assert!(self.write_exec_locks > 0);
                self.write_exec_locks -= 1;
                assert!(self.num_write_mappings > 0);
                self.num_write_mappings -= 1;
            }
            FileWriteGuardMode::ExecMapping => {
                assert!(self.write_exec_locks < 0);
                self.write_exec_locks += 1;
            }
        };
    }

    pub fn enable_sealing(&mut self, initial_seals: SealFlags) {
        self.seals = Some(initial_seals);
    }

    /// Add a new seal to the current set, if allowed.
    pub fn try_add_seal(&mut self, flags: SealFlags) -> Result<(), Errno> {
        if let Some(seals) = self.seals.as_mut() {
            if seals.contains(SealFlags::SEAL) {
                // More seals cannot be added.
                return error!(EPERM);
            }

            // Write seal cannot be added when we have writable mappings.
            if flags.contains(SealFlags::WRITE) && self.num_write_mappings > 0 {
                return error!(EBUSY);
            }

            seals.insert(flags);

            Ok(())
        } else {
            // Seals are not allowed for this file.
            error!(EINVAL)
        }
    }

    /// Fails with EPERM if the current seal flags contain any of the given `flags`.
    pub fn check_no_seal(&self, flags: SealFlags) -> Result<(), Errno> {
        if let Some(seals) = self.seals.as_ref() {
            if seals.intersects(flags) {
                return error!(EPERM);
            }
        }
        Ok(())
    }

    /// Returns current seals.
    pub fn get_seals(&self) -> Result<SealFlags, Errno> {
        self.seals.ok_or_else(|| errno!(EINVAL))
    }
}

/// RAII guard that holds a [`FileWriteGuardMode`] reservation on an [`FsNode`] and releases it on
/// drop without keeping the [`FileWriteGuardState`] mutex locked.
#[derive(Debug)]
#[must_use]
pub struct FileWriteGuardRef<'a> {
    node: &'a FsNode,
    mode: FileWriteGuardMode,
}

impl<'a> FileWriteGuardRef<'a> {
    /// Acquires a [`FileWriteGuardMode`] reservation on `node`.
    pub(in crate::vfs) fn new(node: &'a FsNode, mode: FileWriteGuardMode) -> Result<Self, Errno> {
        node.write_guard_state.lock().acquire(mode)?;
        Ok(Self { node, mode })
    }
}

impl Drop for FileWriteGuardRef<'_> {
    fn drop(&mut self) {
        self.node.write_guard_state.lock().release(self.mode);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::spawn_kernel_and_run;
    use crate::vfs::FsNodeHandle;
    use starnix_uapi::device_id::DeviceId;
    use starnix_uapi::file_mode::FileMode;

    fn create_fs_node(current_task: &crate::task::CurrentTask) -> FsNodeHandle {
        current_task
            .fs()
            .root()
            .create_node(current_task, "foo".into(), FileMode::IFREG, DeviceId::NONE)
            .expect("create_node")
            .entry
            .node
            .clone()
    }

    #[::fuchsia::test]
    async fn test_write_exec_locking() {
        spawn_kernel_and_run(async |current_task| {
            let fs_node = create_fs_node(current_task);

            let write_guard = fs_node
                .create_write_guard(FileWriteGuardMode::WriteFile)
                .expect("FsNode::lock failed unexpectedly");

            assert_eq!(
                fs_node.create_write_guard(FileWriteGuardMode::ExecMapping).unwrap_err(),
                errno!(ETXTBSY)
            );

            let write_mapping_guard = fs_node
                .create_write_guard(FileWriteGuardMode::WriteMapping)
                .expect("FsNode::lock failed unexpectedly");

            assert_eq!(
                fs_node.create_write_guard(FileWriteGuardMode::ExecMapping).unwrap_err(),
                errno!(ETXTBSY)
            );

            std::mem::drop(write_guard);

            assert_eq!(
                fs_node.create_write_guard(FileWriteGuardMode::ExecMapping).unwrap_err(),
                errno!(ETXTBSY)
            );

            std::mem::drop(write_mapping_guard);

            let exec_guard = fs_node
                .create_write_guard(FileWriteGuardMode::ExecMapping)
                .expect("FsNode::lock failed unexpectedly");

            assert_eq!(
                fs_node.create_write_guard(FileWriteGuardMode::WriteFile).unwrap_err(),
                errno!(ETXTBSY)
            );

            std::mem::drop(exec_guard);

            let _write_guard = fs_node
                .create_write_guard(FileWriteGuardMode::WriteFile)
                .expect("FsNode::lock failed unexpectedly");
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_no_seals() {
        let mut state = FileWriteGuardState::default();

        // By default seals are not enabled.
        assert_eq!(state.try_add_seal(SealFlags::WRITE), error!(EINVAL));
        assert_eq!(state.check_no_seal(SealFlags::WRITE), Ok(()));
        assert_eq!(state.get_seals(), error!(EINVAL));
    }

    #[::fuchsia::test]
    async fn test_seals() {
        spawn_kernel_and_run(async |current_task| {
            let fs_node = create_fs_node(current_task);

            {
                let mut state = fs_node.write_guard_state.lock();

                state.enable_sealing(SealFlags::empty());

                assert_eq!(state.check_no_seal(SealFlags::WRITE), Ok(()));
                assert_eq!(state.get_seals(), Ok(SealFlags::empty()));

                // Apply WRITE seal.
                assert_eq!(state.try_add_seal(SealFlags::WRITE), Ok(()));
                assert_eq!(state.check_no_seal(SealFlags::WRITE), error!(EPERM));
                assert_eq!(state.get_seals(), Ok(SealFlags::WRITE));
            }

            // Files with WRITE seal can be opened for write.
            let file_guard = fs_node
                .create_write_guard(FileWriteGuardMode::WriteFile)
                .expect("lock(WriteFile) failed");

            // Files with WRITE seal cannot be mapped.
            assert_eq!(
                fs_node.create_write_guard(FileWriteGuardMode::WriteMapping).unwrap_err(),
                errno!(EPERM)
            );

            std::mem::drop(file_guard);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_seals_block_when_mapped() {
        spawn_kernel_and_run(async |current_task| {
            let fs_node = create_fs_node(current_task);
            fs_node.write_guard_state.lock().enable_sealing(SealFlags::empty());

            let _write_guard = fs_node
                .create_write_guard(FileWriteGuardMode::WriteFile)
                .expect("FsNode::lock failed unexpectedly");
            let write_mapping_guard = fs_node
                .create_write_guard(FileWriteGuardMode::WriteMapping)
                .expect("FsNode::lock failed unexpectedly");

            // Should fail since the file is mapped.
            {
                let mut state = fs_node.write_guard_state.lock();
                assert_eq!(state.try_add_seal(SealFlags::WRITE), error!(EBUSY));
                assert_eq!(state.check_no_seal(SealFlags::WRITE), Ok(()));
            }

            std::mem::drop(write_mapping_guard);

            // Should succeed after file is unmapped.
            {
                let mut state = fs_node.write_guard_state.lock();
                assert_eq!(state.try_add_seal(SealFlags::WRITE), Ok(()));
                assert_eq!(state.check_no_seal(SealFlags::WRITE), error!(EPERM));
            }
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_seals_sealed() {
        spawn_kernel_and_run(async |current_task| {
            let fs_node = create_fs_node(current_task);
            let mut state = fs_node.write_guard_state.lock();

            state.enable_sealing(SealFlags::SEAL);

            assert_eq!(state.get_seals(), Ok(SealFlags::SEAL));

            assert_eq!(state.try_add_seal(SealFlags::WRITE), error!(EPERM));
            assert_eq!(state.get_seals(), Ok(SealFlags::SEAL));
        })
        .await;
    }
}
