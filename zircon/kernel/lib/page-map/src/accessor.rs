// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use super::{Entry, PageMap};
use core::ffi::c_void;
use core::marker::PhantomData;
use core::mem::{align_of, offset_of, size_of};
use core::ptr::NonNull;
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};

/// Provides access to a single object of type `T`.
///
/// Use `PageMap::make_accessor` to create an `Accessor`.
///
/// The `read()` and `write()` methods (and the `read_field!` and `write_field!` macros) provide
/// TOCTOU-safe access to an object or its fields by copying to/from the object (or fields) and
/// then issuing a compiler barrier to prevent copy elision.  See also `barrier_after_copy_to`.
///
/// `Accessor` has the same layout as the C++ `page_map::Accessor` in `<lib/page-map/accessor.h>`,
/// so C++ types embedding an `Accessor` field can release it in their destructor with
/// `rust_page_map_release_entry`.
///
/// Instances of `Accessor` are not safe for concurrent use (i.e. not thread-safe).
#[repr(C)]
pub struct Accessor<'a, T: FromBytes + IntoBytes + Immutable + KnownLayout> {
    /// Many `Accessor` instances may refer to a single `Entry`.  `Entry` must outlive its
    /// `Accessor`s.
    ///
    /// Stored as `NonNull<c_void>` so `Accessor` (and types embedding it, such as `Thread`)
    /// remains FFI-safe (`improper_ctypes`-clean) without requiring `Entry` to be `#[repr(C)]`.
    ///
    /// May be `None` if this object was used as the source of a move operation (see `take`).
    entry: Option<NonNull<c_void>>,
    /// May be `None` if this object was used as the source of a move operation (see `take`).
    object: Option<NonNull<T>>,
    /// The `PageMap` that owns `entry` must outlive this `Accessor`.
    _page_map: PhantomData<&'a PageMap>,
}

// Keep layout and field order in sync with C++ `page_map::Accessor` in
// `//zircon/kernel/lib/page-map/include/lib/page-map/accessor.h`.
zr::static_assert!(size_of::<Accessor<'static, u64>>() == 2 * size_of::<usize>());
zr::static_assert!(align_of::<Accessor<'static, u64>>() == align_of::<usize>());
zr::static_assert!(offset_of!(Accessor<'static, u64>, entry) == 0);
zr::static_assert!(offset_of!(Accessor<'static, u64>, object) == size_of::<usize>());

// SAFETY: An `Accessor` only refers to its `Entry` and object through raw pointers, and releasing
// its `Entry` (see `Entry::release`) synchronizes through the owning `PageMap`'s `lock`, so an
// `Accessor` may be moved to and dropped on another thread.  It is not `Sync` because instances
// are not safe for concurrent use.
unsafe impl<T: FromBytes + IntoBytes + Immutable + KnownLayout + Send> Send for Accessor<'_, T> {}

impl<T: FromBytes + IntoBytes + Immutable + KnownLayout> Default for Accessor<'_, T> {
    /// Construct an invalid `Accessor`.  See `PageMap::make_accessor`.
    fn default() -> Self {
        Self { entry: None, object: None, _page_map: PhantomData }
    }
}

impl<T: FromBytes + IntoBytes + Immutable + KnownLayout> Drop for Accessor<'_, T> {
    /// Because a mapping may be destroyed upon the destruction of the last `Accessor` that
    /// references it, this may only be called from thread context, where it's safe to acquire VM
    /// locks.
    fn drop(&mut self) {
        self.invalidate();
    }
}

impl<'a, T: FromBytes + IntoBytes + Immutable + KnownLayout> Accessor<'a, T> {
    /// Construct an `Accessor` for the existing `entry` and `object`.
    ///
    /// # Safety
    ///
    /// Either `entry` and `object` must both be `None`, or both must be `Some` such that:
    ///   - the caller obtained an accessor reference on `entry` for `object`, and that accessor
    ///     reference has not yet been released
    ///   - `object` points to a properly aligned `T` wholly contained within `entry`'s mapped page
    ///   - `entry` and the `PageMap` that owns it outlive `'a`.
    pub(super) unsafe fn from_raw(
        entry: Option<NonNull<Entry>>,
        object: Option<NonNull<T>>,
    ) -> Self {
        Self { entry: entry.map(NonNull::cast), object, _page_map: PhantomData }
    }

    /// Returns true if this instance is valid.
    ///
    /// Invalid instances are akin to null pointers.  Only valid instances may be read/written.
    #[inline]
    pub fn is_valid(&self) -> bool {
        debug_assert_eq!(self.entry.is_some(), self.object.is_some());
        self.object.is_some()
    }

    /// `Accessor` is a move-only type.
    ///
    /// Moves out of this instance, which will be invalidated.
    #[inline]
    pub fn take(&mut self) -> Self {
        core::mem::take(self)
    }

    /// Read (copy) the object from the VMO.
    ///
    /// # Panics
    ///
    /// Panics if called on an invalid instance.
    #[inline]
    pub fn read(&self) -> T {
        let object = self.object.expect("read() called on an invalid Accessor");
        // SAFETY: `object` points to a properly aligned `T` within the page that `entry` keeps
        // mapped, committed, and pinned for as long as this `Accessor` exists.  `T: FromBytes`, so
        // any bit pattern, including one torn by a concurrent user mode write, is a valid `T`.
        let dst = unsafe { object.as_ptr().read() };
        Self::barrier_after_copy_to(object.as_ptr());
        dst
    }

    /// Write (copy) `src` to the object.
    ///
    /// # Panics
    ///
    /// Panics if called on an invalid instance.
    #[inline]
    pub fn write(&self, src: &T) {
        let object = self.object.expect("write() called on an invalid Accessor");
        // SAFETY: `object` points to a properly aligned `T` within the page that `entry` keeps
        // mapped, committed, and pinned for as long as this `Accessor` exists.  `T: IntoBytes`,
        // so `src` has no uninitialized bytes, and `src` cannot overlap the object in the VMO.
        unsafe { core::ptr::copy_nonoverlapping(src, object.as_ptr(), 1) };
        Self::barrier_after_copy_to(object.as_ptr());
    }

    /// Like `read`, but for reading only one field of an object.  Use `read_field!` instead.
    ///
    /// # Safety
    ///
    /// `project` must return a pointer to a field of the object pointed to by its argument,
    /// without accessing the object.
    ///
    /// # Panics
    ///
    /// Panics if called on an invalid instance.
    #[doc(hidden)]
    #[inline]
    pub unsafe fn read_field_raw<F: FromBytes + IntoBytes + Immutable>(
        &self,
        project: impl FnOnce(*mut T) -> *mut F,
    ) -> F {
        let object = self.object.expect("read_field!() called on an invalid Accessor");
        let field = project(object.as_ptr());
        // SAFETY: The caller guarantees that `field` points to a field of the object, which lies
        // within the page that `entry` keeps mapped, committed, and pinned for as long as this
        // `Accessor` exists.  `F: FromBytes`, so any bit pattern is a valid `F`.  The field may be
        // unaligned if `T` is packed.
        let dst = unsafe { field.read_unaligned() };
        Self::barrier_after_copy_to(field);
        dst
    }

    /// Like `write`, but for writing only one field of an object.  Use `write_field!` instead.
    ///
    /// # Safety
    ///
    /// `project` must return a pointer to a field of the object pointed to by its argument,
    /// without accessing the object.
    ///
    /// # Panics
    ///
    /// Panics if called on an invalid instance.
    #[doc(hidden)]
    #[inline]
    pub unsafe fn write_field_raw<F: FromBytes + IntoBytes + Immutable>(
        &self,
        project: impl FnOnce(*mut T) -> *mut F,
        src: F,
    ) {
        let object = self.object.expect("write_field!() called on an invalid Accessor");
        let field = project(object.as_ptr());
        // SAFETY: The caller guarantees that `field` points to a field of the object, which lies
        // within the page that `entry` keeps mapped, committed, and pinned for as long as this
        // `Accessor` exists.  The field may be unaligned if `T` is packed.
        unsafe { field.write_unaligned(src) };
        Self::barrier_after_copy_to(field);
    }

    /// A compiler barrier used to prevent copy-elision and double-fetch TOCTOU hazards.
    ///
    /// Call this with the VMO mapping pointer `ptr` immediately after copying from or to `*ptr`,
    /// and before validating or acting on a read copy.
    ///
    /// Because this inline assembly omits `nomem` and `readonly`, the compiler must assume that it
    /// may both read and modify `*ptr`, so the compiler:
    ///   - must complete any preceding read from or write to `*ptr` before the barrier (preventing
    ///     dead-store elimination and copy elision)
    ///   - cannot re-fetch from `*ptr` in place of a local copy read before the barrier, or reorder
    ///     subsequent operations on that copy with accesses to `*ptr` across the barrier.
    #[inline(always)]
    pub fn barrier_after_copy_to<U>(ptr: *mut U) {
        // SAFETY: The assembly is empty and executes no instructions.  It omits `nomem` and
        // `readonly`, so the compiler must assume that it may read and write the memory reachable
        // through `ptr`.
        unsafe { core::arch::asm!("/* {0} */", in(reg) ptr, options(nostack, preserves_flags)) };
    }

    /// Invalidate this accessor and release any mapping it may retain.
    ///
    /// After `invalidate`, `is_valid` will return false.
    fn invalidate(&mut self) {
        if let Some(entry) = self.entry {
            // SAFETY: This `Accessor` holds an accessor reference on `entry`, which it gives up
            // here and never uses again.
            unsafe { Entry::release(entry.cast()) };
            self.entry = None;
            self.object = None;
        }
    }
}

/// Like `Accessor::read`, but for reading only one field of an object.
///
/// E.g.
///
/// ```ignore
/// struct Aggregate { field1: i32, field2: i32 }
/// let a: Accessor<'_, Aggregate> = ...;
/// let f1: i32 = read_field!(a, field1);
/// ```
#[macro_export]
macro_rules! read_field {
    ($accessor:expr, $field:ident) => {{
        let accessor = &$accessor;
        // SAFETY: The projection only computes the address of `$field` within the object.
        unsafe { accessor.read_field_raw(|object| &raw mut (*object).$field) }
    }};
}
pub use read_field;

/// Like `Accessor::write`, but for writing only one field of an object.
///
/// E.g.
///
/// ```ignore
/// struct Aggregate { field1: i32, field2: i32 }
/// let a: Accessor<'_, Aggregate> = ...;
/// let f1: i32 = 42;
/// write_field!(a, field1, f1);
/// ```
#[macro_export]
macro_rules! write_field {
    ($accessor:expr, $field:ident, $src:expr) => {{
        let accessor = &$accessor;
        let src = $src;
        // SAFETY: The projection only computes the address of `$field` within the object.
        unsafe { accessor.write_field_raw(|object| &raw mut (*object).$field, src) }
    }};
}
pub use write_field;
