// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Guests of the x86 hypervisor.
//!
//! The layout of `Guest` is asserted at compile time against the C++ `Guest` class declared in
//! `<arch/hypervisor.h>`, using the definitions bindgen generates from the C++ header (the
//! `guest_bindings` crate).

use core::mem::{align_of, offset_of, size_of};
use core::pin::Pin;
use fbl::{Recyclable, RefPtr, UniquePtr};
use guest_bindings as bindings;
use id_allocator::IdAllocator;
use pin_init::{PinInit, pin_data, pin_init, pinned_drop};
use zr::static_assert;
use zx_status::Status;
use zx_types::{ZX_GUEST_TRAP_BELL, ZX_GUEST_TRAP_IO, ZX_GUEST_TRAP_MEM, zx_vaddr_t};

use super::invalidate::ept_pointer_from_pml4;
use super::vmx_cpu_state::{VmxInfo, VmxPage, alloc_vmx_state, broadcast_invept, free_vmx_state};
use crate::arch_rs::x86::feature::{X86_FEATURE_VMX, x86_feature_test};
use crate::arch_rs::x86::registers::{
    X86_MSR_IA32_EFER, X86_MSR_IA32_FMASK, X86_MSR_IA32_FS_BASE, X86_MSR_IA32_GS_BASE,
    X86_MSR_IA32_KERNEL_GS_BASE, X86_MSR_IA32_LSTAR, X86_MSR_IA32_PAT, X86_MSR_IA32_STAR,
    X86_MSR_IA32_SYSENTER_CS, X86_MSR_IA32_SYSENTER_EIP, X86_MSR_IA32_SYSENTER_ESP,
    X86_MSR_IA32_TSC_AUX,
};
use crate::hypervisor::aspace::GuestPhysicalAspace;
use crate::hypervisor::trap_map::TrapMap;
use crate::kernel::types::PAddr;
use crate::ksync_kernel::TrackedMutex;
use crate::object::PortDispatcher;
use crate::vm::vm_address_region::VmAddressRegion;

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

/// Clears the bits for `msr` in the MSR bitmaps, so that guest reads and writes of `msr` do not
/// cause VM exits.
///
/// # Safety
///
/// `msr_bitmaps` must point to an allocated 4KiB MSR bitmaps page (valid for reads and writes of
/// 4KiB within a single allocation), with exclusive access for the duration of the call.
unsafe fn ignore_msr(mut msr_bitmaps: *mut u8, msr: u32) {
    // From Volume 3, Section 24.6.9.
    if msr >= 0xc0000000 {
        msr_bitmaps = msr_bitmaps.wrapping_add(1 << 10);
    }

    let msr_low = (msr & 0x1fff) as u16;
    let msr_byte = usize::from(msr_low / 8);
    let msr_bit = (msr_low % 8) as u8;

    // Ignore reads to the MSR.
    // SAFETY: The caller guarantees that `msr_bitmaps` is valid for reads and writes of 4KiB with
    // exclusive access. Its read bitmaps are the first 2KiB and write bitmaps the second 2KiB;
    // `msr_low` is at most 0x1fff, so `msr_byte` is less than 1KiB, and the largest offset
    // accessed is 1KiB + 2KiB + 1023 < 4KiB.
    unsafe { *msr_bitmaps.add(msr_byte) &= !(1u8 << msr_bit) };

    // Ignore writes to the MSR.
    msr_bitmaps = msr_bitmaps.wrapping_add(2 << 10);
    // SAFETY: As above.
    unsafe { *msr_bitmaps.add(msr_byte) &= !(1u8 << msr_bit) };
}

/// Represents a guest within the hypervisor.
///
/// The C++ `Guest` in `<arch/hypervisor.h>` is a layout-compatible view of this type.
///
/// Dropping a guest frees the VMX state (`free_vmx_state`) before dropping its fields. The fields
/// are independent of each other, so their drop order does not matter.
#[pin_data(PinnedDrop)]
#[derive(Recyclable)]
#[repr(C)]
pub struct Guest {
    gpa: GuestPhysicalAspace,
    #[pin]
    traps: TrapMap,
    #[pin]
    vpid_allocator: VpidAllocator,
    msr_bitmaps_page: VmxPage,
}

static_assert!(size_of::<Guest>() == size_of::<bindings::Guest>());
static_assert!(align_of::<Guest>() == align_of::<bindings::Guest>());
static_assert_field_layout!(Guest, gpa, bindings::Guest, gpa_);
static_assert_field_layout!(Guest, traps, bindings::Guest, traps_);
static_assert_field_layout!(Guest, vpid_allocator, bindings::Guest, vpid_allocator_);
static_assert_field_layout!(Guest, msr_bitmaps_page, bindings::Guest, msr_bitmaps_page_);

impl Guest {
    fn init(gpa: GuestPhysicalAspace) -> impl PinInit<Self, Status> {
        pin_init!(Self {
            gpa,
            traps <- TrapMap::new(),
            vpid_allocator <- VpidAllocator::init(),
            msr_bitmaps_page: VmxPage::default(),
        }? Status)
    }

    /// Creates a guest.
    pub fn create() -> Result<Pin<UniquePtr<Guest>>, Status> {
        // Check that the CPU supports VMX.
        if !x86_feature_test(X86_FEATURE_VMX) {
            return Err(Status::NOT_SUPPORTED);
        }

        let gpa = GuestPhysicalAspace::create()?;

        // Invalidate the EPT across all CPUs.
        let eptp = ept_pointer_from_pml4(gpa.arch_aspace().arch_table_phys().0);
        broadcast_invept(eptp);

        // SAFETY: `Guest::create` is only invoked from userspace syscalls or kernel unit tests
        // after `LK_INIT_LEVEL_KERNEL` initialization has completed, and CPU hotplug is not used
        // while guests exist (see TODO on `alloc_vmx_state`).
        unsafe { alloc_vmx_state() }?;

        let guest = match kalloc::Box::try_pin_init(Self::init(gpa)) {
            Ok(guest) => guest,
            Err(status) => {
                // SAFETY: Matches the `alloc_vmx_state` call above, and no VCPU has been created
                // for this guest.
                unsafe { free_vmx_state() };
                return Err(status);
            }
        };
        // SAFETY: Only the pointer to the guest moves, not the guest, and the guest is pinned again
        // below.
        let guest = unsafe { Pin::into_inner_unchecked(guest) };
        // SAFETY: `kalloc::Box::into_raw` returns a valid, non-null pointer that is the sole owner
        // of the guest, and `Recyclable for Guest` frees the guest as a `kalloc::Box`.
        let guest = unsafe { UniquePtr::from_raw(kalloc::Box::into_raw(guest)) };
        // SAFETY: `UniquePtr` never moves its pointee, and drops it in place. The guest was pinned
        // above.
        let mut guest = unsafe { Pin::new_unchecked(guest) };

        // Set up common MSR bitmaps.
        // SAFETY: `x86_feature_test` checked above that the boot CPU supports VMX, and all CPUs
        // have the same features, so the current CPU supports VMX too.
        let vmx_info = unsafe { VmxInfo::new() };
        let this = guest.as_mut().project();
        this.msr_bitmaps_page.alloc(&vmx_info, u8::MAX)?;
        let msr_bitmaps = this.msr_bitmaps_page.virtual_address::<u8>();

        // SAFETY: `msr_bitmaps` points to the 4KiB MSR bitmaps page allocated and initialized
        // above, which `guest` exclusively owns.
        unsafe {
            // These are saved/restored by VMCS controls.
            ignore_msr(msr_bitmaps, X86_MSR_IA32_SYSENTER_CS);
            ignore_msr(msr_bitmaps, X86_MSR_IA32_SYSENTER_ESP);
            ignore_msr(msr_bitmaps, X86_MSR_IA32_SYSENTER_EIP);
            ignore_msr(msr_bitmaps, X86_MSR_IA32_PAT);
            ignore_msr(msr_bitmaps, X86_MSR_IA32_EFER);
            ignore_msr(msr_bitmaps, X86_MSR_IA32_FS_BASE);
            ignore_msr(msr_bitmaps, X86_MSR_IA32_GS_BASE);

            // These are saved/restored manually.
            ignore_msr(msr_bitmaps, X86_MSR_IA32_STAR);
            ignore_msr(msr_bitmaps, X86_MSR_IA32_LSTAR);
            ignore_msr(msr_bitmaps, X86_MSR_IA32_FMASK);
            ignore_msr(msr_bitmaps, X86_MSR_IA32_KERNEL_GS_BASE);
            ignore_msr(msr_bitmaps, X86_MSR_IA32_TSC_AUX);
        }

        Ok(guest)
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
        match kind {
            ZX_GUEST_TRAP_MEM => {
                if port.is_some() {
                    return Err(Status::INVALID_ARGS);
                }
            }
            ZX_GUEST_TRAP_BELL => {
                if port.is_none() {
                    return Err(Status::INVALID_ARGS);
                }
            }
            ZX_GUEST_TRAP_IO => {
                if port.is_some() {
                    return Err(Status::INVALID_ARGS);
                }
                return self.traps.insert_trap(kind, addr, len, None, key);
            }
            _ => return Err(Status::INVALID_ARGS),
        }

        // Common logic for memory-based traps.
        if !page::is_page_rounded(addr) || !page::is_page_rounded(len) {
            return Err(Status::INVALID_ARGS);
        }
        self.gpa.unmap_range(addr, len)?;
        self.traps.insert_trap(kind, addr, len, port, key)
    }

    /// Returns the guest physical address space.
    pub fn physical_aspace(&self) -> &GuestPhysicalAspace {
        &self.gpa
    }

    /// Returns the root `VmAddressRegion` of the guest physical address space.
    pub fn root_vmar(&self) -> RefPtr<VmAddressRegion> {
        self.gpa.root_vmar()
    }

    /// Returns the traps of the guest.
    pub fn traps(&self) -> &TrapMap {
        &self.traps
    }

    /// Returns the physical address of the MSR bitmaps page.
    pub fn msr_bitmaps_address(&self) -> PAddr {
        self.msr_bitmaps_page.physical_address()
    }

    /// Allocates a VPID for a VCPU of the guest.
    pub fn try_alloc_vpid(&self) -> Result<u16, Status> {
        self.vpid_allocator.try_alloc()
    }

    /// Frees a VPID allocated by `try_alloc_vpid`.
    pub fn free_vpid(&self, vpid: u16) -> Result<(), Status> {
        self.vpid_allocator.free(vpid)
    }
}

#[pinned_drop]
impl PinnedDrop for Guest {
    fn drop(self: Pin<&mut Self>) {
        // SAFETY: `Guest::create` called `alloc_vmx_state`, and `Vcpu::create`'s safety contract
        // requires every `Vcpu` (which `VMCLEAR`s its VMCS on drop) to be dropped before its
        // `Guest`.
        unsafe { free_vmx_state() };
    }
}

/// Unit tests for `Guest`.
#[cfg(ktest)]
#[unittest::suite(name = "x86_hypervisor_guest")]
mod tests {
    use super::Guest;
    use crate::kernel::deadline::{Deadline, DurationMono, TimerSlack};
    use crate::kernel::thread::{
        preempt_clear_timeslice_extension, preempt_set_pending, preempt_set_timeslice_extension,
    };
    use crate::kernel::timer::{Timer, ZX_CLOCK_MONOTONIC};
    use crate::object::Vcpu;
    use crate::vm::arch_vm_aspace::{
        ARCH_MMU_FLAG_PERM_EXECUTE, ARCH_MMU_FLAG_PERM_READ, ARCH_MMU_FLAG_PERM_WRITE,
    };
    use crate::vm::pmm;
    use crate::vm::vm_address_region::flag as vmar_flag;
    use crate::vm::vm_object_paged::VmObjectPaged;
    use core::ffi::c_void;
    use pin_init::stack_pin_init;
    use unittest::{expect_eq, expect_err, unwrap_ok};
    use zx_status::Status;
    use zx_types::{ZX_GUEST_TRAP_MEM, zx_gpaddr_t, zx_packet_type_t, zx_port_packet_t};

    // Page table entry flags, from `<page_tables/x86/constants.h>`.
    /// P    Valid
    const X86_MMU_PG_P: u64 = 0x0001;
    /// R/W  Read/Write
    const X86_MMU_PG_RW: u64 = 0x0002;
    /// U/S  User/Supervisor
    const X86_MMU_PG_U: u64 = 0x0004;
    /// PS   Page size (0=4k,1=4M)
    const X86_MMU_PG_PS: u64 = 0x0080;

    /// Verifies that a VCPU runs guest code until the guest accesses a memory trap.
    #[test]
    fn vcpu_enter() {
        let guest = match Guest::create() {
            Err(Status::NOT_SUPPORTED) => return true,
            guest => unwrap_ok!(guest, "Failed to create Guest"),
        };

        // Map 3 pages at GPA 0x0:
        //   [0x0000, 0x1000): PML4
        //   [0x1000, 0x2000): PDP
        //   [0x2000, 0x3000): Guest entry code
        const VMO_PAGES: usize = 3;
        const ENTRY_ADDR: zx_gpaddr_t = page::SIZE * 2;
        const TRAP_ADDR: zx_gpaddr_t = page::SIZE * 3;
        const TRAP_KEY: u64 = 0x1234;

        let vmo = unwrap_ok!(
            VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, (page::SIZE * VMO_PAGES) as u64),
            "Failed to create VMO"
        );
        unwrap_ok!(vmo.commit_range(0, vmo.size()), "Failed to commit VMO");

        // Set up a 1 GiB identity mapping in the first two pages for the guest's initial CR3 (0x0).
        let pml4_entry = page::SIZE as u64 | X86_MMU_PG_P | X86_MMU_PG_U | X86_MMU_PG_RW;
        unwrap_ok!(vmo.write(0, &pml4_entry.to_ne_bytes()), "Failed to write PML4 entry");
        let pdp_entry = X86_MMU_PG_PS | X86_MMU_PG_P | X86_MMU_PG_U | X86_MMU_PG_RW;
        unwrap_ok!(
            vmo.write(page::SIZE as u64, &pdp_entry.to_ne_bytes()),
            "Failed to write PDP entry"
        );

        // Guest code at ENTRY_ADDR (0x2000):
        //   mov $0x1, %eax             (b8 01 00 00 00)
        //   mov $0x2, %ebx             (bb 02 00 00 00)
        //   add %ebx, %eax             (01 d8)
        //   movq $0, (0x3000)          (48 c7 04 25 00 30 00 00 00 00 00 00)
        const GUEST_CODE: [u8; 24] = [
            0xb8, 0x01, 0x00, 0x00, 0x00, 0xbb, 0x02, 0x00, 0x00, 0x00, 0x01, 0xd8, //
            0x48, 0xc7, 0x04, 0x25, 0x00, 0x30, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        ];
        unwrap_ok!(vmo.write(ENTRY_ADDR as u64, &GUEST_CODE), "Failed to write guest code to VMO");

        let vmo_size = vmo.size() as usize;
        let mapping = guest.root_vmar().create_vm_mapping_no_overwrite(
            0,
            vmo_size,
            0,
            vmar_flag::SPECIFIC,
            VmObjectPaged::into_vm_object(vmo),
            0,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_PERM_EXECUTE,
        );
        unwrap_ok!(mapping, "Failed to map guest code into GPA");

        unwrap_ok!(
            guest.set_trap(ZX_GUEST_TRAP_MEM, TRAP_ADDR, page::SIZE, None, TRAP_KEY),
            "Failed to set trap"
        );

        // SAFETY: `guest` outlives `vcpu`, which is declared after it, and this test runs in
        // normal thread context with interrupts and blocking enabled and no locks held.
        let vcpu = match unsafe { Vcpu::create(&guest, ENTRY_ADDR) } {
            Err(Status::NOT_SUPPORTED) => return true,
            vcpu => unwrap_ok!(vcpu, "Failed to create Vcpu"),
        };

        let mut packet = zx_port_packet_t::default();
        unwrap_ok!(vcpu.enter(&mut packet), "Failed to enter Vcpu");
        expect_eq!(zx_packet_type_t::ZX_PKT_TYPE_GUEST_MEM, packet.packet_type);
        expect_eq!(TRAP_KEY, packet.key);
        // `zx_packet_guest_mem_t::addr` is the first field of the packet's union.
        let guest_mem_addr = u64::from_ne_bytes(packet.union[..8].try_into().unwrap());
        expect_eq!(TRAP_ADDR as u64, guest_mem_addr);

        let vcpu_state = unwrap_ok!(vcpu.read_state(), "Failed to read Vcpu state");
        expect_eq!(3, vcpu_state.rax, "Expected guest rax to be 3");
        expect_eq!(2, vcpu_state.rbx, "Expected guest rbx to be 2");
    }

    /// Verifies that VM exit handles external interrupts before re-enabling preemption.
    #[test]
    fn vcpu_enter_preemption() {
        let guest = match Guest::create() {
            Err(Status::NOT_SUPPORTED) => return true,
            guest => unwrap_ok!(guest, "Failed to create Guest"),
        };

        // Map 3 pages at GPA 0x0:
        //   [0x0000, 0x1000): PML4
        //   [0x1000, 0x2000): PDP
        //   [0x2000, 0x3000): Guest entry code
        const VMO_PAGES: usize = 3;
        const ENTRY_ADDR: zx_gpaddr_t = page::SIZE * 2;

        let vmo = unwrap_ok!(
            VmObjectPaged::create(pmm::ALLOC_FLAG_ANY, 0, (page::SIZE * VMO_PAGES) as u64),
            "Failed to create VMO"
        );
        unwrap_ok!(vmo.commit_range(0, vmo.size()), "Failed to commit VMO");

        // Set up a 1 GiB identity mapping in the first two pages for the guest's initial CR3 (0x0).
        let pml4_entry = page::SIZE as u64 | X86_MMU_PG_P | X86_MMU_PG_U | X86_MMU_PG_RW;
        unwrap_ok!(vmo.write(0, &pml4_entry.to_ne_bytes()), "Failed to write PML4 entry");
        let pdp_entry = X86_MMU_PG_PS | X86_MMU_PG_P | X86_MMU_PG_U | X86_MMU_PG_RW;
        unwrap_ok!(
            vmo.write(page::SIZE as u64, &pdp_entry.to_ne_bytes()),
            "Failed to write PDP entry"
        );

        // Guest code at ENTRY_ADDR (0x2000):
        //   jmp .                      (eb fe)
        const GUEST_CODE: [u8; 2] = [0xeb, 0xfe];
        unwrap_ok!(vmo.write(ENTRY_ADDR as u64, &GUEST_CODE), "Failed to write guest code to VMO");

        let vmo_size = vmo.size() as usize;
        let mapping = guest.root_vmar().create_vm_mapping_no_overwrite(
            0,
            vmo_size,
            0,
            vmar_flag::SPECIFIC,
            VmObjectPaged::into_vm_object(vmo),
            0,
            ARCH_MMU_FLAG_PERM_READ | ARCH_MMU_FLAG_PERM_WRITE | ARCH_MMU_FLAG_PERM_EXECUTE,
        );
        unwrap_ok!(mapping, "Failed to map guest code into GPA");

        // SAFETY: `vcpu` is declared after `guest`, so it is dropped first, and this test runs in
        // normal thread context with interrupts and blocking enabled and no locks held.
        let vcpu = match unsafe { Vcpu::create(&guest, ENTRY_ADDR) } {
            Err(Status::NOT_SUPPORTED) => return true,
            vcpu => unwrap_ok!(vcpu, "Failed to create Vcpu"),
        };

        // Install a 1ms timeslice extension. When timer1 fires at 1ms during guest
        // execution, it sets preempts_pending_ and activates the extension without
        // flushing, so the VCPU re-enters the guest with preempts_pending_ set. When
        // timer2 fires at 3ms after the extension has expired, the VM exit handler
        // must dispatch the interrupt and issue EOI before re-enabling preemption and
        // flushing.
        preempt_set_timeslice_extension(DurationMono::from_millis(1));

        unsafe extern "C" fn set_preempt_pending(_: *mut Timer, _: i64, _: *mut c_void) {
            preempt_set_pending();
        }

        /// # Safety
        ///
        /// The caller must ensure `arg` is non-null, properly aligned for `Vcpu`, and points to a
        /// valid, initialized `Vcpu` that outlives the callback without exclusive aliases.
        unsafe extern "C" fn set_preempt_pending_and_kick(_: *mut Timer, _: i64, arg: *mut c_void) {
            preempt_set_pending();
            // SAFETY: The caller guarantees `arg` is non-null, aligned, and points to a valid
            // `Vcpu` that outlives the callback without exclusive aliases.
            unsafe { &*arg.cast::<Vcpu>() }.kick();
        }

        stack_pin_init!(let timer1 = Timer::init(ZX_CLOCK_MONOTONIC));
        stack_pin_init!(let timer2 = Timer::init(ZX_CLOCK_MONOTONIC));

        // SAFETY: `timer1` is stack-pinned and canceled before going out of scope, is not modified
        // concurrently, and `set_preempt_pending` is a valid timer callback that ignores `arg`.
        unsafe {
            timer1.as_ref().set_deadline(
                &Deadline::after_mono(DurationMono::from_millis(1), TimerSlack::none()),
                set_preempt_pending,
                core::ptr::null_mut(),
            );
        }
        // SAFETY: `timer2` is stack-pinned and canceled before either `timer2` or `vcpu` goes out
        // of scope, is not modified concurrently, and `&*vcpu` is a non-null, aligned pointer to
        // an initialized `Vcpu` with no exclusive references while the timer is active.
        unsafe {
            timer2.as_ref().set_deadline(
                &Deadline::after_mono(DurationMono::from_millis(3), TimerSlack::none()),
                set_preempt_pending_and_kick,
                core::ptr::from_ref::<Vcpu>(&vcpu).cast_mut().cast(),
            );
        }

        let mut packet = zx_port_packet_t::default();
        let enter_result = vcpu.enter(&mut packet);
        timer1.cancel();
        timer2.cancel();
        preempt_clear_timeslice_extension();
        expect_err!(enter_result, Status::CANCELED);
    }
}
