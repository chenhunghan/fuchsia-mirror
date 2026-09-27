// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use crate::verifier::Verifier;
use anyhow::Error;
use delivery_blob::compression::{ChunkedArchiveError, DataBuffer};
use fidl::endpoints::ServerEnd;
use fidl_fuchsia_storage_block as fblock;
use fuchsia_async as fasync;
use fuchsia_sync::Mutex;
use futures::future::{BoxFuture, FutureExt as _};
use futures::stream::TryStreamExt as _;
use mapping::reader::{BlockService, ChildBlockService};
use mapping::{
    DeliveryHandler, Files, PENDING_COMMANDS_CAPACITY, PageRequest, RawMappingCommand,
    process_mapping_command,
};
use std::collections::HashMap;
use std::ops::Range;
use std::sync::Arc;
use std::thread::JoinHandle;
use storage_ptr_slice::MutPtrByteSlice;
use vmo_fifo::{Receiver, SIG_SHUTDOWN};

pub trait MapperHandler: Send + Sync + 'static {
    fn on_open_mapper_session(
        &self,
        mapping_vmo: &zx::Vmo,
        delivery_queue: zx::Vmo,
    ) -> Result<Verifier, zx::Status>;
}

impl<F> MapperHandler for F
where
    F: Fn(&zx::Vmo, zx::Vmo) -> Result<Verifier, zx::Status> + Send + Sync + 'static,
{
    fn on_open_mapper_session(
        &self,
        mapping_vmo: &zx::Vmo,
        delivery_queue: zx::Vmo,
    ) -> Result<Verifier, zx::Status> {
        self(mapping_vmo, delivery_queue)
    }
}

struct PagedVmo {
    vmo: zx::Vmo,
    options: fblock::CreateVmoOptions,
}

/// Services page faults directly using a [`zx::Pager`] owned by the driver mapper session.
#[derive(Clone)]
pub struct DirectPager {
    pager: Arc<zx::Pager>,
    port: Arc<zx::Port>,
    vmos: Arc<Mutex<HashMap<u64, PagedVmo>>>,
}

impl DirectPager {
    pub fn new(pager: Arc<zx::Pager>, port: zx::Port) -> Self {
        Self { pager, port: Arc::new(port), vmos: Arc::new(Mutex::new(HashMap::new())) }
    }

    pub fn create_vmo(
        &self,
        key: u64,
        size: u64,
        options: fblock::CreateVmoOptions,
    ) -> Result<zx::Vmo, zx::Status> {
        let vmo = self.pager.create_vmo(zx::VmoOptions::empty(), &self.port, key, size)?;
        self.vmos
            .lock()
            .insert(key, PagedVmo { vmo: vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?, options });
        Ok(vmo)
    }
}

impl DeliveryHandler for DirectPager {
    type Request = DirectPageRequest;

    fn get_page_request(
        self: &Arc<Self>,
        key: u64,
        original_range: Range<u64>,
    ) -> DirectPageRequest {
        DirectPageRequest {
            pager: Arc::clone(self),
            key,
            original_range: original_range.clone(),
            read_range: original_range,
            committed_len: 0,
            transfer_vmo: None,
            vaddr: 0,
        }
    }

    fn unregister_file(&self, key: u64) {
        self.vmos.lock().remove(&key);
    }
}

/// Implementation of [`PageRequest`] for direct driver page supplies to the kernel pager.
pub struct DirectPageRequest {
    pager: Arc<DirectPager>,
    key: u64,
    original_range: Range<u64>,
    read_range: Range<u64>,
    committed_len: usize,
    transfer_vmo: Option<zx::Vmo>,
    vaddr: usize,
}

impl DataBuffer for DirectPageRequest {
    fn range(&self) -> Range<u64> {
        self.read_range.clone()
    }

    fn mut_ptr_slice(&mut self) -> MutPtrByteSlice<'_> {
        let total_len = (self.read_range.end - self.read_range.start) as usize;
        let remaining = total_len.saturating_sub(self.committed_len);
        if remaining > 0 {
            assert!(self.vaddr != 0, "prepare must be called before accessing mut_ptr_slice");
        }
        // SAFETY: `self.vaddr` is mapped for `total_len` bytes by `prepare()`,
        // and `self.committed_len + remaining` is bounded by `total_len`.
        unsafe {
            MutPtrByteSlice::from(std::slice::from_raw_parts_mut(
                (self.vaddr + self.committed_len) as *mut u8,
                remaining,
            ))
        }
    }

    fn commit(&mut self, size: usize) -> Result<(), ChunkedArchiveError> {
        let vmos_guard = self.pager.vmos.lock();
        if let Some(paged_vmo) = vmos_guard.get(&self.key) {
            let offset = self.read_range.start + self.committed_len as u64;
            self.pager
                .pager
                .supply_pages(
                    &paged_vmo.vmo,
                    offset..offset + size as u64,
                    self.transfer_vmo.as_ref().unwrap(),
                    self.committed_len as u64,
                )
                .map_err(|error| {
                    log::error!(error:?; "DirectPageRequest::commit: supply_pages failed");
                    ChunkedArchiveError::IntegrityError
                })?;
        }
        self.committed_len += size;
        Ok(())
    }
}

impl PageRequest for DirectPageRequest {
    fn prepare(&mut self, read_range: Range<u64>) -> Result<(), ChunkedArchiveError> {
        assert_eq!(self.vaddr, 0, "prepare must only be called once");
        self.read_range = read_range;
        let len = (self.read_range.end - self.read_range.start) as usize;
        let transfer_vmo =
            zx::Vmo::create(len as u64).map_err(|_| ChunkedArchiveError::IntegrityError)?;
        let vaddr = fuchsia_runtime::vmar_root_self()
            .map(0, &transfer_vmo, 0, len, zx::VmarFlags::PERM_READ | zx::VmarFlags::PERM_WRITE)
            .map_err(|_| ChunkedArchiveError::IntegrityError)?;

        self.transfer_vmo = Some(transfer_vmo);
        self.vaddr = vaddr;
        Ok(())
    }
}

impl Drop for DirectPageRequest {
    fn drop(&mut self) {
        let uncommitted_start = std::cmp::max(
            self.original_range.start,
            self.read_range.start + self.committed_len as u64,
        );
        if uncommitted_start < self.original_range.end {
            let vmos_guard = self.pager.vmos.lock();
            if let Some(paged_vmo) = vmos_guard.get(&self.key) {
                if paged_vmo.options.contains(fblock::CreateVmoOptions::SUPPLY_ZEROES_ON_ERROR) {
                    let _ = paged_vmo.vmo.signal(zx::Signals::empty(), zx::Signals::USER_0);
                    let uncommitted_len = self.original_range.end - uncommitted_start;
                    if let Ok(zero_vmo) = zx::Vmo::create(uncommitted_len) {
                        let _ = self.pager.pager.supply_pages(
                            &paged_vmo.vmo,
                            uncommitted_start..self.original_range.end,
                            &zero_vmo,
                            0,
                        );
                    }
                } else {
                    let _ = self.pager.pager.op_range(
                        zx::PagerOp::Fail(zx::Status::IO),
                        &paged_vmo.vmo,
                        uncommitted_start..self.original_range.end,
                    );
                }
            }
        }
        if self.vaddr != 0 {
            let len = (self.read_range.end - self.read_range.start) as usize;
            // SAFETY: `self.vaddr` was mapped in `prepare` with `len` bytes in the root VMAR.
            unsafe {
                let _ = fuchsia_runtime::vmar_root_self().unmap(self.vaddr, len);
            }
        }
    }
}

struct MapperVmoThread {
    mapping_vmo: zx::Vmo,
    thread: Option<JoinHandle<()>>,
}

impl MapperVmoThread {
    fn spawn<D: DeliveryHandler>(
        mapping_vmo: &zx::Vmo,
        files: Arc<Files<dyn BlockService, D>>,
    ) -> Result<Self, Error> {
        let mapping_vmo_dup = mapping_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        let thread_vmo = mapping_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS)?;
        let files_for_vmo = files.clone();
        let thread = std::thread::spawn(move || {
            match Receiver::<RawMappingCommand>::new(mapping_vmo_dup, PENDING_COMMANDS_CAPACITY) {
                Ok(mut receiver) => {
                    while let Ok(msg) = receiver.peek() {
                        if let Err(error) = process_mapping_command(&msg, &files_for_vmo) {
                            log::error!(error:?; "Failed to process mapping command");
                        }
                        if let Err(error) = msg.pop() {
                            log::error!(error:?; "Failed to pop mapping command from FIFO");
                        }
                    }
                }
                Err(error) => {
                    log::error!(error:?; "Failed to create mapping VMO FIFO receiver");
                }
            }
        });
        Ok(Self { mapping_vmo: thread_vmo, thread: Some(thread) })
    }
}

impl Drop for MapperVmoThread {
    fn drop(&mut self) {
        let _ = self.mapping_vmo.signal(zx::Signals::empty(), SIG_SHUTDOWN);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

async fn run_mapper_session_loop<M: MapperHandler + ?Sized, D: DeliveryHandler>(
    handler: Arc<M>,
    service: Arc<dyn BlockService>,
    session: ServerEnd<fblock::MapperSessionMarker>,
    mapping_vmo: zx::Vmo,
    files: Arc<Files<dyn BlockService, D>>,
    direct_pager: Option<DirectPager>,
) -> Result<(), Error> {
    let _mapper_vmo_thread = MapperVmoThread::spawn(&mapping_vmo, files.clone())?;

    let scope = fasync::Scope::new();
    let mut stream = session.into_stream();
    while let Some(request) = stream.try_next().await? {
        match request {
            fblock::MapperSessionRequest::CreateVmo { key, size, options, responder } => {
                if let Some(direct_pager) = direct_pager.as_ref() {
                    match direct_pager.create_vmo(key, size, options) {
                        Ok(vmo) => {
                            responder.send(Ok(vmo))?;
                        }
                        Err(status) => {
                            responder.send(Err(status.into_raw()))?;
                        }
                    }
                } else {
                    responder.send(Err(zx::Status::NOT_SUPPORTED.into_raw()))?;
                }
            }
            fblock::MapperSessionRequest::OpenChildSession {
                session,
                mapping_vmo,
                parent_key,
                port,
                delivery_queue,
                responder,
            } => {
                let parent_file = match files.wait_for_file(parent_key).await {
                    Ok(file) => file,
                    Err(error) => {
                        log::warn!(error:?; "Failed to load parent file for key {parent_key}");
                        responder.send(Err(zx::Status::NOT_FOUND.into_raw()))?;
                        continue;
                    }
                };
                let child_service = Arc::new(ChildBlockService::new(service.clone(), parent_file));
                match serve_mapper_session(
                    handler.clone(),
                    child_service,
                    session,
                    mapping_vmo,
                    port,
                    delivery_queue,
                ) {
                    Ok(child_fut) => {
                        scope.spawn(async move {
                            if let Err(e) = child_fut.await {
                                log::warn!(e:?; "Child mapper session failed");
                            }
                        });
                        responder.send(Ok(()))?;
                    }
                    Err(status) => {
                        log::warn!(status:?; "serve_mapper_session failed for child session");
                        responder.send(Err(status.into_raw()))?;
                    }
                }
            }
            fblock::MapperSessionRequest::Close { responder } => {
                responder.send(Ok(()))?;
                break;
            }
            fblock::MapperSessionRequest::_UnknownMethod { .. } => {}
        }
    }

    scope.cancel().await;
    Ok(())
}

pub fn serve_mapper_session<M: MapperHandler + ?Sized>(
    handler: Arc<M>,
    service: Arc<dyn BlockService>,
    session: ServerEnd<fblock::MapperSessionMarker>,
    mapping_vmo: zx::Vmo,
    port: Option<zx::Port>,
    delivery_queue: Option<zx::Vmo>,
) -> Result<BoxFuture<'static, Result<(), Error>>, zx::Status> {
    match (port, delivery_queue) {
        (Some(port), Some(delivery_queue)) => {
            let verifier = handler.on_open_mapper_session(&mapping_vmo, delivery_queue)?;
            let files = Arc::new(Files::new(service.clone(), verifier, port));
            let pager_thread = files.spawn_pager_thread();
            Ok(async move {
                let _pager_thread = pager_thread;
                run_mapper_session_loop(handler, service, session, mapping_vmo, files, None).await
            }
            .boxed())
        }
        (None, None) => {
            let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty())?);
            let port = zx::Port::create();
            let direct_pager =
                DirectPager::new(pager, port.duplicate_handle(zx::Rights::SAME_RIGHTS)?);
            let files = Arc::new(Files::new(service.clone(), direct_pager.clone(), port));
            let pager_thread = files.spawn_pager_thread();
            Ok(async move {
                let _pager_thread = pager_thread;
                run_mapper_session_loop(
                    handler,
                    service,
                    session,
                    mapping_vmo,
                    files,
                    Some(direct_pager),
                )
                .await
            }
            .boxed())
        }
        _ => Err(zx::Status::INVALID_ARGS),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fidl::endpoints::create_proxy;
    use fidl_fuchsia_storage_block as fblock;
    use mapping::{Extent, Extents, MappingCommand};
    use storage_device::buffer::OwnedBuffer;
    use storage_device::buffer_allocator::{BufferAllocator, BufferSource};
    use vmo_fifo::SyncSender;

    struct FakeHandler;
    impl MapperHandler for FakeHandler {
        fn on_open_mapper_session(
            &self,
            _mapping_vmo: &zx::Vmo,
            delivery_queue: zx::Vmo,
        ) -> Result<Verifier, zx::Status> {
            Ok(Verifier::new(delivery_queue))
        }
    }

    struct FakeBlockService {
        allocator: Arc<BufferAllocator>,
        device_data: Vec<u8>,
    }

    impl FakeBlockService {
        fn new(device_data: Vec<u8>) -> Self {
            let source = BufferSource::new(1024 * 1024);
            let allocator = Arc::new(BufferAllocator::new(4096, source));
            Self { allocator, device_data }
        }
    }

    impl BlockService for FakeBlockService {
        fn allocate_buffer(&self, max_len: usize) -> OwnedBuffer {
            self.allocator.allocate_buffer_sync_owned(max_len)
        }

        fn read_blocks(
            &self,
            device_offset: u64,
            mut dest_buffer: OwnedBuffer,
            on_complete: Box<dyn FnOnce(Result<OwnedBuffer, Error>) + Send>,
        ) -> Result<(), Error> {
            let start = device_offset as usize;
            let len = dest_buffer.len();
            dest_buffer.copy_from_slice(&self.device_data[start..start + len]);
            on_complete(Ok(dest_buffer));
            Ok(())
        }
    }

    #[fuchsia::test]
    fn test_direct_page_request_incremental_commit() {
        let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).unwrap());
        let port = zx::Port::create();
        let direct_pager = Arc::new(DirectPager::new(
            pager.clone(),
            port.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        ));

        let key = 42u64;
        let vmo = direct_pager.create_vmo(key, 8192, fblock::CreateVmoOptions::empty()).unwrap();

        let mut request = direct_pager.get_page_request(key, 0..8192);
        request.prepare(0..8192).expect("prepare");

        let page1 = vec![0xAAu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page1);
        request.commit(4096).expect("commit page 1");

        let page2 = vec![0xBBu8; 4096];
        request.mut_ptr_slice().subslice_mut(0..4096).copy_from_slice(&page2);
        request.commit(4096).expect("commit page 2");

        let mut read_buf = vec![0u8; 8192];
        vmo.read(&mut read_buf, 0).expect("read vmo");
        assert_eq!(&read_buf[..4096], &page1[..]);
        assert_eq!(&read_buf[4096..], &page2[..]);
    }

    #[fuchsia::test]
    fn test_direct_page_request_drop_fails_uncommitted() {
        let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).unwrap());
        let port = zx::Port::create();
        let direct_pager = Arc::new(DirectPager::new(
            pager.clone(),
            port.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        ));

        let key = 43u64;
        let vmo = direct_pager.create_vmo(key, 8192, fblock::CreateVmoOptions::empty()).unwrap();
        let vmo_clone = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();

        let reader_thread = std::thread::spawn(move || {
            let mut buf = vec![0u8; 4096];
            vmo_clone.read(&mut buf, 0)
        });

        let packet = port.wait(zx::MonotonicInstant::INFINITE).expect("port wait");
        assert_eq!(packet.key(), key);

        let mut request = direct_pager.get_page_request(key, 0..8192);
        request.prepare(0..8192).expect("prepare");
        drop(request);

        assert_eq!(reader_thread.join().unwrap(), Err(zx::Status::IO));
    }

    #[fuchsia::test]
    fn test_direct_page_request_drop_supplies_zeroes_on_error() {
        let pager = Arc::new(zx::Pager::create(zx::PagerOptions::empty()).unwrap());
        let port = zx::Port::create();
        let direct_pager = Arc::new(DirectPager::new(
            pager.clone(),
            port.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
        ));

        let key = 44u64;
        let vmo = direct_pager
            .create_vmo(key, 8192, fblock::CreateVmoOptions::SUPPLY_ZEROES_ON_ERROR)
            .unwrap();
        assert_eq!(
            vmo.wait_one(zx::Signals::USER_0, zx::MonotonicInstant::INFINITE_PAST).to_result(),
            Err(zx::Status::TIMED_OUT)
        );
        let vmo_clone = vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap();

        let reader_thread = std::thread::spawn(move || {
            let mut buf = vec![0xFFu8; 4096];
            vmo_clone.read(&mut buf, 0).expect("read should succeed with zeroes");
            buf
        });

        let packet = port.wait(zx::MonotonicInstant::INFINITE).expect("port wait");
        assert_eq!(packet.key(), key);

        let mut request = direct_pager.get_page_request(key, 0..8192);
        request.prepare(0..8192).expect("prepare");
        drop(request);

        let buf = reader_thread.join().unwrap();
        assert_eq!(buf, vec![0u8; 4096]);
        assert!(
            vmo.wait_one(zx::Signals::USER_0, zx::MonotonicInstant::INFINITE_PAST)
                .to_result()
                .is_ok()
        );
    }

    #[fuchsia::test]
    async fn test_mapper_session_create_vmo_and_page_fault() {
        let block_data = vec![0x42u8; 8192];
        let service = Arc::new(FakeBlockService::new(block_data.clone()));
        let handler = Arc::new(FakeHandler);

        let (proxy, server) = create_proxy::<fblock::MapperSessionMarker>();
        let mapping_vmo = zx::Vmo::create(65536).unwrap();

        let fut = serve_mapper_session(
            handler,
            service,
            server,
            mapping_vmo.duplicate_handle(zx::Rights::SAME_RIGHTS).unwrap(),
            None,
            None,
        )
        .unwrap();
        let task = fasync::Task::spawn(async move {
            let _ = fut.await;
        });

        let key = 100u64;
        let paged_vmo = proxy
            .create_vmo(key, 8192, fblock::CreateVmoOptions::empty())
            .await
            .unwrap()
            .expect("create_vmo failed");

        let extents = vec![Extent::try_new(0..8192, Some(0)).unwrap()];
        let encoded_extents = Extents::try_new(&extents, 0).unwrap();
        let mut sender = SyncSender::<RawMappingCommand>::new(mapping_vmo, 8, 256).unwrap();
        let mut payload = sender.reserve_payload(8).unwrap();
        for (mut chunk, val) in
            payload.data().chunks_mut(8).zip(Extents::encode_extents(&encoded_extents))
        {
            chunk.copy_from_slice(&val.to_le_bytes());
        }
        let cmd = MappingCommand::Mappings {
            key,
            offset: payload.offset() as u32,
            stored_size: 8192,
            device_offset: 0,
            metadata_count: 0,
            extent_count: 1,
            encrypted: false,
        };
        payload.commit(cmd.into()).unwrap();

        let mut read_buf = vec![0u8; 8192];
        let read_buf = fasync::unblock(move || {
            paged_vmo.read(&mut read_buf, 0).expect("read paged_vmo");
            read_buf
        })
        .await;
        assert_eq!(read_buf, block_data);

        proxy.close().await.unwrap().expect("close session");
        task.await;
    }
}
