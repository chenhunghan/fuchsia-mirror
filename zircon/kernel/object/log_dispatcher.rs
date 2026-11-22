// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::debuglog_rs::{DlogReader, PersistLog, dlog_record_t, dlog_write};
use crate::ksync_kernel::TrackedCriticalMutex;
use fbl::Canary;
use ksync::{KMutex, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{
    ZX_LOG_FLAG_READABLE, ZX_LOG_READABLE, ZX_OBJ_TYPE_DEBUGLOG, ZX_RIGHT_DUPLICATE,
    ZX_RIGHT_INSPECT, ZX_RIGHT_READ, ZX_RIGHT_SIGNAL, ZX_RIGHT_TRANSFER, ZX_RIGHT_WAIT,
    ZX_RIGHT_WRITE, zx_rights_t,
};

use super::log_dispatcher_ffi::cpp_log_dispatcher_create;
use super::{DispatcherOps, KernelHandle};

use object_constants_rs as object_constants;

const ZX_DEFAULT_LOG_READ_RIGHTS: zx_rights_t = ZX_RIGHT_TRANSFER
    | ZX_RIGHT_DUPLICATE
    | ZX_RIGHT_READ
    | ZX_RIGHT_WRITE
    | ZX_RIGHT_SIGNAL
    | ZX_RIGHT_WAIT
    | ZX_RIGHT_INSPECT;

const ZX_DEFAULT_LOG_WRITE_RIGHTS: zx_rights_t = ZX_RIGHT_TRANSFER
    | ZX_RIGHT_DUPLICATE
    | ZX_RIGHT_WRITE
    | ZX_RIGHT_SIGNAL
    | ZX_RIGHT_WAIT
    | ZX_RIGHT_INSPECT;

zr::static_assert_size_and_align!(
    LogDispatcherState,
    object_constants::kLogDispatcherStateSize,
    object_constants::kLogDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_LOG_CREATE_COUNT, "dispatcher.log.create", Sum);
define_kcounter!(DISPATCHER_LOG_DESTROY_COUNT, "dispatcher.log.destroy", Sum);

#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct LogDispatcherState {
    canary: Canary<{ fbl::magic(b"LOGD") }>,

    #[pin]
    #[guarded_by(lock)]
    reader: DlogReader,

    flags: u32,

    #[mutex]
    lock: KMutex<TrackedCriticalMutex>,
}

impl LogDispatcherState {
    /// Creates a `PinInit` initializer for `LogDispatcherState`.
    pub fn init(
        _dispatcher: *const LogDispatcher,
        flags: u32,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_LOG_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            flags,
            lock <- KMutex::init(),
            reader <- ksync::kcell_init(DlogReader::new()),
        })
    }

    fn flags(&self) -> u32 {
        self.flags
    }
}

#[pinned_drop]
impl PinnedDrop for LogDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        DISPATCHER_LOG_DESTROY_COUNT.add(1);
        let this = self.project();
        if (*this.flags & ZX_LOG_FLAG_READABLE) != 0 {
            let reader_ptr =
                core::ptr::from_ref(this.reader.into_ref().get_ref()).cast::<DlogReader>();
            // SAFETY: `KCell<T, _>` is `#[repr(transparent)]` over `UnsafeCell<T>`, `self.reader`
            // is structurally pinned, and `DlogReader::disconnect` synchronizes internal mutation
            // via atomics and `DLog::readers_lock`.
            unsafe { core::pin::Pin::new_unchecked(&*reader_ptr) }.disconnect();
        }
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    pub struct LogDispatcher,
    LogDispatcherState,
    ZX_OBJ_TYPE_DEBUGLOG,
    object_constants::kLogDispatcherStateOffset
);

impl LogDispatcher {
    /// Callback invoked by `DlogReader` when new log records arrive.
    ///
    /// # Safety
    ///
    /// `cookie` must point to a valid `LogDispatcher`.
    unsafe fn notify(cookie: *mut core::ffi::c_void) {
        // SAFETY: `cookie` was passed during `DlogReader::initialize` in `create` and points to
        // the `LogDispatcher`, which disconnects `reader` in `LogDispatcherState::drop` before
        // destruction.
        unsafe {
            let dispatcher = cookie.cast::<Self>();
            (*dispatcher).update_state(0, ZX_LOG_READABLE);
        }
    }

    /// Returns default rights for a LogDispatcher given creation `flags`.
    fn default_rights(flags: u32) -> zx_rights_t {
        if (flags & ZX_LOG_FLAG_READABLE) != 0 {
            ZX_DEFAULT_LOG_READ_RIGHTS
        } else {
            ZX_DEFAULT_LOG_WRITE_RIGHTS
        }
    }

    /// Creates a new LogDispatcher and returns its kernel handle and rights.
    pub fn create(flags: u32) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        let rights = Self::default_rights(flags);
        // SAFETY: `cpp_log_dispatcher_create` initializes `handle` on success.
        let handle = unsafe { KernelHandle::create(|out| cpp_log_dispatcher_create(flags, out)) }?;
        if (flags & ZX_LOG_FLAG_READABLE) != 0 {
            let dispatcher_ptr = fbl::RefPtr::as_ptr(handle.dispatcher());
            // Ideally we would take the lock here, however `notify` may be called from
            // `initialize`, so locking could cause a re-entrant cycle. We haven't had the
            // opportunity to escape the pointer at this point so nothing else should have the lock
            // anyway.
            //
            // SAFETY: We have exclusive access to `dispatcher_ptr` before `handle` escapes,
            // `(*dispatcher_ptr).state().reader` (`#[repr(transparent)]` `KCell<DlogReader, _>`) is
            // structurally pinned inside the heap-allocated `LogDispatcher`, `LogDispatcherState`
            // disconnects `reader` in `drop` before `LogDispatcher` is destroyed, and
            // `dlog_init_early` has completed.
            unsafe {
                let reader_ptr =
                    core::ptr::from_ref(&(*dispatcher_ptr).state().reader).cast::<DlogReader>();
                core::pin::Pin::new_unchecked(&*reader_ptr).initialize(
                    Some(Self::notify),
                    dispatcher_ptr.cast_mut().cast(),
                    None,
                );
            }
        }
        Ok((handle, rights))
    }

    /// Writes `bytes` to debuglog with given options/flags.
    pub fn write(&self, severity: u32, flags: u32, bytes: &[u8]) -> Result<(), Status> {
        dlog_write(severity, self.state().flags() | flags, bytes, PersistLog::Yes)
    }

    /// Reads a record from debuglog.
    pub fn read(&self, record: &mut dlog_record_t) -> Result<(), Status> {
        if (self.state().flags() & ZX_LOG_FLAG_READABLE) == 0 {
            return Err(Status::BAD_STATE);
        }

        ksync::lock!(let guard = self.state().lock_lock());
        // SAFETY: `self.state().reader` is structurally pinned inside the heap-allocated
        // `LogDispatcher` and is never moved.
        let reader = unsafe { core::pin::Pin::new_unchecked(guard.reader()) };
        let res = reader.read(0, record).map(|_| ());
        if res == Err(Status::SHOULD_WAIT) {
            self.update_state_locked(guard.token(), ZX_LOG_READABLE, 0);
        }
        res
    }
}
