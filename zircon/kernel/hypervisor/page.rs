// Copyright 2018 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Pages of physical memory used by the hypervisor.

use core::mem::{align_of, size_of};
use core::ops::{Deref, DerefMut};
use core::ptr::{self, NonNull};
use pin_init::Init;
use zr::static_assert;
use zx_status::Status;

use crate::kernel::types::PAddr;
use crate::vm::page::VmPagePtr;
use crate::vm::page_state::VmPageState;
use crate::vm::{physmap, pmm};

/// A wired page of physical memory, which is freed when dropped.
///
/// The C++ `hypervisor::Page` in `<hypervisor/page.h>` is a facade over this type.
#[derive(Debug)]
#[repr(C)]
pub struct Page {
    /// The allocated page, or `None` if no page has been allocated.
    // Safety invariant: If `Some`, the page was allocated from the PMM without allowing loaned
    // pages, is in the WIRED state, is not in any page queue, and is exclusively owned by this
    // `Page`.
    page: Option<VmPagePtr>,
    /// The physical address of `page`, or 0 if no page has been allocated.
    // Safety invariant: `pa` is `PAddr(0)` if and only if `page` is `None`. When `page` is
    // `Some(p)`, `pa` is `p`'s non-zero, `page::SIZE`-aligned physical address mapped in the
    // kernel physmap.
    pa: PAddr,
}

static_assert!(size_of::<Page>() == 16);
static_assert!(align_of::<Page>() == 8);

// SAFETY: `Page` uniquely owns its page, so it may be dropped (freeing the page) on any thread.
unsafe impl Send for Page {}
// SAFETY: Shared references only read `page` and `pa`, which are never mutated through `&Page`.
// Accesses to the page's memory are made through raw pointers, which callers must synchronize.
unsafe impl Sync for Page {}

impl Page {
    /// Creates a `Page` with no page allocated.
    #[inline]
    pub const fn new() -> Self {
        Self { page: None, pa: PAddr(0) }
    }

    /// Allocates a wired page, and fills it with `fill`.
    ///
    /// # Panics
    ///
    /// Panics if `self` is already allocated (`self.is_allocated()`) or if `pmm::alloc_page`
    /// returns physical address 0, preventing a wired PMM page leak on double allocation.
    ///
    /// # Safety-usable invariant
    ///
    /// When this returns `Ok(())`, `self.is_allocated()` is `true` and the `page::SIZE` bytes at
    /// `self.virtual_address()` are initialized to `fill` and exclusively owned by `self`.
    pub fn alloc(&mut self, fill: u8) -> Result<(), Status> {
        // Allocating again would leak the previously allocated page.
        assert!(!self.is_allocated());
        let (page, pa) = pmm::alloc_page(0)?;
        // `pa == 0` is the unallocated sentinel for `is_allocated`; enforce the non-zero `pa`
        // safety invariant across architectures where `pmm::alloc_page` does not guarantee it.
        assert!(pa.0 != 0);
        self.page = Some(page);
        self.pa = pa;

        // SAFETY: `page` was just allocated, so `self` has exclusive ownership of it.
        unsafe { page.set_state(VmPageState::WIRED) };
        // SAFETY: `virtual_address` is the physmap mapping of the page that was just allocated,
        // which is `page::SIZE` bytes long and not referenced by anything else.
        unsafe { ptr::write_bytes(self.virtual_address::<u8>(), fill, page::SIZE) };
        Ok(())
    }

    /// Returns the address of the page in the physmap.
    ///
    /// # Safety-usable invariant
    ///
    /// When `self.is_allocated()` is `true`, returns the `page::SIZE`-aligned kernel physmap
    /// address of the `page::SIZE` writable bytes owned by `self`. Must not be dereferenced when
    /// `self.is_allocated()` is `false` (checked only by `debug_assert!`).
    #[inline]
    pub fn virtual_address<T>(&self) -> *mut T {
        debug_assert!(self.pa.0 != 0);
        physmap::paddr_to_physmap_mut(self.pa)
    }

    /// Returns the physical address of the page.
    ///
    /// # Safety-usable invariant
    ///
    /// When `self.is_allocated()` is `true`, returns the non-zero, `page::SIZE`-aligned physical
    /// address of the wired page owned by `self`.
    #[inline]
    pub fn physical_address(&self) -> PAddr {
        debug_assert!(self.pa.0 != 0);
        self.pa
    }

    /// Returns whether a page has been allocated.
    ///
    /// # Safety-usable invariant
    ///
    /// Returns `true` if and only if `self` owns an allocated wired page mapped in the physmap.
    #[inline]
    pub fn is_allocated(&self) -> bool {
        self.pa.0 != 0
    }
}

impl Default for Page {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for Page {
    fn drop(&mut self) {
        if let Some(page) = self.page.take() {
            // SAFETY: By `self.page`'s safety invariant, `page` was allocated from the PMM without
            // allowing loaned pages, is not in any page queue, and `self.page.take()` transfers
            // exclusive ownership to `pmm::free_page`.
            unsafe { pmm::free_page(page) };
        }
    }
}

/// Owns a wired physical page containing an in-place initialized value of type `T`.
#[derive(Debug)]
#[repr(C)]
pub struct PagePtr<T> {
    page: Page,
    // Safety invariant: If `Some(ptr)`, `self.page` is allocated and `ptr` points to a valid,
    // properly aligned, initialized `T` exclusively owned by `self` at the start of `self.page`'s
    // physmap mapping.
    ptr: Option<NonNull<T>>,
}

static_assert!(size_of::<PagePtr<u8>>() == 24);
static_assert!(align_of::<PagePtr<u8>>() == 8);

// SAFETY: `PagePtr<T>` exclusively owns the backing page and `T` instance.
unsafe impl<T: Send> Send for PagePtr<T> {}
// SAFETY: Shared references to `PagePtr<T>` only permit shared access (`&T` via `Deref`, or
// `*const T` via `PagePtr::as_ptr`), which is sound because `T: Sync`. Exclusive access (`&mut T`
// via `DerefMut` or `*mut T` via `PagePtr::as_mut_ptr`) requires an exclusive `&mut self`.
unsafe impl<T: Sync> Sync for PagePtr<T> {}

impl<T> Default for PagePtr<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> PagePtr<T> {
    /// Creates an unallocated `PagePtr<T>`.
    pub const fn new() -> Self {
        Self { page: Page::new(), ptr: None }
    }

    /// Allocates a zero-filled wired page and initializes `T` in place via `init`.
    ///
    /// # Panics
    ///
    /// Panics if `self` is already allocated (`self.ptr.is_some()`), preventing a live `T` from
    /// being overwritten without dropping.
    pub fn alloc_init(&mut self, init: impl Init<T>) -> Result<(), Status> {
        const {
            assert!(size_of::<T>() <= page::SIZE);
            assert!(align_of::<T>() <= page::SIZE);
        }
        assert!(self.ptr.is_none());
        self.page.alloc(0)?;

        let raw = self.page.virtual_address::<T>();
        let ptr = NonNull::new(raw).expect("physmap virtual address must be non-null");
        // SAFETY: By the safety-usable invariants of `Page::alloc` and `Page::virtual_address`,
        // `self.page.is_allocated()` is `true` after `self.page.alloc(0)` returns `Ok(())`, and
        // `ptr.as_ptr()` is the non-null, `page::SIZE`-aligned (`align_of::<T>() <= page::SIZE`)
        // physmap address of `page::SIZE` (`size_of::<T>() <= page::SIZE`) zero-initialized
        // writable bytes exclusively owned by `self.page` for the duration of `__init`.
        let Ok(()) = unsafe { init.__init(ptr.as_ptr()) };
        self.ptr = Some(ptr);
        Ok(())
    }

    /// Allocates a zero-filled wired page containing a zero-initialized `T`.
    pub fn alloc(&mut self) -> Result<(), Status>
    where
        T: pin_init::Zeroable,
    {
        self.alloc_init(pin_init::init_zeroed())
    }

    /// Returns the physical address of the backing page.
    #[inline]
    pub fn physical_address(&self) -> PAddr {
        self.page.physical_address()
    }

    /// Returns a const raw pointer to the contained `T`, or null if unallocated.
    #[inline]
    pub fn as_ptr(this: &Self) -> *const T {
        this.ptr.map_or(ptr::null(), |p| p.as_ptr().cast_const())
    }

    /// Returns a mutable raw pointer to the contained `T`, or null if unallocated.
    #[inline]
    pub fn as_mut_ptr(this: &mut Self) -> *mut T {
        this.ptr.map_or(ptr::null_mut(), NonNull::as_ptr)
    }
}

impl<T> Deref for PagePtr<T> {
    type Target = T;

    #[inline]
    fn deref(&self) -> &Self::Target {
        // SAFETY: By `self.ptr`'s safety invariant, `ptr` is aligned and points to an initialized
        // `T` exclusively owned by `self` in `self.page`, so shared borrow `&self` guarantees no
        // mutable aliases exist for the returned reference's lifetime.
        unsafe { self.ptr.expect("PagePtr::deref called before alloc").as_ref() }
    }
}

impl<T> DerefMut for PagePtr<T> {
    #[inline]
    fn deref_mut(&mut self) -> &mut Self::Target {
        // SAFETY: By `self.ptr`'s safety invariant, `ptr` is aligned and points to an initialized
        // `T` exclusively owned by `self` in `self.page`, and `&mut self` guarantees exclusive
        // access for the returned reference's lifetime.
        unsafe { self.ptr.expect("PagePtr::deref_mut called before alloc").as_mut() }
    }
}

impl<T> Drop for PagePtr<T> {
    fn drop(&mut self) {
        if let Some(ptr) = self.ptr.take() {
            // SAFETY: By `self.ptr`'s safety invariant, `ptr` is aligned and points to an
            // initialized `T` exclusively owned by `self`, and `self.page` is dropped only after
            // `Drop::drop` returns.
            unsafe {
                ptr::drop_in_place(ptr.as_ptr());
            }
        }
    }
}

/// Unit tests for `Page` and `PagePtr`.
#[cfg(ktest)]
#[unittest::suite(name = "hypervisor_page")]
mod tests {
    use super::{Page, PagePtr};
    use unittest::{assert_ok, expect_eq, expect_false, expect_ne, expect_true};

    #[repr(C)]
    #[derive(pin_init::Zeroable)]
    struct TestStruct {
        a: u64,
        b: u64,
    }

    /// Verifies that a `Page` is only allocated after `alloc`, which fills it.
    #[test]
    fn alloc_fills_page() {
        const FILL: u8 = 0xa5;

        let mut page = Page::new();
        expect_false!(page.is_allocated());

        assert_ok!(page.alloc(FILL));
        expect_true!(page.is_allocated());
        expect_ne!(0, page.physical_address().0);
        // SAFETY: `virtual_address` maps the `page::SIZE` bytes of the page `page` owns, which
        // nothing else accesses while the slice is alive.
        let bytes =
            unsafe { core::slice::from_raw_parts(page.virtual_address::<u8>(), page::SIZE) };
        expect_true!(bytes.iter().all(|&b| b == FILL));
    }

    /// Verifies that `PagePtr::alloc` and `PagePtr::alloc_init` initialize `T` in place.
    #[test]
    fn page_ptr_alloc() {
        let mut ptr = PagePtr::<TestStruct>::new();
        expect_true!(PagePtr::as_ptr(&ptr).is_null());
        assert_ok!(ptr.alloc());
        expect_false!(PagePtr::as_ptr(&ptr).is_null());
        expect_false!(PagePtr::as_mut_ptr(&mut ptr).is_null());
        expect_eq!(ptr.a, 0);
        expect_eq!(ptr.b, 0);
        ptr.a = 42;
        ptr.b = 99;
        expect_eq!(ptr.a, 42);
        expect_eq!(ptr.b, 99);

        let mut init_ptr = PagePtr::<TestStruct>::new();
        assert_ok!(init_ptr.alloc_init(pin_init::init!(TestStruct { a: 123, b: 456 })));
        expect_eq!(init_ptr.a, 123);
        expect_eq!(init_ptr.b, 456);
    }
}
