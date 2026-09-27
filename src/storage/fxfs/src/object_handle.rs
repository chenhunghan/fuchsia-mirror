// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::object_store::{DirType, PosixAttributes, Timestamp};
use anyhow::Error;
use async_trait::async_trait;
use std::future::Future;
use std::sync::Arc;
use storage_device::buffer::{BufferFuture, BufferRef, MutableBufferRef};
use storage_units::BlockSize;

// Some places use Default and assume that zero is an invalid object ID, so this cannot be changed
// easily.
pub const INVALID_OBJECT_ID: u64 = 0;

/// A handle for a generic object.  For objects with a data payload, use the ReadObjectHandle or
/// WriteObjectHandle traits.
pub trait ObjectHandle: Send + Sync + 'static {
    /// Returns the object identifier for this object which will be unique for the store that the
    /// object is contained in, but not necessarily unique within the entire system.
    fn object_id(&self) -> u64;

    /// Returns the filesystem block size, which should be at least as big as the device block size,
    /// but not necessarily the same.
    fn block_size(&self) -> BlockSize;

    /// Allocates a buffer for doing I/O (read and write) for the object.
    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_>;

    /// Sets tracing for this object.
    fn set_trace(&self, _v: bool) {}
}

#[derive(Clone, Debug, PartialEq)]
pub struct ObjectProperties {
    /// The number of references to this object.
    pub refs: u64,
    /// The number of bytes allocated to all extents across all attributes for this object.
    pub allocated_size: u64,
    /// The logical content size for the default data attribute of this object, i.e. the size of a
    /// file.  (Objects with no data attribute have size 0.)
    pub data_attribute_size: u64,
    /// The timestamp at which the object was created (i.e. crtime).
    pub creation_time: Timestamp,
    /// The timestamp at which the objects's data was last modified (i.e. mtime).
    pub modification_time: Timestamp,
    /// The timestamp at which the object was last read (i.e. atime).
    pub access_time: Timestamp,
    /// The timestamp at which the object's status was last modified (i.e. ctime).
    pub change_time: Timestamp,
    /// The number of sub-directories.
    pub sub_dirs: u64,
    /// POSIX attributes: mode, uid, gid, rdev
    pub posix_attributes: Option<PosixAttributes>,
    /// The type of directory (encryption, casefolding, etc.)
    pub dir_type: DirType,
}

#[async_trait]
pub trait ReadObjectHandle: ObjectHandle {
    /// Fills `buf` with bytes read from `offset` on the underlying device.
    ///
    /// Both `offset` and `buf.len()` must be aligned to the object's `block_size()`.
    ///
    /// Returns the number of bytes read. If `offset >= size`, returns 0. Holes/sparse extents
    /// within the read range are zero-filled. Callers should not make any assumptions about the
    /// contents of the buffer past the returned read amount.
    async fn read_aligned(&self, offset: u64, buf: MutableBufferRef<'_>) -> Result<usize, Error>;

    /// Returns the size of the object.
    fn get_size(&self) -> u64;
}

pub trait WriteObjectHandle: ObjectHandle {
    /// Writes |buf.len())| bytes at |offset| (or the end of the file), returning the object size
    /// after writing.
    /// The writes may be cached, in which case a later call to |flush| is necessary to persist the
    /// writes.
    fn write_or_append(
        &self,
        offset: Option<u64>,
        buf: BufferRef<'_>,
    ) -> impl Future<Output = Result<u64, Error>> + Send;

    /// Truncates the object to |size| bytes.
    /// The truncate may be cached, in which case a later call to |flush| is necessary to persist
    /// the truncate.
    fn truncate(&self, size: u64) -> impl Future<Output = Result<(), Error>> + Send;

    /// Flushes all pending data and metadata updates for the object.
    fn flush(&self) -> impl Future<Output = Result<(), Error>> + Send;
}

/// This trait is an asynchronous streaming writer.
pub trait WriteBytes: Sized {
    fn block_size(&self) -> BlockSize;

    /// Buffers writes to be written to the underlying handle. This may flush bytes immediately
    /// or when buffers are full.
    fn write_bytes(&mut self, buf: &[u8]) -> impl Future<Output = Result<(), Error>> + Send;

    /// Called to flush to the handle. The total number of bytes written is returned.
    fn complete(self) -> impl Future<Output = Result<u64, Error>> + Send;

    /// Moves the offset forward by `amount`, which will result in zeroes in the output stream, even
    /// if no other data is appended to it.
    fn skip(&mut self, amount: u64) -> impl Future<Output = Result<(), Error>> + Send;
}

impl LayerObject for dyn ReadObjectHandle + '_ {}

/// A handle for reading layer objects.
#[async_trait]
pub trait LayerObject: ReadObjectHandle {
    /// Returns a memory-mapped slice of the entire layer file if supported (e.g. when backed by a
    /// pager-managed VMO).
    fn as_slice(&self) -> Option<&[u8]> {
        None
    }

    /// Returns true if an underlying I/O error occurred while paging in data for this object.
    fn has_io_error(&self) -> bool {
        false
    }

    /// Requests that cached data (such as paged-in pages) for this object be purged.
    fn purge_cached_data(&self) {}

    /// Called when the layer is closed to release any external resources (such as pager
    /// registrations).
    async fn close(&self) {}
}

impl<T: ObjectHandle + ?Sized> ObjectHandle for Arc<T> {
    fn object_id(&self) -> u64 {
        (**self).object_id()
    }

    fn block_size(&self) -> BlockSize {
        (**self).block_size()
    }

    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        (**self).allocate_buffer(size)
    }

    fn set_trace(&self, v: bool) {
        (**self).set_trace(v)
    }
}

#[async_trait]
impl<T: ReadObjectHandle + ?Sized> ReadObjectHandle for Arc<T> {
    async fn read_aligned(&self, offset: u64, buf: MutableBufferRef<'_>) -> Result<usize, Error> {
        (**self).read_aligned(offset, buf).await
    }

    fn get_size(&self) -> u64 {
        (**self).get_size()
    }
}

#[async_trait]
impl<T: LayerObject + ?Sized> LayerObject for Arc<T> {
    fn as_slice(&self) -> Option<&[u8]> {
        (**self).as_slice()
    }

    fn has_io_error(&self) -> bool {
        (**self).has_io_error()
    }

    fn purge_cached_data(&self) {
        (**self).purge_cached_data()
    }

    async fn close(&self) {
        (**self).close().await
    }
}

impl<T: ObjectHandle + ?Sized> ObjectHandle for Box<T> {
    fn object_id(&self) -> u64 {
        (**self).object_id()
    }

    fn block_size(&self) -> BlockSize {
        (**self).block_size()
    }

    fn allocate_buffer(&self, size: usize) -> BufferFuture<'_> {
        (**self).allocate_buffer(size)
    }

    fn set_trace(&self, v: bool) {
        (**self).set_trace(v)
    }
}

#[async_trait]
impl<T: ReadObjectHandle + ?Sized> ReadObjectHandle for Box<T> {
    async fn read_aligned(&self, offset: u64, buf: MutableBufferRef<'_>) -> Result<usize, Error> {
        (**self).read_aligned(offset, buf).await
    }

    fn get_size(&self) -> u64 {
        (**self).get_size()
    }
}

#[async_trait]
impl<T: LayerObject + ?Sized> LayerObject for Box<T> {
    fn as_slice(&self) -> Option<&[u8]> {
        (**self).as_slice()
    }

    fn has_io_error(&self) -> bool {
        (**self).has_io_error()
    }

    fn purge_cached_data(&self) {
        (**self).purge_cached_data()
    }

    async fn close(&self) {
        (**self).close().await
    }
}
