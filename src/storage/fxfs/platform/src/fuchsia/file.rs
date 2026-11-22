// Copyright 2021 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::fuchsia::directory::FxDirectory;
use crate::fuchsia::errors::map_to_status;
use crate::fuchsia::node::{FxNode, OpenedNode};
use crate::fuchsia::paged_object_handle::{BACKGROUND_FLUSH_THRESHOLD, PagedObjectHandle};
use crate::fuchsia::pager::{
    MarkDirtyRange, PageInRange, PagerBacked, PagerPacketReceiverRegistration, default_page_in,
};
use crate::fuchsia::volume::{FxVolume, READ_AHEAD_SIZE};
use anyhow::Error;
use fidl_fuchsia_io as fio;
use fxfs::filesystem::{MAX_FILE_SIZE, SyncOptions};
use fxfs::future_with_guard::FutureWithGuard;
use fxfs::log::*;
use fxfs::object_handle::{ObjectHandle, ReadObjectHandle};
use fxfs::object_store::data_object_handle::OverwriteOptions;
use fxfs::object_store::object_record::EncryptionKey;
use fxfs::object_store::transaction::{LockKey, Options, lock_keys};
use fxfs::object_store::{DataObjectHandle, FSCRYPT_KEY_ID, ObjectDescriptor};
use fxfs_crypto::WrappingKeyId;
use fxfs_macros::ToWeakNode;
use fxfs_trace::{TraceFutureExt, trace_future_args};
use std::fmt::{Debug, Formatter};
use std::ops::Range;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use storage_device::buffer;
use storage_units::BlockSize;
use vfs::directory::entry::{EntryInfo, GetEntryInfo};
use vfs::directory::entry_container::MutableDirectory;
use vfs::execution_scope::ExecutionScope;
use vfs::file::{File, FileOptions, GetVmo, StreamIoConnection, SyncMode};
use vfs::name::Name;
use vfs::{ObjectRequestRef, ProtocolsExt, attributes};
use zx::Status;

/// In many operating systems, it is possible to delete a file with open handles. In this case the
/// file will continue to use space on disk but will not openable and the storage it uses will be
/// freed when the last handle to the file is closed.
/// To provide this behaviour, we use this constant to denote files that are marked for deletion.
///
/// When the top bit of the open count is set, it means the file has been deleted and when the count
/// drops to zero, it will be tombstoned.  Once it has dropped to zero, it cannot be opened again
/// (assertions will fire).
const TO_BE_PURGED: u64 = 1 << (u64::BITS - 1);

/// This is the second most significant bit of `open_count`. It set, it indicates that the file is
/// an unnamed temporary file (i.e. it lives in the graveyard *temporarily* and can be moved out if
/// it was linked into the filesystem permanently). An unnamed temporary file can be linked into a
/// directory, which gives it a name and makes it permanent. Internally, linking a regular file and
/// an unnamed temporary file is handled slightly differently because the latter resides in the
/// graveyard. We need to be able to identify if a file is an unnamed temporary file whenever there
/// is an attempt to link it into a directory. Once it has been linked into the filesystem, it is no
/// longer temporary (it does not reside in the graveyard anymore) and this bit will be set to 0.
const IS_TEMPORARILY_IN_GRAVEYARD: u64 = 1 << (u64::BITS - 2);

/// The file is dirty and needs to be flushed.  When this bit is set, we hold a strong count to
/// ensure the file cannot be dropped.
const IS_DIRTY: u64 = 1 << (u64::BITS - 3);

/// An unnamed temporary file lives in the graveyard and has to marked to be purged to make sure
/// that the storage this file uses will be freed when the last handle to it closes.
const IS_UNNAMED_TEMPORARY: u64 = IS_TEMPORARILY_IN_GRAVEYARD | TO_BE_PURGED;

/// The maximum value of open counts. The two most significant bits are used to indicate other
/// information regarding the state of the file. See the consts defined above.
const MAX_OPEN_COUNTS: u64 = IS_DIRTY - 1;

#[derive(Clone, Copy)]
struct State(u64);

impl Debug for State {
    fn fmt(&self, f: &mut Formatter<'_>) -> Result<(), std::fmt::Error> {
        f.debug_struct("State")
            .field("open_count", &self.open_count())
            .field("to_be_purged", &self.to_be_purged())
            .field("is_temporarily_in_graveyard", &self.is_temporarily_in_graveyard())
            .field("is_dirty", &self.is_dirty())
            .finish()
    }
}

impl State {
    fn open_count(&self) -> u64 {
        self.0 & MAX_OPEN_COUNTS
    }

    fn to_be_purged(&self) -> bool {
        self.0 & TO_BE_PURGED != 0
    }

    fn is_temporarily_in_graveyard(&self) -> bool {
        self.0 & IS_TEMPORARILY_IN_GRAVEYARD != 0
    }

    fn is_unnamed_temporary(&self) -> bool {
        self.0 & IS_UNNAMED_TEMPORARY == IS_UNNAMED_TEMPORARY
    }

    fn will_be_tombstoned(&self) -> bool {
        self.to_be_purged() && self.open_count() == 0
    }

    fn is_dirty(&self) -> bool {
        self.0 & IS_DIRTY != 0
    }
}

/// The type of the flush to reflect the caller intent.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum FlushType {
    /// The default sync type. This flushes everything it can and if there are errors then it
    /// stops early and will try to flush the rest later.
    #[default]
    Sync,

    /// When the file is being closed and the flush needs to clean everything up. If there are
    /// errors then it will still return all resources even if that means data loss.
    LastChance,

    /// Flushing some pages in the background because there are a lot of dirty pages to push. May
    /// not get all dirty pages.
    Background,
}

/// FxFile represents an open connection to a file.
#[derive(ToWeakNode)]
pub struct FxFile {
    handle: PagedObjectHandle,
    state: AtomicU64,
    pager_packet_receiver_registration: PagerPacketReceiverRegistration<Self>,
    background_flush_running: AtomicBool,
}

#[fxfs_trace::trace]
impl FxFile {
    /// Creates a new regular FxFile.
    pub fn new(handle: DataObjectHandle<FxVolume>) -> Arc<Self> {
        let size = handle.get_size();
        Arc::new_cyclic(|weak| {
            let (vmo, pager_packet_receiver_registration) = handle
                .owner()
                .pager()
                .create_vmo(
                    weak.clone(),
                    size,
                    zx::VmoOptions::UNBOUNDED | zx::VmoOptions::TRAP_DIRTY,
                )
                .unwrap();
            vmo.set_name(&zx::Name::new("fxfs-file").unwrap()).unwrap();
            Self {
                handle: PagedObjectHandle::new(handle, vmo),
                state: AtomicU64::new(0),
                pager_packet_receiver_registration,
                background_flush_running: AtomicBool::new(false),
            }
        })
    }

    async fn prepare_connection(this: &OpenedNode<FxFile>, flags: &impl ProtocolsExt) {
        {
            let mut guard = this.pager().recorder();
            if let Some(recorder) = &mut (*guard) {
                let _ = recorder.record_open((**this).clone() as Arc<dyn FxNode>);
            }
        }
        if let Some(rights) = flags.rights() {
            if rights.intersects(fio::Operations::READ_BYTES | fio::Operations::WRITE_BYTES) {
                if let Some(fut) = this.handle.pre_fetch_keys() {
                    let store = this.handle.owner().store();
                    let fs = store.filesystem();
                    // Keep the object from being deleted until after the fetch is complete.
                    let read_lock = fs
                        .clone()
                        .lock_manager()
                        .read_lock(lock_keys!(LockKey::object(
                            store.store_object_id(),
                            this.object_id()
                        )))
                        .await
                        .into_owned(fs);
                    this.handle.owner().scope().spawn(
                        FutureWithGuard::new(read_lock, fut)
                            .trace(trace_future_args!("FxFile::pre_fetch_keys")),
                    );
                }
            }
        }
    }

    /// Creates a new connection on the given `scope`. May take a read lock on the object.
    pub async fn create_connection_async(
        this: OpenedNode<FxFile>,
        scope: ExecutionScope,
        flags: impl ProtocolsExt,
        object_request: ObjectRequestRef<'_>,
    ) -> Result<(), zx::Status> {
        Self::prepare_connection(&this, &flags).await;
        object_request
            .create_connection::<StreamIoConnection<_>, _>(scope, this.take(), flags)
            .await
    }

    /// Creates a new connection on the given `scope` using an existing `zx::Stream`.
    pub async fn create_connection_with_stream_async(
        this: OpenedNode<FxFile>,
        scope: ExecutionScope,
        flags: impl ProtocolsExt,
        object_request: ObjectRequestRef<'_>,
        stream: zx::Stream,
    ) -> Result<(), zx::Status> {
        Self::prepare_connection(&this, &flags).await;
        StreamIoConnection::create_with_stream(scope, this.take(), stream, flags, object_request)
            .await
    }

    /// Open the file as a temporary.  The file must have just been created with no other open
    /// counts.
    pub fn open_as_temporary(self: Arc<Self>) -> OpenedNode<dyn FxNode> {
        assert_eq!(self.state.swap(1 | IS_UNNAMED_TEMPORARY, Ordering::Relaxed), 0);
        OpenedNode(self)
    }

    /// Mark the state as permanent (to be used when the file is currently marked as temporary).
    pub fn mark_as_permanent(&self) {
        assert!(
            State(self.state.fetch_and(!IS_UNNAMED_TEMPORARY, Ordering::Relaxed))
                .is_unnamed_temporary()
        );
    }

    pub fn is_verified_file(&self) -> bool {
        self.handle.uncached_handle().is_verified_file()
    }

    pub fn handle(&self) -> &PagedObjectHandle {
        &self.handle
    }

    /// If this instance has not been marked to be purged, returns an OpenedNode instance.
    /// If marked for purging, returns None.
    pub fn into_opened_node(self: Arc<Self>) -> Option<OpenedNode<FxFile>> {
        self.increment_open_count().then(|| OpenedNode(self))
    }

    /// Persists any unflushed data to disk.
    ///
    /// Flush may be triggered as a background task so this requires an OpenedNode to
    /// ensure that we don't accidentally try to flush a file handle that is in the process of
    /// being removed. (See use of cache in `FxVolume::flush_all_files`.)
    #[trace]
    pub async fn flush(this: &OpenedNode<FxFile>, flush_type: FlushType) -> Result<(), Error> {
        this.handle.flush(flush_type).await.map(|_| ())
    }

    pub fn get_block_size(&self) -> BlockSize {
        self.handle.block_size()
    }

    pub async fn is_allocated(&self, start_offset: u64) -> Result<(bool, u64), Status> {
        self.handle.uncached_handle().is_allocated(start_offset).await.map_err(map_to_status)
    }

    /// Writes `content` to the file at `offset` directly through the uncached handle, bypassing the
    /// pager cache.
    ///
    /// Both `offset` and `content.len()` must be block-aligned (`self.get_block_size()`). If the
    /// write extends beyond the current end of the file, new extents are allocated and the file
    /// size is grown to `offset + content.len()`.
    // TODO(https://fxbug.dev/42171261): Might be better to have a cached/uncached mode for file and
    // call this when in uncached mode.
    pub async fn write_at_uncached(&self, offset: u64, content: &[u8]) -> Result<u64, Status> {
        let mut buf = self.handle.uncached_handle().allocate_buffer(content.len()).await;
        buf.copy_from_slice(content);
        let _ = self
            .handle
            .uncached_handle()
            .overwrite(
                offset,
                buf.as_mut(),
                OverwriteOptions { allow_allocations: true, ..Default::default() },
            )
            .await
            .map_err(map_to_status)?;
        Ok(content.len() as u64)
    }

    /// Reads `length` bytes from the file starting at `offset` directly from the uncached handle,
    /// bypassing the pager cache.
    ///
    /// Both `offset` and `length` must be block-aligned (`self.get_block_size()`). Any portion of
    /// the requested range beyond the end of the file (or in unallocated ranges) is zero-filled in
    /// the returned buffer.
    // TODO(https://fxbug.dev/42171261): Might be better to have a cached/uncached mode for file and
    // call this when in uncached mode.
    pub async fn read_at_uncached(
        &self,
        offset: u64,
        length: usize,
    ) -> Result<buffer::Buffer<'_>, Status> {
        let block_size = self.get_block_size();
        if !block_size.is_aligned(offset) || !block_size.is_aligned(length as u64) {
            return Err(Status::INVALID_ARGS);
        }
        let end = offset.checked_add(length as u64).ok_or(Status::INVALID_ARGS)?;
        self.handle.read_uncached(offset..end).await.map_err(map_to_status)
    }

    pub fn get_size_uncached(&self) -> u64 {
        self.handle.uncached_handle().get_size()
    }

    async fn fscrypt_wrapping_key_id(&self) -> Result<Option<WrappingKeyId>, zx::Status> {
        Ok(self.fscrypt_policy().await?.map(|p| p.key_identifier))
    }

    async fn fscrypt_policy(&self) -> Result<Option<fio::FscryptPolicy>, zx::Status> {
        if self.handle.store().is_encrypted() {
            if let Some(key) = self
                .handle
                .store()
                .get_keys(self.object_id())
                .await
                .map_err(map_to_status)?
                .get(FSCRYPT_KEY_ID)
            {
                match key {
                    EncryptionKey::Fxfs(fxfs_key) => {
                        return Ok(Some(fio::FscryptPolicy {
                            key_identifier: fxfs_key.wrapping_key_id,
                            flags: fio::FscryptPolicyFlags::PAD_16,
                        }));
                    }
                    EncryptionKey::FscryptInoLblk32File { key_identifier } => {
                        return Ok(Some(fio::FscryptPolicy {
                            key_identifier: *key_identifier,
                            flags: fio::FscryptPolicyFlags::PAD_16
                                | fio::FscryptPolicyFlags::IV_INO_LBLK_32,
                        }));
                    }
                    EncryptionKey::FscryptInoLblk64File { key_identifier } => {
                        return Ok(Some(fio::FscryptPolicy {
                            key_identifier: *key_identifier,
                            flags: fio::FscryptPolicyFlags::PAD_16
                                | fio::FscryptPolicyFlags::IV_INO_LBLK_64,
                        }));
                    }
                    EncryptionKey::FscryptInoLblk32Dir { .. }
                    | EncryptionKey::FscryptInoLblk64Dir { .. } => {
                        error!("Unexpected key type for file: {:?}", key);
                        return Ok(None);
                    }
                    EncryptionKey::LegacyFxfs(_) => unreachable!(),
                }
            }
        }
        Ok(None)
    }

    /// Forcibly marks the file as clean.
    pub fn force_clean(&self) {
        let old = State(self.state.fetch_and(!IS_DIRTY, Ordering::Relaxed));
        if old.is_dirty() {
            if self.handle.needs_flush() {
                warn!("File {} was forcibly marked clean; data may be lost", self.object_id(),);
                self.handle.forget_dirty_pages();
            }
            // SAFETY: The IS_DIRTY bit means we took a reference.
            unsafe {
                let _ = Arc::from_raw(self);
            }
        }
    }

    // Increments the open count by 1. Returns true if successful.
    #[must_use]
    fn increment_open_count(&self) -> bool {
        let mut old = self.load_state();
        loop {
            if old.will_be_tombstoned() {
                return false;
            }

            assert!(old.open_count() < MAX_OPEN_COUNTS);

            match self.state.compare_exchange_weak(
                old.0,
                old.0 + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return true,
                Err(new_value) => old.0 = new_value,
            }
        }
    }

    fn load_state(&self) -> State {
        State(self.state.load(Ordering::Relaxed))
    }

    /// Updates the state.  Calls `callback` to map the current state into the desired new state.
    /// This handles any bookkeeping and actions that are required by the change of state.
    fn update_state(self: &Arc<Self>, callback: impl Fn(State) -> State) {
        let mut old = self.load_state();
        loop {
            let mut new = callback(old);
            if new.will_be_tombstoned() {
                // There is no point flushing if the file is to be tombstoned, so we can clear the
                // IS_DIRTY bit.
                new.0 &= !IS_DIRTY;
            }
            match self.state.compare_exchange_weak(
                old.0,
                new.0,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => {
                    if !old.is_dirty() && new.is_dirty() {
                        // The `IS_DIRTY` bit being set means we hold an extra `Arc` reference so
                        // that the node isn't removed from the node cache whilst it still needs
                        // flushing.  A background task will periodically try and flush the file.
                        // When it flushes the file, it takes an open count, and then when it drops
                        // the open count, if the file was successfully flushed, the `IS_DIRTY` bit
                        // is cleared and the extra reference is dropped (see below).
                        let _ = Arc::into_raw(self.clone());
                    } else if old.is_dirty() && !new.is_dirty() {
                        // SAFETY: The IS_DIRTY bit means we took a reference just above.
                        unsafe {
                            let _ = Arc::from_raw(Arc::as_ptr(&self));
                        }
                    }
                    if new.will_be_tombstoned() {
                        // This node is marked `TO_BE_PURGED` and there are no more references to
                        // it. This file will be tombstoned. Actual purging is queued to be done
                        // asynchronously. We don't need to do any flushing in this case - if the
                        // file is going to be deleted anyway, there is no point.
                        self.handle.forget_dirty_pages();
                        let store = self.handle.store();
                        store
                            .filesystem()
                            .queue_tombstone_object(store.store_object_id(), self.object_id());
                    }
                    return;
                }
                Err(v) => old.0 = v,
            }
        }
    }
}

impl OpenedNode<FxFile> {
    pub fn vmo(&self) -> &zx::Vmo {
        self.0.vmo()
    }
}

impl Drop for FxFile {
    fn drop(&mut self) {
        let volume = self.handle.owner();
        volume.cache().remove(self);
    }
}

impl FxNode for FxFile {
    fn object_id(&self) -> u64 {
        self.handle.object_id()
    }

    fn parent(&self) -> Option<Arc<FxDirectory>> {
        unreachable!(); // Add a parent back-reference if needed.
    }

    fn set_parent(&self, _parent: Arc<FxDirectory>) {
        // NOP
    }

    fn open_count_add_one(&self) {
        assert!(self.increment_open_count());
    }

    fn open_count_sub_one(self: Arc<Self>) {
        self.update_state(|old| {
            let mut new = State(old.0 - 1);

            // If the file is dirty, we need to hold a strong reference to make sure the file
            // doesn't go away until it has been flushed.
            if new.open_count() == 0 && !new.to_be_purged() {
                if self.handle.needs_flush() {
                    new.0 |= IS_DIRTY;
                } else {
                    new.0 &= !IS_DIRTY;
                }
            }

            new
        });
    }

    fn object_descriptor(&self) -> ObjectDescriptor {
        ObjectDescriptor::File
    }

    fn terminate(&self) {
        self.pager_packet_receiver_registration.stop_watching_for_zero_children();
    }

    fn mark_to_be_purged(self: Arc<Self>) {
        self.update_state(|old| State(old.0 | TO_BE_PURGED));
    }
}

impl GetEntryInfo for FxFile {
    fn entry_info(&self) -> EntryInfo {
        EntryInfo::new(self.object_id(), fio::DirentType::File)
    }
}

impl vfs::node::Node for FxFile {
    async fn get_attributes(
        &self,
        requested_attributes: fio::NodeAttributesQuery,
    ) -> Result<fio::NodeAttributes2, zx::Status> {
        let needs_props = requested_attributes.intersects(
            !(fio::NodeAttributesQuery::PROTOCOLS
                | fio::NodeAttributesQuery::ABILITIES
                | fio::NodeAttributesQuery::ID),
        );
        let mut props = if needs_props {
            Some(self.handle.get_properties().await.map_err(map_to_status)?)
        } else {
            None
        };

        // In most cases, the reference count of objects can be used as the link count. There are
        // two cases where this is not the case - for unnamed temporary files and unlink files with
        // no more open references to it. For these two cases, the link count should be zero (the
        // object reference count is one as they live in the graveyard). In both cases,
        // `TO_BE_PURGED` will be set and `refs` is one.
        let to_be_purged = self.load_state().to_be_purged();
        let link_count =
            props.as_ref().map(|p| if to_be_purged && p.refs == 1 { 0 } else { p.refs });

        if requested_attributes.contains(fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE) {
            self.handle
                .store()
                .update_access_time(self.handle.object_id(), props.as_mut().unwrap(), || true)
                .await
                .map_err(map_to_status)?;
        }

        let (verification_options, root_hash) = if requested_attributes.intersects(
            fio::NodeAttributesQuery::OPTIONS.union(fio::NodeAttributesQuery::ROOT_HASH),
        ) {
            self.handle.uncached_handle().get_descriptor().unzip()
        } else {
            (None, None)
        };

        let mut abilities = fio::Operations::GET_ATTRIBUTES
            | fio::Operations::UPDATE_ATTRIBUTES
            | fio::Operations::READ_BYTES;
        if !self.is_verified_file() && !self.handle.is_read_only() {
            abilities |= fio::Operations::WRITE_BYTES;
        }

        Ok(attributes!(
            requested_attributes,
            Mutable {
                creation_time: props.as_ref().map(|p| p.creation_time.as_nanos()),
                modification_time: props.as_ref().map(|p| p.modification_time.as_nanos()),
                access_time: props.as_ref().map(|p| p.access_time.as_nanos()),
                mode: props.as_ref().and_then(|p| p.posix_attributes.map(|a| a.mode)),
                uid: props.as_ref().and_then(|p| p.posix_attributes.map(|a| a.uid)),
                gid: props.as_ref().and_then(|p| p.posix_attributes.map(|a| a.gid)),
                rdev: props.as_ref().and_then(|p| p.posix_attributes.map(|a| a.rdev)),
                selinux_context: self
                    .handle
                    .uncached_handle()
                    .get_inline_selinux_context()
                    .await
                    .map_err(map_to_status)?,
                encryption_policy: self.fscrypt_policy().await?,
            },
            Immutable {
                protocols: fio::NodeProtocolKinds::FILE,
                abilities: abilities,
                content_size: self.handle.get_size(),
                storage_size: props.as_ref().map(|p| p.allocated_size),
                link_count: link_count,
                id: self.handle.object_id(),
                change_time: props.as_ref().map(|p| p.change_time.as_nanos()),
                options: verification_options,
                root_hash: root_hash,
                verity_enabled: self.is_verified_file(),
            }
        ))
    }

    fn will_clone(&self) {
        self.open_count_add_one();
    }

    fn close(self: Arc<Self>) {
        self.open_count_sub_one();
    }

    async fn link_into(
        self: Arc<Self>,
        destination_dir: Arc<dyn MutableDirectory>,
        name: Name,
        options: fio::LinkIntoOptions,
    ) -> Result<(), zx::Status> {
        let flags = options.flags.unwrap_or_default();
        if flags.has_unknown_bits() {
            return Err(zx::Status::INVALID_ARGS);
        }
        if !(flags - fio::LinkIntoFlags::REPLACE_EXISTING).is_empty() {
            return Err(zx::Status::NOT_SUPPORTED);
        }
        let replace = flags.contains(fio::LinkIntoFlags::REPLACE_EXISTING);

        let dir = destination_dir
            .into_any()
            .downcast::<FxDirectory>()
            .map_err(|_| zx::Status::INVALID_ARGS)?;
        let object_id = self.object_id();
        let wrapping_key_id = self.fscrypt_wrapping_key_id().await?;

        if replace {
            let extra_keys = [LockKey::object(dir.store().store_object_id(), object_id)];
            let replace_context = dir
                .directory()
                .acquire_context_for_replace(None, &name, false, &extra_keys)
                .await
                .map_err(map_to_status)?;
            dir.check_fscrypt_policy_equivalence(wrapping_key_id)?;
            let state = self.load_state();
            let is_unnamed_temporary = state.is_unnamed_temporary();
            if !is_unnamed_temporary && state.to_be_purged() {
                return Err(zx::Status::NOT_FOUND);
            }
            dir.link_object_into_allowing_replacement(
                replace_context,
                &name,
                object_id,
                ObjectDescriptor::File,
                is_unnamed_temporary,
                move || {
                    if is_unnamed_temporary {
                        self.mark_as_permanent();
                    }
                },
            )
            .await
        } else {
            let store = dir.store();
            let transaction = store
                .new_transaction(
                    lock_keys![
                        LockKey::object(store.store_object_id(), dir.object_id()),
                        LockKey::object(store.store_object_id(), object_id),
                    ],
                    Options::default(),
                )
                .await
                .map_err(map_to_status)?;
            dir.check_fscrypt_policy_equivalence(wrapping_key_id)?;
            let state = self.load_state();
            let is_unnamed_temporary = state.is_unnamed_temporary();
            if !is_unnamed_temporary && state.to_be_purged() {
                return Err(zx::Status::NOT_FOUND);
            }
            dir.link_object_into(
                transaction,
                &name,
                object_id,
                ObjectDescriptor::File,
                is_unnamed_temporary,
                move || {
                    if is_unnamed_temporary {
                        self.mark_as_permanent();
                    }
                },
            )
            .await
        }
    }

    fn query_filesystem(&self) -> Result<fio::FilesystemInfo, Status> {
        Ok(self.handle.owner().filesystem_info_for_volume())
    }

    async fn list_extended_attributes(&self) -> Result<Vec<Vec<u8>>, Status> {
        self.handle.store_handle().list_extended_attributes().await.map_err(map_to_status)
    }

    async fn get_extended_attribute(&self, name: Vec<u8>) -> Result<Vec<u8>, Status> {
        self.handle.store_handle().get_extended_attribute(name).await.map_err(map_to_status)
    }

    async fn set_extended_attribute(
        &self,
        name: Vec<u8>,
        value: Vec<u8>,
        mode: fio::SetExtendedAttributeMode,
    ) -> Result<(), Status> {
        self.handle
            .store_handle()
            .set_extended_attribute(name, value, mode.into())
            .await
            .map_err(map_to_status)
    }

    async fn remove_extended_attribute(&self, name: Vec<u8>) -> Result<(), Status> {
        self.handle.store_handle().remove_extended_attribute(name).await.map_err(map_to_status)
    }
}

impl File for FxFile {
    fn writable(&self) -> bool {
        true
    }

    async fn open_file(&self, _options: &FileOptions) -> Result<(), Status> {
        Ok(())
    }

    async fn truncate(&self, length: u64) -> Result<(), Status> {
        self.handle.truncate(length).await.map_err(map_to_status)?;
        Ok(())
    }

    async fn enable_verity(&self, options: fio::VerificationOptions) -> Result<(), Status> {
        self.handle.enable_verity(options).await.map_err(map_to_status)
    }

    // Returns a VMO handle that supports paging.
    async fn get_backing_memory(&self, flags: fio::VmoFlags) -> Result<zx::Vmo, Status> {
        // We do not support executable VMO handles.
        if flags.contains(fio::VmoFlags::EXECUTE) {
            error!("get_backing_memory does not support execute rights!");
            return Err(Status::NOT_SUPPORTED);
        }

        // If a file is verity-enabled (or read-only), disallow requests for writable reference
        // VMOs. Note that we have no way of revoking access to VMO handles that were already
        // exposed prior to `enable_verity()` being called; we must ensure that any subsequent
        // writes to such VMOs fail via pager `mark_dirty()` checks.
        if flags.contains(fio::VmoFlags::WRITE)
            && !flags.contains(fio::VmoFlags::PRIVATE_CLONE)
            && (self.is_verified_file() || self.handle.is_read_only())
        {
            return Err(Status::ACCESS_DENIED);
        }

        let vmo = self.handle.vmo();
        let mut rights = zx::Rights::BASIC | zx::Rights::MAP | zx::Rights::GET_PROPERTY;
        if flags.contains(fio::VmoFlags::READ) {
            rights |= zx::Rights::READ;
        }
        if flags.contains(fio::VmoFlags::WRITE) {
            rights |= zx::Rights::WRITE;
        }

        let child_vmo = if flags.contains(fio::VmoFlags::PRIVATE_CLONE) {
            // Allow for the VMO's content size and name to be changed even without ZX_RIGHT_WRITE.
            rights |= zx::Rights::SET_PROPERTY;
            let mut child_options = zx::VmoChildOptions::SNAPSHOT_AT_LEAST_ON_WRITE;
            if flags.contains(fio::VmoFlags::WRITE) {
                child_options |= zx::VmoChildOptions::RESIZABLE;
                rights |= zx::Rights::RESIZE;
            }
            vmo.create_child(child_options, 0, vmo.get_stream_size()?)?
        } else {
            vmo.create_child(zx::VmoChildOptions::REFERENCE, 0, 0)?
        };

        let child_vmo = child_vmo.replace_handle(rights)?;
        if self.handle.owner().pager().watch_for_zero_children(self).map_err(map_to_status)? {
            // Take an open count so that we keep this object alive if it is unlinked.
            self.open_count_add_one();
        }
        Ok(child_vmo)
    }

    async fn get_size(&self) -> Result<u64, Status> {
        Ok(self.handle.get_size())
    }

    async fn update_attributes(
        &self,
        attributes: fio::MutableNodeAttributes,
    ) -> Result<(), Status> {
        if attributes == fio::MutableNodeAttributes::default() {
            return Ok(());
        }

        self.handle.update_attributes(&attributes).await.map_err(map_to_status)?;
        Ok(())
    }

    async fn allocate(
        &self,
        offset: u64,
        length: u64,
        _mode: fio::AllocateMode,
    ) -> Result<(), Status> {
        // NB: FILE_BIG is used so the error converts to EFBIG when passed through starnix, which
        // is the required error code when the requested range is larger than the file size.
        let range = offset..offset.checked_add(length).ok_or(Status::FILE_BIG)?;
        self.handle.allocate(range).await.map_err(map_to_status)
    }

    async fn sync(&self, mode: SyncMode) -> Result<(), Status> {
        self.handle.flush(FlushType::Sync).await.map_err(map_to_status)?;

        // TODO(https://fxbug.dev/42178163): at the moment, this doesn't send a flush to the device, which
        // doesn't match minfs.
        if mode == SyncMode::Normal {
            self.handle
                .store()
                .filesystem()
                .sync(SyncOptions::default())
                .await
                .map_err(map_to_status)?;
        }

        Ok(())
    }
}

#[fxfs_trace::trace]
impl PagerBacked for FxFile {
    fn try_keep_open(self: Arc<Self>) -> Result<OpenedNode<Self>, Arc<Self>> {
        let mut old = self.load_state();
        loop {
            if old.open_count() == 0 {
                return Err(self);
            }

            assert!(old.open_count() < MAX_OPEN_COUNTS);

            match self.state.compare_exchange_weak(
                old.0,
                old.0 + 1,
                Ordering::Relaxed,
                Ordering::Relaxed,
            ) {
                Ok(_) => return Ok(OpenedNode(self)),
                Err(new_value) => old.0 = new_value,
            }
        }
    }

    fn pager(&self) -> &crate::pager::Pager {
        self.handle.owner().pager()
    }

    fn pager_packet_receiver_registration(&self) -> &PagerPacketReceiverRegistration<Self> {
        &self.pager_packet_receiver_registration
    }

    fn vmo(&self) -> &zx::Vmo {
        self.handle.vmo()
    }

    fn page_in(self: Arc<Self>, range: PageInRange<Self>) {
        default_page_in(self, range, READ_AHEAD_SIZE);
    }

    #[trace]
    fn mark_dirty(self: Arc<Self>, range: MarkDirtyRange<Self>) {
        let (valid_pages, invalid_pages) = range.split(MAX_FILE_SIZE);
        if let Some(invalid_pages) = invalid_pages {
            invalid_pages.report_failure(zx::Status::FILE_BIG);
        }
        let range = match valid_pages {
            Some(range) => range,
            None => return,
        };

        let byte_count = range.len();
        self.handle.owner().clone().report_pager_dirty(byte_count, move || {
            match self.handle.mark_dirty(range) {
                Ok(dirty_bytes) => {
                    // If there's a whole batch worth to write. Just write it. Spurious failures
                    // here are fine. This is best effort so keep it cheap.
                    if dirty_bytes > BACKGROUND_FLUSH_THRESHOLD
                        && !self.background_flush_running.swap(true, Ordering::Relaxed)
                    {
                        let owner = self.handle.owner().clone();
                        owner.spawn(async move {
                            // Ignore the result, the flush call already logs the errors.
                            let _ = self.handle.flush(FlushType::Background).await;
                            // If this future gets dropped before resetting this it means the
                            // volume is shutting down anyways.
                            self.background_flush_running.store(false, Ordering::Relaxed);
                        });
                    }
                }
                Err(_) => {
                    // Undo the report of the dirty pages since mark_dirty failed.
                    self.handle.owner().report_pager_clean(byte_count)
                }
            }
        });
    }

    fn on_zero_children(self: Arc<Self>) {
        // Drop the open count that we took in `get_backing_memory`.
        self.open_count_sub_one();
    }

    fn byte_size(&self) -> u64 {
        self.handle.uncached_size()
    }

    #[trace("len" => (range.end - range.start))]
    async fn aligned_read(&self, range: Range<u64>) -> Result<buffer::Buffer<'_>, Error> {
        // TODO(https://fxbug.dev/571622248): Consider tracking page faults and refaults for
        // `FxFile` in `page_fault_counter()`. Unlike immutable `FxBlob`s, `FxFile` is mutable,
        // making refault tracking non-trivial. Total faults are not tracked either so as to keep
        // page_faults consistent with page_refaults (both 0) for `FxFile`.
        // https://fxrev.dev/1884420 adds refault tracking so we can revisit once that lands.
        let buffer = self.handle.read_uncached(range).await?;
        Ok(buffer)
    }
}

impl GetVmo for FxFile {
    const PAGER_ON_FIDL_EXECUTOR: bool = true;

    fn get_vmo(&self) -> &zx::Vmo {
        self.vmo()
    }
}

#[cfg(test)]
mod tests {
    use super::FxFile;
    use crate::fuchsia::directory::FxDirectory;
    use crate::fuchsia::node::FxNode;
    use crate::fuchsia::paged_object_handle::{BACKGROUND_FLUSH_THRESHOLD, FLUSH_BATCH_SIZE};
    use crate::fuchsia::symlink::FxSymlink;
    use crate::fuchsia::testing::{
        TestFixture, TestFixtureOptions, close_file_checked, open_dir_checked, open_file,
        open_file_checked,
    };
    use anyhow::format_err;
    use fidl_fuchsia_io as fio;
    use fsverity_merkle::{FsVerityHasher, FsVerityHasherOptions};
    use fuchsia_async::{self as fasync, unblock};
    use fuchsia_fs::file;
    use futures::{FutureExt, join};
    use fxfs::filesystem::SyncOptions;
    use fxfs::fsck::fsck;
    use fxfs::hooks::Hooks;
    use fxfs::object_handle::INVALID_OBJECT_ID;
    use fxfs::object_store::allocator::max_extent_size_for_block_size;
    use fxfs::object_store::transaction::{
        LockKey, Options, TRANSACTION_MAX_JOURNAL_USAGE, lock_keys,
    };
    use fxfs::object_store::{Timestamp, journal};
    use fxfs_crypto::WrappingKeyId;
    use rand::{RngExt as _, rng};
    use std::pin::pin;
    use std::sync::Arc;
    use std::sync::atomic::{self, AtomicBool};
    use std::time::Duration;
    use storage_device::DeviceHolder;
    use storage_device::fake_device::FakeDevice;
    use storage_units::page_size;
    use vfs::directory::entry_container::MutableDirectory;
    use vfs::name::Name;
    use vfs::node::Node;
    use vfs::path::Path;
    use zx::Status;

    const WRAPPING_KEY_ID: WrappingKeyId = u128::to_le_bytes(123);

    #[fuchsia::test(threads = 10)]
    async fn test_empty_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let buf = file
            .read(fio::MAX_BUF)
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("read failed");
        assert!(buf.is_empty());

        let (mutable_attrs, immutable_attrs) = file
            .get_attributes(fio::NodeAttributesQuery::all())
            .await
            .expect("FIDL call failed")
            .expect("GetAttributes failed");
        assert_ne!(immutable_attrs.id.unwrap(), INVALID_OBJECT_ID);
        assert_eq!(immutable_attrs.content_size.unwrap(), 0u64);
        assert_eq!(immutable_attrs.storage_size.unwrap(), 0u64);
        assert_eq!(immutable_attrs.link_count.unwrap(), 1u64);
        assert_ne!(mutable_attrs.creation_time.unwrap(), 0u64);
        assert_ne!(mutable_attrs.modification_time.unwrap(), 0u64);
        assert_eq!(mutable_attrs.creation_time.unwrap(), mutable_attrs.modification_time.unwrap());

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_write_read() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let inputs = vec!["hello, ", "world!"];
        let expected_output = "hello, world!";
        for input in inputs {
            let bytes_written = file
                .write(input.as_bytes())
                .await
                .expect("write failed")
                .map_err(Status::err_from_raw)
                .expect("File write was successful");
            assert_eq!(bytes_written as usize, input.as_bytes().len());
        }

        let buf = file
            .read_at(fio::MAX_BUF, 0)
            .await
            .expect("read_at failed")
            .map_err(Status::err_from_raw)
            .expect("File read was successful");
        assert_eq!(buf.len(), expected_output.as_bytes().len());
        assert!(buf.iter().eq(expected_output.as_bytes().iter()));

        let (_, immutable_attributes) = file
            .get_attributes(
                fio::NodeAttributesQuery::CONTENT_SIZE | fio::NodeAttributesQuery::STORAGE_SIZE,
            )
            .await
            .expect("FIDL call failed")
            .expect("get_attributes failed");

        assert_eq!(
            immutable_attributes.content_size.unwrap(),
            expected_output.as_bytes().len() as u64
        );
        assert_eq!(immutable_attributes.storage_size.unwrap(), fixture.fs().block_size());

        let () = file
            .sync()
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("sync failed");

        let (_, immutable_attributes) = file
            .get_attributes(
                fio::NodeAttributesQuery::CONTENT_SIZE | fio::NodeAttributesQuery::STORAGE_SIZE,
            )
            .await
            .expect("FIDL call failed")
            .expect("get_attributes failed");

        assert_eq!(
            immutable_attributes.content_size.unwrap(),
            expected_output.as_bytes().len() as u64
        );
        assert_eq!(immutable_attributes.storage_size.unwrap(), fixture.fs().block_size());

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_page_in() {
        let input = "hello, world!";
        let reused_device = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                "foo",
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            let bytes_written = file
                .write(input.as_bytes())
                .await
                .expect("write failed")
                .map_err(Status::err_from_raw)
                .expect("File write was successful");
            assert_eq!(bytes_written as usize, input.as_bytes().len());
            assert!(file.sync().await.expect("Sync failed").is_ok());

            close_file_checked(file).await;
            fixture.close().await
        };

        let fixture = TestFixture::open(
            reused_device,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let vmo =
            file.get_backing_memory(fio::VmoFlags::READ).await.expect("Fidl failure").unwrap();
        let mut readback = vec![0; input.as_bytes().len()];
        assert!(vmo.read(&mut readback, 0).is_ok());
        assert_eq!(input.as_bytes(), readback);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_page_in_io_error() {
        let mut device = FakeDevice::new(8192, 512);
        let succeed_requests = Arc::new(AtomicBool::new(true));
        let succeed_requests_clone = succeed_requests.clone();
        device.set_op_callback(Box::new(move |_| {
            if succeed_requests_clone.load(atomic::Ordering::Relaxed) {
                Ok(())
            } else {
                Err(format_err!("Fake error."))
            }
        }));

        let input = "hello, world!";
        let reused_device = {
            let fixture = TestFixture::open(
                DeviceHolder::new(device),
                TestFixtureOptions { format: true, ..Default::default() },
            )
            .await;
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                "foo",
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            let bytes_written = file
                .write(input.as_bytes())
                .await
                .expect("write failed")
                .map_err(Status::err_from_raw)
                .expect("File write was successful");
            assert_eq!(bytes_written as usize, input.as_bytes().len());

            close_file_checked(file).await;
            fixture.close().await
        };

        let fixture = TestFixture::open(
            reused_device,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let vmo =
            file.get_backing_memory(fio::VmoFlags::READ).await.expect("Fidl failure").unwrap();
        succeed_requests.store(false, atomic::Ordering::Relaxed);
        let mut readback = vec![0; input.as_bytes().len()];
        assert!(vmo.read(&mut readback, 0).is_err());

        succeed_requests.store(true, atomic::Ordering::Relaxed);
        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_writes_persist() {
        let mut device = DeviceHolder::new(FakeDevice::new(8192, 512));
        for i in 0..2 {
            let fixture = TestFixture::open(
                device,
                TestFixtureOptions { format: i == 0, ..Default::default() },
            )
            .await;
            let root = fixture.root();

            let flags = if i == 0 {
                fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_READABLE | fio::PERM_WRITABLE
            } else {
                fio::PERM_READABLE | fio::PERM_WRITABLE
            };
            let file = open_file_checked(
                &root,
                "foo",
                flags | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            if i == 0 {
                let _: u64 = file
                    .write(&vec![0xaa as u8; 8192])
                    .await
                    .expect("FIDL call failed")
                    .map_err(Status::err_from_raw)
                    .expect("File write was successful");
            } else {
                let buf = file
                    .read(8192)
                    .await
                    .expect("FIDL call failed")
                    .map_err(Status::err_from_raw)
                    .expect("File read was successful");
                assert_eq!(buf, vec![0xaa as u8; 8192]);
            }

            let (_, immutable_attributes) = file
                .get_attributes(
                    fio::NodeAttributesQuery::CONTENT_SIZE | fio::NodeAttributesQuery::STORAGE_SIZE,
                )
                .await
                .expect("FIDL call failed")
                .expect("get_attributes failed");

            assert_eq!(immutable_attributes.content_size.unwrap(), 8192u64);
            assert_eq!(immutable_attributes.storage_size.unwrap(), 8192u64);

            close_file_checked(file).await;
            device = fixture.close().await;
        }
    }

    #[fuchsia::test(threads = 10)]
    async fn test_append() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let inputs = vec!["hello, ", "world!"];
        let expected_output = "hello, world!";
        for input in inputs {
            let file = open_file_checked(
                &root,
                "foo",
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::FILE_APPEND
                    | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            let bytes_written = file
                .write(input.as_bytes())
                .await
                .expect("FIDL call failed")
                .map_err(Status::err_from_raw)
                .expect("File write was successful");
            assert_eq!(bytes_written as usize, input.as_bytes().len());
            close_file_checked(file).await;
        }

        let file = open_file_checked(
            &root,
            "foo",
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;
        let buf = file
            .read_at(fio::MAX_BUF, 0)
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("File read was successful");
        assert_eq!(buf.len(), expected_output.as_bytes().len());
        assert_eq!(&buf[..], expected_output.as_bytes());

        let (_, immutable_attributes) = file
            .get_attributes(
                fio::NodeAttributesQuery::CONTENT_SIZE | fio::NodeAttributesQuery::STORAGE_SIZE,
            )
            .await
            .expect("FIDL call failed")
            .expect("get_attributes failed");

        assert_eq!(
            immutable_attributes.content_size.unwrap(),
            expected_output.as_bytes().len() as u64
        );
        assert_eq!(immutable_attributes.storage_size.unwrap(), fixture.fs().block_size());

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_seek() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let input = "hello, world!";
        let _: u64 = file
            .write(input.as_bytes())
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("File write was successful");

        {
            let offset = file
                .seek(fio::SeekOrigin::Start, 0)
                .await
                .expect("FIDL call failed")
                .map_err(Status::err_from_raw)
                .expect("seek was successful");
            assert_eq!(offset, 0);
            let buf = file
                .read(5)
                .await
                .expect("FIDL call failed")
                .map_err(Status::err_from_raw)
                .expect("File read was successful");
            assert!(buf.iter().eq("hello".as_bytes().iter()));
        }
        {
            let offset = file
                .seek(fio::SeekOrigin::Current, 2)
                .await
                .expect("FIDL call failed")
                .map_err(Status::err_from_raw)
                .expect("seek was successful");
            assert_eq!(offset, 7);
            let buf = file
                .read(5)
                .await
                .expect("FIDL call failed")
                .map_err(Status::err_from_raw)
                .expect("File read was successful");
            assert!(buf.iter().eq("world".as_bytes().iter()));
        }
        {
            let offset = file
                .seek(fio::SeekOrigin::Current, -5)
                .await
                .expect("FIDL call failed")
                .map_err(Status::err_from_raw)
                .expect("seek was successful");
            assert_eq!(offset, 7);
            let buf = file
                .read(5)
                .await
                .expect("FIDL call failed")
                .map_err(Status::err_from_raw)
                .expect("File read was successful");
            assert!(buf.iter().eq("world".as_bytes().iter()));
        }
        {
            let offset = file
                .seek(fio::SeekOrigin::End, -1)
                .await
                .expect("FIDL call failed")
                .map_err(Status::err_from_raw)
                .expect("seek was successful");
            assert_eq!(offset, 12);
            let buf = file
                .read(1)
                .await
                .expect("FIDL call failed")
                .map_err(Status::err_from_raw)
                .expect("File read was successful");
            assert!(buf.iter().eq("!".as_bytes().iter()));
        }

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_resize_extend() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let input = "hello, world!";
        let len: usize = 16 * 1024;

        let _: u64 = file
            .write(input.as_bytes())
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("File write was successful");

        let offset = file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("Seek was successful");
        assert_eq!(offset, 0);

        let () = file
            .resize(len as u64)
            .await
            .expect("resize failed")
            .map_err(Status::err_from_raw)
            .expect("resize error");

        let mut expected_buf = vec![0 as u8; len];
        expected_buf[..input.as_bytes().len()].copy_from_slice(input.as_bytes());

        let buf = file::read(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), len);
        assert_eq!(buf, expected_buf);

        // Write something at the end of the gap.
        expected_buf[len - 1..].copy_from_slice("a".as_bytes());

        let _: u64 = file
            .write_at("a".as_bytes(), (len - 1) as u64)
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("File write was successful");

        let offset = file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("Seek was successful");
        assert_eq!(offset, 0);

        let buf = file::read(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), len);
        assert_eq!(buf, expected_buf);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_resize_shrink() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let len: usize = 2 * 1024;
        let input = {
            let mut v = vec![0 as u8; len];
            for i in 0..v.len() {
                v[i] = ('a' as u8) + (i % 13) as u8;
            }
            v
        };
        let short_len: usize = 513;

        file::write(&file, &input).await.expect("File write was successful");

        let () = file
            .resize(short_len as u64)
            .await
            .expect("resize failed")
            .map_err(Status::err_from_raw)
            .expect("resize error");

        let offset = file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("Seek was successful");
        assert_eq!(offset, 0);

        let buf = file::read(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), short_len);
        assert_eq!(buf, input[..short_len]);

        // Resize to the original length and verify the data's zeroed.
        let () = file
            .resize(len as u64)
            .await
            .expect("resize failed")
            .map_err(Status::err_from_raw)
            .expect("resize error");

        let expected_buf = {
            let mut v = vec![0 as u8; len];
            v[..short_len].copy_from_slice(&input[..short_len]);
            v
        };

        let offset = file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("seek failed")
            .map_err(Status::err_from_raw)
            .expect("Seek was successful");
        assert_eq!(offset, 0);

        let buf = file::read(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), len);
        assert_eq!(buf, expected_buf);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_resize_shrink_repeated() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let orig_len: usize = 4 * 1024;
        let mut len = orig_len;
        let input = {
            let mut v = vec![0 as u8; len];
            for i in 0..v.len() {
                v[i] = ('a' as u8) + (i % 13) as u8;
            }
            v
        };
        let short_len: usize = 513;

        file::write(&file, &input).await.expect("File write was successful");

        while len > short_len {
            len -= std::cmp::min(len - short_len, 512);
            let () = file
                .resize(len as u64)
                .await
                .expect("resize failed")
                .map_err(Status::err_from_raw)
                .expect("resize error");
        }

        let offset = file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("Seek failed")
            .map_err(Status::err_from_raw)
            .expect("Seek was successful");
        assert_eq!(offset, 0);

        let buf = file::read(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), short_len);
        assert_eq!(buf, input[..short_len]);

        // Resize to the original length and verify the data's zeroed.
        let () = file
            .resize(orig_len as u64)
            .await
            .expect("resize failed")
            .map_err(Status::err_from_raw)
            .expect("resize error");

        let expected_buf = {
            let mut v = vec![0 as u8; orig_len];
            v[..short_len].copy_from_slice(&input[..short_len]);
            v
        };

        let offset = file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("seek failed")
            .map_err(Status::err_from_raw)
            .expect("Seek was successful");
        assert_eq!(offset, 0);

        let buf = file::read(&file).await.expect("File read was successful");
        assert_eq!(buf.len(), orig_len);
        assert_eq!(buf, expected_buf);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_unlink_with_open_race() {
        let fixture = Arc::new(TestFixture::new().await);
        let fixture1 = fixture.clone();
        let fixture2 = fixture.clone();
        let fixture3 = fixture.clone();
        let done = Arc::new(AtomicBool::new(false));
        let done1 = done.clone();
        let done2 = done.clone();
        join!(
            fasync::Task::spawn(async move {
                let root = fixture1.root();
                while !done1.load(atomic::Ordering::Relaxed) {
                    let file = open_file_checked(
                        &root,
                        "foo",
                        fio::Flags::FLAG_MAYBE_CREATE
                            | fio::PERM_READABLE
                            | fio::PERM_WRITABLE
                            | fio::Flags::PROTOCOL_FILE,
                        &Default::default(),
                    )
                    .await;
                    let _: u64 = file
                        .write(b"hello")
                        .await
                        .expect("write failed")
                        .map_err(Status::err_from_raw)
                        .expect("write error");
                }
            }),
            fasync::Task::spawn(async move {
                let root = fixture2.root();
                while !done2.load(atomic::Ordering::Relaxed) {
                    let file = open_file_checked(
                        &root,
                        "foo",
                        fio::Flags::FLAG_MAYBE_CREATE
                            | fio::PERM_READABLE
                            | fio::PERM_WRITABLE
                            | fio::Flags::PROTOCOL_FILE,
                        &Default::default(),
                    )
                    .await;
                    let _: u64 = file
                        .write(b"hello")
                        .await
                        .expect("write failed")
                        .map_err(Status::err_from_raw)
                        .expect("write error");
                }
            }),
            fasync::Task::spawn(async move {
                let root = fixture3.root();
                for _ in 0..300 {
                    let file = open_file_checked(
                        &root,
                        "foo",
                        fio::Flags::FLAG_MAYBE_CREATE
                            | fio::PERM_READABLE
                            | fio::PERM_WRITABLE
                            | fio::Flags::PROTOCOL_FILE,
                        &Default::default(),
                    )
                    .await;
                    assert_eq!(
                        file.close().await.expect("FIDL call failed").map_err(Status::err_from_raw),
                        Ok(())
                    );
                    root.unlink("foo", &fio::UnlinkOptions::default())
                        .await
                        .expect("FIDL call failed")
                        .expect("unlink failed");
                }
                done.store(true, atomic::Ordering::Relaxed);
            })
        );

        Arc::try_unwrap(fixture).unwrap_or_else(|_| panic!()).close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_get_backing_memory_shared_vmo_right_write() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        file.resize(4096)
            .await
            .expect("resize failed")
            .map_err(Status::err_from_raw)
            .expect("resize error");

        let vmo = file
            .get_backing_memory(fio::VmoFlags::SHARED_BUFFER | fio::VmoFlags::READ)
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::err_from_raw)
            .expect("Failed to get VMO");
        let err = vmo.write(&[0, 1, 2, 3], 0).expect_err("VMO should not be writable");
        assert_eq!(Status::ACCESS_DENIED, err);

        let vmo = file
            .get_backing_memory(
                fio::VmoFlags::SHARED_BUFFER | fio::VmoFlags::READ | fio::VmoFlags::WRITE,
            )
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::err_from_raw)
            .expect("Failed to get VMO");
        vmo.write(&[0, 1, 2, 3], 0).expect("VMO should be writable");

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_get_backing_memory_shared_vmo_right_read() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        file.resize(4096)
            .await
            .expect("resize failed")
            .map_err(Status::err_from_raw)
            .expect("resize error");

        let mut data = [0u8; 4];
        let vmo = file
            .get_backing_memory(fio::VmoFlags::SHARED_BUFFER)
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::err_from_raw)
            .expect("Failed to get VMO");
        let err = vmo.read(&mut data, 0).expect_err("VMO should not be readable");
        assert_eq!(Status::ACCESS_DENIED, err);

        let vmo = file
            .get_backing_memory(fio::VmoFlags::SHARED_BUFFER | fio::VmoFlags::READ)
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::err_from_raw)
            .expect("Failed to get VMO");
        vmo.read(&mut data, 0).expect("VMO should be readable");

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_get_backing_memory_shared_vmo_resize() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let vmo = file
            .get_backing_memory(
                fio::VmoFlags::SHARED_BUFFER | fio::VmoFlags::READ | fio::VmoFlags::WRITE,
            )
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::err_from_raw)
            .expect("Failed to get VMO");

        // No RESIZE right.
        let err = vmo.set_size(4096).expect_err("VMO should not be resizable");
        assert_eq!(Status::UNAVAILABLE, err);
        // No SET_PROPERTY right.
        let err =
            vmo.set_content_size(&10).expect_err("content size should not be directly modifiable");
        assert_eq!(Status::ACCESS_DENIED, err);

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_get_backing_memory_private_vmo_resize() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let vmo = file
            .get_backing_memory(
                fio::VmoFlags::PRIVATE_CLONE | fio::VmoFlags::READ | fio::VmoFlags::WRITE,
            )
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::err_from_raw)
            .expect("Failed to get VMO");
        vmo.set_size(10).expect("VMO should be resizable");
        vmo.set_content_size(&20).expect("content size should be modifiable");
        vmo.set_stream_size(20).expect("stream size should be modifiable");

        let vmo = file
            .get_backing_memory(fio::VmoFlags::PRIVATE_CLONE | fio::VmoFlags::READ)
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::err_from_raw)
            .expect("Failed to get VMO");
        let err = vmo.set_size(10).expect_err("VMO should not be resizable");
        assert_eq!(err, Status::ACCESS_DENIED);
        // This zeroes pages, which can't be done on a read-only VMO.
        vmo.set_stream_size(20).expect_err("stream size is not modifiable");
        vmo.set_content_size(&20).expect_err("content is not modifiable");

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn extended_attributes() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let name = b"security.selinux";
        let value_vec = b"bar".to_vec();

        {
            let (iterator_client, iterator_server) =
                fidl::endpoints::create_proxy::<fio::ExtendedAttributeIteratorMarker>();
            file.list_extended_attributes(iterator_server).expect("Failed to make FIDL call");
            let (chunk, last) = iterator_client
                .get_next()
                .await
                .expect("Failed to make FIDL call")
                .expect("Failed to get next iterator chunk");
            assert!(last);
            assert_eq!(chunk, Vec::<Vec<u8>>::new());
        }
        assert_eq!(
            file.get_extended_attribute(name)
                .await
                .expect("Failed to make FIDL call")
                .expect_err("Got successful message back for missing attribute"),
            Status::NOT_FOUND.into_raw(),
        );

        file.set_extended_attribute(
            name,
            fio::ExtendedAttributeValue::Bytes(value_vec.clone()),
            fio::SetExtendedAttributeMode::Set,
        )
        .await
        .expect("Failed to make FIDL call")
        .expect("Failed to set extended attribute");

        {
            let (iterator_client, iterator_server) =
                fidl::endpoints::create_proxy::<fio::ExtendedAttributeIteratorMarker>();
            file.list_extended_attributes(iterator_server).expect("Failed to make FIDL call");
            let (chunk, last) = iterator_client
                .get_next()
                .await
                .expect("Failed to make FIDL call")
                .expect("Failed to get next iterator chunk");
            assert!(last);
            assert_eq!(chunk, vec![name]);
        }
        assert_eq!(
            file.get_extended_attribute(name)
                .await
                .expect("Failed to make FIDL call")
                .expect("Failed to get extended attribute"),
            fio::ExtendedAttributeValue::Bytes(value_vec)
        );

        file.remove_extended_attribute(name)
            .await
            .expect("Failed to make FIDL call")
            .expect("Failed to remove extended attribute");

        {
            let (iterator_client, iterator_server) =
                fidl::endpoints::create_proxy::<fio::ExtendedAttributeIteratorMarker>();
            file.list_extended_attributes(iterator_server).expect("Failed to make FIDL call");
            let (chunk, last) = iterator_client
                .get_next()
                .await
                .expect("Failed to make FIDL call")
                .expect("Failed to get next iterator chunk");
            assert!(last);
            assert_eq!(chunk, Vec::<Vec<u8>>::new());
        }
        assert_eq!(
            file.get_extended_attribute(name)
                .await
                .expect("Failed to make FIDL call")
                .expect_err("Got successful message back for missing attribute"),
            Status::NOT_FOUND.into_raw(),
        );

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_flush_when_closed_from_on_zero_children() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        file.resize(50).await.expect("resize (FIDL) failed").expect("resize failed");

        {
            let vmo = file
                .get_backing_memory(fio::VmoFlags::READ | fio::VmoFlags::WRITE)
                .await
                .expect("get_backing_memory (FIDL) failed")
                .map_err(Status::err_from_raw)
                .expect("get_backing_memory failed");

            std::mem::drop(file);

            fasync::unblock(move || vmo.write(b"hello", 0).expect("write failed")).await;
        }

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_background_flush() {
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(65536, 512)),
            TestFixtureOptions::default(),
        )
        .await;
        {
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                "foo",
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            let stream = file.describe().await.unwrap().stream.unwrap();
            let file_id = file
                .get_attributes(fio::NodeAttributesQuery::ID)
                .await
                .unwrap()
                .unwrap()
                .1
                .id
                .unwrap();
            // Block background flush completion by holding the truncate lock.
            let truncate_guard = fixture
                .fs()
                .truncate_guard(fixture.volume().volume().store().store_object_id(), file_id)
                .await;

            let file_obj = fixture
                .volume()
                .volume()
                .cache()
                .get(file_id)
                .unwrap()
                .into_any()
                .downcast::<FxFile>()
                .unwrap();
            let file_clone = file_obj.clone();

            unblock(move || {
                let page_size = page_size().get();
                let mut offset: u64 = 0;
                while !file_clone
                    .background_flush_running
                    .load(std::sync::atomic::Ordering::Relaxed)
                {
                    assert!(
                        offset <= BACKGROUND_FLUSH_THRESHOLD * 2,
                        "Background flush not triggering"
                    );
                    stream
                        .write_at(zx::StreamWriteOptions::empty(), offset, &[0, 1, 2, 3, 4])
                        .expect("write should succeed");
                    offset += page_size;
                }
            })
            .await;

            // Release the truncate lock to unblock the writing, wait for the flush to complete.
            std::mem::drop(truncate_guard);
            const MAX_WAIT: Duration = Duration::from_secs(10);
            let wait_increments = Duration::from_millis(100);
            let mut total_waited = Duration::ZERO;
            while file_obj.background_flush_running.load(std::sync::atomic::Ordering::Relaxed) {
                total_waited += wait_increments;
                assert!(total_waited < MAX_WAIT);
                fasync::Timer::new(wait_increments).await;
            }
        }

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_get_attributes_fsverity_enabled_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let mut data: Vec<u8> = vec![0x00u8; 1052672];
        rng().fill(&mut data[..]);

        for chunk in data.chunks(8192) {
            file.write(chunk)
                .await
                .expect("FIDL call failed")
                .map_err(Status::err_from_raw)
                .expect("write failed");
        }

        let tree = fsverity_merkle::MerkleTree::from_data(
            &data,
            FsVerityHasher::Sha256(FsVerityHasherOptions::new(vec![0xFF; 8], 4096)),
        );
        let expected_root = tree.root().to_vec();

        let expected_descriptor = fio::VerificationOptions {
            hash_algorithm: Some(fio::HashAlgorithm::Sha256),
            salt: Some(vec![0xFF; 8]),
            ..Default::default()
        };

        file.enable_verity(&expected_descriptor)
            .await
            .expect("FIDL transport error")
            .expect("enable verity failed");

        let (_, immutable_attributes) = file
            .get_attributes(
                fio::NodeAttributesQuery::ROOT_HASH
                    | fio::NodeAttributesQuery::OPTIONS
                    | fio::NodeAttributesQuery::ABILITIES,
            )
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("get_attributes failed");

        assert_eq!(
            immutable_attributes
                .options
                .expect("verification options not present in immutable attributes"),
            expected_descriptor
        );
        assert_eq!(
            immutable_attributes.root_hash.expect("root hash not present in immutable attributes"),
            expected_root
        );
        assert_eq!(
            immutable_attributes.abilities.expect("abilities not present"),
            fio::Operations::GET_ATTRIBUTES
                | fio::Operations::UPDATE_ATTRIBUTES
                | fio::Operations::READ_BYTES
        );

        fixture.close().await;
    }

    /// Verify that once we enable verity on a file, it can never be written to or resized.
    /// This applies even to connections that have [`fio::PERM_WRITABLE`].
    #[fuchsia::test]
    async fn test_write_fail_fsverity_enabled_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        file.write(&[8; 8192])
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("write failed");

        // Obtain a writable VMO before enabling verity.
        let existing_vmo = file
            .get_backing_memory(fio::VmoFlags::READ | fio::VmoFlags::WRITE)
            .await
            .expect("FIDL transport error")
            .map_err(Status::err_from_raw)
            .expect("get_backing_memory failed");

        let descriptor = fio::VerificationOptions {
            hash_algorithm: Some(fio::HashAlgorithm::Sha256),
            salt: Some(vec![0xFF; 8]),
            ..Default::default()
        };

        file.enable_verity(&descriptor)
            .await
            .expect("FIDL transport error")
            .expect("enable verity failed");

        // Writes via an existing writable VMO obtained before enabling verity should fail at
        // the pager level.
        fasync::unblock(move || {
            existing_vmo
                .write(&[2; 8192], 0)
                .expect_err("write via existing VMO succeeded on fsverity-enabled file");
        })
        .await;

        async fn assert_file_is_not_writable(file: &fio::FileProxy) {
            // Writes via FIDL should fail
            file.write(&[2; 8192])
                .await
                .expect("FIDL transport error")
                .map_err(Status::err_from_raw)
                .expect_err("write succeeded on fsverity-enabled file");
            // Getting a writable VMO without PRIVATE_CLONE should fail
            file.get_backing_memory(fio::VmoFlags::READ | fio::VmoFlags::WRITE)
                .await
                .expect("FIDL transport error")
                .map_err(Status::err_from_raw)
                .expect_err("get_backing_memory with WRITE succeeded on fsverity-enabled file");
            // Getting a private clone with WRITE should succeed and not modify the file
            let vmo = file
                .get_backing_memory(
                    fio::VmoFlags::READ | fio::VmoFlags::WRITE | fio::VmoFlags::PRIVATE_CLONE,
                )
                .await
                .expect("FIDL transport error")
                .map_err(Status::err_from_raw)
                .expect("get_backing_memory with PRIVATE_CLONE failed");
            fasync::unblock(move || {
                vmo.write(&[2; 8192], 0).expect("write via private clone VMO failed");
            })
            .await;
            // Truncation should fail
            file.resize(1)
                .await
                .expect("FIDL transport error")
                .map_err(Status::err_from_raw)
                .expect_err("resize succeeded on fsverity-enabled file");
        }

        assert_file_is_not_writable(&file).await;
        close_file_checked(file).await;

        // Ensure that even if new writable connections are created, those also cannot write.
        let file =
            open_file(&root, "foo", fio::PERM_READABLE | fio::PERM_WRITABLE, &Default::default())
                .await
                .expect("failed to open fsverity-enabled file");
        assert_file_is_not_writable(&file).await;
        close_file_checked(file).await;

        // Reopen the filesystem and ensure that the file can't be written to.
        let device = fixture.close().await;
        device.ensure_unique();
        device.reopen(false);
        let fixture =
            TestFixture::open(device, TestFixtureOptions { format: false, ..Default::default() })
                .await;

        let root = fixture.root();
        let file =
            open_file(&root, "foo", fio::PERM_READABLE | fio::PERM_WRITABLE, &Default::default())
                .await
                .expect("failed to open fsverity-enabled file");
        assert_file_is_not_writable(&file).await;
        close_file_checked(file).await;

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_truncate_fail_fsverity_enabled_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        file.write(&[8; 8192])
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("write failed");

        let descriptor = fio::VerificationOptions {
            hash_algorithm: Some(fio::HashAlgorithm::Sha256),
            salt: Some(vec![0xFF; 8]),
            ..Default::default()
        };

        file.enable_verity(&descriptor)
            .await
            .expect("FIDL transport error")
            .expect("enable verity failed");

        // Shrinking via FIDL should fail
        file.resize(1)
            .await
            .expect("FIDL transport error")
            .map_err(Status::err_from_raw)
            .expect_err("shrink succeeded on fsverity-enabled file");

        // Growing via FIDL should fail
        file.resize(16384)
            .await
            .expect("FIDL transport error")
            .map_err(Status::err_from_raw)
            .expect_err("grow succeeded on fsverity-enabled file");

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_fsverity_enabled_file_verified_reads() {
        let mut data: Vec<u8> = vec![0x00u8; 1052672];
        rng().fill(&mut data[..]);
        let mut num_chunks = 0;

        let reused_device = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                "foo",
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            for chunk in data.chunks(fio::MAX_BUF as usize) {
                file.write(chunk)
                    .await
                    .expect("FIDL call failed")
                    .map_err(Status::err_from_raw)
                    .expect("write failed");
                num_chunks += 1;
            }

            let descriptor = fio::VerificationOptions {
                hash_algorithm: Some(fio::HashAlgorithm::Sha256),
                salt: Some(vec![0xFF; 8]),
                ..Default::default()
            };

            file.enable_verity(&descriptor)
                .await
                .expect("FIDL transport error")
                .expect("enable verity failed");

            assert!(file.sync().await.expect("Sync failed").is_ok());
            close_file_checked(file).await;
            fixture.close().await
        };

        let fixture = TestFixture::open(
            reused_device,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        for chunk in 0..num_chunks {
            let buffer = file
                .read(fio::MAX_BUF)
                .await
                .expect("transport error on read")
                .expect("read failed");
            let start = chunk * fio::MAX_BUF as usize;
            assert_eq!(&buffer, &data[start..start + buffer.len()]);
        }

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_enabling_verity_on_verified_file_fails() {
        let reused_device = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                "foo",
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::PROTOCOL_FILE,
                &Default::default(),
            )
            .await;

            file.write(&[1; 8192])
                .await
                .expect("FIDL call failed")
                .map_err(Status::err_from_raw)
                .expect("write failed");

            let descriptor = fio::VerificationOptions {
                hash_algorithm: Some(fio::HashAlgorithm::Sha256),
                salt: Some(vec![0xFF; 8]),
                ..Default::default()
            };

            file.enable_verity(&descriptor)
                .await
                .expect("FIDL transport error")
                .expect("enable verity failed");

            file.enable_verity(&descriptor)
                .await
                .expect("FIDL transport error")
                .expect_err("enabling verity on a verity-enabled file should fail.");

            assert!(file.sync().await.expect("Sync failed").is_ok());
            close_file_checked(file).await;
            fixture.close().await
        };

        let fixture = TestFixture::open(
            reused_device,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let descriptor = fio::VerificationOptions {
            hash_algorithm: Some(fio::HashAlgorithm::Sha256),
            salt: Some(vec![0xFF; 8]),
            ..Default::default()
        };

        file.enable_verity(&descriptor)
            .await
            .expect("FIDL transport error")
            .expect_err("enabling verity on a verity-enabled file should fail.");

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_enable_verity_failure_does_not_wedge_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        file.write(&[1; 8192])
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("write failed");

        // Failing enable_verity (e.g. missing hash algorithm) should not leave the file wedged as
        // read-only.
        file.enable_verity(&fio::VerificationOptions {
            hash_algorithm: None,
            salt: Some(vec![0xFF; 8]),
            ..Default::default()
        })
        .await
        .expect("FIDL transport error")
        .expect_err("enable_verity with missing hash_algorithm should fail");

        // Writing, resizing, and subsequently enabling verity with valid options should succeed.
        file.write(&[2; 4096])
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("write after failed enable_verity failed");
        file.resize(4096)
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("resize after failed enable_verity failed");

        file.enable_verity(&fio::VerificationOptions {
            hash_algorithm: Some(fio::HashAlgorithm::Sha256),
            salt: Some(vec![0xFF; 8]),
            ..Default::default()
        })
        .await
        .expect("FIDL transport error")
        .expect("enable_verity after prior failure failed");

        close_file_checked(file).await;
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_get_attributes_fsverity_not_enabled() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let mut data: Vec<u8> = vec![0x00u8; 8192];
        rng().fill(&mut data[..]);

        file.write(&data)
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("write failed");

        let () = file
            .sync()
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("sync failed");

        let (_, immutable_attributes) = file
            .get_attributes(fio::NodeAttributesQuery::ROOT_HASH | fio::NodeAttributesQuery::OPTIONS)
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("get_attributes failed");

        assert_eq!(immutable_attributes.options, None);
        assert_eq!(immutable_attributes.root_hash, None);

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_update_attributes_also_updates_ctime() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        // Writing to file should update ctime
        file.write("hello, world!".as_bytes())
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("write failed");
        let (_mutable_attributes, immutable_attributes) = file
            .get_attributes(fio::NodeAttributesQuery::CHANGE_TIME)
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("get_attributes failed");
        let ctime_after_write = immutable_attributes.change_time;

        // Updating file attributes updates ctime as well
        file.update_attributes(&fio::MutableNodeAttributes {
            mode: Some(111),
            gid: Some(222),
            ..Default::default()
        })
        .await
        .expect("FIDL call failed")
        .map_err(Status::err_from_raw)
        .expect("update_attributes failed");
        let (_mutable_attributes, immutable_attributes) = file
            .get_attributes(fio::NodeAttributesQuery::CHANGE_TIME)
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("get_attributes failed");
        let ctime_after_update = immutable_attributes.change_time;
        assert!(ctime_after_update > ctime_after_write);

        // Flush metadata
        file.sync()
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("sync failed");
        let (_mutable_attributes, immutable_attributes) = file
            .get_attributes(fio::NodeAttributesQuery::CHANGE_TIME)
            .await
            .expect("FIDL call failed")
            .map_err(Status::err_from_raw)
            .expect("get_attributes failed");
        let ctime_after_sync = immutable_attributes.change_time;
        assert_eq!(ctime_after_sync, ctime_after_update);
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_can_read_and_write_to_it() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let tmpfile = open_file_checked(
            &root,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;

        let buf = vec![0xaa as u8; 8];
        file::write(&tmpfile, buf.as_slice()).await.expect("Failed to write to file");

        tmpfile
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("seek failed")
            .map_err(zx::Status::err_from_raw)
            .expect("seek error");
        let read_buf = file::read(&tmpfile).await.expect("read failed");
        assert_eq!(read_buf, buf);

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_get_space_back_after_closing_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let tmpfile = open_file_checked(
            &root,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;

        const BUFFER_SIZE: u64 = 1024 * 1024;
        let buf = vec![0xaa as u8; BUFFER_SIZE as usize];
        file::write(&tmpfile, buf.as_slice()).await.expect("Failed to write to file");

        let info_after_writing_to_tmpfile = root
            .query_filesystem()
            .await
            .expect("Failed wire call to query filesystem")
            .1
            .expect("Failed to query filesystem");

        close_file_checked(tmpfile).await;

        // We will get space back soon after closing the file buy maybe not immediately.
        for i in 1..50 {
            let info = root
                .query_filesystem()
                .await
                .expect("Failed wire call to query filesystem")
                .1
                .expect("Failed to query filesystem");

            // We should claim back at least that amount of data we wrote to the file. There might
            // be some metadata left that will not be removed until compaction.
            if info_after_writing_to_tmpfile.used_bytes - info.used_bytes >= BUFFER_SIZE {
                break;
            }
            if i == 49 {
                panic!("Did not get space back from unnamed temporary file after closing it.");
            }
        }

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_get_space_back_after_closing_device() {
        const BUFFER_SIZE: u64 = 1024 * 1024;

        let (reused_device, info_after_writing_to_tmpfile) = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let tmpfile = open_file_checked(
                &root,
                ".",
                fio::Flags::PROTOCOL_FILE
                    | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                    | fio::PERM_WRITABLE,
                &fio::Options::default(),
            )
            .await;

            let buf = vec![0xaa as u8; BUFFER_SIZE as usize];
            file::write(&tmpfile, buf.as_slice()).await.expect("Failed to write to file");

            let info_after_writing_to_tmpfile = root
                .query_filesystem()
                .await
                .expect("Failed wire call to query filesystem")
                .1
                .expect("Failed to query filesystem");

            (fixture.close().await, info_after_writing_to_tmpfile)
        };

        let fixture = TestFixture::open(
            reused_device,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let info = root
            .query_filesystem()
            .await
            .expect("Failed wire call to query filesystem")
            .1
            .expect("Failed to query filesystem");

        // We should claim back at least that amount of data we wrote to the file after rebooting
        // device. There might be some metadata left that will not be removed until compaction.
        assert!(info_after_writing_to_tmpfile.used_bytes - info.used_bytes >= BUFFER_SIZE);

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_can_link_into() {
        const FILE1: &str = "foo";
        const FILE2: &str = "bar";
        const BUFFER_SIZE: u64 = 1024 * 1024;
        let buf = vec![0xaa as u8; BUFFER_SIZE as usize];

        let reused_device = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let tmpfile = open_file_checked(
                &root,
                ".",
                fio::Flags::PROTOCOL_FILE
                    | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE,
                &fio::Options::default(),
            )
            .await;

            // Link temporary unnamed file into filesystem, making it permanent.
            let (status, dst_token) = root.deprecated_get_token().await.expect("FIDL call failed");
            zx::Status::ok(status).expect("get_token failed");
            tmpfile
                .link_into(
                    zx::Event::from(dst_token.unwrap()),
                    FILE1,
                    &fio::LinkIntoOptions::default(),
                )
                .await
                .expect("link_into wire message failed")
                .map_err(zx::Status::err_from_raw)
                .expect("link_into failed");

            // We should be able to link the temporary file proxy multiple times.
            let (status, dst_token) = root.deprecated_get_token().await.expect("FIDL call failed");
            zx::Status::ok(status).expect("get_token failed");
            tmpfile
                .link_into(
                    zx::Event::from(dst_token.unwrap()),
                    FILE2,
                    &fio::LinkIntoOptions::default(),
                )
                .await
                .expect("link_into wire message failed")
                .map_err(zx::Status::err_from_raw)
                .expect("link_into failed");

            // Write to tmpfile, we should see the contents of it when reading from FILE1 or FILE2.
            file::write(&tmpfile, buf.as_slice()).await.expect("Failed to write to file");

            root.unlink(FILE1, &fio::UnlinkOptions::default())
                .await
                .expect("unlink wire call failed")
                .map_err(zx::Status::err_from_raw)
                .expect("unlink failed");
            fixture.close().await
        };

        let fixture = TestFixture::open(
            reused_device,
            TestFixtureOptions { format: false, ..Default::default() },
        )
        .await;
        let root = fixture.root();

        // FILE1 was unlinked, so we should not be able to open a connection to it.
        assert_eq!(
            open_file(
                &root,
                FILE1,
                fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
                &fio::Options::default()
            )
            .await
            .expect_err("Open succeeded unexpectedly")
            .root_cause()
            .downcast_ref::<zx::Status>()
            .expect("No status"),
            &zx::Status::NOT_FOUND,
        );

        // The temporary unnamed file was linked to FILE2. We should find the same contents written
        // to it.
        let permanent_file = open_file_checked(
            &root,
            FILE2,
            fio::Flags::PROTOCOL_FILE | fio::PERM_READABLE,
            &fio::Options::default(),
        )
        .await;
        permanent_file
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("seek wire message failed")
            .map_err(zx::Status::err_from_raw)
            .expect("seek error");
        let read_buf = file::read(&permanent_file).await.expect("read failed");
        assert!(read_buf == buf);

        fsck(fixture.fs().clone()).await.expect("fsck failed");

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_file_link_into_self_replacement() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_FILE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        file::write(&file, b"hello").await.expect("write failed");

        let dst_token =
            root.get_token().await.expect("FIDL call failed").expect("get_token failed");

        file.link_into(
            dst_token,
            "foo",
            &fio::LinkIntoOptions {
                flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                ..Default::default()
            },
        )
        .await
        .expect("link_into failed")
        .map_err(zx::Status::err_from_raw)
        .expect("link_into failed");

        let reopened = open_file_checked(
            &root,
            "foo",
            fio::Flags::PROTOCOL_FILE | fio::PERM_READABLE,
            &fio::Options::default(),
        )
        .await;
        assert_eq!(file::read(&reopened).await.expect("read failed"), b"hello");

        close_file_checked(reopened).await;
        close_file_checked(file).await;
        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_file_link_into_self_replacement_in_locked_directory() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();
        let crypt = fixture.crypt().unwrap();

        let encrypted_dir = open_dir_checked(
            &root,
            "encrypted_dir",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_DIRECTORY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            fio::Options::default(),
        )
        .await;
        crypt.add_wrapping_key(WRAPPING_KEY_ID, [1; 32].into()).unwrap();
        encrypted_dir
            .update_attributes(&fio::MutableNodeAttributes {
                encryption_policy: Some(fio::FscryptPolicy {
                    key_identifier: WRAPPING_KEY_ID,
                    flags: fxfs::object_store::LEGACY_FSCRYPT_FLAGS,
                }),
                ..Default::default()
            })
            .await
            .expect("update_attributes wire call failed")
            .map_err(zx::ok)
            .expect("update_attributes failed");

        let file = open_file_checked(
            &encrypted_dir,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_FILE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        file::write(&file, b"secret").await.expect("write failed");
        file.sync()
            .await
            .expect("FIDL call failed")
            .map_err(zx::Status::err_from_raw)
            .expect("sync failed");

        let sub_dir = open_dir_checked(
            &encrypted_dir,
            "subdir",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_DIRECTORY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            fio::Options::default(),
        )
        .await;
        crate::testing::close_dir_checked(sub_dir).await;

        // Lock the directory and obtain the proxy filenames for "foo" and "subdir".
        crypt.forget_wrapping_key(&WRAPPING_KEY_ID).unwrap();
        fixture.volume().volume().store().clear_caches();
        let entries = fuchsia_fs::directory::readdir(&encrypted_dir).await.expect("readdir failed");
        assert_eq!(entries.len(), 2);
        for entry in &entries {
            assert_ne!(entry.name, "foo");
            assert_ne!(entry.name, "subdir");
            let dst_token = encrypted_dir
                .get_token()
                .await
                .expect("FIDL call failed")
                .expect("get_token failed");
            let status = file
                .link_into(
                    dst_token,
                    &entry.name,
                    &fio::LinkIntoOptions {
                        flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                        ..Default::default()
                    },
                )
                .await
                .expect("link_into FIDL call failed")
                .map_err(zx::Status::err_from_raw);
            assert_eq!(status, Err(zx::Status::ACCESS_DENIED));
        }

        close_file_checked(file).await;
        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_symlink_link_into_replace_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let (symlink, symlink_server) = fidl::endpoints::create_proxy::<fio::SymlinkMarker>();
        root.create_symlink("foo", b"target", Some(symlink_server))
            .await
            .expect("FIDL call failed")
            .map_err(zx::Status::err_from_raw)
            .expect("create_symlink failed");

        let file = open_file_checked(
            &root,
            "bar",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_FILE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        file::write(&file, b"world").await.expect("write failed");
        close_file_checked(file).await;

        let dst_token =
            root.get_token().await.expect("FIDL call failed").expect("get_token failed");

        symlink
            .link_into(
                dst_token,
                "bar",
                &fio::LinkIntoOptions {
                    flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                    ..Default::default()
                },
            )
            .await
            .expect("link_into failed")
            .map_err(zx::Status::err_from_raw)
            .expect("link_into failed");

        let (bar_symlink, bar_server) = fidl::endpoints::create_proxy::<fio::SymlinkMarker>();
        root.open(
            "bar",
            fio::Flags::PROTOCOL_SYMLINK | fio::PERM_READABLE,
            &fio::Options::default(),
            bar_server.into_channel(),
        )
        .expect("open failed");
        let info = bar_symlink.describe().await.expect("describe failed");
        assert_eq!(info.target.as_deref(), Some(&b"target"[..]));

        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_file_link_into_replace_symlink() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_FILE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        file::write(&file, b"hello").await.expect("write failed");

        root.create_symlink("bar", b"target", None)
            .await
            .expect("FIDL call failed")
            .map_err(zx::Status::err_from_raw)
            .expect("create_symlink failed");

        let dst_token =
            root.get_token().await.expect("FIDL call failed").expect("get_token failed");

        file.link_into(
            dst_token,
            "bar",
            &fio::LinkIntoOptions {
                flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                ..Default::default()
            },
        )
        .await
        .expect("link_into failed")
        .map_err(zx::Status::err_from_raw)
        .expect("link_into failed");

        let bar = open_file_checked(
            &root,
            "bar",
            fio::Flags::PROTOCOL_FILE | fio::PERM_READABLE,
            &fio::Options::default(),
        )
        .await;
        let read_buf = file::read(&bar).await.expect("read failed");
        assert_eq!(read_buf, b"hello");

        close_file_checked(bar).await;
        close_file_checked(file).await;
        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_file_link_into_replace_existing_open_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file1 = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_FILE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        file::write(&file1, b"hello").await.expect("write failed");

        let file2 = open_file_checked(
            &root,
            "bar",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_FILE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        file::write(&file2, b"world").await.expect("write failed");

        let dst_token =
            root.get_token().await.expect("FIDL call failed").expect("get_token failed");

        file1
            .link_into(
                dst_token,
                "bar",
                &fio::LinkIntoOptions {
                    flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                    ..Default::default()
                },
            )
            .await
            .expect("link_into failed")
            .map_err(zx::Status::err_from_raw)
            .expect("link_into failed");

        let bar = open_file_checked(
            &root,
            "bar",
            fio::Flags::PROTOCOL_FILE | fio::PERM_READABLE,
            &fio::Options::default(),
        )
        .await;
        let read_buf = file::read(&bar).await.expect("read failed");
        assert_eq!(read_buf, b"hello");

        file2
            .seek(fio::SeekOrigin::Start, 0)
            .await
            .expect("seek")
            .map_err(zx::Status::err_from_raw)
            .expect("seek");
        let old_content = file::read(&file2).await.expect("read failed");
        assert_eq!(old_content, b"world");

        close_file_checked(bar).await;
        close_file_checked(file2).await;
        close_file_checked(file1).await;
        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_link_into_replace_existing() {
        const FILE: &str = "foo";
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        // Create an existing file.
        let file = open_file_checked(
            &root,
            FILE,
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_FILE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        file::write(&file, b"initial contents").await.expect("Failed to write to file");

        // Create an unnamed temporary file.
        let tmpfile = open_file_checked(
            &root,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        file::write(&tmpfile, b"updated contents").await.expect("Failed to write to file");

        let dst_token =
            root.get_token().await.expect("FIDL call failed").expect("get_token failed");

        // Without REPLACE_EXISTING, link_into should fail with ALREADY_EXISTS.
        let status = tmpfile
            .link_into(dst_token, FILE, &fio::LinkIntoOptions::default())
            .await
            .expect("link_into wire message failed")
            .map_err(zx::Status::err_from_raw);
        assert_eq!(status, Err(zx::Status::ALREADY_EXISTS));

        // Now link with REPLACE_EXISTING.
        let dst_token =
            root.get_token().await.expect("FIDL call failed").expect("get_token failed");
        tmpfile
            .link_into(
                dst_token,
                FILE,
                &fio::LinkIntoOptions {
                    flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                    ..Default::default()
                },
            )
            .await
            .expect("link_into wire message failed")
            .map_err(zx::Status::err_from_raw)
            .expect("link_into failed");

        // The file should now contain "updated contents".
        let file = open_file_checked(
            &root,
            FILE,
            fio::Flags::PROTOCOL_FILE | fio::PERM_READABLE,
            &fio::Options::default(),
        )
        .await;
        let data = file::read(&file).await.expect("read failed");
        assert_eq!(data, b"updated contents");

        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_link_into_cannot_replace_directory() {
        const DIR: &str = "somedir";
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        // Create a directory.
        let _dir = open_dir_checked(
            &root,
            DIR,
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_DIRECTORY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            fio::Options::default(),
        )
        .await;

        // Create an unnamed temporary file.
        let tmpfile = open_file_checked(
            &root,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;

        let dst_token =
            root.get_token().await.expect("FIDL call failed").expect("get_token failed");

        // Attempt to replace the directory with REPLACE_EXISTING. Should fail with NOT_FILE.
        let status = tmpfile
            .link_into(
                dst_token,
                DIR,
                &fio::LinkIntoOptions {
                    flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                    ..Default::default()
                },
            )
            .await
            .expect("link_into wire message failed")
            .map_err(zx::Status::err_from_raw);
        assert_eq!(status, Err(zx::Status::NOT_FILE));

        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_link_into_invalid_flags() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let tmpfile = open_file_checked(
            &root,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;

        let unknown_flag = fio::LinkIntoFlags::from_bits_allow_unknown(1 << 1);
        for flags in [unknown_flag, fio::LinkIntoFlags::REPLACE_EXISTING | unknown_flag] {
            let dst_token =
                root.get_token().await.expect("FIDL call failed").expect("get_token failed");
            let status = tmpfile
                .link_into(
                    dst_token,
                    "foo",
                    &fio::LinkIntoOptions { flags: Some(flags), ..Default::default() },
                )
                .await
                .expect("link_into wire message failed")
                .map_err(zx::Status::err_from_raw);
            assert_eq!(status, Err(zx::Status::INVALID_ARGS));
        }

        let (symlink, symlink_server) = fidl::endpoints::create_proxy::<fio::SymlinkMarker>();
        root.create_symlink("sym", b"target", Some(symlink_server))
            .await
            .expect("FIDL call failed")
            .map_err(zx::Status::err_from_raw)
            .expect("create_symlink failed");
        for flags in [unknown_flag, fio::LinkIntoFlags::REPLACE_EXISTING | unknown_flag] {
            let dst_token =
                root.get_token().await.expect("FIDL call failed").expect("get_token failed");
            let status = symlink
                .link_into(
                    dst_token,
                    "sym2",
                    &fio::LinkIntoOptions { flags: Some(flags), ..Default::default() },
                )
                .await
                .expect("link_into wire message failed")
                .map_err(zx::Status::err_from_raw);
            assert_eq!(status, Err(zx::Status::INVALID_ARGS));
        }

        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_concurrent_link_into_unnamed_temporary_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let tmpfile = open_file_checked(
            &root,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        file::write(&tmpfile, b"shared contents").await.expect("Failed to write to file");

        let (_, immutable_attrs) = tmpfile
            .get_attributes(fio::NodeAttributesQuery::ID)
            .await
            .expect("FIDL call failed")
            .map_err(zx::Status::err_from_raw)
            .expect("get_attributes failed");
        let file_id = immutable_attrs.id.unwrap();

        let volume = fixture.volume().volume();
        let file_node = volume
            .cache()
            .get(file_id)
            .expect("file node missing")
            .into_any()
            .downcast::<FxFile>()
            .unwrap();
        let root_dir = volume
            .cache()
            .get(volume.store().root_directory_object_id())
            .expect("root dir missing")
            .into_any()
            .downcast::<super::FxDirectory>()
            .unwrap();

        // Hold the transaction lock on `file_id` so both `link_into` futures reach the lock
        // acquisition point concurrently before either can commit.
        let store = volume.store();
        let txn = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), file_id)],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");

        let mut fut1 = pin!(file_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("link1".to_string()).unwrap(),
            fio::LinkIntoOptions::default(),
        ));
        let mut fut2 = pin!(file_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("link2".to_string()).unwrap(),
            fio::LinkIntoOptions::default(),
        ));

        assert!(futures::poll!(&mut fut1).is_pending());
        assert!(futures::poll!(&mut fut2).is_pending());

        drop(txn);

        let (res1, res2) = futures::join!(fut1, fut2);
        res1.expect("first link_into failed");
        res2.expect("second link_into failed");

        for name in ["link1", "link2"] {
            let f = open_file_checked(
                &root,
                name,
                fio::Flags::PROTOCOL_FILE | fio::PERM_READABLE,
                &fio::Options::default(),
            )
            .await;
            assert_eq!(file::read(&f).await.expect("read failed"), b"shared contents");
            close_file_checked(f).await;
        }

        drop(file_node);
        drop(root_dir);
        close_file_checked(tmpfile).await;
        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    macro_rules! query_attributes {
        ($proxy:expr, $query:expr) => {
            $proxy
                .get_attributes($query)
                .await
                .expect("FIDL call failed")
                .map_err(zx::Status::err_from_raw)
                .expect("get_attributes failed")
        };
    }

    fn cached_node<T: FxNode>(fixture: &TestFixture, object_id: u64) -> Arc<T> {
        fixture
            .volume()
            .volume()
            .cache()
            .get(object_id)
            .expect("node missing from cache")
            .into_any()
            .downcast::<T>()
            .unwrap_or_else(|_| panic!("unexpected node type for object {object_id}"))
    }

    async fn open_rw_file_and_id(dir: &fio::DirectoryProxy, name: &str) -> (fio::FileProxy, u64) {
        let file = open_file_checked(
            dir,
            name,
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_FILE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        let (_, immutable) = query_attributes!(file, fio::NodeAttributesQuery::ID);
        (file, immutable.id.unwrap())
    }

    async fn open_unnamed_temp_file_and_id(dir: &fio::DirectoryProxy) -> (fio::FileProxy, u64) {
        let file = open_file_checked(
            dir,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        let (_, immutable) = query_attributes!(file, fio::NodeAttributesQuery::ID);
        (file, immutable.id.unwrap())
    }

    async fn create_symlink_and_id(
        dir: &fio::DirectoryProxy,
        name: &str,
        target: &[u8],
    ) -> (fio::SymlinkProxy, u64) {
        let (symlink, server_end) = fidl::endpoints::create_proxy::<fio::SymlinkMarker>();
        dir.create_symlink(name, target, Some(server_end))
            .await
            .expect("FIDL call failed")
            .map_err(zx::Status::err_from_raw)
            .expect("create_symlink failed");
        let (_, immutable) = query_attributes!(symlink, fio::NodeAttributesQuery::ID);
        (symlink, immutable.id.unwrap())
    }

    fn set_purge_post_commit_hook<'a>(
        hooks: &mut Hooks<'a>,
        hook_ran: &'a AtomicBool,
        fixture: &TestFixture,
        replaced_id: u64,
        dst_name: &'static str,
        is_to_be_purged: impl Fn() -> bool + Send + Sync + 'a,
    ) {
        let fs = fixture.fs().clone();
        let volume = fixture.volume().volume().clone();
        let store_id = volume.store().store_object_id();
        let root_id = volume.store().root_directory_object_id();
        hooks.set_post_commit(move || {
            hook_ran.store(true, atomic::Ordering::Relaxed);
            assert!(
                is_to_be_purged(),
                "object {replaced_id} must be marked to_be_purged before transaction locks drop"
            );
            assert!(
                fs.lock_manager()
                    .txn_lock(lock_keys![LockKey::object(store_id, replaced_id)])
                    .now_or_never()
                    .is_none(),
                "object lock must still be held when post_commit runs"
            );
            assert!(
                volume
                    .dirent_cache()
                    .lookup(&(root_id, dst_name, false))
                    .is_none_or(|node| node.object_id() != replaced_id),
                "dirent_cache must no longer map {dst_name:?} to replaced object {replaced_id}"
            );
        });
    }

    async fn backdate_change_time(fixture: &TestFixture, object_id: u64) {
        let store = fixture.volume().volume().store();
        let mut transaction = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), object_id)],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");
        store
            .update_attributes(&mut transaction, object_id, None, Some(Timestamp::from_nanos(1)))
            .await
            .expect("update_attributes failed");
        transaction.commit().await.expect("commit failed");
    }

    async fn link_file_into(
        file: &fio::FileProxy,
        dir: &fio::DirectoryProxy,
        name: &str,
        replace: bool,
    ) {
        let dst_token = dir.get_token().await.expect("FIDL call failed").expect("get_token failed");
        let options = if replace {
            fio::LinkIntoOptions {
                flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                ..Default::default()
            }
        } else {
            fio::LinkIntoOptions::default()
        };
        file.link_into(dst_token, name, &options)
            .await
            .expect("FIDL call failed")
            .map_err(zx::Status::err_from_raw)
            .expect("link_into failed");
    }

    async fn link_symlink_into(
        symlink: &fio::SymlinkProxy,
        dir: &fio::DirectoryProxy,
        name: &str,
        replace: bool,
    ) {
        let dst_token = dir.get_token().await.expect("FIDL call failed").expect("get_token failed");
        let options = if replace {
            fio::LinkIntoOptions {
                flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                ..Default::default()
            }
        } else {
            fio::LinkIntoOptions::default()
        };
        symlink
            .link_into(dst_token, name, &options)
            .await
            .expect("FIDL call failed")
            .map_err(zx::Status::err_from_raw)
            .expect("symlink link_into failed");
    }

    #[fuchsia::test]
    async fn test_link_into_races_with_unlink() {
        let hook_ran = AtomicBool::new(false);
        let (mut hooks, fs_hooks) = Hooks::new();
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(16384, 512)),
            TestFixtureOptions { hooks: Some(fs_hooks), ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let (file, file_id) = open_rw_file_and_id(&root, "foo").await;
        let store = fixture.volume().volume().store();
        let file_node = cached_node::<FxFile>(&fixture, file_id);
        let root_dir = cached_node::<FxDirectory>(&fixture, store.root_directory_object_id());

        // Hold the transaction lock on `file_id` and poll `unlink` first so `unlink` completes
        // its initial lookup on `root_dir` and blocks on `file_id` (while holding `root_dir`),
        // and both `link_into` calls queue behind `unlink` before checking `to_be_purged()`.
        let txn = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), file_id)],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");

        let mut unlink_fut = pin!(root_dir.clone().unlink("foo", false));
        assert!(futures::poll!(&mut unlink_fut).is_pending());

        let mut link_fut = pin!(file_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("bar".to_string()).unwrap(),
            fio::LinkIntoOptions::default(),
        ));
        assert!(futures::poll!(&mut link_fut).is_pending());

        let mut replace_link_fut = pin!(file_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("baz".to_string()).unwrap(),
            fio::LinkIntoOptions {
                flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                ..Default::default()
            },
        ));
        assert!(futures::poll!(&mut replace_link_fut).is_pending());

        let node_for_hook = file_node.clone();
        set_purge_post_commit_hook(&mut hooks, &hook_ran, &fixture, file_id, "foo", move || {
            node_for_hook.load_state().to_be_purged()
        });

        drop(txn);
        unlink_fut.await.expect("unlink failed");
        drop(hooks);
        assert!(hook_ran.load(atomic::Ordering::Relaxed));

        assert_eq!(link_fut.await, Err(zx::Status::NOT_FOUND));
        assert_eq!(replace_link_fut.await, Err(zx::Status::NOT_FOUND));
        let (_, immutable) = query_attributes!(file, fio::NodeAttributesQuery::LINK_COUNT);
        assert_eq!(immutable.link_count, Some(0));

        drop(file_node);
        drop(root_dir);
        close_file_checked(file).await;
        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_link_into_races_with_file_link_into_replace() {
        let hook_ran = AtomicBool::new(false);
        let (mut hooks, fs_hooks) = Hooks::new();
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(16384, 512)),
            TestFixtureOptions { hooks: Some(fs_hooks), ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let (replaced_file, replaced_id) = open_rw_file_and_id(&root, "foo").await;
        let (replacer_file, replacer_id) = open_unnamed_temp_file_and_id(&root).await;

        let store = fixture.volume().volume().store();
        let replaced_node = cached_node::<FxFile>(&fixture, replaced_id);
        let replacer_node = cached_node::<FxFile>(&fixture, replacer_id);
        let root_dir = cached_node::<FxDirectory>(&fixture, store.root_directory_object_id());

        // Hold the transaction lock on `replaced_id` so the replacing `link_into` blocks after
        // looking up "foo", and concurrent `link_into` calls on `replaced_node` queue behind it.
        let txn = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), replaced_id)],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");

        let mut replace_fut = pin!(replacer_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("foo".to_string()).unwrap(),
            fio::LinkIntoOptions {
                flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                ..Default::default()
            },
        ));
        assert!(futures::poll!(&mut replace_fut).is_pending());

        let mut link_fut = pin!(replaced_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("bar".to_string()).unwrap(),
            fio::LinkIntoOptions::default(),
        ));
        assert!(futures::poll!(&mut link_fut).is_pending());

        let mut replace_link_fut = pin!(replaced_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("baz".to_string()).unwrap(),
            fio::LinkIntoOptions {
                flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                ..Default::default()
            },
        ));
        assert!(futures::poll!(&mut replace_link_fut).is_pending());

        let node_for_hook = replaced_node.clone();
        set_purge_post_commit_hook(
            &mut hooks,
            &hook_ran,
            &fixture,
            replaced_id,
            "foo",
            move || node_for_hook.load_state().to_be_purged(),
        );

        drop(txn);
        replace_fut.await.expect("replace link_into failed");
        drop(hooks);
        assert!(hook_ran.load(atomic::Ordering::Relaxed));

        assert_eq!(link_fut.await, Err(zx::Status::NOT_FOUND));
        assert_eq!(replace_link_fut.await, Err(zx::Status::NOT_FOUND));
        let (_, immutable) = query_attributes!(replaced_file, fio::NodeAttributesQuery::LINK_COUNT);
        assert_eq!(immutable.link_count, Some(0));

        drop(replaced_node);
        drop(replacer_node);
        drop(root_dir);
        close_file_checked(replaced_file).await;
        close_file_checked(replacer_file).await;
        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_link_into_races_with_rename_replace() {
        let hook_ran = AtomicBool::new(false);
        let (mut hooks, fs_hooks) = Hooks::new();
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(16384, 512)),
            TestFixtureOptions { hooks: Some(fs_hooks), ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let (replaced_file, replaced_id) = open_rw_file_and_id(&root, "target").await;
        let (src_file, _) = open_rw_file_and_id(&root, "src").await;
        close_file_checked(src_file).await;

        let store = fixture.volume().volume().store();
        let replaced_node = cached_node::<FxFile>(&fixture, replaced_id);
        let root_dir = cached_node::<FxDirectory>(&fixture, store.root_directory_object_id());

        let txn = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), replaced_id)],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");

        let mut rename_fut = pin!(MutableDirectory::rename(
            root_dir.clone(),
            root_dir.clone(),
            Path::validate_and_split("src").unwrap(),
            Path::validate_and_split("target").unwrap(),
        ));
        assert!(futures::poll!(&mut rename_fut).is_pending());

        let mut link_fut = pin!(replaced_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("target_link".to_string()).unwrap(),
            fio::LinkIntoOptions::default(),
        ));
        assert!(futures::poll!(&mut link_fut).is_pending());

        let node_for_hook = replaced_node.clone();
        set_purge_post_commit_hook(
            &mut hooks,
            &hook_ran,
            &fixture,
            replaced_id,
            "target",
            move || node_for_hook.load_state().to_be_purged(),
        );

        drop(txn);
        rename_fut.await.expect("rename failed");
        drop(hooks);
        assert!(hook_ran.load(atomic::Ordering::Relaxed));

        assert_eq!(link_fut.await, Err(zx::Status::NOT_FOUND));
        let (_, immutable) = query_attributes!(replaced_file, fio::NodeAttributesQuery::LINK_COUNT);
        assert_eq!(immutable.link_count, Some(0));

        drop(replaced_node);
        drop(root_dir);
        close_file_checked(replaced_file).await;
        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_link_into_races_with_symlink_link_into_replace() {
        let hook_ran = AtomicBool::new(false);
        let (mut hooks, fs_hooks) = Hooks::new();
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(16384, 512)),
            TestFixtureOptions { hooks: Some(fs_hooks), ..Default::default() },
        )
        .await;
        let root = fixture.root();

        let (replaced_symlink, replaced_sym_id) =
            create_symlink_and_id(&root, "sym_target", b"old_target").await;
        let (_replacer_symlink, replacer_sym_id) =
            create_symlink_and_id(&root, "sym_src", b"new_target").await;

        let store = fixture.volume().volume().store();
        let replaced_sym_node = cached_node::<FxSymlink>(&fixture, replaced_sym_id);
        let replacer_sym_node = cached_node::<FxSymlink>(&fixture, replacer_sym_id);
        let root_dir = cached_node::<FxDirectory>(&fixture, store.root_directory_object_id());

        let txn = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), replaced_sym_id)],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");

        let mut sym_replace_fut = pin!(replacer_sym_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("sym_target".to_string()).unwrap(),
            fio::LinkIntoOptions {
                flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                ..Default::default()
            },
        ));
        assert!(futures::poll!(&mut sym_replace_fut).is_pending());

        let mut sym_link_fut = pin!(replaced_sym_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("sym_resurrected".to_string()).unwrap(),
            fio::LinkIntoOptions::default(),
        ));
        assert!(futures::poll!(&mut sym_link_fut).is_pending());

        let mut sym_replace_link_fut = pin!(replaced_sym_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("sym_resurrected_replace".to_string()).unwrap(),
            fio::LinkIntoOptions {
                flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                ..Default::default()
            },
        ));
        assert!(futures::poll!(&mut sym_replace_link_fut).is_pending());

        let node_for_hook = replaced_sym_node.clone();
        set_purge_post_commit_hook(
            &mut hooks,
            &hook_ran,
            &fixture,
            replaced_sym_id,
            "sym_target",
            move || node_for_hook.to_be_purged(),
        );

        drop(txn);
        sym_replace_fut.await.expect("replace symlink link_into failed");
        drop(hooks);
        assert!(hook_ran.load(atomic::Ordering::Relaxed));

        assert_eq!(sym_link_fut.await, Err(zx::Status::NOT_FOUND));
        assert_eq!(sym_replace_link_fut.await, Err(zx::Status::NOT_FOUND));
        let (_, immutable) =
            query_attributes!(replaced_symlink, fio::NodeAttributesQuery::LINK_COUNT);
        assert_eq!(immutable.link_count, Some(0));

        drop(replaced_sym_node);
        drop(replacer_sym_node);
        drop(root_dir);
        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_link_into_before_replace_preserves_open_file() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let (replaced_file, replaced_id) = open_rw_file_and_id(&root, "foo").await;
        file::write(&replaced_file, b"original").await.expect("write failed");
        let (replacer_file, replacer_id) = open_unnamed_temp_file_and_id(&root).await;
        file::write(&replacer_file, b"replacement").await.expect("write failed");

        let store = fixture.volume().volume().store();
        let replaced_node = cached_node::<FxFile>(&fixture, replaced_id);
        let replacer_node = cached_node::<FxFile>(&fixture, replacer_id);
        let root_dir = cached_node::<FxDirectory>(&fixture, store.root_directory_object_id());

        // Queue `link_fut` ahead of `replace_fut` on `replaced_id` so the new link commits first
        // and the subsequent replacement removes only one of two links instead of marking the open
        // file to be purged.
        let txn = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), replaced_id)],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");

        let mut link_fut = pin!(replaced_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("bar".to_string()).unwrap(),
            fio::LinkIntoOptions::default(),
        ));
        assert!(futures::poll!(&mut link_fut).is_pending());

        let mut replace_fut = pin!(replacer_node.clone().link_into(
            root_dir.clone(),
            Name::try_from("foo".to_string()).unwrap(),
            fio::LinkIntoOptions {
                flags: Some(fio::LinkIntoFlags::REPLACE_EXISTING),
                ..Default::default()
            },
        ));
        assert!(futures::poll!(&mut replace_fut).is_pending());

        drop(txn);
        let (link_res, replace_res) = join!(link_fut, replace_fut);
        link_res.expect("link_into failed");
        replace_res.expect("replace link_into failed");

        assert!(!replaced_node.load_state().to_be_purged());
        let (_, immutable) = query_attributes!(replaced_file, fio::NodeAttributesQuery::LINK_COUNT);
        assert_eq!(immutable.link_count, Some(1));
        assert_eq!(
            fuchsia_fs::directory::read_file(&root, "bar").await.expect("read bar failed"),
            b"original"
        );
        assert_eq!(
            fuchsia_fs::directory::read_file(&root, "foo").await.expect("read foo failed"),
            b"replacement"
        );

        drop(replaced_node);
        drop(replacer_node);
        drop(root_dir);
        close_file_checked(replaced_file).await;
        close_file_checked(replacer_file).await;
        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_file_link_updates_change_time() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();
        let time_query =
            fio::NodeAttributesQuery::CHANGE_TIME | fio::NodeAttributesQuery::MODIFICATION_TIME;

        let (file, file_id) = open_rw_file_and_id(&root, "foo").await;
        let (mutable0, _) = query_attributes!(file, time_query);
        let mtime0 = mutable0.modification_time.unwrap();

        backdate_change_time(&fixture, file_id).await;
        link_file_into(&file, &root, "bar", false).await;
        let (file_mutable, file_immutable) = query_attributes!(file, time_query);
        let (dir_mutable, dir_immutable) = query_attributes!(root, time_query);
        assert!(file_immutable.change_time.unwrap() > 1);
        assert_eq!(file_mutable.modification_time.unwrap(), mtime0);
        assert_eq!(file_immutable.change_time, dir_immutable.change_time);
        assert_eq!(file_immutable.change_time, dir_mutable.modification_time);

        let (target, _) = open_rw_file_and_id(&root, "baz").await;
        close_file_checked(target).await;

        backdate_change_time(&fixture, file_id).await;
        link_file_into(&file, &root, "baz", true).await;
        let (file_mutable, file_immutable) = query_attributes!(file, time_query);
        let (dir_mutable, dir_immutable) = query_attributes!(root, time_query);
        assert!(file_immutable.change_time.unwrap() > 1);
        assert_eq!(file_mutable.modification_time.unwrap(), mtime0);
        assert_eq!(file_immutable.change_time, dir_immutable.change_time);
        assert_eq!(file_immutable.change_time, dir_mutable.modification_time);

        // Also verify `Directory::Link` (`link_object`) updates `change_time` to match the
        // directory update timestamp.
        backdate_change_time(&fixture, file_id).await;
        let dst_token =
            root.get_token().await.expect("FIDL call failed").expect("get_token failed");
        let status = root.link("foo", dst_token.into(), "qux").await.expect("FIDL call failed");
        zx::Status::ok(status).expect("link failed");
        let (file_mutable, file_immutable) = query_attributes!(file, time_query);
        let (dir_mutable, dir_immutable) = query_attributes!(root, time_query);
        assert!(file_immutable.change_time.unwrap() > 1);
        assert_eq!(file_mutable.modification_time.unwrap(), mtime0);
        assert_eq!(file_immutable.change_time, dir_immutable.change_time);
        assert_eq!(file_immutable.change_time, dir_mutable.modification_time);

        // Also verify linking an unnamed temporary file updates its `change_time`.
        let (tmpfile, tmp_id) = open_unnamed_temp_file_and_id(&root).await;
        let (tmp_mutable0, _) = query_attributes!(tmpfile, time_query);
        let tmp_mtime0 = tmp_mutable0.modification_time.unwrap();

        backdate_change_time(&fixture, tmp_id).await;
        link_file_into(&tmpfile, &root, "from_tmp", false).await;
        let (tmp_mutable, tmp_immutable) = query_attributes!(tmpfile, time_query);
        let (dir_mutable, dir_immutable) = query_attributes!(root, time_query);
        assert!(tmp_immutable.change_time.unwrap() > 1);
        assert_eq!(tmp_mutable.modification_time.unwrap(), tmp_mtime0);
        assert_eq!(tmp_immutable.change_time, dir_immutable.change_time);
        assert_eq!(tmp_immutable.change_time, dir_mutable.modification_time);

        close_file_checked(tmpfile).await;
        close_file_checked(file).await;
        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_symlink_link_into_updates_change_time() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();
        let time_query =
            fio::NodeAttributesQuery::CHANGE_TIME | fio::NodeAttributesQuery::MODIFICATION_TIME;

        let (symlink, sym_id) = create_symlink_and_id(&root, "sym", b"target").await;
        let (sym_mutable0, _) = query_attributes!(symlink, time_query);
        let sym_mtime0 = sym_mutable0.modification_time.unwrap();

        backdate_change_time(&fixture, sym_id).await;
        link_symlink_into(&symlink, &root, "sym_link", false).await;
        let (sym_mutable, sym_immutable) = query_attributes!(symlink, time_query);
        let (dir_mutable, dir_immutable) = query_attributes!(root, time_query);
        assert!(sym_immutable.change_time.unwrap() > 1);
        assert_eq!(sym_mutable.modification_time.unwrap(), sym_mtime0);
        assert_eq!(sym_immutable.change_time, dir_immutable.change_time);
        assert_eq!(sym_immutable.change_time, dir_mutable.modification_time);

        let (target_file, _) = open_rw_file_and_id(&root, "existing").await;
        close_file_checked(target_file).await;

        backdate_change_time(&fixture, sym_id).await;
        link_symlink_into(&symlink, &root, "existing", true).await;
        let (sym_mutable, sym_immutable) = query_attributes!(symlink, time_query);
        let (dir_mutable, dir_immutable) = query_attributes!(root, time_query);
        assert!(sym_immutable.change_time.unwrap() > 1);
        assert_eq!(sym_mutable.modification_time.unwrap(), sym_mtime0);
        assert_eq!(sym_immutable.change_time, dir_immutable.change_time);
        assert_eq!(sym_immutable.change_time, dir_mutable.modification_time);

        fsck(fixture.fs().clone()).await.expect("fsck failed");
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_in_encrypted_directory() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        // Set up encrypted directory
        let crypt = fixture.crypt().unwrap();
        let encrypted_directory = open_dir_checked(
            &root,
            "encrypted_directory",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_DIRECTORY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            fio::Options::default(),
        )
        .await;
        crypt.add_wrapping_key(WRAPPING_KEY_ID, [1; 32].into()).unwrap();
        encrypted_directory
            .update_attributes(&fio::MutableNodeAttributes {
                encryption_policy: Some(fio::FscryptPolicy {
                    key_identifier: WRAPPING_KEY_ID,
                    flags: fxfs::object_store::LEGACY_FSCRYPT_FLAGS,
                }),
                ..Default::default()
            })
            .await
            .expect("update_attributes wire call failed")
            .map_err(zx::ok)
            .expect("update_attributes failed");

        // Create a temporary unnamed file in that directory, it should have the same wrapping key.
        let encryped_tmpfile = open_file_checked(
            &encrypted_directory,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        let (mutable_attributes, _immutable_attributes) = encryped_tmpfile
            .get_attributes(fio::NodeAttributesQuery::ENCRYPTION_POLICY)
            .await
            .expect("get_attributes wire call failed")
            .map_err(zx::Status::err_from_raw)
            .expect("get_attributes failed");
        assert_eq!(
            mutable_attributes.encryption_policy.map(|p| p.key_identifier),
            Some(WRAPPING_KEY_ID)
        );

        // Similar to a regular file, linking a temporary unnamed file into the directory will only
        // work if they have the same wrapping key ID.
        let (status, dst_token) =
            encrypted_directory.deprecated_get_token().await.expect("FIDL call failed");
        zx::Status::ok(status).expect("get_token failed");
        encryped_tmpfile
            .link_into(zx::Event::from(dst_token.unwrap()), "foo", &fio::LinkIntoOptions::default())
            .await
            .expect("link_into wire message failed")
            .expect("link_into failed");

        let unencryped_tmpfile = open_file_checked(
            &root,
            ".",
            fio::Flags::PROTOCOL_FILE
                | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;
        let (mutable_attributes, _immutable_attributes) = unencryped_tmpfile
            .get_attributes(fio::NodeAttributesQuery::ENCRYPTION_POLICY)
            .await
            .expect("get_attributes wire call failed")
            .map_err(zx::Status::err_from_raw)
            .expect("get_attributes failed");
        assert_eq!(mutable_attributes.encryption_policy, None);
        let (status, dst_token) =
            encrypted_directory.deprecated_get_token().await.expect("FIDL call failed");
        zx::Status::ok(status).expect("get_token failed");
        assert_eq!(
            unencryped_tmpfile
                .link_into(
                    zx::Event::from(dst_token.unwrap()),
                    "bar",
                    &fio::LinkIntoOptions::default(),
                )
                .await
                .expect("link_into wire message failed")
                .map_err(zx::Status::err_from_raw)
                .expect_err("link_into passed unexpectedly"),
            zx::Status::BAD_STATE,
        );

        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_in_locked_directory() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        // Set up encrypted directory
        let crypt = fixture.crypt().unwrap();
        let encrypted_directory = open_dir_checked(
            &root,
            "encrypted_directory",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_DIRECTORY
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            fio::Options::default(),
        )
        .await;
        crypt.add_wrapping_key(WRAPPING_KEY_ID, [1; 32].into()).unwrap();
        encrypted_directory
            .update_attributes(&fio::MutableNodeAttributes {
                encryption_policy: Some(fio::FscryptPolicy {
                    key_identifier: WRAPPING_KEY_ID,
                    flags: fxfs::object_store::LEGACY_FSCRYPT_FLAGS,
                }),
                ..Default::default()
            })
            .await
            .expect("update_attributes wire call failed")
            .map_err(zx::ok)
            .expect("update_attributes failed");

        // This locks the directory
        crypt.forget_wrapping_key(&WRAPPING_KEY_ID).unwrap();

        // Open unnamed temporary file in a locked directory and should return (key) UNAVAILABLE.
        assert_eq!(
            open_file(
                &encrypted_directory,
                ".",
                fio::Flags::PROTOCOL_FILE
                    | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE,
                &fio::Options::default()
            )
            .await
            .expect_err("Open succeeded unexpectedly")
            .root_cause()
            .downcast_ref::<zx::Status>()
            .expect("No status"),
            &zx::Status::UNAVAILABLE,
        );
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_unnamed_temporary_file_link_into_with_race() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        for i in 1..100 {
            let tmpfile = open_file_checked(
                &root,
                ".",
                fio::Flags::PROTOCOL_FILE
                    | fio::Flags::FLAG_CREATE_AS_UNNAMED_TEMPORARY
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE,
                &fio::Options::default(),
            )
            .await;

            // Clone tmpfile proxy to use in the separate threads.
            let (tmpfile_clone1, tmpfile_server1) =
                fidl::endpoints::create_proxy::<fio::FileMarker>();
            tmpfile.clone(tmpfile_server1.into_channel().into()).expect("clone failed");
            let (tmpfile_clone2, tmpfile_server2) =
                fidl::endpoints::create_proxy::<fio::FileMarker>();
            tmpfile.clone(tmpfile_server2.into_channel().into()).expect("clone failed");

            // Get the open connection to the sub directory which we would attempt to link the
            // unnamed temporary file into.
            let sub_dir = open_dir_checked(
                &root,
                "A",
                fio::Flags::PROTOCOL_DIRECTORY
                    | fio::PERM_READABLE
                    | fio::PERM_WRITABLE
                    | fio::Flags::FLAG_MAYBE_CREATE,
                fio::Options::default(),
            )
            .await;

            // Get tokens to the sub directory to use for `link_into`.
            let (status, dst_token1) =
                sub_dir.deprecated_get_token().await.expect("FIDL call failed");
            zx::Status::ok(status).expect("get_token failed");
            let (status, dst_token2) =
                sub_dir.deprecated_get_token().await.expect("FIDL call failed");
            zx::Status::ok(status).expect("get_token failed");

            join!(
                fasync::Task::spawn(async move {
                    tmpfile_clone1
                        .link_into(
                            zx::Event::from(dst_token1.unwrap()),
                            &(2 * i).to_string(),
                            &fio::LinkIntoOptions::default(),
                        )
                        .await
                        .expect("link_into wire message failed")
                        .expect("link_into failed");
                }),
                fasync::Task::spawn(async move {
                    tmpfile_clone2
                        .link_into(
                            zx::Event::from(dst_token2.unwrap()),
                            &(2 * i + 1).to_string(),
                            &fio::LinkIntoOptions::default(),
                        )
                        .await
                        .expect("link_into wire message failed")
                        .expect("link_into failed");
                })
            );
            let (_, immutable_attributes) = tmpfile
                .get_attributes(fio::NodeAttributesQuery::LINK_COUNT)
                .await
                .expect("Failed get_attributes wire call")
                .expect("get_attributes failed");
            assert_eq!(immutable_attributes.link_count.unwrap(), 2);
            close_file_checked(tmpfile).await;
        }
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_update_attributes_persists() {
        const FILE: &str = "foo";
        let mtime = Some(Timestamp::now().as_nanos());
        let atime = Some(Timestamp::now().as_nanos());
        let mode = Some(111);

        let device = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                FILE,
                fio::Flags::FLAG_MAYBE_CREATE | fio::PERM_WRITABLE | fio::Flags::PROTOCOL_FILE,
                &fio::Options::default(),
            )
            .await;

            file.update_attributes(&fio::MutableNodeAttributes {
                modification_time: mtime,
                access_time: atime,
                mode: Some(111),
                ..Default::default()
            })
            .await
            .expect("update_attributes FIDL call failed")
            .map_err(zx::ok)
            .expect("update_attributes failed");

            // Calling close should flush the node attributes to the device.
            fixture.close().await
        };

        let fixture =
            TestFixture::open(device, TestFixtureOptions { format: false, ..Default::default() })
                .await;
        let root = fixture.root();
        let file = open_file_checked(
            &root,
            FILE,
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &fio::Options::default(),
        )
        .await;

        let (mutable_attributes, _immutable_attributes) = file
            .get_attributes(
                fio::NodeAttributesQuery::MODIFICATION_TIME
                    | fio::NodeAttributesQuery::ACCESS_TIME
                    | fio::NodeAttributesQuery::MODE,
            )
            .await
            .expect("update_attributesFIDL call failed")
            .map_err(zx::ok)
            .expect("get_attributes failed");
        assert_eq!(mutable_attributes.modification_time, mtime);
        assert_eq!(mutable_attributes.access_time, atime);
        assert_eq!(mutable_attributes.mode, mode);
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_atime_from_pending_access_time_update_request() {
        const FILE: &str = "foo";

        let (device, expected_atime, expected_ctime) = {
            let fixture = TestFixture::new().await;
            let root = fixture.root();

            let file = open_file_checked(
                &root,
                FILE,
                fio::Flags::FLAG_MAYBE_CREATE
                    | fio::PERM_WRITABLE
                    | fio::Flags::PROTOCOL_FILE
                    | fio::Flags::PERM_GET_ATTRIBUTES,
                &fio::Options::default(),
            )
            .await;

            let (mutable_attributes, immutable_attributes) = file
                .get_attributes(
                    fio::NodeAttributesQuery::CHANGE_TIME
                        | fio::NodeAttributesQuery::ACCESS_TIME
                        | fio::NodeAttributesQuery::MODIFICATION_TIME,
                )
                .await
                .expect("update_attributes FIDL call failed")
                .map_err(zx::ok)
                .expect("get_attributes failed");
            let initial_ctime = immutable_attributes.change_time;
            let initial_atime = mutable_attributes.access_time;
            // When creating a file, ctime, mtime, and atime are all updated to the current time.
            assert_eq!(initial_atime, initial_ctime);
            assert_eq!(initial_atime, mutable_attributes.modification_time);

            // Client manages atime and they signal to Fxfs that a file access has occurred and it
            // may require an access time update. They do so by querying with
            // `fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE`.
            let (mutable_attributes, immutable_attributes) = file
                .get_attributes(
                    fio::NodeAttributesQuery::CHANGE_TIME
                        | fio::NodeAttributesQuery::ACCESS_TIME
                        | fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE,
                )
                .await
                .expect("update_attributes FIDL call failed")
                .map_err(zx::ok)
                .expect("get_attributes failed");
            // atime will be updated as atime <= ctime (or mtime)
            assert!(initial_atime < mutable_attributes.access_time);
            let updated_atime = mutable_attributes.access_time;
            // Calling get_attributes with PENDING_ACCESS_TIME_UPDATE will trigger an update of
            // object attributes if access_time needs to be updated. Check that ctime isn't updated.
            assert_eq!(initial_ctime, immutable_attributes.change_time);

            let (mutable_attributes, _) = file
                .get_attributes(
                    fio::NodeAttributesQuery::ACCESS_TIME
                        | fio::NodeAttributesQuery::PENDING_ACCESS_TIME_UPDATE,
                )
                .await
                .expect("update_attributes FIDL call failed")
                .map_err(zx::ok)
                .expect("get_attributes failed");
            // atime will be not be updated as atime > ctime (or mtime)
            assert_eq!(updated_atime, mutable_attributes.access_time);

            (fixture.close().await, mutable_attributes.access_time, initial_ctime)
        };

        let fixture =
            TestFixture::open(device, TestFixtureOptions { format: false, ..Default::default() })
                .await;
        let root = fixture.root();
        let file = open_file_checked(
            &root,
            FILE,
            fio::PERM_READABLE | fio::Flags::PROTOCOL_FILE,
            &fio::Options::default(),
        )
        .await;

        // Make sure that the pending atime update persisted.
        let (mutable_attributes, immutable_attributes) = file
            .get_attributes(
                fio::NodeAttributesQuery::CHANGE_TIME | fio::NodeAttributesQuery::ACCESS_TIME,
            )
            .await
            .expect("update_attributesFIDL call failed")
            .map_err(zx::ok)
            .expect("get_attributes failed");

        assert_eq!(immutable_attributes.change_time, expected_ctime);
        assert_eq!(mutable_attributes.access_time, expected_atime);
        fixture.close().await;
    }

    #[fuchsia::test(threads = 10)]
    async fn test_delete_with_dirty_bytes_and_no_open_handles() {
        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "foo",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        file.resize(4096)
            .await
            .expect("resize failed")
            .map_err(Status::err_from_raw)
            .expect("resize error");

        let vmo = file
            .get_backing_memory(
                fio::VmoFlags::SHARED_BUFFER | fio::VmoFlags::READ | fio::VmoFlags::WRITE,
            )
            .await
            .expect("Failed to make FIDL call")
            .map_err(Status::err_from_raw)
            .expect("Failed to get VMO");

        // Flush the file so that the file isn't dirty.
        file.sync().await.unwrap().unwrap();

        // Close the file handle.
        drop(file);

        // Allow time for the close to be noticed.
        fasync::Timer::new(std::time::Duration::from_millis(10)).await;

        // Modify the file through the VMO (this should mark it dirty).
        vmo.write(&[1, 2, 3, 4], 0).expect("vmo write failed");

        // Drop the VMO so that the open count reaches zero, but unlike in the close case, this
        // will not flush the file immediately.
        drop(vmo);

        // Allow time for the VMO being dropped to be noticed.
        fasync::Timer::new(std::time::Duration::from_millis(10)).await;

        // Delete the file.  The file should be dirty at this point.
        root.unlink("foo", &fio::UnlinkOptions::default())
            .await
            .expect("unlink failed")
            .map_err(Status::err_from_raw)
            .expect("unlink error");

        fixture.close().await;
    }

    // Ensures that closing a file connection immediately blocks any future stream writes to the
    // underlying VMO with a BAD_STATE error, and that the file can still be safely unlinked
    // once the stream write has been rejected and cleanup has occurred.
    #[fuchsia::test]
    async fn test_close_file_before_writing_to_stream() {
        const FILE_NAME: &str = "foo";

        let fixture = TestFixture::new().await;
        let root = fixture.root();
        let file = open_file_checked(
            &root,
            FILE_NAME,
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let stream = file.describe().await.unwrap().stream.unwrap();

        close_file_checked(file).await;

        unblock(move || {
            stream
                .write_at(zx::StreamWriteOptions::empty(), 0, &[1, 2, 3, 4])
                .expect_err("Write should get BAD_STATE");
        })
        .await;

        // Wait a bit to ensure that the stream has been closed and the zero children signal has
        // been processed.
        fasync::Timer::new(Duration::from_millis(100)).await;

        // Now unlink the file.
        root.unlink(FILE_NAME, &fio::UnlinkOptions::default())
            .await
            .expect("unlink wire call failed")
            .expect("unlink failed");

        fixture.close().await;
    }

    // Ensures that closing a file connection blocks stream writes, but reopening the file
    // restores write capabilities. By using a duplicated stream handle from the first open,
    // we prove that when the file is reopened, the pager is marked open again, and writes
    // on the original stream handle succeed once more. This also confirms that the file remains
    // in the Fxfs node caches and is correctly reused when reopened.
    #[fuchsia::test]
    async fn test_close_and_reopen_file_stream() {
        const FILE_NAME: &str = "foo";

        let fixture = TestFixture::new().await;
        let root = fixture.root();
        let file = open_file_checked(
            &root,
            FILE_NAME,
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE
                | fio::Flags::PROTOCOL_FILE,
            &Default::default(),
        )
        .await;

        let page_size = page_size();
        file.resize(8 * page_size)
            .await
            .expect("resize failed")
            .map_err(Status::err_from_raw)
            .expect("resize error");

        let stream1 = file.describe().await.unwrap().stream.unwrap();
        let stream1_dup = stream1.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();

        close_file_checked(file).await;

        // Stream writes on the duplicated stream handle should now fail because all active
        // connections are closed and the handle open status is false.
        // Write at offset 0 (page 0).
        let stream1_dup_clone = stream1_dup.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        unblock(move || {
            stream1_dup_clone
                .write_at(zx::StreamWriteOptions::empty(), 0, &[1, 2, 3, 4])
                .expect_err("Write should get BAD_STATE");
        })
        .await;

        // Open the file again. This retrieves the node from the dirent cache and sets its pager
        // status to open, enabling stream writes on all stream handles pointing to its VMO.
        let file2 = open_file_checked(
            &root,
            FILE_NAME,
            fio::Flags::PROTOCOL_FILE | fio::PERM_READABLE | fio::PERM_WRITABLE,
            &Default::default(),
        )
        .await;

        let stream2 = file2.describe().await.unwrap().stream.unwrap();

        // Now both the old duplicated stream and the new stream should succeed.
        // Write at page-separated offsets to prevent kernel dirty page caching from hiding races:
        // Page 1 and Page 2.
        let stream1_dup_clone2 = stream1_dup.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let stream2_clone = stream2.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        unblock(move || {
            stream1_dup_clone2
                .write_at(zx::StreamWriteOptions::empty(), 1 * page_size, &[5, 6, 7, 8])
                .expect("Write on re-opened stream 1 dup should succeed");
            stream2_clone
                .write_at(zx::StreamWriteOptions::empty(), 2 * page_size, &[9, 10, 11, 12])
                .expect("Write on new stream 2 should succeed");
        })
        .await;

        // Close the reopened connection.
        close_file_checked(file2).await;

        // Writes on both streams should fail again.
        // Write at new page-separated offsets:
        // Page 3 and Page 4.
        unblock(move || {
            stream1_dup
                .write_at(zx::StreamWriteOptions::empty(), 3 * page_size, &[13, 14, 15, 16])
                .expect_err("Write on stream 1 dup should fail after final close");
            stream2
                .write_at(zx::StreamWriteOptions::empty(), 4 * page_size, &[17, 18, 19, 20])
                .expect_err("Write on stream 2 should fail after final close");
        })
        .await;

        fixture.close().await;
    }

    use test_case::test_case;

    #[test_case(
        fxfs_crypto::EncryptionKey::Fxfs(fxfs_crypto::FxfsKey {
            wrapping_key_id: WRAPPING_KEY_ID,
            key: fxfs_crypto::WrappedKeyBytes::from([0xff; fxfs_crypto::FXFS_WRAPPED_KEY_SIZE]),
        });
        "fxfs"
    )]
    #[test_case(
        fxfs_crypto::EncryptionKey::FscryptInoLblk32File {
            key_identifier: WRAPPING_KEY_ID,
        };
        "fscrypt_file"
    )]
    #[fuchsia::test]
    async fn test_supported_wrapping_key_ids(key: fxfs_crypto::EncryptionKey) {
        use fxfs::object_store::transaction::Mutation;
        use fxfs::object_store::{FSCRYPT_KEY_ID, ObjectKey, ObjectValue};

        let fixture = TestFixture::new().await;
        let root = fixture.root();

        let file = open_file_checked(
            &root,
            "key_test_file",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_FILE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;

        let (_, immutable_attributes) = file
            .get_attributes(fio::NodeAttributesQuery::ID)
            .await
            .expect("get_attributes wire call failed")
            .map_err(zx::Status::err_from_raw)
            .expect("get_attributes failed");
        let file_id = immutable_attributes.id.unwrap();

        let store = fixture.volume().volume().store();
        let mut transaction = store
            .new_transaction(
                lock_keys![LockKey::object(store.store_object_id(), file_id)],
                Options::default(),
            )
            .await
            .expect("new_transaction failed");

        transaction.add(
            store.store_object_id(),
            Mutation::replace_or_insert_object(
                ObjectKey::keys(file_id),
                ObjectValue::Keys(vec![(FSCRYPT_KEY_ID, key)].into()),
            ),
        );
        transaction.commit().await.expect("commit failed");

        let (mutable_attributes, _) = file
            .get_attributes(fio::NodeAttributesQuery::ENCRYPTION_POLICY)
            .await
            .expect("get_attributes wire call failed")
            .map_err(zx::Status::err_from_raw)
            .expect("get_attributes failed");

        assert_eq!(
            mutable_attributes.encryption_policy.map(|p| p.key_identifier),
            Some(WRAPPING_KEY_ID)
        );

        fixture.close().await;
    }

    #[fuchsia::test(threads = 3)]
    async fn test_teardown_with_pending_shrink_and_commit_failure() {
        use fxfs::errors::FxfsError;

        let fail = Arc::new(AtomicBool::new(false));
        let fail_clone = fail.clone();
        let (mut hooks, fs_hooks) = fxfs::hooks::Hooks::new();
        hooks.set_pre_commit(move |_| {
            if fail_clone.load(std::sync::atomic::Ordering::Relaxed) {
                Err(FxfsError::Unavailable.into())
            } else {
                Ok(())
            }
        });
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(16384, 512)),
            TestFixtureOptions { encrypted: false, hooks: Some(fs_hooks), ..Default::default() },
        )
        .await;

        let root = fixture.root();
        let file = open_file_checked(
            &root,
            "test_file",
            fio::Flags::FLAG_MAYBE_CREATE
                | fio::Flags::PROTOCOL_FILE
                | fio::PERM_READABLE
                | fio::PERM_WRITABLE,
            &fio::Options::default(),
        )
        .await;

        // Grow the file to 4 pages and sync so the on-disk size is 4 pages.
        let page_size = page_size();
        file.resize(page_size * 4).await.unwrap().expect("resize failed");
        file.sync().await.unwrap().expect("sync failed");

        // Now cause commits to fail. All changes will be pending on the handle.
        fail.store(true, std::sync::atomic::Ordering::Relaxed);

        // Dirty the first 2 pages.
        let stream = file.describe().await.unwrap().stream.unwrap();
        unblock(move || {
            for i in 0..2 {
                stream
                    .write_at(zx::StreamWriteOptions::empty(), i * page_size, &[1u8])
                    .expect("write_at failed");
            }
        })
        .await;

        // Shrink the file to 2 pages so that a pending shrink is recorded on the handle.
        file.resize(page_size * 2).await.unwrap().expect("resize failed");

        // Close the client-side file connection and drop object reference so only the
        // volume's IS_DIRTY raw Arc keeps the file alive.
        drop(file);

        // Closing the fixture terminates the volume, which attempts a LastChance flush on all files.
        // When that fails, we want to make sure that all the dirty pages still get returned to the
        // dirty page tracking and allocator. There is an assert in the `PagedObjectHandle::drop`
        // that will otherwise trigger.
        fixture.close().await;
    }

    /// Logical offsets at or above 4 GiB take the full 9 bytes when serialized, which is the
    /// largest encoding. Writing at this offset makes every logical extent record as big as it
    /// can be.
    const FULL_WIDTH_VARINT_OFFSET: u64 = 1 << 32;

    /// The test devices are smaller than 4 GiB, so each device offset is serialized in 5 bytes.
    /// On a larger device the same offset can take 9 bytes. This is the extra cost per offset.
    const U64_WIDENING_FROM_U32: u64 = 4;

    async fn write_stream_at_offsets(
        stream: &zx::Stream,
        offsets: impl IntoIterator<Item = u64>,
        data: &[u8],
    ) {
        let stream = stream.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();
        let offsets: Vec<u64> = offsets.into_iter().collect();
        let data = data.to_vec();
        unblock(move || {
            for offset in offsets {
                stream
                    .write_at(zx::StreamWriteOptions::empty(), offset, &data)
                    .expect("write_at failed");
            }
        })
        .await;
    }

    /// Syncs `file` and returns the number of journal bytes written by the flush.
    ///
    /// Compactions are paused so nothing else is written to the journal during the measurement,
    /// and the filesystem is synced first so the flush starts on a journal block boundary. While
    /// the flush is paused in `Journal::pre_commit`, a concurrent `fs.sync()` runs; if
    /// `pre_commit` wrote records before taking `writer_mutex`, that sync would pad a second
    /// block. Usage is counted from the initial block boundary to the end of the flush.
    async fn measure_sync_journal_usage(
        fixture: &TestFixture,
        file: &fio::FileProxy,
        file_id: u64,
    ) -> u64 {
        let fs = fixture.fs();
        let journal = fs.journal();
        // The first compaction moves `super_block_header.journal_checkpoint` past zero; the second
        // sets `zero_offset`, so the next `pre_commit` runs its async zeroing pass.
        journal.force_compact().await.expect("force_compact failed");
        journal.force_compact().await.expect("force_compact failed");
        journal.pause_compactions().await;
        fs.sync(SyncOptions { flush_device: true, ..Default::default() })
            .await
            .expect("fs sync failed");
        let offset_before = fs.object_manager().last_end_offset();
        let writer_offset_before = journal::BLOCK_SIZE.align_up(offset_before).unwrap();

        // Hold the journal object lock so `file.sync()` pauses in `Journal::pre_commit` after its
        // initial check, while zeroing the old journal tail left by `force_compact`.
        let journal_lock = fs
            .lock_manager()
            .txn_lock(lock_keys![LockKey::object(
                fs.root_parent_store().store_object_id(),
                journal.super_block_header().journal_object_id,
            )])
            .await;
        let file_key = lock_keys![LockKey::object(
            fixture.volume().volume().store().store_object_id(),
            file_id,
        )];

        let mut sync_fut = std::pin::pin!(file.sync());
        while futures::poll!(std::pin::pin!(fs.lock_manager().read_lock(file_key.clone())))
            .is_ready()
        {
            assert!(futures::poll!(&mut sync_fut).is_pending());
            fuchsia_async::yield_now().await;
        }

        fs.sync(SyncOptions::default()).await.expect("concurrent fs sync failed");
        drop(journal_lock);

        sync_fut.await.unwrap().expect("sync failed");
        let offset_after = fs.object_manager().last_end_offset();
        journal.resume_compactions();
        offset_after - writer_offset_before
    }

    #[fuchsia::test]
    async fn test_max_transaction_size_fragmented_cow_overwrite() {
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(49152, 512)),
            TestFixtureOptions::default(),
        )
        .await;
        // Pause background compactions during setup so neither premature device flushes nor
        // `force_compact` zeroing the contiguous initial journal chunk release multi-block
        // extents back into the allocator before step 4.
        fixture.fs().journal().pause_compactions().await;
        let root = fixture.root();
        let page_size = page_size().get();
        let pages_per_batch = FLUSH_BATCH_SIZE / page_size;
        assert_eq!(pages_per_batch, 128);

        // 1. Write 1 MiB (256 pages) to `target` at a 4 GiB offset and sync it. Step 4 overwrites
        //    every other page, so the old blocks it frees are not adjacent.
        let (target, target_id) = open_rw_file_and_id(&root, "target").await;
        let target_stream = target.describe().await.unwrap().stream.unwrap();
        let batch_buf = vec![0xaa; FLUSH_BATCH_SIZE as usize];
        for i in 0..2 {
            write_stream_at_offsets(
                &target_stream,
                [FULL_WIDTH_VARINT_OFFSET + i * FLUSH_BATCH_SIZE],
                &batch_buf,
            )
            .await;
            target.sync().await.unwrap().expect("sync failed");
        }

        // 2. Write 8 MiB to `donor`, then overwrite every other page. This frees 1024 scattered
        //    4 KiB blocks (more than `metadata_reservation` plus the 1 MiB left for step 4, so
        //    step 3 must exhaust all contiguous tail space). They cannot be reused until the next
        //    device flush, so for now the allocator only has the contiguous free space at the end
        //    of the disk.
        let (donor, _) = open_rw_file_and_id(&root, "donor").await;
        let donor_stream = donor.describe().await.unwrap().stream.unwrap();
        for i in 0..16 {
            write_stream_at_offsets(&donor_stream, [i * FLUSH_BATCH_SIZE], &batch_buf).await;
            donor.sync().await.unwrap().expect("sync failed");
        }
        for batch in 0..8 {
            let offsets =
                (0..pages_per_batch).map(|i| (batch * pages_per_batch + i) * 2 * page_size);
            write_stream_at_offsets(&donor_stream, offsets, &[0xbb]).await;
            donor.sync().await.unwrap().expect("sync failed");
        }

        // 3. Fill the disk with `sponge`, leaving 1 MiB free. This uses up the contiguous space at
        //    the end of the disk and forces a device flush, which releases the scattered blocks
        //    from step 2. After this, the only free space left is isolated 4 KiB holes.
        let (sponge, _) = open_rw_file_and_id(&root, "sponge").await;
        let (status, info) = root.query_filesystem().await.expect("query_filesystem failed");
        Status::ok(status).expect("query_filesystem status failed");
        let info = info.unwrap();
        let free_bytes = info.total_bytes - info.used_bytes;
        let target_needed = 2 * FLUSH_BATCH_SIZE;
        sponge
            .allocate(0, free_bytes - target_needed, fio::AllocateMode::empty())
            .await
            .unwrap()
            .expect("allocate failed");

        // 4. Overwrite every other page of `target` (128 pages, one full flush batch) and sync.
        //    Each page needs its own new block and frees its own old block, so one transaction
        //    records 128 separate allocations, frees, extents and checksums.
        let even_offsets =
            (0..pages_per_batch).map(|i| FULL_WIDTH_VARINT_OFFSET + i * 2 * page_size);
        write_stream_at_offsets(&target_stream, even_offsets, &[0xcc]).await;

        let txn_journal_usage = measure_sync_journal_usage(&fixture, &target, target_id).await;

        // Check that the file really is 256 separate one-page extents, and that none of the 128
        // new blocks are next to each other on disk.
        let extents = cached_node::<FxFile>(&fixture, target_id)
            .handle()
            .uncached_handle()
            .device_extents()
            .await
            .expect("device_extents failed");
        assert_eq!(extents.len(), (pages_per_batch * 2) as usize);
        assert!(extents.iter().all(|e| e.length() == page_size));
        let mut even_device_ranges: Vec<_> =
            extents.iter().step_by(2).map(|e| e.device_range().clone()).collect();
        even_device_ranges.sort_by_key(|r| r.start);
        assert!(
            even_device_ranges.windows(2).all(|w| w[0].end < w[1].start),
            "Expected the 128 new blocks to be non-adjacent on disk",
        );

        // Each page writes 7 device offsets, and each would grow on a disk larger than 4 GiB. The
        // limit must still hold with that growth added, and with a full block of padding on top.
        let device_offset_widening = pages_per_batch * 7 * U64_WIDENING_FROM_U32;
        assert!(
            txn_journal_usage > 12_000,
            "Expected the CoW flush to use over 12 KB of journal, got {txn_journal_usage}",
        );
        assert!(
            txn_journal_usage + device_offset_widening + journal::BLOCK_SIZE.get()
                <= TRANSACTION_MAX_JOURNAL_USAGE,
            "journal usage {txn_journal_usage} plus offset growth {device_offset_widening} \
             plus padding is over the limit of {TRANSACTION_MAX_JOURNAL_USAGE}",
        );

        close_file_checked(target).await;
        close_file_checked(donor).await;
        close_file_checked(sponge).await;
        fixture.close().await;
    }

    #[fuchsia::test]
    async fn test_max_transaction_size_overwrite_partial_flush() {
        // The allocator hands out free space in 2 MiB chunks. Each 448-block extent takes one
        // chunk and leaves a small hole behind. The device is 264 MiB so that, after filesystem
        // metadata, there are still enough whole chunks for all 128 extents.
        let fixture = TestFixture::open(
            DeviceHolder::new(FakeDevice::new(67584, 4096)),
            TestFixtureOptions::default(),
        )
        .await;
        let root = fixture.root();
        let page_size = page_size().get();
        let pages_per_batch = FLUSH_BATCH_SIZE / page_size;
        assert_eq!(pages_per_batch, 128);

        // With 4 KiB blocks, the largest extent the allocator creates is 448 blocks. These extents
        // are never merged, so allocating 128 of them at a 4 GiB offset gives 128 separate
        // OverwritePartial extents, each as large as possible.
        let extent_bytes = max_extent_size_for_block_size(fixture.fs().block_size());
        let max_extent_blocks = extent_bytes / page_size;
        assert_eq!(max_extent_blocks, 448);

        let (file, file_id) = open_rw_file_and_id(&root, "overwrite_file").await;
        file.allocate(
            FULL_WIDTH_VARINT_OFFSET,
            pages_per_batch * extent_bytes,
            fio::AllocateMode::empty(),
        )
        .await
        .unwrap()
        .expect("allocate failed");

        // Each extent tracks which blocks have been written using a bitmap of 14 32-bit words.
        // A word with its top bit set takes the most space to serialize. Write the last block
        // covered by each word so every bitmap is as large as possible. Most blocks stay
        // unwritten, so the extents remain OverwritePartial.
        let stream = file.describe().await.unwrap().stream.unwrap();
        for w in 0..(max_extent_blocks / 32) {
            let offsets = (0..pages_per_batch)
                .map(|i| FULL_WIDTH_VARINT_OFFSET + i * extent_bytes + (w * 32 + 31) * page_size);
            write_stream_at_offsets(&stream, offsets, &[0xee]).await;
            file.sync().await.unwrap().expect("sync failed");
        }

        // Write the first block of each extent (128 pages, one full flush batch) and sync. One
        // transaction then rewrites all 128 large bitmaps and adds 128 checksum records.
        let first_block_offsets =
            (0..pages_per_batch).map(|i| FULL_WIDTH_VARINT_OFFSET + i * extent_bytes);
        write_stream_at_offsets(&stream, first_block_offsets, &[0xdd]).await;

        let txn_journal_usage = measure_sync_journal_usage(&fixture, &file, file_id).await;

        // Check that the file is still 128 separate 448-block extents.
        let extents = cached_node::<FxFile>(&fixture, file_id)
            .handle()
            .uncached_handle()
            .device_extents()
            .await
            .expect("device_extents failed");
        assert_eq!(extents.len(), pages_per_batch as usize);
        assert!(extents.iter().all(|e| e.length() == extent_bytes));

        // Each extent writes 3 device offsets, and each would grow on a disk larger than 4 GiB.
        // The limit must still hold with that growth added, and with a full block of padding on
        // top.
        let device_offset_widening = pages_per_batch * 3 * U64_WIDENING_FROM_U32;
        assert!(
            txn_journal_usage > 16_000,
            "Expected the OverwritePartial flush to use over 16 KB of journal, got \
             {txn_journal_usage}",
        );
        assert!(
            txn_journal_usage + device_offset_widening + journal::BLOCK_SIZE.get()
                <= TRANSACTION_MAX_JOURNAL_USAGE,
            "journal usage {txn_journal_usage} plus offset growth {device_offset_widening} \
             plus padding is over the limit of {TRANSACTION_MAX_JOURNAL_USAGE}",
        );

        close_file_checked(file).await;
        fixture.close().await;
    }
}
