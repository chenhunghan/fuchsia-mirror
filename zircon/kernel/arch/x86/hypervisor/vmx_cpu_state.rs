// Copyright 2017 The Fuchsia Authors
//
// Use of this source code is governed by a MIT-style
// license that can be found in the LICENSE file or at
// https://opensource.org/licenses/MIT

//! Per-CPU VMX state: enabling VMX operation on every CPU while any guest exists.

use core::arch::asm;
use core::mem::{align_of, offset_of, size_of};
use core::ops::{Deref, DerefMut};
use core::pin::Pin;
use core::ptr;
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use debug::dprintf;
use guest_bindings as bindings;
use ksync::{KCell, KMutex};
use lazy_init::{LazyInit, NoCheck};
use pin_init::pin_init;
use zr::static_assert;
use zx_status::Status;
use zx_types::zx_paddr_t;

use super::invalidate::{InvEpt, invept};
use super::vmcs::{X86_MSR_IA32_VMX_PROCBASED_CTLS, X86_MSR_IA32_VMX_PROCBASED_CTLS2};
use crate::arch_rs::max_num_cpus;
use crate::arch_rs::x86::feature::{X86_FEATURE_VMX, x86_feature_test};
use crate::arch_rs::x86::registers::X86_CR4_VMXE;
use crate::arch_rs::x86::x86::{read_msr, write_msr, x86_get_cr0, x86_get_cr4, x86_set_cr4};
use crate::hypervisor::cpu::percpu_exec;
use crate::hypervisor::page::Page;
use crate::kernel::bits::{bit_shift, bits, bits_shift};
use crate::kernel::event::AutounsignalEvent;
use crate::kernel::mp::{self, MpIpiTarget, cpu_num_t, sync_exec};
use crate::ksync_kernel::TrackedMutex;

/// Feature control.
const X86_MSR_IA32_FEATURE_CONTROL: u32 = 0x003a;
/// Basic info.
const X86_MSR_IA32_VMX_BASIC: u32 = 0x0480;
/// CR0 bits fixed to 1 in VMX (if 1).
pub const X86_MSR_IA32_VMX_CR0_FIXED0: u32 = 0x0486;
/// CR0 bits fixed to 0 in VMX (if 0).
pub const X86_MSR_IA32_VMX_CR0_FIXED1: u32 = 0x0487;
/// CR4 bits fixed to 1 in VMX (if 1).
pub const X86_MSR_IA32_VMX_CR4_FIXED0: u32 = 0x0488;
/// CR4 bits fixed to 0 in VMX (if 0).
pub const X86_MSR_IA32_VMX_CR4_FIXED1: u32 = 0x0489;
/// VPID and EPT Capabilities.
const X86_MSR_IA32_VMX_EPT_VPID_CAP: u32 = 0x048c;

// X86_MSR_IA32_VMX_BASIC flags
/// Write back.
pub const VMX_MEMORY_TYPE_WRITE_BACK: u64 = 0x06;

// X86_MSR_IA32_FEATURE_CONTROL flags
/// Locked.
const X86_MSR_IA32_FEATURE_CONTROL_LOCK: u64 = 1 << 0;
/// Enable VMXON.
const X86_MSR_IA32_FEATURE_CONTROL_VMXON: u64 = 1 << 2;

/// Stores VMX info from the IA32_VMX_BASIC MSR.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VmxInfo {
    pub revision_id: u32,
    pub region_size: u16,
    pub write_back: bool,
    pub io_exit_info: bool,
    pub vmx_controls: bool,
}

impl VmxInfo {
    /// Reads the VMX info of the current CPU.
    ///
    /// # Safety
    ///
    /// The current CPU must support VMX (`X86_FEATURE_VMX`), so that IA32_VMX_BASIC exists.
    pub unsafe fn new() -> Self {
        // From Volume 3, Appendix A.1.
        // SAFETY: The caller guarantees that the CPU supports VMX, and so has IA32_VMX_BASIC.
        let basic_info = unsafe { read_msr(X86_MSR_IA32_VMX_BASIC) };
        Self {
            revision_id: bits(basic_info, 30, 0) as u32,
            region_size: bits_shift(basic_info, 44, 32) as u16,
            write_back: bits_shift(basic_info, 53, 50) == VMX_MEMORY_TYPE_WRITE_BACK,
            io_exit_info: bit_shift(basic_info, 54) != 0,
            vmx_controls: bit_shift(basic_info, 55) != 0,
        }
    }
}

/// Stores EPT info from the IA32_VMX_EPT_VPID_CAP MSR.
struct EptInfo {
    page_walk_4: bool,
    write_back: bool,
    large_pages: bool,
    invept: bool,
    invvpid: bool,
}

impl EptInfo {
    /// Reads the EPT info of the current CPU.
    ///
    /// # Safety
    ///
    /// The current CPU must support VMX, and the 1-setting of the "activate secondary controls"
    /// and either the "enable EPT" or "enable VPID" VM-execution controls, so that
    /// IA32_VMX_EPT_VPID_CAP exists. See `ept_vpid_cap_exists`.
    unsafe fn new() -> Self {
        // From Volume 3, Appendix A.10.
        // SAFETY: The caller guarantees that IA32_VMX_EPT_VPID_CAP exists.
        let ept_info = unsafe { read_msr(X86_MSR_IA32_VMX_EPT_VPID_CAP) };
        Self {
            page_walk_4: bit_shift(ept_info, 6) != 0,
            write_back: bit_shift(ept_info, 14) != 0,
            large_pages:
                // 2mb pages are supported.
                bit_shift(ept_info, 16) != 0 &&
                // 1gb pages are supported.
                bit_shift(ept_info, 17) != 0,
            invept:
                // INVEPT instruction is supported.
                bit_shift(ept_info, 20) != 0 &&
                // Single-context INVEPT type is supported.
                bit_shift(ept_info, 25) != 0 &&
                // All-context INVEPT type is supported.
                bit_shift(ept_info, 26) != 0,
            invvpid:
                // INVVPID instruction is supported.
                bit_shift(ept_info, 32) != 0 &&
                // Individual-address INVVPID type is supported.
                bit_shift(ept_info, 40) != 0 &&
                // Single-context INVVPID type is supported.
                bit_shift(ept_info, 41) != 0 &&
                // All-context INVVPID type is supported.
                bit_shift(ept_info, 42) != 0 &&
                // Single-context-retaining-globals INVVPID type is supported.
                bit_shift(ept_info, 43) != 0,
        }
    }
}

/// Returns whether the IA32_VMX_EPT_VPID_CAP MSR exists on the current CPU.
///
/// # Safety
///
/// The current CPU must support VMX (`X86_FEATURE_VMX`).
unsafe fn ept_vpid_cap_exists() -> bool {
    // From Volume 3, Appendix A.3.2: Bit 63 of IA32_VMX_PROCBASED_CTLS reports whether the
    // "activate secondary controls" VM-execution control may be set.
    // SAFETY: IA32_VMX_PROCBASED_CTLS exists on every CPU that supports VMX, which the caller
    // guarantees.
    let procbased_ctls = unsafe { read_msr(X86_MSR_IA32_VMX_PROCBASED_CTLS) };
    if bit_shift(procbased_ctls, 63) == 0 {
        return false;
    }

    // From Volume 3, Appendix A.3.3 and A.10: IA32_VMX_EPT_VPID_CAP exists if the "enable EPT"
    // (bit 33) or "enable VPID" (bit 37) VM-execution control may be set.
    // SAFETY: IA32_VMX_PROCBASED_CTLS2 exists as bit 63 of IA32_VMX_PROCBASED_CTLS is set.
    let procbased_ctls2 = unsafe { read_msr(X86_MSR_IA32_VMX_PROCBASED_CTLS2) };
    bit_shift(procbased_ctls2, 33) != 0 || bit_shift(procbased_ctls2, 37) != 0
}

/// VMX region to be used with both VMXON and VMCS.
#[repr(C)]
pub struct VmxRegion {
    pub revision_id: u32,
}

/// A page of memory used by VMX, such as a VMXON region, an MSR bitmap or a VMCS.
///
/// This mirrors the layout of the C++ `VmxPage` (and its `hypervisor::Page` base) in
/// `<arch/hypervisor.h>`.
#[repr(transparent)]
#[derive(Default)]
pub struct VmxPage(Page);

static_assert!(size_of::<VmxPage>() == size_of::<bindings::VmxPage>());
static_assert!(align_of::<VmxPage>() == align_of::<bindings::VmxPage>());
static_assert!(offset_of!(bindings::VmxPage, _base) == 0);
static_assert!(size_of::<Page>() == size_of::<bindings::hypervisor_Page>());
static_assert!(align_of::<Page>() == align_of::<bindings::hypervisor_Page>());

impl VmxPage {
    /// Allocates a page suitable for VMX, and fills it with `fill`.
    pub fn alloc(&mut self, vmx_info: &VmxInfo, fill: u8) -> Result<(), Status> {
        // From Volume 3, Appendix A.1: Bits 44:32 report the number of bytes that
        // software should allocate for the VMXON region and any VMCS region. It is
        // a value greater than 0 and at most 4096 (bit 44 is set if and only if
        // bits 43:32 are clear).
        if usize::from(vmx_info.region_size) > page::SIZE {
            return Err(Status::NOT_SUPPORTED);
        }

        // Check use of write-back memory for VMX regions is supported.
        if !vmx_info.write_back {
            return Err(Status::NOT_SUPPORTED);
        }

        // The maximum size for a VMXON or VMCS region is 4096, therefore
        // unconditionally allocating a page is adequate.
        self.0.alloc(fill)
    }
}

impl Deref for VmxPage {
    type Target = Page;

    #[inline]
    fn deref(&self) -> &Page {
        &self.0
    }
}

/// The VMXON regions of the CPUs, indexed by CPU number.
struct VmxonPages(fbl::Array<VmxPage>);

// SAFETY: `VmxonPages` uniquely owns its `VmxPage`s, which are `Send`, and the kernel allocator
// backing `fbl::Array` may free memory on any thread. (`kalloc::Box`, which backs `fbl::Array`,
// does not implement `Send`.)
unsafe impl Send for VmxonPages {}
// SAFETY: `&VmxonPages` only gives shared access to its `VmxPage`s, which are `Sync`, as
// `fbl::Array` has no interior mutability.
unsafe impl Sync for VmxonPages {}

const _: () = {
    const fn assert_send_sync<T: Send + Sync>() {}
    assert_send_sync::<VmxPage>();
};

impl Deref for VmxonPages {
    type Target = fbl::Array<VmxPage>;

    fn deref(&self) -> &fbl::Array<VmxPage> {
        &self.0
    }
}

impl DerefMut for VmxonPages {
    fn deref_mut(&mut self) -> &mut fbl::Array<VmxPage> {
        &mut self.0
    }
}

#[ksync::guarded]
struct GuestState {
    #[mutex]
    lock: KMutex<TrackedMutex>,
    #[guarded_by(lock)]
    vmxon_pages: VmxonPages,
}

static GUEST_STATE: LazyInit<GuestState, NoCheck> = LazyInit::uninit();

impl GuestState {
    #[inline]
    fn get() -> &'static Self {
        // SAFETY: `GUEST_STATE` is initialized by `init_vmx_cpu_state` at `LK_INIT_LEVEL_KERNEL`,
        // which callers of `alloc_vmx_state` and `free_vmx_state` guarantee has completed.
        unsafe { GUEST_STATE.get_unchecked() }
    }
}

/// Atomic only for reads, writes still acquire the `GuestState` mutex.
static NUM_GUESTS: AtomicUsize = AtomicUsize::new(0);
/// This starts at an assumption of false and is only made true if every CPU supports ept large
/// pages.
static EPT_SUPPORTS_LARGE_PAGES: AtomicBool = AtomicBool::new(false);
/// Event to prevent vmxoff from racing with an ongoing invept.
static NO_ONGOING_INVEPT_OR_VMXOFF: LazyInit<AutounsignalEvent> = LazyInit::uninit();

fn init_vmx_cpu_state(_level: init::LkInitLevel) {
    // SAFETY: Init hooks run once, on the primary CPU, before any guest can be created and so
    // before any other access to the event. LK_INIT_LEVEL_KERNEL hooks run from `lk_main` before
    // the secondary CPUs are started, which orders this initialization before any access from them.
    let Ok(()) = unsafe {
        Pin::static_ref(&NO_ONGOING_INVEPT_OR_VMXOFF).init_pin(AutounsignalEvent::init(true))
    };
    // SAFETY: As above, this runs once, before any guest can be created and so before any use of
    // `GUEST_STATE`.
    unsafe {
        let Ok(()) = Pin::static_ref(&GUEST_STATE).init_pin(pin_init!(GuestState {
            lock <- KMutex::init(),
            vmxon_pages: KCell::new(VmxonPages(fbl::Array::new())),
        }));
    }
}

init::lk_init_hook!(x86_vmx_cpu_state, init_vmx_cpu_state, init::LK_INIT_LEVEL_KERNEL);

/// Executes VMXON with the VMXON region at `pa`.
///
/// # Safety
///
/// Per Intel SDM Vol. 3 §24.11.5, the caller must ensure `pa` is a 4-KiB-aligned physical address
/// of write-back RAM (not MMIO) whose first 4 bytes are initialized to the CPU's VMCS revision
/// identifier (`IA32_VMX_BASIC[30:0]` with bit 31 cleared), and whose page remains allocated and
/// is not otherwise accessed while `vmxon` runs and, if it returns, until `VMXOFF` executes on
/// this CPU.
unsafe fn vmxon(pa: zx_paddr_t) {
    let err: u8;

    // SAFETY: VMXON reads the 64-bit physical address from `pa`, which is a live local. The caller
    // guarantees that the VMXON region it points to may be used by the CPU.
    unsafe {
        asm!(
            "vmxon qword ptr [{pa}]",
            // Set `err` on error (C or Z flag set)
            "setna {err}",
            pa = in(reg) ptr::from_ref(&pa),
            err = out(reg_byte) err,
            options(nostack),
        );
    }

    assert!(err == 0);
}

/// Executes VMXOFF to leave VMX operation on the current CPU.
///
/// # Safety
///
/// No VMCS that may be used again after leaving VMX operation may be active (un-`VMCLEAR`ed) on
/// the current CPU.
unsafe fn vmxoff() {
    let err: u8;

    // SAFETY: VMXOFF leaves VMX operation, ending CPU hardware access to the VMXON region;
    // omitting `nomem` orders this before any subsequent accesses or deallocation of the page. The
    // caller guarantees no active VMCS will be reused without re-initializing, and executing
    // outside VMX root operation raises a fault rather than continuing.
    unsafe {
        asm!(
            "vmxoff",
            // Set `err` on error (C or Z flag set)
            "setna {err}",
            err = out(reg_byte) err,
            options(nostack),
        );
    }

    assert!(err == 0);
}

/// State shared by `vmxon_task` on every CPU.
struct VmxonTaskState<'a> {
    pages: &'a VmxonPages,
    large_page_support: &'a AtomicBool,
}

/// Enables VMX operation on the current CPU, using `state.pages[cpu_num]` as its VMXON region.
///
/// Returns `Ok` only after VMXON has been executed, and `Err` only before.
///
/// # Safety
///
/// The current CPU must support VMX (`X86_FEATURE_VMX`), and `cpu_num` must be the number of the
/// current CPU (`< state.pages.len()`), on which this runs at most once while `state` is borrowed.
/// `state.pages[cpu_num as usize]` must be allocated and its page must not be otherwise accessed
/// while this runs or, if this returns `Ok`, until VMXOFF is executed on this CPU before the page
/// is freed.
unsafe fn vmxon_task(state: &VmxonTaskState<'_>, cpu_num: cpu_num_t) -> Result<(), Status> {
    let page = &state.pages[cpu_num as usize];

    // Check that we have instruction information when we VM exit on IO.
    // SAFETY: The caller guarantees that the CPU supports VMX.
    let vmx_info = unsafe { VmxInfo::new() };
    if !vmx_info.io_exit_info {
        dprintf!(CRITICAL, "hypervisor: IO instruction information not supported\n");
        return Err(Status::NOT_SUPPORTED);
    }

    // Check that full VMX controls are supported.
    if !vmx_info.vmx_controls {
        dprintf!(CRITICAL, "hypervisor: VMX controls not supported\n");
        return Err(Status::NOT_SUPPORTED);
    }

    // Check that EPT and VPID capabilities are reported.
    // SAFETY: The caller guarantees that the CPU supports VMX.
    if !unsafe { ept_vpid_cap_exists() } {
        dprintf!(CRITICAL, "hypervisor: EPT and VPID not supported\n");
        return Err(Status::NOT_SUPPORTED);
    }

    // Check that a page-walk length of 4 is supported.
    // SAFETY: IA32_VMX_EPT_VPID_CAP exists, as checked above.
    let ept_info = unsafe { EptInfo::new() };
    if !ept_info.page_walk_4 {
        dprintf!(CRITICAL, "hypervisor: EPT page-walk length of 4 not supported\n");
        return Err(Status::NOT_SUPPORTED);
    }

    // Check that use of the write-back memory type is supported.
    if !ept_info.write_back {
        dprintf!(CRITICAL, "hypervisor: EPT write-back memory type not supported\n");
        return Err(Status::NOT_SUPPORTED);
    }

    // Check that use of large pages is supported.
    if !ept_info.large_pages {
        // Informational only, will hurt performance.
        dprintf!(INFO, "hypervisor: EPT large pages not supported\n");
        // As we will migrate between CPUs if *any* cpu does not support large pages we must
        // globally disable them.
        state.large_page_support.store(false, Ordering::SeqCst);
    }

    // Check that the INVEPT instruction is supported.
    if !ept_info.invept {
        dprintf!(CRITICAL, "hypervisor: INVEPT instruction not supported\n");
        return Err(Status::NOT_SUPPORTED);
    }

    // Check that the INVVPID instruction is supported.
    if !ept_info.invvpid {
        dprintf!(CRITICAL, "hypervisor: INVVPID instruction not supported\n");
        return Err(Status::NOT_SUPPORTED);
    }

    // Enable VMXON, if required.
    // SAFETY: IA32_FEATURE_CONTROL exists on every CPU that supports VMX, which the caller
    // guarantees.
    let mut feature_control = unsafe { read_msr(X86_MSR_IA32_FEATURE_CONTROL) };
    if (feature_control & X86_MSR_IA32_FEATURE_CONTROL_VMXON) == 0 {
        if (feature_control & X86_MSR_IA32_FEATURE_CONTROL_LOCK) != 0 {
            dprintf!(CRITICAL, "hypervisor: VMX disabled\n");
            return Err(Status::NOT_SUPPORTED);
        }
        feature_control |= X86_MSR_IA32_FEATURE_CONTROL_LOCK;
        feature_control |= X86_MSR_IA32_FEATURE_CONTROL_VMXON;
        // SAFETY: The register is unlocked, so setting its lock and VMXON enable bits is valid.
        unsafe { write_msr(X86_MSR_IA32_FEATURE_CONTROL, feature_control) };
    }

    // Check control registers are in a VMX-friendly state.
    // SAFETY: The kernel runs at CPL 0.
    let cr0 = unsafe { x86_get_cr0() };
    // SAFETY: The CPU supports VMX, and these are the CR0 fixed MSRs.
    if unsafe { cr_is_invalid(cr0, X86_MSR_IA32_VMX_CR0_FIXED0, X86_MSR_IA32_VMX_CR0_FIXED1) } {
        return Err(Status::BAD_STATE);
    }
    // SAFETY: The kernel runs at CPL 0.
    let cr4 = unsafe { x86_get_cr4() } | X86_CR4_VMXE;
    // SAFETY: The CPU supports VMX, and these are the CR4 fixed MSRs.
    if unsafe { cr_is_invalid(cr4, X86_MSR_IA32_VMX_CR4_FIXED0, X86_MSR_IA32_VMX_CR4_FIXED1) } {
        return Err(Status::BAD_STATE);
    }

    // Enable VMX using the VMXE bit.
    // SAFETY: The kernel runs at CPL 0, and the CPU supports VMX in this CR4 configuration, as
    // checked above.
    unsafe { x86_set_cr4(cr4) };

    // Setup VMXON page.
    let region = page.virtual_address::<VmxRegion>();
    // SAFETY: `region` is the page-aligned physmap address of the page allocated for this CPU,
    // which is large enough for a `VmxRegion`, and which only this CPU accesses as `cpu_num` is
    // the current CPU.
    unsafe { (*region).revision_id = vmx_info.revision_id };

    // Execute VMXON.
    // SAFETY: `page` is an allocated `VmxPage` (4-KiB-aligned write-back RAM from the PMM) whose
    // first 4 bytes were just initialized to `vmx_info.revision_id` (`IA32_VMX_BASIC[30:0]`), and
    // the caller guarantees that the page remains allocated and is not otherwise accessed while
    // this runs or, if this returns `Ok`, until `VMXOFF` is executed on this CPU.
    unsafe { vmxon(page.physical_address().0) };

    // From Volume 3, Section 28.3.3.4: Software can use the INVEPT instruction
    // with the "all-context" INVEPT type immediately after execution of the VMXON
    // instruction or immediately prior to execution of the VMXOFF instruction.
    // Either prevents potentially undesired retention of information cached from
    // EPT paging structures between separate uses of VMX operation.
    invept(InvEpt::Global, 0);

    Ok(())
}

/// Leaves VMX operation and clears `CR4.VMXE` on the current CPU.
///
/// # Safety
///
/// No VMCS that may be used again after leaving VMX operation may be active (un-`VMCLEAR`ed) on
/// the current CPU.
unsafe fn vmxoff_task() {
    // Execute VMXOFF.
    // SAFETY: The caller guarantees no active VMCS will be reused after leaving VMX operation.
    unsafe { vmxoff() };

    // Disable VMX.
    // SAFETY: The kernel runs at CPL 0, and clearing CR4.VMXE is valid outside VMX operation.
    unsafe { x86_set_cr4(x86_get_cr4() & !X86_CR4_VMXE) };
}

/// Performs CPU invalidations of the EPT TLB state using the given EPT pointer.
/// This invalidates on all necessary CPUs and will perform IPIs.
pub fn broadcast_invept(eptp: u64) {
    // If there are no guests then do not perform the invept, since vmx will not be on and we will
    // fault. When vmx is turned back on we will perform a global context invalidation anyway, so
    // this is safe. The reason ept invalidations might occur after vmx has been turned off is that
    // the EPT itself can outlive the guests due to user space having their own handles to the EPT
    // aspace.
    if NUM_GUESTS.load(Ordering::SeqCst) == 0 {
        return;
    }
    let result = NO_ONGOING_INVEPT_OR_VMXOFF.wait_infinite();
    // A non-interruptible wait with an infinite deadline only returns once signaled.
    debug_assert!(result.is_ok());
    // Now that the invept token is held and we are not racing with any attempts at vmxoff,
    // check the `NUM_GUESTS` status again.
    if NUM_GUESTS.load(Ordering::SeqCst) != 0 {
        // SAFETY: `MpIpiTarget::All` does not require interrupts to be disabled.
        unsafe { sync_exec(MpIpiTarget::All, 0, || invept(InvEpt::SingleContext, eptp)) };
    } else {
        core::hint::cold_path();
    }
    NO_ONGOING_INVEPT_OR_VMXOFF.signal();
}

/// Enables VMX on every CPU, if this is the first guest.
///
/// # Safety
///
/// `LK_INIT_LEVEL_KERNEL` initialization must have completed (`init_vmx_cpu_state`), and no CPU
/// may be unplugged while `alloc_vmx_state` runs or, if it returns `Ok(())`, until the matching
/// call to `free_vmx_state` completes.
// TODO(https://fxbug.dev/576430432): Coordinate VMX operation with `HotplugLock` / CPU unplug so
// this can be safe once guest lifetime is managed in Rust.
pub unsafe fn alloc_vmx_state() -> Result<(), Status> {
    ksync::lock!(let mut guard = GuestState::get().lock_lock());
    if NUM_GUESTS.load(Ordering::SeqCst) == 0 {
        // Callers check this before creating a guest, but it is required for the MSR reads below.
        if !x86_feature_test(X86_FEATURE_VMX) {
            return Err(Status::NOT_SUPPORTED);
        }

        let num_cpus = max_num_cpus() as usize;
        let pages = fbl::Array::<VmxPage>::try_new(num_cpus).map_err(|_| Status::NO_MEMORY)?;
        let mut pages = VmxonPages(pages);
        // SAFETY: The CPU supports VMX, as checked above.
        let vmx_info = unsafe { VmxInfo::new() };
        for page in pages.iter_mut() {
            page.alloc(&vmx_info, 0)?;
        }

        let large_page_support = AtomicBool::new(true);
        let state = VmxonTaskState { pages: &pages, large_page_support: &large_page_support };

        // Enable VMX for all online CPUs.
        // SAFETY: Every CPU supports VMX, as checked above (all CPUs have the same features).
        // `percpu_exec` runs the task once on each online CPU (`< max_num_cpus()`), passing the
        // current CPU number, and every page in `pages` was freshly allocated above and is not
        // otherwise accessed. As the caller guarantees that no CPU is unplugged while
        // `alloc_vmx_state` runs or while guests exist, VMXOFF is executed on every CPU in
        // `cpu_mask` before `pages` is dropped on failure, and `free_vmx_state` executes VMXOFF on
        // all CPUs before `vmxon_pages` is freed on success.
        let cpu_mask = percpu_exec(|cpu_num| unsafe { vmxon_task(&state, cpu_num) });
        if cpu_mask != mp::get_online_mask() {
            // SAFETY: No guest or VMCS exists yet (`NUM_GUESTS == 0`).
            let task = || unsafe { vmxoff_task() };
            // SAFETY: `MpIpiTarget::Mask` does not require interrupts to be disabled.
            unsafe { sync_exec(MpIpiTarget::Mask, cpu_mask, task) };
            return Err(Status::NOT_SUPPORTED);
        }
        EPT_SUPPORTS_LARGE_PAGES.store(large_page_support.load(Ordering::SeqCst), Ordering::SeqCst);

        *guard.as_mut().vmxon_pages_mut() = pages;
    }
    // Soundness requires preventing `NUM_GUESTS` from wrapping back to 0 if callers leak guests.
    let prev_guests = NUM_GUESTS.fetch_add(1, Ordering::SeqCst);
    assert!(prev_guests != usize::MAX);
    Ok(())
}

/// Disables VMX on every CPU, if this is the last guest.
///
/// # Safety
///
/// Must be called after `LK_INIT_LEVEL_KERNEL` initialization has completed, at most once for each
/// prior successful call to `alloc_vmx_state`, only after any VMCSs used under that guest have been
/// `VMCLEAR`ed and will not be used again without re-initializing, and while no CPUs have been
/// unplugged since VMX was enabled or are being unplugged concurrently.
pub unsafe fn free_vmx_state() {
    ksync::lock!(let mut guard = GuestState::get().lock_lock());
    let prev_guests = NUM_GUESTS.fetch_sub(1, Ordering::SeqCst);
    debug_assert!(prev_guests != 0);
    if prev_guests == 1 {
        let result = NO_ONGOING_INVEPT_OR_VMXOFF.wait_infinite();
        // A non-interruptible wait with an infinite deadline only returns once signaled.
        debug_assert!(result.is_ok());
        // SAFETY: The caller guarantees all guest VMCSs have been `VMCLEAR`ed, and that no CPUs
        // have been unplugged since VMX was enabled or are being unplugged concurrently.
        let task = || unsafe { vmxoff_task() };
        // SAFETY: `MpIpiTarget::All` does not require interrupts to be disabled.
        unsafe { sync_exec(MpIpiTarget::All, 0, task) };
        NO_ONGOING_INVEPT_OR_VMXOFF.signal();
        *guard.as_mut().vmxon_pages_mut() = VmxonPages(fbl::Array::new());
    }
}

/// Returns whether `cr_value` is invalid for VMX operation, given the MSRs whose 1-bits must be 1
/// (`fixed0_msr`) and whose 0-bits must be 0 (`fixed1_msr`).
///
/// # Safety
///
/// The current CPU must support VMX (`X86_FEATURE_VMX`), and `fixed0_msr` and `fixed1_msr` must be
/// the IA32_VMX_CR0_FIXED0/1 or IA32_VMX_CR4_FIXED0/1 MSRs.
pub unsafe fn cr_is_invalid(cr_value: u64, fixed0_msr: u32, fixed1_msr: u32) -> bool {
    // SAFETY: The caller guarantees that the CPU supports VMX, and so has these MSRs.
    let fixed0 = unsafe { read_msr(fixed0_msr) };
    // SAFETY: As above.
    let fixed1 = unsafe { read_msr(fixed1_msr) };
    !(cr_value | !fixed0) != 0 || !(!cr_value | fixed1) != 0
}

/// Returns whether every CPU supports large pages in EPT.
#[inline]
pub fn vmx_ept_supports_large_pages() -> bool {
    EPT_SUPPORTS_LARGE_PAGES.load(Ordering::SeqCst)
}

/// Unit tests for the per-CPU VMX state.
#[cfg(ktest)]
#[unittest::suite(name = "x86_vmx_cpu_state")]
mod tests {
    use super::{NO_ONGOING_INVEPT_OR_VMXOFF, VmxInfo, VmxPage};
    use unittest::{assert_ok, expect_err, expect_false, expect_true};
    use zx_status::Status;

    /// Verifies that the init hook initialized `NO_ONGOING_INVEPT_OR_VMXOFF` in the signaled state.
    #[test]
    fn no_ongoing_invept_or_vmxoff_initialized() {
        expect_true!(NO_ONGOING_INVEPT_OR_VMXOFF.is_signaled());
    }

    /// Verifies that `VmxPage::alloc` validates `VmxInfo` bounds and write-back support.
    #[test]
    fn vmx_page_alloc_validates_vmx_info() {
        let mut page = VmxPage::default();
        let oversized = VmxInfo {
            revision_id: 1,
            region_size: (page::SIZE + 1) as u16,
            write_back: true,
            io_exit_info: true,
            vmx_controls: true,
        };
        expect_err!(page.alloc(&oversized, 0), Status::NOT_SUPPORTED);
        expect_false!(page.is_allocated());

        let no_write_back =
            VmxInfo { region_size: page::SIZE as u16, write_back: false, ..oversized };
        expect_err!(page.alloc(&no_write_back, 0), Status::NOT_SUPPORTED);
        expect_false!(page.is_allocated());

        let valid = VmxInfo { region_size: page::SIZE as u16, write_back: true, ..oversized };
        assert_ok!(page.alloc(&valid, 0x5a));
        expect_true!(page.is_allocated());
    }
}
