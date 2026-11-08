// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! A lazily-borrowed [`OwnedWaitQueue`], implementing the C++ `LazyOwnedWaitQueueImpl` declared in
//! `<kernel/lazy_owned_wait_queue.h>`, which forwards to this implementation.

use core::ffi::c_void;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::{align_of, offset_of, size_of};
use core::pin::Pin;
use core::ptr::{self, NonNull};
use core::sync::atomic::{AtomicPtr, Ordering};
use ksync::LockToken;
use pin_init::{pin_data, pinned_drop};
use zr::static_assert;

use crate::kernel::owned_wait_queue::OwnedWaitQueue;
use crate::kernel::owned_wait_queue_pool::{OwnedWaitQueuePool, OwnedWaitQueuePoolLock};
use crate::ksync_kernel::IrqSavePolicy;

/// Policy selecting the [`OwnedWaitQueuePool`] that a [`LazyOwnedWaitQueueImpl`] borrows queues
/// from and recycles them to.
///
/// # Safety
///
/// [`pool`](Self::pool) must return the same pool on every call for as long as `self` exists, and
/// that pool must outlive `self`. In particular, the returned reference must not borrow from the
/// storage of `self`, and must remain valid after `self` is destroyed.
/// [`LazyOwnedWaitQueueImpl::release_locked_raw`] relies on this to recycle a queue to the pool
/// that it was borrowed from after `self` may have been destroyed.
pub unsafe trait OwqPoolAccess {
    /// Returns the pool to borrow queues from and recycle them to.
    ///
    /// Called with [`OwnedWaitQueuePoolLock`] held, so must not acquire it.
    fn pool(&self) -> &OwnedWaitQueuePool;
}

/// Global pool accessor policy, always selecting the global [`OwnedWaitQueuePool`].
///
/// Occupies no storage, like the empty C++ `GlobalOwqPoolAccess` base.
#[derive(Debug, Clone, Copy, Default)]
#[repr(C)]
pub struct GlobalOwqPoolAccess;

// SAFETY: `pool` always returns the global pool, which is never destroyed.
unsafe impl OwqPoolAccess for GlobalOwqPoolAccess {
    #[inline]
    fn pool(&self) -> &OwnedWaitQueuePool {
        OwnedWaitQueuePool::get()
    }
}

/// Local pool accessor policy for tests.
///
/// Layout-compatible with the C++ `LocalOwqPoolAccess`.
///
/// Unlike [`GlobalOwqPoolAccess`], this has `Freeze` bytes (`pool`), so a
/// [`LocalLazyOwnedWaitQueue`] must not be destroyed while another thread still holds a `&Self` to
/// it, even one that only uses the `*_raw` functions.
#[repr(C)]
pub struct LocalOwqPoolAccess<'a> {
    pool: &'a OwnedWaitQueuePool,
}

// SAFETY: `pool` always returns `self.pool`, which is never modified after construction and
// outlives `self` because it is borrowed for `'a`.
unsafe impl OwqPoolAccess for LocalOwqPoolAccess<'_> {
    #[inline]
    fn pool(&self) -> &OwnedWaitQueuePool {
        self.pool
    }
}

/// A wrapper class for [`OwnedWaitQueue`] that lazily borrows a queue from a pool
/// when necessary.
///
/// Layout-compatible with the C++ `LazyOwnedWaitQueueImpl<PoolAccess>`, whose `PoolAccess` base
/// precedes its pointer.
///
/// # Invariants
///
/// `ptr` is either null or points to a queue that [`Self::get`] borrowed from
/// `pool_access.pool()` and that is not in that pool's free list. `ptr` is only set to non-null
/// by [`Self::get`] while holding [`OwnedWaitQueuePoolLock`], and is only cleared by
/// [`Self::detach_raw`].
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct LazyOwnedWaitQueueImpl<P: OwqPoolAccess> {
    pool_access: P,
    ptr: AtomicPtr<OwnedWaitQueue>,
    /// No moving or copying allowed; wrapped in `PhantomData` so types embedding
    /// `LazyOwnedWaitQueue` stay FFI-safe (`improper_ctypes` rejects a bare `PhantomPinned`).
    #[pin]
    _pin: PhantomData<PhantomPinned>,
}

/// A [`LazyOwnedWaitQueueImpl`] that borrows from the global [`OwnedWaitQueuePool`].
///
/// Every byte of a `LazyOwnedWaitQueue` is interior-mutable (`GlobalOwqPoolAccess` is zero-sized
/// and `ptr` is atomic), so it has no `Freeze` bytes. `Mutex` relies on this, since it may be
/// destroyed by another thread as soon as its queue is detached.
pub type LazyOwnedWaitQueue = LazyOwnedWaitQueueImpl<GlobalOwqPoolAccess>;

/// A [`LazyOwnedWaitQueueImpl`] that borrows from a specific [`OwnedWaitQueuePool`], for tests.
pub type LocalLazyOwnedWaitQueue<'a> = LazyOwnedWaitQueueImpl<LocalOwqPoolAccess<'a>>;

// These must match the sizes asserted against the C++ types in
// `<kernel/lazy_owned_wait_queue.h>`.
// LazyOwnedWaitQueue must be exactly pointer-sized.
static_assert!(size_of::<LazyOwnedWaitQueue>() == size_of::<*mut c_void>());
static_assert!(align_of::<LazyOwnedWaitQueue>() == align_of::<*mut c_void>());
static_assert!(offset_of!(LazyOwnedWaitQueue, ptr) == 0);
static_assert!(size_of::<LocalLazyOwnedWaitQueue<'static>>() == 2 * size_of::<*mut c_void>());
static_assert!(align_of::<LocalLazyOwnedWaitQueue<'static>>() == align_of::<*mut c_void>());
static_assert!(offset_of!(LocalLazyOwnedWaitQueue<'static>, ptr) == size_of::<*mut c_void>());

// Both instantiations are shared between threads, e.g. by every thread contending on a `Mutex`.
const _: () = {
    const fn assert_sync<T: Sync>() {}
    assert_sync::<LazyOwnedWaitQueue>();
    assert_sync::<LocalLazyOwnedWaitQueue<'static>>();
};

impl LazyOwnedWaitQueue {
    /// Creates an empty `LazyOwnedWaitQueue` that borrows from the global pool.
    ///
    /// Its bit pattern is all zeros, matching a constant-initialized C++ `LazyOwnedWaitQueue`.
    pub const fn new() -> Self {
        Self {
            pool_access: GlobalOwqPoolAccess,
            ptr: AtomicPtr::new(ptr::null_mut()),
            _pin: PhantomData,
        }
    }
}

impl Default for LazyOwnedWaitQueue {
    fn default() -> Self {
        Self::new()
    }
}

impl<'a> LocalLazyOwnedWaitQueue<'a> {
    /// Creates an empty `LocalLazyOwnedWaitQueue` that borrows from `pool`.
    pub const fn new(pool: &'a OwnedWaitQueuePool) -> Self {
        Self {
            pool_access: LocalOwqPoolAccess { pool },
            ptr: AtomicPtr::new(ptr::null_mut()),
            _pin: PhantomData,
        }
    }
}

impl<P: OwqPoolAccess> LazyOwnedWaitQueueImpl<P> {
    /// Returns a pointer to an [`OwnedWaitQueue`], borrowing from the pool if necessary.
    ///
    /// Callers should only call this when they are preparing to use the returned queue to block.
    /// Callers are responsible for coordinating amongst each other so that the last caller on each
    /// `LazyOwnedWaitQueue` to *unblock* after calling this calls [`Self::detach`] or
    /// [`Self::release_locked`] to recycle the queue to the pool.
    ///
    /// Thread-safe. Will not block but may acquire the pool spinlock.
    ///
    /// The returned queue may be detached and recycled by another thread at any time, so callers
    /// must lock it and then check that [`Self::peek`] still returns it before using it. A
    /// recycled queue remains valid for as long as its pool does not free it: the global pool
    /// never frees its queues (see [`OwnedWaitQueuePool::shrink`]), and the queues of a local pool
    /// are only freed by its owner.
    ///
    /// # Panics
    ///
    /// Panics if a queue must be borrowed but the pool is empty.
    #[inline]
    pub fn get(&self) -> NonNull<OwnedWaitQueue> {
        // Ordering: `Acquire` pairs with the `Release` store in `get_slow`, so that a queue
        // published by another thread is observed fully initialized.
        if let Some(current) = NonNull::new(self.ptr.load(Ordering::Acquire)) {
            return current;
        }
        self.get_slow()
    }

    /// The slow path of [`Self::get`]: borrows a queue from the pool under the pool lock, unless
    /// another thread has already done so. Kept out of line so that the common case in `get` needs
    /// no stack frame.
    #[inline(never)]
    #[cold]
    fn get_slow(&self) -> NonNull<OwnedWaitQueue> {
        ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock_policy::<IrqSavePolicy>());

        // Ordering: `Relaxed` suffices because `ptr` is only set to non-null while holding the
        // pool lock (see the type invariants), which is held here, so any non-null value observed
        // was published before the lock was last released and is visible through its acquisition.
        //
        // Both arms fall through to a single exit, rather than returning early, so that the
        // release of `guard` is not duplicated in the generated code.
        match NonNull::new(self.ptr.load(Ordering::Relaxed)) {
            Some(current) => current,
            None => {
                let new_queue = self.pool_access.pool().borrow_locked(guard.as_mut().token_mut());

                // Ordering: `Release` publishes the borrowed queue to lock-free readers in
                // `get`/`peek`.
                self.ptr.store(new_queue.as_ptr(), Ordering::Release);
                new_queue
            }
        }
    }

    /// Releases the underlying [`OwnedWaitQueue`] back to the pool if it exists.
    ///
    /// Will not block.
    ///
    /// # Safety
    ///
    /// In addition to the requirements of [`Self::detach`], the attached queue (if any) must have
    /// no owner, and there must be no concurrent mutation of its owner or waiter fields.
    pub unsafe fn release_locked(&self, token: &mut LockToken<'_, OwnedWaitQueuePoolLock>) {
        // SAFETY: `self` is a valid reference, and the caller upholds the requirements of
        // `release_locked`.
        unsafe { Self::release_locked_raw(self, token) }
    }

    /// Like [`Self::release_locked`], but takes a raw pointer so that no reference to `*this`
    /// outlives the detach, after which another thread may destroy `*this`.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid `LazyOwnedWaitQueueImpl` from the start of the call until the
    /// queue is detached; another thread may destroy `*this` as soon as the detach is visible.
    /// The attached queue (if any) must have no owner, there must be no concurrent mutation of its
    /// owner or waiter fields, and the caller must uphold the remaining requirements of
    /// [`Self::detach_raw`].
    pub unsafe fn release_locked_raw(
        this: *const Self,
        token: &mut LockToken<'_, OwnedWaitQueuePoolLock>,
    ) {
        // Look up the pool before detaching the queue, after which `*this` may be destroyed.
        // `OwqPoolAccess` guarantees that the pool outlives `*this`.
        // SAFETY: The caller guarantees that `this` is valid until the queue is detached.
        let pool = unsafe { (*this).pool_access.pool() };

        // SAFETY: The caller guarantees that `this` is valid until the queue is detached and
        // upholds the requirements of `detach_raw`.
        if let Some(queue) = unsafe { Self::detach_raw(this) } {
            // SAFETY: By the type invariants, `queue` was borrowed from `pool` by `get`, since
            // `OwqPoolAccess` guarantees that `pool()` always returns the same pool, so it is a
            // valid, pinned queue allocated from that pool's cache. It has just been detached, and
            // the caller guarantees that it has no owner (and so is in no list), no waiters, and
            // no concurrent mutation of its owner or waiter fields.
            unsafe { pool.recycle_locked(queue, token) };
        }
    }

    /// Detaches the underlying queue and returns it. Caller takes ownership and is responsible
    /// for calling [`OwnedWaitQueuePool::recycle_locked`] on the returned queue when done.
    ///
    /// Will not block.
    ///
    /// # Safety
    ///
    /// `*self` must not be destroyed before this call returns; if another thread may destroy
    /// `*self` as soon as the queue is detached, use [`Self::detach_raw`] instead.
    ///
    /// No thread may be blocked in the attached queue, and no thread other than the caller may
    /// hold the attached queue's lock having observed via [`Self::peek`] that it is attached:
    /// callers of [`Self::get`] rely on a queue that they have locked and observed attached
    /// staying attached for as long as they hold its lock. Callers ensure this by holding the
    /// queue's lock across the detach.
    #[must_use]
    pub unsafe fn detach(&self) -> Option<NonNull<OwnedWaitQueue>> {
        // SAFETY: `self` is a valid reference, and the caller upholds the requirements of
        // `detach`.
        unsafe { Self::detach_raw(self) }
    }

    /// Like [`Self::detach`], but takes a raw pointer so that no reference to `*this` outlives
    /// the detach, after which another thread may destroy `*this`. For example, the C++
    /// `Mutex::~Mutex()` proceeds as soon as `peek` returns `None` or the detached queue's lock
    /// is released, and releasers hold that lock across the detach, so the destroyer may proceed
    /// as soon as the detach is visible.
    ///
    /// # Safety
    ///
    /// `this` must point to a valid `LazyOwnedWaitQueueImpl` from the start of the call until the
    /// queue is detached; another thread may destroy `*this` as soon as the detach is visible.
    /// The caller must uphold the requirements of [`Self::detach`] other than the one on
    /// destroying `*self`.
    #[must_use]
    pub unsafe fn detach_raw(this: *const Self) -> Option<NonNull<OwnedWaitQueue>> {
        // Ordering: `AcqRel`: the `Acquire` half pairs with the `Release` store in `get_slow`, so
        // that the detached queue is observed fully initialized, and the `Release` half orders
        // this thread's prior accesses to the queue and to `*this` before any thread that
        // observes the null via `peek` (e.g. `Mutex::~Mutex()`) destroys `*this`.
        // SAFETY: The caller guarantees that `this` is valid until the swap takes effect, and
        // `*this` is not accessed afterwards. `AtomicPtr` is entirely interior-mutable, so the
        // `&AtomicPtr` created for the swap carries no `dereferenceable` or protector obligation
        // that extends past the swap (see rust-lang/rust#98017).
        NonNull::new(unsafe { (*this).ptr.swap(ptr::null_mut(), Ordering::AcqRel) })
    }

    /// Returns the current pointer without allocating. Will neither block nor acquire the pool
    /// spinlock. Thread-safe.
    pub fn peek(&self) -> Option<NonNull<OwnedWaitQueue>> {
        // Ordering: `Acquire` pairs with the `Release` store in `get_slow`, so that a published
        // queue is observed fully initialized, and with the `AcqRel` swap in `detach_raw`, so that
        // a caller observing `None` (e.g. before destroying `*self`) happens-after the detaching
        // thread's prior accesses.
        NonNull::new(self.ptr.load(Ordering::Acquire))
    }
}

impl<P: OwqPoolAccess> core::fmt::Debug for LazyOwnedWaitQueueImpl<P> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("LazyOwnedWaitQueueImpl").field("ptr", &self.peek()).finish_non_exhaustive()
    }
}

#[pinned_drop]
impl<P: OwqPoolAccess> PinnedDrop for LazyOwnedWaitQueueImpl<P> {
    fn drop(self: Pin<&mut Self>) {
        // The C++ shim destroys its trivially destructible `PoolAccess` base itself, so the
        // policy must not have drop glue of its own.
        const { assert!(!core::mem::needs_drop::<P>()) };
        // `peek` is an `Acquire` load, so that this destruction happens-after any
        // racing detacher's accesses.
        assert!(
            self.peek().is_none(),
            "Cannot destroy a LazyOwnedWaitQueue that has an OwnedWaitQueue."
        );
    }
}

/// Converts an optional queue to the nullable pointer returned to C++.
#[inline]
fn queue_to_raw(queue: Option<NonNull<OwnedWaitQueue>>) -> *mut OwnedWaitQueue {
    queue.map_or(ptr::null_mut(), NonNull::as_ptr)
}

/// Defines the C++ FFI entry points for one instantiation of [`LazyOwnedWaitQueueImpl`].
macro_rules! lazy_owned_wait_queue_ffi {
    (
        cpp_type: $cpp_type:literal,
        rust_type: $ty:ty,
        destroy: $destroy:ident,
        get: $get:ident,
        release_locked: $release_locked:ident,
        detach: $detach:ident,
        peek: $peek:ident $(,)?
    ) => {
        #[doc = concat!("C++ FFI entry point for `", $cpp_type, "::~LazyOwnedWaitQueueImpl()`.")]
        ///
        /// # Safety
        ///
        /// `lazy` must point to a valid, initialized object that will not be used again. No other
        /// thread may access `*lazy`, except for a `detach_raw` that has already performed its
        /// swap and does not access `*lazy` afterwards.
        // SAFETY: Symbol name is unique across the kernel link unit and matches the C++
        // declaration in `zircon/kernel/include/kernel/lazy_owned_wait_queue.h`.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $destroy(lazy: *mut $ty) {
            debug_assert!(!lazy.is_null());
            // SAFETY: The caller guarantees `lazy` points to a valid, initialized object that will
            // not be used again. A racing `detach_raw` that is still returning from its swap only
            // holds a `&AtomicPtr`, and `Pin<&mut Self>` of this `!Unpin` type asserts no
            // uniqueness, so this does not conflict with it.
            unsafe { ptr::drop_in_place(lazy) };
        }

        #[doc = concat!("C++ FFI entry point for `", $cpp_type, "::Get()`.")]
        // SAFETY: Symbol name is unique across the kernel link unit and matches the C++
        // declaration in `zircon/kernel/include/kernel/lazy_owned_wait_queue.h`.
        #[unsafe(no_mangle)]
        pub extern "C" fn $get(lazy: &$ty) -> *mut OwnedWaitQueue {
            lazy.get().as_ptr()
        }

        #[doc = concat!("C++ FFI entry point for `", $cpp_type, "::ReleaseLocked()`.")]
        ///
        /// Takes a raw pointer because another thread may destroy `*lazy` once its queue is
        /// detached.
        ///
        /// # Safety
        ///
        /// `lazy` must point to a valid object from the start of the call until the queue is
        /// detached, the caller must hold [`OwnedWaitQueuePoolLock`], and the caller must uphold
        /// the requirements of [`LazyOwnedWaitQueueImpl::release_locked_raw`].
        // SAFETY: Symbol name is unique across the kernel link unit and matches the C++
        // declaration in `zircon/kernel/include/kernel/lazy_owned_wait_queue.h`.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $release_locked(lazy: *const $ty) {
            debug_assert!(!lazy.is_null());
            // SAFETY: The caller holds `OwnedWaitQueuePoolLock` for the duration of this call
            // (enforced in C++ by `TA_REQ(OwnedWaitQueuePoolLock::Get())`), and `token` does not
            // outlive this function.
            let mut token = unsafe { LockToken::new() };
            // SAFETY: The caller guarantees that `lazy` is valid until the queue is detached and
            // upholds the requirements of `release_locked_raw`.
            unsafe { <$ty>::release_locked_raw(lazy, &mut token) };
        }

        #[doc = concat!("C++ FFI entry point for `", $cpp_type, "::Detach()`.")]
        ///
        /// Takes a raw pointer because another thread may destroy `*lazy` once its queue is
        /// detached.
        ///
        /// # Safety
        ///
        /// `lazy` must point to a valid object from the start of the call until the queue is
        /// detached, and the caller must uphold the requirements of
        /// [`LazyOwnedWaitQueueImpl::detach_raw`].
        // SAFETY: Symbol name is unique across the kernel link unit and matches the C++
        // declaration in `zircon/kernel/include/kernel/lazy_owned_wait_queue.h`.
        #[unsafe(no_mangle)]
        pub unsafe extern "C" fn $detach(lazy: *const $ty) -> *mut OwnedWaitQueue {
            debug_assert!(!lazy.is_null());
            // SAFETY: The caller guarantees that `lazy` is valid until the queue is detached and
            // upholds the requirements of `detach_raw`.
            queue_to_raw(unsafe { <$ty>::detach_raw(lazy) })
        }

        #[doc = concat!("C++ FFI entry point for `", $cpp_type, "::Peek()`.")]
        // SAFETY: Symbol name is unique across the kernel link unit and matches the C++
        // declaration in `zircon/kernel/include/kernel/lazy_owned_wait_queue.h`.
        #[unsafe(no_mangle)]
        pub extern "C" fn $peek(lazy: &$ty) -> *mut OwnedWaitQueue {
            queue_to_raw(lazy.peek())
        }
    };
}

lazy_owned_wait_queue_ffi! {
    cpp_type: "LazyOwnedWaitQueue",
    rust_type: LazyOwnedWaitQueue,
    destroy: rust_lazy_owned_wait_queue_destroy,
    get: rust_lazy_owned_wait_queue_get,
    release_locked: rust_lazy_owned_wait_queue_release_locked,
    detach: rust_lazy_owned_wait_queue_detach,
    peek: rust_lazy_owned_wait_queue_peek,
}

lazy_owned_wait_queue_ffi! {
    cpp_type: "LocalLazyOwnedWaitQueue",
    rust_type: LocalLazyOwnedWaitQueue<'_>,
    destroy: rust_local_lazy_owned_wait_queue_destroy,
    get: rust_local_lazy_owned_wait_queue_get,
    release_locked: rust_local_lazy_owned_wait_queue_release_locked,
    detach: rust_local_lazy_owned_wait_queue_detach,
    peek: rust_local_lazy_owned_wait_queue_peek,
}

/// Tests for [`LazyOwnedWaitQueue`].
#[cfg(ktest)]
#[unittest::suite(name = "lazy_owned_wait_queue_rust")]
mod tests {
    use super::{
        LazyOwnedWaitQueue, LocalLazyOwnedWaitQueue, OwnedWaitQueue, OwnedWaitQueuePool,
        OwnedWaitQueuePoolLock,
    };
    use crate::kernel::thread::{self, DEFAULT_PRIORITY, ThreadPtr};
    use crate::object_cache::UniquePtr;
    use crate::platform_rs::timer::InstantMono;
    use core::ffi::{CStr, c_void};
    use core::ptr::{self, NonNull};
    use pin_init::stack_pin_init;
    use unittest::{expect_eq, expect_true, unwrap_ok};
    use zx_status::Status;
    use zx_types::ZX_OK;

    /// Frees a queue borrowed from a stack-local pool, which must otherwise be empty when dropped.
    ///
    /// Must not be called while holding the pool lock, because freeing a queue to the pool's
    /// `ObjectCache` acquires a mutex.
    ///
    /// # Safety
    ///
    /// `queue` must have been allocated by `grow()` on a local pool that is still alive, must be
    /// uniquely owned by the caller (no longer in the pool's free list nor attached to any
    /// `LocalLazyOwnedWaitQueue`), and must not be used again.
    unsafe fn free_queue(queue: NonNull<OwnedWaitQueue>) {
        // SAFETY: The caller guarantees `queue` was allocated via `UniquePtr::into_raw` in
        // `grow()` from a cache that is still alive, and that it is uniquely owned.
        drop(unsafe { UniquePtr::<OwnedWaitQueue>::from_raw(queue.as_ptr()) });
    }

    /// Runs `worker` on a new thread for each element of `args`, and returns the threads' return
    /// codes once all of them have been joined.
    ///
    /// All of the threads are created before any is started, so that if creating one fails, no
    /// thread has run and an error is returned. Panics if a started thread cannot be joined,
    /// since it might otherwise still be accessing its argument after this function returns.
    ///
    /// # Safety
    ///
    /// It must be safe to call `worker` concurrently with each element of `args`, and whatever
    /// those elements point to must remain valid until this function returns.
    unsafe fn run_threads<const N: usize>(
        name: &CStr,
        worker: extern "C" fn(*mut c_void) -> i32,
        args: [*mut c_void; N],
    ) -> Result<[i32; N], Status> {
        let mut threads: [Option<ThreadPtr>; N] = [None; N];
        for (i, arg) in args.into_iter().enumerate() {
            // SAFETY: The caller guarantees that `worker` may be run with `arg` until this
            // function returns, and every thread created here is either forgotten without having
            // been started, or joined before this function returns.
            let result = unsafe {
                thread::create_with_priority(name.as_ptr(), worker, arg, DEFAULT_PRIORITY)
            };
            match result {
                Ok(thread) => threads[i] = Some(thread),
                Err(status) => {
                    for thread in threads.into_iter().flatten() {
                        // SAFETY: `thread` was created above and has never been started.
                        unsafe { thread.forget() };
                    }
                    return Err(status);
                }
            }
        }

        for thread in threads.into_iter().flatten() {
            // SAFETY: `thread` was just created and has not been resumed or joined yet.
            unsafe { thread.resume() };
        }

        let mut retcodes = [0; N];
        for (retcode, thread) in retcodes.iter_mut().zip(threads.into_iter().flatten()) {
            // SAFETY: `thread` has not been joined yet.
            match unsafe { thread.join(InstantMono::INFINITE) } {
                Ok(code) => *retcode = code,
                Err(status) => panic!("Failed to join a worker thread: {status:?}"),
            }
        }
        Ok(retcodes)
    }

    /// Verifies that a new `LazyOwnedWaitQueue` for the global pool is empty.
    #[test]
    fn global_new_test() {
        let lazy = LazyOwnedWaitQueue::new();
        expect_true!(lazy.peek().is_none());

        let lazy = LazyOwnedWaitQueue::default();
        expect_true!(lazy.peek().is_none());
    }

    /// Verifies lazy `get` borrowing, `peek`, and `release_locked` on the global pool.
    #[test]
    fn global_lazy_wrapper_test() {
        // Every contended `Mutex` borrows from and returns queues to the global pool in the same
        // way, so borrowing and returning a single queue here leaves the pool in a valid state.
        let lazy = LazyOwnedWaitQueue::new();
        expect_true!(lazy.peek().is_none());

        let queue = lazy.get();
        expect_true!(lazy.peek() == Some(queue));
        expect_true!(lazy.get() == queue);

        {
            ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
            // SAFETY: No other thread can block in or lock the attached queue, which has never had
            // an owner or waiters.
            unsafe { lazy.release_locked(guard.as_mut().token_mut()) };
        }
        expect_true!(lazy.peek().is_none());
    }

    /// Verifies lazy `get` borrowing, `peek`, and single-shot `detach` on a local pool.
    #[test]
    fn get_peek_detach_test() {
        stack_pin_init!(let stack_pool = OwnedWaitQueuePool::new());
        unwrap_ok!(stack_pool.grow());

        let queue = {
            let lazy = LocalLazyOwnedWaitQueue::new(&stack_pool);
            expect_true!(lazy.peek().is_none());

            // The first `get` borrows a queue, and subsequent calls return the same queue.
            let queue = lazy.get();
            expect_true!(lazy.peek() == Some(queue));
            expect_true!(lazy.get() == queue);

            // SAFETY: No other thread can block in or lock `queue`.
            expect_true!(unsafe { lazy.detach() } == Some(queue));
            expect_true!(lazy.peek().is_none());
            // SAFETY: No queue is attached.
            expect_true!(unsafe { LocalLazyOwnedWaitQueue::detach_raw(&lazy) }.is_none());
            queue
        };

        // `queue` was the only queue in the pool, so rather than recycling it, free it directly.
        // SAFETY: `queue` was allocated by `stack_pool.grow()`, was borrowed from its free list
        // by `get`, and has been detached, so it is uniquely owned here.
        unsafe { free_queue(queue) };
    }

    /// Verifies lazy `get` borrowing, `peek`, and `release_locked` on a local pool.
    #[test]
    fn lazy_wrapper_test() {
        stack_pin_init!(let stack_pool = OwnedWaitQueuePool::new());
        unwrap_ok!(stack_pool.grow());

        let queue = {
            let lazy = LocalLazyOwnedWaitQueue::new(&stack_pool);
            expect_true!(lazy.peek().is_none());

            let queue = lazy.get();
            expect_true!(lazy.peek() == Some(queue));
            // A second `get` returns the same queue.
            expect_true!(lazy.get() == queue);

            {
                ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
                // SAFETY: No other thread can block in or lock the attached queue, which has
                // never had an owner or waiters.
                unsafe { lazy.release_locked(guard.as_mut().token_mut()) };
            }
            expect_true!(lazy.peek().is_none());

            // Releasing an empty `LocalLazyOwnedWaitQueue` is a no-op.
            ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
            // SAFETY: No queue is attached.
            unsafe { lazy.release_locked(guard.as_mut().token_mut()) };
            queue
        };

        // The released queue was returned to the pool.
        let borrowed = {
            ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
            stack_pool.borrow_locked(guard.as_mut().token_mut())
        };
        expect_eq!(queue.as_ptr(), borrowed.as_ptr());
        // SAFETY: `borrowed` was allocated by `stack_pool.grow()` and is no longer in its free
        // list.
        unsafe { free_queue(borrowed) };
    }

    /// Verifies that threads racing to `get` an empty queue all observe the same queue.
    #[test]
    fn concurrent_get_test() {
        const NUM_THREADS: usize = 4;

        struct WorkerArgs<'a> {
            lazy: &'a LocalLazyOwnedWaitQueue<'a>,
            result: Option<NonNull<OwnedWaitQueue>>,
        }

        extern "C" fn worker(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to this worker's `WorkerArgs`, which outlives the thread and is
            // not accessed by any other thread until it is joined.
            let args = unsafe { &mut *arg.cast::<WorkerArgs<'_>>() };
            args.result = Some(args.lazy.get());
            0
        }

        stack_pin_init!(let stack_pool = OwnedWaitQueuePool::new());
        // Only one thread borrows a queue, but give the pool capacity for all of them.
        for _ in 0..NUM_THREADS {
            unwrap_ok!(stack_pool.grow());
        }

        {
            let lazy = LocalLazyOwnedWaitQueue::new(&stack_pool);
            let mut args: [WorkerArgs<'_>; NUM_THREADS] =
                core::array::from_fn(|_| WorkerArgs { lazy: &lazy, result: None });

            // SAFETY: Each `worker` only accesses its own `WorkerArgs` and the shared `lazy`, both
            // of which outlive the call.
            let retcodes = unwrap_ok!(unsafe {
                run_threads(
                    c"lazy_race_get_worker",
                    worker,
                    args.each_mut().map(|arg| ptr::from_mut(arg).cast()),
                )
            });
            for retcode in retcodes {
                expect_eq!(ZX_OK, retcode);
            }

            // Verify that all of the threads got the same queue.
            let winner = args[0].result;
            expect_true!(winner.is_some());
            for arg in &args[1..] {
                expect_true!(arg.result == winner);
            }
            expect_true!(lazy.peek() == winner);

            ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
            // SAFETY: All threads that used the attached queue have been joined, and it has never
            // had an owner or waiters.
            unsafe { lazy.release_locked(guard.as_mut().token_mut()) };
        }

        // Drain the pool so that it is empty when dropped.
        for _ in 0..NUM_THREADS {
            let queue = {
                ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
                stack_pool.borrow_locked(guard.as_mut().token_mut())
            };
            // SAFETY: `queue` was allocated by `stack_pool.grow()` and is no longer in its free
            // list.
            unsafe { free_queue(queue) };
        }
    }

    /// Stress-tests concurrent `get` and `release_locked_raw` on lazy queues sharing a pool.
    #[test]
    fn lazy_pool_stress_test() {
        const NUM_THREADS: usize = 16;
        const ITERATIONS: usize = 5000;

        extern "C" fn worker(arg: *mut c_void) -> i32 {
            // SAFETY: `arg` points to `stack_pool`, which outlives all worker threads.
            let pool = unsafe { &*arg.cast::<OwnedWaitQueuePool>() };
            for _ in 0..ITERATIONS {
                let lazy = LocalLazyOwnedWaitQueue::new(pool);
                lazy.get();
                ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
                // SAFETY: `lazy` is local to this thread, so no other thread can block in or lock
                // its queue, and no queue from this pool ever has an owner or waiters.
                unsafe {
                    LocalLazyOwnedWaitQueue::release_locked_raw(&lazy, guard.as_mut().token_mut())
                };
            }
            0
        }

        stack_pin_init!(let stack_pool = OwnedWaitQueuePool::new());
        for _ in 0..NUM_THREADS {
            unwrap_ok!(stack_pool.grow());
        }

        let pool_arg: *mut c_void =
            ptr::from_ref::<OwnedWaitQueuePool>(&stack_pool).cast_mut().cast();
        // SAFETY: `worker` only creates shared references to `stack_pool`, which outlives the
        // call.
        let retcodes = unwrap_ok!(unsafe {
            run_threads(c"lazy_pool_stress_worker", worker, [pool_arg; NUM_THREADS])
        });
        for retcode in retcodes {
            expect_eq!(ZX_OK, retcode);
        }

        // Drain the pool so that it is empty when dropped.
        for _ in 0..NUM_THREADS {
            let queue = {
                ksync::lock!(let mut guard = OwnedWaitQueuePoolLock::lock());
                stack_pool.borrow_locked(guard.as_mut().token_mut())
            };
            // SAFETY: `queue` was allocated by `stack_pool.grow()` and is no longer in its free
            // list.
            unsafe { free_queue(queue) };
        }
    }
}
