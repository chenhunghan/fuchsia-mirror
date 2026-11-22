// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use crate::ksync_kernel::TrackedCriticalMutex;
use core::pin::Pin;
use fbl::{Canary, RefPtr, UniquePtr};
use ksync::{KMutex, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{ZX_DEFAULT_GUEST_RIGHTS, ZX_OBJ_TYPE_GUEST, zx_rights_t, zx_vaddr_t};

use super::KernelHandle;
use super::guest::Guest;
use super::guest_dispatcher_ffi::cpp_guest_dispatcher_create;
use super::port_dispatcher::PortDispatcher;
use super::vm_address_region_dispatcher::VmAddressRegionDispatcher;

use object_constants_rs as object_constants;

zr::static_assert_size_and_align!(
    GuestDispatcherState,
    object_constants::kGuestDispatcherStateSize,
    object_constants::kGuestDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_GUEST_CREATE_COUNT, "dispatcher.guest.create", Sum);
define_kcounter!(DISPATCHER_GUEST_DESTROY_COUNT, "dispatcher.guest.destroy", Sum);

/// Internal state storage for `GuestDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct GuestDispatcherState {
    /// Magic canary value validating structural integrity.
    canary: Canary<{ fbl::magic(b"GSTD") }>,

    /// Owned pointer to the underlying `Guest` object (not protected by lock).
    guest: Pin<UniquePtr<Guest>>,

    /// Mutex guarding state operations.
    #[mutex]
    lock: KMutex<TrackedCriticalMutex>,
}

impl GuestDispatcherState {
    /// Initializes the `GuestDispatcherState`.
    pub fn init(
        _dispatcher: *const GuestDispatcher,
        guest: Pin<UniquePtr<Guest>>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_GUEST_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            guest,
            lock <- KMutex::init(),
        })
    }

    /// Returns a reference to the pinned `Guest` pointer.
    pub fn guest(&self) -> &Pin<UniquePtr<Guest>> {
        &self.guest
    }
}

#[pinned_drop]
impl PinnedDrop for GuestDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        DISPATCHER_GUEST_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    /// Dispatcher for hypervisor Guest objects.
    pub struct GuestDispatcher,
    GuestDispatcherState,
    ZX_OBJ_TYPE_GUEST,
    object_constants::kGuestDispatcherStateOffset
);

impl GuestDispatcher {
    /// Returns default rights for a `GuestDispatcher` handle.
    pub const fn default_rights() -> zx_rights_t {
        ZX_DEFAULT_GUEST_RIGHTS
    }

    /// Creates a new `GuestDispatcher` and its root `VmAddressRegionDispatcher`.
    pub fn create(
        options: u32,
    ) -> Result<
        (KernelHandle<Self>, zx_rights_t, KernelHandle<VmAddressRegionDispatcher>, zx_rights_t),
        Status,
    > {
        if options != 0 {
            return Err(Status::INVALID_ARGS);
        }

        let guest = Guest::create()?;
        let vmar = Guest::root_vmar(&guest);

        // SAFETY: The guest is never moved: its pointer is either adopted by
        // `rust_guest_dispatcher_state_init`, which pins it again, or turned back into a
        // `UniquePtr` and dropped in place below if `cpp_guest_dispatcher_create` fails.
        let guest = unsafe { Pin::into_inner_unchecked(guest) };
        let guest_raw = UniquePtr::into_raw(guest);
        // SAFETY: `cpp_guest_dispatcher_create` takes ownership of `guest_raw` and initializes
        // `out` on `ZX_OK`, and leaves ownership with the caller otherwise.
        let new_guest_handle =
            unsafe { KernelHandle::create(|out| cpp_guest_dispatcher_create(guest_raw, out)) }
                .inspect_err(|_| {
                    // SAFETY: `cpp_guest_dispatcher_create` failed, so `guest_raw` is still the
                    // sole owner of the guest released above.
                    drop(unsafe { UniquePtr::from_raw(guest_raw) });
                })?;

        let (vmar_handle, vmar_rights) = VmAddressRegionDispatcher::create(vmar, 0)?;

        Ok((new_guest_handle, Self::default_rights(), vmar_handle, vmar_rights))
    }

    /// Returns a reference to the pinned `Guest` pointer.
    pub fn guest(&self) -> &Pin<UniquePtr<Guest>> {
        self.state().guest()
    }

    /// Sets a trap on the guest within the specified address range.
    pub fn set_trap(
        &self,
        kind: u32,
        addr: zx_vaddr_t,
        len: usize,
        port: Option<RefPtr<PortDispatcher>>,
        key: u64,
    ) -> Result<(), Status> {
        self.state().canary.assert();
        Guest::set_trap(&self.state().guest, kind, addr, len, port, key)
    }
}
