// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Unified deferred destruction for reference-counted kernel objects.
//!
//! Kernel threads run on small (8 KiB) stacks, so recursively destroying deep graphs of
//! reference-counted objects (such as nested `ChannelDispatcher` handles, `JobDispatcher` trees, or
//! `VmObject` clone chains) or invoking heavy destructors deep on the call stack risks stack
//! overflow.
//!
//! Designated reference-counted objects embed a single-word [`DeferredDeleteNode`]. Kernel entry
//! points install a stack-allocated [`DeferredDeleteScope`] (via [`DeferredDeleteGuard`] or
//! [`enter_raw`] / [`leave_raw`] in Rust, and the `OpaqueStorage`-backed `DeferredDeleteScope`
//! wrapper in C++) on the current [`Thread`](crate::kernel::thread::Thread). When a designated
//! object's reference count reaches zero:
//!
//! 1. If a [`DeferredDeleteScope`] is active on the current thread, the object is appended to the
//!    tail of that scope's queue in $O(1)$ time without acquiring locks or allocating memory, and
//!    actual destruction is deferred until the kernel stack unwinds back to the outermost
//!    entry-point scope.
//! 2. If no [`DeferredDeleteScope`] is installed on the current thread (for example, on standalone
//!    kernel worker threads, boot initialization, or `k ut` kernel unit tests that assert on
//!    destructor side effects immediately after dropping a `RefPtr`), a temporary scope is
//!    installed on the stack and drained synchronously in a flat iterative loop. This preserves
//!    synchronous destruction semantics outside entry points while still bounding stack usage to
//!    $O(1)$ across arbitrarily deep object chains.

use core::cell::UnsafeCell;
use core::marker::{PhantomData, PhantomPinned};
use core::mem::MaybeUninit;
use core::num::NonZeroUsize;
use core::ptr::{self, NonNull};
use zr::{static_assert, static_assert_size_and_align};

use crate::kernel::thread;

static_assert_size_and_align!(
    Option<NonNull<DeferredDeleteScope>>,
    core::mem::size_of::<*mut core::ffi::c_void>(),
    core::mem::align_of::<*mut core::ffi::c_void>()
);

/// Returns a non-null pointer to the calling thread's `recursive_object_deletion_list` slot
/// without forming a `&Thread` reference.
///
/// The slot is owned exclusively by the calling thread and remains valid while it executes.
///
/// # Safety
///
/// The caller must ensure:
///
/// * The calling CPU's current `Thread` pointer has been initialized to a valid, live
///   [`Thread`](thread::Thread) (after `construct_first_thread` / `create_idle_thread` on the
///   calling CPU).
/// * Legacy `SafeDeleter` (`zircon/kernel/object/dispatcher.cc`) is not currently occupying the
///   calling thread's `recursive_object_deletion_list` slot.
#[inline]
unsafe fn current_deferred_delete_slot() -> NonNull<Option<NonNull<DeferredDeleteScope>>> {
    let current = thread::current_get();
    debug_assert!(!current.is_null());
    // SAFETY: Caller guarantees `current` points to the calling CPU's valid, live `Thread` and
    // `SafeDeleter` is not occupying the slot. `Thread::recursive_object_deletion_list_ptr`
    // projects its non-null, `*mut c_void`-aligned `UnsafeCell<*mut c_void>` field without forming
    // `&Thread`, and `Option<NonNull<DeferredDeleteScope>>` matches `*mut c_void` in size,
    // alignment (`static_assert_size_and_align!` above), and null representation (`None` == null).
    unsafe {
        let slot = thread::Thread::recursive_object_deletion_list_ptr(current)
            .cast::<Option<NonNull<DeferredDeleteScope>>>();
        NonNull::new_unchecked(slot)
    }
}

/// Upper bound on the number of objects destroyed in a single drain pass before triggering a debug
/// assertion to diagnose a reference-counting or re-queuing bug.
///
/// Sized well above `MAX_HANDLE_COUNT` (`256 * 1024` handles in `object/handle.rs`, where closing a
/// full handle table or tearing down deep `VmObject` clone / `JobDispatcher` trees can destroy
/// multiple objects per handle) while still catching unbounded re-queuing loops promptly.
const MAX_DEFERRED_DELETIONS: usize = 16 * 1024 * 1024;

/// Low-bit mask encoding [`DeferredDeleteKind`] inside 8-byte-aligned object pointers.
const TAG_MASK: usize = 0b11;

/// Minimum alignment mask verified on every enqueued object pointer.
const ALIGN_MASK: usize = 0b111;

/// Non-null object pointer with [`DeferredDeleteKind`] packed into its low two bits.
///
/// # Safety Invariant
///
/// Every queued `TaggedEntry` (other than [`UNQUEUED`] inside an unqueued [`DeferredDeleteNode`])
/// was formed by ORing a valid active `DeferredDeleteKind` discriminant (within [`TAG_MASK`]) into
/// the low two bits of a non-null, at-least-8-byte-aligned (`addr & ALIGN_MASK == 0`) pointer to a
/// live, uniquely owned (`refcount == 0`) object of the concrete type identified by that tag.
type TaggedEntry = NonNull<u8>;

/// Sentinel stored in [`DeferredDeleteNode::next`] whenever a node is *not* linked in a scope.
///
/// Because valid queued values are either `None` (`0`, when at the tail of a scope) or an
/// 8-byte-aligned pointer ORed with a 2-bit [`DeferredDeleteKind`] (so bit 2 is always `0`),
/// `usize::MAX` (low 3 bits `0b111`) is disjoint from every valid queued value.
const UNQUEUED: Option<TaggedEntry> = Some(NonNull::without_provenance(NonZeroUsize::MAX));

/// Tag stored in the low two bits of a queued object pointer to dispatch destruction without
/// adding a vtable or function pointer to [`DeferredDeleteNode`].
#[cfg(ktest)]
#[repr(usize)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DeferredDeleteKind {
    /// Unit-test `Recyclable` object destroyed via `TestDeferredHeader::destroy`.
    TestObject = 2,
}

static_assert!((TAG_MASK & !ALIGN_MASK) == 0);
static_assert!((TAG_MASK & (1 << 2)) == 0);
static_assert!((usize::MAX & (ALIGN_MASK & !TAG_MASK)) != 0);
#[cfg(ktest)]
static_assert!((DeferredDeleteKind::TestObject as usize) <= TAG_MASK);
#[cfg(ktest)]
static_assert!(core::mem::align_of::<TestDeferredHeader>() > ALIGN_MASK);

/// Singly-linked list node embedded in reference-counted objects that participate in deferred
/// destruction.
///
/// A node must be unqueued (`UNQUEUED`, not linked in any [`DeferredDeleteScope`]) when dropped.
///
/// The C++ `DeferredDeleteNode` class in `zircon/kernel/include/kernel/deferred_deleter.h` reserves
/// `OpaqueStorage` of the same size and alignment and forwards construction and destruction to
/// [`rust_deferred_delete_node_init`] and [`rust_deferred_delete_node_destroy`].
#[repr(C)]
pub struct DeferredDeleteNode {
    next: UnsafeCell<Option<TaggedEntry>>,
    /// Makes `DeferredDeleteNode` `!Unpin` (`Unpin` is an auto trait, so `PhantomData<T>`
    /// propagates `!Unpin`) while keeping it FFI-safe (`improper_ctypes` rejects a bare
    /// `PhantomPinned`).
    _pin: PhantomData<PhantomPinned>,
}

static_assert_size_and_align!(DeferredDeleteNode, 8, 8);

impl DeferredDeleteNode {
    /// Creates a new unqueued [`DeferredDeleteNode`].
    #[inline]
    pub const fn new() -> Self {
        Self { next: UnsafeCell::new(UNQUEUED), _pin: PhantomData }
    }

    /// Marks `node` as the tail of a scope (`next = None`), returning a pointer to its `next` slot
    /// and asserting in debug builds that it was not already queued.
    ///
    /// # Safety
    ///
    /// The caller must ensure `node` points to a valid, exclusively owned [`DeferredDeleteNode`]
    /// that is currently `UNQUEUED`.
    #[inline]
    unsafe fn mark_queued_tail(node: NonNull<Self>) -> NonNull<Option<TaggedEntry>> {
        // SAFETY: Caller guarantees `node` points to a valid, exclusively owned
        // `DeferredDeleteNode`, so its `next` field pointer is in-bounds and non-null.
        let next_slot = unsafe {
            NonNull::new_unchecked(UnsafeCell::raw_get(&raw const (*node.as_ptr()).next))
        };
        // SAFETY: `next_slot` points to the initialized `next` field of `node`.
        debug_assert_eq!(
            unsafe { next_slot.read() },
            UNQUEUED,
            "DeferredDeleteNode double-enqueued for deferred deletion",
        );
        // SAFETY: `next_slot` is valid for writes.
        unsafe { next_slot.write(None) };
        next_slot
    }

    /// Removes `node` from the head of a scope by resetting `next` to `UNQUEUED` and returning the
    /// successor tagged entry (or `None` if `node` was the tail).
    ///
    /// # Safety
    ///
    /// The caller must ensure `node` points to a valid [`DeferredDeleteNode`] currently at the head
    /// of the active [`DeferredDeleteScope`].
    #[inline]
    unsafe fn take_next(node: NonNull<Self>) -> Option<TaggedEntry> {
        // SAFETY: Caller guarantees `node` is valid and exclusively owned by the draining thread.
        let next_slot = unsafe { UnsafeCell::raw_get(&raw const (*node.as_ptr()).next) };
        // SAFETY: `next_slot` is valid for reads and writes.
        let next = unsafe { ptr::replace(next_slot, UNQUEUED) };
        debug_assert_ne!(next, UNQUEUED, "popped unqueued DeferredDeleteNode");
        next
    }
}

// SAFETY: An unqueued `DeferredDeleteNode` (`UNQUEUED`) holds no thread-local state and can be
// moved or owned across threads with its enclosing reference-counted object; once queued at
// `refcount == 0`, it is exclusively owned by the enqueuing/draining thread's
// `DeferredDeleteScope` until `take_next` resets it to `UNQUEUED` prior to destruction.
unsafe impl Send for DeferredDeleteNode {}

// SAFETY: `DeferredDeleteNode` exposes no safe `&self` accessors to `next`. `next` is read or
// mutated only under exclusive ownership—either after `RefCounted::release`'s `Release`/`Acquire`
// `1 -> 0` synchronization when enqueuing and draining, or via `&mut self` in `Drop::drop`.
unsafe impl Sync for DeferredDeleteNode {}

impl Default for DeferredDeleteNode {
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for DeferredDeleteNode {
    #[inline]
    fn drop(&mut self) {
        debug_assert_eq!(
            *self.next.get_mut(),
            UNQUEUED,
            "DeferredDeleteNode dropped while still queued",
        );
    }
}

/// Stack-allocated FIFO deferred-destruction scope registered on
/// [`Thread`](crate::kernel::thread::Thread) at kernel entry points.
///
/// The C++ `DeferredDeleteScope` class in `zircon/kernel/include/kernel/deferred_deleter.h`
/// reserves `OpaqueStorage` of the same size and alignment so C++ functions can place a scope on
/// the stack, forwarding entry and exit to [`rust_deferred_delete_scope_enter`] and
/// [`rust_deferred_delete_scope_leave`].
///
/// Layout invariants:
///
/// - When installed as the outermost scope on the current thread:
///   - `head` holds `Some(entry)` for the first queued object, or `None` when the queue is empty.
///   - `tail` is `Some(slot)`: it points to `self.head` when empty, or to the tail node's `next`
///     field when non-empty, giving $O(1)$ FIFO append.
/// - When nested inside an already-active outer scope (or after being detached):
///   - Both `head` and `tail` are `None`.
#[repr(C)]
pub struct DeferredDeleteScope {
    head: UnsafeCell<Option<TaggedEntry>>,
    tail: UnsafeCell<Option<NonNull<Option<TaggedEntry>>>>,
    /// Makes `DeferredDeleteScope` `!Unpin` (`Unpin` is an auto trait, so `PhantomData<T>`
    /// propagates `!Unpin`) while keeping it FFI-safe (`improper_ctypes` rejects a bare
    /// `PhantomPinned`).
    _pin: PhantomData<PhantomPinned>,
}

static_assert_size_and_align!(DeferredDeleteScope, 16, 8);
static_assert!(!core::mem::needs_drop::<MaybeUninit<DeferredDeleteScope>>());

/// Compile-time check that `$t` does not implement `$trait`.
///
/// If `$t` implemented `$trait`, `<$t as AmbiguousIfImpl<_>>::VALID` would be ambiguous between the
/// blanket `AmbiguousIfImpl<()>` impl and `AmbiguousIfImpl<Invalid>`, failing compilation.
macro_rules! static_assert_not_impl {
    ($t:ty: $($trait:tt)+) => {
        const _: () = {
            trait AmbiguousIfImpl<A> {
                const VALID: () = ();
            }
            impl<T: ?Sized> AmbiguousIfImpl<()> for T {}
            struct Invalid;
            impl<T: ?Sized + $($trait)+> AmbiguousIfImpl<Invalid> for T {}
            let () = <$t as AmbiguousIfImpl<_>>::VALID;
        };
    };
}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    let _ = assert_send_sync::<DeferredDeleteNode>;
};
static_assert_not_impl!(DeferredDeleteNode: Unpin);
static_assert_not_impl!(DeferredDeleteScope: Unpin);
static_assert_not_impl!(DeferredDeleteScope: Send);
static_assert_not_impl!(DeferredDeleteScope: Sync);

impl DeferredDeleteScope {
    /// Returns a non-null pointer to `scope`'s `head` slot without forming a reference to `scope`.
    ///
    /// # Safety
    ///
    /// The caller must ensure `scope` points to a live (possibly uninitialized)
    /// `DeferredDeleteScope` allocation.
    #[inline]
    unsafe fn head_slot(scope: NonNull<Self>) -> NonNull<Option<TaggedEntry>> {
        // SAFETY: Caller guarantees `scope` points to a live `DeferredDeleteScope` allocation, so
        // its `head` field pointer is in-bounds and non-null.
        unsafe { NonNull::new_unchecked(UnsafeCell::raw_get(&raw const (*scope.as_ptr()).head)) }
    }

    /// Returns a non-null pointer to `scope`'s `tail` slot without forming a reference to `scope`.
    ///
    /// # Safety
    ///
    /// The caller must ensure `scope` points to a live (possibly uninitialized)
    /// `DeferredDeleteScope` allocation.
    #[inline]
    unsafe fn tail_slot(scope: NonNull<Self>) -> NonNull<Option<NonNull<Option<TaggedEntry>>>> {
        // SAFETY: Caller guarantees `scope` points to a live `DeferredDeleteScope` allocation, so
        // its `tail` field pointer is in-bounds and non-null.
        unsafe { NonNull::new_unchecked(UnsafeCell::raw_get(&raw const (*scope.as_ptr()).tail)) }
    }
}

/// RAII guard over a caller-allocated `MaybeUninit<DeferredDeleteScope>` that calls [`leave_raw`]
/// when dropped without holding a live `&mut` reference across the guard's lifetime or [`Drop`].
///
/// `PhantomData<&'a mut MaybeUninit<DeferredDeleteScope>>` adds zero size and emits no `&mut`
/// retag or function-entry protector in [`Drop::drop`], so transitive destructors invoked during
/// [`leave_raw`] may soundly load the scope pointer from `Thread::recursive_object_deletion_list`
/// and mutate `head` / `tail` under Stacked Borrows and Tree Borrows while Rust's borrow checker
/// statically enforces alignment, writability, reverse drop order, and absence of aliasing `&` or
/// `&mut` references to the `MaybeUninit<DeferredDeleteScope>` storage while active.
#[must_use = "if unused, the DeferredDeleteScope is immediately left instead of staying active"]
pub struct DeferredDeleteGuard<'a>(
    NonNull<DeferredDeleteScope>,
    PhantomData<&'a mut MaybeUninit<DeferredDeleteScope>>,
);

static_assert_size_and_align!(DeferredDeleteGuard<'static>, 8, 8);
static_assert_not_impl!(DeferredDeleteGuard<'static>: Send);
static_assert_not_impl!(DeferredDeleteGuard<'static>: Sync);

impl<'a> DeferredDeleteGuard<'a> {
    /// Initializes `scope` via [`enter_raw`] and returns an RAII guard whose [`Drop`] calls
    /// [`leave_raw`].
    ///
    /// # Safety
    ///
    /// The caller must ensure:
    ///
    /// * `scope` is not already active on any thread (for example via [`enter_raw`] or
    ///   [`rust_deferred_delete_scope_enter`]).
    /// * The returned [`DeferredDeleteGuard`] is dropped (not leaked via [`core::mem::forget`] or
    ///   otherwise) before `scope` is deallocated, reused, or referenced, unless `scope` has
    ///   already been detached via `detach_empty_current` or `drain_and_detach_current`
    ///   (`DeferredDeleter::DrainAndDetachCurrent`) on a non-returning stack transition that never
    ///   subsequently drops the guard.
    /// * The calling CPU's current `Thread` pointer is initialized (after `construct_first_thread`
    ///   / `create_idle_thread`) in blockable thread context (`!blocking_disallowed()`).
    /// * Legacy `SafeDeleter` (`zircon/kernel/object/dispatcher.cc`) is not active on the calling
    ///   thread while entering or leaving `scope`, and no `Dispatcher` is recycled via
    ///   `SafeDeleter` while `scope` is installed on the calling thread.
    /// * When the returned guard is dropped: unless `scope` was nested or already detached, the
    ///   caller is on the same thread in blockable thread context (`!blocking_disallowed()`);
    ///   furthermore, if `scope` contains any queued objects, interrupts are enabled (once the
    ///   scheduler is active on the calling CPU) and no spinlocks, `ChainLockTransaction`, or locks
    ///   conflicting with queued object destructors are held.
    #[inline]
    pub unsafe fn enter(scope: &'a mut MaybeUninit<DeferredDeleteScope>) -> Self {
        let scope = NonNull::from_mut(scope).cast::<DeferredDeleteScope>();
        // SAFETY: `scope` is derived from an exclusive `&'a mut MaybeUninit<DeferredDeleteScope>`
        // (guaranteeing 8-byte alignment, writability for `size_of::<DeferredDeleteScope>()` bytes,
        // and no aliasing `&` or `&mut` references while the returned guard borrows `'a`), and the
        // caller upholds the non-reentrancy, drop-before-deallocation, initialized-`Thread`,
        // blockable-context, and `SafeDeleter` preconditions required by `enter_raw`. `Self::drop`
        // invokes `leave_raw(scope)`.
        unsafe { enter_raw(scope) };
        Self(scope, PhantomData)
    }
}

impl Drop for DeferredDeleteGuard<'_> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: `DeferredDeleteGuard::enter` initialized `self.0` on the current thread, the `'a`
        // borrow in `PhantomData` keeps the backing `MaybeUninit<DeferredDeleteScope>` live,
        // unmodified, and unaliased through completion of `drop`, and the `enter` caller guarantees
        // same-thread blockable context (when not already detached) plus interrupts enabled and no
        // conflicting locks held when queued objects are present.
        unsafe { leave_raw(self.0) };
    }
}

/// Initializes `scope` in place and installs it on the current thread if no outer scope is active.
///
/// # Safety
///
/// The caller must ensure:
///
/// * `scope` is 8-byte aligned, writable for `size_of::<DeferredDeleteScope>()` bytes, not already
///   active on the current thread (or any other thread), and remains live at that fixed stack
///   address, unmodified outside deferred-deleter operations and without any Rust reference (`&` or
///   `&mut`) to `scope` or its backing storage, until the matching [`leave_raw`] call completes
///   (or, if [`leave_raw`] is skipped on a non-returning stack transition after detaching `scope`
///   via `detach_empty_current` or `drain_and_detach_current` /
///   `DeferredDeleter::DrainAndDetachCurrent`, until that detach call completes).
/// * [`leave_raw`] is called on `scope` (not skipped or leaked) before `scope` is deallocated,
///   reused, or referenced, unless `scope` has already been detached via `detach_empty_current` or
///   `drain_and_detach_current` (`DeferredDeleter::DrainAndDetachCurrent`) on a non-returning stack
///   transition that never subsequently calls [`leave_raw`] on `scope`.
/// * The calling CPU's current `Thread` pointer is initialized (after `construct_first_thread` /
///   `create_idle_thread`) in blockable thread context (`!blocking_disallowed()`).
/// * Legacy `SafeDeleter` (`zircon/kernel/object/dispatcher.cc`) is not active on the calling
///   thread, and no `Dispatcher` is recycled via `SafeDeleter` while `scope` is installed on the
///   calling thread.
#[inline]
pub unsafe fn enter_raw(scope: NonNull<DeferredDeleteScope>) {
    debug_assert!(!crate::arch_rs::blocking_disallowed());
    // SAFETY: Caller guarantees `scope` points to a valid `DeferredDeleteScope` allocation.
    let head_slot = unsafe { DeferredDeleteScope::head_slot(scope) };
    // SAFETY: Caller guarantees `scope` points to a valid `DeferredDeleteScope` allocation.
    let tail_slot = unsafe { DeferredDeleteScope::tail_slot(scope) };
    // SAFETY: Caller guarantees `scope` is writable without conflicting accesses.
    unsafe { head_slot.write(None) };

    // SAFETY: Caller guarantees the current `Thread` pointer is initialized on this CPU and
    // `SafeDeleter` is not active.
    let thread_slot = unsafe { current_deferred_delete_slot() };
    // SAFETY: `thread_slot` points to the calling thread's initialized slot.
    if let Some(active) = unsafe { thread_slot.read() } {
        // Nested inside an already-active outer scope: mark inactive.
        core::hint::cold_path();
        debug_assert_ne!(
            active, scope,
            "DeferredDeleteScope re-entered while already active on current thread",
        );
        // SAFETY: `tail_slot` is valid for writes on `scope`.
        unsafe { tail_slot.write(None) };
    } else {
        // SAFETY: `tail_slot` and `thread_slot` are valid for writes on the current thread.
        unsafe {
            tail_slot.write(Some(head_slot));
            thread_slot.write(Some(scope));
        }
    }
}

/// Drains any objects queued on `scope` (if `scope` was installed as the outermost scope) and
/// detaches `scope` from the current thread.
///
/// # Safety
///
/// The caller must ensure:
///
/// * `scope` was initialized by [`enter_raw`] on the current thread, has not been modified outside
///   deferred-deleter operations since, and remains live at that fixed address without any aliasing
///   Rust reference (`&` or `&mut`) for the duration of this call.
/// * Unless `scope` was nested or already detached via `detach_empty_current` or
///   `drain_and_detach_current` (`DeferredDeleter::DrainAndDetachCurrent`), the caller is on the
///   same thread in blockable thread context (`!blocking_disallowed()`) and legacy `SafeDeleter` is
///   not active on the calling thread.
/// * If `scope` contains any queued objects, interrupts are enabled (once the scheduler is active
///   on the calling CPU) and no spinlocks, `ChainLockTransaction`, or locks conflicting with queued
///   object destructors are held.
#[inline]
pub unsafe fn leave_raw(scope: NonNull<DeferredDeleteScope>) {
    // SAFETY: Caller guarantees `scope` was initialized by `enter_raw` and remains live and
    // unaliased.
    let tail_slot = unsafe { DeferredDeleteScope::tail_slot(scope) };
    // SAFETY: `tail_slot` is initialized and owned by the current thread.
    if unsafe { tail_slot.read() }.is_none() {
        // Nested scope or already detached via `detach_empty_current` / `drain_and_detach_current`.
        core::hint::cold_path();
        return;
    }

    debug_assert!(!crate::arch_rs::blocking_disallowed());

    // SAFETY: `scope` is initialized and unaliased.
    let head_slot = unsafe { DeferredDeleteScope::head_slot(scope) };
    // SAFETY: `head_slot` is initialized and owned by the current thread.
    if unsafe { head_slot.read() }.is_some() {
        core::hint::cold_path();
        // SAFETY: `scope` is installed on the current thread and non-empty, and caller upholds the
        // blockable thread-context, `SafeDeleter`, interrupt, and lock preconditions.
        unsafe { drain_and_detach_installed(scope) };
        return;
    }

    // Fast path: installed and empty.
    // SAFETY: `enter_raw` installed `scope` in the current thread's slot, caller guarantees
    // same-thread blockable context without `SafeDeleter` or aliasing references, and
    // `head_slot.read()` was `None` above.
    unsafe {
        let thread_slot = current_deferred_delete_slot();
        detach_installed_empty(scope, thread_slot);
    }
}

/// Clears `scope.tail` and unlinks `scope` from `thread_slot` after asserting in debug builds that
/// `scope` is currently installed on `thread_slot` and holds no queued entries.
///
/// # Safety
///
/// The caller must ensure `scope` is currently installed as the active [`DeferredDeleteScope`] in
/// the calling thread's `thread_slot` without aliasing Rust references, and `scope` is empty
/// (`head` is `None`).
#[inline]
unsafe fn detach_installed_empty(
    scope: NonNull<DeferredDeleteScope>,
    thread_slot: NonNull<Option<NonNull<DeferredDeleteScope>>>,
) {
    // SAFETY: Caller guarantees `scope` and `thread_slot` are valid, unaliased, and owned by the
    // calling thread.
    unsafe {
        let head_slot = DeferredDeleteScope::head_slot(scope);
        let tail_slot = DeferredDeleteScope::tail_slot(scope);
        debug_assert_eq!(thread_slot.read(), Some(scope));
        debug_assert!(head_slot.read().is_none(), "detaching non-empty DeferredDeleteScope",);
        tail_slot.write(None);
        thread_slot.write(None);
    }
}

/// Drains any objects currently queued on the calling thread's active [`DeferredDeleteScope`],
/// leaving the scope installed on the thread.
///
/// Used before disabling interrupts in paths (such as `restricted_enter`) that may still abort and
/// return normally through the enclosing entry point if a thread signal is pending.
///
/// # Safety
///
/// The caller must ensure:
///
/// * The calling CPU's current `Thread` pointer is initialized in blockable thread context
///   (`!blocking_disallowed()`), legacy `SafeDeleter` is not active on the calling thread, and any
///   installed [`DeferredDeleteScope`] has no aliasing Rust references.
/// * If any objects are queued on the active scope, interrupts are enabled (once the scheduler is
///   active on the calling CPU) and no spinlocks, `ChainLockTransaction`, or locks conflicting with
///   queued object destructors are held.
#[inline]
unsafe fn drain_current() {
    debug_assert!(!crate::arch_rs::blocking_disallowed());
    // SAFETY: Caller guarantees an initialized `Thread` in blockable thread context without
    // `SafeDeleter` active.
    let thread_slot = unsafe { current_deferred_delete_slot() };
    // SAFETY: `thread_slot` belongs to the calling thread.
    let Some(scope) = (unsafe { thread_slot.read() }) else {
        return;
    };
    // SAFETY: `scope` points to the calling thread's installed `DeferredDeleteScope`.
    let head_slot = unsafe { DeferredDeleteScope::head_slot(scope) };
    // SAFETY: `head_slot` is initialized and owned by the calling thread.
    if unsafe { head_slot.read() }.is_some() {
        core::hint::cold_path();
        // SAFETY: `scope` is the calling thread's installed scope, and caller upholds the
        // interrupt and lock preconditions when queued objects are present.
        unsafe { drain_installed(scope) };
    }
}

/// Detaches the calling thread's active [`DeferredDeleteScope`] after verifying it is empty.
///
/// Called once a non-unwinding user-mode transition (such as `restricted_enter` after disabling
/// interrupts and checking pending signals) is committed to resetting the kernel stack without
/// returning to the enclosing entry point.
///
/// # Safety
///
/// The caller must ensure:
///
/// * The calling CPU's current `Thread` pointer is initialized in blockable thread context
///   (`!blocking_disallowed()`), and legacy `SafeDeleter` is not active on the calling thread.
/// * Any active [`DeferredDeleteScope`] on the calling thread is already empty and free of aliasing
///   Rust references; if the enclosing frame subsequently executes [`leave_raw`] / [`Drop`] instead
///   of abandoning the stack, `scope` must remain live, unmodified, and unaliased until that call
///   completes.
#[inline]
unsafe fn detach_empty_current() {
    debug_assert!(!crate::arch_rs::blocking_disallowed());
    // SAFETY: Caller guarantees an initialized `Thread` in blockable thread context without
    // `SafeDeleter` active.
    let thread_slot = unsafe { current_deferred_delete_slot() };
    // SAFETY: `thread_slot` belongs to the calling thread.
    let Some(scope) = (unsafe { thread_slot.read() }) else {
        return;
    };
    // SAFETY: `scope` was just read from `thread_slot` on the calling thread, and caller guarantees
    // `scope` is empty and unaliased.
    unsafe { detach_installed_empty(scope, thread_slot) };
}

/// Drains any objects queued on the calling thread and detaches any installed
/// [`DeferredDeleteScope`].
///
/// # Safety
///
/// The caller must ensure:
///
/// * The calling CPU's current `Thread` pointer is initialized in blockable thread context
///   (`!blocking_disallowed()`), legacy `SafeDeleter` is not active on the calling thread, and any
///   installed [`DeferredDeleteScope`] is free of aliasing Rust references (remaining live,
///   unmodified, and unaliased until any subsequent [`leave_raw`] / [`Drop`] call on that frame
///   completes).
/// * If any objects are queued on the active scope, interrupts are enabled (once the scheduler is
///   active on the calling CPU) and no spinlocks, `ChainLockTransaction`, or locks conflicting with
///   queued object destructors are held (interrupts may remain disabled on empty kernel-thread exit
///   paths where no objects are queued).
#[inline]
unsafe fn drain_and_detach_current() {
    debug_assert!(!crate::arch_rs::blocking_disallowed());
    // SAFETY: Caller guarantees an initialized `Thread` in blockable thread context without
    // `SafeDeleter` active.
    let thread_slot = unsafe { current_deferred_delete_slot() };
    // SAFETY: `thread_slot` belongs to the calling thread.
    let Some(scope) = (unsafe { thread_slot.read() }) else {
        return;
    };
    // SAFETY: `scope` points to the calling thread's installed `DeferredDeleteScope`.
    let head_slot = unsafe { DeferredDeleteScope::head_slot(scope) };
    // SAFETY: `head_slot` is initialized and owned by the calling thread.
    if unsafe { head_slot.read() }.is_some() {
        core::hint::cold_path();
        // SAFETY: `scope` is installed on the calling thread, and caller upholds the interrupt and
        // lock preconditions when queued objects are present.
        unsafe { drain_installed(scope) };
    }
    // SAFETY: `scope` is installed in `thread_slot` without aliasing references and `head_slot` is
    // now `None` (either initially or after `drain_installed`).
    unsafe { detach_installed_empty(scope, thread_slot) };
}

/// Tags `ptr` with `kind` in its low 2 bits while preserving pointer provenance.
///
/// # Safety
///
/// The caller must ensure:
///
/// * `ptr` is at least 8-byte aligned (`(ptr.addr().get() & ALIGN_MASK) == 0`).
/// * `ptr` points to a live, uniquely owned (`refcount == 0`) object of the concrete type
///   identified by `kind` (for [`DeferredDeleteKind::TestObject`], an allocation with an
///   initialized [`TestDeferredHeader`] at offset 0 upholding its `destroy` safety invariant).
#[cfg(ktest)]
#[inline]
unsafe fn tag_ptr(ptr: NonNull<u8>, kind: DeferredDeleteKind) -> TaggedEntry {
    debug_assert_eq!(
        ptr.addr().get() & ALIGN_MASK,
        0,
        "deferred delete object must be 8-byte aligned",
    );
    debug_assert!((kind as usize) <= TAG_MASK, "DeferredDeleteKind exceeds TAG_MASK");
    ptr.map_addr(|addr| addr | (kind as usize))
}

/// Extracts `(untagged_ptr, raw_tag)` from `entry` while preserving pointer provenance.
///
/// # Safety
///
/// The caller must ensure `entry` was constructed from an 8-byte-aligned non-null pointer ORed with
/// tag bits within [`TAG_MASK`], so clearing [`TAG_MASK`] leaves a non-zero address.
#[inline]
unsafe fn untag_ptr(entry: TaggedEntry) -> (NonNull<u8>, usize) {
    let tag = entry.addr().get() & TAG_MASK;
    // SAFETY: Caller guarantees `entry` is an 8-byte-aligned non-null pointer ORed with low tag
    // bits inside `TAG_MASK`, so clearing `TAG_MASK` produces a non-zero address.
    let ptr = unsafe { entry.map_addr(|addr| NonZeroUsize::new_unchecked(addr.get() & !TAG_MASK)) };
    (ptr, tag)
}

/// Appends `(entry, node)` to the tail of `scope` in $O(1)$ time, transferring ownership of the
/// object and `node` to `scope`.
///
/// # Safety
///
/// The caller must ensure:
///
/// * `scope` is a valid, installed [`DeferredDeleteScope`] on the calling thread (`tail` is
///   `Some(...)`) without aliasing Rust references.
/// * `entry` upholds the [`TaggedEntry`] safety invariant (`refcount == 0`) with embedded `node`,
///   and `node` is currently `UNQUEUED`.
/// * Ownership of the object referenced by `entry` and its embedded `node` transfers to `scope`:
///   the caller must never access, move, drop, deallocate, or form references (`&` or `&mut`) to
///   the object or `node` after calling `append_to_scope`.
#[inline]
unsafe fn append_to_scope(
    scope: NonNull<DeferredDeleteScope>,
    entry: TaggedEntry,
    node: NonNull<DeferredDeleteNode>,
) {
    // SAFETY: Caller guarantees `node` is valid, exclusively owned, and unqueued.
    let new_tail = unsafe { DeferredDeleteNode::mark_queued_tail(node) };
    // SAFETY: Caller guarantees `scope` is valid and installed on the calling thread, so
    // `tail_slot` holds `Some(prev_next_slot)` pointing either to `scope.head` or to the previous
    // tail node's `next` field.
    unsafe {
        let tail_slot = DeferredDeleteScope::tail_slot(scope);
        let tail = tail_slot.read();
        debug_assert!(tail.is_some());
        let prev_next_slot = tail.unwrap_unchecked();
        prev_next_slot.write(Some(entry));
        tail_slot.write(Some(new_tail));
    }
}

/// Enqueues `(entry, node)` onto the current thread's [`DeferredDeleteScope`], or drains it
/// synchronously via an iterative stack-local scope if no entry-point scope is active.
///
/// # Safety
///
/// The caller must ensure:
///
/// * `entry` upholds the [`TaggedEntry`] safety invariant (`refcount == 0`) with embedded
///   [`DeferredDeleteNode`] `node` (`UNQUEUED`), and ownership of the object and `node` transfers
///   to the deferred deletion queue: the caller must never access, move, drop, deallocate, or form
///   references (`&` or `&mut`) to the object or `node` after calling `enqueue_entry`.
/// * The calling CPU's current `Thread` pointer is initialized (after `construct_first_thread` /
///   `create_idle_thread`) in blockable thread context (`!blocking_disallowed()`), legacy
///   `SafeDeleter` is not active on the calling thread, and any installed [`DeferredDeleteScope`]
///   has no aliasing Rust references.
/// * If no [`DeferredDeleteScope`] is installed on the calling thread, interrupts are enabled (once
///   the scheduler is active on the calling CPU) and no spinlocks, `ChainLockTransaction`, or locks
///   conflicting with `entry`'s destructor are held.
#[inline]
unsafe fn enqueue_entry(entry: TaggedEntry, node: NonNull<DeferredDeleteNode>) {
    debug_assert!(
        !crate::arch_rs::blocking_disallowed(),
        "cannot enqueue deferred object destruction when blocking is disallowed",
    );
    // SAFETY: Caller guarantees an initialized `Thread` in blockable thread context without
    // `SafeDeleter` active.
    let thread_slot = unsafe { current_deferred_delete_slot() };
    // SAFETY: `thread_slot` belongs to the calling thread.
    if let Some(scope) = unsafe { thread_slot.read() } {
        // SAFETY: `scope` is installed on the calling thread without aliasing references, and
        // caller transfers unique ownership of valid `entry` and unqueued `node`.
        unsafe { append_to_scope(scope, entry, node) };
        return;
    }

    core::hint::cold_path();
    // SAFETY: `thread_slot.read()` was `None` (no outer `DeferredDeleteScope` is installed on the
    // calling thread), and caller upholds all `entry`, `node`, thread-context, `SafeDeleter`,
    // interrupt, and lock preconditions.
    unsafe { enqueue_without_scope(entry, node) };
}

/// Fallback when no entry-point [`DeferredDeleteScope`] is installed on the calling thread.
///
/// Installs a temporary [`DeferredDeleteScope`] in stack `MaybeUninit` storage so that destroying
/// `entry` and any objects transitively dropped by its destructor still executes iteratively in
/// $O(1)$ stack space while completing synchronously before returning to the caller.
///
/// # Safety
///
/// The caller must ensure:
///
/// * No [`DeferredDeleteScope`] is currently installed on the calling thread
///   (`current_deferred_delete_slot()` holds `None`), and legacy `SafeDeleter` is not active on the
///   calling thread.
/// * `entry` upholds the [`TaggedEntry`] safety invariant (`refcount == 0`) with embedded
///   [`DeferredDeleteNode`] `node` (`UNQUEUED`), and ownership of the object and `node` transfers
///   to this function: the caller must never access, move, drop, deallocate, or form references
///   (`&` or `&mut`) to the object or `node` after calling `enqueue_without_scope`.
/// * The calling CPU's current `Thread` pointer is initialized (after `construct_first_thread` /
///   `create_idle_thread`) in blockable thread context (`!blocking_disallowed()`), interrupts are
///   enabled (once the scheduler is active on the calling CPU), and no spinlocks,
///   `ChainLockTransaction`, or locks conflicting with `entry`'s destructor are held.
#[cold]
#[inline(never)]
unsafe fn enqueue_without_scope(entry: TaggedEntry, node: NonNull<DeferredDeleteNode>) {
    debug_assert!(
        // SAFETY: Caller guarantees an initialized `Thread` without `SafeDeleter` active.
        unsafe { current_deferred_delete_slot().read() }.is_none(),
        "enqueue_without_scope called while a DeferredDeleteScope is already installed",
    );
    let mut scope = MaybeUninit::<DeferredDeleteScope>::uninit();
    let scope_ptr = NonNull::from_mut(&mut scope).cast::<DeferredDeleteScope>();
    // SAFETY: `scope_ptr` is aligned, writable for `size_of::<DeferredDeleteScope>()` bytes, not
    // already active on any thread, and stays live on this stack frame without any `&` or `&mut`
    // reference to `scope` after `scope_ptr` is formed until the matching `leave_raw` call below
    // completes; caller guarantees an initialized `Thread` in blockable thread context without
    // `SafeDeleter` active.
    unsafe { enter_raw(scope_ptr) };
    // SAFETY: Caller guarantees no outer scope was installed on the calling thread, so `enter_raw`
    // installed `scope_ptr` as the active scope, and caller transfers unique ownership of valid
    // `entry` and unqueued `node`.
    unsafe { append_to_scope(scope_ptr, entry, node) };
    // SAFETY: `scope_ptr` was initialized by `enter_raw` above without aliasing references, and
    // caller guarantees blockable thread context, interrupts enabled (once scheduler is active),
    // and no spinlocks, `ChainLockTransaction`, or conflicting locks held.
    unsafe { leave_raw(scope_ptr) };
}

/// Drains all queued items on `scope` and unlinks `scope` from the current thread.
///
/// # Safety
///
/// The caller must ensure `scope` is currently installed as the outermost [`DeferredDeleteScope`]
/// on the calling thread without aliasing Rust references or active `SafeDeleter`, in blockable
/// thread context (`!blocking_disallowed()`), with interrupts enabled (once the scheduler is active
/// on the calling CPU), and with no spinlocks, `ChainLockTransaction`, or locks conflicting with
/// queued object destructors held.
#[cold]
#[inline(never)]
unsafe fn drain_and_detach_installed(scope: NonNull<DeferredDeleteScope>) {
    // SAFETY: Caller guarantees `scope` is installed on the calling thread in blockable thread
    // context without `SafeDeleter` active, with interrupts enabled (once scheduler is active) and
    // no spinlocks, `ChainLockTransaction`, or conflicting locks held.
    unsafe {
        drain_installed(scope);
        let thread_slot = current_deferred_delete_slot();
        detach_installed_empty(scope, thread_slot);
    }
}

/// Iteratively pops and destroys every entry on `scope` until empty.
///
/// `scope` remains registered on the calling thread throughout the loop so any objects dropped
/// inside destructors are appended to the tail of `scope` and destroyed in subsequent iterations.
///
/// # Safety
///
/// The caller must ensure `scope` is installed on the calling thread without aliasing Rust
/// references, in blockable thread context (`!blocking_disallowed()`), with interrupts enabled
/// (once the scheduler is active on the calling CPU), and with no spinlocks,
/// `ChainLockTransaction`, or locks conflicting with queued object destructors held.
#[cold]
#[inline(never)]
unsafe fn drain_installed(scope: NonNull<DeferredDeleteScope>) {
    debug_assert!(
        !crate::arch_rs::blocking_disallowed(),
        "cannot drain deferred object deletions when blocking is disallowed",
    );
    debug_assert_eq!(
        crate::arch_rs::ops::num_spinlocks_held(),
        0,
        "cannot drain deferred object deletions while holding a spinlock",
    );
    crate::kconcurrent::debug_assert_no_transaction_active();
    debug_assert!(
        !crate::arch_rs::ints_disabled()
            || !crate::kernel::scheduler::peek_is_active(crate::arch_rs::curr_cpu_num()),
        "cannot drain deferred object deletions with interrupts disabled once scheduler is active",
    );

    // SAFETY: `scope` is valid and exclusively accessed by the calling thread.
    let head_slot = unsafe { DeferredDeleteScope::head_slot(scope) };
    // SAFETY: `scope` is valid and exclusively accessed by the calling thread.
    let tail_slot = unsafe { DeferredDeleteScope::tail_slot(scope) };

    let mut destroyed_count: usize = 0;
    // SAFETY: `head_slot` is initialized and owned by the calling thread throughout the loop.
    while let Some(entry) = unsafe { head_slot.read() } {
        destroyed_count += 1;
        debug_assert!(
            destroyed_count <= MAX_DEFERRED_DELETIONS,
            "deferred deletion exceeded {} iterations; possible reference cycle",
            MAX_DEFERRED_DELETIONS,
        );

        // Unlink `entry` from `scope` *before* running its destructor so `scope` is completely
        // consistent if the destructor enqueues additional objects.
        // SAFETY: `entry` was placed on `scope` by `append_to_scope` and upholds the `TaggedEntry`
        // safety invariant.
        let node = unsafe { entry_delete_node(entry) };
        // SAFETY: `node` is the head node currently in `scope`.
        let next = unsafe { DeferredDeleteNode::take_next(node) };
        // SAFETY: `head_slot` and `tail_slot` are owned by the calling thread.
        unsafe {
            head_slot.write(next);
            if next.is_none() {
                tail_slot.write(Some(head_slot));
            }
        }

        // SAFETY: `entry` upholds the `TaggedEntry` safety invariant, its `node` was unlinked
        // above, and caller upholds the destructor execution-context and lock preconditions.
        unsafe { destroy_entry(entry) };
    }
}

/// Returns the embedded [`DeferredDeleteNode`] pointer for a tagged queue `entry`.
///
/// # Safety
///
/// The caller must ensure `entry` upholds the [`TaggedEntry`] safety invariant: it is an
/// 8-byte-aligned non-null pointer to a live, uniquely owned participating object ORed with the
/// valid active `DeferredDeleteKind` discriminant (within [`TAG_MASK`]) for that pointee type.
#[inline]
unsafe fn entry_delete_node(entry: TaggedEntry) -> NonNull<DeferredDeleteNode> {
    // SAFETY: Caller guarantees `entry` is an 8-byte-aligned non-null pointer ORed with low tag
    // bits inside `TAG_MASK`.
    let (ptr, tag) = unsafe { untag_ptr(entry) };
    #[cfg(not(ktest))]
    let _ = ptr;
    match tag {
        #[cfg(ktest)]
        x if x == DeferredDeleteKind::TestObject as usize => {
            // SAFETY: Caller guarantees `entry` upholds `TaggedEntry`, so `TestObject` identifies
            // `ptr` as a valid, aligned `NonNull<TestDeferredHeader>` whose `delete_node` field is
            // in-bounds and non-null.
            unsafe {
                NonNull::new_unchecked(
                    &raw mut (*ptr.cast::<TestDeferredHeader>().as_ptr()).delete_node,
                )
            }
        }
        _ => {
            debug_assert!(false, "invalid DeferredDeleteKind tag: {tag}");
            // SAFETY: Caller guarantees `entry` carries a valid active `DeferredDeleteKind`
            // discriminant, and every active variant is matched above.
            unsafe { core::hint::unreachable_unchecked() }
        }
    }
}

/// Destroys and frees the object referenced by a tagged queue `entry`.
///
/// # Safety
///
/// The caller must ensure:
///
/// * `entry` upholds the [`TaggedEntry`] safety invariant: it is an 8-byte-aligned non-null pointer
///   to a live, uniquely owned (`refcount == 0`) participating object ORed with the valid active
///   `DeferredDeleteKind` discriminant (within [`TAG_MASK`]) for that pointee type.
/// * The object's embedded [`DeferredDeleteNode`] has already been unlinked via
///   [`DeferredDeleteNode::take_next`], and the caller satisfies the execution-context and lock
///   preconditions required by the object's destructor.
#[inline]
unsafe fn destroy_entry(entry: TaggedEntry) {
    // SAFETY: Caller guarantees `entry` is an 8-byte-aligned non-null pointer ORed with low tag
    // bits inside `TAG_MASK`.
    let (ptr, tag) = unsafe { untag_ptr(entry) };
    #[cfg(not(ktest))]
    let _ = ptr;
    match tag {
        #[cfg(ktest)]
        x if x == DeferredDeleteKind::TestObject as usize => {
            // SAFETY: Caller guarantees `entry` upholds `TaggedEntry`, so `TestObject` identifies
            // `ptr` as a uniquely owned, aligned `NonNull<TestDeferredHeader>` at offset 0 of its
            // allocation whose `delete_node` has been unlinked.
            let destroy = unsafe { (*ptr.cast::<TestDeferredHeader>().as_ptr()).destroy };
            // SAFETY: `TestDeferredHeader::destroy`'s safety invariant guarantees `destroy(ptr)` is
            // sound when called once on the uniquely owned untagged `TestDeferredHeader` pointer
            // after unlinking `delete_node`.
            unsafe { destroy(ptr) };
        }
        _ => {
            debug_assert!(false, "invalid DeferredDeleteKind tag: {tag}");
            // SAFETY: Caller guarantees `entry` carries a valid active `DeferredDeleteKind`
            // discriminant, and every active variant is matched above.
            unsafe { core::hint::unreachable_unchecked() }
        }
    }
}

// ============================================================================
// C++ FFI Entry Points
// ============================================================================

/// Initializes a C++ `DeferredDeleteNode`'s `OpaqueStorage` in place in the unqueued state.
#[unsafe(no_mangle)]
pub extern "C" fn rust_deferred_delete_node_init(storage: &mut MaybeUninit<DeferredDeleteNode>) {
    storage.write(DeferredDeleteNode::new());
}

/// Destroys a C++ `DeferredDeleteNode`'s `OpaqueStorage` in place, asserting in debug builds that
/// it is not still queued.
///
/// # Safety
///
/// The caller must ensure:
///
/// * `storage` holds an initialized [`DeferredDeleteNode`] that is currently unqueued (`UNQUEUED`,
///   not linked in any [`DeferredDeleteScope`], with no live scope `head`, `next`, or `tail`
///   pointers referencing `storage`).
/// * `storage` will not be dropped or accessed as initialized again (unless re-initialized via
///   [`rust_deferred_delete_node_init`]).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_deferred_delete_node_destroy(
    storage: &mut MaybeUninit<DeferredDeleteNode>,
) {
    // SAFETY: Caller guarantees `storage` contains an initialized, unqueued `DeferredDeleteNode`
    // (with no live `DeferredDeleteScope` pointers referencing `storage`) that will not be dropped
    // or accessed as initialized again.
    unsafe { storage.assume_init_drop() };
}

/// Initializes and enters a C++ `DeferredDeleteScope`'s `OpaqueStorage`.
///
/// # Safety
///
/// The caller must ensure:
///
/// * `scope` is 8-byte aligned, writable for `size_of::<DeferredDeleteScope>()` bytes, not already
///   active on the current thread (or any other thread), and remains live at that fixed stack
///   address, unmodified outside deferred-deleter operations and without any aliasing Rust
///   reference (`&` or `&mut`), until the matching [`rust_deferred_delete_scope_leave`] call
///   completes (or, if [`rust_deferred_delete_scope_leave`] is skipped on a non-returning stack
///   transition after detaching `scope` via `detach_empty_current` or
///   `DeferredDeleter::DrainAndDetachCurrent`, until that detach call completes).
/// * [`rust_deferred_delete_scope_leave`] is called on `scope` (not skipped or leaked) before
///   `scope` is deallocated, reused, or referenced, unless `scope` has already been detached via
///   `detach_empty_current` or `DeferredDeleter::DrainAndDetachCurrent` on a non-returning stack
///   transition that never subsequently calls [`rust_deferred_delete_scope_leave`] on `scope`.
/// * The calling CPU's current `Thread` pointer is initialized (after `construct_first_thread` /
///   `create_idle_thread`) in blockable thread context (`!blocking_disallowed()`).
/// * Legacy `SafeDeleter` (`zircon/kernel/object/dispatcher.cc`) is not active on the calling
///   thread, and no `Dispatcher` is recycled via `SafeDeleter` while `scope` is installed on the
///   calling thread.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_deferred_delete_scope_enter(scope: NonNull<DeferredDeleteScope>) {
    // SAFETY: Caller upholds all alignment, non-reentrancy, stack-lifetime,
    // leave-before-deallocation, non-aliasing, initialized-`Thread`, blockable-context, and
    // `SafeDeleter` preconditions required by `enter_raw`.
    unsafe { enter_raw(scope) };
}

/// Drains (if outermost and non-empty) and leaves a C++ `DeferredDeleteScope`'s `OpaqueStorage`.
///
/// # Safety
///
/// The caller must ensure:
///
/// * `scope` was initialized by [`rust_deferred_delete_scope_enter`] on the current thread, has not
///   been modified outside deferred-deleter operations since, and remains live at that fixed
///   address without any aliasing Rust reference (`&` or `&mut`) for the duration of this call.
/// * Unless `scope` was nested or already detached via `detach_empty_current` or
///   `DeferredDeleter::DrainAndDetachCurrent`, the caller is on the same thread in blockable thread
///   context (`!blocking_disallowed()`) and legacy `SafeDeleter` is not active on the calling
///   thread.
/// * If `scope` contains any queued objects, interrupts are enabled (once the scheduler is active
///   on the calling CPU) and no spinlocks, `ChainLockTransaction`, or locks conflicting with queued
///   object destructors are held.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_deferred_delete_scope_leave(scope: NonNull<DeferredDeleteScope>) {
    // SAFETY: Caller guarantees `scope` was initialized by `rust_deferred_delete_scope_enter` on
    // the current thread and remains live, unmodified, and unaliased, and upholds the
    // same-thread blockable-context, `SafeDeleter`, interrupt, and lock preconditions.
    unsafe { leave_raw(scope) };
}

/// Drains any pending deferred deletions on the current thread and detaches any active stack
/// scope.
///
/// # Safety
///
/// The caller must ensure:
///
/// * The calling CPU's current `Thread` pointer is initialized in blockable thread context
///   (`!blocking_disallowed()`), legacy `SafeDeleter` is not active on the calling thread, and any
///   installed [`DeferredDeleteScope`] is free of aliasing Rust references (remaining live,
///   unmodified, and unaliased until any subsequent [`rust_deferred_delete_scope_leave`] /
///   [`leave_raw`] call on that frame completes).
/// * If any objects are queued on the active scope, interrupts are enabled (once the scheduler is
///   active on the calling CPU) and no spinlocks, `ChainLockTransaction`, or locks conflicting with
///   queued object destructors are held (interrupts may remain disabled on empty kernel-thread exit
///   paths where no objects are queued).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn rust_deferred_delete_drain_and_detach_current() {
    // SAFETY: Caller upholds the initialized-thread, blockable-context, `SafeDeleter`, interrupt,
    // and lock preconditions.
    unsafe { drain_and_detach_current() };
}

/// Header placed at offset 0 of `#[cfg(ktest)]` test objects to exercise [`DeferredDeleteScope`]
/// directly from Rust unit tests.
#[cfg(ktest)]
#[repr(C, align(8))]
struct TestDeferredHeader {
    delete_node: DeferredDeleteNode,
    /// Drops and deallocates the enclosing test object once popped from a [`DeferredDeleteScope`].
    ///
    /// Safety invariant: when the enclosing object is uniquely owned (`refcount == 0`) and
    /// `delete_node` has been unlinked (`UNQUEUED`), calling `destroy(ptr)` once with the non-null
    /// untagged pointer to this `TestDeferredHeader` (at offset 0 of the allocation) is sound and
    /// drops and frees the allocation.
    destroy: unsafe fn(NonNull<u8>),
}

/// Unit tests for unified deferred object destruction.
#[cfg(ktest)]
#[unittest::suite(name = "deferred_deleter")]
mod tests {
    use super::{
        DeferredDeleteGuard, DeferredDeleteKind, DeferredDeleteNode, DeferredDeleteScope,
        TestDeferredHeader, current_deferred_delete_slot, detach_empty_current,
        drain_and_detach_current, drain_current, enqueue_entry,
        rust_deferred_delete_drain_and_detach_current, rust_deferred_delete_node_destroy,
        rust_deferred_delete_node_init, rust_deferred_delete_scope_enter,
        rust_deferred_delete_scope_leave, tag_ptr,
    };
    use core::mem::MaybeUninit;
    use core::ptr::NonNull;
    use core::sync::atomic::{AtomicUsize, Ordering};
    use fbl::{HasRefCount, Recyclable, RefCounted, RefPtr};
    use kalloc::{AllocError, Box};
    use unittest::{assert_true, expect_eq, expect_false, expect_true};

    /// Fixed-capacity destruction order recorder used in unit tests without heap allocation.
    struct OrderLog {
        ids: [usize; 4],
        len: usize,
    }

    impl OrderLog {
        /// Creates an empty `OrderLog`.
        const fn new() -> Self {
            Self { ids: [0; 4], len: 0 }
        }

        /// Appends `id` if capacity remains.
        fn push(&mut self, id: usize) {
            if self.len < self.ids.len() {
                self.ids[self.len] = id;
                self.len += 1;
            }
        }
    }

    /// Returns `true` if the calling thread currently has an installed `DeferredDeleteScope`.
    fn has_active_scope() -> bool {
        // SAFETY: Kernel unit tests run on an initialized `Thread` in blockable thread context
        // without `SafeDeleter` active.
        unsafe { current_deferred_delete_slot().read().is_some() }
    }

    /// Reference-counted test node that records its destruction order, optionally holds a child
    /// `RefPtr<ChainNode>` to form deep graphs, and allocates a 512-byte stack buffer inside
    /// `Drop` to verify $O(1)$ stack usage.
    #[repr(C, align(8))]
    struct ChainNode {
        header: TestDeferredHeader,
        ref_count: RefCounted,
        id: usize,
        destroyed_counter: NonNull<AtomicUsize>,
        order_log: Option<NonNull<OrderLog>>,
        child: Option<RefPtr<ChainNode>>,
    }

    impl ChainNode {
        /// Destroys and frees a heap-allocated `ChainNode` once popped from the deferred scope.
        ///
        /// # Safety
        ///
        /// The caller must ensure `raw` was allocated via `Box::into_raw` in
        /// [`ChainNode::allocate`].
        unsafe fn destroy_raw(raw: NonNull<u8>) {
            // SAFETY: `raw` is a `Box<ChainNode>` whose `header` is at offset 0.
            let boxed = unsafe { Box::from_raw(raw.cast::<Self>().as_ptr()) };
            drop(boxed);
        }

        /// Allocates a new `RefPtr<ChainNode>`.
        fn new_ref(
            id: usize,
            destroyed_counter: &AtomicUsize,
            order_log: Option<NonNull<OrderLog>>,
            child: Option<RefPtr<ChainNode>>,
        ) -> RefPtr<Self> {
            // SAFETY: `ref_count` is freshly constructed by `RefCounted::new()` and not yet
            // adopted.
            unsafe {
                RefPtr::try_new(Self {
                    header: TestDeferredHeader {
                        delete_node: DeferredDeleteNode::new(),
                        destroy: Self::destroy_raw,
                    },
                    ref_count: RefCounted::new(),
                    id,
                    destroyed_counter: NonNull::from(destroyed_counter),
                    order_log,
                    child,
                })
            }
            .expect("ChainNode allocation should succeed")
        }

        /// Consumes `self` on stack-heavy frame so recursive destruction of a deep chain would
        /// deterministically overflow the 8 KiB kernel stack.
        #[inline(never)]
        fn burn_stack_and_record(&mut self) {
            let mut stack_frame = [0xA5u8; 512];
            stack_frame[0] = self.id as u8;
            core::hint::black_box(&mut stack_frame);

            // SAFETY: `destroyed_counter` outlives all nodes created in the enclosing test.
            unsafe { self.destroyed_counter.as_ref() }.fetch_add(1, Ordering::Relaxed);
            if let Some(mut log_ptr) = self.order_log {
                // SAFETY: Single-threaded test owns `order_log` and no aliasing reference is held
                // while draining.
                let log = unsafe { log_ptr.as_mut() };
                log.push(self.id);
            }
        }
    }

    impl Drop for ChainNode {
        fn drop(&mut self) {
            self.burn_stack_and_record();
            // Dropping `self.child` here triggers `ChainNode::recycle` for the next node!
        }
    }

    impl HasRefCount for ChainNode {
        fn ref_count(&self) -> &RefCounted {
            &self.ref_count
        }
    }

    // SAFETY: `allocate` returns heap storage via `Box::try_new`, and `recycle` transfers
    // ownership to `DeferredDeleteScope` which invokes `ChainNode::destroy_raw` (`Box::from_raw`).
    unsafe impl Recyclable for ChainNode {
        fn allocate(value: Self) -> Result<NonNull<Self>, AllocError> {
            let boxed = Box::try_new(value)?;
            Ok(NonNull::new(Box::into_raw(boxed)).expect("Box::into_raw is non-null"))
        }

        unsafe fn recycle(ptr: NonNull<Self>) {
            // SAFETY: `ChainNode` is `#[repr(C, align(8))]` with an initialized
            // `TestDeferredHeader` (`destroy: Self::destroy_raw`) at offset 0, and `recycle` holds
            // unique ownership (`refcount == 0`).
            let entry = unsafe { tag_ptr(ptr.cast::<u8>(), DeferredDeleteKind::TestObject) };
            // SAFETY: `ptr` is uniquely owned (`refcount == 0`) and `header.delete_node` is its
            // embedded node within the live `ChainNode` allocation.
            let node =
                unsafe { NonNull::new_unchecked(&raw mut (*ptr.as_ptr()).header.delete_node) };
            // SAFETY: `entry` is a valid tagged `ChainNode` pointer with embedded `node`,
            // ownership of `ptr` and `node` transfers to `enqueue_entry`, and tests run on an
            // initialized `Thread` in blockable thread context without `SafeDeleter` active, with
            // interrupts enabled and no conflicting locks held.
            unsafe {
                enqueue_entry(entry, node);
            }
        }
    }

    /// Verifies that `DeferredDeleteGuard` defers destruction until unwind in FIFO order.
    #[test]
    fn deferred_until_scope_unwinds_fifo() {
        expect_false!(has_active_scope());

        let destroyed = AtomicUsize::new(0);
        let mut order = OrderLog::new();
        let order_ptr = NonNull::from_mut(&mut order);

        {
            let mut scope = MaybeUninit::<DeferredDeleteScope>::uninit();
            // SAFETY: `_guard` is dropped normally at block exit (not leaked) on the same thread in
            // blockable context with interrupts enabled and no locks held.
            let _guard = unsafe { DeferredDeleteGuard::enter(&mut scope) };
            expect_true!(has_active_scope());

            let a = ChainNode::new_ref(1, &destroyed, Some(order_ptr), None);
            let b = ChainNode::new_ref(2, &destroyed, Some(order_ptr), None);
            let c = ChainNode::new_ref(3, &destroyed, Some(order_ptr), None);

            drop(a);
            drop(b);
            drop(c);

            // None of the destructors should have run yet while the scope is still on the stack.
            expect_eq!(destroyed.load(Ordering::Relaxed), 0);
        }

        expect_false!(has_active_scope());
        expect_eq!(destroyed.load(Ordering::Relaxed), 3);
        expect_eq!(order.len, 3);
        expect_eq!(order.ids[0], 1);
        expect_eq!(order.ids[1], 2);
        expect_eq!(order.ids[2], 3);
    }

    /// Verifies that destroying a 2,000-deep chain in a scope uses flat stack space.
    #[test]
    fn deep_chain_in_scope_does_not_overflow_stack() {
        const DEPTH: usize = 2000;
        let destroyed = AtomicUsize::new(0);

        let mut head: Option<RefPtr<ChainNode>> = None;
        for id in (0..DEPTH).rev() {
            head = Some(ChainNode::new_ref(id, &destroyed, None, head));
        }

        {
            let mut scope = MaybeUninit::<DeferredDeleteScope>::uninit();
            // SAFETY: `_guard` is dropped normally at block exit (not leaked) on the same thread in
            // blockable context with interrupts enabled and no locks held.
            let _guard = unsafe { DeferredDeleteGuard::enter(&mut scope) };
            drop(head);
            // Even the root node has not been destroyed yet!
            expect_eq!(destroyed.load(Ordering::Relaxed), 0);
        }

        expect_eq!(destroyed.load(Ordering::Relaxed), DEPTH);
        expect_false!(has_active_scope());
    }

    /// Verifies synchronous iterative fallback when no `DeferredDeleteScope` is active.
    #[test]
    fn missing_scope_falls_back_to_synchronous_iterative_drain() {
        assert_true!(!has_active_scope());

        const DEPTH: usize = 2000;
        let destroyed = AtomicUsize::new(0);

        let mut head: Option<RefPtr<ChainNode>> = None;
        for id in (0..DEPTH).rev() {
            head = Some(ChainNode::new_ref(id, &destroyed, None, head));
        }

        drop(head);
        // Synchronous fallback must have drained every node before `drop(head)` returned.
        expect_eq!(destroyed.load(Ordering::Relaxed), DEPTH);
        expect_false!(has_active_scope());
    }

    /// Verifies that nested scopes defer destruction to the outermost scope.
    #[test]
    fn nested_scopes_defer_to_outermost_scope() {
        let destroyed = AtomicUsize::new(0);
        let mut order = OrderLog::new();
        let order_ptr = NonNull::from_mut(&mut order);

        {
            let mut outer_scope = MaybeUninit::<DeferredDeleteScope>::uninit();
            // SAFETY: `_outer` is dropped normally at block exit (not leaked) on the same thread in
            // blockable context with interrupts enabled and no locks held.
            let _outer = unsafe { DeferredDeleteGuard::enter(&mut outer_scope) };
            let outer_a = ChainNode::new_ref(10, &destroyed, Some(order_ptr), None);
            drop(outer_a);

            {
                let mut inner_scope = MaybeUninit::<DeferredDeleteScope>::uninit();
                // SAFETY: `_inner` is dropped normally at block exit (not leaked) on the same
                // thread in blockable context; nested scope no-ops.
                let _inner = unsafe { DeferredDeleteGuard::enter(&mut inner_scope) };
                let inner_b = ChainNode::new_ref(20, &destroyed, Some(order_ptr), None);
                drop(inner_b);
                expect_eq!(destroyed.load(Ordering::Relaxed), 0);
            }

            // Leaving `_inner` must NOT have drained `outer_a` or `inner_b`.
            expect_eq!(destroyed.load(Ordering::Relaxed), 0);
            expect_true!(has_active_scope());

            let outer_c = ChainNode::new_ref(30, &destroyed, Some(order_ptr), None);
            drop(outer_c);
            expect_eq!(destroyed.load(Ordering::Relaxed), 0);
        }

        expect_false!(has_active_scope());
        expect_eq!(destroyed.load(Ordering::Relaxed), 3);
        expect_eq!(order.len, 3);
        expect_eq!(order.ids[0], 10);
        expect_eq!(order.ids[1], 20);
        expect_eq!(order.ids[2], 30);
    }

    /// Verifies `drain_and_detach_current` drains transitive items and detaches cleanly.
    #[test]
    fn drain_and_detach_current_drains_and_unlinks_cleanly() {
        let destroyed = AtomicUsize::new(0);

        {
            let mut scope = MaybeUninit::<DeferredDeleteScope>::uninit();
            // SAFETY: `_guard` is dropped normally at block exit (not leaked) on the same thread in
            // blockable context with interrupts enabled and no locks held.
            let _guard = unsafe { DeferredDeleteGuard::enter(&mut scope) };
            let child = ChainNode::new_ref(2, &destroyed, None, None);
            let parent = ChainNode::new_ref(1, &destroyed, None, Some(child));
            drop(parent);
            expect_eq!(destroyed.load(Ordering::Relaxed), 0);

            // SAFETY: Blockable thread context with interrupts enabled and no locks held.
            unsafe { drain_and_detach_current() };
            expect_eq!(destroyed.load(Ordering::Relaxed), 2);
            expect_false!(has_active_scope());
            // `_guard`'s `Drop` runs at block exit while `scope` remains live and unaliased, and
            // must cleanly no-op.
        }

        expect_false!(has_active_scope());

        // When no objects are queued (either with no scope installed or an empty scope),
        // `drain_and_detach_current` is valid with interrupts disabled (matching
        // `secondary_cpu_entry` calling `current_exit`).
        let no_scope_while_irq_disabled = {
            let _irqd = crate::arch_rs::InterruptDisableGuard::new();
            // SAFETY: Blockable thread context with no queued objects.
            unsafe { drain_and_detach_current() };
            has_active_scope()
        };
        expect_false!(no_scope_while_irq_disabled);

        {
            let mut empty_scope = MaybeUninit::<DeferredDeleteScope>::uninit();
            // SAFETY: `_guard` is dropped normally at block exit (not leaked) on the same thread in
            // blockable context.
            let _guard = unsafe { DeferredDeleteGuard::enter(&mut empty_scope) };
            let active_after_detach = {
                let _irqd = crate::arch_rs::InterruptDisableGuard::new();
                // SAFETY: Blockable thread context and `empty_scope` has no queued objects.
                unsafe { rust_deferred_delete_drain_and_detach_current() };
                has_active_scope()
            };
            expect_false!(active_after_detach);
        }

        expect_false!(has_active_scope());
    }

    /// Verifies split drain/detach helpers and C++ `OpaqueStorage` FFI entry points.
    #[test]
    fn ffi_storage_and_split_drain_detach_helpers() {
        expect_false!(has_active_scope());

        let mut node_storage = MaybeUninit::<DeferredDeleteNode>::uninit();
        rust_deferred_delete_node_init(&mut node_storage);
        // SAFETY: `rust_deferred_delete_node_init` initialized `node_storage`, and it is unqueued
        // and not used again.
        unsafe { rust_deferred_delete_node_destroy(&mut node_storage) };

        let destroyed = AtomicUsize::new(0);
        let mut scope_storage = MaybeUninit::<DeferredDeleteScope>::uninit();
        let scope_ptr = NonNull::from_mut(&mut scope_storage).cast::<DeferredDeleteScope>();
        // SAFETY: `scope_ptr` stays live, unmodified, and unaliased until the matching
        // `rust_deferred_delete_scope_leave` below completes, in blockable thread context.
        unsafe { rust_deferred_delete_scope_enter(scope_ptr) };
        expect_true!(has_active_scope());

        let a = ChainNode::new_ref(1, &destroyed, None, None);
        drop(a);
        expect_eq!(destroyed.load(Ordering::Relaxed), 0);

        // `drain_current` empties the queue while keeping `scope_ptr` installed.
        // SAFETY: Blockable thread context with interrupts enabled and no locks held.
        unsafe { drain_current() };
        expect_eq!(destroyed.load(Ordering::Relaxed), 1);
        expect_true!(has_active_scope());

        // `detach_empty_current` unlinks the now-empty scope, and `scope_leave` cleanly no-ops.
        // SAFETY: `drain_current` emptied `scope_ptr`, in blockable thread context.
        unsafe { detach_empty_current() };
        expect_false!(has_active_scope());
        // SAFETY: `scope_ptr` was initialized by `rust_deferred_delete_scope_enter` and remains
        // live, unmodified, and unaliased.
        unsafe { rust_deferred_delete_scope_leave(scope_ptr) };

        // Also verify `rust_deferred_delete_drain_and_detach_current` over an active FFI scope.
        // SAFETY: `scope_ptr` stays live, unmodified, and unaliased until the matching
        // `rust_deferred_delete_scope_leave` below completes, in blockable thread context.
        unsafe { rust_deferred_delete_scope_enter(scope_ptr) };
        let b = ChainNode::new_ref(2, &destroyed, None, None);
        drop(b);
        expect_eq!(destroyed.load(Ordering::Relaxed), 1);
        // SAFETY: Blockable thread context with interrupts enabled and no locks held.
        unsafe { rust_deferred_delete_drain_and_detach_current() };
        expect_eq!(destroyed.load(Ordering::Relaxed), 2);
        expect_false!(has_active_scope());
        // SAFETY: `scope_ptr` was initialized by `rust_deferred_delete_scope_enter` and remains
        // live, unmodified, and unaliased.
        unsafe { rust_deferred_delete_scope_leave(scope_ptr) };
    }
}
