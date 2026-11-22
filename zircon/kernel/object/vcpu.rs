// Copyright 2026 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

// On x86, VCPUs are implemented in Rust.
#[cfg(target_arch = "x86_64")]
pub use crate::arch_rs::x86::hypervisor::vcpu::Vcpu;

#[cfg(not(target_arch = "x86_64"))]
pub use facade::Vcpu;

#[cfg(not(target_arch = "x86_64"))]
mod facade {
    use crate::object::guest::Guest;
    use core::pin::Pin;
    use fbl::UniquePtr;
    use zx_status::Status;
    use zx_types::{
        zx_info_vcpu_t, zx_port_packet_t, zx_status_t, zx_vaddr_t, zx_vcpu_io_t, zx_vcpu_state_t,
    };

    unsafe extern "C" {
        fn cpp_vcpu_create(
            guest: *mut Guest,
            entry: zx_vaddr_t,
            vcpu_out: *mut *mut Vcpu,
        ) -> zx_status_t;
        fn cpp_vcpu_destroy(vcpu: *mut Vcpu);
        fn cpp_vcpu_enter(vcpu: *mut Vcpu, packet: *mut zx_port_packet_t) -> zx_status_t;
        fn cpp_vcpu_kick(vcpu: *mut Vcpu);
        fn cpp_vcpu_interrupt(vcpu: *mut Vcpu, vector: u32) -> zx_status_t;
        fn cpp_vcpu_read_state(vcpu: *mut Vcpu, vcpu_state: *mut zx_vcpu_state_t) -> zx_status_t;
        fn cpp_vcpu_write_state(vcpu: *mut Vcpu, vcpu_state: *const zx_vcpu_state_t)
        -> zx_status_t;
        fn cpp_vcpu_write_io_state(vcpu: *mut Vcpu, io_state: *const zx_vcpu_io_t) -> zx_status_t;
        fn cpp_vcpu_get_info(vcpu: *const Vcpu, info_out: *mut zx_info_vcpu_t);
    }

    fbl::impl_opaque_unique_facade!(
        /// Facade for the C++ `Vcpu` class.
        pub struct Vcpu,
        cpp_vcpu_destroy,
    );

    impl Vcpu {
        /// Returns a raw pointer to this `Vcpu` for FFI calls without materializing `&Self`
        /// (which would shrink pointer provenance to `size_of::<OpaqueFacade>() == 0`).
        #[inline]
        pub fn as_ffi(this: &Pin<UniquePtr<Self>>) -> *const Self {
            // SAFETY: `Pin<P>` is `#[repr(transparent)]` over `P`, and `UniquePtr::as_ptr` only
            // reads the raw pointer without moving or dereferencing the pinned `UniquePtr<Self>`.
            UniquePtr::as_ptr(unsafe { &*core::ptr::from_ref(this).cast::<UniquePtr<Self>>() })
        }

        /// Returns a mutable raw pointer to this `Vcpu` for FFI calls without materializing
        /// `&Self`.
        #[inline]
        pub fn as_ffi_mut(this: &Pin<UniquePtr<Self>>) -> *mut Self {
            Self::as_ffi(this).cast_mut()
        }

        /// Creates a new `Vcpu` within `guest`, which begins execution at `entry`.
        ///
        /// # Safety
        ///
        /// The caller must ensure that `guest`'s pointee outlives the returned `Vcpu`, which
        /// refers to it, and that this is called in a normal thread context (`!ints_disabled()`,
        /// `!blocking_disallowed()`, and no spinlocks or active `ChainLockTransaction` held on
        /// the calling thread).
        pub unsafe fn create(
            guest: &Pin<UniquePtr<Guest>>,
            entry: zx_vaddr_t,
        ) -> Result<Pin<UniquePtr<Self>>, Status> {
            let mut vcpu_raw = core::ptr::null_mut();
            // SAFETY: `Guest::as_ffi_mut(guest)` is a valid `Guest` pointer for the lifetime of
            // `guest`'s pointee, and `vcpu_raw` is a valid out-pointer to receive the created
            // `Vcpu*` on `ZX_OK`.
            let status = unsafe { cpp_vcpu_create(Guest::as_ffi_mut(guest), entry, &mut vcpu_raw) };
            Status::ok(status)?;
            // SAFETY: `cpp_vcpu_create` succeeded and returned a non-null owning `Vcpu*` released
            // from `ktl::unique_ptr<Vcpu>`. C++ never moves the heap-allocated VCPU.
            Ok(unsafe { Pin::new_unchecked(UniquePtr::from_raw(vcpu_raw)) })
        }

        /// Enters the guest, returning once the guest traps, faults, or is kicked.
        pub fn enter(
            this: &Pin<UniquePtr<Self>>,
            packet: &mut zx_port_packet_t,
        ) -> Result<(), Status> {
            // SAFETY: `Self::as_ffi_mut(this)` is a valid `Vcpu` pointer for the lifetime of
            // `this`, and `packet` is a valid out-pointer to an initialized `zx_port_packet_t`.
            Status::ok(unsafe { cpp_vcpu_enter(Self::as_ffi_mut(this), packet) })
        }

        /// Kicks the VCPU out of the guest, causing a pending or future `enter` to return.
        pub fn kick(this: &Pin<UniquePtr<Self>>) {
            // SAFETY: `Self::as_ffi_mut(this)` is a valid `Vcpu` pointer for the lifetime of
            // `this`.
            unsafe { cpp_vcpu_kick(Self::as_ffi_mut(this)) }
        }

        /// Raises an interrupt on the VCPU.
        pub fn interrupt(this: &Pin<UniquePtr<Self>>, vector: u32) -> Result<(), Status> {
            // SAFETY: `Self::as_ffi_mut(this)` is a valid `Vcpu` pointer for the lifetime of
            // `this`.
            Status::ok(unsafe { cpp_vcpu_interrupt(Self::as_ffi_mut(this), vector) })
        }

        /// Reads the VCPU's register state.
        pub fn read_state(this: &Pin<UniquePtr<Self>>) -> Result<zx_vcpu_state_t, Status> {
            let mut vcpu_state = zx_vcpu_state_t::default();
            // SAFETY: `Self::as_ffi_mut(this)` is a valid `Vcpu` pointer for the lifetime of
            // `this`, and `&mut vcpu_state` is a valid out-pointer to an initialized
            // `zx_vcpu_state_t`.
            Status::ok(unsafe { cpp_vcpu_read_state(Self::as_ffi_mut(this), &mut vcpu_state) })?;
            Ok(vcpu_state)
        }

        /// Writes the VCPU's register state.
        pub fn write_state(
            this: &Pin<UniquePtr<Self>>,
            vcpu_state: &zx_vcpu_state_t,
        ) -> Result<(), Status> {
            // SAFETY: `Self::as_ffi_mut(this)` is a valid `Vcpu` pointer for the lifetime of
            // `this`, and `vcpu_state` is a valid pointer that is only read from.
            Status::ok(unsafe { cpp_vcpu_write_state(Self::as_ffi_mut(this), vcpu_state) })
        }

        /// Writes the VCPU's pending I/O state.
        pub fn write_io_state(
            this: &Pin<UniquePtr<Self>>,
            io_state: &zx_vcpu_io_t,
        ) -> Result<(), Status> {
            // SAFETY: `Self::as_ffi_mut(this)` is a valid `Vcpu` pointer for the lifetime of
            // `this`, and `io_state` is a valid pointer that is only read from.
            Status::ok(unsafe { cpp_vcpu_write_io_state(Self::as_ffi_mut(this), io_state) })
        }

        /// Returns information about the VCPU.
        pub fn get_info(this: &Pin<UniquePtr<Self>>) -> zx_info_vcpu_t {
            let mut info = zx_info_vcpu_t::default();
            // SAFETY: `Self::as_ffi(this)` is a valid `Vcpu` pointer for the lifetime of `this`,
            // and `&mut info` is a valid out-pointer that `cpp_vcpu_get_info` always initializes.
            unsafe { cpp_vcpu_get_info(Self::as_ffi(this), &mut info) };
            info
        }
    }
}
