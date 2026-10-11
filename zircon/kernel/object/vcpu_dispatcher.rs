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

    /// The guest this VCPU belongs to. Keeps the guest alive for the VCPU's lifetime.
    guest_dispatcher: RefPtr<GuestDispatcher>,

    /// Owned pointer to the underlying C++ `Vcpu` object (not protected by lock).
    vcpu: UniquePtr<Vcpu>,

    /// Mutex guarding state operations.
    #[mutex]
    lock: KMutex<RawCriticalMutex>,
}

impl VcpuDispatcherState {
    /// Initializes the `VcpuDispatcherState`.
    pub fn init(
        _dispatcher: *const VcpuDispatcher,
        guest_dispatcher: RefPtr<GuestDispatcher>,
        vcpu: UniquePtr<Vcpu>,
    ) -> impl PinInit<Self, core::convert::Infallible> {
        DISPATCHER_VCPU_CREATE_COUNT.add(1);
        pin_init!(Self {
            canary: Canary::new(),
            guest_dispatcher,
            vcpu,
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
        let vcpu = Vcpu::create(guest_dispatcher.guest(), entry)?;

        // SAFETY: `RefPtr::into_raw` and `UniquePtr::into_raw` transfer ownership of the guest
        // reference and the `Vcpu` to `cpp_vcpu_dispatcher_create`, which adopts them into
        // `fbl::RefPtr<GuestDispatcher>` and `ktl::unique_ptr<Vcpu>`, and initializes `out` on
        // `ZX_OK`.
        let handle = unsafe {
            KernelHandle::create(|out| {
                cpp_vcpu_dispatcher_create(
                    RefPtr::into_raw(guest_dispatcher).cast_mut(),
                    UniquePtr::into_raw(vcpu),
                    out,
                )
            })
        }?;

        Ok((handle, Self::default_rights()))
    }

    /// Enters the guest, returning once the guest traps, faults, or is kicked.
    pub fn enter(&self, packet: &mut zx_port_packet_t) -> Result<(), Status> {
        self.state().canary.assert();
        self.state().vcpu.enter(packet)
    }

    /// Kicks the VCPU out of the guest.
    pub fn kick(&self) {
        self.state().canary.assert();
        self.state().vcpu.kick()
    }

    /// Raises an interrupt on the VCPU.
    pub fn interrupt(&self, vector: u32) -> Result<(), Status> {
        self.state().canary.assert();
        self.state().vcpu.interrupt(vector)
    }

    /// Reads the VCPU's register state.
    pub fn read_state(&self) -> Result<zx_vcpu_state_t, Status> {
        self.state().canary.assert();
        self.state().vcpu.read_state()
    }

    /// Writes the VCPU's register state.
    pub fn write_state(&self, vcpu_state: &zx_vcpu_state_t) -> Result<(), Status> {
        self.state().canary.assert();
        self.state().vcpu.write_state(vcpu_state)
    }

    /// Writes the VCPU's pending I/O state.
    pub fn write_io_state(&self, io_state: &zx_vcpu_io_t) -> Result<(), Status> {
        self.state().canary.assert();
        self.state().vcpu.write_io_state(io_state)
    }

    /// Returns information about the VCPU.
    pub fn get_info(&self) -> zx_info_vcpu_t {
        self.state().canary.assert();
        self.state().vcpu.get_info()
    }
}
