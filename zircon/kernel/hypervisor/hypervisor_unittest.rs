// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Hypervisor unit tests and shared test helpers.

use fbl::RefPtr;
use zx_status::Status;
use zx_types::zx_gpaddr_t;

#[cfg(target_arch = "aarch64")]
use crate::arch_rs::arm64::{arm64_get_boot_el, arm64_mmu_features_vhe};
use crate::vm::arch_vm_aspace::{
    ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE, ArchMmuFlags,
};
use crate::vm::pmm;
use crate::vm::vm_address_region::{VmAddressRegion, flag as vmar_flag};
use crate::vm::vm_object_paged::VmObjectPaged;

/// Default read/write/execute stage-2 MMU permissions used by hypervisor unit tests.
pub const MMU_FLAGS: ArchMmuFlags =
    ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_PERM_EXECUTE;

/// Returns whether the current CPU and boot configuration support the hypervisor.
///
/// # Safety-usable invariant
///
/// On `aarch64`, returning `true` guarantees that `arm64_feature_init` has completed and the
/// kernel is executing at EL2 (`CurrentEL.EL == 2`) with `FEAT_VHE` enabled (`HCR_EL2.E2H == 1`).
pub fn hypervisor_supported() -> bool {
    #[cfg(target_arch = "x86_64")]
    {
        true
    }
    #[cfg(target_arch = "aarch64")]
    {
        arm64_get_boot_el() >= 2 && arm64_mmu_features_vhe()
    }
    #[cfg(not(any(target_arch = "x86_64", target_arch = "aarch64")))]
    {
        false
    }
}

/// Creates a paged VMO of `vmo_size` bytes for hypervisor unit tests.
pub fn create_vmo(vmo_size: usize) -> Result<RefPtr<VmObjectPaged>, Status> {
    VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, vmo_size as u64)
}

/// Commits all pages of `vmo`.
pub fn commit_vmo(vmo: &VmObjectPaged) -> Result<(), Status> {
    vmo.commit_range(0, vmo.size())
}

/// Maps `vmo` at `addr` inside `vmar` with `mmu_flags` without overwriting existing mappings.
pub fn create_mapping(
    vmar: &VmAddressRegion,
    vmo: RefPtr<VmObjectPaged>,
    addr: zx_gpaddr_t,
    mmu_flags: ArchMmuFlags,
) -> Result<(), Status> {
    let vmo_size = vmo.size() as usize;
    vmar.create_vm_mapping_no_overwrite(
        addr,
        vmo_size,
        /* align_pow2 */ 0,
        vmar_flag::SPECIFIC,
        VmObjectPaged::into_vm_object(vmo),
        /* vmo_offset */ 0,
        mmu_flags,
    )
    .map(|_| ())
}

/// Hypervisor unit tests.
// On x86, guests are implemented in Rust, and the `x86_hypervisor_guest` suite tests this.
#[cfg(not(target_arch = "x86_64"))]
#[unittest::suite(name = "hypervisor")]
mod tests {
    use core::ffi::c_void;
    use core::pin::Pin;
    use fbl::UniquePtr;
    use pin_init::stack_pin_init;
    use unittest::{assert_ok, expect_eq, expect_err, unwrap_ok};
    use zx_status::Status;
    use zx_types::{ZX_GUEST_TRAP_MEM, zx_gpaddr_t, zx_packet_type_t, zx_port_packet_t};

    use super::{MMU_FLAGS, commit_vmo, create_mapping, create_vmo, hypervisor_supported};
    #[cfg(target_arch = "aarch64")]
    use crate::arch_rs::arm64::{arm64_get_boot_el, arm64_mmu_features_pan, arm64_read_pan};
    use crate::kernel::deadline::{Deadline, DurationMono, TimerSlack};
    use crate::kernel::thread::{
        preempt_clear_timeslice_extension, preempt_set_pending, preempt_set_timeslice_extension,
    };
    use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
    use crate::object::{Guest, Vcpu};
    #[cfg(target_arch = "aarch64")]
    use libarch::arm64::DAIF;

    /// Verifies that a VCPU runs guest code until the guest accesses a memory trap.
    #[test]
    fn vcpu_enter() {
        if !hypervisor_supported() {
            return true;
        }

        let guest = match Guest::create() {
            Err(Status::NOT_SUPPORTED) => return true,
            guest => unwrap_ok!(guest, "Failed to create Guest"),
        };

        // Map 3 pages at GPA 0x0, with guest entry code in the third page ([0x2000, 0x3000)).
        const VMO_PAGES: usize = 3;
        const ENTRY_ADDR: zx_gpaddr_t = page::SIZE * 2;
        const TRAP_ADDR: zx_gpaddr_t = page::SIZE * 3;
        const TRAP_KEY: u64 = 0x1234;

        let vmo = unwrap_ok!(create_vmo(page::SIZE * VMO_PAGES), "Failed to create VMO");
        assert_ok!(commit_vmo(&vmo), "Failed to commit VMO");

        #[cfg(target_arch = "aarch64")]
        let guest_code: &[u8] = {
            // Guest code at ENTRY_ADDR (0x2000):
            //   mrs x1, CurrentEL          (0xd5384241)
            //   movz x3, #0x30, lsl #16    (0xd2a00603)  // CPACR_EL1.FPEN = 0b11
            //   msr cpacr_el1, x3          (0xd5181043)
            //   isb                        (0xd5033fdf)
            //   fmov d0, #1.00000000       (0x1e6e1000)
            //   fmov x2, d0                (0x9e660002)
            //   movz x0, #0x3000           (0xd286_0000)
            //   str xzr, [x0]              (0xf900001f)
            const GUEST_CODE: [[u8; 4]; 8] = [
                0xd538_4241u32.to_ne_bytes(),
                0xd2a0_0603u32.to_ne_bytes(),
                0xd518_1043u32.to_ne_bytes(),
                0xd503_3fdfu32.to_ne_bytes(),
                0x1e6e_1000u32.to_ne_bytes(),
                0x9e66_0002u32.to_ne_bytes(),
                0xd286_0000u32.to_ne_bytes(),
                0xf900_001fu32.to_ne_bytes(),
            ];
            GUEST_CODE.as_flattened()
        };
        #[cfg(not(target_arch = "aarch64"))]
        let guest_code: &[u8] = &[];
        assert_ok!(vmo.write(ENTRY_ADDR as u64, guest_code), "Failed to write guest code to VMO");

        assert_ok!(
            create_mapping(&Guest::root_vmar(&guest), vmo, 0, MMU_FLAGS),
            "Failed to map guest code into GPA"
        );

        assert_ok!(
            Guest::set_trap(&guest, ZX_GUEST_TRAP_MEM, TRAP_ADDR, page::SIZE, None, TRAP_KEY),
            "Failed to set trap"
        );

        // SAFETY: `vcpu` is declared after `guest`, so it is dropped first.
        let vcpu = match unsafe { Vcpu::create(&guest, ENTRY_ADDR) } {
            Err(Status::NOT_SUPPORTED) => return true,
            vcpu => unwrap_ok!(vcpu, "Failed to create Vcpu"),
        };

        #[cfg(target_arch = "aarch64")]
        let (host_daif_before, has_pan, host_pan_before) = {
            let daif = u64::from(DAIF.read());
            let has_pan = arm64_mmu_features_pan();
            // SAFETY: `has_pan` (`arm64_mmu_features_pan()`) confirmed `FEAT_PAN` is implemented,
            // and `hypervisor_supported()` confirmed the kernel is executing at EL2
            // (`CurrentEL.EL >= 2`).
            let pan = if has_pan { unsafe { arm64_read_pan() } } else { 0 };
            (daif, has_pan, pan)
        };

        let mut packet = zx_port_packet_t::default();
        assert_ok!(Vcpu::enter(&vcpu, &mut packet), "Failed to enter Vcpu");
        expect_eq!(zx_packet_type_t::ZX_PKT_TYPE_GUEST_MEM, packet.packet_type);
        expect_eq!(TRAP_KEY, packet.key);
        // `zx_packet_guest_mem_t::addr` is the first field of the packet's union.
        let [b0, b1, b2, b3, b4, b5, b6, b7, ..] = packet.union;
        let guest_mem_addr = u64::from_ne_bytes([b0, b1, b2, b3, b4, b5, b6, b7]);
        expect_eq!(TRAP_ADDR as u64, guest_mem_addr);

        let vcpu_state = unwrap_ok!(Vcpu::read_state(&vcpu), "Failed to read Vcpu state");

        #[cfg(target_arch = "aarch64")]
        {
            expect_eq!(
                host_daif_before,
                u64::from(DAIF.read()),
                "Expected host DAIF to be preserved"
            );
            if has_pan {
                // SAFETY: `has_pan` (`arm64_mmu_features_pan()`) confirmed `FEAT_PAN` is
                // implemented, and the host kernel continues executing in kernel mode
                // (`CurrentEL.EL >= 1`).
                let host_pan_after = unsafe { arm64_read_pan() };
                expect_eq!(host_pan_before, host_pan_after, "Expected host PAN to be preserved");
            }

            // Verify the guest executed at EL1 (CurrentEL == 1 << 2).
            expect_eq!(1u64 << 2, vcpu_state.x[1], "Expected guest to execute at EL1");
            // Verify lazy FP trap and state save/restore (IEEE-754 1.0 == 0x3ff0000000000000).
            expect_eq!(
                0x3ff0_0000_0000_0000u64,
                vcpu_state.x[2],
                "Expected guest FP state to be preserved"
            );
            // Verify the host kernel is still executing at EL2 with FEAT_VHE.
            expect_eq!(2u64, arm64_get_boot_el(), "Expected host kernel to remain at EL2");
        }
        #[cfg(not(target_arch = "aarch64"))]
        let _ = vcpu_state;
    }

    /// Verifies that VM exit handles external interrupts before re-enabling preemption.
    #[test]
    fn vcpu_enter_preemption() {
        if !hypervisor_supported() {
            return true;
        }

        let guest = match Guest::create() {
            Err(Status::NOT_SUPPORTED) => return true,
            guest => unwrap_ok!(guest, "Failed to create Guest"),
        };

        // Map 3 pages at GPA 0x0, with guest entry code in the third page ([0x2000, 0x3000)).
        const VMO_PAGES: usize = 3;
        const ENTRY_ADDR: zx_gpaddr_t = page::SIZE * 2;

        let vmo = unwrap_ok!(create_vmo(page::SIZE * VMO_PAGES), "Failed to create VMO");
        assert_ok!(commit_vmo(&vmo), "Failed to commit VMO");

        #[cfg(target_arch = "aarch64")]
        let guest_code: &[u8] = {
            // Guest code at ENTRY_ADDR (0x2000):
            //   b .                        (0x14000000)
            const GUEST_CODE: [u8; 4] = 0x1400_0000u32.to_ne_bytes();
            &GUEST_CODE
        };
        #[cfg(not(target_arch = "aarch64"))]
        let guest_code: &[u8] = &[];
        assert_ok!(vmo.write(ENTRY_ADDR as u64, guest_code), "Failed to write guest code to VMO");

        assert_ok!(
            create_mapping(&Guest::root_vmar(&guest), vmo, 0, MMU_FLAGS),
            "Failed to map guest code into GPA"
        );

        // SAFETY: `vcpu` is declared after `guest`, so it is dropped first.
        let vcpu = match unsafe { Vcpu::create(&guest, ENTRY_ADDR) } {
            Err(Status::NOT_SUPPORTED) => return true,
            vcpu => unwrap_ok!(vcpu, "Failed to create Vcpu"),
        };

        // Install a 1ms timeslice extension. When timer1 fires at 1ms during guest
        // execution, it calls `preempt_set_pending` and activates the extension without
        // flushing, so the VCPU re-enters the guest with a pending preemption. When
        // timer2 fires at 3ms after the extension has expired, the VM exit handler
        // must dispatch the interrupt and issue EOI before re-enabling preemption and
        // flushing.
        preempt_set_timeslice_extension(DurationMono::from_millis(1));

        /// Timer callback that marks preemption pending on the current CPU.
        ///
        /// # Safety
        ///
        /// This function has no safety preconditions; it is `unsafe` only to match the [`Timer`]
        /// callback signature.
        unsafe extern "C" fn set_preempt_pending(_: *mut Timer, _: i64, _: *mut c_void) {
            preempt_set_pending();
        }

        /// Timer callback that marks preemption pending and kicks the target [`Vcpu`].
        ///
        /// # Safety
        ///
        /// The caller must ensure `arg` is non-null, properly aligned for `Pin<UniquePtr<Vcpu>>`,
        /// and points to a valid, initialized `Pin<UniquePtr<Vcpu>>` that outlives the callback
        /// without exclusive aliases.
        unsafe extern "C" fn set_preempt_pending_and_kick(_: *mut Timer, _: i64, arg: *mut c_void) {
            preempt_set_pending();
            // SAFETY: The caller guarantees `arg` is non-null, aligned, and points to a valid
            // `Pin<UniquePtr<Vcpu>>` that outlives the callback without exclusive aliases.
            Vcpu::kick(unsafe { &*arg.cast::<Pin<UniquePtr<Vcpu>>>() });
        }

        stack_pin_init!(let timer1 = Timer::init(ZX_CLOCK_MONOTONIC));
        stack_pin_init!(let timer2 = Timer::init(ZX_CLOCK_MONOTONIC));

        // SAFETY: `timer1` is stack-pinned and canceled before going out of scope, is not modified
        // concurrently, and `set_preempt_pending` has no pointer/argument preconditions.
        unsafe {
            timer1.as_ref().set_deadline(
                &Deadline::after_mono(DurationMono::from_millis(1), TimerSlack::none()),
                set_preempt_pending,
                core::ptr::null_mut(),
            );
        }
        // SAFETY: `timer2` is stack-pinned and canceled before either `timer2` or `vcpu` goes out
        // of scope, is not modified concurrently, and `core::ptr::from_ref(&vcpu)` returns a
        // non-null, aligned pointer to an initialized `Pin<UniquePtr<Vcpu>>` with no exclusive
        // references created while the timer is active.
        unsafe {
            timer2.as_ref().set_deadline(
                &Deadline::after_mono(DurationMono::from_millis(3), TimerSlack::none()),
                set_preempt_pending_and_kick,
                core::ptr::from_ref(&vcpu).cast_mut().cast(),
            );
        }

        let mut packet = zx_port_packet_t::default();
        let enter_result = Vcpu::enter(&vcpu, &mut packet);
        timer1.cancel();
        timer2.cancel();
        preempt_clear_timeslice_extension();
        expect_err!(enter_result, Status::CANCELED);
    }
}
