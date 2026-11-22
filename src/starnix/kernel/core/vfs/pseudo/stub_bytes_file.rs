// Copyright 2025 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::task::CurrentTask;
use crate::vfs::pseudo::simple_file::{BytesFile, BytesFileOps};
use crate::vfs::{FileObject, FsNodeOps};
use bstr::ByteSlice;
use starnix_logging::BugRef;
use starnix_sync::{LockDepMutex, StubBytesFileStateLock};
use starnix_uapi::error;
use starnix_uapi::errors::Errno;
use std::borrow::Cow;
use std::panic::Location;

enum StubBytesData {
    ReadOnly(Box<[u8]>),
    Writable(LockDepMutex<Vec<u8>, StubBytesFileStateLock>),
}

pub struct StubBytesFile {
    data: StubBytesData,
    bug: BugRef,
    location: &'static Location<'static>,
}

impl StubBytesFile {
    #[track_caller]
    pub fn new_node(bug: BugRef) -> impl FsNodeOps {
        Self::new_node_with_data(bug, vec![])
    }

    #[track_caller]
    pub fn new_node_with_data(bug: BugRef, initial_data: impl Into<Vec<u8>>) -> impl FsNodeOps {
        BytesFile::new_node(Self::new_internal(
            bug,
            StubBytesData::Writable(LockDepMutex::new(initial_data.into())),
        ))
    }

    /// Creates a read-only stub file node initialized with `initial_data` that rejects writes
    /// with `EINVAL`, suitable for `0o644` sysfs control files when the underlying feature is
    /// unsupported.
    #[track_caller]
    pub fn new_node_read_only(bug: BugRef, initial_data: impl Into<Vec<u8>>) -> impl FsNodeOps {
        BytesFile::new_node(Self::new_internal(
            bug,
            StubBytesData::ReadOnly(initial_data.into().into_boxed_slice()),
        ))
    }

    #[track_caller]
    fn new_internal(bug: BugRef, data: StubBytesData) -> Self {
        StubBytesFile { data, bug, location: Location::caller() }
    }
}

impl BytesFileOps for StubBytesFile {
    fn write(&self, _current_task: &CurrentTask, data: Vec<u8>) -> Result<(), Errno> {
        match &self.data {
            StubBytesData::ReadOnly(_) => error!(EINVAL),
            StubBytesData::Writable(buf) => {
                *buf.lock() = data;
                Ok(())
            }
        }
    }

    fn read(&self, _current_task: &CurrentTask) -> Result<Cow<'_, [u8]>, Errno> {
        match &self.data {
            StubBytesData::ReadOnly(bytes) => Ok(Cow::Borrowed(bytes)),
            StubBytesData::Writable(buf) => Ok(Cow::Owned(buf.lock().clone())),
        }
    }

    fn open(&self, file: &FileObject, current_task: &CurrentTask) -> Result<(), Errno> {
        let path = file.name.path(&current_task.fs());
        starnix_logging::__track_stub_inner(
            self.bug,
            path.to_str_lossy().as_ref(),
            None,
            self.location,
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::spawn_kernel_and_run;
    use crate::vfs::anon_node::anon_fs;
    use crate::vfs::buffers::{VecInputBuffer, VecOutputBuffer};
    use crate::vfs::{FsNodeInfo, NamespaceNode};
    use starnix_logging::bug_ref;
    use starnix_uapi::auth::FsCred;
    use starnix_uapi::errno;
    use starnix_uapi::file_mode::mode;
    use starnix_uapi::open_flags::OpenFlags;

    #[::fuchsia::test]
    async fn stub_bytes_file_writable_and_read_only() {
        spawn_kernel_and_run(async |current_task| {
            let fs = anon_fs(current_task.kernel());
            let read_all = |file: &FileObject| {
                let mut buf = VecOutputBuffer::new(64);
                let n = file.read(current_task, &mut buf).expect("read");
                buf.data()[..n].to_vec()
            };

            let writable_node = fs.create_node_and_allocate_node_id(
                StubBytesFile::new_node_with_data(
                    bug_ref!("https://fxbug.dev/452096300"),
                    b"initial\n",
                ),
                FsNodeInfo::new(mode!(IFREG, 0o644), FsCred::root()),
            );
            let writable_name = NamespaceNode::new_anonymous_unrooted(current_task, writable_node);
            let writer = writable_name.open(current_task, OpenFlags::RDWR).expect("open writer");
            let reader = writable_name.open(current_task, OpenFlags::RDONLY).expect("open reader");

            assert_eq!(read_all(&writer), b"initial\n");
            let mut input = VecInputBuffer::new(b"updated\n");
            assert_eq!(writer.write(current_task, &mut input), Ok(b"updated\n".len()));
            // Verify a separate open handle observes the update through `BytesFile::new_node`'s
            // shared `Arc`.
            assert_eq!(read_all(&reader), b"updated\n");

            let read_only_node = fs.create_node_and_allocate_node_id(
                StubBytesFile::new_node_read_only(
                    bug_ref!("https://fxbug.dev/452096300"),
                    b"initial\n",
                ),
                FsNodeInfo::new(mode!(IFREG, 0o644), FsCred::root()),
            );
            let read_only_name =
                NamespaceNode::new_anonymous_unrooted(current_task, read_only_node);
            let ro_file = read_only_name.open(current_task, OpenFlags::RDWR).expect("open ro");
            assert_eq!(read_all(&ro_file), b"initial\n");
            let mut input = VecInputBuffer::new(b"updated\n");
            assert_eq!(ro_file.write(current_task, &mut input), Err(errno!(EINVAL)));
            let ro_reopen = read_only_name.open(current_task, OpenFlags::RDONLY).expect("reopen");
            assert_eq!(read_all(&ro_reopen), b"initial\n");

            // Confirm read-only stubs borrow directly without cloning.
            let direct_ro = StubBytesFile::new_internal(
                bug_ref!("https://fxbug.dev/452096300"),
                StubBytesData::ReadOnly(b"initial\n".to_vec().into_boxed_slice()),
            );
            assert!(matches!(direct_ro.read(current_task), Ok(Cow::Borrowed(b"initial\n"))));
        })
        .await;
    }
}
