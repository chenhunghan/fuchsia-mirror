// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::counters::define_kcounter;
use fbl::{Canary, RefPtr, UniquePtr};
use ksync::{KMutex, RawCriticalMutex, guarded};
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

    /// Owned pointer to the underlying C++ `Guest` object (not protected by lock).
    guest: UniquePtr<Guest>,

    /// Mutex guarding state operations.
    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

impl GuestDispatcherState {
    /// Initializes the `GuestDispatcherState`.
    pub fn init(
        _dispatcher: *const GuestDispatcher,
        guest: UniquePtr<Guest>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_GUEST_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            guest,
            lock <- KMutex::init(),
        })
    }

    /// Returns a reference to the underlying `Guest` facade object.
    pub fn guest(&self) -> &Guest {
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
        let vmar = guest.root_vmar();

        // SAFETY: `UniquePtr::into_raw(guest)` transfers sole ownership of `guest` to
        // `cpp_guest_dispatcher_create`, which adopts it into `ktl::unique_ptr<Guest>`, and
        // `cpp_guest_dispatcher_create` initializes `out` on `ZX_OK`.
        let new_guest_handle = unsafe {
            KernelHandle::create(|out| cpp_guest_dispatcher_create(UniquePtr::into_raw(guest), out))
        }?;

        let (vmar_handle, vmar_rights) = VmAddressRegionDispatcher::create(vmar, 0)?;

        Ok((new_guest_handle, Self::default_rights(), vmar_handle, vmar_rights))
    }

    /// Returns a reference to the underlying `Guest` facade object.
    pub fn guest(&self) -> &Guest {
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
        self.state().guest.set_trap(kind, addr, len, port, key)
    }
}
