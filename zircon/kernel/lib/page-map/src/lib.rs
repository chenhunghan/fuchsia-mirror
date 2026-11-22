// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Kernel access to objects within VMOs that are logically owned by usermode.

mod accessor;
mod entry;
mod ffi;

pub use accessor::{Accessor, read_field, write_field};
pub use entry::Entry;
use entry::Key;

use crate::kernel::semaphore::Semaphore;
use crate::ksync_kernel::TrackedCriticalMutex;
use crate::object_cache::{ObjectCache, UniquePtr};
use crate::vm::arch_vm_aspace::{ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE};
use crate::vm::page::object::MAX_PIN_COUNT;
use crate::vm::vm_aspace::VmAspace;
use crate::vm::vm_object_paged::VmObjectPaged;
use core::convert::Infallible;
use core::mem::{DropGuard, align_of, size_of};
use core::pin::Pin;
use core::ptr::NonNull;
use fbl::{RefPtr, WavlTree, WavlTreeKeyable};
use ksync::{KMutex, LockToken, guarded, kcell_init};
use lazy_init::LazyInit;
use pin_init::{PinInit, pin_data, pin_init};
use zerocopy::{FromBytes, Immutable, IntoBytes, KnownLayout};
use zx_status::Status;

/// `PageMap` provides safe and efficient kernel access to objects within VMOs that are logically
/// owned by usermode.  `PageMap` automatically handles the mapping, committing, and pinning of the
/// underlying pages.
///
/// There are limitations on the kinds of objects that may be accessed via `PageMap`.  They:
///   - must be `FromBytes + IntoBytes + Immutable + KnownLayout` (no implicit padding, etc.)
///   - must not span page boundaries
///   - must have an alignment requirement of less than or equal to one page
///   - must be naturally aligned within the VMO
///
/// An object, or a field of an object, may be read/written using an `Accessor` instance (see
/// `make_accessor`).  An `Accessor` provides access to a single object, at a given offset within a
/// VMO.  The offset, together with the VMO's address in memory identify a page, allowing `PageMap`
/// to internally ref-count mappings so that all `Accessor` instances for the same VMO and offset
/// will reference a single mapping.  See also `Key`.
///
/// `Accessor` is designed to avoid TOCTOU hazards by providing read/write methods for access
/// rather than a pointer.
///
/// Instances of `PageMap` are safe for concurrent use (thread-safe).
///
/// TODO(maniscalco): To save memory, explore using the existing physmap mapping instead of
/// creating a new one for each `Entry`.
///
/// TODO(maniscalco): Once it's available, use the btree instead of a WAVL tree for storing
/// `Entry`s
#[guarded]
#[pin_data]
pub struct PageMap {
    #[mutex]
    lock: KMutex<TrackedCriticalMutex>,

    #[pin]
    #[guarded_by(lock)]
    map: WavlTree<Key, UniquePtr<Entry>>,

    /// Semaphore that restricts the number of "unfindable" entries to avoid pin count failures
    /// (see usage in `make_accessor` and `release` for more).
    ///
    /// TODO(https://fxbug.dev/498989748): Once the pin count limit is gone this can be removed.
    #[pin]
    unfindable_entry_count: Semaphore,

    /// The allocator from which `Entry`s are created.
    #[pin]
    allocator: ObjectCache<Entry>,
}

// SAFETY: `map` is only accessed while holding `lock`, and `Semaphore` and `ObjectCache`
// synchronize internally, so a `PageMap` may be moved to another thread.
unsafe impl Send for PageMap {}
// SAFETY: `map` is only accessed while holding `lock`, and `Semaphore` and `ObjectCache`
// synchronize internally, so a `PageMap` may be shared between threads.
unsafe impl Sync for PageMap {}

/// The global `PageMap`.
static PAGE_MAP: LazyInit<PageMap> = LazyInit::uninit();

/// Initializes the global `PageMap` before any thread can use it.
fn page_map_init(_level: init::LkInitLevel) {
    // SAFETY: Init hooks run once on the primary CPU, before any call to `PageMap::get`.
    let Ok(()) = unsafe { Pin::static_ref(&PAGE_MAP).init_pin(PageMap::new()) };
}

init::lk_init_hook!(page_map, page_map_init, init::LK_INIT_LEVEL_KERNEL);

impl PageMap {
    /// Returns an initializer for an empty `PageMap`.
    pub fn new() -> impl PinInit<Self, Infallible> {
        pin_init!(Self {
            lock <- KMutex::init(),
            map <- kcell_init(WavlTree::new()),
            unfindable_entry_count <- Semaphore::new(i64::from(MAX_PIN_COUNT - 1)),
            allocator <- ObjectCache::new(/* reserve_slabs */ 1),
        })
    }

    /// Constructs an `Accessor` for the object at `object_offset_in_vmo` in `vmo`.
    ///
    /// Maps, commits, pins as necessary.
    ///
    /// `Accessor`s act as ref-counted pointers for the underlying VMO and mapping.  Two
    /// `Accessor`s for the same offset in the same VMO will result in only one mapping.  When the
    /// last `Accessor` for a given page is destroyed, the underlying mapping, and possibly its
    /// VMO, will be destroyed.  To avoid unnecessary lock coupling, do not allow an `Accessor` to
    /// be destroyed while holding another lock.
    ///
    /// For the purposes of mapping reuse, VMO "sameness" is determined by the VMO's address in
    /// memory (i.e. `vmo.as_raw()`).
    ///
    /// The object must have an alignment requirements of one page or less, and must be naturally
    /// aligned within the VMO.
    ///
    /// The object may not straddle a page boundary.
    ///
    /// On success, returns a valid `Accessor`.
    ///
    /// Returns `Status::INVALID_ARGS` if the object straddles a page boundary or the object is not
    /// properly aligned.
    ///
    /// Returns `Status::OUT_OF_RANGE` if the provided offset triggers arithmetic overflow.
    pub fn make_accessor<'a, T: FromBytes + IntoBytes + Immutable + KnownLayout>(
        self: Pin<&'a Self>,
        vmo: RefPtr<VmObjectPaged>,
        object_offset_in_vmo: usize,
    ) -> Result<Accessor<'a, T>, Status> {
        const { assert!(size_of::<T>() > 0) };
        const { assert!(size_of::<T>() <= page::SIZE) };
        const { assert!(align_of::<T>() <= page::SIZE) };

        // Make sure the object does not span pages.
        //
        // last is the offset in the vmo of the object's last byte.
        let Some(last) = object_offset_in_vmo.checked_add(size_of::<T>() - 1) else {
            return Err(Status::OUT_OF_RANGE);
        };
        // Make sure the object's first byte and last byte are in the same page.
        let page_offset_in_vmo = page::round_down(object_offset_in_vmo);
        if page::round_down(last) != page_offset_in_vmo {
            return Err(Status::INVALID_ARGS);
        }

        // Make sure the object is properly aligned.
        if !object_offset_in_vmo.is_multiple_of(align_of::<T>()) {
            return Err(Status::INVALID_ARGS);
        }

        // Form a key for the page.
        //
        let object_offset_in_page = object_offset_in_vmo
            .checked_sub(page_offset_in_vmo)
            .expect("object offset is within its page");
        let key = Key { vm_object_paged: vmo.as_raw().addr(), page_offset_in_vmo };

        // Do we already have an entry for this page?
        {
            ksync::lock!(let mut guard = self.lock_lock());

            // Lookup an entry by its key.
            let iter = guard.fields().map.find(&key).map(NonNull::from);

            // If found, make an Accessor and return it.
            if let Some(iter) = iter {
                // SAFETY: `iter` points to an entry in `map`, which cannot be removed and
                // destroyed while `guard` holds `lock`, `guard`'s token is `lock`'s token, and the
                // page-boundary and alignment checks above ensure
                // `object_offset_in_page + size_of::<T>() <= page::SIZE` and
                // `object_offset_in_page` is a multiple of `align_of::<T>()`.
                return Ok(unsafe {
                    self.wrap_entry(
                        iter.as_ref(),
                        object_offset_in_page,
                        guard.as_mut().token_mut(),
                    )
                });
            }
        }

        // Before calling make_entry acquire the semaphore to restrict the number of concurrent
        // make_entry calls. This ensures that even in the worst case we will not attempt to pin a
        // single page so many times that we get an error. Although the semaphore applies to all
        // allocations across all pages of all vmos, and not just to a single page or even a single
        // vmo, more than 31 threads are unlikely to have meaningful parallelism as they will be
        // serialized through both the PMM and the kernel aspace locks respectively.
        // TODO(https://fxbug.dev/498989748): Once the pin count limit is gone this can be removed.
        self.unfindable_entry_count.wait_infinite()?;
        // Make sure we post the semaphore on all exits. This must be done after any new_entry is
        // destroyed to ensure the pin count is reduced prior to potentially letting another thread
        // pin.
        let _post = zr::defer(|| self.unfindable_entry_count.post(None));

        // No entry found, make one.  Take care to do so without holding the lock.  If we later
        // find that someone "beat us to it" then make sure we destroy our newly created entry
        // without holding the lock.
        let mut new_entry = self.make_entry(vmo, page_offset_in_vmo)?;
        let new_entry_ptr = NonNull::new(UniquePtr::as_mut_ptr(&mut new_entry))
            .expect("new_entry holds a non-null UniquePtr");

        let _uninserted_entry;
        ksync::lock!(let mut guard = self.lock_lock());

        let iter = match guard.as_mut().fields_mut().map.insert_or_find(new_entry) {
            Ok(()) => {
                _uninserted_entry = None;
                new_entry_ptr
            }
            Err((entry, cursor)) => {
                _uninserted_entry = Some(entry);
                NonNull::from(cursor.get().expect("insert_or_find found an existing entry"))
            }
        };

        // SAFETY: `iter` points to an entry in `map`, which cannot be removed and destroyed while
        // `guard` holds `lock`, `guard`'s token is `lock`'s token, and the page-boundary and
        // alignment checks above ensure `object_offset_in_page + size_of::<T>() <= page::SIZE` and
        // `object_offset_in_page` is a multiple of `align_of::<T>()`.
        Ok(unsafe {
            self.wrap_entry(iter.as_ref(), object_offset_in_page, guard.as_mut().token_mut())
        })
    }

    /// Returns a reference to the global `PageMap`.
    pub fn get() -> Pin<&'static PageMap> {
        Pin::static_ref(PAGE_MAP.get())
    }

    /// Make an entry for the given VMO and offset.  Commits, pins, and maps the specified page.
    fn make_entry(
        self: Pin<&Self>,
        vmo: RefPtr<VmObjectPaged>,
        page_offset_in_vmo: usize,
    ) -> Result<UniquePtr<Entry>, Status> {
        // Commit and pin.
        const MAPPING_SIZE: usize = page::SIZE;
        vmo.commit_range_pinned(
            page_offset_in_vmo as u64,
            MAPPING_SIZE as u64,
            /*write=*/ true,
        )?;
        let unpin = DropGuard::new(vmo.clone(), move |vmo| {
            vmo.unpin(page_offset_in_vmo as u64, MAPPING_SIZE as u64)
        });

        // Then map into the kernel aspace.
        let kernel_vmar =
            VmAspace::kernel_aspace().root_vmar().expect("kernel aspace has a root VMAR");
        let mapping_result = kernel_vmar.create_vm_mapping_no_overwrite(
            0,
            MAPPING_SIZE,
            0,
            0,
            VmObjectPaged::into_vm_object(vmo.clone()),
            page_offset_in_vmo as u64,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE,
        )?;
        let unmap = DropGuard::new(mapping_result.mapping.clone(), |mapping| {
            let _ = mapping.destroy();
        });

        // Fault-in the range.
        mapping_result.mapping.map_range(0, MAPPING_SIZE, true, false)?;

        let new_entry = self.allocator.allocate_init(Entry::new(
            self.get_ref(),
            vmo,
            mapping_result.mapping,
        ))?;
        DropGuard::dismiss(unmap);
        DropGuard::dismiss(unpin);

        Ok(new_entry)
    }

    /// Wraps an `Entry` with an `Accessor`.
    ///
    /// # Safety
    ///
    /// `entry` must be an entry in `map`, `token` must be the token of `lock` (which must be
    /// held), `object_offset_in_page + size_of::<T>() <= page::SIZE`, and `object_offset_in_page`
    /// must be a multiple of `align_of::<T>()`.
    unsafe fn wrap_entry<'a, T: FromBytes + IntoBytes + Immutable + KnownLayout>(
        self: Pin<&'a Self>,
        entry: &Entry,
        object_offset_in_page: usize,
        token: &mut LockToken<'_, PageMapLockClass>,
    ) -> Accessor<'a, T> {
        let object_address = entry
            .mapping_base()
            .checked_add(object_offset_in_page)
            .expect("object address does not overflow");
        let object = NonNull::new(core::ptr::with_exposed_provenance_mut::<T>(object_address));
        // SAFETY: The caller guarantees that `token` is the token of `lock`, which guards the
        // accessor counts of the entries in `map`.
        unsafe { entry.increment_accessor_count(token) };
        // SAFETY: The accessor reference was just taken on `entry`, which is owned by `self`
        // (`self` outlives `'a`).  `entry`'s mapping is page-aligned, non-null, and `page::SIZE`
        // bytes long (`align_of::<T>() <= page::SIZE`), so the caller's offset bounds guarantee
        // `object` is `Some` and points to a properly aligned `T` within `entry`'s mapped page.
        unsafe { Accessor::from_raw(Some(NonNull::from(entry)), object) }
    }

    /// May destroy `*entry`.
    ///
    /// # Safety
    ///
    /// `entry` must point to an entry in `map` on which the caller holds an accessor reference.
    /// The caller gives up that reference and must not use `entry` afterwards.
    unsafe fn release(&self, entry: NonNull<Entry>) {
        // If we're releasing the last reference we need to destroy the entry.  However, we must
        // limit the number of "unfindable" entries in order to not exceed the max pin count (see
        // unfindable_entry_count declaration for more).
        //
        // To ensure that the mutex-guarded critical section remains fast, we employ a two phase
        // approach.  We lookup the entry and if the count is greater than one, we simply decrement
        // and return.  However, if this was the last reference (i.e. count is one) we must back
        // out, consume an unfindable_entry_count, and try again.  If this second attempt succeeds
        // in releasing the last reference, we remove the entry from the map and destroy it before
        // posting the semaphore.

        // Last reference?
        {
            ksync::lock!(let mut guard = self.lock_lock());
            // SAFETY: The caller's accessor reference keeps `entry` in `map` and alive, and
            // `guard`'s token is the token of `lock`, which guards `entry`'s accessor count.
            unsafe {
                if entry.as_ref().accessor_count(guard.token()) > 1 {
                    entry.as_ref().decrement_accessor_count(guard.as_mut().token_mut());
                    // This wasn't the last reference.  We're done.
                    return;
                }
            }
        }

        // We're likely releasing the last reference.  Acquire the semaphore before we make the
        // entry unfindable.
        let status = self.unfindable_entry_count.wait_infinite();
        assert!(status.is_ok());

        // Make sure we post the semaphore on all exits.  This must be done after any removed entry
        // has been destroyed to ensure the pin count is reduced prior to potentially letting
        // another thread pin.
        let _post = zr::defer(|| self.unfindable_entry_count.post(None));

        let mut _entry_destroyer: Option<UniquePtr<Entry>> = None;
        {
            ksync::lock!(let mut guard = self.lock_lock());
            // SAFETY: The caller's accessor reference keeps `entry` in `map` and alive, and
            // `guard`'s token is the token of `lock`, which guards `entry`'s accessor count.
            if unsafe { entry.as_ref().decrement_accessor_count(guard.as_mut().token_mut()) } {
                // This was indeed the last one.  Remove it from the container so we can destroy it
                // without holding the container's lock.
                //
                // SAFETY: `entry` is still in `map` because only the thread that drops the accessor
                // count to zero removes an entry, which is this thread while `guard` holds `lock`
                // (`Entry`'s key fields are also immutable).  We erase by key rather than by
                // `entry` so that the returned `UniquePtr` is the tree's own pointer with full
                // allocation provenance rather than one derived from a shared reference.
                _entry_destroyer =
                    guard.as_mut().fields_mut().map.erase(&unsafe { entry.as_ref().get_key() });
                debug_assert!(
                    _entry_destroyer
                        .as_ref()
                        .is_some_and(|e| core::ptr::eq(UniquePtr::as_ptr(e), entry.as_ptr()))
                );
            }
        }
    }
}

/// In-tree kernel unit tests for `page_map`.
#[cfg(ktest)]
#[unittest::suite(name = "page-map")]
mod tests {
    use crate::vm::pmm;
    use fbl::HasRefCount;
    use pin_init::stack_pin_init;
    use unittest::{assert_eq, assert_err, assert_false, assert_true, expect_eq, expect_ge};

    // A helper that creates a VMO of a given size.
    fn make_vmo(size: usize) -> Result<RefPtr<VmObjectPaged>, Status> {
        const VMO_OPTIONS: u32 = 0;
        const PMM_ALLOC_FLAGS: u32 = pmm::ALLOC_FLAG_ANY | pmm::ALLOC_FLAG_CAN_WAIT;
        VmObjectPaged::create(PMM_ALLOC_FLAGS, VMO_OPTIONS, size as u64)
    }

    /// Tests constructing and destroying `PageMap`s.
    #[test]
    fn page_map_ctor_dtor_test() {
        stack_pin_init!(let _pm1 = PageMap::new());
        stack_pin_init!(let _pm2 = PageMap::new());
    }

    #[repr(C)]
    #[derive(Clone, Copy, Default, FromBytes, IntoBytes, Immutable, KnownLayout)]
    struct SomeStruct {
        field_a: u32,
        field_b: u16,
        field_c: u16,
    }

    /// Tests `PageMap::make_accessor` success, failure, and VMO reuse.
    #[test]
    fn page_map_make_accessor_test() {
        // Happy case.
        {
            let vmo = make_vmo(page::SIZE);
            assert_true!(vmo.is_ok());
            stack_pin_init!(let pm = PageMap::new());
            let result = pm.as_ref().make_accessor::<SomeStruct>(vmo.unwrap(), 0);
            assert_true!(result.is_ok());
        }

        // Zero size VMO fails.
        {
            let vmo = make_vmo(0);
            assert_true!(vmo.is_ok());
            stack_pin_init!(let pm = PageMap::new());
            let result = pm.as_ref().make_accessor::<SomeStruct>(vmo.unwrap(), 0);
            assert_true!(result.is_err());
            assert_err!(result, Status::OUT_OF_RANGE);
        }

        // Offset out of range fails.
        {
            let vmo = make_vmo(page::SIZE);
            assert_true!(vmo.is_ok());
            let vmo = vmo.unwrap();
            stack_pin_init!(let pm = PageMap::new());

            // Not out of range.
            {
                let result = pm.as_ref().make_accessor::<SomeStruct>(vmo.clone(), 0);
                assert_true!(result.is_ok());
            }

            // Just beyond.
            {
                let result = pm.as_ref().make_accessor::<SomeStruct>(vmo.clone(), page::SIZE);
                assert_err!(result, Status::OUT_OF_RANGE);
            }

            // Way out there.
            {
                let result =
                    pm.as_ref().make_accessor::<SomeStruct>(vmo.clone(), 1000 * page::SIZE);
                assert_err!(result, Status::OUT_OF_RANGE);
            }

            // Trigger integer overflow.
            {
                let result = pm.as_ref().make_accessor::<SomeStruct>(vmo.clone(), usize::MAX);
                assert_err!(result, Status::OUT_OF_RANGE);
            }

            // Trigger integer overflow for any possible mapping base address.
            {
                let result = pm
                    .as_ref()
                    .make_accessor::<SomeStruct>(vmo.clone(), usize::MAX - (16 * page::SIZE) + 1);
                assert_err!(result, Status::OUT_OF_RANGE);
            }

            // Straddles page boundary.
            {
                let result = pm.as_ref().make_accessor::<SomeStruct>(
                    vmo.clone(),
                    page::SIZE - (size_of::<SomeStruct>() / 2),
                );
                assert_err!(result, Status::INVALID_ARGS);
            }

            // Invalid alignment.
            {
                let result = pm.as_ref().make_accessor::<SomeStruct>(vmo.clone(), 1);
                assert_err!(result, Status::INVALID_ARGS);
            }
        }

        // Simple VMO reuse.
        //
        // Create two accessors for the same VMO.  See that changes made to one are visible in the
        // other.
        {
            let vmo = make_vmo(page::SIZE);
            assert_true!(vmo.is_ok());
            let vmo = vmo.unwrap();
            stack_pin_init!(let pm = PageMap::new());

            // Two accessors for the same object.
            let a1 = pm.as_ref().make_accessor::<SomeStruct>(vmo.clone(), 0);
            assert_true!(a1.is_ok());
            let a1 = a1.unwrap();
            assert_true!(a1.is_valid());
            let a2 = pm.as_ref().make_accessor::<SomeStruct>(vmo.clone(), 0);
            assert_true!(a2.is_ok());
            let a2 = a2.unwrap();
            assert_true!(a2.is_valid());

            let mut v1 = SomeStruct { field_a: 1, ..Default::default() };
            a1.write(&v1);

            let mut v2 = a2.read();
            assert_eq!(v2.field_a, 1u32);

            v2.field_a = 42;
            a2.write(&v2);

            v1 = a1.read();
            assert_eq!(v1.field_a, 42u32);
        }
    }

    /// Tests that writing one field of an object through `write_field!` leaves the others alone.
    #[test]
    fn page_map_field_accessor_test() {
        // See when modifying a field of an object, only that field is modified.

        let vmo = make_vmo(page::SIZE);
        assert_true!(vmo.is_ok());
        let vmo = vmo.unwrap();
        stack_pin_init!(let pm = PageMap::new());

        // Two accessors for the same object.
        let accessor = pm.as_ref().make_accessor::<SomeStruct>(vmo.clone(), 0);
        assert_true!(accessor.is_ok());
        let accessor = accessor.unwrap();
        assert_true!(accessor.is_valid());

        // Set an initial value for the whole object.
        let initial_value = SomeStruct { field_a: 1, field_b: 2, field_c: 3 };
        accessor.write(&initial_value);
        let v = accessor.read();
        assert_true!(v.as_bytes() == initial_value.as_bytes());

        // Write one field.
        let a: u32 = 6;
        write_field!(accessor, field_a, a);

        // See that the other fields are unaffected.
        let x: u16 = read_field!(accessor, field_b);
        assert_eq!(x, initial_value.field_b);
        let x: u16 = read_field!(accessor, field_c);
        assert_eq!(x, initial_value.field_c);
    }

    /// Tests that moving out of an `Accessor` invalidates the source.
    #[test]
    fn accessor_move_test() {
        // See that move-assignment invalidates the source.
        {
            let vmo = make_vmo(page::SIZE);
            assert_true!(vmo.is_ok());
            let vmo = vmo.unwrap();
            stack_pin_init!(let pm = PageMap::new());
            let source = pm.as_ref().make_accessor::<SomeStruct>(vmo.clone(), 0);
            assert_true!(source.is_ok());
            let mut source = source.unwrap();
            assert_true!(source.is_valid());

            let mut destination = Accessor::<SomeStruct>::default();
            assert_false!(destination.is_valid());
            destination = source.take();

            assert_true!(destination.is_valid());
            assert_false!(source.is_valid());
        }

        // See that move-constructor invalidates the source.
        {
            let vmo = make_vmo(page::SIZE);
            assert_true!(vmo.is_ok());
            let vmo = vmo.unwrap();
            stack_pin_init!(let pm = PageMap::new());
            let source = pm.as_ref().make_accessor::<SomeStruct>(vmo.clone(), 0);
            assert_true!(source.is_ok());
            let mut source = source.unwrap();
            assert_true!(source.is_valid());

            let destination = source.take();

            assert_true!(destination.is_valid());
            assert_false!(source.is_valid());
        }
    }

    /// Tests that destroying the last `Accessor` for a page drops its VMO reference.
    #[test]
    fn last_accessor_destroys_vmo_test() {
        // Create a VMO and see that its ref-count is one.
        let vmo = make_vmo(page::SIZE);
        assert_true!(vmo.is_ok());
        let vmo = vmo.unwrap();
        expect_eq!(1, vmo.ref_count().ref_count_debug());

        stack_pin_init!(let pm = PageMap::new());

        // Create an accessor and see that the VMO's ref-count is at least two.  Remember the
        // observed ref-count.
        let a1 = pm.as_ref().make_accessor::<SomeStruct>(vmo.clone(), 0);
        assert_true!(a1.is_ok());
        let mut a1 = a1.unwrap();
        assert_true!(a1.is_valid());
        let observed = vmo.ref_count().ref_count_debug();
        expect_ge!(observed, 2);

        // Create a second accessor and see that the ref-count is unchanged.
        let a2 = pm.as_ref().make_accessor::<SomeStruct>(vmo.clone(), 0);
        assert_true!(a2.is_ok());
        let mut a2 = a2.unwrap();
        assert_true!(a2.is_valid());
        expect_eq!(vmo.ref_count().ref_count_debug(), observed);

        // Destroy one accessor.  Ref-count is unchanged.
        a1 = Accessor::default();
        expect_eq!(vmo.ref_count().ref_count_debug(), observed);

        // Destroy the last accessor.  Ref-count is back to one (our original `vmo` RefPtr).
        a2 = Accessor::default();
        expect_eq!(1, vmo.ref_count().ref_count_debug());

        assert_false!(a1.is_valid() || a2.is_valid());
    }
}
