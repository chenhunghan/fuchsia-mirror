// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

use crate::arch_rs::ops::num_spinlocks_held;
use crate::arch_rs::{blocking_disallowed, ints_disabled};
use crate::counters::define_kcounter;
use crate::ksync_kernel::TrackedCriticalMutex;
use core::pin::Pin;
use fbl::{Canary, RefPtr, UniquePtr};
use ksync::{KMutex, guarded};
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zx_status::Status;
use zx_types::{
    ZX_OBJ_TYPE_VCPU, ZX_RIGHT_DUPLICATE, ZX_RIGHT_EXECUTE, ZX_RIGHT_INSPECT, ZX_RIGHT_READ,
    ZX_RIGHT_SIGNAL, ZX_RIGHT_TRANSFER, ZX_RIGHT_WAIT, ZX_RIGHT_WRITE, zx_info_vcpu_t,
    zx_port_packet_t, zx_rights_t, zx_vaddr_t, zx_vcpu_io_t, zx_vcpu_state_t,
};

use super::KernelHandle;
use super::guest_dispatcher::GuestDispatcher;
use super::vcpu::Vcpu;
use super::vcpu_dispatcher_ffi::cpp_vcpu_dispatcher_create;

use object_constants_rs as object_constants;

/// Default rights assigned to a VcpuDispatcher handle (`ZX_DEFAULT_VCPU_RIGHTS`).
pub const DEFAULT_RIGHTS: zx_rights_t = ZX_RIGHT_TRANSFER
    | ZX_RIGHT_DUPLICATE
    | ZX_RIGHT_WAIT
    | ZX_RIGHT_INSPECT
    | ZX_RIGHT_READ
    | ZX_RIGHT_WRITE
    | ZX_RIGHT_EXECUTE
    | ZX_RIGHT_SIGNAL;

zr::static_assert_size_and_align!(
    VcpuDispatcherState,
    object_constants::kVcpuDispatcherStateSize,
    object_constants::kVcpuDispatcherStateAlign,
);

define_kcounter!(DISPATCHER_VCPU_CREATE_COUNT, "dispatcher.vcpu.create", Sum);
define_kcounter!(DISPATCHER_VCPU_DESTROY_COUNT, "dispatcher.vcpu.destroy", Sum);

/// Internal state storage for `VcpuDispatcher`.
#[guarded]
#[pin_data(PinnedDrop)]
#[repr(C)]
pub struct VcpuDispatcherState {
    /// Magic canary value validating structural integrity.
    canary: Canary<{ fbl::magic(b"VCPU") }>,

    /// Owned pointer to the underlying `Vcpu` object (not protected by lock).
    ///
    /// Declared before `guest_dispatcher` so that it is dropped first, as it refers to the guest.
    vcpu: Pin<UniquePtr<Vcpu>>,

    /// The guest this VCPU belongs to. Keeps the guest alive for the VCPU's lifetime.
    guest_dispatcher: RefPtr<GuestDispatcher>,

    /// Mutex guarding state operations.
    #[mutex]
    lock: KMutex<TrackedCriticalMutex>,
}

// Fields are dropped in declaration order, which for a `repr(C)` struct is also offset order, so
// this checks that `vcpu` is dropped before `guest_dispatcher`.
zr::static_assert!(
    core::mem::offset_of!(VcpuDispatcherState, vcpu)
        < core::mem::offset_of!(VcpuDispatcherState, guest_dispatcher)
);

impl VcpuDispatcherState {
    /// Initializes the `VcpuDispatcherState`.
    pub fn init(
        _dispatcher: *const VcpuDispatcher,
        guest_dispatcher: RefPtr<GuestDispatcher>,
        vcpu: Pin<UniquePtr<Vcpu>>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_VCPU_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            vcpu,
            guest_dispatcher,
            lock <- KMutex::init(),
        })
    }
}

#[pinned_drop]
impl PinnedDrop for VcpuDispatcherState {
    fn drop(self: core::pin::Pin<&mut Self>) {
        DISPATCHER_VCPU_DESTROY_COUNT.add(1);
    }
}

crate::object::dispatcher::impl_dispatcher_facade_with_state!(
    /// Dispatcher for hypervisor VCPU objects.
    pub struct VcpuDispatcher,
    VcpuDispatcherState,
    ZX_OBJ_TYPE_VCPU,
    object_constants::kVcpuDispatcherStateOffset
);

impl VcpuDispatcher {
    /// Returns default rights for a `VcpuDispatcher` handle.
    pub const fn default_rights() -> zx_rights_t {
        DEFAULT_RIGHTS
    }

    /// Creates a new `VcpuDispatcher` within `guest_dispatcher`, entering at `entry`.
    pub fn create(
        guest_dispatcher: RefPtr<GuestDispatcher>,
        entry: zx_vaddr_t,
    ) -> Result<(KernelHandle<Self>, zx_rights_t), Status> {
        assert!(!ints_disabled() && !blocking_disallowed() && num_spinlocks_held() == 0);
        // SAFETY: The guest outlives the VCPU, as the `VcpuDispatcherState` that owns the VCPU
        // holds a reference to the guest dispatcher, which owns the guest, and drops the VCPU
        // first. If creating the dispatcher fails, the VCPU is dropped before the reference. The
        // assertion above ensures normal thread context with interrupts and blocking enabled and no
        // spinlocks or active `ChainLockTransaction` held.
        let vcpu = unsafe { Vcpu::create(guest_dispatcher.guest(), entry) }?;

        // SAFETY: The VCPU stays pinned, as ownership is only passed through C++ to
        // `rust_vcpu_dispatcher_state_init`, which pins it again, or returned below.
        let vcpu_raw = UniquePtr::into_raw(unsafe { Pin::into_inner_unchecked(vcpu) });
        let guest_dispatcher_raw = RefPtr::into_raw(guest_dispatcher).cast_mut();
        // SAFETY: `cpp_vcpu_dispatcher_create` takes ownership of the guest reference and the
        // `Vcpu`, and initializes `out`, on `ZX_OK`, and leaves their ownership with the caller
        // otherwise.
        let handle = unsafe {
            KernelHandle::create(|out| {
                cpp_vcpu_dispatcher_create(guest_dispatcher_raw, vcpu_raw, out)
            })
        }
        .inspect_err(|_| {
            // SAFETY: `cpp_vcpu_dispatcher_create` failed, so these are still the sole owners of
            // the `Vcpu` and the guest reference released above. The `Vcpu` is dropped first.
            unsafe {
                drop(UniquePtr::from_raw(vcpu_raw));
                drop(RefPtr::from_raw(guest_dispatcher_raw));
            }
        })?;

        Ok((handle, Self::default_rights()))
    }

    /// Enters the guest, returning once the guest traps, faults, or is kicked.
    pub fn enter(&self, packet: &mut zx_port_packet_t) -> Result<(), Status> {
        self.state().canary.assert();
        Vcpu::enter(&self.state().vcpu, packet)
    }

    /// Kicks the VCPU out of the guest.
    pub fn kick(&self) {
        self.state().canary.assert();
        Vcpu::kick(&self.state().vcpu)
    }

    /// Raises an interrupt on the VCPU.
    pub fn interrupt(&self, vector: u32) -> Result<(), Status> {
        self.state().canary.assert();
        Vcpu::interrupt(&self.state().vcpu, vector)
    }

    /// Reads the VCPU's register state.
    pub fn read_state(&self) -> Result<zx_vcpu_state_t, Status> {
        self.state().canary.assert();
        Vcpu::read_state(&self.state().vcpu)
    }

    /// Writes the VCPU's register state.
    pub fn write_state(&self, vcpu_state: &zx_vcpu_state_t) -> Result<(), Status> {
        self.state().canary.assert();
        Vcpu::write_state(&self.state().vcpu, vcpu_state)
    }

    /// Writes the VCPU's pending I/O state.
    pub fn write_io_state(&self, io_state: &zx_vcpu_io_t) -> Result<(), Status> {
        self.state().canary.assert();
        Vcpu::write_io_state(&self.state().vcpu, io_state)
    }

    /// Returns information about the VCPU.
    pub fn get_info(&self) -> zx_info_vcpu_t {
        self.state().canary.assert();
        Vcpu::get_info(&self.state().vcpu)
    }
}
