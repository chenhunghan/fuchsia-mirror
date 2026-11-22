// Copyright 2026 The Fuchsia Authors. All rights reserved.
// Use of this source code is governed by a BSD-style license that can be
// found in the LICENSE file.

use core::cell::UnsafeCell;
use core::marker::PhantomData;
use core::mem::{ManuallyDrop, MaybeUninit};
use core::{fmt, ptr};

/// Default inline storage capacity in bytes (3 machine words, e.g. 24 bytes on 64-bit platforms).
pub const DEFAULT_INLINE_FN_SIZE: usize = 3 * core::mem::size_of::<usize>();

/// Maximum alignment supported by inline function storage (8 bytes).
const INLINE_STORAGE_ALIGN: usize = 8;

#[repr(C, align(8))]
struct AlignedStorage<const SIZE: usize> {
    bytes: UnsafeCell<[MaybeUninit<u8>; SIZE]>,
}

const _: () = assert!(core::mem::align_of::<AlignedStorage<0>>() == INLINE_STORAGE_ALIGN);

impl<const SIZE: usize> AlignedStorage<SIZE> {
    #[inline]
    const fn uninit() -> Self {
        Self { bytes: UnsafeCell::new([MaybeUninit::uninit(); SIZE]) }
    }

    #[inline]
    fn as_ptr(&self) -> *mut u8 {
        self.bytes.get().cast::<u8>()
    }
}

mod private {
    pub trait SealedThreadSafety {}
    pub trait SealedSig {}
    pub trait SealedArg {}
    pub trait SealedWithSig<Sig> {}
}

/// Marker type for closures that do not require `Send` or `Sync`.
///
/// An [`InlineFn`], [`InlineFnMut`], or [`InlineFnOnce`] parameterized with `Local` is `!Send` and
/// `!Sync`, allowing it to capture thread-local state, raw pointers, or lock guards.
pub struct Local(PhantomData<*mut ()>);

impl fmt::Debug for Local {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Local")
    }
}

/// Marker type for closures that implement `Send` (but not necessarily `Sync`).
///
/// An [`InlineFn`], [`InlineFnMut`], or [`InlineFnOnce`] parameterized with `SendOnly` is `Send`
/// and `!Sync`.
pub struct SendOnly(PhantomData<UnsafeCell<()>>);

impl fmt::Debug for SendOnly {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SendOnly")
    }
}

/// Marker type for closures that implement both `Send` and `Sync`.
///
/// An [`InlineFn`], [`InlineFnMut`], or [`InlineFnOnce`] parameterized with `SendSync` is both
/// `Send` and `Sync`.
pub struct SendSync(PhantomData<()>);

impl fmt::Debug for SendSync {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("SendSync")
    }
}

impl private::SealedThreadSafety for Local {}
impl private::SealedThreadSafety for SendOnly {}
impl private::SealedThreadSafety for SendSync {}

/// Sealed trait enforcing the required `Send` / `Sync` bounds on a closure `F` for a given
/// thread-safety marker (`Local`, `SendOnly`, or `SendSync`).
///
/// # Safety
///
/// - If `Self: Send`, any `F` for which `Self: ThreadSafety<F>` is implemented must satisfy
///   `F: Send`.
/// - If `Self: Sync`, any `F` for which `Self: ThreadSafety<F>` is implemented must satisfy
///   `F: Send + Sync`.
pub unsafe trait ThreadSafety<F>: private::SealedThreadSafety {}

// SAFETY: `Local` is `!Send` and `!Sync`, so no thread-safety bounds on `F` are required.
unsafe impl<F> ThreadSafety<F> for Local {}
// SAFETY: `SendOnly` is `Send` and `!Sync`, and this impl requires `F: Send`.
unsafe impl<F: Send> ThreadSafety<F> for SendOnly {}
// SAFETY: `SendSync` is `Send` and `Sync`, and this impl requires `F: Send + Sync`.
unsafe impl<F: Send + Sync> ThreadSafety<F> for SendSync {}

/// Trait implemented for supported function pointer signature types (e.g., `fn(A, B) -> Ret` or
/// [`Hrtb<fn(RefArg<A>, ValArg<B>) -> Ret>`]).
pub trait FnSig: private::SealedSig {}

/// Sealed argument descriptor for higher-ranked ([`Hrtb`]) inline function signatures.
///
/// Implemented for [`ValArg<T>`] (by-value / non-higher-ranked arguments, including references
/// with a fixed lifetime), [`RefArg<T>`] (shared references `for<'r> &'r T`), and [`MutArg<T>`]
/// (mutable references `for<'r> &'r mut T`).
pub trait FnArg: private::SealedArg {
    /// The concrete argument type for a given call-site lifetime `'r`.
    type Target<'r>;
}

/// By-value (non-higher-ranked) argument descriptor for [`Hrtb`] signatures (`Target<'r> = T`).
///
/// Covariant in `T`, so in contravariant function-parameter position (`Hrtb<fn(ValArg<T>)>`) it
/// is contravariant in `T`, matching `fn(T)`.
pub struct ValArg<T>(PhantomData<fn() -> T>);

impl<T> fmt::Debug for ValArg<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("ValArg")
    }
}

/// Shared-reference argument descriptor for [`Hrtb`] signatures (`Target<'r> = &'r T`).
///
/// Covariant in `T`, so in contravariant function-parameter position (`Hrtb<fn(RefArg<T>)>`) it
/// is contravariant in `T`, matching `for<'r> fn(&'r T)`.
pub struct RefArg<T: ?Sized + 'static>(PhantomData<fn() -> *const T>);

impl<T: ?Sized + 'static> fmt::Debug for RefArg<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("RefArg")
    }
}

/// Mutable-reference argument descriptor for [`Hrtb`] signatures (`Target<'r> = &'r mut T`).
///
/// Invariant in `T`, matching `for<'r> fn(&'r mut T)`.
pub struct MutArg<T: ?Sized + 'static>(PhantomData<fn(&mut T)>);

impl<T: ?Sized + 'static> fmt::Debug for MutArg<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("MutArg")
    }
}

impl<T> private::SealedArg for ValArg<T> {}
impl<T: ?Sized + 'static> private::SealedArg for RefArg<T> {}
impl<T: ?Sized + 'static> private::SealedArg for MutArg<T> {}

impl<T> FnArg for ValArg<T> {
    type Target<'r> = T;
}

impl<T: ?Sized + 'static> FnArg for RefArg<T> {
    type Target<'r> = &'r T;
}

impl<T: ?Sized + 'static> FnArg for MutArg<T> {
    type Target<'r> = &'r mut T;
}

/// Signature wrapper enabling higher-ranked reference arguments (`for<'r> &'r T` and
/// `for<'r> &'r mut T`) in [`InlineFn`], [`InlineFnMut`], and [`InlineFnOnce`].
///
/// Wrap a function pointer signature whose parameter types are [`ValArg`], [`RefArg`], or
/// [`MutArg`], e.g. `Hrtb<fn(RefArg<str>, ValArg<usize>) -> usize>`. Closures passed to `new`
/// and arguments passed to `call` use ordinary Rust references directly.
pub struct Hrtb<Sig>(PhantomData<Sig>);

impl<Sig> fmt::Debug for Hrtb<Sig> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Hrtb")
    }
}

#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct VTableOnce {
    call_once: *const (),
    drop: unsafe fn(*mut u8),
}

// SAFETY: `VTableOnce` holds only static function pointers (`call_once` and `drop`), which are
// immutable code addresses safe to share across threads.
unsafe impl Sync for VTableOnce {}

#[doc(hidden)]
#[derive(Clone, Copy, Debug)]
pub struct VTable {
    call: *const (),
    once: VTableOnce,
}

// SAFETY: `VTable` holds only static function pointers (`call`, `call_once`, and `drop`), which
// are immutable code addresses safe to share across threads.
unsafe impl Sync for VTable {}

trait HasDrop: Sync + 'static {
    fn drop_fn(&self) -> unsafe fn(*mut u8);
}

impl HasDrop for VTableOnce {
    #[inline]
    fn drop_fn(&self) -> unsafe fn(*mut u8) {
        self.drop
    }
}

impl HasDrop for VTable {
    #[inline]
    fn drop_fn(&self) -> unsafe fn(*mut u8) {
        self.once.drop
    }
}

/// Trait implemented by closures that can be invoked once with signature `Sig`.
///
/// # Safety
///
/// - `VTABLE_ONCE.call_once` must be a valid erased trampoline function pointer for `Self` and
///   `Sig` that, when transmuted back to `Sig`'s trampoline type and given a pointer to a properly
///   aligned and initialized `Self` with exclusive ownership, moves `Self` out of the pointer and
///   invokes it once.
/// - `VTABLE_ONCE.drop` must be a valid drop function for `Self` that, given a pointer to a
///   properly aligned and initialized `Self` with exclusive access, drops `Self` in place (or is a
///   no-op if `!core::mem::needs_drop::<Self>()`).
pub unsafe trait FnOnceWithSig<Sig: FnSig>: Sized + private::SealedWithSig<Sig> {
    #[doc(hidden)]
    const VTABLE_ONCE: &'static VTableOnce;
}

/// Trait implemented by closures that can be invoked mutably with signature `Sig`.
///
/// # Safety
///
/// - `VTABLE_MUT.call` must be a valid erased trampoline function pointer for `Self` and `Sig`
///   that, when transmuted back to `Sig`'s trampoline type and given a pointer to a properly
///   aligned and initialized `Self` with exclusive (`&mut Self`) access for the duration of the
///   call, invokes `Self` mutably without moving or dropping it.
/// - `VTABLE_MUT.once` must satisfy the safety requirements of [`FnOnceWithSig::VTABLE_ONCE`] for
///   `Self`.
pub unsafe trait FnMutWithSig<Sig: FnSig>: FnOnceWithSig<Sig> {
    #[doc(hidden)]
    const VTABLE_MUT: &'static VTable;
}

/// Trait implemented by closures that can be invoked by shared reference with signature `Sig`.
///
/// # Safety
///
/// - `VTABLE.call` must be a valid erased trampoline function pointer for `Self` and `Sig` that,
///   when transmuted back to `Sig`'s trampoline type and given a pointer to a properly aligned and
///   initialized `Self` with shared (`&Self`) access for the duration of the call, invokes `Self`
///   by shared reference without mutating, moving, or dropping it.
/// - `VTABLE.once` must satisfy the safety requirements of [`FnOnceWithSig::VTABLE_ONCE`] for
///   `Self`.
pub unsafe trait FnWithSig<Sig: FnSig>: FnMutWithSig<Sig> {
    #[doc(hidden)]
    const VTABLE: &'static VTable;
}

/// No-op drop function used for closure types `F` where `!core::mem::needs_drop::<F>()`.
///
/// # Safety
///
/// `_ptr` is not dereferenced, so no preconditions are required by `noop_drop` itself.
unsafe fn noop_drop(_ptr: *mut u8) {}

/// Drops an initialized instance of `F` in place at `ptr`.
///
/// # Safety
///
/// - `ptr` must be non-null, properly aligned for `F`, and valid for both reads and writes of `F`.
/// - `ptr` must point to a valid, initialized instance of `F` with exclusive access.
/// - The value at `ptr` must not be accessed or dropped again after this call returns.
unsafe fn drop_in_place_fn<F>(ptr: *mut u8) {
    // SAFETY: The caller guarantees `ptr` points to a valid, properly aligned, initialized
    // instance of `F` with exclusive access, and that it is dropped at most once.
    unsafe {
        ptr::drop_in_place(ptr.cast::<F>());
    }
}

const fn drop_fn_for<F>() -> unsafe fn(*mut u8) {
    if core::mem::needs_drop::<F>() { drop_in_place_fn::<F> } else { noop_drop }
}

/// Shared storage and lifecycle management for [`InlineFn`], [`InlineFnMut`], and [`InlineFnOnce`].
///
/// # Safety Invariants
///
/// - `storage` contains a valid, initialized instance of some closure type `F` at offset 0, with
///   `size_of::<F>() <= SIZE` and `align_of::<F>() <= INLINE_STORAGE_ALIGN`.
/// - `F` outlives `'a` (`F: 'a`).
/// - `S: ThreadSafety<F>` holds for the stored closure type `F` (so `S: Send` guarantees
///   `F: Send`, and `S: Sync` guarantees `F: Send + Sync`).
/// - `vtable.drop_fn()` is a valid drop function for the stored closure type `F` at
///   `storage.as_ptr()` (satisfying the drop contract of [`FnOnceWithSig::VTABLE_ONCE`]), and any
///   callable trampolines in `vtable` satisfy the safety invariant of the enclosing wrapper
///   ([`InlineFn`], [`InlineFnMut`], or [`InlineFnOnce`]).
/// - Operations taking `&Self` may only access the stored `F` via shared reference `&F`; mutating
///   `F` requires `&mut Self`, and moving/consuming `F` requires taking `Self` by value and
///   suppressing `RawInlineFn::drop` via `ManuallyDrop`.
struct RawInlineFn<'a, const SIZE: usize, V: HasDrop, S> {
    storage: AlignedStorage<SIZE>,
    vtable: &'static V,
    _marker: PhantomData<(&'a (), S)>,
}

// SAFETY: By the type invariants of `RawInlineFn`, `S: ThreadSafety<F>` holds for the stored
// closure `F`. When `S: Send` (`SendOnly` or `SendSync`), `ThreadSafety<F>` guarantees `F: Send`,
// and `vtable: &'static V` (`V: Sync`) is a shared reference to immutable static function pointers,
// so moving `RawInlineFn` across threads is sound.
unsafe impl<'a, const SIZE: usize, V: HasDrop, S: Send> Send for RawInlineFn<'a, SIZE, V, S> {}

// SAFETY: By the type invariants of `RawInlineFn`, `S: ThreadSafety<F>` holds for the stored
// closure `F`. When `S: Sync` (`SendSync`), `ThreadSafety<F>` guarantees `F: Send + Sync`, and
// shared references `&RawInlineFn` only ever access the stored `F` via shared reference `&F`
// (mutating `F` requires `&mut Self` and consuming `F` requires owned `Self`).
unsafe impl<'a, const SIZE: usize, V: HasDrop, S: Sync> Sync for RawInlineFn<'a, SIZE, V, S> {}

impl<'a, const SIZE: usize, V: HasDrop, S> Drop for RawInlineFn<'a, SIZE, V, S> {
    #[inline]
    fn drop(&mut self) {
        // SAFETY: By the type invariants of `RawInlineFn`, `self.storage` holds a valid,
        // initialized instance of the closure `F` matching `self.vtable.drop_fn()`, `&mut self`
        // guarantees exclusive access, and `drop` is called at most once when `RawInlineFn` is
        // destroyed.
        unsafe {
            (self.vtable.drop_fn())(self.as_ptr());
        }
    }
}

impl<'a, const SIZE: usize, V: HasDrop, S> RawInlineFn<'a, SIZE, V, S> {
    /// Stores `closure` inline with `vtable`.
    ///
    /// # Safety
    ///
    /// `vtable.drop_fn()` must be a valid drop function for `F` (satisfying the drop contract of
    /// [`FnOnceWithSig::VTABLE_ONCE`]), and any callable trampolines in `vtable` must be valid for
    /// `F` under the invariants of the enclosing wrapper.
    #[inline]
    unsafe fn new<F>(closure: F, vtable: &'static V) -> Self
    where
        F: 'a,
        S: ThreadSafety<F>,
    {
        const {
            assert!(
                core::mem::size_of::<F>() <= SIZE,
                "Closure state exceeds inline storage capacity"
            );
            assert!(
                core::mem::align_of::<F>() <= INLINE_STORAGE_ALIGN,
                "Closure alignment requirement exceeds inline storage alignment"
            );
        }

        let storage = AlignedStorage::<SIZE>::uninit();
        // SAFETY: We verified at compile time that `size_of::<F>() <= SIZE` and
        // `align_of::<F>() <= align_of::<AlignedStorage<SIZE>>()`. `storage.as_ptr()` is
        // valid for writes of `F`. `ptr::write` moves `closure` into `storage` without
        // reading uninitialized padding bytes or dropping `closure`.
        unsafe {
            ptr::write(storage.as_ptr().cast::<F>(), closure);
        }

        Self { storage, vtable, _marker: PhantomData }
    }

    #[inline]
    fn as_ptr(&self) -> *mut u8 {
        self.storage.as_ptr()
    }

    #[inline]
    fn resize<const NEW_SIZE: usize>(self) -> RawInlineFn<'a, NEW_SIZE, V, S> {
        const {
            assert!(
                NEW_SIZE >= SIZE,
                "Target inline storage capacity must be greater than or equal to source capacity"
            );
        }

        let this = ManuallyDrop::new(self);
        let storage = AlignedStorage::<NEW_SIZE>::uninit();
        // SAFETY: `this.storage` and `storage` do not overlap, both are at least `SIZE` bytes
        // (`NEW_SIZE >= SIZE`), and copying `MaybeUninit<u8>` bytes preserves the bitwise
        // representation of the stored closure while `ManuallyDrop` prevents `RawInlineFn::drop`
        // from running on `this`.
        unsafe {
            ptr::copy_nonoverlapping(
                this.as_ptr().cast::<MaybeUninit<u8>>(),
                storage.as_ptr().cast::<MaybeUninit<u8>>(),
                SIZE,
            );
        }

        RawInlineFn { storage, vtable: this.vtable, _marker: PhantomData }
    }

    /// Replaces the vtable and thread-safety marker of `self` without modifying the stored closure.
    ///
    /// # Safety
    ///
    /// - `new_vtable.drop_fn()` must be a valid drop function for the closure type `F` currently
    ///   stored in `self`, and any callable trampolines in `new_vtable` must be valid for `F`
    ///   under the invariants of the target wrapper.
    /// - `NewS: ThreadSafety<F>` must hold for the closure type `F` currently stored in `self`
    ///   (e.g., relaxing `SendSync` to `SendOnly` or `Local`, or keeping `S` unchanged).
    #[inline]
    unsafe fn map_vtable_and_safety<NewV: HasDrop, NewS>(
        self,
        new_vtable: &'static NewV,
    ) -> RawInlineFn<'a, SIZE, NewV, NewS> {
        let this = ManuallyDrop::new(self);
        // SAFETY: `this` is wrapped in `ManuallyDrop`, suppressing `RawInlineFn::drop`.
        // `this.storage` is read once to move the underlying `AlignedStorage<SIZE>` into the
        // returned `RawInlineFn`, preserving the initialized closure `F` at offset 0.
        let storage = unsafe { ptr::read(&this.storage) };

        RawInlineFn { storage, vtable: new_vtable, _marker: PhantomData }
    }
}

/// An inline-stored, partially type-erased `Fn` closure with compile-time capacity `SIZE`.
///
/// Similar to C++ `fit::inline_function`, `InlineFn` stores the callable's captured environment
/// directly inline in a fixed-size buffer of `SIZE` bytes without heap allocation. Attempting to
/// construct an `InlineFn` from a closure whose size exceeds `SIZE` or alignment exceeds 8 bytes
/// fails at compile time.
///
/// The `Sig` parameter specifies the function signature using standard Rust function pointer
/// syntax, such as `fn() -> u64` or `fn(u32, bool) -> i32`.
///
/// The `S` parameter controls thread-safety bounds:
/// - [`Local`] (default): `!Send + !Sync`, accepts any closure `F: Fn(...) + 'a`.
/// - [`SendOnly`]: `Send + !Sync`, requires `F: Send`.
/// - [`SendSync`]: `Send + Sync`, requires `F: Send + Sync`.
pub struct InlineFn<'a, Sig: FnSig, const SIZE: usize = DEFAULT_INLINE_FN_SIZE, S = Local> {
    // Safety Invariant: `raw.vtable` satisfies the safety contract of `FnWithSig<Sig>::VTABLE` for
    // the closure `F` stored in `raw.storage` (in particular, `raw.vtable.call` is the erased
    // trampoline for `F` and `Sig` that only accesses `F` by shared reference `&F` without
    // mutating, moving, or dropping it).
    raw: RawInlineFn<'a, SIZE, VTable, S>,
    // Invariant in `Sig`: `raw.vtable.call` / `raw.vtable.once.call_once` are transmuted back to
    // `Sig`'s trampoline function pointer type on call, so `Sig` must not undergo subtyping
    // coercions.
    _sig: PhantomData<fn(Sig) -> Sig>,
}

impl<'a, Sig: FnSig, const SIZE: usize, S> fmt::Debug for InlineFn<'a, Sig, SIZE, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InlineFn")
            .field("size", &SIZE)
            .field("drop", &(self.raw.vtable.once.drop as usize))
            .finish()
    }
}

impl<'a, Sig: FnSig, const SIZE: usize, S> InlineFn<'a, Sig, SIZE, S> {
    /// Wraps `closure` inline, asserting at compile time that its size is `<= SIZE` and
    /// alignment is `<= 8`.
    #[inline]
    pub fn new<F>(closure: F) -> Self
    where
        F: FnWithSig<Sig> + 'a,
        S: ThreadSafety<F>,
    {
        // SAFETY: `F::VTABLE` is the vtable constructed for `F` by `FnWithSig<Sig>`, satisfying
        // both `RawInlineFn::new` and `InlineFn`'s invariant on `raw.vtable`.
        Self { raw: unsafe { RawInlineFn::new(closure, F::VTABLE) }, _sig: PhantomData }
    }

    /// Widens the inline storage capacity to `NEW_SIZE` bytes (`NEW_SIZE >= SIZE`).
    ///
    /// Fails to compile if `NEW_SIZE < SIZE`.
    #[inline]
    pub fn resize<const NEW_SIZE: usize>(self) -> InlineFn<'a, Sig, NEW_SIZE, S> {
        InlineFn { raw: self.raw.resize::<NEW_SIZE>(), _sig: PhantomData }
    }

    /// Relaxes the thread-safety marker of this `InlineFn` to [`Local`] (`!Send + !Sync`).
    #[inline]
    pub fn into_local(self) -> InlineFn<'a, Sig, SIZE, Local> {
        let vtable = self.raw.vtable;
        // SAFETY: `vtable` is the existing valid `FnWithSig<Sig>` vtable for the stored closure
        // `F`, and `Local: ThreadSafety<F>` holds for all `F`.
        InlineFn { raw: unsafe { self.raw.map_vtable_and_safety(vtable) }, _sig: PhantomData }
    }

    /// Converts this shared `InlineFn` into a mutable [`InlineFnMut`] with the same capacity and
    /// thread-safety bounds at zero cost.
    #[inline]
    pub fn into_fn_mut(self) -> InlineFnMut<'a, Sig, SIZE, S> {
        let vtable = self.raw.vtable;
        // SAFETY: `vtable` is the existing valid `FnWithSig<Sig>` vtable for the stored closure
        // `F`. Because `vtable.call` only accesses `F` via shared reference `&F`, it is also valid
        // when invoked with exclusive `&mut F` access in `InlineFnMut`, and `S` is unchanged.
        InlineFnMut { raw: unsafe { self.raw.map_vtable_and_safety(vtable) }, _sig: PhantomData }
    }

    /// Converts this shared `InlineFn` into a single-use [`InlineFnOnce`] with the same capacity
    /// and thread-safety bounds at zero cost.
    #[inline]
    pub fn into_fn_once(self) -> InlineFnOnce<'a, Sig, SIZE, S> {
        let once_vtable = &self.raw.vtable.once;
        // SAFETY: `once_vtable` is the valid `VTableOnce` for the stored closure `F` and `Sig`,
        // satisfying `InlineFnOnce`'s invariant on `raw.vtable`, and `S` is unchanged.
        InlineFnOnce {
            raw: unsafe { self.raw.map_vtable_and_safety(once_vtable) },
            _sig: PhantomData,
        }
    }
}

impl<'a, Sig: FnSig, const SIZE: usize> InlineFn<'a, Sig, SIZE, Local> {
    /// Creates a local (`!Send + !Sync`) `InlineFn`.
    #[inline]
    pub fn new_local<F>(closure: F) -> Self
    where
        F: FnWithSig<Sig> + 'a,
    {
        Self::new(closure)
    }
}

impl<'a, Sig: FnSig, const SIZE: usize> InlineFn<'a, Sig, SIZE, SendOnly> {
    /// Creates a `Send` (`!Sync`) `InlineFn`.
    #[inline]
    pub fn new_send<F>(closure: F) -> Self
    where
        F: FnWithSig<Sig> + Send + 'a,
    {
        Self::new(closure)
    }
}

impl<'a, Sig: FnSig, const SIZE: usize> InlineFn<'a, Sig, SIZE, SendSync> {
    /// Creates a `Send + Sync` `InlineFn`.
    #[inline]
    pub fn new_send_sync<F>(closure: F) -> Self
    where
        F: FnWithSig<Sig> + Send + Sync + 'a,
    {
        Self::new(closure)
    }

    /// Relaxes the thread-safety marker from [`SendSync`] to [`SendOnly`].
    #[inline]
    pub fn into_send(self) -> InlineFn<'a, Sig, SIZE, SendOnly> {
        let vtable = self.raw.vtable;
        // SAFETY: `vtable` is the existing valid `FnWithSig<Sig>` vtable for the stored closure
        // `F`, and `SendSync: ThreadSafety<F>` implies `F: Send`, so `SendOnly: ThreadSafety<F>`
        // holds.
        InlineFn { raw: unsafe { self.raw.map_vtable_and_safety(vtable) }, _sig: PhantomData }
    }
}

impl<'a, Sig: FnSig, const SIZE: usize, S> From<InlineFn<'a, Sig, SIZE, S>>
    for InlineFnMut<'a, Sig, SIZE, S>
{
    #[inline]
    fn from(func: InlineFn<'a, Sig, SIZE, S>) -> Self {
        func.into_fn_mut()
    }
}

impl<'a, Sig: FnSig, const SIZE: usize, S> From<InlineFn<'a, Sig, SIZE, S>>
    for InlineFnOnce<'a, Sig, SIZE, S>
{
    #[inline]
    fn from(func: InlineFn<'a, Sig, SIZE, S>) -> Self {
        func.into_fn_once()
    }
}

/// An inline-stored, partially type-erased `FnMut` closure with compile-time capacity `SIZE`.
///
/// Like [`InlineFn`], `InlineFnMut` stores the callable inline without heap allocation, but
/// takes `&mut self` on invocation, allowing the wrapped closure to mutate its captured state.
pub struct InlineFnMut<'a, Sig: FnSig, const SIZE: usize = DEFAULT_INLINE_FN_SIZE, S = Local> {
    // Safety Invariant: `raw.vtable` satisfies the safety contract of
    // `FnMutWithSig<Sig>::VTABLE_MUT` (or `FnWithSig<Sig>::VTABLE`) for the closure `F` stored in
    // `raw.storage` (in particular, `raw.vtable.call` is the erased trampoline for `F` and `Sig`
    // valid to invoke when given exclusive `&mut F` access without moving or dropping `F`).
    raw: RawInlineFn<'a, SIZE, VTable, S>,
    // Invariant in `Sig`: `raw.vtable.call` / `raw.vtable.once.call_once` are transmuted back to
    // `Sig`'s trampoline function pointer type on call, so `Sig` must not undergo subtyping
    // coercions.
    _sig: PhantomData<fn(Sig) -> Sig>,
}

impl<'a, Sig: FnSig, const SIZE: usize, S> fmt::Debug for InlineFnMut<'a, Sig, SIZE, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InlineFnMut")
            .field("size", &SIZE)
            .field("drop", &(self.raw.vtable.once.drop as usize))
            .finish()
    }
}

impl<'a, Sig: FnSig, const SIZE: usize, S> InlineFnMut<'a, Sig, SIZE, S> {
    /// Wraps a mutable `closure` inline, asserting at compile time that its size is
    /// `<= SIZE` and alignment is `<= 8`.
    #[inline]
    pub fn new<F>(closure: F) -> Self
    where
        F: FnMutWithSig<Sig> + 'a,
        S: ThreadSafety<F>,
    {
        // SAFETY: `F::VTABLE_MUT` is the vtable constructed for `F` by `FnMutWithSig<Sig>`,
        // satisfying both `RawInlineFn::new` and `InlineFnMut`'s invariant on `raw.vtable`.
        Self { raw: unsafe { RawInlineFn::new(closure, F::VTABLE_MUT) }, _sig: PhantomData }
    }

    /// Widens the inline storage capacity to `NEW_SIZE` bytes (`NEW_SIZE >= SIZE`).
    ///
    /// Fails to compile if `NEW_SIZE < SIZE`.
    #[inline]
    pub fn resize<const NEW_SIZE: usize>(self) -> InlineFnMut<'a, Sig, NEW_SIZE, S> {
        InlineFnMut { raw: self.raw.resize::<NEW_SIZE>(), _sig: PhantomData }
    }

    /// Relaxes the thread-safety marker of this `InlineFnMut` to [`Local`] (`!Send + !Sync`).
    #[inline]
    pub fn into_local(self) -> InlineFnMut<'a, Sig, SIZE, Local> {
        let vtable = self.raw.vtable;
        // SAFETY: `vtable` is the existing valid vtable for the stored closure `F` and `Sig`, and
        // `Local: ThreadSafety<F>` holds for all `F`.
        InlineFnMut { raw: unsafe { self.raw.map_vtable_and_safety(vtable) }, _sig: PhantomData }
    }

    /// Converts this mutable `InlineFnMut` into a single-use [`InlineFnOnce`] with the same
    /// capacity and thread-safety bounds at zero cost.
    #[inline]
    pub fn into_fn_once(self) -> InlineFnOnce<'a, Sig, SIZE, S> {
        let once_vtable = &self.raw.vtable.once;
        // SAFETY: `once_vtable` is the valid `VTableOnce` for the stored closure `F` and `Sig`,
        // satisfying `InlineFnOnce`'s invariant on `raw.vtable`, and `S` is unchanged.
        InlineFnOnce {
            raw: unsafe { self.raw.map_vtable_and_safety(once_vtable) },
            _sig: PhantomData,
        }
    }
}

impl<'a, Sig: FnSig, const SIZE: usize> InlineFnMut<'a, Sig, SIZE, Local> {
    /// Creates a local (`!Send + !Sync`) `InlineFnMut`.
    #[inline]
    pub fn new_local<F>(closure: F) -> Self
    where
        F: FnMutWithSig<Sig> + 'a,
    {
        Self::new(closure)
    }
}

impl<'a, Sig: FnSig, const SIZE: usize> InlineFnMut<'a, Sig, SIZE, SendOnly> {
    /// Creates a `Send` (`!Sync`) `InlineFnMut`.
    #[inline]
    pub fn new_send<F>(closure: F) -> Self
    where
        F: FnMutWithSig<Sig> + Send + 'a,
    {
        Self::new(closure)
    }
}

impl<'a, Sig: FnSig, const SIZE: usize> InlineFnMut<'a, Sig, SIZE, SendSync> {
    /// Creates a `Send + Sync` `InlineFnMut`.
    #[inline]
    pub fn new_send_sync<F>(closure: F) -> Self
    where
        F: FnMutWithSig<Sig> + Send + Sync + 'a,
    {
        Self::new(closure)
    }

    /// Relaxes the thread-safety marker from [`SendSync`] to [`SendOnly`].
    #[inline]
    pub fn into_send(self) -> InlineFnMut<'a, Sig, SIZE, SendOnly> {
        let vtable = self.raw.vtable;
        // SAFETY: `vtable` is the existing valid vtable for the stored closure `F` and `Sig`, and
        // `SendSync: ThreadSafety<F>` implies `F: Send`, so `SendOnly: ThreadSafety<F>` holds.
        InlineFnMut { raw: unsafe { self.raw.map_vtable_and_safety(vtable) }, _sig: PhantomData }
    }
}

impl<'a, Sig: FnSig, const SIZE: usize, S> From<InlineFnMut<'a, Sig, SIZE, S>>
    for InlineFnOnce<'a, Sig, SIZE, S>
{
    #[inline]
    fn from(func: InlineFnMut<'a, Sig, SIZE, S>) -> Self {
        func.into_fn_once()
    }
}

/// An inline-stored, partially type-erased `FnOnce` closure with compile-time capacity `SIZE`.
///
/// Similar to C++ `fit::inline_callback`, `InlineFnOnce` stores a single-use callable inline
/// without heap allocation and consumes `self` upon invocation.
pub struct InlineFnOnce<'a, Sig: FnSig, const SIZE: usize = DEFAULT_INLINE_FN_SIZE, S = Local> {
    // Safety Invariant: `raw.vtable` satisfies the safety contract of
    // `FnOnceWithSig<Sig>::VTABLE_ONCE` for the closure `F` stored in `raw.storage` (in
    // particular, `raw.vtable.call_once` is the erased trampoline for `F` and `Sig` valid to
    // invoke once when moving/consuming `F` with exclusive ownership).
    raw: RawInlineFn<'a, SIZE, VTableOnce, S>,
    // Invariant in `Sig`: `raw.vtable.call_once` is transmuted back to `Sig`'s trampoline
    // function pointer type on call, so `Sig` must not undergo subtyping coercions.
    _sig: PhantomData<fn(Sig) -> Sig>,
}

impl<'a, Sig: FnSig, const SIZE: usize, S> fmt::Debug for InlineFnOnce<'a, Sig, SIZE, S> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("InlineFnOnce")
            .field("size", &SIZE)
            .field("drop", &(self.raw.vtable.drop as usize))
            .finish()
    }
}

impl<'a, Sig: FnSig, const SIZE: usize, S> InlineFnOnce<'a, Sig, SIZE, S> {
    /// Wraps a single-use `closure` inline, asserting at compile time that its size is
    /// `<= SIZE` and alignment is `<= 8`.
    #[inline]
    pub fn new<F>(closure: F) -> Self
    where
        F: FnOnceWithSig<Sig> + 'a,
        S: ThreadSafety<F>,
    {
        // SAFETY: `F::VTABLE_ONCE` is the vtable constructed for `F` by `FnOnceWithSig<Sig>`,
        // satisfying both `RawInlineFn::new` and `InlineFnOnce`'s invariant on `raw.vtable`.
        Self { raw: unsafe { RawInlineFn::new(closure, F::VTABLE_ONCE) }, _sig: PhantomData }
    }

    /// Widens the inline storage capacity to `NEW_SIZE` bytes (`NEW_SIZE >= SIZE`).
    ///
    /// Fails to compile if `NEW_SIZE < SIZE`.
    #[inline]
    pub fn resize<const NEW_SIZE: usize>(self) -> InlineFnOnce<'a, Sig, NEW_SIZE, S> {
        InlineFnOnce { raw: self.raw.resize::<NEW_SIZE>(), _sig: PhantomData }
    }

    /// Relaxes the thread-safety marker of this `InlineFnOnce` to [`Local`] (`!Send + !Sync`).
    #[inline]
    pub fn into_local(self) -> InlineFnOnce<'a, Sig, SIZE, Local> {
        let vtable = self.raw.vtable;
        // SAFETY: `vtable` is the existing valid `VTableOnce` for the stored closure `F` and
        // `Sig`, and `Local: ThreadSafety<F>` holds for all `F`.
        InlineFnOnce { raw: unsafe { self.raw.map_vtable_and_safety(vtable) }, _sig: PhantomData }
    }
}

impl<'a, Sig: FnSig, const SIZE: usize> InlineFnOnce<'a, Sig, SIZE, Local> {
    /// Creates a local (`!Send + !Sync`) `InlineFnOnce`.
    #[inline]
    pub fn new_local<F>(closure: F) -> Self
    where
        F: FnOnceWithSig<Sig> + 'a,
    {
        Self::new(closure)
    }
}

impl<'a, Sig: FnSig, const SIZE: usize> InlineFnOnce<'a, Sig, SIZE, SendOnly> {
    /// Creates a `Send` (`!Sync`) `InlineFnOnce`.
    #[inline]
    pub fn new_send<F>(closure: F) -> Self
    where
        F: FnOnceWithSig<Sig> + Send + 'a,
    {
        Self::new(closure)
    }
}

impl<'a, Sig: FnSig, const SIZE: usize> InlineFnOnce<'a, Sig, SIZE, SendSync> {
    /// Creates a `Send + Sync` `InlineFnOnce`.
    #[inline]
    pub fn new_send_sync<F>(closure: F) -> Self
    where
        F: FnOnceWithSig<Sig> + Send + Sync + 'a,
    {
        Self::new(closure)
    }

    /// Relaxes the thread-safety marker from [`SendSync`] to [`SendOnly`].
    #[inline]
    pub fn into_send(self) -> InlineFnOnce<'a, Sig, SIZE, SendOnly> {
        let vtable = self.raw.vtable;
        // SAFETY: `vtable` is the existing valid `VTableOnce` for the stored closure `F` and `Sig`,
        // and `SendSync: ThreadSafety<F>` implies `F: Send`, so `SendOnly: ThreadSafety<F>` holds.
        InlineFnOnce { raw: unsafe { self.raw.map_vtable_and_safety(vtable) }, _sig: PhantomData }
    }
}

/// Type alias for an [`InlineFn`] that is `Send` (but `!Sync`).
pub type SendInlineFn<'a, Sig, const SIZE: usize = DEFAULT_INLINE_FN_SIZE> =
    InlineFn<'a, Sig, SIZE, SendOnly>;

/// Type alias for an [`InlineFn`] that is both `Send` and `Sync`.
pub type SendSyncInlineFn<'a, Sig, const SIZE: usize = DEFAULT_INLINE_FN_SIZE> =
    InlineFn<'a, Sig, SIZE, SendSync>;

/// Type alias for an [`InlineFnMut`] that is `Send` (but `!Sync`).
pub type SendInlineFnMut<'a, Sig, const SIZE: usize = DEFAULT_INLINE_FN_SIZE> =
    InlineFnMut<'a, Sig, SIZE, SendOnly>;

/// Type alias for an [`InlineFnMut`] that is both `Send` and `Sync`.
pub type SendSyncInlineFnMut<'a, Sig, const SIZE: usize = DEFAULT_INLINE_FN_SIZE> =
    InlineFnMut<'a, Sig, SIZE, SendSync>;

/// Type alias for an [`InlineFnOnce`] that is `Send` (but `!Sync`).
pub type SendInlineFnOnce<'a, Sig, const SIZE: usize = DEFAULT_INLINE_FN_SIZE> =
    InlineFnOnce<'a, Sig, SIZE, SendOnly>;

/// Type alias for an [`InlineFnOnce`] that is both `Send` and `Sync`.
pub type SendSyncInlineFnOnce<'a, Sig, const SIZE: usize = DEFAULT_INLINE_FN_SIZE> =
    InlineFnOnce<'a, Sig, SIZE, SendSync>;

macro_rules! impl_inline_fn_for_signature {
    ($($arg:ident : $arg_ty:ident),*) => {
        impl<$($arg_ty,)* Ret> private::SealedSig for fn($($arg_ty),*) -> Ret {}

        impl<$($arg_ty,)* Ret> FnSig for fn($($arg_ty),*) -> Ret {}

        impl<$($arg_ty,)* Ret, F> private::SealedWithSig<fn($($arg_ty),*) -> Ret> for F
        where
            F: FnOnce($($arg_ty),*) -> Ret,
        {
        }

        // SAFETY: `call_once_trampoline` moves `F` out of `state_ptr` and invokes it once with
        // signature `unsafe fn(*mut u8, $arg_ty...) -> Ret`, and `drop_fn_for::<F>()` drops `F`
        // in place at `state_ptr`.
        unsafe impl<$($arg_ty,)* Ret, F> FnOnceWithSig<fn($($arg_ty),*) -> Ret> for F
        where
            F: FnOnce($($arg_ty),*) -> Ret,
        {
            const VTABLE_ONCE: &'static VTableOnce = {
                /// Moves `F` out of `state_ptr` and invokes it once.
                ///
                /// # Safety
                ///
                /// - `state_ptr` must be non-null, properly aligned for `F`, and valid for reads
                ///   of `F`.
                /// - `state_ptr` must point to a valid, initialized instance of `F` with exclusive
                ///   ownership.
                /// - Ownership of `F` is consumed by this call; the value at `state_ptr` must not
                ///   be accessed or dropped again after this call.
                #[allow(clippy::too_many_arguments)]
                unsafe fn call_once_trampoline<$($arg_ty,)* Ret, F: FnOnce($($arg_ty),*) -> Ret>(
                    state_ptr: *mut u8,
                    $($arg: $arg_ty),*
                ) -> Ret {
                    // SAFETY: The caller guarantees `state_ptr` points to a valid, properly
                    // aligned, initialized `F` and transfers ownership of `F` to this call so it
                    // is moved and consumed exactly once.
                    let closure = unsafe { ptr::read(state_ptr.cast::<F>()) };
                    closure($($arg),*)
                }

                &VTableOnce {
                    call_once: call_once_trampoline::<$($arg_ty,)* Ret, F>
                        as unsafe fn(*mut u8 $(, $arg_ty)*) -> Ret
                        as *const (),
                    drop: drop_fn_for::<F>(),
                }
            };
        }

        // SAFETY: `call_mut_trampoline` invokes `F` via `&mut F` with signature
        // `unsafe fn(*mut u8, $arg_ty...) -> Ret`, and `VTABLE_ONCE` is `F`'s valid
        // `FnOnceWithSig` vtable.
        unsafe impl<$($arg_ty,)* Ret, F> FnMutWithSig<fn($($arg_ty),*) -> Ret> for F
        where
            F: FnMut($($arg_ty),*) -> Ret,
        {
            const VTABLE_MUT: &'static VTable = {
                /// Invokes `F` at `state_ptr` by mutable reference `&mut F`.
                ///
                /// # Safety
                ///
                /// - `state_ptr` must be non-null, properly aligned for `F`, and valid for reads
                ///   and writes of `F`.
                /// - `state_ptr` must point to a valid, initialized instance of `F` with exclusive
                ///   (`&mut F`) access for the duration of this call.
                #[allow(clippy::too_many_arguments)]
                unsafe fn call_mut_trampoline<$($arg_ty,)* Ret, F: FnMut($($arg_ty),*) -> Ret>(
                    state_ptr: *mut u8,
                    $($arg: $arg_ty),*
                ) -> Ret {
                    // SAFETY: The caller guarantees `state_ptr` points to a valid, properly
                    // aligned, initialized `F` with exclusive access for the duration of the call.
                    let closure_mut = unsafe { &mut *state_ptr.cast::<F>() };
                    closure_mut($($arg),*)
                }

                &VTable {
                    call: call_mut_trampoline::<$($arg_ty,)* Ret, F>
                        as unsafe fn(*mut u8 $(, $arg_ty)*) -> Ret
                        as *const (),
                    once: *<F as FnOnceWithSig<fn($($arg_ty),*) -> Ret>>::VTABLE_ONCE,
                }
            };
        }

        // SAFETY: `call_trampoline` invokes `F` via `&F` with signature
        // `unsafe fn(*mut u8, $arg_ty...) -> Ret`, and `VTABLE_ONCE` is `F`'s valid
        // `FnOnceWithSig` vtable.
        unsafe impl<$($arg_ty,)* Ret, F> FnWithSig<fn($($arg_ty),*) -> Ret> for F
        where
            F: Fn($($arg_ty),*) -> Ret,
        {
            const VTABLE: &'static VTable = {
                /// Invokes `F` at `state_ptr` by shared reference `&F`.
                ///
                /// # Safety
                ///
                /// - `state_ptr` must be non-null, properly aligned for `F`, and valid for reads
                ///   of `F`.
                /// - `state_ptr` must point to a valid, initialized instance of `F` with shared
                ///   (`&F`) access (no concurrent mutable access) for the duration of this call.
                #[allow(clippy::too_many_arguments)]
                unsafe fn call_trampoline<$($arg_ty,)* Ret, F: Fn($($arg_ty),*) -> Ret>(
                    state_ptr: *mut u8,
                    $($arg: $arg_ty),*
                ) -> Ret {
                    // SAFETY: The caller guarantees `state_ptr` points to a valid, properly
                    // aligned, initialized `F` with shared access for the duration of the call.
                    let closure_ref = unsafe { &*state_ptr.cast::<F>() };
                    closure_ref($($arg),*)
                }

                &VTable {
                    call: call_trampoline::<$($arg_ty,)* Ret, F>
                        as unsafe fn(*mut u8 $(, $arg_ty)*) -> Ret
                        as *const (),
                    once: *<F as FnOnceWithSig<fn($($arg_ty),*) -> Ret>>::VTABLE_ONCE,
                }
            };
        }

        impl<'a, $($arg_ty,)* Ret, const SIZE: usize, S>
            InlineFn<'a, fn($($arg_ty),*) -> Ret, SIZE, S>
        {
            /// Invokes the underlying closure with the provided arguments.
            #[inline]
            #[allow(clippy::too_many_arguments)]
            pub fn call(&self, $($arg: $arg_ty),*) -> Ret {
                // SAFETY: By the type invariants of `RawInlineFn` and `InlineFn`, `self.raw`
                // holds a valid, initialized `F` matching `self.raw.vtable.call`,
                // `self.raw.vtable.call` was erased from an `unsafe fn(*mut u8, ...) -> Ret`
                // trampoline for this exact signature, and `&self` guarantees shared access to `F`
                // for the duration of the call.
                unsafe {
                    let call = core::mem::transmute::<
                        *const (),
                        unsafe fn(*mut u8 $(, $arg_ty)*) -> Ret,
                    >(self.raw.vtable.call);
                    call(self.raw.as_ptr() $(, $arg)*)
                }
            }
        }

        impl<'a, $($arg_ty,)* Ret, const SIZE: usize, S>
            InlineFnMut<'a, fn($($arg_ty),*) -> Ret, SIZE, S>
        {
            /// Invokes the underlying mutable closure with the provided arguments.
            #[inline]
            #[allow(clippy::too_many_arguments)]
            pub fn call(&mut self, $($arg: $arg_ty),*) -> Ret {
                // SAFETY: By the type invariants of `RawInlineFn` and `InlineFnMut`, `self.raw`
                // holds a valid, initialized `F` matching `self.raw.vtable.call`,
                // `self.raw.vtable.call` was erased from an `unsafe fn(*mut u8, ...) -> Ret`
                // trampoline for this exact signature, and `&mut self` guarantees exclusive access
                // to `F` for the duration of the call.
                unsafe {
                    let call = core::mem::transmute::<
                        *const (),
                        unsafe fn(*mut u8 $(, $arg_ty)*) -> Ret,
                    >(self.raw.vtable.call);
                    call(self.raw.as_ptr() $(, $arg)*)
                }
            }
        }

        impl<'a, $($arg_ty,)* Ret, const SIZE: usize, S>
            InlineFnOnce<'a, fn($($arg_ty),*) -> Ret, SIZE, S>
        {
            /// Consumes `self` and invokes the underlying closure once.
            #[inline]
            #[allow(clippy::too_many_arguments)]
            pub fn call(self, $($arg: $arg_ty),*) -> Ret {
                let raw = ManuallyDrop::new(self.raw);
                // SAFETY: By the type invariants of `RawInlineFn` and `InlineFnOnce`, `raw` holds
                // a valid, initialized `F` matching `raw.vtable.call_once`, and
                // `raw.vtable.call_once` was erased from an `unsafe fn(*mut u8, ...) -> Ret`
                // trampoline for this exact signature. Wrapping `self.raw` in `ManuallyDrop`
                // suppresses `RawInlineFn::drop` so `F` is moved and consumed exactly once by
                // `call_once`.
                unsafe {
                    let call_once = core::mem::transmute::<
                        *const (),
                        unsafe fn(*mut u8 $(, $arg_ty)*) -> Ret,
                    >(raw.vtable.call_once);
                    call_once(raw.as_ptr() $(, $arg)*)
                }
            }
        }
    };
}

macro_rules! impl_inline_fn_for_hrtb_signature {
    ($($lt:lifetime => $arg:ident : $arg_ty:ident),+) => {
        impl<$($arg_ty: FnArg,)+ Ret> private::SealedSig for Hrtb<fn($($arg_ty),+) -> Ret> {}

        impl<$($arg_ty: FnArg,)+ Ret> FnSig for Hrtb<fn($($arg_ty),+) -> Ret> {}

        impl<$($arg_ty: FnArg,)+ Ret, F>
            private::SealedWithSig<Hrtb<fn($($arg_ty),+) -> Ret>> for F
        where
            F: for<$($lt),+> FnOnce($($arg_ty::Target<$lt>),+) -> Ret,
        {
        }

        // SAFETY: `Helper::call_once` moves `F` out of `state_ptr` and invokes it once with
        // signature `for<$lt...> unsafe fn(*mut u8, $arg_ty::Target<$lt>...) -> Ret`, and
        // `drop_fn_for::<F>()` drops `F` in place at `state_ptr`.
        unsafe impl<$($arg_ty: FnArg,)+ Ret, F>
            FnOnceWithSig<Hrtb<fn($($arg_ty),+) -> Ret>> for F
        where
            F: for<$($lt),+> FnOnce($($arg_ty::Target<$lt>),+) -> Ret,
        {
            const VTABLE_ONCE: &'static VTableOnce = {
                struct Helper<$($arg_ty: FnArg,)+ Ret, F>(
                    PhantomData<(*const Ret, F $(, *const $arg_ty)+)>,
                );

                impl<$($arg_ty: FnArg,)+ Ret, F> Helper<$($arg_ty,)+ Ret, F>
                where
                    F: for<$($lt),+> FnOnce($($arg_ty::Target<$lt>),+) -> Ret,
                {
                    /// Moves `F` out of `state_ptr` and invokes it once.
                    ///
                    /// # Safety
                    ///
                    /// - `state_ptr` must be non-null, properly aligned for `F`, and valid for
                    ///   reads of `F`.
                    /// - `state_ptr` must point to a valid, initialized instance of `F` with
                    ///   exclusive ownership.
                    /// - Ownership of `F` is consumed by this call; the value at `state_ptr` must
                    ///   not be accessed or dropped again after this call.
                    #[allow(clippy::too_many_arguments)]
                    unsafe fn call_once<$($lt),+>(
                        state_ptr: *mut u8,
                        $($arg: $arg_ty::Target<$lt>),+
                    ) -> Ret {
                        // SAFETY: The caller guarantees `state_ptr` points to a valid, properly
                        // aligned, initialized `F` and transfers ownership of `F` to this call so
                        // it is moved and consumed exactly once.
                        let closure = unsafe { ptr::read(state_ptr.cast::<F>()) };
                        closure($($arg),+)
                    }
                }

                &VTableOnce {
                    call_once: Helper::<$($arg_ty,)+ Ret, F>::call_once
                        as for<$($lt),+> unsafe fn(*mut u8 $(, $arg_ty::Target<$lt>)+) -> Ret
                        as *const (),
                    drop: drop_fn_for::<F>(),
                }
            };
        }

        // SAFETY: `Helper::call_mut` invokes `F` via `&mut F` with signature
        // `for<$lt...> unsafe fn(*mut u8, $arg_ty::Target<$lt>...) -> Ret`, and `VTABLE_ONCE` is
        // `F`'s valid `FnOnceWithSig` vtable.
        unsafe impl<$($arg_ty: FnArg,)+ Ret, F>
            FnMutWithSig<Hrtb<fn($($arg_ty),+) -> Ret>> for F
        where
            F: for<$($lt),+> FnMut($($arg_ty::Target<$lt>),+) -> Ret,
        {
            const VTABLE_MUT: &'static VTable = {
                struct Helper<$($arg_ty: FnArg,)+ Ret, F>(
                    PhantomData<(*const Ret, F $(, *const $arg_ty)+)>,
                );

                impl<$($arg_ty: FnArg,)+ Ret, F> Helper<$($arg_ty,)+ Ret, F>
                where
                    F: for<$($lt),+> FnMut($($arg_ty::Target<$lt>),+) -> Ret,
                {
                    /// Invokes `F` at `state_ptr` by mutable reference `&mut F`.
                    ///
                    /// # Safety
                    ///
                    /// - `state_ptr` must be non-null, properly aligned for `F`, and valid for
                    ///   reads and writes of `F`.
                    /// - `state_ptr` must point to a valid, initialized instance of `F` with
                    ///   exclusive (`&mut F`) access for the duration of this call.
                    #[allow(clippy::too_many_arguments)]
                    unsafe fn call_mut<$($lt),+>(
                        state_ptr: *mut u8,
                        $($arg: $arg_ty::Target<$lt>),+
                    ) -> Ret {
                        // SAFETY: The caller guarantees `state_ptr` points to a valid, properly
                        // aligned, initialized `F` with exclusive access for the duration of the
                        // call.
                        let closure_mut = unsafe { &mut *state_ptr.cast::<F>() };
                        closure_mut($($arg),+)
                    }
                }

                &VTable {
                    call: Helper::<$($arg_ty,)+ Ret, F>::call_mut
                        as for<$($lt),+> unsafe fn(*mut u8 $(, $arg_ty::Target<$lt>)+) -> Ret
                        as *const (),
                    once: *<F as FnOnceWithSig<Hrtb<fn($($arg_ty),+) -> Ret>>>::VTABLE_ONCE,
                }
            };
        }

        // SAFETY: `Helper::call` invokes `F` via `&F` with signature
        // `for<$lt...> unsafe fn(*mut u8, $arg_ty::Target<$lt>...) -> Ret`, and `VTABLE_ONCE` is
        // `F`'s valid `FnOnceWithSig` vtable.
        unsafe impl<$($arg_ty: FnArg,)+ Ret, F>
            FnWithSig<Hrtb<fn($($arg_ty),+) -> Ret>> for F
        where
            F: for<$($lt),+> Fn($($arg_ty::Target<$lt>),+) -> Ret,
        {
            const VTABLE: &'static VTable = {
                struct Helper<$($arg_ty: FnArg,)+ Ret, F>(
                    PhantomData<(*const Ret, F $(, *const $arg_ty)+)>,
                );

                impl<$($arg_ty: FnArg,)+ Ret, F> Helper<$($arg_ty,)+ Ret, F>
                where
                    F: for<$($lt),+> Fn($($arg_ty::Target<$lt>),+) -> Ret,
                {
                    /// Invokes `F` at `state_ptr` by shared reference `&F`.
                    ///
                    /// # Safety
                    ///
                    /// - `state_ptr` must be non-null, properly aligned for `F`, and valid for
                    ///   reads of `F`.
                    /// - `state_ptr` must point to a valid, initialized instance of `F` with
                    ///   shared (`&F`) access (no concurrent mutable access) for the duration of
                    ///   this call.
                    #[allow(clippy::too_many_arguments)]
                    unsafe fn call<$($lt),+>(
                        state_ptr: *mut u8,
                        $($arg: $arg_ty::Target<$lt>),+
                    ) -> Ret {
                        // SAFETY: The caller guarantees `state_ptr` points to a valid, properly
                        // aligned, initialized `F` with shared access for the duration of the call.
                        let closure_ref = unsafe { &*state_ptr.cast::<F>() };
                        closure_ref($($arg),+)
                    }
                }

                &VTable {
                    call: Helper::<$($arg_ty,)+ Ret, F>::call
                        as for<$($lt),+> unsafe fn(*mut u8 $(, $arg_ty::Target<$lt>)+) -> Ret
                        as *const (),
                    once: *<F as FnOnceWithSig<Hrtb<fn($($arg_ty),+) -> Ret>>>::VTABLE_ONCE,
                }
            };
        }

        impl<'a, $($arg_ty: FnArg,)+ Ret, const SIZE: usize, S>
            InlineFn<'a, Hrtb<fn($($arg_ty),+) -> Ret>, SIZE, S>
        {
            /// Invokes the underlying closure with the provided arguments.
            #[inline]
            #[allow(clippy::too_many_arguments)]
            pub fn call<$($lt),+>(&self, $($arg: $arg_ty::Target<$lt>),+) -> Ret {
                // SAFETY: By the type invariants of `RawInlineFn` and `InlineFn`, `self.raw`
                // holds a valid, initialized `F` matching `self.raw.vtable.call`,
                // `self.raw.vtable.call` was erased from a higher-ranked trampoline valid for all
                // call-site lifetimes of this exact signature, and `&self` guarantees shared
                // access to `F` for the duration of the call.
                unsafe {
                    let call = core::mem::transmute::<
                        *const (),
                        unsafe fn(*mut u8 $(, $arg_ty::Target<$lt>)+) -> Ret,
                    >(self.raw.vtable.call);
                    call(self.raw.as_ptr() $(, $arg)+)
                }
            }
        }

        impl<'a, $($arg_ty: FnArg,)+ Ret, const SIZE: usize, S>
            InlineFnMut<'a, Hrtb<fn($($arg_ty),+) -> Ret>, SIZE, S>
        {
            /// Invokes the underlying mutable closure with the provided arguments.
            #[inline]
            #[allow(clippy::too_many_arguments)]
            pub fn call<$($lt),+>(&mut self, $($arg: $arg_ty::Target<$lt>),+) -> Ret {
                // SAFETY: By the type invariants of `RawInlineFn` and `InlineFnMut`, `self.raw`
                // holds a valid, initialized `F` matching `self.raw.vtable.call`,
                // `self.raw.vtable.call` was erased from a higher-ranked trampoline valid for all
                // call-site lifetimes of this exact signature, and `&mut self` guarantees exclusive
                // access to `F` for the duration of the call.
                unsafe {
                    let call = core::mem::transmute::<
                        *const (),
                        unsafe fn(*mut u8 $(, $arg_ty::Target<$lt>)+) -> Ret,
                    >(self.raw.vtable.call);
                    call(self.raw.as_ptr() $(, $arg)+)
                }
            }
        }

        impl<'a, $($arg_ty: FnArg,)+ Ret, const SIZE: usize, S>
            InlineFnOnce<'a, Hrtb<fn($($arg_ty),+) -> Ret>, SIZE, S>
        {
            /// Consumes `self` and invokes the underlying closure once.
            #[inline]
            #[allow(clippy::too_many_arguments)]
            pub fn call<$($lt),+>(self, $($arg: $arg_ty::Target<$lt>),+) -> Ret {
                let raw = ManuallyDrop::new(self.raw);
                // SAFETY: By the type invariants of `RawInlineFn` and `InlineFnOnce`, `raw` holds
                // a valid, initialized `F` matching `raw.vtable.call_once`, and
                // `raw.vtable.call_once` was erased from a higher-ranked trampoline valid for all
                // call-site lifetimes of this exact signature. Wrapping `self.raw` in
                // `ManuallyDrop` suppresses `RawInlineFn::drop` so `F` is moved and consumed
                // exactly once by `call_once`.
                unsafe {
                    let call_once = core::mem::transmute::<
                        *const (),
                        unsafe fn(*mut u8 $(, $arg_ty::Target<$lt>)+) -> Ret,
                    >(raw.vtable.call_once);
                    call_once(raw.as_ptr() $(, $arg)+)
                }
            }
        }
    };
}

impl_inline_fn_for_signature!();
impl_inline_fn_for_signature!(a1: A1);
impl_inline_fn_for_signature!(a1: A1, a2: A2);
impl_inline_fn_for_signature!(a1: A1, a2: A2, a3: A3);
impl_inline_fn_for_signature!(a1: A1, a2: A2, a3: A3, a4: A4);
impl_inline_fn_for_signature!(a1: A1, a2: A2, a3: A3, a4: A4, a5: A5);
impl_inline_fn_for_signature!(a1: A1, a2: A2, a3: A3, a4: A4, a5: A5, a6: A6);
impl_inline_fn_for_signature!(a1: A1, a2: A2, a3: A3, a4: A4, a5: A5, a6: A6, a7: A7);
impl_inline_fn_for_signature!(a1: A1, a2: A2, a3: A3, a4: A4, a5: A5, a6: A6, a7: A7, a8: A8);

impl_inline_fn_for_hrtb_signature!('r1 => a1: A1);
impl_inline_fn_for_hrtb_signature!('r1 => a1: A1, 'r2 => a2: A2);
impl_inline_fn_for_hrtb_signature!('r1 => a1: A1, 'r2 => a2: A2, 'r3 => a3: A3);
impl_inline_fn_for_hrtb_signature!('r1 => a1: A1, 'r2 => a2: A2, 'r3 => a3: A3, 'r4 => a4: A4);
impl_inline_fn_for_hrtb_signature!(
    'r1 => a1: A1,
    'r2 => a2: A2,
    'r3 => a3: A3,
    'r4 => a4: A4,
    'r5 => a5: A5
);
impl_inline_fn_for_hrtb_signature!(
    'r1 => a1: A1,
    'r2 => a2: A2,
    'r3 => a3: A3,
    'r4 => a4: A4,
    'r5 => a5: A5,
    'r6 => a6: A6
);
impl_inline_fn_for_hrtb_signature!(
    'r1 => a1: A1,
    'r2 => a2: A2,
    'r3 => a3: A3,
    'r4 => a4: A4,
    'r5 => a5: A5,
    'r6 => a6: A6,
    'r7 => a7: A7
);
impl_inline_fn_for_hrtb_signature!(
    'r1 => a1: A1,
    'r2 => a2: A2,
    'r3 => a3: A3,
    'r4 => a4: A4,
    'r5 => a5: A5,
    'r6 => a6: A6,
    'r7 => a7: A7,
    'r8 => a8: A8
);

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;
    use core::fmt::Write as _;
    use core::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn test_inline_fn_zero_and_multiple_args() {
        let f0: InlineFn<'_, fn() -> u64> = InlineFn::new(|| 42);
        assert_eq!(f0.call(), 42);

        let factor = 3u32;
        let f2: InlineFn<'_, fn(u32, u32) -> u32> = InlineFn::new(|a, b| (a + b) * factor);
        assert_eq!(f2.call(4, 5), 27);
        assert_eq!(f2.call(1, 2), 9);

        let f8: InlineFn<'_, fn(u32, u32, u32, u32, u32, u32, u32, u32) -> u32> =
            InlineFn::new(|a1, a2, a3, a4, a5, a6, a7, a8| a1 + a2 + a3 + a4 + a5 + a6 + a7 + a8);
        assert_eq!(f8.call(1, 2, 3, 4, 5, 6, 7, 8), 36);
    }

    #[test]
    fn test_zero_sized_closure_and_non_static_refs() {
        let zst_fn: InlineFn<'_, fn(u32) -> u32, 0> = InlineFn::new(|x: u32| x + 1);
        assert_eq!(zst_fn.call(9), 10);

        fn call_with_local_ref<'a, 'b>(local_val: &'a u32, arg: &'b u32) -> u32 {
            let borrow_fn: InlineFn<'a, fn(&'b u32) -> u32> =
                InlineFn::new(|r: &'b u32| *r + *local_val);
            borrow_fn.call(arg)
        }

        let local_val = 100u32;
        let arg = 23u32;
        assert_eq!(call_with_local_ref(&local_val, &arg), 123);
    }

    #[test]
    fn test_zst_with_drop_and_resize() {
        static ZST_DROPS: AtomicUsize = AtomicUsize::new(0);
        struct ZstGuard;
        impl Drop for ZstGuard {
            fn drop(&mut self) {
                ZST_DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }

        ZST_DROPS.store(0, Ordering::SeqCst);
        {
            let guard = ZstGuard;
            let f: InlineFn<'static, fn() -> u32, 0> = InlineFn::new(move || {
                let _ = &guard;
                7
            });
            assert_eq!(f.call(), 7);
            let widened: InlineFn<'static, fn() -> u32, 8> = f.resize::<8>();
            assert_eq!(widened.call(), 7);
            assert_eq!(ZST_DROPS.load(Ordering::SeqCst), 0);
        }
        assert_eq!(ZST_DROPS.load(Ordering::SeqCst), 1);

        {
            let guard = ZstGuard;
            let f_once: InlineFnOnce<'static, fn() -> u32, 0> = InlineFnOnce::new(move || {
                drop(guard);
                11
            });
            let widened_once: InlineFnOnce<'static, fn() -> u32, 8> = f_once.resize::<8>();
            assert_eq!(ZST_DROPS.load(Ordering::SeqCst), 1);
            assert_eq!(widened_once.call(), 11);
            assert_eq!(ZST_DROPS.load(Ordering::SeqCst), 2);
        }
        assert_eq!(ZST_DROPS.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn test_inline_fn_mut() {
        let mut sum = 0u64;
        {
            let mut f: InlineFnMut<'_, fn(u64) -> u64> = InlineFnMut::new(|x| {
                sum += x;
                sum
            });
            assert_eq!(f.call(10), 10);
            assert_eq!(f.call(25), 35);
        }
        assert_eq!(sum, 35);
    }

    #[test]
    fn test_inline_fn_once_consumes_and_drops_once() {
        struct DropCounter<'a>(&'a Cell<usize>, u32);
        impl Drop for DropCounter<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let drops = Cell::new(0);
        let token = DropCounter(&drops, 99);
        let f: InlineFnOnce<'_, fn(u32) -> u32> = InlineFnOnce::new(move |x| {
            let t = token;
            t.1 + x
        });
        assert_eq!(drops.get(), 0);
        assert_eq!(f.call(1), 100);
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn test_drop_without_calling() {
        struct DropCounter<'a>(&'a Cell<usize>, [u64; 2]);
        impl Drop for DropCounter<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let drops = Cell::new(0);
        {
            let token = DropCounter(&drops, [1, 2]);
            let _f: InlineFn<'_, fn() -> u64> = InlineFn::new(move || token.1[0] + token.1[1]);
            assert_eq!(drops.get(), 0);
        }
        assert_eq!(drops.get(), 1);

        {
            let token = DropCounter(&drops, [3, 4]);
            let _f_once: InlineFnOnce<'_, fn() -> u64> =
                InlineFnOnce::new(move || token.1[0] + token.1[1]);
            assert_eq!(drops.get(), 1);
        }
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn test_custom_sizes_and_resize() {
        struct DropCounter<'a>(&'a Cell<usize>, [u64; 2]);
        impl Drop for DropCounter<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let data = [10u64, 20, 30, 40, 50, 60];
        let small: InlineFn<'_, fn() -> u64, 48> = InlineFn::new(move || data.iter().sum());
        assert_eq!(small.call(), 210);

        let same_size: InlineFn<'_, fn() -> u64, 48> = small.resize::<48>();
        assert_eq!(same_size.call(), 210);

        let larger: InlineFn<'_, fn() -> u64, 64> = same_size.resize::<64>();
        assert_eq!(larger.call(), 210);

        // Verify `InlineFnMut::resize` and `InlineFnOnce::resize` preserve state and drop once.
        let drops = Cell::new(0);
        {
            let mut token = DropCounter(&drops, [5, 10]);
            let mut f_mut: InlineFnMut<'_, fn(u64) -> u64, 24> = InlineFnMut::new(move |x| {
                token.1[0] += x;
                token.1[0] + token.1[1]
            });
            assert_eq!(f_mut.call(3), 18);
            let mut f_mut_wide: InlineFnMut<'_, fn(u64) -> u64, 48> = f_mut.resize::<48>();
            assert_eq!(drops.get(), 0);
            assert_eq!(f_mut_wide.call(2), 20);
        }
        assert_eq!(drops.get(), 1);

        {
            let token = DropCounter(&drops, [20, 30]);
            let f_once: InlineFnOnce<'_, fn(u64) -> u64, 24> = InlineFnOnce::new(move |x| {
                let t = token;
                t.1[0] + t.1[1] + x
            });
            let f_once_wide: InlineFnOnce<'_, fn(u64) -> u64, 48> = f_once.resize::<48>();
            assert_eq!(drops.get(), 1);
            assert_eq!(f_once_wide.call(4), 54);
            assert_eq!(drops.get(), 2);
        }
    }

    #[test]
    fn test_align8_with_padding() {
        #[repr(C, align(8))]
        struct Padded<'a> {
            tag: u8,
            // 7 bytes of uninitialized padding between `tag` and `value`.
            value: u64,
            drops: &'a Cell<usize>,
        }

        impl Drop for Padded<'_> {
            fn drop(&mut self) {
                self.drops.set(self.drops.get() + 1);
            }
        }

        let drops = Cell::new(0);
        {
            let p = Padded { tag: 3, value: 100, drops: &drops };
            let f: InlineFn<'_, fn(u64) -> u64, 24> =
                InlineFn::new(move |x| (p.tag as u64) + p.value + x);
            let widened = f.resize::<32>();
            assert_eq!(widened.call(7), 110);
            assert_eq!(drops.get(), 0);
        }
        assert_eq!(drops.get(), 1);
    }

    #[test]
    fn test_reentrant_inline_fn_call() {
        type RecFn<'a> = InlineFn<'a, fn(u32) -> u32>;
        let depth = Cell::new(0u32);
        let self_ptr: Cell<*const RecFn<'_>> = Cell::new(core::ptr::null());
        let f: RecFn<'_> = InlineFn::new(|n| {
            depth.set(depth.get() + 1);
            if n == 0 {
                0
            } else {
                // SAFETY: `self_ptr` points to `f`, which remains live for the duration of `f.call`.
                let me = unsafe { &*self_ptr.get() };
                n + me.call(n - 1)
            }
        });
        self_ptr.set(&f);
        assert_eq!(f.call(4), 10);
        assert_eq!(depth.get(), 5);
    }

    #[test]
    fn test_conversions_between_variants() {
        let drops = Cell::new(0usize);
        struct Guard<'a>(&'a Cell<usize>);
        impl Drop for Guard<'_> {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }

        let g = Guard(&drops);
        let shared: InlineFn<'_, fn(i32) -> i32> = InlineFn::new(move |x| {
            let _ = &g;
            x * 2
        });
        assert_eq!(shared.call(5), 10);

        let mut mutable = InlineFnMut::from(shared);
        assert_eq!(mutable.call(7), 14);

        let once = InlineFnOnce::from(mutable);
        assert_eq!(once.call(9), 18);
        assert_eq!(drops.get(), 1);

        let g2 = Guard(&drops);
        let shared2: InlineFn<'_, fn(i32) -> i32> = InlineFn::new(move |x| {
            let _ = &g2;
            x + 1
        });
        let once_direct = InlineFnOnce::from(shared2);
        assert_eq!(once_direct.call(4), 5);
        assert_eq!(drops.get(), 2);
    }

    #[test]
    fn test_send_and_sync_variants() {
        fn assert_send<T: Send>(_: &T) {}
        fn assert_sync<T: Sync>(_: &T) {}

        let counter = AtomicUsize::new(1);
        let f_sync = SendSyncInlineFn::<fn(usize) -> usize>::new(|v| {
            counter.fetch_add(v, Ordering::Relaxed)
        });
        assert_send(&f_sync);
        assert_sync(&f_sync);
        assert_eq!(f_sync.call(4), 1);
        assert_eq!(counter.load(Ordering::Relaxed), 5);

        let f_send: SendInlineFn<'_, fn(usize) -> usize> = f_sync.into_send();
        assert_send(&f_send);
        assert_eq!(f_send.call(2), 5);

        let f_local = f_send.into_local();
        assert_eq!(f_local.call(3), 7);

        let f_send_explicit =
            SendInlineFn::<fn(usize) -> usize>::new_send(|v| counter.load(Ordering::Relaxed) + v);
        assert_send(&f_send_explicit);
        assert_eq!(f_send_explicit.call(2), 12);

        let mut mut_state = 10u32;
        let mut f_mut_sync = SendSyncInlineFnMut::<fn(u32) -> u32>::new_send_sync(move |x| {
            mut_state += x;
            mut_state
        });
        assert_send(&f_mut_sync);
        assert_sync(&f_mut_sync);
        assert_eq!(f_mut_sync.call(5), 15);

        let mut f_mut_send = f_mut_sync.into_send();
        assert_send(&f_mut_send);
        assert_eq!(f_mut_send.call(5), 20);

        let mut f_mut_local = f_mut_send.into_local();
        assert_eq!(f_mut_local.call(5), 25);

        let mut f_mut_send2 = SendInlineFnMut::<fn(u32) -> u32>::new_send(move |x| {
            mut_state += x;
            mut_state
        });
        assert_send(&f_mut_send2);
        assert_eq!(f_mut_send2.call(5), 15);

        let mut f_mut_local2 = InlineFnMut::<fn(u32) -> u32>::new_local(move |x| {
            mut_state += x;
            mut_state
        });
        assert_eq!(f_mut_local2.call(3), 13);

        let f_once_sync =
            SendSyncInlineFnOnce::<fn(u32) -> u32>::new_send_sync(move |x| mut_state + x);
        assert_send(&f_once_sync);
        assert_sync(&f_once_sync);
        let f_once_send = f_once_sync.into_send();
        assert_send(&f_once_send);
        let f_once_local = f_once_send.into_local();
        assert_eq!(f_once_local.call(20), 30);

        let f_once_send2 = SendInlineFnOnce::<fn(u32) -> u32>::new_send(move |x| mut_state + x);
        assert_send(&f_once_send2);
        assert_eq!(f_once_send2.call(10), 20);

        let f_once_local2 = InlineFnOnce::<fn(u32) -> u32>::new_local(move |x| mut_state + x);
        assert_eq!(f_once_local2.call(5), 15);

        // Local variant can capture `!Send` / `!Sync` types like `Cell` or raw pointers.
        let local_cell = Cell::new(10u32);
        let raw_ptr: *const Cell<u32> = &local_cell;
        let local_fn: InlineFn<'_, fn(u32) -> u32> = InlineFn::new_local(move |x| {
            // SAFETY: `raw_ptr` points to `local_cell` on the same stack frame.
            let cell = unsafe { &*raw_ptr };
            cell.set(cell.get() + x);
            cell.get()
        });
        assert_eq!(local_fn.call(5), 15);
    }

    #[test]
    fn test_debug_formatting() {
        struct Buf {
            bytes: [u8; 128],
            len: usize,
        }
        impl Buf {
            fn new() -> Self {
                Self { bytes: [0; 128], len: 0 }
            }
            fn as_str(&self) -> &str {
                core::str::from_utf8(&self.bytes[..self.len]).unwrap()
            }
        }
        impl fmt::Write for Buf {
            fn write_str(&mut self, s: &str) -> fmt::Result {
                let b = s.as_bytes();
                self.bytes[self.len..self.len + b.len()].copy_from_slice(b);
                self.len += b.len();
                Ok(())
            }
        }

        let f: InlineFn<'_, fn()> = InlineFn::new(|| {});
        let mut buf = Buf::new();
        write!(&mut buf, "{:?}", f).unwrap();
        assert!(buf.as_str().starts_with("InlineFn { size: "));

        let f_mut: InlineFnMut<'_, fn()> = InlineFnMut::new(|| {});
        let mut buf = Buf::new();
        write!(&mut buf, "{:?}", f_mut).unwrap();
        assert!(buf.as_str().starts_with("InlineFnMut { size: "));

        let f_once: InlineFnOnce<'_, fn()> = InlineFnOnce::new(|| {});
        let mut buf = Buf::new();
        write!(&mut buf, "{:?}", f_once).unwrap();
        assert!(buf.as_str().starts_with("InlineFnOnce { size: "));

        let mut buf = Buf::new();
        write!(
            &mut buf,
            "{:?},{:?},{:?}",
            Local(PhantomData),
            SendOnly(PhantomData),
            SendSync(PhantomData)
        )
        .unwrap();
        assert_eq!(buf.as_str(), "Local,SendOnly,SendSync");

        // Verify `fmt::Debug` on `ValArg`, `RefArg`, `MutArg`, and `Hrtb` even when their type
        // parameters do not implement `Debug`.
        struct NoDebug;
        let mut buf = Buf::new();
        write!(
            &mut buf,
            "{:?},{:?},{:?},{:?}",
            ValArg::<NoDebug>(PhantomData),
            RefArg::<NoDebug>(PhantomData),
            MutArg::<NoDebug>(PhantomData),
            Hrtb::<fn(ValArg<NoDebug>)>(PhantomData),
        )
        .unwrap();
        assert_eq!(buf.as_str(), "ValArg,RefArg,MutArg,Hrtb");
    }

    #[test]
    fn test_layout_and_option_niche_optimization() {
        let ptr_size = core::mem::size_of::<usize>();
        assert_eq!(core::mem::size_of::<InlineFn<'static, fn(), 0>>(), ptr_size);
        assert_eq!(core::mem::size_of::<InlineFn<'static, fn(), 8>>(), 8 + ptr_size);
        assert_eq!(core::mem::size_of::<InlineFnMut<'static, fn(), 16>>(), 16 + ptr_size);
        assert_eq!(core::mem::size_of::<InlineFnOnce<'static, fn(), 16>>(), 16 + ptr_size);
        assert_eq!(
            core::mem::size_of::<Option<InlineFn<'static, fn()>>>(),
            core::mem::size_of::<InlineFn<'static, fn()>>()
        );
        assert_eq!(
            core::mem::size_of::<Option<InlineFnMut<'static, fn()>>>(),
            core::mem::size_of::<InlineFnMut<'static, fn()>>()
        );
        assert_eq!(
            core::mem::size_of::<Option<InlineFnOnce<'static, fn()>>>(),
            core::mem::size_of::<InlineFnOnce<'static, fn()>>()
        );
        assert_eq!(
            core::mem::size_of::<
                Option<SendInlineFnMut<'static, Hrtb<fn(RefArg<u32>, ValArg<u32>)>, 16>>,
            >(),
            16 + ptr_size
        );
    }

    #[test]
    fn test_hrtb_signatures() {
        // 1-arg shared reference (including `?Sized` `str`)
        let prefix_len = 2usize;
        let f_str: InlineFn<'_, Hrtb<fn(RefArg<str>) -> usize>> =
            InlineFn::new(|s: &str| s.len() + prefix_len);
        {
            let local = [b'a', b'b', b'c'];
            let s = core::str::from_utf8(&local).unwrap();
            assert_eq!(f_str.call(s), 5);
        }
        assert_eq!(f_str.call("hello"), 7);

        // 1-arg mutable reference
        let mut step = 1u32;
        let mut f_mut_ref: InlineFnMut<'_, Hrtb<fn(MutArg<u32>)>> =
            InlineFnMut::new(move |x: &mut u32| {
                *x += step;
                step += 1;
            });
        let mut val = 10u32;
        f_mut_ref.call(&mut val);
        assert_eq!(val, 11);
        f_mut_ref.call(&mut val);
        assert_eq!(val, 13);

        // 2-arg `fn(&T, U)` with `SendInlineFnMut`, `resize`, and `into_fn_once`
        let mut calls = 0u32;
        let f_pair: SendInlineFnMut<'static, Hrtb<fn(RefArg<u32>, ValArg<u32>) -> u32>, 8> =
            SendInlineFnMut::new(move |a: &u32, b: u32| {
                calls += 1;
                *a + b + calls
            });
        let mut f_pair = f_pair;
        {
            let short_lived = 10u32;
            assert_eq!(f_pair.call(&short_lived, 5), 16);
        }
        let mut f_pair_wide: SendInlineFnMut<
            'static,
            Hrtb<fn(RefArg<u32>, ValArg<u32>) -> u32>,
            16,
        > = f_pair.resize::<16>();
        {
            let short_lived2 = 20u32;
            assert_eq!(f_pair_wide.call(&short_lived2, 3), 25);
        }
        let f_pair_once: SendInlineFnOnce<'static, Hrtb<fn(RefArg<u32>, ValArg<u32>) -> u32>, 16> =
            f_pair_wide.into_fn_once();
        let short_lived3 = 30u32;
        assert_eq!(f_pair_once.call(&short_lived3, 1), 34);

        // 2-arg `fn(U, &T)`, `fn(&T, &U)`, and `fn(&mut T, &mut U)`
        let f_second_ref: InlineFn<'static, Hrtb<fn(ValArg<u32>, RefArg<str>) -> usize>> =
            InlineFn::new(|n: u32, s: &str| n as usize + s.len());
        assert_eq!(f_second_ref.call(4, "abc"), 7);

        let f_two_refs: InlineFn<'static, Hrtb<fn(RefArg<str>, RefArg<[u8]>) -> usize>> =
            InlineFn::new(|a: &str, b: &[u8]| a.len() + b.len());
        assert_eq!(f_two_refs.call("hi", &[1, 2, 3]), 5);

        let f_two_muts: InlineFn<'static, Hrtb<fn(MutArg<u32>, MutArg<u32>)>> =
            InlineFn::new(|a: &mut u32, b: &mut u32| core::mem::swap(a, b));
        let (mut x, mut y) = (1u32, 2u32);
        f_two_muts.call(&mut x, &mut y);
        assert_eq!((x, y), (2, 1));

        // 3-arg `fn(&T, U, V)` and `fn(&mut T, U, V)`
        let f_three: InlineFn<'static, Hrtb<fn(RefArg<u32>, ValArg<u32>, ValArg<u32>) -> u32>> =
            InlineFn::new(|a: &u32, b: u32, c: u32| *a + b + c);
        assert_eq!(f_three.call(&1, 2, 3), 6);

        let f_three_mut: InlineFn<'static, Hrtb<fn(MutArg<u32>, ValArg<u32>, ValArg<u32>)>> =
            InlineFn::new(|a: &mut u32, b: u32, c: u32| *a += b + c);
        let mut z = 10u32;
        f_three_mut.call(&mut z, 4, 5);
        assert_eq!(z, 19);

        // Verify `Send + Sync` and `Drop` behavior for `Hrtb` signatures.
        fn assert_send<T: Send>(_: &T) {}
        fn assert_sync<T: Sync>(_: &T) {}

        static DROPS: AtomicUsize = AtomicUsize::new(0);
        struct StaticDropGuard(u32);
        impl Drop for StaticDropGuard {
            fn drop(&mut self) {
                DROPS.fetch_add(1, Ordering::SeqCst);
            }
        }

        DROPS.store(0, Ordering::SeqCst);
        {
            let guard = StaticDropGuard(7);
            let f_sync: SendSyncInlineFn<'static, Hrtb<fn(RefArg<u32>) -> u32>, 8> =
                SendSyncInlineFn::new(move |x: &u32| *x + guard.0);
            assert_send(&f_sync);
            assert_sync(&f_sync);
            assert_eq!(f_sync.call(&5), 12);
            let mut f_mut = f_sync.into_send().into_fn_mut();
            assert_send(&f_mut);
            assert_eq!(f_mut.call(&6), 13);
            assert_eq!(DROPS.load(Ordering::SeqCst), 0);
        }
        assert_eq!(DROPS.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn test_hrtb_non_static_val_arg_and_variance() {
        // Non-`'static` borrowed value passed via `ValArg<&'a u32>` alongside a higher-ranked
        // `RefArg<str>` parameter across `InlineFn`, `InlineFnMut`, and `InlineFnOnce`.
        fn run_with_borrowed<'a>(base: &'a u32) -> (u32, u32, u32) {
            let f: InlineFn<'_, Hrtb<fn(ValArg<&'a u32>, RefArg<str>) -> u32>> =
                InlineFn::new(|v: &'a u32, s: &str| *v + s.len() as u32);
            let r1 = {
                let bytes = [b'x', b'y'];
                let s = core::str::from_utf8(&bytes).unwrap();
                f.call(base, s)
            };

            let mut acc = 0u32;
            let mut f_mut: InlineFnMut<'_, Hrtb<fn(ValArg<&'a u32>, RefArg<str>) -> u32>> =
                InlineFnMut::new(move |v: &'a u32, s: &str| {
                    acc += *v + s.len() as u32;
                    acc
                });
            let _ = f_mut.call(base, "a");
            let r2 = {
                let bytes = [b'b', b'c', b'd'];
                let s = core::str::from_utf8(&bytes).unwrap();
                f_mut.call(base, s)
            };

            let f_once: InlineFnOnce<'_, Hrtb<fn(ValArg<&'a u32>, RefArg<str>) -> u32>> =
                InlineFnOnce::new(|v: &'a u32, s: &str| *v * 2 + s.len() as u32);
            let r3 = {
                let bytes = [b'z'];
                let s = core::str::from_utf8(&bytes).unwrap();
                f_once.call(base, s)
            };

            (r1, r2, r3)
        }

        let local = 10u32;
        assert_eq!(run_with_borrowed(&local), (12, 24, 21));

        // Compile-time checks verifying `ValArg` and `RefArg` are covariant in `T`, making
        // `Hrtb<fn(ValArg<T>)>` and `Hrtb<fn(RefArg<T>)>` contravariant in `T`.
        fn val_arg_covariant<'a>(x: ValArg<&'static u32>) -> ValArg<&'a u32> {
            x
        }
        fn hrtb_val_arg_contravariant<'a>(
            x: Hrtb<fn(ValArg<&'a u32>)>,
        ) -> Hrtb<fn(ValArg<&'static u32>)> {
            x
        }
        fn hrtb_ref_arg_contravariant(
            x: Hrtb<fn(RefArg<fn(&'static u32)>)>,
        ) -> Hrtb<fn(RefArg<for<'r> fn(&'r u32)>)> {
            x
        }

        let _ = val_arg_covariant(ValArg(PhantomData));
        let _ = hrtb_val_arg_contravariant(Hrtb(PhantomData));
        let _ = hrtb_ref_arg_contravariant(Hrtb(PhantomData));
    }
}
