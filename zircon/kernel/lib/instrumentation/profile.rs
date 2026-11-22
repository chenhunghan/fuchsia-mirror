// Copyright 2019 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! LLVM profile data (`llvm-profdata`) VMO publication.

use core::mem::{MaybeUninit, offset_of};
use core::{ptr, slice};

use lazy_init::LazyInit;
use zr::OpaqueBytes;

use crate::top::boot_constants::elf_build_id;
use crate::vm::pmm;
use crate::vm::vm_object::VmObject;
use crate::vm::vm_object_paged::VmObjectPaged;

use super::InstrumentationDataVmo;
use super::debugdata::debugdata_vmo_name;
use super::kernel_mapped_vmo::KernelMappedVmo;
use super::profile_ffi::{
    cpp_llvm_profdata_bounds, cpp_llvm_profdata_copy_live_data, cpp_llvm_profdata_init,
    cpp_llvm_profdata_live_data_for_bounds, cpp_llvm_profdata_new, cpp_llvm_profdata_size_bytes,
    cpp_llvm_profdata_use_live_data, cpp_llvm_profdata_write_fixed_data,
};

const MODULE: &str = "vmzircon";
const SUFFIX: &str = "profraw";

/// Bounds within the raw profile data containing all active live data sections.
/// Matches `LlvmProfdata::LiveDataBounds` in `<lib/llvm-profdata/llvm-profdata.h>`.
#[repr(C)]
#[derive(Copy, Clone, Debug, Default)]
pub(super) struct LiveDataBounds {
    pub offset: usize,
    pub size: usize,
}

zr::static_assert_size_and_align!(LiveDataBounds, 16, 8);
zr::static_assert!(offset_of!(LiveDataBounds, offset) == 0);
zr::static_assert!(offset_of!(LiveDataBounds, size) == 8);

/// Opaque representation of `LlvmProfdata::LiveData` (`3 * sizeof(std::span<std::byte>)`).
#[repr(C, align(8))]
pub(super) struct LiveData(OpaqueBytes<48>);

zr::static_assert_size_and_align!(LiveData, 48, 8);

/// Opaque representation of `LlvmProfdata` (`//src/lib/llvm-profdata`).
#[repr(C, align(8))]
pub(super) struct LlvmProfdata(OpaqueBytes<72>);

zr::static_assert_size_and_align!(LlvmProfdata, 72, 8);

impl LlvmProfdata {
    /// Name associated with the data in the `fuchsia.debugdata` FIDL protocol
    /// (`LlvmProfdata::kDataSinkName`).
    pub const DATA_SINK_NAME: &'static str = "llvm-profile";

    /// Human-readable title used in log messages about the dump (`LlvmProfdata::kAnnounce`).
    pub const ANNOUNCE: &'static str = "LLVM Profile";

    /// Constructs a default-initialized `LlvmProfdata`.
    pub fn new() -> Self {
        let mut out = MaybeUninit::<Self>::uninit();
        // SAFETY: `cpp_llvm_profdata_new` constructs a valid `LlvmProfdata` in `out`.
        unsafe {
            cpp_llvm_profdata_new(&mut out);
            out.assume_init()
        }
    }

    /// Initializes the object based on the current module's own instrumentation data.
    pub fn init(&mut self, build_id: &'static [u8]) {
        // SAFETY: `self` is a valid `LlvmProfdata` and `build_id` is a valid `'static` byte slice.
        unsafe {
            cpp_llvm_profdata_init(self, build_id.as_ptr(), build_id.len());
        }
    }

    /// Returns the size of the data blob to be published, or zero if there is no data to publish.
    pub fn size_bytes(&self) -> usize {
        // SAFETY: `self` is a valid `LlvmProfdata`.
        unsafe { cpp_llvm_profdata_size_bytes(self) }
    }

    /// Writes the fixed metadata into `data`, leaving the live data areas untouched.
    pub fn write_fixed_data(&mut self, data: &mut [u8]) -> LiveData {
        let mut out = MaybeUninit::<LiveData>::uninit();
        // SAFETY: `self` is a valid `LlvmProfdata`, `data` is a writable slice, and
        // `cpp_llvm_profdata_write_fixed_data` initializes `out`.
        unsafe {
            cpp_llvm_profdata_write_fixed_data(self, data.as_mut_ptr(), data.len(), &mut out);
            out.assume_init()
        }
    }

    /// Returns the bounds within the raw profile data that contain all active live data sections.
    pub fn bounds(&self) -> LiveDataBounds {
        let mut out = MaybeUninit::<LiveDataBounds>::uninit();
        // SAFETY: `self` is a valid `LlvmProfdata` and `cpp_llvm_profdata_bounds` initializes
        // `out`.
        unsafe {
            cpp_llvm_profdata_bounds(self, &mut out);
            out.assume_init()
        }
    }

    /// Returns the `LiveData` containing the individual subspans pointing into a memory region
    /// that maps the live data bounds.
    ///
    /// # Safety
    ///
    /// `bounds_data` must point to a writable region of at least `bounds_size` bytes (>=
    /// `self.bounds().size`) that remains valid for the lifetime of the returned `LiveData`, and
    /// `bounds_data` must have the same alignment modulo `LlvmProfdata::kAlign` (8 bytes) as
    /// `self.bounds().offset` (required by `LlvmProfdata::UseData` when the returned `LiveData` is
    /// passed to [`Self::copy_live_data`] or [`Self::use_live_data`]).
    pub unsafe fn live_data_for_bounds(
        &self,
        bounds_data: *mut u8,
        bounds_size: usize,
    ) -> LiveData {
        let mut out = MaybeUninit::<LiveData>::uninit();
        // SAFETY: Caller guarantees `bounds_data` is valid for `bounds_size` bytes and properly
        // aligned modulo `LlvmProfdata::kAlign` (8 bytes).
        unsafe {
            cpp_llvm_profdata_live_data_for_bounds(self, bounds_data, bounds_size, &mut out);
            out.assume_init()
        }
    }

    /// Copies out the current live data values from their link-time locations where they have
    /// accumulated since startup into `data`.
    ///
    /// # Safety
    ///
    /// `data` must refer to writable, properly aligned live data buffers produced by
    /// [`Self::write_fixed_data`] or [`Self::live_data_for_bounds`].
    pub unsafe fn copy_live_data(&mut self, data: &LiveData) {
        // SAFETY: Caller guarantees `data` buffers are valid and writable.
        unsafe {
            cpp_llvm_profdata_copy_live_data(self, data);
        }
    }

    /// Switches the runtime to use `data` for live data updates hereafter.
    ///
    /// # Safety
    ///
    /// `data` must refer to writable, pinned buffers that remain valid for the remaining lifetime
    /// of the kernel (or until another `use_live_data` call).
    pub unsafe fn use_live_data(data: &LiveData) {
        // SAFETY: Caller guarantees `data` buffers remain valid and pinned.
        unsafe {
            cpp_llvm_profdata_use_live_data(data);
        }
    }
}

/// This holds the pinned mapping of the live-updated data.
static PROFDATA_LIVE_DATA: LazyInit<KernelMappedVmo> = LazyInit::uninit();

/// Publishes the LLVM profile data VMO and switches live profile counters to the mapped VMO.
///
/// # Safety
///
/// Must be called at most once, after the VM system is initialized, because it initializes
/// `PROFDATA_LIVE_DATA` and switches live coverage data to the mapped VMO.
pub(super) unsafe fn llvm_profdata_vmo() -> InstrumentationDataVmo {
    let mut profdata = LlvmProfdata::new();
    profdata.init(elf_build_id());
    if profdata.size_bytes() == 0 {
        return InstrumentationDataVmo::default();
    }

    // Create a VMO to hold the whole profdata dump.
    let status = VmObject::round_size(profdata.size_bytes() as u64);
    assert!(status.is_ok());
    let aligned_size = status.unwrap();
    let status = VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, aligned_size);
    assert!(status.is_ok());
    let vmo = VmObjectPaged::into_vm_object(status.unwrap());

    // First fill in just the fixed data, by mapping the whole VMO into the
    // kernel address space.  Then let the mapping and pinning be cleaned up,
    // since we don't need the whole thing mapped into the kernel at runtime.
    {
        let mut mapped_vmo = KernelMappedVmo::new();
        let status = mapped_vmo.init(vmo.clone(), 0, profdata.size_bytes());
        assert!(status.is_ok());
        // SAFETY: `mapped_vmo.init` succeeded, mapping `mapped_vmo.size_locking()` writable bytes
        // at `mapped_vmo.base_locking()`, which has no other aliases in this scope.
        let mapped_data = unsafe {
            slice::from_raw_parts_mut(
                ptr::with_exposed_provenance_mut::<u8>(mapped_vmo.base_locking().0),
                mapped_vmo.size_locking(),
            )
        };
        let _ = profdata.write_fixed_data(mapped_data);
    }

    // Now map in just the pages holding the live data.  This mapping will be
    // kept alive permanently so the live data can be updated through it.
    let bounds: LiveDataBounds = profdata.bounds();
    let map_offset = page::round_down(bounds.offset);
    let map_size = page::round_up(bounds.offset + bounds.size) - map_offset;

    let mut live_vmo = KernelMappedVmo::new();
    let status = live_vmo.init(vmo, map_offset, map_size);
    assert!(status.is_ok());
    // SAFETY: `PROFDATA_LIVE_DATA` is private to this module and only initialized here, and the
    // caller guarantees `llvm_profdata_vmo` is called at most once.
    unsafe {
        PROFDATA_LIVE_DATA.init(live_vmo);
    }
    let live_vmo = PROFDATA_LIVE_DATA.get();

    let live_base = bounds.offset - map_offset + live_vmo.base_locking().0;
    // SAFETY: `PROFDATA_LIVE_DATA` maps `[map_offset, map_offset + map_size)` of the VMO, which
    // covers `[bounds.offset, bounds.offset + bounds.size)`. Because `map_offset` and
    // `live_vmo.base_locking()` are both page-aligned (a multiple of 8), `live_base` has the same
    // alignment modulo 8 as `bounds.offset`.
    let live_data = unsafe {
        profdata
            .live_data_for_bounds(ptr::with_exposed_provenance_mut::<u8>(live_base), bounds.size)
    };

    // Live data up to this point have collected in global variable space.
    // Copy those data into the mapped VMO data.
    // SAFETY: `live_data` points into the permanently mapped `PROFDATA_LIVE_DATA` region.
    unsafe {
        profdata.copy_live_data(&live_data);
    }

    // Switch instrumented code over to updating the mapped VMO data in place.
    // From this point on, the kernel's VMO mapping is used by all instrumented
    // code and must be kept valid and pinned.
    //
    // TODO(mcgrathr): We could theoretically decommit the global data pages
    // after this to recover that RAM.  That part of the kernel's global data
    // area should never be accessed again.
    // SAFETY: `live_data` points into `PROFDATA_LIVE_DATA`, which remains valid and pinned for
    // the lifetime of the kernel.
    unsafe {
        LlvmProfdata::use_live_data(&live_data);
    }

    let vmo_name =
        debugdata_vmo_name(LlvmProfdata::DATA_SINK_NAME, MODULE, SUFFIX, /*is_static=*/ false);

    InstrumentationDataVmo {
        announce: LlvmProfdata::ANNOUNCE,
        sink_name: LlvmProfdata::DATA_SINK_NAME,
        handle: live_vmo.publish(&vmo_name, profdata.size_bytes()),
        ..InstrumentationDataVmo::default()
    }
}
