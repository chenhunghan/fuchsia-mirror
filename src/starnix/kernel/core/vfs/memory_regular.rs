// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fs::tmpfs::TmpFs;
use crate::mm::memory::MemoryObject;
use crate::mm::{MlockShadowProcess, PAGE_SIZE, ProtectionFlags, VMEX_RESOURCE};
use crate::security;
use crate::signals::{SignalInfo, send_standard_signal};
use crate::task::{CurrentTask, Kernel};
use crate::vfs::buffers::{InputBuffer, OutputBuffer, with_iovec_segments};
use crate::vfs::{
    Anon, AppendLockWriteGuard, DEFAULT_BYTES_PER_BLOCK, DirEntry, FallocMode, FileHandle,
    FileObject, FileOps, FileSystemHandle, FsNode, FsNodeFlags, FsNodeInfo, FsNodeLinkBehavior,
    FsNodeOps, FsString, MAX_LFS_FILESIZE, MemoryXattrStorage, Mount, MountInfo, NamespaceNode,
    OpenAccessCheck, WhatToMount, XattrStorage as _, anon_fs, fileops_impl_noop_sync,
    fs_node_impl_not_dir, fs_node_impl_xattr_delegate,
};
use linux_uapi::{ASHMEM_GET_SIZE, ASHMEM_SET_SIZE};
use memory_pinning::PinnedMapping;
use once_cell::sync::OnceCell;
use starnix_logging::{impossible_error, track_stub};
use starnix_syscalls::{SUCCESS, SyscallArg, SyscallResult};
use starnix_types::math::round_up_to_system_page_size;
use starnix_uapi::errors::{EFBIG, Errno};
use starnix_uapi::file_mode::mode;
use starnix_uapi::open_flags::OpenFlags;
use starnix_uapi::resource_limits::Resource;
use starnix_uapi::seal_flags::SealFlags;
use starnix_uapi::signals::SIGXFSZ;
use starnix_uapi::{errno, error};
use std::sync::Arc;

/// Reads of at least this many bytes are copied directly from the backing memory into the user
/// buffers. Smaller reads go through an intermediate buffer, which is cheaper than preparing the
/// user buffers for direct access.
const DIRECT_READ_THRESHOLD: usize = 4096;

/// Writes of at least this many bytes are copied from the user buffer directly into the VMO.
/// Smaller writes go through a kernel buffer.
const DIRECT_WRITE_THRESHOLD: usize = 4096;

pub struct MemoryRegularNode {
    /// The memory that backs this file.
    memory: Arc<MemoryObject>,
    /// Optional shadow-process VMAR pin keeping the backing VMO in Zircon's
    /// `HIGH_PRIORITY` page queue (e.g., for `tmpfs -o noswap`).
    _pin: Option<Arc<PinnedMapping>>,
    /// Cached read-only VMO handle (without WRITE or RESIZE rights).
    read_only_memory: OnceCell<Arc<MemoryObject>>,
    /// Cached read/exec VMO handle (without WRITE or RESIZE rights, with EXECUTE right).
    read_exec_memory: OnceCell<Arc<MemoryObject>>,
    /// Cached read/write/exec VMO handle (with WRITE, RESIZE, and EXECUTE rights).
    write_exec_memory: OnceCell<Arc<MemoryObject>>,
    xattrs: MemoryXattrStorage,
}

impl MemoryRegularNode {
    /// Create a new writable file node based on a blank VMO.
    pub fn new() -> Result<Self, Errno> {
        let vmo =
            zx::Vmo::create_with_opts(zx::VmoOptions::RESIZABLE, 0).map_err(|_| errno!(ENOMEM))?;
        Ok(Self::from_vmo(vmo, None))
    }

    /// Create a new writable file node whose backing VMO is pinned in the kernel's
    /// memory-pinning shadow process so Zircon will not compress or reclaim its pages
    /// (used for `tmpfs -o noswap`).
    pub fn new_pinned(kernel: &Kernel) -> Result<Self, Errno> {
        let vmo =
            zx::Vmo::create_with_opts(zx::VmoOptions::RESIZABLE, 0).map_err(|_| errno!(ENOMEM))?;
        let shadow_process = MlockShadowProcess::get_or_try_init(kernel)?;
        // In Zircon, mapping any range of a VMO into a VMAR with `MemoryPriority::High`
        // (`fuchsia.starnix.pinned_memory`) increments the whole-VMO `high_priority_count_` on
        // `VmCowPages`, routing all pages subsequently committed to the VMO into
        // `PageQueue::HIGH_PRIORITY`. Because Zircon permits mapping `[0..PAGE_SIZE]` on a
        // 0-length resizable VMO without eagerly committing a page, a single 1-page mapping at
        // node creation suffices without resizing the shadow mapping as the file grows.
        let pin = shadow_process.pin_pages(&vmo, 0, *PAGE_SIZE as usize)?;
        Ok(Self::from_vmo(vmo, Some(pin)))
    }

    fn from_vmo(vmo: zx::Vmo, pin: Option<Arc<PinnedMapping>>) -> Self {
        let mut node =
            Self::from_memory(Arc::new(MemoryObject::from(vmo).with_zx_name(b"starnix:vfs")));
        node._pin = pin;
        node
    }

    /// Create a new file node based on an existing VMO.
    /// Attempts to open the file for writing will fail unless [`memory`] has both
    /// the `WRITE` and `RESIZE` rights.
    pub fn from_memory(memory: Arc<MemoryObject>) -> Self {
        Self {
            memory,
            _pin: None,
            read_only_memory: OnceCell::new(),
            read_exec_memory: OnceCell::new(),
            write_exec_memory: OnceCell::new(),
            xattrs: MemoryXattrStorage::default(),
        }
    }

    /// Returns the cached read-only handle (without WRITE or RESIZE rights) to the backing
    /// memory, shared by all read-only opens of this node.
    fn read_only_memory(&self) -> Result<&Arc<MemoryObject>, Errno> {
        self.read_only_memory.get_or_try_init(|| {
            self.memory
                .duplicate_handle(zx::Rights::VMO_DEFAULT - zx::Rights::WRITE)
                .map(Arc::new)
                .map_err(|_e| errno!(EIO))
        })
    }

    /// Returns the cached executable handle to the backing memory. The handle carries exactly
    /// the rights that `create_file_ops` grants to a readable open: `writable` selects between
    /// the rights of an `O_RDWR` open (with WRITE and RESIZE) and an `O_RDONLY` open (without).
    fn exec_memory(&self, writable: bool) -> Result<Arc<MemoryObject>, Errno> {
        if writable {
            self.write_exec_memory.get_or_try_init(|| {
                self.memory
                    .duplicate_handle(zx::Rights::VMO_DEFAULT | zx::Rights::RESIZE)
                    .map_err(|_e| errno!(EIO))?
                    .replace_as_executable(&VMEX_RESOURCE)
                    .map(Arc::new)
                    .map_err(impossible_error)
            })
        } else {
            self.read_exec_memory.get_or_try_init(|| {
                self.read_only_memory()?
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .and_then(|m| m.replace_as_executable(&VMEX_RESOURCE))
                    .map(Arc::new)
                    .map_err(impossible_error)
            })
        }
        .cloned()
    }
}

impl FsNodeOps for MemoryRegularNode {
    fs_node_impl_not_dir!();
    fs_node_impl_xattr_delegate!(self, self.xattrs);

    fn initial_info(&self, info: &mut FsNodeInfo) {
        info.size = self.memory.get_content_size() as usize;
    }

    fn create_file_ops(
        &self,
        node: &FsNode,
        _current_task: &CurrentTask,
        flags: OpenFlags,
    ) -> Result<Box<dyn FileOps>, Errno> {
        if flags.contains(OpenFlags::TRUNC) {
            // Truncating to zero length must pass the shrink seal check.
            node.write_guard_state.lock().check_no_seal(SealFlags::SHRINK)?;
        }

        // Produce a VMO handle with rights reduced to those requested in |flags|.
        let scoped_memory = if flags.can_read() && !flags.can_write() {
            self.read_only_memory()?.clone()
        } else {
            let mut desired_rights = zx::Rights::VMO_DEFAULT | zx::Rights::RESIZE;
            if !flags.can_read() {
                desired_rights.remove(zx::Rights::READ);
            }
            if !flags.can_write() {
                desired_rights.remove(zx::Rights::WRITE | zx::Rights::RESIZE);
            }
            Arc::new(self.memory.duplicate_handle(desired_rights).map_err(|_e| errno!(EIO))?)
        };
        let file_object = MemoryRegularFile::new(scoped_memory);

        Ok(Box::new(file_object))
    }

    fn truncate(
        &self,
        _guard: &AppendLockWriteGuard<'_>,
        node: &FsNode,
        _current_task: &CurrentTask,
        _file: Option<&FileObject>,
        length: u64,
    ) -> Result<(), Errno> {
        let length = length as usize;

        node.update_info(|info| {
            if info.size == length {
                // The file size remains unaffected.
                return Ok(());
            }

            // We must hold the lock till the end of the operation to guarantee that
            // there is no change to the seals.
            let state = node.write_guard_state.lock();

            if info.size > length {
                // A decrease in file size must pass the shrink seal check.
                state.check_no_seal(SealFlags::SHRINK)?;
            } else {
                // An increase in file size must pass the grow seal check.
                state.check_no_seal(SealFlags::GROW)?;
            }

            let memory_size = update_memory_file_size(&self.memory, info, length)?;
            info.size = length;

            // Zero unused parts of the VMO.
            if memory_size > length {
                self.memory
                    .op_range(zx::VmoOp::ZERO, length as u64, (memory_size - length) as u64)
                    .map_err(impossible_error)?;
            }

            Ok(())
        })
    }
}

impl MemoryRegularNode {
    /// Manipulates allocated memory. `_guard` shows that the caller holds the node's append write
    /// lock.
    fn allocate(
        &self,
        _guard: &AppendLockWriteGuard<'_>,
        node: &FsNode,
        current_task: &CurrentTask,
        mode: FallocMode,
        offset: u64,
        length: u64,
    ) -> Result<(), Errno> {
        match mode {
            FallocMode::PunchHole => {
                // Lock `info()` before acquiring the `write_guard_state` lock to ensure consistent
                // lock ordering.
                let info = node.info();

                // Check write seal. Hold the lock to ensure seals don't change.
                let state = node.write_guard_state.lock();
                state.check_no_seal(SealFlags::WRITE | SealFlags::FUTURE_WRITE)?;

                let mut end = offset.checked_add(length).ok_or_else(|| errno!(EINVAL))? as usize;

                let memory_size = info.storage_size();
                if offset as usize >= memory_size {
                    return Ok(());
                }

                // If punching hole at the end of the file then zero all the
                // way to the end of the VMO to avoid keeping any pages for the tail.
                if end >= info.size {
                    end = memory_size;
                }

                self.memory
                    .op_range(zx::VmoOp::ZERO, offset, end as u64 - offset)
                    .map_err(impossible_error)?;

                Ok(())
            }

            FallocMode::Allocate { keep_size } => {
                let end = offset.checked_add(length).ok_or_else(|| errno!(EFBIG))?;
                // Linux tmpfs enforces `RLIMIT_FSIZE` even when `FALLOC_FL_KEEP_SIZE` is set.
                let new_size = node.check_size_limit(current_task, end)?;
                node.update_info(|info| {
                    if new_size > info.size {
                        // Check GROW seal (even with `keep_size=true`). Hold the lock to ensure
                        // seals don't change.
                        let state = node.write_guard_state.lock();
                        state.check_no_seal(SealFlags::GROW)?;

                        update_memory_file_size(&self.memory, info, new_size)?;

                        if !keep_size {
                            info.size = new_size;
                        }
                    }
                    Ok(())
                })
            }

            _ => error!(EOPNOTSUPP),
        }
    }
}

pub struct MemoryRegularFile {
    pub memory: Arc<MemoryObject>,
}

impl MemoryRegularFile {
    /// Creates a file object based on a VMO.
    pub fn new(memory: Arc<MemoryObject>) -> Self {
        MemoryRegularFile { memory }
    }

    /// Creates a new anonymous file backed by a private regular [`FsNode`] with the specified
    /// `size`, to which no security labeling nor access-checks will be applied.
    pub fn new_private_file_with_ops(
        current_task: &CurrentTask,
        ops: Box<dyn FileOps>,
        size: usize,
        flags: OpenFlags,
        name: &'static str,
    ) -> FileHandle {
        let mut info = FsNodeInfo::new(mode!(IFREG, 0o600), current_task.current_fscred());
        info.size = size;
        let fs = anon_fs(current_task.kernel());
        let node = fs.create_node_with_flags(None, Anon {}, info, FsNodeFlags::IS_PRIVATE);
        security::fs_node_init_anon(current_task, &node, name)
            .expect("Private anon_inode creation cannot fail");
        let name = NamespaceNode::new_anonymous(DirEntry::new(node, None, name.into()));
        FileObject::new(current_task, ops, name, flags)
            .expect("FileObject::new on private anon_inode cannot fail")
    }

    /// Creates a new anonymous file backed by `memory` and a private regular [`FsNode`], to which
    /// no security labeling nor access-checks will be applied.
    pub fn new_private_file(
        current_task: &CurrentTask,
        memory: Arc<MemoryObject>,
        flags: OpenFlags,
        name: &'static str,
    ) -> FileHandle {
        let size = memory.get_size() as usize;
        Self::new_private_file_with_ops(
            current_task,
            Box::new(Self::new(memory)),
            size,
            flags,
            name,
        )
    }

    pub fn read(
        memory: &Arc<MemoryObject>,
        file: &FileObject,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        if data.available() >= DIRECT_READ_THRESHOLD && memory.as_vmo().is_some() {
            Self::read_direct(memory, file, offset, data)
        } else {
            Self::read_buffered(memory, file, offset, data)
        }
    }

    /// Reads from `memory` into `data` through an intermediate kernel buffer.
    fn read_buffered(
        memory: &MemoryObject,
        file: &FileObject,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        let info = file.node().info();
        let file_length = info.size;
        let want_read = data.available();
        if offset < file_length {
            let to_read =
                if file_length < offset + want_read { file_length - offset } else { want_read };
            let buf = memory.read_to_vec(offset as u64, to_read as u64).map_err(|_| errno!(EIO))?;
            drop(info);
            data.write_all(&buf[..])?;
            Ok(to_read)
        } else {
            Ok(0)
        }
    }

    /// Reads from `memory` directly into the segments of `data`, without staging the bytes in an
    /// intermediate kernel buffer.
    ///
    /// Falls back to `read_buffered` for small reads, for buffers that do not expose their
    /// segments, or for any remaining bytes after a `read_raw` failure.
    fn read_direct(
        memory: &MemoryObject,
        file: &FileObject,
        offset: usize,
        data: &mut dyn OutputBuffer,
    ) -> Result<usize, Errno> {
        let want_read = data.available();
        {
            let info = file.node().info();
            let file_length = info.size;
            if offset >= file_length {
                return Ok(0);
            }
            let to_read = std::cmp::min(want_read, file_length - offset);
            if to_read < DIRECT_READ_THRESHOLD {
                let buf =
                    memory.read_to_vec(offset as u64, to_read as u64).map_err(|_| errno!(EIO))?;
                drop(info);
                data.write_all(&buf[..])?;
                return Ok(to_read);
            }
        }
        let mut short_read = false;
        // `with_iovec_segments` prepares the user VMAR mappings before invoking the closure, so
        // acquiring the node info lock inside the closure respects `MemoryManagerStateLock =>
        // FsNodeInfoLevel`. Holding the node info lock across `read_raw` keeps `info.size` and
        // the backing VMO consistent against concurrent truncation or writes. An unmapped segment
        // can still make `read_raw` fail, in which case the remaining bytes are retried through
        // the buffered path after dropping the lock.
        let direct = with_iovec_segments::<_, syncio::zxio::zx_iovec, _>(data, |iovecs| {
            let info = file.node().info();
            let file_length = info.size;
            if offset >= file_length {
                return Ok(0);
            }
            let to_read = std::cmp::min(want_read, file_length - offset);
            let mut copied = 0;
            for iovec in iovecs.iter() {
                if copied == to_read {
                    break;
                }
                let len = std::cmp::min(iovec.capacity, to_read - copied);
                if len == 0 {
                    continue;
                }
                // SAFETY: `iovec` is a segment of `data` with `len <= iovec.capacity`. A user
                // segment is bounded to the user VMAR by `data` and written by `zx_vmo_read`,
                // which captures faults and reports them as errors. A kernel segment, such as a
                // `VecOutputBuffer` or `SpliceOutputBuffer`, is a valid writable allocation for
                // `len` bytes.
                let result = unsafe {
                    memory.read_raw(iovec.buffer as *mut u8, len, (offset + copied) as u64)
                };
                if result.is_err() {
                    short_read = true;
                    break;
                }
                copied += len;
            }
            Ok(copied)
        });
        let copied = match direct {
            Some(Ok(copied)) => copied,
            None | Some(Err(_)) => return Self::read_buffered(memory, file, offset, data),
        };
        if copied > 0 {
            // SAFETY: We just successfully read `copied` bytes from the `MemoryObject`
            // directly to the buffer segments.
            unsafe { data.advance(copied) }?;
        }
        if !short_read {
            return Ok(copied);
        }

        // Read the rest of the data through the kernel buffer.
        match Self::read_buffered(memory, file, offset + copied, data) {
            Ok(remaining_read) => Ok(copied + remaining_read),
            Err(_) if copied > 0 => Ok(copied),
            Err(e) => Err(e),
        }
    }

    pub fn write(
        memory: &Arc<MemoryObject>,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let result = if data.available() >= DIRECT_WRITE_THRESHOLD && memory.as_vmo().is_some() {
            Self::write_direct(memory, file, current_task, offset, data)
        } else {
            Self::write_buffered(memory, file, current_task, offset, data)
        };
        if let Err(ref e) = result
            && *e == EFBIG
        {
            // EFBIG must trigger a signal. Sending the signal must be done outside of the
            // update_info method to ensure no deadlock.
            send_standard_signal(current_task, SignalInfo::kernel(SIGXFSZ));
        }
        result
    }

    /// Writes `data` to `memory` by first copying all of it into a kernel buffer.
    fn write_buffered(
        memory: &MemoryObject,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let buf = data.peek_all()?;

        let written = file.node().update_info(|info| {
            Self::write_locked(
                memory,
                file,
                current_task,
                info,
                offset,
                buf.len(),
                |want_write, _file_size| {
                    memory.write(&buf[..want_write], offset as u64).map_err(|_| errno!(EIO))?;
                    Ok(want_write)
                },
            )
        })?;
        data.advance(written)?;
        Ok(written)
    }

    /// Writes `data` to `memory` by passing its segments to `zx_vmo_write`, without copying them
    /// into a kernel buffer first.
    ///
    /// Falls back to `write_buffered` for the bytes Zircon can't write: all of them if `data`
    /// doesn't expose its segments, or the rest of the write if Zircon fails to read a segment.
    /// Zircon reports faults as errors instead of letting Starnix handle them, so the fallback is
    /// what preserves `EFAULT` and userfaultfd behavior. If a segment faults partway through while
    /// overwriting bytes before EOF, Zircon leaves its readable prefix in the VMO, but
    /// `write_buffered` still rejects the whole segment with `EFAULT` until `InputBuffer` supports
    /// partial segment reads (https://fxbug.dev/42068846).
    fn write_direct(
        memory: &MemoryObject,
        file: &FileObject,
        current_task: &CurrentTask,
        offset: usize,
        data: &mut dyn InputBuffer,
    ) -> Result<usize, Errno> {
        let available = data.available();
        let mut short_write = false;
        // `with_iovec_segments` maps the user buffers in the user VMAR before the node locks are
        // taken, so the lock order is the same as in `write_buffered`.
        let direct = with_iovec_segments::<_, zx::sys::zx_iovec_t, _>(data, |iovecs| {
            Ok(file.node().update_info(|info| {
                Self::write_locked(
                    memory,
                    file,
                    current_task,
                    info,
                    offset,
                    available,
                    |want_write, file_size| {
                        let mut written = 0;
                        for iovec in iovecs.iter() {
                            if written == want_write {
                                break;
                            }
                            let len = std::cmp::min(iovec.capacity, want_write - written);
                            let segment_offset = offset + written;
                            // SAFETY: `iovec` is a segment of `data`. A user segment is read
                            // with user copy routines, which report faults as errors. A kernel
                            // segment, such as a `VecInputBuffer`, is valid for `len` bytes.
                            let result = unsafe {
                                memory.write_raw(iovec.buffer, len, segment_offset as u64)
                            };
                            if result.is_err() {
                                // Zircon may have written part of the segment before it failed.
                                // Zero those bytes past the end of the file, so that they do not
                                // become visible if the file grows later. Bytes copied before
                                // `file_size` stay modified in the VMO, and `write_buffered` will
                                // not count them because `InputBuffer` reads each segment as a
                                // whole (https://fxbug.dev/42068846).
                                let zero_start = std::cmp::max(segment_offset, file_size) as u64;
                                let zero_end =
                                    std::cmp::min((segment_offset + len) as u64, memory.get_size());
                                if zero_end > zero_start {
                                    // Best-effort: `write_raw` can also fail when the VMO itself
                                    // rejects the write (for example after `ASHMEM_SET_SIZE`
                                    // shrinks it), in which case `write_buffered` will report the
                                    // error.
                                    let _ = memory.op_range(
                                        zx::VmoOp::ZERO,
                                        zero_start,
                                        zero_end - zero_start,
                                    );
                                }
                                short_write = true;
                                break;
                            }
                            written += len;
                        }
                        Ok(written)
                    },
                )
            }))
        });

        let written = match direct {
            Some(Ok(result)) => result?,
            // `data` doesn't expose its segments, or they couldn't be mapped. `write_buffered`
            // then reports any error the same way it does for small writes.
            None | Some(Err(_)) => {
                return Self::write_buffered(memory, file, current_task, offset, data);
            }
        };
        data.advance(written)?;
        if !short_write {
            return Ok(written);
        }

        // Write the rest of the data through the kernel buffer.
        match Self::write_buffered(memory, file, current_task, offset + written, data) {
            Ok(remaining_written) => Ok(written + remaining_written),
            Err(_) if written > 0 => Ok(written),
            Err(e) => Err(e),
        }
    }

    /// Checks the seals and the `FSIZE` limit for a write of `want_write` bytes at `offset`, and
    /// grows `memory` if needed. Then calls `do_write` with the number of bytes to write and the
    /// current file size. `do_write` returns the number of bytes that it wrote, and the file size
    /// is updated to include them.
    ///
    /// `info` must be borrowed from `FsNode::update_info`, so that the info lock is held.
    fn write_locked(
        memory: &MemoryObject,
        file: &FileObject,
        current_task: &CurrentTask,
        info: &mut FsNodeInfo,
        offset: usize,
        mut want_write: usize,
        do_write: impl FnOnce(usize, usize) -> Result<usize, Errno>,
    ) -> Result<usize, Errno> {
        if want_write == 0 {
            return Ok(0);
        }

        let mut write_end = offset + want_write;

        // We must hold the lock till the end of the operation to guarantee that
        // there is no change to the seals.
        let state = file.name.entry.node.write_guard_state.lock();

        // Non-zero writes must pass the write seal check.
        state.check_no_seal(SealFlags::WRITE | SealFlags::FUTURE_WRITE)?;

        // Writing past the file size
        if write_end > info.size {
            // The grow seal check failed.
            if let Err(e) = state.check_no_seal(SealFlags::GROW) {
                let page_size = *PAGE_SIZE as usize;
                let eof_page_start = info.size - (info.size % page_size);
                if offset >= info.size {
                    // Write starts outside the file.
                    // Forbid because nothing can be written without growing.
                    return Err(e);
                } else if info.size == eof_page_start {
                    // Write starts inside file and EOF page does not need to grow.
                    // End write at EOF.
                    write_end = info.size;
                    want_write = write_end - offset;
                } else if offset >= eof_page_start {
                    // Write starts in EOF page.
                    // Forbid because EOF page cannot grow.
                    return Err(e);
                } else {
                    // End write at page before EOF.
                    write_end = eof_page_start;
                    want_write = write_end - offset;
                }
            }
        }

        // Check against the FSIZE limit
        let fsize_limit = current_task.thread_group().get_rlimit(Resource::FSIZE) as usize;
        if write_end > fsize_limit {
            if offset >= fsize_limit {
                // Write starts beyond the FSIZE limit.
                return error!(EFBIG);
            }

            // End write at FSIZE limit.
            write_end = fsize_limit;
            want_write = write_end - offset;
        }

        let prev_storage_size = info.storage_size();
        if write_end > info.size && write_end > prev_storage_size {
            update_memory_file_size(memory, info, write_end)?;
        }
        let written = match do_write(want_write, info.size) {
            Ok(written) => written,
            Err(e) => {
                if info.storage_size() > prev_storage_size {
                    let _ = update_memory_file_size(
                        memory,
                        info,
                        std::cmp::max(prev_storage_size, info.size),
                    );
                }
                return Err(e);
            }
        };

        let written_end = offset + written;
        if written > 0 && written_end > info.size {
            info.size = written_end;
        }
        if written < want_write && info.storage_size() > prev_storage_size {
            let _ =
                update_memory_file_size(memory, info, std::cmp::max(prev_storage_size, info.size));
        }
        Ok(written)
    }

    pub fn get_memory(
        memory: &Arc<MemoryObject>,
        file: &FileObject,
        _current_task: &CurrentTask,
        prot: ProtectionFlags,
    ) -> Result<Arc<MemoryObject>, Errno> {
        // In MemoryFileNode::create_file_ops, we downscoped the rights
        // on the VMO to match the rights on the file object. If the caller
        // wants more rights than exist on the file object, return an error
        // instead of returning a MemoryObject that does not conform to
        // the FileOps::get_memory contract.
        if prot.contains(ProtectionFlags::READ) && !file.can_read() {
            return error!(EACCES);
        }
        if prot.contains(ProtectionFlags::WRITE) && !file.can_write() {
            return error!(EACCES);
        }
        let mut memory = Arc::clone(memory);
        if prot.contains(ProtectionFlags::EXEC) {
            // Readable opens of a `MemoryRegularNode` receive handles with a fixed set of rights
            // determined by `create_file_ops`, so the executable handle can be shared across all
            // such opens via the node. Write-only opens get a bespoke handle without READ, so
            // they derive their own executable handle below rather than polluting the cache.
            if file.can_read()
                && let Some(node) = file.node().downcast_ops::<MemoryRegularNode>()
            {
                return node.exec_memory(file.can_write());
            }
            memory = Arc::new(
                memory
                    .duplicate_handle(zx::Rights::SAME_RIGHTS)
                    .map_err(impossible_error)?
                    .replace_as_executable(&VMEX_RESOURCE)
                    .map_err(impossible_error)?,
            );
        }
        Ok(memory)
    }
}

#[macro_export]
macro_rules! fileops_impl_memory {
    ($self:ident, $memory:expr) => {
        $crate::fileops_impl_seekable!();

        fn read(
            &$self,
            file: &$crate::vfs::FileObject,
            _current_task: &$crate::task::CurrentTask,
            offset: usize,
            data: &mut dyn $crate::vfs::buffers::OutputBuffer,
        ) -> Result<usize, starnix_uapi::errors::Errno> {
            $crate::vfs::MemoryRegularFile::read($memory, file, offset, data)
        }

        fn write(
            &$self,
            file: &$crate::vfs::FileObject,
            current_task: &$crate::task::CurrentTask,
            offset: usize,
            data: &mut dyn $crate::vfs::buffers::InputBuffer,
        ) -> Result<usize, starnix_uapi::errors::Errno> {
            $crate::vfs::MemoryRegularFile::write($memory, file, current_task, offset, data)
        }

        fn get_memory(
            &$self,
            file: &$crate::vfs::FileObject,
            current_task: &$crate::task::CurrentTask,
            _length: Option<usize>,
            prot: $crate::mm::ProtectionFlags,
        ) -> Result<Arc<$crate::mm::memory::MemoryObject>, starnix_uapi::errors::Errno> {
            $crate::vfs::MemoryRegularFile::get_memory($memory, file, current_task, prot)
        }
    }
}
pub use fileops_impl_memory;

impl FileOps for MemoryRegularFile {
    fileops_impl_memory!(self, &self.memory);
    fileops_impl_noop_sync!();

    fn readahead(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        _offset: usize,
        _length: usize,
    ) -> Result<(), Errno> {
        track_stub!(TODO("https://fxbug.dev/42082608"), "paged VMO readahead");
        Ok(())
    }

    fn fallocate(
        &self,
        guard: &AppendLockWriteGuard<'_>,
        file: &FileObject,
        current_task: &CurrentTask,
        mode: FallocMode,
        offset: u64,
        length: u64,
    ) -> Result<(), Errno> {
        // Only `MemoryRegularNode`-backed files support resizing; private or read-only VMO files
        // do not.
        let node = file.node();
        let Some(ops) = node.downcast_ops::<MemoryRegularNode>() else {
            return error!(EOPNOTSUPP);
        };
        ops.allocate(guard, node, current_task, mode, offset, length)
    }

    fn ioctl(
        &self,
        _file: &FileObject,
        _current_task: &CurrentTask,
        request: u32,
        arg: SyscallArg,
    ) -> Result<SyscallResult, Errno> {
        match request {
            ASHMEM_GET_SIZE => {
                track_stub!(TODO("https://fxbug.dev/389102161"), "ashmem get_size on memfd");
                Ok(self.memory.get_size().into())
            }
            ASHMEM_SET_SIZE => {
                track_stub!(TODO("https://fxbug.dev/389102161"), "ashmem set_size on memfd");
                self.memory.set_size(arg.into()).map_err(|_| errno!(EINVAL))?;
                Ok(SUCCESS)
            }
            _ => error!(ENOTTY),
        }
    }
}

pub fn new_memfd(
    current_task: &CurrentTask,
    mut name: FsString,
    seals: SealFlags,
    flags: OpenFlags,
) -> Result<FileHandle, Errno> {
    struct MemFdTmpfs {
        tmpfs: FileSystemHandle,
        mount: Arc<Mount>,
    }

    let fs = current_task.kernel().expando.get_or_init(|| {
        let tmpfs = TmpFs::new_fs(current_task.kernel());
        security::file_system_resolve_security(&current_task, &tmpfs).expect("resolve fs security");
        let mounts_guard = current_task.kernel().mounts_lock();
        let mount = Mount::new(&mounts_guard, WhatToMount::Fs(tmpfs.clone()), Default::default())
            .expect("create new tempfs mount for memfd");
        MemFdTmpfs { tmpfs, mount }
    });

    // Create the node as a kernel-internal operation, to skip the filesystem access-checks.
    // TODO: https://fxbug.dev/455785957 - Validate whether any access-checks should be performed
    // during "memfd" creation.
    let fs_node = current_task.override_creds(
        security::creds_start_internal_operation(current_task),
        || {
            fs.tmpfs.root().node.create_tmpfile(
                current_task,
                &MountInfo::detached(),
                mode!(IFREG, 0o600),
                FsNodeLinkBehavior::Disallowed,
            )
        },
    )?;

    // LSM allows security modules to choose to treat mem-FDs as a kind of anonymous inode.
    security::fs_node_init_anon(current_task, &fs_node, "[memfd]")?;

    fs_node.write_guard_state.lock().enable_sealing(seals);

    // memfd instances appear in /proc[pid]/fd as though they are O_TMPFILE files with names of
    // the form "memfd:[name]".
    let mut local_name = FsString::from("memfd:");
    local_name.append(&mut name);
    let dir_entry = DirEntry::new_deleted(fs_node, Some(fs.tmpfs.root().clone()), local_name);
    security::fs_node_init_with_dentry(current_task, &dir_entry)?;

    let name = NamespaceNode::new(fs.mount.clone(), dir_entry);
    name.open(current_task, OpenAccessCheck::skip(flags))
}

/// Sets memory size to `min_size` rounded to whole pages. Returns the new size of the VMO in bytes.
fn update_memory_file_size(
    memory: &MemoryObject,
    node_info: &mut FsNodeInfo,
    requested_size: usize,
) -> Result<usize, Errno> {
    assert!(requested_size <= MAX_LFS_FILESIZE);
    let size = round_up_to_system_page_size(requested_size)?;
    memory.set_size(size as u64).map_err(|status| match status {
        zx::Status::NO_MEMORY => errno!(ENOMEM),
        zx::Status::OUT_OF_RANGE => errno!(ENOMEM),
        _ => impossible_error(status),
    })?;
    node_info.blocks = size / DEFAULT_BYTES_PER_BLOCK;
    Ok(size)
}

#[cfg(test)]
mod test {
    use super::*;
    use crate::mm::syscalls::do_mmap;
    use crate::mm::{MemoryAccessor, MemoryAccessorExt, TaskMemoryAccessor};
    use crate::testing::{map_memory, spawn_kernel_and_run};
    use crate::vfs::buffers::{
        Buffer, OutputBufferCallback, PeekBufferSegmentsCallback, UserBuffersInputBuffer,
        UserBuffersOutputBuffer, VecInputBuffer, VecOutputBuffer,
    };
    use crate::vfs::{FdFlags, LookupContext, Namespace, SeekTarget};
    use smallvec::smallvec;
    use starnix_types::user_buffer::UserBuffer;
    use starnix_uapi::device_id::DeviceId;
    use starnix_uapi::user_address::UserAddress;
    use starnix_uapi::{MAP_SHARED, PROT_READ, PROT_WRITE, rlimit};

    /// Returns `len` bytes of a repeating, non-zero pattern.
    fn test_pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8 + 1).collect()
    }

    /// Reads `len` bytes at `offset` from `file`.
    fn read_file_at(
        current_task: &CurrentTask,
        file: &FileHandle,
        offset: usize,
        len: usize,
    ) -> Vec<u8> {
        let mut buffer = VecOutputBuffer::new(len);
        let read = file.read_at(current_task, offset, &mut buffer).expect("read_at");
        assert_eq!(read, len);
        buffer.data().to_vec()
    }

    #[derive(Debug)]
    struct VectoredOnlyOutputBuffer<'a, M> {
        inner: UserBuffersOutputBuffer<'a, M>,
    }

    impl<'a, M: TaskMemoryAccessor + std::fmt::Debug> Buffer for VectoredOnlyOutputBuffer<'a, M> {
        fn segments_count(&self) -> Result<usize, Errno> {
            self.inner.segments_count()
        }

        fn peek_each_segment(
            &mut self,
            callback: &mut PeekBufferSegmentsCallback<'_>,
        ) -> Result<(), Errno> {
            self.inner.peek_each_segment(callback)
        }
    }

    impl<'a, M: TaskMemoryAccessor + std::fmt::Debug> OutputBuffer for VectoredOnlyOutputBuffer<'a, M> {
        fn write_each(&mut self, _callback: &mut OutputBufferCallback<'_>) -> Result<usize, Errno> {
            panic!("write_each called");
        }

        fn available(&self) -> usize {
            self.inner.available()
        }

        fn bytes_written(&self) -> usize {
            self.inner.bytes_written()
        }

        fn zero(&mut self) -> Result<usize, Errno> {
            panic!("zero called");
        }

        unsafe fn advance(&mut self, length: usize) -> Result<(), Errno> {
            // SAFETY: The caller guarantees that `length` bytes were written.
            unsafe { self.inner.advance(length) }
        }
    }

    #[::fuchsia::test]
    async fn test_get_memory_cached_across_opens_and_exec() {
        spawn_kernel_and_run(async |current_task| {
            let fs = TmpFs::new_fs(current_task.kernel());
            let ns = Namespace::new(fs);
            let root = ns.root();

            root.create_node(current_task, "file".into(), mode!(IFREG, 0o755), DeviceId::NONE)
                .expect("create_node failed");

            let mut context = LookupContext::default();
            let file_node =
                root.lookup_child(current_task, &mut context, "file".into()).expect("lookup");

            let file1 = file_node.open(current_task, OpenFlags::RDONLY).expect("open 1 failed");
            let ro_mem1 = file1
                .get_memory(current_task, None, ProtectionFlags::READ)
                .expect("get_memory READ 1 failed");
            let rx_mem1 = file1
                .get_memory(current_task, None, ProtectionFlags::READ | ProtectionFlags::EXEC)
                .expect("get_memory READ|EXEC 1 failed");
            assert!(rx_mem1.get_rights().contains(zx::Rights::EXECUTE));
            assert!(!rx_mem1.get_rights().contains(zx::Rights::WRITE));

            let file2 = file_node.open(current_task, OpenFlags::RDONLY).expect("open 2 failed");
            let ro_mem2 = file2
                .get_memory(current_task, None, ProtectionFlags::READ)
                .expect("get_memory READ 2 failed");
            let rx_mem2 = file2
                .get_memory(current_task, None, ProtectionFlags::READ | ProtectionFlags::EXEC)
                .expect("get_memory READ|EXEC 2 failed");
            assert!(Arc::ptr_eq(&ro_mem1, &ro_mem2));
            assert!(Arc::ptr_eq(&rx_mem1, &rx_mem2));

            // Read/write opens share a separate executable handle that retains WRITE.
            let file_rw1 = file_node.open(current_task, OpenFlags::RDWR).expect("open rw1 failed");
            let rwx_mem1 = file_rw1
                .get_memory(
                    current_task,
                    None,
                    ProtectionFlags::READ | ProtectionFlags::WRITE | ProtectionFlags::EXEC,
                )
                .expect("get_memory READ|WRITE|EXEC 1 failed");
            assert!(rwx_mem1.get_rights().contains(
                zx::Rights::READ | zx::Rights::WRITE | zx::Rights::RESIZE | zx::Rights::EXECUTE
            ));
            assert!(!Arc::ptr_eq(&rwx_mem1, &rx_mem1));

            let file_rw2 = file_node.open(current_task, OpenFlags::RDWR).expect("open rw2 failed");
            let rwx_mem2 = file_rw2
                .get_memory(current_task, None, ProtectionFlags::READ | ProtectionFlags::EXEC)
                .expect("get_memory READ|EXEC rw2 failed");
            assert!(Arc::ptr_eq(&rwx_mem1, &rwx_mem2));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_get_memory_write_only_exec_does_not_pollute_cache() {
        spawn_kernel_and_run(async |current_task| {
            let fs = TmpFs::new_fs(current_task.kernel());
            let ns = Namespace::new(fs);
            let root = ns.root();

            root.create_node(current_task, "file".into(), mode!(IFREG, 0o755), DeviceId::NONE)
                .expect("create_node failed");

            let mut context = LookupContext::default();
            let file_node =
                root.lookup_child(current_task, &mut context, "file".into()).expect("lookup");

            // A write-only open gets an executable handle without READ...
            let file_wo = file_node.open(current_task, OpenFlags::WRONLY).expect("open wo failed");
            let wx_mem = file_wo
                .get_memory(current_task, None, ProtectionFlags::WRITE | ProtectionFlags::EXEC)
                .expect("get_memory WRITE|EXEC failed");
            assert!(wx_mem.get_rights().contains(zx::Rights::WRITE | zx::Rights::EXECUTE));
            assert!(!wx_mem.get_rights().contains(zx::Rights::READ));

            // ...and must not be handed to a subsequent read/write open.
            let file_rw = file_node.open(current_task, OpenFlags::RDWR).expect("open rw failed");
            let rwx_mem = file_rw
                .get_memory(
                    current_task,
                    None,
                    ProtectionFlags::READ | ProtectionFlags::WRITE | ProtectionFlags::EXEC,
                )
                .expect("get_memory READ|WRITE|EXEC failed");
            assert!(!Arc::ptr_eq(&wx_mem, &rwx_mem));
            assert!(rwx_mem.get_rights().contains(
                zx::Rights::READ | zx::Rights::WRITE | zx::Rights::RESIZE | zx::Rights::EXECUTE
            ));

            // Nor to a read-only open.
            let file_ro = file_node.open(current_task, OpenFlags::RDONLY).expect("open ro failed");
            let rx_mem = file_ro
                .get_memory(current_task, None, ProtectionFlags::READ | ProtectionFlags::EXEC)
                .expect("get_memory READ|EXEC failed");
            assert!(!Arc::ptr_eq(&wx_mem, &rx_mem));
            assert!(!Arc::ptr_eq(&rwx_mem, &rx_mem));
            assert!(rx_mem.get_rights().contains(zx::Rights::READ | zx::Rights::EXECUTE));
            assert!(!rx_mem.get_rights().contains(zx::Rights::WRITE));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_read_into_user_buffers() {
        spawn_kernel_and_run(async |current_task| {
            let fs = TmpFs::new_fs(current_task.kernel());
            let ns = Namespace::new(fs);
            let root = ns.root();

            root.create_node(current_task, "file".into(), mode!(IFREG, 0o644), DeviceId::NONE)
                .expect("create_node failed");
            let mut context = LookupContext::default();
            let file_node =
                root.lookup_child(current_task, &mut context, "file".into()).expect("lookup");
            let file = file_node.open(current_task, OpenFlags::RDWR).expect("open failed");

            const READ_SIZE: usize = 64 * 1024;
            const LARGE_TAIL_SIZE: usize = DIRECT_READ_THRESHOLD + 100;
            const SMALL_TAIL_SIZE: usize = 100;
            let content: Vec<u8> =
                (0..READ_SIZE + LARGE_TAIL_SIZE).map(|i| (i % 251) as u8).collect();
            assert_eq!(
                file.write_at(current_task, 0, &mut VecInputBuffer::new(&content)),
                Ok(content.len())
            );

            let addr = map_memory(current_task, UserAddress::default(), READ_SIZE as u64);
            let first_len: usize = 40 * 1024;
            let second_addr = (addr + first_len).unwrap();

            // A large read spanning two user buffers uses `read_direct` without calling
            // `write_each`.
            let mut buffer = VectoredOnlyOutputBuffer {
                inner: UserBuffersOutputBuffer::unified_new(
                    current_task,
                    smallvec![
                        UserBuffer { address: addr, length: first_len },
                        UserBuffer { address: second_addr, length: READ_SIZE - first_len },
                    ],
                )
                .expect("unified_new"),
            };
            assert_eq!(file.read_at(current_task, 0, &mut buffer), Ok(READ_SIZE));
            assert_eq!(buffer.available(), 0);
            assert_eq!(
                current_task.read_memory_to_vec(addr, READ_SIZE).expect("read_memory_to_vec"),
                &content[..READ_SIZE]
            );

            // A large read that reaches the end of the file with `>= DIRECT_READ_THRESHOLD` bytes
            // remaining still uses `read_direct`.
            let mut buffer = VectoredOnlyOutputBuffer {
                inner: UserBuffersOutputBuffer::unified_new_at(current_task, addr, READ_SIZE)
                    .expect("unified_new_at"),
            };
            assert_eq!(file.read_at(current_task, READ_SIZE, &mut buffer), Ok(LARGE_TAIL_SIZE));
            assert_eq!(buffer.bytes_written(), LARGE_TAIL_SIZE);
            assert_eq!(
                current_task.read_memory_to_vec(addr, LARGE_TAIL_SIZE).expect("read_memory_to_vec"),
                &content[READ_SIZE..]
            );

            // A large buffer reading fewer than `DIRECT_READ_THRESHOLD` bytes before EOF takes the
            // buffered path.
            let small_tail_offset = content.len() - SMALL_TAIL_SIZE;
            let mut buffer = UserBuffersOutputBuffer::unified_new_at(current_task, addr, READ_SIZE)
                .expect("unified_new_at");
            assert_eq!(
                file.read_at(current_task, small_tail_offset, &mut buffer),
                Ok(SMALL_TAIL_SIZE)
            );
            assert_eq!(buffer.bytes_written(), SMALL_TAIL_SIZE);
            assert_eq!(
                current_task.read_memory_to_vec(addr, SMALL_TAIL_SIZE).expect("read_memory_to_vec"),
                &content[small_tail_offset..]
            );

            // A read at the end of the file returns 0.
            let mut buffer = VectoredOnlyOutputBuffer {
                inner: UserBuffersOutputBuffer::unified_new_at(current_task, addr, READ_SIZE)
                    .expect("unified_new_at"),
            };
            assert_eq!(file.read_at(current_task, content.len(), &mut buffer), Ok(0));

            // `VecOutputBuffer` exposes kernel-backed segments to `read_direct`.
            let mut buffer = VecOutputBuffer::new(2 * DIRECT_READ_THRESHOLD);
            assert_eq!(file.read_at(current_task, 1, &mut buffer), Ok(2 * DIRECT_READ_THRESHOLD));
            assert_eq!(buffer.data(), &content[1..1 + 2 * DIRECT_READ_THRESHOLD]);

            // A buffer that does not expose segments falls back to the buffered path.
            let mut buffer = UserBuffersOutputBuffer::syscall_new(
                current_task.mm().unwrap(),
                smallvec![UserBuffer { address: addr, length: 2 * DIRECT_READ_THRESHOLD }],
            )
            .expect("syscall_new");
            assert_eq!(file.read_at(current_task, 2, &mut buffer), Ok(2 * DIRECT_READ_THRESHOLD));
            assert_eq!(
                current_task
                    .read_memory_to_vec(addr, 2 * DIRECT_READ_THRESHOLD)
                    .expect("read_memory_to_vec"),
                &content[2..2 + 2 * DIRECT_READ_THRESHOLD]
            );

            // A two-segment read whose second segment is unmapped returns the bytes read into the
            // first segment.
            current_task
                .mm()
                .unwrap()
                .unmap(second_addr, READ_SIZE - first_len)
                .expect("unmap second segment");
            let mut buffer = UserBuffersOutputBuffer::unified_new(
                current_task,
                smallvec![
                    UserBuffer { address: addr, length: first_len },
                    UserBuffer { address: second_addr, length: READ_SIZE - first_len },
                ],
            )
            .expect("unified_new");
            assert_eq!(file.read_at(current_task, 3, &mut buffer), Ok(first_len));
            assert_eq!(buffer.bytes_written(), first_len);
            assert_eq!(
                current_task.read_memory_to_vec(addr, first_len).expect("read_memory_to_vec"),
                &content[3..3 + first_len]
            );

            // Reading into an unmapped user buffer fails with `EFAULT` and leaves the buffer
            // unchanged.
            current_task.mm().unwrap().unmap(addr, first_len).expect("unmap first segment");
            let mut buffer = UserBuffersOutputBuffer::unified_new_at(current_task, addr, first_len)
                .expect("unified_new_at");
            assert_eq!(file.read_at(current_task, 0, &mut buffer), error!(EFAULT));
            assert_eq!(buffer.available(), first_len);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_write_from_user_buffer() {
        spawn_kernel_and_run(async |current_task| {
            let file =
                new_memfd(current_task, b"write_user".into(), SealFlags::empty(), OpenFlags::RDWR)
                    .expect("new_memfd");
            const LEN: usize = 64 * 1024;
            let pattern = test_pattern(LEN);
            let addr = map_memory(current_task, UserAddress::default(), LEN as u64);
            current_task.write_memory(addr, &pattern).expect("write_memory");

            // A single segment.
            let mut data =
                UserBuffersInputBuffer::unified_new_at(current_task, addr, LEN).expect("buffer");
            assert_eq!(file.write(current_task, &mut data), Ok(LEN));
            assert_eq!(data.available(), 0);
            assert_eq!(file.node().info().size, LEN);
            assert_eq!(read_file_at(current_task, &file, 0, LEN), pattern);

            // Several segments, written at an unaligned offset past the end of the file.
            let half = LEN / 2;
            let mut data = UserBuffersInputBuffer::unified_new(
                current_task,
                smallvec![
                    UserBuffer { address: (addr + half).unwrap(), length: half },
                    UserBuffer { address: addr, length: half },
                ],
            )
            .expect("buffer");
            let offset = LEN + 3;
            assert_eq!(file.write_at(current_task, offset, &mut data), Ok(LEN));
            assert_eq!(file.node().info().size, offset + LEN);
            assert_eq!(read_file_at(current_task, &file, LEN, 3), vec![0; 3]);
            assert_eq!(read_file_at(current_task, &file, offset, half), pattern[half..]);
            assert_eq!(read_file_at(current_task, &file, offset + half, half), pattern[..half]);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_write_from_unmapped_user_buffer() {
        spawn_kernel_and_run(async |current_task| {
            let file = new_memfd(
                current_task,
                b"write_unmapped".into(),
                SealFlags::empty(),
                OpenFlags::RDWR,
            )
            .expect("new_memfd");
            const LEN: usize = 64 * 1024;
            let addr = map_memory(current_task, UserAddress::default(), LEN as u64);
            current_task.mm().unwrap().unmap(addr, LEN).expect("unmap");

            let mut data =
                UserBuffersInputBuffer::unified_new_at(current_task, addr, LEN).expect("buffer");
            assert_eq!(file.write(current_task, &mut data), error!(EFAULT));
            assert_eq!(data.available(), LEN);
            assert_eq!(file.node().info().size, 0);
            assert_eq!(file.node().info().storage_size(), 0);

            // A failing write or a 0-byte write past EOF must not extend the file or VMO size.
            let mut data =
                UserBuffersInputBuffer::unified_new_at(current_task, addr, LEN).expect("buffer");
            assert_eq!(file.write_at(current_task, 1024, &mut data), error!(EFAULT));
            assert_eq!(data.available(), LEN);
            assert_eq!(file.node().info().size, 0);
            assert_eq!(file.node().info().storage_size(), 0);

            assert_eq!(file.write_at(current_task, 1024, &mut VecInputBuffer::new(&[])), Ok(0));
            assert_eq!(file.node().info().size, 0);
            assert_eq!(file.node().info().storage_size(), 0);

            // A 2-page segment whose first page is mapped and second page is unmapped faults
            // partway through `zx_vmo_write`. Bytes copied past EOF before the fault must be
            // zeroed in the VMO (including within the existing EOF page) and not exposed if the
            // file is later extended with `ftruncate`.
            let page_size = *PAGE_SIZE as usize;
            let two_pages = page_size * 2;
            let partial_addr = map_memory(current_task, UserAddress::default(), two_pages as u64);
            let pattern = test_pattern(page_size);
            current_task.write_memory(partial_addr, &pattern).expect("write first page");
            current_task
                .mm()
                .unwrap()
                .unmap((partial_addr + page_size).unwrap(), page_size)
                .expect("unmap second page");

            const PREFIX_LEN: usize = 100;
            let prefix = test_pattern(PREFIX_LEN);
            assert_eq!(file.write(current_task, &mut VecInputBuffer::new(&prefix)), Ok(PREFIX_LEN));
            assert_eq!(file.node().info().size, PREFIX_LEN);
            assert_eq!(file.node().info().storage_size(), page_size);

            let mut data =
                UserBuffersInputBuffer::unified_new_at(current_task, partial_addr, two_pages)
                    .expect("buffer");
            assert_eq!(file.write(current_task, &mut data), error!(EFAULT));
            assert_eq!(data.available(), two_pages);
            assert_eq!(file.node().info().size, PREFIX_LEN);
            assert_eq!(file.node().info().storage_size(), page_size);

            file.ftruncate(current_task, (PREFIX_LEN + two_pages) as u64).expect("ftruncate");
            assert_eq!(read_file_at(current_task, &file, 0, PREFIX_LEN), prefix);
            assert_eq!(
                read_file_at(current_task, &file, PREFIX_LEN, two_pages),
                vec![0; two_pages]
            );
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_write_seal_grow_with_inflated_storage_size() {
        spawn_kernel_and_run(async |current_task| {
            let file =
                new_memfd(current_task, b"seal_grow".into(), SealFlags::empty(), OpenFlags::RDWR)
                    .expect("new_memfd");
            let page_size = *PAGE_SIZE as usize;
            const INITIAL_LEN: usize = 100;
            let initial = test_pattern(INITIAL_LEN);
            assert_eq!(
                file.write(current_task, &mut VecInputBuffer::new(&initial)),
                Ok(INITIAL_LEN)
            );

            // Neither a 0-byte write nor a failing direct write past EOF inflates storage_size.
            assert_eq!(file.write_at(current_task, 1 << 20, &mut VecInputBuffer::new(&[])), Ok(0));
            assert_eq!(file.node().info().storage_size(), page_size);

            let unmapped_len = page_size * 2;
            let unmapped_addr =
                map_memory(current_task, UserAddress::default(), unmapped_len as u64);
            current_task.mm().unwrap().unmap(unmapped_addr, unmapped_len).expect("unmap");
            let mut unmapped_data =
                UserBuffersInputBuffer::unified_new_at(current_task, unmapped_addr, unmapped_len)
                    .expect("buffer");
            assert_eq!(file.write_at(current_task, 1 << 20, &mut unmapped_data), error!(EFAULT));
            assert_eq!(file.node().info().storage_size(), page_size);

            // Inflate the VMO beyond `info.size` with `fallocate(FALLOC_FL_KEEP_SIZE)` and seal
            // against growing.
            file.fallocate(
                current_task,
                FallocMode::Allocate { keep_size: true },
                0,
                (1 << 20) as u64,
            )
            .expect("fallocate");
            assert_eq!(file.node().info().size, INITIAL_LEN);
            assert_eq!(file.node().info().storage_size(), 1 << 20);
            file.node()
                .write_guard_state
                .lock()
                .try_add_seal(SealFlags::GROW)
                .expect("add F_SEAL_GROW");

            // Both direct (>= 4 KiB) and buffered (< 4 KiB) writes starting in the partial EOF
            // page must fail with EPERM rather than growing the file or panicking.
            let write_len = page_size * 2;
            let addr = map_memory(current_task, UserAddress::default(), write_len as u64);
            current_task.write_memory(addr, &test_pattern(write_len)).expect("write_memory");
            let mut direct_data =
                UserBuffersInputBuffer::unified_new_at(current_task, addr, write_len)
                    .expect("buffer");
            assert_eq!(file.write_at(current_task, 0, &mut direct_data), error!(EPERM));
            assert_eq!(file.node().info().size, INITIAL_LEN);

            assert_eq!(
                file.write_at(current_task, 0, &mut VecInputBuffer::new(&[1u8; 200])),
                error!(EPERM)
            );
            assert_eq!(file.node().info().size, INITIAL_LEN);
            assert_eq!(read_file_at(current_task, &file, 0, INITIAL_LEN), initial);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_write_after_ashmem_set_size_shrink() {
        spawn_kernel_and_run(async |current_task| {
            let file = new_memfd(
                current_task,
                b"ashmem_shrink".into(),
                SealFlags::empty(),
                OpenFlags::RDWR,
            )
            .expect("new_memfd");
            assert_eq!(file.write(current_task, &mut VecInputBuffer::new(&[1u8; 100])), Ok(100));
            assert_eq!(file.ioctl(current_task, ASHMEM_SET_SIZE, 0u64.into()), Ok(SUCCESS));

            let page_size = *PAGE_SIZE as usize;
            let addr = map_memory(current_task, UserAddress::default(), page_size as u64);
            current_task.write_memory(addr, &test_pattern(page_size)).expect("write_memory");
            let mut data = UserBuffersInputBuffer::unified_new_at(current_task, addr, page_size)
                .expect("buffer");
            assert_eq!(file.write_at(current_task, 0, &mut data), error!(EIO));
            assert_eq!(data.available(), page_size);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_write_clamped_by_fsize_limit() {
        spawn_kernel_and_run(async |current_task| {
            let file =
                new_memfd(current_task, b"write_fsize".into(), SealFlags::empty(), OpenFlags::RDWR)
                    .expect("new_memfd");
            const LEN: usize = 64 * 1024;
            const LIMIT: usize = 10000;
            let pattern = test_pattern(LEN);
            let addr = map_memory(current_task, UserAddress::default(), LEN as u64);
            current_task.write_memory(addr, &pattern).expect("write_memory");
            current_task
                .thread_group()
                .limits
                .lock()
                .set(Resource::FSIZE, rlimit { rlim_cur: LIMIT as u64, rlim_max: LIMIT as u64 });
            let take_sigxfsz = |task: &CurrentTask| -> bool {
                task.write().signals_mut().take_next_where(|sig| sig.signal == SIGXFSZ).is_some()
            };

            // The write is cut short at the limit, without a signal.
            let mut data =
                UserBuffersInputBuffer::unified_new_at(current_task, addr, LEN).expect("buffer");
            assert_eq!(file.write(current_task, &mut data), Ok(LIMIT));
            assert_eq!(data.available(), LEN - LIMIT);
            assert_eq!(file.node().info().size, LIMIT);
            assert_eq!(read_file_at(current_task, &file, 0, LIMIT), pattern[..LIMIT]);
            assert!(!take_sigxfsz(current_task));

            // A write that starts at the limit fails with EFBIG and raises SIGXFSZ.
            let mut data =
                UserBuffersInputBuffer::unified_new_at(current_task, addr, LEN).expect("buffer");
            assert_eq!(file.write(current_task, &mut data), error!(EFBIG));
            assert_eq!(data.available(), LEN);
            assert_eq!(file.node().info().size, LIMIT);
            assert!(take_sigxfsz(current_task));
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_write_from_mapping_of_same_file() {
        spawn_kernel_and_run(async |current_task| {
            let file =
                new_memfd(current_task, b"write_self".into(), SealFlags::empty(), OpenFlags::RDWR)
                    .expect("new_memfd");
            let page_size = *PAGE_SIZE as usize;
            let len = page_size * 2;
            let pattern = test_pattern(len);
            assert_eq!(file.write(current_task, &mut VecInputBuffer::new(&pattern)), Ok(len));

            let fd = current_task.add_file(file.clone(), FdFlags::empty()).expect("add_file");
            let addr = do_mmap(
                current_task,
                UserAddress::default(),
                len,
                PROT_READ | PROT_WRITE,
                MAP_SHARED,
                fd,
                0,
            )
            .expect("mmap");

            // Append the mapped pages to the file, which grows the memory being read from.
            assert_eq!(file.seek(current_task, SeekTarget::End(0)), Ok(len as i64));
            let mut data =
                UserBuffersInputBuffer::unified_new_at(current_task, addr, len).expect("buffer");
            assert_eq!(file.write(current_task, &mut data), Ok(len));
            assert_eq!(file.node().info().size, len * 2);
            assert_eq!(read_file_at(current_task, &file, 0, len), pattern);
            assert_eq!(read_file_at(current_task, &file, len, len), pattern);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_write_from_unmapped_preserves_existing_memory_file() {
        spawn_kernel_and_run(async |current_task| {
            let page_size = *PAGE_SIZE as usize;
            let pattern = test_pattern(page_size);
            let vmo = zx::Vmo::create_with_opts(zx::VmoOptions::RESIZABLE, page_size as u64)
                .expect("create vmo");
            vmo.write(&pattern, 0).expect("write vmo");
            let memory = Arc::new(MemoryObject::from(vmo));
            let file = MemoryRegularFile::new_private_file(
                current_task,
                memory,
                OpenFlags::RDWR,
                "existing_vmo",
            );
            assert_eq!(file.node().info().size, page_size);
            assert_eq!(file.node().info().storage_size(), 0);

            let unmapped = map_memory(current_task, UserAddress::default(), page_size as u64);
            current_task.mm().expect("mm").unmap(unmapped, page_size).expect("unmap");
            let mut data =
                UserBuffersInputBuffer::unified_new_at(current_task, unmapped, page_size)
                    .expect("buffer");
            assert_eq!(file.write_at(current_task, page_size, &mut data), error!(EFAULT));
            assert_eq!(file.node().info().size, page_size);
            assert_eq!(file.node().info().storage_size(), page_size);
            assert_eq!(read_file_at(current_task, &file, 0, page_size), pattern);
        })
        .await;
    }

    #[::fuchsia::test]
    async fn test_st_blocks_uses_512_byte_units() {
        spawn_kernel_and_run(async |current_task| {
            let file =
                new_memfd(current_task, b"st_blocks".into(), SealFlags::empty(), OpenFlags::RDWR)
                    .expect("new_memfd");
            assert_eq!(file.write(current_task, &mut VecInputBuffer::new(&[1u8; 100])), Ok(100));
            let page_size = *PAGE_SIZE as usize;
            assert_eq!(file.node().info().blocks, page_size / 512);
            assert_eq!(file.node().info().storage_size(), page_size);
        })
        .await;
    }
}
