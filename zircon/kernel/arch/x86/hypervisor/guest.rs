// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Layout-compatible Rust definitions of the C++ `Guest` and `VmxPage` classes declared in
//! `<arch/hypervisor.h>`.
//!
//! The layouts are asserted at compile time against the definitions bindgen generates from the C++
//! header (the `guest_bindings` crate).

use core::ffi::c_void;
use core::mem::{align_of, offset_of, size_of};
use core::ptr::NonNull;
use guest_bindings as bindings;
use id_allocator::IdAllocator;
use zr::static_assert;

use crate::hypervisor::aspace::GuestPhysicalAspace;
use crate::hypervisor::trap_map::TrapMap;
use crate::kernel::types::PAddr;
use crate::ksync_kernel::TrackedMutex;
use crate::vm::page::VmPage;

/// Evaluates to `$f::<F>()`, where `F` is the type of the field `$field` of the struct `$ty`.
macro_rules! field_type_property {
    ($f:ident, $ty:ty, $field:ident) => {{
        const fn pointee_property<T>(_: *const T) -> usize {
            $f::<T>()
        }
        let uninit = core::mem::MaybeUninit::<$ty>::uninit();
        // SAFETY: `uninit.as_ptr()` is a non-null, aligned pointer to a live local, so projecting
        // to one of its fields stays in bounds. Only the field's address is computed; no reference
        // is created and nothing is read.
        pointee_property(unsafe { &raw const (*uninit.as_ptr()).$field })
    }};
}

/// Asserts that the field `$field` of the Rust struct `$ty` has the same offset, size and
/// alignment as the field `$cpp_field` of the bindgen-generated struct `$cpp_ty`.
macro_rules! static_assert_field_layout {
    ($ty:ty, $field:ident, $cpp_ty:ty, $cpp_field:ident) => {
        static_assert!(offset_of!($ty, $field) == offset_of!($cpp_ty, $cpp_field));
        static_assert!(
            field_type_property!(size_of, $ty, $field)
                == field_type_property!(size_of, $cpp_ty, $cpp_field)
        );
        static_assert!(
            field_type_property!(align_of, $ty, $field)
                == field_type_property!(align_of, $cpp_ty, $cpp_field)
        );
    };
}

/// Maximum VCPUs per guest.
pub const MAX_GUEST_VCPUS: usize = 64;

static_assert!(MAX_GUEST_VCPUS == bindings::Guest_kMaxGuestVcpus);

/// VPIDs of a guest's VCPUs are allocated in the range [`MIN_VPID`, `MAX_VPID`). VPID 0 is
/// reserved for the host.
const MIN_VPID: usize = 1;
const MAX_VPID: usize = MAX_GUEST_VCPUS + 1;
const VPID_BITMAP_WORDS: usize = MAX_VPID.div_ceil(usize::BITS as usize);

/// Allocates the VPIDs of a guest's VCPUs.
///
/// The C++ `VpidAllocator` in `<arch/hypervisor.h>` is a facade over this type.
pub type VpidAllocator = IdAllocator<u16, TrackedMutex, MAX_VPID, MIN_VPID, VPID_BITMAP_WORDS>;

// `VpidAllocator::init` only fails if the largest VPID does not fit in a `u16`.
static_assert!(MAX_VPID - 1 <= u16::MAX as usize);

// The C++ `VpidAllocator` facade stores the Rust `VpidAllocator` in place.
static_assert!(size_of::<VpidAllocator>() == size_of::<bindings::VpidAllocator>());
static_assert!(align_of::<VpidAllocator>() == align_of::<bindings::VpidAllocator>());

/// A page of memory used by VMX, such as an MSR bitmap or a VMCS.
///
/// This mirrors the layout of the C++ `VmxPage` (and its `hypervisor::Page` base) in
/// `<arch/hypervisor.h>`.
// TODO(https://fxbug.dev/576430432): Compose a Rust `hypervisor::Page` once `hypervisor/page.cc` is
// ported, rather than duplicating its fields here.
#[repr(C)]
pub struct VmxPage {
    /// The allocated page, or `None` if no page has been allocated.
    page: Option<NonNull<VmPage>>,
    /// The physical address of `page`, or 0 if no page has been allocated.
    pa: PAddr,
}

static_assert!(size_of::<VmxPage>() == size_of::<bindings::VmxPage>());
static_assert!(align_of::<VmxPage>() == align_of::<bindings::VmxPage>());
static_assert!(offset_of!(bindings::VmxPage, _base) == 0);
static_assert_field_layout!(VmxPage, page, bindings::hypervisor_Page, page_);
static_assert_field_layout!(VmxPage, pa, bindings::hypervisor_Page, pa_);

/// Represents a guest within the hypervisor.
///
/// This mirrors the layout of the C++ `Guest` in `<arch/hypervisor.h>`. It is a layout mirror only:
/// it is never constructed, moved or dropped from Rust.
#[repr(C)]
pub struct Guest {
    /// The C++ vtable pointer of `Guest`, which has a virtual destructor.
    vptr: *const c_void,
    gpa: GuestPhysicalAspace,
    traps: TrapMap,
    vpid_allocator: VpidAllocator,
    msr_bitmaps_page: VmxPage,
}

static_assert!(size_of::<Guest>() == size_of::<bindings::Guest>());
static_assert!(align_of::<Guest>() == align_of::<bindings::Guest>());
static_assert_field_layout!(Guest, vptr, bindings::Guest, vtable_);
static_assert_field_layout!(Guest, gpa, bindings::Guest, gpa_);
static_assert_field_layout!(Guest, traps, bindings::Guest, traps_);
static_assert_field_layout!(Guest, vpid_allocator, bindings::Guest, vpid_allocator_);
static_assert_field_layout!(Guest, msr_bitmaps_page, bindings::Guest, msr_bitmaps_page_);
